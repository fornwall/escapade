//! Taskbar and tab progress reporting.
//!
//! ```text
//! OSC 9 ; 4 ; <state> ; <progress> ST
//! ```
//!
//! A long job tells the terminal how far along it is, and the terminal shows it
//! where the user is already looking — the Windows taskbar button, the macOS dock
//! icon, a tab title, a window title, a pane border. The application draws
//! nothing: it sends one small escape sequence per change and the desktop does the
//! rest, which is why this works over ssh and from a program that has no idea what
//! it is attached to.
//!
//! ConEmu invented it. Windows Terminal, mintty, WezTerm, Ghostty, Rio, Contour,
//! Konsole and VTE copied it, and — unusually for an OSC extension — **tmux parses
//! and forwards it**, which matters because long builds are exactly what runs
//! inside tmux.
//!
//! # The collision
//!
//! `OSC 9` is iTerm2's *desktop notification* sequence. `OSC 9 ; 4 ; …` is
//! ConEmu's progress report. They are told apart only by what follows the first
//! semicolon, and terminals implement one, the other, or both:
//!
//! * On a terminal with the notification and not the progress form, a progress
//!   report can surface as a toast reading `4;1;50`.
//! * On a terminal with the progress form and not the notification — Windows
//!   Terminal — a notification whose text begins `4;` moves the taskbar instead of
//!   appearing.
//!
//! Nothing can be done about this from the sending end except to know about it.
//! [`crate::features::notification`] guards the other direction, prefixing a space
//! to a notification that would be misread as a progress report.
//!
//! # The modules
//!
//! * [`sequence`] — the states, the percentage type, and building the sequence.
//! * [`watch`] — turning a messy stream of lines into indicator updates.
//! * [`probe`] — what can honestly be said about support, which is less than
//!   usual: this protocol has no query at all.
//! * [`demo`] — the step-by-step walkthrough.

pub mod demo;
pub mod probe;
pub mod sequence;
pub mod watch;

use std::{
    io::{IsTerminal as _, Write as _, stdin, stdout},
    process::ExitCode,
};

use anyhow::{Result, bail};

use crate::{
    cli::{ProgressAction, ProgressCommand},
    db::Catalog,
    features::progress::sequence::{MAX_PERCENT, Percent, Report, State},
    term::{Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// The feature id in the database. The subcommand is `progress`.
pub const FEATURE_ID: &str = "progress";

/// Entry point for `escapade progress …`.
///
/// Returns an exit status because `probe` is a predicate: `0` supported, `1` not
/// supported, `2` cannot tell. Every other path returns success.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with.
pub fn run(catalog: &Catalog, command: &ProgressCommand, no_color: bool) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        Some(ProgressAction::Explain { no_matrix }) => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        Some(ProgressAction::Probe { format }) => probe::report(&theme, catalog, *format),
        Some(ProgressAction::Demo {
            force,
            only,
            step,
            list,
        }) => {
            if *list {
                demo::list_steps(&theme);
            } else {
                demo::run(&theme, catalog, *force, only.as_deref(), *step)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        None if command.watch => {
            watch_stdin(&theme, command)?;
            Ok(ExitCode::SUCCESS)
        }
        None => {
            report_once(&theme, command)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// What one invocation resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The report to send.
    pub report: Report,
    /// Something the user got wrong, worth a warning.
    pub warning: Option<String>,
    /// Things worth knowing about what will be sent, from
    /// [`sequence::notes_for`].
    pub notes: Vec<String>,
}

/// Works out what one invocation is asking for.
///
/// Pure, and separated from the sending so that the interesting part — which
/// combinations mean what, and which are mistakes — is testable.
///
/// `--clear` wins outright. Otherwise the state is whatever `--state` said, or
/// determinate progress when only a percentage was given. A percentage above 100
/// is **clamped rather than rejected**, and the caller says so on stderr: the
/// protocol has no way to express one, terminals disagree about what they do with
/// one, and a script computing `100 * done / total` against a stale total should
/// not die over an off-by-one.
///
/// # Errors
///
/// Fails on the one genuinely ambiguous request: determinate progress with no
/// percentage to draw.
pub fn resolve(
    percent: Option<u8>,
    state: Option<crate::cli::ProgressState>,
    clear: bool,
) -> Result<Resolved> {
    if clear {
        return Ok(Resolved {
            report: Report::clear(),
            warning: None,
            notes: Vec::new(),
        });
    }

    let mut warning = None;
    let percent = percent.map(|raw| {
        let (percent, clamped) = Percent::clamping(u64::from(raw));
        if clamped {
            warning = Some(format!(
                "{raw} is not a percentage — the protocol defines 0 to {MAX_PERCENT}, so this was \
                 clamped to {MAX_PERCENT}"
            ));
        }
        percent
    });

    let state = state.map_or(State::Set, State::from);
    if state == State::Set && percent.is_none() {
        bail!(
            "determinate progress needs a percentage: `escapade progress 42`. For a bar with no \
             value use `--state indeterminate`, and to take the indicator away use `--clear`"
        );
    }

    Ok(Resolved {
        report: Report::new(state, percent),
        warning,
        // Built from the *request*, so that a percentage which will not be sent can
        // still be reported as dropped.
        notes: sequence::notes_for(state, percent),
    })
}

/// Sends one progress report, or prints what it would send.
///
/// # Errors
///
/// Fails if the request is ambiguous, or if the terminal cannot be written to.
fn report_once(theme: &Theme, command: &ProgressCommand) -> Result<()> {
    let resolved = resolve(command.percent, command.state, command.clear)?;

    // Explanation goes to stderr, so `--dry-run` output and the escape sequence
    // itself stay the only things on stdout.
    let mut err = std::io::stderr();
    if let Some(warning) = &resolved.warning {
        let _ = writeln!(err, "{} {warning}", theme.paint("progress:", Ink::Warn));
    }
    for note in &resolved.notes {
        let _ = writeln!(err, "{} {note}", theme.paint("progress:", Ink::Muted));
    }

    if command.dry_run {
        print_dry_run(theme, &[resolved.report], Multiplexer::detect());
        return Ok(());
    }

    let mut term = Term::new()?;
    term.write_now(&resolved.report.sequence())
}

/// Runs `--watch`, reading percentages from stdin until it closes.
///
/// # Errors
///
/// Fails if the terminal cannot be written to. A stream full of junk is not a
/// failure; it is the expected input.
fn watch_stdin(theme: &Theme, command: &ProgressCommand) -> Result<()> {
    let multiplexer = Multiplexer::detect();
    let initial = command.state.map(State::from);
    let mut err = std::io::stderr();

    // A terminal on stdin means somebody ran this by hand, probably expecting the
    // percentage to be an argument. Say what is being waited for rather than
    // sitting there looking hung.
    if stdin().is_terminal() {
        let _ = writeln!(
            err,
            "{} {}",
            theme.paint("progress:", Ink::Warn),
            theme.paint(
                "reading percentages from the keyboard, one per line — type a number and press \
                 enter, `error` or `done` for a state, Ctrl-D to finish. Did you mean `escapade \
                 progress 42`, or to pipe something in?",
                Ink::Muted
            )
        );
    }
    if let Some(multiplexer) = multiplexer {
        let _ = writeln!(
            err,
            "{} {}",
            theme.paint("progress:", Ink::Muted),
            theme.paint(
                &format!(
                    "inside {} — tmux 3.7 and later parse this sequence and pass it on; older \
                     versions swallow it",
                    multiplexer.name()
                ),
                Ink::Muted
            )
        );
    }

    let input = stdin().lock();
    let tally = if command.dry_run {
        watch::drive(input, initial, &mut |report: Report| {
            println!("  {}", theme.paint(&pretty(&report.sequence()), Ink::Code));
            Ok(())
        })?
    } else {
        let mut term = Term::new()?;
        watch::drive(input, initial, &mut |report: Report| {
            term.write_now(&report.sequence())
        })?
    };

    let summary = format!(
        "{} line{} read, {} update{} sent, {} skipped, {} unchanged — indicator cleared",
        tally.lines,
        plural(tally.lines),
        tally.updates,
        plural(tally.updates),
        tally.skipped,
        tally.unchanged
    );
    let _ = writeln!(
        err,
        "{} {}",
        theme.paint("progress:", Ink::Muted),
        theme.paint(&summary, Ink::Muted)
    );
    Ok(())
}

/// `s`, unless there was exactly one of the thing.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Prints the sequences a run would have sent, in readable form.
fn print_dry_run(theme: &Theme, reports: &[Report], multiplexer: Option<Multiplexer>) {
    println!();
    for report in reports {
        println!("  {}", theme.paint(&pretty(&report.sequence()), Ink::Code));
    }
    println!();
    let bytes: usize = reports.iter().map(|report| report.sequence().len()).sum();
    println!(
        "  {}",
        theme.paint(
            &format!(
                "{} escape {}, {bytes} bytes on the wire",
                reports.len(),
                if reports.len() == 1 { "code" } else { "codes" }
            ),
            Ink::Muted
        )
    );
    for report in reports {
        println!(
            "  {}",
            theme.paint(&format!("expect: {}", report.expectation()), Ink::Muted)
        );
    }
    if let Some(multiplexer) = multiplexer {
        println!(
            "  {}",
            theme.paint(
                &format!(
                    "sent unwrapped, even inside {}: tmux 3.7 parses this sequence and forwards \
                     it, so wrapping it for passthrough would be wrong on a current tmux and is \
                     only needed on an older one",
                    multiplexer.name()
                ),
                Ink::Muted
            )
        );
    }
    if !stdout().is_terminal() {
        println!(
            "  {}",
            theme.paint(
                "stdout is not a terminal, so nothing would have been shown anyway",
                Ink::Muted
            )
        );
    }
    println!();
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests, but the attribute is kept for the
// helpers in here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::ProgressState;

    #[test]
    fn the_feature_id_matches_the_database() {
        let catalog = Catalog::load().unwrap();
        assert!(catalog.feature(FEATURE_ID).is_ok());
    }

    #[test]
    fn a_bare_percentage_is_determinate_progress() {
        let resolved = resolve(Some(42), None, false).unwrap();
        assert_eq!(resolved.report, Report::set(Percent::clamping(42).0));
        assert!(resolved.warning.is_none());
        assert!(resolved.notes.is_empty(), "{:?}", resolved.notes);
        assert_eq!(resolved.report.sequence(), "\u{1b}]9;4;1;42\u{1b}\\");
    }

    #[test]
    fn clear_beats_everything_else() {
        assert_eq!(resolve(None, None, true).unwrap().report, Report::clear());
        // clap forbids `--clear` with a percentage or a state, but the resolver
        // does not depend on that to behave sensibly.
        let resolved = resolve(Some(50), Some(ProgressState::Error), true).unwrap();
        assert_eq!(resolved.report, Report::clear());
        assert!(resolved.notes.is_empty());
    }

    #[test]
    fn every_state_resolves_without_a_percentage_except_normal() {
        for (flag, expected) in [
            (ProgressState::Error, State::Error),
            (ProgressState::Indeterminate, State::Indeterminate),
            (ProgressState::Warning, State::Warning),
        ] {
            let resolved = resolve(None, Some(flag), false).unwrap();
            assert_eq!(resolved.report.state, expected);
            assert!(resolved.warning.is_none());
        }
        // Determinate progress with nothing to draw is the one refusal.
        let error = resolve(None, Some(ProgressState::Normal), false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("needs a percentage"), "{error}");
        assert!(error.contains("--state indeterminate"), "{error}");
    }

    #[test]
    fn a_state_and_a_percentage_travel_together_where_the_state_allows_it() {
        let resolved = resolve(Some(60), Some(ProgressState::Error), false).unwrap();
        assert_eq!(resolved.report.sequence(), "\u{1b}]9;4;2;60\u{1b}\\");
        assert!(resolved.notes.is_empty(), "nothing surprising happened");
        // And the states that ignore a percentage drop it — but say so, which is
        // the whole reason the notes are built from the request rather than from
        // the finished report.
        let resolved = resolve(Some(60), Some(ProgressState::Indeterminate), false).unwrap();
        assert_eq!(resolved.report.percent, None);
        assert!(
            resolved.notes.iter().any(|note| note.contains("ignores")),
            "{:?}",
            resolved.notes
        );
    }

    #[test]
    fn an_impossible_percentage_is_clamped_and_reported() {
        let resolved = resolve(Some(255), None, false).unwrap();
        assert_eq!(resolved.report, Report::set(Percent::FULL));
        let warning = resolved.warning.expect("a warning");
        assert!(warning.contains("255"), "{warning}");
        assert!(warning.contains("clamped"), "{warning}");
        // 100 itself is not worth a warning.
        assert!(resolve(Some(100), None, false).unwrap().warning.is_none());
    }

    #[test]
    fn plurals_read_correctly() {
        assert_eq!(plural(0), "s");
        assert_eq!(plural(1), "");
        assert_eq!(plural(2), "s");
    }
}
