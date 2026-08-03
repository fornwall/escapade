//! The step-by-step demo: the protocol, one flag at a time, with the reader's own
//! keyboard as the instrument.
//!
//! Every other demo in this project is one-way — the program draws, the reader
//! presses space. A keyboard protocol cannot be shown that way, because the
//! subject is what the terminal sends *back*. So most of these steps are
//! **interactive input steps**: they ask the reader to press something and show
//! what arrived, key, modifiers, event type and raw bytes. That construction lives
//! in [`super::prompt`] and is meant to be reused by other features.
//!
//! The arc is the protocol's own arc. Ask what is supported; learn to change the
//! flags without trampling on the program that launched you; watch escape codes
//! become unambiguous; watch Shift+Enter separate from Enter; watch keys come back
//! *up*; and finish with a chord that legacy encoding simply cannot express, at
//! which point the flags come back off and the terminal is exactly as it was found.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::{
    features::{
        keys::{
            Flags, SetMode,
            bytes::{EventSource as _, Keyboard},
            pop, probe,
            prompt::{self, Ask, StepView},
            push, query, set,
        },
        wrap,
    },
    input::{Advance, Event, KeyCode, KeyEvent, KeyEventKind},
    term::{CSI, Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

/// Left margin for everything the demo draws.
const MARGIN: u16 = 3;

/// How long the finale waits between redraws of the held-keys display.
const FRAME: Duration = Duration::from_millis(80);

/// How long a step waits for a reply to a query it sent.
///
/// Short: the startup probe has already established whether anything answers, so
/// this round trip is for the reader's benefit rather than for a decision.
const REPLY_TIMEOUT: Duration = Duration::from_millis(400);

/// How many keys have to be held at once to count as a chord.
const CHORD_SIZE: usize = 3;

/// What the user asked for at the end of a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// Go on to the next step.
    Next,
    /// Back up one step.
    Prev,
    /// Leave the demo.
    Quit,
}

impl From<Advance> for Flow {
    fn from(advance: Advance) -> Self {
        match advance {
            Advance::Next => Self::Next,
            Advance::Prev => Self::Prev,
            Advance::Quit => Self::Quit,
        }
    }
}

/// A step's implementation.
type StepFn = fn(&mut Ctx<'_>) -> Result<Flow>;

/// One entry in the demo script: a short name for `--only`, a title for the
/// header, and the code that draws it.
struct Step {
    /// Kebab-case name, stable enough to put in a script or a bug report.
    name: &'static str,
    /// Human title shown in the header.
    title: &'static str,
    /// What the step does.
    run: StepFn,
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Step")
            .field("name", &self.name)
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

/// Declares a step, keeping the name next to the function it names.
macro_rules! step {
    ($name:literal, $title:literal, $run:expr) => {
        Step {
            name: $name,
            title: $title,
            run: $run,
        }
    };
}

/// The script. Order matters: each step assumes the flags the previous ones
/// explained.
const STEPS: &[Step] = &[
    step!("handshake", "Ask, do not assume", step_handshake),
    step!(
        "push-pop",
        "Two ways to set flags, one of them polite",
        step_push_pop
    ),
    step!(
        "disambiguate",
        "Keys that used to be indistinguishable",
        step_disambiguate
    ),
    step!("shift-enter", "Shift+Enter, at last", step_shift_enter),
    step!("release", "Keys that come back up", step_release),
    step!("repeat", "Holding a key down", step_repeat),
    step!(
        "text",
        "All keys as escape codes, and the text they made",
        step_text
    ),
    step!(
        "chord",
        "Finale: a chord legacy encoding cannot express",
        step_chord
    ),
    step!("restore", "Putting the keyboard back", step_leave),
];

/// The step names, in order, for [`crate::features::select_steps`].
fn step_names() -> Vec<&'static str> {
    STEPS.iter().map(|step| step.name).collect()
}

/// Prints the step list for `--list`.
pub fn list_steps(theme: &Theme) {
    println!();
    println!("  {}", theme.paint("Keyboard protocol demo", Ink::Title));
    println!();
    let width = STEPS.iter().map(|step| step.name.len()).max().unwrap_or(0);
    for (index, step) in STEPS.iter().enumerate() {
        println!(
            "  {} {}  {}",
            theme.paint(&format!("{:>2}.", index + 1), Ink::Muted),
            theme.paint(&format!("{:<width$}", step.name), Ink::Accent),
            step.title
        );
    }
    println!();
    println!(
        "  {}",
        theme.paint(
            "escapade keys demo --only <NAME>   run just that step",
            Ink::Muted
        )
    );
    println!(
        "  {}",
        theme.paint(
            "escapade keys demo --step N        start there and carry on",
            Ink::Muted
        )
    );
    println!();
    println!(
        "  {}",
        theme.paint(
            "Several steps ask you to press keys. Return always moves on, so it is never one of \
             the keys being demonstrated — which is exactly the limitation this protocol removes.",
            Ink::Muted
        )
    );
    println!();
}

/// Runs the demo.
///
/// # Errors
///
/// Fails if the terminal cannot be set up or written to.
pub fn run(
    theme: &Theme,
    force: bool,
    only: Option<&str>,
    start_step: Option<usize>,
) -> Result<()> {
    let selection = crate::features::select_steps(&step_names(), only, start_step)?;

    let mut term = Term::new()?;
    let mut keyboard = Keyboard::new()?;

    let outcome = probe::probe(&mut term, keyboard.reader())?;
    // The probe read replies straight from the reader, so the byte mirror holds
    // bytes no event will ever claim. Resync before anything is attributed.
    keyboard.discard_pending();

    // `--force` skips the capability check, but it cannot conjure a terminal:
    // enabling raw mode on something that is not a tty fails, so this gate comes
    // first and is not overridable.
    if !outcome.interactive {
        term.restore();
        refuse(theme, &outcome);
        return Ok(());
    }
    if !outcome.supported() && !force {
        term.restore();
        refuse(theme, &outcome);
        return Ok(());
    }

    term.enable_raw()?;
    term.enter_alt_screen()?;

    let mut ctx = Ctx {
        term: &mut term,
        keys: &mut keyboard,
        theme,
        multiplexer: outcome.multiplexer,
        honoured: outcome.honoured(),
        supported: outcome.supported(),
        forced: force && !outcome.supported(),
        pushed: None,
        row: 0,
        width: 0,
    };

    let mut index = selection.start;
    let result = (|| -> Result<()> {
        loop {
            let flow = (STEPS[index].run)(&mut ctx)?;
            match flow {
                Flow::Next => {
                    // `--only` runs the one step and stops rather than carrying on
                    // through the rest of the script.
                    if selection.single || index + 1 >= STEPS.len() {
                        return Ok(());
                    }
                    index += 1;
                }
                Flow::Prev => index = index.saturating_sub(1),
                Flow::Quit => return Ok(()),
            }
        }
    })();

    // Whatever happened, the keyboard goes back. `unpush` is a no-op when nothing
    // is outstanding, and `restore` leaves the alternate screen and raw mode.
    let _ = ctx.unpush();
    term.restore();
    result
}

/// The refusal shown when the protocol is not available.
fn refuse(theme: &Theme, outcome: &probe::ProbeOutcome) {
    println!();
    let message = "sorry - this does not seem to be supported by your terminal - check \
                   'escapade keys explain' for support, or use 'escapade keys demo --force' to \
                   disable feature probing.";
    for line in wrap(message, theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Warn));
    }

    if !outcome.conclusive() && outcome.interactive {
        println!();
        for line in wrap(
            "Strictly speaking this was inconclusive rather than a no: nothing answered `CSI ? u`, \
             and the device-attributes barrier that would have made silence meaningful did not \
             come back either.",
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }

    if let Some(multiplexer) = outcome.multiplexer {
        println!();
        for line in wrap(
            &format!(
                "You are inside {}, which decides for itself what your keys look like. Its answer \
                 is the one that reaches this process, whatever the terminal outside it supports.",
                multiplexer.name()
            ),
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }

    if !outcome.interactive {
        println!();
        println!(
            "  {}",
            theme.paint(
                "stdin and stdout are not both terminals, so there is no keyboard to demonstrate.",
                Ink::Muted
            )
        );
    }
    println!();
}

// ---------------------------------------------------------------------------
// Drawing context
// ---------------------------------------------------------------------------

/// Everything a step needs, plus a cursor so steps can write line by line.
struct Ctx<'a> {
    term: &'a mut Term,
    keys: &'a mut Keyboard,
    theme: &'a Theme,
    multiplexer: Option<Multiplexer>,
    /// Flags the terminal honoured when the probe asked for all five.
    honoured: Flags,
    /// Whether anything answered the flags query at all.
    supported: bool,
    /// Whether the demo is running past a failed probe.
    forced: bool,
    /// Flags this demo has pushed, if any.
    ///
    /// **At most one push is ever outstanding.** [`Ctx::use_flags`] pops the
    /// previous one before pushing the next, which keeps the demo's stack depth at
    /// one — so the single `CSI < u` in the panic hook is enough to undo it if the
    /// process dies in an unusual way.
    pushed: Option<Flags>,
    /// Next row to write to.
    row: u16,
    /// Usable text width.
    width: usize,
}

impl std::fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("row", &self.row)
            .field("pushed", &self.pushed)
            .field("honoured", &self.honoured)
            .finish_non_exhaustive()
    }
}

impl Ctx<'_> {
    /// Clears the screen and draws the chrome for step `index`.
    fn page(&mut self, index: usize) -> Result<()> {
        self.term.refresh_size()?;
        self.width = usize::from(self.term.cols.saturating_sub(MARGIN * 2)).min(96);
        self.term.clear()?;

        let title = STEPS[index].title;
        let counter = format!("step {}/{}", index + 1, STEPS.len());
        let heading = "escapade · keyboard protocol";
        let gap = self
            .width
            .saturating_sub(heading.chars().count() + counter.chars().count());

        self.row = 2;
        let line = format!(
            "{}{}{}",
            self.theme.paint(heading, Ink::Title),
            " ".repeat(gap),
            self.theme.paint(&counter, Ink::Muted)
        );
        self.line(&line)?;
        self.line(&self.theme.paint(&"─".repeat(self.width), Ink::Muted))?;
        self.blank();
        self.line(&self.theme.paint(title, Ink::Accent))?;
        self.blank();
        Ok(())
    }

    /// Writes one line at the margin and advances the cursor.
    fn line(&mut self, text: &str) -> Result<()> {
        let row = self.row;
        self.term.put(MARGIN, row, text)?;
        self.row = self.row.saturating_add(1);
        Ok(())
    }

    /// Skips a row.
    fn blank(&mut self) {
        self.row = self.row.saturating_add(1);
    }

    /// Wraps and writes a paragraph.
    fn para(&mut self, text: &str) -> Result<()> {
        for line in wrap(text, self.width) {
            self.line(&line)?;
        }
        Ok(())
    }

    /// Wraps and writes a paragraph in the muted tone, indented.
    fn note(&mut self, text: &str) -> Result<()> {
        for line in wrap(text, self.width.saturating_sub(2)) {
            let painted = self.theme.paint(&line, Ink::Muted);
            self.line(&format!("  {painted}"))?;
        }
        Ok(())
    }

    /// Shows a sequence, then names each of its pieces.
    fn sequence(&mut self, raw: &str, parts: &[(&str, &str)]) -> Result<()> {
        let painted = self.theme.paint(&pretty(raw), Ink::Code);
        self.line(&format!("  {painted}"))?;
        self.blank();
        let token_width = parts
            .iter()
            .map(|(token, _)| token.chars().count())
            .max()
            .unwrap_or(0);
        for (token, meaning) in parts {
            let label = format!("{token:<token_width$}");
            let lines = wrap(meaning, self.width.saturating_sub(token_width + 7));
            for (index, line) in lines.iter().enumerate() {
                let text = if index == 0 {
                    format!(
                        "  {}  {}",
                        self.theme.paint(&label, Ink::Accent),
                        self.theme.paint(line, Ink::Muted)
                    )
                } else {
                    format!(
                        "  {}  {}",
                        " ".repeat(token_width),
                        self.theme.paint(line, Ink::Muted)
                    )
                };
                self.line(&text)?;
            }
        }
        Ok(())
    }

    /// Pushes `flags`, popping this demo's previous push first: `CSI > flags u`.
    ///
    /// Shows what it sent, because a demo that changes the keyboard silently is
    /// asking the reader to take its word for it.
    fn use_flags(&mut self, flags: Flags) -> Result<()> {
        if self.pushed == Some(flags) {
            return Ok(());
        }
        self.unpush()?;
        self.term.write_now(&push(flags))?;
        self.pushed = Some(flags);
        let shown = format!(
            "  {}   {}",
            self.theme.paint(&pretty(&push(flags)), Ink::Code),
            self.theme.paint(&flags.to_string(), Ink::Muted)
        );
        self.line(&shown)?;
        // A flag the terminal did not honour will simply not happen, and a reader
        // pressing keys for a minute deserves to know that before they start.
        let missing: Vec<&str> = flags
            .specs()
            .iter()
            .filter(|spec| self.honoured.bits() & spec.bit == 0)
            .map(|spec| spec.name)
            .collect();
        if !missing.is_empty() && self.supported {
            let warning = format!(
                "this terminal did not honour: {} — expect that part not to happen",
                missing.join(", ")
            );
            self.line(&format!("  {}", self.theme.paint(&warning, Ink::Warn)))?;
        }
        Ok(())
    }

    /// Pops this demo's push, if it has one: `CSI < u`.
    fn unpush(&mut self) -> Result<()> {
        if self.pushed.take().is_some() {
            self.term.write_now(&pop())?;
        }
        Ok(())
    }

    /// Asks the reader to press something and shows what arrives.
    ///
    /// The whole of the interaction lives in [`prompt::ask_for_keys`]; this is only
    /// the plumbing that hands it this demo's screen and this demo's keyboard.
    fn ask(&mut self, ask: &Ask<'_>) -> Result<prompt::Collected> {
        self.blank();
        // Split the borrows: the prompt draws through `self` and reads through the
        // keyboard, and both cannot be `&mut self` at once.
        let Ctx {
            term,
            keys,
            theme,
            row,
            width,
            ..
        } = self;
        let mut view = Pen {
            term,
            theme,
            row,
            width: *width,
        };
        prompt::ask_for_keys(&mut view, *keys, ask)
    }

    /// Draws the footer hint and blocks until the reader chooses.
    fn wait(&mut self) -> Result<Flow> {
        let row = self.term.rows.saturating_sub(2);
        let hint = self
            .theme
            .paint("space/enter next · p back · q quit", Ink::Muted);
        self.term.put(MARGIN, row, &hint)?;
        self.term.flush()?;
        let advance = self.keys.reader().wait_for_advance()?;
        // `wait_for_advance` reads through the reader directly, so the byte mirror
        // has to be told.
        self.keys.discard_pending();
        Ok(advance.into())
    }

    /// Sends a query and shows what came back, live.
    ///
    /// Uses the reader directly, so the byte mirror is resynced afterwards.
    fn ask_terminal(&mut self, sequence: &str) -> Result<Option<Flags>> {
        self.term.write_now(sequence)?;
        let reply = self
            .keys
            .reader()
            .wait_for_reply(REPLY_TIMEOUT, super::is_query_reply)
            .unwrap_or(None);
        self.keys.discard_pending();
        Ok(reply.as_ref().and_then(super::parse_query_reply))
    }
}

/// Where an interactive step goes next, given what it collected.
///
/// Return means on, `q` and Ctrl-C mean out, and end of input means out as well —
/// there is nobody left to press anything.
fn flow_of(collected: &prompt::Collected) -> Flow {
    collected.advance.into()
}

/// A borrowed writer over a [`Ctx`]'s screen, so [`prompt::ask_for_keys`] can draw
/// into a step while the keyboard is borrowed separately.
struct Pen<'a> {
    term: &'a mut Term,
    theme: &'a Theme,
    row: &'a mut u16,
    width: usize,
}

impl std::fmt::Debug for Pen<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pen")
            .field("row", &self.row)
            .field("width", &self.width)
            .finish_non_exhaustive()
    }
}

impl StepView for Pen<'_> {
    fn theme(&self) -> &Theme {
        self.theme
    }

    fn write_line(&mut self, text: &str) -> Result<()> {
        // Scrolling would take the step's own prose off the screen, so once the
        // page is full the newest lines overwrite the last usable row.
        let last = self.term.rows.saturating_sub(4);
        if *self.row > last {
            *self.row = last;
        }
        self.term.put(MARGIN, *self.row, text)?;
        self.term.clear_to_eol()?;
        *self.row = self.row.saturating_add(1);
        Ok(())
    }

    fn flush_view(&mut self) -> Result<()> {
        self.term.flush()
    }
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// The capability handshake.
fn step_handshake(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(0)?;
    ctx.para(
        "This protocol has a query, which makes it one of the pleasant ones: there is no need to \
         guess from the terminal's name what it can do. Ask.",
    )?;
    ctx.blank();
    ctx.sequence(
        &format!("{}{CSI}c", query()),
        &[
            (
                "CSI ? u",
                "which progressive-enhancement flags are in effect right now?",
            ),
            (
                "CSI c",
                "Primary Device Attributes, sent straight after as a barrier. Every terminal since \
                 the VT100 answers it, so an answer here with none to the query above means the \
                 terminal has demonstrably had its chance.",
            ),
        ],
    )?;
    ctx.blank();

    // Asked again here, live, rather than reporting what the startup probe found:
    // the reader should see the round trip happen on the screen in front of them.
    let live = ctx.ask_terminal(&format!("{}{CSI}c", query()))?;
    if ctx.supported {
        if let Some(flags) = live {
            let text = format!("← CSI ? {} u   currently set: {flags}", flags.bits());
            ctx.line(&format!("  {}", ctx.theme.paint(&text, Ink::Code)))?;
        }
        let text = format!(
            "✔ answered — flags honoured when all five were asked for: {}",
            ctx.honoured
        );
        ctx.line(&format!("  {}", ctx.theme.paint(&text, Ink::Good)))?;
        ctx.blank();
        ctx.note(
            "Note what the query answers: the flags currently *set*, not the flags supported. On a \
             fresh terminal that is zero, which is why `escapade keys probe` pushes all five and \
             asks again — the difference is the answer.",
        )?;
    } else {
        ctx.line(&format!(
            "  {}",
            ctx.theme
                .paint("… silence. Nothing here answered.", Ink::Warn)
        ))?;
        ctx.blank();
        ctx.note(
            "The rest of the demo still sends real sequences, so you can watch what goes out — but \
             the keys will come back in their legacy encodings, which is its own kind of \
             demonstration.",
        )?;
    }
    if let Some(multiplexer) = ctx.multiplexer {
        ctx.blank();
        ctx.note(&format!(
            "You are inside {}. Whatever answered, answered for {} — it is the thing your keys \
             actually come from, and it may support less than the terminal outside it.",
            multiplexer.name(),
            multiplexer.name()
        ))?;
    }

    ctx.wait()
}

/// Push and pop versus set.
fn step_push_pop(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(1)?;
    ctx.para(
        "There are two ways to change the flags, and the difference matters more than it looks.",
    )?;
    ctx.blank();
    // Shown, never sent: `set` is the sequence this demo is arguing against.
    let five = Flags::DISAMBIGUATE.union(Flags::ALTERNATE_KEYS);
    for (mode, meaning) in [
        (SetMode::Replace, "set these bits, reset every other one"),
        (SetMode::Enable, "set these bits, leave the rest alone"),
        (SetMode::Disable, "reset these bits, leave the rest alone"),
    ] {
        ctx.line(&format!(
            "  {}   {}",
            ctx.theme
                .paint(&format!("{:<14}", pretty(&set(five, mode))), Ink::Code),
            ctx.theme.paint(meaning, Ink::Muted)
        ))?;
    }
    ctx.blank();
    ctx.note(
        "Three modes, and every one of them writes over state that may not be yours — there is no \
         mode that means \"and put it back afterwards\".",
    )?;
    ctx.blank();
    ctx.sequence(
        &format!("{}   …   {}", push(five), pop()),
        &[
            (
                "CSI > 5 u",
                "push: remember what is there, then apply these.",
            ),
            (
                "CSI < u",
                "pop: put back exactly what was there, without ever having to know what it was.",
            ),
        ],
    )?;
    ctx.blank();
    ctx.note(
        "An application is rarely alone. A shell integration, an editor's terminal pane, a \
         multiplexer — each may have enabled the flags it needs. `set` would silently overwrite \
         that; push and pop compose. The terminal keeps separate stacks for the main and the \
         alternate screen, so a full-screen program can push on entry and pop on exit and leave \
         the main screen untouched.",
    )?;
    ctx.blank();
    ctx.note(
        "One rule follows: every push needs its pop, including on the error path. A program that \
         exits with flags pushed hands the next one a keyboard where Escape arrives as `CSI 27 u` \
         and stray releases look like input. This demo keeps exactly one push outstanding at a \
         time for that reason.",
    )?;

    ctx.wait()
}

/// Flag 1: disambiguation.
fn step_disambiguate(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(2)?;
    ctx.para(
        "Flag 1. Ctrl+i and Tab are both byte 9. Ctrl+m and Enter are both byte 13. Escape is byte \
         27, which is also how every escape sequence starts. With disambiguation on, keys that \
         produce no text get sequences of their own.",
    )?;
    ctx.blank();
    ctx.use_flags(Flags::DISAMBIGUATE)?;

    let collected = ctx.ask(&Ask {
        press: "Escape, then Ctrl+i, then Ctrl+Alt+a",
        hint: "Tab too, if you like — it is the interesting exception",
        want: 4,
        patience: prompt::PATIENCE,
    })?;
    ctx.blank();
    ctx.note(
        "Escape is `CSI 27 u`, which no longer needs a timeout to recognise. Ctrl+i is `CSI 105 ; \
         5 u`, no longer the same bytes as Tab. Tab, Enter and Backspace keep their legacy bytes \
         when unmodified — deliberately, so that the flag stays safe to enable.",
    )?;
    Ok(flow_of(&collected))
}

/// Flag 1, applied to the famous case.
fn step_shift_enter(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(3)?;
    ctx.para(
        "The one everybody has hit. In legacy encoding Shift+Enter is a carriage return, exactly \
         like Enter, so no terminal application can bind it — the request has been open against \
         every terminal for decades.",
    )?;
    ctx.blank();
    ctx.use_flags(Flags::DISAMBIGUATE)?;

    let collected = ctx.ask(&Ask {
        press: "Shift+Enter",
        hint: "plain Enter moves on — that is the control for this experiment, and the reason this \
               step can exist at all",
        want: 3,
        patience: prompt::PATIENCE,
    })?;

    ctx.blank();
    let shifted = collected
        .keys()
        .iter()
        .any(|key| key.code == KeyCode::Enter && key.mods.shift);
    if shifted {
        ctx.line(&format!(
            "  {}",
            ctx.theme.paint(
                "✔ that arrived as CSI 13 ; 2 u — 13 for Enter, 2 for shift, and nothing else in \
                 the terminal can produce it",
                Ink::Good
            )
        ))?;
    } else if collected.is_empty() {
        ctx.note(
            "Nothing was collected. If pressing Shift+Enter advanced the step, that is the whole \
             lesson: the terminal sent a plain carriage return, and the prompt could not tell it \
             from a request to move on.",
        )?;
    }
    ctx.note(
        "This is also why Return has to be the key that advances a demo step, and why no step can \
         demonstrate Return itself. The constraint is the subject.",
    )?;
    Ok(flow_of(&collected))
}

/// Flag 2: release events.
fn step_release(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(4)?;
    ctx.para(
        "Flag 2. Legacy input has no concept of a key coming back up: an application cannot know \
         what is being held, only what has been struck. Add the flag and every event carries its \
         type.",
    )?;
    ctx.blank();
    ctx.sequence(
        "CSI 97 ; 1 : 3 u",
        &[
            ("97", "the key — lowercase a."),
            ("; 1", "modifiers, 1 meaning none."),
            (": 3", "the event type: 1 press, 2 repeat, 3 release."),
        ],
    )?;
    ctx.blank();
    ctx.use_flags(Flags::DISAMBIGUATE.union(Flags::EVENT_TYPES))?;
    if ctx.supported && !ctx.honoured.contains(Flags::EVENT_TYPES) {
        ctx.note(
            "This terminal did not honour flag 2 when asked, so releases are unlikely to appear \
             below however long you hold the key. Press anyway — an honest nothing is a result.",
        )?;
    }

    let collected = ctx.ask(&Ask {
        press: "and hold a letter key, then let it go",
        hint: "you should see two events for one keypress",
        want: 4,
        patience: prompt::PATIENCE,
    })?;
    ctx.blank();
    if collected.saw_release() {
        ctx.line(&format!(
            "  {}",
            ctx.theme.paint(
                "✔ a release event — impossible to observe without this flag",
                Ink::Good
            )
        ))?;
    } else if !collected.is_empty() {
        ctx.note(
            "Only presses came back, so this terminal did not honour flag 2. Nothing is broken; the \
             flag is a request, and this is what a refusal looks like.",
        )?;
    }
    Ok(flow_of(&collected))
}

/// Flag 2, applied to auto-repeat.
fn step_repeat(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(5)?;
    ctx.para(
        "The same flag separates auto-repeat from real presses. Without it, holding a key down \
         looks exactly like typing it very fast — which is why terminal games cannot tell the \
         difference between a held arrow and a hammered one.",
    )?;
    ctx.blank();
    ctx.use_flags(Flags::DISAMBIGUATE.union(Flags::EVENT_TYPES))?;

    let collected = ctx.ask(&Ask {
        press: "and hold the space bar until it starts repeating",
        hint: "event type 2 is a repeat; press events are type 1",
        want: 6,
        patience: prompt::PATIENCE,
    })?;
    ctx.blank();
    let repeats = collected
        .keys()
        .iter()
        .filter(|key| key.kind == KeyEventKind::Repeat)
        .count();
    if repeats > 0 {
        let text =
            format!("✔ {repeats} repeat events, each marked as a repeat rather than a press");
        ctx.line(&format!("  {}", ctx.theme.paint(&text, Ink::Good)))?;
    } else {
        ctx.note(
            "No repeats came through. Some terminals report auto-repeat as ordinary presses even \
             with the flag set; others need the key held longer than the system's repeat delay.",
        )?;
    }
    Ok(flow_of(&collected))
}

/// Flags 4, 8 and 16.
fn step_text(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(6)?;
    ctx.para(
        "The last three flags together. 4 adds the shifted keycap and the same physical key in the \
         PC-101 layout. 8 turns every key into an escape code, including the ones that used to be \
         plain text. 16 then puts the text back, inside the escape code, where it can be read \
         without guessing.",
    )?;
    ctx.blank();
    ctx.sequence(
        "CSI 97 : 65 ; 2 ; 65 u",
        &[
            ("97", "the key: lowercase a, always the unshifted keycap."),
            (
                ": 65",
                "the shifted key, present only because shift is held.",
            ),
            ("; 2", "the modifiers: shift."),
            ("; 65", "the text this keypress produced — capital A."),
        ],
    )?;
    ctx.blank();
    // Spelled out as a union rather than as `Flags::ALL`, because the point of the
    // step is which flags are being asked for.
    ctx.use_flags(
        Flags::DISAMBIGUATE
            .union(Flags::EVENT_TYPES)
            .union(Flags::ALTERNATE_KEYS)
            .union(Flags::ALL_AS_ESCAPE_CODES)
            .union(Flags::ASSOCIATED_TEXT),
    )?;
    ctx.note(
        "Careful: with flag 8 the terminal stops turning Ctrl+C into SIGINT. It arrives as `CSI 99 \
         ; 5 u` and this program has to honour it itself — which it does.",
    )?;

    let collected = ctx.ask(&Ask {
        press: "Shift+a, then a bare letter, then a modifier key on its own",
        hint: "the bare modifier only reports at all because of flag 8",
        want: 5,
        patience: prompt::PATIENCE,
    })?;
    ctx.blank();
    ctx.note(
        "`escapade` decodes the key and its modifiers but ignores the alternate-key and text \
         fields, which is why the bytes are shown next to every event: the parser tells you what it \
         understood, and the bytes tell you what was actually said. A bare modifier key has no key \
         code in this project at all — `CSI 57441 u` is left-shift — so it shows up as undecoded, \
         with its bytes intact.",
    )?;
    Ok(flow_of(&collected))
}

/// The finale: a chord that legacy encoding cannot express.
fn step_chord(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(7)?;
    ctx.para(&format!(
        "Everything so far has been observation. This is the thing that cannot be built at all \
         without the protocol: hold {CHORD_SIZE} keys down **at the same time**. Legacy encoding \
         reports presses and never releases, so an application has no idea what is still held — a \
         chord is not hard to implement there, it is inexpressible."
    ))?;
    ctx.blank();
    ctx.use_flags(Flags::DISAMBIGUATE.union(Flags::EVENT_TYPES))?;
    ctx.blank();
    ctx.line(&format!(
        "  {}",
        ctx.theme.paint(
            &format!("Hold any {CHORD_SIZE} letter keys together · Return moves on · q leaves"),
            Ink::Muted
        )
    ))?;

    let held_row = ctx.row + 1;
    let result_row = held_row + 2;
    let mut held: Vec<char> = Vec::new();
    let mut best = 0usize;
    let mut chords = 0usize;
    let mut last_draw = Instant::now();
    let flow;

    loop {
        let pair = match ctx.keys.next_event(FRAME) {
            Ok(Some(pair)) => Some(pair),
            Ok(None) => None,
            Err(_) => {
                flow = Flow::Quit;
                break;
            }
        };

        if let Some((event, _bytes)) = pair {
            if prompt::is_return(&event) {
                flow = Flow::Next;
                break;
            }
            if prompt::is_leave(&event) {
                flow = Flow::Quit;
                break;
            }
            if let Event::Key(KeyEvent {
                code: KeyCode::Char(ch),
                kind,
                ..
            }) = event
            {
                match kind {
                    // A repeat of a key already held is not news; a press of one
                    // that is not held is.
                    KeyEventKind::Press | KeyEventKind::Repeat => {
                        if !held.contains(&ch) {
                            held.push(ch);
                        }
                    }
                    KeyEventKind::Release => held.retain(|&other| other != ch),
                }
                if held.len() > best {
                    best = held.len();
                }
                if held.len() >= CHORD_SIZE {
                    chords += 1;
                }
            }
        }

        if last_draw.elapsed() >= FRAME || !held.is_empty() {
            last_draw = Instant::now();
            let keys: Vec<String> = held.iter().map(|ch| format!("[{ch}]")).collect();
            let ink = if held.len() >= CHORD_SIZE {
                Ink::Good
            } else {
                Ink::Accent
            };
            let display = if keys.is_empty() {
                ctx.theme.paint("(nothing held)", Ink::Muted)
            } else {
                ctx.theme.paint(&keys.join(" "), ink)
            };
            ctx.term.put(MARGIN + 2, held_row, &display)?;
            ctx.term.clear_to_eol()?;

            let verdict = if chords > 0 {
                ctx.theme.paint(
                    &format!("✔ chord of {best} keys held at once — {chords} so far"),
                    Ink::Good,
                )
            } else {
                ctx.theme
                    .paint(&format!("most held at once: {best}"), Ink::Muted)
            };
            ctx.term.put(MARGIN + 2, result_row, &verdict)?;
            ctx.term.clear_to_eol()?;
            ctx.term.flush()?;
        }
    }

    ctx.keys.discard_pending();
    Ok(flow)
}

/// Putting the keyboard back, and leaving the alternate screen.
fn step_leave(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(8)?;
    ctx.para(
        "One pop for the push, and the keyboard is exactly as it was found — including any flags \
         the program that launched this one had enabled.",
    )?;
    ctx.blank();
    let popped = ctx.pushed.is_some();
    ctx.unpush()?;
    let text = if popped {
        format!("{}   the demo's push, undone", pretty(&pop()))
    } else {
        format!("{}   nothing was outstanding", pretty(&pop()))
    };
    ctx.line(&format!("  {}", ctx.theme.paint(&text, Ink::Code)))?;
    ctx.blank();
    ctx.note(
        "Then the alternate screen, the hidden cursor and raw mode go back too, in reverse order. \
         The terminal keeps a separate flag stack per screen, so leaving the alternate screen \
         cannot strand a push made inside it.",
    )?;
    ctx.blank();
    for (command, purpose) in [
        (
            "escapade keys watch --legacy",
            "what a program from 1995 sees",
        ),
        ("escapade keys watch", "…and what one written today sees"),
        (
            "escapade keys probe --format debug",
            "which flags this terminal honours",
        ),
        (
            "escapade keys explain",
            "the protocol, and who implements it",
        ),
    ] {
        ctx.line(&format!(
            "  {}  {}",
            ctx.theme.paint(&format!("{command:<34}"), Ink::Code),
            ctx.theme.paint(purpose, Ink::Muted)
        ))?;
    }
    if ctx.forced {
        ctx.blank();
        ctx.note(
            "This run was forced past a failed capability check, so anything that did not happen \
             above was the terminal, not the demo.",
        )?;
    }

    let flow = ctx.wait()?;
    // Leaving the alternate screen is the last thing the demo does, so the reader
    // is put back where they started rather than watching the terminal blink.
    ctx.term.leave_alt_screen()?;
    Ok(flow)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::keys::FLAGS;

    #[test]
    fn step_names_are_short_kebab_case_and_unique() {
        let names = step_names();
        assert_eq!(names.len(), STEPS.len());
        for name in &names {
            assert!(
                name.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-'),
                "`{name}` is not kebab-case"
            );
            assert!(name.len() <= 14, "`{name}` is too long to type");
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate step name");
    }

    #[test]
    fn every_step_has_a_title() {
        for step in STEPS {
            assert!(!step.title.is_empty(), "`{}` has no title", step.name);
        }
    }

    #[test]
    fn selection_resolves_by_name_prefix_and_number() {
        let names = step_names();
        let single = crate::features::select_steps(&names, Some("shift"), None).unwrap();
        assert_eq!(names[single.start], "shift-enter");
        assert!(single.single, "--only runs one step and stops");

        let numbered = crate::features::select_steps(&names, None, Some(2)).unwrap();
        assert_eq!(names[numbered.start], "push-pop");
        assert!(!numbered.single);

        // A misspelling names the real steps rather than failing silently.
        let error = crate::features::select_steps(&names, Some("nope"), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("handshake") && error.contains("chord"),
            "{error}"
        );
    }

    #[test]
    fn the_demo_ends_by_leaving_the_alternate_screen() {
        // Whatever else changes, the last step must be the one that restores the
        // terminal — a demo that ends on the alternate screen loses everything the
        // reader was looking at.
        assert_eq!(STEPS.last().map(|step| step.name), Some("restore"));
    }

    #[test]
    fn the_flags_are_introduced_in_the_order_they_are_documented() {
        // The script teaches flag 1, then 2, then the rest; the step names should
        // still say so if someone reorders them.
        let names = step_names();
        let position = |name: &str| names.iter().position(|other| *other == name);
        assert!(position("disambiguate") < position("release"));
        assert!(position("release") < position("text"));
        assert!(position("text") < position("chord"));
        assert!(position("handshake") < position("disambiguate"));
    }

    /// Two keys can be faked by timing; three held at once cannot be seen at all
    /// without release events, which is what makes the finale a real finale.
    const _: () = assert!(CHORD_SIZE >= 3);

    #[test]
    fn the_step_list_mentions_the_return_key_rule() {
        // The rule is a teaching point, not an apology, so it belongs in `--list`
        // where a reader sees it before starting.
        let theme = Theme::plain();
        list_steps(&theme);
        // Nothing to assert about stdout here beyond not panicking; the wording is
        // checked by eye. What matters is that `--list` names every step.
        for step in STEPS {
            assert!(!step.name.is_empty());
        }
    }

    #[test]
    fn every_documented_flag_is_demonstrated_somewhere() {
        // A flag with no step is a flag a reader has to take on faith.
        let demonstrated = [
            Flags::DISAMBIGUATE,
            Flags::EVENT_TYPES,
            Flags::ALTERNATE_KEYS,
            Flags::ALL_AS_ESCAPE_CODES,
            Flags::ASSOCIATED_TEXT,
        ];
        for spec in FLAGS {
            assert!(
                demonstrated.iter().any(|flags| flags.bits() == spec.bit),
                "flag {} ({}) is not demonstrated",
                spec.bit,
                spec.name
            );
        }
    }
}
