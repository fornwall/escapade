//! The kitty text sizing protocol: `OSC 66 ; <metadata> ; <text> ST`.
//!
//! A terminal is a grid of identically sized cells, and for forty years that
//! meant one text size per screen. This protocol lifts that restriction without
//! leaving the grid: the application says how many cells a piece of text should
//! occupy, and the terminal picks a font size to fill them.
//!
//! ```text
//! OSC 66 ; s=4 ; BUILD PASSED ST
//! ```
//!
//! The metadata is a **colon**-separated list of `key=value` pairs — six keys,
//! all integers:
//!
//! ```text
//! s   1..7    overall scale; the block is s*w by s cells
//! w   0..7    width in cells; 0 (the default) means "count it as usual"
//! n   0..15   numerator of a fractional scale, applied on top of s
//! d   0..15   denominator; must be greater than n
//! v   0..2    vertical alignment inside the block: 0 top, 1 bottom, 2 centre
//! h   0..2    horizontal alignment: 0 left, 1 right, 2 centre
//! ```
//!
//! The thing that makes this different from figlet-style ASCII art is that the
//! text stays text. A four-cell-tall `BUILD PASSED` is one run of multicell
//! characters holding the string `BUILD PASSED`, so selecting it with the mouse
//! copies twelve characters. Art drawn out of box-drawing glyphs copies as the
//! picture it is made of.
//!
//! Two things follow from the design and shape all the code here:
//!
//! * **There is no query.** The protocol only ever *tells* the terminal a size;
//!   nothing anywhere in it asks one. Its own documented support detection is a
//!   Cursor Position Report taken around `OSC 66 ; w=2 ; <space> ST` — measure
//!   what happened. See [`probe`].
//! * **Support is per key.** A terminal may implement `w=` (which fixes the
//!   long-standing width-disagreement bug) without implementing `s=` (which
//!   needs a font renderer that can scale). foot does exactly that, so nothing
//!   in this module collapses support into one boolean.
//!
//! Specification: <https://sw.kovidgoyal.net/kitty/text-sizing-protocol/>

pub mod demo;
pub mod probe;
pub mod render;

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::{
    cli::{HorizontalAlign, TextsizeAction, TextsizeCommand, VerticalAlign},
    db::Catalog,
    term::{OSC, ST},
    theme::Theme,
    unicode,
};

/// The feature id in the database. The subcommand is `textsize`.
pub const FEATURE_ID: &str = "text-sizing";

/// The OSC command number text sizing lives under.
pub const COMMAND: &str = "66";

/// Longest text payload the specification allows, in bytes. Longer strings must
/// be split across several escape codes.
pub const MAX_PAYLOAD: usize = 4096;

/// Highest `s` and `w` the protocol can express. Both are three-bit fields in
/// kitty's cell representation, which is where the 7 comes from.
pub const MAX_SCALE: u8 = 7;

/// Highest `n` and `d` the protocol can express — four-bit fields.
pub const MAX_FRACTION: u8 = 15;

// ---------------------------------------------------------------------------
// The keys
// ---------------------------------------------------------------------------

/// One metadata key of `OSC 66`.
///
/// Worth an enum rather than six string literals because [`probe`] reports a
/// verdict *per key* and needs to name them, and because the letters are easy to
/// mix up: `w` is a count of cells, `s` is a multiplier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    /// `s` — overall scale, 1 to 7.
    Scale,
    /// `w` — width in cells, 0 to 7.
    Width,
    /// `n` — numerator of the fractional scale, 0 to 15.
    Numerator,
    /// `d` — denominator of the fractional scale, 0 to 15.
    Denominator,
    /// `v` — vertical alignment, 0 to 2.
    VerticalAlign,
    /// `h` — horizontal alignment, 0 to 2.
    HorizontalAlign,
}

impl Key {
    /// Every key, in the order the specification tabulates them.
    pub const ALL: [Self; 6] = [
        Self::Scale,
        Self::Width,
        Self::Numerator,
        Self::Denominator,
        Self::VerticalAlign,
        Self::HorizontalAlign,
    ];

    /// The single letter used on the wire.
    #[must_use]
    pub fn letter(self) -> char {
        match self {
            Self::Scale => 's',
            Self::Width => 'w',
            Self::Numerator => 'n',
            Self::Denominator => 'd',
            Self::VerticalAlign => 'v',
            Self::HorizontalAlign => 'h',
        }
    }

    /// Human name, for a report column.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Scale => "scale",
            Self::Width => "width",
            Self::Numerator => "numerator",
            Self::Denominator => "denominator",
            Self::VerticalAlign => "vertical alignment",
            Self::HorizontalAlign => "horizontal alignment",
        }
    }

    /// Lowest and highest value the specification allows, inclusive.
    #[must_use]
    pub fn range(self) -> (u8, u8) {
        match self {
            Self::Scale => (1, MAX_SCALE),
            Self::Width => (0, MAX_SCALE),
            Self::Numerator | Self::Denominator => (0, MAX_FRACTION),
            Self::VerticalAlign | Self::HorizontalAlign => (0, 2),
        }
    }

    /// The value the terminal assumes when the key is absent.
    #[must_use]
    pub fn default_value(self) -> u8 {
        match self {
            Self::Scale => 1,
            _ => 0,
        }
    }
}

/// The wire value for a vertical alignment: `0` top, `1` bottom, `2` centred.
///
/// Note that centre is `2` rather than the middle value — reading the enum's
/// declaration order onto the wire would silently swap bottom and centre.
#[must_use]
pub fn valign_value(align: VerticalAlign) -> u8 {
    match align {
        VerticalAlign::Top => 0,
        VerticalAlign::Bottom => 1,
        VerticalAlign::Center => 2,
    }
}

/// The wire value for a horizontal alignment: `0` left, `1` right, `2` centred.
#[must_use]
pub fn halign_value(align: HorizontalAlign) -> u8 {
    match align {
        HorizontalAlign::Left => 0,
        HorizontalAlign::Right => 1,
        HorizontalAlign::Center => 2,
    }
}

// ---------------------------------------------------------------------------
// Fractional scaling
// ---------------------------------------------------------------------------

/// A fractional scale: `n=<numerator>:d=<denominator>`.
///
/// Fractional scaling is applied *on top of* `s`, and — this is the part that
/// surprises people — it does **not** change how many cells the text occupies.
/// It only shrinks the rendered font inside those cells, which is what makes
/// superscripts and subscripts possible: `n=1:d=2` is half-height text in a
/// full-height cell, and `v=` decides where in the cell it sits.
///
/// Construction validates, so a `Fraction` that exists is one the protocol can
/// express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fraction {
    numerator: u8,
    denominator: u8,
}

impl Fraction {
    /// Builds a fraction, rejecting anything the protocol cannot express.
    ///
    /// # Errors
    ///
    /// Fails if either part exceeds [`MAX_FRACTION`], if the numerator is zero
    /// (text of zero height), if the denominator is zero (which switches
    /// fractional scaling off rather than scaling by anything), or if the
    /// denominator is not greater than the numerator — the specification
    /// requires `d > n`, because a fraction here says how much *smaller* than
    /// the base size to draw.
    pub fn new(numerator: u8, denominator: u8) -> Result<Self> {
        if numerator > MAX_FRACTION || denominator > MAX_FRACTION {
            bail!(
                "fraction {numerator}/{denominator} is out of range: the protocol allows 0 to \
                 {MAX_FRACTION} for both `n` and `d`"
            );
        }
        if denominator == 0 {
            bail!(
                "a denominator of 0 switches fractional scaling off rather than scaling by \
                 anything, which is the same as passing no fraction at all"
            );
        }
        if numerator == 0 {
            bail!(
                "a numerator of 0 asks for text of zero height; the smallest useful fraction is \
                 1/{denominator}"
            );
        }
        if denominator <= numerator {
            let why = if denominator == numerator {
                "which is the base size, so it would scale nothing"
            } else {
                "which would be larger than the base size — that is what --scale is for"
            };
            bail!(
                "the protocol requires the denominator to be greater than the numerator, and \
                 {numerator}/{denominator} is not ({why}). Alignment applies only while `n < d` \
                 as well."
            );
        }
        Ok(Self {
            numerator,
            denominator,
        })
    }

    /// Parses the `NUM/DEN` spelling used by `--fraction`.
    ///
    /// # Errors
    ///
    /// Fails if the text is not two decimal integers separated by a single `/`,
    /// or if [`Fraction::new`] rejects the values.
    pub fn parse(spec: &str) -> Result<Self> {
        let trimmed = spec.trim();
        let Some((left, right)) = trimmed.split_once('/') else {
            bail!("`{spec}` is not a fraction: write it as NUM/DEN, for example `1/2`");
        };
        if right.contains('/') {
            bail!("`{spec}` has more than one `/`: a fraction is NUM/DEN, for example `1/2`");
        }
        let numerator = parse_part(left, "numerator", spec)?;
        let denominator = parse_part(right, "denominator", spec)?;
        Self::new(numerator, denominator)
    }

    /// The numerator, `n`.
    #[must_use]
    pub fn numerator(self) -> u8 {
        self.numerator
    }

    /// The denominator, `d`.
    #[must_use]
    pub fn denominator(self) -> u8 {
        self.denominator
    }

    /// `1/2`, as a reader would write it.
    #[must_use]
    pub fn label(self) -> String {
        format!("{}/{}", self.numerator, self.denominator)
    }
}

/// Parses one side of a `NUM/DEN` spelling, with an error that says which side.
fn parse_part(part: &str, side: &str, spec: &str) -> Result<u8> {
    let trimmed = part.trim();
    if trimmed.is_empty() {
        bail!("`{spec}` is missing its {side}: a fraction is NUM/DEN, for example `1/2`");
    }
    if !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("the {side} of `{spec}` is not a decimal number");
    }
    trimmed.parse::<u8>().map_err(|_| {
        anyhow::anyhow!(
            "the {side} of `{spec}` is out of range: the protocol allows 0 to {MAX_FRACTION}"
        )
    })
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// The metadata field of `OSC 66 ; <metadata> ; <text> ST`.
///
/// Each key is an `Option` so that only what was actually asked for goes on the
/// wire. Emitting `s=1:w=0:n=0:d=0:v=0:h=0` would be legal and equivalent, but
/// the sequence is meant to be readable — and a reader who sees `s=4` learns
/// something that `s=4:w=0:n=0:d=0:v=0:h=0` buries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Metadata {
    scale: Option<u8>,
    width: Option<u8>,
    fraction: Option<Fraction>,
    valign: Option<VerticalAlign>,
    halign: Option<HorizontalAlign>,
}

impl Metadata {
    /// Metadata asking for nothing: the sequence still prints its text, in the
    /// ordinary size.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `s`, the overall scale.
    ///
    /// # Errors
    ///
    /// Fails outside 1 to [`MAX_SCALE`]. Out of range is worth an error rather
    /// than a clamp: kitty clamps silently, so `s=9` would render as `s=7` with
    /// nothing to say the terminal had changed the request.
    pub fn with_scale(mut self, scale: u8) -> Result<Self> {
        let (low, high) = Key::Scale.range();
        if !(low..=high).contains(&scale) {
            bail!(
                "scale {scale} is out of range: the protocol allows {low} to {high}. Terminals \
                 clamp rather than refuse, so a larger number would silently render as {high}."
            );
        }
        self.scale = Some(scale);
        Ok(self)
    }

    /// Sets `w`, the width in cells that all the text must render inside.
    ///
    /// `0` is the protocol's default and means "split the text into cells as you
    /// normally would", so it is accepted and simply emits no `w` at all.
    ///
    /// # Errors
    ///
    /// Fails above [`MAX_SCALE`] cells.
    pub fn with_width(mut self, width: u8) -> Result<Self> {
        let (_, high) = Key::Width.range();
        if width > high {
            bail!(
                "width {width} is out of range: the protocol allows 0 to {high} cells, and 0 \
                 means the terminal counts the text as usual"
            );
        }
        // Not `Some(0)`: `w=0` and no `w` at all mean exactly the same thing to
        // the terminal, and the shorter sequence is the clearer one.
        self.width = (width > 0).then_some(width);
        Ok(self)
    }

    /// Sets `n` and `d`, the fractional scale applied on top of `s`.
    #[must_use]
    pub fn with_fraction(mut self, fraction: Fraction) -> Self {
        self.fraction = Some(fraction);
        self
    }

    /// Sets `v`, where the fractionally scaled area sits vertically.
    #[must_use]
    pub fn with_valign(mut self, align: VerticalAlign) -> Self {
        self.valign = Some(align);
        self
    }

    /// Sets `h`, where the fractionally scaled area sits horizontally.
    #[must_use]
    pub fn with_halign(mut self, align: HorizontalAlign) -> Self {
        self.halign = Some(align);
        self
    }

    /// The effective `s`, defaulting to 1.
    #[must_use]
    pub fn scale(self) -> u8 {
        self.scale.unwrap_or(1)
    }

    /// The effective `w`, defaulting to 0 — "the terminal counts it".
    #[must_use]
    pub fn width(self) -> u8 {
        self.width.unwrap_or(0)
    }

    /// The fractional scale, if one was asked for.
    #[must_use]
    pub fn fraction(self) -> Option<Fraction> {
        self.fraction
    }

    /// The vertical alignment, if one was asked for.
    #[must_use]
    pub fn valign(self) -> Option<VerticalAlign> {
        self.valign
    }

    /// The horizontal alignment, if one was asked for.
    #[must_use]
    pub fn halign(self) -> Option<HorizontalAlign> {
        self.halign
    }

    /// Whether every key is absent, so the sequence asks for nothing.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }

    /// The metadata field as it goes on the wire: `s=4:w=2`.
    ///
    /// Colon is the only separator the specification defines, and kitty's parser
    /// rejects anything else after a value — the comma that separates parameters
    /// in the kitty *graphics* protocol is an error here.
    #[must_use]
    pub fn field(self) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(6);
        if let Some(scale) = self.scale {
            parts.push(format!("s={scale}"));
        }
        if let Some(width) = self.width {
            parts.push(format!("w={width}"));
        }
        if let Some(fraction) = self.fraction {
            parts.push(format!("n={}", fraction.numerator()));
            parts.push(format!("d={}", fraction.denominator()));
        }
        if let Some(align) = self.valign {
            parts.push(format!("v={}", valign_value(align)));
        }
        if let Some(align) = self.halign {
            parts.push(format!("h={}", halign_value(align)));
        }
        parts.join(":")
    }

    /// How many cells wide and tall the block is, when `w` fixes it.
    ///
    /// `s * w` by `s`, per the specification. `None` when `w` is 0, because then
    /// the width depends on the text *and* on the terminal's own segmentation —
    /// which is the disagreement `w` exists to remove.
    #[must_use]
    pub fn block(self) -> Option<(u16, u16)> {
        let width = self.width();
        if width == 0 {
            return None;
        }
        Some((
            u16::from(self.scale()) * u16::from(width),
            u16::from(self.scale()),
        ))
    }

    /// Things that are legal but will not do what the caller probably expects.
    ///
    /// Neither is an error: every combination here is a well-formed sequence
    /// that a conforming terminal accepts. They are the cases where the
    /// *specification* quietly does less than the flag name suggests, and
    /// staying silent about them would leave a reader blaming the terminal.
    #[must_use]
    pub fn warnings(self, text: &str) -> Vec<String> {
        let mut warnings = Vec::new();

        let aligned = self.valign.is_some() || self.halign.is_some();
        if aligned && self.fraction.is_none() {
            warnings.push(
                "alignment applies only while `n < d`: it positions the fractionally scaled \
                 render area inside the full block, so with no --fraction there is nothing for \
                 `v`/`h` to move. Add --fraction, or drop the alignment."
                    .to_owned(),
            );
        }

        if let Some(width) = self.width {
            let cells = usize::from(self.scale()) * usize::from(width);
            let natural = unicode::display_width(text);
            if natural > cells {
                warnings.push(format!(
                    "all the text in one OSC 66 must render inside s*w = {cells} cells, and this \
                     text would normally take {natural}. The terminal is free to shrink the font \
                     or to truncate; the intended use of `w` is one escape code per chunk that \
                     fits."
                ));
            }
        }

        warnings
    }
}

// ---------------------------------------------------------------------------
// Sanitising the text payload
// ---------------------------------------------------------------------------

/// Neutralises everything in `text` that could escape the OSC string.
///
/// The payload sits **inside** an OSC string, which changes the rules compared
/// with [`crate::features::hyperlink::sanitize_text`]: there the visible text is
/// ordinary terminal output, so newlines and tabs are legitimate layout and are
/// kept. Here a single control byte is enough to end the string early — `ESC`
/// starts the terminator, `BEL` *is* a terminator, and ECMA-48 leaves the effect
/// of any other C0 inside a string undefined, which in practice means "ends it,
/// on some terminals". The specification requires the payload to be safe UTF-8,
/// and Ghostty's parser discards the entire command when it is not.
///
/// So every control character is replaced rather than dropped, using its glyph
/// from the Unicode Control Pictures block: `ESC` becomes `U+241B`, `BEL`
/// becomes `U+2407`, a newline becomes `U+240A`. Percent-encoding — the right
/// answer for a URI — is wrong here, because this text exists in order to be
/// read: someone asked for it in letters four cells tall. A visible `U+241B`
/// shows that something was in the input and disarms it at the same time.
///
/// The eight-bit C1 controls become `U+FFFD` instead: `0x9b` and `0x9d` are the
/// single-byte spellings of `CSI` and `OSC`, and they have no Control Pictures
/// glyph of their own.
#[must_use]
pub fn sanitize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            // C0, including ESC (0x1b), BEL (0x07), CR, LF and TAB. None of them
            // can survive inside an OSC string.
            '\u{0}'..='\u{1f}' => out.push(control_picture(ch)),
            // DEL, which the specification lists as a control code too.
            '\u{7f}' => out.push('\u{2421}'),
            // C1: the eight-bit CSI and OSC live here.
            '\u{80}'..='\u{9f}' => out.push('\u{fffd}'),
            other => out.push(other),
        }
    }
    out
}

/// Maps a C0 control onto its Control Pictures glyph: `0x1b` becomes `U+241B`.
fn control_picture(ch: char) -> char {
    char::from_u32(0x2400 + u32::from(ch)).unwrap_or('\u{fffd}')
}

/// Whether `text` would pass through [`sanitize_text`] unchanged.
#[must_use]
pub fn text_is_clean(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

// ---------------------------------------------------------------------------
// The sequence
// ---------------------------------------------------------------------------

/// A validated, sanitised `OSC 66 ; <metadata> ; <text> ST`.
///
/// Building one is the only place sanitisation happens, so a `SizedText` that
/// exists is safe to write to a terminal, whatever the text came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizedText {
    metadata: Metadata,
    text: String,
}

impl SizedText {
    /// Builds the sequence, sanitising the text on the way in.
    ///
    /// # Errors
    ///
    /// Fails on empty text — kitty ignores an `OSC 66` whose payload is empty,
    /// so it would draw nothing at all rather than a blank block — and on text
    /// longer than [`MAX_PAYLOAD`] bytes once sanitised, which the specification
    /// requires be split across several escape codes. The length is checked
    /// *after* sanitising, because each replaced control byte grows from one byte
    /// to three.
    pub fn new(metadata: Metadata, text: &str) -> Result<Self> {
        let text = sanitize_text(text);
        if text.is_empty() {
            bail!(
                "there is no text to size: an OSC 66 with an empty payload is ignored, so this \
                 would draw nothing rather than an empty block"
            );
        }
        if text.len() > MAX_PAYLOAD {
            bail!(
                "the text is {} bytes once sanitised and the protocol's limit is {MAX_PAYLOAD}; \
                 split it across several OSC 66 sequences",
                text.len()
            );
        }
        Ok(Self { metadata, text })
    }

    /// The metadata this sequence carries.
    #[must_use]
    pub fn metadata(&self) -> Metadata {
        self.metadata
    }

    /// The sanitised text, as it appears on the wire.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The whole sequence: `OSC 66 ; <metadata> ; <text> ST`.
    ///
    /// `ST` rather than `BEL`: both are legal terminators and `BEL` is what the
    /// specification's examples use, but a stray `BEL` on a terminal that does
    /// not know `OSC 66` rings the bell, and `ESC \` is the standard spelling.
    #[must_use]
    pub fn sequence(&self) -> String {
        format!("{OSC}{COMMAND};{};{}{ST}", self.metadata.field(), self.text)
    }

    /// How many cells the cursor should advance, when that is knowable.
    ///
    /// With `w` set it is `s * w`, exactly, for the whole escape code. With `w`
    /// absent the terminal splits the text itself, so the answer is `s` times
    /// whatever *it* thinks the text is wide — the prediction here uses this
    /// program's own segmentation and may differ from the terminal's, which is
    /// the entire reason `w` exists.
    #[must_use]
    pub fn predicted_cells(&self) -> usize {
        match self.metadata.block() {
            Some((width, _)) => usize::from(width),
            None => usize::from(self.metadata.scale()) * unicode::display_width(&self.text),
        }
    }

    /// Things that are legal but probably not what the caller meant.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        self.metadata.warnings(&self.text)
    }
}

/// Builds the metadata the command line asked for.
///
/// # Errors
///
/// Propagates every validation failure: an out-of-range scale or width, or a
/// malformed fraction.
pub fn metadata_from_command(command: &TextsizeCommand) -> Result<Metadata> {
    let mut metadata = Metadata::new();
    if let Some(scale) = command.scale {
        metadata = metadata.with_scale(scale)?;
    }
    if let Some(width) = command.width {
        metadata = metadata.with_width(width)?;
    }
    if let Some(spec) = &command.fraction {
        metadata = metadata.with_fraction(Fraction::parse(spec)?);
    }
    if let Some(align) = command.valign {
        metadata = metadata.with_valign(align);
    }
    if let Some(align) = command.halign {
        metadata = metadata.with_halign(align);
    }
    Ok(metadata)
}

/// The specification's own support-detection sequence for the `w` key:
/// `OSC 66 ; w=2 ; <space> ST`.
///
/// A space in two cells. A terminal that honours `w` advances two cells, one
/// that prints the text but ignores the metadata advances one, and one that has
/// never heard of `OSC 66` discards the whole string and advances none.
#[must_use]
pub fn width_detection() -> String {
    format!("{OSC}{COMMAND};w=2; {ST}")
}

/// The specification's own support-detection sequence for the `s` key:
/// `OSC 66 ; s=2 ; <space> ST`.
///
/// A space in a two-by-two block, so it advances two cells *and* occupies the
/// row below.
#[must_use]
pub fn scale_detection() -> String {
    format!("{OSC}{COMMAND};s=2; {ST}")
}

/// Entry point for `escapade textsize …`.
///
/// With no subcommand this draws text; the usual verbs sit underneath. Only
/// `probe` returns anything other than success — there, the exit status *is* the
/// answer.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with, and fails if the feature is
/// missing from the database.
pub fn run(catalog: &Catalog, command: &TextsizeCommand, no_color: bool) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        None => {
            render::run(&theme, command)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(TextsizeAction::Explain { no_matrix }) => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        Some(TextsizeAction::Probe { format }) => probe::run(&theme, *format),
        Some(TextsizeAction::Demo {
            force,
            only,
            step,
            list,
        }) => {
            if *list {
                demo::list_steps(&theme);
            } else {
                demo::run(&theme, *force, only.as_deref(), *step)?;
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The wire bytes, spelled out, so the tests assert on the sequence rather
    /// than on the constants that built it.
    const ESC_OSC: &str = "\u{1b}]";
    const ESC_ST: &str = "\u{1b}\\";

    fn sized(metadata: Metadata, text: &str) -> SizedText {
        SizedText::new(metadata, text).unwrap()
    }

    /// A command with nothing set, for the flag-plumbing tests. `TextsizeCommand`
    /// is not `Clone`, so each case builds its own.
    fn command(
        scale: Option<u8>,
        width: Option<u8>,
        fraction: Option<&str>,
        valign: Option<VerticalAlign>,
        halign: Option<HorizontalAlign>,
    ) -> TextsizeCommand {
        TextsizeCommand {
            action: None,
            text: vec!["text".to_owned()],
            scale,
            width,
            fraction: fraction.map(str::to_owned),
            valign,
            halign,
            dry_run: false,
        }
    }

    // -- keys ---------------------------------------------------------------

    #[test]
    fn every_key_has_a_distinct_letter_and_name() {
        let mut letters: Vec<char> = Key::ALL.iter().map(|key| key.letter()).collect();
        letters.sort_unstable();
        let total = letters.len();
        letters.dedup();
        assert_eq!(letters.len(), total, "two keys share a letter");
        assert_eq!(total, 6, "the protocol has exactly six keys");

        for key in Key::ALL {
            assert!(!key.name().is_empty());
            let (low, high) = key.range();
            assert!(low <= high);
            assert!(
                (low..=high).contains(&key.default_value()),
                "{}'s default is outside its own range",
                key.letter()
            );
        }
    }

    #[test]
    fn the_key_ranges_match_the_specification() {
        assert_eq!(Key::Scale.range(), (1, 7));
        assert_eq!(Key::Width.range(), (0, 7));
        assert_eq!(Key::Numerator.range(), (0, 15));
        assert_eq!(Key::Denominator.range(), (0, 15));
        assert_eq!(Key::VerticalAlign.range(), (0, 2));
        assert_eq!(Key::HorizontalAlign.range(), (0, 2));
        // Only `s` defaults to anything other than zero.
        assert_eq!(Key::Scale.default_value(), 1);
        assert_eq!(Key::Width.default_value(), 0);
    }

    #[test]
    fn centre_is_two_on_the_wire_not_the_middle_variant() {
        // The trap: the enums read top/centre/bottom and left/centre/right, but
        // the protocol numbers them top/bottom/centre and left/right/centre.
        assert_eq!(valign_value(VerticalAlign::Top), 0);
        assert_eq!(valign_value(VerticalAlign::Bottom), 1);
        assert_eq!(valign_value(VerticalAlign::Center), 2);
        assert_eq!(halign_value(HorizontalAlign::Left), 0);
        assert_eq!(halign_value(HorizontalAlign::Right), 1);
        assert_eq!(halign_value(HorizontalAlign::Center), 2);
    }

    // -- metadata -----------------------------------------------------------

    #[test]
    fn empty_metadata_is_an_empty_field() {
        let metadata = Metadata::new();
        assert!(metadata.is_empty());
        assert_eq!(metadata.field(), "");
        assert_eq!(metadata.scale(), 1, "s defaults to 1");
        assert_eq!(metadata.width(), 0, "w defaults to 0");
        assert_eq!(metadata.block(), None);
        // Still a valid sequence: it just prints the text.
        assert_eq!(
            sized(metadata, "plain").sequence(),
            format!("{ESC_OSC}66;;plain{ESC_ST}")
        );
    }

    #[test]
    fn each_key_appears_alone_when_it_is_the_only_one_set() {
        assert_eq!(Metadata::new().with_scale(4).unwrap().field(), "s=4");
        assert_eq!(Metadata::new().with_width(2).unwrap().field(), "w=2");
        assert_eq!(
            Metadata::new()
                .with_fraction(Fraction::new(1, 2).unwrap())
                .field(),
            "n=1:d=2"
        );
        assert_eq!(
            Metadata::new().with_valign(VerticalAlign::Bottom).field(),
            "v=1"
        );
        assert_eq!(
            Metadata::new().with_halign(HorizontalAlign::Center).field(),
            "h=2"
        );
    }

    #[test]
    fn every_key_at_once_is_colon_separated_in_specification_order() {
        let metadata = Metadata::new()
            .with_scale(3)
            .unwrap()
            .with_width(2)
            .unwrap()
            .with_fraction(Fraction::new(1, 2).unwrap())
            .with_valign(VerticalAlign::Center)
            .with_halign(HorizontalAlign::Right);
        assert_eq!(metadata.field(), "s=3:w=2:n=1:d=2:v=2:h=1");
        // Colon, never comma: kitty's parser rejects a comma after a value. The
        // comma belongs to the kitty *graphics* protocol.
        assert!(!metadata.field().contains(','));
        assert_eq!(metadata.field().matches(':').count(), 5);
        assert!(!metadata.is_empty());
    }

    #[test]
    fn a_scale_outside_the_range_is_refused_rather_than_clamped() {
        for scale in [0_u8, 8, 9, 255] {
            let error = Metadata::new().with_scale(scale).unwrap_err().to_string();
            assert!(error.contains("out of range"), "scale {scale}: {error}");
        }
        for scale in 1..=MAX_SCALE {
            assert_eq!(
                Metadata::new().with_scale(scale).unwrap().field(),
                format!("s={scale}")
            );
        }
    }

    #[test]
    fn a_width_of_zero_is_the_default_and_emits_nothing() {
        // `w=0` and no `w` mean the same thing to a terminal, so the shorter
        // sequence wins rather than the flag being refused.
        let metadata = Metadata::new().with_width(0).unwrap();
        assert_eq!(metadata.field(), "");
        assert_eq!(metadata.width(), 0);
        assert!(metadata.is_empty());

        let error = Metadata::new().with_width(8).unwrap_err().to_string();
        assert!(error.contains("out of range"), "{error}");
    }

    #[test]
    fn the_block_is_scale_times_width_by_scale() {
        let metadata = Metadata::new()
            .with_scale(3)
            .unwrap()
            .with_width(2)
            .unwrap();
        assert_eq!(metadata.block(), Some((6, 3)));
        // Without `w` the width depends on the terminal's own segmentation, so
        // there is no honest answer.
        assert_eq!(Metadata::new().with_scale(3).unwrap().block(), None);
        assert_eq!(
            Metadata::new().with_width(7).unwrap().block(),
            Some((7, 1)),
            "without a scale the block is one row tall"
        );
    }

    // -- fractions ----------------------------------------------------------

    #[test]
    fn a_fraction_parses_and_renders_as_two_keys() {
        let fraction = Fraction::parse("1/2").unwrap();
        assert_eq!(fraction.numerator(), 1);
        assert_eq!(fraction.denominator(), 2);
        assert_eq!(fraction.label(), "1/2");
        assert_eq!(
            Fraction::parse(" 3 / 4 ").unwrap(),
            Fraction::new(3, 4).unwrap()
        );
        // An unreduced fraction is legal: 2/4 and 1/2 are different requests
        // with the same value, and the protocol carries both.
        assert_eq!(Fraction::parse("2/4").unwrap().label(), "2/4");
        assert_eq!(Fraction::parse("1/15").unwrap().denominator(), 15);
    }

    #[test]
    fn a_malformed_fraction_says_what_is_wrong_with_it() {
        for (spec, expected) in [
            ("2", "NUM/DEN"),
            ("", "NUM/DEN"),
            ("1/2/3", "more than one"),
            ("/2", "missing its numerator"),
            ("1/", "missing its denominator"),
            ("a/2", "numerator of `a/2` is not a decimal"),
            ("1/b", "denominator of `1/b` is not a decimal"),
            ("-1/2", "not a decimal"),
            ("1.5/2", "not a decimal"),
            ("1/0", "denominator of 0"),
            ("0/2", "numerator of 0"),
            ("3/3", "base size"),
            ("3/2", "larger than the base size"),
            ("16/17", "out of range"),
            ("1/300", "out of range"),
        ] {
            let error = Fraction::parse(spec).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "fraction {spec:?} gave: {error}\nexpected it to mention {expected:?}"
            );
        }
    }

    #[test]
    fn a_fraction_must_be_smaller_than_one() {
        assert!(Fraction::new(1, 2).is_ok());
        assert!(Fraction::new(14, 15).is_ok());
        assert!(Fraction::new(0, 2).is_err(), "zero height");
        assert!(Fraction::new(2, 0).is_err(), "no denominator");
        assert!(Fraction::new(2, 2).is_err(), "the base size");
        assert!(Fraction::new(3, 2).is_err(), "bigger than the base size");
        assert!(Fraction::new(16, 17).is_err(), "out of range");
        assert!(Fraction::new(15, 15).is_err(), "the base size again");
    }

    // -- sanitising ---------------------------------------------------------

    #[test]
    fn an_escape_in_the_text_cannot_end_the_osc_string() {
        // The attack: end the OSC string early, then run whatever follows.
        let hostile = "safe\u{1b}\\\u{1b}[2J\u{1b}]0;pwned\u{7}";
        let sequence = sized(Metadata::new().with_scale(2).unwrap(), hostile).sequence();

        // Exactly two escape bytes survive: the `ESC ]` we opened with and the
        // `ESC \` we chose to terminate with.
        assert_eq!(sequence.matches('\u{1b}').count(), 2);
        assert!(sequence.starts_with(&format!("{ESC_OSC}66;s=2;")));
        assert!(sequence.ends_with(ESC_ST));
        assert!(!sequence.contains('\u{7}'), "the BEL must not survive");
        assert!(sequence.contains("safe\u{241b}\\\u{241b}[2J\u{241b}]0;pwned\u{2407}"));
    }

    #[test]
    fn text_carrying_its_own_osc_66_cannot_start_a_second_one() {
        let hostile = "big\u{1b}]66;s=7;GOTCHA\u{1b}\\";
        let sized = sized(Metadata::new().with_scale(2).unwrap(), hostile);
        assert_eq!(sized.sequence().matches("\u{1b}]66").count(), 1);
        assert!(sized.text().contains("\u{241b}]66;s=7;GOTCHA\u{241b}\\"));
    }

    #[test]
    fn a_bel_in_the_text_cannot_terminate_a_bel_terminated_string() {
        // We always end with ST, but a terminal that accepts either would have
        // stopped at a raw BEL in the payload.
        let sized = sized(Metadata::new(), "ring\u{7}ring");
        assert!(!sized.sequence().contains('\u{7}'));
        assert_eq!(sized.text(), "ring\u{2407}ring");
    }

    #[test]
    fn no_control_character_at_all_survives_into_the_payload() {
        // Unlike hyperlink text, which is outside the OSC string and may carry a
        // newline or a tab, nothing control-shaped can live inside one.
        assert_eq!(sanitize_text("a\nb"), "a\u{240a}b");
        assert_eq!(sanitize_text("a\tb"), "a\u{2409}b");
        assert_eq!(sanitize_text("a\rb"), "a\u{240d}b");
        assert_eq!(sanitize_text("nul\u{0}"), "nul\u{2400}");
        assert_eq!(sanitize_text("del\u{7f}"), "del\u{2421}");
        // 0x9b and 0x9d are the eight-bit spellings of CSI and OSC.
        assert_eq!(sanitize_text("c1\u{9b}2J"), "c1\u{fffd}2J");
        assert_eq!(sanitize_text("c1\u{9d}66;s=7;x"), "c1\u{fffd}66;s=7;x");

        for text in ["a\u{1b}b", "a\nb", "a\u{9b}b", "\u{7f}"] {
            assert!(!text_is_clean(text), "{text:?} should not be clean");
            assert!(text_is_clean(&sanitize_text(text)));
        }
    }

    #[test]
    fn ordinary_text_including_emoji_passes_through_untouched() {
        for text in ["BUILD PASSED", "caf\u{e9}", "\u{1f9d1}", "a;b:c=d", "100%"] {
            assert_eq!(sanitize_text(text), text);
            assert!(text_is_clean(text));
        }
    }

    #[test]
    fn semicolons_and_colons_in_the_text_are_safe() {
        // The payload is everything after the second semicolon, so a semicolon
        // in it cannot be mistaken for a delimiter — unlike in a hyperlink URI.
        let sized = sized(Metadata::new().with_scale(2).unwrap(), "a;b:c");
        assert_eq!(sized.sequence(), format!("{ESC_OSC}66;s=2;a;b:c{ESC_ST}"));
    }

    // -- the sequence -------------------------------------------------------

    #[test]
    fn the_sequence_has_two_semicolons_before_the_text() {
        let sized = sized(Metadata::new().with_scale(4).unwrap(), "BUILD PASSED");
        assert_eq!(
            sized.sequence(),
            format!("{ESC_OSC}66;s=4;BUILD PASSED{ESC_ST}")
        );
        assert_eq!(sized.text(), "BUILD PASSED");
        assert_eq!(sized.metadata().scale(), 4);
    }

    #[test]
    fn empty_text_is_refused_because_the_terminal_would_ignore_it() {
        let error = SizedText::new(Metadata::new(), "").unwrap_err().to_string();
        assert!(error.contains("no text to size"), "{error}");
        // A newline becomes a visible glyph, so it counts as text.
        assert!(SizedText::new(Metadata::new(), "\n").is_ok());
    }

    #[test]
    fn over_long_text_is_refused_after_sanitising_not_before() {
        // Just inside the limit.
        let fits = "a".repeat(MAX_PAYLOAD);
        assert!(SizedText::new(Metadata::new(), &fits).is_ok());
        assert!(SizedText::new(Metadata::new(), &"a".repeat(MAX_PAYLOAD + 1)).is_err());

        // Every control byte grows from one byte to three, so a payload that
        // fits before sanitising can fail afterwards. That is why the check
        // comes second.
        let escapes = "\u{1b}".repeat(MAX_PAYLOAD / 2);
        assert_eq!(escapes.len(), MAX_PAYLOAD / 2);
        let error = SizedText::new(Metadata::new(), &escapes)
            .unwrap_err()
            .to_string();
        assert!(error.contains("split it across"), "{error}");
    }

    #[test]
    fn predicted_cells_is_exact_with_a_width_and_a_guess_without_one() {
        let fixed = sized(
            Metadata::new()
                .with_scale(3)
                .unwrap()
                .with_width(2)
                .unwrap(),
            "hi",
        );
        assert_eq!(fixed.predicted_cells(), 6, "s*w, whatever the text");

        let natural = sized(Metadata::new().with_scale(2).unwrap(), "abc");
        assert_eq!(natural.predicted_cells(), 6, "s times our own width");

        let plain = sized(Metadata::new(), "abc");
        assert_eq!(plain.predicted_cells(), 3);
    }

    // -- warnings -----------------------------------------------------------

    #[test]
    fn alignment_without_a_fraction_is_flagged_rather_than_refused() {
        let metadata = Metadata::new()
            .with_scale(3)
            .unwrap()
            .with_valign(VerticalAlign::Bottom);
        let warnings = metadata.warnings("sitting low");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("n < d"), "{}", warnings[0]);
        // Still emitted: it is legal metadata, and refusing it would reject a
        // sequence terminals accept.
        assert_eq!(metadata.field(), "s=3:v=1");

        // With a fraction there is something for it to align.
        let aligned = metadata.with_fraction(Fraction::new(1, 2).unwrap());
        assert!(aligned.warnings("sitting low").is_empty());
    }

    #[test]
    fn text_too_wide_for_its_width_is_flagged() {
        let metadata = Metadata::new().with_width(2).unwrap();
        let warnings = metadata.warnings("wide");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("2 cells"), "{}", warnings[0]);

        // Two characters in two cells is exactly what `w` is for.
        assert!(metadata.warnings("hi").is_empty());
        // And scale multiplies the room available: s=3:w=2 is six cells.
        let roomy = metadata.with_scale(3).unwrap();
        assert!(roomy.warnings("wide!!").is_empty());
        assert_eq!(roomy.warnings("wide!!!").len(), 1);
    }

    #[test]
    fn nothing_unusual_produces_no_warnings() {
        let metadata = Metadata::new().with_scale(4).unwrap();
        assert!(metadata.warnings("BUILD PASSED").is_empty());
        assert!(Metadata::new().warnings("plain").is_empty());
    }

    // -- detection sequences ------------------------------------------------

    #[test]
    fn the_detection_sequences_are_the_ones_the_specification_documents() {
        assert_eq!(width_detection(), format!("{ESC_OSC}66;w=2; {ESC_ST}"));
        assert_eq!(scale_detection(), format!("{ESC_OSC}66;s=2; {ESC_ST}"));
        // A space, so the measurement is of the block and not of a glyph the
        // font might render at some other width.
        for sequence in [width_detection(), scale_detection()] {
            assert!(sequence.contains("; "));
            assert_eq!(sequence.matches('\u{1b}').count(), 2);
        }
    }

    // -- command line -------------------------------------------------------

    #[test]
    fn metadata_from_command_carries_every_flag() {
        let command = command(
            Some(2),
            Some(3),
            Some("1/2"),
            Some(VerticalAlign::Center),
            Some(HorizontalAlign::Left),
        );
        let metadata = metadata_from_command(&command).unwrap();
        assert_eq!(metadata.field(), "s=2:w=3:n=1:d=2:v=2:h=0");
        assert_eq!(metadata.block(), Some((6, 2)));
        assert_eq!(metadata.fraction().unwrap().label(), "1/2");
        assert_eq!(metadata.valign(), Some(VerticalAlign::Center));
        assert_eq!(metadata.halign(), Some(HorizontalAlign::Left));
    }

    #[test]
    fn metadata_from_command_reports_a_bad_flag() {
        assert!(metadata_from_command(&command(Some(9), None, None, None, None)).is_err());
        assert!(metadata_from_command(&command(None, Some(9), None, None, None)).is_err());
        assert!(metadata_from_command(&command(None, None, Some("nope"), None, None)).is_err());
        assert!(
            metadata_from_command(&command(None, None, None, None, None))
                .unwrap()
                .is_empty()
        );
    }
}
