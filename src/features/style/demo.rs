//! The step-by-step demo.
//!
//! Each step names the sequence it is about to send, takes it apart, and only then draws
//! with it — so by the end a reader has seen the bytes behind every mark on screen. The
//! arc is: how attributes compose, how they come back off again, the six underline shapes,
//! the three independent colours, what a terminal without 24-bit colour actually does, the
//! colon-versus-semicolon trap, the linter this all exists for, and then the terminal is
//! handed back.

use anyhow::Result;

use crate::{
    db::Catalog,
    features::{
        Support,
        style::{
            attrs::{Attr, Rendition, SGR_TWENTY_ONE, UnderlineShape, every_attribute},
            color::{Color, ColorDepth, ColorRole, ColorSyntax},
            preference::ColorPreference,
            probe,
        },
        wrap,
    },
    input::{Advance, InputReader},
    term::{CSI, Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// Left margin for everything the demo draws.
const MARGIN: u16 = 3;

/// A sub-parameter of `SGR 4` that no terminal defines, used to show what happens to a
/// shape a terminal has never heard of.
///
/// `4:9` is not a typo and not a hidden feature: it is deliberately out of range, so that
/// every terminal on earth is a "terminal that does not know this shape" for the length of
/// one line.
const UNDEFINED_SUB_PARAMETER: u8 = 9;

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

/// One entry in the demo script: a short name for `--only`, a title for the header, and the
/// code that draws it.
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

/// The script. `restore` is last, and leaving the alternate screen is its job.
const STEPS: &[Step] = &[
    step!("compose", "Nine attributes, one sequence", step_compose),
    step!("resets", "Getting them off again", step_resets),
    step!("underlines", "Six shapes, one parameter", step_underlines),
    step!("colors", "Three colours, all independent", step_colors),
    step!(
        "truecolor",
        "What 24-bit colour does when it cannot",
        step_truecolor
    ),
    step!("syntax", "Colons, semicolons, and the trap", step_syntax),
    step!("linter", "What this is actually for", step_linter),
    step!("restore", "Putting the terminal back", step_leave),
];

/// The step names, in order, for [`crate::features::select_steps`].
fn step_names() -> Vec<&'static str> {
    STEPS.iter().map(|step| step.name).collect()
}

/// Prints the step list for `--list`.
pub fn list_steps(theme: &Theme) {
    println!();
    println!("  {}", theme.paint("Text attributes demo", Ink::Title));
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
            "escapade style demo --only <NAME>   run just that step",
            Ink::Muted
        )
    );
    println!(
        "  {}",
        theme.paint(
            "escapade style demo --step N        start there and carry on",
            Ink::Muted
        )
    );
    println!();
}

/// Runs the demo.
///
/// # Errors
///
/// Fails if `--only` names no step, or if the terminal cannot be set up or written to.
pub fn run(
    theme: &Theme,
    catalog: &Catalog,
    force: bool,
    only: Option<&str>,
    start_step: Option<usize>,
) -> Result<()> {
    let selection = crate::features::select_steps(&step_names(), only, start_step)?;

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let plan = every_attribute();
    let outcome = probe::probe(catalog, &mut term, &mut input, &plan)?;

    // `--force` skips the capability check, but it cannot conjure a terminal: enabling raw
    // mode on something that is not a tty fails with ENXIO, so this gate comes first and is
    // not overridable.
    if !outcome.interactive {
        term.restore();
        refuse(theme, &outcome);
        return Ok(());
    }
    // Only now is `--force` meaningful: raw mode on something that is not a tty fails with
    // ENXIO, so the gate above comes first and is not overridable. This one is deliberately
    // generous — every attribute here degrades to being ignored rather than mishandled, so
    // demonstrating one that does not work withholds nothing — and refuses only when the
    // probe found a definite no for every single attribute.
    let findings = outcome.findings();
    let hopeless = !findings.is_empty()
        && findings
            .iter()
            .all(|finding| finding.support == Support::No);
    if hopeless && !force {
        term.restore();
        refuse_unsupported(theme);
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
        multiplexer: outcome.multiplexer,
        findings: outcome.findings(),
        measurable: outcome.measurable(),
        preference: outcome.preference.clone(),
        row: 0,
        width: 0,
    };

    let result = (|| -> Result<()> {
        loop {
            let flow = (STEPS[index].run)(&mut ctx)?;
            match flow {
                Flow::Next => {
                    // `--only` runs the one step and stops, rather than carrying on through
                    // the rest of the script.
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

    // Belt and braces: whatever a step left set, everything goes off here. `CSI 0 m` is the
    // right tool for once, because on the way out there is no surrounding styling to
    // preserve.
    let _ = ctx.term.write_now(&format!("{CSI}0m"));
    term.restore();
    result
}

/// The refusal shown when the probe found nothing on this terminal that works.
///
/// Separate from [`refuse`], which is about there being no terminal at all: that one is a
/// hard stop, and this one is a suggestion with `--force` as the answer.
fn refuse_unsupported(theme: &Theme) {
    println!();
    for line in wrap(
        "sorry - this does not seem to be supported by your terminal - check \
         'escapade style explain' for support, or use 'escapade style demo --force' to disable \
         feature probing.",
        theme.width().saturating_sub(2),
    ) {
        println!("  {}", theme.paint(&line, Ink::Warn));
    }
    println!();
    for line in wrap(
        "Every attribute came back as a definite no, which for this family means a terminal \
         implementing only the ECMA-48 core. --force is safe here: an attribute a terminal does \
         not implement is ignored rather than mishandled, so the demo would show you exactly \
         that happening.",
        theme.width().saturating_sub(2),
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();
}

/// The refusal shown when there is nothing to demonstrate on.
fn refuse(theme: &Theme, outcome: &probe::ProbeOutcome) {
    println!();
    for line in wrap(
        "sorry - this needs a real terminal to draw on - stdout and stdin are not both \
         terminals, so raw mode would fail. That is why --force cannot help here. Run this \
         directly rather than through a pipe.",
        theme.width().saturating_sub(2),
    ) {
        println!("  {}", theme.paint(&line, Ink::Warn));
    }
    if let Some(multiplexer) = outcome.multiplexer {
        println!();
        for line in wrap(
            &format!(
                "You are also inside {}, which parses SGR itself and re-emits it. Whether an \
                 attribute reaches the terminal beyond it depends on that terminal's terminfo \
                 carrying `Smulx` and `Setulc`.",
                multiplexer.name()
            ),
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
    multiplexer: Option<Multiplexer>,
    /// What the probe concluded, attribute by attribute.
    findings: Vec<probe::Finding>,
    /// Whether the terminal answered the attribute report at all.
    measurable: bool,
    /// The user's colour preference, so the colour steps can say when it is in force.
    preference: ColorPreference,
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
            .field("measurable", &self.measurable)
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
        let heading = "escapade \u{b7} text attributes";
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
        self.line(&self.theme.paint(&"\u{2500}".repeat(self.width), Ink::Muted))?;
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

    /// Draws a real styled sample, labelled with the parameters that made it.
    ///
    /// The label and the sample both come from the same [`Rendition`], so what a reader sees
    /// and what went on the wire cannot drift apart.
    fn sample(&mut self, rendition: &Rendition, text: &str, caption: &str) -> Result<()> {
        let row = format!(
            "{}  {}  {}",
            self.theme
                .paint(&format!("{:<18}", rendition.params()), Ink::Code),
            rendition.wrap(text),
            self.theme.paint(caption, Ink::Muted)
        );
        self.indented(2, &row)
    }

    /// Writes a raw sequence. Nothing here needs a multiplexer bypass: SGR is ordinary
    /// in-band styling that every multiplexer parses itself.
    fn send(&mut self, sequence: &str) -> Result<()> {
        self.term.write(sequence)
    }

    /// Writes one raw line at an indent, without the theme touching it.
    fn raw_line(&mut self, indent: u16, text: &str) -> Result<()> {
        let row = self.row;
        self.term.put(MARGIN + indent, row, text)?;
        self.row = self.row.saturating_add(1);
        Ok(())
    }

    /// What the probe concluded about one attribute, painted as a short column.
    fn verdict(&self, subject: Attr) -> String {
        match self
            .findings
            .iter()
            .find(|finding| finding.subject == subject)
        {
            Some(finding) => {
                let ink = match finding.support {
                    Support::Yes => Ink::Good,
                    Support::No => Ink::Bad,
                    Support::Unknown => Ink::Warn,
                };
                self.theme
                    .paint(&format!("probe: {}", finding.support.as_str()), ink)
            }
            None => String::new(),
        }
    }

    /// A line explaining where the `probe:` column came from, which differs per terminal.
    fn explain_probe_column(&mut self) -> Result<()> {
        if self.measurable {
            self.note(
                "The `probe:` column is a measurement, not a guess. Each attribute was set and the \
                 terminal was asked to report its own state back with DECRQSS; a terminal that \
                 kept what was sent parsed it, and one that reported something else did not.",
            )
        } else {
            self.note(
                "The `probe:` column is inference: this terminal does not answer DECRQSS for SGR, \
                 so there was nothing to measure and its identity was cross-referenced against \
                 escapade's table instead. `escapade style probe --format debug` shows the working.",
            )
        }
    }

    /// Draws the footer hint and blocks until the user chooses.
    fn wait(&mut self) -> Result<Flow> {
        let row = self.term.rows.saturating_sub(2);
        let hint = self
            .theme
            .paint("space/enter next \u{b7} p back \u{b7} q quit", Ink::Muted);
        self.term.put(MARGIN, row, &hint)?;
        self.term.flush()?;
        Ok(self.input.wait_for_advance()?.into())
    }
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// Attributes compose into one sequence, which is why this is one feature.
fn step_compose(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(0)?;
    ctx.para(
        "Select Graphic Rendition is the oldest escape sequence anybody still uses. Every text \
         attribute a terminal has is a parameter of it, separated by semicolons and applied left \
         to right — so all of them travel in one sequence.",
    )?;
    ctx.blank();

    let everything = Rendition::new()
        .with(Attr::Bold)
        .with(Attr::Italic)
        .with(Attr::Underline(UnderlineShape::Curly))
        .with(Attr::Strikethrough)
        .with(Attr::Color(ColorRole::Foreground, Color::Rgb(255, 0, 0)));

    ctx.sequence(
        &everything.open(),
        &[
            ("CSI", "Control Sequence Introducer \u{2014} `ESC [`."),
            ("1", "bold."),
            ("3", "italic."),
            (
                "4:3",
                "a curly underline. One parameter with a sub-parameter, not two parameters \u{2014} the colon is what makes an unsupporting terminal skip the whole thing.",
            ),
            ("9", "crossed out."),
            ("38;2;255;0;0", "a 24-bit red foreground."),
            ("m", "the final byte that makes this SGR."),
        ],
    )?;
    ctx.blank();

    ctx.sample(&everything, "all five at once", "")?;
    ctx.blank();

    ctx.note(&format!(
        "Five attributes in {} bytes. That composability is why `escapade style` is one \
         subcommand rather than nine: nine would force `escapade underline \"$(escapade bold \
         text)\"` and produce nested resets that fight each other.",
        everything.open().len()
    ))?;

    ctx.wait()
}

/// The reset mapping, which is the part nobody expects to be hard.
fn step_resets(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(1)?;
    ctx.para(
        "Every attribute has a parameter that turns it off, and the mapping is neither sequential \
         nor one-to-one. This is the part of SGR that surprises people.",
    )?;
    ctx.blank();

    for (attr, note) in [
        (
            Attr::Bold,
            "and so does faint \u{2014} one reset for two attributes",
        ),
        (Attr::Dim, "the other half of that pair"),
        (Attr::Italic, ""),
        (
            Attr::Underline(UnderlineShape::Curly),
            "whatever the shape was",
        ),
        (
            Attr::Blink,
            "and so does rapid blink \u{2014} the other shared reset",
        ),
        (Attr::Strikethrough, ""),
        (Attr::Overline, "note the gap: 55, not 54"),
    ] {
        let row = format!(
            "{}  {}  {}  {}",
            ctx.theme
                .paint(&format!("{:<12}", attr.name()), Ink::Strong),
            ctx.theme.paint(&format!("{:<5}", attr.params()), Ink::Code),
            ctx.theme
                .paint(&format!("off: {:<4}", attr.reset_params()), Ink::Accent),
            ctx.theme.paint(note, Ink::Muted)
        );
        ctx.indented(2, &row)?;
    }
    ctx.blank();

    ctx.para(
        "Two of those share a reset. `SGR 22` is \"normal intensity\" and cancels bold *and* \
         faint, so there is no way to stop being bold while staying faint; `SGR 25` does the same \
         for the two blinks. A program tracking its own attribute state has to model that, \
         because the terminal does.",
    )?;
    ctx.blank();

    ctx.indented(
        2,
        &ctx.theme.paint(
            &format!("…and then there is SGR {SGR_TWENTY_ONE}."),
            Ink::Warn,
        ),
    )?;
    ctx.blank();
    ctx.note(&format!(
        "ECMA-48 defines `{SGR_TWENTY_ONE}` as \"doubly underlined\". A great many terminals \
         implement it as \"bold off\" instead. That makes it the one parameter here whose meaning \
         genuinely depends on who is reading it, and there is no way to tell from the sending end \
         which you got. Both readings have unambiguous alternatives \u{2014} `4:2` sets a double \
         underline, `22` cancels bold \u{2014} so escapade never emits `{SGR_TWENTY_ONE}` at all.",
    ))?;
    ctx.blank();
    ctx.note(
        "escapade also resets only what it set, rather than reaching for `CSI 0 m`. A blanket \
         reset clears the *surrounding* program's attributes too, which is how one styled word in \
         the middle of a coloured line takes the rest of the line with it.",
    )?;

    ctx.wait()
}

/// The six underline shapes.
fn step_underlines(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(2)?;
    ctx.para(
        "An underline was one bit per cell for forty years, and then grew five more shapes as \
         sub-parameters of the same SGR 4. Not from xterm \u{2014} this is the rare terminal \
         feature xterm neither invented nor implements. They were originated by mintty.",
    )?;
    ctx.blank();

    for shape in UnderlineShape::ALL {
        let rendition = Rendition::new().with(Attr::Underline(*shape));
        let row = format!(
            "{}  {}  {}  {}  {}",
            ctx.theme
                .paint(&format!("{:<5}", shape.sub_parameter_params()), Ink::Code),
            ctx.theme
                .paint(&format!("{:<7}", shape.name()), Ink::Strong),
            ctx.theme.paint(shape.sketch(), Ink::Muted),
            rendition.wrap("the quick brown fox"),
            ctx.verdict(Attr::Underline(*shape))
        );
        ctx.indented(2, &row)?;
    }
    ctx.blank();

    ctx.note(
        "The box-drawing sketch in the third column shows what each shape is *meant* to look \
         like, so it is readable even where the terminal draws a plain line for all of them.",
    )?;
    ctx.blank();

    // Graceful degradation, demonstrated rather than asserted.
    let undefined = format!("{CSI}4:{UNDEFINED_SUB_PARAMETER}m");
    ctx.para(&format!(
        "The claim is that a terminal which does not know a shape falls back to a plain underline \
         rather than breaking. There is a shape no terminal implements \u{2014} `4:{UNDEFINED_SUB_PARAMETER}` \
         is deliberately out of range \u{2014} so every terminal on earth is a \"terminal that does \
         not know this shape\" for the length of one line:"
    ))?;
    ctx.blank();
    let sample = format!("{undefined}an undefined underline shape{CSI}24m");
    ctx.raw_line(2, &sample)?;
    ctx.send(&format!("{CSI}0m"))?;
    ctx.blank();
    ctx.note(
        "Most terminals draw a plain underline there, having kept the `4` and ignored the \
         sub-parameter. Some draw nothing. None print `4:9` as text or lose the rest of the line \
         \u{2014} which is the property that makes these safe to emit without probing first.",
    )?;
    ctx.blank();
    ctx.explain_probe_column()?;

    ctx.wait()
}

/// Three colours, three depths, all independent.
fn step_colors(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(3)?;
    ctx.para(
        "There are three independent colours, not one: the text, the cell background, and \u{2014} \
         much more recently \u{2014} the underline's own colour. Each has its own parameter and its \
         own reset, and that independence is the whole point.",
    )?;
    ctx.blank();

    for role in ColorRole::ALL.iter().copied() {
        let note = match role {
            ColorRole::Foreground => "the text colour",
            ColorRole::Background => "the cell background",
            ColorRole::Underline => "the underline only \u{2014} SGR 39 does not touch this",
        };
        let mut rendition = Rendition::new();
        if role == ColorRole::Underline {
            rendition = rendition.with(Attr::Underline(UnderlineShape::Curly));
        }
        rendition = rendition.with(Attr::Color(role, Color::Basic(1)));
        let row = format!(
            "{}  {}  {}  {}",
            ctx.theme
                .paint(&format!("{:<12}", role.name()), Ink::Strong),
            ctx.theme.paint(
                &format!(
                    "{:<8}",
                    format!("{}/{}", role.extended_param(), role.default_param())
                ),
                Ink::Code
            ),
            rendition.wrap("sample text"),
            ctx.theme.paint(note, Ink::Muted)
        );
        ctx.indented(2, &row)?;
    }
    ctx.blank();

    ctx.para(
        "Each of them comes at three depths, and which one you get depends on how you wrote it:",
    )?;
    ctx.blank();

    // One example per depth, with the depth naming itself rather than the strings
    // restating what `ColorDepth` already knows.
    for (written, color) in [
        ("red", Color::Basic(1)),
        ("173", Color::Palette(173)),
        ("#010203", Color::Rgb(1, 2, 3)),
    ] {
        let attr = Attr::Color(ColorRole::Foreground, color);
        let rendition = Rendition::new().with(attr);
        let depth = color.depth();
        let note = format!(
            "SGR {} \u{2014} the {} form, one of {}",
            attr.params(),
            depth.name(),
            depth.count()
        );
        let row = format!(
            "{}  {}  {}",
            ctx.theme
                .paint(&format!("--color {written:<9}"), Ink::Accent),
            rendition.wrap("sample text"),
            ctx.theme.paint(&note, Ink::Muted)
        );
        ctx.indented(2, &row)?;
    }
    ctx.blank();
    ctx.note(&format!(
        "Three depths, and escapade probes all of them separately: {}.",
        ColorDepth::ALL
            .iter()
            .map(|depth| format!("{} ({})", depth.name(), depth.count()))
            .collect::<Vec<_>>()
            .join(", ")
    ))?;
    ctx.blank();
    ctx.note(
        "Note that a bare number is always the 256-colour form, even below sixteen: `--color 1` \
         asks for `38;5;1` and `--color red` asks for `31`. They are different questions, and a \
         terminal can answer them differently.",
    )?;

    if ctx.preference.suppresses_color() {
        ctx.blank();
        ctx.note(
            "NO_COLOR is set in your environment, so `escapade style` would drop all three of \
             these and keep the bold and the underline shapes. This demo shows them anyway, \
             because showing you what the sequences do is the point of it.",
        )?;
    }

    ctx.wait()
}

/// The quantisation failure, which is the nastiest thing in the family.
fn step_truecolor(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(4)?;
    ctx.para(
        "This is the worst failure mode in the family, because it is not a failure. `SGR 38` is a \
         parameter every terminal recognises, so a 24-bit colour is never ignored \u{2014} a \
         terminal limited to 256 colours *accepts* it and quantises to the nearest palette entry.",
    )?;
    ctx.blank();

    let gradient: Vec<Rendition> = (0..24)
        .map(|step| {
            let value = u8::try_from(step * 11).unwrap_or(255);
            Rendition::new().with(Attr::Color(
                ColorRole::Background,
                Color::Rgb(value, 40, 255 - value),
            ))
        })
        .collect();
    let bar: String = gradient
        .iter()
        .map(|rendition| rendition.wrap("  "))
        .collect();
    ctx.indented(2, &bar)?;
    ctx.blank();
    ctx.note(
        "A smooth gradient means 24-bit colour. Visible bands, or a handful of flat blocks, mean \
         the terminal is rounding every one of those to a palette entry \u{2014} and it never said \
         so.",
    )?;
    ctx.blank();

    ctx.para(
        "That is why escapade probes with a deliberately awkward value rather than a round one:",
    )?;
    ctx.blank();
    ctx.sequence(
        &format!("{CSI}38;2;1;2;3m{CSI}0m"),
        &[
            (
                "1;2;3",
                "small components, none of them equal. No rounding can return this unchanged, so a terminal that reports it back really did keep 24 bits. `#ff0000` would come back looking correct from a terminal that quantised it to bright red, and the measurement would be worthless.",
            ),
        ],
    )?;
    ctx.blank();

    let truecolor = Attr::Color(ColorRole::Foreground, Color::Rgb(1, 2, 3));
    let row = format!(
        "{}  {}",
        ctx.theme.paint("this terminal:", Ink::Accent),
        ctx.verdict(truecolor)
    );
    ctx.indented(2, &row)?;
    if let Some(finding) = ctx
        .findings
        .iter()
        .find(|finding| finding.subject == truecolor)
    {
        let reason = finding.reason.clone();
        ctx.blank();
        ctx.note(&reason)?;
    }

    ctx.wait()
}

/// Colon versus semicolon, which is where the real-world breakage lives.
fn step_syntax(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(5)?;
    ctx.para(
        "There are two ways to spell an extended colour, and they are not the same string with a \
         different separator. This is the one thing in SGR worth being careful about.",
    )?;
    ctx.blank();

    let colon = Color::Rgb(255, 0, 0).params(ColorRole::Underline, ColorSyntax::Colon);
    let semicolon = Color::Rgb(255, 0, 0).params(ColorRole::Underline, ColorSyntax::Semicolon);
    for (syntax, params, gloss, ink) in [
        (
            ColorSyntax::Colon,
            &colon,
            "one parameter, five sub-parameters",
            Ink::Good,
        ),
        (
            ColorSyntax::Semicolon,
            &semicolon,
            "five separate parameters",
            Ink::Bad,
        ),
    ] {
        let row = format!(
            "{}  {}  {}",
            ctx.theme.paint(&format!("{params:<20}"), Ink::Code),
            ctx.theme
                .paint(&format!("{:<10}", syntax.name()), Ink::Accent),
            ctx.theme.paint(gloss, ink)
        );
        ctx.indented(2, &row)?;
    }
    ctx.blank();

    ctx.para(
        "On a terminal that implements the parameter both usually work. On one that does not, only \
         the first is harmless. ECMA-48 says an unrecognised parameter must be skipped whole, \
         sub-parameters included \u{2014} so `58:2::255:0:0` is discarded entirely. The semicolon \
         form has no such protection: it reads as `58` (unknown), `2` (faint), `255` (unknown), \
         `0`, `0`.",
    )?;
    ctx.blank();
    ctx.indented(
        2,
        &ctx.theme
            .paint("…and SGR 0 resets every attribute on the line.", Ink::Warn),
    )?;
    ctx.blank();

    ctx.indented(2, &ctx.theme.paint("what that looks like:", Ink::Muted))?;
    let sample =
        format!("{CSI}1;31mbold red text{CSI}{semicolon}m and the rest of this line{CSI}0m");
    ctx.raw_line(2, &sample)?;
    ctx.send(&format!("{CSI}0m"))?;
    ctx.blank();
    ctx.note(
        "If the bold red survived to the end of that line, this terminal implements SGR 58 and read \
         the semicolon form as one colour. If it went plain half way through, you have just watched \
         a colour specification turn into a full reset.",
    )?;
    ctx.blank();
    ctx.note(
        "escapade therefore spells 38 and 48 with semicolons \u{2014} that form is what every \
         terminal accepts, and being right about ITU-T T.416 is worth less than being understood \
         \u{2014} and spells 58 with colons, because 58 arrived after the colon form was \
         established and terminfo's `Setulc` emits colons. It never emits the semicolon form of 58.",
    )?;

    ctx.wait()
}

/// The linter: what the whole family is actually for.
fn step_linter(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(6)?;
    ctx.para(
        "This is the reason the newer half of the family exists. A linter, a spell-checker or a \
         compiler wants to mark one word inside a line without repainting it \u{2014} the text \
         keeps whatever colour its syntax highlighter gave it, and the mark is a shape and a \
         colour of its own.",
    )?;
    ctx.blank();

    let error = Rendition::new()
        .with(Attr::Underline(UnderlineShape::Curly))
        .with(Attr::Color(ColorRole::Underline, Color::Basic(9)));
    let warning = Rendition::new()
        .with(Attr::Underline(UnderlineShape::Curly))
        .with(Attr::Color(ColorRole::Underline, Color::Basic(11)));
    let hint = Rendition::new()
        .with(Attr::Underline(UnderlineShape::Dotted))
        .with(Attr::Color(ColorRole::Underline, Color::Basic(12)));

    ctx.indented(
        2,
        &ctx.theme
            .paint("a sentence with one misspelt word:", Ink::Muted),
    )?;
    ctx.blank();
    let sentence = format!(
        "The quick brown {} jumped over the lazy dog.",
        error.wrap("mispelled")
    );
    ctx.raw_line(4, &sentence)?;
    ctx.blank();

    ctx.sequence(
        &error.wrap("mispelled"),
        &[
            ("4:3", "curly \u{2014} the shape every editor uses for \"this is wrong\"."),
            (
                "58:5:9",
                "bright red, from the user's own palette, so it matches the rest of their theme. Note the colons.",
            ),
            (
                "24;59",
                "and both of them off again, so the full stop after the word is not underlined too. `39` would not have reset the underline colour.",
            ),
        ],
    )?;
    ctx.blank();

    ctx.indented(
        2,
        &ctx.theme
            .paint("three severities, one line of code:", Ink::Muted),
    )?;
    ctx.blank();
    let diagnostics = format!(
        "let x = {}({}, {});",
        warning.wrap("deprecated_call"),
        error.wrap("undefined_name"),
        hint.wrap("unused_arg")
    );
    ctx.raw_line(4, &diagnostics)?;
    ctx.blank();
    ctx.note(
        "Red curly for an error, yellow curly for a warning, blue dotted for a hint. Note what is \
         *not* happening: none of those words changed colour, so a syntax highlighter underneath \
         is undisturbed. That is what SGR 58 buys that a foreground colour cannot.",
    )?;
    ctx.blank();
    ctx.note(
        "A linter should still not rely on the shapes being distinguishable. On a terminal that \
         draws only plain lines, curly and dotted are the same mark \u{2014} so encode the severity \
         in the colour as well, which is what the three above do.",
    )?;

    if let Some(multiplexer) = ctx.multiplexer {
        ctx.blank();
        ctx.note(&format!(
            "You are inside {}, which parses SGR itself and re-emits what it can. A shape it \
             stores but cannot describe in the outer terminal's terminfo is dropped at that \
             boundary rather than at this one.",
            multiplexer.name()
        ))?;
    }

    ctx.wait()
}

/// The last step: reset everything and hand the terminal back.
fn step_leave(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(7)?;
    ctx.para(
        "Leaving an attribute set is a real bug \u{2014} the kind that leaves somebody's shell \
         prompt bold, or underlined in red, and gets blamed on the terminal.",
    )?;
    ctx.blank();

    let everything = Rendition::new()
        .with(Attr::Bold)
        .with(Attr::Italic)
        .with(Attr::Underline(UnderlineShape::Curly))
        .with(Attr::Strikethrough)
        .with(Attr::Overline)
        .with(Attr::Color(ColorRole::Foreground, Color::Basic(1)))
        .with(Attr::Color(ColorRole::Background, Color::Basic(4)))
        .with(Attr::Color(ColorRole::Underline, Color::Basic(2)));

    ctx.sequence(
        &everything.close(),
        &[
            (
                "22",
                "bold off \u{2014} and faint too, which shares this reset.",
            ),
            ("23", "italic off."),
            ("24", "underline off, whatever the shape was."),
            ("29", "crossed out off."),
            (
                "55",
                "overline off. Not 54, which is the reset for framed and encircled.",
            ),
            ("39", "foreground back to the default."),
            ("49", "background back to the default."),
            ("59", "underline colour back to following the foreground."),
        ],
    )?;
    ctx.blank();
    ctx.send(&format!("{CSI}0m"))?;
    ctx.indented(
        2,
        &ctx.theme
            .paint("\u{2714} every attribute back to its default", Ink::Good),
    )?;
    ctx.blank();

    ctx.sequence(
        &format!("{CSI}?1049l"),
        &[(
            "CSI ?1049l",
            "leave the alternate screen. Mode 1049 saved the cursor and switched buffers on the way in, so your scrollback is exactly as you left it \u{2014} this demo never scrolled it.",
        )],
    )?;
    ctx.blank();
    ctx.note("Press space to return to your shell.")?;

    let flow = ctx.wait()?;

    // The alternate screen goes away here rather than in the cleanup path, so that leaving
    // it is a visible step of the demo rather than a side effect.
    ctx.term.show_cursor()?;
    ctx.term.leave_alt_screen()?;
    ctx.term.disable_raw()?;

    println!();
    println!(
        "  {} {}",
        ctx.theme.paint("try it:", Ink::Accent),
        ctx.theme.paint(
            "escapade style --bold --underline=curly --underline-color red 'mispelled'",
            Ink::Code
        )
    );
    println!(
        "  {} {}",
        ctx.theme.paint("or ask:", Ink::Accent),
        ctx.theme
            .paint("escapade style probe --format debug", Ink::Code)
    );
    println!();

    Ok(flow)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_step_name_is_short_and_kebab_case() {
        for step in STEPS {
            assert!(!step.name.is_empty());
            assert!(
                step.name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-'),
                "{} is not kebab-case",
                step.name
            );
            assert!(step.name.len() <= 12, "{} is long for a --only", step.name);
            assert!(!step.title.is_empty());
        }
    }

    #[test]
    fn step_names_are_unique_and_resolvable() {
        let names = step_names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        let count = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), count, "two steps share a name");

        // Every name resolves to its own index, and stops there.
        for (index, name) in names.iter().enumerate() {
            let selection = crate::features::select_steps(&names, Some(name), None).unwrap();
            assert_eq!(selection.start, index, "{name}");
            assert!(selection.single, "--only must stop after one step");
        }
    }

    #[test]
    fn the_demo_ends_by_leaving_the_alternate_screen() {
        let last = STEPS.last().unwrap();
        assert_eq!(last.name, "restore");
        assert_eq!(
            STEPS.iter().filter(|step| step.name == "restore").count(),
            1
        );
    }

    #[test]
    fn the_script_covers_everything_the_feature_is_about() {
        let names = step_names();
        for wanted in [
            "compose",
            "resets",
            "underlines",
            "colors",
            "truecolor",
            "syntax",
            "linter",
        ] {
            assert!(names.contains(&wanted), "no `{wanted}` step");
        }
        assert_eq!(names.len(), STEPS.len());
    }

    #[test]
    fn a_step_index_matches_its_position_in_the_script() {
        // `page(n)` prints "step n+1/8" and the title of `STEPS[n]`, so the number passed in
        // has to match where the function actually sits.
        assert_eq!(STEPS.len(), 8);
        for (index, name) in [
            (0, "compose"),
            (1, "resets"),
            (2, "underlines"),
            (3, "colors"),
            (4, "truecolor"),
            (5, "syntax"),
            (6, "linter"),
            (7, "restore"),
        ] {
            assert_eq!(STEPS[index].name, name);
        }
    }

    #[test]
    fn the_undefined_sub_parameter_really_is_undefined() {
        // If this ever collides with a real shape, the degradation demonstration stops being
        // a demonstration and starts being a lie.
        assert!(
            UnderlineShape::ALL
                .iter()
                .all(|shape| shape.sub_parameter() != UNDEFINED_SUB_PARAMETER),
            "4:{UNDEFINED_SUB_PARAMETER} is a real shape now"
        );
        assert!(
            UnderlineShape::ALL
                .iter()
                .all(|shape| UNDEFINED_SUB_PARAMETER > shape.sub_parameter()),
            "it must sit above every defined shape, not merely differ from them"
        );
    }

    #[test]
    fn the_reset_step_lists_the_reset_that_each_attribute_actually_uses() {
        // The step prints `attr.reset_params()` directly, so this guards the claims the
        // surrounding prose makes about them.
        assert_eq!(Attr::Bold.reset_params(), Attr::Dim.reset_params());
        assert_eq!(Attr::Blink.reset_params(), Attr::RapidBlink.reset_params());
        assert_eq!(Attr::Overline.reset_params(), "55");
        assert_ne!(Attr::Overline.reset_params(), "54");
    }

    #[test]
    fn the_closing_sequence_the_last_step_shows_is_the_one_it_names() {
        // `step_leave` annotates eight reset parameters; if `close()` stops producing
        // exactly those, the annotations become wrong.
        let everything = Rendition::new()
            .with(Attr::Bold)
            .with(Attr::Italic)
            .with(Attr::Underline(UnderlineShape::Curly))
            .with(Attr::Strikethrough)
            .with(Attr::Overline)
            .with(Attr::Color(ColorRole::Foreground, Color::Basic(1)))
            .with(Attr::Color(ColorRole::Background, Color::Basic(4)))
            .with(Attr::Color(ColorRole::Underline, Color::Basic(2)));
        assert_eq!(everything.reset_params(), "22;23;24;29;55;39;49;59");
    }
}
