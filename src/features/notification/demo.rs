//! The step-by-step demo.
//!
//! Each step states what it is about to do, shows the actual escape sequence and
//! names every piece of it, and only then raises a real notification the user
//! should see appear on their desktop. The arc builds from OSC 9's one field to
//! OSC 99's metadata, replacement and chunking, and finishes with a simulated
//! build that notifies on completion — which is the whole reason anybody wants
//! this.
//!
//! Notifications are intrusive in a way that glyphs are not, so the demo is
//! deliberately quiet: at most one notification per step, and every step past the
//! first reuses the same id so that on OSC 99 they replace each other instead of
//! stacking up in the corner of the screen.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::{
    features::{
        Support,
        notification::{
            probe::{self, ProbeOutcome},
            resolve,
            sequence::{
                MAX_RAW_CHUNK, Protocol, Request, Urgency, build, osc99_close, osc99_query,
            },
        },
        wrap,
    },
    input::{Advance, Event, InputReader, KeyCode, KeyEventKind},
    term::{CSI, Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// Left margin for everything the demo draws.
const MARGIN: u16 = 3;

/// Frame interval for the animated finale: about 30 frames per second.
const FRAME: Duration = Duration::from_millis(33);

/// How long the simulated build in the finale takes, in milliseconds. Long
/// enough that the notification arrives after the user has stopped watching, short
/// enough that they have not wandered off.
const BUILD_MILLIS: u64 = 4200;

/// The id every notification in the demo shares, so each one replaces the last
/// rather than piling up.
const DEMO_ID: &str = "escapade-demo";

/// What the user asked for at the end of a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// Go on to the next step.
    Next,
    /// Back up one step.
    Prev,
    /// Leave the demo.
    Quit,
}

impl From<Advance> for Flow {
    fn from(advance: Advance) -> Self {
        match advance {
            Advance::Next => Self::Next,
            Advance::Prev => Self::Prev,
            Advance::Quit => Self::Quit,
        }
    }
}

/// A step's implementation.
type StepFn = fn(&mut Ctx<'_>) -> Result<Flow>;

/// One step of the demo.
struct Step {
    /// Kebab-case name, stable enough to put in a script or a bug report.
    name: &'static str,
    /// Human title shown in the header.
    title: &'static str,
    /// What the step does.
    run: StepFn,
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Step")
            .field("name", &self.name)
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

/// Declares a step, keeping the name next to the function it names.
macro_rules! step {
    ($name:literal, $title:literal, $run:expr) => {
        Step {
            name: $name,
            title: $title,
            run: $run,
        }
    };
}

/// The script. Order matters: the replacement step needs the notification the
/// step before it raised.
///
/// The names are worth more here than in most features, because they map onto
/// distinct protocols rather than onto stages of one. `--only osc777` is a way to
/// test exactly the sequence a particular terminal is supposed to implement,
/// without sitting through the other two.
const STEPS: &[Step] = &[
    step!("overview", "Three sequences, one job", step_overview),
    step!("osc9", "OSC 9: one field, and that is all", step_osc9),
    step!("osc777", "OSC 777: a title appears", step_osc777),
    step!("osc99", "OSC 99: saying what you mean", step_osc99),
    step!(
        "replace",
        "Replacing a notification you already sent",
        step_replace
    ),
    step!(
        "chunked",
        "A body too long for one escape code",
        step_chunking
    ),
    step!("query", "Asking instead of guessing", step_query),
    step!(
        "build",
        "The one you actually wanted: a build that tells you",
        step_build
    ),
    step!("restore", "Putting the terminal back", step_leave),
];

/// The step names, in order, for [`crate::features::select_steps`].
fn step_names() -> Vec<&'static str> {
    STEPS.iter().map(|step| step.name).collect()
}

/// Prints the step list for `--list`.
pub fn list_steps(theme: &Theme) {
    println!();
    println!("  {}", theme.paint("Desktop notifications demo", Ink::Title));
    println!();
    let width = STEPS.iter().map(|step| step.name.len()).max().unwrap_or(0);
    for (index, step) in STEPS.iter().enumerate() {
        println!(
            "  {} {}  {}",
            theme.paint(&format!("{:>2}.", index + 1), Ink::Muted),
            theme.paint(&format!("{:<width$}", step.name), Ink::Accent),
            step.title
        );
    }
    println!();
    println!(
        "  {}",
        theme.paint(
            "escapade notification demo --only <NAME>   run just that step",
            Ink::Muted
        )
    );
    println!(
        "  {}",
        theme.paint(
            "escapade notification demo --step N        start there and carry on",
            Ink::Muted
        )
    );
    println!();
    println!(
        "  {}",
        theme.paint(
            "Each step raises at most one notification, and they share an id so that on \
             OSC 99 they replace each other.",
            Ink::Muted
        )
    );
    println!();
}

/// Runs the demo.
///
/// # Errors
///
/// Fails if `only` names no step or is an ambiguous prefix, or if the terminal
/// cannot be set up or written to.
pub fn run(
    theme: &Theme,
    force: bool,
    only: Option<&str>,
    start_step: Option<usize>,
) -> Result<()> {
    // Resolved before touching the terminal, so a mistyped `--only` is a plain
    // error message rather than one printed after the alternate screen has been
    // entered and left again.
    let selection = crate::features::select_steps(&step_names(), only, start_step)?;

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;

    let outcome = probe::probe(&mut term, &mut input)?;

    // `--force` skips the capability check, but it cannot conjure a terminal:
    // enabling raw mode on something that is not a tty fails with ENXIO, so this
    // gate comes first and is not overridable.
    if !outcome.interactive || (!outcome.any_supported() && !force) {
        term.restore();
        refuse(theme, &outcome);
        return Ok(());
    }

    let mut index = selection.start;

    term.enable_raw()?;
    term.enter_alt_screen()?;
    term.hide_cursor()?;

    let mut ctx = Ctx {
        multiplexer: outcome.multiplexer,
        forced: force && !outcome.any_supported(),
        outcome,
        term: &mut term,
        input: &mut input,
        theme,
        row: 0,
        width: 0,
    };

    let result = (|| -> Result<()> {
        loop {
            match (STEPS[index].run)(&mut ctx)? {
                // `--only` runs the one step and stops, rather than carrying on
                // through the rest of the script.
                Flow::Next => {
                    if selection.single || index + 1 >= STEPS.len() {
                        return Ok(());
                    }
                    index += 1;
                }
                Flow::Prev => index = index.saturating_sub(1),
                Flow::Quit => return Ok(()),
            }
        }
    })();

    // Take the demo's notification down on the way out, even if a step failed
    // part way through. A critical notification left on screen after the program
    // exited would be nobody's idea of polite.
    let _ = probe::send(ctx.term, ctx.multiplexer, &osc99_close(DEMO_ID));
    term.restore();
    result
}

/// The refusal shown when no protocol looks supported.
fn refuse(theme: &Theme, outcome: &ProbeOutcome) {
    println!();
    let message = "sorry - this does not seem to be supported by your terminal - check \
                   'escapade notification explain' for support, or use \
                   'escapade notification demo --force' to disable feature probing.";
    for line in wrap(message, theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Warn));
    }

    println!();
    if let Some(known) = outcome.identity.known {
        for line in wrap(
            &format!(
                "This terminal was identified as {} (via {}={}), which is not recorded as \
                 implementing OSC 9, OSC 777 or OSC 99. If that is out of date, \
                 data/features/notifications.toml is where to fix it.",
                known.name, outcome.identity.source, outcome.identity.raw
            ),
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    } else {
        for line in wrap(
            "Nothing identified this terminal, and OSC 9 and OSC 777 cannot be asked whether \
             they are there — only OSC 99 has a query, and it stayed silent. That makes this a \
             shrug rather than a no, so --force is a reasonable thing to reach for.",
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }

    if !outcome.conclusive() && outcome.interactive {
        println!();
        for line in wrap(
            "Nothing at all came back — not even the device-attributes request that every \
             terminal answers — so something between here and the terminal is swallowing \
             replies, and even the OSC 99 result is unreliable.",
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }

    if let Some(multiplexer) = outcome.multiplexer {
        println!();
        for line in wrap(
            &format!(
                "You are inside {}, which filters escape sequences it does not recognise. Even a \
                 terminal that raises notifications will look like it does not from in here.",
                multiplexer.name()
            ),
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
        println!("  {}", theme.paint(multiplexer.passthrough_hint(), Ink::Code));
    }

    if !outcome.interactive {
        println!();
        println!(
            "  {}",
            theme.paint(
                "stdout is not a terminal, so nothing could be asked in the first place.",
                Ink::Muted
            )
        );
    }
    println!();
}

// ---------------------------------------------------------------------------
// Drawing context
// ---------------------------------------------------------------------------

/// Everything a step needs, plus a cursor so steps can write line by line.
struct Ctx<'a> {
    term: &'a mut Term,
    input: &'a mut InputReader,
    theme: &'a Theme,
    multiplexer: Option<Multiplexer>,
    /// What the probe concluded.
    outcome: ProbeOutcome,
    /// Whether the demo is running past a failed probe.
    forced: bool,
    /// Next row to write to.
    row: u16,
    /// Usable text width.
    width: usize,
}

impl std::fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("row", &self.row)
            .field("width", &self.width)
            .field("forced", &self.forced)
            .finish_non_exhaustive()
    }
}

impl Ctx<'_> {
    /// Clears the screen and draws the chrome for step `index`.
    fn page(&mut self, index: usize) -> Result<()> {
        self.term.refresh_size()?;
        self.width = usize::from(self.term.cols.saturating_sub(MARGIN * 2)).min(96);
        self.term.clear()?;

        let title = STEPS[index].title;
        let counter = format!("step {}/{}", index + 1, STEPS.len());
        let heading = "escapade · desktop notifications";
        let gap = self
            .width
            .saturating_sub(heading.chars().count() + counter.chars().count());

        self.row = 2;
        let line = format!(
            "{}{}{}",
            self.theme.paint(heading, Ink::Title),
            " ".repeat(gap),
            self.theme.paint(&counter, Ink::Muted)
        );
        self.line(&line)?;
        self.line(&self.theme.paint(&"─".repeat(self.width), Ink::Muted))?;
        self.blank();
        self.line(&self.theme.paint(title, Ink::Accent))?;
        self.blank();
        Ok(())
    }

    /// Writes one line at the margin and advances the cursor.
    fn line(&mut self, text: &str) -> Result<()> {
        let row = self.row;
        self.term.put(MARGIN, row, text)?;
        self.row = self.row.saturating_add(1);
        Ok(())
    }

    /// Writes one line at a deeper indent.
    fn indented(&mut self, indent: usize, text: &str) -> Result<()> {
        let row = self.row;
        self.term
            .put(MARGIN + u16::try_from(indent).unwrap_or(0), row, text)?;
        self.row = self.row.saturating_add(1);
        Ok(())
    }

    /// Skips a row.
    fn blank(&mut self) {
        self.row = self.row.saturating_add(1);
    }

    /// Wraps and writes a paragraph.
    fn para(&mut self, text: &str) -> Result<()> {
        for line in wrap(text, self.width) {
            self.line(&line)?;
        }
        Ok(())
    }

    /// Wraps and writes a paragraph in the muted tone.
    fn note(&mut self, text: &str) -> Result<()> {
        for line in wrap(text, self.width.saturating_sub(2)) {
            let painted = self.theme.paint(&line, Ink::Muted);
            self.indented(2, &painted)?;
        }
        Ok(())
    }

    /// Shows a sequence, then names each of its pieces.
    fn sequence(&mut self, raw: &str, parts: &[(&str, &str)]) -> Result<()> {
        let painted = self.theme.paint(&pretty(raw), Ink::Code);
        self.indented(2, &painted)?;
        self.blank();
        let token_width = parts
            .iter()
            .map(|(token, _)| token.chars().count())
            .max()
            .unwrap_or(0);
        for (token, meaning) in parts {
            let label = format!("{token:<token_width$}");
            let lines = wrap(meaning, self.width.saturating_sub(token_width + 7));
            for (index, line) in lines.iter().enumerate() {
                if index == 0 {
                    let text = format!(
                        "{}  {}",
                        self.theme.paint(&label, Ink::Accent),
                        self.theme.paint(line, Ink::Muted)
                    );
                    self.indented(2, &text)?;
                } else {
                    let text = self.theme.paint(line, Ink::Muted);
                    self.indented(4 + token_width, &text)?;
                }
            }
        }
        Ok(())
    }

    /// Sends a sequence, wrapped for a multiplexer if needed.
    fn send(&mut self, sequence: &str) -> Result<()> {
        probe::send(self.term, self.multiplexer, sequence)
    }

    /// Raises one notification and says what the user should be seeing.
    ///
    /// Every step goes through here, so "at most one notification per step" is a
    /// property of the code rather than a promise in a comment.
    fn raise(&mut self, protocol: Protocol, request: &Request) -> Result<()> {
        let built = build(protocol, request);
        for piece in &built.sequences {
            self.send(piece)?;
        }
        self.term.flush()?;

        let expected = match request.title.as_ref() {
            Some(title) => format!(
                "You should now see a notification titled {:?} with the body {:?}.",
                title.as_str(),
                request.body.as_str()
            ),
            None => format!(
                "You should now see a notification reading {:?}.",
                request.body.as_str()
            ),
        };
        let sent = format!(
            "→ sent {} ({} escape {})",
            protocol.label(),
            built.sequences.len(),
            if built.sequences.len() == 1 {
                "code"
            } else {
                "codes"
            }
        );
        self.indented(2, &self.theme.paint(&sent, Ink::Good))?;
        self.note(&expected)?;
        for note in &built.lost {
            let painted = self
                .theme
                .paint(&format!("… but {note}"), Ink::Warn);
            self.indented(2, &painted)?;
        }
        self.caveat(protocol)?;
        Ok(())
    }

    /// Explains, per protocol, why nothing may have appeared.
    fn caveat(&mut self, protocol: Protocol) -> Result<()> {
        let verdict_supported = self
            .outcome
            .verdict(protocol)
            .is_some_and(probe::Verdict::supported);
        if !verdict_supported {
            let reason = self
                .outcome
                .verdict(protocol)
                .map(|verdict| verdict.reason.clone())
                .unwrap_or_default();
            self.note(&format!(
                "If nothing appeared, that is expected: {reason}. The escape code was still \
                 sent, and the terminal discarded it silently.",
            ))?;
        } else if let Some(known) = self.outcome.identity.known
            && let Some(note) = known.note
        {
            self.note(&format!("{}: {note}.", known.name))?;
        }
        Ok(())
    }

    /// Draws the footer hint and blocks until the user chooses.
    fn wait(&mut self) -> Result<Flow> {
        let row = self.term.rows.saturating_sub(2);
        let hint = self
            .theme
            .paint("space/enter next · p back · q quit", Ink::Muted);
        self.term.put(MARGIN, row, &hint)?;
        self.term.flush()?;
        Ok(self.input.wait_for_advance()?.into())
    }
}

/// Builds the demo's standard request: shared id, so notifications replace each
/// other rather than accumulating.
fn demo_request(title: Option<&str>, body: &str) -> Request {
    let mut request = Request::new(title, body);
    request.id = Some(DEMO_ID.to_owned());
    request
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// The three protocols side by side, and which one this terminal gets.
fn step_overview(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(0)?;
    ctx.para(
        "A program in the terminal can raise a real desktop notification by printing an escape \
         sequence. There are three of them, invented independently, and they differ in what they \
         can say rather than in how they say it.",
    )?;
    ctx.blank();

    for protocol in Protocol::RICHEST_FIRST {
        let (symbol, ink) = match ctx.outcome.verdict(protocol).map(|verdict| verdict.support) {
            Some(Support::Yes) => ("✔", Ink::Good),
            Some(Support::No) => ("✘", Ink::Bad),
            Some(Support::Unknown) | None => ("?", Ink::Warn),
        };
        let line = format!(
            "{} {}  {}",
            ctx.theme.paint(symbol, ink),
            ctx.theme
                .paint(&format!("{:<8}", protocol.label()), Ink::Strong),
            ctx.theme.paint(protocol.capability(), Ink::Muted)
        );
        ctx.indented(2, &line)?;
    }
    ctx.blank();

    let choice = resolve(crate::cli::NotifyProtocol::Auto, &ctx.outcome);
    ctx.indented(
        2,
        &ctx.theme.paint(
            &format!("--protocol auto here resolves to {}", choice.protocol.label()),
            Ink::Accent,
        ),
    )?;
    ctx.note(&choice.reason)?;
    ctx.blank();
    ctx.note(
        "The ✔ and ✘ above are not all the same kind of claim. Only OSC 99 can be asked — the \
         other two have no query and no reply, so their rows are inference from this terminal's \
         identity, not measurement.",
    )?;
    if ctx.forced {
        ctx.blank();
        ctx.note(
            "You are running with --force past a probe that found nothing. Every sequence will \
             still be sent, so you can watch what goes out; expect no notifications to appear.",
        )?;
    }

    ctx.wait()
}

/// OSC 9: the original one-liner.
fn step_osc9(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(1)?;
    ctx.para(
        "The oldest one, from iTerm2, from before 2010. It is one field of text with nowhere to \
         put anything else — no title, no urgency, no id, no way to take it back.",
    )?;
    ctx.blank();

    let request = Request::new(None, "The simplest notification there is");
    let built = build(Protocol::Osc9, &request);
    ctx.sequence(
        &built.joined(),
        &[
            (
                "OSC",
                "Operating System Command, ESC ]. A terminal that does not recognise the number \
                 that follows swallows the whole string and moves on — which is exactly why this \
                 is safe to print without asking permission first.",
            ),
            (
                "9",
                "the number iTerm2 picked. ConEmu later picked it too, for taskbar progress as \
                 OSC 9 ; 4 ; state ; progress — so a message beginning with a digit and a \
                 semicolon is ambiguous, and escapade prefixes a space to keep it text.",
            ),
            (
                "…",
                "the message, in the session's encoding. Terminals disagree about what this \
                 field is: Ghostty documents it as the title, iTerm2 as the message, Contour \
                 renders it as the body under a title of \"ConEmu\".",
            ),
            ("ST", "String Terminator, ESC \\. A lone BEL is also accepted."),
        ],
    )?;
    ctx.blank();

    ctx.raise(Protocol::Osc9, &request)?;

    ctx.wait()
}

/// OSC 777: the urxvt form, with a title.
fn step_osc777(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(2)?;
    ctx.para(
        "urxvt has a generic hatch for talking to its Perl extensions, OSC 777 ; <extension> ; \
         <args>, and the notify extension is the one everybody copied. Two fields now, so a \
         notification can have a title.",
    )?;
    ctx.blank();

    let request = Request::new(Some("escapade"), "Now with a title of its own");
    let built = build(Protocol::Osc777, &request);
    ctx.sequence(
        &built.joined(),
        &[
            ("777", "urxvt's number for \"call one of my extensions\"."),
            (
                "notify",
                "which extension. The original was seven lines of Perl that ran notify-send; \
                 foot, Ghostty, WezTerm, Rio, Konsole, Contour and Windows Terminal all \
                 reimplemented that one command.",
            ),
            (
                "; title ; body",
                "positional fields, split on semicolons. That is the whole hazard of this \
                 protocol: a semicolon in the title shifts the body into a fourth field, so \
                 escapade turns semicolons in a title into commas.",
            ),
        ],
    )?;
    ctx.blank();

    ctx.raise(Protocol::Osc777, &request)?;
    ctx.blank();
    ctx.note(
        "There is still no urgency, no id and no way to close it. Whatever the notification \
         daemon does by default is what you get.",
    )?;

    ctx.wait()
}

/// OSC 99: keyed metadata.
fn step_osc99(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(3)?;
    ctx.para(
        "kitty's protocol, from 2020, written after the terminal working group's attempt to \
         standardise one stalled. Metadata is colon-separated key=value pairs before the \
         payload, which is what makes it extensible: a terminal ignores keys it does not know.",
    )?;
    ctx.blank();

    let mut request = demo_request(Some("Build finished"), "42 tests passed, 0 failed");
    request.urgency = Some(Urgency::Critical);
    let built = build(Protocol::Osc99, &request);

    ctx.sequence(
        &built.sequences.join(""),
        &[
            (
                "99",
                "kitty's number. Both semicolons are always present, even when the metadata is \
                 empty — which is why the minimal form is OSC 99 ; ; Hello world ST.",
            ),
            (
                "i=…",
                "the notification id. It ties these two escape codes to one notification, and it \
                 is what lets a later notification replace this one.",
            ),
            (
                "p=title",
                "which part of the notification the payload is. title is the default, so a \
                 notification with only a body puts its text here.",
            ),
            ("p=body", "the second escape code carries the body."),
            (
                "u=2",
                "urgency: 0 low, 1 normal, 2 critical. The only one of the three protocols that \
                 can say this at all. Critical notifications usually do not auto-dismiss.",
            ),
            (
                "d=0 / d=1",
                "done. d=0 means more escape codes are coming and the notification is not \
                 complete; the last one says d=1 and that is what makes it appear.",
            ),
        ],
    )?;
    ctx.blank();

    ctx.raise(Protocol::Osc99, &request)?;

    ctx.wait()
}

/// Replacing an earlier notification by id.
fn step_replace(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(4)?;
    ctx.para(&format!(
        "The notification from the last step used i={DEMO_ID}. Sending another one with the same \
         id does not add a second notification — it updates the first in place. That is the \
         difference between a progress indicator and a wall of toasts.",
    ))?;
    ctx.blank();

    let mut request = demo_request(Some("Build finished"), "Actually, one test was flaky");
    request.urgency = Some(Urgency::Normal);
    let built = build(Protocol::Osc99, &request);
    ctx.sequence(
        &built.sequences.join(""),
        &[
            (
                &format!("i={DEMO_ID}"),
                "the same id as the previous step. Same id, same notification.",
            ),
            (
                "u=1",
                "and the urgency comes back down, which the desktop applies to the notification \
                 it already has on screen.",
            ),
        ],
    )?;
    ctx.blank();

    ctx.raise(Protocol::Osc99, &request)?;
    ctx.blank();
    ctx.note(
        "How smooth the replacement looks is up to the desktop, not the terminal — some \
         notification daemons update in place, some flash. Neither OSC 9 nor OSC 777 can do this \
         at all: with no id there is nothing to name.",
    )?;
    ctx.blank();
    ctx.sequence(
        &osc99_close(DEMO_ID),
        &[(
            "p=close",
            "and this takes it away again. The demo sends one of these on its way out, so it \
             does not leave a critical notification sitting on your desktop.",
        )],
    )?;

    ctx.wait()
}

/// Chunking a long payload, and the base64 escape hatch.
fn step_chunking(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(5)?;
    ctx.para(&format!(
        "One escape code may carry at most 2048 unencoded bytes of payload, or 4096 encoded. \
         Longer text is split across several, tied together by i= and finished by d=1. escapade \
         chunks at {MAX_RAW_CHUNK} raw bytes — a multiple of three, so base64 produces no \
         padding, and always on a character boundary, so no chunk is half a character.",
    ))?;
    ctx.blank();

    // A body long enough to need three escape codes, and containing a semicolon,
    // which is what forces the base64 path.
    let long = format!(
        "{} ; and it keeps going.",
        "This body is deliberately far longer than one escape code can carry. ".repeat(50)
    );
    let request = demo_request(Some("A long story"), &long);
    let built = build(Protocol::Osc99, &request);

    let first = built.sequences.first().map_or("", String::as_str);
    ctx.sequence(
        first,
        &[
            (
                "e=1",
                "the payload is base64. Needed here because the text contains a semicolon, and \
                 the payload is whatever follows the second one — encoding it removes the \
                 question entirely. It is also the only way to send a body containing anything \
                 the protocol calls unsafe.",
            ),
            (
                "d=0",
                "not done. Every escape code but the last says this, so the terminal accumulates \
                 instead of displaying.",
            ),
        ],
    )?;
    ctx.blank();

    let summary = format!(
        "{} characters of body → {} escape codes, {} bytes total",
        long.chars().count(),
        built.sequences.len(),
        built.joined().len()
    );
    ctx.indented(2, &ctx.theme.paint(&summary, Ink::Strong))?;
    for (index, piece) in built.sequences.iter().enumerate() {
        let label = format!(
            "  {:>2}. {} bytes  {}",
            index + 1,
            piece.len(),
            if index + 1 == built.sequences.len() {
                "d=1 — display it"
            } else {
                "d=0 — keep going"
            }
        );
        ctx.indented(2, &ctx.theme.paint(&label, Ink::Muted))?;
    }
    ctx.blank();

    ctx.raise(Protocol::Osc99, &request)?;
    ctx.blank();
    ctx.note(
        "Sanitizing happens before any of this. A title or body containing a raw ESC, BEL or C1 \
         control could end the OSC string early and leave the rest being read as a fresh escape \
         sequence, so those characters never reach the terminal at all — whitespace controls \
         become a space and the rest are dropped.",
    )?;

    ctx.wait()
}

/// The capability query, which only OSC 99 has.
fn step_query(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(6)?;
    ctx.para(
        "OSC 9 and OSC 777 cannot be asked anything. There is no query, no reply, and no way to \
         tell a terminal that ignored your notification from one that raised it while you were \
         looking elsewhere. OSC 99 has a question.",
    )?;
    ctx.blank();

    ctx.sequence(
        &osc99_query("escapade-probe"),
        &[
            (
                "p=?",
                "the query payload type. The reply lists what this terminal's implementation can \
                 do: which payload types, which urgencies, whether it reports activation, \
                 whether it honours auto-expiry.",
            ),
            (
                "i=…",
                "an id, so the reply can be matched to the question rather than to some other \
                 OSC reply arriving at the same time.",
            ),
        ],
    )?;
    ctx.blank();
    ctx.sequence(
        &format!("{CSI}c"),
        &[(
            "CSI c",
            "Primary Device Attributes, sent straight after as a barrier. Every terminal since \
             the VT100 answers it, and terminals process input in order — so if this comes back \
             and the OSC 99 answer did not, the terminal has demonstrably had its chance. That \
             turns a timeout into a conclusion.",
        )],
    )?;
    ctx.blank();

    // Cloned out of the outcome so the drawing calls below can borrow `ctx`
    // mutably while the reply is still in hand.
    let answered = ctx.outcome.osc99.clone();
    if let Some(support) = answered {
        ctx.indented(2, &ctx.theme.paint("✔ this terminal answered", Ink::Good))?;
        for (label, value) in [
            ("payload types", support.payloads.join(", ")),
            ("urgencies", support.urgencies.join(", ")),
            ("activation", support.actions.join(", ")),
            ("occasions", support.occasions.join(", ")),
        ] {
            if value.is_empty() {
                continue;
            }
            let line = format!(
                "{}  {}",
                ctx.theme.paint(&format!("{label:<14}"), Ink::Muted),
                ctx.theme.paint(&value, Ink::Code)
            );
            ctx.indented(4, &line)?;
        }
    } else {
        let text = if ctx.outcome.answered_barrier {
            "✘ the barrier came back and OSC 99 did not — a real no, not a timeout"
        } else {
            "? nothing came back at all, not even the barrier — something is eating replies"
        };
        ctx.indented(2, &ctx.theme.paint(text, Ink::Warn))?;
    }
    ctx.blank();
    ctx.note(
        "For the other two, all that is left is working out which terminal this is — from \
         TERM_PROGRAM, terminal-specific environment variables, TERM, and the terminal's own \
         answer to XTVERSION — and looking it up in a table. escapade labels that inference \
         rather than dressing it up as measurement.",
    )?;
    if let Some(version) = ctx.outcome.version.clone() {
        ctx.blank();
        let line = format!("XTVERSION said: {version}");
        ctx.indented(2, &ctx.theme.paint(&line, Ink::Accent))?;
    }

    ctx.wait()
}

/// The finale: a build that notifies on completion.
fn step_build(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(7)?;
    ctx.para(
        "This is what the whole feature is for. A long job runs, you go and read something else, \
         and the machine comes and finds you. Watch the build, then look at the corner of your \
         screen.",
    )?;
    ctx.blank();

    let choice = resolve(crate::cli::NotifyProtocol::Auto, &ctx.outcome);
    ctx.indented(
        2,
        &ctx.theme.paint(
            &format!("using {} — {}", choice.protocol.label(), choice.reason),
            Ink::Muted,
        ),
    )?;
    ctx.blank();

    let crates = [
        "escapade v0.1.0",
        "anyhow v1.0.104",
        "clap v4.6.4",
        "crossterm v0.29.0",
        "termimad v0.35.1",
        "unicode-width v0.2.2",
    ];
    let bar_row = ctx.row;
    ctx.blank();
    ctx.blank();
    ctx.blank();
    let hint = ctx
        .theme
        .paint("space/enter next · p back · q quit", Ink::Muted);
    let footer_row = ctx.term.rows.saturating_sub(2);
    ctx.term.put(MARGIN, footer_row, &hint)?;

    let started = Instant::now();
    let mut notified = false;

    loop {
        // Integer arithmetic throughout: a progress bar is a ratio of cells to
        // cells, and going via floating point only adds cast lints.
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(BUILD_MILLIS);
        let elapsed = elapsed.min(BUILD_MILLIS);
        let done = elapsed >= BUILD_MILLIS;

        ctx.term.begin_sync()?;

        let width = u64::from(ctx.term.cols.saturating_sub(MARGIN * 2 + 12)).min(48);
        let filled = width * elapsed / BUILD_MILLIS;
        let percent = 100 * elapsed / BUILD_MILLIS;
        let bar = format!(
            "[{}{}] {percent:>3}%",
            "━".repeat(usize::try_from(filled).unwrap_or(0)),
            "╌".repeat(usize::try_from(width - filled).unwrap_or(0)),
        );
        let painted = ctx
            .theme
            .paint(&bar, if done { Ink::Good } else { Ink::Accent });
        ctx.term.put(MARGIN + 2, bar_row, &painted)?;
        ctx.term.clear_to_eol()?;

        let step = usize::try_from(u64::try_from(crates.len()).unwrap_or(1) * elapsed / BUILD_MILLIS)
            .unwrap_or(0)
            .min(crates.len() - 1);
        let status = if done {
            format!("Finished in {}.{}s", BUILD_MILLIS / 1000, BUILD_MILLIS % 1000 / 100)
        } else {
            format!("Compiling {}", crates[step])
        };
        let painted = ctx
            .theme
            .paint(&status, if done { Ink::Good } else { Ink::Muted });
        ctx.term.put(MARGIN + 2, bar_row + 2, &painted)?;
        ctx.term.clear_to_eol()?;

        ctx.term.end_sync()?;

        if done && !notified {
            notified = true;
            ctx.row = bar_row + 4;
            let body = format!(
                "{} crates compiled in {}.{}s",
                crates.len(),
                BUILD_MILLIS / 1000,
                BUILD_MILLIS % 1000 / 100
            );
            let mut request = demo_request(Some("Build finished"), &body);
            request.urgency = Some(Urgency::Normal);
            ctx.raise(choice.protocol, &request)?;
            ctx.term.put(MARGIN, footer_row, &hint)?;
            ctx.term.flush()?;
        }

        if let Some(event) = ctx.input.read(FRAME)?
            && let Some(flow) = advance_from(&event)
        {
            return Ok(flow);
        }
    }
}

/// The last step: hand the terminal back.
fn step_leave(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(8)?;
    ctx.para(
        "Nothing needs cleaning up on the terminal's side — a notification is fire and forget, \
         and there is no mode left switched on. The one courtesy worth doing is taking down the \
         notification the demo raised, so it is not still sitting there after you quit.",
    )?;
    ctx.blank();

    ctx.sequence(
        &osc99_close(DEMO_ID),
        &[(
            "p=close",
            "close the notification with this id. Only OSC 99 can; a notification raised with \
             OSC 9 or OSC 777 stays until the user or the desktop dismisses it.",
        )],
    )?;
    ctx.blank();
    ctx.send(&osc99_close(DEMO_ID))?;
    ctx.indented(
        2,
        &ctx.theme.paint("✔ closed", Ink::Good),
    )?;
    ctx.blank();

    ctx.sequence(
        &format!("{CSI}?1049l"),
        &[(
            "CSI ?1049l",
            "leave the alternate screen. Mode 1049 saved the cursor and switched buffers on the \
             way in, so your scrollback is exactly as you left it — this demo never scrolled it.",
        )],
    )?;
    ctx.blank();
    ctx.note("Press space to return to your shell.")?;

    let flow = ctx.wait()?;

    // The alternate screen goes away here rather than in the cleanup path, so
    // that leaving it is a visible step of the demo rather than a side effect.
    ctx.term.show_cursor()?;
    ctx.term.leave_alt_screen()?;
    ctx.term.disable_raw()?;

    println!();
    println!(
        "  {} {}",
        ctx.theme.paint("OSC 99:", Ink::Accent),
        ctx.theme.paint(
            "https://sw.kovidgoyal.net/kitty/desktop-notifications/",
            Ink::Link
        )
    );
    let choice = resolve(crate::cli::NotifyProtocol::Auto, &ctx.outcome);
    println!(
        "  {}",
        ctx.theme.paint(
            &format!(
                "escapade notification --title \"Build finished\" \"42 tests passed\"   \
                 uses {} here",
                choice.protocol.label()
            ),
            Ink::Muted
        )
    );
    println!();

    match flow {
        Flow::Prev => Ok(Flow::Prev),
        _ => Ok(Flow::Quit),
    }
}

/// Maps an event to a navigation choice, for the animated step that polls.
fn advance_from(event: &Event) -> Option<Flow> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    match key.code {
        KeyCode::Enter | KeyCode::Char(' ') => Some(Flow::Next),
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => Some(Flow::Quit),
        KeyCode::Char('p') | KeyCode::Left | KeyCode::Backspace => Some(Flow::Prev),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_has_a_title() {
        for step in STEPS {
            assert!(!step.title.is_empty());
            assert!(
                step.title.chars().count() < 60,
                "title too long for the header: {:?}",
                step.title
            );
        }
    }

    #[test]
    fn every_step_name_is_kebab_case() {
        // Names go in scripts and bug reports, so they must be typeable without
        // quoting and stable under `--only`'s case-insensitive matching.
        for step in STEPS {
            let name = step.name;
            assert!(!name.is_empty(), "{:?} has no name", step.title);
            assert!(
                name.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
                "not kebab-case: {name:?}"
            );
            assert!(!name.starts_with('-') && !name.ends_with('-'), "{name:?}");
            assert!(name.len() <= 12, "too long to type comfortably: {name:?}");
        }
    }

    #[test]
    fn step_names_are_unique() {
        let mut names = step_names();
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total, "two steps share a name");
    }

    #[test]
    fn the_protocol_steps_are_named_after_their_protocols() {
        // The whole reason names matter for this feature: someone checking
        // whether their terminal implements OSC 777 should be able to ask for
        // exactly that step.
        for name in ["osc9", "osc777", "osc99"] {
            assert!(
                step_names().contains(&name),
                "no step called {name:?}: {:?}",
                step_names()
            );
        }
    }

    #[test]
    fn only_selects_a_single_step_by_name() {
        let names = step_names();
        let selection = crate::features::select_steps(&names, Some("osc777"), None).unwrap();
        assert_eq!(STEPS[selection.start].name, "osc777");
        assert!(selection.single, "--only must not run on into the next step");

        // `osc9` and `osc99` share a prefix, so the exact match has to win over
        // the ambiguous-prefix error.
        let selection = crate::features::select_steps(&names, Some("osc9"), None).unwrap();
        assert_eq!(STEPS[selection.start].name, "osc9");
        let selection = crate::features::select_steps(&names, Some("OSC99"), None).unwrap();
        assert_eq!(STEPS[selection.start].name, "osc99");

        // A prefix that is unique still resolves.
        assert_eq!(
            STEPS[crate::features::select_steps(&names, Some("osc7"), None)
                .unwrap()
                .start]
                .name,
            "osc777"
        );

        // A name that matches nothing lists the real ones, which is the only
        // useful thing to say to someone who mistyped.
        let error = crate::features::select_steps(&names, Some("nope"), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("osc9"), "{error}");
        assert!(error.contains("chunked"), "{error}");
    }

    #[test]
    fn step_and_only_are_the_only_ways_in() {
        let names = step_names();
        // `--step N` is 1-based and carries on from there.
        let selection = crate::features::select_steps(&names, None, Some(1)).unwrap();
        assert_eq!(selection.start, 0);
        assert!(!selection.single);
        assert_eq!(
            crate::features::select_steps(&names, None, Some(999))
                .unwrap()
                .start,
            STEPS.len() - 1,
            "out of range clamps to the last step"
        );
        assert_eq!(
            crate::features::select_steps(&names, None, None)
                .unwrap()
                .start,
            0
        );
    }

    #[test]
    fn the_demo_ends_by_leaving_the_alternate_screen() {
        let last = &STEPS[STEPS.len() - 1];
        assert_eq!(last.name, "restore");
        assert!(last.title.contains("back"), "last step is {:?}", last.title);
        assert!(std::ptr::fn_addr_eq(last.run, step_leave as StepFn));
    }

    #[test]
    fn the_arc_builds_from_the_simplest_protocol_up() {
        let names = step_names();
        let position = |needle: &str| {
            names
                .iter()
                .position(|name| *name == needle)
                .unwrap_or_else(|| panic!("no step called {needle}"))
        };
        assert!(position("osc9") < position("osc777"));
        assert!(position("osc777") < position("osc99"));
        assert!(position("osc99") < position("replace"));
        assert!(position("replace") < position("chunked"));
        assert!(position("chunked") < position("build"));
    }

    #[test]
    fn advance_keys_map_the_way_the_footer_promises() {
        let key = |code| {
            Event::Key(crate::input::KeyEvent {
                code,
                mods: crate::input::Modifiers::NONE,
                kind: KeyEventKind::Press,
            })
        };
        assert_eq!(advance_from(&key(KeyCode::Char(' '))), Some(Flow::Next));
        assert_eq!(advance_from(&key(KeyCode::Enter)), Some(Flow::Next));
        assert_eq!(advance_from(&key(KeyCode::Char('q'))), Some(Flow::Quit));
        assert_eq!(advance_from(&key(KeyCode::Esc)), Some(Flow::Quit));
        assert_eq!(advance_from(&key(KeyCode::Char('p'))), Some(Flow::Prev));
        assert_eq!(advance_from(&key(KeyCode::Char('z'))), None);
    }

    #[test]
    fn key_release_never_advances_a_step() {
        let release = Event::Key(crate::input::KeyEvent {
            code: KeyCode::Enter,
            mods: crate::input::Modifiers::NONE,
            kind: KeyEventKind::Release,
        });
        assert_eq!(advance_from(&release), None);
    }

    #[test]
    fn every_demo_notification_shares_one_id_so_they_replace_each_other() {
        // The politeness rule, as a test: a demo that leaves nine notifications
        // stacked in the corner of the screen is not a demo, it is an attack.
        let request = demo_request(Some("t"), "b");
        assert_eq!(request.id.as_deref(), Some(DEMO_ID));
        let built = build(Protocol::Osc99, &request);
        for sequence in &built.sequences {
            assert!(
                sequence.contains(&format!("i={DEMO_ID}")),
                "{sequence:?}"
            );
        }
    }

    #[test]
    fn the_demo_id_survives_sanitizing_unchanged() {
        assert_eq!(
            crate::features::notification::sequence::safe_id(DEMO_ID).as_deref(),
            Some(DEMO_ID)
        );
    }

    #[test]
    fn the_chunking_step_really_does_need_several_escape_codes() {
        // If the sample body ever shrinks below the limit, the step would show a
        // single escape code and quietly stop making its point.
        let long = format!(
            "{} ; and it keeps going.",
            "This body is deliberately far longer than one escape code can carry. ".repeat(50)
        );
        let built = build(Protocol::Osc99, &demo_request(Some("A long story"), &long));
        assert!(
            built.sequences.len() >= 3,
            "only {} escape codes",
            built.sequences.len()
        );
        assert!(built.sequences.iter().all(|piece| piece.len() <= 4200));
    }

    #[test]
    fn the_refusal_message_has_the_shape_the_other_features_use() {
        let theme = Theme::plain();
        let identity = probe::Identity {
            known: probe::known_by_token("alacritty"),
            source: "TERM_PROGRAM".to_owned(),
            raw: "alacritty".to_owned(),
        };
        let mut outcome = ProbeOutcome::from_environment();
        outcome.verdicts = probe::infer(
            &identity,
            None,
            probe::Measurement {
                answered_barrier: true,
                multiplexed: false,
                interactive: true,
            },
        );
        outcome.identity = identity;
        outcome.multiplexer = None;
        outcome.interactive = true;
        assert!(!outcome.any_supported(), "the gate the refusal sits behind");
        // Printing it is the only way to exercise the wrapping and the branches.
        refuse(&theme, &outcome);
    }
}
