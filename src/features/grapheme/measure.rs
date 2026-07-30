//! Measuring what the terminal actually did, rather than what it claims.
//!
//! DECRQM answers a question about *policy*: does this terminal implement mode
//! 2027, and is it on. That is worth knowing, but it is not the question an
//! application has. The question an application has is "how many cells did those
//! bytes just take", and the only honest way to find out is to look.
//!
//! So: park the cursor at a known position, print the sequence, ask for the
//! cursor position with `CSI 6 n`, and subtract.
//!
//! ```text
//! →  CR CSI K                 column 1, wipe the scratch line
//! →  CSI 6 n                  where are we?
//! ←  CSI <row> ; <col> R      here
//! →  <the sequence>
//! →  CSI 6 n
//! ←  CSI <row> ; <col> R      cells used = the difference
//! →  CR CSI J                 wipe it again
//! ```
//!
//! The measurement happens on a scratch line that is erased afterwards, so
//! nothing is left in the user's scrollback. `CSI 6 n` is its own barrier: every
//! terminal back to the VT100 answers it, so no reply means no terminal rather
//! than an unsupported feature.
//!
//! # On the kitty text sizing protocol
//!
//! The kitty text sizing protocol is sometimes taken for a way to *ask* a
//! terminal how wide it thinks a string is. It is not. `OSC 66 ; <metadata> ;
//! <text> ST` only ever *tells* the terminal a width (`w=`, cells) and a scale
//! (`s=`), inverting the coordination problem so the application decides. There
//! is no width query and no width reply anywhere in that protocol — its own
//! documented support-detection recipe is this same Cursor Position Report
//! measurement, run around an `OSC 66 ; w=2 ; <space> ST`. So `--debug` falls
//! back to `CSI 6 n`, which is not a consolation prize: it reports what the
//! terminal did rather than what it advertises.
//!
//! <https://sw.kovidgoyal.net/kitty/text-sizing-protocol/>

use std::{
    io::{IsTerminal as _, stdin, stdout},
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    features::grapheme::{
        ModeState, Reliability, barrier, cluster, disable, enable, mode_state_from_reply,
        position_query, query, restore_sequence, sentinel_query, sentinel_state_from_reply,
    },
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term},
    unicode,
};

/// How long to wait for the terminal to answer. Generous enough for a slow ssh
/// link, short enough not to feel like a hang.
const REPLY_TIMEOUT: Duration = Duration::from_millis(400);

/// What DECRQM said about mode 2027, and what that answer is worth.
///
/// The three fields are separate on purpose, because collapsing any two of them
/// loses a distinction that matters:
///
/// * a missing state with the barrier answered is a definite "this terminal does
///   not do DECRQM for mode 2027"; a missing state with no barrier either is
///   "nothing is listening";
/// * a *present* state is only meaningful if `sentinel` says the terminal
///   distinguishes modes it knows from modes it does not. A terminal that answers
///   `4` for an impossible mode answers `4` for everything, and its confident
///   `PermanentlyReset` for mode 2027 is noise wearing the costume of a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModeReport {
    /// The state the terminal reported for mode 2027, when it reported one.
    pub state: Option<ModeState>,
    /// The state it reported for [`super::SENTINEL_MODE`], the impossible control.
    pub sentinel: Option<ModeState>,
    /// Whether the Primary Device Attributes barrier came back.
    pub answered_barrier: bool,
}

impl ModeReport {
    /// Whether the query reached a conclusion either way.
    #[must_use]
    pub fn conclusive(self) -> bool {
        self.state.is_some() || self.answered_barrier
    }

    /// What the control says this run's DECRQM answers are worth.
    #[must_use]
    pub fn reliability(self) -> Reliability {
        super::reliability(self.sentinel, self.state, self.answered_barrier)
    }

    /// The mode state, but only when the control vouches for it.
    ///
    /// This is what everything downstream should use — reports, and above all
    /// [`super::restore_sequence`], which would otherwise decline to undo its own
    /// write on a terminal whose `4` means nothing.
    #[must_use]
    pub fn trusted_state(self) -> Option<ModeState> {
        if self.reliability().trustworthy() {
            self.state
        } else {
            None
        }
    }
}

/// Three counts for the same bytes, plus the fourth that explains the other two.
///
/// This comparison is the single most informative thing this subcommand can
/// print: it puts a prediction, a legacy terminal's answer and a modern
/// terminal's answer side by side for one concrete sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Comparison {
    /// What [`unicode::display_width`] predicts — segmentation-aware, so this is
    /// the number a terminal honouring mode 2027 should agree with.
    pub predicted: usize,
    /// What per-codepoint `wcwidth` gives, which is what a terminal without mode
    /// 2027 does.
    pub legacy: usize,
    /// Cells the terminal actually used with clustering **off**.
    pub clustering_off: Option<usize>,
    /// Cells the terminal actually used with clustering **on**.
    pub clustering_on: Option<usize>,
}

impl Comparison {
    /// A comparison with nothing measured — what a non-terminal stdout yields.
    #[must_use]
    pub fn unmeasured(text: &str) -> Self {
        Self {
            predicted: unicode::display_width(text),
            legacy: cluster::legacy_width(text),
            clustering_off: None,
            clustering_on: None,
        }
    }

    /// Whether this sequence can distinguish a clustering terminal from a
    /// `wcwidth` one at all.
    ///
    /// `A` cannot: both rules say one cell, so any measurement of it proves
    /// nothing. Saying so is more useful than printing a green tick.
    #[must_use]
    pub fn decisive(self) -> bool {
        self.predicted != self.legacy
    }

    /// Whether switching the mode changed the number of cells used.
    ///
    /// `None` when one of the two measurements is missing.
    #[must_use]
    pub fn mode_made_a_difference(self) -> Option<bool> {
        Some(self.clustering_off? != self.clustering_on?)
    }

    /// What the measurement proved.
    #[must_use]
    pub fn verdict(self) -> Verdict {
        let Some(measured) = self.clustering_on else {
            return Verdict::Unknown;
        };
        if measured == self.predicted {
            Verdict::Clustered
        } else if measured == self.legacy {
            Verdict::NotClustered
        } else {
            Verdict::Disagrees(measured)
        }
    }
}

/// What a [`Comparison`] proved about the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// With clustering on, the terminal used exactly the predicted cells.
    Clustered,
    /// With clustering on, the terminal still counted per codepoint.
    NotClustered,
    /// The terminal answered with a count nobody predicted — a font, a
    /// multiplexer or a width table nobody here knows about.
    Disagrees(usize),
    /// Nothing answered, so nothing is known. Not the same as a no.
    Unknown,
}

impl Verdict {
    /// Short label for a report column.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Clustered => "clustered",
            Self::NotClustered => "per codepoint",
            Self::Disagrees(_) => "disagrees",
            Self::Unknown => "unknown",
        }
    }

    /// Symbol chosen to survive a monospace grid, matching the support matrix.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Clustered => "✔",
            Self::NotClustered => "✘",
            Self::Disagrees(_) => "◐",
            Self::Unknown => "?",
        }
    }
}

/// A live measuring session: raw mode, one reusable scratch line, and the mode
/// state as it was found so it can be put back.
///
/// Exactly one of these should exist at a time, because it owns the process's
/// only [`InputReader`] — two would race for the same bytes on stdin and each
/// would see a random half of the terminal's answers.
#[derive(Debug)]
pub struct Ruler {
    term: Term,
    input: InputReader,
    /// Terminal width, needed to turn a wrapped cursor position into a count.
    cols: usize,
    /// What DECRQM said before anything was switched.
    entry: ModeReport,
    /// A multiplexer in the way, if any.
    multiplexer: Option<Multiplexer>,
    /// Whether the mode has already been put back, so restoring is idempotent.
    restored: bool,
    /// Set once a Cursor Position Report goes unanswered.
    ///
    /// A terminal that ignores the first CPR will ignore every later one, and
    /// each attempt costs a full [`REPLY_TIMEOUT`]. Measuring a ten-cluster
    /// sequence would then take four seconds of waiting to learn the same thing
    /// the first attempt already established.
    silent: bool,
}

impl Ruler {
    /// Opens a measuring session, or `Ok(None)` when there is no terminal to
    /// measure.
    ///
    /// Measurement needs stdin as well as stdout: the reply comes back through
    /// the keyboard side of the pty. Piping either one makes this impossible
    /// rather than merely inaccurate, so the caller is told and can say why.
    ///
    /// # Errors
    ///
    /// Fails if the terminal size cannot be read, raw mode cannot be entered, or
    /// the reader thread cannot be started.
    pub fn open() -> Result<Option<Self>> {
        if !(stdout().is_terminal() && stdin().is_terminal()) {
            return Ok(None);
        }
        let mut term = Term::new()?;
        let cols = usize::from(term.cols).max(1);
        let input = InputReader::new()?;
        // Raw mode is required: in cooked mode the reply would sit in the line
        // buffer until the user pressed Enter, and would be echoed at them.
        term.enable_raw()?;
        term.hide_cursor()?;
        let mut ruler = Self {
            term,
            input,
            cols,
            entry: ModeReport::default(),
            multiplexer: Multiplexer::detect(),
            restored: true,
            silent: false,
        };
        ruler.input.drain();
        ruler.entry = ruler.query_mode()?;
        Ok(Some(ruler))
    }

    /// What DECRQM said when the session opened, before anything was switched.
    #[must_use]
    pub fn entry_report(&self) -> ModeReport {
        self.entry
    }

    /// Whether the terminal has stopped answering cursor position reports, so
    /// further measurement would only buy timeouts.
    #[must_use]
    pub fn silent(&self) -> bool {
        self.silent
    }

    /// A multiplexer between this process and the real terminal, if any.
    #[must_use]
    pub fn multiplexer(&self) -> Option<Multiplexer> {
        self.multiplexer
    }

    /// Asks for mode 2027's state with DECRQM, alongside an impossible-mode
    /// control, behind a Primary Device Attributes barrier.
    ///
    /// One round trip, three sequences, in this order:
    ///
    /// ```text
    /// →  CSI ? 2027 $ p       the real question
    /// →  CSI ? 9876543 $ p    the control: a mode that cannot exist
    /// →  CSI c                the barrier
    /// ```
    ///
    /// The control is what makes the real answer worth anything. Some terminals
    /// answer DECRQM with a confident state for modes they have never heard of —
    /// iTerm2 says `4`, permanently reset, for fictional numbers — so without it a
    /// `4` for mode 2027 is indistinguishable from a genuine permanent reset. See
    /// [`Reliability`].
    ///
    /// The loop runs to the barrier rather than stopping at the first report,
    /// because both answers are wanted and either may arrive first. If the DA1
    /// answer arrives with no DECRPM report in front of it, the terminal has
    /// demonstrably had its chance and implements no DECRQM.
    ///
    /// The queries are deliberately **not** wrapped for a multiplexer. Inside tmux
    /// or Zellij the multiplexer is the thing laying out cells, so its answer —
    /// including silence — is the one that matters.
    ///
    /// # Errors
    ///
    /// Fails if stdout does. A terminal that stays quiet is a result, not an
    /// error.
    pub fn query_mode(&mut self) -> Result<ModeReport> {
        let mut report = ModeReport::default();
        self.term.write(&query())?;
        self.term.write(&sentinel_query())?;
        self.term.write_now(&barrier())?;

        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            // An EOF on stdin means there is nobody to answer; treat it as
            // silence rather than as an error.
            let Ok(Some(reply)) = self.input.wait_for_reply(remaining, |_| true) else {
                break;
            };
            if let Some(state) = mode_state_from_reply(&reply) {
                report.state = Some(state);
                continue;
            }
            if let Some(state) = sentinel_state_from_reply(&reply) {
                report.sentinel = Some(state);
                continue;
            }
            if matches!(
                reply,
                Reply::Csi {
                    final_byte: 'c',
                    ..
                }
            ) {
                // Everything sent before the barrier has been processed, so
                // anything still unanswered is going to stay unanswered.
                report.answered_barrier = true;
                break;
            }
        }
        Ok(report)
    }

    /// How many cells `text` occupied when printed — measured, not predicted.
    ///
    /// Prints on a scratch line at a known column, asks with `CSI 6 n`, and
    /// erases afterwards, so nothing survives in the user's scrollback.
    ///
    /// `Ok(None)` means the terminal never answered, or the sequence wrapped in a
    /// way that makes the arithmetic unsafe. That is not a width of zero, and the
    /// callers keep the distinction.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn measure_cells(&mut self, text: &str) -> Result<Option<usize>> {
        if self.silent {
            return Ok(None);
        }
        // Column 1 of a line we are about to own, wiped so a previous
        // measurement cannot be mistaken for this one.
        self.term.write("\r")?;
        self.term.write(&format!("{CSI}K"))?;
        self.term.write_now(&position_query())?;
        let Some((start_row, start_col)) = self.cursor_position() else {
            self.silent = true;
            return Ok(None);
        };

        self.term.write(text)?;
        self.term.write_now(&position_query())?;
        let end = self.cursor_position();

        // Erase whatever was drawn, including any wrapped continuation, before
        // looking at the numbers — the line must be clean even if we bail out.
        self.term.write("\r")?;
        self.term.write_now(&format!("{CSI}J"))?;

        let Some(end) = end else {
            self.silent = true;
            return Ok(None);
        };
        // `cells_between` returns `None` when the screen scrolled under us: the
        // arithmetic would otherwise produce a plausible-looking lie.
        Ok(cells_between((start_row, start_col), end, self.cols))
    }

    /// Measures `text` with clustering off and then on, and pairs both with what
    /// segmentation predicted.
    ///
    /// The mode is put back to whatever DECRQM reported at the start, so this
    /// leaves the terminal as it was found.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn compare(&mut self, text: &str) -> Result<Comparison> {
        let mut comparison = Comparison::unmeasured(text);
        if self.silent {
            // No point switching a mode twice in order to measure nothing: the
            // caller already knows this terminal does not answer, and the
            // unmeasured comparison still carries both predictions.
            return Ok(comparison);
        }

        // From here the mode is ours to put back.
        self.restored = false;
        self.term.write_now(&disable())?;
        comparison.clustering_off = self.measure_cells(text)?;

        self.term.write_now(&enable())?;
        comparison.clustering_on = self.measure_cells(text)?;

        self.restore_mode()?;
        Ok(comparison)
    }

    /// Puts mode 2027 back the way DECRQM reported it at the start.
    ///
    /// Idempotent, so the explicit [`Ruler::close`] and the [`Drop`] behind it do
    /// not send the sequence twice. Calling [`Ruler::compare`] again re-arms it.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn restore_mode(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        // `trusted_state`, not `state`: on a terminal whose `4` means nothing, the
        // raw report would claim there was nothing to undo and leave clustering on.
        if let Some(sequence) = restore_sequence(self.entry.trusted_state()) {
            self.term.write_now(&sequence)?;
        }
        Ok(())
    }

    /// Reads one Cursor Position Report, returning 1-based `(row, col)`.
    ///
    /// `CSI 6 n` is answered as `CSI <row> ; <col> R`; the DECXCPR variant adds a
    /// `?` prefix and a page parameter, so both shapes are accepted.
    fn cursor_position(&mut self) -> Option<(u16, u16)> {
        let reply = self
            .input
            .wait_for_reply(REPLY_TIMEOUT, |reply| {
                matches!(
                    reply,
                    Reply::Csi {
                        final_byte: 'R',
                        ..
                    }
                )
            })
            .ok()
            .flatten();
        let Some(Reply::Csi { params, .. }) = reply else {
            return None;
        };
        parse_cursor_position(&params)
    }

    /// Puts the mode and the terminal back, in that order.
    ///
    /// [`Drop`] does the same thing, so an error propagating out of a measurement
    /// still leaves the terminal as it was found; this is the explicit form, for
    /// the ordinary path where the report is printed afterwards.
    pub fn close(self) {
        drop(self);
    }
}

impl Drop for Ruler {
    fn drop(&mut self) {
        // Best effort, and it has to be a drop rather than only an explicit
        // `close`: `compare` can leave the mode switched on and then return an
        // error, and a tool that documents restoring the mode must not be the one
        // that forgets to.
        let _ = self.restore_mode();
        self.term.restore();
    }
}

/// `1 cell`, `2 cells` — the one place cell counts are turned into words, so every
/// report pluralises the same way.
#[must_use]
pub fn cells(count: usize) -> String {
    if count == 1 {
        "1 cell".to_owned()
    } else {
        format!("{count} cells")
    }
}

/// A measured cell count, or an explicit statement that nothing came back.
///
/// Never `0 cells`: a silence is not a zero, and printing it as one is the whole
/// class of mistake this subcommand is about.
#[must_use]
pub fn measured_cells(count: Option<usize>) -> String {
    match count {
        Some(count) => cells(count),
        None => "no answer".to_owned(),
    }
}

/// Parses the parameters of a Cursor Position Report into 1-based `(row, col)`.
///
/// Accepts both `CSI <row> ; <col> R` and the DECXCPR form
/// `CSI ? <row> ; <col> ; <page> R`.
#[must_use]
pub fn parse_cursor_position(params: &str) -> Option<(u16, u16)> {
    let body = params.strip_prefix('?').unwrap_or(params);
    let mut fields = body.split(';');
    let row = fields.next()?.trim().parse::<u16>().ok()?;
    let col = fields.next()?.trim().parse::<u16>().ok()?;
    // Both are 1-based and a terminal reporting 0 is reporting nonsense, which
    // would underflow the subtraction the caller does.
    if row == 0 || col == 0 {
        return None;
    }
    Some((row, col))
}

/// Turns two cursor positions into a cell count, wrapped lines included.
///
/// `cols` is the terminal width, needed because a sequence that runs past the
/// right margin continues on the next row: the cells used are the whole rows
/// crossed plus the column difference, and the column difference is *negative*
/// when the cursor wrapped.
///
/// Returns `None` when the numbers cannot be turned into a count — the screen
/// scrolled between the two reports, or the cursor ended up behind where it
/// started. Both are unknowable rather than large, and a plausible-looking lie is
/// worse than an admitted gap.
#[must_use]
pub fn cells_between(start: (u16, u16), end: (u16, u16), cols: usize) -> Option<usize> {
    let (start_row, start_col) = start;
    let (end_row, end_col) = end;
    let rows = end_row.checked_sub(start_row)?;
    let advance = usize::from(rows).checked_mul(cols)? + usize::from(end_col);
    advance.checked_sub(usize::from(start_col))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const FARMER: &str = "\u{1f9d1}\u{200d}\u{1f33e}";

    #[test]
    fn parses_a_cursor_position_report() {
        assert_eq!(parse_cursor_position("14;7"), Some((14, 7)));
        assert_eq!(parse_cursor_position("1;1"), Some((1, 1)));
        // DECXCPR adds a private prefix and a page parameter.
        assert_eq!(parse_cursor_position("?14;7;1"), Some((14, 7)));
        // Some terminals pad with spaces.
        assert_eq!(parse_cursor_position(" 14 ; 7 "), Some((14, 7)));
    }

    #[test]
    fn refuses_a_cursor_position_that_cannot_be_true() {
        // Positions are 1-based; a zero would underflow the subtraction.
        assert_eq!(parse_cursor_position("0;7"), None);
        assert_eq!(parse_cursor_position("14;0"), None);
        assert_eq!(parse_cursor_position("14"), None);
        assert_eq!(parse_cursor_position(""), None);
        assert_eq!(parse_cursor_position("x;y"), None);
    }

    #[test]
    fn cells_between_is_the_column_delta_on_one_line() {
        assert_eq!(cells_between((5, 1), (5, 3), 80), Some(2));
        assert_eq!(cells_between((5, 1), (5, 1), 80), Some(0), "nothing drawn");
        assert_eq!(cells_between((5, 10), (5, 15), 80), Some(5));
    }

    #[test]
    fn cells_between_counts_a_wrapped_line() {
        // Started at column 78 of an 80-column terminal and ended at column 3 of
        // the next row: 3 cells to the margin plus 2 more.
        assert_eq!(cells_between((5, 78), (6, 3), 80), Some(80 - 78 + 3));
    }

    #[test]
    fn cells_between_counts_a_line_wrapped_more_than_once() {
        assert_eq!(cells_between((1, 1), (3, 1), 80), Some(160));
    }

    #[test]
    fn cells_between_refuses_to_guess_after_a_scroll() {
        assert_eq!(cells_between((6, 1), (5, 3), 80), None);
    }

    #[test]
    fn cells_between_refuses_a_cursor_that_went_backwards() {
        // Same row, ended left of where it started: something moved the cursor
        // that was not the text we printed.
        assert_eq!(cells_between((5, 10), (5, 3), 80), None);
    }

    #[test]
    fn an_unmeasured_comparison_still_carries_both_predictions() {
        let comparison = Comparison::unmeasured(FARMER);
        assert_eq!(comparison.predicted, 2);
        assert_eq!(comparison.legacy, 4);
        assert_eq!(comparison.clustering_off, None);
        assert_eq!(comparison.clustering_on, None);
        assert_eq!(comparison.verdict(), Verdict::Unknown);
        assert_eq!(comparison.mode_made_a_difference(), None);
    }

    #[test]
    fn a_terminal_that_clusters_matches_the_prediction() {
        let comparison = Comparison {
            predicted: 2,
            legacy: 4,
            clustering_off: Some(4),
            clustering_on: Some(2),
        };
        assert_eq!(comparison.verdict(), Verdict::Clustered);
        assert_eq!(comparison.mode_made_a_difference(), Some(true));
        assert!(comparison.decisive());
    }

    #[test]
    fn a_wcwidth_terminal_is_a_definite_no_not_an_unknown() {
        let comparison = Comparison {
            predicted: 2,
            legacy: 4,
            clustering_off: Some(4),
            clustering_on: Some(4),
        };
        assert_eq!(comparison.verdict(), Verdict::NotClustered);
        assert_eq!(comparison.mode_made_a_difference(), Some(false));
    }

    #[test]
    fn a_terminal_that_already_clustered_needs_no_mode_change() {
        // Ghostty with `grapheme-width-method = unicode`, or anything reporting
        // permanently set: both measurements agree, and both are right.
        let comparison = Comparison {
            predicted: 2,
            legacy: 4,
            clustering_off: Some(2),
            clustering_on: Some(2),
        };
        assert_eq!(comparison.verdict(), Verdict::Clustered);
        assert_eq!(comparison.mode_made_a_difference(), Some(false));
    }

    #[test]
    fn a_third_answer_is_reported_as_itself() {
        // Terminal.app manages 6 cells for the farmer and Windows Terminal 5 —
        // neither prediction, and folding them into "no" would lose the fact.
        let comparison = Comparison {
            predicted: 2,
            legacy: 4,
            clustering_off: Some(6),
            clustering_on: Some(6),
        };
        assert_eq!(comparison.verdict(), Verdict::Disagrees(6));
        assert_eq!(comparison.verdict().label(), "disagrees");
    }

    #[test]
    fn a_sequence_both_rules_agree_on_proves_nothing() {
        let comparison = Comparison {
            predicted: 1,
            legacy: 1,
            clustering_off: Some(1),
            clustering_on: Some(1),
        };
        assert!(
            !comparison.decisive(),
            "plain `A` cannot distinguish the two rules"
        );
        // The verdict is still "clustered", because the terminal did agree — but
        // `decisive` is what stops a report from overclaiming.
        assert_eq!(comparison.verdict(), Verdict::Clustered);
    }

    #[test]
    fn half_a_measurement_is_not_a_verdict() {
        let comparison = Comparison {
            predicted: 2,
            legacy: 4,
            clustering_off: Some(4),
            clustering_on: None,
        };
        assert_eq!(comparison.verdict(), Verdict::Unknown);
        assert_eq!(comparison.mode_made_a_difference(), None);
    }

    #[test]
    fn every_verdict_has_a_distinct_label_and_symbol() {
        let verdicts = [
            Verdict::Clustered,
            Verdict::NotClustered,
            Verdict::Disagrees(3),
            Verdict::Unknown,
        ];
        for field in [verdicts.map(Verdict::label), verdicts.map(Verdict::symbol)] {
            let mut seen = field.to_vec();
            seen.sort_unstable();
            let total = seen.len();
            seen.dedup();
            assert_eq!(seen.len(), total, "two verdicts look the same");
        }
    }

    #[test]
    fn an_untrustworthy_state_is_withheld_rather_than_reported() {
        // iTerm2's shape: `4` for mode 2027, and `4` for a mode that cannot exist.
        let itermish = ModeReport {
            state: Some(ModeState::PermanentlyReset),
            sentinel: Some(ModeState::PermanentlyReset),
            answered_barrier: true,
        };
        assert_eq!(
            itermish.reliability(),
            Reliability::Indiscriminate(ModeState::PermanentlyReset)
        );
        assert_eq!(
            itermish.trusted_state(),
            None,
            "the raw answer must not escape as a state"
        );
        assert_eq!(
            itermish.state,
            Some(ModeState::PermanentlyReset),
            "but it is still kept, so a report can show what was discarded"
        );
    }

    #[test]
    fn a_vouched_for_state_passes_straight_through() {
        let honest = ModeReport {
            state: Some(ModeState::PermanentlyReset),
            sentinel: Some(ModeState::NotRecognized),
            answered_barrier: true,
        };
        assert!(honest.reliability().trustworthy());
        assert_eq!(honest.trusted_state(), Some(ModeState::PermanentlyReset));
    }

    #[test]
    fn an_untrustworthy_report_makes_restoration_undo_its_own_write() {
        // This is the bug the control prevents from becoming a real one. With the
        // raw `4`, `restore_sequence` would conclude "permanently reset, so nothing
        // was changed" and leave clustering switched on for whatever ran next.
        let itermish = ModeReport {
            state: Some(ModeState::PermanentlyReset),
            sentinel: Some(ModeState::PermanentlyReset),
            answered_barrier: true,
        };
        assert_eq!(
            restore_sequence(itermish.state),
            None,
            "the raw answer would send nothing — the bug"
        );
        assert_eq!(
            restore_sequence(itermish.trusted_state()),
            Some(disable()),
            "the trusted answer undoes our own enable — the fix"
        );
    }

    #[test]
    fn no_decrqm_at_all_is_still_conclusive_about_the_query() {
        let konsolish = ModeReport {
            state: None,
            sentinel: None,
            answered_barrier: true,
        };
        assert_eq!(konsolish.reliability(), Reliability::NoDecrqm);
        assert_eq!(konsolish.trusted_state(), None);
        assert!(
            konsolish.conclusive(),
            "the barrier came back, so the silence is a property of the terminal"
        );
    }

    #[test]
    fn a_mode_report_needs_the_state_or_the_barrier_to_conclude() {
        assert!(
            ModeReport {
                state: Some(ModeState::Reset),
                sentinel: Some(ModeState::NotRecognized),
                answered_barrier: false
            }
            .conclusive()
        );
        assert!(
            ModeReport {
                state: None,
                sentinel: None,
                answered_barrier: true
            }
            .conclusive(),
            "the barrier came back, so the silence is a real no"
        );
        assert!(
            !ModeReport::default().conclusive(),
            "total silence is inconclusive, not negative"
        );
    }
}
