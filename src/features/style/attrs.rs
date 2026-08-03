//! The attributes themselves: what each one sets, what turns it off again, and how
//! they compose into a single sequence.
//!
//! # Composition is the whole reason this is one feature
//!
//! SGR parameters are separated by `;` and applied left to right, so every attribute
//! in this module goes out in one sequence:
//!
//! ```text
//! CSI 1;3;4:3;9;38;2;255;0;0 m
//! ```
//!
//! Bold, italic, a curly underline, a strikethrough and a red foreground, in
//! thirty-odd bytes. That is why `escapade style` is one subcommand and not nine:
//! nine would force `escapade underline "$(escapade bold text)"` and produce nested
//! resets that fight each other.
//!
//! # The resets, which are not obvious
//!
//! Each attribute has a parameter that turns it off, and the mapping is neither
//! sequential nor one-to-one:
//!
//! | set | off | note |
//! |-----|-----|------|
//! | `1` bold, `2` faint | `22` | **one reset for two attributes** — you cannot turn off bold and keep faint |
//! | `3` italic | `23` | ECMA-48 also makes this "not Fraktur", `20` being the other thing it undoes |
//! | `4`, `4:1`-`4:5` underline | `24` | `4:0` says the same thing in the sub-parameter spelling |
//! | `5` slow blink, `6` rapid blink | `25` | again one reset for two |
//! | `7` reverse | `27` | |
//! | `8` conceal | `28` | |
//! | `9` crossed out | `29` | |
//! | `53` overline | `55` | note the gap: not `54`, which is "not framed, not encircled" |
//! | `38`/`30`-`37`/`90`-`97` foreground | `39` | |
//! | `48`/`40`-`47`/`100`-`107` background | `49` | |
//! | `58` underline colour | `59` | and `39` does **not** reset it, which is the point |
//!
//! The two "one reset for two attributes" rows are the ones that bite. `SGR 22` is
//! "normal colour or normal intensity" in ECMA-48 and undoes both `1` and `2`, so
//! there is no way to say "stop being bold but stay faint" — and a program that
//! tracks its own attribute state has to model that, because the terminal does.
//!
//! # `SGR 21`, which this crate never emits
//!
//! ECMA-48 gives `21` as "doubly underlined". A great many terminals implement it as
//! "bold off" instead, which is a completely different attribute. That makes `21`
//! the one SGR parameter whose meaning genuinely cannot be relied on, so
//! [`Attr::Bold`] resets with `22` and [`UnderlineShape::Double`] sets with `4:2`.
//! Neither is ambiguous, and nothing is given up by avoiding `21` entirely — see
//! [`SGR_TWENTY_ONE`].
//!
//! # Resetting only what was set
//!
//! [`Rendition::close`] emits the resets for the attributes it set and nothing else.
//! `CSI 0 m` would be shorter and is what most programs do, and it is wrong here: it
//! also clears whatever the *surrounding* program had set, so a styled word in the
//! middle of a coloured line takes the rest of the line's colour with it.

use anyhow::Result;

use crate::{
    cli::{StyleAttrs, UnderlineStyle as CliUnderlineStyle},
    features::style::{
        color::{Color, ColorRole, parse_color, parse_reported_color},
        preference::ColorPreference,
        sanitize_text,
    },
    term::CSI,
};

/// `SGR 21`: "doubly underlined" per ECMA-48, "bold off" in much of the world.
///
/// Recorded as a constant so it can be pointed at, and deliberately never emitted.
/// A sequence whose meaning depends on which terminal reads it is not a sequence
/// worth sending when unambiguous alternatives (`4:2` to set a double underline,
/// `22` to cancel bold) exist for both readings.
pub const SGR_TWENTY_ONE: u8 = 21;

// ---------------------------------------------------------------------------
// Underline shapes
// ---------------------------------------------------------------------------

/// The shape of an underline, selected by a sub-parameter of `SGR 4`.
///
/// Originated by mintty rather than by xterm, which is the surprise in this feature:
/// xterm implements none of `4:2` through `4:5`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UnderlineShape {
    /// No underline: `CSI 4:0 m`, or `CSI 24 m` for anything older.
    None,
    /// The traditional straight line: `CSI 4:1 m`, or plain `CSI 4 m`.
    Single,
    /// Two straight lines: `CSI 4:2 m`.
    Double,
    /// A wavy line: `CSI 4:3 m`. The spell-checker underline.
    Curly,
    /// A dotted line: `CSI 4:4 m`.
    Dotted,
    /// A dashed line: `CSI 4:5 m`.
    Dashed,
}

impl UnderlineShape {
    /// Every shape, in sub-parameter order.
    pub const ALL: &'static [Self] = &[
        Self::None,
        Self::Single,
        Self::Double,
        Self::Curly,
        Self::Dotted,
        Self::Dashed,
    ];

    /// The four shapes that need a terminal to have implemented something new.
    pub const EXTENDED: &'static [Self] = &[Self::Double, Self::Curly, Self::Dotted, Self::Dashed];

    /// The sub-parameter number, `0` through `5`.
    #[must_use]
    pub fn sub_parameter(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Single => 1,
            Self::Double => 2,
            Self::Curly => 3,
            Self::Dotted => 4,
            Self::Dashed => 5,
        }
    }

    /// The canonical sub-parameter spelling: `4:0` through `4:5`.
    #[must_use]
    pub fn sub_parameter_params(self) -> String {
        format!("4:{}", self.sub_parameter())
    }

    /// The parameter this crate actually emits.
    ///
    /// Identical to [`UnderlineShape::sub_parameter_params`] except for the two
    /// shapes that existed before sub-parameters did: [`UnderlineShape::Single`]
    /// emits `4` and [`UnderlineShape::None`] emits `24`. The pairs mean the same
    /// thing, and the short spellings are ECMA-48 — understood by every terminal ever
    /// built, where `4:1` needs a sub-parameter parser. Risking the default case in
    /// exchange for symmetry would be a poor trade; the demo shows the long forms,
    /// which is where seeing them is useful.
    #[must_use]
    pub fn params(self) -> String {
        match self {
            Self::None => "24".to_owned(),
            Self::Single => "4".to_owned(),
            other => other.sub_parameter_params(),
        }
    }

    /// Whether [`UnderlineShape::params`] differs from the sub-parameter spelling.
    #[must_use]
    pub fn has_portable_spelling(self) -> bool {
        matches!(self, Self::None | Self::Single)
    }

    /// The kebab-case name, matching the `--underline=` values.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Single => "single",
            Self::Double => "double",
            Self::Curly => "curly",
            Self::Dotted => "dotted",
            Self::Dashed => "dashed",
        }
    }

    /// One line on what the shape is for.
    #[must_use]
    pub fn purpose(self) -> &'static str {
        match self {
            Self::None => "switches underlining off, which is how a run of styled text ends",
            Self::Single => "the plain underline, and the only one that needs no new support",
            Self::Double => "two lines, for a heading or a total",
            Self::Curly => "the spell-checker and compiler-error underline",
            Self::Dotted => "a lighter mark: a hint, a suggestion, a deprecation",
            Self::Dashed => "the other lighter mark, for when dotted is taken",
        }
    }

    /// A picture of the shape drawn from box-drawing characters every monospace font
    /// has, so a reader can see which shape a row is about even on a terminal that
    /// draws none of them.
    #[must_use]
    pub fn sketch(self) -> &'static str {
        match self {
            Self::None => "      ",
            Self::Single => "──────",
            Self::Double => "══════",
            Self::Curly => "\u{223f}\u{223f}\u{223f}\u{223f}\u{223f}\u{223f}",
            Self::Dotted => "\u{2508}\u{2508}\u{2508}\u{2508}\u{2508}\u{2508}",
            Self::Dashed => "\u{254c}\u{254c}\u{254c}\u{254c}\u{254c}\u{254c}",
        }
    }
}

impl From<CliUnderlineStyle> for UnderlineShape {
    fn from(style: CliUnderlineStyle) -> Self {
        match style {
            CliUnderlineStyle::None => Self::None,
            CliUnderlineStyle::Single => Self::Single,
            CliUnderlineStyle::Double => Self::Double,
            CliUnderlineStyle::Curly => Self::Curly,
            CliUnderlineStyle::Dotted => Self::Dotted,
            CliUnderlineStyle::Dashed => Self::Dashed,
        }
    }
}

// ---------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------

/// One SGR text attribute, with whatever it needs to be concrete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attr {
    /// `SGR 1`, "bold or increased intensity". Rendered by some terminals as a
    /// heavier font, by others as a brighter colour, and by several as both.
    Bold,
    /// `SGR 2`, "faint, decreased intensity". Quietly ignored by a fair number of
    /// terminals, which is a good reason to probe rather than assume.
    Dim,
    /// `SGR 3`, "italicized".
    Italic,
    /// `SGR 4`, with an optional sub-parameter for the shape.
    Underline(UnderlineShape),
    /// `SGR 5`, "slowly blinking". Present in ECMA-48 and in the VT100, and
    /// deliberately not animated by a number of modern terminals.
    Blink,
    /// `SGR 6`, "rapidly blinking". Almost universally treated as `SGR 5`, which a
    /// DECRQSS readback catches: a terminal that folded it reports `5`.
    RapidBlink,
    /// `SGR 7`, "negative image" — foreground and background swapped.
    Reverse,
    /// `SGR 8`, "concealed characters". Never a security measure: the text is still
    /// in the cell, still selectable and still copied.
    Conceal,
    /// `SGR 9`, "crossed-out".
    Strikethrough,
    /// `SGR 53`, "overlined". The rarest of the family.
    Overline,
    /// One of the three independent colours.
    Color(ColorRole, Color),
}

impl Attr {
    /// The SGR parameters that set this attribute, without the `CSI` or the `m`.
    #[must_use]
    pub fn params(self) -> String {
        match self {
            Self::Bold => "1".to_owned(),
            Self::Dim => "2".to_owned(),
            Self::Italic => "3".to_owned(),
            Self::Underline(shape) => shape.params(),
            Self::Blink => "5".to_owned(),
            Self::RapidBlink => "6".to_owned(),
            Self::Reverse => "7".to_owned(),
            Self::Conceal => "8".to_owned(),
            Self::Strikethrough => "9".to_owned(),
            Self::Overline => "53".to_owned(),
            Self::Color(role, color) => color.params(role, role.default_syntax()),
        }
    }

    /// The parameter that turns this attribute off again.
    ///
    /// See the module documentation for the whole table and for the two places where
    /// one reset covers two attributes.
    #[must_use]
    pub fn reset_params(self) -> &'static str {
        match self {
            // ECMA-48 22 is "normal colour or normal intensity" and undoes both `1`
            // and `2`. Not 21, whose meaning is contested — see `SGR_TWENTY_ONE`.
            Self::Bold | Self::Dim => "22",
            Self::Italic => "23",
            Self::Underline(_) => "24",
            Self::Blink | Self::RapidBlink => "25",
            Self::Reverse => "27",
            Self::Conceal => "28",
            Self::Strikethrough => "29",
            // 55, not 54: 54 is "not framed, not encircled", the reset for 51 and 52.
            Self::Overline => "55",
            Self::Color(ColorRole::Foreground, _) => "39",
            Self::Color(ColorRole::Background, _) => "49",
            Self::Color(ColorRole::Underline, _) => "59",
        }
    }

    /// The kebab-case name used in reports, JSON and the demo.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bold => "bold",
            Self::Dim => "dim",
            Self::Italic => "italic",
            Self::Underline(_) => "underline",
            Self::Blink => "blink",
            Self::RapidBlink => "rapid-blink",
            Self::Reverse => "reverse",
            Self::Conceal => "conceal",
            Self::Strikethrough => "strikethrough",
            Self::Overline => "overline",
            Self::Color(role, _) => role.name(),
        }
    }

    /// A label that distinguishes attributes sharing a [`Attr::name`], for tables:
    /// `underline:curly`, `foreground:truecolor`.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Underline(shape) => format!("underline:{}", shape.name()),
            Self::Color(role, color) => format!("{}:{}", role.name(), color.depth().name()),
            other => other.name().to_owned(),
        }
    }

    /// The command-line flag that asks for this attribute.
    #[must_use]
    pub fn flag(self) -> &'static str {
        match self {
            Self::Bold => "--bold",
            Self::Dim => "--dim",
            Self::Italic => "--italic",
            Self::Underline(_) => "--underline",
            Self::Blink => "--blink",
            Self::RapidBlink => "--rapid-blink",
            Self::Reverse => "--reverse",
            Self::Conceal => "--conceal",
            Self::Strikethrough => "--strikethrough",
            Self::Overline => "--overline",
            Self::Color(role, _) => role.flag(),
        }
    }

    /// Whether this attribute is a colour, and therefore subject to `NO_COLOR`.
    #[must_use]
    pub fn is_color(self) -> bool {
        matches!(self, Self::Color(..))
    }

    /// The colour role, when this is a colour.
    #[must_use]
    pub fn role(self) -> Option<ColorRole> {
        match self {
            Self::Color(role, _) => Some(role),
            _ => None,
        }
    }

    /// Whether this attribute needs nothing checked because it predates all of this.
    ///
    /// Three of the family were in the VT100 in 1978 and are both parsed *and*
    /// rendered by every terminal since: bold, a plain underline, and reverse video.
    /// Blink is deliberately excluded even though the VT100 had it, because a
    /// number of modern terminals parse `SGR 5` and choose not to animate it —
    /// "supported" would be a misleading answer, and this is the one place where
    /// silence beats confidence.
    #[must_use]
    pub fn is_universal(self) -> bool {
        matches!(
            self,
            Self::Bold
                | Self::Reverse
                | Self::Underline(UnderlineShape::None | UnderlineShape::Single)
        )
    }

    /// Whether an SGR report retains this attribute.
    ///
    /// This is the reading half of the measurement. The report is an ordinary SGR
    /// parameter list — whatever the terminal says it would have to emit to
    /// reproduce its current state — so "did it keep what I set" is a question about
    /// that list.
    ///
    /// [`UnderlineShape::Double`] has a second acceptable answer: a terminal that
    /// normalises `4:2` to ECMA-48's `21` on the way out has kept the information,
    /// even though this crate would not have emitted `21` itself.
    #[must_use]
    pub fn report_keeps(self, params: &str) -> bool {
        match self {
            Self::Underline(shape) => report_keeps_underline(params, shape),
            Self::Color(role, wanted) => {
                parse_reported_color(params, role).is_some_and(|got| wanted.same_as(got))
            }
            simple => {
                let wanted = simple.params();
                params.split(';').any(|field| field == wanted)
            }
        }
    }

    /// What the terminal reported *instead*, when it did not keep what was set.
    ///
    /// The interesting case is a quantised colour: a terminal handed `38;2;1;2;3`
    /// that answers `38;5;16` has accepted the sequence and thrown the colour away,
    /// and naming the palette entry it landed on is the difference between a useful
    /// report and a bare "no".
    #[must_use]
    pub fn report_substitute(self, params: &str) -> Option<String> {
        match self {
            Self::Color(role, _) => parse_reported_color(params, role).map(Color::describe),
            Self::Underline(shape) if !shape.has_portable_spelling() => UnderlineShape::ALL
                .iter()
                .filter(|other| **other != shape)
                .find(|other| report_keeps_underline(params, **other))
                .map(|other| format!("a {} underline", other.name())),
            _ => None,
        }
    }
}

/// Whether an SGR report retains the sub-parameter for `shape`.
fn report_keeps_underline(params: &str, shape: UnderlineShape) -> bool {
    params.split(';').any(|field| {
        let mut parts = field.split(':');
        let head = parts.next().unwrap_or_default();
        let sub = parts.next();
        match shape {
            // `4` with no sub-parameter, or `4:1`.
            UnderlineShape::Single => head == "4" && matches!(sub, None | Some("1")),
            // `24` or `4:0`. Absence is not evidence: a report with no underline
            // parameter at all is a report of "not underlined", which is what was
            // asked for.
            UnderlineShape::None => {
                head == "24"
                    || (head == "4" && sub == Some("0"))
                    || !params_mention_underline(params)
            }
            // A terminal may normalise `4:2` to ECMA-48's `21`.
            UnderlineShape::Double => {
                head == SGR_TWENTY_ONE.to_string() || (head == "4" && sub == Some("2"))
            }
            other => head == "4" && sub == Some(other.sub_parameter().to_string().as_str()),
        }
    })
}

/// Whether an SGR report says anything about underlining at all.
fn params_mention_underline(params: &str) -> bool {
    params
        .split(';')
        .any(|field| field.split(':').next() == Some("4"))
}

// ---------------------------------------------------------------------------
// Composing them
// ---------------------------------------------------------------------------

/// A set of attributes, in the order they will be emitted.
///
/// Kept as an ordered list rather than a set of flags so that `--dry-run` can walk
/// the sequence in the same order the terminal will, and so the resets can be
/// derived from exactly what was set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendition {
    attrs: Vec<Attr>,
}

impl Rendition {
    /// An empty rendition, which emits nothing at all.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an attribute.
    #[must_use]
    pub fn with(mut self, attr: Attr) -> Self {
        self.attrs.push(attr);
        self
    }

    /// Builds a rendition from the command-line flags, honouring `NO_COLOR`.
    ///
    /// Returns the rendition and the colour attributes that were **dropped** because
    /// the user asked for no colour, so the caller can say so on stderr. The text is
    /// never withheld along with the colour: `NO_COLOR` asks for output without
    /// colour, not for no output.
    ///
    /// # Errors
    ///
    /// Propagates [`parse_color`] for each of the three colour flags, so a bad colour
    /// is reported before anything is written.
    pub fn from_flags(
        flags: &StyleAttrs,
        preference: &ColorPreference,
    ) -> Result<(Self, Vec<Attr>)> {
        let mut rendition = Self::new();
        let mut dropped = Vec::new();

        // Order matches the order of the flags in `StyleAttrs`, which is SGR order,
        // so the emitted sequence reads the way the table in the module
        // documentation does.
        for (asked, attr) in [
            (flags.bold, Attr::Bold),
            (flags.dim, Attr::Dim),
            (flags.italic, Attr::Italic),
        ] {
            if asked {
                rendition = rendition.with(attr);
            }
        }
        if let Some(style) = flags.underline {
            rendition = rendition.with(Attr::Underline(UnderlineShape::from(style)));
        }
        for (asked, attr) in [
            (flags.blink, Attr::Blink),
            (flags.rapid_blink, Attr::RapidBlink),
            (flags.reverse, Attr::Reverse),
            (flags.conceal, Attr::Conceal),
            (flags.strikethrough, Attr::Strikethrough),
            (flags.overline, Attr::Overline),
        ] {
            if asked {
                rendition = rendition.with(attr);
            }
        }

        for (value, role) in [
            (flags.color.as_deref(), ColorRole::Foreground),
            (flags.background.as_deref(), ColorRole::Background),
            (flags.underline_color.as_deref(), ColorRole::Underline),
        ] {
            let Some(value) = value else { continue };
            let attr = Attr::Color(role, parse_color(value)?);
            if preference.suppresses_color() {
                dropped.push(attr);
            } else {
                rendition = rendition.with(attr);
            }
        }

        Ok((rendition, dropped))
    }

    /// The attributes, in emission order.
    #[must_use]
    pub fn attrs(&self) -> &[Attr] {
        &self.attrs
    }

    /// Whether there is nothing to emit.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.attrs.is_empty()
    }

    /// The parameters of the opening sequence, without the `CSI` or the `m`.
    #[must_use]
    pub fn params(&self) -> String {
        self.attrs
            .iter()
            .map(|attr| attr.params())
            .collect::<Vec<_>>()
            .join(";")
    }

    /// The parameters of the closing sequence, deduplicated.
    ///
    /// Deduplication is not cosmetic: `--bold --dim` both reset with `22`, and
    /// emitting `22;22` would be a visible statement that the author did not know
    /// the mapping.
    #[must_use]
    pub fn reset_params(&self) -> String {
        let mut resets: Vec<&str> = Vec::with_capacity(self.attrs.len());
        for attr in &self.attrs {
            let reset = attr.reset_params();
            if !resets.contains(&reset) {
                resets.push(reset);
            }
        }
        resets.join(";")
    }

    /// The sequence that applies the attributes: `CSI <params> m`, or nothing when
    /// there are no attributes.
    #[must_use]
    pub fn open(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        format!("{CSI}{}m", self.params())
    }

    /// The sequence that puts things back.
    ///
    /// Only what was set is unset. `CSI 0 m` would be shorter and would also clear
    /// whatever the surrounding program had configured, which is how a styled word
    /// in the middle of a coloured line ends up taking the rest of the line with it.
    #[must_use]
    pub fn close(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        format!("{CSI}{}m", self.reset_params())
    }

    /// Open sequence, sanitised text, close sequence.
    #[must_use]
    pub fn wrap(&self, text: &str) -> String {
        format!("{}{}{}", self.open(), sanitize_text(text), self.close())
    }

    /// The same bytes as a `printf` one-liner that can be pasted into a shell.
    #[must_use]
    pub fn printf_form(&self, text: &str) -> String {
        let mut body = String::with_capacity(text.len() + 48);
        for ch in self.wrap(text).chars() {
            match ch {
                '\u{1b}' => body.push_str("\\e"),
                '\n' => body.push_str("\\n"),
                '\t' => body.push_str("\\t"),
                '\\' => body.push_str("\\\\"),
                '\'' => body.push_str("'\\''"),
                '%' => body.push_str("%%"),
                other => body.push(other),
            }
        }
        format!("printf '{body}\\n'")
    }
}

/// One attribute of every kind, for tables and for a bare `probe`.
///
/// The colours appear once per depth rather than once per role, plus one background
/// and one underline colour, because depth is what a terminal either implements or
/// does not — asking about `38;2` and `48;2` separately would mostly restate the
/// same answer, but not always, so both are here.
#[must_use]
pub fn every_attribute() -> Vec<Attr> {
    let mut attrs = vec![Attr::Bold, Attr::Dim, Attr::Italic];
    attrs.extend(UnderlineShape::ALL.iter().copied().map(Attr::Underline));
    attrs.extend([
        Attr::Blink,
        Attr::RapidBlink,
        Attr::Reverse,
        Attr::Conceal,
        Attr::Strikethrough,
        Attr::Overline,
    ]);
    for depth_color in probe_colors() {
        attrs.push(Attr::Color(ColorRole::Foreground, depth_color));
    }
    attrs.push(Attr::Color(ColorRole::Background, PROBE_RGB));
    attrs.push(Attr::Color(ColorRole::Underline, PROBE_RGB));
    attrs
}

/// The 24-bit value the probe sets.
///
/// `#010203` on purpose. Every component is small, and no two are equal, so nothing
/// about it can survive being rounded to a palette entry or to one of the basic
/// sixteen: a terminal that reports it back really did keep 24 bits. A round value
/// like `#ff0000` would come back unchanged from a terminal that quantised it to
/// bright red, and the measurement would silently be worthless.
pub const PROBE_RGB: Color = Color::Rgb(1, 2, 3);

/// The palette entry the probe sets: `38;5;173`, well outside the first sixteen so
/// it cannot be confused with a basic colour.
pub const PROBE_PALETTE: Color = Color::Palette(173);

/// The basic colour the probe sets: red, the least interesting possible choice,
/// because the question at this depth is only whether `SGR 31` works at all.
pub const PROBE_BASIC: Color = Color::Basic(1);

/// One colour per depth, for probing.
#[must_use]
pub fn probe_colors() -> Vec<Color> {
    vec![PROBE_BASIC, PROBE_PALETTE, PROBE_RGB]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::style::color::ColorDepth;

    /// Flags with nothing set, to build test cases from.
    fn no_flags() -> StyleAttrs {
        StyleAttrs {
            bold: false,
            dim: false,
            italic: false,
            underline: None,
            blink: false,
            rapid_blink: false,
            reverse: false,
            conceal: false,
            strikethrough: false,
            overline: false,
            color: None,
            background: None,
            underline_color: None,
        }
    }

    // -- underline shapes ---------------------------------------------------

    #[test]
    fn every_shape_has_the_sub_parameter_the_protocol_gives_it() {
        for (shape, sub) in [
            (UnderlineShape::None, 0),
            (UnderlineShape::Single, 1),
            (UnderlineShape::Double, 2),
            (UnderlineShape::Curly, 3),
            (UnderlineShape::Dotted, 4),
            (UnderlineShape::Dashed, 5),
        ] {
            assert_eq!(shape.sub_parameter(), sub, "{}", shape.name());
            assert_eq!(shape.sub_parameter_params(), format!("4:{sub}"));
        }
        let subs: Vec<u8> = UnderlineShape::ALL
            .iter()
            .map(|shape| shape.sub_parameter())
            .collect();
        assert_eq!(subs, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn the_two_pre_sub_parameter_shapes_keep_their_portable_spelling() {
        assert_eq!(UnderlineShape::Single.params(), "4");
        assert_eq!(UnderlineShape::None.params(), "24");
        assert!(UnderlineShape::Single.has_portable_spelling());
        assert!(UnderlineShape::None.has_portable_spelling());
        // The canonical spellings stay reachable, because the demo shows them.
        assert_eq!(UnderlineShape::Single.sub_parameter_params(), "4:1");
        assert_eq!(UnderlineShape::None.sub_parameter_params(), "4:0");
    }

    #[test]
    fn the_four_new_shapes_are_emitted_as_one_parameter_with_a_sub_parameter() {
        for shape in UnderlineShape::EXTENDED {
            assert_eq!(shape.params(), shape.sub_parameter_params());
            assert!(!shape.has_portable_spelling());
            assert!(shape.params().contains(':'));
            assert!(!shape.params().contains(';'));
        }
        assert_eq!(UnderlineShape::EXTENDED.len(), 4);
    }

    #[test]
    fn shapes_map_from_the_command_line_values() {
        for (cli, shape) in [
            (CliUnderlineStyle::None, UnderlineShape::None),
            (CliUnderlineStyle::Single, UnderlineShape::Single),
            (CliUnderlineStyle::Double, UnderlineShape::Double),
            (CliUnderlineStyle::Curly, UnderlineShape::Curly),
            (CliUnderlineStyle::Dotted, UnderlineShape::Dotted),
            (CliUnderlineStyle::Dashed, UnderlineShape::Dashed),
        ] {
            assert_eq!(UnderlineShape::from(cli), shape);
        }
    }

    #[test]
    fn every_sketch_is_printable_and_the_same_width() {
        for shape in UnderlineShape::ALL {
            assert!(!shape.sketch().contains('\u{1b}'));
            assert_eq!(shape.sketch().chars().count(), 6, "{}", shape.name());
            assert!(!shape.purpose().is_empty());
        }
    }

    // -- the reset mapping --------------------------------------------------

    #[test]
    fn every_attribute_resets_with_the_parameter_ecma_48_gives_it() {
        for (attr, reset) in [
            (Attr::Bold, "22"),
            (Attr::Dim, "22"),
            (Attr::Italic, "23"),
            (Attr::Underline(UnderlineShape::Curly), "24"),
            (Attr::Underline(UnderlineShape::Single), "24"),
            (Attr::Blink, "25"),
            (Attr::RapidBlink, "25"),
            (Attr::Reverse, "27"),
            (Attr::Conceal, "28"),
            (Attr::Strikethrough, "29"),
            (Attr::Overline, "55"),
            (Attr::Color(ColorRole::Foreground, PROBE_RGB), "39"),
            (Attr::Color(ColorRole::Background, PROBE_RGB), "49"),
            (Attr::Color(ColorRole::Underline, PROBE_RGB), "59"),
        ] {
            assert_eq!(attr.reset_params(), reset, "{}", attr.label());
        }
    }

    #[test]
    fn one_reset_covers_two_attributes_in_exactly_two_places() {
        // The rows that bite: there is no way to cancel bold and keep faint, nor to
        // cancel rapid blink and keep slow blink. A program tracking its own state
        // has to model that, because the terminal does.
        assert_eq!(Attr::Bold.reset_params(), Attr::Dim.reset_params());
        assert_eq!(Attr::Blink.reset_params(), Attr::RapidBlink.reset_params());

        // …and nowhere else. Every other reset belongs to one attribute.
        let mut pairs: Vec<(&str, &str)> = every_attribute()
            .into_iter()
            .map(|attr| (attr.name(), attr.reset_params()))
            .collect();
        pairs.dedup();
        let mut shared: Vec<&str> = Vec::new();
        for (index, (_, reset)) in pairs.iter().enumerate() {
            if pairs
                .iter()
                .enumerate()
                .any(|(other, (_, r))| other != index && r == reset)
            {
                shared.push(reset);
            }
        }
        shared.sort_unstable();
        shared.dedup();
        // Exactly two: `22` for bold/faint and `25` for the two blinks. `24` is not
        // among them, because the six underline shapes are one attribute wearing six
        // hats rather than six attributes — which is why the list above collapses
        // them before looking for genuinely shared resets.
        assert_eq!(shared, vec!["22", "25"]);
    }

    #[test]
    fn overline_resets_with_55_and_not_54() {
        // 54 is "not framed, not encircled" — the reset for 51 and 52, which
        // essentially nothing implements. Getting this wrong would silently do
        // nothing on every terminal that does implement overline.
        assert_eq!(Attr::Overline.reset_params(), "55");
        assert_ne!(Attr::Overline.reset_params(), "54");
    }

    #[test]
    fn the_three_colours_have_three_separate_resets() {
        let resets: Vec<&str> = ColorRole::ALL
            .iter()
            .map(|role| Attr::Color(*role, PROBE_RGB).reset_params())
            .collect();
        assert_eq!(resets, vec!["39", "49", "59"]);
        // `39` must not reset the underline colour: that independence is the reason
        // a linter can mark a word without touching the syntax highlighting.
        assert_ne!(
            Attr::Color(ColorRole::Foreground, PROBE_RGB).reset_params(),
            Attr::Color(ColorRole::Underline, PROBE_RGB).reset_params()
        );
    }

    #[test]
    fn sgr_21_is_never_emitted_by_anything() {
        // Its meaning is genuinely contested — "doubly underlined" in ECMA-48, "bold
        // off" in much of the world — so nothing here may depend on it.
        assert_eq!(SGR_TWENTY_ONE, 21);
        for attr in every_attribute() {
            assert_ne!(attr.params(), "21", "{}", attr.label());
            assert_ne!(attr.reset_params(), "21", "{}", attr.label());
        }
        assert_eq!(UnderlineShape::Double.params(), "4:2");
        assert_eq!(Attr::Bold.reset_params(), "22");
    }

    // -- setting them -------------------------------------------------------

    #[test]
    fn every_attribute_sets_the_parameter_its_documentation_names() {
        for (attr, params) in [
            (Attr::Bold, "1"),
            (Attr::Dim, "2"),
            (Attr::Italic, "3"),
            (Attr::Underline(UnderlineShape::Single), "4"),
            (Attr::Underline(UnderlineShape::Curly), "4:3"),
            (Attr::Blink, "5"),
            (Attr::RapidBlink, "6"),
            (Attr::Reverse, "7"),
            (Attr::Conceal, "8"),
            (Attr::Strikethrough, "9"),
            (Attr::Overline, "53"),
            (Attr::Color(ColorRole::Foreground, Color::Basic(1)), "31"),
            (
                Attr::Color(ColorRole::Foreground, Color::Palette(173)),
                "38;5;173",
            ),
            (
                Attr::Color(ColorRole::Foreground, Color::Rgb(1, 2, 3)),
                "38;2;1;2;3",
            ),
            (
                Attr::Color(ColorRole::Background, Color::Rgb(1, 2, 3)),
                "48;2;1;2;3",
            ),
            // The underline colour is spelled with colons on purpose.
            (
                Attr::Color(ColorRole::Underline, Color::Rgb(1, 2, 3)),
                "58:2::1:2:3",
            ),
        ] {
            assert_eq!(attr.params(), params, "{}", attr.label());
        }
    }

    #[test]
    fn names_flags_and_labels_are_all_distinct_where_they_need_to_be() {
        let attrs = every_attribute();
        // Labels identify an attribute uniquely; names deliberately do not, because
        // six underline shapes are one attribute.
        let mut labels: Vec<String> = attrs.iter().map(|attr| attr.label()).collect();
        let count = labels.len();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), count, "two attributes share a label");

        for attr in &attrs {
            assert!(attr.flag().starts_with("--"), "{}", attr.label());
            assert!(!attr.name().is_empty());
        }
        assert_eq!(
            Attr::Underline(UnderlineShape::Curly).label(),
            "underline:curly"
        );
        assert_eq!(
            Attr::Color(ColorRole::Foreground, PROBE_RGB).label(),
            "foreground:truecolor"
        );
        assert_eq!(Attr::Underline(UnderlineShape::Curly).flag(), "--underline");
    }

    #[test]
    fn only_the_colours_are_colours() {
        for attr in every_attribute() {
            assert_eq!(attr.is_color(), attr.role().is_some(), "{}", attr.label());
            assert_eq!(attr.is_color(), matches!(attr, Attr::Color(..)));
        }
        assert!(!Attr::Bold.is_color());
        assert!(Attr::Color(ColorRole::Underline, PROBE_RGB).is_color());
    }

    #[test]
    fn only_the_vt100_attributes_are_treated_as_universal() {
        // Blink is excluded on purpose: the VT100 had it, and modern terminals
        // routinely parse it and decline to animate it, so "supported" would
        // mislead.
        assert!(Attr::Bold.is_universal());
        assert!(Attr::Reverse.is_universal());
        assert!(Attr::Underline(UnderlineShape::Single).is_universal());
        assert!(Attr::Underline(UnderlineShape::None).is_universal());
        for attr in [
            Attr::Dim,
            Attr::Italic,
            Attr::Blink,
            Attr::RapidBlink,
            Attr::Conceal,
            Attr::Strikethrough,
            Attr::Overline,
            Attr::Underline(UnderlineShape::Curly),
            Attr::Color(ColorRole::Foreground, PROBE_RGB),
        ] {
            assert!(!attr.is_universal(), "{}", attr.label());
        }
    }

    // -- composing ----------------------------------------------------------

    #[test]
    fn attributes_compose_into_one_sequence() {
        let rendition = Rendition::new()
            .with(Attr::Bold)
            .with(Attr::Italic)
            .with(Attr::Underline(UnderlineShape::Curly))
            .with(Attr::Strikethrough)
            .with(Attr::Color(ColorRole::Foreground, Color::Rgb(255, 0, 0)));
        assert_eq!(rendition.params(), "1;3;4:3;9;38;2;255;0;0");
        assert_eq!(rendition.open(), "\u{1b}[1;3;4:3;9;38;2;255;0;0m");
        assert_eq!(rendition.close(), "\u{1b}[22;23;24;29;39m");
        assert_eq!(rendition.attrs().len(), 5);
        // One sequence, not five: that is the reason this is one subcommand.
        assert_eq!(rendition.open().matches('\u{1b}').count(), 1);
    }

    #[test]
    fn resets_are_deduplicated() {
        // `--bold --dim` both reset with 22, and `22;22` would advertise that the
        // author did not know the mapping.
        let rendition = Rendition::new().with(Attr::Bold).with(Attr::Dim);
        assert_eq!(rendition.params(), "1;2");
        assert_eq!(rendition.close(), "\u{1b}[22m");

        let blinks = Rendition::new().with(Attr::Blink).with(Attr::RapidBlink);
        assert_eq!(blinks.close(), "\u{1b}[25m");

        // Six underline shapes are still one reset, however many are asked for.
        let mut underlines = Rendition::new();
        for shape in UnderlineShape::EXTENDED {
            underlines = underlines.with(Attr::Underline(*shape));
        }
        assert_eq!(underlines.close(), "\u{1b}[24m");
    }

    #[test]
    fn an_empty_rendition_emits_nothing_at_all() {
        let rendition = Rendition::new();
        assert!(rendition.is_empty());
        assert_eq!(rendition.open(), "");
        assert_eq!(rendition.close(), "");
        assert_eq!(rendition.params(), "");
        assert_eq!(rendition.reset_params(), "");
        // `escapade style 'text'` with no flags prints the text and no sequences.
        assert_eq!(rendition.wrap("text"), "text");
        assert!(!rendition.wrap("text").contains('\u{1b}'));
    }

    #[test]
    fn nothing_can_leave_an_attribute_set_after_the_text() {
        for attr in every_attribute() {
            let rendition = Rendition::new().with(attr);
            let emitted = rendition.wrap("text");
            assert!(
                emitted.ends_with(&rendition.close()),
                "{} does not put itself back",
                attr.label()
            );
            assert_eq!(emitted.matches('\u{1b}').count(), 2, "{}", attr.label());
            // The close names the reset for exactly the attribute that was set.
            assert_eq!(
                rendition.close(),
                format!("\u{1b}[{}m", attr.reset_params())
            );
        }
    }

    #[test]
    fn close_resets_only_what_was_set() {
        // `CSI 0 m` would be shorter and would also clear the surrounding
        // program's attributes, taking the rest of a coloured line with it.
        let rendition = Rendition::new().with(Attr::Bold);
        assert_eq!(rendition.close(), "\u{1b}[22m");
        assert_ne!(rendition.close(), "\u{1b}[0m");
        assert!(!rendition.close().contains('0'));
    }

    // -- from the command line ---------------------------------------------

    #[test]
    fn flags_become_attributes_in_sgr_order() {
        let flags = StyleAttrs {
            bold: true,
            italic: true,
            underline: Some(CliUnderlineStyle::Curly),
            strikethrough: true,
            overline: true,
            color: Some("red".to_owned()),
            background: Some("42".to_owned()),
            underline_color: Some("#010203".to_owned()),
            ..no_flags()
        };
        let (rendition, dropped) =
            Rendition::from_flags(&flags, &ColorPreference::allowed()).unwrap();
        assert!(dropped.is_empty());
        assert_eq!(rendition.params(), "1;3;4:3;9;53;31;48;5;42;58:2::1:2:3");
        assert_eq!(rendition.close(), "\u{1b}[22;23;24;29;55;39;49;59m");
    }

    #[test]
    fn no_flags_at_all_is_an_empty_rendition_and_not_an_error() {
        let (rendition, dropped) =
            Rendition::from_flags(&no_flags(), &ColorPreference::allowed()).unwrap();
        assert!(rendition.is_empty());
        assert!(dropped.is_empty());
    }

    #[test]
    fn a_bad_colour_is_refused_before_anything_is_written() {
        let flags = StyleAttrs {
            color: Some("reddish".to_owned()),
            ..no_flags()
        };
        let error = Rendition::from_flags(&flags, &ColorPreference::allowed())
            .unwrap_err()
            .to_string();
        assert!(error.contains("#rrggbb"), "{error}");
    }

    // -- NO_COLOR -----------------------------------------------------------

    #[test]
    fn no_color_drops_the_colours_and_keeps_everything_else() {
        // The spec is about colour, not styling: a curly underline is a shape.
        let flags = StyleAttrs {
            bold: true,
            italic: true,
            underline: Some(CliUnderlineStyle::Curly),
            strikethrough: true,
            color: Some("red".to_owned()),
            background: Some("#010203".to_owned()),
            underline_color: Some("blue".to_owned()),
            ..no_flags()
        };
        let (rendition, dropped) =
            Rendition::from_flags(&flags, &ColorPreference::from_value(Some("1"))).unwrap();

        assert_eq!(rendition.params(), "1;3;4:3;9");
        assert!(!rendition.attrs().iter().any(|attr| attr.is_color()));
        assert_eq!(dropped.len(), 3);
        assert!(dropped.iter().all(|attr| attr.is_color()));
        // The text is never withheld along with the colour.
        assert!(rendition.wrap("text").contains("text"));
    }

    #[test]
    fn no_color_equals_zero_still_drops_the_colours() {
        let flags = StyleAttrs {
            color: Some("red".to_owned()),
            ..no_flags()
        };
        let (rendition, dropped) =
            Rendition::from_flags(&flags, &ColorPreference::from_value(Some("0"))).unwrap();
        assert!(rendition.is_empty());
        assert_eq!(dropped.len(), 1);
    }

    #[test]
    fn an_empty_no_color_keeps_the_colours() {
        let flags = StyleAttrs {
            color: Some("red".to_owned()),
            ..no_flags()
        };
        let (rendition, dropped) =
            Rendition::from_flags(&flags, &ColorPreference::from_value(Some(""))).unwrap();
        assert_eq!(rendition.params(), "31");
        assert!(dropped.is_empty());
    }

    #[test]
    fn a_bad_colour_is_still_refused_under_no_color() {
        // Dropping the colour must not mean skipping its validation: a typo should
        // be reported whether or not the colour would have been emitted.
        let flags = StyleAttrs {
            color: Some("nonsense".to_owned()),
            ..no_flags()
        };
        assert!(Rendition::from_flags(&flags, &ColorPreference::from_value(Some("1"))).is_err());
    }

    // -- reading a report back ---------------------------------------------

    #[test]
    fn a_report_that_kept_a_simple_attribute_is_recognised() {
        assert!(Attr::Bold.report_keeps("0;1"));
        assert!(Attr::Bold.report_keeps("1"));
        assert!(Attr::Italic.report_keeps("0;1;3"));
        assert!(Attr::Overline.report_keeps("0;53"));
        assert!(Attr::Strikethrough.report_keeps("0;9"));
        assert!(!Attr::Bold.report_keeps("0"));
        assert!(!Attr::Bold.report_keeps(""));
        // A whole field, not a substring: `21` is not `2`, and `53` is not `5`.
        assert!(!Attr::Dim.report_keeps("0;21"));
        assert!(!Attr::Blink.report_keeps("0;53"));
        assert!(!Attr::Overline.report_keeps("0;5"));
    }

    #[test]
    fn rapid_blink_folded_into_slow_blink_is_visible_as_such() {
        // Almost every terminal treats `6` as `5`. A readback catches it, which is
        // exactly the kind of silent substitution this probe exists to expose.
        assert!(Attr::RapidBlink.report_keeps("0;6"));
        assert!(!Attr::RapidBlink.report_keeps("0;5"));
        assert!(Attr::Blink.report_keeps("0;5"));
    }

    #[test]
    fn an_underline_report_is_matched_shape_by_shape() {
        assert!(Attr::Underline(UnderlineShape::Curly).report_keeps("0;4:3"));
        assert!(Attr::Underline(UnderlineShape::Dotted).report_keeps("4:4"));
        assert!(Attr::Underline(UnderlineShape::Dashed).report_keeps("1;4:5;39"));
        // A flattened sub-parameter is a no.
        assert!(!Attr::Underline(UnderlineShape::Curly).report_keeps("0;4"));
        assert!(!Attr::Underline(UnderlineShape::Curly).report_keeps("0;4:4"));
        // A plain underline is `4`, and `4:1` says the same thing.
        assert!(Attr::Underline(UnderlineShape::Single).report_keeps("0;4"));
        assert!(Attr::Underline(UnderlineShape::Single).report_keeps("0;4:1"));
        assert!(!Attr::Underline(UnderlineShape::Single).report_keeps("0;4:3"));
    }

    #[test]
    fn a_double_underline_reported_as_ecma_48_twenty_one_still_counts() {
        // A terminal that normalises `4:2` to `21` has kept the information, even
        // though this crate would never emit `21` itself.
        assert!(Attr::Underline(UnderlineShape::Double).report_keeps("0;21"));
        assert!(Attr::Underline(UnderlineShape::Double).report_keeps("0;4:2"));
        // …and `21` means nothing for any other shape.
        assert!(!Attr::Underline(UnderlineShape::Curly).report_keeps("0;21"));
        assert!(!Attr::Underline(UnderlineShape::Single).report_keeps("0;21"));
    }

    #[test]
    fn no_underline_is_kept_when_the_report_says_nothing_about_underlining() {
        // `--underline=none` asks for "not underlined", and a report with no
        // underline parameter is a report of exactly that. Requiring an explicit
        // `24` would call every terminal broken.
        assert!(Attr::Underline(UnderlineShape::None).report_keeps("0"));
        assert!(Attr::Underline(UnderlineShape::None).report_keeps(""));
        assert!(Attr::Underline(UnderlineShape::None).report_keeps("0;24"));
        assert!(Attr::Underline(UnderlineShape::None).report_keeps("0;4:0"));
        // But a report that says it *is* underlined is a no.
        assert!(!Attr::Underline(UnderlineShape::None).report_keeps("0;4"));
        assert!(!Attr::Underline(UnderlineShape::None).report_keeps("0;4:3"));
    }

    #[test]
    fn truecolor_kept_and_truecolor_quantised_are_told_apart() {
        let attr = Attr::Color(ColorRole::Foreground, PROBE_RGB);
        assert!(attr.report_keeps("0;38;2;1;2;3"));
        assert!(attr.report_keeps("0;38:2::1:2:3"));
        // The measurement nothing else can make: accepted, and thrown away.
        assert!(!attr.report_keeps("0;38;5;16"));
        assert_eq!(
            attr.report_substitute("0;38;5;16").as_deref(),
            Some("palette entry 16")
        );
        // Dropped entirely.
        assert!(!attr.report_keeps("0"));
        assert_eq!(attr.report_substitute("0"), None);
    }

    #[test]
    fn the_probe_colour_cannot_survive_quantisation_unchanged() {
        // `#010203` is chosen so that no rounding can return it: the components are
        // small and all different. A round value would make the measurement useless.
        let Color::Rgb(red, green, blue) = PROBE_RGB else {
            panic!("the probe colour must be a 24-bit value");
        };
        assert_ne!(red, green);
        assert_ne!(green, blue);
        assert!(red < 16 && green < 16 && blue < 16);
        // And the palette probe is outside the basic sixteen, so it cannot be
        // confused with `31`.
        assert_eq!(PROBE_PALETTE.depth(), ColorDepth::Palette);
        assert!(matches!(PROBE_PALETTE, Color::Palette(index) if index >= 16));
        assert_eq!(PROBE_BASIC.depth(), ColorDepth::Basic);
    }

    #[test]
    fn a_basic_colour_reported_in_the_palette_spelling_is_not_a_change() {
        let attr = Attr::Color(ColorRole::Foreground, Color::Basic(1));
        assert!(attr.report_keeps("0;31"));
        assert!(attr.report_keeps("0;38;5;1"));
        assert!(!attr.report_keeps("0;32"));
    }

    #[test]
    fn every_attribute_recognises_the_report_of_itself_being_set() {
        // The round trip that matters: whatever an attribute emits, reading that
        // same text back must count as "kept". A mismatch here is a probe that
        // reports every terminal as broken.
        for attr in every_attribute() {
            let params = format!("0;{}", attr.params());
            assert!(
                attr.report_keeps(&params),
                "{} set `{}` and did not recognise `{params}`",
                attr.label(),
                attr.params()
            );
        }
    }

    // -- the printf form ----------------------------------------------------

    #[test]
    fn the_printf_form_is_pasteable_and_carries_no_control_bytes() {
        let rendition = Rendition::new()
            .with(Attr::Bold)
            .with(Attr::Underline(UnderlineShape::Curly));
        assert_eq!(
            rendition.printf_form("warning"),
            "printf '\\e[1;4:3mwarning\\e[22;24m\\n'"
        );

        let quoted = rendition.printf_form("it's 50% done");
        assert!(quoted.contains("it'\\''s 50%% done"), "{quoted}");

        for text in ["a\u{1b}b", "a\nb", "a\tb", "a\u{7}b"] {
            let form = rendition.printf_form(text);
            assert!(!form.contains('\u{1b}'), "{form:?}");
            assert!(!form.contains('\n'), "{form:?}");
            assert!(!form.contains('\u{7}'), "{form:?}");
        }
    }

    // -- the probe set ------------------------------------------------------

    #[test]
    fn the_whole_family_is_covered_once_each() {
        let attrs = every_attribute();
        for shape in UnderlineShape::ALL {
            assert!(attrs.contains(&Attr::Underline(*shape)), "{}", shape.name());
        }
        for simple in [
            Attr::Bold,
            Attr::Dim,
            Attr::Italic,
            Attr::Blink,
            Attr::RapidBlink,
            Attr::Reverse,
            Attr::Conceal,
            Attr::Strikethrough,
            Attr::Overline,
        ] {
            assert!(attrs.contains(&simple), "{}", simple.label());
        }
        for depth in ColorDepth::ALL {
            assert!(
                attrs.iter().any(
                    |attr| matches!(attr, Attr::Color(ColorRole::Foreground, color)
                                         if color.depth() == *depth)
                ),
                "no foreground probe at {}",
                depth.name()
            );
        }
        assert!(attrs.contains(&Attr::Color(ColorRole::Background, PROBE_RGB)));
        assert!(attrs.contains(&Attr::Color(ColorRole::Underline, PROBE_RGB)));
        assert_eq!(probe_colors().len(), ColorDepth::ALL.len());
    }
}
