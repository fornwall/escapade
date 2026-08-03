//! Putting a name and a meaning on one decoded item.
//!
//! Three sources are consulted, in this order:
//!
//! 1. **[`crate::term::pretty`]**, for the introducers. Spelling `CSI ? 1049 h`
//!    rather than `^[[?1049h` is what makes the output greppable and safe, and the
//!    function that names them is the same one every `--dry-run` in the project
//!    already uses.
//! 2. **The feature database**, through [`super::catalog`]. `OSC 8` is the
//!    hyperlinks feature, `APC G` is kitty graphics, `APC 25a1` is the Glyph
//!    Protocol and `CSI ? 2027` is grapheme clustering — and none of that is
//!    written down here, so all of it improves when a data file does.
//! 3. **[`super::names`] and [`super::sgr`]**, for the grammar the database does
//!    not cover: `SGR`, cursor movement, erasing, the mode registries, the C0
//!    controls and the `OSC` numbers whose feature files are still placeholders.
//!
//! Where a parameter changes what a sequence *does*, the description says so —
//! `CSI 1;31 m` is "bold, foreground red" and not "set graphic rendition" — because
//! naming the sequence class is the part a reader can already do.

use super::{
    catalog::{Index, Match, apc_key},
    display::{self, HEX_LIMIT, PAYLOAD_LIMIT},
    names,
    parse::{Decoded, Fields, Flaw, Item, Pending, StringKind, Terminator},
    sgr,
};
use crate::term;

/// Everything worth saying about one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    /// Short kind, matching [`Item::kind`]: `csi`, `osc`, `text`, `malformed`.
    pub kind: &'static str,
    /// The sequence, spelled symbolically and safe to print.
    pub sequence: String,
    /// Specification mnemonic or sequence class: `SGR`, `DECSET`, `OSC 8`.
    pub label: Option<String>,
    /// What it does, in one phrase.
    pub summary: Option<String>,
    /// What the feature database says about it.
    pub features: Vec<Match>,
    /// What is wrong with it, when something is.
    pub problem: Option<String>,
    /// Anything else worth flagging: a non-standard terminator, a payload that is
    /// not text, invisible characters in a run of text.
    pub notes: Vec<String>,
}

impl Description {
    /// The description as one line of prose, without the sequence itself.
    ///
    /// This is what the text output puts in its second column, and what makes a
    /// decode diffable: the same bytes always produce the same sentence.
    #[must_use]
    pub fn prose(&self) -> String {
        let mut out = String::new();
        if let Some(problem) = &self.problem {
            out.push_str(problem);
        }
        match (&self.label, &self.summary) {
            (Some(label), Some(summary)) => {
                if !out.is_empty() {
                    out.push_str(" — ");
                }
                out.push_str(label);
                out.push_str(" — ");
                out.push_str(summary);
            }
            (Some(label), None) => {
                if !out.is_empty() {
                    out.push_str(" — ");
                }
                out.push_str(label);
            }
            (None, Some(summary)) => {
                if !out.is_empty() {
                    out.push_str(" — ");
                }
                out.push_str(summary);
            }
            (None, None) => {}
        }
        if !self.features.is_empty() {
            let names: Vec<&str> = self
                .features
                .iter()
                .map(|found| found.feature_name.as_str())
                .collect();
            if !out.is_empty() {
                out.push(' ');
            }
            out.push('[');
            out.push_str(&names.join(", "));
            out.push(']');
        }
        for note in &self.notes {
            if !out.is_empty() {
                out.push_str("; ");
            }
            out.push_str(note);
        }
        out
    }
}

/// Describes one decoded item, consulting the database for attribution.
#[must_use]
pub fn describe(decoded: &Decoded, index: &Index) -> Description {
    let mut description = match &decoded.item {
        Item::Text(text) => describe_text(text),
        Item::Control(byte) => describe_control(*byte),
        Item::Csi(fields) => describe_csi(fields),
        Item::Dcs {
            fields,
            payload,
            terminator,
        } => describe_dcs(fields, payload, *terminator),
        Item::StringSeq {
            kind,
            payload,
            terminator,
        } => describe_string(*kind, payload, *terminator),
        Item::SingleShift { level, byte } => describe_single_shift(*level, *byte),
        Item::Esc {
            intermediates,
            final_byte,
        } => describe_esc(intermediates, *final_byte),
        Item::Unterminated { pending } => describe_unterminated(*pending, &decoded.bytes),
        Item::Malformed { flaw } => describe_malformed(*flaw, &decoded.bytes),
    };

    // The class name comes from the parser, so the two cannot disagree.
    description.kind = decoded.item.kind();
    description.features = index.lookup(&decoded.item);
    // The database's own words are better than a generic phrase, so they fill in
    // wherever the tables had nothing to say.
    if description.summary.is_none()
        && let Some(found) = description.features.first()
    {
        description.summary = Some(phrase(&found.sequence_name));
    }
    description
}

/// A [`Description`] with the fields most items leave alone.
///
/// `kind` is filled in by [`describe`] from [`Item::kind`], so no describer has to
/// remember to name its own class.
fn base(sequence: String) -> Description {
    Description {
        kind: "",
        sequence,
        label: None,
        summary: None,
        features: Vec::new(),
        problem: None,
        notes: Vec::new(),
    }
}

/// A run of printable text.
///
/// Ordinary text needs no explanation, so it gets none; the exception is text
/// carrying characters that do not show themselves, which is the whole reason
/// `decode` is also an auditing tool. Those are counted and called out, and
/// [`display::escape`] has already spelled them where they occur.
fn describe_text(text: &str) -> Description {
    let mut description = base(display::quote(text));
    let hidden = text.chars().filter(|&ch| !display::showable(ch)).count();
    if hidden > 0 {
        description.notes.push(format!(
            "{hidden} character{} in this run {} invisible when printed",
            if hidden == 1 { "" } else { "s" },
            if hidden == 1 { "is" } else { "are" }
        ));
    }
    description
}

/// A C0 control byte, or `DEL`.
fn describe_control(byte: u8) -> Description {
    let meaning = names::control(byte);
    let mut description = base(meaning.mnemonic.to_owned());
    description.summary = Some(meaning.summary);
    description
}

/// `CSI …`, including `SGR`.
fn describe_csi(fields: &Fields) -> Description {
    let mut pieces = Vec::new();
    if let Some(private) = fields.private {
        pieces.push(char::from(private).to_string());
    }
    pieces.push(fields.raw.clone());
    pieces.push(fields.intermediates_text());
    pieces.push(fields.final_char().to_string());

    let mut description = base(display::spell(term::CSI, &pieces));
    if fields.private.is_none() && fields.final_byte == b'm' {
        description.label = Some("SGR".to_owned());
        description.summary = Some(sgr::summary(&fields.params));
        return description;
    }
    match names::csi(fields) {
        Some(meaning) => {
            description.label = Some(if meaning.mnemonic.is_empty() {
                "CSI".to_owned()
            } else {
                meaning.mnemonic.to_owned()
            });
            description.summary = Some(meaning.summary);
        }
        None => description.label = Some("CSI".to_owned()),
    }
    description
}

/// `DCS <fields> <payload> ST`.
fn describe_dcs(fields: &Fields, payload: &[u8], terminator: Terminator) -> Description {
    let text = display::recover(payload);
    let pieces = vec![
        fields.raw.clone(),
        fields.intermediates_text(),
        fields.final_char().to_string(),
        display::shorten(&text, PAYLOAD_LIMIT),
        terminator.bytes().to_owned(),
    ];
    let mut description = base(display::spell(term::DCS, &pieces));
    match names::dcs(fields, &text) {
        Some(meaning) => {
            description.label = Some(if meaning.mnemonic.is_empty() {
                "DCS".to_owned()
            } else {
                meaning.mnemonic.to_owned()
            });
            description.summary = Some(meaning.summary);
        }
        None => description.label = Some("DCS".to_owned()),
    }
    payload_notes(&mut description, StringKind::Dcs, payload, terminator);
    description
}

/// `OSC`, `APC`, `PM` or `SOS`.
fn describe_string(kind: StringKind, payload: &[u8], terminator: Terminator) -> Description {
    let text = display::recover(payload);
    let pieces = vec![
        display::shorten(&text, PAYLOAD_LIMIT),
        terminator.bytes().to_owned(),
    ];
    let introducer = format!("{}{}", term::ESC, char::from(kind.opener()));
    let mut description = base(display::spell(&introducer, &pieces));

    match kind {
        StringKind::Osc => {
            let fields: Vec<&str> = text.split(';').collect();
            let number = fields.first().copied().unwrap_or_default();
            description.label = Some(
                if number.chars().all(|c| c.is_ascii_digit()) && !number.is_empty() {
                    format!("OSC {number}")
                } else {
                    "OSC".to_owned()
                },
            );
            if let Some(meaning) = names::osc(&fields) {
                description.summary = Some(meaning.summary);
            }
        }
        StringKind::Apc => {
            // The same rule the database is matched by, so the label and the
            // lookup key cannot drift apart.
            let key = apc_key(&text);
            description.label = Some(if key.is_empty() {
                "APC".to_owned()
            } else {
                format!("APC {key}")
            });
            if let Some(meaning) = names::apc(&text) {
                description.summary = Some(meaning.summary);
            }
        }
        StringKind::Pm => {
            description.label = Some("PM".to_owned());
            description.summary =
                Some("a privacy message, which terminals are expected to ignore".to_owned());
        }
        StringKind::Sos => {
            description.label = Some("SOS".to_owned());
            description.summary =
                Some("a character string, which terminals are expected to ignore".to_owned());
        }
        StringKind::Dcs => description.label = Some("DCS".to_owned()),
    }
    payload_notes(&mut description, kind, payload, terminator);
    description
}

/// Flags a payload that is not text, one that is long enough to have been elided,
/// and a terminator that only `OSC` allows.
fn payload_notes(
    description: &mut Description,
    kind: StringKind,
    payload: &[u8],
    terminator: Terminator,
) {
    if terminator == Terminator::Bel && !kind.allows_bel() {
        description.notes.push(format!(
            "ended with BEL, which only OSC blesses — a strict {} parser wants ST",
            kind.name()
        ));
    }
    if !display::bytes_are_text(payload) {
        description
            .notes
            .push("the payload is not valid UTF-8".to_owned());
    }
    if payload.len() > PAYLOAD_LIMIT {
        description
            .notes
            .push(format!("{} bytes of payload", payload.len()));
    }
}

/// `ESC N x` and `ESC O x`.
fn describe_single_shift(level: u8, byte: u8) -> Description {
    let introducer = if level == 2 { "N" } else { "O" };
    let shifted = char::from(byte);
    let pieces = vec![introducer.to_owned(), display::escape(&shifted.to_string())];
    let mut description = base(display::spell(term::ESC, &pieces));
    description.label = Some(format!("SS{level}"));
    description.summary = Some(format!(
        "take the next character from the G{level} character set: {}",
        display::quote_short(&shifted.to_string(), 4)
    ));
    description
}

/// `ESC <intermediates> <final>`.
fn describe_esc(intermediates: &[u8], final_byte: u8) -> Description {
    let mut pieces: Vec<String> = intermediates
        .iter()
        .map(|&byte| char::from(byte).to_string())
        .collect();
    pieces.push(char::from(final_byte).to_string());
    let mut description = base(display::spell(term::ESC, &pieces));
    match names::escape_sequence(intermediates, final_byte) {
        Some(meaning) => {
            description.label = Some(if meaning.mnemonic.is_empty() {
                "ESC".to_owned()
            } else {
                meaning.mnemonic.to_owned()
            });
            description.summary = Some(meaning.summary);
        }
        None => description.label = Some("ESC".to_owned()),
    }
    description
}

/// A sequence the input ended in the middle of.
///
/// Reported as a finding, not swept up: a truncated capture is normal input, and
/// "this OSC never terminated" is exactly the bug someone runs `decode` to find.
fn describe_unterminated(pending: Pending, bytes: &[u8]) -> Description {
    let mut description = base(spell_raw(bytes));
    // No label: the problem below already opens with what went wrong, and
    // "unterminated — unterminated OSC" reads like a stutter.
    description.problem = Some(match pending {
        Pending::Esc => {
            "an ESC with nothing after it: the input ended before the sequence did".to_owned()
        }
        Pending::Csi => {
            "unterminated CSI: no final byte arrived before the end of the input".to_owned()
        }
        Pending::SingleShift(level) => {
            format!("unterminated SS{level}: no character followed it")
        }
        Pending::Payload(kind) => format!(
            "unterminated {}: the payload ran to the end of the input with no {}",
            kind.name(),
            if kind.allows_bel() { "ST or BEL" } else { "ST" }
        ),
    });
    description
}

/// Bytes that form no sequence.
fn describe_malformed(flaw: Flaw, bytes: &[u8]) -> Description {
    let mut description = base(spell_raw(bytes));
    // As above: the problem is the description.
    description.problem = Some(match flaw {
        Flaw::CsiAbandoned => "a byte that cannot appear inside a CSI arrived before the final \
                               byte, so the sequence was abandoned"
            .to_owned(),
        Flaw::CsiOutOfOrder => {
            "parameter bytes after an intermediate byte, which no sequence allows".to_owned()
        }
        Flaw::StringAbandoned(kind) => format!(
            "{} abandoned: a control byte or a fresh ESC arrived before the terminator",
            kind.name()
        ),
        Flaw::StrayEsc => "an ESC followed by a byte that begins no sequence".to_owned(),
        Flaw::InvalidUtf8 => "not valid UTF-8".to_owned(),
        Flaw::EightBitC1(byte) => {
            let name = names::eight_bit_c1(byte);
            if name.is_empty() {
                format!(
                    "byte {byte:#04x}: not valid UTF-8, and a C1 control in an eight-bit stream"
                )
            } else {
                format!(
                    "byte {byte:#04x}: not valid UTF-8 here, though it is the eight-bit form of \
                     {name} in a Latin-1 stream — which it is cannot be told from the bytes"
                )
            }
        }
    });
    description
}

/// Spells raw bytes for display, naming whatever introducers they contain.
///
/// Used for the items that are not a well-formed sequence, where there is nothing
/// to reconstruct from and the bytes themselves are the evidence.
fn spell_raw(bytes: &[u8]) -> String {
    let text = display::recover(bytes);
    display::shorten(&term::pretty(&text), PAYLOAD_LIMIT + HEX_LIMIT)
}

/// Turns a data file's sequence name into a phrase that reads inside a sentence.
///
/// The database capitalises names the way a heading is capitalised — `Open a
/// link`, `Support` — which reads wrong after a dash. An acronym is left alone, so
/// `DECRQM` does not become `dECRQM`.
fn phrase(name: &str) -> String {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    let first_word = name.split_whitespace().next().unwrap_or(name);
    let is_acronym = first_word.len() > 1
        && first_word
            .chars()
            .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit());
    if is_acronym {
        return name.to_owned();
    }
    first.to_lowercase().collect::<String>() + characters.as_str()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{super::parse::decode_all, *};
    use crate::db::Catalog;

    /// Descriptions of every item in a stream, using the real database.
    fn describe_all(bytes: &[u8]) -> Vec<Description> {
        let catalog = Catalog::load().unwrap();
        let index = Index::from_catalog(&catalog);
        decode_all(bytes)
            .iter()
            .map(|decoded| describe(decoded, &index))
            .collect()
    }

    /// A feature's display name, straight from the database.
    fn feature_name(id: &str) -> String {
        Catalog::load().unwrap().feature(id).unwrap().name.clone()
    }

    /// The one description of a one-item stream.
    fn one(bytes: &[u8]) -> Description {
        let mut all = describe_all(bytes);
        assert_eq!(all.len(), 1, "{all:?}");
        all.remove(0)
    }

    /// Sequence and prose, as the text output pairs them.
    fn line(bytes: &[u8]) -> String {
        let description = one(bytes);
        format!("{}\t{}", description.sequence, description.prose())
    }

    #[test]
    fn the_example_from_the_brief_reads_as_the_brief_writes_it() {
        // The feature name is read from the database rather than spelled here,
        // so editing a data file's prose cannot fail this test for the wrong
        // reason.
        let attributes = feature_name("text-attributes");
        let described = describe_all(b"\x1b[1;31mred\x1b[0m");
        assert_eq!(described[0].sequence, "CSI 1;31 m");
        assert_eq!(
            described[0].prose(),
            format!("SGR — bold, foreground red [{attributes}]")
        );
        assert_eq!(described[1].sequence, "\"red\"");
        assert_eq!(described[1].prose(), "");
        assert_eq!(described[2].sequence, "CSI 0 m");
        assert_eq!(
            described[2].prose(),
            format!("SGR — reset all attributes [{attributes}]")
        );
    }

    #[test]
    fn the_alternate_screen_reads_as_the_brief_asks() {
        assert_eq!(
            line(b"\x1b[?1049h"),
            "CSI ? 1049 h\tDECSET — enter the alternate screen"
        );
    }

    #[test]
    fn a_hyperlink_is_attributed_to_the_hyperlinks_feature() {
        let described = one(b"\x1b]8;;https://example.com\x1b\\");
        assert_eq!(described.sequence, "OSC 8;;https://example.com ST");
        assert_eq!(described.label.as_deref(), Some("OSC 8"));
        assert!(
            described.prose().contains("start a hyperlink"),
            "{}",
            described.prose()
        );
        assert!(
            described.prose().contains("Hyperlinks"),
            "the feature should be named: {}",
            described.prose()
        );
    }

    #[test]
    fn the_whole_captured_hyperlink_stream_decodes_and_names_itself() {
        let described = describe_all(b"\x1b]8;;https://example.com\x1b\\click me\x1b]8;;\x1b\\");
        assert_eq!(described.len(), 3);
        assert!(described[0].prose().contains("start a hyperlink"));
        assert_eq!(described[1].sequence, "\"click me\"");
        assert_eq!(described[2].sequence, "OSC 8;; ST");
        assert!(described[2].prose().contains("end the current hyperlink"));
        assert_eq!(described[2].features[0].feature_id, "hyperlinks");
    }

    #[test]
    fn the_two_apc_protocols_are_named_from_the_database() {
        let kitty = one(b"\x1b_Ga=T,f=100,i=42;AAAA\x1b\\");
        assert_eq!(kitty.label.as_deref(), Some("APC G"));
        assert_eq!(kitty.features[0].feature_id, "kitty-graphics");
        assert!(kitty.prose().contains("transmit"), "{}", kitty.prose());

        let glyph = one(b"\x1b_25a1;s\x1b\\");
        assert_eq!(glyph.label.as_deref(), Some("APC 25a1"));
        assert_eq!(glyph.features[0].feature_id, "glyph-protocol");
        assert!(
            glyph.prose().contains("Glyph Protocol"),
            "{}",
            glyph.prose()
        );
    }

    #[test]
    fn a_database_match_and_a_table_meaning_are_both_shown() {
        // Mode 2027 is described by the table *and* claimed by a feature file;
        // neither should displace the other.
        let described = one(b"\x1b[?2027h");
        assert_eq!(described.label.as_deref(), Some("DECSET"));
        assert!(described.prose().contains("grapheme clustering"));
        assert!(described.prose().contains("Grapheme clustering"));
    }

    #[test]
    fn controls_are_named_and_never_printed_as_themselves() {
        let described = describe_all(b"\x07\n");
        assert_eq!(described[0].sequence, "BEL");
        assert!(described[0].prose().contains("ring the bell"));
        assert_eq!(described[1].sequence, "LF");
    }

    #[test]
    fn an_unterminated_osc_says_so() {
        let described = one(b"\x1b]8;;broken");
        assert_eq!(described.kind, "unterminated");
        assert_eq!(described.sequence, "OSC 8;;broken");
        assert!(
            described.prose().contains("unterminated OSC"),
            "{}",
            described.prose()
        );
        assert!(described.prose().contains("no ST or BEL"));
    }

    #[test]
    fn an_unterminated_apc_asks_only_for_st() {
        let described = one(b"\x1b_Gf=100");
        assert!(described.prose().contains("unterminated APC"));
        assert!(
            !described.prose().contains("BEL"),
            "BEL is not an APC terminator: {}",
            described.prose()
        );
    }

    #[test]
    fn a_bel_terminated_apc_is_flagged_as_non_standard() {
        let described = one(b"\x1b_Gf=100\x07");
        assert!(
            described.prose().contains("only OSC blesses"),
            "{}",
            described.prose()
        );
    }

    #[test]
    fn malformed_sequences_explain_what_is_wrong() {
        let abandoned = describe_all(b"\x1b]8;;x\x1b[0m");
        assert_eq!(abandoned[0].kind, "malformed");
        assert!(abandoned[0].prose().contains("OSC abandoned"));
        let stray = describe_all(b"\x1b\x1b[0m");
        assert!(stray[0].prose().contains("begins no sequence"));
    }

    #[test]
    fn an_eight_bit_csi_byte_is_explained_rather_than_obeyed() {
        let described = describe_all(b"\x9b1m");
        assert!(
            described[0].prose().contains("CSI"),
            "{}",
            described[0].prose()
        );
        assert!(described[0].prose().contains("Latin-1"));
    }

    #[test]
    fn invisible_characters_in_text_are_counted_and_spelled_out() {
        let described = one("a\u{202e}b".as_bytes());
        assert_eq!(described.sequence, "\"a\\u{202e}b\"");
        assert!(
            described
                .prose()
                .contains("1 character in this run is invisible"),
            "{}",
            described.prose()
        );
    }

    #[test]
    fn a_long_payload_is_elided_and_counted() {
        let payload = "A".repeat(500);
        let stream = format!("\u{1b}_G{payload}\u{1b}\\");
        let described = one(stream.as_bytes());
        assert!(described.sequence.contains('…'), "{}", described.sequence);
        assert!(described.sequence.len() < 200, "{}", described.sequence);
        assert!(
            described.prose().contains("501 bytes of payload"),
            "{}",
            described.prose()
        );
    }

    #[test]
    fn sixel_and_decrqss_are_told_apart() {
        let sixel = one(b"\x1bP0;1;0q#0;2;0;0;0\x1b\\");
        assert!(sixel.prose().contains("sixel"), "{}", sixel.prose());
        let decrqss = one(b"\x1bP$qm\x1b\\");
        assert_eq!(decrqss.label.as_deref(), Some("DECRQSS"));
    }

    #[test]
    fn single_shifts_and_two_byte_escapes_are_named() {
        assert_eq!(
            line(b"\x1bNx"),
            "ESC N x\tSS2 — take the next character from the G2 character set: \"x\""
        );
        assert!(line(b"\x1b(0").contains("line-drawing set"));
        assert_eq!(line(b"\x1bc").split('\t').next(), Some("ESC c"));
        assert!(line(b"\x1bc").contains("RIS"));
    }

    #[test]
    fn a_shared_sequence_names_every_feature_that_claims_it() {
        let described = one(b"\x1b[c");
        assert_eq!(described.label.as_deref(), Some("DA1"));
        assert!(
            described.features.len() > 1,
            "the barrier is shared: {:?}",
            described.features
        );
        assert!(described.prose().contains(", "), "{}", described.prose());
    }

    #[test]
    fn nothing_a_description_prints_contains_a_control_byte() {
        // The invariant the whole subcommand rests on.
        let nasty = b"\x1b[1;31m\x1b]8;;https://example.com\x1b\\\x07\x1b_G\x00payload\x1b\\\
                      \x1b]0;a\x1bb\x1bP$qm\x1b\\\x9b\xff\x1b[?1049h";
        for described in describe_all(nasty) {
            for text in [described.sequence.clone(), described.prose()] {
                assert!(
                    !text.chars().any(char::is_control),
                    "control byte in {text:?}"
                );
            }
        }
    }

    #[test]
    fn database_names_are_lowercased_but_acronyms_are_not() {
        assert_eq!(phrase("Open a link"), "open a link");
        assert_eq!(
            phrase("set determinate progress"),
            "set determinate progress"
        );
        assert_eq!(phrase("DECRQM sets the mode"), "DECRQM sets the mode");
        assert_eq!(phrase(""), "");
    }
}
