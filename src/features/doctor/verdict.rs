//! What each source claimed, and whether the claims agree.
//!
//! This is the part no other tool does. chafa silently upgrades from one source to
//! another, termprofile silently merges them, termwiz keeps the paths disjoint —
//! all of which produce an answer, and none of which can tell you that the answer
//! came from a source that was wrong. So the model here keeps the sources apart to
//! the very end and compares them.
//!
//! Three things have to stay distinct, and folding any two together is how a
//! diagnostic starts lying:
//!
//! * a source that said **no** — a claim, and comparable;
//! * a source that **cannot** answer — no such capability exists, no such query
//!   does. A permanent gap, and reporting it as a no would invent a disagreement
//!   with every source that says yes;
//! * a source that was asked and **said nothing** — an admission, not a claim.
//!
//! Only claims are compared, and only reliable ones: an answer the sentinel
//! showed to be meaningless must not be allowed to manufacture a disagreement.

use crate::features::Support;

/// What one source said about one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// A definite answer, normalised so two sources can be compared. `yes` and
    /// `no` for a boolean check, an erase character or a terminal family
    /// otherwise.
    Says(String),
    /// This source cannot express an answer here. Not a no.
    NotApplicable,
    /// The source was asked and said nothing.
    Silent,
}

impl Claim {
    /// A `yes`/`no` claim, using the same vocabulary the probes report through.
    ///
    /// [`Support::Unknown`] becomes [`Claim::Silent`], because "could not tell" is
    /// an admission rather than an answer.
    #[must_use]
    pub fn from_support(support: Support) -> Self {
        match support {
            Support::Yes | Support::No => Self::Says(support.as_str().to_owned()),
            Support::Unknown => Self::Silent,
        }
    }

    /// A claim about a value: an erase character, a terminal family.
    #[must_use]
    pub fn value(text: impl Into<String>) -> Self {
        Self::Says(text.into())
    }

    /// The normalised answer, when there is one.
    #[must_use]
    pub fn says(&self) -> Option<&str> {
        match self {
            Self::Says(text) => Some(text),
            _ => None,
        }
    }

    /// How this reads in a table cell.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Says(text) => text,
            Self::NotApplicable => "n/a",
            Self::Silent => "no answer",
        }
    }

    /// Symbol for a table cell, matching the support matrix's vocabulary.
    #[must_use]
    pub fn symbol(&self) -> &'static str {
        match self {
            Self::Says(text) if text == Support::Yes.as_str() => "✔",
            Self::Says(text) if text == Support::No.as_str() => "✘",
            Self::Says(_) => "•",
            Self::NotApplicable => "·",
            Self::Silent => "?",
        }
    }
}

/// Which of the four sources a reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The terminfo entry for `$TERM`.
    Terminfo,
    /// The terminal's own answer, via XTGETTCAP.
    Terminal,
    /// What happened when the thing was actually done.
    Observed,
    /// An environment variable's assertion.
    Environment,
}

impl Origin {
    /// Column heading.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Terminfo => "terminfo",
            Self::Terminal => "terminal",
            Self::Observed => "observed",
            Self::Environment => "env",
        }
    }

    /// Key used in JSON output.
    #[must_use]
    pub fn key(self) -> &'static str {
        self.label()
    }

    /// The four sources, in report order.
    pub const ALL: [Self; 4] = [
        Self::Terminfo,
        Self::Terminal,
        Self::Observed,
        Self::Environment,
    ];
}

/// One source's answer about one check.
#[derive(Debug, Clone)]
pub struct Reading {
    /// Which source this is.
    pub origin: Origin,
    /// The normalised answer, or why there is none.
    pub claim: Claim,
    /// Exactly what the source produced, already made safe to print.
    pub raw: Option<String>,
    /// Why the claim was read that way, when it is not obvious.
    pub detail: Option<String>,
    /// Whether the answer can be relied on.
    ///
    /// An unreliable reading is still shown — hiding it would lose the finding —
    /// but it is excluded from the comparison, so it can neither create nor hide a
    /// disagreement.
    pub reliable: bool,
}

impl Reading {
    /// A reading with nothing else to say.
    #[must_use]
    pub fn new(origin: Origin, claim: Claim) -> Self {
        Self {
            origin,
            claim,
            raw: None,
            detail: None,
            reliable: true,
        }
    }

    /// A source that cannot answer this check at all.
    #[must_use]
    pub fn not_applicable(origin: Origin, why: impl Into<String>) -> Self {
        Self {
            origin,
            claim: Claim::NotApplicable,
            raw: None,
            detail: Some(why.into()),
            reliable: true,
        }
    }

    /// A source that was asked and said nothing.
    #[must_use]
    pub fn silent(origin: Origin, why: impl Into<String>) -> Self {
        Self {
            origin,
            claim: Claim::Silent,
            raw: None,
            detail: Some(why.into()),
            reliable: true,
        }
    }

    /// Attaches the source's raw output.
    #[must_use]
    pub fn with_raw(mut self, raw: impl Into<String>) -> Self {
        self.raw = Some(raw.into());
        self
    }

    /// Attaches an explanation.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Marks the reading as not to be relied on, and says why.
    #[must_use]
    pub fn unreliable(mut self, why: impl Into<String>) -> Self {
        self.reliable = false;
        self.detail = Some(why.into());
        self
    }

    /// The claim, if it counts towards the comparison.
    #[must_use]
    pub fn comparable(&self) -> Option<&str> {
        if self.reliable {
            self.claim.says()
        } else {
            None
        }
    }
}

/// Whether the sources agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agreement {
    /// Two or more sources gave reliable answers and they match.
    Agree,
    /// Two or more sources gave reliable answers and they differ. This is the
    /// finding.
    Disagree,
    /// Fewer than two reliable answers, so there was nothing to compare. Not a
    /// pass and not a failure.
    NotComparable,
}

impl Agreement {
    /// How this reads in JSON, where `agrees` is `true`, `false` or `null`.
    ///
    /// `null` for [`Agreement::NotComparable`] is what keeps the documented
    /// `select(.agrees == false)` correct: an incomparable row is not a
    /// disagreement, and reporting it as one would flood the output with rows
    /// nothing is wrong with.
    #[must_use]
    pub fn json(self) -> &'static str {
        match self {
            Self::Agree => "true",
            Self::Disagree => "false",
            Self::NotComparable => "null",
        }
    }

    /// Short label for a person.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Agree => "agree",
            Self::Disagree => "DISAGREE",
            Self::NotComparable => "not comparable",
        }
    }
}

/// Compares the readings for one check.
///
/// Only reliable claims count. Two sources that both say `yes` agree; a `yes` and
/// a `no` disagree; a single claim with three silences has nothing to agree or
/// disagree with, and saying so is the honest answer.
#[must_use]
pub fn agreement(readings: &[Reading]) -> Agreement {
    let mut claims = readings.iter().filter_map(Reading::comparable);
    let Some(first) = claims.next() else {
        return Agreement::NotComparable;
    };
    let mut count = 1;
    for claim in claims {
        count += 1;
        if claim != first {
            return Agreement::Disagree;
        }
    }
    if count >= 2 {
        Agreement::Agree
    } else {
        Agreement::NotComparable
    }
}

/// Names the sources on each side of a disagreement: `terminfo says no, observed
/// says yes`.
#[must_use]
pub fn describe_split(readings: &[Reading]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for reading in readings {
        if let Some(claim) = reading.comparable() {
            parts.push(format!("{} says {claim}", reading.origin.label()));
        }
    }
    parts.join(", ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn says(origin: Origin, answer: &str) -> Reading {
        Reading::new(origin, Claim::value(answer))
    }

    fn yes(origin: Origin) -> Reading {
        Reading::new(origin, Claim::from_support(Support::Yes))
    }

    fn no(origin: Origin) -> Reading {
        Reading::new(origin, Claim::from_support(Support::No))
    }

    #[test]
    fn two_sources_saying_the_same_thing_agree() {
        let readings = vec![yes(Origin::Terminfo), yes(Origin::Observed)];
        assert_eq!(agreement(&readings), Agreement::Agree);
    }

    #[test]
    fn a_yes_against_a_no_is_the_finding() {
        // The truecolor case: COLORTERM asserts it, terminfo has neither RGB nor
        // Tc, and this row is why the subcommand exists.
        let readings = vec![no(Origin::Terminfo), yes(Origin::Environment)];
        assert_eq!(agreement(&readings), Agreement::Disagree);
        let split = describe_split(&readings);
        assert!(split.contains("terminfo says no"), "{split}");
        assert!(split.contains("env says yes"), "{split}");
    }

    #[test]
    fn one_claim_alone_is_not_agreement() {
        // Three silences and a yes prove nothing. Reporting this as agreement
        // would be the tool congratulating itself.
        let readings = vec![
            yes(Origin::Terminfo),
            Reading::silent(Origin::Terminal, "no reply"),
            Reading::silent(Origin::Observed, "no reply"),
            Reading::not_applicable(Origin::Environment, "no variable asserts this"),
        ];
        assert_eq!(agreement(&readings), Agreement::NotComparable);
    }

    #[test]
    fn nothing_to_compare_is_not_a_disagreement_in_json() {
        // `select(.agrees == false)` must not pick up rows nothing is wrong with.
        assert_eq!(Agreement::NotComparable.json(), "null");
        assert_eq!(Agreement::Disagree.json(), "false");
        assert_eq!(Agreement::Agree.json(), "true");
    }

    #[test]
    fn a_source_that_cannot_answer_never_creates_a_disagreement() {
        // Synchronized output: terminfo has no capability for mode 2026 at all.
        // Reading that absence as a no would put every terminal that implements
        // the mode in disagreement with a source that never had an opinion.
        let readings = vec![
            Reading::not_applicable(Origin::Terminfo, "no terminfo capability exists"),
            Reading::not_applicable(Origin::Terminal, "no capability name to ask for"),
            yes(Origin::Observed),
        ];
        assert_eq!(agreement(&readings), Agreement::NotComparable);
    }

    #[test]
    fn an_unreliable_reading_cannot_manufacture_a_disagreement() {
        // iTerm2's sentinel case: the DECRQM answer is the same one it gives to a
        // mode that cannot exist, so it must not be allowed to contradict
        // terminfo.
        let readings = vec![
            yes(Origin::Terminfo),
            yes(Origin::Terminal),
            no(Origin::Observed).unreliable("the sentinel showed this value means nothing"),
        ];
        assert_eq!(agreement(&readings), Agreement::Agree);
        assert!(!describe_split(&readings).contains("observed"));
    }

    #[test]
    fn an_unreliable_reading_cannot_hide_one_either() {
        let readings = vec![
            yes(Origin::Terminfo),
            no(Origin::Terminal),
            yes(Origin::Observed).unreliable("meaningless here"),
        ];
        assert_eq!(agreement(&readings), Agreement::Disagree);
    }

    #[test]
    fn four_sources_agreeing_is_still_agreement() {
        let readings = vec![
            yes(Origin::Terminfo),
            yes(Origin::Terminal),
            yes(Origin::Observed),
            yes(Origin::Environment),
        ];
        assert_eq!(agreement(&readings), Agreement::Agree);
    }

    #[test]
    fn a_disagreement_among_three_is_found_wherever_it_sits() {
        for odd in 0..3 {
            let mut readings = vec![
                yes(Origin::Terminfo),
                yes(Origin::Terminal),
                yes(Origin::Observed),
            ];
            readings[odd] = no(readings[odd].origin);
            assert_eq!(
                agreement(&readings),
                Agreement::Disagree,
                "odd one at {odd}"
            );
        }
    }

    #[test]
    fn non_boolean_claims_compare_by_value() {
        // Backspace: `kbs` against the tty's erase character.
        assert_eq!(
            agreement(&[says(Origin::Terminfo, "^?"), says(Origin::Observed, "^?")]),
            Agreement::Agree
        );
        assert_eq!(
            agreement(&[says(Origin::Terminfo, "^H"), says(Origin::Observed, "^?")]),
            Agreement::Disagree
        );
        // Identity: families rather than spellings.
        assert_eq!(
            agreement(&[
                says(Origin::Terminfo, "xterm"),
                says(Origin::Observed, "kitty")
            ]),
            Agreement::Disagree
        );
    }

    #[test]
    fn no_readings_at_all_is_not_comparable() {
        assert_eq!(agreement(&[]), Agreement::NotComparable);
        assert_eq!(describe_split(&[]), "");
    }

    #[test]
    fn unknown_support_is_silence_rather_than_a_claim() {
        assert_eq!(Claim::from_support(Support::Unknown), Claim::Silent);
        assert_eq!(Claim::from_support(Support::Yes).says(), Some("yes"));
        assert_eq!(Claim::from_support(Support::No).says(), Some("no"));
        assert_eq!(Claim::Silent.says(), None);
        assert_eq!(Claim::NotApplicable.says(), None);
    }

    #[test]
    fn the_three_non_claims_are_labelled_apart() {
        // Folding any two of these together is how a diagnostic starts lying.
        let labels = [
            Claim::from_support(Support::No).label().to_owned(),
            Claim::NotApplicable.label().to_owned(),
            Claim::Silent.label().to_owned(),
        ];
        let mut sorted = labels.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "{labels:?}");

        let symbols = [
            Claim::from_support(Support::No).symbol(),
            Claim::NotApplicable.symbol(),
            Claim::Silent.symbol(),
        ];
        let mut sorted = symbols.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "{symbols:?}");
    }

    #[test]
    fn every_origin_has_a_distinct_label() {
        let mut labels: Vec<&str> = Origin::ALL.iter().map(|origin| origin.label()).collect();
        labels.sort_unstable();
        let total = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), total);
    }

    #[test]
    fn a_reading_carries_its_raw_value_and_reason() {
        let reading = yes(Origin::Terminfo)
            .with_raw("RGB")
            .with_detail("the RGB extension is present");
        assert_eq!(reading.raw.as_deref(), Some("RGB"));
        assert!(reading.detail.as_deref().unwrap().contains("RGB extension"));
        assert_eq!(reading.comparable(), Some("yes"));
    }
}
