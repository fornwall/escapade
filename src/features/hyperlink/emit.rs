//! `escapade hyperlink --url <URL> <TEXT>` — the top-level form.
//!
//! Unlike the other features, the useful thing about OSC 8 is a one-liner rather
//! than a walkthrough, so emitting a link is what `escapade hyperlink` does when
//! no verb follows it. This is also the one thing in `escapade` that is meant to
//! be used from a script:
//!
//! ```text
//! escapade hyperlink --url "https://github.com/$repo/pull/$n" "#$n $title"
//! git rev-parse HEAD | escapade hyperlink --url "$commit_url"
//! ```
//!
//! The label comes from the arguments or, when there are none, from standard
//! input — which is the more interesting case for sanitisation, because piped
//! text is text somebody else wrote.
//!
//! There is deliberately no capability check in front of any of it. A terminal
//! that does not implement OSC 8 prints the text and drops the link, so gating the
//! sequence on a probe would cost a round trip to gain nothing.

use std::io::{IsTerminal as _, Write as _, stdout};

use anyhow::Result;

use crate::{
    cli::HyperlinkCommand,
    features::{
        hyperlink::{Hyperlink, MAX_URI_LEN, Terminator, sanitize_text, text_is_clean},
        text_operand, wrap,
    },
    term::pretty,
    theme::{Ink, Theme},
};

/// Emits (or explains) one hyperlink.
///
/// # Errors
///
/// Fails if the URL or the id cannot be sanitised, if the label cannot be read
/// from standard input, or if stdout cannot be written.
pub fn run(theme: &Theme, command: &HyperlinkCommand) -> Result<()> {
    // `--url` is `required = true` in the parser, so the fallback here only
    // triggers for `--url ""` — which `Hyperlink::new` rejects with a much better
    // message than a missing-argument error would give.
    let url = command.url.as_deref().unwrap_or_default();
    let mut link = Hyperlink::new(url)?;
    if let Some(id) = &command.id {
        link = link.with_id(id)?;
    }
    // Arguments, or standard input when there are none. Whatever comes back goes
    // through the same sanitiser: a piped label is exactly the untrusted input
    // that could otherwise close the link and open its own.
    let text = text_operand(&command.text)?;

    if command.dry_run {
        explain(theme, &link, url, &text);
        return Ok(());
    }

    if link.over_length() {
        eprintln!(
            "escapade: warning: the URI is {} bytes; VTE and iTerm2 drop links over {MAX_URI_LEN}, \
             so the text will print without one",
            link.uri().len()
        );
    }
    let mut out = stdout().lock();
    out.write_all(link.wrap(&text).as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// Prints the sequence in readable form, names its pieces, and points out
/// anything worth knowing about this particular link.
fn explain(theme: &Theme, link: &Hyperlink, original_url: &str, text: &str) {
    let sanitized = sanitize_text(text);

    println!();
    println!("  {}", theme.paint("the sequence", Ink::Accent));
    println!("    {}", theme.paint(&pretty(&link.open()), Ink::Code));
    println!("    {}", theme.paint(&pretty(&sanitized), Ink::Link));
    println!("    {}", theme.paint(&pretty(&link.close()), Ink::Code));
    println!();
    println!(
        "  {} {}",
        theme.paint("as a shell one-liner:", Ink::Muted),
        theme.paint(&printf_form(link, text), Ink::Code)
    );
    println!(
        "  {} {}",
        theme.paint("cost:", Ink::Muted),
        theme.paint(
            &format!(
                "{} bytes of sequence around {} bytes of text",
                link.open().len() + link.close().len(),
                sanitized.len()
            ),
            Ink::Muted
        )
    );

    show_pieces(theme, link);
    show_notes(theme, &collect_notes(link, original_url, text, &sanitized));

    println!();
    println!(
        "  {}",
        theme.paint(
            "Drop --dry-run to send it. Nothing is probed first: a terminal without OSC 8 prints \
             the text and ignores the link.",
            Ink::Muted
        )
    );
    println!();
}

/// Renders the link as a `printf` one-liner that can be pasted into a shell.
///
/// Control bytes become printf's own escapes (`\e`, `\a`), and a single quote in
/// the text is closed, escaped and reopened the way every shell script does it —
/// so the result is a single argument no matter what the text contains.
fn printf_form(link: &Hyperlink, text: &str) -> String {
    let raw = link.wrap(text);
    let mut body = String::with_capacity(raw.len() + 16);
    for ch in raw.chars() {
        match ch {
            '\u{1b}' => body.push_str("\\e"),
            '\u{7}' => body.push_str("\\a"),
            '\n' => body.push_str("\\n"),
            '\t' => body.push_str("\\t"),
            '\\' => body.push_str("\\\\"),
            '\'' => body.push_str("'\\''"),
            // printf reads `%` as the start of a conversion, and a
            // percent-encoded URI is full of them.
            '%' => body.push_str("%%"),
            other => body.push(other),
        }
    }
    format!("printf '{body}\\n'")
}

/// Names every piece of the sequence.
fn show_pieces(theme: &Theme, link: &Hyperlink) {
    println!();
    println!("  {}", theme.paint("piece by piece", Ink::Accent));
    let params = link.params();
    let param_note = if params.is_empty() {
        "the parameter field, empty here. This is why a plain hyperlink has two \
         semicolons in a row."
            .to_owned()
    } else {
        format!(
            "the parameter field. `{params}` groups this link with any other run of cells sharing \
             the same id and the same URI, so they highlight as one on hover."
        )
    };
    let pieces: [(&str, String); 6] = [
        (
            "OSC",
            "Operating System Command — `ESC ]`. The escape class for anything that talks to the \
             terminal rather than to the screen."
                .to_owned(),
        ),
        (
            "8",
            "the command number. A terminal that does not know command 8 parses the string per \
             ECMA-48 and throws it away, which is why this is safe to send blind."
                .to_owned(),
        ),
        (if params.is_empty() { ";;" } else { ";…;" }, param_note),
        (
            "URI",
            format!(
                "the target, percent-encoded: `{}`. {} bytes of the {MAX_URI_LEN} VTE and iTerm2 \
                 allow.",
                link.uri(),
                link.uri().len()
            ),
        ),
        (
            link.terminator().name(),
            match link.terminator() {
                Terminator::St => {
                    "String Terminator — `ESC \\`. Ends the OSC string, and the standard choice."
                        .to_owned()
                }
                Terminator::Bel => {
                    "0x07, xterm's terminator. One byte shorter, and an audible bell on any \
                     terminal that does not recognise the OSC."
                        .to_owned()
                }
            },
        ),
        (
            "close",
            "`OSC 8 ; ; ST` — the same sequence with the URI left out. An empty URI means \"no \
             link\", so this is how a link ends."
                .to_owned(),
        ),
    ];
    let token_width = pieces
        .iter()
        .map(|(token, _)| token.chars().count())
        .max()
        .unwrap_or(0);
    for (token, meaning) in &pieces {
        let label = format!("{token:<token_width$}");
        let lines = wrap(meaning, theme.width().saturating_sub(token_width + 8));
        for (index, line) in lines.iter().enumerate() {
            if index == 0 {
                println!(
                    "    {}  {}",
                    theme.paint(&label, Ink::Accent),
                    theme.paint(line, Ink::Muted)
                );
            } else {
                println!(
                    "    {}  {}",
                    " ".repeat(token_width),
                    theme.paint(line, Ink::Muted)
                );
            }
        }
    }
}

/// Prints the per-link observations, if there are any.
fn show_notes(theme: &Theme, notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    println!();
    println!("  {}", theme.paint("worth knowing", Ink::Accent));
    for note in notes {
        let lines = wrap(note, theme.width().saturating_sub(8));
        for (index, line) in lines.iter().enumerate() {
            if index == 0 {
                println!("    {} {}", theme.paint("·", Ink::Warn), line);
            } else {
                println!("      {line}");
            }
        }
    }
}

/// The per-link observations worth surfacing in `--dry-run`.
fn collect_notes(
    link: &Hyperlink,
    original_url: &str,
    text: &str,
    sanitized_text: &str,
) -> Vec<String> {
    let mut notes = Vec::new();

    if link.uri() != original_url {
        // The original may contain the very bytes this feature exists to defuse,
        // so it goes through the text sanitiser before being shown.
        notes.push(format!(
            "The URI was percent-encoded on the way in: `{}` became `{}`. Bytes outside 32–126 \
             must be encoded, and a raw ESC or BEL would have ended the OSC string early — that is \
             the whole attack this closes.",
            pretty(&sanitize_text(original_url)),
            link.uri()
        ));
    }
    if !text_is_clean(text) {
        notes.push(format!(
            "Control bytes in the visible text were replaced with their Control Pictures glyphs: \
             `{}`. The text is outside the OSC string, so an ESC there could have closed this link \
             and opened a different one over the rest of the line.",
            pretty(sanitized_text)
        ));
    }
    if sanitized_text.is_empty() {
        notes.push(
            "The visible text is empty, so this link occupies no cells and there is nothing to \
             click. Pass some text."
                .to_owned(),
        );
    }
    if link.over_length() {
        notes.push(format!(
            "The URI is {} bytes. VTE and iTerm2 cap it at {MAX_URI_LEN} and will drop the link, \
             printing the text on its own.",
            link.uri().len()
        ));
    }
    if link.uri().starts_with("file:///") {
        notes.push(
            "This is a `file://` URI with no hostname. It works locally, and then silently refers \
             to the wrong machine's filesystem as soon as the output crosses an ssh connection. \
             Put the hostname in: `file://myhost/path`."
                .to_owned(),
        );
    }
    if sanitized_text.contains('\n') && link.id().is_none() {
        notes.push(
            "The text spans more than one line and has no `id`. Most terminals then highlight \
             only the row under the pointer, so it looks like two links — pass `--id` to make \
             both runs hover as one."
                .to_owned(),
        );
    }
    if !stdout().is_terminal() {
        notes.push(
            "stdout is not a terminal. The sequence would still be written, which is exactly what \
             you want when piping into `less -R`, and exactly what you do not want when capturing \
             output to a log."
                .to_owned(),
        );
    }
    notes
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn link(uri: &str) -> Hyperlink {
        Hyperlink::new(uri).unwrap()
    }

    #[test]
    fn encoding_the_uri_is_reported() {
        let notes = collect_notes(&link("https://x/a b"), "https://x/a b", "hi", "hi");
        assert!(
            notes.iter().any(|note| note.contains("percent-encoded")),
            "{notes:?}"
        );
    }

    #[test]
    fn an_untouched_uri_produces_no_encoding_note() {
        let notes = collect_notes(&link("https://x/ab"), "https://x/ab", "hi", "hi");
        assert!(!notes.iter().any(|note| note.contains("percent-encoded")));
    }

    #[test]
    fn a_hostnameless_file_uri_is_called_out() {
        let notes = collect_notes(
            &link("file:///etc/hosts"),
            "file:///etc/hosts",
            "hosts",
            "hosts",
        );
        assert!(
            notes.iter().any(|note| note.contains("no hostname")),
            "{notes:?}"
        );

        let with_host = collect_notes(
            &link("file://myhost/etc/hosts"),
            "file://myhost/etc/hosts",
            "hosts",
            "hosts",
        );
        assert!(!with_host.iter().any(|note| note.contains("no hostname")));
    }

    #[test]
    fn sanitized_text_and_empty_text_are_both_reported() {
        let notes = collect_notes(&link("https://x"), "https://x", "a\u{1b}b", "a\u{241b}b");
        assert!(
            notes.iter().any(|note| note.contains("Control Pictures")),
            "{notes:?}"
        );

        let notes = collect_notes(&link("https://x"), "https://x", "", "");
        assert!(
            notes.iter().any(|note| note.contains("no cells")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_multiline_link_without_an_id_is_reported() {
        let notes = collect_notes(&link("https://x"), "https://x", "a\nb", "a\nb");
        assert!(notes.iter().any(|note| note.contains("`id`")), "{notes:?}");

        let with_id = link("https://x").with_id("pair").unwrap();
        let notes = collect_notes(&with_id, "https://x", "a\nb", "a\nb");
        assert!(!notes.iter().any(|note| note.contains("hover as one")));
    }

    #[test]
    fn over_length_uris_are_reported() {
        let long = "a".repeat(MAX_URI_LEN + 1);
        let notes = collect_notes(&link(&long), &long, "x", "x");
        assert!(
            notes.iter().any(|note| note.contains("cap it")),
            "{notes:?}"
        );
    }

    #[test]
    fn the_printf_form_is_pasteable_and_contains_no_control_bytes() {
        let plain = link("https://example.com");
        assert_eq!(
            printf_form(&plain, "click me"),
            "printf '\\e]8;;https://example.com\\e\\\\click me\\e]8;;\\e\\\\\\n'"
        );

        let bel = link("https://example.com").with_terminator(Terminator::Bel);
        assert!(printf_form(&bel, "x").contains("\\a"));

        // A quote in the text must not end the shell string.
        let quoted = printf_form(&plain, "it's here");
        assert!(quoted.contains("it'\\''s here"), "{quoted}");

        // Percent-encoding in the URI must survive printf's own format parsing.
        let encoded = link("https://example.com/a b");
        let form = printf_form(&encoded, "x");
        assert!(form.contains("a%%20b"), "{form}");

        for form in [
            printf_form(&plain, "a\u{1b}b"),
            printf_form(&bel, "a\u{7}b"),
            printf_form(&plain, "a\nb"),
        ] {
            assert!(!form.contains('\u{1b}'), "{form:?}");
            assert!(!form.contains('\u{7}'), "{form:?}");
            assert!(!form.contains('\n'), "{form:?}");
        }
    }

    #[test]
    fn every_note_is_safe_to_print() {
        let hostile = "https://x/\u{1b}\\\u{7}";
        let notes = collect_notes(&link(hostile), hostile, "t\u{1b}x", "t\u{241b}x");
        assert!(!notes.is_empty());
        for note in &notes {
            assert!(!note.contains('\u{1b}'), "{note:?}");
            assert!(!note.contains('\u{7}'), "{note:?}");
        }
    }
}
