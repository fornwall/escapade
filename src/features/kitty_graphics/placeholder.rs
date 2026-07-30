//! Unicode placeholders: making an image behave like text.
//!
//! A placement anchored to the cursor is a floating object. It does not scroll,
//! `tmux` does not know it exists, and a full-screen application that redraws a
//! line has no idea it just moved something. The placeholder mechanism inverts
//! that: the image is transmitted, a *virtual placement* is created with `U=1`,
//! and then the image is displayed by printing ordinary text — the private-use
//! character U+10EEEE, once per cell, coloured to say which image it is.
//!
//! Because it is only text, every layer above the terminal moves it correctly:
//! tmux reflows it, an editor scrolls it, `less` pages it. The terminal draws the
//! right slice of the right image into each cell.
//!
//! Three pieces of information have to fit into a cell of text:
//!
//! * **which image** — the low 24 bits of the image id go in the foreground
//!   colour, and a third diacritic carries the most significant byte when the id
//!   needs more than 24 bits. The placement id, if any, goes in the *underline*
//!   colour.
//! * **which row** — the first combining character.
//! * **which column** — the second combining character.
//!
//! The diacritics are drawn from [`DIACRITICS`], a fixed list of 297 combining
//! marks of canonical combining class 230 that survive Unicode normalisation.
//! Index `n` in that list means the number `n`, so a placeholder can address 297
//! rows and 297 columns.
//!
//! Specification:
//! <https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders>

use std::fmt::Write as _;

use crate::term::CSI;

/// The placeholder character, U+10EEEE.
///
/// A terminal that implements placeholders replaces this glyph with a slice of an
/// image; one that does not draws whatever its font has for an unassigned
/// private-use codepoint, which is to say tofu.
pub const PLACEHOLDER: char = '\u{10eeee}';

/// The number of rows or columns a placeholder can address.
pub const MAX_INDEX: usize = DIACRITICS.len();

/// Combining characters that encode row and column numbers.
///
/// Generated from `gen/rowcolumn-diacritics.txt` in the kitty source tree, which
/// derives it from Unicode 6.0.0 `UnicodeData.txt` by taking every `Mn` mark of
/// combining class 230 (above the base character) that has no decomposition
/// mapping, minus a handful that normalisation would fuse into a precomposed
/// character. Position in this list *is* the encoded number, so the order must
/// never change.
pub const DIACRITICS: [char; 297] = [
    '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}', '\u{033f}',
    '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035b}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036a}', '\u{036b}', '\u{036c}', '\u{036d}', '\u{036e}', '\u{036f}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059c}', '\u{059d}', '\u{059e}', '\u{059f}', '\u{05a0}', '\u{05a1}',
    '\u{05a8}', '\u{05a9}', '\u{05ab}', '\u{05ac}', '\u{05af}', '\u{05c4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065a}', '\u{065b}', '\u{065d}', '\u{065e}', '\u{06d6}', '\u{06d7}', '\u{06d8}',
    '\u{06d9}', '\u{06da}', '\u{06db}', '\u{06dc}', '\u{06df}', '\u{06e0}', '\u{06e1}', '\u{06e2}',
    '\u{06e4}', '\u{06e7}', '\u{06e8}', '\u{06eb}', '\u{06ec}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073a}', '\u{073d}', '\u{073f}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074a}', '\u{07eb}', '\u{07ec}', '\u{07ed}', '\u{07ee}',
    '\u{07ef}', '\u{07f0}', '\u{07f1}', '\u{07f3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081b}', '\u{081c}', '\u{081d}', '\u{081e}', '\u{081f}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082a}', '\u{082b}', '\u{082c}',
    '\u{082d}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0f82}', '\u{0f83}', '\u{0f86}', '\u{0f87}',
    '\u{135d}', '\u{135e}', '\u{135f}', '\u{17dd}', '\u{193a}', '\u{1a17}', '\u{1a75}', '\u{1a76}',
    '\u{1a77}', '\u{1a78}', '\u{1a79}', '\u{1a7a}', '\u{1a7b}', '\u{1a7c}', '\u{1b6b}', '\u{1b6d}',
    '\u{1b6e}', '\u{1b6f}', '\u{1b70}', '\u{1b71}', '\u{1b72}', '\u{1b73}', '\u{1cd0}', '\u{1cd1}',
    '\u{1cd2}', '\u{1cda}', '\u{1cdb}', '\u{1ce0}', '\u{1dc0}', '\u{1dc1}', '\u{1dc3}', '\u{1dc4}',
    '\u{1dc5}', '\u{1dc6}', '\u{1dc7}', '\u{1dc8}', '\u{1dc9}', '\u{1dcb}', '\u{1dcc}', '\u{1dd1}',
    '\u{1dd2}', '\u{1dd3}', '\u{1dd4}', '\u{1dd5}', '\u{1dd6}', '\u{1dd7}', '\u{1dd8}', '\u{1dd9}',
    '\u{1dda}', '\u{1ddb}', '\u{1ddc}', '\u{1ddd}', '\u{1dde}', '\u{1ddf}', '\u{1de0}', '\u{1de1}',
    '\u{1de2}', '\u{1de3}', '\u{1de4}', '\u{1de5}', '\u{1de6}', '\u{1dfe}', '\u{20d0}', '\u{20d1}',
    '\u{20d4}', '\u{20d5}', '\u{20d6}', '\u{20d7}', '\u{20db}', '\u{20dc}', '\u{20e1}', '\u{20e7}',
    '\u{20e9}', '\u{20f0}', '\u{2cef}', '\u{2cf0}', '\u{2cf1}', '\u{2de0}', '\u{2de1}', '\u{2de2}',
    '\u{2de3}', '\u{2de4}', '\u{2de5}', '\u{2de6}', '\u{2de7}', '\u{2de8}', '\u{2de9}', '\u{2dea}',
    '\u{2deb}', '\u{2dec}', '\u{2ded}', '\u{2dee}', '\u{2def}', '\u{2df0}', '\u{2df1}', '\u{2df2}',
    '\u{2df3}', '\u{2df4}', '\u{2df5}', '\u{2df6}', '\u{2df7}', '\u{2df8}', '\u{2df9}', '\u{2dfa}',
    '\u{2dfb}', '\u{2dfc}', '\u{2dfd}', '\u{2dfe}', '\u{2dff}', '\u{a66f}', '\u{a67c}', '\u{a67d}',
    '\u{a6f0}', '\u{a6f1}', '\u{a8e0}', '\u{a8e1}', '\u{a8e2}', '\u{a8e3}', '\u{a8e4}', '\u{a8e5}',
    '\u{a8e6}', '\u{a8e7}', '\u{a8e8}', '\u{a8e9}', '\u{a8ea}', '\u{a8eb}', '\u{a8ec}', '\u{a8ed}',
    '\u{a8ee}', '\u{a8ef}', '\u{a8f0}', '\u{a8f1}', '\u{aab0}', '\u{aab2}', '\u{aab3}', '\u{aab7}',
    '\u{aab8}', '\u{aabe}', '\u{aabf}', '\u{aac1}', '\u{fe20}', '\u{fe21}', '\u{fe22}', '\u{fe23}',
    '\u{fe24}', '\u{fe25}', '\u{fe26}', '\u{10a0f}', '\u{10a38}', '\u{1d185}', '\u{1d186}', '\u{1d187}',
    '\u{1d188}', '\u{1d189}', '\u{1d1aa}', '\u{1d1ab}', '\u{1d1ac}', '\u{1d1ad}', '\u{1d242}', '\u{1d243}',
    '\u{1d244}', 
];

/// The number of low bits of an image id that fit in a 24-bit foreground colour.
const COLOUR_BITS: u32 = 24;

/// The diacritic for `index`, or `None` past the end of the table.
#[must_use]
pub fn diacritic(index: usize) -> Option<char> {
    DIACRITICS.get(index).copied()
}

/// Whether an image id can be carried by a placeholder's colours.
///
/// The low 24 bits become the foreground colour and the next 8 the third
/// diacritic, so the only ids that cannot be encoded are those whose low 24 bits
/// are zero: the terminal would read the colour as image id `0`, which means
/// "unset". Ids above 2^32 do not exist.
#[must_use]
pub fn is_encodable(image_id: u32) -> bool {
    image_id & 0x00ff_ffff != 0
}

/// The SGR sequence that encodes an image id as a truecolor foreground:
/// `CSI 38 ; 2 ; r ; g ; b m`.
///
/// The three colour components are the top, middle and bottom bytes of the id's
/// low 24 bits. A terminal in 256-colour mode would instead see `CSI 38;5;n m`
/// and read the whole id from `n`, which is why placeholder ids are usually kept
/// small — but truecolor is unambiguous and every terminal that implements
/// placeholders supports it.
#[must_use]
pub fn foreground(image_id: u32) -> String {
    let red = (image_id >> 16) & 0xff;
    let green = (image_id >> 8) & 0xff;
    let blue = image_id & 0xff;
    format!("{CSI}38;2;{red};{green};{blue}m")
}

/// The SGR sequence that encodes a placement id as the underline colour:
/// `CSI 58 ; 2 ; r ; g ; b m`.
///
/// Omitted or zero lets the terminal pick any virtual placement of the image.
#[must_use]
pub fn underline_colour(placement_id: u32) -> String {
    let red = (placement_id >> 16) & 0xff;
    let green = (placement_id >> 8) & 0xff;
    let blue = placement_id & 0xff;
    format!("{CSI}58;2;{red};{green};{blue}m")
}

/// Resets the foreground and underline colours: `CSI 39 m CSI 59 m`.
#[must_use]
pub fn reset_colours() -> String {
    format!("{CSI}39m{CSI}59m")
}

/// One placeholder cell: `U+10EEEE` followed by its row, column and — when the
/// id needs more than 24 bits — most-significant-byte diacritics.
///
/// Returns `None` when the row or column is past [`MAX_INDEX`], since there is no
/// diacritic to encode it with.
#[must_use]
pub fn cell(image_id: u32, row: usize, col: usize) -> Option<String> {
    let mut text = String::with_capacity(16);
    text.push(PLACEHOLDER);
    text.push(diacritic(row)?);
    text.push(diacritic(col)?);
    let most_significant = (image_id >> COLOUR_BITS) as usize;
    if most_significant != 0 {
        text.push(diacritic(most_significant)?);
    }
    Some(text)
}

/// The rows of text that display an image over `cols` × `rows` cells.
///
/// Every cell spells out its own row and column rather than relying on the
/// inheritance rules, which let a cell with no diacritics continue the cell to its
/// left. Being explicit costs a few bytes and buys correctness: the inheritance
/// rules are documented to fail for horizontally scrolled and overlapping images,
/// and a terminal is not obliged to guess.
///
/// Returns `None` when the area is larger than [`MAX_INDEX`] in either direction.
#[must_use]
pub fn block_lines(
    image_id: u32,
    placement_id: Option<u32>,
    cols: u16,
    rows: u16,
) -> Option<Vec<String>> {
    if usize::from(cols) > MAX_INDEX || usize::from(rows) > MAX_INDEX {
        return None;
    }
    let mut prefix = foreground(image_id);
    if let Some(placement_id) = placement_id {
        prefix.push_str(&underline_colour(placement_id));
    }
    let suffix = reset_colours();

    let mut lines = Vec::with_capacity(usize::from(rows));
    for row in 0..usize::from(rows) {
        let mut line = prefix.clone();
        for col in 0..usize::from(cols) {
            line.push_str(&cell(image_id, row, col)?);
        }
        let _ = write!(line, "{suffix}");
        lines.push(line);
    }
    Some(lines)
}

/// [`block_lines`], joined with newlines.
#[must_use]
pub fn block(image_id: u32, placement_id: Option<u32>, cols: u16, rows: u16) -> Option<String> {
    Some(block_lines(image_id, placement_id, cols, rows)?.join("\n"))
}

/// A printable rendering of one placeholder cell, for explaining it.
///
/// The placeholder and its diacritics are invisible or misleading on screen — the
/// whole point is that the terminal replaces them — so the demo shows this
/// instead: `U+10EEEE U+0305 U+030D` and the numbers those marks mean.
#[must_use]
pub fn describe_cell(image_id: u32, row: usize, col: usize) -> String {
    let mut parts = vec!["U+10EEEE".to_owned()];
    let mut meanings = vec![format!("row {row}"), format!("column {col}")];
    for index in [row, col] {
        match diacritic(index) {
            Some(mark) => parts.push(format!("U+{:04X}", u32::from(mark))),
            None => parts.push("(out of range)".to_owned()),
        }
    }
    let most_significant = (image_id >> COLOUR_BITS) as usize;
    if most_significant != 0
        && let Some(mark) = diacritic(most_significant)
    {
        parts.push(format!("U+{:04X}", u32::from(mark)));
        meanings.push(format!("id byte 3 = {most_significant}"));
    }
    format!("{}  ({})", parts.join(" "), meanings.join(", "))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_first_three_diacritics_are_the_ones_the_spec_quotes() {
        // "U+305 is the diacritic corresponding to the number 0 and U+30D
        // corresponds to 1" … "U+30E is the diacritic corresponding to 2".
        assert_eq!(diacritic(0), Some('\u{305}'));
        assert_eq!(diacritic(1), Some('\u{30d}'));
        assert_eq!(diacritic(2), Some('\u{30e}'));
    }

    #[test]
    fn the_table_is_the_size_kitty_generates_and_has_no_duplicates() {
        assert_eq!(DIACRITICS.len(), 297);
        let mut sorted = DIACRITICS;
        sorted.sort_unstable();
        let mut deduped = sorted.to_vec();
        deduped.dedup();
        assert_eq!(deduped.len(), DIACRITICS.len(), "a diacritic is repeated");
    }

    #[test]
    fn every_diacritic_is_a_combining_mark_above_the_character() {
        // Every entry comes from the class-230 block of UnicodeData.txt, so none
        // of them may be a printable character that would take up a cell.
        for &mark in &DIACRITICS {
            let code = u32::from(mark);
            assert!(code >= 0x300, "U+{code:04X} is too low to be a combining mark");
            assert!(
                !mark.is_ascii(),
                "U+{code:04X} would occupy a cell of its own"
            );
        }
    }

    #[test]
    fn the_table_is_addressed_by_position() {
        for (index, &mark) in DIACRITICS.iter().enumerate() {
            assert_eq!(diacritic(index), Some(mark));
        }
        assert_eq!(diacritic(MAX_INDEX), None, "one past the end");
    }

    #[test]
    fn a_two_by_two_placeholder_matches_the_spec_example() {
        // printf "\e[38;5;42m\U10EEEE\U0305\U0305\U10EEEE\U0305\U030D\e[39m\n"
        // printf "\e[38;5;42m\U10EEEE\U030D\U0305\U10EEEE\U030D\U030D\e[39m\n"
        //
        // The example uses 256-colour mode for the id; escapade always writes
        // truecolor, so the colour prefix differs while the cells are identical.
        let lines = block_lines(42, None, 2, 2).unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("\u{1b}[38;2;0;0;42m"));
        assert!(lines[0].contains("\u{10eeee}\u{305}\u{305}\u{10eeee}\u{305}\u{30d}"));
        assert!(lines[1].contains("\u{10eeee}\u{30d}\u{305}\u{10eeee}\u{30d}\u{30d}"));
    }

    #[test]
    fn ids_above_24_bits_get_a_third_diacritic() {
        // The spec's example: image ID 33554474 = 42 + (2 << 24), whose third
        // diacritic is U+30E, the mark for 2.
        let id = 42 + (2 << 24);
        assert_eq!(id, 33_554_474);
        let cell = cell(id, 0, 0).unwrap();
        assert_eq!(
            cell,
            "\u{10eeee}\u{305}\u{305}\u{30e}",
            "row 0, column 0, id byte 3 = 2"
        );
        // And the low 24 bits still travel in the colour.
        assert_eq!(foreground(id), "\u{1b}[38;2;0;0;42m");
    }

    #[test]
    fn the_foreground_colour_is_the_low_three_bytes_of_the_id() {
        assert_eq!(foreground(0x0012_3456), "\u{1b}[38;2;18;52;86m");
        assert_eq!(foreground(1), "\u{1b}[38;2;0;0;1m");
        assert_eq!(underline_colour(7), "\u{1b}[58;2;0;0;7m");
    }

    #[test]
    fn ids_whose_low_24_bits_are_zero_cannot_be_encoded() {
        assert!(is_encodable(1));
        assert!(is_encodable(0x00ff_ffff));
        assert!(is_encodable(0x0100_0001));
        assert!(!is_encodable(0), "zero means unset");
        assert!(
            !is_encodable(0x0100_0000),
            "the colour would read as image id 0"
        );
    }

    #[test]
    fn a_block_carries_a_placement_id_in_the_underline_colour() {
        let lines = block_lines(5, Some(9), 1, 1).unwrap();
        assert!(lines[0].contains("\u{1b}[58;2;0;0;9m"));
        assert!(lines[0].ends_with("\u{1b}[39m\u{1b}[59m"), "colours reset");
    }

    #[test]
    fn every_cell_of_a_block_names_its_own_row_and_column() {
        let lines = block_lines(1, None, 3, 2).unwrap();
        for (row, line) in lines.iter().enumerate() {
            let cells: Vec<&str> = line.split(PLACEHOLDER).skip(1).collect();
            assert_eq!(cells.len(), 3, "three cells per row");
            for (col, cell) in cells.iter().enumerate() {
                let marks: Vec<char> = cell
                    .chars()
                    .take_while(|mark| DIACRITICS.contains(mark))
                    .collect();
                assert_eq!(marks[0], DIACRITICS[row], "the row diacritic");
                assert_eq!(marks[1], DIACRITICS[col], "then the column");
            }
        }
    }

    #[test]
    fn areas_larger_than_the_diacritic_table_are_refused() {
        let too_wide = u16::try_from(MAX_INDEX + 1).unwrap();
        assert!(block_lines(1, None, too_wide, 1).is_none());
        assert!(block_lines(1, None, 1, too_wide).is_none());
        assert!(block_lines(1, None, u16::try_from(MAX_INDEX).unwrap(), 1).is_some());
    }

    #[test]
    fn a_block_is_only_text_plus_colour() {
        // The claim the whole mechanism rests on: nothing here is an APC string,
        // so tmux, vim and less move it around like any other characters.
        let block = block(42, None, 4, 3).unwrap();
        for part in block.split('\u{1b}') {
            assert!(
                !part.starts_with('_'),
                "a placeholder block must contain no APC strings"
            );
        }
        assert_eq!(block.lines().count(), 3);
    }

    #[test]
    fn describe_cell_is_printable_and_names_the_codepoints() {
        let described = describe_cell(42, 1, 2);
        assert!(described.contains("U+10EEEE"));
        assert!(described.contains("U+030D"), "row 1");
        assert!(described.contains("U+030E"), "column 2");
        assert!(described.contains("row 1"));
        assert!(!described.contains('\u{1b}'));

        let with_msb = describe_cell(33_554_474, 0, 0);
        assert!(with_msb.contains("id byte 3 = 2"));
    }
}
