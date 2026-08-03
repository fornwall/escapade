//! Clipboard access from wherever the program happens to be running (OSC 52).
//!
//! ```text
//! OSC 52 ; Pc ; Pd ST
//! ```
//!
//! The clipboard belongs to the machine the human is sitting at. The program
//! usually does not run there — it runs over ssh, in a container, in a CI runner,
//! in a VM — and every clipboard API there is talks to the local window system,
//! which is the wrong one. `xclip`, `pbcopy` and `wl-copy` all fail for the same
//! reason: they are on the wrong side of the network.
//!
//! This sequence goes the other way. It travels back up the same stream the
//! program's output does, arrives at the terminal emulator the human is looking
//! at, and that program — which *is* on the right machine — puts the text on the
//! clipboard. It is the only mechanism that works from the far end of an ssh hop,
//! and it is why every editor worth using has reimplemented it.
//!
//! # Two very different halves
//!
//! Writing is ordinary. It has no reply, it is discarded by terminals that do not
//! implement it, and the base64 payload cannot smuggle an `ESC` out into the
//! stream, so it is safe to emit unconditionally.
//!
//! Reading is not ordinary at all: it is the only sequence in this project that
//! carries data *out* of the user's session, on the input stream, invisibly. A
//! program on a compromised host could take the password you copied a minute ago.
//! That is why terminals refuse it by default, why this crate never sends it
//! except when the user asked for it by name, and why [`read`] is a module of its
//! own with a `--force` on the way in.
//!
//! # The modules
//!
//! * [`sequence`] — selections, base64 and the three sequences.
//! * [`probe`] — what can honestly be said about support, which is more than
//!   usual: the clipboard extension puts the answer in the device-attributes
//!   reply.
//! * [`read`] — the query round trip, and refusing it where it destroys.
//! * [`demo`] — the step-by-step walkthrough.

pub mod demo;
pub mod probe;
pub mod read;
pub mod sequence;

use std::{
    io::{IsTerminal as _, Write as _, stdin, stdout},
    process::ExitCode,
};

use anyhow::{Result, bail};

use crate::{
    cli::{ClipboardAction, ClipboardCommand},
    db::Catalog,
    features::{bytes_operand, clipboard::sequence::Selection},
    input::InputReader,
    term::{Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// The feature id in the database. The subcommand is `clipboard`.
pub const FEATURE_ID: &str = "clipboard";

/// Whether this process can ask the terminal anything at all.
///
/// Both directions are needed for a read: the query goes out on stdout and the
/// answer arrives on stdin. A *write* needs neither — it is fire and forget — so
/// this is deliberately not consulted before copying.
#[must_use]
pub fn interactive() -> bool {
    stdout().is_terminal() && stdin().is_terminal()
}

/// Entry point for `escapade clipboard …`.
///
/// Returns an exit status because `probe` is a predicate: `0` supported, `1` not
/// supported, `2` cannot tell. `paste` reports a refusal as a failure, since a
/// script doing `clip=$(escapade clipboard paste)` must not carry on with an empty
/// string as though it had read one.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with.
pub fn run(catalog: &Catalog, command: &ClipboardCommand, no_color: bool) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        ClipboardAction::Explain { no_matrix } => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        ClipboardAction::Probe { format } => probe::report(&theme, catalog, *format),
        ClipboardAction::Copy {
            text,
            selection,
            dry_run,
        } => {
            copy(&theme, text, Selection::from(*selection), *dry_run)?;
            Ok(ExitCode::SUCCESS)
        }
        ClipboardAction::Paste { selection, force } => {
            paste(&theme, catalog, Selection::from(*selection), *force)
        }
        ClipboardAction::Demo {
            force,
            only,
            step,
            list,
        } => {
            if *list {
                demo::list_steps(&theme);
            } else {
                demo::run(&theme, catalog, *force, only.as_deref(), *step)?;
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

// ---------------------------------------------------------------------------
// Copying
// ---------------------------------------------------------------------------

/// Puts text on the clipboard, or prints what it would send.
///
/// The operand is read as **bytes**, not text, and the trailing newline is kept.
/// That is the `pbcopy` contract rather than the `$(…)` one: what goes on the
/// clipboard is what was piped in, byte for byte, because a clipboard is not
/// obliged to hold UTF-8 and because trimming somebody's file for them is not this
/// program's business. Callers who want the newline gone can say
/// `printf %s "$value" | escapade clipboard copy`.
///
/// # Errors
///
/// Fails if no text was given and standard input is a terminal, or if the terminal
/// cannot be written to.
fn copy(theme: &Theme, text: &[String], selection: Selection, dry_run: bool) -> Result<()> {
    let data = bytes_operand(text)?;
    let sequence = sequence::set(selection, &data);
    let multiplexer = Multiplexer::detect();

    // Everything explanatory goes to stderr, so a `--dry-run` piped into a file
    // contains the sequence and nothing else.
    let mut err = std::io::stderr();
    for note in sequence::hazards(
        &data,
        sequence.len(),
        multiplexer == Some(Multiplexer::Tmux),
    ) {
        let _ = writeln!(
            err,
            "{} {}",
            theme.paint("clipboard:", Ink::Warn),
            theme.paint(&note, Ink::Muted)
        );
    }
    if let Some(multiplexer) = multiplexer {
        // One line, wrapped by the terminal rather than by hand: repeating the
        // `clipboard:` prefix down six lines of explanation reads as six problems.
        let _ = writeln!(
            err,
            "{} {}",
            theme.paint("clipboard:", Ink::Muted),
            theme.paint(&multiplexer_note(multiplexer), Ink::Muted)
        );
    }

    if dry_run {
        print_dry_run(theme, selection, &data, &sequence, multiplexer);
        return Ok(());
    }

    // A copy does not need a terminal on *stdin* — nothing comes back — but it
    // does need one on stdout, or the sequence goes wherever stdout goes and
    // nothing reaches the clipboard. Said rather than fixed: redirecting it is a
    // reasonable thing to do on purpose, and stderr cannot corrupt the capture.
    if !stdout().is_terminal() {
        let _ = writeln!(
            err,
            "{} {}",
            theme.paint("clipboard:", Ink::Warn),
            theme.paint(
                "stdout is not a terminal, so the sequence went into the pipe rather than to the \
                 clipboard",
                Ink::Muted
            )
        );
    }

    let mut term = Term::new()?;
    term.write_now(&sequence)
}

/// What a multiplexer will do with the sequence, which is rarely "forward it".
///
/// tmux gets the long version because tmux is where this feature most often
/// appears not to work, and because the fix is one line of configuration that
/// nobody guesses.
fn multiplexer_note(multiplexer: Multiplexer) -> String {
    match multiplexer {
        Multiplexer::Tmux => format!(
            "inside tmux, which drops this sequence unless `set -s set-clipboard on` — the \
             default is `external`, and that means tmux sets the terminal clipboard for its own \
             copy-mode copies while ignoring the sequence from applications. With it on, tmux also \
             needs the terminal's `Ms` capability to pass anything outward: `{}`",
            "set -as terminal-features ',*:clipboard'"
        ),
        Multiplexer::Screen => "inside GNU Screen, which does not implement this sequence at all, \
                                so it will be swallowed"
            .to_owned(),
        Multiplexer::Zellij => "inside Zellij, which implements the write itself: the text will \
                                reach the system clipboard through Zellij rather than through the \
                                terminal"
            .to_owned(),
    }
}

/// Prints the sequence a copy would have sent, in readable form.
fn print_dry_run(
    theme: &Theme,
    selection: Selection,
    data: &[u8],
    sequence: &str,
    multiplexer: Option<Multiplexer>,
) {
    println!();
    println!("  {}", theme.paint(&pretty(sequence), Ink::Code));
    println!();
    for (label, value) in [
        (
            "selection",
            format!("{} — {}", selection.label(), selection.pasted_with()),
        ),
        (
            "payload",
            format!(
                "{} byte{} in, {} of base64 out",
                data.len(),
                if data.len() == 1 { "" } else { "s" },
                sequence::encode(data).len()
            ),
        ),
        ("on the wire", format!("{} bytes", sequence.len())),
    ] {
        println!(
            "  {}  {}",
            theme.paint(&format!("{label:>11}"), Ink::Accent),
            theme.paint(&value, Ink::Muted)
        );
    }
    if let Some(multiplexer) = multiplexer {
        println!(
            "  {}  {}",
            theme.paint(&format!("{:>11}", "sent as"), Ink::Accent),
            theme.paint(
                &format!(
                    "unwrapped, even inside {} — this is not an exotic sequence a multiplexer \
                     needs passthrough for, it is one every multiplexer has an opinion about",
                    multiplexer.name()
                ),
                Ink::Muted
            )
        );
    }
    if !stdout().is_terminal() {
        println!(
            "  {}  {}",
            theme.paint(&format!("{:>11}", "note"), Ink::Accent),
            theme.paint(
                "stdout is not a terminal, so a real copy would go into the pipe rather than to \
                 the clipboard",
                Ink::Muted
            )
        );
    }
    println!();
}

// ---------------------------------------------------------------------------
// Pasting
// ---------------------------------------------------------------------------

/// Reads the clipboard back and writes it to stdout, byte for byte.
///
/// The identity round trip happens first, and not for the verdict: it is there to
/// find out whether asking this terminal would *destroy* the clipboard, which it
/// would on Konsole. Only then is the query sent.
///
/// # Errors
///
/// Fails when the terminal declines to answer, with the reason, so that
/// `clip=$(escapade clipboard paste)` fails loudly rather than yielding an empty
/// string that looks like an empty clipboard.
fn paste(theme: &Theme, catalog: &Catalog, selection: Selection, force: bool) -> Result<ExitCode> {
    if !interactive() {
        bail!(
            "reading the clipboard needs a terminal on both stdin and stdout — the query goes out \
             on one and the answer comes back on the other. Copying has no such requirement."
        );
    }

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;

    let identity = probe::probe(catalog, &mut term, &mut input)?;
    if read::konsole_hazard(&identity) && !force {
        term.restore();
        bail!(
            "this looks like Konsole, where asking for the clipboard destroys it: Konsole has no \
             branch for the `?` payload, so the query takes the write path, the `?` is discarded \
             as invalid base64, and the clipboard is set to the empty string. Nothing was sent. \
             Pass --force to ask anyway."
        );
    }

    // How long to wait depends on whether a human is in the loop: see
    // `read::PROMPT_TIMEOUT`, where the barrier stops being a conclusion.
    let patience = if read::asks_first(&identity) {
        let _ = writeln!(
            std::io::stderr(),
            "{} {}",
            theme.paint("clipboard:", Ink::Muted),
            theme.paint(
                "this terminal asks before letting a program read the clipboard — answer the \
                 prompt it is showing you",
                Ink::Muted
            )
        );
        read::PROMPT_TIMEOUT
    } else {
        read::REPLY_TIMEOUT
    };
    let outcome = read::ask(&mut term, &mut input, selection, patience);
    term.restore();
    let outcome = outcome?;

    let Some(reply) = outcome.reply else {
        let refusal = outcome
            .refusal()
            .unwrap_or_else(|| "the terminal did not answer".to_owned());
        bail!("{refusal}");
    };

    let Some(data) = reply.data else {
        // Decoding is strict on purpose, so this is reachable and worth reporting
        // with the payload attached rather than guessed at: whatever the terminal
        // sent, it was not the base64 the protocol calls for.
        bail!(
            "the terminal answered the clipboard query with something that is not base64: {:?}",
            reply.raw
        );
    };

    // Written as bytes, not as a string: a clipboard can hold anything, and a
    // lossy conversion would quietly corrupt whatever it held.
    let mut out = stdout().lock();
    out.write_all(&data)?;
    out.flush()?;

    if !data.is_empty() && data.last() != Some(&b'\n') && stdout().is_terminal() {
        // A prompt landing mid-line after a clipboard with no trailing newline is
        // the usual mess. The marker goes to stderr so the captured value is
        // exactly the clipboard.
        let _ = writeln!(std::io::stderr(), "{}", theme.paint("⏎", Ink::Muted));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_feature_id_matches_the_database() {
        let catalog = Catalog::load().unwrap();
        assert!(catalog.feature(FEATURE_ID).is_ok());
    }

    #[test]
    fn the_data_file_is_complete_enough_to_explain() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        assert!(feature.summary.len() > 400, "the summary is a placeholder");
        assert!(
            feature.security.is_some(),
            "this feature is mostly security"
        );
        assert!(feature.how_it_works.is_some());
        assert!(!feature.sequence.is_empty());
        assert!(!feature.support.is_empty());
        assert!(!feature.resource.is_empty());
    }

    #[test]
    fn every_support_row_cites_a_source() {
        // The house rule from CONTRIBUTING.md, and the one that keeps this file
        // from becoming folklore. `unknown` rows are exempt: they are the honest
        // admission that nobody checked.
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        for row in &feature.support {
            if row.status == crate::db::SupportStatus::Unknown {
                continue;
            }
            assert!(
                row.source.is_some(),
                "the {} row claims {} with no source",
                row.terminal,
                row.status.label()
            );
        }
    }

    #[test]
    fn the_selections_the_cli_offers_are_the_ones_the_protocol_has() {
        use crate::cli::ClipboardSelection;
        assert_eq!(Selection::from(ClipboardSelection::Clipboard).code(), 'c');
        assert_eq!(Selection::from(ClipboardSelection::Primary).code(), 'p');
        assert_eq!(Selection::from(ClipboardSelection::Select).code(), 's');
    }

    #[test]
    fn the_tmux_note_names_the_option_that_actually_fixes_it() {
        let note = multiplexer_note(Multiplexer::Tmux);
        assert!(note.contains("set-clipboard on"), "{note}");
        assert!(note.contains("external"), "{note}");
        // Screen has no implementation at all, which is a different sentence.
        assert!(multiplexer_note(Multiplexer::Screen).contains("does not implement"));
        // Zellij handles it itself rather than forwarding, which is a third one.
        assert!(multiplexer_note(Multiplexer::Zellij).contains("itself"));
    }
}
