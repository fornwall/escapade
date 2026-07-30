//! Straight 8-bit RGBA pixels, and the two things we ever do to them:
//! resample, and composite onto a background.
//!
//! The protocol's `f=32` wants exactly this — four bytes per pixel, sRGB, alpha
//! *not* premultiplied, top row first — so this is the common currency between
//! the decoders, the SVG rasterizer, the procedural shapes and the wire.

use std::fmt;

use anyhow::{Context as _, Result, bail};

/// Bytes per pixel in the `f=32` format.
pub const CHANNELS: usize = 4;

/// An 8-bit RGBA image in the sRGB colour space, rows top to bottom.
#[derive(Clone, PartialEq, Eq)]
pub struct Rgba {
    /// Width in pixels. Never zero.
    width: u32,
    /// Height in pixels. Never zero.
    height: u32,
    /// `width * height * 4` bytes of straight (non-premultiplied) RGBA.
    data: Vec<u8>,
}

impl fmt::Debug for Rgba {
    /// Prints the shape, never the pixels — a debug print of a 4K frame would be
    /// 33 million characters.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rgba")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl Rgba {
    /// A fully transparent image.
    ///
    /// # Errors
    ///
    /// Fails if either dimension is zero, or if the pixel count overflows
    /// `usize` on this platform.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let len = byte_len(width, height)?;
        Ok(Self {
            width,
            height,
            data: vec![0; len],
        })
    }

    /// Wraps existing RGBA bytes.
    ///
    /// # Errors
    ///
    /// Fails if `data` is not exactly `width * height * 4` bytes long.
    pub fn from_raw(width: u32, height: u32, data: Vec<u8>) -> Result<Self> {
        let expected = byte_len(width, height)?;
        if data.len() != expected {
            bail!(
                "expected {expected} bytes for a {width}x{height} RGBA image, got {}",
                data.len()
            );
        }
        Ok(Self {
            width,
            height,
            data,
        })
    }

    /// Builds an image from a function of pixel coordinates.
    ///
    /// # Errors
    ///
    /// Fails if either dimension is zero.
    pub fn generate(
        width: u32,
        height: u32,
        mut shade: impl FnMut(u32, u32) -> [u8; 4],
    ) -> Result<Self> {
        let mut image = Self::new(width, height)?;
        for y in 0..height {
            for x in 0..width {
                image.set(x, y, shade(x, y));
            }
        }
        Ok(image)
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The raw `f=32` payload.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Byte offset of a pixel, or `None` when it is outside the image.
    fn offset(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = y as usize * self.width as usize + x as usize;
        Some(index * CHANNELS)
    }

    /// The pixel at `(x, y)`, or transparent black outside the image.
    #[must_use]
    pub fn get(&self, x: u32, y: u32) -> [u8; 4] {
        match self.offset(x, y) {
            Some(offset) => [
                self.data[offset],
                self.data[offset + 1],
                self.data[offset + 2],
                self.data[offset + 3],
            ],
            None => [0, 0, 0, 0],
        }
    }

    /// Sets the pixel at `(x, y)`. Coordinates outside the image are ignored.
    pub fn set(&mut self, x: u32, y: u32, rgba: [u8; 4]) {
        if let Some(offset) = self.offset(x, y) {
            self.data[offset..offset + CHANNELS].copy_from_slice(&rgba);
        }
    }

    /// Whether any pixel is not fully opaque.
    ///
    /// Worth knowing before choosing a format: an image with no transparency can
    /// go out as `f=24` and save a quarter of the bytes.
    #[must_use]
    pub fn has_transparency(&self) -> bool {
        self.data
            .chunks_exact(CHANNELS)
            .any(|pixel| pixel[3] != 0xff)
    }

    /// Composites the image over an opaque background and drops the alpha
    /// channel, producing an `f=24` payload.
    ///
    /// Alpha is treated as straight, so the arithmetic is the textbook
    /// `src * a + dst * (1 - a)` per channel, in 8-bit integers with rounding.
    #[must_use]
    pub fn flatten(&self, background: Colour) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.width as usize * self.height as usize * 3);
        for pixel in self.data.chunks_exact(CHANNELS) {
            let alpha = u32::from(pixel[3]);
            for (index, &source) in pixel[..3].iter().enumerate() {
                let source = u32::from(source);
                let destination = u32::from(background.channel(index));
                // Round to nearest rather than truncating, so a 50% blend of 0
                // and 255 is 128 and not 127. The result is a convex combination
                // of two bytes and so cannot exceed 255.
                let blended = (source * alpha + destination * (255 - alpha) + 127) / 255;
                out.push(u8::try_from(blended).unwrap_or(u8::MAX));
            }
        }
        out
    }

    /// Composites the image over an opaque background, keeping four channels.
    #[must_use]
    pub fn over(&self, background: Colour) -> Self {
        let flat = self.flatten(background);
        let mut data = Vec::with_capacity(self.data.len());
        for pixel in flat.chunks_exact(3) {
            data.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0xff]);
        }
        Self {
            width: self.width,
            height: self.height,
            data,
        }
    }

    /// Encodes the image as a PNG.
    ///
    /// This exists for the demo, which wants to show `f=100` without shipping a
    /// sample file. `render` never encodes a PNG — it forwards the one it was
    /// given. The encoder comes free with tiny-skia, which is already in the
    /// dependency tree for SVG rasterization.
    ///
    /// # Errors
    ///
    /// Fails if the dimensions are not ones tiny-skia accepts, or if encoding
    /// fails.
    pub fn encode_png(&self) -> Result<Vec<u8>> {
        // tiny-skia stores premultiplied alpha; the protocol's f=32 does not.
        let mut premultiplied = Vec::with_capacity(self.data.len());
        for pixel in self.data.chunks_exact(CHANNELS) {
            let alpha = u32::from(pixel[3]);
            for &channel in &pixel[..3] {
                let scaled = (u32::from(channel) * alpha + 127) / 255;
                premultiplied.push(u8::try_from(scaled).unwrap_or(u8::MAX));
            }
            premultiplied.push(pixel[3]);
        }
        let size = resvg::tiny_skia::IntSize::from_wh(self.width, self.height)
            .context("a PNG cannot be encoded at this size")?;
        let pixmap = resvg::tiny_skia::Pixmap::from_vec(premultiplied, size)
            .context("building a pixmap for PNG encoding")?;
        pixmap.encode_png().context("encoding a PNG")
    }

    /// Resamples to `width` × `height`.
    ///
    /// This is a **box filter**: each destination pixel is the unweighted average
    /// of every source pixel whose area it covers, computed in premultiplied
    /// space so that transparent pixels do not drag their colour into the
    /// average. For downscaling that is the right thing — it is what
    /// "area averaging" means, and it does not alias the way point sampling does.
    /// For upscaling a box filter degenerates to nearest-neighbour, which is
    /// visibly blocky; escapade avoids that case entirely by never upscaling
    /// itself. Enlarging is the *terminal's* job, via the `c` and `r` keys, which
    /// keeps the full-resolution pixels available for a terminal on a HiDPI
    /// screen.
    ///
    /// # Errors
    ///
    /// Fails if either target dimension is zero.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn resize(&self, width: u32, height: u32) -> Result<Self> {
        if width == self.width && height == self.height {
            return Ok(self.clone());
        }
        let mut out = Self::new(width, height)?;
        let x_ratio = f64::from(self.width) / f64::from(width);
        let y_ratio = f64::from(self.height) / f64::from(height);

        for y in 0..height {
            // The source rows this destination row covers, always at least one.
            let y0 = (f64::from(y) * y_ratio).floor() as u32;
            let y1 = ((f64::from(y) + 1.0) * y_ratio).ceil() as u32;
            let y1 = y1.clamp(y0 + 1, self.height);
            for x in 0..width {
                let x0 = (f64::from(x) * x_ratio).floor() as u32;
                let x1 = ((f64::from(x) + 1.0) * x_ratio).ceil() as u32;
                let x1 = x1.clamp(x0 + 1, self.width);
                out.set(x, y, self.average(x0, y0, x1, y1));
            }
        }
        Ok(out)
    }

    /// The premultiplied average of the source rectangle `[x0, x1) × [y0, y1)`,
    /// returned as straight RGBA.
    #[allow(clippy::cast_possible_truncation)]
    fn average(&self, x0: u32, y0: u32, x1: u32, y1: u32) -> [u8; 4] {
        let mut sum = [0u64; 4];
        let mut count = 0u64;
        for y in y0..y1 {
            for x in x0..x1 {
                let pixel = self.get(x, y);
                let alpha = u64::from(pixel[3]);
                // Premultiply before averaging: a transparent pixel contributes
                // its alpha but not its (meaningless) colour.
                sum[0] += u64::from(pixel[0]) * alpha;
                sum[1] += u64::from(pixel[1]) * alpha;
                sum[2] += u64::from(pixel[2]) * alpha;
                sum[3] += alpha;
                count += 1;
            }
        }
        let alpha_sum = sum[3];
        if count == 0 || alpha_sum == 0 {
            return [0, 0, 0, 0];
        }
        // Divide the premultiplied sums by the *sum* of the alphas, which is
        // exactly un-premultiplying by the average alpha. Each quotient is
        // bounded by 255, since every numerator is a sum of `channel * alpha`
        // terms over the same alphas that make up the denominator. The `+ half`
        // terms round to nearest instead of truncating.
        let unpremultiply = |channel: u64| ((channel + alpha_sum / 2) / alpha_sum).min(255) as u8;
        [
            unpremultiply(sum[0]),
            unpremultiply(sum[1]),
            unpremultiply(sum[2]),
            ((alpha_sum + count / 2) / count).min(255) as u8,
        ]
    }
}

/// Total byte length of an RGBA buffer, with the zero and overflow checks.
fn byte_len(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 {
        bail!("an image cannot be {width}x{height}: both dimensions must be positive");
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(CHANNELS))
        .ok_or_else(|| anyhow::anyhow!("a {width}x{height} image does not fit in memory"))
}

/// An opaque 24-bit colour, for backgrounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Colour {
    /// Red.
    pub red: u8,
    /// Green.
    pub green: u8,
    /// Blue.
    pub blue: u8,
}

impl Colour {
    /// Opaque black.
    pub const BLACK: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
    };

    /// Opaque white.
    pub const WHITE: Self = Self {
        red: 0xff,
        green: 0xff,
        blue: 0xff,
    };

    /// Channel `index`, `0` being red.
    #[must_use]
    pub fn channel(self, index: usize) -> u8 {
        match index {
            0 => self.red,
            1 => self.green,
            _ => self.blue,
        }
    }

    /// The `#rrggbb` spelling.
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.red, self.green, self.blue)
    }

    /// As a 32-bit RGBA integer, which is what the animation `Y` key wants.
    #[must_use]
    pub fn to_rgba_u32(self) -> u32 {
        (u32::from(self.red) << 24)
            | (u32::from(self.green) << 16)
            | (u32::from(self.blue) << 8)
            | 0xff
    }
}

/// What `--background` asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Background {
    /// No flattening: transparency is sent to the terminal, which composites the
    /// image over whatever is behind it. The default.
    #[default]
    Keep,
    /// Composite onto this colour before sending.
    Colour(Colour),
}

/// Parses a `--background` value: `#rrggbb`, `#rgb`, a colour name, or `none`.
///
/// The names are the eight ANSI colours plus `grey`/`gray` — enough to be useful
/// at a prompt without pretending to be a CSS colour database.
///
/// # Errors
///
/// Fails on anything unrecognised, listing what is accepted.
pub fn parse_background(text: &str) -> Result<Background> {
    let value = text.trim();
    if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("transparent") {
        return Ok(Background::Keep);
    }
    if let Some(hex) = value.strip_prefix('#') {
        return Ok(Background::Colour(parse_hex(hex)?));
    }
    let named = match value.to_ascii_lowercase().as_str() {
        "black" => Colour::BLACK,
        "white" => Colour::WHITE,
        "red" => Colour {
            red: 0xcc,
            green: 0x24,
            blue: 0x1d,
        },
        "green" => Colour {
            red: 0x98,
            green: 0x97,
            blue: 0x1a,
        },
        "yellow" => Colour {
            red: 0xd7,
            green: 0x99,
            blue: 0x21,
        },
        "blue" => Colour {
            red: 0x45,
            green: 0x85,
            blue: 0x88,
        },
        "magenta" => Colour {
            red: 0xb1,
            green: 0x62,
            blue: 0x86,
        },
        "cyan" => Colour {
            red: 0x68,
            green: 0x9d,
            blue: 0x6a,
        },
        "grey" | "gray" => Colour {
            red: 0x80,
            green: 0x80,
            blue: 0x80,
        },
        _ => bail!(
            "unrecognised colour `{value}`: use #rrggbb, #rgb, `none`, or one of \
             black, white, red, green, yellow, blue, magenta, cyan, grey"
        ),
    };
    Ok(Background::Colour(named))
}

/// Parses `rrggbb` or `rgb` hex digits.
fn parse_hex(hex: &str) -> Result<Colour> {
    let channel = |text: &str| -> Result<u8> {
        u8::from_str_radix(text, 16)
            .map_err(|_| anyhow::anyhow!("`{text}` is not a pair of hex digits"))
    };
    match hex.len() {
        3 => {
            let digits: Vec<char> = hex.chars().collect();
            // `#abc` means `#aabbcc`, as everywhere else.
            let doubled = |index: usize| -> Result<u8> {
                let digit = digits[index];
                channel(&format!("{digit}{digit}"))
            };
            Ok(Colour {
                red: doubled(0)?,
                green: doubled(1)?,
                blue: doubled(2)?,
            })
        }
        6 => Ok(Colour {
            red: channel(&hex[0..2])?,
            green: channel(&hex[2..4])?,
            blue: channel(&hex[4..6])?,
        }),
        other => bail!("a hex colour needs 3 or 6 digits, `#{hex}` has {other}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A 2x2 image: opaque red, opaque green, opaque blue, transparent.
    fn quad() -> Rgba {
        Rgba::from_raw(
            2,
            2,
            vec![
                0xff, 0, 0, 0xff, //
                0, 0xff, 0, 0xff, //
                0, 0, 0xff, 0xff, //
                0, 0, 0, 0x00,
            ],
        )
        .unwrap()
    }

    #[test]
    fn dimensions_and_pixels_round_trip() {
        let image = quad();
        assert_eq!((image.width(), image.height()), (2, 2));
        assert_eq!(image.get(0, 0), [0xff, 0, 0, 0xff]);
        assert_eq!(image.get(1, 1), [0, 0, 0, 0]);
        assert_eq!(image.as_bytes().len(), 2 * 2 * 4);
    }

    #[test]
    fn out_of_bounds_reads_are_transparent_and_writes_are_ignored() {
        let mut image = quad();
        assert_eq!(image.get(99, 99), [0, 0, 0, 0]);
        image.set(99, 99, [1, 2, 3, 4]);
        assert_eq!(image, quad(), "the write must not have landed anywhere");
    }

    #[test]
    fn a_wrong_sized_buffer_is_refused() {
        let error = Rgba::from_raw(2, 2, vec![0; 15]).unwrap_err();
        assert!(error.to_string().contains("expected 16 bytes"), "{error}");
        assert!(Rgba::new(0, 4).is_err(), "zero width");
        assert!(Rgba::new(4, 0).is_err(), "zero height");
    }

    #[test]
    fn transparency_is_detected() {
        assert!(quad().has_transparency());
        assert!(!quad().over(Colour::BLACK).has_transparency());
    }

    #[test]
    fn flattening_drops_the_alpha_channel_and_blends() {
        let flat = quad().flatten(Colour::WHITE);
        assert_eq!(flat.len(), 2 * 2 * 3, "three bytes per pixel now");
        assert_eq!(&flat[0..3], &[0xff, 0, 0], "opaque red is untouched");
        assert_eq!(
            &flat[9..12],
            &[0xff, 0xff, 0xff],
            "the transparent pixel becomes the background"
        );
    }

    #[test]
    fn half_transparent_blends_to_the_midpoint() {
        let image = Rgba::from_raw(1, 1, vec![0xff, 0xff, 0xff, 128]).unwrap();
        let flat = image.flatten(Colour::BLACK);
        assert_eq!(flat, vec![128, 128, 128], "rounded, not truncated");
    }

    #[test]
    fn downscaling_averages_the_area_it_covers() {
        // Four differently coloured quadrants collapsing to one pixel each.
        let image = Rgba::generate(4, 4, |x, y| {
            let value = if x < 2 { 0 } else { 200 };
            let other = if y < 2 { 0 } else { 100 };
            [value, other, 0, 0xff]
        })
        .unwrap();
        let small = image.resize(2, 2).unwrap();
        assert_eq!((small.width(), small.height()), (2, 2));
        assert_eq!(small.get(0, 0), [0, 0, 0, 0xff]);
        assert_eq!(small.get(1, 0), [200, 0, 0, 0xff]);
        assert_eq!(small.get(0, 1), [0, 100, 0, 0xff]);
        assert_eq!(small.get(1, 1), [200, 100, 0, 0xff]);
    }

    #[test]
    fn downscaling_a_gradient_keeps_the_ends_and_averages_the_middle() {
        #[allow(clippy::cast_possible_truncation)]
        let image = Rgba::generate(256, 1, |x, _| [x as u8, 0, 0, 0xff]).unwrap();
        let small = image.resize(2, 1).unwrap();
        // The left half averages 0..=127 (mean 63.5) and the right half
        // 128..=255 (mean 191.5), both rounded to nearest.
        assert_eq!(small.get(0, 0)[0], 64);
        assert_eq!(small.get(1, 0)[0], 192);
    }

    #[test]
    fn averaging_ignores_the_colour_of_transparent_pixels() {
        // Half opaque white, half transparent *green*: the result must be white
        // at half alpha, not a greenish tint.
        let image = Rgba::from_raw(
            2,
            1,
            vec![0xff, 0xff, 0xff, 0xff, 0x00, 0xff, 0x00, 0x00],
        )
        .unwrap();
        let small = image.resize(1, 1).unwrap();
        assert_eq!(small.get(0, 0), [0xff, 0xff, 0xff, 128]);
    }

    #[test]
    fn resizing_to_the_same_size_is_a_copy() {
        let image = quad();
        assert_eq!(image.resize(2, 2).unwrap(), image);
        assert!(image.resize(0, 2).is_err());
    }

    #[test]
    fn resizing_never_loses_or_invents_pixels() {
        let image = Rgba::generate(37, 11, |x, y| {
            #[allow(clippy::cast_possible_truncation)]
            [(x * 7) as u8, (y * 23) as u8, 0, 0xff]
        })
        .unwrap();
        for (width, height) in [(1, 1), (5, 3), (37, 11), (12, 12)] {
            let resized = image.resize(width, height).unwrap();
            assert_eq!(resized.width(), width);
            assert_eq!(resized.height(), height);
            assert_eq!(
                resized.as_bytes().len(),
                width as usize * height as usize * CHANNELS
            );
        }
    }

    #[test]
    fn generate_visits_every_pixel() {
        let image = Rgba::generate(3, 2, |x, y| {
            #[allow(clippy::cast_possible_truncation)]
            [x as u8, y as u8, 0, 0xff]
        })
        .unwrap();
        for y in 0..2 {
            for x in 0..3 {
                #[allow(clippy::cast_possible_truncation)]
                let expected = [x as u8, y as u8, 0, 0xff];
                assert_eq!(image.get(x, y), expected);
            }
        }
    }

    #[test]
    fn png_encoding_round_trips_through_the_dimensions() {
        let image = Rgba::generate(8, 5, |x, y| {
            #[allow(clippy::cast_possible_truncation)]
            [(x * 30) as u8, (y * 50) as u8, 0x80, 0xff]
        })
        .unwrap();
        let png = image.encode_png().unwrap();
        assert_eq!(
            super::super::png::dimensions(&png).unwrap(),
            (8, 5),
            "the encoder must produce a PNG our own IHDR reader understands"
        );
        assert!(png.len() < image.as_bytes().len(), "and it should compress");
    }

    #[test]
    fn png_encoding_preserves_transparency() {
        let mut image = Rgba::new(2, 1).unwrap();
        image.set(0, 0, [0xff, 0x00, 0x00, 0x80]);
        let png = image.encode_png().unwrap();
        assert!(super::super::png::is_png(&png));
    }

    #[test]
    fn background_colours_parse() {
        assert_eq!(
            parse_background("#ff8000").unwrap(),
            Background::Colour(Colour {
                red: 0xff,
                green: 0x80,
                blue: 0x00
            })
        );
        assert_eq!(
            parse_background("#abc").unwrap(),
            Background::Colour(Colour {
                red: 0xaa,
                green: 0xbb,
                blue: 0xcc
            })
        );
        assert_eq!(
            parse_background("white").unwrap(),
            Background::Colour(Colour::WHITE)
        );
        assert_eq!(parse_background("BLACK").unwrap(), Background::Colour(Colour::BLACK));
        assert_eq!(parse_background("none").unwrap(), Background::Keep);
        assert_eq!(parse_background("  none  ").unwrap(), Background::Keep);
        assert_eq!(Background::default(), Background::Keep);
    }

    #[test]
    fn bad_background_values_say_what_is_accepted() {
        for bad in ["chartreuse", "#12", "#gggggg", "#1234567"] {
            let error = parse_background(bad).unwrap_err().to_string();
            assert!(
                error.contains("hex") || error.contains("unrecognised"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn colour_conversions() {
        let colour = Colour {
            red: 0x12,
            green: 0x34,
            blue: 0x56,
        };
        assert_eq!(colour.to_hex(), "#123456");
        assert_eq!(colour.to_rgba_u32(), 0x1234_56ff);
        // The spec's example: 0xff0000ff is opaque red for the `Y` key.
        assert_eq!(
            Colour {
                red: 0xff,
                green: 0,
                blue: 0
            }
            .to_rgba_u32(),
            4_278_190_335
        );
        assert_eq!(colour.channel(0), 0x12);
        assert_eq!(colour.channel(2), 0x56);
    }
}
