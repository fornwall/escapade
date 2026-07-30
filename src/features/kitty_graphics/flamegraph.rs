//! Profiler output in, flame graph out.
//!
//! Two steps, both [`inferno`]'s:
//!
//! 1. **Collapse.** Profiler output — `perf script`, DTrace, macOS `sample`,
//!    VTune, Visual Studio — is folded into *collapsed stacks*, one
//!    `frame;frame;frame count` per line. Input that is already collapsed skips
//!    this. When the type is unknown, `inferno::collapse::guess` tries every
//!    collapser it has.
//! 2. **Draw.** Collapsed stacks become an SVG.
//!
//! And then the SVG goes through the same resvg path as any other SVG, which is
//! the point of doing it this way: a flame graph is a vector image, and this crate
//! already knows how to put a vector image in a terminal.
//!
//! Two things are specific to rendering into a terminal rather than a browser.
//! The SVG that inferno produces is interactive — it carries JavaScript, hover
//! tooltips and a search box — and every bit of that is inert once rasterized;
//! what survives is the shape, the colours and the labels. And a flame graph is
//! *wide and short*, so the geometry it wants is nothing like a photograph's; see
//! [`default_width_cols`].

use anyhow::{Context as _, Result, bail};
use inferno::{
    collapse::{Collapse, dtrace, guess, perf, sample, vsprof, vtune},
    flamegraph::{
        Direction, Options,
        color::{BasicPalette, MultiPalette, Palette},
    },
};

use crate::cli::{FlamegraphArgs, FlamegraphPalette, InputKind};

/// inferno's own default SVG width, used when the terminal's is not known.
const DEFAULT_IMAGE_WIDTH: usize = 1200;

/// Collapses profiler output into collapsed stacks.
///
/// [`InputKind::Folded`] passes the text through untouched. Every other profile
/// kind runs the matching collapser. [`InputKind::Auto`] runs
/// `inferno::collapse::guess`, which tries each of them in turn — and then falls
/// back to treating the input as already collapsed, because **inferno's guesser
/// does not recognise collapsed stacks**: handed `main;foo 10` it succeeds and
/// produces nothing at all. That is worth knowing, since collapsed stacks are the
/// most likely thing to arrive on a pipe.
///
/// # Errors
///
/// Fails if the collapser rejects the input, or if the result is empty and the
/// input is not collapsed stacks either — which is what a wrong guess looks like,
/// and is worth an error rather than a blank image.
pub fn collapse(bytes: &[u8], kind: InputKind) -> Result<String> {
    if kind == InputKind::Folded {
        return as_collapsed(bytes);
    }

    let mut folded = Vec::new();
    let specific = match kind {
        InputKind::Perf => Some(perf::Folder::default().collapse(bytes, &mut folded)),
        InputKind::Dtrace => Some(dtrace::Folder::default().collapse(bytes, &mut folded)),
        InputKind::Sample => Some(sample::Folder::default().collapse(bytes, &mut folded)),
        InputKind::Vtune => Some(vtune::Folder::default().collapse(bytes, &mut folded)),
        InputKind::Vsprof => Some(vsprof::Folder::default().collapse(bytes, &mut folded)),
        _ => None,
    };

    if let Some(result) = specific {
        result.with_context(|| format!("collapsing the input as {}", super::sniff::name(kind)))?;
        let text = String::from_utf8(folded).context("the collapser produced invalid UTF-8")?;
        if text.trim().is_empty() {
            bail!(
                "the input collapsed to nothing — it does not look like {} output",
                super::sniff::name(kind)
            );
        }
        return Ok(text);
    }

    // Auto: let inferno try every format it knows.
    let guessed = guess::Folder::default().collapse(bytes, &mut folded);
    if guessed.is_ok() {
        let text = String::from_utf8(folded).context("the collapser produced invalid UTF-8")?;
        if !text.trim().is_empty() {
            return Ok(text);
        }
    }
    // The guesser has no collapsed-stacks collapser, so that case lands here.
    as_collapsed(bytes).context(
        "inferno's format guesser did not recognise this as profiler output, and it is \
         not collapsed stacks either",
    )
}

/// Takes input that is already collapsed stacks, checking that it looks like it.
fn as_collapsed(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes)
        .context("collapsed stacks must be UTF-8 text")?
        .to_owned();
    if !text.lines().any(is_collapsed_line) {
        bail!(
            "this does not look like collapsed stacks: expected lines of \
             `frame;frame count`"
        );
    }
    Ok(text)
}

/// Whether a line has the shape of a collapsed stack: `frame;frame <count>`.
fn is_collapsed_line(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return false;
    }
    // The count is the last whitespace-separated field, and must be a number.
    match line.rsplit_once(char::is_whitespace) {
        Some((stack, count)) => {
            !stack.is_empty() && count.parse::<f64>().is_ok_and(|count| count >= 0.0)
        }
        None => false,
    }
}

/// Renders collapsed stacks as an SVG.
///
/// `target_width` is the pixel width the image will be shown at, used when
/// `--flamegraph-image-width` was not given: generating the SVG at the width it
/// will be displayed at keeps inferno's own proportions between the frame height,
/// the font size and the width, which is what makes the labels legible.
///
/// # Errors
///
/// Fails if inferno cannot draw the graph — most often because the collapsed
/// stacks are malformed.
pub fn to_svg(collapsed: &str, args: &FlamegraphArgs, target_width: Option<u32>) -> Result<Vec<u8>> {
    let mut options = options(args, target_width);
    let mut svg = Vec::new();
    // `from_lines` wants an iterator of &str, one collapsed stack per line.
    let lines: Vec<&str> = collapsed.lines().collect();
    inferno::flamegraph::from_lines(&mut options, lines, &mut svg)
        .context("drawing the flame graph")?;
    if svg.is_empty() {
        bail!("the flame graph came out empty");
    }
    Ok(svg)
}

/// Maps [`FlamegraphArgs`] onto inferno's [`Options`].
///
/// Fields with no flag are left at inferno's defaults, deliberately: this is a
/// one-shot render into a terminal, so the differential-profile and palette-map
/// machinery has nothing to do here.
#[must_use]
pub fn options(args: &FlamegraphArgs, target_width: Option<u32>) -> Options<'static> {
    let mut options = Options::default();

    if let Some(title) = &args.title {
        options.title.clone_from(title);
    }
    options.subtitle.clone_from(&args.subtitle);
    if let Some(palette) = args.colors {
        options.colors = colors(palette);
    }
    if args.inverted {
        // An "icicle" graph: the root at the top, stacks growing downwards.
        options.direction = Direction::Inverted;
    }
    options.reverse_stack_order = args.reverse;
    options.flame_chart = args.flame_chart;
    options.hash = args.hash;
    if let Some(min_width) = args.min_width {
        options.min_width = min_width;
    }
    if let Some(count_name) = &args.count_name {
        options.count_name.clone_from(count_name);
    }
    if let Some(frame_height) = args.frame_height {
        options.frame_height = frame_height;
    }
    if let Some(font_size) = args.font_size {
        options.font_size = font_size;
    }
    options.image_width = Some(match args.image_width {
        Some(width) => width,
        None => target_width.map_or(DEFAULT_IMAGE_WIDTH, |width| {
            // Never below inferno's own minimum useful width, or the labels
            // vanish entirely.
            (width as usize).max(360)
        }),
    });
    if let Some(notes) = &args.notes {
        options.notes.clone_from(notes);
    }
    options
}

/// Maps the CLI's palette names onto inferno's two-level [`Palette`] enum.
///
/// The basic palettes pick a colour from a spectrum without looking at the
/// function name; the multi palettes read the name and colour by language or
/// annotation, which is what makes a Java or Rust flame graph readable at a
/// glance.
#[must_use]
pub fn colors(palette: FlamegraphPalette) -> Palette {
    match palette {
        FlamegraphPalette::Hot => Palette::Basic(BasicPalette::Hot),
        FlamegraphPalette::Mem => Palette::Basic(BasicPalette::Mem),
        FlamegraphPalette::Io => Palette::Basic(BasicPalette::Io),
        FlamegraphPalette::Red => Palette::Basic(BasicPalette::Red),
        FlamegraphPalette::Green => Palette::Basic(BasicPalette::Green),
        FlamegraphPalette::Blue => Palette::Basic(BasicPalette::Blue),
        FlamegraphPalette::Aqua => Palette::Basic(BasicPalette::Aqua),
        FlamegraphPalette::Yellow => Palette::Basic(BasicPalette::Yellow),
        FlamegraphPalette::Purple => Palette::Basic(BasicPalette::Purple),
        FlamegraphPalette::Orange => Palette::Basic(BasicPalette::Orange),
        FlamegraphPalette::Java => Palette::Multi(MultiPalette::Java),
        FlamegraphPalette::Js => Palette::Multi(MultiPalette::Js),
        FlamegraphPalette::Perl => Palette::Multi(MultiPalette::Perl),
        FlamegraphPalette::Python => Palette::Multi(MultiPalette::Python),
        FlamegraphPalette::Rust => Palette::Multi(MultiPalette::Rust),
        FlamegraphPalette::Wakeup => Palette::Multi(MultiPalette::Wakeup),
    }
}

/// How many columns a flame graph should occupy when the user did not say.
///
/// The photograph default — natural size, capped at the terminal width — is wrong
/// here. A flame graph is an *aspect ratio nothing like an image*: 1200 pixels wide
/// and often under 300 tall, where the horizontal axis is the whole population of
/// samples and the vertical axis is stack depth. Anything less than the full width
/// of the terminal throws away the axis that carries the information, so the
/// default is every column there is, with the row count derived from the aspect
/// ratio.
#[must_use]
pub fn default_width_cols(terminal_cols: u16) -> u16 {
    terminal_cols.max(1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A tiny profile: two stacks under one root.
    const FOLDED: &str = "main;foo;bar 10\nmain;foo;baz 5\nmain;quux 2\n";

    fn args() -> FlamegraphArgs {
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

    #[test]
    fn already_collapsed_input_passes_through_untouched() {
        let collapsed = collapse(FOLDED.as_bytes(), InputKind::Folded).unwrap();
        assert_eq!(collapsed, FOLDED);
    }

    #[test]
    fn collapsed_stacks_are_recognised_by_shape() {
        assert!(is_collapsed_line("main;foo;bar 10"));
        assert!(is_collapsed_line("main 1"));
        assert!(is_collapsed_line("a;b 2.5"), "fractional counts are allowed");
        assert!(!is_collapsed_line(""));
        assert!(!is_collapsed_line("# a comment"));
        assert!(!is_collapsed_line("no count here"));
        assert!(!is_collapsed_line("singleword"));
    }

    #[test]
    fn input_that_is_not_collapsed_is_refused_when_forced_as_folded() {
        let error = collapse(b"just some prose, no counts\n", InputKind::Folded)
            .unwrap_err()
            .to_string();
        assert!(error.contains("collapsed stacks"), "{error}");
    }

    #[test]
    fn auto_detection_falls_back_to_treating_input_as_collapsed() {
        // inferno's guesser does *not* recognise collapsed stacks — it succeeds
        // and emits nothing — so this only works because of the fallback.
        let collapsed = collapse(FOLDED.as_bytes(), InputKind::Auto).unwrap();
        assert!(collapsed.contains("main;foo;bar"), "{collapsed}");
    }

    #[test]
    fn infernos_guesser_really_does_ignore_collapsed_stacks() {
        // Pinning the surprising behaviour the fallback exists for, so that a
        // future inferno that fixes it is noticed here rather than in the field.
        let mut folded = Vec::new();
        let result = guess::Folder::default().collapse(FOLDED.as_bytes(), &mut folded);
        assert!(result.is_ok(), "the guesser does not error");
        assert!(
            String::from_utf8(folded).unwrap().trim().is_empty(),
            "if this ever stops being empty, the fallback in `collapse` is redundant"
        );
    }

    #[test]
    fn the_guesser_refuses_input_that_is_not_a_profile_at_all() {
        let error = collapse(b"\x00\x01\x02 not text\n", InputKind::Auto).unwrap_err();
        let error = error.to_string();
        assert!(!error.is_empty());
    }

    #[test]
    fn a_tiny_profile_becomes_a_real_svg() {
        let svg = to_svg(FOLDED, &args(), None).unwrap();
        let text = String::from_utf8(svg).unwrap();
        assert!(text.starts_with("<?xml"), "{}", &text[..40.min(text.len())]);
        assert!(text.contains("<svg"));
        assert!(text.contains("bar"), "frame labels should be in the SVG");
        assert!(text.contains("</svg>"));
    }

    #[test]
    fn the_svg_rasterizes_to_non_empty_pixels() {
        use crate::features::kitty_graphics::{pixels::Background, source};

        // The whole point of drawing an SVG: it goes through the same rasterizer as
        // any other vector input, so this checks the two halves actually meet.
        let svg = to_svg(FOLDED, &args(), Some(640)).unwrap();
        let image = source::rasterize_svg(&svg, None, 96.0, Background::Keep).unwrap();
        assert!(image.width() > 100, "a flame graph is wide");
        assert!(image.height() > 10);
        // Something must have been drawn: not every pixel is transparent.
        let drawn = image
            .as_bytes()
            .chunks_exact(4)
            .filter(|pixel| pixel[3] != 0)
            .count();
        assert!(drawn > 1000, "only {drawn} pixels were painted");
    }

    #[test]
    fn the_default_options_match_infernos_own() {
        let options = options(&args(), None);
        let reference = Options::default();
        assert_eq!(options.title, reference.title);
        assert_eq!(options.frame_height, reference.frame_height);
        assert_eq!(options.font_size, reference.font_size);
        assert!((options.min_width - reference.min_width).abs() < f64::EPSILON);
        assert_eq!(options.count_name, reference.count_name);
        assert_eq!(options.direction, Direction::Straight);
        assert!(!options.hash);
        assert!(!options.reverse_stack_order);
        assert!(!options.flame_chart);
        assert_eq!(
            options.image_width,
            Some(DEFAULT_IMAGE_WIDTH),
            "with no target width, inferno's own default"
        );
    }

    #[test]
    fn every_flag_reaches_the_option_it_names() {
        let args = FlamegraphArgs {
            title: Some("hot path".to_owned()),
            subtitle: Some("release build".to_owned()),
            colors: Some(FlamegraphPalette::Rust),
            inverted: true,
            reverse: true,
            flame_chart: true,
            hash: true,
            min_width: Some(0.5),
            count_name: Some("bytes".to_owned()),
            frame_height: Some(24),
            font_size: Some(14),
            image_width: Some(2000),
            notes: Some("recorded on a laptop".to_owned()),
        };
        let options = options(&args, Some(640));

        assert_eq!(options.title, "hot path");
        assert_eq!(options.subtitle.as_deref(), Some("release build"));
        assert_eq!(options.colors, Palette::Multi(MultiPalette::Rust));
        assert_eq!(options.direction, Direction::Inverted);
        assert!(options.reverse_stack_order);
        assert!(options.flame_chart);
        assert!(options.hash);
        assert!((options.min_width - 0.5).abs() < f64::EPSILON);
        assert_eq!(options.count_name, "bytes");
        assert_eq!(options.frame_height, 24);
        assert_eq!(options.font_size, 14);
        assert_eq!(
            options.image_width,
            Some(2000),
            "an explicit width beats the target width"
        );
        assert_eq!(options.notes, "recorded on a laptop");
    }

    #[test]
    fn the_target_width_sets_the_svg_width_when_no_flag_does() {
        let wide = options(&args(), Some(800));
        assert_eq!(wide.image_width, Some(800));
        // But never so narrow that the labels disappear.
        let narrow = options(&args(), Some(40));
        assert_eq!(narrow.image_width, Some(360));
    }

    #[test]
    fn every_palette_maps_to_a_real_inferno_palette() {
        let cases = [
            (FlamegraphPalette::Hot, Palette::Basic(BasicPalette::Hot)),
            (FlamegraphPalette::Mem, Palette::Basic(BasicPalette::Mem)),
            (FlamegraphPalette::Io, Palette::Basic(BasicPalette::Io)),
            (FlamegraphPalette::Red, Palette::Basic(BasicPalette::Red)),
            (FlamegraphPalette::Green, Palette::Basic(BasicPalette::Green)),
            (FlamegraphPalette::Blue, Palette::Basic(BasicPalette::Blue)),
            (FlamegraphPalette::Aqua, Palette::Basic(BasicPalette::Aqua)),
            (FlamegraphPalette::Yellow, Palette::Basic(BasicPalette::Yellow)),
            (FlamegraphPalette::Purple, Palette::Basic(BasicPalette::Purple)),
            (FlamegraphPalette::Orange, Palette::Basic(BasicPalette::Orange)),
            (FlamegraphPalette::Java, Palette::Multi(MultiPalette::Java)),
            (FlamegraphPalette::Js, Palette::Multi(MultiPalette::Js)),
            (FlamegraphPalette::Perl, Palette::Multi(MultiPalette::Perl)),
            (FlamegraphPalette::Python, Palette::Multi(MultiPalette::Python)),
            (FlamegraphPalette::Rust, Palette::Multi(MultiPalette::Rust)),
            (FlamegraphPalette::Wakeup, Palette::Multi(MultiPalette::Wakeup)),
        ];
        for (flag, expected) in cases {
            assert_eq!(colors(flag), expected, "{flag:?}");
        }
        // And the CLI's list is exactly inferno's list, no more and no less.
        assert_eq!(cases.len(), Palette::VARIANTS.len());
    }

    #[test]
    fn a_palette_changes_the_colours_in_the_output() {
        let mut hot = args();
        hot.colors = Some(FlamegraphPalette::Hot);
        let mut mem = args();
        mem.colors = Some(FlamegraphPalette::Mem);
        // The palettes are randomised per frame, but the hue ranges differ, so
        // the documents cannot be identical.
        assert_ne!(
            to_svg(FOLDED, &hot, None).unwrap(),
            to_svg(FOLDED, &mem, None).unwrap()
        );
    }

    #[test]
    fn a_title_and_subtitle_appear_in_the_svg() {
        let mut args = args();
        args.title = Some("escapade profile".to_owned());
        args.subtitle = Some("a subtitle".to_owned());
        let svg = String::from_utf8(to_svg(FOLDED, &args, None).unwrap()).unwrap();
        assert!(svg.contains("escapade profile"));
        assert!(svg.contains("a subtitle"));
    }

    #[test]
    fn malformed_collapsed_stacks_are_an_error_not_a_panic() {
        let error = to_svg("this is not collapsed at all", &args(), None);
        assert!(error.is_err(), "inferno should reject it");
    }

    #[test]
    fn the_default_width_is_the_whole_terminal() {
        assert_eq!(default_width_cols(120), 120);
        assert_eq!(default_width_cols(80), 80);
        assert_eq!(default_width_cols(0), 1, "never zero");
    }
}
