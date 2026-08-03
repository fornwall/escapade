//! The shared `explain` renderer.
//!
//! Every feature's `explain` output is produced from its database entry by this
//! one function, so features cannot drift apart in presentation and adding a
//! feature means writing a TOML file rather than writing a renderer.
//!
//! Prose is handed to the markdown renderer, which wraps it and styles inline
//! code. Only the support matrix is laid out by hand, because it is the one part
//! that needs a different color per row — a matrix where ✔ and ✘ look the same
//! is not a matrix, it is a list.

use std::fmt::Write as _;

use crate::{
    db::{Catalog, Feature, Support, SupportStatus, Terminal, TerminalKind},
    features::wrap,
    theme::{Ink, Theme},
};

/// Renders a feature's full explanation to stdout.
pub fn render(theme: &Theme, catalog: &Catalog, feature: &Feature, show_matrix: bool) {
    header(theme, feature);

    theme.markdown(&feature.summary);

    if let Some(problem) = &feature.problem {
        theme.section("Why not just use what we have");
        theme.markdown(problem);
    }

    if let Some(how) = &feature.how_it_works {
        theme.section("How it works");
        theme.markdown(how);
    }

    if !feature.sequence.is_empty() {
        theme.section("The sequences");
        theme.markdown(&sequences_markdown(feature));
    }

    if !feature.param.is_empty() {
        theme.section("Parameters");
        theme.markdown(&params_markdown(feature));
    }

    if let Some(payload) = &feature.payload {
        theme.section("The payload");
        theme.markdown(payload);
    }

    if let Some(security) = &feature.security {
        theme.section("Why this is safe");
        theme.markdown(security);
    }

    if !feature.user.is_empty() {
        theme.section("Who is using it");
        theme.markdown(&users_markdown(feature));
    }

    if show_matrix && !feature.support.is_empty() {
        theme.section("Terminal support");
        matrix(theme, catalog, feature);
    }

    if let Some(caveats) = &feature.caveats {
        theme.section("Things that will bite you");
        theme.markdown(caveats);
    }

    if !feature.resource.is_empty() {
        theme.section("Further reading");
        for resource in &feature.resource {
            println!(
                "  {} {}",
                theme.paint("›", Ink::Muted),
                theme.inline(&resource.title)
            );
            println!("    {}", theme.paint(&resource.url, Ink::Link));
        }
    }

    footer(theme, feature);
}

/// Title, tagline and the metadata badges under it.
fn header(theme: &Theme, feature: &Feature) {
    println!();
    println!("  {}", theme.paint(&feature.name, Ink::Title));
    for line in wrap(&feature.tagline, theme.width().saturating_sub(4)) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }

    let mut badges = vec![format!("status: {}", feature.status.label())];
    if let Some(transport) = &feature.transport {
        badges.push(format!("transport: {transport}"));
    }
    if let Some(introduced) = &feature.introduced {
        badges.push(format!("introduced: {introduced}"));
    }
    if let Some(author) = &feature.author {
        badges.push(format!("by: {author}"));
    }
    if let Some(version) = &feature.spec_version {
        badges.push(format!("spec: v{version}"));
    }

    println!();
    let separator = theme.paint(" · ", Ink::Muted);
    let painted: Vec<String> = badges
        .iter()
        .map(|badge| theme.paint(badge, Ink::Accent))
        .collect();
    println!("  {}", painted.join(&separator));
    println!();
}

/// Builds the sequence reference as markdown.
///
/// Requests and replies go in a fenced block with arrows, which keeps them
/// monospaced and visually separate from the prose without needing hand-drawn
/// layout.
fn sequences_markdown(feature: &Feature) -> String {
    let mut out = String::new();
    for sequence in &feature.sequence {
        match &sequence.verb {
            Some(verb) => append(&mut out, format_args!("### `{verb}` — {}\n", sequence.name)),
            None => append(&mut out, format_args!("### {}\n", sequence.name)),
        }
        append(&mut out, format_args!("{}\n\n", sequence.purpose));
        out.push_str("```\n");
        append(&mut out, format_args!("→  {}\n", sequence.request));
        if let Some(reply) = &sequence.reply {
            append(&mut out, format_args!("←  {reply}\n"));
        }
        // The blank line matters: a paragraph butted straight against a closing
        // fence renders as part of the code block.
        out.push_str("```\n\n");
        if let Some(notes) = &sequence.notes {
            append(&mut out, format_args!("{}\n", notes.trim()));
        }
        out.push('\n');
    }
    out
}

/// Appends formatted text to a string.
///
/// Writing into a `String` cannot fail, so the `Result` is discarded — but going
/// through `write!` rather than `push_str(&format!(…))` avoids allocating a
/// temporary for every fragment.
fn append(out: &mut String, args: std::fmt::Arguments<'_>) {
    let _ = out.write_fmt(args);
}

/// Builds the parameter reference as a markdown bullet list.
fn params_markdown(feature: &Feature) -> String {
    let mut out = String::new();
    for param in &feature.param {
        let default = if param.default.is_empty() {
            String::new()
        } else {
            format!(", default `{}`", param.default)
        };
        append(
            &mut out,
            format_args!(
                "* **`{}`** *({}{default})* — {}\n",
                param.key, param.applies_to, param.description
            ),
        );
    }
    out
}

/// Builds the users list as markdown.
fn users_markdown(feature: &Feature) -> String {
    let mut out = String::new();
    for user in &feature.user {
        append(
            &mut out,
            format_args!("* **{}** *({})*", user.name, user.kind),
        );
        if let Some(notes) = &user.notes {
            append(&mut out, format_args!(" — {notes}"));
        }
        out.push('\n');
        if let Some(url) = &user.url {
            append(&mut out, format_args!("  {url}\n"));
        }
    }
    out
}

/// Draws the support matrix.
///
/// Rows that say something interesting get a line each plus any notes. The long
/// tail of "no known implementation" is collapsed into one sentence, because
/// fourteen identical ✘ rows communicate less than a list of names does.
fn matrix(theme: &Theme, catalog: &Catalog, feature: &Feature) {
    let rows = feature.support_matrix(catalog);
    let (interesting, unsupported): (Vec<_>, Vec<_>) = rows
        .iter()
        .partition(|(_, support)| support.status != SupportStatus::No);

    let name_width = interesting
        .iter()
        .map(|(terminal, _)| terminal.name.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 24);

    println!();
    for (terminal, support) in &interesting {
        row(theme, terminal, support, name_width);
    }

    if !unsupported.is_empty() {
        let names: Vec<&str> = unsupported
            .iter()
            .map(|(terminal, _)| terminal.name.as_str())
            .collect();
        println!();
        let label = format!("{} no known implementation:", SupportStatus::No.symbol());
        let indent = label.chars().count() + 3;
        let lines = wrap(&names.join(", "), theme.width().saturating_sub(indent));
        for (index, line) in lines.iter().enumerate() {
            if index == 0 {
                println!(
                    "  {} {}",
                    theme.paint(&label, Ink::Bad),
                    theme.paint(line, Ink::Muted)
                );
            } else {
                println!("{}{}", " ".repeat(indent), theme.paint(line, Ink::Muted));
            }
        }
    }

    println!();
    println!(
        "  {}",
        theme.paint(
            &format!("Corrections welcome: data/features/{}.toml", feature.id),
            Ink::Muted
        )
    );
}

/// One support-matrix row, plus its notes and source underneath.
fn row(theme: &Theme, terminal: &Terminal, support: &Support, name_width: usize) {
    let ink = status_ink(support.status);
    let name = if terminal.kind == TerminalKind::Multiplexer {
        format!("{} (multiplexer)", terminal.name)
    } else {
        terminal.name.clone()
    };

    let padding = name_width.saturating_sub(terminal.name.chars().count());
    let mut line = format!(
        "  {} {}{} {}",
        theme.paint(support.status.symbol(), ink),
        theme.paint(&name, Ink::Strong),
        " ".repeat(padding),
        theme.paint(support.status.label(), ink),
    );
    if let Some(since) = &support.since {
        line.push_str(&theme.paint(&format!("  · since {since}"), Ink::Muted));
    }
    println!("{line}");

    if let Some(notes) = &support.notes {
        println!("{}", theme.markdown_indented(notes, 6));
    }
    // The project homepage is worth showing for terminals a reader might go and
    // install; for the long tail of unsupported ones it would just be noise, and
    // those rows are collapsed away before they reach here.
    if let Some(url) = &terminal.url {
        println!("      {}", theme.paint(url, Ink::Link));
    }
    if let Some(source) = &support.source {
        println!(
            "      {} {}",
            theme.paint("source:", Ink::Muted),
            theme.paint(source, Ink::Link)
        );
    }
}

/// The color for a support status.
fn status_ink(status: SupportStatus) -> Ink {
    match status {
        SupportStatus::Yes => Ink::Good,
        SupportStatus::Partial | SupportStatus::Flag | SupportStatus::Passthrough => Ink::Warn,
        SupportStatus::No => Ink::Bad,
        SupportStatus::Unknown => Ink::Muted,
    }
}

/// The verbs each feature actually exposes, for the closing section.
///
/// This restates a little of [`crate::cli`], which is not ideal — but the verbs
/// genuinely differ per feature (`grapheme` has no `demo`, `hyperlink`'s primary
/// form is not a verb at all), and a footer that advertises a subcommand the
/// feature does not have is worse than a small table. The alternative, threading
/// the command surface through every caller, buys accuracy at the cost of
/// coupling every feature module to the renderer.
///
/// A test asserts every feature in the database appears here.
fn verbs_for(feature_id: &str) -> &'static [(&'static str, &'static str)] {
    match feature_id {
        "glyph-protocol" => &[
            ("probe", "does this terminal support it?"),
            (
                "probe <CODEPOINTS>",
                "which codepoints can it actually draw?",
            ),
            ("demo", "watch it work, sequence by sequence"),
        ],
        "kitty-graphics" => &[
            ("probe", "does this terminal support it?"),
            (
                "render <FILE>",
                "display an image, diagram or profile right now",
            ),
            ("demo", "watch it work, sequence by sequence"),
        ],
        "grapheme-clustering" => &[
            ("probe", "does this terminal support it?"),
            ("probe <CODEPOINTS>", "is this sequence one character here?"),
            (
                "render <CODEPOINTS>",
                "render it, with clustering switched on",
            ),
        ],
        "hyperlinks" => &[
            ("--url <URL> <TEXT>", "emit a link right now"),
            ("probe", "does this terminal support it?"),
            ("demo", "watch it work, sequence by sequence"),
        ],
        "notifications" => &[
            ("--title <T> <BODY>", "raise a notification right now"),
            ("probe", "which protocols does this terminal support?"),
            ("demo", "watch it work, sequence by sequence"),
        ],
        "progress" => &[
            ("<PERCENT>", "show progress right now"),
            ("--watch", "drive it from a stream of percentages"),
            ("probe", "does this terminal show it?"),
            ("demo", "watch it work, sequence by sequence"),
        ],
        "text-sizing" => &[
            ("--scale <N> <TEXT>", "draw text several cells tall"),
            ("probe", "which of the protocol's keys work here?"),
            ("demo", "watch it work, sequence by sequence"),
        ],
        "color" => &[
            ("", "report the terminal's colours"),
            ("--watch", "print a line each time they change"),
            ("--is-dark", "answer light or dark through the exit status"),
            ("probe", "which colour queries does it answer?"),
        ],
        "clipboard" => &[
            (
                "copy <TEXT>",
                "put text on the clipboard, over ssh if need be",
            ),
            ("paste", "read it back, if the terminal permits"),
            ("probe", "does this terminal allow it?"),
            ("demo", "watch it work, sequence by sequence"),
        ],
        "text-attributes" => &[
            ("--bold --underline=curly <TEXT>", "style text right now"),
            (
                "probe --bold",
                "which attributes does this terminal render?",
            ),
            ("demo", "watch it work, sequence by sequence"),
        ],
        "keyboard-protocol" => &[
            ("watch", "see what your keyboard actually sends"),
            ("probe", "which protocol flags work here?"),
            ("demo", "watch it work, sequence by sequence"),
        ],
        "unscroll" => &[
            ("", "pull scrollback back onto the screen"),
            ("probe", "can this terminal unscroll?"),
        ],
        "dimensions" => &[
            ("", "report the size in cells and pixels"),
            (
                "probe --rows-at-least <N>",
                "test the size, answering through the exit status",
            ),
        ],
        // A feature with no table falls back to the one verb every feature has.
        _ => &[("--help", "the verbs this feature offers")],
    }
}

/// Closing pointers: the spec, and the commands that do something.
fn footer(theme: &Theme, feature: &Feature) {
    let subcommand = super::subcommand_for(&feature.id);
    theme.section("Try it");
    let verbs = verbs_for(&feature.id);
    let width = verbs
        .iter()
        .map(|(verb, _)| subcommand.len() + verb.len())
        .max()
        .unwrap_or(0);
    for (verb, description) in verbs {
        // An empty verb is the feature's bare top-level form.
        let invocation = if verb.is_empty() {
            format!("escapade {subcommand}")
        } else {
            format!("escapade {subcommand} {verb}")
        };
        let padding = width.saturating_sub(subcommand.len() + verb.len());
        println!(
            "  {}{}   {}",
            theme.paint(&invocation, Ink::Code),
            " ".repeat(padding),
            theme.paint(description, Ink::Muted)
        );
    }
    if let Some(spec) = &feature.spec_url {
        println!();
        println!(
            "  {} {}",
            theme.paint("Specification:", Ink::Muted),
            theme.paint(spec, Ink::Link)
        );
    }
    if let Some(examples) = &feature.examples_url {
        println!(
            "  {}      {}",
            theme.paint("Examples:", Ink::Muted),
            theme.paint(examples, Ink::Link)
        );
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_every_feature_without_panicking() {
        // Exercises the whole layout path against the real database, which is
        // what catches a data file that parses but has a section the renderer
        // does not expect.
        let catalog = Catalog::load().unwrap();
        let theme = Theme::plain();
        for feature in catalog.features() {
            render(&theme, &catalog, feature, true);
            render(&theme, &catalog, feature, false);
        }
    }

    #[test]
    fn generated_markdown_covers_every_entry() {
        let catalog = Catalog::load().unwrap();
        let feature = catalog.feature("glyph-protocol").unwrap();

        let sequences = sequences_markdown(feature);
        for sequence in &feature.sequence {
            assert!(
                sequences.contains(&sequence.request),
                "sequence `{}` is missing from the output",
                sequence.name
            );
        }

        let params = params_markdown(feature);
        for param in &feature.param {
            assert!(
                params.contains(&param.key),
                "parameter `{}` is missing from the output",
                param.key
            );
        }
    }

    #[test]
    fn every_feature_advertises_only_verbs_it_has() {
        // The footer used to print `<feature> demo` unconditionally, which was a
        // lie for `grapheme`. Every feature must have an explicit entry, so
        // adding a feature without one is caught here rather than by a reader
        // running a command that does not exist.
        let catalog = Catalog::load().unwrap();
        for feature in catalog.features() {
            let verbs = verbs_for(&feature.id);
            assert_ne!(
                verbs,
                verbs_for("no-such-feature"),
                "feature `{}` has no entry in verbs_for",
                feature.id
            );
            assert!(!verbs.is_empty());
            for (verb, description) in verbs {
                // An empty verb is legitimate: it denotes the feature's bare
                // top-level form, as in `escapade color` or `escapade dimensions`,
                // which `footer` renders without a trailing space. The
                // description is what must always be there.
                assert!(
                    !description.is_empty(),
                    "feature `{}` has a verb `{verb}` with no description",
                    feature.id
                );
            }
        }
    }

    #[test]
    fn status_colors_distinguish_yes_from_no() {
        assert_eq!(status_ink(SupportStatus::Yes), Ink::Good);
        assert_eq!(status_ink(SupportStatus::No), Ink::Bad);
        assert_ne!(
            status_ink(SupportStatus::Passthrough),
            status_ink(SupportStatus::Yes)
        );
    }
}
