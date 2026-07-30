//! Source three: what the terminal actually does.
//!
//! Every probe here is split in two — a function that builds the bytes to send,
//! and a pure function that interprets the replies. The replies arrive as a
//! [`Round`], so every interpretation is testable without a terminal, which is
//! the only way any of this can be trusted.

use std::{
    fmt::Write as _,
    process::{Command, Stdio},
};

use crate::{
    features::doctor::{identity, session::Round},
    input::Reply,
    term::{CSI, DCS, OSC, ST},
};

/// The RGB triple the SGR readback sets.
///
/// Near-black on purpose: it quantises to a different value in the 256-colour
/// cube than it is in truecolor, so a terminal that silently downgraded is caught
/// rather than accidentally agreeing. Nothing is printed with it, so it never
/// appears on screen.
pub const PROBE_RGB: (u8, u8, u8) = (1, 2, 3);

// ---------------------------------------------------------------------------
// Truecolor: DECRQSS readback of the graphic rendition
// ---------------------------------------------------------------------------

/// Sets an RGB foreground and asks for the graphic rendition back.
///
/// ```text
/// →  CSI 0 m                    start from a known state
/// →  CSI 38 ; 2 ; 1 ; 2 ; 3 m   an RGB foreground nothing would pick by accident
/// →  DCS $ q m ST               DECRQSS: what is the graphic rendition?
/// ←  DCS 1 $ r <params> m ST    DECRPSS
/// →  CSI 0 m                    put it back
/// ```
///
/// The final `CSI 0 m` is not optional: leaving a foreground colour set would
/// tint whatever the shell printed next.
#[must_use]
pub fn sgr_readback_query() -> String {
    let (red, green, blue) = PROBE_RGB;
    format!("{CSI}0m{CSI}38;2;{red};{green};{blue}m{DCS}$qm{ST}{CSI}0m")
}

/// What the terminal reported the foreground colour as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readback {
    /// A direct-colour triple. Equal to [`PROBE_RGB`] means the 24 bits survived.
    Direct(u8, u8, u8),
    /// An indexed colour: the terminal quantised.
    Indexed(u32),
    /// The reply was a valid DECRPSS but mentioned no foreground colour at all,
    /// so the terminal dropped the attribute rather than downgrading it.
    NoColor,
}

/// The whole result of the SGR readback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SgrReadback {
    /// The DECRPSS parameter text, when a reply came back.
    pub raw: Option<String>,
    /// The reply's leading status flag, kept only for reporting.
    ///
    /// **Nothing branches on this.** xterm's `ctlseqs` records that DEC's own
    /// manuals had the DECRQSS valid/invalid flag reversed, and that testing a
    /// real VT420 in 1996 showed the documented codes were the wrong way round —
    /// so the canonical reference says the reference is wrong about exactly this
    /// field. The verdict comes from the *content* of the reply instead.
    pub advisory_flag: Option<u32>,
    /// What the reply said the colour was.
    pub colour: Option<Readback>,
}

impl SgrReadback {
    /// Whether the terminal reported back the triple that was set.
    #[must_use]
    pub fn kept_the_triple(&self) -> bool {
        let (red, green, blue) = PROBE_RGB;
        self.colour == Some(Readback::Direct(red, green, blue))
    }
}

/// Interprets the replies to [`sgr_readback_query`].
#[must_use]
pub fn interpret_sgr(round: &Round) -> SgrReadback {
    let reply = round.replies.iter().find_map(|reply| match reply {
        Reply::Dcs(payload) => parse_decrpss(payload),
        _ => None,
    });
    match reply {
        None => SgrReadback {
            raw: None,
            advisory_flag: None,
            colour: None,
        },
        Some((flag, params)) => SgrReadback {
            colour: Some(parse_foreground(&params)),
            raw: Some(params),
            advisory_flag: flag,
        },
    }
}

/// Parses a DECRPSS reply for SGR: `<Ps> $ r <params> m`.
///
/// Returns the advisory status flag and the parameter text with the trailing `m`
/// removed. `None` for a DCS payload that is not a DECRPSS reply about SGR —
/// XTGETTCAP and XTVERSION answer on the same stream.
#[must_use]
pub fn parse_decrpss(payload: &str) -> Option<(Option<u32>, String)> {
    let at = payload.find("$r")?;
    let flag = payload[..at].trim();
    let flag = if flag.is_empty() {
        None
    } else {
        Some(flag.parse::<u32>().ok()?)
    };
    // The setting is echoed back with the same final byte it is set with, so an
    // SGR report ends in `m`. Anything else is a report about another setting.
    let body = payload[at + 2..].strip_suffix('m')?;
    Some((flag, body.to_owned()))
}

/// Finds the foreground colour in an SGR parameter string.
///
/// Both spellings are accepted, because both are in the field: the legacy
/// semicolon form `38;2;r;g;b`, and ITU-T T.416's colon form `38:2::r:g:b`, whose
/// empty third element is a colour-space id nothing uses. Terminals echo back
/// whichever they prefer regardless of which one was sent.
#[must_use]
pub fn parse_foreground(params: &str) -> Readback {
    let fields: Vec<&str> = params.split(';').collect();
    for (index, field) in fields.iter().enumerate() {
        let parts: Vec<&str> = field.split(':').collect();
        if parts[0].trim() != "38" {
            continue;
        }
        if parts.len() > 1 {
            // Colon form: everything is inside this one field.
            let numbers: Vec<u32> = parts[2..]
                .iter()
                .filter_map(|part| part.trim().parse::<u32>().ok())
                .collect();
            return match parts[1].trim() {
                "2" => triple(&numbers),
                "5" => numbers.first().map_or(Readback::NoColor, |&index| {
                    Readback::Indexed(index)
                }),
                _ => Readback::NoColor,
            };
        }
        // Semicolon form: the selector and the components are separate fields.
        let rest: Vec<u32> = fields[index + 1..]
            .iter()
            .filter_map(|field| field.trim().parse::<u32>().ok())
            .collect();
        return match rest.split_first() {
            Some((2, components)) => triple(components),
            Some((5, [index, ..])) => Readback::Indexed(*index),
            _ => Readback::NoColor,
        };
    }
    Readback::NoColor
}

/// The first three numbers as a direct-colour triple.
fn triple(numbers: &[u32]) -> Readback {
    match numbers {
        [red, green, blue, ..] => Readback::Direct(
            u8::try_from(*red).unwrap_or(u8::MAX),
            u8::try_from(*green).unwrap_or(u8::MAX),
            u8::try_from(*blue).unwrap_or(u8::MAX),
        ),
        _ => Readback::NoColor,
    }
}

// ---------------------------------------------------------------------------
// Cell size: a ladder, because no single query is portable
// ---------------------------------------------------------------------------

/// Which query produced a cell size.
///
/// Reporting the rung is not bookkeeping: it identifies the terminal family as
/// surely as any version string, because which of these three a terminal answers
/// is a stable property of its lineage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// `CSI 16 t`, the direct question. Skipped by Alacritty, VTE and iTerm2.
    Csi16t,
    /// iTerm2's own `OSC 1337 ; ReportCellSize ST`.
    ITerm2,
    /// `CSI 14 t` (text area in pixels) divided by `CSI 18 t` (text area in
    /// characters). Works anywhere both are answered, at the cost of rounding.
    Divided,
}

impl Rung {
    /// How the measurement was obtained, for the report.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Csi16t => "CSI 16 t",
            Self::ITerm2 => "OSC 1337 ReportCellSize",
            Self::Divided => "CSI 14 t / CSI 18 t",
        }
    }
}

/// A measured cell size in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellSize {
    /// Cell width in pixels.
    pub width: f64,
    /// Cell height in pixels.
    pub height: f64,
    /// Which rung of the ladder answered.
    pub rung: Rung,
}

impl CellSize {
    /// `8x16`, rounded, for a table cell.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{:.0}x{:.0}", self.width, self.height)
    }
}

/// Asks for the cell size every way there is, in one round.
///
/// ```text
/// →  CSI 16 t                        cell size in pixels
/// →  OSC 1337 ; ReportCellSize ST    iTerm2's version of the same question
/// →  CSI 14 t                        text area in pixels
/// →  CSI 18 t                        text area in characters
/// ```
///
/// All four go out together because they are all harmless and a round trip is the
/// expensive part. The unanswered ones cost nothing but the bytes.
#[must_use]
pub fn cell_size_query() -> String {
    format!("{CSI}16t{OSC}1337;ReportCellSize{ST}{CSI}14t{CSI}18t")
}

/// Interprets the replies to [`cell_size_query`], preferring the most direct rung.
#[must_use]
pub fn interpret_cell_size(round: &Round) -> Option<CellSize> {
    let mut from_16t = None;
    let mut from_iterm2 = None;
    let mut area_pixels = None;
    let mut area_cells = None;

    for reply in &round.replies {
        match reply {
            Reply::Csi { params, final_byte: 't' } => {
                // `CSI 6 ; height ; width t`, `CSI 4 ; height ; width t`,
                // `CSI 8 ; height ; width t` — height always precedes width.
                let mut fields = params.split(';').map(str::trim);
                let kind = fields.next().unwrap_or_default();
                let height = fields.next().and_then(|text| text.parse::<f64>().ok());
                let width = fields.next().and_then(|text| text.parse::<f64>().ok());
                if let (Some(height), Some(width)) = (height, width) {
                    match kind {
                        "6" => from_16t = Some((width, height)),
                        "4" => area_pixels = Some((width, height)),
                        "8" => area_cells = Some((width, height)),
                        _ => {}
                    }
                }
            }
            Reply::Osc(payload) => {
                if let Some(size) = parse_iterm2_cell_size(payload) {
                    from_iterm2 = Some(size);
                }
            }
            _ => {}
        }
    }

    if let Some((width, height)) = from_16t.filter(|&(w, h)| w > 0.0 && h > 0.0) {
        return Some(CellSize { width, height, rung: Rung::Csi16t });
    }
    if let Some((width, height)) = from_iterm2.filter(|&(w, h)| w > 0.0 && h > 0.0) {
        return Some(CellSize { width, height, rung: Rung::ITerm2 });
    }
    // Dividing is last because it rounds: a 1918-pixel text area 80 cells wide
    // gives 23.975, and the true cell width is 24 with two columns of padding.
    if let (Some((pixel_width, pixel_height)), Some((cells_wide, cells_high))) =
        (area_pixels, area_cells)
        && cells_wide > 0.0
        && cells_high > 0.0
    {
        return Some(CellSize {
            width: pixel_width / cells_wide,
            height: pixel_height / cells_high,
            rung: Rung::Divided,
        });
    }
    None
}

/// Parses iTerm2's answer: `OSC 1337 ; ReportCellSize=<height>;<width>[;<scale>] ST`.
///
/// Height comes first, matching `CSI 16 t`, and the values are decimal rather than
/// integer. The optional third field is the display's scale factor, which iTerm2
/// added later; it is ignored, because the pixels an application draws into are
/// the ones reported.
///
/// Returned as `(width, height)` so it matches the rest of this module.
#[must_use]
pub fn parse_iterm2_cell_size(payload: &str) -> Option<(f64, f64)> {
    let body = payload.strip_prefix("1337;")?;
    let body = body.strip_prefix("ReportCellSize=")?;
    let mut fields = body.split(';').map(str::trim);
    let height = fields.next()?.parse::<f64>().ok()?;
    let width = fields.next()?.parse::<f64>().ok()?;
    Some((width, height))
}

// ---------------------------------------------------------------------------
// Identity: XTVERSION, backed by device attributes
// ---------------------------------------------------------------------------

/// Asks the terminal to name itself.
///
/// ```text
/// →  CSI > 0 q     XTVERSION
/// ←  DCS > | text ST
/// →  CSI > c       Secondary Device Attributes
/// ←  CSI > Pp ; Pv ; Pc c
/// ```
///
/// Primary Device Attributes is not sent here: it is the barrier every round ends
/// with, so its answer is collected anyway.
#[must_use]
pub fn identity_query() -> String {
    format!("{CSI}>0q{CSI}>c")
}

/// What the terminal said about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// The XTVERSION text, introducer stripped.
    pub xtversion: Option<String>,
    /// Primary Device Attributes parameters, e.g. `?62;4`.
    pub da1: Option<String>,
    /// Secondary Device Attributes parameters, e.g. `>41;356;0`.
    pub da2: Option<String>,
}

/// Interprets the replies to [`identity_query`] plus the round's barrier.
#[must_use]
pub fn interpret_identity(round: &Round) -> Identity {
    let mut identity = Identity::default();
    for reply in &round.replies {
        match reply {
            Reply::Dcs(payload) => {
                // Only XTVERSION answers DCS with a `>|` introducer; a DECRPSS or
                // XTGETTCAP reply arriving late must not be read as a version.
                if payload.starts_with(">|")
                    && let Some(text) = identity::strip_xtversion(payload)
                {
                    identity.xtversion = Some(text.to_owned());
                }
            }
            Reply::Csi { params, final_byte: 'c' } => {
                if params.starts_with('>') {
                    identity.da2 = Some(params.clone());
                } else {
                    identity.da1 = Some(params.clone());
                }
            }
            _ => {}
        }
    }
    identity
}

/// The family the terminal named itself as, preferring XTVERSION.
///
/// XTVERSION is the only string of the four the terminal composes itself, so
/// where it disagrees with `TERM` it is the one to believe.
#[must_use]
pub fn observed_family(identity: &Identity) -> Option<&'static str> {
    identity
        .xtversion
        .as_deref()
        .and_then(identity::family)
}

/// A one-line summary of everything the identity round heard.
#[must_use]
pub fn identity_detail(identity: &Identity) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(version) = &identity.xtversion {
        parts.push(format!("XTVERSION {version}"));
    }
    if let Some(da1) = &identity.da1 {
        parts.push(format!("DA1 {da1}"));
    }
    if let Some(da2) = &identity.da2 {
        parts.push(format!("DA2 {da2}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

// ---------------------------------------------------------------------------
// Backspace: the tty's own erase character
// ---------------------------------------------------------------------------

/// What `stty -a` said about the erase character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Erase {
    /// The erase character, canonically spelled `^?` or `^H`.
    Found {
        /// The canonical spelling, for comparison against `kbs`.
        canonical: String,
        /// Exactly what `stty` printed.
        raw: String,
    },
    /// `stty` printed an erase character in a spelling nothing here recognises.
    Unrecognised(String),
    /// `stty` ran but said nothing about `erase`.
    NotReported,
    /// `stty` is not installed, or there is no controlling terminal to ask about.
    Unavailable(String),
}

/// Reads the terminal driver's erase character with `stty -a`.
///
/// This has to be read **before** raw mode is entered. Raw mode does not clear
/// `VERASE`, but a tool that reports the line discipline's configuration should
/// report the one the user has, not the one it installed itself.
///
/// `stty` needs the terminal on its standard input, so `/dev/tty` is opened for
/// it explicitly: with a redirected stdin it would report about a pipe, or refuse.
#[must_use]
pub fn stty_erase() -> Erase {
    let tty = match std::fs::File::open("/dev/tty") {
        Ok(file) => file,
        Err(error) => {
            return Erase::Unavailable(format!("/dev/tty could not be opened: {error}"));
        }
    };
    let output = Command::new("stty")
        .arg("-a")
        .stdin(Stdio::from(tty))
        .stderr(Stdio::null())
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(_) => return Erase::Unavailable("`stty -a` failed".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Erase::Unavailable("`stty` is not installed".to_owned());
        }
        Err(error) => return Erase::Unavailable(error.to_string()),
    };
    parse_stty_erase(&String::from_utf8_lossy(&output.stdout))
}

/// Picks `erase = ^?` out of `stty -a` output.
///
/// The control-character block is one long list separated by `;`, so the value
/// ends at the next semicolon or at the end of the line.
#[must_use]
pub fn parse_stty_erase(text: &str) -> Erase {
    let Some(at) = text.find("erase = ").map(|at| at + "erase = ".len()) else {
        return Erase::NotReported;
    };
    let rest = &text[at..];
    let end = rest
        .find([';', '\n'])
        .unwrap_or(rest.len());
    let raw = rest[..end].trim().to_owned();
    match canonical_erase(&raw) {
        Some(canonical) => Erase::Found {
            canonical: canonical.to_owned(),
            raw,
        },
        None => Erase::Unrecognised(raw),
    }
}

/// Canonicalises the many spellings of the two bytes that erase.
///
/// `kbs` and `stty` spell the same byte differently, and neither is consistent
/// across systems: `^?`, `\177`, `\0177` and `0x7f` are all DEL, and `^H`,
/// `\010`, `\b` and `0x08` are all BS. Comparing the spellings rather than the
/// bytes would report a disagreement between two sources that agree.
#[must_use]
pub fn canonical_erase(text: &str) -> Option<&'static str> {
    let text = text.trim();
    match text {
        "^?" | "\\177" | "\\0177" | "\\x7f" | "\\x7F" | "0x7f" | "0x7F" | "127" | "DEL" | "del"
        | "\u{7f}" => Some("^?"),
        "^H" | "^h" | "\\010" | "\\b" | "\\x08" | "0x08" | "8" | "BS" | "bs" | "\u{8}" => {
            Some("^H")
        }
        _ => None,
    }
}

/// A one-line description of what the erase character means for the reader.
#[must_use]
pub fn erase_explanation(canonical: &str) -> &'static str {
    match canonical {
        "^?" => "DEL (0x7f), which is what almost every terminal sends",
        "^H" => "BS (0x08), the older convention",
        _ => "a byte neither convention uses",
    }
}

// ---------------------------------------------------------------------------
// Making replies safe to print
// ---------------------------------------------------------------------------

/// Renders arbitrary text so it can be printed without being executed.
///
/// Terminal replies routinely contain real control bytes — a decoded `Ms` value
/// begins with `ESC ]` — and printing one would run it rather than show it.
/// [`crate::term::pretty`] names the introducers; anything still left below
/// `0x20`, plus `DEL`, becomes `<XX>` so nothing can slip through.
#[must_use]
pub fn printable(text: &str) -> String {
    let named = crate::term::pretty(text);
    let mut out = String::with_capacity(named.len());
    for ch in named.chars() {
        if (ch.is_control() && ch != ' ') || ch == '\u{7f}' {
            let _ = write!(out, "<{:02X}>", ch as u32);
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn round(replies: Vec<Reply>) -> Round {
        Round {
            replies,
            answered_barrier: true,
        }
    }

    fn csi(params: &str, final_byte: char) -> Reply {
        Reply::Csi {
            params: params.to_owned(),
            final_byte,
        }
    }

    // -- SGR readback --------------------------------------------------------

    #[test]
    fn the_sgr_query_sets_a_triple_and_puts_the_colour_back() {
        let query = sgr_readback_query();
        assert!(query.contains("38;2;1;2;3m"), "{query:?}");
        assert!(query.contains("\u{1b}P$qm\u{1b}\\"), "DECRQSS for SGR: {query:?}");
        assert!(query.ends_with("\u{1b}[0m"), "the colour must be reset: {query:?}");
    }

    #[test]
    fn parses_a_decrpss_reply() {
        assert_eq!(
            parse_decrpss("1$r0;38;2;1;2;3m"),
            Some((Some(1), "0;38;2;1;2;3".to_owned()))
        );
        assert_eq!(
            parse_decrpss("0$r0m"),
            Some((Some(0), "0".to_owned()))
        );
        // A reply with no status digits at all.
        assert_eq!(parse_decrpss("$r0m"), Some((None, "0".to_owned())));
    }

    #[test]
    fn ignores_a_decrpss_reply_about_another_setting() {
        // DECSCUSR is reported with a final `q`, not `m`.
        assert_eq!(parse_decrpss("1$r2 q"), None);
        // And a DCS payload that is not DECRPSS at all.
        assert_eq!(parse_decrpss("1+r524742=38"), None);
        assert_eq!(parse_decrpss(">|xterm(400)"), None);
    }

    #[test]
    fn a_truecolor_terminal_reports_the_triple_back() {
        let outcome = interpret_sgr(&round(vec![Reply::Dcs("1$r0;38;2;1;2;3m".to_owned())]));
        assert_eq!(outcome.colour, Some(Readback::Direct(1, 2, 3)));
        assert!(outcome.kept_the_triple());
        assert_eq!(outcome.advisory_flag, Some(1));
    }

    #[test]
    fn the_colon_form_is_understood_too() {
        // ITU-T T.416 spelling, with the unused colour-space id left empty. foot
        // and kitty echo this back regardless of which form was sent.
        let outcome = interpret_sgr(&round(vec![Reply::Dcs("1$r0;38:2::1:2:3m".to_owned())]));
        assert_eq!(outcome.colour, Some(Readback::Direct(1, 2, 3)));
        assert!(outcome.kept_the_triple());
        // And the form without the empty element.
        assert_eq!(parse_foreground("0;38:2:1:2:3"), Readback::Direct(1, 2, 3));
    }

    #[test]
    fn a_quantising_terminal_reports_an_index() {
        let outcome = interpret_sgr(&round(vec![Reply::Dcs("1$r0;38;5;16m".to_owned())]));
        assert_eq!(outcome.colour, Some(Readback::Indexed(16)));
        assert!(!outcome.kept_the_triple());
        assert_eq!(parse_foreground("0;38:5:16"), Readback::Indexed(16));
    }

    #[test]
    fn a_terminal_that_dropped_the_colour_is_not_a_terminal_that_kept_it() {
        let outcome = interpret_sgr(&round(vec![Reply::Dcs("1$r0m".to_owned())]));
        assert_eq!(outcome.colour, Some(Readback::NoColor));
        assert!(!outcome.kept_the_triple());
    }

    #[test]
    fn a_terminal_reporting_a_different_triple_is_not_agreement() {
        // Something rounded, or answered about a colour nobody set.
        let outcome = interpret_sgr(&round(vec![Reply::Dcs("1$r0;38;2;0;0;0m".to_owned())]));
        assert_eq!(outcome.colour, Some(Readback::Direct(0, 0, 0)));
        assert!(!outcome.kept_the_triple());
    }

    #[test]
    fn the_verdict_does_not_depend_on_the_advisory_flag() {
        // ctlseqs records that DEC's manuals had this flag reversed, so a `0` in
        // front of a correct triple must still read as truecolor.
        let outcome = interpret_sgr(&round(vec![Reply::Dcs("0$r0;38;2;1;2;3m".to_owned())]));
        assert_eq!(outcome.advisory_flag, Some(0));
        assert!(
            outcome.kept_the_triple(),
            "the content is the evidence, not the flag"
        );
    }

    #[test]
    fn silence_leaves_the_readback_empty_rather_than_negative() {
        let outcome = interpret_sgr(&round(Vec::new()));
        assert_eq!(outcome.colour, None);
        assert_eq!(outcome.raw, None);
        assert!(!outcome.kept_the_triple());
    }

    #[test]
    fn a_background_colour_is_not_mistaken_for_a_foreground_one() {
        // 48 is the background selector; only 38 answers this question.
        assert_eq!(parse_foreground("0;48;2;1;2;3"), Readback::NoColor);
        assert_eq!(parse_foreground("0;1;4"), Readback::NoColor);
        assert_eq!(parse_foreground(""), Readback::NoColor);
    }

    // -- cell size -----------------------------------------------------------

    #[test]
    fn the_cell_size_query_asks_every_way_there_is() {
        let query = cell_size_query();
        assert!(query.contains("\u{1b}[16t"));
        assert!(query.contains("ReportCellSize"));
        assert!(query.contains("\u{1b}[14t"));
        assert!(query.contains("\u{1b}[18t"));
    }

    #[test]
    fn csi_16t_is_preferred_when_it_answers() {
        let size = interpret_cell_size(&round(vec![
            csi("6;16;8", 't'),
            csi("4;384;640", 't'),
            csi("8;24;80", 't'),
        ]))
        .unwrap();
        assert_eq!(size.rung, Rung::Csi16t);
        assert_eq!(size.label(), "8x16");
    }

    #[test]
    fn iterm2_answers_its_own_query_when_csi_16t_is_skipped() {
        let size = interpret_cell_size(&round(vec![Reply::Osc(
            "1337;ReportCellSize=17.0;7.0".to_owned(),
        )]))
        .unwrap();
        assert_eq!(size.rung, Rung::ITerm2);
        assert_eq!(size.label(), "7x17");
    }

    #[test]
    fn iterm2_reports_height_before_width_and_may_add_a_scale() {
        assert_eq!(
            parse_iterm2_cell_size("1337;ReportCellSize=17.0;7.0"),
            Some((7.0, 17.0))
        );
        // The third field is the display scale, added in a later version.
        assert_eq!(
            parse_iterm2_cell_size("1337;ReportCellSize=17.0;7.0;2.0"),
            Some((7.0, 17.0))
        );
        assert_eq!(parse_iterm2_cell_size("1337;SetProfile=x"), None);
        assert_eq!(parse_iterm2_cell_size("52;c;aGk="), None);
    }

    #[test]
    fn dividing_the_text_area_is_the_last_rung() {
        // Alacritty and VTE skip `CSI 16 t`, so this is the only rung left.
        let size = interpret_cell_size(&round(vec![csi("4;384;640", 't'), csi("8;24;80", 't')]))
            .unwrap();
        assert_eq!(size.rung, Rung::Divided);
        assert_eq!(size.label(), "8x16");
    }

    #[test]
    fn dividing_needs_both_halves() {
        assert_eq!(interpret_cell_size(&round(vec![csi("4;384;640", 't')])), None);
        assert_eq!(interpret_cell_size(&round(vec![csi("8;24;80", 't')])), None);
        // A zero cell count would divide by zero and report infinity.
        assert_eq!(
            interpret_cell_size(&round(vec![csi("4;384;640", 't'), csi("8;0;0", 't')])),
            None
        );
    }

    #[test]
    fn a_terminal_that_answers_nothing_has_no_cell_size() {
        assert_eq!(interpret_cell_size(&round(Vec::new())), None);
        // Window position reports share the final byte and must be ignored.
        assert_eq!(interpret_cell_size(&round(vec![csi("3;0;0", 't')])), None);
    }

    #[test]
    fn every_rung_has_a_distinct_label() {
        let labels = [Rung::Csi16t, Rung::ITerm2, Rung::Divided].map(Rung::label);
        let mut sorted = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3);
    }

    // -- identity ------------------------------------------------------------

    #[test]
    fn the_identity_query_asks_xtversion_and_secondary_attributes() {
        let query = identity_query();
        assert!(query.contains("\u{1b}[>0q"), "XTVERSION: {query:?}");
        assert!(query.contains("\u{1b}[>c"), "DA2: {query:?}");
    }

    #[test]
    fn collects_xtversion_and_both_device_attribute_replies() {
        let identity = interpret_identity(&round(vec![
            Reply::Dcs(">|kitty 0.35.2".to_owned()),
            csi(">1;4000;29", 'c'),
            csi("?62;4", 'c'),
        ]));
        assert_eq!(identity.xtversion.as_deref(), Some("kitty 0.35.2"));
        assert_eq!(identity.da2.as_deref(), Some(">1;4000;29"));
        assert_eq!(identity.da1.as_deref(), Some("?62;4"));
        assert_eq!(observed_family(&identity), Some("kitty"));
    }

    #[test]
    fn secondary_attributes_are_not_mistaken_for_the_barrier() {
        // Both end in `c`; only the `>` prefix tells them apart, and taking DA2
        // for DA1 would end a round before its real barrier arrived.
        let identity = interpret_identity(&round(vec![csi(">41;356;0", 'c')]));
        assert_eq!(identity.da2.as_deref(), Some(">41;356;0"));
        assert_eq!(identity.da1, None);
    }

    #[test]
    fn a_late_xtgettcap_reply_is_not_read_as_a_version() {
        let identity = interpret_identity(&round(vec![Reply::Dcs("1+r524742=38".to_owned())]));
        assert_eq!(identity.xtversion, None);
        assert_eq!(observed_family(&identity), None);
    }

    #[test]
    fn a_silent_terminal_yields_no_identity_and_no_detail() {
        let identity = interpret_identity(&round(Vec::new()));
        assert_eq!(identity, Identity::default());
        assert_eq!(identity_detail(&identity), None);
    }

    #[test]
    fn the_detail_line_names_every_source_that_answered() {
        let identity = Identity {
            xtversion: Some("foot(1.16.2)".to_owned()),
            da1: Some("?62;4".to_owned()),
            da2: None,
        };
        let detail = identity_detail(&identity).unwrap();
        assert!(detail.contains("XTVERSION foot(1.16.2)"), "{detail}");
        assert!(detail.contains("DA1 ?62;4"), "{detail}");
        assert!(!detail.contains("DA2"), "{detail}");
    }

    // -- erase character -----------------------------------------------------

    #[test]
    fn finds_the_erase_character_in_stty_output() {
        let text = "speed 38400 baud; rows 24; columns 80; line = 0;\n\
                    intr = ^C; quit = ^\\; erase = ^?; kill = ^U; eof = ^D;\n";
        assert_eq!(
            parse_stty_erase(text),
            Erase::Found {
                canonical: "^?".to_owned(),
                raw: "^?".to_owned()
            }
        );
    }

    #[test]
    fn finds_the_older_convention_too() {
        let text = "intr = ^C; erase = ^H; kill = ^U;\n";
        assert_eq!(
            parse_stty_erase(text),
            Erase::Found {
                canonical: "^H".to_owned(),
                raw: "^H".to_owned()
            }
        );
    }

    #[test]
    fn an_erase_at_the_end_of_a_line_still_parses() {
        assert_eq!(
            parse_stty_erase("intr = ^C\nerase = ^?\nkill = ^U\n"),
            Erase::Found {
                canonical: "^?".to_owned(),
                raw: "^?".to_owned()
            }
        );
    }

    #[test]
    fn stty_output_without_an_erase_is_not_an_error() {
        assert_eq!(parse_stty_erase("speed 38400 baud;\n"), Erase::NotReported);
        assert_eq!(parse_stty_erase(""), Erase::NotReported);
    }

    #[test]
    fn an_unfamiliar_erase_spelling_is_kept_rather_than_guessed_at() {
        assert_eq!(
            parse_stty_erase("erase = <undef>;\n"),
            Erase::Unrecognised("<undef>".to_owned())
        );
    }

    #[test]
    fn both_bytes_are_recognised_in_every_spelling_the_sources_use() {
        // `kbs` and `stty` spell the same byte differently; comparing spellings
        // would report a disagreement between two sources that agree.
        for spelling in ["^?", "\\177", "\\0177", "0x7f", "DEL", "\u{7f}"] {
            assert_eq!(canonical_erase(spelling), Some("^?"), "{spelling:?}");
        }
        for spelling in ["^H", "\\010", "\\b", "0x08", "BS", "\u{8}"] {
            assert_eq!(canonical_erase(spelling), Some("^H"), "{spelling:?}");
        }
        assert_eq!(canonical_erase("^C"), None);
        assert_eq!(canonical_erase(""), None);
    }

    #[test]
    fn each_erase_byte_is_explained_differently() {
        assert!(erase_explanation("^?").contains("0x7f"));
        assert!(erase_explanation("^H").contains("0x08"));
        assert!(erase_explanation("^C").contains("neither"));
    }

    // -- printability --------------------------------------------------------

    #[test]
    fn a_reply_containing_escape_sequences_is_made_safe_to_print() {
        // A decoded `Ms` value begins with a real escape byte; printing it would
        // run it rather than show it.
        let value = "\u{1b}]52;%p1%s;%p2%s\u{7}";
        let safe = printable(value);
        assert!(!safe.contains('\u{1b}'), "{safe:?}");
        assert!(!safe.contains('\u{7}'), "{safe:?}");
        assert!(safe.contains("OSC"), "{safe:?}");
    }

    #[test]
    fn every_control_byte_is_replaced() {
        // This used to be named "…including the ones pretty leaves", because
        // `term::pretty` only escaped the four controls it had names for and left
        // the rest raw. It now escapes all of them, so a control byte may arrive
        // here already spelled `\xNN` and never reach this replacement. The
        // guarantee is the same either way: nothing control-like survives.
        let nasty = "a\u{1}b\u{7f}c\u{9b}d\u{0}";
        let safe = printable(nasty);
        for ch in safe.chars() {
            assert!(!ch.is_control() || ch == ' ', "{safe:?} kept {ch:?}");
        }
        assert!(safe.contains("<01>") || safe.contains("\\x01"), "{safe:?}");
        assert!(safe.contains("<7F>") || safe.contains("\\x7f"), "{safe:?}");
    }

    #[test]
    fn ordinary_text_passes_through_unchanged() {
        assert_eq!(printable("xterm-256color"), "xterm-256color");
        assert_eq!(printable("8"), "8");
        assert_eq!(printable(""), "");
    }
}
