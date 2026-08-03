//! `escapade style [ATTRIBUTES] <TEXT>` — the top-level form.
//!
//! Styling text is what this subcommand does when no verb follows it, because for
//! this feature the useful thing is a one-liner rather than a walkthrough:
//!
//! ```text
//! escapade style --bold --underline=curly --color blue 'warning'
//! cargo build 2>&1 | escapade style --italic
//! ```
//!
//! There is deliberately no capability check in front of it. Every attribute here is
//! one parameter of one `CSI … m`, and a terminal that does not implement a parameter
//! is required to ignore it, so gating on a probe would cost a round trip to gain
//! nothing. `escapade style probe` is there for the caller who wants to know anyway.
//!
//! `--dry-run` is the interesting half: it prints the sequence in readable form, names
//! every piece of it, and points out whatever is worth knowing about this particular
//! combination — an attribute whose reset is shared, a colour dropped by `NO_COLOR`, a
//! shape that degrades to a plain line.

use std::io::{IsTerminal as _, Write as _, stdout};

use anyhow::Result;

use crate::{
    cli::StyleCommand,
    features::{
        style::{
            attrs::{Attr, Rendition, SGR_TWENTY_ONE, UnderlineShape},
            color::{Color, ColorRole},
            preference::{ColorPreference, NO_COLOR},
            sanitize_text, text_is_clean,
        },
        text_operand, wrap,
    },
    term::pretty,
    theme::{Ink, Theme},
};

/// Styles (or explains) one run of text.
///
/// # Errors
///
/// Fails if a colour cannot be parsed, if no text was given and standard input is a
/// terminal, or if standard output cannot be written.
pub fn run(theme: &Theme, command: &StyleCommand) -> Result<()> {
    let preference = ColorPreference::from_env();
    // Colours are parsed — and rejected — before anything is read or written, so a
    // typo is reported without having consumed the pipe. Note that this happens even
    // when `NO_COLOR` will drop the colour: a mistake is still a mistake.
    let (rendition, dropped) = Rendition::from_flags(&command.attrs, &preference)?;

    // Args-or-stdin. This is what turns the "no arguments and a terminal on stdin"
    // mistake into an error naming both ways to supply the text, rather than a hang.
    let text = text_operand(&command.text)?;

    if command.dry_run {
        explain(theme, &rendition, &dropped, &preference, &text);
        return Ok(());
    }

    // The one thing worth saying on stderr: colour was asked for and withheld. On
    // stderr rather than stdout so it cannot corrupt the styled output itself, and
    // only when a colour was actually requested, so it is never noise.
    if !dropped.is_empty() {
        eprintln!(
            "escapade: {NO_COLOR} is set, so {} left out; the text is unaffected",
            describe_dropped(&dropped)
        );
    }

    let mut out = stdout().lock();
    out.write_all(rendition.wrap(&text).as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// Names the colours that `NO_COLOR` removed, for a one-line message.
fn describe_dropped(dropped: &[Attr]) -> String {
    let names: Vec<&str> = dropped.iter().map(|attr| attr.name()).collect();
    match names.as_slice() {
        [] => "nothing was".to_owned(),
        [one] => format!("the {one} colour was"),
        many => format!("the {} colours were", many.join(", ")),
    }
}

/// Prints the sequence in readable form, names its pieces, and points out anything
/// worth knowing about this particular request.
fn explain(
    theme: &Theme,
    rendition: &Rendition,
    dropped: &[Attr],
    preference: &ColorPreference,
    text: &str,
) {
    let sanitized = sanitize_text(text);

    println!();
    if rendition.is_empty() {
        println!("  {}", theme.paint("no attributes", Ink::Accent));
        for line in wrap(
            "No attribute flags were given, so there is no sequence to show: the text would be \
             printed exactly as it arrived. Pass something like --bold, --underline=curly or \
             --color red.",
            theme.width().saturating_sub(4),
        ) {
            println!("  {}", theme.paint(&line, Ink::Muted));
        }
    } else {
        println!("  {}", theme.paint("the sequence", Ink::Accent));
        println!("    {}", theme.paint(&pretty(&rendition.open()), Ink::Code));
        println!("    {}", theme.paint(&pretty(&sanitized), Ink::Strong));
        println!(
            "    {}",
            theme.paint(&pretty(&rendition.close()), Ink::Code)
        );
        println!();
        println!(
            "  {} {}",
            theme.paint("as a shell one-liner:", Ink::Muted),
            theme.paint(&rendition.printf_form(text), Ink::Code)
        );
        println!(
            "  {} {}",
            theme.paint("cost:", Ink::Muted),
            theme.paint(
                &format!(
                    "{} attribute{} in {} bytes of sequence around {} bytes of text",
                    rendition.attrs().len(),
                    if rendition.attrs().len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    rendition.open().len() + rendition.close().len(),
                    sanitized.len()
                ),
                Ink::Muted
            )
        );
        show_pieces(theme, rendition);
    }

    show_notes(
        theme,
        &collect_notes(rendition, dropped, preference, text, &sanitized),
    );

    println!();
    for line in wrap(
        "Drop --dry-run to send it. Nothing is probed first: every one of these is a parameter of \
         one SGR sequence, and a terminal that does not implement a parameter ignores it.",
        theme.width().saturating_sub(4),
    ) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();
}

/// Names every piece of the sequence, attribute by attribute, then the reset.
fn show_pieces(theme: &Theme, rendition: &Rendition) {
    let mut pieces: Vec<(String, String)> = vec![(
        "CSI".to_owned(),
        "Control Sequence Introducer — `ESC [`. Select Graphic Rendition lives here, along with \
         every other cell attribute."
            .to_owned(),
    )];

    for attr in rendition.attrs() {
        pieces.push((attr.params(), describe_attr(*attr)));
    }

    pieces.push((
        "m".to_owned(),
        "the final byte that makes this Select Graphic Rendition rather than one of the other \
         hundred CSI sequences."
            .to_owned(),
    ));

    pieces.push((
        rendition.reset_params(),
        format!(
            "the reset, at the end. {} Only what was set is unset: `CSI 0 m` would be shorter and \
             would also clear whatever the surrounding program had configured, which is how a \
             styled word in the middle of a coloured line ends up taking the rest of the line with \
             it.",
            describe_resets(rendition)
        ),
    ));

    println!();
    println!("  {}", theme.paint("piece by piece", Ink::Accent));
    let token_width = pieces
        .iter()
        .map(|(token, _)| token.chars().count())
        .max()
        .unwrap_or(0)
        .min(24);
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

/// One sentence on what an attribute is and what to watch for.
fn describe_attr(attr: Attr) -> String {
    match attr {
        Attr::Bold => "`SGR 1`, \"bold or increased intensity\". Some terminals draw a heavier \
                       font, some brighten the colour instead, and several do both — so bold is \
                       reliably *different* and not reliably *heavier*."
            .to_owned(),
        Attr::Dim => "`SGR 2`, \"faint\". Shares its reset with bold, and is quietly ignored by a \
                      fair number of terminals."
            .to_owned(),
        Attr::Italic => "`SGR 3`, \"italicized\". Needs a font with an italic face; a terminal \
                         without one may synthesise a slant or ignore it."
            .to_owned(),
        Attr::Underline(shape) => match shape {
            UnderlineShape::Single => "the underline itself. ECMA-48 `SGR 4`, \"singly \
                                       underlined\", understood by every terminal ever built. \
                                       `4:1` says the same thing in the newer spelling and buys \
                                       nothing."
                .to_owned(),
            UnderlineShape::None => "ECMA-48 `SGR 24`, \"not underlined\". `4:0` is the symmetric \
                                     spelling; `24` is the one that needs no sub-parameter parser."
                .to_owned(),
            other => format!(
                "`SGR 4` with sub-parameter {}: {}. One parameter, not two — the colon is what \
                 makes a terminal that does not know this shape skip it whole and draw a plain \
                 line instead. Originated by mintty; xterm implements none of these.",
                other.sub_parameter(),
                other.purpose()
            ),
        },
        Attr::Blink => "`SGR 5`, \"slowly blinking\". In ECMA-48 and in the VT100, and \
                        deliberately not animated by a number of modern terminals — so this is \
                        one to probe rather than assume."
            .to_owned(),
        Attr::RapidBlink => "`SGR 6`, \"rapidly blinking\". Almost universally folded into \
                             `SGR 5`, which a readback catches: a terminal that folded it reports \
                             `5` back."
            .to_owned(),
        Attr::Reverse => "`SGR 7`, \"negative image\" — foreground and background swapped. \
                          Universal, and note that it swaps the *effective* colours, so it \
                          interacts with the defaults rather than with any colour you set."
            .to_owned(),
        Attr::Conceal => "`SGR 8`, \"concealed characters\". Never a security measure: the text \
                          is still in the cell, still selectable, and still copied to the \
                          clipboard."
            .to_owned(),
        Attr::Strikethrough => {
            "`SGR 9`, \"crossed-out\". The one attribute in this family whose ECMA-48 number and \
             its common name disagree — terminfo calls it `smxx`."
                .to_owned()
        }
        Attr::Overline => "`SGR 53`, \"overlined\". The rarest of the family, and the one whose \
                           reset is easiest to get wrong: it is `55`, not `54`, because `54` is \
                           \"not framed, not encircled\" — the reset for `51` and `52`, which \
                           essentially nothing implements."
            .to_owned(),
        Attr::Color(role, color) => describe_color(role, color),
    }
}

/// One sentence on a colour, keyed on what makes that role and depth interesting.
fn describe_color(role: ColorRole, color: Color) -> String {
    let depth = match color {
        Color::Basic(_) => "One of the sixteen colours every terminal has had since the 1980s, \
                            resolved against the user's own palette."
            .to_owned(),
        Color::Palette(index) => {
            format!("Palette entry {index} of 256, resolved against the user's own colour scheme.")
        }
        Color::Rgb(..) => format!(
            "24-bit direct colour, {}. This is the depth worth probing: a terminal that does not \
             implement it may quantise the colour to the nearest palette entry rather than \
             refusing it, and the result looks like a bug in your program.",
            color.describe()
        ),
    };
    let role_note = match role {
        ColorRole::Foreground => {
            "`SGR 38` for the text colour, or `30`-`37`/`90`-`97` for a basic one — the bright \
             eight being aixterm's addition."
        }
        ColorRole::Background => {
            "`SGR 48` for the cell background, or `40`-`47`/`100`-`107` for a basic one."
        }
        ColorRole::Underline => {
            "`SGR 58` for the underline's own colour, independent of the text colour — which is \
             the whole point for a linter, and why `SGR 39` does not reset it. Spelled with \
             colons, because that is what terminfo's `Setulc` emits and what the terminals \
             implementing it were written against. There is no basic form, so a named colour \
             becomes `58:5:<n>`."
        }
    };
    format!("{role_note} {depth}")
}

/// Explains the reset parameters actually being emitted, calling out the shared ones.
fn describe_resets(rendition: &Rendition) -> String {
    let mut notes: Vec<String> = Vec::new();
    let attrs = rendition.attrs();
    let has = |wanted: Attr| attrs.contains(&wanted);

    if has(Attr::Bold) && has(Attr::Dim) {
        notes.push(
            "`22` appears once and cancels *both* bold and faint: ECMA-48 gives them one reset \
             between them, so there is no way to stop being bold and stay faint."
                .to_owned(),
        );
    } else if has(Attr::Bold) {
        notes.push(format!(
            "`22` cancels bold — not `{SGR_TWENTY_ONE}`, whose meaning is genuinely contested: \
             ECMA-48 says \"doubly underlined\" and a great many terminals implement \"bold off\"."
        ));
    }
    if has(Attr::Blink) && has(Attr::RapidBlink) {
        notes.push(
            "`25` cancels both blinks, the other place ECMA-48 gives two attributes one reset."
                .to_owned(),
        );
    }
    if has(Attr::Overline) {
        notes.push(
            "`55` cancels the overline; `54` would not, being the reset for framed and encircled."
                .to_owned(),
        );
    }
    if attrs
        .iter()
        .any(|attr| attr.role() == Some(ColorRole::Underline))
    {
        notes.push(
            "`59` puts the underline colour back to \"follow the foreground\"; `39` would not \
             touch it."
                .to_owned(),
        );
    }
    if notes.is_empty() {
        return format!(
            "One reset per attribute, in the order they were set: `{}`.",
            rendition.reset_params()
        );
    }
    notes.join(" ")
}

/// Prints the per-request observations, if there are any.
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
                println!("    {} {line}", theme.paint("\u{b7}", Ink::Warn));
            } else {
                println!("      {line}");
            }
        }
    }
}

/// The observations worth surfacing for this particular request.
///
/// Pure, and separated from the printing so the interesting decisions can be tested
/// without a terminal.
#[expect(
    clippy::too_many_lines,
    reason = "a flat list of independent observations, each a paragraph the reader might \
              need; splitting it would scatter them without making any one clearer"
)]
fn collect_notes(
    rendition: &Rendition,
    dropped: &[Attr],
    preference: &ColorPreference,
    text: &str,
    sanitized: &str,
) -> Vec<String> {
    let mut notes = Vec::new();
    let attrs = rendition.attrs();

    // NO_COLOR first: it is the note most likely to be the answer somebody came for.
    if !dropped.is_empty() {
        let names: Vec<&str> = dropped.iter().map(|attr| attr.name()).collect();
        notes.push(format!(
            "{} — so {} left out of the sequence above. This is your environment and not a \
             limitation of your terminal; `unset {NO_COLOR}` to allow colour. Everything that is \
             not a colour still applies, because NO_COLOR asks for output without colour rather \
             than output without styling.",
            // The value is the user's own data and is quoted back at them, so it goes
            // through the sanitiser like any other untrusted text.
            preference
                .explanation()
                .map_or_else(|| format!("{NO_COLOR} is set"), |text| sanitize_text(&text),),
            match names.as_slice() {
                [one] => format!("the {one} colour was"),
                many => format!("the {} colours were", many.join(", ")),
            }
        ));
    }

    if !text_is_clean(text) {
        notes.push(format!(
            "Control bytes in the text were replaced with their Control Pictures glyphs: `{}`. The \
             text sits between the two sequences, so an ESC in it could have cancelled the \
             styling, recoloured the rest of the line, or cleared the screen.",
            pretty(sanitized)
        ));
    }

    if sanitized.is_empty() && !rendition.is_empty() {
        notes.push(
            "The text is empty, so this styles no cells at all — the sequences would be written \
             and nothing would show. Pass some text, or pipe some in."
                .to_owned(),
        );
    }

    if attrs.contains(&Attr::Underline(UnderlineShape::None))
        && attrs
            .iter()
            .any(|attr| attr.role() == Some(ColorRole::Underline))
    {
        notes.push(
            "An underline colour was given together with `--underline=none`, so there is no line \
             for it to colour. `SGR 58` is still sent and still remembered by the terminal, which \
             is why the reset includes `SGR 59`."
                .to_owned(),
        );
    }

    for attr in attrs {
        if let Attr::Underline(shape) = attr
            && !shape.has_portable_spelling()
        {
            notes.push(format!(
                "`{}` needs a terminal that parses SGR sub-parameters. One that does not draws an \
                 ordinary underline instead: the shape is lost, the text is not, and nothing is \
                 corrupted. These shapes were originated by mintty, and xterm implements none of \
                 them.",
                shape.params()
            ));
        }
    }

    if attrs.contains(&Attr::RapidBlink) {
        notes.push(
            "`SGR 6` is \"rapidly blinking\" in ECMA-48, and almost every terminal treats it as \
             `SGR 5`. `escapade style probe --rapid-blink` can tell the difference where the \
             terminal answers an attribute readback, because a terminal that folded it reports \
             `5` rather than `6`."
                .to_owned(),
        );
    }

    if attrs.contains(&Attr::Conceal) {
        notes.push(
            "`SGR 8` hides the glyphs and nothing else. The characters are still in the cells, \
             still selectable and still copied, so this is a way to avoid *showing* something and \
             never a way to keep it secret."
                .to_owned(),
        );
    }

    if attrs.contains(&Attr::Blink) {
        notes.push(
            "Blinking text is a genuine accessibility problem — flashing content is a \
             photosensitivity trigger — and a number of terminals decline to animate `SGR 5` for \
             that reason. Treat it as decorative, never as the only way something is marked."
                .to_owned(),
        );
    }

    if let Some(Attr::Color(role, color)) = attrs
        .iter()
        .copied()
        .find(|attr| matches!(attr, Attr::Color(_, Color::Rgb(..))))
    {
        notes.push(format!(
            "The {} is 24-bit ({}). A terminal without truecolor support tends to *accept* this \
             and quantise it to the nearest palette entry rather than ignore it, so the failure \
             mode is a colour that is nearly right — which is exactly what `escapade style probe \
             {} '{}'` measures.",
            role.name(),
            color.describe(),
            role.flag(),
            color.describe()
        ));
    }

    if !stdout().is_terminal() && !rendition.is_empty() {
        notes.push(
            "stdout is not a terminal. The sequence would still be written, which is what you want \
             when piping into `less -R` and not what you want when capturing output to a log."
                .to_owned(),
        );
    }

    notes
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::features::style::attrs::PROBE_RGB;

    /// Notes for a request, with the text sanitised the way `explain` does it.
    fn notes(rendition: &Rendition, text: &str) -> Vec<String> {
        collect_notes(
            rendition,
            &[],
            &ColorPreference::allowed(),
            text,
            &sanitize_text(text),
        )
    }

    #[test]
    fn an_extended_underline_shape_is_flagged_as_needing_sub_parameter_support() {
        for shape in UnderlineShape::EXTENDED {
            let rendition = Rendition::new().with(Attr::Underline(*shape));
            let found = notes(&rendition, "word");
            assert!(
                found.iter().any(|note| note.contains("sub-parameters")),
                "{}: {found:?}",
                shape.name()
            );
            assert!(
                found.iter().any(|note| note.contains("mintty")),
                "the provenance is the interesting part: {found:?}"
            );
        }
    }

    #[test]
    fn a_plain_underline_needs_no_such_warning() {
        let rendition = Rendition::new().with(Attr::Underline(UnderlineShape::Single));
        assert!(
            !notes(&rendition, "word")
                .iter()
                .any(|note| note.contains("sub-parameters"))
        );
    }

    #[test]
    fn rapid_blink_conceal_and_blink_each_get_the_note_they_deserve() {
        for (attr, wanted) in [
            (Attr::RapidBlink, "folded"),
            (Attr::Conceal, "never a way to keep it secret"),
            (Attr::Blink, "photosensitivity"),
        ] {
            let rendition = Rendition::new().with(attr);
            let found = notes(&rendition, "word");
            assert!(
                found.iter().any(|note| note.contains(wanted)),
                "{}: {found:?}",
                attr.label()
            );
        }
    }

    #[test]
    fn truecolor_is_flagged_as_the_depth_that_fails_by_being_nearly_right() {
        let rendition = Rendition::new().with(Attr::Color(ColorRole::Foreground, PROBE_RGB));
        let found = notes(&rendition, "word");
        assert!(
            found.iter().any(|note| note.contains("quantise")),
            "{found:?}"
        );
        // A palette colour needs no such warning.
        let palette =
            Rendition::new().with(Attr::Color(ColorRole::Foreground, Color::Palette(202)));
        assert!(
            !notes(&palette, "word")
                .iter()
                .any(|note| note.contains("quantise"))
        );
    }

    #[test]
    fn an_underline_colour_with_no_line_to_draw_is_called_out() {
        let rendition = Rendition::new()
            .with(Attr::Underline(UnderlineShape::None))
            .with(Attr::Color(ColorRole::Underline, PROBE_RGB));
        assert!(
            notes(&rendition, "word")
                .iter()
                .any(|note| note.contains("no line for it to colour")),
            "{:?}",
            notes(&rendition, "word")
        );
    }

    #[test]
    fn sanitised_text_and_empty_text_are_both_reported() {
        let rendition = Rendition::new().with(Attr::Bold);
        let found = notes(&rendition, "a\u{1b}[2Jb");
        assert!(
            found.iter().any(|note| note.contains("Control Pictures")),
            "{found:?}"
        );

        let found = notes(&rendition, "");
        assert!(
            found.iter().any(|note| note.contains("no cells at all")),
            "{found:?}"
        );
        // …but an empty rendition with empty text has nothing to say about cells.
        let empty = Rendition::new();
        assert!(
            !notes(&empty, "")
                .iter()
                .any(|note| note.contains("no cells at all"))
        );
    }

    // -- NO_COLOR -----------------------------------------------------------

    #[test]
    fn the_no_color_note_leads_and_says_it_is_the_environment() {
        // Written for one specific reader: somebody with a stray NO_COLOR=1 who
        // cannot work out where their colours went.
        let rendition = Rendition::new().with(Attr::Bold);
        let dropped = vec![Attr::Color(ColorRole::Foreground, PROBE_RGB)];
        let found = collect_notes(
            &rendition,
            &dropped,
            &ColorPreference::from_value(Some("1")),
            "word",
            "word",
        );
        let first = found.first().expect("a note");
        assert!(first.contains("NO_COLOR"), "{first}");
        assert!(first.contains("your environment"), "{first}");
        assert!(
            first.contains("not a limitation of your terminal"),
            "{first}"
        );
        assert!(first.contains("foreground"), "{first}");
        assert!(
            first.contains("without colour rather than output without styling"),
            "{first}"
        );
    }

    #[test]
    fn nothing_dropped_means_no_no_color_note() {
        let rendition = Rendition::new().with(Attr::Color(ColorRole::Foreground, PROBE_RGB));
        assert!(
            !notes(&rendition, "word")
                .iter()
                .any(|note| note.contains("NO_COLOR"))
        );
    }

    #[test]
    fn the_dropped_message_reads_correctly_for_one_and_for_several() {
        assert_eq!(describe_dropped(&[]), "nothing was");
        assert_eq!(
            describe_dropped(&[Attr::Color(ColorRole::Foreground, PROBE_RGB)]),
            "the foreground colour was"
        );
        let three = [
            Attr::Color(ColorRole::Foreground, PROBE_RGB),
            Attr::Color(ColorRole::Background, PROBE_RGB),
            Attr::Color(ColorRole::Underline, PROBE_RGB),
        ];
        assert_eq!(
            describe_dropped(&three),
            "the foreground, background, underline-color colours were"
        );
    }

    // -- the reset explanation ---------------------------------------------

    #[test]
    fn the_shared_reset_is_explained_when_both_attributes_are_set() {
        let both = Rendition::new().with(Attr::Bold).with(Attr::Dim);
        let text = describe_resets(&both);
        assert!(text.contains("*both*"), "{text}");
        assert!(text.contains("stay faint"), "{text}");

        let blinks = Rendition::new().with(Attr::Blink).with(Attr::RapidBlink);
        assert!(describe_resets(&blinks).contains("both blinks"));
    }

    #[test]
    fn bold_alone_explains_why_it_is_not_twenty_one() {
        let bold = Rendition::new().with(Attr::Bold);
        let text = describe_resets(&bold);
        assert!(text.contains("21"), "{text}");
        assert!(text.contains("contested"), "{text}");
        assert!(text.contains("doubly underlined"), "{text}");
    }

    #[test]
    fn overline_explains_the_fifty_five_not_fifty_four_trap() {
        let rendition = Rendition::new().with(Attr::Overline);
        let text = describe_resets(&rendition);
        assert!(text.contains("55"), "{text}");
        assert!(text.contains("54"), "{text}");
        assert!(text.contains("encircled"), "{text}");
    }

    #[test]
    fn an_underline_colour_explains_that_thirty_nine_does_not_reset_it() {
        let rendition = Rendition::new().with(Attr::Color(ColorRole::Underline, PROBE_RGB));
        let text = describe_resets(&rendition);
        assert!(text.contains("59"), "{text}");
        assert!(text.contains("39"), "{text}");
    }

    // -- safety -------------------------------------------------------------

    #[test]
    fn every_description_is_safe_to_print() {
        for attr in crate::features::style::attrs::every_attribute() {
            let text = describe_attr(attr);
            assert!(!text.is_empty(), "{}", attr.label());
            assert!(!text.contains('\u{1b}'), "{}", attr.label());
        }
    }

    #[test]
    fn every_note_is_safe_to_print() {
        // The notes quote the user's own text back, so they are an injection path of
        // their own if the quoting is wrong.
        let hostile = "a\u{1b}[2J\u{7}\u{9b}0m";
        for attr in crate::features::style::attrs::every_attribute() {
            let rendition = Rendition::new().with(attr);
            for note in collect_notes(
                &rendition,
                &[attr],
                &ColorPreference::from_value(Some("\u{1b}[2J")),
                hostile,
                &sanitize_text(hostile),
            ) {
                assert!(!note.contains('\u{1b}'), "{note:?}");
                assert!(!note.contains('\u{7}'), "{note:?}");
                assert!(!note.contains('\u{9b}'), "{note:?}");
            }
        }
    }
}
