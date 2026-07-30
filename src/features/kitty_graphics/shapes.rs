//! Images generated from arithmetic, so `render` has something to show without
//! an input file.
//!
//! These are not decoration. Each one answers a question you actually have about
//! a terminal's graphics implementation:
//!
//! * [`Shape::Gradient`] — is the terminal dithering or truncating colour?
//! * [`Shape::Checkerboard`] — is alpha composited, and against what?
//! * [`Shape::ColorWheel`] — is the colour space sRGB, as the spec requires?
//! * [`Shape::Mandelbrot`] — how does fine detail survive being scaled to cells?
//! * [`Shape::Rings`] — does scaling alias? Concentric rings make moiré obvious.
//! * [`Shape::TestCard`] — where exactly are the image's edges, and is the aspect
//!   ratio preserved?
//!
//! Everything here is pure arithmetic over [`Rgba`]: no image decoding, no
//! fonts, nothing to install.

use anyhow::Result;

use crate::{cli::Shape, features::kitty_graphics::pixels::Rgba};

/// Default edge length for a generated image, in pixels.
///
/// Large enough that a terminal scaling it down to a few cells has real detail to
/// work with, small enough that the base64 payload stays around 300KB.
pub const DEFAULT_SIZE: u32 = 480;

/// Generates `shape` at `width` × `height` pixels.
///
/// # Errors
///
/// Fails if either dimension is zero.
pub fn generate(shape: Shape, width: u32, height: u32) -> Result<Rgba> {
    match shape {
        Shape::Gradient => gradient(width, height),
        Shape::Checkerboard => checkerboard(width, height),
        Shape::ColorWheel => color_wheel(width, height),
        Shape::Mandelbrot => mandelbrot(width, height),
        Shape::Rings => rings(width, height),
        Shape::TestCard => test_card(width, height),
    }
}

/// The shape's name as `--shape` spells it.
#[must_use]
pub fn name(shape: Shape) -> &'static str {
    match shape {
        Shape::Gradient => "gradient",
        Shape::Checkerboard => "checkerboard",
        Shape::ColorWheel => "color-wheel",
        Shape::Mandelbrot => "mandelbrot",
        Shape::Rings => "rings",
        Shape::TestCard => "test-card",
    }
}

/// One line about what the shape is for.
#[must_use]
pub fn purpose(shape: Shape) -> &'static str {
    match shape {
        Shape::Gradient => "a smooth two-axis ramp — shows colour quantisation and dithering",
        Shape::Checkerboard => "opaque and transparent squares — shows what alpha composites against",
        Shape::ColorWheel => "hue around, saturation outwards — shows the colour space",
        Shape::Mandelbrot => "fine detail everywhere — shows what scaling to cells costs",
        Shape::Rings => "concentric rings — makes resampling moiré obvious",
        Shape::TestCard => "edges, centre and a grey ramp — shows the exact image bounds",
    }
}

/// A two-axis ramp: red rightwards, green downwards, blue along the diagonal.
///
/// # Errors
///
/// Fails if either dimension is zero.
#[allow(clippy::cast_possible_truncation)]
pub fn gradient(width: u32, height: u32) -> Result<Rgba> {
    Rgba::generate(width, height, |x, y| {
        let red = scale(x, width);
        let green = scale(y, height);
        // The diagonal keeps the corners from being pure primaries, which makes
        // banding easier to see.
        let blue = 255 - (u16::from(red) + u16::from(green)).min(255) as u8;
        [red, green, blue, 0xff]
    })
}

/// Alternating opaque and fully transparent squares.
///
/// This is the one shape whose *point* is the alpha channel: whatever shows
/// through the transparent squares is what the terminal composites images
/// against — the cell background, the text behind them, or another image at a
/// lower z-index.
///
/// # Errors
///
/// Fails if either dimension is zero.
pub fn checkerboard(width: u32, height: u32) -> Result<Rgba> {
    let square = (width.min(height) / 8).max(1);
    Rgba::generate(width, height, |x, y| {
        let (column, row) = (x / square, y / square);
        if (column + row) % 2 == 0 {
            // A colour that is unmistakably ours rather than a terminal default.
            [0x2e, 0xa0, 0xc0, 0xff]
        } else {
            [0, 0, 0, 0]
        }
    })
}

/// An HSV wheel: hue by angle, saturation by radius, transparent outside.
///
/// # Errors
///
/// Fails if either dimension is zero.
#[allow(clippy::cast_precision_loss)]
pub fn color_wheel(width: u32, height: u32) -> Result<Rgba> {
    let center_x = f64::from(width) / 2.0;
    let center_y = f64::from(height) / 2.0;
    let radius = center_x.min(center_y);
    Rgba::generate(width, height, |x, y| {
        let dx = f64::from(x) - center_x + 0.5;
        let dy = f64::from(y) - center_y + 0.5;
        let distance = dx.hypot(dy);
        if distance > radius {
            return [0, 0, 0, 0];
        }
        // atan2 runs -pi..pi; shift to 0..1 turns.
        let hue = dy.atan2(dx) / std::f64::consts::TAU + 0.5;
        let saturation = (distance / radius).min(1.0);
        let [red, green, blue] = hsv_to_rgb(hue, saturation, 1.0);
        // One pixel of feathering at the rim, so the circle does not look
        // stair-stepped before the terminal ever scales it.
        let edge = (radius - distance).clamp(0.0, 1.0);
        [red, green, blue, quantize(edge)]
    })
}

/// The Mandelbrot set, smooth-coloured by escape time.
///
/// # Errors
///
/// Fails if either dimension is zero.
#[allow(clippy::cast_precision_loss)]
pub fn mandelbrot(width: u32, height: u32) -> Result<Rgba> {
    /// Enough to resolve the filaments at this size without being slow.
    const MAX_ITERATIONS: u32 = 300;
    // A view that puts the seahorse valley on screen rather than centring the
    // boring cardioid.
    let (left, right) = (-2.2, 0.8);
    let (top, bottom) = (-1.25, 1.25);

    Rgba::generate(width, height, |px, py| {
        let cx = left + (right - left) * f64::from(px) / f64::from(width);
        let cy = top + (bottom - top) * f64::from(py) / f64::from(height);
        let (mut zx, mut zy) = (0.0f64, 0.0f64);
        let mut iterations = 0;
        while zx * zx + zy * zy <= 4.0 && iterations < MAX_ITERATIONS {
            let next_x = zx * zx - zy * zy + cx;
            zy = 2.0 * zx * zy + cy;
            zx = next_x;
            iterations += 1;
        }
        if iterations == MAX_ITERATIONS {
            return [0x10, 0x10, 0x18, 0xff];
        }
        // Normalised iteration count, which removes the banding plain escape
        // time gives.
        let magnitude = (zx * zx + zy * zy).sqrt().max(1.000_001);
        let smooth = f64::from(iterations) + 1.0 - magnitude.ln().ln() / std::f64::consts::LN_2;
        let hue = 0.62 + 0.35 * (smooth / 40.0).fract();
        let [red, green, blue] = hsv_to_rgb(hue.fract(), 0.75, (smooth / 18.0).min(1.0));
        [red, green, blue, 0xff]
    })
}

/// Concentric rings, spaced so that resampling produces visible moiré.
///
/// # Errors
///
/// Fails if either dimension is zero.
#[allow(clippy::cast_precision_loss)]
pub fn rings(width: u32, height: u32) -> Result<Rgba> {
    let center_x = f64::from(width) / 2.0;
    let center_y = f64::from(height) / 2.0;
    let period = f64::from(width.min(height)) / 48.0;
    Rgba::generate(width, height, |x, y| {
        let distance = (f64::from(x) - center_x).hypot(f64::from(y) - center_y);
        // A sine in the radius: alias-prone by construction, which is the point.
        let wave = (distance / period * std::f64::consts::TAU).sin();
        let value = quantize(f64::midpoint(wave, 1.0));
        [value, value, value, 0xff]
    })
}

/// A test card: a one-pixel border, corner brackets, a centre cross, a cell grid
/// and a greyscale ramp.
///
/// Every element answers "where does the image actually start and stop?", which
/// is the first thing that goes wrong when `c`, `r`, `X` and `Y` are involved.
///
/// # Errors
///
/// Fails if either dimension is zero.
#[allow(clippy::cast_possible_truncation)]
pub fn test_card(width: u32, height: u32) -> Result<Rgba> {
    let bracket = (width.min(height) / 8).max(2);
    let grid = (width.min(height) / 12).max(4);
    let ramp_height = (height / 10).max(2);

    Rgba::generate(width, height, |x, y| {
        let last_x = width - 1;
        let last_y = height - 1;

        // The outermost pixels: a bright border proves nothing was cropped.
        if x == 0 || y == 0 || x == last_x || y == last_y {
            return [0xff, 0x30, 0x30, 0xff];
        }
        // Corner brackets, in case the border is lost to rounding.
        let near_corner = (x < bracket || x > last_x - bracket)
            && (y < bracket || y > last_y - bracket);
        if near_corner && (x < 3 || y < 3 || x > last_x - 3 || y > last_y - 3) {
            return [0xff, 0xff, 0xff, 0xff];
        }
        // A greyscale ramp along the bottom, in 16 steps.
        if y > last_y - ramp_height {
            let step = (x * 16 / width).min(15);
            let value = (step * 17) as u8;
            return [value, value, value, 0xff];
        }
        // Centre cross.
        let center_x = width / 2;
        let center_y = height / 2;
        if x.abs_diff(center_x) < 2 || y.abs_diff(center_y) < 2 {
            return [0x30, 0xff, 0x90, 0xff];
        }
        // A faint grid everywhere else, so scaling artefacts have something to
        // beat against.
        if x % grid == 0 || y % grid == 0 {
            return [0x50, 0x50, 0x60, 0xff];
        }
        [0x18, 0x18, 0x20, 0xff]
    })
}

/// Maps `value` in `0..extent` onto `0..=255`.
#[allow(clippy::cast_possible_truncation)]
fn scale(value: u32, extent: u32) -> u8 {
    if extent <= 1 {
        return 0;
    }
    // `value` is always below `extent`, so the product cannot exceed 255.
    (value * 255 / (extent - 1)).min(255) as u8
}

/// Rounds a `0.0..=1.0` fraction to a byte.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn quantize(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Converts HSV to sRGB bytes. `hue` is in turns, `saturation` and `value` in
/// `0.0..=1.0`.
///
/// The protocol requires sRGB, so this deliberately does *not* do any gamma
/// juggling: the components come out as the sRGB-encoded values a terminal
/// expects to blend directly.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn hsv_to_rgb(hue: f64, saturation: f64, value: f64) -> [u8; 3] {
    let hue = hue.rem_euclid(1.0) * 6.0;
    let sector = hue.floor();
    let fraction = hue - sector;
    let p = value * (1.0 - saturation);
    let q = value * (1.0 - saturation * fraction);
    let t = value * (1.0 - saturation * (1.0 - fraction));
    let (red, green, blue) = match sector as u32 % 6 {
        0 => (value, t, p),
        1 => (q, value, p),
        2 => (p, value, t),
        3 => (p, q, value),
        4 => (t, p, value),
        _ => (value, p, q),
    };
    [quantize(red), quantize(green), quantize(blue)]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Every shape `--shape` accepts.
    const ALL: [Shape; 6] = [
        Shape::Gradient,
        Shape::Checkerboard,
        Shape::ColorWheel,
        Shape::Mandelbrot,
        Shape::Rings,
        Shape::TestCard,
    ];

    #[test]
    fn every_shape_generates_at_the_requested_size() {
        for shape in ALL {
            for (width, height) in [(1, 1), (17, 5), (64, 64)] {
                let image = generate(shape, width, height).unwrap();
                assert_eq!(
                    (image.width(), image.height()),
                    (width, height),
                    "{}",
                    name(shape)
                );
            }
        }
    }

    #[test]
    fn every_shape_has_a_name_and_a_purpose() {
        for shape in ALL {
            assert!(!name(shape).is_empty());
            assert!(!purpose(shape).is_empty());
        }
        assert_eq!(name(Shape::ColorWheel), "color-wheel");
    }

    #[test]
    fn zero_sized_requests_are_refused() {
        for shape in ALL {
            assert!(generate(shape, 0, 8).is_err(), "{}", name(shape));
            assert!(generate(shape, 8, 0).is_err(), "{}", name(shape));
        }
    }

    #[test]
    fn only_the_two_shapes_that_should_be_transparent_are() {
        // Which matters because `render` picks `f=24` over `f=32` for an image with
        // nothing transparent in it, saving a quarter of the payload.
        for shape in ALL {
            let image = generate(shape, 64, 64).unwrap();
            let expected = matches!(shape, Shape::Checkerboard | Shape::ColorWheel);
            assert_eq!(
                image.has_transparency(),
                expected,
                "{} transparency",
                name(shape)
            );
        }
    }

    #[test]
    fn the_gradient_reaches_the_ends_of_its_ramps() {
        let image = gradient(256, 256).unwrap();
        assert_eq!(image.get(0, 0)[0], 0, "red starts at zero");
        assert_eq!(image.get(255, 0)[0], 255, "and reaches full");
        assert_eq!(image.get(0, 255)[1], 255, "green runs down the other axis");
    }

    #[test]
    fn the_checkerboard_alternates_opaque_and_transparent() {
        let image = checkerboard(64, 64).unwrap();
        // Squares are an eighth of the short side, so 8 pixels here.
        assert_eq!(image.get(0, 0)[3], 0xff, "top left square is opaque");
        assert_eq!(image.get(8, 0)[3], 0x00, "its neighbour is transparent");
        assert_eq!(image.get(0, 8)[3], 0x00, "and so is the one below");
        assert_eq!(image.get(8, 8)[3], 0xff, "diagonal neighbours match");
    }

    #[test]
    fn the_checkerboard_survives_being_smaller_than_one_square() {
        let image = checkerboard(3, 3).unwrap();
        assert_eq!(image.width(), 3);
    }

    #[test]
    fn the_colour_wheel_is_a_circle_with_transparent_corners() {
        let image = color_wheel(128, 128).unwrap();
        assert_eq!(image.get(0, 0)[3], 0, "the corner is outside the circle");
        assert_eq!(image.get(64, 64)[3], 0xff, "the centre is opaque");
        // The centre has saturation zero, which in HSV is white.
        let center = image.get(64, 64);
        assert!(
            center[0] > 200 && center[1] > 200 && center[2] > 200,
            "unsaturated centre should be near white, got {center:?}"
        );
    }

    #[test]
    fn the_colour_wheel_puts_different_hues_on_opposite_sides() {
        let image = color_wheel(128, 128).unwrap();
        let left = image.get(4, 64);
        let right = image.get(123, 64);
        assert_ne!(left, right, "opposite rim points must differ in hue");
    }

    #[test]
    fn the_mandelbrot_interior_is_dark_and_the_exterior_is_not() {
        let image = mandelbrot(128, 128).unwrap();
        // (-0.5, 0) is deep inside the cardioid; with left=-2.2, right=0.8 that
        // is x = (−0.5 + 2.2)/3 * 128 ≈ 72, y = 64.
        let inside = image.get(72, 64);
        assert_eq!(inside, [0x10, 0x10, 0x18, 0xff], "interior is flat and dark");
        // The far left of the view is well outside the set.
        let outside = image.get(1, 64);
        assert_ne!(outside, inside);
    }

    #[test]
    fn the_rings_oscillate_from_the_centre_outwards() {
        let image = rings(480, 480).unwrap();
        let along: Vec<u8> = (240..480).map(|x| image.get(x, 240)[0]).collect();
        let minimum = along.iter().copied().min().unwrap();
        let maximum = along.iter().copied().max().unwrap();
        assert!(minimum < 20, "rings should reach black, got {minimum}");
        assert!(maximum > 235, "and white, got {maximum}");
        // Count sign changes about the midpoint: several rings must be crossed.
        let crossings = along
            .windows(2)
            .filter(|pair| (pair[0] < 128) != (pair[1] < 128))
            .count();
        assert!(crossings >= 8, "expected several rings, counted {crossings}");
    }

    #[test]
    fn the_test_card_has_a_border_on_every_edge() {
        let image = test_card(96, 64).unwrap();
        let border = [0xff, 0x30, 0x30, 0xff];
        assert_eq!(image.get(0, 0), border);
        assert_eq!(image.get(95, 0), border);
        assert_eq!(image.get(0, 63), border);
        assert_eq!(image.get(95, 63), border);
        assert_eq!(image.get(48, 0), border, "top edge");
        assert_eq!(image.get(0, 32), border, "left edge");
    }

    #[test]
    fn the_test_card_ramp_climbs_from_black_to_white() {
        let image = test_card(320, 160).unwrap();
        // Sample inside the ramp but clear of the corner brackets.
        let row = 158;
        let first = image.get(25, row)[0];
        let last = image.get(300, row)[0];
        assert!(first < 40, "ramp starts dark, got {first}");
        assert!(last > 220, "ramp ends bright, got {last}");
    }

    #[test]
    fn hsv_primaries_land_where_they_should() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), [255, 0, 0], "red");
        assert_eq!(hsv_to_rgb(1.0 / 3.0, 1.0, 1.0), [0, 255, 0], "green");
        assert_eq!(hsv_to_rgb(2.0 / 3.0, 1.0, 1.0), [0, 0, 255], "blue");
        assert_eq!(hsv_to_rgb(0.0, 0.0, 1.0), [255, 255, 255], "white");
        assert_eq!(hsv_to_rgb(0.0, 0.0, 0.0), [0, 0, 0], "black");
        // Hue wraps.
        assert_eq!(hsv_to_rgb(1.0, 1.0, 1.0), hsv_to_rgb(0.0, 1.0, 1.0));
        assert_eq!(hsv_to_rgb(-0.5, 1.0, 1.0), hsv_to_rgb(0.5, 1.0, 1.0));
    }

    #[test]
    fn quantize_and_scale_stay_in_range() {
        assert_eq!(quantize(-1.0), 0);
        assert_eq!(quantize(0.5), 128);
        assert_eq!(quantize(2.0), 255);
        assert_eq!(scale(0, 1), 0, "a single-pixel axis has no ramp");
        assert_eq!(scale(0, 256), 0);
        assert_eq!(scale(255, 256), 255);
    }

    #[test]
    fn the_default_size_keeps_payloads_reasonable() {
        // 480x480 RGBA is 921600 bytes, or about 1.2MB of base64 — big, but the
        // shapes are the one place we choose the resolution ourselves.
        let bytes = DEFAULT_SIZE as usize * DEFAULT_SIZE as usize * 4;
        assert!(bytes < 1_000_000, "{bytes} bytes per generated shape");
    }
}
