//! Getting the *bytes* back out of the input stream, next to the events.
//!
//! [`crate::input::InputReader`] hands out decoded events and keeps the bytes to
//! itself. That is the right design for an input reader — it buffers partial
//! sequences, re-buffers events that arrived while it was waiting for a query
//! reply — and it is exactly wrong for a feature whose whole point is showing what
//! the terminal sent.
//!
//! So the byte stream is **mirrored** on the way in ([`Tee`]) and each event is
//! attributed to the bytes at the front of the mirror ([`Attribution`]) by
//! re-running the same pure parser the reader used. [`Keyboard`] bundles the two
//! into the one thing a caller wants: an event, and the bytes that produced it.

use std::{
    io::{self, Read},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;

use crate::input::{Event, InputReader, KeyCode, KeyEvent, ParseOutcome, parse_event};

/// `ESC`, whose presence at the front of a stalled buffer means the Escape key.
const ESC: u8 = 0x1b;

/// A [`Read`] that copies everything it hands out into a shared buffer.
///
/// The copy is made *before* the bytes are returned to the caller, so the mirror
/// is never behind what the reader has seen — which is what makes attribution
/// possible at all.
#[derive(Debug)]
pub struct Tee<R> {
    inner: R,
    mirror: Arc<Mutex<Vec<u8>>>,
}

impl<R: Read> Read for Tee<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buf)?;
        // A poisoned mirror would mean another thread panicked while holding it.
        // Losing the byte display is a better outcome than panicking here.
        if let Ok(mut mirror) = self.mirror.lock() {
            mirror.extend_from_slice(&buf[..count]);
        }
        Ok(count)
    }
}

/// Pairs each event the reader emits with the bytes that produced it.
///
/// The reader and this type see the same byte stream and parse it with the same
/// pure function, so they agree on how many bytes each event took and the front of
/// the mirror can be split off event by event.
///
/// The one case where they can disagree is a *stalled* sequence: after
/// [`crate::input::ESC_TIMEOUT`] the reader resolves a lone `ESC` into the Escape
/// key and any longer unfinished sequence into [`Event::Unknown`], while
/// [`parse_event`] on the same bytes still reports
/// [`ParseOutcome::Incomplete`]. Both of those are recognised in
/// [`Attribution::bytes_for`], so a timed-out Escape is still attributed to
/// exactly one byte.
#[derive(Debug)]
pub struct Attribution {
    mirror: Arc<Mutex<Vec<u8>>>,
    pending: Vec<u8>,
}

impl Attribution {
    /// Wraps `source` in a mirror and starts a reader over it.
    ///
    /// # Errors
    ///
    /// Fails if the operating system refuses to create the reader thread.
    pub fn wrap<R: Read + Send + 'static>(source: R) -> Result<(InputReader, Self)> {
        let mirror = Arc::new(Mutex::new(Vec::new()));
        let tee = Tee {
            inner: source,
            mirror: Arc::clone(&mirror),
        };
        let reader = InputReader::from_reader(tee, "escapade-keys")?;
        Ok((
            reader,
            Self {
                mirror,
                pending: Vec::new(),
            },
        ))
    }

    /// The bytes that produced `event`, which the reader has just returned.
    ///
    /// Call once per event, in the order the reader produced them.
    #[must_use]
    pub fn bytes_for(&mut self, event: &Event) -> Vec<u8> {
        if let Ok(mut mirror) = self.mirror.lock() {
            self.pending.append(&mut mirror);
        }
        let consumed = match parse_event(&self.pending) {
            ParseOutcome::Event { consumed, .. } => consumed,
            // The reader resolved a stalled sequence. Both of its two outcomes
            // say precisely how many bytes went into it.
            ParseOutcome::Incomplete => match event {
                Event::Unknown(bytes) if self.pending.starts_with(bytes) => bytes.len(),
                Event::Key(KeyEvent {
                    code: KeyCode::Esc, ..
                }) if self.pending.first() == Some(&ESC) => 1,
                _ => self.pending.len(),
            },
        };
        self.pending.drain(..consumed).collect()
    }

    /// Forgets every byte seen so far.
    ///
    /// **Required after any event is taken from the reader without going through
    /// [`Attribution::bytes_for`]** — a capability probe using
    /// [`InputReader::wait_for_reply`], or an [`InputReader::drain`]. Without it
    /// the mirror still holds those bytes and every later event would be
    /// attributed to the wrong ones.
    pub fn resync(&mut self) {
        if let Ok(mut mirror) = self.mirror.lock() {
            mirror.clear();
        }
        self.pending.clear();
    }
}

/// A source of input events that can also say which bytes produced them.
///
/// The seam exists so that an interactive demo step ([`super::prompt`]) can be
/// written once and used both by a feature that cares about the raw encoding and
/// by one that only wants the keypress.
pub trait EventSource {
    /// Waits up to `timeout` for one event and the bytes behind it.
    ///
    /// `Ok(None)` means the timeout expired with nothing to report; an error means
    /// input has ended.
    ///
    /// # Errors
    ///
    /// Returns the reader's I/O error, or [`io::ErrorKind::UnexpectedEof`] once
    /// input is exhausted.
    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<(Event, Vec<u8>)>>;

    /// Discards pending input, so the next step starts from silence.
    fn discard_pending(&mut self);
}

/// An [`InputReader`] with no byte attribution at all.
///
/// Provided so a feature that has an ordinary reader can still use the shared
/// interactive step; the byte column simply comes out empty. Any feature that
/// wants the bytes should use [`Keyboard`] instead.
impl EventSource for InputReader {
    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<(Event, Vec<u8>)>> {
        Ok(self.read(timeout)?.map(|event| (event, Vec::new())))
    }

    fn discard_pending(&mut self) {
        self.drain();
    }
}

/// An input reader that keeps the bytes.
///
/// This is what `escapade keys` uses everywhere: the reader for its timing rules —
/// escape disambiguation, replies picked out of the middle of user input — and the
/// mirror for the encoding.
#[derive(Debug)]
pub struct Keyboard {
    reader: InputReader,
    attribution: Attribution,
}

impl Keyboard {
    /// Reads from standard input.
    ///
    /// # Errors
    ///
    /// Fails if the operating system refuses to create the reader thread.
    pub fn new() -> Result<Self> {
        Self::from_reader(io::stdin())
    }

    /// Reads from any byte source, which is what makes this testable.
    ///
    /// # Errors
    ///
    /// Fails if the operating system refuses to create the reader thread.
    pub fn from_reader<R: Read + Send + 'static>(source: R) -> Result<Self> {
        let (reader, attribution) = Attribution::wrap(source)?;
        Ok(Self {
            reader,
            attribution,
        })
    }

    /// The underlying reader, for a capability probe or an advance prompt.
    ///
    /// Anything taken from it this way bypasses attribution, so
    /// [`Keyboard::discard_pending`] must be called before the next attributed
    /// read. Both `escapade keys` callers do exactly that.
    pub fn reader(&mut self) -> &mut InputReader {
        &mut self.reader
    }
}

impl EventSource for Keyboard {
    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<(Event, Vec<u8>)>> {
        let Some(event) = self.reader.read(timeout)? else {
            return Ok(None);
        };
        let bytes = self.attribution.bytes_for(&event);
        Ok(Some((event, bytes)))
    }

    fn discard_pending(&mut self) {
        self.reader.drain();
        self.attribution.resync();
    }
}

/// Reads every event from a recorded byte stream, with its bytes, exactly as the
/// real loop does — no terminal involved.
///
/// Shared by the tests of every module that formats events.
#[cfg(test)]
pub fn replay(bytes: &'static [u8]) -> Vec<(Vec<u8>, Event)> {
    let mut keyboard = match Keyboard::from_reader(io::Cursor::new(bytes)) {
        Ok(keyboard) => keyboard,
        Err(error) => panic!("could not start a reader: {error}"),
    };
    let mut out = Vec::new();
    // A generous timeout that is never actually waited out: the source is a
    // cursor, so it reaches end of input immediately and the reader reports it.
    while let Ok(Some((event, bytes))) = keyboard.next_event(Duration::from_secs(5)) {
        out.push((bytes, event));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::keys::event::{describe_event, symbolic};

    #[test]
    fn every_event_is_attributed_to_the_bytes_that_produced_it() {
        // A whole session in one stream: text, a kitty key, a release, a legacy
        // arrow, and Ctrl-D.
        let events = replay(b"a\x1b[13;2u\x1b[97;1:3u\x1b[A\x04");
        let rendered: Vec<(String, String)> = events
            .iter()
            .map(|(bytes, event)| (symbolic(bytes), describe_event(event)))
            .collect();
        assert_eq!(
            rendered,
            vec![
                ("a".to_owned(), "a  press".to_owned()),
                ("CSI 13;2u".to_owned(), "Shift+Enter  press".to_owned()),
                ("CSI 97;1:3u".to_owned(), "a  release".to_owned()),
                ("CSI A".to_owned(), "Up  press".to_owned()),
                ("^D".to_owned(), "Ctrl+d  press".to_owned()),
            ]
        );
    }

    #[test]
    fn attribution_survives_a_multi_byte_scalar_and_a_paste() {
        let events = replay("é\u{1b}[200~hi\u{1b}[201~🦀".as_bytes());
        let bytes: Vec<Vec<u8>> = events.iter().map(|(bytes, _)| bytes.clone()).collect();
        assert_eq!(bytes[0], "é".as_bytes());
        assert_eq!(bytes[1], "\u{1b}[200~hi\u{1b}[201~".as_bytes());
        assert_eq!(bytes[2], "🦀".as_bytes());
        // Nothing is lost and nothing is counted twice.
        let total: usize = bytes.iter().map(Vec::len).sum();
        assert_eq!(total, "é\u{1b}[200~hi\u{1b}[201~🦀".len());
    }

    #[test]
    fn a_lone_escape_at_end_of_input_is_one_byte() {
        // The reader resolves this by stalling, where `parse_event` alone would
        // still be waiting. Attribution has to handle it, or the byte column
        // would be wrong for every event after it.
        let events = replay(b"\x1b");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, b"\x1b");
        assert_eq!(symbolic(&events[0].0), "ESC");
        assert_eq!(describe_event(&events[0].1), "Escape  press");
    }

    #[test]
    fn an_unfinished_sequence_is_attributed_whole() {
        let events = replay(b"\x1b[99");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, b"\x1b[99");
        assert_eq!(events[0].1, Event::Unknown(b"\x1b[99".to_vec()));
    }

    #[test]
    fn nothing_is_attributed_to_bytes_that_were_taken_behind_our_back() {
        // The trap `resync` exists for: a probe reads a reply through the reader
        // directly, so those bytes never pass through `bytes_for` and would
        // otherwise be blamed on the next keypress.
        let mut keyboard = Keyboard::from_reader(io::Cursor::new(b"\x1b[?1u\r")).unwrap();
        let reply = keyboard
            .reader()
            .wait_for_reply(Duration::from_secs(1), |_| true)
            .unwrap();
        assert!(reply.is_some(), "the reply should have been picked up");

        keyboard.discard_pending();
        // With the mirror resynced there is nothing left to mis-attribute; the
        // pipe has also been drained, which is the documented cost.
        let next = keyboard.next_event(Duration::from_millis(50)).ok().flatten();
        assert!(next.is_none_or(|(_, bytes)| bytes == b"\r"));
    }

    #[test]
    fn a_plain_reader_reports_events_with_no_bytes() {
        // The fallback impl, for features that have no mirror.
        let mut reader = InputReader::from_reader(io::Cursor::new(b"a"), "test").unwrap();
        let (event, bytes) = reader
            .next_event(Duration::from_secs(1))
            .unwrap()
            .expect("an event");
        assert!(matches!(event, Event::Key(_)));
        assert!(bytes.is_empty(), "a bare reader cannot say which bytes");
    }
}
