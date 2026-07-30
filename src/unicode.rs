//! Unicode facts, and the codepoint-range syntax the CLI accepts.
//!
//! Several features need the same things: what a codepoint is called, whether it
//! is a character at all, how wide it renders, and where one user-perceived
//! character ends and the next begins. Rather than each subcommand reaching for
//! a different crate, everything goes through here.
//!
//! # Why this is a module and not direct dependency use
//!
//! The crates behind this are deliberately narrow — [`unicode_names2`] for the
//! Name property, [`unicode_properties`] for general category and emoji status,
//! [`unicode_segmentation`] for grapheme and word breaking, [`unicode_width`]
//! for East Asian width. Together they are a few hundred kilobytes of tables.
//! Swapping in something heavier and more complete (ICU4X, if line breaking,
//! bidi or normalization ever matter) is then a change to one file rather than
//! to every caller.
//!
//! The Unicode version the tables describe is [`UNICODE_VERSION`].

use std::fmt;

use anyhow::{Result, bail};
// The category traits are used through qualified paths at their call sites, so
// only the one whose methods are called directly needs importing.
use unicode_properties::{GeneralCategory, GeneralCategoryGroup, UnicodeGeneralCategory as _};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// The Unicode version the bundled tables describe, as `major.minor.patch`.
#[must_use]
pub fn unicode_version() -> String {
    let (major, minor, patch) = unicode_properties::UNICODE_VERSION;
    format!("{major}.{minor}.{patch}")
}

/// Highest valid Unicode codepoint.
pub const MAX_CODEPOINT: u32 = 0x10_FFFF;

/// How many codepoints a single specification may expand to.
///
/// Each one costs a query and a reply, and the Glyph Protocol glossary holds
/// 1024 registrations, so a request past this is more likely a typo — an
/// accidental `U+0-U+10FFFF` — than an intention.
pub const MAX_SPEC_CODEPOINTS: usize = 1024;

// ---------------------------------------------------------------------------
// Codepoint specifications
// ---------------------------------------------------------------------------

/// Parses the codepoint syntax the CLI accepts.
///
/// A specification is a comma-separated list of items, each either a single
/// codepoint or an inclusive range:
///
/// ```text
/// U+1F60A                              one codepoint
/// U+1F60A-U+1F60C                      a range
/// U+1F60A,U+1F60C-U+1F60F,U+1F611      any combination
/// ```
///
/// A codepoint may be written `U+1F60A`, `0x1F60A` or bare `1F60A`, in any case.
/// A range's end may drop the prefix: `U+1F60A-1F60C` works. Whitespace around
/// items and dashes is ignored, and `..` is accepted in place of `-`.
///
/// As a convenience, an item that is not valid hex but *is* a single character is
/// taken literally, so `escapade glyph supports 😊` does what it looks
/// like. Hex always wins where both readings are possible: `A` is U+0041 either
/// way, but `ABC` is U+0ABC rather than three letters.
///
/// Order is preserved and duplicates are dropped, so the output reads back in the
/// order it was asked for. Use [`parse_sequence`] when repeats are meaningful.
///
/// # Errors
///
/// Fails on an empty specification, an item that is neither hex nor a single
/// character, a codepoint above [`MAX_CODEPOINT`], a range whose end precedes its
/// start, or an expansion larger than [`MAX_SPEC_CODEPOINTS`].
pub fn parse_spec(spec: &str) -> Result<Vec<u32>> {
    parse(spec, Repeats::Drop)
}

/// Parses the same syntax as [`parse_spec`], but keeps every codepoint exactly as
/// written — order and repeats included.
///
/// This is the right function whenever the codepoints are *text* rather than a
/// set of things to look up. A ZWJ emoji sequence such as
/// `U+1F468,U+200D,U+1F469,U+200D,U+1F467` repeats U+200D, and de-duplicating it
/// would silently turn a family into something else entirely.
///
/// # Errors
///
/// As [`parse_spec`].
#[allow(dead_code, reason = "used by the grapheme feature, which renders text")]
pub fn parse_sequence(spec: &str) -> Result<Vec<u32>> {
    parse(spec, Repeats::Keep)
}

/// Whether a duplicate codepoint is meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repeats {
    /// Keep every codepoint as written: the input is text.
    Keep,
    /// Collapse duplicates: the input is a set of things to look up.
    Drop,
}

/// Shared implementation of the codepoint specification syntax.
fn parse(spec: &str, repeats: Repeats) -> Result<Vec<u32>> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        bail!("empty codepoint specification");
    }

    let mut codepoints: Vec<u32> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for item in trimmed.split(',') {
        let item = item.trim();
        if item.is_empty() {
            bail!("empty item in `{spec}` — check for a stray comma");
        }

        let (start, end) = if let Some((low, high)) = split_range(item) {
            (parse_codepoint(low)?, parse_codepoint(high)?)
        } else {
            let single = parse_codepoint(item)?;
            (single, single)
        };

        if end < start {
            bail!("range `{item}` ends (U+{end:04X}) before it starts (U+{start:04X})");
        }
        let span = usize::try_from(end - start).unwrap_or(usize::MAX).saturating_add(1);
        if codepoints.len().saturating_add(span) > MAX_SPEC_CODEPOINTS {
            bail!(
                "`{spec}` covers more than {MAX_SPEC_CODEPOINTS} codepoints — \
                 narrow the range, or split it across several runs"
            );
        }

        for codepoint in start..=end {
            if repeats == Repeats::Keep || seen.insert(codepoint) {
                codepoints.push(codepoint);
            }
        }
    }

    Ok(codepoints)
}

/// Splits a range item into its two halves, on `-` or `..`.
///
/// The separator is looked for after the first character, so a leading `-` is not
/// mistaken for one. Hex never contains `-`, so there is no ambiguity beyond
/// that.
fn split_range(item: &str) -> Option<(&str, &str)> {
    if let Some(index) = item.find("..").filter(|&index| index > 0) {
        return Some((&item[..index], &item[index + 2..]));
    }
    let dash = item.char_indices().skip(1).find(|&(_, ch)| ch == '-')?;
    Some((&item[..dash.0], &item[dash.0 + 1..]))
}

/// Parses one codepoint: `U+1F60A`, `0x1F60A`, `1F60A`, or a literal character.
fn parse_codepoint(text: &str) -> Result<u32> {
    let text = text.trim();
    if text.is_empty() {
        bail!("empty codepoint");
    }

    let hex = text
        .strip_prefix("U+")
        .or_else(|| text.strip_prefix("u+"))
        .or_else(|| text.strip_prefix("0x"))
        .or_else(|| text.strip_prefix("0X"));

    // An explicit prefix means it must be hex; a bare item may fall back to
    // being read as a literal character.
    let explicit = hex.is_some();
    let digits = hex.unwrap_or(text);

    match u32::from_str_radix(digits, 16) {
        Ok(codepoint) if codepoint <= MAX_CODEPOINT => Ok(codepoint),
        Ok(codepoint) => bail!(
            "U+{codepoint:X} is above the highest Unicode codepoint U+{MAX_CODEPOINT:X}"
        ),
        Err(_) if explicit => bail!("`{text}` is not a hexadecimal codepoint"),
        Err(_) => {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => Ok(u32::from(ch)),
                _ => bail!(
                    "`{text}` is neither a hexadecimal codepoint nor a single character"
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Describing a codepoint
// ---------------------------------------------------------------------------

/// What a codepoint is, beyond its number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharInfo {
    /// The codepoint itself.
    pub codepoint: u32,
    /// The character, when the codepoint is a valid scalar value. Surrogates
    /// (U+D800–U+DFFF) are codepoints but not characters, so this is `None` for
    /// them.
    pub character: Option<char>,
    /// The Unicode Name property, or a synthesized description for the ranges
    /// that have no name.
    pub name: String,
    /// Whether [`CharInfo::name`] is the real Name property rather than
    /// something this module made up.
    pub named: bool,
    /// General category, when the codepoint is a character.
    pub category: Option<GeneralCategory>,
    /// Whether it is recommended for use as emoji (`Emoji=Yes`).
    pub emoji: bool,
    /// Columns it occupies when printed, by East Asian Width. Zero for combining
    /// marks, two for wide characters.
    pub width: usize,
}

impl CharInfo {
    /// `U+1F60A`-style label.
    #[must_use]
    pub fn label(&self) -> String {
        format!("U+{:04X}", self.codepoint)
    }

    /// The name in title case, which reads better in a list than the shouted
    /// original: `Smiling Face with Smiling Eyes`.
    #[must_use]
    pub fn title_name(&self) -> String {
        title_case(&self.name)
    }

    /// Whether printing this would disturb the display — controls, surrogates
    /// and unassigned codepoints.
    #[must_use]
    pub fn printable(&self) -> bool {
        let Some(character) = self.character else {
            return false;
        };
        if character.is_control() {
            return false;
        }
        !matches!(
            self.category,
            Some(GeneralCategory::Unassigned | GeneralCategory::Control | GeneralCategory::Format)
        )
    }

    /// Whether this codepoint is in a Private Use Area, where the Glyph Protocol
    /// is allowed to register outlines.
    #[must_use]
    pub fn private_use(&self) -> bool {
        matches!(self.category, Some(GeneralCategory::PrivateUse))
    }
}

impl fmt::Display for CharInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.label(), self.title_name())
    }
}

/// Looks up everything known about a codepoint.
#[must_use]
pub fn describe(codepoint: u32) -> CharInfo {
    let character = char::from_u32(codepoint);
    let category = character.map(unicode_properties::UnicodeGeneralCategory::general_category);
    let (name, named) = match character.and_then(unicode_names2::name) {
        Some(name) => (name.to_string(), true),
        None => (synthesized_name(codepoint, category), false),
    };

    CharInfo {
        codepoint,
        character,
        name,
        named,
        category,
        emoji: character.is_some_and(unicode_properties::UnicodeEmoji::is_emoji_char),
        width: character
            .and_then(unicode_width::UnicodeWidthChar::width)
            .unwrap_or(0),
    }
}

/// A description for the codepoints Unicode gives no Name property.
///
/// Control characters, private use areas, surrogates and unassigned codepoints
/// all have an empty `na` field; the standard describes them by their block
/// instead, and that is far more useful in a listing than an empty column.
fn synthesized_name(codepoint: u32, category: Option<GeneralCategory>) -> String {
    match category {
        Some(GeneralCategory::Control) => "<control>".to_owned(),
        Some(GeneralCategory::PrivateUse) => match codepoint {
            0xE000..=0xF8FF => "<private use>".to_owned(),
            0xF_0000..=0xF_FFFD => "<private use, plane 15>".to_owned(),
            _ => "<private use, plane 16>".to_owned(),
        },
        Some(GeneralCategory::Surrogate) => "<surrogate>".to_owned(),
        _ if (0xD800..=0xDFFF).contains(&codepoint) => "<surrogate>".to_owned(),
        _ => "<unassigned>".to_owned(),
    }
}

/// Converts a shouted Unicode name to title case.
///
/// Unicode names are all uppercase. Lowercasing the small function words is what
/// makes `LATIN SMALL LETTER A WITH ACUTE` read as
/// `Latin Small Letter a with Acute` rather than as a headline. Tokens that are
/// clearly not words — hex suffixes, single letters, bracketed placeholders —
/// keep their original form.
#[must_use]
pub fn title_case(name: &str) -> String {
    /// Words that stay lowercase unless they lead.
    ///
    /// Deliberately excludes the articles "a" and "an": a bare single letter in a
    /// Unicode name is the letter being *named*, as in
    /// `LATIN SMALL LETTER A WITH ACUTE`, and lowercasing it loses that.
    const MINOR: &[&str] = &[
        "with", "and", "or", "of", "the", "in", "for", "to", "above", "below", "over",
    ];

    if name.starts_with('<') {
        return name.to_owned();
    }

    let mut out = String::with_capacity(name.len());
    for (index, word) in name.split(' ').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let leading = index == 0;
        // Checked before the minor-word list so a single letter always survives
        // as written, whatever it happens to spell. Only single letters are
        // short-circuited here; hyphenated and digit-bearing tokens still go
        // through `title_case_word`, which handles them part by part.
        if !leading && word.chars().count() == 1 {
            out.push_str(word);
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if !leading && MINOR.contains(&lower.as_str()) {
            out.push_str(&lower);
        } else {
            out.push_str(&title_case_word(word, leading));
        }
    }
    out
}

/// Title-cases one whitespace-delimited token, respecting hyphenated parts.
fn title_case_word(word: &str, leading: bool) -> String {
    word.split_inclusive('-')
        .map(|part| {
            let (body, separator) = match part.strip_suffix('-') {
                Some(body) => (body, "-"),
                None => (part, ""),
            };
            let cased = if keep_as_is(body, leading) {
                body.to_owned()
            } else {
                let mut chars = body.chars();
                match chars.next() {
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                    }
                    None => String::new(),
                }
            };
            cased + separator
        })
        .collect()
}

/// Whether a token should be left exactly as Unicode spells it.
///
/// A single letter is a letter being named, not a word to capitalize
/// (`LETTER A`), and anything with a digit in it is an identifier such as the
/// `4E00` in `CJK UNIFIED IDEOGRAPH-4E00`.
fn keep_as_is(body: &str, leading: bool) -> bool {
    /// Acronyms that appear in Unicode names and read wrong in title case —
    /// `Cjk Unified Ideograph` rather than `CJK Unified Ideograph`. Unicode names
    /// are entirely uppercase, so there is no way to detect these
    /// programmatically; the list simply grows as cases turn up.
    const ACRONYMS: &[&str] = &["CJK", "ASCII", "IPA", "OCR", "NKO", "TV", "SIP", "APL"];

    if body.chars().any(char::is_numeric) {
        return true;
    }
    if ACRONYMS.contains(&body) {
        return true;
    }
    !leading && body.chars().count() == 1
}

// ---------------------------------------------------------------------------
// Segmentation and width
// ---------------------------------------------------------------------------

// The segmentation helpers below have no caller yet. They are the reason this
// module is a module rather than three scattered dependency calls: cursor
// movement, selection and width-correct truncation all need grapheme clusters,
// and the next feature that renders text will reach for them. They are covered
// by tests, so they cannot rot silently.

/// Splits text into grapheme clusters — what a user calls "one character".
///
/// A flag, a family emoji and an `e` with a combining acute are each one cluster
/// and several codepoints. Anything that moves a cursor or counts characters for
/// display has to work in these, not in `char`s.
#[must_use]
pub fn graphemes(text: &str) -> Vec<&str> {
    text.graphemes(true).collect()
}

/// Number of grapheme clusters in `text`.
#[must_use]
#[allow(dead_code, reason = "published surface of this module; see note above")]
pub fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

/// Splits text into words, by the Unicode word-boundary rules.
#[must_use]
#[allow(dead_code, reason = "published surface of this module; see note above")]
pub fn words(text: &str) -> Vec<&str> {
    text.unicode_words().collect()
}

/// Columns `text` occupies in a terminal, by East Asian Width.
///
/// This is an estimate by construction: it is what the standard recommends, not
/// what any particular terminal and font actually do, and emoji sequences are
/// where the two most often disagree.
#[must_use]
pub fn display_width(text: &str) -> usize {
    text.width()
}

/// Columns one character occupies, treating unknowns as zero.
#[must_use]
pub fn char_width(character: char) -> usize {
    character.width().unwrap_or(0)
}

/// The broad class of a character, as a short human label.
#[must_use]
pub fn category_group_name(character: char) -> &'static str {
    match character.general_category_group() {
        GeneralCategoryGroup::Letter => "letter",
        GeneralCategoryGroup::Mark => "mark",
        GeneralCategoryGroup::Number => "number",
        GeneralCategoryGroup::Punctuation => "punctuation",
        GeneralCategoryGroup::Symbol => "symbol",
        GeneralCategoryGroup::Separator => "separator",
        GeneralCategoryGroup::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_single_codepoint_in_every_accepted_spelling() {
        for spelling in ["U+1F60A", "u+1f60a", "0x1F60A", "1F60A", "1f60a"] {
            assert_eq!(parse_spec(spelling).unwrap(), vec![0x1_F60A], "{spelling}");
        }
    }

    #[test]
    fn parses_the_range_from_the_brief() {
        assert_eq!(
            parse_spec("U+1F60A-U+1F60C").unwrap(),
            vec![0x1_F60A, 0x1_F60B, 0x1_F60C]
        );
    }

    #[test]
    fn parses_the_combination_from_the_brief() {
        assert_eq!(
            parse_spec("U+1F60A,U+1F60C-U+1F60F,U+1F611").unwrap(),
            vec![
                0x1_F60A, 0x1_F60C, 0x1_F60D, 0x1_F60E, 0x1_F60F, 0x1_F611
            ]
        );
    }

    #[test]
    fn accepts_dot_dot_and_a_bare_range_end() {
        assert_eq!(parse_spec("U+41..U+43").unwrap(), vec![0x41, 0x42, 0x43]);
        assert_eq!(parse_spec("U+41-43").unwrap(), vec![0x41, 0x42, 0x43]);
    }

    #[test]
    fn tolerates_whitespace() {
        assert_eq!(
            parse_spec("  U+41 , U+43 - U+44  ").unwrap(),
            vec![0x41, 0x43, 0x44]
        );
    }

    #[test]
    fn a_single_codepoint_range_is_just_that_codepoint() {
        assert_eq!(parse_spec("U+41-U+41").unwrap(), vec![0x41]);
    }

    #[test]
    fn duplicates_collapse_but_order_is_kept() {
        assert_eq!(
            parse_spec("U+43,U+41,U+43,U+42").unwrap(),
            vec![0x43, 0x41, 0x42],
            "the listing should read back in the order it was asked for"
        );
    }

    #[test]
    fn parse_sequence_keeps_repeats_because_they_are_text() {
        // A family emoji repeats the zero-width joiner. Dropping the repeat
        // would change the sequence into a different one.
        let family = parse_sequence("U+1F468,U+200D,U+1F469,U+200D,U+1F467").unwrap();
        assert_eq!(
            family,
            vec![0x1_F468, 0x200D, 0x1_F469, 0x200D, 0x1_F467]
        );
        let text: String = family.iter().filter_map(|&cp| char::from_u32(cp)).collect();
        assert_eq!(grapheme_count(&text), 1, "still one grapheme cluster");

        // The de-duplicating variant would have broken it.
        assert_eq!(
            parse_spec("U+1F468,U+200D,U+1F469,U+200D,U+1F467")
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn both_parsers_accept_the_same_syntax() {
        for spec in ["U+41", "U+41-U+43", "U+41,U+50-U+52", "😊"] {
            assert_eq!(
                parse_spec(spec).unwrap(),
                parse_sequence(spec).unwrap(),
                "{spec} has no repeats, so the two must agree"
            );
        }
    }

    #[test]
    fn falls_back_to_a_literal_character_when_hex_cannot_apply() {
        assert_eq!(parse_spec("😊").unwrap(), vec![0x1_F60A]);
        assert_eq!(parse_spec("→").unwrap(), vec![0x2192]);
        // Hex wins wherever both readings are possible.
        assert_eq!(parse_spec("ABC").unwrap(), vec![0x0ABC]);
    }

    #[test]
    fn rejects_nonsense() {
        for bad in [
            "",
            "   ",
            "U+41,,U+42",
            "U+ZZZZ",
            "0xNOPE",
            "U+110000",
            "U+43-U+41",
            "hello",
        ] {
            assert!(parse_spec(bad).is_err(), "should have rejected {bad:?}");
        }
    }

    #[test]
    fn rejects_an_expansion_that_would_flood_the_terminal() {
        let error = parse_spec("U+0-U+10FFFF").unwrap_err().to_string();
        assert!(error.contains(&MAX_SPEC_CODEPOINTS.to_string()), "{error}");
        // Right at the limit is fine.
        assert_eq!(
            parse_spec(&format!("U+0-U+{:X}", MAX_SPEC_CODEPOINTS - 1))
                .unwrap()
                .len(),
            MAX_SPEC_CODEPOINTS
        );
    }

    #[test]
    fn the_limit_counts_across_items_not_per_item() {
        let half = MAX_SPEC_CODEPOINTS / 2;
        let spec = format!("U+0-U+{:X},U+2000-U+{:X}", half, 0x2000 + half);
        assert!(parse_spec(&spec).is_err());
    }

    #[test]
    fn describes_a_named_character() {
        let info = describe(0x1_F60A);
        assert_eq!(info.label(), "U+1F60A");
        assert_eq!(info.name, "SMILING FACE WITH SMILING EYES");
        assert!(info.named);
        assert_eq!(info.title_name(), "Smiling Face with Smiling Eyes");
        assert_eq!(info.character, Some('😊'));
        assert!(info.emoji);
        assert!(info.printable());
        assert_eq!(info.width, 2, "emoji are double width");
    }

    #[test]
    fn describes_a_private_use_codepoint() {
        let info = describe(0xE0A0);
        assert!(!info.named, "the PUA has no Name property");
        assert_eq!(info.name, "<private use>");
        assert!(info.private_use());
        assert_eq!(info.character, Some('\u{e0a0}'));
    }

    #[test]
    fn describes_a_surrogate_which_is_not_a_character() {
        let info = describe(0xD800);
        assert_eq!(info.character, None);
        assert_eq!(info.name, "<surrogate>");
        assert!(!info.printable());
    }

    #[test]
    fn describes_a_control_character() {
        let info = describe(0x1B);
        assert_eq!(info.name, "<control>");
        assert!(
            !info.printable(),
            "printing ESC would execute whatever followed it"
        );
    }

    #[test]
    fn describes_an_unassigned_codepoint_without_panicking() {
        let info = describe(0x10_FFFF);
        assert!(!info.printable());
        assert!(!info.name.is_empty());
    }

    #[test]
    fn title_case_lowercases_function_words_but_never_the_first() {
        assert_eq!(
            title_case("SMILING FACE WITH SMILING EYES"),
            "Smiling Face with Smiling Eyes"
        );
        assert_eq!(title_case("WITH ACUTE"), "With Acute");
        assert_eq!(
            title_case("LATIN SMALL LETTER A WITH ACUTE"),
            "Latin Small Letter A with Acute",
            "the A is the letter being named, not the article"
        );
        assert_eq!(title_case("MODIFIER LETTER SMALL H"), "Modifier Letter Small H");
    }

    #[test]
    fn title_case_leaves_identifiers_and_placeholders_alone() {
        assert_eq!(title_case("<private use>"), "<private use>");
        assert_eq!(
            title_case("CJK UNIFIED IDEOGRAPH-4E00"),
            "CJK Unified Ideograph-4E00",
            "the acronym stays shouted and the hex suffix survives intact"
        );
    }

    #[test]
    fn title_case_handles_hyphenated_words() {
        assert_eq!(title_case("HYPHEN-MINUS"), "Hyphen-Minus");
    }

    #[test]
    fn graphemes_group_combining_sequences() {
        assert_eq!(graphemes("e\u{301}"), vec!["e\u{301}"]);
        assert_eq!(grapheme_count("e\u{301}"), 1, "one perceived character");
        assert_eq!("e\u{301}".chars().count(), 2, "but two codepoints");
    }

    #[test]
    fn graphemes_keep_emoji_sequences_whole() {
        // A ZWJ family sequence is one cluster made of many codepoints.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(grapheme_count(family), 1);
        assert!(family.chars().count() > 1);
        // Regional indicator pairs make one flag.
        assert_eq!(grapheme_count("\u{1F1F8}\u{1F1EA}"), 1);
    }

    #[test]
    fn words_splits_on_unicode_boundaries() {
        assert_eq!(words("hello, world!"), vec!["hello", "world"]);
    }

    #[test]
    fn widths_match_what_a_terminal_reserves() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('漢'), 2);
        assert_eq!(char_width('\u{301}'), 0, "combining marks take no columns");
        assert_eq!(display_width("ab漢"), 4);
    }

    #[test]
    fn category_groups_have_readable_names() {
        assert_eq!(category_group_name('a'), "letter");
        assert_eq!(category_group_name('1'), "number");
        assert_eq!(category_group_name('.'), "punctuation");
        assert_eq!(category_group_name('+'), "symbol");
        assert_eq!(category_group_name(' '), "separator");
    }

    #[test]
    fn unicode_version_is_reported() {
        let version = unicode_version();
        assert!(
            version.split('.').count() == 3,
            "expected major.minor.patch, got {version}"
        );
    }
}
