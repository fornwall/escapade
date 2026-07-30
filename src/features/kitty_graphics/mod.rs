//! The kitty graphics protocol: real pixels in a terminal.
//!
//! Every command is an APC string:
//!
//! ```text
//! ESC _ G <control data> ; <base64 payload> ESC \
//! ```
//!
//! The control data is a comma-separated list of `key=value` pairs and the payload
//! is base64-encoded binary. Because APC is unknown to terminals that do not
//! implement it, they drop the entire string silently — which is what makes it safe
//! to speak the protocol optimistically, exactly as with the Glyph Protocol.
//!
//! Three ideas are worth knowing before reading any of the submodules:
//!
//! * **The terminal does not decode image formats.** It understands RGB, RGBA and
//!   PNG, and nothing else. Every other format is the client's problem, which is
//!   why [`source`] exists.
//! * **Transmission and display are separate.** An image is transmitted once and
//!   *placed* as many times as you like — at different sizes, cropped differently,
//!   at different depths. A file manager showing a directory of thumbnails
//!   transmits each one once.
//! * **Images are positioned at the cursor, not at a pixel.** There is no "draw at
//!   (x, y)"; there is "draw here, scaled into this many cells, offset by less than
//!   one cell". Which is also why Unicode placeholders exist: they turn an image
//!   into text, so everything above the terminal moves it correctly.
//!
//! The modules:
//!
//! * [`proto`] — the wire format: typed control keys, chunking, reply parsing.
//! * [`placeholder`] — U+10EEEE and the row/column diacritics.
//! * [`layout`] — cell size detection and the `--fit` arithmetic.
//! * [`pixels`] — straight RGBA, resampling, alpha compositing.
//! * [`png`] — reading a PNG's dimensions without decoding it.
//! * [`shapes`] — images generated from arithmetic.
//! * [`sniff`] — working out what an input is, when nothing said.
//! * [`source`] — bytes in, pixels or PNG bytes out.
//! * [`flamegraph`] — profiler output in, an SVG of a flame graph out.
//! * [`probe`], [`render`], [`demo`] — the three verbs.
//!
//! Specification: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>

pub mod demo;
pub mod flamegraph;
pub mod layout;
pub mod pixels;
pub mod placeholder;
pub mod png;
pub mod probe;
pub mod proto;
pub mod render;
pub mod shapes;
pub mod sniff;
pub mod source;

use std::process::ExitCode;

use anyhow::Result;

use crate::{
    cli::{FeatureCommand, GraphicsAction},
    db::Catalog,
    theme::Theme,
};

/// The feature id in the database. The subcommand is `graphics`.
pub const FEATURE_ID: &str = "kitty-graphics";

/// Entry point for `escapade graphics …`.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with.
pub fn run(
    catalog: &Catalog,
    command: &FeatureCommand<GraphicsAction>,
    no_color: bool,
) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        GraphicsAction::Explain { no_matrix } => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
        }
        // The only verb with something to say through the exit status: it is a
        // predicate, so it answers there and prints only when asked to.
        GraphicsAction::Probe { format } => return probe::report(&theme, *format),
        GraphicsAction::Demo {
            force,
            only,
            step,
            list,
        } => {
            if *list {
                demo::list_steps(&theme);
            } else {
                demo::run(&theme, *force, *step, only.as_deref())?;
            }
        }
        GraphicsAction::Render {
            file,
            input_type,
            shape,
            cols,
            rows,
            fit,
            scale,
            dpi,
            layout,
            background,
            z,
            placeholder,
            dry_run,
            force,
            flamegraph,
        } => render::run(
            &theme,
            &render::Options {
                file: file.as_deref(),
                input_type: *input_type,
                shape: *shape,
                cols: *cols,
                rows: *rows,
                fit: *fit,
                scale: *scale,
                dpi: *dpi,
                layout,
                background: background.as_deref(),
                z: *z,
                placeholder: *placeholder,
                dry_run: *dry_run,
                force: *force,
                flamegraph,
            },
        )?,
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_feature_id_resolves_to_a_data_file() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        assert_eq!(feature.id, FEATURE_ID);
    }
}
