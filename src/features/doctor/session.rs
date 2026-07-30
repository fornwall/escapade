//! Talking to the terminal: which channel, for how long, and how a round ends.
//!
//! # Which channel
//!
//! Requiring stdin *and* stdout to be terminals is the bug `--probe-mode` exists
//! to fix. A program in a pipeline still has a terminal, and it is reachable as
//! `/dev/tty` regardless of what happened to the standard streams — so
//! `--probe-mode=ctty` opens that, `stdio` uses the standard streams, and the
//! default `any` tries the controlling terminal first and falls back.
//!
//! Replies are read through [`InputReader::from_reader`], which is what makes the
//! `/dev/tty` path possible at all.
//!
//! # How a round ends
//!
//! Every batch of queries is followed by a Primary Device Attributes request
//! (`CSI c`), which every terminal since the VT100 answers. Terminals process
//! input in order, so a DA1 answer arriving with nothing in front of it proves the
//! terminal had its chance and declined — which turns a timeout into a conclusion.
//!
//! **A multiplexer invalidates that.** tmux answers DA1 from its own emulator
//! rather than forwarding it, so inside one a barrier reply says nothing about the
//! terminal beyond it. Results under a multiplexer are therefore inconclusive
//! rather than negative; see [`crate::features::glyph_protocol::probe::ProbeOutcome::verdict`]
//! for the same reasoning at length. Nothing here is wrapped for passthrough,
//! because a wrapped query is no longer ordered behind the barrier that is
//! supposed to fence it.
//!
//! # Draining
//!
//! A reply nobody read is delivered to whatever runs next — the user's shell,
//! which prints it as garbage. Because the barrier is the last thing written in a
//! round, reading until it arrives has consumed everything the round provoked. If
//! it never arrives, the reader is drained anyway.

use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, IsTerminal as _, Write as _, stdin, stdout},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};

use crate::{
    cli::ProbeMode,
    input::{InputReader, Reply},
    term::{CSI, Multiplexer},
};

/// The path to the controlling terminal.
const CTTY: &str = "/dev/tty";

/// How long to keep listening after a conclusion, to consume a barrier reply
/// still in flight.
///
/// Short, because by this point the terminal has demonstrably answered something
/// and the reply is either already buffered or arriving immediately.
const DRAIN: Duration = Duration::from_millis(150);

/// Where a round's queries were written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// The controlling terminal, `/dev/tty`.
    Ctty,
    /// Standard input and standard output.
    Stdio,
}

impl Channel {
    /// Short label for the report.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ctty => "/dev/tty",
            Self::Stdio => "stdio",
        }
    }
}

/// Why no channel could be opened.
///
/// Every variant is a sentence a reader can act on, because "could not probe" on
/// its own is the least useful thing a diagnostic can say.
#[derive(Debug, Clone)]
pub struct NoChannel {
    /// What was tried and what went wrong.
    pub reason: String,
    /// What the reader could do differently.
    pub hint: Option<String>,
}

/// What one batch of queries got back.
///
/// This is the boundary between the terminal and everything testable: each probe
/// turns a `Round` into a verdict with no I/O, so every interpretation in
/// [`super::observe`] has a unit test.
#[derive(Debug, Clone, Default)]
pub struct Round {
    /// Everything the terminal said, in arrival order.
    pub replies: Vec<Reply>,
    /// Whether the Primary Device Attributes barrier came back.
    pub answered_barrier: bool,
}

impl Round {
    /// Whether the terminal said anything at all.
    #[must_use]
    pub fn heard_anything(&self) -> bool {
        self.answered_barrier || !self.replies.is_empty()
    }
}

/// An open conversation with the terminal.
#[derive(Debug)]
pub struct Session {
    writer: Writer,
    input: InputReader,
    /// How long to wait for each round's replies, from `--timeout-ms`.
    timeout: Duration,
    /// Which channel was opened.
    channel: Channel,
    /// A multiplexer between this process and the real terminal, if any.
    multiplexer: Option<Multiplexer>,
    /// Whether raw mode was entered, so it is only left once.
    raw: bool,
    /// Set once a whole round comes back empty.
    ///
    /// A terminal that ignored the barrier will ignore every later one, and each
    /// round then costs a full timeout. Six checks at a second each is six seconds
    /// of waiting to learn what the first round already established.
    silent: bool,
    /// Whether anything was ever heard, which is what licenses any conclusion.
    heard_anything: bool,
}

/// Where queries are written. Two variants rather than a boxed trait object, so
/// the type stays [`fmt::Debug`] without a hand-written implementation.
#[derive(Debug)]
enum Writer {
    Stdout(io::Stdout),
    Tty(File),
}

impl Writer {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Stdout(out) => out.write_all(bytes),
            Self::Tty(file) => file.write_all(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stdout(out) => out.flush(),
            Self::Tty(file) => file.flush(),
        }
    }
}

impl Session {
    /// Opens a conversation on the channel `mode` asks for.
    ///
    /// `Ok(Err(reason))` means there was no terminal to talk to, which is a
    /// diagnosis rather than a failure — the report still has terminfo and the
    /// environment to show.
    ///
    /// # Errors
    ///
    /// Fails only if a reader thread cannot be started, which is the one problem
    /// here that is not a finding.
    pub fn open(mode: ProbeMode, timeout: Duration) -> Result<Result<Self, NoChannel>> {
        let opened = match mode {
            ProbeMode::Ctty => open_ctty(),
            ProbeMode::Stdio => open_stdio(),
            // The controlling terminal first: it is right whenever output is
            // redirected, which is exactly when stdio is wrong.
            ProbeMode::Any => match open_ctty() {
                Ok(channel) => Ok(channel),
                Err(ctty) => match open_stdio() {
                    Ok(channel) => Ok(channel),
                    Err(stdio) => Err(NoChannel {
                        reason: format!("{} And {}", ctty.reason, lower_first(&stdio.reason)),
                        hint: stdio.hint.or(ctty.hint),
                    }),
                },
            },
        };

        let (writer, reader, channel) = match opened {
            Ok(opened) => opened,
            Err(reason) => return Ok(Err(reason)),
        };

        let input = match reader {
            Reader::Stdin => InputReader::new().context("starting the stdin reader")?,
            Reader::Tty(file) => InputReader::from_reader(file, "escapade-doctor-tty")
                .context("starting the /dev/tty reader")?,
        };

        Ok(Ok(Self {
            writer,
            input,
            timeout,
            channel,
            multiplexer: Multiplexer::detect(),
            raw: false,
            silent: false,
            heard_anything: false,
        }))
    }

    /// Which channel was opened.
    #[must_use]
    pub fn channel(&self) -> Channel {
        self.channel
    }

    /// A multiplexer between this process and the real terminal, if any.
    #[must_use]
    pub fn multiplexer(&self) -> Option<Multiplexer> {
        self.multiplexer
    }

    /// Whether the terminal has ever answered anything.
    ///
    /// This is what licenses any conclusion at all, and what the exit status turns
    /// on: a channel that opened but was never answered is still nothing to ask.
    #[must_use]
    pub fn heard_anything(&self) -> bool {
        self.heard_anything
    }

    /// Enters raw mode, without which a reply would sit in the line buffer until
    /// the user pressed Enter and would be echoed back at them.
    ///
    /// crossterm reconfigures the controlling terminal rather than whatever `stdin`
    /// happens to be, which is what makes this work with output redirected.
    ///
    /// # Errors
    ///
    /// Fails if the terminal cannot be reconfigured.
    pub fn enable_raw(&mut self) -> Result<()> {
        if !self.raw {
            crossterm::terminal::enable_raw_mode().context("entering raw mode")?;
            self.raw = true;
            self.input.drain();
        }
        Ok(())
    }

    /// Sends `queries`, then the barrier, and collects everything that comes back.
    ///
    /// Returns an empty round once the terminal has gone silent, rather than
    /// spending another timeout confirming it.
    ///
    /// # Errors
    ///
    /// Fails if the channel cannot be written to. A terminal that stays quiet is a
    /// result, not an error.
    pub fn round(&mut self, queries: &str) -> Result<Round> {
        if self.silent {
            return Ok(Round::default());
        }
        self.enable_raw()?;

        self.writer
            .write_all(queries.as_bytes())
            .context("writing queries to the terminal")?;
        // The barrier is deliberately unwrapped even under a multiplexer, so it
        // reports on the round trip that can actually be reasoned about.
        self.writer
            .write_all(format!("{CSI}c").as_bytes())
            .context("writing the device-attributes barrier")?;
        self.writer.flush().context("flushing the terminal")?;

        let mut round = Round::default();
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            // An end of file means there is nobody to answer; treat it as silence
            // rather than as an error.
            let Ok(Some(reply)) = self.input.wait_for_reply(remaining, |_| true) else {
                break;
            };
            let is_barrier = matches!(
                &reply,
                Reply::Csi { params, final_byte: 'c' } if !params.starts_with('>')
            );
            round.replies.push(reply);
            if is_barrier {
                // Everything sent before the barrier has been processed, so
                // waiting longer would learn nothing — and everything this round
                // provoked has now been consumed.
                round.answered_barrier = true;
                break;
            }
        }

        if !round.answered_barrier {
            // Whatever is still in flight would otherwise land in the user's
            // shell and be printed as stray text.
            self.drain_barrier(&mut round);
        }

        if round.heard_anything() {
            self.heard_anything = true;
        } else {
            self.silent = true;
        }
        Ok(round)
    }

    /// Consumes a barrier reply still in flight, and anything queued behind it.
    fn drain_barrier(&mut self, round: &mut Round) {
        let found = self.input.wait_for_reply(DRAIN, |reply| {
            matches!(reply, Reply::Csi { params, final_byte: 'c' } if !params.starts_with('>'))
        });
        if let Ok(Some(reply)) = found {
            round.replies.push(reply);
            round.answered_barrier = true;
        }
        self.input.drain();
    }

    /// Leaves raw mode and drains anything still pending.
    ///
    /// [`Drop`] does the same, so a round returning an error still restores the
    /// terminal; this is the explicit form for the ordinary path, where the report
    /// is printed afterwards and wants a cooked terminal to print into.
    pub fn close(mut self) {
        self.restore();
    }

    /// Best-effort restore, safe to call more than once.
    fn restore(&mut self) {
        self.input.drain();
        if self.raw {
            let _ = crossterm::terminal::disable_raw_mode();
            self.raw = false;
        }
        let _ = self.writer.flush();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Which byte source a reply thread should read.
enum Reader {
    Stdin,
    Tty(File),
}

impl fmt::Debug for Reader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdin => formatter.write_str("Stdin"),
            Self::Tty(_) => formatter.write_str("Tty(/dev/tty)"),
        }
    }
}

/// Opens the controlling terminal for reading and writing.
///
/// Two file descriptors are opened on the same terminal: the reader thread takes
/// ownership of one and blocks in a read on it, so the writer needs its own.
fn open_ctty() -> Result<(Writer, Reader, Channel), NoChannel> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    let write_end = options.open(CTTY).map_err(|error| NoChannel {
        reason: format!("The controlling terminal {CTTY} could not be opened: {error}."),
        hint: None,
    })?;
    let read_end = options.open(CTTY).map_err(|error| NoChannel {
        reason: format!("The controlling terminal {CTTY} could not be opened twice: {error}."),
        hint: None,
    })?;
    if !write_end.is_terminal() {
        return Err(NoChannel {
            reason: format!("{CTTY} opened but is not a terminal."),
            hint: None,
        });
    }
    Ok((Writer::Tty(write_end), Reader::Tty(read_end), Channel::Ctty))
}

/// Uses the standard streams.
///
/// Both have to be terminals here, and unlike the `any` default that is not too
/// strict: it is what the reader explicitly asked for. Queries go to stdout and
/// replies come back on stdin, so either one being a pipe makes the round trip
/// impossible rather than merely inaccurate.
fn open_stdio() -> Result<(Writer, Reader, Channel), NoChannel> {
    let out = stdout().is_terminal();
    let in_ = stdin().is_terminal();
    if out && in_ {
        return Ok((Writer::Stdout(stdout()), Reader::Stdin, Channel::Stdio));
    }
    let which = match (out, in_) {
        (false, false) => "neither standard output nor standard input is a terminal",
        (false, true) => "standard output is not a terminal",
        (true, false) => "standard input is not a terminal",
        (true, true) => unreachable!(),
    };
    Err(NoChannel {
        reason: format!("On the stdio channel, {which}."),
        hint: Some(
            "Queries go to standard output and replies come back on standard input, so a redirect \
             on either breaks the round trip. Try --probe-mode=ctty, which talks to /dev/tty \
             regardless of what the standard streams are connected to."
                .to_owned(),
        ),
    })
}

/// Lowercases the first character, for joining two sentences with "And".
fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_round_that_heard_nothing_knows_it() {
        assert!(!Round::default().heard_anything());
        assert!(
            Round {
                replies: Vec::new(),
                answered_barrier: true
            }
            .heard_anything()
        );
        assert!(
            Round {
                replies: vec![Reply::Dcs("1+r5463=31".to_owned())],
                answered_barrier: false
            }
            .heard_anything(),
            "an answer without a barrier is still an answer"
        );
    }

    #[test]
    fn each_channel_has_a_distinct_label() {
        assert_eq!(Channel::Ctty.label(), "/dev/tty");
        assert_eq!(Channel::Stdio.label(), "stdio");
        assert_ne!(Channel::Ctty.label(), Channel::Stdio.label());
    }

    #[test]
    fn the_stdio_channel_explains_which_stream_is_wrong() {
        // Under `cargo test` neither stream is a terminal, so this is the branch
        // that runs — and the message has to name a fix rather than just fail.
        let failure = open_stdio().expect_err("no terminal under cargo test");
        assert!(failure.reason.contains("terminal"), "{}", failure.reason);
        let hint = failure.hint.unwrap();
        assert!(hint.contains("--probe-mode=ctty"), "{hint}");
    }

    #[test]
    fn joining_two_failures_reads_as_one_sentence() {
        assert_eq!(lower_first("On the stdio channel, x."), "on the stdio channel, x.");
        assert_eq!(lower_first(""), "");
        assert_eq!(lower_first("a"), "a");
    }

    #[test]
    fn a_secondary_device_attributes_reply_is_not_the_barrier() {
        // Both end in `c`. Treating DA2 as the barrier would end a round before
        // the real barrier arrived, and the identity probe sends DA2 every time.
        let da2 = Reply::Csi {
            params: ">41;356;0".to_owned(),
            final_byte: 'c',
        };
        let da1 = Reply::Csi {
            params: "?62;4".to_owned(),
            final_byte: 'c',
        };
        let is_barrier = |reply: &Reply| {
            matches!(reply, Reply::Csi { params, final_byte: 'c' } if !params.starts_with('>'))
        };
        assert!(!is_barrier(&da2));
        assert!(is_barrier(&da1));
        // tmux answers DA1 without the `?` private prefix.
        assert!(is_barrier(&Reply::Csi {
            params: "?1;2".to_owned(),
            final_byte: 'c'
        }));
    }
}
