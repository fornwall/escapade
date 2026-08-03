//! `--watch`: being told when the colours change, or admitting to polling.
//!
//! This is the half that makes querying worth doing at all. A program that asks
//! once at startup is correct for exactly as long as the user leaves their theme
//! alone; the moment they switch to light, every colour that program computed
//! from the background is wrong, and it has no way to find out.
//!
//! There are two ways to keep up, and they are not equivalent:
//!
//! * **Notification.** `CSI ? 2031 h` asks the terminal to report colour-scheme
//!   changes, and it then sends one unsolicited report per change. Instant, free
//!   when nothing is happening, and supported by a handful of terminals.
//! * **Polling.** Re-send `OSC 11 ; ? ST` on a timer and compare. Works
//!   everywhere the query works, costs a round trip per tick, and notices a change
//!   up to one interval late.
//!
//! Which one is in use is **printed**, along with the interval when polling. A
//! user watching for an instant response deserves to know why it is not instant,
//! and the difference is not otherwise visible — both paths print the same lines.
//!
//! Notification is a mode, which means it is terminal state that outlives this
//! process: it is disabled on every path out, including a failure part way
//! through, because leaving it on would have the terminal sending reports to
//! whatever runs next.

use std::{
    fmt::Write as _,
    process::ExitCode,
    time::{Duration, Instant, SystemTime},
};

use anyhow::Result;

use crate::{
    cli::{ColorCommand, OutputFormat},
    features::{
        Support,
        color::{
            notify::{self, Scheme},
            query::{Answer, Answers, Target, ask, ask_again, interactive},
            report::{self, Verdict},
        },
        wrap,
    },
    input::{Event, InputReader, KeyCode, KeyEventKind},
    term::Term,
    theme::{Ink, Theme},
};

/// How often to re-query when the terminal cannot notify.
///
/// One second is a compromise, and the report says so rather than pretending
/// otherwise: fast enough that a theme switch feels noticed, slow enough that a
/// terminal is not answering four queries a second forever.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long one read waits before the loop looks at the clock again.
///
/// Only the loop's responsiveness to Ctrl-C depends on this; the poll interval is
/// measured against [`Instant`], not counted in ticks.
const TICK: Duration = Duration::from_millis(100);

/// What prompted a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// The first reading, before anything had changed.
    Initial,
    /// The terminal said the colour scheme had changed.
    Notification,
    /// A re-query found something different.
    Poll,
}

impl Trigger {
    /// Stable machine-readable name.
    fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Notification => "notification",
            Self::Poll => "poll",
        }
    }

    /// Short parenthetical for the text output.
    fn note(self) -> &'static str {
        match self {
            Self::Initial => "at start",
            Self::Notification => "notified",
            Self::Poll => "noticed by polling",
        }
    }
}

/// Runs `escapade color --watch`.
///
/// # Errors
///
/// Fails if the terminal cannot be set up or written to.
pub fn run(theme: &Theme, command: &ColorCommand) -> Result<ExitCode> {
    if !interactive() {
        return Ok(refuse(theme, command));
    }

    // The background is always watched, whatever else was asked for: light or dark
    // is a statement about it, and it is the change anyone running this cares
    // about.
    let mut targets = Target::resolve(&command.query, command.index);
    if !targets.contains(&Target::Background) {
        targets.insert(0, Target::Background);
    }

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    term.enable_raw()?;

    let notification = notify::probe(&mut term, &mut input)?;
    let result = watch(theme, command, &mut term, &mut input, &targets, notification);

    // Notification is terminal state, not output. Nothing else takes it down, so
    // it comes down here whether the loop ended by a keypress or by an error.
    let _ = notify::stop(&mut term, notification.enabled);
    term.restore();
    result
}

/// The loop itself, with the terminal already prepared.
fn watch(
    theme: &Theme,
    command: &ColorCommand,
    term: &mut Term,
    input: &mut InputReader,
    targets: &[Target],
    notification: notify::State,
) -> Result<ExitCode> {
    let started = Instant::now();
    let mut answers = ask(term, input, targets)?;
    if !answers.any_answered() {
        term.disable_raw()?;
        return Ok(nothing_to_watch(theme, command, &answers));
    }

    let mut scheme = notification.scheme;
    if command.format == OutputFormat::Text {
        header(theme, term, notification, targets)?;
    }
    emit(
        theme,
        command,
        term,
        &answers,
        &answers.answers.clone(),
        scheme,
        Trigger::Initial,
        started,
    )?;

    let mut last_poll = Instant::now();
    loop {
        // A quit key, a colour-scheme report, or nothing — whichever comes first.
        match input.read(TICK) {
            Ok(Some(Event::Key(key))) => {
                if key.kind != KeyEventKind::Release && quits(key.code, key.mods.ctrl) {
                    return Ok(ExitCode::SUCCESS);
                }
            }
            Ok(Some(Event::Reply(reply))) => {
                if let Some(reported) = notify::parse_scheme_report(&reply) {
                    scheme = Some(reported);
                    let next = ask_again(term, input, targets)?;
                    let changed = changes(&answers, &next);
                    answers = next;
                    emit(
                        theme,
                        command,
                        term,
                        &answers,
                        &changed,
                        scheme,
                        Trigger::Notification,
                        started,
                    )?;
                    last_poll = Instant::now();
                }
            }
            // A timeout, or an event this loop has no use for. Nothing to do
            // either way; the clock is checked below regardless.
            Ok(_) => {}
            // stdin closed: there is nobody left to press Ctrl-C.
            Err(_) => return Ok(ExitCode::SUCCESS),
        }

        // Polling only runs when the terminal cannot tell us. When it can, this
        // loop is idle between reports and costs nothing.
        if notification.support() != Support::Yes && last_poll.elapsed() >= POLL_INTERVAL {
            last_poll = Instant::now();
            let next = ask_again(term, input, targets)?;
            let changed = changes(&answers, &next);
            if !changed.is_empty() {
                answers = next;
                emit(
                    theme,
                    command,
                    term,
                    &answers,
                    &changed,
                    scheme,
                    Trigger::Poll,
                    started,
                )?;
            }
        }
    }
}

/// Whether a key means "stop".
///
/// In raw mode Ctrl-C is a keystroke rather than a signal, so it has to be
/// recognised here — which is also why the cleanup below it always runs.
fn quits(code: KeyCode, ctrl: bool) -> bool {
    match code {
        KeyCode::Char('q' | 'Q') | KeyCode::Esc => true,
        KeyCode::Char('c' | 'd') => ctrl,
        _ => false,
    }
}

/// Which colours differ between two readings.
///
/// Compared by the parsed value rather than the raw text, so a terminal that
/// answers `rgb:f/0/0` once and `rgb:ffff/0000/0000` the next time is not reported
/// as having changed colour. It has not: those are the same red.
fn changes(before: &Answers, after: &Answers) -> Vec<Answer> {
    after
        .answers
        .iter()
        .filter(|answer| {
            let previous = before
                .answers
                .iter()
                .find(|old| old.target == answer.target)
                .and_then(|old| old.reported.as_ref())
                .map(|old| old.rgb);
            let current = answer.reported.as_ref().map(|new| new.rgb);
            previous != current
        })
        .cloned()
        .collect()
}

/// Prints one report of the current state.
#[expect(
    clippy::too_many_arguments,
    reason = "one line of output assembled from the theme, the command, the \
              readings, what changed, the scheme, the reason and the clock; \
              bundling them into a struct would only move the list"
)]
fn emit(
    theme: &Theme,
    command: &ColorCommand,
    term: &mut Term,
    answers: &Answers,
    changed: &[Answer],
    scheme: Option<Scheme>,
    trigger: Trigger,
    started: Instant,
) -> Result<()> {
    let verdict = report::verdict(scheme, answers.get(Target::Background).map(|r| r.rgb));
    match command.format {
        OutputFormat::Text => text_line(theme, term, changed, verdict, trigger, started),
        OutputFormat::Json => json_lines(term, answers, changed, verdict, trigger, started),
    }
}

/// One line per change, in a form a person can read as it scrolls past.
fn text_line(
    theme: &Theme,
    term: &mut Term,
    changed: &[Answer],
    verdict: Option<Verdict>,
    trigger: Trigger,
    started: Instant,
) -> Result<()> {
    let mut line = format!("  {}  ", theme.paint(&elapsed(started), Ink::Muted));
    match verdict {
        Some(verdict) => {
            let ink = if verdict.dark { Ink::Accent } else { Ink::Warn };
            line.push_str(&theme.paint(&format!("{:<5}", verdict.label()), ink));
        }
        None => line.push_str(&theme.paint("  ?  ", Ink::Muted)),
    }
    for answer in changed {
        let value = answer.reported.as_ref().map_or_else(
            || "no answer".to_owned(),
            |reported| format!("{} {}", reported.rgb.hex(), reported.raw),
        );
        let _ = write!(
            line,
            "  {} {}",
            theme.paint(&answer.target.label(), Ink::Strong),
            theme.paint(&value, Ink::Code)
        );
    }
    line.push_str(&theme.paint(&format!("  ({})", trigger.note()), Ink::Muted));
    // Raw mode means a bare newline does not return the cursor to column one.
    term.write(&line)?;
    term.write("\r\n")?;
    term.flush()
}

/// The same information as JSON Lines: a change marker, the colours, the verdict.
///
/// Deliberately the same record shapes the one-shot report emits, so a consumer
/// needs one parser for `escapade color --format json` and
/// `escapade color --watch --format json`.
fn json_lines(
    term: &mut Term,
    answers: &Answers,
    changed: &[Answer],
    verdict: Option<Verdict>,
    trigger: Trigger,
    started: Instant,
) -> Result<()> {
    let unix = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let mut out = format!(
        "{{\"feature\":\"color\",\"record\":\"change\",\"trigger\":\"{}\",\
         \"elapsed_ms\":{},\"unix_time\":{unix},\"changed\":{}}}\n",
        trigger.as_str(),
        started.elapsed().as_millis(),
        changed.len()
    );
    let subset = Answers {
        answers: changed.to_vec(),
        ..answers.clone()
    };
    out.push_str(&report::json_report(&subset, verdict));
    term.write(&out.replace('\n', "\r\n"))?;
    term.flush()
}

/// Elapsed time as `+MM:SS`.
///
/// A wall clock would need a date library; time since the watch started is what a
/// reader of a scrolling log actually wants anyway, and the JSON output carries
/// the absolute time for anything that needs it.
fn elapsed(started: Instant) -> String {
    let seconds = started.elapsed().as_secs();
    format!("+{:02}:{:02}", seconds / 60, seconds % 60)
}

/// The opening block: what is being watched, and how changes will be noticed.
fn header(
    theme: &Theme,
    term: &mut Term,
    notification: notify::State,
    targets: &[Target],
) -> Result<()> {
    let names: Vec<String> = targets.iter().map(|target| target.label()).collect();
    let mut out = String::new();
    out.push_str("\r\n");
    let _ = write!(
        out,
        "  {}\r\n",
        theme.paint("Watching for colour changes", Ink::Title)
    );
    let _ = write!(
        out,
        "  {} {}\r\n",
        theme.paint("colours:", Ink::Muted),
        theme.paint(&names.join(", "), Ink::Code)
    );

    // The honesty requirement: say which mechanism is in use, and if it is the
    // slow one, say why and how slow.
    let (label, ink) = match notification.support() {
        Support::Yes => ("instant — the terminal reports changes itself", Ink::Good),
        Support::No | Support::Unknown => ("polling", Ink::Warn),
    };
    let _ = write!(
        out,
        "  {} {}\r\n",
        theme.paint("updates:", Ink::Muted),
        theme.paint(label, ink)
    );
    for line in wrap(&notification.explain(POLL_INTERVAL), theme.width().saturating_sub(4)) {
        let _ = write!(out, "    {}\r\n", theme.paint(&line, Ink::Muted));
    }
    let _ = write!(
        out,
        "  {}\r\n\r\n",
        theme.paint("press q or Ctrl-C to stop", Ink::Muted)
    );
    term.write(&out)?;
    term.flush()
}

/// The refusal when there is no terminal to watch.
fn refuse(theme: &Theme, command: &ColorCommand) -> ExitCode {
    let reason = "cannot watch: stdin and stdout are not both terminals, so there is no terminal \
                  to query and no reply channel to listen on";
    if command.format == OutputFormat::Json {
        println!(
            "{{\"feature\":\"color\",\"record\":\"verdict\",\"dark\":null,\"basis\":null,\
             \"interactive\":false,\"reason\":\"{}\"}}",
            report::escape_json(reason)
        );
    } else {
        println!();
        for line in wrap(reason, theme.width().saturating_sub(4)) {
            println!("  {}", theme.paint(&line, Ink::Warn));
        }
        println!();
    }
    ExitCode::from(2)
}

/// The refusal when the terminal answered nothing, so there is nothing to watch.
fn nothing_to_watch(theme: &Theme, command: &ColorCommand, answers: &Answers) -> ExitCode {
    let reason = if answers.silence_is_conclusive() {
        "nothing to watch: this terminal does not answer colour queries. It answered the \
         device-attributes barrier, so that is a real no rather than a timeout."
    } else {
        "nothing to watch: no colour reply came back, and neither did the device-attributes \
         barrier, so something between here and the terminal is swallowing replies."
    };
    if command.format == OutputFormat::Json {
        println!(
            "{{\"feature\":\"color\",\"record\":\"verdict\",\"dark\":null,\"basis\":null,\
             \"interactive\":true,\"reason\":\"{}\"}}",
            report::escape_json(reason)
        );
    } else {
        println!();
        for line in wrap(reason, theme.width().saturating_sub(4)) {
            println!("  {}", theme.paint(&line, Ink::Warn));
        }
        println!();
    }
    ExitCode::from(2)
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::color::value::parse_reply;

    /// A reading with one colour in it.
    fn reading(payload: &str, target: Target) -> Answers {
        Answers {
            answers: vec![Answer {
                target,
                reported: Some(parse_reply(payload).unwrap()),
            }],
            answered_barrier: true,
            interactive: true,
            ..Answers::default()
        }
    }

    #[test]
    fn a_changed_background_is_a_change() {
        let before = reading("11;rgb:1e1e/1e1e/2e2e", Target::Background);
        let after = reading("11;rgb:efef/f1f1/f5f5", Target::Background);
        let changed = changes(&before, &after);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].target, Target::Background);
    }

    #[test]
    fn the_same_colour_at_a_different_digit_width_is_not_a_change() {
        // The trap turned into a false positive: a terminal that answers `f` one
        // time and `ffff` the next has not changed colour, and a watch that
        // compared the raw text would report a theme switch that never happened.
        let before = reading("11;rgb:f/0/0", Target::Background);
        let after = reading("11;rgb:ffff/0000/0000", Target::Background);
        assert!(changes(&before, &after).is_empty());
    }

    #[test]
    fn a_colour_that_stops_being_answered_counts_as_a_change() {
        let before = reading("11;rgb:0/0/0", Target::Background);
        let after = Answers {
            answers: vec![Answer {
                target: Target::Background,
                reported: None,
            }],
            ..Answers::default()
        };
        assert_eq!(changes(&before, &after).len(), 1);
        // And so does the reverse.
        assert_eq!(changes(&after, &before).len(), 1);
    }

    #[test]
    fn an_unchanged_reading_produces_nothing() {
        let before = reading("11;rgb:1e1e/1e1e/2e2e", Target::Background);
        let after = reading("11;rgb:1e1e/1e1e/2e2e", Target::Background);
        assert!(changes(&before, &after).is_empty());
    }

    #[test]
    fn only_the_colours_that_moved_are_reported() {
        let before = Answers {
            answers: vec![
                Answer {
                    target: Target::Foreground,
                    reported: Some(parse_reply("10;rgb:f/f/f").unwrap()),
                },
                Answer {
                    target: Target::Background,
                    reported: Some(parse_reply("11;rgb:0/0/0").unwrap()),
                },
            ],
            ..Answers::default()
        };
        let after = Answers {
            answers: vec![
                Answer {
                    target: Target::Foreground,
                    reported: Some(parse_reply("10;rgb:f/f/f").unwrap()),
                },
                Answer {
                    target: Target::Background,
                    reported: Some(parse_reply("11;rgb:f/f/f").unwrap()),
                },
            ],
            ..Answers::default()
        };
        let changed = changes(&before, &after);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].target, Target::Background);
    }

    #[test]
    fn quit_keys_are_the_ones_every_other_command_uses() {
        assert!(quits(KeyCode::Char('q'), false));
        assert!(quits(KeyCode::Char('Q'), false));
        assert!(quits(KeyCode::Esc, false));
        assert!(quits(KeyCode::Char('c'), true), "Ctrl-C");
        assert!(quits(KeyCode::Char('d'), true), "Ctrl-D");
        // Without the modifier they are just letters.
        assert!(!quits(KeyCode::Char('c'), false));
        assert!(!quits(KeyCode::Char('d'), false));
        assert!(!quits(KeyCode::Char('x'), true));
        assert!(!quits(KeyCode::Enter, false));
    }

    #[test]
    fn elapsed_time_is_minutes_and_seconds() {
        let now = Instant::now();
        assert_eq!(elapsed(now), "+00:00");
    }

    #[test]
    fn triggers_have_stable_names_and_readable_notes() {
        for trigger in [Trigger::Initial, Trigger::Notification, Trigger::Poll] {
            assert!(!trigger.as_str().is_empty());
            assert!(!trigger.note().is_empty());
            assert_eq!(trigger.as_str(), trigger.as_str().to_ascii_lowercase());
        }
        // The two mechanisms must be distinguishable in the output, which is the
        // entire point of printing which one is in use.
        assert_ne!(Trigger::Notification.note(), Trigger::Poll.note());
        assert!(Trigger::Poll.note().contains("polling"));
    }
}
