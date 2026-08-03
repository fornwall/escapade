//! The text output: one item per line, the sequence on the left and what it does
//! on the right.
//!
//! ```text
//! CSI 1;31 m      SGR — bold, foreground red
//! "red"
//! CSI 0 m         SGR — reset all attributes
//! ```
//!
//! The shape is chosen to be **diffable**. A decode is a text form of a byte
//! stream, and the second use a decoder gets put to is a regression test — "these
//! bytes used to say this" — so the same bytes always produce the same line, in a
//! fixed column, with no width-dependent wrapping. `--hex` adds an offset column
//! and a hex line; `--unicode` adds the breakdown of a run of text.

use std::io::{self, Write};

use super::{
    Options, annotate,
    describe::Description,
    display::{self, HEX_LIMIT},
    parse::{Decoded, Item},
};
use crate::{
    theme::{Ink, Theme},
    unicode,
};

/// Column the prose starts in, when the sequence is short enough to leave room.
const PROSE_COLUMN: usize = 16;

/// Width of the offset column that `--hex` adds.
const OFFSET_WIDTH: usize = 6;

/// Writes one item.
///
/// # Errors
///
/// Fails if the output does — a closed pipe, most likely, which the caller treats
/// as a normal end rather than a fault.
pub fn item(
    out: &mut impl Write,
    theme: &Theme,
    options: Options,
    decoded: &Decoded,
    description: &Description,
) -> io::Result<()> {
    let (prefix, indent) = if options.hex {
        (
            format!("{:0width$x}  ", decoded.offset, width = OFFSET_WIDTH),
            " ".repeat(OFFSET_WIDTH + 2),
        )
    } else {
        (String::new(), String::new())
    };

    let sequence = theme.paint(&description.sequence, sequence_ink(description));
    let prose = prose(theme, description);
    let padding = PROSE_COLUMN.saturating_sub(unicode::display_width(&description.sequence));
    if prose.is_empty() {
        writeln!(out, "{prefix}{sequence}")?;
    } else if padding > 0 {
        writeln!(out, "{prefix}{sequence}{}{prose}", " ".repeat(padding))?;
    } else {
        // A long sequence keeps its prose on the same line rather than wrapping,
        // so one item is always one line and a diff stays readable.
        writeln!(out, "{prefix}{sequence}  {prose}")?;
    }

    if options.hex {
        let hex = display::hex(&decoded.bytes, HEX_LIMIT);
        writeln!(out, "{indent}{}", theme.paint(&hex, Ink::Muted))?;
    }

    if options.unicode
        && let Item::Text(text) = &decoded.item
    {
        annotation(out, theme, &indent, text)?;
    }
    Ok(())
}

/// The ink a sequence is drawn in, which is how a problem catches the eye while
/// scrolling.
fn sequence_ink(description: &Description) -> Ink {
    match description.kind {
        "malformed" => Ink::Bad,
        "unterminated" => Ink::Warn,
        "text" => Ink::Strong,
        _ => Ink::Code,
    }
}

/// The prose column, painted.
///
/// Deliberately the same words, in the same order, as [`Description::prose`] — a
/// test holds the two together — so that the plain-text output and the JSON
/// `description` field never disagree.
fn prose(theme: &Theme, description: &Description) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(problem) = &description.problem {
        parts.push(theme.paint(problem, Ink::Bad));
    }
    match (&description.label, &description.summary) {
        (Some(label), Some(summary)) => {
            parts.push(format!("{} — {summary}", theme.paint(label, Ink::Accent)));
        }
        (Some(label), None) => parts.push(theme.paint(label, Ink::Accent)),
        (None, Some(summary)) => parts.push(summary.clone()),
        (None, None) => {}
    }
    let mut line = parts.join(" — ");
    if !description.features.is_empty() {
        let names: Vec<&str> = description
            .features
            .iter()
            .map(|found| found.feature_name.as_str())
            .collect();
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&theme.paint(&format!("[{}]", names.join(", ")), Ink::Link));
    }
    for note in &description.notes {
        if !line.is_empty() {
            line.push_str("; ");
        }
        line.push_str(&theme.paint(note, Ink::Muted));
    }
    line
}

/// The `--unicode` breakdown of one run of text.
///
/// Clusters with more than one codepoint get a heading of their own, because the
/// interesting question about `👨‍👩‍👧` is that those five codepoints are *one*
/// character, and a flat list of five would not answer it.
fn annotation(out: &mut impl Write, theme: &Theme, indent: &str, text: &str) -> io::Result<()> {
    let analysis = annotate::analyse(text);
    writeln!(
        out,
        "{indent}  {}",
        theme.paint(&analysis.summary(), Ink::Muted)
    )?;
    for cluster in &analysis.clusters {
        let deeper = if cluster.is_composite() {
            writeln!(
                out,
                "{indent}  {}",
                theme.paint(
                    &format!(
                        "cluster {} — {} codepoints, {} wide",
                        display::quote(&cluster.text),
                        cluster.codepoints.len(),
                        columns(cluster.width)
                    ),
                    Ink::Muted
                )
            )?;
            "    "
        } else {
            "  "
        };
        for info in &cluster.codepoints {
            let glyph = annotate::glyph(info);
            let glyph_padding = 3usize.saturating_sub(unicode::display_width(&glyph));
            writeln!(
                out,
                "{indent}{deeper}{:<9}{glyph}{}{:<34}{:<11}{}",
                info.label(),
                " ".repeat(glyph_padding),
                info.title_name(),
                annotate::category(info),
                columns(info.width),
            )?;
        }
    }
    Ok(())
}

/// `1 column` but `2 columns`.
fn columns(width: usize) -> String {
    if width == 1 {
        "1 column".to_owned()
    } else {
        format!("{width} columns")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        super::{catalog::Index, describe::describe, parse::decode_all},
        *,
    };
    use crate::db::Catalog;

    fn options() -> Options {
        Options {
            hex: false,
            unicode: false,
            sequences_only: false,
        }
    }

    /// The whole text rendering of a stream, without colour.
    fn render(bytes: &[u8], options: Options) -> String {
        let catalog = Catalog::load().unwrap();
        let index = Index::from_catalog(&catalog);
        let theme = Theme::plain();
        let mut out = Vec::new();
        for decoded in decode_all(bytes) {
            if options.sequences_only && decoded.item.is_text() {
                continue;
            }
            let description = describe(&decoded, &index);
            item(&mut out, &theme, options, &decoded, &description).unwrap();
        }
        String::from_utf8(out).unwrap()
    }

    /// A feature's display name, straight from the database.
    fn feature_name(id: &str) -> String {
        Catalog::load().unwrap().feature(id).unwrap().name.clone()
    }

    #[test]
    fn the_output_from_the_brief() {
        let attributes = feature_name("text-attributes");
        let rendered = render(b"\x1b[1;31mred\x1b[0m", options());
        assert_eq!(
            rendered,
            format!(
                "CSI 1;31 m      SGR — bold, foreground red [{attributes}]\n\
                 \"red\"\n\
                 CSI 0 m         SGR — reset all attributes [{attributes}]\n"
            )
        );
    }

    #[test]
    fn a_hyperlink_stream_names_its_feature() {
        let rendered = render(
            b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
            options(),
        );
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3, "{rendered}");
        assert!(lines[0].starts_with("OSC 8;;https://example.com ST  OSC 8 — start a hyperlink"));
        assert!(lines[0].ends_with("[Hyperlinks (OSC 8)]"));
        assert_eq!(lines[1], "\"link\"");
        assert!(lines[2].contains("end the current hyperlink"));
    }

    #[test]
    fn an_unterminated_osc_is_the_last_line_and_says_what_is_missing() {
        let rendered = render(b"\x1b]8;;broken", options());
        assert!(rendered.starts_with("OSC 8;;broken"), "{rendered}");
        assert!(rendered.contains("unterminated OSC"), "{rendered}");
    }

    #[test]
    fn sequences_only_drops_the_text_but_keeps_the_controls() {
        // A stray control byte is exactly what someone using this flag is hunting.
        let rendered = render(
            b"before\x1b[1m\x07after",
            Options {
                sequences_only: true,
                ..options()
            },
        );
        assert!(!rendered.contains("before"), "{rendered}");
        assert!(!rendered.contains("after"), "{rendered}");
        assert!(rendered.contains("CSI 1 m"), "{rendered}");
        assert!(rendered.contains("BEL"), "{rendered}");
    }

    #[test]
    fn hex_adds_an_offset_column_and_the_bytes() {
        let rendered = render(
            b"\x1b[1mhi",
            Options {
                hex: true,
                ..options()
            },
        );
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(
            lines[0].starts_with("000000  CSI 1 m         SGR — bold"),
            "{}",
            lines[0]
        );
        assert_eq!(lines[1], "        1b 5b 31 6d");
        assert_eq!(lines[2], "000004  \"hi\"");
        assert_eq!(lines[3], "        68 69");
    }

    #[test]
    fn hex_counts_the_bytes_it_did_not_show() {
        let payload = "A".repeat(200);
        let stream = format!("\u{1b}_G{payload}\u{1b}\\");
        let rendered = render(
            stream.as_bytes(),
            Options {
                hex: true,
                ..options()
            },
        );
        assert!(rendered.contains("… +"), "{rendered}");
    }

    #[test]
    fn unicode_annotates_the_family_from_the_brief() {
        let rendered = render(
            "family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}".as_bytes(),
            Options {
                unicode: true,
                ..options()
            },
        );
        assert!(
            rendered.contains("12 codepoints in 8 grapheme clusters"),
            "{rendered}"
        );
        assert!(rendered.contains("cluster \""), "{rendered}");
        assert!(rendered.contains("U+1F468"), "{rendered}");
        assert!(rendered.contains("Zero Width Joiner"), "{rendered}");
        // The joiner is named, never printed.
        assert!(!rendered.contains('\u{200d}'), "{rendered}");
    }

    #[test]
    fn unicode_annotation_only_applies_to_text() {
        let rendered = render(
            b"\x1b[0m",
            Options {
                unicode: true,
                ..options()
            },
        );
        assert_eq!(rendered.lines().count(), 1, "{rendered}");
    }

    #[test]
    fn one_item_is_always_one_line_plus_its_extras() {
        // Long sequences must not wrap: a wrapped decode is not diffable.
        let long = format!("\u{1b}]0;{}\u{7}", "t".repeat(300));
        let rendered = render(long.as_bytes(), options());
        assert_eq!(rendered.lines().count(), 1, "{rendered}");
    }

    #[test]
    fn the_painted_prose_says_exactly_what_the_json_description_says() {
        // The two output formats must not drift apart.
        let catalog = Catalog::load().unwrap();
        let index = Index::from_catalog(&catalog);
        let theme = Theme::plain();
        let stream = b"\x1b[1;31m\x1b]8;;https://example.com\x1b\\\x1b]8;;broken";
        for decoded in decode_all(stream) {
            let description = describe(&decoded, &index);
            assert_eq!(prose(&theme, &description), description.prose());
        }
    }

    #[test]
    fn nothing_rendered_contains_a_control_byte() {
        let nasty = b"\x1b]0;a\x00b\x07\x1b[1;31m\x1b_G\x1b\\\x9b\xffx\x1b[?1049h\x1b]8;;broken";
        let rendered = render(
            nasty,
            Options {
                hex: true,
                unicode: true,
                sequences_only: false,
            },
        );
        for character in rendered.chars() {
            assert!(
                !character.is_control() || character == '\n',
                "control byte {:#04x} in the output",
                u32::from(character)
            );
        }
    }
}
