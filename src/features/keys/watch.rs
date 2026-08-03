//! `escapade keys watch` — the `showkey` of modern terminals.
//!
//! Press a key, see two things: what the key *was*, and the bytes that said so.
//! That second half is the reason this exists. Every input library will tell you
//! it saw Shift+Enter; none of them will show you `CSI 13 ; 2 u`, and the bytes
//! are what you need when a key does not work in one terminal, when a keybinding
//! has to be written into a config file, or when a bug report needs evidence.
//!
//! # Run it twice
//!
//! ```console
//! $ escapade keys watch --legacy   # press Enter, then Shift+Enter
//! $ escapade keys watch            # press Enter, then Shift+Enter
//! ```
//!
//! In the first run both presses are one carriage return and the terminal cannot
//! tell you which happened. In the second, one is `CR` and the other is
//! `CSI 13 ; 2 u`. `--legacy` pushes nothing at all — not even flags of zero — so
//! the first run is exactly what a program written in 1995 sees.
//!
//! The closing summary counts the *distinct encodings* seen per key, which is
//! that comparison reduced to one number: `Enter  1 encoding` in the first run,
//! `Enter  2 encodings` in the second.
//!
//! # Getting the bytes back out
//!
//! [`crate::input::InputReader`] hands out decoded events, not bytes. The byte
//! stream is mirrored on the way in and each event attributed to the bytes that
//! produced it; see [`super::bytes`].

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{IsTerminal as _, Write as _, stdin, stdout},
    process::ExitCode,
    time::Duration,
};

use anyhow::Result;

use crate::{
    cli::ProbeFormat,
    features::keys::{
        Flags,
        bytes::{EventSource as _, Keyboard},
        event::{describe_event, hex, json_line, key_name, symbolic},
        pop, push,
    },
    input::{Event, KeyCode, KeyEvent, KeyEventKind},
    term::{Term, pretty},
    theme::{Ink, Theme},
};

/// How long a single read waits before looping. Only the loop's responsiveness
/// depends on it; the reader keeps its own escape-disambiguation timing.
const POLL: Duration = Duration::from_millis(200);

/// Column the decoded description starts at, so the byte column stays readable.
const BYTES_WIDTH: usize = 26;

// ---------------------------------------------------------------------------
// The tally
// ---------------------------------------------------------------------------

/// How many distinct byte sequences each key turned out to have.
///
/// This is the `--legacy` comparison in one table. In legacy mode Enter has one
/// encoding no matter how it is modified; with the disambiguate flag it has as
/// many as there are modifier combinations pressed.
#[derive(Debug, Default)]
pub struct Tally {
    /// Key name to the distinct symbolic encodings seen for it, in sorted order.
    keys: BTreeMap<String, BTreeSet<String>>,
    /// Total events seen, including the ones that are not keys.
    events: usize,
}

impl Tally {
    /// Records one event and its bytes.
    pub fn record(&mut self, event: &Event, bytes: &[u8]) {
        self.events += 1;
        if let Event::Key(key) = event {
            // Release and repeat events share their key's row: the question is
            // how many different things this *key* can send, and a release is one
            // of them.
            self.keys
                .entry(key_name(key.code))
                .or_default()
                .insert(symbolic(bytes));
        }
    }

    /// Total events recorded.
    #[must_use]
    pub fn events(&self) -> usize {
        self.events
    }

    /// One line per key: its name, how many encodings it had, and which.
    ///
    /// Keys with more than one encoding come first, because they are the evidence
    /// that a flag took effect.
    #[must_use]
    pub fn summary(&self) -> Vec<(String, usize, String)> {
        let mut rows: Vec<(String, usize, String)> = self
            .keys
            .iter()
            .map(|(name, encodings)| {
                let list: Vec<&str> = encodings.iter().map(String::as_str).collect();
                (name.clone(), encodings.len(), list.join(", "))
            })
            .collect();
        rows.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        rows
    }
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

/// Whether a key event should end the watch.
///
/// Ctrl-D and Ctrl-C, and only on the way down — with `report event types`
/// enabled the release of Ctrl-C arrives too, and quitting on that would mean the
/// last thing shown was never displayed.
#[must_use]
pub fn is_quit(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c' | 'd'),
            mods,
            kind: KeyEventKind::Press,
        }) if mods.ctrl
    )
}

/// Runs `escapade keys watch`.
///
/// `wanted` is the flags to push, or `None` for `--legacy`, which pushes nothing
/// whatsoever. The flags are popped on **every** path out of here, including an
/// early error: [`Term`] tracks what it pushed and restores it on drop, and the
/// panic hook installed in `main` does the same for a panic.
///
/// # Errors
///
/// Fails if the terminal cannot be put into raw mode, or if stdout closes.
pub fn run(theme: &Theme, wanted: Option<Flags>, format: Option<ProbeFormat>) -> Result<ExitCode> {
    // Reading keys needs a terminal on stdin; showing them needs one on stdout.
    // Without both, this still works — it becomes a decoder for a byte stream, so
    // `printf '\033[13;2u' | escapade keys watch` explains a sequence from a bug
    // report — but nothing may be pushed, because there is nothing to push to.
    let piped = !stdin().is_terminal() || !stdout().is_terminal();
    let json = format == Some(ProbeFormat::Json);
    let verbose = format == Some(ProbeFormat::Debug);

    let mut term = if piped { None } else { Some(Term::new()?) };
    let pushed = match (&mut term, wanted) {
        (Some(term), Some(flags)) => {
            term.enable_raw()?;
            term.push_kitty_keyboard(flags.bits())?;
            Some(flags)
        }
        (Some(term), None) => {
            term.enable_raw()?;
            None
        }
        (None, _) => None,
    };
    // In raw mode a bare newline moves down without returning, so every line has
    // to carry the carriage return itself.
    let raw = term.is_some();

    if !json {
        header(theme, pushed, wanted, piped, raw);
    }

    let mut keyboard = Keyboard::new()?;
    let mut tally = Tally::default();

    loop {
        let (event, bytes) = match keyboard.next_event(POLL) {
            Ok(Some(pair)) => pair,
            // Nothing arrived within the poll window; keep waiting.
            Ok(None) => continue,
            // End of input: a closed pipe, or a terminal that went away.
            Err(_) => break,
        };
        tally.record(&event, &bytes);

        if json {
            emit(raw, &json_line(&bytes, &event));
        } else {
            emit(
                raw,
                &format!("{}\n", event_line(theme, &event, &bytes, verbose)),
            );
        }
        let _ = stdout().flush();

        if is_quit(&event) {
            break;
        }
    }

    // Back to a sane terminal *before* the summary, so it prints in cooked mode
    // with the flags already popped. `restore` is idempotent and covers the
    // early-return paths too.
    if let Some(term) = &mut term {
        term.restore();
    }
    if !json {
        footer(theme, &tally, pushed, wanted);
    }
    Ok(ExitCode::SUCCESS)
}

/// Writes a line, adding the carriage return raw mode needs.
fn emit(raw: bool, line: &str) {
    if raw {
        // The `\r` goes in front, which keeps a JSON line valid — a leading
        // carriage return is whitespace as far as any JSON reader is concerned.
        print!("\r{}", line.replace('\n', "\r\n"));
    } else {
        print!("{line}");
    }
}

/// One line for one event: the bytes, then what they decoded to.
///
/// The bytes come first because they are the fixed-width part, and because they
/// are what a reader is here for.
#[must_use]
pub fn event_line(theme: &Theme, event: &Event, bytes: &[u8], verbose: bool) -> String {
    let sequence = symbolic(bytes);
    let padding = BYTES_WIDTH.saturating_sub(sequence.chars().count());
    let mut line = format!(
        "  {}{}  {}",
        theme.paint(&sequence, Ink::Code),
        " ".repeat(padding),
        theme.paint(&describe_event(event), ink_for(event))
    );
    if verbose {
        line.push_str(&theme.paint(&format!("   [{}]", hex(bytes)), Ink::Muted));
    }
    line
}

/// The colour for an event, so releases and undecoded bytes stand out.
fn ink_for(event: &Event) -> Ink {
    match event {
        Event::Key(key) => match key.kind {
            KeyEventKind::Press => Ink::Strong,
            KeyEventKind::Repeat => Ink::Accent,
            KeyEventKind::Release => Ink::Warn,
        },
        Event::Unknown(_) => Ink::Bad,
        _ => Ink::Muted,
    }
}

/// The banner: what was pushed, and what to press.
fn header(theme: &Theme, pushed: Option<Flags>, wanted: Option<Flags>, piped: bool, raw: bool) {
    let mut lines = vec![String::new()];
    lines.push(format!(
        "  {}",
        theme.paint("escapade keys watch", Ink::Title)
    ));
    lines.push(String::new());

    match pushed {
        Some(flags) => {
            lines.push(format!(
                "  {}  {}",
                theme.paint(&pretty(&push(flags)), Ink::Code),
                theme.paint(&flags.to_string(), Ink::Muted)
            ));
            for spec in flags.specs() {
                lines.push(format!(
                    "      {} {}",
                    theme.paint(&format!("{:>2}", spec.bit), Ink::Code),
                    theme.paint(spec.summary, Ink::Muted)
                ));
            }
        }
        None if piped => lines.push(format!(
            "  {}",
            theme.paint(
                "not a terminal — decoding the bytes on standard input and pushing nothing",
                Ink::Warn
            )
        )),
        None => {
            let reason = if wanted.is_none() {
                "--legacy: nothing pushed, so this is what a program that never heard of the \
                 protocol sees"
            } else {
                "nothing pushed"
            };
            lines.push(format!("  {}", theme.paint(reason, Ink::Warn)));
        }
    }

    lines.push(String::new());
    if !piped {
        lines.push(format!(
            "  {}",
            theme.paint(
                "Press Enter, then Shift+Enter. Then Ctrl-D or Ctrl-C to stop.",
                Ink::Muted
            )
        ));
        lines.push(String::new());
    }
    for line in lines {
        emit(raw, &format!("{line}\n"));
    }
}

/// The closing tally, and the pointer to the other half of the comparison.
fn footer(theme: &Theme, tally: &Tally, pushed: Option<Flags>, wanted: Option<Flags>) {
    println!();
    if tally.events() == 0 {
        println!("  {}", theme.paint("no input", Ink::Muted));
        println!();
        return;
    }

    println!(
        "  {}",
        theme.paint(
            &format!("{} events · distinct encodings per key", tally.events()),
            Ink::Accent
        )
    );
    println!();
    let width = tally
        .summary()
        .iter()
        .map(|(name, _, _)| name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, 16);
    for (name, count, encodings) in tally.summary() {
        let ink = if count > 1 { Ink::Good } else { Ink::Muted };
        println!(
            "    {}  {}  {}",
            theme.paint(&format!("{name:<width$}"), Ink::Strong),
            theme.paint(&format!("{count} encoding{}", plural(count)), ink),
            theme.paint(&encodings, Ink::Code)
        );
    }

    println!();
    if pushed.is_some() {
        println!(
            "  {}",
            theme.paint(&format!("{}   flags popped", pretty(&pop())), Ink::Muted)
        );
    }
    // The other half of the comparison, whichever half this run was.
    let (command, promise) = if wanted.is_none() {
        (
            "escapade keys watch",
            "the same keys with the protocol on — Shift+Enter becomes its own sequence",
        )
    } else {
        (
            "escapade keys watch --legacy",
            "the same keys with nothing enabled — Shift+Enter collapses into Enter",
        )
    };
    println!(
        "  {}  {}",
        theme.paint(command, Ink::Code),
        theme.paint(promise, Ink::Muted)
    );
    println!();
}

/// `s` unless there is exactly one.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::keys::bytes::replay;

    #[test]
    fn the_tally_is_the_legacy_comparison() {
        // Legacy: Enter and Shift+Enter both arrive as one carriage return, so
        // Enter has exactly one encoding however it was pressed.
        let mut legacy = Tally::default();
        for (bytes, event) in replay(b"\r\r") {
            legacy.record(&event, &bytes);
        }
        assert_eq!(
            legacy.summary(),
            vec![("Enter".to_owned(), 1, "CR".to_owned())]
        );

        // With the disambiguate flag the second press is a different sequence,
        // and the tally says so.
        let mut enhanced = Tally::default();
        for (bytes, event) in replay(b"\r\x1b[13;2u") {
            enhanced.record(&event, &bytes);
        }
        assert_eq!(
            enhanced.summary(),
            vec![("Enter".to_owned(), 2, "CR, CSI 13;2u".to_owned())]
        );
        assert_eq!(enhanced.events(), 2);
    }

    #[test]
    fn the_tally_leads_with_the_keys_that_gained_encodings() {
        let mut tally = Tally::default();
        for (bytes, event) in replay(b"a\r\x1b[13;2u\x1b[13;5u") {
            tally.record(&event, &bytes);
        }
        let rows = tally.summary();
        assert_eq!(rows[0].0, "Enter", "the interesting row comes first");
        assert_eq!(rows[0].1, 3);
        assert_eq!(rows[1].0, "a");
        assert_eq!(rows[1].1, 1);
    }

    #[test]
    fn the_tally_counts_non_key_events_but_gives_them_no_row() {
        let mut tally = Tally::default();
        for (bytes, event) in replay(b"\x1b[I\x1b[?5u\x1b[<0;1;1M") {
            tally.record(&event, &bytes);
        }
        assert_eq!(tally.events(), 3);
        assert!(tally.summary().is_empty());
    }

    #[test]
    fn only_a_ctrl_c_or_ctrl_d_press_stops_the_watch() {
        let quits = |bytes: &'static [u8]| replay(bytes).iter().any(|(_, event)| is_quit(event));
        assert!(quits(b"\x04"), "Ctrl-D");
        assert!(quits(b"\x03"), "Ctrl-C");
        assert!(quits(b"\x1b[99;5u"), "Ctrl+c as a protocol sequence");
        assert!(quits(b"\x1b[100;5u"), "Ctrl+d as a protocol sequence");
        assert!(!quits(b"c"), "a bare c is text");
        assert!(!quits(b"d"), "and so is a bare d");
        assert!(!quits(b"\x1b[99u"), "c with no modifier is not Ctrl+c");
        // A *release* of Ctrl-C must not quit: with event types enabled it always
        // follows the press, and quitting on it would swallow the last line.
        assert!(!quits(b"\x1b[99;5:3u"), "release must not quit");
        assert!(quits(b"\x1b[99;5:1u"), "explicit press must");
    }

    #[test]
    fn event_lines_are_safe_to_print_and_show_both_halves() {
        let theme = Theme::plain();
        for (bytes, event) in replay(b"\x1b[13;2u\x1b[2J") {
            for verbose in [false, true] {
                let line = event_line(&theme, &event, &bytes, verbose);
                assert!(!line.contains('\u{1b}'), "{line:?}");
                assert!(line.contains(&symbolic(&bytes)));
                assert!(line.contains(&describe_event(&event)));
            }
        }
        // Verbose adds the hex, plain does not.
        let (bytes, event) = replay(b"\r").pop().unwrap();
        assert!(event_line(&theme, &event, &bytes, true).contains("[0d]"));
        assert!(!event_line(&theme, &event, &bytes, false).contains("[0d]"));
    }

    #[test]
    fn a_summary_of_nothing_does_not_panic() {
        let theme = Theme::plain();
        footer(&theme, &Tally::default(), None, None);
        footer(
            &theme,
            &Tally::default(),
            Some(Flags::ALL),
            Some(Flags::ALL),
        );
    }
}
