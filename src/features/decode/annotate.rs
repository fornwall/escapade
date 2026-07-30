//! `--unicode`: what the text between the sequences is made of.
//!
//! Everything here comes from [`crate::unicode`] — codepoint names, general
//! categories, East Asian widths and UAX #29 grapheme clustering — so the answers
//! agree with what `escapade grapheme` reports about the same text. This module
//! only groups them into the shape the two output formats need.
//!
//! Clusters are the unit, not codepoints. "One character" to a user is a cluster:
//! a family emoji is five codepoints and one cluster, and a decode that listed
//! five characters would be answering a different question from the one asked.

use crate::unicode::{self, CharInfo};

/// One grapheme cluster and the codepoints it is made of.
#[derive(Debug, Clone)]
pub struct Cluster {
    /// The cluster itself.
    pub text: String,
    /// Columns it occupies, by East Asian Width.
    pub width: usize,
    /// Its codepoints, in order.
    pub codepoints: Vec<CharInfo>,
}

impl Cluster {
    /// Whether this cluster is more than one codepoint, and so worth a heading of
    /// its own in the output.
    #[must_use]
    pub fn is_composite(&self) -> bool {
        self.codepoints.len() > 1
    }
}

/// A run of text, broken down.
#[derive(Debug, Clone)]
pub struct Analysis {
    /// The clusters, in order.
    pub clusters: Vec<Cluster>,
    /// How many codepoints there are in total.
    pub codepoints: usize,
    /// How many columns the run occupies.
    pub width: usize,
}

impl Analysis {
    /// The one-line summary that precedes the detail.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} in {}, {} wide",
            plural(self.codepoints, "codepoint"),
            plural(self.clusters.len(), "grapheme cluster"),
            plural(self.width, "column"),
        )
    }
}

/// Breaks a run of text into clusters and codepoints.
#[must_use]
pub fn analyse(text: &str) -> Analysis {
    let clusters: Vec<Cluster> = unicode::graphemes(text)
        .into_iter()
        .map(|cluster| Cluster {
            text: cluster.to_owned(),
            width: unicode::display_width(cluster),
            codepoints: cluster
                .chars()
                .map(|character| unicode::describe(u32::from(character)))
                .collect(),
        })
        .collect();
    Analysis {
        codepoints: clusters
            .iter()
            .map(|cluster| cluster.codepoints.len())
            .sum(),
        width: unicode::display_width(text),
        clusters,
    }
}

/// The character itself, when it is safe to print.
///
/// The house rule, as in `glyph_protocol::supports::rendered`: a codepoint is
/// never printed without checking it first. Here it also covers the invisible
/// ones, since a listing whose "character" column is blank for a joiner and blank
/// for a space is a listing that cannot be read.
#[must_use]
pub fn glyph(info: &CharInfo) -> String {
    match info.character {
        Some(character) if info.printable() && super::display::showable(character) => {
            character.to_string()
        }
        _ => "·".to_owned(),
    }
}

/// The broad category of a codepoint, as one word.
#[must_use]
pub fn category(info: &CharInfo) -> &'static str {
    match info.character {
        Some(character) => unicode::category_group_name(character),
        None => "other",
    }
}

/// `1 column` but `3 columns`.
fn plural(value: usize, singular: &str) -> String {
    if value == 1 {
        format!("{value} {singular}")
    } else {
        format!("{value} {singular}s")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_one_codepoint_per_cluster() {
        let analysis = analyse("abc");
        assert_eq!(analysis.clusters.len(), 3);
        assert_eq!(analysis.codepoints, 3);
        assert_eq!(analysis.width, 3);
        assert!(!analysis.clusters[0].is_composite());
        assert_eq!(analysis.clusters[0].codepoints[0].label(), "U+0061");
    }

    #[test]
    fn a_family_emoji_is_one_cluster_of_five_codepoints() {
        // The example from the brief: `family 👨‍👩‍👧`.
        let analysis = analyse("family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}");
        assert_eq!(analysis.codepoints, 7 + 5);
        assert_eq!(analysis.clusters.len(), 7 + 1);
        let family = analysis.clusters.last().unwrap();
        assert!(family.is_composite());
        assert_eq!(family.codepoints.len(), 5);
        assert_eq!(family.codepoints[1].label(), "U+200D");
        assert!(family.width >= 2, "an emoji is at least two columns wide");
    }

    #[test]
    fn a_combining_mark_joins_its_base() {
        let analysis = analyse("e\u{301}");
        assert_eq!(analysis.clusters.len(), 1);
        assert_eq!(analysis.codepoints, 2);
        assert_eq!(analysis.width, 1, "the mark adds no width");
    }

    #[test]
    fn a_wide_character_counts_two_columns() {
        let analysis = analyse("日本");
        assert_eq!(analysis.width, 4);
        assert_eq!(analysis.clusters[0].width, 2);
    }

    #[test]
    fn the_summary_counts_in_the_right_units() {
        assert_eq!(analyse("a").summary(), "1 codepoint in 1 grapheme cluster, 1 column wide");
        assert_eq!(
            analyse("ab").summary(),
            "2 codepoints in 2 grapheme clusters, 2 columns wide"
        );
    }

    #[test]
    fn nothing_invisible_is_printed_as_itself() {
        for codepoint in [0x200d_u32, 0x1b, 0x202e, 0xe0a0, 0x7f] {
            let info = unicode::describe(codepoint);
            assert_eq!(glyph(&info), "·", "U+{codepoint:04X} was printed");
        }
        assert_eq!(glyph(&unicode::describe(0x41)), "A");
    }

    #[test]
    fn categories_are_single_words() {
        assert_eq!(category(&unicode::describe(0x41)), "letter");
        assert_eq!(category(&unicode::describe(0x200d)), "other");
        assert_eq!(category(&unicode::describe(0x20)), "separator");
        // A surrogate is a codepoint but not a character.
        assert_eq!(category(&unicode::describe(0xd800)), "other");
    }
}
