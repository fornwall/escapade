//! Asking the terminal what it does with the keyboard — and measuring the answer.
//!
//! This protocol is one of the few with a real query, so there is no need to
//! infer anything from a terminal's name or its `TERM` variable:
//!
//! ```text
//! →  CSI ? u        which flags are in effect?
//! ←  CSI ? 0 u      none — but something answered, which is the point
//! ```
//!
//! # Why one query is not enough
//!
//! `CSI ? u` reports the flags **currently set**, not the flags the terminal is
//! *capable of*. A terminal that implements every one of them answers `0` on a
//! fresh session, because nothing has asked for anything yet. Reading that as
//! "supports nothing" would be wrong in the most misleading direction.
//!
//! So the probe measures instead of asking twice:
//!
//! ```text
//! →  CSI ? u        1. baseline: what is set right now?
//! →  CSI > 31 u     2. push every flag the specification defines
//! →  CSI ? u        3. and now what is set?
//! →  CSI < u        4. pop — restore whatever was there in step 1
//! →  CSI c          5. the barrier
//! ```
//!
//! Step 3 answers with the bits the terminal actually **honoured**, which is the
//! useful number. Terminals process input in order, so the two flag replies
//! arrive in order and the barrier arrives last.
//!
//! The whole burst — including the pop — is written *before* a single reply is
//! read. That is deliberate: no timeout, no error and no early return can leave
//! the terminal with flags pushed, because the pop was already on the wire.
//!
//! # The barrier, and what a multiplexer does to it
//!
//! A terminal that does not know `CSI ? u` says nothing, and silence is not an
//! answer. It is paired with a Primary Device Attributes request (`CSI c`), which
//! every terminal since the VT100 answers, so silence about the flags *plus* an
//! answer about the device attributes is a real no rather than a slow link.
//!
//! Inside tmux or Zellij that reasoning collapses, because **a multiplexer
//! answers `CSI c` from its own emulator**. A barrier reply then says nothing
//! about the terminal being asked about, so no negative verdict is licensed. See
//! [`ProbeOutcome::verdict`].

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
        keys::{
            FLAGS, Flags, event::escape_json, parse_query_reply, pop, push, query,
        },
    },
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// How long to wait for the terminal to answer.
///
/// Three round trips fit inside this comfortably on a local terminal; the budget
/// is sized for a slow ssh hop, and it is a ceiling rather than a delay — the
/// barrier ends the wait as soon as it arrives.
const REPLY_TIMEOUT: Duration = Duration::from_millis(1500);

/// How long to spend consuming the barrier reply once a conclusion is reached.
///
/// Short, because by then the terminal has demonstrably answered something and
/// the barrier is either already buffered or arriving immediately.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

/// What a probe concluded.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// Flags that were already in effect when the probe started, if the terminal
    /// answered at all. `Some(Flags::NONE)` and `None` are entirely different
    /// findings: the first is a terminal saying "none set", the second is silence.
    pub current: Option<Flags>,
    /// Flags still in effect after asking for all five — the ones the terminal
    /// actually honoured.
    pub effective: Option<Flags>,
    /// Whether the terminal answered the Primary Device Attributes barrier.
    pub answered_barrier: bool,
    /// Multiplexer in the way, if any.
    pub multiplexer: Option<Multiplexer>,
    /// Whether stdin and stdout are both terminals. Without that there is nothing
    /// to probe.
    pub interactive: bool,
    /// Everything the terminal said, in order, for `--format debug`.
    pub replies: Vec<Reply>,
}

/// What a probe is entitled to claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The terminal answered the flags query.
    Supported,
    /// The terminal answered the barrier but not the query, having demonstrably
    /// had its chance.
    Unsupported,
    /// Nothing can honestly be claimed either way.
    Inconclusive,
}

impl ProbeOutcome {
    /// Whether anything answered `CSI ? u`.
    #[must_use]
    pub fn supported(&self) -> bool {
        self.current.is_some()
    }

    /// The flags the terminal honoured when asked for all five.
    ///
    /// Falls back to the baseline reply when the second query went unanswered,
    /// and to no flags at all when neither did — a caller that needs to know
    /// which of those happened should look at [`ProbeOutcome::supported`] first.
    #[must_use]
    pub fn honoured(&self) -> Flags {
        self.effective.or(self.current).unwrap_or(Flags::NONE)
    }

    /// What this probe may legitimately conclude.
    ///
    /// A negative needs the barrier, and the barrier only licenses one when it was
    /// answered by the terminal being asked about. **A multiplexer answers Primary
    /// Device Attributes itself** — tmux replies `CSI ?1;2c` from its own emulator
    /// — so inside one, a barrier reply proves nothing about the terminal beyond
    /// it, and reporting "unsupported" would be a confident false claim.
    ///
    /// A *positive* is still a conclusion inside a multiplexer, but it means
    /// something slightly different there: whoever answered is the thing this
    /// process's keys come from, so its answer is the operative one — while the
    /// flags it honours may still exceed what it can deliver from the terminal
    /// beyond it. `--format debug` says so.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        if self.supported() {
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

/// The whole burst the probe writes, in one string.
///
/// Built as one value so that a report can show exactly what was sent, and so
/// that the pop can never be separated from the push by a control-flow path.
#[must_use]
pub fn burst() -> String {
    format!(
        "{}{}{}{}{CSI}c",
        query(),
        push(Flags::ALL),
        query(),
        pop()
    )
}

/// Runs the probe using an existing terminal and reader.
///
/// Takes both rather than creating them because there must only ever be one
/// [`InputReader`] per process — two would race for the same bytes on stdin, and
/// each would see a random half of the terminal's answer.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal that
/// simply stays quiet is not an error; it is a negative result.
pub fn probe(term: &mut Term, input: &mut InputReader) -> Result<ProbeOutcome> {
    let mut outcome = ProbeOutcome {
        multiplexer: Multiplexer::detect(),
        interactive: stdout().is_terminal() && stdin().is_terminal(),
        ..ProbeOutcome::default()
    };
    if !outcome.interactive {
        return Ok(outcome);
    }

    // Raw mode is required: in cooked mode the replies would sit in the line
    // buffer until the user pressed Enter, and would be echoed back at them.
    term.enable_raw()?;
    input.drain();

    // Deliberately not wrapped for a multiplexer. `CSI` sequences are not
    // filtered the way `APC` payloads are, and the question worth answering is
    // what *this process* gets when it presses a key — which, inside tmux, is
    // whatever tmux does, not whatever the outer terminal does.
    term.write_now(&burst())?;

    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // An EOF on stdin means there is nobody to answer; treat it as silence.
        let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
            break;
        };
        outcome.replies.push(reply.clone());

        if let Some(flags) = parse_query_reply(&reply) {
            // The two flag replies arrive in the order they were asked for.
            if outcome.current.is_none() {
                outcome.current = Some(flags);
            } else if outcome.effective.is_none() {
                outcome.effective = Some(flags);
            }
        } else if matches!(reply, Reply::Csi { final_byte: 'c', .. }) {
            // The barrier came back. Everything sent before it has already been
            // processed, so waiting longer would learn nothing.
            outcome.answered_barrier = true;
            break;
        }
    }

    // A supporting terminal answers the flag queries first, so the loop can end
    // with the barrier reply still in flight. Unread, it would be delivered to
    // whatever runs next — the user's shell, which prints it as stray text.
    // Consuming it here is what keeps probing invisible.
    if !outcome.answered_barrier
        && let Ok(Some(reply)) = input.wait_for_reply(DRAIN_TIMEOUT, |reply| {
            matches!(reply, Reply::Csi { final_byte: 'c', .. })
        })
    {
        outcome.replies.push(reply);
        outcome.answered_barrier = true;
    }
    // Anything else the terminal volunteered — a second reply to a query it
    // answered twice, a focus event — goes no further than here.
    input.drain();

    Ok(outcome)
}

/// Runs a probe from scratch, for `escapade keys probe`.
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

/// Runs `escapade keys probe` and reports through the exit status.
///
/// Silent unless a `--format` is given, so it composes as a shell predicate:
/// `if escapade keys probe; then …`.
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
        Some(ProbeFormat::Debug) => debug_report(theme, &outcome, support),
    }
    Ok(support.exit_code())
}

/// One JSON object, one line — the house machine-readable shape.
#[must_use]
pub fn json_line(outcome: &ProbeOutcome, support: Support) -> String {
    let honoured = outcome.honoured();
    let names: Vec<String> = honoured
        .names()
        .iter()
        .map(|name| format!("\"{}\"", escape_json(name)))
        .collect();
    let multiplexer = outcome
        .multiplexer
        .map_or_else(|| "null".to_owned(), |m| format!("\"{}\"", m.name()));
    let number = |flags: Option<Flags>| {
        flags.map_or_else(|| "null".to_owned(), |flags| flags.bits().to_string())
    };
    format!(
        "{{\"feature\":\"keyboard-protocol\",\"verdict\":\"{}\",\"flags\":{},\
         \"flag_names\":[{}],\"flags_before\":{},\"answered_barrier\":{},\
         \"multiplexer\":{multiplexer},\"interactive\":{}}}\n",
        support.as_str(),
        number(outcome.effective.or(outcome.current)),
        names.join(","),
        number(outcome.current),
        outcome.answered_barrier,
        outcome.interactive,
    )
}

/// The human-readable findings, for `--format debug`.
#[expect(
    clippy::too_many_lines,
    reason = "one screen of output, written top to bottom; splitting it would \
              scatter the layout across functions without simplifying anything"
)]
fn debug_report(theme: &Theme, outcome: &ProbeOutcome, support: Support) {
    println!();
    println!("  {}", theme.paint("Keyboard protocol support", Ink::Title));
    println!();

    let term_name = std::env::var("TERM").unwrap_or_else(|_| "unset".to_owned());
    print!("  {} {}", theme.paint("terminal:", Ink::Muted), term_name);
    if let Ok(program) = std::env::var("TERM_PROGRAM") {
        print!(
            "{}",
            theme.paint(&format!("  (TERM_PROGRAM={program})"), Ink::Muted)
        );
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
            theme.paint("?", Ink::Warn)
        );
        note(
            theme,
            "Capability detection needs a real terminal to answer. Run this directly, not through \
             a pipe.",
        );
        println!();
        return;
    }

    match support {
        Support::Yes => {
            println!(
                "  {} {}",
                theme.paint("✔", Ink::Good),
                theme.paint("supported", Ink::Good)
            );
        }
        Support::No => {
            println!(
                "  {} {}",
                theme.paint("✘", Ink::Bad),
                theme.paint("not supported", Ink::Bad)
            );
            note(
                theme,
                "The terminal answered the device-attributes barrier but ignored `CSI ? u`, so \
                 this is a real no rather than a timeout.",
            );
        }
        Support::Unknown => {
            println!(
                "  {} {}",
                theme.paint("?", Ink::Warn),
                theme.paint("inconclusive", Ink::Warn)
            );
            let explanation = match (outcome.multiplexer, outcome.answered_barrier) {
                (Some(multiplexer), true) => format!(
                    "The device-attributes barrier came back, but {} answers that itself rather \
                     than forwarding it — so it says nothing about whether the protocol is \
                     spoken. Inside a multiplexer a negative cannot be established.",
                    multiplexer.name()
                ),
                (Some(multiplexer), false) => format!(
                    "Nothing came back at all, and {} is in the way.",
                    multiplexer.name()
                ),
                (None, _) => "Nothing came back at all — not even the device-attributes barrier \
                              every terminal answers. Something between here and the terminal is \
                              swallowing replies."
                    .to_owned(),
            };
            note(theme, &explanation);
        }
    }

    if outcome.supported() {
        println!();
        println!("  {}", theme.paint("flags this terminal honoured", Ink::Accent));
        let honoured = outcome.honoured();
        for spec in FLAGS {
            let held = honoured.bits() & spec.bit != 0;
            let (symbol, ink) = if held {
                ("✔", Ink::Good)
            } else {
                ("✘", Ink::Bad)
            };
            println!(
                "    {} {} {}  {}",
                theme.paint(symbol, ink),
                theme.paint(&format!("{:>2}", spec.bit), Ink::Code),
                theme.paint(&format!("{:<38}", spec.name), Ink::Strong),
                theme.paint(spec.summary, Ink::Muted)
            );
        }
        println!();
        println!(
            "    {} {}",
            theme.paint("bitmask:", Ink::Muted),
            theme.paint(&honoured.to_string(), Ink::Code)
        );
        if honoured.unknown_bits() != 0 {
            note(
                theme,
                "The terminal reported a bit this build does not know about, which means it \
                 implements a flag newer than this binary. It is shown as-is rather than masked \
                 away.",
            );
        }
        if honoured != Flags::ALL {
            note(
                theme,
                "Flags that came back unset were asked for and not granted, so an application \
                 must not assume them. Pushing a flag is a request, never a guarantee — always \
                 query afterwards if the behaviour matters.",
            );
        }
        if let Some(before) = outcome.current
            && !before.is_empty()
        {
            println!();
            println!(
                "    {} {}",
                theme.paint("already enabled before this probe:", Ink::Warn),
                theme.paint(&before.to_string(), Ink::Code)
            );
            note(
                theme,
                "Something outside this process — a shell integration, an editor, a multiplexer — \
                 had already pushed flags. That is exactly why an application should push and pop \
                 rather than set: this state was restored, not overwritten.",
            );
        }
        if let Some(multiplexer) = outcome.multiplexer {
            note(
                theme,
                &format!(
                    "This answer came from inside {}, so it is {}'s claim about the keyboard it \
                     gives this process, not the outer terminal's. The two can differ: a \
                     multiplexer can accept a flag and still be unable to deliver, for instance \
                     key releases it never receives.",
                    multiplexer.name(),
                    multiplexer.name()
                ),
            );
        }
    }

    println!();
    println!("  {}", theme.paint("what was sent", Ink::Accent));
    for (sequence, purpose) in [
        (query(), "which flags are set right now?"),
        (push(Flags::ALL), "ask for all five"),
        (query(), "and now?  the difference is what was honoured"),
        (pop(), "restore — sent up front, so no path can skip it"),
        (format!("{CSI}c"), "the barrier"),
    ] {
        println!(
            "    {}  {}",
            theme.paint(&format!("{:<12}", pretty(&sequence)), Ink::Code),
            theme.paint(purpose, Ink::Muted)
        );
    }

    println!();
    println!("  {}", theme.paint("what came back", Ink::Accent));
    if outcome.replies.is_empty() {
        println!("    {}", theme.paint("(nothing)", Ink::Muted));
    }
    for reply in &outcome.replies {
        println!(
            "    {}",
            theme.paint(&super::event::describe_reply(reply), Ink::Code)
        );
    }

    println!();
    println!(
        "  {} {}",
        theme.paint("escapade keys watch", Ink::Code),
        theme.paint("to see what your keys actually send", Ink::Muted)
    );
    println!(
        "  {} {}",
        theme.paint("escapade keys explain", Ink::Code),
        theme.paint("for the protocol and who implements it", Ink::Muted)
    );
    println!();
}

/// Prints a wrapped, muted explanatory paragraph under a finding.
fn note(theme: &Theme, text: &str) {
    for line in crate::features::wrap(text, theme.width().saturating_sub(4)) {
        println!("    {}", theme.paint(&line, Ink::Muted));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// An outcome that got as far as being interactive, and nothing else.
    fn started() -> ProbeOutcome {
        ProbeOutcome {
            interactive: true,
            ..ProbeOutcome::default()
        }
    }

    #[test]
    fn the_burst_asks_pushes_asks_pops_and_fences_in_that_order() {
        let burst = burst();
        let positions: Vec<usize> = ["[?u", "[>31u", "[<u", "[c"]
            .iter()
            .map(|needle| burst.find(needle).expect("missing part of the burst"))
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "order matters: {burst:?}"
        );
        // Two queries, one on each side of the push.
        assert_eq!(burst.matches("[?u").count(), 2);
        // The pop precedes the barrier, so it is already on the wire before a
        // single reply is read and no early return can skip it.
        assert!(burst.find("[<u") < burst.find("[c"));
        assert_eq!(pretty(&burst), "CSI ?u CSI >31u CSI ?u CSI <u CSI c");
    }

    #[test]
    fn a_terminal_answering_none_still_counts_as_support() {
        // The trap this probe exists to avoid: `CSI ? 0 u` is a terminal that
        // speaks the protocol and has nothing enabled yet, not a terminal that
        // cannot do it.
        let outcome = ProbeOutcome {
            current: Some(Flags::NONE),
            effective: Some(Flags::ALL),
            ..started()
        };
        assert!(outcome.supported());
        assert_eq!(outcome.verdict(), Verdict::Supported);
        assert_eq!(support(&outcome), Support::Yes);
        assert_eq!(outcome.honoured(), Flags::ALL);
    }

    #[test]
    fn honoured_flags_come_from_the_second_query() {
        // A terminal that accepts only disambiguation reports only that back.
        let outcome = ProbeOutcome {
            current: Some(Flags::NONE),
            effective: Some(Flags::DISAMBIGUATE),
            ..started()
        };
        assert_eq!(outcome.honoured(), Flags::DISAMBIGUATE);
        assert!(!outcome.honoured().contains(Flags::EVENT_TYPES));
    }

    #[test]
    fn a_missing_second_reply_falls_back_to_the_baseline() {
        let outcome = ProbeOutcome {
            current: Some(Flags::EVENT_TYPES),
            effective: None,
            ..started()
        };
        assert_eq!(outcome.honoured(), Flags::EVENT_TYPES);
        assert!(outcome.supported());
    }

    #[test]
    fn silence_plus_a_barrier_is_a_definite_no() {
        let outcome = ProbeOutcome {
            answered_barrier: true,
            ..started()
        };
        assert_eq!(outcome.verdict(), Verdict::Unsupported);
        assert_eq!(support(&outcome), Support::No);
        assert!(outcome.conclusive());
        assert_eq!(outcome.honoured(), Flags::NONE);
    }

    #[test]
    fn total_silence_is_inconclusive_not_negative() {
        let outcome = started();
        assert_eq!(outcome.verdict(), Verdict::Inconclusive);
        assert_eq!(support(&outcome), Support::Unknown);
        assert!(!outcome.conclusive());
    }

    #[test]
    fn a_barrier_answered_inside_a_multiplexer_proves_nothing() {
        // tmux answers Primary Device Attributes from its own emulator, so the
        // barrier says nothing about the protocol. Claiming "not supported" here
        // would be a confident false statement.
        let outcome = ProbeOutcome {
            answered_barrier: true,
            multiplexer: Some(Multiplexer::Tmux),
            ..started()
        };
        assert_eq!(outcome.verdict(), Verdict::Inconclusive);
        assert_eq!(support(&outcome), Support::Unknown);
    }

    #[test]
    fn support_is_still_conclusive_inside_a_multiplexer() {
        // Something answered in the protocol's own language. Whoever that was is
        // the thing this process's keys come from.
        let outcome = ProbeOutcome {
            current: Some(Flags::NONE),
            effective: Some(Flags::DISAMBIGUATE),
            multiplexer: Some(Multiplexer::Zellij),
            ..started()
        };
        assert_eq!(outcome.verdict(), Verdict::Supported);
    }

    #[test]
    fn a_non_interactive_run_concludes_nothing() {
        let outcome = ProbeOutcome::default();
        assert!(!outcome.interactive);
        assert_eq!(support(&outcome), Support::Unknown);
        // Inconclusive exits 2, never 1: a probe that could not ask has not
        // discovered that the answer is no.
        assert_eq!(
            format!("{:?}", Support::Unknown.exit_code()),
            format!("{:?}", ExitCode::from(2))
        );
    }

    #[test]
    fn json_reports_the_flags_and_never_folds_unknown_into_no() {
        let outcome = ProbeOutcome {
            current: Some(Flags::DISAMBIGUATE),
            effective: Some(Flags::from_bits(3)),
            answered_barrier: true,
            ..started()
        };
        let line = json_line(&outcome, support(&outcome));
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"feature\":\"keyboard-protocol\""));
        assert!(line.contains("\"verdict\":\"yes\""));
        assert!(line.contains("\"flags\":3"));
        assert!(line.contains("\"flags_before\":1"));
        assert!(line.contains("\"disambiguate escape codes\""));
        assert!(line.contains("\"report event types\""));
        assert!(line.contains("\"multiplexer\":null"));

        // Silence must be `null`, distinguishable from a terminal answering 0.
        let quiet = json_line(&started(), support(&started()));
        assert!(quiet.contains("\"verdict\":\"unknown\""), "{quiet}");
        assert!(quiet.contains("\"flags\":null"), "{quiet}");
        assert!(quiet.contains("\"flags_before\":null"), "{quiet}");

        let zero = ProbeOutcome {
            current: Some(Flags::NONE),
            effective: Some(Flags::NONE),
            ..started()
        };
        let zero = json_line(&zero, Support::Yes);
        assert!(zero.contains("\"flags\":0"), "{zero}");
        assert!(zero.contains("\"flag_names\":[]"), "{zero}");
    }

    #[test]
    fn json_is_free_of_control_bytes() {
        let outcome = ProbeOutcome {
            current: Some(Flags::ALL),
            effective: Some(Flags::ALL),
            multiplexer: Some(Multiplexer::Tmux),
            ..started()
        };
        let line = json_line(&outcome, Support::Yes);
        assert!(!line[..line.len() - 1].chars().any(char::is_control), "{line}");
    }

    #[test]
    fn a_report_never_panics_whatever_came_back() {
        // Exercises the whole layout, including the paths a terminal in a strange
        // state reaches: unknown future bits, flags already pushed by somebody
        // else, and a multiplexer in the way.
        let theme = Theme::plain();
        let outcomes = [
            ProbeOutcome::default(),
            started(),
            ProbeOutcome {
                answered_barrier: true,
                ..started()
            },
            ProbeOutcome {
                current: Some(Flags::from_bits(0b11_1111)),
                effective: Some(Flags::from_bits(0b11_1111)),
                answered_barrier: true,
                replies: vec![Reply::Csi {
                    params: "?63".to_owned(),
                    final_byte: 'u',
                }],
                ..started()
            },
            ProbeOutcome {
                current: Some(Flags::EVENT_TYPES),
                effective: Some(Flags::ALL),
                multiplexer: Some(Multiplexer::Tmux),
                answered_barrier: true,
                ..started()
            },
        ];
        for outcome in outcomes {
            let support = support(&outcome);
            debug_report(&theme, &outcome, support);
            let _ = json_line(&outcome, support);
        }
    }
}
