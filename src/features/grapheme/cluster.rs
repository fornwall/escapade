//! Turning a codepoint list into grapheme clusters, and into the two cell counts
//! that disagree about them.
//!
//! Nothing here talks to a terminal. That is deliberate: the interesting claim
//! this subcommand makes is "the terminal used *n* cells and it should have used
//! *m*", and *m* has to be computable and testable without one.
//!
//! Two numbers are worked out for every cluster:
//!
//! * **expected** — [`unicode::display_width`] over the whole cluster. This is
//!   UAX #29 segmentation plus the emoji-presentation rules, which is what mode
//!   2027 asks the terminal to agree with.
//! * **legacy** — the per-codepoint `wcwidth` widths added up, which is what a
//!   terminal without mode 2027 does.
//!
//! They differ in both directions, which is why both are worth printing. A ZWJ
//! sequence overcounts (🧑‍🌾 is 2 clustered, 4 unclustered); a variation-selector
//! sequence undercounts (❤️ is U+2764 U+FE0F — 2 clustered, 1 unclustered,
//! because VS16 promotes the cluster to emoji presentation).

use unicode_properties::GeneralCategory;

use crate::unicode::{self, CharInfo};

/// Placeholder printed instead of a codepoint that must not reach the terminal.
pub const PLACEHOLDER: &str = "·";

/// U+25CC DOTTED CIRCLE — the base Unicode itself uses to show a combining mark
/// in isolation, so the mark cannot latch onto neighbouring text in a report.
pub const DOTTED_CIRCLE: char = '\u{25cc}';

/// One grapheme cluster, with the two widths that disagree about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cluster {
    /// The cluster's text, exactly as it would be written to the terminal.
    pub text: String,
    /// Its codepoints, in order, repeats kept.
    pub codepoints: Vec<CharInfo>,
    /// Cells it should occupy: UAX #29 plus emoji presentation, which is what a
    /// terminal in mode 2027 is being asked to agree with.
    pub expected: usize,
    /// Cells a `wcwidth` terminal gives it: the per-codepoint widths added up.
    pub legacy: usize,
}

impl Cluster {
    /// Whether counting per codepoint gets this cluster wrong.
    ///
    /// Only clusters where this is true can prove anything about a terminal: if
    /// the two rules agree, both a clustering and a non-clustering terminal
    /// produce the same number and the measurement decides nothing.
    #[must_use]
    pub fn miscounted(&self) -> bool {
        self.legacy != self.expected
    }

    /// How the two counts differ, as a short phrase for a report.
    #[must_use]
    pub fn miscount_direction(&self) -> Option<&'static str> {
        match self.legacy.cmp(&self.expected) {
            std::cmp::Ordering::Greater => Some("wcwidth overcounts"),
            std::cmp::Ordering::Less => Some("wcwidth undercounts"),
            std::cmp::Ordering::Equal => None,
        }
    }
}

/// Splits `text` into grapheme clusters and works out both widths for each.
#[must_use]
pub fn analyze(text: &str) -> Vec<Cluster> {
    unicode::graphemes(text)
        .into_iter()
        .map(|cluster| Cluster {
            text: cluster.to_owned(),
            codepoints: cluster
                .chars()
                .map(|ch| unicode::describe(ch.into()))
                .collect(),
            expected: unicode::display_width(cluster),
            legacy: legacy_width(cluster),
        })
        .collect()
}

/// The width a `wcwidth` terminal gives `text`: every codepoint measured on its
/// own, summed, with no segmentation anywhere.
#[must_use]
pub fn legacy_width(text: &str) -> usize {
    text.chars().map(unicode::char_width).sum()
}

/// A codepoint list turned into text, or refused because part of it is not safe
/// to write to a terminal.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Assembly {
    /// Every codepoint asked for, in order, repeats kept.
    pub codepoints: Vec<CharInfo>,
    /// The text, when every codepoint was safe to send.
    pub text: Option<String>,
    /// The codepoints that made it unsafe, if any.
    pub refused: Vec<CharInfo>,
}

impl Assembly {
    /// Whether the sequence can be sent to a terminal at all.
    #[must_use]
    pub fn sendable(&self) -> bool {
        self.text.is_some()
    }
}

/// Builds the text a codepoint list denotes, refusing anything unsafe to write.
///
/// The list is treated as *text*: order and repeats are preserved, because a
/// family emoji legitimately repeats U+200D and de-duplicating it would silently
/// turn a family into something else.
#[must_use]
pub fn assemble(codepoints: &[u32]) -> Assembly {
    let infos: Vec<CharInfo> = codepoints.iter().copied().map(unicode::describe).collect();
    let refused: Vec<CharInfo> = infos
        .iter()
        .filter(|info| !is_sendable(info))
        .cloned()
        .collect();

    let text = if refused.is_empty() {
        Some(
            infos
                .iter()
                .filter_map(|info| info.character)
                .collect::<String>(),
        )
    } else {
        None
    };

    Assembly {
        codepoints: infos,
        text,
        refused,
    }
}

/// Whether a codepoint may be written to the terminal as part of a sequence.
///
/// Format characters such as U+200D ZERO WIDTH JOINER and U+FE0F VARIATION
/// SELECTOR-16 **are** sendable and are the entire point of this feature — a
/// grapheme cluster is usually held together by one. What must never be sent is a
/// control character (a raw `ESC` would execute whatever bytes followed it), a
/// surrogate (not a character at all), or an unassigned codepoint whose rendering
/// nobody can predict.
#[must_use]
pub fn is_sendable(info: &CharInfo) -> bool {
    // `CharInfo::printable` is the shared guard, and it excludes the Format
    // category — right for a codepoint listing, wrong here, so Format is added
    // back explicitly rather than by loosening the shared rule.
    info.printable() || matches!(info.category, Some(GeneralCategory::Format))
}

/// The character itself, when it is safe to *print*, or [`PLACEHOLDER`].
///
/// Stricter than [`is_sendable`]: a ZWJ is fine to send inside a cluster but has
/// no visible form of its own, so a breakdown line shows the placeholder for it.
/// Controls, surrogates and unassigned codepoints are never printed — emitting a
/// raw `ESC` here would execute whatever came after it, and the whole point of
/// this tool is knowing which bytes go to the terminal.
///
/// A combining mark on its own is shown on [`DOTTED_CIRCLE`], the convention the
/// Unicode standard itself uses. Printing it bare would silently attach it to
/// whatever character happened to precede it in the report — a listing that
/// changes the text around it is not a listing.
#[must_use]
pub fn rendered(info: &CharInfo) -> String {
    let Some(character) = info.character else {
        return PLACEHOLDER.to_owned();
    };
    if !info.printable() {
        return PLACEHOLDER.to_owned();
    }
    if matches!(
        info.category,
        Some(
            GeneralCategory::NonspacingMark
                | GeneralCategory::EnclosingMark
                | GeneralCategory::SpacingMark
        )
    ) {
        return format!("{DOTTED_CIRCLE}{character}");
    }
    character.to_string()
}

/// One line of the `--debug` codepoint breakdown: `U+1F9D1  Adult  🧑`.
#[must_use]
pub fn breakdown_line(info: &CharInfo) -> String {
    let mut line = format!(
        "{}  {}  {}",
        info.label(),
        info.title_name(),
        rendered(info)
    );
    if let Some(note) = breakdown_note(info) {
        line.push_str("  ");
        line.push_str(note);
    }
    line
}

/// The role a codepoint plays in holding a cluster together, when it has one.
#[must_use]
pub fn breakdown_note(info: &CharInfo) -> Option<&'static str> {
    match info.codepoint {
        0x200D => Some("joins the codepoints on either side into one cluster"),
        0xFE0F => Some("VS16: forces emoji presentation, and width 2"),
        0xFE0E => Some("VS15: forces text presentation, without changing the width"),
        0x1_F3FB..=0x1_F3FF => Some("skin tone modifier"),
        0x1_F1E6..=0x1_F1FF => Some("regional indicator: two in a row make a flag"),
        0xE0020..=0xE007F => Some("tag character: spells out a subdivision flag"),
        _ if !is_sendable(info) => Some("not safe to send — never written to the terminal"),
        _ => None,
    }
}

/// Total cells a whole string should occupy, cluster by cluster.
///
/// Takes anything iterable so a caller with clusters embedded in report rows does
/// not have to clone them out into a slice first.
#[must_use]
pub fn expected_width<'a>(clusters: impl IntoIterator<Item = &'a Cluster>) -> usize {
    clusters.into_iter().map(|cluster| cluster.expected).sum()
}

/// Total cells a `wcwidth` terminal gives a whole string.
#[must_use]
pub fn legacy_total<'a>(clusters: impl IntoIterator<Item = &'a Cluster>) -> usize {
    clusters.into_iter().map(|cluster| cluster.legacy).sum()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// U+1F9D1 U+200D U+1F33E — the farmer from the article.
    const FARMER: &str = "\u{1f9d1}\u{200d}\u{1f33e}";
    /// U+1F468 U+200D U+1F469 U+200D U+1F467 — man, woman, girl.
    const FAMILY: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";

    #[test]
    fn a_zwj_sequence_is_one_cluster_that_wcwidth_overcounts() {
        let clusters = analyze(FARMER);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].codepoints.len(), 3);
        assert_eq!(clusters[0].expected, 2);
        assert_eq!(clusters[0].legacy, 4, "2 + 0 + 2 counted per codepoint");
        assert_eq!(clusters[0].miscount_direction(), Some("wcwidth overcounts"));
    }

    #[test]
    fn a_longer_zwj_sequence_is_still_two_cells() {
        let clusters = analyze(FAMILY);
        assert_eq!(
            clusters.len(),
            1,
            "a family is one user-perceived character"
        );
        assert_eq!(clusters[0].expected, 2);
        assert_eq!(clusters[0].legacy, 6, "three wide emoji and two joiners");
    }

    #[test]
    fn a_variation_selector_makes_wcwidth_undercount() {
        // U+2764 HEAVY BLACK HEART is narrow; U+FE0F promotes the cluster to
        // emoji presentation, which is wide. This is the opposite error from a
        // ZWJ sequence, and a tool that only knew about overcounting would miss
        // it entirely.
        let clusters = analyze("\u{2764}\u{fe0f}");
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].expected, 2);
        assert_eq!(clusters[0].legacy, 1);
        assert_eq!(
            clusters[0].miscount_direction(),
            Some("wcwidth undercounts")
        );
    }

    #[test]
    fn a_flag_is_two_regional_indicators_in_one_cluster() {
        let clusters = analyze("\u{1f1f8}\u{1f1ea}");
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].codepoints.len(), 2);
        assert_eq!(clusters[0].expected, 2);
    }

    #[test]
    fn a_combining_mark_joins_its_base() {
        let clusters = analyze("e\u{301}");
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].expected, 1);
        assert_eq!(clusters[0].legacy, 1);
        assert!(
            !clusters[0].miscounted(),
            "combining marks are already width 0, so wcwidth gets this one right"
        );
    }

    #[test]
    fn plain_text_is_one_cluster_per_character() {
        let clusters = analyze("abc");
        assert_eq!(clusters.len(), 3);
        assert!(clusters.iter().all(|cluster| cluster.expected == 1));
        assert!(clusters.iter().all(|cluster| !cluster.miscounted()));
    }

    #[test]
    fn several_clusters_are_segmented_and_summed_separately() {
        let clusters = analyze(&format!("A{FARMER}\u{4e00}"));
        assert_eq!(clusters.len(), 3);
        assert_eq!(expected_width(&clusters), 1 + 2 + 2);
        assert_eq!(legacy_total(&clusters), 1 + 4 + 2);
        assert_eq!(expected_width(clusters.iter().take(1)), 1);
    }

    #[test]
    fn an_empty_string_has_no_clusters() {
        assert!(analyze("").is_empty());
        assert_eq!(expected_width(&Vec::new()), 0);
        assert_eq!(legacy_total(&Vec::new()), 0);
    }

    #[test]
    fn assembling_keeps_order_and_repeats() {
        // The family emoji repeats U+200D. Losing one of them would change the
        // sequence into something else entirely.
        let codepoints = [0x1_F468, 0x200D, 0x1_F469, 0x200D, 0x1_F467];
        let assembly = assemble(&codepoints);
        assert_eq!(assembly.codepoints.len(), 5);
        assert_eq!(assembly.text.as_deref(), Some(FAMILY));
        assert!(assembly.refused.is_empty());
        assert!(assembly.sendable());
    }

    #[test]
    fn joiners_and_variation_selectors_are_sendable() {
        for codepoint in [0x200D, 0xFE0F, 0xFE0E, 0x1_F3FF, 0x0301] {
            let info = unicode::describe(codepoint);
            assert!(
                is_sendable(&info),
                "U+{codepoint:04X} holds clusters together and must be sendable"
            );
        }
    }

    #[test]
    fn a_control_character_is_refused_rather_than_written() {
        // U+001B would run whatever bytes followed it as an escape sequence.
        let assembly = assemble(&[0x41, 0x1B, 0x42]);
        assert!(assembly.text.is_none(), "must not build a string with ESC");
        assert!(!assembly.sendable());
        assert_eq!(assembly.refused.len(), 1);
        assert_eq!(assembly.refused[0].codepoint, 0x1B);
        assert_eq!(assembly.codepoints.len(), 3, "the listing still shows it");
    }

    #[test]
    fn surrogates_and_unassigned_codepoints_are_refused() {
        for codepoint in [0xD800u32, 0xDFFF, 0x0007, 0x000A, 0x0F_FFFF] {
            let assembly = assemble(&[codepoint]);
            assert!(
                assembly.text.is_none(),
                "U+{codepoint:04X} should not be written to a terminal"
            );
        }
    }

    #[test]
    fn nothing_unsafe_ever_reaches_a_rendered_string() {
        for codepoint in [0x1Bu32, 0x07, 0x0A, 0x0D, 0xD800, 0x200D, 0x0F_FFFF, 0x00] {
            let rendered = rendered(&unicode::describe(codepoint));
            assert_eq!(
                rendered, PLACEHOLDER,
                "U+{codepoint:04X} must render as a placeholder"
            );
        }
        for codepoint in [0x41u32, 0x1_F9D1, 0x4E00] {
            let info = unicode::describe(codepoint);
            assert_ne!(rendered(&info), PLACEHOLDER);
        }
    }

    #[test]
    fn a_lone_combining_mark_gets_a_dotted_circle_to_sit_on() {
        // U+0301 COMBINING ACUTE ACCENT printed bare would accent whatever
        // character came before it in the report.
        for codepoint in [0x0301u32, 0xFE0F, 0x20DD, 0x0903] {
            let rendered = rendered(&unicode::describe(codepoint));
            assert!(
                rendered.starts_with(DOTTED_CIRCLE),
                "U+{codepoint:04X} rendered as {rendered:?}"
            );
        }
        // Ordinary characters are untouched.
        assert_eq!(rendered(&unicode::describe(0x41)), "A");
    }

    #[test]
    fn breakdown_lines_read_like_the_brief_asks() {
        let line = breakdown_line(&unicode::describe(0x1_F9D1));
        assert!(line.starts_with("U+1F9D1"), "{line}");
        assert!(line.contains("Adult"), "{line}");
        assert!(line.contains('\u{1f9d1}'), "{line}");
    }

    #[test]
    fn breakdown_lines_never_contain_a_control_byte() {
        for codepoint in [0x00u32, 0x1B, 0x07, 0x9B, 0xD800, 0x10_FFFF] {
            let line = breakdown_line(&unicode::describe(codepoint));
            assert!(!line.contains('\u{1b}'), "{line:?}");
            assert!(
                !line.chars().any(char::is_control),
                "{line:?} contains a control character"
            );
        }
    }

    #[test]
    fn breakdown_notes_name_the_cluster_glue() {
        assert!(
            breakdown_note(&unicode::describe(0x200D))
                .unwrap_or_default()
                .contains("joins")
        );
        assert!(
            breakdown_note(&unicode::describe(0xFE0F))
                .unwrap_or_default()
                .contains("VS16")
        );
        assert!(
            breakdown_note(&unicode::describe(0x1_F1F8))
                .unwrap_or_default()
                .contains("flag")
        );
        assert!(breakdown_note(&unicode::describe(0x41)).is_none());
        assert!(
            breakdown_note(&unicode::describe(0x1B))
                .unwrap_or_default()
                .contains("not safe")
        );
    }

    #[test]
    fn legacy_width_never_segments() {
        assert_eq!(legacy_width(FARMER), 4);
        assert_eq!(legacy_width(""), 0);
        assert_eq!(legacy_width("\u{200d}"), 0, "a lone joiner is zero cells");
    }
}
