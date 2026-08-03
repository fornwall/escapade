//! Glyph Protocol: build the sequences, parse the replies.
//!
//! Every message is an APC string framed as
//!
//! ```text
//! ESC _ 25a1 ; <verb> [ ; key=value ]* [ ; payload ] ESC \
//! ```
//!
//! The `25a1` prefix is the hex codepoint of U+25A1 WHITE SQUARE — tofu, the
//! empty box a terminal draws when nothing in the font covers a codepoint, and
//! the thing this protocol exists to prevent.
//!
//! Because APC is unknown to terminals that do not implement it, they drop the
//! entire string silently. That is what makes it safe to speak the protocol
//! optimistically: the worst case is nothing happens.
//!
//! Specification: <https://rapha.land/introducing-glyph-protocol-for-terminals/>

pub mod art;
pub mod demo;
pub mod glyf;
pub mod probe;
pub mod supports;

use std::process::ExitCode;

use anyhow::Result;

use crate::{
    cli::{FeatureCommand, GlyphAction},
    db::Catalog,
    term::{APC, Multiplexer, ST, Term, wrapped},
    theme::Theme,
};

/// The feature id in the database, and the subcommand name.
pub const FEATURE_ID: &str = "glyph-protocol";

/// Protocol identifier: the hex codepoint of U+25A1 WHITE SQUARE.
pub const PREFIX: &str = "25a1";

/// The tofu character the protocol is named after.
pub const TOFU: char = '\u{25a1}';

/// Default units-per-em coordinate space for outlines.
pub const DEFAULT_UPM: u16 = 1000;

/// Maximum simultaneous registrations per session, after which the terminal
/// evicts the oldest in FIFO order.
pub const GLOSSARY_CAPACITY: usize = 1024;

/// The three Unicode Private Use Areas, the only places a glyph may be
/// registered. Anything else is refused with `reason=out_of_namespace`.
pub const PUA_RANGES: [(u32, u32); 3] = [
    (0xE000, 0xF8FF),
    (0xF_0000, 0xF_FFFD),
    (0x10_0000, 0x10_FFFD),
];

/// Whether `codepoint` may be registered.
#[must_use]
pub fn is_private_use(codepoint: u32) -> bool {
    PUA_RANGES
        .iter()
        .any(|&(low, high)| (low..=high).contains(&codepoint))
}

/// A payload format, as advertised by the `s` reply and selected by `fmt=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// TrueType simple glyph outlines. Required in v1.
    Glyf,
    /// Layered flat-color glyphs (OpenType COLR v0). Added in v1.2.
    Colrv0,
    /// Full paint graph: gradients, transforms, compositing (COLR v1).
    Colrv1,
}

impl Format {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Glyf => "glyf",
            Self::Colrv0 => "colrv0",
            Self::Colrv1 => "colrv1",
        }
    }

    /// Parses a wire name, returning `None` for formats we do not know.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "glyf" => Some(Self::Glyf),
            "colrv0" => Some(Self::Colrv0),
            "colrv1" => Some(Self::Colrv1),
            _ => None,
        }
    }
}

/// How much the terminal should say in response to a registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplyLevel {
    /// `reply=1`: acknowledge both success and failure. The default.
    #[default]
    Both,
    /// `reply=2`: only speak up on failure.
    FailureOnly,
    /// `reply=0`: say nothing. Right for bulk registration at startup.
    Silent,
}

impl ReplyLevel {
    /// The wire value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Both => "1",
            Self::FailureOnly => "2",
            Self::Silent => "0",
        }
    }
}

/// Which sources can render a codepoint, from a `q` reply's `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Coverage {
    /// The user's font covers it.
    pub system: bool,
    /// A registration in this session covers it, shadowing any font.
    pub glossary: bool,
}

impl Coverage {
    /// Parses the comma-separated `status` value.
    #[must_use]
    pub fn parse(status: &str) -> Self {
        let mut coverage = Self::default();
        for source in status.split(',').map(str::trim) {
            match source {
                "system" => coverage.system = true,
                "glossary" => coverage.glossary = true,
                _ => {}
            }
        }
        coverage
    }

    /// Whether anything at all would render, as opposed to tofu.
    #[must_use]
    pub fn renders(self) -> bool {
        self.system || self.glossary
    }

    /// Human description.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match (self.system, self.glossary) {
            (true, true) => "the system font and this session's glossary (the glossary wins)",
            (true, false) => "the system font",
            (false, true) => "this session's glossary",
            (false, false) => "nothing — this would render as tofu",
        }
    }
}

/// A parsed reply from the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlyphReply {
    /// Answer to `s`: the protocol is supported, with these payload formats.
    Support {
        /// Formats we recognise.
        formats: Vec<Format>,
        /// Format names the terminal advertised that we do not know about.
        unknown: Vec<String>,
    },
    /// Answer to `q`: what covers this codepoint.
    Query {
        /// The codepoint asked about.
        codepoint: u32,
        /// What can render it.
        coverage: Coverage,
    },
    /// Answer to `r`: whether the registration took.
    Register {
        /// The codepoint registered.
        codepoint: u32,
        /// Whether it succeeded.
        ok: bool,
        /// Failure code, when it did not.
        reason: Option<String>,
    },
    /// Answer to `c`.
    Clear {
        /// The codepoint cleared, or `None` for the whole glossary.
        codepoint: Option<u32>,
        /// Whether it succeeded.
        ok: bool,
    },
}

// -- building sequences ------------------------------------------------------

/// `APC 25a1 ; s ST` — does this terminal speak the protocol?
#[must_use]
pub fn support_query() -> String {
    format!("{APC}{PREFIX};s{ST}")
}

/// `APC 25a1 ; q ; cp=… ST` — can this codepoint be rendered, and by what?
#[must_use]
pub fn codepoint_query(codepoint: u32) -> String {
    format!("{APC}{PREFIX};q;cp={codepoint:X}{ST}")
}

/// `APC 25a1 ; r ; cp=… ; upm=… ; <base64> ST` — install an outline.
///
/// The payload must already be base64; [`glyf::Outline::encode_base64`] produces it.
#[must_use]
pub fn register(
    codepoint: u32,
    format: Format,
    upm: u16,
    reply: ReplyLevel,
    payload_base64: &str,
) -> String {
    use std::fmt::Write as _;

    let mut sequence = format!("{APC}{PREFIX};r;cp={codepoint:X}");
    // Writing into a `String` cannot fail, so each `Result` is discarded.
    // `glyf` is the default, so leaving it off keeps the common case shorter.
    if format != Format::Glyf {
        let _ = write!(sequence, ";fmt={}", format.as_str());
    }
    if upm != DEFAULT_UPM {
        let _ = write!(sequence, ";upm={upm}");
    }
    if reply != ReplyLevel::Both {
        let _ = write!(sequence, ";reply={}", reply.as_str());
    }
    sequence.push(';');
    sequence.push_str(payload_base64);
    sequence.push_str(ST);
    sequence
}

/// Like [`register`], but always spells out `upm` and `fmt`.
///
/// The demo shows the sequences it sends, and an explicit `upm=1000` teaches
/// more than an omitted default.
#[must_use]
pub fn register_verbose(
    codepoint: u32,
    format: Format,
    upm: u16,
    reply: ReplyLevel,
    payload_base64: &str,
) -> String {
    format!(
        "{APC}{PREFIX};r;cp={codepoint:X};fmt={};upm={upm};reply={};{payload_base64}{ST}",
        format.as_str(),
        reply.as_str()
    )
}

/// `APC 25a1 ; c [ ; cp=… ] ST` — drop one registration, or all of them.
#[must_use]
pub fn clear(codepoint: Option<u32>) -> String {
    match codepoint {
        Some(codepoint) => format!("{APC}{PREFIX};c;cp={codepoint:X}{ST}"),
        None => format!("{APC}{PREFIX};c{ST}"),
    }
}

// -- parsing replies --------------------------------------------------------

/// Parses an APC payload — the bytes between `ESC _` and the terminator.
///
/// Returns `None` when the payload belongs to some other APC protocol, which is
/// entirely normal: anything not starting with `25a1;` is none of our business.
#[must_use]
pub fn parse_reply(payload: &str) -> Option<GlyphReply> {
    let mut fields = payload.split(';');
    if fields.next()? != PREFIX {
        return None;
    }
    let verb = fields.next()?;

    let mut params: Vec<(&str, &str)> = Vec::new();
    for field in fields {
        if let Some((key, value)) = field.split_once('=') {
            params.push((key, value));
        }
    }
    let get = |key: &str| {
        params
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| *value)
    };
    let codepoint = || get("cp").and_then(|hex| u32::from_str_radix(hex, 16).ok());
    let succeeded = || get("status") == Some("0");

    match verb {
        "s" => {
            let mut formats = Vec::new();
            let mut unknown = Vec::new();
            for name in get("fmt").unwrap_or("").split(',').map(str::trim) {
                if name.is_empty() {
                    continue;
                }
                match Format::parse(name) {
                    Some(format) => formats.push(format),
                    None => unknown.push(name.to_owned()),
                }
            }
            Some(GlyphReply::Support { formats, unknown })
        }
        "q" => Some(GlyphReply::Query {
            codepoint: codepoint()?,
            coverage: Coverage::parse(get("status").unwrap_or("")),
        }),
        "r" => Some(GlyphReply::Register {
            codepoint: codepoint()?,
            ok: succeeded(),
            reason: get("reason").map(str::to_owned),
        }),
        "c" => Some(GlyphReply::Clear {
            codepoint: codepoint(),
            ok: succeeded(),
        }),
        _ => None,
    }
}

/// Sends a protocol sequence, wrapping it for a multiplexer if one is in the
/// way, and flushes.
///
/// # Errors
///
/// Fails if stdout does.
pub fn send(term: &mut Term, multiplexer: Option<Multiplexer>, sequence: &str) -> Result<()> {
    term.write_now(&wrapped(multiplexer, sequence))
}

/// Sends a *query*, which must leave the screen exactly as it found it.
///
/// Same wrapping as [`send`], with [`crate::term::shielded`] outside it: a
/// terminal that does not implement the protocol is supposed to ignore an APC
/// string wholesale, and the ones that instead print the body — macOS
/// Terminal.app consumes only the `ESC _` — would otherwise write `25a1;s` at the
/// prompt for every question asked. `escapade glyph probe` asks one per
/// codepoint, so the leak is a screenful rather than a line.
///
/// [`send`] is deliberately left alone: registrations and clears are for
/// terminals that answered the query, and the demo owns the screen it draws on,
/// so restoring the cursor and erasing a line there would fight the caller.
///
/// # Errors
///
/// Fails if stdout does.
pub fn send_query(term: &mut Term, multiplexer: Option<Multiplexer>, query: &str) -> Result<()> {
    term.write_query(&wrapped(multiplexer, query))
}

/// Entry point for `escapade glyph …`.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with.
pub fn run(
    catalog: &Catalog,
    command: &FeatureCommand<GlyphAction>,
    no_color: bool,
) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    let feature = catalog.feature(FEATURE_ID)?;
    match &command.action {
        GlyphAction::Explain { no_matrix } => {
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
        }
        // One verb, two questions: bare `probe` asks about the protocol, `probe
        // <CODEPOINTS>` asks which of those the terminal can actually draw.
        GlyphAction::Probe {
            target,
            format,
            long,
        } => {
            return match target {
                Some(spec) => supports::report(&theme, spec, *long, *format),
                None => probe::report(&theme, *format),
            };
        }
        GlyphAction::Demo {
            force,
            only,
            step,
            list,
        } => {
            if *list {
                demo::list_steps(&theme);
            } else {
                demo::run(&theme, *force, only.as_deref(), *step)?;
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_use_areas_are_the_only_legal_targets() {
        assert!(is_private_use(0xE0A0), "powerline branch icon");
        assert!(is_private_use(0xE000), "start of the basic PUA");
        assert!(is_private_use(0xF8FF), "end of the basic PUA");
        assert!(is_private_use(0xF_0000), "supplementary plane A");
        assert!(is_private_use(0x10_FFFD), "supplementary plane B");

        assert!(!is_private_use(u32::from('A')), "real text is off limits");
        assert!(!is_private_use(0xDFFF), "just below the basic PUA");
        assert!(!is_private_use(0xF900), "just above the basic PUA");
        assert!(!is_private_use(0x10_FFFE), "past the last legal codepoint");
    }

    #[test]
    fn support_query_matches_the_spec() {
        assert_eq!(support_query(), "\u{1b}_25a1;s\u{1b}\\");
    }

    #[test]
    fn codepoint_query_uses_uppercase_hex_without_a_prefix() {
        assert_eq!(codepoint_query(0xE0A0), "\u{1b}_25a1;q;cp=E0A0\u{1b}\\");
    }

    #[test]
    fn register_omits_parameters_that_match_their_defaults() {
        let sequence = register(0xE0A0, Format::Glyf, DEFAULT_UPM, ReplyLevel::Both, "AAEC");
        assert_eq!(sequence, "\u{1b}_25a1;r;cp=E0A0;AAEC\u{1b}\\");
    }

    #[test]
    fn register_spells_out_non_default_parameters() {
        let sequence = register(0xE0A0, Format::Colrv0, 512, ReplyLevel::Silent, "AAEC");
        assert_eq!(
            sequence,
            "\u{1b}_25a1;r;cp=E0A0;fmt=colrv0;upm=512;reply=0;AAEC\u{1b}\\"
        );
    }

    #[test]
    fn clear_targets_one_codepoint_or_everything() {
        assert_eq!(clear(Some(0xE0A0)), "\u{1b}_25a1;c;cp=E0A0\u{1b}\\");
        assert_eq!(clear(None), "\u{1b}_25a1;c\u{1b}\\");
    }

    #[test]
    fn parses_the_support_reply_from_the_spec() {
        let reply = parse_reply("25a1;s;fmt=glyf").unwrap();
        assert_eq!(
            reply,
            GlyphReply::Support {
                formats: vec![Format::Glyf],
                unknown: Vec::new(),
            }
        );
    }

    #[test]
    fn support_reply_keeps_formats_it_does_not_recognise() {
        let reply = parse_reply("25a1;s;fmt=glyf,colrv1,glyf3000").unwrap();
        assert_eq!(
            reply,
            GlyphReply::Support {
                formats: vec![Format::Glyf, Format::Colrv1],
                unknown: vec!["glyf3000".to_owned()],
            }
        );
    }

    #[test]
    fn parses_query_coverage() {
        let reply = parse_reply("25a1;q;cp=E0A0;status=system").unwrap();
        assert_eq!(
            reply,
            GlyphReply::Query {
                codepoint: 0xE0A0,
                coverage: Coverage {
                    system: true,
                    glossary: false
                },
            }
        );

        let both = parse_reply("25a1;q;cp=E0A0;status=system,glossary").unwrap();
        let GlyphReply::Query { coverage, .. } = both else {
            panic!("expected a query reply");
        };
        assert!(coverage.system && coverage.glossary);
        assert!(coverage.renders());
    }

    #[test]
    fn empty_query_status_means_tofu() {
        let reply = parse_reply("25a1;q;cp=E0A0;status=").unwrap();
        let GlyphReply::Query { coverage, .. } = reply else {
            panic!("expected a query reply");
        };
        assert!(!coverage.renders());
        assert!(coverage.describe().contains("tofu"));
    }

    #[test]
    fn parses_registration_success_and_failure() {
        assert_eq!(
            parse_reply("25a1;r;cp=E0A0;status=0").unwrap(),
            GlyphReply::Register {
                codepoint: 0xE0A0,
                ok: true,
                reason: None,
            }
        );
        assert_eq!(
            parse_reply("25a1;r;cp=0041;status=1;reason=out_of_namespace").unwrap(),
            GlyphReply::Register {
                codepoint: 0x41,
                ok: false,
                reason: Some("out_of_namespace".to_owned()),
            }
        );
    }

    #[test]
    fn ignores_apc_payloads_belonging_to_other_protocols() {
        assert!(parse_reply("G,i=1;OK").is_none(), "kitty graphics");
        assert!(parse_reply("").is_none());
        assert!(parse_reply("25a1").is_none(), "no verb");
        assert!(parse_reply("25a1;zzz").is_none(), "unknown verb");
    }

    #[test]
    fn reply_levels_use_the_documented_wire_values() {
        assert_eq!(ReplyLevel::default(), ReplyLevel::Both);
        assert_eq!(ReplyLevel::Both.as_str(), "1");
        assert_eq!(ReplyLevel::FailureOnly.as_str(), "2");
        assert_eq!(ReplyLevel::Silent.as_str(), "0");
    }
}
