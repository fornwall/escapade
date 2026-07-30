//! Terminal input: keys, mouse reports and terminal replies to escape-sequence
//! queries.
//!
//! The module has two halves:
//!
//! * [`parse_event`] is a pure function that turns a byte slice into one
//!   [`Event`]. It has no dependency on a terminal, so demo steps and tests can
//!   drive it with recorded bytes.
//! * [`InputReader`] owns a background thread that reads `stdin`, feeds
//!   [`parse_event`], and adds the timing rules a real terminal needs (escape
//!   disambiguation, push-back of user input while waiting for a query reply).
//!
//! Putting the terminal into raw mode is *not* done here; the reader assumes it
//! has already happened.
//!
//! Everything is implemented with `std` only, and without `unsafe`.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

/// How long [`InputReader`] waits for more bytes before it gives up on an
/// unfinished escape sequence.
///
/// A lone `ESC` keypress is byte-for-byte identical to the start of every
/// escape sequence (`ESC [ …`, `ESC O …`, `ESC _ …`), so it can only be
/// recognised by the *absence* of following bytes: real sequences arrive from
/// the terminal in a single burst, a human pressing Escape does not. After this
/// window a stalled buffer is resolved instead of blocking forever — a single
/// `ESC` becomes [`KeyCode::Esc`], anything longer becomes [`Event::Unknown`]
/// so a demo can show the bytes rather than swallow them.
pub const ESC_TIMEOUT: Duration = Duration::from_millis(50);

/// `ESC`, the escape byte that introduces every sequence.
const ESC: u8 = 0x1b;
/// `BEL`, accepted as a string terminator for `OSC`/`APC`/`DCS` payloads.
const BEL: u8 = 0x07;
/// Final byte of the 7-bit string terminator `ESC \` (`ST`).
const ST_FINAL: u8 = b'\\';
/// Bracketed paste introducer: `CSI 200~`.
const PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed paste terminator: `CSI 201~`.
const PASTE_END: &[u8] = b"\x1b[201~";
/// Bytes read from `stdin` per syscall. One burst of a large paste easily
/// exceeds this; the reader simply loops.
const READ_CHUNK: usize = 4096;
/// X10 mouse reports bias every value by `0x20` so it stays out of the C0 range.
const X10_BIAS: u8 = 32;
/// Number of bytes that follow the `M` of an X10 mouse report (button, col, row).
const X10_ARGS: usize = 3;

// ---------------------------------------------------------------------------
// Event model
// ---------------------------------------------------------------------------

/// One decoded terminal input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A key press, repeat or release.
    Key(KeyEvent),
    /// A mouse button, wheel or motion report.
    Mouse(MouseEvent),
    /// Terminal answered a query we sent.
    Reply(Reply),
    /// Text delivered inside bracketed paste (`CSI 200~ … CSI 201~`).
    Paste(String),
    /// The terminal window gained focus (`CSI I`).
    FocusGained,
    /// The terminal window lost focus (`CSI O`).
    FocusLost,
    /// Bytes that could not be interpreted (kept so demos can show them).
    Unknown(Vec<u8>),
}

/// A reply sent by the terminal in answer to a query escape sequence.
///
/// Capability probing depends on these: the program writes a query and then
/// uses [`InputReader::wait_for_reply`] to pick the answer out of the input
/// stream without losing whatever the user typed meanwhile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// `APC` payload: bytes between `ESC _` and the string terminator.
    ///
    /// The Glyph Protocol answers here, e.g.
    /// `ESC _ 25a1;s;fmt=glyf ESC \`.
    Apc(String),
    /// `DCS` payload: between `ESC P` and `ST`.
    Dcs(String),
    /// `OSC` payload: between `ESC ]` and `ST` (or `BEL`).
    Osc(String),
    /// A `CSI`-shaped device report, e.g. Primary Device Attributes
    /// `CSI ? 62 ; 4 c`.
    ///
    /// `params` is the raw parameter text including any private prefix
    /// (`"?62;4"`), `final_byte` is `'c'`.
    Csi {
        /// Raw parameter text, private prefix included, intermediates excluded.
        params: String,
        /// The final byte that ended the sequence.
        final_byte: char,
    },
}

/// A key event: which key, which modifiers, and whether it was a press,
/// an auto-repeat or a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// The key itself.
    pub code: KeyCode,
    /// Modifiers held down while the key was pressed.
    pub mods: Modifiers,
    /// Press, repeat or release.
    pub kind: KeyEventKind,
}

/// Whether a [`KeyEvent`] is a press, an auto-repeat or a release.
///
/// Legacy terminal input only reports presses. `Repeat` and `Release` only
/// arrive under the kitty keyboard protocol, once the `report event types`
/// flag (`0b10`) has been enabled with `CSI > 2 u`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventKind {
    /// Key went down (kitty event type `1`, and every legacy key).
    Press,
    /// Key is auto-repeating (kitty event type `2`).
    Repeat,
    /// Key came up (kitty event type `3`).
    Release,
}

/// A keyboard key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    /// A character-producing key, reported as the character it produces.
    Char(char),
    /// Return / Enter.
    Enter,
    /// Tab.
    Tab,
    /// Shift-Tab (`CSI Z`).
    BackTab,
    /// Backspace.
    Backspace,
    /// Escape.
    Esc,
    /// Delete (forward delete).
    Delete,
    /// Insert.
    Insert,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Function key `n`, 1-based (`F(1)` is F1).
    F(u8),
}

/// Modifier keys held while an event was generated.
///
/// Terminals put these on the wire as a `1 + bitmask` parameter; see
/// [`Modifiers::from_csi_param`].
//
// Four independent `bool`s are deliberate: they mirror the four modifier bits a
// terminal reports and keep pattern matching in demo steps obvious. A bitflags
// newtype would hide exactly the wire format this crate exists to show.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Alt (also reported as Meta by some terminals).
    pub alt: bool,
    /// Control.
    pub ctrl: bool,
    /// Super (Command / Windows key). Only reported by protocols that support
    /// it, such as the kitty keyboard protocol.
    pub super_: bool,
}

impl Modifiers {
    /// No modifiers at all.
    pub const NONE: Self = Self {
        shift: false,
        alt: false,
        ctrl: false,
        super_: false,
    };

    /// Bit for Shift in the `CSI` modifier encoding.
    const BIT_SHIFT: u8 = 0b0001;
    /// Bit for Alt in the `CSI` modifier encoding.
    const BIT_ALT: u8 = 0b0010;
    /// Bit for Ctrl in the `CSI` modifier encoding.
    const BIT_CTRL: u8 = 0b0100;
    /// Bit for Super in the `CSI` modifier encoding.
    const BIT_SUPER: u8 = 0b1000;

    /// Decodes the standard `1 + bitmask` modifier parameter used by both the
    /// legacy `CSI 1 ; <mods> A` forms and the kitty `CSI … ; <mods> u` form.
    ///
    /// The bits are shift `1`, alt `2`, ctrl `4`, super `8`. kitty also defines
    /// hyper `16`, meta `32`, caps lock `64` and num lock `128`; those have no
    /// field here and are ignored. A parameter of `0` (some terminals send it
    /// instead of `1`) means "no modifiers".
    #[must_use]
    pub const fn from_csi_param(param: u8) -> Self {
        let bits = param.saturating_sub(1);
        Self {
            shift: bits & Self::BIT_SHIFT != 0,
            alt: bits & Self::BIT_ALT != 0,
            ctrl: bits & Self::BIT_CTRL != 0,
            super_: bits & Self::BIT_SUPER != 0,
        }
    }

    /// Returns `true` when no modifier is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.shift && !self.alt && !self.ctrl && !self.super_
    }
}

impl fmt::Display for Modifiers {
    /// Renders modifiers in a stable order, e.g. `Ctrl+Shift`. Renders as the
    /// empty string when no modifier is set.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (active, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.super_, "Super"),
        ] {
            if active {
                if !first {
                    f.write_str("+")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// A mouse report. `col`/`row` are 1-based, exactly as the terminal reports
/// them, so `(1, 1)` is the top-left cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// What the mouse did.
    pub kind: MouseKind,
    /// 1-based column.
    pub col: u16,
    /// 1-based row.
    pub row: u16,
    /// Modifiers held during the report.
    pub mods: Modifiers,
}

/// What a [`MouseEvent`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    /// Button pressed.
    Down(MouseButton),
    /// Button released.
    Up(MouseButton),
    /// Motion with a button held.
    Drag(MouseButton),
    /// Motion with no button held (needs "any event" mouse mode, `CSI ?1003h`).
    Moved,
    /// Wheel up (button code `64`).
    ScrollUp,
    /// Wheel down (button code `65`).
    ScrollDown,
    /// Wheel/trackpad left (button code `66`).
    ScrollLeft,
    /// Wheel/trackpad right (button code `67`).
    ScrollRight,
}

/// The three mouse buttons terminals distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// Left button (button bits `0`).
    Left,
    /// Middle button (button bits `1`).
    Middle,
    /// Right button (button bits `2`).
    Right,
}

/// What the user asked for while a demo step was waiting.
///
/// See [`InputReader::wait_for_advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    /// Go to the next step.
    Next,
    /// Go back to the previous step.
    Prev,
    /// Leave the program.
    Quit,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Result of [`parse_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Consumed `consumed` bytes and produced an event.
    Event {
        /// The decoded event.
        event: Event,
        /// How many bytes from the front of the input it used.
        consumed: usize,
    },
    /// A valid prefix of a longer sequence — need more bytes.
    Incomplete,
}

/// Builds a completed [`ParseOutcome`].
fn done(event: Event, consumed: usize) -> ParseOutcome {
    ParseOutcome::Event { event, consumed }
}

/// Builds an [`Event::Unknown`] outcome from the first `consumed` bytes.
fn unknown(buf: &[u8], consumed: usize) -> ParseOutcome {
    let consumed = consumed.min(buf.len());
    done(Event::Unknown(buf[..consumed].to_vec()), consumed)
}

/// Builds a key press outcome.
fn press(code: KeyCode, mods: Modifiers, consumed: usize) -> ParseOutcome {
    done(
        Event::Key(KeyEvent {
            code,
            mods,
            kind: KeyEventKind::Press,
        }),
        consumed,
    )
}

/// Parses one event from the front of `buf`.
///
/// Never blocks and never panics. An empty `buf` is [`ParseOutcome::Incomplete`].
/// Bytes that cannot form any known sequence come back as [`Event::Unknown`] so
/// that the caller always makes progress.
#[must_use]
pub fn parse_event(buf: &[u8]) -> ParseOutcome {
    let Some(&first) = buf.first() else {
        return ParseOutcome::Incomplete;
    };
    match first {
        ESC => parse_escape(buf),
        b'\r' | b'\n' => press(KeyCode::Enter, Modifiers::NONE, 1),
        b'\t' => press(KeyCode::Tab, Modifiers::NONE, 1),
        0x7f | 0x08 => press(KeyCode::Backspace, Modifiers::NONE, 1),
        // C0 controls are Ctrl+<letter>: 0x00 is Ctrl+Space, 0x01..=0x1a are
        // Ctrl+a..Ctrl+z, 0x1c..=0x1f are Ctrl+\, Ctrl+], Ctrl+^, Ctrl+_.
        0x00 => press(KeyCode::Char(' '), CTRL, 1),
        0x01..=0x1a => press(KeyCode::Char(ctrl_letter(first)), CTRL, 1),
        0x1c..=0x1f => press(KeyCode::Char(ctrl_symbol(first)), CTRL, 1),
        _ => parse_utf8(buf),
    }
}

/// `Modifiers` with only Ctrl set, for decoding C0 control bytes.
const CTRL: Modifiers = Modifiers {
    ctrl: true,
    ..Modifiers::NONE
};

/// Maps `0x01..=0x1a` to `'a'..='z'`.
fn ctrl_letter(byte: u8) -> char {
    char::from(b'a' + (byte - 1))
}

/// Maps `0x1c..=0x1f` to `'\\'`, `']'`, `'^'`, `'_'`.
fn ctrl_symbol(byte: u8) -> char {
    char::from(byte + 0x40)
}

/// Decodes one UTF-8 scalar from the front of `buf` into [`KeyCode::Char`].
///
/// A truncated trailing sequence is [`ParseOutcome::Incomplete`]; a byte that
/// cannot start (or continue) a scalar is one byte of [`Event::Unknown`].
fn parse_utf8(buf: &[u8]) -> ParseOutcome {
    let Some(&lead) = buf.first() else {
        return ParseOutcome::Incomplete;
    };
    let len = match lead {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        // Continuation bytes and over-long/out-of-range leads never start a
        // scalar value.
        _ => return unknown(buf, 1),
    };
    if buf.len() < len {
        // Could still become valid once the rest of the scalar arrives.
        return ParseOutcome::Incomplete;
    }
    match std::str::from_utf8(&buf[..len]) {
        Ok(text) => match text.chars().next() {
            Some(ch) => press(KeyCode::Char(ch), Modifiers::NONE, len),
            None => unknown(buf, 1),
        },
        Err(_) => unknown(buf, 1),
    }
}

/// Dispatches on the byte after `ESC`.
///
/// * `ESC [` — `CSI`, see [`parse_csi`].
/// * `ESC O` — `SS3`, the application-mode cursor and F1-F4 keys.
/// * `ESC P` / `ESC ]` / `ESC _` — `DCS` / `OSC` / `APC` replies.
/// * `ESC ESC` — resolved as a bare Escape; the rest is parsed separately.
/// * anything else — `Alt` plus whatever that byte (or scalar) decodes to.
fn parse_escape(buf: &[u8]) -> ParseOutcome {
    let Some(&second) = buf.get(1) else {
        return ParseOutcome::Incomplete;
    };
    match second {
        b'[' => parse_csi(buf),
        b'O' => parse_ss3(buf),
        b'P' => parse_string_sequence(buf, Reply::Dcs),
        b']' => parse_string_sequence(buf, Reply::Osc),
        b'_' => parse_string_sequence(buf, Reply::Apc),
        // Two escapes in a row: report the first one and let the caller parse
        // the second on the next pass.
        ESC => press(KeyCode::Esc, Modifiers::NONE, 1),
        // `ESC` immediately followed by a key is that key with Alt held. Note
        // that Alt+O, Alt+P, Alt+] and Alt+_ are unreachable: the introducers
        // above win, exactly as in every other terminal program.
        _ => match parse_event(&buf[1..]) {
            ParseOutcome::Incomplete => ParseOutcome::Incomplete,
            ParseOutcome::Event { event, consumed } => match event {
                Event::Key(key) => done(
                    Event::Key(KeyEvent {
                        mods: Modifiers { alt: true, ..key.mods },
                        ..key
                    }),
                    consumed + 1,
                ),
                _ => unknown(buf, consumed + 1),
            },
        },
    }
}

/// Parses `SS3` sequences: `ESC O A/B/C/D` are the application-mode arrows,
/// `ESC O H/F` Home/End, and `ESC O P/Q/R/S` are F1-F4.
fn parse_ss3(buf: &[u8]) -> ParseOutcome {
    let Some(&final_byte) = buf.get(2) else {
        return ParseOutcome::Incomplete;
    };
    let code = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        _ => return unknown(buf, 3),
    };
    press(code, Modifiers::NONE, 3)
}

/// Parses the string-payload sequences `ESC P … ST` (`DCS`), `ESC ] … ST`
/// (`OSC`) and `ESC _ … ST` (`APC`).
///
/// The terminator is `ESC \` (`ST`); a lone `BEL` is also accepted, which is
/// required for `OSC` in practice and harmless defensiveness for the other two.
/// Payload bytes are decoded lossily, since a reply is text by convention but
/// nothing enforces it. Returns [`ParseOutcome::Incomplete`] until the
/// terminator has arrived.
fn parse_string_sequence(buf: &[u8], wrap: fn(String) -> Reply) -> ParseOutcome {
    let body_start = 2;
    let mut i = body_start;
    while i < buf.len() {
        match buf[i] {
            BEL => {
                let payload = String::from_utf8_lossy(&buf[body_start..i]).into_owned();
                return done(Event::Reply(wrap(payload)), i + 1);
            }
            ESC => {
                return match buf.get(i + 1) {
                    None => ParseOutcome::Incomplete,
                    Some(&ST_FINAL) => {
                        let payload = String::from_utf8_lossy(&buf[body_start..i]).into_owned();
                        done(Event::Reply(wrap(payload)), i + 2)
                    }
                    // An embedded escape that is not `ST` means the terminal
                    // abandoned the payload; hand back the bytes so far.
                    Some(_) => unknown(buf, i),
                };
            }
            _ => i += 1,
        }
    }
    ParseOutcome::Incomplete
}

/// A split-up `CSI` sequence: `ESC [ <private> <params> <intermediates> <final>`.
#[derive(Debug)]
struct Csi<'a> {
    /// Private-parameter marker byte (`<`, `=`, `>` or `?`), if present.
    private: Option<u8>,
    /// Parameter bytes (digits, `;` and `:`), without the private marker.
    params: &'a [u8],
    /// The final byte in `0x40..=0x7e`.
    final_byte: u8,
    /// Total length of the sequence, `ESC [` included.
    len: usize,
}

/// Outcome of splitting a `CSI` sequence.
#[derive(Debug)]
enum CsiSplit<'a> {
    /// A complete, well-formed sequence.
    Complete(Csi<'a>),
    /// Still waiting for the final byte.
    Incomplete,
    /// A byte appeared that cannot occur inside `CSI`; this many bytes are junk.
    Invalid(usize),
}

/// Splits `ESC [ …` into private marker, parameters and final byte.
fn split_csi(buf: &[u8]) -> CsiSplit<'_> {
    let mut i = 2;
    let private = match buf.get(i) {
        None => return CsiSplit::Incomplete,
        Some(&byte) if matches!(byte, b'<' | b'=' | b'>' | b'?') => {
            i += 1;
            Some(byte)
        }
        Some(_) => None,
    };
    let params_start = i;
    // Set once an intermediate byte ends the parameter run.
    let mut params_end = None;
    while let Some(&byte) = buf.get(i) {
        match byte {
            // Parameter bytes: digits, `;`, `:` and the private markers.
            0x30..=0x3f => i += 1,
            // Intermediate bytes, e.g. the `$` of `CSI ? 1 $ y`.
            0x20..=0x2f => {
                params_end.get_or_insert(i);
                i += 1;
            }
            // Final byte.
            0x40..=0x7e => {
                return CsiSplit::Complete(Csi {
                    private,
                    params: &buf[params_start..params_end.unwrap_or(i)],
                    final_byte: byte,
                    len: i + 1,
                });
            }
            _ => return CsiSplit::Invalid(i + 1),
        }
    }
    CsiSplit::Incomplete
}

/// Parses `CSI` sequences, i.e. everything starting with `ESC [`.
///
/// Precedence, highest first:
///
/// 1. `CSI M` with no parameters — legacy X10 mouse, see [`parse_x10_mouse`].
/// 2. `CSI < b ; col ; row M|m` — SGR mouse, see [`parse_sgr_mouse`].
/// 3. Any private prefix (`?`, `>`, `=`, and `<` when not SGR mouse) — a device
///    report, so [`Reply::Csi`]. This is what makes `CSI ? <flags> u` the kitty
///    keyboard flags reply rather than a key.
/// 4. Final byte `c`, `n`, `y`, `t` or `S`, or `R` with parameters — device
///    reports (`DA1`, `DSR`, `DECRPM`, window ops, `XTSMGRAPHICS`, cursor
///    position).
/// 5. `CSI 200~ … CSI 201~` — bracketed paste.
/// 6. `CSI I` / `CSI O` — focus in/out.
/// 7. Keys: `u` (kitty), `~` (tilde keys), `A`-`F`/`H`/`P`-`S`/`Z` (cursor and
///    function keys).
fn parse_csi(buf: &[u8]) -> ParseOutcome {
    let csi = match split_csi(buf) {
        CsiSplit::Complete(csi) => csi,
        CsiSplit::Incomplete => return ParseOutcome::Incomplete,
        CsiSplit::Invalid(len) => return unknown(buf, len),
    };

    if csi.private == Some(b'<') && matches!(csi.final_byte, b'M' | b'm') {
        return parse_sgr_mouse(buf, &csi);
    }
    if csi.private.is_none() && csi.params.is_empty() && csi.final_byte == b'M' {
        return parse_x10_mouse(buf, csi.len);
    }
    if csi.private.is_some() {
        return csi_reply(&csi);
    }
    if matches!(csi.final_byte, b'c' | b'n' | b'y' | b't' | b'S')
        || (csi.final_byte == b'R' && !csi.params.is_empty())
    {
        return csi_reply(&csi);
    }

    let fields = Fields::new(csi.params);
    match csi.final_byte {
        b'u' => parse_kitty_key(buf, &csi, &fields),
        b'~' => parse_tilde(buf, &csi, &fields),
        b'I' if csi.params.is_empty() => done(Event::FocusGained, csi.len),
        b'O' if csi.params.is_empty() => done(Event::FocusLost, csi.len),
        b'Z' => key_from_csi(KeyCode::BackTab, &fields, csi.len),
        b'A' => key_from_csi(KeyCode::Up, &fields, csi.len),
        b'B' => key_from_csi(KeyCode::Down, &fields, csi.len),
        b'C' => key_from_csi(KeyCode::Right, &fields, csi.len),
        b'D' => key_from_csi(KeyCode::Left, &fields, csi.len),
        b'H' => key_from_csi(KeyCode::Home, &fields, csi.len),
        b'F' => key_from_csi(KeyCode::End, &fields, csi.len),
        // `CSI 1 P/Q/S` are kitty's F1/F2/F4. F3 would be `CSI 1 R`, which is
        // indistinguishable from a cursor position report, so kitty encodes F3
        // as `CSI 13 ~` instead and rule 4 above keeps `R` a report.
        b'P' => key_from_csi(KeyCode::F(1), &fields, csi.len),
        b'Q' => key_from_csi(KeyCode::F(2), &fields, csi.len),
        b'S' => key_from_csi(KeyCode::F(4), &fields, csi.len),
        _ => unknown(buf, csi.len),
    }
}

/// Wraps a `CSI` sequence up as a [`Reply::Csi`] device report.
fn csi_reply(csi: &Csi<'_>) -> ParseOutcome {
    let mut params = String::new();
    if let Some(private) = csi.private {
        params.push(char::from(private));
    }
    params.push_str(&String::from_utf8_lossy(csi.params));
    done(
        Event::Reply(Reply::Csi {
            params,
            final_byte: char::from(csi.final_byte),
        }),
        csi.len,
    )
}

/// The `;`-separated parameter fields of a `CSI` sequence, each of which may
/// carry `:`-separated sub-parameters.
#[derive(Debug)]
struct Fields<'a> {
    /// The raw parameter text, split lazily.
    raw: &'a [u8],
}

impl<'a> Fields<'a> {
    /// Wraps raw parameter bytes.
    fn new(raw: &'a [u8]) -> Self {
        Self { raw }
    }

    /// Returns field `index`, counting from zero, or `None` when absent.
    fn field(&self, index: usize) -> Option<&'a [u8]> {
        self.raw.split(|&byte| byte == b';').nth(index)
    }

    /// Returns sub-parameter `sub` of field `index`, or `None` when absent.
    fn sub(&self, index: usize, sub: usize) -> Option<&'a [u8]> {
        self.field(index)?.split(|&byte| byte == b':').nth(sub)
    }

    /// Parses sub-parameter `sub` of field `index` as a number.
    fn number(&self, index: usize, sub: usize) -> Option<u32> {
        parse_number(self.sub(index, sub)?)
    }

    /// Decodes the modifier field (field 1 of most sequences), which carries
    /// `mods` and optionally `:event-type`.
    fn mods_and_kind(&self) -> (Modifiers, KeyEventKind) {
        let mods = match self.number(1, 0) {
            Some(value) => Modifiers::from_csi_param(u8::try_from(value).unwrap_or(u8::MAX)),
            None => Modifiers::NONE,
        };
        let kind = match self.number(1, 1) {
            Some(2) => KeyEventKind::Repeat,
            Some(3) => KeyEventKind::Release,
            // Event type 1 is a press, and so is a missing event type.
            _ => KeyEventKind::Press,
        };
        (mods, kind)
    }
}

/// Parses an ASCII decimal number, rejecting anything else.
fn parse_number(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// Builds a key event for the legacy cursor/function key forms, which carry
/// their modifiers (and, under kitty, their event type) in field 1:
/// `CSI 1 ; <mods> A`, `CSI 1 ; <mods> : <event> A`.
fn key_from_csi(code: KeyCode, fields: &Fields<'_>, consumed: usize) -> ParseOutcome {
    let (mods, kind) = fields.mods_and_kind();
    done(Event::Key(KeyEvent { code, mods, kind }), consumed)
}

/// Parses the tilde keys `CSI <n> ~` and their modified form
/// `CSI <n> ; <mods> ~`, plus bracketed paste.
///
/// `1~`/`7~` Home, `2~` Insert, `3~` Delete, `4~`/`8~` End, `5~` Page Up,
/// `6~` Page Down, `11~`-`15~` F1-F5, `17~`-`21~` F6-F10, `23~` F11, `24~` F12.
/// `200~` and `201~` bracket a paste.
fn parse_tilde(buf: &[u8], csi: &Csi<'_>, fields: &Fields<'_>) -> ParseOutcome {
    let Some(number) = fields.number(0, 0) else {
        return unknown(buf, csi.len);
    };
    if number == PASTE_START_NUMBER {
        return parse_paste(buf, csi.len);
    }
    let code = match number {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        11 => KeyCode::F(1),
        12 => KeyCode::F(2),
        13 => KeyCode::F(3),
        14 => KeyCode::F(4),
        15 => KeyCode::F(5),
        17 => KeyCode::F(6),
        18 => KeyCode::F(7),
        19 => KeyCode::F(8),
        20 => KeyCode::F(9),
        21 => KeyCode::F(10),
        23 => KeyCode::F(11),
        24 => KeyCode::F(12),
        _ => return unknown(buf, csi.len),
    };
    key_from_csi(code, fields, csi.len)
}

/// The `<n>` of the bracketed-paste introducer `CSI 200~`.
const PASTE_START_NUMBER: u32 = 200;

/// Collects the body of a bracketed paste: everything between `CSI 200~` and
/// `CSI 201~`.
///
/// Returns [`ParseOutcome::Incomplete`] while the closing marker is missing, so
/// a paste split across several reads is not truncated.
fn parse_paste(buf: &[u8], body_start: usize) -> ParseOutcome {
    let body = &buf[body_start..];
    match find(body, PASTE_END) {
        Some(offset) => {
            let text = String::from_utf8_lossy(&body[..offset]).into_owned();
            done(
                Event::Paste(text),
                body_start + offset + PASTE_END.len(),
            )
        }
        None => ParseOutcome::Incomplete,
    }
}

/// Returns the offset of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parses kitty keyboard protocol key events:
/// `CSI <unicode-key-code> [ : <shifted> : <base-layout> ]
///  [ ; <mods> [ : <event-type> ] ] [ ; <text-codepoints> ] u`.
///
/// Only the primary `unicode-key-code` is used; the alternate keys and the
/// associated-text field are parsed past but not reported. Event types are
/// `1` press, `2` repeat, `3` release.
///
/// `CSI ? <flags> u` never reaches this function — the private `?` marker makes
/// it a [`Reply::Csi`], because it is the answer to `CSI ? u`, not a key.
fn parse_kitty_key(buf: &[u8], csi: &Csi<'_>, fields: &Fields<'_>) -> ParseOutcome {
    let Some(number) = fields.number(0, 0) else {
        return unknown(buf, csi.len);
    };
    let Some(code) = kitty_key_code(number) else {
        return unknown(buf, csi.len);
    };
    let (mods, kind) = fields.mods_and_kind();
    done(Event::Key(KeyEvent { code, mods, kind }), csi.len)
}

/// Start of the kitty functional-key private use area.
const KITTY_PUA_START: u32 = 57_344;
/// Last functional key kitty defines (`ISO_LEVEL5_SHIFT`).
const KITTY_PUA_END: u32 = 57_454;

/// Maps a kitty `unicode-key-code` to a [`KeyCode`].
///
/// The C0-based codes come first: `27` Escape, `13` Enter, `9` Tab,
/// `127` Backspace. Note that the kitty specification does *not* place
/// Escape, Enter, Tab, Backspace, Insert, Delete, the arrows, Home, End,
/// Page Up/Down or F1-F12 in the private use area — those keep their legacy
/// encodings (`2~`, `3~`, `1A`, `15~`, …), which [`parse_tilde`] and
/// [`parse_csi`] handle. The private use area holds the keys legacy encodings
/// have no room for:
///
/// * `57358`-`57363` Caps Lock, Scroll Lock, Num Lock, Print Screen, Pause,
///   Menu — no [`KeyCode`] variant, so reported as [`Event::Unknown`].
/// * `57376`-`57398` F13-F35.
/// * `57399`-`57415` keypad `0`-`9`, `.`, `/`, `*`, `-`, `+`, Enter, `=`.
/// * `57417`-`57426` keypad Left/Right/Up/Down/Page Up/Page Down/Home/End/
///   Insert/Delete.
/// * `57416` keypad separator, `57427` keypad Begin, `57428`-`57440` media and
///   volume keys, `57441`-`57454` the individual modifier keys — no
///   [`KeyCode`] variant, so [`Event::Unknown`].
fn kitty_key_code(number: u32) -> Option<KeyCode> {
    Some(match number {
        9 => KeyCode::Tab,
        // 13 is Enter, 57414 the keypad's own Enter key.
        13 | 57_414 => KeyCode::Enter,
        27 => KeyCode::Esc,
        127 => KeyCode::Backspace,
        57_376..=57_398 => KeyCode::F(u8::try_from(13 + (number - 57_376)).ok()?),
        57_399..=57_408 => KeyCode::Char(char::from_digit(number - 57_399, 10)?),
        57_409 => KeyCode::Char('.'),
        57_410 => KeyCode::Char('/'),
        57_411 => KeyCode::Char('*'),
        57_412 => KeyCode::Char('-'),
        57_413 => KeyCode::Char('+'),
        57_415 => KeyCode::Char('='),
        57_417 => KeyCode::Left,
        57_418 => KeyCode::Right,
        57_419 => KeyCode::Up,
        57_420 => KeyCode::Down,
        57_421 => KeyCode::PageUp,
        57_422 => KeyCode::PageDown,
        57_423 => KeyCode::Home,
        57_424 => KeyCode::End,
        57_425 => KeyCode::Insert,
        57_426 => KeyCode::Delete,
        // Remaining private use area codes have no representation here.
        KITTY_PUA_START..=KITTY_PUA_END => return None,
        // Everything else is a character-producing key, reported as the
        // character its unshifted keycap produces.
        0x20..=0x10_ffff => KeyCode::Char(char::from_u32(number)?),
        _ => return None,
    })
}

/// Parses SGR mouse reports: `CSI < b ; col ; row M|m`.
///
/// `b` is a bitmask: the low two bits select the button (`0` left, `1` middle,
/// `2` right, `3` none), `+4` shift, `+8` alt/meta, `+16` ctrl, `+32` motion,
/// `+64` wheel (`64` up, `65` down, `66` left, `67` right). A final `M` is a
/// press (or a motion report), a final `m` a release — the great advantage over
/// X10, which cannot say which button came up.
fn parse_sgr_mouse(buf: &[u8], csi: &Csi<'_>) -> ParseOutcome {
    let fields = Fields::new(csi.params);
    let (Some(button_mask), Some(col), Some(row)) =
        (fields.number(0, 0), fields.number(1, 0), fields.number(2, 0))
    else {
        return unknown(buf, csi.len);
    };
    let (Ok(col), Ok(row)) = (u16::try_from(col), u16::try_from(row)) else {
        return unknown(buf, csi.len);
    };
    let mask = u8::try_from(button_mask).unwrap_or(u8::MAX);
    let released = csi.final_byte == b'm';
    let kind = mouse_kind(mask, released);
    done(
        Event::Mouse(MouseEvent {
            kind,
            col,
            row,
            mods: mouse_mods(mask),
        }),
        csi.len,
    )
}

/// Parses legacy X10 mouse reports: `CSI M <b+32> <col+32> <row+32>`.
///
/// The three values are single raw bytes and routinely leave ASCII (column 200
/// is byte `0xe8`), so they are handled as bytes and never as characters. X10
/// encodes a release as button code `3`, which does not say *which* button was
/// let go; this reports [`MouseButton::Left`] in that case.
fn parse_x10_mouse(buf: &[u8], consumed: usize) -> ParseOutcome {
    let Some(args) = buf.get(consumed..consumed + X10_ARGS) else {
        return ParseOutcome::Incomplete;
    };
    let total = consumed + X10_ARGS;
    let (Some(mask), Some(col), Some(row)) = (
        args[0].checked_sub(X10_BIAS),
        args[1].checked_sub(X10_BIAS),
        args[2].checked_sub(X10_BIAS),
    ) else {
        return unknown(buf, total);
    };
    // Button code 3 with no motion bit is "some button was released".
    let released = mask & 0b11 == 0b11 && mask & MOUSE_MOTION == 0 && mask & MOUSE_WHEEL == 0;
    let kind = if released {
        MouseKind::Up(MouseButton::Left)
    } else {
        mouse_kind(mask, false)
    };
    done(
        Event::Mouse(MouseEvent {
            kind,
            col: u16::from(col),
            row: u16::from(row),
            mods: mouse_mods(mask),
        }),
        total,
    )
}

/// Shift bit of a mouse button mask.
const MOUSE_SHIFT: u8 = 0b0000_0100;
/// Alt (meta) bit of a mouse button mask.
const MOUSE_ALT: u8 = 0b0000_1000;
/// Ctrl bit of a mouse button mask.
const MOUSE_CTRL: u8 = 0b0001_0000;
/// Motion bit of a mouse button mask.
const MOUSE_MOTION: u8 = 0b0010_0000;
/// Wheel bit of a mouse button mask.
const MOUSE_WHEEL: u8 = 0b0100_0000;

/// Decodes the modifier bits shared by X10 and SGR mouse masks.
fn mouse_mods(mask: u8) -> Modifiers {
    Modifiers {
        shift: mask & MOUSE_SHIFT != 0,
        alt: mask & MOUSE_ALT != 0,
        ctrl: mask & MOUSE_CTRL != 0,
        super_: false,
    }
}

/// Decodes the button/motion/wheel bits of a mouse mask.
///
/// `released` distinguishes the SGR `m` final byte from `M`. Motion with a
/// button held is a drag, motion without one is plain movement.
fn mouse_kind(mask: u8, released: bool) -> MouseKind {
    if mask & MOUSE_WHEEL != 0 {
        return match mask & 0b11 {
            0 => MouseKind::ScrollUp,
            1 => MouseKind::ScrollDown,
            2 => MouseKind::ScrollLeft,
            _ => MouseKind::ScrollRight,
        };
    }
    let button = match mask & 0b11 {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        // Button code 3 means "no button" in the SGR encoding.
        _ => None,
    };
    match (button, mask & MOUSE_MOTION != 0) {
        (Some(button), true) => MouseKind::Drag(button),
        (Some(button), false) if released => MouseKind::Up(button),
        (Some(button), false) => MouseKind::Down(button),
        (None, _) => MouseKind::Moved,
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Reads events from `stdin`.
///
/// A background thread does the blocking reads and ships raw bytes over a
/// channel, which is what makes timeouts possible with nothing but `std`: the
/// foreground code waits on the channel, never on the file descriptor. The
/// reader keeps a byte buffer for partially received sequences and an event
/// queue for input that arrived while [`InputReader::wait_for_reply`] was
/// looking for a terminal reply.
///
/// The terminal must already be in raw mode; this type does not change terminal
/// state.
#[derive(Debug)]
pub struct InputReader {
    /// Raw byte chunks (or a read error) from the stdin thread.
    rx: Receiver<io::Result<Vec<u8>>>,
    /// Bytes received but not yet parsed.
    buf: Vec<u8>,
    /// Events that arrived while waiting for something more specific.
    queued: VecDeque<Event>,
    /// When the current unfinished sequence was first seen, for [`ESC_TIMEOUT`].
    stalled_since: Option<Instant>,
    /// Set once `stdin` reached end of file or the reader thread stopped.
    eof: bool,
}

impl InputReader {
    /// Spawns the background `stdin` reader thread.
    ///
    /// # Errors
    ///
    /// Fails if the operating system refuses to create the thread.
    pub fn new() -> io::Result<Self> {
        Self::from_reader(io::stdin(), "escapade-stdin")
    }

    /// Spawns a reader thread over any byte source.
    ///
    /// Reading replies from `stdin` is wrong whenever `stdin` is not the terminal
    /// — a program in a pipeline still has a terminal, reachable as `/dev/tty`,
    /// and that is where its queries are answered. `name` labels the thread so a
    /// stuck read is identifiable in a debugger.
    ///
    /// # Errors
    ///
    /// Fails if the operating system refuses to create the thread.
    pub fn from_reader<R>(mut source: R, name: &str) -> io::Result<Self>
    where
        R: Read + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        // The thread is deliberately detached: it is parked in a blocking read
        // and cannot be interrupted without `unsafe`. It exits on end of file,
        // on a read error, or as soon as the receiver is gone.
        thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let mut chunk = [0u8; READ_CHUNK];
                loop {
                    match source.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(count) => {
                            if tx.send(Ok(chunk[..count].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            drop(tx.send(Err(error)));
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            rx,
            buf: Vec::new(),
            queued: VecDeque::new(),
            stalled_since: None,
            eof: false,
        })
    }

    /// Reads the next event, waiting at most `timeout`. `Ok(None)` on timeout.
    ///
    /// # Errors
    ///
    /// Returns the reader thread's I/O error, or [`io::ErrorKind::UnexpectedEof`]
    /// once `stdin` is closed and every buffered byte has been consumed.
    pub fn read(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(Some(event));
        }
        self.read_parsed(Some(Instant::now() + timeout))
    }

    /// Blocks until an event arrives (no timeout).
    ///
    /// # Errors
    ///
    /// Returns the reader thread's I/O error, or [`io::ErrorKind::UnexpectedEof`]
    /// once `stdin` is closed and every buffered byte has been consumed.
    pub fn read_blocking(&mut self) -> io::Result<Event> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(event);
        }
        loop {
            if let Some(event) = self.read_parsed(None)? {
                return Ok(event);
            }
        }
    }

    /// Blocks until the user presses an advance/quit key, ignoring everything
    /// else.
    ///
    /// Space and Enter mean [`Advance::Next`]; `q`, `Q`, Escape, `Ctrl+c` and
    /// `Ctrl+d` mean [`Advance::Quit`]; Left arrow, Backspace and `p` mean
    /// [`Advance::Prev`]. Mouse reports, terminal replies and key releases are
    /// skipped.
    ///
    /// # Errors
    ///
    /// Returns the reader thread's I/O error, or [`io::ErrorKind::UnexpectedEof`]
    /// if `stdin` closes before a matching key arrives.
    pub fn wait_for_advance(&mut self) -> io::Result<Advance> {
        loop {
            let event = self.read_blocking()?;
            let Event::Key(key) = event else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let advance = match key.code {
                KeyCode::Char(' ') | KeyCode::Enter => Advance::Next,
                KeyCode::Char('q' | 'Q') | KeyCode::Esc => Advance::Quit,
                KeyCode::Char('c' | 'd') if key.mods.ctrl => Advance::Quit,
                KeyCode::Char('p') | KeyCode::Left | KeyCode::Backspace => Advance::Prev,
                _ => continue,
            };
            return Ok(advance);
        }
    }

    /// Discards all currently-pending input without blocking.
    ///
    /// Bytes still in flight inside the operating system are not affected; this
    /// only clears what has already reached the reader.
    pub fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                    self.eof = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        self.buf.clear();
        self.queued.clear();
        self.stalled_since = None;
    }

    /// Waits up to `timeout` for the next terminal reply matching `pred`,
    /// buffering any user input seen meanwhile so it is not lost.
    ///
    /// Already-queued events are searched first, so two probes in a row cannot
    /// steal each other's answer. Buffered events come back from later
    /// [`InputReader::read`] calls in arrival order.
    ///
    /// # Errors
    ///
    /// Returns the reader thread's I/O error, or [`io::ErrorKind::UnexpectedEof`]
    /// if `stdin` closes while waiting.
    pub fn wait_for_reply(
        &mut self,
        timeout: Duration,
        pred: impl Fn(&Reply) -> bool,
    ) -> io::Result<Option<Reply>> {
        let queued_match = self
            .queued
            .iter()
            .position(|event| matches!(event, Event::Reply(reply) if pred(reply)));
        if let Some(index) = queued_match
            && let Some(Event::Reply(reply)) = self.queued.remove(index)
        {
            return Ok(Some(reply));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.read_parsed(Some(deadline))? {
                None => return Ok(None),
                Some(Event::Reply(reply)) if pred(&reply) => return Ok(Some(reply)),
                Some(other) => self.queued.push_back(other),
            }
        }
    }

    /// Parses one event, pulling in bytes until `deadline` (or forever when it
    /// is `None`).
    ///
    /// Ignores [`InputReader::queued`]; callers that want queued events must
    /// pop them first.
    fn read_parsed(&mut self, deadline: Option<Instant>) -> io::Result<Option<Event>> {
        loop {
            if let ParseOutcome::Event { event, consumed } = parse_event(&self.buf) {
                self.buf.drain(..consumed);
                self.stalled_since = None;
                return Ok(Some(event));
            }
            if self.eof {
                // No more bytes will ever arrive, so an unfinished sequence can
                // be resolved immediately.
                if self.buf.is_empty() {
                    return Err(eof_error());
                }
                return Ok(Some(self.resolve_stalled()));
            }

            let esc_deadline = self.escape_deadline();
            let wait_until = match (deadline, esc_deadline) {
                (None, other) | (other, None) => other,
                (Some(a), Some(b)) => Some(a.min(b)),
            };
            let received = match wait_until {
                None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(instant) => self
                    .rx
                    .recv_timeout(instant.saturating_duration_since(Instant::now())),
            };
            match received {
                Ok(Ok(bytes)) => {
                    self.buf.extend_from_slice(&bytes);
                    self.stalled_since = None;
                }
                Ok(Err(error)) => return Err(error),
                Err(RecvTimeoutError::Disconnected) => self.eof = true,
                Err(RecvTimeoutError::Timeout) => {
                    if esc_deadline.is_some_and(|instant| Instant::now() >= instant) {
                        return Ok(Some(self.resolve_stalled()));
                    }
                    // The caller's own timeout expired; partially received
                    // bytes stay buffered for the next call.
                    return Ok(None);
                }
            }
        }
    }

    /// Returns the instant at which the currently buffered partial sequence
    /// should be given up on, starting the [`ESC_TIMEOUT`] window if needed.
    fn escape_deadline(&mut self) -> Option<Instant> {
        if self.buf.is_empty() || self.buf.starts_with(PASTE_START) {
            // Nothing pending, or a paste in progress: a paste body can legally
            // stall for a long time and must not be cut short.
            return None;
        }
        let since = *self.stalled_since.get_or_insert_with(Instant::now);
        Some(since + ESC_TIMEOUT)
    }

    /// Turns an unfinished sequence into an event once input has stalled: a
    /// lone `ESC` is the Escape key, anything else is handed back verbatim.
    fn resolve_stalled(&mut self) -> Event {
        self.stalled_since = None;
        if self.buf == [ESC] {
            self.buf.clear();
            return Event::Key(KeyEvent {
                code: KeyCode::Esc,
                mods: Modifiers::NONE,
                kind: KeyEventKind::Press,
            });
        }
        Event::Unknown(std::mem::take(&mut self.buf))
    }
}

/// The error reported once `stdin` is exhausted.
fn eof_error() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "stdin closed")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;

    /// Builds a press event.
    fn key(code: KeyCode, mods: Modifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            mods,
            kind: KeyEventKind::Press,
        })
    }

    /// Builds an event with an explicit kind.
    fn key_kind(code: KeyCode, mods: Modifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent { code, mods, kind })
    }

    /// Builds a mouse event.
    fn mouse(kind: MouseKind, col: u16, row: u16, mods: Modifiers) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            col,
            row,
            mods,
        })
    }

    const NONE: Modifiers = Modifiers::NONE;
    const SHIFT: Modifiers = Modifiers {
        shift: true,
        ..NONE
    };
    const ALT: Modifiers = Modifiers { alt: true, ..NONE };
    const CTRL_ONLY: Modifiers = Modifiers { ctrl: true, ..NONE };
    const CTRL_SHIFT: Modifiers = Modifiers {
        shift: true,
        ctrl: true,
        ..NONE
    };

    /// Asserts that `input` parses to `want` and consumes all of `input`.
    #[track_caller]
    fn check(input: &[u8], want: &Event) {
        match parse_event(input) {
            ParseOutcome::Event { event, consumed } => {
                assert_eq!(&event, want, "parsing {input:?}");
                assert_eq!(consumed, input.len(), "consumed bytes for {input:?}");
            }
            ParseOutcome::Incomplete => panic!("unexpected Incomplete for {input:?}"),
        }
    }

    /// Asserts that every strict prefix of `input` is incomplete and the whole
    /// of `input` parses to `want`.
    #[track_caller]
    fn check_incremental(input: &[u8], want: &Event) {
        for end in 1..input.len() {
            assert_eq!(
                parse_event(&input[..end]),
                ParseOutcome::Incomplete,
                "prefix {:?} of {input:?} should be incomplete",
                &input[..end]
            );
        }
        check(input, want);
    }

    #[test]
    fn plain_bytes() {
        let cases: &[(&[u8], Event)] = &[
            (b"\r", key(KeyCode::Enter, NONE)),
            (b"\n", key(KeyCode::Enter, NONE)),
            (b"\t", key(KeyCode::Tab, NONE)),
            (b"\x7f", key(KeyCode::Backspace, NONE)),
            (b"\x08", key(KeyCode::Backspace, NONE)),
            (b"a", key(KeyCode::Char('a'), NONE)),
            (b" ", key(KeyCode::Char(' '), NONE)),
            (b"~", key(KeyCode::Char('~'), NONE)),
        ];
        for (input, want) in cases {
            check(input, want);
        }
    }

    #[test]
    fn ctrl_combinations() {
        let cases: &[(&[u8], Event)] = &[
            (b"\x01", key(KeyCode::Char('a'), CTRL_ONLY)),
            (b"\x03", key(KeyCode::Char('c'), CTRL_ONLY)),
            (b"\x04", key(KeyCode::Char('d'), CTRL_ONLY)),
            (b"\x1a", key(KeyCode::Char('z'), CTRL_ONLY)),
            (b"\x00", key(KeyCode::Char(' '), CTRL_ONLY)),
            (b"\x1c", key(KeyCode::Char('\\'), CTRL_ONLY)),
            (b"\x1f", key(KeyCode::Char('_'), CTRL_ONLY)),
        ];
        for (input, want) in cases {
            check(input, want);
        }
    }

    #[test]
    fn utf8_characters() {
        let cases: &[(&[u8], Event)] = &[
            ("é".as_bytes(), key(KeyCode::Char('é'), NONE)),
            ("→".as_bytes(), key(KeyCode::Char('→'), NONE)),
            ("🦀".as_bytes(), key(KeyCode::Char('🦀'), NONE)),
            ("□".as_bytes(), key(KeyCode::Char('□'), NONE)),
        ];
        for (input, want) in cases {
            check_incremental(input, want);
        }
    }

    #[test]
    fn invalid_utf8_is_unknown() {
        check(&[0xff], &Event::Unknown(vec![0xff]));
        // A continuation byte cannot start a scalar value.
        check(&[0x80], &Event::Unknown(vec![0x80]));
        // Valid lead byte, invalid continuation: one byte is rejected and the
        // next parse looks at the rest.
        assert_eq!(
            parse_event(&[0xc3, 0x28]),
            ParseOutcome::Event {
                event: Event::Unknown(vec![0xc3]),
                consumed: 1
            }
        );
    }

    #[test]
    fn alt_prefixed_keys() {
        let cases: &[(&[u8], Event)] = &[
            (b"\x1ba", key(KeyCode::Char('a'), ALT)),
            (b"\x1b\r", key(KeyCode::Enter, ALT)),
            (b"\x1b\x7f", key(KeyCode::Backspace, ALT)),
            (
                b"\x1b\x03",
                key(
                    KeyCode::Char('c'),
                    Modifiers {
                        ctrl: true,
                        alt: true,
                        ..NONE
                    },
                ),
            ),
        ];
        for (input, want) in cases {
            check(input, want);
        }
        // `ESC ESC` yields one Escape and leaves the second byte alone.
        assert_eq!(
            parse_event(b"\x1b\x1b"),
            ParseOutcome::Event {
                event: key(KeyCode::Esc, NONE),
                consumed: 1
            }
        );
    }

    #[test]
    fn legacy_cursor_and_function_keys() {
        let cases: &[(&[u8], Event)] = &[
            (b"\x1b[A", key(KeyCode::Up, NONE)),
            (b"\x1b[B", key(KeyCode::Down, NONE)),
            (b"\x1b[C", key(KeyCode::Right, NONE)),
            (b"\x1b[D", key(KeyCode::Left, NONE)),
            (b"\x1b[H", key(KeyCode::Home, NONE)),
            (b"\x1b[F", key(KeyCode::End, NONE)),
            (b"\x1b[Z", key(KeyCode::BackTab, NONE)),
            (b"\x1bOA", key(KeyCode::Up, NONE)),
            (b"\x1bOB", key(KeyCode::Down, NONE)),
            (b"\x1bOC", key(KeyCode::Right, NONE)),
            (b"\x1bOD", key(KeyCode::Left, NONE)),
            (b"\x1bOP", key(KeyCode::F(1), NONE)),
            (b"\x1bOQ", key(KeyCode::F(2), NONE)),
            (b"\x1bOR", key(KeyCode::F(3), NONE)),
            (b"\x1bOS", key(KeyCode::F(4), NONE)),
            (b"\x1b[1~", key(KeyCode::Home, NONE)),
            (b"\x1b[2~", key(KeyCode::Insert, NONE)),
            (b"\x1b[3~", key(KeyCode::Delete, NONE)),
            (b"\x1b[4~", key(KeyCode::End, NONE)),
            (b"\x1b[5~", key(KeyCode::PageUp, NONE)),
            (b"\x1b[6~", key(KeyCode::PageDown, NONE)),
            (b"\x1b[7~", key(KeyCode::Home, NONE)),
            (b"\x1b[8~", key(KeyCode::End, NONE)),
            (b"\x1b[11~", key(KeyCode::F(1), NONE)),
            (b"\x1b[12~", key(KeyCode::F(2), NONE)),
            (b"\x1b[13~", key(KeyCode::F(3), NONE)),
            (b"\x1b[14~", key(KeyCode::F(4), NONE)),
            (b"\x1b[15~", key(KeyCode::F(5), NONE)),
            (b"\x1b[17~", key(KeyCode::F(6), NONE)),
            (b"\x1b[18~", key(KeyCode::F(7), NONE)),
            (b"\x1b[19~", key(KeyCode::F(8), NONE)),
            (b"\x1b[20~", key(KeyCode::F(9), NONE)),
            (b"\x1b[21~", key(KeyCode::F(10), NONE)),
            (b"\x1b[23~", key(KeyCode::F(11), NONE)),
            (b"\x1b[24~", key(KeyCode::F(12), NONE)),
            (b"\x1b[1P", key(KeyCode::F(1), NONE)),
        ];
        for (input, want) in cases {
            check(input, want);
        }
    }

    #[test]
    fn modified_cursor_and_tilde_keys() {
        let super_ctrl = Modifiers {
            ctrl: true,
            super_: true,
            ..NONE
        };
        let cases: &[(&[u8], Event)] = &[
            (b"\x1b[1;2A", key(KeyCode::Up, SHIFT)),
            (b"\x1b[1;3B", key(KeyCode::Down, ALT)),
            (b"\x1b[1;5C", key(KeyCode::Right, CTRL_ONLY)),
            (b"\x1b[1;6D", key(KeyCode::Left, CTRL_SHIFT)),
            (b"\x1b[1;13F", key(KeyCode::End, super_ctrl)),
            (b"\x1b[1;5H", key(KeyCode::Home, CTRL_ONLY)),
            (b"\x1b[3;5~", key(KeyCode::Delete, CTRL_ONLY)),
            (b"\x1b[5;2~", key(KeyCode::PageUp, SHIFT)),
            (b"\x1b[24;6~", key(KeyCode::F(12), CTRL_SHIFT)),
            // kitty adds the event type to legacy forms too.
            (
                b"\x1b[1;5:3C",
                key_kind(KeyCode::Right, CTRL_ONLY, KeyEventKind::Release),
            ),
            (
                b"\x1b[3;1:2~",
                key_kind(KeyCode::Delete, NONE, KeyEventKind::Repeat),
            ),
        ];
        for (input, want) in cases {
            check(input, want);
        }
    }

    #[test]
    fn kitty_key_events() {
        let cases: &[(&[u8], Event)] = &[
            (b"\x1b[27u", key(KeyCode::Esc, NONE)),
            (b"\x1b[13u", key(KeyCode::Enter, NONE)),
            (b"\x1b[9u", key(KeyCode::Tab, NONE)),
            (b"\x1b[127u", key(KeyCode::Backspace, NONE)),
            (b"\x1b[97u", key(KeyCode::Char('a'), NONE)),
            (b"\x1b[97;5u", key(KeyCode::Char('a'), CTRL_ONLY)),
            (b"\x1b[97;6u", key(KeyCode::Char('a'), CTRL_SHIFT)),
            // Alternate keys: `a` with shift, shifted `A`, base layout `a`.
            (b"\x1b[97:65:97;2u", key(KeyCode::Char('a'), SHIFT)),
            // Press, repeat, release.
            (
                b"\x1b[97;1:1u",
                key_kind(KeyCode::Char('a'), NONE, KeyEventKind::Press),
            ),
            (
                b"\x1b[97;1:2u",
                key_kind(KeyCode::Char('a'), NONE, KeyEventKind::Repeat),
            ),
            (
                b"\x1b[97;1:3u",
                key_kind(KeyCode::Char('a'), NONE, KeyEventKind::Release),
            ),
            (
                b"\x1b[27;5:3u",
                key_kind(KeyCode::Esc, CTRL_ONLY, KeyEventKind::Release),
            ),
            // Associated text field is parsed past and ignored.
            (b"\x1b[97;1;97u", key(KeyCode::Char('a'), NONE)),
            // Private use area functional keys.
            (b"\x1b[57376u", key(KeyCode::F(13), NONE)),
            (b"\x1b[57398u", key(KeyCode::F(35), NONE)),
            (b"\x1b[57399u", key(KeyCode::Char('0'), NONE)),
            (b"\x1b[57408u", key(KeyCode::Char('9'), NONE)),
            (b"\x1b[57413u", key(KeyCode::Char('+'), NONE)),
            (b"\x1b[57414u", key(KeyCode::Enter, NONE)),
            (b"\x1b[57417u", key(KeyCode::Left, NONE)),
            (b"\x1b[57426u", key(KeyCode::Delete, NONE)),
            // Keys with no `KeyCode` counterpart stay raw: Caps Lock and the
            // individual modifier keys.
            (b"\x1b[57358u", Event::Unknown(b"\x1b[57358u".to_vec())),
            (b"\x1b[57441u", Event::Unknown(b"\x1b[57441u".to_vec())),
        ];
        for (input, want) in cases {
            check(input, want);
        }
    }

    #[test]
    fn kitty_flags_reply_is_not_a_key() {
        check(
            b"\x1b[?1u",
            &Event::Reply(Reply::Csi {
                params: "?1".to_owned(),
                final_byte: 'u',
            }),
        );
        check(
            b"\x1b[?13u",
            &Event::Reply(Reply::Csi {
                params: "?13".to_owned(),
                final_byte: 'u',
            }),
        );
    }

    #[test]
    fn sgr_mouse_reports() {
        let cases: &[(&[u8], Event)] = &[
            (
                b"\x1b[<0;10;20M",
                mouse(MouseKind::Down(MouseButton::Left), 10, 20, NONE),
            ),
            (
                b"\x1b[<0;10;20m",
                mouse(MouseKind::Up(MouseButton::Left), 10, 20, NONE),
            ),
            (
                b"\x1b[<1;1;1M",
                mouse(MouseKind::Down(MouseButton::Middle), 1, 1, NONE),
            ),
            (
                b"\x1b[<2;5;6M",
                mouse(MouseKind::Down(MouseButton::Right), 5, 6, NONE),
            ),
            (
                b"\x1b[<32;11;21M",
                mouse(MouseKind::Drag(MouseButton::Left), 11, 21, NONE),
            ),
            (
                b"\x1b[<34;11;21M",
                mouse(MouseKind::Drag(MouseButton::Right), 11, 21, NONE),
            ),
            (b"\x1b[<35;12;22M", mouse(MouseKind::Moved, 12, 22, NONE)),
            (b"\x1b[<64;3;4M", mouse(MouseKind::ScrollUp, 3, 4, NONE)),
            (b"\x1b[<65;3;4M", mouse(MouseKind::ScrollDown, 3, 4, NONE)),
            (b"\x1b[<66;3;4M", mouse(MouseKind::ScrollLeft, 3, 4, NONE)),
            (b"\x1b[<67;3;4M", mouse(MouseKind::ScrollRight, 3, 4, NONE)),
            // Modifier bits: shift 4, alt 8, ctrl 16.
            (
                b"\x1b[<4;1;1M",
                mouse(MouseKind::Down(MouseButton::Left), 1, 1, SHIFT),
            ),
            (
                b"\x1b[<20;1;1M",
                mouse(MouseKind::Down(MouseButton::Left), 1, 1, CTRL_SHIFT),
            ),
            (
                b"\x1b[<8;1;1M",
                mouse(MouseKind::Down(MouseButton::Left), 1, 1, ALT),
            ),
            // Coordinates well beyond one byte.
            (
                b"\x1b[<0;400;300M",
                mouse(MouseKind::Down(MouseButton::Left), 400, 300, NONE),
            ),
        ];
        for (input, want) in cases {
            check(input, want);
        }
    }

    #[test]
    fn x10_mouse_reports() {
        // `CSI M <b+32> <col+32> <row+32>`: left press at column 1, row 1.
        check(
            b"\x1b[M\x20\x21\x21",
            &mouse(MouseKind::Down(MouseButton::Left), 1, 1, NONE),
        );
        // Button code 3 is a release of an unidentified button.
        check(
            b"\x1b[M\x23\x21\x21",
            &mouse(MouseKind::Up(MouseButton::Left), 1, 1, NONE),
        );
        // Wheel up.
        check(b"\x1b[M\x60\x21\x21", &mouse(MouseKind::ScrollUp, 1, 1, NONE));
        // Coordinate bytes above ASCII: column 200 is 200 + 32 = 232 (0xe8).
        check(
            &[ESC, b'[', b'M', 0x20, 0xe8, 0x21],
            &mouse(MouseKind::Down(MouseButton::Left), 200, 1, NONE),
        );
        // Drag with ctrl held: 32 (motion) + 16 (ctrl) + 32 (bias) = 80.
        check(
            b"\x1b[M\x50\x22\x23",
            &mouse(MouseKind::Drag(MouseButton::Left), 2, 3, CTRL_ONLY),
        );
    }

    #[test]
    fn mouse_wins_over_report_for_csi_less_than() {
        // Final `M`/`m` after `<` is a mouse report ...
        check(
            b"\x1b[<0;1;2M",
            &mouse(MouseKind::Down(MouseButton::Left), 1, 2, NONE),
        );
        // ... while any other final byte after a private prefix is a device
        // report, not a mouse event.
        check(
            b"\x1b[<0;1;2c",
            &Event::Reply(Reply::Csi {
                params: "<0;1;2".to_owned(),
                final_byte: 'c',
            }),
        );
    }

    #[test]
    fn focus_events() {
        check(b"\x1b[I", &Event::FocusGained);
        check(b"\x1b[O", &Event::FocusLost);
    }

    #[test]
    fn bracketed_paste() {
        check(
            b"\x1b[200~hello\x1b[201~",
            &Event::Paste("hello".to_owned()),
        );
        // Newlines and escapes inside the paste body are just text.
        check(
            b"\x1b[200~a\nb\x1b[201~",
            &Event::Paste("a\nb".to_owned()),
        );
        check(b"\x1b[200~\x1b[201~", &Event::Paste(String::new()));
        // Without the closing marker there is nothing to hand out yet.
        assert_eq!(
            parse_event(b"\x1b[200~partial"),
            ParseOutcome::Incomplete
        );
        // Trailing bytes after the paste are left for the next parse.
        assert_eq!(
            parse_event(b"\x1b[200~hi\x1b[201~x"),
            ParseOutcome::Event {
                event: Event::Paste("hi".to_owned()),
                consumed: 14
            }
        );
    }

    #[test]
    fn string_replies() {
        // The Glyph Protocol answer, the reason `APC` matters here.
        check(
            b"\x1b_25a1;s;fmt=glyf\x1b\\",
            &Event::Reply(Reply::Apc("25a1;s;fmt=glyf".to_owned())),
        );
        // `BEL` is accepted as a terminator as well.
        check(
            b"\x1b_25a1;s\x07",
            &Event::Reply(Reply::Apc("25a1;s".to_owned())),
        );
        check(
            b"\x1bP>|kitty(0.42.0)\x1b\\",
            &Event::Reply(Reply::Dcs(">|kitty(0.42.0)".to_owned())),
        );
        check(
            b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\",
            &Event::Reply(Reply::Osc("11;rgb:1e1e/1e1e/2e2e".to_owned())),
        );
        check(
            b"\x1b]11;rgb:0/0/0\x07",
            &Event::Reply(Reply::Osc("11;rgb:0/0/0".to_owned())),
        );
        // Unterminated payloads wait for more bytes.
        assert_eq!(parse_event(b"\x1b_25a1"), ParseOutcome::Incomplete);
        assert_eq!(parse_event(b"\x1b_25a1\x1b"), ParseOutcome::Incomplete);
        assert_eq!(parse_event(b"\x1bPxyz"), ParseOutcome::Incomplete);
        assert_eq!(parse_event(b"\x1b]11;"), ParseOutcome::Incomplete);
    }

    #[test]
    fn csi_device_reports() {
        let cases: &[(&[u8], Event)] = &[
            // Primary Device Attributes: `CSI ? 62 ; 4 c`.
            (
                b"\x1b[?62;4c",
                Event::Reply(Reply::Csi {
                    params: "?62;4".to_owned(),
                    final_byte: 'c',
                }),
            ),
            // Device Status Report / cursor position.
            (
                b"\x1b[12;40R",
                Event::Reply(Reply::Csi {
                    params: "12;40".to_owned(),
                    final_byte: 'R',
                }),
            ),
            (
                b"\x1b[0n",
                Event::Reply(Reply::Csi {
                    params: "0".to_owned(),
                    final_byte: 'n',
                }),
            ),
            // Secondary Device Attributes.
            (
                b"\x1b[>0;276;0c",
                Event::Reply(Reply::Csi {
                    params: ">0;276;0".to_owned(),
                    final_byte: 'c',
                }),
            ),
            // `DECRPM`, window ops and `XTSMGRAPHICS`.
            (
                b"\x1b[?2026;2$y",
                Event::Reply(Reply::Csi {
                    params: "?2026;2".to_owned(),
                    final_byte: 'y',
                }),
            ),
            (
                b"\x1b[4;1080;1920t",
                Event::Reply(Reply::Csi {
                    params: "4;1080;1920".to_owned(),
                    final_byte: 't',
                }),
            ),
            (
                b"\x1b[?2;0;4096S",
                Event::Reply(Reply::Csi {
                    params: "?2;0;4096".to_owned(),
                    final_byte: 'S',
                }),
            ),
        ];
        for (input, want) in cases {
            check(input, want);
        }
    }

    #[test]
    fn incomplete_until_last_byte() {
        let cases: &[(&[u8], Event)] = &[
            (b"\x1b[A", key(KeyCode::Up, NONE)),
            (b"\x1b[1;5C", key(KeyCode::Right, CTRL_ONLY)),
            (b"\x1b[24;6~", key(KeyCode::F(12), CTRL_SHIFT)),
            (
                b"\x1b[97;1:3u",
                key_kind(KeyCode::Char('a'), NONE, KeyEventKind::Release),
            ),
            (b"\x1bOP", key(KeyCode::F(1), NONE)),
            (
                b"\x1b[<0;10;20M",
                mouse(MouseKind::Down(MouseButton::Left), 10, 20, NONE),
            ),
            (
                b"\x1b[M\x20\x21\x21",
                mouse(MouseKind::Down(MouseButton::Left), 1, 1, NONE),
            ),
            (
                b"\x1b[200~hi\x1b[201~",
                Event::Paste("hi".to_owned()),
            ),
            (
                b"\x1b_25a1;s;fmt=glyf\x1b\\",
                Event::Reply(Reply::Apc("25a1;s;fmt=glyf".to_owned())),
            ),
            (
                b"\x1b[?62;4c",
                Event::Reply(Reply::Csi {
                    params: "?62;4".to_owned(),
                    final_byte: 'c',
                }),
            ),
        ];
        for (input, want) in cases {
            check_incremental(input, want);
        }
    }

    #[test]
    fn empty_input_is_incomplete() {
        assert_eq!(parse_event(b""), ParseOutcome::Incomplete);
    }

    #[test]
    fn malformed_sequences_are_unknown() {
        // A byte that cannot appear inside `CSI` ends the sequence as junk.
        check(b"\x1b[1\x05", &Event::Unknown(b"\x1b[1\x05".to_vec()));
        // Unhandled tilde number and unhandled final byte.
        check(b"\x1b[99~", &Event::Unknown(b"\x1b[99~".to_vec()));
        check(b"\x1b[1;2W", &Event::Unknown(b"\x1b[1;2W".to_vec()));
        // Unknown `SS3` final byte.
        check(b"\x1bOZ", &Event::Unknown(b"\x1bOZ".to_vec()));
    }

    #[test]
    fn parse_consumes_only_one_event() {
        let stream = b"\x1b[Aab";
        let ParseOutcome::Event { event, consumed } = parse_event(stream) else {
            panic!("expected an event");
        };
        assert_eq!(event, key(KeyCode::Up, NONE));
        assert_eq!(consumed, 3);
        let ParseOutcome::Event { event, consumed } = parse_event(&stream[consumed..]) else {
            panic!("expected an event");
        };
        assert_eq!(event, key(KeyCode::Char('a'), NONE));
        assert_eq!(consumed, 1);
    }

    #[test]
    fn modifier_decoding() {
        assert_eq!(Modifiers::from_csi_param(1), Modifiers::NONE);
        // A parameter of 0 is treated as "no modifiers" too.
        assert_eq!(Modifiers::from_csi_param(0), Modifiers::NONE);
        assert_eq!(Modifiers::from_csi_param(2), SHIFT);
        assert_eq!(Modifiers::from_csi_param(3), ALT);
        assert_eq!(Modifiers::from_csi_param(5), CTRL_ONLY);
        assert_eq!(Modifiers::from_csi_param(6), CTRL_SHIFT);
        assert_eq!(
            Modifiers::from_csi_param(9),
            Modifiers {
                super_: true,
                ..NONE
            }
        );
        // Hyper (16), meta (32) and the lock bits have no field and are dropped.
        assert_eq!(Modifiers::from_csi_param(65), Modifiers::NONE);
        assert!(Modifiers::NONE.is_empty());
        assert!(Modifiers::default().is_empty());
        assert!(!SHIFT.is_empty());
    }

    #[test]
    fn modifier_display() {
        assert_eq!(Modifiers::NONE.to_string(), "");
        assert_eq!(CTRL_SHIFT.to_string(), "Ctrl+Shift");
        assert_eq!(ALT.to_string(), "Alt");
        assert_eq!(
            Modifiers {
                shift: true,
                alt: true,
                ctrl: true,
                super_: true,
            }
            .to_string(),
            "Ctrl+Alt+Shift+Super"
        );
    }

    /// Signature of [`InputReader::new`].
    type NewFn = fn() -> io::Result<InputReader>;
    /// Signature of [`InputReader::read`].
    type ReadFn = fn(&mut InputReader, Duration) -> io::Result<Option<Event>>;
    /// Signature of [`InputReader::read_blocking`].
    type ReadBlockingFn = fn(&mut InputReader) -> io::Result<Event>;
    /// Signature of [`InputReader::wait_for_advance`].
    type WaitForAdvanceFn = fn(&mut InputReader) -> io::Result<Advance>;
    /// Signature of [`InputReader::drain`].
    type DrainFn = fn(&mut InputReader);
    /// Predicate accepted by [`InputReader::wait_for_reply`].
    type ReplyPredicate = fn(&Reply) -> bool;
    /// Signature of [`InputReader::wait_for_reply`].
    type WaitForReplyFn =
        fn(&mut InputReader, Duration, ReplyPredicate) -> io::Result<Option<Reply>>;

    #[test]
    fn reader_api_is_addressable() {
        // The reader itself needs a terminal, so this only pins the public
        // signatures down. It also keeps dead-code analysis happy when this
        // file is compiled standalone as its own test crate.
        black_box::<NewFn>(InputReader::new);
        black_box::<ReadFn>(InputReader::read);
        black_box::<ReadBlockingFn>(InputReader::read_blocking);
        black_box::<WaitForAdvanceFn>(InputReader::wait_for_advance);
        black_box::<DrainFn>(InputReader::drain);
        black_box::<WaitForReplyFn>(InputReader::wait_for_reply);
        assert!(ESC_TIMEOUT >= Duration::from_millis(25));
        assert_eq!(eof_error().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn advance_keys_are_classified() {
        // Mirrors `wait_for_advance`, which needs a live terminal to exercise.
        let classify = |event: &Event| -> Option<Advance> {
            let Event::Key(key) = event else {
                return None;
            };
            if key.kind == KeyEventKind::Release {
                return None;
            }
            match key.code {
                KeyCode::Char(' ') | KeyCode::Enter => Some(Advance::Next),
                KeyCode::Char('q' | 'Q') | KeyCode::Esc => Some(Advance::Quit),
                KeyCode::Char('c' | 'd') if key.mods.ctrl => Some(Advance::Quit),
                KeyCode::Char('p') | KeyCode::Left | KeyCode::Backspace => Some(Advance::Prev),
                _ => None,
            }
        };
        let cases: &[(&[u8], Option<Advance>)] = &[
            (b" ", Some(Advance::Next)),
            (b"\r", Some(Advance::Next)),
            (b"q", Some(Advance::Quit)),
            (b"Q", Some(Advance::Quit)),
            (b"\x03", Some(Advance::Quit)),
            (b"\x04", Some(Advance::Quit)),
            (b"\x1b[D", Some(Advance::Prev)),
            (b"\x7f", Some(Advance::Prev)),
            (b"p", Some(Advance::Prev)),
            (b"x", None),
            (b"\x1b[<0;1;1M", None),
            (b"\x1b[?62;4c", None),
        ];
        for (input, want) in cases {
            let ParseOutcome::Event { event, .. } = parse_event(input) else {
                panic!("expected an event for {input:?}");
            };
            assert_eq!(classify(&event), *want, "for {input:?}");
        }
        // Escape needs the stall timeout in a real reader, but once decoded as
        // a key it quits.
        assert_eq!(
            classify(&key(KeyCode::Esc, NONE)),
            Some(Advance::Quit)
        );
    }
}
