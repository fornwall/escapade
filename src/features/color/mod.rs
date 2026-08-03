//! Colour queries and change notification: OSC 4/10/11/12/17/19, DEC mode 2031.
//!
//! ```text
//! →  OSC 10 ; ? ST              default foreground
//! →  OSC 11 ; ? ST              default background
//! →  OSC 12 ; ? ST              cursor
//! →  OSC 17 ; ? ST              selection background
//! →  OSC 19 ; ? ST              selection foreground
//! →  OSC 4 ; <index> ; ? ST     one of the 256 palette entries
//! ←  OSC <same number> ; rgb:<r>/<g>/<b> ST
//!
//! →  CSI ? 2031 h               report colour-scheme changes from now on
//! ←  CSI ? 997 ; 1 n            …dark   (unsolicited, one per change)
//! ←  CSI ? 997 ; 2 n            …light
//! ```
//!
//! # Why this exists
//!
//! Every program that colours its own output is choosing colours against a
//! background it cannot see. The traditional answers are all guesses: `COLORFGBG`
//! is set by a handful of terminals and stale in every one that inherits it over
//! ssh, `TERM` says nothing about a theme, and "assume dark" is why so much
//! tooling is unreadable on a light terminal. **The terminal knows, and it will
//! say if asked.**
//!
//! `?` in place of a colour turns the sequence that *sets* a colour into one that
//! reports it. That is the entire mechanism, and it is why this feature is old
//! enough to be nearly universal while the far newer mode 2031 — which reports
//! *changes* — is not.
//!
//! # The modules
//!
//! * [`value`] — the reply format, and the light/dark decision. Pure, and the
//!   home of the digit-scaling trap that makes `rgb:f/0/0` full red.
//! * [`query`] — the round trip: batching, the barrier, and draining.
//! * [`notify`] — mode 2031, the subscription, and switching it off again.
//! * [`report`] — the one-shot report, and `--is-dark`.
//! * [`watch`] — `--watch`: notification when the terminal has it, polling when
//!   it does not, and saying which.
//! * [`probe`] — the predicate. The rare probe in this project that *measures*.
//! * [`demo`] — the walkthrough.

pub mod demo;
pub mod notify;
pub mod probe;
pub mod query;
pub mod report;
pub mod value;
pub mod watch;

use std::process::ExitCode;

use anyhow::Result;

use crate::{
    cli::{ColorAction, ColorCommand},
    db::Catalog,
    theme::Theme,
};

/// The feature id in the database. The subcommand is `color`.
pub const FEATURE_ID: &str = "color";

/// Entry point for `escapade color …`.
///
/// Returns an [`ExitCode`] because two of these paths answer through it rather
/// than through their output: `probe` is a predicate, and `--is-dark` reports
/// light or dark as 0, 1 or 2 while printing nothing at all.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with.
pub fn run(catalog: &Catalog, command: &ColorCommand, no_color: bool) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        Some(ColorAction::Explain { no_matrix }) => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        Some(ColorAction::Probe { format }) => probe::report(&theme, catalog, *format),
        Some(ColorAction::Demo {
            force,
            only,
            step,
            list,
        }) => {
            if *list {
                demo::list_steps(&theme);
            } else {
                demo::run(&theme, *force, only.as_deref(), *step)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        None if command.watch => watch::run(&theme, command),
        None => report::run(&theme, command),
    }
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_feature_is_in_the_database_under_the_id_this_module_uses() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        assert_eq!(feature.id, FEATURE_ID);
    }

    #[test]
    fn the_feature_file_is_complete_enough_to_explain() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        assert_eq!(feature.name, "Colour queries");
        assert!(feature.spec_url.is_some());
        assert!(feature.problem.is_some());
        assert!(feature.how_it_works.is_some());
        assert!(feature.payload.is_some());
        assert!(feature.caveats.is_some());
        assert!(!feature.param.is_empty());
        assert!(!feature.user.is_empty());
        assert!(!feature.resource.is_empty());
    }

    #[test]
    fn the_file_documents_both_halves_of_the_feature() {
        // Asking and being told are separate capabilities with separate
        // verdicts, and a page that covered only the first would leave a reader
        // thinking a snapshot is all there is.
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();

        let queries: Vec<&str> = feature
            .sequence
            .iter()
            .map(|sequence| sequence.request.as_str())
            .collect();
        for expected in ["OSC 11 ; ? ST", "OSC 4 ; 1 ; ? ST", "CSI c"] {
            assert!(
                queries.iter().any(|request| request.contains(expected)),
                "no sequence for {expected}: {queries:?}"
            );
        }
        for expected in ["CSI ? 2031 h", "CSI ? 2031 l", "CSI ? 996 n"] {
            let anywhere = queries.iter().any(|request| request.contains(expected))
                || feature
                    .sequence
                    .iter()
                    .filter_map(|sequence| sequence.notes.as_deref())
                    .any(|notes| notes.contains(expected));
            assert!(anywhere, "mode 2031 is undocumented: {expected}");
        }
    }

    #[test]
    fn the_trap_that_inverts_every_verdict_is_written_down() {
        // `rgb:f/0/0` is full red. A page that does not say so leaves the reader
        // to reimplement the bug that this whole feature file exists to prevent.
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        let how = feature.how_it_works.as_deref().unwrap();
        assert!(how.contains("rgb:f/0/0"), "{how}");
        assert!(how.contains("XParseColor"), "{how}");
        let payload = feature.payload.as_deref().unwrap();
        assert!(payload.contains("#f00"), "the `#` form is the opposite case");
    }

    #[test]
    fn the_multiplexer_caveat_is_stated_rather_than_left_implied() {
        // tmux answers colour queries out of its own palette *and* answers the
        // barrier. Both halves have to be in the prose or every verdict read
        // inside tmux is misleading.
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        let caveats = feature.caveats.as_deref().unwrap();
        assert!(caveats.contains("tmux"), "{caveats}");
        assert!(caveats.contains("barrier"), "{caveats}");
    }

    #[test]
    fn every_positive_support_row_cites_a_source() {
        // An unchecked claim about a terminal nobody can verify is worth less
        // than an admission that nobody checked — which is what `unknown` is
        // for, and those rows are allowed to have no source.
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        for row in &feature.support {
            if row.status == crate::db::SupportStatus::Unknown {
                continue;
            }
            assert!(
                row.source.is_some(),
                "no source for the {} row",
                row.terminal
            );
        }
        assert!(
            feature
                .support
                .iter()
                .any(|row| row.status == crate::db::SupportStatus::Yes),
            "a feature this old should have at least one confirmed implementation"
        );
    }
}
