//! The guided walkthrough: `escapade graphics demo`.
//!
//! Ten steps on the alternate screen, each one printing the escape sequence it is
//! about to send, naming every control key in it, and then sending it so the
//! reader sees the sequence and its effect at the same time. The order is
//! cumulative — transmit, formats, chunking, scaling, layering, deletion,
//! animation, placeholders — and the last two steps put it all together and then
//! hand the terminal back.
//!
//! Everything is drawn with explicit cursor positioning rather than newlines,
//! because raw mode makes `\n` a line feed without a carriage return, and because
//! an image is placed *at the cursor*: knowing exactly where the cursor is, is the
//! whole game.

use std::time::Duration;

use anyhow::Result;

use crate::{
    cli::Shape,
    features::{
        Selection,
        kitty_graphics::{
            layout::CellSize,
            pixels::{Colour, Rgba},
            placeholder,
            probe::Accepts,
            proto::{
                self, Action, Animation, AnimationState, Blend, Composition, CursorMovement,
                DeleteTarget, Deletion, Format, FrameData, Placement, Quiet, Rect, Sequence,
                Transmit,
            },
            render, shapes,
        },
        wrap,
    },
    input::{Advance, Event, InputReader, KeyCode, MouseKind, Reply},
    term::{Multiplexer, Term},
    theme::{Ink, Theme},
};

/// Image ids the demo uses. Picked in a band unlikely to collide with another
/// program's, and low enough to fit in a Unicode placeholder's foreground colour.
const ID_BASE: u32 = 0x00E5_0000;

/// The image transmitted in step one and referred to later.
const ID_FIRST: u32 = ID_BASE + 1;
/// The three format samples.
const ID_RGB: u32 = ID_BASE + 2;
/// RGBA sample.
const ID_RGBA: u32 = ID_BASE + 3;
/// PNG sample.
const ID_PNG: u32 = ID_BASE + 4;
/// The large image used to demonstrate chunking.
const ID_LARGE: u32 = ID_BASE + 5;
/// First id of the band the six shape steps use. Each step may take a few
/// consecutive ids for the copies it places side by side.
const ID_SHAPE: u32 = ID_BASE + 6;
/// The animated image.
const ID_ANIMATED: u32 = ID_BASE + 9;
/// The image displayed through Unicode placeholders.
const ID_PLACEHOLDER: u32 = ID_BASE + 10;
/// The interactive finale's sprite.
const ID_SPRITE: u32 = ID_BASE + 11;
/// The finale's backdrop.
const ID_BACKDROP: u32 = ID_BASE + 12;

/// How long to wait for a reply when the demo shows one.
const REPLY_TIMEOUT: Duration = Duration::from_millis(400);

/// Frame interval for the finale, in milliseconds.
const FRAME_MS: u64 = 60;

/// A step's implementation.
type StepFn = fn(&mut Stage<'_>) -> Result<Advance>;

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

/// The script. Order matters: the protocol is introduced first, then the six
/// generated shapes each carry one lesson about what the terminal does with the
/// pixels, and the last two steps put it together and hand the terminal back.
///
/// The shape steps are named after the shapes, so `--only mandelbrot` works and
/// `escapade graphics render --shape mandelbrot` shows the same image.
const STEPS: &[Step] = &[
    step!("transmit", "Transmit and place an image", |stage| stage
        .transmit_and_place()),
    step!("formats", "Three formats: RGB, RGBA and PNG", |stage| stage
        .formats()),
    step!("chunking", "Chunking: 4096 bytes at a time", |stage| stage
        .chunking()),
    step!(
        "gradient",
        "Gradient: a smooth ramp, and who quantises it",
        |stage| stage.gradient()
    ),
    step!(
        "checkerboard",
        "Checkerboard: alpha, z-index, and what shows through",
        |stage| stage.checkerboard()
    ),
    step!(
        "color-wheel",
        "Colour wheel: hue, saturation and the sRGB requirement",
        |stage| stage.color_wheel()
    ),
    step!(
        "mandelbrot",
        "Mandelbrot: fine detail, and what fitting it to cells costs",
        |stage| stage.mandelbrot()
    ),
    step!(
        "rings",
        "Rings: who resamples, the terminal or the client",
        |stage| stage.rings()
    ),
    step!(
        "test-card",
        "Test card: how big is a cell, and what fitting does",
        |stage| stage.test_card()
    ),
    step!("deletion", "Deleting images and placements", |stage| stage
        .deletion()),
    step!(
        "animation",
        "Animation: frames, gaps and composition",
        |stage| stage.animation()
    ),
    step!(
        "placeholders",
        "Unicode placeholders: images that behave like text",
        |stage| stage.placeholders()
    ),
    step!(
        "finale",
        "Putting it together (arrow keys, mouse, q to finish)",
        |stage| stage.finale()
    ),
    step!("farewell", "Leaving the alternate screen", |stage| stage
        .farewell()),
];

/// The step names, in order, for [`crate::features::select_steps`].
fn step_names() -> Vec<&'static str> {
    STEPS.iter().map(|step| step.name).collect()
}

/// A placement of `image` under `id`, transmitted and displayed in one go.
///
/// Every shape step wants the same thing — send these pixels, put them there, say
/// nothing back — so it lives here rather than six times over.
fn shape_sequences(id: u32, image: &Rgba, placement: &Placement) -> Vec<Sequence> {
    let transmit = Transmit::pixels(Format::Rgba, image.width(), image.height())
        .with_action(Action::TransmitAndDisplay)
        .with_id(id)
        .with_quiet(Quiet::Silent);
    proto::transmit(&transmit, Some(placement), image.as_bytes())
}

/// Prints the steps, with their names, for `--list`.
pub fn list_steps(theme: &Theme) {
    let width = STEPS
        .iter()
        .map(|step| step.name.chars().count())
        .max()
        .unwrap_or(0);

    println!();
    println!(
        "  {}",
        theme.paint("escapade graphics demo — the steps", Ink::Title)
    );
    println!();
    for (index, step) in STEPS.iter().enumerate() {
        println!(
            "  {} {}  {}",
            theme.paint(&format!("{:>2}.", index + 1), Ink::Muted),
            theme.paint(&format!("{:width$}", step.name), Ink::Accent),
            step.title
        );
    }
    println!();
    println!(
        "  {}",
        theme.paint(
            "Run one with --only NAME (a unique prefix will do), or start anywhere \
             with --step N. Space or Enter advances, p goes back, q quits.",
            Ink::Muted
        )
    );
    println!();
}

/// Runs the demo.
///
/// `only` runs a single named step and returns; `step` starts the whole walkthrough
/// at that number. The two are mutually exclusive at the command line.
///
/// # Errors
///
/// Fails if `only` names no step, if the terminal cannot be set up, if the protocol
/// is unsupported and `force` is not set, or if stdout cannot be written to.
pub fn run(theme: &Theme, force: bool, step: Option<usize>, only: Option<&str>) -> Result<()> {
    let selection = crate::features::select_steps(&step_names(), only, step)?;

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;

    // Probed unconditionally: `--force` waives the capability check, but not the
    // need for a terminal. Enabling raw mode on something that is not a tty fails
    // with ENXIO, so that gate is not overridable.
    let outcome = super::probe::probe(&mut term, &mut input)?;
    if !outcome.interactive {
        term.restore();
        anyhow::bail!(
            "a demo needs a terminal to draw on, and stdout is not one — \
             nothing could be probed and nothing can be rendered"
        );
    }
    if !outcome.supported && !force {
        term.restore();
        anyhow::bail!(render::unsupported_message("demo"));
    }

    let mut stage = Stage {
        term,
        input,
        theme,
        multiplexer: outcome.multiplexer,
        accepts: outcome.accepts,
        cell: outcome.cell,
        row: 1,
    };
    let result = stage.play(selection);
    stage.term.restore();
    result
}

/// The alternate screen, plus everything a step needs to draw on it.
struct Stage<'a> {
    term: Term,
    input: InputReader,
    theme: &'a Theme,
    multiplexer: Option<Multiplexer>,
    /// What the probe established the terminal accepts, which the transmission
    /// steps report rather than guessing at.
    accepts: Accepts,
    /// The measured cell size, which the test-card step is about.
    cell: CellSize,
    /// Next free row, 1-based. Every drawing helper advances it.
    row: u16,
}

impl std::fmt::Debug for Stage<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stage").field("row", &self.row).finish()
    }
}

impl Stage<'_> {
    /// Sets up the alternate screen for a walkthrough.
    fn open(&mut self) -> Result<()> {
        self.term.enable_raw()?;
        self.term.enter_alt_screen()?;
        self.term.hide_cursor()?;
        self.input.drain();
        Ok(())
    }

    /// Walks the script, honouring back, quit, and `--only`.
    fn play(&mut self, selection: Selection) -> Result<()> {
        self.open()?;

        let mut index = selection.start.min(STEPS.len() - 1);
        loop {
            match self.step(index)? {
                Advance::Quit => break,
                // `--only` means one step: there is nothing to go back to and
                // nothing to go on to.
                Advance::Next | Advance::Prev if selection.single => break,
                Advance::Prev => index = index.saturating_sub(1),
                Advance::Next => {
                    if index + 1 >= STEPS.len() {
                        break;
                    }
                    index += 1;
                }
            }
        }
        Ok(())
    }

    /// Draws one step and waits for the user.
    fn step(&mut self, index: usize) -> Result<Advance> {
        let step = &STEPS[index];
        self.begin(index, step)?;
        (step.run)(self)
    }

    // -- drawing ------------------------------------------------------------

    /// Clears the screen, removes every image from the previous step, and draws
    /// the header.
    fn begin(&mut self, index: usize, step: &Step) -> Result<()> {
        self.term.refresh_size()?;
        // Free the previous step's images, so a step never inherits a placement
        // it did not create.
        let clear = proto::delete(&Deletion {
            quiet: Quiet::Silent,
            ..Deletion::hard(DeleteTarget::Visible)
        });
        self.emit(&[clear])?;
        self.term.clear()?;
        self.row = 2;

        let title = format!(
            "{}/{}  {}  ({})",
            index + 1,
            STEPS.len(),
            step.title,
            step.name
        );
        self.put(&self.theme.paint(&title, Ink::Title).clone())?;
        self.row += 1;
        Ok(())
    }

    /// Writes one line at the next free row.
    fn put(&mut self, text: &str) -> Result<()> {
        let row = self.row;
        self.term.put(3, row, text)?;
        self.row += 1;
        Ok(())
    }

    /// Writes wrapped prose.
    fn prose(&mut self, text: &str) -> Result<()> {
        let width = usize::from(self.term.cols.saturating_sub(6)).min(90);
        for line in wrap(text, width) {
            self.put(&line)?;
        }
        Ok(())
    }

    /// Writes a blank line.
    fn gap(&mut self) {
        self.row += 1;
    }

    /// Shows a sequence as text: `→ APC G a=T,f=32,… ; <payload elided> ST`.
    fn show(&mut self, sequence: &Sequence) -> Result<()> {
        let arrow = self.theme.paint("→", Ink::Muted);
        let body = self.theme.paint(&sequence.display(24), Ink::Code);
        self.put(&format!("{arrow} {body}"))
    }

    /// Shows what came back from the terminal.
    fn show_reply(&mut self, text: &str) -> Result<()> {
        let arrow = self.theme.paint("←", Ink::Muted);
        let body = self.theme.paint(text, Ink::Good);
        self.put(&format!("{arrow} {body}"))
    }

    /// Explains the control keys one at a time.
    fn keys(&mut self, pairs: &[(&str, &str)]) -> Result<()> {
        let width = pairs
            .iter()
            .map(|(key, _)| key.chars().count())
            .max()
            .unwrap_or(0);
        for (key, meaning) in pairs {
            let padded = format!("{key:width$}");
            let line = format!(
                "    {}  {}",
                self.theme.paint(&padded, Ink::Accent),
                self.theme.paint(meaning, Ink::Muted)
            );
            self.put(&line)?;
        }
        Ok(())
    }

    /// Writes sequences to the terminal, wrapping them for a multiplexer.
    fn emit(&mut self, sequences: &[Sequence]) -> Result<()> {
        for sequence in sequences {
            let wire = sequence.wire();
            match self.multiplexer {
                Some(multiplexer) => self.term.write(&multiplexer.wrap(&wire))?,
                None => self.term.write(&wire)?,
            }
        }
        self.term.flush()
    }

    /// Moves the cursor to where an image should be placed and sends it there.
    ///
    /// Placement happens at the cursor, so this is the only way to control where
    /// an image lands — there is no "draw at pixel (x, y)" in the protocol, only
    /// "draw here, offset by up to one cell".
    fn place_at(&mut self, col: u16, row: u16, sequences: &[Sequence]) -> Result<()> {
        self.term.move_to(col, row)?;
        self.emit(sequences)
    }

    /// Draws the footer and blocks until the user asks to move.
    fn wait(&mut self) -> Result<Advance> {
        let row = self.term.rows;
        let hint = self
            .theme
            .paint("space next  ·  p back  ·  q quit", Ink::Muted);
        self.term.put(3, row, &hint)?;
        self.term.flush()?;
        Ok(self.input.wait_for_advance()?)
    }

    /// The row images should start on, given how much prose has been printed.
    fn image_row(&self) -> u16 {
        self.row + 1
    }

    // -- steps --------------------------------------------------------------

    /// Step 1: `a=T` — transmit data and display it in one command.
    fn transmit_and_place(&mut self) -> Result<Advance> {
        self.prose(
            "Every graphics command is an APC string: ESC _ G, a comma-separated list of \
             key=value control data, a semicolon, a base64 payload, and ESC \\. A terminal that \
             has never heard of the protocol drops the whole string silently, which is what \
             makes it safe to try.",
        )?;
        self.gap();
        self.prose(
            "The simplest useful command transmits pixels and displays them at the cursor in \
             one go. Here are 64x64 RGBA pixels — 16KB, so it also happens to need chunking, \
             which step 3 comes back to.",
        )?;
        self.gap();

        let image = shapes::gradient(64, 64)?;
        let transmit = Transmit::pixels(Format::Rgba, image.width(), image.height())
            .with_action(Action::TransmitAndDisplay)
            .with_id(ID_FIRST);
        let placement = Placement::default().with_area(Some(8), None);
        let sequences = proto::transmit(&transmit, Some(&placement), image.as_bytes());

        self.show(&sequences[0])?;
        self.keys(&[
            ("a=T", "transmit data and display it here"),
            ("f=32", "the payload is 32-bit RGBA, four bytes per pixel"),
            (
                "s=64,v=64",
                "width and height in pixels — required for raw pixels",
            ),
            (
                "i=…",
                "an image id, so later commands can refer to this image",
            ),
            (
                "c=8",
                "display it over 8 columns; the terminal derives the row count",
            ),
            ("m=1", "more chunks follow — see step 3"),
        ])?;
        self.gap();
        let quieter = Transmit::pixels(Format::Rgba, 64, 64)
            .with_action(Action::TransmitAndDisplay)
            .with_id(ID_FIRST)
            .with_quiet(Quiet::SuppressOk);
        self.prose("The same command told to keep the OK to itself:")?;
        self.show(&proto::transmit(&quieter, Some(&placement), &[])[0])?;
        self.gap();

        // With `q` left at its default the terminal answers, and in raw mode we
        // can actually show the answer.
        let row = self.image_row();
        self.place_at(4, row, &sequences)?;
        let reply = self
            .input
            .wait_for_reply(REPLY_TIMEOUT, |reply| matches!(reply, Reply::Apc(_)))?;
        self.row = row + 5;
        match reply {
            Some(Reply::Apc(payload)) => {
                let parsed = proto::parse_reply(&payload);
                self.show_reply(&format!("APC {payload} ST"))?;
                if let Some(parsed) = parsed {
                    let outcome = if parsed.is_ok() {
                        "the image loaded".to_owned()
                    } else {
                        format!("{:?}", parsed.error())
                    };
                    self.prose(&format!(
                        "The reply echoes the image id we chose and says {outcome}. Set q=1 to \
                         suppress that OK, or q=2 to suppress errors too — anything sharing a \
                         screen with another program wants at least q=1.",
                    ))?;
                }
            }
            _ => {
                self.prose(
                    "No reply came back within 400ms. The image may still have rendered; some \
                     terminals answer only when they must.",
                )?;
            }
        }
        self.wait()
    }

    /// Step 2: `f=24`, `f=32` and `f=100` side by side.
    fn formats(&mut self) -> Result<Advance> {
        self.prose(
            "A terminal is required to understand exactly three payload formats, and no image \
             library is needed for any of them. f=24 is three bytes per pixel, f=32 is four \
             with a straight alpha channel, and f=100 is a whole PNG file, whose dimensions \
             the terminal reads out of the IHDR chunk itself.",
        )?;
        self.gap();

        let opaque = shapes::gradient(96, 96)?;
        let translucent = shapes::checkerboard(96, 96)?;
        let wheel = shapes::color_wheel(96, 96)?;
        let png = wheel.encode_png()?;

        let as_rgb = {
            let transmit = Transmit::pixels(Format::Rgb, 96, 96)
                .with_action(Action::TransmitAndDisplay)
                .with_id(ID_RGB)
                .with_quiet(Quiet::Silent);
            proto::transmit(
                &transmit,
                Some(&Placement::default().with_area(Some(10), Some(5))),
                &opaque.flatten(Colour::BLACK),
            )
        };
        let as_rgba = {
            let transmit = Transmit::pixels(Format::Rgba, 96, 96)
                .with_action(Action::TransmitAndDisplay)
                .with_id(ID_RGBA)
                .with_quiet(Quiet::Silent);
            proto::transmit(
                &transmit,
                Some(&Placement::default().with_area(Some(10), Some(5))),
                translucent.as_bytes(),
            )
        };
        let as_png = {
            let transmit = Transmit::png()
                .with_action(Action::TransmitAndDisplay)
                .with_id(ID_PNG)
                .with_quiet(Quiet::Silent);
            proto::transmit(
                &transmit,
                Some(&Placement::default().with_area(Some(10), Some(5))),
                &png,
            )
        };

        self.show(&as_png[0])?;
        self.keys(&[
            ("f=100", "a PNG file, sent byte for byte"),
            (
                "no s or v",
                "PNG carries its own size, so the dimension keys are omitted",
            ),
            (
                "q=2",
                "say nothing back: this step is not listening for replies",
            ),
        ])?;
        self.gap();
        self.prose(&format!(
            "The same 96x96 image costs {} bytes as RGB, {} as RGBA, and {} as PNG. \
             PNG is usually the smallest and is the only one of the three that compresses, \
             but it also makes the terminal do the decoding.",
            96 * 96 * 3,
            96 * 96 * 4,
            png.len()
        ))?;

        let row = self.image_row();
        self.place_at(4, row, &as_rgb)?;
        self.place_at(18, row, &as_rgba)?;
        self.place_at(32, row, &as_png)?;
        self.row = row + 6;
        self.put(
            &self
                .theme
                .paint("   f=24 RGB      f=32 RGBA     f=100 PNG", Ink::Accent)
                .clone(),
        )?;
        self.gap();
        self.prose(
            "The middle one is a checkerboard of opaque and fully transparent squares: what \
             shows through is whatever the terminal composites images against.",
        )?;
        self.wait()
    }

    /// Step 3: `m=1` and `m=0`, and the 4096-byte limit.
    fn chunking(&mut self) -> Result<Advance> {
        self.prose(
            "An escape sequence cannot be arbitrarily long, so a payload is base64-encoded and \
             then cut into chunks of at most 4096 bytes. Every chunk except the last carries \
             m=1; the last carries m=0. Only the first chunk carries the real control data — \
             the rest may contain nothing but m, q, and a=f for animation frames.",
        )?;
        self.gap();

        let image = shapes::mandelbrot(320, 240)?;
        let transmit = Transmit::pixels(Format::Rgba, image.width(), image.height())
            .with_action(Action::TransmitAndDisplay)
            .with_id(ID_LARGE)
            .with_quiet(Quiet::Silent);
        let sequences = proto::transmit(
            &transmit,
            Some(&Placement::default().with_area(Some(30), None)),
            image.as_bytes(),
        );

        self.show(&sequences[0])?;
        self.show(&sequences[1])?;
        self.put(&self.theme.paint("  ⋮", Ink::Muted).clone())?;
        self.show(sequences.last().unwrap_or(&sequences[0]))?;
        self.gap();
        self.prose(&format!(
            "That is {} pixels — {} bytes of RGBA, {} of base64 — in {} sequences. Chunks must \
             be a multiple of four bytes so a boundary never splits a base64 quantum, and no \
             other graphics command may be interleaved with them. The terminal displays nothing \
             until the last chunk arrives.",
            image.width() * image.height(),
            image.as_bytes().len(),
            sequences.iter().map(|s| s.payload.len()).sum::<usize>(),
            sequences.len()
        ))?;

        self.gap();
        let media = if self.accepts.has_any() {
            let mut available = Vec::new();
            if self.accepts.local_files {
                available.push("t=f, so an image already on disk can be sent as a path");
            }
            if self.accepts.compression {
                available.push("o=z, so the payload can be deflated first");
            }
            format!(
                "None of this is necessary when the terminal and the client share a \
                 machine. This one answered a query action saying it accepts {}.",
                available.join(", and ")
            )
        } else {
            "When the terminal and the client share a machine, all of this can be \
             skipped by sending a file path (t=f) or a shared-memory name (t=s) instead \
             — but this terminal did not accept either when asked, so the bytes go down \
             the pty."
                .to_owned()
        };
        self.prose(&media)?;

        let row = self.image_row();
        self.place_at(4, row, &sequences)?;
        self.row = row + 12;
        self.wait()
    }

    /// Prints the one-line reason a generated shape exists.
    ///
    /// The same sentence `escapade graphics render --shape … --dry-run` prints, so
    /// the two cannot drift apart.
    fn shape_purpose(&mut self, shape: Shape) -> Result<()> {
        let line = format!("{} — {}", shapes::name(shape), shapes::purpose(shape));
        self.put(&self.theme.paint(&line, Ink::Accent).clone())
    }

    /// Step 4: the gradient, and where colour banding comes from.
    fn gradient(&mut self) -> Result<Advance> {
        self.shape_purpose(Shape::Gradient)?;
        self.gap();
        self.prose(
            "Two ramps at right angles, one per channel. The protocol requires sRGB and \
             8 bits per channel, so a smooth ramp across a wide terminal has more pixels \
             than it has values to put in them — any banding you can see was already in \
             the payload, not added by the terminal. What the terminal *does* add is the \
             scaling: this is 512 pixels of ramp fitted into the cells below.",
        )?;
        self.gap();

        let image = shapes::gradient(512, 128)?;
        let placement = Placement::default().with_area(Some(48), Some(6));
        let sequences = shape_sequences(ID_SHAPE, &image, &placement);
        self.show(&sequences[0])?;
        self.keys(&[
            ("a=T", Action::TransmitAndDisplay.describe()),
            ("f=32", "four bytes per pixel, straight alpha, sRGB"),
            ("s=512,v=128", "the pixels we generated"),
            ("c=48,r=6", "the cells they are fitted into"),
        ])?;

        let row = self.image_row();
        self.place_at(4, row, &sequences)?;
        self.row = row + 7;
        self.wait()
    }

    /// Step 5: the checkerboard, alpha compositing and `z`.
    fn checkerboard(&mut self) -> Result<Advance> {
        self.shape_purpose(Shape::Checkerboard)?;
        self.gap();
        self.prose(
            "Half of this image is fully transparent, so it answers a question the other \
             shapes cannot: what does the terminal composite an image *against*? On the \
             left, sent as f=32 with the alpha channel intact and placed at z=-1, the \
             answer is the cell background and the text — the glyphs are drawn over the \
             picture. On the right the same image was flattened here first, onto a solid \
             colour, and sent as f=24: three bytes per pixel and no transparency left to \
             composite.",
        )?;
        self.gap();

        let image = shapes::checkerboard(192, 192)?;
        let flattened = image.over(Colour {
            red: 0x30,
            green: 0x20,
            blue: 0x50,
        });

        let transparent = shape_sequences(
            ID_SHAPE,
            &image,
            &Placement::default()
                .with_area(Some(18), Some(9))
                .with_z(Some(-1)),
        );
        // The flattened copy has nothing transparent left, so the alpha channel is a
        // quarter of the payload carrying no information: send it as f=24.
        let opaque = {
            let transmit = Transmit::pixels(Format::Rgb, flattened.width(), flattened.height())
                .with_action(Action::TransmitAndDisplay)
                .with_id(ID_SHAPE + 1)
                .with_quiet(Quiet::Silent);
            let placement = Placement::default().with_area(Some(18), Some(9));
            proto::transmit(
                &transmit,
                Some(&placement),
                &flattened.flatten(Colour::BLACK),
            )
        };

        self.show(&transparent[0])?;
        self.keys(&[
            (
                "f=32",
                "keep the alpha channel and let the terminal composite",
            ),
            ("z=-1", "draw underneath the text in these cells"),
            (
                "f=24",
                "or flatten it here and send three bytes per pixel instead",
            ),
        ])?;
        self.gap();
        self.prose(&format!(
            "A z below {} goes under cells with a non-default background colour as well \
             as under the text, which is the layer an application wants for a wallpaper.",
            proto::Z_BELOW_BACKGROUNDS
        ))?;

        let row = self.image_row();
        self.place_at(4, row, &transparent)?;
        self.place_at(26, row, &opaque)?;
        // Text into the very cells the transparent copy occupies.
        for offset in 0..4u16 {
            let text = match offset {
                0 => "text over",
                1 => "the holes",
                2 => "in the",
                _ => "checkerboard",
            };
            self.term.put(
                6,
                row + 2 + offset,
                &self.theme.paint(text, Ink::Strong).clone(),
            )?;
        }
        self.row = row + 10;
        self.put(
            &self
                .theme
                .paint("   f=32, z=-1            f=24, flattened", Ink::Accent)
                .clone(),
        )?;
        self.wait()
    }

    /// Step 6: the colour wheel, and the sRGB requirement.
    fn color_wheel(&mut self) -> Result<Advance> {
        self.shape_purpose(Shape::ColorWheel)?;
        self.gap();
        self.prose(
            "Hue around, saturation outwards, and a one-pixel feathered rim so the circle \
             does not look stair-stepped before the terminal has even touched it. The \
             colours here are the bytes the terminal blends with, because the protocol \
             requires the payload to be in the sRGB colour space — there is no colour \
             management step and no profile to attach.",
        )?;
        self.gap();

        let image = shapes::color_wheel(256, 256)?;
        let placement = Placement::default().with_area(Some(14), Some(7));
        let sequences = shape_sequences(ID_SHAPE, &image, &placement);
        self.show(&sequences[0])?;
        self.gap();
        self.prose(
            "The corners outside the circle are transparent, which is why the rim blends \
             into whatever is behind it rather than sitting in a black box.",
        )?;

        let row = self.image_row();
        self.place_at(4, row, &sequences)?;
        self.row = row + 8;
        self.wait()
    }

    /// Step 7: the Mandelbrot set, and what fitting detail into cells costs.
    fn mandelbrot(&mut self) -> Result<Advance> {
        self.shape_purpose(Shape::Mandelbrot)?;
        self.gap();
        self.prose(
            "Detail at every scale, which makes it the worst case for fitting an image to \
             a character grid. The same 480x360 payload is placed twice: on the left over \
             a generous rectangle, on the right squeezed into a few cells. Nothing was \
             resampled here — both placements carry the identical pixels and differ only \
             in c and r, so what you are looking at is the terminal's own downscaling.",
        )?;
        self.gap();

        let image = shapes::mandelbrot(480, 360)?;
        let large = shape_sequences(
            ID_SHAPE,
            &image,
            &Placement::default().with_area(Some(32), Some(12)),
        );
        let small = shape_sequences(
            ID_SHAPE + 1,
            &image,
            &Placement::default().with_area(Some(8), Some(3)),
        );
        self.show(&small[0])?;
        self.keys(&[
            ("c=32,r=12", "the same image, fitted generously"),
            ("c=8,r=3", "and fitted into a thumbnail"),
        ])?;

        let row = self.image_row();
        self.place_at(4, row, &large)?;
        self.place_at(38, row, &small)?;
        self.row = row + 13;
        self.wait()
    }

    /// Step 8: rings, and the choice of who does the resampling.
    fn rings(&mut self) -> Result<Advance> {
        self.shape_purpose(Shape::Rings)?;
        self.gap();
        self.prose(
            "Concentric rings alias badly on purpose, which makes them the way to see who \
             is doing the resampling. Left: the full 384x384 payload with c and r, so the \
             *terminal* scales — and on a HiDPI screen it will use every pixel sent, \
             because it renders the placement at its real device resolution. Right: the \
             same image resampled here first, with a box filter, down to exactly the \
             pixels the placement covers, and sent with no c or r at all.",
        )?;
        self.gap();

        let image = shapes::rings(384, 384)?;
        let cells = (12u16, 6u16);
        let terminal_scaled = shape_sequences(
            ID_SHAPE,
            &image,
            &Placement::default().with_area(Some(cells.0), Some(cells.1)),
        );

        // What the placement actually covers, in pixels, from the measured cell size.
        let target = (
            u32::from(cells.0) * self.cell.width,
            u32::from(cells.1) * self.cell.height,
        );
        let resampled = image.resize(target.0.max(1), target.1.max(1))?;
        let client_scaled = shape_sequences(
            ID_SHAPE + 1,
            &resampled,
            // No c or r: one image pixel per screen pixel.
            &Placement::default(),
        );

        self.show(&terminal_scaled[0])?;
        self.show(&client_scaled[0])?;
        self.keys(&[
            (
                "c=12,r=6",
                "the terminal scales, and keeps the detail for a HiDPI screen",
            ),
            (
                "no c or r",
                "one image pixel per screen pixel, already resampled here",
            ),
        ])?;
        self.gap();
        self.prose(&format!(
            "The client-side copy is {}x{} pixels because a cell here is {}x{} — {}. \
             Sending it costs a fraction of the bytes and throws away everything a \
             higher-resolution display could have used.",
            resampled.width(),
            resampled.height(),
            self.cell.width,
            self.cell.height,
            self.cell.source.describe()
        ))?;

        let row = self.image_row();
        self.place_at(4, row, &terminal_scaled)?;
        self.place_at(20, row, &client_scaled)?;
        self.row = row + 7;
        self.put(
            &self
                .theme
                .paint("   terminal scales      client resampled", Ink::Accent)
                .clone(),
        )?;
        self.wait()
    }

    /// Step 9: the test card, cell size, and what fitting does.
    fn test_card(&mut self) -> Result<Advance> {
        self.shape_purpose(Shape::TestCard)?;
        self.gap();
        self.prose(&format!(
            "A border on every edge, corner brackets, a centre cross and a grey ramp — so \
             you can see exactly where the image starts and stops once the terminal has \
             fitted it to cells. That fitting needs one number the protocol does not \
             provide: how many pixels a cell has. CSI 16 t asks directly and CSI 14 t \
             gives the window size to divide; here the answer was {}x{} pixels, {}.",
            self.cell.width,
            self.cell.height,
            self.cell.source.describe()
        ))?;
        self.gap();

        let image = shapes::test_card(240, 240)?;
        let contained = shape_sequences(
            ID_SHAPE,
            &image,
            &Placement::default().with_area(Some(10), None),
        );
        let stretched = shape_sequences(
            ID_SHAPE + 1,
            &image,
            &Placement::default().with_area(Some(16), Some(4)),
        );
        let cropped = shape_sequences(
            ID_SHAPE + 2,
            &image,
            &Placement::default()
                .with_area(Some(10), Some(5))
                .with_source(Some(Rect::new(60, 60, 120, 120))),
        );

        self.show(&cropped[0])?;
        self.keys(&[
            (
                "c=10",
                "one axis only: the terminal derives the other exactly",
            ),
            (
                "c=16,r=4",
                "both axes: the aspect ratio is whatever you asked for",
            ),
            ("x=60,y=60", "a source rectangle, in image pixels…"),
            ("w=120,h=120", "…which crops rather than scales"),
        ])?;
        self.gap();
        self.prose(
            "The square copy kept its proportions because only c was given. The wide one \
             was told both numbers and distorted to obey. The third is the middle quarter \
             of the same payload: the border and brackets are not missing, they are \
             outside the source rectangle.",
        )?;

        let row = self.image_row();
        self.place_at(4, row, &contained)?;
        self.place_at(16, row, &stretched)?;
        self.place_at(34, row, &cropped)?;
        self.row = row + 6;
        self.put(
            &self
                .theme
                .paint("   c=10 only      c=16,r=4        + crop", Ink::Accent)
                .clone(),
        )?;
        self.wait()
    }

    /// Step 6: `a=d` and the meaning of the `d` key's case.
    fn deletion(&mut self) -> Result<Advance> {
        self.prose(
            "a=d deletes. The d key says what: d=a everything on screen, d=i one image id, \
             d=p everything intersecting a cell, d=z everything at one z-index, and so on. \
             Case matters — a lowercase d value deletes the placements but keeps the image \
             data, so it can be shown again without retransmitting; uppercase frees the data \
             too.",
        )?;
        self.gap();

        let image = shapes::rings(96, 96)?;
        let mut ids = Vec::new();
        for index in 0..4u32 {
            let id = ID_BASE + 40 + index;
            ids.push(id);
        }

        let sequences: Vec<Vec<Sequence>> = ids
            .iter()
            .map(|&id| {
                proto::transmit(
                    &Transmit::pixels(Format::Rgba, 96, 96)
                        .with_action(Action::TransmitAndDisplay)
                        .with_id(id)
                        .with_quiet(Quiet::Silent),
                    Some(&Placement::default().with_area(Some(8), Some(4))),
                    image.as_bytes(),
                )
            })
            .collect();

        let soft = proto::delete(&Deletion {
            quiet: Quiet::Silent,
            ..Deletion::soft(DeleteTarget::Id {
                image: ids[1],
                placement: None,
            })
        });
        let hard = proto::delete(&Deletion {
            quiet: Quiet::Silent,
            ..Deletion::hard(DeleteTarget::Id {
                image: ids[3],
                placement: None,
            })
        });
        // Two more targets, built and shown but not sent: there is nothing here at
        // that cell or with animation frames, and a delete that hits nothing is not
        // worth watching.
        let at_cell = proto::delete(&Deletion {
            quiet: Quiet::Silent,
            ..Deletion::soft(DeleteTarget::CellAtZ {
                col: 4,
                row: 9,
                z: -1,
            })
        });
        let frames = proto::delete(&Deletion {
            quiet: Quiet::Silent,
            ..Deletion::hard(DeleteTarget::Frames { image: ids[0] })
        });
        let by_number = proto::delete(&Deletion {
            quiet: Quiet::Silent,
            ..Deletion::soft(DeleteTarget::Number {
                number: 13,
                placement: None,
            })
        });

        self.show(&soft)?;
        self.show(&hard)?;
        self.show(&at_cell)?;
        self.show(&frames)?;
        self.show(&by_number)?;
        self.keys(&[
            (
                "d=i",
                "delete by image id, keeping the pixels in the terminal",
            ),
            ("d=I", "the same, but free the image data as well"),
            ("d=q", "everything at one cell *and* one z-index"),
            ("d=F", "an image's animation frames, and their data"),
            ("d=n", "the newest image with an image number, given by I"),
        ])?;
        self.gap();
        self.prose(
            "Four copies go up; the second and fourth are then deleted. Note that d=i without \
             a p key removes every placement of that image, and that a virtual placement — the \
             kind a Unicode placeholder uses — can only be deleted by i, I, n, N, r or R, \
             because it has no position on screen for the others to match.",
        )?;

        let row = self.image_row();
        for (index, chunks) in sequences.iter().enumerate() {
            let col = 4 + u16::try_from(index).unwrap_or(0) * 10;
            self.place_at(col, row, chunks)?;
        }
        self.row = row + 5;
        self.term.flush()?;

        // Show them all, then delete two, so the difference is visible.
        std::thread::sleep(Duration::from_millis(700));
        self.emit(&[soft, hard])?;
        self.put(
            &self
                .theme
                .paint("   kept      deleted      kept      deleted", Ink::Accent)
                .clone(),
        )?;
        self.wait()
    }

    /// Step 7: `a=f` frames, `a=c` composition and `a=a` control.
    //
    // Four frames, a composition and two control commands, each explained where it
    // is built. Splitting it would separate the frames from the animation they add
    // up to.
    #[allow(clippy::too_many_lines)]
    fn animation(&mut self) -> Result<Advance> {
        self.prose(
            "An animation is an ordinary image with frames added to it. a=f transmits frame \
             data, which is composited onto a canvas: either a flat colour from the Y key or \
             the pixels of an earlier frame named by c. Because a frame can cover just a \
             rectangle — x, y with s, v — an animation where one small thing moves costs \
             almost nothing per frame.",
        )?;
        self.gap();

        let size = 96u32;
        let base = shapes::gradient(size, size)?;
        let mut sequences = proto::transmit(
            &Transmit::pixels(Format::Rgba, size, size)
                .with_action(Action::Transmit)
                .with_id(ID_ANIMATED)
                .with_quiet(Quiet::Silent),
            None,
            base.as_bytes(),
        );

        // Each frame paints a small square at a different place, over the base
        // frame's pixels.
        let block = 24u32;
        let dot = Rgba::generate(block, block, |_, _| [0xff, 0xf0, 0x40, 0xff])?;
        let positions = [(8, 8), (56, 8), (56, 56), (8, 56)];
        for (index, (x, y)) in positions.iter().enumerate() {
            let mut transmit = Transmit::pixels(Format::Rgba, block, block)
                .with_action(Action::Frame)
                .with_id(ID_ANIMATED)
                .with_quiet(Quiet::Silent);
            transmit.frame = Some(FrameData {
                at: (*x, *y),
                // The last frame starts from a flat colour instead of the root
                // frame, which is what the Y key is for: a 32-bit RGBA canvas.
                base_frame: if index + 1 == positions.len() {
                    None
                } else {
                    Some(1)
                },
                background: if index + 1 == positions.len() {
                    Some(
                        Colour {
                            red: 0x20,
                            green: 0x10,
                            blue: 0x30,
                        }
                        .to_rgba_u32(),
                    )
                } else {
                    None
                },
                gap: Some(160),
                blend: Blend::Overwrite,
                ..FrameData::default()
            });
            let frame = proto::transmit(&transmit, None, dot.as_bytes());
            if index == 0 {
                self.show(&frame[0])?;
            }
            sequences.extend(frame);
        }

        // The root frame is created with the image data and has no gap of its
        // own, so it has to be set separately.
        let root_gap = Sequence {
            control: format!(
                "{},q=2",
                proto::animate(&Animation::set_gap(ID_ANIMATED, 1, 160)).control
            ),
            payload: String::new(),
        };
        let play = proto::animate(&Animation {
            quiet: Quiet::Silent,
            loops: Some(1),
            ..Animation::play(ID_ANIMATED)
        });
        self.show(&root_gap)?;
        self.show(&play)?;
        // The streaming mode, shown but not used: it is what a client sends when it
        // intends to keep producing frames.
        let streaming = proto::animate(&Animation {
            image_id: ID_ANIMATED,
            state: Some(AnimationState::Loading),
            quiet: Quiet::Silent,
            ..Animation::default()
        });
        self.show(&streaming)?;
        self.keys(&[
            ("a=f", Action::Frame.describe()),
            (
                "x=8,y=8",
                "where in the image this rectangle of pixels lands",
            ),
            ("c=1", "compose it onto frame 1, the root frame"),
            (
                "Y=…",
                "or onto a flat 32-bit RGBA colour, with no base frame",
            ),
            ("z=160", "wait 160ms before the next frame"),
            ("X=1", "overwrite pixels instead of alpha-blending them"),
            ("a=a,s=3", "run the animation, looping at the end"),
            (
                "a=a,s=2",
                "or run it and wait at the end for more frames to arrive",
            ),
            ("v=1", "loop forever (v=n plays n-1 loops)"),
        ])?;
        self.gap();
        self.prose(
            "a=c goes further and composes a rectangle from one frame onto another, so a \
             client can edit frames it has already sent. Once the gaps are set, the animation \
             is the terminal's to run: the client can exit and it keeps going.",
        )?;

        let row = self.image_row();
        let placement = Placement::of_image(ID_ANIMATED)
            .with_area(Some(10), Some(5))
            .with_quiet(Quiet::Silent);
        sequences.push(root_gap);
        sequences.push(play);
        self.place_at(4, row, &sequences)?;
        self.place_at(4, row, &[proto::place(&placement)])?;

        // And one composition, to prove a=c does something.
        let composed = proto::compose(&Composition {
            image_id: ID_ANIMATED,
            source_frame: 1,
            destination_frame: 3,
            size: Some((block, block)),
            source_at: (0, 0),
            destination_at: (36, 36),
            blend: Blend::AlphaBlend,
            quiet: Quiet::Silent,
        });
        self.emit(&[composed])?;
        self.row = row + 6;
        self.wait()
    }

    /// Step 8: `U=1` plus U+10EEEE.
    fn placeholders(&mut self) -> Result<Advance> {
        self.prose(
            "A placement anchored to the cursor is a floating object: it does not scroll, and \
             nothing above the terminal knows it exists. The alternative is to transmit the \
             image, create a virtual placement with U=1, and then display it by printing \
             ordinary text — the character U+10EEEE, once per cell, with the image id in its \
             foreground colour and the row and column in combining diacritics.",
        )?;
        self.gap();

        let image = shapes::color_wheel(160, 160)?;
        let (cols, rows) = (8u16, 4u16);
        let transmit = Transmit::pixels(Format::Rgba, image.width(), image.height())
            .with_action(Action::Transmit)
            .with_id(ID_PLACEHOLDER)
            .with_quiet(Quiet::Silent);
        let mut sequences = proto::transmit(&transmit, None, image.as_bytes());
        let virtual_placement = proto::place(
            &Placement::of_image(ID_PLACEHOLDER)
                .virtualized(cols, rows)
                .with_quiet(Quiet::Silent),
        );
        self.show(&sequences[0])?;
        self.show(&virtual_placement)?;
        self.keys(&[
            ("a=t", "transmit only — do not display anything yet"),
            (
                "a=p,U=1",
                "create a *virtual* placement: an invisible prototype",
            ),
            (
                "c=8,r=4",
                "the size it will be fitted to, which the placeholder cells then fill",
            ),
        ])?;
        self.gap();
        self.put(
            &self
                .theme
                .paint("Then, as text, per cell:", Ink::Accent)
                .clone(),
        )?;
        for row in 0..2usize {
            let described = placeholder::describe_cell(ID_PLACEHOLDER, row, 0);
            self.put(
                &self
                    .theme
                    .paint(&format!("    {described}"), Ink::Code)
                    .clone(),
            )?;
        }
        self.gap();

        sequences.push(virtual_placement);
        self.emit(&sequences)?;

        let row = self.image_row();
        if let Some(lines) = placeholder::block_lines(ID_PLACEHOLDER, None, cols, rows) {
            for (offset, line) in lines.iter().enumerate() {
                let offset = u16::try_from(offset).unwrap_or(0);
                self.term.put(4, row + offset, line)?;
            }
        }
        self.row = row + rows + 1;
        self.prose(
            "Those cells are real text. tmux reflows them, an editor scrolls them, less pages \
             them — and the image follows, because the terminal redraws whichever slice each \
             cell now holds. This is the only way images survive a multiplexer.",
        )?;
        self.gap();

        // A virtual placement has no position on screen, so every position-based
        // delete target misses it. Rather than repeat that list in prose, ask the
        // targets themselves.
        let candidates = [
            DeleteTarget::Visible,
            DeleteTarget::Id {
                image: ID_PLACEHOLDER,
                placement: None,
            },
            DeleteTarget::Number {
                number: 1,
                placement: None,
            },
            DeleteTarget::AtCursor,
            DeleteTarget::Cell { col: 1, row: 1 },
            DeleteTarget::IdRange { first: 1, last: 9 },
            DeleteTarget::Column(1),
            DeleteTarget::Row(1),
            DeleteTarget::ZIndex(0),
        ];
        let reaching: Vec<String> = candidates
            .iter()
            .filter(|target| target.reaches_virtual_placements())
            .map(|target| format!("d={}", target.as_char()))
            .collect();
        self.prose(&format!(
            "A virtual placement has no position on screen, so only the targets that name \
             an image can delete one: {} and their uppercase forms. Everything \
             positional — the cursor, a cell, a row, a z-index — misses it entirely.",
            reaching.join(", ")
        ))?;
        self.wait()
    }

    /// Step 9: an interactive composition, driven by the keyboard and mouse.
    //
    // One long frame loop: setting the scene, then reading events and redrawing.
    // Splitting it would separate the state from the loop that owns it.
    #[allow(clippy::too_many_lines)]
    fn finale(&mut self) -> Result<Advance> {
        self.prose(
            "Everything at once: a backdrop under the text at z=-1, an animation running on \
             the terminal's own clock, and a sprite you can move. Arrow keys move it, a mouse \
             click sends it somewhere, and q or space finishes.",
        )?;
        self.gap();
        self.prose(
            "Moving it re-sends a placement with the same image id and placement id, which \
             replaces the old one rather than adding to it — the protocol's answer to \
             flicker-free movement. Each frame is wrapped in synchronized output, mode 2026, so \
             the terminal presents it all at once.",
        )?;

        let top = self.image_row();
        let bottom = self.term.rows.saturating_sub(3);
        let height = bottom.saturating_sub(top).max(4);
        let width = self.term.cols.saturating_sub(6).max(10);

        // A backdrop under the text.
        let backdrop = shapes::rings(320, 160)?;
        let backdrop_sequences = proto::transmit(
            &Transmit::pixels(Format::Rgba, 320, 160)
                .with_action(Action::TransmitAndDisplay)
                .with_id(ID_BACKDROP)
                .with_quiet(Quiet::Silent),
            Some(
                &Placement::default()
                    .with_area(Some(width), Some(height))
                    .with_z(Some(-2)),
            ),
            backdrop.as_bytes(),
        );
        self.place_at(4, top, &backdrop_sequences)?;

        // An animated sprite, transmitted once and then only ever re-placed.
        let sprite = shapes::color_wheel(64, 64)?;
        let sprite_sequences = proto::transmit(
            &Transmit::pixels(Format::Rgba, 64, 64)
                .with_action(Action::Transmit)
                .with_id(ID_SPRITE)
                .with_quiet(Quiet::Silent),
            None,
            sprite.as_bytes(),
        );
        self.emit(&sprite_sequences)?;
        // Give it a spin, so the sprite is animated as well as movable.
        let mut frames = Vec::new();
        for angle in 1..=4u32 {
            let tint = Rgba::generate(64, 64, |x, y| {
                let wave = u8::try_from((x + y + angle * 16) % 256).unwrap_or(0);
                [wave, 0x40, 0xff - wave, 0x30]
            })?;
            let mut transmit = Transmit::pixels(Format::Rgba, 64, 64)
                .with_action(Action::Frame)
                .with_id(ID_SPRITE)
                .with_quiet(Quiet::Silent);
            transmit.frame = Some(FrameData {
                base_frame: Some(1),
                gap: Some(120),
                ..FrameData::default()
            });
            frames.extend(proto::transmit(&transmit, None, tint.as_bytes()));
        }
        frames.push(proto::animate(&Animation {
            image_id: ID_SPRITE,
            state: Some(AnimationState::Run),
            loops: Some(1),
            quiet: Quiet::Silent,
            ..Animation::default()
        }));
        self.emit(&frames)?;

        self.term.enable_mouse()?;
        let mut col = 4 + width / 2;
        let mut row = top + height / 2;
        let mut moves = 0u32;

        let outcome = loop {
            self.term.begin_sync()?;
            let mut placement = Placement::of_image(ID_SPRITE)
                .with_area(Some(6), Some(3))
                .with_z(Some(2))
                .with_quiet(Quiet::Silent);
            // A placement id makes this *the same* placement every time, so the
            // terminal replaces it instead of stacking copies.
            placement.placement_id = Some(1);
            // And C=1 keeps the cursor where the status line wants it, instead of
            // having it dragged along behind the sprite.
            placement.cursor = CursorMovement::Stay;
            self.term.move_to(col, row)?;
            self.emit(&[proto::place(&placement)])?;
            let status = format!("  cursor {col},{row}   moves {moves}   arrows · click · q  ");
            self.term.put(
                3,
                self.term.rows,
                &self.theme.paint(&status, Ink::Muted).clone(),
            )?;
            self.term.end_sync()?;

            let event = self.input.read(Duration::from_millis(FRAME_MS))?;
            match event {
                Some(Event::Key(key)) => match key.code {
                    KeyCode::Left => {
                        col = col.saturating_sub(2).max(1);
                        moves += 1;
                    }
                    KeyCode::Right => {
                        col = (col + 2).min(self.term.cols.saturating_sub(6));
                        moves += 1;
                    }
                    KeyCode::Up => {
                        row = row.saturating_sub(1).max(top);
                        moves += 1;
                    }
                    KeyCode::Down => {
                        row = (row + 1).min(bottom);
                        moves += 1;
                    }
                    KeyCode::Char('p') => break Advance::Prev,
                    // Finishing and quitting both mean "on to the last step",
                    // which is the one that hands the terminal back.
                    KeyCode::Char(' ' | 'q' | 'Q') | KeyCode::Enter | KeyCode::Esc => {
                        break Advance::Next;
                    }
                    _ => {}
                },
                Some(Event::Mouse(mouse)) => {
                    if matches!(mouse.kind, MouseKind::Down(_) | MouseKind::Drag(_)) {
                        col = mouse.col.clamp(1, self.term.cols.saturating_sub(6));
                        row = mouse.row.clamp(top, bottom);
                        moves += 1;
                    }
                }
                // Nothing happened; loop and redraw so the status line stays live.
                Some(_) | None => {}
            }
        };

        // Stop the animation before the step ends: the images are about to be
        // deleted anyway, but leaving a terminal running an animation it will never
        // show again is untidy.
        let stop = proto::animate(&Animation {
            quiet: Quiet::Silent,
            ..Animation::stop(ID_SPRITE)
        });
        self.emit(&[stop])?;
        self.term.disable_mouse()?;
        Ok(outcome)
    }

    /// Step 10: clean up and hand the terminal back.
    fn farewell(&mut self) -> Result<Advance> {
        self.prose(
            "Last step. Before a full-screen program exits it should delete what it drew: \
             a=d,d=A frees every visible placement and its image data. Leaving the alternate \
             screen clears images there anyway — the protocol requires it — but a program that \
             drew on the main screen has to clean up after itself.",
        )?;
        self.gap();
        let cleanup = proto::delete(&Deletion {
            quiet: Quiet::Silent,
            ..Deletion::hard(DeleteTarget::Visible)
        });
        self.show(&cleanup)?;
        self.keys(&[
            ("a=d", "delete"),
            (
                "d=A",
                "every placement on screen, freeing the image data too",
            ),
        ])?;
        self.gap();
        self.prose("Press space to leave the alternate screen and return to your shell.")?;
        let advance = self.wait()?;

        self.emit(&[cleanup])?;
        self.term.show_cursor()?;
        self.term.leave_alt_screen()?;
        self.term.disable_raw()?;

        println!();
        println!(
            "  {}",
            self.theme
                .paint("That is the kitty graphics protocol.", Ink::Title)
        );
        println!();
        for line in [
            ("escapade graphics explain", "the whole protocol, in text"),
            ("escapade graphics probe", "what your terminal supports"),
            (
                "escapade graphics render <FILE>",
                "images, SVGs and Graphviz diagrams",
            ),
        ] {
            println!(
                "  {}  {}",
                self.theme.paint(line.0, Ink::Code),
                self.theme.paint(line.1, Ink::Muted)
            );
        }
        println!();

        // Whatever the user pressed, this is the end of the walkthrough.
        Ok(match advance {
            Advance::Prev => Advance::Prev,
            _ => Advance::Quit,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_step_has_a_kebab_case_name_and_a_title() {
        for step in STEPS {
            assert!(!step.name.is_empty());
            assert!(!step.title.is_empty());
            assert!(!step.title.contains('\u{1b}'));
            assert!(
                step.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-'),
                "`{}` should be kebab-case so it is easy to type",
                step.name
            );
        }
    }

    #[test]
    fn step_names_are_unique() {
        let mut names = step_names();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two steps share a name");
    }

    #[test]
    fn every_generated_shape_has_a_step_named_after_it() {
        // `--only mandelbrot` and `render --shape mandelbrot` should agree about
        // what that word means.
        for shape in [
            Shape::Gradient,
            Shape::Checkerboard,
            Shape::ColorWheel,
            Shape::Mandelbrot,
            Shape::Rings,
            Shape::TestCard,
        ] {
            let name = shapes::name(shape);
            assert!(step_names().contains(&name), "no demo step called `{name}`");
        }
    }

    #[test]
    fn only_resolves_through_the_shared_selector() {
        let names = step_names();
        let select = |only: Option<&str>, step: Option<usize>| {
            crate::features::select_steps(&names, only, step)
        };
        assert_eq!(select(Some("transmit"), None).unwrap().start, 0);
        assert!(select(Some("transmit"), None).unwrap().single);
        assert_eq!(
            select(Some("mandel"), None).unwrap().start,
            step_names()
                .iter()
                .position(|n| *n == "mandelbrot")
                .unwrap(),
            "an unambiguous prefix is enough"
        );
        assert_eq!(select(None, Some(3)).unwrap().start, 2, "--step is 1-based");
        assert!(!select(None, Some(3)).unwrap().single);
        assert!(select(Some("nonsense"), None).is_err());
    }

    #[test]
    fn the_last_step_is_the_one_that_leaves_the_alternate_screen() {
        let last = &STEPS[STEPS.len() - 1];
        assert!(
            last.title.contains("alternate screen"),
            "the walkthrough must end by handing the terminal back"
        );
        assert_eq!(last.name, "farewell");
    }

    #[test]
    fn listing_the_steps_needs_no_terminal() {
        list_steps(&Theme::plain());
    }

    #[test]
    fn the_demo_image_ids_are_all_distinct_and_placeholder_safe() {
        // The shape steps take up to three consecutive ids from ID_SHAPE, so those
        // are listed as the band they occupy.
        let ids = [
            ID_FIRST,
            ID_RGB,
            ID_RGBA,
            ID_PNG,
            ID_LARGE,
            ID_SHAPE,
            ID_SHAPE + 1,
            ID_SHAPE + 2,
            ID_ANIMATED,
            ID_PLACEHOLDER,
            ID_SPRITE,
            ID_BACKDROP,
        ];
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        let count = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), count, "two steps share an image id");
        for id in ids {
            assert!(
                placeholder::is_encodable(id),
                "id {id} cannot be encoded in a placeholder"
            );
        }
    }

    #[test]
    fn the_refusal_message_comes_from_the_shared_wording() {
        let message = render::unsupported_message("demo");
        assert!(message.starts_with("sorry - this does not seem to be supported"));
        assert!(message.contains("escapade graphics demo --force"));
    }
}
