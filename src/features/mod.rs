//! One module per terminal feature, plus the cross-feature overview.

pub mod clipboard;
pub mod color;
pub mod decode;
pub mod dimensions;
pub mod doctor;
pub mod explain;
pub mod glyph_protocol;
pub mod grapheme;
pub mod hyperlink;
pub mod keys;
pub mod kitty_graphics;
pub mod notification;
pub mod progress;
pub mod style;
pub mod textsize;
pub mod unscroll;

use anyhow::{Context as _, Result};

use crate::{
    db::Catalog,
    theme::{Ink, Theme},
};

/// Renders `escapade list`: every known feature with support at a glance.
pub fn list(catalog: &Catalog, no_color: bool) {
    let theme = Theme::new(no_color);

    println!();
    println!(
        "{}  {}",
        theme.paint("escapade", Ink::Title),
        theme.paint("— modern terminal features", Ink::Muted)
    );

    for feature in catalog.features() {
        let supported = feature.supported_by(catalog);
        let verdict = if supported.is_empty() {
            theme.paint("no known implementations", Ink::Bad)
        } else {
            theme.paint(&supported.join(", "), Ink::Good)
        };

        println!();
        println!(
            "  {}  {}",
            theme.paint(&feature.name, Ink::Accent),
            theme.paint(&format!("[{}]", feature.status.label()), Ink::Muted)
        );
        println!(
            "  {}",
            theme.paint(
                &format!("$ escapade {}", subcommand_for(&feature.id)),
                Ink::Code
            )
        );
        for line in wrap(&feature.tagline, theme.width().saturating_sub(4)) {
            println!("    {line}");
        }
        println!("    {} {verdict}", theme.paint("works in:", Ink::Muted));
    }

    println!();
    println!(
        "  {}",
        theme.paint(
            "Run `escapade <feature> explain` for the details, or `escapade <feature> --help` \
             for what it can do — not every feature has a demo.",
            Ink::Muted
        )
    );
    println!();
}

/// Maps a feature id to the subcommand that exposes it.
///
/// These usually match; `kitty-graphics` is shortened to `graphics` because
/// that is what people call it at the prompt.
fn subcommand_for(feature_id: &str) -> &str {
    match feature_id {
        "glyph-protocol" => "glyph",
        "kitty-graphics" => "graphics",
        "grapheme-clustering" => "grapheme",
        "hyperlinks" => "hyperlink",
        "notifications" => "notification",
        "text-sizing" => "textsize",
        "text-attributes" => "style",
        "keyboard-protocol" => "keys",
        // `unscroll` and `color` are already their own subcommand names.
        other => other,
    }
}

/// Reads a command's text operand from arguments or standard input.
///
/// The Unix convention, as `cat`, `grep` and `pbcopy` use it: words on the
/// command line if any were given, otherwise the stream. A lone `-` asks for the
/// stream explicitly, which matters when the text you want *is* a dash.
///
/// The important case is the mistake: with no arguments and a terminal on
/// standard input, a naive implementation blocks forever with no indication of
/// why. That is reported as an error naming both ways to supply the text.
///
/// # Errors
///
/// Fails if standard input is a terminal and no arguments were given, or if
/// reading standard input fails.
pub fn bytes_operand(args: &[String]) -> Result<Vec<u8>> {
    use std::io::{IsTerminal as _, Read as _, stdin};

    resolve_operand(args, stdin().is_terminal(), || {
        let mut bytes = Vec::new();
        stdin().read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

/// As [`bytes_operand`], but as text with one trailing newline removed.
///
/// The trailing newline is dropped because `echo hello | escapade …` is the
/// normal way to pipe a single value in, and almost nothing wants the newline
/// that `echo` adds. Commands that must preserve their input byte for byte —
/// putting a file on the clipboard, say — should use [`bytes_operand`].
///
/// # Errors
///
/// As [`bytes_operand`], plus failure if the input is not valid UTF-8.
pub fn text_operand(args: &[String]) -> Result<String> {
    let bytes = bytes_operand(args)?;
    let mut text = String::from_utf8(bytes).context("input is not valid UTF-8")?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

/// The decision half of [`bytes_operand`], separated so it can be tested without
/// a terminal or a real standard input.
fn resolve_operand(
    args: &[String],
    stdin_is_terminal: bool,
    read: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let explicit_stdin = args.len() == 1 && args[0] == "-";
    if !args.is_empty() && !explicit_stdin {
        return Ok(args.join(" ").into_bytes());
    }
    if stdin_is_terminal && !explicit_stdin {
        anyhow::bail!(
            "no text given — pass it as arguments, or pipe it in on standard input \
             (use `-` to read standard input even from a terminal)"
        );
    }
    read()
}

/// The three answers a probe can honestly give, and the exit status for each.
///
/// Every `probe` is silent by default and reports through the exit status, so it
/// can be used directly as a shell predicate:
///
/// ```sh
/// if escapade graphics probe; then …
/// ```
///
/// **`Unknown` is deliberately a third status rather than being folded into
/// `No`.** A terminal that could not be asked — because output is redirected,
/// because a multiplexer filters the query, or because nothing answered — has
/// told us nothing, and reporting that as "unsupported" is the exact class of
/// confident wrong answer this project exists to avoid. A script that genuinely
/// wants "anything but yes" can write `if ! escapade … probe`, which treats both
/// non-zero statuses alike; a script that wants to distinguish them can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// The terminal answered, and it supports the feature.
    Yes,
    /// The terminal answered, having demonstrably had its chance, and does not.
    No,
    /// Nothing could honestly be concluded.
    Unknown,
}

impl Support {
    /// The process exit status: 0 supported, 1 not supported, 2 inconclusive.
    #[must_use]
    pub fn exit_code(self) -> std::process::ExitCode {
        match self {
            Self::Yes => std::process::ExitCode::SUCCESS,
            Self::No => std::process::ExitCode::from(1),
            Self::Unknown => std::process::ExitCode::from(2),
        }
    }

    /// Lowercase word for machine-readable output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }
}

/// Which steps of a demo to run.
///
/// Every demo names its steps, so a reader who wants the one thing they came for
/// does not have to press space eleven times to reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Index of the step to begin at.
    pub start: usize,
    /// Whether to stop after that one step rather than carrying on.
    pub single: bool,
}

/// Resolves a demo's `--only` and `--step` flags against its step names.
///
/// `--only <NAME>` matches a step name exactly, case-insensitively, or by an
/// unambiguous prefix — `--only mandel` finds `mandelbrot`. `--step N` is
/// 1-based, to match what `--list` prints, and clamps rather than failing so that
/// `--step 99` lands on the last step instead of erroring.
///
/// # Errors
///
/// Fails if `--only` matches no step, or matches more than one by prefix. Both
/// messages list the available names, since a reader who mistyped one needs to
/// see the real ones.
pub fn select_steps(names: &[&str], only: Option<&str>, step: Option<usize>) -> Result<Selection> {
    if names.is_empty() {
        anyhow::bail!("this demo has no steps");
    }

    if let Some(wanted) = only {
        let wanted = wanted.trim();
        let exact = names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(wanted));
        if let Some(start) = exact {
            return Ok(Selection {
                start,
                single: true,
            });
        }

        let matches: Vec<usize> = names
            .iter()
            .enumerate()
            .filter(|(_, name)| {
                name.to_ascii_lowercase()
                    .starts_with(&wanted.to_ascii_lowercase())
            })
            .map(|(index, _)| index)
            .collect();
        match matches.as_slice() {
            [start] => {
                return Ok(Selection {
                    start: *start,
                    single: true,
                });
            }
            // Deliberately free of the word "step": this is shared with
            // `doctor`, which matches checks. The caller adds the noun through
            // `anyhow` context.
            [] => anyhow::bail!("`{wanted}` is not one of: {}", names.join(", ")),
            several => anyhow::bail!(
                "`{wanted}` is ambiguous — it matches {}: {}",
                several.len(),
                several
                    .iter()
                    .map(|&index| names[index])
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    Ok(Selection {
        start: step
            .map_or(0, |step| step.saturating_sub(1))
            .min(names.len() - 1),
        single: false,
    })
}

/// Greedy word wrap. Good enough for taglines and table cells; anything longer
/// goes through the markdown renderer instead.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_win_and_are_joined_with_spaces() {
        let args = ["hello".to_owned(), "world".to_owned()];
        let got = resolve_operand(&args, true, || unreachable!("stdin must not be read")).unwrap();
        assert_eq!(got, b"hello world");
    }

    #[test]
    fn no_arguments_reads_the_stream() {
        let got = resolve_operand(&[], false, || Ok(b"piped".to_vec())).unwrap();
        assert_eq!(got, b"piped");
    }

    #[test]
    fn a_lone_dash_reads_the_stream_even_from_a_terminal() {
        let args = ["-".to_owned()];
        let got = resolve_operand(&args, true, || Ok(b"explicit".to_vec())).unwrap();
        assert_eq!(got, b"explicit");
    }

    #[test]
    fn a_dash_among_other_words_is_just_a_dash() {
        let args = ["a".to_owned(), "-".to_owned()];
        let got = resolve_operand(&args, true, || unreachable!()).unwrap();
        assert_eq!(got, b"a -");
    }

    #[test]
    fn nothing_to_read_and_nowhere_to_read_from_is_an_error_not_a_hang() {
        // The failure mode this exists to prevent: blocking forever on a
        // terminal with no indication of what is being waited for.
        let error = resolve_operand(&[], true, || unreachable!())
            .unwrap_err()
            .to_string();
        assert!(error.contains("arguments"), "{error}");
        assert!(error.contains("standard input"), "{error}");
    }

    #[test]
    fn probe_exit_statuses_are_distinct_and_documented() {
        use std::process::ExitCode;
        // A shell predicate needs 0 for yes; anything else is "not yes".
        assert_eq!(
            format!("{:?}", Support::Yes.exit_code()),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert_ne!(
            format!("{:?}", Support::No.exit_code()),
            format!("{:?}", Support::Unknown.exit_code()),
            "inconclusive must not be indistinguishable from unsupported"
        );
    }

    #[test]
    fn support_words_are_stable_for_machine_output() {
        assert_eq!(Support::Yes.as_str(), "yes");
        assert_eq!(Support::No.as_str(), "no");
        assert_eq!(Support::Unknown.as_str(), "unknown");
    }

    const DEMO: &[&str] = &["handshake", "gradient", "mandelbrot", "mandala", "restore"];

    #[test]
    fn only_matches_a_step_name_exactly() {
        let selection = select_steps(DEMO, Some("mandelbrot"), None).unwrap();
        assert_eq!(
            selection,
            Selection {
                start: 2,
                single: true
            }
        );
    }

    #[test]
    fn only_is_case_insensitive() {
        assert_eq!(select_steps(DEMO, Some("Gradient"), None).unwrap().start, 1);
    }

    #[test]
    fn only_accepts_an_unambiguous_prefix() {
        assert_eq!(select_steps(DEMO, Some("grad"), None).unwrap().start, 1);
        assert_eq!(select_steps(DEMO, Some("h"), None).unwrap().start, 0);
    }

    #[test]
    fn only_rejects_an_ambiguous_prefix_and_says_which() {
        // `manda` prefixes both `mandelbrot` and `mandala`.
        let error = select_steps(DEMO, Some("mand"), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("mandelbrot") && error.contains("mandala"),
            "{error}"
        );
    }

    #[test]
    fn an_exact_match_beats_a_prefix_of_another_step() {
        // A step whose whole name prefixes another must still be reachable.
        let names = ["ring", "rings"];
        assert_eq!(select_steps(&names, Some("ring"), None).unwrap().start, 0);
        assert_eq!(select_steps(&names, Some("rings"), None).unwrap().start, 1);
    }

    #[test]
    fn only_lists_the_real_names_when_it_finds_nothing() {
        let error = select_steps(DEMO, Some("nope"), None)
            .unwrap_err()
            .to_string();
        for name in DEMO {
            assert!(error.contains(name), "{error} should mention {name}");
        }
    }

    #[test]
    fn step_is_one_based_and_clamps() {
        assert_eq!(select_steps(DEMO, None, Some(1)).unwrap().start, 0);
        assert_eq!(select_steps(DEMO, None, Some(3)).unwrap().start, 2);
        assert_eq!(
            select_steps(DEMO, None, Some(999)).unwrap().start,
            DEMO.len() - 1
        );
        assert_eq!(select_steps(DEMO, None, Some(0)).unwrap().start, 0);
    }

    #[test]
    fn without_only_the_demo_runs_on_past_the_starting_step() {
        assert!(!select_steps(DEMO, None, Some(2)).unwrap().single);
        assert!(!select_steps(DEMO, None, None).unwrap().single);
    }

    #[test]
    fn wrap_respects_the_width() {
        let text = "Applications ship their own vector glyphs to the terminal at runtime";
        for line in wrap(text, 24) {
            assert!(line.chars().count() <= 24, "too long: {line:?}");
        }
    }

    #[test]
    fn wrap_keeps_every_word() {
        let text = "one two three four five six seven";
        let joined = wrap(text, 12).join(" ");
        assert_eq!(joined, text);
    }

    #[test]
    fn wrap_handles_words_longer_than_the_width() {
        let lines = wrap("short aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", 20);
        assert_eq!(lines.len(), 2, "an over-long word gets its own line");
    }

    #[test]
    fn feature_ids_map_to_real_subcommands() {
        let catalog = Catalog::load().unwrap();
        for feature in catalog.features() {
            let subcommand = subcommand_for(&feature.id);
            assert!(
                matches!(
                    subcommand,
                    "glyph"
                        | "graphics"
                        | "grapheme"
                        | "hyperlink"
                        | "notification"
                        | "progress"
                        | "textsize"
                        | "color"
                        | "clipboard"
                        | "style"
                        | "keys"
                        | "unscroll"
                        | "dimensions"
                ),
                "feature `{}` maps to unknown subcommand `{subcommand}`",
                feature.id
            );
        }
    }
}
