//! Turning bytes into text that is safe to print.
//!
//! Everything this subcommand shows came from an untrusted byte stream, and the
//! whole point of showing it is that the bytes themselves are dangerous: writing
//! a real `ESC` while explaining what it does would execute it, and a hostile log
//! line is exactly the input someone runs `decode` on. So nothing reaches the
//! output without passing through here first.
//!
//! Two rules, both tested:
//!
//! * No control byte is ever printed as itself. [`crate::term::pretty`] names the
//!   introducers — `CSI`, `OSC`, `ST`, `BEL` — and [`escape`] catches every
//!   remaining control, which `pretty` passes through unchanged.
//! * Nothing invisible is printed as itself either. A zero-width joiner, a bidi
//!   override or a private-use codepoint becomes `\u{200d}`-style text, because a
//!   decoder that renders a right-to-left override faithfully has reversed the
//!   line it was trying to explain. This is the same rule the data files are held
//!   to, and the reason `glyph_protocol::supports::rendered` exists.

use std::fmt::Write as _;

use crate::{term, unicode};

/// How much of a payload a one-line summary shows before eliding the rest.
///
/// A kitty graphics `APC` carries kilobytes of base64 and a sixel `DCS` more than
/// that; the byte count and `--hex` are how those are inspected, not the summary
/// line.
pub const PAYLOAD_LIMIT: usize = 96;

/// How many bytes `--hex` shows for one item.
pub const HEX_LIMIT: usize = 32;

/// Whether a character may be printed as itself.
///
/// Controls, format characters (joiners, bidi overrides, soft hyphens),
/// private-use codepoints and unassigned codepoints all fail this: Unicode's
/// "other" general-category group is precisely the set that either does something
/// invisible or renders as tofu.
#[must_use]
pub fn showable(character: char) -> bool {
    !character.is_control() && unicode::category_group_name(character) != "other"
}

/// Replaces anything unprintable with `\u{…}`, leaving the rest alone.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if showable(character) {
            out.push(character);
        } else {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(character));
        }
    }
    out
}

/// Escapes `text` and quotes it, which is how a run of text appears in the
/// output.
#[must_use]
pub fn quote(text: &str) -> String {
    format!("\"{}\"", escape(text))
}

/// Escapes `text` and cuts it to `limit` characters, marking the cut.
#[must_use]
pub fn shorten(text: &str, limit: usize) -> String {
    let escaped = escape(text);
    if escaped.chars().count() <= limit {
        return escaped;
    }
    let kept: String = escaped.chars().take(limit).collect();
    format!("{kept}…")
}

/// Escapes, shortens and quotes a fragment quoted inside a description — a window
/// title, a URI, a notification body.
#[must_use]
pub fn quote_short(text: &str, limit: usize) -> String {
    format!("\"{}\"", shorten(text, limit))
}

/// A payload as text, with invalid bytes shown as the replacement character.
///
/// Control strings are text by convention and nothing enforces it, so this is
/// lossy on purpose; [`bytes_are_text`] says whether anything was lost.
#[must_use]
pub fn payload_text(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload).into_owned()
}

/// Bytes as text for display, with invalid bytes spelled `\xNN`.
///
/// Unlike [`payload_text`], nothing is lost: a stray `0x9b` shows as `\x9b`
/// rather than as a replacement character, which matters when the byte *is* the
/// finding. Control bytes are left in place for [`crate::term::pretty`] to name,
/// so this is always followed by [`escape`] — [`spell`] does both.
#[must_use]
pub fn recover(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(text) => {
                out.push_str(text);
                break;
            }
            Err(error) => {
                let good = error.valid_up_to();
                out.push_str(std::str::from_utf8(&rest[..good]).unwrap_or(""));
                // `None` means the input ended mid-scalar, so the rest is all bad.
                let bad = error.error_len().unwrap_or(rest.len() - good).max(1);
                for byte in &rest[good..(good + bad).min(rest.len())] {
                    let _ = write!(out, "\\x{byte:02x}");
                }
                rest = &rest[(good + bad).min(rest.len())..];
            }
        }
    }
    out
}

/// Whether a payload is valid UTF-8, so a caller can say when it is not.
#[must_use]
pub fn bytes_are_text(payload: &[u8]) -> bool {
    std::str::from_utf8(payload).is_ok()
}

/// Space-separated hex, up to `limit` bytes, with the remainder counted.
#[must_use]
pub fn hex(bytes: &[u8], limit: usize) -> String {
    let mut out = String::with_capacity(bytes.len().min(limit) * 3 + 16);
    for (index, byte) in bytes.iter().take(limit).enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{byte:02x}");
    }
    if bytes.len() > limit {
        let _ = write!(out, " … +{} bytes", bytes.len() - limit);
    }
    out
}

/// Spells a sequence out symbolically: real introducer bytes named by
/// [`crate::term::pretty`], the pieces spaced the way the data files write them,
/// and no control byte left raw.
///
/// `pieces` are joined with single spaces, empty ones dropped, so
/// `spell("\x1b[", &["?", "1049", "h"])` reads `CSI ? 1049 h` — the spelling the
/// feature database and `escapade explain` use.
#[must_use]
pub fn spell(introducer: &str, pieces: &[String]) -> String {
    let body: Vec<&str> = pieces
        .iter()
        .map(String::as_str)
        .filter(|piece| !piece.is_empty())
        .collect();
    // `pretty` names ESC, CSI, OSC, APC, DCS, ST and BEL and passes everything
    // else through, so it is followed by `escape` for whatever it did not name.
    escape(&term::pretty(&format!("{introducer}{}", body.join(" "))))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_text_passes_through() {
        assert_eq!(escape("hello, world"), "hello, world");
        assert_eq!(escape("naïve 日本語"), "naïve 日本語");
        assert_eq!(quote("red"), "\"red\"");
    }

    #[test]
    fn control_bytes_never_survive() {
        for byte in 0u8..0x20 {
            let text = char::from(byte).to_string();
            let escaped = escape(&text);
            assert!(
                !escaped.chars().any(char::is_control),
                "{byte:#04x} survived as {escaped:?}"
            );
        }
        assert_eq!(escape("\u{1b}"), "\\u{1b}");
        assert_eq!(escape("\u{7f}"), "\\u{7f}");
        assert_eq!(escape("\u{9b}"), "\\u{9b}");
    }

    #[test]
    fn invisible_characters_become_visible() {
        // A right-to-left override would otherwise reverse the very line that is
        // explaining it.
        assert_eq!(escape("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(escape("\u{200d}"), "\\u{200d}");
        assert_eq!(escape("\u{ad}"), "\\u{ad}");
        // Private use renders as tofu, so it is spelled out too.
        assert_eq!(escape("\u{e0a0}"), "\\u{e0a0}");
    }

    #[test]
    fn spell_names_the_introducers_the_way_the_database_writes_them() {
        assert_eq!(
            spell("\u{1b}[", &["?".to_owned(), "1049".to_owned(), "h".to_owned()]),
            "CSI ? 1049 h"
        );
        assert_eq!(
            spell("\u{1b}[", &["1;31".to_owned(), "m".to_owned()]),
            "CSI 1;31 m"
        );
        assert_eq!(spell("\u{1b}[", &[String::new(), "H".to_owned()]), "CSI H");
        assert_eq!(
            spell(
                "\u{1b}]",
                &["8;;https://example.com".to_owned(), "\u{1b}\\".to_owned()]
            ),
            "OSC 8;;https://example.com ST"
        );
        assert_eq!(
            spell("\u{1b}]", &["0;title".to_owned(), "\u{7}".to_owned()]),
            "OSC 0;title BEL"
        );
    }

    #[test]
    fn a_spelled_sequence_is_safe_even_when_the_payload_is_not() {
        // A payload that smuggles in its own control bytes must still come out
        // inert.
        let spelled = spell("\u{1b}]", &["0;a\u{0}b\u{9b}".to_owned()]);
        assert!(!spelled.chars().any(char::is_control), "{spelled:?}");
        // `term::pretty` now escapes every control byte itself rather than only
        // the four it has names for, so a NUL arrives here already spelled as
        // `\x00` and this escaper never sees it. The invariant asserted above is
        // what matters; the spelling is whichever layer got there first.
        assert!(
            spelled.contains("\\x00") || spelled.contains("\\u{0}"),
            "{spelled:?}"
        );
    }

    #[test]
    fn shorten_marks_where_it_cut() {
        assert_eq!(shorten("abcdef", 3), "abc…");
        assert_eq!(shorten("abc", 3), "abc");
        assert_eq!(quote_short("abcdef", 3), "\"abc…\"");
    }

    #[test]
    fn hex_counts_what_it_left_out() {
        assert_eq!(hex(b"\x1b[1m", 32), "1b 5b 31 6d");
        assert_eq!(hex(b"abcd", 2), "61 62 … +2 bytes");
        assert_eq!(hex(b"", 8), "");
    }

    #[test]
    fn recover_spells_the_bytes_it_cannot_decode() {
        assert_eq!(recover(b"plain"), "plain");
        assert_eq!(recover(b"a\xffb"), "a\\xffb");
        assert_eq!(recover(b"\x9b"), "\\x9b");
        // A truncated scalar at the end is spelled byte by byte.
        assert_eq!(recover(b"\xf0\x9f"), "\\xf0\\x9f");
        // Control bytes are left for `pretty` to name.
        assert_eq!(recover(b"\x1b[1m"), "\u{1b}[1m");
        assert_eq!(recover("é".as_bytes()), "é");
    }

    #[test]
    fn payload_text_is_lossy_but_says_so() {
        assert!(bytes_are_text(b"plain"));
        assert!(!bytes_are_text(b"\xff"));
        assert_eq!(payload_text(b"\xff"), "\u{fffd}");
    }
}
