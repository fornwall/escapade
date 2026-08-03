//! The three independent colours, at three depths, in two spellings.
//!
//! An SGR colour is three questions stacked on top of each other, and the answers
//! are almost independent:
//!
//! * **Which colour?** [`ColorRole`] — foreground (`SGR 30`-`37`/`38`/`39`),
//!   background (`40`-`47`/`48`/`49`) or the underline's own colour (`58`/`59`).
//! * **How deep?** [`ColorDepth`] — one of the sixteen basic colours, one of the
//!   256 palette entries, or a direct 24-bit value.
//! * **Spelled how?** [`ColorSyntax`] — `38;2;r;g;b` or `38:2::r:g:b`.
//!
//! That last one is where the real-world breakage lives, and it is worth being
//! precise about. `38;2;r;g;b` is five SGR *parameters*; `38:2::r:g:b` is one
//! parameter with five *sub-parameters*, which is the form ITU-T T.416 (ISO 8613-6)
//! actually specifies — the empty third slot is T.416's colour-space identifier,
//! which nothing uses and most terminals expect to be there. The semicolon form is
//! a misreading of that specification that became universal because xterm accepted
//! it, and it is now the safer choice for `38` and `48` precisely because everything
//! accepts it. For `58` the balance tips the other way: the colon form is what
//! terminfo's `Setulc` emits and what the terminals that implement underline colour
//! were written against.
//!
//! So [`ColorRole::default_syntax`] picks per role, deliberately, and
//! [`ColorSyntax`] exists so the demo can show a reader what the other spelling does
//! on a terminal that does not implement the parameter: nothing at all in the colon
//! form, and `SGR 2` followed by `SGR 0` — faint, then a full reset — in the
//! semicolon form, because a zero component is indistinguishable from `SGR 0`.

use anyhow::{Result, bail};

/// Which of the three independent colours an [`Color`] is being used as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ColorRole {
    /// The text colour: `SGR 30`-`37`, `90`-`97`, `38`, reset with `39`.
    Foreground,
    /// The cell background: `SGR 40`-`47`, `100`-`107`, `48`, reset with `49`.
    Background,
    /// The underline's own colour: `SGR 58`, reset with `59`. No basic form.
    Underline,
}

impl ColorRole {
    /// Every role, in SGR order.
    pub const ALL: &'static [Self] = &[Self::Foreground, Self::Background, Self::Underline];

    /// The extended-colour parameter: `38`, `48` or `58`.
    #[must_use]
    pub fn extended_param(self) -> u8 {
        match self {
            Self::Foreground => 38,
            Self::Background => 48,
            Self::Underline => 58,
        }
    }

    /// The parameter that puts this colour back to the terminal's default: `39`,
    /// `49` or `59`.
    ///
    /// These are three separate resets, and that is the point of the feature: `39`
    /// does not touch the underline colour, and `59` does not touch the text.
    #[must_use]
    pub fn default_param(self) -> u8 {
        self.extended_param() + 1
    }

    /// The base parameters for the eight basic colours and their bright forms —
    /// `(30, 90)` for the foreground, `(40, 100)` for the background.
    ///
    /// `None` for the underline colour, which has no basic form at all: `SGR 58`
    /// takes only a palette index or an RGB triple, so a basic colour asked for
    /// there is emitted as `58:5:<n>` instead. The sixteen palette entries *are*
    /// the basic colours, so nothing is lost but the spelling.
    #[must_use]
    pub fn basic_base(self) -> Option<(u8, u8)> {
        match self {
            Self::Foreground => Some((30, 90)),
            Self::Background => Some((40, 100)),
            Self::Underline => None,
        }
    }

    /// The spelling this crate emits for the role, and why:
    ///
    /// * foreground and background get **semicolons**, because `38;2;r;g;b` is what
    ///   every terminal, every library and every piece of documentation in the world
    ///   uses, and being right about T.416 is worth less than being understood;
    /// * the underline colour gets **colons**, because `58` arrived long after the
    ///   colon form was established, terminfo's `Setulc` emits colons, and the
    ///   terminals that implement it were written against that.
    #[must_use]
    pub fn default_syntax(self) -> ColorSyntax {
        match self {
            Self::Foreground | Self::Background => ColorSyntax::Semicolon,
            Self::Underline => ColorSyntax::Colon,
        }
    }

    /// The `--flag` that sets this colour.
    #[must_use]
    pub fn flag(self) -> &'static str {
        match self {
            Self::Foreground => "--color",
            Self::Background => "--background",
            Self::Underline => "--underline-color",
        }
    }

    /// Short name for reports and JSON.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Background => "background",
            Self::Underline => "underline-color",
        }
    }
}

/// How many colours a specification can express.
///
/// This is what `probe` actually asks about: the interesting question is never "does
/// this terminal do colour" but "does it do the *depth* I am about to use, or will it
/// quietly round my colour to something else".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ColorDepth {
    /// The sixteen colours every terminal has had since the 1980s.
    Basic,
    /// The 256-entry palette: `SGR 38;5;<n>`.
    Palette,
    /// 24-bit direct colour: `SGR 38;2;<r>;<g>;<b>`.
    TrueColor,
}

impl ColorDepth {
    /// Every depth, shallowest first.
    pub const ALL: &'static [Self] = &[Self::Basic, Self::Palette, Self::TrueColor];

    /// Short name for reports and JSON.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Palette => "256-color",
            Self::TrueColor => "truecolor",
        }
    }

    /// How many distinct colours it can name.
    #[must_use]
    pub fn count(self) -> u32 {
        match self {
            Self::Basic => 16,
            Self::Palette => 256,
            Self::TrueColor => 16_777_216,
        }
    }
}

/// Which separator to spell an extended colour with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorSyntax {
    /// `38:2::<r>:<g>:<b>` and `38:5:<n>` — one parameter with sub-parameters, as
    /// ITU-T T.416 specifies, empty colour-space slot included.
    Colon,
    /// `38;2;<r>;<g>;<b>` and `38;5;<n>` — separate parameters. Not what T.416 says,
    /// and what the entire world does.
    #[default]
    Semicolon,
}

impl ColorSyntax {
    /// The separator character.
    #[must_use]
    pub fn separator(self) -> char {
        match self {
            Self::Colon => ':',
            Self::Semicolon => ';',
        }
    }

    /// Display name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Colon => "colon",
            Self::Semicolon => "semicolon",
        }
    }
}

/// A colour, at whichever depth it was asked for.
///
/// The depth is part of the value rather than a separate setting, because it is
/// decided by what the user wrote: a name is basic, a number is a palette index, and
/// `#rrggbb` is 24-bit. Keeping them apart is what lets `probe` ask about the depth
/// that is actually going to be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// One of the sixteen basic colours, `0`-`15`. `0`-`7` are the originals and
    /// `8`-`15` the bright forms aixterm added as `90`-`97`.
    Basic(u8),
    /// A 256-colour palette entry: `SGR 38;5;<n>`.
    Palette(u8),
    /// A direct 24-bit value: `SGR 38;2;<r>;<g>;<b>`.
    Rgb(u8, u8, u8),
}

impl Color {
    /// The depth this colour needs the terminal to implement.
    #[must_use]
    pub fn depth(self) -> ColorDepth {
        match self {
            Self::Basic(_) => ColorDepth::Basic,
            Self::Palette(_) => ColorDepth::Palette,
            Self::Rgb(..) => ColorDepth::TrueColor,
        }
    }

    /// The SGR parameter text for this colour in `role`, without the `CSI` or `m`.
    ///
    /// A basic colour becomes `31`-style for the roles that have one, and
    /// `58:5:<n>` for the underline colour, which does not.
    #[must_use]
    pub fn params(self, role: ColorRole, syntax: ColorSyntax) -> String {
        let extended = role.extended_param();
        let separator = syntax.separator();
        match self {
            Self::Basic(index) => match role.basic_base() {
                Some((normal, bright)) => {
                    if index < 8 {
                        (normal + index).to_string()
                    } else {
                        (bright + (index - 8)).to_string()
                    }
                }
                // SGR 58 has no basic form; the first sixteen palette entries are
                // the basic colours, so this loses the spelling and nothing else.
                None => format!("{extended}{separator}5{separator}{index}"),
            },
            Self::Palette(index) => format!("{extended}{separator}5{separator}{index}"),
            Self::Rgb(red, green, blue) => match syntax {
                ColorSyntax::Colon => format!("{extended}:2::{red}:{green}:{blue}"),
                ColorSyntax::Semicolon => format!("{extended};2;{red};{green};{blue}"),
            },
        }
    }

    /// How to describe the colour to a reader.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Basic(index) => match BASIC_NAMES.get(usize::from(index)) {
                Some(name) => format!("{name} (basic {index})"),
                None => format!("basic {index}"),
            },
            Self::Palette(index) => format!("palette entry {index}"),
            Self::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
        }
    }

    /// Whether two colours name the same colour, allowing for spelling.
    ///
    /// [`Color::Basic`] and [`Color::Palette`] agree for the first sixteen entries,
    /// because they *are* the same sixteen colours — a terminal handed `31` and
    /// reporting back `38:5:1` has not changed anything, it has only normalised the
    /// spelling. Nothing else compares equal: a 24-bit value quantised to a palette
    /// entry is a different colour, and saying otherwise would throw away the most
    /// useful measurement in this module.
    #[must_use]
    pub fn same_as(self, other: Self) -> bool {
        match (self, other) {
            (Self::Basic(a) | Self::Palette(a), Self::Basic(b) | Self::Palette(b)) => {
                a == b && a < 16 || self == other
            }
            _ => self == other,
        }
    }
}

/// The eight basic colour names and their bright forms, indexed by palette entry.
const BASIC_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright-black",
    "bright-red",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-magenta",
    "bright-cyan",
    "bright-white",
];

/// Aliases accepted in addition to [`BASIC_NAMES`].
const BASIC_ALIASES: &[(&str, u8)] = &[("gray", 8), ("grey", 8)];

/// The accepted forms, quoted in every rejection message.
const COLOR_FORMS: &str = "`#rrggbb` or `#rgb` for 24-bit colour, a palette index \
                           `0`-`255` for the 256-colour form, or one of the basic names: \
                           black, red, green, yellow, blue, magenta, cyan, white, gray, \
                           and the `bright-` form of each";

/// Parses a colour, choosing the depth from the form that was written.
///
/// * `#rrggbb`, or `#rgb` expanded by doubling each digit — [`Color::Rgb`].
/// * a bare decimal `0`-`255` — [`Color::Palette`]. This is deliberate even for
///   `0`-`15`: a number selects a palette entry, so `escapade style --color 1`
///   emits `38;5;1` and asks the terminal for the 256-colour form. Write `red` to
///   get `31`.
/// * a basic name — [`Color::Basic`].
///
/// # Errors
///
/// Fails on anything else, naming every accepted form. Being strict matters more
/// here than being clever: a mistyped colour that silently became black would look
/// exactly like a terminal that does not implement the parameter, and telling those
/// two apart is the whole job of this subcommand.
pub fn parse_color(input: &str) -> Result<Color> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("the colour is empty — pass {COLOR_FORMS}");
    }

    if let Some(hex) = trimmed.strip_prefix('#') {
        return parse_hex(trimmed, hex);
    }

    let lower = trimmed.to_ascii_lowercase();

    if lower.bytes().all(|byte| byte.is_ascii_digit()) {
        // Out of range is an error rather than a clamp: `--color 300` meant
        // something, and it was not "255".
        return match lower.parse::<u32>() {
            Ok(index) if index <= 255 => Ok(Color::Palette(u8::try_from(index).unwrap_or(0))),
            _ => bail!(
                "`{trimmed}` is out of range: a palette index runs from 0 to 255. For a colour \
                 outside the palette write it as `#rrggbb`"
            ),
        };
    }

    if let Some(index) = BASIC_NAMES.iter().position(|name| *name == lower) {
        return Ok(Color::Basic(u8::try_from(index).unwrap_or(0)));
    }
    if let Some((_, index)) = BASIC_ALIASES.iter().find(|(name, _)| *name == lower) {
        return Ok(Color::Basic(*index));
    }

    bail!("`{trimmed}` is not a colour. Accepted forms are {COLOR_FORMS}");
}

/// Parses the body of a `#…` colour. `whole` is only used for error messages.
fn parse_hex(whole: &str, hex: &str) -> Result<Color> {
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(
            "`{whole}` is not a hex colour: everything after the `#` must be a hexadecimal digit. \
             Accepted forms are {COLOR_FORMS}"
        );
    }
    let digit = |index: usize| -> u8 { u8::from_str_radix(&hex[index..=index], 16).unwrap_or(0) };
    let pair = |start: usize| -> u8 { u8::from_str_radix(&hex[start..start + 2], 16).unwrap_or(0) };
    match hex.len() {
        // `#abc` is `#aabbcc`: each digit doubled, so `#fff` is white rather than a
        // very dark grey. The CSS rule, and the only expansion anybody means.
        3 => Ok(Color::Rgb(digit(0) * 17, digit(1) * 17, digit(2) * 17)),
        6 => Ok(Color::Rgb(pair(0), pair(2), pair(4))),
        other => bail!(
            "`{whole}` has {other} hex digits; a hex colour needs either 3 (`#rgb`) or 6 \
             (`#rrggbb`). Accepted forms are {COLOR_FORMS}"
        ),
    }
}

// ---------------------------------------------------------------------------
// Reading a colour back out of an SGR report
// ---------------------------------------------------------------------------

/// Recovers the colour a terminal reports for `role` from an SGR parameter list.
///
/// This is the half of the truecolor measurement that does the actual work. Given
/// what a terminal answered a DECRQSS SGR request with, it returns the colour the
/// terminal says is in effect — and comparing that against the colour that was set
/// is what distinguishes **truecolor accepted** from **truecolor silently
/// quantised**. A terminal that rounded `38;2;1;2;3` to the nearest palette entry
/// reports `38;5;16`, and this returns [`Color::Palette`] where [`Color::Rgb`] was
/// asked for.
///
/// Every spelling a terminal might answer in is accepted, because the point is to
/// read what it said rather than to insist on a form:
///
/// * `31` and `91` — basic and bright basic.
/// * `38;5;<n>` and `38:5:<n>` — palette.
/// * `38;2;<r>;<g>;<b>`, `38:2::<r>:<g>:<b>` and `38:2:<r>:<g>:<b>` — direct colour,
///   with or without T.416's empty colour-space slot.
/// * `39` — back to the default, which is reported as `None`.
///
/// Later fields win, exactly as they would when rendering, so a report that sets a
/// colour and then resets it comes back as `None`.
#[must_use]
pub fn parse_reported_color(params: &str, role: ColorRole) -> Option<Color> {
    let extended = role.extended_param();
    let default = role.default_param();
    let fields: Vec<&str> = params.split(';').collect();
    let mut found = None;
    let mut index = 0;

    while index < fields.len() {
        let field = fields[index];
        let mut subs = field.split(':');
        let head = subs.next().unwrap_or_default();
        let head_number = head.parse::<u16>().ok();

        if head_number == Some(u16::from(default)) {
            found = None;
            index += 1;
            continue;
        }

        if let Some(number) = head_number
            && let Some(basic) = basic_from_param(number, role)
        {
            found = Some(Color::Basic(basic));
            index += 1;
            continue;
        }

        if head_number != Some(u16::from(extended)) {
            index += 1;
            continue;
        }

        // An extended colour. Either the rest of this field carries it as
        // sub-parameters, or the following fields carry it as parameters.
        let rest: Vec<&str> = subs.collect();
        if rest.is_empty() {
            let consumed = read_semicolon_color(&fields, index + 1, &mut found);
            index += 1 + consumed;
        } else {
            read_colon_color(&rest, &mut found);
            index += 1;
        }
    }

    found
}

/// Maps a basic-colour parameter onto a palette index, if it is one for `role`.
fn basic_from_param(number: u16, role: ColorRole) -> Option<u8> {
    let (normal, bright) = role.basic_base()?;
    let normal = u16::from(normal);
    let bright = u16::from(bright);
    if (normal..normal + 8).contains(&number) {
        return u8::try_from(number - normal).ok();
    }
    if (bright..bright + 8).contains(&number) {
        return u8::try_from(number - bright + 8).ok();
    }
    None
}

/// Reads the sub-parameters of a colon-form extended colour into `found`.
fn read_colon_color(rest: &[&str], found: &mut Option<Color>) {
    match rest.first().and_then(|text| text.parse::<u8>().ok()) {
        Some(5) => {
            if let Some(index) = rest.get(1).and_then(|text| text.parse::<u8>().ok()) {
                *found = Some(Color::Palette(index));
            }
        }
        Some(2) => {
            // T.416 puts an empty colour-space identifier after the `2`, and some
            // terminals leave it out. Accept either by taking the last three
            // numeric sub-parameters.
            let numbers: Vec<u8> = rest[1..]
                .iter()
                .filter_map(|text| text.parse::<u8>().ok())
                .collect();
            if let [red, green, blue] = numbers[..] {
                *found = Some(Color::Rgb(red, green, blue));
            }
        }
        _ => {}
    }
}

/// Reads a semicolon-form extended colour starting at `start`, returning how many
/// fields it consumed beyond the `38`/`48`/`58` itself.
fn read_semicolon_color(fields: &[&str], start: usize, found: &mut Option<Color>) -> usize {
    match fields.get(start).and_then(|text| text.parse::<u8>().ok()) {
        Some(5) => {
            if let Some(index) = fields
                .get(start + 1)
                .and_then(|text| text.parse::<u8>().ok())
            {
                *found = Some(Color::Palette(index));
                return 2;
            }
            1
        }
        Some(2) => {
            let components: Vec<u8> = (0..3)
                .filter_map(|offset| {
                    fields
                        .get(start + 1 + offset)
                        .and_then(|text| text.parse::<u8>().ok())
                })
                .collect();
            if let [red, green, blue] = components[..] {
                *found = Some(Color::Rgb(red, green, blue));
                return 4;
            }
            1
        }
        _ => 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // -- roles --------------------------------------------------------------

    #[test]
    fn every_role_has_its_own_extended_parameter_and_its_own_reset() {
        assert_eq!(ColorRole::Foreground.extended_param(), 38);
        assert_eq!(ColorRole::Background.extended_param(), 48);
        assert_eq!(ColorRole::Underline.extended_param(), 58);
        assert_eq!(ColorRole::Foreground.default_param(), 39);
        assert_eq!(ColorRole::Background.default_param(), 49);
        assert_eq!(ColorRole::Underline.default_param(), 59);

        // Three separate resets is the whole point: `39` must not be able to reset
        // the underline colour, or a linter could not leave the text alone.
        let resets: Vec<u8> = ColorRole::ALL.iter().map(|r| r.default_param()).collect();
        assert_eq!(resets, vec![39, 49, 59]);
        assert_eq!(resets.len(), 3);
    }

    #[test]
    fn only_the_underline_colour_lacks_a_basic_form() {
        assert_eq!(ColorRole::Foreground.basic_base(), Some((30, 90)));
        assert_eq!(ColorRole::Background.basic_base(), Some((40, 100)));
        assert_eq!(ColorRole::Underline.basic_base(), None);
    }

    #[test]
    fn the_default_spelling_is_chosen_per_role_on_purpose() {
        // Semicolons for 38/48 because that is what the world accepts; colons for
        // 58 because that is what the terminals implementing it were written for.
        assert_eq!(
            ColorRole::Foreground.default_syntax(),
            ColorSyntax::Semicolon
        );
        assert_eq!(
            ColorRole::Background.default_syntax(),
            ColorSyntax::Semicolon
        );
        assert_eq!(ColorRole::Underline.default_syntax(), ColorSyntax::Colon);
    }

    // -- building -----------------------------------------------------------

    #[test]
    fn basic_colours_use_the_short_parameters() {
        assert_eq!(
            Color::Basic(1).params(ColorRole::Foreground, ColorSyntax::Semicolon),
            "31"
        );
        assert_eq!(
            Color::Basic(1).params(ColorRole::Background, ColorSyntax::Semicolon),
            "41"
        );
        // The bright eight are aixterm's 90-97 and 100-107, not `1;31`.
        assert_eq!(
            Color::Basic(9).params(ColorRole::Foreground, ColorSyntax::Semicolon),
            "91"
        );
        assert_eq!(
            Color::Basic(15).params(ColorRole::Background, ColorSyntax::Semicolon),
            "107"
        );
    }

    #[test]
    fn a_basic_underline_colour_falls_back_to_the_palette_form() {
        // There is no `58`-equivalent of `31`, and the first sixteen palette
        // entries are those colours, so nothing is lost but the spelling.
        assert_eq!(
            Color::Basic(1).params(ColorRole::Underline, ColorSyntax::Colon),
            "58:5:1"
        );
        assert_eq!(
            Color::Basic(9).params(ColorRole::Underline, ColorSyntax::Colon),
            "58:5:9"
        );
    }

    #[test]
    fn palette_colours_are_the_same_shape_in_every_role() {
        assert_eq!(
            Color::Palette(202).params(ColorRole::Foreground, ColorSyntax::Semicolon),
            "38;5;202"
        );
        assert_eq!(
            Color::Palette(202).params(ColorRole::Background, ColorSyntax::Semicolon),
            "48;5;202"
        );
        assert_eq!(
            Color::Palette(202).params(ColorRole::Underline, ColorSyntax::Colon),
            "58:5:202"
        );
    }

    #[test]
    fn truecolor_is_spelled_the_way_the_role_asks_for() {
        assert_eq!(
            Color::Rgb(1, 2, 3).params(ColorRole::Foreground, ColorSyntax::Semicolon),
            "38;2;1;2;3"
        );
        assert_eq!(
            Color::Rgb(1, 2, 3).params(ColorRole::Underline, ColorSyntax::Colon),
            "58:2::1:2:3"
        );
    }

    #[test]
    fn the_colon_form_keeps_the_empty_colour_space_slot() {
        // `38:2::1:2:3` — two colons after the `2`. Sub-parameter 2 is T.416's
        // colour-space identifier; dropping it shifts every component down one.
        let params = Color::Rgb(1, 2, 3).params(ColorRole::Foreground, ColorSyntax::Colon);
        assert_eq!(params, "38:2::1:2:3");
        assert!(params.contains("2::"));
        assert_eq!(params.split(':').nth(2), Some(""));
        assert_eq!(params.split(':').count(), 6);
    }

    #[test]
    fn the_semicolon_form_contains_parameters_readable_as_sgr_zero() {
        // The trap: on a terminal that does not implement the parameter, a zero
        // component is `SGR 0` and resets every attribute on the line.
        let params = Color::Rgb(1, 0, 0).params(ColorRole::Foreground, ColorSyntax::Semicolon);
        assert!(params.split(';').any(|field| field == "0"));
        // The colon form cannot be misread that way: it is one parameter.
        let colon = Color::Rgb(1, 0, 0).params(ColorRole::Foreground, ColorSyntax::Colon);
        assert!(!colon.contains(';'));

        assert_eq!(ColorSyntax::default(), ColorSyntax::Semicolon);
        assert_eq!(ColorSyntax::Colon.separator(), ':');
        assert_eq!(ColorSyntax::Semicolon.separator(), ';');
        assert_eq!(ColorSyntax::Colon.name(), "colon");
        assert_eq!(ColorSyntax::Semicolon.name(), "semicolon");
    }

    // -- depth --------------------------------------------------------------

    #[test]
    fn the_form_written_decides_the_depth_probed() {
        assert_eq!(Color::Basic(1).depth(), ColorDepth::Basic);
        assert_eq!(Color::Palette(1).depth(), ColorDepth::Palette);
        assert_eq!(Color::Rgb(1, 2, 3).depth(), ColorDepth::TrueColor);

        assert_eq!(ColorDepth::Basic.count(), 16);
        assert_eq!(ColorDepth::Palette.count(), 256);
        assert_eq!(ColorDepth::TrueColor.count(), 16_777_216);
        let names: Vec<&str> = ColorDepth::ALL.iter().map(|d| d.name()).collect();
        assert_eq!(names, vec!["basic", "256-color", "truecolor"]);
    }

    // -- parsing ------------------------------------------------------------

    #[test]
    fn hex_colours_parse_at_both_lengths_and_either_case() {
        assert_eq!(parse_color("#ff0000").unwrap(), Color::Rgb(255, 0, 0));
        assert_eq!(parse_color("#FF8800").unwrap(), Color::Rgb(255, 136, 0));
        assert_eq!(parse_color("  #5f87ff ").unwrap(), Color::Rgb(95, 135, 255));
        // `#abc` doubles each digit, so `#fff` is white and not a dark grey.
        assert_eq!(parse_color("#fff").unwrap(), Color::Rgb(255, 255, 255));
        assert_eq!(parse_color("#f00").unwrap(), Color::Rgb(255, 0, 0));
        assert_eq!(parse_color("#123").unwrap(), Color::Rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn a_bare_number_is_always_the_palette_form_even_below_sixteen() {
        // `--color 1` asks for `38;5;1`, which needs the 256-colour form; `--color
        // red` asks for `31`, which does not. The distinction is the point.
        assert_eq!(parse_color("1").unwrap(), Color::Palette(1));
        assert_eq!(parse_color("1").unwrap().depth(), ColorDepth::Palette);
        assert_eq!(parse_color("red").unwrap(), Color::Basic(1));
        assert_eq!(parse_color("red").unwrap().depth(), ColorDepth::Basic);
        assert_eq!(parse_color("0").unwrap(), Color::Palette(0));
        assert_eq!(parse_color("255").unwrap(), Color::Palette(255));
    }

    #[test]
    fn basic_names_and_their_bright_forms_parse() {
        for (index, name) in BASIC_NAMES.iter().enumerate() {
            assert_eq!(
                parse_color(name).unwrap(),
                Color::Basic(u8::try_from(index).unwrap()),
                "{name}"
            );
        }
        assert_eq!(parse_color("RED").unwrap(), Color::Basic(1));
        assert_eq!(parse_color("Bright-Cyan").unwrap(), Color::Basic(14));
        assert_eq!(parse_color("gray").unwrap(), parse_color("grey").unwrap());
        assert_eq!(parse_color("gray").unwrap(), Color::Basic(8));
    }

    #[test]
    fn malformed_colours_are_refused_with_the_accepted_forms() {
        for input in [
            "",
            "   ",
            "#",
            "#ff",
            "#ffff",
            "#ff000",
            "#fff0000",
            "#gggggg",
            "reddish",
            "rgb(1,2,3)",
            "0xff0000",
            "-1",
            "1.5",
            "ff0000",
            "256",
            "1000",
        ] {
            let error = parse_color(input).unwrap_err().to_string();
            assert!(
                error.contains("#rrggbb") || error.contains("0 to 255"),
                "{input:?} gave: {error}"
            );
        }
    }

    #[test]
    fn a_hex_colour_of_the_wrong_length_says_which_lengths_are_allowed() {
        let error = parse_color("#ff00").unwrap_err().to_string();
        assert!(error.contains("either 3"), "{error}");
        assert!(error.contains('4'), "{error}");
    }

    #[test]
    fn colours_describe_themselves_without_control_bytes() {
        assert_eq!(Color::Basic(1).describe(), "red (basic 1)");
        assert_eq!(Color::Basic(9).describe(), "bright-red (basic 9)");
        assert_eq!(Color::Palette(202).describe(), "palette entry 202");
        assert_eq!(Color::Rgb(255, 136, 0).describe(), "#ff8800");
        for color in [Color::Basic(0), Color::Palette(1), Color::Rgb(1, 2, 3)] {
            assert!(!color.describe().contains('\u{1b}'));
        }
    }

    // -- reading a report back ---------------------------------------------

    #[test]
    fn a_truecolor_report_is_read_back_exactly() {
        for params in [
            "0;38;2;1;2;3",
            "38;2;1;2;3",
            "0;38:2::1:2:3",
            // Some terminals leave out T.416's empty colour-space slot.
            "0;38:2:1:2:3",
            "1;3;38;2;1;2;3;48;5;4",
        ] {
            assert_eq!(
                parse_reported_color(params, ColorRole::Foreground),
                Some(Color::Rgb(1, 2, 3)),
                "{params}"
            );
        }
    }

    #[test]
    fn a_quantised_truecolor_report_is_visibly_not_what_was_asked_for() {
        // The measurement that no other method can make: the terminal accepted
        // `38;2;1;2;3` and reported a palette entry, so it rounded the colour.
        let reported = parse_reported_color("0;38;5;16", ColorRole::Foreground);
        assert_eq!(reported, Some(Color::Palette(16)));
        assert!(!Color::Rgb(1, 2, 3).same_as(reported.unwrap()));
        // …and a terminal that dropped it entirely reports nothing.
        assert_eq!(parse_reported_color("0;1", ColorRole::Foreground), None);
    }

    #[test]
    fn basic_colours_are_read_back_from_either_range() {
        assert_eq!(
            parse_reported_color("0;31", ColorRole::Foreground),
            Some(Color::Basic(1))
        );
        assert_eq!(
            parse_reported_color("0;91", ColorRole::Foreground),
            Some(Color::Basic(9))
        );
        assert_eq!(
            parse_reported_color("0;41", ColorRole::Background),
            Some(Color::Basic(1))
        );
        assert_eq!(
            parse_reported_color("0;107", ColorRole::Background),
            Some(Color::Basic(15))
        );
        // A foreground parameter is not a background one.
        assert_eq!(parse_reported_color("0;31", ColorRole::Background), None);
        assert_eq!(parse_reported_color("0;41", ColorRole::Foreground), None);
    }

    #[test]
    fn the_underline_colour_is_read_back_in_either_spelling() {
        assert_eq!(
            parse_reported_color("0;4:3;58:2::255:0:0", ColorRole::Underline),
            Some(Color::Rgb(255, 0, 0))
        );
        assert_eq!(
            parse_reported_color("0;4;58;2;255;0;0", ColorRole::Underline),
            Some(Color::Rgb(255, 0, 0))
        );
        assert_eq!(
            parse_reported_color("58:5:9", ColorRole::Underline),
            Some(Color::Palette(9))
        );
        // SGR 58 has no basic form, so nothing in 30-37 is an underline colour.
        assert_eq!(parse_reported_color("0;31", ColorRole::Underline), None);
    }

    #[test]
    fn the_three_roles_do_not_read_each_other_s_colours() {
        let params = "38;2;1;2;3;48;5;4;58:2::9:9:9";
        assert_eq!(
            parse_reported_color(params, ColorRole::Foreground),
            Some(Color::Rgb(1, 2, 3))
        );
        assert_eq!(
            parse_reported_color(params, ColorRole::Background),
            Some(Color::Palette(4))
        );
        assert_eq!(
            parse_reported_color(params, ColorRole::Underline),
            Some(Color::Rgb(9, 9, 9))
        );
    }

    #[test]
    fn a_default_parameter_clears_whatever_was_found() {
        assert_eq!(
            parse_reported_color("38;2;1;2;3;39", ColorRole::Foreground),
            None
        );
        assert_eq!(
            parse_reported_color("48;5;4;49", ColorRole::Background),
            None
        );
        assert_eq!(
            parse_reported_color("58:5:9;59", ColorRole::Underline),
            None
        );
        // …and a later colour beats an earlier one, as it would when rendering.
        assert_eq!(
            parse_reported_color("31;38;2;1;2;3", ColorRole::Foreground),
            Some(Color::Rgb(1, 2, 3))
        );
        assert_eq!(
            parse_reported_color("38;2;1;2;3;31", ColorRole::Foreground),
            Some(Color::Basic(1))
        );
    }

    #[test]
    fn nonsense_reports_do_not_panic_or_invent_a_colour() {
        for params in [
            "",
            ";",
            ";;;;",
            "38",
            "38;",
            "38;2",
            "38;2;1",
            "38;2;1;2",
            "38;5",
            "38:2",
            "38:",
            "38:9:9:9",
            "999999999",
            "abc",
            "38;abc;1;2;3",
            "-1",
        ] {
            // Only assertion that matters: it returns rather than panicking, and it
            // does not claim a colour it was not told about.
            let found = parse_reported_color(params, ColorRole::Foreground);
            assert!(
                found.is_none() || matches!(found, Some(Color::Rgb(..) | Color::Palette(_))),
                "{params:?} gave {found:?}"
            );
        }
        assert_eq!(parse_reported_color("38;2;1", ColorRole::Foreground), None);
    }

    #[test]
    fn spelling_normalisation_is_not_treated_as_a_change() {
        // A terminal handed `31` may report `38:5:1`; that is the same colour.
        assert!(Color::Basic(1).same_as(Color::Palette(1)));
        assert!(Color::Palette(15).same_as(Color::Basic(15)));
        assert!(Color::Basic(1).same_as(Color::Basic(1)));
        assert!(Color::Rgb(1, 2, 3).same_as(Color::Rgb(1, 2, 3)));
        // But a quantised colour is a different colour, and must stay visible.
        assert!(!Color::Rgb(1, 2, 3).same_as(Color::Palette(16)));
        assert!(!Color::Palette(16).same_as(Color::Rgb(1, 2, 3)));
        assert!(!Color::Basic(1).same_as(Color::Palette(2)));
        assert!(!Color::Palette(200).same_as(Color::Basic(1)));
    }
}
