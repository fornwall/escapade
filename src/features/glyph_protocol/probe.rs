//! Runtime capability detection.
//!
//! Asking a terminal what it can do is harder than it sounds, because the answer
//! to "does it support this?" arrives as *silence* when it does not. Waiting on a
//! timeout alone is a guess: a slow terminal, a loaded machine or an ssh hop can
//! all look like a refusal.
//!
//! So the query is paired with a **barrier**: a Primary Device Attributes
//! request (`CSI c`) sent immediately afterwards, which every terminal since the
//! VT100 answers. Terminals process input in order, so if the DA1 answer comes
//! back and the Glyph Protocol answer did not, the terminal has demonstrably had
//! its chance. That turns a guess into a conclusion.

use std::{
    io::{IsTerminal as _, stdin, stdout},
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    cli::ProbeFormat,
    features::{
        Support,
        glyph_protocol::{Format, GlyphReply, parse_reply, send, support_query},
    },
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// How long to wait for the terminal to answer. Generous enough for a slow ssh
/// link, short enough not to feel like a hang.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to spend consuming the barrier reply once a conclusion is reached.
///
/// Short, because by this point the terminal has demonstrably answered something
/// and the reply is either already buffered or arriving immediately.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(150);

/// What a probe concluded.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// Whether the terminal answered the capability query.
    pub supported: bool,
    /// Payload formats it advertised.
    pub formats: Vec<Format>,
    /// Format names it advertised that we do not recognise.
    pub unknown_formats: Vec<String>,
    /// Whether the terminal answered the Primary Device Attributes barrier. If
    /// this is false, nothing was heard at all and the result is inconclusive
    /// rather than negative.
    pub answered_barrier: bool,
    /// Multiplexer in the way, if any.
    pub multiplexer: Option<Multiplexer>,
    /// Whether stdin and stdout are both terminals. Without that there is
    /// nothing to probe.
    pub interactive: bool,
    /// Everything the terminal said, in order, for `--verbose`.
    pub replies: Vec<Reply>,
}

/// What a probe is entitled to claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The terminal answered the capability query.
    Supported,
    /// The terminal answered the barrier but not the query, having demonstrably
    /// had its chance.
    Unsupported,
    /// Nothing can honestly be claimed either way.
    Inconclusive,
}

impl ProbeOutcome {
    /// What this probe may legitimately conclude.
    ///
    /// The barrier only licenses a negative when it was answered by the terminal
    /// we are asking about. **A multiplexer answers Primary Device Attributes
    /// itself** — tmux replies `CSI ?1;2c` from its own emulator — so inside one,
    /// a barrier reply says nothing whatsoever about the outer terminal, and
    /// treating it as proof of absence is a confident false claim.
    ///
    /// Turning passthrough on does not rescue this: the barrier is then no longer
    /// ordered behind the query it is supposed to fence, so the reply can arrive
    /// before the thing it was meant to prove had been processed. Under a
    /// multiplexer the honest answer is that we cannot tell.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        if self.supported {
            Verdict::Supported
        } else if self.answered_barrier && self.multiplexer.is_none() {
            Verdict::Unsupported
        } else {
            Verdict::Inconclusive
        }
    }

    /// Whether the probe reached a definite conclusion either way.
    #[must_use]
    pub fn conclusive(&self) -> bool {
        self.verdict() != Verdict::Inconclusive
    }
}

/// Runs the probe using an existing terminal and reader.
///
/// Takes both rather than creating them because there must only ever be one
/// [`InputReader`] per process — two would race for the same bytes on stdin, and
/// each would see a random half of the terminal's answer.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal
/// that simply stays quiet is not an error; it is a negative result.
pub fn probe(term: &mut Term, input: &mut InputReader) -> Result<ProbeOutcome> {
    let mut outcome = ProbeOutcome {
        multiplexer: Multiplexer::detect(),
        interactive: stdout().is_terminal() && stdin().is_terminal(),
        ..ProbeOutcome::default()
    };
    if !outcome.interactive {
        return Ok(outcome);
    }

    // Raw mode is required: in cooked mode the reply would sit in the line
    // buffer until the user pressed Enter, and would be echoed back at them.
    term.enable_raw()?;
    input.drain();

    send(term, outcome.multiplexer, &support_query())?;
    // The barrier is deliberately *not* wrapped for the multiplexer, so that it
    // reports on the round trip we can actually reason about. Note that a
    // multiplexer answers it from its own emulator rather than forwarding it,
    // which is precisely why a barrier reply cannot license a negative verdict
    // from inside one — see `ProbeOutcome::verdict`.
    term.write_now(&format!("{CSI}c"))?;

    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // An EOF on stdin means there is no one to answer; treat it as silence.
        let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
            break;
        };
        outcome.replies.push(reply.clone());

        match &reply {
            Reply::Apc(payload) => {
                if let Some(GlyphReply::Support { formats, unknown }) = parse_reply(payload) {
                    outcome.supported = true;
                    outcome.formats = formats;
                    outcome.unknown_formats = unknown;
                    break;
                }
            }
            Reply::Csi { final_byte: 'c', .. } => {
                // The barrier came back. Anything sent before it has already
                // been processed, so continuing to wait would learn nothing.
                outcome.answered_barrier = true;
                break;
            }
            _ => {}
        }
    }

    // The affirmative answer arrives before the barrier, so concluding early
    // leaves the barrier reply in flight. Unread, it is delivered to whatever
    // runs next — the user's shell, which prints it as stray text. Consuming it
    // here is what keeps probing invisible.
    if !outcome.answered_barrier
        && let Ok(Some(reply)) = input.wait_for_reply(DRAIN_TIMEOUT, |reply| {
            matches!(reply, Reply::Csi { final_byte: 'c', .. })
        })
    {
        outcome.replies.push(reply);
        outcome.answered_barrier = true;
    }

    Ok(outcome)
}

/// Runs a probe from scratch, for `escapade glyph probe`.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn detect() -> Result<ProbeOutcome> {
    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let outcome = probe(&mut term, &mut input);
    // Restore before the caller prints anything, so the report appears in a
    // normal cooked-mode terminal.
    term.restore();
    outcome
}

/// The three-state answer this probe licenses.
#[must_use]
pub fn support(outcome: &ProbeOutcome) -> Support {
    match outcome.verdict() {
        Verdict::Supported => Support::Yes,
        Verdict::Unsupported => Support::No,
        Verdict::Inconclusive => Support::Unknown,
    }
}

/// Runs `escapade glyph probe` and reports through the exit status.
///
/// Silent unless a `--format` is given, so it composes as a shell predicate.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn report(theme: &Theme, format: Option<ProbeFormat>) -> Result<ExitCode> {
    let outcome = detect()?;
    let support = support(&outcome);
    match format {
        None => {}
        Some(ProbeFormat::Json) => print!("{}", json_line(&outcome, support)),
        Some(ProbeFormat::Debug) => debug_report(theme, &outcome),
    }
    Ok(support.exit_code())
}

/// One JSON object, one line — the house machine-readable shape.
fn json_line(outcome: &ProbeOutcome, support: Support) -> String {
    let formats: Vec<String> = outcome
        .formats
        .iter()
        .map(|format| format!("\"{}\"", format.as_str()))
        .chain(
            outcome
                .unknown_formats
                .iter()
                .map(|name| format!("\"{}\"", escape_json(name))),
        )
        .collect();
    let multiplexer = outcome
        .multiplexer
        .map_or_else(|| "null".to_owned(), |m| format!("\"{}\"", m.name()));
    format!(
        "{{\"feature\":\"glyph-protocol\",\"verdict\":\"{}\",\"formats\":[{}],\
         \"answered_barrier\":{},\"multiplexer\":{multiplexer},\"interactive\":{}}}\n",
        support.as_str(),
        formats.join(","),
        outcome.answered_barrier,
        outcome.interactive,
    )
}

/// Minimal JSON string escaping, for the few strings that reach the output.
fn escape_json(text: &str) -> String {
    text.chars()
        .flat_map(|ch| match ch {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            ch if ch.is_control() => format!("\\u{:04x}", ch as u32).chars().collect(),
            ch => vec![ch],
        })
        .collect()
}

/// The human-readable findings, for `--format debug`.
#[expect(
    clippy::too_many_lines,
    reason = "one screen of output, written top to bottom; splitting it would \
              scatter the layout across functions without simplifying anything"
)]
fn debug_report(theme: &Theme, outcome: &ProbeOutcome) {
    let verbose = true;

    println!();
    println!("  {}", theme.paint("Glyph Protocol support", Ink::Title));
    println!();

    let term_name = std::env::var("TERM").unwrap_or_else(|_| "unset".to_owned());
    let program = std::env::var("TERM_PROGRAM").ok();
    print!("  {} {}", theme.paint("terminal:", Ink::Muted), term_name);
    if let Some(program) = program {
        print!("{}", theme.paint(&format!("  (TERM_PROGRAM={program})"), Ink::Muted));
    }
    println!();
    if let Some(multiplexer) = outcome.multiplexer {
        println!(
            "  {} {}",
            theme.paint("multiplexer:", Ink::Muted),
            theme.paint(multiplexer.name(), Ink::Warn)
        );
    }
    println!();

    if !outcome.interactive {
        println!(
            "  {} cannot probe — stdin and stdout are not both terminals",
            theme.paint("?", Ink::Warn),
        );
        println!(
            "    {}",
            theme.paint(
                "Capability detection needs a real terminal to answer. Run this directly, not through a pipe.",
                Ink::Muted
            )
        );
        println!();
        return;
    }

    if outcome.supported {
        println!(
            "  {} {}",
            theme.paint("✔", Ink::Good),
            theme.paint("supported", Ink::Good)
        );
        let mut formats: Vec<&str> = outcome.formats.iter().map(|f| f.as_str()).collect();
        formats.extend(outcome.unknown_formats.iter().map(String::as_str));
        println!(
            "    {} {}",
            theme.paint("payload formats:", Ink::Muted),
            theme.paint(&formats.join(", "), Ink::Code)
        );
        if !outcome.unknown_formats.is_empty() {
            println!(
                "    {}",
                theme.paint(
                    &format!(
                        "escapade does not know about: {} — newer than this build",
                        outcome.unknown_formats.join(", ")
                    ),
                    Ink::Muted
                )
            );
        }
    } else if outcome.verdict() == Verdict::Unsupported {
        println!(
            "  {} {}",
            theme.paint("✘", Ink::Bad),
            theme.paint("not supported", Ink::Bad)
        );
        println!(
            "    {}",
            theme.paint(
                "The terminal answered the device-attributes barrier but ignored the capability query, so this is a real no rather than a timeout.",
                Ink::Muted
            )
        );
    } else {
        println!(
            "  {} {}",
            theme.paint("?", Ink::Warn),
            theme.paint("inconclusive", Ink::Warn)
        );
        // Three different reasons land here, and they call for different advice.
        let explanation = match (outcome.multiplexer, outcome.answered_barrier) {
            (Some(multiplexer), true) => format!(
                "The device-attributes barrier came back, but {} answers that itself rather than \
                 forwarding it — so it says nothing about the terminal underneath. Inside a \
                 multiplexer this cannot be determined either way.",
                multiplexer.name()
            ),
            (Some(multiplexer), false) => format!(
                "Nothing came back at all, and {} is in the way. It filters escape sequences it \
                 does not recognise, so both the query and its answer may have been dropped.",
                multiplexer.name()
            ),
            (None, _) => "Nothing came back at all — not even the device-attributes barrier \
                          every terminal answers. Something between here and the terminal is \
                          swallowing replies."
                .to_owned(),
        };
        for line in crate::features::wrap(&explanation, theme.width().saturating_sub(4)) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
    }

    if let Some(multiplexer) = outcome.multiplexer
        && !outcome.supported
    {
        println!();
        println!(
            "  {} {}",
            theme.paint("note:", Ink::Warn),
            format_args!(
                "{} filters unknown escape sequences by default.",
                multiplexer.name()
            )
        );
        println!("    {}", theme.paint(multiplexer.passthrough_hint(), Ink::Code));
    }

    if verbose {
        println!();
        println!("  {}", theme.paint("what was sent", Ink::Accent));
        let query = match outcome.multiplexer {
            Some(multiplexer) => multiplexer.wrap(&support_query()),
            None => support_query(),
        };
        println!("    {}", theme.paint(&pretty(&query), Ink::Code));
        println!(
            "    {}  {}",
            theme.paint(&pretty(&format!("{CSI}c")), Ink::Code),
            theme.paint("← the barrier", Ink::Muted)
        );

        println!();
        println!("  {}", theme.paint("what came back", Ink::Accent));
        if outcome.replies.is_empty() {
            println!("    {}", theme.paint("(nothing)", Ink::Muted));
        }
        for reply in &outcome.replies {
            println!("    {}", theme.paint(&describe_reply(reply), Ink::Code));
        }
    }

    println!();
    println!(
        "  {} {}",
        theme.paint("escapade glyph explain", Ink::Code),
        theme.paint("for what this protocol is and who implements it", Ink::Muted)
    );
    println!();
}

/// One-line rendering of a reply, safe to print.
fn describe_reply(reply: &Reply) -> String {
    match reply {
        Reply::Apc(payload) => format!("APC {payload} ST"),
        Reply::Dcs(payload) => format!("DCS {payload} ST"),
        Reply::Osc(payload) => format!("OSC {payload} ST"),
        Reply::Csi { params, final_byte } => format!("CSI {params}{final_byte}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_barrier_answered_inside_a_multiplexer_proves_nothing() {
        // tmux answers Primary Device Attributes from its own emulator, so the
        // barrier says nothing about the terminal beyond it. Claiming
        // "not supported" here was a confident false statement.
        let outcome = ProbeOutcome {
            supported: false,
            answered_barrier: true,
            interactive: true,
            multiplexer: Some(Multiplexer::Tmux),
            ..ProbeOutcome::default()
        };
        assert_eq!(outcome.verdict(), Verdict::Inconclusive);
        assert!(!outcome.conclusive());
    }

    #[test]
    fn support_is_still_conclusive_inside_a_multiplexer() {
        // A positive answer needs no barrier: something replied in the protocol's
        // own language, which only the real terminal could have done.
        let outcome = ProbeOutcome {
            supported: true,
            interactive: true,
            multiplexer: Some(Multiplexer::Tmux),
            ..ProbeOutcome::default()
        };
        assert_eq!(outcome.verdict(), Verdict::Supported);
    }

    #[test]
    fn a_silent_terminal_that_answered_the_barrier_is_a_definite_no() {
        let outcome = ProbeOutcome {
            supported: false,
            answered_barrier: true,
            interactive: true,
            multiplexer: None,
            ..ProbeOutcome::default()
        };
        assert_eq!(outcome.verdict(), Verdict::Unsupported);
        assert!(outcome.conclusive());
    }

    #[test]
    fn total_silence_is_inconclusive_not_negative() {
        let outcome = ProbeOutcome {
            interactive: true,
            ..ProbeOutcome::default()
        };
        assert!(
            !outcome.conclusive(),
            "no barrier reply means we learned nothing"
        );
    }

    #[test]
    fn support_implies_a_conclusion_even_without_the_barrier() {
        // The barrier reply is usually still in flight when we stop listening,
        // because the affirmative answer arrives first and ends the wait.
        let outcome = ProbeOutcome {
            supported: true,
            formats: vec![Format::Glyf],
            interactive: true,
            ..ProbeOutcome::default()
        };
        assert!(outcome.conclusive());
        assert!(!outcome.answered_barrier);
        assert_eq!(outcome.formats, vec![Format::Glyf]);
    }

    #[test]
    fn replies_render_without_control_bytes() {
        let replies = [
            Reply::Apc("25a1;s;fmt=glyf".to_owned()),
            Reply::Csi {
                params: "?62;4".to_owned(),
                final_byte: 'c',
            },
        ];
        for reply in &replies {
            let rendered = describe_reply(reply);
            assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        }
    }
}
