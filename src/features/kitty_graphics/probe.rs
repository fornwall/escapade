//! Runtime capability detection.
//!
//! The protocol's own answer to "do you support this?" is the *query action*: a
//! one-pixel image sent with `a=q`, which asks the terminal to validate the data,
//! answer, and store nothing. A terminal that implements the protocol must reply
//! immediately, before processing anything else.
//!
//! That "before processing anything else" is what makes detection reliable, and
//! it is why the query is paired with a **barrier** — a Primary Device Attributes
//! request (`CSI c`) sent immediately afterwards, which every terminal since the
//! VT100 answers. Terminals process input in order, so if the DA1 answer comes
//! back and the graphics answer did not, the terminal has demonstrably had its
//! chance and declined. Silence plus silence, on the other hand, means something
//! is eating replies — a multiplexer, or a pipe — and the right answer is "I do
//! not know" rather than "no".
//!
//! The spec spells the pair out exactly, and [`super::proto::support_query`]
//! sends those bytes verbatim:
//!
//! ```text
//! APC G i=31,s=1,v=1,a=q,t=d,f=24 ; AAAA ST   CSI c
//! ```

use std::{
    io::{IsTerminal as _, stdin, stdout},
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    cli::ProbeFormat,
    features::{
        Support,
        kitty_graphics::{
            layout::{self, CellSize},
            proto::{self, GraphicsReply, Outcome},
        },
    },
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// How long to wait for the terminal to answer. Generous enough for a slow ssh
/// link, short enough not to feel like a hang.
const REPLY_TIMEOUT: Duration = Duration::from_millis(400);

/// How long to wait for a medium query. Shorter than [`REPLY_TIMEOUT`], because by
/// this point the terminal has already proved it answers promptly.
const MEDIA_TIMEOUT: Duration = Duration::from_millis(200);

/// One black pixel as `f=24` RGB: the smallest legal image, and what every
/// capability query carries.
const PROBE_PIXEL: [u8; 3] = [0, 0, 0];

/// Image id for the `t=f` capability query.
const FILE_PROBE_ID: u32 = proto::PROBE_IMAGE_ID + 1;

/// Image id for the `o=z` capability query.
const COMPRESSION_PROBE_ID: u32 = proto::PROBE_IMAGE_ID + 2;

/// What a probe concluded.
///
/// The default is "nothing has been checked": unsupported, no barrier reply, not
/// interactive, with an assumed cell size.
#[derive(Debug, Clone, Default)]
pub struct ProbeOutcome {
    /// Whether the terminal answered the query action at all. An *error* reply
    /// still counts: only a terminal that implements the protocol knows how to
    /// say `EINVAL`.
    pub supported: bool,
    /// The parsed graphics reply, when there was one.
    pub reply: Option<GraphicsReply>,
    /// Whether the terminal answered the Primary Device Attributes barrier. If
    /// this is false and nothing else came back, the result is inconclusive
    /// rather than negative.
    pub answered_barrier: bool,
    /// Multiplexer in the way, if any.
    pub multiplexer: Option<Multiplexer>,
    /// Whether stdin and stdout are both terminals. Without that there is
    /// nothing to probe.
    pub interactive: bool,
    /// The cell size, which every placement calculation depends on.
    pub cell: CellSize,
    /// Which transmission media and payload options the terminal proved it takes.
    pub accepts: Accepts,
    /// Everything the terminal said, in order, for `--verbose`.
    pub replies: Vec<Reply>,
}

/// Optional parts of the protocol the terminal demonstrably accepts.
///
/// None of this can be inferred: a client cannot know whether it shares a
/// filesystem with its terminal — it may be at the far end of an ssh connection —
/// and there is no capability string to ask. So each one is established the way the
/// specification intends, by sending a one-pixel image with `a=q` and reading the
/// answer. `a=q` makes the terminal validate the data and then throw it away, so
/// asking costs nothing but a round trip.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Accepts {
    /// `t=f`: the terminal opened and read a file this process named. Which means
    /// an image already on disk can be sent as a *path* — no base64, no pty, no
    /// copy.
    pub local_files: bool,
    /// `o=z`: the terminal inflated a zlib payload. The one win available when the
    /// filesystem is not shared, since raw pixels compress very well.
    pub compression: bool,
}

impl Accepts {
    /// Whether anything beyond plain direct transmission is available.
    #[must_use]
    pub fn has_any(self) -> bool {
        self.local_files || self.compression
    }
}

/// What a probe is entitled to claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The terminal answered the query action.
    Supported,
    /// The terminal answered the barrier but not the query, having demonstrably
    /// had its chance.
    Unsupported,
    /// Nothing can honestly be claimed either way.
    Inconclusive,
}

impl ProbeOutcome {
    /// What this probe may legitimately conclude.
    ///
    /// The barrier only licenses a negative when it was answered by the terminal
    /// we are asking about. **A multiplexer answers Primary Device Attributes
    /// itself** — tmux replies from its own emulator — so inside one, a barrier
    /// reply says nothing whatsoever about the outer terminal, and treating it as
    /// proof of absence is a confident false claim. Turning passthrough on does not
    /// rescue it either: the barrier is then no longer ordered behind the query it
    /// is meant to fence.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        if self.supported {
            Verdict::Supported
        } else if self.answered_barrier && self.multiplexer.is_none() {
            Verdict::Unsupported
        } else {
            Verdict::Inconclusive
        }
    }

    /// Whether the probe reached a definite conclusion either way.
    #[must_use]
    pub fn conclusive(&self) -> bool {
        self.verdict() != Verdict::Inconclusive
    }

    /// Whether the terminal both implements the protocol and accepted the
    /// one-pixel test image.
    #[must_use]
    pub fn accepted_the_test_image(&self) -> bool {
        matches!(
            &self.reply,
            Some(GraphicsReply {
                outcome: Outcome::Ok,
                ..
            })
        )
    }
}

/// Runs the probe using an existing terminal and reader.
///
/// Takes both rather than creating them because there must only ever be one
/// [`InputReader`] per process — two would race for the same bytes on stdin, and
/// each would see a random half of the terminal's answer.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal that
/// simply stays quiet is not an error; it is a negative result.
pub fn probe(term: &mut Term, input: &mut InputReader) -> Result<ProbeOutcome> {
    let mut outcome = ProbeOutcome {
        multiplexer: Multiplexer::detect(),
        interactive: stdout().is_terminal() && stdin().is_terminal(),
        ..ProbeOutcome::default()
    };
    if !outcome.interactive {
        return Ok(outcome);
    }

    // Raw mode is required: in cooked mode the reply would sit in the line buffer
    // until the user pressed Enter, and would be echoed back at them.
    term.enable_raw()?;
    input.drain();

    send(term, outcome.multiplexer, &proto::support_query())?;
    // The barrier is deliberately *not* wrapped for the multiplexer: we want to
    // know whether the round trip completed, and tmux answers DA1 itself.
    term.write_now(&format!("{CSI}c"))?;

    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // An EOF on stdin means there is no one to answer; treat it as silence.
        let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
            break;
        };
        outcome.replies.push(reply.clone());

        match &reply {
            Reply::Apc(payload) => {
                if let Some(parsed) = proto::parse_reply(payload) {
                    outcome.supported = true;
                    outcome.reply = Some(parsed);
                    break;
                }
            }
            Reply::Csi { final_byte: 'c', .. } => {
                // The barrier came back. Anything sent before it has already been
                // processed, so continuing to wait would learn nothing.
                outcome.answered_barrier = true;
                break;
            }
            _ => {}
        }
    }

    // The cell size matters even when graphics are unsupported — it is what
    // `render` would use — so ask regardless.
    outcome.cell = layout::query_cell_size(term, input)?;
    if outcome.supported {
        outcome.accepts = probe_media(term, input, outcome.multiplexer, &mut outcome.replies);
    }
    Ok(outcome)
}

/// Asks which optional transmission media work here.
///
/// Each question is a one-pixel `f=24` image sent with `a=q` and its own image id,
/// so the answers can be told apart. A terminal that does not implement a medium
/// answers with an error code rather than staying silent — kitty answers `EBADF`
/// for an unreadable file, Konsole `ENOTSUPPORTED` for any medium but `t=d` — so
/// silence and failure both mean "no", and only `OK` means yes.
///
/// Failures here are never fatal: they downgrade to direct transmission, which
/// always works.
fn probe_media(
    term: &mut Term,
    input: &mut InputReader,
    multiplexer: Option<Multiplexer>,
    replies: &mut Vec<Reply>,
) -> Accepts {
    let mut accepts = Accepts::default();

    // `t=f`: hand the terminal a path and see whether it reads it.
    if let Some(path) = write_probe_file(&PROBE_PIXEL) {
        let mut command = proto::Transmit::pixels(proto::Format::Rgb, 1, 1)
            .with_action(proto::Action::Query)
            .with_id(FILE_PROBE_ID);
        command.medium = proto::Medium::File;
        let sequences = proto::transmit(&command, None, path.as_os_str().as_encoded_bytes());
        accepts.local_files = ask(term, input, multiplexer, &sequences, FILE_PROBE_ID, replies);
        // The terminal only deletes a `t=t` file; a `t=f` file is ours to clean up.
        drop(std::fs::remove_file(&path));
    }

    // `o=z`: hand it a compressed payload and see whether it inflates it.
    let mut command = proto::Transmit::pixels(proto::Format::Rgb, 1, 1)
        .with_action(proto::Action::Query)
        .with_id(COMPRESSION_PROBE_ID);
    command.compression = Some(proto::Compression::Zlib);
    let sequences = proto::transmit(&command, None, &compress(&PROBE_PIXEL));
    accepts.compression = ask(
        term,
        input,
        multiplexer,
        &sequences,
        COMPRESSION_PROBE_ID,
        replies,
    );

    accepts
}

/// Sends a query and waits for the reply that carries `image_id`.
///
/// Replies for other ids are kept in `replies` for `--verbose` and otherwise
/// ignored, which is what makes it safe to have more than one question in flight.
fn ask(
    term: &mut Term,
    input: &mut InputReader,
    multiplexer: Option<Multiplexer>,
    sequences: &[proto::Sequence],
    image_id: u32,
    replies: &mut Vec<Reply>,
) -> bool {
    for sequence in sequences {
        if send(term, multiplexer, &sequence.wire()).is_err() {
            return false;
        }
    }
    let deadline = Instant::now() + MEDIA_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let Ok(Some(reply)) = input.wait_for_reply(remaining, |reply| matches!(reply, Reply::Apc(_)))
        else {
            return false;
        };
        replies.push(reply.clone());
        if let Reply::Apc(payload) = &reply
            && let Some(parsed) = proto::parse_reply(payload)
            && parsed.image_id == Some(image_id)
        {
            return parsed.is_ok();
        }
    }
}

/// Writes the `t=f` probe payload somewhere the terminal can read it.
///
/// Returns `None` when the file cannot be written, which simply means the medium
/// goes untested and unused.
fn write_probe_file(payload: &[u8]) -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "escapade-{}-{}-probe",
        proto::TEMP_FILE_MARKER,
        std::process::id()
    ));
    std::fs::write(&path, payload).ok()?;
    Some(path)
}

/// Compresses a payload for `o=z`: a zlib (RFC 1950) deflate stream.
#[must_use]
pub fn compress(payload: &[u8]) -> Vec<u8> {
    // Level 6 is zlib's own default: the point here is to shrink a pty transfer,
    // not to win a compression contest, and the terminal has to inflate it.
    miniz_oxide::deflate::compress_to_vec_zlib(payload, 6)
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

/// Runs a probe from scratch, for `escapade graphics probe`.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn detect() -> Result<ProbeOutcome> {
    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let outcome = probe(&mut term, &mut input);
    // Restore before the caller prints anything, so the report appears in a
    // normal cooked-mode terminal.
    term.restore();
    outcome
}

/// The verdict this probe reports through its exit status.
///
/// The status answers one question — **can this terminal display an image at
/// all** — so it is a function of whether the protocol is implemented, not of the
/// transmission media. `t=f` and `o=z` are optimisations: a terminal that takes
/// neither still displays images perfectly well through `t=d`, so they are reported
/// as fields rather than folded into the verdict.
///
/// An *error* reply counts as support, for the same reason the probe treats it that
/// way: only a terminal that parsed the command knows how to answer `EINVAL`. What
/// it made of the one-pixel test image is reported separately as
/// `accepted_test_image`, because a refusal there is a transient or local problem —
/// a full image quota, say — and not an absence of the feature.
#[must_use]
pub fn support(outcome: &ProbeOutcome) -> Support {
    match outcome.verdict() {
        Verdict::Supported => Support::Yes,
        Verdict::Unsupported => Support::No,
        Verdict::Inconclusive => Support::Unknown,
    }
}

/// Runs `escapade graphics probe` and reports through the exit status.
///
/// Silent unless a `--format` is given, so it composes as a shell predicate.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn report(theme: &Theme, format: Option<ProbeFormat>) -> Result<ExitCode> {
    let outcome = detect()?;
    let support = support(&outcome);
    match format {
        None => {}
        Some(ProbeFormat::Json) => print!("{}", json_line(&outcome, support)),
        Some(ProbeFormat::Debug) => debug_report(theme, &outcome),
    }
    Ok(support.exit_code())
}

/// One JSON object, one line — the house machine-readable shape.
fn json_line(outcome: &ProbeOutcome, support: Support) -> String {
    let multiplexer = outcome
        .multiplexer
        .map_or_else(|| "null".to_owned(), |m| format!("\"{}\"", m.name()));
    let error = outcome.reply.as_ref().and_then(GraphicsReply::error).map_or_else(
        || "null".to_owned(),
        |code| format!("\"{}\"", escape_json(code.as_str())),
    );
    format!(
        "{{\"feature\":\"{}\",\"verdict\":\"{}\",\"accepted_test_image\":{},\
         \"error\":{error},\"local_files\":{},\"compression\":{},\
         \"cell_width\":{},\"cell_height\":{},\"cell_size_source\":\"{}\",\
         \"answered_barrier\":{},\"multiplexer\":{multiplexer},\"interactive\":{}}}\n",
        super::FEATURE_ID,
        support.as_str(),
        outcome.accepted_the_test_image(),
        outcome.accepts.local_files,
        outcome.accepts.compression,
        outcome.cell.width,
        outcome.cell.height,
        outcome.cell.source.as_str(),
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
#[expect(
    clippy::too_many_lines,
    reason = "one screen of output, written top to bottom; splitting it would scatter \
              the layout across functions without simplifying anything"
)]
fn debug_report(theme: &Theme, outcome: &ProbeOutcome) {
    let verbose = true;


    println!();
    println!("  {}", theme.paint("kitty graphics protocol support", Ink::Title));
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
            "  {} cannot probe — stdin and stdout are not both terminals",
            theme.paint("?", Ink::Warn)
        );
        println!(
            "    {}",
            theme.paint(
                "Capability detection needs a real terminal to answer. Run this directly, not through a pipe.",
                Ink::Muted
            )
        );
        println!();
        return;
    }

    if outcome.supported {
        println!(
            "  {} {}",
            theme.paint("✔", Ink::Good),
            theme.paint("supported", Ink::Good)
        );
        if outcome.accepted_the_test_image() {
            println!(
                "    {}",
                theme.paint(
                    "the terminal loaded the one-pixel test image and said OK",
                    Ink::Muted
                )
            );
        }
        match &outcome.reply {
            Some(GraphicsReply {
                outcome: Outcome::Ok,
                ..
            })
            | None => {}
            Some(GraphicsReply {
                outcome: Outcome::Error { code, detail },
                ..
            }) => {
                println!(
                    "    {} {}",
                    theme.paint("but the test image was rejected:", Ink::Warn),
                    theme.paint(code.as_str(), Ink::Code)
                );
                println!("    {}", theme.paint(code.explain(), Ink::Muted));
                if let Some(detail) = detail {
                    println!("    {}", theme.paint(detail, Ink::Muted));
                }
                println!(
                    "    {}",
                    theme.paint(
                        "Only a terminal that implements the protocol knows how to send that code, so support is not in doubt.",
                        Ink::Muted
                    )
                );
            }
        }

        // What else it takes, which decides whether `render` can skip the pty.
        println!();
        println!("  {}", theme.paint("transmission media", Ink::Accent));
        for (available, key, what) in [
            (
                outcome.accepts.local_files,
                "t=f",
                "read an image from a file path, with no base64 at all",
            ),
            (
                outcome.accepts.compression,
                "o=z",
                "inflate a zlib-compressed payload",
            ),
        ] {
            let (mark, ink) = if available {
                ("✔", Ink::Good)
            } else {
                ("✘", Ink::Muted)
            };
            println!(
                "    {} {}  {}",
                theme.paint(mark, ink),
                theme.paint(key, Ink::Code),
                theme.paint(what, Ink::Muted)
            );
        }
        if !outcome.accepts.has_any() {
            println!(
                "    {}",
                theme.paint(
                    "Everything goes inline as base64 (t=d), which always works and is the \
                     only option over ssh.",
                    Ink::Muted
                )
            );
        }
    } else if outcome.conclusive() {
        println!(
            "  {} {}",
            theme.paint("✘", Ink::Bad),
            theme.paint("not supported", Ink::Bad)
        );
        println!(
            "    {}",
            theme.paint(
                "The terminal answered the device-attributes barrier but ignored the query action, so this is a real no rather than a timeout.",
                Ink::Muted
            )
        );
    } else {
        debug_assert!(!outcome.conclusive(), "a conclusion was reached above");
        println!(
            "  {} {}",
            theme.paint("?", Ink::Warn),
            theme.paint("inconclusive", Ink::Warn)
        );
        println!(
            "    {}",
            theme.paint(
                "Nothing came back at all — not even the device-attributes barrier every terminal answers. Something between here and the terminal is swallowing replies.",
                Ink::Muted
            )
        );
    }

    println!();
    let cell_ink = if outcome.cell.source.is_measured() {
        Ink::Good
    } else {
        Ink::Warn
    };
    println!(
        "  {} {}",
        theme.paint("cell size:", Ink::Muted),
        theme.paint(
            &format!("{}x{} pixels", outcome.cell.width, outcome.cell.height),
            cell_ink
        )
    );
    println!("    {}", theme.paint(outcome.cell.source.describe(), Ink::Muted));
    if !outcome.cell.source.is_measured() {
        println!(
            "    {}",
            theme.paint(
                "Without it, `render` has to guess how many cells an image needs, and the aspect ratio may come out wrong.",
                Ink::Muted
            )
        );
    }

    if let Some(multiplexer) = outcome.multiplexer
        && !outcome.supported
    {
        println!();
        println!(
            "  {} {} filters unknown escape sequences by default.",
            theme.paint("note:", Ink::Warn),
            multiplexer.name()
        );
        println!("    {}", theme.paint(multiplexer.passthrough_hint(), Ink::Code));
    }

    if verbose {
        println!();
        println!("  {}", theme.paint("what was sent", Ink::Accent));
        let query = match outcome.multiplexer {
            Some(multiplexer) => multiplexer.wrap(&proto::support_query()),
            None => proto::support_query(),
        };
        println!("    {}", theme.paint(&pretty(&query), Ink::Code));
        println!(
            "    {}  {}",
            theme.paint(&pretty(&format!("{CSI}c")), Ink::Code),
            theme.paint("← the barrier", Ink::Muted)
        );
        println!(
            "    {}  {}",
            theme.paint(&pretty(&format!("{CSI}16t{CSI}14t")), Ink::Code),
            theme.paint("← cell size, then window size", Ink::Muted)
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

    println!();
    println!(
        "  {} {}",
        theme.paint("escapade graphics explain", Ink::Code),
        theme.paint("for what this protocol is and who implements it", Ink::Muted)
    );
    println!(
        "  {} {}",
        theme.paint("escapade graphics demo", Ink::Code),
        theme.paint("to watch it work", Ink::Muted)
    );
    println!();}

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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::kitty_graphics::{layout::CellSource, proto::ErrorCode};

    #[test]
    fn a_silent_terminal_that_answered_the_barrier_is_a_definite_no() {
        let outcome = ProbeOutcome {
            answered_barrier: true,
            interactive: true,
            ..ProbeOutcome::default()
        };
        assert!(outcome.conclusive());
        assert!(!outcome.supported);
        assert!(!outcome.accepted_the_test_image());
    }

    #[test]
    fn total_silence_is_inconclusive_not_negative() {
        let outcome = ProbeOutcome {
            interactive: true,
            ..ProbeOutcome::default()
        };
        assert!(
            !outcome.conclusive(),
            "no barrier reply means we learned nothing"
        );
    }

    #[test]
    fn an_ok_reply_is_support_and_acceptance() {
        let outcome = ProbeOutcome {
            supported: true,
            reply: proto::parse_reply("Gi=31;OK"),
            interactive: true,
            ..ProbeOutcome::default()
        };
        assert!(outcome.conclusive());
        assert!(outcome.accepted_the_test_image());
    }

    #[test]
    fn an_error_reply_still_proves_the_protocol_is_implemented() {
        // Only a terminal that parsed the command can answer EINVAL.
        let outcome = ProbeOutcome {
            supported: true,
            reply: proto::parse_reply("Gi=31;EINVAL:bad geometry"),
            interactive: true,
            ..ProbeOutcome::default()
        };
        assert!(outcome.supported);
        assert!(outcome.conclusive());
        assert!(
            !outcome.accepted_the_test_image(),
            "supported, but it did not like the image"
        );
        assert_eq!(
            outcome.reply.unwrap().error(),
            Some(&ErrorCode::Invalid),
            "and the code is kept for the report"
        );
    }

    #[test]
    fn a_probe_defaults_to_an_assumed_cell_size() {
        let outcome = ProbeOutcome::default();
        assert_eq!(outcome.cell.source, CellSource::Assumed);
        assert!(!outcome.interactive, "nothing has been checked yet");
    }

    #[test]
    fn replies_render_without_control_bytes() {
        let replies = [
            Reply::Apc("Gi=31;OK".to_owned()),
            Reply::Csi {
                params: "?62;4".to_owned(),
                final_byte: 'c',
            },
            Reply::Osc("11;rgb:00/00/00".to_owned()),
            Reply::Dcs("1$r".to_owned()),
        ];
        for reply in &replies {
            let rendered = describe_reply(reply);
            assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        }
    }
}
