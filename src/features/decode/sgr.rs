//! `SGR` — Select Graphic Rendition, the sequence people actually hit.
//!
//! Nine tenths of the escape sequences in a captured stream are `CSI … m`, and
//! naming its parameters is the difference between "an SGR sequence" and "bold,
//! foreground red". This module does only that: parameters in, phrases out.
//!
//! # The two spellings of an extended colour
//!
//! A 24-bit colour is written either `38;2;1;2;3`, where the components are
//! ordinary parameters, or `38:2::1:2:3`, where they are sub-parameters of one.
//! The second is the spelling ECMA-48 actually specifies — the empty field is the
//! colour space, which the standard says to leave out — and it is unambiguous,
//! because a parser that does not understand it can still skip the whole
//! parameter. The first is what almost everything emits. Both appear here, along
//! with the four-field `38:2:1:2:3` variant that drops the empty colour space, so
//! that a decode of real output does not go quiet at the interesting part.
//!
//! # `SGR 21`
//!
//! ECMA-48 says doubly underlined; much of the world implements bold off. There is
//! no way to tell which was meant from the bytes, so both readings are reported.
//! See `style::attrs::SGR_TWENTY_ONE`, which refuses to emit it for the same
//! reason.

use super::parse::Param;

/// The eight basic colours, in the order the parameters take them.
const BASIC: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

/// Names every attribute the parameters set, in order.
///
/// An `SGR` with no parameters at all is `SGR 0`: `CSI m` resets everything, which
/// is a spelling that turns up in real output and is easy to misread as a no-op.
#[must_use]
pub fn attributes(params: &[Param]) -> Vec<String> {
    if params.is_empty() {
        return vec!["reset all attributes".to_owned()];
    }
    let mut out = Vec::new();
    let mut index = 0;
    while index < params.len() {
        let param = &params[index];
        let mut step = 1;
        let phrase = match param.value() {
            // An empty parameter is a zero, so `CSI ;1m` really does reset first.
            None => "reset all attributes".to_owned(),
            Some(code) => match code {
                0 => "reset all attributes".to_owned(),
                1 => "bold".to_owned(),
                2 => "dim".to_owned(),
                3 => "italic".to_owned(),
                4 => underline(param),
                5 => "blink".to_owned(),
                6 => "rapid blink, which nearly every terminal treats as blink".to_owned(),
                7 => "reverse video".to_owned(),
                8 => "concealed, which hides the text but still copies it".to_owned(),
                9 => "crossed out".to_owned(),
                10 => "the primary font".to_owned(),
                11..=19 => format!("alternative font {}", code - 10),
                20 => "fraktur".to_owned(),
                21 => "doubly underlined, or bold off — terminals disagree".to_owned(),
                22 => "normal intensity, cancelling bold and dim".to_owned(),
                23 => "not italic".to_owned(),
                24 => "not underlined".to_owned(),
                25 => "not blinking".to_owned(),
                26 => "proportional spacing".to_owned(),
                27 => "not reversed".to_owned(),
                28 => "revealed".to_owned(),
                29 => "not crossed out".to_owned(),
                30..=37 => format!("foreground {}", basic(code - 30)),
                38 => {
                    let (phrase, used) = extended("foreground", params, index);
                    step = used;
                    phrase
                }
                39 => "the default foreground".to_owned(),
                40..=47 => format!("background {}", basic(code - 40)),
                48 => {
                    let (phrase, used) = extended("background", params, index);
                    step = used;
                    phrase
                }
                49 => "the default background".to_owned(),
                50 => "not proportional spacing".to_owned(),
                51 => "framed".to_owned(),
                52 => "encircled".to_owned(),
                53 => "overlined".to_owned(),
                54 => "neither framed nor encircled".to_owned(),
                55 => "not overlined".to_owned(),
                58 => {
                    let (phrase, used) = extended("underline", params, index);
                    step = used;
                    phrase
                }
                59 => "the default underline colour".to_owned(),
                60..=65 => "an ideogram attribute".to_owned(),
                73 => "superscript".to_owned(),
                74 => "subscript".to_owned(),
                75 => "neither superscript nor subscript".to_owned(),
                90..=97 => format!("foreground bright {}", basic(code - 90)),
                100..=107 => format!("background bright {}", basic(code - 100)),
                other => format!("attribute {other}, which is not in this table"),
            },
        };
        out.push(phrase);
        index += step.max(1);
    }
    out
}

/// The attributes as one phrase, which is what a line of output shows.
#[must_use]
pub fn summary(params: &[Param]) -> String {
    attributes(params).join(", ")
}

/// One of the eight basic colours, by offset.
fn basic(offset: u32) -> &'static str {
    usize::try_from(offset)
        .ok()
        .and_then(|index| BASIC.get(index))
        .copied()
        .unwrap_or("an out-of-range colour")
}

/// `SGR 4`, with the sub-parameter that says which shape.
///
/// The shapes are mintty's invention rather than xterm's, which is the surprise in
/// this corner of the protocol: xterm implements none of `4:2` through `4:5`.
fn underline(param: &Param) -> String {
    if !param.has_subs() {
        return "underline".to_owned();
    }
    match param.sub(1) {
        Some(0) => "not underlined".to_owned(),
        Some(1) | None => "a single underline".to_owned(),
        Some(2) => "a double underline".to_owned(),
        Some(3) => "a curly underline".to_owned(),
        Some(4) => "a dotted underline".to_owned(),
        Some(5) => "a dashed underline".to_owned(),
        Some(other) => format!("underline style {other}, which is not in this table"),
    }
}

/// `SGR 38`, `48` and `58`: an extended colour in either spelling.
///
/// Returns the phrase and how many parameters it consumed, since the semicolon
/// spelling swallows the parameters that follow it.
fn extended(role: &str, params: &[Param], index: usize) -> (String, usize) {
    let param = &params[index];
    if param.has_subs() {
        return (colon_colour(role, param), 1);
    }
    // The semicolon spelling: the selector and its arguments are separate
    // parameters, so they are consumed here and not described again.
    let selector = params.get(index + 1).and_then(Param::value);
    match selector {
        Some(5) => match params.get(index + 2).and_then(Param::value) {
            Some(entry) => (format!("{role} palette entry {entry}"), 3),
            None => (format!("{role} palette entry, unstated"), 2),
        },
        Some(2) => {
            let component = |offset: usize| params.get(index + offset).and_then(Param::value);
            match (component(2), component(3), component(4)) {
                (Some(red), Some(green), Some(blue)) => {
                    (format!("{role} {}", rgb(red, green, blue)), 5)
                }
                _ => (
                    format!("{role} in 24-bit colour, with missing components"),
                    2,
                ),
            }
        }
        Some(0) => (format!("an implementation-defined {role} colour"), 2),
        Some(1) => (format!("a transparent {role}"), 2),
        Some(3) => (format!("{role} in CMY, which almost nothing implements"), 2),
        Some(4) => (
            format!("{role} in CMYK, which almost nothing implements"),
            2,
        ),
        Some(other) => (
            format!("{role} in colour space {other}, which is not in this table"),
            2,
        ),
        None => (
            format!("an extended {role} colour with no colour space given"),
            1,
        ),
    }
}

/// The sub-parameter spelling: `38:5:173`, `38:2::1:2:3`, `38:2:1:2:3`.
fn colon_colour(role: &str, param: &Param) -> String {
    match param.sub(1) {
        Some(5) => match param.sub(2) {
            Some(entry) => format!("{role} palette entry {entry}"),
            None => format!("{role} palette entry, unstated"),
        },
        Some(2) => {
            // Six fields is the specified form, with the colour space left empty;
            // five is the same thing with the empty field dropped, which is
            // common enough that refusing to read it would be unhelpful.
            let start = if param.len() >= 6 { 3 } else { 2 };
            match (param.sub(start), param.sub(start + 1), param.sub(start + 2)) {
                (Some(red), Some(green), Some(blue)) => {
                    format!("{role} {}", rgb(red, green, blue))
                }
                _ => format!("{role} in 24-bit colour, with missing components"),
            }
        }
        Some(0) => format!("an implementation-defined {role} colour"),
        Some(1) => format!("a transparent {role}"),
        Some(other) => format!("{role} in colour space {other}, which is not in this table"),
        None => format!("an extended {role} colour with no colour space given"),
    }
}

/// A colour as both decimal components and the hex a designer would recognise.
fn rgb(red: u32, green: u32, blue: u32) -> String {
    if red > 255 || green > 255 || blue > 255 {
        return format!("rgb({red}, {green}, {blue}), whose components are out of range");
    }
    format!("rgb({red}, {green}, {blue}) — #{red:02x}{green:02x}{blue:02x}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        super::parse::{Item, Outcome, parse_item},
        *,
    };

    /// The parameters of an `SGR` sequence.
    fn params(bytes: &[u8]) -> Vec<Param> {
        match parse_item(bytes, true) {
            Outcome::Item {
                item: Item::Csi(fields),
                ..
            } => fields.params,
            other => panic!("expected a CSI, got {other:?}"),
        }
    }

    fn described(bytes: &[u8]) -> String {
        summary(&params(bytes))
    }

    #[test]
    fn the_line_from_the_brief() {
        assert_eq!(described(b"\x1b[1;31m"), "bold, foreground red");
        assert_eq!(described(b"\x1b[0m"), "reset all attributes");
    }

    #[test]
    fn a_bare_sgr_is_a_reset() {
        // `CSI m` is easy to misread as doing nothing at all.
        assert_eq!(described(b"\x1b[m"), "reset all attributes");
        assert_eq!(described(b"\x1b[;1m"), "reset all attributes, bold");
    }

    #[test]
    fn the_basic_colours_and_their_bright_forms() {
        assert_eq!(described(b"\x1b[30m"), "foreground black");
        assert_eq!(described(b"\x1b[37m"), "foreground white");
        assert_eq!(described(b"\x1b[44m"), "background blue");
        assert_eq!(described(b"\x1b[91m"), "foreground bright red");
        assert_eq!(described(b"\x1b[107m"), "background bright white");
        assert_eq!(
            described(b"\x1b[39;49m"),
            "the default foreground, the default background"
        );
    }

    #[test]
    fn every_attribute_under_thirty_is_named() {
        for code in 0u32..=29 {
            let sequence = format!("\u{1b}[{code}m");
            let text = described(sequence.as_bytes());
            assert!(
                !text.contains("not in this table"),
                "SGR {code} is unnamed: {text}"
            );
        }
    }

    #[test]
    fn underline_sub_parameters_name_the_shape() {
        assert_eq!(described(b"\x1b[4m"), "underline");
        assert_eq!(described(b"\x1b[4:3m"), "a curly underline");
        assert_eq!(described(b"\x1b[4:0m"), "not underlined");
        assert_eq!(described(b"\x1b[4:5m"), "a dashed underline");
        assert!(described(b"\x1b[4:9m").contains("not in this table"));
    }

    #[test]
    fn the_specified_spelling_of_a_24_bit_colour() {
        // `38:2::1:2:3` — the colour space is deliberately empty.
        assert_eq!(
            described(b"\x1b[38:2::1:2:3m"),
            "foreground rgb(1, 2, 3) — #010203"
        );
    }

    #[test]
    fn the_common_spelling_of_a_24_bit_colour() {
        assert_eq!(
            described(b"\x1b[38;2;1;2;3m"),
            "foreground rgb(1, 2, 3) — #010203"
        );
        assert_eq!(
            described(b"\x1b[48;2;255;128;0m"),
            "background rgb(255, 128, 0) — #ff8000"
        );
        // And the four-field colon form, which drops the empty colour space.
        assert_eq!(
            described(b"\x1b[38:2:1:2:3m"),
            "foreground rgb(1, 2, 3) — #010203"
        );
    }

    #[test]
    fn a_semicolon_colour_swallows_its_arguments_and_nothing_more() {
        // The components must not be described again as attributes of their own:
        // `2`, `1`, `2` and `3` would otherwise read as dim, bold, dim, italic.
        assert_eq!(
            described(b"\x1b[1;38;2;1;2;3;4m"),
            "bold, foreground rgb(1, 2, 3) — #010203, underline"
        );
        assert_eq!(
            described(b"\x1b[38;5;173;1m"),
            "foreground palette entry 173, bold"
        );
    }

    #[test]
    fn a_palette_colour_in_either_spelling() {
        assert_eq!(described(b"\x1b[38;5;173m"), "foreground palette entry 173");
        assert_eq!(described(b"\x1b[38:5:173m"), "foreground palette entry 173");
        assert_eq!(described(b"\x1b[48:5:0m"), "background palette entry 0");
    }

    #[test]
    fn an_underline_colour_is_the_third_independent_colour() {
        assert_eq!(
            described(b"\x1b[58:2::255:0:0m"),
            "underline rgb(255, 0, 0) — #ff0000"
        );
        assert_eq!(described(b"\x1b[59m"), "the default underline colour");
    }

    #[test]
    fn a_truncated_extended_colour_says_what_is_missing() {
        assert!(described(b"\x1b[38m").contains("no colour space"));
        assert!(described(b"\x1b[38;2;1m").contains("missing components"));
        assert!(described(b"\x1b[38;5m").contains("unstated"));
    }

    #[test]
    fn sgr_21_reports_both_readings_because_the_bytes_cannot_settle_it() {
        let text = described(b"\x1b[21m");
        assert!(text.contains("doubly underlined"), "{text}");
        assert!(text.contains("bold off"), "{text}");
    }

    #[test]
    fn an_unknown_attribute_is_named_as_unknown_rather_than_guessed_at() {
        let text = described(b"\x1b[200m");
        assert!(text.contains("200"), "{text}");
        assert!(text.contains("not in this table"), "{text}");
    }

    #[test]
    fn an_out_of_range_component_is_reported_not_wrapped() {
        assert!(described(b"\x1b[38;2;300;0;0m").contains("out of range"));
    }

    #[test]
    fn a_long_real_world_sequence_reads_as_a_sentence() {
        // What a syntax highlighter emits: bold, italic, curly red underline.
        assert_eq!(
            described(b"\x1b[1;3;4:3;58:2::255:0:0;38;5;12m"),
            "bold, italic, a curly underline, underline rgb(255, 0, 0) — #ff0000, \
             foreground palette entry 12"
        );
    }

    #[test]
    fn no_phrase_contains_a_control_byte() {
        for sequence in [
            &b"\x1b[0m"[..],
            b"\x1b[1;31m",
            b"\x1b[38:2::1:2:3m",
            b"\x1b[4:3m",
            b"\x1b[200m",
        ] {
            let text = described(sequence);
            assert!(!text.chars().any(char::is_control), "{text:?}");
        }
    }
}
