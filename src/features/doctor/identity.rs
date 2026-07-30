//! Normalising the many names a terminal goes by into one family token.
//!
//! The identity check compares four strings that all claim to name the same
//! program: the terminfo entry's name, the `TN` capability the terminal reports,
//! `TERM_PROGRAM` or `TERM` from the environment, and the text XTVERSION returns.
//! They are spelled differently by every source — `xterm-kitty`, `kitty 0.35.2`,
//! `iTerm.app`, `iTerm2 3.5.0` — so comparing them literally would report a
//! disagreement on every terminal in existence.
//!
//! Matching is by keyword against an ordered table, and the order is load-bearing:
//! `xterm-kitty` and `xterm-ghostty` both contain `xterm`, and `screen.xterm-256color`
//! is Screen rather than xterm. The specific families come first and `xterm` is last.
//!
//! An unrecognised name yields `None` rather than a guess. That is deliberate: a
//! source whose family cannot be worked out has said nothing comparable, and
//! inventing a mismatch out of ignorance would be the same overclaiming this
//! subcommand exists to expose.

/// Keyword to family, most specific first.
///
/// Short keywords that could appear inside an unrelated word are matched at a
/// token boundary instead of anywhere; see [`contains_token`].
const FAMILIES: &[(&str, &str)] = &[
    // Multiplexers first: `screen.xterm-256color` and `tmux-256color` name the
    // multiplexer, and inside one the multiplexer *is* the terminal as far as
    // escape sequences are concerned.
    ("tmux", "tmux"),
    ("screen", "screen"),
    ("zellij", "zellij"),
    // Then everything whose name embeds another terminal's.
    ("kitty", "kitty"),
    ("ghostty", "ghostty"),
    ("wezterm", "wezterm"),
    ("iterm", "iterm2"),
    ("apple_terminal", "terminal-app"),
    ("terminal.app", "terminal-app"),
    ("windows terminal", "windows-terminal"),
    ("windowsterminal", "windows-terminal"),
    ("konsole", "konsole"),
    ("gnome-terminal", "vte"),
    ("xfce4-terminal", "vte"),
    ("vte", "vte"),
    ("alacritty", "alacritty"),
    ("contour", "contour"),
    ("foot", "foot"),
    ("mintty", "mintty"),
    ("mlterm", "mlterm"),
    ("rxvt", "rxvt"),
    ("urxvt", "rxvt"),
    ("putty", "putty"),
    ("warp", "warp"),
    ("vscode", "vscode"),
    ("hyper", "hyper"),
    ("linux", "linux-console"),
    ("rio", "rio"),
    ("st", "st"),
    // Last, so that every terminal pretending to be xterm is recognised as
    // itself first. This entry is the reason the table is ordered at all.
    ("xterm", "xterm"),
];

/// Keywords short or common enough that a substring match would misfire.
const BOUNDED: &[&str] = &["st", "rio", "vte", "warp", "linux", "hyper"];

/// The family a name belongs to, or `None` when it is not recognised.
///
/// Matching is case-insensitive, and the input may be any of the spellings the
/// four sources use — a terminfo entry name, a `TERM_PROGRAM` value, or the free
/// text XTVERSION returns.
#[must_use]
pub fn family(name: &str) -> Option<&'static str> {
    let haystack = name.trim().to_ascii_lowercase();
    if haystack.is_empty() {
        return None;
    }
    FAMILIES
        .iter()
        .find(|(keyword, _)| {
            if BOUNDED.contains(keyword) {
                contains_token(&haystack, keyword)
            } else {
                haystack.contains(keyword)
            }
        })
        .map(|&(_, family)| family)
}

/// Whether `haystack` contains `token` delimited by something that is not a
/// letter or a digit.
///
/// `st` must match `st-256color` and `st` but not `ghostty` or `wezterm`; `rio`
/// must match `rio` but not a version string that happens to contain the letters.
fn contains_token(haystack: &str, token: &str) -> bool {
    let bytes = haystack.as_bytes();
    let token_bytes = token.as_bytes();
    let alnum = |byte: u8| byte.is_ascii_alphanumeric();
    haystack
        .match_indices(token)
        .any(|(index, _)| {
            let before_ok = index == 0 || !alnum(bytes[index - 1]);
            let after = index + token_bytes.len();
            let after_ok = after == bytes.len() || !alnum(bytes[after]);
            before_ok && after_ok
        })
}

/// Strips the `> |` introducer from an XTVERSION reply payload.
///
/// XTVERSION (`CSI > 0 q`) is answered `DCS > | <name and version> ST`, so the
/// payload [`crate::input::Reply::Dcs`] delivers is `>|xterm(400)`. A terminal
/// that answers without the introducer is accepted as-is rather than discarded.
#[must_use]
pub fn strip_xtversion(payload: &str) -> Option<&str> {
    let text = payload.strip_prefix(">|").unwrap_or(payload);
    let text = text.trim();
    if text.is_empty() { None } else { Some(text) }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn terminals_pretending_to_be_xterm_are_recognised_as_themselves() {
        // This is the whole reason the table is ordered.
        assert_eq!(family("xterm-kitty"), Some("kitty"));
        assert_eq!(family("xterm-ghostty"), Some("ghostty"));
        assert_eq!(family("xterm-256color"), Some("xterm"));
        assert_eq!(family("xterm"), Some("xterm"));
    }

    #[test]
    fn a_multiplexer_wins_over_the_terminal_it_names() {
        // `screen.xterm-256color` is Screen running inside xterm, and inside a
        // multiplexer the multiplexer is the terminal.
        assert_eq!(family("screen.xterm-256color"), Some("screen"));
        assert_eq!(family("tmux-256color"), Some("tmux"));
        assert_eq!(family("screen-256color"), Some("screen"));
    }

    #[test]
    fn every_source_spelling_of_one_terminal_lands_on_one_family() {
        for spelling in ["iTerm.app", "iTerm2 3.5.0", "iterm2"] {
            assert_eq!(family(spelling), Some("iterm2"), "{spelling}");
        }
        for spelling in ["xterm-kitty", "kitty 0.35.2", "KITTY"] {
            assert_eq!(family(spelling), Some("kitty"), "{spelling}");
        }
        for spelling in ["WezTerm", "wezterm 20240203-110809", "wezterm"] {
            assert_eq!(family(spelling), Some("wezterm"), "{spelling}");
        }
        for spelling in ["foot", "foot(1.16.2)", "foot-extra"] {
            assert_eq!(family(spelling), Some("foot"), "{spelling}");
        }
        for spelling in ["Apple_Terminal", "Terminal.app"] {
            assert_eq!(family(spelling), Some("terminal-app"), "{spelling}");
        }
    }

    #[test]
    fn a_short_keyword_does_not_match_inside_another_word() {
        // `st` inside `ghostty` and `wezterm` was the bug this guards.
        assert_eq!(family("st-256color"), Some("st"));
        assert_eq!(family("st"), Some("st"));
        assert_eq!(family("ghostty 1.0.0"), Some("ghostty"));
        assert_eq!(family("wezterm"), Some("wezterm"));
        // `vte` must not match `favorite`.
        assert_eq!(family("favorite-term"), None);
    }

    #[test]
    fn an_unrecognised_name_is_not_guessed_at() {
        assert_eq!(family("some-new-terminal-2027"), None);
        assert_eq!(family(""), None);
        assert_eq!(family("   "), None);
        assert_eq!(family("dumb"), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(family("KITTY 0.35"), Some("kitty"));
        assert_eq!(family("Konsole"), Some("konsole"));
        assert_eq!(family("XTerm(400)"), Some("xterm"));
    }

    #[test]
    fn xtversion_payloads_lose_their_introducer() {
        assert_eq!(strip_xtversion(">|xterm(400)"), Some("xterm(400)"));
        assert_eq!(strip_xtversion(">|kitty 0.35.2"), Some("kitty 0.35.2"));
        // A terminal that omits the introducer is still understood.
        assert_eq!(strip_xtversion("foot(1.16.2)"), Some("foot(1.16.2)"));
        assert_eq!(strip_xtversion(">|"), None);
        assert_eq!(strip_xtversion(""), None);
    }

    #[test]
    fn every_family_token_is_kebab_case() {
        // Digits are allowed: `iterm2` is the project's own spelling.
        for (_, family) in FAMILIES {
            assert!(
                family
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
                "{family}"
            );
        }
    }

    #[test]
    fn every_bounded_keyword_is_actually_in_the_table() {
        for keyword in BOUNDED {
            assert!(
                FAMILIES.iter().any(|(candidate, _)| candidate == keyword),
                "{keyword} is listed as bounded but is not a keyword"
            );
        }
    }
}
