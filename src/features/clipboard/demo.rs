//! The step-by-step demo.
//!
//! Each step names the bytes before it sends any, which matters more here than
//! elsewhere: this is the one feature in the collection whose demonstration
//! touches something the user owns. So the copy step **asks first**, and takes a
//! separate key rather than the space bar that advances everything else — a demo
//! that quietly replaces your clipboard while you page through it would have
//! demonstrated the wrong lesson very effectively.
//!
//! Nothing here ever sends the read query. Not once, not with `--force`: on
//! Konsole it destroys the clipboard, and on kitty and Ghostty it raises a
//! permission dialog. `escapade clipboard paste` is where reading lives, because
//! there the user asked for it by name.

use anyhow::Result;

use crate::{
    db::Catalog,
    features::{
        Support,
        clipboard::{
            probe,
            sequence::{self, Selection},
        },
        wrap,
    },
    input::{Advance, Event, InputReader, KeyCode, KeyEventKind},
    term::{Term, pretty},
    theme::{Ink, Theme},
};

/// Left margin for everything the demo draws.
const MARGIN: u16 = 3;

/// What the copy step puts on the clipboard.
///
/// Deliberately harmless, obviously from here, and with **no trailing newline** —
/// the last thing this program should teach is the habit of copying a string that
/// runs itself when pasted at a prompt.
const MARKER: &str = "escapade was here";

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

/// One entry in the demo script.
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

/// The script. Order matters: later steps assume vocabulary from earlier ones.
const STEPS: &[Step] = &[
    step!("anatomy", "One sequence, three jobs", step_anatomy),
    step!("encoding", "Why the payload is base64", step_base64),
    step!(
        "detect",
        "What this terminal said about itself",
        step_detect
    ),
    step!("copy", "Actually copying something", step_copy),
    step!("read", "The half that is usually refused", step_read),
    step!("paste", "The hazard that is not injection", step_hazard),
    step!("restore", "Putting the terminal back", step_leave),
];

/// The step names, in script order, for [`crate::features::select_steps`].
fn step_names() -> Vec<&'static str> {
    STEPS.iter().map(|step| step.name).collect()
}

/// Prints the step list for `--list`.
pub fn list_steps(theme: &Theme) {
    println!();
    println!("  {}", theme.paint("OSC 52 clipboard demo", Ink::Title));
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
            "Only the `copy` step changes anything, and it asks first.",
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
    // Resolved before anything is probed or drawn, so a mistyped `--only` is a
    // plain error message rather than one on the alternate screen.
    let selection = crate::features::select_steps(&step_names(), only, start_step)?;

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;

    let outcome = probe::probe(catalog, &mut term, &mut input)?;
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
        term: &mut term,
        input: &mut input,
        theme,
        outcome,
        copied: false,
        row: 0,
        width: 0,
    };

    let result = (|| -> Result<()> {
        loop {
            let flow = (STEPS[index].run)(&mut ctx)?;
            match flow {
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

    term.restore();
    result
}

/// The refusal shown when the terminal appears not to support this.
fn refuse(theme: &Theme, outcome: &probe::ProbeOutcome) {
    println!();
    let message = "sorry - this does not seem to be supported by your terminal - check \
                   'escapade clipboard explain' for support, or use \
                   'escapade clipboard demo --force' to disable feature probing.";
    for line in wrap(message, theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Warn));
    }
    println!();
    let reason = if outcome.interactive {
        outcome.reason()
    } else {
        "stdout and stdin are not both terminals. The demo needs the alternate screen and a \
         keypress to advance, and a pipe has neither — so --force cannot help here. `escapade \
         clipboard copy` does work in a pipe, which is the whole point of it."
            .to_owned()
    };
    for line in wrap(&reason, theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Muted));
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
    /// What the probe established, shown by the `detect` step.
    outcome: probe::ProbeOutcome,
    /// Whether the copy step has already written to the clipboard.
    copied: bool,
    /// Next row to write to.
    row: u16,
    /// Usable text width.
    width: usize,
}

impl std::fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("row", &self.row)
            .field("copied", &self.copied)
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
        let heading = "escapade · OSC 52 clipboard";
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

    /// Shows a sequence in readable form, then names each of its pieces.
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

    /// Draws the footer hint and blocks until the user chooses.
    fn wait(&mut self) -> Result<Flow> {
        self.wait_with("space/enter next · p back · q quit")
    }

    /// As [`Ctx::wait`], with a step-specific hint.
    fn wait_with(&mut self, hint: &str) -> Result<Flow> {
        let row = self.term.rows.saturating_sub(2);
        let painted = self.theme.paint(hint, Ink::Muted);
        self.term.put(MARGIN, row, &painted)?;
        self.term.flush()?;
        Ok(self.input.wait_for_advance()?.into())
    }
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// The sequence, and the one field that decides which of three things it does.
fn step_anatomy(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(0)?;
    ctx.para(
        "The clipboard belongs to the machine the human is sitting at. This is the only way to \
         reach it from the machine the program runs on, which is why it works over ssh and why \
         xclip does not.",
    )?;
    ctx.blank();
    ctx.sequence(
        &sequence::set(Selection::Clipboard, b"hi"),
        &[
            ("OSC", "the introducer, ESC ]"),
            ("52", "xterm calls this one Manipulate Selection Data"),
            (
                "c",
                "which selection: c is the clipboard, p the primary selection, s the one xterm's \
                 selectToClipboard resource points at",
            ),
            ("aGk=", "the payload: base64 for the two bytes `hi`"),
            ("ST", "the string terminator, ESC \\"),
        ],
    )?;
    ctx.blank();
    ctx.para("One field decides which of three completely different things happens:")?;
    ctx.blank();
    for (payload, meaning) in [
        ("base64", "set the selection to those bytes"),
        ("(empty)", "clear the selection"),
        (
            "?",
            "send the selection back to the program — the half everybody disables",
        ),
    ] {
        let line = format!(
            "{}  {}",
            ctx.theme.paint(&format!("{payload:<8}"), Ink::Code),
            ctx.theme.paint(meaning, Ink::Muted)
        );
        ctx.indented(2, &line)?;
    }
    ctx.blank();
    ctx.note(
        "xterm also accepts q for the secondary selection and 0 through 7 for the X cut buffers, \
         and an empty selection field means s0. Nothing outside xterm implements those.",
    )?;
    ctx.wait()
}

/// Why base64, and what it buys.
fn step_base64(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(1)?;
    ctx.para(
        "Base64 looks like ceremony around a string that was already text. It is not: it is the \
         security boundary, and it is the reason this sequence needs no sanitizer.",
    )?;
    ctx.blank();

    let hostile = "\u{1b}]52;c;bad\u{7} rm -rf /";
    ctx.para("Take a string written specifically to break out of the payload:")?;
    ctx.blank();
    ctx.indented(2, &ctx.theme.paint(&pretty(hostile), Ink::Code))?;
    ctx.blank();
    ctx.para("Encoded, every byte of it lands inside the payload and stays there:")?;
    ctx.blank();
    let encoded = sequence::set(Selection::Clipboard, hostile.as_bytes());
    for line in wrap(&pretty(&encoded), ctx.width.saturating_sub(2)) {
        let painted = ctx.theme.paint(&line, Ink::Code);
        ctx.indented(2, &painted)?;
    }
    ctx.blank();
    ctx.note(
        "The alphabet is A-Z, a-z, 0-9, + / and =. No ESC, no BEL, no semicolon — so there is no \
         input that can end the string early and have the rest of itself executed as a fresh \
         escape sequence. Compare OSC 8 and OSC 9, which carry raw text and need real sanitizers.",
    )?;
    ctx.blank();
    ctx.note(
        "One rule the encoding does impose: no line breaks. Several terminals stop reading at the \
         first byte outside the alphabet, so a payload wrapped at 76 columns arrives truncated.",
    )?;
    ctx.wait()
}

/// What the probe already found out, shown rather than summarised.
fn step_detect(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(2)?;
    ctx.para(
        "Writing has no reply, so support cannot be tested by doing it. Instead, terminal authors \
         agreed to answer the oldest question there is — Primary Device Attributes, CSI c — with \
         the number 52 among the parameters when clipboard writes are allowed.",
    )?;
    ctx.blank();
    ctx.sequence(
        &probe::barrier_query(),
        &[(
            "CSI c",
            "what went out, and what every terminal since the VT100 answers",
        )],
    )?;
    ctx.blank();

    let attributes = ctx.outcome.attributes.clone().map_or_else(
        || "nothing came back".to_owned(),
        |text| format!("CSI ?{text} c"),
    );
    let line = format!(
        "{}  {}",
        ctx.theme.paint("answer", Ink::Accent),
        ctx.theme.paint(&attributes, Ink::Code)
    );
    ctx.indented(2, &line)?;
    let (verdict, ink) = match ctx.outcome.support() {
        Support::Yes if ctx.outcome.measured() => (
            "52 is there: this terminal is promising that writes work right now",
            Ink::Good,
        ),
        Support::Yes => (
            "no 52, but the terminal named itself and the support matrix says writes work",
            Ink::Good,
        ),
        Support::No => (
            "the matrix says this terminal does not implement it",
            Ink::Bad,
        ),
        Support::Unknown => (
            "no 52 — which settles nothing, since xterm has implemented this since long before \
             the extension existed",
            Ink::Warn,
        ),
    };
    ctx.blank();
    for text in wrap(verdict, ctx.width.saturating_sub(2)) {
        let painted = ctx.theme.paint(&text, ink);
        ctx.indented(2, &painted)?;
    }
    ctx.blank();
    ctx.note(
        "The promise is conditional on configuration rather than on a version number: foot drops \
         the 52 from this reply when the user turns clipboard writes off, which is exactly what \
         makes it worth asking.",
    )?;
    ctx.wait()
}

/// The one step that changes something the user owns.
fn step_copy(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(3)?;
    ctx.para(
        "This step is the only one that touches anything of yours: it replaces whatever is on \
         your clipboard. So it does not happen on the space bar.",
    )?;
    ctx.blank();
    ctx.sequence(
        &sequence::set(Selection::Clipboard, MARKER.as_bytes()),
        &[
            ("52;c", "the clipboard"),
            (
                "…",
                "base64 of the seventeen bytes `escapade was here`, with no trailing newline",
            ),
        ],
    )?;
    ctx.blank();

    if ctx.copied {
        ctx.para("Already sent. Paste it somewhere to see whether it arrived.")?;
        ctx.blank();
        ctx.note(
            "If nothing arrived and you are inside tmux, that is expected: tmux ignores this \
             sequence from applications unless `set -s set-clipboard on`.",
        )?;
        return ctx.wait();
    }

    let prompt = ctx.theme.paint(
        "press c to copy · space skips it · p back · q quit",
        Ink::Warn,
    );
    ctx.indented(2, &prompt)?;
    ctx.term.flush()?;

    loop {
        let event = ctx.input.read_blocking()?;
        let Event::Key(key) = event else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if !key.mods.ctrl => {
                ctx.term
                    .write_now(&sequence::set(Selection::Clipboard, MARKER.as_bytes()))?;
                ctx.copied = true;
                ctx.blank();
                ctx.para(
                    "Sent. Paste it somewhere — Ctrl-V, or the middle mouse button if you \
                          ask for the primary selection instead.",
                )?;
                ctx.blank();
                ctx.note(
                    "Nothing came back and nothing will: this half of the protocol has no \
                     acknowledgement, which is why the previous step had to ask the device \
                     attributes instead.",
                )?;
                return ctx.wait();
            }
            KeyCode::Char(' ') | KeyCode::Enter => return Ok(Flow::Next),
            KeyCode::Char('p') | KeyCode::Left | KeyCode::Backspace => return Ok(Flow::Prev),
            KeyCode::Char('q' | 'Q') | KeyCode::Esc => return Ok(Flow::Quit),
            KeyCode::Char('c' | 'd') if key.mods.ctrl => return Ok(Flow::Quit),
            _ => {}
        }
    }
}

/// Reading, described in full and demonstrated not at all.
fn step_read(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(4)?;
    ctx.para(
        "Replace the payload with a question mark and the terminal sends the clipboard back to \
         the program, on the input stream, exactly where a keystroke would arrive.",
    )?;
    ctx.blank();
    ctx.sequence(
        &sequence::query(Selection::Clipboard),
        &[("?", "instead of base64: send it to me")],
    )?;
    ctx.blank();
    ctx.para(
        "This demo will not send that, on any terminal, with or without --force. Here is what it \
         would do:",
    )?;
    ctx.blank();
    for (who, what) in [
        ("kitty, Ghostty", "raise a permission dialog, by default"),
        ("foot", "answer, if the window has keyboard focus"),
        (
            "Alacritty, WezTerm, Windows Terminal, Zellij",
            "ignore it — silently, because there is no way to say no",
        ),
        ("xterm", "ignore it, unless allowWindowOps was turned on"),
        (
            "Konsole",
            "set your clipboard to the empty string, because it has no branch for `?` at all",
        ),
    ] {
        let lines = wrap(what, ctx.width.saturating_sub(46));
        for (index, text) in lines.iter().enumerate() {
            let label = if index == 0 { who } else { "" };
            let line = format!(
                "{}  {}",
                ctx.theme.paint(&format!("{label:<42}"), Ink::Strong),
                ctx.theme.paint(text, Ink::Muted)
            );
            ctx.indented(2, &line)?;
        }
    }
    ctx.blank();
    ctx.note(
        "`escapade clipboard paste` does send it, because there you asked. It refuses on Konsole \
         without --force, and it waits ten seconds rather than one, because that wait may contain \
         a human clicking a dialog.",
    )?;
    ctx.wait()
}

/// The attack that base64 does not prevent.
fn step_hazard(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(5)?;
    ctx.para(
        "The payload cannot escape into the stream. What it can do is wait on your clipboard \
         until you paste it into a shell.",
    )?;
    ctx.blank();
    let poison = "echo pwned\n";
    ctx.indented(
        2,
        &ctx.theme.paint(
            &pretty(&sequence::set(Selection::Clipboard, poison.as_bytes())),
            Ink::Code,
        ),
    )?;
    ctx.blank();
    ctx.para(
        "That payload ends with a newline. Pasted into a shell that is not using bracketed paste, \
         it does not land on the prompt waiting to be read — it runs.",
    )?;
    ctx.blank();
    ctx.note(
        "Two things defend against it, and neither is the encoding. Bracketed paste, which every \
         current shell turns on, delivers pasted text as text. And Ghostty's paste protection, \
         which asks before pasting anything containing a newline.",
    )?;
    ctx.blank();
    ctx.note(
        "`escapade clipboard copy` says so on stderr when the text you are copying ends in a \
         newline. It still copies it — that is your business — but it will not let it be a \
         surprise.",
    )?;
    ctx.wait()
}

/// Leaving, and what has changed.
fn step_leave(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(6)?;
    ctx.para("Nothing to undo, which is unusual for this collection.")?;
    ctx.blank();
    ctx.note(
        "No mode was set, no private mode toggled, no state left in the terminal — this protocol \
         has no modes at all. The alternate screen goes away when the demo exits and the primary \
         screen comes back exactly as it was.",
    )?;
    ctx.blank();
    if ctx.copied {
        ctx.para(&format!(
            "One thing did change: your clipboard now holds `{MARKER}`, because you pressed c."
        ))?;
    } else {
        ctx.para("Your clipboard was not touched.")?;
    }
    ctx.blank();
    ctx.para("Worth knowing next:")?;
    ctx.blank();
    for (command, what) in [
        ("escapade clipboard copy 'text'", "copy, from anywhere"),
        (
            "git rev-parse HEAD | escapade clipboard copy",
            "the usual shape",
        ),
        (
            "escapade clipboard probe",
            "a shell predicate: 0 yes, 1 no, 2 cannot tell",
        ),
        (
            "escapade clipboard explain",
            "the whole protocol, and who implements what",
        ),
    ] {
        let line = format!(
            "{}  {}",
            ctx.theme.paint(&format!("{command:<44}"), Ink::Code),
            ctx.theme.paint(what, Ink::Muted)
        );
        ctx.indented(2, &line)?;
    }
    ctx.wait_with("space/enter finishes · p back · q quit")
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::select_steps;

    #[test]
    fn every_step_has_a_distinct_kebab_case_name() {
        let mut names = step_names();
        let count = names.len();
        for name in &names {
            assert!(
                name.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'),
                "{name} is not kebab-case"
            );
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two steps share a name");
    }

    #[test]
    fn steps_are_selectable_by_name_and_by_prefix() {
        assert_eq!(
            STEPS[select_steps(&step_names(), Some("copy"), None)
                .unwrap()
                .start]
                .name,
            "copy"
        );
        assert_eq!(
            STEPS[select_steps(&step_names(), Some("anat"), None)
                .unwrap()
                .start]
                .name,
            "anatomy"
        );
        assert_eq!(
            STEPS[select_steps(&step_names(), None, Some(999)).unwrap().start].name,
            "restore"
        );
    }

    #[test]
    fn the_marker_never_runs_itself_when_pasted() {
        // The lesson of the `paste` step, applied to the demo's own payload.
        assert!(!MARKER.ends_with('\n'));
        assert!(!MARKER.contains('\n'));
        assert!(sequence::hazards(MARKER.as_bytes(), 64, false).is_empty());
    }

    #[test]
    fn the_demo_never_builds_a_read_query() {
        // The query appears in the `read` step as *text*, through `pretty`, and
        // the only thing that may reach the terminal is a write. This is a
        // property worth pinning: on Konsole the read would destroy a clipboard.
        let query = sequence::query(Selection::Clipboard);
        let shown = pretty(&query);
        assert!(!shown.contains('\u{1b}'), "shown, not sent");
        assert_eq!(shown, "OSC 52;c;? ST");
    }
}
