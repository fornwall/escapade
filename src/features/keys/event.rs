//! Rendering one input event, and the bytes that produced it.
//!
//! Everything here is a pure function of `(bytes, event)`, which is deliberate:
//! [`watch`](super::watch) and [`demo`](super::demo) show the same three things
//! for every keypress — **what key it was, what modifiers were held, and which
//! bytes said so** — and a reader comparing two runs of `escapade keys watch`
//! needs those to be formatted identically both times.
//!
//! The byte rendering is the part that has to be right. Printing a captured
//! `ESC` verbatim would hand it back to the terminal as a command rather than
//! showing it, so bytes go through [`symbolic`] (names for the introducers and
//! the control codes) or [`hex`] (no interpretation at all). Neither can emit a
//! control byte; a test checks that.

use std::fmt::Write as _;

use crate::input::{Event, KeyCode, KeyEvent, KeyEventKind, Modifiers, Reply};

/// Names one key, the way a person would say it.
///
/// Characters come back as themselves rather than as codepoints, except for
/// Space, which is named because a lone space in a report is invisible.
#[must_use]
pub fn key_name(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "Space".to_owned(),
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::BackTab => "BackTab".to_owned(),
        KeyCode::Backspace => "Backspace".to_owned(),
        KeyCode::Esc => "Escape".to_owned(),
        KeyCode::Delete => "Delete".to_owned(),
        KeyCode::Insert => "Insert".to_owned(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PageUp".to_owned(),
        KeyCode::PageDown => "PageDown".to_owned(),
        KeyCode::F(number) => format!("F{number}"),
    }
}

/// Names a key together with its modifiers: `Ctrl+Shift+Enter`, or `a`.
///
/// Modifier order comes from [`Modifiers`]'s own `Display`, so it is the same
/// here as everywhere else in the program.
#[must_use]
pub fn describe_key(key: &KeyEvent) -> String {
    if key.mods.is_empty() {
        key_name(key.code)
    } else {
        format!("{}+{}", key.mods, key_name(key.code))
    }
}

/// `press`, `repeat` or `release`.
///
/// Only `press` ever appears without the `report event types` flag (`CSI > 2 u`);
/// seeing either of the other two is itself proof that the flag took effect.
#[must_use]
pub fn kind_label(kind: KeyEventKind) -> &'static str {
    match kind {
        KeyEventKind::Press => "press",
        KeyEventKind::Repeat => "repeat",
        KeyEventKind::Release => "release",
    }
}

/// One line describing any event: the key and its modifiers, the mouse report,
/// the terminal reply, or the bytes that could not be decoded.
///
/// [`Event::Unknown`] is not a failure to be hidden. Under this protocol it is
/// usually a real key that has no [`KeyCode`] — Caps Lock is `CSI 57358 u`, the
/// individual modifier keys are `CSI 57441 u` and up — so the bytes are what the
/// reader wants to see.
#[must_use]
pub fn describe_event(event: &Event) -> String {
    match event {
        Event::Key(key) => format!("{}  {}", describe_key(key), kind_label(key.kind)),
        Event::Mouse(mouse) => format!(
            "mouse {:?} at {},{}{}",
            mouse.kind,
            mouse.col,
            mouse.row,
            modifier_suffix(mouse.mods)
        ),
        Event::Paste(text) => format!("paste of {} characters", text.chars().count()),
        Event::FocusGained => "window focused".to_owned(),
        Event::FocusLost => "window unfocused".to_owned(),
        Event::Reply(reply) => format!("terminal reply — {}", describe_reply(reply)),
        Event::Unknown(bytes) => format!(
            "undecoded ({} byte{})",
            bytes.len(),
            if bytes.len() == 1 { "" } else { "s" }
        ),
    }
}

/// `  with Ctrl+Shift`, or nothing at all when no modifier was held.
fn modifier_suffix(mods: Modifiers) -> String {
    if mods.is_empty() {
        String::new()
    } else {
        format!(" with {mods}")
    }
}

/// One-line rendering of a terminal reply, safe to print.
#[must_use]
pub fn describe_reply(reply: &Reply) -> String {
    match reply {
        Reply::Apc(payload) => format!("APC {payload} ST"),
        Reply::Dcs(payload) => format!("DCS {payload} ST"),
        Reply::Osc(payload) => format!("OSC {payload} ST"),
        Reply::Csi { params, final_byte } => format!("CSI {params}{final_byte}"),
    }
}

/// A short word for the kind of event, for machine-readable output.
#[must_use]
pub fn event_kind(event: &Event) -> &'static str {
    match event {
        Event::Key(_) => "key",
        Event::Mouse(_) => "mouse",
        Event::Reply(_) => "reply",
        Event::Paste(_) => "paste",
        Event::FocusGained => "focus-gained",
        Event::FocusLost => "focus-lost",
        Event::Unknown(_) => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Bytes
// ---------------------------------------------------------------------------

/// Renders raw input bytes symbolically: `CSI 13;2u`, `^D`, `CR`, `a`.
///
/// This is the half of a key report that cannot be got from any library: the
/// exact bytes the terminal sent, in a form that can be pasted into a bug report
/// or compared against a specification. Introducers get their standard names
/// (`CSI`, `SS3`, `ESC`, `OSC`, `APC`, `DCS`, `ST`), the named C0 controls get
/// theirs (`NUL`, `TAB`, `CR`, `LF`, `BS`, `DEL`), any remaining control byte
/// gets caret notation, printable ASCII and valid UTF-8 appear as themselves, and
/// anything left is `\xNN`.
///
/// The result never contains a control byte, so it is always safe to print.
#[must_use]
pub fn symbolic(bytes: &[u8]) -> String {
    /// Appends a name with a space on either side, without doubling one.
    fn token(out: &mut String, name: &str) {
        if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push_str(name);
        out.push(' ');
    }

    let mut out = String::with_capacity(bytes.len() * 2);
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        index += 1;
        match byte {
            0x1b => {
                // The byte after `ESC` decides which introducer this is; a lone
                // trailing `ESC` is just the Escape key.
                let (name, extra) = match bytes.get(index) {
                    Some(b'[') => ("CSI", 1),
                    Some(b'O') => ("SS3", 1),
                    Some(b']') => ("OSC", 1),
                    Some(b'_') => ("APC", 1),
                    Some(b'P') => ("DCS", 1),
                    Some(b'\\') => ("ST", 1),
                    _ => ("ESC", 0),
                };
                index += extra;
                token(&mut out, name);
            }
            0x00 => token(&mut out, "NUL"),
            0x07 => token(&mut out, "BEL"),
            0x08 => token(&mut out, "BS"),
            0x09 => token(&mut out, "TAB"),
            0x0a => token(&mut out, "LF"),
            0x0d => token(&mut out, "CR"),
            0x7f => token(&mut out, "DEL"),
            // Every other C0 control is a Ctrl+key: caret notation says which.
            0x01..=0x1f => token(&mut out, &format!("^{}", char::from(byte + 0x40))),
            0x20..=0x7e => out.push(char::from(byte)),
            _ => {
                // A byte over 0x7f only means anything as part of a UTF-8 scalar.
                index -= 1;
                if let Some((ch, len)) = utf8_char(&bytes[index..]) {
                    out.push(ch);
                    index += len;
                } else {
                    let _ = write!(out, "\\x{byte:02x}");
                    index += 1;
                }
            }
        }
    }
    out.trim_end().to_owned()
}

/// Decodes one UTF-8 scalar from the front of `bytes`, with its length.
fn utf8_char(bytes: &[u8]) -> Option<(char, usize)> {
    for len in 1..=bytes.len().min(4) {
        if let Ok(text) = std::str::from_utf8(&bytes[..len])
            && let Some(ch) = text.chars().next()
        {
            return Some((ch, len));
        }
    }
    None
}

/// Renders raw bytes as lowercase hex pairs: `1b 5b 31 33 3b 32 75`.
///
/// No interpretation whatsoever, which is what makes it the right thing to put in
/// a bug report next to [`symbolic`].
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// One JSON object on one line, describing an event and the bytes behind it.
///
/// The house machine-readable shape: JSON Lines, so `escapade keys watch
/// --format json | jq` streams a keypress at a time rather than waiting for a
/// document that never ends.
#[must_use]
pub fn json_line(bytes: &[u8], event: &Event) -> String {
    let mut fields = vec![
        format!("\"kind\":\"{}\"", event_kind(event)),
        format!("\"bytes\":\"{}\"", hex(bytes).replace(' ', "")),
        format!("\"sequence\":\"{}\"", escape_json(&symbolic(bytes))),
    ];
    match event {
        Event::Key(key) => {
            fields.push(format!("\"key\":\"{}\"", escape_json(&key_name(key.code))));
            fields.push(format!("\"mods\":[{}]", json_mods(key.mods)));
            fields.push(format!("\"event\":\"{}\"", kind_label(key.kind)));
        }
        Event::Paste(text) => {
            fields.push(format!("\"text\":\"{}\"", escape_json(text)));
        }
        Event::Reply(reply) => {
            fields.push(format!("\"reply\":\"{}\"", escape_json(&describe_reply(reply))));
        }
        Event::Mouse(mouse) => {
            fields.push(format!("\"mouse\":\"{:?}\"", mouse.kind));
            fields.push(format!("\"col\":{}", mouse.col));
            fields.push(format!("\"row\":{}", mouse.row));
            fields.push(format!("\"mods\":[{}]", json_mods(mouse.mods)));
        }
        Event::Unknown(_) | Event::FocusGained | Event::FocusLost => {}
    }
    format!("{{{}}}\n", fields.join(","))
}

/// The held modifiers as a JSON array body: `"ctrl","shift"`.
fn json_mods(mods: Modifiers) -> String {
    [
        (mods.ctrl, "ctrl"),
        (mods.alt, "alt"),
        (mods.shift, "shift"),
        (mods.super_, "super"),
    ]
    .into_iter()
    .filter(|(held, _)| *held)
    .map(|(_, name)| format!("\"{name}\""))
    .collect::<Vec<_>>()
    .join(",")
}

/// Minimal JSON string escaping, for the few strings that reach the output.
#[must_use]
pub fn escape_json(text: &str) -> String {
    text.chars()
        .flat_map(|ch| match ch {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            ch if ch.is_control() => format!("\\u{:04x}", ch as u32).chars().collect(),
            ch => vec![ch],
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::input::{ParseOutcome, parse_event};

    /// Parses `bytes` the way the real reader does, and returns both halves of
    /// what a report shows.
    #[track_caller]
    fn decode(bytes: &[u8]) -> Event {
        match parse_event(bytes) {
            ParseOutcome::Event { event, consumed } => {
                assert_eq!(consumed, bytes.len(), "test input must be one whole event");
                event
            }
            ParseOutcome::Incomplete => panic!("incomplete: {bytes:?}"),
        }
    }

    #[test]
    fn a_key_report_names_the_key_the_modifiers_and_the_event() {
        // The comparison the whole subcommand exists for.
        let plain = decode(b"\r");
        let shifted = decode(b"\x1b[13;2u");
        assert_eq!(describe_event(&plain), "Enter  press");
        assert_eq!(describe_event(&shifted), "Shift+Enter  press");
        assert_ne!(describe_event(&plain), describe_event(&shifted));
    }

    #[test]
    fn event_types_are_named() {
        assert_eq!(describe_event(&decode(b"\x1b[97;1:1u")), "a  press");
        assert_eq!(describe_event(&decode(b"\x1b[97;1:2u")), "a  repeat");
        assert_eq!(describe_event(&decode(b"\x1b[97;1:3u")), "a  release");
        assert_eq!(
            describe_event(&decode(b"\x1b[27;5:3u")),
            "Ctrl+Escape  release"
        );
    }

    #[test]
    fn keys_with_no_key_code_are_shown_as_their_bytes_not_swallowed() {
        // `57358` is Caps Lock, which has no `KeyCode`. The report has to say
        // *something* useful, and the bytes are the useful thing.
        let event = decode(b"\x1b[57358u");
        assert_eq!(event, Event::Unknown(b"\x1b[57358u".to_vec()));
        assert_eq!(describe_event(&event), "undecoded (8 bytes)");
        assert_eq!(symbolic(b"\x1b[57358u"), "CSI 57358u");
    }

    #[test]
    fn key_names_are_the_names_a_person_would_use() {
        assert_eq!(key_name(KeyCode::Char('a')), "a");
        assert_eq!(key_name(KeyCode::Char(' ')), "Space");
        assert_eq!(key_name(KeyCode::F(13)), "F13");
        assert_eq!(key_name(KeyCode::PageUp), "PageUp");
        assert_eq!(key_name(KeyCode::Esc), "Escape");
    }

    #[test]
    fn symbolic_names_the_introducers_and_the_controls() {
        assert_eq!(symbolic(b"\x1b[13;2u"), "CSI 13;2u");
        assert_eq!(symbolic(b"\x1b[97:65:97;2;65u"), "CSI 97:65:97;2;65u");
        assert_eq!(symbolic(b"\x1bOP"), "SS3 P");
        assert_eq!(symbolic(b"\x1b"), "ESC");
        assert_eq!(symbolic(b"\x1ba"), "ESC a");
        assert_eq!(symbolic(b"\r"), "CR");
        assert_eq!(symbolic(b"\n"), "LF");
        assert_eq!(symbolic(b"\t"), "TAB");
        assert_eq!(symbolic(b"\x7f"), "DEL");
        assert_eq!(symbolic(b"\x00"), "NUL");
        assert_eq!(symbolic(b"a"), "a");
        assert_eq!(symbolic(b"ab"), "ab");
    }

    #[test]
    fn symbolic_uses_caret_notation_for_the_rest_of_c0() {
        // Ctrl+D, the key that ends `watch`.
        assert_eq!(symbolic(b"\x04"), "^D");
        assert_eq!(symbolic(b"\x03"), "^C");
        assert_eq!(symbolic(b"\x01"), "^A");
        // 0x1f is Ctrl+_ — the top of the range, and the one an off-by-one hits.
        assert_eq!(symbolic(b"\x1f"), "^_");
    }

    #[test]
    fn symbolic_shows_text_and_undecodable_bytes_for_what_they_are() {
        assert_eq!(symbolic("é".as_bytes()), "é");
        assert_eq!(symbolic("🦀".as_bytes()), "🦀");
        // Alt+é: the escape prefix, then a two-byte scalar.
        assert_eq!(symbolic("\u{1b}é".as_bytes()), "ESC é");
        // A stray continuation byte is not a character and must not be invented.
        assert_eq!(symbolic(&[0x80]), "\\x80");
        assert_eq!(symbolic(&[0xff, b'a']), "\\xffa");
    }

    #[test]
    fn rendered_bytes_are_always_safe_to_print() {
        // The failure this prevents: echoing a captured escape sequence back to
        // the terminal, which would execute it instead of showing it.
        let nasty: &[&[u8]] = &[
            b"\x1b[2J",
            b"\x1b_payload\x1b\\",
            b"\x1b]0;title\x07",
            &[0x1b, 0x1b, 0x00, 0xff],
        ];
        for bytes in nasty {
            for rendered in [symbolic(bytes), hex(bytes)] {
                assert!(
                    !rendered.chars().any(char::is_control),
                    "{rendered:?} contains a control character"
                );
            }
        }
    }

    #[test]
    fn hex_is_plain_and_pasteable() {
        assert_eq!(hex(b"\x1b[13;2u"), "1b 5b 31 33 3b 32 75");
        assert_eq!(hex(b""), "");
        assert_eq!(hex(&[0x00, 0xff]), "00 ff");
    }

    #[test]
    fn json_carries_both_the_decoding_and_the_bytes() {
        let bytes = b"\x1b[13;2u";
        let line = json_line(bytes, &decode(bytes));
        assert!(line.ends_with('\n'), "JSON Lines needs the newline");
        assert!(line.contains("\"kind\":\"key\""));
        assert!(line.contains("\"key\":\"Enter\""));
        assert!(line.contains("\"mods\":[\"shift\"]"));
        assert!(line.contains("\"event\":\"press\""));
        assert!(line.contains("\"bytes\":\"1b5b31333b3275\""));
        assert!(line.contains("\"sequence\":\"CSI 13;2u\""));
        assert!(!line.contains('\u{1b}'), "no raw escape in machine output");
    }

    #[test]
    fn json_escapes_what_it_must() {
        assert_eq!(escape_json("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(escape_json("\u{1b}"), "\\u001b");
        // A paste is arbitrary text, so it is the field most likely to contain a
        // quote or a newline.
        let event = decode(b"\x1b[200~he\"llo\n\x1b[201~");
        let line = json_line(b"\x1b[200~he\"llo\n\x1b[201~", &event);
        assert!(line.contains("\\\"llo"), "{line}");
        assert!(line.contains("\\u000a"), "{line}");
        assert_eq!(line.matches('\n').count(), 1, "only the terminating newline");
    }

    #[test]
    fn json_covers_every_event_shape_without_panicking() {
        let inputs: &[&[u8]] = &[
            b"a",
            b"\x1b[13;2u",
            b"\x1b[97;1:3u",
            b"\x1b[<0;10;20M",
            b"\x1b[200~hi\x1b[201~",
            b"\x1b[I",
            b"\x1b[O",
            b"\x1b[?5u",
            b"\x1b[57358u",
        ];
        for bytes in inputs {
            let line = json_line(bytes, &decode(bytes));
            assert!(line.starts_with('{') && line.ends_with("}\n"), "{line}");
        }
    }

    #[test]
    fn the_flags_reply_is_reported_as_a_reply_not_a_key() {
        let event = decode(b"\x1b[?5u");
        assert_eq!(event_kind(&event), "reply");
        assert_eq!(describe_event(&event), "terminal reply — CSI ?5u");
    }
}
