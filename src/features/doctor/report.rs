//! Text output: three sources side by side, with disagreements impossible to miss.
//!
//! Agreement is quiet — a muted tick in each column and nothing else — and a
//! disagreement gets a marker, the two sides named, every source's raw value, and
//! the note explaining what the difference means in practice. A reader scanning the
//! output should be able to find the problem without reading a word of the rows
//! that are fine.

use crate::{
    decrqm::{self, Trust},
    features::doctor::{
        Diagnosis, Row,
        checks::Catalogue,
        verdict::{Agreement, Claim, Origin},
    },
    theme::{Ink, Theme},
};

/// Width of the check-name column.
const NAME_WIDTH: usize = 20;

/// Width of each source column.
const CELL_WIDTH: usize = 14;

/// Prints the check names, for `--only`.
pub fn list(theme: &Theme, catalogue: &Catalogue) {
    println!();
    println!("  {}", theme.paint("Checks", Ink::Title));
    println!();
    for check in catalogue.checks() {
        println!(
            "  {}  {}",
            theme.paint(&pad(&check.id, NAME_WIDTH), Ink::Code),
            theme.paint(&check.name, Ink::Strong)
        );
        for line in crate::features::wrap(
            &check.question,
            theme.width().saturating_sub(NAME_WIDTH + 6),
        ) {
            println!(
                "  {}{}",
                " ".repeat(NAME_WIDTH + 2),
                theme.paint(&line, Ink::Muted)
            );
        }
    }
    println!();
    println!(
        "  {}  {}",
        theme.paint("escapade doctor --only <NAME>", Ink::Code),
        theme.paint("runs one of them", Ink::Muted)
    );
    println!(
        "  {}  {}",
        theme.paint("data/checks.toml", Ink::Code),
        theme.paint(
            "is where they are defined — corrections welcome",
            Ink::Muted
        )
    );
    println!();
}

/// Prints the whole report.
pub fn render(theme: &Theme, diagnosis: &Diagnosis<'_>, only_disagreements: bool) {
    println!();
    println!("  {}", theme.paint("Terminal diagnosis", Ink::Title));
    println!();
    preamble(theme, diagnosis);
    warnings(theme, diagnosis);

    let shown: Vec<&Row<'_>> = diagnosis
        .rows
        .iter()
        .filter(|row| !only_disagreements || row.agreement == Agreement::Disagree)
        .collect();

    if shown.is_empty() {
        let message = if only_disagreements {
            "No disagreements. Every check that could be compared came out the same from every source."
        } else {
            "No checks selected."
        };
        println!("  {}", theme.paint(message, Ink::Good));
        println!();
        return;
    }

    header(theme);
    for row in &shown {
        print_row(theme, row);
    }
    summary(theme, diagnosis, only_disagreements);
}

/// The environment and the channel: what was asked, and of what.
fn preamble(theme: &Theme, diagnosis: &Diagnosis<'_>) {
    let label = |text: &str| theme.paint(&pad(text, 14), Ink::Muted);

    println!(
        "  {}{}",
        label("$TERM"),
        diagnosis.term.as_deref().unwrap_or("unset")
    );
    if let Some(program) = &diagnosis.term_program {
        println!("  {}{program}", label("TERM_PROGRAM"));
    }
    println!(
        "  {}{}",
        label("terminfo"),
        theme.paint(
            &diagnosis.terminfo.explanation(),
            if diagnosis.terminfo.entry().is_some() {
                Ink::Strong
            } else {
                Ink::Warn
            }
        )
    );
    match diagnosis.channel {
        Some(channel) => println!(
            "  {}{}  {}",
            label("probe channel"),
            theme.paint(channel.label(), Ink::Strong),
            theme.paint(
                &format!("timeout {} ms", diagnosis.timeout.as_millis()),
                Ink::Muted
            )
        ),
        None => println!(
            "  {}{}",
            label("probe channel"),
            theme.paint("none", Ink::Bad)
        ),
    }
    if let Some(multiplexer) = diagnosis.multiplexer {
        println!(
            "  {}{}",
            label("multiplexer"),
            theme.paint(multiplexer.name(), Ink::Warn)
        );
    }
    println!();
}

/// Anything that makes the whole run less trustworthy than it looks.
#[allow(
    clippy::too_many_lines,
    reason = "a flat sequence of independent caveats; each is short and none depends on another"
)]
fn warnings(theme: &Theme, diagnosis: &Diagnosis<'_>) {
    let mut said_anything = false;

    if let Some(failure) = &diagnosis.no_channel {
        println!(
            "  {} {}",
            theme.paint("✘", Ink::Bad),
            theme.paint("the terminal could not be asked", Ink::Bad)
        );
        for line in crate::features::wrap(&failure.reason, theme.width().saturating_sub(4)) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
        if let Some(hint) = &failure.hint {
            for line in crate::features::wrap(hint, theme.width().saturating_sub(4)) {
                println!("    {}", theme.paint(&line, Ink::Muted));
            }
        }
        for line in crate::features::wrap(
            "Only terminfo and the environment answered, so two of the four sources are missing \
             and nothing was cross-checked against the terminal itself.",
            theme.width().saturating_sub(4),
        ) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
        println!();
        said_anything = true;
    } else if !diagnosis.probed() {
        println!(
            "  {} {}",
            theme.paint("✘", Ink::Bad),
            theme.paint("the terminal never answered", Ink::Bad)
        );
        for line in crate::features::wrap(
            "A channel was opened but nothing came back — not even the device-attributes barrier \
             every terminal since the VT100 answers. Something between here and the terminal is \
             swallowing replies.",
            theme.width().saturating_sub(4),
        ) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
        println!();
        said_anything = true;
    }

    if let Some(warning) = diagnosis.sentinel.warning() {
        println!(
            "  {} {}",
            theme.paint("!", Ink::Warn),
            theme.paint("DECRQM answers here cannot be trusted", Ink::Warn)
        );
        for line in crate::features::wrap(&warning, theme.width().saturating_sub(4)) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
        said_anything = true;
        println!();
    } else if diagnosis.sentinel.trust() == Trust::Trusted {
        let control = format!(
            "DECRQM control: the terminal ignored mode {}, which does not exist, so its mode \
             answers mean what they say",
            decrqm::SENTINEL_MODE
        );
        let mut lines =
            crate::features::wrap(&control, theme.width().saturating_sub(4)).into_iter();
        if let Some(first) = lines.next() {
            println!(
                "  {} {}",
                theme.paint("✔", Ink::Good),
                theme.paint(&first, Ink::Muted)
            );
        }
        for line in lines {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
        said_anything = true;
    }

    if diagnosis.decrqm_not_implemented {
        for line in crate::features::wrap(
            "This terminal implements no DECRQM at all: the barrier came back with no mode report \
             in front of it. Konsole behaves this way, and it is a permanent property rather than \
             a slow reply — so every mode row below is unknowable here, not negative.",
            theme.width().saturating_sub(4),
        ) {
            println!("  {}", theme.paint(&line, Ink::Warn));
        }
        said_anything = true;
    }

    if let Some(multiplexer) = diagnosis.multiplexer {
        for line in crate::features::wrap(
            &format!(
                "Inside {}, every source below describes {} rather than the terminal beyond it — \
                 which is correct, because the multiplexer is what lays out the cells. But it also \
                 answers the device-attributes barrier itself, so a silence here licenses no \
                 negative conclusion. Passthrough is not offered: a wrapped query is no longer \
                 ordered behind the barrier that is supposed to fence it.",
                multiplexer.name(),
                multiplexer.name()
            ),
            theme.width().saturating_sub(4),
        ) {
            println!("  {}", theme.paint(&line, Ink::Warn));
        }
        said_anything = true;
    }

    if said_anything {
        println!();
    }
}

/// The column headings.
fn header(theme: &Theme) {
    let mut line = format!("  {}", pad("check", NAME_WIDTH));
    for origin in Origin::ALL {
        line.push_str(&pad(origin.label(), CELL_WIDTH));
    }
    println!("{}", theme.paint(&line, Ink::Accent));
    println!(
        "  {}",
        theme.paint(
            &"─".repeat(NAME_WIDTH + CELL_WIDTH * Origin::ALL.len()),
            Ink::Muted
        )
    );
}

/// One check: the four columns, then whatever the reader needs to act on.
fn print_row(theme: &Theme, row: &Row<'_>) {
    let name_ink = match row.agreement {
        Agreement::Disagree => Ink::Strong,
        _ => Ink::Muted,
    };
    let mut line = format!(
        "  {}",
        theme.paint(&pad(&row.check.id, NAME_WIDTH), name_ink)
    );
    for origin in Origin::ALL {
        let cell = match row.reading(origin) {
            Some(reading) => {
                // An unreliable answer gets its own symbol rather than the
                // claim's, so `--no-color` output cannot present a meaningless
                // answer as a confident one.
                let symbol = if reading.reliable {
                    reading.claim.symbol()
                } else {
                    "!"
                };
                let text = format!("{symbol} {}", reading.claim.label());
                let ink = cell_ink(&reading.claim, reading.reliable);
                // Padding is applied before painting, since a painted string
                // carries escape sequences its length would count.
                theme.paint(&pad(&text, CELL_WIDTH), ink)
            }
            None => pad("", CELL_WIDTH),
        };
        line.push_str(&cell);
    }
    match row.agreement {
        Agreement::Disagree => {
            line.push_str(&theme.paint(&format!("  ⚠ {}", Agreement::Disagree.label()), Ink::Bad));
        }
        Agreement::NotComparable => {
            line.push_str(&theme.paint(
                &format!("  – {}", Agreement::NotComparable.label()),
                Ink::Muted,
            ));
        }
        Agreement::Agree => {}
    }
    println!("{line}");

    if row.agreement != Agreement::Disagree {
        return;
    }

    // Everything below here is only printed for a row worth reading.
    let indent = " ".repeat(4);
    println!(
        "{indent}{}",
        theme.paint(&super::verdict::describe_split(&row.readings), Ink::Bad)
    );
    for origin in Origin::ALL {
        let Some(reading) = row.reading(origin) else {
            continue;
        };
        let mut detail = String::new();
        if let Some(raw) = &reading.raw {
            detail.push_str(raw);
        }
        if let Some(explanation) = &reading.detail {
            if !detail.is_empty() {
                detail.push_str(" — ");
            }
            detail.push_str(explanation);
        }
        if detail.is_empty() {
            continue;
        }
        let label = theme.paint(&pad(origin.label(), 10), Ink::Muted);
        let wrapped = crate::features::wrap(&detail, theme.width().saturating_sub(16));
        for (index, line) in wrapped.iter().enumerate() {
            if index == 0 {
                println!("{indent}{label}{}", theme.paint(line, Ink::Code));
            } else {
                println!("{indent}{}{}", " ".repeat(10), theme.paint(line, Ink::Code));
            }
        }
    }
    println!("{indent}{}", theme.paint("what it means", Ink::Accent));
    let rendered = theme.markdown_indented(&reflow(&row.check.notes), indent.len() + 2);
    // The renderer does not terminate its last line, and without this the next
    // row of the table is printed onto the end of the note.
    println!("{}", rendered.trim_end());
}

/// Collapses the hard line breaks inside each paragraph of a note.
///
/// The data file wraps its prose for readability, and the markdown renderer then
/// wraps it again to the terminal width — reflowing lines that were already broken
/// produces ragged output with words stranded on their own line. Paragraph breaks
/// (blank lines) are kept, since they are the only structure the renderer needs.
fn reflow(notes: &str) -> String {
    notes
        .split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Which colour a cell gets.
fn cell_ink(claim: &Claim, reliable: bool) -> Ink {
    if !reliable {
        return Ink::Warn;
    }
    match claim {
        Claim::Says(text) if text == "yes" => Ink::Good,
        Claim::Says(text) if text == "no" => Ink::Bad,
        Claim::Says(_) => Ink::Strong,
        Claim::NotApplicable | Claim::Silent => Ink::Muted,
    }
}

/// The closing tally, and what the exit status will be.
fn summary(theme: &Theme, diagnosis: &Diagnosis<'_>, only_disagreements: bool) {
    let disagreements = diagnosis.disagreements();
    let incomparable = diagnosis.incomparable();
    let total = diagnosis.rows.len();

    println!();
    let headline = if disagreements == 0 {
        theme.paint(
            &format!("no disagreements across {total} checks"),
            Ink::Good,
        )
    } else {
        theme.paint(
            &format!(
                "{disagreements} disagreement{} across {total} checks",
                if disagreements == 1 { "" } else { "s" }
            ),
            Ink::Bad,
        )
    };
    let mut line = format!("  {headline}");
    if incomparable > 0 {
        line.push_str(&theme.paint(
            &format!(" · {incomparable} could not be compared, for want of a second source"),
            Ink::Muted,
        ));
    }
    println!("{line}");

    let (code, meaning) = if diagnosis.probed() {
        if disagreements > 0 {
            (1, "at least one disagreement was found")
        } else {
            (0, "every check ran and the sources agree")
        }
    } else {
        (
            2,
            "the checks could not be run — there was no terminal to ask",
        )
    };
    println!(
        "  {} {}",
        theme.paint(&format!("exit {code}"), Ink::Accent),
        theme.paint(meaning, Ink::Muted)
    );

    if disagreements > 0 && !only_disagreements {
        println!(
            "  {}  {}",
            theme.paint("escapade doctor --disagreements", Ink::Code),
            theme.paint("shows only those rows", Ink::Muted)
        );
    }
    println!();
}

/// Pads to `width` display columns, truncating with an ellipsis if it does not fit.
///
/// Measured in characters rather than bytes, and never applied to painted text —
/// a painted string carries escape sequences its length would count.
fn pad(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count >= width {
        let kept: String = text.chars().take(width.saturating_sub(2)).collect();
        return format!("{kept}… ");
    }
    format!("{text}{}", " ".repeat(width - count))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        decrqm::{ModeState, Sentinel},
        features::{
            Support,
            doctor::{
                Row,
                session::{Channel, NoChannel},
                terminfo,
                verdict::{Reading, agreement},
            },
        },
        term::Multiplexer,
    };

    fn catalogue() -> Catalogue {
        Catalogue::load().unwrap()
    }

    fn rows(catalogue: &Catalogue) -> Vec<Row<'_>> {
        catalogue
            .checks()
            .iter()
            .enumerate()
            .map(|(index, check)| {
                // A deliberate mixture: agreement, disagreement, silence, a
                // not-applicable source and an unreliable reading.
                let readings = match index % 4 {
                    0 => vec![
                        Reading::new(Origin::Terminfo, Claim::from_support(Support::No))
                            .with_detail("the entry has no RGB"),
                        Reading::new(Origin::Environment, Claim::from_support(Support::Yes))
                            .with_raw("COLORTERM=truecolor"),
                    ],
                    1 => vec![
                        Reading::new(Origin::Terminfo, Claim::from_support(Support::Yes)),
                        Reading::new(Origin::Observed, Claim::from_support(Support::Yes)),
                    ],
                    2 => vec![
                        Reading::not_applicable(Origin::Terminfo, "no capability exists"),
                        Reading::silent(Origin::Observed, "nothing answered"),
                    ],
                    _ => vec![
                        Reading::new(Origin::Terminfo, Claim::value("^?")),
                        Reading::new(Origin::Observed, Claim::value("^H"))
                            .unreliable("meaningless here"),
                    ],
                };
                let agreement = agreement(&readings);
                Row {
                    check,
                    readings,
                    agreement,
                }
            })
            .collect()
    }

    fn diagnosis(rows: Vec<Row<'_>>) -> Diagnosis<'_> {
        Diagnosis {
            rows,
            terminfo: terminfo::Source::Entry {
                entry: Box::new(terminfo::parse(
                    "xterm-256color|xterm with 256 colors,\n\tam,\n",
                )),
                queried: "xterm-256color".to_owned(),
            },
            channel: Some(Channel::Ctty),
            no_channel: None,
            multiplexer: None,
            sentinel: Sentinel {
                state: None,
                answered_barrier: true,
            },
            heard_anything: true,
            decrqm_not_implemented: false,
            timeout: Duration::from_secs(1),
            term: Some("xterm-256color".to_owned()),
            term_program: Some("iTerm.app".to_owned()),
        }
    }

    #[test]
    fn rendering_every_shape_of_row_never_panics() {
        let catalogue = catalogue();
        let theme = Theme::plain();
        let diagnosis = diagnosis(rows(&catalogue));
        render(&theme, &diagnosis, false);
        render(&theme, &diagnosis, true);
        list(&theme, &catalogue);
    }

    #[test]
    fn rendering_the_unreachable_terminal_path_never_panics() {
        let catalogue = catalogue();
        let theme = Theme::plain();
        let mut diagnosis = diagnosis(rows(&catalogue));
        diagnosis.channel = None;
        diagnosis.heard_anything = false;
        diagnosis.no_channel = Some(NoChannel {
            reason: "On the stdio channel, standard output is not a terminal.".to_owned(),
            hint: Some("Try --probe-mode=ctty.".to_owned()),
        });
        diagnosis.terminfo = terminfo::Source::NoInfocmp;
        diagnosis.sentinel = Sentinel::default();
        render(&theme, &diagnosis, false);
        render(&theme, &diagnosis, true);
    }

    #[test]
    fn rendering_a_compromised_sentinel_and_a_multiplexer_never_panics() {
        let catalogue = catalogue();
        let theme = Theme::plain();
        let mut diagnosis = diagnosis(rows(&catalogue));
        diagnosis.multiplexer = Some(Multiplexer::Tmux);
        diagnosis.sentinel = Sentinel {
            state: Some(ModeState::PermanentlyReset),
            answered_barrier: true,
        };
        diagnosis.decrqm_not_implemented = true;
        render(&theme, &diagnosis, false);
    }

    #[test]
    fn rendering_no_rows_at_all_never_panics() {
        let theme = Theme::plain();
        let diagnosis = diagnosis(Vec::new());
        render(&theme, &diagnosis, false);
        render(&theme, &diagnosis, true);
    }

    #[test]
    fn agreement_is_quiet_and_disagreement_is_not() {
        // The colours are what carry this, but the marker has to be there in plain
        // mode too, or `--no-color` output would hide the finding.
        let disagreeing = vec![
            Reading::new(Origin::Terminfo, Claim::from_support(Support::No)),
            Reading::new(Origin::Observed, Claim::from_support(Support::Yes)),
        ];
        let agreement = agreement(&disagreeing);
        assert_eq!(agreement, Agreement::Disagree);
        assert_eq!(agreement.label(), "DISAGREE");
        assert_eq!(Agreement::Agree.label(), "agree");
    }

    #[test]
    fn a_cell_is_coloured_by_what_it_says_and_whether_it_can_be_trusted() {
        assert_eq!(
            cell_ink(&Claim::from_support(Support::Yes), true),
            Ink::Good
        );
        assert_eq!(cell_ink(&Claim::from_support(Support::No), true), Ink::Bad);
        assert_eq!(cell_ink(&Claim::Silent, true), Ink::Muted);
        assert_eq!(cell_ink(&Claim::NotApplicable, true), Ink::Muted);
        assert_eq!(cell_ink(&Claim::value("^?"), true), Ink::Strong);
        // Unreliable wins over everything: a green tick that means nothing is
        // worse than no tick at all.
        assert_eq!(
            cell_ink(&Claim::from_support(Support::Yes), false),
            Ink::Warn
        );
    }

    #[test]
    fn padding_keeps_the_columns_aligned() {
        assert_eq!(pad("abc", 6), "abc   ");
        assert_eq!(pad("", 3), "   ");
        assert_eq!(pad("abcdef", 6).chars().count(), 6);
        // Over-long text is truncated rather than shifting every later column.
        assert_eq!(pad("abcdefghij", 6).chars().count(), 6);
        assert!(pad("abcdefghij", 6).contains('…'));
    }

    #[test]
    fn padding_counts_characters_rather_than_bytes() {
        // A check id is ASCII, but a family token or an erase spelling need not be.
        assert_eq!(pad("✔ yes", 8).chars().count(), 8);
    }

    #[test]
    fn the_table_fits_the_narrowest_theme_width() {
        // Theme clamps to at least 40 columns; the name column plus four cells is
        // wider than that, so the header rule must not be built from the width.
        let width = NAME_WIDTH + CELL_WIDTH * Origin::ALL.len();
        assert!(
            width > 40,
            "the rule is drawn from the columns, not the terminal"
        );
        assert_eq!(width, 20 + 56);
    }

    #[test]
    fn every_check_in_the_catalogue_has_a_question_the_list_can_print() {
        let catalogue = catalogue();
        for check in catalogue.checks() {
            assert!(!check.question.is_empty(), "{}", check.id);
            assert!(
                check.id.chars().count() <= NAME_WIDTH,
                "{} is too wide",
                check.id
            );
        }
    }
}
