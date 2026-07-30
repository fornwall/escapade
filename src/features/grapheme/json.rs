//! Just enough JSON to emit `--format json`, without a dependency.
//!
//! The crate has `serde` for reading TOML but no `serde_json`, and this output is
//! a handful of flat objects, so it is written by hand.
//!
//! Escaping is deliberately aggressive: every non-ASCII character becomes a
//! `\uXXXX` escape rather than being passed through as UTF-8. That is valid JSON,
//! it survives any consumer's encoding assumptions, and — the reason it matters
//! here — it makes it impossible for this subcommand to emit a raw control byte
//! while describing sequences whose whole point is that they contain invisible
//! codepoints.

use std::fmt::Write as _;

/// A JSON value, already encoded.
///
/// A newtype rather than a bare `String` so a caller cannot accidentally pass
//  unescaped text where a value is expected.
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
            // Printable ASCII passes through; everything else — controls,
            // combining marks, joiners, astral emoji — is escaped.
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

/// JSON `null`, for a measurement that did not happen.
///
/// Never `0`: a silence is not a zero, and a consumer must be able to tell them
/// apart as easily as a reader of the text output can.
#[must_use]
pub fn null() -> Value {
    Value("null".to_owned())
}

/// A number, or `null` when there is nothing to report.
#[must_use]
pub fn optional_number(value: Option<usize>) -> Value {
    match value {
        Some(value) => number(value),
        None => null(),
    }
}

/// A string, or `null`.
#[must_use]
pub fn optional_string(value: Option<&str>) -> Value {
    match value {
        Some(value) => string(value),
        None => null(),
    }
}

/// An array of already-encoded values.
#[must_use]
pub fn array(items: &[Value]) -> Value {
    let joined: Vec<&str> = items.iter().map(Value::as_str).collect();
    Value(format!("[{}]", joined.join(",")))
}

/// An object, with keys in the order given.
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plain_strings_are_quoted_and_unchanged() {
        assert_eq!(string("hello").as_str(), r#""hello""#);
        assert_eq!(string("").as_str(), r#""""#);
        assert_eq!(string("a b~c").as_str(), r#""a b~c""#);
    }

    #[test]
    fn json_specials_are_escaped() {
        assert_eq!(string("say \"hi\"").as_str(), r#""say \"hi\"""#);
        assert_eq!(string("a\\b").as_str(), r#""a\\b""#);
        assert_eq!(string("a\nb").as_str(), r#""a\nb""#);
        assert_eq!(string("a\tb").as_str(), r#""a\tb""#);
    }

    #[test]
    fn control_bytes_never_survive_into_the_output() {
        // The whole reason this module escapes so eagerly. A raw ESC in JSON
        // output would still be executed by whatever terminal displayed it.
        for character in ['\u{1b}', '\u{7}', '\u{0}', '\u{9b}'] {
            let encoded = string(&character.to_string());
            assert!(!encoded.as_str().contains(character), "{encoded:?}");
            assert!(
                !encoded.as_str().chars().any(char::is_control),
                "{encoded:?}"
            );
        }
        assert_eq!(string("\u{1b}").as_str(), "\"\\u001b\"");
    }

    #[test]
    fn invisible_codepoints_are_visible_in_the_output() {
        // A ZWJ passed through as UTF-8 would make the JSON unreviewable, which
        // is the same problem the data files have.
        assert_eq!(string("\u{200d}").as_str(), "\"\\u200d\"");
        assert_eq!(string("\u{fe0f}").as_str(), "\"\\ufe0f\"");
    }

    #[test]
    fn astral_codepoints_become_surrogate_pairs() {
        // U+1F9D1 is outside the BMP, so JSON spells it with two \u escapes.
        assert_eq!(string("\u{1f9d1}").as_str(), "\"\\ud83e\\uddd1\"");
    }

    #[test]
    fn a_whole_zwj_sequence_round_trips_as_escapes() {
        assert_eq!(
            string("\u{1f9d1}\u{200d}\u{1f33e}").as_str(),
            "\"\\ud83e\\uddd1\\u200d\\ud83c\\udf3e\""
        );
    }

    #[test]
    fn absent_measurements_are_null_not_zero() {
        assert_eq!(optional_number(None).as_str(), "null");
        assert_eq!(optional_number(Some(0)).as_str(), "0");
        assert_eq!(optional_string(None).as_str(), "null");
        assert_eq!(optional_string(Some("x")).as_str(), r#""x""#);
    }

    #[test]
    fn objects_keep_their_field_order() {
        let encoded = object(&[
            ("verdict", string("clustered")),
            ("expected", number(2)),
            ("measured", null()),
            ("decisive", boolean(true)),
        ]);
        assert_eq!(
            encoded.as_str(),
            r#"{"verdict":"clustered","expected":2,"measured":null,"decisive":true}"#
        );
    }

    #[test]
    fn arrays_nest() {
        let encoded = array(&[
            object(&[("cp", string("U+0041"))]),
            object(&[("cp", string("U+200D"))]),
        ]);
        assert_eq!(encoded.as_str(), r#"[{"cp":"U+0041"},{"cp":"U+200D"}]"#);
        assert_eq!(array(&[]).as_str(), "[]");
    }

    #[test]
    fn every_emitted_object_is_one_line() {
        // `--format json` is documented as one object per line, so a newline
        // inside a value would break a consumer reading line by line.
        let encoded = object(&[("text", string("a\nb")), ("items", array(&[number(1)]))]);
        assert!(!encoded.as_str().contains('\n'), "{encoded:?}");
    }
}
