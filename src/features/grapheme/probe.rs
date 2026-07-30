//! `escapade grapheme probe` — ask the terminal whether it clusters, then check.
//!
//! Three questions, in order, because each one keeps the one before it honest:
//!
//! 1. **Is the terminal's answer worth anything?** `CSI ? 9876543 $ p` — DECRQM
//!    for a mode that cannot exist. See [`Reliability`].
//! 2. **What does it claim?** `CSI ? 2027 $ p` (DECRQM), behind a Primary Device
//!    Attributes barrier so silence can be told apart from slowness.
//! 3. **What does it do?** Print [`REFERENCE_SEQUENCE`] with the mode off and
//!    then on, and count the cells the cursor moved.
//!
//! There are six different answers to question 2 and they are not
//! interchangeable:
//!
//! | outcome | meaning |
//! |---|---|
//! | `1` set | implemented, on now |
//! | `2` reset | implemented, off now — send `CSI ? 2027 h` |
//! | `3` permanently set | always on, nothing to enable |
//! | `4` permanently reset | knows the mode, refuses it |
//! | `0` not recognised | parses DECRQM, never heard of 2027 |
//! | *no answer* | no DECRQM at all, or something ate the reply |
//!
//! Collapsing the last two into "unsupported" is exactly the mistake this tool
//! exists to prevent: one is a terminal telling you something, the other is a
//! terminal you never reached.
//!
//! **And every row above is void unless question 1 says otherwise.** iTerm2
//! answers `4` for modes it has never heard of, fictional ones included, so
//! without the control a `4` here reads as a confident finding when it is not a
//! statement at all. Question 3 is unaffected by any of this, which is why the
//! measurement is the primitive and the mode query is a hint.

use anyhow::Result;

use crate::{
    cli::ProbeFormat,
    features::{
        grapheme::{
            Finding, MODE, ModeState, REFERENCE_SEQUENCE, Reliability, SENTINEL_MODE, barrier,
            cluster, json,
            measure::{self, Comparison, ModeReport, Ruler, Verdict},
            query, sentinel_query,
        },
        wrap,
    },
    input::Reply,
    term::{Multiplexer, pretty},
    theme::{Ink, Theme},
    unicode::CharInfo,
};

/// What a probe concluded.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// What DECRQM said, and whether the barrier came back.
    pub report: ModeReport,
    /// What the reference sequence actually measured, when it could be measured.
    pub comparison: Option<Comparison>,
    /// A multiplexer in the way, if any.
    pub multiplexer: Option<Multiplexer>,
    /// Whether stdin and stdout are both terminals. Without that there is
    /// nothing to probe and nothing to measure.
    pub interactive: bool,
    /// Everything the terminal said, in order, for `--verbose`.
    pub replies: Vec<Reply>,
}

impl ProbeOutcome {
    /// Whether the probe reached a conclusion by either route.
    ///
    /// A measurement counts even when DECRQM stayed silent: a terminal that moves
    /// the cursor two cells for a three-codepoint emoji is clustering, whatever
    /// it declines to say about it.
    #[must_use]
    pub fn conclusive(&self) -> bool {
        self.report.conclusive()
            || self
                .comparison
                .is_some_and(|comparison| comparison.clustering_on.is_some())
    }

    /// The state to report, or `None` when nothing answered **or the control says
    /// the answer means nothing**.
    ///
    /// This is the accessor reports should use. A terminal that answers `4` for a
    /// mode that cannot exist has not told us mode 2027 is permanently reset; it
    /// has told us its DECRQM is decorative.
    #[must_use]
    pub fn state(&self) -> Option<ModeState> {
        self.report.trusted_state()
    }

    /// What the terminal literally said, trustworthy or not.
    ///
    /// Only for showing a reader the discarded answer alongside the reason it was
    /// discarded — never for deciding anything.
    #[must_use]
    pub fn raw_state(&self) -> Option<ModeState> {
        self.report.state
    }

    /// What the impossible-mode control says this run's answers are worth.
    #[must_use]
    pub fn reliability(&self) -> Reliability {
        self.report.reliability()
    }
}

/// Runs the probe: DECRQM behind a barrier, then a measurement.
///
/// # Errors
///
/// Fails if the terminal cannot be set up or written to. A terminal that simply
/// stays quiet is not an error; it is a negative result.
pub fn detect() -> Result<ProbeOutcome> {
    let Some(mut ruler) = Ruler::open()? else {
        return Ok(ProbeOutcome {
            multiplexer: Multiplexer::detect(),
            interactive: false,
            ..ProbeOutcome::default()
        });
    };

    let mut outcome = ProbeOutcome {
        report: ruler.entry_report(),
        multiplexer: ruler.multiplexer(),
        interactive: true,
        ..ProbeOutcome::default()
    };
    outcome.comparison = Some(ruler.compare(REFERENCE_SEQUENCE)?);
    ruler.close();
    Ok(outcome)
}

/// What this probe concluded about clustering, for the exit status.
///
/// The measurement decides whenever there is one — a terminal that moved the
/// cursor two cells for a three-codepoint emoji is clustering, whatever it says.
/// DECRQM is consulted only when nothing could be measured, and only when the
/// impossible-mode control vouched for its answer.
#[must_use]
pub fn finding(outcome: &ProbeOutcome) -> Finding {
    if let Some(comparison) = outcome.comparison {
        match comparison.verdict() {
            // An indecisive sequence proves nothing even though it "matched".
            Verdict::Clustered if comparison.decisive() => return Finding::Supported,
            Verdict::NotClustered | Verdict::Disagrees(_) => return Finding::Unsupported,
            Verdict::Clustered | Verdict::Unknown => {}
        }
    }
    // Nothing measured. Fall back to what the terminal claims, but only if the
    // control said its claims mean anything.
    match outcome.state() {
        Some(state) if state.enabled() || state.settable() => Finding::Supported,
        Some(_) => Finding::Unsupported,
        None => Finding::Undetermined,
    }
}

/// Runs `escapade grapheme probe`, reporting only if asked to.
///
/// Silent by default: the exit status is the answer. `--format debug` prints the
/// full account, `--format json` prints one object for a script to read.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn report(theme: &Theme, format: Option<ProbeFormat>) -> Result<Finding> {
    let outcome = detect()?;
    let finding = finding(&outcome);

    match format {
        None => {}
        Some(ProbeFormat::Json) => println!("{}", json_line(&outcome, finding).as_str()),
        Some(ProbeFormat::Debug) => debug_report(theme, &outcome),
    }
    Ok(finding)
}

/// The `--format json` line: one object describing the whole probe.
#[must_use]
pub fn json_line(outcome: &ProbeOutcome, finding: Finding) -> json::Value {
    let comparison = outcome.comparison;
    json::object(&[
        ("feature", json::string(super::FEATURE_ID)),
        ("mode", json::number(MODE as usize)),
        ("finding", json::string(finding.json_value())),
        ("interactive", json::boolean(outcome.interactive)),
        ("sequence", json::string(REFERENCE_SEQUENCE)),
        (
            "decrqm",
            json::object(&[
                (
                    "state",
                    json::optional_string(outcome.state().map(ModeState::label)),
                ),
                (
                    "reported",
                    json::optional_string(outcome.raw_state().map(ModeState::label)),
                ),
                (
                    "control",
                    json::optional_string(outcome.report.sentinel.map(ModeState::label)),
                ),
                ("control_mode", json::number(SENTINEL_MODE as usize)),
                (
                    "reliability",
                    json::string(outcome.reliability().json_value()),
                ),
                (
                    "answered_barrier",
                    json::boolean(outcome.report.answered_barrier),
                ),
            ]),
        ),
        (
            "cells",
            json::object(&[
                (
                    "predicted",
                    json::optional_number(comparison.map(|c| c.predicted)),
                ),
                (
                    "wcwidth",
                    json::optional_number(comparison.map(|c| c.legacy)),
                ),
                (
                    "clustering_off",
                    json::optional_number(comparison.and_then(|c| c.clustering_off)),
                ),
                (
                    "clustering_on",
                    json::optional_number(comparison.and_then(|c| c.clustering_on)),
                ),
            ]),
        ),
        (
            "verdict",
            json::optional_string(comparison.map(|c| c.verdict().label())),
        ),
        (
            "multiplexer",
            json::optional_string(outcome.multiplexer.map(Multiplexer::name)),
        ),
    ])
}

/// The `--format debug` account: everything the probe saw and concluded.
fn debug_report(theme: &Theme, outcome: &ProbeOutcome) {
    println!();
    println!(
        "  {}",
        theme.paint("Grapheme clustering (mode 2027)", Ink::Title)
    );
    println!();
    environment(theme, outcome);

    if outcome.interactive {
        claim(theme, outcome);
        println!();
        behaviour(theme, outcome);

        if let Some(multiplexer) = outcome.multiplexer {
            println!();
            multiplexer_note(theme, multiplexer);
        }
    } else {
        not_interactive(theme);
    }

    // The byte listing is documentation of what a probe does, so it is worth
    // printing even when nothing was sent — `--format debug` through a pipe is
    // exactly how someone reads it.
    println!();
    raw_bytes(theme, outcome);

    println!();
    println!(
        "  {} {}",
        theme.paint("escapade grapheme explain", Ink::Code),
        theme.paint("for what mode 2027 is and who implements it", Ink::Muted)
    );
    println!();
}

/// The `TERM`/`TERM_PROGRAM`/multiplexer preamble.
fn environment(theme: &Theme, outcome: &ProbeOutcome) {
    let term_name = std::env::var("TERM").unwrap_or_else(|_| "unset".to_owned());
    print!("  {} {term_name}", theme.paint("terminal:", Ink::Muted));
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
}

/// The "there is nothing to probe" path.
fn not_interactive(theme: &Theme) {
    println!(
        "  {} {}",
        theme.paint("?", Ink::Warn),
        theme.paint(
            "cannot probe — stdin and stdout are not both terminals",
            Ink::Warn
        )
    );
    for line in note_lines(
        theme,
        "Both halves are needed: the query goes out on stdout and the answer comes back on stdin. \
         Through a pipe there is nobody to ask, so this is neither a yes nor a no. Run it directly.",
    ) {
        println!("{line}");
    }
    println!();
    println!(
        "  {} {}",
        theme.paint("escapade grapheme explain", Ink::Code),
        theme.paint("works anywhere, and needs no terminal", Ink::Muted)
    );
    println!(
        "  {} {}",
        theme.paint("escapade grapheme probe --format debug", Ink::Code),
        theme.paint("lists the bytes a probe sends, even from here", Ink::Muted)
    );
}

/// What DECRQM said.
fn claim(theme: &Theme, outcome: &ProbeOutcome) {
    println!("  {}", theme.paint("what it claims  (DECRQM)", Ink::Accent));

    // The control comes first, because it decides whether anything below it is a
    // finding or a coincidence.
    control(theme, outcome);

    let reliability = outcome.reliability();
    if let Reliability::Indiscriminate(sentinel) = reliability {
        discarded(theme, outcome, sentinel, reliability);
        return;
    }

    match outcome.state() {
        Some(state) => {
            let ink = if state.enabled() {
                Ink::Good
            } else if state.recognized() {
                Ink::Warn
            } else {
                Ink::Bad
            };
            let symbol = if state.enabled() {
                "✔"
            } else if state.recognized() {
                "◐"
            } else {
                "✘"
            };
            println!(
                "    {} {} {}",
                theme.paint(symbol, ink),
                theme.paint(state.label(), ink),
                theme.paint(&format!("(DECRPM value {})", state.value()), Ink::Muted)
            );
            for line in note_lines(theme, state.explanation()) {
                println!("{line}");
            }
            if state.settable() {
                println!(
                    "      {}  {}",
                    theme.paint(&pretty(&super::set(!state.enabled())), Ink::Code),
                    theme.paint(
                        if state.enabled() {
                            "turns it off"
                        } else {
                            "turns it on"
                        },
                        Ink::Muted
                    )
                );
            }
        }
        None if reliability == Reliability::NoDecrqm => {
            println!(
                "    {} {}",
                theme.paint("✘", Ink::Bad),
                theme.paint("this terminal implements no DECRQM", Ink::Bad)
            );
            for line in note_lines(theme, &reliability.explanation()) {
                println!("{line}");
            }
            for line in note_lines(
                theme,
                "That is a no about the *query*, not about clustering: a terminal can segment text \
                 perfectly and still have no way to say so. The measurement below is the one to \
                 trust. Konsole is a known example.",
            ) {
                println!("{line}");
            }
        }
        None if outcome.report.answered_barrier => {
            println!(
                "    {} {}",
                theme.paint("✘", Ink::Bad),
                theme.paint("no DECRQM answer for mode 2027", Ink::Bad)
            );
            for line in note_lines(
                theme,
                "The terminal answered the device-attributes barrier but said nothing about mode \
                 2027, so it does not implement DECRQM for it. This is a real no rather than a \
                 timeout — but it is a no about the *query*, not necessarily about clustering, so \
                 the measurement below is the one to trust.",
            ) {
                println!("{line}");
            }
        }
        None => {
            println!(
                "    {} {}",
                theme.paint("?", Ink::Warn),
                theme.paint("nothing came back at all", Ink::Warn)
            );
            for line in note_lines(theme, &reliability.explanation()) {
                println!("{line}");
            }
        }
    }
}

/// The impossible-mode control, and what it proves about the answers around it.
fn control(theme: &Theme, outcome: &ProbeOutcome) {
    let reliability = outcome.reliability();
    let (symbol, ink) = if reliability.trustworthy() {
        ("✔", Ink::Good)
    } else if matches!(reliability, Reliability::Indiscriminate(_)) {
        ("✘", Ink::Bad)
    } else {
        ("?", Ink::Muted)
    };
    println!(
        "    {} {}  {}",
        theme.paint(symbol, ink),
        theme.paint(
            &format!("control: impossible mode {SENTINEL_MODE}"),
            Ink::Muted
        ),
        theme.paint(
            &match outcome.report.sentinel {
                Some(state) => format!("answered {} ({})", state.label(), state.value()),
                None => "no answer".to_owned(),
            },
            ink
        )
    );
    println!(
        "      {}",
        theme.paint(&format!("→ {}", reliability.label()), ink)
    );
}

/// The path where the terminal answered, and the control proved it meaningless.
///
/// The literal answer is still shown, next to the reason it was thrown away: a
/// reader who ran this on iTerm2 needs to see the `4` *and* see why it is not the
/// `4` they would get from foot.
fn discarded(theme: &Theme, outcome: &ProbeOutcome, sentinel: ModeState, reliability: Reliability) {
    println!(
        "    {} {}",
        theme.paint("✘", Ink::Bad),
        theme.paint("mode 2027 answer discarded", Ink::Bad)
    );
    if let Some(state) = outcome.raw_state() {
        println!(
            "      {} {}",
            theme.paint("it said:", Ink::Muted),
            theme.paint(
                &format!("{} (DECRPM value {})", state.label(), state.value()),
                Ink::Muted
            )
        );
        if state == sentinel {
            println!(
                "      {}",
                theme.paint(
                    "— identical to what it said about a mode that does not exist",
                    Ink::Muted
                )
            );
        }
    }
    for line in note_lines(theme, &reliability.explanation()) {
        println!("{line}");
    }
    for line in note_lines(
        theme,
        "Nothing is lost: the measurement below never asked the terminal's opinion in the first \
         place.",
    ) {
        println!("{line}");
    }
}

/// What the terminal actually did with the reference sequence.
fn behaviour(theme: &Theme, outcome: &ProbeOutcome) {
    println!("  {}", theme.paint("what it does  (CSI 6 n)", Ink::Accent));

    let clusters = cluster::analyze(REFERENCE_SEQUENCE);
    let listing: Vec<String> = clusters
        .first()
        .map(|first| first.codepoints.iter().map(CharInfo::label).collect())
        .unwrap_or_default();
    println!(
        "    {} {}  {}",
        theme.paint("sequence:", Ink::Muted),
        theme.paint(&listing.join(" "), Ink::Code),
        REFERENCE_SEQUENCE
    );

    let Some(comparison) = outcome.comparison else {
        println!("    {}", theme.paint("(not measured)", Ink::Muted));
        return;
    };
    for line in comparison_lines(theme, &comparison) {
        println!("{line}");
    }

    if let Some(changed) = comparison.mode_made_a_difference() {
        println!(
            "    {}",
            theme.paint(
                if changed {
                    "switching the mode changed the count, so the terminal honours it"
                } else {
                    "switching the mode changed nothing — either it is unimplemented, or it was \
                     already deciding this the same way"
                },
                Ink::Muted
            )
        );
    }

    let verdict = comparison.verdict();
    let ink = match verdict {
        Verdict::Clustered => Ink::Good,
        Verdict::NotClustered => Ink::Bad,
        Verdict::Disagrees(_) => Ink::Warn,
        Verdict::Unknown => Ink::Muted,
    };
    println!(
        "    {} {}",
        theme.paint(verdict.symbol(), ink),
        theme.paint(verdict_sentence(verdict, &comparison), ink)
    );

    if let Some(sentence) = disagreement(outcome, verdict) {
        println!();
        for line in note_lines(theme, sentence) {
            println!("{line}");
        }
    }

    if !outcome.conclusive() {
        println!();
        for line in note_lines(
            theme,
            "Neither route reached a conclusion: no mode report, no barrier answer and no cursor \
             position report. Nothing here is a verdict about the terminal — it is a verdict about \
             the channel to it.",
        ) {
            println!("{line}");
        }
    }
}

/// What to say when the claim and the measurement do not agree.
///
/// The measurement wins. It is not a matter of preference: DECRQM reports a policy
/// a terminal believes about itself, and the cursor moved however far it moved.
/// Every case here is a real terminal — iTerm2 clusters without implementing the
/// mode, Konsole may cluster with no DECRQM to say so, and foot can report
/// permanently reset while a different width method quietly does the right thing.
#[must_use]
pub fn disagreement(outcome: &ProbeOutcome, verdict: Verdict) -> Option<&'static str> {
    // With nothing measured there is nothing to disagree with.
    if verdict == Verdict::Unknown {
        return None;
    }
    let clustered = verdict == Verdict::Clustered;
    let switching_worked = outcome
        .comparison
        .and_then(Comparison::mode_made_a_difference)
        == Some(true);
    match outcome.state() {
        // The sharpest contradiction available: the terminal called the mode
        // permanent, and then switching it changed the cell count.
        Some(state) if !state.settable() && switching_worked => Some(
            "The claim contradicts the measurement outright: DECRQM called the mode permanent, and \
             then switching it changed how many cells the sequence took. Whatever the report says, \
             the mode is live here. Trust the measurement.",
        ),
        Some(state) if state.enabled() && !clustered => Some(
            "The claim and the measurement disagree: DECRQM says clustering is on, and the cursor \
             did not move as if it were. Trust the measurement — it is what your layout will have \
             to live with.",
        ),
        Some(state) if state.recognized() && !state.enabled() && clustered => Some(
            "The claim and the measurement disagree: DECRQM says the mode is not in effect, yet the \
             sequence occupied the clustered width anyway. Some terminals get cluster widths right \
             without going through mode 2027. Trust the measurement.",
        ),
        Some(state) if !state.recognized() && clustered => Some(
            "DECRQM has never heard of mode 2027, and the terminal clustered anyway. Correct \
             behaviour and a missing capability query are independent; this is why measuring beats \
             asking.",
        ),
        None if clustered => Some(
            "Nothing usable came back from DECRQM, and the terminal clustered regardless. The mode \
             query is a hint; the cursor position report is the primitive, and here only the \
             primitive said anything.",
        ),
        _ => None,
    }
}

/// The four labelled numbers, as indented lines.
///
/// Shared with `supports` and `render --debug` so the same comparison always
/// reads the same way.
#[must_use]
pub fn comparison_lines(theme: &Theme, comparison: &Comparison) -> Vec<String> {
    let rows = [
        (
            "UAX #29 predicts",
            measure::cells(comparison.predicted),
            Ink::Accent,
        ),
        (
            "wcwidth predicts",
            measure::cells(comparison.legacy),
            Ink::Muted,
        ),
        (
            "clustering off",
            measure::measured_cells(comparison.clustering_off),
            Ink::Warn,
        ),
        (
            "clustering on",
            measure::measured_cells(comparison.clustering_on),
            Ink::Good,
        ),
    ];
    rows.iter()
        .map(|(label, value, ink)| {
            format!(
                "    {}{}  {}",
                theme.paint(label, Ink::Muted),
                " ".repeat(18usize.saturating_sub(label.chars().count())),
                theme.paint(value, *ink)
            )
        })
        .collect()
}

/// One sentence saying what the measurement proved.
#[must_use]
pub fn verdict_sentence(verdict: Verdict, comparison: &Comparison) -> &'static str {
    match verdict {
        Verdict::Clustered if !comparison.decisive() => {
            "the counts agree — but both rules predict the same width for this sequence, so it \
             proves nothing either way"
        }
        Verdict::Clustered => "the terminal advanced by cluster, exactly as predicted",
        Verdict::NotClustered => {
            "the terminal advanced per codepoint, so layouts built on cluster widths will drift here"
        }
        Verdict::Disagrees(_) => {
            "the terminal used a width neither rule predicts — its own table, or a multiplexer in \
             between"
        }
        Verdict::Unknown => "no cursor position report came back, so nothing was measured",
    }
}

/// The multiplexer caveat, which is a real one here.
fn multiplexer_note(theme: &Theme, multiplexer: Multiplexer) {
    println!(
        "  {} {}",
        theme.paint("note:", Ink::Warn),
        theme.paint(
            &format!(
                "{} is between this process and the terminal.",
                multiplexer.name()
            ),
            Ink::Warn
        )
    );
    for line in note_lines(
        theme,
        "That makes the multiplexer the thing counting cells, so the measurement above is a \
         measurement of it — which is the number that actually matters while you are inside it. \
         The mode is not forwarded, so the outer terminal's own answer is unreachable from here.",
    ) {
        println!("{line}");
    }
    println!(
        "      {}",
        theme.paint(multiplexer.passthrough_hint(), Ink::Code)
    );
}

/// `--verbose`: the bytes, named rather than emitted.
fn raw_bytes(theme: &Theme, outcome: &ProbeOutcome) {
    println!("  {}", theme.paint("what was sent", Ink::Accent));
    for (sequence, note) in [
        (query(), "DECRQM: what is mode 2027 doing?"),
        (
            sentinel_query(),
            "the control: a mode that cannot exist, to see if the answer above means anything",
        ),
        (barrier(), "the barrier every terminal answers"),
        (
            super::disable(),
            "clustering off, for the first measurement",
        ),
        (super::position_query(), "where did the cursor end up?"),
        (super::enable(), "clustering on, for the second"),
    ] {
        println!(
            "    {}{}  {}",
            theme.paint(&pretty(&sequence), Ink::Code),
            " ".repeat(20usize.saturating_sub(pretty(&sequence).chars().count())),
            theme.paint(note, Ink::Muted)
        );
    }

    println!();
    println!("  {}", theme.paint("what came back", Ink::Accent));
    if outcome.replies.is_empty() {
        println!(
            "    {}",
            theme.paint(
                "(replies are consumed as they arrive during measurement)",
                Ink::Muted
            )
        );
    }
    if let Some(state) = outcome.raw_state() {
        println!(
            "    {}",
            theme.paint(&format!("CSI ?2027;{}$y", state.value()), Ink::Code)
        );
    }
    if let Some(state) = outcome.report.sentinel {
        println!(
            "    {}  {}",
            theme.paint(
                &format!("CSI ?{SENTINEL_MODE};{}$y", state.value()),
                Ink::Code
            ),
            theme.paint("the control's answer", Ink::Muted)
        );
    }
    for reply in &outcome.replies {
        println!("    {}", theme.paint(&describe_reply(reply), Ink::Code));
    }
}

/// One-line rendering of a reply, guaranteed safe to print.
#[must_use]
pub fn describe_reply(reply: &Reply) -> String {
    match reply {
        Reply::Apc(payload) => format!("APC {payload} ST"),
        Reply::Dcs(payload) => format!("DCS {payload} ST"),
        Reply::Osc(payload) => format!("OSC {payload} ST"),
        Reply::Csi { params, final_byte } => format!("CSI {params}{final_byte}"),
    }
}

/// Wraps an explanatory note to the theme width, indented under a verdict.
fn note_lines(theme: &Theme, text: &str) -> Vec<String> {
    wrap(text, theme.width().saturating_sub(6))
        .into_iter()
        .map(|line| format!("      {}", theme.paint(&line, Ink::Muted)))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn measured(off: Option<usize>, on: Option<usize>) -> Comparison {
        Comparison {
            predicted: 2,
            legacy: 4,
            clustering_off: off,
            clustering_on: on,
        }
    }

    // -- the exit status ----------------------------------------------------

    fn outcome(
        state: Option<ModeState>,
        sentinel: Option<ModeState>,
        on: Option<usize>,
    ) -> ProbeOutcome {
        ProbeOutcome {
            report: ModeReport {
                state,
                sentinel,
                answered_barrier: true,
            },
            comparison: Some(measured(Some(4), on)),
            interactive: true,
            ..ProbeOutcome::default()
        }
    }

    #[test]
    fn the_measurement_decides_the_exit_status_when_there_is_one() {
        // Clustered, and the sequence could tell the rules apart: a real yes.
        assert_eq!(
            finding(&outcome(None, None, Some(2))),
            Finding::Supported,
            "measured clustering beats a silent DECRQM"
        );
        // Per-codepoint: a real no, even if DECRQM claimed the mode is set.
        assert_eq!(
            finding(&outcome(
                Some(ModeState::Set),
                Some(ModeState::NotRecognized),
                Some(4)
            )),
            Finding::Unsupported,
            "the cursor moved four cells; the claim does not get a vote"
        );
        // A third answer nobody predicted is still a no, not an unknown.
        assert_eq!(finding(&outcome(None, None, Some(6))), Finding::Unsupported);
    }

    #[test]
    fn an_indiscriminate_terminal_cannot_rescue_an_unmeasurable_probe() {
        // iTerm2's `4` would read as `Unsupported` if it were believed. With the
        // control in place it is discarded, and the honest answer is "undetermined".
        let unmeasurable = ProbeOutcome {
            report: ModeReport {
                state: Some(ModeState::PermanentlyReset),
                sentinel: Some(ModeState::PermanentlyReset),
                answered_barrier: true,
            },
            comparison: Some(measured(None, None)),
            interactive: true,
            ..ProbeOutcome::default()
        };
        assert_eq!(finding(&unmeasurable), Finding::Undetermined);

        // The same reply, vouched for by the control, *is* a no.
        let honest = ProbeOutcome {
            report: ModeReport {
                state: Some(ModeState::PermanentlyReset),
                sentinel: Some(ModeState::NotRecognized),
                answered_barrier: true,
            },
            ..unmeasurable.clone()
        };
        assert_eq!(finding(&honest), Finding::Unsupported);
    }

    #[test]
    fn a_trustworthy_claim_answers_when_nothing_could_be_measured() {
        for (state, expected) in [
            (ModeState::Set, Finding::Supported),
            (ModeState::PermanentlySet, Finding::Supported),
            (ModeState::Reset, Finding::Supported),
            (ModeState::PermanentlyReset, Finding::Unsupported),
            (ModeState::NotRecognized, Finding::Unsupported),
        ] {
            let probe = ProbeOutcome {
                report: ModeReport {
                    state: Some(state),
                    sentinel: Some(ModeState::NotRecognized),
                    answered_barrier: true,
                },
                comparison: Some(measured(None, None)),
                interactive: true,
                ..ProbeOutcome::default()
            };
            assert_eq!(finding(&probe), expected, "{state:?}");
        }
    }

    #[test]
    fn a_piped_probe_is_undetermined_rather_than_a_no() {
        // The most important exit status in the set: a script must not read
        // "I could not ask" as "your terminal cannot do this".
        let piped = ProbeOutcome::default();
        assert_eq!(finding(&piped), Finding::Undetermined);
        assert_ne!(finding(&piped), Finding::Unsupported);
    }

    #[test]
    fn the_exit_codes_are_the_documented_ones() {
        assert_eq!(
            format!("{:?}", Finding::Supported.exit_code()),
            format!("{:?}", std::process::ExitCode::from(0))
        );
        assert_eq!(
            format!("{:?}", Finding::Unsupported.exit_code()),
            format!("{:?}", std::process::ExitCode::from(1))
        );
        assert_eq!(
            format!("{:?}", Finding::Undetermined.exit_code()),
            format!("{:?}", std::process::ExitCode::from(2))
        );
    }

    #[test]
    fn the_json_line_is_one_line_and_names_the_control() {
        let probe = outcome(
            Some(ModeState::PermanentlyReset),
            Some(ModeState::PermanentlyReset),
            Some(2),
        );
        let line = json_line(&probe, finding(&probe));
        let text = line.as_str();
        assert!(!text.contains('\n'), "{text}");
        assert!(text.starts_with('{') && text.ends_with('}'), "{text}");
        assert!(text.contains(r#""finding":"supported""#), "{text}");
        assert!(text.contains(r#""reliability":"indiscriminate""#), "{text}");
        assert!(text.contains(r#""control_mode":9876543"#), "{text}");
        // The discarded reply is still reported, distinctly from the trusted one.
        assert!(text.contains(r#""state":null"#), "{text}");
        assert!(text.contains(r#""reported":"permanently reset""#), "{text}");
        // And no raw control byte or bare emoji anywhere in it.
        assert!(!text.chars().any(char::is_control), "{text}");
        assert!(text.is_ascii(), "{text}");
    }

    #[test]
    fn a_measurement_concludes_even_when_decrqm_stays_silent() {
        let outcome = ProbeOutcome {
            interactive: true,
            comparison: Some(measured(Some(4), Some(2))),
            ..ProbeOutcome::default()
        };
        assert!(outcome.conclusive());
        assert_eq!(outcome.state(), None, "it never said anything");
    }

    #[test]
    fn decrqm_alone_concludes_too() {
        let outcome = ProbeOutcome {
            interactive: true,
            report: ModeReport {
                state: Some(ModeState::Reset),
                sentinel: Some(ModeState::NotRecognized),
                answered_barrier: true,
            },
            ..ProbeOutcome::default()
        };
        assert!(outcome.conclusive());
        assert_eq!(outcome.state(), Some(ModeState::Reset));
    }

    #[test]
    fn total_silence_is_inconclusive_not_negative() {
        let outcome = ProbeOutcome {
            interactive: true,
            comparison: Some(measured(None, None)),
            ..ProbeOutcome::default()
        };
        assert!(
            !outcome.conclusive(),
            "no barrier and no measurement means nothing was learned"
        );
    }

    #[test]
    fn a_barrier_with_no_mode_report_is_a_definite_no_about_the_query() {
        let outcome = ProbeOutcome {
            interactive: true,
            report: ModeReport {
                state: None,
                sentinel: Some(ModeState::NotRecognized),
                answered_barrier: true,
            },
            ..ProbeOutcome::default()
        };
        assert!(outcome.conclusive());
        assert_eq!(outcome.state(), None);
    }

    #[test]
    fn the_verdict_sentence_refuses_to_overclaim_on_an_undecidable_sequence() {
        let undecidable = Comparison {
            predicted: 1,
            legacy: 1,
            clustering_off: Some(1),
            clustering_on: Some(1),
        };
        let sentence = verdict_sentence(undecidable.verdict(), &undecidable);
        assert!(sentence.contains("proves nothing"), "{sentence}");

        let decisive = measured(Some(4), Some(2));
        let sentence = verdict_sentence(decisive.verdict(), &decisive);
        assert!(sentence.contains("by cluster"), "{sentence}");
    }

    #[test]
    fn a_permanent_mode_that_switches_is_reported_as_a_contradiction() {
        // foot answers `4` when its width method cannot honour the mode. If
        // switching it then changes the count, the report is simply wrong — and
        // that is worth saying more sharply than "they disagree".
        let outcome = ProbeOutcome {
            report: ModeReport {
                state: Some(ModeState::PermanentlyReset),
                sentinel: Some(ModeState::NotRecognized),
                answered_barrier: true,
            },
            comparison: Some(measured(Some(4), Some(2))),
            interactive: true,
            ..ProbeOutcome::default()
        };
        let sentence = disagreement(&outcome, Verdict::Clustered).unwrap_or_default();
        assert!(sentence.contains("contradicts"), "{sentence}");
        assert!(sentence.contains("called the mode permanent"), "{sentence}");

        // Without a change in the count there is no contradiction to report.
        let unchanged = ProbeOutcome {
            comparison: Some(measured(Some(2), Some(2))),
            ..outcome.clone()
        };
        let sentence = disagreement(&unchanged, Verdict::Clustered).unwrap_or_default();
        assert!(!sentence.contains("contradicts"), "{sentence}");
    }

    #[test]
    fn nothing_measured_means_nothing_to_disagree_with() {
        let outcome = ProbeOutcome {
            report: ModeReport {
                state: Some(ModeState::Set),
                sentinel: Some(ModeState::NotRecognized),
                answered_barrier: true,
            },
            comparison: Some(measured(None, None)),
            interactive: true,
            ..ProbeOutcome::default()
        };
        assert_eq!(disagreement(&outcome, Verdict::Unknown), None);
    }

    #[test]
    fn a_discarded_answer_leaves_only_the_measurement_speaking() {
        // iTerm2's shape: the claim is void, so the note must credit the
        // measurement rather than describe a disagreement between two claims.
        let outcome = ProbeOutcome {
            report: ModeReport {
                state: Some(ModeState::PermanentlyReset),
                sentinel: Some(ModeState::PermanentlyReset),
                answered_barrier: true,
            },
            comparison: Some(measured(Some(4), Some(2))),
            interactive: true,
            ..ProbeOutcome::default()
        };
        let sentence = disagreement(&outcome, Verdict::Clustered).unwrap_or_default();
        assert!(sentence.contains("Nothing usable came back"), "{sentence}");
        assert!(sentence.contains("primitive"), "{sentence}");
    }

    #[test]
    fn every_verdict_gets_its_own_sentence() {
        let comparison = measured(Some(4), Some(4));
        let sentences = [
            Verdict::Clustered,
            Verdict::NotClustered,
            Verdict::Disagrees(6),
            Verdict::Unknown,
        ]
        .map(|verdict| verdict_sentence(verdict, &comparison));
        let mut seen = sentences.to_vec();
        seen.sort_unstable();
        let total = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), total);
    }

    #[test]
    fn comparison_lines_name_all_four_numbers() {
        let theme = Theme::plain();
        let lines = comparison_lines(&theme, &measured(Some(4), Some(2))).join("\n");
        assert!(lines.contains("UAX #29 predicts"), "{lines}");
        assert!(lines.contains("wcwidth predicts"), "{lines}");
        assert!(lines.contains("clustering off"), "{lines}");
        assert!(lines.contains("clustering on"), "{lines}");
        assert!(lines.contains("2 cells"), "{lines}");
        assert!(lines.contains("4 cells"), "{lines}");
    }

    #[test]
    fn comparison_lines_say_no_answer_rather_than_zero() {
        let theme = Theme::plain();
        let lines = comparison_lines(&theme, &measured(None, None)).join("\n");
        assert!(lines.contains("no answer"), "{lines}");
        assert!(
            !lines.contains("0 cells"),
            "a silence is not a zero: {lines}"
        );
    }

    /// Prints the claim block for one hand-built report, so the three sentinel
    /// cases can be eyeballed as well as asserted.
    fn show(label: &str, report: ModeReport) -> String {
        let theme = Theme::plain();
        println!("--- {label} ---");
        let outcome = ProbeOutcome {
            report,
            comparison: Some(measured(Some(4), Some(2))),
            interactive: true,
            ..ProbeOutcome::default()
        };
        claim(&theme, &outcome);
        behaviour(&theme, &outcome);
        format!("{:?}", outcome.reliability())
    }

    #[test]
    fn the_three_sentinel_cases_render_distinctly() {
        // Case 1: control says `0`, so the `4` for mode 2027 is a real finding.
        assert_eq!(
            show(
                "case 1: discriminating (foot-like, a real permanent reset)",
                ModeReport {
                    state: Some(ModeState::PermanentlyReset),
                    sentinel: Some(ModeState::NotRecognized),
                    answered_barrier: true,
                }
            ),
            "Discriminating"
        );
        // Case 2: control also says `4`, so nothing said about 2027 counts.
        assert_eq!(
            show(
                "case 2: indiscriminate (iTerm2-like, the same `4` for everything)",
                ModeReport {
                    state: Some(ModeState::PermanentlyReset),
                    sentinel: Some(ModeState::PermanentlyReset),
                    answered_barrier: true,
                }
            ),
            "Indiscriminate(PermanentlyReset)"
        );
        // Case 3: neither answered, barrier did — no DECRQM at all.
        assert_eq!(
            show(
                "case 3: no DECRQM at all (Konsole-like)",
                ModeReport {
                    state: None,
                    sentinel: None,
                    answered_barrier: true,
                }
            ),
            "NoDecrqm"
        );
    }

    #[test]
    fn replies_render_without_control_bytes() {
        let replies = [
            Reply::Csi {
                params: "?2027;1".to_owned(),
                final_byte: 'y',
            },
            Reply::Csi {
                params: "14;3".to_owned(),
                final_byte: 'R',
            },
            Reply::Apc("25a1;s".to_owned()),
            Reply::Osc("11;rgb:00/00/00".to_owned()),
            Reply::Dcs("1$r0m".to_owned()),
        ];
        for reply in &replies {
            let rendered = describe_reply(reply);
            assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        }
    }

    #[test]
    fn printing_a_report_never_panics_on_any_outcome() {
        let theme = Theme::plain();
        let states = [
            None,
            Some(ModeState::NotRecognized),
            Some(ModeState::Set),
            Some(ModeState::Reset),
            Some(ModeState::PermanentlySet),
            Some(ModeState::PermanentlyReset),
            Some(ModeState::Undefined(9)),
        ];
        // Every sentinel answer crossed with every mode answer, because the
        // reliability branches are where a formatting slip would hide.
        let sentinels = [
            None,
            Some(ModeState::NotRecognized),
            Some(ModeState::PermanentlyReset),
            Some(ModeState::Set),
            Some(ModeState::Undefined(7)),
        ];
        for (state, sentinel) in states
            .into_iter()
            .flat_map(|state| sentinels.into_iter().map(move |sentinel| (state, sentinel)))
        {
            for barrier_answered in [false, true] {
                for comparison in [
                    None,
                    Some(measured(Some(4), Some(2))),
                    Some(measured(None, None)),
                ] {
                    let outcome = ProbeOutcome {
                        report: ModeReport {
                            state,
                            sentinel,
                            answered_barrier: barrier_answered,
                        },
                        comparison,
                        multiplexer: Some(Multiplexer::Tmux),
                        interactive: true,
                        replies: vec![Reply::Csi {
                            params: "?62;4".to_owned(),
                            final_byte: 'c',
                        }],
                    };
                    environment(&theme, &outcome);
                    claim(&theme, &outcome);
                    behaviour(&theme, &outcome);
                    multiplexer_note(&theme, Multiplexer::Tmux);
                    raw_bytes(&theme, &outcome);
                }
            }
        }
        not_interactive(&theme);
    }
}
