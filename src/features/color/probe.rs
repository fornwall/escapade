//! Measurement, for once — and the difference is worth stating.
//!
//! Most probes in this project are inferences. `OSC 9 ; 4` has no query, mode
//! 2027 has one that terminals lie about, and unscroll has none at all, so those
//! probes reason from what a terminal calls itself and what a support matrix says
//! about that name. This one does not have to. **A colour query has a reply**, and
//! a reply is the terminal's own answer about its own behaviour, arriving in band,
//! after the query, in the same session. There is nothing to infer.
//!
//! So the verdict here rests on the round trip and on nothing else. The support
//! matrix is not consulted, the environment is not consulted, and `TERM` is not
//! consulted — if this terminal answers `OSC 11` it supports `OSC 11`, whatever a
//! table written last year says.
//!
//! # Two capabilities, one exit status
//!
//! There are two separable things here, and the probe measures both:
//!
//! * **Asking** — `OSC 4/10/11/12/17/19`, which is what the feature is for.
//! * **Being told** — DEC mode 2031, which is what makes an answer stay correct.
//!
//! The exit status reports the first, because that is what a script gating on
//! `escapade color probe` is about to do. The second is reported in `--format
//! json` and `--format debug` under its own key, so a program that specifically
//! wants live updates can ask for exactly that and get a separate answer rather
//! than a blended one.
//!
//! # What licenses a "no"
//!
//! Only the barrier. A colour that did not come back means nothing on its own —
//! the terminal may be slow, the link may be slow — so silence is a refusal only
//! once `CSI c` has come back to prove the terminal reached the end of the queue
//! and chose not to answer. Inside a multiplexer even that proves nothing, because
//! the multiplexer answers the barrier itself; see
//! [`super::query::Answers::silence_is_conclusive`].

use std::process::ExitCode;

use anyhow::Result;

use crate::{
    cli::ProbeFormat,
    db::Catalog,
    decrqm::ModeState,
    features::{
        Support,
        color::{
            FEATURE_ID,
            notify::{self, Scheme},
            query::{self, Answer, Answers, Target},
            report::escape_json,
        },
        wrap,
    },
    input::InputReader,
    term::{Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// The colours the probe asks about.
///
/// The three dynamic colours, and no palette entries. The palette is 256 more
/// round trips for a question the first three have already answered, and a
/// terminal that answers `OSC 11` and refuses `OSC 4` is rare enough to belong in
/// the report rather than in the exit status.
const PROBED: [Target; 3] = Target::DEFAULT;

/// Everything one probe established.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// What the colour queries produced.
    pub answers: Answers,
    /// What the change-notification query produced.
    pub notify: notify::State,
}

impl Outcome {
    /// The verdict on colour queries, which is what the exit status reports.
    ///
    /// [`Support::Yes`] the moment any colour comes back: the terminal has
    /// demonstrated the capability by using it. [`Support::No`] when nothing came
    /// back and silence is conclusive. [`Support::Unknown`] otherwise — no
    /// terminal to ask, no barrier, or a multiplexer standing in for one.
    #[must_use]
    pub fn support(&self) -> Support {
        if !self.answers.interactive {
            return Support::Unknown;
        }
        if self.answers.any_answered() {
            return Support::Yes;
        }
        if self.answers.silence_is_conclusive() {
            Support::No
        } else {
            Support::Unknown
        }
    }

    /// The colours that came back.
    #[must_use]
    pub fn answered(&self) -> Vec<&Answer> {
        self.answers
            .answers
            .iter()
            .filter(|answer| answer.answered())
            .collect()
    }

    /// Why the verdict is what it is, in one sentence.
    #[must_use]
    pub fn reason(&self) -> String {
        if !self.answers.interactive {
            return "stdin and stdout are not both terminals, so the query had nowhere to go and \
                    the reply had nowhere to come back from"
                .to_owned();
        }
        let answered = self.answered();
        if !answered.is_empty() {
            let names: Vec<String> = answered
                .iter()
                .map(|answer| answer.target.label())
                .collect();
            return format!(
                "the terminal answered {} of {} colour queries ({}), which is the capability \
                 itself rather than a claim about it",
                answered.len(),
                self.answers.answers.len(),
                names.join(", ")
            );
        }
        if let Some(multiplexer) = self.answers.multiplexer {
            return format!(
                "no colour came back, and {} answers the device-attributes barrier itself, so \
                 nothing here distinguishes a terminal that refused from one that was never \
                 reached",
                multiplexer.name()
            );
        }
        if self.answers.answered_barrier {
            return "the device-attributes barrier came back and no colour did, so this terminal \
                    had its chance and does not answer colour queries"
                .to_owned();
        }
        "neither a colour nor the device-attributes barrier came back, so something between here \
         and the terminal is swallowing replies — nothing was established"
            .to_owned()
    }

    /// Why the notification verdict is what it is.
    #[must_use]
    pub fn notify_reason(&self) -> String {
        if !self.answers.interactive {
            return "not asked: there is no terminal on both ends".to_owned();
        }
        match self.notify.mode {
            Some(state) => format!(
                "DECRQM answered {} ({}) for mode {}",
                state.value(),
                state.label(),
                notify::MODE
            ),
            None if self.notify.answered_barrier => format!(
                "the barrier came back and no DECRQM answer did, so this terminal does not \
                 report mode {}",
                notify::MODE
            ),
            None => "nothing answered the mode query, barrier included".to_owned(),
        }
    }
}

/// Runs one probe.
///
/// Both round trips share a single raw-mode session, and the subscription mode 2031
/// is taken back down before returning — a probe that leaves the terminal
/// reporting colour changes to whatever runs next has broken the thing it was
/// asked to inspect.
///
/// # Errors
///
/// Fails if the terminal cannot be set up or written to.
pub fn detect() -> Result<Outcome> {
    if !query::interactive() {
        return Ok(Outcome {
            answers: Answers {
                answers: PROBED
                    .iter()
                    .map(|&target| Answer {
                        target,
                        reported: None,
                    })
                    .collect(),
                multiplexer: Multiplexer::detect(),
                interactive: false,
                ..Answers::default()
            },
            notify: notify::State {
                multiplexer: Multiplexer::detect(),
                ..notify::State::default()
            },
        });
    }

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    term.enable_raw()?;

    let answers = query::ask(&mut term, &mut input, &PROBED);
    let notified = answers
        .as_ref()
        .ok()
        .map(|_| notify::probe(&mut term, &mut input));

    // Whatever happened above, the mode goes back off before the terminal is
    // handed back.
    let state = match notified {
        Some(Ok(state)) => {
            let _ = notify::stop(&mut term, state.enabled);
            state
        }
        _ => notify::State::default(),
    };
    term.restore();

    Ok(Outcome {
        answers: answers?,
        notify: state,
    })
}

/// Runs `escapade color probe`.
///
/// Silent by default: the answer is the exit status, so this composes as a shell
/// predicate. `catalog` is accepted for symmetry with every other probe and
/// deliberately unused — see the module documentation for why a measurement does
/// not consult a table.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn report(theme: &Theme, catalog: &Catalog, format: Option<ProbeFormat>) -> Result<ExitCode> {
    let _ = catalog;
    let outcome = detect()?;

    match format {
        None => {}
        Some(ProbeFormat::Json) => print!("{}", json_line(&outcome)),
        Some(ProbeFormat::Debug) => print_debug(theme, &outcome),
    }

    Ok(outcome.support().exit_code())
}

// ---------------------------------------------------------------------------
// `--format json`
// ---------------------------------------------------------------------------

/// One JSON object, one line — the house machine-readable shape.
///
/// `measurable` is `true` here, and it is the only probe in this project that can
/// say so. A script that treats `"verdict":"unknown"` as "probably fine" is right
/// to do that for a write-only feature and wrong to do it here: for this one,
/// unknown means the round trip did not complete.
///
/// `notification` is a nested verdict rather than a flattened field, so that
/// adding to it later cannot change the meaning of the top-level one.
#[must_use]
pub fn json_line(outcome: &Outcome) -> String {
    let colours: Vec<String> = outcome
        .answers
        .answers
        .iter()
        .map(|answer| {
            let reported = answer.reported.as_ref();
            format!(
                "{{\"target\":\"{}\",\"answered\":{},\"raw\":{},\"hex\":{}}}",
                answer.target.slug(),
                answer.answered(),
                reported.map_or_else(
                    || "null".to_owned(),
                    |value| format!("\"{}\"", escape_json(&value.raw))
                ),
                reported.map_or_else(
                    || "null".to_owned(),
                    |value| format!("\"{}\"", value.rgb.hex())
                ),
            )
        })
        .collect();
    format!(
        "{{\"feature\":\"{FEATURE_ID}\",\"verdict\":\"{}\",\"measurable\":true,\
         \"reason\":\"{}\",\"answered_barrier\":{},\"multiplexer\":{},\"interactive\":{},\
         \"colors\":[{}],\"notification\":{{\"verdict\":\"{}\",\"mode\":{},\"decrqm\":{},\
         \"scheme\":{},\"reason\":\"{}\"}}}}\n",
        outcome.support().as_str(),
        escape_json(&outcome.reason()),
        outcome.answers.answered_barrier,
        outcome
            .answers
            .multiplexer
            .map_or_else(|| "null".to_owned(), |m| format!("\"{}\"", m.name())),
        outcome.answers.interactive,
        colours.join(","),
        outcome.notify.support().as_str(),
        notify::MODE,
        outcome
            .notify
            .mode
            .map_or_else(|| "null".to_owned(), |state| state.value().to_string()),
        outcome
            .notify
            .scheme
            .map_or_else(|| "null".to_owned(), |s| format!("\"{}\"", s.as_str())),
        escape_json(&outcome.notify_reason()),
    )
}

// ---------------------------------------------------------------------------
// `--format debug`
// ---------------------------------------------------------------------------

/// Prints everything the probe saw and concluded, for a person.
#[expect(
    clippy::too_many_lines,
    reason = "one screen of output, written top to bottom; splitting it would \
              scatter the layout across functions without simplifying anything"
)]
fn print_debug(theme: &Theme, outcome: &Outcome) {
    let width = theme.width().saturating_sub(6);

    println!();
    println!(
        "  {}",
        theme.paint(
            "Colour query support (OSC 4/10/11/12, DEC mode 2031)",
            Ink::Title
        )
    );
    println!();
    for line in wrap(
        "This one is measured rather than inferred. A colour query has a reply, so the terminal \
         answers for itself and no support table is consulted.",
        width,
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();

    verdict_line(theme, "asking", outcome.support(), &outcome.reason());
    println!();
    verdict_line(
        theme,
        "being told",
        outcome.notify.support(),
        &outcome.notify_reason(),
    );
    println!();

    println!("  {}", theme.paint("what went out", Ink::Accent));
    for target in PROBED {
        println!(
            "    {}  {}",
            theme.paint(&format!("{:<22}", pretty(&target.query())), Ink::Code),
            theme.paint(&target.label(), Ink::Muted)
        );
    }
    for (sequence, meaning) in [
        (
            crate::decrqm::query(notify::MODE),
            "DECRQM — do you report scheme changes?",
        ),
        (notify::request(), "which scheme are you in right now?"),
        (
            query::barrier(),
            "the barrier, which every terminal answers",
        ),
    ] {
        println!(
            "    {}  {}",
            theme.paint(&format!("{:<22}", pretty(&sequence)), Ink::Code),
            theme.paint(meaning, Ink::Muted)
        );
    }
    println!();

    println!("  {}", theme.paint("what came back", Ink::Accent));
    for answer in &outcome.answers.answers {
        let (text, ink) = match &answer.reported {
            Some(reported) => (
                format!("{}   {}", reported.rgb.hex(), reported.raw),
                Ink::Code,
            ),
            None => ("no answer".to_owned(), Ink::Muted),
        };
        println!(
            "    {}  {}",
            theme.paint(&format!("{:<22}", answer.target.label()), Ink::Strong),
            theme.paint(&text, ink)
        );
    }
    let barrier = if outcome.answers.answered_barrier {
        ("answered", Ink::Good)
    } else {
        ("no answer", Ink::Warn)
    };
    println!(
        "    {}  {}",
        theme.paint(&format!("{:<22}", "device attributes"), Ink::Strong),
        theme.paint(barrier.0, barrier.1)
    );
    println!(
        "    {}  {}",
        theme.paint(&format!("{:<22}", "mode 2031"), Ink::Strong),
        theme.paint(
            &outcome
                .notify
                .mode
                .map_or_else(|| "no answer".to_owned(), decrqm_label),
            Ink::Code
        )
    );
    println!(
        "    {}  {}",
        theme.paint(&format!("{:<22}", "reported scheme"), Ink::Strong),
        theme.paint(
            outcome.notify.scheme.map_or("not reported", Scheme::as_str),
            Ink::Code
        )
    );

    if !outcome.answers.stray.is_empty() {
        println!();
        println!("  {}", theme.paint("replies nobody asked for", Ink::Accent));
        for reply in &outcome.answers.stray {
            println!("    {}", theme.paint(&format!("{reply:?}"), Ink::Muted));
        }
    }

    if let Some(warning) = outcome.notify.sentinel.warning() {
        println!();
        for line in wrap(&warning, width) {
            println!("  {}", theme.paint(&line, Ink::Warn));
        }
    }

    if let Some(multiplexer) = outcome.answers.multiplexer {
        println!();
        for line in wrap(
            &format!(
                "{} is between this process and the terminal. It answers the barrier itself and \
                 keeps its own idea of the palette, so a colour that came back may be the \
                 multiplexer's rather than the terminal's, and a colour that did not licenses no \
                 negative verdict.",
                multiplexer.name()
            ),
            width,
        ) {
            println!("  {}", theme.paint(&line, Ink::Warn));
        }
    }
    println!();
}

/// One verdict, with its symbol, exit status and reason.
fn verdict_line(theme: &Theme, what: &str, support: Support, reason: &str) {
    let (symbol, ink) = match support {
        Support::Yes => ("✔", Ink::Good),
        Support::No => ("✘", Ink::Bad),
        Support::Unknown => ("?", Ink::Warn),
    };
    println!(
        "  {} {}  {}",
        theme.paint(symbol, ink),
        theme.paint(&format!("{what}: {}", support.as_str()), ink),
        theme.paint(
            &format!("({})", exit_status_note(what, support)),
            Ink::Muted
        )
    );
    for line in wrap(reason, theme.width().saturating_sub(6)) {
        println!("      {}", theme.paint(&line, Ink::Muted));
    }
}

/// What the exit status will be, said only where it is actually the exit status.
fn exit_status_note(what: &str, support: Support) -> String {
    if what == "asking" {
        format!(
            "exit status {}",
            match support {
                Support::Yes => 0,
                Support::No => 1,
                Support::Unknown => 2,
            }
        )
    } else {
        "reported separately, not in the exit status".to_owned()
    }
}

/// A DECRPM state as `value (label)`.
fn decrqm_label(state: ModeState) -> String {
    format!("{} ({})", state.value(), state.label())
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{decrqm::Sentinel, features::color::value::parse_reply};

    /// An outcome with the given answers, as a real round trip would build one.
    fn outcome(answers: Vec<Answer>, answered_barrier: bool, interactive: bool) -> Outcome {
        Outcome {
            answers: Answers {
                answers,
                answered_barrier,
                interactive,
                ..Answers::default()
            },
            notify: notify::State::default(),
        }
    }

    /// One colour that came back.
    fn answered(target: Target, payload: &str) -> Answer {
        Answer {
            target,
            reported: Some(parse_reply(payload).unwrap()),
        }
    }

    /// One colour that did not.
    fn silent(target: Target) -> Answer {
        Answer {
            target,
            reported: None,
        }
    }

    #[test]
    fn an_answered_colour_is_the_capability_itself() {
        let probe = outcome(
            vec![
                answered(Target::Background, "11;rgb:1e1e/1e1e/2e2e"),
                silent(Target::Cursor),
            ],
            true,
            true,
        );
        assert_eq!(probe.support(), Support::Yes);
        assert!(probe.reason().contains("background"), "{}", probe.reason());
    }

    #[test]
    fn one_answer_out_of_three_is_still_a_yes() {
        // The feature is "can this terminal be asked". A terminal that answers
        // the foreground and ignores the cursor can be asked.
        let probe = outcome(
            vec![
                answered(Target::Foreground, "10;rgb:f/f/f"),
                silent(Target::Background),
                silent(Target::Cursor),
            ],
            true,
            true,
        );
        assert_eq!(probe.support(), Support::Yes);
    }

    #[test]
    fn silence_under_the_barrier_is_the_only_no() {
        let probe = outcome(
            vec![silent(Target::Foreground), silent(Target::Background)],
            true,
            true,
        );
        assert_eq!(probe.support(), Support::No);
        assert!(
            probe.reason().contains("had its chance"),
            "{}",
            probe.reason()
        );
    }

    #[test]
    fn silence_without_the_barrier_establishes_nothing() {
        // The failure this project exists to avoid: reporting a slow link as an
        // unsupported terminal.
        let probe = outcome(vec![silent(Target::Background)], false, true);
        assert_eq!(probe.support(), Support::Unknown);
        assert!(probe.reason().contains("swallowing"), "{}", probe.reason());
    }

    #[test]
    fn a_multiplexer_voids_a_negative_but_not_a_positive() {
        let mut probe = outcome(vec![silent(Target::Background)], true, true);
        probe.answers.multiplexer = Some(Multiplexer::Tmux);
        assert_eq!(probe.support(), Support::Unknown);
        assert!(probe.reason().contains("tmux"), "{}", probe.reason());

        // An answer that arrived is an answer, whoever it came from.
        probe.answers.answers = vec![answered(Target::Background, "11;rgb:0/0/0")];
        assert_eq!(probe.support(), Support::Yes);
    }

    #[test]
    fn no_terminal_is_unknown_rather_than_no() {
        let probe = outcome(vec![silent(Target::Background)], false, false);
        assert_eq!(probe.support(), Support::Unknown);
        assert!(
            probe.reason().contains("not both terminals"),
            "{}",
            probe.reason()
        );
    }

    #[test]
    fn the_two_capabilities_are_reported_separately() {
        // A terminal can answer colour queries perfectly and know nothing about
        // mode 2031. Blending the two into one verdict would lose that.
        let mut probe = outcome(
            vec![answered(Target::Background, "11;rgb:0/0/0")],
            true,
            true,
        );
        probe.notify = notify::State {
            mode: Some(ModeState::NotRecognized),
            sentinel: Sentinel {
                state: None,
                answered_barrier: true,
            },
            answered_barrier: true,
            ..notify::State::default()
        };
        assert_eq!(probe.support(), Support::Yes);
        assert_eq!(probe.notify.support(), Support::No);
    }

    #[test]
    fn the_exit_status_follows_the_asking_verdict_only() {
        assert_eq!(
            format!("{:?}", Support::Yes.exit_code()),
            format!("{:?}", ExitCode::SUCCESS)
        );
        // The note says outright that the second verdict is not in the status.
        assert!(exit_status_note("asking", Support::Yes).contains("exit status 0"));
        assert!(exit_status_note("being told", Support::No).contains("not in the exit status"));
    }

    // -- the machine-readable shape -----------------------------------------

    #[test]
    fn json_is_one_object_on_one_line_and_ends_with_a_newline() {
        let probe = outcome(
            vec![
                answered(Target::Background, "11;rgb:1e1e/1e1e/2e2e"),
                silent(Target::Cursor),
            ],
            true,
            true,
        );
        let line = json_line(&probe);
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.contains("\"feature\":\"color\""));
        assert!(line.contains("\"verdict\":\"yes\""));
        assert!(line.contains("\"measurable\":true"));
    }

    #[test]
    fn the_json_carries_the_raw_reply_beside_the_normalised_value() {
        // The evidence rule: a reader must be able to check the claim that
        // `rgb:f/0/0` means `#ff0000`, and a bug report needs the original.
        let probe = outcome(
            vec![answered(Target::Background, "11;rgb:f/0/0")],
            true,
            true,
        );
        let line = json_line(&probe);
        assert!(line.contains("\"raw\":\"rgb:f/0/0\""), "{line}");
        assert!(line.contains("\"hex\":\"#ff0000\""), "{line}");
    }

    #[test]
    fn an_unanswered_colour_is_null_rather_than_black() {
        let probe = outcome(vec![silent(Target::Cursor)], true, true);
        let line = json_line(&probe);
        assert!(line.contains("\"answered\":false"), "{line}");
        assert!(line.contains("\"raw\":null"), "{line}");
        assert!(line.contains("\"hex\":null"), "{line}");
        assert!(!line.contains("#000000"), "{line}");
    }

    #[test]
    fn the_notification_verdict_is_nested_under_its_own_key() {
        let probe = outcome(vec![silent(Target::Background)], true, true);
        let line = json_line(&probe);
        assert!(line.contains("\"notification\":{\"verdict\":"), "{line}");
        assert!(line.contains("\"mode\":2031"), "{line}");
        assert!(line.contains("\"scheme\":null"), "{line}");
    }

    #[test]
    fn json_output_never_contains_a_control_byte() {
        // Everything reaching this output has been through a terminal.
        let mut probe = outcome(vec![silent(Target::Background)], true, true);
        probe.notify.scheme = Some(Scheme::Dark);
        let line = json_line(&probe);
        assert!(!line.contains('\u{1b}'), "{line}");
        assert!(line.contains("\"scheme\":\"dark\""), "{line}");
    }

    #[test]
    fn every_reason_is_a_plain_sentence() {
        for probe in [
            outcome(vec![silent(Target::Background)], true, true),
            outcome(vec![silent(Target::Background)], false, true),
            outcome(vec![silent(Target::Background)], false, false),
            outcome(
                vec![answered(Target::Background, "11;rgb:0/0/0")],
                true,
                true,
            ),
        ] {
            assert!(!probe.reason().is_empty());
            assert!(!probe.reason().contains('\u{1b}'));
            assert!(!probe.notify_reason().is_empty());
            assert!(!probe.notify_reason().contains('\u{1b}'));
        }
    }

    #[test]
    fn the_probe_asks_about_the_dynamic_colours_and_not_the_palette() {
        // 256 more round trips for a question the first three have answered.
        assert_eq!(PROBED.len(), 3);
        assert!(PROBED.iter().all(|target| target.index().is_none()));
    }
}
