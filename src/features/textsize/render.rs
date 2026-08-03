//! Drawing text: the top-level form of `escapade textsize`.
//!
//! ```console
//! $ escapade textsize --scale 4 'BUILD PASSED'
//! ```
//!
//! One sequence goes out, `OSC 66 ; s=4 ; BUILD PASSED ST`, and the terminal
//! draws twelve characters four cells tall — which still copy as twelve
//! characters.
//!
//! # Why this measures before it draws
//!
//! An `OSC` string a terminal does not recognise is parsed to its terminator and
//! **discarded, payload included**. So on a terminal without text sizing, this
//! command would print nothing at all: not smaller text, not unsized text —
//! nothing. That is not a graceful degradation, and it is not something the
//! protocol can fix from the sending side, because there is no fallback text
//! anywhere in the sequence.
//!
//! So when there is a terminal to ask, [`super::probe`] measures first, and if
//! the measurement shows the string being thrown away the text is printed
//! plainly instead. That costs one round trip of a few milliseconds and turns
//! silence into output. When there is nothing to ask — output redirected to a
//! file or a pipe — the sequence is written as-is, because then the bytes are
//! data and their consumer decides what to do with them.

use std::io::{IsTerminal as _, Write as _, stdout};

use anyhow::Result;

use crate::{
    cli::TextsizeCommand,
    features::{
        Support,
        textsize::{
            Key, MAX_PAYLOAD, Metadata, SizedText, metadata_from_command, probe, text_is_clean,
        },
        wrap,
    },
    term::pretty,
    theme::{Ink, Theme},
};

/// Draws the text the command line asked for.
///
/// # Errors
///
/// Fails if the flags describe a sequence the protocol cannot express, if the
/// text cannot be carried in one escape code, or if stdout cannot be written to.
pub fn run(theme: &Theme, command: &TextsizeCommand) -> Result<()> {
    let text = command.text.join(" ");
    let metadata = metadata_from_command(command)?;
    let sized = SizedText::new(metadata, &text)?;

    // Worth saying out loud rather than doing quietly: the text on screen will
    // not be the text that was passed in.
    let mut warnings = sized.warnings();
    if !text_is_clean(&text) {
        warnings.push(
            "the text contained control characters, which cannot survive inside an OSC string — \
             each was replaced with its visible Control Pictures glyph"
                .to_owned(),
        );
    }

    if command.dry_run {
        describe(theme, &sized, &warnings);
        return Ok(());
    }

    for warning in &warnings {
        for line in wrap(warning, theme.width().saturating_sub(2)) {
            eprintln!("{}", theme.paint(&line, Ink::Warn));
        }
    }

    // Only bother measuring when there is a terminal on both ends *and* the
    // output is going to a screen. A pipe gets the sequence verbatim.
    if stdout().is_terminal() {
        let outcome = probe::detect(&keys_used(sized.metadata()))?;
        if outcome.text_would_be_lost() {
            eprintln!(
                "{}",
                theme.paint(
                    "This terminal discards OSC 66 rather than ignoring it, so the sized text \
                     would not appear at all. Printing it plainly instead.",
                    Ink::Warn
                )
            );
            println!("{}", sized.text());
            return Ok(());
        }
        for key in keys_used(sized.metadata()) {
            if outcome.support_for(key) == Support::No && !outcome.text_would_be_lost() {
                eprintln!(
                    "{}",
                    theme.paint(
                        &format!(
                            "This terminal parses OSC 66 but does not act on `{}=`, so the text \
                             will appear at the ordinary size.",
                            key.letter()
                        ),
                        Ink::Warn
                    )
                );
            }
        }
    }

    emit(&sized)
}

/// Writes the sequence, then enough newlines to clear the block it drew.
///
/// The cursor ends up on the *first* row of a scaled block, because the
/// specification has it move `s * w` cells to the right in the row it was
/// already in. Printing a single newline after `s=4` would put the shell prompt
/// three rows inside the banner.
fn emit(sized: &SizedText) -> Result<()> {
    let mut out = stdout().lock();
    out.write_all(sized.sequence().as_bytes())?;
    // One newline per row the block occupies.
    for _ in 0..sized.metadata().scale() {
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

/// Which keys this request depends on, cheapest first.
///
/// `w` leads because measuring it needs only one row, and because its verdict
/// doubles as the answer to "does any text sent through `OSC 66` survive here".
/// That is also why a request using neither `w` nor `s` — a bare
/// `escapade textsize 'hello'`, or one that only sets a fraction — still probes
/// `w`: the question is not whether the sizing works, it is whether the text
/// arrives.
fn keys_used(metadata: Metadata) -> Vec<Key> {
    let mut keys = Vec::with_capacity(2);
    if metadata.width() > 0 {
        keys.push(Key::Width);
    }
    if metadata.scale() > 1 {
        keys.push(Key::Scale);
    }
    if keys.is_empty() {
        keys.push(Key::Width);
    }
    keys
}

/// Prints the sequence and names every piece of it, for `--dry-run`.
#[expect(
    clippy::too_many_lines,
    reason = "one screen of output, written top to bottom, plus one entry per \
              metadata key; splitting it would scatter the layout without \
              simplifying anything"
)]
fn describe(theme: &Theme, sized: &SizedText, warnings: &[String]) {
    let metadata = sized.metadata();

    println!();
    println!("  {}", theme.paint(&pretty(&sized.sequence()), Ink::Code));
    println!();

    let mut parts: Vec<(String, String)> = vec![
        (
            "OSC".to_owned(),
            "Operating System Command, ESC ]. A terminal that does not implement command 66 \
             parses this string to its terminator and discards it — text included — so nothing \
             appears at all rather than appearing unsized."
                .to_owned(),
        ),
        (
            super::COMMAND.to_owned(),
            "the command number for text sizing.".to_owned(),
        ),
        (
            ";".to_owned(),
            "ends the command number. The metadata starts here: colon-separated `key=value` \
             pairs, and colon only — a comma is an error, unlike in the kitty graphics protocol."
                .to_owned(),
        ),
    ];

    if metadata.is_empty() {
        parts.push((
            "(none)".to_owned(),
            "no metadata: the terminal splits the text into cells exactly as it would without \
             this protocol. Legal, and occasionally useful — it is `w=0:s=1`."
                .to_owned(),
        ));
    }
    if metadata.scale() > 1 {
        let scale = metadata.scale();
        parts.push((
            format!("s={scale}"),
            format!(
                "scale: {scale} times the base font size, drawn in a block {scale} cells tall. \
                 Change the terminal's font size and this changes with it."
            ),
        ));
    }
    if metadata.width() > 0 {
        let width = metadata.width();
        parts.push((
            format!("w={width}"),
            format!(
                "width: all the text in this one escape code renders inside s*w = {} cells. This \
                 is the key that ends the width disagreement — the application decides, so the \
                 terminal cannot disagree.",
                usize::from(metadata.scale()) * usize::from(width)
            ),
        ));
    }
    if let Some(fraction) = metadata.fraction() {
        parts.push((
            format!("n={}:d={}", fraction.numerator(), fraction.denominator()),
            format!(
                "fractional scale: the font is drawn at {} of the size it otherwise would be. It \
                 does *not* change how many cells the text occupies — that is what makes \
                 superscripts possible.",
                fraction.label()
            ),
        ));
    }
    if let Some(align) = metadata.valign() {
        parts.push((
            format!("v={}", super::valign_value(align)),
            format!(
                "vertical alignment: the fractionally scaled area sits at the {} of the block.",
                align_word(super::valign_value(align), "top", "bottom")
            ),
        ));
    }
    if let Some(align) = metadata.halign() {
        parts.push((
            format!("h={}", super::halign_value(align)),
            format!(
                "horizontal alignment: the fractionally scaled area sits at the {} of the block.",
                align_word(super::halign_value(align), "left", "right")
            ),
        ));
    }

    parts.push((
        ";".to_owned(),
        "ends the metadata. Everything from here to the terminator is the text.".to_owned(),
    ));
    parts.push((
        sized.text().to_owned(),
        format!(
            "the payload: {} bytes of it, of at most {MAX_PAYLOAD}. Control characters were \
             replaced with their Control Pictures glyphs on the way in, because any one of them \
             could have ended this string early.",
            sized.text().len()
        ),
    ));
    parts.push((
        "ST".to_owned(),
        "String Terminator, ESC \\. A BEL would also be legal, and is what the specification's \
         own examples use."
            .to_owned(),
    ));

    let token_width = parts
        .iter()
        .map(|(token, _)| token.chars().count())
        .max()
        .unwrap_or(0)
        .min(24);
    for (token, meaning) in &parts {
        let label = format!("{token:<token_width$}");
        let lines = wrap(meaning, theme.width().saturating_sub(token_width + 6));
        for (index, line) in lines.iter().enumerate() {
            if index == 0 {
                println!(
                    "  {}  {}",
                    theme.paint(&label, Ink::Accent),
                    theme.paint(line, Ink::Muted)
                );
            } else {
                println!(
                    "  {}  {}",
                    " ".repeat(token_width),
                    theme.paint(line, Ink::Muted)
                );
            }
        }
    }

    println!();
    let rows = metadata.scale();
    let rows = if rows == 1 {
        "1 row".to_owned()
    } else {
        format!("{rows} rows")
    };
    let geometry = match metadata.block() {
        Some((width, _)) => format!(
            "the cursor advances exactly {width} cells, and the block is {width} cells by {rows}"
        ),
        None => format!(
            "the cursor advances {} cells if the terminal counts this text the way we do, and the \
             block is {rows} tall. Pass --width to make the advance exact.",
            sized.predicted_cells(),
        ),
    };
    for line in wrap(&geometry, theme.width().saturating_sub(4)) {
        println!("  {}", theme.paint(&line, Ink::Strong));
    }

    for warning in warnings {
        println!();
        for line in wrap(warning, theme.width().saturating_sub(4)) {
            println!("  {}", theme.paint(&line, Ink::Warn));
        }
    }

    println!();
    println!(
        "  {}",
        theme.paint("Nothing was sent: this was --dry-run.", Ink::Muted)
    );
    println!();
}

/// The word for an alignment value, given what `0` and `1` mean for that axis.
fn align_word(value: u8, low: &'static str, high: &'static str) -> &'static str {
    match value {
        0 => low,
        1 => high,
        _ => "centre",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{
        cli::{HorizontalAlign, VerticalAlign},
        features::textsize::Fraction,
    };

    fn command(
        text: &str,
        scale: Option<u8>,
        width: Option<u8>,
        fraction: Option<&str>,
    ) -> TextsizeCommand {
        TextsizeCommand {
            action: None,
            text: text.split(' ').map(str::to_owned).collect(),
            scale,
            width,
            fraction: fraction.map(str::to_owned),
            valign: None,
            halign: None,
            dry_run: true,
        }
    }

    #[test]
    fn words_are_joined_with_spaces() {
        let command = command("BUILD PASSED", Some(4), None, None);
        let text = command.text.join(" ");
        let metadata = metadata_from_command(&command).unwrap();
        let sized = SizedText::new(metadata, &text).unwrap();
        assert_eq!(sized.text(), "BUILD PASSED");
        assert_eq!(sized.sequence(), "\u{1b}]66;s=4;BUILD PASSED\u{1b}\\");
    }

    #[test]
    fn a_request_probes_the_keys_it_depends_on() {
        let scaled = Metadata::new().with_scale(4).unwrap();
        assert_eq!(keys_used(scaled), vec![Key::Scale]);

        let wide = Metadata::new().with_width(2).unwrap();
        assert_eq!(keys_used(wide), vec![Key::Width]);

        let both = scaled.with_width(2).unwrap();
        assert_eq!(
            keys_used(both),
            vec![Key::Width, Key::Scale],
            "width first: one row to measure instead of two"
        );
    }

    #[test]
    fn a_request_using_no_measurable_key_still_checks_that_text_survives() {
        // Neither `s` nor `w` is set, but the sequence can still vanish whole on
        // a terminal that does not know OSC 66 — so `w` is measured as a canary.
        assert_eq!(keys_used(Metadata::new()), vec![Key::Width]);
        let fraction_only = Metadata::new().with_fraction(Fraction::new(1, 2).unwrap());
        assert_eq!(keys_used(fraction_only), vec![Key::Width]);
    }

    #[test]
    fn the_dry_run_description_never_prints_a_control_byte() {
        // `describe` writes to stdout, so the assertion here is on the pieces it
        // is built from: the sequence goes through `pretty`, and the text has
        // already been sanitised.
        let metadata = Metadata::new()
            .with_scale(3)
            .unwrap()
            .with_width(2)
            .unwrap()
            .with_fraction(Fraction::new(1, 2).unwrap())
            .with_valign(VerticalAlign::Bottom)
            .with_halign(HorizontalAlign::Center);
        let sized = SizedText::new(metadata, "a\u{1b}b").unwrap();
        let rendered = pretty(&sized.sequence());
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.starts_with("OSC 66;s=3:w=2:n=1:d=2:v=1:h=2;"));
        assert!(rendered.ends_with("ST"));
    }

    #[test]
    fn alignment_words_follow_the_wire_values() {
        assert_eq!(align_word(0, "top", "bottom"), "top");
        assert_eq!(align_word(1, "top", "bottom"), "bottom");
        assert_eq!(align_word(2, "top", "bottom"), "centre");
        assert_eq!(align_word(0, "left", "right"), "left");
        assert_eq!(align_word(1, "left", "right"), "right");
    }

    #[test]
    fn describe_runs_over_every_shape_of_request_without_panicking() {
        // The layout arithmetic — column widths, wraps, the geometry line — is
        // easy to get wrong for the empty-metadata case, which has no keys at
        // all, and for the widest one.
        let theme = Theme::plain();
        for (scale, width, fraction) in [
            (None, None, None),
            (Some(7), None, None),
            (None, Some(7), None),
            (Some(2), Some(3), Some("1/2")),
        ] {
            let command = command("text here", scale, width, fraction);
            let metadata = metadata_from_command(&command).unwrap();
            let sized = SizedText::new(metadata, &command.text.join(" ")).unwrap();
            describe(&theme, &sized, &sized.warnings());
        }
    }
}
