//! OSC 8 hyperlinks: build them, and build them safely.
//!
//! A hyperlink is an Operating System Command string carrying a URI:
//!
//! ```text
//! OSC 8 ; <params> ; <URI> ST   <visible text>   OSC 8 ; ; ST
//! ```
//!
//! where `OSC` is `ESC ]` and `ST` is `ESC \`. Note the two semicolons: the
//! first ends the command number `8`, the second ends the (usually empty)
//! parameter field. Closing a link is the same sequence with the URI left out,
//! because an empty URI means "no link".
//!
//! It is a cell attribute, like bold or a foreground colour — not a container.
//! Opening a link while one is already open replaces it; closing one that was
//! never open does nothing. There is no nesting.
//!
//! A terminal that does not implement OSC 8 still parses the OSC string per
//! ECMA-48, discards it, and prints the text — so hyperlinks are safe to emit
//! unconditionally. That makes the interesting work here *sanitisation* rather
//! than capability detection:
//!
//! * [`sanitize_uri`] percent-encodes everything that could end the OSC string
//!   early or confuse a terminal's parameter splitting.
//! * [`sanitize_text`] neutralises control bytes in the *visible* text, which is
//!   outside the OSC string and would otherwise be free to close the link or
//!   open a different one.
//!
//! Specification: <https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda>

pub mod demo;
pub mod emit;
pub mod probe;

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::{
    cli::{HyperlinkAction, HyperlinkCommand},
    db::Catalog,
    term::ST,
    theme::Theme,
};

/// The feature id in the database. The subcommand is `hyperlink`.
pub const FEATURE_ID: &str = "hyperlinks";

/// Operating System Command (`ESC ]`) and `BEL` (0x07), re-exported from
/// [`crate::term`] so that a reader of this module has the whole wire vocabulary
/// of a hyperlink — `OSC`, `ST`, `BEL` — in one place.
pub use crate::term::{BEL, OSC};

/// The OSC command number hyperlinks live under.
pub const COMMAND: &str = "8";

/// Longest URI VTE and iTerm2 accept, in bytes. Past this the link is dropped
/// and only the text survives.
pub const MAX_URI_LEN: usize = 2083;

/// Longest `id` VTE accepts, in bytes. iTerm2 imposes no limit.
pub const MAX_ID_LEN: usize = 250;

/// Which string terminator to end the OSC sequence with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Terminator {
    /// `ST` — `ESC \`. The standard, and the default.
    #[default]
    St,
    /// `BEL` — 0x07. xterm's spelling: one byte shorter, and an audible bell on
    /// any terminal that does not recognise the OSC.
    Bel,
}

impl Terminator {
    /// The bytes to emit.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::St => ST,
            Self::Bel => BEL,
        }
    }

    /// The name used when showing the sequence to a reader.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::St => "ST",
            Self::Bel => "BEL",
        }
    }
}

// -- sanitisation -----------------------------------------------------------

/// First printable ASCII byte a URI may carry verbatim. `0x20` (space) and
/// everything below it is a control byte or forbidden in a URI.
const FIRST_PRINTABLE: u8 = 0x21;

/// Last printable ASCII byte; `0x7f` is DEL.
const LAST_PRINTABLE: u8 = 0x7e;

/// Printable ASCII that must still be percent-encoded in a URI.
///
/// `"`, `<`, `>`, `` ` ``, `\`, `^`, `{`, `}` and `|` are excluded from URIs by
/// RFC 3986 in the first place. `;` is legal in a URI and legal in the OSC 8
/// payload — the URI is everything after the second semicolon — but a terminal
/// that splits the payload on *every* semicolon would truncate the target, and
/// `%3B` costs nothing.
const MUST_ENCODE: &[u8] = b";\"<>`\\^{}|";

/// Appends `byte` as a percent-escape, upper-case as RFC 3986 prefers.
fn percent_encode(out: &mut String, byte: u8) {
    const HEX: [u8; 16] = *b"0123456789ABCDEF";
    out.push('%');
    out.push(char::from(HEX[usize::from(byte >> 4)]));
    out.push(char::from(HEX[usize::from(byte & 0x0f)]));
}

/// Whether `byte` has to be percent-encoded on its way into an OSC 8 URI.
#[must_use]
pub fn uri_byte_needs_encoding(byte: u8) -> bool {
    !(FIRST_PRINTABLE..=LAST_PRINTABLE).contains(&byte) || MUST_ENCODE.contains(&byte)
}

/// Percent-encodes `input` so it is safe inside `OSC 8 ; … ; <URI> ST`.
///
/// Every byte outside the printable ASCII range 0x21–0x7E is encoded, as the
/// specification requires, along with the handful of printable bytes listed in
/// `MUST_ENCODE`. That is what makes an `ESC` or a `BEL` smuggled into a URI
/// harmless: it becomes the three characters `%1B`, which cannot end the OSC
/// string early or start a sequence of their own.
///
/// Non-ASCII input is encoded byte by byte, so UTF-8 becomes the percent-encoded
/// UTF-8 a URI is supposed to carry.
///
/// `%` is deliberately *not* encoded. The argument is a URI, not an arbitrary
/// string, so it may already contain escapes; encoding `%` again would turn
/// `%20` into `%2520` and break every already-correct URI passed in.
///
/// # Errors
///
/// Fails on the empty string. `OSC 8 ; ; ST` — an OSC 8 sequence with an empty
/// URI — is the sequence that *closes* a link, so an empty target cannot be
/// expressed as an open sequence at all. Silently emitting a close would produce
/// text that looks like a link and is not one, which is worse than an error.
pub fn sanitize_uri(input: &str) -> Result<String> {
    if input.is_empty() {
        bail!(
            "the link target is empty, and an OSC 8 sequence with an empty URI is the sequence \
             that closes a link — pass a real target, or print plain text instead"
        );
    }
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        if uri_byte_needs_encoding(byte) {
            percent_encode(&mut out, byte);
        } else {
            out.push(char::from(byte));
        }
    }
    Ok(out)
}

/// Checks that `input` can serve as an OSC 8 parameter value, returning it
/// unchanged.
///
/// Unlike a URI, a bad `id` is **rejected** rather than encoded. An id is an
/// opaque token whose only job is to compare equal to the ids on other runs of
/// cells, possibly produced by a different program in the same pipeline. Quietly
/// rewriting it would break that comparison in a way nothing on screen would
/// explain, so a bad id is an error the caller can see and fix.
///
/// # Errors
///
/// Fails if `input` is empty (only a *non-empty* id groups cells, so an empty one
/// is indistinguishable from passing none), if it is longer than [`MAX_ID_LEN`]
/// (VTE truncates there, and two ids differing only past the cut would silently
/// start hovering as one link), if it contains `;` or `:` (the delimiters of the
/// parameter field), or if it contains any byte outside printable ASCII.
pub fn sanitize_id(input: &str) -> Result<String> {
    if input.is_empty() {
        bail!(
            "the link id is empty; only a non-empty id groups cells into one link, so an empty \
             one is indistinguishable from passing no id at all"
        );
    }
    if input.len() > MAX_ID_LEN {
        bail!(
            "the link id is {} bytes; VTE truncates ids at {MAX_ID_LEN}, so two ids differing \
             only past that point would silently hover as one link",
            input.len()
        );
    }
    for &byte in input.as_bytes() {
        match byte {
            b';' => bail!(
                "the link id contains `;`, which ends the parameter field of \
                 `OSC 8 ; params ; URI ST`"
            ),
            b':' => bail!("the link id contains `:`, which separates one parameter from the next"),
            0x00..=0x1f | 0x7f..=0xff => bail!(
                "the link id contains byte 0x{byte:02x}; OSC 8 parameters must stay inside \
                 printable ASCII (32–126)"
            ),
            _ => {}
        }
    }
    Ok(input.to_owned())
}

/// Neutralises the control bytes in a link's *visible* text.
///
/// The text sits between the open and close sequences, outside the OSC string,
/// so it is ordinary terminal output. Untrusted text is therefore free to carry
/// its own `OSC 8 ; ; ST` and close the link early, open a different link over
/// the rest of the line, or do anything else an escape sequence can do.
///
/// Percent-encoding is not an option here — the caller asked for this text to be
/// *read* — so every byte that could introduce or terminate an escape sequence is
/// replaced by its glyph from the Unicode Control Pictures block: `ESC` becomes
/// `␛`, `BEL` becomes `␇`. Those are inert, one cell wide and visible, so a
/// filename with an `ESC` in it looks suspicious instead of executing.
///
/// `\n` and `\t` are kept. Neither can introduce a sequence, both are legitimate
/// layout, and a link that wraps onto the next line is exactly what the `id`
/// parameter exists for. `\r` is *not* kept: returning to column zero is how
/// text gets overwritten by later text, which is a spoofing tool rather than
/// layout.
#[must_use]
pub fn sanitize_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\n' | '\t' => out.push(ch),
            // C0 controls. `ESC` (0x1b) and `BEL` (0x07) are the dangerous ones,
            // but none of them belong in link text.
            '\u{0}'..='\u{1f}' => out.push(control_picture(ch)),
            '\u{7f}' => out.push('\u{2421}'),
            // C1 controls: 0x9b and 0x9d are the eight-bit spellings of `CSI`
            // and `OSC`, so letting them through would reopen the whole hole.
            '\u{80}'..='\u{9f}' => out.push('\u{fffd}'),
            other => out.push(other),
        }
    }
    out
}

/// Maps a C0 control onto its Unicode Control Pictures glyph: 0x1b becomes
/// U+241B `␛`.
fn control_picture(ch: char) -> char {
    char::from_u32(0x2400 + u32::from(ch)).unwrap_or('\u{fffd}')
}

/// Whether `text` contains anything [`sanitize_text`] would replace.
#[must_use]
pub fn text_is_clean(text: &str) -> bool {
    !text
        .chars()
        .any(|ch| !matches!(ch, '\n' | '\t') && ch.is_control())
}

// -- building sequences -----------------------------------------------------

/// A hyperlink: a sanitised URI, an optional `id`, and a choice of terminator.
///
/// Construction is the only place sanitisation happens, so a `Hyperlink` that
/// exists is a `Hyperlink` that is safe to emit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hyperlink {
    /// Percent-encoded target.
    uri: String,
    /// Sanitised `id` parameter, when one was asked for.
    id: Option<String>,
    /// How to end the OSC string.
    terminator: Terminator,
}

impl Hyperlink {
    /// Builds a link to `uri`, percent-encoding it on the way in.
    ///
    /// # Errors
    ///
    /// Propagates [`sanitize_uri`], which rejects the empty URI.
    pub fn new(uri: &str) -> Result<Self> {
        Ok(Self {
            uri: sanitize_uri(uri)?,
            id: None,
            terminator: Terminator::default(),
        })
    }

    /// Attaches an `id`, so this link and others sharing the id *and* the URI
    /// highlight together on hover.
    ///
    /// # Errors
    ///
    /// Propagates [`sanitize_id`].
    pub fn with_id(mut self, id: &str) -> Result<Self> {
        self.id = Some(sanitize_id(id)?);
        Ok(self)
    }

    /// Chooses the string terminator. `ST` is the default and the right answer.
    #[must_use]
    pub fn with_terminator(mut self, terminator: Terminator) -> Self {
        self.terminator = terminator;
        self
    }

    /// The percent-encoded target, as it appears on the wire.
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// The `id` parameter, if any.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// The terminator this link will be emitted with.
    #[must_use]
    pub fn terminator(&self) -> Terminator {
        self.terminator
    }

    /// The parameter field: `id=<id>`, or empty when there is no id.
    #[must_use]
    pub fn params(&self) -> String {
        match &self.id {
            Some(id) => format!("id={id}"),
            None => String::new(),
        }
    }

    /// Opens the link: `OSC 8 ; <params> ; <URI> ST`.
    #[must_use]
    pub fn open(&self) -> String {
        format!(
            "{OSC}{COMMAND};{};{}{}",
            self.params(),
            self.uri,
            self.terminator.as_str()
        )
    }

    /// Closes the link: `OSC 8 ; ; ST`.
    #[must_use]
    pub fn close(&self) -> String {
        close(self.terminator)
    }

    /// Open sequence, sanitised text, close sequence — the whole link, ready to
    /// write to a terminal.
    #[must_use]
    pub fn wrap(&self, text: &str) -> String {
        format!("{}{}{}", self.open(), sanitize_text(text), self.close())
    }

    /// Whether the URI exceeds [`MAX_URI_LEN`], in which case VTE and iTerm2
    /// drop the link and print only the text.
    #[must_use]
    pub fn over_length(&self) -> bool {
        self.uri.len() > MAX_URI_LEN
    }
}

/// Closes whatever link is open: `OSC 8 ; ; ST`.
///
/// Legal even when no link is open, which is what lets a program close after
/// every label without tracking state.
#[must_use]
pub fn close(terminator: Terminator) -> String {
    format!("{OSC}{COMMAND};;{}", terminator.as_str())
}

/// Builds a `file://` URI for `path` on `host`, percent-encoding the path.
///
/// The hostname is not optional in a `file://` hyperlink: the terminal compares
/// it against its own and refuses to open a file belonging to another machine,
/// which is the only thing stopping `file:///etc/passwd` printed over ssh from
/// opening the *local* `/etc/passwd`. Pass [`local_hostname`] unless you have a
/// better answer.
///
/// `%`, `#` and `?` are encoded here as well as the bytes [`sanitize_uri`]
/// handles, because this function *constructs* a URI from a raw path rather than
/// accepting one that is already encoded.
#[must_use]
pub fn file_uri(host: &str, path: &str) -> String {
    let mut uri = format!("file://{host}");
    for &byte in path.as_bytes() {
        if uri_byte_needs_encoding(byte) || matches!(byte, b'%' | b'#' | b'?') {
            percent_encode(&mut uri, byte);
        } else {
            uri.push(char::from(byte));
        }
    }
    uri
}

/// This machine's hostname, for [`file_uri`].
///
/// `gethostname()` needs libc and this crate forbids `unsafe`, so the value is
/// recovered from the places it is also written down: `$HOSTNAME`, `$HOST`, then
/// the kernel's own copy. Falls back to `localhost`, which every terminal accepts
/// as "here" — that gives up the ssh protection, but it never produces a URI
/// pointing at the wrong machine.
#[must_use]
pub fn local_hostname() -> String {
    for key in ["HOSTNAME", "HOST"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }
    "localhost".to_owned()
}

/// Entry point for `escapade hyperlink …`.
///
/// With no subcommand this emits a link; the usual verbs sit underneath.
///
/// Only `probe` returns anything but [`ExitCode::SUCCESS`]: its finding *is* the
/// answer, so it goes in the exit status where a shell can branch on it.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with, and fails if the feature is
/// missing from the database.
pub fn run(catalog: &Catalog, command: &HyperlinkCommand, no_color: bool) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        None => {
            emit::run(&theme, command)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(HyperlinkAction::Explain { no_matrix }) => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        Some(HyperlinkAction::Probe { format }) => probe::report(&theme, catalog, *format),
        Some(HyperlinkAction::Demo {
            force,
            only,
            step,
            list,
        }) => {
            if *list {
                demo::list_steps(&theme);
            } else {
                demo::run(&theme, catalog, *force, only.as_deref(), *step)?;
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The literal string terminator, so tests assert on the wire format rather
    /// than on the constants that produced it.
    const ESC_ST: &str = "\u{1b}\\";

    #[test]
    fn a_plain_link_has_two_semicolons_and_an_empty_parameter_field() {
        let link = Hyperlink::new("https://example.com").unwrap();
        assert_eq!(
            link.open(),
            format!("\u{1b}]8;;https://example.com{ESC_ST}")
        );
        assert_eq!(link.close(), format!("\u{1b}]8;;{ESC_ST}"));
        assert_eq!(link.params(), "");
        assert_eq!(link.id(), None);
        assert_eq!(link.terminator(), Terminator::St);
    }

    #[test]
    fn wrapping_text_puts_it_between_the_two_sequences() {
        let link = Hyperlink::new("https://example.com").unwrap();
        assert_eq!(
            link.wrap("click me"),
            format!("\u{1b}]8;;https://example.com{ESC_ST}click me\u{1b}]8;;{ESC_ST}")
        );
    }

    #[test]
    fn an_id_becomes_the_parameter_field() {
        let link = Hyperlink::new("https://example.com")
            .unwrap()
            .with_id("row-42")
            .unwrap();
        assert_eq!(link.params(), "id=row-42");
        assert_eq!(
            link.open(),
            format!("\u{1b}]8;id=row-42;https://example.com{ESC_ST}")
        );
        // The close sequence never carries parameters: the same bytes whether or
        // not the link being closed had an id.
        assert_eq!(link.close(), close(Terminator::St));
    }

    #[test]
    fn bel_termination_swaps_only_the_terminator() {
        let st = Hyperlink::new("https://example.com").unwrap();
        let bel = st.clone().with_terminator(Terminator::Bel);

        assert!(st.open().ends_with(ESC_ST));
        assert_eq!(bel.open(), "\u{1b}]8;;https://example.com\u{7}");
        assert_eq!(bel.close(), "\u{1b}]8;;\u{7}");

        // Same payload, one byte shorter.
        assert_eq!(st.open().len(), bel.open().len() + 1);
        assert_eq!(Terminator::default(), Terminator::St);
        assert_eq!(Terminator::St.name(), "ST");
        assert_eq!(Terminator::Bel.name(), "BEL");
    }

    #[test]
    fn non_ascii_uris_are_percent_encoded_per_utf8_byte() {
        // U+00E9 is 0xc3 0xa9 in UTF-8, and both bytes must be encoded.
        let link = Hyperlink::new("https://example.com/caf\u{e9}").unwrap();
        assert_eq!(link.uri(), "https://example.com/caf%C3%A9");

        // A four-byte scalar and a space, in one go.
        let link = Hyperlink::new("https://example.com/a b/\u{1f600}").unwrap();
        assert_eq!(link.uri(), "https://example.com/a%20b/%F0%9F%98%80");
        assert!(!link.open().contains(' '));
    }

    #[test]
    fn already_encoded_uris_are_left_alone() {
        // Encoding `%` again would turn every correct URI into a broken one.
        let link = Hyperlink::new("https://example.com/a%20b?q=1&r=2#frag").unwrap();
        assert_eq!(link.uri(), "https://example.com/a%20b?q=1&r=2#frag");
    }

    // -- hostile input ------------------------------------------------------

    #[test]
    fn an_escape_in_the_uri_cannot_end_the_osc_string() {
        // The attack: end the OSC string early, then run whatever follows.
        let hostile = "https://example.com/\u{1b}\\\u{1b}[2J\u{1b}]0;pwned\u{7}";
        let link = Hyperlink::new(hostile).unwrap();
        let open = link.open();

        // `[` and `]` are printable and legal in a URI, so they stay; every
        // control byte and the `;` are gone, which is what disarms the payload.
        assert_eq!(
            link.uri(),
            "https://example.com/%1B%5C%1B[2J%1B]0%3Bpwned%07"
        );
        // Exactly two escape bytes survive: the `ESC ]` we opened with and the
        // `ESC \` we chose to terminate with.
        assert_eq!(open.matches('\u{1b}').count(), 2);
        assert!(open.starts_with("\u{1b}]8;;"));
        assert!(open.ends_with(ESC_ST));
        assert!(!open.contains('\u{7}'), "the BEL must not survive either");
    }

    #[test]
    fn a_bel_in_the_uri_cannot_end_a_bel_terminated_string() {
        let link = Hyperlink::new("https://example.com/\u{7}evil")
            .unwrap()
            .with_terminator(Terminator::Bel);
        let open = link.open();
        assert_eq!(link.uri(), "https://example.com/%07evil");
        assert_eq!(
            open.matches('\u{7}').count(),
            1,
            "only the terminator may be a BEL"
        );
    }

    #[test]
    fn a_semicolon_in_the_uri_cannot_look_like_a_parameter_separator() {
        let link = Hyperlink::new("https://example.com/a;b").unwrap();
        assert_eq!(link.uri(), "https://example.com/a%3Bb");
        // Two semicolons in the whole sequence: the structural ones.
        assert_eq!(link.open().matches(';').count(), 2);
    }

    #[test]
    fn text_containing_its_own_osc_8_cannot_escape_the_link() {
        let link = Hyperlink::new("https://example.com").unwrap();
        // The attack: close our link, then relabel the rest of the line.
        let hostile = "safe\u{1b}]8;;https://evil.example\u{1b}\\ and now mine";
        let emitted = link.wrap(hostile);

        // Four escape bytes, all ours: `ESC ]` and `ESC \` for the open, the same
        // pair for the close. None from the text.
        assert_eq!(emitted.matches('\u{1b}').count(), 4);
        assert!(!emitted.contains("evil.example\u{1b}"));
        assert!(emitted.contains('\u{241b}'), "ESC shows up as a glyph");
        assert!(emitted.contains("safe\u{241b}]8;;https://evil.example\u{241b}\\"));
    }

    #[test]
    fn text_keeps_newlines_and_tabs_but_nothing_else_control() {
        assert_eq!(sanitize_text("a\nb\tc"), "a\nb\tc");
        // `\r` would let later text overwrite earlier text.
        assert_eq!(sanitize_text("visible\rhidden"), "visible\u{240d}hidden");
        assert_eq!(sanitize_text("bell\u{7}"), "bell\u{2407}");
        assert_eq!(sanitize_text("del\u{7f}"), "del\u{2421}");
        // 0x9d is the eight-bit spelling of OSC.
        assert_eq!(sanitize_text("c1\u{9d}8;;x"), "c1\u{fffd}8;;x");
        assert_eq!(sanitize_text("caf\u{e9} \u{1f600}"), "caf\u{e9} \u{1f600}");

        assert!(text_is_clean("a\nb\tc caf\u{e9}"));
        assert!(!text_is_clean("a\u{1b}b"));
        assert!(!text_is_clean("a\u{9b}b"));
    }

    #[test]
    fn an_empty_uri_is_refused_because_it_is_the_close_sequence() {
        let error = Hyperlink::new("").unwrap_err().to_string();
        assert!(error.contains("closes a link"), "{error}");
        // The close sequence itself stays reachable on purpose.
        assert_eq!(close(Terminator::St), "\u{1b}]8;;\u{1b}\\");
    }

    #[test]
    fn a_very_long_uri_still_builds_but_is_flagged() {
        let long = format!("https://example.com/{}", "a".repeat(4000));
        let link = Hyperlink::new(&long).unwrap();

        assert!(link.over_length());
        assert!(link.uri().len() > MAX_URI_LEN);
        // Well-formed regardless: the terminal drops the link, not the text.
        assert!(link.open().starts_with("\u{1b}]8;;"));
        assert!(link.open().ends_with(ESC_ST));
        assert_eq!(link.wrap("label").matches('\u{1b}').count(), 4);

        // Exactly at the limit is not flagged.
        let short = Hyperlink::new(&"a".repeat(MAX_URI_LEN)).unwrap();
        assert!(!short.over_length());
    }

    #[test]
    fn ids_are_rejected_rather_than_rewritten() {
        let link = Hyperlink::new("https://example.com").unwrap();

        for (id, expected) in [
            ("", "empty"),
            ("a;b", "`;`"),
            ("file:12:3", "`:`"),
            ("a\u{1b}b", "0x1b"),
            ("caf\u{e9}", "0xc3"),
        ] {
            let error = link.clone().with_id(id).unwrap_err().to_string();
            assert!(error.contains(expected), "id {id:?} gave: {error}");
        }

        let too_long = "x".repeat(MAX_ID_LEN + 1);
        let error = link.clone().with_id(&too_long).unwrap_err().to_string();
        assert!(error.contains("truncates"), "{error}");

        // Exactly at the limit is fine.
        assert!(link.with_id(&"x".repeat(MAX_ID_LEN)).is_ok());
    }

    #[test]
    fn an_id_survives_verbatim_when_it_is_acceptable() {
        let id = "escapade-row-7_/.~=+abc";
        let link = Hyperlink::new("https://example.com")
            .unwrap()
            .with_id(id)
            .unwrap();
        assert_eq!(link.id(), Some(id));
        assert_eq!(link.params(), format!("id={id}"));
    }

    // -- file URIs ----------------------------------------------------------

    #[test]
    fn file_uris_carry_a_hostname_and_an_encoded_path() {
        assert_eq!(
            file_uri("myhost", "/home/you/notes.txt"),
            "file://myhost/home/you/notes.txt"
        );
        assert_eq!(
            file_uri("myhost", "/tmp/a b/caf\u{e9}?x#y%z"),
            "file://myhost/tmp/a%20b/caf%C3%A9%3Fx%23y%25z"
        );
        // The result is already clean, so sanitising it again changes nothing.
        let built = file_uri("myhost", "/tmp/a b");
        assert_eq!(sanitize_uri(&built).unwrap(), built);
    }

    #[test]
    fn local_hostname_is_never_empty_and_never_has_whitespace() {
        let host = local_hostname();
        assert!(!host.is_empty());
        assert!(!host.contains(char::is_whitespace), "{host:?}");
    }

    #[test]
    fn no_sequence_this_module_builds_contains_a_loose_control_byte() {
        let link = Hyperlink::new("https://example.com/\u{1b}\u{7}\u{0}")
            .unwrap()
            .with_id("plain")
            .unwrap();
        for sequence in [link.open(), link.close(), link.wrap("label")] {
            let chars: Vec<char> = sequence.chars().collect();
            for (index, ch) in chars.iter().enumerate() {
                if *ch == '\u{1b}' {
                    assert!(
                        matches!(chars.get(index + 1), Some(']' | '\\')),
                        "loose ESC in {sequence:?}"
                    );
                }
            }
            assert!(!sequence.contains('\u{7}'));
            assert!(!sequence.contains('\u{0}'));
        }
    }
}
