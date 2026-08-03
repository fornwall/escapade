//! `--format json`: one object per item, hand-written.
//!
//! The crate has `serde` for reading TOML and no JSON serialiser, and adding one
//! for this would be a dependency for a hundred lines of formatting. So this is
//! written by hand, following `glyph_protocol::probe`'s escaping and
//! `grapheme::json`'s builder shape.
//!
//! Escaping is deliberately aggressive: everything outside printable ASCII becomes
//! a `\uXXXX` escape. That is valid JSON, it survives any consumer's encoding
//! assumptions, and — the reason it matters here — it makes it impossible for a
//! subcommand whose whole subject is control bytes to emit one. A decode of a
//! hostile log line is safe to `cat`.
//!
//! One object per line, so `escapade decode --format json | jq 'select(.kind ==
//! "osc")'` works on a stream as it arrives.

use std::fmt::Write as _;

use super::{
    Options, annotate,
    describe::Description,
    display::HEX_LIMIT,
    parse::{Decoded, Fields, Item, StringKind},
    sgr,
};
use crate::unicode::CharInfo;

/// A JSON value, already encoded.
///
/// A newtype rather than a bare `String`, so unescaped text cannot be passed where
/// a value belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value(String);

impl Value {
    /// The encoded text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A JSON string, quoted and escaped.
#[must_use]
pub fn string(text: &str) -> Value {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Printable ASCII passes through; controls, joiners and astral emoji
            // are escaped.
            ' '..='~' => out.push(character),
            other => {
                let mut buffer = [0u16; 2];
                for unit in other.encode_utf16(&mut buffer) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
    Value(out)
}

/// A JSON number.
#[must_use]
pub fn number(value: usize) -> Value {
    Value(value.to_string())
}

/// A JSON boolean.
#[must_use]
pub fn boolean(value: bool) -> Value {
    Value(if value { "true" } else { "false" }.to_owned())
}

/// JSON `null`, for something that is absent rather than empty.
#[must_use]
pub fn null() -> Value {
    Value("null".to_owned())
}

/// A string, or `null` when there is nothing to say.
#[must_use]
pub fn optional_string(value: Option<&str>) -> Value {
    match value {
        Some(text) => string(text),
        None => null(),
    }
}

/// An array of already-encoded values.
#[must_use]
pub fn array(items: &[Value]) -> Value {
    let joined: Vec<&str> = items.iter().map(Value::as_str).collect();
    Value(format!("[{}]", joined.join(",")))
}

/// An object, keys in the order given.
#[must_use]
pub fn object(fields: &[(&str, Value)]) -> Value {
    let mut out = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(string(key).as_str());
        out.push(':');
        out.push_str(value.as_str());
    }
    out.push('}');
    Value(out)
}

/// One item as one JSON object.
///
/// Every object carries the common fields — where the item is, what class it is,
/// how it reads — and then whatever its class has to add: parameters for a `CSI`,
/// a payload for an `OSC`, codepoints for a run of text under `--unicode`.
#[must_use]
pub fn item(decoded: &Decoded, description: &Description, options: Options) -> Value {
    let mut fields: Vec<(&str, Value)> = vec![
        ("kind", string(description.kind)),
        ("offset", number(decoded.offset)),
        ("length", number(decoded.bytes.len())),
        ("sequence", string(&description.sequence)),
        ("label", optional_string(description.label.as_deref())),
        ("summary", optional_string(description.summary.as_deref())),
        ("problem", optional_string(description.problem.as_deref())),
        (
            "notes",
            array(
                &description
                    .notes
                    .iter()
                    .map(|note| string(note))
                    .collect::<Vec<Value>>(),
            ),
        ),
        ("features", features(description)),
        ("description", string(&description.prose())),
    ];

    fields.extend(specifics(&decoded.item, options));

    if options.hex {
        fields.push((
            "hex",
            string(&super::display::hex(&decoded.bytes, HEX_LIMIT)),
        ));
        fields.push(("hex_complete", boolean(decoded.bytes.len() <= HEX_LIMIT)));
    }

    object(&fields)
}

/// One item as one line of JSON Lines.
#[must_use]
pub fn line(decoded: &Decoded, description: &Description, options: Options) -> String {
    let mut out = item(decoded, description, options).0;
    out.push('\n');
    out
}

/// What the database says, as an array of objects.
fn features(description: &Description) -> Value {
    let entries: Vec<Value> = description
        .features
        .iter()
        .map(|found| {
            object(&[
                ("id", string(&found.feature_id)),
                ("name", string(&found.feature_name)),
                ("sequence", string(&found.sequence_name)),
                ("purpose", string(&found.purpose)),
                ("verb", optional_string(found.verb.as_deref())),
            ])
        })
        .collect();
    array(&entries)
}

/// The fields that only one class of item has.
fn specifics(item: &Item, options: Options) -> Vec<(&'static str, Value)> {
    match item {
        Item::Text(text) => {
            let mut fields = vec![("text", string(text))];
            if options.unicode {
                let analysis = annotate::analyse(text);
                fields.push(("codepoints", number(analysis.codepoints)));
                fields.push(("width", number(analysis.width)));
                fields.push(("clusters", clusters(&analysis)));
            }
            fields
        }
        Item::Control(byte) => vec![
            ("byte", number(usize::from(*byte))),
            ("name", string(super::names::control(*byte).mnemonic)),
        ],
        Item::Csi(fields) => {
            let mut out = control_fields(fields);
            if fields.private.is_none() && fields.final_byte == b'm' {
                out.push((
                    "attributes",
                    array(
                        &sgr::attributes(&fields.params)
                            .iter()
                            .map(|attribute| string(attribute))
                            .collect::<Vec<Value>>(),
                    ),
                ));
            }
            out
        }
        Item::Dcs {
            fields,
            payload,
            terminator,
        } => {
            let mut out = control_fields(fields);
            out.push(("payload", string(&super::display::payload_text(payload))));
            out.push(("payload_bytes", number(payload.len())));
            out.push(("terminator", string(terminator.name())));
            out
        }
        Item::StringSeq {
            kind,
            payload,
            terminator,
        } => {
            let text = super::display::payload_text(payload);
            let mut out = vec![
                ("payload", string(&text)),
                ("payload_bytes", number(payload.len())),
                ("terminator", string(terminator.name())),
            ];
            if matches!(kind, StringKind::Osc) {
                out.push((
                    "fields",
                    array(&text.split(';').map(string).collect::<Vec<Value>>()),
                ));
            }
            out
        }
        Item::SingleShift { level, byte } => vec![
            ("level", number(usize::from(*level))),
            ("byte", number(usize::from(*byte))),
        ],
        Item::Esc {
            intermediates,
            final_byte,
        } => vec![
            (
                "intermediates",
                string(
                    &intermediates
                        .iter()
                        .map(|&b| char::from(b))
                        .collect::<String>(),
                ),
            ),
            ("final", string(&char::from(*final_byte).to_string())),
        ],
        Item::Unterminated { .. } | Item::Malformed { .. } => Vec::new(),
    }
}

/// The parameter fields shared by `CSI` and `DCS`.
fn control_fields(fields: &Fields) -> Vec<(&'static str, Value)> {
    vec![
        (
            "private",
            optional_string(
                fields
                    .private
                    .map(|byte| char::from(byte).to_string())
                    .as_deref(),
            ),
        ),
        (
            "params",
            array(
                &parameter_texts(fields)
                    .iter()
                    .map(|param| string(param))
                    .collect::<Vec<Value>>(),
            ),
        ),
        (
            "params_numeric",
            array(
                &fields
                    .params
                    .iter()
                    .map(|param| match param.value() {
                        Some(value) => unsigned(value),
                        None => null(),
                    })
                    .collect::<Vec<Value>>(),
            ),
        ),
        ("intermediates", string(&fields.intermediates_text())),
        ("final", string(&fields.final_char().to_string())),
    ]
}

/// The parameters exactly as written, so a consumer sees `4:3` rather than a
/// flattened `4`.
fn parameter_texts(fields: &Fields) -> Vec<String> {
    if fields.raw.is_empty() {
        return Vec::new();
    }
    fields.raw.split(';').map(str::to_owned).collect()
}

/// The clusters of a text run, for `--unicode`.
fn clusters(analysis: &annotate::Analysis) -> Value {
    let entries: Vec<Value> = analysis
        .clusters
        .iter()
        .map(|cluster| {
            object(&[
                ("text", string(&cluster.text)),
                ("width", number(cluster.width)),
                (
                    "codepoints",
                    array(
                        &cluster
                            .codepoints
                            .iter()
                            .map(codepoint)
                            .collect::<Vec<Value>>(),
                    ),
                ),
            ])
        })
        .collect();
    array(&entries)
}

/// One codepoint, as `--unicode` describes it.
fn codepoint(info: &CharInfo) -> Value {
    object(&[
        ("codepoint", string(&info.label())),
        ("name", string(&info.title_name())),
        ("named", boolean(info.named)),
        ("category", string(annotate::category(info))),
        ("width", number(info.width)),
        ("emoji", boolean(info.emoji)),
    ])
}

/// A `u32` as a JSON number, without a cast through `usize`.
fn unsigned(value: u32) -> Value {
    Value(value.to_string())
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

    /// The JSON lines a stream produces.
    fn lines(bytes: &[u8], options: Options) -> Vec<String> {
        let catalog = Catalog::load().unwrap();
        let index = Index::from_catalog(&catalog);
        decode_all(bytes)
            .iter()
            .map(|decoded| line(decoded, &describe(decoded, &index), options))
            .collect()
    }

    /// The value of a top-level key, found textually — enough for a test, and it
    /// keeps this module free of a JSON *parser* as well as of a serialiser.
    fn field(line: &str, key: &str) -> String {
        let needle = format!("\"{key}\":");
        let start = line
            .find(&needle)
            .unwrap_or_else(|| panic!("no {key} in {line}"))
            + needle.len();
        let rest = &line[start..];
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        for (index, character) in rest.char_indices() {
            if in_string {
                match character {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match character {
                '"' => in_string = true,
                '[' | '{' => depth += 1,
                ']' | '}' if depth > 0 => depth -= 1,
                ',' | '}' if depth == 0 => return rest[..index].to_owned(),
                _ => {}
            }
        }
        rest.to_owned()
    }

    #[test]
    fn plain_strings_are_quoted_and_specials_escaped() {
        assert_eq!(string("hello").as_str(), r#""hello""#);
        assert_eq!(string("say \"hi\"").as_str(), r#""say \"hi\"""#);
        assert_eq!(string("a\\b").as_str(), r#""a\\b""#);
        assert_eq!(string("a\nb").as_str(), r#""a\nb""#);
    }

    #[test]
    fn no_control_byte_survives_into_the_output() {
        for character in ['\u{1b}', '\u{7}', '\u{0}', '\u{9b}'] {
            let encoded = string(&character.to_string());
            assert!(!encoded.as_str().contains(character), "{encoded:?}");
        }
        assert_eq!(string("\u{1b}").as_str(), "\"\\u001b\"");
    }

    #[test]
    fn astral_codepoints_become_surrogate_pairs() {
        assert_eq!(string("\u{1f468}").as_str(), "\"\\ud83d\\udc68\"");
    }

    #[test]
    fn one_object_per_item_and_one_line_each() {
        let lines = lines(b"\x1b[1;31mred\x1b[0m", options());
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(line.matches('\n').count(), 1, "{line:?}");
            assert!(line.ends_with('\n'));
            assert!(line.starts_with('{'));
        }
    }

    #[test]
    fn the_common_fields_are_present_and_correct() {
        let lines = lines(b"\x1b[1;31mred", options());
        assert_eq!(field(&lines[0], "kind"), "\"csi\"");
        assert_eq!(field(&lines[0], "offset"), "0");
        assert_eq!(field(&lines[0], "length"), "7");
        assert_eq!(field(&lines[0], "sequence"), "\"CSI 1;31 m\"");
        assert_eq!(field(&lines[0], "label"), "\"SGR\"");
        assert_eq!(field(&lines[0], "summary"), "\"bold, foreground red\"");
        assert_eq!(field(&lines[0], "problem"), "null");
        assert_eq!(field(&lines[1], "kind"), "\"text\"");
        assert_eq!(field(&lines[1], "offset"), "7");
        assert_eq!(field(&lines[1], "text"), "\"red\"");
    }

    #[test]
    fn csi_parameters_keep_their_sub_parameters() {
        let lines = lines(b"\x1b[38:2::1:2:3m", options());
        assert_eq!(field(&lines[0], "params"), "[\"38:2::1:2:3\"]");
        assert!(field(&lines[0], "attributes").contains("#010203"));
    }

    #[test]
    fn the_private_marker_and_intermediates_are_separate_fields() {
        let lines = lines(b"\x1b[?2027$p", options());
        assert_eq!(field(&lines[0], "private"), "\"?\"");
        assert_eq!(field(&lines[0], "intermediates"), "\"$\"");
        assert_eq!(field(&lines[0], "final"), "\"p\"");
        assert_eq!(field(&lines[0], "params"), "[\"2027\"]");
    }

    #[test]
    fn an_osc_carries_its_payload_fields_and_feature() {
        let lines = lines(b"\x1b]8;;https://example.com\x1b\\", options());
        assert_eq!(field(&lines[0], "kind"), "\"osc\"");
        assert_eq!(field(&lines[0], "terminator"), "\"ST\"");
        assert_eq!(
            field(&lines[0], "fields"),
            "[\"8\",\"\",\"https://example.com\"]"
        );
        assert!(field(&lines[0], "features").contains("\"id\":\"hyperlinks\""));
    }

    #[test]
    fn a_selectable_kind_is_what_the_help_text_promises() {
        // `escapade decode --format json capture.bin | jq 'select(.kind == "osc")'`
        let lines = lines(b"text\x1b]0;t\x07\x1b[0m", options());
        let kinds: Vec<String> = lines.iter().map(|line| field(line, "kind")).collect();
        assert_eq!(kinds, vec!["\"text\"", "\"osc\"", "\"csi\""]);
    }

    #[test]
    fn problems_are_reported_in_their_own_field() {
        let lines = lines(b"\x1b]8;;broken", options());
        assert_eq!(field(&lines[0], "kind"), "\"unterminated\"");
        assert!(field(&lines[0], "problem").contains("unterminated OSC"));
        assert_eq!(field(&lines[0], "summary"), "null");
    }

    #[test]
    fn hex_is_added_only_when_asked_for() {
        let plain = lines(b"\x1b[0m", options());
        assert!(!plain[0].contains("\"hex\""));
        let with_hex = lines(
            b"\x1b[0m",
            Options {
                hex: true,
                ..options()
            },
        );
        assert_eq!(field(&with_hex[0], "hex"), "\"1b 5b 30 6d\"");
        assert_eq!(field(&with_hex[0], "hex_complete"), "true");
    }

    #[test]
    fn unicode_detail_is_added_only_when_asked_for() {
        let plain = lines("é".as_bytes(), options());
        assert!(!plain[0].contains("clusters"));
        let annotated = lines(
            "family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}".as_bytes(),
            Options {
                unicode: true,
                ..options()
            },
        );
        assert_eq!(field(&annotated[0], "codepoints"), "12");
        assert!(annotated[0].contains("\"U+200D\""));
        assert!(annotated[0].contains("Zero Width Joiner"));
    }

    #[test]
    fn every_line_is_free_of_control_bytes_and_of_raw_emoji() {
        let nasty = "\u{1b}]0;a\u{0}b\u{7}\u{1b}[1m\u{200d}text\u{1b}_G\u{1b}\\";
        for line in lines(
            nasty.as_bytes(),
            Options {
                hex: true,
                unicode: true,
                sequences_only: false,
            },
        ) {
            let body = line.trim_end_matches('\n');
            assert!(!body.chars().any(char::is_control), "{body:?}");
            assert!(body.is_ascii(), "{body:?}");
        }
    }
}
