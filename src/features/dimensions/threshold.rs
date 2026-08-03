//! Threshold tests: the arithmetic behind `escapade dimensions probe --rows-at-least 24`.
//!
//! Three rules, and the third is the whole point of the module:
//!
//! 1. **Thresholds are inclusive.** `--rows-at-least 20` passes at 20, and
//!    `--rows-at-most 20` passes at 20. That is what the flag names say, and a
//!    threshold test almost always means "I need at least this much room".
//! 2. **Several thresholds mean all of them must hold.**
//! 3. **A dimension that was asked about but could not be determined is
//!    [`Support::Unknown`], not a failed threshold.** Pixel sizes are unavailable
//!    on a great many terminals, and answering "no, your window is too small"
//!    when the truth is "nobody would say how big your window is" would be a
//!    guess dressed up as a measurement.
//!
//! A definite failure still wins over an unknown, because it is a conclusion: if
//! one conjunct of an `and` is false the whole thing is false, whatever the
//! others turn out to be. So `--cols-at-most 10 --width-at-least 100` on an
//! 80-column terminal with no pixel size is a solid *no*, while
//! `--cols-at-least 10 --width-at-least 100` on the same terminal is unknown.
//!
//! Everything here takes numbers, so all of it is tested without a terminal.

use anyhow::{Result, bail};

use crate::features::{Support, dimensions::size::Sizes};

/// The bounds asked for on one dimension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bounds {
    /// `--<dimension>-at-least`: pass at this value or above.
    pub at_least: Option<u32>,
    /// `--<dimension>-at-most`: pass at this value or below.
    pub at_most: Option<u32>,
    /// `--<dimension>-exactly`: pass at this value only.
    pub exactly: Option<u32>,
}

impl Bounds {
    /// Whether nothing was asked about this dimension.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.at_least.is_none() && self.at_most.is_none() && self.exactly.is_none()
    }

    /// Whether `value` satisfies every bound given. Inclusive at both ends.
    #[must_use]
    pub fn holds(self, value: u32) -> bool {
        self.at_least.is_none_or(|least| value >= least)
            && self.at_most.is_none_or(|most| value <= most)
            && self.exactly.is_none_or(|exact| value == exact)
    }

    /// The pair of flags that cannot both hold, if this dimension asks for
    /// something impossible.
    ///
    /// Only `at_least`/`at_most` inversions and an `exactly` outside them are
    /// possible; there is one of each flag, so nothing else can contradict.
    #[must_use]
    pub fn contradiction(self) -> Option<(String, String)> {
        let least = self.at_least;
        let most = self.at_most;
        if let (Some(least), Some(most)) = (least, most)
            && least > most
        {
            return Some((format!("at-least {least}"), format!("at-most {most}")));
        }
        if let (Some(exact), Some(least)) = (self.exactly, least)
            && exact < least
        {
            return Some((format!("exactly {exact}"), format!("at-least {least}")));
        }
        if let (Some(exact), Some(most)) = (self.exactly, most)
            && exact > most
        {
            return Some((format!("exactly {exact}"), format!("at-most {most}")));
        }
        None
    }
}

/// The four numbers a threshold can be tested against, each of which may be
/// unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Available {
    /// Rows, in cells.
    pub rows: Option<u32>,
    /// Columns, in cells.
    pub cols: Option<u32>,
    /// Window width, in pixels.
    pub width: Option<u32>,
    /// Window height, in pixels.
    pub height: Option<u32>,
}

impl Available {
    /// Picks the four testable numbers out of everything that was gathered.
    #[must_use]
    pub fn from_sizes(sizes: &Sizes) -> Self {
        let cells = sizes.cells();
        let window = sizes.window();
        Self {
            rows: cells.map(|cells| cells.vertical),
            cols: cells.map(|cells| cells.horizontal),
            width: window.map(|window| window.horizontal),
            height: window.map(|window| window.vertical),
        }
    }
}

/// Every threshold flag, grouped by dimension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Thresholds {
    /// `--rows-*`.
    pub rows: Bounds,
    /// `--cols-*`.
    pub cols: Bounds,
    /// `--width-*`, the window's pixel width.
    pub width: Bounds,
    /// `--height-*`, the window's pixel height.
    pub height: Bounds,
}

impl Thresholds {
    /// Whether no threshold was given at all, which turns `probe` into the
    /// general question of whether the size can be determined.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.rows.is_empty()
            && self.cols.is_empty()
            && self.width.is_empty()
            && self.height.is_empty()
    }

    /// Whether any threshold needs a pixel size, and therefore a round trip on
    /// the terminals whose `TIOCGWINSZ` leaves the pixel fields zero.
    #[must_use]
    pub fn wants_pixels(self) -> bool {
        !self.width.is_empty() || !self.height.is_empty()
    }

    /// The four dimensions, with their names, in reporting order.
    #[must_use]
    pub fn axes(self) -> [(&'static str, Bounds); 4] {
        [
            ("rows", self.rows),
            ("cols", self.cols),
            ("width", self.width),
            ("height", self.height),
        ]
    }

    /// Rejects a request no terminal could ever satisfy.
    ///
    /// `--rows-at-least 30 --rows-at-most 10` is a **usage error, not exit 1**.
    /// Exit 1 means "your terminal does not meet this requirement", and a script
    /// author who reads that from a contradiction will go and resize their window
    /// forever. The contradiction is a statement about the request, not a
    /// measurement of the terminal, and this tool does not dress mistakes up as
    /// findings. It is also the only failure mode here that a person can fix by
    /// editing their command line, which is exactly what stderr is for.
    ///
    /// # Errors
    ///
    /// Fails when a dimension's bounds exclude every possible value, naming both
    /// flags.
    pub fn check(self) -> Result<()> {
        for (name, bounds) in self.axes() {
            if let Some((left, right)) = bounds.contradiction() {
                bail!(
                    "--{name}-{left} and --{name}-{right} can never both hold, \
                     so this test could only ever fail — did you mean one of them?"
                );
            }
        }
        Ok(())
    }

    /// Tests every threshold given against what could be determined.
    #[must_use]
    pub fn evaluate(self, available: &Available) -> Outcome {
        let values = [
            available.rows,
            available.cols,
            available.width,
            available.height,
        ];
        let mut outcome = Outcome::default();
        for ((name, bounds), value) in self.axes().into_iter().zip(values) {
            if bounds.is_empty() {
                continue;
            }
            match value {
                // A dimension nobody could measure is not a dimension that
                // failed. This is the distinction the whole feature turns on.
                None => outcome.undetermined.push(name),
                Some(value) if !bounds.holds(value) => outcome.unmet.push(name),
                Some(_) => {}
            }
        }
        outcome
    }
}

/// What a threshold test concluded, and which dimensions decided it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Dimensions that were determined and did not satisfy their bounds.
    pub unmet: Vec<&'static str>,
    /// Dimensions that were asked about and could not be determined.
    pub undetermined: Vec<&'static str>,
}

impl Outcome {
    /// The three-state answer, and so the exit status.
    ///
    /// A definite failure outranks an unknown: a conjunction with one false
    /// member is false however the rest turn out.
    #[must_use]
    pub fn support(&self) -> Support {
        if !self.unmet.is_empty() {
            Support::No
        } else if !self.undetermined.is_empty() {
            Support::Unknown
        } else {
            Support::Yes
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// An 80x24 terminal, 800x480 pixels — everything known.
    fn known() -> Available {
        Available {
            rows: Some(24),
            cols: Some(80),
            width: Some(800),
            height: Some(480),
        }
    }

    /// The common terminal: cells known, pixels not.
    fn cells_only() -> Available {
        Available {
            rows: Some(24),
            cols: Some(80),
            ..Available::default()
        }
    }

    fn rows(bounds: Bounds) -> Thresholds {
        Thresholds {
            rows: bounds,
            ..Thresholds::default()
        }
    }

    fn support(thresholds: Thresholds, available: &Available) -> Support {
        thresholds.evaluate(available).support()
    }

    #[test]
    fn at_least_is_inclusive_at_its_boundary() {
        let at_least = |n| {
            rows(Bounds {
                at_least: Some(n),
                ..Bounds::default()
            })
        };
        // 24 rows: exactly at, one below, one above.
        assert_eq!(support(at_least(24), &known()), Support::Yes, "24 >= 24");
        assert_eq!(support(at_least(23), &known()), Support::Yes);
        assert_eq!(support(at_least(25), &known()), Support::No);
    }

    #[test]
    fn at_most_is_inclusive_at_its_boundary() {
        let at_most = |n| {
            rows(Bounds {
                at_most: Some(n),
                ..Bounds::default()
            })
        };
        assert_eq!(support(at_most(24), &known()), Support::Yes, "24 <= 24");
        assert_eq!(support(at_most(25), &known()), Support::Yes);
        assert_eq!(support(at_most(23), &known()), Support::No);
    }

    #[test]
    fn exactly_matches_only_the_number_given() {
        let exactly = |n| {
            rows(Bounds {
                exactly: Some(n),
                ..Bounds::default()
            })
        };
        assert_eq!(support(exactly(24), &known()), Support::Yes);
        assert_eq!(support(exactly(23), &known()), Support::No);
        assert_eq!(support(exactly(25), &known()), Support::No);
    }

    #[test]
    fn every_dimension_is_inclusive_the_same_way() {
        // The boundary case for all twelve flags at once, so a dimension wired
        // up with a `>` instead of a `>=` cannot hide.
        let at = |value| Bounds {
            at_least: Some(value),
            ..Bounds::default()
        };
        let up_to = |value| Bounds {
            at_most: Some(value),
            ..Bounds::default()
        };
        let exact = |value| Bounds {
            exactly: Some(value),
            ..Bounds::default()
        };
        for bounds in [at, up_to, exact] {
            let thresholds = Thresholds {
                rows: bounds(24),
                cols: bounds(80),
                width: bounds(800),
                height: bounds(480),
            };
            assert_eq!(
                support(thresholds, &known()),
                Support::Yes,
                "every flag passes at its own boundary: {thresholds:?}"
            );
        }
    }

    #[test]
    fn several_thresholds_all_have_to_hold() {
        let thresholds = Thresholds {
            rows: Bounds {
                at_least: Some(20),
                ..Bounds::default()
            },
            cols: Bounds {
                at_most: Some(40),
                ..Bounds::default()
            },
            ..Thresholds::default()
        };
        // 24 rows passes, 80 columns does not.
        let outcome = thresholds.evaluate(&known());
        assert_eq!(outcome.support(), Support::No);
        assert_eq!(outcome.unmet, ["cols"]);

        let narrow = Available {
            cols: Some(40),
            ..known()
        };
        assert_eq!(support(thresholds, &narrow), Support::Yes, "40 <= 40");
    }

    #[test]
    fn at_least_and_at_most_together_are_a_range() {
        let thresholds = rows(Bounds {
            at_least: Some(20),
            at_most: Some(24),
            exactly: None,
        });
        assert_eq!(
            support(thresholds, &known()),
            Support::Yes,
            "24 is in 20..=24"
        );
        for rows_count in [19, 25] {
            let available = Available {
                rows: Some(rows_count),
                ..known()
            };
            assert_eq!(support(thresholds, &available), Support::No);
        }
    }

    #[test]
    fn an_undeterminable_dimension_is_unknown_and_never_no() {
        // The behaviour that matters most: pixel size is unavailable on many
        // terminals, and "no" would be a guess.
        let thresholds = Thresholds {
            width: Bounds {
                at_least: Some(100),
                ..Bounds::default()
            },
            ..Thresholds::default()
        };
        let outcome = thresholds.evaluate(&cells_only());
        assert_eq!(outcome.support(), Support::Unknown);
        assert_eq!(outcome.undetermined, ["width"]);
        assert!(outcome.unmet.is_empty());
        assert_eq!(
            format!("{:?}", outcome.support().exit_code()),
            format!("{:?}", std::process::ExitCode::from(2)),
            "and it exits 2, not 1"
        );
    }

    #[test]
    fn an_unknown_dimension_nobody_asked_about_changes_nothing() {
        let thresholds = rows(Bounds {
            at_least: Some(20),
            ..Bounds::default()
        });
        assert_eq!(
            support(thresholds, &cells_only()),
            Support::Yes,
            "the missing pixel size was not asked about"
        );
    }

    #[test]
    fn a_definite_failure_outranks_an_unknown() {
        // `cols <= 10` is false on an 80-column terminal whatever the pixel size
        // turns out to be, so the conjunction is provably false.
        let thresholds = Thresholds {
            cols: Bounds {
                at_most: Some(10),
                ..Bounds::default()
            },
            width: Bounds {
                at_least: Some(100),
                ..Bounds::default()
            },
            ..Thresholds::default()
        };
        let outcome = thresholds.evaluate(&cells_only());
        assert_eq!(outcome.support(), Support::No);
        assert_eq!(outcome.unmet, ["cols"]);
        assert_eq!(
            outcome.undetermined,
            ["width"],
            "but the gap is still reported"
        );
    }

    #[test]
    fn nothing_known_and_something_asked_is_unknown() {
        let thresholds = rows(Bounds {
            at_least: Some(1),
            ..Bounds::default()
        });
        let outcome = thresholds.evaluate(&Available::default());
        assert_eq!(outcome.support(), Support::Unknown);
        assert_eq!(outcome.undetermined, ["rows"]);
    }

    #[test]
    fn no_thresholds_at_all_is_vacuously_true() {
        let outcome = Thresholds::default().evaluate(&Available::default());
        assert_eq!(outcome.support(), Support::Yes);
        assert!(Thresholds::default().is_empty());
    }

    #[test]
    fn contradictory_bounds_are_a_usage_error_rather_than_a_failed_test() {
        let thresholds = rows(Bounds {
            at_least: Some(30),
            at_most: Some(10),
            exactly: None,
        });
        let error = thresholds.check().unwrap_err().to_string();
        assert!(error.contains("--rows-at-least 30"), "{error}");
        assert!(error.contains("--rows-at-most 10"), "{error}");
        // Had it been allowed through it would have exited 1, which reads as
        // "your terminal is too small" — a misdiagnosis of a typo.
        assert_eq!(support(thresholds, &known()), Support::No);
    }

    #[test]
    fn exactly_outside_a_range_is_contradictory_too() {
        for bounds in [
            Bounds {
                exactly: Some(10),
                at_least: Some(30),
                at_most: None,
            },
            Bounds {
                exactly: Some(30),
                at_least: None,
                at_most: Some(10),
            },
        ] {
            assert!(rows(bounds).check().is_err(), "{bounds:?}");
        }
    }

    #[test]
    fn a_satisfiable_request_passes_the_check() {
        for bounds in [
            Bounds {
                at_least: Some(20),
                at_most: Some(20),
                exactly: None,
            },
            Bounds {
                at_least: Some(20),
                at_most: Some(24),
                exactly: Some(22),
            },
            Bounds {
                exactly: Some(20),
                at_least: Some(20),
                at_most: Some(20),
            },
        ] {
            assert!(
                rows(bounds).check().is_ok(),
                "{bounds:?} is satisfiable — at-least and at-most are inclusive"
            );
        }
        assert!(Thresholds::default().check().is_ok());
    }

    #[test]
    fn only_pixel_thresholds_ask_for_a_round_trip() {
        assert!(!Thresholds::default().wants_pixels());
        assert!(
            !rows(Bounds {
                at_least: Some(1),
                ..Bounds::default()
            })
            .wants_pixels()
        );
        let pixels = Thresholds {
            height: Bounds {
                at_most: Some(1000),
                ..Bounds::default()
            },
            ..Thresholds::default()
        };
        assert!(pixels.wants_pixels());
        assert!(!pixels.is_empty());
    }

    #[test]
    fn available_reads_the_chain_rather_than_the_raw_sources() {
        use crate::features::dimensions::size::Sizes;

        let sizes = Sizes {
            ioctl_cells: Some((80, 24)),
            reported_window: Some((800, 480)),
            ..Sizes::default()
        };
        let available = Available::from_sizes(&sizes);
        assert_eq!(available.cols, Some(80));
        assert_eq!(available.rows, Some(24));
        assert_eq!(available.width, Some(800));
        assert_eq!(available.height, Some(480));

        let nothing = Available::from_sizes(&Sizes::default());
        assert_eq!(nothing, Available::default());
    }
}
