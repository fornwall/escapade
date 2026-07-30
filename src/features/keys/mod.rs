//! The kitty keyboard protocol: `CSI > flags u` and the `CSI … u` key encoding.
//!
//! A terminal keyboard has always been a byte stream, and the bytes are lossy.
//! `Ctrl+i` and Tab are both `0x09`. `Ctrl+m` and Enter are both `0x0d`. Escape
//! is `0x1b`, which is also the first byte of every escape sequence, so an
//! application can only recognise the *key* by waiting to see whether more bytes
//! follow. Nothing reports a key **release**. And **Shift+Enter is
//! indistinguishable from Enter** — the single most-reported "why can't my
//! terminal app do this" limitation there is.
//!
//! The kitty keyboard protocol fixes that by *progressive enhancement*: the
//! application asks for exactly the extra information it can handle, one flag bit
//! at a time, and everything it does not ask for keeps its legacy encoding.
//!
//! ```text
//! CSI > 1 u        push flags = 1 (disambiguate escape codes)
//! CSI < u          pop, restoring whatever was there before
//! CSI ? u          which flags are in effect?
//! CSI ? 1 u        the terminal's answer
//! CSI 13 ; 2 u     Shift+Enter — a key legacy encoding cannot express
//! ```
//!
//! # What this module is, and is not
//!
//! The *parser* for the protocol's key encoding is [`crate::input`], and it was
//! there before this feature existed — every demo in `escapade` pushes
//! `CSI > 1 u` so it can see key releases. This module is the part that
//! **explains, probes and demonstrates** it:
//!
//! * [`Flags`] and [`FLAGS`] — the five flag bits, and what each one changes.
//! * [`push`], [`pop`], [`set`], [`query`] — the sequences, built symbolically.
//! * [`probe`] — asks with `CSI ? u`, behind a `CSI c` barrier.
//! * [`watch`] — the `showkey` of modern terminals: every event, with its bytes.
//! * [`demo`] — a keyboard-driven walk through the flags, one at a time.
//! * [`prompt`] — the reusable *interactive input step*, for any feature's demo.
//!
//! # Which functional keys this project supports
//!
//! A correction worth stating plainly, because it is widely got wrong: the
//! protocol's private-use functional-key range **does not contain** Escape,
//! Enter, Tab, Backspace, the arrows, Home, End, Page Up, Page Down or F1–F12.
//! Those keep their legacy encodings — `27u`, `13u`, `9u`, `127u`, `CSI 1 A`
//! through `CSI 1 D`, `CSI 2 ~`, `CSI 3 ~`, and so on. The private-use table
//! starts at `CAPS_LOCK` = 57358 and holds only the keys legacy encodings have no
//! room for: the lock keys, F13–F35, the keypad, the media keys and the
//! individual modifier keys.
//!
//! `crate::input::kitty_key_code` documents exactly which of those codes this
//! project maps and which it deliberately leaves as [`crate::input::Event::Unknown`],
//! and that is the source of truth for what `escapade` reports.
//!
//! Specification: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>

pub mod bytes;
pub mod demo;
pub mod event;
pub mod probe;
pub mod prompt;
pub mod watch;

use std::{fmt, process::ExitCode};

use anyhow::Result;

use crate::{
    cli::{KeysAction, KeysCommand},
    db::Catalog,
    input::Reply,
    term::CSI,
    theme::Theme,
};

/// The feature id in the database. The subcommand is `keys`.
pub const FEATURE_ID: &str = "keyboard-protocol";

// ---------------------------------------------------------------------------
// The flags
// ---------------------------------------------------------------------------

/// One progressive-enhancement flag bit, with what it changes on the wire.
///
/// The `detail` text is what a reader actually needs: the flag names are short
/// enough to be misleading, and the interesting part of each one is which
/// sequences start to look different.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlagSpec {
    /// The bit value, as it appears in the bitmask.
    pub bit: u8,
    /// The specification's name for it.
    pub name: &'static str,
    /// One line, for a table or a `--list`.
    pub summary: &'static str,
    /// What changes on the wire when it is set.
    pub detail: &'static str,
}

/// The five flags, lowest bit first.
///
/// The order is the specification's, and it is also the order to adopt them in:
/// each one is useful alone, and the later ones assume the earlier ones are
/// understood. Flag 16 is defined only in combination with flag 8.
pub const FLAGS: &[FlagSpec] = &[
    FlagSpec {
        bit: 0b0_0001,
        name: "disambiguate escape codes",
        summary: "every key that produces no text gets its own unambiguous sequence",
        detail: "Escape, Alt+key, Ctrl+key, Ctrl+Alt+key and Shift+Alt+key stop \
                 sharing bytes with anything else: they arrive as `CSI number ; \
                 modifiers u`, or as `CSI 1 ; modifiers <letter>` for the keys \
                 that already had a CSI form. This is the flag that makes \
                 Shift+Enter `CSI 13 ; 2 u` instead of a bare carriage return, \
                 and it is the one to enable if you enable only one. Enter, Tab \
                 and Backspace still send their legacy bytes when unmodified.",
    },
    FlagSpec {
        bit: 0b0_0010,
        name: "report event types",
        summary: "key repeat and key release become visible, not just press",
        detail: "Events grow an `: event-type` sub-parameter on the modifier \
                 field: `1` press, `2` repeat, `3` release. Without this flag a \
                 repeat is reported as another press and a release is not \
                 reported at all, so an application cannot know what is being \
                 held down. Enter, Tab and Backspace get release events only if \
                 flag 8 is also set.",
    },
    FlagSpec {
        bit: 0b0_0100,
        name: "report alternate keys",
        summary: "the shifted keycap and the same physical key in the PC-101 layout",
        detail: "The key field grows up to two colon-separated alternates: \
                 `CSI unicode-key : shifted-key : base-layout-key u`. The shifted \
                 key is only present when Shift is in the modifiers. The base \
                 layout key is what the same physical key would produce on a \
                 standard PC-101 layout, which is how a shortcut keeps working on \
                 a Cyrillic or Dvorak keyboard.",
    },
    FlagSpec {
        bit: 0b0_1000,
        name: "report all keys as escape codes",
        summary: "even plain text keys and bare modifier presses arrive as sequences",
        detail: "Normally a key that produces text just sends the text. With this \
                 flag every key is an escape code, including Enter, Tab, \
                 Backspace and the modifier keys pressed on their own. Note what \
                 that costs: **Ctrl+C no longer raises SIGINT** — it arrives as \
                 `CSI 99 ; 5 u` and the application is responsible for it.",
    },
    FlagSpec {
        bit: 0b1_0000,
        name: "report associated text",
        summary: "the text a key produced, carried inside the escape code",
        detail: "Adds a third field of colon-separated codepoints: `CSI key ; \
                 modifiers ; text u`. It exists because flag 8 takes the text \
                 away — with both set, an application gets the key identity *and* \
                 the characters it produced, which is what a text editor needs. \
                 The specification defines this flag only as an enhancement to \
                 flag 8; on its own it is undefined.",
    },
];

/// A set of progressive-enhancement flags.
///
/// A newtype rather than a bare `u8` so that the bitmask cannot be confused with
/// a key code or a modifier mask, both of which are also small integers in this
/// protocol. The wire form is still just the number, which is what
/// [`Flags::bits`] hands back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub struct Flags(u8);

impl Flags {
    /// No enhancements: the legacy encoding, exactly as it has always been.
    pub const NONE: Self = Self(0);

    /// Disambiguate escape codes (`0b1`).
    pub const DISAMBIGUATE: Self = Self(0b0_0001);
    /// Report event types — press, repeat, release (`0b10`).
    pub const EVENT_TYPES: Self = Self(0b0_0010);
    /// Report alternate keys — shifted keycap and base layout (`0b100`).
    pub const ALTERNATE_KEYS: Self = Self(0b0_0100);
    /// Report all keys as escape codes (`0b1000`).
    pub const ALL_AS_ESCAPE_CODES: Self = Self(0b0_1000);
    /// Report associated text (`0b10000`).
    pub const ASSOCIATED_TEXT: Self = Self(0b1_0000);

    /// Every flag the specification defines.
    pub const ALL: Self = Self(0b1_1111);

    /// Wraps a raw bitmask, including bits the specification does not define —
    /// a terminal may answer with more than five bits, and hiding that would
    /// make this build's age look like the terminal's limitation.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The raw bitmask, as it goes on the wire.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every bit of `other` is set here.
    ///
    /// `Flags::NONE` is contained in everything, so a caller asking about no
    /// flags at all always gets `true`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no flag at all is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The union of two sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The specifications of the flags that are set, lowest bit first.
    #[must_use]
    pub fn specs(self) -> Vec<&'static FlagSpec> {
        FLAGS
            .iter()
            .filter(|spec| self.0 & spec.bit != 0)
            .collect()
    }

    /// The names of the flags that are set, lowest bit first.
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        self.specs().iter().map(|spec| spec.name).collect()
    }

    /// Bits that are set but which the specification does not define.
    ///
    /// Reported rather than masked away: a terminal that answers `63` knows a
    /// sixth flag this build has never heard of, and that is news about the
    /// terminal, not an error in its reply.
    #[must_use]
    pub const fn unknown_bits(self) -> u8 {
        self.0 & !Self::ALL.0
    }
}

impl fmt::Display for Flags {
    /// Renders the bitmask and the names, e.g. `3 (disambiguate escape codes,
    /// report event types)`, or `0 (none — legacy encoding)` when empty.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return write!(f, "0 (none — legacy encoding)");
        }
        let mut names = self.names();
        let unknown = self.unknown_bits();
        let extra;
        if unknown != 0 {
            extra = format!("unknown bits {unknown:#b}");
            names.push(&extra);
        }
        write!(f, "{} ({})", self.0, names.join(", "))
    }
}

// ---------------------------------------------------------------------------
// The sequences
// ---------------------------------------------------------------------------

/// How [`set`] combines its flags with the ones already in effect.
///
/// The numbers are the specification's `mode` parameter, and they are the reason
/// `set` is the wrong sequence for a well-behaved application: all three of them
/// overwrite state that belongs to somebody else. See [`push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetMode {
    /// Mode `1`: set the given bits, reset every other bit.
    Replace,
    /// Mode `2`: set the given bits, leave the rest alone.
    Enable,
    /// Mode `3`: reset the given bits, leave the rest alone.
    Disable,
}

impl SetMode {
    /// The wire value.
    #[must_use]
    pub const fn value(self) -> u8 {
        match self {
            Self::Replace => 1,
            Self::Enable => 2,
            Self::Disable => 3,
        }
    }
}

/// Pushes flags onto the terminal's stack: `CSI > <flags> u`.
///
/// **This is the sequence a well-behaved application should use**, and the reason
/// is composition. An application is rarely alone: it may have been launched from
/// a shell with its own keyboard integration, or from an editor's terminal pane,
/// each of which enabled the flags *it* needs. `set` would overwrite that state
/// with no way to discover what it was; push and pop treat it as a stack, so
/// [`pop`] restores exactly what was there without the application ever having to
/// know what that was.
///
/// The terminal keeps separate stacks for the main and the alternate screen,
/// which is what lets a full-screen program push on entry, pop on exit, and leave
/// the main screen's keyboard mode untouched throughout.
///
/// Omitting the flags is legal and means zero; this always writes them, because a
/// sequence whose meaning depends on an omitted parameter is a poor thing to
/// teach.
#[must_use]
pub fn push(flags: Flags) -> String {
    format!("{CSI}>{}u", flags.bits())
}

/// Pops one entry from the terminal's stack: `CSI < u`.
///
/// The sequence takes an optional count (`CSI < 2 u` pops two entries) which
/// defaults to `1` when omitted, and one is all a well-structured program ever
/// needs: every push gets its pop, including on the error path. A program that
/// exits leaving flags pushed hands the next program a keyboard it does not
/// expect, where Escape arrives as `CSI 27 u` and a stray release event looks
/// like input.
#[must_use]
pub fn pop() -> String {
    format!("{CSI}<u")
}

/// Sets flags absolutely: `CSI = <flags> ; <mode> u`.
///
/// Provided for completeness and for `explain`; prefer [`push`]. The three
/// [`SetMode`] values differ only in which bits they leave alone, and none of
/// them can restore what was there before.
#[must_use]
pub fn set(flags: Flags, mode: SetMode) -> String {
    format!("{CSI}={};{}u", flags.bits(), mode.value())
}

/// Asks which flags are currently in effect: `CSI ? u`.
///
/// The answer is `CSI ? <flags> u`. Unlike most of this protocol the query is
/// safe to send anywhere: a terminal that has never heard of it ignores an
/// unknown `CSI … u` sequence, which is exactly what makes silence meaningful
/// when paired with a barrier. See [`probe`].
#[must_use]
pub fn query() -> String {
    format!("{CSI}?u")
}

/// Parses the flags reply `CSI ? <flags> u`.
///
/// Returns `None` for anything that is not that reply. The private `?` prefix is
/// what keeps this out of the key decoder — [`crate::input`] routes any `CSI` with
/// a private prefix to [`Reply::Csi`], so `CSI ? 1 u` can never be mistaken for a
/// keypress with code `1`.
///
/// An empty parameter (`CSI ? u`) is read as zero: the specification allows an
/// omitted parameter to mean zero, and a terminal with no flags set has nothing
/// else it could sensibly say.
#[must_use]
pub fn parse_query_reply(reply: &Reply) -> Option<Flags> {
    let Reply::Csi { params, final_byte } = reply else {
        return None;
    };
    if *final_byte != 'u' {
        return None;
    }
    let digits = params.strip_prefix('?')?;
    if digits.is_empty() {
        return Some(Flags::NONE);
    }
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // A terminal that reports more bits than a `u8` holds is reporting something
    // this build cannot represent; that is a finding, not a value to truncate.
    digits.parse::<u8>().ok().map(Flags::from_bits)
}

/// Whether a reply is the flags reply, for [`crate::input::InputReader::wait_for_reply`].
#[must_use]
pub fn is_query_reply(reply: &Reply) -> bool {
    parse_query_reply(reply).is_some()
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Entry point for `escapade keys …`.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with — a terminal that cannot be
/// put into raw mode, or a write to a closed stdout.
pub fn run(catalog: &Catalog, command: &KeysCommand, no_color: bool) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        KeysAction::Explain { no_matrix } => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        KeysAction::Probe { format } => probe::report(&theme, *format),
        KeysAction::Watch {
            flags,
            legacy,
            format,
        } => {
            // `--legacy` is not "flags = 0 via a different spelling": it means
            // nothing is pushed at all, so the reader sees what a program that
            // never heard of the protocol sees.
            let wanted = if *legacy {
                None
            } else {
                Some(Flags::from_bits(*flags))
            };
            watch::run(&theme, wanted, *format)
        }
        KeysAction::Demo {
            force,
            only,
            step,
            list,
        } => {
            if *list {
                demo::list_steps(&theme);
                return Ok(ExitCode::SUCCESS);
            }
            demo::run(&theme, *force, only.as_deref(), *step)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn sequences_match_the_specification() {
        assert_eq!(push(Flags::DISAMBIGUATE), "\u{1b}[>1u");
        assert_eq!(push(Flags::ALL), "\u{1b}[>31u");
        assert_eq!(push(Flags::NONE), "\u{1b}[>0u");
        assert_eq!(pop(), "\u{1b}[<u");
        assert_eq!(query(), "\u{1b}[?u");
        assert_eq!(set(Flags::DISAMBIGUATE, SetMode::Replace), "\u{1b}[=1;1u");
        assert_eq!(set(Flags::EVENT_TYPES, SetMode::Enable), "\u{1b}[=2;2u");
        assert_eq!(set(Flags::EVENT_TYPES, SetMode::Disable), "\u{1b}[=2;3u");
    }

    #[test]
    fn the_five_flag_bits_are_the_documented_powers_of_two() {
        let bits: Vec<u8> = FLAGS.iter().map(|spec| spec.bit).collect();
        assert_eq!(bits, vec![1, 2, 4, 8, 16]);
        assert_eq!(Flags::ALL.bits(), 31);
        // Every constant lines up with its table entry.
        assert_eq!(Flags::DISAMBIGUATE.bits(), FLAGS[0].bit);
        assert_eq!(Flags::EVENT_TYPES.bits(), FLAGS[1].bit);
        assert_eq!(Flags::ALTERNATE_KEYS.bits(), FLAGS[2].bit);
        assert_eq!(Flags::ALL_AS_ESCAPE_CODES.bits(), FLAGS[3].bit);
        assert_eq!(Flags::ASSOCIATED_TEXT.bits(), FLAGS[4].bit);
    }

    #[test]
    fn every_flag_has_a_name_and_a_description() {
        for spec in FLAGS {
            assert!(!spec.name.is_empty(), "flag {} has no name", spec.bit);
            assert!(!spec.summary.is_empty(), "flag {} has no summary", spec.bit);
            assert!(
                spec.detail.len() > spec.summary.len(),
                "flag {} should say more in `detail` than in `summary`",
                spec.bit
            );
            assert!(spec.bit.is_power_of_two());
        }
        // Names are distinct, so a reader can tell which one a report means.
        let mut names: Vec<&str> = FLAGS.iter().map(|spec| spec.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn decoding_a_bitmask_names_exactly_the_flags_that_are_set() {
        assert_eq!(Flags::NONE.names(), Vec::<&str>::new());
        assert_eq!(
            Flags::from_bits(0b0011).names(),
            vec!["disambiguate escape codes", "report event types"]
        );
        assert_eq!(Flags::from_bits(0b1_0000).names(), vec!["report associated text"]);
        assert_eq!(Flags::ALL.names().len(), 5);
        // Flags are listed lowest bit first, whatever order the bits arrived in.
        let names = Flags::from_bits(0b1_0101).names();
        assert_eq!(
            names,
            vec![
                "disambiguate escape codes",
                "report alternate keys",
                "report associated text"
            ]
        );
    }

    #[test]
    fn containment_and_emptiness() {
        let two = Flags::DISAMBIGUATE.union(Flags::EVENT_TYPES);
        assert!(two.contains(Flags::DISAMBIGUATE));
        assert!(two.contains(Flags::EVENT_TYPES));
        assert!(!two.contains(Flags::ALTERNATE_KEYS));
        // The empty set is contained in everything, including itself.
        assert!(two.contains(Flags::NONE));
        assert!(Flags::NONE.contains(Flags::NONE));
        assert!(Flags::NONE.is_empty());
        assert!(!two.is_empty());
    }

    #[test]
    fn bits_beyond_the_specification_are_reported_not_swallowed() {
        // A terminal answering 63 knows a sixth flag this build does not. That
        // is news about the terminal; masking it away would hide it.
        let future = Flags::from_bits(0b11_1111);
        assert_eq!(future.unknown_bits(), 0b10_0000);
        assert_eq!(future.names().len(), 5);
        assert!(future.to_string().contains("unknown bits"));
        assert_eq!(Flags::ALL.unknown_bits(), 0);
    }

    #[test]
    fn display_is_readable_for_a_report() {
        assert_eq!(Flags::NONE.to_string(), "0 (none — legacy encoding)");
        assert_eq!(
            Flags::from_bits(3).to_string(),
            "3 (disambiguate escape codes, report event types)"
        );
    }

    #[test]
    fn the_query_reply_parses() {
        let reply = |params: &str| Reply::Csi {
            params: params.to_owned(),
            final_byte: 'u',
        };
        assert_eq!(parse_query_reply(&reply("?1")), Some(Flags::DISAMBIGUATE));
        assert_eq!(parse_query_reply(&reply("?31")), Some(Flags::ALL));
        // A terminal with nothing enabled answers zero, or omits the parameter.
        assert_eq!(parse_query_reply(&reply("?0")), Some(Flags::NONE));
        assert_eq!(parse_query_reply(&reply("?")), Some(Flags::NONE));
    }

    #[test]
    fn only_the_flags_reply_is_the_flags_reply() {
        // Primary Device Attributes: same private prefix, different final byte.
        assert!(parse_query_reply(&Reply::Csi {
            params: "?62;4".to_owned(),
            final_byte: 'c',
        })
        .is_none());
        // A key event has no private prefix, so it never reaches this parser as
        // a reply at all — but be explicit about it anyway.
        assert!(parse_query_reply(&Reply::Csi {
            params: "13;2".to_owned(),
            final_byte: 'u',
        })
        .is_none());
        // Junk in the parameter is not a flag set.
        assert!(parse_query_reply(&Reply::Csi {
            params: "?1;2".to_owned(),
            final_byte: 'u',
        })
        .is_none());
        // A reply too large for the five defined bits to fit in a `u8` is not
        // silently truncated to something plausible.
        assert!(parse_query_reply(&Reply::Csi {
            params: "?9999".to_owned(),
            final_byte: 'u',
        })
        .is_none());
        assert!(parse_query_reply(&Reply::Apc("25a1;s".to_owned())).is_none());
        assert!(is_query_reply(&Reply::Csi {
            params: "?5".to_owned(),
            final_byte: 'u',
        }));
    }

    #[test]
    fn the_parser_this_feature_documents_agrees_with_the_spec_on_shift_enter() {
        // The claim the whole feature rests on: with flag 1 enabled, Shift+Enter
        // and Enter are different byte sequences, and `crate::input` decodes both.
        use crate::input::{Event, KeyCode, KeyEventKind, Modifiers, ParseOutcome, parse_event};

        let expect_key = |bytes: &[u8]| match parse_event(bytes) {
            ParseOutcome::Event {
                event: Event::Key(key),
                ..
            } => key,
            other => panic!("expected a key from {bytes:?}, got {other:?}"),
        };

        let plain = expect_key(b"\r");
        let shifted = expect_key(b"\x1b[13;2u");
        assert_eq!(plain.code, KeyCode::Enter);
        assert_eq!(shifted.code, KeyCode::Enter);
        assert!(!plain.mods.shift, "legacy Enter carries no modifier");
        assert!(shifted.mods.shift, "the protocol form carries Shift");
        assert_ne!(plain.mods, shifted.mods);
        assert_eq!(shifted.kind, KeyEventKind::Press);
        assert_eq!(plain.mods, Modifiers::NONE);
    }
}
