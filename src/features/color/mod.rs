//! Colour queries and change notification (OSC 4/10/11/12, DEC mode 2031).
//!
//! PLACEHOLDER — being implemented. The `run` signature is the contract.

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::{cli::ColorCommand, db::Catalog};

/// The feature id in the database. The subcommand is `color`.
pub const FEATURE_ID: &str = "color";

/// Entry point for `escapade color …`.
///
/// Returns an [`ExitCode`] because `--is-dark` reports its answer that way.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with.
pub fn run(_catalog: &Catalog, _command: &ColorCommand, _no_color: bool) -> Result<ExitCode> {
    bail!("`escapade color` is not implemented yet");
}
