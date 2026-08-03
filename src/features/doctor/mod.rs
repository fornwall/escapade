//! `escapade doctor` — diagnose a terminal by cross-checking its capabilities
//! against three independent sources.
//!
//! Three things claim to describe a terminal's capabilities, and they routinely
//! disagree:
//!
//! 1. **terminfo** — what the database entry for `$TERM` says. Often stale, often
//!    for a different terminal than the one actually running, and `$TERM` is
//!    frequently a lie: `xterm-256color` is set by a great many terminals that are
//!    not xterm.
//! 2. **The terminal's own answer** — XTGETTCAP (`DCS + q <hex> ST`) and the mode
//!    and attribute queries. The terminal describing itself.
//! 3. **Observed behaviour** — what happens when you do the thing and measure the
//!    result.
//!
//! A fourth, the environment, asserts capabilities without being asked:
//! `COLORTERM=truecolor` is a claim like any other and is diffed against the rest.
//!
//! Every existing tool picks one source and hopes, or silently upgrades from one to
//! another. **Nothing diffs them.** That is what this does: not "what can this
//! terminal do" but "which of the things telling you what this terminal can do is
//! wrong".
//!
//! ```text
//! exit 0   every check ran and the sources agree
//! exit 1   at least one disagreement was found
//! exit 2   the checks could not be run — there was no terminal to ask
//! ```
//!
//! # Layout
//!
//! * [`checks`] — the catalogue, `data/checks.toml`, and its loader.
//! * [`terminfo`] — source one: `infocmp -x -1`, and its parser.
//! * [`tcap`] — source two, part one: XTGETTCAP encoding and reply grammar.
//! * [`crate::decrqm`] — source two, part two: DECRQM, DECRPM, and the sentinel.
//! * [`observe`] — source three: the probes that measure, each split into a query
//!   builder and a pure interpreter.
//! * [`identity`] — normalising the many names a terminal goes by.
//! * [`session`] — which channel, for how long, and how a round ends.
//! * [`verdict`] — claims, and whether they agree.
//! * [`report`] and [`json`] — the two output shapes.

pub mod checks;
pub mod identity;
pub mod json;
pub mod observe;
pub mod report;
pub mod session;
pub mod tcap;
pub mod terminfo;
pub mod verdict;

use std::{collections::BTreeMap, process::ExitCode, time::Duration};

use anyhow::{Context as _, Result};

use crate::{
    cli::{DoctorCommand, OutputFormat},
    decrqm::{self, ModeState, Sentinel},
    features::{
        Support,
        doctor::{
            checks::{Catalogue, Check, CheckKind, ObservedProbe},
            observe::Erase,
            session::{Channel, NoChannel, Round, Session},
            verdict::{Agreement, Claim, Origin, Reading, agreement},
        },
    },
    term::Multiplexer,
    theme::Theme,
};

/// Entry point for `escapade doctor`.
///
/// Returns an exit status rather than `()` because that status *is* the finding: a
/// script wants to branch on whether the terminal is healthy without parsing any
/// output.
///
/// # Errors
///
/// Fails if the check catalogue is malformed, if `--only` names no check, or if
/// the terminal cannot be written to. A terminal that stays quiet is a diagnosis,
/// not an error.
pub fn run(command: &DoctorCommand, no_color: bool) -> Result<ExitCode> {
    let catalogue = Catalogue::load()?;
    let theme = Theme::new(no_color);

    if command.list {
        report::list(&theme, &catalogue);
        return Ok(ExitCode::SUCCESS);
    }

    let selected = select(&catalogue, command.only.as_deref())?;
    let diagnosis = diagnose(command, &selected);

    match command.format {
        OutputFormat::Text => report::render(&theme, &diagnosis, command.disagreements),
        OutputFormat::Json => json::render(&diagnosis, command.disagreements),
    }
    Ok(diagnosis.exit_code())
}

/// Resolves `--only` against the check ids.
///
/// [`crate::features::select_steps`] already does exact, case-insensitive and
/// unambiguous-prefix matching, and is reused rather than reimplemented. The
/// context line names what was being matched, since the shared function does not
/// know it is looking at checks rather than demo steps.
fn select<'a>(catalogue: &'a Catalogue, only: Option<&str>) -> Result<Vec<&'a Check>> {
    let ids = catalogue.ids();
    let selection = crate::features::select_steps(&ids, only, None)
        .context("resolving --only against the checks in data/checks.toml")?;
    if only.is_some() {
        Ok(vec![&catalogue.checks()[selection.start]])
    } else {
        Ok(catalogue.checks().iter().collect())
    }
}

/// One check's four readings and what they add up to.
#[derive(Debug, Clone)]
pub struct Row<'a> {
    /// The check this row answers.
    pub check: &'a Check,
    /// What each source said, in [`Origin::ALL`] order.
    pub readings: Vec<Reading>,
    /// Whether the sources agree.
    pub agreement: Agreement,
}

impl Row<'_> {
    /// The reading from one source.
    #[must_use]
    pub fn reading(&self, origin: Origin) -> Option<&Reading> {
        self.readings
            .iter()
            .find(|reading| reading.origin == origin)
    }
}

/// Everything the run established.
#[derive(Debug)]
pub struct Diagnosis<'a> {
    /// One row per selected check.
    pub rows: Vec<Row<'a>>,
    /// What happened when the terminfo entry was read.
    pub terminfo: terminfo::Source,
    /// Which channel was opened, if any.
    pub channel: Option<Channel>,
    /// Why no channel could be opened, if none could.
    pub no_channel: Option<NoChannel>,
    /// A multiplexer between this process and the real terminal.
    pub multiplexer: Option<Multiplexer>,
    /// What the impossible-mode control established.
    pub sentinel: Sentinel,
    /// Whether the terminal ever answered anything.
    pub heard_anything: bool,
    /// Whether DECRQM is silent for every real mode behind an answered barrier —
    /// which is Konsole's behaviour, and a stronger finding than "no answer".
    ///
    /// Only ever set outside a multiplexer: inside one the barrier is answered by
    /// the multiplexer rather than the terminal, so it licenses nothing.
    pub decrqm_not_implemented: bool,
    /// The reply timeout that was used.
    pub timeout: Duration,
    /// `$TERM`, as found.
    pub term: Option<String>,
    /// `TERM_PROGRAM`, as found.
    pub term_program: Option<String>,
}

impl Diagnosis<'_> {
    /// How many rows disagree.
    #[must_use]
    pub fn disagreements(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.agreement == Agreement::Disagree)
            .count()
    }

    /// How many rows had nothing to compare.
    #[must_use]
    pub fn incomparable(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.agreement == Agreement::NotComparable)
            .count()
    }

    /// The process exit status.
    ///
    /// `2` is checked first and dominates: with no terminal to ask, two of the four
    /// sources are missing, and a disagreement between the two that remain is worth
    /// printing but is not the diagnosis the caller asked for. A script that got `1`
    /// would believe the terminal had been examined.
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        if !self.heard_anything {
            return ExitCode::from(2);
        }
        if self.disagreements() > 0 {
            return ExitCode::from(1);
        }
        ExitCode::SUCCESS
    }

    /// Whether the terminal was reachable at all.
    #[must_use]
    pub fn probed(&self) -> bool {
        self.heard_anything
    }
}

// ---------------------------------------------------------------------------
// Running the checks
// ---------------------------------------------------------------------------

/// What the terminal said, gathered before any of it is interpreted.
#[derive(Debug, Default)]
struct Answers {
    /// XTGETTCAP answers, by capability name.
    tcap: BTreeMap<String, TcapAnswer>,
    /// Whether the XTGETTCAP round was answered at all.
    tcap_round: Round,
    /// DECRPM answers, by mode.
    modes: BTreeMap<u32, ModeState>,
    /// The impossible-mode control.
    sentinel: Sentinel,
    /// Whether the DECRQM round's barrier came back.
    decrqm_barrier: bool,
    /// The SGR readback.
    sgr: Option<observe::SgrReadback>,
    /// The cell-size ladder.
    cell_size: CellSizeAnswer,
    /// XTVERSION and the device attributes.
    identity: Option<observe::Identity>,
    /// The tty's erase character.
    erase: Option<Erase>,
}

/// What the cell-size ladder produced.
///
/// Three states rather than a nested option, because "not asked" and "asked and
/// nothing answered" have different explanations and must not be collapsed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum CellSizeAnswer {
    /// No selected check needed a cell size, so nothing was sent.
    #[default]
    NotProbed,
    /// Every rung of the ladder was tried and none answered.
    NoAnswer,
    /// A measurement, and which rung produced it.
    Measured(observe::CellSize),
}

/// What XTGETTCAP said about one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TcapAnswer {
    /// `DCS 1 + r <name> = <value> ST`. The value is `None` for a bare name.
    Present(Option<String>),
    /// `DCS 0 + r … ST` — the terminal explicitly disowned the capability.
    Absent,
}

/// Runs every source and builds the rows.
fn diagnose<'a>(command: &DoctorCommand, selected: &[&'a Check]) -> Diagnosis<'a> {
    let terminfo = terminfo::read(command.terminfo.as_deref());
    let timeout = Duration::from_millis(command.timeout_ms.max(1));

    let mut answers = Answers::default();
    // Read before raw mode: this reports the line discipline the user has, not the
    // one this process installed.
    if selected
        .iter()
        .any(|check| check.observed == ObservedProbe::SttyErase)
    {
        answers.erase = Some(observe::stty_erase());
    }

    let mut channel = None;
    let mut no_channel = None;
    let mut heard_anything = false;
    let mut multiplexer = Multiplexer::detect();

    match Session::open(command.probe_mode, timeout) {
        Ok(Ok(mut session)) => {
            channel = Some(session.channel());
            multiplexer = session.multiplexer();
            // A write failure mid-round is reported as silence: the terminal is
            // unreachable, which is exactly what the missing answers say.
            let _ = interrogate(&mut session, selected, &mut answers);
            heard_anything = session.heard_anything();
            // Explicitly, so the report prints into a cooked terminal.
            session.close();
        }
        Ok(Err(reason)) => no_channel = Some(reason),
        Err(error) => {
            no_channel = Some(NoChannel {
                reason: format!("The reply reader could not be started: {error:#}."),
                hint: None,
            });
        }
    }

    let decrqm_wanted = selected
        .iter()
        .any(|check| check.observed == ObservedProbe::Decrqm);
    let decrqm_not_implemented = decrqm_wanted
        && answers.decrqm_barrier
        && answers.modes.is_empty()
        && answers.sentinel.state.is_none()
        && multiplexer.is_none();

    let rows = selected
        .iter()
        .map(|check| build_row(check, &terminfo, &answers, multiplexer))
        .collect();

    Diagnosis {
        rows,
        terminfo,
        channel,
        no_channel,
        multiplexer,
        sentinel: answers.sentinel,
        heard_anything,
        decrqm_not_implemented,
        timeout,
        term: std::env::var("TERM").ok(),
        term_program: std::env::var("TERM_PROGRAM").ok(),
    }
}

/// Sends every query the selected checks need, one round per source.
fn interrogate(session: &mut Session, selected: &[&Check], answers: &mut Answers) -> Result<()> {
    // Round one: XTGETTCAP, one query per name behind a single barrier.
    let names = tcap_names(selected);
    if !names.is_empty() {
        let queries: String = names.iter().map(|name| tcap::query(name)).collect();
        let round = session.round(&queries)?;
        answers.tcap = collect_tcap(&names, &round.replies);
        answers.tcap_round = round;
    }

    // Round two: DECRQM, the impossible mode first.
    let modes = decrqm_modes(selected);
    if !modes.is_empty() {
        let mut queries = decrqm::query(decrqm::SENTINEL_MODE);
        for &mode in &modes {
            queries.push_str(&decrqm::query(mode));
        }
        let round = session.round(&queries)?;
        answers.decrqm_barrier = round.answered_barrier;
        for reply in &round.replies {
            if let Some((mode, state)) = decrqm::report_from_reply(reply) {
                if mode == decrqm::SENTINEL_MODE {
                    answers.sentinel.state = Some(state);
                } else {
                    answers.modes.insert(mode, state);
                }
            }
        }
        answers.sentinel.answered_barrier = round.answered_barrier;
    }

    // Rounds three to five: one per observed probe that needs its own conversation.
    if wants(selected, ObservedProbe::SgrReadback) {
        let round = session.round(&observe::sgr_readback_query())?;
        answers.sgr = Some(observe::interpret_sgr(&round));
    }
    if wants(selected, ObservedProbe::CellSize) {
        let round = session.round(&observe::cell_size_query())?;
        answers.cell_size = match observe::interpret_cell_size(&round) {
            Some(size) => CellSizeAnswer::Measured(size),
            None => CellSizeAnswer::NoAnswer,
        };
    }
    if wants(selected, ObservedProbe::Identity) {
        let round = session.round(&observe::identity_query())?;
        answers.identity = Some(observe::interpret_identity(&round));
    }
    Ok(())
}

/// Whether any selected check uses `probe`.
fn wants(selected: &[&Check], probe: ObservedProbe) -> bool {
    selected.iter().any(|check| check.observed == probe)
}

/// Every XTGETTCAP name the selected checks need, deduplicated, in order.
fn tcap_names(selected: &[&Check]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for check in selected {
        for spec in &check.xtgettcap {
            if !names.contains(&spec.name) {
                names.push(spec.name.clone());
            }
        }
    }
    names
}

/// Every DEC private mode the selected checks need, deduplicated, in order.
fn decrqm_modes(selected: &[&Check]) -> Vec<u32> {
    let mut modes: Vec<u32> = Vec::new();
    for check in selected {
        if check.observed == ObservedProbe::Decrqm
            && let Some(mode) = check.mode
            && !modes.contains(&mode)
        {
            modes.push(mode);
        }
    }
    modes
}

/// Matches XTGETTCAP replies to the names that were asked for.
///
/// Successful replies identify themselves, so they are matched by name. Failures
/// are the problem: the documented shape is `DCS 0 + r ST` with no payload at all,
/// so an anonymous failure says only that *something* was refused. Terminals
/// process input in order, so anonymous failures are assigned in order to the
/// names still waiting for an answer — which is correct, and is the only reading
/// available without one round trip per name.
#[must_use]
pub fn collect_tcap(
    queried: &[String],
    replies: &[crate::input::Reply],
) -> BTreeMap<String, TcapAnswer> {
    let mut answers: BTreeMap<String, TcapAnswer> = BTreeMap::new();
    let mut anonymous_failures = 0usize;

    for reply in replies {
        let crate::input::Reply::Dcs(payload) = reply else {
            continue;
        };
        match tcap::parse(payload) {
            Some(tcap::Reply::Found(entries)) => {
                for entry in entries {
                    answers.insert(entry.name, TcapAnswer::Present(entry.value));
                }
            }
            Some(tcap::Reply::NotFound(names)) => {
                if names.is_empty() {
                    anonymous_failures += 1;
                } else {
                    for name in names {
                        answers.insert(name, TcapAnswer::Absent);
                    }
                }
            }
            None => {}
        }
    }

    for name in queried {
        if anonymous_failures == 0 {
            break;
        }
        if !answers.contains_key(name) {
            answers.insert(name.clone(), TcapAnswer::Absent);
            anonymous_failures -= 1;
        }
    }
    answers
}

// ---------------------------------------------------------------------------
// Turning answers into readings
// ---------------------------------------------------------------------------

/// Builds one row: four readings and their agreement.
fn build_row<'a>(
    check: &'a Check,
    source: &terminfo::Source,
    answers: &Answers,
    multiplexer: Option<Multiplexer>,
) -> Row<'a> {
    let readings = vec![
        read_terminfo(check, source),
        read_terminal(check, answers),
        read_observed(check, answers, multiplexer),
        read_environment(check),
    ];
    let agreement = agreement(&readings);
    Row {
        check,
        readings,
        agreement,
    }
}

/// Source one: the terminfo entry.
fn read_terminfo(check: &Check, source: &terminfo::Source) -> Reading {
    let origin = Origin::Terminfo;
    if check.terminfo.is_empty() {
        return Reading::not_applicable(
            origin,
            "no terminfo capability describes this at all — see the note",
        );
    }
    let Some(entry) = source.entry() else {
        // A missing entry is not a claim of absence.
        return Reading::silent(origin, source.explanation());
    };

    if check.kind == CheckKind::Identity {
        return match identity::family(&entry.name) {
            Some(family) => Reading::new(origin, Claim::value(family)).with_raw(entry.name.clone()),
            None => Reading::silent(
                origin,
                format!("`{}` is not a name this recognises", entry.name),
            )
            .with_raw(entry.name.clone()),
        };
    }

    // The entry's own name, for a spec that asks for `TN` rather than a capability.
    let lookup = |spec: &checks::CapSpec| -> Option<Option<String>> {
        if spec.is_terminal_name() {
            return Some(Some(entry.name.clone()));
        }
        entry
            .get(&spec.name)
            .map(|value| value.text().map(str::to_owned))
    };

    match check.kind {
        CheckKind::Value => {
            for spec in &check.terminfo {
                if let Some(value) = lookup(spec) {
                    let raw = value.clone().unwrap_or_default();
                    return Reading::new(origin, Claim::value(normalise_value(&raw)))
                        .with_raw(format!("{}={}", spec.name, observe::printable(&raw)));
                }
            }
            // Deliberately silent rather than a `no`: this check compares a
            // *value*, and a capability that is not there has no value. Claiming
            // `no` would make the word "no" the value and disagree with every
            // source that reported a real one.
            Reading::silent(
                origin,
                format!(
                    "the entry has no {}, so it offers no value to compare",
                    spec_list(&check.terminfo)
                ),
            )
        }
        CheckKind::Boolean => {
            for spec in &check.terminfo {
                let Some(value) = lookup(spec) else { continue };
                if spec.satisfied_by(value.as_deref()) {
                    return Reading::new(origin, Claim::from_support(Support::Yes))
                        .with_raw(render_cap(&spec.name, value.as_deref()));
                }
            }
            Reading::new(origin, Claim::from_support(Support::No)).with_detail(format!(
                "the entry has no {} — and a terminfo entry is meant to describe the terminal \
                 exhaustively, so this is a claim rather than a gap",
                spec_list(&check.terminfo)
            ))
        }
        // Handled above.
        CheckKind::Identity => Reading::silent(origin, "unreachable"),
    }
}

/// Source two: what the terminal said about itself.
fn read_terminal(check: &Check, answers: &Answers) -> Reading {
    let origin = Origin::Terminal;
    if check.xtgettcap.is_empty() {
        return Reading::not_applicable(
            origin,
            "there is no capability name to ask the terminal for — see the note",
        );
    }
    if !answers.tcap_round.heard_anything() {
        return Reading::silent(origin, "the terminal was not reached");
    }

    let mut disowned = Vec::new();
    for spec in &check.xtgettcap {
        match answers.tcap.get(&spec.name) {
            Some(TcapAnswer::Present(value)) => {
                if check.kind == CheckKind::Identity {
                    let raw = value.clone().unwrap_or_default();
                    return match identity::family(&raw) {
                        Some(family) => Reading::new(origin, Claim::value(family)).with_raw(raw),
                        None => Reading::silent(
                            origin,
                            format!(
                                "`{}` is not a name this recognises",
                                observe::printable(&raw)
                            ),
                        )
                        .with_raw(observe::printable(&raw)),
                    };
                }
                if check.kind == CheckKind::Value {
                    let raw = value.clone().unwrap_or_default();
                    return Reading::new(origin, Claim::value(normalise_value(&raw)))
                        .with_raw(format!("{}={}", spec.name, observe::printable(&raw)));
                }
                if spec.satisfied_by(value.as_deref()) {
                    return Reading::new(origin, Claim::from_support(Support::Yes))
                        .with_raw(render_cap(&spec.name, value.as_deref()));
                }
                // Present but below the threshold: an explicit answer, and a no.
                return Reading::new(origin, Claim::from_support(Support::No))
                    .with_raw(render_cap(&spec.name, value.as_deref()))
                    .with_detail(format!(
                        "the terminal reported {}, which is below the threshold this check needs",
                        render_cap(&spec.name, value.as_deref())
                    ));
            }
            Some(TcapAnswer::Absent) => disowned.push(spec.name.clone()),
            None => {}
        }
    }

    if disowned.is_empty() {
        return Reading::silent(
            origin,
            "the terminal did not answer XTGETTCAP, which most terminals do not implement",
        );
    }
    let raw = format!("DCS 0 + r for {}", disowned.join(", "));
    if check.kind == CheckKind::Boolean {
        return Reading::new(origin, Claim::from_support(Support::No))
            .with_raw(raw)
            .with_detail(
                "the terminal answered XTGETTCAP and explicitly disowned the capability, which is \
                 a stronger statement than terminfo simply lacking it"
                    .to_owned(),
            );
    }
    // A value or identity check needs a value. "I do not have that capability"
    // is an answer to a different question, and comparing the word `no` against
    // an erase character or a terminal name would report a disagreement that is
    // purely an artefact of the model.
    Reading::silent(
        origin,
        format!(
            "the terminal answered XTGETTCAP and disowned {}, so it offered no value to compare",
            disowned.join(", ")
        ),
    )
    .with_raw(raw)
}

/// Source three: what the terminal actually did.
fn read_observed(check: &Check, answers: &Answers, multiplexer: Option<Multiplexer>) -> Reading {
    let origin = Origin::Observed;
    match check.observed {
        ObservedProbe::None => Reading::not_applicable(
            origin,
            "no observed test is offered for this, and the note says why",
        ),

        ObservedProbe::Decrqm => read_decrqm(check.mode, answers, multiplexer),

        ObservedProbe::SgrReadback => read_sgr(answers.sgr.as_ref()),

        ObservedProbe::CellSize => read_cell_size(answers.cell_size),

        ObservedProbe::Identity => read_identity(answers.identity.as_ref()),

        ObservedProbe::SttyErase => match &answers.erase {
            None | Some(Erase::NotReported) => {
                Reading::silent(origin, "`stty -a` reported no erase character")
            }
            Some(Erase::Found { canonical, raw }) => {
                Reading::new(origin, Claim::value(canonical.clone()))
                    .with_raw(raw.clone())
                    .with_detail(format!(
                        "the line discipline erases on {}",
                        observe::erase_explanation(canonical)
                    ))
            }
            Some(Erase::Unrecognised(raw)) => Reading::silent(
                origin,
                format!("`stty -a` reported erase = {raw}, which is neither convention"),
            )
            .with_raw(raw.clone()),
            Some(Erase::Unavailable(why)) => Reading::silent(origin, why.clone()),
        },
    }
}

/// The DECRQM arm of [`read_observed`].
fn read_decrqm(mode: Option<u32>, answers: &Answers, multiplexer: Option<Multiplexer>) -> Reading {
    let origin = Origin::Observed;
    let Some(mode) = mode else {
        return Reading::silent(origin, "no mode to ask about");
    };
    let Some(&state) = answers.modes.get(&mode) else {
        // The barrier came back with no report in front of it, so the terminal
        // had its chance. That is a statement about DECRQM, not about the mode.
        if answers.decrqm_barrier && multiplexer.is_none() {
            return Reading::silent(
                origin,
                format!(
                    "the device-attributes barrier came back with no DECRPM report in front of it, \
                     so this terminal does not implement DECRQM for mode {mode}. That is not the \
                     same as the mode being unsupported — Konsole implements no DECRQM at all \
                     while supporting the modes."
                ),
            );
        }
        return Reading::silent(origin, "DECRQM was not answered");
    };
    let reading = Reading::new(origin, Claim::from_support(state.support()))
        .with_raw(format!("CSI ? {mode} ; {} $ y", state.value()))
        .with_detail(format!("DECRQM reports mode {mode} {}", state.label()));
    if answers.sentinel.informative(state) {
        reading
    } else {
        reading.unreliable(format!(
            "DECRQM reports mode {mode} {} — but this terminal gives the same answer for a mode \
             that cannot exist, so it means nothing here",
            state.label()
        ))
    }
}

/// The DECRQSS-readback arm of [`read_observed`].
fn read_sgr(readback: Option<&observe::SgrReadback>) -> Reading {
    let origin = Origin::Observed;
    let Some(readback) = readback else {
        return Reading::silent(origin, "the terminal was not reached");
    };
    let Some(colour) = readback.colour else {
        return Reading::silent(
            origin,
            "DECRQSS was not answered, so the graphic rendition could not be read back",
        );
    };
    let (red, green, blue) = observe::PROBE_RGB;
    let raw = readback
        .raw
        .as_deref()
        .map(observe::printable)
        .unwrap_or_default();
    if readback.kept_the_triple() {
        return Reading::new(origin, Claim::from_support(Support::Yes))
            .with_raw(raw)
            .with_detail(format!(
                "set {red};{green};{blue} and the terminal reported the same triple back, so the \
                 24 bits survived"
            ));
    }
    let what = match colour {
        observe::Readback::Direct(r, g, b) => format!("a different triple, {r};{g};{b}"),
        observe::Readback::Indexed(index) => format!("palette index {index} — it quantised"),
        observe::Readback::NoColor => {
            "no foreground colour at all — it dropped the attribute".to_owned()
        }
    };
    Reading::new(origin, Claim::from_support(Support::No))
        .with_raw(raw)
        .with_detail(format!(
            "set {red};{green};{blue} and the terminal reported {what}"
        ))
}

/// The cell-size arm of [`read_observed`].
fn read_cell_size(answer: CellSizeAnswer) -> Reading {
    let origin = Origin::Observed;
    match answer {
        CellSizeAnswer::NotProbed => Reading::silent(origin, "the terminal was not reached"),
        CellSizeAnswer::NoAnswer => Reading::silent(
            origin,
            "none of CSI 16 t, OSC 1337 ReportCellSize or CSI 14 t with CSI 18 t was answered",
        ),
        CellSizeAnswer::Measured(size) => Reading::new(origin, Claim::from_support(Support::Yes))
            .with_raw(format!("{} px", size.label()))
            .with_detail(format!(
                "{} pixels per cell, from {} — which rung answered identifies the terminal family",
                size.label(),
                size.rung.label()
            )),
    }
}

/// The XTVERSION arm of [`read_observed`].
fn read_identity(identity: Option<&observe::Identity>) -> Reading {
    let origin = Origin::Observed;
    let Some(identity) = identity else {
        return Reading::silent(origin, "the terminal was not reached");
    };
    let detail = observe::identity_detail(identity);
    let Some(family) = observe::observed_family(identity) else {
        let why = match &identity.xtversion {
            Some(version) => format!(
                "XTVERSION answered `{}`, which is not a name this recognises",
                observe::printable(version)
            ),
            None => "XTVERSION was not answered, so the terminal did not name itself".to_owned(),
        };
        let reading = Reading::silent(origin, why);
        return match detail {
            Some(detail) => reading.with_raw(detail),
            None => reading,
        };
    };
    let mut reading = Reading::new(origin, Claim::value(family));
    if let Some(version) = &identity.xtversion {
        reading = reading.with_raw(observe::printable(version));
    }
    match detail {
        Some(detail) => reading.with_detail(detail),
        None => reading,
    }
}

/// The fourth source: what the environment asserts.
fn read_environment(check: &Check) -> Reading {
    let origin = Origin::Environment;
    if check.env.is_empty() {
        return Reading::not_applicable(origin, "no environment variable asserts this");
    }
    for name in &check.env {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        let raw = format!("{name}={}", observe::printable(&value));
        if check.kind == CheckKind::Identity {
            return match identity::family(&value) {
                Some(family) => Reading::new(origin, Claim::value(family)).with_raw(raw),
                None => Reading::silent(
                    origin,
                    format!(
                        "`{}` is not a name this recognises",
                        observe::printable(&value)
                    ),
                )
                .with_raw(raw),
            };
        }
        return Reading::new(origin, Claim::from_support(Support::Yes)).with_raw(raw);
    }
    // Absence of an environment variable is not a negative claim: nothing sets
    // COLORTERM on a terminal that merely forgot to.
    Reading::silent(
        origin,
        format!(
            "none of {} is set, and an unset variable asserts nothing",
            check.env.join(", ")
        ),
    )
}

/// Canonicalises a `value` check's answer.
///
/// Control-byte spellings are canonicalised to `^X` form, because `kbs` and `stty`
/// spell the same byte differently and comparing spellings would report a
/// disagreement between two sources that agree. Anything else is compared as the
/// text it is, made safe to print first.
#[must_use]
pub fn normalise_value(text: &str) -> String {
    observe::canonical_erase(text).map_or_else(|| observe::printable(text.trim()), str::to_owned)
}

/// `RGB`, `colors=0x100` — one capability and its value, for a raw column.
fn render_cap(name: &str, value: Option<&str>) -> String {
    match value {
        Some(value) if !value.is_empty() => {
            format!("{name}={}", observe::printable(value))
        }
        _ => name.to_owned(),
    }
}

/// `RGB, Tc or colors>=16777216` — the capabilities a check looked for.
fn spec_list(specs: &[checks::CapSpec]) -> String {
    let names: Vec<String> = specs
        .iter()
        .map(|spec| match spec.min {
            Some(min) => format!("{}>={min}", spec.name),
            None => spec.name.clone(),
        })
        .collect();
    match names.split_last() {
        None => "capability".to_owned(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::input::Reply;

    fn catalogue() -> Catalogue {
        Catalogue::load().unwrap()
    }

    fn check(id: &str) -> Check {
        catalogue()
            .checks()
            .iter()
            .find(|check| check.id == id)
            .unwrap()
            .clone()
    }

    fn entry(text: &str) -> terminfo::Source {
        terminfo::Source::Entry {
            entry: Box::new(terminfo::parse(text)),
            queried: "test".to_owned(),
        }
    }

    fn diagnosis(rows: Vec<Row<'_>>, heard: bool) -> Diagnosis<'_> {
        Diagnosis {
            rows,
            terminfo: terminfo::Source::NoTerm,
            channel: None,
            no_channel: None,
            multiplexer: None,
            sentinel: Sentinel::default(),
            heard_anything: heard,
            decrqm_not_implemented: false,
            timeout: Duration::from_secs(1),
            term: None,
            term_program: None,
        }
    }

    fn row_with(check: &Check, readings: Vec<Reading>) -> Row<'_> {
        let agreement = agreement(&readings);
        Row {
            check,
            readings,
            agreement,
        }
    }

    // -- exit codes ----------------------------------------------------------

    #[test]
    fn a_terminal_that_could_not_be_asked_exits_two() {
        let check = check("truecolor");
        // Even with terminfo and the environment disagreeing, 2 dominates: two of
        // the four sources are missing, and a script that got 1 would believe the
        // terminal had been examined.
        let row = row_with(
            &check,
            vec![
                Reading::new(Origin::Terminfo, Claim::from_support(Support::No)),
                Reading::new(Origin::Environment, Claim::from_support(Support::Yes)),
            ],
        );
        let diagnosis = diagnosis(vec![row], false);
        assert_eq!(diagnosis.disagreements(), 1);
        assert_eq!(
            format!("{:?}", diagnosis.exit_code()),
            format!("{:?}", ExitCode::from(2))
        );
        assert!(!diagnosis.probed());
    }

    #[test]
    fn a_disagreement_exits_one() {
        let check = check("truecolor");
        let row = row_with(
            &check,
            vec![
                Reading::new(Origin::Terminfo, Claim::from_support(Support::No)),
                Reading::new(Origin::Observed, Claim::from_support(Support::Yes)),
            ],
        );
        let diagnosis = diagnosis(vec![row], true);
        assert_eq!(
            format!("{:?}", diagnosis.exit_code()),
            format!("{:?}", ExitCode::from(1))
        );
    }

    #[test]
    fn agreement_exits_zero() {
        let check = check("truecolor");
        let row = row_with(
            &check,
            vec![
                Reading::new(Origin::Terminfo, Claim::from_support(Support::Yes)),
                Reading::new(Origin::Observed, Claim::from_support(Support::Yes)),
            ],
        );
        let diagnosis = diagnosis(vec![row], true);
        assert_eq!(
            format!("{:?}", diagnosis.exit_code()),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert_eq!(diagnosis.disagreements(), 0);
    }

    #[test]
    fn rows_with_nothing_to_compare_do_not_change_the_exit_code() {
        let check = check("cell-size");
        let row = row_with(
            &check,
            vec![Reading::silent(Origin::Observed, "nothing answered")],
        );
        let diagnosis = diagnosis(vec![row], true);
        assert_eq!(diagnosis.incomparable(), 1);
        assert_eq!(
            format!("{:?}", diagnosis.exit_code()),
            format!("{:?}", ExitCode::SUCCESS)
        );
    }

    // -- terminfo readings ---------------------------------------------------

    #[test]
    fn terminfo_without_rgb_or_tc_is_a_definite_no_about_truecolor() {
        // The disagreement that motivates the whole subcommand: this entry is
        // real, it is what `xterm-256color` looks like, and COLORTERM says
        // truecolor anyway.
        let source = entry("xterm-256color|xterm with 256 colors,\n\tcolors#0x100,\n\tkbs=^?,\n");
        let reading = read_terminfo(&check("truecolor"), &source);
        assert_eq!(reading.claim.says(), Some("no"));
        let detail = reading.detail.unwrap();
        assert!(detail.contains("RGB"), "{detail}");
        assert!(detail.contains("exhaustively"), "{detail}");
    }

    #[test]
    fn a_large_colors_value_satisfies_the_threshold_where_256_does_not() {
        let direct = entry("foo|Foo,\n\tcolors#0x1000000,\n");
        assert_eq!(
            read_terminfo(&check("truecolor"), &direct).claim.says(),
            Some("yes")
        );
        let indexed = entry("foo|Foo,\n\tcolors#0x100,\n");
        assert_eq!(
            read_terminfo(&check("truecolor"), &indexed).claim.says(),
            Some("no")
        );
    }

    #[test]
    fn a_missing_terminfo_entry_is_silence_rather_than_a_no() {
        // `infocmp` not being installed says nothing about the terminal, and
        // reading it as a no would put it in disagreement with every other source.
        let reading = read_terminfo(&check("truecolor"), &terminfo::Source::NoInfocmp);
        assert_eq!(reading.claim, Claim::Silent);
        assert!(reading.detail.unwrap().contains("infocmp"));
    }

    #[test]
    fn a_check_with_no_terminfo_capability_is_not_applicable_rather_than_no() {
        // Cell size: terminfo has no capability for pixel geometry at all.
        let source = entry("foo|Foo,\n\tam,\n");
        let reading = read_terminfo(&check("cell-size"), &source);
        assert_eq!(reading.claim, Claim::NotApplicable);
    }

    #[test]
    fn the_backspace_check_compares_the_erase_byte_not_its_spelling() {
        let source = entry("foo|Foo,\n\tkbs=^?,\n");
        let reading = read_terminfo(&check("backspace"), &source);
        assert_eq!(reading.claim.says(), Some("^?"));

        let old = entry("foo|Foo,\n\tkbs=\\177,\n");
        assert_eq!(
            read_terminfo(&check("backspace"), &old).claim.says(),
            Some("^?"),
            "a different spelling of the same byte must not read as a disagreement"
        );

        let bs = entry("foo|Foo,\n\tkbs=^H,\n");
        assert_eq!(
            read_terminfo(&check("backspace"), &bs).claim.says(),
            Some("^H")
        );
    }

    #[test]
    fn identity_reads_the_entry_name_as_a_family() {
        let source = entry("xterm-kitty|KovIdTTY,\n\tam,\n");
        let reading = read_terminfo(&check("identity"), &source);
        assert_eq!(reading.claim.says(), Some("kitty"));
        assert_eq!(reading.raw.as_deref(), Some("xterm-kitty"));
    }

    #[test]
    fn an_unrecognised_entry_name_is_silence_rather_than_a_guess() {
        let source = entry("brand-new-terminal|New,\n\tam,\n");
        let reading = read_terminfo(&check("identity"), &source);
        assert_eq!(reading.claim, Claim::Silent);
        assert_eq!(reading.raw.as_deref(), Some("brand-new-terminal"));
    }

    // -- XTGETTCAP readings --------------------------------------------------

    fn answers_with_tcap(tcap: BTreeMap<String, TcapAnswer>) -> Answers {
        Answers {
            tcap,
            tcap_round: Round {
                replies: Vec::new(),
                answered_barrier: true,
            },
            ..Answers::default()
        }
    }

    #[test]
    fn a_disowned_capability_is_silence_for_a_value_check_not_the_word_no() {
        // Comparing the word `no` against an erase character or a terminal name
        // would report a disagreement that is purely an artefact of the model.
        let mut tcap = BTreeMap::new();
        tcap.insert("kbs".to_owned(), TcapAnswer::Absent);
        let reading = read_terminal(&check("backspace"), &answers_with_tcap(tcap));
        assert_eq!(reading.claim, Claim::Silent);
        assert_eq!(reading.comparable(), None);
        assert!(reading.raw.unwrap().contains("DCS 0 + r"));

        let mut tcap = BTreeMap::new();
        tcap.insert("TN".to_owned(), TcapAnswer::Absent);
        assert_eq!(
            read_terminal(&check("identity"), &answers_with_tcap(tcap)).claim,
            Claim::Silent
        );
    }

    #[test]
    fn a_terminfo_entry_with_no_value_is_silent_rather_than_saying_no() {
        // Same reasoning on the terminfo side: `kbs` absent means no value, not
        // the value "no".
        let source = entry("foo|Foo,\n\tam,\n");
        let reading = read_terminfo(&check("backspace"), &source);
        assert_eq!(reading.claim, Claim::Silent);
        assert!(reading.detail.unwrap().contains("no value to compare"));
    }

    #[test]
    fn a_terminal_that_disowns_a_capability_says_no() {
        let mut tcap = BTreeMap::new();
        tcap.insert("RGB".to_owned(), TcapAnswer::Absent);
        tcap.insert("Tc".to_owned(), TcapAnswer::Absent);
        tcap.insert("Co".to_owned(), TcapAnswer::Absent);
        let reading = read_terminal(&check("truecolor"), &answers_with_tcap(tcap));
        assert_eq!(reading.claim.says(), Some("no"));
        assert!(reading.detail.unwrap().contains("disowned"));
    }

    #[test]
    fn a_terminal_that_never_answered_xtgettcap_is_silent_not_negative() {
        // Most terminals do not implement XTGETTCAP. Reading that as a no would
        // put them in disagreement with their own terminfo entry everywhere.
        let reading = read_terminal(&check("truecolor"), &answers_with_tcap(BTreeMap::new()));
        assert_eq!(reading.claim, Claim::Silent);
        assert!(reading.detail.unwrap().contains("XTGETTCAP"));
    }

    #[test]
    fn a_capability_present_but_below_the_threshold_is_an_explicit_no() {
        let mut tcap = BTreeMap::new();
        tcap.insert("Co".to_owned(), TcapAnswer::Present(Some("256".to_owned())));
        let reading = read_terminal(&check("truecolor"), &answers_with_tcap(tcap));
        assert_eq!(reading.claim.says(), Some("no"));
        assert!(reading.raw.unwrap().contains("256"));
    }

    #[test]
    fn a_present_capability_is_a_yes_whatever_its_value() {
        let mut tcap = BTreeMap::new();
        tcap.insert("Tc".to_owned(), TcapAnswer::Present(None));
        let reading = read_terminal(&check("truecolor"), &answers_with_tcap(tcap));
        assert_eq!(reading.claim.says(), Some("yes"));
        assert_eq!(reading.raw.as_deref(), Some("Tc"));
    }

    #[test]
    fn a_check_with_no_names_to_ask_for_is_not_applicable() {
        let reading = read_terminal(&check("cell-size"), &answers_with_tcap(BTreeMap::new()));
        assert_eq!(reading.claim, Claim::NotApplicable);
    }

    // -- matching XTGETTCAP replies to names ---------------------------------

    #[test]
    fn successful_replies_are_matched_by_their_own_name() {
        let queried = vec!["RGB".to_owned(), "Tc".to_owned()];
        let replies = vec![
            Reply::Dcs(format!("1+r{}=38", tcap::encode("RGB"))),
            Reply::Dcs(format!("1+r{}=31", tcap::encode("Tc"))),
        ];
        let answers = collect_tcap(&queried, &replies);
        assert_eq!(
            answers.get("RGB"),
            Some(&TcapAnswer::Present(Some("8".to_owned())))
        );
        assert_eq!(
            answers.get("Tc"),
            Some(&TcapAnswer::Present(Some("1".to_owned())))
        );
    }

    #[test]
    fn anonymous_failures_are_assigned_in_the_order_asked() {
        // `DCS 0 + r ST` carries no name, so the only reading available is that
        // terminals answer in order.
        let queried = vec!["RGB".to_owned(), "Tc".to_owned(), "Ms".to_owned()];
        let replies = vec![
            Reply::Dcs("0+r".to_owned()),
            Reply::Dcs("0+r".to_owned()),
            Reply::Dcs(format!("1+r{}=1b5d3532", tcap::encode("Ms"))),
        ];
        let answers = collect_tcap(&queried, &replies);
        assert_eq!(answers.get("RGB"), Some(&TcapAnswer::Absent));
        assert_eq!(answers.get("Tc"), Some(&TcapAnswer::Absent));
        assert!(matches!(answers.get("Ms"), Some(TcapAnswer::Present(_))));
    }

    #[test]
    fn a_named_failure_is_matched_by_name_rather_than_position() {
        let queried = vec!["RGB".to_owned(), "Tc".to_owned()];
        let replies = vec![Reply::Dcs(format!("0+r{}", tcap::encode("Tc")))];
        let answers = collect_tcap(&queried, &replies);
        assert_eq!(answers.get("Tc"), Some(&TcapAnswer::Absent));
        assert_eq!(answers.get("RGB"), None, "nothing was said about RGB");
    }

    #[test]
    fn more_anonymous_failures_than_names_does_not_panic() {
        let queried = vec!["RGB".to_owned()];
        let replies = vec![
            Reply::Dcs("0+r".to_owned()),
            Reply::Dcs("0+r".to_owned()),
            Reply::Dcs("0+r".to_owned()),
        ];
        let answers = collect_tcap(&queried, &replies);
        assert_eq!(answers.len(), 1);
    }

    #[test]
    fn replies_that_are_not_xtgettcap_are_ignored() {
        let queried = vec!["RGB".to_owned()];
        let replies = vec![
            Reply::Dcs("1$r0;38;2;1;2;3m".to_owned()),
            Reply::Dcs(">|kitty 0.35.2".to_owned()),
            Reply::Csi {
                params: "?62;4".to_owned(),
                final_byte: 'c',
            },
        ];
        assert!(collect_tcap(&queried, &replies).is_empty());
    }

    // -- observed readings ---------------------------------------------------

    #[test]
    fn a_decrqm_answer_of_two_means_the_mode_is_implemented() {
        let mut answers = Answers {
            decrqm_barrier: true,
            ..Answers::default()
        };
        answers.modes.insert(2004, ModeState::Reset);
        answers.sentinel = Sentinel {
            state: None,
            answered_barrier: true,
        };
        let reading = read_observed(&check("bracketed-paste"), &answers, None);
        assert_eq!(reading.claim.says(), Some("yes"));
        assert!(reading.reliable);
    }

    #[test]
    fn a_decrqm_answer_matching_the_sentinel_is_kept_but_marked_unreliable() {
        // iTerm2: `4` for a mode that cannot exist, so `4` for a real mode is the
        // same non-answer.
        let mut answers = Answers {
            decrqm_barrier: true,
            sentinel: Sentinel {
                state: Some(ModeState::PermanentlyReset),
                answered_barrier: true,
            },
            ..Answers::default()
        };
        answers.modes.insert(2026, ModeState::PermanentlyReset);
        let reading = read_observed(&check("synchronized-output"), &answers, None);
        assert!(!reading.reliable);
        assert_eq!(reading.comparable(), None, "it must not be compared");
        assert!(reading.detail.unwrap().contains("cannot exist"));
    }

    #[test]
    fn a_different_value_from_a_compromised_terminal_is_still_reliable() {
        let mut answers = Answers {
            decrqm_barrier: true,
            sentinel: Sentinel {
                state: Some(ModeState::PermanentlyReset),
                answered_barrier: true,
            },
            ..Answers::default()
        };
        answers.modes.insert(2026, ModeState::Reset);
        let reading = read_observed(&check("synchronized-output"), &answers, None);
        assert!(reading.reliable);
        assert_eq!(reading.comparable(), Some("yes"));
    }

    #[test]
    fn silent_decrqm_behind_an_answered_barrier_names_the_konsole_case() {
        let answers = Answers {
            decrqm_barrier: true,
            sentinel: Sentinel {
                state: None,
                answered_barrier: true,
            },
            ..Answers::default()
        };
        let reading = read_observed(&check("synchronized-output"), &answers, None);
        assert_eq!(reading.claim, Claim::Silent);
        let detail = reading.detail.unwrap();
        assert!(detail.contains("does not implement DECRQM"), "{detail}");
        assert!(detail.contains("Konsole"), "{detail}");
    }

    #[test]
    fn inside_a_multiplexer_the_barrier_licenses_no_such_conclusion() {
        // tmux answers the barrier from its own emulator, so it proves nothing
        // about what is beyond it.
        let answers = Answers {
            decrqm_barrier: true,
            sentinel: Sentinel {
                state: None,
                answered_barrier: true,
            },
            ..Answers::default()
        };
        let reading = read_observed(
            &check("synchronized-output"),
            &answers,
            Some(Multiplexer::Tmux),
        );
        assert_eq!(reading.claim, Claim::Silent);
        assert!(
            !reading
                .detail
                .unwrap()
                .contains("does not implement DECRQM")
        );
    }

    #[test]
    fn the_sgr_readback_reports_a_quantising_terminal_as_a_no() {
        let answers = Answers {
            sgr: Some(observe::SgrReadback {
                raw: Some("0;38;5;16".to_owned()),
                advisory_flag: Some(1),
                colour: Some(observe::Readback::Indexed(16)),
            }),
            ..Answers::default()
        };
        let reading = read_observed(&check("truecolor"), &answers, None);
        assert_eq!(reading.claim.says(), Some("no"));
        assert!(reading.detail.unwrap().contains("quantised"));
    }

    #[test]
    fn the_sgr_readback_reports_a_surviving_triple_as_a_yes() {
        let answers = Answers {
            sgr: Some(observe::SgrReadback {
                raw: Some("0;38;2;1;2;3".to_owned()),
                advisory_flag: Some(0),
                colour: Some(observe::Readback::Direct(1, 2, 3)),
            }),
            ..Answers::default()
        };
        let reading = read_observed(&check("truecolor"), &answers, None);
        assert_eq!(
            reading.claim.says(),
            Some("yes"),
            "the advisory flag being 0 must not change the verdict"
        );
    }

    #[test]
    fn the_cell_size_row_names_the_rung_that_answered() {
        let answers = Answers {
            cell_size: CellSizeAnswer::Measured(observe::CellSize {
                width: 8.0,
                height: 16.0,
                rung: observe::Rung::Divided,
            }),
            ..Answers::default()
        };
        let reading = read_observed(&check("cell-size"), &answers, None);
        assert_eq!(reading.claim.says(), Some("yes"));
        assert!(reading.detail.unwrap().contains("CSI 14 t / CSI 18 t"));
    }

    #[test]
    fn a_check_with_no_observed_test_is_not_applicable() {
        let reading = read_observed(&check("clipboard"), &Answers::default(), None);
        assert_eq!(reading.claim, Claim::NotApplicable);
    }

    #[test]
    fn the_erase_row_uses_the_line_discipline() {
        let answers = Answers {
            erase: Some(Erase::Found {
                canonical: "^?".to_owned(),
                raw: "^?".to_owned(),
            }),
            ..Answers::default()
        };
        let reading = read_observed(&check("backspace"), &answers, None);
        assert_eq!(reading.claim.says(), Some("^?"));
        assert!(reading.detail.unwrap().contains("0x7f"));
    }

    // -- environment readings ------------------------------------------------

    #[test]
    fn a_check_with_no_environment_variable_is_not_applicable() {
        assert_eq!(
            read_environment(&check("cell-size")).claim,
            Claim::NotApplicable
        );
    }

    // -- helpers -------------------------------------------------------------

    #[test]
    fn the_capability_list_reads_as_a_sentence() {
        let check = check("truecolor");
        let list = spec_list(&check.terminfo);
        assert!(list.contains("RGB"), "{list}");
        assert!(list.contains(" or "), "{list}");
        assert!(list.contains("colors>=16777216"), "{list}");
    }

    #[test]
    fn a_single_capability_needs_no_conjunction() {
        let check = check("clipboard");
        assert_eq!(spec_list(&check.terminfo), "Ms");
    }

    #[test]
    fn normalising_a_value_canonicalises_control_bytes_and_sanitises_the_rest() {
        assert_eq!(normalise_value("\\177"), "^?");
        assert_eq!(normalise_value("^H"), "^H");
        assert_eq!(normalise_value("plain"), "plain");
        assert!(!normalise_value("\u{1b}[m").contains('\u{1b}'));
    }

    #[test]
    fn a_raw_capability_cell_never_carries_a_control_byte() {
        // `Ms` decodes to a value that starts with a real escape byte.
        let rendered = render_cap("Ms", Some("\u{1b}]52;%p1%s\u{7}"));
        assert!(!rendered.contains('\u{1b}'), "{rendered}");
        assert_eq!(render_cap("Tc", None), "Tc");
        assert_eq!(render_cap("Tc", Some("")), "Tc");
    }

    #[test]
    fn every_check_asks_for_exactly_the_names_the_round_will_query() {
        let catalogue = catalogue();
        let selected: Vec<&Check> = catalogue.checks().iter().collect();
        let names = tcap_names(&selected);
        // No duplicates, and every check's names are covered.
        let mut sorted = names.clone();
        sorted.sort();
        let total = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), total, "a name is queried twice");
        for check in &selected {
            for spec in &check.xtgettcap {
                assert!(names.contains(&spec.name), "{} missing", spec.name);
            }
        }
    }

    #[test]
    fn the_sentinel_mode_is_never_one_of_the_checks() {
        let catalogue = catalogue();
        let selected: Vec<&Check> = catalogue.checks().iter().collect();
        let modes = decrqm_modes(&selected);
        assert!(!modes.contains(&decrqm::SENTINEL_MODE));
        assert!(!modes.is_empty(), "some check should use DECRQM");
        let mut sorted = modes.clone();
        sorted.sort_unstable();
        let total = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), total, "a mode is queried twice");
    }

    #[test]
    fn only_selects_one_check_by_prefix() {
        let catalogue = catalogue();
        let selected = select(&catalogue, Some("true")).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "truecolor");
    }

    #[test]
    fn only_is_case_insensitive_and_exact_matches_win() {
        let catalogue = catalogue();
        assert_eq!(
            select(&catalogue, Some("IDENTITY")).unwrap()[0].id,
            "identity"
        );
        assert_eq!(select(&catalogue, Some("mouse")).unwrap()[0].id, "mouse");
    }

    #[test]
    fn an_unknown_only_lists_the_real_names() {
        let catalogue = catalogue();
        let error = select(&catalogue, Some("nonsense")).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("truecolor"), "{message}");
        assert!(message.contains("data/checks.toml"), "{message}");
    }

    #[test]
    fn without_only_every_check_is_selected() {
        let catalogue = catalogue();
        assert_eq!(
            select(&catalogue, None).unwrap().len(),
            catalogue.checks().len()
        );
    }

    #[test]
    fn building_a_row_for_every_check_with_no_answers_never_panics() {
        // The non-terminal path: nothing was heard, and every row still has to
        // produce four readings and an honest agreement.
        let catalogue = catalogue();
        let answers = Answers::default();
        for check in catalogue.checks() {
            let row = build_row(check, &terminfo::Source::NoInfocmp, &answers, None);
            assert_eq!(row.readings.len(), 4);
            for origin in Origin::ALL {
                assert!(
                    row.reading(origin).is_some(),
                    "{} missing {origin:?}",
                    check.id
                );
            }
            assert_ne!(
                row.agreement,
                Agreement::Disagree,
                "{} cannot disagree with nothing",
                check.id
            );
        }
    }
}
