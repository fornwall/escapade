//! The **interactive input step**: ask the reader to press something, then show
//! what actually arrived.
//!
//! This is a construction rather than a step. Demos in this project have so far
//! been one-way — the program draws, the reader presses space — and a keyboard
//! protocol cannot be demonstrated that way, because the whole subject is what
//! the terminal sends *back*. Any feature that wants to say "press this and watch"
//! can use it: mouse reporting, bracketed paste, focus events, progressive
//! enhancement of any kind.
//!
//! ```ignore
//! let collected = prompt::ask_for_keys(
//!     ctx,                       // anything that implements `StepView`
//!     keyboard,                  // anything that implements `EventSource`
//!     &prompt::Ask {
//!         press: "Shift+Enter",
//!         hint: "and then plain Enter, to compare",
//!         want: 4,
//!         patience: prompt::PATIENCE,
//!     },
//! )?;
//! ```
//!
//! # The rules, and why one of them is a teaching point
//!
//! **Return always advances to the next step.** That is not negotiable: it is how
//! every other step in every other demo works, and a reader who cannot get out of
//! a step is stuck. The consequence is that Return can never be one of the keys
//! being demonstrated — and *that consequence is the protocol's own subject*. In
//! legacy encoding, "the reader pressed Return" and "the reader pressed
//! Shift+Return" are the same bytes, so a prompt cannot tell an answer from a
//! request to move on. Rather than apologise for the limitation, the prompt says so
//! on screen: it is the clearest possible demonstration of why the protocol exists.
//!
//! With the disambiguate flag enabled the constraint relaxes exactly as far as the
//! protocol allows: **Shift+Enter is collected, plain Enter advances**, because
//! they are finally different sequences. A step can therefore demonstrate
//! Shift+Enter using Enter as its own control.
//!
//! The other rules:
//!
//! * `q` and Ctrl-C leave, and they work mid-collection — a reader who has enabled
//!   `report all keys as escape codes` has given away Ctrl-C's usual meaning, so
//!   the application has to honour it itself.
//! * Every event is shown **as it arrives**, not collected up and printed at the
//!   end. Watching a release event appear when a key comes up is the entire point.
//! * When nothing arrives at all, the prompt says so rather than waiting silently.
//!   A key that the terminal did not report is a real finding — it is what happens
//!   when you press Caps Lock without `report all keys as escape codes` — and an
//!   empty screen looks like a hung program instead.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::{
    features::keys::{
        bytes::EventSource,
        event::{describe_event, symbolic},
    },
    input::{Advance, Event, KeyCode, KeyEvent, KeyEventKind},
    theme::{Ink, Theme},
};

/// How long to wait with nothing at all arriving before saying so.
///
/// Long enough that a reader who is reading the prompt rather than typing is not
/// nagged, short enough that a key the terminal silently swallowed does not look
/// like a hang.
pub const PATIENCE: Duration = Duration::from_secs(6);

/// How long each read waits before the loop checks the clock.
const POLL: Duration = Duration::from_millis(120);

/// Column the event description starts at.
const BYTES_WIDTH: usize = 24;

/// Where an interactive step draws.
///
/// Deliberately three methods, all of which the demo contexts in this project
/// already have, so adopting [`ask_for_keys`] costs a feature one small `impl`
/// block rather than a refactor.
pub trait StepView {
    /// The theme, for colour and width.
    fn theme(&self) -> &Theme;

    /// Writes one line at the step's own indentation and moves down a row.
    ///
    /// # Errors
    ///
    /// Fails if the terminal cannot be written to.
    fn write_line(&mut self, text: &str) -> Result<()>;

    /// Makes everything written so far visible.
    ///
    /// Called after every event, because a step that batches its output defeats
    /// the purpose of watching keys arrive.
    ///
    /// # Errors
    ///
    /// Fails if the terminal cannot be written to.
    fn flush_view(&mut self) -> Result<()>;
}

/// What an interactive step asks for.
#[derive(Debug, Clone)]
pub struct Ask<'a> {
    /// What the reader should press, e.g. `"Shift+Enter"`. Shown verbatim.
    pub press: &'a str,
    /// One more line of context under the prompt, or `""` for none.
    pub hint: &'a str,
    /// How many events are worth showing before saying that is enough.
    ///
    /// Not a limit on what is collected — the reader stays in control until they
    /// press Return — just the point at which the step stops asking for more.
    pub want: usize,
    /// How long to wait in total silence before reporting it.
    pub patience: Duration,
}

impl Default for Ask<'_> {
    fn default() -> Self {
        Self {
            press: "any key",
            hint: "",
            want: 3,
            patience: PATIENCE,
        }
    }
}

/// One event, with the bytes that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// What the event decoded to.
    pub event: Event,
    /// The bytes the terminal sent. Empty if the source does not keep them.
    pub bytes: Vec<u8>,
}

/// What an interactive step ended up with.
#[derive(Debug, Clone)]
pub struct Collected {
    /// Where to go next: Return means [`Advance::Next`], `q` and Ctrl-C mean
    /// [`Advance::Quit`], and end of input means [`Advance::Quit`] as well.
    pub advance: Advance,
    /// Everything that arrived, in order, excluding the key that ended the step.
    pub observations: Vec<Observation>,
    /// Whether the wait ran out with nothing arriving at all.
    pub silent: bool,
}

impl Collected {
    /// Whether any event at all was seen.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }

    /// The key events among the observations, in order.
    #[must_use]
    pub fn keys(&self) -> Vec<KeyEvent> {
        self.observations
            .iter()
            .filter_map(|observation| match observation.event {
                Event::Key(key) => Some(key),
                _ => None,
            })
            .collect()
    }

    /// Whether any collected event was a release, which only the
    /// `report event types` flag can produce.
    #[must_use]
    pub fn saw_release(&self) -> bool {
        self.keys()
            .iter()
            .any(|key| key.kind == KeyEventKind::Release)
    }
}

/// Whether an event is the plain Return that advances a step.
///
/// Plain means *no modifiers at all*: Shift+Enter, Ctrl+Enter and Alt+Enter are
/// answers to the prompt, not requests to move on — which is only true at all
/// because the protocol makes them distinguishable. Under legacy encoding the
/// terminal sends the same carriage return for every one of them, and this
/// function cannot tell them apart because there is nothing to tell apart.
#[must_use]
pub fn is_return(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Enter,
            mods,
            kind: KeyEventKind::Press,
        }) if mods.is_empty()
    )
}

/// Whether an event asks to leave: `q`, `Q`, Ctrl-C or Ctrl-D.
///
/// Deliberately not Escape. Escape is one of the keys this protocol exists to
/// disambiguate, so a step demonstrating it must be able to receive it.
#[must_use]
pub fn is_leave(event: &Event) -> bool {
    match event {
        Event::Key(KeyEvent {
            code,
            mods,
            kind: KeyEventKind::Press,
        }) => match code {
            KeyCode::Char('q' | 'Q') => mods.is_empty(),
            KeyCode::Char('c' | 'd') => mods.ctrl,
            _ => false,
        },
        _ => false,
    }
}

/// Asks the reader to press something, and shows what arrives until they press
/// Return.
///
/// See the module documentation for the rules. Pending input is discarded on the
/// way out, so the release event of the Return that ended this step cannot leak
/// into the next one and look like an answer to its prompt.
///
/// # Errors
///
/// Fails if the terminal cannot be written to. Input ending is not an error; it
/// comes back as [`Advance::Quit`].
pub fn ask_for_keys<V, S>(view: &mut V, keys: &mut S, ask: &Ask<'_>) -> Result<Collected>
where
    V: StepView,
    S: EventSource,
{
    banner(view, ask)?;

    let mut collected = Collected {
        advance: Advance::Next,
        observations: Vec::new(),
        silent: false,
    };
    let mut said_enough = false;
    let started = Instant::now();

    loop {
        let pair = match keys.next_event(POLL) {
            Ok(Some(pair)) => pair,
            Ok(None) => {
                // Nothing yet. Say so once, rather than looking like a hang.
                if collected.observations.is_empty()
                    && !collected.silent
                    && started.elapsed() >= ask.patience
                {
                    collected.silent = true;
                    let theme_line = view.theme().paint(
                        "    … nothing arrived. This terminal reported no event for that — which \
                         is itself the answer.",
                        Ink::Warn,
                    );
                    view.write_line(&theme_line)?;
                    view.flush_view()?;
                }
                continue;
            }
            // Input ended: a closed terminal, or Ctrl-D at the operating system
            // level. Either way there is nobody left to ask.
            Err(_) => {
                collected.advance = Advance::Quit;
                break;
            }
        };
        let (event, bytes) = pair;

        if is_return(&event) {
            collected.advance = Advance::Next;
            break;
        }
        if is_leave(&event) {
            collected.advance = Advance::Quit;
            break;
        }

        view.write_line(&observation_line(view.theme(), &event, &bytes))?;
        view.flush_view()?;
        collected.observations.push(Observation { event, bytes });

        if collected.observations.len() >= ask.want && !said_enough {
            said_enough = true;
            let line = view.theme().paint(
                "    that is the shape of it — Return when you are ready",
                Ink::Muted,
            );
            view.write_line(&line)?;
            view.flush_view()?;
        }
    }

    // The Return that ended this step also produced a release event under the
    // `report event types` flag, and it is still in the pipe. Left there, it
    // would appear in the next step as an answer to a question nobody asked.
    keys.discard_pending();
    Ok(collected)
}

/// One line for one arrived event: the bytes, then what they decoded to.
///
/// A source that does not keep bytes gets the description alone rather than an
/// empty column.
#[must_use]
pub fn observation_line(theme: &Theme, event: &Event, bytes: &[u8]) -> String {
    let description = theme.paint(&describe_event(event), ink_for(event));
    if bytes.is_empty() {
        return format!("    {description}");
    }
    let sequence = symbolic(bytes);
    let padding = BYTES_WIDTH.saturating_sub(sequence.chars().count());
    format!(
        "    {}{}  {}",
        theme.paint(&sequence, Ink::Code),
        " ".repeat(padding),
        description
    )
}

/// The colour for an event, so a release does not look like a press.
fn ink_for(event: &Event) -> Ink {
    match event {
        Event::Key(key) => match key.kind {
            KeyEventKind::Press => Ink::Strong,
            KeyEventKind::Repeat => Ink::Accent,
            KeyEventKind::Release => Ink::Warn,
        },
        Event::Unknown(_) => Ink::Bad,
        _ => Ink::Muted,
    }
}

/// The prompt itself, including the rules.
fn banner<V: StepView>(view: &mut V, ask: &Ask<'_>) -> Result<()> {
    let press = view
        .theme()
        .paint(&format!("Press {}", ask.press), Ink::Accent);
    view.write_line(&format!("  {press}"))?;
    if !ask.hint.is_empty() {
        let hint = view.theme().paint(ask.hint, Ink::Muted);
        view.write_line(&format!("  {hint}"))?;
    }
    let rules = view.theme().paint(
        "Return moves on · q leaves · everything else is shown below as it arrives",
        Ink::Muted,
    );
    view.write_line(&format!("  {rules}"))?;
    view.flush_view()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::{Cursor, Read};

    use super::*;
    use crate::features::keys::bytes::Keyboard;

    /// A view that keeps its lines instead of drawing them, so the whole
    /// interactive step can be tested with no terminal anywhere.
    #[derive(Debug)]
    struct Recorder {
        theme: Theme,
        lines: Vec<String>,
        flushes: usize,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                theme: Theme::plain(),
                lines: Vec::new(),
                flushes: 0,
            }
        }

        /// Everything written, as one string.
        fn text(&self) -> String {
            self.lines.join("\n")
        }
    }

    impl StepView for Recorder {
        fn theme(&self) -> &Theme {
            &self.theme
        }

        fn write_line(&mut self, text: &str) -> Result<()> {
            self.lines.push(text.trim_end().to_owned());
            Ok(())
        }

        fn flush_view(&mut self) -> Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    /// A source that says nothing for a while and then ends, so the silence path
    /// can be tested without waiting for a real reader.
    #[derive(Debug)]
    struct Silence;

    impl Read for Silence {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(400));
            Ok(0)
        }
    }

    /// Runs the step over a recorded byte stream.
    fn run(bytes: &'static [u8], ask: &Ask<'_>) -> (Recorder, Collected) {
        let mut view = Recorder::new();
        let mut keyboard = Keyboard::from_reader(Cursor::new(bytes)).unwrap();
        let collected = ask_for_keys(&mut view, &mut keyboard, ask).unwrap();
        (view, collected)
    }

    #[test]
    fn return_advances_and_collects_nothing() {
        let (_, collected) = run(b"\r", &Ask::default());
        assert!(matches!(collected.advance, Advance::Next));
        assert!(collected.is_empty());
        assert!(!collected.silent);
    }

    #[test]
    fn events_are_collected_until_return_and_shown_as_they_arrive() {
        let ask = Ask {
            press: "Shift+Enter",
            ..Ask::default()
        };
        let (view, collected) = run(b"\x1b[13;2u\x1b[13;5u\r", &ask);
        assert_eq!(collected.observations.len(), 2);
        assert!(matches!(collected.advance, Advance::Next));

        // Both halves of each event are on screen, and the prompt named the key.
        let text = view.text();
        assert!(text.contains("Press Shift+Enter"), "{text}");
        assert!(text.contains("CSI 13;2u"), "{text}");
        assert!(text.contains("Shift+Enter  press"), "{text}");
        assert!(text.contains("CSI 13;5u"), "{text}");
        assert!(text.contains("Ctrl+Enter  press"), "{text}");
        // One flush per line at least: nothing is batched until the end.
        assert!(view.flushes >= 3, "flushed {} times", view.flushes);
    }

    #[test]
    fn shift_enter_is_an_answer_while_plain_enter_moves_on() {
        // The exact distinction the protocol buys, and the reason an interactive
        // step can demonstrate Shift+Enter at all.
        assert!(!is_return(&Event::Key(KeyEvent {
            code: KeyCode::Enter,
            mods: crate::input::Modifiers {
                shift: true,
                ..crate::input::Modifiers::NONE
            },
            kind: KeyEventKind::Press,
        })));
        let (_, collected) = run(b"\x1b[13;2u\r", &Ask::default());
        assert_eq!(collected.observations.len(), 1);
        assert_eq!(collected.keys()[0].code, KeyCode::Enter);
        assert!(collected.keys()[0].mods.shift);
    }

    #[test]
    fn a_release_only_counts_as_return_when_it_is_a_press() {
        // With `report event types` on, Return arrives twice. Only the press may
        // advance, or the release would advance the *next* step too.
        assert!(is_return(&Event::Key(KeyEvent {
            code: KeyCode::Enter,
            mods: crate::input::Modifiers::NONE,
            kind: KeyEventKind::Press,
        })));
        assert!(!is_return(&Event::Key(KeyEvent {
            code: KeyCode::Enter,
            mods: crate::input::Modifiers::NONE,
            kind: KeyEventKind::Release,
        })));
    }

    #[test]
    fn q_leaves_mid_collection() {
        let (_, collected) = run(b"\x1b[13;2uq\r", &Ask::default());
        assert!(matches!(collected.advance, Advance::Quit));
        // What arrived before the quit is still reported.
        assert_eq!(collected.observations.len(), 1);
    }

    #[test]
    fn ctrl_c_leaves_even_when_the_terminal_stopped_sending_sigint() {
        // With `report all keys as escape codes` the terminal no longer raises
        // SIGINT, so the application is the only thing that can honour Ctrl-C.
        let (_, collected) = run(b"\x1b[99;5u", &Ask::default());
        assert!(matches!(collected.advance, Advance::Quit));
        let (_, raw) = run(b"\x03", &Ask::default());
        assert!(matches!(raw.advance, Advance::Quit));
    }

    #[test]
    fn escape_is_collected_not_treated_as_a_way_out() {
        // Escape is one of the keys the protocol disambiguates, so a step about it
        // has to be able to receive it.
        assert!(!is_leave(&Event::Key(KeyEvent {
            code: KeyCode::Esc,
            mods: crate::input::Modifiers::NONE,
            kind: KeyEventKind::Press,
        })));
        let (_, collected) = run(b"\x1b[27u\r", &Ask::default());
        assert_eq!(collected.observations.len(), 1);
        assert_eq!(collected.keys()[0].code, KeyCode::Esc);
    }

    #[test]
    fn a_bare_q_leaves_but_a_modified_one_is_an_answer() {
        assert!(is_leave(&Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            mods: crate::input::Modifiers::NONE,
            kind: KeyEventKind::Press,
        })));
        // Ctrl+q is a plausible thing to demonstrate; it must not exit.
        assert!(!is_leave(&Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            mods: crate::input::Modifiers {
                ctrl: true,
                ..crate::input::Modifiers::NONE
            },
            kind: KeyEventKind::Press,
        })));
    }

    #[test]
    fn end_of_input_ends_the_step_rather_than_hanging() {
        let (_, collected) = run(b"", &Ask::default());
        assert!(matches!(collected.advance, Advance::Quit));
        assert!(collected.is_empty());
    }

    #[test]
    fn reaching_the_wanted_count_says_so_once() {
        let ask = Ask {
            want: 2,
            ..Ask::default()
        };
        let (view, collected) = run(b"abc\r", &ask);
        assert_eq!(collected.observations.len(), 3);
        let text = view.text();
        assert_eq!(
            text.matches("that is the shape of it").count(),
            1,
            "said more than once: {text}"
        );
    }

    #[test]
    fn silence_is_reported_rather_than_waited_out_forever() {
        let mut view = Recorder::new();
        let mut keyboard = Keyboard::from_reader(Silence).unwrap();
        let ask = Ask {
            press: "Caps Lock",
            patience: Duration::from_millis(1),
            ..Ask::default()
        };
        let collected = ask_for_keys(&mut view, &mut keyboard, &ask).unwrap();
        assert!(collected.silent, "the empty wait should have been reported");
        assert!(collected.is_empty());
        let text = view.text();
        assert!(text.contains("nothing arrived"), "{text}");
        assert!(
            text.contains("itself the answer"),
            "a key the terminal never reported is a finding: {text}"
        );
    }

    #[test]
    fn releases_are_recognised_in_what_was_collected() {
        let (_, collected) = run(b"\x1b[97;1:1u\x1b[97;1:3u\r", &Ask::default());
        assert_eq!(collected.observations.len(), 2);
        assert!(collected.saw_release());
        let (_, presses_only) = run(b"a\r", &Ask::default());
        assert!(!presses_only.saw_release());
    }

    #[test]
    fn an_event_line_shows_the_bytes_and_survives_a_source_without_them() {
        let theme = Theme::plain();
        let event = Event::Key(KeyEvent {
            code: KeyCode::Enter,
            mods: crate::input::Modifiers {
                shift: true,
                ..crate::input::Modifiers::NONE
            },
            kind: KeyEventKind::Press,
        });
        let with = observation_line(&theme, &event, b"\x1b[13;2u");
        assert!(with.contains("CSI 13;2u") && with.contains("Shift+Enter"));
        assert!(!with.contains('\u{1b}'));
        let without = observation_line(&theme, &event, b"");
        assert!(without.contains("Shift+Enter"));
        assert!(!without.contains("CSI"));
    }

    #[test]
    fn undecodable_keys_are_still_shown() {
        // Caps Lock: a real key with no `KeyCode`. The bytes are the answer.
        let (view, collected) = run(b"\x1b[57358u\r", &Ask::default());
        assert_eq!(collected.observations.len(), 1);
        assert!(view.text().contains("CSI 57358u"), "{}", view.text());
    }
}
