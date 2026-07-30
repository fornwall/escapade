//! Command line surface.
//!
//! One subcommand per terminal feature, each with the same verbs: `explain`
//! (read about it), `probe` (does *this* terminal do it?) and `demo` (watch it
//! happen). Features that are useful outside a demo add their own verb —
//! `graphics` has `render`. Keeping that shape uniform is deliberate: as
//! features are added, there is nothing new to learn.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Demonstrate, visualize and actually use modern terminal features.
#[derive(Debug, Parser)]
#[command(
    name = "escapade",
    version,
    about,
    long_about = "Demonstrate, visualize and actually use modern terminal features.\n\n\
                  Terminals have grown a lot of capabilities that almost nothing takes \
                  advantage of, largely because they are undocumented, unevenly supported, \
                  and invisible until something renders wrong. escapade explains each one, \
                  probes whether your terminal implements it, and shows it running.",
    arg_required_else_help = true,
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Print without colors or styling
    #[arg(long, global = true)]
    pub no_color: bool,
}

/// Top-level subcommands: one per feature, plus an overview.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Ship your own vector glyphs to the terminal, no patched font required
    #[command(name = "glyph", arg_required_else_help = true)]
    GlyphProtocol(FeatureCommand<GlyphAction>),

    /// Display real images in the terminal with the kitty graphics protocol
    #[command(name = "graphics", visible_alias = "kitty-graphics", arg_required_else_help = true)]
    Graphics(FeatureCommand<GraphicsAction>),

    /// Treat multi-codepoint sequences as one character, the way users see them
    #[command(name = "grapheme", arg_required_else_help = true)]
    Grapheme(FeatureCommand<GraphemeAction>),

    /// Make terminal text clickable with OSC 8 hyperlinks
    #[command(name = "hyperlink", arg_required_else_help = true)]
    Hyperlink(HyperlinkCommand),

    /// Raise a desktop notification from inside the terminal
    #[command(name = "notification", visible_alias = "notify", arg_required_else_help = true)]
    Notification(NotificationCommand),

    /// Show progress in the taskbar or tab title, from a script
    #[command(name = "progress", arg_required_else_help = true)]
    Progress(ProgressCommand),

    /// Draw text several cells tall that still copies as one line
    #[command(name = "textsize", visible_alias = "text-sizing", arg_required_else_help = true)]
    Textsize(TextsizeCommand),

    /// Ask the terminal what colour it actually is, and watch for changes
    #[command(name = "color", visible_alias = "colour")]
    Color(ColorCommand),

    /// Reach the clipboard of the machine you are sitting at, over ssh
    #[command(name = "clipboard", arg_required_else_help = true)]
    Clipboard(ClipboardCommand),

    /// Style text: bold, italic, underlines, strikethrough, colours
    #[command(name = "style", visible_alias = "sgr", arg_required_else_help = true)]
    Style(StyleCommand),

    /// Report the terminal's size in cells and pixels, or test it against a threshold
    #[command(name = "dimensions", visible_alias = "size")]
    Dimensions(DimensionsCommand),

    /// Pull scrollback back down onto the screen, undoing a scroll
    #[command(name = "unscroll")]
    Unscroll(UnscrollCommand),

    /// See what your keyboard actually sends, and what the kitty protocol adds
    #[command(name = "keys", arg_required_else_help = true)]
    Keys(KeysCommand),

    /// Turn a stream of terminal bytes back into named, explained sequences
    #[command(name = "decode")]
    Decode(DecodeCommand),

    /// Diagnose this terminal: what terminfo claims, what it says about itself,
    /// and what it actually does
    #[command(name = "doctor")]
    Doctor(DoctorCommand),

    /// List every feature escapade knows about, with support at a glance
    List,
}

// ---------------------------------------------------------------------------
// Progress reporting
// ---------------------------------------------------------------------------

/// `escapade progress`.
///
/// A long-running command can tell the terminal how far along it is, and the
/// terminal shows it where the user is already looking — the taskbar, the dock
/// icon, the tab title. The whole point is to be driven from a script, so the
/// primary form is a bare percentage.
#[derive(Debug, Args)]
#[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true, after_help = "\
Examples:
  escapade progress 42
  escapade progress --state error
  escapade progress --clear
  for i in $(seq 0 100); do sleep 0.05; echo $i; done | escapade progress --watch
  cargo build 2>&1 | grep --line-buffered -o '[0-9]*%' | escapade progress --watch")]
pub struct ProgressCommand {
    #[command(subcommand)]
    pub action: Option<ProgressAction>,

    /// How far along, 0 to 100
    #[arg(value_name = "PERCENT", required_unless_present_any = ["state", "clear", "watch"])]
    pub percent: Option<u8>,

    /// Show a state other than ordinary progress
    #[arg(long, value_enum)]
    pub state: Option<ProgressState>,

    /// Remove the indicator
    #[arg(long, conflicts_with_all = ["percent", "state"])]
    pub clear: bool,

    /// Read percentages from standard input, one per line, until it closes
    #[arg(long, conflicts_with_all = ["percent", "clear"])]
    pub watch: bool,

    /// Print the escape sequence in readable form instead of sending it
    #[arg(long)]
    pub dry_run: bool,
}

/// What kind of progress to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ProgressState {
    /// Ordinary determinate progress, needs a percentage
    Normal,
    /// Something went wrong; usually drawn red
    Error,
    /// Working, but the total is unknown
    Indeterminate,
    /// Paused or needing attention; usually drawn yellow
    Warning,
}

/// The usual verbs for progress reporting.
#[derive(Debug, Subcommand)]
pub enum ProgressAction {
    /// Explain the sequence, its states, and which terminals show it
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },
    /// Check whether this terminal appears to show progress
    Probe {
        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },
    /// Walk through the states step by step
    Demo {
        /// Run even if the terminal appears not to support it
        #[arg(long)]
        force: bool,
        /// Run only the named step. See --list for the names
        #[arg(long, value_name = "NAME", conflicts_with = "step")]
        only: Option<String>,
        /// Start at this step number instead of the first
        #[arg(long, value_name = "N")]
        step: Option<usize>,
        /// List the steps, with their names, and exit
        #[arg(long)]
        list: bool,
    },
}

// ---------------------------------------------------------------------------
// Text sizing
// ---------------------------------------------------------------------------

/// `escapade textsize`.
///
/// Text drawn several cells tall, which still selects and copies as the one line
/// it really is — unlike figlet-style ASCII art, which copies as the picture it
/// is made of.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true, after_help = "\
Support for this protocol is per-key rather than all-or-nothing: some terminals
implement --width but not --scale. `probe` reports each separately, and a request
a terminal cannot honour degrades to ordinary text rather than failing.

Examples:
  escapade textsize --scale 4 'BUILD PASSED'
  escapade textsize --width 2 'wide'
  escapade textsize --scale 2 --fraction 1/2 'half again'
  escapade textsize --scale 3 --valign bottom 'sitting low'")]
pub struct TextsizeCommand {
    #[command(subcommand)]
    pub action: Option<TextsizeAction>,

    /// Text to draw. Joined with spaces; read from standard input when omitted,
    /// and `-` reads it even from a terminal
    #[arg(value_name = "TEXT")]
    pub text: Vec<String>,

    /// Make each character this many cells tall and wide, 1 to 7
    #[arg(long, short, value_name = "N")]
    pub scale: Option<u8>,

    /// Make each character occupy this many cells horizontally
    #[arg(long, short, value_name = "N")]
    pub width: Option<u8>,

    /// Scale by a fraction as well, written NUM/DEN
    #[arg(long, value_name = "NUM/DEN")]
    pub fraction: Option<String>,

    /// Where to sit the text within its taller block
    #[arg(long, value_enum)]
    pub valign: Option<VerticalAlign>,

    /// Where to sit the text within its wider block
    #[arg(long, value_enum)]
    pub halign: Option<HorizontalAlign>,

    /// Print the escape sequence in readable form instead of sending it
    #[arg(long)]
    pub dry_run: bool,
}

/// Vertical placement of scaled text within its block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum VerticalAlign {
    /// Against the top of the block
    Top,
    /// Centred
    Center,
    /// Against the bottom
    Bottom,
}

/// Horizontal placement of scaled text within its block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum HorizontalAlign {
    /// Against the left edge
    Left,
    /// Centred
    Center,
    /// Against the right edge
    Right,
}

/// The usual verbs for text sizing.
#[derive(Debug, Subcommand)]
pub enum TextsizeAction {
    /// Explain the protocol, its keys, and which terminals implement which
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },
    /// Check which keys of the protocol this terminal implements
    Probe {
        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },
    /// Walk through the protocol step by step
    Demo {
        /// Run even if the terminal appears not to support it
        #[arg(long)]
        force: bool,
        /// Run only the named step. See --list for the names
        #[arg(long, value_name = "NAME", conflicts_with = "step")]
        only: Option<String>,
        /// Start at this step number instead of the first
        #[arg(long, value_name = "N")]
        step: Option<usize>,
        /// List the steps, with their names, and exit
        #[arg(long)]
        list: bool,
    },
}

// ---------------------------------------------------------------------------
// Colour
// ---------------------------------------------------------------------------

/// `escapade color`.
///
/// The only reliable way to know what colour a terminal is, is to ask it — and
/// the only way to keep knowing is to be told when it changes. Guessing from
/// `$COLORFGBG` or the terminal's name is what produces unreadable dark-on-dark
/// text when a user switches theme.
#[derive(Debug, Args)]
#[command(after_help = "\
Colour replies use XParseColor syntax, where each channel may be 1 to 4 hex
digits and the digits are most-significant — so `rgb:f/0/0` is full red, not
almost-black. escapade reports both the raw reply and the normalised 8-bit value.

Exit status with --is-dark:
  0   the background is dark
  1   the background is light
  2   the terminal would not say

Examples:
  escapade color
  escapade color --query background --format json
  escapade color --index 4
  escapade color --is-dark && export THEME=dark
  escapade color --watch    # print a line each time the user changes theme")]
pub struct ColorCommand {
    #[command(subcommand)]
    pub action: Option<ColorAction>,

    /// Which colours to report. Repeatable; defaults to the main three
    #[arg(long, value_enum, value_name = "TARGET")]
    pub query: Vec<ColorTarget>,

    /// Report this palette entry, 0 to 255
    #[arg(long, value_name = "N")]
    pub index: Option<u8>,

    /// Keep running, printing a line each time the colours change
    #[arg(long)]
    pub watch: bool,

    /// Say nothing; report light or dark through the exit status
    #[arg(long, conflicts_with_all = ["watch", "format"])]
    pub is_dark: bool,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

/// A colour worth asking about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ColorTarget {
    /// Default text colour
    Foreground,
    /// Default background colour
    Background,
    /// Cursor colour
    Cursor,
    /// All 256 palette entries
    Palette,
    /// Selection background
    Selection,
}

/// The usual verbs for colour queries.
#[derive(Debug, Subcommand)]
pub enum ColorAction {
    /// Explain the queries, the reply format, and change notification
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },
    /// Check which colour queries this terminal answers
    Probe {
        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },
    /// Walk through the queries step by step
    Demo {
        /// Run even if the terminal appears not to support it
        #[arg(long)]
        force: bool,
        /// Run only the named step. See --list for the names
        #[arg(long, value_name = "NAME", conflicts_with = "step")]
        only: Option<String>,
        /// Start at this step number instead of the first
        #[arg(long, value_name = "N")]
        step: Option<usize>,
        /// List the steps, with their names, and exit
        #[arg(long)]
        list: bool,
    },
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// `escapade clipboard`.
///
/// The clipboard belongs to the machine the human is sitting at, not the machine
/// the program is running on. OSC 52 is the only way to reach across that gap, and
/// it is why this works from inside ssh, inside a container, and inside a CI
/// runner's terminal.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct ClipboardCommand {
    #[command(subcommand)]
    pub action: ClipboardAction,
}

/// What to do with the clipboard.
#[derive(Debug, Subcommand)]
pub enum ClipboardAction {
    /// Put text on the clipboard, reading standard input if no text is given
    #[command(after_help = "\
Reading the clipboard is disabled by default in most terminals, and writing is
sometimes capped in size — both are deliberate, since a remote host being able to
read your clipboard is a real hazard. See `explain`.

Examples:
  escapade clipboard copy 'hello'
  git rev-parse HEAD | escapade clipboard copy
  escapade clipboard copy --selection primary 'middle-click paste'
  cat report.txt | escapade clipboard copy --dry-run")]
    Copy {
        /// Text to copy. Read from standard input when omitted
        #[arg(value_name = "TEXT")]
        text: Vec<String>,

        /// Which selection to write
        #[arg(long, short, value_enum, default_value_t = ClipboardSelection::Clipboard)]
        selection: ClipboardSelection,

        /// Print the escape sequence in readable form instead of sending it
        #[arg(long)]
        dry_run: bool,
    },

    /// Read the clipboard back, if the terminal permits it
    Paste {
        /// Which selection to read
        #[arg(long, short, value_enum, default_value_t = ClipboardSelection::Clipboard)]
        selection: ClipboardSelection,
    },

    /// Explain the sequence, its size limits, and why reading is usually refused
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },

    /// Check whether this terminal accepts clipboard writes, and reads
    Probe {
        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },

    /// Walk through the protocol step by step
    Demo {
        /// Run even if the terminal appears not to support it
        #[arg(long)]
        force: bool,
        /// Run only the named step. See --list for the names
        #[arg(long, value_name = "NAME", conflicts_with = "step")]
        only: Option<String>,
        /// Start at this step number instead of the first
        #[arg(long, value_name = "N")]
        step: Option<usize>,
        /// List the steps, with their names, and exit
        #[arg(long)]
        list: bool,
    },
}

/// Which X-style selection a clipboard operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ClipboardSelection {
    /// The system clipboard, pasted with Ctrl-V
    Clipboard,
    /// The X primary selection, pasted with the middle mouse button
    Primary,
    /// The cut buffer
    Select,
}

/// The SGR text attributes, shared by the top-level form and by `probe`.
///
/// Flattened into both so the flags that *set* an attribute are the same flags
/// that *ask about* one — `--bold` styles text, `style probe --bold` asks whether
/// bold works. Keeping one struct means the two can never drift apart.
#[derive(Debug, Args)]
#[command(next_help_heading = "Attributes")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "these are command line flags, and each attribute is independently \
              present or absent"
)]
pub struct StyleAttrs {
    /// Bold, SGR 1
    #[arg(long, short)]
    pub bold: bool,

    /// Faint, SGR 2
    #[arg(long)]
    pub dim: bool,

    /// Italic, SGR 3
    #[arg(long, short)]
    pub italic: bool,

    /// Underline, SGR 4, in one of six styles
    ///
    /// The style is always required rather than optional. With an optional value,
    /// `--underline curly hello` does not error: clap reads a bare `--underline`
    /// and lets `curly` fall through into the positional text, silently
    /// underlining "curly hello" with a plain line. A required value makes that
    /// impossible to express. This regressed once already — there is a test.
    #[arg(long, short, value_enum, value_name = "STYLE")]
    pub underline: Option<UnderlineStyle>,

    /// Slow blink, SGR 5
    #[arg(long)]
    pub blink: bool,

    /// Rapid blink, SGR 6 — almost universally treated as SGR 5
    #[arg(long)]
    pub rapid_blink: bool,

    /// Swap foreground and background, SGR 7
    #[arg(long)]
    pub reverse: bool,

    /// Conceal, SGR 8 — widely ignored, and never a security measure
    #[arg(long)]
    pub conceal: bool,

    /// Strikethrough, SGR 9
    #[arg(long, short)]
    pub strikethrough: bool,

    /// Overline, SGR 53
    #[arg(long)]
    pub overline: bool,

    /// Text colour: #rrggbb, a palette index 0-255, or a name
    #[arg(long, short, value_name = "COLOR")]
    pub color: Option<String>,

    /// Background colour, same forms
    #[arg(long, value_name = "COLOR")]
    pub background: Option<String>,

    /// Underline colour, independent of the text colour, SGR 58
    #[arg(long, value_name = "COLOR")]
    pub underline_color: Option<String>,
}

/// `escapade style`.
///
/// Every SGR text attribute in one place, because they compose into one sequence.
/// Anything else would force `escapade underline "$(escapade bold text)"`.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true, after_help = "\
Text comes from the arguments, or from standard input when there are none. A lone
`-` reads standard input even from a terminal.

`--underline` always takes a style: `--underline single` for a plain line,
`--underline curly` for a wavy one. It is required rather than optional so it can
never be confused with the text being styled.

Attributes compose into a single sequence, and are reset afterwards. A terminal
that does not know one ignores it — often silently, which is what `probe` is for.

Examples:
  escapade style --bold --underline curly --color blue 'warning'
  escapade style --strikethrough --dim 'done'
  cargo build 2>&1 | escapade style --italic
  escapade style --underline dotted --underline-color '#5f87ff' 'note'
  escapade style probe --bold --italic && echo 'both work here'")]
pub struct StyleCommand {
    #[command(subcommand)]
    pub action: Option<StyleAction>,

    /// Text to style. Joined with spaces; read from standard input when omitted,
    /// and `-` reads it even from a terminal
    #[arg(value_name = "TEXT")]
    pub text: Vec<String>,

    #[command(flatten)]
    pub attrs: StyleAttrs,

    /// Print the escape sequence in readable form instead of sending it
    #[arg(long)]
    pub dry_run: bool,
}

/// The underline styles of SGR 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum UnderlineStyle {
    /// No underline at all, `4:0`
    None,
    /// The traditional straight line, `4:1`
    Single,
    /// Two straight lines, `4:2`
    Double,
    /// A wavy line, `4:3` — the spell-checker underline
    Curly,
    /// A dotted line, `4:4`
    Dotted,
    /// A dashed line, `4:5`
    Dashed,
}

/// The usual verbs for text attributes.
#[derive(Debug, Subcommand)]
pub enum StyleAction {
    /// Explain the attributes, their resets, and who actually renders each
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },

    /// Ask which attributes this terminal actually renders
    #[command(after_help = "\
The attribute flags select what is asked about: `probe --bold` asks about bold
alone. With no attribute flags, asks about the whole family.

Where the terminal implements DECRQSS, this is a real measurement — the attribute
is set and the terminal is asked to report its own SGR state back, so a silently
dropped or faked attribute is caught. Where it does not, the answer falls back to
inference from the terminal's identity, and says so.

Exit status:
  0   every attribute asked about is supported
  1   at least one demonstrably is not
  2   at least one could not be determined")]
    Probe {
        #[command(flatten)]
        attrs: StyleAttrs,

        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },

    /// Walk through the attributes step by step
    Demo {
        /// Run even if the terminal appears not to support them
        #[arg(long)]
        force: bool,
        /// Run only the named step. See --list for the names
        #[arg(long, value_name = "NAME", conflicts_with = "step")]
        only: Option<String>,
        /// Start at this step number instead of the first
        #[arg(long, value_name = "N")]
        step: Option<usize>,
        /// List the steps, with their names, and exit
        #[arg(long)]
        list: bool,
    },
}

/// `escapade dimensions`.
///
/// Bare, it reports the size. Under `probe`, it tests the size against
/// thresholds and answers through the exit status.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true, after_help = "\
Examples:
  escapade dimensions
  escapade dimensions --format json
  escapade dimensions probe --rows-at-least 20 --cols-at-most 40
  escapade dimensions probe --cols-exactly 80 && echo 'classic'")]
pub struct DimensionsCommand {
    #[command(subcommand)]
    pub action: Option<DimensionsAction>,

    /// Output format for the report
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

/// The usual verbs for size reporting.
#[derive(Debug, Subcommand)]
pub enum DimensionsAction {
    /// Explain the size queries, and why pixel dimensions are often unavailable
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },

    /// Test the size against thresholds, or ask whether it can be determined
    #[command(after_help = "\
Thresholds are inclusive, which is why they are spelled --at-least and --at-most
rather than --above and --below: --rows-at-least 20 passes when there are exactly
20 rows. Giving several means all of them must hold.

With no thresholds, this asks the general question instead: can the size be
determined at all, and by which query?

Exit status:
  0   every threshold holds (or, bare, the size is known)
  1   at least one threshold does not hold
  2   a size that was asked about could not be determined — pixel dimensions in
      particular are unavailable on many terminals, and reporting that as a
      failed threshold would be a guess rather than a measurement")]
    Probe {
        /// Pass when there are this many rows or more
        #[arg(long, value_name = "N")]
        rows_at_least: Option<u32>,
        /// Pass when there are this many rows or fewer
        #[arg(long, value_name = "N")]
        rows_at_most: Option<u32>,
        /// Pass when there are exactly this many rows
        #[arg(long, value_name = "N")]
        rows_exactly: Option<u32>,

        /// Pass when there are this many columns or more
        #[arg(long, value_name = "N")]
        cols_at_least: Option<u32>,
        /// Pass when there are this many columns or fewer
        #[arg(long, value_name = "N")]
        cols_at_most: Option<u32>,
        /// Pass when there are exactly this many columns
        #[arg(long, value_name = "N")]
        cols_exactly: Option<u32>,

        /// Pass when the window is this many pixels wide or more
        #[arg(long, value_name = "PX")]
        width_at_least: Option<u32>,
        /// Pass when the window is this many pixels wide or fewer
        #[arg(long, value_name = "PX")]
        width_at_most: Option<u32>,
        /// Pass when the window is exactly this many pixels wide
        #[arg(long, value_name = "PX")]
        width_exactly: Option<u32>,

        /// Pass when the window is this many pixels tall or more
        #[arg(long, value_name = "PX")]
        height_at_least: Option<u32>,
        /// Pass when the window is this many pixels tall or fewer
        #[arg(long, value_name = "PX")]
        height_at_most: Option<u32>,
        /// Pass when the window is exactly this many pixels tall
        #[arg(long, value_name = "PX")]
        height_exactly: Option<u32>,

        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },
}

/// `escapade unscroll`.
///
/// When `less`, `man` or `git log` exits the alternate screen, everything it
/// showed disappears and the prompt lands near the top of an otherwise empty
/// screen. Unscroll pulls those lines back down.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true, after_help = "\
Examples:
  escapade unscroll            # pull back a screenful
  escapade unscroll 5          # pull back five lines
  escapade unscroll --dry-run")]
pub struct UnscrollCommand {
    #[command(subcommand)]
    pub action: Option<UnscrollAction>,

    /// How many lines to pull back. Defaults to filling the screen
    #[arg(value_name = "LINES")]
    pub lines: Option<u16>,

    /// Print the escape sequence in readable form instead of sending it
    #[arg(long)]
    pub dry_run: bool,
}

/// The usual verbs for unscrolling.
#[derive(Debug, Subcommand)]
pub enum UnscrollAction {
    /// Explain the sequence and which terminals implement it
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },
    /// Ask whether this terminal can unscroll
    Probe {
        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },
}

/// `escapade keys`.
///
/// Legacy terminal input cannot distinguish Shift+Enter from Enter, tell a key
/// release from a press, or report which physical key produced a character. The
/// kitty keyboard protocol fixes all of that, one opt-in flag at a time.
#[derive(Debug, Args)]
pub struct KeysCommand {
    #[command(subcommand)]
    pub action: KeysAction,
}

/// What to do with keyboard reporting.
#[derive(Debug, Subcommand)]
pub enum KeysAction {
    /// Print every key event as it arrives, with the bytes that produced it
    #[command(after_help = "\
The `showkey` of modern terminals. Press keys and watch what the terminal sends;
Ctrl-D or Ctrl-C to stop.

Run it twice, once with --legacy, to see what the protocol actually buys you:

  escapade keys watch --legacy     # Shift+Enter is indistinguishable from Enter
  escapade keys watch              # …and now it is not

Flags are the kitty protocol bitmask: 1 disambiguate escape codes, 2 report event
types (this is what makes key release visible), 4 report alternate keys, 8 report
all keys as escape codes, 16 report associated text.")]
    Watch {
        /// Which kitty keyboard protocol flags to enable, as a bitmask
        #[arg(long, value_name = "N", default_value_t = 0b0001_1111)]
        flags: u8,

        /// Enable nothing; show what legacy encoding alone reports
        #[arg(long, conflicts_with = "flags")]
        legacy: bool,

        /// Output format
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },

    /// Explain the protocol, its flags, and which terminals implement it
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },

    /// Ask whether this terminal implements the protocol, and which flags
    Probe {
        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },

    /// Walk through the protocol step by step, pressing keys as you go
    #[command(after_help = "Several steps ask you to press keys and show what \
                            arrived. Return always advances to the next step.")]
    Demo {
        /// Run even if the terminal appears not to support the protocol
        #[arg(long)]
        force: bool,
        /// Run only the named step. See --list for the names
        #[arg(long, value_name = "NAME", conflicts_with = "step")]
        only: Option<String>,
        /// Start at this step number instead of the first
        #[arg(long, value_name = "N")]
        step: Option<usize>,
        /// List the steps, with their names, and exit
        #[arg(long)]
        list: bool,
    },
}

/// `escapade decode`.
///
/// The inverse of the `demo` verb: instead of showing a sequence and then its
/// effect, this takes an effect you already have — a byte stream that rendered
/// wrong — and names the sequences in it.
///
/// It is also how you look at untrusted text before printing it. A log line, a
/// filename or a branch name can carry escape sequences, and `cat -v` shows you
/// `^[[1;31m` without telling you what it does.
#[derive(Debug, Args)]
#[command(after_help = "\
Reads standard input when no FILE is given, so it composes with anything that
produces terminal output.

Examples:
  printf '\\e[1;31mred\\e[0m' | escapade decode
  escapade decode --unicode <<< 'family: 👨‍👩‍👧'
  ./my-tui 2>&1 | escapade decode --sequences-only
  escapade decode --format json capture.bin | jq 'select(.kind == \"osc\")'
  git log --oneline -1 | escapade decode --hex")]
pub struct DecodeCommand {
    /// File to decode. Omitted or `-` reads standard input
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,

    /// Annotate the text between sequences with codepoint names, categories,
    /// widths and grapheme cluster boundaries
    #[arg(long)]
    pub unicode: bool,

    /// Show byte offsets and hex alongside each item
    #[arg(long)]
    pub hex: bool,

    /// Drop the plain text, showing only the escape sequences
    #[arg(long)]
    pub sequences_only: bool,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

/// `escapade doctor`.
///
/// Three sources describe a terminal's capabilities and they routinely disagree:
/// the terminfo entry for `$TERM`, the terminal's own answers to capability
/// queries, and what it does when you actually try. Existing tools pick one and
/// hope. This one reports all three and points at the differences.
#[derive(Debug, Args)]
#[command(after_help = "Exit status:
  0   every check ran and the sources agree
  1   at least one disagreement was found
  2   the checks could not be run (no terminal to ask)

Examples:
  escapade doctor
  escapade doctor --disagreements
  escapade doctor --only truecolor
  escapade doctor --format json | jq '.checks[] | select(.agrees == false)'
  escapade doctor --terminfo xterm-256color   # compare against a different entry")]
pub struct DoctorCommand {
    /// Report only the checks whose sources disagree
    #[arg(long)]
    pub disagreements: bool,

    /// Run only the named check. See --list for the names
    #[arg(long, value_name = "NAME")]
    pub only: Option<String>,

    /// List the checks, with their names, and exit
    #[arg(long)]
    pub list: bool,

    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Compare against this terminfo entry instead of the one for $TERM
    #[arg(long, value_name = "NAME")]
    pub terminfo: Option<String>,

    /// How long to wait for each reply from the terminal
    #[arg(long, value_name = "MS", default_value_t = 1000)]
    pub timeout_ms: u64,

    /// Which channel to talk to the terminal on
    #[arg(long, value_enum, default_value_t = ProbeMode::Any)]
    pub probe_mode: ProbeMode,
}

/// How a `probe` should report itself.
///
/// Absent means silence: the exit status is the answer, so `probe` composes as a
/// shell predicate. Anything else is for a human or a script that wants detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ProbeFormat {
    /// One JSON object per line, for a script to consume
    Json,
    /// Everything the probe saw and concluded, for a person to read
    Debug,
}

/// How to render a command's results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Styled output for a person to read
    #[default]
    Text,
    /// One JSON object per line, for a script to consume
    Json,
}

/// Which file descriptor to send queries on and read replies from.
///
/// Named after chafa's flag of the same purpose. Using stdio is wrong whenever
/// output is redirected — a program in a pipeline still has a terminal, and it is
/// reachable as `/dev/tty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ProbeMode {
    /// Try the controlling terminal, then fall back to stdio
    #[default]
    Any,
    /// Only the controlling terminal, /dev/tty
    Ctty,
    /// Only stdin and stdout
    Stdio,
}

/// Wrapper giving each feature subcommand its own nested action.
#[derive(Debug, Args)]
pub struct FeatureCommand<A: Subcommand> {
    #[command(subcommand)]
    pub action: A,
}

/// What to do with the Glyph Protocol.
#[derive(Debug, Subcommand)]
pub enum GlyphAction {
    /// Explain the protocol: what it does, its escape sequences, who uses it,
    /// and which terminals support it
    #[command(after_help = "The support matrix lives in data/features/glyph-protocol.toml — \
                            corrections welcome.")]
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },

    /// Ask whether this terminal supports the protocol, or specific codepoints
    #[command(after_help = "\
Silent by default: the exit status is the answer, so this composes as a shell
predicate. 0 supported, 1 not supported, 2 could not be determined.

With no CODEPOINTS, probes the protocol itself. With CODEPOINTS, asks which of
them the terminal can actually draw — a comma-separated list of codepoints and
inclusive ranges, each written U+1F60A, 0x1F60A or bare 1F60A, in any case; an
item that is not valid hex but is a single character is taken literally.

The capability query is sent alongside a Primary Device Attributes request, so a
terminal that stays silent can be told apart from one that is merely slow. Note
that 2 is not a softer 1: font coverage does not depend on this protocol, so an
unanswered codepoint may still render.

Examples:
  escapade glyph probe && echo 'custom glyphs available'
  escapade glyph probe --format debug
  escapade glyph probe U+1F60A,U+1F60C-U+1F60F --format debug
  escapade glyph probe U+E0A0 --format json | jq -r .verdict")]
    Probe {
        /// Codepoints to ask about. Omit to probe the protocol itself
        #[arg(value_name = "CODEPOINTS")]
        target: Option<String>,

        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,

        /// With CODEPOINTS, also show general category, width and emoji status
        #[arg(long, short)]
        long: bool,
    },

    /// Walk through the protocol step by step, rendering real custom glyphs
    #[command(after_help = "Space or Enter advances, p goes back, q quits.\n\
                            The demo takes over the alternate screen and restores your \
                            terminal on exit.")]
    Demo {
        /// Run even if the terminal appears not to support the protocol
        #[arg(long)]
        force: bool,

        /// Run only the named step. See --list for the names
        #[arg(long, value_name = "NAME", conflicts_with = "step")]
        only: Option<String>,

        /// Start at this step number instead of the first
        #[arg(long, value_name = "N")]
        step: Option<usize>,

        /// List the steps, with their names, and exit
        #[arg(long)]
        list: bool,
    },
}

/// What to do with the kitty graphics protocol.
#[derive(Debug, Subcommand)]
#[expect(
    clippy::large_enum_variant,
    reason = "`render` genuinely takes many options; the enum is built once at \
              startup, so the indirection a Box would add costs more in \
              readability than the bytes are worth"
)]
pub enum GraphicsAction {
    /// Explain the protocol: transmission, placement, animation, and which
    /// terminals support it
    #[command(after_help = "The support matrix lives in data/features/kitty-graphics.toml — \
                            corrections welcome.")]
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },

    /// Ask this terminal whether it speaks the protocol, and show its answer
    Probe {
        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },

    /// Walk through the protocol step by step, displaying real images
    #[command(after_help = "Space or Enter advances, p goes back, q quits.")]
    Demo {
        /// Run even if the terminal appears not to support the protocol
        #[arg(long)]
        force: bool,

        /// Run only the named step. See --list for the names
        #[arg(long, value_name = "NAME", conflicts_with = "step")]
        only: Option<String>,

        /// Start at this step number instead of the first
        #[arg(long, value_name = "N")]
        step: Option<usize>,

        /// List the steps, with their names, and exit
        #[arg(long)]
        list: bool,
    },

    /// Display an image, a diagram, a profile or a generated shape in the terminal
    #[command(after_help = "\
The input type is sniffed in three stages: the file extension first, then the
leading bytes, and finally — for text that looks like profiler output — inferno's
format guesser. --input-type overrides all of it.

  .png                    sent as-is, no re-encoding
  .jpg .jpeg .gif .webp   decoded and sent as pixels
  .svg                    rasterized at --dpi
  .dot .gv                laid out with graphviz, then rasterized
  .folded .collapsed      collapsed stacks, rendered as a flame graph
  perf/DTrace/sample/…    raw profiler output, collapsed then rendered

FILE may be `-` to read standard input, which is also the default when stdin is
a pipe. That is the point of the sniffer: piped input has no file name.

Examples:
  escapade graphics render diagram.dot --cols 60 --layout neato
  escapade graphics render logo.svg --dpi 192
  escapade graphics render --shape color-wheel --cols 20
  escapade graphics render photo.png --rows 15 --fit cover
  perf script | escapade graphics render - --flamegraph-title 'hot path'
  escapade graphics render out.folded --flamegraph-colors rust --cols 100")]
    Render {
        /// Image, vector, diagram or profile to display. `-` reads standard input
        #[arg(value_name = "FILE")]
        file: Option<PathBuf>,

        /// Force the input type instead of sniffing it
        #[arg(long, value_enum, default_value_t = InputKind::Auto)]
        input_type: InputKind,

        /// Render a generated image instead of reading a file
        #[arg(long, value_enum, conflicts_with = "file")]
        shape: Option<Shape>,

        /// Width in terminal cells
        #[arg(long, value_name = "N")]
        cols: Option<u16>,

        /// Height in terminal cells
        #[arg(long, value_name = "N")]
        rows: Option<u16>,

        /// How to fit the image into the requested area
        #[arg(long, value_enum, default_value_t = Fit::Contain)]
        fit: Fit,

        /// Scale the source before fitting, as a multiplier
        #[arg(long, value_name = "FACTOR")]
        scale: Option<f32>,

        /// Rasterization resolution for vector inputs
        #[arg(long, default_value_t = 96.0, value_name = "DPI")]
        dpi: f32,

        /// Graphviz layout engine for .dot input
        #[arg(long, default_value = "dot", value_name = "ENGINE")]
        layout: String,

        /// Color behind transparent pixels, as #rrggbb, a name, or 'none'
        #[arg(long, value_name = "COLOR")]
        background: Option<String>,

        /// Stacking order relative to text; negative sits behind
        #[arg(long, allow_hyphen_values = true, value_name = "N")]
        z: Option<i32>,

        /// Anchor the image to cells with Unicode placeholders, so it scrolls
        /// with the text instead of floating
        #[arg(long)]
        placeholder: bool,

        /// Print the escape sequence in readable form instead of sending it
        #[arg(long)]
        dry_run: bool,

        /// Render even if the terminal appears not to support the protocol
        #[arg(long)]
        force: bool,

        #[command(flatten)]
        flamegraph: FlamegraphArgs,
    },
}

/// Flame-graph options, passed through to `inferno`.
///
/// Only the ones that change what a *reader* sees are exposed — inferno's
/// differential and palette-map machinery has no meaning for a one-shot render
/// into a terminal. They are grouped so `--help` keeps them together and so they
/// can be ignored wholesale when the input is not a profile.
#[derive(Debug, Args)]
#[command(next_help_heading = "Flame graph options")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "these are command line flags, and each one is independently \
              present or absent"
)]
pub struct FlamegraphArgs {
    /// Title across the top of the graph
    #[arg(long = "flamegraph-title", value_name = "TEXT")]
    pub title: Option<String>,

    /// Subtitle under the title
    #[arg(long = "flamegraph-subtitle", value_name = "TEXT")]
    pub subtitle: Option<String>,

    /// Color palette
    #[arg(long = "flamegraph-colors", value_enum, value_name = "PALETTE")]
    pub colors: Option<FlamegraphPalette>,

    /// Draw as an icicle graph, with the root at the top
    #[arg(long = "flamegraph-inverted")]
    pub inverted: bool,

    /// Reverse stack order, merging from the leaves down
    #[arg(long = "flamegraph-reverse")]
    pub reverse: bool,

    /// Keep samples in time order instead of merging them
    #[arg(long = "flamegraph-flame-chart")]
    pub flame_chart: bool,

    /// Colour frames by their own name hash, so they stay stable across runs
    #[arg(long = "flamegraph-hash")]
    pub hash: bool,

    /// Smallest frame to draw, as a percentage of the total
    #[arg(long = "flamegraph-min-width", value_name = "PERCENT")]
    pub min_width: Option<f64>,

    /// What one sample counts as, for the tooltip and totals
    #[arg(long = "flamegraph-count-name", value_name = "NAME")]
    pub count_name: Option<String>,

    /// Height of one frame, in pixels before rasterization
    #[arg(long = "flamegraph-frame-height", value_name = "PX")]
    pub frame_height: Option<usize>,

    /// Text size, in pixels before rasterization
    #[arg(long = "flamegraph-font-size", value_name = "PX")]
    pub font_size: Option<usize>,

    /// Width of the generated SVG, before it is fitted to the cell grid
    #[arg(long = "flamegraph-image-width", value_name = "PX")]
    pub image_width: Option<usize>,

    /// Note shown alongside the title
    #[arg(long = "flamegraph-notes", value_name = "TEXT")]
    pub notes: Option<String>,
}

/// How to interpret the input to `escapade graphics render`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum InputKind {
    /// Sniff it: extension, then leading bytes, then inferno's format guesser
    #[default]
    Auto,
    /// PNG image
    Png,
    /// JPEG image
    Jpeg,
    /// GIF image
    Gif,
    /// WebP image
    Webp,
    /// SVG vector image
    Svg,
    /// Graphviz DOT source
    Dot,
    /// Collapsed ("folded") stacks, one `frame;frame count` per line
    Folded,
    /// Output of `perf script`
    Perf,
    /// DTrace stack output
    Dtrace,
    /// macOS `sample` output
    Sample,
    /// Intel VTune `amplxe-cl` output
    Vtune,
    /// Visual Studio profiler CSV
    Vsprof,
}

/// Flame-graph color palettes offered by inferno.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum FlamegraphPalette {
    /// Warm reds and yellows — inferno's default
    Hot,
    /// Greens, for memory profiles
    Mem,
    /// Blues, for I/O profiles
    Io,
    /// Single-hue red
    Red,
    /// Single-hue green
    Green,
    /// Single-hue blue
    Blue,
    /// Single-hue aqua
    Aqua,
    /// Single-hue yellow
    Yellow,
    /// Single-hue purple
    Purple,
    /// Single-hue orange
    Orange,
    /// Colour by language for Java stacks
    Java,
    /// Colour by language for JavaScript stacks
    Js,
    /// Colour by language for Perl stacks
    Perl,
    /// Colour by language for Python stacks
    Python,
    /// Colour by language for Rust stacks
    Rust,
    /// Highlight wakeup stacks
    Wakeup,
}

/// `escapade hyperlink`.
///
/// Unlike the other features, the useful thing here is a one-liner rather than a
/// walkthrough, so emitting a link is the top-level form and the usual verbs are
/// subcommands underneath it:
///
/// ```text
/// escapade hyperlink --url https://example.com "click me"
/// escapade hyperlink explain
/// ```
///
/// `subcommand_negates_reqs` is what lets `--url` be required for the first form
/// without also being required for the second.
#[derive(Debug, Args)]
#[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
pub struct HyperlinkCommand {
    #[command(subcommand)]
    pub action: Option<HyperlinkAction>,

    /// Target of the link
    #[arg(long, value_name = "URL", required = true)]
    pub url: Option<String>,

    /// Text to show as the link. Joined with spaces; read from standard input
    /// when omitted, and `-` reads it even from a terminal
    #[arg(value_name = "TEXT")]
    pub text: Vec<String>,

    /// Link id, so separate runs of cells highlight together on hover
    #[arg(long, value_name = "ID")]
    pub id: Option<String>,

    /// Print the escape sequence in readable form instead of emitting it
    #[arg(long)]
    pub dry_run: bool,
}

/// `escapade notification`.
///
/// Same shape as [`HyperlinkCommand`]: raising a notification is the top-level
/// form, and the usual verbs are subcommands.
///
/// ```text
/// escapade notification --title "Build finished" "42 tests passed"
/// escapade notification explain
/// ```
#[derive(Debug, Args)]
#[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
pub struct NotificationCommand {
    #[command(subcommand)]
    pub action: Option<NotificationAction>,

    /// Notification body. Joined with spaces; read from standard input when
    /// omitted, and `-` reads it even from a terminal
    #[arg(value_name = "BODY")]
    pub body: Vec<String>,

    /// Notification title, shown above the body
    #[arg(long, short, value_name = "TITLE")]
    pub title: Option<String>,

    /// Which protocol to use
    #[arg(long, value_enum, default_value_t = NotifyProtocol::Auto)]
    pub protocol: NotifyProtocol,

    /// How insistently to present it, where the protocol allows saying
    #[arg(long, value_enum)]
    pub urgency: Option<Urgency>,

    /// Notification id, so a later notification can replace this one
    #[arg(long, value_name = "ID")]
    pub id: Option<String>,

    /// Print the escape sequence in readable form instead of emitting it
    #[arg(long)]
    pub dry_run: bool,
}

/// Which desktop-notification escape sequence to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum NotifyProtocol {
    /// Pick the richest protocol the terminal appears to support
    #[default]
    Auto,
    /// kitty's desktop notification protocol (OSC 99) — titles, urgency, ids
    Kitty,
    /// The original one-liner (OSC 9) — body text only
    Osc9,
    /// The rxvt-unicode form (OSC 777) — title and body
    Osc777,
}

/// How insistently a notification should be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Urgency {
    /// Show it quietly
    Low,
    /// The default
    Normal,
    /// Demand attention
    Critical,
}

/// The usual verbs for the notification feature.
#[derive(Debug, Subcommand)]
pub enum NotificationAction {
    /// Explain terminal notifications: the competing sequences, and what each
    /// one can express
    #[command(after_help = "The support matrix lives in data/features/notifications.toml — \
                            corrections welcome.")]
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },

    /// Check which notification protocols this terminal appears to support
    Probe {
        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },

    /// Walk through the notification protocols step by step
    #[command(after_help = "Space or Enter advances, p goes back, q quits.")]
    Demo {
        /// Run even if the terminal appears not to support notifications
        #[arg(long)]
        force: bool,

        /// Run only the named step. See --list for the names
        #[arg(long, value_name = "NAME", conflicts_with = "step")]
        only: Option<String>,

        /// Start at this step number instead of the first
        #[arg(long, value_name = "N")]
        step: Option<usize>,

        /// List the steps, with their names, and exit
        #[arg(long)]
        list: bool,
    },
}

/// The usual verbs for the hyperlink feature.
#[derive(Debug, Subcommand)]
pub enum HyperlinkAction {
    /// Explain OSC 8: the sequence, its parameters, and which terminals support it
    #[command(after_help = "The support matrix lives in data/features/hyperlinks.toml — \
                            corrections welcome.")]
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },

    /// Check whether this terminal appears to support hyperlinks
    Probe {
        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,
    },

    /// Walk through OSC 8 step by step, emitting real links
    #[command(after_help = "Space or Enter advances, p goes back, q quits.")]
    Demo {
        /// Run even if the terminal appears not to support hyperlinks
        #[arg(long)]
        force: bool,

        /// Run only the named step. See --list for the names
        #[arg(long, value_name = "NAME", conflicts_with = "step")]
        only: Option<String>,

        /// Start at this step number instead of the first
        #[arg(long, value_name = "N")]
        step: Option<usize>,

        /// List the steps, with their names, and exit
        #[arg(long)]
        list: bool,
    },
}

/// What to do with grapheme clustering.
#[derive(Debug, Subcommand)]
pub enum GraphemeAction {
    /// Explain grapheme clustering: why terminals get it wrong, what mode 2027
    /// changes, and which terminals implement it
    #[command(after_help = "The support matrix lives in data/features/grapheme-clustering.toml — \
                            corrections welcome.")]
    Explain {
        /// Skip the support matrix
        #[arg(long)]
        no_matrix: bool,
    },

    /// Ask whether this terminal clusters graphemes, or measure a given sequence
    #[command(after_help = "\
Silent by default: the exit status is the answer, so this composes as a shell
predicate. 0 supported, 1 not supported, 2 could not be determined.

With no CODEPOINTS, probes mode 2027 itself. With CODEPOINTS, measures whether
the terminal treats that sequence as a single character. The syntax matches
`escapade glyph probe`, except that the list is read as a run of text rather than
a set, so repeats are kept — a family emoji legitimately repeats U+200D.

Examples:
  escapade grapheme probe && echo 'clustering available'
  escapade grapheme probe --format debug
  escapade grapheme probe U+1F9D1,U+200D,U+1F33E --format debug")]
    Probe {
        /// Codepoints forming the sequence to measure. Omit to probe the mode
        #[arg(value_name = "CODEPOINTS")]
        target: Option<String>,

        /// Report the findings instead of answering through the exit status alone
        #[arg(long, value_enum)]
        format: Option<ProbeFormat>,

        /// With CODEPOINTS, also show a per-codepoint breakdown
        #[arg(long, short)]
        long: bool,
    },

    /// Render a sequence, with grapheme clustering enabled
    #[command(after_help = "\
Examples:
  escapade grapheme render U+1F9D1,U+200D,U+1F33E
  escapade grapheme render --disabled U+1F9D1,U+200D,U+1F33E
  escapade grapheme render --debug U+1F9D1,U+200D,U+1F33E

By default this turns grapheme clustering on for the duration of the render, so
the sequence occupies the cells the terminal thinks one character should.
--disabled skips that, which is the interesting comparison: the same bytes, with
the terminal advancing the cursor per codepoint instead.")]
    Render {
        /// Codepoints forming the sequence to render
        #[arg(value_name = "CODEPOINTS")]
        spec: String,

        /// Do not enable grapheme clustering first
        #[arg(long)]
        disabled: bool,

        /// Break the sequence down codepoint by codepoint, and measure it
        #[arg(long)]
        debug: bool,
    },
}

/// Images `escapade graphics render --shape` can generate without an input file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Shape {
    /// A smooth two-axis color gradient
    Gradient,
    /// An alpha-checkerboard, for seeing how transparency composites
    Checkerboard,
    /// An HSV color wheel
    ColorWheel,
    /// The Mandelbrot set
    Mandelbrot,
    /// Concentric rings, for spotting resampling artefacts
    Rings,
    /// A test card with resolution and alignment markers
    TestCard,
}

/// How to fit an image into the requested cell area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Fit {
    /// Scale to fit inside the area, preserving aspect ratio
    #[default]
    Contain,
    /// Fill the area, preserving aspect ratio, cropping the overflow
    Cover,
    /// Fill the area exactly, distorting the aspect ratio
    Stretch,
    /// Do not scale; one image pixel per screen pixel
    Native,
}
