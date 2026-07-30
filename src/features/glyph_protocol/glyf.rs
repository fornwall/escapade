//! Encoding TrueType simple-glyph outlines — the `glyf` payload format.
//!
//! Glyph Protocol accepts a deliberately small subset of the OpenType `glyf`
//! table: one simple glyph, no composites, no hinting instructions. That subset
//! is small enough to write from scratch, which is why this file exists rather
//! than a font-crate dependency — the demo shows the reader every byte and
//! explains what it is for, and that only works if we produced them ourselves.
//!
//! The geometry model is two kinds of point. Two on-curve points in a row draw
//! a straight line. An off-curve point between two on-curve points draws a
//! quadratic Bézier with the off-curve point as its control. That is all there
//! is; everything else here is packing.
//!
//! Reference: <https://learn.microsoft.com/en-us/typography/opentype/spec/glyf>

use base64::prelude::{BASE64_STANDARD, Engine as _};

/// `ON_CURVE_POINT`: the point is on the curve, not a Bézier control.
const FLAG_ON_CURVE: u8 = 0x01;
/// `X_SHORT_VECTOR`: the x delta fits in one unsigned byte.
const FLAG_X_SHORT: u8 = 0x02;
/// `Y_SHORT_VECTOR`: the y delta fits in one unsigned byte.
const FLAG_Y_SHORT: u8 = 0x04;
/// With `X_SHORT_VECTOR`, the delta is positive; without it, the delta is zero.
const FLAG_X_SAME_OR_POSITIVE: u8 = 0x10;
/// With `Y_SHORT_VECTOR`, the delta is positive; without it, the delta is zero.
const FLAG_Y_SAME_OR_POSITIVE: u8 = 0x20;

/// One point of a contour, in the outline's `upm` coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    /// Horizontal position.
    pub x: i16,
    /// Vertical position, increasing upwards as in font coordinates.
    pub y: i16,
    /// Whether the curve passes through this point, as opposed to being pulled
    /// towards it.
    pub on_curve: bool,
}

impl Point {
    /// A point the outline passes through.
    #[must_use]
    pub const fn on(x: i16, y: i16) -> Self {
        Self {
            x,
            y,
            on_curve: true,
        }
    }

    /// A quadratic Bézier control point.
    #[must_use]
    pub const fn off(x: i16, y: i16) -> Self {
        Self {
            x,
            y,
            on_curve: false,
        }
    }
}

/// A closed loop of points. TrueType closes contours implicitly — there is no
/// terminating point, the last point simply joins back to the first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Contour {
    /// The points, in drawing order. Winding direction decides inside from
    /// outside: outer contours run clockwise, holes counter-clockwise.
    pub points: Vec<Point>,
}

/// A complete glyph outline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outline {
    /// The contours making up the shape.
    pub contours: Vec<Contour>,
}

/// One field of an encoded glyph, for the byte-by-byte walkthrough the demo
/// prints.
#[derive(Debug, Clone)]
pub struct Field {
    /// The field's name in the OpenType specification.
    pub label: String,
    /// The bytes it occupies.
    pub bytes: Vec<u8>,
    /// What those bytes mean here.
    pub meaning: String,
}

impl Outline {
    /// Total number of points across all contours.
    #[must_use]
    pub fn point_count(&self) -> usize {
        self.contours.iter().map(|c| c.points.len()).sum()
    }

    /// Every point, in encoding order.
    pub fn points(&self) -> impl Iterator<Item = &Point> {
        self.contours.iter().flat_map(|c| c.points.iter())
    }

    /// The bounding box as `(xMin, yMin, xMax, yMax)`.
    ///
    /// Returns all zeros for an empty outline, which is what a `.notdef`-style
    /// blank glyph looks like.
    #[must_use]
    pub fn bounds(&self) -> (i16, i16, i16, i16) {
        let mut points = self.points();
        let Some(first) = points.next() else {
            return (0, 0, 0, 0);
        };
        let mut bounds = (first.x, first.y, first.x, first.y);
        for point in points {
            bounds.0 = bounds.0.min(point.x);
            bounds.1 = bounds.1.min(point.y);
            bounds.2 = bounds.2.max(point.x);
            bounds.3 = bounds.3.max(point.y);
        }
        bounds
    }

    /// Encodes to `glyf` bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        self.encode_with_fields().0
    }

    /// Encodes to base64, ready to drop into a registration sequence.
    #[must_use]
    pub fn encode_base64(&self) -> String {
        BASE64_STANDARD.encode(self.encode())
    }

    /// Encodes, and describes each field along the way.
    ///
    /// Keeping the walkthrough in the same function as the encoder is
    /// deliberate: an explanation that lives elsewhere drifts out of date.
    ///
    /// Point deltas are written in the short form when they fit in a byte and
    /// omitted entirely when zero. Real font compilers additionally use
    /// `REPEAT_FLAG` to collapse runs of identical flags; that is skipped here
    /// because it makes the flag array harder to read for no benefit at these
    /// sizes.
    #[must_use]
    pub fn encode_with_fields(&self) -> (Vec<u8>, Vec<Field>) {
        let mut bytes = Vec::new();
        let mut fields = Vec::new();

        let contour_count = i16::try_from(self.contours.len()).unwrap_or(i16::MAX);
        push_field(
            &mut bytes,
            &mut fields,
            "numberOfContours",
            &contour_count.to_be_bytes(),
            format!(
                "{contour_count} contour{} — positive means a simple glyph, negative would mean a composite",
                if contour_count == 1 { "" } else { "s" }
            ),
        );

        let (x_min, y_min, x_max, y_max) = self.bounds();
        push_field(
            &mut bytes,
            &mut fields,
            "xMin, yMin, xMax, yMax",
            &[
                x_min.to_be_bytes(),
                y_min.to_be_bytes(),
                x_max.to_be_bytes(),
                y_max.to_be_bytes(),
            ]
            .concat(),
            format!(
                "bounding box ({x_min},{y_min})–({x_max},{y_max}), which the terminal uses to fit the glyph to the cell"
            ),
        );

        let mut end_points = Vec::new();
        let mut running = 0usize;
        for contour in &self.contours {
            running += contour.points.len();
            end_points.push(u16::try_from(running.saturating_sub(1)).unwrap_or(u16::MAX));
        }
        let end_bytes: Vec<u8> = end_points.iter().flat_map(|e| e.to_be_bytes()).collect();
        push_field(
            &mut bytes,
            &mut fields,
            "endPtsOfContours",
            &end_bytes,
            format!(
                "last point index of each contour: {end_points:?} — this is the only thing separating one contour from the next"
            ),
        );

        push_field(
            &mut bytes,
            &mut fields,
            "instructionLength",
            &0u16.to_be_bytes(),
            "no hinting instructions; the protocol rejects them anyway".to_owned(),
        );

        // Flags and coordinates are three parallel passes over the same points:
        // all flags, then all x deltas, then all y deltas.
        let mut flags = Vec::with_capacity(self.point_count());
        let mut x_bytes = Vec::new();
        let mut y_bytes = Vec::new();
        let (mut previous_x, mut previous_y) = (0i16, 0i16);

        for point in self.points() {
            let mut flag = if point.on_curve { FLAG_ON_CURVE } else { 0 };
            let delta_x = point.x - previous_x;
            let delta_y = point.y - previous_y;

            flag |= encode_delta(delta_x, FLAG_X_SHORT, FLAG_X_SAME_OR_POSITIVE, &mut x_bytes);
            flag |= encode_delta(delta_y, FLAG_Y_SHORT, FLAG_Y_SAME_OR_POSITIVE, &mut y_bytes);

            flags.push(flag);
            previous_x = point.x;
            previous_y = point.y;
        }

        let on_curve_count = self.points().filter(|p| p.on_curve).count();
        push_field(
            &mut bytes,
            &mut fields,
            "flags",
            &flags,
            format!(
                "one byte per point ({} total, {on_curve_count} on-curve): bit 0 on-curve, bit 1 x fits in a byte, bit 2 y fits in a byte, bits 4-5 sign or same-as-previous",
                flags.len()
            ),
        );
        push_field(
            &mut bytes,
            &mut fields,
            "xCoordinates",
            &x_bytes,
            "x deltas from the previous point, first relative to the origin"
                .to_owned(),
        );
        push_field(
            &mut bytes,
            &mut fields,
            "yCoordinates",
            &y_bytes,
            "y deltas, same encoding — deltas are why outlines compress so well"
                .to_owned(),
        );

        (bytes, fields)
    }
}

/// Appends a field's bytes and records its description.
fn push_field(
    bytes: &mut Vec<u8>,
    fields: &mut Vec<Field>,
    label: &str,
    field_bytes: &[u8],
    meaning: String,
) {
    bytes.extend_from_slice(field_bytes);
    fields.push(Field {
        label: label.to_owned(),
        bytes: field_bytes.to_vec(),
        meaning,
    });
}

/// Encodes one coordinate delta, returning the flag bits it needs.
///
/// Three cases, in order of how many bytes they cost: zero deltas cost nothing
/// and set only the same-or-positive bit, deltas within ±255 cost one byte plus
/// a sign bit, and everything else costs a signed 16-bit value.
fn encode_delta(delta: i16, short_flag: u8, sign_flag: u8, out: &mut Vec<u8>) -> u8 {
    if delta == 0 {
        sign_flag
    } else if let Ok(magnitude) = u8::try_from(delta.abs()) {
        out.push(magnitude);
        if delta > 0 {
            short_flag | sign_flag
        } else {
            short_flag
        }
    } else {
        out.extend_from_slice(&delta.to_be_bytes());
        0
    }
}

/// Builds outlines with a familiar pen API.
///
/// The pen closes each contour implicitly, matching TrueType, so there is no
/// `close_to_start` step to forget.
#[derive(Debug, Default)]
pub struct Pen {
    contours: Vec<Contour>,
    current: Vec<Point>,
}

impl Pen {
    /// A fresh pen.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a new contour at `(x, y)`, finishing any contour in progress.
    #[must_use]
    pub fn move_to(mut self, x: i16, y: i16) -> Self {
        self.finish_contour();
        self.current.push(Point::on(x, y));
        self
    }

    /// Straight line to `(x, y)`.
    #[must_use]
    pub fn line_to(mut self, x: i16, y: i16) -> Self {
        self.current.push(Point::on(x, y));
        self
    }

    /// Quadratic Bézier to `(x, y)` bending towards control point `(cx, cy)`.
    #[must_use]
    pub fn quad_to(mut self, cx: i16, cy: i16, x: i16, y: i16) -> Self {
        self.current.push(Point::off(cx, cy));
        self.current.push(Point::on(x, y));
        self
    }

    /// Appends a pre-computed run of points to the current contour.
    #[must_use]
    pub fn points(mut self, points: impl IntoIterator<Item = Point>) -> Self {
        self.current.extend(points);
        self
    }

    /// Finishes the outline.
    #[must_use]
    pub fn build(mut self) -> Outline {
        self.finish_contour();
        Outline {
            contours: self.contours,
        }
    }

    fn finish_contour(&mut self) {
        if !self.current.is_empty() {
            self.contours.push(Contour {
                points: std::mem::take(&mut self.current),
            });
        }
    }
}

/// Points approximating a circle with eight quadratic arcs.
///
/// One quadratic per quadrant is visibly fat — off by about 2.7% of the radius
/// — so this uses eight, placing each control point where the tangents at the
/// arc's ends meet: `radius / cos(22.5°)` out along the bisector.
///
/// `clockwise` picks the winding direction, which is how a hole is
/// distinguished from a solid: an outer contour and an inner contour wound the
/// same way merge instead of cutting.
#[must_use]
pub fn circle(center_x: i16, center_y: i16, radius: i16, clockwise: bool) -> Vec<Point> {
    const SEGMENTS: u16 = 8;
    let step = std::f64::consts::TAU / f64::from(SEGMENTS);
    let control_distance = f64::from(radius) / (step / 2.0).cos();
    let sign = if clockwise { -1.0 } else { 1.0 };

    let mut points = Vec::with_capacity(usize::from(SEGMENTS) * 2);
    for segment in 0..SEGMENTS {
        let start = sign * f64::from(segment) * step;
        let mid = start + sign * step / 2.0;
        points.push(Point::on(
            round(f64::from(center_x) + f64::from(radius) * start.cos()),
            round(f64::from(center_y) + f64::from(radius) * start.sin()),
        ));
        points.push(Point::off(
            round(f64::from(center_x) + control_distance * mid.cos()),
            round(f64::from(center_y) + control_distance * mid.sin()),
        ));
    }
    points
}

/// Rounds a computed coordinate and clamps it into the em coordinate type.
///
/// Curve maths happens in `f64` — trigonometry for arcs, interpolation for
/// flattening — and has to land back in the `i16` that `glyf` stores.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "clamped into i16 range on the lines above, so the cast is exact"
)]
pub fn round(value: f64) -> i16 {
    let rounded = value.round();
    if rounded > f64::from(i16::MAX) {
        i16::MAX
    } else if rounded < f64::from(i16::MIN) {
        i16::MIN
    } else {
        rounded as i16
    }
}

/// A hex dump with an ASCII gutter, `width` bytes per row.
#[must_use]
pub fn hex_dump(bytes: &[u8], width: usize) -> Vec<String> {
    bytes
        .chunks(width)
        .enumerate()
        .map(|(row, chunk)| {
            let hex: Vec<String> = chunk.iter().map(|byte| format!("{byte:02x}")).collect();
            let ascii: String = chunk
                .iter()
                .map(|&byte| {
                    if byte.is_ascii_graphic() {
                        char::from(byte)
                    } else {
                        '.'
                    }
                })
                .collect();
            format!(
                "{:04x}  {:<width$}  {ascii}",
                row * width,
                hex.join(" "),
                width = width * 3 - 1
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The simplest useful glyph: a filled triangle, three on-curve points.
    fn triangle() -> Outline {
        Pen::new()
            .move_to(0, 0)
            .line_to(500, 1000)
            .line_to(1000, 0)
            .build()
    }

    #[test]
    fn triangle_encodes_to_the_expected_bytes() {
        let bytes = triangle().encode();
        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0x00, 0x01,                                     // numberOfContours = 1
            0x00, 0x00, 0x00, 0x00, 0x03, 0xE8, 0x03, 0xE8, // bbox 0,0 - 1000,1000
            0x00, 0x02,                                     // endPtsOfContours = [2]
            0x00, 0x00,                                     // instructionLength = 0
            0x31, 0x01, 0x01,                               // flags
            0x01, 0xF4, 0x01, 0xF4,                         // x deltas: +500, +500
            0x03, 0xE8, 0xFC, 0x18,                         // y deltas: +1000, -1000
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn first_point_at_the_origin_costs_no_coordinate_bytes() {
        // Delta of zero sets the same-or-positive bit and emits nothing.
        let (_, fields) = triangle().encode_with_fields();
        let flags = fields.iter().find(|f| f.label == "flags").unwrap();
        assert_eq!(
            flags.bytes[0],
            FLAG_ON_CURVE | FLAG_X_SAME_OR_POSITIVE | FLAG_Y_SAME_OR_POSITIVE
        );
    }

    #[test]
    fn small_deltas_use_the_one_byte_form() {
        let outline = Pen::new()
            .move_to(0, 0)
            .line_to(100, 0)
            .line_to(100, 100)
            .build();
        let (_, fields) = outline.encode_with_fields();
        let x = fields.iter().find(|f| f.label == "xCoordinates").unwrap();
        // Only the +100 step costs a byte; the other two x deltas are zero.
        assert_eq!(x.bytes, vec![100]);

        let flags = fields.iter().find(|f| f.label == "flags").unwrap();
        assert_eq!(flags.bytes[1] & FLAG_X_SHORT, FLAG_X_SHORT);
        assert_eq!(
            flags.bytes[1] & FLAG_X_SAME_OR_POSITIVE,
            FLAG_X_SAME_OR_POSITIVE,
            "a positive short delta sets the sign bit"
        );
    }

    #[test]
    fn negative_short_deltas_clear_the_sign_bit() {
        let outline = Pen::new().move_to(200, 0).line_to(100, 0).build();
        let (_, fields) = outline.encode_with_fields();
        let flags = fields.iter().find(|f| f.label == "flags").unwrap();
        assert_eq!(flags.bytes[1] & FLAG_X_SHORT, FLAG_X_SHORT);
        assert_eq!(flags.bytes[1] & FLAG_X_SAME_OR_POSITIVE, 0);
        let x = fields.iter().find(|f| f.label == "xCoordinates").unwrap();
        assert_eq!(x.bytes, vec![200, 100], "magnitudes, with sign in the flags");
    }

    #[test]
    fn fields_account_for_every_encoded_byte() {
        let outline = Pen::new()
            .move_to(0, 0)
            .quad_to(500, 900, 1000, 0)
            .build();
        let (bytes, fields) = outline.encode_with_fields();
        let from_fields: Vec<u8> = fields.iter().flat_map(|f| f.bytes.clone()).collect();
        assert_eq!(
            bytes, from_fields,
            "the walkthrough must describe the whole payload"
        );
    }

    #[test]
    fn quad_to_adds_an_off_curve_control_point() {
        let outline = Pen::new()
            .move_to(0, 0)
            .quad_to(500, 900, 1000, 0)
            .build();
        assert_eq!(outline.point_count(), 3);
        let on_curve: Vec<bool> = outline.points().map(|p| p.on_curve).collect();
        assert_eq!(on_curve, vec![true, false, true]);
    }

    #[test]
    fn multiple_contours_get_separate_end_points() {
        let ring = Pen::new()
            .move_to(0, 0)
            .line_to(1000, 0)
            .line_to(1000, 1000)
            .move_to(200, 200)
            .line_to(800, 200)
            .line_to(800, 800)
            .build();
        assert_eq!(ring.contours.len(), 2);
        let (_, fields) = ring.encode_with_fields();
        let ends = fields
            .iter()
            .find(|f| f.label == "endPtsOfContours")
            .unwrap();
        assert_eq!(ends.bytes, vec![0x00, 0x02, 0x00, 0x05]);
    }

    #[test]
    fn bounds_cover_control_points_too() {
        let outline = Pen::new().move_to(0, 0).quad_to(500, 1200, 1000, 0).build();
        assert_eq!(outline.bounds(), (0, 0, 1000, 1200));
    }

    #[test]
    fn empty_outline_encodes_without_panicking() {
        let outline = Outline::default();
        assert_eq!(outline.bounds(), (0, 0, 0, 0));
        assert_eq!(outline.point_count(), 0);
        assert!(!outline.encode().is_empty());
    }

    #[test]
    fn circle_alternates_on_and_off_curve_points() {
        let points = circle(500, 500, 400, true);
        assert_eq!(points.len(), 16, "eight quadratic arcs");
        for (index, point) in points.iter().enumerate() {
            assert_eq!(point.on_curve, index % 2 == 0);
        }
    }

    #[test]
    fn circle_on_curve_points_sit_on_the_radius() {
        let (cx, cy, r) = (500.0, 500.0, 400.0);
        for point in circle(500, 500, 400, false).iter().filter(|p| p.on_curve) {
            let distance =
                ((f64::from(point.x) - cx).powi(2) + (f64::from(point.y) - cy).powi(2)).sqrt();
            assert!(
                (distance - r).abs() <= 1.0,
                "point {point:?} is {distance} from the centre, expected {r}"
            );
        }
    }

    #[test]
    fn winding_direction_reverses() {
        let clockwise = circle(500, 500, 400, true);
        let counter = circle(500, 500, 400, false);
        assert_eq!(clockwise[0], counter[0], "both start at angle zero");
        assert_ne!(
            clockwise[2], counter[2],
            "and then run in opposite directions"
        );
    }

    #[test]
    fn base64_round_trips() {
        let outline = triangle();
        let encoded = outline.encode_base64();
        assert!(!encoded.contains('\n'), "must be one APC-safe line");
        assert_eq!(BASE64_STANDARD.decode(&encoded).unwrap(), outline.encode());
    }

    #[test]
    fn payload_stays_in_the_size_range_the_spec_describes() {
        // The spec puts a typical icon at 150-400 bytes of glyf data.
        let icon = Pen::new()
            .points(circle(500, 500, 450, true))
            .points(circle(500, 500, 300, false))
            .build();
        let size = icon.encode().len();
        assert!(size < 400, "a two-contour icon came to {size} bytes");
    }

    #[test]
    fn hex_dump_lays_out_offset_hex_and_ascii() {
        let rows = hex_dump(b"ABCD", 4);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].starts_with("0000  41 42 43 44"));
        assert!(rows[0].ends_with("ABCD"));
    }
}
