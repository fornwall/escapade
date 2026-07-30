//! Getting the numbers: one ioctl, and at most one round trip.
//!
//! The cheap source comes first. `TIOCGWINSZ` — reached through
//! [`crossterm::terminal::window_size`], because `unsafe_code` is forbidden here
//! and an `ioctl` cannot be called without it — needs no query, cannot be
//! swallowed by a multiplexer, and cannot be confused by a terminal that answers
//! reports out of order. When it answers everything, nothing is sent at all.
//!
//! When it does not, three queries go out together, followed by a barrier:
//!
//! ```text
//! →  CSI 18 t              how big is the text area, in characters?
//! →  CSI 14 t              how big is it in pixels?
//! →  CSI 16 t              how big is one cell in pixels?
//! →  CSI c                 the barrier
//! ←  CSI 8 ; rows ; cols t
//! ←  CSI 4 ; height ; width t
//! ←  CSI 6 ; height ; width t
//! ←  CSI ? 62 ; 4 c        the barrier came back
//! ```
//!
//! All three at once, because a terminal processes input in order and answers in
//! order: asking separately would cost three timeouts to learn the same thing.
//! The Primary Device Attributes barrier is what distinguishes silence from
//! slowness — every terminal since the VT100 answers it, so if the barrier's
//! reply arrives and a size report did not, that terminal does not implement that
//! report. See [`crate::features::glyph_protocol::probe`] for the full argument,
//! including why a multiplexer invalidates the negative.
//!
//! Every reply provoked is consumed before returning, including the barrier's.
//! An unread reply is delivered to whatever runs next — the user's shell, which
//! prints it as stray text.
//!
//! The queries are deliberately **not** wrapped in a multiplexer's passthrough.
//! Inside tmux or Zellij the multiplexer is the thing that lays out cells, so its
//! answer is the one an application actually has to live with; wrapping would ask
//! a question about a window this process cannot draw in.

use std::{
    io::{IsTerminal as _, stdin, stdout},
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    features::dimensions::size::{Sizes, absorb_report},
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term},
};

/// How long to wait for the terminal to answer. Generous enough for a slow ssh
/// link, short enough not to feel like a hang.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

/// How long to spend consuming replies still in flight once a conclusion has been
/// reached.
///
/// Short, because by this point the terminal has demonstrably answered something
/// and anything else is either buffered already or arriving immediately.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(150);

/// Asks for the text area size in characters: `CSI 18 t`.
///
/// Answered with `CSI 8 ; rows ; cols t`.
#[must_use]
pub fn cells_query() -> String {
    format!("{CSI}18t")
}

/// Asks for the text area size in pixels: `CSI 14 t`.
///
/// Answered with `CSI 4 ; height ; width t`. Note that this is the *text area*,
/// not the window: `CSI 14 ; 2 t` is the variant that includes the window
/// manager's decoration and the scroll bar.
#[must_use]
pub fn window_query() -> String {
    format!("{CSI}14t")
}

/// Asks for one cell's size in pixels: `CSI 16 t`.
///
/// Answered with `CSI 6 ; height ; width t`. The most useful of the three and the
/// least implemented — Alacritty, VTE and iTerm2 all skip it.
#[must_use]
pub fn cell_query() -> String {
    format!("{CSI}16t")
}

/// The Primary Device Attributes barrier: `CSI c`.
///
/// Answered with `CSI ? <classes> c` by every terminal since the VT100, which is
/// what makes silence in front of it a conclusion rather than a timeout.
#[must_use]
pub fn barrier() -> String {
    format!("{CSI}c")
}

/// What the caller needs, which decides whether a round trip is worth its
/// timeout.
#[derive(Debug, Clone, Copy)]
pub struct Wanted {
    /// Whether the window's pixel size is needed. Cell counts alone are almost
    /// always answered by the ioctl, so a `--rows-at-least` test can be instant.
    pub window_pixels: bool,
    /// Whether the size of a single cell is needed.
    ///
    /// This one is worth a round trip even when the ioctl answered, because the
    /// ioctl can only ever supply a cell size by division: `CSI 16 t` is the only
    /// source that measures it, and the difference is whatever padding the
    /// terminal keeps around its text area.
    pub cell_pixels: bool,
    /// Whether to ask even when everything wanted is already known.
    ///
    /// `probe` sets this: its question is which queries this terminal *answers*,
    /// and that cannot be learned by not asking.
    pub always_ask: bool,
}

impl Wanted {
    /// Whether `sizes` already holds everything this caller asked for, to the
    /// standard it asked for it.
    fn satisfied_by(self, sizes: &Sizes) -> bool {
        if sizes.cells().is_none() {
            return false;
        }
        if self.window_pixels && sizes.window().is_none() {
            return false;
        }
        if self.cell_pixels && !sizes.cell().is_some_and(|cell| cell.origin.is_measured()) {
            return false;
        }
        true
    }
}

/// Collects every size that can be had, in preference order.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to. A terminal that
/// stays quiet is a result, not an error.
pub fn gather(wanted: Wanted) -> Result<Sizes> {
    let mut sizes = Sizes {
        multiplexer: Multiplexer::detect(),
        // Both halves are needed: the query goes out on stdout and the answer
        // comes back through the keyboard side of the pty.
        interactive: stdout().is_terminal() && stdin().is_terminal(),
        ..Sizes::default()
    };
    read_ioctl(&mut sizes);

    if !sizes.interactive || (wanted.satisfied_by(&sizes) && !wanted.always_ask) {
        return Ok(sizes);
    }

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let outcome = ask(&mut term, &mut input, &mut sizes);
    // Restore before the caller prints anything, so the report appears in a
    // normal cooked-mode terminal.
    term.restore();
    outcome?;
    Ok(sizes)
}

/// Reads what the kernel already knows: `TIOCGWINSZ`, via `crossterm`.
///
/// `window_size` carries all four fields, of which `ws_xpixel` and `ws_ypixel`
/// are the interesting ones — Linux's `tty_ioctl(4)` calls them "unused", but most
/// Unix terminals fill them in, and when they do the whole pixel size is free
/// without sending anything. `size` is the fallback for cells alone: the same
/// ioctl, plus a `tput` last resort for the case where even that fails.
fn read_ioctl(sizes: &mut Sizes) {
    if let Ok(window) = crossterm::terminal::window_size() {
        sizes.ioctl_cells = nonzero(u32::from(window.columns), u32::from(window.rows));
        sizes.ioctl_pixels = nonzero(u32::from(window.width), u32::from(window.height));
    }
    if sizes.ioctl_cells.is_none()
        && let Ok((cols, rows)) = crossterm::terminal::size()
    {
        sizes.ioctl_cells = nonzero(u32::from(cols), u32::from(rows));
    }
}

/// A pair, unless either half is zero — which is how "the field exists but was
/// never filled in" arrives.
fn nonzero(horizontal: u32, vertical: u32) -> Option<(u32, u32)> {
    if horizontal == 0 || vertical == 0 {
        None
    } else {
        Some((horizontal, vertical))
    }
}

/// Sends the three size queries and the barrier, and folds in every answer.
///
/// # Errors
///
/// Fails if the terminal cannot be put in raw mode or written to.
fn ask(term: &mut Term, input: &mut InputReader, sizes: &mut Sizes) -> Result<()> {
    // Raw mode is required: in cooked mode the replies would sit in the line
    // buffer until the user pressed Enter, and would be echoed back at them.
    term.enable_raw()?;
    input.drain();

    term.write(&cells_query())?;
    term.write(&window_query())?;
    term.write(&cell_query())?;
    term.write_now(&barrier())?;
    sizes.queried = true;

    listen(input, sizes, REPLY_TIMEOUT);
    if !sizes.answered_barrier {
        // The barrier's reply is the last one sent, so a timeout can leave it —
        // and any report behind it — in flight. Unread, it lands in the user's
        // shell as stray text; consuming it here is what keeps this invisible.
        listen(input, sizes, DRAIN_TIMEOUT);
    }
    Ok(())
}

/// Reads replies until the barrier comes back or `timeout` expires.
fn listen(input: &mut InputReader, sizes: &mut Sizes, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        // An EOF on stdin means there is nobody to answer; treat it as silence
        // rather than as an error.
        let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
            return;
        };
        match &reply {
            Reply::Csi {
                params,
                final_byte: 't',
            } => {
                absorb_report(sizes, params);
            }
            Reply::Csi {
                final_byte: 'c', ..
            } => {
                // Everything sent before the barrier has now been processed, so
                // anything still missing is going to stay missing.
                sizes.answered_barrier = true;
                return;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::term::pretty;

    #[test]
    fn the_queries_are_the_sequences_they_claim_to_be() {
        assert_eq!(pretty(&cells_query()), "CSI 18t");
        assert_eq!(pretty(&window_query()), "CSI 14t");
        assert_eq!(pretty(&cell_query()), "CSI 16t");
        assert_eq!(pretty(&barrier()), "CSI c");
    }

    #[test]
    fn a_query_is_a_control_sequence_and_nothing_else() {
        // A stray printable byte in one of these would land on the user's screen.
        for query in [cells_query(), window_query(), cell_query(), barrier()] {
            assert!(query.starts_with(CSI));
            assert_eq!(query.matches('\u{1b}').count(), 1, "{query:?}");
        }
    }

    #[test]
    fn a_zero_field_is_not_a_measurement() {
        assert_eq!(nonzero(0, 24), None);
        assert_eq!(nonzero(80, 0), None);
        assert_eq!(nonzero(80, 24), Some((80, 24)));
    }

    #[test]
    fn the_ioctl_alone_satisfies_a_cells_only_caller() {
        let ioctl = Sizes {
            ioctl_cells: Some((80, 24)),
            ..Sizes::default()
        };
        let cells_only = Wanted {
            window_pixels: false,
            cell_pixels: false,
            always_ask: false,
        };
        assert!(
            cells_only.satisfied_by(&ioctl),
            "a --rows-at-least test must not cost a round trip"
        );
        assert!(!cells_only.satisfied_by(&Sizes::default()));
    }

    #[test]
    fn a_derived_cell_size_does_not_satisfy_a_caller_that_wants_it_measured() {
        // Both pixel fields of TIOCGWINSZ filled in, so a cell size is available
        // by division — but only CSI 16 t measures one, and the difference is the
        // terminal's padding.
        let ioctl = Sizes {
            ioctl_cells: Some((80, 24)),
            ioctl_pixels: Some((800, 480)),
            ..Sizes::default()
        };
        let window_only = Wanted {
            window_pixels: true,
            cell_pixels: false,
            always_ask: false,
        };
        assert!(window_only.satisfied_by(&ioctl));
        let with_cell = Wanted {
            cell_pixels: true,
            ..window_only
        };
        assert!(!with_cell.satisfied_by(&ioctl));
        assert!(with_cell.satisfied_by(&Sizes {
            reported_cell: Some((10, 20)),
            ..ioctl
        }));
    }

    #[test]
    fn a_report_reply_lands_in_the_right_field() {
        // The parsing half of `listen`, exercised without a terminal.
        let mut sizes = Sizes::default();
        assert!(absorb_report(&mut sizes, "8;24;80"));
        assert_eq!(sizes.reported_cells, Some((80, 24)));
    }
}
