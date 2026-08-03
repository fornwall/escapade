//! The step-by-step demo.
//!
//! Each step states what it is about to do, shows the actual escape sequence and
//! names every piece of it, and then sends it for real — so the thing to watch is
//! not the terminal's grid but the **taskbar button, dock icon or tab title**. That
//! is unusual enough to be worth saying on every step, because a reader looking at
//! the middle of the screen will conclude that nothing happened.
//!
//! The arc is: the shape of the sequence, a percentage climbing for real, then the
//! three states that have no percentage, then taking the indicator away — which is
//! the step that matters most, because an indicator nobody clears is an indicator
//! stuck at 60% until the window closes.
//!
//! Whatever happens, including an error part way through and including `q` on the
//! first step, the run ends by sending state `0`. A demo that leaves a stuck
//! progress bar in somebody's taskbar is worse than no demo.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::{
    db::Catalog,
    features::{
        progress::{
            probe::{self, ProbeOutcome},
            sequence::{NUMBER, Percent, Report, SUBCOMMAND, State},
        },
        wrap,
    },
    input::{Advance, Event, InputReader, KeyCode, KeyEventKind},
    term::{CSI, Term, pretty},
    theme::{Ink, Theme},
};

/// Left margin for everything the demo draws.
const MARGIN: u16 = 3;

/// Frame interval for the animated step: about 30 frames per second.
const FRAME: Duration = Duration::from_millis(33);

/// How long the simulated job in the climbing step takes, in milliseconds.
const JOB_MILLIS: u64 = 6000;

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

/// The script. Order matters: `clear` is only interesting after something has
/// been left on screen, and `restore` has to be last.
const STEPS: &[Step] = &[
    step!("overview", "One sequence, five states", step_overview),
    step!("climb", "A percentage, climbing, for real", step_climb),
    step!("pulse", "Indeterminate: busy, total unknown", step_pulse),
    step!("error", "Error: the same bar, turned red", step_error),
    step!(
        "warning",
        "Warning: paused, or wanting attention",
        step_warning
    ),
    step!(
        "clear",
        "Taking it away again, which is not optional",
        step_clear
    ),
    step!("collision", "The OSC 9 collision", step_collision),
    step!("restore", "Putting the terminal back", step_leave),
];

/// The step names, in order, for [`crate::features::select_steps`].
fn step_names() -> Vec<&'static str> {
    STEPS.iter().map(|step| step.name).collect()
}

/// Prints the step list for `--list`.
pub fn list_steps(theme: &Theme) {
    println!();
    println!("  {}", theme.paint("Taskbar progress demo", Ink::Title));
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
            "escapade progress demo --only <NAME>   run just that step",
            Ink::Muted
        )
    );
    println!(
        "  {}",
        theme.paint(
            "escapade progress demo --step N        start there and carry on",
            Ink::Muted
        )
    );
    println!();
    println!(
        "  {}",
        theme.paint(
            "Watch the taskbar, dock icon or tab title rather than the screen — that is where \
             this feature draws. However the demo ends, it sends state 0 on the way out.",
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
    catalog: &Catalog,
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

    let outcome = probe::probe(catalog, &mut term, &mut input)?;

    // `--force` skips the capability check, but it cannot conjure a terminal:
    // enabling raw mode on something that is not a tty fails with ENXIO, so the
    // interactive gate comes first and is not overridable.
    if !outcome.interactive || (!outcome.looks_supported() && !force) {
        term.restore();
        refuse(theme, &outcome);
        return Ok(());
    }

    let mut index = selection.start;

    term.enable_raw()?;
    term.enter_alt_screen()?;
    term.hide_cursor()?;

    let mut ctx = Ctx {
        forced: force && !outcome.looks_supported(),
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

    // Unconditional, and deliberately outside the `?` path above: quitting on the
    // first step, or failing half way through the climb, must not leave a taskbar
    // button sitting at 40% for the rest of the day.
    let _ = term.write_now(&Report::clear().sequence());
    term.restore();
    result
}

/// The refusal shown when the probe found positive evidence of non-support.
fn refuse(theme: &Theme, outcome: &ProbeOutcome) {
    println!();
    let message = "sorry - this does not seem to be supported by your terminal - check \
                   'escapade progress explain' for support, or use \
                   'escapade progress demo --force' to disable feature probing.";
    for line in wrap(message, theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Warn));
    }

    println!();
    for line in wrap(&outcome.reason(), theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }

    if !outcome.interactive {
        println!();
        for line in wrap(
            "stdout and stdin are not both terminals, so there is nothing to draw on and nothing \
             that could be asked. Progress reports themselves travel down a pipe perfectly well — \
             `escapade progress 42` works there — it is only the demo that needs a terminal.",
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
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
    /// What the probe concluded.
    outcome: ProbeOutcome,
    /// Whether the demo is running past a probe that said no.
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
        let heading = "escapade · taskbar progress";
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

    /// Sends one report for real, and says what to look for.
    ///
    /// Every step goes through here, so "the demo only ever sends progress
    /// reports" is a property of the code rather than a promise in a comment.
    fn send(&mut self, report: Report) -> Result<()> {
        self.term.write_now(&report.sequence())?;
        let sent = format!("→ sent {} bytes", report.sequence().len());
        self.indented(2, &self.theme.paint(&sent, Ink::Good))?;
        let expectation = format!(
            "look at the taskbar or tab, not here: {}",
            report.expectation()
        );
        self.note(&expectation)?;
        self.caveat()?;
        Ok(())
    }

    /// Explains why nothing may have appeared.
    fn caveat(&mut self) -> Result<()> {
        if self.forced {
            let reason = self.outcome.reason();
            self.note(&format!(
                "The probe found evidence against this working here — {reason} — and you asked for \
                 --force anyway. The escape code was still sent; an unrecognised OSC string is \
                 discarded silently.",
            ))?;
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

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// The shape of the sequence, and the five states.
fn step_overview(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(0)?;
    ctx.para(
        "One sequence does the whole feature. It carries a state and a number, and it draws \
         nothing on the screen: what moves is the taskbar button, the dock icon, the tab title or \
         the pane border — wherever this terminal decided to put it.",
    )?;
    ctx.blank();

    ctx.sequence(
        &Report::set(Percent::clamping(42).0).sequence(),
        &[
            (
                "OSC",
                "Operating System Command, ESC ]. A terminal that does not recognise what follows \
                 swallows the whole string and moves on, which is why this is safe to send without \
                 asking permission.",
            ),
            (
                "9",
                "the number ConEmu picked — and the number iTerm2 had already picked for desktop \
                 notifications. That collision has its own step later.",
            ),
            (
                "4",
                "ConEmu's sub-command for progress. It had others (a message box, the working \
                 directory); only this one was copied.",
            ),
            (
                "1",
                "the state: 0 remove, 1 set, 2 error, 3 indeterminate, 4 warning or paused.",
            ),
            (
                "42",
                "the percentage, 0 to 100. Meaningful only for state 1; states 0 and 3 ignore it, \
                 and 2 and 4 treat it as optional — though what \"absent\" means differs per \
                 terminal.",
            ),
            (
                "ST",
                "String Terminator, ESC \\. BEL works nearly everywhere too — but VTE rejects a \
                 BEL-terminated progress report on purpose, and Microsoft's own tutorial uses BEL, \
                 which is why so many copied snippets do nothing on GNOME Terminal. escapade \
                 always sends ST.",
            ),
        ],
    )?;
    ctx.blank();

    for state in State::ALL {
        let line = format!(
            "{}  {}  {}",
            ctx.theme.paint(&format!("{}", state.code()), Ink::Strong),
            ctx.theme
                .paint(&format!("{:<14}", state.flag()), Ink::Accent),
            ctx.theme.paint(
                &format!(
                    "{} [percentage {}]",
                    state.describe(),
                    state.percent_use().label()
                ),
                Ink::Muted
            )
        );
        ctx.indented(2, &line)?;
    }
    ctx.blank();
    ctx.note(&format!(
        "This terminal: {}. There is no query for this protocol — no question to ask and no reply \
         defined — so that is inference, not measurement.",
        ctx.outcome.reason()
    ))?;

    ctx.wait()
}

/// A real percentage, climbing.
fn step_climb(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(1)?;
    ctx.para(
        "Now for real. A pretend job runs for six seconds, and every time the whole-number \
         percentage changes, one escape code goes out. That is 101 sequences over six seconds — \
         and not one more, because sending the same value twice is pure noise for the window \
         manager to animate.",
    )?;
    ctx.blank();
    ctx.sequence(
        &Report::set(Percent::ZERO).sequence(),
        &[(
            "1 ; <n>",
            "state 1 with a percentage, sent again for each new value. There is no \"begin\" and no \
             \"end\": the indicator simply holds whatever it was last told.",
        )],
    )?;
    ctx.blank();

    let bar_row = ctx.row;
    ctx.blank();
    ctx.blank();
    let hint = ctx
        .theme
        .paint("space/enter next · p back · q quit", Ink::Muted);
    let footer_row = ctx.term.rows.saturating_sub(2);
    ctx.term.put(MARGIN, footer_row, &hint)?;

    let started = Instant::now();
    let mut last_sent: Option<Percent> = None;
    let mut sent = 0_u32;

    loop {
        // Integer arithmetic throughout: a progress bar is a ratio of cells to
        // cells, and going via floating point only adds cast lints.
        let elapsed = u64::try_from(started.elapsed().as_millis())
            .unwrap_or(JOB_MILLIS)
            .min(JOB_MILLIS);
        let (percent, _) = Percent::clamping(100 * elapsed / JOB_MILLIS);
        let done = elapsed >= JOB_MILLIS;

        if last_sent != Some(percent) {
            ctx.term.write(&Report::set(percent).sequence())?;
            last_sent = Some(percent);
            sent += 1;
        }

        ctx.term.begin_sync()?;
        let width = u64::from(ctx.term.cols.saturating_sub(MARGIN * 2 + 14)).min(48);
        let filled = width * u64::from(percent.get()) / 100;
        let bar = format!(
            "[{}{}] {:>3}%",
            "━".repeat(usize::try_from(filled).unwrap_or(0)),
            "╌".repeat(usize::try_from(width.saturating_sub(filled)).unwrap_or(0)),
            percent.get()
        );
        let painted = ctx
            .theme
            .paint(&bar, if done { Ink::Good } else { Ink::Accent });
        ctx.term.put(MARGIN + 2, bar_row, &painted)?;
        ctx.term.clear_to_eol()?;

        let status = format!(
            "{} escape {} sent · on the wire: {}",
            sent,
            if sent == 1 { "code" } else { "codes" },
            pretty(&Report::set(percent).sequence())
        );
        let painted = ctx.theme.paint(&status, Ink::Muted);
        ctx.term.put(MARGIN + 2, bar_row + 2, &painted)?;
        ctx.term.clear_to_eol()?;
        ctx.term.end_sync()?;

        if let Some(event) = ctx.input.read(FRAME)?
            && let Some(flow) = advance_from(&event)
        {
            return Ok(flow);
        }
    }
}

/// State 3: indeterminate.
fn step_pulse(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(2)?;
    ctx.para(
        "Plenty of work has no denominator. Resolving dependencies, waiting on a lock, streaming \
         something of unknown length — state 3 says \"busy, no idea how far\", and the platform \
         draws whatever it uses for that: a pulsing bar on Windows, a barber pole elsewhere.",
    )?;
    ctx.blank();

    let report = Report::new(State::Indeterminate, None);
    ctx.sequence(
        &report.sequence(),
        &[
            (
                "3",
                "indeterminate. Windows Terminal's documentation says the percentage is ignored \
                 here; ConEmu's says nothing about it at all, and VTE quietly forces the stored \
                 value to 0. escapade drops the number rather than sending one whose fate depends \
                 on the terminal.",
            ),
            (
                "0",
                "the field is still written, because a four-field form is what every \
                 implementation was built against. Its value is irrelevant.",
            ),
        ],
    )?;
    ctx.blank();
    ctx.send(report)?;
    ctx.blank();
    ctx.note(
        "There is no timeout on this. An indeterminate indicator pulses until something else is \
         sent — which is the whole argument for sending state 0 when your program exits, however \
         it exits.",
    )?;

    ctx.wait()
}

/// State 2: error.
fn step_error(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(3)?;
    ctx.para(
        "State 2 says the job failed. On Windows this is the taskbar's red bar; elsewhere it is \
         whatever the terminal chose. The useful detail is that the percentage is optional: leave \
         it out and the indicator keeps the position it already had, so the bar stays where the \
         failure happened rather than jumping.",
    )?;
    ctx.blank();

    // Put a value on the indicator first, so that keeping it is visible.
    let sixty = Report::set(Percent::clamping(60).0);
    ctx.indented(
        2,
        &ctx.theme.paint(
            &format!("first, back to 60%:  {}", pretty(&sixty.sequence())),
            Ink::Muted,
        ),
    )?;
    ctx.term.write_now(&sixty.sequence())?;
    ctx.blank();

    let report = Report::new(State::Error, None);
    ctx.sequence(
        &report.sequence(),
        &[
            ("2", "error."),
            (
                "(no value)",
                "three fields, not four. ConEmu documents the percentage as optional here, and \
                 omitting it is the difference between \"turn the bar I have red\" and \"turn it \
                 red and move it to zero\". On Windows Terminal an explicit 0 is indistinguishable \
                 from an absent field, so omitting is the only way to ask for the first.",
            ),
        ],
    )?;
    ctx.blank();
    ctx.send(report)?;
    ctx.blank();
    ctx.note(
        "It is not portable, though: VTE reads the absent field as 0% and iTerm2 shows a pulsing \
         indeterminate error instead of a positioned red bar. A percentage can be given as well — \
         `escapade progress 60 --state error` sends OSC 9 ; 4 ; 2 ; 60 ST — and that is the form \
         to prefer when the position matters.",
    )?;

    ctx.wait()
}

/// State 4: warning / paused.
fn step_warning(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(4)?;
    ctx.para(
        "State 4 is the yellow one: a build waiting on a passphrase, a deploy waiting on a \
         confirmation. Like the error state, the percentage is optional here.",
    )?;
    ctx.blank();

    let report = Report::new(State::Warning, None);
    ctx.sequence(
        &report.sequence(),
        &[(
            "4",
            "the one state whose *name* forked. ConEmu, tmux, VTE, Ghostty and WezTerm call it \
             paused; Windows Terminal and iTerm2 call it warning. The yellow is the same either \
             way, and escapade spells it --state warning.",
        )],
    )?;
    ctx.blank();
    ctx.send(report)?;
    ctx.blank();
    ctx.note(
        "iTerm2 is the exception to \"optional\": it documents the percentage as required for this \
         state and does nothing at all without one. And a terminal that shows progress as a tab \
         ring has nowhere to put a colour, so on it states 2 and 4 can look exactly like ordinary \
         progress. Konsole has not implemented either of them yet. Nothing reports any of that \
         back to you.",
    )?;

    ctx.wait()
}

/// State 0: remove, and why it is mandatory.
fn step_clear(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(5)?;
    ctx.para(
        "This is the step that matters. A progress indicator is terminal *state*, like a mode or a \
         title — not output. Nothing takes it down when your process exits, so a script that is \
         killed at 40% leaves a taskbar button sitting at 40% until the user closes the window.",
    )?;
    ctx.blank();

    let report = Report::clear();
    ctx.sequence(
        &report.sequence(),
        &[
            ("0", "remove the indicator. Nothing else does this."),
            (
                "0",
                "the percentage, ignored. Written anyway, for the same reason as everywhere else: \
                 four fields is the shape every implementation was built against.",
            ),
        ],
    )?;
    ctx.blank();
    ctx.send(report)?;
    ctx.blank();
    ctx.note(
        "Put it in a trap: `trap 'escapade progress --clear' EXIT INT TERM`. `escapade progress \
         --watch` does it for you when its input closes, however that happens, and this demo does \
         it on the way out even if you quit half way through the climb.",
    )?;

    ctx.wait()
}

/// The collision with iTerm2's notification sequence.
fn step_collision(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(6)?;
    ctx.para(
        "The one thing about this feature that will actually bite you. Two protocols were given \
         the same number by different people, years apart, and both are still in use.",
    )?;
    ctx.blank();

    ctx.indented(
        2,
        &ctx.theme.paint(
            &pretty(&format!(
                "{}9;Build finished{}",
                crate::term::OSC,
                crate::term::ST
            )),
            Ink::Code,
        ),
    )?;
    ctx.indented(
        4,
        &ctx.theme.paint(
            "iTerm2, before 2010: raise a desktop notification",
            Ink::Muted,
        ),
    )?;
    ctx.blank();
    ctx.indented(
        2,
        &ctx.theme.paint(
            &pretty(&Report::set(Percent::clamping(50).0).sequence()),
            Ink::Code,
        ),
    )?;
    ctx.indented(
        4,
        &ctx.theme
            .paint("ConEmu, later: move the taskbar progress bar", Ink::Muted),
    )?;
    ctx.blank();

    ctx.para(
        "They are told apart only by what follows the first semicolon, and most terminals \
         implement one and not the other. So the failure modes are not \"nothing happens\":",
    )?;
    ctx.blank();
    for (terminal, what) in [
        (
            "kitty",
            "has the notification and not the progress form — a progress report arrives as a \
             desktop toast reading 4;1;50",
        ),
        (
            "Windows Terminal",
            "has the progress form and not the notification — a notification whose text begins 4; \
             moves the taskbar instead of appearing, and any other notification is discarded in \
             silence",
        ),
        (
            "Rio, Ghostty, WezTerm",
            "implement both and disambiguate: OSC 9 ; 4 ; … is progress, anything else is a \
             notification. Ghostty's release notes put it plainly: you cannot send a notification \
             starting with ;4",
        ),
        (
            "iTerm2",
            "implements both with the strictest rule of all: any numeric leading field means it is \
             not a notification, so OSC 9 ; 42 ST is silently dropped rather than shown",
        ),
    ] {
        let lines = wrap(what, ctx.width.saturating_sub(26));
        for (index, line) in lines.iter().enumerate() {
            if index == 0 {
                let text = format!(
                    "{}  {}",
                    ctx.theme.paint(&format!("{terminal:<22}"), Ink::Accent),
                    ctx.theme.paint(line, Ink::Muted)
                );
                ctx.indented(2, &text)?;
            } else {
                let text = ctx.theme.paint(line, Ink::Muted);
                ctx.indented(26, &text)?;
            }
        }
    }
    ctx.blank();
    ctx.note(
        "escapade guards the other direction: `escapade notification` prefixes a space to a \
         message that begins with a digit and a semicolon, so a notification reading \"4;50 done\" \
         cannot be mistaken for a progress report.",
    )?;

    ctx.wait()
}

/// The last step: hand the terminal back.
fn step_leave(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(7)?;
    ctx.para(
        "Two things to put back, and only one of them belongs to this feature. The indicator goes \
         first, then the alternate screen.",
    )?;
    ctx.blank();

    ctx.sequence(
        &Report::clear().sequence(),
        &[(
            "0",
            "the indicator, removed. Sent here as a step you can see, and sent again \
             unconditionally on the way out, because it is the one piece of cleanup that this \
             protocol makes the application's job.",
        )],
    )?;
    ctx.blank();
    ctx.term.write_now(&Report::clear().sequence())?;
    ctx.indented(2, &ctx.theme.paint("✔ cleared", Ink::Good))?;
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
        ctx.theme.paint("the sequence:", Ink::Accent),
        ctx.theme.paint(
            &format!("OSC {NUMBER} ; {SUBCOMMAND} ; <state> ; <progress> ST"),
            Ink::Code
        )
    );
    for (command, meaning) in [
        ("escapade progress 42", "one report"),
        ("… | escapade progress --watch", "driven by a stream"),
        ("escapade progress --clear", "put it away"),
    ] {
        println!(
            "  {}   {}",
            ctx.theme.paint(&format!("{command:<30}"), Ink::Code),
            ctx.theme.paint(meaning, Ink::Muted)
        );
    }
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
// clippy.toml sets allow-unwrap-in-tests, but the attribute is kept for the
// helpers in here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::select_steps;

    #[test]
    fn every_step_has_a_title_that_fits_the_header() {
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
    fn every_step_name_is_kebab_case_and_typeable() {
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
    fn step_names_are_unique_and_resolvable() {
        let names = step_names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        let total = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), total, "two steps share a name");

        for (index, name) in names.iter().enumerate() {
            let selection = select_steps(&names, Some(name), None).unwrap();
            assert_eq!(selection.start, index, "{name}");
            assert!(selection.single, "--only runs one step");
        }
    }

    #[test]
    fn the_states_each_get_a_step_of_their_own() {
        // Somebody checking whether their terminal draws the yellow one should be
        // able to ask for exactly that.
        for name in ["climb", "pulse", "error", "warning", "clear"] {
            assert!(
                step_names().contains(&name),
                "no step called {name:?}: {:?}",
                step_names()
            );
        }
    }

    #[test]
    fn leaving_the_alternate_screen_is_the_last_step() {
        assert_eq!(
            STEPS.last().map(|step| step.name),
            Some("restore"),
            "the alternate screen must be left as a visible final step"
        );
    }

    #[test]
    fn the_collision_gets_its_own_step() {
        assert!(step_names().contains(&"collision"));
    }

    #[test]
    fn navigation_keys_map_to_the_expected_flow() {
        use crate::input::{KeyEvent, Modifiers};
        let key = |code: KeyCode| {
            Event::Key(KeyEvent {
                code,
                mods: Modifiers::NONE,
                kind: KeyEventKind::Press,
            })
        };
        assert_eq!(advance_from(&key(KeyCode::Enter)), Some(Flow::Next));
        assert_eq!(advance_from(&key(KeyCode::Char(' '))), Some(Flow::Next));
        assert_eq!(advance_from(&key(KeyCode::Char('q'))), Some(Flow::Quit));
        assert_eq!(advance_from(&key(KeyCode::Esc)), Some(Flow::Quit));
        assert_eq!(advance_from(&key(KeyCode::Char('p'))), Some(Flow::Prev));
        assert_eq!(advance_from(&key(KeyCode::Char('x'))), None);
    }

    #[test]
    fn advance_maps_onto_flow() {
        assert_eq!(Flow::from(Advance::Next), Flow::Next);
        assert_eq!(Flow::from(Advance::Prev), Flow::Prev);
        assert_eq!(Flow::from(Advance::Quit), Flow::Quit);
    }
}
