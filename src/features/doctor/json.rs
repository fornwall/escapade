//! JSON Lines output: one self-contained JSON object per line.
//!
//! The convention is ripgrep's — a stream of records, each with a `kind`, so the
//! output can be consumed as it is produced rather than buffered until the end.
//!
//! # Why every record carries a `checks` array
//!
//! `escapade doctor --help` promises that
//!
//! ```sh
//! escapade doctor --format json | jq '.checks[] | select(.agrees == false)'
//! ```
//!
//! works. A stream of bare check objects would break it — `.checks[]` on a record
//! with no `checks` key is an error, not an empty result — so every record has
//! one: `[]` on the `run` and `summary` records, and the single check on a `check`
//! record. That keeps the documented command correct while the output stays valid
//! JSON Lines, and it is the same shape ripgrep uses when it wraps each record's
//! payload under a key.
//!
//! `agrees` is `true`, `false` or `null`, and `null` is the interesting one: a
//! check with fewer than two comparable answers is not a disagreement, so
//! `select(.agrees == false)` must not pick it up.
//!
//! Serialisation is by hand. Every value that reaches [`push_string`] is escaped,
//! and every raw value has already been through
//! [`crate::features::doctor::observe::printable`], so no control byte from a
//! terminal reply can reach the output. Adding `serde_json` to emit three record
//! shapes would be more dependency than saving.

use std::fmt::Write as _;

use crate::features::doctor::{
    Diagnosis, Row,
    modes::{self, Trust},
    verdict::{Agreement, Origin, Reading},
};

/// Prints the whole diagnosis as JSON Lines.
///
/// Valid output is produced even when nothing could be probed: the `run` record
/// says why, every check still appears with its sources, and the `summary` record
/// carries the exit status.
pub fn render(diagnosis: &Diagnosis<'_>, only_disagreements: bool) {
    print!("{}", run_record(diagnosis));
    for row in &diagnosis.rows {
        if only_disagreements && row.agreement != Agreement::Disagree {
            continue;
        }
        print!("{}", check_record(row));
    }
    print!("{}", summary_record(diagnosis, only_disagreements));
}

/// The opening record: the environment, the channel, and anything that makes the
/// whole run unreliable.
#[must_use]
pub fn run_record(diagnosis: &Diagnosis<'_>) -> String {
    let mut out = String::from("{\"kind\":\"run\",\"checks\":[]");
    push_optional(&mut out, "term", diagnosis.term.as_deref());
    push_optional(&mut out, "term_program", diagnosis.term_program.as_deref());
    push_optional(&mut out, "terminfo_entry", diagnosis.terminfo.queried());
    out.push_str(",\"terminfo_read\":");
    out.push_str(bool_json(diagnosis.terminfo.entry().is_some()));
    push_field(&mut out, "terminfo_status", &diagnosis.terminfo.explanation());
    push_optional(
        &mut out,
        "probe_channel",
        diagnosis.channel.map(super::session::Channel::label),
    );
    push_optional(
        &mut out,
        "probe_failure",
        diagnosis.no_channel.as_ref().map(|failure| failure.reason.as_str()),
    );
    push_optional(
        &mut out,
        "multiplexer",
        diagnosis.multiplexer.map(crate::term::Multiplexer::name),
    );
    let _ = write!(
        out,
        ",\"timeout_ms\":{}",
        diagnosis.timeout.as_millis()
    );
    out.push_str(",\"probed\":");
    out.push_str(bool_json(diagnosis.probed()));

    // The sentinel: the control that decides whether any DECRQM answer counts.
    out.push_str(",\"decrqm_sentinel\":{\"mode\":");
    let _ = write!(out, "{}", modes::SENTINEL_MODE);
    match diagnosis.sentinel.trust() {
        Trust::Trusted => out.push_str(",\"answered\":false,\"trust\":\"trusted\""),
        Trust::Unknown => out.push_str(",\"answered\":false,\"trust\":\"unknown\""),
        Trust::Compromised(state) => {
            let _ = write!(
                out,
                ",\"answered\":true,\"value\":{},\"trust\":\"compromised\"",
                state.value()
            );
        }
    }
    push_optional(&mut out, "warning", diagnosis.sentinel.warning().as_deref());
    out.push('}');

    out.push_str(",\"decrqm_not_implemented\":");
    out.push_str(bool_json(diagnosis.decrqm_not_implemented));
    out.push_str("}\n");
    out
}

/// One check's record: every source's raw and interpreted value, whether they
/// agree, and how far each answer can be trusted.
#[must_use]
pub fn check_record(row: &Row<'_>) -> String {
    let mut check = String::from("{\"id\":");
    push_string(&mut check, &row.check.id);
    push_field(&mut check, "name", &row.check.name);
    push_field(&mut check, "question", &row.check.question);
    push_field(&mut check, "kind", kind_key(row.check.kind));
    let _ = write!(check, ",\"agrees\":{}", row.agreement.json());
    push_field(&mut check, "agreement", agreement_key(row.agreement));
    if let Some(mode) = row.check.mode {
        let _ = write!(check, ",\"mode\":{mode}");
    }

    check.push_str(",\"sources\":{");
    let mut first = true;
    for origin in Origin::ALL {
        if let Some(reading) = row.reading(origin) {
            if !first {
                check.push(',');
            }
            first = false;
            push_string(&mut check, origin.key());
            check.push(':');
            check.push_str(&source_object(reading));
        }
    }
    check.push('}');

    // The two sides of a disagreement, spelled out so a script does not have to
    // reconstruct them from `sources`.
    if row.agreement == Agreement::Disagree {
        push_field(
            &mut check,
            "disagreement",
            &super::verdict::describe_split(&row.readings),
        );
    }
    push_field(&mut check, "notes", &row.check.notes);
    check.push('}');

    format!("{{\"kind\":\"check\",\"checks\":[{check}]}}\n")
}

/// The closing record: the counts and the exit status.
#[must_use]
pub fn summary_record(diagnosis: &Diagnosis<'_>, only_disagreements: bool) -> String {
    let mut out = String::from("{\"kind\":\"summary\",\"checks\":[]");
    let _ = write!(
        out,
        ",\"total\":{},\"disagreements\":{},\"not_comparable\":{},\"exit_code\":{}",
        diagnosis.rows.len(),
        diagnosis.disagreements(),
        diagnosis.incomparable(),
        exit_code_number(diagnosis)
    );
    out.push_str(",\"filtered\":");
    out.push_str(bool_json(only_disagreements));
    out.push_str("}\n");
    out
}

/// One source's answer.
fn source_object(reading: &Reading) -> String {
    let mut out = String::from("{\"claim\":");
    match reading.claim.says() {
        Some(answer) => push_string(&mut out, answer),
        None => out.push_str("null"),
    }
    push_field(&mut out, "state", claim_key(&reading.claim));
    push_optional(&mut out, "raw", reading.raw.as_deref());
    push_optional(&mut out, "detail", reading.detail.as_deref());
    out.push_str(",\"reliable\":");
    out.push_str(bool_json(reading.reliable));
    out.push_str(",\"compared\":");
    out.push_str(bool_json(reading.comparable().is_some()));
    out.push('}');
    out
}

/// The three-state a source can be in, as a stable key.
fn claim_key(claim: &super::verdict::Claim) -> &'static str {
    match claim {
        super::verdict::Claim::Says(_) => "answered",
        // Kept distinct from `silent` on purpose: one is a permanent gap and the
        // other is a source that had its chance.
        super::verdict::Claim::NotApplicable => "not-applicable",
        super::verdict::Claim::Silent => "silent",
    }
}

/// A stable key for an agreement, alongside the nullable `agrees` boolean.
fn agreement_key(agreement: Agreement) -> &'static str {
    match agreement {
        Agreement::Agree => "agree",
        Agreement::Disagree => "disagree",
        Agreement::NotComparable => "not-comparable",
    }
}

/// A stable key for a check's comparison kind.
fn kind_key(kind: super::checks::CheckKind) -> &'static str {
    match kind {
        super::checks::CheckKind::Boolean => "boolean",
        super::checks::CheckKind::Value => "value",
        super::checks::CheckKind::Identity => "identity",
    }
}

/// The exit status as a number, so the JSON says what the process will say.
fn exit_code_number(diagnosis: &Diagnosis<'_>) -> u8 {
    if diagnosis.probed() {
        u8::from(diagnosis.disagreements() > 0)
    } else {
        2
    }
}

/// `true` or `false`, spelled once.
fn bool_json(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// Appends `,"key":"value"`.
fn push_field(out: &mut String, key: &str, value: &str) {
    out.push(',');
    push_string(out, key);
    out.push(':');
    push_string(out, value);
}

/// Appends `,"key":"value"` or `,"key":null`.
fn push_optional(out: &mut String, key: &str, value: Option<&str>) {
    out.push(',');
    push_string(out, key);
    out.push(':');
    match value {
        Some(value) => push_string(out, value),
        None => out.push_str("null"),
    }
}

/// Appends a JSON string literal, escaped.
///
/// Everything outside the printable ASCII range that JSON gives a short escape to
/// becomes `\u00XX`, so a stray control byte cannot terminate the string early or
/// be re-executed by whatever prints the output.
pub fn push_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::features::{
        Support,
        doctor::{
            checks::Catalogue,
            modes::{ModeState, Sentinel},
            session::{Channel, NoChannel},
            terminfo,
            verdict::{Claim, agreement},
        },
    };

    fn catalogue() -> Catalogue {
        Catalogue::load().unwrap()
    }

    fn row(check: &super::super::Check, readings: Vec<Reading>) -> Row<'_> {
        let agreement = agreement(&readings);
        Row { check, readings, agreement }
    }

    fn empty_diagnosis(rows: Vec<Row<'_>>) -> Diagnosis<'_> {
        Diagnosis {
            rows,
            terminfo: terminfo::Source::NoInfocmp,
            channel: None,
            no_channel: Some(NoChannel {
                reason: "nothing to ask".to_owned(),
                hint: None,
            }),
            multiplexer: None,
            sentinel: Sentinel::default(),
            heard_anything: false,
            decrqm_not_implemented: false,
            timeout: Duration::from_secs(1),
            term: Some("xterm-256color".to_owned()),
            term_program: None,
        }
    }

    /// A deliberately minimal JSON well-formedness check: balanced braces,
    /// brackets and quotes, with escapes honoured. Enough to catch a missing comma
    /// or an unescaped control byte without adding a parser dependency.
    fn looks_like_json(line: &str) -> bool {
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        for ch in line.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                } else if ch.is_control() {
                    return false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                ch if ch.is_control() => return false,
                _ => {}
            }
            if depth < 0 {
                return false;
            }
        }
        depth == 0 && !in_string
    }

    #[test]
    fn every_record_is_one_valid_json_object_on_one_line() {
        let catalogue = catalogue();
        let check = &catalogue.checks()[0];
        let rows = vec![row(
            check,
            vec![
                Reading::new(Origin::Terminfo, Claim::from_support(Support::No)),
                Reading::new(Origin::Environment, Claim::from_support(Support::Yes)),
            ],
        )];
        let diagnosis = empty_diagnosis(rows);
        let output = format!(
            "{}{}{}",
            run_record(&diagnosis),
            check_record(&diagnosis.rows[0]),
            summary_record(&diagnosis, false)
        );
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in lines {
            assert!(looks_like_json(line), "{line}");
            assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
        }
    }

    #[test]
    fn every_record_carries_a_checks_array_so_the_documented_jq_works() {
        // `jq '.checks[] | select(.agrees == false)'` errors on a record with no
        // `checks` key, so the run and summary records have an empty one.
        let catalogue = catalogue();
        let diagnosis = empty_diagnosis(Vec::new());
        assert!(run_record(&diagnosis).contains("\"checks\":[]"));
        assert!(summary_record(&diagnosis, false).contains("\"checks\":[]"));

        let check = &catalogue.checks()[0];
        let record = check_record(&row(check, Vec::new()));
        assert!(record.contains("\"checks\":["), "{record}");
        assert!(record.contains("\"kind\":\"check\""), "{record}");
    }

    #[test]
    fn a_disagreement_serialises_agrees_false() {
        let catalogue = catalogue();
        let check = &catalogue.checks()[0];
        let record = check_record(&row(
            check,
            vec![
                Reading::new(Origin::Terminfo, Claim::from_support(Support::No)),
                Reading::new(Origin::Observed, Claim::from_support(Support::Yes)),
            ],
        ));
        assert!(record.contains("\"agrees\":false"), "{record}");
        assert!(record.contains("\"disagreement\":"), "{record}");
        assert!(record.contains("terminfo says no"), "{record}");
    }

    #[test]
    fn nothing_to_compare_serialises_agrees_null_rather_than_false() {
        // Otherwise `select(.agrees == false)` would report every row that simply
        // could not be checked as a finding.
        let catalogue = catalogue();
        let check = &catalogue.checks()[0];
        let record = check_record(&row(
            check,
            vec![Reading::silent(Origin::Terminfo, "no entry")],
        ));
        assert!(record.contains("\"agrees\":null"), "{record}");
        assert!(!record.contains("\"agrees\":false"), "{record}");
        assert!(record.contains("\"agreement\":\"not-comparable\""), "{record}");
    }

    #[test]
    fn agreement_serialises_agrees_true() {
        let catalogue = catalogue();
        let check = &catalogue.checks()[0];
        let record = check_record(&row(
            check,
            vec![
                Reading::new(Origin::Terminfo, Claim::from_support(Support::Yes)),
                Reading::new(Origin::Observed, Claim::from_support(Support::Yes)),
            ],
        ));
        assert!(record.contains("\"agrees\":true"), "{record}");
        assert!(!record.contains("\"disagreement\":"), "{record}");
    }

    #[test]
    fn a_source_that_cannot_answer_is_distinguishable_from_one_that_said_nothing() {
        let catalogue = catalogue();
        let check = &catalogue.checks()[0];
        let record = check_record(&row(
            check,
            vec![
                Reading::not_applicable(Origin::Terminfo, "no capability exists"),
                Reading::silent(Origin::Terminal, "no reply"),
            ],
        ));
        assert!(record.contains("\"state\":\"not-applicable\""), "{record}");
        assert!(record.contains("\"state\":\"silent\""), "{record}");
        // Both have a null claim, which is why `state` has to exist.
        assert_eq!(record.matches("\"claim\":null").count(), 2, "{record}");
    }

    #[test]
    fn an_unreliable_reading_says_so_and_says_it_was_not_compared() {
        let catalogue = catalogue();
        let check = &catalogue.checks()[0];
        let record = check_record(&row(
            check,
            vec![
                Reading::new(Origin::Observed, Claim::from_support(Support::No))
                    .unreliable("the sentinel showed this means nothing"),
            ],
        ));
        assert!(record.contains("\"reliable\":false"), "{record}");
        assert!(record.contains("\"compared\":false"), "{record}");
    }

    #[test]
    fn the_run_record_reports_the_sentinel() {
        let catalogue = catalogue();
        drop(catalogue);
        let mut diagnosis = empty_diagnosis(Vec::new());
        diagnosis.sentinel = Sentinel {
            state: Some(ModeState::PermanentlyReset),
            answered_barrier: true,
        };
        let record = run_record(&diagnosis);
        assert!(record.contains("\"trust\":\"compromised\""), "{record}");
        assert!(record.contains("\"value\":4"), "{record}");
        assert!(record.contains("9876543"), "{record}");
        assert!(looks_like_json(record.trim_end()), "{record}");
    }

    #[test]
    fn the_summary_reports_the_exit_status_the_process_will_use() {
        let catalogue = catalogue();
        let check = &catalogue.checks()[0];

        // Nothing probed: 2, even with a disagreement between the two sources
        // that did answer.
        let mut diagnosis = empty_diagnosis(vec![row(
            check,
            vec![
                Reading::new(Origin::Terminfo, Claim::from_support(Support::No)),
                Reading::new(Origin::Environment, Claim::from_support(Support::Yes)),
            ],
        )]);
        assert!(summary_record(&diagnosis, false).contains("\"exit_code\":2"));

        // Probed, with a disagreement: 1.
        diagnosis.heard_anything = true;
        assert!(summary_record(&diagnosis, false).contains("\"exit_code\":1"));
        assert!(summary_record(&diagnosis, false).contains("\"disagreements\":1"));

        // Probed, agreeing: 0.
        let agreeing = row(
            check,
            vec![
                Reading::new(Origin::Terminfo, Claim::from_support(Support::Yes)),
                Reading::new(Origin::Environment, Claim::from_support(Support::Yes)),
            ],
        );
        let mut diagnosis = empty_diagnosis(vec![agreeing]);
        diagnosis.heard_anything = true;
        assert!(summary_record(&diagnosis, false).contains("\"exit_code\":0"));
    }

    #[test]
    fn valid_json_is_emitted_even_when_nothing_could_be_probed() {
        let catalogue = catalogue();
        let rows: Vec<Row<'_>> = catalogue
            .checks()
            .iter()
            .map(|check| {
                row(
                    check,
                    Origin::ALL
                        .iter()
                        .map(|&origin| Reading::silent(origin, "nothing to ask"))
                        .collect(),
                )
            })
            .collect();
        let diagnosis = empty_diagnosis(rows);
        let mut output = run_record(&diagnosis);
        for row in &diagnosis.rows {
            output.push_str(&check_record(row));
        }
        output.push_str(&summary_record(&diagnosis, false));
        for line in output.lines() {
            assert!(looks_like_json(line), "{line}");
        }
        assert!(output.contains("\"probed\":false"));
        assert!(output.contains("\"exit_code\":2"));
    }

    #[test]
    fn strings_are_escaped_so_a_reply_cannot_break_the_output() {
        let mut out = String::new();
        push_string(&mut out, "a\"b\\c\nd\te\u{1b}f\u{7f}");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\te\\u001bf\\u007f\"");
        assert!(looks_like_json(&format!("{{\"x\":{out}}}")));
    }

    #[test]
    fn a_terminal_reply_full_of_control_bytes_still_serialises() {
        let catalogue = catalogue();
        let check = &catalogue.checks()[0];
        let record = check_record(&row(
            check,
            vec![
                Reading::new(Origin::Terminal, Claim::from_support(Support::Yes))
                    .with_raw("\u{1b}]52;%p1%s\u{7}\n"),
            ],
        ));
        assert!(looks_like_json(record.trim_end()), "{record}");
        assert!(!record.trim_end().contains('\u{1b}'));
    }

    #[test]
    fn the_notes_survive_serialisation_with_their_newlines_escaped() {
        let catalogue = catalogue();
        for check in catalogue.checks() {
            let record = check_record(&row(check, Vec::new()));
            assert!(looks_like_json(record.trim_end()), "{}", check.id);
            assert!(record.contains("\"notes\":"), "{}", check.id);
            assert_eq!(record.lines().count(), 1, "{} spans lines", check.id);
        }
    }

    #[test]
    fn every_kind_and_agreement_has_a_stable_key() {
        use super::super::checks::CheckKind;
        let kinds = [CheckKind::Boolean, CheckKind::Value, CheckKind::Identity].map(kind_key);
        let mut sorted = kinds.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3);

        let agreements =
            [Agreement::Agree, Agreement::Disagree, Agreement::NotComparable].map(agreement_key);
        let mut sorted = agreements.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3);
    }

    #[test]
    fn the_channel_is_reported_when_one_was_opened() {
        let mut diagnosis = empty_diagnosis(Vec::new());
        diagnosis.channel = Some(Channel::Ctty);
        diagnosis.no_channel = None;
        let record = run_record(&diagnosis);
        assert!(record.contains("\"probe_channel\":\"/dev/tty\""), "{record}");
        assert!(record.contains("\"probe_failure\":null"), "{record}");
    }
}
