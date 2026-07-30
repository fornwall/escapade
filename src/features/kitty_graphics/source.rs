//! Turning an input into something the protocol can carry.
//!
//! There are two destinations. A PNG becomes `f=100` and travels **untouched** —
//! decoding it just to re-encode it would cost time and throw away whatever the
//! encoder knew about the image. Everything else becomes `f=32` straight RGBA,
//! because a terminal is only required to understand RGB, RGBA and PNG, and
//! teaching it JPEG is not our business.
//!
//! What an input *is* has already been decided by [`super::sniff`] before anything
//! here runs, so this module is a dispatch table on [`InputKind`] and nothing more
//! clever than that. Everything works from bytes rather than paths, because the
//! input may have arrived on standard input and have no path at all.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context as _, Result, bail};

use crate::{
    cli::{FlamegraphArgs, InputKind},
    features::kitty_graphics::{
        flamegraph,
        pixels::{Background, Colour, Rgba},
        png,
    },
};

/// Refuses a rasterization big enough to exhaust memory, whatever `--dpi` says.
const MAX_RASTER_PIXELS: u64 = 64_000_000;

/// The bytes to render, and the name they came from.
///
/// `name` is `None` for standard input, which is exactly why the sniffer exists.
#[derive(Clone, PartialEq, Eq)]
pub struct Input {
    /// The path the bytes were read from, or `None` for standard input.
    pub name: Option<PathBuf>,
    /// The bytes themselves.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for Input {
    /// Prints the name and the size, never the bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Input")
            .field("name", &self.name)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl Input {
    /// The leading bytes, for the sniffer.
    #[must_use]
    pub fn head(&self) -> &[u8] {
        let end = self.bytes.len().min(super::sniff::HEAD_LEN);
        &self.bytes[..end]
    }

    /// How to refer to this input in a message.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.name {
            Some(path) => path.display().to_string(),
            None => "standard input".to_owned(),
        }
    }

    /// The first line of the input, for an error message about a failed guess.
    #[must_use]
    pub fn first_line(&self) -> String {
        let head = String::from_utf8_lossy(self.head());
        let line = head.lines().next().unwrap_or_default().trim();
        if line.chars().count() > 72 {
            let truncated: String = line.chars().take(72).collect();
            format!("{truncated}…")
        } else {
            line.to_owned()
        }
    }
}

/// Where an image came from, for the one-line summary `render` prints.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    /// A PNG, forwarded verbatim.
    Png(Option<PathBuf>),
    /// A raster file decoded by the `image` crate.
    Raster {
        /// The file, or `None` for standard input.
        name: Option<PathBuf>,
        /// Format name, as `image` identified it.
        format: String,
    },
    /// An SVG rasterized by resvg.
    Svg {
        /// The file, or `None` for standard input.
        name: Option<PathBuf>,
        /// The resolution it was rasterized at.
        dpi: f32,
    },
    /// A Graphviz source laid out by `dot` and rendered to PNG.
    Graphviz {
        /// The file, or `None` for standard input.
        name: Option<PathBuf>,
        /// The layout engine used.
        engine: String,
    },
    /// Profiler output collapsed and drawn as a flame graph.
    Flamegraph {
        /// The file, or `None` for standard input.
        name: Option<PathBuf>,
        /// The profile format it was read as.
        kind: InputKind,
        /// How many collapsed stacks went into it.
        stacks: usize,
    },
    /// A shape generated from arithmetic.
    Generated(&'static str),
}

impl Origin {
    /// One line naming the input and how it was read.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Png(name) => format!("{} (PNG, sent as-is)", show(name.as_ref())),
            Self::Raster { name, format } => {
                format!("{} ({format}, decoded to RGBA)", show(name.as_ref()))
            }
            Self::Svg { name, dpi } => format!("{} (SVG, rasterized at {dpi} dpi)", show(name.as_ref())),
            Self::Graphviz { name, engine } => {
                format!("{} (Graphviz, laid out with {engine})", show(name.as_ref()))
            }
            Self::Flamegraph { name, kind, stacks } => {
                // `Auto` here means the guesser worked it out, so name it that way
                // rather than calling it an "auto profile".
                let format = if *kind == InputKind::Auto {
                    "profiler output".to_owned()
                } else {
                    format!("{} output", super::sniff::name(*kind))
                };
                format!(
                    "{} ({format}, {stacks} collapsed stack{}, drawn as a flame graph)",
                    show(name.as_ref()),
                    if *stacks == 1 { "" } else { "s" }
                )
            }
            Self::Generated(name) => format!("--shape {name} (generated)"),
        }
    }
}

/// Renders an optional path for a message.
fn show(name: Option<&PathBuf>) -> String {
    match name {
        Some(path) => path.display().to_string(),
        None => "standard input".to_owned(),
    }
}

/// An image ready to be transmitted.
#[derive(Debug)]
pub enum Source {
    /// PNG bytes for `f=100`. The dimensions come from the IHDR chunk.
    Png {
        /// The file's bytes, exactly as they were on disk.
        bytes: Vec<u8>,
        /// Width from IHDR.
        width: u32,
        /// Height from IHDR.
        height: u32,
    },
    /// Straight RGBA pixels for `f=32`.
    Pixels(Rgba),
}

impl Source {
    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        match self {
            Self::Png { width, .. } => *width,
            Self::Pixels(image) => image.width(),
        }
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        match self {
            Self::Png { height, .. } => *height,
            Self::Pixels(image) => image.height(),
        }
    }

    /// Size of the payload that will go on the wire, before base64.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        match self {
            Self::Png { bytes, .. } => bytes.len(),
            Self::Pixels(image) => image.as_bytes().len(),
        }
    }
}

/// The layout engines `dot -K` accepts, for a helpful error message.
pub const GRAPHVIZ_ENGINES: [&str; 8] = [
    "dot", "neato", "twopi", "circo", "fdp", "sfdp", "osage", "patchwork",
];

/// Everything the pipelines need beyond the bytes themselves.
#[derive(Debug, Clone, Copy)]
pub struct Settings<'a> {
    /// `--dpi`, for vector inputs.
    pub dpi: f32,
    /// `--layout`, the Graphviz engine.
    pub layout: &'a str,
    /// `--background`.
    pub background: Background,
    /// Flame-graph options.
    pub flamegraph: &'a FlamegraphArgs,
    /// The pixel width the placement will occupy, when it is known. Used to
    /// generate a flame graph at the width it will actually be shown at.
    pub target_width: Option<u32>,
}

/// Loads `input`, which has already been identified as `kind`.
///
/// `background` is honoured by the pipelines that can do it without a decode
/// step: SVG and flame graphs fill the canvas before rasterizing, and Graphviz is
/// told to draw its own background. A PNG is forwarded byte for byte, so there is
/// nothing to composite onto — the caller is expected to have rejected that
/// combination.
///
/// # Errors
///
/// Fails if a decoder rejects the input, if `dot` is missing or fails, or if the
/// profile cannot be collapsed.
pub fn load(input: &Input, kind: InputKind, settings: &Settings<'_>) -> Result<(Source, Origin)> {
    if input.bytes.is_empty() {
        bail!("{} is empty", input.describe());
    }
    match kind {
        InputKind::Png => load_png(input),
        InputKind::Jpeg | InputKind::Gif | InputKind::Webp => decode_raster(input),
        InputKind::Svg => load_svg(input, settings),
        InputKind::Dot => load_graphviz(input, settings),
        InputKind::Folded
        | InputKind::Perf
        | InputKind::Dtrace
        | InputKind::Sample
        | InputKind::Vtune
        | InputKind::Vsprof
        | InputKind::Auto => load_flamegraph(input, kind, settings),
    }
}

/// Takes a PNG's dimensions from its IHDR chunk and forwards the bytes.
fn load_png(input: &Input) -> Result<(Source, Origin)> {
    if !png::is_png(&input.bytes) {
        bail!(
            "{} does not start with the PNG signature — was --input-type png right?",
            input.describe()
        );
    }
    let (width, height) = png::dimensions(&input.bytes)
        .with_context(|| format!("reading the dimensions of {}", input.describe()))?;
    Ok((
        Source::Png {
            bytes: input.bytes.clone(),
            width,
            height,
        },
        Origin::Png(input.name.clone()),
    ))
}

/// Decodes a JPEG, GIF or WebP to RGBA.
///
/// GIFs decode to their first frame. Turning an animated GIF into a protocol
/// animation would mean transmitting every frame with `a=f`, which the demo shows
/// but `render` deliberately does not do: a file preview should not start playing.
fn decode_raster(input: &Input) -> Result<(Source, Origin)> {
    // A mislabelled PNG still takes the fast path: it is both faster and lossless.
    if png::is_png(&input.bytes) {
        return load_png(input);
    }
    let format = image::guess_format(&input.bytes)
        .with_context(|| format!("recognising the image format of {}", input.describe()))?;
    let decoded = image::load_from_memory_with_format(&input.bytes, format)
        .with_context(|| format!("decoding {}", input.describe()))?;
    let rgba = decoded.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    let image = Rgba::from_raw(width, height, rgba.into_raw())?;
    Ok((
        Source::Pixels(image),
        Origin::Raster {
            name: input.name.clone(),
            format: format!("{format:?}"),
        },
    ))
}

/// Rasterizes an SVG with resvg.
fn load_svg(input: &Input, settings: &Settings<'_>) -> Result<(Source, Origin)> {
    let resources = input
        .name
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    let image = rasterize_svg(&input.bytes, resources, settings.dpi, settings.background)
        .with_context(|| format!("rasterizing {}", input.describe()))?;
    Ok((
        Source::Pixels(image),
        Origin::Svg {
            name: input.name.clone(),
            dpi: settings.dpi,
        },
    ))
}

/// Rasterizes SVG data at `dpi`, optionally over a background colour.
///
/// `dpi` scales the output: the SVG's own user units are taken as 96 per inch —
/// the CSS reference pixel — so `--dpi 192` produces twice as many pixels in each
/// direction. That is the knob that matters on a HiDPI screen, where a diagram
/// rasterized at 96 dpi and then enlarged by the terminal looks soft.
///
/// System fonts are loaded, so `<text>` elements render. Without that, an SVG with
/// labels comes out blank where the labels should be — a silent and very confusing
/// failure, and one that would gut a flame graph in particular.
///
/// Anything resvg cannot handle — the JavaScript and hover behaviour in inferno's
/// output, for instance — is skipped. resvg reports those through the `log` crate,
/// and since escapade installs no logger they go nowhere, which is what we want:
/// they are not the user's problem.
///
/// # Errors
///
/// Fails if the data is not SVG, or if the requested resolution is absurd.
pub fn rasterize_svg(
    data: &[u8],
    resources_dir: Option<PathBuf>,
    dpi: f32,
    background: Background,
) -> Result<Rgba> {
    let mut options = resvg::usvg::Options {
        dpi,
        resources_dir,
        ..resvg::usvg::Options::default()
    };
    options.fontdb_mut().load_system_fonts();

    let tree = resvg::usvg::Tree::from_data(data, &options).context("parsing the data as SVG")?;

    let scale = dpi / 96.0;
    let size = tree
        .size()
        .to_int_size()
        .scale_by(scale)
        .with_context(|| format!("scaling by {scale} produced an empty image"))?;
    guard_raster_size(size.width(), size.height())?;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .context("allocating the SVG raster buffer")?;
    if let Background::Colour(colour) = background {
        pixmap.fill(resvg::tiny_skia::Color::from_rgba8(
            colour.red,
            colour.green,
            colour.blue,
            0xff,
        ));
    }
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    from_pixmap(&pixmap)
}

/// Converts a tiny-skia pixmap to straight RGBA.
///
/// tiny-skia stores premultiplied alpha; the protocol's `f=32` wants straight
/// alpha, so every pixel is demultiplied on the way out. Skipping this step gives
/// an image that looks right on an opaque background and wrong on everything else.
fn from_pixmap(pixmap: &resvg::tiny_skia::Pixmap) -> Result<Rgba> {
    let mut data = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        let colour = pixel.demultiply();
        data.extend_from_slice(&[colour.red(), colour.green(), colour.blue(), colour.alpha()]);
    }
    Rgba::from_raw(pixmap.width(), pixmap.height(), data)
}

/// Refuses an absurd raster size before allocating it.
fn guard_raster_size(width: u32, height: u32) -> Result<()> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_RASTER_PIXELS {
        bail!("rasterizing at that resolution would need {width}x{height} pixels; lower --dpi");
    }
    Ok(())
}

/// Lays out a Graphviz source with `dot` and renders it to PNG.
///
/// Shelling out is the right call here rather than linking a Rust port: Graphviz
/// *is* the layout engine, `dot` is on every machine that has diagrams to draw,
/// and its `-K` engines are the reason to reach for it in the first place. The
/// source goes in on `dot`'s standard input, which works whether it came from a
/// file or a pipe, and the output is PNG — so it goes straight onto the `f=100`
/// fast path and no pixels are ever decoded on this route.
fn load_graphviz(input: &Input, settings: &Settings<'_>) -> Result<(Source, Origin)> {
    let engine = settings.layout;
    let mut command = Command::new("dot");
    command
        .arg(format!("-K{engine}"))
        .arg("-Tpng")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Background::Colour(colour) = settings.background {
        // Let Graphviz paint its own background, which keeps the result a PNG we
        // never have to decode.
        command.arg(format!("-Gbgcolor={}", colour.to_hex()));
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
            "`dot` was not found on PATH. Rendering Graphviz sources needs Graphviz:\n  \
             Debian/Ubuntu: apt install graphviz\n  \
             Fedora:        dnf install graphviz\n  \
             Arch:          pacman -S graphviz\n  \
             macOS:         brew install graphviz"
        ),
        Err(error) => return Err(error).context("running `dot`"),
    };

    // The pipe has to be closed before waiting, or `dot` never sees end of input.
    {
        let stdin = child
            .stdin
            .as_mut()
            .context("`dot` did not provide a standard input")?;
        stdin
            .write_all(&input.bytes)
            .context("writing the graph to `dot`")?;
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().context("waiting for `dot`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let hint = if GRAPHVIZ_ENGINES.contains(&engine) {
            String::new()
        } else {
            format!(
                "\n`{engine}` is not a layout engine escapade knows about; try one of: {}",
                GRAPHVIZ_ENGINES.join(", ")
            )
        };
        let detail = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        bail!("`dot -K{engine} -Tpng` failed{detail}{hint}");
    }

    let bytes = output.stdout;
    let (width, height) = png::dimensions(&bytes)
        .context("`dot` did not produce a PNG — does this build support -Tpng?")?;
    Ok((
        Source::Png {
            bytes,
            width,
            height,
        },
        Origin::Graphviz {
            name: input.name.clone(),
            engine: engine.to_owned(),
        },
    ))
}

/// Collapses profiler output, draws it as a flame graph, and rasterizes that.
fn load_flamegraph(
    input: &Input,
    kind: InputKind,
    settings: &Settings<'_>,
) -> Result<(Source, Origin)> {
    // When the kind was guessed rather than known, `render` has already wrapped
    // this in a message naming every stage it tried, so adding another layer here
    // would only make the chain longer without saying anything new.
    let collapsed = if kind == InputKind::Auto {
        flamegraph::collapse(&input.bytes, kind)?
    } else {
        flamegraph::collapse(&input.bytes, kind).with_context(|| {
            format!(
                "reading {} as {} output",
                input.describe(),
                super::sniff::name(kind)
            )
        })?
    };
    let stacks = collapsed.lines().filter(|line| !line.trim().is_empty()).count();
    let svg = flamegraph::to_svg(&collapsed, settings.flamegraph, settings.target_width)?;
    // A flame graph has no transparent regions worth keeping, and inferno draws
    // its own background, so `--background` only shows through at the edges.
    let image = rasterize_svg(&svg, None, settings.dpi, settings.background)
        .context("rasterizing the flame graph")?;
    Ok((
        Source::Pixels(image),
        Origin::Flamegraph {
            name: input.name.clone(),
            kind,
            stacks,
        },
    ))
}

/// Whether `--background` can be honoured for an input of this kind.
///
/// A PNG is forwarded byte for byte, so there is nothing to composite onto without
/// decoding it — which is exactly what the fast path exists to avoid.
#[must_use]
pub fn background_is_supported(kind: InputKind) -> bool {
    kind != InputKind::Png
}

/// Applies a background to already-decoded pixels, if one was asked for.
#[must_use]
pub fn apply_background(source: Source, background: Background) -> Source {
    match (source, background) {
        (Source::Pixels(image), Background::Colour(colour)) => Source::Pixels(image.over(colour)),
        (source, _) => source,
    }
}

/// The colour a flattened image should end up on, given `--background`.
#[must_use]
pub fn flatten_colour(background: Background) -> Option<Colour> {
    match background {
        Background::Keep => None,
        Background::Colour(colour) => Some(colour),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The flame-graph flags, all at their defaults.
    fn flamegraph_args() -> FlamegraphArgs {
        FlamegraphArgs {
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
        }
    }

    fn settings(flamegraph: &FlamegraphArgs, background: Background) -> Settings<'_> {
        Settings {
            dpi: 96.0,
            layout: "dot",
            background,
            flamegraph,
            target_width: None,
        }
    }

    fn input(name: Option<&str>, bytes: &[u8]) -> Input {
        Input {
            name: name.map(PathBuf::from),
            bytes: bytes.to_vec(),
        }
    }

    /// The smallest valid PNG: signature, IHDR, a one-pixel IDAT, IEND.
    fn minimal_png() -> Vec<u8> {
        let mut bytes = png::SIGNATURE.to_vec();
        let chunk = |kind: &[u8; 4], data: &[u8]| -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            // The CRC is never checked by escapade, and this PNG never leaves the
            // test that made it.
            out.extend_from_slice(&[0, 0, 0, 0]);
            out
        };
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&chunk(b"IHDR", &ihdr));
        bytes.extend_from_slice(&chunk(b"IDAT", &[0x78, 0x9c, 0x63, 0x00, 0x00]));
        bytes.extend_from_slice(&chunk(b"IEND", &[]));
        bytes
    }

    #[test]
    fn origins_describe_themselves_without_panicking() {
        let origins = [
            Origin::Png(Some(PathBuf::from("a.png"))),
            Origin::Png(None),
            Origin::Raster {
                name: Some(PathBuf::from("a.jpg")),
                format: "Jpeg".to_owned(),
            },
            Origin::Svg {
                name: Some(PathBuf::from("a.svg")),
                dpi: 192.0,
            },
            Origin::Graphviz {
                name: None,
                engine: "neato".to_owned(),
            },
            Origin::Flamegraph {
                name: None,
                kind: InputKind::Perf,
                stacks: 12,
            },
            Origin::Generated("rings"),
        ];
        for origin in &origins {
            let described = origin.describe();
            assert!(!described.is_empty());
            assert!(!described.contains('\u{1b}'));
        }
        assert!(origins[1].describe().contains("standard input"));
        assert!(origins[3].describe().contains("192"));
        assert!(origins[4].describe().contains("neato"));
        assert!(origins[5].describe().contains("12 collapsed stacks"));
    }

    #[test]
    fn a_png_is_forwarded_byte_for_byte() {
        let png = minimal_png();
        let args = flamegraph_args();
        let (source, origin) = load(
            &input(Some("a.png"), &png),
            InputKind::Png,
            &settings(&args, Background::Keep),
        )
        .unwrap();
        assert!(matches!(origin, Origin::Png(_)));
        match source {
            Source::Png {
                bytes,
                width,
                height,
            } => {
                assert_eq!(bytes, png, "the bytes must not be touched");
                assert_eq!((width, height), (1, 1));
            }
            Source::Pixels(_) => panic!("a PNG must not be decoded"),
        }
    }

    #[test]
    fn a_mislabelled_png_still_takes_the_fast_path() {
        let png = minimal_png();
        let args = flamegraph_args();
        // Sniffed as JPEG by a wrong extension, but the bytes say otherwise.
        let (source, origin) = load(
            &input(Some("a.jpg"), &png),
            InputKind::Jpeg,
            &settings(&args, Background::Keep),
        )
        .unwrap();
        assert!(matches!(origin, Origin::Png(_)));
        assert!(matches!(source, Source::Png { .. }));
    }

    #[test]
    fn forcing_png_on_something_else_says_so() {
        let args = flamegraph_args();
        let error = load(
            &input(None, b"not a png at all"),
            InputKind::Png,
            &settings(&args, Background::Keep),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("PNG signature"), "{error}");
        assert!(error.contains("--input-type png"), "{error}");
    }

    #[test]
    fn an_empty_input_is_refused() {
        let args = flamegraph_args();
        let error = load(
            &input(None, b""),
            InputKind::Png,
            &settings(&args, Background::Keep),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("empty"), "{error}");
    }

    #[test]
    fn an_svg_rasterizes_at_the_requested_resolution() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10">
             <rect width="20" height="10" fill="#ff0000"/>
           </svg>"##;
        let args = flamegraph_args();
        let (source, origin) = load(
            &input(Some("square.svg"), svg),
            InputKind::Svg,
            &settings(&args, Background::Keep),
        )
        .unwrap();
        assert_eq!((source.width(), source.height()), (20, 10));
        assert!(matches!(origin, Origin::Svg { .. }));
        match source {
            Source::Pixels(image) => assert_eq!(image.get(5, 5), [0xff, 0, 0, 0xff]),
            Source::Png { .. } => panic!("an SVG must arrive as pixels"),
        }

        // Doubling the dpi doubles the pixels in each direction.
        let mut doubled = settings(&args, Background::Keep);
        doubled.dpi = 192.0;
        let (source, _) = load(&input(None, svg), InputKind::Svg, &doubled).unwrap();
        assert_eq!((source.width(), source.height()), (40, 20));
    }

    #[test]
    fn an_svg_background_fills_the_transparent_area() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
             <circle cx="5" cy="5" r="2" fill="#00ff00"/>
           </svg>"##;
        let image = rasterize_svg(svg, None, 96.0, Background::Keep).unwrap();
        assert_eq!(image.get(0, 0)[3], 0, "the corner starts empty");

        let filled = rasterize_svg(svg, None, 96.0, Background::Colour(Colour::WHITE)).unwrap();
        assert_eq!(filled.get(0, 0), [0xff, 0xff, 0xff, 0xff]);
        assert!(!filled.has_transparency());
    }

    #[test]
    fn an_invalid_svg_is_reported_as_such() {
        let args = flamegraph_args();
        let error = load(
            &input(None, b"this is not xml at all"),
            InputKind::Svg,
            &settings(&args, Background::Keep),
        )
        .unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("SVG"), "{chain}");
    }

    #[test]
    fn absurd_raster_sizes_are_refused_before_allocating() {
        assert!(guard_raster_size(100, 100).is_ok());
        let error = guard_raster_size(100_000, 100_000).unwrap_err().to_string();
        assert!(error.contains("--dpi"), "{error}");
    }

    #[test]
    fn collapsed_stacks_become_a_flame_graph() {
        let args = flamegraph_args();
        let folded = b"main;foo;bar 10\nmain;foo;baz 5\nmain;quux 2\n";
        let (source, origin) = load(
            &input(Some("out.folded"), folded),
            InputKind::Folded,
            &settings(&args, Background::Keep),
        )
        .unwrap();
        match origin {
            Origin::Flamegraph { stacks, kind, .. } => {
                assert_eq!(stacks, 3);
                assert_eq!(kind, InputKind::Folded);
            }
            other => panic!("expected a flame graph, got {other:?}"),
        }
        // Wide and short, as flame graphs are.
        assert!(source.width() > source.height(), "{source:?}");
        assert!(source.payload_len() > 0);
    }

    #[test]
    fn a_profile_that_is_not_one_reports_what_it_tried() {
        let args = flamegraph_args();
        let error = load(
            &input(None, b"just some prose\n"),
            InputKind::Folded,
            &settings(&args, Background::Keep),
        )
        .unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains("standard input"), "{chain}");
        assert!(chain.contains("folded output"), "{chain}");
        assert!(chain.contains("collapsed stacks"), "{chain}");
    }

    #[test]
    fn the_first_line_is_quoted_and_truncated_for_error_messages() {
        let short = input(None, b"digraph G {\nmore\n");
        assert_eq!(short.first_line(), "digraph G {");
        let long = input(None, "x".repeat(200).as_bytes());
        assert!(long.first_line().ends_with('…'));
        assert!(long.first_line().chars().count() <= 73);
    }

    #[test]
    fn the_head_is_capped_at_the_sniffers_window() {
        let big = input(None, &vec![b'a'; 10_000]);
        assert_eq!(big.head().len(), super::super::sniff::HEAD_LEN);
        let small = input(None, b"abc");
        assert_eq!(small.head(), b"abc");
    }

    #[test]
    fn inputs_name_themselves_for_messages() {
        assert_eq!(input(Some("a/b.png"), b"x").describe(), "a/b.png");
        assert_eq!(input(None, b"x").describe(), "standard input");
    }

    #[test]
    fn background_is_only_unsupported_for_png() {
        assert!(!background_is_supported(InputKind::Png));
        for kind in [
            InputKind::Jpeg,
            InputKind::Svg,
            InputKind::Dot,
            InputKind::Folded,
            InputKind::Auto,
        ] {
            assert!(background_is_supported(kind), "{kind:?}");
        }
    }

    #[test]
    fn applying_a_background_makes_pixels_opaque_and_leaves_png_alone() {
        let mut image = Rgba::new(2, 2).unwrap();
        image.set(0, 0, [0xff, 0, 0, 0x80]);
        match apply_background(Source::Pixels(image), Background::Colour(Colour::WHITE)) {
            Source::Pixels(image) => assert!(!image.has_transparency()),
            Source::Png { .. } => panic!("pixels must stay pixels"),
        }

        let png = Source::Png {
            bytes: vec![1, 2, 3],
            width: 1,
            height: 1,
        };
        match apply_background(png, Background::Colour(Colour::BLACK)) {
            Source::Png { bytes, .. } => assert_eq!(bytes, vec![1, 2, 3], "untouched"),
            Source::Pixels(_) => panic!("a PNG must never be decoded"),
        }
    }

    #[test]
    fn keeping_the_background_changes_nothing() {
        let mut image = Rgba::new(1, 1).unwrap();
        image.set(0, 0, [1, 2, 3, 4]);
        match apply_background(Source::Pixels(image), Background::Keep) {
            Source::Pixels(image) => assert_eq!(image.get(0, 0), [1, 2, 3, 4]),
            Source::Png { .. } => panic!("unreachable"),
        }
        assert_eq!(flatten_colour(Background::Keep), None);
        assert_eq!(
            flatten_colour(Background::Colour(Colour::BLACK)),
            Some(Colour::BLACK)
        );
    }

    #[test]
    fn the_sniffed_kind_of_a_real_file_drives_the_load() {
        let args = flamegraph_args();
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"/>"#;
        let input = input(Some("square.svg"), svg);
        let kind = super::super::sniff::detect(input.name.as_deref(), input.head())
            .unwrap()
            .kind;
        let (source, origin) = load(&input, kind, &settings(&args, Background::Keep)).unwrap();
        assert_eq!((source.width(), source.height()), (4, 4));
        assert!(matches!(origin, Origin::Svg { .. }));
    }

    #[test]
    fn the_documented_graphviz_engines_include_the_common_ones() {
        for engine in ["dot", "neato", "twopi", "circo", "fdp"] {
            assert!(GRAPHVIZ_ENGINES.contains(&engine));
        }
    }

    #[test]
    fn graphviz_reports_a_missing_dot_binary_by_name() {
        // Only meaningful where `dot` is absent; where it is present, the graph
        // renders instead, which is just as good a check of the same path.
        let args = flamegraph_args();
        let result = load(
            &input(Some("g.dot"), b"digraph G { a -> b }\n"),
            InputKind::Dot,
            &settings(&args, Background::Keep),
        );
        match result {
            Ok((source, origin)) => {
                assert!(matches!(origin, Origin::Graphviz { .. }));
                assert!(matches!(source, Source::Png { .. }), "dot emits PNG");
                assert!(source.width() > 0);
            }
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("graphviz") || message.contains("dot"),
                    "{message}"
                );
            }
        }
    }
}
