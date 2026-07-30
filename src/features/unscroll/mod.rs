//! Unscrolling: pulling lines back down out of the scrollback buffer.
//!
//! ```text
//! CSI Ps + T
//! ```
//!
//! kitty's two-character extension of `SD` (Scroll Down, from the VT420). `SD`
//! scrolls the screen down and fills the new rows at the top with blanks; add a
//! `+` intermediate before the final byte and the terminal fills them **from the
//! scrollback buffer** instead. Text that scrolled off the top comes back down
//! onto the screen, in place, and whatever was at the bottom is pushed off.
//!
//! There is no reply, no acknowledgement and no query for support, so this module
//! is almost entirely one `format!` — the interesting parts are how many lines to
//! ask for and what can honestly be said about whether it worked.
//!
//! # What this is for, and what it is not for
//!
//! The obvious guess — restoring what `less` or `man` was showing after it exits —
//! **does not work**, and it is worth knowing why before reaching for this:
//!
//! * On the alternate screen the extension degrades to plain `SD` and inserts
//!   blank lines, because the alternate screen has no scrollback. The
//!   specification says so, and kitty's implementation drops the
//!   fill-from-scrollback flag whenever the active buffer is not the main one.
//! * After a pager exits there is nothing of it in scrollback either: alternate
//!   screen output never enters the history buffer at all, and leaving the
//!   alternate screen restores the primary screen exactly as it was.
//!
//! What it is for is the primary screen, and kitty's own motivating case is the
//! clearest one: a shell prints a block of completions below the prompt, the top
//! of the screen scrolls away, and erasing the completions does not bring it back.
//! Unscroll brings it back. Likewise for a command that clears the screen and
//! prints three lines, or that dumps a screenful over the top of something you
//! were reading.
//!
//! # The modules
//!
//! * [`probe`] — what can honestly be said about support, which is an inference
//!   rather than a measurement: this sequence has no query and no reply.

pub mod probe;

use std::{
    io::{IsTerminal as _, Write as _, stdout},
    process::ExitCode,
};

use anyhow::Result;
use crossterm::terminal;

use crate::{
    cli::{UnscrollAction, UnscrollCommand},
    db::Catalog,
    features::wrap,
    term::{CSI, Multiplexer, pretty},
    theme::{Ink, Theme},
};

/// The feature id in the database. The subcommand is `unscroll`.
pub const FEATURE_ID: &str = "unscroll";

/// The screen height assumed when the real one cannot be measured.
///
/// The VT100's, and what every terminal still falls back to. Only reached when
/// stdout is not a terminal — a pipe, a file, a test — where there is no screen to
/// unscroll in the first place and the number only decides what `--dry-run`
/// prints.
pub const FALLBACK_ROWS: u16 = 24;

/// The count the protocol uses when the parameter is omitted.
///
/// `SD`'s default, which this extension inherits by defining its syntax as
/// identical: xterm's `ctlseqs` documents `CSI Ps T` as "Scroll down Ps lines
/// (default = 1)", and kitty, mintty and contour all default an absent parameter
/// to 1. Recorded here because it is *not* what bare `escapade unscroll` sends —
/// see [`Plan::new`].
pub const PROTOCOL_DEFAULT: u16 = 1;

/// Pulls lines back from scrollback: `CSI <lines> + T`.
///
/// The `+` is an ECMA-48 intermediate byte, and it is the whole difference between
/// this and `SD`. Nothing else uses `+` with a final byte of `T`, which is why the
/// extension could be defined without colliding with anything — including bare
/// `CSI Ps T` with five parameters, which is xterm's Initiate Highlight Mouse
/// Tracking.
#[must_use]
pub fn sequence(lines: u16) -> String {
    format!("{CSI}{lines}+T")
}

/// How many lines one invocation resolved to, and why.
///
/// Pure, and separated from the sending so that the decisions — the default, the
/// clamp, the refusal to send a zero — are testable without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// The count that will go on the wire, or `None` when nothing will be sent.
    pub lines: Option<u16>,
    /// What the user asked for, if they named a count.
    pub requested: Option<u16>,
    /// The screen height this was resolved against.
    pub rows: u16,
    /// Whether that height was measured rather than assumed.
    pub measured: bool,
}

impl Plan {
    /// Resolves a request into a line count.
    ///
    /// Three decisions live here.
    ///
    /// **Bare `escapade unscroll` sends a screenful, not the protocol's default of
    /// one line.** One line is a correct default for a program emitting this in a
    /// loop and a useless one for a command a person runs: the point of running it
    /// is to get your screen back, and a screenful is what "back" means. It is also
    /// the largest request that behaves identically everywhere, since the
    /// specification requires every implementation to accept at least one screen
    /// worth and mintty and contour accept exactly that.
    ///
    /// **An explicit count is clamped to one screenful** for the same reason, plus
    /// a worse one: beyond a screenful the extra lines only push existing screen
    /// content off the bottom, and nothing catches lines that fall off the bottom.
    /// mintty and contour clamp there themselves; kitty would honour a larger
    /// request, so a clamp here is what keeps `escapade unscroll 5000` from meaning
    /// two different things on two terminals.
    ///
    /// **Zero sends nothing at all.** `CSI 0 + T` is read as zero lines by kitty
    /// and contour but as one line by mintty, so there is no portable way to spell
    /// "zero" and no reason to try.
    #[must_use]
    pub fn new(requested: Option<u16>, rows: u16, measured: bool) -> Self {
        // A zero-row screen is not a screen, but it is what a broken ioctl
        // reports, and answering a perfectly clear request with "nothing" would be
        // more confusing than assuming a row.
        let rows = rows.max(1);
        let lines = match requested {
            None => Some(rows),
            Some(0) => None,
            Some(count) => Some(count.min(rows)),
        };
        Self {
            lines,
            requested,
            rows,
            measured,
        }
    }

    /// Whether an explicit count was reduced to fit the screen.
    #[must_use]
    pub fn clamped(&self) -> bool {
        matches!(self.requested, Some(count) if count > self.rows)
    }

    /// The bytes to send, or `None` when there is nothing to do.
    #[must_use]
    pub fn sequence(&self) -> Option<String> {
        self.lines.map(sequence)
    }

    /// What a reader should know about how this resolved, in order.
    ///
    /// Only says something when the resolution was not the obvious one, so the
    /// common case stays silent.
    #[must_use]
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        match self.requested {
            None => notes.push(format!(
                "no count given, so one screenful: {} lines. The protocol's own default is \
                 {PROTOCOL_DEFAULT} line, which is not a useful thing to run by hand, and a \
                 screenful is the largest request every implementation is required to accept.",
                self.rows
            )),
            Some(0) => notes.push(
                "zero lines, so nothing was sent. `CSI 0 + T` means zero lines to kitty and \
                 contour but one line to mintty, so escapade does not send it."
                    .to_owned(),
            ),
            Some(count) if self.clamped() => notes.push(format!(
                "{count} clamped to {} — one screenful. Past that the extra lines only push screen \
                 content off the bottom, where nothing catches it; mintty and contour clamp there \
                 themselves.",
                self.rows
            )),
            Some(_) => {}
        }
        if !self.measured {
            notes.push(format!(
                "stdout is not a terminal, so the screen height could not be measured and \
                 {FALLBACK_ROWS} rows were assumed."
            ));
        }
        notes
    }
}

/// Entry point for `escapade unscroll …`.
///
/// Returns an exit status because `probe` is a predicate: `0` supported, `1` not
/// supported, `2` cannot tell. Every other path returns success.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with.
pub fn run(catalog: &Catalog, command: &UnscrollCommand, no_color: bool) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        Some(UnscrollAction::Explain { no_matrix }) => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        Some(UnscrollAction::Probe { format }) => probe::report(&theme, catalog, *format),
        None => {
            unscroll(&theme, command)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The terminal's height, and whether it could actually be read.
///
/// The `TIOCGWINSZ` ioctl, through crossterm — no round trip and no reply to
/// drain, which matters for a command whose whole job is one escape sequence.
fn screen_rows() -> (u16, bool) {
    match terminal::size() {
        Ok((_, rows)) if rows > 0 => (rows, true),
        _ => (FALLBACK_ROWS, false),
    }
}

/// Sends one unscroll, or prints what it would send.
///
/// # Errors
///
/// Fails if stdout cannot be written to.
fn unscroll(theme: &Theme, command: &UnscrollCommand) -> Result<()> {
    let (rows, measured) = screen_rows();
    let plan = Plan::new(command.lines, rows, measured);

    if command.dry_run {
        print_dry_run(theme, &plan);
        return Ok(());
    }

    // Notes go to stderr so the escape sequence stays the only thing on stdout —
    // this is a command meant to be usable from a keybinding.
    let mut err = std::io::stderr();
    for note in plan.notes() {
        let _ = writeln!(err, "{} {note}", theme.paint("unscroll:", Ink::Muted));
    }

    let Some(sequence) = plan.sequence() else {
        return Ok(());
    };
    // Written straight to stdout rather than through `Term`: there is no mode to
    // change and nothing to restore, and constructing a `Term` would fail on the
    // pipe where writing works fine.
    let mut out = stdout();
    out.write_all(sequence.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// Prints what would be sent, in readable form.
fn print_dry_run(theme: &Theme, plan: &Plan) {
    println!();
    match plan.sequence() {
        Some(sequence) => {
            println!("  {}", theme.paint(&pretty(&sequence), Ink::Code));
            println!();
            println!(
                "  {}",
                theme.paint(
                    &format!(
                        "1 escape code, {} bytes on the wire, no reply",
                        sequence.len()
                    ),
                    Ink::Muted
                )
            );
        }
        None => println!("  {}", theme.paint("(nothing to send)", Ink::Muted)),
    }
    for note in plan.notes() {
        for line in wrap(&note, theme.width().saturating_sub(4)) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }
    if let Some(multiplexer) = Multiplexer::detect() {
        for line in wrap(
            &format!(
                "sent unwrapped, and it will not work: inside {} the scrollback belongs to the \
                 multiplexer rather than to the terminal underneath, so there is nothing for \
                 passthrough to fix.",
                multiplexer.name()
            ),
            theme.width().saturating_sub(4),
        ) {
            println!("  {}", theme.paint(&line, Ink::Warn));
        }
    }
    if !stdout().is_terminal() {
        for line in wrap(
            "stdout is not a terminal, so there is no screen to unscroll — this would go into the \
             pipe as bytes.",
            theme.width().saturating_sub(4),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }
    println!();
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // -- the sequence -------------------------------------------------------

    #[test]
    fn a_count_goes_in_front_of_the_plus() {
        assert_eq!(sequence(5), "\u{1b}[5+T");
        assert_eq!(sequence(1), "\u{1b}[1+T");
        assert_eq!(sequence(u16::MAX), "\u{1b}[65535+T");
    }

    #[test]
    fn the_intermediate_is_what_distinguishes_it_from_plain_sd() {
        // Drop the `+` and this is SD, which fills with blanks instead. The
        // difference is two characters and it is the entire protocol.
        let built = sequence(3);
        assert!(built.ends_with("+T"), "{built:?}");
        assert_eq!(built.replace("+T", "T"), "\u{1b}[3T");
    }

    #[test]
    fn the_sequence_renders_without_a_control_byte() {
        let shown = pretty(&sequence(12));
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
        assert_eq!(shown, "CSI 12+T");
    }

    // -- resolving a count --------------------------------------------------

    #[test]
    fn bare_unscroll_sends_a_screenful_rather_than_the_protocol_default() {
        // The protocol default is one line. That is right for a program emitting
        // this in a loop and useless for a person running it once.
        let plan = Plan::new(None, 30, true);
        assert_eq!(plan.lines, Some(30));
        assert_ne!(plan.lines, Some(PROTOCOL_DEFAULT));
        assert_eq!(plan.sequence().as_deref(), Some("\u{1b}[30+T"));
        assert!(!plan.clamped());
        assert!(
            plan.notes()[0].contains("screenful"),
            "the choice should be explained: {:?}",
            plan.notes()
        );
    }

    #[test]
    fn an_explicit_count_that_fits_is_sent_verbatim_and_silently() {
        let plan = Plan::new(Some(5), 24, true);
        assert_eq!(plan.lines, Some(5));
        assert_eq!(plan.sequence().as_deref(), Some("\u{1b}[5+T"));
        assert!(!plan.clamped());
        assert!(plan.notes().is_empty(), "{:?}", plan.notes());
    }

    #[test]
    fn a_count_equal_to_the_screen_height_is_not_clamped() {
        let plan = Plan::new(Some(24), 24, true);
        assert_eq!(plan.lines, Some(24));
        assert!(!plan.clamped());
    }

    #[test]
    fn an_absurd_count_is_clamped_to_one_screenful() {
        // kitty would honour 50000 by pulling in the whole scrollback; mintty and
        // contour cap at one screen. Clamping is what stops the same command from
        // meaning two different things on two terminals.
        let plan = Plan::new(Some(50_000), 24, true);
        assert_eq!(plan.lines, Some(24));
        assert!(plan.clamped());
        let note = &plan.notes()[0];
        assert!(note.contains("50000"), "{note}");
        assert!(note.contains("24"), "{note}");

        let plan = Plan::new(Some(u16::MAX), 1, true);
        assert_eq!(plan.lines, Some(1), "a one-row screen clamps to one line");
    }

    #[test]
    fn zero_sends_nothing_because_the_terminals_disagree_about_it() {
        let plan = Plan::new(Some(0), 24, true);
        assert_eq!(plan.lines, None);
        assert!(plan.sequence().is_none());
        assert!(
            !plan.clamped(),
            "zero was honoured, not reduced to fit the screen"
        );
        assert!(plan.notes()[0].contains("mintty"), "{:?}", plan.notes());
    }

    #[test]
    fn an_unmeasurable_screen_still_produces_a_plan_and_says_it_guessed() {
        let plan = Plan::new(None, FALLBACK_ROWS, false);
        assert_eq!(plan.lines, Some(FALLBACK_ROWS));
        assert!(
            plan.notes()
                .iter()
                .any(|note| note.contains("not a terminal")),
            "{:?}",
            plan.notes()
        );
    }

    #[test]
    fn a_zero_row_screen_is_treated_as_one_row() {
        // What a broken ioctl reports.
        let plan = Plan::new(None, 0, false);
        assert_eq!(plan.rows, 1);
        assert_eq!(plan.lines, Some(1));
    }

    // -- the data file ------------------------------------------------------

    #[test]
    fn the_feature_file_is_complete_enough_to_explain() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        assert_eq!(feature.name, "Unscroll");
        assert!(feature.spec_url.is_some());
        // The extension, and the SD it extends.
        assert_eq!(feature.sequence.len(), 2);
        assert!(feature.sequence[0].request.contains("+ T"));
        assert!(!feature.param.is_empty());
        assert!(feature.caveats.is_some());
        assert!(!feature.resource.is_empty());
    }

    #[test]
    fn the_matrix_names_the_three_implementations_and_cites_every_row() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();

        let mut yes: Vec<&str> = feature
            .support
            .iter()
            .filter(|row| row.status == crate::db::SupportStatus::Yes)
            .map(|row| row.terminal.as_str())
            .collect();
        yes.sort_unstable();
        assert_eq!(yes, ["contour", "kitty", "mintty"]);

        for row in &feature.support {
            // An uncited claim about a terminal nobody can check is worth less
            // than an admission that nobody checked.
            assert!(
                row.source.is_some(),
                "no source for the {} row",
                row.terminal
            );
        }
    }

    #[test]
    fn the_alternate_screen_limitation_is_documented_rather_than_left_implied() {
        // The use case everyone reaches for first does not work. If that is not in
        // the prose, the prose is misleading.
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        let caveats = feature.caveats.as_deref().unwrap();
        assert!(caveats.contains("alternate screen"), "{caveats}");
        assert!(caveats.contains("less"), "{caveats}");
    }
}
