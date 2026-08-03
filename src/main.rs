//! escapade — demonstrate, visualize, play with and actually use modern
//! terminal features.
//!
//! Terminals have grown a lot of capabilities that almost nothing takes
//! advantage of: they are undocumented, unevenly supported, and invisible until
//! something renders wrong. Each feature here gets the same treatment — an
//! `explain` that documents it, a `probe` that asks your terminal whether it
//! implements it, and a `demo` that shows it working while naming every byte it
//! sends.
//!
//! The layout:
//!
//! * [`db`] — the feature database, one TOML file per feature, in the spirit of
//!   <https://webassembly.org/features/>.
//! * [`term`] — raw mode, the alternate screen, private modes.
//! * [`input`] — parsing keys, mouse reports and terminal replies out of the
//!   byte stream.
//! * [`theme`] — colors and markdown rendering.
//! * [`unicode`] — codepoint names, categories, grapheme clustering, widths.
//! * [`features`] — one module per feature.

mod cli;
mod db;
mod decrqm;
mod features;
mod input;
mod term;
mod theme;
mod unicode;

use std::process::ExitCode;

use clap::Parser as _;

use crate::cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Any demo can leave the terminal in raw mode on the alternate screen, so
    // the hook goes in before anything touches it.
    term::install_panic_hook();

    match run(&cli) {
        Ok(code) => code,
        Err(error) => {
            // `{:#}` prints the whole anyhow context chain on one line.
            eprintln!("escapade: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatches to a feature and reports the process exit status.
///
/// Most commands succeed or fail, so their status is just success. `doctor` is
/// different: its exit code carries the finding, because a script wants to branch
/// on whether the terminal is healthy without parsing any output.
fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    let catalog = db::Catalog::load()?;
    // Every feature returns an exit status, uniformly. Most only ever return
    // success; the ones whose finding *is* the answer — `probe`, `doctor`,
    // `color --is-dark` — return 1 or 2, so a script can branch without parsing.
    match &cli.command {
        Command::GlyphProtocol(command) => {
            features::glyph_protocol::run(&catalog, command, cli.no_color)
        }
        Command::Graphics(command) => features::kitty_graphics::run(&catalog, command, cli.no_color),
        Command::Grapheme(command) => features::grapheme::run(&catalog, command, cli.no_color),
        Command::Hyperlink(command) => features::hyperlink::run(&catalog, command, cli.no_color),
        Command::Notification(command) => {
            features::notification::run(&catalog, command, cli.no_color)
        }
        Command::Progress(command) => features::progress::run(&catalog, command, cli.no_color),
        Command::Textsize(command) => features::textsize::run(&catalog, command, cli.no_color),
        Command::Color(command) => features::color::run(&catalog, command, cli.no_color),
        Command::Clipboard(command) => features::clipboard::run(&catalog, command, cli.no_color),
        Command::Style(command) => features::style::run(&catalog, command, cli.no_color),
        Command::Dimensions(command) => {
            features::dimensions::run(&catalog, command, cli.no_color)
        }
        Command::Unscroll(command) => features::unscroll::run(&catalog, command, cli.no_color),
        Command::Keys(command) => features::keys::run(&catalog, command, cli.no_color),
        Command::Decode(command) => features::decode::run(&catalog, command, cli.no_color),
        Command::Doctor(command) => features::doctor::run(command, cli.no_color),
        Command::List => {
            features::list(&catalog, cli.no_color);
            Ok(ExitCode::SUCCESS)
        }
    }
}
