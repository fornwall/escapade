//! The size model: every number this feature can learn, where it came from, and
//! which source to believe when two of them disagree.
//!
//! Three quantities are wanted, and each has its own chain of sources:
//!
//! | quantity | preferred | fallback | last resort |
//! | --- | --- | --- | --- |
//! | size in cells | `TIOCGWINSZ` | `CSI 18 t` | — |
//! | window pixels | `TIOCGWINSZ` | `CSI 14 t` | cell size × cells |
//! | cell pixels | `CSI 16 t` | window pixels ÷ cells | — |
//!
//! `TIOCGWINSZ` leads because it needs no round trip and cannot be swallowed by
//! a multiplexer: the kernel hands over the numbers the terminal already pushed
//! into the pty. Its two pixel fields are the weak point — Linux's `tty_ioctl(4)`
//! calls `ws_xpixel` and `ws_ypixel` "unused", and while most Unix terminals do
//! fill them in, VTE-based terminals, GNU Screen and everything on Windows do
//! not. That is why the escape-sequence rungs exist at all.
//!
//! Everything here is pure, so the whole fallback chain is unit-testable without
//! a terminal. The bytes go out in [`super::query`].

use crate::{
    // Reused rather than reimplemented: `kitty_graphics::layout` already parses
    // the `CSI 4 …` and `CSI 6 …` reports for image placement, and a second copy
    // would be a second place to get the height-before-width order wrong. These
    // three parsers really want to live together in `term`; see the module docs
    // of `super` for why they do not yet.
    features::kitty_graphics::layout::{parse_cell_pixels, parse_window_pixels},
    term::Multiplexer,
};

/// How a number was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The `TIOCGWINSZ` ioctl, reached through `crossterm`. No round trip, and a
    /// multiplexer cannot filter it.
    Ioctl,
    /// `CSI 18 t`, answered with `CSI 8 ; rows ; cols t`.
    CellsReport,
    /// `CSI 14 t`, answered with `CSI 4 ; height ; width t`.
    WindowReport,
    /// `CSI 16 t`, answered with `CSI 6 ; height ; width t`.
    CellReport,
    /// Arithmetic on two other readings, not a measurement of its own.
    Derived,
}

impl Origin {
    /// Stable machine-readable name, for `--format json`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ioctl => "tiocgwinsz",
            Self::CellsReport => "csi-18-t",
            Self::WindowReport => "csi-14-t",
            Self::CellReport => "csi-16-t",
            Self::Derived => "derived",
        }
    }

    /// How the number was arrived at, for a person.
    ///
    /// This is the genuinely useful half of the report: `TIOCGWINSZ` and
    /// `CSI 18 t` can disagree, and a derived cell size is off by however much
    /// padding the terminal puts around its text area.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Ioctl => "TIOCGWINSZ — the kernel already knew, no query sent",
            Self::CellsReport => "reported by the terminal (CSI 18 t)",
            Self::WindowReport => "reported by the terminal (CSI 14 t), text area only",
            Self::CellReport => "reported by the terminal (CSI 16 t)",
            Self::Derived => "derived by division, so padding makes it approximate",
        }
    }

    /// Whether the number was measured rather than calculated from two others.
    #[must_use]
    pub fn is_measured(self) -> bool {
        self != Self::Derived
    }
}

/// A pair of numbers that were measured together, and where they came from.
///
/// One type serves all three quantities, because the fallback chain and the
/// reporting are the same shape for each. The height-before-width trap lives in
/// the parsing, not here: by the time a value reaches this struct it is always
/// horizontal first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    /// Horizontal extent: columns for a size in cells, pixels for a pixel size.
    pub horizontal: u32,
    /// Vertical extent: rows for a size in cells, pixels for a pixel size.
    pub vertical: u32,
    /// Which rung of the chain answered.
    pub origin: Origin,
}

impl Reading {
    /// A reading, or `None` when either number is zero.
    ///
    /// A zero is never a real size, and terminals do emit them. xterm's own
    /// documentation notes that some terminals answer `0` for width and height
    /// and that "such terminals should be modified", and tmux is one of them: it
    /// answers `CSI 16 t` unconditionally from its own state, so a pane whose
    /// cell size tmux never learned gets a well-formed `CSI 6 ; 0 ; 0 t`.
    /// Treating that as unknown is both honest and safer than dividing by it.
    #[must_use]
    pub fn new(horizontal: u32, vertical: u32, origin: Origin) -> Option<Self> {
        if horizontal == 0 || vertical == 0 {
            return None;
        }
        Some(Self {
            horizontal,
            vertical,
            origin,
        })
    }
}

/// Every raw source, kept separately so the report can say who said what.
///
/// The fields are what was *heard*; [`Sizes::cells`], [`Sizes::window`] and
/// [`Sizes::cell`] apply the preference order to them. Keeping the two apart is
/// what makes it possible to notice that `TIOCGWINSZ` and `CSI 18 t` disagree
/// instead of silently picking one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sizes {
    /// Columns and rows from `TIOCGWINSZ`.
    pub ioctl_cells: Option<(u32, u32)>,
    /// `ws_xpixel` and `ws_ypixel` from `TIOCGWINSZ`, when they are not zero.
    pub ioctl_pixels: Option<(u32, u32)>,
    /// Columns and rows from `CSI 18 t`.
    pub reported_cells: Option<(u32, u32)>,
    /// Text-area width and height in pixels from `CSI 14 t`.
    pub reported_window: Option<(u32, u32)>,
    /// Cell width and height in pixels from `CSI 16 t`.
    pub reported_cell: Option<(u32, u32)>,
    /// A multiplexer between this process and the real terminal, if any.
    pub multiplexer: Option<Multiplexer>,
    /// Whether stdin and stdout are both terminals, without which nothing can be
    /// asked.
    pub interactive: bool,
    /// Whether the queries were actually sent.
    pub queried: bool,
    /// Whether the `CSI c` barrier came back, which is what licenses "this
    /// terminal does not answer that query" rather than "nothing came back".
    pub answered_barrier: bool,
}

impl Sizes {
    /// The size in cells: `TIOCGWINSZ` first, then `CSI 18 t`.
    #[must_use]
    pub fn cells(&self) -> Option<Reading> {
        self.ioctl_cells
            .and_then(|(cols, rows)| Reading::new(cols, rows, Origin::Ioctl))
            .or_else(|| {
                self.reported_cells
                    .and_then(|(cols, rows)| Reading::new(cols, rows, Origin::CellsReport))
            })
    }

    /// The window's pixel size: `TIOCGWINSZ`, then `CSI 14 t`, then the cell size
    /// multiplied back up.
    ///
    /// The last rung is for the terminal that answers `CSI 16 t` but not
    /// `CSI 14 t`. It is marked [`Origin::Derived`] because it is the *text
    /// area* rounded to whole cells, which is not quite the window: `CSI 14 t`
    /// reports the text area too, and only `CSI 14 ; 2 t` includes the window
    /// manager's frame and the scroll bar.
    #[must_use]
    pub fn window(&self) -> Option<Reading> {
        if let Some(reading) = self
            .ioctl_pixels
            .and_then(|(width, height)| Reading::new(width, height, Origin::Ioctl))
            .or_else(|| {
                self.reported_window
                    .and_then(|(width, height)| Reading::new(width, height, Origin::WindowReport))
            })
        {
            return Some(reading);
        }
        let (cell_width, cell_height) = self.reported_cell?;
        let cells = self.cells()?;
        Reading::new(
            cell_width.checked_mul(cells.horizontal)?,
            cell_height.checked_mul(cells.vertical)?,
            Origin::Derived,
        )
    }

    /// The size of one cell in pixels: `CSI 16 t`, else the window divided by the
    /// grid.
    ///
    /// The division is the interesting rung, because `CSI 16 t` is the query
    /// terminals most often skip — Alacritty, VTE and iTerm2 among them. It is
    /// accurate to a pixel or two, depending on how much padding the terminal
    /// keeps around its text area, and never derived from an already-derived
    /// window size.
    #[must_use]
    pub fn cell(&self) -> Option<Reading> {
        if let Some(reading) = self
            .reported_cell
            .and_then(|(width, height)| Reading::new(width, height, Origin::CellReport))
        {
            return Some(reading);
        }
        let (width, height) = self.measured_window()?;
        let cells = self.cells()?;
        Reading::new(
            width / cells.horizontal,
            height / cells.vertical,
            Origin::Derived,
        )
    }

    /// The window pixel size, but only from a real measurement — the rung
    /// [`Sizes::cell`] is allowed to divide, without the circularity of dividing
    /// a size that was itself multiplied up from a cell.
    fn measured_window(&self) -> Option<(u32, u32)> {
        self.ioctl_pixels.or(self.reported_window)
    }

    /// Whether `TIOCGWINSZ` and `CSI 18 t` gave different cell counts.
    ///
    /// Worth saying out loud when it happens: the usual cause is a resize that
    /// the terminal has told one of them about and not the other, and it means
    /// one of the two numbers on screen is stale.
    #[must_use]
    pub fn cells_disagree(&self) -> bool {
        match (self.ioctl_cells, self.reported_cells) {
            (Some(ioctl), Some(reported)) => ioctl != reported,
            _ => false,
        }
    }

    /// Which of the three queries answered, in the order they are sent.
    ///
    /// Only meaningful once [`Sizes::queried`] is set; before that, every entry
    /// is false because nothing was asked.
    #[must_use]
    pub fn answers(&self) -> [(&'static str, bool); 3] {
        [
            (Origin::CellsReport.as_str(), self.reported_cells.is_some()),
            (Origin::WindowReport.as_str(), self.reported_window.is_some()),
            (Origin::CellReport.as_str(), self.reported_cell.is_some()),
        ]
    }
}

/// Parses `CSI 8 ; <rows> ; <cols> t` — the answer to `CSI 18 t` — into
/// `(cols, rows)`.
///
/// **Note the order.** Every one of xterm's size reports puts the *vertical*
/// number first: `CSI 8 ; height ; width t` for the text area in characters,
/// `CSI 4 ; height ; width t` for it in pixels, `CSI 6 ; height ; width t` for a
/// cell. Reading them as width-then-height transposes the answer, and on a
/// terminal that happens to be square it does so invisibly.
#[must_use]
pub fn parse_text_area_cells(params: &str) -> Option<(u32, u32)> {
    let mut fields = params.split(';');
    if fields.next()?.trim().parse::<u32>().ok()? != 8 {
        return None;
    }
    let rows = fields.next()?.trim().parse::<u32>().ok()?;
    let cols = fields.next()?.trim().parse::<u32>().ok()?;
    if cols == 0 || rows == 0 {
        return None;
    }
    Some((cols, rows))
}

/// Folds one `CSI … t` reply into `sizes`, whichever of the three it is.
///
/// Returns whether the reply was recognised, so a caller can tell a size report
/// apart from the other things that end in `t`.
pub fn absorb_report(sizes: &mut Sizes, params: &str) -> bool {
    if let Some(cells) = parse_text_area_cells(params) {
        sizes.reported_cells = Some(cells);
        return true;
    }
    if let Some(window) = parse_window_pixels(params) {
        sizes.reported_window = Some(window);
        return true;
    }
    if let Some(cell) = parse_cell_pixels(params) {
        sizes.reported_cell = Some(cell);
        return true;
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// An 80x24 terminal of 10x20 cells, as `TIOCGWINSZ` would give it.
    fn from_ioctl() -> Sizes {
        Sizes {
            ioctl_cells: Some((80, 24)),
            ioctl_pixels: Some((800, 480)),
            interactive: true,
            ..Sizes::default()
        }
    }

    #[test]
    fn every_size_report_puts_the_vertical_number_first() {
        // Verified against xterm's ctlseqs, "Window manipulation" (dtterm):
        //   CSI 18 t -> CSI 8 ; height ; width t   (characters)
        //   CSI 14 t -> CSI 4 ; height ; width t   (pixels, text area)
        //   CSI 16 t -> CSI 6 ; height ; width t   (pixels, one cell)
        assert_eq!(parse_text_area_cells("8;24;80"), Some((80, 24)));
        assert_eq!(parse_window_pixels("4;1080;1920"), Some((1920, 1080)));
        assert_eq!(parse_cell_pixels("6;20;10"), Some((10, 20)));
    }

    #[test]
    fn a_report_of_the_wrong_kind_is_not_a_cell_count() {
        assert_eq!(parse_text_area_cells("4;1080;1920"), None, "that is pixels");
        assert_eq!(parse_text_area_cells("6;20;10"), None, "that is a cell");
        assert_eq!(parse_text_area_cells("9;24;80"), None, "that is the screen");
        assert_eq!(parse_text_area_cells("8;24"), None, "truncated");
        assert_eq!(parse_text_area_cells("8;x;80"), None, "not a number");
        assert_eq!(parse_text_area_cells(""), None);
        // Some terminals pad their parameters.
        assert_eq!(parse_text_area_cells(" 8 ; 24 ; 80 "), Some((80, 24)));
    }

    #[test]
    fn a_zero_is_treated_as_no_answer_at_all() {
        assert_eq!(parse_text_area_cells("8;0;80"), None);
        assert_eq!(Reading::new(0, 10, Origin::Ioctl), None);
        assert_eq!(Reading::new(10, 0, Origin::Ioctl), None);
        // The ioctl's pixel fields are zero on most terminals, and a 0x0 window
        // must not be reported as a measurement.
        let sizes = Sizes {
            ioctl_cells: Some((80, 24)),
            ioctl_pixels: Some((0, 0)),
            ..Sizes::default()
        };
        assert_eq!(sizes.window(), None);
        assert_eq!(sizes.cell(), None);
    }

    #[test]
    fn the_ioctl_wins_over_a_report_because_it_needs_no_round_trip() {
        let sizes = Sizes {
            reported_cells: Some((100, 30)),
            ..from_ioctl()
        };
        let cells = sizes.cells().unwrap();
        assert_eq!((cells.horizontal, cells.vertical), (80, 24));
        assert_eq!(cells.origin, Origin::Ioctl);
        assert!(sizes.cells_disagree(), "and the disagreement is reported");
    }

    #[test]
    fn a_report_answers_when_the_ioctl_could_not() {
        let sizes = Sizes {
            reported_cells: Some((100, 30)),
            reported_window: Some((1000, 600)),
            interactive: true,
            queried: true,
            ..Sizes::default()
        };
        let cells = sizes.cells().unwrap();
        assert_eq!((cells.horizontal, cells.vertical), (100, 30));
        assert_eq!(cells.origin, Origin::CellsReport);
        assert_eq!(sizes.window().unwrap().origin, Origin::WindowReport);
        assert!(!sizes.cells_disagree(), "one source cannot disagree");
    }

    #[test]
    fn a_cell_size_is_derived_when_csi_16_t_is_skipped() {
        // Alacritty, VTE and iTerm2 all answer CSI 14 t and ignore CSI 16 t, so
        // this is the common case rather than an exotic one.
        let sizes = from_ioctl();
        let cell = sizes.cell().unwrap();
        assert_eq!((cell.horizontal, cell.vertical), (10, 20));
        assert_eq!(cell.origin, Origin::Derived);
        assert!(!cell.origin.is_measured());
    }

    #[test]
    fn a_reported_cell_size_beats_a_derived_one() {
        let sizes = Sizes {
            reported_cell: Some((9, 19)),
            ..from_ioctl()
        };
        let cell = sizes.cell().unwrap();
        assert_eq!((cell.horizontal, cell.vertical), (9, 19));
        assert_eq!(cell.origin, Origin::CellReport);
        assert!(cell.origin.is_measured());
    }

    #[test]
    fn a_window_size_can_be_multiplied_back_up_from_a_cell() {
        // The terminal that answers CSI 16 t but not CSI 14 t.
        let sizes = Sizes {
            ioctl_cells: Some((80, 24)),
            reported_cell: Some((10, 20)),
            interactive: true,
            queried: true,
            ..Sizes::default()
        };
        let window = sizes.window().unwrap();
        assert_eq!((window.horizontal, window.vertical), (800, 480));
        assert_eq!(window.origin, Origin::Derived);
    }

    #[test]
    fn a_derived_window_is_never_divided_back_into_a_cell() {
        // Otherwise the two derivations would feed each other and a cell size
        // would look like it had been measured twice.
        let sizes = Sizes {
            ioctl_cells: Some((80, 24)),
            reported_cell: Some((10, 20)),
            ..Sizes::default()
        };
        assert_eq!(sizes.cell().unwrap().origin, Origin::CellReport);
        assert_eq!(sizes.measured_window(), None);
    }

    #[test]
    fn pixels_stay_unknown_when_nothing_reports_them() {
        // The overwhelmingly common case: cells are known, pixels are not.
        let sizes = Sizes {
            ioctl_cells: Some((80, 24)),
            interactive: true,
            queried: true,
            answered_barrier: true,
            ..Sizes::default()
        };
        assert!(sizes.cells().is_some());
        assert_eq!(sizes.window(), None, "no pixel source at all");
        assert_eq!(sizes.cell(), None, "and nothing to divide");
    }

    #[test]
    fn nothing_at_all_is_known_without_a_terminal() {
        let sizes = Sizes::default();
        assert_eq!(sizes.cells(), None);
        assert_eq!(sizes.window(), None);
        assert_eq!(sizes.cell(), None);
        assert!(sizes.answers().iter().all(|&(_, answered)| !answered));
    }

    #[test]
    fn absorb_sorts_the_three_replies_by_their_leading_parameter() {
        let mut sizes = Sizes::default();
        assert!(absorb_report(&mut sizes, "8;24;80"));
        assert!(absorb_report(&mut sizes, "4;480;800"));
        assert!(absorb_report(&mut sizes, "6;20;10"));
        assert_eq!(sizes.reported_cells, Some((80, 24)));
        assert_eq!(sizes.reported_window, Some((800, 480)));
        assert_eq!(sizes.reported_cell, Some((10, 20)));
        assert!(sizes.answers().iter().all(|&(_, answered)| answered));
        // Anything else that ends in `t` is somebody else's reply.
        assert!(!absorb_report(&mut sizes, "9;24;80"));
        assert!(!absorb_report(&mut sizes, "2"));
    }

    #[test]
    fn origins_have_distinct_names_and_descriptions() {
        let origins = [
            Origin::Ioctl,
            Origin::CellsReport,
            Origin::WindowReport,
            Origin::CellReport,
            Origin::Derived,
        ];
        let mut names: Vec<&str> = origins.iter().map(|origin| origin.as_str()).collect();
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total, "two origins share a name");
        for origin in origins {
            assert!(!origin.describe().is_empty());
        }
        assert!(Origin::Ioctl.is_measured());
        assert!(!Origin::Derived.is_measured());
    }
}
