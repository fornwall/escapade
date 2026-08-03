//! The feature database: terminal registry plus one file per feature.
//!
//! Everything `escapade` knows about a feature — its prose, its escape
//! sequences, who uses it and which terminals implement it — lives in TOML
//! under `data/`, in the spirit of <https://webassembly.org/features/>. The
//! files are baked into the binary with [`include_str!`], so the tool stays a
//! single self-contained executable, but they can be overridden at runtime by
//! pointing `ESCAPADE_DATA_DIR` at a checkout. That makes editing prose a
//! matter of saving a file rather than recompiling.
//!
//! # Adding a feature
//!
//! 1. Write `data/features/<id>.toml`, modelled on `glyph-protocol.toml`.
//! 2. Add one line to [`EMBEDDED_FEATURES`].
//! 3. Add a subcommand in [`crate::cli`] if the feature has a demo.
//!
//! Support rows reference terminals by `id`; [`Catalog::load`] rejects ids that
//! are not in the registry, so a typo fails loudly instead of quietly dropping
//! a row from the matrix.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

/// The terminal registry, shared by every feature's support matrix.
const EMBEDDED_TERMINALS: &str = include_str!("../data/terminals.toml");

/// Every known feature, as `(id, embedded TOML)`. Add a line here when adding
/// a feature file.
const EMBEDDED_FEATURES: &[(&str, &str)] = &[
    (
        "glyph-protocol",
        include_str!("../data/features/glyph-protocol.toml"),
    ),
    (
        "kitty-graphics",
        include_str!("../data/features/kitty-graphics.toml"),
    ),
    (
        "grapheme-clustering",
        include_str!("../data/features/grapheme-clustering.toml"),
    ),
    (
        "hyperlinks",
        include_str!("../data/features/hyperlinks.toml"),
    ),
    (
        "notifications",
        include_str!("../data/features/notifications.toml"),
    ),
    ("progress", include_str!("../data/features/progress.toml")),
    (
        "text-sizing",
        include_str!("../data/features/text-sizing.toml"),
    ),
    ("color", include_str!("../data/features/color.toml")),
    ("clipboard", include_str!("../data/features/clipboard.toml")),
    (
        "text-attributes",
        include_str!("../data/features/text-attributes.toml"),
    ),
    ("unscroll", include_str!("../data/features/unscroll.toml")),
    (
        "dimensions",
        include_str!("../data/features/dimensions.toml"),
    ),
    (
        "keyboard-protocol",
        include_str!("../data/features/keyboard-protocol.toml"),
    ),
];

/// Environment variable pointing at a `data/` directory to read instead of the
/// embedded copies, for editing content without a rebuild.
const DATA_DIR_ENV: &str = "ESCAPADE_DATA_DIR";

/// A terminal or multiplexer that can appear in a support matrix.
#[derive(Debug, Clone, Deserialize)]
pub struct Terminal {
    /// Lowercase kebab-case key referenced by feature support rows.
    pub id: String,
    /// Display name, spelled the way the project spells it.
    pub name: String,
    /// Project homepage, if it has one.
    pub url: Option<String>,
    /// Whether this sits between the application and the real terminal.
    #[serde(default)]
    pub kind: TerminalKind,
}

/// Distinguishes real terminals from multiplexers, which can block a feature
/// purely by filtering its escape sequences.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalKind {
    /// A terminal emulator.
    #[default]
    Terminal,
    /// A multiplexer such as tmux, Zellij or GNU Screen.
    Multiplexer,
}

#[derive(Debug, Deserialize)]
struct TerminalRegistry {
    terminal: Vec<Terminal>,
}

/// How settled a feature is as a standard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeatureStatus {
    /// Written down and implemented, but young and subject to change.
    Experimental,
    /// Specified, with no implementations yet.
    Proposed,
    /// No formal standard, but implemented widely enough to rely on.
    DeFactoStandard,
    /// Covered by ECMA-48, ISO or an equivalent formal standard.
    Standard,
}

impl FeatureStatus {
    /// Short human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Experimental => "experimental",
            Self::Proposed => "proposed",
            Self::DeFactoStandard => "de facto standard",
            Self::Standard => "standard",
        }
    }
}

/// Whether a particular terminal implements a particular feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportStatus {
    /// Implemented and enabled by default.
    Yes,
    /// Implemented with meaningful gaps.
    Partial,
    /// Implemented, but off until the user opts in.
    Flag,
    /// Multiplexer: forwards the sequence only when configured to.
    Passthrough,
    /// No known implementation.
    No,
    /// Nobody has checked.
    Unknown,
}

impl SupportStatus {
    /// Symbol used in the support matrix, chosen to survive a monospace grid.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Yes => "✔",
            Self::Partial => "◐",
            Self::Flag => "⚑",
            Self::Passthrough => "⇄",
            Self::No => "✘",
            Self::Unknown => "?",
        }
    }

    /// Short human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::Partial => "partial",
            Self::Flag => "behind a flag",
            Self::Passthrough => "needs passthrough",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }

    /// Sort key placing the rows a reader cares about at the top.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Yes => 0,
            Self::Partial => 1,
            Self::Flag => 2,
            Self::Passthrough => 3,
            Self::Unknown => 4,
            Self::No => 5,
        }
    }

    /// Whether an application can expect the feature to work here.
    #[must_use]
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Yes | Self::Partial | Self::Flag)
    }
}

/// One escape sequence in a feature's protocol.
#[derive(Debug, Clone, Deserialize)]
pub struct Sequence {
    /// The protocol verb, if the feature has verbs (Glyph Protocol's `s`, `q`, …).
    pub verb: Option<String>,
    /// Short name for the operation.
    pub name: String,
    /// What it is for, in one sentence.
    pub purpose: String,
    /// The sequence an application sends, written symbolically.
    pub request: String,
    /// The sequence the terminal sends back, if any.
    pub reply: Option<String>,
    /// Anything a reader would trip over.
    pub notes: Option<String>,
}

/// One parameter accepted by a feature's sequences.
#[derive(Debug, Clone, Deserialize)]
pub struct Param {
    /// Parameter name as it appears on the wire.
    pub key: String,
    /// Which verbs or contexts it applies to.
    pub applies_to: String,
    /// Default value; empty when the parameter is required or has none.
    #[serde(default)]
    pub default: String,
    /// What it does.
    pub description: String,
}

/// A project that speaks the protocol, on either end.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    /// Project name.
    pub name: String,
    /// `terminal`, `library`, `application`, …
    pub kind: String,
    /// Where to look.
    pub url: Option<String>,
    /// What it does with the feature.
    pub notes: Option<String>,
}

/// One row of a feature's support matrix.
#[derive(Debug, Clone, Deserialize)]
pub struct Support {
    /// Terminal id, which must exist in the registry.
    pub terminal: String,
    /// Whether it works there.
    pub status: SupportStatus,
    /// Version or date support arrived.
    pub since: Option<String>,
    /// Caveats, gaps, configuration needed.
    pub notes: Option<String>,
    /// Where this claim was checked.
    pub source: Option<String>,
}

/// A link worth following.
#[derive(Debug, Clone, Deserialize)]
pub struct Resource {
    /// Link text.
    pub title: String,
    /// Target.
    pub url: String,
}

/// Everything known about one terminal feature.
#[derive(Debug, Clone, Deserialize)]
pub struct Feature {
    /// Kebab-case id, matching both the file name and the subcommand name.
    pub id: String,
    /// Display name.
    pub name: String,
    /// One-sentence pitch.
    pub tagline: String,
    /// How settled the feature is.
    pub status: FeatureStatus,
    /// Who designed it.
    pub author: Option<String>,
    /// When it appeared, as `YYYY-MM`.
    pub introduced: Option<String>,
    /// Which escape-sequence class carries it (`APC`, `CSI`, `OSC`, `DCS`).
    pub transport: Option<String>,
    /// Canonical specification.
    pub spec_url: Option<String>,
    /// Specification version this file was written against.
    pub spec_version: Option<String>,
    /// Example code, if the project publishes any.
    pub examples_url: Option<String>,
    /// What it does and why it exists. Markdown.
    pub summary: String,
    /// The problem it displaces, and the alternatives. Markdown.
    #[serde(default)]
    pub problem: Option<String>,
    /// Wire format and framing. Markdown.
    #[serde(default)]
    pub how_it_works: Option<String>,
    /// Payload format details. Markdown.
    #[serde(default)]
    pub payload: Option<String>,
    /// Trust model and threat mitigations. Markdown.
    #[serde(default)]
    pub security: Option<String>,
    /// Practical gotchas. Markdown.
    #[serde(default)]
    pub caveats: Option<String>,
    /// The protocol's sequences.
    #[serde(default)]
    pub sequence: Vec<Sequence>,
    /// The protocol's parameters.
    #[serde(default)]
    pub param: Vec<Param>,
    /// Projects using it.
    #[serde(default)]
    pub user: Vec<User>,
    /// Per-terminal support.
    #[serde(default)]
    pub support: Vec<Support>,
    /// Further reading.
    #[serde(default)]
    pub resource: Vec<Resource>,
}

impl Feature {
    /// Support rows sorted most-interesting-first, paired with their registry
    /// entry so callers get display names and URLs without a second lookup.
    #[must_use]
    pub fn support_matrix<'a>(&'a self, catalog: &'a Catalog) -> Vec<(&'a Terminal, &'a Support)> {
        let mut rows: Vec<(&Terminal, &Support)> = self
            .support
            .iter()
            .filter_map(|support| {
                catalog
                    .terminal(&support.terminal)
                    .map(|terminal| (terminal, support))
            })
            .collect();
        rows.sort_by_key(|(terminal, support)| {
            (
                support.status.rank(),
                terminal.kind == TerminalKind::Multiplexer,
            )
        });
        rows
    }

    /// Terminals where an application can expect the feature to work.
    #[must_use]
    pub fn supported_by<'a>(&'a self, catalog: &'a Catalog) -> Vec<&'a str> {
        self.support
            .iter()
            .filter(|support| support.status.is_usable())
            .filter_map(|support| catalog.terminal(&support.terminal))
            .map(|terminal| terminal.name.as_str())
            .collect()
    }
}

/// The loaded database.
#[derive(Debug)]
pub struct Catalog {
    terminals: Vec<Terminal>,
    by_id: BTreeMap<String, usize>,
    features: Vec<Feature>,
}

impl Catalog {
    /// Loads the registry and every feature file, then checks cross-references.
    ///
    /// # Errors
    ///
    /// Fails if a file is missing or malformed, if a feature's `id` disagrees
    /// with its file name, or if a support row names an unknown terminal.
    pub fn load() -> Result<Self> {
        let overlay = env::var_os(DATA_DIR_ENV).map(PathBuf::from);

        let terminals_toml = match &overlay {
            Some(dir) => read_overlay(dir, "terminals.toml")?,
            None => None,
        };
        let terminals_toml = terminals_toml.unwrap_or_else(|| EMBEDDED_TERMINALS.to_owned());
        let registry: TerminalRegistry =
            toml::from_str(&terminals_toml).context("parsing terminals.toml")?;

        let mut by_id = BTreeMap::new();
        for (index, terminal) in registry.terminal.iter().enumerate() {
            if by_id.insert(terminal.id.clone(), index).is_some() {
                bail!("terminals.toml: duplicate terminal id `{}`", terminal.id);
            }
        }

        let mut features = Vec::with_capacity(EMBEDDED_FEATURES.len());
        for (id, embedded) in EMBEDDED_FEATURES {
            let relative = format!("features/{id}.toml");
            let toml_text = match &overlay {
                Some(dir) => read_overlay(dir, &relative)?,
                None => None,
            };
            let toml_text = toml_text.unwrap_or_else(|| (*embedded).to_owned());
            let feature: Feature =
                toml::from_str(&toml_text).with_context(|| format!("parsing {relative}"))?;
            if feature.id != *id {
                bail!("{relative}: declares id `{}`, expected `{id}`", feature.id);
            }
            features.push(feature);
        }

        let catalog = Self {
            terminals: registry.terminal,
            by_id,
            features,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    fn validate(&self) -> Result<()> {
        for feature in &self.features {
            let mut seen = BTreeSet::new();
            for support in &feature.support {
                if self.terminal(&support.terminal).is_none() {
                    bail!(
                        "feature `{}`: support row names terminal `{}`, which is not in terminals.toml",
                        feature.id,
                        support.terminal
                    );
                }
                if !seen.insert(&support.terminal) {
                    bail!(
                        "feature `{}`: duplicate support row for `{}`",
                        feature.id,
                        support.terminal
                    );
                }
            }
        }
        Ok(())
    }

    /// Looks a terminal up by registry id.
    #[must_use]
    pub fn terminal(&self, id: &str) -> Option<&Terminal> {
        self.by_id.get(id).map(|&index| &self.terminals[index])
    }

    /// Looks a feature up by id.
    ///
    /// # Errors
    ///
    /// Fails if no feature has that id, which would mean a subcommand exists
    /// without a matching data file.
    pub fn feature(&self, id: &str) -> Result<&Feature> {
        self.features
            .iter()
            .find(|feature| feature.id == id)
            .with_context(|| format!("no feature `{id}` in the database"))
    }

    /// Every feature, in registry order.
    #[must_use]
    pub fn features(&self) -> &[Feature] {
        &self.features
    }
}

/// Reads `relative` from an overlay directory, returning `Ok(None)` when the
/// file is simply absent so the embedded copy can be used instead.
fn read_overlay(dir: &Path, relative: &str) -> Result<Option<String>> {
    let path = dir.join(relative);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_data_loads_and_validates() {
        let catalog = Catalog::load().unwrap();
        assert!(!catalog.features().is_empty());
        assert!(catalog.terminal("rio").is_some());
    }

    #[test]
    fn glyph_protocol_is_complete_enough_to_explain() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature("glyph-protocol").unwrap();

        assert_eq!(feature.name, "Glyph Protocol");
        assert_eq!(feature.status, FeatureStatus::Experimental);
        // The four verbs: support, query, register, clear.
        assert_eq!(feature.sequence.len(), 4);
        assert!(!feature.param.is_empty());
        assert!(!feature.user.is_empty());
        assert!(!feature.resource.is_empty());
        assert!(feature.spec_url.is_some());
    }

    #[test]
    fn support_matrix_resolves_every_row_and_leads_with_support() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature("glyph-protocol").unwrap();

        let rows = feature.support_matrix(&catalog);
        assert_eq!(
            rows.len(),
            feature.support.len(),
            "every support row must resolve to a registry entry"
        );

        let ranks: Vec<u8> = rows.iter().map(|(_, s)| s.status.rank()).collect();
        assert!(
            ranks.windows(2).all(|pair| pair[0] <= pair[1]),
            "rows should be ordered by status: {ranks:?}"
        );
        assert_eq!(rows[0].0.id, "rio", "the one implementation should lead");
    }

    #[test]
    fn data_files_never_embed_a_literal_escape_byte() {
        // A raw 0x1b in the prose would be executed by the reader's terminal
        // when `explain` prints it, rather than shown as text.
        assert!(!EMBEDDED_TERMINALS.contains('\u{1b}'));
        for (id, text) in EMBEDDED_FEATURES {
            assert!(!text.contains('\u{1b}'), "{id}.toml contains a raw ESC");
        }
    }

    #[test]
    fn data_files_never_embed_an_invisible_character() {
        // Writing a Private Use Area character into the prose is self-defeating:
        // it renders as tofu for exactly the readers these files are explaining
        // tofu to. Spell such codepoints out as `U+E0A0` instead. Same for the
        // zero-width characters, which are impossible to notice in review.
        let invisible = |ch: char| {
            matches!(ch as u32,
                0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD)
                || matches!(
                    ch,
                    '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{00ad}'
                )
        };
        for (id, text) in EMBEDDED_FEATURES {
            if let Some(found) = text.chars().find(|&ch| invisible(ch)) {
                panic!(
                    "{id}.toml contains the invisible character U+{:04X} — write it as a \
                     codepoint reference instead",
                    found as u32
                );
            }
        }
    }
}
