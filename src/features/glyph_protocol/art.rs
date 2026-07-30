//! The glyphs the demo registers, and a way to look at an outline before
//! sending it.
//!
//! # Coordinate space
//!
//! Shapes are authored in a box [`DESIGN_WIDTH`] × [`DESIGN_HEIGHT`] inside a
//! `upm=1000` em, which is roughly the proportions of a monospace cell. The
//! specification says the terminal maps the `upm` space onto its cell, without
//! pinning down how a non-square design box is treated, so drawing at cell
//! proportions is the choice least likely to come out stretched. If a terminal
//! turns out to fit the full em square to the cell instead, only these constants
//! need to change.
//!
//! # Winding
//!
//! TrueType fills by non-zero winding: contours wound the same way merge, and a
//! contour wound the opposite way cuts a hole. Solid parts here are clockwise,
//! holes counter-clockwise.

use std::f64::consts::TAU;

use crate::{
    features::glyph_protocol::glyf::{Contour, Outline, Pen, Point, circle, round},
    theme::{Ink, Theme},
};

/// Width of the design box, in em units.
pub const DESIGN_WIDTH: i16 = 600;

/// Height of the design box, in em units.
pub const DESIGN_HEIGHT: i16 = 1000;

// The demo's codepoints, all inside the basic Private Use Area. Grouped so a
// reader watching the sequences can tell at a glance which glyph is which.

/// A plain triangle: the smallest interesting outline.
pub const CP_TRIANGLE: u32 = 0xE900;
/// A chevron built from quadratic curves.
pub const CP_CHEVRON: u32 = 0xE901;
/// A ring: two contours, opposite winding.
pub const CP_RING: u32 = 0xE902;
/// The same ring registered at a coarse `upm`, to show scale independence.
pub const CP_RING_COARSE: u32 = 0xE903;
/// Git branch icon, as a Nerd Font would supply at U+E0A0.
pub const CP_BRANCH: u32 = 0xE904;
/// Padlock.
pub const CP_LOCK: u32 = 0xE905;
/// Folder.
pub const CP_FOLDER: u32 = 0xE906;
/// Lightning bolt.
pub const CP_BOLT: u32 = 0xE907;
/// The glyph the interactive editor rewrites in place.
pub const CP_EDITOR: u32 = 0xE908;

/// First of [`SPINNER_FRAMES`] consecutive codepoints holding animation frames.
pub const CP_SPINNER_BASE: u32 = 0xE910;
/// How many frames the spinner animation uses.
pub const SPINNER_FRAMES: u32 = 12;

/// Player ship, for the finale.
pub const CP_SHIP: u32 = 0xE920;
/// Falling hazard, for the finale.
pub const CP_ALIEN: u32 = 0xE921;
/// Collectible, for the finale.
pub const CP_STAR: u32 = 0xE922;

/// Every codepoint the demo registers, for cleaning up afterwards.
#[must_use]
pub fn all_codepoints() -> Vec<u32> {
    let mut codepoints = vec![
        CP_TRIANGLE,
        CP_CHEVRON,
        CP_RING,
        CP_RING_COARSE,
        CP_BRANCH,
        CP_LOCK,
        CP_FOLDER,
        CP_BOLT,
        CP_EDITOR,
        CP_SHIP,
        CP_ALIEN,
        CP_STAR,
    ];
    codepoints.extend(CP_SPINNER_BASE..CP_SPINNER_BASE + SPINNER_FRAMES);
    codepoints
}

/// The character for a codepoint, for printing it into the terminal.
///
/// Every codepoint here is a hard-coded Private Use Area constant, so the
/// conversion cannot fail; tofu is the fallback if one somehow did.
#[must_use]
pub fn glyph(codepoint: u32) -> char {
    char::from_u32(codepoint).unwrap_or(super::TOFU)
}

// -- primitives --------------------------------------------------------------

/// An axis-aligned rectangle, wound clockwise.
#[must_use]
pub fn rect(left: i16, bottom: i16, right: i16, top: i16) -> Contour {
    Contour {
        points: vec![
            Point::on(left, bottom),
            Point::on(left, top),
            Point::on(right, top),
            Point::on(right, bottom),
        ],
    }
}

/// A closed polygon from on-curve points.
#[must_use]
pub fn polygon(points: &[(i16, i16)]) -> Contour {
    Contour {
        points: points.iter().map(|&(x, y)| Point::on(x, y)).collect(),
    }
}

/// Points along a circular arc, as quadratic segments of at most 45°.
///
/// Each segment's control point sits where the tangents at its ends meet, at
/// `radius / cos(half the segment angle)` along the bisector. The final on-curve
/// point is included, so an arc can be chained into a larger contour.
#[must_use]
pub fn arc(center_x: i16, center_y: i16, radius: i16, from: f64, to: f64) -> Vec<Point> {
    let sweep = to - from;
    let segments = segment_count(sweep);
    let step = sweep / f64::from(u16::try_from(segments).unwrap_or(u16::MAX));
    let control_distance = f64::from(radius) / (step / 2.0).cos().abs();

    let at = |angle: f64, distance: f64| {
        (
            round(f64::from(center_x) + distance * angle.cos()),
            round(f64::from(center_y) + distance * angle.sin()),
        )
    };

    let mut points = Vec::with_capacity(segments * 2 + 1);
    let (x, y) = at(from, f64::from(radius));
    points.push(Point::on(x, y));
    for segment in 0..segments {
        let start = from + step * f64::from(u16::try_from(segment).unwrap_or(u16::MAX));
        let (cx, cy) = at(start + step / 2.0, control_distance);
        let (ex, ey) = at(start + step, f64::from(radius));
        points.push(Point::off(cx, cy));
        points.push(Point::on(ex, ey));
    }
    points
}

/// A slice of an annulus: out along one radius, round the outer edge, back in,
/// and round the inner edge the other way.
///
/// This is the shape a progress arc or a spinner segment wants.
#[must_use]
pub fn ring_sector(
    center_x: i16,
    center_y: i16,
    outer: i16,
    inner: i16,
    from: f64,
    to: f64,
) -> Contour {
    let mut points = arc(center_x, center_y, outer, from, to);
    let mut back = arc(center_x, center_y, inner, to, from);
    points.append(&mut back);
    Contour { points }
}

/// How many quadratic segments to spend on an arc sweeping `sweep` radians:
/// one per 45°, rounded up, and never fewer than one.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is clamped to 1..=64 before the cast"
)]
fn segment_count(sweep: f64) -> usize {
    let raw = (sweep.abs() / (TAU / 8.0)).ceil();
    if raw.is_finite() {
        (raw.clamp(1.0, 64.0)) as usize
    } else {
        1
    }
}

// -- the glyphs --------------------------------------------------------------

/// Three on-curve points. The smallest outline worth registering, and the one
/// the byte-anatomy step walks through.
#[must_use]
pub fn triangle() -> Outline {
    Pen::new()
        .move_to(80, 120)
        .line_to(300, 860)
        .line_to(520, 120)
        .build()
}

/// A chevron, drawn with four quadratic curves and one straight edge, so the
/// curve step has something whose control points are obvious.
#[must_use]
pub fn chevron() -> Outline {
    Pen::new()
        .move_to(140, 820)
        .quad_to(320, 660, 420, 500)
        .quad_to(320, 340, 140, 180)
        .line_to(260, 110)
        .quad_to(500, 330, 540, 500)
        .quad_to(500, 670, 260, 890)
        .build()
}

/// A ring: an outer circle clockwise, an inner circle counter-clockwise so it
/// cuts a hole instead of filling it in.
#[must_use]
pub fn ring() -> Outline {
    Outline {
        contours: vec![
            Contour {
                points: circle(300, 500, 240, true),
            },
            Contour {
                points: circle(300, 500, 140, false),
            },
        ],
    }
}

/// A git branch icon — three nodes joined by a stem, the thing Nerd Fonts
/// supplies at U+E0A0 and every shell prompt depends on.
///
/// Five overlapping clockwise contours. Non-zero winding merges them, so the
/// stems and the nodes become one shape without any path arithmetic.
#[must_use]
pub fn branch() -> Outline {
    Outline {
        contours: vec![
            rect(190, 210, 250, 790),
            rect(220, 470, 430, 530),
            Contour {
                points: circle(220, 810, 105, true),
            },
            Contour {
                points: circle(220, 190, 105, true),
            },
            Contour {
                points: circle(450, 500, 105, true),
            },
        ],
    }
}

/// A padlock: body, shackle, keyhole.
#[must_use]
pub fn lock() -> Outline {
    let shackle_outer = arc(300, 560, 170, 0.0, TAU / 2.0);
    let shackle_inner = arc(300, 560, 110, TAU / 2.0, 0.0);
    Outline {
        contours: vec![
            rect(110, 120, 490, 560),
            Contour {
                points: shackle_outer.into_iter().chain(shackle_inner).collect(),
            },
            // Counter-clockwise, so it becomes a hole in the body.
            Contour {
                points: circle(300, 350, 60, false),
            },
        ],
    }
}

/// A folder: body with a tab along the top left.
#[must_use]
pub fn folder() -> Outline {
    Outline {
        contours: vec![polygon(&[
            (90, 200),
            (90, 760),
            (270, 760),
            (330, 680),
            (510, 680),
            (510, 200),
        ])],
    }
}

/// A lightning bolt: seven points, no curves.
#[must_use]
pub fn bolt() -> Outline {
    Outline {
        contours: vec![polygon(&[
            (330, 900),
            (140, 470),
            (280, 470),
            (200, 110),
            (470, 560),
            (320, 560),
            (390, 900),
        ])],
    }
}

/// One frame of a spinner: a 100° arc rotated by frame number.
///
/// Registering each frame at its own codepoint turns animation into printing a
/// different character, which is why this costs almost nothing to redraw.
#[must_use]
pub fn spinner_frame(frame: u32) -> Outline {
    let step = TAU / f64::from(SPINNER_FRAMES);
    let from = f64::from(frame) * -step;
    Outline {
        contours: vec![ring_sector(300, 500, 250, 160, from, from - TAU * 0.28)],
    }
}

/// The player's ship for the finale: a blunt arrowhead with a notched tail.
#[must_use]
pub fn ship() -> Outline {
    Outline {
        contours: vec![polygon(&[
            (300, 880),
            (110, 200),
            (300, 330),
            (490, 200),
        ])],
    }
}

/// A falling hazard: a blocky, deliberately retro silhouette.
#[must_use]
pub fn alien() -> Outline {
    Outline {
        contours: vec![
            rect(150, 400, 450, 700),
            rect(90, 250, 200, 420),
            rect(400, 250, 510, 420),
            rect(210, 720, 270, 850),
            rect(330, 720, 390, 850),
        ],
    }
}

/// A collectible star: an eight-pointed sparkle, small and bright.
#[must_use]
pub fn star() -> Outline {
    let (cx, cy) = (300.0_f64, 500.0_f64);
    let (long, short) = (230.0_f64, 80.0_f64);
    let mut points = Vec::with_capacity(8);
    for index in 0..8 {
        let angle = TAU * f64::from(index) / 8.0 + TAU / 16.0;
        let radius = if index % 2 == 0 { long } else { short };
        points.push(Point::on(
            round(cx + radius * angle.cos()),
            round(cy + radius * angle.sin()),
        ));
    }
    Pen::new().points(points).build()
}

/// The starting shape for the interactive editor: a curved wedge whose points
/// are far enough apart to drag around without them overlapping.
#[must_use]
pub fn editor_start() -> Vec<Point> {
    vec![
        Point::on(120, 180),
        Point::off(300, 900),
        Point::on(480, 180),
        Point::off(300, 380),
    ]
}

// -- visualizing an outline --------------------------------------------------

/// A braille-cell drawing surface: eight addressable dots per character cell,
/// which is the highest resolution a terminal grid offers without graphics.
///
/// Dot bit layout within a cell, per the Unicode braille block:
///
/// ```text
///   1 4      0x01 0x08
///   2 5      0x02 0x10
///   3 6      0x04 0x20
///   7 8      0x40 0x80
/// ```
#[derive(Debug)]
pub struct Canvas {
    cols: usize,
    rows: usize,
    dots: Vec<u8>,
    ink: Vec<Option<Ink>>,
}

/// Dot bit for a position within a braille cell, indexed `[x][y]`.
const BRAILLE_BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

impl Canvas {
    /// A blank canvas `cols` × `rows` character cells, so `cols * 2` by
    /// `rows * 4` dots.
    #[must_use]
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            dots: vec![0; cols * rows],
            ink: vec![None; cols * rows],
        }
    }

    /// Dot-space width.
    #[must_use]
    pub fn dot_width(&self) -> i32 {
        i32::try_from(self.cols * 2).unwrap_or(i32::MAX)
    }

    /// Dot-space height.
    #[must_use]
    pub fn dot_height(&self) -> i32 {
        i32::try_from(self.rows * 4).unwrap_or(i32::MAX)
    }

    /// Lights one dot. Out-of-range coordinates are dropped rather than
    /// wrapping, so a shape that overflows the box is clipped.
    pub fn set(&mut self, x: i32, y: i32, ink: Option<Ink>) {
        if x < 0 || y < 0 || x >= self.dot_width() || y >= self.dot_height() {
            return;
        }
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return;
        };
        let index = (y / 4) * self.cols + (x / 2);
        self.dots[index] |= BRAILLE_BITS[x % 2][y % 4];
        if ink.is_some() {
            self.ink[index] = ink;
        }
    }

    /// Draws a line with Bresenham's algorithm.
    pub fn line(&mut self, from: (i32, i32), to: (i32, i32), ink: Option<Ink>) {
        let (mut x, mut y) = from;
        let (dx, dy) = ((to.0 - x).abs(), -(to.1 - y).abs());
        let (step_x, step_y) = (if x < to.0 { 1 } else { -1 }, if y < to.1 { 1 } else { -1 });
        let mut error = dx + dy;
        loop {
            self.set(x, y, ink);
            if x == to.0 && y == to.1 {
                break;
            }
            let doubled = error * 2;
            if doubled >= dy {
                error += dy;
                x += step_x;
            }
            if doubled <= dx {
                error += dx;
                y += step_y;
            }
        }
    }

    /// Fills a small square, for marking a point of interest.
    pub fn blob(&mut self, center: (i32, i32), ink: Option<Ink>) {
        for dx in -1..=1 {
            for dy in -1..=1 {
                self.set(center.0 + dx, center.1 + dy, ink);
            }
        }
    }

    /// Renders to one string per row, colored per cell.
    #[must_use]
    pub fn render(&self, theme: &Theme) -> Vec<String> {
        (0..self.rows)
            .map(|row| {
                let mut line = String::new();
                for col in 0..self.cols {
                    let index = row * self.cols + col;
                    let ch = char::from_u32(0x2800 + u32::from(self.dots[index])).unwrap_or(' ');
                    match self.ink[index] {
                        Some(ink) => line.push_str(&theme.paint(&ch.to_string(), ink)),
                        None => line.push(ch),
                    }
                }
                line
            })
            .collect()
    }
}

/// Draws an outline into a braille canvas, marking its points.
///
/// On-curve points are green, control points amber, so the relationship between
/// the two is visible: every amber dot pulls the curve towards it without the
/// curve reaching it.
#[must_use]
pub fn plot(theme: &Theme, outline: &Outline, upm: i16, cols: usize, rows: usize) -> Vec<String> {
    let mut canvas = Canvas::new(cols, rows);
    // Captured by value, so the closure does not hold a borrow on the canvas it
    // is about to be drawn into.
    let (dot_width, dot_height) = (canvas.dot_width(), canvas.dot_height());
    let to_dot = move |point: &Point| -> (i32, i32) {
        let upm = f64::from(upm.max(1));
        let width = f64::from(dot_width - 1);
        let height = f64::from(dot_height - 1);
        // Font y grows upwards; screen y grows downwards, hence the subtraction.
        (
            to_dot_coordinate(f64::from(point.x) / upm * width),
            to_dot_coordinate(height - f64::from(point.y) / upm * height),
        )
    };

    for contour in &outline.contours {
        for segment in flatten(contour) {
            canvas.line(to_dot(&segment.0), to_dot(&segment.1), Some(Ink::Accent));
        }
    }
    for point in outline.points() {
        let ink = if point.on_curve { Ink::Good } else { Ink::Warn };
        canvas.blob(to_dot(point), Some(ink));
    }
    canvas.render(theme)
}

/// Flattens a contour into straight segments.
///
/// Walks the closed contour from its first on-curve point. A pair of consecutive
/// off-curve points implies an on-curve point at their midpoint — a TrueType
/// space-saving trick this handles even though [`Pen`] never emits it.
#[must_use]
pub fn flatten(contour: &Contour) -> Vec<(Point, Point)> {
    /// Straight-line pieces per quadratic curve. Enough that a curve the height
    /// of a cell has no visible corners.
    const CURVE_STEPS: u16 = 12;

    let points = &contour.points;
    let count = points.len();
    if count < 2 {
        return Vec::new();
    }
    let Some(start) = points.iter().position(|point| point.on_curve) else {
        // An all-off-curve contour is legal: every implied on-curve point is a
        // midpoint. Rare enough that drawing the control polygon is fine.
        return points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .take(count)
            .map(|(a, b)| (*a, *b))
            .collect();
    };

    let mut segments = Vec::new();
    let mut current = points[start];
    let mut offset = 1;
    while offset <= count {
        let point = points[(start + offset) % count];
        if point.on_curve {
            segments.push((current, point));
            current = point;
            offset += 1;
        } else {
            let following = points[(start + offset + 1) % count];
            let end = if following.on_curve {
                following
            } else {
                midpoint(point, following)
            };
            let mut previous = current;
            for step in 1..=CURVE_STEPS {
                let t = f64::from(step) / f64::from(CURVE_STEPS);
                let next = quadratic(current, point, end, t);
                segments.push((previous, next));
                previous = next;
            }
            current = end;
            offset += if following.on_curve { 2 } else { 1 };
        }
    }
    segments
}

/// Converts a computed dot position to canvas coordinates.
///
/// Out-of-range values are clamped rather than wrapped; [`Canvas::set`] clips
/// them, so a shape that overflows its box is cropped instead of reappearing on
/// the far side.
#[expect(
    clippy::cast_possible_truncation,
    reason = "clamped into i32 range before the cast"
)]
fn to_dot_coordinate(value: f64) -> i32 {
    let rounded = value.round();
    if rounded.is_nan() {
        return -1;
    }
    rounded.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// The implied on-curve point between two control points.
///
/// Both inputs are `i16`, so their average always fits back into one.
fn midpoint(a: Point, b: Point) -> Point {
    Point::on(
        i16::try_from(i32::midpoint(i32::from(a.x), i32::from(b.x))).unwrap_or(0),
        i16::try_from(i32::midpoint(i32::from(a.y), i32::from(b.y))).unwrap_or(0),
    )
}

/// Evaluates a quadratic Bézier at `t`.
fn quadratic(start: Point, control: Point, end: Point, t: f64) -> Point {
    let inverse = 1.0 - t;
    let blend = |a: i16, b: i16, c: i16| {
        round(
            inverse * inverse * f64::from(a)
                + 2.0 * inverse * t * f64::from(b)
                + t * t * f64::from(c),
        )
    };
    Point::on(
        blend(start.x, control.x, end.x),
        blend(start.y, control.y, end.y),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_demo_codepoint_is_registrable() {
        for codepoint in all_codepoints() {
            assert!(
                super::super::is_private_use(codepoint),
                "U+{codepoint:04X} is outside the Private Use Areas"
            );
            assert!(char::from_u32(codepoint).is_some());
        }
    }

    #[test]
    fn demo_codepoints_are_unique() {
        let mut codepoints = all_codepoints();
        let total = codepoints.len();
        codepoints.sort_unstable();
        codepoints.dedup();
        assert_eq!(codepoints.len(), total, "two glyphs share a codepoint");
    }

    #[test]
    fn demo_stays_well_under_the_glossary_limit() {
        assert!(all_codepoints().len() < super::super::GLOSSARY_CAPACITY);
    }

    /// Every glyph, so one loop can check invariants that must hold for all.
    fn all_glyphs() -> Vec<(&'static str, Outline)> {
        let mut glyphs = vec![
            ("triangle", triangle()),
            ("chevron", chevron()),
            ("ring", ring()),
            ("branch", branch()),
            ("lock", lock()),
            ("folder", folder()),
            ("bolt", bolt()),
            ("ship", ship()),
            ("alien", alien()),
            ("star", star()),
        ];
        for frame in 0..SPINNER_FRAMES {
            glyphs.push(("spinner", spinner_frame(frame)));
        }
        glyphs
    }

    #[test]
    fn every_glyph_fits_the_design_box() {
        for (name, outline) in all_glyphs() {
            let (x_min, y_min, x_max, y_max) = outline.bounds();
            assert!(x_min >= 0 && y_min >= 0, "{name} has negative coordinates");
            assert!(
                x_max <= DESIGN_WIDTH,
                "{name} is {x_max} wide, box is {DESIGN_WIDTH}"
            );
            assert!(
                y_max <= DESIGN_HEIGHT,
                "{name} is {y_max} tall, box is {DESIGN_HEIGHT}"
            );
        }
    }

    #[test]
    fn every_glyph_encodes_to_a_plausible_payload() {
        for (name, outline) in all_glyphs() {
            assert!(!outline.contours.is_empty(), "{name} is empty");
            assert!(outline.point_count() >= 3, "{name} cannot enclose an area");
            let encoded = outline.encode();
            assert!(
                encoded.len() < 1024,
                "{name} came to {} bytes, which is far above the 150-400 the spec expects",
                encoded.len()
            );
            let base64 = outline.encode_base64();
            assert!(
                !base64.contains('\u{1b}') && !base64.contains(';'),
                "{name} produced a payload that would break APC framing"
            );
        }
    }

    #[test]
    fn ring_uses_opposite_winding_so_the_hole_is_a_hole() {
        let ring = ring();
        assert_eq!(ring.contours.len(), 2);
        // The outer contour is clockwise and the inner counter-clockwise, so
        // their signed areas have opposite signs.
        let outer = signed_area(&ring.contours[0]);
        let inner = signed_area(&ring.contours[1]);
        assert!(
            outer * inner < 0.0,
            "outer {outer} and inner {inner} wind the same way, so the ring would fill solid"
        );
    }

    /// Twice the signed area of the control polygon; the sign gives winding.
    fn signed_area(contour: &Contour) -> f64 {
        let points = &contour.points;
        let mut total = 0.0;
        for index in 0..points.len() {
            let a = points[index];
            let b = points[(index + 1) % points.len()];
            total += f64::from(a.x) * f64::from(b.y) - f64::from(b.x) * f64::from(a.y);
        }
        total
    }

    #[test]
    fn arc_starts_and_ends_where_asked() {
        let points = arc(300, 500, 200, 0.0, TAU / 2.0);
        let first = points.first().unwrap();
        let last = points.last().unwrap();
        assert!(first.on_curve && last.on_curve);
        assert_eq!((first.x, first.y), (500, 500), "0° is due right");
        assert_eq!((last.x, last.y), (100, 500), "180° is due left");
    }

    #[test]
    fn spinner_frames_are_all_distinct() {
        let mut payloads: Vec<Vec<u8>> = (0..SPINNER_FRAMES)
            .map(|frame| spinner_frame(frame).encode())
            .collect();
        let total = payloads.len();
        payloads.sort_unstable();
        payloads.dedup();
        assert_eq!(payloads.len(), total, "the animation would visibly stall");
    }

    #[test]
    fn flatten_turns_a_curve_into_many_segments() {
        let curved = Contour {
            points: vec![
                Point::on(0, 0),
                Point::off(500, 1000),
                Point::on(1000, 0),
            ],
        };
        let segments = flatten(&curved);
        assert!(
            segments.len() > 10,
            "a quadratic should flatten to many pieces, got {}",
            segments.len()
        );
        // The chain must be continuous: each segment starts where the last ended.
        for pair in segments.windows(2) {
            assert_eq!(pair[0].1, pair[1].0, "flattened path has a gap");
        }
    }

    #[test]
    fn flatten_handles_implied_on_curve_midpoints() {
        // Two consecutive off-curve points imply an on-curve point between them.
        let contour = Contour {
            points: vec![
                Point::on(0, 0),
                Point::off(1000, 0),
                Point::off(1000, 1000),
                Point::on(0, 1000),
            ],
        };
        let segments = flatten(&contour);
        assert!(!segments.is_empty());
        for pair in segments.windows(2) {
            assert_eq!(pair[0].1, pair[1].0);
        }
    }

    #[test]
    fn flatten_survives_degenerate_contours() {
        assert!(flatten(&Contour::default()).is_empty());
        assert!(
            flatten(&Contour {
                points: vec![Point::on(0, 0)]
            })
            .is_empty()
        );
        // All control points, no on-curve point anywhere.
        let all_off = Contour {
            points: vec![Point::off(0, 0), Point::off(100, 0), Point::off(50, 100)],
        };
        assert_eq!(flatten(&all_off).len(), 3);
    }

    #[test]
    fn canvas_maps_dots_to_the_right_braille_bits() {
        let mut canvas = Canvas::new(1, 1);
        canvas.set(0, 0, None);
        assert_eq!(canvas.render(&Theme::plain())[0], "\u{2801}", "dot 1");

        let mut canvas = Canvas::new(1, 1);
        canvas.set(1, 3, None);
        assert_eq!(canvas.render(&Theme::plain())[0], "\u{2880}", "dot 8");

        let mut canvas = Canvas::new(1, 1);
        for x in 0..2 {
            for y in 0..4 {
                canvas.set(x, y, None);
            }
        }
        assert_eq!(canvas.render(&Theme::plain())[0], "\u{28ff}", "all eight");
    }

    #[test]
    fn canvas_clips_instead_of_wrapping() {
        let mut canvas = Canvas::new(2, 1);
        canvas.set(-5, 0, None);
        canvas.set(99, 0, None);
        canvas.set(0, -1, None);
        canvas.set(0, 99, None);
        assert_eq!(canvas.render(&Theme::plain())[0], "\u{2800}\u{2800}");
    }

    #[test]
    fn plot_produces_the_requested_shape_of_output() {
        let theme = Theme::plain();
        let rendered = plot(&theme, &triangle(), DESIGN_HEIGHT, 20, 10);
        assert_eq!(rendered.len(), 10);
        for line in &rendered {
            assert_eq!(line.chars().count(), 20);
        }
        assert!(
            rendered.iter().any(|line| line.contains('\u{28ff}')
                || line.chars().any(|ch| ch != '\u{2800}')),
            "nothing was drawn"
        );
    }

    #[test]
    fn plot_puts_the_apex_near_the_top() {
        // Font coordinates grow upwards, screen rows downwards. If that flip is
        // missing, the triangle renders upside down.
        let theme = Theme::plain();
        let rendered = plot(&theme, &triangle(), DESIGN_HEIGHT, 24, 12);
        let ink_per_row: Vec<usize> = rendered
            .iter()
            .map(|line| line.chars().filter(|&ch| ch != '\u{2800}').count())
            .collect();
        let top = ink_per_row[..3].iter().sum::<usize>();
        let bottom = ink_per_row[ink_per_row.len() - 3..].iter().sum::<usize>();
        assert!(
            bottom > top,
            "the wide base should be at the bottom: rows {ink_per_row:?}"
        );
    }
}
