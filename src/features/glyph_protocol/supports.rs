//! `escapade glyph probe <CODEPOINTS>` — ask the terminal what it
//! can actually draw.
//!
//! This is the `q` verb applied in bulk. For each codepoint the terminal answers
//! which sources cover it, which turns "will my icon show up on this machine?"
//! from a guess into a question with an answer.
//!
//! # Why the queries are batched
//!
//! Sending a thousand queries and only then starting to read would risk a
//! deadlock: the terminal's replies fill the pty buffer, the terminal blocks
//! writing, and we block writing our next query into a buffer it has stopped
//! draining. Queries therefore go out in batches of [`BATCH_SIZE`], each closed
//! by a Primary Device Attributes request that tells us the batch is fully
//! processed. That keeps at most a batch of replies in flight and makes the wait
//! self-terminating rather than timeout-driven.

use std::{
    collections::HashMap,
    io::{IsTerminal as _, stdout},
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};

use crate::{
    cli::ProbeFormat,
    features::Support,
    features::glyph_protocol::{
        Coverage, GlyphReply, codepoint_query, is_private_use, parse_reply,
    },
    input::{InputReader, Reply},
    term::{CSI, Multiplexer, Term, wrapped},
    theme::{Ink, Theme},
    unicode::{self, CharInfo},
};

/// How many queries to have outstanding at once.
const BATCH_SIZE: usize = 64;

/// How long to wait for a batch to come back before giving up on it.
const BATCH_TIMEOUT: Duration = Duration::from_millis(600);

/// Longest name printed before truncation.
const NAME_WIDTH: usize = 44;

/// What the terminal said about one codepoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The terminal answered, and something covers the codepoint.
    Renders(Coverage),
    /// The terminal answered, and nothing covers it — this would be tofu.
    Unsupported,
    /// No answer, so nothing is known. Not the same as unsupported.
    Unknown,
}

/// One row of the report.
#[derive(Debug, Clone)]
pub struct Row {
    /// Unicode facts about the codepoint.
    pub info: CharInfo,
    /// What the terminal said.
    pub verdict: Verdict,
}

/// Renders `escapade glyph probe`.
///
/// # Errors
///
/// Fails if the specification cannot be parsed, or if the terminal cannot be
/// set up. A terminal that declines to answer is a result, not an error.
pub fn report(
    theme: &Theme,
    spec: &str,
    long: bool,
    format: Option<ProbeFormat>,
) -> Result<ExitCode> {
    let codepoints = unicode::parse_spec(spec)
        .with_context(|| format!("parsing codepoint specification `{spec}`"))?;

    let interactive = stdout().is_terminal();
    let multiplexer = Multiplexer::detect();

    let coverage = if interactive {
        query_all(&codepoints)?
    } else {
        HashMap::new()
    };

    let rows: Vec<Row> = codepoints
        .iter()
        .map(|&codepoint| Row {
            info: unicode::describe(codepoint),
            verdict: match coverage.get(&codepoint) {
                Some(coverage) if coverage.renders() => Verdict::Renders(*coverage),
                Some(_) => Verdict::Unsupported,
                None => Verdict::Unknown,
            },
        })
        .collect();

    match format {
        None => {}
        Some(ProbeFormat::Json) => {
            for row in &rows {
                print!("{}", json_line(row));
            }
        }
        Some(ProbeFormat::Debug) => {
            print(theme, &rows, long);
            summarize(theme, &rows, interactive, multiplexer);
        }
    }
    Ok(overall(&rows).exit_code())
}

/// The single verdict for a whole set of codepoints.
///
/// Conservative on purpose: one unanswered codepoint makes the answer `Unknown`,
/// and one that nothing covers makes it `No`. A script asking "can this terminal
/// draw all of these?" wants the weakest link, not an average.
#[must_use]
pub fn overall(rows: &[Row]) -> Support {
    if rows.iter().any(|row| row.verdict == Verdict::Unknown) {
        Support::Unknown
    } else if rows.iter().any(|row| row.verdict == Verdict::Unsupported) {
        Support::No
    } else {
        Support::Yes
    }
}

/// One JSON object per codepoint, one per line.
fn json_line(row: &Row) -> String {
    let verdict = match row.verdict {
        Verdict::Renders(_) => "yes",
        Verdict::Unsupported => "no",
        Verdict::Unknown => "unknown",
    };
    let sources = match row.verdict {
        Verdict::Renders(coverage) => {
            let mut names = Vec::new();
            if coverage.system {
                names.push("\"system\"");
            }
            if coverage.glossary {
                names.push("\"glossary\"");
            }
            names.join(",")
        }
        _ => String::new(),
    };
    format!(
        "{{\"codepoint\":\"U+{:04X}\",\"name\":\"{}\",\"verdict\":\"{verdict}\",\
         \"sources\":[{sources}],\"private_use\":{},\"width\":{}}}\n",
        row.info.codepoint,
        row.info.name.replace('\\', "\\\\").replace('"', "\\\""),
        is_private_use(row.info.codepoint),
        row.info.width,
    )
}

/// Queries every codepoint, in batches, and collects the answers.
fn query_all(codepoints: &[u32]) -> Result<HashMap<u32, Coverage>> {
    let mut term = Term::new()?;
    let mut input = InputReader::new()?;
    let multiplexer = Multiplexer::detect();

    term.enable_raw()?;
    input.drain();

    let mut coverage = HashMap::with_capacity(codepoints.len());
    for batch in codepoints.chunks(BATCH_SIZE) {
        // One shield for the whole batch rather than one per query: a terminal
        // that prints APC strings instead of ignoring them would leak all
        // sixty-four onto the same line, and one erase takes care of all of them.
        let queries: String = batch
            .iter()
            .map(|&codepoint| wrapped(multiplexer, &codepoint_query(codepoint)))
            .collect();
        term.write_query(&queries)?;
        // Barrier: sent unwrapped, because we want to know that the round trip
        // completed even if a multiplexer swallowed the queries themselves.
        term.write_now(&format!("{CSI}c"))?;

        let deadline = Instant::now() + BATCH_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let Ok(Some(reply)) = input.wait_for_reply(remaining, |_| true) else {
                break;
            };
            match reply {
                Reply::Apc(payload) => {
                    if let Some(GlyphReply::Query {
                        codepoint,
                        coverage: answer,
                    }) = parse_reply(&payload)
                    {
                        coverage.insert(codepoint, answer);
                    }
                }
                // The batch has been fully processed; anything unanswered in it
                // is going to stay unanswered.
                Reply::Csi {
                    final_byte: 'c', ..
                } => break,
                _ => {}
            }
        }

        // A terminal that ignored the whole first batch will ignore the rest.
        if coverage.is_empty() {
            break;
        }
    }

    term.restore();
    Ok(coverage)
}

/// Prints one line per codepoint.
fn print(theme: &Theme, rows: &[Row], long: bool) {
    println!();
    for row in rows {
        let name = truncate(&row.info.title_name(), NAME_WIDTH);
        let padding = NAME_WIDTH.saturating_sub(name.chars().count());

        let (verdict, ink) = match row.verdict {
            Verdict::Renders(_) => (rendered(&row.info), Ink::Strong),
            Verdict::Unsupported => ("UNSUPPORTED".to_owned(), Ink::Bad),
            Verdict::Unknown => ("Unknown".to_owned(), Ink::Muted),
        };

        // Labels run from `U+0041` to `U+10FFFF`, so pad to the longest to keep
        // the name column aligned.
        let label = format!("{:<8}", row.info.label());
        let mut line = format!(
            "  {}  {}{}  {}",
            theme.paint(&label, Ink::Accent),
            theme.paint(&name, Ink::Strong),
            " ".repeat(padding),
            theme.paint(&verdict, ink),
        );

        // Padding here is by display width, since the verdict column can hold a
        // double-width emoji whose column count is not its character count.
        let verdict_width = unicode::display_width(&verdict).max(row.info.width);
        line.push_str(&" ".repeat(14usize.saturating_sub(verdict_width)));
        line.push_str(&theme.paint(&annotation(&row.info, row.verdict), Ink::Muted));

        if long {
            line.push_str(&theme.paint(&details(&row.info), Ink::Muted));
        }
        println!("{line}");
    }
}

/// The character itself, when it is safe to print.
///
/// Controls, surrogates and unassigned codepoints are never printed: emitting a
/// raw `ESC` here would execute whatever came after it, and the whole point of
/// this tool is knowing which bytes go to the terminal.
fn rendered(info: &CharInfo) -> String {
    match info.character {
        Some(character) if info.printable() => character.to_string(),
        _ => "·".to_owned(),
    }
}

/// The trailing note explaining a verdict.
fn annotation(info: &CharInfo, verdict: Verdict) -> String {
    match verdict {
        Verdict::Renders(coverage) => match (coverage.system, coverage.glossary) {
            (true, true) => "system font, shadowed by a registration".to_owned(),
            (true, false) => "system font".to_owned(),
            (false, true) => "registered this session".to_owned(),
            (false, false) => String::new(),
        },
        // The protocol's three allowed ranges, not the Unicode PrivateUse
        // category: they nearly coincide, but registration is what matters here.
        Verdict::Unsupported => {
            if is_private_use(info.codepoint) {
                "nothing covers it — a glyph could be registered here".to_owned()
            } else {
                "nothing covers it — this is tofu".to_owned()
            }
        }
        // The character is still printed so the reader can judge with their own
        // eyes, which is the only tool left when the terminal will not answer.
        Verdict::Unknown => match info.character {
            // A private-use codepoint has no assigned appearance at all, so
            // "prints as ␀ here" would look like a bug rather than the answer.
            Some(character) if is_private_use(info.codepoint) => {
                format!("prints as [{character}] — private use, so entirely font-dependent")
            }
            Some(character) if info.printable() => format!("prints as {character} here"),
            _ => "not a printable character".to_owned(),
        },
    }
}

/// Extra Unicode facts for `--long`.
fn details(info: &CharInfo) -> String {
    let mut parts = Vec::new();
    if let Some(character) = info.character {
        parts.push(unicode::category_group_name(character).to_owned());
    }
    parts.push(format!("width {}", info.width));
    if info.emoji {
        parts.push("emoji".to_owned());
    }
    if info.private_use() {
        parts.push("private use".to_owned());
    }
    if !info.named {
        parts.push("unnamed".to_owned());
    }
    format!("  [{}]", parts.join(", "))
}

/// Truncates to `width` display columns, marking where it cut.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Prints the closing tally and any explanation the reader needs.
fn summarize(theme: &Theme, rows: &[Row], interactive: bool, multiplexer: Option<Multiplexer>) {
    let renders = rows
        .iter()
        .filter(|row| matches!(row.verdict, Verdict::Renders(_)))
        .count();
    let unsupported = rows
        .iter()
        .filter(|row| row.verdict == Verdict::Unsupported)
        .count();
    let unknown = rows.len() - renders - unsupported;

    println!();
    let mut tally = vec![theme.paint(&format!("{renders} render"), Ink::Good)];
    if unsupported > 0 {
        tally.push(theme.paint(&format!("{unsupported} unsupported"), Ink::Bad));
    }
    if unknown > 0 {
        tally.push(theme.paint(&format!("{unknown} unknown"), Ink::Muted));
    }
    println!(
        "  {} {} {}",
        tally.join(theme.paint(" · ", Ink::Muted).as_str()),
        theme.paint("of", Ink::Muted),
        theme.paint(&format!("{} codepoints", rows.len()), Ink::Muted)
    );

    if unknown == rows.len() && !rows.is_empty() {
        println!();
        let reason = if interactive {
            "Every answer is unknown because this terminal did not respond to the Glyph Protocol query at all."
        } else {
            "Every answer is unknown because stdout is not a terminal, so there was nobody to ask."
        };
        for line in crate::features::wrap(reason, theme.width().saturating_sub(2)) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
        for line in crate::features::wrap(
            "Unknown is not the same as unsupported: font coverage is independent of whether the \
             terminal implements this protocol, so these characters may well render fine.",
            theme.width().saturating_sub(2),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
        if let Some(multiplexer) = multiplexer {
            println!(
                "  {}",
                theme.paint(multiplexer.passthrough_hint(), Ink::Code)
            );
        }
    }

    println!();
    println!(
        "  {}",
        theme.paint(
            &format!(
                "Unicode {} · escapade glyph explain",
                unicode::unicode_version()
            ),
            Ink::Muted
        )
    );
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(codepoint: u32, verdict: Verdict) -> Row {
        Row {
            info: unicode::describe(codepoint),
            verdict,
        }
    }

    #[test]
    fn the_three_verdicts_render_as_the_brief_specifies() {
        let smiling = row(
            0x1_F60A,
            Verdict::Renders(Coverage {
                system: true,
                glossary: false,
            }),
        );
        assert_eq!(rendered(&smiling.info), "😊");

        let unsupported = row(0xE0A0, Verdict::Unsupported);
        assert_eq!(unsupported.info.title_name(), "<private use>");

        let unknown = row(0x1_F611, Verdict::Unknown);
        assert!(annotation(&unknown.info, Verdict::Unknown).contains("prints as"));
    }

    #[test]
    fn never_prints_a_control_character() {
        // U+001B would run whatever followed it as an escape sequence.
        let escape = unicode::describe(0x1B);
        assert_eq!(rendered(&escape), "·");
        assert!(!rendered(&escape).contains('\u{1b}'));

        let surrogate = unicode::describe(0xD800);
        assert_eq!(rendered(&surrogate), "·");
    }

    #[test]
    fn annotation_distinguishes_coverage_sources() {
        let info = unicode::describe(0x41);
        assert_eq!(
            annotation(
                &info,
                Verdict::Renders(Coverage {
                    system: true,
                    glossary: false
                })
            ),
            "system font"
        );
        assert_eq!(
            annotation(
                &info,
                Verdict::Renders(Coverage {
                    system: false,
                    glossary: true
                })
            ),
            "registered this session"
        );
        assert!(
            annotation(
                &info,
                Verdict::Renders(Coverage {
                    system: true,
                    glossary: true
                })
            )
            .contains("shadowed")
        );
    }

    #[test]
    fn unsupported_private_use_suggests_registering_a_glyph() {
        let pua = unicode::describe(0xE0A0);
        let note = annotation(&pua, Verdict::Unsupported);
        assert!(note.contains("registered"), "{note}");

        let ordinary = unicode::describe(0x1_F60A);
        assert!(annotation(&ordinary, Verdict::Unsupported).contains("tofu"));
    }

    #[test]
    fn truncate_marks_where_it_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789", 5), "0123…");
        assert_eq!(truncate("0123456789", 10), "0123456789");
    }

    #[test]
    fn long_details_list_the_unicode_facts() {
        let emoji = details(&unicode::describe(0x1_F60A));
        assert!(emoji.contains("emoji"));
        assert!(emoji.contains("width 2"));

        let pua = details(&unicode::describe(0xE0A0));
        assert!(pua.contains("private use"));
        assert!(pua.contains("unnamed"));
    }

    #[test]
    fn printing_a_report_never_panics_on_awkward_input() {
        let theme = Theme::plain();
        let rows = vec![
            row(
                0x41,
                Verdict::Renders(Coverage {
                    system: true,
                    glossary: false,
                }),
            ),
            row(0x1_F60A, Verdict::Unsupported),
            row(0xD800, Verdict::Unknown),
            row(0x1B, Verdict::Unknown),
            row(0x10_FFFF, Verdict::Unknown),
            row(0x4E00, Verdict::Unsupported),
        ];
        print(&theme, &rows, false);
        print(&theme, &rows, true);
        summarize(&theme, &rows, true, None);
        summarize(&theme, &[], false, Some(Multiplexer::Tmux));
    }
}
