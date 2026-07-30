//! `escapade grapheme probe <CODEPOINTS>` — does this terminal treat the sequence
//! as one character?
//!
//! The same verb as the bare mode probe, aimed at a concrete sequence, which is
//! the more useful question: an application does not care whether mode 2027 exists,
//! it cares how many cells *this* string will take.
//!
//! Silent by default, like every `probe`: the exit status is the answer. See
//! [`Finding`].
//!
//! The codepoint list is read with [`unicode::parse_sequence`], **not**
//! `parse_spec`. Here the list is *text*: `U+1F468,U+200D,U+1F469,U+200D,U+1F467`
//! is a family emoji that legitimately repeats U+200D, and the de-duplicating
//! parser would quietly turn it into something else.
//!
//! Each grapheme cluster gets its own row, and each row carries three verdicts'
//! worth of honesty:
//!
//! * **measured** — the terminal answered, and the cell count either matched the
//!   segmentation-aware prediction or it did not.
//! * **not supported** — the terminal answered, and it counted per codepoint.
//! * **unknown** — nothing answered. That is not a no; it is an absence, and the
//!   two are printed differently.

use anyhow::{Context as _, Result};

use crate::{
    cli::ProbeFormat,
    features::{
        grapheme::{
            Finding, ModeState, Reliability,
            cluster::{self, Assembly, Cluster},
            json,
            measure::{self, Comparison, ModeReport, Ruler, Verdict},
            probe::{comparison_lines, verdict_sentence},
        },
        wrap,
    },
    term::Multiplexer,
    theme::{Ink, Theme},
    unicode::{self, CharInfo},
};

/// One row of the report: a cluster and what the terminal did with it.
#[derive(Debug, Clone)]
pub struct Row {
    /// The cluster and its two predicted widths.
    pub cluster: Cluster,
    /// What was measured, if anything.
    pub comparison: Comparison,
}

impl Row {
    /// The verdict for this cluster.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        self.comparison.verdict()
    }
}

/// Everything a report needs, gathered before the terminal is handed back so the
/// report itself runs in a normal cooked-mode terminal.
#[derive(Debug)]
pub struct Findings {
    /// One entry per grapheme cluster, in order.
    pub rows: Vec<Row>,
    /// The whole sequence measured in one go, which is what an application would
    /// actually print.
    pub whole: Comparison,
    /// What DECRQM said, and what the control says it is worth.
    pub mode: ModeReport,
    /// Whether there was a terminal to measure against.
    pub interactive: bool,
    /// A multiplexer in the way, if any.
    pub multiplexer: Option<Multiplexer>,
}

/// What the measured sequence proved, for the exit status.
///
/// Every cluster has to come out right for this to be a yes. A string is laid out
/// as a whole, and one cluster the terminal miscounts is enough to shift everything
/// after it — so "most of them worked" is not a useful answer to give a script.
#[must_use]
pub fn finding(findings: &Findings) -> Finding {
    if findings.rows.is_empty() {
        return Finding::Undetermined;
    }
    // Any measured disagreement is a definite no.
    if findings
        .rows
        .iter()
        .any(|row| matches!(row.verdict(), Verdict::NotClustered | Verdict::Disagrees(_)))
    {
        return Finding::Unsupported;
    }
    // Otherwise every row must have been measured, and the sequence must have been
    // capable of telling the two width rules apart in the first place.
    if findings
        .rows
        .iter()
        .any(|row| row.verdict() == Verdict::Unknown)
    {
        return Finding::Undetermined;
    }
    if findings.rows.iter().any(|row| row.cluster.miscounted()) {
        Finding::Supported
    } else {
        // Nothing here could have distinguished a clustering terminal from a
        // `wcwidth` one, so the clean measurement proves nothing.
        Finding::Undetermined
    }
}

/// Runs `escapade grapheme probe <CODEPOINTS>`, reporting only if asked to.
///
/// # Errors
///
/// Fails if the codepoint specification cannot be parsed, or if the terminal
/// cannot be set up. A terminal that declines to answer is a result, not an error.
pub fn report(
    theme: &Theme,
    spec: &str,
    format: Option<ProbeFormat>,
    long: bool,
) -> Result<Finding> {
    let codepoints = unicode::parse_sequence(spec)
        .with_context(|| format!("parsing codepoint sequence `{spec}`"))?;
    let assembly = cluster::assemble(&codepoints);

    let Some(text) = assembly.text.as_deref() else {
        // Part of the sequence must never be written, so nothing was measured and
        // there is nothing to conclude. This one always speaks up, whatever the
        // format: silently exiting `2` would leave the user with no idea why.
        refusal(theme, &assembly);
        return Ok(Finding::Undetermined);
    };

    let findings = measure(text)?;
    let finding = finding(&findings);

    match format {
        None => {}
        Some(ProbeFormat::Json) => {
            for line in json_lines(&findings, finding, long) {
                println!("{}", line.as_str());
            }
        }
        Some(ProbeFormat::Debug) => {
            println!();
            println!(
                "  {}",
                theme.paint("Grapheme clustering for this sequence", Ink::Title)
            );
            println!();
            print_rows(theme, &findings.rows, long);
            print_whole(theme, &findings);
            summarize(theme, &findings, finding);
        }
    }
    Ok(finding)
}

/// The `--format json` output: one object per cluster, then one summary object.
#[must_use]
pub fn json_lines(findings: &Findings, finding: Finding, long: bool) -> Vec<json::Value> {
    let mut lines: Vec<json::Value> = findings
        .rows
        .iter()
        .map(|row| {
            let mut fields = vec![
                ("type", json::string("cluster")),
                ("text", json::string(&row.cluster.text)),
                (
                    "codepoints",
                    json::array(
                        &row.cluster
                            .codepoints
                            .iter()
                            .map(|info| json::string(&info.label()))
                            .collect::<Vec<_>>(),
                    ),
                ),
                ("expected", json::number(row.cluster.expected)),
                ("wcwidth", json::number(row.cluster.legacy)),
                (
                    "measured",
                    json::optional_number(row.comparison.clustering_on),
                ),
                (
                    "measured_unclustered",
                    json::optional_number(row.comparison.clustering_off),
                ),
                ("verdict", json::string(row.verdict().label())),
                ("decisive", json::boolean(row.cluster.miscounted())),
            ];
            if long {
                fields.push((
                    "breakdown",
                    json::array(
                        &row.cluster
                            .codepoints
                            .iter()
                            .map(|info| {
                                json::object(&[
                                    ("codepoint", json::string(&info.label())),
                                    ("name", json::string(&info.title_name())),
                                    ("wcwidth", json::number(info.width)),
                                    ("emoji", json::boolean(info.emoji)),
                                    ("role", json::optional_string(cluster::breakdown_note(info))),
                                ])
                            })
                            .collect::<Vec<_>>(),
                    ),
                ));
            }
            json::object(&fields)
        })
        .collect();

    lines.push(json::object(&[
        ("type", json::string("summary")),
        ("feature", json::string(super::FEATURE_ID)),
        ("finding", json::string(finding.json_value())),
        ("clusters", json::number(findings.rows.len())),
        (
            "expected",
            json::number(cluster::expected_width(
                findings.rows.iter().map(|row| &row.cluster),
            )),
        ),
        (
            "wcwidth",
            json::number(cluster::legacy_total(
                findings.rows.iter().map(|row| &row.cluster),
            )),
        ),
        (
            "measured",
            json::optional_number(findings.whole.clustering_on),
        ),
        (
            "measured_unclustered",
            json::optional_number(findings.whole.clustering_off),
        ),
        (
            "mode_state",
            json::optional_string(findings.mode.trusted_state().map(ModeState::label)),
        ),
        (
            "reliability",
            json::string(findings.mode.reliability().json_value()),
        ),
        ("interactive", json::boolean(findings.interactive)),
        (
            "multiplexer",
            json::optional_string(findings.multiplexer.map(Multiplexer::name)),
        ),
    ]));
    lines
}

/// Opens one measuring session and measures the whole sequence and every cluster.
fn measure(text: &str) -> Result<Findings> {
    let clusters = cluster::analyze(text);
    let Some(mut ruler) = Ruler::open()? else {
        return Ok(Findings {
            rows: clusters
                .into_iter()
                .map(|cluster| Row {
                    comparison: Comparison::unmeasured(&cluster.text),
                    cluster,
                })
                .collect(),
            whole: Comparison::unmeasured(text),
            mode: ModeReport::default(),
            interactive: false,
            multiplexer: Multiplexer::detect(),
        });
    };

    let mode = ruler.entry_report();
    let multiplexer = ruler.multiplexer();
    let whole = ruler.compare(text)?;

    let single = clusters.len() == 1;
    let mut rows = Vec::with_capacity(clusters.len());
    for cluster in clusters {
        let comparison = if single {
            // One cluster means the whole-sequence measurement already *is* the
            // per-cluster one; measuring again would only cost round trips.
            whole
        } else if ruler.silent() {
            // The terminal has stopped answering. Every further attempt costs a
            // full timeout and tells us exactly what the first one did.
            Comparison::unmeasured(&cluster.text)
        } else {
            ruler.compare(&cluster.text)?
        };
        rows.push(Row {
            cluster,
            comparison,
        });
    }
    ruler.close();

    Ok(Findings {
        rows,
        whole,
        mode,
        interactive: true,
        multiplexer,
    })
}

/// Prints the "this cannot be sent" path.
///
/// Always prints, whatever `--format` says: exiting `2` in silence would leave the
/// user with no idea that their sequence was refused rather than merely
/// unmeasurable.
fn refusal(theme: &Theme, assembly: &Assembly) {
    debug_assert!(!assembly.sendable());
    println!(
        "  {} {}",
        theme.paint("✘", Ink::Bad),
        theme.paint("refusing to send this sequence", Ink::Bad)
    );
    println!();
    for info in &assembly.refused {
        println!(
            "    {}  {}",
            theme.paint(&info.label(), Ink::Accent),
            theme.paint(&info.title_name(), Ink::Strong)
        );
    }
    println!();
    for line in wrap(
        "Measuring means printing. A control character, a surrogate or an unassigned codepoint \
         cannot be printed safely — a raw ESC would execute whatever bytes followed it — so nothing \
         was sent and nothing was measured. Joiners and variation selectors are fine; those are \
         what clusters are made of.",
        theme.width().saturating_sub(4),
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();
}

/// One line per cluster, plus the per-codepoint breakdown under `--long`.
fn print_rows(theme: &Theme, rows: &[Row], long: bool) {
    for (index, row) in rows.iter().enumerate() {
        if long && index > 0 {
            println!();
        }
        let verdict = row.verdict();
        let ink = verdict_ink(verdict);
        let labels: Vec<String> = row.cluster.codepoints.iter().map(CharInfo::label).collect();

        println!(
            "  {} {}  {}",
            theme.paint(verdict.symbol(), ink),
            theme.paint(&labels.join(" "), Ink::Code),
            render_cluster(&row.cluster)
        );
        println!(
            "      {} {}   {} {}   {} {}",
            theme.paint("expected", Ink::Muted),
            theme.paint(&measure::cells(row.cluster.expected), Ink::Accent),
            theme.paint("measured", Ink::Muted),
            theme.paint(&measure::measured_cells(row.comparison.clustering_on), ink),
            theme.paint("per codepoint", Ink::Muted),
            theme.paint(&measure::cells(row.cluster.legacy), Ink::Muted),
        );
        println!(
            "      {} {}",
            theme.paint(verdict.label(), ink),
            theme.paint(
                &match row.cluster.miscount_direction() {
                    Some(direction) => format!("· {direction} here"),
                    None => "· both rules agree on this cluster, so it decides nothing".to_owned(),
                },
                Ink::Muted
            )
        );

        if long {
            for info in &row.cluster.codepoints {
                println!(
                    "        {}",
                    theme.paint(&cluster::breakdown_line(info), Ink::Muted)
                );
                println!("          {}", theme.paint(&details(info), Ink::Muted));
            }
        }
    }
}

/// The cluster itself, or a placeholder when it holds nothing printable.
///
/// A cluster made only of format characters — a lone U+200D, say — has no visible
/// form, and printing it would leave a confusing gap rather than a character.
fn render_cluster(cluster: &Cluster) -> String {
    if cluster.codepoints.iter().any(CharInfo::printable) {
        cluster.text.clone()
    } else {
        cluster::PLACEHOLDER.to_owned()
    }
}

/// Extra Unicode facts for `--long`.
fn details(info: &CharInfo) -> String {
    let mut parts = Vec::new();
    if let Some(character) = info.character {
        parts.push(unicode::category_group_name(character).to_owned());
    }
    parts.push(format!("wcwidth {}", info.width));
    if info.emoji {
        parts.push("emoji".to_owned());
    }
    if !info.named {
        parts.push("unnamed".to_owned());
    }
    format!("[{}]", parts.join(", "))
}

/// The whole-sequence measurement, which is what an application would print.
fn print_whole(theme: &Theme, findings: &Findings) {
    if findings.rows.len() < 2 {
        return;
    }
    println!();
    println!(
        "  {}",
        theme.paint("the whole sequence, printed in one go", Ink::Accent)
    );
    for line in comparison_lines(theme, &findings.whole) {
        println!("{line}");
    }

    // Summing the per-cluster numbers is the same arithmetic done locally, and it
    // is worth showing next to the measurement: when the two disagree, something
    // about the terminal's segmentation differs from ours, not just its widths.
    let clusters = || findings.rows.iter().map(|row| &row.cluster);
    let miscounted = clusters().filter(|cluster| cluster.miscounted()).count();
    println!(
        "    {}  {}",
        theme.paint("summed per cluster", Ink::Muted),
        theme.paint(
            &format!(
                "{} expected, {} per codepoint · {miscounted} of {} clusters would be miscounted",
                measure::cells(cluster::expected_width(clusters())),
                measure::cells(cluster::legacy_total(clusters())),
                findings.rows.len()
            ),
            Ink::Muted
        )
    );
}

/// How many rows landed on each verdict.
///
/// `Disagrees` carries a payload, so the rows are bucketed by discriminant rather
/// than by equality — a terminal answering 5 and one answering 6 belong in the
/// same bucket.
fn tally(rows: &[Row]) -> [usize; 4] {
    let mut counts = [0usize; 4];
    for row in rows {
        let bucket = match row.verdict() {
            Verdict::Clustered => 0,
            Verdict::NotClustered => 1,
            Verdict::Disagrees(_) => 2,
            Verdict::Unknown => 3,
        };
        counts[bucket] += 1;
    }
    counts
}

/// The closing tally, the mode state, and any reason the answers are unknown.
fn summarize(theme: &Theme, findings: &Findings, finding: Finding) {
    let rows = &findings.rows;
    let [clustered, per_codepoint, disagrees, unknown] = tally(rows);

    println!();
    let mut parts = Vec::new();
    for (count, label, ink) in [
        (clustered, "clustered", Ink::Good),
        (per_codepoint, "per codepoint", Ink::Bad),
        (disagrees, "disagreeing", Ink::Warn),
        (unknown, "unknown", Ink::Muted),
    ] {
        if count > 0 {
            parts.push(theme.paint(&format!("{count} {label}"), ink));
        }
    }
    if parts.is_empty() {
        parts.push(theme.paint("nothing to measure", Ink::Muted));
    }
    println!(
        "  {} {} {}",
        parts.join(theme.paint(" · ", Ink::Muted).as_str()),
        theme.paint("of", Ink::Muted),
        theme.paint(
            &format!(
                "{} grapheme {}",
                rows.len(),
                if rows.len() == 1 {
                    "cluster"
                } else {
                    "clusters"
                }
            ),
            Ink::Muted
        )
    );

    println!();
    println!(
        "  {} {}",
        theme.paint("mode 2027:", Ink::Muted),
        // `trusted_state`, not `state`: on a terminal that answers the same way for
        // modes it has never heard of, printing its answer here would dress noise
        // up as a finding.
        match findings.mode.trusted_state() {
            Some(state) => theme.paint(state.label(), mode_ink(state)),
            None => match findings.mode.reliability() {
                Reliability::Indiscriminate(_) => theme.paint(
                    "DECRQM answers discarded — this terminal answers the same for modes that do \
                     not exist",
                    Ink::Bad
                ),
                Reliability::NoDecrqm =>
                    theme.paint("this terminal implements no DECRQM", Ink::Bad),
                Reliability::Silent => theme.paint("not asked — nothing answered", Ink::Muted),
                Reliability::Discriminating =>
                    theme.paint("no DECRQM answer for mode 2027", Ink::Bad),
            },
        }
    );

    conclusion(theme, findings, unknown);

    if let Some(multiplexer) = findings.multiplexer {
        println!();
        println!(
            "  {} {}",
            theme.paint("note:", Ink::Warn),
            theme.paint(
                &format!(
                    "{} lays out the cells here, so these are its widths, not the outer terminal's.",
                    multiplexer.name()
                ),
                Ink::Muted
            )
        );
    }

    // The same word the exit status carries, so `--format debug` and a shell
    // predicate can never be read as disagreeing.
    println!();
    println!(
        "  {} {}",
        theme.paint("finding:", Ink::Muted),
        theme.paint(
            finding.label(),
            match finding {
                Finding::Supported => Ink::Good,
                Finding::Unsupported => Ink::Bad,
                Finding::Undetermined => Ink::Muted,
            }
        )
    );

    println!();
    println!(
        "  {}",
        theme.paint(
            &format!(
                "Unicode {} · escapade grapheme explain",
                unicode::unicode_version()
            ),
            Ink::Muted
        )
    );
    println!();
}

/// The closing paragraph: either why everything is unknown, or what was proved.
fn conclusion(theme: &Theme, findings: &Findings, unknown: usize) {
    let rows = &findings.rows;
    let say = |text: &str| {
        for line in wrap(text, theme.width().saturating_sub(4)) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    };

    if unknown == rows.len() && !rows.is_empty() {
        println!();
        say(if findings.interactive {
            "Every answer is unknown because the terminal never returned a cursor position report, \
             so there was no way to see what it did."
        } else {
            "Every answer is unknown because stdin and stdout are not both terminals. Measuring \
             means printing the sequence and reading back where the cursor went, and a pipe has \
             nowhere to read from."
        });
        say(
            "Unknown is not the same as unsupported: the expected and per-codepoint widths above \
             are computed locally and remain correct, and the terminal may well cluster perfectly.",
        );
    } else if let Some(row) = rows.iter().find(|row| row.verdict() != Verdict::Unknown) {
        println!();
        say(verdict_sentence(row.verdict(), &row.comparison));
    }
}

/// The color for a verdict.
fn verdict_ink(verdict: Verdict) -> Ink {
    match verdict {
        Verdict::Clustered => Ink::Good,
        Verdict::NotClustered => Ink::Bad,
        Verdict::Disagrees(_) => Ink::Warn,
        Verdict::Unknown => Ink::Muted,
    }
}

/// The color for a mode state.
fn mode_ink(state: ModeState) -> Ink {
    if state.enabled() {
        Ink::Good
    } else if state.recognized() {
        Ink::Warn
    } else {
        Ink::Bad
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn row(text: &str, on: Option<usize>) -> Row {
        let cluster = cluster::analyze(text)
            .into_iter()
            .next()
            .unwrap_or_else(|| Cluster {
                text: String::new(),
                codepoints: Vec::new(),
                expected: 0,
                legacy: 0,
            });
        let mut comparison = Comparison::unmeasured(&cluster.text);
        comparison.clustering_off = Some(comparison.legacy);
        comparison.clustering_on = on;
        Row {
            cluster,
            comparison,
        }
    }

    #[test]
    fn the_family_emoji_survives_parsing_with_its_repeats() {
        // `parse_spec` would collapse the two U+200D and turn the family into
        // something else. This is why `supports` uses `parse_sequence`.
        let spec = "U+1F468,U+200D,U+1F469,U+200D,U+1F467";
        let codepoints = unicode::parse_sequence(spec).unwrap();
        assert_eq!(codepoints.len(), 5);
        assert_eq!(codepoints[1], 0x200D);
        assert_eq!(codepoints[3], 0x200D);

        let deduplicated = unicode::parse_spec(spec).unwrap();
        assert_eq!(deduplicated.len(), 4, "the set parser drops one joiner");

        let assembly = cluster::assemble(&codepoints);
        let clusters = cluster::analyze(assembly.text.as_deref().unwrap_or_default());
        assert_eq!(clusters.len(), 1, "one family, one cluster");
        assert_eq!(clusters[0].expected, 2);
        assert_eq!(clusters[0].legacy, 6);
    }

    #[test]
    fn the_farmer_from_the_brief_is_one_cluster() {
        let codepoints = unicode::parse_sequence("U+1F9D1,U+200D,U+1F33E").unwrap();
        let assembly = cluster::assemble(&codepoints);
        let clusters = cluster::analyze(assembly.text.as_deref().unwrap_or_default());
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].expected, 2);
        assert_eq!(clusters[0].legacy, 4);
    }

    #[test]
    fn a_measured_row_reports_the_verdict_it_measured() {
        assert_eq!(
            row("\u{1f9d1}\u{200d}\u{1f33e}", Some(2)).verdict(),
            Verdict::Clustered
        );
        assert_eq!(
            row("\u{1f9d1}\u{200d}\u{1f33e}", Some(4)).verdict(),
            Verdict::NotClustered
        );
        assert_eq!(
            row("\u{1f9d1}\u{200d}\u{1f33e}", Some(6)).verdict(),
            Verdict::Disagrees(6)
        );
        assert_eq!(
            row("\u{1f9d1}\u{200d}\u{1f33e}", None).verdict(),
            Verdict::Unknown
        );
    }

    #[test]
    fn a_cluster_with_nothing_printable_renders_as_a_placeholder() {
        let joiner = cluster::analyze("\u{200d}").into_iter().next().unwrap();
        assert_eq!(render_cluster(&joiner), cluster::PLACEHOLDER);

        let farmer = cluster::analyze("\u{1f9d1}\u{200d}\u{1f33e}")
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(render_cluster(&farmer), "\u{1f9d1}\u{200d}\u{1f33e}");
    }

    #[test]
    fn a_refused_sequence_is_reported_rather_than_printed() {
        let theme = Theme::plain();
        // U+001B: printing it would execute whatever bytes followed.
        let assembly = cluster::assemble(&[0x41, 0x1B]);
        assert!(assembly.text.is_none(), "must never become sendable text");
        refusal(&theme, &assembly);

        // A safe sequence never reaches the refusal path at all.
        let safe = cluster::assemble(&[0x1_F9D1, 0x200D, 0x1_F33E]);
        assert!(safe.sendable());
    }

    #[test]
    fn long_details_list_the_per_codepoint_facts() {
        let emoji = details(&unicode::describe(0x1_F9D1));
        assert!(emoji.contains("emoji"), "{emoji}");
        assert!(emoji.contains("wcwidth 2"), "{emoji}");

        let joiner = details(&unicode::describe(0x200D));
        assert!(joiner.contains("wcwidth 0"), "{joiner}");
    }

    #[test]
    fn printing_a_report_never_panics_on_awkward_input() {
        let theme = Theme::plain();
        let rows = vec![
            row("A", Some(1)),
            row("\u{1f9d1}\u{200d}\u{1f33e}", Some(2)),
            row("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", Some(6)),
            row("\u{2764}\u{fe0f}", None),
            row("e\u{301}", Some(1)),
            row("\u{200d}", Some(0)),
            row("\u{4e00}", Some(2)),
        ];
        for long in [false, true] {
            print_rows(&theme, &rows, long);
        }
        for interactive in [false, true] {
            for state in [
                None,
                Some(ModeState::Set),
                Some(ModeState::NotRecognized),
                Some(ModeState::PermanentlyReset),
            ] {
                // Both a control that vouches for the state and one that voids it,
                // so the summary line is exercised on either side of the branch.
                let sentinel = if interactive {
                    Some(ModeState::NotRecognized)
                } else {
                    Some(ModeState::PermanentlyReset)
                };
                let findings = Findings {
                    rows: rows.clone(),
                    whole: Comparison::unmeasured("\u{1f9d1}\u{200d}\u{1f33e}"),
                    mode: ModeReport {
                        state,
                        sentinel,
                        answered_barrier: true,
                    },
                    interactive,
                    multiplexer: Some(Multiplexer::Tmux),
                };
                print_whole(&theme, &findings);
                summarize(&theme, &findings, finding(&findings));
            }
        }
        // The all-unknown path, which must say why rather than claim a no.
        let unknown = Findings {
            rows: vec![row("\u{1f9d1}\u{200d}\u{1f33e}", None)],
            whole: Comparison::unmeasured("\u{1f9d1}\u{200d}\u{1f33e}"),
            mode: ModeReport::default(),
            interactive: false,
            multiplexer: None,
        };
        summarize(&theme, &unknown, finding(&unknown));

        // And the empty case, which must not divide by or index into nothing.
        let empty = Findings {
            rows: Vec::new(),
            whole: Comparison::unmeasured(""),
            mode: ModeReport::default(),
            interactive: false,
            multiplexer: None,
        };
        summarize(&theme, &empty, finding(&empty));
        print_whole(&theme, &empty);
        assert_eq!(finding(&empty), Finding::Undetermined);
    }

    fn findings(rows: Vec<Row>) -> Findings {
        Findings {
            rows,
            whole: Comparison::unmeasured("\u{1f9d1}\u{200d}\u{1f33e}"),
            mode: ModeReport {
                state: Some(ModeState::Reset),
                sentinel: Some(ModeState::NotRecognized),
                answered_barrier: true,
            },
            interactive: true,
            multiplexer: None,
        }
    }

    #[test]
    fn one_miscounted_cluster_makes_the_whole_sequence_a_no() {
        // A string is laid out as a whole: a single cluster the terminal gets
        // wrong shifts everything after it, so "two out of three" is not a yes.
        let mixed = findings(vec![
            row("\u{1f9d1}\u{200d}\u{1f33e}", Some(2)),
            row("\u{2764}\u{fe0f}", Some(1)),
        ]);
        assert_eq!(finding(&mixed), Finding::Unsupported);
    }

    #[test]
    fn every_cluster_clustering_is_a_yes() {
        let good = findings(vec![
            row("\u{1f9d1}\u{200d}\u{1f33e}", Some(2)),
            row("\u{2764}\u{fe0f}", Some(2)),
        ]);
        assert_eq!(finding(&good), Finding::Supported);
    }

    #[test]
    fn an_unmeasured_cluster_leaves_the_answer_undetermined() {
        let partial = findings(vec![
            row("\u{1f9d1}\u{200d}\u{1f33e}", Some(2)),
            row("\u{2764}\u{fe0f}", None),
        ]);
        assert_eq!(finding(&partial), Finding::Undetermined);
        assert_ne!(finding(&partial), Finding::Unsupported);
    }

    #[test]
    fn a_sequence_that_could_not_prove_anything_is_undetermined() {
        // `A` and a CJK ideograph are the same width under both rules, so a clean
        // measurement of them says nothing about clustering. Reporting that as
        // "supported" would be the tool overclaiming about its own evidence.
        let undecidable = findings(vec![row("A", Some(1)), row("\u{4e00}", Some(2))]);
        assert_eq!(finding(&undecidable), Finding::Undetermined);
    }

    #[test]
    fn no_clusters_at_all_is_undetermined() {
        assert_eq!(finding(&findings(Vec::new())), Finding::Undetermined);
    }

    #[test]
    fn the_json_output_is_one_object_per_line_plus_a_summary() {
        let measured = findings(vec![
            row("\u{1f9d1}\u{200d}\u{1f33e}", Some(2)),
            row("A", Some(1)),
        ]);
        let lines = json_lines(&measured, finding(&measured), false);
        assert_eq!(lines.len(), 3, "two clusters and one summary");
        for line in &lines {
            let text = line.as_str();
            assert!(!text.contains('\n'), "{text}");
            assert!(text.is_ascii(), "non-ASCII survived escaping: {text}");
            assert!(!text.chars().any(char::is_control), "{text}");
        }
        assert!(lines[0].as_str().contains(r#""type":"cluster""#));
        assert!(lines[0].as_str().contains(r#""expected":2"#));
        assert!(lines[0].as_str().contains(r#""wcwidth":4"#));
        assert!(lines[0].as_str().contains(r#""decisive":true"#));
        assert!(lines[1].as_str().contains(r#""decisive":false"#));
        let summary = lines[2].as_str();
        assert!(summary.contains(r#""type":"summary""#), "{summary}");
        assert!(summary.contains(r#""clusters":2"#), "{summary}");
        assert!(
            summary.contains(r#""reliability":"discriminating""#),
            "{summary}"
        );
    }

    #[test]
    fn long_json_adds_the_per_codepoint_breakdown() {
        let measured = findings(vec![row("\u{1f9d1}\u{200d}\u{1f33e}", Some(2))]);
        let plain = json_lines(&measured, Finding::Supported, false);
        let long = json_lines(&measured, Finding::Supported, true);
        assert!(!plain[0].as_str().contains("breakdown"));
        let text = long[0].as_str();
        assert!(text.contains(r#""breakdown""#), "{text}");
        assert!(text.contains(r#""codepoint":"U+200D""#), "{text}");
        assert!(
            text.contains("joins"),
            "the joiner's role travels with it: {text}"
        );
        assert!(text.is_ascii(), "{text}");
    }

    #[test]
    fn measured_cells_never_reports_a_silence_as_zero() {
        assert_eq!(measure::measured_cells(Some(0)), "0 cells");
        assert_eq!(measure::measured_cells(None), "no answer");
    }
}
