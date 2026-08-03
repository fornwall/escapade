//! Being told when the colours change: DEC private mode 2031.
//!
//! ```text
//! →  CSI ? 2031 h              start reporting colour-scheme changes
//! →  CSI ? 2031 l              stop
//! →  CSI ? 996 n               ask what the scheme is right now (DSR)
//! ←  CSI ? 997 ; 1 n           dark
//! ←  CSI ? 997 ; 2 n           light
//! ```
//!
//! The mode is a *subscription*. While it is set the terminal sends one
//! unsolicited `CSI ? 997 ; Ps n` every time the user switches theme — including
//! when the switch comes from the operating system following its own light/dark
//! schedule, which is the case no application can otherwise notice. `CSI ? 996 n`
//! asks the same question on demand and is answered in the same shape.
//!
//! # Why this is not just another colour query
//!
//! `OSC 11` reports the background *colour*; mode 2031 reports the *scheme*. They
//! usually agree, and when they disagree the scheme is the one that answers the
//! question being asked: a terminal can be set to a light theme over a dark
//! translucent background, where the arithmetic in [`super::value`] says dark and
//! the user is looking at a light window. See [`super::report::verdict`], which
//! prefers this whenever it is available and says which basis it used.
//!
//! # Why the mode has to be asked about before it is trusted
//!
//! A terminal that has never heard of a private mode discards the set sequence in
//! silence, so `CSI ? 2031 h` never fails and never confirms. The only way to tell
//! subscription from wishful thinking is DECRQM — and a DECRQM answer is worthless
//! on its own, because iTerm2 answers modes that do not exist. So this module
//! reuses [`crate::decrqm`], sentinel and all, rather than carrying its own copy
//! of the same five values.
//!
//! # Why it is switched off again on every path out
//!
//! A mode is terminal state, not output: it outlives the process that set it.
//! Leaving 2031 on would have the terminal delivering `CSI ? 997 ; 1 n` to
//! whatever ran next — the user's shell, which prints it as garbage. [`stop`] is
//! therefore called from the cleanup path in [`super::watch`], including after an
//! error part way through.

use std::time::Duration;

use anyhow::Result;

use crate::{
    decrqm::{self, ModeState, SENTINEL_MODE, Sentinel},
    features::Support,
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term},
};

/// How long to wait for the terminal to answer.
///
/// A full second, matching every other probe in this project: a slow ssh link
/// must not be reported as a terminal that does not implement the mode.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// The DEC private mode that turns colour-scheme reporting on.
pub const MODE: u32 = 2031;

/// The DSR request that asks for the scheme once: `CSI ? 996 n`.
pub const REQUEST_PS: u16 = 996;

/// The report both the subscription and the request answer with: `CSI ? 997 …`.
///
/// Note that it is **not** 996. The request and the report are deliberately
/// different numbers, which is what keeps a report from being mistaken for
/// another program's request echoed back.
pub const REPORT_PS: u16 = 997;

/// Which way round the terminal's colours are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// Light text on a dark background.
    Dark,
    /// Dark text on a light background.
    Light,
}

impl Scheme {
    /// Decodes the parameter of a `CSI ? 997 ; Ps n` report.
    ///
    /// `1` is dark and `2` is light. Anything else is not decoded to a guess:
    /// a third value would be a protocol this code does not know, and reporting
    /// it as light because it is not 1 is how a probe starts lying.
    #[must_use]
    pub fn from_value(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Dark),
            2 => Some(Self::Light),
            _ => None,
        }
    }

    /// The parameter this scheme is spelled with.
    #[must_use]
    pub fn value(self) -> u32 {
        match self {
            Self::Dark => 1,
            Self::Light => 2,
        }
    }

    /// Whether this is the dark scheme.
    #[must_use]
    pub fn is_dark(self) -> bool {
        self == Self::Dark
    }

    /// Stable lowercase name, for reports and machine output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

/// Starts the subscription: `CSI ? 2031 h`.
#[must_use]
pub fn enable() -> String {
    format!("{CSI}?{MODE}h")
}

/// Ends it: `CSI ? 2031 l`.
#[must_use]
pub fn disable() -> String {
    format!("{CSI}?{MODE}l")
}

/// Asks what the scheme is right now: `CSI ? 996 n`.
///
/// Independent of the subscription — a terminal may answer this without
/// implementing the mode, and the reply is the same `CSI ? 997 ; Ps n` either way.
#[must_use]
pub fn request() -> String {
    format!("{CSI}?{REQUEST_PS}n")
}

/// Reads a colour-scheme report out of a reply: `CSI ? 997 ; Ps n`.
///
/// Returns `None` for anything else, including a report carrying a parameter
/// neither 1 nor 2, and including the same numbers without the `?` — without the
/// private prefix this is an ANSI DSR about something else entirely.
#[must_use]
pub fn parse_scheme_report(reply: &Reply) -> Option<Scheme> {
    let Reply::Csi { params, final_byte } = reply else {
        return None;
    };
    if *final_byte != 'n' {
        return None;
    }
    let body = params.strip_prefix('?')?;
    let (ps, value) = body.split_once(';')?;
    if ps.trim().parse::<u16>().ok()? != REPORT_PS {
        return None;
    }
    Scheme::from_value(value.trim().parse::<u32>().ok()?)
}

/// What was learned about change notification, and whether it is now running.
///
/// `Copy`, because [`super::watch`] hands it to the loop and still needs it
/// afterwards to know whether there is a subscription to take down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct State {
    /// What DECRQM said about mode 2031, if it said anything.
    pub mode: Option<ModeState>,
    /// What the sentinel established about this terminal's DECRQM answers.
    pub sentinel: Sentinel,
    /// The scheme the terminal reported while being asked, if it reported one.
    pub scheme: Option<Scheme>,
    /// Whether `CSI ? 2031 h` was actually sent, and so has to be undone.
    pub enabled: bool,
    /// Whether the Primary Device Attributes barrier came back.
    pub answered_barrier: bool,
    /// A multiplexer between here and the terminal, if any.
    pub multiplexer: Option<Multiplexer>,
}

impl State {
    /// Whether this terminal will report colour-scheme changes.
    ///
    /// [`Support::Yes`] needs a DECRQM answer that is *informative* — one the
    /// sentinel has not shown to be the terminal's answer to nonsense — saying the
    /// mode exists. A scheme report on its own is not enough: `CSI ? 996 n` is a
    /// separate request, and a terminal can answer it without implementing the
    /// subscription, in which case asking again is the only way to find out
    /// anything and that is polling by another name.
    ///
    /// [`Support::No`] needs the same informative answer saying the mode does not
    /// exist. Everything else — silence, a compromised sentinel, a multiplexer in
    /// the way — is [`Support::Unknown`], because none of them distinguish a
    /// terminal that will not report from one that was never asked.
    #[must_use]
    pub fn support(self) -> Support {
        if self.multiplexer.is_some() {
            return Support::Unknown;
        }
        match self.mode {
            Some(state) if self.sentinel.informative(state) => state.support(),
            None if self.answered_barrier => Support::No,
            // An answer the sentinel has discredited, or no answer and no
            // barrier to say the terminal ever had its chance. Different
            // situations, and neither one establishes anything.
            Some(_) | None => Support::Unknown,
        }
    }

    /// One paragraph on how changes will be noticed, and why.
    ///
    /// Always says something. The user watching for an instant response has no
    /// other way to tell an idle subscription from a poll loop, since both print
    /// the same lines when a colour finally moves.
    #[must_use]
    pub fn explain(self, poll: Duration) -> String {
        let seconds = poll.as_secs_f64();
        match self.support() {
            Support::Yes => format!(
                "DEC mode {MODE} is set, so the terminal sends a report the moment the scheme \
                 changes and nothing is re-queried in the meantime."
            ),
            Support::No => format!(
                "This terminal answered DECRQM for mode {MODE} with \"not recognised\", so there \
                 is nothing to subscribe to and the colours are re-queried every {seconds:.0}s \
                 instead. A change is noticed up to that late."
            ),
            Support::Unknown => match self.multiplexer {
                Some(multiplexer) => format!(
                    "Inside {} the device-attributes barrier is answered by the multiplexer \
                     itself, so nothing here can establish whether the terminal beyond it \
                     implements mode {MODE}. The subscription was requested anyway and the \
                     colours are also re-queried every {seconds:.0}s, so a change is noticed \
                     either way.",
                    multiplexer.name()
                ),
                None => format!(
                    "Nothing conclusive came back about mode {MODE}, so the subscription was \
                     requested optimistically and the colours are re-queried every {seconds:.0}s \
                     as well. Whichever works first reports the change."
                ),
            },
        }
    }
}

/// Asks about mode 2031, asks for the current scheme, and subscribes if it can.
///
/// One round trip carries all of it: the sentinel, the real mode, the scheme
/// request and the barrier go out together, and the replies are read back until
/// the barrier proves the terminal has had its chance.
///
/// The subscription is then requested unless DECRQM said outright that the mode
/// does not exist — an optimistic `CSI ? 2031 h` costs one write and is discarded
/// by a terminal that does not know it, whereas skipping it whenever support was
/// merely *unproven* would mean never subscribing inside tmux, where nothing can
/// be proven at all.
///
/// The terminal must already be in raw mode; the caller owns that, because it also
/// owns taking it back down.
///
/// # Errors
///
/// Fails if the terminal cannot be written to. Silence is not an error, it is a
/// result.
pub fn probe(term: &mut Term, input: &mut InputReader) -> Result<State> {
    let mut state = State {
        multiplexer: Multiplexer::detect(),
        ..State::default()
    };

    // The sentinel goes first so its answer is in hand before any real mode's is
    // interpreted. See `crate::decrqm` for what it defends against.
    term.write(&decrqm::query(SENTINEL_MODE))?;
    term.write(&decrqm::query(MODE))?;
    term.write(&request())?;
    term.write_now(&super::query::barrier())?;

    let deadline = std::time::Instant::now() + REPLY_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        // EOF on stdin means there is nobody to answer; that is silence, not a
        // failure.
        let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
            break;
        };
        if let Some((mode, mode_state)) = decrqm::report_from_reply(&reply) {
            if mode == SENTINEL_MODE {
                state.sentinel.state = Some(mode_state);
            } else if mode == MODE {
                state.mode = Some(mode_state);
            }
            continue;
        }
        if let Some(scheme) = parse_scheme_report(&reply) {
            state.scheme = Some(scheme);
            continue;
        }
        if matches!(&reply, Reply::Csi { final_byte: 'c', .. }) {
            // Everything sent before the barrier has been processed, so anything
            // still missing is missing on purpose.
            state.answered_barrier = true;
            state.sentinel.answered_barrier = true;
            break;
        }
    }

    if state.support() != Support::No {
        term.write_now(&enable())?;
        state.enabled = true;
    }
    Ok(state)
}

/// Takes the subscription back down: `CSI ? 2031 l`.
///
/// A no-op when nothing was subscribed, so the caller can put this on its cleanup
/// path unconditionally without having to know how the run went.
///
/// # Errors
///
/// Fails if the terminal cannot be written to — which the caller is expected to
/// ignore, since by then it is already on its way out.
pub fn stop(term: &mut Term, enabled: bool) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    term.write_now(&disable())
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A `CSI`-shaped reply, as [`InputReader`] delivers one.
    fn csi(params: &str, final_byte: char) -> Reply {
        Reply::Csi {
            params: params.to_owned(),
            final_byte,
        }
    }

    // -- the sequences ------------------------------------------------------

    #[test]
    fn the_sequences_match_the_specification() {
        assert_eq!(enable(), "\u{1b}[?2031h");
        assert_eq!(disable(), "\u{1b}[?2031l");
        assert_eq!(request(), "\u{1b}[?996n");
        // The mode query is DECRQM, borrowed rather than reimplemented.
        assert_eq!(decrqm::query(MODE), "\u{1b}[?2031$p");
    }

    #[test]
    fn the_request_and_the_report_are_different_numbers() {
        // Easy to conflate, and conflating them means a program's own request
        // could be mistaken for a report. 996 asks, 997 answers.
        assert_ne!(REQUEST_PS, REPORT_PS);
        assert_eq!(REQUEST_PS, 996);
        assert_eq!(REPORT_PS, 997);
    }

    // -- the report ---------------------------------------------------------

    #[test]
    fn parses_both_schemes() {
        assert_eq!(parse_scheme_report(&csi("?997;1", 'n')), Some(Scheme::Dark));
        assert_eq!(parse_scheme_report(&csi("?997;2", 'n')), Some(Scheme::Light));
    }

    #[test]
    fn tolerates_the_padding_some_terminals_add() {
        assert_eq!(parse_scheme_report(&csi("? 997 ; 1 ", 'n')), Some(Scheme::Dark));
    }

    #[test]
    fn the_private_prefix_is_required() {
        // Without the `?` this is an ANSI DSR, a different number space entirely.
        assert_eq!(parse_scheme_report(&csi("997;1", 'n')), None);
    }

    #[test]
    fn refuses_what_is_not_a_scheme_report() {
        assert_eq!(parse_scheme_report(&csi("?997;1", 'y')), None, "wrong final byte");
        assert_eq!(parse_scheme_report(&csi("?996;1", 'n')), None, "the request, not the report");
        assert_eq!(parse_scheme_report(&csi("?997", 'n')), None, "no value");
        assert_eq!(parse_scheme_report(&csi("?997;x", 'n')), None, "unparseable value");
        assert_eq!(parse_scheme_report(&Reply::Osc("11;rgb:0/0/0".to_owned())), None);
        assert_eq!(parse_scheme_report(&csi("?62;4", 'c')), None, "the barrier");
    }

    #[test]
    fn a_value_the_protocol_does_not_define_is_not_guessed_at() {
        // The failure that matters: folding an unknown value into "light"
        // because it is not 1 would answer a question nobody could answer.
        assert_eq!(parse_scheme_report(&csi("?997;0", 'n')), None);
        assert_eq!(parse_scheme_report(&csi("?997;3", 'n')), None);
        assert_eq!(Scheme::from_value(9), None);
    }

    #[test]
    fn the_scheme_round_trips_through_its_parameter() {
        for scheme in [Scheme::Dark, Scheme::Light] {
            assert_eq!(Scheme::from_value(scheme.value()), Some(scheme));
            assert_eq!(scheme.as_str(), scheme.as_str().to_ascii_lowercase());
        }
        assert!(Scheme::Dark.is_dark());
        assert!(!Scheme::Light.is_dark());
    }

    // -- the verdict --------------------------------------------------------

    /// A state with a trustworthy sentinel and the given DECRQM answer.
    fn with_mode(mode: Option<ModeState>) -> State {
        State {
            mode,
            sentinel: Sentinel {
                state: None,
                answered_barrier: true,
            },
            answered_barrier: true,
            ..State::default()
        }
    }

    #[test]
    fn an_implemented_mode_is_a_yes_however_it_is_currently_set() {
        // Set, reset and permanently set all mean the terminal has the mode.
        for state in [
            ModeState::Set,
            ModeState::Reset,
            ModeState::PermanentlySet,
        ] {
            assert_eq!(with_mode(Some(state)).support(), Support::Yes, "{state:?}");
        }
    }

    #[test]
    fn a_mode_the_terminal_has_never_heard_of_is_a_no() {
        assert_eq!(
            with_mode(Some(ModeState::NotRecognized)).support(),
            Support::No
        );
        assert_eq!(
            with_mode(Some(ModeState::PermanentlyReset)).support(),
            Support::No
        );
    }

    #[test]
    fn silence_under_the_barrier_is_a_no_and_silence_without_it_is_not() {
        assert_eq!(with_mode(None).support(), Support::No);
        let unbarriered = State {
            answered_barrier: false,
            ..with_mode(None)
        };
        assert_eq!(unbarriered.support(), Support::Unknown);
    }

    #[test]
    fn an_answer_the_sentinel_has_discredited_carries_no_information() {
        // iTerm2's behaviour: it answers 4 for a mode that cannot exist, so a 4
        // about a real mode says nothing. It must not become a No.
        let compromised = State {
            mode: Some(ModeState::PermanentlyReset),
            sentinel: Sentinel {
                state: Some(ModeState::PermanentlyReset),
                answered_barrier: true,
            },
            answered_barrier: true,
            ..State::default()
        };
        assert_eq!(compromised.support(), Support::Unknown);
        // A different value from the same terminal is still a real answer.
        let informative = State {
            mode: Some(ModeState::Set),
            ..compromised
        };
        assert_eq!(informative.support(), Support::Yes);
    }

    #[test]
    fn a_multiplexer_voids_the_verdict_in_both_directions() {
        // tmux answers the barrier itself, so neither a yes nor a no can be
        // attributed to the terminal beyond it.
        for mode in [ModeState::Set, ModeState::NotRecognized] {
            let state = State {
                multiplexer: Some(Multiplexer::Tmux),
                ..with_mode(Some(mode))
            };
            assert_eq!(state.support(), Support::Unknown, "{mode:?}");
        }
    }

    #[test]
    fn a_scheme_report_alone_does_not_prove_the_subscription() {
        // `CSI ? 996 n` is a separate request. Answering it says the terminal
        // knows its own scheme, not that it will volunteer the next one.
        let state = State {
            scheme: Some(Scheme::Dark),
            ..with_mode(None)
        };
        assert_ne!(state.support(), Support::Yes);
    }

    // -- what the user is told ----------------------------------------------

    #[test]
    fn every_verdict_explains_itself_and_names_the_interval_when_polling() {
        let poll = Duration::from_secs(1);

        let yes = with_mode(Some(ModeState::Set)).explain(poll);
        assert!(yes.contains("2031"), "{yes}");

        let no = with_mode(Some(ModeState::NotRecognized)).explain(poll);
        assert!(no.contains("re-queried every 1s"), "{no}");

        let unknown = State {
            answered_barrier: false,
            ..with_mode(None)
        };
        assert!(unknown.explain(poll).contains("1s"), "{}", unknown.explain(poll));

        let multiplexed = State {
            multiplexer: Some(Multiplexer::Tmux),
            ..with_mode(Some(ModeState::Set))
        };
        let text = multiplexed.explain(poll);
        assert!(text.contains("tmux"), "{text}");
        assert!(text.contains("1s"), "{text}");
    }

    #[test]
    fn nothing_the_user_is_shown_contains_a_control_byte() {
        // These strings are printed into a terminal. An escape in one would be
        // executed rather than read.
        let poll = Duration::from_secs(1);
        for state in [
            with_mode(Some(ModeState::Set)),
            with_mode(Some(ModeState::NotRecognized)),
            with_mode(None),
            State {
                multiplexer: Some(Multiplexer::Tmux),
                ..with_mode(None)
            },
        ] {
            assert!(!state.explain(poll).contains('\u{1b}'));
        }
    }
}
