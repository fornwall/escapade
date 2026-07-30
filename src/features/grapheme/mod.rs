//! Grapheme clustering: DEC private mode 2027.
//!
//! A terminal advances its cursor as bytes arrive. Traditionally it does that
//! one codepoint at a time, asking `wcwidth` how many cells each one wants. That
//! works right up until a *grapheme cluster* — what a user calls one character —
//! is spelled with more than one codepoint. The farmer emoji 🧑‍🌾 is
//! U+1F9D1 U+200D U+1F33E: `wcwidth` says 2, 0 and 2, so a `wcwidth` terminal
//! moves the cursor four cells for something the font draws in two. Every column
//! an application computed after that point is wrong, and a full-screen layout
//! comes apart.
//!
//! **Mode 2027**, from the Terminal Unicode Core specification, asks the
//! terminal to segment its input into grapheme clusters by UAX #29 and advance
//! per cluster instead:
//!
//! ```text
//! CSI ? 2027 h               enable    (DECSET)
//! CSI ? 2027 l               disable   (DECRST)
//! CSI ? 2027 $ p             query     (DECRQM)
//! CSI ? 2027 ; <value> $ y   the answer (DECRPM)
//! ```
//!
//! The `<value>` semantics are DECRPM's, not this specification's — the spec
//! deliberately reuses DECRQM so it needs no new sequence. See [`ModeState`].
//!
//! # The query cannot be trusted on its own
//!
//! Some terminals answer DECRQM confidently about modes they have never heard of.
//! iTerm2 replies `4` — permanently reset — for any mode it does not recognise,
//! including numbers that do not exist. Taken at face value, that reads as "this
//! terminal knows grapheme clustering and has it firmly disabled", which is a
//! fabrication.
//!
//! So every query here is paired with a **control**: the same DECRQM for
//! [`SENTINEL_MODE`], a number no terminal can implement, in the same round trip
//! and behind the same [`barrier`]. Whatever a terminal says about a mode that
//! cannot exist is the measure of what its other answers are worth. See
//! [`Reliability`] and [`reliability`].
//!
//! None of this touches [`measure`], which prints the sequence and reads the
//! cursor back. That is why the measurement is the primitive and the mode query is
//! only a hint.
//!
//! Specification: <https://github.com/contour-terminal/terminal-unicode-core>

pub mod cluster;
pub mod json;
pub mod measure;
pub mod probe;
pub mod render;
pub mod sequence;

use std::process::ExitCode;

use anyhow::Result;

use crate::{
    cli::{FeatureCommand, GraphemeAction},
    db::Catalog,
    input::Reply,
    term::CSI,
    theme::Theme,
};

/// The feature id in the database. The subcommand is `grapheme`.
pub const FEATURE_ID: &str = "grapheme-clustering";

/// The DEC private mode number the Terminal Unicode Core specification claims.
pub const MODE: u32 = 2027;

/// A mode number no terminal can possibly implement, queried as a **control**.
///
/// Some terminals answer DECRQM with a confident state for modes they have never
/// heard of. iTerm2 answers `4` (permanently reset) for anything it does not
/// recognise, fictional numbers included — so on iTerm2 a `4` for mode 2027 does
/// not mean "knows grapheme clustering, firmly disabled". It means nothing at all,
/// and reporting it as a state is worse than reporting nothing, because a reader
/// cannot tell it from a genuine permanent reset.
///
/// Querying this number alongside the real one turns that into something
/// detectable: whatever a terminal says about a mode that cannot exist is what its
/// answers are worth. See [`Reliability`].
pub const SENTINEL_MODE: u32 = 9_876_543;

// Enforced at compile time, because the control is worthless if it could ever
// name a mode that exists, and `u16` would silently truncate it in a reply.
const _: () = assert!(
    SENTINEL_MODE > 1_000_000,
    "must be far outside real mode numbers"
);
const _: () = assert!(SENTINEL_MODE != MODE);
const _: () = assert!(
    SENTINEL_MODE > u16::MAX as u32,
    "the report parser must not use u16"
);

/// A sequence whose cell count differs between `wcwidth` and UAX #29, used
/// whenever something needs a sequence to measure and the user did not name one.
///
/// U+1F9D1 U+200D U+1F33E — 🧑‍🌾: two cells clustered, four unclustered.
pub const REFERENCE_SEQUENCE: &str = "\u{1f9d1}\u{200d}\u{1f33e}";

// -- building sequences ------------------------------------------------------

/// Enables grapheme clustering: `CSI ? 2027 h`.
#[must_use]
pub fn enable() -> String {
    format!("{CSI}?{MODE}h")
}

/// Disables grapheme clustering: `CSI ? 2027 l`.
#[must_use]
pub fn disable() -> String {
    format!("{CSI}?{MODE}l")
}

/// `CSI ? 2027 h` when `enabled`, `CSI ? 2027 l` otherwise.
#[must_use]
pub fn set(enabled: bool) -> String {
    if enabled { enable() } else { disable() }
}

/// Asks for the mode's current state: `CSI ? 2027 $ p` (DECRQM).
///
/// The `$` is an intermediate byte, so this is not the same shape as the
/// `CSI ? 2027 h` that sets it — a terminal can perfectly well implement one and
/// ignore the other, which is why [`ModeState::NotRecognized`] and "no answer at
/// all" have to stay separate verdicts.
#[must_use]
pub fn query() -> String {
    format!("{CSI}?{MODE}$p")
}

/// Asks about [`SENTINEL_MODE`]: `CSI ? 9876543 $ p`.
///
/// The control query. Sent in the same round trip as [`query`] and behind the same
/// [`barrier`], because what a terminal says about a mode that cannot exist is the
/// measure of what its other answers are worth.
#[must_use]
pub fn sentinel_query() -> String {
    format!("{CSI}?{SENTINEL_MODE}$p")
}

/// Primary Device Attributes: `CSI c`.
///
/// Sent straight after a query as a **barrier**. Every terminal since the VT100
/// answers it, and terminals process input in order, so a DA1 answer arriving
/// with no DECRPM answer in front of it proves the terminal had its chance and
/// declined — which turns a timeout into a conclusion.
#[must_use]
pub fn barrier() -> String {
    format!("{CSI}c")
}

/// Cursor Position Report request: `CSI 6 n`.
///
/// The whole measurement in [`measure`] rests on this: it is the only way to ask
/// a terminal what it *did*, as opposed to what it says it supports.
#[must_use]
pub fn position_query() -> String {
    format!("{CSI}6n")
}

// -- the DECRPM answer ------------------------------------------------------

/// What DECRPM reported about a mode.
///
/// The numbers are DEC's, as documented for DECRPM in xterm's control-sequence
/// reference: `0` not recognized, `1` set, `2` reset, `3` permanently set,
/// `4` permanently reset. The Terminal Unicode Core specification does not
/// restate them; it says DECRQM's reply "will indicate each state accurately
/// enough not to need any new VT sequence introduced" and leaves the encoding to
/// DECRQM.
///
/// All five matter here, and so does their absence:
///
/// * `1`/`2` — the terminal implements the mode and it can be switched.
/// * `3` — clustering is always on. Ghostty behaves this way when its
///   `grapheme-width-method` is already `unicode`; foot reports permanently
///   *reset* when its width method is not `double-width`.
/// * `0` — the terminal parses DECRQM but has never heard of mode 2027.
/// * no reply — the terminal does not implement DECRQM at all, or something in
///   between swallowed it. Not the same as `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeState {
    /// `0` — DECRQM understood, mode 2027 unknown.
    NotRecognized,
    /// `1` — implemented and currently on.
    Set,
    /// `2` — implemented and currently off.
    Reset,
    /// `3` — always on; `CSI ? 2027 l` will not turn it off.
    PermanentlySet,
    /// `4` — always off; `CSI ? 2027 h` will not turn it on.
    PermanentlyReset,
    /// A value DECRPM does not define. Kept rather than folded into one of the
    /// others, because guessing is how a probe starts lying.
    Undefined(u16),
}

impl ModeState {
    /// Decodes a DECRPM value.
    #[must_use]
    pub fn from_value(value: u16) -> Self {
        match value {
            0 => Self::NotRecognized,
            1 => Self::Set,
            2 => Self::Reset,
            3 => Self::PermanentlySet,
            4 => Self::PermanentlyReset,
            other => Self::Undefined(other),
        }
    }

    /// The DECRPM value this state is spelled with.
    #[must_use]
    pub fn value(self) -> u16 {
        match self {
            Self::NotRecognized => 0,
            Self::Set => 1,
            Self::Reset => 2,
            Self::PermanentlySet => 3,
            Self::PermanentlyReset => 4,
            Self::Undefined(other) => other,
        }
    }

    /// Short human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::NotRecognized => "not recognised",
            Self::Set => "set",
            Self::Reset => "reset",
            Self::PermanentlySet => "permanently set",
            Self::PermanentlyReset => "permanently reset",
            Self::Undefined(_) => "undefined DECRPM value",
        }
    }

    /// One sentence a reader can act on.
    #[must_use]
    pub fn explanation(self) -> &'static str {
        match self {
            Self::NotRecognized => {
                "The terminal parses DECRQM but does not know mode 2027, so it counts cells with \
                 wcwidth and will not be talked out of it."
            }
            Self::Set => {
                "Grapheme clustering is on right now — either the terminal defaults to it or \
                 something in this session switched it on."
            }
            Self::Reset => {
                "The terminal implements mode 2027 but has it off. Send `CSI ? 2027 h` to turn it \
                 on, and put it back when you are done."
            }
            Self::PermanentlySet => {
                "Clustering is always on and cannot be switched off. Nothing to enable — just \
                 measure widths per cluster."
            }
            Self::PermanentlyReset => {
                "The terminal knows the mode and refuses it, usually because it is configured for \
                 a width method that cannot honour it. Enabling it will not work; count widths the \
                 way the terminal does instead."
            }
            Self::Undefined(_) => {
                "DECRPM answered with a value it does not define. Treat the mode as unavailable \
                 and measure rather than trust."
            }
        }
    }

    /// Whether clustering is on right now.
    #[must_use]
    pub fn enabled(self) -> bool {
        matches!(self, Self::Set | Self::PermanentlySet)
    }

    /// Whether the terminal knows what mode 2027 is.
    ///
    /// `4` counts: a terminal that says "permanently reset" has recognised the
    /// mode and told us it will never honour it, which is far more useful than
    /// silence.
    #[must_use]
    pub fn recognized(self) -> bool {
        !matches!(self, Self::NotRecognized | Self::Undefined(_))
    }

    /// Whether `CSI ? 2027 h` / `l` will actually change anything.
    #[must_use]
    pub fn settable(self) -> bool {
        matches!(self, Self::Set | Self::Reset)
    }
}

/// What the [`SENTINEL_MODE`] control says about a terminal's DECRQM answers.
///
/// Derived from the sentinel and the barrier, never from the mode 2027 answer
/// itself — a state cannot vouch for its own trustworthiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reliability {
    /// The terminal treats a mode it does not know differently from one it does:
    /// it answered `0` for the impossible mode, or stayed silent about it while
    /// answering the real one. Either way its mode 2027 answer means what it says.
    Discriminating,
    /// The terminal gave the impossible mode a confident non-zero state, so it
    /// answers the same way whether or not it knows a mode. **Every DECRQM result
    /// in this run is uninformative**, including a `4` for mode 2027 that would
    /// otherwise read as a definite no. iTerm2 behaves this way.
    Indiscriminate(ModeState),
    /// Neither query was answered, but the barrier was: this terminal implements
    /// no DECRQM at all. A better finding than "no answer", because it is a
    /// property of the terminal rather than of the moment. Konsole is one.
    NoDecrqm,
    /// Nothing came back, not even the barrier. Nothing is known about anything.
    Silent,
}

impl Reliability {
    /// Whether a mode state from this run may be reported as a state.
    #[must_use]
    pub fn trustworthy(self) -> bool {
        matches!(self, Self::Discriminating)
    }

    /// Short human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Discriminating => "answers are meaningful",
            Self::Indiscriminate(_) => "answers are meaningless",
            Self::NoDecrqm => "no DECRQM at all",
            Self::Silent => "nothing answered",
        }
    }

    /// Stable machine-readable name, for `--format json`.
    #[must_use]
    pub fn json_value(self) -> &'static str {
        match self {
            Self::Discriminating => "discriminating",
            Self::Indiscriminate(_) => "indiscriminate",
            Self::NoDecrqm => "no-decrqm",
            Self::Silent => "silent",
        }
    }

    /// What this means for the reader, in one sentence.
    #[must_use]
    pub fn explanation(self) -> String {
        match self {
            Self::Discriminating => format!(
                "The control query for impossible mode {SENTINEL_MODE} came back as `not \
                 recognised` (or not at all), so this terminal distinguishes modes it knows from \
                 modes it does not. Its answer about mode 2027 can be taken at face value."
            ),
            Self::Indiscriminate(state) => format!(
                "The control query for impossible mode {SENTINEL_MODE} came back as `{}` — a \
                 confident state for a mode that cannot exist. This terminal answers the same way \
                 regardless of whether it knows a mode, so nothing it said about mode 2027 is \
                 informative. iTerm2 does exactly this.",
                state.label()
            ),
            Self::NoDecrqm => format!(
                "Neither mode 2027 nor impossible mode {SENTINEL_MODE} was answered, but the \
                 device-attributes barrier was. This terminal does not implement DECRQM, which is \
                 a permanent property rather than a slow reply."
            ),
            Self::Silent => "Not even the device-attributes barrier came back, so nothing here is \
                             a statement about the terminal — only about the channel to it."
                .to_owned(),
        }
    }
}

/// Reads the control against the real query to decide what the answers are worth.
///
/// * impossible mode answered `0` → [`Reliability::Discriminating`]
/// * impossible mode answered anything else → [`Reliability::Indiscriminate`]
/// * impossible mode silent but mode 2027 answered → [`Reliability::Discriminating`],
///   because staying silent about a mode it does not know *is* discrimination
/// * both silent, barrier answered → [`Reliability::NoDecrqm`]
/// * nothing at all → [`Reliability::Silent`]
#[must_use]
pub fn reliability(
    sentinel: Option<ModeState>,
    mode: Option<ModeState>,
    answered_barrier: bool,
) -> Reliability {
    match sentinel {
        Some(ModeState::NotRecognized) => Reliability::Discriminating,
        Some(state) => Reliability::Indiscriminate(state),
        // Silence about an impossible mode is the honest answer, so a terminal
        // that answered the real query and skipped this one is discriminating.
        None if mode.is_some() => Reliability::Discriminating,
        None if answered_barrier => Reliability::NoDecrqm,
        None => Reliability::Silent,
    }
}

/// Parses a DECRPM report — `CSI ? 2027 ; 1 $ y` — from a `CSI`-shaped reply.
///
/// `params` is the raw parameter text with the private prefix kept and
/// intermediates dropped, exactly as [`Reply::Csi`] delivers it, so a report for
/// mode 2027 arrives as `("?2027;1", 'y')`.
///
/// Returns the mode number alongside its state, because a report about some other
/// mode arrives on the same stream and must not be mistaken for ours — the
/// sentinel control depends on telling the two apart. `None` for anything that is
/// not a DEC private mode report.
///
/// The mode number is `u32`: [`SENTINEL_MODE`] is deliberately far outside the
/// range real modes occupy, and would not survive a `u16`.
#[must_use]
pub fn parse_mode_report(params: &str, final_byte: char) -> Option<(u32, ModeState)> {
    if final_byte != 'y' {
        return None;
    }
    // The `?` is not decoration: without it this is a report about an *ANSI*
    // mode, which is a different number space entirely.
    let body = params.strip_prefix('?')?;
    let (mode, value) = body.split_once(';')?;
    let mode = mode.trim().parse::<u32>().ok()?;
    let value = value.trim().parse::<u16>().ok()?;
    Some((mode, ModeState::from_value(value)))
}

/// Picks the state for `wanted` out of a reply, ignoring reports about other modes.
#[must_use]
pub fn mode_state_for(reply: &Reply, wanted: u32) -> Option<ModeState> {
    let Reply::Csi { params, final_byte } = reply else {
        return None;
    };
    match parse_mode_report(params, *final_byte) {
        Some((mode, state)) if mode == wanted => Some(state),
        _ => None,
    }
}

/// Picks mode 2027's state out of a reply.
#[must_use]
pub fn mode_state_from_reply(reply: &Reply) -> Option<ModeState> {
    mode_state_for(reply, MODE)
}

/// Picks the [`SENTINEL_MODE`] control's state out of a reply.
#[must_use]
pub fn sentinel_state_from_reply(reply: &Reply) -> Option<ModeState> {
    mode_state_for(reply, SENTINEL_MODE)
}

/// What to send to put the mode back the way it was found, if anything.
///
/// `previous` is the **trusted** state DECRQM reported before the mode was
/// touched, or `None` when nothing answered *or the answer could not be trusted*.
/// Blindly sending `CSI ? 2027 l` afterwards would be a bug: an application that
/// had clustering on for a good reason would find it off, and this tool would have
/// broken the very thing it documents.
///
/// Feeding an untrusted state in here is the same bug wearing a hat. On a terminal
/// that answers `4` for every mode, `PermanentlyReset` would say "nothing was
/// changed" and leave clustering switched on for whatever ran next — so callers
/// must pass what the sentinel vouches for, not the raw report.
#[must_use]
#[allow(
    clippy::match_same_arms,
    reason = "each arm documents a different reason for sending nothing"
)]
pub fn restore_sequence(previous: Option<ModeState>) -> Option<String> {
    match previous {
        // It was already on. Leave it on.
        Some(ModeState::Set | ModeState::PermanentlySet) => None,
        // Recognised, and we changed it. Put it back.
        Some(ModeState::Reset) => Some(disable()),
        // The mode cannot be changed, so nothing was changed.
        Some(ModeState::PermanentlyReset | ModeState::NotRecognized) => None,
        // A value we do not understand, or no answer at all: we sent an enable,
        // so we send the matching disable. Undoing our own write is the least
        // surprising thing left.
        Some(ModeState::Undefined(_)) | None => Some(disable()),
    }
}

/// What a probe concluded, as the exit status carries it.
///
/// `probe` is silent by default so it composes as a shell predicate:
///
/// ```sh
/// escapade grapheme probe && echo 'clustering available'
/// ```
///
/// Three codes rather than two, because "I could not find out" is not the same
/// answer as "no" and a script that treats them alike will make the wrong call on
/// exactly the terminals this tool exists to warn about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finding {
    /// Clustering works here. Exit status `0`.
    Supported,
    /// It demonstrably does not. Exit status `1`.
    Unsupported,
    /// Nothing could be established either way. Exit status `2`.
    Undetermined,
}

impl Finding {
    /// The process exit status.
    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        match self {
            Self::Supported => ExitCode::SUCCESS,
            Self::Unsupported => ExitCode::FAILURE,
            Self::Undetermined => ExitCode::from(2),
        }
    }

    /// Short human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "not supported",
            Self::Undetermined => "undetermined",
        }
    }

    /// The value used in `--format json`, which is the label without spaces.
    #[must_use]
    pub fn json_value(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Undetermined => "undetermined",
        }
    }
}

/// Entry point for `escapade grapheme …`.
///
/// Returns the process exit status, because `probe` answers through it — see
/// [`Finding`]. Everything else succeeds or propagates an error.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with.
pub fn run(
    catalog: &Catalog,
    command: &FeatureCommand<GraphemeAction>,
    no_color: bool,
) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        GraphemeAction::Explain { no_matrix } => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        // One verb, two questions. Without codepoints the subject is the mode
        // itself; with them it is a concrete sequence, which is the more useful
        // question and the one the measurement answers directly.
        GraphemeAction::Probe {
            target: None,
            format,
            ..
        } => Ok(probe::report(&theme, *format)?.exit_code()),
        GraphemeAction::Probe {
            target: Some(spec),
            format,
            long,
        } => Ok(sequence::report(&theme, spec, *format, *long)?.exit_code()),
        GraphemeAction::Render {
            spec,
            disabled,
            debug,
        } => {
            render::report(&theme, spec, *disabled, *debug)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn sequences_match_the_specification() {
        assert_eq!(enable(), "\u{1b}[?2027h");
        assert_eq!(disable(), "\u{1b}[?2027l");
        assert_eq!(query(), "\u{1b}[?2027$p");
        assert_eq!(barrier(), "\u{1b}[c");
        assert_eq!(position_query(), "\u{1b}[6n");
    }

    #[test]
    fn set_picks_the_right_final_byte() {
        assert_eq!(set(true), enable());
        assert_eq!(set(false), disable());
        assert!(set(true).ends_with('h'));
        assert!(set(false).ends_with('l'));
    }

    #[test]
    fn the_mode_number_appears_verbatim_in_every_private_mode_sequence() {
        for sequence in [enable(), disable(), query()] {
            assert!(sequence.contains("2027"), "{sequence:?}");
            assert!(sequence.starts_with("\u{1b}[?"), "{sequence:?}");
        }
    }

    #[test]
    fn decrpm_values_use_the_dec_numbering() {
        // 0 not recognized, 1 set, 2 reset, 3 permanently set, 4 permanently
        // reset — per xterm's DECRPM documentation. Getting 1 and 2 the wrong
        // way round would invert every verdict this subcommand prints.
        assert_eq!(ModeState::from_value(0), ModeState::NotRecognized);
        assert_eq!(ModeState::from_value(1), ModeState::Set);
        assert_eq!(ModeState::from_value(2), ModeState::Reset);
        assert_eq!(ModeState::from_value(3), ModeState::PermanentlySet);
        assert_eq!(ModeState::from_value(4), ModeState::PermanentlyReset);
        assert_eq!(ModeState::from_value(7), ModeState::Undefined(7));
    }

    #[test]
    fn every_state_round_trips_through_its_value() {
        for value in 0..=8 {
            assert_eq!(ModeState::from_value(value).value(), value);
        }
    }

    #[test]
    fn only_set_and_permanently_set_mean_clustering_is_on() {
        assert!(ModeState::Set.enabled());
        assert!(ModeState::PermanentlySet.enabled());
        assert!(!ModeState::Reset.enabled());
        assert!(!ModeState::PermanentlyReset.enabled());
        assert!(!ModeState::NotRecognized.enabled());
        assert!(!ModeState::Undefined(9).enabled());
    }

    #[test]
    fn permanently_reset_still_counts_as_recognised() {
        // "I know this mode and will never honour it" is information; "0" is not.
        assert!(ModeState::PermanentlyReset.recognized());
        assert!(ModeState::PermanentlySet.recognized());
        assert!(ModeState::Set.recognized());
        assert!(ModeState::Reset.recognized());
        assert!(!ModeState::NotRecognized.recognized());
        assert!(!ModeState::Undefined(5).recognized());
    }

    #[test]
    fn only_the_switchable_states_are_settable() {
        assert!(ModeState::Set.settable());
        assert!(ModeState::Reset.settable());
        assert!(!ModeState::PermanentlySet.settable());
        assert!(!ModeState::PermanentlyReset.settable());
        assert!(!ModeState::NotRecognized.settable());
    }

    #[test]
    fn every_state_has_a_distinct_label_and_explanation() {
        let states = [
            ModeState::NotRecognized,
            ModeState::Set,
            ModeState::Reset,
            ModeState::PermanentlySet,
            ModeState::PermanentlyReset,
        ];
        for field in [
            states.map(ModeState::label),
            states.map(ModeState::explanation),
        ] {
            let mut seen = field.to_vec();
            seen.sort_unstable();
            let total = seen.len();
            seen.dedup();
            assert_eq!(seen.len(), total, "two states share a description");
        }
    }

    #[test]
    fn parses_the_decrpm_report_for_every_value() {
        for (value, expected) in [
            (0, ModeState::NotRecognized),
            (1, ModeState::Set),
            (2, ModeState::Reset),
            (3, ModeState::PermanentlySet),
            (4, ModeState::PermanentlyReset),
        ] {
            let params = format!("?2027;{value}");
            assert_eq!(
                parse_mode_report(&params, 'y'),
                Some((2027, expected)),
                "{params}"
            );
        }
    }

    #[test]
    fn ignores_replies_that_are_not_dec_private_mode_reports() {
        // Primary Device Attributes — the barrier, which arrives on the same
        // stream and must never be read as a mode report.
        assert_eq!(parse_mode_report("?62;4", 'c'), None);
        // Cursor Position Report.
        assert_eq!(parse_mode_report("14;1", 'R'), None);
        // A report about an ANSI mode, not a DEC private one.
        assert_eq!(parse_mode_report("2027;1", 'y'), None);
        // Truncated and malformed.
        assert_eq!(parse_mode_report("?2027", 'y'), None);
        assert_eq!(parse_mode_report("?;1", 'y'), None);
        assert_eq!(parse_mode_report("?2027;x", 'y'), None);
        assert_eq!(parse_mode_report("", 'y'), None);
    }

    #[test]
    fn a_report_about_another_mode_keeps_its_number() {
        // Synchronized output is mode 2026 — one digit away, and a completely
        // different feature.
        assert_eq!(
            parse_mode_report("?2026;1", 'y'),
            Some((2026, ModeState::Set))
        );
        let reply = Reply::Csi {
            params: "?2026;1".to_owned(),
            final_byte: 'y',
        };
        assert_eq!(mode_state_from_reply(&reply), None, "wrong mode");
    }

    #[test]
    fn extracts_our_mode_from_a_reply() {
        let reply = Reply::Csi {
            params: "?2027;3".to_owned(),
            final_byte: 'y',
        };
        assert_eq!(
            mode_state_from_reply(&reply),
            Some(ModeState::PermanentlySet)
        );
        assert_eq!(
            mode_state_from_reply(&Reply::Apc("25a1;s".to_owned())),
            None
        );
    }

    // -- the impossible-mode control ----------------------------------------

    #[test]
    fn the_sentinel_query_is_a_decrqm_for_the_impossible_mode() {
        // The numeric properties are asserted at compile time next to the
        // constant; this covers the sequence built from it.
        assert_eq!(sentinel_query(), "\u{1b}[?9876543$p");
        assert!(sentinel_query().ends_with("$p"), "it must be a DECRQM");
        assert!(sentinel_query().starts_with("\u{1b}[?"), "and DEC-private");
        assert_ne!(sentinel_query(), query());
    }

    #[test]
    fn case_one_impossible_mode_not_recognised_means_answers_are_meaningful() {
        // The terminal distinguishes modes it knows from modes it does not, so its
        // mode 2027 answer says what it means — including a `4`.
        let reliability = reliability(
            Some(ModeState::NotRecognized),
            Some(ModeState::PermanentlyReset),
            true,
        );
        assert_eq!(reliability, Reliability::Discriminating);
        assert!(reliability.trustworthy());
    }

    #[test]
    fn case_two_impossible_mode_answering_four_voids_every_result() {
        // iTerm2: `4` for mode 9876543, and `4` for mode 2027. The second is not a
        // finding, it is an echo of the first.
        let reliability = reliability(
            Some(ModeState::PermanentlyReset),
            Some(ModeState::PermanentlyReset),
            true,
        );
        assert_eq!(
            reliability,
            Reliability::Indiscriminate(ModeState::PermanentlyReset)
        );
        assert!(
            !reliability.trustworthy(),
            "a `4` for a mode that cannot exist makes every `4` worthless"
        );
        assert!(reliability.explanation().contains("iTerm2"));
    }

    #[test]
    fn case_two_covers_any_non_zero_answer_not_just_four() {
        for state in [
            ModeState::Set,
            ModeState::Reset,
            ModeState::PermanentlySet,
            ModeState::PermanentlyReset,
            ModeState::Undefined(9),
        ] {
            let reliability = reliability(Some(state), Some(ModeState::Reset), true);
            assert_eq!(
                reliability,
                Reliability::Indiscriminate(state),
                "a confident {state:?} about an impossible mode is still nonsense"
            );
            assert!(!reliability.trustworthy());
        }
    }

    #[test]
    fn case_three_silence_from_both_with_a_barrier_means_no_decrqm_at_all() {
        // Konsole. A permanent property of the terminal, not a slow reply — which
        // is a better finding than "no answer".
        let reliability = reliability(None, None, true);
        assert_eq!(reliability, Reliability::NoDecrqm);
        assert!(!reliability.trustworthy());
        assert!(
            reliability
                .explanation()
                .contains("does not implement DECRQM")
        );
    }

    #[test]
    fn silence_from_everything_including_the_barrier_says_nothing_at_all() {
        let reliability = reliability(None, None, false);
        assert_eq!(reliability, Reliability::Silent);
        assert!(!reliability.trustworthy());
        assert!(reliability.explanation().contains("channel"));
    }

    #[test]
    fn staying_silent_about_an_impossible_mode_is_itself_discrimination() {
        // Answered the real query, ignored the fictional one: that is a terminal
        // telling known modes from unknown ones just as clearly as a `0` would.
        let reliability = reliability(None, Some(ModeState::Reset), true);
        assert_eq!(reliability, Reliability::Discriminating);
        assert!(reliability.trustworthy());
    }

    #[test]
    fn every_reliability_has_a_distinct_label_and_explanation() {
        let all = [
            Reliability::Discriminating,
            Reliability::Indiscriminate(ModeState::PermanentlyReset),
            Reliability::NoDecrqm,
            Reliability::Silent,
        ];
        for mut seen in [
            all.map(Reliability::label).map(str::to_owned).to_vec(),
            all.map(Reliability::explanation).to_vec(),
        ] {
            seen.sort();
            let total = seen.len();
            seen.dedup();
            assert_eq!(seen.len(), total, "two reliabilities read the same");
        }
    }

    #[test]
    fn the_sentinel_reply_is_matched_by_mode_number_not_by_position() {
        // Both reports arrive on the same stream and may come back in either
        // order, so they can only be told apart by the number they carry.
        let real = Reply::Csi {
            params: format!("?{MODE};2"),
            final_byte: 'y',
        };
        let control = Reply::Csi {
            params: format!("?{SENTINEL_MODE};0"),
            final_byte: 'y',
        };
        assert_eq!(mode_state_from_reply(&real), Some(ModeState::Reset));
        assert_eq!(mode_state_from_reply(&control), None);
        assert_eq!(
            sentinel_state_from_reply(&control),
            Some(ModeState::NotRecognized)
        );
        assert_eq!(sentinel_state_from_reply(&real), None);
    }

    #[test]
    fn a_large_mode_number_survives_parsing() {
        // The sentinel does not fit in a `u16`, so the report parser must not use
        // one — otherwise the control's own answer would be silently dropped and
        // every run would look `NoDecrqm`.
        assert_eq!(
            parse_mode_report(&format!("?{SENTINEL_MODE};4"), 'y'),
            Some((SENTINEL_MODE, ModeState::PermanentlyReset))
        );
    }

    #[test]
    fn restoring_never_switches_off_a_mode_the_user_had_on() {
        assert_eq!(restore_sequence(Some(ModeState::Set)), None);
        assert_eq!(restore_sequence(Some(ModeState::PermanentlySet)), None);
    }

    #[test]
    fn restoring_puts_back_a_mode_that_was_off() {
        assert_eq!(restore_sequence(Some(ModeState::Reset)), Some(disable()));
    }

    #[test]
    fn restoring_sends_nothing_when_nothing_could_have_changed() {
        assert_eq!(restore_sequence(Some(ModeState::NotRecognized)), None);
        assert_eq!(restore_sequence(Some(ModeState::PermanentlyReset)), None);
    }

    #[test]
    fn restoring_undoes_our_own_write_when_the_state_is_unknown() {
        assert_eq!(restore_sequence(None), Some(disable()));
        assert_eq!(
            restore_sequence(Some(ModeState::Undefined(9))),
            Some(disable())
        );
    }

    #[test]
    fn the_reference_sequence_is_one_cluster_that_wcwidth_miscounts() {
        let clusters = cluster::analyze(REFERENCE_SEQUENCE);
        assert_eq!(clusters.len(), 1, "the farmer is one grapheme");
        assert_eq!(clusters[0].expected, 2);
        assert_eq!(clusters[0].legacy, 4);
        assert!(
            clusters[0].miscounted(),
            "a reference sequence both rules agree on could not prove anything"
        );
    }

    #[test]
    fn the_reference_sequence_is_safe_to_send_and_to_print() {
        // `probe` prints it verbatim in its report, so this is the invariant that
        // keeps that safe without a runtime guard on a compile-time constant.
        assert!(!REFERENCE_SEQUENCE.contains('\u{1b}'));
        for character in REFERENCE_SEQUENCE.chars() {
            let info = crate::unicode::describe(character.into());
            assert!(
                cluster::is_sendable(&info),
                "U+{:04X} in the reference sequence is not sendable",
                info.codepoint
            );
            assert!(!character.is_control());
        }
    }
}
