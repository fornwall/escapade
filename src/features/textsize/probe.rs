//! Per-key capability detection, by measurement.
//!
//! This protocol has no query. It only ever *tells* the terminal a size, so
//! there is no sequence to ask with and no reply to parse — and the
//! specification's own support detection says so plainly: send a `CR`, a Cursor
//! Position Report, `OSC 66 ; w=2 ; <space> ST`, another CPR, then
//! `OSC 66 ; s=2 ; <space> ST` and a third CPR, and compare the three positions.
//!
//! ```text
//! →  CR CSI K                   column 1, wipe the scratch line
//! →  CSI 6 n                    where are we?
//! ←  CSI <row> ; <col> R        here
//! →  OSC 66 ; w=2 ; <space> ST  a space, in two cells
//! →  CSI 6 n
//! ←  CSI <row> ; <col> R        the difference is what the terminal did
//! →  CR CSI J                   wipe it again
//! ```
//!
//! That produces three distinguishable outcomes per key rather than two, which
//! is the whole reason to measure rather than guess. For `w=2` around a single
//! space:
//!
//! | cells | what it means |
//! |-------|---------------|
//! | 2 | the key was honoured |
//! | 1 | the text was drawn, the key ignored |
//! | 0 | the whole OSC string was discarded — the text is *lost* |
//!
//! The last row is the one that matters most in practice, and it is why this
//! module exists: on a terminal that has never heard of `OSC 66`, text sent
//! through it does not degrade to plain text, it **disappears**. An unknown OSC
//! string is parsed and thrown away, payload and all.
//!
//! # Why the verdict is per key
//!
//! `w=` fixes the width-disagreement bug and needs no new rendering; `s=` needs
//! a font renderer that can draw at arbitrary sizes and composite a glyph across
//! several cells. They are separable, and terminals have separated them: foot
//! implements `w` only. A single boolean would report foot as unsupported and be
//! wrong about the thing foot does.
//!
//! # What cannot be measured
//!
//! `n`, `d`, `v` and `h` — deliberately. Fractional scaling explicitly does
//! **not** change the number of cells the text occupies; it changes the font
//! size inside them, and alignment moves the result around inside the block.
//! Nothing about that is visible to a Cursor Position Report, so a terminal
//! could honour `w` and `s` and silently ignore the other four, and no
//! measurement anywhere could tell. [`Key::MEASURABLE`] is the honest limit of
//! this probe, and the report says so rather than implying the other keys were
//! checked.
//!
//! # The barrier
//!
//! `CSI c` is sent after the measurements, and its answer is recorded — but it
//! never licenses a negative here. Every "no" in this module comes from a
//! measurement, which is stronger evidence than a barrier could be: the terminal
//! told us where the cursor ended up. What the barrier buys is telling
//! *silence* apart from *no terminal*: if no CPR came back but the device
//! attributes did, this terminal does not implement CPR, and the answer is
//! [`Support::Unknown`] rather than a false negative. Inside a multiplexer the
//! barrier is answered by the multiplexer itself, so it says even less — which
//! costs nothing, because it was never load-bearing.
//!
//! Specification: <https://sw.kovidgoyal.net/kitty/text-sizing-protocol/>

use std::{
    fmt::Write as _,
    io::{IsTerminal as _, stdin, stdout},
    process::ExitCode,
    time::Duration,
};

use anyhow::Result;

use crate::{
    cli::ProbeFormat,
    features::{
        Support,
        grapheme::measure::{cells_between, parse_cursor_position},
        textsize::{Key, scale_detection, width_detection},
        wrap,
    },
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// How long to wait for a reply. A full second: the brief for every probe in
/// this project is that a slow ssh link must not look like an unsupported
/// feature, and a wrong negative here is worse than a pause.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to spend collecting the barrier reply once measuring is done.
///
/// Short, because by this point the terminal has answered two CPRs and its
/// device-attributes reply is already in flight.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(150);

/// Cursor Position Report request: `CSI 6 n`.
fn position_query() -> String {
    format!("{CSI}6n")
}

/// Primary Device Attributes request: `CSI c` — the barrier.
fn barrier() -> String {
    format!("{CSI}c")
}

impl Key {
    /// The keys a Cursor Position Report can actually decide.
    ///
    /// `w` first, because it is the one a partial implementation has, and
    /// because its verdict doubles as the answer to "does any text sent through
    /// `OSC 66` survive at all".
    pub const MEASURABLE: [Self; 2] = [Self::Width, Self::Scale];

    /// The detection sequence for this key, when it has one.
    #[must_use]
    pub fn detection(self) -> Option<String> {
        match self {
            Self::Width => Some(width_detection()),
            Self::Scale => Some(scale_detection()),
            _ => None,
        }
    }

    /// How many cells the detection sequence asks for.
    ///
    /// Two, for both: `w=2` puts a space in two cells side by side, and `s=2`
    /// puts one in a two-by-two block, which is also two cells wide.
    #[must_use]
    pub fn detection_cells(self) -> usize {
        match self {
            Self::Width | Self::Scale => 2,
            _ => 0,
        }
    }

    /// How many rows the detection sequence occupies.
    ///
    /// `s=2` draws two rows tall, which matters: if the scratch line is the last
    /// row of the screen, the specification says the terminal must *discard* a
    /// block that does not fit, and the measurement would report a false
    /// negative. So the caller has to make room first.
    #[must_use]
    pub fn detection_rows(self) -> u16 {
        match self {
            Self::Scale => 2,
            _ => 1,
        }
    }
}

// ---------------------------------------------------------------------------
// What one key's measurement showed
// ---------------------------------------------------------------------------

/// What a measurement proved about one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The terminal used exactly the cells the key asked for.
    Honoured,
    /// The text was drawn, but in the cells it would have taken anyway: the
    /// escape code was parsed and the key was not acted on.
    Ignored,
    /// Nothing was drawn at all. The OSC string was parsed as an unknown command
    /// and discarded, text and all — so anything sent this way is lost.
    Discarded,
    /// The terminal answered with a count nobody predicted.
    Disagrees(usize),
    /// Nothing answered, so nothing is known. Not the same as a no.
    Unknown,
}

impl Verdict {
    /// Short label for a report line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Honoured => "honoured",
            Self::Ignored => "ignored",
            Self::Discarded => "discarded",
            Self::Disagrees(_) => "disagrees",
            Self::Unknown => "unknown",
        }
    }

    /// Symbol chosen to survive a monospace grid, matching the support matrix.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Honoured => "✔",
            Self::Ignored | Self::Discarded => "✘",
            Self::Disagrees(_) => "◐",
            Self::Unknown => "?",
        }
    }

    /// What this verdict entitles a caller to claim.
    ///
    /// Everything except [`Verdict::Unknown`] is a measurement, and a
    /// measurement is a conclusion: the terminal was asked for two cells and
    /// reported what it did. Only a missing measurement is inconclusive.
    #[must_use]
    pub fn support(self) -> Support {
        match self {
            Self::Honoured => Support::Yes,
            Self::Ignored | Self::Discarded | Self::Disagrees(_) => Support::No,
            Self::Unknown => Support::Unknown,
        }
    }

    /// One sentence a person can act on.
    #[must_use]
    pub fn explanation(self, key: Key) -> String {
        let letter = key.letter();
        match self {
            Self::Honoured => format!(
                "The terminal used exactly the {} cells `{letter}=2` asked for.",
                key.detection_cells()
            ),
            Self::Ignored => format!(
                "The text was drawn but took the cells it would have taken anyway, so `{letter}` \
                 was parsed and not acted on. Text sent this way still appears — it just is not \
                 sized."
            ),
            Self::Discarded => "Nothing was drawn: the cursor did not move. The whole OSC string \
                 was treated as an unknown command and thrown away, so text sent through `OSC 66` \
                 is lost here rather than shown unsized."
                .to_owned(),
            Self::Disagrees(cells) => format!(
                "The terminal used {cells} cells, which is neither the {} asked for nor the 1 a \
                 plain space takes. Something between here and the screen is counting \
                 differently.",
                key.detection_cells()
            ),
            Self::Unknown => {
                "No cursor position came back, so nothing was measured. That is not a no."
                    .to_owned()
            }
        }
    }
}

/// One key, and what measuring it showed.
#[derive(Debug, Clone)]
pub struct KeyReport {
    /// The key that was tested.
    pub key: Key,
    /// The sequence that was sent.
    pub sequence: String,
    /// Cells the key asked for.
    pub asked: usize,
    /// Cells the same text takes without the protocol — one, for a space.
    pub plain: usize,
    /// Cells the terminal actually used, when it said.
    pub measured: Option<usize>,
}

impl KeyReport {
    /// A report for a key that could not be measured at all.
    #[must_use]
    pub fn unmeasured(key: Key) -> Self {
        Self {
            key,
            sequence: key.detection().unwrap_or_default(),
            asked: key.detection_cells(),
            plain: 1,
            measured: None,
        }
    }

    /// What the measurement proved.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        let Some(measured) = self.measured else {
            return Verdict::Unknown;
        };
        if measured == self.asked {
            Verdict::Honoured
        } else if measured == 0 {
            Verdict::Discarded
        } else if measured == self.plain {
            Verdict::Ignored
        } else {
            Verdict::Disagrees(measured)
        }
    }

    /// What this key's measurement entitles a caller to claim.
    #[must_use]
    pub fn support(&self) -> Support {
        self.verdict().support()
    }
}

// ---------------------------------------------------------------------------
// The outcome
// ---------------------------------------------------------------------------

/// Everything one probe run established.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// One entry per key that was asked about, in the order they were measured.
    pub reports: Vec<KeyReport>,
    /// Whether the Primary Device Attributes barrier came back. Diagnostic only
    /// — see the module documentation.
    pub answered_barrier: bool,
    /// Multiplexer between this process and the real terminal, if any.
    pub multiplexer: Option<Multiplexer>,
    /// Whether stdin and stdout are both terminals. Without that there is
    /// nothing to measure.
    pub interactive: bool,
}

impl ProbeOutcome {
    /// The report for one key, if it was asked about.
    #[must_use]
    pub fn report(&self, key: Key) -> Option<&KeyReport> {
        self.reports.iter().find(|report| report.key == key)
    }

    /// What was concluded about one key.
    #[must_use]
    pub fn support_for(&self, key: Key) -> Support {
        self.report(key)
            .map_or(Support::Unknown, KeyReport::support)
    }

    /// The single answer the exit status carries.
    ///
    /// **Every key asked about has to be supported**, because the question a
    /// bare `escapade textsize probe` asks is about the protocol, and the
    /// protocol's headline capability is scaling. So on foot — `w` yes, `s` no —
    /// this is [`Support::No`], while `--format json` still reports `w` as a
    /// yes. Collapsing a partial implementation to "yes" would make the
    /// predicate useless for its main job, which is deciding whether to draw a
    /// four-cell-tall banner.
    ///
    /// One measurement missing outranks any number of definite noes: a run that
    /// could not ask about half the protocol has not established that the
    /// protocol is absent.
    #[must_use]
    pub fn support(&self) -> Support {
        if self.reports.is_empty() {
            return Support::Unknown;
        }
        if self.reports.iter().any(|report| report.measured.is_none()) {
            return Support::Unknown;
        }
        if self
            .reports
            .iter()
            .all(|report| report.support() == Support::Yes)
        {
            Support::Yes
        } else {
            Support::No
        }
    }

    /// Whether any measured key showed the OSC string being thrown away.
    ///
    /// This is the question the rendering path needs answered, and it is not the
    /// same as "unsupported": a terminal that ignores `s=` still *prints* the
    /// text, so sending the sequence costs nothing. A terminal that discards the
    /// string loses the text entirely, and something else has to be printed
    /// instead.
    #[must_use]
    pub fn text_would_be_lost(&self) -> bool {
        self.reports
            .iter()
            .any(|report| report.verdict() == Verdict::Discarded)
    }

    /// Whether the run reached a conclusion either way.
    #[must_use]
    pub fn conclusive(&self) -> bool {
        self.support() != Support::Unknown
    }
}

// ---------------------------------------------------------------------------
// Measuring
// ---------------------------------------------------------------------------

/// A measuring session: raw mode, and one scratch line used over and over.
///
/// The approach is [`crate::features::grapheme::measure`]'s, and the two
/// non-obvious pieces of arithmetic — parsing a Cursor Position Report and
/// turning two positions into a cell count across a wrap — are imported from
/// there rather than written again. What is different is the timeout (a full
/// second, not 400ms) and the row reservation below, which that module does not
/// need because nothing it prints is more than one row tall.
///
/// Exactly one of these may exist at a time, because it borrows the process's
/// only [`InputReader`]: two would race for the same bytes on stdin and each
/// would see a random half of the terminal's answers.
#[derive(Debug)]
struct Measurer<'a> {
    term: &'a mut Term,
    input: &'a mut InputReader,
    /// Terminal width, needed to turn a wrapped cursor position into a count.
    cols: usize,
    /// Terminal height, needed to know when a tall block would not fit.
    rows: u16,
    /// Set once a Cursor Position Report goes unanswered.
    ///
    /// A terminal that ignores the first CPR will ignore every later one, and
    /// each attempt costs a full [`REPLY_TIMEOUT`]. Trying again only buys
    /// timeouts.
    silent: bool,
}

impl<'a> Measurer<'a> {
    /// Borrows a terminal and reader for measuring. Assumes raw mode is on.
    fn new(term: &'a mut Term, input: &'a mut InputReader) -> Self {
        let cols = usize::from(term.cols).max(1);
        let rows = term.rows.max(1);
        Self {
            term,
            input,
            cols,
            rows,
            silent: false,
        }
    }

    /// How many cells `sequence` occupied — measured, not predicted.
    ///
    /// `rows_needed` is the height of what is about to be drawn. A block taller
    /// than the space left below the cursor must be discarded by the terminal
    /// per the specification, so the rows are reserved first by scrolling; that
    /// turns a false negative into a real measurement.
    ///
    /// `Ok(None)` means the terminal never answered, or that the screen moved
    /// under us in a way that makes the arithmetic a guess. Neither is a width
    /// of zero, and the distinction is the whole point.
    fn measure(&mut self, sequence: &str, rows_needed: u16) -> Result<Option<usize>> {
        if self.silent {
            return Ok(None);
        }

        // Column 1 of a line we are about to own, wiped so a previous
        // measurement cannot be mistaken for this one.
        self.term.write("\r")?;
        self.term.write(&format!("{CSI}K"))?;
        self.term.write_now(&position_query())?;
        let Some((row, _)) = self.cursor_position() else {
            self.silent = true;
            return Ok(None);
        };

        // Make room below, if a tall block would otherwise not fit on screen.
        let bottom = row.saturating_add(rows_needed.saturating_sub(1));
        let start = if bottom > self.rows {
            let deficit = bottom - self.rows;
            // In raw mode a newline is a bare LF: it moves down, scrolling at
            // the bottom margin, and leaves the column alone. Coming back up
            // afterwards lands on the same content with blank rows beneath it.
            for _ in 0..deficit {
                self.term.write("\n")?;
            }
            self.term.write(&format!("{CSI}{deficit}A"))?;
            self.term.write("\r")?;
            self.term.write_now(&position_query())?;
            let Some(position) = self.cursor_position() else {
                self.silent = true;
                return Ok(None);
            };
            position
        } else {
            (row, 1)
        };

        self.term.write(sequence)?;
        self.term.write_now(&position_query())?;
        let end = self.cursor_position();

        // Erase what was drawn — including the rows below, which a scaled block
        // occupies — before looking at the numbers, so the line is clean even if
        // we bail out.
        self.term.write("\r")?;
        self.term.write_now(&format!("{CSI}J"))?;

        let Some(end) = end else {
            self.silent = true;
            return Ok(None);
        };
        Ok(cells_between(start, end, self.cols))
    }

    /// Reads one Cursor Position Report, returning 1-based `(row, col)`.
    fn cursor_position(&mut self) -> Option<(u16, u16)> {
        let reply = self
            .input
            .wait_for_reply(REPLY_TIMEOUT, |reply| {
                matches!(
                    reply,
                    Reply::Csi {
                        final_byte: 'R',
                        ..
                    }
                )
            })
            .ok()
            .flatten();
        let Some(Reply::Csi { params, .. }) = reply else {
            return None;
        };
        parse_cursor_position(&params)
    }
}

/// Measures each of `keys` using an existing terminal and reader.
///
/// Takes both rather than creating them because there must only ever be one
/// [`InputReader`] per process — two would race for the same bytes on stdin.
///
/// The sequences are deliberately **not** wrapped for a multiplexer. Inside tmux
/// or Zellij the multiplexer is the thing laying out cells, so its answer is the
/// one an application needs: if tmux discards `OSC 66` then text sent through it
/// is lost, whatever the terminal beyond tmux would have done. Forwarding the
/// sequence past the multiplexer would be worse than useless — it would leave
/// the multiplexer believing a cell holds one character while the terminal drew a
/// four-by-four block over it.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal that
/// stays quiet is a result, not an error.
pub fn probe(term: &mut Term, input: &mut InputReader, keys: &[Key]) -> Result<ProbeOutcome> {
    let mut outcome = ProbeOutcome {
        multiplexer: Multiplexer::detect(),
        interactive: stdout().is_terminal() && stdin().is_terminal(),
        ..ProbeOutcome::default()
    };
    if !outcome.interactive {
        // No terminal to ask, so every key stays unmeasured rather than being
        // reported as absent.
        outcome.reports = keys.iter().copied().map(KeyReport::unmeasured).collect();
        return Ok(outcome);
    }

    // Raw mode is required: in cooked mode the reply would sit in the line
    // buffer until the user pressed Enter, and would be echoed at them.
    term.enable_raw()?;
    term.hide_cursor()?;
    input.drain();

    {
        let mut measurer = Measurer::new(term, input);
        for &key in keys {
            let mut report = KeyReport::unmeasured(key);
            if !report.sequence.is_empty() {
                report.measured = measurer.measure(&report.sequence, key.detection_rows())?;
            }
            outcome.reports.push(report);
        }
    }

    // The barrier goes last. Unlike a query-based probe it proves nothing about
    // support — the measurements did that — but it distinguishes a terminal that
    // does not implement CPR from nothing listening at all, which is the
    // difference between "unknown" and "unknown, and here is why".
    term.write_now(&barrier())?;
    if let Ok(Some(_)) = input.wait_for_reply(DRAIN_TIMEOUT, |reply| {
        matches!(
            reply,
            Reply::Csi {
                final_byte: 'c',
                ..
            }
        )
    }) {
        outcome.answered_barrier = true;
    }
    // Anything still in flight belongs to us and must not be delivered to
    // whatever runs next, which would print it as stray text.
    input.drain();

    term.show_cursor()?;
    Ok(outcome)
}

/// Runs a probe from scratch.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn detect(keys: &[Key]) -> Result<ProbeOutcome> {
    if !(stdout().is_terminal() && stdin().is_terminal()) {
        // `Term::new` needs a terminal to read a size from, and creating an
        // `InputReader` starts a thread on stdin. Neither is worth doing to
        // learn what is already known.
        return Ok(ProbeOutcome {
            reports: keys.iter().copied().map(KeyReport::unmeasured).collect(),
            multiplexer: Multiplexer::detect(),
            interactive: false,
            answered_barrier: false,
        });
    }
    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let outcome = probe(&mut term, &mut input, keys);
    // Restore before the caller prints anything, so a report appears in a normal
    // cooked-mode terminal.
    term.restore();
    outcome
}

// ---------------------------------------------------------------------------
// `escapade textsize probe`
// ---------------------------------------------------------------------------

/// Renders `escapade textsize probe`, and returns the exit status that *is* the
/// answer.
///
/// With no `--format` this prints nothing whatsoever, so it composes:
///
/// ```sh
/// if escapade textsize probe; then banner --big; fi
/// ```
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn run(theme: &Theme, format: Option<ProbeFormat>) -> Result<ExitCode> {
    let outcome = detect(&Key::MEASURABLE)?;
    match format {
        None => {}
        Some(ProbeFormat::Json) => print_json(&outcome),
        Some(ProbeFormat::Debug) => print_debug(theme, &outcome),
    }
    Ok(outcome.support().exit_code())
}

/// JSON Lines: one self-contained object per key, then one for the protocol.
///
/// Hand-rolled because this crate has no JSON serialiser and one object is not
/// worth a dependency. Every string that could contain anything unexpected goes
/// through [`json_string`].
fn print_json(outcome: &ProbeOutcome) {
    for report in &outcome.reports {
        let measured = match report.measured {
            Some(cells) => cells.to_string(),
            None => "null".to_owned(),
        };
        println!(
            "{{\"feature\":\"{}\",\"scope\":\"key\",\"key\":\"{}\",\"name\":{},\"support\":\"{}\",\"verdict\":\"{}\",\"asked\":{},\"plain\":{},\"measured\":{},\"sequence\":{}}}",
            super::FEATURE_ID,
            report.key.letter(),
            json_string(report.key.name()),
            report.support().as_str(),
            report.verdict().label(),
            report.asked,
            report.plain,
            measured,
            json_string(&pretty(&report.sequence)),
        );
    }

    let keys: Vec<String> = outcome
        .reports
        .iter()
        .map(|report| {
            format!(
                "\"{}\":\"{}\"",
                report.key.letter(),
                report.support().as_str()
            )
        })
        .collect();
    let unmeasurable: Vec<String> = Key::ALL
        .iter()
        .filter(|key| !Key::MEASURABLE.contains(key))
        .map(|key| format!("\"{}\"", key.letter()))
        .collect();
    let multiplexer = match outcome.multiplexer {
        Some(multiplexer) => json_string(multiplexer.name()),
        None => "null".to_owned(),
    };
    println!(
        "{{\"feature\":\"{}\",\"scope\":\"protocol\",\"support\":\"{}\",\"keys\":{{{}}},\"unmeasurable\":[{}],\"text_would_be_lost\":{},\"interactive\":{},\"multiplexer\":{},\"answered_barrier\":{},\"term\":{}}}",
        super::FEATURE_ID,
        outcome.support().as_str(),
        keys.join(","),
        unmeasurable.join(","),
        outcome.text_would_be_lost(),
        outcome.interactive,
        multiplexer,
        outcome.answered_barrier,
        json_string(&std::env::var("TERM").unwrap_or_default()),
    );
}

/// Quotes and escapes a string for JSON, per RFC 8259.
///
/// `TERM` and a multiplexer name are the only variable strings that reach here,
/// but they come from the environment, so a quote or a control byte in one must
/// produce invalid input for the consumer rather than invalid JSON.
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
            control if control < ' ' || control == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The human-readable report, for `--format debug`.
#[expect(
    clippy::too_many_lines,
    reason = "one screen of output, written top to bottom; splitting it would \
              scatter the layout across functions without simplifying anything"
)]
fn print_debug(theme: &Theme, outcome: &ProbeOutcome) {
    println!();
    println!(
        "  {}",
        theme.paint("Text sizing protocol support, by key", Ink::Title)
    );
    println!();

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

    if !outcome.interactive {
        println!(
            "  {} cannot measure — stdin and stdout are not both terminals",
            theme.paint("?", Ink::Warn)
        );
        note(
            theme,
            "This protocol has no query, so support can only be established by printing something \
             and asking where the cursor ended up. That needs a real terminal on both ends. Run \
             this directly rather than through a pipe.",
        );
        println!();
        println!("  {}", theme.paint("exit status: 2 (unknown)", Ink::Muted));
        println!();
        return;
    }

    for report in &outcome.reports {
        let verdict = report.verdict();
        let ink = match verdict.support() {
            Support::Yes => Ink::Good,
            Support::No => Ink::Bad,
            Support::Unknown => Ink::Warn,
        };
        let measured = match report.measured {
            Some(cells) => format!("{cells} cells"),
            None => "no answer".to_owned(),
        };
        let (low, high) = report.key.range();
        println!(
            "  {} {}  {}  {}",
            theme.paint(verdict.symbol(), ink),
            theme.paint(
                &format!("{}= {:<20}", report.key.letter(), report.key.name()),
                Ink::Strong
            ),
            theme.paint(&format!("{:<10}", verdict.label()), ink),
            theme.paint(
                &format!(
                    "asked {} cells, used {measured}  ·  range {low}–{high}, default {}",
                    report.asked,
                    report.key.default_value()
                ),
                Ink::Muted
            ),
        );
        note(theme, &verdict.explanation(report.key));
        println!(
            "      {} {}",
            theme.paint("sent:", Ink::Muted),
            theme.paint(&pretty(&report.sequence), Ink::Code)
        );
    }

    println!();
    let support = outcome.support();
    let (symbol, label, ink) = match support {
        Support::Yes => ("✔", "supported", Ink::Good),
        Support::No => ("✘", "not supported", Ink::Bad),
        Support::Unknown => ("?", "unknown", Ink::Warn),
    };
    println!(
        "  {} {} {}",
        theme.paint(symbol, ink),
        theme.paint(label, ink),
        theme.paint(
            &format!("— exit status {}", exit_digit(support)),
            Ink::Muted
        )
    );
    note(
        theme,
        "The exit status answers about the protocol as a whole, so it is a yes only when every \
         measured key was honoured. A terminal implementing `w` but not `s` therefore exits 1 \
         while still reporting `w` as a yes above — read the per-key lines, or `--format json`, \
         when that difference matters.",
    );

    if outcome.text_would_be_lost() {
        println!();
        println!(
            "  {} {}",
            theme.paint("warning:", Ink::Warn),
            theme.paint(
                "text sent through OSC 66 is lost here, not merely unsized",
                Ink::Warn
            )
        );
        note(
            theme,
            "The cursor did not move at all, so this terminal parses the OSC string and discards \
             it, payload included. `escapade textsize` measures this before drawing and prints \
             plain text instead.",
        );
    }

    let unmeasurable: Vec<String> = Key::ALL
        .iter()
        .filter(|key| !Key::MEASURABLE.contains(key))
        .map(|key| format!("`{}`", key.letter()))
        .collect();
    println!();
    note(
        theme,
        &format!(
            "Not measured, and not measurable: {}. Fractional scaling deliberately does not change \
             how many cells text occupies, and alignment only moves the render area inside the \
             block, so a Cursor Position Report cannot see either. A terminal could honour \
             everything above and ignore all four.",
            unmeasurable.join(", ")
        ),
    );

    if !outcome.answered_barrier {
        println!();
        note(
            theme,
            "The device-attributes barrier went unanswered, which means replies are not reaching \
             this process at all. Any measurement above that says `unknown` says so for that \
             reason.",
        );
    } else if let Some(multiplexer) = outcome.multiplexer {
        println!();
        note(
            theme,
            &format!(
                "The measurements above describe {}, which is the thing laying out these cells — \
                 not the terminal beyond it. That is the answer an application needs, and it is \
                 why the sequences were not sent through passthrough: forwarding OSC 66 past a \
                 multiplexer leaves the two disagreeing about the same grid.",
                multiplexer.name()
            ),
        );
    }

    println!();
    println!(
        "  {} {}",
        theme.paint("escapade textsize explain", Ink::Code),
        theme.paint(
            "for what this protocol is and who implements it",
            Ink::Muted
        )
    );
    println!();
}

/// The exit status digit, for the report to state out loud.
fn exit_digit(support: Support) -> u8 {
    match support {
        Support::Yes => 0,
        Support::No => 1,
        Support::Unknown => 2,
    }
}

/// A wrapped, indented, muted note under a report line.
fn note(theme: &Theme, text: &str) {
    for line in wrap(text, theme.width().saturating_sub(6)) {
        println!("      {}", theme.paint(&line, Ink::Muted));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A report with a measurement in it, for the verdict tests.
    fn measured(key: Key, cells: usize) -> KeyReport {
        KeyReport {
            measured: Some(cells),
            ..KeyReport::unmeasured(key)
        }
    }

    fn outcome(reports: Vec<KeyReport>) -> ProbeOutcome {
        ProbeOutcome {
            reports,
            interactive: true,
            answered_barrier: true,
            multiplexer: None,
        }
    }

    // -- per-key verdicts ---------------------------------------------------

    #[test]
    fn two_cells_for_a_two_cell_request_is_a_yes() {
        let report = measured(Key::Width, 2);
        assert_eq!(report.verdict(), Verdict::Honoured);
        assert_eq!(report.support(), Support::Yes);
    }

    #[test]
    fn one_cell_means_the_text_appeared_and_the_key_did_not() {
        // A space took one cell: the escape code was parsed, `w` was not acted
        // on. Text sent this way still shows up.
        let report = measured(Key::Width, 1);
        assert_eq!(report.verdict(), Verdict::Ignored);
        assert_eq!(report.support(), Support::No);
        assert!(
            report
                .verdict()
                .explanation(Key::Width)
                .contains("still appears")
        );
    }

    #[test]
    fn no_movement_at_all_means_the_text_was_thrown_away() {
        // The classic no: an unknown OSC string is discarded, payload included.
        // Distinguishing this from `Ignored` is what lets the rendering path
        // know whether sending the sequence would lose the text.
        let report = measured(Key::Scale, 0);
        assert_eq!(report.verdict(), Verdict::Discarded);
        assert_eq!(report.support(), Support::No);
        assert!(outcome(vec![report]).text_would_be_lost());
        assert!(!outcome(vec![measured(Key::Scale, 1)]).text_would_be_lost());
    }

    #[test]
    fn a_third_number_is_reported_as_itself() {
        let report = measured(Key::Scale, 3);
        assert_eq!(report.verdict(), Verdict::Disagrees(3));
        assert_eq!(report.support(), Support::No);
        assert!(report.verdict().explanation(Key::Scale).contains('3'));
    }

    #[test]
    fn no_answer_is_unknown_and_not_a_no() {
        let report = KeyReport::unmeasured(Key::Width);
        assert_eq!(report.verdict(), Verdict::Unknown);
        assert_eq!(report.support(), Support::Unknown);
        assert!(!outcome(vec![report]).text_would_be_lost());
    }

    #[test]
    fn every_verdict_has_a_distinct_label() {
        let verdicts = [
            Verdict::Honoured,
            Verdict::Ignored,
            Verdict::Discarded,
            Verdict::Disagrees(4),
            Verdict::Unknown,
        ];
        let mut labels: Vec<&str> = verdicts.iter().map(|v| v.label()).collect();
        labels.sort_unstable();
        let total = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), total, "two verdicts read the same");
        for verdict in verdicts {
            assert!(!verdict.symbol().is_empty());
            assert!(!verdict.explanation(Key::Width).is_empty());
        }
    }

    // -- the single-valued answer -------------------------------------------

    #[test]
    fn the_protocol_is_supported_only_when_every_measured_key_was_honoured() {
        let both = outcome(vec![measured(Key::Width, 2), measured(Key::Scale, 2)]);
        assert_eq!(both.support(), Support::Yes);
        assert_eq!(both.support_for(Key::Width), Support::Yes);
        assert_eq!(both.support_for(Key::Scale), Support::Yes);
        assert!(both.conclusive());
    }

    #[test]
    fn a_width_only_terminal_is_a_no_overall_and_a_yes_for_width() {
        // foot's shape: `w` implemented, `s` not. Reporting this as plain
        // "unsupported" would be wrong about the half foot does, and reporting
        // it as "supported" would break the one thing the predicate is for.
        let footish = outcome(vec![measured(Key::Width, 2), measured(Key::Scale, 1)]);
        assert_eq!(footish.support_for(Key::Width), Support::Yes);
        assert_eq!(footish.support_for(Key::Scale), Support::No);
        assert_eq!(footish.support(), Support::No);
        assert!(!footish.text_would_be_lost(), "the text still appears");
    }

    #[test]
    fn one_missing_measurement_outranks_any_number_of_definite_noes() {
        // Half the protocol was never asked about, so the protocol has not been
        // shown to be absent — even though the half that answered said no.
        let half = outcome(vec![
            measured(Key::Width, 0),
            KeyReport::unmeasured(Key::Scale),
        ]);
        assert_eq!(half.support(), Support::Unknown);
        assert!(!half.conclusive());
        // The measured half is still reported for what it is.
        assert_eq!(half.support_for(Key::Width), Support::No);
        assert!(half.text_would_be_lost());
    }

    #[test]
    fn asking_about_one_key_answers_about_that_key() {
        // The per-key structure is what would let a caller ask a narrower
        // question: probing only `w` on a width-only terminal is a yes.
        let narrow = outcome(vec![measured(Key::Width, 2)]);
        assert_eq!(narrow.support(), Support::Yes);
        assert_eq!(narrow.support_for(Key::Scale), Support::Unknown);
        assert!(narrow.report(Key::Scale).is_none());
    }

    #[test]
    fn a_probe_that_asked_nothing_knows_nothing() {
        assert_eq!(ProbeOutcome::default().support(), Support::Unknown);
        assert!(!ProbeOutcome::default().conclusive());
        assert_eq!(
            ProbeOutcome::default().support_for(Key::Width),
            Support::Unknown
        );
    }

    #[test]
    fn a_non_interactive_run_is_unknown_rather_than_unsupported() {
        // The path a pipe takes. Every key comes back unmeasured, so the exit
        // status is 2 and nothing claims the terminal lacks the feature.
        let piped = detect(&Key::MEASURABLE).unwrap();
        if !piped.interactive {
            assert_eq!(piped.support(), Support::Unknown);
            assert_eq!(piped.reports.len(), 2);
            assert!(piped.reports.iter().all(|r| r.measured.is_none()));
        }
    }

    #[test]
    fn the_exit_statuses_are_zero_one_two() {
        assert_eq!(exit_digit(Support::Yes), 0);
        assert_eq!(exit_digit(Support::No), 1);
        assert_eq!(exit_digit(Support::Unknown), 2);
    }

    // -- the measurable set -------------------------------------------------

    #[test]
    fn only_width_and_scale_can_be_measured() {
        assert_eq!(Key::MEASURABLE, [Key::Width, Key::Scale]);
        for key in Key::MEASURABLE {
            assert!(key.detection().is_some());
            assert_eq!(key.detection_cells(), 2);
        }
        // Fractional scaling does not change cell counts by design, and
        // alignment moves things inside a block, so neither leaves a trace in a
        // Cursor Position Report.
        for key in [
            Key::Numerator,
            Key::Denominator,
            Key::VerticalAlign,
            Key::HorizontalAlign,
        ] {
            assert!(
                key.detection().is_none(),
                "{} is not measurable",
                key.letter()
            );
            assert!(!Key::MEASURABLE.contains(&key));
        }
    }

    #[test]
    fn a_scaled_block_is_two_rows_tall_and_a_wide_one_is_not() {
        // This is what the row reservation in `Measurer::measure` is for: a
        // two-row block on the last line of the screen must be discarded by the
        // terminal, which would look exactly like no support.
        assert_eq!(Key::Scale.detection_rows(), 2);
        assert_eq!(Key::Width.detection_rows(), 1);
    }

    #[test]
    fn the_detection_sequences_carry_no_loose_control_bytes() {
        for key in Key::MEASURABLE {
            let sequence = key.detection().unwrap();
            assert!(!pretty(&sequence).contains('\u{1b}'));
            assert_eq!(sequence.matches('\u{1b}').count(), 2);
        }
    }

    // -- JSON ---------------------------------------------------------------

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string("a\\b"), "\"a\\\\b\"");
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
        assert_eq!(json_string("a\u{1b}b"), "\"a\\u001bb\"");
        assert_eq!(json_string("a\u{7f}b"), "\"a\\u007fb\"");
        // A hostile TERM must not be able to close the object early.
        let hostile = json_string("xterm\",\"support\":\"yes");
        assert!(!hostile.contains("\",\"support"));
        assert!(hostile.starts_with('"') && hostile.ends_with('"'));
    }
}
