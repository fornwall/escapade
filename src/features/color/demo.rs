//! The step-by-step demo.
//!
//! Unlike most demos in this project, this one has something to *show back*.
//! Progress moves a taskbar button you have to be told to look at; graphics
//! paints pixels. A colour query produces an answer, and the answer is about the
//! terminal the reader is sitting in front of — so every step here is run live
//! against their terminal and prints what it actually said, next to what that
//! means.
//!
//! The arc is: one round trip, in full; the trap in the reply format, shown by
//! parsing a real answer two ways; what the answers are *for*, which is contrast;
//! the palette; and finally being told about changes rather than asking, which is
//! the only step that alters terminal state and the reason the demo has a cleanup
//! path at all.
//!
//! Whatever happens — an error part way through, `q` on the first step — the run
//! ends by sending `CSI ? 2031 l`. A demo that leaves a terminal reporting scheme
//! changes to the next program is worse than no demo.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::{
    features::{
        Support,
        color::{
            notify::{self, Scheme},
            probe::{self, Outcome},
            query::{self, Answer, Answers, Target},
            value::{Rgb, parse_spec},
        },
        wrap,
    },
    input::{Advance, Event, InputReader, KeyCode, KeyEventKind},
    term::{Term, pretty},
    theme::{Ink, Theme},
};

/// Left margin for everything the demo draws.
const MARGIN: u16 = 3;

/// How long the scheme step waits for the user to switch their theme.
const WATCH_WINDOW: Duration = Duration::from_secs(20);

/// How long one read waits before the countdown is redrawn.
const TICK: Duration = Duration::from_millis(200);

/// The palette entries the palette step asks about: the sixteen ANSI colours.
const PALETTE_SHOWN: u8 = 16;

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

/// The script. Order matters: the trap only lands once a real reply has been
/// seen, and `restore` has to be last.
const STEPS: &[Step] = &[
    step!("round-trip", "One question, one answer", step_round_trip),
    step!("scaling", "The trap: rgb:f/0/0 is full red", step_scaling),
    step!("contrast", "What the answers are for", step_contrast),
    step!("palette", "The other 256", step_palette),
    step!("scheme", "Being told, instead of asking", step_scheme),
    step!("restore", "Putting the terminal back", step_leave),
];

/// The step names, in order, for [`crate::features::select_steps`].
fn step_names() -> Vec<&'static str> {
    STEPS.iter().map(|step| step.name).collect()
}

/// Prints the step list for `--list`.
pub fn list_steps(theme: &Theme) {
    println!();
    println!("  {}", theme.paint("Colour query demo", Ink::Title));
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
            "escapade color demo --only <NAME>   run just that step",
            Ink::Muted
        )
    );
    println!(
        "  {}",
        theme.paint(
            "escapade color demo --step N        start there and carry on",
            Ink::Muted
        )
    );
    println!();
    println!(
        "  {}",
        theme.paint(
            "Every step runs against this terminal and prints what it actually answered. Only \
             the `scheme` step changes anything, and it is undone however the demo ends.",
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
    // error rather than one printed after the alternate screen has come and gone.
    let selection = crate::features::select_steps(&step_names(), only, start_step)?;

    let outcome = probe::detect()?;

    // `--force` skips the capability check, but it cannot conjure a terminal.
    if !outcome.answers.interactive || (outcome.support() == Support::No && !force) {
        refuse(theme, &outcome);
        return Ok(());
    }

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let mut index = selection.start;

    term.enable_raw()?;
    term.enter_alt_screen()?;
    term.hide_cursor()?;

    let mut ctx = Ctx {
        forced: force && outcome.support() != Support::Yes,
        outcome,
        subscribed: false,
        term: &mut term,
        input: &mut input,
        theme,
        row: 0,
        width: 0,
    };

    let result = (|| -> Result<()> {
        loop {
            match (STEPS[index].run)(&mut ctx)? {
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
    let subscribed = ctx.subscribed;

    // Unconditional, and deliberately outside the `?` path above: quitting on the
    // scheme step, or failing during it, must not leave the terminal reporting
    // theme changes to the user's shell for the rest of the session.
    let _ = notify::stop(&mut term, subscribed);
    term.restore();
    result
}

/// The refusal shown when there is nothing to ask, or positive evidence of a
/// terminal that will not answer.
fn refuse(theme: &Theme, outcome: &Outcome) {
    println!();
    let message = "sorry - this does not seem to be supported by your terminal - check \
                   'escapade color explain' for support, or use \
                   'escapade color demo --force' to disable feature probing.";
    for line in wrap(message, theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Warn));
    }

    println!();
    for line in wrap(&outcome.reason(), theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }

    if !outcome.answers.interactive {
        println!();
        for line in wrap(
            "stdin and stdout are not both terminals. Unlike most of this project, that is fatal \
             here rather than merely limiting: a colour query is a round trip, so with a pipe on \
             either end there is nothing to ask and nowhere for the answer to arrive.",
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
    /// What the probe concluded, before any step ran.
    outcome: Outcome,
    /// Whether the demo is running past a probe that did not say yes.
    forced: bool,
    /// Whether mode 2031 is currently switched on because of this demo.
    subscribed: bool,
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
            .field("subscribed", &self.subscribed)
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
        let heading = "escapade · terminal colours";
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

    /// Asks the terminal about some colours, right now, inside the demo.
    ///
    /// Goes through [`query::ask_again`] rather than [`query::ask`]: the initial
    /// drain exists to clear junk before the first query of a *run*, and inside a
    /// demo it would swallow the keypress the reader made a moment ago.
    fn ask(&mut self, targets: &[Target]) -> Result<Answers> {
        query::ask_again(self.term, self.input, targets)
    }

    /// Draws one colour: a swatch, the normalised value, the raw reply.
    fn colour_row(&mut self, answer: &Answer) -> Result<()> {
        let label = format!("{:<22}", answer.target.label());
        let text = match &answer.reported {
            Some(reported) => format!(
                "{} {}  {}  {}",
                self.theme.paint(&label, Ink::Strong),
                swatch(reported.rgb),
                self.theme.paint(&reported.rgb.hex(), Ink::Code),
                self.theme.paint(&reported.raw, Ink::Muted)
            ),
            None => format!(
                "{} {}",
                self.theme.paint(&label, Ink::Strong),
                self.theme.paint("no answer", Ink::Muted)
            ),
        };
        self.indented(2, &text)
    }

    /// Explains why nothing may have come back.
    fn caveat(&mut self) -> Result<()> {
        if self.forced {
            let reason = self.outcome.reason();
            self.note(&format!(
                "The probe did not confirm this works here — {reason} — and you asked for --force \
                 anyway. The queries are still sent; a terminal that does not implement them \
                 simply says nothing."
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

/// A colour as a block of that colour, or a placeholder without colour.
///
/// Written with a direct SGR rather than through [`Theme`], because the whole
/// point is a colour the theme does not know about — it came from the terminal a
/// moment ago.
fn swatch(rgb: Rgb) -> String {
    let (r, g, b) = rgb.rgb8();
    format!("\u{1b}[38;2;{r};{g};{b}m███\u{1b}[0m")
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// One query, one reply, shown in full.
fn step_round_trip(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(0)?;
    ctx.para(
        "Nearly everything else in this project is written to a terminal and hoped for. This is a \
         conversation: the query goes out on stdout, the terminal answers on stdin, and what comes \
         back is this terminal's own statement about itself.",
    )?;
    ctx.blank();

    ctx.sequence(
        &Target::Background.query(),
        &[
            (
                "OSC",
                "Operating System Command, ESC ]. A terminal that does not recognise what follows \
                 swallows the whole string, which is why an unsupported query is silent rather \
                 than damaging.",
            ),
            (
                "11",
                "the default background colour. 10 is the foreground and 12 the cursor; 17 and 19 \
                 are the selection colours, under xterm's older name of \"highlight\".",
            ),
            (
                "?",
                "the whole trick. The same sequence with a colour here *sets* it; with a question \
                 mark it asks instead, and this is the only reason the feature exists.",
            ),
            (
                "ST",
                "String Terminator, ESC \\. BEL also ends an OSC and both are accepted, but a BEL \
                 in an outgoing query has been known to make terminals beep.",
            ),
        ],
    )?;
    ctx.blank();

    let answers = ctx.ask(&[Target::Background, Target::Foreground, Target::Cursor])?;
    ctx.indented(
        2,
        &ctx.theme
            .paint("what this terminal just said:", Ink::Accent)
            .clone(),
    )?;
    ctx.blank();
    for answer in &answers.answers.clone() {
        ctx.colour_row(answer)?;
    }
    ctx.blank();

    if answers.any_answered() {
        ctx.note(
            "Three queries, one round trip. They were sent back to back and followed by a single \
             `CSI c` barrier, so the whole exchange costs one wait rather than three — which \
             matters over ssh, and matters a great deal for the 256 palette entries two steps from \
             here.",
        )?;
    } else if answers.silence_is_conclusive() {
        ctx.note(
            "Nothing came back, and the `CSI c` barrier did — so this terminal reached the end of \
             the queue and chose not to answer. That is a real no, not a timeout.",
        )?;
    } else {
        ctx.note(
            "Neither a colour nor the barrier came back, so something between here and the \
             terminal is swallowing replies. Nothing has been established either way.",
        )?;
    }
    ctx.caveat()?;
    ctx.wait()
}

/// The digit-width trap, shown against a real reply.
fn step_scaling(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(1)?;
    ctx.para(
        "The reply is in XParseColor syntax, which `ctlseqs` defers to rather than defining. Each \
         channel is one to four hex digits — and the digits given are the *most significant* ones, \
         scaled across the full range rather than padded on the left.",
    )?;
    ctx.blank();

    let rows = [
        ("rgb:f/0/0", "one digit, scaled: f becomes ffff"),
        ("rgb:ffff/0/0", "four digits, the same colour written out"),
        ("#f00", "the older form, left-justified and zero-filled"),
    ];
    for (spec, meaning) in rows {
        let parsed = parse_spec(spec)?;
        let text = format!(
            "{} {}  {}  {}",
            ctx.theme.paint(&format!("{spec:<14}"), Ink::Code),
            swatch(parsed),
            ctx.theme.paint(&parsed.hex(), Ink::Strong),
            ctx.theme.paint(meaning, Ink::Muted)
        );
        ctx.indented(2, &text)?;
    }
    ctx.blank();

    ctx.para(
        "So `rgb:f/0/0` is full red. A naive `u16::from_str_radix` per field reads `f` as 15 out \
         of 65535 and turns it into near-black — which means a bright terminal is reported as a \
         dark one, `--is-dark` answers backwards, and nothing about the bug is visible until \
         somebody with a light theme complains.",
    )?;
    ctx.blank();
    ctx.note(
        "XParseColor(3) states the difference outright: in the `#` form, when fewer than 16 bits \
         are given they \"represent the most significant bits of the value (unlike the rgb: \
         syntax, in which values are scaled)\". Both forms are in circulation, so both are \
         implemented.",
    )?;
    ctx.wait()
}

/// What the answers are for: contrast against the real background.
fn step_contrast(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(2)?;
    ctx.para(
        "This is the point of the whole exercise. A program that picks its own colours is picking \
         them against a background it has never seen, and \"grey on grey\" is what that looks like \
         when the guess is wrong.",
    )?;
    ctx.blank();

    let answers = ctx.ask(&[Target::Foreground, Target::Background])?;
    let foreground = answers.get(Target::Foreground).map(|value| value.rgb);
    let background = answers.get(Target::Background).map(|value| value.rgb);

    for answer in &answers.answers.clone() {
        ctx.colour_row(answer)?;
    }
    ctx.blank();

    match (foreground, background) {
        (Some(fg), Some(bg)) => {
            let ratio = fg.contrast_ratio(bg);
            let verdict = if ratio >= 4.5 {
                ("passes WCAG AA for body text (4.5:1)", Ink::Good)
            } else {
                ("below WCAG AA for body text (4.5:1)", Ink::Warn)
            };
            let text = format!(
                "{} {}  {}",
                ctx.theme.paint(&format!("{:<22}", "contrast"), Ink::Strong),
                ctx.theme.paint(&format!("{ratio:.2}:1"), Ink::Code),
                ctx.theme.paint(verdict.0, verdict.1)
            );
            ctx.indented(2, &text)?;
            ctx.blank();

            let dark = bg.is_dark();
            ctx.note(&format!(
                "The background's relative luminance puts it on the {} side of CIE L* = 50, so a \
                 program reading this would style itself for a {} theme. That is what `escapade \
                 color --is-dark` reports through its exit status, and what the next step \
                 replaces with something better.",
                if dark { "dark" } else { "light" },
                if dark { "dark" } else { "light" }
            ))?;
        }
        _ => {
            ctx.note(
                "Both colours are needed for a contrast ratio and this terminal did not give up \
                 both, so nothing is computed here. A guessed contrast ratio would be worse than \
                 none.",
            )?;
        }
    }
    ctx.caveat()?;
    ctx.wait()
}

/// The palette: 256 entries, one round trip.
fn step_palette(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(3)?;
    ctx.para(
        "`OSC 4` asks about the indexed palette, and takes the index before the question mark. The \
         first sixteen are the ANSI colours every terminal theme redefines — which is exactly why \
         asking beats assuming that colour 1 is red.",
    )?;
    ctx.blank();

    ctx.sequence(
        &Target::Palette(1).query(),
        &[
            (
                "4",
                "the indexed palette, rather than a named dynamic colour.",
            ),
            ("1", "the entry: 0 to 255. Here, ANSI red."),
            (
                "?",
                "the same ask-rather-than-set trick. The reply repeats the index, which is how \
                 several answers in one batch are told apart.",
            ),
        ],
    )?;
    ctx.blank();

    let targets: Vec<Target> = (0..PALETTE_SHOWN).map(Target::Palette).collect();
    let answers = ctx.ask(&targets)?;

    let answered = answers.answers.clone();
    let mut cells: Vec<String> = Vec::new();
    for answer in &answered {
        let index = answer.target.index().unwrap_or(0);
        cells.push(match &answer.reported {
            Some(reported) => format!(
                "{} {} {}",
                ctx.theme.paint(&format!("{index:>3}"), Ink::Muted),
                swatch(reported.rgb),
                ctx.theme.paint(&reported.rgb.hex(), Ink::Code)
            ),
            None => format!(
                "{} {}",
                ctx.theme.paint(&format!("{index:>3}"), Ink::Muted),
                ctx.theme.paint("   no answer", Ink::Muted)
            ),
        });
    }
    for row in cells.chunks(4) {
        ctx.indented(2, &row.join("   "))?;
    }
    ctx.blank();

    if answers.any_answered() {
        ctx.note(&format!(
            "{PALETTE_SHOWN} queries and one barrier, in a single round trip. Asking one at a \
             time would be {PALETTE_SHOWN} waits — which is why the queries all go out before \
             anything is read."
        ))?;
    } else {
        ctx.note(
            "This terminal answered nothing about the palette. Some terminals implement the \
             dynamic colours and not `OSC 4`; that is a gap worth reporting rather than a failure \
             of the query.",
        )?;
    }
    ctx.caveat()?;
    ctx.wait()
}

/// Mode 2031: being told rather than asking.
#[expect(
    clippy::too_many_lines,
    reason = "the only step with a live event loop in it: the prose, the \
              subscription, the countdown and the cleanup are one sequence, and \
              splitting them would hide that the mode is set and unset in one place"
)]
fn step_scheme(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(4)?;
    ctx.para(
        "Everything so far is a snapshot, and a snapshot is correct only until the user changes \
         their theme. Mode 2031 is a subscription: set it, and the terminal reports every scheme \
         change on its own.",
    )?;
    ctx.blank();

    ctx.sequence(
        &notify::enable(),
        &[
            ("CSI ?", "a DEC private mode, rather than an ANSI one."),
            ("2031", "colour-scheme change notification."),
            (
                "h",
                "set. `l` resets it, and this demo sends that on every path out — a mode outlives \
                 the process that set it.",
            ),
        ],
    )?;
    ctx.blank();

    let support = ctx.outcome.notify.support();
    let (symbol, ink) = match support {
        Support::Yes => ("✔", Ink::Good),
        Support::No => ("✘", Ink::Bad),
        Support::Unknown => ("?", Ink::Warn),
    };
    let verdict = format!(
        "{} {}",
        ctx.theme.paint(symbol, ink),
        ctx.theme.paint(&ctx.outcome.notify_reason(), Ink::Muted)
    );
    ctx.indented(2, &verdict)?;
    ctx.blank();

    if support == Support::No {
        ctx.note(
            "Nothing to subscribe to here. `escapade color --watch` falls back to re-asking on a \
             timer on such a terminal, and says so rather than pretending the result is instant.",
        )?;
        return ctx.wait();
    }

    // From here the mode is on, and the cleanup path in `run` owns taking it down.
    ctx.term.write_now(&notify::enable())?;
    ctx.subscribed = true;

    ctx.indented(
        2,
        &ctx.theme
            .paint("subscribed — switch your terminal's theme now", Ink::Accent)
            .clone(),
    )?;
    ctx.blank();

    let listening_row = ctx.row;
    let started = Instant::now();
    let mut reports: Vec<Scheme> = Vec::new();

    let flow = loop {
        let left = WATCH_WINDOW.saturating_sub(started.elapsed());
        if left.is_zero() {
            break None;
        }
        let countdown = format!(
            "listening for {}s more — or press space to move on",
            left.as_secs()
        );
        ctx.term.put(
            MARGIN + 2,
            listening_row,
            &ctx.theme.paint(&countdown, Ink::Muted),
        )?;
        ctx.term.clear_to_eol()?;
        ctx.term.flush()?;

        match ctx.input.read(TICK) {
            Ok(Some(Event::Reply(reply))) => {
                if let Some(scheme) = notify::parse_scheme_report(&reply) {
                    reports.push(scheme);
                    let row = listening_row + 2 + u16::try_from(reports.len()).unwrap_or(0);
                    // The sequence is reconstructed from the decoded scheme
                    // rather than echoed back raw: the reply arrives parsed, and
                    // printing the bytes a terminal sent without re-encoding
                    // them is how a stray control byte reaches the screen.
                    let text = format!(
                        "{}  {}  {}",
                        ctx.theme.paint("→ unprompted", Ink::Good),
                        ctx.theme.paint(
                            &format!("CSI ? {} ; {} n", notify::REPORT_PS, scheme.value()),
                            Ink::Code
                        ),
                        ctx.theme.paint(scheme.as_str(), Ink::Strong)
                    );
                    ctx.term.put(MARGIN + 2, row, &text)?;
                }
            }
            Ok(Some(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                break Some(Flow::from(match key.code {
                    KeyCode::Char('q' | 'Q') | KeyCode::Esc => Advance::Quit,
                    KeyCode::Char('p' | 'P') => Advance::Prev,
                    _ => Advance::Next,
                }));
            }
            Ok(_) => {}
            Err(_) => break Some(Flow::Quit),
        }
    };

    ctx.row = listening_row + 3 + u16::try_from(reports.len()).unwrap_or(0);
    ctx.blank();
    if reports.is_empty() {
        ctx.note(
            "No report arrived. Either the theme did not change, or this terminal accepts the \
             mode and never acts on it — which is why `escapade color probe` reports the mode and \
             the queries as two separate verdicts rather than one.",
        )?;
    } else {
        ctx.note(&format!(
            "{} report(s) arrived unprompted, with no query sent for any of them. That is the \
             difference between this and polling: an idle subscription costs nothing, and a \
             change is known the instant it happens rather than up to a poll interval later.",
            reports.len()
        ))?;
    }

    match flow {
        Some(flow) => Ok(flow),
        None => ctx.wait(),
    }
}

/// Putting the terminal back, and saying exactly what was left behind.
fn step_leave(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(5)?;
    ctx.para(
        "A colour query changes nothing, which is the nicest property this feature has: every \
         step before the last one was read-only, and a terminal that ignored the queries is in \
         exactly the state it started in.",
    )?;
    ctx.blank();

    ctx.para("The one exception is the subscription, and it comes down here:")?;
    ctx.blank();
    ctx.indented(
        2,
        &ctx.theme
            .paint(&pretty(&notify::disable()), Ink::Code)
            .clone(),
    )?;
    ctx.blank();

    if ctx.subscribed {
        ctx.note(
            "Mode 2031 was set by the scheme step and is switched off on the way out — including \
             if you had quit during it, or if it had failed part way through. Leaving it on would \
             have the terminal delivering scheme reports to your shell, which would print them as \
             stray text.",
        )?;
    } else {
        ctx.note(
            "Nothing was subscribed in this run, so this is a no-op. It is sent from the cleanup \
             path regardless, because the cleanup path cannot know how the run went.",
        )?;
    }
    ctx.blank();

    ctx.para("Where to go next:")?;
    ctx.blank();
    for (command, what) in [
        (
            "escapade color",
            "the one-shot report you just walked through",
        ),
        (
            "escapade color --is-dark",
            "the same question as an exit status",
        ),
        ("escapade color --watch", "one line per change, forever"),
        ("escapade color probe", "the two verdicts, separately"),
        (
            "escapade color explain",
            "the protocol, and who implements it",
        ),
    ] {
        let text = format!(
            "{}  {}",
            ctx.theme.paint(&format!("{command:<28}"), Ink::Code),
            ctx.theme.paint(what, Ink::Muted)
        );
        ctx.indented(2, &text)?;
    }
    ctx.wait()
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn step_names_are_unique_and_script_safe() {
        let names = step_names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        let total = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), total, "two steps share a name");
        for name in names {
            assert!(
                name.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'),
                "{name} is not kebab-case"
            );
        }
    }

    #[test]
    fn every_step_is_reachable_by_name_and_by_number() {
        let names = step_names();
        for (index, name) in names.iter().enumerate() {
            let selection = crate::features::select_steps(&names, Some(name), None).unwrap();
            assert_eq!(selection.start, index);
            assert!(selection.single);
        }
        let last = crate::features::select_steps(&names, None, Some(names.len())).unwrap();
        assert_eq!(last.start, names.len() - 1);
    }

    #[test]
    fn the_last_step_is_the_one_that_puts_the_terminal_back() {
        // The cleanup is unconditional in `run`, but the script should still end
        // by saying so — a reader who quits early has not seen it.
        assert_eq!(STEPS.last().unwrap().name, "restore");
    }

    #[test]
    fn the_subscription_step_comes_before_the_restore_step() {
        let names = step_names();
        let scheme = names.iter().position(|name| *name == "scheme").unwrap();
        let restore = names.iter().position(|name| *name == "restore").unwrap();
        assert!(scheme < restore);
    }

    #[test]
    fn a_swatch_is_the_colour_and_nothing_else() {
        let painted = swatch(parse_spec("rgb:f/0/0").unwrap());
        assert!(painted.contains("38;2;255;0;0"), "{painted:?}");
        // It must reset, or the rest of the line inherits the colour.
        assert!(painted.ends_with("\u{1b}[0m"), "{painted:?}");
    }

    #[test]
    fn the_palette_step_asks_for_the_ansi_sixteen() {
        assert_eq!(PALETTE_SHOWN, 16);
    }
}
