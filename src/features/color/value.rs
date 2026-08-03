//! Colour values: parsing what the terminal said, and deciding what it means.
//!
//! Everything here is pure, so the whole of the reply format and the light/dark
//! decision is unit-testable without a terminal in sight. The bytes go out in
//! [`super::query`].
//!
//! # The trap
//!
//! A terminal answers a colour query in **XParseColor** syntax, which `ctlseqs`
//! defers to rather than defining. The form that matters is
//!
//! ```text
//! rgb:<red>/<green>/<blue>
//! ```
//!
//! where each channel is **one to four hex digits**, and the digits given are the
//! *most significant* ones — the value is scaled across the full range rather
//! than padded on the left. So `rgb:f/0/0` is **full red**, and `rgb:ffff/0/0` is
//! the same colour written at four digits. A naive `u16::from_str_radix` per field
//! reads `f` as 15 out of 65535 and turns full red into near-black: bright
//! terminals get reported as dark ones, `--is-dark` answers backwards, and
//! nothing about the bug is visible until someone with a light theme complains.
//!
//! The older `#` form is the opposite, and both are in circulation, so both are
//! implemented here:
//!
//! | reply | red channel | normalised |
//! |-------|-------------|------------|
//! | `rgb:f/0/0` | scaled: `0xffff` | `#ff0000` |
//! | `rgb:ffff/0/0` | `0xffff` | `#ff0000` |
//! | `#f00` | left-justified, zero-filled: `0xf000` | `#ef0000` |
//!
//! XParseColor(3) states the difference outright: in the `#` form, when fewer
//! than 16 bits are given they "represent the most significant bits of the value
//! (unlike the `rgb:` syntax, in which values are scaled)".

use anyhow::{Result, bail};

/// Luminance of CIE L\* = 50 — perceptual middle grey — which is where the
/// light/dark boundary is drawn. See [`Rgb::is_dark`].
pub const DARK_THRESHOLD: f64 = 0.184_186_8;

/// The most hex digits one channel may have.
const MAX_DIGITS: u32 = 4;

/// A colour at the precision the terminal reported it: 16 bits per channel.
///
/// X11 works in 16 bits per channel and so does every terminal reply, so that is
/// what is kept. The 8-bit form everyone actually uses is derived on demand by
/// [`Rgb::rgb8`], never stored, so a `1a1a/2b2b/3c3c` reply can be shown at full
/// precision alongside `#1a2b3c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    /// Red, 0 to 65535.
    pub r: u16,
    /// Green, 0 to 65535.
    pub g: u16,
    /// Blue, 0 to 65535.
    pub b: u16,
}

impl Rgb {
    /// Black.
    ///
    /// `#[cfg(test)]` along with [`Rgb::from_rgb8`] below: nothing in the running
    /// program constructs a colour, it only ever parses one the terminal sent.
    /// Both are here because they make the tests read in the 8-bit hex everyone
    /// actually thinks in, and a colour written `Rgb::new(0x1e1e, 0x1e1e, 0x2e2e)`
    /// is one nobody can check against `#1e1e2e` at a glance.
    #[cfg(test)]
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0 };

    /// A colour from three 16-bit channels.
    #[must_use]
    pub const fn new(r: u16, g: u16, b: u16) -> Self {
        Self { r, g, b }
    }

    /// A colour from three 8-bit channels, widened the way X does it.
    ///
    /// `0xab` becomes `0xabab`, not `0x00ab`: an 8-bit channel is two hex digits
    /// of a four-digit value, so it scales rather than being padded.
    #[cfg(test)]
    #[must_use]
    pub const fn from_rgb8(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: widen8(r),
            g: widen8(g),
            b: widen8(b),
        }
    }

    /// The colour as three 8-bit channels, rounded rather than truncated.
    ///
    /// Truncation would report `0xffff` as 254 and, worse, make the round trip
    /// through 8 bits asymmetric.
    #[must_use]
    pub fn rgb8(self) -> (u8, u8, u8) {
        (narrow16(self.r), narrow16(self.g), narrow16(self.b))
    }

    /// `#rrggbb`, the form a person recognises and a config file accepts.
    #[must_use]
    pub fn hex(self) -> String {
        let (r, g, b) = self.rgb8();
        format!("#{r:02x}{g:02x}{b:02x}")
    }

    /// The colour written back out as a four-digit XParseColor specification.
    ///
    /// Useful for showing what a one-digit reply actually meant: `rgb:f/0/0`
    /// canonicalises to `rgb:ffff/0000/0000`.
    #[must_use]
    pub fn to_xparsecolor(self) -> String {
        format!("rgb:{:04x}/{:04x}/{:04x}", self.r, self.g, self.b)
    }

    /// Relative luminance, per the sRGB definition WCAG 2 uses.
    ///
    /// Each channel is taken to 0.0–1.0, un-gamma'd with the sRGB transfer
    /// function, and mixed as
    ///
    /// ```text
    /// Y = 0.2126 R + 0.7152 G + 0.0722 B
    /// ```
    ///
    /// The linearisation is the part that cannot be skipped. Averaging the raw
    /// channels — or using the gamma-encoded values in the same weighted sum —
    /// makes mid-tones read far brighter than they look, and the weights are why
    /// pure blue (`#0000ff`, luminance 0.07) is dark while pure green
    /// (`#00ff00`, luminance 0.72) is not.
    #[must_use]
    pub fn luminance(self) -> f64 {
        let (r, g, b) = self.rgb8();
        0.2126 * linearise(r) + 0.7152 * linearise(g) + 0.0722 * linearise(b)
    }

    /// Whether this is a dark colour, by luminance rather than by guesswork.
    ///
    /// The cut is at [`DARK_THRESHOLD`], the relative luminance of **CIE
    /// L\* = 50** — the midpoint of perceptual lightness, which lands at about
    /// `#777777`. Cutting at luminance 0.5 instead would be a much brighter
    /// boundary, calling every grey up to `#b4b4b4` dark; cutting at 8-bit
    /// midpoint `#808080` ignores gamma. L\* = 50 is the one boundary of the
    /// three that matches what a person would say if shown the colour.
    ///
    /// A background exactly on the boundary is called light: the ties have to go
    /// somewhere, and light is the safer default for a foreground chosen from it.
    #[must_use]
    pub fn is_dark(self) -> bool {
        self.luminance() < DARK_THRESHOLD
    }

    /// Contrast ratio against another colour, 1.0 to 21.0, as WCAG defines it.
    ///
    /// Reported next to the foreground and background so a reader can see at a
    /// glance whether their own theme clears the 4.5 : 1 that body text wants.
    #[must_use]
    pub fn contrast_ratio(self, other: Self) -> f64 {
        let (lighter, darker) = {
            let (a, b) = (self.luminance(), other.luminance());
            if a >= b { (a, b) } else { (b, a) }
        };
        (lighter + 0.05) / (darker + 0.05)
    }
}

/// Widens an 8-bit channel to 16 bits by repeating it: `0xab` to `0xabab`.
#[cfg(test)]
const fn widen8(value: u8) -> u16 {
    (value as u16) << 8 | (value as u16)
}

/// Narrows a 16-bit channel to 8 bits with rounding.
///
/// `(v * 255 + 32767) / 65535` in integer arithmetic, which is why the widths
/// are u32 in the middle. The result cannot exceed 255, but the conversion
/// saturates rather than relying on the next reader re-proving that.
fn narrow16(value: u16) -> u8 {
    let scaled = (u32::from(value) * 255 + 32767) / 65535;
    u8::try_from(scaled).unwrap_or(u8::MAX)
}

/// The sRGB electro-optical transfer function: gamma-encoded 0–255 to linear
/// 0.0–1.0.
fn linearise(channel: u8) -> f64 {
    let c = f64::from(channel) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Which channel a malformed reply went wrong in, for the error message.
const CHANNEL_NAMES: [&str; 3] = ["red", "green", "blue"];

/// Parses an XParseColor specification — the value half of a colour reply.
///
/// Accepts the two forms terminals actually send:
///
/// * `rgb:<r>/<g>/<b>`, each channel 1 to 4 hex digits, **scaled** so that the
///   digits given are the most significant ones.
/// * `#RGB`, `#RRGGBB`, `#RRRGGGBBB` or `#RRRRGGGGBBBB`, where the digits given
///   are the most significant ones and the rest are **zero**.
///
/// Case is insignificant, as X specifies.
///
/// # Errors
///
/// Fails on anything else, naming what was wrong. `rgbi:` (floating-point
/// intensities) and colour names are deliberately refused rather than guessed at:
/// no terminal replies with either, so accepting them would only hide a reply
/// that had actually been misread.
pub fn parse_spec(spec: &str) -> Result<Rgb> {
    let spec = spec.trim();
    if let Some(digits) = spec.strip_prefix('#') {
        return parse_sharp(digits);
    }
    // `rgb:` is matched case-insensitively but `rgbi:` must not slip through it,
    // so the longer prefix is checked first.
    let lower = spec.to_ascii_lowercase();
    if lower.starts_with("rgbi:") {
        bail!(
            "reply uses the `rgbi:` floating-point form, which escapade does not parse: {spec:?}"
        );
    }
    let Some(rest) = lower.strip_prefix("rgb:") else {
        bail!("not an XParseColor specification: {spec:?}");
    };

    let mut fields = rest.split('/');
    let mut channels = [0u16; 3];
    for (index, channel) in channels.iter_mut().enumerate() {
        let Some(field) = fields.next() else {
            bail!("reply has {index} channels, expected 3 separated by `/`: {spec:?}");
        };
        *channel = parse_channel(field, CHANNEL_NAMES[index], spec)?;
    }
    if fields.next().is_some() {
        bail!("reply has more than 3 channels: {spec:?}");
    }
    Ok(Rgb::new(channels[0], channels[1], channels[2]))
}

/// Parses one `rgb:` channel and scales it to 16 bits.
///
/// This is the whole trap in four lines. `n` digits give a value in
/// `0..=16^n - 1`, and the 16-bit result is that value **scaled across the full
/// range**, not left-padded:
///
/// ```text
/// value_16 = round(value * 65535 / (16^n - 1))
/// ```
///
/// For one digit that makes `f` into `0xffff`, for two it makes `ab` into
/// `0xabab`, and for three it makes `abc` into `0xabca` — the digit pattern
/// repeated and cut at 16 bits, which is exactly what "the digits are the most
/// significant ones" means.
fn parse_channel(field: &str, name: &str, spec: &str) -> Result<u16> {
    let digits = u32::try_from(field.len()).unwrap_or(u32::MAX);
    if field.is_empty() {
        bail!("the {name} channel is empty: {spec:?}");
    }
    if digits > MAX_DIGITS {
        bail!(
            "the {name} channel has {digits} hex digits; XParseColor allows 1 to {MAX_DIGITS}: \
             {spec:?}"
        );
    }
    if !field.chars().all(|ch| ch.is_ascii_hexdigit()) {
        bail!("the {name} channel {field:?} is not hexadecimal: {spec:?}");
    }
    // Cannot fail: at most four hex digits fit in a u32, and every character was
    // just checked to be one.
    let value = u32::from_str_radix(field, 16).unwrap_or(0);
    let max = (1u32 << (4 * digits)) - 1;
    let scaled = (value * u32::from(u16::MAX) + max / 2) / max;
    Ok(u16::try_from(scaled).unwrap_or(u16::MAX))
}

/// Parses the older `#…` form, where missing bits are zero rather than scaled.
fn parse_sharp(digits: &str) -> Result<Rgb> {
    if !digits.chars().all(|ch| ch.is_ascii_hexdigit()) {
        bail!("`#{digits}` is not hexadecimal");
    }
    let per_channel = match digits.len() {
        3 => 1,
        6 => 2,
        9 => 3,
        12 => 4,
        other => bail!(
            "`#{digits}` has {other} hex digits; the `#` form takes 3, 6, 9 or 12 \
             (`#RGB`, `#RRGGBB`, `#RRRGGGBBB`, `#RRRRGGGGBBBB`)"
        ),
    };
    let mut channels = [0u16; 3];
    for (index, channel) in channels.iter_mut().enumerate() {
        let field = &digits[index * per_channel..(index + 1) * per_channel];
        let value = u32::from_str_radix(field, 16).unwrap_or(0);
        // Left-justify: the digits given are the most significant, the rest zero.
        let shift = 16 - 4 * u32::try_from(per_channel).unwrap_or(4);
        *channel = u16::try_from(value << shift).unwrap_or(u16::MAX);
    }
    Ok(Rgb::new(channels[0], channels[1], channels[2]))
}

/// A colour the terminal reported, kept with the text it arrived as.
///
/// The raw specification is retained on purpose. It is the evidence: a reader who
/// has just been told that `rgb:f/0/0` is full red should be able to see the
/// terminal say `rgb:f/0/0` and the normalised `#ff0000` on the same line, and a
/// bug report about a colour being read wrongly is only actionable with the
/// original text in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    /// The OSC number that answered — 4, 10, 11, 12, 17 or 19.
    pub ps: u16,
    /// Palette index, for an `OSC 4` reply. `None` for the dynamic colours.
    pub index: Option<u8>,
    /// The specification exactly as it arrived, e.g. `rgb:1a1a/2b2b/3c3c`.
    pub raw: String,
    /// The parsed value.
    pub rgb: Rgb,
}

/// Parses a colour reply — the payload between `OSC` and `ST`.
///
/// The two shapes, as `ctlseqs` documents them:
///
/// ```text
/// 11 ; rgb:1e1e/1e1e/2e2e          a dynamic colour (OSC 10, 11, 12, 17, 19)
/// 4 ; 4 ; rgb:0000/0000/ffff       a palette entry (OSC 4)
/// ```
///
/// # Errors
///
/// Fails if the payload is not a colour reply at all, or if its specification is
/// malformed. A reply for a *different* OSC number is not an error here — the
/// caller matches on [`Reported::ps`] — because a terminal may answer several
/// queries and the answers can arrive interleaved.
pub fn parse_reply(payload: &str) -> Result<Reported> {
    let mut fields = payload.splitn(3, ';');
    let Some(number) = fields.next() else {
        bail!("empty OSC payload");
    };
    let ps: u16 = number
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("OSC payload does not start with a number: {payload:?}"))?;

    // `OSC 4` and `OSC 5` carry an index before the specification; the dynamic
    // colours do not. Anything else is not a colour reply.
    let (index, spec) = if matches!(ps, 4 | 5) {
        let Some(index_text) = fields.next() else {
            bail!("OSC {ps} reply has no palette index: {payload:?}");
        };
        let index: u8 = index_text.trim().parse().map_err(|_| {
            anyhow::anyhow!("OSC {ps} reply has an unusable palette index {index_text:?}")
        })?;
        let Some(spec) = fields.next() else {
            bail!("OSC {ps} reply has no colour after its index: {payload:?}");
        };
        (Some(index), spec)
    } else {
        let Some(spec) = fields.next() else {
            bail!("OSC {ps} reply has no colour in it: {payload:?}");
        };
        (None, spec)
    };

    // A terminal answering several palette entries at once packs them as
    // `4;1;spec;2;spec`. Only the first pair is taken; the caller asks for one
    // entry at a time, so the rest would be an answer to a question nobody asked.
    let spec = spec.split(';').next().unwrap_or(spec).trim();
    let rgb = parse_spec(spec)?;
    Ok(Reported {
        ps,
        index,
        raw: spec.to_owned(),
        rgb,
    })
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The normalised 8-bit hex for a specification, for terse assertions.
    fn hex_of(spec: &str) -> String {
        parse_spec(spec).unwrap().hex()
    }

    // -- the digit-width trap ------------------------------------------------
    //
    // These are the most important tests in the feature. Every one of them fails
    // if the channels are read with a plain `from_str_radix` and not scaled.

    #[test]
    fn one_digit_per_channel_is_scaled_not_padded() {
        // The headline case: `rgb:f/0/0` is FULL RED. Reading `f` as 15/65535
        // would make this #000000 and report a red terminal as a black one.
        assert_eq!(hex_of("rgb:f/0/0"), "#ff0000");
        assert_eq!(hex_of("rgb:0/f/0"), "#00ff00");
        assert_eq!(hex_of("rgb:0/0/f"), "#0000ff");
        assert_eq!(hex_of("rgb:f/f/f"), "#ffffff");
        assert_eq!(hex_of("rgb:0/0/0"), "#000000");
        // And a mid value: 8 of 15 is 53%, which is 0x88 not 0x08.
        assert_eq!(hex_of("rgb:8/8/8"), "#888888");
    }

    #[test]
    fn two_digits_per_channel_repeat_into_16_bits() {
        assert_eq!(hex_of("rgb:ff/00/00"), "#ff0000");
        assert_eq!(hex_of("rgb:1a/2b/3c"), "#1a2b3c");
        let parsed = parse_spec("rgb:ab/ab/ab").unwrap();
        assert_eq!(parsed.r, 0xabab, "0xab widens to 0xabab, not 0x00ab");
    }

    #[test]
    fn three_digits_per_channel_repeat_and_cut_at_16_bits() {
        // 0xabc of 0xfff, scaled: the pattern abcabc… cut to 16 bits is 0xabca.
        let parsed = parse_spec("rgb:abc/000/fff").unwrap();
        assert_eq!(parsed.r, 0xabca);
        assert_eq!(parsed.g, 0x0000);
        assert_eq!(parsed.b, 0xffff, "the maximum at any width is the maximum");
        assert_eq!(parsed.hex(), "#ab00ff");
    }

    #[test]
    fn four_digits_per_channel_are_taken_as_they_are() {
        let parsed = parse_spec("rgb:1e1e/1e1e/2e2e").unwrap();
        assert_eq!((parsed.r, parsed.g, parsed.b), (0x1e1e, 0x1e1e, 0x2e2e));
        assert_eq!(parsed.hex(), "#1e1e2e");
        assert_eq!(hex_of("rgb:ffff/0000/0000"), "#ff0000");
        assert_eq!(hex_of("rgb:0000/0000/0000"), "#000000");
    }

    #[test]
    fn every_width_spells_the_same_colour() {
        // The property that makes the scaling right rather than merely plausible:
        // full red is full red however many digits it is written with.
        for spec in [
            "rgb:f/0/0",
            "rgb:ff/00/00",
            "rgb:fff/000/000",
            "rgb:ffff/0000/0000",
        ] {
            assert_eq!(hex_of(spec), "#ff0000", "{spec}");
            assert_eq!(parse_spec(spec).unwrap().r, u16::MAX, "{spec}");
        }
        for spec in [
            "rgb:f/f/f",
            "rgb:ff/ff/ff",
            "rgb:fff/fff/fff",
            "rgb:ffff/ffff/ffff",
        ] {
            assert_eq!(hex_of(spec), "#ffffff", "{spec}");
        }
    }

    #[test]
    fn channels_may_have_different_widths_in_one_reply() {
        // Nothing requires the three channels to agree, and terminals do mix
        // them: xterm strips leading zeroes, so a dark blue can come back as
        // `rgb:0/0/8000`.
        assert_eq!(hex_of("rgb:f/ff/ffff"), "#ffffff");
        assert_eq!(hex_of("rgb:0/00/0000"), "#000000");
        let mixed = parse_spec("rgb:8/80/8000").unwrap();
        assert_eq!(
            (mixed.r, mixed.g, mixed.b),
            (0x8888, 0x8080, 0x8000),
            "each channel scales by its own width"
        );
    }

    #[test]
    fn case_is_insignificant() {
        assert_eq!(hex_of("RGB:FF/00/AB"), "#ff00ab");
        assert_eq!(hex_of("rgb:Ff/0a/Bc"), "#ff0abc");
        assert_eq!(
            parse_spec("#AABBCC").unwrap(),
            parse_spec("#aabbcc").unwrap()
        );
    }

    #[test]
    fn the_two_forms_of_the_same_digits_are_not_the_same_colour() {
        // A consequence of zero-filling rather than scaling that is easy to miss:
        // `#aabbcc` is 0xaa00/0xbb00/0xcc00, which normalises to #a9bacb — one
        // short on every channel — while `rgb:aa/bb/cc` is 0xaaaa/0xbbbb/0xcccc
        // and normalises back to #aabbcc exactly. The `#` form cannot represent
        // an 8-bit colour losslessly, which is why X discourages it.
        assert_eq!(hex_of("rgb:aa/bb/cc"), "#aabbcc");
        assert_eq!(hex_of("#aabbcc"), "#a9bacb");
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        assert_eq!(hex_of("  rgb:ff/00/00  "), "#ff0000");
    }

    // -- the `#` form, which is the other way round -------------------------

    #[test]
    fn the_sharp_form_zero_fills_instead_of_scaling() {
        // XParseColor(3): in the `#` form the digits given are the most
        // significant bits and the rest are zero — "unlike the rgb: syntax, in
        // which values are scaled". So `#f00` is NOT full red.
        let sharp = parse_spec("#f00").unwrap();
        assert_eq!(sharp.r, 0xf000);
        assert_eq!(sharp.hex(), "#ef0000");
        let scaled = parse_spec("rgb:f/0/0").unwrap();
        assert_eq!(scaled.r, 0xffff);
        assert_ne!(sharp, scaled, "the two forms genuinely differ");
    }

    #[test]
    fn the_sharp_form_accepts_all_four_widths() {
        assert_eq!(
            parse_spec("#3a7").unwrap(),
            Rgb::new(0x3000, 0xa000, 0x7000)
        );
        assert_eq!(
            parse_spec("#1a2b3c").unwrap(),
            Rgb::new(0x1a00, 0x2b00, 0x3c00)
        );
        assert_eq!(
            parse_spec("#123456789").unwrap(),
            Rgb::new(0x1230, 0x4560, 0x7890)
        );
        assert_eq!(
            parse_spec("#123456789abc").unwrap(),
            Rgb::new(0x1234, 0x5678, 0x9abc)
        );
        // The example from XParseColor(3) itself.
        assert_eq!(
            parse_spec("#3a7").unwrap(),
            parse_spec("#3000a0007000").unwrap()
        );
    }

    #[test]
    fn eight_bit_values_widen_by_repeating() {
        assert_eq!(
            Rgb::from_rgb8(0xab, 0x00, 0xff),
            Rgb::new(0xabab, 0, 0xffff)
        );
        assert_eq!(Rgb::from_rgb8(0x1a, 0x2b, 0x3c).hex(), "#1a2b3c");
    }

    #[test]
    fn narrowing_rounds_rather_than_truncates() {
        assert_eq!(Rgb::new(0xffff, 0xffff, 0xffff).rgb8(), (255, 255, 255));
        assert_eq!(Rgb::new(0, 0, 0).rgb8(), (0, 0, 0));
        // 0x8080 is exactly 128/255; truncating the division would say 127.
        assert_eq!(Rgb::new(0x8080, 0x8080, 0x8080).rgb8(), (128, 128, 128));
        // Every 8-bit value must survive the round trip through 16 bits.
        for value in 0..=255u8 {
            let (r, _, _) = Rgb::from_rgb8(value, value, value).rgb8();
            assert_eq!(r, value, "round trip broke at {value}");
        }
    }

    #[test]
    fn canonical_form_spells_out_all_four_digits() {
        assert_eq!(
            parse_spec("rgb:f/0/0").unwrap().to_xparsecolor(),
            "rgb:ffff/0000/0000"
        );
    }

    // -- malformed replies ---------------------------------------------------

    #[test]
    fn a_channel_with_five_digits_is_refused() {
        let error = parse_spec("rgb:fffff/0/0").unwrap_err().to_string();
        assert!(error.contains("red"), "{error}");
        assert!(error.contains("1 to 4"), "{error}");
    }

    #[test]
    fn a_missing_or_extra_channel_is_refused() {
        assert!(parse_spec("rgb:ff/00").is_err(), "two channels");
        assert!(parse_spec("rgb:ff").is_err(), "one channel");
        assert!(parse_spec("rgb:ff/00/00/00").is_err(), "four channels");
        let error = parse_spec("rgb:ff//00").unwrap_err().to_string();
        assert!(
            error.contains("green") && error.contains("empty"),
            "{error}"
        );
    }

    #[test]
    fn non_hexadecimal_digits_are_refused() {
        assert!(parse_spec("rgb:gg/00/00").is_err());
        assert!(parse_spec("rgb:-1/00/00").is_err());
        assert!(parse_spec("rgb:0x1/00/00").is_err());
        assert!(parse_spec("#nothex").is_err());
    }

    #[test]
    fn a_sharp_form_of_the_wrong_length_is_refused() {
        for spec in ["#f", "#ff", "#ffff", "#fffff", "#ffffffff", "#"] {
            assert!(parse_spec(spec).is_err(), "{spec} should not parse");
        }
    }

    #[test]
    fn forms_escapade_does_not_parse_say_so_rather_than_guessing() {
        // Silently reading `rgbi:1.0/0/0` as black would be the same class of bug
        // as the digit-width trap, so it is refused by name.
        let error = parse_spec("rgbi:1.0/0.0/0.0").unwrap_err().to_string();
        assert!(error.contains("rgbi"), "{error}");
        assert!(parse_spec("red").is_err(), "colour names are not parsed");
        assert!(parse_spec("").is_err());
    }

    // -- whole replies -------------------------------------------------------

    #[test]
    fn parses_a_dynamic_colour_reply() {
        let reported = parse_reply("11;rgb:1e1e/1e1e/2e2e").unwrap();
        assert_eq!(reported.ps, 11);
        assert_eq!(reported.index, None);
        assert_eq!(reported.raw, "rgb:1e1e/1e1e/2e2e");
        assert_eq!(reported.rgb.hex(), "#1e1e2e");
    }

    #[test]
    fn the_raw_reply_is_kept_verbatim_however_short() {
        // The evidence for the whole feature: a reader must be able to see that
        // the terminal really did say `f`, next to the #ff0000 it means.
        let reported = parse_reply("11;rgb:f/0/0").unwrap();
        assert_eq!(reported.raw, "rgb:f/0/0");
        assert_eq!(reported.rgb.hex(), "#ff0000");
    }

    #[test]
    fn parses_every_dynamic_colour_number() {
        for ps in [10u16, 11, 12, 17, 19] {
            let reported = parse_reply(&format!("{ps};rgb:0/0/0")).unwrap();
            assert_eq!(reported.ps, ps);
            assert_eq!(reported.index, None);
        }
    }

    #[test]
    fn parses_a_palette_reply_with_its_index() {
        let reported = parse_reply("4;4;rgb:0000/0000/ffff").unwrap();
        assert_eq!(reported.ps, 4);
        assert_eq!(reported.index, Some(4));
        assert_eq!(reported.rgb.hex(), "#0000ff");

        // Index 0 and index 255 are both legal and must not be confused with
        // "no index".
        assert_eq!(parse_reply("4;0;rgb:0/0/0").unwrap().index, Some(0));
        assert_eq!(parse_reply("4;255;rgb:f/f/f").unwrap().index, Some(255));
    }

    #[test]
    fn a_palette_index_outside_a_byte_is_refused() {
        // 256 is not a palette entry, and reading it modulo 256 as entry 0 would
        // report a colour for the wrong cell.
        let error = parse_reply("4;256;rgb:0/0/0").unwrap_err().to_string();
        assert!(error.contains("palette index"), "{error}");
        assert!(parse_reply("4;-1;rgb:0/0/0").is_err());
    }

    #[test]
    fn several_palette_entries_in_one_reply_yield_the_first() {
        let reported = parse_reply("4;1;rgb:f/0/0;2;rgb:0/f/0").unwrap();
        assert_eq!(reported.index, Some(1));
        assert_eq!(reported.rgb.hex(), "#ff0000");
        assert_eq!(reported.raw, "rgb:f/0/0", "the raw text is just that entry");
    }

    #[test]
    fn malformed_replies_are_errors_rather_than_colours() {
        for payload in [
            "",
            "11",
            "11;",
            "notanumber;rgb:0/0/0",
            "4;rgb:0/0/0",
            "4;1",
            "11;rgb:0/0",
            "11;nonsense",
        ] {
            assert!(
                parse_reply(payload).is_err(),
                "{payload:?} should not parse as a colour"
            );
        }
    }

    #[test]
    fn an_unrelated_osc_reply_is_not_mistaken_for_a_colour() {
        // A terminal may answer several queries; the caller filters on `ps`, but
        // a payload that is not a colour at all must not parse as one.
        assert!(parse_reply("52;c;aGVsbG8=").is_err());
        assert!(parse_reply("L;window title").is_err());
    }

    // -- luminance and the light/dark boundary -------------------------------

    #[test]
    fn luminance_matches_the_srgb_definition_at_the_extremes() {
        let white = Rgb::from_rgb8(255, 255, 255).luminance();
        let black = Rgb::from_rgb8(0, 0, 0).luminance();
        assert!((white - 1.0).abs() < 1e-9, "white is 1.0, got {white}");
        assert!(black.abs() < 1e-12, "black is 0.0, got {black}");
    }

    #[test]
    fn the_channel_weights_are_not_an_average() {
        // Green carries 0.7152 of the luminance and blue 0.0722. An
        // implementation that averaged the channels would call these equal, and
        // would then call a pure blue background light.
        let red = Rgb::from_rgb8(255, 0, 0).luminance();
        let green = Rgb::from_rgb8(0, 255, 0).luminance();
        let blue = Rgb::from_rgb8(0, 0, 255).luminance();
        assert!((red - 0.2126).abs() < 1e-4, "{red}");
        assert!((green - 0.7152).abs() < 1e-4, "{green}");
        assert!((blue - 0.0722).abs() < 1e-4, "{blue}");
        assert!(blue < green, "blue is much darker than green");
    }

    #[test]
    fn the_gamma_curve_is_applied() {
        // Mid-grey #808080 linearises to about 0.216, not to 0.5. Skipping the
        // transfer function is the other classic way to get this wrong.
        let mid = Rgb::from_rgb8(128, 128, 128).luminance();
        assert!((mid - 0.2159).abs() < 1e-3, "{mid}");
    }

    #[test]
    fn real_terminal_backgrounds_land_on_the_expected_side() {
        let dark = [
            ("#000000", "black"),
            ("#1e1e2e", "catppuccin mocha"),
            ("#282c34", "one dark"),
            ("#2e3440", "nord"),
            ("#002b36", "solarized dark"),
            ("#1d2021", "gruvbox dark"),
            (
                "#0000ff",
                "pure blue, which is dark despite being saturated",
            ),
        ];
        for (hex, name) in dark {
            let rgb = parse_spec(hex).unwrap();
            assert!(rgb.is_dark(), "{name} ({hex}) should be dark");
        }

        let light = [
            ("#ffffff", "white"),
            ("#fdf6e3", "solarized light"),
            ("#eff1f5", "catppuccin latte"),
            ("#fbf1c7", "gruvbox light"),
            ("#f8f8f2", "a light editor theme"),
            ("#00ff00", "pure green, which is light"),
        ];
        for (hex, name) in light {
            let rgb = parse_spec(hex).unwrap();
            assert!(!rgb.is_dark(), "{name} ({hex}) should be light");
        }
    }

    #[test]
    fn the_boundary_is_perceptual_middle_grey() {
        // CIE L* = 50 is around #777777. Either side of it must decide the other
        // way, and a colour exactly on it counts as light.
        assert!(parse_spec("#6f6f6f").unwrap().is_dark(), "just below");
        assert!(!parse_spec("#808080").unwrap().is_dark(), "just above");

        let boundary = Rgb::from_rgb8(119, 119, 119);
        let below = Rgb::from_rgb8(118, 118, 118);
        assert!(
            below.luminance() < DARK_THRESHOLD,
            "#767676 must fall below the threshold, got {}",
            below.luminance()
        );
        assert!(
            boundary.luminance() >= DARK_THRESHOLD,
            "#777777 must fall on or above it, got {}",
            boundary.luminance()
        );

        // Ties go to light, because `is_dark` compares strictly less-than. The
        // threshold is between the two greys above, so the pair of assertions
        // straddling it is the observable form of that claim.
    }

    #[test]
    fn is_dark_is_monotonic_in_grey() {
        // Once dark, staying dark as it gets darker. A threshold applied to the
        // wrong quantity would not necessarily do this.
        let mut seen_light = false;
        for value in 0..=255u8 {
            let dark = Rgb::from_rgb8(value, value, value).is_dark();
            if !dark {
                seen_light = true;
            }
            assert!(!(seen_light && dark), "grey {value} went back to dark");
        }
        assert!(seen_light, "white must be light");
    }

    #[test]
    fn contrast_ratio_spans_one_to_twenty_one() {
        let white = Rgb::from_rgb8(255, 255, 255);
        let black = Rgb::from_rgb8(0, 0, 0);
        let ratio = white.contrast_ratio(black);
        assert!((ratio - 21.0).abs() < 0.01, "{ratio}");
        // Order must not matter.
        assert!((black.contrast_ratio(white) - ratio).abs() < 1e-9);
        // A colour against itself is 1 : 1.
        assert!((white.contrast_ratio(white) - 1.0).abs() < 1e-9);
        // A real theme: catppuccin mocha text on its background clears 4.5 : 1.
        let bg = parse_spec("#1e1e2e").unwrap();
        let fg = parse_spec("#cdd6f4").unwrap();
        assert!(fg.contrast_ratio(bg) > 4.5, "{}", fg.contrast_ratio(bg));
    }
}
