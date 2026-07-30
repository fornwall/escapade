//! The check catalogue: `data/checks.toml`.
//!
//! Following [`crate::db`], the file is baked into the binary with
//! [`include_str!`] so the tool stays a single executable, and an overlay
//! directory named by `ESCAPADE_DATA_DIR` wins over the embedded copy so prose
//! and capability lists can be edited without a rebuild.
//!
//! Everything about a check lives in the file except the observed probe, which
//! has to be code: [`ObservedProbe`] is the enum the `observed` field names.
//! That split is the point — which terminfo capability maps to which check, what
//! the terminal is asked for, and what a disagreement means are all data.

use std::{env, fs, path::PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

/// The embedded catalogue.
const EMBEDDED_CHECKS: &str = include_str!("../../../data/checks.toml");

/// Environment variable pointing at a `data/` directory to read instead of the
/// embedded copy. Same variable [`crate::db`] uses, so one export covers both.
const DATA_DIR_ENV: &str = "ESCAPADE_DATA_DIR";

/// How the sources' answers for one check are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckKind {
    /// Present or absent. Every source's answer normalises to `yes` or `no`.
    #[default]
    Boolean,
    /// The capability's value itself, normalised — `kbs` against the tty's
    /// erase character.
    Value,
    /// A terminal family name, normalised through
    /// [`crate::features::doctor::identity`].
    Identity,
}

/// Which observed probe a check runs.
///
/// This is the one field that cannot be data: the probe is a conversation with
/// the terminal, so the file names a variant and the code implements it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedProbe {
    /// No observed test exists, and the check's notes say why. Not a failure —
    /// a capability nobody can measure is a finding.
    None,
    /// Set an RGB foreground and read the graphic rendition back with DECRQSS:
    /// `CSI 38 ; 2 ; 1 ; 2 ; 3 m` then `DCS $ q m ST`.
    SgrReadback,
    /// Ask a DEC private mode for its state: `CSI ? <mode> $ p` (DECRQM).
    Decrqm,
    /// The cell-size ladder: `CSI 16 t`, then `OSC 1337 ; ReportCellSize ST`,
    /// then `CSI 14 t` divided by `CSI 18 t`.
    CellSize,
    /// XTVERSION (`CSI > 0 q`) backed by Primary and Secondary Device
    /// Attributes.
    Identity,
    /// The tty's own erase character, read with `stty -a`.
    SttyErase,
}

impl ObservedProbe {
    /// Resolves the `observed` field of a check.
    fn parse(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            "sgr-readback" => Some(Self::SgrReadback),
            "decrqm" => Some(Self::Decrqm),
            "cell-size" => Some(Self::CellSize),
            "identity" => Some(Self::Identity),
            "stty-erase" => Some(Self::SttyErase),
            _ => None,
        }
    }

    /// The names accepted in the data file, for the error message when one is
    /// misspelled.
    const NAMES: &'static [&'static str] = &[
        "none",
        "sgr-readback",
        "decrqm",
        "cell-size",
        "identity",
        "stty-erase",
    ];
}

/// One capability to read, with an optional numeric threshold.
///
/// Written `Name` for plain presence or `Name>=N` for a numeric capability that
/// has to reach `N`. The threshold form is what lets `colors` participate in the
/// truecolor check without the rule moving into Rust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapSpec {
    /// Capability name as terminfo and XTGETTCAP spell it.
    pub name: String,
    /// Smallest numeric value that counts as present, if any.
    pub min: Option<u64>,
}

impl CapSpec {
    /// Parses `Name` or `Name>=N`.
    ///
    /// # Errors
    ///
    /// Fails on an empty name or an unparseable threshold, so a typo in the data
    /// file is loud rather than silently treated as a capability nothing has.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if let Some((name, min)) = text.split_once(">=") {
            let name = name.trim();
            if name.is_empty() {
                bail!("capability spec `{text}` has no name");
            }
            let min = min
                .trim()
                .parse::<u64>()
                .with_context(|| format!("capability spec `{text}`: threshold is not a number"))?;
            return Ok(Self {
                name: name.to_owned(),
                min: Some(min),
            });
        }
        if text.is_empty() {
            bail!("empty capability spec");
        }
        if text.contains(['>', '<', '=']) {
            bail!("capability spec `{text}`: only the `Name>=N` comparison is supported");
        }
        Ok(Self {
            name: text.to_owned(),
            min: None,
        })
    }

    /// Whether this spec names the terminal's own description rather than a
    /// capability.
    ///
    /// `TN` is XTGETTCAP's spelling and `name` is terminfo's; both mean the
    /// entry name.
    #[must_use]
    pub fn is_terminal_name(&self) -> bool {
        self.name == "TN" || self.name == "name"
    }

    /// Whether a value satisfies the threshold, if there is one.
    ///
    /// A spec with a threshold and a value that is not a number is *not*
    /// satisfied: `colors>=16777216` must not be talked into a yes by a string.
    #[must_use]
    pub fn satisfied_by(&self, value: Option<&str>) -> bool {
        match self.min {
            None => true,
            Some(min) => value.and_then(parse_number).is_some_and(|got| got >= min),
        }
    }
}

/// Parses a terminfo numeric value, which `infocmp` prints in hex once it is
/// large: `colors#0x100`.
#[must_use]
pub fn parse_number(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse::<u64>().ok(),
    }
}

/// One check, as loaded and validated.
#[derive(Debug, Clone)]
pub struct Check {
    /// Kebab-case id. This is what `--only` matches and what JSON reports.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The question this check answers, one sentence.
    pub question: String,
    /// How the sources' answers are compared.
    pub kind: CheckKind,
    /// Capabilities to read from the terminfo entry.
    pub terminfo: Vec<CapSpec>,
    /// Capability names to ask the terminal for with XTGETTCAP.
    pub xtgettcap: Vec<CapSpec>,
    /// Environment variables that assert the capability.
    pub env: Vec<String>,
    /// Which observed probe runs.
    pub observed: ObservedProbe,
    /// DEC private mode number, for [`ObservedProbe::Decrqm`].
    pub mode: Option<u32>,
    /// What a disagreement here means in practice. Markdown.
    pub notes: String,
}

/// The file's shape, before validation.
#[derive(Debug, Deserialize)]
struct CheckFile {
    check: Vec<RawCheck>,
}

#[derive(Debug, Deserialize)]
struct RawCheck {
    id: String,
    name: String,
    question: String,
    #[serde(default)]
    kind: CheckKind,
    #[serde(default)]
    terminfo: Vec<String>,
    #[serde(default)]
    xtgettcap: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    observed: String,
    #[serde(default)]
    mode: Option<u32>,
    notes: String,
}

/// The loaded catalogue, in file order.
#[derive(Debug)]
pub struct Catalogue {
    checks: Vec<Check>,
}

impl Catalogue {
    /// Loads and validates the catalogue.
    ///
    /// # Errors
    ///
    /// Fails if the file is missing from the overlay directory in a way that is
    /// not "simply absent", if it does not parse, or if any entry is malformed.
    /// Validation is deliberately strict: a check whose `observed` names a probe
    /// that does not exist would otherwise silently report `n/a` forever.
    pub fn load() -> Result<Self> {
        let text = match env::var_os(DATA_DIR_ENV).map(PathBuf::from) {
            Some(dir) => {
                let path = dir.join("checks.toml");
                match fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        EMBEDDED_CHECKS.to_owned()
                    }
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("reading {}", path.display()));
                    }
                }
            }
            None => EMBEDDED_CHECKS.to_owned(),
        };
        Self::parse(&text)
    }

    /// Parses and validates catalogue text. Split out so tests can feed it
    /// deliberately broken files.
    ///
    /// # Errors
    ///
    /// Fails on malformed TOML or on any entry that fails validation.
    pub fn parse(text: &str) -> Result<Self> {
        let file: CheckFile = toml::from_str(text).context("parsing checks.toml")?;
        if file.check.is_empty() {
            bail!("checks.toml: no [[check]] entries");
        }

        let mut checks = Vec::with_capacity(file.check.len());
        for raw in file.check {
            checks.push(convert(&raw)?);
        }

        for (index, check) in checks.iter().enumerate() {
            if checks[..index].iter().any(|other| other.id == check.id) {
                bail!("checks.toml: duplicate check id `{}`", check.id);
            }
        }

        Ok(Self { checks })
    }

    /// Every check, in file order.
    #[must_use]
    pub fn checks(&self) -> &[Check] {
        &self.checks
    }

    /// Check ids, for `--list` and for [`crate::features::select_steps`].
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.checks.iter().map(|check| check.id.as_str()).collect()
    }
}

/// Validates one raw entry into a [`Check`].
fn convert(raw: &RawCheck) -> Result<Check> {
    let id = raw.id.trim().to_owned();
    if id.is_empty() {
        bail!("checks.toml: a check has an empty id");
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("checks.toml: check id `{id}` is not lowercase kebab-case");
    }
    for (field, value) in [("name", &raw.name), ("question", &raw.question), ("notes", &raw.notes)]
    {
        if value.trim().is_empty() {
            bail!("checks.toml: check `{id}` has an empty `{field}`");
        }
    }

    let observed = ObservedProbe::parse(raw.observed.trim()).with_context(|| {
        format!(
            "checks.toml: check `{id}` names observed probe `{}`, which does not exist. \
             Known probes: {}",
            raw.observed,
            ObservedProbe::NAMES.join(", ")
        )
    })?;

    match (observed, raw.mode) {
        (ObservedProbe::Decrqm, None) => {
            bail!("checks.toml: check `{id}` uses observed = \"decrqm\" but sets no `mode`");
        }
        (ObservedProbe::Decrqm, Some(0)) => {
            bail!("checks.toml: check `{id}` sets mode = 0, which is not a DEC private mode");
        }
        (probe, Some(mode)) if probe != ObservedProbe::Decrqm => {
            bail!(
                "checks.toml: check `{id}` sets mode = {mode} but its observed probe is \
                 `{}`, which does not take one",
                raw.observed
            );
        }
        _ => {}
    }

    let terminfo = parse_specs(&id, "terminfo", &raw.terminfo)?;
    let xtgettcap = parse_specs(&id, "xtgettcap", &raw.xtgettcap)?;

    let env: Vec<String> = raw.env.iter().map(|name| name.trim().to_owned()).collect();
    if env.iter().any(String::is_empty) {
        bail!("checks.toml: check `{id}` has an empty environment variable name");
    }

    // A check that no source can answer would print four blanks forever.
    if terminfo.is_empty()
        && xtgettcap.is_empty()
        && env.is_empty()
        && observed == ObservedProbe::None
    {
        bail!("checks.toml: check `{id}` has no source that can answer it");
    }

    Ok(Check {
        id,
        name: raw.name.trim().to_owned(),
        question: raw.question.trim().to_owned(),
        kind: raw.kind,
        terminfo,
        xtgettcap,
        env,
        observed,
        mode: raw.mode,
        notes: raw.notes.trim().to_owned(),
    })
}

/// Parses a list of capability specs, naming the check and field on failure.
fn parse_specs(id: &str, field: &str, specs: &[String]) -> Result<Vec<CapSpec>> {
    specs
        .iter()
        .map(|spec| {
            CapSpec::parse(spec)
                .with_context(|| format!("checks.toml: check `{id}`, field `{field}`"))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_catalogue_loads_and_validates() {
        let catalogue = Catalogue::load().unwrap();
        assert!(catalogue.checks().len() >= 8, "the brief asks for a real set");
        let ids = catalogue.ids();
        for wanted in [
            "truecolor",
            "alt-screen",
            "bracketed-paste",
            "synchronized-output",
            "clipboard",
            "backspace",
            "mouse",
            "cell-size",
            "identity",
        ] {
            assert!(ids.contains(&wanted), "missing check `{wanted}`");
        }
    }

    #[test]
    fn every_decrqm_check_names_a_mode_and_nothing_else_does() {
        let catalogue = Catalogue::load().unwrap();
        for check in catalogue.checks() {
            if check.observed == ObservedProbe::Decrqm {
                assert!(check.mode.is_some(), "{} needs a mode", check.id);
            } else {
                assert!(check.mode.is_none(), "{} must not set a mode", check.id);
            }
        }
    }

    #[test]
    fn the_catalogue_never_embeds_a_literal_escape_byte() {
        // A raw 0x1b in the prose would be executed by the reader's terminal
        // when the notes are printed, rather than shown as text.
        assert!(!EMBEDDED_CHECKS.contains('\u{1b}'));
        assert!(!EMBEDDED_CHECKS.contains('\u{7}'));
    }

    #[test]
    fn the_catalogue_never_embeds_an_invisible_character() {
        let invisible = |ch: char| {
            matches!(ch as u32, 0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD)
                || matches!(ch, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{00ad}')
        };
        assert_eq!(EMBEDDED_CHECKS.chars().find(|&ch| invisible(ch)), None);
    }

    #[test]
    fn cap_specs_parse_presence_and_thresholds() {
        assert_eq!(
            CapSpec::parse("RGB").unwrap(),
            CapSpec { name: "RGB".to_owned(), min: None }
        );
        assert_eq!(
            CapSpec::parse("colors>=16777216").unwrap(),
            CapSpec { name: "colors".to_owned(), min: Some(16_777_216) }
        );
        // Whitespace is tolerated on both sides of the operator.
        assert_eq!(
            CapSpec::parse(" Co >= 256 ").unwrap(),
            CapSpec { name: "Co".to_owned(), min: Some(256) }
        );
    }

    #[test]
    fn cap_specs_reject_what_they_cannot_honour() {
        // Silently treating these as a capability nothing has would make the
        // whole check quietly wrong.
        assert!(CapSpec::parse("").is_err());
        assert!(CapSpec::parse(">=5").is_err());
        assert!(CapSpec::parse("colors>=lots").is_err());
        assert!(CapSpec::parse("colors>256").is_err());
        assert!(CapSpec::parse("colors=256").is_err());
    }

    #[test]
    fn a_threshold_is_not_satisfied_by_a_string() {
        let spec = CapSpec::parse("colors>=16777216").unwrap();
        assert!(spec.satisfied_by(Some("16777216")));
        assert!(spec.satisfied_by(Some("0x1000000")));
        assert!(!spec.satisfied_by(Some("256")));
        assert!(!spec.satisfied_by(Some("0x100")));
        assert!(!spec.satisfied_by(Some("yes")));
        assert!(!spec.satisfied_by(None));

        // Without a threshold, presence is the whole test — including a
        // boolean capability, which has no value at all.
        let plain = CapSpec::parse("RGB").unwrap();
        assert!(plain.satisfied_by(None));
        assert!(plain.satisfied_by(Some("")));
    }

    #[test]
    fn terminfo_numbers_may_be_hex() {
        // `infocmp` prints large numerics in hex: `colors#0x100`.
        assert_eq!(parse_number("256"), Some(256));
        assert_eq!(parse_number("0x100"), Some(256));
        assert_eq!(parse_number("0x1000000"), Some(16_777_216));
        assert_eq!(parse_number(" 8 "), Some(8));
        assert_eq!(parse_number("nope"), None);
        assert_eq!(parse_number(""), None);
    }

    #[test]
    fn both_spellings_of_the_entry_name_are_recognised() {
        assert!(CapSpec::parse("TN").unwrap().is_terminal_name());
        assert!(CapSpec::parse("name").unwrap().is_terminal_name());
        assert!(!CapSpec::parse("RGB").unwrap().is_terminal_name());
    }

    #[test]
    fn an_unknown_observed_probe_fails_loudly() {
        let error = Catalogue::parse(
            "[[check]]\nid = \"x\"\nname = \"X\"\nquestion = \"?\"\nobserved = \"telepathy\"\n\
             notes = \"n\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("telepathy"), "{error}");
        assert!(error.contains("sgr-readback"), "should list the real names: {error}");
    }

    #[test]
    fn decrqm_without_a_mode_fails_loudly() {
        let error = Catalogue::parse(
            "[[check]]\nid = \"x\"\nname = \"X\"\nquestion = \"?\"\nobserved = \"decrqm\"\n\
             notes = \"n\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("mode"), "{error}");
    }

    #[test]
    fn a_mode_on_a_probe_that_takes_none_fails_loudly() {
        let error = Catalogue::parse(
            "[[check]]\nid = \"x\"\nname = \"X\"\nquestion = \"?\"\nobserved = \"identity\"\n\
             mode = 2026\nnotes = \"n\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("2026"), "{error}");
    }

    #[test]
    fn a_check_no_source_can_answer_fails_loudly() {
        let error = Catalogue::parse(
            "[[check]]\nid = \"x\"\nname = \"X\"\nquestion = \"?\"\nobserved = \"none\"\n\
             notes = \"n\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no source"), "{error}");
    }

    #[test]
    fn duplicate_ids_fail_loudly() {
        let one = "[[check]]\nid = \"x\"\nname = \"X\"\nquestion = \"?\"\nobserved = \"none\"\n\
                   terminfo = [\"Ms\"]\nnotes = \"n\"\n";
        let error = Catalogue::parse(&format!("{one}{one}")).unwrap_err().to_string();
        assert!(error.contains("duplicate"), "{error}");
    }

    #[test]
    fn a_bad_id_fails_loudly() {
        let error = Catalogue::parse(
            "[[check]]\nid = \"True_Color\"\nname = \"X\"\nquestion = \"?\"\n\
             observed = \"none\"\nterminfo = [\"Ms\"]\nnotes = \"n\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("kebab-case"), "{error}");
    }

    #[test]
    fn an_empty_catalogue_fails_loudly() {
        assert!(Catalogue::parse("").is_err());
    }

    #[test]
    fn every_probe_name_round_trips() {
        for name in ObservedProbe::NAMES {
            assert!(ObservedProbe::parse(name).is_some(), "{name}");
        }
    }
}
