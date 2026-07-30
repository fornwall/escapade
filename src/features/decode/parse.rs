//! The grammar an application *emits*, byte by byte.
//!
//! This is the other half of [`crate::input`]. That module parses what a terminal
//! *sends back* — keys, mouse reports, replies — and its job is to recognise the
//! handful of shapes a keyboard produces. This one parses what a program *writes*:
//! `CSI` with parameters and intermediates, `OSC` and its two terminators, `DCS`,
//! `APC`, the single shifts, the two-byte escapes, C0 controls, and the printable
//! text in between. Nothing here talks to a terminal, so all of it is testable.
//!
//! # Chunks
//!
//! Bytes arrive from a pipe in arbitrary pieces and a sequence can straddle a
//! read boundary, so [`parse_item`] answers [`Outcome::Incomplete`] for a valid
//! prefix — exactly as [`crate::input::ParseOutcome::Incomplete`] does — and
//! [`Decoder`] keeps the leftovers until the next chunk arrives. Passing
//! `end_of_input` turns those leftovers into [`Item::Unterminated`], because at
//! the end of a capture a half-finished sequence is a *finding*: "this OSC never
//! terminated" is precisely the bug someone runs `decode` to find.
//!
//! # Guessing
//!
//! Where a real terminal would silently improvise, this reports the trouble
//! instead. A control byte inside a `CSI`, parameter bytes after an intermediate,
//! a fresh `ESC` inside an `OSC` payload and a newline inside a control string are
//! all [`Item::Malformed`] rather than a best guess, and the bytes are handed back
//! so the caller can show them.

/// The escape byte, which introduces everything.
const ESC: u8 = 0x1b;

/// Bell, accepted in place of `ST` to end a control string. xterm allows it for
/// `OSC` and a great deal of software emits it, so a decoder has to know it.
const BEL: u8 = 0x07;

/// The byte after `ESC` that makes a String Terminator.
const ST_FINAL: u8 = b'\\';

/// How much text is buffered before a run is emitted without waiting for its end.
///
/// A text run is held back until something ends it, so that `"hello"` arrives as
/// one item rather than as however many pieces the pipe happened to deliver. This
/// caps the wait: a program that writes megabytes without a single control byte
/// still gets decoded incrementally instead of filling memory. Newlines are C0
/// controls and therefore end a run, so line-oriented output never comes near it.
pub const TEXT_RUN_MAX: usize = 4096;

/// Which control string this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringKind {
    /// Operating System Command, `ESC ]`: titles, colours, hyperlinks, clipboard.
    Osc,
    /// Device Control String, `ESC P`: sixel, DECRQSS, XTGETTCAP, tmux passthrough.
    Dcs,
    /// Application Program Command, `ESC _`: kitty graphics, Glyph Protocol.
    Apc,
    /// Privacy Message, `ESC ^`.
    Pm,
    /// Start Of String, `ESC X`.
    Sos,
}

impl StringKind {
    /// The name ECMA-48 gives it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Osc => "OSC",
            Self::Dcs => "DCS",
            Self::Apc => "APC",
            Self::Pm => "PM",
            Self::Sos => "SOS",
        }
    }

    /// Lowercase name, used as the `kind` field of the JSON output.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Osc => "osc",
            Self::Dcs => "dcs",
            Self::Apc => "apc",
            Self::Pm => "pm",
            Self::Sos => "sos",
        }
    }

    /// The byte that follows `ESC` to open it.
    #[must_use]
    pub fn opener(self) -> u8 {
        match self {
            Self::Osc => b']',
            Self::Dcs => b'P',
            Self::Apc => b'_',
            Self::Pm => b'^',
            Self::Sos => b'X',
        }
    }

    /// Whether `BEL` is an accepted terminator here.
    ///
    /// Only xterm's `OSC` extension blesses it. Everything else wants `ST`, so a
    /// `BEL`-terminated `APC` is worth pointing out rather than passing over.
    #[must_use]
    pub fn allows_bel(self) -> bool {
        matches!(self, Self::Osc)
    }

    /// Maps the byte after `ESC` to the kind it opens.
    fn from_opener(byte: u8) -> Option<Self> {
        match byte {
            b']' => Some(Self::Osc),
            b'P' => Some(Self::Dcs),
            b'_' => Some(Self::Apc),
            b'^' => Some(Self::Pm),
            b'X' => Some(Self::Sos),
            _ => None,
        }
    }
}

/// How a control string ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    /// `ESC \`, the String Terminator.
    St,
    /// `BEL`, xterm's one-byte alternative.
    Bel,
}

impl Terminator {
    /// Name for display and for JSON.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::St => "ST",
            Self::Bel => "BEL",
        }
    }

    /// The bytes it is written with.
    #[must_use]
    pub fn bytes(self) -> &'static str {
        match self {
            Self::St => "\u{1b}\\",
            Self::Bel => "\u{7}",
        }
    }
}

/// One `;`-separated parameter, with its `:`-separated sub-parameters.
///
/// Sub-parameters are what `SGR 4:3` (a curly underline) and
/// `SGR 38:2::1:2:3` (a 24-bit colour, with the colour-space id left empty) are
/// made of, so they cannot be flattened away.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Param {
    /// The values, in order. `None` is an empty field, meaning the default
    /// applies — or, rarely, a value too large to be a parameter at all.
    pub parts: Vec<Option<u32>>,
}

impl Param {
    /// The parameter proper, before any sub-parameter.
    #[must_use]
    pub fn value(&self) -> Option<u32> {
        self.parts.first().copied().flatten()
    }

    /// Sub-parameter `index`, counting the parameter itself as zero.
    #[must_use]
    pub fn sub(&self, index: usize) -> Option<u32> {
        self.parts.get(index).copied().flatten()
    }

    /// How many fields the parameter has, sub-parameters included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// Whether the parameter carries sub-parameters.
    #[must_use]
    pub fn has_subs(&self) -> bool {
        self.parts.len() > 1
    }
}

/// The parameter part shared by `CSI` and the prefix of a `DCS`:
/// `<private> <parameters> <intermediates> <final>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fields {
    /// Private-parameter marker: `<`, `=`, `>` or `?`.
    pub private: Option<u8>,
    /// The parameter bytes exactly as written, private marker excluded. Kept
    /// verbatim so the output can show what was in the stream rather than a
    /// normalised rewrite of it.
    pub raw: String,
    /// The parsed parameters.
    pub params: Vec<Param>,
    /// Intermediate bytes, `0x20..=0x2f` — the `$` of `CSI ? 2027 $ p`.
    pub intermediates: Vec<u8>,
    /// The final byte, `0x40..=0x7e`, which says what the sequence *is*.
    pub final_byte: u8,
}

impl Fields {
    /// Parameter `index`, or `None` when it was omitted.
    #[must_use]
    pub fn value(&self, index: usize) -> Option<u32> {
        self.params.get(index).and_then(Param::value)
    }

    /// Parameter `index`, or `default` when it was omitted — which is how every
    /// sequence with an optional count is specified.
    #[must_use]
    pub fn value_or(&self, index: usize, default: u32) -> u32 {
        self.value(index).unwrap_or(default)
    }

    /// The intermediates as text, for matching and display.
    #[must_use]
    pub fn intermediates_text(&self) -> String {
        self.intermediates.iter().map(|&b| char::from(b)).collect()
    }

    /// The final byte as a character. Always ASCII by construction.
    #[must_use]
    pub fn final_char(&self) -> char {
        char::from(self.final_byte)
    }
}

/// What was still open when the input ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// `ESC`, with nothing or only intermediates after it.
    Esc,
    /// `ESC [ …` with no final byte yet.
    Csi,
    /// `ESC N` or `ESC O` with no character after it.
    SingleShift(u8),
    /// A control string with no `ST` and no `BEL`.
    Payload(StringKind),
}

/// Why a run of bytes is not a sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flaw {
    /// A byte that cannot occur inside `CSI` appeared before the final byte.
    /// Real terminals mostly execute it and carry on, which is precisely the kind
    /// of improvisation that makes corrupted output hard to explain.
    CsiAbandoned,
    /// Parameter bytes came after an intermediate byte, which no sequence allows.
    CsiOutOfOrder,
    /// A control string was cut short by a control byte or by a fresh `ESC`.
    StringAbandoned(StringKind),
    /// `ESC` followed by a byte that begins no sequence.
    StrayEsc,
    /// Bytes that are not valid UTF-8.
    InvalidUtf8,
    /// A single byte in `0x80..=0x9f`. In a UTF-8 stream this is invalid; in a
    /// Latin-1 one it is the eight-bit form of a C1 control — `0x9b` *is* `CSI`.
    /// Which it is cannot be known from the bytes, so it is named and not acted
    /// on.
    EightBitC1(u8),
}

/// One decoded thing: a sequence, a control, a run of text, or a problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// Printable text between sequences.
    Text(String),
    /// A C0 control byte, or `DEL`.
    Control(u8),
    /// `ESC [ … <final>`.
    Csi(Fields),
    /// `ESC P <fields> <payload> ST`.
    Dcs {
        /// The parameters before the payload.
        fields: Fields,
        /// The payload bytes, terminator excluded.
        payload: Vec<u8>,
        /// How it ended.
        terminator: Terminator,
    },
    /// `OSC`, `APC`, `PM` or `SOS` with a payload.
    StringSeq {
        /// Which one.
        kind: StringKind,
        /// The payload bytes, introducer and terminator excluded.
        payload: Vec<u8>,
        /// How it ended.
        terminator: Terminator,
    },
    /// `ESC N x` (SS2) or `ESC O x` (SS3): the next character comes from G2 or G3.
    SingleShift {
        /// 2 for SS2, 3 for SS3.
        level: u8,
        /// The single shifted byte.
        byte: u8,
    },
    /// A two- or three-byte escape sequence: `ESC 7`, `ESC c`, `ESC ( B`, `ESC # 8`.
    Esc {
        /// Intermediate bytes, `0x20..=0x2f`.
        intermediates: Vec<u8>,
        /// The final byte.
        final_byte: u8,
    },
    /// The input ended in the middle of a sequence.
    Unterminated {
        /// What was open.
        pending: Pending,
    },
    /// Bytes that form no sequence.
    Malformed {
        /// What is wrong with them.
        flaw: Flaw,
    },
}

impl Item {
    /// Short kind name, used as the JSON `kind` field and for grouping.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Control(_) => "control",
            Self::Csi(_) => "csi",
            Self::Dcs { .. } => "dcs",
            Self::StringSeq { kind, .. } => kind.key(),
            Self::SingleShift { level, .. } => {
                if *level == 2 {
                    "ss2"
                } else {
                    "ss3"
                }
            }
            Self::Esc { .. } => "esc",
            Self::Unterminated { .. } => "unterminated",
            Self::Malformed { .. } => "malformed",
        }
    }

    /// Whether this is a run of printable text, which `--sequences-only` drops.
    #[must_use]
    pub fn is_text(&self) -> bool {
        matches!(self, Self::Text(_))
    }
}

/// An [`Item`] with its place in the stream and the bytes it was made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// Byte offset from the start of the input.
    pub offset: usize,
    /// Exactly the bytes this item consumed.
    pub bytes: Vec<u8>,
    /// What they mean.
    pub item: Item,
}

/// Result of [`parse_item`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// An item, and how many bytes from the front of the input it used. Always at
    /// least one byte, so a caller always makes progress.
    Item {
        /// What was decoded.
        item: Item,
        /// Bytes consumed.
        consumed: usize,
    },
    /// A valid prefix of something longer — more bytes are needed.
    Incomplete,
}

/// Builds a completed [`Outcome`].
fn item(item: Item, consumed: usize) -> Outcome {
    Outcome::Item { item, consumed }
}

/// Parses one item from the front of `buf`.
///
/// Never blocks and never panics. `end_of_input` says whether more bytes can
/// still arrive: with `false`, a valid prefix is [`Outcome::Incomplete`]; with
/// `true`, the same prefix becomes [`Item::Unterminated`] or [`Item::Malformed`],
/// and every non-empty input therefore yields an item.
#[must_use]
pub fn parse_item(buf: &[u8], end_of_input: bool) -> Outcome {
    let Some(&first) = buf.first() else {
        return Outcome::Incomplete;
    };
    match first {
        ESC => parse_escape(buf, end_of_input),
        // Every C0 control and DEL is an item of its own: a stray NUL or
        // backspace in a stream is exactly what someone is looking for.
        0x00..=0x1f | 0x7f => item(Item::Control(first), 1),
        _ => parse_text(buf, end_of_input),
    }
}

/// Parses a run of printable text, stopping at the first control byte.
///
/// Invalid UTF-8 is reported rather than replaced: the valid prefix comes back as
/// text, and the offending bytes as [`Flaw::InvalidUtf8`] or
/// [`Flaw::EightBitC1`]. A scalar value truncated at the end of the buffer is
/// [`Outcome::Incomplete`], since the rest of it may be in the next chunk.
fn parse_text(buf: &[u8], end_of_input: bool) -> Outcome {
    let scan_end = buf
        .iter()
        .position(|&byte| byte < 0x20 || byte == 0x7f)
        .unwrap_or(buf.len());
    let slice = &buf[..scan_end];

    let (valid, invalid) = match std::str::from_utf8(slice) {
        Ok(text) => (text, None),
        Err(error) => (
            // `valid_up_to` is a scalar boundary by definition, so this cannot
            // fail; the fallback keeps the parser total instead of panicking.
            std::str::from_utf8(&slice[..error.valid_up_to()]).unwrap_or(""),
            Some(error.error_len()),
        ),
    };

    if !valid.is_empty() {
        // A run is held together until something ends it, so that text arrives in
        // the pieces the program wrote rather than the pieces the pipe delivered.
        let truncated_scalar = matches!(invalid, Some(None));
        let ran_out = invalid.is_none() && scan_end == buf.len();
        let could_grow =
            !end_of_input && valid.len() < TEXT_RUN_MAX && (truncated_scalar || ran_out);
        if could_grow {
            return Outcome::Incomplete;
        }
        return item(Item::Text(valid.to_owned()), valid.len());
    }

    match invalid {
        // A scalar value cut in half. Only the end of the input makes it a fault.
        Some(None) => {
            if end_of_input {
                item(Item::Malformed { flaw: Flaw::InvalidUtf8 }, scan_end)
            } else {
                Outcome::Incomplete
            }
        }
        Some(Some(len)) => {
            let flaw = match buf.first() {
                Some(&byte) if len == 1 && matches!(byte, 0x80..=0x9f) => Flaw::EightBitC1(byte),
                _ => Flaw::InvalidUtf8,
            };
            item(Item::Malformed { flaw }, len.max(1))
        }
        // Unreachable: `parse_item` sends control bytes elsewhere, so `scan_end`
        // is at least one and `valid` is only empty when the slice was.
        None => Outcome::Incomplete,
    }
}

/// Dispatches on the byte after `ESC`.
fn parse_escape(buf: &[u8], end_of_input: bool) -> Outcome {
    let Some(&second) = buf.get(1) else {
        return if end_of_input {
            item(Item::Unterminated { pending: Pending::Esc }, 1)
        } else {
            Outcome::Incomplete
        };
    };
    match second {
        b'[' => parse_csi(buf, end_of_input),
        b'P' => parse_dcs(buf, end_of_input),
        b'N' => parse_single_shift(buf, 2, end_of_input),
        b'O' => parse_single_shift(buf, 3, end_of_input),
        _ if StringKind::from_opener(second).is_some() => {
            match StringKind::from_opener(second) {
                Some(kind) => parse_string(buf, kind, end_of_input),
                // Guarded by the arm's condition.
                None => item(Item::Malformed { flaw: Flaw::StrayEsc }, 1),
            }
        }
        // `ESC <intermediates> <final>`: charset designation, DECALN, S7C1T.
        0x20..=0x2f => parse_esc_sequence(buf, end_of_input),
        0x30..=0x7e => item(
            Item::Esc {
                intermediates: Vec::new(),
                final_byte: second,
            },
            2,
        ),
        // `ESC ESC`, `ESC` followed by a control byte, and anything in the
        // eight-bit range: the first escape led nowhere. Reported on its own, so
        // the byte after it is parsed afresh.
        _ => item(Item::Malformed { flaw: Flaw::StrayEsc }, 1),
    }
}

/// Parses `ESC N x` (SS2) and `ESC O x` (SS3), which shift one character out of
/// the G2 or G3 character set.
fn parse_single_shift(buf: &[u8], level: u8, end_of_input: bool) -> Outcome {
    match buf.get(2) {
        Some(&byte) if byte >= 0x20 && byte != 0x7f => item(Item::SingleShift { level, byte }, 3),
        // A control byte cannot be shifted into a character set.
        Some(_) => item(Item::Malformed { flaw: Flaw::StrayEsc }, 2),
        None => {
            if end_of_input {
                item(
                    Item::Unterminated {
                        pending: Pending::SingleShift(level),
                    },
                    2,
                )
            } else {
                Outcome::Incomplete
            }
        }
    }
}

/// Parses `ESC <intermediates> <final>`: `ESC ( B`, `ESC # 8`, `ESC % G`.
fn parse_esc_sequence(buf: &[u8], end_of_input: bool) -> Outcome {
    let mut intermediates = Vec::new();
    let mut index = 1;
    while let Some(&byte) = buf.get(index) {
        match byte {
            0x20..=0x2f => {
                intermediates.push(byte);
                index += 1;
            }
            0x30..=0x7e => {
                return item(
                    Item::Esc {
                        intermediates,
                        final_byte: byte,
                    },
                    index + 1,
                );
            }
            _ => return item(Item::Malformed { flaw: Flaw::StrayEsc }, index),
        }
    }
    if end_of_input {
        item(Item::Unterminated { pending: Pending::Esc }, index)
    } else {
        Outcome::Incomplete
    }
}

/// Outcome of scanning the parameter part of a `CSI` or a `DCS`.
#[derive(Debug)]
enum FieldScan {
    /// Well-formed, ending at the final byte.
    Complete {
        /// The parsed parameters.
        fields: Fields,
        /// Total length, introducer included.
        len: usize,
    },
    /// Still waiting for the final byte.
    Incomplete,
    /// A byte appeared that cannot be there.
    Invalid {
        /// Bytes to hand back, up to but excluding the offending byte.
        len: usize,
        /// Whether the fault is parameters after an intermediate.
        out_of_order: bool,
    },
}

/// Splits `<private> <parameters> <intermediates> <final>` starting at `start`.
fn scan_fields(buf: &[u8], start: usize) -> FieldScan {
    let mut index = start;
    let private = match buf.get(index) {
        Some(&byte) if matches!(byte, b'<' | b'=' | b'>' | b'?') => {
            index += 1;
            Some(byte)
        }
        _ => None,
    };
    let params_start = index;
    // Set once an intermediate byte has ended the parameter run.
    let mut params_end = None;
    let mut intermediates = Vec::new();

    while let Some(&byte) = buf.get(index) {
        match byte {
            // Parameter bytes: digits, `;`, `:`, and the private markers.
            0x30..=0x3f => {
                if params_end.is_some() {
                    return FieldScan::Invalid {
                        len: index,
                        out_of_order: true,
                    };
                }
                index += 1;
            }
            // Intermediate bytes.
            0x20..=0x2f => {
                params_end.get_or_insert(index);
                intermediates.push(byte);
                index += 1;
            }
            // The final byte ends the sequence.
            0x40..=0x7e => {
                let raw = String::from_utf8_lossy(&buf[params_start..params_end.unwrap_or(index)])
                    .into_owned();
                let params = parse_params(&raw);
                return FieldScan::Complete {
                    fields: Fields {
                        private,
                        raw,
                        params,
                        intermediates,
                        final_byte: byte,
                    },
                    len: index + 1,
                };
            }
            _ => {
                return FieldScan::Invalid {
                    len: index.max(start),
                    out_of_order: false,
                };
            }
        }
    }
    FieldScan::Incomplete
}

/// Splits parameter text into parameters and sub-parameters.
///
/// `1;31` is two parameters, `4:3` is one with a sub-parameter, and
/// `38:2::1:2:3` is one with five — the empty one being the colour space, which
/// the specification says to leave out.
fn parse_params(raw: &str) -> Vec<Param> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(';')
        .map(|field| Param {
            parts: field
                .split(':')
                .map(|part| if part.is_empty() { None } else { part.parse().ok() })
                .collect(),
        })
        .collect()
}

/// Parses `ESC [ …`.
fn parse_csi(buf: &[u8], end_of_input: bool) -> Outcome {
    match scan_fields(buf, 2) {
        FieldScan::Complete { fields, len } => item(Item::Csi(fields), len),
        FieldScan::Incomplete => {
            if end_of_input {
                item(Item::Unterminated { pending: Pending::Csi }, buf.len())
            } else {
                Outcome::Incomplete
            }
        }
        FieldScan::Invalid { len, out_of_order } => item(
            Item::Malformed {
                flaw: if out_of_order {
                    Flaw::CsiOutOfOrder
                } else {
                    Flaw::CsiAbandoned
                },
            },
            len,
        ),
    }
}

/// Outcome of scanning a control string's payload.
#[derive(Debug)]
enum PayloadScan {
    /// Terminated properly.
    Complete {
        /// Index one past the last payload byte.
        end: usize,
        /// Which terminator ended it.
        terminator: Terminator,
        /// Total length, terminator included.
        len: usize,
    },
    /// No terminator yet.
    Incomplete,
    /// Ended by something that is not a terminator.
    Abandoned {
        /// Index of the byte that ended it, which is also how much to consume.
        end: usize,
    },
}

/// Scans a control-string payload from `start` to its terminator.
///
/// Only `HT` and non-control bytes belong in a control string. A newline inside
/// one almost always means a capture was cut mid-sequence — and treating it as
/// payload would swallow the whole rest of the file — so it ends the string as
/// [`PayloadScan::Abandoned`] instead.
fn scan_payload(buf: &[u8], start: usize) -> PayloadScan {
    let mut index = start;
    while let Some(&byte) = buf.get(index) {
        match byte {
            BEL => {
                return PayloadScan::Complete {
                    end: index,
                    terminator: Terminator::Bel,
                    len: index + 1,
                };
            }
            ESC => {
                return match buf.get(index + 1) {
                    None => PayloadScan::Incomplete,
                    Some(&ST_FINAL) => PayloadScan::Complete {
                        end: index,
                        terminator: Terminator::St,
                        len: index + 2,
                    },
                    // A fresh escape abandons the string; it is parsed next.
                    Some(_) => PayloadScan::Abandoned { end: index },
                };
            }
            0x00..=0x08 | 0x0a..=0x1f | 0x7f => return PayloadScan::Abandoned { end: index },
            _ => index += 1,
        }
    }
    PayloadScan::Incomplete
}

/// Parses `OSC`, `APC`, `PM` and `SOS`.
fn parse_string(buf: &[u8], kind: StringKind, end_of_input: bool) -> Outcome {
    match scan_payload(buf, 2) {
        PayloadScan::Complete {
            end,
            terminator,
            len,
        } => item(
            Item::StringSeq {
                kind,
                payload: buf[2..end].to_vec(),
                terminator,
            },
            len,
        ),
        PayloadScan::Abandoned { end } => item(
            Item::Malformed {
                flaw: Flaw::StringAbandoned(kind),
            },
            end,
        ),
        PayloadScan::Incomplete => {
            if end_of_input {
                item(
                    Item::Unterminated {
                        pending: Pending::Payload(kind),
                    },
                    buf.len(),
                )
            } else {
                Outcome::Incomplete
            }
        }
    }
}

/// Parses `ESC P <fields> <payload> ST`.
///
/// A `DCS` carries a `CSI`-shaped prefix before its payload, which is what makes
/// `DCS $ q m ST` a DECRQSS request for the current `SGR` state and
/// `DCS 0 ; 1 ; 0 q …` a sixel image.
fn parse_dcs(buf: &[u8], end_of_input: bool) -> Outcome {
    let (fields, prefix_len) = match scan_fields(buf, 2) {
        FieldScan::Complete { fields, len } => (fields, len),
        FieldScan::Incomplete => {
            return if end_of_input {
                item(
                    Item::Unterminated {
                        pending: Pending::Payload(StringKind::Dcs),
                    },
                    buf.len(),
                )
            } else {
                Outcome::Incomplete
            };
        }
        FieldScan::Invalid { len, out_of_order } => {
            return item(
                Item::Malformed {
                    flaw: if out_of_order {
                        Flaw::CsiOutOfOrder
                    } else {
                        Flaw::CsiAbandoned
                    },
                },
                len,
            );
        }
    };

    match scan_payload(buf, prefix_len) {
        PayloadScan::Complete {
            end,
            terminator,
            len,
        } => item(
            Item::Dcs {
                fields,
                payload: buf[prefix_len..end].to_vec(),
                terminator,
            },
            len,
        ),
        PayloadScan::Abandoned { end } => item(
            Item::Malformed {
                flaw: Flaw::StringAbandoned(StringKind::Dcs),
            },
            end,
        ),
        PayloadScan::Incomplete => {
            if end_of_input {
                item(
                    Item::Unterminated {
                        pending: Pending::Payload(StringKind::Dcs),
                    },
                    buf.len(),
                )
            } else {
                Outcome::Incomplete
            }
        }
    }
}

/// Feeds chunks through [`parse_item`], holding back whatever straddles a
/// boundary.
///
/// The offsets it reports are absolute, counted from the first byte ever fed, so
/// `--hex` can point at a place in the file.
#[derive(Debug, Default)]
pub struct Decoder {
    /// Bytes not yet decoded.
    buf: Vec<u8>,
    /// Stream offset of `buf[0]`.
    offset: usize,
}

impl Decoder {
    /// A decoder with nothing buffered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a chunk and returns everything that is now complete.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Decoded> {
        self.buf.extend_from_slice(chunk);
        self.drain(false)
    }

    /// Returns what is left, once no more bytes can arrive. Anything still open
    /// becomes [`Item::Unterminated`].
    pub fn finish(&mut self) -> Vec<Decoded> {
        self.drain(true)
    }

    /// How many bytes are waiting for more input.
    ///
    /// Only the tests ask, but what they ask is load-bearing: that a text run
    /// held back at a chunk boundary is bounded by [`TEXT_RUN_MAX`].
    #[cfg(test)]
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Decodes as much of the buffer as it can.
    fn drain(&mut self, end_of_input: bool) -> Vec<Decoded> {
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < self.buf.len() {
            match parse_item(&self.buf[pos..], end_of_input) {
                Outcome::Item { item, consumed } => {
                    // Clamped so that a parser bug cannot become an endless loop
                    // or an out-of-range slice.
                    let consumed = consumed.clamp(1, self.buf.len() - pos);
                    out.push(Decoded {
                        offset: self.offset + pos,
                        bytes: self.buf[pos..pos + consumed].to_vec(),
                        item,
                    });
                    pos += consumed;
                }
                Outcome::Incomplete => break,
            }
        }
        self.buf.drain(..pos);
        self.offset += pos;
        out
    }
}

/// Decodes a whole byte string in one go.
///
/// [`Decoder`] is what the subcommand uses, because input arrives in chunks; this
/// is the convenience form the tests of every module here are written against.
#[cfg(test)]
#[must_use]
pub fn decode_all(bytes: &[u8]) -> Vec<Decoded> {
    let mut decoder = Decoder::new();
    let mut items = decoder.feed(bytes);
    items.extend(decoder.finish());
    items
}

/// Decodes a byte string as if it arrived in chunks of `size` bytes.
///
/// Only interesting to tests, where it proves that where the chunk boundaries
/// fall makes no difference to the result.
#[cfg(test)]
#[must_use]
pub fn decode_chunked(bytes: &[u8], size: usize) -> Vec<Decoded> {
    let mut decoder = Decoder::new();
    let mut items = Vec::new();
    for chunk in bytes.chunks(size.max(1)) {
        items.extend(decoder.feed(chunk));
    }
    items.extend(decoder.finish());
    items
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The items of a decode, without offsets, for terse assertions.
    fn items(bytes: &[u8]) -> Vec<Item> {
        decode_all(bytes).into_iter().map(|d| d.item).collect()
    }

    fn text(value: &str) -> Item {
        Item::Text(value.to_owned())
    }

    fn csi(bytes: &[u8]) -> Fields {
        match parse_item(bytes, true) {
            Outcome::Item {
                item: Item::Csi(fields),
                ..
            } => fields,
            other => panic!("expected a CSI, got {other:?}"),
        }
    }

    #[test]
    fn plain_text_is_one_run() {
        assert_eq!(items(b"hello"), vec![text("hello")]);
    }

    #[test]
    fn the_example_from_the_brief() {
        assert_eq!(
            items(b"\x1b[1;31mred\x1b[0m"),
            vec![
                Item::Csi(csi(b"\x1b[1;31m")),
                text("red"),
                Item::Csi(csi(b"\x1b[0m")),
            ]
        );
    }

    #[test]
    fn csi_splits_into_private_params_intermediates_and_final() {
        let fields = csi(b"\x1b[?2027$p");
        assert_eq!(fields.private, Some(b'?'));
        assert_eq!(fields.raw, "2027");
        assert_eq!(fields.value(0), Some(2027));
        assert_eq!(fields.intermediates, vec![b'$']);
        assert_eq!(fields.final_byte, b'p');
    }

    #[test]
    fn csi_with_no_parameters_has_none() {
        let fields = csi(b"\x1b[H");
        assert_eq!(fields.private, None);
        assert!(fields.params.is_empty());
        assert_eq!(fields.value_or(0, 1), 1, "an absent parameter is its default");
    }

    #[test]
    fn an_empty_parameter_is_the_default_not_a_zero() {
        let fields = csi(b"\x1b[;5H");
        assert_eq!(fields.value(0), None);
        assert_eq!(fields.value(1), Some(5));
        assert_eq!(fields.value_or(0, 1), 1);
    }

    #[test]
    fn sub_parameters_survive_parsing() {
        let fields = csi(b"\x1b[4:3m");
        assert_eq!(fields.params.len(), 1);
        assert!(fields.params[0].has_subs());
        assert_eq!(fields.params[0].value(), Some(4));
        assert_eq!(fields.params[0].sub(1), Some(3));
    }

    #[test]
    fn an_omitted_colour_space_is_an_empty_sub_parameter() {
        // `38:2::1:2:3` is the specified spelling: the colour space id is left
        // out, which is a hole in the middle of the parameter, not a missing one.
        let fields = csi(b"\x1b[38:2::1:2:3m");
        let param = &fields.params[0];
        assert_eq!(param.len(), 6);
        assert_eq!(param.value(), Some(38));
        assert_eq!(param.sub(1), Some(2));
        assert_eq!(param.sub(2), None, "the colour space was left empty");
        assert_eq!(
            (param.sub(3), param.sub(4), param.sub(5)),
            (Some(1), Some(2), Some(3))
        );
    }

    #[test]
    fn a_parameter_too_large_to_be_one_reads_as_absent() {
        // Nothing sensible can be done with 2^40, and a wrong number would be
        // worse than "no number".
        let fields = csi(b"\x1b[1099511627776m");
        assert_eq!(fields.value(0), None);
        assert_eq!(fields.raw, "1099511627776", "the bytes are still shown");
    }

    #[test]
    fn a_seven_digit_mode_number_still_parses() {
        // `doctor` asks about mode 9876543 on purpose, so this must not clamp.
        let fields = csi(b"\x1b[?9876543$p");
        assert_eq!(fields.value(0), Some(9_876_543));
    }

    #[test]
    fn osc_ends_at_st_or_at_bel() {
        let with_st = items(b"\x1b]0;title\x1b\\");
        assert_eq!(
            with_st,
            vec![Item::StringSeq {
                kind: StringKind::Osc,
                payload: b"0;title".to_vec(),
                terminator: Terminator::St,
            }]
        );
        let with_bel = items(b"\x1b]0;title\x07");
        assert_eq!(
            with_bel,
            vec![Item::StringSeq {
                kind: StringKind::Osc,
                payload: b"0;title".to_vec(),
                terminator: Terminator::Bel,
            }]
        );
    }

    #[test]
    fn a_captured_hyperlink_stream_decodes_into_five_items() {
        let stream = b"\x1b]8;;https://example.com\x1b\\click me\x1b]8;;\x1b\\";
        assert_eq!(
            items(stream),
            vec![
                Item::StringSeq {
                    kind: StringKind::Osc,
                    payload: b"8;;https://example.com".to_vec(),
                    terminator: Terminator::St,
                },
                text("click me"),
                Item::StringSeq {
                    kind: StringKind::Osc,
                    payload: b"8;;".to_vec(),
                    terminator: Terminator::St,
                },
            ]
        );
    }

    #[test]
    fn apc_and_dcs_carry_their_payloads() {
        assert_eq!(
            items(b"\x1b_25a1;s\x1b\\"),
            vec![Item::StringSeq {
                kind: StringKind::Apc,
                payload: b"25a1;s".to_vec(),
                terminator: Terminator::St,
            }]
        );
        assert_eq!(
            items(b"\x1bP$qm\x1b\\"),
            vec![Item::Dcs {
                fields: Fields {
                    private: None,
                    raw: String::new(),
                    params: Vec::new(),
                    intermediates: vec![b'$'],
                    final_byte: b'q',
                },
                payload: b"m".to_vec(),
                terminator: Terminator::St,
            }]
        );
    }

    #[test]
    fn a_dcs_prefix_is_parsed_like_a_csi() {
        // Sixel: `DCS 0;1;0 q <data> ST`.
        let decoded = items(b"\x1bP0;1;0q#0;2;0;0;0\x1b\\");
        match &decoded[0] {
            Item::Dcs { fields, payload, .. } => {
                assert_eq!(fields.final_byte, b'q');
                assert_eq!(fields.value(1), Some(1));
                assert_eq!(payload, b"#0;2;0;0;0");
            }
            other => panic!("expected a DCS, got {other:?}"),
        }
    }

    #[test]
    fn privacy_message_and_start_of_string_are_recognised() {
        assert_eq!(
            items(b"\x1b^secret\x1b\\"),
            vec![Item::StringSeq {
                kind: StringKind::Pm,
                payload: b"secret".to_vec(),
                terminator: Terminator::St,
            }]
        );
        assert_eq!(
            items(b"\x1bXstring\x1b\\"),
            vec![Item::StringSeq {
                kind: StringKind::Sos,
                payload: b"string".to_vec(),
                terminator: Terminator::St,
            }]
        );
    }

    #[test]
    fn single_shifts_take_the_next_byte() {
        assert_eq!(
            items(b"\x1bNx\x1bOy"),
            vec![
                Item::SingleShift { level: 2, byte: b'x' },
                Item::SingleShift { level: 3, byte: b'y' },
            ]
        );
    }

    #[test]
    fn two_byte_escapes_and_charset_designations() {
        assert_eq!(
            items(b"\x1b7\x1b8\x1bc"),
            vec![
                Item::Esc {
                    intermediates: Vec::new(),
                    final_byte: b'7'
                },
                Item::Esc {
                    intermediates: Vec::new(),
                    final_byte: b'8'
                },
                Item::Esc {
                    intermediates: Vec::new(),
                    final_byte: b'c'
                },
            ]
        );
        assert_eq!(
            items(b"\x1b(B\x1b#8"),
            vec![
                Item::Esc {
                    intermediates: vec![b'('],
                    final_byte: b'B'
                },
                Item::Esc {
                    intermediates: vec![b'#'],
                    final_byte: b'8'
                },
            ]
        );
    }

    #[test]
    fn c0_controls_are_items_of_their_own() {
        assert_eq!(
            items(b"a\r\nb\x07"),
            vec![
                text("a"),
                Item::Control(b'\r'),
                Item::Control(b'\n'),
                text("b"),
                Item::Control(0x07),
            ]
        );
        assert_eq!(items(b"\x7f"), vec![Item::Control(0x7f)]);
    }

    #[test]
    fn an_unterminated_osc_is_reported_as_one() {
        assert_eq!(
            items(b"\x1b]8;;broken"),
            vec![Item::Unterminated {
                pending: Pending::Payload(StringKind::Osc)
            }]
        );
    }

    #[test]
    fn an_unterminated_csi_dcs_apc_and_esc_are_all_reported() {
        assert_eq!(
            items(b"\x1b[1;3"),
            vec![Item::Unterminated { pending: Pending::Csi }]
        );
        assert_eq!(
            items(b"\x1bP$q"),
            vec![Item::Unterminated {
                pending: Pending::Payload(StringKind::Dcs)
            }]
        );
        assert_eq!(
            items(b"\x1b_Gf=100"),
            vec![Item::Unterminated {
                pending: Pending::Payload(StringKind::Apc)
            }]
        );
        assert_eq!(
            items(b"\x1b"),
            vec![Item::Unterminated { pending: Pending::Esc }]
        );
        assert_eq!(
            items(b"\x1b("),
            vec![Item::Unterminated { pending: Pending::Esc }]
        );
        assert_eq!(
            items(b"\x1bN"),
            vec![Item::Unterminated {
                pending: Pending::SingleShift(2)
            }]
        );
    }

    #[test]
    fn an_ending_st_is_not_needed_to_have_arrived_yet() {
        // The same bytes, before the end of the input, are merely incomplete.
        assert_eq!(parse_item(b"\x1b]8;;broken", false), Outcome::Incomplete);
        assert_eq!(parse_item(b"\x1b[1;3", false), Outcome::Incomplete);
        assert_eq!(parse_item(b"\x1b", false), Outcome::Incomplete);
        // `ESC` then `ESC \` split across the boundary: the terminator is two
        // bytes and only the first has arrived.
        assert_eq!(parse_item(b"\x1b]0;t\x1b", false), Outcome::Incomplete);
    }

    #[test]
    fn a_control_byte_inside_a_csi_abandons_it() {
        assert_eq!(
            items(b"\x1b[1;\x07m"),
            vec![
                Item::Malformed { flaw: Flaw::CsiAbandoned },
                Item::Control(0x07),
                text("m"),
            ]
        );
    }

    #[test]
    fn parameters_after_an_intermediate_are_malformed() {
        assert_eq!(
            items(b"\x1b[1$2p"),
            vec![
                Item::Malformed {
                    flaw: Flaw::CsiOutOfOrder
                },
                text("2p"),
            ]
        );
    }

    #[test]
    fn a_fresh_escape_inside_a_payload_abandons_the_string() {
        // The second sequence is still decoded, which is the point of handing the
        // bytes back rather than swallowing them.
        assert_eq!(
            items(b"\x1b]8;;x\x1b[0m"),
            vec![
                Item::Malformed {
                    flaw: Flaw::StringAbandoned(StringKind::Osc)
                },
                Item::Csi(csi(b"\x1b[0m")),
            ]
        );
    }

    #[test]
    fn a_newline_inside_an_osc_ends_it_rather_than_swallowing_the_file() {
        // A log line cut mid-sequence is the common case, and treating the rest
        // of the file as payload would hide everything after it.
        assert_eq!(
            items(b"\x1b]8;;broken\nnext line"),
            vec![
                Item::Malformed {
                    flaw: Flaw::StringAbandoned(StringKind::Osc)
                },
                Item::Control(b'\n'),
                text("next line"),
            ]
        );
    }

    #[test]
    fn a_stray_escape_is_reported_and_the_next_sequence_still_decodes() {
        assert_eq!(
            items(b"\x1b\x1b[0m"),
            vec![
                Item::Malformed { flaw: Flaw::StrayEsc },
                Item::Csi(csi(b"\x1b[0m")),
            ]
        );
    }

    #[test]
    fn invalid_utf8_is_named_and_the_text_around_it_survives() {
        assert_eq!(
            items(b"a\xffb"),
            vec![
                text("a"),
                Item::Malformed { flaw: Flaw::InvalidUtf8 },
                text("b"),
            ]
        );
    }

    #[test]
    fn a_truncated_scalar_at_the_end_is_invalid_utf8() {
        // The first two bytes of a four-byte emoji, and no more input coming.
        assert_eq!(
            items(b"\xf0\x9f"),
            vec![Item::Malformed { flaw: Flaw::InvalidUtf8 }]
        );
        // Mid-stream it is merely incomplete.
        assert_eq!(parse_item(b"\xf0\x9f", false), Outcome::Incomplete);
    }

    #[test]
    fn an_eight_bit_c1_byte_is_named_not_acted_on() {
        // 0x9b is CSI in the eight-bit encoding. In a UTF-8 stream it is a broken
        // byte, and acting on it would be a guess.
        assert_eq!(
            items(b"\x9b1m"),
            vec![
                Item::Malformed {
                    flaw: Flaw::EightBitC1(0x9b)
                },
                text("1m"),
            ]
        );
    }

    #[test]
    fn a_family_emoji_stays_one_text_run() {
        let family = "family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert_eq!(items(family.as_bytes()), vec![text(family)]);
    }

    #[test]
    fn chunk_boundaries_never_change_the_result() {
        let stream = b"\x1b[1;31mred\x1b[0m\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x07\
                       \x1b_Gf=100,a=T;AAAA\x1b\\family \xf0\x9f\x91\xa8\xe2\x80\x8d\xf0\x9f\x91\xa7\n";
        let whole = decode_all(stream);
        for size in 1..=stream.len() {
            assert_eq!(
                decode_chunked(stream, size),
                whole,
                "chunks of {size} bytes decoded differently"
            );
        }
    }

    #[test]
    fn offsets_and_bytes_point_back_into_the_stream() {
        let stream = b"\x1b[1mbold\x1b[0m";
        let decoded = decode_all(stream);
        for entry in &decoded {
            assert_eq!(
                &stream[entry.offset..entry.offset + entry.bytes.len()],
                entry.bytes.as_slice(),
                "{entry:?}"
            );
        }
        assert_eq!(decoded[1].offset, 4);
        assert_eq!(decoded[2].offset, 8);
    }

    #[test]
    fn a_long_text_run_is_emitted_before_it_grows_without_bound() {
        let long = "x".repeat(TEXT_RUN_MAX * 2 + 5);
        let mut decoder = Decoder::new();
        let items = decoder.feed(long.as_bytes());
        assert!(!items.is_empty(), "the run must not be held indefinitely");
        assert!(
            decoder.buffered() < TEXT_RUN_MAX,
            "at most one capped run stays buffered"
        );
    }

    #[test]
    fn a_text_run_is_not_split_at_a_chunk_boundary() {
        // The reason runs are held back at all: `"hello"` must not arrive as
        // `"hel"` and `"lo"` just because the pipe delivered it that way.
        let mut decoder = Decoder::new();
        assert!(decoder.feed(b"hel").is_empty());
        assert!(decoder.feed(b"lo").is_empty());
        assert_eq!(
            decoder.feed(b"\n").first().map(|d| d.item.clone()),
            Some(text("hello"))
        );
    }

    #[test]
    fn every_single_byte_input_makes_progress() {
        for byte in 0u8..=255 {
            let outcome = parse_item(&[byte], true);
            match outcome {
                Outcome::Item { consumed, .. } => {
                    assert!(consumed >= 1, "byte {byte:#04x} consumed nothing");
                }
                Outcome::Incomplete => panic!("byte {byte:#04x} was incomplete at end of input"),
            }
        }
    }

    #[test]
    fn every_two_byte_input_makes_progress() {
        for first in 0u8..=255 {
            for second in 0u8..=255 {
                let input = [first, second];
                match parse_item(&input, true) {
                    Outcome::Item { consumed, .. } => {
                        assert!((1..=2).contains(&consumed), "{input:?} consumed {consumed}");
                    }
                    Outcome::Incomplete => panic!("{input:?} was incomplete at end of input"),
                }
                // And every such input decodes fully, without looping.
                assert!(!decode_all(&input).is_empty());
            }
        }
    }

    #[test]
    fn decoding_consumes_exactly_the_input() {
        let stream = b"\x1b[1m\x07text\x1b]0;t\x07\xff\x1b_G\x1b\\\x1bP$qm\x1b\\";
        let decoded = decode_all(stream);
        let total: usize = decoded.iter().map(|d| d.bytes.len()).sum();
        assert_eq!(total, stream.len(), "{decoded:?}");
        let mut expected = 0;
        for entry in &decoded {
            assert_eq!(entry.offset, expected);
            expected += entry.bytes.len();
        }
    }
}
