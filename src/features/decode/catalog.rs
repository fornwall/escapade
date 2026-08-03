//! Descriptions taken from the feature database, rather than written here.
//!
//! Every feature file lists its sequences, and each one carries a `request`
//! written symbolically: `"CSI ? 2027 h"`, `"OSC 8 ; ; https://example.com ST"`,
//! `"ESC _ 25a1 ; s ST"`, `"APC G i=31,s=1,v=1,a=q,t=d,f=24 ; AAAA ST"`. Those
//! strings are the only machine-readable statement in the database of *what
//! bytes* a feature is, so this module parses them into patterns and matches
//! decoded items against them. The consequence is the point: `decode` learns a new
//! sequence when a data file gains one, with no code change and no prose of its
//! own in the way.
//!
//! # How specific a match is
//!
//! A pattern is a list of fields, each either a literal (`8`, `4`, `notify`,
//! `ReportCellSize`) or a placeholder — anything that is plainly example data,
//! such as `https://example.com`, `id=neighbour-42` or `42 tests passed`. Matching
//! walks the leading fields and counts how many literals agree:
//!
//! * A mismatch in the **first** field disqualifies the pattern. `CSI 18 t` is not
//!   `CSI 22 t`.
//! * A mismatch later on stops the count but keeps the match, so
//!   `OSC 9 ; 4 ; 1 ; 77` still resolves to progress even though the data file's
//!   example says `42`.
//! * The best score wins, and ties are reported as ties — which is the honest
//!   answer for `CSI c`, a barrier two features share, and for `OSC 9`, a number
//!   iTerm2 and ConEmu both claimed.
//!
//! # What the schema does not say
//!
//! There is no field declaring *which* number or verb identifies a sequence, so
//! the discriminator is derived from `request` by the rules above. That works
//! because the data files spell sequences consistently, and it is the reason this
//! module is heuristic rather than a lookup: see the report in `TODO.md` for the
//! schema change that would make it exact.

use crate::db::Catalog;

use super::parse::{Fields, Item, StringKind};

/// One field of a pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    /// A field that has to agree, byte for byte or number for number.
    Literal(String),
    /// Example data, which agrees with anything.
    Any,
}

/// The byte shape of a database sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shape {
    /// `CSI <private> <params> <intermediates> <final>`.
    Csi {
        /// Private marker, if the request has one.
        private: Option<u8>,
        /// Intermediate bytes.
        intermediates: Vec<u8>,
        /// The final byte.
        final_byte: u8,
        /// Leading parameter fields.
        params: Vec<Field>,
    },
    /// `OSC <fields> ST`, split on `;`.
    Osc {
        /// The `;`-separated fields.
        fields: Vec<Field>,
    },
    /// `APC <key> …`, keyed by the identifier the payload starts with, and by the
    /// protocol verb where the feature declares one.
    Apc {
        /// `G` for kitty graphics, `25a1` for the Glyph Protocol.
        key: String,
        /// The `verb` field of the data file: `a=T` for kitty, `s` for the Glyph
        /// Protocol. Both protocols put several operations behind one key, so
        /// without this every kitty sequence would match every kitty row.
        verb: Option<String>,
    },
    /// `DCS <params> <intermediates> <final> …`.
    Dcs {
        /// Intermediate bytes.
        intermediates: Vec<u8>,
        /// The final byte.
        final_byte: u8,
        /// Leading parameter fields.
        params: Vec<Field>,
    },
}

/// How well a pattern matched, for choosing between candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Score {
    /// Literal fields that agreed, from the front.
    literals: usize,
    /// Whether the walk ended without disagreeing about anything.
    agreed: bool,
}

/// One database sequence, ready to match against.
#[derive(Debug, Clone)]
struct Pattern {
    /// Byte shape.
    shape: Shape,
    /// Feature id, e.g. `hyperlinks`.
    feature_id: String,
    /// Feature display name.
    feature_name: String,
    /// The sequence's short name from the data file.
    sequence_name: String,
    /// Its one-sentence purpose.
    purpose: String,
    /// The protocol verb, where the feature has verbs.
    verb: Option<String>,
}

/// What the database says about a decoded sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// Feature id, which is also the subcommand to run for more.
    pub feature_id: String,
    /// Feature display name.
    pub feature_name: String,
    /// The sequence's name in the data file.
    pub sequence_name: String,
    /// Its purpose, one sentence.
    pub purpose: String,
    /// The protocol verb, if any.
    pub verb: Option<String>,
}

/// Every database sequence, indexed by shape.
#[derive(Debug, Default)]
pub struct Index {
    /// The patterns, in database order.
    patterns: Vec<Pattern>,
}

impl Index {
    /// An index that matches nothing, which is the baseline the tests compare
    /// database-driven descriptions against.
    #[cfg(test)]
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds the index from every feature's `[[sequence]]` blocks.
    ///
    /// Requests that cannot be read as bytes — a combined example such as
    /// kitty's `"APC … ST then CSI … m …"`, or prose — are skipped rather than
    /// guessed at.
    #[must_use]
    pub fn from_catalog(catalog: &Catalog) -> Self {
        let mut patterns = Vec::new();
        for feature in catalog.features() {
            for sequence in &feature.sequence {
                if let Some(shape) =
                    parse_request_with_verb(&sequence.request, sequence.verb.as_deref())
                {
                    patterns.push(Pattern {
                        shape,
                        feature_id: feature.id.clone(),
                        feature_name: feature.name.clone(),
                        sequence_name: sequence.name.clone(),
                        purpose: sequence.purpose.clone(),
                        verb: sequence.verb.clone(),
                    });
                }
            }
        }
        Self { patterns }
    }

    /// How many patterns were understood, which is what a test asserts on.
    #[cfg(test)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// Whether the index knows nothing.
    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Everything the database says about this item, best matches first.
    ///
    /// Several features can share one sequence — the `CSI c` barrier does, by
    /// design — so this returns all the matches that tie for best rather than
    /// picking one and hiding the rest.
    #[must_use]
    pub fn lookup(&self, item: &Item) -> Vec<Match> {
        let mut scored: Vec<(Score, &Pattern)> = self
            .patterns
            .iter()
            .filter_map(|pattern| score(&pattern.shape, item).map(|score| (score, pattern)))
            .collect();
        if scored.is_empty() {
            return Vec::new();
        }
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let best = scored[0].0;
        let mut out: Vec<Match> = Vec::new();
        for pattern in scored
            .into_iter()
            .filter(|(score, _)| *score == best)
            .map(|(_, pattern)| pattern)
        {
            if out
                .iter()
                .any(|found| found.feature_id == pattern.feature_id)
            {
                continue;
            }
            out.push(Match {
                feature_id: pattern.feature_id.clone(),
                feature_name: pattern.feature_name.clone(),
                sequence_name: pattern.sequence_name.clone(),
                purpose: pattern.purpose.clone(),
                verb: pattern.verb.clone(),
            });
        }
        out
    }
}

/// Scores one pattern against one item.
fn score(shape: &Shape, item: &Item) -> Option<Score> {
    match (shape, item) {
        (
            Shape::Csi {
                private,
                intermediates,
                final_byte,
                params,
            },
            Item::Csi(fields),
        ) => {
            // An `SGR` parameter is a value, not an identifier: the data file's
            // `CSI 1;3;9 m` is an *example* of setting three attributes, and
            // reading its `1` as part of the sequence's identity would attribute
            // `CSI 1;31 m` to it while leaving `CSI 31 m` unattributed. So `SGR`
            // is matched on its final byte alone, and which attributes it sets is
            // named by `super::sgr` instead.
            let identifying: &[Field] = if private.is_none() && *final_byte == b'm' {
                &[]
            } else {
                params
            };
            score_fields(*private, intermediates, *final_byte, identifying, fields)
        }
        (
            Shape::Dcs {
                intermediates,
                final_byte,
                params,
            },
            Item::Dcs { fields, .. },
        ) => score_fields(None, intermediates, *final_byte, params, fields),
        (
            Shape::Osc { fields },
            Item::StringSeq {
                kind: StringKind::Osc,
                payload,
                ..
            },
        ) => {
            let text = String::from_utf8_lossy(payload);
            let actual: Vec<&str> = text.split(';').collect();
            let score = walk(fields, &actual)?;
            // An `OSC` is identified by its number, so a pattern that agreed
            // about nothing has not identified anything.
            (score.literals > 0).then_some(score)
        }
        (
            Shape::Apc { key, verb },
            Item::StringSeq {
                kind: StringKind::Apc,
                payload,
                ..
            },
        ) => {
            let text = String::from_utf8_lossy(payload);
            if key.is_empty() || apc_key(&text) != *key {
                return None;
            }
            let Some(verb) = verb else {
                return Some(Score {
                    literals: 1,
                    agreed: true,
                });
            };
            // The key says which protocol; the verb says which operation.
            Some(Score {
                literals: if has_verb(&text, verb) { 2 } else { 1 },
                agreed: has_verb(&text, verb),
            })
        }
        _ => None,
    }
}

/// Scores a `CSI`-shaped pattern: the framing has to be identical, and then the
/// leading parameters are walked.
fn score_fields(
    private: Option<u8>,
    intermediates: &[u8],
    final_byte: u8,
    params: &[Field],
    fields: &Fields,
) -> Option<Score> {
    if private != fields.private
        || intermediates != fields.intermediates.as_slice()
        || final_byte != fields.final_byte
    {
        return None;
    }
    let actual: Vec<&str> = if fields.raw.is_empty() {
        Vec::new()
    } else {
        fields.raw.split(';').collect()
    };
    walk(params, &actual)
}

/// Walks pattern fields against actual fields, counting the leading agreements.
fn walk(pattern: &[Field], actual: &[&str]) -> Option<Score> {
    let mut literals = 0;
    for (index, field) in pattern.iter().enumerate() {
        let Field::Literal(expected) = field else {
            // A placeholder ends the comparison without disagreeing.
            return Some(Score {
                literals,
                agreed: true,
            });
        };
        let Some(found) = actual.get(index) else {
            // The pattern is more specific than the item; nothing disagreed, but
            // nothing more agreed either.
            return Some(Score {
                literals,
                agreed: true,
            });
        };
        if same_field(expected, found) {
            literals += 1;
        } else if index == 0 {
            // The first field is the identity of the sequence.
            return None;
        } else {
            return Some(Score {
                literals,
                agreed: false,
            });
        }
    }
    Some(Score {
        literals,
        agreed: true,
    })
}

/// Whether a pattern field and an actual field are the same.
///
/// Numerically where both are numbers, so `0` and `00` agree, and textually
/// otherwise.
fn same_field(expected: &str, found: &str) -> bool {
    match (expected.parse::<u32>(), found.trim().parse::<u32>()) {
        (Ok(left), Ok(right)) => left == right,
        _ => expected == found.trim(),
    }
}

/// Whether an `APC` payload carries this verb.
///
/// A verb of the form `a=T` is one pair of the comma-separated list kitty's
/// protocol puts before the first `;`. A bare verb such as the Glyph Protocol's
/// `s` is the field after the key.
fn has_verb(payload: &str, verb: &str) -> bool {
    let verb = verb.trim();
    let head = payload.split(';').next().unwrap_or_default();
    if verb.contains('=') {
        // The key is glued to the first pair — kitty writes `Ga=T,f=100` — so it
        // comes off before the pairs are read.
        let pairs = head.strip_prefix(&apc_key(payload)).unwrap_or(head);
        return pairs
            .split(',')
            .any(|pair| pair.trim().eq_ignore_ascii_case(verb));
    }
    payload
        .split(';')
        .nth(1)
        .is_some_and(|field| field.trim() == verb)
}

/// Reads a data file's `request` into a shape, for the requests that have no verb.
#[cfg(test)]
fn parse_request(request: &str) -> Option<Shape> {
    parse_request_with_verb(request, None)
}

/// Reads a `request` into a shape, with the data file's `verb` field where it
/// has one. The verb is what tells one `APC` operation from another.
fn parse_request_with_verb(request: &str, verb: Option<&str>) -> Option<Shape> {
    let mut tokens: Vec<&str> = request.split_whitespace().collect();
    // Some requests show a whole interaction: `APC … ST then CSI … m …`. Only the
    // first sequence is a shape; the rest is illustration.
    if let Some(position) = tokens.iter().position(|token| *token == "then") {
        tokens.truncate(position);
    }
    // The introducer is either named or spelled `ESC ]`.
    let (kind, rest) = match tokens.split_first()? {
        (&"CSI", rest) => ('c', rest),
        (&"OSC", rest) => ('o', rest),
        (&"APC", rest) => ('a', rest),
        (&"DCS", rest) => ('d', rest),
        (&"ESC", rest) => match rest.split_first()? {
            (&"[", rest) => ('c', rest),
            (&"]", rest) => ('o', rest),
            (&"_", rest) => ('a', rest),
            (&"P", rest) => ('d', rest),
            _ => return None,
        },
        _ => return None,
    };
    // The terminator carries no information for matching.
    let mut body = rest.to_vec();
    while matches!(body.last(), Some(&"ST" | &"BEL")) {
        body.pop();
    }

    match kind {
        'c' | 'd' => {
            let shape = parse_control_shape(&body)?;
            if kind == 'c' {
                Some(Shape::Csi {
                    private: shape.private,
                    intermediates: shape.intermediates,
                    final_byte: shape.final_byte,
                    params: shape.params,
                })
            } else {
                Some(Shape::Dcs {
                    intermediates: shape.intermediates,
                    final_byte: shape.final_byte,
                    params: shape.params,
                })
            }
        }
        'o' => Some(Shape::Osc {
            fields: split_fields(&body.join(" ")),
        }),
        _ => Some(Shape::Apc {
            key: apc_key(&body.concat()),
            verb: verb.map(str::to_owned),
        }),
    }
}

/// The `CSI`-shaped part of a request.
#[derive(Debug)]
struct ControlShape {
    /// Private marker, if the request has one.
    private: Option<u8>,
    /// Intermediate bytes.
    intermediates: Vec<u8>,
    /// The final byte.
    final_byte: u8,
    /// Leading parameter fields.
    params: Vec<Field>,
}

/// Reads the `CSI`-shaped part of a request: private marker, parameters,
/// intermediates, final byte.
fn parse_control_shape(body: &[&str]) -> Option<ControlShape> {
    let mut private = None;
    let mut intermediates = Vec::new();
    let mut params: Vec<Field> = Vec::new();
    let mut final_byte = None;

    for (index, token) in body.iter().enumerate() {
        let single = (token.len() == 1).then(|| token.as_bytes()[0]);
        match single {
            Some(byte)
                if matches!(byte, b'<' | b'=' | b'>' | b'?')
                    && params.is_empty()
                    && intermediates.is_empty() =>
            {
                private = Some(byte);
            }
            Some(byte) if (0x20..=0x2f).contains(&byte) => intermediates.push(byte),
            Some(byte) if (0x40..=0x7e).contains(&byte) && index + 1 == body.len() => {
                final_byte = Some(byte);
            }
            _ => {
                if *token != ";" {
                    params.extend(split_fields(token));
                }
            }
        }
    }
    Some(ControlShape {
        private,
        intermediates,
        final_byte: final_byte?,
        params,
    })
}

/// Splits a `;`-separated run into fields, classifying each one.
fn split_fields(text: &str) -> Vec<Field> {
    text.split(';').map(classify).collect()
}

/// Decides whether a field is a literal or example data.
///
/// The rule is deliberately conservative: anything that could be example data is
/// treated as a placeholder, because a wrong literal *rejects* a real sequence,
/// while a wrong placeholder only makes a match less specific.
fn classify(field: &str) -> Field {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return Field::Literal(String::new());
    }
    if is_placeholder(trimmed) {
        Field::Any
    } else {
        Field::Literal(trimmed.to_owned())
    }
}

/// Whether a field is plainly not a literal.
fn is_placeholder(field: &str) -> bool {
    // Prose: `42 tests passed`, `Build finished`.
    if field.contains(char::is_whitespace) {
        return true;
    }
    // Example data and metasyntax: `https://example.com`, `id=neighbour-42`,
    // `<base64-glyf>`, `[ ; cp=E0A0 ]`.
    if field
        .chars()
        .any(|character| "<>[]{}=/:.%\"'…*|,".contains(character))
    {
        return true;
    }
    // The ECMA-48 convention for a parameter that stands for a number: `Ps`,
    // `Pn`, `Pt`.
    let mut characters = field.chars();
    matches!(
        (characters.next(), characters.next(), characters.next()),
        (Some('P'), Some(_), None)
    )
}

/// The identifier an `APC` payload starts with.
///
/// Shared with [`super::describe`], so the label on the line and the key the
/// database is looked up by are the same string by construction.
///
/// kitty's graphics protocol writes `Ga=T,f=100;…`, so its key is `G`; the Glyph
/// Protocol writes `25a1;s`, so its key is `25a1`. Both are found the same way:
/// take the payload up to the first `;`, then cut it before the first `key=value`
/// pair.
pub fn apc_key(payload: &str) -> String {
    let head = payload.split(';').next().unwrap_or_default();
    let mut previous: Option<usize> = None;
    for (index, character) in head.char_indices() {
        if character == '=' {
            return match previous {
                Some(start) => head[..start].to_owned(),
                None => String::new(),
            };
        }
        previous = Some(index);
    }
    head.to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{super::parse::decode_all, *};

    /// The embedded database, which needs no terminal and no network.
    fn index() -> Index {
        Index::from_catalog(&Catalog::load().unwrap())
    }

    /// The first item of a decode.
    fn first(bytes: &[u8]) -> Item {
        decode_all(bytes).into_iter().next().unwrap().item
    }

    fn features(index: &Index, bytes: &[u8]) -> Vec<String> {
        index
            .lookup(&first(bytes))
            .into_iter()
            .map(|found| found.feature_id)
            .collect()
    }

    #[test]
    fn the_database_yields_patterns_for_every_transport() {
        let index = index();
        assert!(
            index.len() >= 20,
            "only {} patterns were understood",
            index.len()
        );
    }

    #[test]
    fn osc_8_is_the_hyperlinks_feature() {
        let index = index();
        assert_eq!(
            features(&index, b"\x1b]8;;https://example.com\x1b\\"),
            vec!["hyperlinks"]
        );
        // The closing form, and the BEL-terminated spelling, are the same feature.
        assert_eq!(features(&index, b"\x1b]8;;\x1b\\"), vec!["hyperlinks"]);
        assert_eq!(
            features(&index, b"\x1b]8;;https://x\x07"),
            vec!["hyperlinks"]
        );
    }

    #[test]
    fn a_hyperlink_with_an_id_finds_the_right_row() {
        let index = index();
        let found = index.lookup(&first(b"\x1b]8;id=42;https://example.com\x1b\\"));
        assert_eq!(found[0].feature_id, "hyperlinks");
    }

    #[test]
    fn apc_g_is_kitty_graphics_and_apc_25a1_is_the_glyph_protocol() {
        let index = index();
        assert_eq!(
            features(&index, b"\x1b_Ga=T,f=100,i=42;AAAA\x1b\\"),
            vec!["kitty-graphics"]
        );
        assert_eq!(
            features(&index, b"\x1b_25a1;s\x1b\\"),
            vec!["glyph-protocol"]
        );
        assert_eq!(
            features(&index, b"\x1b_25a1;q;cp=E0A0\x1b\\"),
            vec!["glyph-protocol"]
        );
    }

    #[test]
    fn csi_2027_is_grapheme_clustering_in_both_directions() {
        let index = index();
        assert_eq!(
            features(&index, b"\x1b[?2027h"),
            vec!["grapheme-clustering"]
        );
        assert_eq!(
            features(&index, b"\x1b[?2027l"),
            vec!["grapheme-clustering"]
        );
        assert_eq!(
            features(&index, b"\x1b[?2027$p"),
            vec!["grapheme-clustering"]
        );
    }

    #[test]
    fn the_size_queries_are_the_dimensions_feature() {
        let index = index();
        assert_eq!(features(&index, b"\x1b[18t"), vec!["dimensions"]);
        assert_eq!(features(&index, b"\x1b[16t"), vec!["dimensions"]);
        // And a window operation no data file claims is not attributed to one.
        assert!(features(&index, b"\x1b[22t").is_empty());
    }

    #[test]
    fn a_shared_sequence_is_reported_as_shared() {
        // `CSI c` is the barrier several features pair their queries with, and
        // pretending it belongs to one of them would be a lie.
        let found = features(&index(), b"\x1b[c");
        assert!(found.len() > 1, "expected several features, got {found:?}");
        assert!(found.contains(&"dimensions".to_owned()), "{found:?}");
    }

    #[test]
    fn progress_survives_a_percentage_the_data_file_never_mentions() {
        // The data file's example is 42%. A real stream says 77%, and the match
        // must still be progress rather than the notification that shares OSC 9.
        let index = index();
        let found = index.lookup(&first(b"\x1b]9;4;1;77\x1b\\"));
        assert_eq!(found[0].feature_id, "progress", "{found:?}");
    }

    #[test]
    fn osc_9_with_text_is_a_notification_rather_than_progress() {
        let index = index();
        let found = index.lookup(&first(b"\x1b]9;Build finished\x1b\\"));
        assert_eq!(found[0].feature_id, "notifications", "{found:?}");
    }

    #[test]
    fn the_progress_states_are_told_apart_by_their_own_field() {
        let index = index();
        for (bytes, expected) in [
            (&b"\x1b]9;4;0;0\x1b\\"[..], "remove the indicator"),
            (b"\x1b]9;4;2\x1b\\", "error"),
            (b"\x1b]9;4;3;0\x1b\\", "indeterminate"),
        ] {
            let found = index.lookup(&first(bytes));
            assert_eq!(found[0].feature_id, "progress");
            assert_eq!(found[0].sequence_name, expected, "{bytes:?}");
        }
    }

    #[test]
    fn an_apc_verb_picks_the_operation_and_not_just_the_protocol() {
        // Both protocols put every operation behind one key, so the verb is what
        // tells a transmit from a query.
        let index = index();
        for (bytes, expected) in [
            (
                &b"\x1b_Ga=T,f=100,i=42;AAAA\x1b\\"[..],
                "Transmit and display",
            ),
            (b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\", "Query"),
            (b"\x1b_Ga=d,d=i,i=42\x1b\\", "Delete"),
            (b"\x1b_25a1;s\x1b\\", "Support"),
            (b"\x1b_25a1;q;cp=E0A0\x1b\\", "Query"),
            (b"\x1b_25a1;r;cp=E0A0;upm=1000;AA\x1b\\", "Register"),
        ] {
            let found = index.lookup(&first(bytes));
            assert_eq!(found[0].sequence_name, expected, "{bytes:?} -> {found:?}");
        }
    }

    #[test]
    fn an_apc_whose_verb_matches_nothing_still_names_the_protocol() {
        // A kitty sequence with an action no data file lists is still kitty
        // graphics, and saying so is better than saying nothing.
        let found = index().lookup(&first(b"\x1b_Ga=Z,i=1\x1b\\"));
        assert_eq!(found[0].feature_id, "kitty-graphics");
    }

    #[test]
    fn an_unclaimed_sequence_matches_nothing() {
        let index = index();
        assert!(features(&index, b"\x1b[3A").is_empty(), "cursor movement");
        assert!(
            features(&index, b"\x1b]0;title\x07").is_empty(),
            "window title"
        );
        assert!(
            features(&index, b"\x1b_Zunknown\x1b\\").is_empty(),
            "an unknown APC"
        );
    }

    #[test]
    fn every_sgr_belongs_to_the_text_attributes_feature() {
        // Not just the ones whose parameters happen to match a data file's
        // example: the parameters of an `SGR` say what it does, not what it is.
        let index = index();
        for bytes in [
            &b"\x1b[m"[..],
            b"\x1b[0m",
            b"\x1b[1;31m",
            b"\x1b[31m",
            b"\x1b[4:3m",
            b"\x1b[38;2;1;2;3m",
            b"\x1b[107m",
        ] {
            assert_eq!(
                features(&index, bytes),
                vec!["text-attributes"],
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn an_empty_index_matches_nothing_and_does_not_mind() {
        let index = Index::empty();
        assert!(index.is_empty());
        assert!(index.lookup(&first(b"\x1b]8;;https://x\x1b\\")).is_empty());
    }

    #[test]
    fn requests_are_read_into_the_shapes_they_describe() {
        assert_eq!(
            parse_request("CSI ? 2027 $ p"),
            Some(Shape::Csi {
                private: Some(b'?'),
                intermediates: vec![b'$'],
                final_byte: b'p',
                params: vec![Field::Literal("2027".to_owned())],
            })
        );
        assert_eq!(
            parse_request("CSI c"),
            Some(Shape::Csi {
                private: None,
                intermediates: Vec::new(),
                final_byte: b'c',
                params: Vec::new(),
            })
        );
        assert_eq!(
            parse_request("OSC 8 ; ; https://example.com ST"),
            Some(Shape::Osc {
                fields: vec![
                    Field::Literal("8".to_owned()),
                    Field::Literal(String::new()),
                    Field::Any,
                ],
            })
        );
        assert_eq!(
            parse_request_with_verb("ESC _ 25a1 ; s ST", Some("s")),
            Some(Shape::Apc {
                key: "25a1".to_owned(),
                verb: Some("s".to_owned()),
            })
        );
    }

    #[test]
    fn a_placeholder_parameter_does_not_become_a_literal() {
        // `CSI Ps + T` must match `CSI 3 + T`, so `Ps` cannot be a literal.
        let shape = parse_request("CSI Ps + T").unwrap();
        assert_eq!(
            shape,
            Shape::Csi {
                private: None,
                intermediates: vec![b'+'],
                final_byte: b'T',
                params: vec![Field::Any],
            }
        );
    }

    #[test]
    fn a_combined_example_is_cut_at_the_word_then() {
        // kitty's Unicode placeholder row shows an APC and a CSI in one request.
        let shape = parse_request("APC G a=p,U=1,i=42,c=2 ST   then   CSI 38;2;0;0;42 m  …");
        assert_eq!(
            shape,
            Some(Shape::Apc {
                key: "G".to_owned(),
                verb: None
            })
        );
    }

    #[test]
    fn a_request_that_is_not_a_sequence_is_skipped() {
        assert_eq!(parse_request("prose about a feature"), None);
        assert_eq!(parse_request(""), None);
        assert_eq!(parse_request("CSI 1"), None, "no final byte");
    }

    #[test]
    fn the_apc_key_is_the_identifier_before_the_parameters() {
        assert_eq!(apc_key("Gi=31,s=1,v=1;AAAA"), "G");
        assert_eq!(apc_key("Ga=T,f=100"), "G");
        assert_eq!(apc_key("25a1;s"), "25a1");
        assert_eq!(apc_key("25a1;r;cp=E0A0"), "25a1");
        assert_eq!(apc_key(""), "");
    }

    #[test]
    fn fields_are_compared_by_number_where_both_are_numbers() {
        assert!(same_field("0", "00"));
        assert!(same_field("8", " 8"));
        assert!(!same_field("8", "9"));
        assert!(same_field("notify", "notify"));
        assert!(!same_field("notify", "Notify"));
    }

    #[test]
    fn every_understood_pattern_carries_prose_worth_printing() {
        let index = index();
        for pattern in &index.patterns {
            assert!(
                !pattern.sequence_name.is_empty(),
                "{pattern:?} has no name to print"
            );
            assert!(
                !pattern.purpose.chars().any(char::is_control),
                "{pattern:?} carries a control byte"
            );
        }
    }
}
