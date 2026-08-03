//! Reading the clipboard back, and refusing to do it where it does harm.
//!
//! ```text
//! →  OSC 52 ; c ; ? ST
//! →  CSI c                     the barrier
//! ←  OSC 52 ; c ; <base64> ST
//! ```
//!
//! Unlike the write, this has a reply, so it can be measured rather than
//! inferred — and unlike every other query in this project, sending it is not
//! free. Three things can happen that are not "an answer comes back":
//!
//! * **A dialog appears.** kitty defaults to `read-clipboard-ask` and Ghostty to
//!   `clipboard-read = ask`, so the terminal interrupts the user to ask whether
//!   this program may read their clipboard. That is the system working as
//!   designed, and it is why nothing here asks without being told to.
//! * **Nothing at all happens.** The common case: Alacritty, WezTerm, Windows
//!   Terminal, Zellij and stock xterm all ignore the query. There is no error
//!   reply, so silence under the barrier is the only signal.
//! * **The clipboard is destroyed.** Konsole has no branch for `?` — it takes the
//!   write path, `QByteArray::fromBase64` drops the `?` as an invalid character,
//!   and the clipboard is set to the empty string. The user asked to read their
//!   clipboard and lost it instead, which is why [`konsole_hazard`] exists and why
//!   it takes `--force` to get past.
//!
//! The barrier is what makes silence mean anything: `CSI c` goes out behind the
//! query, and a terminal that answers the barrier without answering the query has
//! demonstrably had its chance. Inside a multiplexer even that proves nothing,
//! because the multiplexer answers the barrier itself.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::{
    features::clipboard::{
        probe::{self, ProbeOutcome},
        sequence::{self, Selection},
    },
    input::{InputReader, Reply as InputReply},
    term::{Multiplexer, Term},
};

/// How long to wait for a clipboard that is coming or not coming.
///
/// A second, the house figure, and in practice the wait ends far sooner: the
/// barrier comes back and settles it.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to wait when the answer depends on a *human*.
///
/// kitty and Ghostty raise a permission dialog for a read, and — this is the part
/// that breaks the usual reasoning — **the barrier does not wait for it**. The
/// device-attributes request is parsed and answered while the dialog is still on
/// screen, so the ordering argument that makes silence conclusive everywhere else
/// in this project is worthless here: concluding at the barrier would report a
/// refusal at the exact moment the user is reaching for the mouse.
///
/// So when the terminal is one that asks, the barrier is recorded and the wait
/// continues. Twenty seconds is long enough to read a dialog and click it.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to spend consuming a barrier reply still in flight.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// What one read established.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// The clipboard, if the terminal answered.
    pub reply: Option<sequence::Reply>,
    /// Whether the device-attributes barrier came back.
    pub answered_barrier: bool,
    /// A multiplexer between this process and the terminal, if any.
    pub multiplexer: Option<Multiplexer>,
    /// Whether stdin and stdout are both terminals.
    pub interactive: bool,
    /// Replies that were heard but answered nothing that was asked.
    pub stray: Vec<InputReply>,
}

impl Outcome {
    /// Why nothing came back, in one sentence. `None` when something did.
    ///
    /// Silence is only ever a refusal when the barrier came back and no
    /// multiplexer is in the way. tmux answers device attributes from its own
    /// emulator, and it answers this query itself too — with its newest paste
    /// buffer rather than the system clipboard — so inside one, neither silence
    /// nor an answer describes the terminal beyond it.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        if self.reply.is_some() {
            return None;
        }
        if !self.interactive {
            return Some(
                "stdin and stdout are not both terminals, so the query had nowhere to go and the \
                 answer had nowhere to come back from — reading needs a terminal on both ends, \
                 even though copying does not"
                    .to_owned(),
            );
        }
        if let Some(multiplexer) = self.multiplexer {
            return Some(format!(
                "nothing came back, and {} handles this query itself rather than passing it on, \
                 so this says nothing about the terminal beyond it",
                multiplexer.name()
            ));
        }
        if self.answered_barrier {
            return Some(
                "the terminal answered the device-attributes barrier and did not answer the \
                 clipboard query, so it had its chance and declined — which is the default nearly \
                 everywhere, and deliberate: a program that can read your clipboard can read the \
                 password you copied a moment ago"
                    .to_owned(),
            );
        }
        Some(
            "neither the clipboard nor the device-attributes barrier came back, so something \
             between here and the terminal is swallowing replies and nothing was established"
                .to_owned(),
        )
    }
}

/// Whether this terminal is likely to ask the user before answering.
///
/// kitty defaults to `read-clipboard-ask` and Ghostty to `clipboard-read = ask`.
/// Out-of-band evidence counts, as it does for [`konsole_hazard`]: being wrong
/// costs a longer wait on a terminal that was never going to answer, and being
/// right is the difference between a permission dialog and a false refusal.
#[must_use]
pub fn asks_first(outcome: &ProbeOutcome) -> bool {
    outcome
        .evidence
        .iter()
        .any(|item| matches!(item.implies, Some("kitty" | "ghostty")))
}

/// Whether asking this terminal for its clipboard would destroy it.
///
/// True when *any* evidence points at Konsole, in band or not. That is a lower
/// bar than any verdict in this project accepts, on purpose: the cost of a false
/// positive is a refusal the user can override with `--force`, and the cost of a
/// false negative is the user's clipboard.
#[must_use]
pub fn konsole_hazard(outcome: &ProbeOutcome) -> bool {
    outcome
        .evidence
        .iter()
        .any(|item| item.implies == Some("konsole"))
}

/// Asks the terminal for one selection, and waits for it.
///
/// Whatever happens, the barrier reply is consumed before returning: an unread
/// reply is delivered to whatever runs next, and a shell prompt printing
/// `52;c;aGVsbG8=` is both ugly and a small information leak in its own right.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal that
/// declines to answer is not an error; it is the usual outcome.
pub fn ask(
    term: &mut Term,
    input: &mut InputReader,
    selection: Selection,
    patience: Duration,
) -> Result<Outcome> {
    let mut outcome = Outcome {
        multiplexer: Multiplexer::detect(),
        interactive: crate::features::clipboard::interactive(),
        ..Outcome::default()
    };
    if !outcome.interactive {
        return Ok(outcome);
    }

    term.enable_raw()?;
    input.drain();

    // Shielded: `OSC 52 ; c ; ? ST` is a control string, and a terminal that does
    // not implement it can print `52;c;?` rather than discarding it.
    term.write_query(&sequence::query(selection))?;
    term.write_now(&probe::barrier_query())?;

    let expect_prompt = patience > REPLY_TIMEOUT;
    let deadline = Instant::now() + patience;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
            break;
        };
        match &reply {
            InputReply::Osc(payload) => match sequence::parse_reply(payload) {
                // The selection is checked as well as the number: a terminal
                // answering about the primary selection when the clipboard was
                // asked for has not answered the question.
                Some(parsed) if parsed.mentions(selection) => {
                    outcome.reply = Some(parsed);
                    break;
                }
                _ => outcome.stray.push(reply.clone()),
            },
            InputReply::Csi {
                final_byte: 'c', ..
            } => {
                // Everything sent before the barrier has been processed, so the
                // query has been seen — and, on a terminal that does not ask the
                // user, ignored. On one that does, the dialog is still open and
                // the barrier has raced past it, so the wait carries on.
                outcome.answered_barrier = true;
                if !expect_prompt {
                    break;
                }
            }
            _ => outcome.stray.push(reply.clone()),
        }
    }

    if !outcome.answered_barrier
        && let Ok(Some(reply)) = input.wait_for_reply(DRAIN_TIMEOUT, |reply| {
            matches!(
                reply,
                InputReply::Csi {
                    final_byte: 'c',
                    ..
                }
            )
        })
    {
        outcome.stray.retain(|kept| kept != &reply);
        outcome.answered_barrier = true;
    }

    Ok(outcome)
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::clipboard::probe::Evidence;

    fn silent(interactive: bool, answered_barrier: bool) -> Outcome {
        Outcome {
            answered_barrier,
            interactive,
            ..Outcome::default()
        }
    }

    #[test]
    fn an_answer_needs_no_explanation() {
        let outcome = Outcome {
            reply: sequence::parse_reply("52;c;aGVsbG8="),
            interactive: true,
            answered_barrier: true,
            ..Outcome::default()
        };
        assert!(outcome.refusal().is_none());
        assert_eq!(outcome.reply.unwrap().data.as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn silence_under_the_barrier_is_a_refusal_and_says_why_it_is_the_default() {
        let outcome = silent(true, true);
        let refusal = outcome.refusal().unwrap();
        assert!(refusal.contains("had its chance"), "{refusal}");
        assert!(refusal.contains("password"), "{refusal}");
    }

    #[test]
    fn silence_without_a_barrier_establishes_nothing() {
        let outcome = silent(true, false);
        assert!(outcome.refusal().unwrap().contains("swallowing"));
    }

    #[test]
    fn inside_a_multiplexer_neither_answer_describes_the_terminal() {
        let mut outcome = silent(true, true);
        outcome.multiplexer = Some(Multiplexer::Tmux);
        assert!(outcome.refusal().unwrap().contains("tmux"));
    }

    #[test]
    fn without_a_terminal_the_refusal_says_copying_still_works() {
        let outcome = silent(false, false);
        let refusal = outcome.refusal().unwrap();
        assert!(refusal.contains("copying does not"), "{refusal}");
    }

    #[test]
    fn a_terminal_that_asks_the_user_is_waited_for_rather_than_written_off() {
        // The barrier argument fails exactly here: kitty answers device
        // attributes while its permission dialog is still on screen.
        assert!(PROMPT_TIMEOUT > REPLY_TIMEOUT);

        let mut outcome = ProbeOutcome::default();
        assert!(!asks_first(&outcome));
        outcome.evidence.push(Evidence {
            source: "XTVERSION",
            value: "kitty 0.44.0".to_owned(),
            implies: Some("kitty"),
            in_band: true,
        });
        assert!(asks_first(&outcome));
    }

    #[test]
    fn konsole_is_recognised_from_any_evidence_at_all() {
        let mut outcome = ProbeOutcome::default();
        assert!(!konsole_hazard(&outcome));

        // Out of band is enough here, unlike everywhere else in this project:
        // being wrong costs a --force, and being right saves the clipboard.
        outcome.evidence.push(Evidence {
            source: "KONSOLE_VERSION",
            value: "240800".to_owned(),
            implies: Some("konsole"),
            in_band: false,
        });
        assert!(konsole_hazard(&outcome));
    }

    #[test]
    fn the_query_that_goes_out_is_the_documented_one() {
        assert_eq!(
            crate::term::pretty(&sequence::query(Selection::Clipboard)),
            "OSC 52;c;? ST"
        );
    }
}
