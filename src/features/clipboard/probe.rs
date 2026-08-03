//! Measuring what can be measured, which for once is the useful half.
//!
//! Writing to the clipboard has no reply and no observable side effect: the thing
//! that changes lives in the window system, not in the grid. So the obvious way to
//! test it — write something and read it back — is out, because reading is refused
//! far more often than writing, and because a probe that overwrites the user's
//! clipboard to satisfy its own curiosity has broken the thing it was asked about.
//!
//! There is a better way, and it costs nothing extra. The **clipboard extension**
//! agreed between terminal authors puts the number `52` among the parameters of the
//! Primary Device Attributes reply, and the spec is explicit about what that
//! promises:
//!
//! > If a terminal has an option to disable clipboard access, it MUST only
//! > indicate support for this extension when writing to the clipboard is actually
//! > allowed.
//!
//! Which makes `CSI c` — the barrier every probe in this project already sends, to
//! tell a silent terminal from a slow one — do double duty. The same round trip
//! that proves the terminal was listening also carries its answer, and the answer
//! is about *this session's configuration*, not about a version number in a table.
//! foot demonstrates the point: it drops the `52` from its own reply when the user
//! has turned clipboard writes off.
//!
//! # What licenses each verdict
//!
//! * **Yes, measured** — `52` came back in the device-attributes reply.
//! * **Yes, inferred** — the terminal named itself with XTVERSION and the support
//!   matrix says writes work there. Alacritty and Rio are here: they implement the
//!   write and have not adopted the extension.
//! * **No** — the matrix says the terminal does not implement it at all, on the
//!   terminal's own in-band say-so. VTE parses the number and has no handler; st
//!   has neither.
//! * **Unknown** — everything else, and in particular *the absence of `52` proves
//!   nothing*. xterm has implemented this since before the extension existed.
//!
//! # Reading is not probed
//!
//! Deliberately, and it is the most important decision in this module. The only way
//! to measure a read is to ask for the clipboard, and asking is not free:
//!
//! * kitty and Ghostty raise a permission dialog by default — a probe that makes a
//!   window pop up is not a probe.
//! * **Konsole destroys the clipboard.** It has no query branch at all: `?` falls
//!   through to the write path, `QByteArray::fromBase64` discards it as an invalid
//!   character, and the clipboard is set to the empty string.
//!
//! So the exit status is about writing, reading is reported as unmeasured with the
//! reason, and the only thing that ever asks is `escapade clipboard paste` — where
//! the user asked.

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
        clipboard::{FEATURE_ID, sequence},
        doctor::identity,
        wrap,
    },
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// How long to wait for the terminal to answer.
///
/// A full second, per the house rule: a slow ssh link must not be reported as a
/// terminal that lacks the feature. Under the barrier the wait almost always ends
/// early, as soon as the device-attributes reply arrives.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to spend consuming a reply still in flight after a conclusion.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// The device-attributes parameter that means "OSC 52 writes work here".
pub const CLIPBOARD_EXTENSION: &str = "52";

/// XTVERSION: `CSI > q`, answered `DCS > | <name and version> ST`.
#[must_use]
pub fn version_query() -> String {
    format!("{CSI}>q")
}

/// Primary Device Attributes: `CSI c` — the barrier, and here also the question.
#[must_use]
pub fn barrier_query() -> String {
    format!("{CSI}c")
}

// ---------------------------------------------------------------------------
// Identity
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
    /// Whether this came from the terminal itself rather than the environment.
    ///
    /// The distinction a `yes` turns on. `TERM_PROGRAM` is set by whatever
    /// launched the shell and is inherited by every child, including one on the
    /// far side of an ssh hop — which is precisely the situation this feature
    /// exists for, and precisely when the environment is lying.
    pub in_band: bool,
}

/// Environment variables that identify a terminal by their mere presence.
///
/// Kept short on purpose: this is the untrustworthy tier of evidence, and its only
/// job is to tell the reader of a `--format debug` report which terminal we think
/// is out there. The trustworthy tier is the XTVERSION reply.
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
    ("MINTTY_SHORTCUT", "mintty"),
    ("WT_SESSION", "windows-terminal"),
    ("VTE_VERSION", "gnome-terminal"),
];

/// The generic `TERM` values that identify nothing.
///
/// Reading `xterm-256color` as "this is xterm" would be a confident wrong answer
/// on nearly every terminal in the matrix — and for this feature it would be the
/// *worst* wrong answer available, since xterm is the one terminal that refuses
/// clipboard access by default.
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

/// Maps a name onto a registry id, via the shared family table.
///
/// [`identity::family`] is the one table in this project that knows every spelling
/// terminals use for themselves; it is reused rather than copied, because the
/// copies that already exist have drifted apart at least once. Its tokens are
/// family names rather than registry ids, so the two that differ are translated
/// here and anything with no row in the registry becomes `None`.
#[must_use]
pub fn registry_id_for(name: &str) -> Option<&'static str> {
    match identity::family(name)? {
        "vte" => Some("gnome-terminal"),
        // Families with no row in `terminals.toml`: naming one would be worse than
        // saying nothing, because the matrix lookup would silently find nothing
        // and the reason would claim an identification that leads nowhere.
        "terminal-app" | "mlterm" | "rxvt" | "putty" | "hyper" | "linux-console" => None,
        other => Some(other),
    }
}

/// Identifies a terminal from `TERM`, refusing to be fooled by the generic values.
#[must_use]
pub fn id_from_term(term: &str) -> Option<&'static str> {
    let lower = term.to_ascii_lowercase();
    if GENERIC_TERMS.contains(&lower.as_str()) {
        return None;
    }
    registry_id_for(&lower)
}

/// Gathers every clue, most trustworthy first.
///
/// Pure, so the identification can be tested without a terminal. `xtversion` is
/// threaded in rather than read here because getting it takes a round trip.
#[must_use]
pub fn collect_evidence(xtversion: Option<&str>) -> Vec<Evidence> {
    let mut evidence = Vec::new();

    if let Some(name) = xtversion {
        evidence.push(Evidence {
            source: "XTVERSION",
            value: name.to_owned(),
            implies: registry_id_for(name),
            in_band: true,
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
            });
        }
    }

    if let Ok(program) = env::var("TERM_PROGRAM") {
        evidence.push(Evidence {
            implies: registry_id_for(&program),
            source: "TERM_PROGRAM",
            value: program,
            in_band: false,
        });
    }

    let term = env::var("TERM").unwrap_or_default();
    if !term.is_empty() {
        evidence.push(Evidence {
            implies: id_from_term(&term),
            source: "TERM",
            value: term,
            in_band: false,
        });
    }

    evidence
}

// ---------------------------------------------------------------------------
// The outcome
// ---------------------------------------------------------------------------

/// What the probe established, and everything it established it from.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// Whether stdin and stdout are both terminals. Without both there is no
    /// round trip — though a copy itself travels down a pipe perfectly well.
    pub interactive: bool,
    /// Multiplexer between this process and the terminal, if any.
    pub multiplexer: Option<Multiplexer>,
    /// The terminal's XTVERSION self-description, if it answered.
    pub xtversion: Option<String>,
    /// Whether the device-attributes barrier came back.
    pub answered_barrier: bool,
    /// The parameters of the device-attributes reply, as they arrived.
    pub attributes: Option<String>,
    /// Whether `52` was among them.
    pub advertised: bool,
    /// Every piece of evidence, in the order it was consulted.
    pub evidence: Vec<Evidence>,
    /// The registry id the evidence settled on.
    pub identified: Option<&'static str>,
    /// Display name of the identified terminal.
    pub terminal_name: Option<String>,
    /// What the support matrix says about writing there.
    pub status: Option<SupportStatus>,
    /// The matrix's caveats for this terminal, which is where reading is
    /// described.
    pub notes: Option<String>,
    /// Everything the terminal said, in order, for the debug report.
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

    /// The verdict on **writing**, which is what the exit status reports.
    #[must_use]
    pub fn support(&self) -> Support {
        if !self.interactive {
            return Support::Unknown;
        }
        // The measurement. It outranks everything below it, including a matrix row
        // that disagrees: the terminal in front of us has just promised, in band,
        // that writes are allowed in this session.
        if self.advertised {
            return Support::Yes;
        }
        // Without the terminal's own name there is nothing left to reason from: an
        // environment variable is somebody else's claim about what is at the far
        // end of the pipe, and this is the feature people run across an ssh hop.
        if self.in_band_evidence().is_none() {
            return Support::Unknown;
        }
        match self.status {
            // `partial` is a yes for the exit status: Konsole and Zellij write to
            // the clipboard correctly and fall down on reading, which is not what
            // the status answers. The gap is in `notes` for anyone who cares.
            Some(SupportStatus::Yes | SupportStatus::Partial) => Support::Yes,
            Some(SupportStatus::No) => Support::No,
            // Behind a flag, or needing a multiplexer to be configured: whether it
            // works *here* is exactly what cannot be seen from here — and it is
            // exactly what the extension parameter would have answered.
            Some(SupportStatus::Flag | SupportStatus::Passthrough | SupportStatus::Unknown)
            | None => Support::Unknown,
        }
    }

    /// Whether the verdict rests on a measurement rather than a table.
    #[must_use]
    pub fn measured(&self) -> bool {
        self.advertised
    }

    /// Why the verdict is what it is, in one sentence.
    #[must_use]
    pub fn reason(&self) -> String {
        if !self.interactive {
            return "stdin and stdout are not both terminals, so the terminal could not be asked \
                    anything — note that copying itself does not need a terminal on stdin"
                .to_owned();
        }
        if self.advertised {
            return format!(
                "the terminal answered device attributes with {}, and a terminal may only \
                 advertise the clipboard extension when writing is actually allowed",
                self.attributes
                    .clone()
                    .unwrap_or_else(|| CLIPBOARD_EXTENSION.to_owned())
            );
        }
        let name = self
            .terminal_name
            .clone()
            .or_else(|| self.xtversion.clone());
        match (self.in_band_evidence(), self.status, name) {
            (Some(_), Some(status), Some(name)) => format!(
                "{name} did not advertise the clipboard extension, which settles nothing on its \
                 own; it named itself in its XTVERSION reply and the matrix records writing there \
                 as {}",
                status.label()
            ),
            (Some(_), _, Some(name)) => format!(
                "{name} named itself, did not advertise the clipboard extension, and has no row \
                 in the matrix"
            ),
            (Some(_), _, None) => "the terminal named itself with a name escapade has no row for, \
                                   and did not advertise the clipboard extension"
                .to_owned(),
            // Named itself, but not with a name there is a row for. Worth its own
            // sentence: "did not name itself" would be false, and the difference
            // is the difference between an unknown terminal and a missing row.
            (None, _, Some(name)) => format!(
                "the terminal called itself {name}, which escapade has no support row for, and it \
                 did not advertise the clipboard extension — so nothing was established, and a \
                 missing extension parameter is not a refusal"
            ),
            (None, ..) => {
                let head = match self.multiplexer {
                    Some(multiplexer) => format!(
                        "{} answered the device-attributes barrier itself, so nothing here \
                         describes the terminal beyond it",
                        multiplexer.name()
                    ),
                    None if self.answered_barrier => "the terminal answered device attributes \
                                                      without the clipboard extension and did not \
                                                      name itself"
                        .to_owned(),
                    None => "nothing came back at all, barrier included, so something between \
                             here and the terminal is swallowing replies"
                        .to_owned(),
                };
                format!(
                    "{head} — and a missing extension parameter is not a refusal: xterm has \
                     implemented this sequence since long before the extension existed"
                )
            }
        }
    }

    /// Why reading was not measured. Always the same reason, and worth repeating
    /// wherever the write verdict appears.
    ///
    /// The matrix row's own caveats are deliberately *not* appended: they run to a
    /// paragraph for tmux alone, `explain` already prints them, and burying one
    /// sentence of reasoning under six of quotation is how a report stops being
    /// read.
    #[expect(
        clippy::unused_self,
        reason = "a method rather than a free function because it belongs beside \
                  `reason()` and is called the same way; that it needs no state is \
                  the point — no probe result can ever change this answer"
    )]
    #[must_use]
    pub fn read_reason(&self) -> String {
        "not measured: the only way to test a read is to ask for the clipboard, which raises a \
         permission dialog on kitty and Ghostty and, on Konsole, overwrites the clipboard with an \
         empty string instead of answering. `escapade clipboard paste` asks; nothing else does."
            .to_owned()
    }

    /// Whether the demo should run without `--force`.
    ///
    /// Generous on purpose, like the hyperlink and progress demos: an unsupported
    /// OSC 52 is discarded silently, so the only thing lost on a terminal that
    /// ignores it is the clipboard step. A positive `No` is the one refusal.
    #[must_use]
    pub fn looks_supported(&self) -> bool {
        self.interactive && self.support() != Support::No
    }
}

/// Pulls the parameters out of a Primary Device Attributes reply.
///
/// A DA1 reply is `CSI ? 62 ; 22 ; 52 c`, and [`crate::input`] hands over the
/// parameter text with the private-prefix `?` still on the front. A reply without
/// that prefix is not a DA1 — `CSI > 0 ; 276 ; 0 c` is the *secondary* attributes
/// reply, whose numbers mean something else entirely and one of which could easily
/// be 52.
#[must_use]
pub fn device_attributes(params: &str) -> Option<String> {
    params.strip_prefix('?').map(str::to_owned)
}

/// Whether a device-attributes parameter list advertises the clipboard extension.
///
/// Whole-parameter comparison, never a substring: `152` and `520` both contain
/// `52` and neither means anything of the kind.
#[must_use]
pub fn advertises_clipboard(attributes: &str) -> bool {
    attributes
        .split(';')
        .any(|param| param.trim() == CLIPBOARD_EXTENSION)
}

// ---------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------

/// Runs the probe with an existing terminal and reader.
///
/// Takes both rather than making them, because there must only ever be one
/// [`InputReader`] per process — two would race for the same bytes of stdin.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A silent
/// terminal is not an error; it is one of the results.
pub fn probe(catalog: &Catalog, term: &mut Term, input: &mut InputReader) -> Result<ProbeOutcome> {
    let mut outcome = ProbeOutcome {
        multiplexer: Multiplexer::detect(),
        interactive: stdout().is_terminal() && stdin().is_terminal(),
        ..ProbeOutcome::default()
    };

    if outcome.interactive {
        // Raw mode is required: in cooked mode the reply would sit in the line
        // buffer until Enter, having first been echoed at the user.
        term.enable_raw()?;
        input.drain();

        term.write_now(&version_query())?;
        // Not wrapped for a multiplexer, and that is the point: a multiplexer
        // answers device attributes from its own emulator, so what comes back
        // describes whoever will actually handle the clipboard sequence.
        term.write_now(&barrier_query())?;

        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            // EOF on stdin means nobody is going to answer; treat it as silence.
            let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
                break;
            };
            outcome.replies.push(reply.clone());
            match &reply {
                Reply::Dcs(payload) => {
                    if let Some(name) = identity::strip_xtversion(payload) {
                        outcome.xtversion = Some(name.to_owned());
                    }
                }
                Reply::Csi {
                    params,
                    final_byte: 'c',
                } => {
                    outcome.answered_barrier = true;
                    if let Some(attributes) = device_attributes(params) {
                        outcome.advertised = advertises_clipboard(&attributes);
                        outcome.attributes = Some(attributes);
                        // Everything sent before the barrier has been processed,
                        // so waiting longer would learn nothing.
                        break;
                    }
                }
                _ => {}
            }
        }

        // XTVERSION can arrive first and end the wait with the barrier still in
        // flight. An unread reply is delivered to whatever runs next — the user's
        // shell, which prints it as stray text.
        if !outcome.answered_barrier
            && let Ok(Some(reply)) = input.wait_for_reply(DRAIN_TIMEOUT, |reply| {
                matches!(reply, Reply::Csi { final_byte: 'c', .. })
            })
        {
            if let Reply::Csi { params, .. } = &reply
                && let Some(attributes) = device_attributes(params)
            {
                outcome.advertised = advertises_clipboard(&attributes);
                outcome.attributes = Some(attributes);
            }
            outcome.replies.push(reply);
            outcome.answered_barrier = true;
        }
    }

    conclude(&mut outcome, catalog);
    Ok(outcome)
}

/// Turns the evidence into an identity, and the identity into a matrix row.
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
        // work for no answer. The environment is still collected: it licenses no
        // verdict, but it tells a reader which terminal we think is out there.
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
    // Restored before the caller prints, so any report lands in a cooked terminal.
    term.restore();
    outcome
}

/// Runs `escapade clipboard probe`.
///
/// Silent unless a `--format` was asked for: the answer is the exit status, `0`
/// writes work, `1` they do not, `2` cannot tell. A script that is about to pipe
/// something into `clipboard copy` wants exactly this, once, at the top.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn report(theme: &Theme, catalog: &Catalog, format: Option<ProbeFormat>) -> Result<ExitCode> {
    let outcome = detect(catalog)?;

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
/// `measurable` is `true` only when the extension parameter came back, because
/// that is the only path here that measures anything. `read` is nested under its
/// own key with `measured` permanently `false`, so that a script cannot mistake
/// the write verdict for an answer about reading.
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
        "{{\"feature\":\"{FEATURE_ID}\",\"verdict\":\"{}\",\"measurable\":{},\"reason\":\"{}\",\
         \"advertised\":{},\"device_attributes\":{},\"terminal\":{},\"xtversion\":{},\
         \"matrix_status\":{},\"answered_barrier\":{},\"multiplexer\":{},\"interactive\":{},\
         \"read\":{{\"measured\":false,\"reason\":\"{}\"}},\"evidence\":[{}]}}\n",
        outcome.support().as_str(),
        outcome.measured(),
        escape_json(&outcome.reason()),
        outcome.advertised,
        quoted_or_null(outcome.attributes.as_deref()),
        quoted_or_null(outcome.identified),
        quoted_or_null(outcome.xtversion.as_deref()),
        quoted_or_null(outcome.status.map(SupportStatus::label)),
        outcome.answered_barrier,
        quoted_or_null(outcome.multiplexer.map(Multiplexer::name)),
        outcome.interactive,
        escape_json(&outcome.read_reason()),
        evidence.join(",")
    )
}

/// A JSON string, or `null` when there is nothing to say.
fn quoted_or_null(value: Option<&str>) -> String {
    value.map_or_else(
        || "null".to_owned(),
        |text| format!("\"{}\"", escape_json(text)),
    )
}

/// Minimal JSON string escaping.
///
/// Everything reaching this output has been through a terminal or an environment
/// variable, so control bytes are a real possibility rather than a hypothetical.
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
    reason = "one screen of output written top to bottom; splitting it would \
              scatter the layout across functions without simplifying anything"
)]
fn print_debug(theme: &Theme, outcome: &ProbeOutcome) {
    let width = theme.width().saturating_sub(6);

    println!();
    println!(
        "  {}",
        theme.paint("Clipboard access (OSC 52)", Ink::Title)
    );
    println!();
    for line in wrap(
        "Writing has no reply, so it cannot be tested by doing it. What can be tested is the \
         clipboard extension: a terminal that puts 52 among its device-attributes parameters is \
         promising that writes are allowed in this session, and must withdraw the promise when \
         the user turns clipboard access off.",
        width,
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();

    let support = outcome.support();
    let (symbol, ink) = match support {
        Support::Yes => ("✔", Ink::Good),
        Support::No => ("✘", Ink::Bad),
        Support::Unknown => ("?", Ink::Warn),
    };
    println!(
        "  {} {}  {}",
        theme.paint(symbol, ink),
        theme.paint(&format!("writing: {}", support.as_str()), ink),
        theme.paint(
            &format!(
                "(exit status {}, {})",
                match support {
                    Support::Yes => 0,
                    Support::No => 1,
                    Support::Unknown => 2,
                },
                if outcome.measured() {
                    "measured"
                } else {
                    "inferred"
                }
            ),
            Ink::Muted
        )
    );
    for line in wrap(&outcome.reason(), width) {
        println!("      {}", theme.paint(&line, Ink::Muted));
    }
    println!();
    println!(
        "  {} {}  {}",
        theme.paint("?", Ink::Warn),
        theme.paint("reading: not probed", Ink::Warn),
        theme.paint("(never in the exit status)", Ink::Muted)
    );
    for line in wrap(&outcome.read_reason(), width) {
        println!("      {}", theme.paint(&line, Ink::Muted));
    }
    println!();

    println!("  {}", theme.paint("what went out", Ink::Accent));
    for (sequence, meaning) in [
        (version_query(), "XTVERSION — what do you call yourself?"),
        (
            barrier_query(),
            "device attributes — the barrier, and the clipboard question",
        ),
    ] {
        println!(
            "    {}  {}",
            theme.paint(&format!("{:<12}", pretty(&sequence)), Ink::Code),
            theme.paint(meaning, Ink::Muted)
        );
    }
    println!(
        "    {}  {}",
        theme.paint(&format!("{:<12}", ""), Ink::Code),
        theme.paint(
            &format!(
                "no {} sequence was sent: a write would replace the clipboard and a read is \
                 destructive on Konsole",
                pretty(&sequence::query(sequence::Selection::Clipboard))
            ),
            Ink::Muted
        )
    );
    println!();

    println!("  {}", theme.paint("what came back", Ink::Accent));
    println!(
        "    {}  {}",
        theme.paint(&format!("{:<22}", "device attributes"), Ink::Strong),
        theme.paint(
            &outcome
                .attributes
                .clone()
                .map_or_else(|| "no answer".to_owned(), |text| format!("CSI ?{text} c")),
            Ink::Code
        )
    );
    println!(
        "    {}  {}",
        theme.paint(&format!("{:<22}", "extension 52"), Ink::Strong),
        if outcome.advertised {
            theme.paint("advertised", Ink::Good)
        } else {
            theme.paint("not advertised — which is not a refusal", Ink::Warn)
        }
    );
    println!(
        "    {}  {}",
        theme.paint(&format!("{:<22}", "XTVERSION"), Ink::Strong),
        theme.paint(
            outcome.xtversion.as_deref().unwrap_or("no answer"),
            Ink::Code
        )
    );

    println!();
    println!("  {}", theme.paint("evidence, most trustworthy first", Ink::Accent));
    if outcome.evidence.is_empty() {
        println!("    {}", theme.paint("nothing identified this terminal", Ink::Muted));
    }
    for item in &outcome.evidence {
        let implies = item.implies.unwrap_or("nothing escapade has a row for");
        println!(
            "    {} {}  {}",
            theme.paint(if item.in_band { "•" } else { "◦" }, Ink::Muted),
            theme.paint(&format!("{:<16}", item.source), Ink::Strong),
            theme.paint(&format!("{} → {implies}", item.value), Ink::Muted)
        );
    }

    if let Some(multiplexer) = outcome.multiplexer {
        println!();
        for line in wrap(
            &format!(
                "{} is between this process and the terminal, and it is a participant rather than \
                 a bystander: it answered the barrier itself, and it decides whether a clipboard \
                 sequence reaches the terminal at all.",
                multiplexer.name()
            ),
            width,
        ) {
            println!("  {}", theme.paint(&line, Ink::Warn));
        }
    }
    println!();
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// An outcome as a round trip would build one.
    fn outcome(attributes: Option<&str>, xtversion: Option<&str>) -> ProbeOutcome {
        let mut outcome = ProbeOutcome {
            interactive: true,
            answered_barrier: attributes.is_some(),
            attributes: attributes.map(str::to_owned),
            advertised: attributes.is_some_and(advertises_clipboard),
            xtversion: xtversion.map(str::to_owned),
            ..ProbeOutcome::default()
        };
        // Only the in-band half of the evidence, so the tests do not depend on
        // whatever terminal happens to be running them.
        outcome.evidence = xtversion
            .map(|name| {
                vec![Evidence {
                    source: "XTVERSION",
                    value: name.to_owned(),
                    implies: registry_id_for(name),
                    in_band: true,
                }]
            })
            .unwrap_or_default();
        outcome.identified = outcome.deciding_evidence().and_then(|item| item.implies);
        outcome
    }

    #[test]
    fn the_extension_parameter_is_the_measurement() {
        let probe = outcome(Some("62;22;28;52"), None);
        assert_eq!(probe.support(), Support::Yes);
        assert!(probe.measured());
        assert!(probe.reason().contains("may only advertise"), "{}", probe.reason());
    }

    #[test]
    fn a_missing_extension_parameter_is_never_a_refusal() {
        // xterm implemented this sequence for a decade before the extension
        // existed. Reading silence here as "no" would be the confident wrong
        // answer this project exists to avoid.
        let probe = outcome(Some("62;1;4"), None);
        assert_eq!(probe.support(), Support::Unknown);
        assert!(!probe.measured());
        assert!(probe.reason().contains("not a refusal"), "{}", probe.reason());
    }

    #[test]
    fn fifty_two_is_matched_as_a_whole_parameter() {
        assert!(advertises_clipboard("62;52"));
        assert!(advertises_clipboard("52"));
        assert!(advertises_clipboard("62;52;22"));
        // The near misses, all of which a substring search would accept.
        assert!(!advertises_clipboard("62;520"));
        assert!(!advertises_clipboard("152"));
        assert!(!advertises_clipboard("1;5;2"));
        assert!(!advertises_clipboard("62;4;6"));
    }

    #[test]
    fn only_primary_device_attributes_are_read_for_the_extension() {
        // `CSI > 0 ; 276 ; 0 c` is the *secondary* reply: a firmware version, not
        // a capability list, and one whose middle number could easily be 52.
        assert_eq!(device_attributes("?62;22"), Some("62;22".to_owned()));
        assert_eq!(device_attributes(">0;52;0"), None);
        assert_eq!(device_attributes("62;52"), None, "no private prefix");
    }

    #[test]
    fn a_terminal_that_names_itself_falls_back_to_the_matrix() {
        let catalog = Catalog::load().unwrap();
        let mut probe = outcome(Some("62;1;4"), Some("Alacritty 0.15.0"));
        conclude(&mut probe, &catalog);
        assert_eq!(probe.identified, Some("alacritty"));
        assert_eq!(probe.support(), Support::Yes);
        assert!(!probe.measured(), "a table is not a measurement");
        assert!(probe.reason().contains("matrix"), "{}", probe.reason());
    }

    #[test]
    fn a_terminal_the_matrix_says_no_about_is_a_no() {
        let catalog = Catalog::load().unwrap();
        let mut probe = outcome(Some("65;1;9"), Some("VTE(8002)"));
        conclude(&mut probe, &catalog);
        assert_eq!(probe.identified, Some("gnome-terminal"));
        assert_eq!(probe.support(), Support::No);
    }

    #[test]
    fn a_measured_yes_outranks_a_matrix_row_that_disagrees() {
        // The extension promise is about this session; the row is about a version
        // somebody checked once. If xterm is configured to allow writes and says
        // so, that is the answer.
        let catalog = Catalog::load().unwrap();
        let mut probe = outcome(Some("62;52"), Some("XTerm(400)"));
        conclude(&mut probe, &catalog);
        assert_eq!(probe.identified, Some("xterm"));
        assert_eq!(probe.support(), Support::Yes);
        assert!(probe.measured());
    }

    #[test]
    fn a_terminal_with_no_row_is_not_a_terminal_that_stayed_quiet() {
        // Two different silences: one where nothing answered, and one where the
        // terminal answered with a name nobody has written a row for.
        let probe = outcome(Some("62;22"), Some("FakeTerm(1.2.3)"));
        assert_eq!(probe.support(), Support::Unknown);
        let reason = probe.reason();
        assert!(reason.contains("FakeTerm(1.2.3)"), "{reason}");
        assert!(reason.contains("no support row"), "{reason}");
        assert!(!reason.contains("did not name itself"), "{reason}");
    }

    #[test]
    fn an_unidentified_terminal_establishes_nothing() {
        let probe = outcome(None, None);
        assert_eq!(probe.support(), Support::Unknown);
        assert!(probe.reason().contains("swallowing"), "{}", probe.reason());
    }

    #[test]
    fn no_terminal_is_unknown_rather_than_no() {
        let mut probe = outcome(None, None);
        probe.interactive = false;
        assert_eq!(probe.support(), Support::Unknown);
        assert!(probe.reason().contains("not both terminals"), "{}", probe.reason());
        // And the reason says the useful part: copying still works in a pipe.
        assert!(probe.reason().contains("copying"), "{}", probe.reason());
    }

    #[test]
    fn reading_is_never_measured_and_always_says_why() {
        let probe = outcome(Some("62;52"), None);
        let reason = probe.read_reason();
        assert!(reason.starts_with("not measured"), "{reason}");
        assert!(reason.contains("Konsole"), "{reason}");
        assert!(json_line(&probe).contains("\"read\":{\"measured\":false"));
    }

    #[test]
    fn the_json_separates_measurement_from_inference() {
        let measured = json_line(&outcome(Some("62;52"), None));
        assert!(measured.contains("\"measurable\":true"), "{measured}");
        assert!(measured.contains("\"advertised\":true"), "{measured}");
        assert!(measured.contains("\"device_attributes\":\"62;52\""), "{measured}");

        let inferred = json_line(&outcome(Some("62;1"), Some("kitty 0.44.0")));
        assert!(inferred.contains("\"measurable\":false"), "{inferred}");
        assert!(inferred.contains("\"advertised\":false"), "{inferred}");
        assert!(inferred.contains("\"xtversion\":\"kitty 0.44.0\""), "{inferred}");
    }

    #[test]
    fn json_is_one_object_on_one_line_and_never_holds_a_control_byte() {
        let mut probe = outcome(Some("62;52"), Some("weird\u{1b}[name"));
        probe.evidence.push(Evidence {
            source: "TERM",
            value: "a \"quoted\"\tvalue".to_owned(),
            implies: None,
            in_band: false,
        });
        let line = json_line(&probe);
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(!line.contains('\u{1b}'), "{line}");
        assert!(line.contains("\\u001b"), "{line}");
        assert!(line.contains("\\\"quoted\\\""), "{line}");
        assert!(line.contains("\"feature\":\"clipboard\""));
    }

    #[test]
    fn the_shared_family_table_is_reused_rather_than_copied() {
        // The point of going through `doctor::identity` — every spelling it knows
        // works here too, including the ones that embed another terminal's name.
        assert_eq!(registry_id_for("xterm-kitty"), Some("kitty"));
        assert_eq!(registry_id_for("kitty 0.44.0"), Some("kitty"));
        assert_eq!(registry_id_for("foot(1.16.2)"), Some("foot"));
        assert_eq!(registry_id_for("WezTerm 20240203"), Some("wezterm"));
        // The one family whose token differs from its registry id.
        assert_eq!(registry_id_for("VTE(8002)"), Some("gnome-terminal"));
        assert_eq!(registry_id_for("gnome-terminal"), Some("gnome-terminal"));
        // And a family with no row in the registry identifies nothing at all,
        // rather than naming a terminal the matrix lookup will not find.
        assert_eq!(registry_id_for("Apple_Terminal"), None);
        assert_eq!(registry_id_for("mlterm 3.9"), None);
    }

    #[test]
    fn generic_term_values_identify_nothing() {
        // The worst available wrong answer for this feature: xterm is the one
        // terminal in the matrix that refuses clipboard access by default, and
        // nearly everything sets TERM=xterm-256color.
        assert_eq!(id_from_term("xterm-256color"), None);
        assert_eq!(id_from_term("xterm"), None);
        assert_eq!(id_from_term("dumb"), None);
        // A TERM that really does name something still counts.
        assert_eq!(id_from_term("xterm-kitty"), Some("kitty"));
        assert_eq!(id_from_term("foot"), Some("foot"));
    }

    #[test]
    fn every_registry_id_this_probe_can_produce_exists() {
        let catalog = Catalog::load().unwrap();
        for (_, id) in ENV_MARKERS {
            assert!(catalog.terminal(id).is_some(), "unknown terminal id `{id}`");
        }
        for name in [
            "kitty", "ghostty", "wezterm", "alacritty", "foot", "contour", "iTerm2", "konsole",
            "gnome-terminal", "mintty", "rio", "st", "xterm", "tmux", "zellij", "screen",
            "Windows Terminal", "vscode", "warp",
        ] {
            if let Some(id) = registry_id_for(name) {
                assert!(
                    catalog.terminal(id).is_some(),
                    "`{name}` maps to unknown terminal id `{id}`"
                );
            }
        }
    }

    #[test]
    fn the_feature_id_matches_the_database() {
        let catalog = Catalog::load().unwrap();
        assert!(catalog.feature(FEATURE_ID).is_ok());
    }
}
