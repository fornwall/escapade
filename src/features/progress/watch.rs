//! `--watch`: driving the indicator from a stream.
//!
//! This is the reason the feature exists. Nobody types `escapade progress 42`;
//! they pipe something into it:
//!
//! ```sh
//! cargo build 2>&1 | grep --line-buffered -o '[0-9]*%' | escapade progress --watch
//! ```
//!
//! Which means the input is a **build log**, not a protocol. It has blank lines in
//! it, and half-written lines, and lines that are only a spinner, and lines with a
//! per cent sign, and lines that are pure prose. So every rule here is chosen for
//! that: a line that cannot be understood is skipped rather than fatal, because
//! aborting the pipeline over one unparseable line would be a much worse bug than
//! missing one update.
//!
//! Two properties are worth stating outright, because both are easy to get wrong:
//!
//! * **Nothing is redrawn when nothing changed.** A log printing `42%` two hundred
//!   times produces one escape sequence, not two hundred. The taskbar is a shared
//!   resource and some window managers animate every change.
//! * **The indicator is always cleared when the stream ends.** A progress
//!   indicator is terminal state, not output: nothing takes it down when the
//!   process exits, so a pipeline that dies at 60% would leave a taskbar stuck at
//!   60% until the user closed the window.

use std::io::BufRead;

use anyhow::Result;

use super::sequence::{Percent, Report, State};

/// What one line of input turned out to mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// A percentage to show.
    Percent(Percent),
    /// A bare word naming a state: `error`, `warning`, `busy`.
    State(State),
    /// A bare word meaning "that is over now": `done`, `clear`.
    Clear,
    /// Nothing usable. Skipped, and counted.
    Ignored,
}

/// Words that name a state on a line of their own.
///
/// Deliberately short and deliberately English: this is a convenience for shell
/// scripts that already know how their own build went, not an attempt to parse
/// anybody's log format.
///
/// ```sh
/// { make || echo error; } | escapade progress --watch
/// ```
const KEYWORDS: &[(&str, Line)] = &[
    ("error", Line::State(State::Error)),
    ("err", Line::State(State::Error)),
    ("fail", Line::State(State::Error)),
    ("failed", Line::State(State::Error)),
    ("failure", Line::State(State::Error)),
    ("warn", Line::State(State::Warning)),
    ("warning", Line::State(State::Warning)),
    ("pause", Line::State(State::Warning)),
    ("paused", Line::State(State::Warning)),
    ("busy", Line::State(State::Indeterminate)),
    ("working", Line::State(State::Indeterminate)),
    ("indeterminate", Line::State(State::Indeterminate)),
    ("done", Line::Clear),
    ("finished", Line::Clear),
    ("complete", Line::Clear),
    ("completed", Line::Clear),
    ("clear", Line::Clear),
    ("reset", Line::Clear),
];

/// Reads one line of the stream.
///
/// The rules, in order:
///
/// 1. Whitespace is trimmed, which is what makes `\r\n` input work — the carriage
///    return is whitespace.
/// 2. A blank line means nothing and is skipped, not treated as zero.
/// 3. One trailing `%` is stripped, so `42%` and `42 %` both work. That is the
///    shape `grep -o '[0-9]*%'` produces, which is the documented way to drive
///    this from a log.
/// 4. What is left is a percentage if it is a whole number, or a number with a
///    decimal fraction — `42`, `042`, `42.5`. The fraction is dropped rather than
///    rounded: the protocol has integer per cent and a bar that reads 42 when the
///    log said 42.5 surprises nobody.
/// 5. Otherwise, a single word from [`KEYWORDS`] names a state. Trailing
///    punctuation is ignored, so `done.` and `error:` work.
/// 6. Anything else is [`Line::Ignored`]. Notably a line like
///    `Compiling serde v1.0.229` is *not* mined for the digits inside it: reading
///    `1` out of that would move the bar backwards, and a wrong indicator is worse
///    than no indicator.
///
/// A number above 100 is clamped, because `Percent` cannot hold anything else and
/// a log that says `120%` means "finished".
#[must_use]
pub fn parse_line(line: &str) -> Line {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Line::Ignored;
    }

    let number = trimmed
        .strip_suffix('%')
        .map_or(trimmed, str::trim_end);
    if let Some(value) = parse_number(number) {
        let (percent, _) = Percent::clamping(value);
        return Line::Percent(percent);
    }

    let word = trimmed
        .trim_matches(|ch: char| ch.is_ascii_punctuation() || ch.is_whitespace())
        .to_ascii_lowercase();
    KEYWORDS
        .iter()
        .find(|(keyword, _)| *keyword == word)
        .map_or(Line::Ignored, |(_, line)| *line)
}

/// Parses a whole number, or the integer part of a decimal one.
///
/// Returns `None` for anything else at all — a leading sign, an embedded space, a
/// second decimal point, a thousands separator, a version number. Overflow
/// saturates rather than failing, because a line reading `99999999999999999999`
/// is nonsense but it is clearly nonsense meaning "a lot", and clamping handles
/// that one step later.
#[must_use]
fn parse_number(text: &str) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text, None),
    };
    if whole.is_empty() || !whole.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    // A decimal point with nothing after it (`42.`) is a typo, not a number, and
    // a fraction containing anything but digits (`1.2.3`, `1.2rc1`) is a version.
    if let Some(fraction) = fraction
        && (fraction.is_empty() || !fraction.chars().all(|ch| ch.is_ascii_digit()))
    {
        return None;
    }
    let mut value: u64 = 0;
    for ch in whole.chars() {
        let digit = u64::from(ch.to_digit(10).unwrap_or(0));
        value = value.saturating_mul(10).saturating_add(digit);
    }
    Some(value)
}

/// What a watch run did, for the summary on stderr.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Lines read from the stream.
    pub lines: usize,
    /// Escape sequences actually sent.
    pub updates: usize,
    /// Lines that meant nothing and were skipped.
    pub skipped: usize,
    /// Updates suppressed because the value had not changed.
    pub unchanged: usize,
}

/// Reads `input` to the end, sending each *change* through `emit`.
///
/// The whole watch behaviour lives here rather than in the terminal-facing
/// caller, so that all of it — the de-duplication, the tolerance for junk, the
/// clear at the end — is testable against a `&[u8]` with no terminal in sight.
///
/// `initial` is the state named by `--state`, sent before the first line arrives
/// so that `--watch --state indeterminate` shows a pulse while the first
/// percentage is still being computed.
///
/// Input is split on `\n` and converted lossily, so invalid UTF-8 cannot end the
/// run: a mangled line simply fails to parse and is skipped like any other junk.
///
/// # Errors
///
/// Propagates whatever `emit` fails with — in practice, stdout closing. A read
/// error on the input stream ends the loop as if the stream had closed, because
/// the indicator still needs taking down.
pub fn drive<R, E>(input: R, initial: Option<State>, emit: &mut E) -> Result<Tally>
where
    R: BufRead,
    E: FnMut(Report) -> Result<()>,
{
    let mut tally = Tally::default();
    let mut last: Option<Report> = None;

    if let Some(state) = initial {
        let report = Report::new(state, None);
        emit(report)?;
        tally.updates += 1;
        last = Some(report);
    }

    for chunk in input.split(b'\n') {
        let Ok(bytes) = chunk else {
            // A read error means the stream is gone. Stop reading, but fall
            // through to the clear below: leaving the indicator behind would be
            // the worse failure.
            break;
        };
        tally.lines += 1;
        let text = String::from_utf8_lossy(&bytes);
        let report = match parse_line(&text) {
            Line::Percent(percent) => Report::set(percent),
            Line::State(state) => Report::new(state, None),
            Line::Clear => Report::clear(),
            Line::Ignored => {
                tally.skipped += 1;
                continue;
            }
        };
        if last == Some(report) {
            tally.unchanged += 1;
            continue;
        }
        emit(report)?;
        tally.updates += 1;
        last = Some(report);
    }

    // Always leave the taskbar as it was found. The only exception is having just
    // sent a clear, in which case a second one would be noise.
    if last != Some(Report::clear()) {
        emit(Report::clear())?;
        tally.updates += 1;
    }

    Ok(tally)
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests, but the attribute is kept for the
// helpers in here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The percentage a line parsed to, or `None` if it was not a percentage.
    fn percent_of(line: &str) -> Option<u8> {
        match parse_line(line) {
            Line::Percent(percent) => Some(percent.get()),
            _ => None,
        }
    }

    /// Runs `drive` over a string, collecting the reports it emitted.
    fn driven(input: &str, initial: Option<State>) -> (Vec<Report>, Tally) {
        let mut sent = Vec::new();
        let tally = drive(input.as_bytes(), initial, &mut |report| {
            sent.push(report);
            Ok(())
        })
        .expect("collecting cannot fail");
        (sent, tally)
    }

    // -- the happy path -----------------------------------------------------

    #[test]
    fn a_bare_number_is_a_percentage() {
        assert_eq!(percent_of("0"), Some(0));
        assert_eq!(percent_of("42"), Some(42));
        assert_eq!(percent_of("100"), Some(100));
        assert_eq!(percent_of("042"), Some(42), "leading zeroes are harmless");
    }

    #[test]
    fn a_trailing_per_cent_sign_is_stripped() {
        // The shape `grep -o '[0-9]*%'` produces.
        assert_eq!(percent_of("42%"), Some(42));
        assert_eq!(percent_of("42 %"), Some(42));
        assert_eq!(percent_of("  7%  "), Some(7));
        assert_eq!(percent_of("100%"), Some(100));
    }

    #[test]
    fn whitespace_and_carriage_returns_are_trimmed() {
        // A log written for a terminal ends its lines with \r\n, and a progress
        // line often ends with a bare \r.
        assert_eq!(percent_of("42\r"), Some(42));
        assert_eq!(percent_of("\t42\t"), Some(42));
        assert_eq!(percent_of(" 42 "), Some(42));
    }

    #[test]
    fn a_decimal_fraction_is_truncated_rather_than_rejected() {
        assert_eq!(percent_of("42.5"), Some(42));
        assert_eq!(percent_of("42.9%"), Some(42));
        assert_eq!(percent_of("0.1"), Some(0));
        assert_eq!(percent_of("99.999"), Some(99));
    }

    #[test]
    fn a_number_above_one_hundred_is_clamped() {
        assert_eq!(percent_of("101"), Some(100));
        assert_eq!(percent_of("255"), Some(100));
        assert_eq!(percent_of("1000"), Some(100));
        // Absurdly long digit runs must clamp, not wrap or panic.
        assert_eq!(percent_of(&"9".repeat(40)), Some(100));
    }

    // -- the messy stream ---------------------------------------------------

    #[test]
    fn a_blank_line_means_nothing_rather_than_zero() {
        for line in ["", "   ", "\t", "\r", "\r\n"] {
            assert_eq!(parse_line(line), Line::Ignored, "{line:?}");
        }
    }

    #[test]
    fn prose_is_skipped_rather_than_mined_for_digits() {
        // The important negative case: reading `1` out of this would move the bar
        // backwards halfway through a build.
        for line in [
            "Compiling serde v1.0.229",
            "   Finished `release` profile [optimized] target(s) in 12.34s",
            "warning: unused variable: `x`",
            "50/100",
            "step 3 of 7",
            "[==>    ] 42",
            "1.2.3",
            "-5",
            "+42",
            "0x2a",
            "42abc",
            "42.",
            "%",
            "NaN",
        ] {
            assert_eq!(parse_line(line), Line::Ignored, "{line:?}");
        }
    }

    #[test]
    fn invalid_utf8_is_skipped_rather_than_fatal() {
        let mut input: Vec<u8> = b"42\n".to_vec();
        input.extend_from_slice(&[0xff, 0xfe, b'\n']);
        input.extend_from_slice(b"84\n");
        let mut sent = Vec::new();
        let tally = drive(&input[..], None, &mut |report| {
            sent.push(report);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            sent,
            vec![
                Report::set(Percent::clamping(42).0),
                Report::set(Percent::clamping(84).0),
                Report::clear(),
            ]
        );
        assert_eq!(tally.skipped, 1);
    }

    #[test]
    fn a_bare_keyword_names_a_state() {
        assert_eq!(parse_line("error"), Line::State(State::Error));
        assert_eq!(parse_line("ERROR"), Line::State(State::Error));
        assert_eq!(parse_line("failed"), Line::State(State::Error));
        assert_eq!(parse_line("warning"), Line::State(State::Warning));
        assert_eq!(parse_line("paused"), Line::State(State::Warning));
        assert_eq!(parse_line("busy"), Line::State(State::Indeterminate));
        assert_eq!(parse_line("done"), Line::Clear);
        assert_eq!(parse_line("clear"), Line::Clear);
        // Punctuation around the word is ignored.
        assert_eq!(parse_line("error:"), Line::State(State::Error));
        assert_eq!(parse_line("done."), Line::Clear);
        assert_eq!(parse_line("  DONE!  "), Line::Clear);
        // But a keyword inside a sentence is not a keyword.
        assert_eq!(parse_line("error: 3 tests failed"), Line::Ignored);
    }

    #[test]
    fn every_keyword_is_lowercase_and_unique() {
        let mut seen: Vec<&str> = KEYWORDS.iter().map(|(word, _)| *word).collect();
        seen.sort_unstable();
        let total = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), total, "two keywords collide");
        for (word, _) in KEYWORDS {
            assert_eq!(*word, word.to_ascii_lowercase());
            assert!(!word.is_empty());
        }
    }

    // -- driving the indicator ----------------------------------------------

    #[test]
    fn a_stream_of_percentages_becomes_a_stream_of_updates() {
        let (sent, tally) = driven("0\n50\n100\n", None);
        assert_eq!(
            sent,
            vec![
                Report::set(Percent::ZERO),
                Report::set(Percent::clamping(50).0),
                Report::set(Percent::FULL),
                Report::clear(),
            ]
        );
        assert_eq!(tally.updates, 4);
        assert_eq!(tally.skipped, 0);
    }

    #[test]
    fn nothing_is_redrawn_when_the_value_has_not_changed() {
        let (sent, tally) = driven("42\n42\n42\n42%\n42.4\n", None);
        assert_eq!(
            sent,
            vec![Report::set(Percent::clamping(42).0), Report::clear()],
            "one update, however many times the log said 42"
        );
        assert_eq!(tally.lines, 5);
        assert_eq!(tally.unchanged, 4);
    }

    #[test]
    fn the_indicator_is_cleared_when_the_stream_closes() {
        for input in ["42\n", "42", "", "nonsense\n"] {
            let (sent, _) = driven(input, None);
            assert_eq!(
                sent.last(),
                Some(&Report::clear()),
                "{input:?} left the indicator up"
            );
        }
    }

    #[test]
    fn a_stream_ending_in_done_is_not_cleared_twice() {
        let (sent, _) = driven("42\ndone\n", None);
        assert_eq!(
            sent,
            vec![Report::set(Percent::clamping(42).0), Report::clear()]
        );
    }

    #[test]
    fn an_initial_state_is_sent_before_the_first_line() {
        let (sent, _) = driven("42\n", Some(State::Indeterminate));
        assert_eq!(
            sent,
            vec![
                Report::new(State::Indeterminate, None),
                Report::set(Percent::clamping(42).0),
                Report::clear(),
            ]
        );
    }

    #[test]
    fn a_state_keyword_keeps_the_percentage_the_terminal_already_had() {
        // `error` sends no percentage at all, which is what makes the terminal
        // keep its current value and merely turn it red.
        let (sent, _) = driven("60\nerror\n", None);
        assert_eq!(sent.len(), 3);
        assert!(sent[1].omits_percent());
        assert_eq!(sent[1].state, State::Error);
        assert!(!sent[1].sequence().contains("60"));
    }

    #[test]
    fn junk_between_updates_does_not_interrupt_them() {
        let input = "\nCompiling foo\n10%\n\nCompiling bar\n10%\nLinking\n90%\n\n";
        let (sent, tally) = driven(input, None);
        assert_eq!(
            sent,
            vec![
                Report::set(Percent::clamping(10).0),
                Report::set(Percent::clamping(90).0),
                Report::clear(),
            ]
        );
        assert_eq!(tally.skipped, 6, "five prose or blank lines plus the trailing one");
        assert_eq!(tally.unchanged, 1);
    }

    #[test]
    fn an_emit_failure_stops_the_run_rather_than_being_swallowed() {
        let mut count = 0;
        let result = drive("1\n2\n3\n".as_bytes(), None, &mut |_| {
            count += 1;
            if count == 2 {
                anyhow::bail!("stdout closed");
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(count, 2);
    }

    #[test]
    fn every_sequence_a_watch_run_can_emit_is_well_formed() {
        let (sent, _) = driven("0\n42\n100\nerror\nwarning\nbusy\ndone\n", None);
        for report in sent {
            let sequence = report.sequence();
            assert!(sequence.starts_with("\u{1b}]9;4;"), "{sequence:?}");
            assert!(sequence.ends_with("\u{1b}\\"), "{sequence:?}");
        }
    }
}
