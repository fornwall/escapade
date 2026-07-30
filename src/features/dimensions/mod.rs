//! Terminal size in cells and in pixels: `TIOCGWINSZ`, `CSI 18 t`, `CSI 14 t`,
//! `CSI 16 t`.
//!
//! "How big is the terminal" has three answers, and they arrive by different
//! routes with different reliability:
//!
//! * **cells** — columns and rows. Essentially always available.
//! * **window pixels** — the size of the text area. Often unavailable.
//! * **cell pixels** — the size of one character cell, which is what anything
//!   drawing an image, a sparkline or a box of a particular physical size
//!   actually needs. The least available of the three.
//!
//! # The fallback chain
//!
//! `TIOCGWINSZ` comes first for both cells and pixels: no round trip, no timeout,
//! and no multiplexer can filter it, because the numbers are already in the pty.
//! Its `ws_xpixel`/`ws_ypixel` fields are the catch — Linux's `tty_ioctl(4)`
//! documents them as "unused". Most Unix terminals fill them in anyway; VTE-based
//! terminals, GNU Screen and everything on Windows do not, and several fill them
//! in only from the first resize onward.
//!
//! When the ioctl cannot answer, three queries go out in one round trip behind a
//! `CSI c` barrier. `CSI 18 t` gives cells, `CSI 14 t` the text area in pixels,
//! `CSI 16 t` one cell in pixels. If none of them supplies a cell size, it is
//! derived by dividing the window pixels by the grid — accurate to however much
//! padding the terminal keeps around its text area. [`size`] holds the chain, and
//! every number this feature prints says which rung produced it.
//!
//! # The reply order trap
//!
//! Every one of xterm's size reports puts the **vertical number first**:
//!
//! ```text
//! →  CSI 18 t   ←  CSI 8 ; rows ; cols t
//! →  CSI 14 t   ←  CSI 4 ; height ; width t
//! →  CSI 16 t   ←  CSI 6 ; height ; width t
//! ```
//!
//! Reading them as width-then-height transposes the answer, and on a window that
//! happens to be square it does so invisibly. Verified against xterm's `ctlseqs`,
//! "Window manipulation (dtterm)".
//!
//! # Why a pixel size is so often missing
//!
//! `CSI 16 t` is the query terminals skip: Alacritty, VTE (so GNOME Terminal),
//! iTerm2 and Warp all answer `CSI 14 t` and ignore `CSI 16 t`, iTerm2 offering a
//! proprietary `OSC 1337 ReportCellSize` in its place. st answers none of the
//! three at all and can only be asked through the ioctl. So an application that
//! wants a cell size has to be ready to divide, and ready to assume.
//!
//! # The multiplexer caveat
//!
//! A multiplexer answers these queries **itself**, from its own emulator. tmux
//! replies to all three and forwards none; Zellij answers `CSI 18 t` itself and
//! relays the two pixel queries outward; GNU Screen implements only `CSI 18 t` and
//! never fills in the ioctl's pixel fields, so inside Screen a pixel size is
//! simply unobtainable. Inside any of them a pixel size is the multiplexer's idea
//! of the pane rather than the window's — and the barrier reply comes from the
//! multiplexer too, so it cannot license the conclusion that the real terminal
//! does not implement a report. That is exactly the case this feature reports as
//! unknown rather than guessing at.
//!
//! # Where the parsers live
//!
//! [`size`] borrows `parse_window_pixels` and `parse_cell_pixels` from
//! [`crate::features::kitty_graphics::layout`], which needed a cell size first and
//! already had them, rather than keeping a second copy of the height-before-width
//! order. Both of them, and this module's [`size::parse_text_area_cells`], really
//! belong next to the sequences they parse in [`crate::term`]; moving them is
//! outside this feature's remit.

pub mod probe;
pub mod query;
pub mod report;
pub mod size;
pub mod threshold;

use std::process::ExitCode;

use anyhow::Result;

use crate::{
    cli::{DimensionsAction, DimensionsCommand, OutputFormat},
    db::Catalog,
    features::{
        dimensions::{
            query::Wanted,
            threshold::{Bounds, Thresholds},
        },
        explain,
    },
    theme::Theme,
};

/// The feature id in the database. The subcommand is `dimensions`.
pub const FEATURE_ID: &str = "dimensions";

/// Entry point for `escapade dimensions …`.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with: a contradictory pair of
/// thresholds, or a terminal that cannot be set up.
pub fn run(catalog: &Catalog, command: &DimensionsCommand, no_color: bool) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        Some(DimensionsAction::Explain { no_matrix }) => {
            let feature = catalog.feature(FEATURE_ID)?;
            explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        Some(action @ DimensionsAction::Probe { format, .. }) => {
            probe::report(&theme, thresholds_of(action), *format)
        }
        None => {
            // The report wants all three quantities, and a cell size that was
            // measured rather than divided — so it asks even when the ioctl has
            // already supplied the window's pixel size.
            let sizes = query::gather(Wanted {
                window_pixels: true,
                cell_pixels: true,
                always_ask: false,
            })?;
            match command.format {
                OutputFormat::Text => report::render(&theme, &sizes),
                OutputFormat::Json => print!("{}", report::json_line(&sizes)),
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Collects the twelve threshold flags into [`Thresholds`].
///
/// Separated out and tested, because twelve near-identical fields is precisely
/// where a copy-paste puts `cols_at_most` into the `rows` bounds and nothing ever
/// notices.
#[must_use]
pub fn thresholds_of(action: &DimensionsAction) -> Thresholds {
    match action {
        DimensionsAction::Explain { .. } => Thresholds::default(),
        DimensionsAction::Probe {
            rows_at_least,
            rows_at_most,
            rows_exactly,
            cols_at_least,
            cols_at_most,
            cols_exactly,
            width_at_least,
            width_at_most,
            width_exactly,
            height_at_least,
            height_at_most,
            height_exactly,
            format: _,
        } => Thresholds {
            rows: Bounds {
                at_least: *rows_at_least,
                at_most: *rows_at_most,
                exactly: *rows_exactly,
            },
            cols: Bounds {
                at_least: *cols_at_least,
                at_most: *cols_at_most,
                exactly: *cols_exactly,
            },
            width: Bounds {
                at_least: *width_at_least,
                at_most: *width_at_most,
                exactly: *width_exactly,
            },
            height: Bounds {
                at_least: *height_at_least,
                at_most: *height_at_most,
                exactly: *height_exactly,
            },
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_threshold_flag_reaches_its_own_dimension() {
        // Twelve distinct values, so a field wired to the wrong bound shows up as
        // the wrong number rather than as nothing at all.
        let action = DimensionsAction::Probe {
            rows_at_least: Some(1),
            rows_at_most: Some(2),
            rows_exactly: Some(3),
            cols_at_least: Some(4),
            cols_at_most: Some(5),
            cols_exactly: Some(6),
            width_at_least: Some(7),
            width_at_most: Some(8),
            width_exactly: Some(9),
            height_at_least: Some(10),
            height_at_most: Some(11),
            height_exactly: Some(12),
            format: None,
        };
        assert_eq!(
            thresholds_of(&action),
            Thresholds {
                rows: Bounds {
                    at_least: Some(1),
                    at_most: Some(2),
                    exactly: Some(3)
                },
                cols: Bounds {
                    at_least: Some(4),
                    at_most: Some(5),
                    exactly: Some(6)
                },
                width: Bounds {
                    at_least: Some(7),
                    at_most: Some(8),
                    exactly: Some(9)
                },
                height: Bounds {
                    at_least: Some(10),
                    at_most: Some(11),
                    exactly: Some(12)
                },
            }
        );
    }

    #[test]
    fn no_flags_at_all_is_the_bare_probe() {
        let action = DimensionsAction::Probe {
            rows_at_least: None,
            rows_at_most: None,
            rows_exactly: None,
            cols_at_least: None,
            cols_at_most: None,
            cols_exactly: None,
            width_at_least: None,
            width_at_most: None,
            width_exactly: None,
            height_at_least: None,
            height_at_most: None,
            height_exactly: None,
            format: None,
        };
        assert!(thresholds_of(&action).is_empty());
        assert!(thresholds_of(&DimensionsAction::Explain { no_matrix: false }).is_empty());
    }

    #[test]
    fn the_feature_id_matches_the_database() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        assert_eq!(feature.id, FEATURE_ID);
        // The three queries this feature is about must all be documented.
        let sequences: Vec<&str> = feature
            .sequence
            .iter()
            .map(|sequence| sequence.request.as_str())
            .collect();
        for query in ["CSI 18 t", "CSI 14 t", "CSI 16 t"] {
            assert!(
                sequences.iter().any(|request| request.contains(query)),
                "no sequence documents {query}: {sequences:?}"
            );
        }
    }
}
