//! `escapade decode` — bytes back into named sequences.
//!
//! The inverse of the `demo` verb. Instead of showing a sequence and then its
//! effect, this takes an effect you already have — a byte stream that rendered
//! wrong — and names the sequences in it.
//!
//! ```console
//! $ printf '\e[1;31mred\e[0m' | escapade decode
//! CSI 1;31 m      SGR — bold, foreground red
//! "red"
//! CSI 0 m         SGR — reset all attributes
//! ```
//!
//! # What it is for
//!
//! 1. **Debugging your own output.** A TUI draws a corrupted border; capture its
//!    bytes and ask what it actually emitted, rather than squinting at `cat -v`.
//!    The answer is usually a sequence that was never terminated or a character
//!    set that was never switched back, and both are named here as such.
//! 2. **Auditing untrusted text** before printing it — log lines, filenames,
//!    branch names. This is the counterpart to the sanitizing in `hyperlink`,
//!    `notification` and `style`: `decode` shows *why* those sanitizers exist. It
//!    is safe to point at hostile input, because nothing it prints is a control
//!    byte and nothing invisible stays invisible.
//! 3. **Learning**, and as a diffable text form of a byte stream for regression
//!    tests.
//!
//! # Layout
//!
//! * [`parse`] — the grammar an application *emits*, which is not the grammar
//!   [`crate::input`] parses. Pure, incremental, and where most of the tests are.
//! * [`names`] and [`sgr`] — what the sequences are called and what they do.
//! * [`catalog`] — the same question answered from the feature database, so that
//!   `OSC 8` is the hyperlinks feature rather than prose written here.
//! * [`describe`] — one item, named: `term::pretty` for the introducers, the
//!   database for attribution, the tables for the rest.
//! * [`display`] — the escaping every byte passes through on its way out.
//! * [`render`], [`json`] and [`annotate`] — the two output formats and the
//!   `--unicode` breakdown.
//!
//! # Why there is no data file
//!
//! Every *feature* subcommand has one, plus `explain`, `probe` and `demo`.
//! `decode` has none of that on purpose: it is a tool rather than a terminal
//! capability, like `doctor`. It sends nothing, asks the terminal nothing, and
//! works with output redirected — so there is no probe to write, and nothing for a
//! support matrix to be about.
//!
//! # Prior art
//!
//! GNU `teseq` is the only comparable tool, and it is dormant: it predates every
//! protocol this project covers, so it names the VT100-era sequences and passes
//! the rest through as unlabelled bytes. That gap is the reason [`catalog`]
//! attributes a sequence to a feature in the database rather than to a table
//! written here — the naming grows as the database does.

mod annotate;
mod catalog;
mod describe;
mod display;
mod json;
mod names;
mod parse;
mod render;
mod sgr;

use std::{
    fs::File,
    io::{self, BufWriter, IsTerminal as _, Read, Write},
    path::Path,
    process::ExitCode,
};

use anyhow::{Context as _, Result, bail};

use self::{catalog::Index, parse::Decoder};
use crate::{
    cli::{DecodeCommand, OutputFormat},
    db::Catalog,
    theme::Theme,
};

/// How much is read at a time.
///
/// Decoding is incremental, so this only decides how often output appears when
/// watching a live pipeline; a sequence split across two reads is held until the
/// rest of it arrives.
const CHUNK: usize = 8 * 1024;

/// What the flags asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// `--hex`: byte offsets and the bytes themselves.
    pub hex: bool,
    /// `--unicode`: codepoints, categories, widths and cluster boundaries for the
    /// text between sequences.
    pub unicode: bool,
    /// `--sequences-only`: drop the runs of text.
    pub sequences_only: bool,
}

/// Entry point for `escapade decode`.
///
/// # Errors
///
/// Fails if the file cannot be opened, if the input cannot be read, or if the
/// feature database will not load. A closed output pipe is not a failure —
/// `escapade decode … | head` is a normal thing to do — so it ends quietly.
pub fn run(catalog: &Catalog, command: &DecodeCommand, no_color: bool) -> Result<ExitCode> {
    let options = Options {
        hex: command.hex,
        unicode: command.unicode,
        sequences_only: command.sequences_only,
    };
    // Descriptions come from the database wherever it has them, so a data file
    // gaining a sequence teaches `decode` a new one with no code change.

    let index = Index::from_catalog(catalog);
    let theme = Theme::new(no_color);

    let mut input = open(command.file.as_deref())?;
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    match decode(
        &mut input,
        &mut out,
        &theme,
        options,
        command.format,
        &index,
    ) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        // Whatever was consuming the output has gone away, which is what `head`
        // does. Reporting it as an error would be noise.
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(ExitCode::SUCCESS),
        Err(error) => Err(error).context("decoding"),
    }
}

/// Opens the input: a file, or standard input for `-` or for no argument at all.
///
/// The mistake worth catching is the bare invocation at a prompt, which would
/// otherwise block forever with no indication of why — the same trap
/// [`crate::features::bytes_operand`] guards against.
///
/// # Errors
///
/// Fails if the file cannot be opened, or if there is no input to read.
fn open(file: Option<&Path>) -> Result<Box<dyn Read>> {
    match file {
        Some(path) if path != Path::new("-") => {
            let opened = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            Ok(Box::new(opened))
        }
        Some(_) => Ok(Box::new(io::stdin())),
        None => {
            if io::stdin().is_terminal() {
                bail!(
                    "no input — pipe a byte stream in, name a FILE, or pass `-` to read \
                     standard input even from a terminal"
                );
            }
            Ok(Box::new(io::stdin()))
        }
    }
}

/// Decodes everything `input` produces into `out`.
///
/// Separated from [`run`] so it can be driven from a byte slice, with no terminal
/// and no files: everything below this point is pure apart from the writing.
fn decode(
    input: &mut impl Read,
    out: &mut impl Write,
    theme: &Theme,
    options: Options,
    format: OutputFormat,
    index: &Index,
) -> io::Result<()> {
    let mut decoder = Decoder::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        for decoded in decoder.feed(&buffer[..read]) {
            emit(out, theme, options, format, index, &decoded)?;
        }
        // Flushed per chunk, so watching a live program shows something before
        // the program exits.
        out.flush()?;
    }
    // Whatever is still open at the end is reported as unterminated rather than
    // dropped: a truncated capture is ordinary input, and the truncation is
    // usually the finding.
    for decoded in decoder.finish() {
        emit(out, theme, options, format, index, &decoded)?;
    }
    out.flush()
}

/// Writes one item in the requested format.
fn emit(
    out: &mut impl Write,
    theme: &Theme,
    options: Options,
    format: OutputFormat,
    index: &Index,
    decoded: &parse::Decoded,
) -> io::Result<()> {
    if options.sequences_only && decoded.item.is_text() {
        return Ok(());
    }
    let description = describe::describe(decoded, index);
    match format {
        OutputFormat::Text => render::item(out, theme, options, decoded, &description),
        OutputFormat::Json => out.write_all(json::line(decoded, &description, options).as_bytes()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A reader that hands out a few bytes at a time, so the streaming path is
    /// what a test exercises rather than one convenient read.
    #[derive(Debug)]
    struct Dribble<'a> {
        /// What is left to hand out.
        rest: &'a [u8],
        /// Bytes per read.
        step: usize,
    }

    impl Read for Dribble<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let take = self.step.min(self.rest.len()).min(buffer.len());
            buffer[..take].copy_from_slice(&self.rest[..take]);
            self.rest = &self.rest[take..];
            Ok(take)
        }
    }

    fn options() -> Options {
        Options {
            hex: false,
            unicode: false,
            sequences_only: false,
        }
    }

    /// Runs the whole pipeline over a byte slice.
    fn decoded(bytes: &[u8], options: Options, format: OutputFormat, step: usize) -> String {
        let catalog = Catalog::load().unwrap();
        let index = Index::from_catalog(&catalog);
        let theme = Theme::plain();
        let mut input = Dribble {
            rest: bytes,
            step: step.max(1),
        };
        let mut out = Vec::new();
        decode(&mut input, &mut out, &theme, options, format, &index).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_first_example_in_the_help_text() {
        // The feature attribution is read from the database, so that editing a
        // data file cannot fail this for the wrong reason.
        let attributes = Catalog::load()
            .unwrap()
            .feature("text-attributes")
            .unwrap()
            .name
            .clone();
        let output = decoded(
            b"\x1b[1;31mred\x1b[0m",
            options(),
            OutputFormat::Text,
            CHUNK,
        );
        assert_eq!(
            output,
            format!(
                "CSI 1;31 m      SGR — bold, foreground red [{attributes}]\n\
                 \"red\"\n\
                 CSI 0 m         SGR — reset all attributes [{attributes}]\n"
            )
        );
    }

    #[test]
    fn a_few_bytes_at_a_time_decode_to_exactly_the_same_thing() {
        // The straddling case, end to end: a sequence split across reads must not
        // change the output, and neither must a run of text.
        let stream = b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x07\x1b[38:2::1:2:3mx\n";
        let whole = decoded(stream, options(), OutputFormat::Text, CHUNK);
        for step in 1..=8 {
            assert_eq!(
                decoded(stream, options(), OutputFormat::Text, step),
                whole,
                "reading {step} bytes at a time changed the output"
            );
        }
    }

    #[test]
    fn json_is_one_object_per_item_per_line() {
        let output = decoded(
            b"\x1b[1;31mred\x1b[0m",
            options(),
            OutputFormat::Json,
            CHUNK,
        );
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in lines {
            assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
        }
    }

    #[test]
    fn an_unterminated_sequence_at_the_end_of_the_input_is_reported() {
        let output = decoded(b"\x1b]8;;broken", options(), OutputFormat::Text, CHUNK);
        assert!(output.contains("unterminated OSC"), "{output}");
        // And in JSON, where a script can select on it.
        let json = decoded(b"\x1b]8;;broken", options(), OutputFormat::Json, 1);
        assert!(json.contains("\"kind\":\"unterminated\""), "{json}");
    }

    #[test]
    fn invalid_utf8_does_not_stop_the_decode() {
        let output = decoded(
            b"before\xffafter\x1b[0m",
            options(),
            OutputFormat::Text,
            CHUNK,
        );
        assert!(output.contains("\"before\""), "{output}");
        assert!(output.contains("not valid UTF-8"), "{output}");
        assert!(output.contains("\"after\""), "{output}");
        assert!(output.contains("SGR"), "{output}");
    }

    #[test]
    fn every_flag_combination_runs_and_stays_safe_to_print() {
        let stream = "text \u{1f468}\u{200d}\u{1f469}\u{1b}[1;31m\u{1b}]8;;https://x\u{1b}\\\
                      \u{7}\u{1b}_Ga=T;AAAA\u{1b}\\\u{1b}]0;broken"
            .as_bytes();
        for hex in [false, true] {
            for unicode in [false, true] {
                for sequences_only in [false, true] {
                    for format in [OutputFormat::Text, OutputFormat::Json] {
                        let options = Options {
                            hex,
                            unicode,
                            sequences_only,
                        };
                        let output = decoded(stream, options, format, 3);
                        assert!(!output.is_empty());
                        for character in output.chars() {
                            assert!(
                                !character.is_control() || character == '\n',
                                "control byte in output for {options:?} {format:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn nothing_is_dropped_or_double_counted() {
        // Every byte of the input is accounted for by exactly one item, which is
        // what makes the `--hex` offsets trustworthy.
        let stream = b"a\x1b[1mb\x07\x1b]0;t\x1b\\\xff\x1b_G\x1b\\\x1bP$qm\x1b\\\x1b[";
        let json = decoded(stream, options(), OutputFormat::Json, 5);
        let mut expected = 0usize;
        for line in json.lines() {
            let offset: usize = between(line, "\"offset\":", ",").parse().unwrap();
            let length: usize = between(line, "\"length\":", ",").parse().unwrap();
            assert_eq!(offset, expected, "{line}");
            expected += length;
        }
        assert_eq!(expected, stream.len());
    }

    /// The text between two markers, for reading a field out of a JSON line.
    fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
        let from = text.find(start).unwrap() + start.len();
        let rest = &text[from..];
        &rest[..rest.find(end).unwrap()]
    }
}
