//! Source two, part one: XTGETTCAP — asking the terminal for a capability.
//!
//! ```text
//! →  DCS + q <hex-name> ST                    what is your `RGB`?
//! ←  DCS 1 + r <hex-name> = <hex-value> ST    this
//! ←  DCS 0 + r ST                             I have no such capability
//! ```
//!
//! Names and values are hex, two digits per byte, and several name/value pairs
//! may share one reply separated by `;`.
//!
//! This is the only source with a genuine three-state answer, and that is what
//! makes it worth diffing against terminfo. `1 + r` is a yes, `0 + r` is a
//! terminal explicitly disowning a capability its terminfo entry may well claim,
//! and silence is neither — it means the terminal does not implement XTGETTCAP,
//! which most do not.
//!
//! # Why names are not batched
//!
//! xterm's own documentation for XTGETTCAP says that "an invalid name (one not
//! found in xterm's tables) ends processing of the list of names". So a batch of
//! five names where the second is unknown silently loses the last three, and they
//! come back as silence — indistinguishable from a terminal that does not
//! implement the query at all. One name per query costs a few more bytes and
//! removes the failure mode entirely.
//!
//! Verified against xterm's `ctlseqs` (`DCS + q Pt ST`, Request
//! Termcap/Terminfo String).

use crate::term::{DCS, ST};

/// One name/value pair out of a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The capability name, hex-decoded.
    pub name: String,
    /// The value, hex-decoded. `None` when the reply carried a bare name with no
    /// `=`, which is how a boolean capability comes back.
    pub value: Option<String>,
}

/// A parsed XTGETTCAP reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// `DCS 1 + r …` — the terminal has these capabilities.
    Found(Vec<Entry>),
    /// `DCS 0 + r …` — the terminal has no such capability. Some terminals echo
    /// the names back, some send nothing after the `r`, so the list may be empty.
    NotFound(Vec<String>),
}

/// Asks the terminal for a capability: `DCS + q <hex> ST`.
///
/// One name per query, deliberately — see the module documentation.
#[must_use]
pub fn query(name: &str) -> String {
    format!("{DCS}+q{}{ST}", encode(name))
}

/// Hex-encodes a capability name, two uppercase digits per byte.
///
/// Non-ASCII is not possible in a capability name, but the encoding is defined
/// over bytes rather than characters so that a name containing one is still
/// encoded rather than silently dropped.
#[must_use]
pub fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len() * 2);
    for byte in name.as_bytes() {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    out
}

/// Decodes a hex string, or `None` if it is not one.
///
/// Odd length, a stray non-hex digit, or bytes that are not valid UTF-8 all fail:
/// a capability value that cannot be decoded is not a value, and guessing at one
/// would put invented text in a report about lying.
#[must_use]
pub fn decode(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        out.push(high << 4 | low);
    }
    String::from_utf8(out).ok()
}

/// One hex digit from a nibble.
fn hex_digit(nibble: u8) -> char {
    char::from(match nibble {
        0..=9 => b'0' + nibble,
        _ => b'A' + (nibble - 10),
    })
}

/// One nibble from a hex digit, either case.
fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Parses a DCS payload as an XTGETTCAP reply.
///
/// `payload` is everything between `ESC P` and the string terminator, exactly as
/// [`crate::input::Reply::Dcs`] delivers it — so `1+r524742=31` for a successful
/// `RGB` query.
///
/// Returns `None` for a DCS payload that is not an XTGETTCAP reply at all, which
/// matters because DECRQSS and XTVERSION answer on the same stream.
#[must_use]
pub fn parse(payload: &str) -> Option<Reply> {
    if let Some(rest) = payload.strip_prefix("1+r") {
        return parse_found(rest);
    }
    // The failure form is documented as `DCS 0 + r ST` with nothing after the
    // `r`, but terminals in the field echo the requested name back, so both
    // shapes are accepted.
    let rest = payload.strip_prefix("0+r")?;
    let names = rest
        .split(';')
        .filter(|item| !item.is_empty())
        .map(|item| decode(item).unwrap_or_else(|| item.to_owned()))
        .collect();
    Some(Reply::NotFound(names))
}

/// Parses the payload of a `DCS 1 + r … ST` success reply.
fn parse_found(rest: &str) -> Option<Reply> {
    let mut entries = Vec::new();
    for item in rest.split(';') {
        if item.is_empty() {
            continue;
        }
        let (name_hex, value_hex) = match item.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (item, None),
        };
        // A name that is not hex means this is not the reply we think it is.
        let name = decode(name_hex)?;
        if name.is_empty() {
            return None;
        }
        let value = match value_hex {
            // An empty value is a real answer — a boolean capability comes back
            // as `<name>=` on some terminals — so it stays `Some("")`.
            Some(hex) => Some(decode(hex)?),
            None => None,
        };
        entries.push(Entry { name, value });
    }
    if entries.is_empty() {
        return None;
    }
    Some(Reply::Found(entries))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_shielded_round_of_queries_leaves_no_trace_on_a_terminal_that_prints_dcs() {
        // One name per query means a round is a long string of them, and a
        // terminal with no state for `ESC P` prints every body: `+q524742+q...`.
        // `Session::round` sends the batch through the same shield.
        use crate::term::{
            screen::{Screen, Strings},
            shielded,
        };
        let queries: String = ["RGB", "Tc", "setrgbf", "Su", "hpa"]
            .iter()
            .map(|name| query(name))
            .collect();
        let mut screen = Screen::new(80, 24, Strings::Printed);
        screen.goto(23, 0);
        let before = screen.text();
        screen.feed(&shielded(&queries));
        assert_eq!(screen.text(), before, "the queries were left on the screen");
        assert_eq!(screen.cursor(), (23, 0));
        assert_eq!(screen.scrolled(), 0);
    }

    #[test]
    fn the_query_matches_the_specification() {
        // DCS + q 524742 ST, where 524742 is "RGB".
        assert_eq!(query("RGB"), "\u{1b}P+q524742\u{1b}\\");
        assert_eq!(query("Tc"), "\u{1b}P+q5463\u{1b}\\");
        assert_eq!(query("Co"), "\u{1b}P+q436F\u{1b}\\");
    }

    #[test]
    fn hex_encoding_is_two_digits_per_byte() {
        assert_eq!(encode("RGB"), "524742");
        assert_eq!(encode("smcup"), "736D637570");
        assert_eq!(encode(""), "");
        // Every byte gets two digits, including one whose high nibble is zero.
        assert_eq!(encode("\u{1}"), "01");
    }

    #[test]
    fn hex_encoding_round_trips() {
        for name in [
            "RGB", "Tc", "Co", "smcup", "rmcup", "Ms", "BE", "kbs", "TN", "Sync",
        ] {
            assert_eq!(decode(&encode(name)).as_deref(), Some(name), "{name}");
        }
    }

    #[test]
    fn hex_decoding_accepts_either_case() {
        assert_eq!(decode("524742").as_deref(), Some("RGB"));
        assert_eq!(decode("524742").as_deref(), decode("524742").as_deref());
        assert_eq!(decode("436f").as_deref(), Some("Co"));
        assert_eq!(decode("436F").as_deref(), Some("Co"));
    }

    #[test]
    fn hex_decoding_refuses_what_is_not_hex() {
        assert_eq!(decode("52474"), None, "odd length");
        assert_eq!(decode("52zz42"), None, "not a hex digit");
        assert_eq!(decode("ff"), None, "not valid UTF-8");
        assert_eq!(decode("").as_deref(), Some(""));
    }

    #[test]
    fn parses_a_successful_reply() {
        // DCS 1 + r 524742 = 38 ST — RGB, value "8".
        let reply = parse("1+r524742=38").unwrap();
        assert_eq!(
            reply,
            Reply::Found(vec![Entry {
                name: "RGB".to_owned(),
                value: Some("8".to_owned())
            }])
        );
    }

    #[test]
    fn parses_the_failure_form() {
        // The documented shape has nothing after the `r`.
        assert_eq!(parse("0+r").unwrap(), Reply::NotFound(Vec::new()));
        // Terminals in the field echo the name back instead.
        assert_eq!(
            parse("0+r524742").unwrap(),
            Reply::NotFound(vec!["RGB".to_owned()])
        );
        assert_eq!(
            parse("0+r5463;524742").unwrap(),
            Reply::NotFound(vec!["Tc".to_owned(), "RGB".to_owned()])
        );
    }

    #[test]
    fn a_failure_form_with_an_undecodable_name_keeps_the_raw_text() {
        // The point of a `0 + r` is that the terminal said no; losing the reply
        // because its echo is malformed would turn a definite no into silence.
        assert_eq!(
            parse("0+rnothex").unwrap(),
            Reply::NotFound(vec!["nothex".to_owned()])
        );
    }

    #[test]
    fn parses_several_pairs_in_one_reply() {
        // xterm "replies with each name/value pair in the same response".
        let reply = parse("1+r524742=38;5463=31").unwrap();
        assert_eq!(
            reply,
            Reply::Found(vec![
                Entry {
                    name: "RGB".to_owned(),
                    value: Some("8".to_owned())
                },
                Entry {
                    name: "Tc".to_owned(),
                    value: Some("1".to_owned())
                },
            ])
        );
    }

    #[test]
    fn a_bare_name_is_a_boolean_capability() {
        // No `=` at all: the capability is present and has no value.
        let reply = parse("1+r5463").unwrap();
        assert_eq!(
            reply,
            Reply::Found(vec![Entry {
                name: "Tc".to_owned(),
                value: None
            }])
        );
    }

    #[test]
    fn an_empty_value_is_still_an_answer() {
        // `<name>=` with nothing after it is how some terminals spell a boolean,
        // and it must not be confused with a bare name or with silence.
        let reply = parse("1+r5463=").unwrap();
        assert_eq!(
            reply,
            Reply::Found(vec![Entry {
                name: "Tc".to_owned(),
                value: Some(String::new())
            }])
        );
    }

    #[test]
    fn a_value_containing_control_bytes_decodes_verbatim() {
        // `Ms` is `OSC 52 ; …`, so its value starts with a real escape byte. The
        // decoder must not sanitise; the report is responsible for that.
        let hex = encode("\u{1b}]52;%p1%s;%p2%s\u{7}");
        let reply = parse(&format!("1+r4D73={hex}")).unwrap();
        let Reply::Found(entries) = reply else {
            panic!("expected a successful reply");
        };
        assert_eq!(entries[0].name, "Ms");
        assert!(entries[0].value.as_deref().unwrap().contains('\u{1b}'));
    }

    #[test]
    fn ignores_a_dcs_payload_that_is_not_xtgettcap() {
        // DECRQSS and XTVERSION answer on the same stream.
        assert_eq!(parse("1$r0;38:2::1:2:3m"), None);
        assert_eq!(parse(">|xterm(400)"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("1+q524742"), None, "the verb is `r` in a reply");
    }

    #[test]
    fn refuses_a_successful_reply_whose_name_is_not_hex() {
        // Accepting this would put invented capability names in the report.
        assert_eq!(parse("1+rRGB=8"), None);
        assert_eq!(parse("1+r524742=zz"), None);
        assert_eq!(parse("1+r"), None, "a success with nothing in it");
        assert_eq!(parse("1+r="), None, "a success with no name");
    }
}
