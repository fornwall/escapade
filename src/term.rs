//! Terminal control: raw mode, the alternate screen, mouse reporting and the
//! handful of private modes the demos rely on.
//!
//! Sequences are written out explicitly rather than through a wrapper library.
//! That is the whole point of this project — the code that drives the terminal
//! should be the code a reader can copy — and it means the demo can show the
//! very bytes it is about to send. Only raw mode goes through [`crossterm`],
//! because doing it properly means `termios` on Unix and console handles on
//! Windows, and none of that is instructive.

use std::io::{Stdout, Write as _, stdout};

use anyhow::Result;
use crossterm::terminal;

/// The escape byte, as a string, for building sequences.
pub const ESC: &str = "\u{1b}";

/// Control Sequence Introducer: `ESC [`.
pub const CSI: &str = "\u{1b}[";

/// Application Program Command: `ESC _`. Glyph Protocol lives here.
pub const APC: &str = "\u{1b}_";

/// Operating System Command: `ESC ]`. Hyperlinks and notifications live here.
pub const OSC: &str = "\u{1b}]";

/// Device Control String: `ESC P`. tmux passthrough lives here.
pub const DCS: &str = "\u{1b}P";

/// String Terminator: `ESC \`.
pub const ST: &str = "\u{1b}\\";

/// Bell: `0x07`. xterm accepts it in place of [`ST`] to end an OSC string, and a
/// lot of software emits it, so anything parsing OSC has to handle both.
pub const BEL: &str = "\u{7}";

/// A multiplexer sitting between this process and the real terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Multiplexer {
    /// tmux, which filters APC unless `allow-passthrough` is on.
    Tmux,
    /// GNU Screen.
    Screen,
    /// Zellij.
    Zellij,
}

impl Multiplexer {
    /// Detects a multiplexer from the environment.
    #[must_use]
    pub fn detect() -> Option<Self> {
        if std::env::var_os("TMUX").is_some() {
            return Some(Self::Tmux);
        }
        if std::env::var_os("ZELLIJ").is_some() {
            return Some(Self::Zellij);
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.starts_with("screen") {
            return Some(Self::Screen);
        }
        if term.starts_with("tmux") {
            return Some(Self::Tmux);
        }
        None
    }

    /// Display name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Screen => "GNU Screen",
            Self::Zellij => "Zellij",
        }
    }

    /// What the user must do so that exotic sequences reach the real terminal.
    #[must_use]
    pub fn passthrough_hint(self) -> &'static str {
        match self {
            Self::Tmux => "set -g allow-passthrough on   # in ~/.tmux.conf, then reload",
            // Screen *does* have passthrough, contrary to a common belief: it
            // wants a bare `DCS <payload> ST` with no introducer and no escape
            // doubling, in chunks of at most 256 bytes. It is not offered here
            // because Screen also answers queries out of order, which defeats
            // the barrier the probe depends on.
            Self::Screen => {
                "GNU Screen answers queries out of order, so capability probing \
                 is unreliable inside it; run outside Screen"
            }
            Self::Zellij => "Zellij's APC forwarding is unverified; try running outside it",
        }
    }

    /// Wraps `sequence` so the multiplexer forwards it verbatim.
    ///
    /// tmux uses `DCS tmux; <payload> ST` with every `ESC` in the payload
    /// doubled. Other multiplexers have no equivalent, so the sequence is
    /// returned unchanged and will most likely be swallowed.
    #[must_use]
    pub fn wrap(self, sequence: &str) -> String {
        match self {
            Self::Tmux => {
                let escaped = sequence.replace(ESC, "\u{1b}\u{1b}");
                format!("{DCS}tmux;{escaped}{ST}")
            }
            Self::Screen | Self::Zellij => sequence.to_owned(),
        }
    }
}

/// Owns the terminal's mutable state and restores it on drop.
///
/// Every mode this switches on is tracked, so [`Term::restore`] puts things
/// back exactly as they were even if a demo exits early or panics.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag tracks one independent terminal mode we switched on, so \
              that `restore` can put back exactly what we changed; a bitflags \
              type would obscure which mode is which at the call sites"
)]
pub struct Term {
    out: Stdout,
    /// Terminal width in cells.
    pub cols: u16,
    /// Terminal height in cells.
    pub rows: u16,
    raw: bool,
    alt_screen: bool,
    mouse: bool,
    bracketed_paste: bool,
    kitty_keyboard: bool,
    cursor_hidden: bool,
}

impl Term {
    /// Captures the current size. Changes nothing yet.
    ///
    /// # Errors
    ///
    /// Fails if the terminal size cannot be determined.
    pub fn new() -> Result<Self> {
        let (cols, rows) = terminal::size()?;
        Ok(Self {
            out: stdout(),
            cols,
            rows,
            raw: false,
            alt_screen: false,
            mouse: false,
            bracketed_paste: false,
            kitty_keyboard: false,
            cursor_hidden: false,
        })
    }

    /// Re-reads the terminal size, after a resize.
    ///
    /// # Errors
    ///
    /// Fails if the terminal size cannot be determined.
    pub fn refresh_size(&mut self) -> Result<()> {
        let (cols, rows) = terminal::size()?;
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    /// Writes a string verbatim, without flushing.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn write(&mut self, text: &str) -> Result<()> {
        self.out.write_all(text.as_bytes())?;
        Ok(())
    }

    /// Flushes stdout.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }

    /// Writes a string and flushes immediately — for sequences whose reply we
    /// are about to wait for.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn write_now(&mut self, text: &str) -> Result<()> {
        self.write(text)?;
        self.flush()
    }

    /// Writes a query and flushes, with [`shielded`] around it so that a terminal
    /// which *prints* the sequence instead of ignoring it leaves nothing behind.
    ///
    /// Use this for every APC, DCS or OSC query. A `CSI` query needs no shield:
    /// the control-sequence grammar is universal, so a terminal that has never
    /// heard of the final byte still consumes the whole sequence.
    ///
    /// The shield restores the cursor and erases to the end of the line, so the
    /// caller must not want the query to move the cursor, and must not need what
    /// is to the right of it — see [`shielded`] for why that is the right trade.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn write_query(&mut self, query: &str) -> Result<()> {
        self.write_now(&shielded(query))
    }

    // -- modes ---------------------------------------------------------------

    /// Puts the terminal in raw mode: no line buffering, no echo, no signal
    /// translation, so every keypress arrives as bytes.
    ///
    /// # Errors
    ///
    /// Fails if the terminal cannot be reconfigured.
    pub fn enable_raw(&mut self) -> Result<()> {
        if !self.raw {
            terminal::enable_raw_mode()?;
            self.raw = true;
        }
        Ok(())
    }

    /// Restores cooked mode.
    ///
    /// # Errors
    ///
    /// Fails if the terminal cannot be reconfigured.
    pub fn disable_raw(&mut self) -> Result<()> {
        if self.raw {
            terminal::disable_raw_mode()?;
            self.raw = false;
        }
        Ok(())
    }

    /// Switches to the alternate screen: `CSI ? 1049 h`.
    ///
    /// Private mode 1049 saves the cursor, switches buffers and clears — the
    /// combination full-screen applications want, so scrollback survives.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn enter_alt_screen(&mut self) -> Result<()> {
        if !self.alt_screen {
            self.write_now(&format!("{CSI}?1049h"))?;
            self.alt_screen = true;
        }
        Ok(())
    }

    /// Returns to the main screen: `CSI ? 1049 l`.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn leave_alt_screen(&mut self) -> Result<()> {
        if self.alt_screen {
            self.write_now(&format!("{CSI}?1049l"))?;
            self.alt_screen = false;
        }
        Ok(())
    }

    /// Enables mouse reporting: button events, drag motion, and SGR-encoded
    /// coordinates.
    ///
    /// `CSI ? 1000 h` reports presses and releases, `CSI ? 1002 h` adds motion
    /// while a button is held, and `CSI ? 1006 h` switches to SGR encoding so
    /// coordinates past column 223 still work.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn enable_mouse(&mut self) -> Result<()> {
        if !self.mouse {
            self.write_now(&format!("{CSI}?1000h{CSI}?1002h{CSI}?1006h"))?;
            self.mouse = true;
        }
        Ok(())
    }

    /// Disables mouse reporting, in the reverse order it was enabled.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn disable_mouse(&mut self) -> Result<()> {
        if self.mouse {
            self.write_now(&format!("{CSI}?1006l{CSI}?1002l{CSI}?1000l"))?;
            self.mouse = false;
        }
        Ok(())
    }

    /// Enables bracketed paste: `CSI ? 2004 h`, so pasted text arrives fenced
    /// by `CSI 200~` and `CSI 201~` instead of masquerading as typing.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn enable_bracketed_paste(&mut self) -> Result<()> {
        if !self.bracketed_paste {
            self.write_now(&format!("{CSI}?2004h"))?;
            self.bracketed_paste = true;
        }
        Ok(())
    }

    /// Disables bracketed paste.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn disable_bracketed_paste(&mut self) -> Result<()> {
        if self.bracketed_paste {
            self.write_now(&format!("{CSI}?2004l"))?;
            self.bracketed_paste = false;
        }
        Ok(())
    }

    /// Pushes kitty keyboard protocol flags: `CSI > <flags> u`.
    ///
    /// Flags are a bitmask — 1 disambiguate escape codes, 2 report event types
    /// (this is how key *release* becomes visible), 4 report alternate keys,
    /// 8 report all keys as escape codes, 16 report associated text. Pushing
    /// rather than setting means [`Term::pop_kitty_keyboard`] restores whatever
    /// the outer application had configured.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn push_kitty_keyboard(&mut self, flags: u8) -> Result<()> {
        if !self.kitty_keyboard {
            self.write_now(&format!("{CSI}>{flags}u"))?;
            self.kitty_keyboard = true;
        }
        Ok(())
    }

    /// Pops kitty keyboard protocol flags: `CSI < u`.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn pop_kitty_keyboard(&mut self) -> Result<()> {
        if self.kitty_keyboard {
            self.write_now(&format!("{CSI}<u"))?;
            self.kitty_keyboard = false;
        }
        Ok(())
    }

    /// Begins synchronized output: `CSI ? 2026 h`.
    ///
    /// The terminal buffers everything until the matching end, then presents it
    /// in one frame. This is what makes animation tear-free without
    /// double-buffering tricks; terminals that do not know mode 2026 ignore it.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn begin_sync(&mut self) -> Result<()> {
        self.write(&format!("{CSI}?2026h"))
    }

    /// Ends synchronized output: `CSI ? 2026 l`, and flushes the frame.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn end_sync(&mut self) -> Result<()> {
        self.write(&format!("{CSI}?2026l"))?;
        self.flush()
    }

    // -- drawing -------------------------------------------------------------

    /// Hides the cursor: `CSI ? 25 l`.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn hide_cursor(&mut self) -> Result<()> {
        if !self.cursor_hidden {
            self.write(&format!("{CSI}?25l"))?;
            self.cursor_hidden = true;
        }
        Ok(())
    }

    /// Shows the cursor: `CSI ? 25 h`.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn show_cursor(&mut self) -> Result<()> {
        if self.cursor_hidden {
            self.write(&format!("{CSI}?25h"))?;
            self.cursor_hidden = false;
        }
        Ok(())
    }

    /// Clears the screen and homes the cursor: `CSI 2 J` then `CSI H`.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn clear(&mut self) -> Result<()> {
        self.write(&format!("{CSI}2J{CSI}H"))
    }

    /// Moves the cursor, using 1-based coordinates as `CSI row ; col H` does.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn move_to(&mut self, col: u16, row: u16) -> Result<()> {
        self.write(&format!("{CSI}{row};{col}H"))
    }

    /// Clears from the cursor to the end of the line: `CSI K`.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn clear_to_eol(&mut self) -> Result<()> {
        self.write(&format!("{CSI}K"))
    }

    /// Writes `text` at a position, in one call.
    ///
    /// # Errors
    ///
    /// Fails if stdout does.
    pub fn put(&mut self, col: u16, row: u16, text: &str) -> Result<()> {
        self.move_to(col, row)?;
        self.write(text)
    }

    /// Puts the terminal back the way it was found. Idempotent, and safe to
    /// call from a panic path.
    pub fn restore(&mut self) {
        // Best-effort: if one step fails there is nothing useful to do but try
        // the rest, so the terminal is left as close to sane as possible.
        let _ = self.pop_kitty_keyboard();
        let _ = self.disable_mouse();
        let _ = self.disable_bracketed_paste();
        let _ = self.show_cursor();
        let _ = self.leave_alt_screen();
        let _ = self.disable_raw();
        let _ = self.flush();
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Installs a panic hook that restores the terminal before the message prints.
///
/// Without this, a panic inside raw mode on the alternate screen leaves the
/// user with an invisible cursor and no echo — and the backtrace scribbled over
/// the demo.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = stdout();
        let _ = terminal::disable_raw_mode();
        let _ = write!(
            out,
            "{CSI}<u{CSI}?1006l{CSI}?1002l{CSI}?1000l{CSI}?2004l{CSI}?25h{CSI}?1049l"
        );
        let _ = out.flush();
        previous(info);
    }));
}

/// A sequence as it goes on the wire: wrapped for a multiplexer if one is in the
/// way, unchanged if not.
///
/// The `Option` is what every caller actually has, [`Multiplexer::detect`] having
/// answered "none" for the ordinary case.
#[must_use]
pub fn wrapped(multiplexer: Option<Multiplexer>, sequence: &str) -> String {
    match multiplexer {
        Some(multiplexer) => multiplexer.wrap(sequence),
        None => sequence.to_owned(),
    }
}

/// Wraps a query so that a terminal which prints it rather than ignoring it
/// leaves nothing on the screen.
///
/// ```text
/// ESC 7   CSI ? 7 l   <query>   ESC 8   CSI K   CSI ? 7 h
/// ```
///
/// ECMA-48 says a control string a terminal does not implement — APC, DCS or OSC
/// — is consumed and discarded. Not every terminal does that. macOS Terminal.app
/// swallows the two-byte `ESC _` introducer and then prints the body as ordinary
/// text, so `escapade graphics probe`, whose whole contract is to be silent and
/// answer through its exit status, wrote `Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA` across
/// the user's prompt. The leak starts at the first byte *after* the introducer,
/// which is what identifies the fault. It is not one terminal's bug to
/// special-case: any terminal can do this with any string sequence it has no
/// state for, and the ones being probed are by definition the ones that do not
/// implement the sequence.
///
/// Each of the four sequences around the query is load-bearing:
///
/// * `ESC 7` — **DECSC**, save the cursor, so a leak that moved it is undone.
/// * `CSI ? 7 l` — **DECAWM reset**, autowrap off for the duration of the query.
///   This is what makes the rest correct. With autowrap on, a leak longer than
///   the space left on the line continues onto the next one, and a leak near the
///   bottom of the screen **scrolls** — which restoring the cursor cannot undo,
///   because every row has moved and the top one is gone. With autowrap off the
///   terminal holds the cursor at the last column and overprints that one cell,
///   so however long the payload is, the damage is confined to the remainder of
///   a single line.
/// * `ESC 8` — **DECRC**, put the cursor back.
/// * `CSI K` — **EL**, erase from the cursor to the end of the line: exactly the
///   region a leak can have reached, now that it cannot wrap. `CSI J` would be
///   wrong — the leak cannot reach below the cursor, so erasing to the end of the
///   display would destroy the user's screen in order to repair one line.
/// * `CSI ? 7 h` — autowrap back on.
///
/// `CSI Ps X` (ECH) would erase the exact cell count rather than the rest of the
/// line, but the terminal this exists for cannot: `nsterm` has no `ech` in its
/// terminfo entry, while `sc`, `rc`, `el`, `smam` and `rmam` are all there. So EL
/// it is, and the cost is that the caller must have nothing to the right of the
/// cursor that it wants to keep. Every probe qualifies: they run before anything
/// is printed, from a shell prompt, and print their report only after the
/// terminal has been restored.
///
/// Everything goes out in one write, so a terminal processes the leak and its
/// repair in the same batch and never gets to present the state in between — and
/// nothing can fail part-way through and leave autowrap off.
///
/// Batching is therefore free: a round of a dozen queries takes one shield rather
/// than a dozen, because with autowrap off their leaks all land on the same line.
#[must_use]
pub fn shielded(query: &str) -> String {
    format!("{ESC}7{CSI}?7l{query}{ESC}8{CSI}K{CSI}?7h")
}

/// Renders a sequence for display, replacing control bytes with their names.
///
/// Printing a sequence that contains a real `ESC` would execute it instead of
/// showing it, so everything user-facing goes through here.
///
/// Terminator names are padded on both sides, so a sequence followed immediately
/// by text reads as `OSC 8 ; ; ST click me` rather than running together. Any
/// resulting trailing space is trimmed, so a string that ends in a terminator
/// does not gain one.
#[must_use]
pub fn pretty(sequence: &str) -> String {
    /// Appends a control name with a space on each side, without doubling a
    /// space that is already there. Both boundaries matter: without the leading
    /// one, `<text> OSC` runs together as `textOSC`; without the trailing one,
    /// `ST <text>` runs together as `STtext`.
    fn token(out: &mut String, name: &str) {
        if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push_str(name);
        out.push(' ');
    }

    use std::fmt::Write as _;

    let mut out = String::with_capacity(sequence.len() * 2);
    let mut chars = sequence.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    token(&mut out, "CSI");
                }
                Some('_') => {
                    chars.next();
                    token(&mut out, "APC");
                }
                Some('P') => {
                    chars.next();
                    token(&mut out, "DCS");
                }
                Some(']') => {
                    chars.next();
                    token(&mut out, "OSC");
                }
                Some('\\') => {
                    chars.next();
                    token(&mut out, "ST");
                }
                _ => token(&mut out, "ESC"),
            },
            '\u{7}' => token(&mut out, "BEL"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            // Every other control byte is escaped rather than passed through.
            // The whole promise of this function is that its output is safe to
            // print, and naming only the four controls it knows about broke that
            // promise for the rest — each caller then had to re-escape, and one
            // forgetting to would emit the very byte it was describing.
            other if other.is_control() => {
                let _ = write!(out, "\\x{:02x}", u32::from(other));
            }
            other => out.push(other),
        }
    }
    // A control name at the very end would otherwise leave a trailing space.
    out.trim_end().to_owned()
}

/// A pocket terminal, so that a test can watch what a sequence does to a screen.
///
/// It is not an emulator. It knows printable characters, `DECSC`/`DECRC`,
/// autowrap, `EL`, `ED`, and — the point of the exercise — two ways of treating a
/// control string: the conforming one, and the one macOS Terminal.app uses. That
/// is exactly enough to test [`shielded`], which cannot otherwise be tested at all
/// without a terminal, and there is no pty harness in this repository to borrow:
/// `doctor`'s "four terminals" are synthetic [`crate::features::doctor::session::Round`]
/// values, which model replies rather than a screen.
#[cfg(test)]
pub mod screen {
    /// What a terminal does with an APC, DCS or OSC string it does not implement.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Strings {
        /// ECMA-48 conformance: introducer, body and terminator are all consumed.
        Ignored,
        /// macOS Terminal.app: the two-byte introducer is consumed and the body is
        /// printed as text.
        Printed,
    }

    /// A grid, a cursor, and just enough of the state machine.
    #[derive(Debug, Clone)]
    pub struct Screen {
        cols: usize,
        rows: usize,
        cells: Vec<char>,
        row: usize,
        /// Zero-based, and equal to `cols` when a wrap is pending.
        col: usize,
        wrap: bool,
        saved: (usize, usize),
        /// How many rows were lost off the top — the damage a cursor restore
        /// cannot undo.
        scrolled: usize,
        strings: Strings,
    }

    impl Screen {
        /// A blank screen with autowrap on, as every terminal starts.
        #[must_use]
        pub fn new(cols: usize, rows: usize, strings: Strings) -> Self {
            Self {
                cols,
                rows,
                cells: vec![' '; cols * rows],
                row: 0,
                col: 0,
                wrap: true,
                saved: (0, 0),
                scrolled: 0,
                strings,
            }
        }

        /// Moves the cursor, zero-based.
        pub fn goto(&mut self, row: usize, col: usize) {
            self.row = row.min(self.rows - 1);
            self.col = col.min(self.cols);
        }

        /// The cursor, as `(row, col)`.
        #[must_use]
        pub fn cursor(&self) -> (usize, usize) {
            (self.row, self.col)
        }

        /// Whether autowrap is on, which the shield must leave as it found it.
        #[must_use]
        pub fn wrap(&self) -> bool {
            self.wrap
        }

        /// How many rows scrolled off the top.
        #[must_use]
        pub fn scrolled(&self) -> usize {
            self.scrolled
        }

        /// The whole screen, one string per row, trailing blanks kept.
        #[must_use]
        pub fn text(&self) -> Vec<String> {
            self.cells
                .chunks(self.cols)
                .map(|row| row.iter().collect())
                .collect()
        }

        /// Feeds bytes to the terminal.
        pub fn feed(&mut self, text: &str) {
            let mut chars = text.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch != '\u{1b}' {
                    self.print(ch);
                    continue;
                }
                match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let mut final_byte = '\0';
                        for next in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&next) {
                                final_byte = next;
                                break;
                            }
                            params.push(next);
                        }
                        self.csi(&params, final_byte);
                    }
                    // DECSC and DECRC. The pending-wrap column travels with them.
                    Some('7') => self.saved = (self.row, self.col),
                    Some('8') => {
                        self.row = self.saved.0;
                        self.col = self.saved.1;
                    }
                    // A control string. `Strings::Printed` falls out of the loop
                    // and the body is printed by the arm above, which is exactly
                    // the fault being modelled; the `ESC \` that ends it is
                    // consumed as an escape this pocket terminal has no use for.
                    Some('_' | 'P' | ']') if self.strings == Strings::Ignored => {
                        while let Some(next) = chars.next() {
                            if next == '\u{7}' {
                                break;
                            }
                            if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                                chars.next();
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        /// Prints one character, wrapping only if autowrap says so.
        fn print(&mut self, ch: char) {
            if self.col == self.cols {
                if self.wrap {
                    self.col = 0;
                    self.linefeed();
                } else {
                    // Autowrap off: the cursor stays put and the last cell of the
                    // line is overprinted, however much more text arrives.
                    self.col = self.cols - 1;
                }
            }
            let index = self.row * self.cols + self.col;
            self.cells[index] = ch;
            self.col += 1;
        }

        /// The handful of control sequences the shield and its alternatives use.
        fn csi(&mut self, params: &str, final_byte: char) {
            match (params, final_byte) {
                ("?7", 'h') => self.wrap = true,
                ("?7", 'l') => self.wrap = false,
                // EL: erase from the cursor to the end of the line.
                ("", 'K') => {
                    let col = self.col.min(self.cols - 1);
                    let start = self.row * self.cols + col;
                    let end = (self.row + 1) * self.cols;
                    self.cells[start..end].fill(' ');
                }
                // ED: erase from the cursor to the end of the display.
                ("", 'J') => {
                    let col = self.col.min(self.cols - 1);
                    let start = self.row * self.cols + col;
                    self.cells[start..].fill(' ');
                }
                _ => {}
            }
        }

        /// Moves down a row, scrolling at the bottom.
        fn linefeed(&mut self) {
            if self.row + 1 < self.rows {
                self.row += 1;
                return;
            }
            self.cells.drain(..self.cols);
            self.cells.extend(std::iter::repeat_n(' ', self.cols));
            self.scrolled += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        screen::{Screen, Strings},
        *,
    };

    /// The bytes `escapade graphics probe` sends, and the bytes the bug report
    /// showed on the screen: everything after the two-byte `ESC _` introducer.
    const KITTY_QUERY: &str = "\u{1b}_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\u{1b}\\";
    const KITTY_LEAK: &str = "Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA";

    /// A screen with something on it, so that erasing too much is visible.
    fn busy(cols: usize, rows: usize, strings: Strings) -> Screen {
        let mut screen = Screen::new(cols, rows, strings);
        for row in 0..rows {
            screen.goto(row, 0);
            let line: String = format!("row {row} ").chars().cycle().take(cols).collect();
            screen.feed(&line);
        }
        screen
    }

    #[test]
    fn shielded_names_its_sequences() {
        assert_eq!(
            pretty(&shielded("QUERY")),
            "ESC 7 CSI ?7lQUERY ESC 8 CSI K CSI ?7h"
        );
    }

    #[test]
    fn an_unshielded_query_lands_on_the_screen_of_a_terminal_that_prints_it() {
        // The bug, reproduced: Terminal.app eats `ESC _` and prints the rest.
        let mut screen = Screen::new(80, 24, Strings::Printed);
        screen.goto(23, 0);
        screen.feed(KITTY_QUERY);
        assert!(screen.text()[23].starts_with(KITTY_LEAK));
        assert_eq!(screen.cursor(), (23, KITTY_LEAK.len()));
    }

    #[test]
    fn a_shielded_query_leaves_no_trace_on_a_terminal_that_prints_it() {
        let mut screen = Screen::new(80, 24, Strings::Printed);
        screen.goto(23, 0);
        let before = screen.text();
        screen.feed(&shielded(KITTY_QUERY));
        assert_eq!(screen.text(), before, "the leak was not erased");
        assert_eq!(screen.cursor(), (23, 0), "the cursor moved");
        assert!(screen.wrap(), "autowrap was left off");
        assert_eq!(screen.scrolled(), 0);
    }

    #[test]
    fn on_a_conforming_terminal_at_a_fresh_line_the_shield_costs_nothing() {
        // Where a probe actually runs: a screen with scrollback above it and the
        // cursor at column 1 of the line the shell just opened. Nothing was
        // printed, so there is nothing to erase, and the shield invents no damage
        // of its own.
        let mut screen = busy(80, 24, Strings::Ignored);
        screen.goto(5, 0);
        screen.feed(&format!("{CSI}K"));
        screen.goto(5, 0);
        let before = screen.text();
        screen.feed(&shielded(KITTY_QUERY));
        assert_eq!(screen.text(), before);
        assert_eq!(screen.cursor(), (5, 0));
        assert!(screen.wrap());
    }

    #[test]
    fn a_leak_at_the_bottom_of_a_narrow_screen_would_scroll_the_screen_away() {
        // Why `ESC 8` is not enough on its own: 31 characters at column 1 of a
        // 20-column terminal wrap twice, and at the bottom of the screen every
        // wrap costs a row that no cursor restore can bring back.
        let mut screen = busy(20, 8, Strings::Printed);
        screen.goto(7, 0);
        screen.feed(KITTY_QUERY);
        assert!(screen.scrolled() > 0, "the screen should have scrolled");
    }

    #[test]
    fn the_shield_stops_a_narrow_screen_from_scrolling() {
        // Autowrap off is what makes this work: the overflow overprints the last
        // cell of the line instead of moving to the next one.
        let mut screen = busy(20, 8, Strings::Printed);
        screen.goto(7, 0);
        let before = screen.text();
        screen.feed(&shielded(KITTY_QUERY));
        assert_eq!(screen.scrolled(), 0, "the screen scrolled anyway");
        assert_eq!(screen.text()[7], " ".repeat(20), "the leak survived");
        assert_eq!(
            screen.text()[..7],
            before[..7],
            "rows above the query were touched"
        );
        assert_eq!(screen.cursor(), (7, 0));
    }

    #[test]
    fn the_shield_erases_only_the_rest_of_one_line() {
        // The documented cost, pinned: everything to the right of the cursor on
        // the cursor's own line goes, and nothing else. Erasing with `CSI J`
        // instead would take the rest of the screen with it, which is why it is
        // not what the shield sends.
        let mut screen = busy(20, 8, Strings::Printed);
        screen.goto(4, 10);
        let before = screen.text();
        screen.feed(&shielded(KITTY_QUERY));
        assert_eq!(
            screen.text()[4],
            format!("{} {}", &before[4][..10], " ".repeat(9))
        );
        assert_eq!(screen.text()[5..], before[5..], "rows below were erased");
    }

    #[test]
    fn a_long_payload_is_no_worse_than_a_short_one() {
        // A glyph registration carries kilobytes of base64. With autowrap off it
        // still cannot reach past the line it started on.
        let payload = format!("\u{1b}_25a1;r;cp=E0A0;{}\u{1b}\\", "A".repeat(4096));
        let mut screen = busy(40, 10, Strings::Printed);
        screen.goto(9, 0);
        let before = screen.text();
        screen.feed(&shielded(&payload));
        assert_eq!(screen.scrolled(), 0);
        assert_eq!(screen.text()[..9], before[..9]);
        assert_eq!(screen.text()[9], " ".repeat(40));
    }

    #[test]
    fn pretty_names_the_introducers() {
        assert_eq!(pretty(&format!("{CSI}?1049h")), "CSI ?1049h");
        assert_eq!(pretty(&format!("{APC}25a1;s{ST}")), "APC 25a1;s ST");
        assert_eq!(pretty(&format!("{ESC}Ou")), "ESC Ou");
    }

    #[test]
    fn pretty_pads_a_terminator_followed_by_text() {
        // Without the padding this read `…;; STclick me`.
        let link = format!("{OSC}8;;{ST}click me");
        assert_eq!(pretty(&link), "OSC 8;; ST click me");
        // But a sequence that *ends* in a terminator gains no trailing space.
        assert_eq!(pretty(&format!("{OSC}8;;{ST}")), "OSC 8;; ST");
        assert_eq!(pretty(&format!("{OSC}0;title{BEL}x")), "OSC 0;title BEL x");
    }

    #[test]
    fn pretty_separates_a_whole_hyperlink_stream() {
        // The case that motivated the padding: an open sequence, the visible
        // text, and a close sequence, all in one call. Neither boundary may run
        // together.
        let stream = format!("{OSC}8;;https://example.com{ST}click me{OSC}8;;{ST}");
        assert_eq!(
            pretty(&stream),
            "OSC 8;;https://example.com ST click me OSC 8;; ST"
        );
    }

    #[test]
    fn pretty_escapes_every_control_byte_not_just_the_named_ones() {
        // The guarantee is unconditional: no control byte survives, whether or
        // not `pretty` has a name for it.
        for byte in 0u8..=0x1f {
            let rendered = pretty(&format!("x{}y", char::from(byte)));
            assert!(
                !rendered.chars().any(char::is_control),
                "control byte {byte:#04x} survived as {rendered:?}"
            );
        }
        assert_eq!(pretty("a\u{0}b"), "a\\x00b");
        assert_eq!(pretty("a\u{7f}b"), "a\\x7fb");
    }

    #[test]
    fn pretty_output_is_safe_to_print() {
        let nasty = format!("{CSI}2J{APC}payload{ST}{DCS}x{ST}\u{7}");
        assert!(!pretty(&nasty).contains('\u{1b}'));
        assert!(!pretty(&nasty).contains('\u{7}'));
    }

    #[test]
    fn tmux_passthrough_doubles_every_escape() {
        let inner = format!("{APC}25a1;s{ST}");
        let wrapped = Multiplexer::Tmux.wrap(&inner);
        assert!(wrapped.starts_with("\u{1b}Ptmux;"));
        assert!(wrapped.ends_with(ST));
        // Two ESC in the payload become four, plus one opening and one closing.
        assert_eq!(wrapped.matches('\u{1b}').count(), 4 + 2);
    }

    #[test]
    fn other_multiplexers_pass_the_sequence_through_unchanged() {
        let inner = format!("{APC}25a1;s{ST}");
        assert_eq!(Multiplexer::Zellij.wrap(&inner), inner);
    }
}
