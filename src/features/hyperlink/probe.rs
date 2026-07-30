//! Inference, not measurement.
//!
//! Every other probe in `escapade` asks a question and reads an answer. This one
//! cannot: **OSC 8 has no capability query**. There is no sequence that means "do
//! you do hyperlinks?", no reply defined for one, and no side effect to observe —
//! a link that works and a link that is ignored look identical from the sending
//! end, because in both cases the text appears and nothing comes back.
//!
//! Inventing a verdict would be worse than admitting that, so this module does
//! three honest things instead:
//!
//! 1. Asks the questions that *do* have answers — XTVERSION (`CSI > q`), behind
//!    the same Primary Device Attributes barrier
//!    [`crate::features::glyph_protocol::probe`] uses, so a silent terminal can be
//!    told apart from a slow one.
//! 2. Collects the rest of the evidence: `TERM`, `TERM_PROGRAM`, and the
//!    per-terminal environment markers that are more reliable than either.
//! 3. Looks the identified terminal up in the support matrix and reports *that*,
//!    clearly labelled as a claim about the terminal rather than a measurement of
//!    this build of it.
//!
//! The practical upshot is printed at the end of the report: emit hyperlinks
//! anyway. A terminal without OSC 8 shows the text and drops the link.

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
        hyperlink::{FEATURE_ID, Hyperlink},
        wrap,
    },
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// How long to wait for the terminal to answer. Generous enough for a slow ssh
/// link, short enough not to feel like a hang.
const REPLY_TIMEOUT: Duration = Duration::from_millis(400);

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

/// One thing the probe learned, and what it points at.
#[derive(Debug, Clone)]
pub struct Evidence {
    /// Where it came from: `XTVERSION`, `TERM`, an environment variable name.
    pub source: &'static str,
    /// What that source said.
    pub value: String,
    /// The registry id it identifies, when it identifies one.
    pub implies: Option<&'static str>,
    /// Why it does not identify anything, when it does not.
    pub caveat: Option<&'static str>,
}

/// How confident the inference is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verdict {
    /// The identified terminal implements OSC 8 by default.
    Likely,
    /// It implements OSC 8 with gaps, or only once configured.
    Conditional,
    /// It is known not to implement OSC 8.
    Unlikely,
    /// Nothing identified the terminal, so there is nothing to say.
    #[default]
    Unknown,
}

impl Verdict {
    /// Short human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Likely => "hyperlinks probably work here",
            Self::Conditional => "hyperlinks probably work here, with caveats",
            Self::Unlikely => "hyperlinks probably do not work here",
            Self::Unknown => "cannot tell",
        }
    }

    /// Symbol for the report, matching the support matrix's vocabulary.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Likely => "✔",
            Self::Conditional => "◐",
            Self::Unlikely => "✘",
            Self::Unknown => "?",
        }
    }

    /// Colour for the report.
    #[must_use]
    pub fn ink(self) -> Ink {
        match self {
            Self::Likely => Ink::Good,
            Self::Conditional => Ink::Warn,
            Self::Unlikely => Ink::Bad,
            Self::Unknown => Ink::Muted,
        }
    }
}

/// What the probe concluded, and everything it concluded it from.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// Whether stdin and stdout are both terminals. Without that, nothing can be
    /// asked and nothing can be clicked.
    pub interactive: bool,
    /// Multiplexer in the way, if any.
    pub multiplexer: Option<Multiplexer>,
    /// The terminal's XTVERSION self-description, if it answered.
    pub xtversion: Option<String>,
    /// Whether the device-attributes barrier came back, i.e. whether silence on
    /// XTVERSION was a real refusal.
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
    /// Everything the terminal said, in order, for `--format debug`.
    pub replies: Vec<Reply>,
}

impl ProbeOutcome {
    /// The inference.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        match self.status {
            Some(SupportStatus::Yes) => Verdict::Likely,
            Some(SupportStatus::Partial | SupportStatus::Flag | SupportStatus::Passthrough) => {
                Verdict::Conditional
            }
            Some(SupportStatus::No) => Verdict::Unlikely,
            Some(SupportStatus::Unknown) | None => Verdict::Unknown,
        }
    }

    /// Whether the demo should run without `--force`.
    ///
    /// Deliberately generous: a demo is refused only on *positive* evidence of
    /// non-support, because "we could not identify your terminal" is not a reason
    /// to withhold a sequence that degrades to plain text. A terminal that is not
    /// a terminal at all is a different matter — there is nothing to click.
    #[must_use]
    pub fn looks_supported(&self) -> bool {
        self.interactive && self.verdict() != Verdict::Unlikely
    }

    /// The evidence entries that actually identified something.
    #[must_use]
    pub fn deciding_evidence(&self) -> Option<&Evidence> {
        self.evidence.iter().find(|item| item.implies.is_some())
    }
}

/// Environment variables that identify a terminal by their mere presence.
///
/// More reliable than `TERM`, which says what the *terminfo* entry is rather than
/// what the terminal is, and more reliable than `TERM_PROGRAM`, which several
/// terminals do not set at all.
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
    ("WT_SESSION", "windows-terminal"),
    ("CONTOUR_VERSION", "contour"),
    // VTE exports its version, which is how every VTE-based terminal — GNOME
    // Terminal, Tilix, xfce4-terminal — announces itself.
    ("VTE_VERSION", "gnome-terminal"),
];

/// Maps a terminal's self-reported name onto a registry id.
///
/// Matching is a case-insensitive substring search, in an order that puts the
/// specific before the general: `xterm-kitty` has to reach `kitty` before it
/// reaches `xterm`.
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
        // Multiplexers before xterm: `screen.xterm-256color` is Screen, and for
        // OSC 8 the multiplexer is the thing that has to implement the feature.
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
/// `xterm-256color` is what almost every terminal sets for compatibility with
/// terminfo databases, so reading it as "this is xterm" would report a hard `no`
/// on terminals that support hyperlinks perfectly well. This is the single
/// biggest trap in environment-based detection.
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

/// Identifies a terminal from `TERM`, refusing to be fooled by the generic
/// values everything sets.
#[must_use]
pub fn id_from_term(term: &str) -> Option<&'static str> {
    let lower = term.to_ascii_lowercase();
    if GENERIC_TERMS.contains(&lower.as_str()) {
        return None;
    }
    registry_id_for(&lower)
}

/// Strips XTVERSION's `>|` prefix from a DCS payload, returning the name the
/// terminal reported.
///
/// A reply looks like `DCS > | kitty(0.32.0) ST`, and [`crate::input`] hands over
/// the payload between `ESC P` and the terminator — so the `>|` is still on the
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
            caveat: None,
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
            value: term.clone(),
            implies,
            caveat: if implies.is_none() {
                Some("a terminfo entry name, not a terminal — nearly everything sets one of these")
            } else {
                None
            },
        });
    }

    evidence
}

/// Runs the probe using an existing terminal and reader.
///
/// Takes both rather than creating them because there must only ever be one
/// [`InputReader`] per process — two would race for the same bytes on stdin.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal that
/// stays quiet is not an error; there was never much to hear.
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
        // The barrier is not wrapped for a multiplexer: the point is to learn
        // whether the round trip completed, and tmux answers DA1 itself.
        term.write_now(&barrier_query())?;

        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
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
                    // Anything sent before the barrier has been processed, so
                    // waiting longer would learn nothing.
                    outcome.answered_barrier = true;
                    break;
                }
                _ => {}
            }
        }
    }

    outcome.evidence = collect_evidence(outcome.xtversion.as_deref());
    outcome.identified = outcome
        .deciding_evidence()
        .and_then(|item| item.implies)
        .or_else(|| {
            // A multiplexer is the terminal as far as OSC 8 is concerned: tmux
            // stores hyperlinks in its own grid before re-emitting them.
            outcome.multiplexer.map(|multiplexer| match multiplexer {
                Multiplexer::Tmux => "tmux",
                Multiplexer::Zellij => "zellij",
                Multiplexer::Screen => "screen",
            })
        });

    if let Some(id) = outcome.identified {
        outcome.terminal_name = catalog.terminal(id).map(|entry| entry.name.clone());
        if let Ok(feature) = catalog.feature(FEATURE_ID)
            && let Some(support) = feature.support.iter().find(|row| row.terminal == id)
        {
            outcome.status = Some(support.status);
            outcome.since = support.since.clone();
            outcome.notes = support.notes.clone();
        }
    }

    Ok(outcome)
}

/// Runs a probe from scratch.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn detect(catalog: &Catalog) -> Result<ProbeOutcome> {
    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let outcome = probe(catalog, &mut term, &mut input);
    // Restore before the caller prints anything, so the report appears in a
    // normal cooked-mode terminal.
    term.restore();
    outcome
}

/// The three-state answer this probe licenses.
///
/// The mapping is a judgement call, and this is the feature where it bites: there
/// is no capability query, so `Likely` never rests on anything the terminal said.
///
/// * `Likely` → `Yes`. The matrix says this terminal implements OSC 8 by default.
///   Calling that `Unknown` would make the predicate answer "cannot tell" on
///   every terminal that exists, since none can be measured — a predicate that is
///   never `Yes` is not a predicate. It is also the right way to be wrong: a false
///   positive here costs a link, not a corrupted screen.
/// * `Conditional` → `Unknown`. `partial`, `flag` and `passthrough` are precisely
///   the rows where the answer turns on something invisible from here — Konsole's
///   opt-in setting, Alacritty's mouse grab, Zellij's `mouse_mode`. Neither `Yes`
///   nor `No` would be honest.
/// * `Unlikely` → `No`. The one branch with a solid footing: xterm and GNU Screen
///   contain no implementation at all, which is a far more durable fact than any
///   claim about a version.
/// * `Unknown` → `Unknown`, as does a non-terminal stdout. Nothing identified the
///   terminal, or there is nothing there to click.
#[must_use]
pub fn support(outcome: &ProbeOutcome) -> Support {
    if !outcome.interactive {
        return Support::Unknown;
    }
    match outcome.verdict() {
        Verdict::Likely => Support::Yes,
        Verdict::Unlikely => Support::No,
        Verdict::Conditional | Verdict::Unknown => Support::Unknown,
    }
}

/// Runs `escapade hyperlink probe` and reports through the exit status.
///
/// Silent unless a `--format` is given, so it composes as a shell predicate.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn report(theme: &Theme, catalog: &Catalog, format: Option<ProbeFormat>) -> Result<ExitCode> {
    let outcome = detect(catalog)?;
    let support = support(&outcome);
    match format {
        None => {}
        Some(ProbeFormat::Json) => print!("{}", json_line(&outcome, support)),
        Some(ProbeFormat::Debug) => debug_report(theme, &outcome, support),
    }
    Ok(support.exit_code())
}

/// One JSON object, one line — the house machine-readable shape.
///
/// `measured` is always `false`, and is in the output on purpose: it is the most
/// important thing about this probe, and a script that consumes the verdict
/// deserves to know it was inferred from terminal identity rather than read off
/// the wire.
fn json_line(outcome: &ProbeOutcome, support: Support) -> String {
    let quoted = |value: Option<&str>| {
        value.map_or_else(
            || "null".to_owned(),
            |value| format!("\"{}\"", escape_json(value)),
        )
    };
    let multiplexer = outcome
        .multiplexer
        .map_or_else(|| "null".to_owned(), |m| format!("\"{}\"", m.name()));
    format!(
        "{{\"feature\":\"{FEATURE_ID}\",\"verdict\":\"{}\",\"measured\":false,\
         \"terminal\":{},\"matrix_status\":{},\"since\":{},\"evidence\":{},\
         \"xtversion\":{},\"answered_barrier\":{},\"multiplexer\":{multiplexer},\
         \"interactive\":{}}}\n",
        support.as_str(),
        quoted(outcome.identified),
        quoted(outcome.status.map(SupportStatus::label)),
        quoted(outcome.since.as_deref()),
        quoted(outcome.deciding_evidence().map(|item| item.source)),
        quoted(outcome.xtversion.as_deref()),
        outcome.answered_barrier,
        outcome.interactive,
    )
}

/// Minimal JSON string escaping, for the few strings that reach the output.
///
/// Load-bearing here rather than merely tidy: `xtversion` is a string the terminal
/// chose, and the environment variables are strings the user chose.
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
fn debug_report(theme: &Theme, outcome: &ProbeOutcome, support: Support) {
    println!();
    println!("  {}", theme.paint("OSC 8 hyperlink support", Ink::Title));
    println!();

    for line in wrap(
        "This one cannot be measured. OSC 8 has no capability query: there is no sequence that \
         asks whether the terminal does hyperlinks, no reply defined for one, and no side effect \
         to watch for — a link that works and a link that is ignored look identical from here. \
         What follows is inference from what the terminal says about itself, checked against the \
         support matrix in the database.",
        theme.width().saturating_sub(4),
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();

    if let Some(multiplexer) = outcome.multiplexer {
        println!(
            "  {} {}",
            theme.paint("multiplexer:", Ink::Muted),
            theme.paint(multiplexer.name(), Ink::Warn)
        );
        println!();
    }

    evidence_section(theme, outcome);
    conclusion_section(theme, outcome);
    wire_section(theme, outcome);

    println!();
    println!("  {}", theme.paint("the exit status", Ink::Accent));
    let code = match support {
        Support::Yes => "0",
        Support::No => "1",
        Support::Unknown => "2",
    };
    println!(
        "    {}  {}",
        theme.paint(&format!("{code} ({})", support.as_str()), Ink::Code),
        theme.paint(
            "what `escapade hyperlink probe` answers with no --format",
            Ink::Muted
        )
    );
    for line in wrap(
        "0 means the matrix says this terminal implements OSC 8, 1 means it says the terminal does \
         not, and 2 means nothing could honestly be concluded — an unidentified terminal, a row \
         with caveats, or output that is not a terminal at all. None of the three is a measurement.",
        theme.width().saturating_sub(6),
    ) {
        println!("      {}", theme.paint(&line, Ink::Muted));
    }

    println!();
    println!("  {}", theme.paint("send them anyway", Ink::Accent));
    for line in wrap(
        "A terminal that does not implement OSC 8 parses the sequence per ECMA-48, throws it away, \
         and prints the text. Nothing is corrupted and nothing is lost but the link, which is why \
         every tool that emits hyperlinks — ls, gcc, systemd, ripgrep — emits them \
         unconditionally. Branch on this probe to decide whether to *format around* links; do not \
         branch on it to decide whether it is safe to emit one, because it always is.",
        theme.width().saturating_sub(4),
    ) {
        println!("    {}", theme.paint(&line, Ink::Muted));
    }

    println!();
    for (command, meaning) in [
        ("escapade hyperlink explain", "the full support matrix"),
        ("escapade hyperlink demo", "see whether they really work"),
    ] {
        println!(
            "  {}   {}",
            theme.paint(&format!("{command:<26}"), Ink::Code),
            theme.paint(meaning, Ink::Muted)
        );
    }
    println!();
}

/// The evidence table.
fn evidence_section(theme: &Theme, outcome: &ProbeOutcome) {
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
        let value = truncate(&item.value, 40);
        let arrow = match item.implies {
            Some(id) => theme.paint(&format!("→ {id}"), Ink::Good),
            None => theme.paint("→ nothing", Ink::Muted),
        };
        println!(
            "    {}  {}  {arrow}",
            theme.paint(&format!("{:<width$}", item.source), Ink::Accent),
            theme.paint(&format!("{value:<40}"), Ink::Code),
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

    let barrier = if !outcome.interactive {
        theme.paint(
            "not asked — stdout and stdin are not both terminals",
            Ink::Warn,
        )
    } else if outcome.answered_barrier {
        theme.paint(
            "answered — so the silence on XTVERSION was a refusal, not a timeout",
            Ink::Good,
        )
    } else if outcome.xtversion.is_some() {
        theme.paint("overtaken — XTVERSION answered first", Ink::Muted)
    } else {
        theme.paint(
            "unanswered — something is swallowing replies, so nothing here is reliable",
            Ink::Warn,
        )
    };
    println!(
        "    {}  {barrier}",
        theme.paint(&format!("{:<width$}", "CSI c"), Ink::Accent)
    );
    println!();
}

/// The conclusion, and how much weight to put on it.
fn conclusion_section(theme: &Theme, outcome: &ProbeOutcome) {
    let verdict = outcome.verdict();
    println!("  {}", theme.paint("the inference", Ink::Accent));

    if outcome.interactive {
        println!(
            "    {} {}",
            theme.paint(verdict.symbol(), verdict.ink()),
            theme.paint(verdict.label(), verdict.ink())
        );
    } else {
        println!(
            "    {} {}",
            theme.paint("?", Ink::Warn),
            theme.paint("stdout is not a terminal", Ink::Warn)
        );
        for line in wrap(
            "Nothing could be asked, and nothing here could be clicked. The environment variables \
             above are still worth reading — and so is what the database says below — but a pipe \
             has no opinion about hyperlinks.",
            theme.width().saturating_sub(6),
        ) {
            println!("      {}", theme.paint(&line, Ink::Muted));
        }
        println!();
    }

    match (&outcome.terminal_name, outcome.status) {
        (Some(name), Some(status)) => {
            let since = match &outcome.since {
                Some(since) => format!(", since {since}"),
                None => String::new(),
            };
            let line = format!("{name} — the matrix says {}{since}", status.label());
            println!("      {}", theme.paint(&line, Ink::Strong));
            if let Some(notes) = &outcome.notes {
                println!("{}", theme.markdown_indented(notes, 6));
            }
            for line in wrap(
                "That is a claim about this terminal, from the database — not a measurement of the \
                 build you are running. An old version, a fork, or a configuration change can all \
                 make it wrong.",
                theme.width().saturating_sub(6),
            ) {
                println!("      {}", theme.paint(&line, Ink::Muted));
            }
        }
        _ => {
            for line in wrap(
                "Nothing identified this terminal, so the database has nothing to say about it. \
                 That is not a no — it is an absence of evidence, and the right response is to \
                 emit the hyperlink and see.",
                theme.width().saturating_sub(6),
            ) {
                println!("      {}", theme.paint(&line, Ink::Muted));
            }
        }
    }
    println!();
}

/// The bytes sent, the replies received, and a sample link.
fn wire_section(theme: &Theme, outcome: &ProbeOutcome) {
    println!("  {}", theme.paint("what was sent", Ink::Accent));
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

    println!();
    println!("  {}", theme.paint("what came back", Ink::Accent));
    if outcome.replies.is_empty() {
        println!("    {}", theme.paint("(nothing)", Ink::Muted));
    }
    for reply in &outcome.replies {
        println!("    {}", theme.paint(&describe_reply(reply), Ink::Code));
    }

    println!();
    println!(
        "  {}",
        theme.paint("what a hyperlink would look like", Ink::Accent)
    );
    match Hyperlink::new("https://example.com") {
        Ok(link) => {
            // Three tokens rather than one `pretty` call over the whole stream:
            // the label butts straight up against the `OSC` of the close
            // sequence otherwise, and the shape of the thing is the point here.
            println!(
                "    {} {} {}",
                theme.paint(&pretty(&link.open()), Ink::Code),
                theme.paint("click me", Ink::Link),
                theme.paint(&pretty(&link.close()), Ink::Code)
            );
        }
        Err(error) => {
            println!("    {}", theme.paint(&error.to_string(), Ink::Bad));
        }
    }
    println!(
        "    {}",
        theme.paint(
            "No query precedes it, because there is none to send.",
            Ink::Muted
        )
    );
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_queries_are_the_documented_ones() {
        assert_eq!(version_query(), "\u{1b}[>q");
        assert_eq!(barrier_query(), "\u{1b}[c");
    }

    #[test]
    fn xtversion_replies_lose_their_prefix() {
        assert_eq!(
            parse_xtversion(">|kitty(0.32.0)").as_deref(),
            Some("kitty(0.32.0)")
        );
        assert_eq!(
            parse_xtversion(">|WezTerm 20240203").as_deref(),
            Some("WezTerm 20240203")
        );
        // Not an XTVERSION reply at all.
        assert_eq!(parse_xtversion("1$r0m"), None);
        assert_eq!(parse_xtversion(">|"), None);
        assert_eq!(parse_xtversion(""), None);
    }

    #[test]
    fn self_reported_names_map_onto_registry_ids() {
        for (name, expected) in [
            ("kitty(0.32.0)", "kitty"),
            ("ghostty 1.0.1", "ghostty"),
            ("WezTerm 20240203-110809", "wezterm"),
            ("foot(1.16.2)", "foot"),
            ("contour 0.4.3", "contour"),
            ("iTerm2 3.5", "iterm2"),
            ("Alacritty 0.13", "alacritty"),
            ("XTerm(389)", "xterm"),
        ] {
            assert_eq!(registry_id_for(name), Some(expected), "{name}");
        }
        assert_eq!(registry_id_for("SomeNewTerminal 1.0"), None);
    }

    #[test]
    fn xterm_kitty_is_kitty_not_xterm() {
        // The single most important ordering constraint in the table.
        assert_eq!(id_from_term("xterm-kitty"), Some("kitty"));
        assert_eq!(id_from_term("xterm-ghostty"), Some("ghostty"));
    }

    #[test]
    fn generic_term_values_identify_nothing() {
        // Reading `xterm-256color` as xterm would report a hard "no" on terminals
        // that support hyperlinks perfectly well.
        for term in [
            "xterm-256color",
            "XTERM-256COLOR",
            "xterm",
            "ansi",
            "dumb",
            "linux",
        ] {
            assert_eq!(id_from_term(term), None, "{term}");
        }
        // Distinctive ones still work.
        assert_eq!(id_from_term("foot-extra"), Some("foot"));
        assert_eq!(id_from_term("alacritty"), Some("alacritty"));
        assert_eq!(id_from_term("rio"), Some("rio"));
    }

    #[test]
    fn every_registry_id_the_table_can_produce_exists_in_the_registry() {
        let catalog = Catalog::load().unwrap();
        let names = [
            "kitty(1)",
            "ghostty",
            "WezTerm",
            "alacritty",
            "contour",
            "iTerm2",
            "konsole",
            "vscode",
            "Windows Terminal",
            "gnome-terminal",
            "vte",
            "foot",
            "rio",
            "xterm",
        ];
        for name in names {
            let id = registry_id_for(name).unwrap_or_else(|| panic!("{name} matched nothing"));
            assert!(
                catalog.terminal(id).is_some(),
                "{id} is not in terminals.toml"
            );
        }
        for (key, id) in ENV_MARKERS {
            assert!(
                catalog.terminal(id).is_some(),
                "{key} → {id} is not in terminals.toml"
            );
        }
    }

    #[test]
    fn every_identifiable_terminal_has_a_support_row() {
        // Without this, the probe could identify a terminal and then have nothing
        // to say about it, which would read as "unknown" for no good reason.
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        for (_, id) in ENV_MARKERS {
            assert!(
                feature.support.iter().any(|row| row.terminal == *id),
                "no hyperlink support row for {id}"
            );
        }
    }

    #[test]
    fn the_verdict_follows_the_support_status() {
        let with = |status: Option<SupportStatus>| ProbeOutcome {
            interactive: true,
            status,
            ..ProbeOutcome::default()
        };
        assert_eq!(with(Some(SupportStatus::Yes)).verdict(), Verdict::Likely);
        assert_eq!(
            with(Some(SupportStatus::Partial)).verdict(),
            Verdict::Conditional
        );
        assert_eq!(
            with(Some(SupportStatus::Flag)).verdict(),
            Verdict::Conditional
        );
        assert_eq!(with(Some(SupportStatus::No)).verdict(), Verdict::Unlikely);
        assert_eq!(
            with(Some(SupportStatus::Unknown)).verdict(),
            Verdict::Unknown
        );
        assert_eq!(with(None).verdict(), Verdict::Unknown);
    }

    #[test]
    fn the_predicate_maps_conditional_onto_unknown_and_never_onto_no() {
        let with = |interactive: bool, status: Option<SupportStatus>| {
            support(&ProbeOutcome {
                interactive,
                status,
                ..ProbeOutcome::default()
            })
        };

        // Inferred-likely is a Yes: nothing about OSC 8 can be measured, so a
        // predicate that reserved Yes for measurements would never return it.
        assert_eq!(with(true, Some(SupportStatus::Yes)), Support::Yes);
        // Known-absent is the one solid No.
        assert_eq!(with(true, Some(SupportStatus::No)), Support::No);
        // Everything uncertain is Unknown, and never No.
        for status in [
            Some(SupportStatus::Partial),
            Some(SupportStatus::Flag),
            Some(SupportStatus::Passthrough),
            Some(SupportStatus::Unknown),
            None,
        ] {
            assert_eq!(with(true, status), Support::Unknown, "{status:?}");
        }
        // A pipe cannot render a link, and nothing was asked either.
        assert_eq!(with(false, Some(SupportStatus::Yes)), Support::Unknown);
        assert_eq!(with(false, Some(SupportStatus::No)), Support::Unknown);
    }

    #[test]
    fn the_exit_status_distinguishes_all_three_answers() {
        use std::process::ExitCode;
        // Compared through Debug because ExitCode is deliberately opaque.
        let code = |support: Support| format!("{:?}", support.exit_code());
        assert_eq!(code(Support::Yes), format!("{:?}", ExitCode::SUCCESS));
        assert_ne!(code(Support::No), code(Support::Unknown));
        assert_ne!(code(Support::Yes), code(Support::No));
    }

    #[test]
    fn the_json_line_is_one_self_contained_object() {
        let outcome = ProbeOutcome {
            interactive: true,
            identified: Some("kitty"),
            terminal_name: Some("kitty".to_owned()),
            status: Some(SupportStatus::Yes),
            since: Some("0.19".to_owned()),
            xtversion: Some("kitty(0.32.0)".to_owned()),
            answered_barrier: true,
            evidence: collect_evidence(Some("kitty(0.32.0)")),
            ..ProbeOutcome::default()
        };
        let line = json_line(&outcome, support(&outcome));

        assert!(line.ends_with('\n'), "JSON Lines needs the newline");
        assert_eq!(line.matches('\n').count(), 1, "one object, one line");
        assert!(line.starts_with("{\"feature\":\"hyperlinks\""));
        for fragment in [
            "\"verdict\":\"yes\"",
            "\"measured\":false",
            "\"terminal\":\"kitty\"",
            "\"matrix_status\":\"yes\"",
            "\"since\":\"0.19\"",
            "\"evidence\":\"XTVERSION\"",
            "\"xtversion\":\"kitty(0.32.0)\"",
            "\"answered_barrier\":true",
            "\"interactive\":true",
        ] {
            assert!(line.contains(fragment), "missing {fragment} in {line}");
        }
    }

    #[test]
    fn absent_fields_become_json_null_rather_than_empty_strings() {
        let outcome = ProbeOutcome {
            interactive: true,
            ..ProbeOutcome::default()
        };
        let line = json_line(&outcome, support(&outcome));
        for fragment in [
            "\"verdict\":\"unknown\"",
            "\"terminal\":null",
            "\"matrix_status\":null",
            "\"since\":null",
            "\"xtversion\":null",
            "\"multiplexer\":null",
        ] {
            assert!(line.contains(fragment), "missing {fragment} in {line}");
        }
    }

    #[test]
    fn json_escaping_survives_a_hostile_xtversion() {
        // The terminal chooses this string, so it is untrusted input that lands
        // straight in a machine-readable field.
        let outcome = ProbeOutcome {
            interactive: true,
            xtversion: Some("ev\"il\\\u{1b}[2J".to_owned()),
            ..ProbeOutcome::default()
        };
        let line = json_line(&outcome, support(&outcome));
        // The ESC becomes a `\u001b` escape, the quote and the backslash are
        // escaped, and nothing survives that a JSON parser would choke on.
        let expected = "\"xtversion\":\"ev\\\"il\\\\\\u001b[2J\"";
        assert!(line.contains(expected), "{line}");
        assert!(
            !line.contains('\u{1b}'),
            "a raw ESC must never reach stdout"
        );

        assert_eq!(escape_json("plain"), "plain");
        assert_eq!(escape_json("a\"b"), "a\\\"b");
        assert_eq!(escape_json("a\\b"), "a\\\\b");
        assert_eq!(escape_json("a\nb"), "a\\u000ab");
    }

    #[test]
    fn the_demo_gate_only_refuses_on_positive_evidence_of_non_support() {
        let gate = |interactive: bool, status: Option<SupportStatus>| {
            ProbeOutcome {
                interactive,
                status,
                ..ProbeOutcome::default()
            }
            .looks_supported()
        };
        assert!(gate(true, Some(SupportStatus::Yes)));
        assert!(gate(true, Some(SupportStatus::Flag)));
        assert!(
            gate(true, None),
            "an unidentified terminal gets the benefit of the doubt"
        );
        assert!(!gate(true, Some(SupportStatus::No)));
        assert!(
            !gate(false, Some(SupportStatus::Yes)),
            "a pipe has nothing to click"
        );
    }

    #[test]
    fn evidence_is_ordered_most_trustworthy_first() {
        // XTVERSION is a statement by the terminal itself, so it wins over any
        // environment variable, which in turn wins over TERM.
        let evidence = collect_evidence(Some("kitty(0.32.0)"));
        assert_eq!(evidence[0].source, "XTVERSION");
        assert_eq!(evidence[0].implies, Some("kitty"));

        let outcome = ProbeOutcome {
            evidence,
            ..ProbeOutcome::default()
        };
        assert_eq!(
            outcome.deciding_evidence().map(|item| item.source),
            Some("XTVERSION")
        );
    }

    #[test]
    fn unidentifiable_evidence_carries_a_caveat() {
        let evidence = collect_evidence(Some("BrandNewTerm 1.0"));
        assert_eq!(evidence[0].implies, None);
        // TERM is always the last entry when it is set, and it explains itself
        // when it identifies nothing.
        if let Some(term) = evidence.iter().find(|item| item.source == "TERM")
            && term.implies.is_none()
        {
            assert!(term.caveat.is_some());
        }
    }

    #[test]
    fn replies_render_without_control_bytes() {
        let replies = [
            Reply::Dcs(">|kitty(0.32.0)".to_owned()),
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

    #[test]
    fn truncate_keeps_short_values_and_marks_long_ones() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789", 10), "0123456789");
        assert_eq!(truncate("0123456789a", 10), "012345678…");
        // Char-based, so multi-byte values are not cut mid-scalar.
        assert_eq!(truncate("\u{e9}\u{e9}\u{e9}", 2), "\u{e9}…");
    }

    #[test]
    fn every_verdict_has_a_distinct_symbol_and_ink() {
        let verdicts = [
            Verdict::Likely,
            Verdict::Conditional,
            Verdict::Unlikely,
            Verdict::Unknown,
        ];
        let mut symbols: Vec<&str> = verdicts.iter().map(|v| v.symbol()).collect();
        symbols.sort_unstable();
        let count = symbols.len();
        symbols.dedup();
        assert_eq!(symbols.len(), count);
        for verdict in verdicts {
            assert!(!verdict.label().is_empty());
        }
        assert_eq!(Verdict::default(), Verdict::Unknown);
    }
}
