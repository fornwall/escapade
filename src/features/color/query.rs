//! Asking the terminal what colour it is: the queries, and the round trip.
//!
//! Six things can be asked about, and they are five different OSC numbers plus
//! one that takes an index:
//!
//! ```text
//! →  OSC 10 ; ? ST              default foreground
//! →  OSC 11 ; ? ST              default background
//! →  OSC 12 ; ? ST              cursor
//! →  OSC 17 ; ? ST              selection background
//! →  OSC 19 ; ? ST              selection foreground
//! →  OSC 4 ; <index> ; ? ST     one of the 256 palette entries
//! ←  OSC <same number> ; rgb:<r>/<g>/<b> ST
//! ```
//!
//! xterm calls 17 and 19 the *highlight* colours, which is the same thing under
//! an older name. The pair is easy to get backwards — 17 is the background and 19
//! the foreground, with 18 being the tektronix cursor in between — so the numbers
//! are written down once, here, and never repeated as literals.
//!
//! # The barrier
//!
//! Every query is followed by one Primary Device Attributes request, `CSI c`,
//! which every terminal since the VT100 answers. Terminals process input in
//! order, so a barrier reply with no colour reply before it means the terminal
//! demonstrably had its chance and declined. Without that, a silent terminal and
//! a slow one are indistinguishable. See
//! [`crate::features::glyph_protocol::probe`], which established the pattern, and
//! [`super::probe`] for why a multiplexer voids the negative.
//!
//! # Why the queries are not wrapped for a multiplexer
//!
//! Unlike APC, `OSC 4`, `OSC 10` and `OSC 11` are sequences tmux *knows*: it
//! keeps its own idea of the palette and answers from that. Wrapping the query in
//! tmux's passthrough form would ask the outer terminal instead, which sounds
//! better until you notice that the answer then describes a terminal whose colours
//! tmux may be overriding. Either way the reply cannot be attributed with
//! confidence, so the query goes out plain and the report says a multiplexer is in
//! the way.

use std::{
    io::{IsTerminal as _, stdin, stdout},
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    cli::ColorTarget,
    features::color::value::{Reported, parse_reply},
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, OSC, ST, Term},
};

/// How long to wait for the terminal to answer.
///
/// A full second, per the house rule for every probe in this project: a slow ssh
/// link must not be reported as a terminal that does not implement the query.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to spend consuming replies still in flight once a conclusion is
/// reached.
///
/// Short, because by this point the terminal has answered something and anything
/// outstanding is either already buffered or arriving immediately. Skipping this
/// is what leaves `11;rgb:…` printed in the user's shell a moment later.
pub const DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// One colour that can be asked about.
///
/// `Selection` in the command line surface is two of these, because the
/// foreground and background of a selection are separate queries and terminals do
/// implement one without the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Default text colour: `OSC 10`.
    Foreground,
    /// Default background colour: `OSC 11`.
    Background,
    /// Cursor colour: `OSC 12`.
    Cursor,
    /// Selection background, xterm's highlight background: `OSC 17`.
    SelectionBackground,
    /// Selection foreground, xterm's highlight foreground: `OSC 19`.
    SelectionForeground,
    /// One of the 256 palette entries: `OSC 4 ; <index>`.
    Palette(u8),
}

impl Target {
    /// The three colours worth reporting by default.
    ///
    /// Foreground, background and cursor: the ones an application styling its own
    /// output actually needs, and the ones nearly every terminal answers.
    pub const DEFAULT: [Self; 3] = [Self::Foreground, Self::Background, Self::Cursor];

    /// The OSC number that asks about this colour, and that answers.
    #[must_use]
    pub fn ps(self) -> u16 {
        match self {
            Self::Palette(_) => 4,
            Self::Foreground => 10,
            Self::Background => 11,
            Self::Cursor => 12,
            Self::SelectionBackground => 17,
            Self::SelectionForeground => 19,
        }
    }

    /// The palette index, for a palette target.
    #[must_use]
    pub fn index(self) -> Option<u8> {
        match self {
            Self::Palette(index) => Some(index),
            _ => None,
        }
    }

    /// Human label for a report line.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Foreground => "foreground".to_owned(),
            Self::Background => "background".to_owned(),
            Self::Cursor => "cursor".to_owned(),
            Self::SelectionBackground => "selection background".to_owned(),
            Self::SelectionForeground => "selection foreground".to_owned(),
            Self::Palette(index) => format!("palette {index}"),
        }
    }

    /// Stable machine-readable name, for `--format json`.
    #[must_use]
    pub fn slug(self) -> String {
        match self {
            Self::Foreground => "foreground".to_owned(),
            Self::Background => "background".to_owned(),
            Self::Cursor => "cursor".to_owned(),
            Self::SelectionBackground => "selection-background".to_owned(),
            Self::SelectionForeground => "selection-foreground".to_owned(),
            Self::Palette(index) => format!("palette-{index}"),
        }
    }

    /// The query for this colour, written symbolically in the doc comment
    /// because the real one contains control bytes.
    ///
    /// * `OSC 10 ; ? ST`, `OSC 11 ; ? ST`, `OSC 12 ; ? ST`
    /// * `OSC 17 ; ? ST`, `OSC 19 ; ? ST`
    /// * `OSC 4 ; <index> ; ? ST`
    ///
    /// `ST` is used rather than `BEL` to end the string. Both are accepted
    /// everywhere, but `ST` is the standards-conforming terminator and `BEL` in an
    /// outgoing query has been known to make a terminal beep.
    #[must_use]
    pub fn query(self) -> String {
        match self {
            Self::Palette(index) => format!("{OSC}4;{index};?{ST}"),
            other => format!("{OSC}{};?{ST}", other.ps()),
        }
    }

    /// Whether `reply` is this target's answer.
    ///
    /// The OSC number is not enough on its own: every palette entry answers as
    /// `OSC 4`, so the index has to match too, or querying entries 1 and 2 in one
    /// batch would attribute both answers to entry 1.
    #[must_use]
    pub fn matches(self, reply: &Reported) -> bool {
        reply.ps == self.ps() && reply.index == self.index()
    }

    /// Expands one command line target into the queries it means.
    ///
    /// `--query selection` becomes two queries, and `--query palette` becomes 256
    /// — which is what the flag says, and why `--index N` exists for the far more
    /// common case of wanting one entry.
    #[must_use]
    pub fn expand(target: ColorTarget) -> Vec<Self> {
        match target {
            ColorTarget::Foreground => vec![Self::Foreground],
            ColorTarget::Background => vec![Self::Background],
            ColorTarget::Cursor => vec![Self::Cursor],
            ColorTarget::Selection => vec![Self::SelectionBackground, Self::SelectionForeground],
            ColorTarget::Palette => (0..=255).map(Self::Palette).collect(),
        }
    }

    /// The full target list for a command's `--query` flags and `--index`.
    ///
    /// With neither, the answer is [`Target::DEFAULT`]. Duplicates are removed
    /// while order is kept, so `--query background --query background` sends one
    /// query rather than two and cannot end up with a reply left over.
    #[must_use]
    pub fn resolve(queries: &[ColorTarget], index: Option<u8>) -> Vec<Self> {
        let mut targets: Vec<Self> = if queries.is_empty() && index.is_none() {
            Self::DEFAULT.to_vec()
        } else {
            queries.iter().copied().flat_map(Self::expand).collect()
        };
        if let Some(index) = index {
            targets.push(Self::Palette(index));
        }
        let mut seen = Vec::with_capacity(targets.len());
        targets.retain(|target| {
            if seen.contains(target) {
                false
            } else {
                seen.push(*target);
                true
            }
        });
        targets
    }
}

/// What one target's query produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The colour asked about.
    pub target: Target,
    /// What came back, or `None` if the terminal said nothing about it.
    pub reported: Option<Reported>,
}

impl Answer {
    /// Whether the terminal answered this one.
    #[must_use]
    pub fn answered(&self) -> bool {
        self.reported.is_some()
    }
}

/// Everything one round trip established.
#[expect(
    clippy::struct_field_names,
    reason = "`answers.answers` reads oddly, but the struct is the whole result \
              of a round trip and the field is the per-target answers within it; \
              every alternative name describes the list less well"
)]
#[derive(Debug, Clone, Default)]
pub struct Answers {
    /// One entry per target, in the order they were asked about.
    pub answers: Vec<Answer>,
    /// Whether the Primary Device Attributes barrier came back. Without it, a
    /// missing colour is inconclusive rather than a refusal.
    pub answered_barrier: bool,
    /// A multiplexer between this process and the real terminal, if any.
    pub multiplexer: Option<Multiplexer>,
    /// Whether stdin and stdout are both terminals. Without that, nothing could
    /// be asked at all.
    pub interactive: bool,
    /// Replies that were heard but belonged to nothing that was asked, kept so a
    /// debug report can show them rather than swallow them.
    pub stray: Vec<Reply>,
}

impl Answers {
    /// The answer for one target, if it was asked about.
    #[must_use]
    pub fn get(&self, target: Target) -> Option<&Reported> {
        self.answers
            .iter()
            .find(|answer| answer.target == target)
            .and_then(|answer| answer.reported.as_ref())
    }

    /// Whether the terminal answered at least one query.
    #[must_use]
    pub fn any_answered(&self) -> bool {
        self.answers.iter().any(Answer::answered)
    }

    /// Whether a missing answer may be reported as a refusal.
    ///
    /// Only when the barrier came back **and** no multiplexer is in the way: a
    /// multiplexer answers Primary Device Attributes from its own emulator, so
    /// inside one the barrier says nothing about the terminal beyond it.
    #[must_use]
    pub fn silence_is_conclusive(&self) -> bool {
        self.interactive && self.answered_barrier && self.multiplexer.is_none()
    }
}

/// Whether this process can ask the terminal anything at all.
///
/// Both directions are needed: the query goes out on stdout and the answer comes
/// back on stdin, so a redirect on either end breaks the round trip.
#[must_use]
pub fn interactive() -> bool {
    stdout().is_terminal() && stdin().is_terminal()
}

/// Asks the terminal about every target in one batch, and collects the answers.
///
/// The queries all go out first, then the barrier, then replies are read until
/// every target has answered or the barrier comes back. Batching is what keeps
/// `escapade color` to a single round trip rather than one per colour, which
/// matters over ssh and matters a great deal for a 256-entry palette dump.
///
/// Whatever happens, the barrier reply is consumed before returning. An unread
/// reply is delivered to whatever runs next — the user's shell, which prints it
/// as stray text — and invisibility is the whole point of a query.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal that
/// simply says nothing is not an error; it is a result.
pub fn ask(term: &mut Term, input: &mut InputReader, targets: &[Target]) -> Result<Answers> {
    round_trip(term, input, targets, true)
}

/// As [`ask`], but without discarding input that is already pending.
///
/// For [`super::watch`], which is holding a loop open and must not throw away the
/// Ctrl-C the user pressed a moment before a re-query started. The initial drain
/// exists to clear junk before the *first* query of a run; in a loop it would
/// silently eat keystrokes.
///
/// # Errors
///
/// As [`ask`].
pub fn ask_again(term: &mut Term, input: &mut InputReader, targets: &[Target]) -> Result<Answers> {
    round_trip(term, input, targets, false)
}

/// The round trip both entry points share.
fn round_trip(
    term: &mut Term,
    input: &mut InputReader,
    targets: &[Target],
    drain_first: bool,
) -> Result<Answers> {
    let mut outcome = Answers {
        answers: targets
            .iter()
            .map(|&target| Answer {
                target,
                reported: None,
            })
            .collect(),
        multiplexer: Multiplexer::detect(),
        interactive: interactive(),
        ..Answers::default()
    };
    if !outcome.interactive || targets.is_empty() {
        return Ok(outcome);
    }

    // Raw mode is required: in cooked mode the reply would sit in the line buffer
    // until the user pressed Enter, and would be echoed back at them first.
    term.enable_raw()?;
    if drain_first {
        input.drain();
    }

    // Every colour question is an OSC string, and a terminal that implements none
    // of them may print the body — `10;?`, `11;?`, `4;3;?` — instead of
    // discarding it. One shield covers the whole batch; the barrier is a control
    // sequence and needs none.
    let queries: String = targets.iter().map(|target| target.query()).collect();
    term.write_query(&queries)?;
    term.write_now(&barrier())?;

    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut outstanding = targets.len();
    while outstanding > 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // An EOF on stdin means there is nobody to answer; treat it as silence.
        let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
            break;
        };
        match &reply {
            Reply::Osc(payload) => match parse_reply(payload) {
                Ok(reported) => {
                    if let Some(answer) = outcome
                        .answers
                        .iter_mut()
                        .find(|answer| answer.target.matches(&reported) && !answer.answered())
                    {
                        answer.reported = Some(reported);
                        outstanding -= 1;
                    } else {
                        outcome.stray.push(reply.clone());
                    }
                }
                // Not a colour reply, or a malformed one. Either way it is not an
                // answer to anything here; keep it for the debug report.
                Err(_) => outcome.stray.push(reply.clone()),
            },
            Reply::Csi {
                final_byte: 'c', ..
            } => {
                // Everything sent before the barrier has been processed, so
                // anything still missing is missing on purpose.
                outcome.answered_barrier = true;
                break;
            }
            _ => outcome.stray.push(reply.clone()),
        }
    }

    drain_barrier(input, &mut outcome);
    Ok(outcome)
}

/// Runs one round trip from scratch, for the commands that only need one.
///
/// The terminal is restored before returning, so the caller prints its report
/// into a normal cooked-mode terminal.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn detect(targets: &[Target]) -> Result<Answers> {
    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let outcome = ask(&mut term, &mut input, targets);
    term.restore();
    outcome
}

/// Primary Device Attributes request: `CSI c` — the barrier.
#[must_use]
pub fn barrier() -> String {
    format!("{CSI}c")
}

/// Consumes the barrier reply if it has not already arrived.
///
/// Called on every path out of [`ask`], including the ones that concluded early,
/// because the barrier is sent last and so is still in flight exactly when a
/// query was answered promptly.
fn drain_barrier(input: &mut InputReader, outcome: &mut Answers) {
    if outcome.answered_barrier {
        return;
    }
    if let Ok(Some(reply)) = input.wait_for_reply(DRAIN_TIMEOUT, |reply| {
        matches!(
            reply,
            Reply::Csi {
                final_byte: 'c',
                ..
            }
        )
    }) {
        outcome.stray.retain(|kept| kept != &reply);
        outcome.answered_barrier = true;
    }
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::term::pretty;

    #[test]
    fn a_shielded_batch_leaves_no_trace_on_a_terminal_that_prints_osc() {
        use crate::term::{
            screen::{Screen, Strings},
            shielded,
        };
        let targets = [
            Target::Background,
            Target::Foreground,
            Target::Cursor,
            Target::Palette(3),
        ];
        let queries: String = targets.iter().map(|target| target.query()).collect();
        let mut screen = Screen::new(80, 24, Strings::Printed);
        screen.goto(23, 0);
        let before = screen.text();
        screen.feed(&shielded(&queries));
        assert_eq!(screen.text(), before, "the queries were left on the screen");
        assert_eq!(screen.cursor(), (23, 0));
        assert_eq!(screen.scrolled(), 0);
    }

    #[test]
    fn every_target_asks_with_the_documented_osc_number() {
        assert_eq!(pretty(&Target::Foreground.query()), "OSC 10;? ST");
        assert_eq!(pretty(&Target::Background.query()), "OSC 11;? ST");
        assert_eq!(pretty(&Target::Cursor.query()), "OSC 12;? ST");
        assert_eq!(pretty(&Target::SelectionBackground.query()), "OSC 17;? ST");
        assert_eq!(pretty(&Target::SelectionForeground.query()), "OSC 19;? ST");
        assert_eq!(pretty(&Target::Palette(4).query()), "OSC 4;4;? ST");
        assert_eq!(pretty(&Target::Palette(255).query()), "OSC 4;255;? ST");
    }

    #[test]
    fn selection_background_is_seventeen_and_foreground_is_nineteen() {
        // The pair xterm documents as highlight background and highlight
        // foreground, and the pair easiest to swap by accident.
        assert_eq!(Target::SelectionBackground.ps(), 17);
        assert_eq!(Target::SelectionForeground.ps(), 19);
    }

    #[test]
    fn a_query_ends_with_st_and_never_with_bel() {
        for target in [Target::Background, Target::Palette(0)] {
            let query = target.query();
            assert!(query.ends_with('\\'), "{:?}", pretty(&query));
            assert!(!query.contains('\u{7}'), "a query must not beep");
        }
    }

    #[test]
    fn a_palette_answer_is_matched_by_index_as_well_as_number() {
        let one = parse_reply("4;1;rgb:f/0/0").unwrap();
        assert!(Target::Palette(1).matches(&one));
        assert!(!Target::Palette(2).matches(&one), "index must be checked");
        assert!(!Target::Background.matches(&one));

        let background = parse_reply("11;rgb:0/0/0").unwrap();
        assert!(Target::Background.matches(&background));
        assert!(
            !Target::Palette(11).matches(&background),
            "11 is not index 11"
        );
    }

    #[test]
    fn the_default_report_is_foreground_background_and_cursor() {
        let targets = Target::resolve(&[], None);
        assert_eq!(
            targets,
            vec![Target::Foreground, Target::Background, Target::Cursor]
        );
    }

    #[test]
    fn selection_expands_to_both_of_its_colours() {
        let targets = Target::resolve(&[ColorTarget::Selection], None);
        assert_eq!(
            targets,
            vec![Target::SelectionBackground, Target::SelectionForeground]
        );
    }

    #[test]
    fn palette_expands_to_every_entry_exactly_once() {
        let targets = Target::resolve(&[ColorTarget::Palette], None);
        assert_eq!(targets.len(), 256);
        assert_eq!(targets.first(), Some(&Target::Palette(0)));
        assert_eq!(targets.last(), Some(&Target::Palette(255)));
    }

    #[test]
    fn an_index_is_added_to_whatever_else_was_asked_for() {
        assert_eq!(Target::resolve(&[], Some(4)), vec![Target::Palette(4)]);
        assert_eq!(
            Target::resolve(&[ColorTarget::Background], Some(7)),
            vec![Target::Background, Target::Palette(7)]
        );
        // Index 0 is a real entry, not an absent one.
        assert_eq!(Target::resolve(&[], Some(0)), vec![Target::Palette(0)]);
    }

    #[test]
    fn repeated_queries_are_asked_once() {
        // Two identical queries would produce one reply on most terminals, and
        // the second target would then look unanswered.
        let targets = Target::resolve(
            &[
                ColorTarget::Background,
                ColorTarget::Background,
                ColorTarget::Foreground,
            ],
            None,
        );
        assert_eq!(targets, vec![Target::Background, Target::Foreground]);
        // And an --index that duplicates a --query palette entry collapses too.
        let with_index = Target::resolve(&[ColorTarget::Palette], Some(4));
        assert_eq!(with_index.len(), 256);
    }

    #[test]
    fn slugs_are_stable_kebab_case_and_unique() {
        let targets = [
            Target::Foreground,
            Target::Background,
            Target::Cursor,
            Target::SelectionBackground,
            Target::SelectionForeground,
            Target::Palette(0),
            Target::Palette(255),
        ];
        let mut slugs: Vec<String> = targets.iter().map(|target| target.slug()).collect();
        for slug in &slugs {
            assert!(
                slug.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-'),
                "{slug} is not kebab-case"
            );
        }
        let count = slugs.len();
        slugs.sort();
        slugs.dedup();
        assert_eq!(slugs.len(), count, "two targets share a slug");
    }

    #[test]
    fn the_barrier_is_primary_device_attributes() {
        assert_eq!(pretty(&barrier()), "CSI c");
    }

    #[test]
    fn silence_is_only_conclusive_outside_a_multiplexer() {
        let base = Answers {
            interactive: true,
            answered_barrier: true,
            ..Answers::default()
        };
        assert!(base.silence_is_conclusive());

        // tmux answers Primary Device Attributes from its own emulator, so the
        // barrier proves nothing about the terminal beyond it.
        let inside = Answers {
            multiplexer: Some(Multiplexer::Tmux),
            ..base.clone()
        };
        assert!(!inside.silence_is_conclusive());

        // No barrier at all: something is swallowing replies.
        let quiet = Answers {
            answered_barrier: false,
            ..base.clone()
        };
        assert!(!quiet.silence_is_conclusive());

        // Not a terminal: nothing was asked in the first place.
        let piped = Answers {
            interactive: false,
            ..base
        };
        assert!(!piped.silence_is_conclusive());
    }

    #[test]
    fn answers_are_looked_up_by_target() {
        let reported = parse_reply("11;rgb:1e1e/1e1e/2e2e").unwrap();
        let answers = Answers {
            answers: vec![
                Answer {
                    target: Target::Foreground,
                    reported: None,
                },
                Answer {
                    target: Target::Background,
                    reported: Some(reported.clone()),
                },
            ],
            ..Answers::default()
        };
        assert_eq!(answers.get(Target::Background), Some(&reported));
        assert_eq!(answers.get(Target::Foreground), None);
        assert_eq!(answers.get(Target::Cursor), None, "never asked");
        assert!(answers.any_answered());
    }
}
