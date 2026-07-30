//! Per-attribute capability detection: one honest verdict for each attribute asked
//! about, and a stated reason for every one of them.
//!
//! # There is a real measurement here
//!
//! Most capability probing in this crate is inference. This one is not, on any terminal
//! that implements **DECRQSS** — "Request Selection or Setting". The trick is that the
//! terminal will tell you its own state if you ask:
//!
//! 1. Reset, then set the attribute: `CSI 0 m` `CSI 4:3 m`.
//! 2. Ask what the attributes now are: `DCS $ q m ST`.
//! 3. The terminal answers `DCS 1 $ r <the SGR it would emit> m ST`.
//!
//! Read the answer and you know what the terminal *did* with what you sent. A terminal
//! that parsed the sub-parameter reports `4:3`; one that flattened it reports `4`; one
//! that never understood the parameter reports neither. The same trick catches the
//! nastiest failure in the whole family: set `38;2;1;2;3` and a terminal without 24-bit
//! colour reports back `38;5;16`, because it *accepted* your colour and quantised it.
//! Nothing else can see that, and it is why the probe colour is `#010203` — small
//! components, none equal, so no rounding can return it unchanged.
//!
//! Nothing is printed while this happens, because setting an attribute changes no cells.
//!
//! # What the measurement does and does not prove
//!
//! It measures **parsing and retention**, not rendering. A terminal could record `4:3`
//! and draw a straight line, or draw a beautiful curl and report a lossy `4`. Both are
//! unlikely and neither is impossible, so this is presented as strong evidence rather
//! than proof, and [`Basis`] labels every verdict with what it rests on. Plenty of
//! terminals do not implement DECRQSS for SGR at all; those fall back to terminal
//! identity (`TERM`, `TERM_PROGRAM`, the per-terminal environment markers, and XTVERSION
//! `CSI > q`) cross-referenced against [`crate::features::style::supports`], and say so.
//!
//! Everything is paired with a Primary Device Attributes barrier (`CSI c`), which every
//! terminal since the VT100 answers, so silence can be told apart from slowness. **A
//! multiplexer answers both device attributes and DECRQSS from its own emulator**, so
//! from inside one nothing measured here says anything about the terminal beyond it —
//! see [`ProbeOutcome::finding`].
//!
//! # `NO_COLOR` decides the verdict for a colour, and says so
//!
//! `escapade style probe --color '#ff0000'` is asked by a script deciding whether to
//! colour its own output, so a user preference that says "no colour" is a real answer to
//! it and the verdict honours that. What is *not* acceptable is losing the distinction,
//! so the capability is measured anyway and reported alongside — see
//! [`Finding::capability`]. "Your terminal does 24-bit colour perfectly well; your own
//! environment is switching it off" is the answer for the reader this most often bites.

use std::{
    env,
    fmt::Write as _,
    io::{IsTerminal as _, stdin, stdout},
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    cli::{ProbeFormat, StyleAttrs},
    db::{Catalog, SupportStatus},
    features::{
        Support,
        style::{
            FEATURE_ID,
            attrs::{Attr, Rendition, UnderlineShape, every_attribute},
            preference::{ColorPreference, NO_COLOR},
            sanitize_text, supports,
        },
        wrap,
    },
    input::{InputReader, Reply},
    term::{CSI, DCS, Multiplexer, ST, Term, pretty},
    theme::{Ink, Theme},
};

/// How long to wait for the terminal to answer.
///
/// A full second: generous enough for a slow ssh link or a loaded machine, and this probe
/// is not on anybody's hot path. Under the barrier the wait almost always ends early, as
/// soon as the device-attributes reply arrives.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to spend consuming replies still in flight after a conclusion is reached.
///
/// Every reply this probe provokes must be read, because an unread one is delivered to
/// whatever runs next and printed as garbage. This probe can provoke twenty of them.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------------------
// The queries
// ---------------------------------------------------------------------------

/// XTVERSION: `CSI > q`. A terminal that implements it answers
/// `DCS > | <name and version> ST`; one that does not stays quiet.
#[must_use]
pub fn version_query() -> String {
    format!("{CSI}>q")
}

/// DECRQSS for Select Graphic Rendition: `DCS $ q m ST`.
///
/// The terminal answers with the SGR sequence it would have to emit to reproduce its
/// current attributes. That is what turns "did you understand `4:3`?" into a question
/// with an observable answer.
#[must_use]
pub fn sgr_query() -> String {
    format!("{DCS}$qm{ST}")
}

/// Primary Device Attributes: `CSI c`. The barrier — every terminal since the VT100
/// answers it, so its reply proves the round trip completed.
#[must_use]
pub fn barrier_query() -> String {
    format!("{CSI}c")
}

/// `CSI 0 m` — a clean slate before each measurement, and after the last one.
#[must_use]
pub fn reset_all() -> String {
    format!("{CSI}0m")
}

/// Which attributes to probe, given the flags.
///
/// With no attribute flags at all, the whole family. With flags, exactly what was asked
/// for — and for a colour, **the form the user wrote decides what is measured**:
/// `--color red` probes `SGR 31`, `--color 173` probes the 256-colour form, and
/// `--color '#010203'` probes 24-bit. Those are three different questions and a terminal
/// can answer them differently, so conflating them would make the command useless for
/// the case that matters most.
///
/// # Errors
///
/// Propagates colour parsing, so a mistyped colour is reported rather than probed.
pub fn subjects(flags: &StyleAttrs) -> Result<Vec<Attr>> {
    // Built with colour allowed, whatever `NO_COLOR` says: the preference decides the
    // verdict for a colour, not whether it is a legitimate thing to ask about.
    let (rendition, _) = Rendition::from_flags(flags, &ColorPreference::allowed())?;
    if rendition.is_empty() {
        return Ok(every_attribute());
    }
    Ok(rendition.attrs().to_vec())
}

/// The whole measurement, pipelined into one write.
///
/// For each subject: reset, set the attribute, ask what the attributes are. The terminal
/// processes input in order and answers in order, so the replies come back in the same
/// sequence as the plan and are matched up positionally. One round trip for all of it,
/// and a final `CSI 0 m` so nothing is left set.
#[must_use]
pub fn measurement_sequence(plan: &[Attr]) -> String {
    let mut out = String::with_capacity(plan.len() * 40 + 8);
    for attr in plan {
        out.push_str(&reset_all());
        let _ = write!(out, "{CSI}{}m", attr.params());
        out.push_str(&sgr_query());
    }
    out.push_str(&reset_all());
    out
}

// ---------------------------------------------------------------------------
// Reading the answers
// ---------------------------------------------------------------------------

/// A terminal's answer to [`sgr_query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SgrReport {
    /// `DCS 1 $ r <params> m ST` — the attributes, as the terminal would emit them.
    Reported(String),
    /// `DCS 0 $ r ST` — the terminal understands DECRQSS but declines to answer for this
    /// setting. An answer, and not a useful one.
    Refused,
}

/// Parses a DCS payload as an answer to [`sgr_query`].
///
/// [`crate::input`] hands over the bytes between `ESC P` and the terminator, so the `1$r`
/// is still on the front and the `m` still on the back. Returns `None` for anything that
/// is not a DECRQSS answer about SGR — an XTVERSION reply (`>|kitty 0.36.0`) arrives on
/// the same channel and must not be mistaken for one.
///
/// Note that the verdict never rests on the leading flag alone. DEC's own manuals
/// disagree with practice about which of `0` and `1` means valid, so what actually
/// matters is whether a payload came back at all — a distinction this type preserves by
/// keeping [`SgrReport::Refused`] separate from `None` rather than collapsing both.
#[must_use]
pub fn parse_sgr_report(payload: &str) -> Option<SgrReport> {
    if let Some(rest) = payload.strip_prefix("0$r") {
        // A refusal carries no selector, but be tolerant of one.
        if rest.is_empty() || rest.ends_with('m') {
            return Some(SgrReport::Refused);
        }
        return None;
    }
    let params = payload.strip_prefix("1$r")?.strip_suffix('m')?;
    Some(SgrReport::Reported(params.to_owned()))
}

/// Strips XTVERSION's `>|` prefix from a DCS payload.
#[must_use]
pub fn parse_xtversion(payload: &str) -> Option<String> {
    let trimmed = payload.strip_prefix(">|")?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// One subject, and what the terminal said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measurement {
    /// What was set before asking.
    pub subject: Attr,
    /// The terminal's answer, when it gave one.
    pub report: Option<SgrReport>,
}

impl Measurement {
    /// Whether the terminal kept what was set: `Some(true)` yes, `Some(false)` no, `None`
    /// nothing observable was said.
    #[must_use]
    pub fn kept(&self) -> Option<bool> {
        match &self.report {
            Some(SgrReport::Reported(params)) => Some(self.subject.report_keeps(params)),
            // A refusal is an answer that carries no information about the attribute, so
            // it is not evidence either way.
            Some(SgrReport::Refused) | None => None,
        }
    }

    /// The reported parameter text, for the debug report.
    #[must_use]
    pub fn reported(&self) -> Option<&str> {
        match &self.report {
            Some(SgrReport::Reported(params)) => Some(params),
            Some(SgrReport::Refused) | None => None,
        }
    }

    /// What the terminal substituted instead, when it substituted something — a
    /// quantised colour being the case worth naming.
    #[must_use]
    pub fn substitute(&self) -> Option<String> {
        self.subject.report_substitute(self.reported()?)
    }
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// One thing the probe learned about who is at the other end, and what it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// Where it came from: `XTVERSION`, `TERM`, or an environment variable name.
    pub source: &'static str,
    /// What that source said.
    pub value: String,
    /// The registry id it identifies, when it identifies one.
    pub implies: Option<&'static str>,
    /// Whether it came from the terminal itself rather than from the environment.
    ///
    /// `TERM_PROGRAM` is set by whatever launched the shell and is inherited by every
    /// child, including one on the far side of an `ssh` hop; an XTVERSION reply is the
    /// terminal that is actually there, answering for itself.
    pub in_band: bool,
    /// Why it identifies nothing, when it identifies nothing.
    pub caveat: Option<&'static str>,
}

/// Environment variables that identify a terminal by their mere presence.
const ENV_MARKERS: &[(&str, &str)] = &[
    ("KITTY_WINDOW_ID", "kitty"),
    ("GHOSTTY_RESOURCES_DIR", "ghostty"),
    ("GHOSTTY_BIN_DIR", "ghostty"),
    ("WEZTERM_PANE", "wezterm"),
    ("WEZTERM_EXECUTABLE", "wezterm"),
    ("ALACRITTY_WINDOW_ID", "alacritty"),
    ("ALACRITTY_SOCKET", "alacritty"),
    ("KONSOLE_VERSION", "konsole"),
    ("ITERM_SESSION_ID", "iterm2"),
    ("CONTOUR_VERSION", "contour"),
    ("WT_SESSION", "windows-terminal"),
    // VTE exports its version, which is how every VTE-based terminal announces itself.
    // For this feature the version matters: VTE learned the underline shapes and SGR 58
    // in 0.52, long before most of the rest.
    ("VTE_VERSION", "gnome-terminal"),
];

/// Maps a terminal's self-reported name onto a registry id.
///
/// A case-insensitive substring search, most specific first, so `xterm-kitty` reaches
/// `kitty` before it reaches `xterm`.
#[must_use]
pub fn registry_id_for(name: &str) -> Option<&'static str> {
    /// `(needle, registry id)`, most specific first.
    const TABLE: &[(&str, &str)] = &[
        ("kitty", "kitty"),
        ("ghostty", "ghostty"),
        ("wezterm", "wezterm"),
        ("alacritty", "alacritty"),
        ("contour", "contour"),
        ("iterm", "iterm2"),
        ("konsole", "konsole"),
        ("vscode", "vscode"),
        ("windows terminal", "windows-terminal"),
        ("windowsterminal", "windows-terminal"),
        ("gnome", "gnome-terminal"),
        ("vte", "gnome-terminal"),
        ("foot", "foot"),
        ("rio", "rio"),
        // Multiplexers before xterm: `screen.xterm-256color` is Screen.
        ("tmux", "tmux"),
        ("zellij", "zellij"),
        ("screen", "screen"),
        ("xterm", "xterm"),
    ];
    let lower = name.to_ascii_lowercase();
    TABLE
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, id)| *id)
}

/// The generic `TERM` values that identify nothing.
///
/// `xterm-256color` is what almost every terminal sets for terminfo compatibility, so
/// reading it as "this is xterm" would report a confident no on terminals that draw curly
/// underlines perfectly well. That matters more here than anywhere: for this feature xterm
/// is the one that says no, which makes the mistake maximally misleading.
const GENERIC_TERMS: &[&str] = &[
    "xterm",
    "xterm-color",
    "xterm-16color",
    "xterm-88color",
    "xterm-256color",
    "xterm-direct",
    "xterm-new",
    "ansi",
    "vt100",
    "vt220",
    "linux",
    "dumb",
];

/// Identifies a terminal from `TERM`, refusing to be fooled by the generic values
/// everything sets.
#[must_use]
pub fn id_from_term(term: &str) -> Option<&'static str> {
    let lower = term.to_ascii_lowercase();
    if GENERIC_TERMS.contains(&lower.as_str()) {
        return None;
    }
    registry_id_for(&lower)
}

/// Pulls a dotted version number out of a terminal's self-description.
///
/// Terminals write themselves down in their own style — `kitty 0.36.0`, `foot(1.16.2)`,
/// `WezTerm 20240203-110809`, `XTerm(389)` — so this takes the first run of
/// digits-and-dots and splits it on the dots.
#[must_use]
pub fn parse_version(text: &str) -> Option<Vec<u64>> {
    let mut components: Vec<u64> = Vec::new();
    let mut current: Option<u64> = None;
    let mut started = false;
    for ch in text.chars() {
        if let Some(digit) = ch.to_digit(10) {
            started = true;
            current = Some(
                current
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(u64::from(digit)),
            );
        } else if ch == '.' && current.is_some() {
            components.push(current.take().unwrap_or(0));
        } else if started {
            break;
        }
    }
    if let Some(last) = current {
        components.push(last);
    }
    if components.is_empty() {
        None
    } else {
        Some(components)
    }
}

/// Compares two dotted versions, treating a missing component as zero.
///
/// `0.52` and `0.52.0` are the same version, and `0.10` is later than `0.9` — which is
/// why this is not a string comparison.
#[must_use]
pub fn version_at_least(found: &[u64], wanted: &[u64]) -> bool {
    for index in 0..found.len().max(wanted.len()) {
        let mine = found.get(index).copied().unwrap_or(0);
        let theirs = wanted.get(index).copied().unwrap_or(0);
        if mine != theirs {
            return mine > theirs;
        }
    }
    true
}

/// Gathers every environment-based clue, in order of trustworthiness.
///
/// Pure, so the identification logic is testable without a terminal. `xtversion` is
/// threaded in rather than read here because it takes a round trip to get.
#[must_use]
pub fn collect_evidence(xtversion: Option<&str>) -> Vec<Evidence> {
    let mut evidence = Vec::new();

    if let Some(name) = xtversion {
        let implies = registry_id_for(name);
        evidence.push(Evidence {
            source: "XTVERSION",
            value: name.to_owned(),
            implies,
            in_band: true,
            caveat: if implies.is_none() {
                Some("the terminal named itself, but not with a name escapade has a row for")
            } else {
                None
            },
        });
    }

    for (key, id) in ENV_MARKERS {
        if let Some(value) = env::var_os(key) {
            let shown = value.to_string_lossy().into_owned();
            evidence.push(Evidence {
                source: key,
                value: if shown.is_empty() {
                    "(set)".to_owned()
                } else {
                    shown
                },
                implies: Some(id),
                in_band: false,
                caveat: None,
            });
        }
    }

    if let Ok(program) = env::var("TERM_PROGRAM") {
        let implies = registry_id_for(&program);
        evidence.push(Evidence {
            source: "TERM_PROGRAM",
            value: program,
            implies,
            in_band: false,
            caveat: if implies.is_none() {
                Some("not a terminal escapade has a row for")
            } else {
                None
            },
        });
    }

    let term = env::var("TERM").unwrap_or_default();
    if !term.is_empty() {
        let implies = id_from_term(&term);
        evidence.push(Evidence {
            source: "TERM",
            value: term,
            implies,
            in_band: false,
            caveat: if implies.is_none() {
                Some("a terminfo entry name, not a terminal — nearly everything sets one of these")
            } else {
                None
            },
        });
    }

    evidence
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// What a verdict rests on.
///
/// Kept separate from the verdict itself so the report can say which it is. An inference
/// dressed up as a measurement is the failure mode this whole module is arranged to avoid,
/// and a user preference mistaken for a terminal limitation is the one that wastes the
/// most of somebody's afternoon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// ECMA-48 and in the VT100. Nothing to check and nobody to ask.
    Standard,
    /// The terminal reported its own attributes back and the answer was read off them.
    Measured,
    /// Terminal identity, cross-referenced against the support table.
    Inferred,
    /// The user's own environment, not the terminal: `NO_COLOR`.
    Preference,
    /// Neither a measurement nor an identification.
    Nothing,
}

impl Basis {
    /// Stable lowercase word for the JSON and the debug table.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Measured => "measured",
            Self::Inferred => "inferred",
            Self::Preference => "preference",
            Self::Nothing => "nothing",
        }
    }
}

/// The verdict for one attribute, where it came from, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The attribute this is about.
    pub subject: Attr,
    /// The answer, as the exit status will report it.
    pub support: Support,
    /// What the answer rests on.
    pub basis: Basis,
    /// Why it is what it is, in one sentence. Never empty.
    pub reason: String,
    /// What the *terminal* can do, when that differs from the verdict.
    ///
    /// Only populated when a user preference overrode a capability, which is the one case
    /// where "no" and "cannot" are different answers and conflating them would send a
    /// reader looking for a fault in their terminal that is not there.
    pub capability: Option<Support>,
    /// The reason behind [`Finding::capability`].
    pub capability_reason: Option<String>,
}

/// What the probe concluded, and everything it concluded it from.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// Whether stdin and stdout are both terminals. Without that, nothing can be asked —
    /// though the sequences themselves travel down a pipe fine.
    pub interactive: bool,
    /// Multiplexer in the way, if any.
    pub multiplexer: Option<Multiplexer>,
    /// The terminal's XTVERSION self-description, if it answered.
    pub xtversion: Option<String>,
    /// Whether the device-attributes barrier came back.
    pub answered_barrier: bool,
    /// One entry per subject, in the order they were asked about.
    pub measurements: Vec<Measurement>,
    /// Every piece of identity evidence, in the order it was consulted.
    pub evidence: Vec<Evidence>,
    /// The registry id the evidence settled on.
    pub identified: Option<&'static str>,
    /// Display name of the identified terminal.
    pub terminal_name: Option<String>,
    /// What the support matrix says about it overall.
    pub status: Option<SupportStatus>,
    /// The version support arrived in, per the matrix.
    pub since: Option<String>,
    /// The matrix's caveats for this terminal.
    pub notes: Option<String>,
    /// Everything the terminal said, in order.
    pub replies: Vec<Reply>,
    /// The user's colour preference.
    pub preference: ColorPreference,
}

impl ProbeOutcome {
    /// Whether any DECRQSS answer came back at all.
    #[must_use]
    pub fn measurable(&self) -> bool {
        self.measurements
            .iter()
            .any(|measurement| measurement.report.is_some())
    }

    /// The measurement for one subject, if it was asked about.
    #[must_use]
    pub fn measurement(&self, subject: Attr) -> Option<&Measurement> {
        self.measurements
            .iter()
            .find(|measurement| measurement.subject == subject)
    }

    /// The first piece of evidence that both identified something and came from the
    /// terminal itself.
    #[must_use]
    pub fn in_band_evidence(&self) -> Option<&Evidence> {
        self.evidence
            .iter()
            .find(|item| item.in_band && item.implies.is_some())
    }

    /// The first piece of evidence that identified anything at all.
    #[must_use]
    pub fn deciding_evidence(&self) -> Option<&Evidence> {
        self.evidence.iter().find(|item| item.implies.is_some())
    }

    /// The version the terminal reported for itself, if it reported one.
    #[must_use]
    pub fn reported_version(&self) -> Option<Vec<u64>> {
        self.xtversion.as_deref().and_then(parse_version)
    }

    /// Whether the reported version predates the version the table says the underline
    /// shapes arrived in.
    ///
    /// Only consulted when both versions are known: an absent version is not evidence of
    /// an old one.
    #[must_use]
    pub fn predates_support(&self) -> bool {
        let Some(found) = self.reported_version() else {
            return false;
        };
        let Some(wanted) = self
            .identified
            .and_then(supports::since)
            .and_then(parse_version)
        else {
            return false;
        };
        !version_at_least(&found, &wanted)
    }

    /// The verdict for one attribute, with its reason.
    ///
    /// Order of precedence, which is the order of honesty:
    ///
    /// 1. **The user's preference**, for a colour under `NO_COLOR`. It is the answer to
    ///    the question a script is actually asking, and the capability is reported
    ///    alongside so the two are never confused.
    /// 2. **ECMA-48**, for the three attributes that were in the VT100. Nothing to
    ///    measure and no reason to be coy.
    /// 3. **Measurement**, when the terminal answered DECRQSS *and* no multiplexer is in
    ///    the way. A multiplexer answers DECRQSS from its own emulator, so what it kept
    ///    says nothing about what the terminal beyond it renders.
    /// 4. **Inference** from identity, which requires the terminal to have named itself
    ///    in band. An environment variable is somebody else's claim about what is at the
    ///    other end of the pipe, and after an `ssh` hop it is frequently a stale one.
    /// 5. Otherwise [`Support::Unknown`], with a reason saying why.
    #[must_use]
    pub fn finding(&self, subject: Attr) -> Finding {
        let (support, basis, reason) = self.capability_finding(subject);

        // The preference sits on top of the capability rather than replacing it, so the
        // report can say both things at once.
        if subject.is_color() && self.preference.suppresses_color() {
            let explanation = self.preference.explanation().map_or_else(
                || format!("{NO_COLOR} is set"),
                |text| sanitize_text(&text),
            );
            return Finding {
                subject,
                support: Support::No,
                basis: Basis::Preference,
                reason: format!(
                    "{explanation}. This is a decision, not a limitation: the verdict is no \
                     because a script asking whether to emit colour here should be told no."
                ),
                capability: Some(support),
                capability_reason: Some(reason),
            };
        }

        Finding {
            subject,
            support,
            basis,
            reason,
            capability: None,
            capability_reason: None,
        }
    }

    /// The verdict about the *terminal*, ignoring any user preference.
    #[expect(
        clippy::too_many_lines,
        reason = "one decision written top to bottom in order of precedence, with the reason \
                  string built beside each branch; splitting it would separate the verdicts \
                  from their justifications, which is exactly what this module exists to keep \
                  together"
    )]
    fn capability_finding(&self, subject: Attr) -> (Support, Basis, String) {
        if subject.is_universal() {
            return (
                Support::Yes,
                Basis::Standard,
                format!(
                    "`{}` is ECMA-48 and was in the VT100; every terminal ever built renders it",
                    subject.params()
                ),
            );
        }

        if let Some(measurement) = self.measurement(subject)
            && let Some(kept) = measurement.kept()
        {
            let reported = measurement.reported().unwrap_or_default();
            if let Some(multiplexer) = self.multiplexer {
                return (
                    Support::Unknown,
                    Basis::Nothing,
                    format!(
                        "{} answered the attribute report itself rather than forwarding it, so \
                         `{reported}` describes {}'s own emulator and says nothing about the \
                         terminal beyond it",
                        multiplexer.name(),
                        multiplexer.name()
                    ),
                );
            }
            let set = subject.params();
            if kept {
                return (
                    Support::Yes,
                    Basis::Measured,
                    format!(
                        "`{set}` was set and the terminal reported its attributes back as \
                         `{reported}` — it parsed and kept it"
                    ),
                );
            }
            let substitute = measurement
                .substitute()
                .map(|what| format!(", substituting {what}"))
                .unwrap_or_default();
            return (
                Support::No,
                Basis::Measured,
                format!(
                    "`{set}` was set and the terminal reported its attributes back as \
                     `{reported}`{substitute} — it did not keep what was sent"
                ),
            );
        }

        // No measurement. Fall back to identity.
        let Some(evidence) = self.in_band_evidence() else {
            let reason = match self.deciding_evidence() {
                Some(evidence) => format!(
                    "the terminal neither reported its attributes nor named itself; {}={} points \
                     at {} but that is the environment's claim, not the terminal's",
                    evidence.source,
                    sanitize_text(&evidence.value),
                    evidence.implies.unwrap_or("nothing")
                ),
                None if !self.interactive => {
                    "stdout and stdin are not both terminals, so nothing could be asked and \
                     nothing identified this terminal"
                        .to_owned()
                }
                None => "nothing identified this terminal, and it did not answer the attribute \
                         report either"
                    .to_owned(),
            };
            return (Support::Unknown, Basis::Nothing, reason);
        };

        let name = self
            .terminal_name
            .clone()
            .unwrap_or_else(|| sanitize_text(&evidence.value));

        if let Some(multiplexer) = self.multiplexer {
            return (
                Support::Unknown,
                Basis::Nothing,
                format!(
                    "{} is in the way, and whether an attribute reaches the terminal beyond it \
                     depends on that terminal's terminfo — `Smulx` and `Setulc` for the underline \
                     shapes and colour — which cannot be seen from in here",
                    multiplexer.name()
                ),
            );
        }

        let Some(id) = self.identified else {
            return (
                Support::Unknown,
                Basis::Nothing,
                format!("{name} named itself, but escapade has no row for it"),
            );
        };

        // Only the underline shapes are young enough for a version to matter.
        if matches!(subject, Attr::Underline(shape) if UnderlineShape::EXTENDED.contains(&shape))
            && self.predates_support()
        {
            return (
                Support::No,
                Basis::Inferred,
                format!(
                    "{name} reported itself as `{}`, and the table records the underline shapes as \
                     arriving in {}",
                    sanitize_text(&evidence.value),
                    supports::since(id).unwrap_or("a later version")
                ),
            );
        }

        let support = supports::support(id, subject);
        (
            support,
            match support {
                Support::Unknown => Basis::Nothing,
                Support::Yes | Support::No => Basis::Inferred,
            },
            match support {
                Support::Yes => format!(
                    "{name} named itself in its XTVERSION reply, and escapade's table says it \
                     renders `{}`",
                    subject.params()
                ),
                Support::No => format!(
                    "{name} named itself in its XTVERSION reply, and escapade's table says it does \
                     not render `{}` — it will be ignored",
                    subject.params()
                ),
                Support::Unknown => format!(
                    "{name} named itself, but escapade's table does not record whether it renders \
                     `{}`",
                    subject.params()
                ),
            },
        )
    }

    /// Findings for every subject that was asked about.
    #[must_use]
    pub fn findings(&self) -> Vec<Finding> {
        self.measurements
            .iter()
            .map(|measurement| self.finding(measurement.subject))
            .collect()
    }

    /// The answer the exit status carries.
    ///
    /// `Yes` only when every attribute asked about is supported. A definite `No` wins over
    /// an `Unknown`, because it is the more actionable answer: a script told "one of these
    /// demonstrably does not work" can pick a different approach, where "one of these
    /// might not work" leaves it no better off than before it asked.
    #[must_use]
    pub fn support(&self) -> Support {
        let findings = self.findings();
        if findings.is_empty() {
            return Support::Unknown;
        }
        if findings.iter().any(|f| f.support == Support::No) {
            return Support::No;
        }
        if findings.iter().any(|f| f.support == Support::Unknown) {
            return Support::Unknown;
        }
        Support::Yes
    }
}

// ---------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------

/// Runs the probe using an existing terminal and reader.
///
/// Takes both rather than creating them because there must only ever be one
/// [`InputReader`] per process — two would race for the same bytes on stdin, and each
/// would see a random half of the terminal's answers.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal that simply
/// stays quiet is not an error; it is a result.
pub fn probe(
    catalog: &Catalog,
    term: &mut Term,
    input: &mut InputReader,
    plan: &[Attr],
) -> Result<ProbeOutcome> {
    let mut outcome = ProbeOutcome {
        multiplexer: Multiplexer::detect(),
        interactive: stdout().is_terminal() && stdin().is_terminal(),
        preference: ColorPreference::from_env(),
        measurements: plan
            .iter()
            .map(|subject| Measurement {
                subject: *subject,
                report: None,
            })
            .collect(),
        ..ProbeOutcome::default()
    };

    if outcome.interactive {
        // Raw mode is required: in cooked mode a reply would sit in the line buffer until
        // the user pressed Enter, and would be echoed back at them.
        term.enable_raw()?;
        input.drain();

        term.write_now(&version_query())?;
        term.write_now(&measurement_sequence(plan))?;
        // The barrier goes last and is deliberately not wrapped for a multiplexer: the
        // question is whether the round trip completed. A multiplexer answers device
        // attributes from its own emulator rather than forwarding them, which is also why
        // nothing measured from inside one licenses a verdict about the terminal beyond it.
        term.write_now(&barrier_query())?;

        // DECRQSS answers arrive in the order they were asked for, so they are matched up
        // positionally. Replies are ordered, so report `n` answers query `n`; a short
        // tail simply leaves the rest unmeasured.
        let mut next = 0usize;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            // EOF on stdin means there is nobody to answer; treat it as silence.
            let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
                break;
            };
            outcome.replies.push(reply.clone());
            match &reply {
                Reply::Dcs(payload) => {
                    if let Some(name) = parse_xtversion(payload) {
                        outcome.xtversion = Some(name);
                    } else if let Some(report) = parse_sgr_report(payload) {
                        if let Some(slot) = outcome.measurements.get_mut(next) {
                            slot.report = Some(report);
                        }
                        next += 1;
                    }
                }
                Reply::Csi {
                    final_byte: 'c', ..
                } => {
                    // Everything sent before the barrier has been processed, so waiting
                    // longer would learn nothing.
                    outcome.answered_barrier = true;
                    break;
                }
                _ => {}
            }
        }

        // Anything still in flight has to be consumed, or it is delivered to whatever runs
        // next — the user's shell, which prints it as stray text. This probe is silent by
        // default, so that matters more here than anywhere.
        drain_remaining(input, &mut outcome);
    }

    outcome.evidence = collect_evidence(outcome.xtversion.as_deref());
    outcome.identified = outcome.deciding_evidence().and_then(|item| item.implies);
    describe_terminal(catalog, &mut outcome);

    Ok(outcome)
}

/// Fills in the display name and matrix row for the identified terminal.
fn describe_terminal(catalog: &Catalog, outcome: &mut ProbeOutcome) {
    let Some(id) = outcome.identified else {
        return;
    };
    outcome.terminal_name = catalog.terminal(id).map(|entry| entry.name.clone());
    if let Ok(feature) = catalog.feature(FEATURE_ID)
        && let Some(support) = feature.support.iter().find(|row| row.terminal == id)
    {
        outcome.status = Some(support.status);
        outcome.since = support.since.clone();
        outcome.notes = support.notes.clone();
    }
}

/// Consumes every reply still in flight, up to [`DRAIN_TIMEOUT`] of quiet.
///
/// The loop matters: this probe can provoke twenty replies, and stopping at the first
/// barrier leaves the rest queued. Anything that arrives here is still recorded, so
/// `--format debug` shows the full transcript.
fn drain_remaining(input: &mut InputReader, outcome: &mut ProbeOutcome) {
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
            break;
        };
        if let Reply::Dcs(payload) = &reply
            && let Some(name) = parse_xtversion(payload)
        {
            outcome.xtversion = Some(name);
        }
        if matches!(reply, Reply::Csi { final_byte: 'c', .. }) {
            outcome.answered_barrier = true;
        }
        outcome.replies.push(reply);
    }
}

/// Runs a probe from scratch.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn detect(catalog: &Catalog, plan: &[Attr]) -> Result<ProbeOutcome> {
    if !(stdout().is_terminal() && stdin().is_terminal()) {
        // Nothing to ask, and spawning a reader thread on a redirected stdin would be work
        // for no answer. The identity evidence is still worth collecting: it is what the
        // report falls back on.
        let mut outcome = ProbeOutcome {
            multiplexer: Multiplexer::detect(),
            interactive: false,
            preference: ColorPreference::from_env(),
            measurements: plan
                .iter()
                .map(|subject| Measurement {
                    subject: *subject,
                    report: None,
                })
                .collect(),
            evidence: collect_evidence(None),
            ..ProbeOutcome::default()
        };
        outcome.identified = outcome.deciding_evidence().and_then(|item| item.implies);
        describe_terminal(catalog, &mut outcome);
        return Ok(outcome);
    }

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let outcome = probe(catalog, &mut term, &mut input, plan);
    // Restore before the caller prints anything, so any report appears in a normal
    // cooked-mode terminal.
    term.restore();
    outcome
}

/// Runs `escapade style probe`, returning the exit status that is the answer.
///
/// Silent unless a `--format` was asked for: `0` every attribute supported, `1` at least
/// one demonstrably not, `2` at least one could not be determined.
///
/// # Errors
///
/// Fails if a colour cannot be parsed, or if the terminal cannot be set up.
pub fn report(
    theme: &Theme,
    catalog: &Catalog,
    flags: &StyleAttrs,
    format: Option<ProbeFormat>,
) -> Result<ExitCode> {
    let plan = subjects(flags)?;
    let outcome = detect(catalog, &plan)?;
    let support = outcome.support();

    match format {
        None => {}
        Some(ProbeFormat::Json) => print!("{}", json_line(&outcome)),
        Some(ProbeFormat::Debug) => print_debug(theme, &outcome),
    }

    Ok(support.exit_code())
}

// ---------------------------------------------------------------------------
// `--format json`
// ---------------------------------------------------------------------------

/// Renders the outcome as one self-contained JSON object, on one line.
///
/// Hand-written rather than pulled in from a crate: the shape is fixed, the whole job is
/// two functions, and `escapade` earns its "no dependency you cannot read" property by
/// not adding one for this.
#[must_use]
pub fn json_line(outcome: &ProbeOutcome) -> String {
    let mut out = String::with_capacity(2048);
    out.push('{');
    let _ = write!(out, "\"feature\":{},", json_string(FEATURE_ID));
    let _ = write!(out, 
        "\"verdict\":{},",
        json_string(outcome.support().as_str())
    );
    let _ = write!(out, "\"measurable\":{},", outcome.measurable());
    let _ = write!(out, "\"interactive\":{},", outcome.interactive);
    let _ = write!(out, 
        "\"answered_barrier\":{},",
        outcome.answered_barrier
    );
    let _ = write!(out, 
        "\"no_color\":{},",
        match outcome.preference.value() {
            Some(value) => format!("{{\"set\":true,\"value\":{}}}", json_string(value)),
            None => "{\"set\":false,\"value\":null}".to_owned(),
        }
    );
    let _ = write!(out, 
        "\"multiplexer\":{},",
        match outcome.multiplexer {
            Some(multiplexer) => json_string(multiplexer.name()),
            None => "null".to_owned(),
        }
    );
    let _ = write!(out, 
        "\"xtversion\":{},",
        match &outcome.xtversion {
            Some(version) => json_string(version),
            None => "null".to_owned(),
        }
    );
    let _ = write!(out, 
        "\"terminal\":{},",
        match outcome.identified {
            Some(id) => json_string(id),
            None => "null".to_owned(),
        }
    );
    let _ = write!(out, 
        "\"matrix_status\":{},",
        match outcome.status {
            Some(status) => json_string(status.label()),
            None => "null".to_owned(),
        }
    );

    out.push_str("\"attributes\":[");
    for (index, finding) in outcome.findings().iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let reported = outcome
            .measurement(finding.subject)
            .and_then(Measurement::reported);
        let _ = write!(out, 
            "{{\"attribute\":{},\"sgr\":{},\"reset\":{},\"verdict\":{},\"reason_code\":{},\
             \"reason\":{},\"reported\":{},\"capability\":{},\"capability_reason\":{}}}",
            json_string(&finding.subject.label()),
            json_string(&finding.subject.params()),
            json_string(finding.subject.reset_params()),
            json_string(finding.support.as_str()),
            json_string(finding.basis.as_str()),
            json_string(&finding.reason),
            match reported {
                Some(params) => json_string(params),
                None => "null".to_owned(),
            },
            match finding.capability {
                Some(support) => json_string(support.as_str()),
                None => "null".to_owned(),
            },
            match &finding.capability_reason {
                Some(reason) => json_string(reason),
                None => "null".to_owned(),
            },
        );
    }
    out.push_str("],");

    out.push_str("\"evidence\":[");
    for (index, item) in outcome.evidence.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, 
            "{{\"source\":{},\"value\":{},\"implies\":{},\"in_band\":{}}}",
            json_string(item.source),
            json_string(&item.value),
            match item.implies {
                Some(id) => json_string(id),
                None => "null".to_owned(),
            },
            item.in_band
        );
    }
    out.push(']');
    out.push_str("}\n");
    out
}

/// Quotes and escapes a string for JSON.
///
/// Environment variables and terminal replies are both user data — `TERM_PROGRAM` can
/// contain a quote, a backslash or a control character — so this escapes properly rather
/// than hoping. Control characters become `\uXXXX`, which also means the output can never
/// carry a raw `ESC` into the reader's terminal.
#[must_use]
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < '\u{20}' || ch == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(ch));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// `--format debug`
// ---------------------------------------------------------------------------

/// Prints everything the probe saw and concluded, for a person.
#[expect(
    clippy::too_many_lines,
    reason = "one screen of output, written top to bottom; splitting it would scatter \
              the layout across functions without simplifying anything"
)]
fn print_debug(theme: &Theme, outcome: &ProbeOutcome) {
    let support = outcome.support();

    println!();
    println!("  {}", theme.paint("Text attribute support (SGR)", Ink::Title));
    println!();

    for line in wrap(
        "A terminal that ignores an attribute looks exactly like one that renders it, from the \
         sending end. Where the terminal implements DECRQSS this is a real measurement: the \
         attribute is set and the terminal is asked to report its own SGR state back, so a \
         silently dropped attribute or a quantised colour is caught. Where it does not, the answer \
         falls back to inference from the terminal's identity — and every line below says which.",
        theme.width().saturating_sub(4),
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();

    // The NO_COLOR banner goes near the top, before the verdicts, because for the reader
    // who has it set by mistake it *is* the answer and burying it would waste their time.
    if outcome.preference.suppresses_color()
        && outcome.findings().iter().any(|f| f.subject.is_color())
    {
        let explanation = outcome
            .preference
            .explanation()
            .map(|text| sanitize_text(&text))
            .unwrap_or_default();
        for line in wrap(&explanation, theme.width().saturating_sub(6)) {
            println!("  {} {}", theme.paint("!", Ink::Warn), theme.paint(&line, Ink::Warn));
        }
        for line in wrap(
            "Every colour below is therefore reported as `no`, with the terminal's own capability \
             shown next to it. The two are different answers and this report keeps them apart.",
            theme.width().saturating_sub(6),
        ) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
        println!();
    }

    let (symbol, ink) = verdict_marks(support);
    println!(
        "  {} {}  {}",
        theme.paint(symbol, ink),
        theme.paint(support.as_str(), ink),
        theme.paint(
            &format!(
                "for the {} attribute{} asked about (exit status {})",
                outcome.measurements.len(),
                if outcome.measurements.len() == 1 { "" } else { "s" },
                exit_status_number(support)
            ),
            Ink::Muted
        )
    );
    println!();

    println!("  {}", theme.paint("attribute by attribute", Ink::Accent));
    println!(
        "    {}",
        theme.paint(
            "each row names the flag that asks about it on its own",
            Ink::Muted
        )
    );
    let findings = outcome.findings();
    let label_width = findings
        .iter()
        .map(|finding| finding.subject.label().chars().count())
        .max()
        .unwrap_or(0);
    let sgr_width = findings
        .iter()
        .map(|finding| finding.subject.params().chars().count())
        .max()
        .unwrap_or(0);
    for finding in &findings {
        let (symbol, ink) = verdict_marks(finding.support);
        println!(
            "    {} {}  {}  {}  {}",
            theme.paint(symbol, ink),
            theme.paint(
                &format!("{:<label_width$}", finding.subject.label()),
                Ink::Strong
            ),
            theme.paint(&format!("{:<sgr_width$}", finding.subject.params()), Ink::Code),
            theme.paint(&format!("{:<7}", finding.support.as_str()), ink),
            theme.paint(&format!("[{}]", finding.basis.as_str()), Ink::Muted)
        );
        println!(
            "        {}",
            theme.paint(
                &format!("escapade style probe {}", finding.subject.flag()),
                Ink::Code
            )
        );
        for line in wrap(&finding.reason, theme.width().saturating_sub(8)) {
            println!("        {}", theme.paint(&line, Ink::Muted));
        }
        // The capability, when a preference overrode it. This is the line that stops
        // somebody blaming their terminal for their own environment variable.
        if let Some(capability) = finding.capability {
            let (_, capability_ink) = verdict_marks(capability);
            println!(
                "        {} {}",
                theme.paint("terminal itself:", Ink::Accent),
                theme.paint(capability.as_str(), capability_ink)
            );
            if let Some(reason) = &finding.capability_reason {
                for line in wrap(reason, theme.width().saturating_sub(10)) {
                    println!("          {}", theme.paint(&line, Ink::Muted));
                }
            }
        }
    }
    println!();

    println!("  {}", theme.paint("what was asked", Ink::Accent));
    for (sequence, meaning) in [
        (version_query(), "XTVERSION — who are you?"),
        (
            format!("{CSI}4:3m{}", sgr_query()),
            "set the attribute, then ask what the attributes are (once per attribute)",
        ),
        (
            barrier_query(),
            "the barrier, which every terminal answers, so silence is distinguishable from slowness",
        ),
    ] {
        println!(
            "    {}  {}",
            theme.paint(&format!("{:<26}", pretty(&sequence)), Ink::Code),
            theme.paint(meaning, Ink::Muted)
        );
    }
    println!();

    println!("  {}", theme.paint("what came back", Ink::Accent));
    if !outcome.interactive {
        for line in wrap(
            "Nothing was asked: stdout and stdin are not both terminals, so there was nobody to \
             answer. Note that this does not stop the sequences from being useful — a styled run \
             written into a pipe reaches whatever eventually renders it.",
            theme.width().saturating_sub(6),
        ) {
            println!("      {}", theme.paint(&line, Ink::Muted));
        }
    } else if outcome.replies.is_empty() {
        println!("    {}", theme.paint("(nothing at all)", Ink::Muted));
    }
    for reply in &outcome.replies {
        println!("    {}", theme.paint(&describe_reply(reply), Ink::Code));
    }
    if let Some(multiplexer) = outcome.multiplexer {
        println!();
        for line in wrap(
            &format!(
                "{} answers both device attributes and the attribute report from its own emulator \
                 rather than forwarding them, so nothing measured in here says anything about the \
                 terminal beyond it. Whether an attribute survives depends on the outer terminal's \
                 terminfo carrying `Smulx` and `Setulc`.",
                multiplexer.name()
            ),
            theme.width().saturating_sub(6),
        ) {
            println!("      {}", theme.paint(&line, Ink::Warn));
        }
    }
    println!();

    println!("  {}", theme.paint("who this looks like", Ink::Accent));
    if outcome.evidence.is_empty() {
        println!(
            "    {}",
            theme.paint("(nothing — not even TERM is set)", Ink::Muted)
        );
    }
    let width = outcome
        .evidence
        .iter()
        .map(|item| item.source.chars().count())
        .max()
        .unwrap_or(0)
        .max(9);
    for item in &outcome.evidence {
        let arrow = match item.implies {
            Some(id) => theme.paint(&format!("\u{2192} {id}"), Ink::Good),
            None => theme.paint("\u{2192} nothing", Ink::Muted),
        };
        let band = if item.in_band {
            theme.paint("[in band]", Ink::Accent)
        } else {
            theme.paint("[ambient]", Ink::Muted)
        };
        println!(
            "    {}  {}  {arrow}  {band}",
            theme.paint(&format!("{:<width$}", item.source), Ink::Accent),
            theme.paint(
                &format!("{:<28}", truncate(&sanitize_text(&item.value), 28)),
                Ink::Code
            ),
        );
        if let Some(caveat) = item.caveat {
            for line in wrap(caveat, theme.width().saturating_sub(width + 12)) {
                println!(
                    "    {}  {}",
                    " ".repeat(width),
                    theme.paint(&line, Ink::Muted)
                );
            }
        }
    }
    println!();

    println!("  {}", theme.paint("what the matrix says", Ink::Accent));
    match (&outcome.terminal_name, outcome.status) {
        (Some(name), Some(status)) => {
            let since = match &outcome.since {
                Some(since) => format!(", since {since}"),
                None => String::new(),
            };
            println!(
                "    {}",
                theme.paint(&format!("{name} — {}{since}", status.label()), Ink::Strong)
            );
            if let Some(notes) = &outcome.notes {
                println!("{}", theme.markdown_indented(notes, 4));
            }
        }
        _ => {
            for line in wrap(
                "Nothing identified this terminal, so the matrix has nothing to say about it. That \
                 is not a no — and since an unrecognised parameter is ignored rather than \
                 mishandled, the right response is usually to send the sequence anyway.",
                theme.width().saturating_sub(6),
            ) {
                println!("      {}", theme.paint(&line, Ink::Muted));
            }
        }
    }

    println!();
    for line in wrap(
        "Send them anyway, in most cases. Every attribute here is one parameter of one SGR \
         sequence, and a terminal that does not implement a parameter ignores it. The two things \
         worth branching on are whether to *rely* on a distinction — a linter using curly for \
         errors and dotted for hints has one mark rather than two on a terminal that draws neither \
         — and whether to emit 24-bit colour, since a terminal without it substitutes a nearby \
         colour rather than refusing.",
        theme.width().saturating_sub(4),
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();
}

/// Symbol and colour for a verdict.
fn verdict_marks(support: Support) -> (&'static str, Ink) {
    match support {
        Support::Yes => ("\u{2714}", Ink::Good),
        Support::No => ("\u{2718}", Ink::Bad),
        Support::Unknown => ("?", Ink::Warn),
    }
}

/// The number behind a [`Support`], for printing next to the word.
fn exit_status_number(support: Support) -> u8 {
    match support {
        Support::Yes => 0,
        Support::No => 1,
        Support::Unknown => 2,
    }
}

/// One-line rendering of a reply, safe to print.
fn describe_reply(reply: &Reply) -> String {
    let body = match reply {
        Reply::Apc(payload) => format!("APC {payload} ST"),
        Reply::Dcs(payload) => format!("DCS {payload} ST"),
        Reply::Osc(payload) => format!("OSC {payload} ST"),
        Reply::Csi { params, final_byte } => format!("CSI {params}{final_byte}"),
    };
    // A reply is whatever the terminal chose to send, so it is untrusted like any other
    // input and goes through the sanitiser before being printed back.
    sanitize_text(&body)
}

/// Shortens `text` to `limit` characters, with an ellipsis when it had to.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::style::{
        attrs::{PROBE_PALETTE, PROBE_RGB},
        color::{Color, ColorRole},
    };

    /// An outcome with the given measurements and nothing else.
    fn measured(entries: &[(Attr, Option<SgrReport>)]) -> ProbeOutcome {
        ProbeOutcome {
            interactive: true,
            answered_barrier: true,
            measurements: entries
                .iter()
                .map(|(subject, report)| Measurement {
                    subject: *subject,
                    report: report.clone(),
                })
                .collect(),
            ..ProbeOutcome::default()
        }
    }

    /// An outcome that was told `name` by the terminal itself.
    fn in_band(name: &str, plan: &[Attr]) -> ProbeOutcome {
        let implies = registry_id_for(name);
        ProbeOutcome {
            interactive: true,
            answered_barrier: true,
            xtversion: Some(name.to_owned()),
            identified: implies,
            terminal_name: Some(name.to_owned()),
            evidence: vec![Evidence {
                source: "XTVERSION",
                value: name.to_owned(),
                implies,
                in_band: true,
                caveat: None,
            }],
            measurements: plan
                .iter()
                .map(|subject| Measurement {
                    subject: *subject,
                    report: None,
                })
                .collect(),
            ..ProbeOutcome::default()
        }
    }

    /// Flags with nothing set.
    fn no_flags() -> StyleAttrs {
        StyleAttrs {
            bold: false,
            dim: false,
            italic: false,
            underline: None,
            blink: false,
            rapid_blink: false,
            reverse: false,
            conceal: false,
            strikethrough: false,
            overline: false,
            color: None,
            background: None,
            underline_color: None,
        }
    }

    // -- the queries --------------------------------------------------------

    #[test]
    fn the_queries_are_the_documented_ones() {
        assert_eq!(version_query(), "\u{1b}[>q");
        assert_eq!(sgr_query(), "\u{1b}P$qm\u{1b}\\");
        assert_eq!(barrier_query(), "\u{1b}[c");
        assert_eq!(reset_all(), "\u{1b}[0m");
    }

    #[test]
    fn the_pipelined_sequence_resets_between_measurements_and_at_the_end() {
        let plan = vec![Attr::Bold, Attr::Underline(UnderlineShape::Curly)];
        let sequence = measurement_sequence(&plan);
        assert_eq!(sequence.matches("\u{1b}[0m").count(), plan.len() + 1);
        assert!(sequence.ends_with("\u{1b}[0m"), "nothing may be left set");
        assert_eq!(sequence.matches("\u{1b}P$qm\u{1b}\\").count(), plan.len());
        for attr in &plan {
            assert!(sequence.contains(&format!("\u{1b}[{}m", attr.params())));
        }
    }

    #[test]
    fn the_measurement_prints_nothing() {
        // Setting an attribute changes no cells, which is what makes this invisible.
        let sequence = measurement_sequence(&every_attribute());
        let printable: String = sequence
            .chars()
            .filter(|ch| !ch.is_control() && !"[];:$qm0123456789P\\".contains(*ch))
            .collect();
        assert!(printable.is_empty(), "would print: {printable:?}");
    }

    // -- what to probe ------------------------------------------------------

    #[test]
    fn no_flags_probes_the_whole_family() {
        let plan = subjects(&no_flags()).unwrap();
        assert_eq!(plan, every_attribute());
        assert!(plan.len() > 15);
    }

    #[test]
    fn flags_probe_exactly_what_was_asked_for() {
        let flags = StyleAttrs {
            bold: true,
            overline: true,
            ..no_flags()
        };
        assert_eq!(subjects(&flags).unwrap(), vec![Attr::Bold, Attr::Overline]);
    }

    #[test]
    fn the_colour_form_written_decides_what_is_measured() {
        // Three different questions, and a terminal can answer them differently. This is
        // the whole reason the depth is part of the value rather than a separate setting.
        for (written, expected) in [
            ("red", Color::Basic(1)),
            ("173", Color::Palette(173)),
            ("#010203", Color::Rgb(1, 2, 3)),
        ] {
            let flags = StyleAttrs {
                color: Some(written.to_owned()),
                ..no_flags()
            };
            assert_eq!(
                subjects(&flags).unwrap(),
                vec![Attr::Color(ColorRole::Foreground, expected)],
                "{written}"
            );
        }
    }

    #[test]
    fn a_colour_is_still_probed_under_no_color() {
        // The preference decides the verdict, not whether the question is legitimate.
        let flags = StyleAttrs {
            color: Some("#010203".to_owned()),
            ..no_flags()
        };
        assert_eq!(subjects(&flags).unwrap().len(), 1);
    }

    #[test]
    fn a_bad_colour_is_refused_rather_than_probed() {
        let flags = StyleAttrs {
            color: Some("reddish".to_owned()),
            ..no_flags()
        };
        assert!(subjects(&flags).is_err());
    }

    // -- parsing the answers ------------------------------------------------

    #[test]
    fn decrqss_answers_are_unwrapped() {
        assert_eq!(
            parse_sgr_report("1$r0;4:3m"),
            Some(SgrReport::Reported("0;4:3".to_owned()))
        );
        assert_eq!(
            parse_sgr_report("1$rm"),
            Some(SgrReport::Reported(String::new()))
        );
        assert_eq!(parse_sgr_report("0$r"), Some(SgrReport::Refused));
        assert_eq!(parse_sgr_report("0$rm"), Some(SgrReport::Refused));
        assert_eq!(parse_sgr_report("1$r0;4:3"), None, "no final `m`");
        assert_eq!(parse_sgr_report(""), None);
    }

    #[test]
    fn an_xtversion_reply_is_never_mistaken_for_an_attribute_report() {
        // Both arrive as DCS payloads on the same channel.
        assert_eq!(parse_sgr_report(">|kitty 0.36.0"), None);
        assert_eq!(
            parse_xtversion(">|kitty 0.36.0").as_deref(),
            Some("kitty 0.36.0")
        );
        assert_eq!(parse_xtversion("1$r0;4:3m"), None);
        assert_eq!(parse_xtversion(">|"), None);
    }

    #[test]
    fn a_refusal_is_an_answer_that_carries_no_information() {
        let measurement = Measurement {
            subject: Attr::Bold,
            report: Some(SgrReport::Refused),
        };
        assert_eq!(measurement.kept(), None);
        assert_eq!(measurement.reported(), None);
        assert_eq!(measurement.substitute(), None);
    }

    // -- verdicts -----------------------------------------------------------

    #[test]
    fn the_vt100_attributes_never_need_a_terminal() {
        let outcome = ProbeOutcome::default();
        for attr in [
            Attr::Bold,
            Attr::Reverse,
            Attr::Underline(UnderlineShape::Single),
        ] {
            let finding = outcome.finding(attr);
            assert_eq!(finding.support, Support::Yes, "{}", attr.label());
            assert_eq!(finding.basis, Basis::Standard);
            assert!(finding.reason.contains("VT100"), "{}", finding.reason);
        }
    }

    #[test]
    fn a_measurement_that_kept_the_attribute_is_a_yes() {
        let outcome = measured(&[(
            Attr::Underline(UnderlineShape::Curly),
            Some(SgrReport::Reported("0;4:3".to_owned())),
        )]);
        let finding = outcome.finding(Attr::Underline(UnderlineShape::Curly));
        assert_eq!(finding.support, Support::Yes);
        assert_eq!(finding.basis, Basis::Measured);
        assert!(finding.reason.contains("4:3"), "{}", finding.reason);
    }

    #[test]
    fn a_measurement_that_lost_the_attribute_is_a_no() {
        let outcome = measured(&[(
            Attr::Underline(UnderlineShape::Curly),
            Some(SgrReport::Reported("0;4".to_owned())),
        )]);
        let finding = outcome.finding(Attr::Underline(UnderlineShape::Curly));
        assert_eq!(finding.support, Support::No);
        assert_eq!(finding.basis, Basis::Measured);
        assert!(
            finding.reason.contains("did not keep"),
            "{}",
            finding.reason
        );
    }

    #[test]
    fn a_quantised_colour_is_reported_as_the_substitution_it_is() {
        // The measurement nothing else can make: the terminal accepted 24-bit colour and
        // threw it away. Naming the palette entry it landed on is the whole value.
        let subject = Attr::Color(ColorRole::Foreground, PROBE_RGB);
        let outcome = measured(&[(subject, Some(SgrReport::Reported("0;38;5;16".to_owned())))]);
        let finding = outcome.finding(subject);
        assert_eq!(finding.support, Support::No);
        assert_eq!(finding.basis, Basis::Measured);
        assert!(
            finding.reason.contains("palette entry 16"),
            "{}",
            finding.reason
        );
    }

    #[test]
    fn attributes_are_judged_one_at_a_time() {
        let outcome = measured(&[
            (
                Attr::Underline(UnderlineShape::Curly),
                Some(SgrReport::Reported("4:3".to_owned())),
            ),
            (
                Attr::Underline(UnderlineShape::Dotted),
                Some(SgrReport::Reported("4".to_owned())),
            ),
            (Attr::Overline, None),
        ]);
        assert_eq!(
            outcome.finding(Attr::Underline(UnderlineShape::Curly)).support,
            Support::Yes
        );
        assert_eq!(
            outcome.finding(Attr::Underline(UnderlineShape::Dotted)).support,
            Support::No
        );
        assert_eq!(outcome.finding(Attr::Overline).support, Support::Unknown);
    }

    #[test]
    fn nothing_measured_inside_a_multiplexer_licenses_a_verdict() {
        // tmux answers DECRQSS from its own emulator, so what tmux kept says nothing about
        // the terminal beyond it — in either direction.
        for reported in ["4:3", "4"] {
            let mut outcome = measured(&[(
                Attr::Underline(UnderlineShape::Curly),
                Some(SgrReport::Reported(reported.to_owned())),
            )]);
            outcome.multiplexer = Some(Multiplexer::Tmux);
            let finding = outcome.finding(Attr::Underline(UnderlineShape::Curly));
            assert_eq!(finding.support, Support::Unknown, "reported {reported}");
            assert!(finding.reason.contains("tmux"), "{}", finding.reason);
        }
    }

    #[test]
    fn inference_needs_the_terminal_to_have_named_itself() {
        let outcome = ProbeOutcome {
            interactive: true,
            identified: Some("kitty"),
            terminal_name: Some("kitty".to_owned()),
            evidence: vec![Evidence {
                source: "TERM_PROGRAM",
                value: "kitty".to_owned(),
                implies: Some("kitty"),
                in_band: false,
                caveat: None,
            }],
            measurements: vec![Measurement {
                subject: Attr::Overline,
                report: None,
            }],
            ..ProbeOutcome::default()
        };
        let finding = outcome.finding(Attr::Overline);
        assert_eq!(finding.support, Support::Unknown);
        assert!(
            finding.reason.contains("not the terminal's"),
            "{}",
            finding.reason
        );
    }

    #[test]
    fn xterm_naming_itself_is_a_real_no_for_the_underline_shapes() {
        // The headline fact of this feature.
        let plan: Vec<Attr> = UnderlineShape::EXTENDED
            .iter()
            .map(|shape| Attr::Underline(*shape))
            .collect();
        let outcome = in_band("XTerm(389)", &plan);
        assert_eq!(outcome.identified, Some("xterm"));
        for shape in UnderlineShape::EXTENDED {
            let finding = outcome.finding(Attr::Underline(*shape));
            assert_eq!(finding.support, Support::No, "{}", shape.name());
            assert_eq!(finding.basis, Basis::Inferred);
            assert!(finding.reason.contains("ignored"), "{}", finding.reason);
        }
        // …and a plain underline is still a yes, because it always is.
        assert_eq!(
            outcome.finding(Attr::Underline(UnderlineShape::Single)).support,
            Support::Yes
        );
    }

    #[test]
    fn a_measurement_beats_the_table() {
        // A build newer or older than the table is exactly why measurement wins.
        let subject = Attr::Underline(UnderlineShape::Curly);
        let mut outcome = in_band("XTerm(999)", &[subject]);
        outcome.measurements = vec![Measurement {
            subject,
            report: Some(SgrReport::Reported("4:3".to_owned())),
        }];
        let finding = outcome.finding(subject);
        assert_eq!(finding.support, Support::Yes);
        assert_eq!(finding.basis, Basis::Measured);
    }

    #[test]
    fn a_self_reported_version_older_than_the_table_is_a_no_for_the_shapes() {
        let subject = Attr::Underline(UnderlineShape::Curly);
        let mut outcome = in_band("VTE 0.40.0", &[subject]);
        outcome.identified = Some("gnome-terminal");
        outcome.evidence[0].implies = Some("gnome-terminal");
        assert!(outcome.predates_support(), "0.40 predates VTE 0.52");
        let finding = outcome.finding(subject);
        assert_eq!(finding.support, Support::No);
        assert_eq!(finding.basis, Basis::Inferred);
        assert!(finding.reason.contains("0.52"), "{}", finding.reason);
    }

    // -- every verdict states a reason -------------------------------------

    #[test]
    fn every_finding_states_a_reason_in_every_situation() {
        // The requirement that motivated `Finding::reason`: an answer without a reason is
        // not much better than no answer, because the reader cannot tell whether to act
        // on it.
        let plan = every_attribute();
        let outcomes = [
            ProbeOutcome::default(),
            ProbeOutcome {
                interactive: true,
                ..ProbeOutcome::default()
            },
            measured(
                &plan
                    .iter()
                    .map(|attr| (*attr, Some(SgrReport::Reported("0;1".to_owned()))))
                    .collect::<Vec<_>>(),
            ),
            in_band("kitty 0.36.0", &plan),
            in_band("XTerm(389)", &plan),
            ProbeOutcome {
                interactive: true,
                multiplexer: Some(Multiplexer::Tmux),
                measurements: plan
                    .iter()
                    .map(|subject| Measurement {
                        subject: *subject,
                        report: None,
                    })
                    .collect(),
                ..ProbeOutcome::default()
            },
            ProbeOutcome {
                interactive: true,
                preference: ColorPreference::from_value(Some("1")),
                measurements: plan
                    .iter()
                    .map(|subject| Measurement {
                        subject: *subject,
                        report: None,
                    })
                    .collect(),
                ..ProbeOutcome::default()
            },
        ];
        for outcome in &outcomes {
            for attr in &plan {
                let finding = outcome.finding(*attr);
                assert!(
                    !finding.reason.trim().is_empty(),
                    "{} has no reason",
                    attr.label()
                );
                assert!(
                    !finding.reason.contains('\u{1b}'),
                    "{} leaks an escape",
                    attr.label()
                );
            }
        }
    }

    // -- NO_COLOR -----------------------------------------------------------

    #[test]
    fn no_color_makes_a_colour_a_no_and_says_it_is_a_decision() {
        let subject = Attr::Color(ColorRole::Foreground, PROBE_RGB);
        let outcome = ProbeOutcome {
            interactive: true,
            preference: ColorPreference::from_value(Some("1")),
            measurements: vec![Measurement {
                subject,
                report: Some(SgrReport::Reported("0;38;2;1;2;3".to_owned())),
            }],
            ..ProbeOutcome::default()
        };
        let finding = outcome.finding(subject);
        assert_eq!(finding.support, Support::No);
        assert_eq!(finding.basis, Basis::Preference);
        assert!(finding.reason.contains("NO_COLOR"), "{}", finding.reason);
        assert!(
            finding.reason.contains("not a limitation of your terminal"),
            "{}",
            finding.reason
        );
        assert!(finding.reason.contains("a decision"), "{}", finding.reason);

        // …and the capability is reported alongside, measured and positive.
        assert_eq!(finding.capability, Some(Support::Yes));
        let capability_reason = finding.capability_reason.unwrap();
        assert!(capability_reason.contains("38;2;1;2;3"), "{capability_reason}");
    }

    #[test]
    fn no_color_equals_zero_still_suppresses_colour() {
        let subject = Attr::Color(ColorRole::Foreground, PROBE_PALETTE);
        let outcome = ProbeOutcome {
            interactive: true,
            preference: ColorPreference::from_value(Some("0")),
            measurements: vec![Measurement {
                subject,
                report: None,
            }],
            ..ProbeOutcome::default()
        };
        assert_eq!(outcome.finding(subject).support, Support::No);
        assert_eq!(outcome.finding(subject).basis, Basis::Preference);
    }

    #[test]
    fn an_empty_no_color_does_not_suppress_colour() {
        let subject = Attr::Color(ColorRole::Foreground, PROBE_RGB);
        let outcome = ProbeOutcome {
            interactive: true,
            preference: ColorPreference::from_value(Some("")),
            measurements: vec![Measurement {
                subject,
                report: Some(SgrReport::Reported("0;38;2;1;2;3".to_owned())),
            }],
            ..ProbeOutcome::default()
        };
        let finding = outcome.finding(subject);
        assert_eq!(finding.support, Support::Yes);
        assert_eq!(finding.basis, Basis::Measured);
        assert_eq!(finding.capability, None);
    }

    #[test]
    fn no_color_leaves_everything_that_is_not_a_colour_alone() {
        // The variable is about colour, not styling: a curly underline is a shape.
        let outcome = ProbeOutcome {
            interactive: true,
            preference: ColorPreference::from_value(Some("1")),
            measurements: vec![Measurement {
                subject: Attr::Underline(UnderlineShape::Curly),
                report: Some(SgrReport::Reported("0;4:3".to_owned())),
            }],
            ..ProbeOutcome::default()
        };
        let finding = outcome.finding(Attr::Underline(UnderlineShape::Curly));
        assert_eq!(finding.support, Support::Yes);
        assert_eq!(finding.basis, Basis::Measured);
        assert_ne!(finding.basis, Basis::Preference);
    }

    #[test]
    fn a_terminal_that_cannot_do_the_colour_says_so_under_no_color_too() {
        // Both facts at once: your environment said no, and your terminal could not have
        // done it anyway.
        let subject = Attr::Color(ColorRole::Foreground, PROBE_RGB);
        let outcome = ProbeOutcome {
            interactive: true,
            preference: ColorPreference::from_value(Some("1")),
            measurements: vec![Measurement {
                subject,
                report: Some(SgrReport::Reported("0;38;5;16".to_owned())),
            }],
            ..ProbeOutcome::default()
        };
        let finding = outcome.finding(subject);
        assert_eq!(finding.support, Support::No);
        assert_eq!(finding.basis, Basis::Preference);
        assert_eq!(finding.capability, Some(Support::No));
        assert!(
            finding
                .capability_reason
                .as_deref()
                .unwrap()
                .contains("palette entry 16")
        );
    }

    // -- the exit status ----------------------------------------------------

    #[test]
    fn yes_requires_every_attribute_asked_about_to_be_supported() {
        let outcome = measured(&[
            (Attr::Italic, Some(SgrReport::Reported("0;3".to_owned()))),
            (Attr::Overline, Some(SgrReport::Reported("0;53".to_owned()))),
        ]);
        assert_eq!(outcome.support(), Support::Yes);
    }

    #[test]
    fn one_definite_no_makes_the_whole_answer_no() {
        let outcome = measured(&[
            (Attr::Italic, Some(SgrReport::Reported("0;3".to_owned()))),
            (Attr::Overline, Some(SgrReport::Reported("0".to_owned()))),
        ]);
        assert_eq!(outcome.support(), Support::No);
    }

    #[test]
    fn a_definite_no_beats_an_unknown_because_it_is_more_actionable() {
        let outcome = measured(&[
            (Attr::Overline, Some(SgrReport::Reported("0".to_owned()))),
            (Attr::Conceal, None),
        ]);
        assert_eq!(outcome.support(), Support::No);
    }

    #[test]
    fn one_unknown_and_no_failures_is_unknown() {
        let outcome = measured(&[
            (Attr::Italic, Some(SgrReport::Reported("0;3".to_owned()))),
            (Attr::Conceal, None),
        ]);
        assert_eq!(outcome.support(), Support::Unknown);
    }

    #[test]
    fn nothing_asked_about_is_unknown_rather_than_a_vacuous_yes() {
        assert_eq!(ProbeOutcome::default().support(), Support::Unknown);
    }

    #[test]
    fn the_three_exit_statuses_are_distinct_and_documented() {
        assert_eq!(exit_status_number(Support::Yes), 0);
        assert_eq!(exit_status_number(Support::No), 1);
        assert_eq!(exit_status_number(Support::Unknown), 2);
        assert_ne!(
            format!("{:?}", Support::No.exit_code()),
            format!("{:?}", Support::Unknown.exit_code()),
            "inconclusive must not be indistinguishable from unsupported"
        );
    }

    // -- output -------------------------------------------------------------

    #[test]
    fn the_json_is_one_line_and_carries_no_control_bytes() {
        let plan = every_attribute();
        let mut outcome = in_band("kitty 0.36.0", &plan);
        outcome.replies = vec![Reply::Dcs("1$r0;4:3m".to_owned())];
        let line = json_line(&outcome);
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(!line.contains('\u{1b}'));
        assert!(line.contains("\"feature\":\"text-attributes\""));
        assert!(line.contains("\"no_color\":{\"set\":false,\"value\":null}"));
        for attr in &plan {
            assert!(
                line.contains(&format!("\"attribute\":\"{}\"", attr.label())),
                "{}",
                attr.label()
            );
        }
        // Every entry carries a reason and a machine-readable code for it.
        assert_eq!(
            line.matches("\"reason_code\":").count(),
            plan.len(),
            "every attribute needs a reason code"
        );
    }

    #[test]
    fn the_json_records_the_preference_and_the_capability_separately() {
        let subject = Attr::Color(ColorRole::Foreground, PROBE_RGB);
        let outcome = ProbeOutcome {
            interactive: true,
            preference: ColorPreference::from_value(Some("1")),
            measurements: vec![Measurement {
                subject,
                report: Some(SgrReport::Reported("0;38;2;1;2;3".to_owned())),
            }],
            ..ProbeOutcome::default()
        };
        let line = json_line(&outcome);
        assert!(line.contains("\"no_color\":{\"set\":true,\"value\":\"1\"}"), "{line}");
        assert!(line.contains("\"verdict\":\"no\""), "{line}");
        assert!(line.contains("\"reason_code\":\"preference\""), "{line}");
        assert!(line.contains("\"capability\":\"yes\""), "{line}");
    }

    #[test]
    fn hostile_environment_values_cannot_escape_the_json() {
        assert_eq!(json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_string("a\u{1b}b"), "\"a\\u001bb\"");
        assert_eq!(json_string("a\nb\tc"), "\"a\\nb\\tc\"");

        let outcome = ProbeOutcome {
            evidence: vec![Evidence {
                source: "TERM_PROGRAM",
                value: "evil\u{1b}[2J\"".to_owned(),
                implies: None,
                in_band: false,
                caveat: None,
            }],
            preference: ColorPreference::from_value(Some("\u{1b}[2J")),
            ..ProbeOutcome::default()
        };
        let line = json_line(&outcome);
        assert!(!line.contains('\u{1b}'), "{line}");
    }

    #[test]
    fn replies_render_without_control_bytes() {
        for reply in [
            Reply::Dcs("1$r0;4:3m".to_owned()),
            Reply::Dcs(">|kitty 0.36.0".to_owned()),
            Reply::Csi {
                params: "?62;4".to_owned(),
                final_byte: 'c',
            },
            // A hostile reply is still just input.
            Reply::Osc("evil\u{1b}[2J".to_owned()),
        ] {
            let rendered = describe_reply(&reply);
            assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        }
    }

    #[test]
    fn truncation_keeps_the_width_it_promises() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789", 5).chars().count(), 5);
        assert!(truncate("0123456789", 5).ends_with('\u{2026}'));
    }

    #[test]
    fn every_basis_has_a_stable_word() {
        let words: Vec<&str> = [
            Basis::Standard,
            Basis::Measured,
            Basis::Inferred,
            Basis::Preference,
            Basis::Nothing,
        ]
        .iter()
        .map(|basis| basis.as_str())
        .collect();
        assert_eq!(
            words,
            vec!["standard", "measured", "inferred", "preference", "nothing"]
        );
    }

    // -- identity -----------------------------------------------------------

    #[test]
    fn self_reported_names_map_onto_registry_ids() {
        for (name, expected) in [
            ("WezTerm 20240203-110809", "wezterm"),
            ("ghostty 1.2.0", "ghostty"),
            ("kitty(0.36.0)", "kitty"),
            ("foot(1.16.2)", "foot"),
            ("tmux 3.7", "tmux"),
            ("XTerm(389)", "xterm"),
        ] {
            assert_eq!(registry_id_for(name), Some(expected), "{name}");
        }
        assert_eq!(registry_id_for("SomeNewTerminal 1.0"), None);
    }

    #[test]
    fn generic_term_values_identify_nothing() {
        for term in ["xterm-256color", "XTERM-256COLOR", "ansi", "dumb", "linux"] {
            assert_eq!(id_from_term(term), None, "{term}");
        }
        assert_eq!(id_from_term("xterm-kitty"), Some("kitty"));
        assert_eq!(id_from_term("xterm-ghostty"), Some("ghostty"));
    }

    #[test]
    fn every_id_the_tables_can_produce_exists_in_the_registry() {
        let catalog = Catalog::load().unwrap();
        for (key, id) in ENV_MARKERS {
            assert!(
                catalog.terminal(id).is_some(),
                "{key} → {id} is not in terminals.toml"
            );
        }
    }

    #[test]
    fn every_identified_terminal_has_a_support_row() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        for (key, id) in ENV_MARKERS {
            assert!(
                feature.support.iter().any(|row| row.terminal == *id),
                "{key} identifies {id}, which has no row in text-attributes.toml"
            );
        }
    }

    #[test]
    fn versions_are_compared_numerically_not_textually() {
        assert!(version_at_least(&[0, 10], &[0, 9]));
        assert!(version_at_least(&[1, 0], &[0, 52]));
        assert!(version_at_least(&[0, 52, 0], &[0, 52]));
        assert!(!version_at_least(&[0, 51], &[0, 52]));
    }

    #[test]
    fn versions_are_pulled_out_of_however_a_terminal_writes_itself_down() {
        assert_eq!(parse_version("kitty 0.36.0"), Some(vec![0, 36, 0]));
        assert_eq!(parse_version("foot(1.16.2)"), Some(vec![1, 16, 2]));
        assert_eq!(parse_version("XTerm(389)"), Some(vec![389]));
        assert_eq!(parse_version("no digits here"), None);
    }
}
