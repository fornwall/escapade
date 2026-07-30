//! Per-attribute support, at a finer grain than the support matrix can express.
//!
//! `data/features/text-attributes.toml` has one row per terminal and one status per
//! row, which is the right shape for a table a person reads. It is the wrong shape
//! for the question `escapade style probe --overline` asks, because support here is
//! emphatically **not** all-or-nothing. Every terminal in the table renders bold; a
//! good many ignore `SGR 2`; overline is rare; and xterm draws a plain underline and
//! none of the five shapes. One verdict for "text attributes" would be wrong for
//! almost every attribute in the family.
//!
//! So the per-attribute detail lives here, next to the code that reasons about it,
//! and the data file stays the summary. A test checks the two agree about which
//! terminals exist.
//!
//! # Three states, on purpose
//!
//! A capability in [`Row::renders`] is a yes. One in [`Row::unverified`] is an honest
//! "nobody has checked *this attribute* on *this terminal*". One in neither is a no.
//! Folding the middle case into a no is exactly the confident-wrong-answer failure
//! this project exists to avoid, and it is why this table has gaps in it rather than
//! guesses: a row that claimed `no` for something nobody had looked at would be
//! indistinguishable, to a reader, from one that had been checked.
//!
//! This is the *fallback* path. Where the terminal implements DECRQSS,
//! [`crate::features::style::probe`] measures the attribute instead of looking it up,
//! and a measurement always wins — which is what keeps a table that is inevitably
//! slightly out of date from being the last word.

use crate::features::{
    Support,
    style::{
        attrs::{Attr, UnderlineShape},
        color::{ColorDepth, ColorRole},
    },
};

/// One thing a terminal either does or does not do.
///
/// A key for the table, so it carries only what distinguishes one capability from
/// another: the underline *shape*, and a colour's *role* and *depth*. The actual
/// colour value is irrelevant to whether a terminal can express it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// `SGR 1`.
    Bold,
    /// `SGR 2`.
    Dim,
    /// `SGR 3`.
    Italic,
    /// `SGR 4` with the given shape.
    Underline(UnderlineShape),
    /// `SGR 5`.
    Blink,
    /// `SGR 6`, rendered *differently* from `SGR 5`.
    RapidBlink,
    /// `SGR 7`.
    Reverse,
    /// `SGR 8`.
    Conceal,
    /// `SGR 9`.
    Strikethrough,
    /// `SGR 53`.
    Overline,
    /// A colour of the given role at the given depth.
    Color(ColorRole, ColorDepth),
}

/// The capability an attribute needs the terminal to have.
#[must_use]
pub fn capability_of(attr: Attr) -> Capability {
    match attr {
        Attr::Bold => Capability::Bold,
        Attr::Dim => Capability::Dim,
        Attr::Italic => Capability::Italic,
        Attr::Underline(shape) => Capability::Underline(shape),
        Attr::Blink => Capability::Blink,
        Attr::RapidBlink => Capability::RapidBlink,
        Attr::Reverse => Capability::Reverse,
        Attr::Conceal => Capability::Conceal,
        Attr::Strikethrough => Capability::Strikethrough,
        Attr::Overline => Capability::Overline,
        Attr::Color(role, color) => Capability::Color(role, color.depth()),
    }
}

// The shared groups, so a row reads as what makes it different from the others.

/// Bold, reverse and a plain underline: the VT100 set, which everything renders.
const VT100: &[Capability] = &[
    Capability::Bold,
    Capability::Reverse,
    Capability::Underline(UnderlineShape::None),
    Capability::Underline(UnderlineShape::Single),
];

/// The four underline shapes that need sub-parameter support.
const ALL_SHAPES: &[Capability] = &[
    Capability::Underline(UnderlineShape::Double),
    Capability::Underline(UnderlineShape::Curly),
    Capability::Underline(UnderlineShape::Dotted),
    Capability::Underline(UnderlineShape::Dashed),
];

/// The two shapes that arrived first and are the most widely implemented.
const EARLY_SHAPES: &[Capability] = &[
    Capability::Underline(UnderlineShape::Double),
    Capability::Underline(UnderlineShape::Curly),
];

/// The two shapes that came later and are the least evenly implemented.
const LATE_SHAPES: &[Capability] = &[
    Capability::Underline(UnderlineShape::Dotted),
    Capability::Underline(UnderlineShape::Dashed),
];

/// Basic and 256-colour foreground and background — the floor for anything modern.
const PALETTE_COLORS: &[Capability] = &[
    Capability::Color(ColorRole::Foreground, ColorDepth::Basic),
    Capability::Color(ColorRole::Background, ColorDepth::Basic),
    Capability::Color(ColorRole::Foreground, ColorDepth::Palette),
    Capability::Color(ColorRole::Background, ColorDepth::Palette),
];

/// 24-bit foreground and background.
const TRUE_COLORS: &[Capability] = &[
    Capability::Color(ColorRole::Foreground, ColorDepth::TrueColor),
    Capability::Color(ColorRole::Background, ColorDepth::TrueColor),
];

/// Every depth of underline colour, `SGR 58`.
const UNDERLINE_COLORS: &[Capability] = &[
    Capability::Color(ColorRole::Underline, ColorDepth::Basic),
    Capability::Color(ColorRole::Underline, ColorDepth::Palette),
    Capability::Color(ColorRole::Underline, ColorDepth::TrueColor),
];

/// The common modern set: everything except the awkward three.
const MODERN: &[Capability] = &[
    Capability::Dim,
    Capability::Italic,
    Capability::Strikethrough,
];

/// A row's capability groups, flattened at lookup time.
///
/// Storing groups rather than one flat list is what keeps the table legible: a row
/// says "the VT100 set, the modern set, all four shapes, truecolor" rather than
/// repeating nineteen entries.
type Groups = &'static [&'static [Capability]];

/// One terminal's entry, as groups.
struct Entry {
    terminal: &'static str,
    renders: Groups,
    unverified: Groups,
    since: Option<&'static str>,
}

/// The per-attribute table.
///
/// Rows are deliberately incomplete rather than optimistic. Where a primary source
/// was not to hand the capability is `unverified`, which the probe reports as
/// [`Support::Unknown`] — and where the terminal answers DECRQSS none of this is
/// consulted at all, because a measurement beats a table.
const ENTRIES: &[Entry] = &[
    // The four that implement essentially the whole family.
    Entry {
        terminal: "kitty",
        renders: &[VT100, MODERN, ALL_SHAPES, PALETTE_COLORS, TRUE_COLORS, UNDERLINE_COLORS],
        unverified: &[&[Capability::Blink, Capability::RapidBlink, Capability::Conceal]],
        since: None,
    },
    Entry {
        terminal: "wezterm",
        renders: &[VT100, MODERN, ALL_SHAPES, PALETTE_COLORS, TRUE_COLORS, UNDERLINE_COLORS],
        unverified: &[&[
            Capability::Blink,
            Capability::RapidBlink,
            Capability::Conceal,
            Capability::Overline
        ]],
        since: None,
    },
    Entry {
        terminal: "foot",
        renders: &[VT100, MODERN, ALL_SHAPES, PALETTE_COLORS, TRUE_COLORS, UNDERLINE_COLORS],
        unverified: &[&[Capability::Blink, Capability::RapidBlink, Capability::Conceal]],
        since: None,
    },
    Entry {
        terminal: "ghostty",
        renders: &[VT100, MODERN, ALL_SHAPES, PALETTE_COLORS, TRUE_COLORS, UNDERLINE_COLORS],
        unverified: &[&[
            Capability::Blink,
            Capability::RapidBlink,
            Capability::Conceal,
            Capability::Overline
        ]],
        since: None,
    },
    // VTE had curly underlines and SGR 58 years before most, and the two later
    // shapes are the ones to check.
    Entry {
        terminal: "gnome-terminal",
        renders: &[VT100, MODERN, EARLY_SHAPES, PALETTE_COLORS, TRUE_COLORS, UNDERLINE_COLORS],
        unverified: &[
            LATE_SHAPES,
            &[Capability::Blink, Capability::RapidBlink, Capability::Conceal]
        ],
        since: Some("0.52"),
    },
    Entry {
        terminal: "windows-terminal",
        renders: &[VT100, MODERN, ALL_SHAPES, PALETTE_COLORS, TRUE_COLORS, UNDERLINE_COLORS],
        unverified: &[&[
            Capability::Blink,
            Capability::RapidBlink,
            Capability::Conceal,
            Capability::Overline
        ]],
        since: None,
    },
    Entry {
        terminal: "vscode",
        renders: &[VT100, MODERN, ALL_SHAPES, PALETTE_COLORS, TRUE_COLORS, UNDERLINE_COLORS],
        unverified: &[&[
            Capability::Blink,
            Capability::RapidBlink,
            Capability::Conceal,
            Capability::Overline
        ]],
        since: None,
    },
    Entry {
        terminal: "alacritty",
        renders: &[VT100, MODERN, ALL_SHAPES, PALETTE_COLORS, TRUE_COLORS, UNDERLINE_COLORS],
        unverified: &[&[
            Capability::Blink,
            Capability::RapidBlink,
            Capability::Conceal,
            Capability::Overline
        ]],
        since: None,
    },
    Entry {
        terminal: "iterm2",
        renders: &[VT100, MODERN, EARLY_SHAPES, PALETTE_COLORS, TRUE_COLORS],
        unverified: &[
            LATE_SHAPES,
            UNDERLINE_COLORS,
            &[
                Capability::Blink,
                Capability::RapidBlink,
                Capability::Conceal,
                Capability::Overline
            ]
        ],
        since: None,
    },
    Entry {
        terminal: "konsole",
        renders: &[VT100, MODERN, PALETTE_COLORS, TRUE_COLORS],
        unverified: &[
            ALL_SHAPES,
            UNDERLINE_COLORS,
            &[
                Capability::Blink,
                Capability::RapidBlink,
                Capability::Conceal,
                Capability::Overline
            ]
        ],
        since: None,
    },
    Entry {
        terminal: "contour",
        renders: &[VT100, MODERN, PALETTE_COLORS, TRUE_COLORS],
        unverified: &[
            ALL_SHAPES,
            UNDERLINE_COLORS,
            &[
                Capability::Blink,
                Capability::RapidBlink,
                Capability::Conceal,
                Capability::Overline
            ]
        ],
        since: None,
    },
    Entry {
        terminal: "rio",
        renders: &[VT100, MODERN, PALETTE_COLORS, TRUE_COLORS],
        unverified: &[
            ALL_SHAPES,
            UNDERLINE_COLORS,
            &[
                Capability::Blink,
                Capability::RapidBlink,
                Capability::Conceal,
                Capability::Overline
            ]
        ],
        since: None,
    },
    // The interesting row, and the reason this feature is worth a page. xterm is the
    // reference implementation of nearly everything in terminal emulation, and it
    // implements *none* of the five underline shapes and no `SGR 58`: those were
    // originated by mintty. It also, in most builds, quantises 24-bit colour to the
    // 256-entry palette rather than rendering it — which is precisely what the
    // truecolor readback in `probe` was built to catch, so that is left unverified
    // here rather than asserted either way.
    Entry {
        terminal: "xterm",
        renders: &[VT100, MODERN, PALETTE_COLORS, &[Capability::Conceal]],
        unverified: &[TRUE_COLORS, &[Capability::Blink, Capability::Overline]],
        since: None,
    },
    // Multiplexers. Whether an attribute survives one depends on the *outer*
    // terminal's terminfo — `Smulx` and `Setulc` for the underline shapes and
    // colour — so from inside, most of this is not knowable, and saying so is the
    // only honest answer.
    Entry {
        terminal: "tmux",
        renders: &[VT100, MODERN, PALETTE_COLORS],
        unverified: &[
            ALL_SHAPES,
            TRUE_COLORS,
            UNDERLINE_COLORS,
            &[
                Capability::Blink,
                Capability::RapidBlink,
                Capability::Conceal,
                Capability::Overline
            ]
        ],
        since: None,
    },
    Entry {
        terminal: "zellij",
        renders: &[VT100, PALETTE_COLORS],
        unverified: &[
            ALL_SHAPES,
            TRUE_COLORS,
            UNDERLINE_COLORS,
            MODERN,
            &[
                Capability::Blink,
                Capability::RapidBlink,
                Capability::Conceal,
                Capability::Overline
            ]
        ],
        since: None,
    },
    Entry {
        terminal: "screen",
        renders: &[VT100, PALETTE_COLORS],
        unverified: &[
            TRUE_COLORS,
            MODERN,
            &[Capability::Blink, Capability::RapidBlink, Capability::Conceal]
        ],
        since: None,
    },
];

/// Every capability a terminal renders, flattened.
#[must_use]
pub fn renders(terminal: &str) -> Vec<Capability> {
    flatten(terminal, |entry| entry.renders)
}

/// Every capability nobody has checked on a terminal, flattened.
#[must_use]
pub fn unverified(terminal: &str) -> Vec<Capability> {
    flatten(terminal, |entry| entry.unverified)
}

/// Flattens one of an entry's group lists.
fn flatten(terminal: &str, pick: fn(&Entry) -> Groups) -> Vec<Capability> {
    ENTRIES
        .iter()
        .find(|entry| entry.terminal == terminal)
        .map(|entry| pick(entry).iter().flat_map(|group| group.iter().copied()).collect())
        .unwrap_or_default()
}

/// Whether `terminal` is in the table at all.
#[must_use]
pub fn known(terminal: &str) -> bool {
    ENTRIES.iter().any(|entry| entry.terminal == terminal)
}

/// The version the underline shapes arrived in for `terminal`, if the table records
/// one.
#[must_use]
pub fn since(terminal: &str) -> Option<&'static str> {
    ENTRIES
        .iter()
        .find(|entry| entry.terminal == terminal)
        .and_then(|entry| entry.since)
}

/// Whether `terminal` renders `attr`, as a three-state answer.
///
/// A terminal that is not in the table is [`Support::Unknown`] rather than
/// unsupported: absence from a hand-maintained list is not evidence about a terminal,
/// and a new terminal is far more likely to implement all of this than none of it.
#[must_use]
pub fn support(terminal: &str, attr: Attr) -> Support {
    // The three VT100 attributes need no table: they have been in every terminal
    // since 1978, and a table entry could only ever get them wrong.
    if attr.is_universal() {
        return Support::Yes;
    }
    if !known(terminal) {
        return Support::Unknown;
    }
    let capability = capability_of(attr);
    if renders(terminal).contains(&capability) {
        Support::Yes
    } else if unverified(terminal).contains(&capability) {
        Support::Unknown
    } else {
        Support::No
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{
        db::Catalog,
        features::style::{
            FEATURE_ID,
            attrs::{PROBE_BASIC, PROBE_PALETTE, PROBE_RGB, every_attribute},
        },
    };

    #[test]
    fn every_row_names_a_terminal_in_the_registry() {
        let catalog = Catalog::load().unwrap();
        for entry in ENTRIES {
            assert!(
                catalog.terminal(entry.terminal).is_some(),
                "{} is not in terminals.toml",
                entry.terminal
            );
        }
    }

    #[test]
    fn no_terminal_appears_twice() {
        let mut ids: Vec<&str> = ENTRIES.iter().map(|entry| entry.terminal).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "a terminal has two rows");
    }

    #[test]
    fn no_row_claims_a_capability_twice() {
        for entry in ENTRIES {
            let renders = renders(entry.terminal);
            for capability in unverified(entry.terminal) {
                assert!(
                    !renders.contains(&capability),
                    "{}: {capability:?} is both rendered and unverified",
                    entry.terminal
                );
            }
            // And no group repeats a capability within itself.
            let mut seen = Vec::new();
            for capability in renders {
                assert!(
                    !seen.contains(&capability),
                    "{}: {capability:?} listed twice",
                    entry.terminal
                );
                seen.push(capability);
            }
        }
    }

    #[test]
    fn the_vt100_attributes_are_supported_everywhere_including_nowhere() {
        for terminal in ["kitty", "xterm", "no-such-terminal", ""] {
            for attr in [
                Attr::Bold,
                Attr::Reverse,
                Attr::Underline(UnderlineShape::Single),
                Attr::Underline(UnderlineShape::None),
            ] {
                assert_eq!(
                    support(terminal, attr),
                    Support::Yes,
                    "{terminal} / {}",
                    attr.label()
                );
            }
        }
    }

    #[test]
    fn an_unknown_terminal_is_unknown_and_never_unsupported() {
        // Absence from a hand-maintained list says nothing about a terminal, and a
        // new one is far likelier to implement all of this than none of it.
        for attr in every_attribute() {
            let found = support("no-such-terminal", attr);
            assert_ne!(found, Support::No, "{}", attr.label());
        }
        assert!(!known("no-such-terminal"));
        assert!(renders("no-such-terminal").is_empty());
        assert!(unverified("no-such-terminal").is_empty());
    }

    #[test]
    fn xterm_is_a_definite_no_for_every_underline_shape_and_for_sgr_58() {
        // The headline fact of this feature: the reference implementation of
        // terminal emulation implements none of the five shapes, which mintty
        // originated, and no underline colour.
        for shape in UnderlineShape::EXTENDED {
            assert_eq!(
                support("xterm", Attr::Underline(*shape)),
                Support::No,
                "{}",
                shape.name()
            );
        }
        for color in [PROBE_BASIC, PROBE_PALETTE, PROBE_RGB] {
            assert_eq!(
                support("xterm", Attr::Color(ColorRole::Underline, color)),
                Support::No,
                "{}",
                color.describe()
            );
        }
        // …while a plain underline is still a yes, because it always is.
        assert_eq!(
            support("xterm", Attr::Underline(UnderlineShape::Single)),
            Support::Yes
        );
        assert_eq!(support("xterm", Attr::Italic), Support::Yes);
    }

    #[test]
    fn xterm_truecolor_is_left_unverified_rather_than_asserted() {
        // Most xterm builds quantise 24-bit colour to the 256-entry palette, and
        // whether this one does depends on how it was compiled. That is exactly what
        // the readback in `probe` measures, so the table declines to guess.
        assert_eq!(
            support("xterm", Attr::Color(ColorRole::Foreground, PROBE_RGB)),
            Support::Unknown
        );
        assert_eq!(
            support("xterm", Attr::Color(ColorRole::Foreground, PROBE_PALETTE)),
            Support::Yes
        );
    }

    #[test]
    fn a_partially_checked_terminal_answers_attribute_by_attribute() {
        // The whole reason this table is finer-grained than the support matrix.
        assert_eq!(
            support("gnome-terminal", Attr::Underline(UnderlineShape::Curly)),
            Support::Yes
        );
        assert_eq!(
            support("gnome-terminal", Attr::Underline(UnderlineShape::Dotted)),
            Support::Unknown
        );
        assert_eq!(support("gnome-terminal", Attr::Italic), Support::Yes);
    }

    #[test]
    fn only_vte_is_version_gated_because_only_the_shapes_are_young_enough_to_gate() {
        assert_eq!(since("gnome-terminal"), Some("0.52"));
        assert_eq!(since("kitty"), None);
        assert_eq!(since("no-such-terminal"), None);
    }

    #[test]
    fn every_attribute_maps_to_exactly_one_capability() {
        for attr in every_attribute() {
            let capability = capability_of(attr);
            // The colour value must not reach the key: only role and depth do.
            if let Attr::Color(role, color) = attr {
                assert_eq!(capability, Capability::Color(role, color.depth()));
            }
            // And the mapping is total: every attribute answers for every terminal.
            for entry in ENTRIES {
                let _ = support(entry.terminal, attr);
            }
        }
        // Two colours of the same role and depth are the same capability.
        assert_eq!(
            capability_of(Attr::Color(ColorRole::Foreground, PROBE_RGB)),
            capability_of(Attr::Color(
                ColorRole::Foreground,
                crate::features::style::color::Color::Rgb(9, 9, 9)
            ))
        );
    }

    #[test]
    fn every_row_has_a_matching_support_row_in_the_data_file() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature(FEATURE_ID).unwrap();
        // Skipped while the data file is still a placeholder with no support rows;
        // once it has them, every per-attribute row must be represented.
        if feature.support.is_empty() {
            return;
        }
        for entry in ENTRIES {
            assert!(
                feature
                    .support
                    .iter()
                    .any(|support| support.terminal == entry.terminal),
                "{} has a per-attribute row but no row in text-attributes.toml",
                entry.terminal
            );
        }
    }
}
