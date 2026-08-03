//! How many cells, and which pixels: turning `--cols`, `--rows`, `--fit` and
//! `--scale` into `c`, `r`, `x`, `y`, `w` and `h`.
//!
//! Two things make this more than a division. The first is that the answer
//! depends on the size of a **cell in pixels**, which is a property of the
//! terminal and the user's font, not of the image — and the only way to learn it
//! is to ask. The second is that scaling should be done by the *terminal*
//! wherever possible: sending full-resolution pixels and a `c`/`r` pair lets a
//! terminal on a HiDPI screen render the image at its real device resolution,
//! while resampling here would throw those pixels away before they ever arrived.
//!
//! So the rule is: compute cell counts, send the original pixels, and only
//! resample when the payload would be grossly larger than anything the placement
//! could show.

use std::time::Duration;

use anyhow::Result;

use crate::{
    cli::Fit,
    features::kitty_graphics::proto::Rect,
    input::{InputReader, Reply},
    term::{CSI, Term},
};

/// Cell size assumed when the terminal will not say: a typical 8×16 bitmap cell.
///
/// Being wrong here is not fatal — it changes how many cells an image is asked to
/// occupy, not whether it renders — but it does mean the aspect ratio can come out
/// stretched, so `render` says out loud when it had to guess.
pub const FALLBACK_CELL: (u32, u32) = (8, 16);

/// How long to wait for a size report.
const REPLY_TIMEOUT: Duration = Duration::from_millis(200);

/// How much bigger than the placement the source has to be before escapade
/// resamples it itself, as a linear factor per axis.
///
/// Below this, the extra pixels are useful: a terminal on a HiDPI display renders
/// the placement at twice the nominal cell size and wants them. Above it, they are
/// just base64 nobody will ever see.
pub const RESAMPLE_THRESHOLD: u32 = 2;

/// Where a cell size came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellSource {
    /// `CSI 16 t` — the terminal reported the cell size directly. Exact, but
    /// implemented in fewer terminals.
    CellReport,
    /// `CSI 14 t` — the terminal reported the window size in pixels, divided by
    /// the grid size. Accurate to a pixel or two, depending on padding.
    WindowReport,
    /// Nobody answered, so [`FALLBACK_CELL`] was assumed.
    Assumed,
}

impl CellSource {
    /// How the number was arrived at, for the report.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::CellReport => "reported by the terminal (CSI 16 t)",
            Self::WindowReport => "derived from the window size (CSI 14 t)",
            Self::Assumed => "assumed — the terminal did not answer either size query",
        }
    }

    /// Lowercase word for machine-readable output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CellReport => "cell-report",
            Self::WindowReport => "window-report",
            Self::Assumed => "assumed",
        }
    }

    /// Whether the value was measured rather than guessed.
    #[must_use]
    pub fn is_measured(self) -> bool {
        !matches!(self, Self::Assumed)
    }
}

/// The size of one terminal cell, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSize {
    /// Cell width in pixels. Never zero.
    pub width: u32,
    /// Cell height in pixels. Never zero.
    pub height: u32,
    /// How this was determined.
    pub source: CellSource,
}

impl Default for CellSize {
    fn default() -> Self {
        Self {
            width: FALLBACK_CELL.0,
            height: FALLBACK_CELL.1,
            source: CellSource::Assumed,
        }
    }
}

impl CellSize {
    /// A measured cell size, falling back when either dimension is nonsense.
    ///
    /// Terminals that do not implement the query sometimes answer `0`, which the
    /// spec explicitly calls out as a bug; treating it as "unknown" is kinder
    /// than dividing by it.
    #[must_use]
    pub fn measured(width: u32, height: u32, source: CellSource) -> Self {
        if width == 0 || height == 0 {
            return Self::default();
        }
        Self {
            width,
            height,
            source,
        }
    }
}

/// Asks the terminal how big a cell is.
///
/// Two queries go out together, most precise first:
///
/// * `CSI 16 t` — answered with `CSI 6 ; <height> ; <width> t`, the size of a
///   single cell in pixels. Exactly what we want, supported by fewer terminals.
/// * `CSI 14 t` — answered with `CSI 4 ; <height> ; <width> t`, the size of the
///   text area in pixels, which divided by the grid gives the cell size.
///
/// Neither is part of the graphics protocol; both long predate it. A terminal
/// that answers neither leaves [`CellSize::default`] in place.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. Silence is not
/// an error.
pub fn query_cell_size(term: &mut Term, input: &mut InputReader) -> Result<CellSize> {
    term.enable_raw()?;
    input.drain();
    // Both queries at once: the terminal answers in order, so a terminal that
    // knows only one of them still gets us an answer in one round trip.
    term.write_now(&format!("{CSI}16t{CSI}14t"))?;

    let mut result = CellSize::default();
    let (cols, rows) = (u32::from(term.cols.max(1)), u32::from(term.rows.max(1)));
    // Two replies at most, and the first is the one we prefer.
    for _ in 0..2 {
        let Ok(Some(Reply::Csi { params, .. })) = input.wait_for_reply(REPLY_TIMEOUT, |reply| {
            matches!(
                reply,
                Reply::Csi {
                    final_byte: 't',
                    ..
                }
            )
        }) else {
            break;
        };
        if let Some((width, height)) = parse_cell_pixels(&params) {
            return Ok(CellSize::measured(width, height, CellSource::CellReport));
        }
        if let Some((width, height)) = parse_window_pixels(&params)
            && result.source == CellSource::Assumed
        {
            result = CellSize::measured(width / cols, height / rows, CellSource::WindowReport);
        }
    }
    Ok(result)
}

/// Parses `CSI 6 ; <height> ; <width> t` — the answer to `CSI 16 t` — into
/// `(width, height)`.
#[must_use]
pub fn parse_cell_pixels(params: &str) -> Option<(u32, u32)> {
    parse_size_report(params, 6)
}

/// Parses `CSI 4 ; <height> ; <width> t` — the answer to `CSI 14 t` — into
/// `(width, height)`.
#[must_use]
pub fn parse_window_pixels(params: &str) -> Option<(u32, u32)> {
    parse_size_report(params, 4)
}

/// Parses a `CSI <kind> ; <height> ; <width> t` report.
///
/// Note the order: the reports put **height before width**, which is the
/// opposite of how everything else in this module talks about sizes, and a
/// famous source of transposed images.
fn parse_size_report(params: &str, kind: u32) -> Option<(u32, u32)> {
    let mut fields = params.split(';');
    if fields.next()?.parse::<u32>().ok()? != kind {
        return None;
    }
    let height = fields.next()?.parse::<u32>().ok()?;
    let width = fields.next()?.parse::<u32>().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// What the user asked for on the command line.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    /// `--cols`.
    pub cols: Option<u16>,
    /// `--rows`.
    pub rows: Option<u16>,
    /// `--fit`.
    pub fit: Fit,
    /// `--scale`, as a multiplier applied to the source size before fitting.
    pub scale: Option<f32>,
    /// Terminal width in cells, used to keep an unconstrained image from
    /// flooding the screen.
    pub terminal_cols: u16,
    /// Whether the placement needs an explicit `c` *and* `r`, which Unicode
    /// placeholders do: the placeholder cells are the placement's area, so the
    /// terminal cannot be left to work one of them out.
    pub needs_explicit_area: bool,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            cols: None,
            rows: None,
            fit: Fit::Contain,
            scale: None,
            terminal_cols: 80,
            needs_explicit_area: false,
        }
    }
}

/// The computed placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// The `c` key, when the terminal should scale to a column count.
    pub cols: Option<u16>,
    /// The `r` key.
    pub rows: Option<u16>,
    /// The `x`, `y`, `w`, `h` source rectangle, in *original* image pixels.
    pub crop: Option<Rect>,
    /// How many cells the placement will occupy, for the cursor and for the
    /// report. Always at least 1×1.
    pub area: (u16, u16),
    /// Size to resample the source to before sending, when the source is so much
    /// larger than the placement that sending it whole would be wasteful.
    pub resample: Option<(u32, u32)>,
}

/// Computes the placement for a `width` × `height` source.
///
/// # Errors
///
/// Fails if the source has a zero dimension, or if `--scale` is not a positive,
/// finite number.
pub fn plan(source: (u32, u32), cell: CellSize, request: &Request) -> Result<Geometry> {
    let frame = Frame::new(source, cell, request.scale)?;

    // The whole fit matrix in one place. Note that a single axis is handled
    // before the two-axis modes: with only `--cols` there is nothing to cover or
    // stretch against, so every mode behaves the same.
    let mut geometry = match (request.fit, request.cols, request.rows) {
        (Fit::Native, cols, rows) => frame.native(cols, rows),
        (_, Some(cols), None) => frame.fit_cols(cols),
        (_, None, Some(rows)) => frame.fit_rows(rows),
        (Fit::Stretch, Some(cols), Some(rows)) => Frame::stretch(cols, rows),
        (Fit::Cover, Some(cols), Some(rows)) => frame.cover(cols, rows),
        (Fit::Contain, Some(cols), Some(rows)) => frame.contain(cols, rows),
        (Fit::Contain | Fit::Stretch | Fit::Cover, None, None) => {
            frame.unconstrained(request.terminal_cols)
        }
    };

    if request.needs_explicit_area {
        // A Unicode placeholder has to know both numbers, so fill in whichever
        // the terminal would have derived.
        geometry.cols = Some(geometry.area.0);
        geometry.rows = Some(geometry.area.1);
    }

    geometry.resample = resample_target(source, cell, geometry.area, geometry.crop.is_some());
    Ok(geometry)
}

/// The numbers every fit mode works from.
///
/// `--scale` is folded in here once: it multiplies the size the image is
/// *treated* as having, so every mode below sees a pre-scaled source and none of
/// them has to think about it. The bytes on the wire stay at the original
/// resolution — scale moves the target, not the pixels.
#[derive(Debug, Clone, Copy)]
struct Frame {
    /// The source's real pixel dimensions, which crop rectangles are relative to.
    source: (u32, u32),
    /// Source size after `--scale`.
    scaled: (f64, f64),
    /// Cell size in pixels.
    cell: (f64, f64),
    /// The `--scale` factor.
    scale: f64,
    /// Cells the image occupies at one image pixel per screen pixel.
    natural: (u16, u16),
}

impl Frame {
    /// Derives the frame, validating the source size and `--scale`.
    fn new(source: (u32, u32), cell: CellSize, scale: Option<f32>) -> Result<Self> {
        let (source_width, source_height) = source;
        if source_width == 0 || source_height == 0 {
            anyhow::bail!("cannot place a {source_width}x{source_height} image");
        }
        let scale = match scale {
            None => 1.0f64,
            Some(scale) if scale.is_finite() && scale > 0.0 => f64::from(scale),
            Some(scale) => anyhow::bail!("--scale must be a positive number, got {scale}"),
        };
        let scaled = (
            (f64::from(source_width) * scale).max(1.0),
            (f64::from(source_height) * scale).max(1.0),
        );
        let cell = (f64::from(cell.width), f64::from(cell.height));
        Ok(Self {
            source,
            scaled,
            cell,
            scale,
            natural: (ceil_div(scaled.0, cell.0), ceil_div(scaled.1, cell.1)),
        })
    }

    /// Cell area in pixels.
    fn pixels(&self, cols: u16, rows: u16) -> (f64, f64) {
        (
            f64::from(u32::from(cols)) * self.cell.0,
            f64::from(u32::from(rows)) * self.cell.1,
        )
    }

    /// No `c`/`r` at all: the terminal draws the image at its own size. An area
    /// smaller than the image can then only be achieved by cropping.
    fn native(&self, cols: Option<u16>, rows: Option<u16>) -> Geometry {
        let area = (
            cols.map_or(self.natural.0, |cols| cols.max(1).min(self.natural.0)),
            rows.map_or(self.natural.1, |rows| rows.max(1).min(self.natural.1)),
        );
        let crop = if area == self.natural {
            None
        } else {
            let (width, height) = self.pixels(area.0, area.1);
            Some(crop_rect(
                0.0,
                0.0,
                width / self.scale,
                height / self.scale,
                self.source,
            ))
        };
        Geometry {
            cols: None,
            rows: None,
            crop,
            area,
            resample: None,
        }
    }

    /// `c` and `r` exactly as asked, aspect ratio be damned.
    fn stretch(cols: u16, rows: u16) -> Geometry {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Geometry {
            cols: Some(cols),
            rows: Some(rows),
            crop: None,
            area: (cols, rows),
            resample: None,
        }
    }

    /// Fill the area, preserving the aspect ratio, and crop the overflow with a
    /// centred source rectangle.
    fn cover(&self, cols: u16, rows: u16) -> Geometry {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let (target_width, target_height) = self.pixels(cols, rows);
        // The larger factor fills both axes; whatever sticks out is cropped.
        let factor = (target_width / self.scaled.0).max(target_height / self.scaled.1);
        let visible_width = (target_width / factor).min(self.scaled.0);
        let visible_height = (target_height / factor).min(self.scaled.1);
        let crop = crop_rect(
            (self.scaled.0 - visible_width) / 2.0 / self.scale,
            (self.scaled.1 - visible_height) / 2.0 / self.scale,
            visible_width / self.scale,
            visible_height / self.scale,
            self.source,
        );
        Geometry {
            cols: Some(cols),
            rows: Some(rows),
            crop: Some(crop),
            area: (cols, rows),
            resample: None,
        }
    }

    /// Fit inside the area, preserving the aspect ratio, using less of it than
    /// asked for on whichever axis does not bind.
    fn contain(&self, cols: u16, rows: u16) -> Geometry {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let (target_width, target_height) = self.pixels(cols, rows);
        let factor = (target_width / self.scaled.0).min(target_height / self.scaled.1);
        let fitted_cols = ceil_div(self.scaled.0 * factor, self.cell.0).min(cols);
        let fitted_rows = ceil_div(self.scaled.1 * factor, self.cell.1).min(rows);
        Geometry {
            cols: Some(fitted_cols),
            rows: Some(fitted_rows),
            crop: None,
            area: (fitted_cols, fitted_rows),
            resample: None,
        }
    }

    /// A column count, leaving `r` unset so the *terminal* derives the height
    /// from the aspect ratio — which it can do exactly, where rounding to whole
    /// cells here could not.
    fn fit_cols(&self, cols: u16) -> Geometry {
        let cols = cols.max(1);
        let factor = self.pixels(cols, 0).0 / self.scaled.0;
        Geometry {
            cols: Some(cols),
            rows: None,
            crop: None,
            area: (cols, ceil_div(self.scaled.1 * factor, self.cell.1)),
            resample: None,
        }
    }

    /// A row count, with the column count left to the terminal.
    fn fit_rows(&self, rows: u16) -> Geometry {
        let rows = rows.max(1);
        let factor = self.pixels(0, rows).1 / self.scaled.1;
        Geometry {
            cols: None,
            rows: Some(rows),
            crop: None,
            area: (ceil_div(self.scaled.0 * factor, self.cell.0), rows),
            resample: None,
        }
    }

    /// Natural size, except that an image wider than the terminal is brought down
    /// to fit rather than being silently truncated at the right edge.
    fn unconstrained(&self, terminal_cols: u16) -> Geometry {
        if terminal_cols > 0 && self.natural.0 > terminal_cols {
            return self.fit_cols(terminal_cols);
        }
        Geometry {
            cols: None,
            rows: None,
            crop: None,
            area: self.natural,
            resample: None,
        }
    }
}

/// The size to resample to, when the source dwarfs the placement.
///
/// Skipped when a crop is in play: the crop rectangle is expressed in source
/// pixels, so resampling would invalidate it, and cropping here instead would
/// mean the terminal no longer receives the whole image.
fn resample_target(
    source: (u32, u32),
    cell: CellSize,
    area: (u16, u16),
    cropped: bool,
) -> Option<(u32, u32)> {
    if cropped {
        return None;
    }
    let displayed_width = u32::from(area.0) * cell.width;
    let displayed_height = u32::from(area.1) * cell.height;
    if displayed_width == 0 || displayed_height == 0 {
        return None;
    }
    let too_wide = source.0 / displayed_width >= RESAMPLE_THRESHOLD;
    let too_tall = source.1 / displayed_height >= RESAMPLE_THRESHOLD;
    if too_wide && too_tall {
        Some((displayed_width.max(1), displayed_height.max(1)))
    } else {
        None
    }
}

/// Builds a source rectangle, clamped to the image and rounded outwards so a
/// crop never loses an edge pixel to floating point.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn crop_rect(x: f64, y: f64, width: f64, height: f64, source: (u32, u32)) -> Rect {
    let x = x.max(0.0).round().min(f64::from(source.0 - 1)) as u32;
    let y = y.max(0.0).round().min(f64::from(source.1 - 1)) as u32;
    let width = (width.round().max(1.0) as u32).min(source.0 - x);
    let height = (height.round().max(1.0) as u32).min(source.1 - y);
    Rect::new(x, y, width, height)
}

/// Divides and rounds up, in cells, clamped to at least one and to `u16`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ceil_div(pixels: f64, per_cell: f64) -> u16 {
    if per_cell <= 0.0 {
        return 1;
    }
    let cells = (pixels / per_cell).ceil().max(1.0);
    if cells > f64::from(u16::MAX) {
        u16::MAX
    } else {
        cells as u16
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A cell that makes the arithmetic easy to read: 10 wide, 20 tall.
    const CELL: CellSize = CellSize {
        width: 10,
        height: 20,
        source: CellSource::CellReport,
    };

    fn request(cols: Option<u16>, rows: Option<u16>, fit: Fit) -> Request {
        Request {
            cols,
            rows,
            fit,
            terminal_cols: 80,
            ..Request::default()
        }
    }

    #[test]
    fn size_reports_are_height_then_width() {
        // `CSI 16 t` is answered with `CSI 6 ; height ; width t`.
        assert_eq!(parse_cell_pixels("6;20;10"), Some((10, 20)));
        // `CSI 14 t` is answered with `CSI 4 ; height ; width t`.
        assert_eq!(parse_window_pixels("4;1080;1920"), Some((1920, 1080)));
    }

    #[test]
    fn size_reports_of_the_wrong_kind_are_ignored() {
        assert_eq!(parse_cell_pixels("4;1080;1920"), None, "that is a window");
        assert_eq!(parse_window_pixels("6;20;10"), None, "that is a cell");
        assert_eq!(parse_cell_pixels("8;1;2"), None);
        assert_eq!(parse_cell_pixels(""), None);
        assert_eq!(parse_cell_pixels("6;20"), None, "truncated");
        assert_eq!(parse_cell_pixels("6;x;10"), None, "not a number");
    }

    #[test]
    fn terminals_that_answer_zero_are_treated_as_not_answering() {
        // The spec calls this out: "some terminals return 0 for the width and
        // height values. Such terminals should be modified".
        assert_eq!(parse_window_pixels("4;0;0"), None);
        assert_eq!(parse_cell_pixels("6;0;10"), None);
        let fallback = CellSize::measured(0, 16, CellSource::CellReport);
        assert_eq!(fallback.source, CellSource::Assumed);
        assert_eq!((fallback.width, fallback.height), FALLBACK_CELL);
    }

    #[test]
    fn cell_sources_describe_themselves() {
        for source in [
            CellSource::CellReport,
            CellSource::WindowReport,
            CellSource::Assumed,
        ] {
            assert!(!source.describe().is_empty());
            assert!(!source.as_str().is_empty());
            assert!(
                source
                    .as_str()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-'),
                "{} should be machine-readable",
                source.as_str()
            );
        }
        assert!(CellSource::CellReport.is_measured());
        assert!(CellSource::WindowReport.is_measured());
        assert!(!CellSource::Assumed.is_measured());
        assert_eq!(CellSize::default().source, CellSource::Assumed);
    }

    #[test]
    fn an_image_that_fits_is_placed_at_its_natural_size() {
        // 100x80 pixels in 10x20 cells is exactly 10 columns and 4 rows.
        let geometry = plan((100, 80), CELL, &request(None, None, Fit::Contain)).unwrap();
        assert_eq!(geometry.area, (10, 4));
        assert_eq!(geometry.cols, None, "no scaling means no c key");
        assert_eq!(geometry.rows, None);
        assert_eq!(geometry.crop, None);
        assert_eq!(geometry.resample, None);
    }

    #[test]
    fn a_partial_cell_rounds_up_to_a_whole_one() {
        let geometry = plan((101, 81), CELL, &request(None, None, Fit::Contain)).unwrap();
        assert_eq!(geometry.area, (11, 5), "a cell is never half-occupied");
    }

    #[test]
    fn an_oversized_image_is_capped_at_the_terminal_width() {
        let mut request = request(None, None, Fit::Contain);
        request.terminal_cols = 40;
        // 1000 pixels wide is 100 cells, more than the terminal has.
        let geometry = plan((1000, 500), CELL, &request).unwrap();
        assert_eq!(geometry.cols, Some(40));
        assert_eq!(geometry.rows, None, "the terminal derives the height");
        // 40 cells is 400 pixels, a factor of 0.4; 500 * 0.4 = 200 pixels = 10 rows.
        assert_eq!(geometry.area, (40, 10));
    }

    #[test]
    fn only_cols_lets_the_terminal_work_out_the_rows() {
        let geometry = plan((200, 100), CELL, &request(Some(10), None, Fit::Contain)).unwrap();
        assert_eq!(geometry.cols, Some(10));
        assert_eq!(
            geometry.rows, None,
            "leaving r off is how the aspect ratio is preserved exactly"
        );
        // 10 cells = 100 px wide, half the source, so 50 px tall = 2.5 -> 3 rows.
        assert_eq!(geometry.area, (10, 3));
    }

    #[test]
    fn only_rows_works_the_same_way_round() {
        let geometry = plan((200, 100), CELL, &request(None, Some(2), Fit::Contain)).unwrap();
        assert_eq!(geometry.rows, Some(2));
        assert_eq!(geometry.cols, None);
        // 2 rows = 40 px tall, a factor of 0.4; 200 * 0.4 = 80 px = 8 cols.
        assert_eq!(geometry.area, (8, 2));
    }

    #[test]
    fn contain_fits_inside_the_box_and_may_use_less_than_asked_for() {
        // A wide image in a square box: the width binds.
        let geometry = plan((400, 100), CELL, &request(Some(20), Some(20), Fit::Contain)).unwrap();
        assert_eq!(geometry.cols, Some(20), "the width binds");
        // 20 cells = 200 px, factor 0.5; 100 * 0.5 = 50 px tall = 2.5 -> 3 rows.
        assert_eq!(geometry.rows, Some(3));
        assert_eq!(geometry.area, (20, 3));
        assert_eq!(geometry.crop, None, "contain never crops");
    }

    #[test]
    fn contain_never_exceeds_the_requested_box() {
        for (width, height) in [(400, 100), (100, 400), (33, 71), (1, 1000)] {
            let geometry = plan(
                (width, height),
                CELL,
                &request(Some(12), Some(6), Fit::Contain),
            )
            .unwrap();
            assert!(
                geometry.area.0 <= 12,
                "{width}x{height} -> {:?}",
                geometry.area
            );
            assert!(
                geometry.area.1 <= 6,
                "{width}x{height} -> {:?}",
                geometry.area
            );
        }
    }

    #[test]
    fn cover_fills_the_box_and_crops_the_overflow() {
        // A wide image forced into a square box: the sides get cropped.
        let geometry = plan((400, 100), CELL, &request(Some(10), Some(10), Fit::Cover)).unwrap();
        assert_eq!(geometry.cols, Some(10));
        assert_eq!(geometry.rows, Some(10));
        assert_eq!(geometry.area, (10, 10));
        let crop = geometry.crop.unwrap();
        // The box is 100x200 px. Filling it needs a factor of 2 (200/100 tall),
        // so only 50x100 source pixels are visible, centred horizontally.
        assert_eq!(crop.width, 50);
        assert_eq!(crop.height, 100);
        assert_eq!(crop.x, (400 - 50) / 2);
        assert_eq!(crop.y, 0);
    }

    #[test]
    fn a_crop_never_leaves_the_image() {
        for (width, height) in [(400, 100), (100, 400), (7, 3), (1000, 1)] {
            let geometry = plan(
                (width, height),
                CELL,
                &request(Some(9), Some(4), Fit::Cover),
            )
            .unwrap();
            if let Some(crop) = geometry.crop {
                assert!(crop.x + crop.width <= width, "{width}x{height}: {crop:?}");
                assert!(crop.y + crop.height <= height, "{width}x{height}: {crop:?}");
                assert!(crop.width > 0 && crop.height > 0);
            }
        }
    }

    #[test]
    fn stretch_uses_exactly_the_area_it_was_given() {
        let geometry = plan((400, 100), CELL, &request(Some(7), Some(3), Fit::Stretch)).unwrap();
        assert_eq!(geometry.cols, Some(7));
        assert_eq!(geometry.rows, Some(3));
        assert_eq!(geometry.area, (7, 3));
        assert_eq!(geometry.crop, None, "stretch distorts rather than crops");
    }

    #[test]
    fn native_sends_no_scaling_keys_at_all() {
        let geometry = plan((100, 80), CELL, &request(None, None, Fit::Native)).unwrap();
        assert_eq!(geometry.cols, None);
        assert_eq!(geometry.rows, None);
        assert_eq!(geometry.crop, None);
        assert_eq!(geometry.area, (10, 4));
    }

    #[test]
    fn native_with_a_smaller_area_crops_rather_than_scales() {
        let geometry = plan((100, 80), CELL, &request(Some(5), Some(2), Fit::Native)).unwrap();
        assert_eq!(geometry.cols, None, "still no scaling");
        assert_eq!(geometry.area, (5, 2));
        let crop = geometry.crop.unwrap();
        assert_eq!((crop.x, crop.y), (0, 0));
        assert_eq!((crop.width, crop.height), (50, 40));
    }

    #[test]
    fn native_ignores_an_area_larger_than_the_image() {
        let geometry = plan((100, 80), CELL, &request(Some(50), Some(50), Fit::Native)).unwrap();
        assert_eq!(geometry.area, (10, 4), "the image is only this big");
        assert_eq!(geometry.crop, None);
    }

    #[test]
    fn scale_multiplies_the_target_without_touching_the_pixels() {
        let mut request = request(None, None, Fit::Contain);
        request.scale = Some(2.0);
        // 100x80 at 2x is treated as 200x160, so 20 cols by 8 rows.
        let geometry = plan((100, 80), CELL, &request).unwrap();
        assert_eq!(geometry.area, (20, 8));
        assert_eq!(
            geometry.resample, None,
            "the original pixels still go on the wire"
        );

        request.scale = Some(0.5);
        let geometry = plan((100, 80), CELL, &request).unwrap();
        assert_eq!(geometry.area, (5, 2));
    }

    #[test]
    fn scale_interacts_with_an_explicit_column_count() {
        let mut request = request(Some(10), None, Fit::Contain);
        request.scale = Some(4.0);
        // The column count wins; scale only changes the aspect arithmetic, and
        // that is unaffected by a uniform factor.
        let geometry = plan((200, 100), CELL, &request).unwrap();
        assert_eq!(geometry.cols, Some(10));
        assert_eq!(geometry.area.1, 3, "aspect ratio is scale-invariant");
    }

    #[test]
    fn a_bad_scale_is_refused() {
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let mut request = request(None, None, Fit::Contain);
            request.scale = Some(bad);
            let error = plan((10, 10), CELL, &request).unwrap_err().to_string();
            assert!(error.contains("--scale"), "{bad}: {error}");
        }
    }

    #[test]
    fn a_zero_sized_source_is_refused() {
        assert!(plan((0, 10), CELL, &request(None, None, Fit::Contain)).is_err());
        assert!(plan((10, 0), CELL, &request(None, None, Fit::Contain)).is_err());
    }

    #[test]
    fn a_placeholder_needs_both_numbers_spelled_out() {
        let mut request = request(Some(10), None, Fit::Contain);
        request.needs_explicit_area = true;
        let geometry = plan((200, 100), CELL, &request).unwrap();
        assert_eq!(geometry.cols, Some(10));
        assert_eq!(
            geometry.rows,
            Some(3),
            "a placeholder's cells are its area, so r cannot be left implicit"
        );
        assert_eq!(geometry.area, (10, 3));
    }

    #[test]
    fn a_hugely_oversized_source_is_resampled_down() {
        // A 4000x3000 photo in 20 columns: the placement is 200x... pixels, so
        // sending 12 million pixels would be absurd.
        let geometry = plan(
            (4000, 3000),
            CELL,
            &request(Some(20), Some(10), Fit::Contain),
        )
        .unwrap();
        let (width, height) = geometry.resample.unwrap();
        assert_eq!(
            (width, height),
            (
                u32::from(geometry.area.0) * CELL.width,
                u32::from(geometry.area.1) * CELL.height
            ),
            "resample to exactly what the placement shows"
        );
    }

    #[test]
    fn a_moderately_oversized_source_keeps_its_pixels_for_hidpi() {
        // Only 1.5x too big per axis: those pixels are what a HiDPI terminal
        // renders with, so they stay.
        let geometry = plan((300, 300), CELL, &request(Some(20), Some(10), Fit::Contain)).unwrap();
        assert_eq!(geometry.resample, None);
        assert_eq!(RESAMPLE_THRESHOLD, 2);
    }

    #[test]
    fn a_cropped_placement_is_never_resampled() {
        let geometry = plan((4000, 3000), CELL, &request(Some(4), Some(2), Fit::Cover)).unwrap();
        assert!(geometry.crop.is_some());
        assert_eq!(
            geometry.resample, None,
            "the crop rectangle is in source pixels, so it must stay valid"
        );
    }

    #[test]
    fn every_plan_produces_a_usable_area() {
        // A sweep over the whole flag space: nothing may come out zero-sized.
        for fit in [Fit::Contain, Fit::Cover, Fit::Stretch, Fit::Native] {
            for cols in [None, Some(1), Some(13)] {
                for rows in [None, Some(1), Some(7)] {
                    for source in [(1, 1), (3, 400), (1920, 1080), (65_000, 3)] {
                        let geometry = plan(source, CELL, &request(cols, rows, fit)).unwrap();
                        assert!(
                            geometry.area.0 >= 1 && geometry.area.1 >= 1,
                            "{fit:?} {cols:?} {rows:?} {source:?} -> {geometry:?}"
                        );
                        if let Some(cols) = geometry.cols {
                            assert!(cols >= 1);
                        }
                        if let Some(rows) = geometry.rows {
                            assert!(rows >= 1);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_fallback_cell_is_a_plausible_bitmap_cell() {
        assert_eq!(FALLBACK_CELL, (8, 16));
        let cell = CellSize::default();
        assert!(
            cell.height > cell.width,
            "cells are taller than they are wide"
        );
    }
}
