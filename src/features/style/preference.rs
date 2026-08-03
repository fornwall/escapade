//! `NO_COLOR`, read once and passed around rather than looked up in the middle of
//! things.
//!
//! <https://no-color.org> is a one-paragraph specification, and the paragraph
//! matters: software should check for a `NO_COLOR` environment variable that,
//! **when present and not an empty string, regardless of its value**, prevents the
//! addition of colour. So the test is presence and non-emptiness, not truthiness —
//! `NO_COLOR=0` means *no colour*, and `NO_COLOR=` (empty) means nothing at all.
//! Both of those are widely got wrong, which is why they have tests here.
//!
//! The variable is about **colour**, not about styling in general, and this module is
//! named for that distinction. `escapade style --bold --underline=curly --color red`
//! under `NO_COLOR` emits the bold and the curly underline and drops the red: the
//! shape of a spell-checker underline is not a colour, and suppressing it would be
//! obeying a rule nobody wrote.
//!
//! # What it means for `probe`
//!
//! `escapade style probe --color '#ff0000'` has two defensible readings, and the
//! choice is deliberate:
//!
//! * *capability* — "can this terminal render 24-bit colour?", which a user
//!   preference plainly does not affect;
//! * *should-I* — "should I emit 24-bit colour here?", which it decides outright.
//!
//! The second is what makes the command useful, because the script running it is
//! asking whether to colour its own output:
//!
//! ```sh
//! if escapade style probe --color '#ff0000'; then use_color; fi
//! ```
//!
//! A probe that ignored `NO_COLOR` would turn every such script into one that
//! politely disregards the user, and the entire value of the variable is that
//! setting it once works everywhere. So the preference decides the **verdict**.
//!
//! What is *not* acceptable is losing the distinction, so the capability is still
//! measured and reported alongside. That combination — "your terminal does 24-bit
//! colour perfectly well; your own environment is switching it off" — is the answer
//! for the person this most often bites: somebody with a stray `NO_COLOR=1` in a
//! shell profile who cannot work out where their colours went.

use std::env;

/// The name of the variable, in one place.
pub const NO_COLOR: &str = "NO_COLOR";

/// Whether the user has asked for no colour, and what they said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColorPreference {
    /// The value of `NO_COLOR`, when it is set to something non-empty.
    ///
    /// `None` covers both "unset" and "set to the empty string", which the
    /// specification treats identically.
    suppressed: Option<String>,
}

impl ColorPreference {
    /// Reads the preference from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_value(env::var(NO_COLOR).ok().as_deref())
    }

    /// Builds a preference from a value that may or may not be set.
    ///
    /// The whole decision lives here, taking the value as an argument, so every case
    /// the specification distinguishes can be tested without touching the real
    /// environment — and so that no code path can accidentally consult the
    /// environment a second time and disagree with the first.
    #[must_use]
    pub fn from_value(value: Option<&str>) -> Self {
        Self {
            // Present *and* non-empty. Not parsed, not compared against "0" or
            // "false": the specification says "regardless of its value".
            suppressed: value.filter(|value| !value.is_empty()).map(str::to_owned),
        }
    }

    /// A preference that suppresses nothing, for tests and for `--no-color`'s
    /// opposite.
    #[must_use]
    pub fn allowed() -> Self {
        Self { suppressed: None }
    }

    /// Whether colour should be left out.
    #[must_use]
    pub fn suppresses_color(&self) -> bool {
        self.suppressed.is_some()
    }

    /// The value the user set, for quoting back at them.
    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.suppressed.as_deref()
    }

    /// The sentence to show when this is what decided an answer.
    ///
    /// Names the variable, quotes the value, and says out loud that it came from the
    /// environment and not from the terminal — because the reader who most needs
    /// this message is the one who thinks their terminal has broken.
    #[must_use]
    pub fn explanation(&self) -> Option<String> {
        let value = self.suppressed.as_deref()?;
        Some(format!(
            "colour suppressed: {NO_COLOR} is set (value: `{value}`) in your environment — this is \
             your setting, not a limitation of your terminal. Unset it to allow colour: \
             `unset {NO_COLOR}`"
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn unset_allows_colour() {
        let preference = ColorPreference::from_value(None);
        assert!(!preference.suppresses_color());
        assert_eq!(preference.value(), None);
        assert_eq!(preference.explanation(), None);
        assert_eq!(preference, ColorPreference::allowed());
        assert_eq!(preference, ColorPreference::default());
    }

    #[test]
    fn any_non_empty_value_suppresses_colour_including_zero() {
        // The case implementations get wrong: the test is presence, not truthiness.
        for value in ["1", "0", "false", "no", "true", "yes", " ", "off"] {
            let preference = ColorPreference::from_value(Some(value));
            assert!(
                preference.suppresses_color(),
                "NO_COLOR={value:?} must suppress colour"
            );
            assert_eq!(preference.value(), Some(value));
        }
    }

    #[test]
    fn the_empty_string_does_not_suppress_colour() {
        // Explicitly carved out by the specification, and the other case
        // implementations get wrong.
        let preference = ColorPreference::from_value(Some(""));
        assert!(!preference.suppresses_color());
        assert_eq!(preference.value(), None);
        assert_eq!(preference.explanation(), None);
    }

    #[test]
    fn the_explanation_names_the_variable_and_quotes_the_value() {
        // Written for one specific reader: somebody with a stray NO_COLOR=1 who
        // thinks their terminal has broken.
        let explanation = ColorPreference::from_value(Some("1"))
            .explanation()
            .unwrap();
        assert!(explanation.contains("NO_COLOR"), "{explanation}");
        assert!(explanation.contains('1'), "{explanation}");
        assert!(explanation.contains("your environment"), "{explanation}");
        assert!(
            explanation.contains("not a limitation of your terminal"),
            "{explanation}"
        );
        assert!(explanation.contains("unset"), "{explanation}");
    }

    #[test]
    fn a_hostile_value_cannot_smuggle_a_sequence_into_the_explanation() {
        // The value is quoted back at the user, so it is an injection path. It is
        // sanitised at the point of printing; this checks the value survives as
        // data rather than being interpreted here.
        let preference = ColorPreference::from_value(Some("\u{1b}[2J"));
        assert!(preference.suppresses_color());
        assert_eq!(preference.value(), Some("\u{1b}[2J"));
    }

    #[test]
    fn reading_the_real_environment_agrees_with_reading_the_value() {
        // No environment mutation: this only checks the two entry points cannot
        // disagree about whatever is actually set right now.
        let from_env = ColorPreference::from_env();
        let expected = ColorPreference::from_value(env::var(NO_COLOR).ok().as_deref());
        assert_eq!(from_env, expected);
    }
}
