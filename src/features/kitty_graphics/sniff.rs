//! Working out what the input actually is.
//!
//! `render` accepts images, vector graphics, Graphviz sources and profiler
//! output, from a file or from a pipe — and a pipe has no file name. So the type
//! is decided in three stages, cheapest and most certain first:
//!
//! 1. **The file extension**, when there is one. Unambiguous, free, and what the
//!    user almost certainly meant.
//! 2. **The leading bytes.** The [`infer`] crate recognises PNG, JPEG, GIF and
//!    WebP from their magic numbers. It does not recognise SVG or DOT — neither
//!    has a magic number, both being text — so those two are matched here with
//!    deliberately narrow rules.
//! 3. **inferno's format guesser**, for text that is none of the above. That
//!    stage does not live in this module, because it is not a *test*: it is an
//!    attempt to fold the input into collapsed stacks, and either it works or the
//!    input was something we do not handle. [`detect`] returns `None` and
//!    [`super::render`] hands the bytes to inferno.
//!
//! Everything here is a pure function of a name and a byte slice, so the whole
//! decision tree is testable without touching the filesystem.

use std::path::Path;

use crate::cli::InputKind;

/// How many leading bytes are enough to decide. Magic numbers are a handful of
/// bytes; the text heuristics only ever look at the first line or two.
pub const HEAD_LEN: usize = 1024;

/// Which stage of the sniffer produced an answer.
///
/// Worth reporting: a user whose file was misread wants to know whether the
/// extension or the bytes were believed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// `--input-type` was given, so nothing was sniffed at all.
    Forced,
    /// The file extension.
    Extension,
    /// A magic number in the leading bytes.
    Magic,
    /// A text heuristic: the input looks like SVG or DOT.
    Text,
    /// inferno's format guesser folded it into collapsed stacks.
    Guessed,
}

impl Stage {
    /// How the decision was reached, for the dry-run report.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Forced => "forced with --input-type",
            Self::Extension => "from the file extension",
            Self::Magic => "from the leading bytes",
            Self::Text => "from the text of the first line",
            Self::Guessed => "guessed by inferno's format detector",
        }
    }
}

/// A concluded input type and the stage that concluded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    /// What the input is. Never [`InputKind::Auto`].
    pub kind: InputKind,
    /// How that was decided.
    pub stage: Stage,
}

/// Sniffs the input type from a name and its leading bytes, keeping the stage that
/// produced the answer.
///
/// Returns `None` when nothing recognised it, which is the signal to try inferno's
/// format guesser.
#[must_use]
pub fn detect(name: Option<&Path>, head: &[u8]) -> Option<Detection> {
    if let Some(kind) = name.and_then(from_extension) {
        return Some(Detection {
            kind,
            stage: Stage::Extension,
        });
    }
    if let Some(kind) = from_magic(head) {
        return Some(Detection {
            kind,
            stage: Stage::Magic,
        });
    }
    if let Some(kind) = from_text(head) {
        return Some(Detection {
            kind,
            stage: Stage::Text,
        });
    }
    None
}

/// Maps a file extension to an input type, case-insensitively.
///
/// `.svgz` is a gzipped SVG, which resvg unpacks itself. `.folded` and
/// `.collapsed` are the two names the flame-graph world uses for the same
/// format.
#[must_use]
pub fn from_extension(path: &Path) -> Option<InputKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => InputKind::Png,
        "jpg" | "jpeg" => InputKind::Jpeg,
        "gif" => InputKind::Gif,
        "webp" => InputKind::Webp,
        "svg" | "svgz" => InputKind::Svg,
        "dot" | "gv" => InputKind::Dot,
        "folded" | "collapsed" => InputKind::Folded,
        "perf" => InputKind::Perf,
        _ => return None,
    })
}

/// Recognises a raster format from its magic number.
///
/// This is what the extension check cannot do: a `.jpg` that is really a PNG
/// takes the PNG fast path, and a pipe with no name at all is still identifiable.
#[must_use]
pub fn from_magic(head: &[u8]) -> Option<InputKind> {
    let inferred = infer::get(head)?;
    Some(match inferred.extension() {
        "png" => InputKind::Png,
        "jpg" | "jpeg" => InputKind::Jpeg,
        "gif" => InputKind::Gif,
        "webp" => InputKind::Webp,
        _ => return None,
    })
}

/// Recognises SVG and DOT from the shape of their first line.
///
/// Neither has a magic number, so these are heuristics — narrow ones, because a
/// false positive here means an image viewer tries to run Graphviz on somebody's
/// log file:
///
/// * **SVG** — the first non-whitespace text is `<svg`, or it is `<?xml` and an
///   `<svg` element appears within the first [`HEAD_LEN`] bytes. Requiring the
///   `<svg` in the second case is what keeps every other XML dialect out.
/// * **DOT** — the *first* token is `graph`, `digraph`, or `strict` followed by
///   one of those. Anchoring at the start is what stops a file that merely
///   mentions `digraph` in a comment forty lines down from being misread.
#[must_use]
pub fn from_text(head: &[u8]) -> Option<InputKind> {
    // Not valid UTF-8 means not text, and neither format is binary.
    let text = std::str::from_utf8(head).ok().or_else(|| {
        // A truncated head can split a multi-byte character; use what is valid.
        let valid = head
            .iter()
            .rposition(u8::is_ascii)
            .map_or(0, |index| index + 1);
        std::str::from_utf8(&head[..valid]).ok()
    })?;
    let trimmed = text.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');

    if trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && text.contains("<svg")) {
        return Some(InputKind::Svg);
    }
    if looks_like_dot(trimmed) {
        return Some(InputKind::Dot);
    }
    None
}

/// Whether `text` opens with a Graphviz graph declaration.
fn looks_like_dot(text: &str) -> bool {
    let mut words = text.split_whitespace();
    let first = words.next().unwrap_or_default();
    // `strict` is a modifier, so look at what follows it.
    let keyword = if first.eq_ignore_ascii_case("strict") {
        words.next().unwrap_or_default()
    } else {
        first
    };
    // A graph name or `{` may be attached directly to the keyword.
    let keyword = keyword
        .split(['{', '('])
        .next()
        .unwrap_or_default()
        .trim_end_matches(|c: char| !c.is_alphanumeric());
    keyword.eq_ignore_ascii_case("graph") || keyword.eq_ignore_ascii_case("digraph")
}

/// Whether an input type is profiler output that needs collapsing before it can
/// become a flame graph.
#[must_use]
pub fn is_profile(kind: InputKind) -> bool {
    matches!(
        kind,
        InputKind::Folded
            | InputKind::Perf
            | InputKind::Dtrace
            | InputKind::Sample
            | InputKind::Vtune
            | InputKind::Vsprof
    )
}

/// The name of an input type, as `--input-type` spells it.
#[must_use]
pub fn name(kind: InputKind) -> &'static str {
    match kind {
        InputKind::Auto => "auto",
        InputKind::Png => "png",
        InputKind::Jpeg => "jpeg",
        InputKind::Gif => "gif",
        InputKind::Webp => "webp",
        InputKind::Svg => "svg",
        InputKind::Dot => "dot",
        InputKind::Folded => "folded",
        InputKind::Perf => "perf",
        InputKind::Dtrace => "dtrace",
        InputKind::Sample => "sample",
        InputKind::Vtune => "vtune",
        InputKind::Vsprof => "vsprof",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A few bytes of each format, enough for `infer` to recognise it.
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00";
    const GIF: &[u8] = b"GIF89a\x10\x00\x10\x00";
    const WEBP: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";

    fn path(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn every_extension_maps_to_the_kind_it_names() {
        let cases = [
            ("a.png", InputKind::Png),
            ("a.PNG", InputKind::Png),
            ("a.jpg", InputKind::Jpeg),
            ("a.jpeg", InputKind::Jpeg),
            ("a.JPEG", InputKind::Jpeg),
            ("a.gif", InputKind::Gif),
            ("a.webp", InputKind::Webp),
            ("a.svg", InputKind::Svg),
            ("a.svgz", InputKind::Svg),
            ("a.dot", InputKind::Dot),
            ("a.gv", InputKind::Dot),
            ("a.folded", InputKind::Folded),
            ("a.collapsed", InputKind::Folded),
            ("out.perf", InputKind::Perf),
        ];
        for (name, expected) in cases {
            assert_eq!(from_extension(&path(name)), Some(expected), "{name}");
        }
    }

    #[test]
    fn unknown_and_missing_extensions_do_not_match() {
        assert_eq!(from_extension(&path("a.tiff")), None);
        assert_eq!(from_extension(&path("README")), None);
        assert_eq!(from_extension(&path("archive.tar.gz")), None);
        assert_eq!(from_extension(&path(".hidden")), None);
    }

    #[test]
    fn magic_numbers_identify_the_raster_formats() {
        assert_eq!(from_magic(PNG), Some(InputKind::Png));
        assert_eq!(from_magic(JPEG), Some(InputKind::Jpeg));
        assert_eq!(from_magic(GIF), Some(InputKind::Gif));
        assert_eq!(from_magic(WEBP), Some(InputKind::Webp));
    }

    #[test]
    fn magic_numbers_do_not_identify_text_or_rubbish() {
        assert_eq!(from_magic(b"main;foo 10\n"), None);
        assert_eq!(from_magic(b""), None);
        assert_eq!(from_magic(b"<svg xmlns=\"...\">"), None, "SVG has no magic");
        assert_eq!(from_magic(b"digraph G {}"), None, "nor does DOT");
    }

    #[test]
    fn svg_is_recognised_from_its_opening_element() {
        assert_eq!(from_text(b"<svg xmlns=\"x\"/>"), Some(InputKind::Svg));
        assert_eq!(
            from_text(b"  \n\t<svg/>"),
            Some(InputKind::Svg),
            "leading space"
        );
        assert_eq!(
            from_text("\u{feff}<svg/>".as_bytes()),
            Some(InputKind::Svg),
            "a byte order mark"
        );
        assert_eq!(
            from_text(b"<?xml version=\"1.0\"?>\n<!DOCTYPE svg>\n<svg width=\"1\">"),
            Some(InputKind::Svg),
            "the usual xml preamble"
        );
    }

    #[test]
    fn xml_that_is_not_svg_is_not_claimed() {
        assert_eq!(
            from_text(b"<?xml version=\"1.0\"?>\n<rss version=\"2.0\"><channel/>"),
            None,
            "requiring an <svg element is what keeps other dialects out"
        );
    }

    #[test]
    fn dot_is_recognised_from_its_first_token() {
        for source in [
            "digraph G { a -> b }",
            "graph { a -- b }",
            "strict digraph G { a -> b }",
            "strict graph {}",
            "digraph{a}",
            "\n\n  digraph deps {\n",
            "DIGRAPH G { }",
        ] {
            assert_eq!(
                from_text(source.as_bytes()),
                Some(InputKind::Dot),
                "{source:?}"
            );
        }
    }

    #[test]
    fn a_file_that_merely_mentions_digraph_is_not_dot() {
        let mut text = String::new();
        for index in 0..40 {
            use std::fmt::Write as _;
            let _ = writeln!(text, "line {index} of an ordinary log file");
        }
        text.push_str("note: the graphviz digraph was written by hand\n");
        assert_eq!(
            from_text(text.as_bytes()),
            None,
            "the keyword has to be the first token, not merely present"
        );
    }

    #[test]
    fn collapsed_stacks_match_nothing_and_fall_through_to_inferno() {
        assert_eq!(from_text(b"main;foo;bar 10\nmain;baz 5\n"), None);
        assert_eq!(detect(None, b"main;foo 10\n"), None);
    }

    #[test]
    fn perf_output_matches_nothing_and_falls_through() {
        let perf = b"swapper     0 [000] 12345.678: cpu-clock:\n\t ffffffff81 native_write_msr\n";
        assert_eq!(detect(None, perf), None);
    }

    #[test]
    fn binary_rubbish_is_not_mistaken_for_text() {
        assert_eq!(from_text(&[0xff, 0xfe, 0x00, 0x01, 0x80]), None);
        assert_eq!(from_text(b""), None);
    }

    #[test]
    fn a_head_truncated_mid_character_still_works() {
        // "<svg…" where the ellipsis is cut in half.
        let mut head = "<svg>…".as_bytes().to_vec();
        head.truncate(head.len() - 1);
        assert_eq!(from_text(&head), Some(InputKind::Svg));
    }

    #[test]
    fn the_extension_beats_the_leading_bytes() {
        // A `.dot` file whose contents are a PNG: the name wins, because that is
        // what the user asked for and the extension is the unambiguous signal.
        let detection = detect(Some(&path("graph.dot")), PNG).unwrap();
        assert_eq!(detection.kind, InputKind::Dot);
        assert_eq!(detection.stage, Stage::Extension);
    }

    #[test]
    fn the_leading_bytes_win_when_there_is_no_useful_extension() {
        let detection = detect(Some(&path("mystery.bin")), PNG).unwrap();
        assert_eq!(detection.kind, InputKind::Png);
        assert_eq!(detection.stage, Stage::Magic);

        let detection = detect(None, GIF).unwrap();
        assert_eq!(detection.kind, InputKind::Gif);
        assert_eq!(detection.stage, Stage::Magic);
    }

    #[test]
    fn text_heuristics_are_the_last_resort_before_inferno() {
        let detection = detect(Some(&path("noext")), b"digraph G {}").unwrap();
        assert_eq!(detection.kind, InputKind::Dot);
        assert_eq!(detection.stage, Stage::Text);
        assert_eq!(detect(None, b"who knows"), None);
    }

    #[test]
    fn profiles_are_the_kinds_that_need_collapsing() {
        for kind in [
            InputKind::Folded,
            InputKind::Perf,
            InputKind::Dtrace,
            InputKind::Sample,
            InputKind::Vtune,
            InputKind::Vsprof,
        ] {
            assert!(is_profile(kind), "{}", name(kind));
        }
        for kind in [
            InputKind::Auto,
            InputKind::Png,
            InputKind::Jpeg,
            InputKind::Gif,
            InputKind::Webp,
            InputKind::Svg,
            InputKind::Dot,
        ] {
            assert!(!is_profile(kind), "{}", name(kind));
        }
    }

    #[test]
    fn every_kind_has_a_name_and_every_stage_a_description() {
        for kind in [
            InputKind::Auto,
            InputKind::Png,
            InputKind::Jpeg,
            InputKind::Gif,
            InputKind::Webp,
            InputKind::Svg,
            InputKind::Dot,
            InputKind::Folded,
            InputKind::Perf,
            InputKind::Dtrace,
            InputKind::Sample,
            InputKind::Vtune,
            InputKind::Vsprof,
        ] {
            assert!(!name(kind).is_empty());
        }
        for stage in [
            Stage::Forced,
            Stage::Extension,
            Stage::Magic,
            Stage::Text,
            Stage::Guessed,
        ] {
            assert!(!stage.describe().is_empty());
        }
    }

    #[test]
    fn the_head_length_is_enough_for_an_xml_preamble() {
        // A long DOCTYPE followed by the <svg element must still be caught.
        let mut source = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        source.push_str("<!-- ");
        source.push_str(&"x".repeat(400));
        source.push_str(" -->\n");
        source.push_str("<svg/>");
        assert!(source.len() < HEAD_LEN);
        assert_eq!(from_text(source.as_bytes()), Some(InputKind::Svg));
    }
}
