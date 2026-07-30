//! Building the three notification sequences, and making user text safe to put
//! inside them.
//!
//! There are three competing escapes, and they are not variations on a theme —
//! they differ in what they can *say*:
//!
//! ```text
//! OSC 9   ; <text> ST                        one field, and nothing else
//! OSC 777 ; notify ; <title> ; <body> ST     two fields, split on semicolons
//! OSC 99  ; <key>=<value>: … ; <payload> ST  keyed metadata, chunkable
//! ```
//!
//! Every builder here takes user-supplied text, and user-supplied text is the
//! interesting problem: a body containing a raw `ESC` or `BEL` would end the OSC
//! string early and leave the rest of the body being interpreted as a fresh
//! escape sequence. [`Safe`] is the type that cannot happen to, and none of the
//! builders accept a bare `&str`.
//!
//! Specifications:
//! * OSC 99 — <https://sw.kovidgoyal.net/kitty/desktop-notifications/>
//! * OSC 9 — <https://iterm2.com/documentation-escape-codes.html>
//! * OSC 777 — urxvt's `notify` Perl extension, via
//!   <https://codeberg.org/dnkl/foot/pulls/236>

use base64::prelude::{BASE64_STANDARD, Engine as _};

use crate::term::ST;

/// Operating System Command: `ESC ]`.
///
/// All three notification protocols live here, which is the reason a terminal
/// that has never heard of any of them still does the right thing: an OSC string
/// with an unrecognised number is consumed up to its terminator and discarded, so
/// the worst case of emitting one is that nothing happens. That is what makes it
/// safe to send unconditionally, with no capability check at all.
pub use crate::term::OSC;

/// OSC 99's limit on an *unencoded* payload, in bytes, per escape code.
///
/// The specification says "2048 bytes, before being encoded or 4096 encoded
/// bytes".
pub const MAX_PLAIN_PAYLOAD: usize = 2048;

/// Bytes of raw text put into one `e=1` chunk.
///
/// A multiple of three, so base64 encoding a full chunk produces no padding and
/// the chunks concatenate into one valid base64 stream as well as decoding
/// independently. 2046 rather than 2048 for that reason; the resulting 2728
/// base64 characters are comfortably inside the 4096-byte encoded limit.
pub const MAX_RAW_CHUNK: usize = 2046;

/// The characters an OSC 99 identifier may contain, per the specification:
/// `[a-zA-Z0-9_-+.]`.
const ID_EXTRA: [char; 4] = ['_', '-', '+', '.'];

/// How insistently a notification should be presented.
///
/// This is the crate's own vocabulary; [`Urgency::osc99`] maps it onto the only
/// protocol that can express it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Urgency {
    /// Show it quietly.
    Low,
    /// The default.
    #[default]
    Normal,
    /// Demand attention. On most desktops this one does not auto-dismiss.
    Critical,
}

impl Urgency {
    /// The OSC 99 `u=` value: `0` low, `1` normal, `2` critical.
    #[must_use]
    pub fn osc99(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }

    /// Human name, as spelled on the command line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::Critical => "critical",
        }
    }
}

impl From<crate::cli::Urgency> for Urgency {
    fn from(urgency: crate::cli::Urgency) -> Self {
        match urgency {
            crate::cli::Urgency::Low => Self::Low,
            crate::cli::Urgency::Normal => Self::Normal,
            crate::cli::Urgency::Critical => Self::Critical,
        }
    }
}

/// One of the three wire protocols, already resolved — `auto` is gone by the
/// time anything here sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// kitty's desktop notification protocol. Titles, bodies, urgency, ids,
    /// replacement, icons, chunking.
    Osc99,
    /// urxvt's `notify` extension. Title and body, nothing more.
    Osc777,
    /// iTerm2's original one-liner. One field of text.
    Osc9,
}

impl Protocol {
    /// Short name, as spelled by `--protocol`.
    #[must_use]
    pub fn flag(self) -> &'static str {
        match self {
            Self::Osc99 => "kitty",
            Self::Osc777 => "osc777",
            Self::Osc9 => "osc9",
        }
    }

    /// The OSC number, for display.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Osc99 => "OSC 99",
            Self::Osc777 => "OSC 777",
            Self::Osc9 => "OSC 9",
        }
    }

    /// One line on what it can express.
    #[must_use]
    pub fn capability(self) -> &'static str {
        match self {
            Self::Osc99 => "title, body, urgency, id, replacement, icons, chunked payloads",
            Self::Osc777 => "title and body",
            Self::Osc9 => "one field of text",
        }
    }

    /// Richest first. [`super::resolve`] walks this order.
    pub const RICHEST_FIRST: [Self; 3] = [Self::Osc99, Self::Osc777, Self::Osc9];
}

// ---------------------------------------------------------------------------
// Sanitizing
// ---------------------------------------------------------------------------

/// Text that has been proved safe to place inside an OSC string.
///
/// "Safe" is the OSC 99 specification's *escape code safe UTF-8*: valid UTF-8
/// (guaranteed by `str`) containing none of the C0 or C1 control characters —
/// U+0000–U+001F, U+007F and U+0080–U+009F. That set is exactly what could end
/// the string early or start a new sequence: `ESC` (U+001B) begins `ST`, `BEL`
/// (U+0007) is the legacy terminator, and the C1 block contains the 8-bit forms
/// of `ST`, `OSC` and `APC`.
///
/// The only way to get one is [`Safe::new`], so a builder that takes a `Safe`
/// cannot be handed hostile text by mistake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Safe {
    text: String,
    /// Whether [`Safe::new`] had to change anything.
    altered: bool,
}

impl Safe {
    /// Makes `text` safe, replacing anything that could break out of the string.
    ///
    /// Newline, carriage return and tab become a single space — a notification
    /// is one or two lines of text on a desktop panel, so the whitespace was
    /// never going to survive anyway. Every other control character is dropped.
    /// Runs of resulting whitespace collapse, and the result is trimmed.
    ///
    /// Rejecting instead was tempting and is wrong: a body assembled from
    /// command output will contain a stray control byte eventually, and losing
    /// the notification is worse than losing the byte. What matters is that the
    /// byte cannot reach the terminal, and it cannot.
    #[must_use]
    pub fn new(text: &str) -> Self {
        let mut out = String::with_capacity(text.len());
        let mut altered = false;
        let mut pending_space = false;
        for ch in text.chars() {
            match ch {
                '\n' | '\r' | '\t' => {
                    altered = true;
                    pending_space = true;
                }
                // C0 (minus the three above), DEL, and C1.
                '\u{0}'..='\u{1f}' | '\u{7f}' | '\u{80}'..='\u{9f}' => altered = true,
                ' ' => pending_space = true,
                other => {
                    if pending_space && !out.is_empty() {
                        out.push(' ');
                    }
                    pending_space = false;
                    out.push(other);
                }
            }
        }
        Self { text: out, altered }
    }

    /// The safe text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether making it safe changed it, so a caller can say so.
    #[must_use]
    pub fn altered(&self) -> bool {
        self.altered
    }

    /// Whether there is nothing left.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Whether this can be written into an OSC 99 payload without base64.
    ///
    /// Controls are already gone, so the only remaining hazard is `;`: the
    /// payload is whatever follows the second semicolon, and implementations
    /// that split on *every* semicolon would truncate. Anything else goes on the
    /// wire as itself, which keeps the common case legible in `--dry-run`.
    #[must_use]
    pub fn plain_in_osc99(&self) -> bool {
        !self.text.contains(';')
    }
}

/// Sanitizes a notification id to OSC 99's identifier alphabet.
///
/// The specification restricts identifiers to `[a-zA-Z0-9_-+.]` and tells
/// terminals to sanitize what they receive, precisely because an id is echoed
/// back to the application when the user clicks the notification — an
/// unsanitized id is an injection into *our* input stream, not the terminal's.
/// Doing it on this side as well means the id we match against is the id we
/// sent.
///
/// Illegal characters are replaced with `-`, and an id that sanitizes to nothing
/// becomes `None`.
#[must_use]
pub fn safe_id(id: &str) -> Option<String> {
    let cleaned: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ID_EXTRA.contains(&ch) {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

// ---------------------------------------------------------------------------
// What a notification asks for
// ---------------------------------------------------------------------------

/// A notification as the user described it, before any protocol got involved.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// Title, shown above the body. Not every protocol has somewhere to put it.
    pub title: Option<Safe>,
    /// Body text.
    pub body: Safe,
    /// Urgency, when the user asked for one specifically.
    pub urgency: Option<Urgency>,
    /// Id, so a later notification can replace this one.
    ///
    /// This is usually set even when the user did not ask for one, because OSC 99
    /// needs an id to tie a chunked notification's escape codes together. Whether
    /// the *user* wanted one is [`Request::id_explicit`].
    pub id: Option<String>,
    /// Whether the id was asked for, rather than minted to make chunking work.
    ///
    /// Only an explicit id is worth reporting as lost on a protocol that has no
    /// id: telling someone their `--id` was dropped is useful, telling them an id
    /// they never asked for was dropped is noise.
    pub id_explicit: bool,
}

impl Request {
    /// Builds a request from raw strings, sanitizing as it goes.
    #[must_use]
    pub fn new(title: Option<&str>, body: &str) -> Self {
        Self {
            title: title.map(Safe::new).filter(|safe| !safe.is_empty()),
            body: Safe::new(body),
            urgency: None,
            id: None,
            id_explicit: false,
        }
    }

    /// Whether sanitizing changed the title or the body.
    #[must_use]
    pub fn altered(&self) -> bool {
        self.body.altered() || self.title.as_ref().is_some_and(Safe::altered)
    }
}

/// A built notification: the escapes to send, and what got lost on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// Which protocol built it.
    pub protocol: Protocol,
    /// The escape sequences, in order. More than one only when OSC 99 had to
    /// chunk, or had a title *and* a body.
    pub sequences: Vec<String>,
    /// Things the request asked for that this protocol cannot express. Printed
    /// on stderr rather than swallowed — a notification that quietly lost its
    /// urgency is how you end up not trusting the tool.
    pub lost: Vec<String>,
}

impl Built {
    /// Everything concatenated, ready to write in one call.
    #[must_use]
    pub fn joined(&self) -> String {
        self.sequences.concat()
    }
}

/// Builds `request` for `protocol`.
#[must_use]
pub fn build(protocol: Protocol, request: &Request) -> Built {
    match protocol {
        Protocol::Osc99 => build_osc99(request),
        Protocol::Osc777 => build_osc777(request),
        Protocol::Osc9 => build_osc9(request),
    }
}

// ---------------------------------------------------------------------------
// OSC 9
// ---------------------------------------------------------------------------

/// Raises a notification with body text only: `OSC 9 ; <text> ST`.
///
/// The oldest of the three, from iTerm2, from before 2010. There is exactly one
/// field and no way to add another, so a title, an urgency and an id all have
/// nowhere to go. Rather than drop a title the user asked for, it is folded into
/// the text as `<title> — <body>`; [`Built::lost`] records that it happened.
///
/// One booby trap: ConEmu also claimed OSC 9, for `OSC 9 ; 4 ; <state> ;
/// <progress> ST` taskbar progress, and terminals that implement both
/// disambiguate by looking at the first character. A message that begins with a
/// digit followed by `;` is therefore prefixed with a space, so a notification
/// reading `4; done` cannot be mistaken for a progress report.
#[must_use]
fn build_osc9(request: &Request) -> Built {
    let mut lost = Vec::new();
    let mut text = match &request.title {
        Some(title) => {
            lost.push(format!(
                "OSC 9 has no title field — folded {:?} into the message text",
                title.as_str()
            ));
            format!("{} — {}", title.as_str(), request.body.as_str())
        }
        None => request.body.as_str().to_owned(),
    };
    if let Some(urgency) = request.urgency {
        lost.push(format!(
            "OSC 9 cannot express urgency, so --urgency {} was dropped",
            urgency.label()
        ));
    }
    if request.id_explicit {
        lost.push("OSC 9 has no notification id, so --id was dropped and this cannot be replaced later".to_owned());
    }
    if let Some(note) = length_warning(&text, Protocol::Osc9) {
        lost.push(note);
    }
    if starts_like_conemu(&text) {
        text.insert(0, ' ');
        lost.push(
            "the message began with a digit and a semicolon, which ConEmu-compatible \
             terminals read as a progress report — a space was prefixed to keep it text"
                .to_owned(),
        );
    }

    Built {
        protocol: Protocol::Osc9,
        sequences: vec![format!("{OSC}9;{text}{ST}")],
        lost,
    }
}

/// Warns when a payload is long enough to be at risk on a protocol that cannot
/// chunk.
///
/// Only OSC 99 says how long a payload may be, and only OSC 99 has an answer for
/// text that exceeds it. OSC 9 and OSC 777 have no stated limit and no chunking,
/// so a long one relies entirely on the terminal's own OSC string buffer — and a
/// terminal that runs out truncates or drops the string without saying so. The
/// documented OSC 99 limit is the obvious threshold to warn at.
fn length_warning(text: &str, protocol: Protocol) -> Option<String> {
    if text.len() <= MAX_PLAIN_PAYLOAD {
        return None;
    }
    Some(format!(
        "the payload is {} bytes, and {} has neither a stated length limit nor any way to \
         chunk — a terminal whose OSC buffer is smaller will truncate or drop it silently. \
         --protocol kitty splits this across escape codes instead",
        text.len(),
        protocol.label()
    ))
}

/// Whether `text` would be mistaken for ConEmu's `OSC 9 ; 4 ; …` progress form.
fn starts_like_conemu(text: &str) -> bool {
    let mut chars = text.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some(first), Some(';')) if first.is_ascii_digit()
    )
}

// ---------------------------------------------------------------------------
// OSC 777
// ---------------------------------------------------------------------------

/// Raises a notification with a title: `OSC 777 ; notify ; <title> ; <body> ST`.
///
/// urxvt's `OSC 777` is a generic "call one of my Perl extensions" hatch, and
/// `notify` is the extension everyone copied. The fields are positional and
/// split on `;`, which is the whole hazard: a semicolon in the title shifts the
/// body into a fourth field that some implementations refuse and others merge.
/// So a semicolon in the *title* becomes a comma, and [`Built::lost`] says so.
///
/// The body is left alone. It is the last field, and the implementations that
/// matter treat it as "everything remaining" — urxvt's original regular
/// expression is `notify;(\S+);(.*)`. Contour, which splits into exactly three
/// parts, will refuse such a notification; that is worth a warning but not worth
/// mangling every body containing a semicolon.
#[must_use]
fn build_osc777(request: &Request) -> Built {
    let mut lost = Vec::new();

    let title = match &request.title {
        Some(title) => {
            let (title, replaced) = swap_semicolons(title.as_str());
            if replaced {
                lost.push(
                    "OSC 777 splits its fields on semicolons, so semicolons in the title \
                     became commas"
                        .to_owned(),
                );
            }
            title
        }
        // The field is positional, so it cannot simply be left out; an empty
        // title is the documented way to say "no title".
        None => String::new(),
    };

    let body = request.body.as_str();
    if body.contains(';') {
        lost.push(
            "the body contains a semicolon — most implementations take the last field as \
             everything remaining, but Contour requires exactly three fields and will \
             ignore this notification"
                .to_owned(),
        );
    }
    if let Some(urgency) = request.urgency {
        lost.push(format!(
            "OSC 777 cannot express urgency, so --urgency {} was dropped (foot applies its \
             own configured urgency instead)",
            urgency.label()
        ));
    }
    if request.id_explicit {
        lost.push("OSC 777 has no notification id, so --id was dropped and this cannot be replaced later".to_owned());
    }
    if let Some(note) = length_warning(&format!("{title}{body}"), Protocol::Osc777) {
        lost.push(note);
    }

    Built {
        protocol: Protocol::Osc777,
        sequences: vec![format!("{OSC}777;notify;{title};{body}{ST}")],
        lost,
    }
}

/// Replaces every `;` with `,`, reporting whether it had to.
fn swap_semicolons(text: &str) -> (String, bool) {
    if text.contains(';') {
        (text.replace(';', ","), true)
    } else {
        (text.to_owned(), false)
    }
}

// ---------------------------------------------------------------------------
// OSC 99
// ---------------------------------------------------------------------------

/// Raises a notification with metadata: `OSC 99 ; <metadata> ; <payload> ST`.
///
/// Metadata is colon-separated `key=value` pairs; both semicolons are always
/// present even when the metadata is empty, which is why the minimal form is
/// `OSC 99 ; ; Hello world ST`.
///
/// The pieces this uses:
///
/// * `p=title` / `p=body` — which part of the notification the payload is.
///   `title` is the default, so a body-only notification puts its text in the
///   title slot; that is what the specification's own one-line example does.
/// * `u=0|1|2` — urgency.
/// * `i=<id>` — the notification id. Sending a second notification with the
///   same id replaces the first.
/// * `e=1` — the payload is base64. Used whenever the text is not safe to write
///   literally.
/// * `d=0` — more escapes are coming; the notification is not complete. The
///   final escape carries `d=1` (the default), which is what makes it appear.
///
/// Long text is split across several escapes, because one escape may carry at
/// most [`MAX_PLAIN_PAYLOAD`] unencoded bytes. `id` is required for that: it is
/// what ties the chunks together, so a request that needs chunking without an id
/// gets one from the caller.
#[must_use]
fn build_osc99(request: &Request) -> Built {
    // Body-only notifications go in the title slot: with one payload and no
    // `p=`, that is what the terminal shows, and it is the spec's own example.
    let parts: Vec<(&'static str, &Safe)> = match &request.title {
        Some(title) => vec![("title", title), ("body", &request.body)],
        None => vec![("title", &request.body)],
    };

    // Every payload becomes a list of (encoded chunk, whether it is base64).
    let mut pieces: Vec<(&'static str, String, bool)> = Vec::new();
    for (kind, text) in parts {
        if text.is_empty() && kind == "body" {
            continue;
        }
        let encoded = text.plain_in_osc99();
        for chunk in chunk_payload(text.as_str(), encoded) {
            pieces.push((kind, chunk, !encoded));
        }
    }
    if pieces.is_empty() {
        // An empty notification is still a notification: an empty title slot is
        // the honest encoding of "the user gave us nothing".
        pieces.push(("title", String::new(), false));
    }

    let multi = pieces.len() > 1;
    // Chunks are correlated by id, so one is mandatory as soon as there are two
    // escapes. `Request::id` is filled in by the caller for exactly this case.
    let id = request.id.as_deref().and_then(safe_id);

    let mut sequences = Vec::with_capacity(pieces.len());
    let final_index = pieces.len() - 1;
    for (index, (kind, payload, base64)) in pieces.iter().enumerate() {
        let mut keys: Vec<String> = Vec::new();
        if let Some(id) = &id {
            keys.push(format!("i={id}"));
        }
        // `p=title` is the default; spelling it out only where it is not obvious
        // keeps the single-escape form as short as the specification's example.
        if *kind != "title" || multi {
            keys.push(format!("p={kind}"));
        }
        if *base64 {
            keys.push("e=1".to_owned());
        }
        // Urgency belongs to the notification, not to a payload, so it goes on
        // the escape that creates it.
        if index == 0
            && let Some(urgency) = request.urgency
        {
            keys.push(format!("u={}", urgency.osc99()));
        }
        if index != final_index {
            keys.push("d=0".to_owned());
        } else if multi {
            // `d=1` is the default, so it is only worth the bytes when earlier
            // escapes said `d=0` and a reader needs to see the pair.
            keys.push("d=1".to_owned());
        }
        sequences.push(format!("{OSC}99;{};{payload}{ST}", keys.join(":")));
    }

    let mut lost = Vec::new();
    if multi && id.is_none() {
        lost.push(
            "this notification needs several escape codes but has no id to tie them \
             together, so the terminal may show them as separate notifications"
                .to_owned(),
        );
    }

    Built {
        protocol: Protocol::Osc99,
        sequences,
        lost,
    }
}

/// Splits `text` into per-escape payloads.
///
/// With `plain` set the text goes on the wire as itself, at most
/// [`MAX_PLAIN_PAYLOAD`] bytes per escape. Otherwise each chunk of at most
/// [`MAX_RAW_CHUNK`] bytes is base64-encoded separately.
///
/// Splits always land on a character boundary, never inside a UTF-8 scalar. That
/// costs a few bytes per chunk and buys a useful property: each chunk is
/// independently valid UTF-8, so a terminal that decodes chunk by chunk gets the
/// same answer as one that concatenates first.
#[must_use]
pub fn chunk_payload(text: &str, plain: bool) -> Vec<String> {
    let limit = if plain { MAX_PLAIN_PAYLOAD } else { MAX_RAW_CHUNK };
    let mut chunks = Vec::new();
    for chunk in split_on_char_boundaries(text, limit) {
        chunks.push(if plain {
            chunk.to_owned()
        } else {
            BASE64_STANDARD.encode(chunk)
        });
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Splits `text` into pieces of at most `limit` bytes, never mid-character.
fn split_on_char_boundaries(text: &str, limit: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.len() <= limit {
            pieces.push(rest);
            break;
        }
        // Walk back from `limit` to the nearest character boundary. At most three
        // steps, because a UTF-8 scalar is at most four bytes.
        let mut end = limit;
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        // A single character longer than the limit cannot happen (the limits here
        // are in the thousands), but refusing to loop forever is free.
        if end == 0 {
            pieces.push(rest);
            break;
        }
        let (head, tail) = rest.split_at(end);
        pieces.push(head);
        rest = tail;
    }
    pieces
}

/// `OSC 99 ; i=<id> : p=? ; ST` — ask what this terminal's OSC 99 supports.
///
/// The one protocol of the three that can be asked anything. The reply carries
/// `p=` (which payload types work), `u=` (which urgencies), `a=`, `c=`, `o=`,
/// `s=` and `w=`; a terminal that does not implement OSC 99 says nothing at all,
/// which is why the caller pairs this with a Primary Device Attributes barrier.
#[must_use]
pub fn osc99_query(id: &str) -> String {
    format!("{OSC}99;i={id}:p=?;{ST}")
}

/// `OSC 99 ; i=<id> : p=close ; ST` — take an earlier notification down again.
#[must_use]
pub fn osc99_close(id: &str) -> String {
    format!("{OSC}99;i={id}:p=close;{ST}")
}

/// What a terminal said in answer to [`osc99_query`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Osc99Support {
    /// Payload types it accepts (`p=`), always including `title`.
    pub payloads: Vec<String>,
    /// Urgency levels it honours (`u=`).
    pub urgencies: Vec<String>,
    /// Activation actions it can report (`a=`).
    pub actions: Vec<String>,
    /// Whether it reports close events (`c=1`).
    pub close_events: bool,
    /// Whether it honours auto-expiry (`w=1`).
    pub auto_expire: bool,
    /// Display occasions it understands (`o=`).
    pub occasions: Vec<String>,
    /// Sound names it knows (`s=`).
    pub sounds: Vec<String>,
}

/// Parses the payload of an OSC reply, if it is an answer to [`osc99_query`].
///
/// The reply is `OSC 99 ; i=<id> : p=? ; key=value : key=value ST`, so the
/// payload seen here is `99;i=<id>:p=?;<answers>`. Returns `None` for any other
/// OSC reply, which is the common case — dynamic colours, the window title and
/// the clipboard all live in OSC too.
#[must_use]
pub fn parse_osc99_reply(payload: &str) -> Option<Osc99Support> {
    let rest = payload.strip_prefix("99;")?;
    let (metadata, answers) = rest.split_once(';')?;
    if !metadata.split(':').any(|field| field == "p=?") {
        return None;
    }

    let mut support = Osc99Support::default();
    for field in answers.split(':') {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let list = || {
            value
                .split(',')
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        };
        match key {
            "p" => support.payloads = list(),
            "u" => support.urgencies = list(),
            "a" => support.actions = list(),
            "o" => support.occasions = list(),
            "s" => support.sounds = list(),
            "c" => support.close_events = value == "1",
            "w" => support.auto_expire = value == "1",
            _ => {}
        }
    }
    Some(support)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The escape byte, spelled out once so the assertions below read as the
    /// bytes that go on the wire.
    const E: &str = "\u{1b}";

    fn request(title: Option<&str>, body: &str) -> Request {
        Request::new(title, body)
    }

    // -- sanitizing ---------------------------------------------------------

    #[test]
    fn plain_text_survives_untouched() {
        let safe = Safe::new("42 tests passed");
        assert_eq!(safe.as_str(), "42 tests passed");
        assert!(!safe.altered());
    }

    #[test]
    fn an_escape_in_the_body_cannot_reach_the_terminal() {
        // The hostile input: end the OSC string, then start a fresh one that
        // clears the screen.
        let hostile = "done\u{1b}\\\u{1b}[2Jgotcha";
        let safe = Safe::new(hostile);
        assert!(safe.altered());
        assert!(!safe.as_str().contains('\u{1b}'), "{:?}", safe.as_str());
        assert_eq!(safe.as_str(), "done\\[2Jgotcha");
    }

    #[test]
    fn a_bel_in_the_body_cannot_terminate_the_string() {
        let safe = Safe::new("done\u{7}rm -rf /");
        assert!(!safe.as_str().contains('\u{7}'));
        assert_eq!(safe.as_str(), "donerm -rf /");
    }

    #[test]
    fn eight_bit_string_terminator_is_stripped_too() {
        // U+009C is the C1 String Terminator, and U+009D is OSC. Both are single
        // characters that a terminal in 8-bit mode acts on.
        let safe = Safe::new("a\u{9c}b\u{9d}c\u{90}d");
        assert_eq!(safe.as_str(), "abcd");
        assert!(safe.altered());
    }

    #[test]
    fn newlines_and_tabs_become_a_single_space() {
        let safe = Safe::new("line one\nline two\t\tindented\r\n");
        assert_eq!(safe.as_str(), "line one line two indented");
        assert!(safe.altered());
    }

    #[test]
    fn every_control_character_is_removed() {
        let hostile: String = (0u32..=0x9f)
            .filter_map(char::from_u32)
            .chain(std::iter::once('\u{7f}'))
            .collect();
        let safe = Safe::new(&hostile);
        for ch in safe.as_str().chars() {
            assert!(!ch.is_control(), "control character {ch:?} survived sanitizing");
        }
    }

    #[test]
    fn an_empty_body_stays_empty_rather_than_becoming_whitespace() {
        assert!(Safe::new("").is_empty());
        assert!(Safe::new("   \n\t ").is_empty());
    }

    #[test]
    fn ids_are_reduced_to_the_documented_alphabet() {
        assert_eq!(safe_id("build-42").as_deref(), Some("build-42"));
        assert_eq!(safe_id("a.b+c_d").as_deref(), Some("a.b+c_d"));
        // A hostile id would come back to us in the activation report.
        assert_eq!(
            safe_id("x;p=close\u{1b}\\").as_deref(),
            Some("x-p-close")
        );
        assert_eq!(safe_id("!!!"), None);
        assert_eq!(safe_id(""), None);
    }

    // -- OSC 9 --------------------------------------------------------------

    #[test]
    fn osc9_is_one_field_of_text() {
        let built = build(Protocol::Osc9, &request(None, "42 tests passed"));
        assert_eq!(
            built.sequences,
            vec![format!("{E}]9;42 tests passed{E}\\")]
        );
        assert!(built.lost.is_empty());
    }

    #[test]
    fn osc9_folds_a_title_in_and_says_it_did() {
        let built = build(Protocol::Osc9, &request(Some("Build finished"), "42 tests passed"));
        assert_eq!(
            built.sequences,
            vec![format!("{E}]9;Build finished — 42 tests passed{E}\\")]
        );
        assert_eq!(built.lost.len(), 1);
        assert!(built.lost[0].contains("no title field"));
    }

    #[test]
    fn osc9_reports_urgency_and_id_as_inexpressible() {
        let mut req = request(None, "body");
        req.urgency = Some(Urgency::Critical);
        req.id = Some("build".to_owned());
        req.id_explicit = true;
        let built = build(Protocol::Osc9, &req);
        assert_eq!(built.lost.len(), 2);
        assert!(built.lost.iter().any(|note| note.contains("urgency")));
        assert!(built.lost.iter().any(|note| note.contains("--id")));

        // An id that was minted rather than asked for is not a loss worth
        // reporting: nobody wanted it.
        let mut quiet = request(None, "body");
        quiet.id = Some("escapade-1234".to_owned());
        assert!(build(Protocol::Osc9, &quiet).lost.is_empty());
    }

    #[test]
    fn osc9_disambiguates_itself_from_conemu_progress() {
        // ConEmu's taskbar progress is `OSC 9 ; 4 ; <state> ; <progress> ST`, so
        // a message starting `4;` would be read as a progress report.
        let built = build(Protocol::Osc9, &request(None, "4;50 done"));
        assert_eq!(built.sequences, vec![format!("{E}]9; 4;50 done{E}\\")]);
        assert!(built.lost.iter().any(|note| note.contains("progress")));

        // A digit that is not followed by a semicolon is not ambiguous.
        let built = build(Protocol::Osc9, &request(None, "42 tests passed"));
        assert_eq!(built.sequences[0], format!("{E}]9;42 tests passed{E}\\"));
    }

    #[test]
    fn osc9_never_emits_a_control_byte_from_hostile_input() {
        let built = build(
            Protocol::Osc9,
            &request(Some("t\u{1b}\\x"), "b\u{7}\u{1b}]0;pwned\u{7}"),
        );
        let joined = built.joined();
        assert_eq!(joined.matches('\u{1b}').count(), 2, "only OSC and ST");
        assert!(!joined.contains('\u{7}'));
        assert!(joined.starts_with(&format!("{E}]9;")));
        assert!(joined.ends_with(&format!("{E}\\")));
    }

    // -- OSC 777 ------------------------------------------------------------

    #[test]
    fn osc777_gains_a_title() {
        let built = build(
            Protocol::Osc777,
            &request(Some("Build finished"), "42 tests passed"),
        );
        assert_eq!(
            built.sequences,
            vec![format!("{E}]777;notify;Build finished;42 tests passed{E}\\")]
        );
        assert!(built.lost.is_empty());
    }

    #[test]
    fn osc777_keeps_the_title_field_present_when_there_is_no_title() {
        // The fields are positional; omitting one would make the body the title.
        let built = build(Protocol::Osc777, &request(None, "body only"));
        assert_eq!(
            built.sequences,
            vec![format!("{E}]777;notify;;body only{E}\\")]
        );
    }

    #[test]
    fn osc777_cannot_let_a_semicolon_in_the_title_shift_the_fields() {
        let built = build(Protocol::Osc777, &request(Some("a;b"), "body"));
        assert_eq!(
            built.sequences,
            vec![format!("{E}]777;notify;a,b;body{E}\\")]
        );
        // Still exactly three semicolons after the introducer, so the body
        // cannot be pushed into a fourth field.
        assert_eq!(built.sequences[0].matches(';').count(), 3);
        assert!(built.lost.iter().any(|note| note.contains("commas")));
    }

    #[test]
    fn osc777_warns_about_a_semicolon_in_the_body() {
        let built = build(Protocol::Osc777, &request(Some("t"), "done; 42 passed"));
        assert!(built.lost.iter().any(|note| note.contains("Contour")));
    }

    #[test]
    fn osc777_reports_urgency_and_id_as_inexpressible() {
        let mut req = request(Some("t"), "b");
        req.urgency = Some(Urgency::Low);
        req.id = Some("build".to_owned());
        req.id_explicit = true;
        let built = build(Protocol::Osc777, &req);
        assert_eq!(built.lost.len(), 2);

        let mut quiet = request(Some("t"), "b");
        quiet.id = Some("escapade-1234".to_owned());
        assert!(build(Protocol::Osc777, &quiet).lost.is_empty());
    }

    #[test]
    fn osc777_never_emits_a_control_byte_from_hostile_input() {
        let built = build(
            Protocol::Osc777,
            &request(Some("t\u{1b}\\;x"), "b\u{7}\u{9c}\u{1b}[2J"),
        );
        let joined = built.joined();
        assert_eq!(joined.matches('\u{1b}').count(), 2);
        assert!(!joined.contains('\u{7}'));
        assert!(!joined.contains('\u{9c}'));
    }

    // -- OSC 99 -------------------------------------------------------------

    #[test]
    fn osc99_matches_the_specifications_one_line_example() {
        // The spec's own example is `printf '\x1b]99;;Hello world\x1b\\'`.
        let built = build(Protocol::Osc99, &request(None, "Hello world"));
        assert_eq!(
            built.sequences,
            vec![format!("{E}]99;;Hello world{E}\\")]
        );
        assert!(built.lost.is_empty());
    }

    #[test]
    fn osc99_splits_a_title_and_body_across_two_escapes() {
        let mut req = request(Some("Build finished"), "42 tests passed");
        req.id = Some("build".to_owned());
        let built = build(Protocol::Osc99, &req);
        assert_eq!(
            built.sequences,
            vec![
                format!("{E}]99;i=build:p=title:d=0;Build finished{E}\\"),
                format!("{E}]99;i=build:p=body:d=1;42 tests passed{E}\\"),
            ]
        );
    }

    #[test]
    fn osc99_marks_every_escape_but_the_last_as_incomplete() {
        let mut req = request(Some("t"), "b");
        req.id = Some("i".to_owned());
        let built = build(Protocol::Osc99, &req);
        let (last, earlier) = built
            .sequences
            .split_last()
            .expect("at least one sequence");
        for sequence in earlier {
            assert!(sequence.contains("d=0"), "{sequence:?}");
        }
        assert!(last.contains("d=1"), "{last:?}");
    }

    #[test]
    fn osc99_maps_urgency_onto_the_documented_values() {
        for (urgency, wire) in [
            (Urgency::Low, "u=0"),
            (Urgency::Normal, "u=1"),
            (Urgency::Critical, "u=2"),
        ] {
            let mut req = request(None, "body");
            req.urgency = Some(urgency);
            let built = build(Protocol::Osc99, &req);
            assert!(built.sequences[0].contains(wire), "{:?}", built.sequences[0]);
            assert!(built.lost.is_empty(), "urgency is expressible here");
        }
        assert_eq!(Urgency::default(), Urgency::Normal);
    }

    #[test]
    fn osc99_puts_urgency_only_on_the_escape_that_creates_the_notification() {
        let mut req = request(Some("t"), "b");
        req.id = Some("i".to_owned());
        req.urgency = Some(Urgency::Critical);
        let built = build(Protocol::Osc99, &req);
        assert!(built.sequences[0].contains("u=2"));
        assert!(!built.sequences[1].contains("u="));
    }

    #[test]
    fn osc99_carries_the_id_on_every_escape_so_chunks_stay_together() {
        let mut req = request(Some("t"), "b");
        req.id = Some("build-42".to_owned());
        let built = build(Protocol::Osc99, &req);
        for sequence in &built.sequences {
            assert!(sequence.contains("i=build-42"), "{sequence:?}");
        }
    }

    #[test]
    fn osc99_base64_encodes_text_it_cannot_write_literally() {
        let built = build(Protocol::Osc99, &request(None, "done; 42 passed"));
        assert_eq!(built.sequences.len(), 1);
        assert!(built.sequences[0].contains("e=1"), "{:?}", built.sequences[0]);
        // `ZG9uZTsgNDIgcGFzc2Vk` is base64 for "done; 42 passed".
        assert_eq!(
            built.sequences[0],
            format!("{E}]99;e=1;ZG9uZTsgNDIgcGFzc2Vk{E}\\")
        );
        // Exactly the two structural semicolons: the payload's own one is inside
        // the base64 now and cannot be mistaken for a field separator.
        assert_eq!(built.sequences[0].matches(';').count(), 2);
    }

    #[test]
    fn osc99_leaves_text_it_can_write_literally_readable() {
        let built = build(Protocol::Osc99, &request(None, "héllo wörld — ✔"));
        assert!(!built.sequences[0].contains("e=1"));
        assert!(built.sequences[0].contains("héllo wörld — ✔"));
    }

    #[test]
    fn osc99_never_emits_a_control_byte_from_hostile_input() {
        let mut req = request(
            Some("t\u{1b}\\;p=close;x"),
            "b\u{7}\u{1b}]99;i=other:p=close;\u{1b}\\",
        );
        req.id = Some("legit".to_owned());
        let built = build(Protocol::Osc99, &req);
        let joined = built.joined();
        // Two escapes per sequence, and nothing else.
        assert_eq!(joined.matches('\u{1b}').count(), built.sequences.len() * 2);
        assert!(!joined.contains('\u{7}'));
        // The injected `p=close` must have been base64'd, not left as metadata.
        for sequence in &built.sequences {
            assert!(sequence.contains("e=1"), "{sequence:?}");
            let payload = sequence
                .trim_start_matches(&format!("{E}]99;"))
                .trim_end_matches(&format!("{E}\\"));
            let (_, payload) = payload.split_once(';').expect("metadata then payload");
            assert!(!payload.contains(';'), "{payload:?}");
        }
    }

    #[test]
    fn the_unchunkable_protocols_warn_about_a_long_payload() {
        let long = "a".repeat(MAX_PLAIN_PAYLOAD + 1);
        for protocol in [Protocol::Osc9, Protocol::Osc777] {
            let built = build(protocol, &request(None, &long));
            assert_eq!(built.sequences.len(), 1, "{protocol:?} cannot chunk");
            assert!(
                built.lost.iter().any(|note| note.contains("chunk")),
                "{protocol:?} said nothing about {} bytes: {:?}",
                long.len(),
                built.lost
            );
        }
        // OSC 99 chunks instead of warning, because it can.
        let built = build(Protocol::Osc99, &request(None, &long));
        assert_eq!(built.sequences.len(), 2);
        assert!(!built.lost.iter().any(|note| note.contains("truncate")));

        // And a body at the limit is not worth a warning anywhere.
        for protocol in Protocol::RICHEST_FIRST {
            let built = build(protocol, &request(None, &"a".repeat(MAX_PLAIN_PAYLOAD)));
            assert!(built.lost.is_empty(), "{protocol:?}: {:?}", built.lost);
        }
    }

    #[test]
    fn osc99_chunks_exactly_at_the_size_boundary() {
        // Plain payloads are limited to MAX_PLAIN_PAYLOAD bytes per escape.
        let at_limit = "a".repeat(MAX_PLAIN_PAYLOAD);
        assert_eq!(chunk_payload(&at_limit, true).len(), 1);

        let one_over = "a".repeat(MAX_PLAIN_PAYLOAD + 1);
        let chunks = chunk_payload(&one_over, true);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), MAX_PLAIN_PAYLOAD);
        assert_eq!(chunks[1].len(), 1);
        assert_eq!(chunks.concat(), one_over);
    }

    #[test]
    fn osc99_base64_chunks_stop_at_the_raw_limit() {
        let at_limit = "a".repeat(MAX_RAW_CHUNK);
        let chunks = chunk_payload(&at_limit, false);
        assert_eq!(chunks.len(), 1);
        // A multiple of three encodes without padding, which is what MAX_RAW_CHUNK
        // is chosen for.
        assert!(!chunks[0].ends_with('='), "unexpected padding");
        assert!(chunks[0].len() <= 4096, "inside the encoded limit");

        let chunks = chunk_payload(&"a".repeat(MAX_RAW_CHUNK + 1), false);
        assert_eq!(chunks.len(), 2);
        let decoded: Vec<u8> = chunks
            .iter()
            .flat_map(|chunk| BASE64_STANDARD.decode(chunk).expect("valid base64"))
            .collect();
        assert_eq!(decoded.len(), MAX_RAW_CHUNK + 1);
    }

    #[test]
    fn chunk_boundaries_never_split_a_character() {
        // '✔' is three bytes, so 2046 bytes is 682 of them exactly; shifting by
        // one byte forces the split off the multiple-of-three boundary.
        let text = format!("x{}", "✔".repeat(1000));
        for chunk in chunk_payload(&text, false) {
            let decoded = BASE64_STANDARD.decode(&chunk).expect("valid base64");
            assert!(
                std::str::from_utf8(&decoded).is_ok(),
                "a chunk was split mid-character"
            );
        }
    }

    #[test]
    fn a_very_long_body_becomes_several_escapes_that_reassemble() {
        let body = "n".repeat(MAX_RAW_CHUNK * 3 + 17);
        let mut req = request(Some("Long"), &format!("{body};"));
        req.id = Some("long".to_owned());
        let built = build(Protocol::Osc99, &req);
        // One title escape plus four body escapes.
        assert_eq!(built.sequences.len(), 5);
        assert!(built.lost.is_empty(), "{:?}", built.lost);

        let mut rebuilt = Vec::new();
        for sequence in built.sequences.iter().skip(1) {
            assert!(sequence.contains("p=body"));
            assert!(sequence.contains("e=1"));
            let payload = sequence
                .trim_start_matches(&format!("{E}]99;"))
                .trim_end_matches(&format!("{E}\\"))
                .split_once(';')
                .expect("metadata then payload")
                .1
                .to_owned();
            rebuilt.extend(BASE64_STANDARD.decode(&payload).expect("valid base64"));
        }
        assert_eq!(String::from_utf8(rebuilt).expect("utf-8"), format!("{body};"));
    }

    #[test]
    fn an_unidentified_chunked_notification_says_it_may_split_in_two() {
        let long = "a".repeat(MAX_PLAIN_PAYLOAD + 1);
        let built = build(Protocol::Osc99, &request(None, &long));
        assert_eq!(built.sequences.len(), 2);
        assert!(built.lost.iter().any(|note| note.contains("no id")));
    }

    #[test]
    fn an_empty_notification_is_still_a_well_formed_escape() {
        let built = build(Protocol::Osc99, &request(None, ""));
        assert_eq!(built.sequences, vec![format!("{E}]99;;{E}\\")]);

        let built = build(Protocol::Osc9, &request(None, ""));
        assert_eq!(built.sequences, vec![format!("{E}]9;{E}\\")]);

        let built = build(Protocol::Osc777, &request(None, ""));
        assert_eq!(built.sequences, vec![format!("{E}]777;notify;;{E}\\")]);
    }

    #[test]
    fn an_empty_body_with_a_title_does_not_produce_an_empty_body_escape() {
        let mut req = request(Some("Just a title"), "");
        req.id = Some("i".to_owned());
        let built = build(Protocol::Osc99, &req);
        assert_eq!(
            built.sequences,
            vec![format!("{E}]99;i=i;Just a title{E}\\")]
        );
    }

    // -- queries ------------------------------------------------------------

    #[test]
    fn the_query_matches_the_documented_shape() {
        assert_eq!(osc99_query("esc-1"), format!("{E}]99;i=esc-1:p=?;{E}\\"));
        assert_eq!(
            osc99_close("build"),
            format!("{E}]99;i=build:p=close;{E}\\")
        );
    }

    #[test]
    fn parses_a_query_reply() {
        let support = parse_osc99_reply("99;i=esc-1:p=?;p=title,body,close:u=0,1,2:c=1:w=1")
            .expect("a query reply");
        assert_eq!(support.payloads, ["title", "body", "close"]);
        assert_eq!(support.urgencies, ["0", "1", "2"]);
        assert!(support.close_events);
        assert!(support.auto_expire);
    }

    #[test]
    fn ignores_osc_replies_belonging_to_something_else() {
        assert!(parse_osc99_reply("11;rgb:1c1c/1c1c/1c1c").is_none(), "colours");
        assert!(parse_osc99_reply("").is_none());
        assert!(parse_osc99_reply("99;i=x:p=close;").is_none(), "not a query");
        assert!(parse_osc99_reply("99;i=x").is_none(), "no payload field");
    }

    // -- vocabulary ---------------------------------------------------------

    #[test]
    fn protocols_are_ordered_richest_first() {
        assert_eq!(
            Protocol::RICHEST_FIRST,
            [Protocol::Osc99, Protocol::Osc777, Protocol::Osc9]
        );
        // The ordering has to hold as a measurable property, not just as a
        // comment: each protocol expresses strictly less of the same request
        // than the one before it.
        let mut request = request(Some("Title"), "Body");
        request.urgency = Some(Urgency::Critical);
        request.id = Some("id".to_owned());
        request.id_explicit = true;
        let losses: Vec<usize> = Protocol::RICHEST_FIRST
            .iter()
            .map(|protocol| build(*protocol, &request).lost.len())
            .collect();
        assert_eq!(losses, [0, 2, 3], "richest first means fewest losses first");
    }

    #[test]
    fn no_builder_can_be_made_to_emit_two_sequences_from_one_field() {
        // The property that matters: whatever goes in, the number of escape
        // introducers out is fixed by the builder, not by the input.
        let hostile = [
            "\u{1b}]9;injected\u{7}",
            "a;b;c;d",
            "\u{9d}99;;injected\u{9c}",
            "\u{0}\u{1}\u{2}",
        ];
        for protocol in Protocol::RICHEST_FIRST {
            for text in hostile {
                let built = build(protocol, &request(Some(text), text));
                let joined = built.joined();
                assert_eq!(
                    joined.matches("\u{1b}]").count(),
                    built.sequences.len(),
                    "{protocol:?} with {text:?} produced {joined:?}"
                );
            }
        }
    }
}
