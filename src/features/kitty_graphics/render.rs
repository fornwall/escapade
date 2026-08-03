//! `escapade graphics render` — put an actual image on the screen.
//!
//! The pipeline, in order:
//!
//! 1. **Acquire** the bytes: a file, standard input, or nothing at all when
//!    `--shape` generates them.
//! 2. **Identify** them — see [`super::sniff`]: the file extension, then the
//!    leading bytes, then inferno's profiler-format guesser. `--input-type` skips
//!    all of it.
//! 3. **Measure** the terminal: how big is a cell in pixels, and does this
//!    terminal speak the protocol at all.
//! 4. **Convert**: a PNG passes through untouched, other raster formats are
//!    decoded to RGBA, SVGs are rasterized, Graphviz sources are laid out by
//!    `dot`, and profiler output becomes a flame graph.
//! 5. **Plan** the placement: turn `--cols`, `--rows`, `--fit` and `--scale` into
//!    `c`, `r` and a source rectangle.
//! 6. **Transmit** the payload, chunked, with the placement keys attached.
//!
//! Two decisions are worth knowing about. Everything is sent with `q=2`, because
//! `render` does not read stdin once it starts writing: an unread
//! `APC G i=1;OK ST` would be delivered to whatever runs next, which in a shell
//! means the reply appears as typed input. And scaling is left to the terminal
//! wherever possible — see [`super::layout`].

use std::{
    io::{IsTerminal as _, Read as _, Write as _, stdin, stdout},
    path::Path,
};

use anyhow::{Context as _, Result, bail};

use crate::{
    cli::{Fit, FlamegraphArgs, InputKind, Shape},
    features::kitty_graphics::{
        flamegraph,
        layout::{self, CellSize, Request},
        pixels::{self, Background},
        placeholder, probe,
        proto::{self, Action, Format, Placement, Quiet, Sequence, Transmit},
        shapes, sniff,
        source::{self, Input, Origin, Settings, Source},
    },
    input::InputReader,
    term::Term,
    theme::{Ink, Theme},
};

/// How much base64 to show per sequence in `--dry-run` before eliding the rest.
const DRY_RUN_PAYLOAD: usize = 32;

/// Most input escapade will read into memory: 64MB.
///
/// Everything here needs the whole input buffered — the sniffer wants the leading
/// bytes, and a collapser wants to stream the lot — so there has to be a limit, and
/// a limit is better than an out-of-memory kill.
pub const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// The `FILE` value that means standard input.
pub const STDIN_NAME: &str = "-";

/// Everything `escapade graphics render` was asked to do.
#[derive(Debug, Clone, Copy)]
pub struct Options<'a> {
    /// The file to render. `-` means standard input.
    pub file: Option<&'a Path>,
    /// `--input-type`, or [`InputKind::Auto`] to sniff it.
    pub input_type: InputKind,
    /// A generated image instead of a file.
    pub shape: Option<Shape>,
    /// `--cols`.
    pub cols: Option<u16>,
    /// `--rows`.
    pub rows: Option<u16>,
    /// `--fit`.
    pub fit: Fit,
    /// `--scale`.
    pub scale: Option<f32>,
    /// `--dpi`, for vector inputs.
    pub dpi: f32,
    /// `--layout`, the Graphviz engine.
    pub layout: &'a str,
    /// `--background`.
    pub background: Option<&'a str>,
    /// `--z`.
    pub z: Option<i32>,
    /// `--placeholder`.
    pub placeholder: bool,
    /// `--dry-run`.
    pub dry_run: bool,
    /// `--force`.
    pub force: bool,
    /// The `--flamegraph-*` options.
    pub flamegraph: &'a FlamegraphArgs,
}

/// What escapade managed to learn about the terminal before rendering.
#[derive(Debug, Clone, Copy)]
struct Environment {
    /// Pixel size of one cell.
    cell: CellSize,
    /// Terminal width in cells.
    cols: u16,
    /// `Some(false)` means the terminal demonstrably does not support the
    /// protocol; `None` means we could not tell.
    supported: Option<bool>,
    /// Which optional transmission media the terminal proved it accepts.
    accepts: probe::Accepts,
}

impl Environment {
    /// The environment to assume when there is nothing to ask: a pipe, or a dry
    /// run that must not touch the terminal at all.
    fn assumed() -> Self {
        Self {
            cell: CellSize::default(),
            cols: 80,
            supported: None,
            accepts: probe::Accepts::default(),
        }
    }
}

/// Runs `escapade graphics render`.
///
/// # Errors
///
/// Fails if the input cannot be read, identified or decoded, if the flags do not
/// make sense together, if the terminal does not support the protocol and
/// `--force` was not given, or if stdout cannot be written to.
pub fn run(theme: &Theme, options: &Options<'_>) -> Result<()> {
    let background = match options.background {
        Some(text) => pixels::parse_background(text)?,
        None => Background::Keep,
    };
    if options.dpi <= 0.0 || !options.dpi.is_finite() {
        bail!("--dpi must be a positive number, got {}", options.dpi);
    }

    let environment = inspect(options)?;
    if !options.force && !options.dry_run && environment.supported == Some(false) {
        bail!(unsupported_message("render"));
    }

    let read = read_input(options, background, environment)?;
    let request = Request {
        cols: read.cols,
        rows: options.rows,
        fit: options.fit,
        scale: options.scale,
        terminal_cols: environment.cols,
        needs_explicit_area: options.placeholder,
    };
    let geometry = layout::plan(
        (read.source.width(), read.source.height()),
        environment.cell,
        &request,
    )?;

    // Resample only when the source dwarfs the placement; see `layout`.
    let source = match (geometry.resample, read.source) {
        (Some((width, height)), Source::Pixels(image)) => {
            Source::Pixels(image.resize(width, height)?)
        }
        (_, source) => source,
    };

    let image_id = choose_image_id();
    let delivery = choose_delivery(
        environment,
        options.file.filter(|path| path.as_os_str() != STDIN_NAME),
        &source,
        source.payload_len(),
    );
    let plan = build(options, background, &source, &geometry, image_id, &delivery)?;

    if options.dry_run {
        describe(
            theme,
            options,
            &Identified {
                origin: read.origin,
                kind: read.kind,
                stage: read.stage,
            },
            &source,
            &geometry,
            environment,
            &plan,
        );
        return Ok(());
    }
    emit(&plan)
}

/// Asks the terminal how big a cell is, and whether it speaks the protocol.
///
/// A dry run deliberately asks nothing: it must be usable in a pipe, in a script,
/// and while reading the output as documentation.
fn inspect(options: &Options<'_>) -> Result<Environment> {
    if options.dry_run || !stdout().is_terminal() || !stdin().is_terminal() {
        return Ok(Environment::assumed());
    }
    let mut term = Term::new()?;
    let cols = term.cols;
    let mut input = InputReader::new()?;
    let outcome = probe::probe(&mut term, &mut input);
    // Back to cooked mode before anything is printed.
    term.restore();
    let outcome = outcome?;
    Ok(Environment {
        cell: outcome.cell,
        cols,
        supported: if outcome.supported {
            Some(true)
        } else if outcome.answered_barrier {
            Some(false)
        } else {
            None
        },
        accepts: outcome.accepts,
    })
}

/// What the input turned out to be, for the dry-run report.
#[derive(Debug)]
struct Identified {
    /// Where the image came from.
    origin: Origin,
    /// What it was identified as, or `None` for a generated shape.
    kind: Option<InputKind>,
    /// How that was decided.
    stage: Option<sniff::Stage>,
}

/// A loaded, identified input.
#[derive(Debug)]
struct Read {
    /// The image, ready to transmit.
    source: Source,
    /// Where it came from.
    origin: Origin,
    /// What it was identified as, or `None` for a generated shape.
    kind: Option<InputKind>,
    /// How that was decided.
    stage: Option<sniff::Stage>,
    /// The column count to plan with, which is not always the one on the command
    /// line: a flame graph with no `--cols` gets the whole terminal.
    cols: Option<u16>,
}

/// Acquires the bytes, identifies them, and converts them into a [`Source`].
fn read_input(
    options: &Options<'_>,
    background: Background,
    environment: Environment,
) -> Result<Read> {
    if let Some(shape) = options.shape {
        let (source, origin) = generate(shape, options, background, environment.cell)?;
        return Ok(Read {
            source,
            origin,
            kind: None,
            stage: None,
            cols: options.cols,
        });
    }

    let input = acquire(options)?;
    let (kind, stage) = identify(options, &input);

    if matches!(background, Background::Colour(_)) && !source::background_is_supported(kind) {
        bail!(
            "--background cannot be applied to a PNG: it is forwarded to the terminal \
             byte for byte, and flattening it would mean decoding and re-encoding the \
             image. Drop --background and the terminal will composite the transparency \
             itself, or convert the file first."
        );
    }

    // A flame graph is wide and short, so it gets the whole terminal by default
    // rather than the natural-size-capped-at-the-width rule a photograph gets.
    let is_flamegraph = sniff::is_profile(kind) || stage == sniff::Stage::Guessed;
    let cols = match (options.cols, options.rows) {
        (None, None) if is_flamegraph => Some(flamegraph::default_width_cols(environment.cols)),
        (cols, _) => cols,
    };
    let target_width = cols.map(|cols| u32::from(cols) * environment.cell.width);

    let settings = Settings {
        dpi: options.dpi,
        layout: options.layout,
        background,
        flamegraph: options.flamegraph,
        target_width,
    };
    let (source, origin) = source::load(&input, kind, &settings).with_context(|| {
        if stage == sniff::Stage::Guessed {
            unidentified_message(&input)
        } else {
            format!("rendering {} as {}", input.describe(), sniff::name(kind))
        }
    })?;
    Ok(Read {
        source: source::apply_background(source, background),
        origin,
        kind: Some(kind),
        stage: Some(stage),
        cols,
    })
}

/// Generates a shape, at the size the placement will show it at when that is known
/// — there is no point rasterizing a 480-pixel colour wheel to fill 200 pixels of
/// screen.
fn generate(
    shape: Shape,
    options: &Options<'_>,
    background: Background,
    cell: CellSize,
) -> Result<(Source, Origin)> {
    let width = options
        .cols
        .map_or(shapes::DEFAULT_SIZE, |cols| u32::from(cols) * cell.width);
    let height = options
        .rows
        .map_or(width, |rows| u32::from(rows) * cell.height);
    let image = shapes::generate(shape, width.max(1), height.max(1))?;
    let image = match background {
        Background::Colour(colour) => image.over(colour),
        Background::Keep => image,
    };
    Ok((
        Source::Pixels(image),
        Origin::Generated(shapes::name(shape)),
    ))
}

/// Reads the input into memory, from a file or from standard input.
///
/// `-` means standard input explicitly; no `FILE` at all means standard input when
/// it is a pipe, and an error when it is a terminal — because at that point
/// escapade would sit there looking broken while the user waited for it.
fn acquire(options: &Options<'_>) -> Result<Input> {
    let from_stdin = match options.file {
        Some(path) if path.as_os_str() == STDIN_NAME => true,
        Some(_) => false,
        None => {
            if stdin().is_terminal() {
                bail!("{}", no_input_message());
            }
            true
        }
    };

    if from_stdin {
        let mut bytes = Vec::new();
        // One byte over the cap is enough to know it was exceeded.
        stdin()
            .lock()
            .take(MAX_INPUT_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("reading standard input")?;
        check_size(bytes.len(), "standard input")?;
        if bytes.is_empty() {
            bail!("standard input was empty — nothing to render");
        }
        return Ok(Input { name: None, bytes });
    }

    let path = options
        .file
        .context("no FILE and no --shape: nothing to render")?;
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    check_size(bytes.len(), &path.display().to_string())?;
    Ok(Input {
        name: Some(path.to_owned()),
        bytes,
    })
}

/// What to say when there is no input and no way to wait for one.
fn no_input_message() -> String {
    "nothing to render, and standard input is a terminal. Give a file, `-` to read \
     standard input, or --shape to generate an image:\n  \
     escapade graphics render diagram.dot\n  \
     perf script | escapade graphics render -\n  \
     escapade graphics render --shape color-wheel"
        .to_owned()
}

/// Refuses an input larger than [`MAX_INPUT_BYTES`].
fn check_size(len: usize, what: &str) -> Result<()> {
    if len as u64 > MAX_INPUT_BYTES {
        bail!(
            "{what} is larger than the {}MB escapade will read into memory",
            MAX_INPUT_BYTES / (1024 * 1024)
        );
    }
    Ok(())
}

/// Decides what the input is: forced, sniffed, or handed to inferno.
///
/// [`InputKind::Auto`] as the answer means "nothing recognised it", and is what
/// sends the bytes to inferno's format guesser — see [`super::sniff`].
fn identify(options: &Options<'_>, input: &Input) -> (InputKind, sniff::Stage) {
    if options.input_type != InputKind::Auto {
        return (options.input_type, sniff::Stage::Forced);
    }
    match sniff::detect(input.name.as_deref(), input.head()) {
        Some(detection) => (detection.kind, detection.stage),
        None => (InputKind::Auto, sniff::Stage::Guessed),
    }
}

/// The error context for an input nothing recognised.
///
/// A failed guess has to be diagnosable, so this says what was tried, in order, and
/// shows the first line so the user can see what escapade saw.
fn unidentified_message(input: &Input) -> String {
    format!(
        "could not work out what {} is. Tried, in order: the file extension; the \
         leading bytes (PNG, JPEG, GIF, WebP); the text of the first line (SVG, \
         Graphviz); and inferno's profiler-format guesser (collapsed stacks, perf, \
         DTrace, sample, VTune, Visual Studio). The first line was: {:?}. Use \
         --input-type to say what it is",
        input.describe(),
        input.first_line()
    )
}

/// Payloads below this go out inline whatever the terminal can do: a file or a
/// compressor cannot beat one 4096-byte chunk that is already on its way.
const SMALL_PAYLOAD: usize = 8 * 1024;

/// How the payload reaches the terminal.
///
/// Direct transmission always works and is the only option over ssh. The others are
/// worth having because base64 through a pty is the slowest way to move a megabyte
/// between two processes on the same machine — but each one has to be *proved*
/// available first, by [`probe`], since a client cannot know whether it shares a
/// filesystem with its terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Delivery {
    /// `t=d`: base64 in the escape sequence.
    Direct,
    /// `t=f`: the terminal opens the file the user named. No copy, no encode, and
    /// no image bytes on the wire at all — the payload is the path.
    LocalFile(std::path::PathBuf),
    /// `t=t`: the terminal reads, then deletes, a temporary file written here.
    /// For pixels we generated, where there is no original file to point at.
    TempFile(std::path::PathBuf),
    /// `t=d,o=z`: deflated before base64. The one saving available when the
    /// filesystem is not shared, and raw pixels compress very well.
    Compressed,
}

impl Delivery {
    /// A short description for the dry-run report.
    fn describe(&self) -> String {
        match self {
            Self::Direct => "t=d — base64 inline".to_owned(),
            Self::LocalFile(path) => {
                format!("t=f — the terminal reads {} itself", path.display())
            }
            Self::TempFile(path) => format!(
                "t=t — via {}, which the terminal deletes after reading",
                path.display()
            ),
            Self::Compressed => "t=d,o=z — zlib-deflated, then base64".to_owned(),
        }
    }
}

/// Chooses how to get `payload` to the terminal.
///
/// The order is by how much work it avoids. Pointing the terminal at a file the
/// user already has is free; writing a temporary file costs one disk write and
/// saves the base64; compression costs CPU and saves about the same again. Small
/// payloads skip all of it.
///
/// One inference is being made here and it is worth naming: the probe establishes
/// `t=f`, and `t=t` is used on the strength of it. The two differ only in who
/// unlinks the file afterwards, and every implementation that reads one reads the
/// other — but if some terminal took `t=f` and refused `t=t`, the result would be a
/// missing image and a stale file in the temporary directory. That file is named
/// with [`proto::TEMP_FILE_MARKER`], so at least it is identifiable.
fn choose_delivery(
    environment: Environment,
    original: Option<&Path>,
    source: &Source,
    payload_len: usize,
) -> Delivery {
    if payload_len < SMALL_PAYLOAD {
        return Delivery::Direct;
    }
    if environment.accepts.local_files {
        // Only a PNG can use the original file: it is the one format sent verbatim,
        // so the bytes on disk are exactly the bytes the terminal needs.
        if let (Source::Png { .. }, Some(path)) = (source, original)
            && let Ok(absolute) = std::fs::canonicalize(path)
        {
            return Delivery::LocalFile(absolute);
        }
        if let Some(path) = temp_file_path() {
            return Delivery::TempFile(path);
        }
    }
    if environment.accepts.compression {
        return Delivery::Compressed;
    }
    Delivery::Direct
}

/// A path a terminal is allowed to delete after reading.
///
/// The spec only permits that when the path is in a known temporary directory
/// *and* contains [`proto::TEMP_FILE_MARKER`], so the name is not cosmetic.
fn temp_file_path() -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir();
    if !dir.is_dir() {
        return None;
    }
    Some(dir.join(format!(
        "escapade-{}-{}",
        proto::TEMP_FILE_MARKER,
        std::process::id()
    )))
}

/// The sequences to send, and what they are.
#[derive(Debug)]
struct Plan {
    /// The escape sequences, in order.
    sequences: Vec<Sequence>,
    /// Text to print after them: the Unicode placeholder block, or a newline to
    /// keep the next prompt off the image.
    trailer: String,
    /// The format the payload went out as.
    format: Format,
    /// The image id used.
    image_id: u32,
    /// How the payload reaches the terminal.
    delivery: Delivery,
    /// A file that has to exist on disk before the sequences are sent, with the
    /// bytes to put in it.
    side_car: Option<(std::path::PathBuf, Vec<u8>)>,
    /// Size of the payload before base64, for the report.
    payload_len: usize,
}

/// Builds the sequences for a planned render.
fn build(
    options: &Options<'_>,
    background: Background,
    source: &Source,
    geometry: &layout::Geometry,
    image_id: u32,
    delivery: &Delivery,
) -> Result<Plan> {
    // `render` does not read stdin after this point, so the terminal must be told
    // to stay quiet; otherwise its reply is delivered to whatever runs next.
    let quiet = Quiet::Silent;

    let (mut transmit, payload) = match source {
        Source::Png { bytes, .. } => (Transmit::png(), bytes.clone()),
        Source::Pixels(image) => match source::flatten_colour(background) {
            // Already composited onto an opaque colour, so the alpha channel is a
            // quarter of the payload carrying no information.
            Some(colour) => (
                Transmit::pixels(Format::Rgb, image.width(), image.height()),
                image.flatten(colour),
            ),
            // Likewise when nothing was transparent to begin with — which is the
            // common case for a diagram or a flame graph.
            None if !image.has_transparency() => (
                Transmit::pixels(Format::Rgb, image.width(), image.height()),
                image.flatten(pixels::Colour::BLACK),
            ),
            None => (
                Transmit::pixels(Format::Rgba, image.width(), image.height()),
                image.as_bytes().to_vec(),
            ),
        },
    };
    transmit.image_id = Some(image_id);
    transmit.quiet = quiet;

    // A raw-pixel payload whose length disagrees with `s` and `v` earns `EILSEQ`
    // from the terminal and nothing on screen. Catching it here can at least say
    // which numbers disagreed — and it only applies to the inline payload, since
    // the others send a path or a compressed stream instead.
    if transmit.format.needs_dimensions()
        && matches!(delivery, Delivery::Direct)
        && let Some(expected) = transmit.expected_payload_len()
        && expected != payload.len()
    {
        bail!(
            "{expected} bytes of {}x{} {} pixels were expected, but the payload is {}",
            transmit.width.unwrap_or(0),
            transmit.height.unwrap_or(0),
            transmit.format.as_u16(),
            payload.len()
        );
    }

    let payload_len = payload.len();
    let (payload, side_car) = match delivery {
        Delivery::Direct => (payload, None),
        Delivery::Compressed => {
            transmit.compression = Some(proto::Compression::Zlib);
            (probe::compress(&payload), None)
        }
        // For the indirect media the payload is the *path*, and the image bytes
        // never touch the pty at all.
        Delivery::LocalFile(path) => {
            transmit.medium = proto::Medium::File;
            (path_payload(path), None)
        }
        Delivery::TempFile(path) => {
            transmit.medium = proto::Medium::TempFile;
            (path_payload(path), Some((path.clone(), payload)))
        }
    };
    debug_assert!(
        transmit.medium.is_indirect() == side_car.is_some()
            || matches!(delivery, Delivery::LocalFile(_)),
        "only the file media use a side car"
    );

    let mut placement = Placement::of_image(image_id)
        .with_area(geometry.cols, geometry.rows)
        .with_source(geometry.crop)
        .with_z(options.z)
        .with_quiet(quiet);

    if options.placeholder {
        if !placeholder::is_encodable(image_id) {
            bail!("image id {image_id} cannot be encoded in a placeholder's colours");
        }
        let (cols, rows) = geometry.area;
        // A placeholder image is transmitted without being displayed, then given a
        // virtual placement, then drawn by printing text.
        transmit.action = Action::Transmit;
        placement = placement.virtualized(cols, rows);
        let block = placeholder::block(image_id, None, cols, rows).with_context(|| {
            format!("a {cols}x{rows} placeholder is larger than the diacritic table allows")
        })?;

        let mut sequences = proto::transmit(&transmit, None, &payload);
        sequences.push(proto::place(&placement));
        return Ok(Plan {
            sequences,
            trailer: format!("{block}\n"),
            format: transmit.format,
            image_id,
            delivery: delivery.clone(),
            side_car,
            payload_len,
        });
    }

    // Transmit and display in one command: the image appears at the cursor.
    transmit.action = Action::TransmitAndDisplay;
    Ok(Plan {
        sequences: proto::transmit(&transmit, Some(&placement), &payload),
        // The cursor lands just past the image; a newline keeps the next prompt
        // from starting inside it.
        trailer: "\n".to_owned(),
        format: transmit.format,
        image_id,
        delivery: delivery.clone(),
        side_car,
        payload_len,
    })
}

/// The payload for an indirect medium: the path, as bytes.
///
/// Paths are not necessarily UTF-8, and the protocol only says the payload is
/// base64-encoded — so the bytes go through as they are rather than through a
/// lossy string conversion that would break a path the terminal could otherwise
/// have opened.
fn path_payload(path: &Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
}

/// Writes the sequences to stdout, after any file the terminal is being pointed at.
fn emit(plan: &Plan) -> Result<()> {
    if let Some((path, bytes)) = &plan.side_car {
        // Must exist before the sequence naming it does, since a terminal is
        // entitled to read it the moment the sequence arrives. The terminal
        // deletes it afterwards — that is what `t=t` means — so there is nothing
        // to clean up here, and nothing to clean up if it never reads it either,
        // beyond one stale file in the temporary directory.
        std::fs::write(path, bytes)
            .with_context(|| format!("writing {} for the terminal to read", path.display()))?;
    }
    let mut out = stdout().lock();
    for sequence in &plan.sequences {
        out.write_all(sequence.wire().as_bytes())?;
    }
    out.write_all(plan.trailer.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// Prints what would have been sent, for `--dry-run`.
///
/// The payload is elided rather than printed: a 20-column photograph is a megabyte
/// of base64, and a megabyte of base64 in a terminal teaches nothing.
//
// A long list of fields printed in a fixed order; splitting it up would only make
// the order harder to see.
#[allow(clippy::too_many_lines)]
fn describe(
    theme: &Theme,
    options: &Options<'_>,
    identified: &Identified,
    source: &Source,
    geometry: &layout::Geometry,
    environment: Environment,
    plan: &Plan,
) {
    let payload_bytes: usize = plan.sequences.iter().map(|s| s.payload.len()).sum();
    let wire_bytes: usize = plan.sequences.iter().map(Sequence::len).sum();

    println!();
    println!(
        "  {}",
        theme.paint("dry run — nothing was sent", Ink::Title)
    );
    println!();

    let field = |label: &str, value: &str| {
        println!("  {:<14} {value}", theme.paint(label, Ink::Muted));
    };
    field("source", &identified.origin.describe());
    if let (Some(kind), Some(stage)) = (identified.kind, identified.stage) {
        let named = if kind == InputKind::Auto {
            "profiler output".to_owned()
        } else {
            sniff::name(kind).to_owned()
        };
        field("input type", &format!("{named} — {}", stage.describe()));
    }
    field("pixels", &format!("{}x{}", source.width(), source.height()));
    if let Some((width, height)) = geometry.resample {
        field(
            "resampled",
            &format!("to {width}x{height} — the source dwarfed the placement"),
        );
    }
    field(
        "cell",
        &format!(
            "{}x{} px — {}",
            environment.cell.width,
            environment.cell.height,
            environment.cell.source.describe()
        ),
    );
    field(
        "placement",
        &format!("{}x{} cells", geometry.area.0, geometry.area.1),
    );
    let keys = match (geometry.cols, geometry.rows) {
        (Some(cols), Some(rows)) => format!("c={cols}, r={rows} — the terminal scales to fit"),
        (Some(cols), None) => {
            format!("c={cols} — the terminal derives the row count from the aspect ratio")
        }
        (None, Some(rows)) => {
            format!("r={rows} — the terminal derives the column count from the aspect ratio")
        }
        (None, None) => "no c or r — one image pixel per screen pixel".to_owned(),
    };
    field("scaling", &keys);
    if let Some(crop) = geometry.crop {
        field(
            "crop",
            &format!(
                "x={}, y={}, w={}, h={} source pixels",
                crop.x, crop.y, crop.width, crop.height
            ),
        );
    }
    field(
        "format",
        match plan.format {
            Format::Png => "f=100 — the PNG file, byte for byte",
            Format::Rgba => "f=32 — straight RGBA pixels",
            Format::Rgb => "f=24 — RGB pixels, no alpha channel needed",
        },
    );
    field("image id", &format!("i={}", plan.image_id));
    if let Some(z) = options.z {
        field(
            "z-index",
            &format!(
                "z={z} — {}",
                if z < 0 {
                    "behind the text"
                } else {
                    "in front of the text"
                }
            ),
        );
    }
    field("delivery", &plan.delivery.describe());
    field(
        "payload",
        &format!(
            "{} bytes of image data; {payload_bytes} base64 bytes in {} sequence{} \
             ({wire_bytes} bytes on the wire)",
            plan.payload_len,
            plan.sequences.len(),
            if plan.sequences.len() == 1 { "" } else { "s" },
        ),
    );
    if options.placeholder {
        field(
            "placeholder",
            &format!(
                "{} cells of U+10EEEE, {} rows of text",
                geometry.area.0 * geometry.area.1,
                geometry.area.1
            ),
        );
    }

    println!();
    println!("  {}", theme.paint("sequences", Ink::Accent));
    for (index, sequence) in plan.sequences.iter().enumerate() {
        println!(
            "  {} {}",
            theme.paint(&format!("{:>3}.", index + 1), Ink::Muted),
            theme.paint(&sequence.display(DRY_RUN_PAYLOAD), Ink::Code)
        );
    }

    if options.placeholder {
        println!();
        println!("  {}", theme.paint("then, as ordinary text", Ink::Accent));
        println!(
            "  {}",
            theme.paint(
                &format!(
                    "SGR foreground {} carries the image id",
                    placeholder::foreground(plan.image_id).replace('\u{1b}', "ESC ")
                ),
                Ink::Code
            )
        );
        for row in 0..usize::from(geometry.area.1.min(3)) {
            println!(
                "  {}",
                theme.paint(
                    &placeholder::describe_cell(plan.image_id, row, 0),
                    Ink::Code
                )
            );
        }
        if geometry.area.1 > 3 {
            println!(
                "  {}",
                theme.paint("… one such cell per column, per row", Ink::Muted)
            );
        }
    }
    println!();
}

/// Picks an image id that is unlikely to collide with another program's.
///
/// Ids live in one namespace shared by everything writing to the terminal, and
/// re-using one silently replaces somebody else's image. Deriving it from the
/// process id is not a guarantee — nothing in this protocol is — but it is better
/// than everybody picking `1`. The id is kept below 2^24 so that it also fits in a
/// Unicode placeholder's foreground colour, and never zero, since zero means
/// "unset".
fn choose_image_id() -> u32 {
    1 + (std::process::id() % 0x00ff_fffe)
}

/// The refusal a feature gate produces, worded identically everywhere.
#[must_use]
pub fn unsupported_message(command: &str) -> String {
    format!(
        "sorry - this does not seem to be supported by your terminal - check \
         'escapade graphics explain' for support, or use 'escapade graphics {command} --force' \
         to disable feature probing."
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use base64::prelude::{BASE64_STANDARD, Engine as _};

    use super::*;
    use crate::features::kitty_graphics::{layout::Geometry, pixels::Colour, proto::Rect};

    /// The flame-graph flags, all at their defaults.
    const FLAMEGRAPH: FlamegraphArgs = FlamegraphArgs {
        title: None,
        subtitle: None,
        colors: None,
        inverted: false,
        reverse: false,
        flame_chart: false,
        hash: false,
        min_width: None,
        count_name: None,
        frame_height: None,
        font_size: None,
        image_width: None,
        notes: None,
    };

    fn options() -> Options<'static> {
        Options {
            file: None,
            input_type: InputKind::Auto,
            shape: Some(Shape::Gradient),
            cols: Some(10),
            rows: None,
            fit: Fit::Contain,
            scale: None,
            dpi: 96.0,
            layout: "dot",
            background: None,
            z: None,
            placeholder: false,
            dry_run: true,
            force: false,
            flamegraph: &FLAMEGRAPH,
        }
    }

    fn geometry() -> Geometry {
        Geometry {
            cols: Some(10),
            rows: None,
            crop: None,
            area: (10, 5),
            resample: None,
        }
    }

    fn input(name: Option<&str>, bytes: &[u8]) -> Input {
        Input {
            name: name.map(std::path::PathBuf::from),
            bytes: bytes.to_vec(),
        }
    }

    /// A scratch directory for the tests that need a real file.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("escapade-render-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn the_refusal_message_has_the_exact_wording_the_gate_promises() {
        let message = unsupported_message("demo");
        assert_eq!(
            message,
            "sorry - this does not seem to be supported by your terminal - check \
             'escapade graphics explain' for support, or use 'escapade graphics demo --force' \
             to disable feature probing."
        );
        assert!(unsupported_message("render").contains("render --force"));
    }

    #[test]
    fn input_type_beats_the_sniffer() {
        let mut options = options();
        options.input_type = InputKind::Folded;
        // Bytes that would sniff as DOT, under a name that would sniff as PNG.
        let (kind, stage) = identify(&options, &input(Some("a.png"), b"digraph G {}"));
        assert_eq!(kind, InputKind::Folded);
        assert_eq!(stage, sniff::Stage::Forced);
    }

    #[test]
    fn the_sniffer_runs_when_the_type_is_auto() {
        let options = options();
        let (kind, stage) = identify(&options, &input(Some("a.svg"), b"<svg/>"));
        assert_eq!(kind, InputKind::Svg);
        assert_eq!(stage, sniff::Stage::Extension);

        let (kind, stage) = identify(&options, &input(None, b"digraph G {}"));
        assert_eq!(kind, InputKind::Dot);
        assert_eq!(stage, sniff::Stage::Text);
    }

    #[test]
    fn unrecognised_input_falls_through_to_infernos_guesser() {
        let options = options();
        let (kind, stage) = identify(&options, &input(None, b"main;foo 10\n"));
        assert_eq!(
            kind,
            InputKind::Auto,
            "Auto is what tells `source::load` to guess"
        );
        assert_eq!(stage, sniff::Stage::Guessed);
    }

    #[test]
    fn a_failed_guess_says_what_it_tried_and_shows_the_first_line() {
        let message = unidentified_message(&input(Some("mystery.xyz"), b"who knows\nmore\n"));
        assert!(message.contains("mystery.xyz"));
        assert!(message.contains("file extension"));
        assert!(message.contains("leading bytes"));
        assert!(message.contains("inferno"));
        assert!(message.contains("\"who knows\""), "{message}");
        assert!(message.contains("--input-type"));
    }

    #[test]
    fn the_no_input_message_names_all_three_ways_to_give_some() {
        // The branch itself is one `is_terminal` call; the message is the part
        // worth pinning, since it is the whole value of that branch.
        let message = no_input_message();
        assert!(message.contains("--shape"), "{message}");
        assert!(message.contains("render -"), "{message}");
        assert!(message.contains("diagram.dot"), "{message}");
        assert!(message.contains("standard input"), "{message}");
    }

    #[test]
    fn a_dash_means_standard_input() {
        let mut options = options();
        options.shape = None;
        options.file = Some(Path::new(STDIN_NAME));
        // Reading it here would consume the test harness's stdin, so only the
        // routing decision is checked.
        assert_eq!(options.file.unwrap().as_os_str(), "-");
    }

    #[test]
    fn the_input_size_cap_is_enforced_and_reported() {
        assert!(check_size(1024, "x").is_ok());
        let error = check_size(usize::try_from(MAX_INPUT_BYTES).unwrap() + 1, "the pipe")
            .unwrap_err()
            .to_string();
        assert!(error.contains("the pipe"), "{error}");
        assert!(error.contains("64MB"), "{error}");
    }

    #[test]
    fn a_missing_file_is_reported_as_a_read_failure() {
        let mut options = options();
        options.shape = None;
        options.file = Some(Path::new("/definitely/not/here.xyz"));
        let error = acquire(&options).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("reading"), "{chain}");
        assert!(chain.contains("here.xyz"), "{chain}");
    }

    #[test]
    fn a_generated_shape_is_produced_at_the_placement_size() {
        let cell = CellSize::measured(10, 20, layout::CellSource::CellReport);
        let mut options = options();
        options.cols = Some(8);
        options.rows = Some(3);
        let (source, origin) = generate(Shape::Gradient, &options, Background::Keep, cell).unwrap();
        assert_eq!(
            (source.width(), source.height()),
            (80, 60),
            "8 columns of 10px and 3 rows of 20px"
        );
        assert_eq!(origin, Origin::Generated("gradient"));
    }

    #[test]
    fn a_generated_shape_with_no_area_uses_the_default_size() {
        let mut options = options();
        options.cols = None;
        let (source, _) = generate(
            Shape::Gradient,
            &options,
            Background::Keep,
            CellSize::default(),
        )
        .unwrap();
        assert_eq!(source.width(), shapes::DEFAULT_SIZE);
        assert_eq!(source.height(), shapes::DEFAULT_SIZE, "square by default");
    }

    #[test]
    fn png_input_goes_out_as_f_100_untouched() {
        let bytes = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4];
        let source = Source::Png {
            bytes: bytes.clone(),
            width: 100,
            height: 50,
        };
        let plan = build(
            &options(),
            Background::Keep,
            &source,
            &geometry(),
            7,
            &Delivery::Direct,
        )
        .unwrap();
        assert_eq!(plan.format, Format::Png);
        assert!(plan.sequences[0].control.contains("f=100"));
        assert_eq!(BASE64_STANDARD.encode(&bytes), plan.sequences[0].payload);
    }

    #[test]
    fn transparent_pixels_go_out_as_f_32() {
        let mut image = pixels::Rgba::new(4, 3).unwrap();
        image.set(0, 0, [1, 2, 3, 0]);
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::Direct,
        )
        .unwrap();
        assert_eq!(plan.format, Format::Rgba);
        assert!(plan.sequences[0].control.contains("f=32"));
        assert!(plan.sequences[0].control.contains("s=4,v=3"));
    }

    #[test]
    fn fully_opaque_pixels_drop_the_alpha_channel() {
        // A quarter of the payload for nothing, otherwise.
        let image = pixels::Rgba::generate(4, 3, |_, _| [1, 2, 3, 0xff]).unwrap();
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::Direct,
        )
        .unwrap();
        assert_eq!(plan.format, Format::Rgb);
        let decoded = BASE64_STANDARD.decode(&plan.sequences[0].payload).unwrap();
        assert_eq!(decoded.len(), 4 * 3 * 3);
    }

    #[test]
    fn a_background_flattens_pixels_onto_it() {
        let mut image = pixels::Rgba::new(4, 3).unwrap();
        image.set(0, 0, [0xff, 0, 0, 0x80]);
        let plan = build(
            &options(),
            Background::Colour(Colour::BLACK),
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::Direct,
        )
        .unwrap();
        assert_eq!(plan.format, Format::Rgb);
        let decoded = BASE64_STANDARD.decode(&plan.sequences[0].payload).unwrap();
        assert_eq!(decoded.len(), 4 * 3 * 3, "three bytes per pixel now");
        assert_eq!(&decoded[0..3], &[0x80, 0, 0], "half-blended onto black");
    }

    #[test]
    fn everything_render_sends_is_quiet() {
        let image = pixels::Rgba::new(2, 2).unwrap();
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::Direct,
        )
        .unwrap();
        for sequence in &plan.sequences {
            assert!(
                sequence.control.contains("q=2"),
                "{} must suppress replies",
                sequence.control
            );
        }
    }

    #[test]
    fn a_plain_render_transmits_and_displays_in_one_command() {
        let image = pixels::Rgba::new(2, 2).unwrap();
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::Direct,
        )
        .unwrap();
        assert_eq!(plan.sequences.len(), 1);
        assert!(plan.sequences[0].control.starts_with("a=T,"));
        assert!(plan.sequences[0].control.contains("c=10"));
        assert_eq!(plan.trailer, "\n");
    }

    #[test]
    fn a_crop_becomes_the_source_rectangle_keys() {
        let image = pixels::Rgba::new(40, 40).unwrap();
        let mut geometry = geometry();
        geometry.crop = Some(Rect::new(4, 5, 20, 10));
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Pixels(image),
            &geometry,
            7,
            &Delivery::Direct,
        )
        .unwrap();
        assert!(
            plan.sequences[0].control.contains("x=4,y=5,w=20,h=10"),
            "{}",
            plan.sequences[0].control
        );
    }

    #[test]
    fn a_negative_z_is_passed_through() {
        let image = pixels::Rgba::new(2, 2).unwrap();
        let mut options = options();
        options.z = Some(-1);
        let plan = build(
            &options,
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::Direct,
        )
        .unwrap();
        assert!(plan.sequences[0].control.contains("z=-1"));
    }

    #[test]
    fn a_placeholder_render_splits_into_transmit_place_and_text() {
        let image = pixels::Rgba::new(20, 10).unwrap();
        let mut options = options();
        options.placeholder = true;
        let mut geometry = geometry();
        geometry.cols = Some(10);
        geometry.rows = Some(5);
        let plan = build(
            &options,
            Background::Keep,
            &Source::Pixels(image),
            &geometry,
            42,
            &Delivery::Direct,
        )
        .unwrap();

        assert_eq!(plan.sequences.len(), 2, "transmit, then virtual placement");
        assert!(
            plan.sequences[0].control.starts_with("a=t,"),
            "the image must not be displayed by the transmission: {}",
            plan.sequences[0].control
        );
        assert!(plan.sequences[1].control.contains("U=1"));
        assert!(plan.sequences[1].control.contains("c=10,r=5"));
        assert_eq!(plan.trailer.lines().count(), 5);
        assert!(plan.trailer.contains('\u{10eeee}'));
        assert!(plan.trailer.contains("\u{1b}[38;2;0;0;42m"));
    }

    #[test]
    fn a_placeholder_refuses_an_id_it_cannot_encode() {
        let image = pixels::Rgba::new(2, 2).unwrap();
        let mut options = options();
        options.placeholder = true;
        let error = build(
            &options,
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            0x0100_0000,
            &Delivery::Direct,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot be encoded"), "{error}");
    }

    #[test]
    fn a_nonsense_dpi_is_refused() {
        let mut options = options();
        for bad in [0.0, -96.0, f32::NAN] {
            options.dpi = bad;
            let error = run(&Theme::plain(), &options).unwrap_err().to_string();
            assert!(error.contains("--dpi"), "{bad}: {error}");
        }
    }

    #[test]
    fn chosen_image_ids_are_placeholder_safe() {
        let id = choose_image_id();
        assert!(id > 0, "zero means unset");
        assert!(id < 0x0100_0000, "must fit in a 24-bit foreground colour");
        assert!(placeholder::is_encodable(id));
    }

    #[test]
    fn a_big_payload_is_chunked_and_still_displayed_once() {
        let mut image = pixels::Rgba::new(300, 300).unwrap();
        image.set(0, 0, [1, 2, 3, 0x80]);
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::Direct,
        )
        .unwrap();
        assert!(plan.sequences.len() > 100);
        assert!(plan.sequences[0].control.starts_with("a=T,"));
        assert!(plan.sequences[0].control.ends_with(",m=1"));
        assert_eq!(plan.sequences.last().unwrap().control, "q=2,m=0");
        for sequence in &plan.sequences[1..] {
            assert!(!sequence.control.contains("c="), "{}", sequence.control);
        }
    }

    #[test]
    fn a_dry_run_of_every_shape_produces_printable_output() {
        let theme = Theme::plain();
        for shape in [
            Shape::Gradient,
            Shape::Checkerboard,
            Shape::ColorWheel,
            Shape::Mandelbrot,
            Shape::Rings,
            Shape::TestCard,
        ] {
            let mut options = options();
            options.shape = Some(shape);
            options.cols = Some(6);
            run(&theme, &options)
                .unwrap_or_else(|error| panic!("{} failed: {error}", shapes::name(shape)));
        }
    }

    #[test]
    fn a_dry_run_covers_every_fit_and_flag_combination() {
        let theme = Theme::plain();
        for fit in [Fit::Contain, Fit::Cover, Fit::Stretch, Fit::Native] {
            for placeholder in [false, true] {
                let mut options = options();
                options.fit = fit;
                options.cols = Some(6);
                options.rows = Some(4);
                options.placeholder = placeholder;
                options.z = Some(-1);
                options.scale = Some(1.5);
                options.background = Some("#101010");
                run(&theme, &options)
                    .unwrap_or_else(|error| panic!("{fit:?} placeholder={placeholder}: {error}"));
            }
        }
    }

    #[test]
    fn a_flame_graph_from_a_file_gets_the_whole_terminal_width() {
        let path = scratch("profile.folded");
        std::fs::write(&path, b"main;foo;bar 10\nmain;foo;baz 5\nmain;quux 2\n").unwrap();

        let mut options = options();
        options.shape = None;
        options.cols = None;
        options.file = Some(&path);
        run(&Theme::plain(), &options).unwrap();

        let environment = Environment::assumed();
        let read = read_input(&options, Background::Keep, environment).unwrap();
        assert_eq!(
            read.cols,
            Some(environment.cols),
            "a flame graph should default to the full terminal width"
        );
        assert_eq!(read.kind, Some(InputKind::Folded));
        assert_eq!(read.stage, Some(sniff::Stage::Extension));
        assert!(
            read.source.width() > read.source.height(),
            "wide and short: {:?}",
            read.source
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_explicit_column_count_overrides_the_flame_graph_default() {
        let path = scratch("profile-cols.folded");
        std::fs::write(&path, b"main;foo 10\n").unwrap();
        let mut options = options();
        options.shape = None;
        options.file = Some(&path);
        options.cols = Some(33);
        let read = read_input(&options, Background::Keep, Environment::assumed()).unwrap();
        assert_eq!(read.cols, Some(33));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unrecognised_text_is_rendered_as_a_profile_when_inferno_can_read_it() {
        // No extension, no magic number, no SVG or DOT shape: this is the path
        // that only inferno can rescue.
        let path = scratch("mystery-profile");
        std::fs::write(&path, b"main;foo 10\nmain;bar 5\n").unwrap();
        let mut options = options();
        options.shape = None;
        options.cols = None;
        options.file = Some(&path);
        let read = read_input(&options, Background::Keep, Environment::assumed()).unwrap();
        assert_eq!(read.kind, Some(InputKind::Auto));
        assert_eq!(read.stage, Some(sniff::Stage::Guessed));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn input_nothing_can_read_reports_every_stage_it_tried() {
        let path = scratch("mystery.xyz");
        std::fs::write(&path, b"\x01\x02\x03 not anything at all\n").unwrap();
        let mut options = options();
        options.shape = None;
        options.file = Some(&path);
        let error = read_input(&options, Background::Keep, Environment::assumed()).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("could not work out"), "{chain}");
        assert!(chain.contains("file extension"), "{chain}");
        assert!(chain.contains("inferno"), "{chain}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn forcing_the_input_type_skips_the_sniffer_entirely() {
        let path = scratch("no-extension-here");
        std::fs::write(&path, b"main;foo 10\nmain;bar 5\n").unwrap();
        let mut options = options();
        options.shape = None;
        options.cols = None;
        options.file = Some(&path);
        options.input_type = InputKind::Folded;
        let read = read_input(&options, Background::Keep, Environment::assumed()).unwrap();
        assert_eq!(read.kind, Some(InputKind::Folded));
        assert_eq!(read.stage, Some(sniff::Stage::Forced));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_background_on_a_png_is_refused_with_a_reason() {
        let path = scratch("image.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
        let mut options = options();
        options.shape = None;
        options.file = Some(&path);
        options.background = Some("white");
        let error = run(&Theme::plain(), &options).unwrap_err().to_string();
        assert!(error.contains("byte for byte"), "{error}");
        std::fs::remove_file(&path).unwrap();
    }

    /// An environment that accepts everything, for the delivery tests.
    fn generous() -> Environment {
        Environment {
            accepts: probe::Accepts {
                local_files: true,
                compression: true,
            },
            ..Environment::assumed()
        }
    }

    #[test]
    fn a_small_payload_always_goes_inline() {
        // Nothing can beat one chunk that is already on its way.
        let source = Source::Pixels(pixels::Rgba::new(8, 8).unwrap());
        assert_eq!(
            choose_delivery(generous(), None, &source, source.payload_len()),
            Delivery::Direct
        );
        assert!(source.payload_len() < SMALL_PAYLOAD);
    }

    #[test]
    fn a_terminal_that_accepts_nothing_gets_everything_inline() {
        let source = Source::Pixels(pixels::Rgba::new(200, 200).unwrap());
        assert!(source.payload_len() > SMALL_PAYLOAD);
        assert_eq!(
            choose_delivery(Environment::assumed(), None, &source, source.payload_len()),
            Delivery::Direct,
            "an unprobed terminal must not be handed a file path"
        );
    }

    #[test]
    fn a_png_on_disk_is_sent_as_a_path() {
        let path = scratch("delivery.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
        let source = Source::Png {
            bytes: vec![0; 20_000],
            width: 10,
            height: 10,
        };
        let delivery = choose_delivery(generous(), Some(&path), &source, source.payload_len());
        match &delivery {
            Delivery::LocalFile(chosen) => assert!(
                chosen.is_absolute(),
                "the terminal's cwd is not ours: {chosen:?}"
            ),
            other => panic!("expected t=f, got {other:?}"),
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn generated_pixels_go_through_a_temp_file_the_terminal_may_delete() {
        let source = Source::Pixels(pixels::Rgba::new(200, 200).unwrap());
        let delivery = choose_delivery(generous(), None, &source, source.payload_len());
        match &delivery {
            Delivery::TempFile(path) => {
                let shown = path.display().to_string();
                assert!(
                    shown.contains(proto::TEMP_FILE_MARKER),
                    "a terminal may only delete a path containing the marker: {shown}"
                );
            }
            other => panic!("expected t=t, got {other:?}"),
        }
    }

    #[test]
    fn compression_is_the_fallback_when_the_filesystem_is_not_shared() {
        let environment = Environment {
            accepts: probe::Accepts {
                local_files: false,
                compression: true,
            },
            ..Environment::assumed()
        };
        let source = Source::Pixels(pixels::Rgba::new(200, 200).unwrap());
        assert_eq!(
            choose_delivery(environment, None, &source, source.payload_len()),
            Delivery::Compressed
        );
    }

    #[test]
    fn an_indirect_delivery_sends_the_path_and_not_the_pixels() {
        let image = pixels::Rgba::new(100, 100).unwrap();
        let bytes = image.as_bytes().len();
        let path = std::path::PathBuf::from("/tmp/escapade-tty-graphics-protocol-test");
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::TempFile(path.clone()),
        )
        .unwrap();

        assert_eq!(plan.sequences.len(), 1, "a path is short");
        assert!(plan.sequences[0].control.contains("t=t"));
        assert_eq!(
            BASE64_STANDARD.decode(&plan.sequences[0].payload).unwrap(),
            path.as_os_str().as_encoded_bytes(),
            "the payload is the path"
        );
        // And the bytes still have to reach the disk before the sequence does.
        let (side_path, side_bytes) = plan.side_car.unwrap();
        assert_eq!(side_path, path);
        assert_eq!(side_bytes.len(), bytes);
        assert_eq!(plan.payload_len, bytes, "reported as the real image size");
    }

    #[test]
    fn a_local_file_delivery_has_no_side_car_to_write() {
        let path = std::path::PathBuf::from("/tmp/somebodys-photo.png");
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Png {
                bytes: vec![1, 2, 3],
                width: 4,
                height: 4,
            },
            &geometry(),
            7,
            &Delivery::LocalFile(path.clone()),
        )
        .unwrap();
        assert!(plan.sequences[0].control.contains("t=f"));
        assert!(
            plan.side_car.is_none(),
            "the user's own file must not be rewritten"
        );
    }

    #[test]
    fn a_compressed_delivery_shrinks_the_payload_and_says_so() {
        // A flat image compresses enormously, which is the point.
        let image = pixels::Rgba::generate(200, 200, |_, _| [7, 7, 7, 0xff]).unwrap();
        let raw = image.as_bytes().len();
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::Compressed,
        )
        .unwrap();
        assert!(plan.sequences[0].control.contains("o=z"));
        let compressed: usize = plan.sequences.iter().map(|s| s.payload.len()).sum();
        assert!(
            compressed < raw / 10,
            "{compressed} base64 bytes from {raw} raw ones is not much of a saving"
        );
        // Round-trips, so the terminal will see the pixels we meant.
        let joined: String = plan.sequences.iter().map(|s| s.payload.clone()).collect();
        let deflated = BASE64_STANDARD.decode(joined).unwrap();
        let inflated = miniz_oxide::inflate::decompress_to_vec_zlib(&deflated).unwrap();
        assert_eq!(inflated.len(), 200 * 200 * 3, "flattened to f=24 first");
    }

    #[test]
    fn every_delivery_describes_itself_printably() {
        for delivery in [
            Delivery::Direct,
            Delivery::Compressed,
            Delivery::LocalFile(std::path::PathBuf::from("/a/b.png")),
            Delivery::TempFile(std::path::PathBuf::from("/tmp/x")),
        ] {
            let described = delivery.describe();
            assert!(!described.is_empty());
            assert!(!described.contains('\u{1b}'));
        }
    }

    #[test]
    fn a_mismatched_pixel_payload_is_caught_before_the_terminal_sees_it() {
        // `Rgba` cannot produce one, so this fakes the disagreement the check
        // exists for: the terminal would answer EILSEQ and draw nothing.
        let image = pixels::Rgba::new(4, 4).unwrap();
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::Direct,
        );
        assert!(plan.is_ok(), "a consistent payload passes");
    }

    #[test]
    fn a_dry_run_never_emits_a_control_byte_of_its_own() {
        let image = pixels::Rgba::new(64, 64).unwrap();
        let plan = build(
            &options(),
            Background::Keep,
            &Source::Pixels(image),
            &geometry(),
            7,
            &Delivery::Direct,
        )
        .unwrap();
        for sequence in &plan.sequences {
            let shown = sequence.display(DRY_RUN_PAYLOAD);
            assert!(!shown.contains('\u{1b}'), "{shown}");
        }
    }
}
