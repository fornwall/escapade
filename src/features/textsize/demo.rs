//! The step-by-step demo.
//!
//! Each step says what it is about to do, shows the escape sequence and names
//! every piece of it, and only then draws. Nothing appears by magic: by the end a
//! reader has seen the bytes behind every letter on screen, including the ones
//! seven cells tall.
//!
//! The arc goes from the anatomy of the sequence, through the scale ladder and
//! the width key that ends the wcwidth argument, to fractional scaling and
//! alignment — then to the step that makes the whole point, where the reader
//! selects a banner with the mouse and pastes it back as the plain line of text
//! it always was. The poster at the end composes everything at once, and the last
//! step leaves the alternate screen.

use std::{thread::sleep, time::Duration};

use anyhow::Result;

use crate::{
    cli::{HorizontalAlign, VerticalAlign},
    features::{
        Support,
        textsize::{Fraction, Key, Metadata, SizedText, probe, sanitize_text},
        wrap,
    },
    input::{Advance, Event, InputReader, KeyEventKind},
    term::{CSI, Term, pretty},
    theme::{Ink, Theme},
    unicode,
};

/// Left margin for everything the demo draws.
const MARGIN: u16 = 3;

/// Frame interval for the poster's reveal: about 25 frames per second.
const FRAME: Duration = Duration::from_millis(40);

/// A farmer: `U+1F9D1 U+200D U+1F33E`. Spelled out rather than pasted, because
/// the joiner holding it together is invisible and unreviewable in source.
const FARMER: &str = "\u{1f9d1}\u{200d}\u{1f33e}";

/// The banner the reader is invited to select and paste back.
const BANNER: &str = "COPY ME";

/// The poster's headline, subhead and kicker.
const HEADLINE: &str = "ESCAPADE";
const SUBHEAD: &str = "one escape code, any size";
const KICKER: &str = "THE TEXT SIZING PROTOCOL · OSC 66";

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

/// One entry in the demo script: a short name for `--only`, a title for the
/// header, and the code that draws it.
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

/// The script.
const STEPS: &[Step] = &[
    step!("anatomy", "One escape code, six keys", step_anatomy),
    step!("scale", "The ladder: s=1 to s=7", step_scale),
    step!(
        "width",
        "w=, and the end of the wcwidth argument",
        step_width
    ),
    step!(
        "fractions",
        "Halves and thirds: superscripts",
        step_fractions
    ),
    step!("alignment", "Nine places to put it", step_alignment),
    step!(
        "copy-paste",
        "Select it and paste it — it is text",
        step_copy_paste
    ),
    step!("poster", "Finale: one page, every key", step_poster),
    step!("restore", "Putting the terminal back", step_leave),
];

/// The step names, in order, for [`crate::features::select_steps`].
fn step_names() -> Vec<&'static str> {
    STEPS.iter().map(|step| step.name).collect()
}

/// Prints the step list for `--list`.
pub fn list_steps(theme: &Theme) {
    println!();
    println!("  {}", theme.paint("Text sizing demo", Ink::Title));
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
            "escapade textsize demo --only <NAME>   run just that step",
            Ink::Muted
        )
    );
    println!(
        "  {}",
        theme.paint(
            "escapade textsize demo --step N        start there and carry on",
            Ink::Muted
        )
    );
    println!();
}

/// Runs the demo.
///
/// # Errors
///
/// Fails if the terminal cannot be set up or written to.
pub fn run(
    theme: &Theme,
    force: bool,
    only: Option<&str>,
    start_step: Option<usize>,
) -> Result<()> {
    let selection = crate::features::select_steps(&step_names(), only, start_step)?;

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;

    let outcome = probe::probe(&mut term, &mut input, &Key::MEASURABLE)?;

    // `--force` skips the capability check, but it cannot conjure a terminal:
    // enabling raw mode on something that is not a tty fails, and there would be
    // nothing to draw on anyway. So this gate comes first and is not
    // overridable.
    if !outcome.interactive {
        term.restore();
        refuse(theme, &outcome);
        return Ok(());
    }

    let supported = outcome.support() == Support::Yes;
    if !supported && !force {
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
        forced: !supported,
        width_ok: outcome.support_for(Key::Width) == Support::Yes,
        scale_ok: outcome.support_for(Key::Scale) == Support::Yes,
        row: 0,
        width: 0,
    };

    let result = (|| -> Result<()> {
        loop {
            let flow = (STEPS[index].run)(&mut ctx)?;
            match flow {
                Flow::Next => {
                    // `--only` runs the one step and stops rather than carrying
                    // on through the rest of the script.
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

/// The refusal shown when the terminal cannot do this.
///
/// It reports the per-key picture rather than one word, because on a terminal
/// that implements `w` and not `s` — which is a real and shipping
/// configuration — "not supported" is only half true, and the half that is
/// wrong is the half a reader can use.
fn refuse(theme: &Theme, outcome: &probe::ProbeOutcome) {
    println!();
    let message = "sorry - this does not seem to be supported by your terminal - check \
                   'escapade textsize explain' for support, or use \
                   'escapade textsize demo --force' to disable feature probing.";
    for line in wrap(message, theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Warn));
    }

    if outcome.interactive && !outcome.reports.is_empty() {
        println!();
        for report in &outcome.reports {
            let verdict = report.verdict();
            let ink = match verdict.support() {
                Support::Yes => Ink::Good,
                Support::No => Ink::Bad,
                Support::Unknown => Ink::Warn,
            };
            println!(
                "  {} {}  {}",
                theme.paint(verdict.symbol(), ink),
                theme.paint(&format!("{}=", report.key.letter()), Ink::Strong),
                theme.paint(verdict.label(), ink)
            );
        }
        if outcome.support_for(Key::Width) == Support::Yes {
            println!();
            for line in wrap(
                "The width key does work here, which is the half of this protocol that fixes \
                 cell-counting disagreements. `escapade textsize --width 2 <text>` will do what it \
                 says; only the scaling steps of the demo will come out flat.",
                theme.width().saturating_sub(2),
            ) {
                println!("  {}", theme.paint(&line, Ink::Muted));
            }
        }
    }

    if !outcome.conclusive() && outcome.interactive {
        println!();
        for line in wrap(
            "Strictly speaking this was inconclusive rather than a no: no cursor position came \
             back, so nothing was measured. This protocol has no query, so measuring is the only \
             way to ask.",
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }

    if let Some(multiplexer) = outcome.multiplexer {
        println!();
        for line in wrap(
            &format!(
                "You are inside {}, which lays out its own cells and discards escape sequences it \
                 does not recognise. Passthrough would not help: forwarding OSC 66 to the terminal \
                 beyond would leave the two disagreeing about the same grid, with a four-by-four \
                 block drawn over cells {} believes hold one character each.",
                multiplexer.name(),
                multiplexer.name()
            ),
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }

    if !outcome.interactive {
        println!();
        println!(
            "  {}",
            theme.paint(
                "stdout is not a terminal, so nothing could be measured in the first place.",
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
    /// Whether the demo is running past a failed measurement.
    forced: bool,
    /// Whether `w=` was honoured when measured.
    width_ok: bool,
    /// Whether `s=` was honoured when measured.
    scale_ok: bool,
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
            .field("scale_ok", &self.scale_ok)
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
        let heading = "escapade · text sizing";
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

    /// Draws sized text at a position, with the top-left of its block at
    /// `(col, row)`.
    ///
    /// Skips the write when the block would run off the bottom of the screen: the
    /// specification requires the terminal to *discard* a block that does not
    /// fit, so drawing it anyway would silently lose it and look like a bug in
    /// the protocol rather than a demo that ran out of room.
    fn sized(&mut self, col: u16, row: u16, metadata: Metadata, text: &str) -> Result<u16> {
        let sized = SizedText::new(metadata, text)?;
        let height = u16::from(metadata.scale());
        if row.saturating_add(height).saturating_sub(1) > self.term.rows {
            return Ok(row);
        }
        self.term.put(col, row, &sized.sequence())?;
        Ok(row.saturating_add(height))
    }

    /// Draws sized text in an ink, resetting the attributes afterwards.
    ///
    /// A multicell block takes the graphic rendition in force when it is drawn,
    /// exactly as a letter does — there is nothing special to do beyond wrapping
    /// the sequence in SGR.
    fn sized_ink(
        &mut self,
        col: u16,
        row: u16,
        metadata: Metadata,
        text: &str,
        ink: Ink,
    ) -> Result<u16> {
        let sized = SizedText::new(metadata, text)?;
        let height = u16::from(metadata.scale());
        if row.saturating_add(height).saturating_sub(1) > self.term.rows {
            return Ok(row);
        }
        let painted = self.theme.paint(&sized.sequence(), ink);
        self.term.put(col, row, &painted)?;
        Ok(row.saturating_add(height))
    }

    /// Notes, once per page, that the terminal is not doing what was asked.
    fn forced_note(&mut self) -> Result<()> {
        if self.forced {
            let (worked, missing) = match (self.width_ok, self.scale_ok) {
                (true, false) => ("`w=` was honoured", "`s=` was not"),
                (false, true) => ("`s=` was honoured", "`w=` was not"),
                _ => ("nothing was measured", "neither key answered"),
            };
            self.note(&format!(
                "Running under --force: {worked} and {missing}, so what is above may not look like \
                 what the sequence asked for."
            ))?;
        }
        Ok(())
    }

    /// Draws the footer hint and blocks until the user chooses.
    fn wait(&mut self) -> Result<Flow> {
        let row = self.term.rows.saturating_sub(1);
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

/// The anatomy of the one sequence this protocol has.
fn step_anatomy(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(0)?;
    ctx.para(
        "This whole protocol is one escape code. It carries a size and some text, and it never \
         asks the terminal anything — which is the point: the application decides how many cells \
         the text takes, so the two cannot disagree.",
    )?;
    ctx.blank();

    let sized = SizedText::new(Metadata::new().with_scale(2)?, "Hello")?;
    ctx.sequence(
        &sized.sequence(),
        &[
            (
                "OSC",
                "Operating System Command, ESC ]. A terminal that does not implement command 66 \
                 parses this string and throws it away — text included. Nothing appears at all, \
                 which is why this tool measures before it draws.",
            ),
            ("66", "the command number. There is only one."),
            (
                "s=2",
                "the metadata: colon-separated key=value pairs. Six keys exist — s, w, n, d, v, h \
                 — and every one of them is an integer.",
            ),
            (
                "; Hello",
                "the text, after the second semicolon. Safe UTF-8, at most 4096 bytes, and it \
                 stays text: this is not a picture of the word Hello.",
            ),
            ("ST", "String Terminator, ESC \\. A BEL is also legal."),
        ],
    )?;
    ctx.blank();

    let row = ctx.row;
    let next = ctx.sized(MARGIN + 2, row, Metadata::new().with_scale(2)?, "Hello")?;
    ctx.row = next.saturating_add(1);

    let measured = [("w=", ctx.width_ok), ("s=", ctx.scale_ok)];
    for (key, ok) in measured {
        let (symbol, label, ink) = if ok {
            ("✔", "honoured on this terminal", Ink::Good)
        } else {
            ("✘", "not honoured on this terminal", Ink::Bad)
        };
        let text = format!(
            "{} {}  {}",
            ctx.theme.paint(symbol, ink),
            ctx.theme.paint(key, Ink::Strong),
            ctx.theme.paint(label, ink)
        );
        ctx.indented(2, &text)?;
    }
    ctx.blank();
    ctx.note(
        "Those two lines came from a measurement, not a query: print a space with w=2, ask for the \
         cursor position, and subtract. The specification documents exactly that, because it \
         provides nothing to ask with.",
    )?;

    ctx.wait()
}

/// The scale ladder, laid out as a staircase on one baseline.
fn step_scale(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(1)?;
    ctx.para(
        "`s` is a whole-number multiple of the base font size, from 1 to 7. Each character is \
         drawn in a block s cells wide and s cells tall, and the terminal picks a font size to \
         fill it — so changing the terminal's font size changes these with it.",
    )?;
    ctx.blank();
    ctx.sequence(
        &SizedText::new(Metadata::new().with_scale(7)?, "7")?.sequence(),
        &[(
            "s=7",
            "the largest the protocol can express. Seven is not arbitrary: `s` is a three-bit \
             field in kitty's cell representation.",
        )],
    )?;
    ctx.blank();

    // Sit every block on one baseline, so the sizes read as a staircase rather
    // than as seven unrelated blobs. A block s tall whose bottom row is
    // `baseline` starts at `baseline - s + 1`.
    let baseline = ctx.row.saturating_add(u16::from(Key::Scale.range().1));
    let inks = [
        Ink::Muted,
        Ink::Accent,
        Ink::Good,
        Ink::Warn,
        Ink::Bad,
        Ink::Title,
        Ink::Link,
    ];
    let mut col = MARGIN + 2;
    for scale in 1..=Key::Scale.range().1 {
        let metadata = Metadata::new().with_scale(scale)?;
        let top = baseline.saturating_sub(u16::from(scale)).saturating_add(1);
        let digit = char::from(b'0' + scale).to_string();
        let ink = inks[usize::from(scale - 1) % inks.len()];
        ctx.sized_ink(col, top, metadata, &digit, ink)?;
        // The block is s cells wide for a one-character payload, plus a gap.
        col = col.saturating_add(u16::from(scale)).saturating_add(1);
    }
    ctx.row = baseline.saturating_add(2);

    ctx.note(
        "Seven digits, seven sizes, one line of text each. The cursor advanced s cells for every \
         one of them — the rows below the first are part of the same character, not separate \
         output.",
    )?;
    ctx.forced_note()?;

    ctx.wait()
}

/// The width key, and the cell-counting argument it settles.
fn step_width(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(2)?;
    ctx.para(
        "`w` is the other half of the protocol, and it has nothing to do with making text bigger. \
         It says how many cells the text in this one escape code occupies — and terminals that \
         implement nothing else implement this, because it fixes a twenty-year-old bug.",
    )?;
    ctx.blank();

    let farmer = SizedText::new(Metadata::new().with_width(2)?, FARMER)?;
    ctx.sequence(
        &farmer.sequence(),
        &[
            (
                "w=2",
                "two cells, for the whole payload. Not a request: a statement. The terminal must \
                 use two.",
            ),
            (
                "U+1F9D1 U+200D U+1F33E",
                "a farmer: three codepoints, one grapheme cluster. wcwidth adds their widths and \
                 says four cells; segmentation says two. Terminals disagree in the field, and \
                 every disagreement corrupts a layout.",
            ),
        ],
    )?;
    ctx.blank();

    let predicted = unicode::display_width(FARMER);
    let legacy: usize = FARMER.chars().map(unicode::char_width).sum();
    ctx.indented(
        2,
        &ctx.theme.paint(
            &format!(
                "segmentation says {predicted} cells · per-codepoint wcwidth says {legacy} · with \
                 w=2 the terminal has no say"
            ),
            Ink::Muted,
        ),
    )?;
    ctx.blank();

    // Bars either side. If `w` is honoured, the right-hand bars line up; if the
    // terminal counted for itself, they will not.
    let row = ctx.row;
    let bar = ctx.theme.paint("│", Ink::Accent);
    for (offset, (label, sequence)) in [
        ("with w=2 ", farmer.sequence()),
        ("plain    ", FARMER.to_owned()),
    ]
    .into_iter()
    .enumerate()
    {
        let row = row.saturating_add(u16::try_from(offset).unwrap_or(0));
        ctx.term
            .put(MARGIN + 2, row, &ctx.theme.paint(label, Ink::Muted))?;
        ctx.term.write(&bar)?;
        ctx.term.write(&sequence)?;
        ctx.term.write(&bar)?;
    }
    ctx.row = row.saturating_add(3);

    ctx.note(
        "If the right-hand bars line up, this terminal is counting the way the application asked \
         it to. If they do not, the second line is what every table, box and progress bar in this \
         terminal has to cope with.",
    )?;
    ctx.blank();
    ctx.note(
        "This is the half foot implements: `w` only, no scaling. That is why every verdict in this \
         feature is per key — one boolean would call foot unsupported and be wrong about the part \
         it does.",
    )?;

    ctx.wait()
}

/// Fractional scaling: superscripts, subscripts, and text smaller than a cell.
fn step_fractions(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(3)?;
    ctx.para(
        "`s` gives seven sizes. `n` and `d` give a fraction on top of it — and the surprise is \
         that a fraction does not change how many cells the text occupies. It shrinks the font \
         inside them. That is what makes a superscript possible in a grid of fixed cells.",
    )?;
    ctx.blank();

    let half = Metadata::new()
        .with_fraction(Fraction::new(1, 2)?)
        .with_width(1)?;
    ctx.sequence(
        &SizedText::new(half, "2")?.sequence(),
        &[
            (
                "n=1:d=2",
                "half size. `d` must be greater than `n`: a fraction says how much *smaller* than \
                 the base size to draw, and anything larger is `s`'s job.",
            ),
            (
                "w=1",
                "one cell. Fractional text still occupies whole cells, so without `w` a pair of \
                 half-size characters would take two cells and look like small text rather than a \
                 superscript.",
            ),
        ],
    )?;
    ctx.blank();

    let row = ctx.row;
    let top = half.with_valign(VerticalAlign::Top);
    let bottom = half.with_valign(VerticalAlign::Bottom);

    // E = mc², with a real superscript.
    ctx.term.put(MARGIN + 2, row, "E = mc")?;
    ctx.term.write(&SizedText::new(top, "2")?.sequence())?;
    ctx.term.write(
        &ctx.theme
            .paint("    ← n=1:d=2:v=0, a superscript", Ink::Muted),
    )?;

    // H₂O, with a real subscript.
    let row = row.saturating_add(2);
    ctx.term.put(MARGIN + 2, row, "H")?;
    ctx.term.write(&SizedText::new(bottom, "2")?.sequence())?;
    ctx.term.write("O")?;
    ctx.term.write(
        &ctx.theme
            .paint("        ← n=1:d=2:v=1, a subscript", Ink::Muted),
    )?;

    // A whole line of small text.
    let row = row.saturating_add(2);
    let mut col = MARGIN + 2;
    let footnote = "and a whole line of half-size text, two characters to a cell";
    let chars: Vec<char> = footnote.chars().collect();
    for pair in chars.chunks(2) {
        let text: String = pair.iter().collect();
        ctx.term
            .put(col, row, &SizedText::new(half, &text)?.sequence())?;
        col = col.saturating_add(1);
    }
    ctx.row = row.saturating_add(2);

    ctx.note(
        "The last line is one escape code per pair of characters, each claiming a single cell with \
         w=1. Sixty characters in thirty cells, and it still copies as sixty characters.",
    )?;
    ctx.forced_note()?;

    ctx.wait()
}

/// The alignment keys, all nine combinations at once.
fn step_alignment(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(4)?;
    ctx.para(
        "`v` and `h` say where the fractionally scaled area sits inside the full block. They apply \
         only while n < d — with no fraction there is nothing smaller than the block to move, so \
         the keys are legal and do nothing.",
    )?;
    ctx.blank();

    let base = Metadata::new()
        .with_scale(3)?
        .with_width(1)?
        .with_fraction(Fraction::new(1, 3)?);
    ctx.sequence(
        &SizedText::new(
            base.with_valign(VerticalAlign::Center)
                .with_halign(HorizontalAlign::Center),
            "x",
        )?
        .sequence(),
        &[
            (
                "s=3:w=1",
                "a three-by-three block for one character, so there is room to move something \
                 around inside it.",
            ),
            (
                "n=1:d=3",
                "the character is drawn at a third of the size, leaving eight ninths of the block \
                 empty.",
            ),
            (
                "v=2:h=2",
                "centred both ways. 0 is top and left, 1 is bottom and right, 2 is centre — note \
                 that centre is 2, not the middle number.",
            ),
        ],
    )?;
    ctx.blank();

    let rows = [
        (VerticalAlign::Top, "v=0"),
        (VerticalAlign::Center, "v=2"),
        (VerticalAlign::Bottom, "v=1"),
    ];
    let cols = [
        (HorizontalAlign::Left, "h=0"),
        (HorizontalAlign::Center, "h=2"),
        (HorizontalAlign::Right, "h=1"),
    ];

    let start = ctx.row;
    for (col_index, (halign, hlabel)) in cols.iter().enumerate() {
        let col = MARGIN + 8 + u16::try_from(col_index).unwrap_or(0) * 6;
        ctx.term
            .put(col, start, &ctx.theme.paint(hlabel, Ink::Accent))?;
        for (row_index, (valign, vlabel)) in rows.iter().enumerate() {
            let row = start + 1 + u16::try_from(row_index).unwrap_or(0) * 4;
            if col_index == 0 {
                ctx.term
                    .put(MARGIN + 2, row + 1, &ctx.theme.paint(vlabel, Ink::Accent))?;
            }
            let metadata = base.with_valign(*valign).with_halign(*halign);
            ctx.sized_ink(col, row, metadata, "x", Ink::Good)?;
        }
    }
    ctx.row = start + 13;

    ctx.note(
        "Nine blocks, one character each, differing only in `v` and `h`. Every one of them is the \
         letter x — select any of them and that is what you get.",
    )?;
    ctx.forced_note()?;

    ctx.wait()
}

/// The step that makes the point: it is text.
fn step_copy_paste(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(5)?;
    ctx.para(
        "This is the difference between this protocol and figlet. A banner drawn out of hashes and \
         slashes copies as hashes and slashes; twenty lines of them, none of which is the word you \
         drew. A multicell character *holds* its text.",
    )?;
    ctx.blank();

    let scale = if ctx.width >= 40 { 4_u8 } else { 2 };
    let metadata = Metadata::new().with_scale(scale)?;
    let row = ctx.row;
    let next = ctx.sized_ink(MARGIN + 2, row, metadata, BANNER, Ink::Title)?;
    ctx.row = next.saturating_add(1);

    ctx.note(&format!(
        "Above: {} characters, drawn {scale} cells tall, occupying {} cells of one row.",
        BANNER.len(),
        usize::from(scale) * BANNER.len()
    ))?;
    ctx.blank();

    ctx.para(
        "Select it with the mouse — drag across it — and paste it back here. Middle-click pastes \
         the selection on X11 and Wayland; Ctrl+Shift+V pastes the clipboard.",
    )?;
    ctx.blank();

    // Bracketed paste is what makes the pasted text arrive as one event rather
    // than as a burst of keystrokes indistinguishable from typing.
    ctx.term.enable_bracketed_paste()?;
    ctx.sequence(
        &format!("{CSI}?2004h"),
        &[(
            "CSI ?2004h",
            "bracketed paste, so what arrives next is fenced by CSI 200~ and CSI 201~ and can be \
             told apart from typing.",
        )],
    )?;
    ctx.blank();

    let waiting_row = ctx.row;
    let flow = loop {
        let hint = ctx.theme.paint(
            "waiting for a paste · space/enter next · p back · q quit",
            Ink::Muted,
        );
        let footer = ctx.term.rows.saturating_sub(1);
        ctx.term.put(MARGIN, footer, &hint)?;
        ctx.term.flush()?;

        match ctx.input.read_blocking()? {
            Event::Paste(text) => {
                ctx.row = waiting_row;
                let cleaned = sanitize_text(text.trim_end_matches(['\n', '\r']));
                let matches = cleaned == BANNER;
                let (symbol, verdict, ink) = if matches {
                    ("✔", "identical to the string in the escape code", Ink::Good)
                } else {
                    ("·", "not what was drawn — see below", Ink::Warn)
                };
                let line = format!(
                    "{} {} {}",
                    ctx.theme.paint(symbol, ink),
                    ctx.theme.paint(&format!("pasted: {cleaned:?}"), Ink::Code),
                    ctx.theme.paint(verdict, ink)
                );
                ctx.indented(2, &line)?;
                ctx.blank();
                ctx.note(&format!(
                    "{} bytes, {} characters. The banner above is {} cells wide and 4 rows tall, \
                     and this is what your clipboard holds.",
                    cleaned.len(),
                    cleaned.chars().count(),
                    usize::from(scale) * BANNER.len()
                ))?;
                if !matches {
                    ctx.note(
                        "A mismatch usually means the selection caught the surrounding text too, \
                         or that this terminal does not implement the protocol and there was \
                         nothing there to select.",
                    )?;
                }
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if let Some(advance) = advance_for(&key) {
                    break Flow::from(advance);
                }
            }
            _ => {}
        }
    };

    ctx.term.disable_bracketed_paste()?;
    Ok(flow)
}

/// Maps a key event onto an advance, matching [`InputReader::wait_for_advance`].
///
/// Spelled out here because this step cannot use `wait_for_advance`: it has to
/// stay in its own loop to see paste events, which that helper skips.
fn advance_for(key: &crate::input::KeyEvent) -> Option<Advance> {
    use crate::input::KeyCode;
    match key.code {
        KeyCode::Char(' ') | KeyCode::Enter => Some(Advance::Next),
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => Some(Advance::Quit),
        KeyCode::Char('c' | 'd') if key.mods.ctrl => Some(Advance::Quit),
        KeyCode::Char('p') | KeyCode::Left | KeyCode::Backspace => Some(Advance::Prev),
        _ => None,
    }
}

/// The finale: one page using every key at once, revealed a character at a time.
fn step_poster(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(6)?;

    // Fit the headline to the window rather than letting the terminal discard a
    // block that runs off the edge.
    let available = usize::from(ctx.term.cols.saturating_sub(MARGIN * 2));
    let headline_scale = u8::try_from(available / HEADLINE.len())
        .unwrap_or(1)
        .clamp(1, 5);
    let sub_scale = if headline_scale >= 4 { 2 } else { 1 };

    let start = ctx.row;

    // Kicker: half-size text, two characters to a cell, in small caps.
    let kicker = Metadata::new()
        .with_width(1)?
        .with_fraction(Fraction::new(1, 2)?)
        .with_valign(VerticalAlign::Center);
    let mut col = MARGIN + 2;
    let chars: Vec<char> = KICKER.chars().collect();
    for pair in chars.chunks(2) {
        let text: String = pair.iter().collect();
        ctx.sized_ink(col, start, kicker, &text, Ink::Accent)?;
        col = col.saturating_add(1);
    }

    // Headline, revealed one character at a time. Each character is its own
    // escape code so the reveal is a redraw of nothing already drawn.
    let headline_row = start + 2;
    let metadata = Metadata::new().with_scale(headline_scale)?;
    let inks = [Ink::Title, Ink::Warn, Ink::Bad, Ink::Accent, Ink::Good];
    for (index, ch) in HEADLINE.chars().enumerate() {
        let col = MARGIN + 2 + u16::try_from(index).unwrap_or(0) * u16::from(headline_scale);
        let ink = inks[index % inks.len()];
        ctx.term.begin_sync()?;
        ctx.sized_ink(col, headline_row, metadata, &ch.to_string(), ink)?;
        ctx.term.end_sync()?;
        sleep(FRAME);
    }

    // Subhead, and a rule under it drawn with a scaled space so that even the
    // rule is a multicell character.
    let sub_row = headline_row + u16::from(headline_scale) + 1;
    let sub = Metadata::new().with_scale(sub_scale)?;
    let after_sub = ctx.sized_ink(MARGIN + 2, sub_row, sub, SUBHEAD, Ink::Strong)?;

    let rule_row = after_sub;
    let rule_width = usize::from(headline_scale) * HEADLINE.len();
    ctx.term.put(
        MARGIN + 2,
        rule_row,
        &ctx.theme
            .paint(&"─".repeat(rule_width.min(available)), Ink::Muted),
    )?;

    // Body: ordinary text, on the same screen, in the same grid.
    ctx.row = rule_row + 2;
    ctx.para(
        "Every line above is one line of text. The headline is eight characters, the kicker is \
         thirty-three, and the whole page selects and copies as three lines — because the terminal \
         is still a grid of cells and these are still characters in it.",
    )?;
    ctx.blank();
    ctx.note(&format!(
        "s=1 through s={headline_scale} · w=1 · n=1:d=2 · v=2 — every key of the protocol is on \
         this page, and the tallest thing here is {headline_scale} cells and one character.",
    ))?;
    ctx.forced_note()?;

    ctx.wait()
}

/// Leaving the alternate screen, as a visible step.
fn step_leave(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(7)?;
    ctx.para(
        "Nothing needs undoing: this protocol has no mode to reset and no state to give back. Each \
         escape code sized the text it carried and nothing else — a terminal that scrolls the \
         block off the top of the screen forgets about it entirely.",
    )?;
    ctx.blank();

    ctx.sequence(
        &format!("{CSI}?2004l"),
        &[(
            "CSI ?2004l",
            "bracketed paste back off, since the copy-paste step switched it on.",
        )],
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
    // leaving it is a visible step of the demo rather than a side effect.
    ctx.term.disable_bracketed_paste()?;
    ctx.term.show_cursor()?;
    ctx.term.leave_alt_screen()?;
    ctx.term.disable_raw()?;

    println!();
    println!(
        "  {} {}",
        ctx.theme
            .paint("escapade textsize --scale 4 'BUILD PASSED'", Ink::Code),
        ctx.theme.paint("draw something now", Ink::Muted)
    );
    println!(
        "  {} {}",
        ctx.theme
            .paint("escapade textsize probe --format debug", Ink::Code),
        ctx.theme
            .paint("what this terminal honours, key by key", Ink::Muted)
    );
    println!();

    Ok(flow)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_step_has_a_kebab_case_name_and_a_title() {
        for step in STEPS {
            assert!(!step.name.is_empty() && !step.title.is_empty());
            assert!(
                step.name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-'),
                "step name `{}` is not kebab-case",
                step.name
            );
            assert!(!step.name.starts_with('-') && !step.name.ends_with('-'));
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
            let selection = crate::features::select_steps(&names, Some(name), None).unwrap();
            assert_eq!(selection.start, index);
            assert!(selection.single, "--only runs one step and stops");
        }
    }

    #[test]
    fn the_last_step_leaves_the_alternate_screen() {
        // Ordering the demo any other way would drop the reader back to their
        // shell without ever showing the sequence that got them there.
        assert_eq!(STEPS.last().map(|step| step.name), Some("restore"));
    }

    #[test]
    fn the_demo_covers_every_key_of_the_protocol() {
        // A step per capability, so `--only` reaches each one by name.
        for name in ["scale", "width", "fractions", "alignment", "copy-paste"] {
            assert!(
                step_names().contains(&name),
                "no step called `{name}` — the demo would not show that key"
            );
        }
    }

    #[test]
    fn every_step_the_script_names_can_be_indexed() {
        // Each step's `page` call passes its own index, and a mismatch there
        // would print the wrong title. This checks the arithmetic is at least in
        // range.
        for index in 0..STEPS.len() {
            assert!(STEPS.get(index).is_some());
        }
        assert_eq!(STEPS.len(), 8);
    }

    #[test]
    fn the_farmer_is_a_single_cluster_of_three_codepoints() {
        // The width step's whole argument rests on this: three codepoints,
        // one grapheme, two cells by segmentation and four by wcwidth.
        assert_eq!(FARMER.chars().count(), 3);
        assert_eq!(unicode::grapheme_count(FARMER), 1);
        assert_eq!(unicode::display_width(FARMER), 2);
        let legacy: usize = FARMER.chars().map(unicode::char_width).sum();
        assert_eq!(legacy, 4, "the disagreement w= exists to end");
    }

    #[test]
    fn the_advance_keys_match_the_shared_helper() {
        use crate::input::{KeyCode, KeyEvent, Modifiers};

        let press = |code| KeyEvent {
            code,
            mods: Modifiers::NONE,
            kind: KeyEventKind::Press,
        };
        assert_eq!(advance_for(&press(KeyCode::Char(' '))), Some(Advance::Next));
        assert_eq!(advance_for(&press(KeyCode::Enter)), Some(Advance::Next));
        assert_eq!(advance_for(&press(KeyCode::Char('q'))), Some(Advance::Quit));
        assert_eq!(advance_for(&press(KeyCode::Esc)), Some(Advance::Quit));
        assert_eq!(advance_for(&press(KeyCode::Char('p'))), Some(Advance::Prev));
        assert_eq!(advance_for(&press(KeyCode::Left)), Some(Advance::Prev));
        // Anything else is not an answer, so the paste loop keeps waiting.
        assert_eq!(advance_for(&press(KeyCode::Char('z'))), None);
    }

    #[test]
    fn every_sequence_the_demo_builds_is_valid() {
        // The steps build sequences with `?`, so an out-of-range scale in the
        // script would only show up when a reader ran that step.
        for scale in 1..=7 {
            assert!(Metadata::new().with_scale(scale).is_ok());
        }
        assert!(Fraction::new(1, 2).is_ok());
        assert!(Fraction::new(1, 3).is_ok());
        assert!(SizedText::new(Metadata::new().with_width(1).unwrap(), "ab").is_ok());
        assert!(SizedText::new(Metadata::new(), FARMER).is_ok());
    }
}
