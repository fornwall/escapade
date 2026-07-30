//! Clipboard access from a remote host (OSC 52).
//!
//! PLACEHOLDER — being implemented. The `run` signature is the contract.

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::{cli::ClipboardCommand, db::Catalog};

/// The feature id in the database. The subcommand is `clipboard`.
pub const FEATURE_ID: &str = "clipboard";

/// Entry point for `escapade clipboard …`.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with.
pub fn run(_catalog: &Catalog, _command: &ClipboardCommand, _no_color: bool) -> Result<ExitCode> {
    bail!("`escapade clipboard` is not implemented yet");
}
