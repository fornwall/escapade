//! The SGR text attributes: bold, italic, six underline shapes, strikethrough,
//! overline, and three independent colours.
//!
//! `CSI <params> m` — Select Graphic Rendition — is the oldest and most widely
//! implemented escape sequence there is, and also the one with the most corners.
//! This module is one subcommand rather than nine because **the attributes compose
//! into a single sequence**:
//!
//! ```text
//! CSI 1;3;4:3;9;38;2;255;0;0 m
//! ```
//!
//! Bold, italic, a curly underline, a strikethrough and a red foreground in thirty
//! bytes. Nine subcommands would force `escapade underline "$(escapade bold text)"`
//! and produce nested resets that fight each other.
//!
//! # What is actually hard here
//!
//! Not the sending. Every part of this is one `CSI … m`, and a terminal that does not
//! implement a parameter is required to ignore it. The hard parts are:
//!
//! * **The resets are not obvious.** `1` and `2` share `22`; `53` resets with `55`
//!   and not `54`; `39` does not touch the underline colour. [`attrs`] has the whole
//!   table, and [`attrs::Rendition::close`] resets only what was set rather than
//!   reaching for `CSI 0 m`.
//! * **`SGR 21` means two different things** — "doubly underlined" in ECMA-48, "bold
//!   off" in much of the world — so this crate never emits it. See
//!   [`attrs::SGR_TWENTY_ONE`].
//! * **Colons and semicolons are not interchangeable.** `38;2;r;g;b` is five
//!   parameters and `38:2::r:g:b` is one; on a terminal that does not implement the
//!   parameter the first can be read as `SGR 2` followed by `SGR 0`, which resets
//!   every attribute on the line. [`color`] picks per role and explains why.
//! * **Support is invisible.** A terminal that ignores `SGR 3` looks exactly like one
//!   that renders it, from the sending end. [`probe`] measures it where it can, by
//!   setting the attribute and asking the terminal to report its own state back.
//! * **The text is untrusted.** It goes between our own sequences, so
//!   [`sanitize_text`] neutralises anything that could introduce one of its own.

pub mod attrs;
pub mod color;
pub mod demo;
pub mod emit;
pub mod preference;
pub mod probe;
pub mod supports;

use std::process::ExitCode;

use anyhow::Result;

use crate::{
    cli::{StyleAction, StyleCommand},
    db::Catalog,
    theme::Theme,
};

/// The feature id in the database. The subcommand is `style`, aliased `sgr`.
pub const FEATURE_ID: &str = "text-attributes";

// ---------------------------------------------------------------------------
// Sanitisation
// ---------------------------------------------------------------------------

/// Neutralises the control bytes in text that is about to be styled.
///
/// The text sits *between* the sequence that sets the attributes and the one that
/// puts them back, so it is ordinary terminal output with all the authority that
/// implies. Untrusted text — a filename, a branch name, a compiler message, a line of
/// somebody's log — is otherwise free to carry `CSI 0 m` and cancel the styling, or
/// `CSI 2 J` and clear the screen, or `OSC 8` and make the rest of the line a link
/// somewhere else.
///
/// Percent-encoding is not an option, because the whole point of the text is that
/// somebody reads it. So every byte that could introduce or terminate an escape
/// sequence is replaced by its glyph from the Unicode Control Pictures block: `ESC`
/// becomes `␛`, `BEL` becomes `␇`. Those are inert, one cell wide and visible, so
/// text with an `ESC` in it looks suspicious rather than executing.
///
/// `\n` and `\t` survive. Neither can introduce a sequence, both are legitimate
/// layout, and every SGR attribute is a cell attribute that spans a line break
/// without complaint. `\r` does not survive: returning to column zero is how text
/// gets overwritten by later text, which is a spoofing tool and not layout.
#[must_use]
pub fn sanitize_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\n' | '\t' => out.push(ch),
            // C0 controls. `ESC` (0x1b) is the dangerous one, but none of the rest
            // belong in a line of text either.
            '\u{0}'..='\u{1f}' => out.push(control_picture(ch)),
            // DEL has its own Control Pictures glyph, U+2421.
            '\u{7f}' => out.push('\u{2421}'),
            // C1 controls: 0x9b is the eight-bit spelling of `CSI` and 0x9d of
            // `OSC`, so letting them through would reopen the whole hole. There are
            // no Control Pictures for these, so they become the replacement
            // character.
            '\u{80}'..='\u{9f}' => out.push('\u{fffd}'),
            other => out.push(other),
        }
    }
    out
}

/// Maps a C0 control onto its Unicode Control Pictures glyph: `0x1b` becomes
/// U+241B `␛`.
fn control_picture(ch: char) -> char {
    char::from_u32(0x2400 + u32::from(ch)).unwrap_or('\u{fffd}')
}

/// Whether `text` contains anything [`sanitize_text`] would replace.
#[must_use]
pub fn text_is_clean(text: &str) -> bool {
    !text
        .chars()
        .any(|ch| !matches!(ch, '\n' | '\t') && ch.is_control())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Entry point for `escapade style …`.
///
/// With no subcommand this styles text, which is the useful thing to do with this
/// feature from a script; the usual verbs sit underneath.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with, and fails if the feature is
/// missing from the database.
pub fn run(catalog: &Catalog, command: &StyleCommand, no_color: bool) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        None => {
            emit::run(&theme, command)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(StyleAction::Explain { no_matrix }) => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        // The only path whose exit status carries a finding, so the only one that does
        // not simply return success.
        Some(StyleAction::Probe { attrs, format }) => probe::report(&theme, catalog, attrs, *format),
        Some(StyleAction::Demo {
            force,
            only,
            step,
            list,
        }) => {
            if *list {
                demo::list_steps(&theme);
            } else {
                demo::run(&theme, catalog, *force, only.as_deref(), *step)?;
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::style::attrs::{Attr, Rendition, UnderlineShape};

    #[test]
    fn layout_survives_but_carriage_returns_do_not() {
        assert_eq!(sanitize_text("a\nb\tc"), "a\nb\tc");
        // `\r` would let later text overwrite earlier text.
        assert_eq!(sanitize_text("visible\rhidden"), "visible\u{240d}hidden");
        assert_eq!(sanitize_text("bell\u{7}"), "bell\u{2407}");
        assert_eq!(sanitize_text("del\u{7f}"), "del\u{2421}");
        assert_eq!(sanitize_text("nul\u{0}"), "nul\u{2400}");
        assert_eq!(sanitize_text("caf\u{e9} \u{1f600}"), "caf\u{e9} \u{1f600}");
        assert_eq!(sanitize_text(""), "");
    }

    #[test]
    fn an_escape_in_the_text_cannot_reach_the_terminal() {
        // The attack: cancel our styling, clear the screen, retitle the window.
        let hostile = "safe\u{1b}[0m\u{1b}[2J\u{1b}]0;pwned\u{7}";
        let rendition = Rendition::new()
            .with(Attr::Bold)
            .with(Attr::Underline(UnderlineShape::Curly));
        let emitted = rendition.wrap(hostile);

        // Exactly two escape bytes survive: the open and the close, both ours.
        assert_eq!(emitted.matches('\u{1b}').count(), 2);
        assert!(emitted.starts_with("\u{1b}[1;4:3m"));
        assert!(emitted.ends_with("\u{1b}[22;24m"));
        // The payload is still readable, as visible glyphs.
        assert!(emitted.contains("safe\u{241b}[0m\u{241b}[2J\u{241b}]0;pwned\u{2407}"));
        assert!(!emitted.contains('\u{7}'));
    }

    #[test]
    fn an_sgr_of_its_own_in_the_text_cannot_change_the_styling() {
        for hostile in [
            "word\u{1b}[0mrest",
            "word\u{1b}[22mrest",
            "word\u{1b}[24mrest",
            "word\u{1b}[39mrest",
            "word\u{1b}[38;2;0;255;0mrest",
        ] {
            let emitted = Rendition::new().with(Attr::Bold).wrap(hostile);
            assert_eq!(
                emitted.matches('\u{1b}').count(),
                2,
                "only our own two sequences may be an ESC: {emitted:?}"
            );
            assert!(emitted.contains('\u{241b}'), "{emitted:?}");
        }
    }

    #[test]
    fn the_eight_bit_csi_is_neutralised_too() {
        // 0x9b is `CSI` in a single byte, 0x9d is `OSC`. A sanitiser that only
        // looked for 0x1b would let both straight through.
        assert_eq!(sanitize_text("a\u{9b}0mb"), "a\u{fffd}0mb");
        assert_eq!(sanitize_text("a\u{9d}0;titleb"), "a\u{fffd}0;titleb");
        assert!(
            !Rendition::new()
                .with(Attr::Bold)
                .wrap("a\u{9b}0mb")
                .contains('\u{9b}')
        );
    }

    #[test]
    fn text_is_clean_agrees_with_the_sanitiser() {
        for text in ["plain", "a\nb", "a\tb", "caf\u{e9}", ""] {
            assert!(text_is_clean(text), "{text:?}");
            assert_eq!(sanitize_text(text), text);
        }
        for text in ["a\u{1b}b", "a\u{9b}b", "a\rb", "a\u{7}b", "a\u{7f}b"] {
            assert!(!text_is_clean(text), "{text:?}");
            assert_ne!(sanitize_text(text), text);
        }
    }

    #[test]
    fn the_sanitiser_never_lets_a_loose_escape_out() {
        // Every control character there is, in one string.
        let hostile: String = (0u32..=0x9f)
            .filter_map(char::from_u32)
            .chain("plain text".chars())
            .collect();
        let rendition = Rendition::new().with(Attr::Overline);
        let emitted = rendition.wrap(&hostile);
        let chars: Vec<char> = emitted.chars().collect();
        for (index, ch) in chars.iter().enumerate() {
            if *ch == '\u{1b}' {
                assert_eq!(chars.get(index + 1), Some(&'['), "loose ESC at {index}");
            }
        }
        assert_eq!(emitted.matches('\u{1b}').count(), 2);
    }

    #[test]
    fn the_feature_is_in_the_database_under_the_id_this_module_uses() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        assert_eq!(feature.id, "text-attributes");
        assert!(!feature.summary.trim().is_empty());
    }
}
