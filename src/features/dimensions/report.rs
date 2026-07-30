//! `escapade dimensions`: the three sizes, and where each number came from.
//!
//! The provenance is not decoration. `TIOCGWINSZ` and `CSI 18 t` are two
//! different sources that can disagree — one of them can be stale after a resize
//! — and a pixel size is frequently unavailable altogether, in which case the
//! useful output is *why*, not a plausible-looking number. So every line names
//! its source, and every gap names what was tried.

use crate::{
    features::{
        dimensions::size::{Reading, Sizes},
        wrap,
    },
    theme::{Ink, Theme},
};

/// Column width for the value, so the three sources line up under each other.
const VALUE_WIDTH: usize = 16;

/// Prints the human-readable report.
pub fn render(theme: &Theme, sizes: &Sizes) {
    println!();
    println!("  {}", theme.paint("Terminal size", Ink::Title));
    println!();

    match sizes.cells() {
        Some(cells) => row(theme, "cells", &format!("{} × {}", cells.horizontal, cells.vertical), cells.origin.describe()),
        None => row(theme, "cells", "unavailable", &no_cells_reason(sizes)),
    }
    match sizes.window() {
        Some(window) => row(theme, "window", &pixels(window), window.origin.describe()),
        None => row(theme, "window", "unavailable", &no_pixels_reason(sizes, "CSI 14 t")),
    }
    match sizes.cell() {
        Some(cell) => row(theme, "cell", &pixels(cell), cell.origin.describe()),
        None => row(theme, "cell", "unavailable", &no_pixels_reason(sizes, "CSI 16 t")),
    }

    for note in notes(sizes) {
        println!();
        for line in wrap(&note, theme.width().saturating_sub(4)) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }

    println!();
    println!(
        "  {} {}",
        theme.paint("escapade dimensions explain", Ink::Code),
        theme.paint("for the queries behind these numbers", Ink::Muted)
    );
    println!();
}

/// One labelled line: what it is, the numbers, and where they came from.
fn row(theme: &Theme, label: &str, value: &str, source: &str) {
    let padding = VALUE_WIDTH.saturating_sub(value.chars().count());
    println!(
        "  {}{}{}  {}",
        theme.paint(&format!("{label:<8}"), Ink::Muted),
        theme.paint(value, Ink::Strong),
        " ".repeat(padding),
        theme.paint(source, Ink::Muted)
    );
}

/// `1280 × 384 px`.
fn pixels(reading: Reading) -> String {
    format!("{} × {} px", reading.horizontal, reading.vertical)
}

/// Why there is no size in cells — which is rare, and worth explaining when it
/// happens.
fn no_cells_reason(sizes: &Sizes) -> String {
    if sizes.queried && sizes.answered_barrier {
        "TIOCGWINSZ failed and the terminal ignored CSI 18 t".to_owned()
    } else if sizes.interactive {
        "TIOCGWINSZ failed and nothing answered CSI 18 t".to_owned()
    } else {
        "TIOCGWINSZ failed, and with output redirected there is nobody to ask".to_owned()
    }
}

/// Why a pixel size is missing, in terms of what was actually tried.
fn no_pixels_reason(sizes: &Sizes, query: &str) -> String {
    let ioctl = "TIOCGWINSZ left ws_xpixel and ws_ypixel at zero";
    if !sizes.interactive {
        return format!("{ioctl}, and with output redirected {query} cannot be asked");
    }
    if !sizes.queried {
        return format!("{ioctl}, and no query was needed for what was asked");
    }
    if sizes.answered_barrier {
        format!("{ioctl}, and the terminal answered the barrier but not {query}")
    } else {
        format!("{ioctl}, and nothing came back at all — not even the barrier")
    }
}

/// The caveats worth a paragraph of their own, in the order they matter.
fn notes(sizes: &Sizes) -> Vec<String> {
    let mut notes = Vec::new();

    if sizes.cells_disagree()
        && let (Some((ioctl_cols, ioctl_rows)), Some((reported_cols, reported_rows))) =
            (sizes.ioctl_cells, sizes.reported_cells)
    {
        notes.push(format!(
            "TIOCGWINSZ and CSI 18 t disagree: the ioctl says {ioctl_cols} × {ioctl_rows}, \
             the terminal reports {reported_cols} × {reported_rows}. One of them is stale — \
             usually a resize that has reached one and not the other. The ioctl is the one \
             shown above, because it is what the kernel will tell every other program too."
        ));
    }

    if let Some(multiplexer) = sizes.multiplexer {
        let name = multiplexer.name();
        if sizes.window().is_some() || sizes.cell().is_some() {
            notes.push(format!(
                "{name} is in the way, and it answers these queries from its own emulator rather \
                 than forwarding them. The pixel sizes above are {name}'s idea of the pane, which \
                 is the size an application drawing here actually has — but it is not necessarily \
                 what the real terminal window measures."
            ));
        } else {
            notes.push(format!(
                "{name} is in the way. It answers these queries itself and generally has no pixel \
                 size to report, which is why the pixel rows say unavailable rather than a number."
            ));
        }
    }

    if sizes.window().is_none() && sizes.cell().is_none() && sizes.multiplexer.is_none() {
        notes.push(
            "A missing pixel size is normal rather than a fault: CSI 16 t is skipped by \
             Alacritty, by VTE and by iTerm2, and the pixel fields of TIOCGWINSZ are documented \
             as unused on Linux. Anything that needs a cell size has to be prepared to assume one."
                .to_owned(),
        );
    }

    notes
}

/// One JSON object, one line — the house machine-readable shape.
///
/// Unknown sizes are `null` rather than zero or omitted: a script that gets a
/// number can trust it was measured, and one that gets `null` knows it has to
/// decide what to do about that.
#[must_use]
pub fn json_line(sizes: &Sizes) -> String {
    let multiplexer = sizes
        .multiplexer
        .map_or_else(|| "null".to_owned(), |m| format!("\"{}\"", m.name()));
    format!(
        "{{\"feature\":\"dimensions\",\"cells\":{},\"window\":{},\"cell\":{},\
         \"multiplexer\":{multiplexer},\"interactive\":{},\"queried\":{}}}\n",
        cells_json(sizes.cells()),
        pixels_json(sizes.window()),
        pixels_json(sizes.cell()),
        sizes.interactive,
        sizes.queried,
    )
}

/// `{"cols":80,"rows":24,"source":"tiocgwinsz"}`, or `null`.
fn cells_json(reading: Option<Reading>) -> String {
    reading.map_or_else(
        || "null".to_owned(),
        |reading| {
            format!(
                "{{\"cols\":{},\"rows\":{},\"source\":\"{}\"}}",
                reading.horizontal,
                reading.vertical,
                reading.origin.as_str()
            )
        },
    )
}

/// `{"width":1280,"height":384,"source":"csi-14-t"}`, or `null`.
fn pixels_json(reading: Option<Reading>) -> String {
    reading.map_or_else(
        || "null".to_owned(),
        |reading| {
            format!(
                "{{\"width\":{},\"height\":{},\"source\":\"{}\"}}",
                reading.horizontal,
                reading.vertical,
                reading.origin.as_str()
            )
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::term::Multiplexer;

    fn everything() -> Sizes {
        Sizes {
            ioctl_cells: Some((80, 24)),
            ioctl_pixels: Some((800, 480)),
            interactive: true,
            ..Sizes::default()
        }
    }

    #[test]
    fn the_json_shape_names_every_number_and_its_source() {
        let line = json_line(&everything());
        assert!(line.ends_with('\n'), "one object, one line");
        assert!(line.contains("\"feature\":\"dimensions\""));
        assert!(line.contains("\"cells\":{\"cols\":80,\"rows\":24,\"source\":\"tiocgwinsz\"}"));
        assert!(line.contains("\"window\":{\"width\":800,\"height\":480,\"source\":\"tiocgwinsz\"}"));
        assert!(line.contains("\"cell\":{\"width\":10,\"height\":20,\"source\":\"derived\"}"));
        assert!(line.contains("\"multiplexer\":null"));
        assert!(line.contains("\"interactive\":true"));
        assert!(line.contains("\"queried\":false"));
    }

    #[test]
    fn an_unknown_size_is_null_rather_than_zero() {
        let sizes = Sizes {
            ioctl_cells: Some((80, 24)),
            queried: true,
            answered_barrier: true,
            interactive: true,
            ..Sizes::default()
        };
        let line = json_line(&sizes);
        assert!(line.contains("\"window\":null"), "{line}");
        assert!(line.contains("\"cell\":null"), "{line}");
        assert!(!line.contains(":0"), "a zero would read as a measurement: {line}");
    }

    #[test]
    fn nothing_known_is_a_valid_object_of_nulls() {
        let line = json_line(&Sizes::default());
        assert!(line.contains("\"cells\":null"));
        assert!(line.contains("\"window\":null"));
        assert!(line.contains("\"cell\":null"));
        assert!(line.contains("\"interactive\":false"));
    }

    #[test]
    fn a_multiplexer_is_named_in_the_json() {
        let sizes = Sizes {
            multiplexer: Some(Multiplexer::Tmux),
            ..everything()
        };
        assert!(json_line(&sizes).contains("\"multiplexer\":\"tmux\""));
    }

    #[test]
    fn the_json_carries_no_control_bytes() {
        // It is printed straight to stdout, so a stray ESC would be executed.
        let sizes = Sizes {
            multiplexer: Some(Multiplexer::Screen),
            ..everything()
        };
        let line = json_line(&sizes);
        assert!(!line.contains('\u{1b}'));
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn a_missing_pixel_size_explains_which_step_failed() {
        let redirected = Sizes {
            ioctl_cells: Some((80, 24)),
            ..Sizes::default()
        };
        assert!(no_pixels_reason(&redirected, "CSI 16 t").contains("redirected"));

        let answered = Sizes {
            interactive: true,
            queried: true,
            answered_barrier: true,
            ..redirected
        };
        assert!(no_pixels_reason(&answered, "CSI 16 t").contains("barrier but not CSI 16 t"));

        let silent = Sizes {
            answered_barrier: false,
            ..answered
        };
        assert!(no_pixels_reason(&silent, "CSI 14 t").contains("nothing came back"));

        let unasked = Sizes {
            queried: false,
            ..answered
        };
        assert!(no_pixels_reason(&unasked, "CSI 14 t").contains("no query was needed"));

        // Every explanation blames the ioctl's pixel fields too, since that is the
        // rung a reader would otherwise expect to have worked.
        for sizes in [redirected, answered, silent, unasked] {
            assert!(no_pixels_reason(&sizes, "CSI 14 t").contains("ws_xpixel"));
        }
    }

    #[test]
    fn a_disagreement_between_the_two_cell_sources_is_reported() {
        let sizes = Sizes {
            reported_cells: Some((100, 30)),
            ..everything()
        };
        let reported = notes(&sizes);
        assert!(reported.iter().any(|note| note.contains("disagree")), "{reported:?}");
        assert!(reported.iter().any(|note| note.contains("100 × 30")), "{reported:?}");
        assert!(notes(&everything()).iter().all(|note| !note.contains("disagree")));
    }

    #[test]
    fn a_multiplexer_gets_the_honest_caveat() {
        let with_pixels = Sizes {
            multiplexer: Some(Multiplexer::Tmux),
            ..everything()
        };
        let inside = notes(&with_pixels);
        assert!(inside.iter().any(|note| note.contains("its own emulator")), "{inside:?}");

        let without = Sizes {
            multiplexer: Some(Multiplexer::Tmux),
            ioctl_pixels: None,
            ..everything()
        };
        let no_pixels = notes(&without);
        assert!(
            no_pixels.iter().any(|note| note.contains("no pixel size to report")),
            "{no_pixels:?}"
        );
    }

    #[test]
    fn a_terminal_with_no_pixel_size_is_told_that_this_is_normal() {
        let sizes = Sizes {
            ioctl_pixels: None,
            queried: true,
            answered_barrier: true,
            ..everything()
        };
        let reported = notes(&sizes);
        assert!(reported.iter().any(|note| note.contains("Alacritty")), "{reported:?}");
        // But not when everything is known — there is nothing to excuse.
        assert!(notes(&everything()).iter().all(|note| !note.contains("Alacritty")));
    }
}
