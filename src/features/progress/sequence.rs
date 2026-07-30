//! Building the progress sequence, and the two types that make it impossible to
//! build a malformed one.
//!
//! ```text
//! OSC 9 ; 4 ; <state> ; <progress> ST
//! ```
//!
//! One sequence, five states, one number. ConEmu invented it; everyone who
//! implemented it afterwards copied ConEmu's numbering:
//!
//! ```text
//! 0  remove the indicator
//! 1  set it to <progress> per cent
//! 2  error   — usually drawn red
//! 3  indeterminate — busy, total unknown
//! 4  paused (ConEmu, tmux, VTE, Ghostty) / warning (Windows Terminal, iTerm2)
//! ```
//!
//! Two types carry the whole burden of correctness. [`State`] is an enum, so a
//! state number outside 0–4 cannot be expressed. [`Percent`] is a `u8` that has
//! been proved to be 0–100, so a percentage outside the range cannot be expressed
//! either. Between them, the only thing that ever reaches [`format!`] is a
//! small integer — which is why no amount of hostile input can terminate the OSC
//! string early. See [`Report::sequence`].
//!
//! # Always terminate with ST
//!
//! Everything here ends the string with [`ST`] rather than [`crate::term::BEL`],
//! and that is not a stylistic choice. Every implementation accepts `ST`; **VTE
//! deliberately rejects `BEL`** for this sequence and this sequence only, on the
//! grounds that it has outgrown ConEmu and should be treated as a new thing. Since
//! Microsoft's own tutorial writes the sequence with `BEL`, a `BEL`-terminated
//! progress report is a common snippet that silently does nothing on every
//! VTE-based terminal.

use crate::term::{OSC, ST};

/// The OSC number ConEmu used, shared with iTerm2's notifications.
///
/// This is the collision that makes the feature awkward: `OSC 9 ; <text> ST` is
/// iTerm2's desktop notification, and `OSC 9 ; 4 ; …` is ConEmu's progress
/// report. The two are told apart only by what follows the first semicolon.
pub const NUMBER: u8 = 9;

/// ConEmu's sub-command for progress, i.e. the `4` in `OSC 9 ; 4 ; …`.
///
/// ConEmu defined nine `OSC 9 ; <n>` sub-commands — sleep, a GUI message box, tab
/// text, progress, wait for a keypress, run a GuiMacro, run a process, print an
/// environment variable, report the working directory. Progress is the one anybody
/// else implemented.
pub const SUBCOMMAND: u8 = 4;

/// The largest percentage the protocol defines.
pub const MAX_PERCENT: u8 = 100;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// What kind of progress to show: the `<state>` field.
///
/// An enum rather than the bare integer, so `OSC 9 ; 4 ; 7 ; 50 ST` cannot be
/// built at all. That matters more than it looks: Windows Terminal drops the whole
/// sequence when the state exceeds 4, and ConEmu's own source has two undocumented
/// states — `5` and `6`, "long run start" and "long run stop" — which take a *name*
/// rather than a number and which nothing else implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// `0` — take the indicator away. The only state that is also a cleanup.
    Remove,
    /// `1` — determinate progress at `<progress>` per cent.
    Set,
    /// `2` — something failed. Drawn red where the platform has a red.
    Error,
    /// `3` — working, total unknown. Drawn as a pulse or a marquee.
    Indeterminate,
    /// `4` — paused, or needing attention. Drawn yellow.
    ///
    /// The one state whose *name* forked: ConEmu, tmux, VTE, Ghostty and WezTerm
    /// call it paused; Windows Terminal and iTerm2 call it warning. The rendering
    /// is the same either way.
    Warning,
}

impl State {
    /// Every state, in wire order. Handy for demos and exhaustiveness tests.
    pub const ALL: [Self; 5] = [
        Self::Remove,
        Self::Set,
        Self::Error,
        Self::Indeterminate,
        Self::Warning,
    ];

    /// The `<state>` field as it goes on the wire.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Remove => 0,
            Self::Set => 1,
            Self::Error => 2,
            Self::Indeterminate => 3,
            Self::Warning => 4,
        }
    }

    /// Short name, as spelled by `--state`. `Remove` is spelled `--clear`,
    /// because removing the indicator is not a state anybody wants to *show*.
    #[must_use]
    pub fn flag(self) -> &'static str {
        match self {
            Self::Remove => "clear",
            Self::Set => "normal",
            Self::Error => "error",
            Self::Indeterminate => "indeterminate",
            Self::Warning => "warning",
        }
    }

    /// One clause on what the terminal is being asked to do.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Remove => "remove the indicator",
            Self::Set => "determinate progress, at the given percentage",
            Self::Error => "error — usually a red bar",
            Self::Indeterminate => "busy, with no known total — usually a pulse",
            Self::Warning => "paused or needing attention — usually a yellow bar",
        }
    }

    /// How this state treats the `<progress>` field.
    ///
    /// Straight from ConEmu's table, which is the only prose specification this
    /// protocol has:
    ///
    /// ```text
    /// st = 0: remove progress.
    /// st = 1: set progress value to pr (number, 0-100).
    /// st = 2: set error state in progress on Windows 7 taskbar, pr is optional.
    /// st = 3: set indeterminate state.
    /// st = 4: set paused state, pr is optional.
    /// ```
    ///
    /// Two footnotes, because the table is not the whole truth. ConEmu says nothing
    /// about `pr` for state 3; the well-known "`pr` is ignored" wording is Windows
    /// Terminal's documentation, and Windows Terminal, iTerm2 and Ghostty do all
    /// ignore it — while VTE forces the value to 0 instead. And ConEmu's *code*
    /// treats `pr` as optional for state 1 too, defaulting it to 0, which
    /// contradicts its own table; `Required` here is the honest description of what
    /// the caller must supply for the request to mean anything.
    #[must_use]
    pub fn percent_use(self) -> PercentUse {
        match self {
            Self::Remove | Self::Indeterminate => PercentUse::Ignored,
            Self::Set => PercentUse::Required,
            Self::Error | Self::Warning => PercentUse::Optional,
        }
    }
}

impl From<crate::cli::ProgressState> for State {
    fn from(state: crate::cli::ProgressState) -> Self {
        match state {
            crate::cli::ProgressState::Normal => Self::Set,
            crate::cli::ProgressState::Error => Self::Error,
            crate::cli::ProgressState::Indeterminate => Self::Indeterminate,
            crate::cli::ProgressState::Warning => Self::Warning,
        }
    }
}

/// What a state does with the `<progress>` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PercentUse {
    /// The state is meaningless without one.
    Required,
    /// Honoured when present; the last known value stands when it is not.
    Optional,
    /// Discarded by the terminal, whatever it says.
    Ignored,
}

impl PercentUse {
    /// Short label for the reference table in `demo` and `probe`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
            Self::Ignored => "ignored",
        }
    }
}

// ---------------------------------------------------------------------------
// Percent
// ---------------------------------------------------------------------------

/// A percentage that has been proved to be 0–100.
///
/// The protocol's range is 0–100 and nothing else, so this is the type that makes
/// "150 per cent" unrepresentable rather than merely unlikely.
///
/// It has to be clamped on this side, because terminals do four different things
/// with an out-of-range value: Windows Terminal and Ghostty clamp it to 100, tmux
/// and VTE **reject the entire sequence**, Konsole drops the update, and WezTerm
/// clamps 150 to 100 but turns 300 into 0 because it parses into a `u8` first. So
/// sending `150` does not mean "full" — on half of these it means "no update at
/// all", which looks exactly like a hang.
///
/// [`Percent::clamping`] is the only constructor, because clamping is the policy:
/// it returns the flag saying whether it had to, and every caller reports that
/// rather than deciding for itself. A second, rejecting constructor would just be a
/// second answer to a question this crate has already answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Percent(u8);

impl Percent {
    /// Zero per cent.
    pub const ZERO: Self = Self(0);

    /// One hundred per cent.
    pub const FULL: Self = Self(MAX_PERCENT);

    /// Builds a percentage, clamping into range, and says whether it had to.
    ///
    /// The forgiving constructor, for values arriving on a stream: `--watch` is
    /// fed by build logs, and a log that says `101%` should move the bar to the
    /// end rather than kill the process.
    #[must_use]
    pub fn clamping(value: u64) -> (Self, bool) {
        if value > u64::from(MAX_PERCENT) {
            (Self::FULL, true)
        } else {
            // The comparison above bounds `value` at 100, so this cannot fail;
            // `unwrap_or` keeps it provable rather than asserted.
            (Self(u8::try_from(value).unwrap_or(MAX_PERCENT)), false)
        }
    }

    /// The value, 0–100.
    #[must_use]
    pub fn get(self) -> u8 {
        self.0
    }
}

impl std::fmt::Display for Percent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// A whole report
// ---------------------------------------------------------------------------

/// One progress report: a state, and a percentage when the state has room for
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Which state.
    pub state: State,
    /// The percentage, when there is one to send.
    pub percent: Option<Percent>,
}

impl Report {
    /// Determinate progress: `OSC 9 ; 4 ; 1 ; <percent> ST`.
    #[must_use]
    pub fn set(percent: Percent) -> Self {
        Self {
            state: State::Set,
            percent: Some(percent),
        }
    }

    /// Removes the indicator: `OSC 9 ; 4 ; 0 ; 0 ST`.
    #[must_use]
    pub fn clear() -> Self {
        Self {
            state: State::Remove,
            percent: None,
        }
    }

    /// A report in any state, with an optional percentage.
    ///
    /// The percentage is dropped for the states that ignore it — [`State::Remove`]
    /// and [`State::Indeterminate`] — so that what goes on the wire matches what
    /// the terminal will act on. A `Report` is therefore *canonical*: two requests
    /// that mean the same thing compare equal, which is what lets [`super::watch`]
    /// suppress a redundant redraw with `==`. Because the dropped percentage is
    /// gone, saying so out loud is [`notes_for`]'s job, not this type's.
    #[must_use]
    pub fn new(state: State, percent: Option<Percent>) -> Self {
        let percent = match state.percent_use() {
            PercentUse::Ignored => None,
            PercentUse::Required | PercentUse::Optional => percent,
        };
        Self { state, percent }
    }

    /// The escape sequence: `OSC 9 ; 4 ; <state> ; <progress> ST`.
    ///
    /// The `<progress>` field is written even in the states that ignore it, because
    /// the four-field form is the shape implementations were built against — and
    /// one of them, Konsole, checks the field count before acting on a state at
    /// all. Sending a `0` the terminal is documented to ignore costs two bytes and
    /// cannot be misread.
    ///
    /// A state with an *optional* percentage and none given is the one case where
    /// the field is genuinely omitted, because a `0` there would not be ignored: it
    /// would move a red bar back to the start. See [`Report::omits_percent`].
    ///
    /// The terminator is always `ST`. VTE rejects a `BEL`-terminated progress
    /// report on purpose, so `BEL` is the one choice here that can silently lose
    /// the sequence.
    ///
    /// Nothing in here comes from a string. `State::code` returns a `u8` from a
    /// closed set and `Percent` holds a `u8` bounded at 100, so every byte of the
    /// payload is a digit or one of the two semicolons this function writes. There
    /// is no input that could smuggle an `ESC`, a `BEL` or a stray `;` into it.
    #[must_use]
    pub fn sequence(self) -> String {
        let state = self.state.code();
        match self.percent {
            Some(percent) => format!("{OSC}{NUMBER};{SUBCOMMAND};{state};{percent}{ST}"),
            None if self.omits_percent() => format!("{OSC}{NUMBER};{SUBCOMMAND};{state}{ST}"),
            None => format!("{OSC}{NUMBER};{SUBCOMMAND};{state};0{ST}"),
        }
    }

    /// Whether this report leaves the `<progress>` field out entirely.
    ///
    /// True only for a state that would *honour* a percentage if given one and was
    /// not given one, i.e. error and paused. Writing `0` there would set the value
    /// to zero rather than leave it alone — and on Windows Terminal an explicit `0`
    /// is indistinguishable from an absent field, so omitting it is the only way to
    /// ask for "leave the value where it is" at all.
    #[must_use]
    pub fn omits_percent(self) -> bool {
        self.percent.is_none() && self.state.percent_use() == PercentUse::Optional
    }

    /// One line describing what the user should expect to see.
    #[must_use]
    pub fn expectation(self) -> String {
        match (self.state, self.percent) {
            (State::Remove, _) => "the indicator disappears".to_owned(),
            (State::Set, Some(percent)) => format!("a bar at {percent}%"),
            (State::Set, None) => "a bar, with no value to draw".to_owned(),
            (State::Error, _) => "the bar turns red (or the dock icon does)".to_owned(),
            (State::Indeterminate, _) => "the bar pulses, with no position".to_owned(),
            (State::Warning, _) => "the bar turns yellow".to_owned(),
        }
    }
}

/// What is worth telling the user about a request, before it is canonicalized.
///
/// Takes the request rather than the [`Report`] because the interesting notes are
/// about the *difference* between the two: a percentage that was given and will not
/// be sent is invisible in the finished report, and it is exactly the sort of quiet
/// almost-working this program exists to make loud.
///
/// Three things get a note, and nothing else does:
///
/// * a percentage handed to a state that ignores it,
/// * determinate progress with no percentage, which cannot draw anything,
/// * error or paused with no percentage, where "absent" means four different things
///   in four different terminals.
#[must_use]
pub fn notes_for(state: State, requested: Option<Percent>) -> Vec<String> {
    let mut notes = Vec::new();
    match state.percent_use() {
        PercentUse::Ignored if requested.is_some() => notes.push(format!(
            "the {} state ignores the percentage, so it is not sent",
            state.flag()
        )),
        PercentUse::Required if requested.is_none() => notes.push(
            "determinate progress needs a percentage — without one the terminal has nothing to \
             draw"
                .to_owned(),
        ),
        PercentUse::Optional if requested.is_none() => notes.push(format!(
            "no percentage given, so the {} state is sent without the field — Windows Terminal \
             and tmux then keep the value they already had, but VTE and WezTerm read it as 0% \
             and iTerm2 wants a percentage for state 4 and does nothing without one. Pass one — \
             `escapade progress 60 --state {}` — to be sure",
            state.flag(),
            state.flag()
        )),
        _ => {}
    }
    notes
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests, but the attribute is kept for the
// helpers in here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The escape byte, spelled out so the assertions read as the bytes that go
    /// on the wire.
    const E: &str = "\u{1b}";

    // -- states -------------------------------------------------------------

    #[test]
    fn the_state_numbers_are_conemus() {
        assert_eq!(State::Remove.code(), 0);
        assert_eq!(State::Set.code(), 1);
        assert_eq!(State::Error.code(), 2);
        assert_eq!(State::Indeterminate.code(), 3);
        assert_eq!(State::Warning.code(), 4);
    }

    #[test]
    fn every_state_has_a_distinct_code_and_flag() {
        let mut codes: Vec<u8> = State::ALL.iter().map(|state| state.code()).collect();
        codes.sort_unstable();
        let total = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), total, "two states share a wire number");

        let mut flags: Vec<&str> = State::ALL.iter().map(|state| state.flag()).collect();
        flags.sort_unstable();
        let total = flags.len();
        flags.dedup();
        assert_eq!(flags.len(), total, "two states share a name");

        for state in State::ALL {
            assert!(!state.describe().is_empty(), "{state:?}");
        }
    }

    #[test]
    fn every_cli_state_maps_onto_a_wire_state() {
        use crate::cli::ProgressState;
        assert_eq!(State::from(ProgressState::Normal), State::Set);
        assert_eq!(State::from(ProgressState::Error), State::Error);
        assert_eq!(
            State::from(ProgressState::Indeterminate),
            State::Indeterminate
        );
        assert_eq!(State::from(ProgressState::Warning), State::Warning);
        // `--state` cannot name Remove; `--clear` does that.
        assert_eq!(State::Remove.flag(), "clear");
    }

    #[test]
    fn only_the_determinate_state_needs_a_percentage() {
        assert_eq!(State::Set.percent_use(), PercentUse::Required);
        assert_eq!(State::Error.percent_use(), PercentUse::Optional);
        assert_eq!(State::Warning.percent_use(), PercentUse::Optional);
        assert_eq!(State::Indeterminate.percent_use(), PercentUse::Ignored);
        assert_eq!(State::Remove.percent_use(), PercentUse::Ignored);
    }

    // -- percentages --------------------------------------------------------

    #[test]
    fn a_percentage_is_zero_to_one_hundred_and_nothing_else() {
        assert_eq!(Percent::ZERO.get(), 0);
        assert_eq!(Percent::FULL.get(), 100);
        assert_eq!(Percent::default(), Percent::ZERO);
        // Whatever comes in, what comes out is inside the protocol's range.
        for raw in 0..=u32::from(u8::MAX) + 1 {
            assert!(Percent::clamping(u64::from(raw)).0.get() <= MAX_PERCENT);
        }
    }

    #[test]
    fn clamping_saturates_at_one_hundred_and_says_it_did() {
        assert_eq!(Percent::clamping(0), (Percent::ZERO, false));
        assert_eq!(Percent::clamping(42).0.get(), 42);
        assert!(!Percent::clamping(100).1);
        assert_eq!(Percent::clamping(101), (Percent::FULL, true));
        // A number far past anything a u8 could hold must clamp rather than wrap.
        assert_eq!(Percent::clamping(1_000_000), (Percent::FULL, true));
        assert_eq!(Percent::clamping(u64::MAX), (Percent::FULL, true));
    }

    #[test]
    fn percentages_display_as_bare_digits() {
        assert_eq!(Percent::clamping(7).0.to_string(), "7");
        assert_eq!(Percent::FULL.to_string(), "100");
    }

    // -- sequences ----------------------------------------------------------

    #[test]
    fn determinate_progress_is_state_one() {
        assert_eq!(
            Report::set(Percent::clamping(42).0).sequence(),
            format!("{E}]9;4;1;42{E}\\"),
            "OSC 9 ; 4 ; 1 ; 42 ST"
        );
        assert_eq!(
            Report::set(Percent::ZERO).sequence(),
            format!("{E}]9;4;1;0{E}\\")
        );
        assert_eq!(
            Report::set(Percent::FULL).sequence(),
            format!("{E}]9;4;1;100{E}\\")
        );
    }

    #[test]
    fn clearing_is_state_zero_with_a_zero_value() {
        // The field is written even though it is ignored: a four-field form is
        // what every implementation was written against.
        assert_eq!(Report::clear().sequence(), format!("{E}]9;4;0;0{E}\\"));
        assert!(!Report::clear().omits_percent());
    }

    #[test]
    fn the_valueless_states_build_the_documented_forms() {
        // Error and paused omit the field, so the terminal keeps its value.
        assert_eq!(
            Report::new(State::Error, None).sequence(),
            format!("{E}]9;4;2{E}\\")
        );
        assert_eq!(
            Report::new(State::Warning, None).sequence(),
            format!("{E}]9;4;4{E}\\")
        );
        // Indeterminate ignores the field rather than honouring it, so a zero is
        // harmless and keeps the four-field shape.
        assert_eq!(
            Report::new(State::Indeterminate, None).sequence(),
            format!("{E}]9;4;3;0{E}\\")
        );
    }

    #[test]
    fn every_state_builds_a_sequence_for_every_percentage() {
        for state in State::ALL {
            for percent in [None, Some(Percent::ZERO), Some(Percent::FULL)] {
                let report = Report::new(state, percent);
                let sequence = report.sequence();
                assert!(sequence.starts_with(&format!("{E}]9;4;")), "{sequence:?}");
                assert!(sequence.ends_with(&format!("{E}\\")), "{sequence:?}");
                // Exactly one OSC introducer and one terminator, always.
                assert_eq!(sequence.matches(E).count(), 2, "{sequence:?}");
            }
        }
    }

    #[test]
    fn a_state_that_ignores_the_percentage_never_carries_one() {
        for state in [State::Remove, State::Indeterminate] {
            let requested = Some(Percent::clamping(55).0);
            let report = Report::new(state, requested);
            assert_eq!(report.percent, None, "{state:?} kept a percentage");
            assert!(!report.sequence().contains("55"));
            let notes = notes_for(state, requested);
            assert!(
                notes.iter().any(|note| note.contains("ignores")),
                "{notes:?}"
            );
            // Canonical, so the same request twice in a row is one update.
            assert_eq!(report, Report::new(state, None));
        }
    }

    #[test]
    fn a_state_that_honours_a_percentage_carries_the_one_it_was_given() {
        for state in [State::Set, State::Error, State::Warning] {
            let report = Report::new(state, Some(Percent::clamping(55).0));
            assert_eq!(report.sequence(), format!("{E}]9;4;{};55{E}\\", state.code()));
            assert!(!report.omits_percent());
        }
    }

    #[test]
    fn an_optional_percentage_is_omitted_rather_than_sent_as_zero() {
        // Sending 0 here would drag the bar back to the start instead of leaving
        // the last value alone.
        for state in [State::Error, State::Warning] {
            let report = Report::new(state, None);
            assert!(report.omits_percent(), "{state:?}");
            assert_eq!(report.sequence(), format!("{E}]9;4;{}{E}\\", state.code()));
            // The note has to name the divergence, not just say "optional": the
            // whole point is that omitting the field means different things in
            // different terminals.
            let notes = notes_for(state, None);
            assert!(
                notes
                    .iter()
                    .any(|note| note.contains("Windows Terminal") && note.contains("VTE")),
                "{notes:?}"
            );
        }
    }

    #[test]
    fn determinate_progress_without_a_percentage_complains() {
        let notes = notes_for(State::Set, None);
        assert!(
            notes.iter().any(|note| note.contains("needs a percentage")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_request_that_matches_the_state_gets_no_notes() {
        // Notes are for surprises. The ordinary cases must be silent, or nobody
        // reads them.
        assert!(notes_for(State::Set, Some(Percent::FULL)).is_empty());
        assert!(notes_for(State::Error, Some(Percent::FULL)).is_empty());
        assert!(notes_for(State::Warning, Some(Percent::ZERO)).is_empty());
        assert!(notes_for(State::Remove, None).is_empty());
        assert!(notes_for(State::Indeterminate, None).is_empty());
    }

    // -- the safety property ------------------------------------------------

    #[test]
    fn no_input_can_break_out_of_the_osc_string() {
        // The payload is built from integers, so this is a property of the types
        // rather than of any sanitizing step. Sweep every byte value a caller
        // could offer as a percentage and every state, and check that nothing
        // between the introducer and the terminator is anything but digits and
        // the structural semicolons.
        for raw in 0..=u8::MAX {
            let (percent, _) = Percent::clamping(u64::from(raw));
            for state in State::ALL {
                let sequence = Report::new(state, Some(percent)).sequence();
                let body = sequence
                    .strip_prefix(&format!("{E}]"))
                    .and_then(|rest| rest.strip_suffix(&format!("{E}\\")))
                    .expect("an OSC string");
                assert!(
                    body.chars().all(|ch| ch.is_ascii_digit() || ch == ';'),
                    "{body:?} contains something other than digits and semicolons"
                );
                assert!(!body.contains('\u{7}'), "{body:?}");
            }
        }
    }

    #[test]
    fn the_sequence_is_the_conemu_shape() {
        // Two structural semicolons before the state, then at most one more.
        let with = Report::set(Percent::FULL).sequence();
        assert_eq!(with.matches(';').count(), 3);
        let without = Report::new(State::Error, None).sequence();
        assert_eq!(without.matches(';').count(), 2);
        // And the constants are the ones the protocol names.
        assert_eq!(NUMBER, 9);
        assert_eq!(SUBCOMMAND, 4);
        assert_eq!(MAX_PERCENT, 100);
    }

    #[test]
    fn every_report_can_say_what_to_expect() {
        for state in State::ALL {
            for percent in [None, Some(Percent::FULL)] {
                let report = Report::new(state, percent);
                assert!(!report.expectation().is_empty(), "{report:?}");
            }
        }
    }
}
