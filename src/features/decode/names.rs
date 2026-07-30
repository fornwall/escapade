//! What the sequences are called, and what they do.
//!
//! The tables here name the parts of the grammar that no data file describes.
//! Anything a feature file *does* describe is looked up instead — see
//! [`super::catalog`] — because prose in the database improves without a rebuild
//! and prose in Rust does not. These are the gaps that leaves: the C0 controls,
//! the two-byte escapes, cursor movement and erasing, the mode registries, and
//! the numbers in the `OSC` namespace whose feature files are still placeholders.
//!
//! Every entry is a mnemonic and one short phrase. The mnemonic is what a
//! specification calls the sequence (`DECSET`, `CUP`, `ED`), because that is the
//! word to search for; the phrase says what it does to the screen, in the
//! imperative, because that is what the reader is trying to find out.

use super::parse::Fields;

/// A named sequence: its mnemonic, and what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meaning {
    /// Specification mnemonic, e.g. `DECSET`. Empty when the sequence has none.
    pub mnemonic: &'static str,
    /// What it does, in one phrase.
    pub summary: String,
}

/// Builds a [`Meaning`] from a mnemonic and a phrase.
///
/// Always `Some`, because every caller is one arm of a table whose misses are
/// `None`. Returning the bare value would put `Some(…)` on a hundred arms and
/// bury the tables in punctuation.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the tables read better with the Option in one place"
)]
fn meaning(mnemonic: &'static str, summary: impl Into<String>) -> Option<Meaning> {
    Some(Meaning {
        mnemonic,
        summary: summary.into(),
    })
}

/// `1 row` but `3 rows`.
fn count(value: u32, singular: &str) -> String {
    if value == 1 {
        format!("{value} {singular}")
    } else {
        format!("{value} {singular}s")
    }
}

// ---------------------------------------------------------------------------
// C0 controls
// ---------------------------------------------------------------------------

/// The abbreviation and meaning of a C0 control byte, or of `DEL`.
///
/// These are the bytes `cat -v` shows as `^G` and leaves at that. Several of them
/// are indistinguishable from a bug in the producer — a `NUL` in the middle of a
/// line, a `SUB` where a character should be — which is why they are decoded as
/// items of their own rather than folded into the surrounding text.
#[must_use]
pub fn control(byte: u8) -> Meaning {
    let (mnemonic, summary) = match byte {
        0x00 => ("NUL", "null; padding, and never meaningful in text"),
        0x01 => ("SOH", "start of heading"),
        0x02 => ("STX", "start of text"),
        0x03 => ("ETX", "end of text; what Ctrl+C sends"),
        0x04 => ("EOT", "end of transmission; what Ctrl+D sends"),
        0x05 => ("ENQ", "enquiry; some terminals answer with an answerback string"),
        0x06 => ("ACK", "acknowledge"),
        0x07 => ("BEL", "ring the bell, or flash the window"),
        0x08 => ("BS", "move the cursor back one column"),
        0x09 => ("HT", "move to the next tab stop"),
        0x0a => ("LF", "move down one row"),
        0x0b => ("VT", "vertical tab; treated as a line feed nearly everywhere"),
        0x0c => ("FF", "form feed; treated as a line feed, or clears the screen"),
        0x0d => ("CR", "move to the first column"),
        0x0e => ("SO", "shift out: use the G1 character set from here on"),
        0x0f => ("SI", "shift in: return to the G0 character set"),
        0x10 => ("DLE", "data link escape"),
        0x11 => ("DC1", "device control 1; XON, which resumes a paused terminal"),
        0x12 => ("DC2", "device control 2"),
        0x13 => ("DC3", "device control 3; XOFF, which pauses output"),
        0x14 => ("DC4", "device control 4"),
        0x15 => ("NAK", "negative acknowledge"),
        0x16 => ("SYN", "synchronous idle"),
        0x17 => ("ETB", "end of transmission block"),
        0x18 => ("CAN", "cancel: abandon the sequence in progress"),
        0x19 => ("EM", "end of medium"),
        0x1a => ("SUB", "substitute: stands in for a character that was invalid"),
        0x1b => ("ESC", "escape, with nothing after it"),
        0x1c => ("FS", "file separator"),
        0x1d => ("GS", "group separator"),
        0x1e => ("RS", "record separator"),
        0x1f => ("US", "unit separator"),
        0x7f => ("DEL", "delete; padding, historically a punched-out character"),
        _ => ("", "not a control byte"),
    };
    Meaning {
        mnemonic,
        summary: summary.to_owned(),
    }
}

/// The C1 control a byte in `0x80..=0x9f` would be in an eight-bit encoding.
///
/// Reported for information only. `0x9b` is `CSI` and `0x9d` is `OSC` in Latin-1,
/// but in a UTF-8 stream the same byte is simply broken, and a decoder that acted
/// on it would be guessing at the stream's encoding.
#[must_use]
pub fn eight_bit_c1(byte: u8) -> &'static str {
    match byte {
        0x84 => "IND",
        0x85 => "NEL",
        0x88 => "HTS",
        0x8d => "RI",
        0x8e => "SS2",
        0x8f => "SS3",
        0x90 => "DCS",
        0x9b => "CSI",
        0x9c => "ST",
        0x9d => "OSC",
        0x9e => "PM",
        0x9f => "APC",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Two- and three-byte escape sequences
// ---------------------------------------------------------------------------

/// Names `ESC <intermediates> <final>`.
///
/// The ones that matter in practice are the cursor save/restore pair, `RIS`, the
/// keypad modes and the character-set designations — the last of which is how a
/// program asks for the line-drawing set, and therefore how a border ends up as
/// `qqqq` when something forgot to switch back.
#[must_use]
pub fn escape_sequence(intermediates: &[u8], final_byte: u8) -> Option<Meaning> {
    match (intermediates, final_byte) {
        ([], b'6') => meaning("DECBI", "back index: scroll right when at the left margin"),
        ([], b'7') => meaning("DECSC", "save the cursor position and attributes"),
        ([], b'8') => meaning("DECRC", "restore the saved cursor position and attributes"),
        ([], b'9') => meaning("DECFI", "forward index: scroll left when at the right margin"),
        ([], b'=') => meaning("DECKPAM", "put the numeric keypad in application mode"),
        ([], b'>') => meaning("DECKPNM", "put the numeric keypad in numeric mode"),
        ([], b'D') => meaning("IND", "move down one row, scrolling if needed"),
        ([], b'E') => meaning("NEL", "move to the first column of the next row"),
        ([], b'H') => meaning("HTS", "set a tab stop at the cursor"),
        ([], b'M') => meaning("RI", "move up one row, scrolling if needed"),
        ([], b'V') => meaning("SPA", "start of a protected area"),
        ([], b'W') => meaning("EPA", "end of a protected area"),
        ([], b'Z') => meaning("DECID", "ask the terminal to identify itself; use CSI c instead"),
        ([], b'c') => meaning("RIS", "reset the terminal to its power-up state"),
        ([], b'l') => meaning("", "lock the memory above the cursor (HP)"),
        ([], b'm') => meaning("", "unlock memory (HP)"),
        ([], b'n') => meaning("LS2", "use the G2 character set for the rest of the output"),
        ([], b'o') => meaning("LS3", "use the G3 character set for the rest of the output"),
        ([], b'|') => meaning("LS3R", "use G3 for the right half of the code table"),
        ([], b'}') => meaning("LS2R", "use G2 for the right half of the code table"),
        ([], b'~') => meaning("LS1R", "use G1 for the right half of the code table"),
        ([], b'\\') => meaning(
            "ST",
            "string terminator, with no control string open to terminate",
        ),
        ([b' '], b'F') => meaning("S7C1T", "send seven-bit controls, so ST is ESC \\"),
        ([b' '], b'G') => meaning("S8C1T", "send eight-bit controls, so ST is the byte 0x9c"),
        ([b'#'], b'3') => meaning("DECDHL", "double-height line, top half"),
        ([b'#'], b'4') => meaning("DECDHL", "double-height line, bottom half"),
        ([b'#'], b'5') => meaning("DECSWL", "single-width line"),
        ([b'#'], b'6') => meaning("DECDWL", "double-width line"),
        ([b'#'], b'8') => meaning("DECALN", "fill the screen with E, an alignment test pattern"),
        ([b'%'], b'@') => meaning("", "interpret output as ISO 8859-1"),
        ([b'%'], b'G' | b'8') => meaning("", "interpret output as UTF-8"),
        ([b'('..=b'+' | b'-'..=b'/'], _) => charset(intermediates, final_byte),
        _ => None,
    }
}

/// Names a character-set designation: `ESC ( B`, `ESC ) 0`, `ESC ( 0`.
///
/// `ESC ( 0` is the one to recognise on sight: it selects DEC Special Graphics,
/// where `q` draws a horizontal line. A program that designates it and forgets
/// `ESC ( B` leaves every following letter as box drawing, which is one of the
/// classic ways a screen turns to soup.
fn charset(intermediates: &[u8], final_byte: u8) -> Option<Meaning> {
    let slot = match intermediates.first().copied() {
        Some(b'(') => "G0",
        Some(b')' | b'-') => "G1",
        Some(b'*' | b'.') => "G2",
        Some(b'+' | b'/') => "G3",
        _ => return None,
    };
    let set = match final_byte {
        b'0' => "DEC Special Graphics, the line-drawing set",
        b'1' => "DEC alternate character ROM",
        b'2' => "DEC alternate character ROM, graphics",
        b'A' => "United Kingdom",
        b'B' => "US ASCII",
        b'<' => "DEC supplemental",
        b'>' => "DEC technical",
        b'4' => "Dutch",
        b'5' | b'C' => "Finnish",
        b'R' => "French",
        b'Q' => "French Canadian",
        b'K' => "German",
        b'Y' => "Italian",
        b'6' | b'E' => "Norwegian or Danish",
        b'Z' => "Spanish",
        b'7' | b'H' => "Swedish",
        b'=' => "Swiss",
        _ => "an unrecognised character set",
    };
    meaning("SCS", format!("designate {set} as {slot}"))
}

// ---------------------------------------------------------------------------
// CSI
// ---------------------------------------------------------------------------

/// Names a `CSI` sequence, parameters included.
///
/// `SGR` is handled by [`super::sgr`] rather than here, because naming its
/// parameters is a job in itself.
#[must_use]
pub fn csi(fields: &Fields) -> Option<Meaning> {
    match fields.private {
        None => csi_standard(fields),
        Some(b'?') => csi_dec_private(fields),
        Some(b'>') => csi_greater(fields),
        Some(b'=') => csi_equals(fields),
        Some(b'<') => csi_less(fields),
        Some(_) => None,
    }
}

/// `CSI` with no private marker: ECMA-48's own repertoire, plus the DEC and xterm
/// sequences that share it.
fn csi_standard(fields: &Fields) -> Option<Meaning> {
    let first = fields.value_or(0, 1);
    let selector = fields.value_or(0, 0);
    match (fields.intermediates.as_slice(), fields.final_byte) {
        ([], b'@') => meaning("ICH", format!("insert {} at the cursor", count(first, "blank character"))),
        ([], b'A') => meaning("CUU", format!("move the cursor up {}", count(first, "row"))),
        ([], b'B') => meaning("CUD", format!("move the cursor down {}", count(first, "row"))),
        ([], b'C') => meaning("CUF", format!("move the cursor right {}", count(first, "column"))),
        ([], b'D') => meaning("CUB", format!("move the cursor left {}", count(first, "column"))),
        ([], b'E') => meaning("CNL", format!("move to column 1, {} down", count(first, "row"))),
        ([], b'F') => meaning("CPL", format!("move to column 1, {} up", count(first, "row"))),
        ([], b'G') => meaning("CHA", format!("move the cursor to column {first}")),
        ([], b'H' | b'f') => {
            let row = fields.value_or(0, 1);
            let column = fields.value_or(1, 1);
            let mnemonic = if fields.final_byte == b'H' { "CUP" } else { "HVP" };
            meaning(mnemonic, format!("move the cursor to row {row}, column {column}"))
        }
        ([], b'I') => meaning("CHT", format!("move forward {}", count(first, "tab stop"))),
        ([], b'J') => meaning("ED", erase_in_display(selector)),
        ([], b'K') => meaning("EL", erase_in_line(selector)),
        ([], b'L') => meaning("IL", format!("insert {} at the cursor", count(first, "row"))),
        ([], b'M') => meaning("DL", format!("delete {} at the cursor", count(first, "row"))),
        ([], b'P') => meaning("DCH", format!("delete {} at the cursor", count(first, "character"))),
        ([], b'R') => meaning(
            "CPR",
            format!(
                "a reply, not a request: the cursor is at row {}, column {}",
                fields.value_or(0, 1),
                fields.value_or(1, 1)
            ),
        ),
        ([], b'S') => meaning("SU", format!("scroll the screen up {}", count(first, "row"))),
        ([], b'T' | b'^') => {
            meaning("SD", format!("scroll the screen down {}", count(first, "row")))
        }
        ([b'+'], b'T') => meaning(
            "",
            format!(
                "pull {} of scrollback back onto the screen (kitty)",
                count(first, "line")
            ),
        ),
        ([], b'X') => meaning(
            "ECH",
            format!("erase {} in place, without moving anything", count(first, "character")),
        ),
        ([], b'Z') => meaning("CBT", format!("move back {}", count(first, "tab stop"))),
        ([], b'`') => meaning("HPA", format!("move the cursor to column {first}")),
        ([], b'a') => meaning("HPR", format!("move the cursor right {}", count(first, "column"))),
        ([], b'b') => meaning("REP", format!("repeat the previous character {first} times")),
        ([], b'c') => meaning(
            "DA1",
            "ask the terminal what it is — the barrier every terminal answers",
        ),
        ([], b'd') => meaning("VPA", format!("move the cursor to row {first}")),
        ([], b'e') => meaning("VPR", format!("move the cursor down {}", count(first, "row"))),
        ([], b'g') => meaning(
            "TBC",
            match selector {
                3 | 5 => "clear every tab stop".to_owned(),
                _ => "clear the tab stop at the cursor".to_owned(),
            },
        ),
        ([], b'h') => meaning("SM", modes(fields, ModeKind::Ansi, true)),
        ([], b'i') => meaning("MC", media_copy(selector)),
        ([], b'l') => meaning("RM", modes(fields, ModeKind::Ansi, false)),
        ([], b'n') => meaning("DSR", device_status(selector)),
        ([b'!'], b'p') => meaning("DECSTR", "soft reset: modes, margins, attributes and tab stops"),
        ([b'"'], b'p') => meaning("DECSCL", format!("set conformance level {first}")),
        ([b'$'], b'p') => meaning(
            "DECRQM",
            format!("ask whether ANSI mode {} is set", fields.value_or(0, 0)),
        ),
        ([b' '], b'q') => meaning("DECSCUSR", cursor_style(selector)),
        ([], b'q') => meaning("DECLL", format!("set the keyboard LEDs to {selector}")),
        ([], b'r') => meaning("DECSTBM", scrolling_region(fields)),
        ([b'$'], b'r') => meaning("DECCARA", "change attributes inside a rectangle"),
        ([b'$'], b't') => meaning("DECRARA", "reverse attributes inside a rectangle"),
        ([], b's') => meaning(
            "",
            "save the cursor position (ANSI.SYS), or set the left and right margins",
        ),
        ([], b't') => meaning("XTWINOPS", window_operation(fields)),
        ([], b'u') => meaning("", "restore the cursor position saved by CSI s"),
        ([b'$'], b'v') => meaning("DECCRA", "copy a rectangle of characters"),
        ([], b'x') => meaning("DECREQTPARM", "ask for the terminal's parameters"),
        ([b'$'], b'x') => meaning("DECFRA", "fill a rectangle with one character"),
        ([b'*'], b'y') => meaning("DECRQCRA", "ask for a checksum of a rectangle"),
        ([b'$'], b'z') => meaning("DECERA", "erase a rectangle"),
        ([b'$'], b'{') => meaning("DECSERA", "selectively erase a rectangle"),
        ([b'\''], b'}') => meaning("DECIC", format!("insert {}", count(first, "column"))),
        ([b'\''], b'~') => meaning("DECDC", format!("delete {}", count(first, "column"))),
        ([b'$'], b'|') => meaning("DECSCPP", format!("set the page to {} wide", count(first, "column"))),
        ([b'*'], b'|') => meaning("DECSNLS", format!("set the page to {} tall", count(first, "row"))),
        _ => None,
    }
}

/// `CSI ? …`: the DEC private modes, plus the private forms of erasing and
/// reporting.
fn csi_dec_private(fields: &Fields) -> Option<Meaning> {
    let selector = fields.value_or(0, 0);
    match (fields.intermediates.as_slice(), fields.final_byte) {
        ([], b'h') => meaning("DECSET", modes(fields, ModeKind::DecPrivate, true)),
        ([], b'l') => meaning("DECRST", modes(fields, ModeKind::DecPrivate, false)),
        ([b'$'], b'p') => meaning("DECRQM", format!("ask whether {} is set", mode_phrase(selector))),
        ([b'$'], b'y') => meaning(
            "DECRPM",
            format!(
                "a reply, not a request: {} is {}",
                mode_phrase(selector),
                mode_state(fields.value_or(1, 0))
            ),
        ),
        ([], b'J') => meaning("DECSED", format!("selectively {}", erase_in_display(selector))),
        ([], b'K') => meaning("DECSEL", format!("selectively {}", erase_in_line(selector))),
        ([], b'n') => meaning("DECDSR", device_status(selector)),
        ([], b'W') => meaning("DECST8C", "reset tab stops to every eighth column"),
        ([], b'i') => meaning("MC", media_copy(selector)),
        _ => None,
    }
}

/// `CSI > …`: secondary device attributes, xterm's modifier resources, kitty's
/// keyboard flags.
fn csi_greater(fields: &Fields) -> Option<Meaning> {
    match (fields.intermediates.as_slice(), fields.final_byte) {
        ([], b'c') => meaning("DA2", "ask for the terminal's firmware version"),
        ([], b'q') => meaning("XTVERSION", "ask the terminal for its name and version"),
        ([], b'm') => meaning("XTMODKEYS", "configure how modifiers are encoded in key reports"),
        ([], b'p') => meaning("XTSMPOINTER", "set when the mouse pointer hides while typing"),
        ([], b'u') => meaning(
            "",
            format!(
                "push keyboard protocol flags {} onto the stack (kitty)",
                fields.value_or(0, 0)
            ),
        ),
        ([], b'T') => meaning("", "set the title-mode flags"),
        _ => None,
    }
}

/// `CSI = …`: tertiary device attributes, and kitty's set-flags form.
fn csi_equals(fields: &Fields) -> Option<Meaning> {
    match (fields.intermediates.as_slice(), fields.final_byte) {
        ([], b'c') => meaning("DA3", "ask for the terminal's unit id"),
        ([], b'u') => meaning(
            "",
            format!(
                "set keyboard protocol flags to {} (kitty)",
                fields.value_or(0, 0)
            ),
        ),
        _ => None,
    }
}

/// `CSI < …`: kitty's pop-flags form, and the SGR mouse reports a capture picks up
/// from the *other* direction.
fn csi_less(fields: &Fields) -> Option<Meaning> {
    match (fields.intermediates.as_slice(), fields.final_byte) {
        ([], b'u') => meaning("", "pop keyboard protocol flags off the stack (kitty)"),
        ([], b'M' | b'm') => meaning(
            "",
            "an SGR mouse report — a reply from the terminal, not something an \
             application sends",
        ),
        _ => None,
    }
}

/// `CSI Ps J` and `CSI ? Ps J`.
fn erase_in_display(selector: u32) -> String {
    match selector {
        1 => "erase from the start of the screen to the cursor".to_owned(),
        2 => "erase the whole screen".to_owned(),
        3 => "erase the scrollback".to_owned(),
        _ => "erase from the cursor to the end of the screen".to_owned(),
    }
}

/// `CSI Ps K` and `CSI ? Ps K`.
fn erase_in_line(selector: u32) -> String {
    match selector {
        1 => "erase from the start of the line to the cursor".to_owned(),
        2 => "erase the whole line".to_owned(),
        _ => "erase from the cursor to the end of the line".to_owned(),
    }
}

/// `CSI Ps n`: the reports a program can ask for.
fn device_status(selector: u32) -> String {
    match selector {
        5 => "ask whether the terminal is in working order".to_owned(),
        6 => "ask where the cursor is — the measurement everything else builds on".to_owned(),
        15 => "ask about the printer".to_owned(),
        25 => "ask whether user-defined keys are locked".to_owned(),
        26 => "ask about the keyboard".to_owned(),
        996 => "ask which colour scheme the terminal is using".to_owned(),
        other => format!("device status report {other}"),
    }
}

/// `CSI Ps i`: media copy, which is also how xterm dumps the screen.
fn media_copy(selector: u32) -> String {
    match selector {
        0 => "print the screen".to_owned(),
        4 => "stop the print log".to_owned(),
        5 => "start the print log".to_owned(),
        10 => "write the screen to a file as HTML".to_owned(),
        11 => "write the screen to a file as SVG".to_owned(),
        other => format!("media copy {other}"),
    }
}

/// `CSI Ps SP q`: DECSCUSR's cursor shapes.
fn cursor_style(selector: u32) -> String {
    match selector {
        0 | 1 => "a blinking block cursor".to_owned(),
        2 => "a steady block cursor".to_owned(),
        3 => "a blinking underline cursor".to_owned(),
        4 => "a steady underline cursor".to_owned(),
        5 => "a blinking bar cursor".to_owned(),
        6 => "a steady bar cursor".to_owned(),
        other => format!("cursor style {other}"),
    }
}

/// `CSI Pt ; Pb r`: the scrolling region.
fn scrolling_region(fields: &Fields) -> String {
    match (fields.value(0), fields.value(1)) {
        (Some(top), Some(bottom)) => {
            format!("confine scrolling to rows {top} through {bottom}")
        }
        _ => "let scrolling use the whole screen again".to_owned(),
    }
}

/// `CSI Ps t`: xterm's window operations, including the size reports.
fn window_operation(fields: &Fields) -> String {
    match fields.value_or(0, 0) {
        1 => "de-iconify the window".to_owned(),
        2 => "iconify the window".to_owned(),
        3 => "move the window".to_owned(),
        4 => "resize the window in pixels".to_owned(),
        5 => "raise the window".to_owned(),
        6 => "lower the window".to_owned(),
        7 => "redraw the window".to_owned(),
        8 => "resize the window in characters".to_owned(),
        9 => "maximize or restore the window".to_owned(),
        10 => "enter or leave full screen".to_owned(),
        11 => "ask whether the window is iconified".to_owned(),
        13 => "ask where the window is".to_owned(),
        14 => "ask how big the text area is in pixels".to_owned(),
        15 => "ask how big the screen is in pixels".to_owned(),
        16 => "ask how big one character cell is in pixels".to_owned(),
        18 => "ask how big the text area is in characters".to_owned(),
        19 => "ask how big the screen is in characters".to_owned(),
        20 => "ask for the icon label".to_owned(),
        21 => "ask for the window title".to_owned(),
        22 => "push the window title onto the title stack".to_owned(),
        23 => "pop the window title off the title stack".to_owned(),
        other => format!("window operation {other}"),
    }
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

/// Which registry a mode number belongs to.
///
/// The two are entirely separate number spaces, and the `?` is what tells them
/// apart: ANSI mode 4 is insert/replace, while DEC private mode 4 is smooth
/// scrolling. Reading one as the other is a classic misdiagnosis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeKind {
    /// `CSI Ps h` — the standard modes.
    Ansi,
    /// `CSI ? Ps h` — the DEC private modes.
    DecPrivate,
}

/// How a mode reads when it is being switched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verbs {
    /// enable / disable
    Enable,
    /// enter / leave
    Enter,
    /// show / hide
    Show,
    /// begin / end
    Begin,
}

impl Verbs {
    /// The verb for setting, or for resetting.
    fn word(self, set: bool) -> &'static str {
        match (self, set) {
            (Self::Enable, true) => "enable",
            (Self::Enable, false) => "disable",
            (Self::Enter, true) => "enter",
            (Self::Enter, false) => "leave",
            (Self::Show, true) => "show",
            (Self::Show, false) => "hide",
            (Self::Begin, true) => "begin",
            (Self::Begin, false) => "end",
        }
    }
}

/// Describes every mode a `SM`, `RM`, `DECSET` or `DECRST` names.
///
/// One sequence can carry several modes — `CSI ? 1000 ; 1002 ; 1006 h` is how
/// mouse reporting is usually switched on — so they are all named, in order.
fn modes(fields: &Fields, kind: ModeKind, set: bool) -> String {
    let described: Vec<String> = fields
        .params
        .iter()
        .map(|param| match param.value() {
            Some(number) => mode_action(number, kind, set),
            None => "an omitted mode number".to_owned(),
        })
        .collect();
    if described.is_empty() {
        return format!("{} nothing: no mode number was given", if set { "set" } else { "reset" });
    }
    described.join(", then ")
}

/// What setting or resetting one mode does.
fn mode_action(number: u32, kind: ModeKind, set: bool) -> String {
    let entry = match kind {
        ModeKind::Ansi => ansi_mode(number),
        ModeKind::DecPrivate => dec_mode(number),
    };
    match entry {
        Some((_, subject, verbs)) => format!("{} {subject}", verbs.word(set)),
        None => match kind {
            ModeKind::Ansi if set => format!("set ANSI mode {number}, which is not in this table"),
            ModeKind::Ansi => format!("reset ANSI mode {number}, which is not in this table"),
            ModeKind::DecPrivate if set => {
                format!("set private mode {number}, which is not in this table")
            }
            ModeKind::DecPrivate => {
                format!("reset private mode {number}, which is not in this table")
            }
        },
    }
}

/// A mode as a noun phrase, for the query and report forms.
#[must_use]
pub fn mode_phrase(number: u32) -> String {
    match dec_mode(number) {
        Some(("", subject, _)) => format!("{subject} (mode {number})"),
        Some((mnemonic, subject, _)) => format!("{subject} ({mnemonic}, mode {number})"),
        None => format!("private mode {number}"),
    }
}

/// The states DECRPM reports.
fn mode_state(value: u32) -> &'static str {
    match value {
        0 => "not recognised",
        1 => "set",
        2 => "reset",
        3 => "permanently set",
        4 => "permanently reset",
        _ => "reported with a value this table does not know",
    }
}

/// The standard modes, by number.
fn ansi_mode(number: u32) -> Option<(&'static str, &'static str, Verbs)> {
    match number {
        2 => Some(("KAM", "the keyboard lock", Verbs::Enable)),
        3 => Some(("CRM", "showing control characters instead of acting on them", Verbs::Enable)),
        4 => Some(("IRM", "insert mode, where typing pushes text right", Verbs::Enable)),
        12 => Some(("SRM", "local echo", Verbs::Enable)),
        20 => Some(("LNM", "treating a line feed as a new line", Verbs::Enable)),
        _ => None,
    }
}

/// The DEC private modes worth naming.
///
/// Deliberately not exhaustive — the registry has hundreds of entries, most of
/// them unimplemented — but everything a modern application actually sets is
/// here, plus the several ways of reaching the alternate screen, which are the
/// commonest thing to see and the easiest to confuse.
fn dec_mode(number: u32) -> Option<(&'static str, &'static str, Verbs)> {
    let entry = match number {
        1 => ("DECCKM", "application cursor keys, where arrows send ESC O A", Verbs::Enable),
        2 => ("DECANM", "VT52 compatibility mode", Verbs::Enable),
        3 => ("DECCOLM", "132-column mode, which clears the screen", Verbs::Enable),
        4 => ("DECSCLM", "smooth scrolling", Verbs::Enable),
        5 => ("DECSCNM", "reverse video for the whole screen", Verbs::Enable),
        6 => ("DECOM", "origin mode, where row 1 is the top of the scrolling region", Verbs::Enable),
        7 => ("DECAWM", "autowrap at the right margin", Verbs::Enable),
        8 => ("DECARM", "auto-repeat when a key is held", Verbs::Enable),
        9 => ("", "X10 mouse reporting, which reports presses only", Verbs::Enable),
        12 => ("", "a blinking cursor", Verbs::Enable),
        25 => ("DECTCEM", "the cursor", Verbs::Show),
        45 => ("", "reverse wraparound", Verbs::Enable),
        47 => ("", "the alternate screen, without saving the cursor", Verbs::Enter),
        66 => ("DECNKM", "the application keypad", Verbs::Enable),
        67 => ("DECBKM", "sending backspace rather than delete", Verbs::Enable),
        69 => ("DECLRMM", "left and right margins", Verbs::Enable),
        80 => ("DECSDM", "sixel scrolling", Verbs::Enable),
        1000 => ("", "mouse reporting for presses and releases", Verbs::Enable),
        1001 => ("", "mouse highlight tracking", Verbs::Enable),
        1002 => ("", "mouse reporting including drags", Verbs::Enable),
        1003 => ("", "mouse reporting for every movement", Verbs::Enable),
        1004 => ("", "focus in and focus out reporting", Verbs::Enable),
        1005 => ("", "UTF-8 mouse coordinates, which are ambiguous past column 95", Verbs::Enable),
        1006 => ("", "SGR mouse coordinates, which work past column 223", Verbs::Enable),
        1015 => ("", "urxvt mouse coordinates", Verbs::Enable),
        1016 => ("", "SGR-pixel mouse coordinates", Verbs::Enable),
        1034 => ("", "treating the eighth bit as a meta modifier", Verbs::Enable),
        1047 => ("", "the alternate screen, clearing it on the way out", Verbs::Enter),
        1048 => ("", "saving the cursor position", Verbs::Enable),
        1049 => ("", "the alternate screen", Verbs::Enter),
        2004 => ("", "bracketed paste, which fences pasted text", Verbs::Enable),
        2026 => ("", "synchronized output, so a frame is presented at once", Verbs::Begin),
        2027 => ("", "grapheme clustering by UAX #29", Verbs::Enable),
        2031 => ("", "notifications when the colour scheme changes", Verbs::Enable),
        2048 => ("", "in-band resize notifications", Verbs::Enable),
        7727 => ("", "application escape mode (mintty)", Verbs::Enable),
        9001 => ("", "win32 input mode", Verbs::Enable),
        _ => return None,
    };
    Some(entry)
}

// ---------------------------------------------------------------------------
// OSC
// ---------------------------------------------------------------------------

/// Names an `OSC` from its `;`-separated fields.
///
/// The `OSC` namespace is a registry only by custom: numbers were claimed by
/// whoever got there first, two of them twice. `OSC 9` is iTerm2's notification
/// *and* ConEmu's progress report depending on whether the next field is `4`,
/// which is exactly the sort of thing the feature database knows and this table
/// only has to fall back on.
#[must_use]
pub fn osc(fields: &[&str]) -> Option<Meaning> {
    let number = fields.first().copied().unwrap_or_default();
    let rest = |index: usize| fields.get(index).copied().unwrap_or_default();
    let joined = |index: usize| fields.get(index..).unwrap_or_default().join(";");
    match number {
        "0" => meaning("", format!("set the window title and icon name to {}", short(&joined(1)))),
        "1" => meaning("", format!("set the icon name to {}", short(&joined(1)))),
        "2" => meaning("", format!("set the window title to {}", short(&joined(1)))),
        "4" => meaning("", palette(fields)),
        "7" => meaning("", format!("report the working directory as {}", short(&joined(1)))),
        "8" => meaning("", hyperlink(fields)),
        "9" if rest(1) == "4" => meaning("", "report progress to the taskbar (ConEmu's OSC 9;4)"),
        "9" => meaning("", format!("raise a notification saying {}", short(&joined(1)))),
        "10" => meaning("", color_slot("foreground", rest(1))),
        "11" => meaning("", color_slot("background", rest(1))),
        "12" => meaning("", color_slot("cursor", rest(1))),
        "17" => meaning("", color_slot("selection background", rest(1))),
        "22" => meaning("", format!("set the mouse pointer shape to {}", short(&joined(1)))),
        "52" => meaning("", clipboard(fields)),
        "66" => meaning("", "draw text at a multiple of the cell size (kitty text sizing)"),
        "99" => meaning("", "raise a notification (kitty's protocol)"),
        "104" => meaning(
            "",
            if fields.len() > 1 {
                format!("reset palette entry {}", short(&joined(1)))
            } else {
                "reset every palette entry".to_owned()
            },
        ),
        "110" => meaning("", "reset the foreground colour to the default"),
        "111" => meaning("", "reset the background colour to the default"),
        "112" => meaning("", "reset the cursor colour to the default"),
        "133" => meaning("", shell_integration(rest(1))),
        "777" => meaning("", format!("call the urxvt extension {}", short(rest(1)))),
        "1337" => meaning("", format!("iTerm2 proprietary command {}", short(rest(1)))),
        other if other.chars().all(|c| c.is_ascii_digit()) && !other.is_empty() => None,
        _ => meaning("", "an OSC whose first field is not a number"),
    }
}

/// `OSC 8`: the hyperlink, whose empty form is the one people forget.
fn hyperlink(fields: &[&str]) -> String {
    let params = fields.get(1).copied().unwrap_or_default();
    let uri = fields.get(2..).unwrap_or_default().join(";");
    if uri.is_empty() {
        return "end the current hyperlink".to_owned();
    }
    if params.is_empty() {
        format!("start a hyperlink to {}", short(&uri))
    } else {
        format!("start a hyperlink to {} with parameters {}", short(&uri), short(params))
    }
}

/// `OSC 4`: palette entries, set or asked about.
fn palette(fields: &[&str]) -> String {
    match (fields.get(1), fields.get(2)) {
        (Some(index), Some(&"?")) => format!("ask what palette entry {} is", short(index)),
        (Some(index), Some(spec)) => {
            format!("set palette entry {} to {}", short(index), short(spec))
        }
        (Some(index), None) => format!("palette entry {}, with no value", short(index)),
        _ => "a palette command with no entry number".to_owned(),
    }
}

/// `OSC 10`, `11`, `12`: a colour slot, set or asked about.
fn color_slot(slot: &str, value: &str) -> String {
    if value == "?" {
        format!("ask what the {slot} colour is")
    } else if value.is_empty() {
        format!("the {slot} colour, with no value")
    } else {
        format!("set the {slot} colour to {}", short(value))
    }
}

/// `OSC 52`: the clipboard, which is the one sequence that can read data back out
/// of the user's session.
fn clipboard(fields: &[&str]) -> String {
    let selection = match fields.get(1).copied().unwrap_or_default() {
        "" | "s0" => "the selection or clipboard".to_owned(),
        "c" => "the clipboard".to_owned(),
        "p" => "the primary selection".to_owned(),
        "q" => "the secondary selection".to_owned(),
        other => format!("selection {}", short(other)),
    };
    match fields.get(2).copied() {
        Some("?") => format!("read {selection} — a sequence that can exfiltrate data"),
        Some(data) => format!(
            "put {} of base64 onto {selection}",
            count_bytes(data.len())
        ),
        None => format!("a clipboard command for {selection} with no payload"),
    }
}

/// `OSC 133`: the shell integration marks, which is how a terminal learns where
/// prompts and output begin.
fn shell_integration(mark: &str) -> String {
    match mark {
        "A" => "mark the start of a prompt".to_owned(),
        "B" => "mark the start of the typed command".to_owned(),
        "C" => "mark the start of the command's output".to_owned(),
        "D" => "mark the end of the command".to_owned(),
        other => format!("shell integration mark {}", short(other)),
    }
}

// ---------------------------------------------------------------------------
// DCS, APC, PM, SOS
// ---------------------------------------------------------------------------

/// Names a `DCS` from its prefix and payload.
///
/// Sixel is the one to recognise: `DCS Ps ; Ps ; Ps q` followed by kilobytes of
/// what looks like line noise is an image, not corruption.
#[must_use]
pub fn dcs(fields: &Fields, payload: &str) -> Option<Meaning> {
    match (fields.intermediates.as_slice(), fields.final_byte) {
        ([], b'q') => meaning("", "sixel image data"),
        ([b'$'], b'q') => meaning(
            "DECRQSS",
            format!("ask the terminal to report its current setting for {}", short(payload)),
        ),
        ([b'$'], b'r') => meaning(
            "DECRPSS",
            format!(
                "a reply, not a request: the current setting is {}",
                short(payload)
            ),
        ),
        ([b'+'], b'q') => meaning("XTGETTCAP", xtgettcap(payload)),
        ([b'+'], b'r' | b'p') => meaning("XTGETTCAP", "a reply carrying terminfo capabilities"),
        ([], b'|') => meaning("DECUDK", "define the user-defined keys"),
        ([b'$'], b'|') => meaning("DECRQSS", "ask about the status line"),
        ([], b'{') => meaning("DECDLD", "download a soft character set"),
        ([], b't') if payload.starts_with("mux;") => meaning(
            "",
            "a tmux passthrough wrapper: the payload is a sequence for the terminal \
             outside tmux, with every ESC doubled",
        ),
        _ => None,
    }
}

/// `DCS + q <hex names> ST`: the capability names are hex-encoded, so they are
/// decoded here — the whole point of the query is which names were asked for.
fn xtgettcap(payload: &str) -> String {
    let names: Vec<String> = payload
        .split(';')
        .map(|hex| match decode_hex(hex) {
            Some(name) => short(&name),
            None => short(hex),
        })
        .collect();
    if names.is_empty() {
        return "ask for terminfo capabilities".to_owned();
    }
    format!("ask the terminal for the terminfo capabilities {}", names.join(", "))
}

/// Decodes an even-length run of hex digits into text.
fn decode_hex(hex: &str) -> Option<String> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut out = String::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for pair in bytes.chunks(2) {
        let text = std::str::from_utf8(pair).ok()?;
        let byte = u8::from_str_radix(text, 16).ok()?;
        out.push(char::from(byte));
    }
    Some(out)
}

/// Names an `APC` payload when no feature file claims it.
///
/// Both known users of `APC` are in the database — kitty graphics under `G`, the
/// Glyph Protocol under `25a1` — so this only has to say something honest about
/// the rest.
#[must_use]
pub fn apc(payload: &str) -> Option<Meaning> {
    if payload.is_empty() {
        return meaning("", "an empty application program command");
    }
    None
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Quotes and shortens a fragment lifted out of a payload.
fn short(text: &str) -> String {
    super::display::quote_short(text, 48)
}

/// `1 byte` but `4 bytes`.
fn count_bytes(len: usize) -> String {
    if len == 1 {
        "1 byte".to_owned()
    } else {
        format!("{len} bytes")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use super::super::parse::{Item, Outcome, parse_item};

    /// The `Fields` of a `CSI`, for driving the tables.
    fn fields(bytes: &[u8]) -> Fields {
        match parse_item(bytes, true) {
            Outcome::Item {
                item: Item::Csi(fields),
                ..
            }
            | Outcome::Item {
                item: Item::Dcs { fields, .. },
                ..
            } => fields,
            other => panic!("expected a CSI or DCS, got {other:?}"),
        }
    }

    fn summary(bytes: &[u8]) -> String {
        csi(&fields(bytes)).map(|m| m.summary).unwrap_or_default()
    }

    fn mnemonic(bytes: &[u8]) -> String {
        csi(&fields(bytes))
            .map(|m| m.mnemonic.to_owned())
            .unwrap_or_default()
    }

    #[test]
    fn the_mode_from_the_brief_reads_as_the_brief_asks() {
        assert_eq!(summary(b"\x1b[?1049h"), "enter the alternate screen");
        assert_eq!(summary(b"\x1b[?1049l"), "leave the alternate screen");
        assert_eq!(mnemonic(b"\x1b[?1049h"), "DECSET");
    }

    #[test]
    fn cursor_movement_counts_correctly() {
        assert_eq!(summary(b"\x1b[A"), "move the cursor up 1 row");
        assert_eq!(summary(b"\x1b[3A"), "move the cursor up 3 rows");
        assert_eq!(summary(b"\x1b[10;20H"), "move the cursor to row 10, column 20");
        assert_eq!(summary(b"\x1b[H"), "move the cursor to row 1, column 1");
    }

    #[test]
    fn erasing_distinguishes_its_three_directions() {
        assert_eq!(summary(b"\x1b[J"), "erase from the cursor to the end of the screen");
        assert_eq!(summary(b"\x1b[1J"), "erase from the start of the screen to the cursor");
        assert_eq!(summary(b"\x1b[2J"), "erase the whole screen");
        assert_eq!(summary(b"\x1b[3J"), "erase the scrollback");
    }

    #[test]
    fn the_two_mode_registries_are_kept_apart() {
        // Mode 4 means entirely different things with and without the `?`.
        assert!(summary(b"\x1b[4h").contains("insert mode"));
        assert!(summary(b"\x1b[?4h").contains("smooth scrolling"));
    }

    #[test]
    fn several_modes_in_one_sequence_are_all_named() {
        let text = summary(b"\x1b[?1000;1002;1006h");
        assert!(text.contains("presses and releases"), "{text}");
        assert!(text.contains("drags"), "{text}");
        assert!(text.contains("SGR mouse"), "{text}");
    }

    #[test]
    fn an_unknown_mode_says_so_rather_than_inventing_a_meaning() {
        let text = summary(b"\x1b[?4242h");
        assert!(text.contains("4242"), "{text}");
        assert!(text.contains("not in this table"), "{text}");
    }

    #[test]
    fn mode_queries_and_reports_are_told_apart() {
        assert!(summary(b"\x1b[?2027$p").starts_with("ask whether"));
        let report = summary(b"\x1b[?2026;2$y");
        assert!(report.starts_with("a reply"), "{report}");
        assert!(report.ends_with("is reset"), "{report}");
    }

    #[test]
    fn the_screen_capture_sequences_are_named() {
        assert_eq!(summary(b"\x1b[10i"), "write the screen to a file as HTML");
        assert_eq!(summary(b"\x1b[11i"), "write the screen to a file as SVG");
    }

    #[test]
    fn window_operations_include_the_size_queries() {
        assert_eq!(summary(b"\x1b[18t"), "ask how big the text area is in characters");
        assert_eq!(summary(b"\x1b[16t"), "ask how big one character cell is in pixels");
        assert_eq!(mnemonic(b"\x1b[18t"), "XTWINOPS");
    }

    #[test]
    fn kitty_keyboard_and_unscroll_have_their_private_forms() {
        assert!(summary(b"\x1b[>1u").contains("push keyboard protocol flags 1"));
        assert!(summary(b"\x1b[<u").contains("pop keyboard protocol flags"));
        assert!(summary(b"\x1b[3+T").contains("3 lines of scrollback"));
    }

    #[test]
    fn a_cursor_position_report_is_marked_as_a_reply() {
        // A capture of a whole session carries the terminal's answers as well as
        // the application's requests, and mistaking one for the other is exactly
        // the confusion a decoder should not add to.
        let text = summary(b"\x1b[3;5R");
        assert!(text.starts_with("a reply"), "{text}");
        assert!(text.contains("row 3, column 5"), "{text}");
        assert_eq!(mnemonic(b"\x1b[3;5R"), "CPR");
    }

    #[test]
    fn an_sgr_mouse_report_is_marked_as_a_reply() {
        assert!(summary(b"\x1b[<0;10;20M").contains("a reply from the terminal"));
    }

    #[test]
    fn cursor_shapes_are_named() {
        assert_eq!(summary(b"\x1b[5 q"), "a blinking bar cursor");
        assert_eq!(mnemonic(b"\x1b[5 q"), "DECSCUSR");
    }

    #[test]
    fn control_bytes_are_named_and_explained() {
        assert_eq!(control(0x07).mnemonic, "BEL");
        assert_eq!(control(0x0a).mnemonic, "LF");
        assert_eq!(control(0x7f).mnemonic, "DEL");
        assert!(control(0x00).summary.contains("never meaningful"));
        // Every C0 byte and DEL has a name; nothing falls through.
        for byte in (0u8..0x20).chain(std::iter::once(0x7f)) {
            assert!(!control(byte).mnemonic.is_empty(), "{byte:#04x}");
        }
    }

    #[test]
    fn eight_bit_c1_bytes_name_their_seven_bit_form() {
        assert_eq!(eight_bit_c1(0x9b), "CSI");
        assert_eq!(eight_bit_c1(0x9f), "APC");
        assert_eq!(eight_bit_c1(0x81), "");
    }

    #[test]
    fn the_two_byte_escapes_are_named() {
        assert_eq!(escape_sequence(&[], b'7').unwrap().mnemonic, "DECSC");
        assert_eq!(escape_sequence(&[], b'c').unwrap().mnemonic, "RIS");
        assert_eq!(escape_sequence(&[], b'D').unwrap().mnemonic, "IND");
        let stray = escape_sequence(&[], b'\\').unwrap();
        assert_eq!(stray.mnemonic, "ST");
        assert!(stray.summary.contains("no control string open"));
    }

    #[test]
    fn charset_designation_names_both_the_slot_and_the_set() {
        let graphics = escape_sequence(b"(", b'0').unwrap();
        assert_eq!(graphics.mnemonic, "SCS");
        assert!(graphics.summary.contains("line-drawing set"), "{graphics:?}");
        assert!(graphics.summary.contains("G0"), "{graphics:?}");
        let ascii = escape_sequence(b"(", b'B').unwrap();
        assert!(ascii.summary.contains("US ASCII"), "{ascii:?}");
        let g1 = escape_sequence(b")", b'0').unwrap();
        assert!(g1.summary.contains("G1"), "{g1:?}");
    }

    #[test]
    fn osc_titles_hyperlinks_and_the_empty_hyperlink() {
        let title = osc(&["0", "my window"]).unwrap();
        assert_eq!(title.summary, "set the window title and icon name to \"my window\"");
        let open = osc(&["8", "", "https://example.com"]).unwrap();
        assert_eq!(open.summary, "start a hyperlink to \"https://example.com\"");
        let close = osc(&["8", "", ""]).unwrap();
        assert_eq!(close.summary, "end the current hyperlink");
        let with_id = osc(&["8", "id=42", "https://example.com"]).unwrap();
        assert!(with_id.summary.contains("parameters \"id=42\""), "{with_id:?}");
    }

    #[test]
    fn a_uri_containing_a_semicolon_is_not_cut_in_half() {
        let open = osc(&["8", "", "https://example.com/a", "b"]).unwrap();
        assert!(open.summary.contains("a;b"), "{open:?}");
    }

    #[test]
    fn osc_nine_is_disambiguated_by_its_second_field() {
        assert!(osc(&["9", "4", "1", "42"]).unwrap().summary.contains("progress"));
        assert!(
            osc(&["9", "build finished"])
                .unwrap()
                .summary
                .contains("notification")
        );
    }

    #[test]
    fn colour_and_palette_queries_are_told_from_settings() {
        assert_eq!(
            osc(&["11", "?"]).unwrap().summary,
            "ask what the background colour is"
        );
        assert_eq!(
            osc(&["10", "rgb:ff/ff/ff"]).unwrap().summary,
            "set the foreground colour to \"rgb:ff/ff/ff\""
        );
        assert!(osc(&["4", "1", "?"]).unwrap().summary.starts_with("ask what palette entry"));
        assert!(osc(&["104"]).unwrap().summary.contains("every palette entry"));
    }

    #[test]
    fn a_clipboard_read_is_called_out_as_exfiltration() {
        let read = osc(&["52", "c", "?"]).unwrap();
        assert!(read.summary.contains("exfiltrate"), "{read:?}");
        let write = osc(&["52", "c", "aGVsbG8="]).unwrap();
        assert!(write.summary.contains("8 bytes of base64"), "{write:?}");
    }

    #[test]
    fn an_unclaimed_osc_number_is_left_to_the_database() {
        assert!(osc(&["5379", "x"]).is_none());
    }

    #[test]
    fn dcs_names_sixel_decrqss_and_the_tmux_wrapper() {
        assert_eq!(dcs(&fields(b"\x1bP0;1;0q\x1b\\"), "").unwrap().summary, "sixel image data");
        let decrqss = dcs(&fields(b"\x1bP$qm\x1b\\"), "m").unwrap();
        assert_eq!(decrqss.mnemonic, "DECRQSS");
        assert!(decrqss.summary.contains("\"m\""), "{decrqss:?}");
        let tmux = dcs(&fields(b"\x1bPtmux;x\x1b\\"), "mux;x").unwrap();
        assert!(tmux.summary.contains("tmux passthrough"), "{tmux:?}");
    }

    #[test]
    fn xtgettcap_decodes_the_hex_names_it_asks_about() {
        // `524742` is `RGB`, the capability that says a terminal takes 24-bit
        // colour. Leaving it as hex would defeat the purpose of decoding.
        let query = dcs(&fields(b"\x1bP+q524742\x1b\\"), "524742").unwrap();
        assert_eq!(query.mnemonic, "XTGETTCAP");
        assert!(query.summary.contains("\"RGB\""), "{query:?}");
        let two = dcs(&fields(b"\x1bP+q524742;5463\x1b\\"), "524742;5463").unwrap();
        assert!(two.summary.contains("\"Tc\""), "{two:?}");
    }

    #[test]
    fn no_summary_in_any_table_carries_a_control_byte() {
        // These strings are printed as prose. One raw ESC here would be executed.
        let mut summaries = Vec::new();
        for byte in 0u8..0x20 {
            summaries.push(control(byte).summary);
        }
        for number in [1u32, 25, 1049, 2026, 2027, 4242] {
            summaries.push(mode_phrase(number));
        }
        for bytes in [
            &b"\x1b[?1049h"[..],
            b"\x1b[38:2::1:2:3m",
            b"\x1b[10;20H",
            b"\x1b[18t",
        ] {
            if let Some(found) = csi(&fields(bytes)) {
                summaries.push(found.summary);
            }
        }
        for summary in summaries {
            assert!(
                !summary.chars().any(char::is_control),
                "control byte in {summary:?}"
            );
        }
    }
}
