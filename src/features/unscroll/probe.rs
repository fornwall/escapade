//! Inference, not measurement — because there is nothing to measure.
//!
//! `CSI Ps + T` is write-only. There is no query for support, no reply, no
//! acknowledgement, and no side effect an application can read back: the terminal
//! that unscrolled and the terminal that discarded the sequence are byte-for-byte
//! identical from the sending end. Worse than [`crate::features::progress`], whose
//! effect at least happens somewhere the user can see — here the effect is *on the
//! grid*, and reading the grid back means a cursor-position round trip whose answer
//! would be the same either way, since the cursor deliberately does not move.
//!
//! So this probe does what it can honestly do:
//!
//! 1. Asks the question that *does* have an answer — XTVERSION (`CSI > q`) behind
//!    the Primary Device Attributes barrier that
//!    [`crate::features::glyph_protocol::probe`] uses, so a silent terminal can be
//!    told apart from a slow one. Every reply it provokes is drained, because an
//!    unread one is delivered to whatever runs next and printed as garbage.
//! 2. Collects the ambient evidence: `TERM`, `TERM_PROGRAM` and the per-terminal
//!    environment markers.
//! 3. Looks the identified terminal up in the support matrix, which for this
//!    feature is short and well cited: three implementations, and a dozen negatives
//!    read out of the other terminals' own parsers.
//!
//! The identification is shared with [`crate::features::progress::probe`] rather
//! than copied. That table has been wrong in two places at once before, which is
//! exactly the argument for having one of it.
//!
//! The verdict is stingy in the house style. [`Support::Yes`] requires the terminal
//! to have **named itself**, in band, in its XTVERSION reply; an environment
//! variable is somebody else's claim about what is on the far end of the pipe, and
//! after an `ssh` hop or a `sudo -E` it is frequently a stale one. Everything
//! resting on the environment alone comes back [`Support::Unknown`] with a reason
//! saying so — including a detected multiplexer, which is *probably* fatal for this
//! feature but not provably so from a variable.

use std::{
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
        progress::probe::{
            Evidence, barrier_query, collect_evidence, parse_version, parse_xtversion,
            version_at_least, version_query,
        },
        unscroll::FEATURE_ID,
        wrap,
    },
    input::{InputReader, Reply},
    term::{Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// How long to wait for the terminal to answer. Generous enough for a slow ssh
/// link, and under the barrier the wait almost always ends early.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to spend consuming a reply still in flight after a conclusion.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// What the probe concluded, and everything it concluded it from.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// Whether stdin and stdout are both terminals. Without that there is nobody
    /// to ask and no screen to unscroll.
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

    /// Whether the terminal named a version older than the one that gained this.
    ///
    /// Only consulted when both versions are known: an absent version is not
    /// evidence of an old one. It matters here because two of the three
    /// implementations are version-gated — kitty 0.20.2 and mintty 3.5.2 — so a
    /// name alone cannot answer the question.
    #[must_use]
    pub fn predates_support(&self) -> bool {
        match (
            self.xtversion.as_deref().and_then(parse_version),
            self.since.as_deref().and_then(parse_version),
        ) {
            (Some(found), Some(wanted)) => !version_at_least(&found, &wanted),
            _ => false,
        }
    }

    /// The answer, as the exit status will report it.
    ///
    /// `Yes` demands the terminal named itself in band *and* the matrix row for
    /// that name says the feature works. Anything less is `Unknown`: this protocol
    /// cannot be measured, and an environment variable is not the terminal talking.
    ///
    /// `No` is claimed only from the terminal's own words — a matrix row saying it
    /// does not implement this, or a self-reported version predating the one that
    /// gained it. Note what is deliberately *not* here: a detected multiplexer.
    /// Inside tmux this almost certainly does not work, because the multiplexer
    /// owns the scrollback and tmux's CSI table has no `+` entry at all, but `$TMUX`
    /// in the environment is still the environment's word rather than tmux's own,
    /// and folding that into a `No` is the overclaiming this tool exists to avoid.
    /// When tmux answers XTVERSION it identifies itself and the matrix supplies the
    /// `No` properly.
    #[must_use]
    pub fn support(&self) -> Support {
        if self.in_band_evidence().is_none() {
            return Support::Unknown;
        }
        if self.predates_support() {
            return Support::No;
        }
        match self.status {
            // `partial` would still be a working unscroll with gaps named in
            // `notes`; no row currently uses it for this feature.
            Some(SupportStatus::Yes | SupportStatus::Partial) => Support::Yes,
            Some(SupportStatus::No) => Support::No,
            // Behind a flag, or a multiplexer that must be configured to forward
            // it: whether it works here is exactly what cannot be seen.
            Some(SupportStatus::Flag | SupportStatus::Passthrough | SupportStatus::Unknown)
            | None => Support::Unknown,
        }
    }

    /// Why the verdict is what it is, in one sentence.
    #[must_use]
    pub fn reason(&self) -> String {
        if !self.interactive {
            return "stdout and stdin are not both terminals, so the terminal could not be asked \
                    to name itself — and there would be no screen to unscroll either"
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
                None => "nothing identified this terminal, and this sequence has no query to fall \
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
                "{name} reported itself as {}, and the matrix says unscrolling arrived in {}",
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
}

// ---------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------

/// Runs the probe using an existing terminal and reader.
///
/// Takes both rather than creating them because there must only ever be one
/// [`InputReader`] per process — two would race for the same bytes on stdin, and
/// each would see a random half of the terminal's answer.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal that
/// stays quiet is not an error; for this sequence there was never much to hear.
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

        // XTVERSION can arrive first and end the wait, leaving the barrier reply in
        // flight. Unread, it lands in whatever runs next — the user's shell, which
        // prints it as stray text. Consuming it is what keeps a probe that is
        // silent by design actually silent.
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
/// The last step of every path, kept in one place so the piped case and the
/// interactive case cannot disagree about what the evidence meant.
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
    // Restore before the caller prints anything, so any report appears in a normal
    // cooked-mode terminal.
    term.restore();
    outcome
}

/// Runs `escapade unscroll probe`, returning the exit status that is the answer.
///
/// Silent unless a `--format` was asked for: `0` supported, `1` not supported, `2`
/// cannot tell. That makes it usable as a shell predicate, which is the natural
/// thing to want from a key binding deciding whether to bother.
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
/// `measurable` is always `false`, and it is the most important field here: a
/// script reading this needs to know that `"verdict":"unknown"` is the *normal*
/// answer for this feature rather than a probe that went wrong.
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
/// Absent is not empty: `"xtversion":null` means the terminal never named itself,
/// which is the difference between an inference and a measurement.
fn quoted_or_null(value: Option<&str>) -> String {
    value.map_or_else(
        || "null".to_owned(),
        |text| format!("\"{}\"", escape_json(text)),
    )
}

/// Minimal JSON string escaping, for the few strings that reach the output.
///
/// Environment variables are user data — `TERM_PROGRAM` can hold a quote, a
/// backslash or an escape byte — so this escapes rather than hopes. That is also
/// what stops a hostile variable from carrying a raw `ESC` into the reader's
/// terminal by way of our own output.
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
    let width = theme.width().saturating_sub(6);

    println!();
    println!(
        "  {}",
        theme.paint("Unscroll support (CSI Ps + T)", Ink::Title)
    );
    println!();

    for line in wrap(
        "This one cannot be measured. The sequence is write-only — no query, no reply, no \
         acknowledgement — and the only thing it changes is the grid, which it rearranges without \
         moving the cursor, so there is nothing to read back either. What follows is inference, and \
         a yes is claimed only when the terminal named itself in band.",
        width,
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
    for line in wrap(&outcome.reason(), width) {
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
    } else {
        // Nothing was sent, so "nothing came back" would be a lie about a question
        // that was never asked.
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
    let label_width = outcome
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
            theme.paint(&format!("{:<label_width$}", item.source), Ink::Accent),
            theme.paint(&format!("{:<32}", truncate(&item.value, 32)), Ink::Code),
        );
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
                 is not a no — it is an absence of evidence, and since an unimplemented CSI \
                 sequence is discarded silently, sending the unscroll anyway costs nothing.",
                width,
            ) {
                println!("      {}", theme.paint(&line, Ink::Muted));
            }
        }
    }

    if let Some(multiplexer) = outcome.multiplexer {
        println!();
        println!(
            "  {} {}",
            theme.paint("note:", Ink::Warn),
            theme.paint(multiplexer.name(), Ink::Warn)
        );
        for line in wrap(
            "Inside a multiplexer the scrollback belongs to the multiplexer, not to the terminal \
             underneath, so this feature has to be implemented by the thing holding the history. \
             tmux does not implement it and passthrough cannot help. This is not folded into the \
             verdict, because an environment variable is not the multiplexer's own word.",
            width,
        ) {
            println!("      {}", theme.paint(&line, Ink::Muted));
        }
    }

    println!();
    println!("  {}", theme.paint("what would be sent", Ink::Accent));
    println!(
        "    {}  {}",
        theme.paint(&pretty(&super::sequence(3)), Ink::Code),
        theme.paint("three lines back down; no query precedes it", Ink::Muted)
    );
    println!();
    for line in wrap(
        "Send it anyway. A terminal without this sequence discards it, so being wrong costs \
         nothing — which is why this probe is worth reading and rarely worth branching on. What is \
         worth knowing is where it will not help: on the alternate screen it inserts blank lines \
         instead, and after a pager exits there is nothing of the pager in scrollback to recover.",
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
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::progress::probe::registry_id_for;

    /// An outcome that was told `name` by the terminal itself.
    fn in_band(name: &str, status: Option<SupportStatus>, since: Option<&str>) -> ProbeOutcome {
        ProbeOutcome {
            interactive: true,
            xtversion: Some(name.to_owned()),
            answered_barrier: true,
            identified: registry_id_for(name),
            terminal_name: Some(name.to_owned()),
            status,
            since: since.map(str::to_owned),
            evidence: vec![Evidence {
                source: "XTVERSION",
                value: name.to_owned(),
                implies: registry_id_for(name),
                in_band: true,
                caveat: None,
            }],
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

    // -- the verdict --------------------------------------------------------

    #[test]
    fn an_in_band_name_with_a_yes_row_is_a_yes() {
        let outcome = in_band("kitty 0.36.0", Some(SupportStatus::Yes), Some("0.20.2"));
        assert_eq!(outcome.support(), Support::Yes);
        assert_eq!(exit_status_number(outcome.support()), 0);
    }

    #[test]
    fn a_version_older_than_the_matrix_row_is_a_definite_no() {
        // Unscrolling arrived in kitty 0.20.2; 0.19.3 predates it.
        let old = in_band("kitty 0.19.3", Some(SupportStatus::Yes), Some("0.20.2"));
        assert!(old.predates_support());
        assert_eq!(old.support(), Support::No);
        assert!(old.reason().contains("0.20.2"), "{}", old.reason());

        // mintty 3.5.2, and the release string in the matrix carries a date the
        // version parser must not trip over.
        let old = in_band("mintty 3.5.1", Some(SupportStatus::Yes), Some("3.5.2"));
        assert_eq!(old.support(), Support::No);
        let new = in_band("mintty 3.7.7", Some(SupportStatus::Yes), Some("3.5.2"));
        assert_eq!(new.support(), Support::Yes);
    }

    #[test]
    fn a_missing_version_is_not_evidence_of_an_old_one() {
        // contour has no `since` in the matrix, and saying no on that basis would
        // be inventing a fact.
        let contour = in_band("contour 0.4.3", Some(SupportStatus::Yes), None);
        assert!(!contour.predates_support());
        assert_eq!(contour.support(), Support::Yes);

        let nameless = in_band("someterm", Some(SupportStatus::Yes), Some("1.2"));
        assert!(!nameless.predates_support());
    }

    #[test]
    fn a_matrix_no_from_an_in_band_name_is_a_no() {
        // VTE knows this sequence and deliberately ignores it, which is the
        // clearest `no` in the matrix.
        let outcome = in_band("ghostty 1.2.0", Some(SupportStatus::No), None);
        assert_eq!(outcome.support(), Support::No);
        assert!(
            outcome.reason().contains("XTVERSION"),
            "{}",
            outcome.reason()
        );
    }

    #[test]
    fn yes_requires_the_terminal_to_have_named_itself() {
        // The same terminal, the same matrix row, two kinds of evidence.
        let guessed = ambient("kitty", Some(SupportStatus::Yes));
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
    fn an_ambient_no_is_also_only_unknown() {
        // The asymmetry is deliberate: ambient evidence licenses no claim at all,
        // in either direction.
        let guessed = ambient("alacritty", Some(SupportStatus::No));
        assert_eq!(guessed.support(), Support::Unknown);
    }

    #[test]
    fn a_flag_or_a_passthrough_row_leaves_it_unknown() {
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
    fn a_multiplexer_in_the_environment_never_collapses_into_a_no() {
        // Inside tmux this almost certainly does not work — tmux owns the
        // scrollback and implements no `+` intermediate — but `$TMUX` is the
        // environment's word, not tmux's, so the honest answer is that we cannot
        // tell. When tmux answers XTVERSION the matrix supplies the `no` properly.
        let outcome = ProbeOutcome {
            interactive: true,
            multiplexer: Some(Multiplexer::Tmux),
            ..ProbeOutcome::default()
        };
        assert_eq!(outcome.support(), Support::Unknown);

        let named = in_band("tmux 3.5a", Some(SupportStatus::No), None);
        assert_eq!(named.support(), Support::No);
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
    }

    // -- the matrix this probe reads ----------------------------------------

    #[test]
    fn every_terminal_the_identification_can_name_has_a_support_row() {
        // The probe answers from the matrix, so a terminal it can identify but has
        // no row for silently degrades to `unknown`.
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        for name in [
            "kitty",
            "mintty",
            "contour",
            "ghostty",
            "wezterm",
            "alacritty",
            "foot",
            "xterm",
            "konsole",
            "windows terminal",
            "vscode",
            "vte",
            "rio",
            "tmux",
            "zellij",
            "screen",
        ] {
            let id = registry_id_for(name).unwrap_or_else(|| panic!("{name} matched nothing"));
            assert!(
                feature.support.iter().any(|row| row.terminal == id),
                "no unscroll support row for {id} (from {name})"
            );
        }
    }

    // -- output -------------------------------------------------------------

    #[test]
    fn json_is_one_line_and_escapes_what_it_must() {
        let mut outcome = in_band("kitty 0.36.0", Some(SupportStatus::Yes), Some("0.20.2"));
        outcome.evidence.push(Evidence {
            source: "TERM_PROGRAM",
            // Hostile: a quote, a backslash and an escape byte, all of which a
            // user can put in an environment variable.
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
        assert!(line.contains("\"feature\":\"unscroll\""));
        assert!(line.contains("\"verdict\":\"yes\""));
        assert!(line.contains("\"measurable\":false"));
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
        assert!(!escape_json(&all).chars().any(char::is_control));
        assert_eq!(escape_json("plain"), "plain");
        assert_eq!(escape_json("\u{7f}"), "\\u007f");
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
            Reply::Dcs(">|kitty 0.36.0".to_owned()),
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
