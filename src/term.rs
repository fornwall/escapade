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

#[cfg(test)]
mod tests {
    use super::*;

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
