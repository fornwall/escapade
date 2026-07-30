//! Working out which of the three notification protocols this terminal speaks.
//!
//! Notifications are the awkward case for capability detection, because the
//! three protocols are not equally answerable:
//!
//! * **OSC 99 can be asked.** `OSC 99 ; i=<id> : p=? ; ST` has a defined reply,
//!   so support is *measured*. As in [`crate::features::glyph_protocol::probe`],
//!   the query is paired with a Primary Device Attributes barrier (`CSI c`) that
//!   every terminal since the VT100 answers, which is what turns silence into a
//!   conclusion instead of a guess.
//! * **OSC 9 and OSC 777 cannot be asked anything at all.** They have no query
//!   and no reply. The only thing left is to work out *which terminal this is* —
//!   from `TERM_PROGRAM`, terminal-specific environment variables, `TERM`, and
//!   the terminal's own answer to XTVERSION (`CSI > q`) — and look the answer up
//!   in a table. That is inference, not measurement, and everything this module
//!   prints says which of the two it is doing.
//!
//! The distinction matters more than it sounds. An inferred "no" is a reason to
//! send the sequence anyway: an unrecognised OSC string is discarded silently, so
//! being wrong costs nothing, while refusing to notify because a table was out of
//! date costs the user their notification.
//!
//! # What the exit status answers
//!
//! `escapade notification probe` is a shell predicate, and a predicate has to be
//! about one thing. Three protocols cannot be squeezed into one status, so the
//! status answers the question a script actually has — *"will a notification
//! appear if I run `escapade notification`?"* — by reporting on the protocol
//! [`crate::features::notification::resolve`] would choose. `--format debug` and
//! `--format json` report all three separately, because that is where the detail
//! belongs.

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
        notification::sequence::{Osc99Support, Protocol, osc99_query, parse_osc99_reply},
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

/// The id used for the capability query. Fixed rather than random because it is
/// only ever correlated with its own immediate reply.
const QUERY_ID: &str = "escapade-probe";

/// How firmly a per-protocol verdict is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// The terminal was asked and answered. Only OSC 99 can reach this.
    Measured,
    /// Inferred from the terminal's identity plus the support matrix.
    Inferred,
    /// Nothing identified the terminal, so nothing is known.
    Unknown,
}

impl Confidence {
    /// Short label for the report.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Inferred => "inferred",
            Self::Unknown => "unknown",
        }
    }
}

/// What is believed about one protocol, and on what grounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Which protocol.
    pub protocol: Protocol,
    /// The three-state answer for this protocol.
    pub support: Support,
    /// How that answer was arrived at.
    pub confidence: Confidence,
    /// Why, in one clause, for the user.
    pub reason: String,
}

impl Verdict {
    /// Whether an application can expect this protocol to work here.
    ///
    /// Only [`Support::Yes`] counts. [`Support::Unknown`] deliberately does not:
    /// the whole point of the third state is that "we could not tell" is not
    /// "yes", and a caller that wants to try anyway should say so rather than
    /// have it assumed.
    #[must_use]
    pub fn supported(&self) -> bool {
        self.support == Support::Yes
    }
}

/// What the probe learned by *asking*, as opposed to by looking things up.
///
/// Separated from the identity because it is what decides whether an inference
/// may be believed at all: a multiplexer in the way, or output that is not a
/// terminal, makes the tidiest table lookup worthless.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Measurement {
    /// The device-attributes barrier came back.
    pub answered_barrier: bool,
    /// A multiplexer is in the way.
    pub multiplexed: bool,
    /// stdin and stdout are both terminals, so asking was possible at all.
    pub interactive: bool,
}

impl Measurement {
    /// Whether a barrier reply licenses a negative conclusion.
    ///
    /// **A multiplexer answers Primary Device Attributes itself** — tmux replies
    /// `CSI ?1;2c` from its own emulator — so inside one, a barrier reply says
    /// nothing about the terminal beyond it, and treating it as proof of absence
    /// is a confident false claim.
    fn barrier_proves_absence(self) -> bool {
        self.answered_barrier && !self.multiplexed && self.interactive
    }

    /// Whether a table lookup can be trusted to describe what will actually
    /// happen.
    ///
    /// It cannot when a multiplexer is filtering the stream — the terminal beyond
    /// it may implement every one of the three and still never see the sequence —
    /// and it cannot when there is no terminal on the other end to implement
    /// anything.
    fn inference_is_usable(self) -> bool {
        self.interactive && !self.multiplexed
    }
}

/// A terminal `escapade` can recognise from the environment, with what it is
/// known to implement.
///
/// This is the notification support matrix in executable form. It is
/// deliberately per-protocol, which `data/features/notifications.toml` cannot be
/// — a matrix row has one status per terminal, and here the whole point is that
/// a terminal may implement one protocol and not another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    /// Terminal id, matching `data/terminals.toml`.
    pub id: &'static str,
    /// Display name.
    pub name: &'static str,
    /// Implements kitty's OSC 99.
    pub osc99: bool,
    /// Implements urxvt's `OSC 777 ; notify`.
    pub osc777: bool,
    /// Implements iTerm2's OSC 9.
    pub osc9: bool,
    /// Anything a user should know about this terminal specifically.
    pub note: Option<&'static str>,
}

impl Known {
    /// Whether this terminal implements `protocol`.
    #[must_use]
    pub fn implements(&self, protocol: Protocol) -> bool {
        match protocol {
            Protocol::Osc99 => self.osc99,
            Protocol::Osc777 => self.osc777,
            Protocol::Osc9 => self.osc9,
        }
    }
}

/// Every terminal whose notification support has been checked, keyed by the
/// lowercase tokens that identify it.
///
/// A token is matched against `TERM_PROGRAM`, against `TERM`, and against the
/// first word of the XTVERSION reply, so one entry covers all three routes. The
/// sources for each row are in `data/features/notifications.toml`.
const KNOWN: &[(&[&str], Known)] = &[
    (
        &["kitty", "xterm-kitty"],
        Known {
            id: "kitty",
            name: "kitty",
            osc99: true,
            osc777: false,
            osc9: true,
            // kovidgoyal, closing kitty#1474: "I went ahead and implemented the
            // OSC 99 protocol … And I also added support to kitty for OSC 9 for
            // backwards compat." OSC 777 was refused as urxvt-specific.
            note: Some("declined OSC 777 as a urxvt-specific extension mechanism"),
        },
    ),
    (
        &["ghostty", "xterm-ghostty"],
        Known {
            id: "ghostty",
            name: "Ghostty",
            osc99: false,
            osc777: true,
            osc9: true,
            note: Some("OSC 99 is requested but not documented as implemented"),
        },
    ),
    (
        &["wezterm"],
        Known {
            id: "wezterm",
            name: "WezTerm",
            osc99: false,
            osc777: true,
            osc9: true,
            note: Some("OSC 99 is not in WezTerm's escape sequence reference"),
        },
    ),
    (
        &["foot", "foot-extra", "foot-direct"],
        Known {
            id: "foot",
            name: "foot",
            osc99: true,
            osc777: true,
            osc9: true,
            note: Some("only notifies while the window is unfocused, by design"),
        },
    ),
    (
        &["contour"],
        Known {
            id: "contour",
            name: "Contour",
            osc99: true,
            osc777: true,
            osc9: true,
            note: Some("OSC 777 must have exactly three fields or it is ignored"),
        },
    ),
    (
        &["iterm.app", "iterm2"],
        Known {
            id: "iterm2",
            name: "iTerm2",
            osc99: false,
            osc777: false,
            osc9: true,
            note: Some("OSC 9 is the original, and can be turned off per profile"),
        },
    ),
    (
        &["rio"],
        Known {
            id: "rio",
            name: "Rio",
            osc99: false,
            osc777: true,
            osc9: true,
            note: Some("OSC 9 falls back to a notification when it is not a progress report"),
        },
    ),
    (
        &["konsole"],
        Known {
            id: "konsole",
            name: "Konsole",
            osc99: false,
            osc777: true,
            osc9: false,
            note: None,
        },
    ),
    (
        &["alacritty"],
        Known {
            id: "alacritty",
            name: "Alacritty",
            osc99: false,
            osc777: false,
            osc9: false,
            note: Some("no notification protocol of any kind"),
        },
    ),
    (
        &["gnome-terminal", "vte", "xterm-256color-vte"],
        Known {
            id: "gnome-terminal",
            name: "GNOME Terminal / VTE",
            osc99: false,
            osc777: false,
            osc9: false,
            note: Some("upstream VTE has none of these; Fedora patches OSC 777 in downstream"),
        },
    ),
    (
        &["windows-terminal", "windowsterminal"],
        Known {
            id: "windows-terminal",
            name: "Windows Terminal",
            osc99: false,
            osc777: true,
            osc9: false,
            note: Some(
                "OSC 777 is behind an optional feature; OSC 9 there is ConEmu taskbar progress",
            ),
        },
    ),
    (
        &["vscode"],
        Known {
            id: "vscode",
            name: "VS Code terminal",
            osc99: false,
            osc777: false,
            osc9: false,
            note: Some("unverified — treated as unsupported, but sending costs nothing"),
        },
    ),
    (
        &["xterm", "xterm-256color"],
        Known {
            id: "xterm",
            name: "xterm",
            osc99: false,
            osc777: false,
            osc9: false,
            note: None,
        },
    ),
];

/// Identifies the terminal from one token, matched case-insensitively.
#[must_use]
pub fn known_by_token(token: &str) -> Option<Known> {
    let token = token.trim().to_ascii_lowercase();
    if token.is_empty() {
        return None;
    }
    KNOWN
        .iter()
        .find(|(tokens, _)| tokens.contains(&token.as_str()))
        .map(|(_, known)| *known)
}

/// Where a terminal's identity came from, worth showing because the routes are
/// not equally trustworthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The terminal, if it was recognised.
    pub known: Option<Known>,
    /// Which environment variable or reply named it.
    pub source: String,
    /// The raw value that was matched.
    pub raw: String,
}

/// Identifies the terminal from the environment alone.
///
/// Tried in order of how specific each signal is. `TERM_PROGRAM` is checked
/// first because it names the application; `TERM` last because it names a
/// *terminfo entry*, and a terminal launched with `TERM=xterm-256color` for
/// compatibility is extremely common.
#[must_use]
pub fn identify_from_env() -> Identity {
    // Terminal-specific variables, which are the strongest signal available
    // without asking the terminal anything: nothing but kitty sets
    // KITTY_WINDOW_ID.
    const MARKERS: &[(&str, &str)] = &[
        ("KITTY_WINDOW_ID", "kitty"),
        ("GHOSTTY_RESOURCES_DIR", "ghostty"),
        ("GHOSTTY_BIN_DIR", "ghostty"),
        ("WEZTERM_PANE", "wezterm"),
        ("WEZTERM_EXECUTABLE", "wezterm"),
        ("ALACRITTY_WINDOW_ID", "alacritty"),
        ("ALACRITTY_SOCKET", "alacritty"),
        ("KONSOLE_VERSION", "konsole"),
        ("VTE_VERSION", "vte"),
        ("WT_SESSION", "windows-terminal"),
        ("RIO_CONFIG", "rio"),
        ("CONTOUR_PROFILE", "contour"),
        ("FOOT_PID", "foot"),
        ("ITERM_SESSION_ID", "iterm2"),
    ];

    if let Ok(program) = std::env::var("TERM_PROGRAM")
        && let Some(known) = known_by_token(&program)
    {
        return Identity {
            known: Some(known),
            source: "TERM_PROGRAM".to_owned(),
            raw: program,
        };
    }
    for (variable, token) in MARKERS {
        if std::env::var_os(variable).is_some()
            && let Some(known) = known_by_token(token)
        {
            return Identity {
                known: Some(known),
                source: (*variable).to_owned(),
                raw: (*token).to_owned(),
            };
        }
    }
    let term = std::env::var("TERM").unwrap_or_default();
    Identity {
        known: known_by_token(&term),
        source: "TERM".to_owned(),
        raw: term,
    }
}

/// Parses an XTVERSION reply into a terminal name.
///
/// The reply is `DCS > | <name and version> ST`, so the payload seen here starts
/// `>|`. Terminals write the name in their own style — `kitty 0.36.0`,
/// `foot(1.16.2)`, `WezTerm 20240203-110809`, `XTerm(389)` — so the name is
/// whatever precedes the first space, parenthesis or digit.
#[must_use]
pub fn parse_xtversion(payload: &str) -> Option<String> {
    let text = payload.strip_prefix(">|")?;
    let name: String = text
        .chars()
        .take_while(|ch| !ch.is_whitespace() && *ch != '(' && !ch.is_ascii_digit())
        .collect();
    let name = name.trim_end_matches(['-', '_', '.']).to_owned();
    if name.is_empty() { None } else { Some(name) }
}

/// Everything a probe found out.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    /// One verdict per protocol, richest first.
    pub verdicts: Vec<Verdict>,
    /// The OSC 99 capability reply, when there was one.
    pub osc99: Option<Osc99Support>,
    /// Whether the terminal answered the device-attributes barrier. Without it,
    /// silence means nothing was heard rather than "no".
    pub answered_barrier: bool,
    /// What the terminal called itself in its XTVERSION reply.
    pub version: Option<String>,
    /// How the terminal was identified.
    pub identity: Identity,
    /// Multiplexer in the way, if any.
    pub multiplexer: Option<Multiplexer>,
    /// Whether stdin and stdout are both terminals.
    pub interactive: bool,
    /// Everything the terminal said, for `--verbose`.
    pub replies: Vec<Reply>,
}

impl ProbeOutcome {
    /// A probe that asked nothing, for the paths where asking is impossible.
    ///
    /// Used when stdout is not a terminal: there is nobody to answer, but the
    /// environment still identifies the terminal, so inference still works and a
    /// piped `escapade notification …` still picks a sensible protocol.
    #[must_use]
    pub fn from_environment() -> Self {
        let identity = identify_from_env();
        let measurement = Measurement {
            multiplexed: Multiplexer::detect().is_some(),
            ..Measurement::default()
        };
        Self {
            verdicts: infer(&identity, None, measurement),
            osc99: None,
            answered_barrier: false,
            version: None,
            identity,
            multiplexer: Multiplexer::detect(),
            interactive: false,
            replies: Vec::new(),
        }
    }

    /// The measurable evidence, gathered up for [`infer`].
    #[must_use]
    pub fn measurement(&self) -> Measurement {
        Measurement {
            answered_barrier: self.answered_barrier,
            multiplexed: self.multiplexer.is_some(),
            interactive: self.interactive,
        }
    }

    /// The verdict for one protocol.
    #[must_use]
    pub fn verdict(&self, protocol: Protocol) -> Option<&Verdict> {
        self.verdicts
            .iter()
            .find(|verdict| verdict.protocol == protocol)
    }

    /// Whether any protocol looks usable — the gate the demo checks.
    #[must_use]
    pub fn any_supported(&self) -> bool {
        self.verdicts.iter().any(Verdict::supported)
    }

    /// The single answer `probe`'s exit status carries.
    ///
    /// It is the verdict for the protocol that would actually be *used*, which is
    /// the one `--protocol auto` resolves to. A script asking this question wants
    /// to know whether a notification will appear, not whether some protocol it
    /// will never send is implemented.
    ///
    /// When nothing is believed in, `auto` falls back to
    /// [`super::FALLBACK`] — so the answer is that protocol's own verdict. That
    /// matters: a terminal which demonstrably lacks OSC 99 but was never
    /// identified has *not* been shown to lack notifications, and reporting `No`
    /// on the strength of the one protocol that happens to be answerable would be
    /// the confident wrong answer this whole design is trying to avoid.
    #[must_use]
    pub fn support(&self) -> Support {
        for &protocol in &Protocol::RICHEST_FIRST {
            if let Some(verdict) = self.verdict(protocol)
                && verdict.supported()
            {
                return Support::Yes;
            }
        }
        self.verdict(super::FALLBACK)
            .map_or(Support::Unknown, |verdict| verdict.support)
    }

    /// Whether anything definite was learned.
    #[must_use]
    pub fn conclusive(&self) -> bool {
        self.support() != Support::Unknown
    }
}

/// Builds the three verdicts from an identity, a measured OSC 99 answer if there
/// was one, and what the probe could observe.
///
/// The mapping from evidence to [`Support`] is the whole point of this function,
/// so it is worth stating outright:
///
/// * **Measured yes** — the terminal answered the OSC 99 capability query.
///   [`Support::Yes`], and only OSC 99 can ever get here.
/// * **Measured no** — OSC 99 was asked, the device-attributes barrier came back,
///   and no multiplexer was in the way to have answered that barrier itself. The
///   terminal demonstrably had its chance: [`Support::No`].
/// * **Inferred** — the terminal was identified and the table says whether it
///   implements this protocol. That is the same claim `explain`'s support matrix
///   makes, sourced the same way, so it is reported as [`Support::Yes`] or
///   [`Support::No`] rather than hedged into a shrug. OSC 9 and OSC 777 have no
///   query in the protocol at all, so hedging would make them permanently
///   unknowable — and "iTerm2 shows OSC 9 notifications" is not a guess.
/// * **Inference that cannot be trusted** — a multiplexer is filtering the stream,
///   or output is not a terminal. The table may be perfectly right about the
///   terminal and still wrong about what will happen. [`Support::Unknown`].
/// * **Nothing identified the terminal** — [`Support::Unknown`].
#[must_use]
pub fn infer(
    identity: &Identity,
    osc99: Option<&Osc99Support>,
    measurement: Measurement,
) -> Vec<Verdict> {
    Protocol::RICHEST_FIRST
        .iter()
        .map(|&protocol| verdict_for(protocol, identity, osc99, measurement))
        .collect()
}

/// One protocol's verdict. See [`infer`] for the mapping.
fn verdict_for(
    protocol: Protocol,
    identity: &Identity,
    osc99: Option<&Osc99Support>,
    measurement: Measurement,
) -> Verdict {
    let verdict = |support, confidence, reason: String| Verdict {
        protocol,
        support,
        confidence,
        reason,
    };

    if protocol == Protocol::Osc99 {
        if let Some(support) = osc99 {
            return verdict(
                Support::Yes,
                Confidence::Measured,
                format!(
                    "the terminal answered the capability query with p={}",
                    if support.payloads.is_empty() {
                        "title".to_owned()
                    } else {
                        support.payloads.join(",")
                    }
                ),
            );
        }
        if measurement.barrier_proves_absence() {
            return verdict(
                Support::No,
                Confidence::Measured,
                "the terminal answered the device-attributes barrier but ignored the OSC 99 \
                 capability query, so it demonstrably had its chance"
                    .to_owned(),
            );
        }
    }

    let Some(known) = identity.known else {
        return verdict(
            Support::Unknown,
            Confidence::Unknown,
            format!(
                "nothing identified this terminal ({}={:?}), and {} cannot be asked",
                identity.source, identity.raw, protocol.label()
            ),
        );
    };

    let implemented = known.implements(protocol);
    if !measurement.inference_is_usable() {
        let obstacle = if measurement.interactive {
            "a multiplexer is filtering the stream, so it may never arrive"
        } else {
            "stdin and stdout are not both terminals, so there is nothing there to raise it"
        };
        return verdict(
            Support::Unknown,
            Confidence::Unknown,
            format!(
                "{} is recorded as {} {}, but {obstacle}",
                known.name,
                if implemented {
                    "implementing"
                } else {
                    "not implementing"
                },
                protocol.label(),
            ),
        );
    }

    verdict(
        if implemented { Support::Yes } else { Support::No },
        Confidence::Inferred,
        format!(
            "{} is recorded as {} {} ({}={})",
            known.name,
            if implemented {
                "implementing"
            } else {
                "not implementing"
            },
            protocol.label(),
            identity.source,
            identity.raw
        ),
    )
}

/// Probes using an existing terminal and reader.
///
/// Takes both rather than creating them because there must only ever be one
/// [`InputReader`] per process; two would each see a random half of the
/// terminal's answer.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal
/// that stays quiet is not an error, it is a negative result.
pub fn probe(term: &mut Term, input: &mut InputReader) -> Result<ProbeOutcome> {
    let mut outcome = ProbeOutcome::from_environment();
    outcome.interactive = stdout().is_terminal() && stdin().is_terminal();
    if !outcome.interactive {
        return Ok(outcome);
    }

    // Raw mode is required: in cooked mode the reply would sit in the line
    // buffer until Enter, and would be echoed at the user on the way.
    term.enable_raw()?;
    input.drain();

    send(term, outcome.multiplexer, &osc99_query(QUERY_ID))?;
    // XTVERSION names the terminal even when the environment does not — an ssh
    // hop drops TERM_PROGRAM but the terminal still answers for itself.
    send(term, outcome.multiplexer, &format!("{CSI}>q"))?;
    // The barrier is deliberately not wrapped for the multiplexer, so that it
    // reports on the round trip we can actually reason about. Note that a
    // multiplexer answers it from its own emulator rather than forwarding it,
    // which is exactly why a barrier reply cannot license a negative verdict from
    // inside one — see `Measurement::barrier_proves_absence`.
    term.write_now(&format!("{CSI}c"))?;

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
            Reply::Osc(payload) => {
                if let Some(support) = parse_osc99_reply(payload) {
                    outcome.osc99 = Some(support);
                }
            }
            Reply::Dcs(payload) => {
                if let Some(name) = parse_xtversion(payload) {
                    outcome.version = Some(name);
                }
            }
            Reply::Csi { final_byte: 'c', .. } => {
                // Everything sent before the barrier has now been processed, so
                // waiting longer would learn nothing.
                outcome.answered_barrier = true;
                break;
            }
            _ => {}
        }
    }

    // Two replies were asked for before the barrier, so concluding on the barrier
    // can leave either still in flight. Unread, they are delivered to whatever
    // runs next — the user's shell, which prints them as stray text. Draining is
    // what keeps a silent probe actually silent.
    if !outcome.answered_barrier {
        while let Ok(Some(reply)) = input.wait_for_reply(DRAIN_TIMEOUT, |_| true) {
            let barrier = matches!(reply, Reply::Csi { final_byte: 'c', .. });
            match &reply {
                Reply::Osc(payload) => {
                    if let Some(support) = parse_osc99_reply(payload) {
                        outcome.osc99 = Some(support);
                    }
                }
                Reply::Dcs(payload) => {
                    if let Some(name) = parse_xtversion(payload) {
                        outcome.version = Some(name);
                    }
                }
                _ => {}
            }
            outcome.replies.push(reply);
            if barrier {
                outcome.answered_barrier = true;
                break;
            }
        }
    }

    // XTVERSION beats the environment: it came from the terminal itself.
    if let Some(name) = &outcome.version
        && let Some(known) = known_by_token(name)
    {
        outcome.identity = Identity {
            known: Some(known),
            source: "XTVERSION (CSI > q)".to_owned(),
            raw: name.clone(),
        };
    }
    outcome.verdicts = infer(
        &outcome.identity,
        outcome.osc99.as_ref(),
        outcome.measurement(),
    );

    Ok(outcome)
}

/// Sends a sequence, wrapping it for a multiplexer if one is in the way.
///
/// # Errors
///
/// Fails if stdout does.
pub fn send(term: &mut Term, multiplexer: Option<Multiplexer>, sequence: &str) -> Result<()> {
    match multiplexer {
        Some(multiplexer) => term.write_now(&multiplexer.wrap(sequence)),
        None => term.write_now(sequence),
    }
}

/// Probes from scratch.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn detect() -> Result<ProbeOutcome> {
    if !(stdout().is_terminal() && stdin().is_terminal()) {
        // Nothing to ask, and creating an InputReader would spawn a thread on a
        // closed stdin for no reason.
        return Ok(ProbeOutcome::from_environment());
    }
    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let outcome = probe(&mut term, &mut input);
    // Restore before the caller prints, so the report lands in a cooked-mode
    // terminal.
    term.restore();
    outcome
}

/// Runs `escapade notification probe` and reports through the exit status.
///
/// Silent unless a `--format` is given, so it composes as a shell predicate. The
/// status is [`ProbeOutcome::support`] — the verdict for the protocol that would
/// actually be used.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn report(theme: &Theme, format: Option<ProbeFormat>) -> Result<ExitCode> {
    let outcome = detect()?;
    let support = outcome.support();
    match format {
        None => {}
        Some(ProbeFormat::Json) => print!("{}", json_line(&outcome, support)),
        Some(ProbeFormat::Debug) => debug_report(theme, &outcome, support),
    }
    Ok(support.exit_code())
}

/// One JSON object, one line — the house machine-readable shape.
///
/// `verdict` is the single answer the exit status carries; `protocol` names the
/// one it is about. All three protocols appear under `protocols` regardless, since
/// a script choosing a protocol for itself needs them separately.
fn json_line(outcome: &ProbeOutcome, support: Support) -> String {
    let chosen = crate::features::notification::resolve(crate::cli::NotifyProtocol::Auto, outcome);
    let protocols: Vec<String> = outcome
        .verdicts
        .iter()
        .map(|verdict| {
            format!(
                "\"{}\":{{\"support\":\"{}\",\"confidence\":\"{}\",\"reason\":\"{}\"}}",
                verdict.protocol.flag(),
                verdict.support.as_str(),
                verdict.confidence.label(),
                escape_json(&verdict.reason)
            )
        })
        .collect();
    let quoted = |text: &str| format!("\"{}\"", escape_json(text));
    let terminal = outcome
        .identity
        .known
        .map_or_else(|| "null".to_owned(), |known| quoted(known.id));
    let version = outcome
        .version
        .as_deref()
        .map_or_else(|| "null".to_owned(), quoted);
    let multiplexer = outcome
        .multiplexer
        .map_or_else(|| "null".to_owned(), |m| quoted(m.name()));
    let osc99 = outcome.osc99.as_ref().map_or_else(
        || "null".to_owned(),
        |support| {
            let list = |items: &[String]| {
                let quoted: Vec<String> = items.iter().map(|item| quoted(item)).collect();
                format!("[{}]", quoted.join(","))
            };
            format!(
                "{{\"payloads\":{},\"urgencies\":{},\"actions\":{},\"occasions\":{},\
                 \"close_events\":{},\"auto_expire\":{}}}",
                list(&support.payloads),
                list(&support.urgencies),
                list(&support.actions),
                list(&support.occasions),
                support.close_events,
                support.auto_expire,
            )
        },
    );
    format!(
        "{{\"feature\":\"notifications\",\"verdict\":\"{}\",\"protocol\":\"{}\",\
         \"protocols\":{{{}}},\"terminal\":{terminal},\"identified_by\":{},\"xtversion\":{version},\
         \"osc99\":{osc99},\"answered_barrier\":{},\"multiplexer\":{multiplexer},\
         \"interactive\":{}}}\n",
        support.as_str(),
        chosen.protocol.flag(),
        protocols.join(","),
        quoted(&outcome.identity.source),
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
fn debug_report(theme: &Theme, outcome: &ProbeOutcome, support: Support) {
    println!();
    println!(
        "  {}",
        theme.paint("Desktop notification support", Ink::Title)
    );
    println!();

    report_identity(theme, outcome);
    report_verdicts(theme, outcome);
    report_chosen(theme, outcome, support);
    report_osc99(theme, outcome);
    report_notes(theme, outcome);
    report_traffic(theme, outcome);

    println!();
    println!(
        "  {} {}",
        theme.paint("escapade notification explain", Ink::Code),
        theme.paint(
            "for what each protocol can express, and who implements it",
            Ink::Muted
        )
    );
    println!();
}

/// Which protocol the exit status is about, and what it said.
///
/// Spelled out because a single-valued status over three protocols is exactly the
/// kind of thing a reader will otherwise have to guess at.
fn report_chosen(theme: &Theme, outcome: &ProbeOutcome, support: Support) {
    let chosen = crate::features::notification::resolve(crate::cli::NotifyProtocol::Auto, outcome);
    let ink = match support {
        Support::Yes => Ink::Good,
        Support::No => Ink::Bad,
        Support::Unknown => Ink::Warn,
    };
    println!();
    println!(
        "  {} {} {}",
        theme.paint("would use:", Ink::Muted),
        theme.paint(chosen.protocol.label(), Ink::Accent),
        theme.paint(&format!("→ exit status {}", support.as_str()), ink)
    );
    for line in crate::features::wrap(&chosen.reason, theme.width().saturating_sub(6)) {
        println!("      {}", theme.paint(&line, Ink::Muted));
    }
}

/// Who are we talking to, and how do we know.
fn report_identity(theme: &Theme, outcome: &ProbeOutcome) {
    let term_name = std::env::var("TERM").unwrap_or_else(|_| "unset".to_owned());
    print!("  {} {}", theme.paint("terminal:", Ink::Muted), term_name);
    if let Ok(program) = std::env::var("TERM_PROGRAM") {
        print!(
            "{}",
            theme.paint(&format!("  (TERM_PROGRAM={program})"), Ink::Muted)
        );
    }
    println!();
    if let Some(version) = &outcome.version {
        println!(
            "  {} {}",
            theme.paint("XTVERSION:", Ink::Muted),
            theme.paint(version, Ink::Code)
        );
    }
    match outcome.identity.known {
        Some(known) => println!(
            "  {} {} {}",
            theme.paint("identified:", Ink::Muted),
            theme.paint(known.name, Ink::Strong),
            theme.paint(
                &format!("via {}={}", outcome.identity.source, outcome.identity.raw),
                Ink::Muted
            )
        ),
        None => println!(
            "  {} {}",
            theme.paint("identified:", Ink::Muted),
            theme.paint("not recognised", Ink::Warn)
        ),
    }
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
            "  {} stdin and stdout are not both terminals, so nothing could be asked",
            theme.paint("?", Ink::Warn),
        );
        for line in crate::features::wrap(
            "The verdicts below are inference from the environment only. Notifications \
             themselves work fine down a pipe — it is only the question that needs a terminal \
             to answer it.",
            theme.width().saturating_sub(4),
        ) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
        println!();
    }
}

/// The three per-protocol verdicts, each labelled with how it was arrived at.
fn report_verdicts(theme: &Theme, outcome: &ProbeOutcome) {
    for verdict in &outcome.verdicts {
        let (symbol, ink) = match verdict.support {
            Support::Yes => ("✔", Ink::Good),
            Support::No => ("✘", Ink::Bad),
            Support::Unknown => ("?", Ink::Warn),
        };
        println!(
            "  {} {:<9} {}  {}",
            theme.paint(symbol, ink),
            theme.paint(verdict.protocol.label(), Ink::Strong),
            theme.paint(
                &format!("[{} · {}]", verdict.support.as_str(), verdict.confidence.label()),
                Ink::Muted
            ),
            theme.paint(verdict.protocol.capability(), Ink::Muted)
        );
        println!("      {}", theme.paint(&verdict.reason, Ink::Muted));
    }
}

/// The detail of the OSC 99 capability reply, or why there was not one.
fn report_osc99(theme: &Theme, outcome: &ProbeOutcome) {
    if let Some(support) = &outcome.osc99 {
        println!();
        println!("  {}", theme.paint("what OSC 99 said it can do", Ink::Accent));
        for (label, value) in [
            ("payload types", support.payloads.join(", ")),
            ("urgencies", support.urgencies.join(", ")),
            ("activation", support.actions.join(", ")),
            ("occasions", support.occasions.join(", ")),
            ("sounds", support.sounds.join(", ")),
            ("close events", support.close_events.to_string()),
            ("auto-expiry", support.auto_expire.to_string()),
        ] {
            if value.is_empty() {
                continue;
            }
            println!(
                "    {} {}",
                theme.paint(&format!("{label:<14}"), Ink::Muted),
                theme.paint(&value, Ink::Code)
            );
        }
    } else if outcome.interactive {
        println!();
        let message = if outcome.answered_barrier {
            "OSC 99 was asked and stayed silent while the device-attributes barrier came \
             back, so that is a real no rather than a timeout."
        } else {
            "Nothing came back at all, not even the device-attributes barrier every terminal \
             answers. Something between here and the terminal is swallowing replies, so the \
             OSC 99 result is inconclusive."
        };
        for line in crate::features::wrap(message, theme.width().saturating_sub(4)) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }
}

/// Per-terminal caveats, the multiplexer warning, and the reason none of this is
/// load-bearing.
fn report_notes(theme: &Theme, outcome: &ProbeOutcome) {
    if let Some(known) = outcome.identity.known
        && let Some(note) = known.note
    {
        println!();
        println!(
            "  {} {}",
            theme.paint("note:", Ink::Warn),
            theme.paint(note, Ink::Muted)
        );
    }

    if let Some(multiplexer) = outcome.multiplexer {
        println!();
        for line in crate::features::wrap(
            &format!(
                "{} filters escape sequences it does not recognise, so a notification raised \
                 from in here may never reach the real terminal.",
                multiplexer.name()
            ),
            theme.width().saturating_sub(4),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
        println!("  {}", theme.paint(multiplexer.passthrough_hint(), Ink::Code));
    }

    println!();
    for line in crate::features::wrap(
        "An unrecognised OSC string is discarded silently, so a notification is safe to send \
         even where this says no. `escapade notification` picks the richest protocol it \
         believes in and tells you which one it used.",
        theme.width().saturating_sub(4),
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
}

/// The raw bytes in both directions, for `--verbose`.
fn report_traffic(theme: &Theme, outcome: &ProbeOutcome) {
    println!();
    println!("  {}", theme.paint("what was sent", Ink::Accent));
    let query = match outcome.multiplexer {
        Some(multiplexer) => multiplexer.wrap(&osc99_query(QUERY_ID)),
        None => osc99_query(QUERY_ID),
    };
    println!("    {}", theme.paint(&pretty(&query), Ink::Code));
    println!(
        "    {}  {}",
        theme.paint(&pretty(&format!("{CSI}>q")), Ink::Code),
        theme.paint("← XTVERSION, to find out who we are talking to", Ink::Muted)
    );
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

    fn identity(token: &str) -> Identity {
        Identity {
            known: known_by_token(token),
            source: "TERM_PROGRAM".to_owned(),
            raw: token.to_owned(),
        }
    }

    /// A terminal that could be asked freely: interactive, no multiplexer in the
    /// way. This is the condition under which an inference is worth believing, so
    /// most tests want it.
    fn clean() -> Measurement {
        Measurement {
            answered_barrier: true,
            multiplexed: false,
            interactive: true,
        }
    }

    #[test]
    fn every_known_terminal_is_in_the_registry() {
        // A row naming a terminal that is not in data/terminals.toml would mean
        // the probe and the support matrix disagree about who exists.
        let catalog = crate::db::Catalog::load().unwrap();
        for (tokens, known) in KNOWN {
            assert!(
                catalog.terminal(known.id).is_some(),
                "{:?} names unknown terminal id {}",
                tokens,
                known.id
            );
        }
    }

    #[test]
    fn known_terminals_have_distinct_tokens() {
        let mut seen: Vec<&str> = KNOWN.iter().flat_map(|(tokens, _)| *tokens).copied().collect();
        seen.sort_unstable();
        let total = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), total, "two terminals claim the same token");
    }

    #[test]
    fn tokens_identify_terminals_case_insensitively() {
        assert_eq!(known_by_token("kitty").map(|k| k.id), Some("kitty"));
        assert_eq!(known_by_token("iTerm.app").map(|k| k.id), Some("iterm2"));
        assert_eq!(known_by_token("  WezTerm ").map(|k| k.id), Some("wezterm"));
        assert_eq!(known_by_token("nonesuch"), None);
        assert_eq!(known_by_token(""), None);
    }

    #[test]
    fn the_matrix_records_that_kitty_declined_osc_777() {
        let kitty = known_by_token("kitty").expect("kitty is known");
        assert!(kitty.osc99, "kitty is the reference implementation");
        assert!(kitty.osc9, "added for backwards compatibility");
        assert!(!kitty.osc777, "explicitly refused as urxvt-specific");
    }

    #[test]
    fn a_measured_osc99_answer_beats_any_inference() {
        // Alacritty is recorded as implementing nothing. If it ever answers the
        // query, the answer wins — the table is the fallback, not the authority.
        let support = Osc99Support {
            payloads: vec!["title".to_owned(), "body".to_owned()],
            ..Osc99Support::default()
        };
        let verdicts = infer(&identity("alacritty"), Some(&support), clean());
        let osc99 = &verdicts[0];
        assert_eq!(osc99.protocol, Protocol::Osc99);
        assert_eq!(osc99.support, Support::Yes);
        assert_eq!(osc99.confidence, Confidence::Measured);
        // The unanswerable ones stay inferred.
        assert_eq!(verdicts[1].confidence, Confidence::Inferred);
        assert_eq!(verdicts[1].support, Support::No);
    }

    #[test]
    fn a_silent_osc99_behind_an_answered_barrier_is_a_measured_no() {
        // The barrier came back and OSC 99 did not, with nothing in the way to
        // have answered the barrier on the terminal's behalf.
        let verdicts = infer(&identity("iTerm.app"), None, clean());
        assert_eq!(verdicts[0].protocol, Protocol::Osc99);
        assert_eq!(verdicts[0].support, Support::No);
        assert_eq!(verdicts[0].confidence, Confidence::Measured);
        assert!(
            verdicts[0].reason.contains("demonstrably"),
            "{}",
            verdicts[0].reason
        );
    }

    #[test]
    fn a_silent_osc99_with_no_barrier_falls_back_to_the_table() {
        // Without the barrier there is no proof the terminal had its chance, so
        // the silence licenses nothing and the table takes over. iTerm2 is
        // recorded as not implementing OSC 99, which is still a real claim — just
        // an inferred one rather than a measured one.
        let measurement = Measurement {
            answered_barrier: false,
            multiplexed: false,
            interactive: true,
        };
        let verdicts = infer(&identity("iTerm.app"), None, measurement);
        assert_eq!(verdicts[0].protocol, Protocol::Osc99);
        assert_eq!(verdicts[0].support, Support::No);
        assert_eq!(
            verdicts[0].confidence,
            Confidence::Inferred,
            "silence without a barrier is not a measurement"
        );
        assert_eq!(verdicts[2].protocol, Protocol::Osc9);
        assert_eq!(verdicts[2].support, Support::Yes);

        // With no identity either, there is genuinely nothing to say.
        let verdicts = infer(&identity("some-new-terminal"), None, measurement);
        for verdict in &verdicts {
            assert_eq!(verdict.support, Support::Unknown, "{:?}", verdict.protocol);
        }
    }

    #[test]
    fn a_measured_no_beats_a_table_that_says_yes() {
        // kitty is recorded as implementing OSC 99. If it answered the barrier and
        // not the query — an old build, or the query disabled — the measurement is
        // the better evidence and must win.
        let verdicts = infer(&identity("kitty"), None, clean());
        assert_eq!(verdicts[0].protocol, Protocol::Osc99);
        assert_eq!(verdicts[0].support, Support::No);
        assert_eq!(verdicts[0].confidence, Confidence::Measured);
        // And OSC 9, which kitty also implements, is untouched by that.
        assert_eq!(verdicts[2].support, Support::Yes);
    }

    #[test]
    fn a_barrier_answered_inside_a_multiplexer_proves_nothing() {
        // tmux answers Primary Device Attributes from its own emulator, so the
        // barrier says nothing about the terminal beyond it — and the table says
        // nothing about whether the sequence will even get there.
        let measurement = Measurement {
            answered_barrier: true,
            multiplexed: true,
            interactive: true,
        };
        let verdicts = infer(&identity("kitty"), None, measurement);
        for verdict in &verdicts {
            assert_eq!(
                verdict.support,
                Support::Unknown,
                "{:?} claimed something from inside a multiplexer",
                verdict.protocol
            );
        }
        assert!(verdicts[1].reason.contains("multiplexer"), "{}", verdicts[1].reason);
    }

    #[test]
    fn a_measured_answer_survives_a_multiplexer() {
        // A positive OSC 99 answer needs no barrier and no table: something
        // replied in the protocol's own language, which only the real terminal
        // could have done, so passthrough is demonstrably working.
        let measurement = Measurement {
            answered_barrier: true,
            multiplexed: true,
            interactive: true,
        };
        let support = Osc99Support::default();
        let verdicts = infer(&identity("kitty"), Some(&support), measurement);
        assert_eq!(verdicts[0].support, Support::Yes);
        assert_eq!(verdicts[0].confidence, Confidence::Measured);
    }

    #[test]
    fn inference_reports_itself_as_inference() {
        let verdicts = infer(&identity("ghostty"), None, clean());
        for verdict in verdicts.iter().skip(1) {
            assert_eq!(verdict.confidence, Confidence::Inferred);
            assert!(verdict.reason.contains("Ghostty"), "{}", verdict.reason);
        }
        assert_eq!(
            verdicts[0].support,
            Support::No,
            "OSC 99 is unconfirmed in Ghostty, and here it was also asked"
        );
        assert_eq!(verdicts[1].support, Support::Yes, "OSC 777 is documented");
        assert_eq!(verdicts[2].support, Support::Yes, "OSC 9 is documented");
    }

    #[test]
    fn an_unrecognised_terminal_is_unknown_rather_than_unsupported() {
        // The one rule that must never bend: Unknown does not collapse into No.
        let measurement = Measurement {
            answered_barrier: false,
            multiplexed: false,
            interactive: true,
        };
        let verdicts = infer(&identity("some-new-terminal"), None, measurement);
        for verdict in &verdicts {
            assert_eq!(verdict.confidence, Confidence::Unknown);
            assert_eq!(verdict.support, Support::Unknown);
            assert_ne!(verdict.support, Support::No);
            assert!(verdict.reason.contains("nothing identified"));
        }
    }

    #[test]
    fn a_non_interactive_probe_concludes_nothing() {
        // Inference from the environment still identifies the terminal, but there
        // is no terminal on the other end to raise anything, so no claim is safe.
        let verdicts = infer(&identity("kitty"), None, Measurement::default());
        for verdict in &verdicts {
            assert_eq!(verdict.support, Support::Unknown, "{:?}", verdict.protocol);
        }
        assert!(verdicts[2].reason.contains("nothing there"), "{}", verdicts[2].reason);
    }

    #[test]
    fn verdicts_come_back_richest_first() {
        let verdicts = infer(&identity("kitty"), None, clean());
        let order: Vec<Protocol> = verdicts.iter().map(|verdict| verdict.protocol).collect();
        assert_eq!(order, Protocol::RICHEST_FIRST.to_vec());
    }

    #[test]
    fn the_exit_status_is_about_the_protocol_that_would_be_used() {
        let with = |token: &str, osc99: Option<&Osc99Support>, measurement| {
            let identity = identity(token);
            let mut outcome = ProbeOutcome::from_environment();
            outcome.verdicts = infer(&identity, osc99, measurement);
            outcome.identity = identity;
            outcome.osc99 = osc99.cloned();
            outcome.multiplexer = None;
            outcome.answered_barrier = measurement.answered_barrier;
            outcome.interactive = measurement.interactive;
            outcome
        };

        // kitty answers OSC 99, which is what would be used. Yes.
        let answered = Osc99Support::default();
        assert_eq!(
            with("kitty", Some(&answered), clean()).support(),
            Support::Yes
        );

        // iTerm2 has no OSC 99, so `auto` would use OSC 9 — which it does have.
        // The OSC 99 `No` must not become the answer.
        assert_eq!(with("iTerm.app", None, clean()).support(), Support::Yes);

        // Alacritty implements none of them, and was positively identified.
        assert_eq!(with("alacritty", None, clean()).support(), Support::No);

        // An unidentified terminal is a shrug, never a no.
        assert_eq!(
            with("some-new-terminal", None, clean()).support(),
            Support::Unknown
        );
    }

    #[test]
    fn exit_codes_follow_the_shared_convention() {
        assert_eq!(Support::Yes.exit_code(), ExitCode::SUCCESS);
        assert_eq!(Support::Yes.as_str(), "yes");
        assert_eq!(Support::No.as_str(), "no");
        assert_eq!(Support::Unknown.as_str(), "unknown");
    }

    #[test]
    fn json_is_one_self_contained_line() {
        let mut outcome = ProbeOutcome::from_environment();
        let identity = identity("kitty");
        let support = Osc99Support {
            payloads: vec!["title".to_owned(), "body".to_owned()],
            urgencies: vec!["0".to_owned(), "1".to_owned(), "2".to_owned()],
            close_events: true,
            ..Osc99Support::default()
        };
        outcome.verdicts = infer(&identity, Some(&support), clean());
        outcome.identity = identity;
        outcome.osc99 = Some(support);
        outcome.multiplexer = None;
        outcome.interactive = true;
        outcome.answered_barrier = true;

        let line = json_line(&outcome, outcome.support());
        assert!(line.ends_with('\n'), "one object per line");
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.starts_with("{\"feature\":\"notifications\""));
        assert!(line.contains("\"verdict\":\"yes\""));
        assert!(line.contains("\"protocol\":\"kitty\""));
        // All three appear regardless of which one the status is about.
        for protocol in Protocol::RICHEST_FIRST {
            assert!(line.contains(protocol.flag()), "{protocol:?} missing: {line}");
        }
        assert!(line.contains("\"terminal\":\"kitty\""));
        assert!(line.contains("\"close_events\":true"));
        // Nothing that would break a line-oriented reader.
        assert!(!line[..line.len() - 1].contains('\n'));
        assert!(!line.contains('\u{1b}'));
    }

    #[test]
    fn json_escapes_what_it_interpolates() {
        assert_eq!(escape_json("plain"), "plain");
        assert_eq!(escape_json("a\"b"), "a\\\"b");
        assert_eq!(escape_json("a\\b"), "a\\\\b");
        assert_eq!(escape_json("a\u{1b}b"), "a\\u001bb");
        assert_eq!(escape_json("a\nb"), "a\\u000ab");
    }

    #[test]
    fn a_hostile_term_variable_cannot_break_the_json() {
        // TERM is attacker-controlled in the sense that anything can set it, and
        // it is interpolated into the `reason` strings.
        let identity = Identity {
            known: None,
            source: "TERM".to_owned(),
            raw: "x\",\"verdict\":\"yes\"\u{1b}".to_owned(),
        };
        let mut outcome = ProbeOutcome::from_environment();
        outcome.verdicts = infer(&identity, None, clean());
        outcome.identity = identity;
        outcome.multiplexer = None;
        let line = json_line(&outcome, outcome.support());
        assert_eq!(line.matches("\"verdict\":").count(), 1, "{line}");
        assert!(!line.contains('\u{1b}'), "{line}");
    }

    #[test]
    fn xtversion_replies_are_reduced_to_a_name() {
        assert_eq!(parse_xtversion(">|kitty 0.36.0").as_deref(), Some("kitty"));
        assert_eq!(parse_xtversion(">|foot(1.16.2)").as_deref(), Some("foot"));
        assert_eq!(parse_xtversion(">|XTerm(389)").as_deref(), Some("XTerm"));
        assert_eq!(
            parse_xtversion(">|WezTerm 20240203-110809").as_deref(),
            Some("WezTerm")
        );
        assert_eq!(parse_xtversion(">|ghostty 1.0.1").as_deref(), Some("ghostty"));
        // Not an XTVERSION reply.
        assert_eq!(parse_xtversion("1$r2026h"), None);
        assert_eq!(parse_xtversion(">|"), None);
    }

    #[test]
    fn xtversion_names_route_into_the_same_table() {
        let name = parse_xtversion(">|XTerm(389)").expect("a name");
        assert_eq!(known_by_token(&name).map(|k| k.id), Some("xterm"));
    }

    #[test]
    fn the_demo_gate_is_any_protocol_at_all() {
        let with = |token: &str| {
            let identity = identity(token);
            let mut outcome = ProbeOutcome::from_environment();
            outcome.verdicts = infer(&identity, None, clean());
            outcome.identity = identity;
            outcome.osc99 = None;
            outcome.multiplexer = None;
            outcome.interactive = true;
            outcome.answered_barrier = true;
            outcome
        };

        let alacritty = with("alacritty");
        assert!(!alacritty.any_supported());
        assert!(
            alacritty.conclusive(),
            "Alacritty was identified, so 'no' is a real answer"
        );

        assert!(
            with("iTerm.app").any_supported(),
            "OSC 9 on its own is enough to run the demo"
        );

        // An unidentified terminal that answered the barrier has demonstrably no
        // OSC 99 — but that says nothing about the OSC 777 `auto` would fall back
        // to, so the overall answer stays a shrug rather than hardening into a no.
        let unknown = with("nonesuch");
        assert!(!unknown.any_supported());
        assert_eq!(
            unknown.verdict(Protocol::Osc99).map(|v| v.support),
            Some(Support::No),
            "OSC 99 was asked and demonstrably did not answer"
        );
        assert_eq!(unknown.support(), Support::Unknown);
        assert!(
            !unknown.conclusive(),
            "an unidentified terminal has told us nothing about what would be sent"
        );
    }

    #[test]
    fn replies_render_without_control_bytes() {
        let replies = [
            Reply::Osc("99;i=x:p=?;p=title,body:u=0,1,2".to_owned()),
            Reply::Dcs(">|kitty 0.36.0".to_owned()),
            Reply::Csi {
                params: "?62;4".to_owned(),
                final_byte: 'c',
            },
        ];
        for reply in &replies {
            let rendered = describe_reply(reply);
            assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
            assert!(!rendered.contains('\u{7}'), "{rendered:?}");
        }
    }
}
