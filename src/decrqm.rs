//! DECRQM: asking a DEC private mode for its state.
//!
//! ```text
//! →  CSI ? <mode> $ p            DECRQM
//! ←  CSI ? <mode> ; <value> $ y  DECRPM
//! ```
//!
//! `<value>` is `0` not recognized, `1` set, `2` reset, `3` permanently set,
//! `4` permanently reset. Verified against xterm's `ctlseqs`, "Request DEC
//! private mode (DECRQM)".
//!
//! The `$` is an intermediate byte, so this is not the same shape as the
//! `CSI ? <mode> h` that sets the mode — a terminal can perfectly well implement
//! one and ignore the other, which is why [`ModeState::NotRecognized`] and "no
//! answer at all" have to stay separate verdicts.
//!
//! # Why a sentinel is mandatory
//!
//! A DECRQM answer is worthless on its own, because two terminals break the
//! contract in opposite directions:
//!
//! * **iTerm2 answers modes it has never heard of.** Asked about a *fictional*
//!   mode it replies `4`, permanently reset — the same answer it gives for a real
//!   mode it genuinely refuses. So `4` cannot be read as "knows the mode, has it
//!   firmly off" without first checking what the terminal says about a mode that
//!   cannot exist.
//! * **Konsole answers nothing at all**, despite supporting the modes. There is
//!   no timeout long enough to fix that; the Primary Device Attributes barrier is
//!   what turns the hang into the conclusion "this terminal does not implement
//!   DECRQM".
//!
//! So every run asks about [`SENTINEL_MODE`] first. If the terminal answers it,
//! every reply carrying the sentinel's value is marked unreliable and the report
//! says why.
//!
//! # Why this lives at the crate root
//!
//! Three features ask a mode about itself and none of them owns the question:
//! [`crate::features::grapheme`] asks about mode 2027,
//! [`crate::features::color::notify`] about mode 2031, and
//! [`crate::features::doctor`] about a list of them. It was carried as two
//! near-identical copies until the third caller arrived, and the copies had
//! already drifted — one parsed DECRPM values as `u16`, which is a bug waiting for
//! a terminal that answers something larger.
//!
//! What is *not* here is what a given feature makes of a state. The prose
//! explaining what a reset mode 2027 means to a text-measuring program belongs to
//! that feature, and so does whatever it decides to do about it.

use crate::{features::Support, input::Reply, term::CSI};

/// A DEC private mode that cannot exist, used as a control.
///
/// Any answer to this is a terminal answering a question it cannot possibly know
/// the answer to. The number is far outside every allocated range and outside
/// `u16` entirely, so it cannot collide with a private mode someone allocates
/// later.
pub const SENTINEL_MODE: u32 = 9_876_543;

// The sentinel's whole value is that no terminal can implement it. These hold the
// number to that promise at compile time.
const _: () = assert!(SENTINEL_MODE > 1_000_000);
const _: () = assert!(SENTINEL_MODE > u16::MAX as u32);

/// Asks a DEC private mode for its state: `CSI ? <mode> $ p` (DECRQM).
#[must_use]
pub fn query(mode: u32) -> String {
    format!("{CSI}?{mode}$p")
}

/// What DECRPM reported about a mode.
///
/// All five values matter, and so does their absence: no reply means the terminal
/// does not implement DECRQM at all, or something in between swallowed it, which
/// is not the same as `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeState {
    /// `0` — DECRQM understood, this mode unknown.
    NotRecognized,
    /// `1` — implemented and currently on.
    Set,
    /// `2` — implemented and currently off.
    Reset,
    /// `3` — always on; the reset sequence will not turn it off.
    PermanentlySet,
    /// `4` — always off; the set sequence will not turn it on. Also what iTerm2
    /// says about modes that do not exist, which is why the sentinel exists.
    PermanentlyReset,
    /// A value DECRPM does not define. Kept rather than folded into one of the
    /// others, because guessing is how a probe starts lying.
    Undefined(u32),
}

impl ModeState {
    /// Decodes a DECRPM value.
    #[must_use]
    pub fn from_value(value: u32) -> Self {
        match value {
            0 => Self::NotRecognized,
            1 => Self::Set,
            2 => Self::Reset,
            3 => Self::PermanentlySet,
            4 => Self::PermanentlyReset,
            other => Self::Undefined(other),
        }
    }

    /// The DECRPM value this state is spelled with, for reporting.
    #[must_use]
    pub fn value(self) -> u32 {
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

    /// Whether this state means the terminal *has* the capability.
    ///
    /// `1`, `2` and `3` are all yes: a mode that is implemented but currently off
    /// is still implemented, which is what a capability check asks. `0` and `4`
    /// are both no — `0` because the terminal has never heard of the mode, and
    /// `4` because it has heard of it and will never honour it. An undefined
    /// value is not an answer at all.
    #[must_use]
    pub fn support(self) -> Support {
        match self {
            Self::Set | Self::Reset | Self::PermanentlySet => Support::Yes,
            Self::NotRecognized | Self::PermanentlyReset => Support::No,
            Self::Undefined(_) => Support::Unknown,
        }
    }

    /// Whether the mode is on right now.
    #[must_use]
    pub fn enabled(self) -> bool {
        matches!(self, Self::Set | Self::PermanentlySet)
    }

    /// Whether the terminal knows what the mode is.
    ///
    /// `4` counts: a terminal that says "permanently reset" has recognised the
    /// mode and told us it will never honour it, which is far more useful than
    /// silence.
    #[must_use]
    pub fn recognized(self) -> bool {
        !matches!(self, Self::NotRecognized | Self::Undefined(_))
    }

    /// Whether the matching `CSI ? <mode> h` / `l` will actually change anything.
    #[must_use]
    pub fn settable(self) -> bool {
        matches!(self, Self::Set | Self::Reset)
    }
}

/// Parses a DECRPM report — `CSI ? 2026 ; 2 $ y` — out of a `CSI`-shaped reply.
///
/// `params` is the raw parameter text with the private prefix kept and
/// intermediates dropped, exactly as [`Reply::Csi`] delivers it, so a report for
/// mode 2026 arrives as `("?2026;2", 'y')`.
///
/// The `?` is not decoration: without it this is a report about an *ANSI* mode,
/// which is a different number space entirely, and mode 4 means insert/replace
/// rather than anything to do with private modes.
///
/// The mode number is returned alongside the state, because reports about other
/// modes arrive on the same stream and must not be mistaken for the one asked
/// about — telling the sentinel's answer from a real mode's depends on it. Both
/// numbers are parsed as `u32`: [`SENTINEL_MODE`] would not survive a `u16`, and
/// neither would a DECRPM value a terminal invents beyond one.
#[must_use]
pub fn parse_report(params: &str, final_byte: char) -> Option<(u32, ModeState)> {
    if final_byte != 'y' {
        return None;
    }
    let body = params.strip_prefix('?')?;
    let (mode, value) = body.split_once(';')?;
    let mode = mode.trim().parse::<u32>().ok()?;
    let value = value.trim().parse::<u32>().ok()?;
    Some((mode, ModeState::from_value(value)))
}

/// Picks a DECRPM report out of a reply, whatever mode it is about.
#[must_use]
pub fn report_from_reply(reply: &Reply) -> Option<(u32, ModeState)> {
    let Reply::Csi { params, final_byte } = reply else {
        return None;
    };
    parse_report(params, *final_byte)
}

/// Picks the state for `wanted` out of a reply, ignoring reports about other
/// modes.
#[must_use]
pub fn state_for(reply: &Reply, wanted: u32) -> Option<ModeState> {
    match report_from_reply(reply) {
        Some((mode, state)) if mode == wanted => Some(state),
        _ => None,
    }
}

/// What the sentinel established about this terminal's DECRQM answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// The terminal ignored the impossible mode, as it should. Answers about real
    /// modes mean what they say.
    Trusted,
    /// The terminal answered the impossible mode. Every answer carrying this same
    /// value is uninformative, because the terminal produces it for questions it
    /// cannot have an answer to.
    Compromised(ModeState),
    /// Nothing came back at all, barrier included, so DECRQM was never exercised.
    Unknown,
}

/// The sentinel probe's raw result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sentinel {
    /// What the terminal said about [`SENTINEL_MODE`], if anything.
    pub state: Option<ModeState>,
    /// Whether the Primary Device Attributes barrier came back.
    pub answered_barrier: bool,
}

impl Sentinel {
    /// What this licenses.
    #[must_use]
    pub fn trust(self) -> Trust {
        match self.state {
            Some(state) => Trust::Compromised(state),
            // Silence about a mode that cannot exist is the correct behaviour,
            // and the barrier proves the terminal had its chance to misbehave.
            None if self.answered_barrier => Trust::Trusted,
            None => Trust::Unknown,
        }
    }

    /// Whether a real mode's answer carries information.
    ///
    /// The comparison is against the sentinel's exact value rather than being
    /// all-or-nothing: on iTerm2 the sentinel comes back `4`, which makes every
    /// `4` uninformative — but a `2` from the same terminal is still a real
    /// answer, because it is not the answer the terminal gives to nonsense.
    #[must_use]
    pub fn informative(self, state: ModeState) -> bool {
        self.state != Some(state)
    }

    /// One sentence for the report, or `None` when there is nothing to warn about.
    #[must_use]
    pub fn warning(self) -> Option<String> {
        match self.trust() {
            Trust::Trusted | Trust::Unknown => None,
            Trust::Compromised(state) => Some(format!(
                "This terminal answered DECRQM for mode {SENTINEL_MODE}, which does not exist, \
                 with {} ({}). Every DECRQM answer of {} in this run therefore carries no \
                 information — iTerm2 behaves this way. Rows relying on one are marked unreliable.",
                state.value(),
                state.label(),
                state.value()
            )),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_query_matches_the_specification() {
        assert_eq!(query(2026), "\u{1b}[?2026$p");
        assert_eq!(query(1049), "\u{1b}[?1049$p");
        assert_eq!(query(2027), "\u{1b}[?2027$p");
        assert_eq!(query(2031), "\u{1b}[?2031$p");
        assert_eq!(query(SENTINEL_MODE), "\u{1b}[?9876543$p");
    }

    #[test]
    fn the_sentinel_mode_cannot_be_confused_with_a_real_one() {
        // Outside u16 entirely, so no private mode anyone allocates later can
        // collide with it — and so it cannot be represented by a u16 mode type.
        assert!(SENTINEL_MODE > u32::from(u16::MAX));
    }

    #[test]
    fn parses_a_mode_report() {
        assert_eq!(parse_report("?2026;2", 'y'), Some((2026, ModeState::Reset)));
        assert_eq!(parse_report("?1049;1", 'y'), Some((1049, ModeState::Set)));
        assert_eq!(
            parse_report("?2027;3", 'y'),
            Some((2027, ModeState::PermanentlySet))
        );
        assert_eq!(
            parse_report("?9876543;4", 'y'),
            Some((SENTINEL_MODE, ModeState::PermanentlyReset)),
            "the sentinel's mode number must survive parsing"
        );
        // Some terminals pad with spaces.
        assert_eq!(parse_report("? 2026 ; 2 ", 'y'), Some((2026, ModeState::Reset)));
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
            assert_eq!(parse_report(&params, 'y'), Some((2027, expected)), "{params}");
        }
    }

    #[test]
    fn refuses_what_is_not_a_private_mode_report() {
        // Without the `?` this is an ANSI mode, a different number space.
        assert_eq!(parse_report("2026;2", 'y'), None);
        // A cursor position report has the wrong final byte.
        assert_eq!(parse_report("?2026;2", 'R'), None);
        assert_eq!(parse_report("?2026", 'y'), None);
        assert_eq!(parse_report("?x;y", 'y'), None);
        assert_eq!(parse_report("", 'y'), None);
    }

    #[test]
    fn a_report_is_matched_by_mode_number_not_by_position() {
        // Reports arrive on one stream and may come back in either order, so they
        // can only be told apart by the number they carry.
        let real = Reply::Csi {
            params: "?2027;2".to_owned(),
            final_byte: 'y',
        };
        let control = Reply::Csi {
            params: format!("?{SENTINEL_MODE};0"),
            final_byte: 'y',
        };
        assert_eq!(state_for(&real, 2027), Some(ModeState::Reset));
        assert_eq!(state_for(&real, SENTINEL_MODE), None);
        assert_eq!(
            state_for(&control, SENTINEL_MODE),
            Some(ModeState::NotRecognized)
        );
        assert_eq!(state_for(&control, 2027), None);
        // Anything that is not a CSI reply carries no report.
        assert_eq!(state_for(&Reply::Osc("11;rgb:0/0/0".to_owned()), 2027), None);
    }

    #[test]
    fn an_undefined_value_is_kept_as_itself() {
        assert_eq!(
            parse_report("?2026;9", 'y'),
            Some((2026, ModeState::Undefined(9)))
        );
        assert_eq!(ModeState::Undefined(9).value(), 9);
        assert_eq!(ModeState::Undefined(9).support(), Support::Unknown);
    }

    #[test]
    fn a_value_too_large_for_a_u16_is_kept_rather_than_dropped() {
        // The drift the two former copies had: one parsed DECRPM values as u16,
        // so a terminal answering anything larger produced no report at all
        // rather than an undefined one.
        assert_eq!(
            parse_report("?2026;70000", 'y'),
            Some((2026, ModeState::Undefined(70_000)))
        );
    }

    #[test]
    fn zero_and_four_are_both_no() {
        // The correction that matters: `4` is not "knows the mode, firmly off",
        // because iTerm2 says `4` about modes it has never heard of.
        assert_eq!(ModeState::NotRecognized.support(), Support::No);
        assert_eq!(ModeState::PermanentlyReset.support(), Support::No);
    }

    #[test]
    fn one_two_and_three_are_all_yes() {
        // A mode that is implemented but currently off is still implemented.
        assert_eq!(ModeState::Set.support(), Support::Yes);
        assert_eq!(ModeState::Reset.support(), Support::Yes);
        assert_eq!(ModeState::PermanentlySet.support(), Support::Yes);
    }

    #[test]
    fn every_decrpm_value_round_trips() {
        for value in 0..=4 {
            assert_eq!(ModeState::from_value(value).value(), value);
        }
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
    fn only_the_on_states_are_enabled() {
        assert!(ModeState::Set.enabled());
        assert!(ModeState::PermanentlySet.enabled());
        assert!(!ModeState::Reset.enabled());
        assert!(!ModeState::PermanentlyReset.enabled());
        assert!(!ModeState::NotRecognized.enabled());
    }

    #[test]
    fn every_state_has_a_distinct_label() {
        let labels = [
            ModeState::NotRecognized,
            ModeState::Set,
            ModeState::Reset,
            ModeState::PermanentlySet,
            ModeState::PermanentlyReset,
            ModeState::Undefined(7),
        ]
        .map(ModeState::label);
        let mut seen = labels.to_vec();
        seen.sort_unstable();
        let total = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), total, "two states share a label");
    }

    #[test]
    fn a_silent_sentinel_behind_an_answered_barrier_is_trustworthy() {
        let sentinel = Sentinel {
            state: None,
            answered_barrier: true,
        };
        assert_eq!(sentinel.trust(), Trust::Trusted);
        assert!(sentinel.warning().is_none());
        // Every real answer is informative on such a terminal.
        for value in 0..=4 {
            assert!(sentinel.informative(ModeState::from_value(value)));
        }
    }

    #[test]
    fn a_sentinel_that_answered_compromises_only_its_own_value() {
        // iTerm2: the impossible mode comes back `4`.
        let sentinel = Sentinel {
            state: Some(ModeState::PermanentlyReset),
            answered_barrier: true,
        };
        assert_eq!(
            sentinel.trust(),
            Trust::Compromised(ModeState::PermanentlyReset)
        );
        assert!(
            !sentinel.informative(ModeState::PermanentlyReset),
            "a 4 from this terminal is the answer it gives to nonsense"
        );
        assert!(
            sentinel.informative(ModeState::Reset),
            "a 2 is still a real answer — it is not what nonsense produces"
        );
        let warning = sentinel.warning().unwrap();
        assert!(warning.contains("9876543"), "{warning}");
        assert!(warning.contains("iTerm2"), "{warning}");
    }

    #[test]
    fn a_sentinel_that_never_answered_anything_licenses_nothing() {
        let sentinel = Sentinel {
            state: None,
            answered_barrier: false,
        };
        assert_eq!(sentinel.trust(), Trust::Unknown);
        assert!(sentinel.warning().is_none());
    }
}
