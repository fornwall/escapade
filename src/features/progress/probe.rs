//! Inference, not measurement — and this time there is not even a question to
//! ask.
//!
//! `OSC 9 ; 4` is **write-only**. There is no query, no reply, no acknowledgement
//! and no observable side effect: the indicator it moves is in the taskbar, the
//! dock or the tab strip, not in the grid, so nothing about the terminal's screen
//! changes and nothing comes back. A progress report that worked and a progress
//! report that was discarded are byte-for-byte identical from the sending end.
//!
//! Inventing a verdict would be worse than saying so, so this probe does three
//! honest things instead:
//!
//! 1. Asks the questions that *do* have answers — XTVERSION (`CSI > q`) behind the
//!    Primary Device Attributes barrier
//!    [`crate::features::glyph_protocol::probe`] uses, so a silent terminal can be
//!    told apart from a slow one. Every reply it provokes is drained, because an
//!    unread one is delivered to whatever runs next and printed as garbage.
//! 2. Collects the rest of the evidence: `TERM`, `TERM_PROGRAM` and the
//!    per-terminal environment markers.
//! 3. Looks the identified terminal up in the support matrix.
//!
//! The verdict is deliberately stingy. [`crate::features::Support::Yes`] requires
//! the terminal to have **named itself**, in band, in its XTVERSION reply — an
//! environment variable is somebody else's claim about what is at the other end of
//! the pipe, and after an `ssh` hop or a `sudo -E` it is frequently a stale one.
//! Everything resting on the environment alone comes back as
//! [`crate::features::Support::Unknown`] with a reason saying so.

use std::{
    env,
    io::{IsTerminal as _, stdin, stdout},
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    cli::ProbeFormat,
    db::{Catalog, SupportStatus},
    features::{
        Support,
        progress::{
            FEATURE_ID,
            sequence::{Percent, Report},
        },
        wrap,
    },
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// How long to wait for the terminal to answer.
///
/// A full second: generous enough for a slow ssh link or a loaded machine, and
/// this probe is not on anybody's hot path. Under the barrier the wait almost
/// always ends early, as soon as the device-attributes reply arrives.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to spend consuming a reply that is still in flight after a
/// conclusion has been reached.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// XTVERSION: `CSI > q`. A terminal that implements it answers
/// `DCS > | <name and version> ST`; one that does not stays quiet.
#[must_use]
pub fn version_query() -> String {
    format!("{CSI}>q")
}

/// Primary Device Attributes: `CSI c`. The barrier — every terminal since the
/// VT100 answers it, so its reply proves the round trip completed.
#[must_use]
pub fn barrier_query() -> String {
    format!("{CSI}c")
}

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

/// One thing the probe learned, and what it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// Where it came from: `XTVERSION`, `TERM`, or an environment variable name.
    pub source: &'static str,
    /// What that source said.
    pub value: String,
    /// The registry id it identifies, when it identifies one.
    pub implies: Option<&'static str>,
    /// Whether this came from the terminal itself rather than from the
    /// environment.
    ///
    /// The distinction the whole verdict turns on. `TERM_PROGRAM` is set by
    /// whatever launched the shell and is inherited by every child, including one
    /// on the far side of an `ssh` hop; an XTVERSION reply is the terminal that is
    /// actually there, answering for itself.
    pub in_band: bool,
    /// Why it does not identify anything, when it does not.
    pub caveat: Option<&'static str>,
}

/// Environment variables that identify a terminal by their mere presence.
///
/// Note `ConEmuPID` and `ConEmuANSI`: ConEmu invented this sequence, so its own
/// marker belongs at the top of any table about it. `WT_SESSION` matters for the
/// same reason — Windows Terminal is where most people meet the feature.
const ENV_MARKERS: &[(&str, &str)] = &[
    ("ConEmuPID", "conemu"),
    ("ConEmuANSI", "conemu"),
    ("WT_SESSION", "windows-terminal"),
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
    ("MINTTY_SHORTCUT", "mintty"),
    // VTE exports its version, which is how every VTE-based terminal announces
    // itself. For this feature the version is the whole question: VTE learned to
    // parse OSC 9 ; 4 in 0.80.
    ("VTE_VERSION", "gnome-terminal"),
];

/// Maps a terminal's self-reported name onto a registry id.
///
/// A case-insensitive substring search, most specific first, so `xterm-kitty`
/// reaches `kitty` before it reaches `xterm`.
#[must_use]
pub fn registry_id_for(name: &str) -> Option<&'static str> {
    /// `(needle, registry id)`, most specific first.
    const TABLE: &[(&str, &str)] = &[
        ("conemu", "conemu"),
        ("kitty", "kitty"),
        ("ghostty", "ghostty"),
        ("wezterm", "wezterm"),
        ("mintty", "mintty"),
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
        // Multiplexers before xterm: `screen.xterm-256color` is Screen. For this
        // feature the multiplexer is a real participant — it has to forward the
        // sequence — so identifying one is a result rather than a nuisance.
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
/// `xterm-256color` is what almost every terminal sets for terminfo
/// compatibility, so reading it as "this is xterm" would report a confident no on
/// terminals that show progress perfectly well.
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

/// Strips XTVERSION's `>|` prefix from a DCS payload.
///
/// A reply looks like `DCS > | mintty 3.7.7 ST`, and [`crate::input`] hands over
/// the payload between `ESC P` and the terminator, so the `>|` is still on the
/// front.
#[must_use]
pub fn parse_xtversion(payload: &str) -> Option<String> {
    let body = payload.strip_prefix(">|")?;
    let trimmed = body.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Pulls a dotted version number out of a terminal's self-description.
///
/// Terminals write themselves down in their own style — `mintty 3.7.7`,
/// `kitty 0.36.0`, `foot(1.16.2)`, `WezTerm 20240203-110809`, `XTerm(389)` — so
/// this takes the first run of digits-and-dots and splits it on the dots. The
/// components are returned as they were written, with no assumption about how many
/// there are.
///
/// This exists because support for this feature is **version-gated everywhere**:
/// Windows Terminal 1.6, mintty 3.4.2, Ghostty 1.2, VTE 0.80, tmux 3.7. A name
/// alone is not enough to answer the question.
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
            components.push(current.unwrap_or(0));
            current = None;
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
/// `1.6` and `1.6.0` are the same version, and `1.10` is later than `1.9` — which
/// is why this is not a string comparison.
#[must_use]
pub fn version_at_least(found: &[u64], wanted: &[u64]) -> bool {
    let length = found.len().max(wanted.len());
    for index in 0..length {
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
/// Pure, so the identification logic is testable without a terminal. `xtversion`
/// is threaded in rather than read here because it takes a round trip to get.
#[must_use]
pub fn collect_evidence(xtversion: Option<&str>) -> Vec<Evidence> {
    let mut evidence = Vec::new();

    if let Some(name) = xtversion {
        evidence.push(Evidence {
            source: "XTVERSION",
            value: name.to_owned(),
            implies: registry_id_for(name),
            in_band: true,
            caveat: if registry_id_for(name).is_none() {
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
// The outcome
// ---------------------------------------------------------------------------

/// What the probe concluded, and everything it concluded it from.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// Whether stdin and stdout are both terminals. Without that, nothing can be
    /// asked — though progress reports themselves travel down a pipe fine.
    pub interactive: bool,
    /// Multiplexer in the way, if any.
    pub multiplexer: Option<Multiplexer>,
    /// The terminal's XTVERSION self-description, if it answered.
    pub xtversion: Option<String>,
    /// Whether the device-attributes barrier came back.
    pub answered_barrier: bool,
    /// Every piece of evidence, in the order it was consulted.
    pub evidence: Vec<Evidence>,
    /// The registry id the evidence settled on.
    pub identified: Option<&'static str>,
    /// Display name of the identified terminal.
    pub terminal_name: Option<String>,
    /// What the support matrix says about it.
    pub status: Option<SupportStatus>,
    /// The version support arrived in, per the matrix.
    pub since: Option<String>,
    /// The matrix's caveats for this terminal.
    pub notes: Option<String>,
    /// Everything the terminal said, in order.
    pub replies: Vec<Reply>,
}

impl ProbeOutcome {
    /// The first piece of evidence that both identified something and came from
    /// the terminal itself.
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

    /// The version the matrix says support arrived in, if it names one.
    #[must_use]
    pub fn required_version(&self) -> Option<Vec<u64>> {
        self.since.as_deref().and_then(parse_version)
    }

    /// Whether the reported version is old enough to predate support.
    ///
    /// Only ever consulted when both versions are known: an absent version is not
    /// evidence of an old one.
    #[must_use]
    pub fn predates_support(&self) -> bool {
        match (self.reported_version(), self.required_version()) {
            (Some(found), Some(wanted)) => !version_at_least(&found, &wanted),
            _ => false,
        }
    }

    /// The answer, as the exit status will report it.
    ///
    /// `Yes` demands two things at once: the terminal named itself in band, and
    /// the matrix row for that name says the feature works. Anything less is
    /// `Unknown`, because this protocol cannot be measured and an environment
    /// variable is not the terminal talking.
    ///
    /// `No` is claimed in exactly two situations, both of which rest on the
    /// terminal's own words: a matrix row that says the terminal does not
    /// implement it, and a self-reported version older than the one that gained
    /// it.
    #[must_use]
    pub fn support(&self) -> Support {
        let Some(_) = self.in_band_evidence() else {
            return Support::Unknown;
        };
        if self.predates_support() {
            return Support::No;
        }
        match self.status {
            // `partial` is still a yes: Konsole shows the states it implements and
            // ignores the rest, which is a working indicator with gaps, and the
            // gaps are in `notes` for anyone who cares.
            Some(SupportStatus::Yes | SupportStatus::Partial) => Support::Yes,
            Some(SupportStatus::No) => Support::No,
            // Behind a flag, or a multiplexer that has to be configured to
            // forward it: whether it works here is exactly what we cannot see.
            Some(SupportStatus::Flag | SupportStatus::Passthrough | SupportStatus::Unknown)
            | None => Support::Unknown,
        }
    }

    /// Why the verdict is what it is, in one sentence, for `--format debug` and
    /// for the JSON.
    #[must_use]
    pub fn reason(&self) -> String {
        if !self.interactive {
            return "stdout and stdin are not both terminals, so the terminal could not be asked \
                    to name itself"
                .to_owned();
        }
        let Some(evidence) = self.in_band_evidence() else {
            return match self.deciding_evidence() {
                Some(evidence) => format!(
                    "the terminal did not name itself; {}={} points at {} but that is the \
                     environment's claim, not the terminal's",
                    evidence.source,
                    evidence.value,
                    evidence.implies.unwrap_or("nothing")
                ),
                None => "nothing identified this terminal, and this protocol has no query to fall \
                     back on"
                    .to_owned(),
            };
        };
        let name = self
            .terminal_name
            .clone()
            .unwrap_or_else(|| evidence.value.clone());
        if self.predates_support() {
            return format!(
                "{name} reported itself as {}, and the matrix says progress arrived in {}",
                evidence.value,
                self.since.clone().unwrap_or_default()
            );
        }
        match self.status {
            Some(status) => format!(
                "{name} named itself in its XTVERSION reply, and the matrix records support as {}",
                status.label()
            ),
            None => format!("{name} named itself, but the matrix has no row for it"),
        }
    }

    /// Whether the demo should run without `--force`.
    ///
    /// Deliberately generous, and for the same reason the hyperlink demo is: a
    /// progress report that is not implemented is discarded silently, so refusing
    /// to demonstrate it on an unidentified terminal would withhold something
    /// harmless. Only a positive `No` refuses. A terminal that is not a terminal
    /// is a different matter — there is nothing to draw on and raw mode would
    /// fail.
    #[must_use]
    pub fn looks_supported(&self) -> bool {
        self.interactive && self.support() != Support::No
    }
}

// ---------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------

/// Runs the probe using an existing terminal and reader.
///
/// Takes both rather than creating them because there must only ever be one
/// [`InputReader`] per process — two would race for the same bytes on stdin.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal that
/// stays quiet is not an error; for this protocol there was never much to hear.
pub fn probe(catalog: &Catalog, term: &mut Term, input: &mut InputReader) -> Result<ProbeOutcome> {
    let mut outcome = ProbeOutcome {
        multiplexer: Multiplexer::detect(),
        interactive: stdout().is_terminal() && stdin().is_terminal(),
        ..ProbeOutcome::default()
    };

    if outcome.interactive {
        // Raw mode is required: in cooked mode the reply would sit in the line
        // buffer until the user pressed Enter, and would be echoed at them.
        term.enable_raw()?;
        input.drain();

        term.write_now(&version_query())?;
        // The barrier is deliberately not wrapped for a multiplexer: the question
        // is whether the round trip completed, and a multiplexer answers device
        // attributes from its own emulator rather than forwarding it — which is
        // also why a barrier reply from inside one licenses no negative verdict
        // about the terminal beyond it.
        term.write_now(&barrier_query())?;

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
                    }
                }
                Reply::Csi {
                    final_byte: 'c', ..
                } => {
                    // Everything sent before the barrier has been processed, so
                    // waiting longer would learn nothing.
                    outcome.answered_barrier = true;
                    break;
                }
                _ => {}
            }
        }

        // XTVERSION can arrive first and end the wait, leaving the barrier reply
        // in flight. Unread, it lands in whatever runs next — the user's shell,
        // which prints it as stray text. Consuming it is what keeps probing
        // invisible, which matters more here than anywhere else: this probe is
        // silent by design.
        if !outcome.answered_barrier
            && let Ok(Some(reply)) = input.wait_for_reply(DRAIN_TIMEOUT, |reply| {
                matches!(
                    reply,
                    Reply::Csi {
                        final_byte: 'c',
                        ..
                    }
                )
            })
        {
            outcome.replies.push(reply);
            outcome.answered_barrier = true;
        }
    }

    conclude(&mut outcome, catalog);
    Ok(outcome)
}

/// Turns the evidence into an identity, and the identity into a matrix row.
///
/// The last step of every probe path, kept in one place so that the piped case and
/// the interactive case cannot disagree about what the evidence meant.
fn conclude(outcome: &mut ProbeOutcome, catalog: &Catalog) {
    outcome.evidence = collect_evidence(outcome.xtversion.as_deref());
    outcome.identified = outcome.deciding_evidence().and_then(|item| item.implies);

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

/// Runs a probe from scratch.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn detect(catalog: &Catalog) -> Result<ProbeOutcome> {
    if !(stdout().is_terminal() && stdin().is_terminal()) {
        // Nothing to ask, and spawning a reader thread on a closed stdin would be
        // work for no answer. The environment is still worth reading: it cannot
        // license a `yes`, but it can tell a reader which terminal we think this is.
        let mut outcome = ProbeOutcome {
            multiplexer: Multiplexer::detect(),
            interactive: false,
            ..ProbeOutcome::default()
        };
        conclude(&mut outcome, catalog);
        return Ok(outcome);
    }
    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let outcome = probe(catalog, &mut term, &mut input);
    // Restore before the caller prints anything, so any report appears in a
    // normal cooked-mode terminal.
    term.restore();
    outcome
}

/// Runs `escapade progress probe`, returning the exit status that is the answer.
///
/// Silent unless a `--format` was asked for: `0` supported, `1` not supported,
/// `2` cannot tell. That makes it usable as a shell predicate, which for this
/// feature is the natural thing to want — a script that is about to emit progress
/// reports in a loop wants to know once, at the top, and never mention it again.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn report(theme: &Theme, catalog: &Catalog, format: Option<ProbeFormat>) -> Result<ExitCode> {
    let outcome = detect(catalog)?;
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

/// One JSON object, one line — the house machine-readable shape.
///
/// Hand-written, like the other probes': the shape is fixed, the job is one
/// function, and `escapade` keeps its "no dependency you cannot read" property by
/// not adding one for this.
///
/// The extra field here is `measurable`, which is always `false`. A script reading
/// this needs to know that `"verdict":"unknown"` is the *normal* answer for this
/// feature rather than a probe that went wrong — OSC 9 ; 4 has no query, so there
/// is nothing to measure and the verdict can only ever be an inference.
#[must_use]
pub fn json_line(outcome: &ProbeOutcome) -> String {
    let evidence: Vec<String> = outcome
        .evidence
        .iter()
        .map(|item| {
            format!(
                "{{\"source\":\"{}\",\"value\":\"{}\",\"implies\":{},\"in_band\":{}}}",
                escape_json(item.source),
                escape_json(&item.value),
                quoted_or_null(item.implies),
                item.in_band
            )
        })
        .collect();
    format!(
        "{{\"feature\":\"{FEATURE_ID}\",\"verdict\":\"{}\",\"measurable\":false,\
         \"reason\":\"{}\",\"terminal\":{},\"xtversion\":{},\"matrix_status\":{},\
         \"since\":{},\"answered_barrier\":{},\"multiplexer\":{},\"interactive\":{},\
         \"evidence\":[{}]}}\n",
        outcome.support().as_str(),
        escape_json(&outcome.reason()),
        quoted_or_null(outcome.identified),
        quoted_or_null(outcome.xtversion.as_deref()),
        quoted_or_null(outcome.status.map(SupportStatus::label)),
        quoted_or_null(outcome.since.as_deref()),
        outcome.answered_barrier,
        quoted_or_null(outcome.multiplexer.map(Multiplexer::name)),
        outcome.interactive,
        evidence.join(",")
    )
}

/// A JSON string, or `null` when there is nothing to say.
///
/// Absent is not the same as empty here: `"xtversion":null` means the terminal
/// never named itself, which is the difference between an inference and a
/// measurement.
fn quoted_or_null(value: Option<&str>) -> String {
    value.map_or_else(
        || "null".to_owned(),
        |text| format!("\"{}\"", escape_json(text)),
    )
}

/// Minimal JSON string escaping, for the few strings that reach the output.
///
/// Environment variables are user data — `TERM_PROGRAM` can hold a quote, a
/// backslash or an escape byte — so this escapes rather than hopes, and control
/// characters become `\uXXXX`. That is also what stops a hostile variable from
/// carrying a raw `ESC` into the reader's terminal by way of our own output.
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

// ---------------------------------------------------------------------------
// `--format debug`
// ---------------------------------------------------------------------------

/// Prints everything the probe saw and concluded, for a person.
#[expect(
    clippy::too_many_lines,
    reason = "one screen of output, written top to bottom; splitting it would \
              scatter the layout across functions without simplifying anything"
)]
fn print_debug(theme: &Theme, outcome: &ProbeOutcome) {
    let support = outcome.support();

    println!();
    println!(
        "  {}",
        theme.paint("Taskbar progress support (OSC 9 ; 4)", Ink::Title)
    );
    println!();

    for line in wrap(
        "This one cannot be measured. OSC 9 ; 4 is write-only: no query, no reply, no \
         acknowledgement, and the thing it changes is outside the grid, so there is nothing to \
         observe either. What follows is inference — and a verdict of yes is only claimed when the \
         terminal named itself in band, because an environment variable survives an ssh hop and a \
         terminal does not.",
        theme.width().saturating_sub(4),
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();

    let (symbol, ink) = match support {
        Support::Yes => ("✔", Ink::Good),
        Support::No => ("✘", Ink::Bad),
        Support::Unknown => ("?", Ink::Warn),
    };
    println!(
        "  {} {}  {}",
        theme.paint(symbol, ink),
        theme.paint(support.as_str(), ink),
        theme.paint(
            &format!("(exit status {})", exit_status_number(support)),
            Ink::Muted
        )
    );
    for line in wrap(&outcome.reason(), theme.width().saturating_sub(6)) {
        println!("      {}", theme.paint(&line, Ink::Muted));
    }
    println!();

    println!("  {}", theme.paint("what the terminal said", Ink::Accent));
    if outcome.interactive {
        for (sequence, meaning) in [
            (version_query(), "XTVERSION — who are you?"),
            (barrier_query(), "the barrier, which every terminal answers"),
        ] {
            println!(
                "    {}  {}",
                theme.paint(&format!("{:<10}", pretty(&sequence)), Ink::Code),
                theme.paint(meaning, Ink::Muted)
            );
        }
        if outcome.replies.is_empty() {
            println!("    {}", theme.paint("(nothing came back)", Ink::Muted));
        }
        for reply in &outcome.replies {
            println!("    {}", theme.paint(&describe_reply(reply), Ink::Code));
        }
        if let Some(multiplexer) = outcome.multiplexer {
            for line in wrap(
                &format!(
                    "{} answers device attributes from its own emulator rather than forwarding \
                     it, so a barrier reply from in here says nothing about the terminal beyond \
                     it.",
                    multiplexer.name()
                ),
                theme.width().saturating_sub(6),
            ) {
                println!("      {}", theme.paint(&line, Ink::Muted));
            }
        }
    } else {
        // Nothing was sent, so claiming "nothing came back" would be a lie about
        // a question that was never asked.
        println!(
            "    {}",
            theme.paint(
                "nothing was asked — there is no terminal on both ends",
                Ink::Muted
            )
        );
    }
    println!();

    println!(
        "  {}",
        theme.paint("what the environment says", Ink::Accent)
    );
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
            Some(id) => theme.paint(&format!("→ {id}"), Ink::Good),
            None => theme.paint("→ nothing", Ink::Muted),
        };
        let band = if item.in_band {
            theme.paint("[in band]", Ink::Accent)
        } else {
            theme.paint("[ambient]", Ink::Muted)
        };
        println!(
            "    {}  {}  {arrow}  {band}",
            theme.paint(&format!("{:<width$}", item.source), Ink::Accent),
            theme.paint(&format!("{:<32}", truncate(&item.value, 32)), Ink::Code),
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
            if let Some(found) = outcome.reported_version() {
                let numbers: Vec<String> = found.iter().map(u64::to_string).collect();
                let verdict = if outcome.predates_support() {
                    theme.paint("older than that", Ink::Bad)
                } else if outcome.required_version().is_some() {
                    theme.paint("new enough", Ink::Good)
                } else {
                    theme.paint("no version to compare against", Ink::Muted)
                };
                println!(
                    "    {} {}  {verdict}",
                    theme.paint("this build:", Ink::Muted),
                    theme.paint(&numbers.join("."), Ink::Code)
                );
            }
            if let Some(notes) = &outcome.notes {
                println!("{}", theme.markdown_indented(notes, 4));
            }
        }
        _ => {
            for line in wrap(
                "Nothing identified this terminal, so the matrix has nothing to say about it. That \
                 is not a no — it is an absence of evidence, and since an unrecognised OSC string \
                 is discarded silently, the right response is to send the progress report anyway.",
                theme.width().saturating_sub(6),
            ) {
                println!("      {}", theme.paint(&line, Ink::Muted));
            }
        }
    }

    println!();
    println!("  {}", theme.paint("what would be sent", Ink::Accent));
    println!(
        "    {}  {}",
        theme.paint(&pretty(&Report::set(Percent::FULL).sequence()), Ink::Code),
        theme.paint("no query precedes it, because there is none", Ink::Muted)
    );
    println!();
    for line in wrap(
        "Send them anyway. A terminal without this sequence consumes the OSC string and discards \
         it, so the cost of being wrong is nothing at all — which is why this probe is worth \
         reading but rarely worth branching on. What is worth doing unconditionally is sending \
         state 0 on the way out, so a job that dies does not leave a taskbar stuck at 60%.",
        theme.width().saturating_sub(4),
    ) {
        println!("    {}", theme.paint(&line, Ink::Muted));
    }
    println!();
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
    match reply {
        Reply::Apc(payload) => format!("APC {payload} ST"),
        Reply::Dcs(payload) => format!("DCS {payload} ST"),
        Reply::Osc(payload) => format!("OSC {payload} ST"),
        Reply::Csi { params, final_byte } => format!("CSI {params}{final_byte}"),
    }
}

/// Shortens `text` to `limit` characters, with an ellipsis when it had to.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests, but the attribute is kept for the
// helpers in here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// An outcome that was told `name` by the terminal itself.
    fn in_band(name: &str, status: Option<SupportStatus>, since: Option<&str>) -> ProbeOutcome {
        let evidence = vec![Evidence {
            source: "XTVERSION",
            value: name.to_owned(),
            implies: registry_id_for(name),
            in_band: true,
            caveat: None,
        }];
        ProbeOutcome {
            interactive: true,
            xtversion: Some(name.to_owned()),
            answered_barrier: true,
            identified: registry_id_for(name),
            terminal_name: Some(name.to_owned()),
            status,
            since: since.map(str::to_owned),
            evidence,
            ..ProbeOutcome::default()
        }
    }

    /// An outcome that only has the environment's word for it.
    fn ambient(id: &'static str, status: Option<SupportStatus>) -> ProbeOutcome {
        ProbeOutcome {
            interactive: true,
            answered_barrier: true,
            identified: Some(id),
            terminal_name: Some(id.to_owned()),
            status,
            evidence: vec![Evidence {
                source: "TERM_PROGRAM",
                value: id.to_owned(),
                implies: Some(id),
                in_band: false,
                caveat: None,
            }],
            ..ProbeOutcome::default()
        }
    }

    // -- queries ------------------------------------------------------------

    #[test]
    fn the_queries_are_the_documented_ones() {
        assert_eq!(version_query(), "\u{1b}[>q");
        assert_eq!(barrier_query(), "\u{1b}[c");
    }

    #[test]
    fn xtversion_replies_lose_their_prefix() {
        assert_eq!(
            parse_xtversion(">|mintty 3.7.7").as_deref(),
            Some("mintty 3.7.7")
        );
        assert_eq!(parse_xtversion("1$r0m"), None);
        assert_eq!(parse_xtversion(">|"), None);
        assert_eq!(parse_xtversion(""), None);
    }

    // -- identification -----------------------------------------------------

    #[test]
    fn self_reported_names_map_onto_registry_ids() {
        for (name, expected) in [
            ("mintty 3.7.7", "mintty"),
            ("ConEmu 210912", "conemu"),
            ("WezTerm 20240203-110809", "wezterm"),
            ("ghostty 1.2.0", "ghostty"),
            ("kitty(0.36.0)", "kitty"),
            ("Windows Terminal 1.19", "windows-terminal"),
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
        assert_eq!(id_from_term("rio"), Some("rio"));
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
        for name in [
            "conemu",
            "kitty",
            "ghostty",
            "wezterm",
            "mintty",
            "alacritty",
            "contour",
            "iterm2",
            "konsole",
            "vscode",
            "windows terminal",
            "gnome",
            "vte",
            "foot",
            "rio",
            "tmux",
            "zellij",
            "screen",
            "xterm",
        ] {
            let id = registry_id_for(name).unwrap_or_else(|| panic!("{name} matched nothing"));
            assert!(
                catalog.terminal(id).is_some(),
                "{id} is not in terminals.toml"
            );
        }
    }

    #[test]
    fn every_identifiable_terminal_has_a_support_row() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        for (_, id) in ENV_MARKERS {
            assert!(
                feature.support.iter().any(|row| row.terminal == *id),
                "no progress support row for {id}"
            );
        }
    }

    #[test]
    fn in_band_evidence_is_distinguished_from_ambient_evidence() {
        let outcome = in_band("kitty(0.36.0)", Some(SupportStatus::No), None);
        assert_eq!(
            outcome.in_band_evidence().map(|item| item.source),
            Some("XTVERSION")
        );
        let outcome = ambient("kitty", Some(SupportStatus::Yes));
        assert!(outcome.in_band_evidence().is_none());
        assert_eq!(
            outcome.deciding_evidence().map(|item| item.source),
            Some("TERM_PROGRAM")
        );
    }

    // -- versions -----------------------------------------------------------

    #[test]
    fn versions_are_pulled_out_of_however_the_terminal_wrote_them() {
        assert_eq!(parse_version("mintty 3.7.7"), Some(vec![3, 7, 7]));
        assert_eq!(parse_version("foot(1.16.2)"), Some(vec![1, 16, 2]));
        assert_eq!(parse_version("XTerm(389)"), Some(vec![389]));
        assert_eq!(parse_version("1.6.10571.0"), Some(vec![1, 6, 10571, 0]));
        assert_eq!(parse_version("0.80"), Some(vec![0, 80]));
        // A date-stamped build is a single big number, which is honest: it cannot
        // be compared against a dotted version, and `required_version` will not
        // find one to compare it with.
        assert_eq!(
            parse_version("WezTerm 20240203-110809"),
            Some(vec![20_240_203])
        );
        assert_eq!(parse_version("no digits here"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn version_comparison_treats_missing_components_as_zero() {
        assert!(version_at_least(&[1, 6], &[1, 6]));
        assert!(version_at_least(&[1, 6, 0], &[1, 6]));
        assert!(version_at_least(&[1, 6], &[1, 6, 0]));
        assert!(version_at_least(&[1, 10], &[1, 9]), "not a string compare");
        assert!(version_at_least(&[2], &[1, 99]));
        assert!(!version_at_least(&[1, 5], &[1, 6]));
        assert!(!version_at_least(&[3, 4, 1], &[3, 4, 2]));
        assert!(version_at_least(&[3, 4, 2], &[3, 4, 2]));
    }

    #[test]
    fn a_version_older_than_the_matrix_row_is_a_definite_no() {
        // mintty gained progress in 3.4.2; 3.4.1 predates it.
        let old = in_band("mintty 3.4.1", Some(SupportStatus::Yes), Some("3.4.2"));
        assert!(old.predates_support());
        assert_eq!(old.support(), Support::No);
        assert!(old.reason().contains("3.4.2"), "{}", old.reason());

        let new = in_band("mintty 3.7.7", Some(SupportStatus::Yes), Some("3.4.2"));
        assert!(!new.predates_support());
        assert_eq!(new.support(), Support::Yes);
    }

    #[test]
    fn a_missing_version_is_not_evidence_of_an_old_one() {
        let no_version = in_band("someterm", Some(SupportStatus::Yes), Some("1.2"));
        assert!(!no_version.predates_support());
        let no_row_version = in_band("mintty 3.7.7", Some(SupportStatus::Yes), None);
        assert!(!no_row_version.predates_support());
    }

    // -- the verdict --------------------------------------------------------

    #[test]
    fn yes_requires_the_terminal_to_have_named_itself() {
        // The same terminal, the same matrix row, two kinds of evidence.
        assert_eq!(
            in_band("ghostty 1.2.0", Some(SupportStatus::Yes), Some("1.2.0")).support(),
            Support::Yes
        );
        let guessed = ambient("ghostty", Some(SupportStatus::Yes));
        assert_eq!(
            guessed.support(),
            Support::Unknown,
            "an environment variable is not the terminal talking"
        );
        assert!(
            guessed.reason().contains("environment"),
            "{}",
            guessed.reason()
        );
    }

    #[test]
    fn partial_support_still_counts_as_yes() {
        assert_eq!(
            in_band("konsole 22.12", Some(SupportStatus::Partial), None).support(),
            Support::Yes
        );
    }

    #[test]
    fn a_flag_or_a_multiplexer_leaves_it_unknown() {
        for status in [
            SupportStatus::Flag,
            SupportStatus::Passthrough,
            SupportStatus::Unknown,
        ] {
            assert_eq!(
                in_band("someterm 1.0", Some(status), None).support(),
                Support::Unknown,
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_matrix_no_from_an_in_band_name_is_a_no() {
        let outcome = in_band("kitty(0.36.0)", Some(SupportStatus::No), None);
        assert_eq!(outcome.support(), Support::No);
        assert!(
            outcome.reason().contains("XTVERSION"),
            "{}",
            outcome.reason()
        );
    }

    #[test]
    fn a_pipe_can_conclude_nothing() {
        let outcome = ProbeOutcome {
            interactive: false,
            identified: Some("kitty"),
            status: Some(SupportStatus::Yes),
            ..ProbeOutcome::default()
        };
        assert_eq!(outcome.support(), Support::Unknown);
        assert!(outcome.reason().contains("not both terminals"));
    }

    #[test]
    fn total_silence_is_unknown_rather_than_no() {
        let outcome = ProbeOutcome {
            interactive: true,
            evidence: Vec::new(),
            ..ProbeOutcome::default()
        };
        assert_eq!(outcome.support(), Support::Unknown);
        assert!(
            outcome.reason().contains("no query"),
            "{}",
            outcome.reason()
        );
    }

    #[test]
    fn the_exit_statuses_are_the_shared_ones() {
        assert_eq!(exit_status_number(Support::Yes), 0);
        assert_eq!(exit_status_number(Support::No), 1);
        assert_eq!(exit_status_number(Support::Unknown), 2);
        assert_eq!(
            format!("{:?}", Support::Yes.exit_code()),
            format!("{:?}", ExitCode::SUCCESS)
        );
    }

    #[test]
    fn the_demo_gate_only_refuses_on_a_positive_no() {
        assert!(in_band("mintty 3.7.7", Some(SupportStatus::Yes), None).looks_supported());
        assert!(
            ambient("nonesuch", None).looks_supported(),
            "an unidentified terminal gets the benefit of the doubt"
        );
        assert!(!in_band("alacritty 0.13", Some(SupportStatus::No), None).looks_supported());
        assert!(
            !ProbeOutcome::default().looks_supported(),
            "a pipe has no taskbar"
        );
    }

    // -- output -------------------------------------------------------------

    #[test]
    fn json_is_one_line_and_escapes_what_it_must() {
        let mut outcome = in_band("mintty 3.7.7", Some(SupportStatus::Yes), Some("3.4.2"));
        outcome.evidence.push(Evidence {
            source: "TERM_PROGRAM",
            // Hostile: a quote, a backslash and an escape byte, all of which a
            // user could put in an environment variable.
            value: "a\"b\\c\u{1b}]0;pwned\u{7}".to_owned(),
            implies: None,
            in_band: false,
            caveat: None,
        });
        let line = json_line(&outcome);
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "one object, one line");
        assert!(!line.contains('\u{1b}'), "a raw ESC reached the output");
        assert!(!line.contains('\u{7}'));
        assert!(line.contains("\\u001b"));
        assert!(line.contains("a\\\"b\\\\c"));
        assert!(line.contains("\"verdict\":\"yes\""));
        assert!(line.contains("\"measurable\":false"));
        assert!(line.contains("\"in_band\":true"));
    }

    #[test]
    fn json_renders_absent_values_as_null() {
        let line = json_line(&ProbeOutcome::default());
        assert!(line.contains("\"xtversion\":null"), "{line}");
        assert!(line.contains("\"terminal\":null"), "{line}");
        assert!(line.contains("\"multiplexer\":null"), "{line}");
        assert!(line.contains("\"matrix_status\":null"), "{line}");
        assert!(line.contains("\"verdict\":\"unknown\""), "{line}");
        assert!(line.contains("\"evidence\":[]"), "{line}");
    }

    #[test]
    fn json_escaping_leaves_no_control_character_behind() {
        // C0, DEL and C1: the whole set that could end a string early or start a
        // fresh escape sequence in the terminal that prints this line.
        let all: String = (0u32..=0x9f).filter_map(char::from_u32).collect();
        let encoded = escape_json(&all);
        assert!(!encoded.chars().any(char::is_control), "{encoded}");
        assert_eq!(escape_json("plain"), "plain");
        assert_eq!(escape_json("\u{7f}"), "\\u007f");
        assert_eq!(escape_json("a\"b\\c"), "a\\\"b\\\\c");
    }

    #[test]
    fn absent_values_are_null_rather_than_empty_strings() {
        assert_eq!(quoted_or_null(None), "null");
        assert_eq!(quoted_or_null(Some("kitty")), "\"kitty\"");
        assert_eq!(quoted_or_null(Some("")), "\"\"", "empty is not absent");
    }

    #[test]
    fn replies_render_without_control_bytes() {
        let replies = [
            Reply::Dcs(">|mintty 3.7.7".to_owned()),
            Reply::Csi {
                params: "?62;4".to_owned(),
                final_byte: 'c',
            },
        ];
        for reply in &replies {
            assert!(!describe_reply(reply).contains('\u{1b}'));
        }
    }

    #[test]
    fn truncate_keeps_short_values_and_marks_long_ones() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789", 10), "0123456789");
        assert_eq!(truncate("0123456789a", 10), "012345678…");
    }
}
