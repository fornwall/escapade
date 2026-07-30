//! Source one: what the terminfo database claims.
//!
//! The entry is read by shelling out to `infocmp`, which every system with a
//! terminfo database has and which needs no dependency. `-x` is not optional:
//! the capabilities that matter most here — `RGB`, `Tc`, `Ms`, `BE`, `XM`,
//! `Sync` — are all ncurses *user-defined* extensions, and without `-x`
//! `infocmp` does not print them. A tool that reads the entry through a library
//! which drops extensions concludes the terminal has none of them, which is the
//! most common way these capabilities get lost.
//!
//! `-1` puts one capability per line, which is what makes the output worth
//! parsing rather than unpicking:
//!
//! ```text
//! #  Reconstructed via infocmp from file: …/x/xterm-256color
//! xterm-256color|xterm with 256 colors,
//!         am, bce, ..., colors#0x100, ..., smcup=\E[?1049h\E[22;0;0t,
//! ```
//!
//! `infocmp` being absent is itself a finding worth reporting, not an error: it
//! means nothing on this machine can tell an application what `$TERM` claims.

use std::{
    collections::BTreeMap,
    process::{Command, Stdio},
};

/// One terminfo capability's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A boolean capability, present by virtue of being listed: `am`, `Tc`.
    Boolean,
    /// A numeric capability: `colors#256`, printed in hex once large.
    Number(String),
    /// A string capability: `smcup=\E[?1049h`. Kept exactly as `infocmp`
    /// rendered it, which means `\E` rather than a real escape byte — the value
    /// is therefore always safe to print.
    String(String),
}

impl Value {
    /// The value as text, for reporting and for threshold comparison. `None` for
    /// a boolean, which has no value.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Boolean => None,
            Self::Number(text) | Self::String(text) => Some(text),
        }
    }
}

/// A parsed terminfo entry.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    /// The entry's primary name, as the first field of the alias line.
    pub name: String,
    /// The human description, as the last field of the alias line.
    pub description: String,
    /// Every capability `infocmp -x` printed.
    pub caps: BTreeMap<String, Value>,
}

impl Entry {
    /// Looks a capability up.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.caps.get(name)
    }
}

/// What happened when the terminfo entry was read.
#[derive(Debug, Clone)]
pub enum Source {
    /// The entry was read.
    Entry {
        /// The entry itself.
        entry: Box<Entry>,
        /// Which name was looked up.
        queried: String,
    },
    /// `infocmp` is not installed. Nothing on this machine can say what `$TERM`
    /// claims, which is a finding in its own right.
    NoInfocmp,
    /// `infocmp` ran and refused: no entry for that name, usually.
    Failed {
        /// Which name was looked up.
        queried: String,
        /// What `infocmp` said, trimmed to one line.
        reason: String,
    },
    /// `$TERM` is unset and no `--terminfo` name was given, so there was nothing
    /// to look up.
    NoTerm,
}

impl Source {
    /// The entry, when there is one.
    #[must_use]
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Self::Entry { entry, .. } => Some(entry),
            _ => None,
        }
    }

    /// The name that was looked up, when there was one.
    #[must_use]
    pub fn queried(&self) -> Option<&str> {
        match self {
            Self::Entry { queried, .. } | Self::Failed { queried, .. } => Some(queried),
            _ => None,
        }
    }

    /// One line a reader can act on.
    #[must_use]
    pub fn explanation(&self) -> String {
        match self {
            Self::Entry { entry, queried } => {
                if entry.name == *queried {
                    format!("{} — {}", entry.name, entry.description)
                } else {
                    // `infocmp xterm` can resolve to an entry that calls itself
                    // something else; say so rather than quietly renaming it.
                    format!(
                        "{queried} resolves to {} — {}",
                        entry.name, entry.description
                    )
                }
            }
            Self::NoInfocmp => {
                "`infocmp` is not installed, so the terminfo entry could not be read. Nothing on \
                 this machine can tell an application what $TERM claims."
                    .to_owned()
            }
            Self::Failed { queried, reason } => {
                format!("`infocmp {queried}` failed: {reason}")
            }
            Self::NoTerm => {
                "$TERM is unset and no --terminfo name was given, so there was no entry to read."
                    .to_owned()
            }
        }
    }
}

/// Reads a terminfo entry, preferring `wanted` over `$TERM`.
///
/// Never fails: every way this can go wrong is a [`Source`] variant, because
/// "the terminfo source is missing" is a diagnosis rather than a crash.
#[must_use]
pub fn read(wanted: Option<&str>) -> Source {
    let name = match wanted.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => name.to_owned(),
        None => match std::env::var("TERM") {
            Ok(term) if !term.trim().is_empty() => term,
            _ => return Source::NoTerm,
        },
    };

    // `-x` for user-defined extensions, `-1` for one capability per line.
    let output = Command::new("infocmp")
        .args(["-x", "-1", &name])
        .stdin(Stdio::null())
        .output();

    let output = match output {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Source::NoInfocmp,
        Err(error) => {
            return Source::Failed {
                queried: name,
                reason: error.to_string(),
            };
        }
    };

    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr)
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("no output")
            .trim()
            .to_owned();
        return Source::Failed {
            queried: name,
            reason,
        };
    }

    let text = String::from_utf8_lossy(&output.stdout);
    Source::Entry {
        entry: Box::new(parse(&text)),
        queried: name,
    }
}

/// Parses `infocmp -x -1` output.
///
/// Comment lines start with `#`. The first non-comment line is the alias line,
/// `name|alias|…|description,`. Every line after it holds capabilities, one per
/// line with `-1` but tolerating several per line so the parser also copes with
/// output produced without it.
#[must_use]
pub fn parse(text: &str) -> Entry {
    let mut entry = Entry::default();
    let mut seen_alias_line = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !seen_alias_line {
            seen_alias_line = true;
            let names = trimmed.trim_end_matches(',');
            let mut fields = names.split('|');
            fields.next().unwrap_or_default().trim().clone_into(&mut entry.name);
            // The description is the last field; an entry with no aliases and no
            // description leaves it empty rather than repeating the name.
            fields
                .next_back()
                .unwrap_or_default()
                .trim()
                .clone_into(&mut entry.description);
            continue;
        }
        for cap in split_capabilities(trimmed) {
            if let Some((name, value)) = parse_capability(cap) {
                entry.caps.insert(name, value);
            }
        }
    }

    entry
}

/// Splits a capability line on commas, ignoring commas inside a string value.
///
/// A string capability can contain a comma — `acsc` is full of them, and
/// `%,` appears in parameterised strings — so a naive `split(',')` shreds the
/// entry. `\,` is terminfo's escape for a literal comma; anything else is a
/// separator.
fn split_capabilities(line: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let bytes = line.as_bytes();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b',' => {
                parts.push(line[start..index].trim());
                index += 1;
                start = index;
            }
            _ => index += 1,
        }
    }
    if start < line.len() {
        parts.push(line[start..].trim());
    }
    parts.retain(|part| !part.is_empty());
    parts
}

/// Parses one capability: `am`, `colors#0x100` or `smcup=\E[?1049h`.
///
/// The `=` and `#` forms are distinguished by whichever comes first, since a
/// string value may perfectly well contain a `#`.
fn parse_capability(cap: &str) -> Option<(String, Value)> {
    let cap = cap.trim();
    if cap.is_empty() {
        return None;
    }
    let equals = cap.find('=');
    let hash = cap.find('#');
    let split = match (equals, hash) {
        (Some(equals), Some(hash)) => Some((equals.min(hash), cap.as_bytes()[equals.min(hash)])),
        (Some(equals), None) => Some((equals, b'=')),
        (None, Some(hash)) => Some((hash, b'#')),
        (None, None) => None,
    };

    match split {
        None => Some((cap.to_owned(), Value::Boolean)),
        Some((at, kind)) => {
            let name = cap[..at].trim();
            if name.is_empty() {
                return None;
            }
            let value = cap[at + 1..].to_owned();
            let value = if kind == b'#' {
                Value::Number(value.trim().to_owned())
            } else {
                Value::String(value)
            };
            Some((name.to_owned(), value))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Real `infocmp -x -1 xterm-256color` output, abridged.
    const XTERM: &str = "\
#\tReconstructed via infocmp from file: /usr/share/terminfo/x/xterm-256color
xterm-256color|xterm with 256 colors,
\tam,
\tbce,
\tcolors#0x100,
\tit#8,
\tkbs=^?,
\trmcup=\\E[?1049l\\E[23;0;0t,
\tsmcup=\\E[?1049h\\E[22;0;0t,
\tacsc=++\\,\\,--..00``aaffgg,
\tBD=\\E[?2004l,
\tBE=\\E[?2004h,
\tMs=\\E]52;%p1%s;%p2%s\\007,
\tXM=\\E[?1006;1000%?%p1%{1}%=%th%el%;,
";

    #[test]
    fn parses_the_alias_line() {
        let entry = parse(XTERM);
        assert_eq!(entry.name, "xterm-256color");
        assert_eq!(entry.description, "xterm with 256 colors");
    }

    #[test]
    fn parses_the_three_capability_shapes() {
        let entry = parse(XTERM);
        assert_eq!(entry.get("am"), Some(&Value::Boolean));
        assert_eq!(entry.get("colors"), Some(&Value::Number("0x100".to_owned())));
        assert_eq!(
            entry.get("kbs"),
            Some(&Value::String("^?".to_owned()))
        );
        assert_eq!(entry.get("am").unwrap().text(), None);
        assert_eq!(entry.get("colors").unwrap().text(), Some("0x100"));
    }

    #[test]
    fn parses_the_extensions_that_only_dash_x_prints() {
        // These are the whole reason `-x` is passed. Without them the tool would
        // report bracketed paste and OSC 52 as unsupported everywhere.
        let entry = parse(XTERM);
        assert!(entry.get("BE").is_some());
        assert!(entry.get("BD").is_some());
        assert!(entry.get("Ms").is_some());
        assert!(entry.get("XM").is_some());
    }

    #[test]
    fn a_string_value_survives_its_own_commas() {
        // `acsc=++\,\,--..` would be shredded by a naive split on commas, taking
        // every capability after it on the line with it.
        let entry = parse(XTERM);
        assert_eq!(
            entry.get("acsc").unwrap().text(),
            Some("++\\,\\,--..00``aaffgg")
        );
        // And the capability after `acsc` is still there.
        assert!(entry.get("BD").is_some());
    }

    #[test]
    fn several_capabilities_on_one_line_still_parse() {
        // Output produced without `-1` wraps many capabilities per line.
        let entry = parse("foo|Foo,\n\tam, bce, colors#8, kbs=^H,\n");
        assert_eq!(entry.get("am"), Some(&Value::Boolean));
        assert_eq!(entry.get("colors"), Some(&Value::Number("8".to_owned())));
        assert_eq!(entry.get("kbs").unwrap().text(), Some("^H"));
    }

    #[test]
    fn a_hash_inside_a_string_value_is_not_a_numeric_separator() {
        // `#` is legal inside a string capability, and taking it for the numeric
        // separator would rename the capability and lose the value.
        let entry = parse("foo|Foo,\n\tu6=\\E[%i%d#%dR,\n");
        assert_eq!(entry.get("u6").unwrap().text(), Some("\\E[%i%d#%dR"));
        assert!(entry.get("u6=\\E[%i%d").is_none());
    }

    #[test]
    fn an_equals_inside_a_string_value_is_kept() {
        let entry = parse("foo|Foo,\n\tSync=\\EP=%p1%ds\\E\\\\,\n");
        assert_eq!(entry.get("Sync").unwrap().text(), Some("\\EP=%p1%ds\\E\\\\"));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let entry = parse("# a comment\n\n#\tanother\nfoo|Foo,\n\n\tam,\n");
        assert_eq!(entry.name, "foo");
        assert_eq!(entry.caps.len(), 1);
    }

    #[test]
    fn an_entry_with_aliases_takes_the_last_field_as_the_description() {
        let entry = parse("st-256color|stterm-256color|simpleterm with 256 colors,\n\tam,\n");
        assert_eq!(entry.name, "st-256color");
        assert_eq!(entry.description, "simpleterm with 256 colors");
    }

    #[test]
    fn an_entry_with_no_description_does_not_invent_one() {
        let entry = parse("dumb,\n\tam,\n");
        assert_eq!(entry.name, "dumb");
        assert_eq!(entry.description, "");
    }

    #[test]
    fn empty_output_parses_to_an_empty_entry() {
        let entry = parse("");
        assert_eq!(entry.name, "");
        assert!(entry.caps.is_empty());
    }

    #[test]
    fn a_missing_infocmp_explains_itself() {
        let source = Source::NoInfocmp;
        assert!(source.entry().is_none());
        assert!(source.explanation().contains("infocmp"));
        assert!(source.queried().is_none());
    }

    #[test]
    fn a_resolved_alias_is_reported_as_a_resolution() {
        let source = Source::Entry {
            entry: Box::new(parse("xterm-256color|xterm with 256 colors,\n\tam,\n")),
            queried: "xterm".to_owned(),
        };
        let explanation = source.explanation();
        assert!(explanation.contains("resolves to"), "{explanation}");
        assert!(explanation.contains("xterm-256color"), "{explanation}");
    }

    #[test]
    fn a_matching_name_is_not_reported_as_a_resolution() {
        let source = Source::Entry {
            entry: Box::new(parse("foot|footclient,\n\tam,\n")),
            queried: "foot".to_owned(),
        };
        assert!(!source.explanation().contains("resolves to"));
    }

    #[test]
    fn split_capabilities_keeps_escaped_commas_together() {
        assert_eq!(split_capabilities("am, bce,"), vec!["am", "bce"]);
        assert_eq!(split_capabilities("acsc=a\\,b, am,"), vec!["acsc=a\\,b", "am"]);
        assert_eq!(split_capabilities(",,,"), Vec::<&str>::new());
    }

    #[test]
    fn a_capability_with_no_name_is_dropped_rather_than_stored() {
        assert_eq!(parse_capability("=oops"), None);
        assert_eq!(parse_capability(""), None);
        assert_eq!(parse_capability("   "), None);
    }
}
