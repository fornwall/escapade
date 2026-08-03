//! The report: what the terminal said, what it means, and `--is-dark`.
//!
//! Two rules shape the whole of this module.
//!
//! **Show the raw reply next to the normalised value.** `rgb:f/0/0` and
//! `#ff0000` on the same line is the only way a reader can check the claim that
//! the first means the second, and it is the only useful thing to put in a bug
//! report. Hiding the reply behind a tidy hex string is how the digit-width trap
//! stayed invisible for as long as it did.
//!
//! **Never guess.** A colour that was not reported is printed as *no answer*, not
//! as black, and the light/dark verdict is withheld entirely rather than defaulted
//! when the background is unknown. That is what the third exit status is for.

use std::{fmt::Write as _, process::ExitCode};

use anyhow::Result;

use crate::{
    cli::{ColorCommand, OutputFormat},
    features::color::{
        notify::Scheme,
        query::{Answer, Answers, Target, detect, interactive},
        value::{Reported, Rgb},
    },
    features::wrap,
    theme::{Ink, Theme},
};

/// Contrast ratio WCAG 2 asks for on body text. Shown next to the measured one so
/// the number means something.
const WCAG_AA_BODY: f64 = 4.5;

/// Palette entries per row in the text report. Four fits `NNN ▉ #rrggbb` twice
/// over in 80 columns with room for the gutter.
const PALETTE_COLUMNS: usize = 4;

/// How the light/dark answer was arrived at.
///
/// Kept separate from the answer itself because the two have very different
/// standing: one is the terminal stating its own scheme, the other is arithmetic
/// on a colour. Both are reported, and the report says which it used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The terminal reported its colour scheme itself.
    Reported,
    /// Computed from the background colour's relative luminance.
    Luminance,
}

impl Basis {
    /// Stable machine-readable name, for `--format json`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Luminance => "luminance",
        }
    }

    /// How the answer was reached, for a person.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Reported => "the terminal said so itself (DEC mode 2031 colour-scheme report)",
            Self::Luminance => "relative luminance of the reported background",
        }
    }
}

/// The light/dark determination, and what it rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    /// Whether the background is dark.
    pub dark: bool,
    /// How that was decided.
    pub basis: Basis,
}

impl Verdict {
    /// `dark` or `light`.
    #[must_use]
    pub fn label(self) -> &'static str {
        if self.dark { "dark" } else { "light" }
    }

    /// The exit status `--is-dark` reports: 0 dark, 1 light.
    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        if self.dark {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        }
    }
}

/// Decides light or dark, preferring the terminal's own word over arithmetic.
///
/// A terminal that implements colour-scheme reporting is telling us what its
/// *theme* is, which is the question actually being asked. Luminance of the
/// background is an excellent proxy — and the only option nearly everywhere — but
/// it is still a proxy: a terminal can be set to a light theme with a dark
/// translucent background, and its own report is right where the arithmetic is
/// wrong.
///
/// Returns `None` when neither is available, which is the case `--is-dark`
/// answers with exit status 2.
#[must_use]
pub fn verdict(scheme: Option<Scheme>, background: Option<Rgb>) -> Option<Verdict> {
    if let Some(scheme) = scheme {
        return Some(Verdict {
            dark: scheme.is_dark(),
            basis: Basis::Reported,
        });
    }
    background.map(|rgb| Verdict {
        dark: rgb.is_dark(),
        basis: Basis::Luminance,
    })
}

/// Runs the bare `escapade color` report, and `--is-dark`.
///
/// # Errors
///
/// Fails if the terminal cannot be set up.
pub fn run(theme: &Theme, command: &ColorCommand) -> Result<ExitCode> {
    let targets = Target::resolve(&command.query, command.index);

    // The one thing that must never happen is a hang, and the one thing that must
    // never be printed is a guess. Both are the same check: without a terminal on
    // both ends there is nobody to ask.
    if !interactive() {
        return Ok(no_terminal(theme, command));
    }

    // `--is-dark` needs the background whatever else was asked for, since its
    // whole answer comes from it.
    let mut targets = targets;
    if command.is_dark && !targets.contains(&Target::Background) {
        targets.push(Target::Background);
    }

    let answers = detect(&targets)?;
    let scheme = None;
    let verdict = verdict(scheme, answers.get(Target::Background).map(|r| r.rgb));

    if command.is_dark {
        // Silent by design: the answer is the exit status, so this composes as a
        // shell predicate without a subshell to swallow output.
        return Ok(verdict.map_or_else(|| ExitCode::from(2), Verdict::exit_code));
    }

    match command.format {
        OutputFormat::Text => text_report(theme, &answers, verdict),
        OutputFormat::Json => print!("{}", json_report(&answers, verdict)),
    }

    // Nothing answered is not a report, it is a failure to obtain one, and a
    // script should be able to tell without parsing.
    Ok(if answers.any_answered() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}

/// The refusal when there is no terminal to ask.
///
/// Exit status 2 — inconclusive — rather than 1, because nothing was learned about
/// the terminal at all. Guessing from `$COLORFGBG` or `$TERM` here is precisely
/// the behaviour this feature exists to replace.
fn no_terminal(theme: &Theme, command: &ColorCommand) -> ExitCode {
    let reason = "cannot ask: stdin and stdout are not both terminals, so the query has nowhere \
                  to go and the reply has nowhere to come back from";
    if command.is_dark {
        // Silent, as documented. Exit status 2 is the answer.
    } else if command.format == OutputFormat::Json {
        println!(
            "{{\"feature\":\"color\",\"record\":\"verdict\",\"dark\":null,\"basis\":null,\
             \"interactive\":false,\"reason\":\"{}\"}}",
            escape_json(reason)
        );
    } else {
        println!();
        println!(
            "  {} {}",
            theme.paint("?", Ink::Warn),
            theme.paint("no answer", Ink::Warn)
        );
        for line in wrap(reason, theme.width().saturating_sub(4)) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
        println!();
        println!(
            "    {}",
            theme.paint("Run it directly rather than through a pipe.", Ink::Muted)
        );
        println!();
    }
    ExitCode::from(2)
}

// ---------------------------------------------------------------------------
// The human-readable report
// ---------------------------------------------------------------------------

/// Prints the report a person reads.
fn text_report(theme: &Theme, answers: &Answers, verdict: Option<Verdict>) {
    println!();
    println!("  {}", theme.paint("Terminal colours", Ink::Title));

    let (palette, dynamic): (Vec<&Answer>, Vec<&Answer>) = answers
        .answers
        .iter()
        .partition(|answer| answer.target.index().is_some());

    if !dynamic.is_empty() {
        println!();
        let width = dynamic
            .iter()
            .map(|answer| answer.target.label().chars().count())
            .max()
            .unwrap_or(0);
        for answer in &dynamic {
            colour_line(theme, answer, width);
        }
    }

    if !palette.is_empty() {
        palette_block(theme, &palette);
    }

    summary(theme, answers, verdict);
    println!();
}

/// One dynamic colour: label, swatch, normalised value, the reply verbatim.
fn colour_line(theme: &Theme, answer: &Answer, width: usize) {
    let label = answer.target.label();
    let padding = " ".repeat(width.saturating_sub(label.chars().count()));
    let Some(reported) = &answer.reported else {
        println!(
            "  {}{padding}  {}",
            theme.paint(&label, Ink::Strong),
            theme.paint("no answer", Ink::Muted)
        );
        return;
    };

    let rgb = reported.rgb;
    println!(
        "  {}{padding}  {} {}   {}   {}",
        theme.paint(&label, Ink::Strong),
        swatch(theme, rgb),
        theme.paint(&rgb.hex(), Ink::Code),
        theme.paint(&reported.raw, Ink::Accent),
        theme.paint(&format!("luminance {:.3}", rgb.luminance()), Ink::Muted),
    );
    // A one-digit reply is exactly the case the trap bites on, so it is worth
    // spelling out what it expanded to rather than leaving the reader to trust it.
    if reported.raw.starts_with("rgb:") && reported.raw != rgb.to_xparsecolor() {
        println!(
            "  {padding}{}  {}",
            " ".repeat(width),
            theme.paint(
                &format!("= {} at full width", rgb.to_xparsecolor()),
                Ink::Muted
            )
        );
    }
}

/// The palette, four entries to a row.
///
/// A 256-entry dump is unreadable as 256 lines and useless as a picture, so it is
/// a grid: index, swatch, hex, and the raw reply only when it differs from the
/// canonical form.
fn palette_block(theme: &Theme, palette: &[&Answer]) {
    println!();
    println!("  {}", theme.paint("palette", Ink::Accent));
    println!();
    let mut row = String::new();
    for (position, answer) in palette.iter().enumerate() {
        let index = answer.target.index().unwrap_or(0);
        let cell = match &answer.reported {
            Some(reported) => format!(
                "{:>4} {} {}",
                theme.paint(&index.to_string(), Ink::Muted),
                swatch(theme, reported.rgb),
                theme.paint(&reported.rgb.hex(), Ink::Code)
            ),
            None => format!(
                "{:>4}   {}",
                theme.paint(&index.to_string(), Ink::Muted),
                theme.paint("no answer", Ink::Muted)
            ),
        };
        row.push_str("  ");
        row.push_str(&cell);
        if (position + 1) % PALETTE_COLUMNS == 0 {
            println!("  {row}");
            row.clear();
        }
    }
    if !row.is_empty() {
        println!("  {row}");
    }
}

/// The closing block: the verdict, the contrast, and why anything is missing.
fn summary(theme: &Theme, answers: &Answers, verdict: Option<Verdict>) {
    let foreground = answers.get(Target::Foreground).map(|r| r.rgb);
    let background = answers.get(Target::Background).map(|r| r.rgb);

    if let (Some(fg), Some(bg)) = (foreground, background) {
        let ratio = fg.contrast_ratio(bg);
        let ink = if ratio >= WCAG_AA_BODY {
            Ink::Good
        } else {
            Ink::Warn
        };
        println!();
        println!(
            "  {} {}   {}",
            theme.paint("contrast", Ink::Strong),
            theme.paint(&format!("{ratio:.1} : 1"), ink),
            theme.paint(
                &format!("WCAG AA wants {WCAG_AA_BODY:.1} : 1 for body text"),
                Ink::Muted
            )
        );
    }

    println!();
    if let Some(verdict) = verdict {
        let ink = if verdict.dark { Ink::Accent } else { Ink::Warn };
        println!(
            "  {} {}   {}",
            theme.paint("scheme", Ink::Strong),
            theme.paint(verdict.label(), ink),
            theme.paint(&format!("({})", verdict.basis.describe()), Ink::Muted)
        );
    } else {
        println!(
            "  {} {}",
            theme.paint("scheme", Ink::Strong),
            theme.paint("unknown — the terminal would not say", Ink::Warn)
        );
        for line in wrap(unanswered_reason(answers), theme.width().saturating_sub(4)) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
    }

    let missing: Vec<String> = answers
        .answers
        .iter()
        .filter(|answer| !answer.answered())
        .map(|answer| answer.target.label())
        .collect();
    if !missing.is_empty() && answers.any_answered() {
        println!();
        let text = format!(
            "no answer for {}. {}",
            missing.join(", "),
            if answers.silence_is_conclusive() {
                "The device-attributes barrier came back, so this terminal really does not \
                 implement those queries."
            } else {
                "The barrier did not come back either, so this may be a swallowed reply \
                 rather than a missing feature."
            }
        );
        for line in wrap(&text, theme.width().saturating_sub(4)) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    }
}

/// Why no verdict could be reached, in one sentence a user can act on.
fn unanswered_reason(answers: &Answers) -> &'static str {
    if !answers
        .answers
        .iter()
        .any(|a| a.target == Target::Background)
    {
        "The background was not among the colours asked about, and light or dark is a statement \
         about the background."
    } else if answers.silence_is_conclusive() {
        "The terminal answered the device-attributes barrier but not the background query, so it \
         genuinely does not implement OSC 11. Nothing else can be substituted for it: $COLORFGBG \
         is set by almost nothing and is not updated when the theme changes."
    } else if let Some(multiplexer) = answers.multiplexer {
        match multiplexer {
            crate::term::Multiplexer::Tmux => {
                "Nothing came back, and tmux is in the way — it answers some colour queries from \
                 its own idea of the palette and drops others."
            }
            _ => {
                "Nothing came back, and a multiplexer is in the way; it may have dropped the \
                  query or the reply."
            }
        }
    } else {
        "Nothing came back at all — not even the device-attributes request every terminal \
         answers — so something between here and the terminal is swallowing replies."
    }
}

/// A block of colour, for terminals that can show one.
///
/// Truecolor SGR rather than a palette approximation, because the entire subject
/// here is exact colour values. Returns an empty string when styling is off, so
/// `--no-color` output stays plain text and column alignment is computed from the
/// label rather than from this.
fn swatch(theme: &Theme, rgb: Rgb) -> String {
    if !theme.color() {
        return String::new();
    }
    let (r, g, b) = rgb.rgb8();
    format!("\u{1b}[48;2;{r};{g};{b}m  \u{1b}[0m")
}

// ---------------------------------------------------------------------------
// The machine-readable report
// ---------------------------------------------------------------------------

/// The whole report as JSON Lines: one object per colour, then one verdict.
///
/// Hand-rolled rather than serialised, as everywhere else in this project, so
/// that the exact shape is visible in the code that emits it.
#[must_use]
pub fn json_report(answers: &Answers, verdict: Option<Verdict>) -> String {
    let mut out = String::new();
    for answer in &answers.answers {
        out.push_str(&json_color_line(answer));
    }
    out.push_str(&json_verdict_line(answers, verdict));
    out
}

/// One colour, as one JSON object on one line.
fn json_color_line(answer: &Answer) -> String {
    let target = answer.target;
    let index = target
        .index()
        .map_or_else(|| "null".to_owned(), |index| index.to_string());
    let mut line = format!(
        "{{\"feature\":\"color\",\"record\":\"color\",\"target\":\"{}\",\"osc\":{},\"index\":{index}",
        target.slug(),
        target.ps()
    );
    match &answer.reported {
        Some(Reported { raw, rgb, .. }) => {
            let (r8, g8, b8) = rgb.rgb8();
            let _ = write!(
                line,
                ",\"answered\":true,\"raw\":\"{}\",\"hex\":\"{}\",\
                 \"rgb16\":[{},{},{}],\"rgb8\":[{r8},{g8},{b8}],\
                 \"luminance\":{:.6},\"dark\":{}",
                escape_json(raw),
                rgb.hex(),
                rgb.r,
                rgb.g,
                rgb.b,
                rgb.luminance(),
                rgb.is_dark()
            );
        }
        None => {
            line.push_str(
                ",\"answered\":false,\"raw\":null,\"hex\":null,\"rgb16\":null,\"rgb8\":null,\
                 \"luminance\":null,\"dark\":null",
            );
        }
    }
    line.push_str("}\n");
    line
}

/// The closing verdict object.
fn json_verdict_line(answers: &Answers, verdict: Option<Verdict>) -> String {
    let (dark, basis) = match verdict {
        Some(verdict) => (
            verdict.dark.to_string(),
            format!("\"{}\"", verdict.basis.as_str()),
        ),
        None => ("null".to_owned(), "null".to_owned()),
    };
    let multiplexer = answers
        .multiplexer
        .map_or_else(|| "null".to_owned(), |m| format!("\"{}\"", m.name()));
    format!(
        "{{\"feature\":\"color\",\"record\":\"verdict\",\"dark\":{dark},\"basis\":{basis},\
         \"answered_barrier\":{},\"multiplexer\":{multiplexer},\"interactive\":{}}}\n",
        answers.answered_barrier, answers.interactive
    )
}

/// Minimal JSON string escaping, for the few strings that reach the output.
pub(super) fn escape_json(text: &str) -> String {
    text.chars()
        .flat_map(|ch| match ch {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            ch if ch.is_control() => format!("\\u{:04x}", ch as u32).chars().collect(),
            ch => vec![ch],
        })
        .collect()
}

#[cfg(test)]
// clippy.toml sets allow-unwrap-in-tests; the attribute covers the helpers in
// here that are not themselves `#[test]` functions.
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::color::value::parse_reply;

    /// An answered target, built from a reply payload.
    fn answered(target: Target, payload: &str) -> Answer {
        Answer {
            target,
            reported: Some(parse_reply(payload).unwrap()),
        }
    }

    /// A target the terminal said nothing about.
    fn silent(target: Target) -> Answer {
        Answer {
            target,
            reported: None,
        }
    }

    /// A dark-background report with all three default colours.
    fn dark_answers() -> Answers {
        Answers {
            answers: vec![
                answered(Target::Foreground, "10;rgb:cdcd/d6d6/f4f4"),
                answered(Target::Background, "11;rgb:1e1e/1e1e/2e2e"),
                answered(Target::Cursor, "12;rgb:f5f5/e0e0/dcdc"),
            ],
            answered_barrier: true,
            interactive: true,
            ..Answers::default()
        }
    }

    // -- the verdict ---------------------------------------------------------

    #[test]
    fn a_dark_background_is_dark_and_exits_zero() {
        let verdict = verdict(None, Some(Rgb::from_rgb8(0x1e, 0x1e, 0x2e))).unwrap();
        assert!(verdict.dark);
        assert_eq!(verdict.basis, Basis::Luminance);
        assert_eq!(verdict.label(), "dark");
        assert_eq!(
            format!("{:?}", verdict.exit_code()),
            format!("{:?}", ExitCode::SUCCESS)
        );
    }

    #[test]
    fn a_light_background_exits_one_rather_than_zero() {
        let verdict = verdict(None, Some(Rgb::from_rgb8(0xfd, 0xf6, 0xe3))).unwrap();
        assert!(!verdict.dark);
        assert_eq!(verdict.label(), "light");
        assert_eq!(
            format!("{:?}", verdict.exit_code()),
            format!("{:?}", ExitCode::from(1))
        );
    }

    #[test]
    fn no_background_means_no_verdict_at_all() {
        // The whole reason for the third exit status: not knowing is not "light".
        assert!(verdict(None, None).is_none());
    }

    #[test]
    fn the_terminals_own_report_beats_the_arithmetic() {
        // A light theme with a dark translucent background: luminance says dark,
        // the terminal says light, and the terminal is right.
        let verdict = verdict(Some(Scheme::Light), Some(Rgb::from_rgb8(0, 0, 0))).unwrap();
        assert!(!verdict.dark);
        assert_eq!(verdict.basis, Basis::Reported);
        assert!(verdict.basis.describe().contains("2031"));

        let dark = verdict_from_scheme(Scheme::Dark);
        assert!(dark.dark);
        assert_eq!(dark.basis, Basis::Reported);
    }

    /// The verdict for a reported scheme with no background at all.
    fn verdict_from_scheme(scheme: Scheme) -> Verdict {
        verdict(Some(scheme), None).unwrap()
    }

    #[test]
    fn a_reported_scheme_needs_no_background_query() {
        assert!(verdict(Some(Scheme::Dark), None).is_some());
    }

    // -- the JSON shape ------------------------------------------------------

    #[test]
    fn json_is_one_object_per_line_and_ends_with_a_newline() {
        let answers = dark_answers();
        let output = json_report(
            &answers,
            verdict(None, Some(Rgb::from_rgb8(0x1e, 0x1e, 0x2e))),
        );
        assert!(output.ends_with('\n'));
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 4, "three colours and one verdict: {output}");
        for line in &lines {
            assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
            assert!(!line.contains('\n'));
            // Balanced braces, which is as much structure as a hand-rolled
            // emitter can be checked for without a parser.
            assert_eq!(
                line.matches('{').count(),
                line.matches('}').count(),
                "{line}"
            );
        }
    }

    #[test]
    fn a_json_colour_record_carries_the_raw_reply_and_the_normalised_value() {
        let line = json_color_line(&answered(Target::Background, "11;rgb:f/0/0"));
        assert!(line.contains("\"record\":\"color\""), "{line}");
        assert!(line.contains("\"target\":\"background\""), "{line}");
        assert!(line.contains("\"osc\":11"), "{line}");
        assert!(line.contains("\"index\":null"), "{line}");
        assert!(line.contains("\"answered\":true"), "{line}");
        // Both halves of the point of this feature, on one line.
        assert!(line.contains("\"raw\":\"rgb:f/0/0\""), "{line}");
        assert!(line.contains("\"hex\":\"#ff0000\""), "{line}");
        assert!(line.contains("\"rgb8\":[255,0,0]"), "{line}");
        assert!(line.contains("\"rgb16\":[65535,0,0]"), "{line}");
        // Pure red lands *just* on the light side: luminance 0.2126 against the
        // L* = 50 threshold of 0.1842. That is the red channel's 0.2126 weight
        // doing all the work — pure blue, weighted 0.0722, is comfortably dark —
        // and it is the right answer, since black text on pure red has a better
        // contrast ratio than white does.
        assert!(line.contains("\"dark\":false"), "{line}");
        assert!(
            parse_reply("11;rgb:0/0/f").unwrap().rgb.is_dark(),
            "pure blue is the dark one of the two"
        );
    }

    #[test]
    fn a_json_palette_record_carries_its_index() {
        let line = json_color_line(&answered(Target::Palette(4), "4;4;rgb:0/0/f"));
        assert!(line.contains("\"target\":\"palette-4\""), "{line}");
        assert!(line.contains("\"osc\":4"), "{line}");
        assert!(line.contains("\"index\":4"), "{line}");
        // Index 0 must be a number, not confused with null.
        let zero = json_color_line(&answered(Target::Palette(0), "4;0;rgb:0/0/0"));
        assert!(zero.contains("\"index\":0"), "{zero}");
    }

    #[test]
    fn an_unanswered_colour_is_null_rather_than_black() {
        // The bug this guards against: reporting a colour nobody gave us as
        // #000000, which a consumer cannot tell from a real black background.
        let line = json_color_line(&silent(Target::Cursor));
        assert!(line.contains("\"answered\":false"), "{line}");
        assert!(line.contains("\"hex\":null"), "{line}");
        assert!(line.contains("\"luminance\":null"), "{line}");
        assert!(line.contains("\"dark\":null"), "{line}");
        assert!(!line.contains("#000000"), "{line}");
    }

    #[test]
    fn the_json_verdict_records_its_basis() {
        let answers = dark_answers();
        let line = json_verdict_line(&answers, verdict(None, Some(Rgb::from_rgb8(0, 0, 0))));
        assert!(line.contains("\"record\":\"verdict\""), "{line}");
        assert!(line.contains("\"dark\":true"), "{line}");
        assert!(line.contains("\"basis\":\"luminance\""), "{line}");
        assert!(line.contains("\"answered_barrier\":true"), "{line}");
        assert!(line.contains("\"multiplexer\":null"), "{line}");
        assert!(line.contains("\"interactive\":true"), "{line}");

        let reported = json_verdict_line(&answers, verdict(Some(Scheme::Light), None));
        assert!(reported.contains("\"dark\":false"), "{reported}");
        assert!(reported.contains("\"basis\":\"reported\""), "{reported}");
    }

    #[test]
    fn an_undecidable_verdict_is_null_in_json() {
        let answers = Answers {
            answers: vec![silent(Target::Background)],
            interactive: true,
            ..Answers::default()
        };
        let line = json_verdict_line(&answers, None);
        assert!(line.contains("\"dark\":null"), "{line}");
        assert!(line.contains("\"basis\":null"), "{line}");
    }

    #[test]
    fn a_multiplexer_is_named_in_the_verdict_record() {
        let answers = Answers {
            multiplexer: Some(crate::term::Multiplexer::Tmux),
            interactive: true,
            ..Answers::default()
        };
        let line = json_verdict_line(&answers, None);
        assert!(line.contains("\"multiplexer\":\"tmux\""), "{line}");
    }

    #[test]
    fn json_output_never_contains_a_control_byte() {
        // A raw reply is terminal output, and printing it verbatim into JSON
        // would let a hostile terminal inject an escape sequence into a log.
        let answer = Answer {
            target: Target::Background,
            reported: Some(Reported {
                ps: 11,
                index: None,
                raw: "rgb:0/0/0\u{1b}[2J\"".to_owned(),
                rgb: Rgb::BLACK,
            }),
        };
        let line = json_color_line(&answer);
        assert!(!line.contains('\u{1b}'), "{line}");
        assert!(line.contains("\\u001b"), "{line}");
        assert!(line.contains("\\\""), "the quote is escaped too: {line}");
    }

    #[test]
    fn escaping_handles_the_characters_json_forbids() {
        assert_eq!(escape_json("plain"), "plain");
        assert_eq!(escape_json("a\"b"), "a\\\"b");
        assert_eq!(escape_json("a\\b"), "a\\\\b");
        assert_eq!(escape_json("a\nb"), "a\\u000ab");
    }

    // -- the text report -----------------------------------------------------

    #[test]
    fn the_text_report_shows_the_raw_reply_beside_the_normalised_value() {
        // Rendered with a plain theme so the assertions are about content.
        let theme = Theme::plain();
        let answers = dark_answers();
        // Exercising the whole layout path is what catches a panic in a width
        // calculation; the content assertions below are on the pieces.
        text_report(
            &theme,
            &answers,
            verdict(None, Some(Rgb::from_rgb8(0x1e, 0x1e, 0x2e))),
        );
        assert_eq!(swatch(&theme, Rgb::BLACK), "", "no styling means no swatch");
    }

    #[test]
    fn the_reason_for_an_unanswerable_verdict_depends_on_what_was_heard() {
        let conclusive = Answers {
            answers: vec![silent(Target::Background)],
            answered_barrier: true,
            interactive: true,
            ..Answers::default()
        };
        assert!(unanswered_reason(&conclusive).contains("OSC 11"));
        assert!(
            unanswered_reason(&conclusive).contains("COLORFGBG"),
            "the substitute people reach for has to be named and refused"
        );

        let silent_terminal = Answers {
            answers: vec![silent(Target::Background)],
            interactive: true,
            ..Answers::default()
        };
        assert!(unanswered_reason(&silent_terminal).contains("swallowing"));

        let multiplexed = Answers {
            answers: vec![silent(Target::Background)],
            multiplexer: Some(crate::term::Multiplexer::Tmux),
            interactive: true,
            ..Answers::default()
        };
        assert!(unanswered_reason(&multiplexed).contains("tmux"));

        let never_asked = Answers {
            answers: vec![silent(Target::Cursor)],
            answered_barrier: true,
            interactive: true,
            ..Answers::default()
        };
        assert!(unanswered_reason(&never_asked).contains("not among the colours"));
    }

    #[test]
    fn a_palette_report_renders_as_a_grid_without_panicking() {
        let theme = Theme::plain();
        // A part-row at the end is the case an off-by-one in the grid would break.
        for count in [1usize, 4, 5, 256] {
            let answers = Answers {
                answers: (0..count)
                    .map(|index| {
                        let index = u8::try_from(index).unwrap_or(255);
                        answered(Target::Palette(index), &format!("4;{index};rgb:1/2/3"))
                    })
                    .collect(),
                answered_barrier: true,
                interactive: true,
                ..Answers::default()
            };
            text_report(&theme, &answers, None);
        }
    }

    #[test]
    fn contrast_is_only_reported_when_both_colours_are_known() {
        let theme = Theme::plain();
        let one_sided = Answers {
            answers: vec![
                answered(Target::Background, "11;rgb:0/0/0"),
                silent(Target::Foreground),
            ],
            answered_barrier: true,
            interactive: true,
            ..Answers::default()
        };
        // No panic, and no contrast ratio invented from a colour nobody gave us.
        summary(&theme, &one_sided, verdict(None, Some(Rgb::BLACK)));
    }
}
