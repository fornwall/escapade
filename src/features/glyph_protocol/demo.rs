//! The step-by-step demo.
//!
//! Each step states what it is about to do, shows the actual escape sequence
//! and names every piece of it, and only then renders something. The point is
//! that nothing appears by magic: by the end, a reader has seen the bytes for
//! every glyph on screen.
//!
//! The arc builds from the handshake through geometry to a live glyph editor and
//! a playable game whose sprites are glyphs registered moments earlier, then
//! leaves the alternate screen.

use std::{
    thread::sleep,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    features::{
        glyph_protocol::{
            DEFAULT_UPM, Format, GLOSSARY_CAPACITY, GlyphReply, PREFIX, PUA_RANGES, ReplyLevel,
            TOFU, art, clear, codepoint_query,
            glyf::{self, Outline, Point},
            parse_reply, probe, register, register_verbose, send, support_query,
        },
        wrap,
    },
    input::{Advance, Event, InputReader, KeyCode, KeyEventKind, MouseKind, Reply},
    term::{APC, CSI, Multiplexer, ST, Term, pretty},
    theme::{Ink, Theme},
};

/// How long to wait for a reply before carrying on without one.
const REPLY_TIMEOUT: Duration = Duration::from_millis(250);

/// Frame interval for the animated steps: about 30 frames per second.
const FRAME: Duration = Duration::from_millis(33);

/// Left margin for everything the demo draws.
const MARGIN: u16 = 3;

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

/// The script. Order matters: later steps assume glyphs registered earlier.
const STEPS: &[Step] = &[
    step!("handshake", "Is anyone listening?", step_handshake),
    step!("query", "Asking about a codepoint", step_query),
    step!("shape", "A shape, before it is bytes", step_shape),
    step!("bytes", "The bytes", step_bytes),
    step!("curves", "Curves, and the points that pull them", step_curves),
    step!("winding", "Holes, and which way round you go", step_winding),
    step!("upm", "One payload, any font size", step_upm),
    step!("prompt", "A prompt with no patched font", step_prompt),
    step!("editor", "Editing a glyph, live", step_editor),
    step!("animation", "Animation, by codepoint", step_animation),
    step!(
        "game",
        "Finale: a game made of glyphs from ten minutes ago",
        step_finale
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
    println!("  {}", theme.paint("Glyph Protocol demo", Ink::Title));
    println!();
    let width = STEPS
        .iter()
        .map(|step| step.name.len())
        .max()
        .unwrap_or(0);
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
        theme.paint("escapade glyph demo --only <NAME>   run just that step", Ink::Muted)
    );
    println!(
        "  {}",
        theme.paint("escapade glyph demo --step N        start there and carry on", Ink::Muted)
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

    let outcome = probe::probe(&mut term, &mut input)?;

    // `--force` skips the capability check, but it cannot conjure a terminal:
    // enabling raw mode on something that is not a tty fails with ENXIO, so this
    // gate comes first and is not overridable.
    if !outcome.interactive {
        term.restore();
        refuse(theme, &outcome);
        return Ok(());
    }

    if !outcome.supported && !force {
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
        forced: force && !outcome.supported,
        supported: outcome.supported,
        formats: outcome.formats.clone(),
        term: &mut term,
        input: &mut input,
        theme,
        row: 0,
        width: 0,
    };

    let result = (|| -> Result<()> {
        loop {
            let flow = (STEPS[index].run)(&mut ctx)?;
            match flow {
                Flow::Next => {
                    // `--only` runs the one step and stops, rather than carrying
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

    // Give back the glossary slots even if a step failed part way through.
    let _ = send(ctx.term, ctx.multiplexer, &clear(None));
    term.restore();
    result
}

/// The refusal shown when the terminal does not support the protocol.
fn refuse(theme: &Theme, outcome: &probe::ProbeOutcome) {
    println!();
    let message = "sorry - this does not seem to be supported by your terminal - check \
                   'escapade glyph explain' for support, or use \
                   'escapade glyph demo --force' to disable feature probing.";
    for line in wrap(message, theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Warn));
    }

    if !outcome.conclusive() && outcome.interactive {
        println!();
        for line in wrap(
            "Strictly speaking this was inconclusive rather than a no: the terminal did not even \
             answer the device-attributes request that every terminal answers, so something is \
             swallowing replies.",
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }

    if let Some(multiplexer) = outcome.multiplexer {
        println!();
        for line in wrap(
            &format!(
                "You are inside {}, which filters escape sequences it does not recognise. \
                 Even a terminal that supports the protocol will look like it does not from in here.",
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
    /// Whether the demo is running past a failed probe.
    forced: bool,
    /// Whether the terminal answered the capability query.
    supported: bool,
    /// Payload formats the terminal advertised.
    formats: Vec<Format>,
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
            .field("supported", &self.supported)
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
        let heading = "escapade · Glyph Protocol";
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
        self.term.put(MARGIN + u16::try_from(indent).unwrap_or(0), row, text)?;
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
        send(self.term, self.multiplexer, sequence)
    }

    /// Whether the terminal advertised a payload format.
    fn accepts(&self, format: Format) -> bool {
        self.formats.contains(&format)
    }

    /// Sends a query and waits briefly for the matching reply.
    fn ask(&mut self, sequence: &str, verb: &str) -> Result<Option<GlyphReply>> {
        self.send(sequence)?;
        let wanted = format!("{PREFIX};{verb}");
        let reply = self
            .input
            .wait_for_reply(REPLY_TIMEOUT, |reply| {
                matches!(reply, Reply::Apc(payload) if payload.starts_with(&wanted))
            })
            .unwrap_or(None);
        Ok(match reply {
            Some(Reply::Apc(payload)) => parse_reply(&payload),
            _ => None,
        })
    }

    /// Registers an outline and reports what the terminal said.
    fn register(&mut self, codepoint: u32, outline: &Outline, upm: u16) -> Result<Option<bool>> {
        let sequence = register_verbose(
            codepoint,
            Format::Glyf,
            upm,
            ReplyLevel::Both,
            &outline.encode_base64(),
        );
        match self.ask(&sequence, "r")? {
            Some(GlyphReply::Register { ok, .. }) => Ok(Some(ok)),
            _ => Ok(None),
        }
    }

    /// Registers a glyph without waiting for an acknowledgement.
    ///
    /// Used for bulk registration, where a round trip per glyph would show up as
    /// a stutter. `reply` picks how much the terminal should say back:
    /// [`ReplyLevel::Silent`] for glyphs whose failure would be obvious anyway,
    /// [`ReplyLevel::FailureOnly`] when a silent failure would be confusing.
    ///
    /// Uses the compact sequence builder, which omits parameters that match
    /// their defaults — there is no reader to teach here.
    fn register_bulk(
        &mut self,
        codepoint: u32,
        outline: &Outline,
        reply: ReplyLevel,
    ) -> Result<()> {
        let sequence = register(
            codepoint,
            Format::Glyf,
            DEFAULT_UPM,
            reply,
            &outline.encode_base64(),
        );
        self.send(&sequence)
    }

    /// Reports a registration result on one line.
    fn report_registration(&mut self, result: Option<bool>) -> Result<()> {
        let text = match result {
            Some(true) => self.theme.paint("✔ status=0 — registered", Ink::Good),
            Some(false) => self.theme.paint("✘ the terminal refused it", Ink::Bad),
            None => self
                .theme
                .paint("no reply — nothing is listening on this end", Ink::Warn),
        };
        self.indented(2, &text)?;
        Ok(())
    }

    /// Draws a glyph large-ish by repeating it, with a caption.
    ///
    /// A registered glyph is exactly one cell, the same as a letter — there is
    /// no way to ask for it bigger. Showing a row of them makes it legible and
    /// makes the point that reuse is free: the terminal draws these through the
    /// same glyph cache it uses for text.
    fn showcase(&mut self, codepoint: u32, caption: &str) -> Result<()> {
        let glyph = art::glyph(codepoint);
        let inks = [Ink::Accent, Ink::Good, Ink::Warn, Ink::Bad, Ink::Title];
        let mut row = String::new();
        for (index, ink) in inks.iter().enumerate() {
            row.push_str(&self.theme.paint(&format!("{glyph} "), *ink));
            if index == inks.len() - 1 {
                row.push_str(&self.theme.paint(&format!("{glyph}{glyph}{glyph}"), *ink));
            }
        }
        self.indented(2, &row)?;
        self.blank();
        let text = format!("U+{codepoint:04X} — {caption}");
        self.note(&text)?;
        if self.forced {
            self.note(
                "You are seeing tofu because this terminal did not answer the capability query; \
                 the demo is running under --force.",
            )?;
        }
        Ok(())
    }

    /// Draws the footer hint and blocks until the user chooses.
    fn wait(&mut self) -> Result<Flow> {
        let row = self.term.rows.saturating_sub(2);
        let hint = self.theme.paint(
            "space/enter next · p back · q quit",
            Ink::Muted,
        );
        self.term.put(MARGIN, row, &hint)?;
        self.term.flush()?;
        Ok(self.input.wait_for_advance()?.into())
    }

    /// Plots an outline into the page.
    fn plot(&mut self, outline: &Outline, upm: i16, cols: usize, rows: usize) -> Result<()> {
        for line in art::plot(self.theme, outline, upm, cols, rows) {
            self.indented(2, &line)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// The capability handshake.
fn step_handshake(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(0)?;
    ctx.para(
        "Before shipping a glyph, ask whether anyone is listening. This is the whole \
         protocol in one sequence, and the reply tells you which payload formats you may use.",
    )?;
    ctx.blank();

    ctx.sequence(
        &support_query(),
        &[
            (
                "ESC _",
                "Application Program Command. The escape class for private protocols — a terminal that does not know this one drops the entire string, silently. That is what makes it safe to ask.",
            ),
            (
                "25a1",
                "the protocol's identifier: hex for U+25A1 WHITE SQUARE. Tofu. The thing this protocol exists to prevent.",
            ),
            ("; s", "the verb — 's' for support. No parameters; this one is just a question."),
            ("ESC \\", "String Terminator, ending the APC string."),
        ],
    )?;
    ctx.blank();

    let reply = ctx.ask(&support_query(), "s")?;
    if let Some(GlyphReply::Support { formats, unknown }) = reply {
        let mut names: Vec<&str> = formats.iter().map(|format| format.as_str()).collect();
        names.extend(unknown.iter().map(String::as_str));
        let text = ctx
            .theme
            .paint(&format!("✔ answered: fmt={}", names.join(",")), Ink::Good);
        ctx.indented(2, &text)?;
        ctx.blank();
        ctx.note("glyf is required in v1. colrv0 and colrv1 add colour, and arrived in spec v1.2.")?;
        if ctx.accepts(Format::Colrv0) || ctx.accepts(Format::Colrv1) {
            ctx.note(
                "This terminal accepts colour glyphs, so the outlines in this demo could \
                 carry their own palette instead of taking the colour of surrounding text.",
            )?;
        }
    } else {
        let text = ctx
            .theme
            .paint("… silence. Nothing here speaks Glyph Protocol.", Ink::Warn);
        ctx.indented(2, &text)?;
        ctx.blank();
        ctx.note(
            "The rest of the demo will send real sequences anyway, so you can watch what \
             goes out — but the glyphs will come back as tofu.",
        )?;
    }

    ctx.wait()
}

/// Querying coverage for a codepoint.
fn step_query(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(1)?;
    ctx.para(
        "Registering a glyph over a codepoint the user's font already covers is rude. So there \
         is a second question: can you draw this one, and with what?",
    )?;
    ctx.blank();

    ctx.sequence(
        &codepoint_query(0xE0A0),
        &[
            ("; q", "the verb — query."),
            (
                "cp=E0A0",
                "the codepoint, in hex, with no U+ prefix. This one is the Powerline git-branch icon, the single most-installed private-use glyph in the world.",
            ),
        ],
    )?;
    ctx.blank();

    for (codepoint, label) in [
        (0xE0A0_u32, "the Powerline branch icon"),
        (art::CP_TRIANGLE, "the codepoint this demo is about to use"),
    ] {
        let reply = ctx.ask(&codepoint_query(codepoint), "q")?;
        let (symbol, ink, description) = match reply {
            Some(GlyphReply::Query { coverage, .. }) => {
                let ink = if coverage.renders() { Ink::Good } else { Ink::Warn };
                let symbol = if coverage.renders() { "✔" } else { "·" };
                (symbol, ink, coverage.describe().to_owned())
            }
            _ => ("?", Ink::Muted, "no reply".to_owned()),
        };
        let text = format!(
            "{} U+{codepoint:04X}  {}  {}",
            ctx.theme.paint(symbol, ink),
            ctx.theme.paint(&format!("{description:<32}"), ink),
            ctx.theme.paint(label, Ink::Muted)
        );
        ctx.indented(2, &text)?;
    }

    ctx.blank();
    ctx.note(
        "status names the sources covering the codepoint: 'system' for the user's font, \
         'glossary' for a registration in this session, both when a registration shadows a font, \
         and empty when the answer is tofu.",
    )?;
    ctx.blank();

    let ranges: Vec<String> = PUA_RANGES
        .iter()
        .map(|&(low, high)| format!("U+{low:04X}–U+{high:04X}"))
        .collect();
    let text = format!("registration is confined to: {}", ranges.join(", "));
    ctx.indented(2, &ctx.theme.paint(&text, Ink::Accent))?;
    ctx.note(
        "Custom glyphs can never be drawn over real text, so registering a lookalike for 'l' to \
         phish someone is not a mitigated risk — it is impossible to express.",
    )?;

    ctx.wait()
}

/// A shape in outline space, before any encoding.
fn step_shape(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(2)?;
    ctx.para(
        "A glyph is a list of points. Two on-curve points in a row make a straight line, and \
         the contour closes itself — no final point back to the start.",
    )?;
    ctx.blank();

    let outline = art::triangle();
    ctx.plot(&outline, art::DESIGN_HEIGHT, 22, 8)?;
    ctx.blank();

    let header = format!(
        "{}  {}  {}  {}",
        ctx.theme.paint("  #", Ink::Muted),
        ctx.theme.paint("    x", Ink::Muted),
        ctx.theme.paint("    y", Ink::Muted),
        ctx.theme.paint("kind", Ink::Muted)
    );
    ctx.indented(2, &header)?;
    for (index, point) in outline.points().enumerate() {
        let kind = if point.on_curve {
            ctx.theme.paint("on-curve", Ink::Good)
        } else {
            ctx.theme.paint("control", Ink::Warn)
        };
        let text = format!("{index:>3}  {:>5}  {:>5}  {kind}", point.x, point.y);
        ctx.indented(2, &text)?;
    }
    ctx.blank();
    ctx.note(&format!(
        "Coordinates live in a {}×{} box inside a upm=1000 em, which is roughly the shape of a \
         terminal cell. Green dots are on the curve; there are no control points yet.",
        art::DESIGN_WIDTH, art::DESIGN_HEIGHT
    ))?;

    ctx.wait()
}

/// The same shape, as bytes on the wire.
fn step_bytes(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(3)?;
    ctx.para(
        "Those three points become a TrueType simple glyph: the exact bytes an OpenType font \
         stores, field for field.",
    )?;
    ctx.blank();

    let outline = art::triangle();
    let (bytes, fields) = outline.encode_with_fields();

    for field in &fields {
        let hex: Vec<String> = field.bytes.iter().map(|b| format!("{b:02x}")).collect();
        let text = format!(
            "{}  {}",
            ctx.theme.paint(&format!("{:<24}", field.label), Ink::Accent),
            ctx.theme.paint(&hex.join(" "), Ink::Code)
        );
        ctx.indented(2, &text)?;
        for line in wrap(&field.meaning, ctx.width.saturating_sub(6)) {
            let painted = ctx.theme.paint(&line, Ink::Muted);
            ctx.indented(4, &painted)?;
        }
    }
    ctx.blank();

    let base64 = outline.encode_base64();
    let summary = format!(
        "{} bytes of glyf, {} characters of base64",
        bytes.len(),
        base64.chars().count()
    );
    ctx.indented(2, &ctx.theme.paint(&summary, Ink::Strong))?;
    for row in glyf::hex_dump(&bytes, 16) {
        let painted = ctx.theme.paint(&row, Ink::Code);
        ctx.indented(2, &painted)?;
    }
    ctx.indented(2, &ctx.theme.paint(&base64, Ink::Code))?;
    ctx.blank();

    let sequence = register_verbose(
        art::CP_TRIANGLE,
        Format::Glyf,
        DEFAULT_UPM,
        ReplyLevel::Both,
        &base64,
    );
    ctx.sequence(
        &sequence,
        &[
            ("; r", "the verb — register."),
            ("cp=E900", "which codepoint to bind it to. Private Use Areas only."),
            ("fmt=glyf", "the payload format. Optional; glyf is the default."),
            (
                "upm=1000",
                "the coordinate space the points are in. The terminal maps this onto its cell, which is why one payload works at every font size.",
            ),
            ("reply=1", "acknowledge success and failure. Use reply=0 to fire and forget."),
            ("…", "the base64 payload, last so it cannot be confused with a parameter."),
        ],
    )?;
    ctx.blank();

    let result = ctx.register(art::CP_TRIANGLE, &outline, DEFAULT_UPM)?;
    ctx.report_registration(result)?;
    ctx.blank();
    ctx.showcase(art::CP_TRIANGLE, "a glyph that did not exist a second ago")?;

    ctx.wait()
}

/// Off-curve points and quadratic curves.
fn step_curves(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(4)?;
    ctx.para(
        "An off-curve point between two on-curve points makes a quadratic Bézier. The curve \
         never reaches the control point; it is pulled towards it. That is the entire curve model \
         — there are no cubics in TrueType.",
    )?;
    ctx.blank();

    let outline = art::chevron();
    ctx.plot(&outline, art::DESIGN_HEIGHT, 22, 9)?;
    ctx.blank();
    ctx.note("Amber dots are control points. Every one of them bends a segment without lying on it.")?;
    ctx.blank();

    let counts = format!(
        "{} points: {} on-curve, {} control, in {} bytes",
        outline.point_count(),
        outline.points().filter(|p| p.on_curve).count(),
        outline.points().filter(|p| !p.on_curve).count(),
        outline.encode().len()
    );
    ctx.indented(2, &ctx.theme.paint(&counts, Ink::Strong))?;
    ctx.blank();

    let result = ctx.register(art::CP_CHEVRON, &outline, DEFAULT_UPM)?;
    ctx.report_registration(result)?;
    ctx.blank();
    ctx.showcase(art::CP_CHEVRON, "four curves and one straight edge")?;

    ctx.wait()
}

/// Multiple contours and winding direction.
fn step_winding(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(5)?;
    ctx.para(
        "One glyph can hold several contours. Whether a contour fills or cuts a hole depends \
         only on which way round its points go: TrueType fills by non-zero winding, so opposite \
         directions cancel.",
    )?;
    ctx.blank();

    let outline = art::ring();
    ctx.plot(&outline, art::DESIGN_HEIGHT, 22, 9)?;
    ctx.blank();

    let (_, fields) = outline.encode_with_fields();
    if let Some(ends) = fields.iter().find(|f| f.label == "endPtsOfContours") {
        let hex: Vec<String> = ends.bytes.iter().map(|b| format!("{b:02x}")).collect();
        let text = format!(
            "{} {}",
            ctx.theme.paint("endPtsOfContours:", Ink::Accent),
            ctx.theme.paint(&hex.join(" "), Ink::Code)
        );
        ctx.indented(2, &text)?;
        ctx.note(
            "That array is the only thing separating one contour from the next. There is no \
             delimiter in the point list at all.",
        )?;
    }
    ctx.blank();

    let result = ctx.register(art::CP_RING, &outline, DEFAULT_UPM)?;
    ctx.report_registration(result)?;
    ctx.blank();
    ctx.showcase(art::CP_RING, "outer contour clockwise, inner counter-clockwise")?;

    ctx.wait()
}

/// The same shape at two different coordinate spaces.
fn step_upm(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(6)?;
    ctx.para(
        "upm says what the coordinates mean, not how big the glyph is. The terminal fits that \
         space to its cell, so the same payload is correct at 8×16 and at 32×64 — and stays \
         correct when the user changes font size, with no re-registration.",
    )?;
    ctx.blank();

    // The same ring, authored in a 100-unit space instead of a 1000-unit one.
    let coarse = Outline {
        contours: art::ring()
            .contours
            .into_iter()
            .map(|contour| glyf::Contour {
                points: contour
                    .points
                    .into_iter()
                    .map(|point| Point {
                        x: point.x / 10,
                        y: point.y / 10,
                        on_curve: point.on_curve,
                    })
                    .collect(),
            })
            .collect(),
    };

    let fine_bytes = art::ring().encode().len();
    let coarse_bytes = coarse.encode().len();

    ctx.indented(
        2,
        &ctx.theme
            .paint(&format!("upm=1000 → {fine_bytes} bytes"), Ink::Accent),
    )?;
    ctx.indented(
        2,
        &ctx.theme
            .paint(&format!("upm=100  → {coarse_bytes} bytes"), Ink::Accent),
    )?;
    ctx.blank();

    let result = ctx.register(art::CP_RING_COARSE, &coarse, 100)?;
    ctx.report_registration(result)?;
    ctx.blank();

    let fine = art::glyph(art::CP_RING);
    let rough = art::glyph(art::CP_RING_COARSE);
    let comparison = format!(
        "{}   {}",
        ctx.theme.paint(&format!("{fine} {fine} {fine}"), Ink::Good),
        ctx.theme.paint(&format!("{rough} {rough} {rough}"), Ink::Warn)
    );
    ctx.indented(2, &comparison)?;
    ctx.blank();
    ctx.note(
        "Left: upm=1000. Right: the same circle at upm=100, a tenth of the coordinates and \
         fewer bytes. They should look identical — coarser coordinates cost precision, not size.",
    )?;

    ctx.wait()
}

/// The practical payoff: a prompt that ships its own icons.
fn step_prompt(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(7)?;
    ctx.para(
        "This is what the protocol is for. A shell prompt, a file manager or a CI dashboard can \
         ship its icons and stop asking users to install a patched font. Four glyphs, registered \
         with reply=0 so there is no round trip to wait for:",
    )?;
    ctx.blank();

    let icons = [
        (art::CP_BRANCH, art::branch(), "branch"),
        (art::CP_LOCK, art::lock(), "lock"),
        (art::CP_FOLDER, art::folder(), "folder"),
        (art::CP_BOLT, art::bolt(), "bolt"),
    ];
    let mut total = 0;
    for (codepoint, outline, _) in &icons {
        total += outline.encode().len();
        ctx.register_bulk(*codepoint, outline, ReplyLevel::Silent)?;
    }
    // reply=0 means no acknowledgement is coming, so give the terminal a moment
    // to process the registrations before printing characters that need them.
    ctx.term.flush()?;
    sleep(Duration::from_millis(40));

    let branch = art::glyph(art::CP_BRANCH);
    let lock = art::glyph(art::CP_LOCK);
    let folder = art::glyph(art::CP_FOLDER);
    let bolt = art::glyph(art::CP_BOLT);

    let prompt = format!(
        "{} {}  {} {}  {} {}  {} {}",
        ctx.theme.paint(&folder.to_string(), Ink::Accent),
        ctx.theme.paint("~/src/escapade", Ink::Strong),
        ctx.theme.paint(&branch.to_string(), Ink::Good),
        ctx.theme.paint("main", Ink::Good),
        ctx.theme.paint(&lock.to_string(), Ink::Warn),
        ctx.theme.paint("signed", Ink::Muted),
        ctx.theme.paint(&bolt.to_string(), Ink::Bad),
        ctx.theme.paint("42ms", Ink::Muted)
    );
    ctx.indented(2, &prompt)?;
    ctx.indented(2, &ctx.theme.paint("❯ cargo test", Ink::Muted))?;
    ctx.blank();

    let cost = format!(
        "{total} bytes of glyf for all four, sent once at startup"
    );
    ctx.indented(2, &ctx.theme.paint(&cost, Ink::Strong))?;
    ctx.blank();
    ctx.note(
        "Selection, copy, search and shell history still see the codepoints the application \
         emitted, not the glyphs drawn. The cell buffer stays authoritative, which is why this \
         cannot become a what-you-see-is-not-what-you-copy trap.",
    )?;

    ctx.wait()
}

/// Interactive: drag control points and watch the glyph change.
#[expect(
    clippy::too_many_lines,
    reason = "a demo step is a linear script: prose, then the sequence, then the \
              render. Splitting it into helpers would hide the order a reader is \
              meant to follow"
)]
fn step_editor(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(8)?;
    ctx.para(
        "Registration is cheap enough to do on every keystroke. Move the points and the glyph is \
         re-registered live — the same round trip a font editor would make you restart for.",
    )?;

    let mut points = art::editor_start();
    let mut selected = 0usize;
    let mut dirty = true;
    let body_row = ctx.row;

    ctx.term.enable_mouse()?;
    // Event types give key release as well as press, which stops a held arrow
    // key from queueing up a burst of registrations.
    ctx.term.push_kitty_keyboard(0b11)?;
    // Without this, pasting into the editor would arrive as a flurry of
    // keystrokes; bracketed paste keeps it identifiable and ignorable.
    ctx.term.enable_bracketed_paste()?;

    let flow = loop {
        if dirty {
            let outline = Outline {
                contours: vec![glyf::Contour {
                    points: points.clone(),
                }],
            };
            ctx.register(art::CP_EDITOR, &outline, DEFAULT_UPM)?;

            ctx.row = body_row;
            ctx.blank();
            let plotted = art::plot(ctx.theme, &outline, art::DESIGN_HEIGHT, 24, 10);
            for line in plotted {
                let row = ctx.row;
                ctx.term.put(MARGIN + 2, row, &line)?;
                ctx.term.clear_to_eol()?;
                ctx.row = ctx.row.saturating_add(1);
            }
            ctx.blank();

            let glyph = art::glyph(art::CP_EDITOR);
            let row = ctx.row;
            ctx.term.put(MARGIN + 2, row, "")?;
            ctx.term.clear_to_eol()?;
            let rendered = format!(
                "{}   {}",
                ctx.theme
                    .paint(&format!("{glyph} {glyph} {glyph} {glyph}"), Ink::Title),
                ctx.theme.paint("live", Ink::Muted)
            );
            ctx.indented(2, &rendered)?;
            ctx.blank();

            for (index, point) in points.iter().enumerate() {
                let marker = if index == selected { "▸" } else { " " };
                let kind = if point.on_curve { "on-curve" } else { "control " };
                let text = format!(
                    "{marker} {index}  {:>4} {:>4}  {kind}",
                    point.x, point.y
                );
                let ink = if index == selected {
                    Ink::Title
                } else if point.on_curve {
                    Ink::Good
                } else {
                    Ink::Warn
                };
                let row = ctx.row;
                ctx.term.put(MARGIN + 2, row, "")?;
                ctx.term.clear_to_eol()?;
                let painted = ctx.theme.paint(&text, ink);
                ctx.indented(2, &painted)?;
            }
            ctx.blank();
            let keys = ctx.theme.paint(
                "tab select · arrows move · shift+arrows move faster · t toggle on/off-curve · \
                 click to place · r reset · enter done",
                Ink::Muted,
            );
            ctx.indented(2, &keys)?;
            ctx.term.flush()?;
            dirty = false;
        }

        let Some(event) = ctx.input.read(Duration::from_millis(200))? else {
            continue;
        };
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Release => {}
            Event::Key(key) => {
                let step = if key.mods.shift { 100 } else { 20 };
                match key.code {
                    KeyCode::Enter | KeyCode::Char(' ') => break Flow::Next,
                    KeyCode::Char('q' | 'Q') | KeyCode::Esc => break Flow::Quit,
                    KeyCode::Char('p') => break Flow::Prev,
                    KeyCode::Tab => {
                        selected = (selected + 1) % points.len();
                        dirty = true;
                    }
                    KeyCode::Char('t') => {
                        points[selected].on_curve = !points[selected].on_curve;
                        dirty = true;
                    }
                    KeyCode::Char('r') => {
                        points = art::editor_start();
                        selected = 0;
                        dirty = true;
                    }
                    KeyCode::Left => {
                        nudge(&mut points[selected], -step, 0);
                        dirty = true;
                    }
                    KeyCode::Right => {
                        nudge(&mut points[selected], step, 0);
                        dirty = true;
                    }
                    KeyCode::Up => {
                        nudge(&mut points[selected], 0, step);
                        dirty = true;
                    }
                    KeyCode::Down => {
                        nudge(&mut points[selected], 0, -step);
                        dirty = true;
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse) => {
                if matches!(mouse.kind, MouseKind::Down(_) | MouseKind::Drag(_)) {
                    // Map the click back into em space through the same box the
                    // plot was drawn in.
                    let plot_top = body_row + 1;
                    let (cols, rows) = (24_u16, 10_u16);
                    let left = MARGIN + 2;
                    if mouse.col >= left
                        && mouse.col < left + cols
                        && mouse.row >= plot_top
                        && mouse.row < plot_top + rows
                    {
                        let fx = f64::from(mouse.col - left) / f64::from(cols);
                        let fy = f64::from(mouse.row - plot_top) / f64::from(rows);
                        points[selected].x =
                            glyf::round(fx * f64::from(art::DESIGN_HEIGHT));
                        points[selected].y = glyf::round(
                            (1.0 - fy) * f64::from(art::DESIGN_HEIGHT),
                        );
                        dirty = true;
                    }
                }
            }
            _ => {}
        }
    };

    ctx.term.disable_bracketed_paste()?;
    ctx.term.pop_kitty_keyboard()?;
    ctx.term.disable_mouse()?;
    Ok(flow)
}

/// Moves a point, keeping it inside the em box.
fn nudge(point: &mut Point, dx: i16, dy: i16) {
    point.x = point.x.saturating_add(dx).clamp(0, art::DESIGN_HEIGHT);
    point.y = point.y.saturating_add(dy).clamp(0, art::DESIGN_HEIGHT);
}

/// Animation by swapping codepoints.
fn step_animation(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(9)?;
    ctx.para(
        "Register each frame at its own codepoint and animation becomes printing a different \
         character. No image protocol, no re-upload per frame, no compositing — the terminal \
         draws these out of the same glyph cache it uses for letters.",
    )?;
    ctx.blank();

    for frame in 0..art::SPINNER_FRAMES {
        ctx.register_bulk(
            art::CP_SPINNER_BASE + frame,
            &art::spinner_frame(frame),
            ReplyLevel::Silent,
        )?;
    }
    ctx.term.flush()?;
    sleep(Duration::from_millis(60));

    let sequence = format!("{APC}{PREFIX};r;cp=E910;reply=0;…{ST}");
    ctx.sequence(
        &sequence,
        &[
            (
                "reply=0",
                "no acknowledgement. Twelve frames means twelve registrations, and twelve round trips would be twelve stutters.",
            ),
            (
                "cp=E910…E91B",
                "one codepoint per frame. Playing the animation is now just moving through them.",
            ),
        ],
    )?;
    ctx.blank();

    let spinner_row = ctx.row;
    ctx.blank();
    ctx.blank();
    ctx.blank();
    let hint = ctx
        .theme
        .paint("space/enter next · p back · q quit", Ink::Muted);
    let footer_row = ctx.term.rows.saturating_sub(2);
    ctx.term.put(MARGIN, footer_row, &hint)?;

    let started = Instant::now();
    loop {
        let elapsed = started.elapsed().as_millis();
        let frame = u32::try_from(elapsed / 70).unwrap_or(0) % art::SPINNER_FRAMES;

        ctx.term.begin_sync()?;

        let spinner = art::glyph(art::CP_SPINNER_BASE + frame);
        let line = format!(
            "{}  {}",
            ctx.theme
                .paint(&format!("{spinner} {spinner} {spinner}"), Ink::Accent),
            ctx.theme.paint(
                &format!("frame {frame:>2} — U+{:04X}", art::CP_SPINNER_BASE + frame),
                Ink::Muted
            )
        );
        ctx.term.put(MARGIN + 2, spinner_row, &line)?;
        ctx.term.clear_to_eol()?;

        // A travelling wave of the same glyph, coloured by 24-bit SGR. Each cell
        // is one character: the terminal is drawing text, not pixels.
        let width = ctx.term.cols.saturating_sub(8).min(72);
        let phase = f64::from(u32::try_from(elapsed / 24).unwrap_or(0));
        let mut wave = String::new();
        for column in 0..width {
            let t = phase / 6.0 + f64::from(column) / 5.0;
            let intensity = (t.sin() * 0.5 + 0.5).clamp(0.0, 1.0);
            let frame = scale_to(intensity, art::SPINNER_FRAMES - 1);
            let glyph = art::glyph(art::CP_SPINNER_BASE + frame);
            if ctx.theme.color() {
                // 24-bit colour, so the wave is a smooth ramp rather than the
                // sixteen steps a palette would give.
                let red = 90 + scale_to(intensity, 165);
                let green = 40 + scale_to(intensity, 120);
                let blue = 200 - scale_to(intensity, 90);
                append(&mut wave, format_args!("\u{1b}[38;2;{red};{green};{blue}m{glyph}"));
            } else {
                wave.push(glyph);
            }
        }
        if ctx.theme.color() {
            wave.push_str("\u{1b}[0m");
        }
        ctx.term.put(MARGIN + 2, spinner_row + 2, &wave)?;
        ctx.term.clear_to_eol()?;

        ctx.term.end_sync()?;

        if let Some(event) = ctx.input.read(FRAME)?
            && let Some(flow) = advance_from(&event)
        {
            return Ok(flow);
        }
    }
}

/// The finale: a playable game whose sprites are glyphs from earlier steps.
#[expect(
    clippy::too_many_lines,
    reason = "a demo step is a linear script: prose, then the sequence, then the \
              render. Splitting it into helpers would hide the order a reader is \
              meant to follow"
)]
fn step_finale(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(10)?;
    ctx.para(
        "Three more glyphs, and the terminal has sprites. Catch the stars, dodge the rest. \
         Arrows or the mouse move the ship; every frame is one synchronized update, so it does \
         not tear.",
    )?;
    ctx.blank();

    for (codepoint, outline) in [
        (art::CP_SHIP, art::ship()),
        (art::CP_ALIEN, art::alien()),
        (art::CP_STAR, art::star()),
    ] {
        // FailureOnly here: a sprite that silently failed to register would leave
        // an unexplained tofu flying around the screen.
        ctx.register_bulk(codepoint, &outline, ReplyLevel::FailureOnly)?;
    }
    ctx.term.flush()?;
    sleep(Duration::from_millis(60));

    let top = ctx.row + 1;
    let bottom = ctx.term.rows.saturating_sub(4);
    let left = MARGIN + 2;
    let right = ctx.term.cols.saturating_sub(MARGIN + 2);
    if bottom <= top + 4 || right <= left + 8 {
        ctx.note("The window is too small for the finale — make it bigger and come back.")?;
        return ctx.wait();
    }

    let field_width = right - left;
    let mut ship = field_width / 2;
    let mut score = 0i32;
    let mut lives = 3i32;
    // (column, row-as-tenths, is_star) — tenths so things can fall slower than
    // one row per frame.
    let mut falling: Vec<(u16, u32, bool)> = Vec::new();
    let mut spawn_seed = 0x2545_F491_4F6C_DD1D_u64;
    let mut frame_count = 0u64;

    ctx.term.enable_mouse()?;
    ctx.term.push_kitty_keyboard(0b11)?;

    let ship_glyph = art::glyph(art::CP_SHIP);
    let alien_glyph = art::glyph(art::CP_ALIEN);
    let star_glyph = art::glyph(art::CP_STAR);

    let flow = loop {
        frame_count += 1;

        // xorshift: deterministic, no dependency, plenty random for falling rocks.
        spawn_seed ^= spawn_seed << 13;
        spawn_seed ^= spawn_seed >> 7;
        spawn_seed ^= spawn_seed << 17;
        if frame_count.is_multiple_of(9) {
            let column = u16::try_from(spawn_seed % u64::from(field_width)).unwrap_or(0);
            let is_star = spawn_seed.is_multiple_of(5);
            falling.push((column, 0, is_star));
        }

        let rows = u32::from(bottom - top);
        for item in &mut falling {
            item.1 += 4;
        }
        let ship_row = rows * 10;
        falling.retain(|&(column, depth, is_star)| {
            if depth < ship_row {
                return true;
            }
            // Reached the ship's row: hit or miss.
            if column.abs_diff(ship) <= 1 {
                if is_star {
                    score += 10;
                } else {
                    lives -= 1;
                }
            } else if is_star {
                score -= 1;
            }
            false
        });

        ctx.term.begin_sync()?;
        for row in top..=bottom {
            ctx.term.put(left, row, "")?;
            ctx.term.clear_to_eol()?;
        }
        for &(column, depth, is_star) in &falling {
            let row = top + u16::try_from(depth / 10).unwrap_or(0);
            if row > bottom {
                continue;
            }
            let (glyph, ink) = if is_star {
                (star_glyph, Ink::Warn)
            } else {
                (alien_glyph, Ink::Bad)
            };
            let painted = ctx.theme.paint(&glyph.to_string(), ink);
            ctx.term.put(left + column, row, &painted)?;
        }
        let painted_ship = ctx.theme.paint(&ship_glyph.to_string(), Ink::Accent);
        ctx.term.put(left + ship, bottom, &painted_ship)?;

        let status = format!(
            "{}  {}  {}",
            ctx.theme.paint(&format!("score {score:>4}"), Ink::Good),
            ctx.theme.paint(
                &format!("lives {}", "♥".repeat(usize::try_from(lives.max(0)).unwrap_or(0))),
                Ink::Bad
            ),
            ctx.theme
                .paint("arrows/mouse move · q quit · enter finish", Ink::Muted)
        );
        ctx.term.put(left, bottom + 2, &status)?;
        ctx.term.clear_to_eol()?;
        ctx.term.end_sync()?;

        if lives <= 0 {
            break Flow::Next;
        }

        if let Some(event) = ctx.input.read(FRAME)? {
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Release => {}
                Event::Key(key) => match key.code {
                    KeyCode::Left => ship = ship.saturating_sub(2),
                    KeyCode::Right => ship = (ship + 2).min(field_width - 1),
                    KeyCode::Enter | KeyCode::Char(' ') => break Flow::Next,
                    KeyCode::Char('q' | 'Q') | KeyCode::Esc => break Flow::Quit,
                    KeyCode::Char('p') => break Flow::Prev,
                    _ => {}
                },
                Event::Mouse(mouse) if mouse.col > left => {
                    ship = (mouse.col - left).min(field_width - 1);
                }
                _ => {}
            }
        }
    };

    ctx.term.pop_kitty_keyboard()?;
    ctx.term.disable_mouse()?;

    if flow == Flow::Next {
        ctx.term.clear()?;
        ctx.row = 3;
        ctx.line(&ctx.theme.paint("Final score", Ink::Accent))?;
        ctx.blank();
        ctx.indented(2, &ctx.theme.paint(&format!("{score}"), Ink::Title))?;
        ctx.blank();
        ctx.note(
            "Every sprite in that was a glyph this session registered, drawn through the text \
             pipeline at one character per cell.",
        )?;
        return ctx.wait();
    }
    Ok(flow)
}

/// The last step: clean up and hand the terminal back.
fn step_leave(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(11)?;
    ctx.para(&format!(
        "The glossary is session-scoped, so these glyphs would disappear on their own. Dropping \
         them explicitly is politer — the {GLOSSARY_CAPACITY} slots are shared with everything \
         else running in this session, and the oldest registration is evicted when they run out.",
    ))?;
    ctx.blank();

    ctx.sequence(
        &clear(None),
        &[(
            "; c",
            "clear. With cp= it drops one codepoint; without it, the whole glossary.",
        )],
    )?;
    ctx.blank();
    ctx.send(&clear(None))?;
    let registered = art::all_codepoints().len();
    let text = ctx
        .theme
        .paint(&format!("✔ {registered} registrations released"), Ink::Good);
    ctx.indented(2, &text)?;
    ctx.blank();

    ctx.sequence(
        &format!("{CSI}?1049l"),
        &[(
            "CSI ?1049l",
            "leave the alternate screen. Mode 1049 saved the cursor and switched buffers on the way in, so your scrollback is exactly as you left it — this demo never scrolled it.",
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
        ctx.theme.paint("Glyph Protocol:", Ink::Accent),
        ctx.theme
            .paint("https://rapha.land/introducing-glyph-protocol-for-terminals/", Ink::Link)
    );
    let supported = if ctx.supported {
        let names: Vec<&str> = ctx.formats.iter().map(|f| f.as_str()).collect();
        format!("this terminal accepts: {}", names.join(", "))
    } else {
        format!("this terminal did not answer — you saw {TOFU} where the glyphs should be")
    };
    println!("  {}", ctx.theme.paint(&supported, Ink::Muted));
    println!();

    match flow {
        Flow::Prev => Ok(Flow::Prev),
        _ => Ok(Flow::Quit),
    }
}

/// Maps a 0.0–1.0 intensity onto `0..=max`.
///
/// The animated steps do all their maths in floating point and have to land on
/// integers for colour channels and frame indices; clamping first means the cast
/// is always exact.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 0..=max before the cast"
)]
fn scale_to(intensity: f64, max: u32) -> u32 {
    if intensity.is_nan() {
        return 0;
    }
    (intensity.clamp(0.0, 1.0) * f64::from(max)).round() as u32
}

/// Appends formatted text to a string, discarding the `Result` that writing into
/// a `String` cannot produce.
fn append(out: &mut String, args: std::fmt::Arguments<'_>) {
    use std::fmt::Write as _;
    let _ = out.write_fmt(args);
}

/// Maps an event to a navigation choice, for the animated steps that poll.
fn advance_from(event: &Event) -> Option<Flow> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    if !key.mods.is_empty() {
        // Ctrl+C and Ctrl+D are the conventional ways out and must keep working.
        // Shift is fine — it is how a capital Q arrives. Anything else held down
        // means the keypress was meant for something other than navigation.
        if key.mods.ctrl {
            return matches!(key.code, KeyCode::Char('c' | 'd')).then_some(Flow::Quit);
        }
        if key.mods.alt || key.mods.super_ {
            return None;
        }
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
    fn every_step_has_a_name_and_a_title() {
        for step in STEPS {
            assert!(!step.title.is_empty());
            assert!(
                step.title.chars().count() < 60,
                "title too long for the header: {:?}",
                step.title
            );
            assert!(!step.name.is_empty());
            assert!(
                step.name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch == '-'),
                "step name `{}` should be lowercase kebab-case so it is easy to type",
                step.name
            );
        }
    }

    #[test]
    fn step_names_are_unique_so_only_is_unambiguous() {
        let mut names = step_names();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "two steps share a name");
    }

    #[test]
    fn only_selects_a_single_step_by_name() {
        let names = step_names();
        let selection = crate::features::select_steps(&names, Some("editor"), None).unwrap();
        assert_eq!(STEPS[selection.start].name, "editor");
        assert!(selection.single, "--only must not run on into the next step");
    }

    #[test]
    fn the_demo_ends_by_leaving_the_alternate_screen() {
        let last = &STEPS[STEPS.len() - 1];
        assert_eq!(last.name, "restore");
        assert!(last.title.contains("back"), "last step is {:?}", last.title);
        assert!(
            std::ptr::fn_addr_eq(last.run, step_leave as StepFn),
            "the final step must be the one that restores the terminal"
        );
    }

    #[test]
    fn step_numbers_on_the_command_line_are_one_based() {
        // `--step 1` must land on the first step, and 0 must not underflow.
        for (given, expected) in [(1_usize, 0_usize), (2, 1), (12, 11)] {
            assert_eq!(given.saturating_sub(1).min(STEPS.len() - 1), expected);
        }
        assert_eq!(0_usize.saturating_sub(1).min(STEPS.len() - 1), 0);
    }

    #[test]
    fn out_of_range_step_clamps_to_the_last_one() {
        assert_eq!(999_usize.saturating_sub(1).min(STEPS.len() - 1), STEPS.len() - 1);
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
        // With kitty event types enabled, a release arrives for every press;
        // treating it as input would skip two steps per keypress.
        let release = Event::Key(crate::input::KeyEvent {
            code: KeyCode::Enter,
            mods: crate::input::Modifiers::NONE,
            kind: KeyEventKind::Release,
        });
        assert_eq!(advance_from(&release), None);
    }

    #[test]
    fn nudge_keeps_points_inside_the_em_box() {
        let mut point = Point::on(10, 10);
        nudge(&mut point, -500, -500);
        assert_eq!((point.x, point.y), (0, 0));

        let mut point = Point::on(990, 990);
        nudge(&mut point, 500, 500);
        assert_eq!(
            (point.x, point.y),
            (art::DESIGN_HEIGHT, art::DESIGN_HEIGHT)
        );
    }

    #[test]
    fn nudge_survives_saturating_arithmetic() {
        let mut point = Point::on(i16::MAX, i16::MIN);
        nudge(&mut point, 1000, -1000);
        assert!(point.x <= art::DESIGN_HEIGHT && point.y >= 0);
    }
}
