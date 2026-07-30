//! `escapade dimensions probe`: threshold tests, and the general question.
//!
//! Two questions share one verb, the way `escapade glyph probe [CODEPOINTS]`
//! does. With thresholds, the question is "is the terminal at least this big",
//! answered through the exit status so it composes as a shell predicate:
//!
//! ```sh
//! if escapade dimensions probe --rows-at-least 24; then full_screen; else compact; fi
//! ```
//!
//! With no thresholds, the question is whether the size can be determined at all,
//! and by which query — which is what tells a reader whether the pixel numbers
//! their layout wants are ever going to arrive on this terminal.
//!
//! Both answer with [`Support`], so exit 2 stays available for the case this
//! feature exists to get right: a dimension that was asked about and could not be
//! determined is not a dimension that failed its threshold.

use std::process::ExitCode;

use anyhow::Result;

use crate::{
    cli::ProbeFormat,
    features::{
        Support,
        dimensions::{
            query::{self, Wanted, barrier, cell_query, cells_query, window_query},
            size::{Reading, Sizes},
            threshold::{Available, Outcome, Thresholds},
        },
        wrap,
    },
    term::pretty,
    theme::{Ink, Theme},
};

/// Whether the size can be determined at all.
///
/// `No` needs the same care as anywhere else in this project. It is only claimed
/// when the barrier came back — proving the terminal had its chance — and there is
/// no multiplexer answering that barrier on the real terminal's behalf. Anything
/// else is `Unknown`: nothing was heard, so nothing is known.
#[must_use]
pub fn size_verdict(sizes: &Sizes) -> Support {
    if sizes.cells().is_some() {
        Support::Yes
    } else if sizes.answered_barrier && sizes.multiplexer.is_none() {
        Support::No
    } else {
        Support::Unknown
    }
}

/// Runs the probe and reports through the exit status.
///
/// Silent unless a `--format` is given, so it composes as a shell predicate.
///
/// # Errors
///
/// Fails if a dimension's thresholds contradict each other, or if the terminal
/// cannot be set up. A terminal that stays quiet is a result, not an error.
pub fn report(theme: &Theme, thresholds: Thresholds, format: Option<ProbeFormat>) -> Result<ExitCode> {
    thresholds.check()?;

    let bare = thresholds.is_empty();
    let sizes = query::gather(Wanted {
        // A cells-only test is answered by the ioctl, so it need not cost a round
        // trip. The bare form asks regardless, because its whole subject is which
        // queries this terminal answers.
        window_pixels: bare || thresholds.wants_pixels(),
        cell_pixels: bare,
        always_ask: bare,
    })?;

    let outcome = thresholds.evaluate(&Available::from_sizes(&sizes));
    let support = if bare {
        size_verdict(&sizes)
    } else {
        outcome.support()
    };

    match format {
        None => {}
        Some(ProbeFormat::Json) => print!("{}", json_line(&sizes, &outcome, support)),
        Some(ProbeFormat::Debug) => debug_report(theme, &sizes, thresholds, &outcome, support),
    }
    Ok(support.exit_code())
}

/// One JSON object, one line — the house machine-readable shape.
#[must_use]
pub fn json_line(sizes: &Sizes, outcome: &Outcome, support: Support) -> String {
    let answers: Vec<String> = sizes
        .answers()
        .iter()
        .map(|(query, answered)| format!("\"{query}\":{answered}"))
        .collect();
    let multiplexer = sizes
        .multiplexer
        .map_or_else(|| "null".to_owned(), |m| format!("\"{}\"", m.name()));
    format!(
        "{{\"feature\":\"dimensions\",\"verdict\":\"{}\",\"cells\":{},\"window\":{},\"cell\":{},\
         \"answers\":{{{}}},\"unmet\":{},\"undetermined\":{},\"answered_barrier\":{},\
         \"multiplexer\":{multiplexer},\"interactive\":{}}}\n",
        support.as_str(),
        source_json(sizes.cells()),
        source_json(sizes.window()),
        source_json(sizes.cell()),
        answers.join(","),
        names_json(&outcome.unmet),
        names_json(&outcome.undetermined),
        sizes.answered_barrier,
        sizes.interactive,
    )
}

/// A reading as `[horizontal, vertical, "source"]`, or `null`.
fn source_json(reading: Option<Reading>) -> String {
    reading.map_or_else(
        || "null".to_owned(),
        |reading| {
            format!(
                "[{},{},\"{}\"]",
                reading.horizontal,
                reading.vertical,
                reading.origin.as_str()
            )
        },
    )
}

/// A list of dimension names, all of which are fixed identifiers.
fn names_json(names: &[&'static str]) -> String {
    let quoted: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
    format!("[{}]", quoted.join(","))
}

/// The human-readable findings, for `--format debug`.
fn debug_report(
    theme: &Theme,
    sizes: &Sizes,
    thresholds: Thresholds,
    outcome: &Outcome,
    support: Support,
) {
    println!();
    println!("  {}", theme.paint("Terminal size queries", Ink::Title));
    println!();

    if let Some(multiplexer) = sizes.multiplexer {
        println!(
            "  {} {}",
            theme.paint("multiplexer:", Ink::Muted),
            theme.paint(multiplexer.name(), Ink::Warn)
        );
        println!();
    }

    verdict_line(theme, support, thresholds, outcome, sizes);
    println!();
    sources(theme, sizes);

    if !thresholds.is_empty() {
        println!();
        println!("  {}", theme.paint("what was asked for", Ink::Accent));
        for (name, bounds) in thresholds.axes() {
            if bounds.is_empty() {
                continue;
            }
            let mut asked = Vec::new();
            if let Some(least) = bounds.at_least {
                asked.push(format!("at least {least}"));
            }
            if let Some(most) = bounds.at_most {
                asked.push(format!("at most {most}"));
            }
            if let Some(exact) = bounds.exactly {
                asked.push(format!("exactly {exact}"));
            }
            let mark = if outcome.unmet.contains(&name) {
                theme.paint("✘", Ink::Bad)
            } else if outcome.undetermined.contains(&name) {
                theme.paint("?", Ink::Warn)
            } else {
                theme.paint("✔", Ink::Good)
            };
            println!("    {mark} {name:<8}{}", theme.paint(&asked.join(", "), Ink::Muted));
        }
    }

    println!();
    println!("  {}", theme.paint("what was sent", Ink::Accent));
    if sizes.queried {
        for query in [cells_query(), window_query(), cell_query()] {
            println!("    {}", theme.paint(&pretty(&query), Ink::Code));
        }
        println!(
            "    {}  {}",
            theme.paint(&pretty(&barrier()), Ink::Code),
            theme.paint("← the barrier", Ink::Muted)
        );
    } else {
        let reason = if sizes.interactive {
            "(nothing — TIOCGWINSZ already answered everything that was asked for)"
        } else {
            "(nothing — stdin and stdout are not both terminals, so there was nobody to ask)"
        };
        println!("    {}", theme.paint(reason, Ink::Muted));
    }

    println!();
    println!(
        "  {} {}",
        theme.paint("escapade dimensions explain", Ink::Code),
        theme.paint("for what these queries are and who answers them", Ink::Muted)
    );
    println!();
}

/// The verdict, with the sentence that justifies it.
fn verdict_line(
    theme: &Theme,
    support: Support,
    thresholds: Thresholds,
    outcome: &Outcome,
    sizes: &Sizes,
) {
    let (symbol, ink, label) = match support {
        Support::Yes => ("✔", Ink::Good, "yes"),
        Support::No => ("✘", Ink::Bad, "no"),
        Support::Unknown => ("?", Ink::Warn, "unknown"),
    };
    println!(
        "  {} {}",
        theme.paint(symbol, ink),
        theme.paint(label, ink)
    );

    let explanation = if thresholds.is_empty() {
        bare_explanation(sizes, support)
    } else {
        threshold_explanation(outcome)
    };
    for line in wrap(&explanation, theme.width().saturating_sub(4)) {
        println!("    {}", theme.paint(&line, Ink::Muted));
    }
}

/// What the bare form's verdict means.
fn bare_explanation(sizes: &Sizes, support: Support) -> String {
    match support {
        Support::Yes => {
            let cells = sizes
                .cells()
                .map_or("an unnamed source", |cells| cells.origin.describe());
            let pixels = match (sizes.window(), sizes.cell()) {
                (Some(_), Some(_)) => "and the pixel sizes came back too",
                (Some(_), None) => "the window's pixel size is known but a cell's is not",
                (None, Some(_)) => "a cell's pixel size is known but the window's is not",
                (None, None) => "no pixel size could be had at all",
            };
            format!("The size in cells is known: {cells}. In pixels, {pixels}.")
        }
        Support::No => "The terminal answered the device-attributes barrier but reported no size, \
                        and TIOCGWINSZ could not answer either — so this is a real no rather than \
                        a timeout."
            .to_owned(),
        Support::Unknown => "Nothing could be determined. TIOCGWINSZ did not answer, and neither \
                             did the terminal — not even the device-attributes barrier every \
                             terminal answers, so something between here and it is swallowing \
                             replies."
            .to_owned(),
    }
}

/// What a threshold verdict means, naming the dimensions that decided it.
fn threshold_explanation(outcome: &Outcome) -> String {
    if !outcome.unmet.is_empty() {
        return format!(
            "{} did not satisfy the thresholds given.",
            outcome.unmet.join(", ")
        );
    }
    if !outcome.undetermined.is_empty() {
        return format!(
            "{} could not be determined, so no threshold on it can honestly pass or fail. This is \
             exit 2 rather than exit 1: pixel sizes are unavailable on many terminals, and \
             answering \"too small\" when the truth is \"nobody would say\" would be a guess.",
            outcome.undetermined.join(", ")
        );
    }
    "Every threshold holds.".to_owned()
}

/// One line per quantity: what it is, and which rung of the chain answered.
fn sources(theme: &Theme, sizes: &Sizes) {
    println!("  {}", theme.paint("where the numbers came from", Ink::Accent));
    let rows = [
        ("cells", sizes.cells()),
        ("window", sizes.window()),
        ("cell", sizes.cell()),
    ];
    for (label, reading) in rows {
        match reading {
            Some(reading) => println!(
                "    {} {:<8}{:<12} {}",
                theme.paint("✔", Ink::Good),
                theme.paint(label, Ink::Strong),
                theme.paint(
                    &format!("{} × {}", reading.horizontal, reading.vertical),
                    Ink::Strong
                ),
                theme.paint(reading.origin.describe(), Ink::Muted)
            ),
            None => println!(
                "    {} {:<8}{}",
                theme.paint("?", Ink::Warn),
                theme.paint(label, Ink::Strong),
                theme.paint("unavailable", Ink::Muted)
            ),
        }
    }

    if !sizes.interactive {
        println!();
        for line in wrap(
            "stdin and stdout are not both terminals, so nothing could be asked — only \
             TIOCGWINSZ was available, and it does not report a cell size at all.",
            theme.width().saturating_sub(6),
        ) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
        return;
    }

    println!();
    println!("  {}", theme.paint("which queries answered", Ink::Accent));
    if !sizes.queried {
        println!(
            "    {}",
            theme.paint("(none sent — TIOCGWINSZ already had the answer)", Ink::Muted)
        );
        return;
    }
    for (query, answered) in sizes.answers() {
        let (symbol, ink, label) = if answered {
            ("✔", Ink::Good, "answered")
        } else if sizes.answered_barrier {
            ("✘", Ink::Bad, "not implemented")
        } else {
            ("?", Ink::Warn, "no answer, and no barrier either")
        };
        println!(
            "    {} {:<12}{}",
            theme.paint(symbol, ink),
            theme.paint(query, Ink::Code),
            theme.paint(label, ink)
        );
    }
    if !sizes.answered_barrier {
        for line in wrap(
            "The device-attributes barrier did not come back, so the silence above proves nothing: \
             a slow link looks exactly like a terminal that does not implement these reports.",
            theme.width().saturating_sub(6),
        ) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
    } else if let Some(multiplexer) = sizes.multiplexer {
        for line in wrap(
            &format!(
                "The barrier came back, but {} answers it from its own emulator, so a missing size \
                 report says nothing about the terminal underneath.",
                multiplexer.name()
            ),
            theme.width().saturating_sub(6),
        ) {
            println!("    {}", theme.paint(&line, Ink::Muted));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{
        features::dimensions::threshold::Bounds,
        term::Multiplexer,
    };

    fn known() -> Sizes {
        Sizes {
            ioctl_cells: Some((80, 24)),
            interactive: true,
            queried: true,
            answered_barrier: true,
            ..Sizes::default()
        }
    }

    #[test]
    fn a_known_size_is_a_yes() {
        assert_eq!(size_verdict(&known()), Support::Yes);
    }

    #[test]
    fn no_size_and_an_answered_barrier_is_a_definite_no() {
        let sizes = Sizes {
            ioctl_cells: None,
            ..known()
        };
        assert_eq!(size_verdict(&sizes), Support::No);
    }

    #[test]
    fn total_silence_is_unknown_rather_than_no() {
        let sizes = Sizes {
            ioctl_cells: None,
            answered_barrier: false,
            ..known()
        };
        assert_eq!(size_verdict(&sizes), Support::Unknown);
    }

    #[test]
    fn a_barrier_answered_inside_a_multiplexer_licenses_nothing() {
        // tmux answers Primary Device Attributes itself, so it cannot vouch for
        // the terminal beyond it.
        let sizes = Sizes {
            ioctl_cells: None,
            multiplexer: Some(Multiplexer::Tmux),
            ..known()
        };
        assert_eq!(size_verdict(&sizes), Support::Unknown);
    }

    #[test]
    fn the_probe_json_shape_carries_the_verdict_and_the_sources() {
        let sizes = known();
        let thresholds = Thresholds {
            width: Bounds {
                at_least: Some(100),
                ..Bounds::default()
            },
            ..Thresholds::default()
        };
        let outcome = thresholds.evaluate(&Available::from_sizes(&sizes));
        let line = json_line(&sizes, &outcome, outcome.support());
        assert!(line.ends_with('\n'), "one object, one line");
        assert!(line.contains("\"feature\":\"dimensions\""));
        assert!(line.contains("\"verdict\":\"unknown\""), "{line}");
        assert!(line.contains("\"cells\":[80,24,\"tiocgwinsz\"]"), "{line}");
        assert!(line.contains("\"window\":null"), "{line}");
        assert!(line.contains("\"undetermined\":[\"width\"]"), "{line}");
        assert!(line.contains("\"unmet\":[]"), "{line}");
        assert!(line.contains("\"csi-16-t\":false"), "{line}");
        assert!(line.contains("\"answered_barrier\":true"));
        assert!(!line.contains('\u{1b}'));
    }

    #[test]
    fn a_bare_probe_json_has_empty_threshold_lists() {
        let sizes = known();
        let outcome = Thresholds::default().evaluate(&Available::from_sizes(&sizes));
        let line = json_line(&sizes, &outcome, size_verdict(&sizes));
        assert!(line.contains("\"verdict\":\"yes\""));
        assert!(line.contains("\"unmet\":[]"));
        assert!(line.contains("\"undetermined\":[]"));
    }

    #[test]
    fn an_unmet_threshold_is_named_in_the_explanation() {
        let thresholds = Thresholds {
            cols: Bounds {
                at_least: Some(200),
                ..Bounds::default()
            },
            ..Thresholds::default()
        };
        let outcome = thresholds.evaluate(&Available::from_sizes(&known()));
        assert_eq!(outcome.support(), Support::No);
        assert!(threshold_explanation(&outcome).contains("cols"));
    }

    #[test]
    fn an_undetermined_threshold_explains_why_it_is_not_a_failure() {
        let thresholds = Thresholds {
            height: Bounds {
                at_most: Some(2000),
                ..Bounds::default()
            },
            ..Thresholds::default()
        };
        let outcome = thresholds.evaluate(&Available::from_sizes(&known()));
        let explanation = threshold_explanation(&outcome);
        assert!(explanation.contains("height"), "{explanation}");
        assert!(explanation.contains("exit 2"), "{explanation}");
    }

    #[test]
    fn every_threshold_holding_says_so_plainly() {
        let thresholds = Thresholds {
            rows: Bounds {
                at_least: Some(24),
                ..Bounds::default()
            },
            ..Thresholds::default()
        };
        let outcome = thresholds.evaluate(&Available::from_sizes(&known()));
        assert_eq!(outcome.support(), Support::Yes);
        assert_eq!(threshold_explanation(&outcome), "Every threshold holds.");
    }

    #[test]
    fn the_bare_explanation_says_which_pixel_sizes_are_missing() {
        let explanation = bare_explanation(&known(), Support::Yes);
        assert!(explanation.contains("TIOCGWINSZ"), "{explanation}");
        assert!(explanation.contains("no pixel size"), "{explanation}");

        let complete = Sizes {
            ioctl_pixels: Some((800, 480)),
            ..known()
        };
        assert!(bare_explanation(&complete, Support::Yes).contains("came back too"));

        // And the two half-known cases are distinguished rather than lumped in.
        let cell_only = Sizes {
            reported_cell: Some((10, 20)),
            ..known()
        };
        assert!(
            bare_explanation(&cell_only, Support::Yes).contains("a cell's pixel size is known")
                || bare_explanation(&cell_only, Support::Yes).contains("came back too"),
            "{}",
            bare_explanation(&cell_only, Support::Yes)
        );
    }

    #[test]
    fn the_debug_report_runs_for_every_shape_of_finding() {
        // Exercises the whole layout path without a terminal, which is what
        // catches an index or a padding mistake in it.
        let theme = Theme::plain();
        let thresholds = Thresholds {
            rows: Bounds {
                at_least: Some(24),
                at_most: Some(40),
                exactly: None,
            },
            width: Bounds {
                exactly: Some(800),
                ..Bounds::default()
            },
            ..Thresholds::default()
        };
        for sizes in [
            Sizes::default(),
            known(),
            Sizes {
                multiplexer: Some(Multiplexer::Tmux),
                answered_barrier: false,
                ..known()
            },
            Sizes {
                ioctl_pixels: Some((800, 480)),
                queried: false,
                ..known()
            },
        ] {
            for thresholds in [Thresholds::default(), thresholds] {
                let outcome = thresholds.evaluate(&Available::from_sizes(&sizes));
                let support = if thresholds.is_empty() {
                    size_verdict(&sizes)
                } else {
                    outcome.support()
                };
                debug_report(&theme, &sizes, thresholds, &outcome, support);
            }
        }
    }
}
