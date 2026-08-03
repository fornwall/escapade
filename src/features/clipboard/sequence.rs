//! The protocol: selections, base64, and building the three sequences.
//!
//! ```text
//! OSC 52 ; Pc ; Pd ST
//! ```
//!
//! `Pc` names the selections to act on and `Pd` is the payload: base64 to write,
//! `?` to read, empty to clear. That is the whole of it — one sequence that does
//! three different things depending on one field.
//!
//! # Why the payload cannot be injected
//!
//! Base64's alphabet is `A`–`Z`, `a`–`z`, `0`–`9`, `+`, `/` and `=`. None of those
//! is `ESC`, none is `BEL`, and none is the semicolon that separates the fields —
//! so no input, however hostile, can end the string early and have the rest of
//! itself read as a fresh escape sequence. The encoding is not decoration here; it
//! is the reason this sequence can carry arbitrary bytes safely, and it is why
//! [`crate::features::hyperlink`] and [`crate::features::notification`] need
//! sanitizers and this module does not.
//!
//! What the encoding does *not* protect is the person who pastes the result. See
//! [`hazards`].

use base64::prelude::{BASE64_STANDARD, Engine as _};

use crate::{
    cli::ClipboardSelection,
    term::{OSC, ST},
};

/// The OSC number: `52`, xterm's "Manipulate Selection Data".
pub const PS: u16 = 52;

/// tmux's `input-buffer-size` default, in bytes.
///
/// "Number of bytes accepted in a single input before dropping" — so a sequence
/// longer than this is discarded whole rather than truncated, which looks exactly
/// like a copy that silently did nothing. Sourced from `tmux.h`
/// (`INPUT_BUF_DEFAULT_SIZE`), and it is the smallest limit any implementation in
/// the support matrix imposes by default.
pub const TMUX_INPUT_LIMIT: usize = 1_048_576;

/// One selection an operation can target.
///
/// Three of the twelve codes xterm accepts. The rest — `q` for the secondary
/// selection and `0` through `7` for the cut buffers — are X11 concepts that no
/// terminal outside xterm implements, so they are documented in `explain` rather
/// than given a flag that would do nothing nearly everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// The system clipboard: `c`. What Ctrl-V pastes.
    Clipboard,
    /// The X primary selection: `p`. What a middle click pastes.
    Primary,
    /// xterm's configurable selection: `s`. Whichever of the two the
    /// `selectToClipboard` resource points at.
    Select,
}

impl Selection {
    /// The single character that names this selection in `Pc`.
    #[must_use]
    pub fn code(self) -> char {
        match self {
            Self::Clipboard => 'c',
            Self::Primary => 'p',
            Self::Select => 's',
        }
    }

    /// Human label for a report line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Clipboard => "clipboard",
            Self::Primary => "primary selection",
            Self::Select => "configurable selection",
        }
    }

    /// How a human gets at it, which is the part people actually want to know.
    #[must_use]
    pub fn pasted_with(self) -> &'static str {
        match self {
            Self::Clipboard => "Ctrl-V, or Cmd-V",
            Self::Primary => "the middle mouse button, on X11 and Wayland",
            Self::Select => "whichever of the two xterm's selectToClipboard resource points at",
        }
    }

}

impl From<ClipboardSelection> for Selection {
    fn from(selection: ClipboardSelection) -> Self {
        match selection {
            ClipboardSelection::Clipboard => Self::Clipboard,
            ClipboardSelection::Primary => Self::Primary,
            ClipboardSelection::Select => Self::Select,
        }
    }
}

/// Encodes bytes the way this protocol wants them: standard alphabet, padded,
/// and on one line.
///
/// RFC 4648 with no line breaks. The clipboard extension spec says the encoding
/// "SHOULD be without line feeds or other whitespace", and a line feed here would
/// be worse than untidy — several terminals stop parsing at the first byte that is
/// not in the alphabet, so a wrapped payload arrives truncated.
#[must_use]
pub fn encode(data: &[u8]) -> String {
    BASE64_STANDARD.encode(data)
}

/// Decodes a payload that came back from the terminal.
///
/// Deliberately not lenient. A terminal that answers with something outside the
/// alphabet is not a terminal whose answer should be guessed at, and the caller
/// reports the raw text instead.
#[must_use]
pub fn decode(payload: &str) -> Option<Vec<u8>> {
    BASE64_STANDARD.decode(payload).ok()
}

/// `OSC 52 ; <selection> ; <base64> ST` — put `data` on the selection.
///
/// Empty `data` produces an empty payload, which is the documented way to *clear*
/// a selection rather than a sequence that does nothing: xterm clears for any `Pd`
/// that is neither base64 nor `?`, and the clipboard extension spec asks for the
/// empty form specifically. That is why [`hazards`] says so out loud — a script
/// that copies the output of a command which produced nothing has cleared the
/// user's clipboard, and should be told.
#[must_use]
pub fn set(selection: Selection, data: &[u8]) -> String {
    format!("{OSC}{PS};{};{}{ST}", selection.code(), encode(data))
}

/// `OSC 52 ; <selection> ; ? ST` — ask for the selection's contents.
///
/// The one sequence in this project that carries data *out* of the user's
/// session, and the reason most terminals refuse it by default. Never send this
/// without the user having asked for it: on Konsole it destroys the clipboard
/// rather than reading it, and on kitty and Ghostty it raises a permission dialog
/// the user did not ask to see.
#[must_use]
pub fn query(selection: Selection) -> String {
    format!("{OSC}{PS};{};?{ST}", selection.code())
}

/// A terminal's answer to [`query`], parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The `Pc` field as the terminal echoed it. Usually one character, but
    /// xterm echoes back every selection code it was given.
    pub selections: String,
    /// The payload exactly as it arrived, kept so a report can show what could
    /// not be decoded.
    pub raw: String,
    /// The decoded bytes, when the payload was valid base64.
    pub data: Option<Vec<u8>>,
}

impl Reply {
    /// Whether this answer covers `selection`.
    #[must_use]
    pub fn mentions(&self, selection: Selection) -> bool {
        // An empty `Pc` means xterm's default, `s0`, which includes the
        // configurable selection — so it answers for `s` and for nothing else
        // that can be named here.
        if self.selections.is_empty() {
            return selection == Selection::Select;
        }
        self.selections.contains(selection.code())
    }
}

/// Parses an OSC payload into a clipboard reply.
///
/// The payload arrives without the `OSC` and the terminator, so it looks like
/// `52;c;aGVsbG8=`. Anything that is not a `52` string is not ours.
#[must_use]
pub fn parse_reply(payload: &str) -> Option<Reply> {
    let rest = payload.strip_prefix("52;")?;
    // The selection list cannot contain a semicolon, so the first one ends it.
    // Splitting from the left rather than the right matters: base64 has no
    // semicolon either, but a malformed payload might.
    let (selections, data) = rest.split_once(';')?;
    Some(Reply {
        selections: selections.to_owned(),
        raw: data.to_owned(),
        data: decode(data),
    })
}

// ---------------------------------------------------------------------------
// What is worth warning about
// ---------------------------------------------------------------------------

/// Things worth telling the user before a copy goes out.
///
/// Everything here is about the *payload*, not the terminal — those belong to
/// [`super::probe`] — and every one of them is a case where the copy appears to
/// work and does not, or works and does something the user did not intend.
#[must_use]
pub fn hazards(data: &[u8], sequence_len: usize, inside_tmux: bool) -> Vec<String> {
    let mut notes = Vec::new();

    if data.is_empty() {
        notes.push(
            "the text is empty, which clears the selection rather than setting it — that is what \
             an empty payload means in this protocol, not a no-op"
                .to_owned(),
        );
    }

    if inside_tmux && sequence_len > TMUX_INPUT_LIMIT {
        notes.push(format!(
            "the sequence is {sequence_len} bytes and tmux drops any single sequence over its \
             input-buffer-size, which defaults to {TMUX_INPUT_LIMIT} — the copy will look like it \
             worked and nothing will arrive"
        ));
    }

    if data.contains(&b'\n') {
        let trailing = data.last() == Some(&b'\n');
        notes.push(if trailing {
            "the text ends with a newline, so pasting it into a shell that is not using bracketed \
             paste runs it as a command rather than leaving it on the prompt"
                .to_owned()
        } else {
            "the text contains a newline, so pasting it into a shell that is not using bracketed \
             paste runs everything up to that newline"
                .to_owned()
        });
    }

    notes
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::term::pretty;

    #[test]
    fn a_write_is_the_documented_sequence() {
        assert_eq!(
            pretty(&set(Selection::Clipboard, b"hello")),
            "OSC 52;c;aGVsbG8= ST"
        );
        assert_eq!(
            pretty(&set(Selection::Primary, b"hello")),
            "OSC 52;p;aGVsbG8= ST"
        );
    }

    #[test]
    fn a_query_and_a_clear_differ_only_in_the_payload() {
        assert_eq!(pretty(&query(Selection::Clipboard)), "OSC 52;c;? ST");
        // Clearing is a write with nothing in it, not a sequence of its own.
        assert_eq!(pretty(&set(Selection::Clipboard, b"")), "OSC 52;c; ST");
    }

    #[test]
    fn every_sequence_ends_with_st_and_never_beeps() {
        for sequence in [
            set(Selection::Clipboard, b"x"),
            set(Selection::Select, b""),
            query(Selection::Primary),
        ] {
            assert!(sequence.ends_with('\\'), "{}", pretty(&sequence));
            assert!(!sequence.contains('\u{7}'), "a sequence must not beep");
        }
    }

    #[test]
    fn the_encoded_payload_can_never_end_the_string_early() {
        // The security property the whole feature rests on: base64's alphabet
        // contains no ESC, no BEL and no semicolon, so hostile input cannot break
        // out of the payload. Try to break it with the bytes that would.
        let hostile = b"\x1b]52;c;evil\x07\x1b\\; drop table students; --";
        let sequence = set(Selection::Clipboard, hostile);
        let payload = sequence
            .strip_prefix("\u{1b}]52;c;")
            .unwrap()
            .strip_suffix("\u{1b}\\")
            .unwrap();
        assert!(
            payload
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '+' || ch == '/' || ch == '='),
            "{payload}"
        );
        // And it survives the round trip byte for byte.
        assert_eq!(decode(payload).unwrap(), hostile);
    }

    #[test]
    fn encoding_never_wraps_however_long_the_input() {
        // A line feed in the payload is not cosmetic: terminals that stop at the
        // first byte outside the alphabet would receive a truncated clipboard.
        let long = vec![b'a'; 10_000];
        let encoded = encode(&long);
        assert!(!encoded.contains('\n'), "the payload must be one line");
        assert!(!encoded.contains('\r'));
        assert_eq!(decode(&encoded).unwrap(), long);
    }

    #[test]
    fn arbitrary_bytes_survive_the_round_trip() {
        // A clipboard is not text. Bytes that are not UTF-8 must still come back
        // unchanged, which is why the operand is read as bytes.
        let bytes: Vec<u8> = (0u8..=255).collect();
        assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn a_reply_is_parsed_into_its_selection_and_its_bytes() {
        let reply = parse_reply("52;c;aGVsbG8=").unwrap();
        assert_eq!(reply.selections, "c");
        assert_eq!(reply.data.as_deref(), Some(&b"hello"[..]));
        assert!(reply.mentions(Selection::Clipboard));
        assert!(!reply.mentions(Selection::Primary));
    }

    #[test]
    fn a_reply_that_names_several_selections_answers_for_each() {
        // xterm echoes back the whole list it was given, so a one-character
        // comparison would fail to match its own answer.
        let reply = parse_reply("52;s0;aGk=").unwrap();
        assert!(reply.mentions(Selection::Select));
        assert!(!reply.mentions(Selection::Clipboard));
    }

    #[test]
    fn an_empty_selection_field_means_xterms_default() {
        // Pc empty is `s0` — the configurable selection and cut buffer 0.
        let reply = parse_reply("52;;aGk=").unwrap();
        assert!(reply.mentions(Selection::Select));
        assert!(!reply.mentions(Selection::Clipboard));
    }

    #[test]
    fn an_undecodable_reply_keeps_its_raw_text() {
        // Konsole answers a query by writing to the clipboard, and other
        // terminals have been known to answer with something that is not base64.
        // Losing the original would make that unreportable.
        let reply = parse_reply("52;c;not base64!").unwrap();
        assert_eq!(reply.data, None);
        assert_eq!(reply.raw, "not base64!");
    }

    #[test]
    fn replies_to_other_sequences_are_not_ours() {
        assert!(parse_reply("11;rgb:1e1e/1e1e/2e2e").is_none());
        assert!(parse_reply("521;c;aGk=").is_none(), "52 is not a prefix");
        assert!(parse_reply("52;c").is_none(), "no payload field at all");
    }

    #[test]
    fn selection_codes_are_the_xterm_ones() {
        assert_eq!(Selection::Clipboard.code(), 'c');
        assert_eq!(Selection::Primary.code(), 'p');
        assert_eq!(Selection::Select.code(), 's');
        // Every code is one character, which is what makes `mentions` a
        // containment test against the list a terminal echoes back.
        for selection in [Selection::Clipboard, Selection::Primary, Selection::Select] {
            assert_eq!(selection.code().len_utf8(), 1);
        }
    }

    #[test]
    fn an_empty_payload_is_reported_as_a_clear_rather_than_a_no_op() {
        let notes = hazards(b"", set(Selection::Clipboard, b"").len(), false);
        assert!(notes.iter().any(|note| note.contains("clears")), "{notes:?}");
    }

    #[test]
    fn a_trailing_newline_is_called_out_because_it_runs_on_paste() {
        let notes = hazards(b"rm -rf /\n", 40, false);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("runs it"), "{notes:?}");

        // A newline in the middle is a different sentence: everything up to it
        // runs, and the rest is left on the prompt.
        let notes = hazards(b"first\nsecond", 40, false);
        assert!(notes[0].contains("up to that newline"), "{notes:?}");

        // Ordinary text says nothing at all.
        assert!(hazards(b"abc123", 40, false).is_empty());
    }

    #[test]
    fn an_oversized_payload_is_only_a_problem_where_it_is_one() {
        let big = TMUX_INPUT_LIMIT + 1;
        assert!(hazards(b"x", big, false).is_empty(), "outside tmux");
        let notes = hazards(b"x", big, true);
        assert!(notes.iter().any(|note| note.contains("input-buffer-size")), "{notes:?}");
    }
}
