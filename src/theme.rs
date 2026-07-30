//! Colors, styling and markdown rendering.
//!
//! Prose comes out of the feature database as markdown and is rendered by
//! [`termimad`]. Tables that need per-cell color — the support matrix above all
//! — are drawn by hand instead, because a skin applies one style to the whole
//! table and a support matrix is only readable when ✔ and ✘ differ at a glance.
//!
//! Colors are 256-color SGR sequences rather than truecolor, so they survive
//! terminals that only do 256, and every one of them routes through
//! [`Theme::paint`] so `--no-color` and `NO_COLOR` genuinely disable styling.

use std::{env, io::IsTerminal as _};

use termimad::{
    MadSkin,
    crossterm::style::{Attribute, Color},
    gray,
};

/// Widest line we will render. Long measures are hard to read, and the demo
/// steps are authored assuming they get at least this much.
const MAX_WIDTH: usize = 96;

/// Fallback width when the terminal will not say how wide it is.
const FALLBACK_WIDTH: usize = 80;

/// A semantic color role. Naming the role rather than the color keeps the
/// palette in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// Feature names and top-level titles.
    Title,
    /// Structural highlights: headings, labels, the protocol's own vocabulary.
    Accent,
    /// Supported, succeeded, safe.
    Good,
    /// Conditional: behind a flag, needs passthrough, unverified.
    Warn,
    /// Unsupported or failed.
    Bad,
    /// Secondary text that should recede.
    Muted,
    /// Escape sequences and byte values.
    Code,
    /// Emphasis without a hue.
    Strong,
    /// URLs.
    Link,
}

impl Ink {
    /// The SGR parameters for this role, without the surrounding `CSI`/`m`.
    const fn sgr(self) -> &'static str {
        match self {
            Self::Title => "1;38;5;214",
            Self::Accent => "38;5;44",
            Self::Good => "38;5;77",
            Self::Warn => "38;5;179",
            Self::Bad => "38;5;167",
            Self::Muted => "38;5;244",
            Self::Code => "38;5;222",
            Self::Strong => "1",
            Self::Link => "4;38;5;39",
        }
    }

    /// The matching crossterm color, for the markdown skin.
    const fn color(self) -> Color {
        match self {
            Self::Title => Color::AnsiValue(214),
            Self::Accent => Color::AnsiValue(44),
            Self::Good => Color::AnsiValue(77),
            Self::Warn => Color::AnsiValue(179),
            Self::Bad => Color::AnsiValue(167),
            Self::Muted => Color::AnsiValue(244),
            Self::Code => Color::AnsiValue(222),
            Self::Strong => Color::AnsiValue(255),
            Self::Link => Color::AnsiValue(39),
        }
    }
}

/// Rendering configuration for one run.
#[derive(Debug)]
pub struct Theme {
    /// Whether to emit styling at all.
    color: bool,
    /// Usable text width.
    width: usize,
    /// Markdown skin, styled to match [`Ink`].
    skin: MadSkin,
}

impl Theme {
    /// Builds a theme, honouring `--no-color`, `NO_COLOR` and whether stdout is
    /// a terminal.
    #[must_use]
    pub fn new(no_color_flag: bool) -> Self {
        let color = !no_color_flag
            && env::var_os("NO_COLOR").is_none()
            && std::io::stdout().is_terminal();
        let width = detect_width();
        let skin = if color { styled_skin() } else { MadSkin::no_style() };
        Self { color, width, skin }
    }

    /// A theme that never styles anything.
    ///
    /// Only tests need to construct one directly; real runs get the same effect
    /// from [`Theme::new`] when `--no-color` is passed, `NO_COLOR` is set, or
    /// stdout is not a terminal.
    #[cfg(test)]
    #[must_use]
    pub fn plain() -> Self {
        Self {
            color: false,
            width: FALLBACK_WIDTH,
            skin: MadSkin::no_style(),
        }
    }

    /// Usable text width.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Whether styling is enabled.
    #[must_use]
    pub fn color(&self) -> bool {
        self.color
    }

    /// Wraps `text` in the SGR sequence for `ink`, or returns it untouched when
    /// color is off.
    ///
    /// The result contains escape sequences, so never measure it with `len()`
    /// — use [`Theme::visible_width`].
    #[must_use]
    pub fn paint(&self, text: &str, ink: Ink) -> String {
        if self.color {
            format!("\u{1b}[{}m{text}\u{1b}[0m", ink.sgr())
        } else {
            text.to_owned()
        }
    }

    /// Renders markdown to stdout, wrapped to the theme width.
    pub fn markdown(&self, source: &str) {
        print!("{}", self.skin.text(source, Some(self.width)));
    }

    /// Renders markdown indented by `indent` columns, wrapped to fit.
    ///
    /// Used for notes hanging under a hand-drawn table row: the markdown
    /// renderer handles wrapping and inline code, and the indent is applied
    /// afterwards so it cannot confuse the wrap width.
    #[must_use]
    pub fn markdown_indented(&self, source: &str, indent: usize) -> String {
        let inner = self.width.saturating_sub(indent).max(24);
        let rendered = self.skin.text(source, Some(inner)).to_string();
        let pad = " ".repeat(indent);
        rendered
            .lines()
            .map(|line| {
                if line.trim().is_empty() {
                    String::new()
                } else {
                    format!("{pad}{line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Renders a single line of inline markdown — bold, italics, inline code —
    /// without block-level wrapping.
    #[must_use]
    pub fn inline(&self, source: &str) -> String {
        self.skin.inline(source).to_string()
    }

    /// A section heading: the title, then a rule under just the title.
    pub fn section(&self, title: &str) {
        println!();
        println!("{}", self.paint(title, Ink::Accent));
        println!(
            "{}",
            self.paint(&"─".repeat(title.chars().count()), Ink::Muted)
        );
    }
}

/// Builds the markdown skin from the [`Ink`] palette.
fn styled_skin() -> MadSkin {
    let mut skin = MadSkin::default();

    skin.headers[0].compound_style.set_fg(Ink::Title.color());
    skin.headers[0].compound_style.add_attr(Attribute::Bold);
    skin.headers[0].align = termimad::Alignment::Left;
    skin.headers[1].compound_style.set_fg(Ink::Accent.color());
    skin.headers[2].compound_style.set_fg(Ink::Strong.color());

    skin.bold.set_fg(Ink::Strong.color());
    skin.bold.add_attr(Attribute::Bold);
    skin.italic.set_fg(Ink::Warn.color());
    skin.inline_code.set_fgbg(Ink::Code.color(), gray(2));
    skin.code_block.compound_style.set_fgbg(Ink::Code.color(), gray(2));
    skin.bullet = termimad::StyledChar::from_fg_char(Ink::Accent.color(), '•');
    skin.quote_mark.set_fg(Ink::Accent.color());
    skin.table.compound_style.set_fg(gray(9));
    skin.horizontal_rule = termimad::StyledChar::from_fg_char(gray(6), '─');

    skin
}

/// Terminal width, clamped to something readable.
fn detect_width() -> usize {
    let detected =
        crossterm::terminal::size().map_or(FALLBACK_WIDTH, |(cols, _)| usize::from(cols));
    detected.clamp(40, MAX_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_theme_never_emits_escapes() {
        let theme = Theme::plain();
        assert_eq!(theme.paint("hello", Ink::Good), "hello");
        assert!(!theme.paint("x", Ink::Bad).contains('\u{1b}'));
    }

    #[test]
    fn every_ink_role_has_distinct_sgr() {
        let roles = [
            Ink::Title,
            Ink::Accent,
            Ink::Good,
            Ink::Warn,
            Ink::Bad,
            Ink::Muted,
            Ink::Code,
            Ink::Strong,
            Ink::Link,
        ];
        let mut seen: Vec<&str> = roles.iter().map(|ink| ink.sgr()).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "two roles share an SGR sequence");
    }
}
