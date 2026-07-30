//! `escapade grapheme render <CODEPOINTS>` — print a sequence and see it.
//!
//! Three modes, and the difference between the first two is the whole point:
//!
//! * default — query the mode, enable it with `CSI ? 2027 h`, render, then put
//!   the mode back to **whatever the query reported**. Sending a blind
//!   `CSI ? 2027 l` afterwards would switch off a mode the user's shell or editor
//!   had deliberately turned on.
//! * `--disabled` — render the same bytes without enabling anything, which is
//!   what every unmodified terminal does today. Run both and the corruption is
//!   visible rather than described.
//! * `--debug` — one line per codepoint, then the measured comparison.
//!
//! # The kitty text sizing protocol
//!
//! `--debug` was meant to ask the terminal, via the kitty text sizing protocol,
//! what size it assigns the grapheme. **That protocol has no width query.** Its
//! only sequence is `OSC 66 ; <metadata> ; <text> ST`, and the metadata keys
//! (`w` width in cells, `s` scale, `n`/`d` fractional scale, `v`/`h` alignment)
//! all *tell* the terminal a size — the protocol deliberately moves the decision
//! to the application so the two cannot disagree. There is no request and no
//! reply, and its own documented way of detecting support is a Cursor Position
//! Report taken around an `OSC 66 ; w=2 ; <space> ST`.
//!
//! So the fallback is the `CSI 6 n` measurement in [`super::measure`], which is
//! the better answer anyway: it reports what the terminal *did*, not what it
//! advertises. No sequence is invented here.

use std::io::Write as _;

use anyhow::{Context as _, Result};

use crate::{
    features::{
        grapheme::{
            ModeState, Reliability, cluster,
            measure::{self, Comparison, ModeReport, Ruler},
            probe::comparison_lines,
            restore_sequence, set,
        },
        wrap,
    },
    term::pretty,
    theme::{Ink, Theme},
    unicode::{self, CharInfo},
};

/// What was learned about the terminal before anything was rendered.
#[derive(Debug, Clone, Default)]
struct Findings {
    /// What DECRQM said, when it was asked.
    mode: ModeReport,
    /// The whole sequence measured off and on, for `--debug`.
    comparison: Option<Comparison>,
    /// Whether there was a terminal to ask at all.
    interactive: bool,
}

/// Renders `escapade grapheme render`.
///
/// # Errors
///
/// Fails if the codepoint specification cannot be parsed, if the terminal cannot
/// be set up, or if stdout cannot be written to.
pub fn report(theme: &Theme, spec: &str, disabled: bool, debug: bool) -> Result<()> {
    let codepoints = unicode::parse_sequence(spec)
        .with_context(|| format!("parsing codepoint sequence `{spec}`"))?;
    let assembly = cluster::assemble(&codepoints);

    println!();
    let Some(text) = assembly.text.as_deref() else {
        refusal(theme, &assembly);
        return Ok(());
    };

    // Everything that needs raw mode happens first and in one session, so the
    // report below prints into a normal cooked-mode terminal.
    let findings = investigate(text, debug)?;

    if debug {
        breakdown(theme, text);
        println!();
        measurement(theme, &findings);
        println!();
    }

    draw(theme, text, disabled, &findings)?;
    Ok(())
}

/// Queries the mode, and measures the sequence when `--debug` asked for it.
fn investigate(text: &str, debug: bool) -> Result<Findings> {
    let Some(mut ruler) = Ruler::open()? else {
        return Ok(Findings::default());
    };
    let mut findings = Findings {
        mode: ruler.entry_report(),
        comparison: None,
        interactive: true,
    };
    if debug {
        findings.comparison = Some(ruler.compare(text)?);
    }
    ruler.close();
    Ok(findings)
}

/// The "this cannot be sent" path.
fn refusal(theme: &Theme, assembly: &cluster::Assembly) {
    println!(
        "  {} {}",
        theme.paint("✘", Ink::Bad),
        theme.paint("refusing to render this sequence", Ink::Bad)
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
        "A control character, a surrogate or an unassigned codepoint is never written to the \
         terminal: a raw ESC would execute whatever bytes came after it. Joiners such as U+200D and \
         variation selectors such as U+FE0F are fine — they are what grapheme clusters are made of.",
        theme.width().saturating_sub(4),
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();
}

/// `--debug`: one line per codepoint, then one per cluster.
fn breakdown(theme: &Theme, text: &str) {
    println!("  {}", theme.paint("codepoints", Ink::Accent));
    for character in text.chars() {
        let info = unicode::describe(character.into());
        println!(
            "    {}",
            theme.inline(&cluster::breakdown_line(&info).replace('`', ""))
        );
    }

    let clusters = cluster::analyze(text);
    println!();
    println!(
        "  {}",
        theme.paint(
            &format!(
                "{} {} after UAX #29 segmentation",
                clusters.len(),
                if clusters.len() == 1 {
                    "grapheme cluster"
                } else {
                    "grapheme clusters"
                }
            ),
            Ink::Accent
        )
    );
    for cluster in &clusters {
        let labels: Vec<String> = cluster.codepoints.iter().map(CharInfo::label).collect();
        println!(
            "    {}  {}  {}",
            theme.paint(&labels.join(" "), Ink::Code),
            theme.paint(
                &format!("{} expected", measure::cells(cluster.expected)),
                Ink::Accent
            ),
            theme.paint(
                &match cluster.miscount_direction() {
                    Some(direction) => format!(
                        "{} per codepoint — {direction}",
                        measure::cells(cluster.legacy)
                    ),
                    None => format!(
                        "{} per codepoint — both rules agree",
                        measure::cells(cluster.legacy)
                    ),
                },
                Ink::Muted
            )
        );
    }
}

/// `--debug`: the measured comparison, and why it is a `CSI 6 n` measurement and
/// not a text-sizing-protocol query.
fn measurement(theme: &Theme, findings: &Findings) {
    println!("  {}", theme.paint("what the terminal did", Ink::Accent));

    if let Some(comparison) = &findings.comparison {
        for line in comparison_lines(theme, comparison) {
            println!("{line}");
        }
    } else {
        println!("    {}", theme.paint("nothing measured", Ink::Muted));
        for line in wrap(
            "Measuring means printing the sequence and reading back where the cursor went, which \
             needs stdin and stdout to both be terminals.",
            theme.width().saturating_sub(6),
        ) {
            println!("      {}", theme.paint(&line, Ink::Muted));
        }
    }

    println!();
    println!(
        "    {} {}",
        theme.paint("mode 2027:", Ink::Muted),
        theme.paint(&mode_label(findings), Ink::Muted)
    );
    println!();
    for line in wrap(
        "This is a Cursor Position Report measurement, not a size query. The kitty text sizing \
         protocol has no width query: OSC 66 only ever tells the terminal a width (w=) and a scale \
         (s=), and its own support detection is this same CSI 6 n trick. Measuring is the better \
         answer regardless — it is what the terminal did, not what it claims.",
        theme.width().saturating_sub(6),
    ) {
        println!("      {}", theme.paint(&line, Ink::Muted));
    }
}

/// Actually renders the sequence, switching the mode around it when asked.
fn draw(theme: &Theme, text: &str, disabled: bool, findings: &Findings) -> Result<()> {
    let enabling = !disabled && findings.interactive;

    println!(
        "  {}",
        theme.paint(
            if disabled {
                "rendered with clustering left alone  (--disabled)"
            } else {
                "rendered with clustering enabled"
            },
            Ink::Accent
        )
    );
    if enabling {
        println!(
            "    {}  {}",
            theme.paint(&pretty(&set(true)), Ink::Code),
            theme.paint("enable", Ink::Muted)
        );
    }
    println!();

    let mut out = std::io::stdout();
    if enabling {
        write!(out, "{}", set(true))?;
    }
    // Between two markers, so the cells the sequence claims are visible even when
    // the glyph itself is missing from the font.
    write!(out, "    [{text}]")?;
    if enabling {
        // Put the mode back to exactly what the query reported — never a blind
        // reset, which would switch off a mode the user's shell had turned on.
        if let Some(sequence) = restore_sequence(findings.mode.trusted_state()) {
            write!(out, "{sequence}")?;
        }
    }
    writeln!(out)?;
    out.flush()?;

    println!();
    if enabling {
        if let Some(sequence) = restore_sequence(findings.mode.trusted_state()) {
            println!(
                "    {}  {}",
                theme.paint(&pretty(&sequence), Ink::Code),
                theme.paint(&restore_note(findings.mode.trusted_state()), Ink::Muted)
            );
        } else {
            println!(
                "    {}",
                theme.paint(&restore_note(findings.mode.trusted_state()), Ink::Muted)
            );
        }
    }

    for line in wrap(
        &closing_note(disabled, findings),
        theme.width().saturating_sub(4),
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();
    Ok(())
}

/// Why the restore did, or did not, send anything.
#[must_use]
fn restore_note(previous: Option<ModeState>) -> String {
    match previous {
        Some(state @ (ModeState::Set | ModeState::PermanentlySet)) => format!(
            "left as found — the mode was already {} before this ran",
            state.label()
        ),
        Some(ModeState::Reset) => "restored: the mode was off before this ran".to_owned(),
        Some(state @ (ModeState::NotRecognized | ModeState::PermanentlyReset)) => {
            format!("nothing to restore — the mode is {}", state.label())
        }
        Some(ModeState::Undefined(value)) => {
            format!("DECRPM answered {value}, which it does not define — our enable was undone")
        }
        None => "the mode state was never reported, so our own enable was undone".to_owned(),
    }
}

/// The mode state as a short phrase, absences and untrustworthy answers included.
fn mode_label(findings: &Findings) -> String {
    if let Some(state) = findings.mode.trusted_state() {
        return format!("{} (DECRPM value {})", state.label(), state.value());
    }
    match findings.mode.reliability() {
        Reliability::Indiscriminate(sentinel) => format!(
            "answer discarded — this terminal also reports `{}` for a mode that cannot exist",
            sentinel.label()
        ),
        Reliability::NoDecrqm => "this terminal implements no DECRQM".to_owned(),
        Reliability::Discriminating => "no DECRQM answer for mode 2027".to_owned(),
        Reliability::Silent if findings.interactive => "nothing answered".to_owned(),
        Reliability::Silent => "not asked — stdout is not a terminal".to_owned(),
    }
}

/// The pointer at the end, which depends on what just happened.
fn closing_note(disabled: bool, findings: &Findings) -> String {
    if !findings.interactive {
        return "stdout is not a terminal, so the mode was not touched and the sequence was written \
                as plain text. Run this in a terminal to see the difference."
            .to_owned();
    }
    if disabled {
        // `--disabled` means "do not enable it", not "force it off". If something
        // further out already had the mode on, this render was clustered too, and
        // claiming otherwise would be the exact mistake this tool is about.
        if findings
            .mode
            .trusted_state()
            .is_some_and(ModeState::enabled)
        {
            return "Note that clustering was already on before this ran, so --disabled did not \
                    turn it off — it only declined to enable it. Nothing here shows the \
                    unclustered behaviour; `escapade grapheme probe <CODEPOINTS> --format debug` measures \
                    both, because it switches the mode itself."
                .to_owned();
        }
        return "That is what an unmodified terminal does. Run the same command without --disabled \
                to see it with mode 2027 on, and `escapade grapheme probe <CODEPOINTS> --format debug` \
                for the measured cell counts."
            .to_owned();
    }
    if findings
        .mode
        .trusted_state()
        .is_some_and(ModeState::recognized)
    {
        return "Run the same command with --disabled for the comparison: the same bytes, with the \
                terminal advancing per codepoint."
            .to_owned();
    }
    // Nothing usable came back — and *why* nothing came back is worth naming,
    // because "it never answered" and "it answered nonsense" send a reader to
    // different places.
    match findings.mode.reliability() {
        Reliability::Indiscriminate(_) => {
            "The terminal's DECRQM answer was discarded: it reports the same state for modes that \
             do not exist, so there is no telling whether the enable above did anything. \
             `escapade grapheme probe --format debug` measures it instead of asking."
                .to_owned()
        }
        Reliability::NoDecrqm => {
            "This terminal implements no DECRQM, so there was no way to learn what the enable above \
             did — or to put the mode back to anything but off. \
             `escapade grapheme probe --format debug` measures what actually happened."
                .to_owned()
        }
        Reliability::Discriminating | Reliability::Silent => {
            "The terminal never reported mode 2027, so the enable above may have done nothing. \
             `escapade grapheme probe --format debug` measures what actually happened."
                .to_owned()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const FARMER: &str = "\u{1f9d1}\u{200d}\u{1f33e}";

    #[test]
    fn restoring_explains_itself_for_every_state() {
        assert!(restore_note(Some(ModeState::Set)).contains("left as found"));
        assert!(restore_note(Some(ModeState::PermanentlySet)).contains("left as found"));
        assert!(restore_note(Some(ModeState::Reset)).contains("restored"));
        assert!(restore_note(Some(ModeState::NotRecognized)).contains("nothing to restore"));
        assert!(restore_note(Some(ModeState::PermanentlyReset)).contains("nothing to restore"));
        assert!(restore_note(Some(ModeState::Undefined(9))).contains("undone"));
        assert!(restore_note(None).contains("undone"));
    }

    #[test]
    fn a_mode_that_was_on_is_never_switched_off() {
        // The bug this guards: rendering with `--` then leaving the user's editor
        // without the clustering it had asked for.
        for state in [ModeState::Set, ModeState::PermanentlySet] {
            assert_eq!(restore_sequence(Some(state)), None, "{state:?}");
        }
        assert_eq!(
            restore_sequence(Some(ModeState::Reset)),
            Some(super::super::disable())
        );
    }

    #[test]
    fn mode_label_distinguishes_every_kind_of_absence() {
        let silent = Findings {
            mode: ModeReport::default(),
            comparison: None,
            interactive: true,
        };
        assert_eq!(mode_label(&silent), "nothing answered");

        let piped = Findings {
            interactive: false,
            ..Findings::default()
        };
        assert!(mode_label(&piped).contains("not a terminal"));

        // Answered the control but not mode 2027: a real no about the query.
        let mode_only_missing = Findings {
            mode: ModeReport {
                state: None,
                sentinel: Some(ModeState::NotRecognized),
                answered_barrier: true,
            },
            comparison: None,
            interactive: true,
        };
        assert_eq!(
            mode_label(&mode_only_missing),
            "no DECRQM answer for mode 2027"
        );

        // Answered neither, but the barrier came back: no DECRQM at all.
        let no_decrqm = Findings {
            mode: ModeReport {
                state: None,
                sentinel: None,
                answered_barrier: true,
            },
            comparison: None,
            interactive: true,
        };
        assert!(mode_label(&no_decrqm).contains("no DECRQM"));

        // Answered confidently about a mode that cannot exist: discard it.
        let indiscriminate = Findings {
            mode: ModeReport {
                state: Some(ModeState::PermanentlyReset),
                sentinel: Some(ModeState::PermanentlyReset),
                answered_barrier: true,
            },
            comparison: None,
            interactive: true,
        };
        let label = mode_label(&indiscriminate);
        assert!(label.contains("discarded"), "{label}");
        assert!(
            label.contains("cannot exist"),
            "the reason must travel with the verdict: {label}"
        );

        let answered = Findings {
            mode: ModeReport {
                state: Some(ModeState::PermanentlySet),
                sentinel: Some(ModeState::NotRecognized),
                answered_barrier: false,
            },
            comparison: None,
            interactive: true,
        };
        assert!(mode_label(&answered).contains("permanently set"));
        assert!(mode_label(&answered).contains('3'));
    }

    #[test]
    fn the_closing_note_says_what_to_run_next() {
        let interactive = Findings {
            mode: ModeReport {
                state: Some(ModeState::Reset),
                sentinel: Some(ModeState::NotRecognized),
                answered_barrier: true,
            },
            comparison: None,
            interactive: true,
        };
        assert!(closing_note(true, &interactive).contains("--disabled"));
        assert!(closing_note(false, &interactive).contains("--disabled"));

        // `--disabled` does not force the mode off, so when it was already on the
        // note must not claim to be showing unclustered behaviour.
        for state in [ModeState::Set, ModeState::PermanentlySet] {
            let already_on = Findings {
                mode: ModeReport {
                    state: Some(state),
                    sentinel: Some(ModeState::NotRecognized),
                    answered_barrier: true,
                },
                comparison: None,
                interactive: true,
            };
            let note = closing_note(true, &already_on);
            assert!(note.contains("already on"), "{state:?}: {note}");
            assert!(!note.contains("unmodified terminal"), "{state:?}: {note}");
        }

        let piped = Findings::default();
        assert!(closing_note(false, &piped).contains("not a terminal"));

        let silent = Findings {
            interactive: true,
            ..Findings::default()
        };
        assert!(closing_note(false, &silent).contains("probe"));
        assert!(closing_note(false, &silent).contains("never reported"));

        // A discarded answer and a missing one send a reader to different places.
        let indiscriminate = Findings {
            mode: ModeReport {
                state: Some(ModeState::PermanentlyReset),
                sentinel: Some(ModeState::PermanentlyReset),
                answered_barrier: true,
            },
            comparison: None,
            interactive: true,
        };
        let note = closing_note(false, &indiscriminate);
        assert!(note.contains("discarded"), "{note}");
        assert!(note.contains("do not exist"), "{note}");

        let no_decrqm = Findings {
            mode: ModeReport {
                state: None,
                sentinel: None,
                answered_barrier: true,
            },
            comparison: None,
            interactive: true,
        };
        let note = closing_note(false, &no_decrqm);
        assert!(note.contains("no DECRQM"), "{note}");
    }

    #[test]
    fn breakdown_lines_look_like_the_brief_asks() {
        let info = unicode::describe(0x1_F9D1);
        let line = cluster::breakdown_line(&info);
        assert!(line.starts_with("U+1F9D1  Adult  \u{1f9d1}"), "{line}");
    }

    #[test]
    fn printing_never_panics_and_never_emits_a_stray_escape() {
        let theme = Theme::plain();
        breakdown(&theme, FARMER);
        breakdown(&theme, "A\u{2764}\u{fe0f}\u{4e00}");
        breakdown(&theme, "");

        for comparison in [
            None,
            Some(Comparison::unmeasured(FARMER)),
            Some(Comparison {
                predicted: 2,
                legacy: 4,
                clustering_off: Some(4),
                clustering_on: Some(2),
            }),
        ] {
            for interactive in [false, true] {
                measurement(
                    &theme,
                    &Findings {
                        mode: ModeReport::default(),
                        comparison,
                        interactive,
                    },
                );
            }
        }

        refusal(&theme, &cluster::assemble(&[0x1B, 0xD800]));
    }

    #[test]
    fn a_refused_sequence_never_becomes_text() {
        for codepoint in [0x1Bu32, 0x07, 0x9B, 0xD800] {
            let assembly = cluster::assemble(&[0x41, codepoint]);
            assert!(
                assembly.text.is_none(),
                "U+{codepoint:04X} must never be rendered"
            );
        }
    }
}
