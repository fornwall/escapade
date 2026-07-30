//! The kitty graphics protocol on the wire: building commands, chunking
//! payloads, parsing replies.
//!
//! Every command has the same shape:
//!
//! ```text
//! ESC _ G <control data> ; <base64 payload> ESC \
//! ```
//!
//! The control data is a comma-separated list of `key=value` pairs, where every
//! key is a single character and every value an integer or a single character.
//! The payload is arbitrary binary data, base64-encoded so that a terminal which
//! does not implement the protocol cannot be confused by control bytes inside an
//! APC string — and since APC is ignored wholesale by terminals that do not know
//! it, speaking the protocol at an unknown terminal is safe.
//!
//! The keys are terse and heavily overloaded: `c` is the column count of a
//! placement, the base frame of an animation frame *and* the current frame of an
//! animation control command, depending on `a`. Rather than hand callers a bag of
//! `(char, String)` pairs and let them pick wrong, this module has one type per
//! command family — [`Transmit`], [`Placement`], [`Deletion`], [`Animation`],
//! [`Composition`] — and each knows which keys it is allowed to emit.
//!
//! Specification: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>

use std::fmt::{self, Display, Write as _};

use base64::prelude::{BASE64_STANDARD, Engine as _};

use crate::term::{APC, ST};

/// The character that marks an APC string as a graphics command: `ESC _ G …`.
pub const PREFIX: char = 'G';

/// Maximum size of one base64 payload chunk, in bytes.
///
/// The spec requires chunks no larger than this, and that every chunk except the
/// last has a size that is a multiple of four — otherwise a chunk boundary would
/// fall inside a base64 quantum and the terminal could not decode incrementally.
/// 4096 is itself a multiple of four, so splitting at exactly the limit
/// satisfies both rules at once.
pub const CHUNK_LIMIT: usize = 4096;

/// Largest legal image id, image number or placement id.
///
/// Ids are 32-bit and `0` means "unset", so the usable range is `1..=MAX_ID`.
#[expect(
    dead_code,
    reason = "completes the protocol model: the specification states this bound, and \
              writing it down is the only way to record it — `u32` makes it \
              unenforceable, so no code can reference it"
)]
pub const MAX_ID: u32 = 4_294_967_295;

/// The image id used by the support probe, matching the example in the spec.
pub const PROBE_IMAGE_ID: u32 = 31;

/// The string a `t=t` path must contain before a terminal will delete the file.
///
/// The spec makes this a *requirement* on the terminal, not a convention: a
/// temporary file may only be unlinked if it is in a known temporary directory and
/// has this string somewhere in its full path. Which is what stops a stray
/// `a=t,t=t` from talking a terminal into deleting an arbitrary file.
pub const TEMP_FILE_MARKER: &str = "tty-graphics-protocol";

/// z-index below which images are drawn under cells with a non-default
/// background colour, rather than merely under the text: `INT32_MIN / 2`.
pub const Z_BELOW_BACKGROUNDS: i32 = i32::MIN / 2;

// ---------------------------------------------------------------------------
// Typed key values
// ---------------------------------------------------------------------------

/// The `a` key: what a command is asking the terminal to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Action {
    /// `a=t` — transmit image data and stop there. The default.
    #[default]
    Transmit,
    /// `a=T` — transmit and immediately create a placement at the cursor.
    TransmitAndDisplay,
    /// `a=q` — try to load the data, answer, then throw it away. Neither stores
    /// the image nor replaces an existing one with the same id.
    Query,
    /// `a=p` — place an image that was transmitted earlier.
    Put,
    /// `a=d` — delete images or placements.
    Delete,
    /// `a=f` — transmit data for an animation frame.
    Frame,
    /// `a=a` — control an animation: play, stop, set the current frame or a gap.
    Animate,
    /// `a=c` — compose one animation frame's pixels onto another's.
    Compose,
}

impl Action {
    /// The wire character.
    #[must_use]
    pub fn as_char(self) -> char {
        match self {
            Self::Transmit => 't',
            Self::TransmitAndDisplay => 'T',
            Self::Query => 'q',
            Self::Put => 'p',
            Self::Delete => 'd',
            Self::Frame => 'f',
            Self::Animate => 'a',
            Self::Compose => 'c',
        }
    }

    /// One-line description, for the demo's key-by-key walkthrough.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Transmit => "transmit data only",
            Self::TransmitAndDisplay => "transmit data and display it here",
            Self::Query => "ask whether this would work, store nothing",
            Self::Put => "display an image transmitted earlier",
            Self::Delete => "delete images or placements",
            Self::Frame => "transmit an animation frame",
            Self::Animate => "control an animation",
            Self::Compose => "compose one frame onto another",
        }
    }
}

/// The `f` key: how the payload's pixels are encoded.
///
/// A terminal is required to understand all three. Everything else — JPEG, GIF,
/// WebP, SVG — is the client's problem, which is the point: terminals should not
/// have to link image decoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// `f=24` — three bytes per pixel, sRGB, no alpha.
    Rgb,
    /// `f=32` — four bytes per pixel, sRGB with a straight (not premultiplied)
    /// alpha channel. The protocol default.
    #[default]
    Rgba,
    /// `f=100` — a whole PNG file, dimensions read from its IHDR chunk.
    Png,
}

impl Format {
    /// The wire value.
    #[must_use]
    pub fn as_u16(self) -> u16 {
        match self {
            Self::Rgb => 24,
            Self::Rgba => 32,
            Self::Png => 100,
        }
    }

    /// Bytes per pixel, or `None` for a container format whose size is not a
    /// function of the pixel count.
    #[must_use]
    pub fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            Self::Rgb => Some(3),
            Self::Rgba => Some(4),
            Self::Png => None,
        }
    }

    /// Whether `s` and `v` (width and height) must be sent with this format.
    ///
    /// Raw pixels carry no dimensions, so without them the terminal cannot know
    /// where one row ends. PNG carries its own.
    #[must_use]
    pub fn needs_dimensions(self) -> bool {
        self.bytes_per_pixel().is_some()
    }

    /// Parses a wire value.
    ///
    /// Nothing in escapade reads an `f` key back — the terminal never sends one —
    /// so this is the inverse of [`Format::as_u16`] kept for the model's sake, and
    /// exercised only by the tests that pin the three legal values.
    #[allow(
        dead_code,
        reason = "completes the protocol model as the inverse of as_u16; tests use it"
    )]
    #[must_use]
    pub fn parse(value: u16) -> Option<Self> {
        match value {
            24 => Some(Self::Rgb),
            32 => Some(Self::Rgba),
            100 => Some(Self::Png),
            _ => None,
        }
    }
}

/// The `t` key: where the terminal should read the data from.
///
/// The three local media exist because base64 through a pty is the slowest way
/// to move a megabyte between two processes on the same machine. They are only
/// available when the client really is on the same machine, which it cannot know
/// a priori — hence [`Action::Query`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Medium {
    /// `t=d` — direct: the payload *is* the data. The default, and the only one
    /// that works over ssh.
    #[default]
    Direct,
    /// `t=f` — the payload is a path to a regular file the terminal should read
    /// and leave alone.
    File,
    /// `t=t` — a temporary file the terminal deletes after reading. For safety a
    /// terminal should only delete it if it lives in a known temporary directory
    /// *and* has `tty-graphics-protocol` somewhere in its path.
    TempFile,
    /// `t=s` — a POSIX shared memory object (a Windows named shared memory
    /// object on Windows), which the terminal reads, then unlinks and closes.
    ///
    /// Modelled but never sent: creating one means `shm_open`, which needs either
    /// `unsafe` — forbidden crate-wide — or another dependency, and `t=t` already
    /// gets the same benefit through a file the terminal deletes. It stays here
    /// because `explain` documents all four media and because a reply naming `t=s`
    /// has to be describable.
    #[allow(
        dead_code,
        reason = "completes the protocol model: shm_open needs unsafe, which the crate forbids"
    )]
    SharedMemory,
}

impl Medium {
    /// The wire character.
    #[must_use]
    pub fn as_char(self) -> char {
        match self {
            Self::Direct => 'd',
            Self::File => 'f',
            Self::TempFile => 't',
            Self::SharedMemory => 's',
        }
    }

    /// Whether the payload is a path or name rather than the data itself.
    #[must_use]
    pub fn is_indirect(self) -> bool {
        !matches!(self, Self::Direct)
    }
}

/// The `o` key: payload compression, applied before base64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// `o=z` — RFC 1950 zlib deflate. The only value the protocol defines.
    Zlib,
}

impl Compression {
    /// The wire character.
    #[must_use]
    pub fn as_char(self) -> char {
        match self {
            Self::Zlib => 'z',
        }
    }
}

/// The `q` key: how much the terminal should say back.
///
/// Anything that shares a screen with another program — and anything going
/// through a multiplexer — wants at least [`Quiet::SuppressOk`], because an
/// unread `APC G i=1;OK ST` ends up printed as text by whatever reads stdin
/// next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quiet {
    /// `q=0` — report success and failure. The default.
    #[default]
    Verbose,
    /// `q=1` — suppress `OK`, still report errors.
    SuppressOk,
    /// `q=2` — say nothing at all.
    Silent,
}

impl Quiet {
    /// The wire value.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Verbose => 0,
            Self::SuppressOk => 1,
            Self::Silent => 2,
        }
    }
}

/// The `C` key: whether placing an image moves the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorMovement {
    /// `C=0` — move the cursor right by the placement's columns and down by its
    /// rows. The default.
    #[default]
    Advance,
    /// `C=1` — leave the cursor exactly where it is.
    Stay,
}

impl CursorMovement {
    /// The wire value.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Advance => 0,
            Self::Stay => 1,
        }
    }
}

/// The `X` key of a frame transmission, and the `C` key of a composition: how
/// incoming pixels combine with what is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Blend {
    /// Alpha-blend the source over the destination. The default.
    #[default]
    AlphaBlend,
    /// `1` — overwrite the destination pixels, alpha included.
    Overwrite,
}

impl Blend {
    /// The wire value.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::AlphaBlend => 0,
            Self::Overwrite => 1,
        }
    }
}

/// A rectangle in image pixels, used for source crops, frame updates and
/// composition regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    /// Left edge, in pixels from the left of the image (`x`).
    pub x: u32,
    /// Top edge, in pixels from the top of the image (`y`).
    pub y: u32,
    /// Width in pixels (`w`, or `s` when transmitting frame data).
    pub width: u32,
    /// Height in pixels (`h`, or `v` when transmitting frame data).
    pub height: u32,
}

impl Rect {
    /// A rectangle at the origin.
    #[must_use]
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// The `d` key: what a delete command should remove.
///
/// Every variant has a lowercase and an uppercase spelling. Lowercase deletes
/// the *placements* but keeps the image data, so the same image can be shown
/// again without retransmitting it; uppercase frees the data too. That
/// distinction is the whole reason a file manager can scroll through a directory
/// of thumbnails without resending anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteTarget {
    /// `d=a` — every placement visible on screen.
    Visible,
    /// `d=i` — the image with this id, optionally only one of its placements.
    Id {
        /// Image id, sent as `i`.
        image: u32,
        /// Placement id, sent as `p`; without it, every placement goes.
        placement: Option<u32>,
    },
    /// `d=n` — the newest image with this image *number*, sent as `I`.
    Number {
        /// Image number.
        number: u32,
        /// Placement id, sent as `p`.
        placement: Option<u32>,
    },
    /// `d=c` — every placement intersecting the cursor's cell.
    AtCursor,
    /// `d=f` — the animation frames of an image.
    //
    // The control-data table says only "Delete animation frames"; it does not
    // spell out which keys select the image. kitty's implementation resolves the
    // image from `i`/`I` like every other frame command, which is what this
    // sends. Flagged rather than assumed silently.
    Frames {
        /// Image id, sent as `i`.
        image: u32,
    },
    /// `d=p` — every placement intersecting one cell, given by `x` and `y`.
    /// These are 1-based cursor coordinates, so `x=1,y=1` is the top left cell.
    Cell {
        /// Column, 1-based.
        col: u32,
        /// Row, 1-based.
        row: u32,
    },
    /// `d=q` — like [`DeleteTarget::Cell`], but only at one z-index.
    CellAtZ {
        /// Column, 1-based.
        col: u32,
        /// Row, 1-based.
        row: u32,
        /// z-index, sent as `z`.
        z: i32,
    },
    /// `d=r` — every image whose id is in an inclusive range, the ends given by
    /// `x` and `y`. Added in kitty 0.33.0.
    IdRange {
        /// Lowest id to delete, sent as `x`.
        first: u32,
        /// Highest id to delete, sent as `y`.
        last: u32,
    },
    /// `d=x` — every placement intersecting a column, given by `x`.
    Column(u32),
    /// `d=y` — every placement intersecting a row, given by `y`.
    Row(u32),
    /// `d=z` — every placement at one z-index, given by `z`.
    ZIndex(i32),
}

impl DeleteTarget {
    /// The lowercase wire character, which keeps the image data.
    #[must_use]
    pub fn as_char(self) -> char {
        match self {
            Self::Visible => 'a',
            Self::Id { .. } => 'i',
            Self::Number { .. } => 'n',
            Self::AtCursor => 'c',
            Self::Frames { .. } => 'f',
            Self::Cell { .. } => 'p',
            Self::CellAtZ { .. } => 'q',
            Self::IdRange { .. } => 'r',
            Self::Column(_) => 'x',
            Self::Row(_) => 'y',
            Self::ZIndex(_) => 'z',
        }
    }

    /// Whether this target can reach a *virtual* placement — the invisible
    /// prototype behind a Unicode placeholder.
    ///
    /// Virtual placements have no location on screen, so every position-based
    /// target misses them. Only `i`, `I`, `r`, `R`, `n` and `N` can delete one.
    #[must_use]
    pub fn reaches_virtual_placements(self) -> bool {
        matches!(
            self,
            Self::Id { .. } | Self::Number { .. } | Self::IdRange { .. }
        )
    }
}

/// A delete command: `APC G a=d,d=<target>[,…] ST`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deletion {
    /// What to remove.
    pub target: DeleteTarget,
    /// Whether to free the stored image data as well, which is what the
    /// uppercase spelling of the `d` value means.
    pub free_data: bool,
    /// Response suppression.
    pub quiet: Quiet,
}

impl Deletion {
    /// A deletion that keeps the image data, so it can be redisplayed.
    #[must_use]
    pub fn soft(target: DeleteTarget) -> Self {
        Self {
            target,
            free_data: false,
            quiet: Quiet::Verbose,
        }
    }

    /// A deletion that frees the image data too.
    #[must_use]
    pub fn hard(target: DeleteTarget) -> Self {
        Self {
            target,
            free_data: true,
            quiet: Quiet::Verbose,
        }
    }

    /// The `d` value: the target's character, uppercased when freeing data.
    #[must_use]
    pub fn key(self) -> char {
        let base = self.target.as_char();
        if self.free_data {
            base.to_ascii_uppercase()
        } else {
            base
        }
    }
}

/// The `s` key of an animation control command: what the animation should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationState {
    /// `s=1` — stop, which also resets the loop counter.
    Stop,
    /// `s=2` — run, but wait at the last frame for more to arrive instead of
    /// looping. This is how a client streams an animation it is still producing.
    Loading,
    /// `s=3` — run, looping back to the first frame at the end.
    Run,
}

impl AnimationState {
    /// The wire value.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Stop => 1,
            Self::Loading => 2,
            Self::Run => 3,
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Control data being assembled, in the order the keys will appear.
///
/// Order carries no meaning on the wire, but a stable order makes the output
/// diffable, the tests literal and the demo's explanations line up with what
/// the reader sees.
#[derive(Debug, Default)]
struct Keys(Vec<String>);

impl Keys {
    /// Appends `key=value`.
    fn put(&mut self, key: char, value: impl Display) {
        self.0.push(format!("{key}={value}"));
    }

    /// Appends `key=value` when the value is present.
    fn put_some(&mut self, key: char, value: Option<impl Display>) {
        if let Some(value) = value {
            self.put(key, value);
        }
    }

    /// Appends `key=value` unless the value is the protocol default.
    fn put_non_default<T: Display + PartialEq>(&mut self, key: char, value: T, default: &T) {
        if value != *default {
            self.put(key, value);
        }
    }

    /// The comma-separated control data.
    fn finish(self) -> String {
        self.0.join(",")
    }
}

/// One complete escape sequence, kept in two halves.
///
/// Control data and payload stay separate so that a sequence can be *shown*
/// without dumping a quarter of a megabyte of base64 at the reader —
/// [`Sequence::display`] elides the payload and says how big it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequence {
    /// The control data: everything between `APC G` and the `;`.
    pub control: String,
    /// The base64 payload, possibly empty.
    pub payload: String,
}

impl Sequence {
    /// A sequence with no payload, such as a placement or a deletion.
    #[must_use]
    pub fn control_only(control: String) -> Self {
        Self {
            control,
            payload: String::new(),
        }
    }

    /// The bytes to write to the terminal.
    #[must_use]
    pub fn wire(&self) -> String {
        if self.payload.is_empty() {
            format!("{APC}{PREFIX}{}{ST}", self.control)
        } else {
            format!("{APC}{PREFIX}{};{}{ST}", self.control, self.payload)
        }
    }

    /// A printable rendering, with a payload longer than `max_payload` replaced
    /// by a byte count.
    ///
    /// Note that this is *not* a valid sequence — it is text about a sequence.
    #[must_use]
    pub fn display(&self, max_payload: usize) -> String {
        let payload = if self.payload.is_empty() {
            String::new()
        } else if self.payload.len() <= max_payload {
            format!(";{}", self.payload)
        } else {
            let head: String = self.payload.chars().take(max_payload).collect();
            format!(
                ";{head}… <{} more base64 bytes>",
                self.payload.len() - head.len()
            )
        };
        crate::term::pretty(&format!("{APC}{PREFIX}{}{payload}{ST}", self.control))
    }

    /// Total wire size in bytes, payload and framing included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.wire().len()
    }

    /// Whether the sequence carries no payload.
    #[expect(
        dead_code,
        reason = "clippy::len_without_is_empty requires this alongside len(), which \
                  render uses to total up the bytes on the wire"
    )]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

impl Display for Sequence {
    /// Renders with the payload elided, so a `Sequence` can never accidentally
    /// be printed as executable bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display(0))
    }
}

/// How an image, or one animation frame of it, is being transmitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmit {
    /// `a` — [`Action::Transmit`], [`Action::TransmitAndDisplay`],
    /// [`Action::Query`] or [`Action::Frame`].
    pub action: Action,
    /// `f` — the payload's pixel format.
    pub format: Format,
    /// `t` — where the terminal reads the data from.
    pub medium: Medium,
    /// `s` — image width in pixels. Required for `f=24` and `f=32`.
    pub width: Option<u32>,
    /// `v` — image height in pixels. Required for `f=24` and `f=32`.
    pub height: Option<u32>,
    /// `i` — image id. Mutually exclusive with [`Transmit::image_number`].
    pub image_id: Option<u32>,
    /// `I` — image number, for when the client cannot know which ids are free.
    /// The terminal answers with the id it allocated.
    pub image_number: Option<u32>,
    /// `o` — compression applied before base64.
    pub compression: Option<Compression>,
    /// `S` — how many bytes to read, needed when combining PNG with
    /// compression, or when reading part of a file.
    pub data_size: Option<u32>,
    /// `O` — byte offset to start reading a file or shared memory object at.
    pub data_offset: Option<u32>,
    /// `N` — usage hints. `1` is *transient*: the terminal may evict this image
    /// first under storage pressure.
    pub usage_hint: Option<u32>,
    /// `q` — response suppression.
    pub quiet: Quiet,
    /// Frame keys, when `a=f`.
    pub frame: Option<FrameData>,
}

impl Transmit {
    /// A PNG transmission: `f=100`, dimensions read from the payload's IHDR.
    #[must_use]
    pub fn png() -> Self {
        Self {
            action: Action::Transmit,
            format: Format::Png,
            medium: Medium::Direct,
            width: None,
            height: None,
            image_id: None,
            image_number: None,
            compression: None,
            data_size: None,
            data_offset: None,
            usage_hint: None,
            quiet: Quiet::Verbose,
            frame: None,
        }
    }

    /// A raw-pixel transmission of the given format and dimensions.
    #[must_use]
    pub fn pixels(format: Format, width: u32, height: u32) -> Self {
        Self {
            format,
            width: Some(width),
            height: Some(height),
            ..Self::png()
        }
    }

    /// Sets the image id.
    #[must_use]
    pub fn with_id(mut self, id: u32) -> Self {
        self.image_id = Some(id);
        self
    }

    /// Sets the action.
    #[must_use]
    pub fn with_action(mut self, action: Action) -> Self {
        self.action = action;
        self
    }

    /// Sets response suppression.
    #[must_use]
    pub fn with_quiet(mut self, quiet: Quiet) -> Self {
        self.quiet = quiet;
        self
    }

    /// The expected payload size in bytes for a raw-pixel format, or `None` for
    /// PNG or when the dimensions are missing.
    #[must_use]
    pub fn expected_payload_len(&self) -> Option<usize> {
        let bytes_per_pixel = self.format.bytes_per_pixel()?;
        let width = self.width? as usize;
        let height = self.height? as usize;
        Some(width * height * bytes_per_pixel)
    }

    /// The control data for the first (or only) chunk, without the `m` key.
    fn control(&self, placement: Option<&Placement>) -> String {
        let mut keys = Keys::default();
        keys.put('a', self.action.as_char());
        keys.put('f', self.format.as_u16());
        keys.put_non_default('t', self.medium.as_char(), &Medium::Direct.as_char());
        keys.put_some('s', self.width);
        keys.put_some('v', self.height);
        keys.put_some('i', self.image_id);
        keys.put_some('I', self.image_number);
        if let Some(compression) = self.compression {
            keys.put('o', compression.as_char());
        }
        keys.put_some('S', self.data_size);
        keys.put_some('O', self.data_offset);
        keys.put_some('N', self.usage_hint);
        if let Some(frame) = &self.frame {
            frame.write_keys(&mut keys);
        }
        if let Some(placement) = placement {
            placement.write_display_keys(&mut keys);
        }
        keys.put_non_default('q', self.quiet.as_u8(), &0);
        keys.finish()
    }

    /// The control data continuation chunks are allowed to carry.
    ///
    /// The spec is strict here: after the first chunk, only `m` and optionally
    /// `q` may appear — plus `a=f` when the data belongs to an animation frame,
    /// because otherwise the terminal cannot tell frame data from image data.
    fn continuation_control(&self, more: bool) -> String {
        let mut keys = Keys::default();
        if self.action == Action::Frame {
            keys.put('a', Action::Frame.as_char());
        }
        keys.put_non_default('q', self.quiet.as_u8(), &0);
        keys.put('m', u8::from(more));
        keys.finish()
    }
}

/// The `a=f` keys: how a frame's pixels combine with a canvas.
///
/// A frame is built by composing the transmitted pixels onto a background, which
/// is either a flat colour or the pixels of an earlier frame. That is what makes
/// a mostly-static animation cheap: send the background once as the root frame,
/// then send only the moving rectangle for each frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameData {
    /// `x`, `y` — where in the image the transmitted rectangle lands. Its size
    /// comes from the `s` and `v` keys of the transmission.
    pub at: (u32, u32),
    /// `c` — 1-based frame number whose pixels are the canvas. `c=1` is the root
    /// frame, i.e. the base image data. Without it the canvas is
    /// [`FrameData::background`].
    pub base_frame: Option<u32>,
    /// `r` — 1-based frame number to edit instead of creating a new frame. The
    /// canvas is then the existing frame itself, which is how a frame made of
    /// several rectangles is assembled.
    pub edit_frame: Option<u32>,
    /// `z` — milliseconds to wait before the next frame. Zero is ignored;
    /// negative makes a *gapless* frame, which is never shown but can serve as
    /// base data. Unspecified means 40ms, except on the root frame, which
    /// defaults to no gap at all.
    pub gap: Option<i32>,
    /// `X` — how the transmitted pixels combine with the canvas.
    pub blend: Blend,
    /// `Y` — canvas colour as 32-bit RGBA, used when no base frame is given.
    /// The default, `0`, is a transparent black pixel.
    pub background: Option<u32>,
}

impl FrameData {
    fn write_keys(&self, keys: &mut Keys) {
        keys.put_non_default('x', self.at.0, &0);
        keys.put_non_default('y', self.at.1, &0);
        keys.put_some('c', self.base_frame);
        keys.put_some('r', self.edit_frame);
        keys.put_some('z', self.gap);
        keys.put_non_default('X', self.blend.as_u8(), &0);
        keys.put_some('Y', self.background);
    }
}

/// Where and how an image is drawn: `a=p`, or the display half of `a=T`.
//
// `placement_id` and `virtual_placement` repeat the type's name because the
// protocol does: "placement id" and "virtual placement" are the spec's own terms,
// and renaming them here would cost a reader the connection to the document.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Placement {
    /// `i` — the image to place.
    pub image_id: Option<u32>,
    /// `I` — the image number to place, resolved to the newest such image.
    pub image_number: Option<u32>,
    /// `p` — an id for this placement, so it can later be moved, replaced or
    /// deleted on its own. A placement is identified by the *pair* of image id
    /// and placement id.
    pub placement_id: Option<u32>,
    /// `c` — how many columns to display over. The terminal scales to fit.
    pub cols: Option<u16>,
    /// `r` — how many rows to display over.
    pub rows: Option<u16>,
    /// `x`, `y`, `w`, `h` — the part of the source image to show. The displayed
    /// area is the intersection of this with the image, so an oversized
    /// rectangle is harmless.
    pub source: Option<Rect>,
    /// `X`, `Y` — pixel offsets inside the first cell, which must be smaller
    /// than a cell. This is how an image is positioned off the cell grid.
    pub cell_offset: (u32, u32),
    /// `z` — stacking order. Negative draws under the text, which is what makes
    /// text-over-image possible; below [`Z_BELOW_BACKGROUNDS`] it goes under
    /// non-default cell background colours too.
    pub z: Option<i32>,
    /// `C` — whether the cursor moves past the image afterwards.
    pub cursor: CursorMovement,
    /// `U=1` — create a *virtual* placement, the invisible prototype a Unicode
    /// placeholder refers to. The image is not drawn by this command at all.
    pub virtual_placement: bool,
    /// `P`, `Q`, `H`, `V` — position relative to another placement.
    pub relative: Option<Relative>,
    /// `q` — response suppression.
    pub quiet: Quiet,
}

impl Placement {
    /// A placement of an image by id.
    #[must_use]
    pub fn of_image(image_id: u32) -> Self {
        Self {
            image_id: Some(image_id),
            ..Self::default()
        }
    }

    /// Sets the cell area to scale into.
    #[must_use]
    pub fn with_area(mut self, cols: Option<u16>, rows: Option<u16>) -> Self {
        self.cols = cols;
        self.rows = rows;
        self
    }

    /// Sets the source rectangle to crop to.
    #[must_use]
    pub fn with_source(mut self, source: Option<Rect>) -> Self {
        self.source = source;
        self
    }

    /// Sets the stacking order.
    #[must_use]
    pub fn with_z(mut self, z: Option<i32>) -> Self {
        self.z = z;
        self
    }

    /// Turns this into a virtual placement for a Unicode placeholder.
    ///
    /// Virtual placements need an explicit `c` and `r`, because there is no
    /// cursor position to derive an area from — the placeholder cells are the
    /// area.
    #[must_use]
    pub fn virtualized(mut self, cols: u16, rows: u16) -> Self {
        self.virtual_placement = true;
        self.cols = Some(cols);
        self.rows = Some(rows);
        self
    }

    /// Sets response suppression.
    #[must_use]
    pub fn with_quiet(mut self, quiet: Quiet) -> Self {
        self.quiet = quiet;
        self
    }

    /// Writes the keys that describe *where and how*, without `a` or `i`.
    ///
    /// Shared with `a=T`, which is a transmission and a placement in one
    /// command and therefore emits both key sets.
    fn write_display_keys(&self, keys: &mut Keys) {
        keys.put_some('p', self.placement_id);
        if let Some(source) = self.source {
            keys.put_non_default('x', source.x, &0);
            keys.put_non_default('y', source.y, &0);
            keys.put_non_default('w', source.width, &0);
            keys.put_non_default('h', source.height, &0);
        }
        keys.put_some('c', self.cols);
        keys.put_some('r', self.rows);
        keys.put_non_default('X', self.cell_offset.0, &0);
        keys.put_non_default('Y', self.cell_offset.1, &0);
        keys.put_some('z', self.z);
        keys.put_non_default('C', self.cursor.as_u8(), &0);
        if self.virtual_placement {
            keys.put('U', 1);
        }
        if let Some(relative) = self.relative {
            relative.write_keys(keys);
        }
    }
}

/// The `P`, `Q`, `H`, `V` keys: position this placement relative to another.
///
/// A relative placement follows its parent when the parent moves, and dies when
/// the parent does. Chained with a Unicode placeholder as the parent, this is how
/// an image is made to scroll with text while still being positioned precisely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Relative {
    /// `P` — the parent's image id.
    pub parent_image: u32,
    /// `Q` — the parent's placement id.
    pub parent_placement: u32,
    /// `H` — horizontal offset in cells from the parent's top left cell.
    /// Positive is right.
    pub horizontal: i32,
    /// `V` — vertical offset in cells. Positive is down.
    pub vertical: i32,
}

impl Relative {
    fn write_keys(&self, keys: &mut Keys) {
        keys.put('P', self.parent_image);
        keys.put('Q', self.parent_placement);
        keys.put_non_default('H', self.horizontal, &0);
        keys.put_non_default('V', self.vertical, &0);
    }
}

/// An animation control command: `a=a`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Animation {
    /// `i` — the image whose animation this controls. Required.
    pub image_id: u32,
    /// `s` — play, stop, or play-and-wait-for-more.
    pub state: Option<AnimationState>,
    /// `c` — make this 1-based frame the current one. This is a client-driven
    /// animation: the client is the clock.
    pub current_frame: Option<u32>,
    /// `r` — the 1-based frame whose gap [`Animation::gap`] sets. The root
    /// frame's gap can only be set this way, since it is created with the base
    /// image data and has no gap of its own.
    pub frame: Option<u32>,
    /// `z` — gap in milliseconds before the next frame.
    pub gap: Option<i32>,
    /// `v` — loop count. `1` loops forever and is the default; any larger `n`
    /// plays `n - 1` loops.
    pub loops: Option<u32>,
    /// `q` — response suppression.
    pub quiet: Quiet,
}

impl Animation {
    /// Starts a looping, terminal-driven animation.
    #[must_use]
    pub fn play(image_id: u32) -> Self {
        Self {
            image_id,
            state: Some(AnimationState::Run),
            ..Self::default()
        }
    }

    /// Stops an animation, resetting its loop counter.
    #[must_use]
    pub fn stop(image_id: u32) -> Self {
        Self {
            image_id,
            state: Some(AnimationState::Stop),
            ..Self::default()
        }
    }

    /// Sets one frame's gap, in milliseconds.
    #[must_use]
    pub fn set_gap(image_id: u32, frame: u32, gap: i32) -> Self {
        Self {
            image_id,
            frame: Some(frame),
            gap: Some(gap),
            ..Self::default()
        }
    }
}

/// A frame composition command: `a=c`, copying pixels between frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Composition {
    /// `i` — the image both frames belong to.
    pub image_id: u32,
    /// `r` — 1-based source frame, whose pixels are read.
    pub source_frame: u32,
    /// `c` — 1-based destination frame, which is edited.
    pub destination_frame: u32,
    /// `w`, `h` — size of the rectangle. Defaulting to the full image.
    pub size: Option<(u32, u32)>,
    /// `X`, `Y` — top left of the source rectangle.
    pub source_at: (u32, u32),
    /// `x`, `y` — top left of the destination rectangle.
    pub destination_at: (u32, u32),
    /// `C` — how source pixels combine with destination pixels.
    pub blend: Blend,
    /// `q` — response suppression.
    pub quiet: Quiet,
}

// ---------------------------------------------------------------------------
// Building sequences
// ---------------------------------------------------------------------------

/// `APC G i=31,s=1,v=1,a=q,t=d,f=24 ; AAAA ST` — the support query.
///
/// This is the sequence the spec recommends for capability detection, down to
/// the id: a single black pixel, sent with `a=q` so the terminal validates it,
/// answers, and stores nothing. Pair it with a Primary Device Attributes request
/// as a barrier — see [`super::probe`].
#[must_use]
pub fn support_query() -> String {
    // Written out literally rather than through `Transmit`, because the exact
    // byte sequence is quoted in the specification and a test asserts it.
    format!("{APC}{PREFIX}i={PROBE_IMAGE_ID},s=1,v=1,a=q,t=d,f=24;AAAA{ST}")
}

/// Splits base64 text into chunks no larger than [`CHUNK_LIMIT`].
///
/// Every chunk except the last comes out a multiple of four bytes long, as the
/// protocol requires, because the limit itself is a multiple of four.
#[must_use]
pub fn chunk_base64(encoded: &str) -> Vec<&str> {
    if encoded.is_empty() {
        return Vec::new();
    }
    // Base64 is ASCII, so byte slicing cannot split a character.
    encoded
        .as_bytes()
        .chunks(CHUNK_LIMIT)
        .filter_map(|chunk| std::str::from_utf8(chunk).ok())
        .collect()
}

/// Builds the sequences that transmit `data`, and optionally display it.
///
/// The payload is base64-encoded and split into chunks; the first chunk carries
/// the full control data, every later chunk carries only `m` (and `q`, and `a=f`
/// for frame data). `m=1` means "more to come", `m=0` marks the last chunk. A
/// single-chunk transmission omits `m` entirely, since its default is `0`.
///
/// Nothing is displayed until the terminal has the whole sequence, and no other
/// graphics command may be interleaved with the chunks.
#[must_use]
pub fn transmit(command: &Transmit, placement: Option<&Placement>, data: &[u8]) -> Vec<Sequence> {
    let encoded = BASE64_STANDARD.encode(data);
    let chunks = chunk_base64(&encoded);
    let first_control = command.control(placement);

    if chunks.len() <= 1 {
        return vec![Sequence {
            control: first_control,
            payload: chunks.first().map(|c| (*c).to_owned()).unwrap_or_default(),
        }];
    }

    let mut sequences = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.iter().enumerate() {
        let last = index + 1 == chunks.len();
        let control = if index == 0 {
            let mut control = first_control.clone();
            let _ = write!(control, ",m=1");
            control
        } else {
            command.continuation_control(!last)
        };
        sequences.push(Sequence {
            control,
            payload: (*chunk).to_owned(),
        });
    }
    sequences
}

/// `APC G a=p,i=<id>[,c=…,r=…,z=…] ST` — display an image transmitted earlier.
#[must_use]
pub fn place(placement: &Placement) -> Sequence {
    let mut keys = Keys::default();
    keys.put('a', Action::Put.as_char());
    keys.put_some('i', placement.image_id);
    keys.put_some('I', placement.image_number);
    placement.write_display_keys(&mut keys);
    keys.put_non_default('q', placement.quiet.as_u8(), &0);
    Sequence::control_only(keys.finish())
}

/// `APC G a=d[,d=<what>,…] ST` — delete images or placements.
///
/// With no `d` key at all this deletes every placement visible on screen, which
/// is the sequence to send when a full-screen application exits.
#[must_use]
pub fn delete(deletion: &Deletion) -> Sequence {
    let mut keys = Keys::default();
    keys.put('a', Action::Delete.as_char());
    keys.put('d', deletion.key());
    match deletion.target {
        DeleteTarget::Visible | DeleteTarget::AtCursor => {}
        DeleteTarget::Id { image, placement } => {
            keys.put('i', image);
            keys.put_some('p', placement);
        }
        DeleteTarget::Number { number, placement } => {
            keys.put('I', number);
            keys.put_some('p', placement);
        }
        DeleteTarget::Frames { image } => keys.put('i', image),
        DeleteTarget::Cell { col, row } => {
            keys.put('x', col);
            keys.put('y', row);
        }
        DeleteTarget::CellAtZ { col, row, z } => {
            keys.put('x', col);
            keys.put('y', row);
            keys.put('z', z);
        }
        DeleteTarget::IdRange { first, last } => {
            keys.put('x', first);
            keys.put('y', last);
        }
        DeleteTarget::Column(col) => keys.put('x', col),
        DeleteTarget::Row(row) => keys.put('y', row),
        DeleteTarget::ZIndex(z) => keys.put('z', z),
    }
    keys.put_non_default('q', deletion.quiet.as_u8(), &0);
    Sequence::control_only(keys.finish())
}

/// `APC G a=a,i=<id>[,s=…,c=…,r=…,z=…,v=…] ST` — control an animation.
#[must_use]
pub fn animate(animation: &Animation) -> Sequence {
    let mut keys = Keys::default();
    keys.put('a', Action::Animate.as_char());
    keys.put('i', animation.image_id);
    if let Some(state) = animation.state {
        keys.put('s', state.as_u8());
    }
    keys.put_some('r', animation.frame);
    keys.put_some('c', animation.current_frame);
    keys.put_some('z', animation.gap);
    keys.put_some('v', animation.loops);
    keys.put_non_default('q', animation.quiet.as_u8(), &0);
    Sequence::control_only(keys.finish())
}

/// `APC G a=c,i=<id>,r=<src>,c=<dst>,w=…,h=…,X=…,Y=…,x=…,y=… ST` — compose one
/// animation frame's pixels onto another's.
#[must_use]
pub fn compose(composition: &Composition) -> Sequence {
    let mut keys = Keys::default();
    keys.put('a', Action::Compose.as_char());
    keys.put('i', composition.image_id);
    keys.put('r', composition.source_frame);
    keys.put('c', composition.destination_frame);
    if let Some((width, height)) = composition.size {
        keys.put('w', width);
        keys.put('h', height);
    }
    keys.put_non_default('X', composition.source_at.0, &0);
    keys.put_non_default('Y', composition.source_at.1, &0);
    keys.put_non_default('x', composition.destination_at.0, &0);
    keys.put_non_default('y', composition.destination_at.1, &0);
    keys.put_non_default('C', composition.blend.as_u8(), &0);
    keys.put_non_default('q', composition.quiet.as_u8(), &0);
    Sequence::control_only(keys.finish())
}

// ---------------------------------------------------------------------------
// Parsing replies
// ---------------------------------------------------------------------------

/// An error code from a failed graphics command.
///
/// The codes are borrowed from `errno`, which makes most of them guessable, and
/// arrive as `CODE:<detail>` in the reply's message. The set below is the one
/// kitty actually emits; anything else is preserved verbatim in
/// [`ErrorCode::Other`] rather than being flattened into a generic failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    /// `EBADPNG` — the payload was not a PNG the terminal could decode.
    BadPng,
    /// `EBADF` — a file could not be opened, or was not a regular file.
    BadFile,
    /// `ECYCLE` — the relative placement would form a cycle.
    Cycle,
    /// `EFBIG` — the image or its dimensions exceed what the terminal allows.
    TooBig,
    /// `EILSEQ` — the payload was not decodable: bad base64, bad zlib, or a
    /// pixel count that disagrees with `s` and `v`.
    BadSequence,
    /// `EINVAL` — the control data does not make sense, such as sending both `i`
    /// and `I`, or making a virtual placement relative.
    Invalid,
    /// `ENODATA` — a command referred to data that was never sent.
    NoData,
    /// `ENOENT` — no such image, placement or frame. The reply an application
    /// gets when it assumed an id survived and it did not.
    NotFound,
    /// `ENOMEM` — the terminal could not allocate.
    OutOfMemory,
    /// `ENOPARENT` — the parent placement of a relative placement is gone.
    NoParent,
    /// `ENOSPC` — the image storage quota is full.
    NoSpace,
    /// `EPERM` — the terminal refused to read the path, e.g. under `/proc`.
    Denied,
    /// `ETOODEEP` — the chain of relative placements is longer than allowed.
    TooDeep,
    /// A code this build does not know about.
    Other(String),
}

impl ErrorCode {
    /// Parses a code name.
    #[must_use]
    pub fn parse(code: &str) -> Self {
        match code {
            "EBADPNG" => Self::BadPng,
            "EBADF" => Self::BadFile,
            "ECYCLE" => Self::Cycle,
            "EFBIG" => Self::TooBig,
            "EILSEQ" => Self::BadSequence,
            "EINVAL" => Self::Invalid,
            "ENODATA" => Self::NoData,
            "ENOENT" => Self::NotFound,
            "ENOMEM" => Self::OutOfMemory,
            "ENOPARENT" => Self::NoParent,
            "ENOSPC" => Self::NoSpace,
            "EPERM" => Self::Denied,
            "ETOODEEP" => Self::TooDeep,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The code name as it appears on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::BadPng => "EBADPNG",
            Self::BadFile => "EBADF",
            Self::Cycle => "ECYCLE",
            Self::TooBig => "EFBIG",
            Self::BadSequence => "EILSEQ",
            Self::Invalid => "EINVAL",
            Self::NoData => "ENODATA",
            Self::NotFound => "ENOENT",
            Self::OutOfMemory => "ENOMEM",
            Self::NoParent => "ENOPARENT",
            Self::NoSpace => "ENOSPC",
            Self::Denied => "EPERM",
            Self::TooDeep => "ETOODEEP",
            Self::Other(code) => code,
        }
    }

    /// What it means, in a sentence a user can act on.
    #[must_use]
    pub fn explain(&self) -> &'static str {
        match self {
            Self::BadPng => "the terminal could not decode the PNG",
            Self::BadFile => "the terminal could not open the file, or it was not a regular file",
            Self::Cycle => "the relative placement would form a cycle",
            Self::TooBig => "the image is larger than the terminal allows",
            Self::BadSequence => "the payload did not decode — bad base64, bad zlib, or the pixel count disagrees with the declared size",
            Self::Invalid => "the control data was rejected as invalid",
            Self::NoData => "the command referred to data that was never transmitted",
            Self::NotFound => "no image, placement or frame with that id — it needs retransmitting",
            Self::OutOfMemory => "the terminal could not allocate memory for the image",
            Self::NoParent => "the parent placement no longer exists",
            Self::NoSpace => "the terminal's image storage quota is full",
            Self::Denied => "the terminal refused to read that path",
            Self::TooDeep => "the chain of relative placements is too long",
            Self::Other(_) => "an error this build of escapade does not recognise",
        }
    }
}

impl Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the terminal said about a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The message was `OK`.
    Ok,
    /// The message was an error code, with any detail that followed the colon.
    Error {
        /// The code.
        code: ErrorCode,
        /// Human-readable detail from the terminal, when it sent any.
        detail: Option<String>,
    },
}

/// A parsed graphics reply: `APC G i=<id>[,I=…][,p=…] ; <message> ST`.
///
/// A reply always echoes the ids from the command it answers, which is how a
/// client with several requests in flight matches answers to questions. When the
/// command used an image *number* (`I`), the reply also carries the `i` the
/// terminal allocated — that is the only way to learn it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicsReply {
    /// `i` — the image id, allocated by the terminal if the command sent `I`.
    pub image_id: Option<u32>,
    /// `I` — the image number, echoed back.
    pub image_number: Option<u32>,
    /// `p` — the placement id, echoed back.
    pub placement_id: Option<u32>,
    /// Success or failure.
    pub outcome: Outcome,
}

impl GraphicsReply {
    /// Whether the command succeeded.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.outcome == Outcome::Ok
    }

    /// The error code, when the command failed.
    #[must_use]
    pub fn error(&self) -> Option<&ErrorCode> {
        match &self.outcome {
            Outcome::Ok => None,
            Outcome::Error { code, .. } => Some(code),
        }
    }
}

/// Parses the payload of an APC string — the bytes between `ESC _` and `ST`.
///
/// Returns `None` when the payload belongs to some other APC protocol, which is
/// entirely normal: anything not starting with `G` is none of our business.
#[must_use]
pub fn parse_reply(payload: &str) -> Option<GraphicsReply> {
    let body = payload.strip_prefix(PREFIX)?;
    // The message is separated from the control data by the first `;`. Error
    // detail text can itself contain colons but never a semicolon, since the
    // terminal is required to send printable ASCII and spaces only.
    let (control, message) = body.split_once(';')?;

    let mut reply = GraphicsReply {
        image_id: None,
        image_number: None,
        placement_id: None,
        outcome: parse_message(message),
    };
    for field in control.split(',') {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let number = value.parse::<u32>().ok();
        match key {
            "i" => reply.image_id = number,
            "I" => reply.image_number = number,
            "p" => reply.placement_id = number,
            _ => {}
        }
    }
    Some(reply)
}

/// Parses a reply message: `OK`, `CODE`, or `CODE:detail`.
fn parse_message(message: &str) -> Outcome {
    let message = message.trim();
    if message == "OK" {
        return Outcome::Ok;
    }
    let (code, detail) = match message.split_once(':') {
        Some((code, detail)) => (code, Some(detail.trim().to_owned())),
        None => (message, None),
    };
    Outcome::Error {
        code: ErrorCode::parse(code.trim()),
        detail: detail.filter(|text| !text.is_empty()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Every sequence quoted below is copied from the specification, with the
    /// literal `<ESC>` spellings turned into bytes.
    fn spec(text: &str) -> String {
        text.replace("<ESC>_", APC).replace("<ESC>\\", ST)
    }

    #[test]
    fn support_query_matches_the_sequence_in_the_spec() {
        assert_eq!(
            support_query(),
            spec("<ESC>_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA<ESC>\\")
        );
    }

    #[test]
    fn a_png_transmission_is_the_minimal_example_from_the_spec() {
        // <ESC>_Gf=100;<payload><ESC>\ — the whole of "PNG data".
        let sequences = transmit(&Transmit::png(), None, b"pretend png");
        assert_eq!(sequences.len(), 1);
        assert_eq!(sequences[0].control, "a=t,f=100");
        assert_eq!(
            sequences[0].wire(),
            spec("<ESC>_Ga=t,f=100;cHJldGVuZCBwbmc=<ESC>\\")
        );
    }

    #[test]
    fn transmit_and_display_matches_the_shell_example() {
        // The spec's send-png script sends `a=T,f=100,` on the first chunk.
        let command = Transmit::png().with_action(Action::TransmitAndDisplay);
        let sequences = transmit(&command, None, b"x");
        assert_eq!(sequences[0].control, "a=T,f=100");
    }

    #[test]
    fn rgb_pixels_carry_their_dimensions() {
        // <ESC>_Gf=24,s=10,v=20;<payload><ESC>\
        let command = Transmit::pixels(Format::Rgb, 10, 20);
        let sequences = transmit(&command, None, &[0; 600]);
        assert_eq!(sequences[0].control, "a=t,f=24,s=10,v=20");
        assert_eq!(
            command.expected_payload_len(),
            Some(600),
            "3 bytes per pixel for a 10x20 image, as the spec computes"
        );
    }

    #[test]
    fn rgba_is_the_default_format_and_needs_dimensions() {
        assert_eq!(Format::default(), Format::Rgba);
        assert!(Format::Rgba.needs_dimensions());
        assert!(Format::Rgb.needs_dimensions());
        assert!(
            !Format::Png.needs_dimensions(),
            "PNG carries its own size in IHDR"
        );
        assert_eq!(Format::parse(100), Some(Format::Png));
        assert_eq!(Format::parse(64), None);
    }

    #[test]
    fn shared_memory_transmission_names_the_object_in_the_payload() {
        // <ESC>_Gs=10,v=2,t=s,S=80,O=10;<encoded name><ESC>\
        let mut command = Transmit::pixels(Format::Rgba, 10, 2);
        command.medium = Medium::SharedMemory;
        command.data_size = Some(80);
        command.data_offset = Some(10);
        let sequences = transmit(&command, None, b"/kitty-shm");
        assert_eq!(sequences[0].control, "a=t,f=32,t=s,s=10,v=2,S=80,O=10");
        assert!(Medium::SharedMemory.is_indirect());
        assert!(!Medium::Direct.is_indirect());
    }

    #[test]
    fn chunking_splits_at_4096_and_flags_every_chunk_but_the_last() {
        // Three chunks means 8193..=12288 base64 bytes, so 6145..=9216 raw.
        let data = vec![0xABu8; 7000];
        let sequences = transmit(&Transmit::png(), None, &data);
        assert_eq!(sequences.len(), 3, "9336 base64 bytes over a 4096 limit");

        assert!(
            sequences[0].control.starts_with("a=t,f=100"),
            "the first chunk carries the full control data: {}",
            sequences[0].control
        );
        assert!(sequences[0].control.ends_with(",m=1"));
        assert_eq!(sequences[1].control, "m=1");
        assert_eq!(sequences[2].control, "m=0", "the last chunk closes with m=0");

        for chunk in &sequences[..sequences.len() - 1] {
            assert_eq!(chunk.payload.len(), CHUNK_LIMIT);
            assert_eq!(
                chunk.payload.len() % 4,
                0,
                "a chunk boundary must not split a base64 quantum"
            );
        }
        assert!(sequences[2].payload.len() <= CHUNK_LIMIT);
    }

    #[test]
    fn chunks_reassemble_into_the_original_payload() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let sequences = transmit(&Transmit::png(), None, &data);
        let rejoined: String = sequences.iter().map(|s| s.payload.clone()).collect();
        assert_eq!(BASE64_STANDARD.decode(rejoined).unwrap(), data);
    }

    #[test]
    fn a_single_chunk_omits_the_m_key_because_its_default_is_zero() {
        let sequences = transmit(&Transmit::png(), None, b"small");
        assert_eq!(sequences.len(), 1);
        assert!(!sequences[0].control.contains('m'));
    }

    #[test]
    fn frame_continuation_chunks_keep_a_equals_f() {
        // "When sending animation frame data, subsequent chunks must also
        // specify the a=f key."
        let command = Transmit::pixels(Format::Rgba, 64, 64)
            .with_action(Action::Frame)
            .with_id(7);
        let sequences = transmit(&command, None, &vec![0u8; 64 * 64 * 4]);
        assert!(sequences.len() > 1, "16KiB of pixels needs chunking");
        for continuation in &sequences[1..] {
            assert!(
                continuation.control.starts_with("a=f,"),
                "{}",
                continuation.control
            );
        }
    }

    #[test]
    fn quiet_is_repeated_on_continuation_chunks() {
        let command = Transmit::png().with_quiet(Quiet::Silent);
        let sequences = transmit(&command, None, &vec![0u8; 7000]);
        assert!(sequences[0].control.contains("q=2"));
        assert_eq!(sequences[1].control, "q=2,m=1");
    }

    #[test]
    fn chunk_base64_handles_the_edges() {
        assert!(chunk_base64("").is_empty());
        assert_eq!(chunk_base64("AAAA"), vec!["AAAA"]);
        let exact = "A".repeat(CHUNK_LIMIT);
        assert_eq!(chunk_base64(&exact).len(), 1);
        let one_over = "A".repeat(CHUNK_LIMIT + 4);
        assert_eq!(chunk_base64(&one_over).len(), 2);
        assert_eq!(chunk_base64(&one_over)[1].len(), 4);
    }

    #[test]
    fn transmit_and_display_merges_both_key_sets() {
        let command = Transmit::png()
            .with_action(Action::TransmitAndDisplay)
            .with_id(3);
        let placement = Placement::default()
            .with_area(Some(20), None)
            .with_z(Some(-1));
        let sequences = transmit(&command, Some(&placement), b"x");
        assert_eq!(sequences[0].control, "a=T,f=100,i=3,c=20,z=-1");
    }

    #[test]
    fn placement_of_a_previously_transmitted_image() {
        // <ESC>_Ga=p,i=10<ESC>\
        let sequence = place(&Placement::of_image(10));
        assert_eq!(sequence.wire(), spec("<ESC>_Ga=p,i=10<ESC>\\"));
    }

    #[test]
    fn virtual_placement_matches_the_unicode_placeholder_example() {
        // <ESC>_Ga=p,U=1,i=<image_id>,c=<columns>,r=<rows><ESC>\
        let placement = Placement::of_image(42).virtualized(2, 2);
        let sequence = place(&placement);
        assert_eq!(sequence.control, "a=p,i=42,c=2,r=2,U=1");
    }

    #[test]
    fn source_rectangles_crop_without_scaling() {
        let placement = Placement::of_image(1)
            .with_source(Some(Rect::new(10, 20, 100, 50)))
            .with_area(Some(8), Some(4));
        assert_eq!(place(&placement).control, "a=p,i=1,x=10,y=20,w=100,h=50,c=8,r=4");
    }

    #[test]
    fn relative_placements_name_their_parent() {
        // <ESC>_Ga=p,i=…,p=…,P=…,Q=…<ESC>\
        let mut placement = Placement::of_image(2);
        placement.placement_id = Some(5);
        placement.relative = Some(Relative {
            parent_image: 1,
            parent_placement: 9,
            horizontal: 3,
            vertical: -2,
        });
        assert_eq!(place(&placement).control, "a=p,i=2,p=5,P=1,Q=9,H=3,V=-2");
    }

    #[test]
    fn cursor_can_be_told_to_stay_put() {
        let mut placement = Placement::of_image(1);
        placement.cursor = CursorMovement::Stay;
        assert_eq!(place(&placement).control, "a=p,i=1,C=1");
    }

    #[test]
    fn deletions_match_every_example_in_the_spec() {
        assert_eq!(
            delete(&Deletion::soft(DeleteTarget::Visible)).wire(),
            spec("<ESC>_Ga=d,d=a<ESC>\\"),
            "the spec's bare `a=d` form, with the default d spelled out"
        );
        assert_eq!(
            delete(&Deletion::soft(DeleteTarget::Id {
                image: 10,
                placement: None
            }))
            .wire(),
            spec("<ESC>_Ga=d,d=i,i=10<ESC>\\")
        );
        assert_eq!(
            delete(&Deletion::soft(DeleteTarget::Id {
                image: 10,
                placement: Some(7)
            }))
            .wire(),
            spec("<ESC>_Ga=d,d=i,i=10,p=7<ESC>\\")
        );
        assert_eq!(
            delete(&Deletion::hard(DeleteTarget::ZIndex(-1))).wire(),
            spec("<ESC>_Ga=d,d=Z,z=-1<ESC>\\")
        );
        assert_eq!(
            delete(&Deletion::soft(DeleteTarget::Cell { col: 3, row: 4 })).wire(),
            spec("<ESC>_Ga=d,d=p,x=3,y=4<ESC>\\")
        );
    }

    #[test]
    fn freeing_data_uppercases_the_delete_target() {
        assert_eq!(Deletion::soft(DeleteTarget::Visible).key(), 'a');
        assert_eq!(Deletion::hard(DeleteTarget::Visible).key(), 'A');
        assert_eq!(
            Deletion::hard(DeleteTarget::IdRange { first: 1, last: 9 }).key(),
            'R'
        );
    }

    #[test]
    fn only_id_shaped_deletions_reach_virtual_placements() {
        assert!(
            DeleteTarget::Id {
                image: 1,
                placement: None
            }
            .reaches_virtual_placements()
        );
        assert!(
            DeleteTarget::Number {
                number: 1,
                placement: None
            }
            .reaches_virtual_placements()
        );
        assert!(DeleteTarget::IdRange { first: 1, last: 2 }.reaches_virtual_placements());
        for target in [
            DeleteTarget::Visible,
            DeleteTarget::AtCursor,
            DeleteTarget::Cell { col: 1, row: 1 },
            DeleteTarget::Column(1),
            DeleteTarget::Row(1),
            DeleteTarget::ZIndex(0),
        ] {
            assert!(
                !target.reaches_virtual_placements(),
                "{target:?} has no screen position to match"
            );
        }
    }

    #[test]
    fn animation_control_matches_the_spec_examples() {
        // <ESC>_Ga=a,i=3,c=7<ESC>\ — make the seventh frame current.
        let animation = Animation {
            image_id: 3,
            current_frame: Some(7),
            ..Animation::default()
        };
        assert_eq!(animate(&animation).wire(), spec("<ESC>_Ga=a,i=3,c=7<ESC>\\"));

        // <ESC>_Ga=a,i=7,r=3,z=48<ESC>\ — set the third frame's gap.
        assert_eq!(
            animate(&Animation::set_gap(7, 3, 48)).wire(),
            spec("<ESC>_Ga=a,i=7,r=3,z=48<ESC>\\")
        );

        assert_eq!(animate(&Animation::play(1)).control, "a=a,i=1,s=3");
        assert_eq!(animate(&Animation::stop(1)).control, "a=a,i=1,s=1");
    }

    #[test]
    fn frame_transmission_carries_the_rectangle_and_gap() {
        let mut command = Transmit::pixels(Format::Rgba, 100, 200)
            .with_action(Action::Frame)
            .with_id(1);
        command.frame = Some(FrameData {
            at: (10, 5),
            base_frame: Some(1),
            gap: Some(48),
            blend: Blend::Overwrite,
            background: Some(0xff00_00ff),
            ..FrameData::default()
        });
        let sequences = transmit(&command, None, &vec![0u8; 100 * 200 * 4]);
        assert!(
            sequences[0].control.starts_with(
                "a=f,f=32,s=100,v=200,i=1,x=10,y=5,c=1,z=48,X=1,Y=4278190335"
            ),
            "{}",
            sequences[0].control
        );
    }

    #[test]
    fn composition_matches_the_spec_example() {
        // <ESC>_Ga=c,i=1,r=7,c=9,w=23,h=27,X=4,Y=8,x=1,y=3<ESC>\
        let composition = Composition {
            image_id: 1,
            source_frame: 7,
            destination_frame: 9,
            size: Some((23, 27)),
            source_at: (4, 8),
            destination_at: (1, 3),
            ..Composition::default()
        };
        assert_eq!(
            compose(&composition).wire(),
            spec("<ESC>_Ga=c,i=1,r=7,c=9,w=23,h=27,X=4,Y=8,x=1,y=3<ESC>\\")
        );
    }

    #[test]
    fn parses_the_ok_reply() {
        // <ESC>_Gi=31;OK<ESC>\
        let reply = parse_reply("Gi=31;OK").unwrap();
        assert_eq!(
            reply,
            GraphicsReply {
                image_id: Some(31),
                image_number: None,
                placement_id: None,
                outcome: Outcome::Ok,
            }
        );
        assert!(reply.is_ok());
        assert!(reply.error().is_none());
    }

    #[test]
    fn parses_an_enoent_reply_with_detail() {
        // <ESC>_Gi=<id>;ENOENT:<some detailed error msg><ESC>\
        let reply = parse_reply("Gi=10;ENOENT:Image not found").unwrap();
        assert_eq!(reply.image_id, Some(10));
        assert_eq!(
            reply.outcome,
            Outcome::Error {
                code: ErrorCode::NotFound,
                detail: Some("Image not found".to_owned()),
            }
        );
        assert_eq!(reply.error().unwrap().as_str(), "ENOENT");
    }

    #[test]
    fn parses_a_placement_acknowledgement() {
        // <ESC>_Gi=<image id>,p=<placement id>;OK<ESC>\
        let reply = parse_reply("Gi=3,p=7;OK").unwrap();
        assert_eq!(reply.image_id, Some(3));
        assert_eq!(reply.placement_id, Some(7));
    }

    #[test]
    fn a_reply_to_an_image_number_carries_the_allocated_id() {
        // <ESC>_Gi=99,I=13;OK<ESC>\
        let reply = parse_reply("Gi=99,I=13;OK").unwrap();
        assert_eq!(reply.image_id, Some(99));
        assert_eq!(reply.image_number, Some(13));
    }

    #[test]
    fn every_documented_error_code_round_trips() {
        for code in [
            "EBADPNG", "EBADF", "ECYCLE", "EFBIG", "EILSEQ", "EINVAL", "ENODATA", "ENOENT",
            "ENOMEM", "ENOPARENT", "ENOSPC", "EPERM", "ETOODEEP",
        ] {
            let parsed = ErrorCode::parse(code);
            assert_eq!(parsed.as_str(), code);
            assert!(
                !matches!(parsed, ErrorCode::Other(_)),
                "{code} should be recognised"
            );
            assert!(!parsed.explain().is_empty());
        }
    }

    #[test]
    fn unknown_error_codes_are_preserved_rather_than_flattened() {
        let reply = parse_reply("Gi=1;ENEWTHING:from the future").unwrap();
        assert_eq!(
            reply.error(),
            Some(&ErrorCode::Other("ENEWTHING".to_owned()))
        );
    }

    #[test]
    fn an_error_without_detail_parses() {
        let reply = parse_reply("Gi=1;EINVAL").unwrap();
        assert_eq!(
            reply.outcome,
            Outcome::Error {
                code: ErrorCode::Invalid,
                detail: None,
            }
        );
    }

    #[test]
    fn ignores_apc_payloads_belonging_to_other_protocols() {
        assert!(parse_reply("25a1;s;fmt=glyf").is_none(), "glyph protocol");
        assert!(parse_reply("").is_none());
        assert!(parse_reply("G").is_none(), "no message separator");
    }

    #[test]
    fn sequences_never_print_their_own_escape_bytes() {
        let sequences = transmit(&Transmit::png(), None, &vec![0u8; 9000]);
        for sequence in &sequences {
            let shown = sequence.display(16);
            assert!(!shown.contains('\u{1b}'), "{shown}");
            assert!(shown.starts_with("APC G"));
        }
        // And the payload really is elided.
        let shown = sequences[0].display(16);
        assert!(shown.contains("more base64 bytes"), "{shown}");
        assert!(shown.len() < 200, "elided output should stay short: {shown}");
    }

    #[test]
    fn display_keeps_short_payloads_intact() {
        let sequences = transmit(&Transmit::png(), None, b"hi");
        let shown = sequences[0].display(64);
        assert!(shown.ends_with("aGk= ST"), "{shown}");
    }

    #[test]
    fn quiet_and_cursor_and_blend_use_the_documented_wire_values() {
        assert_eq!(Quiet::default(), Quiet::Verbose);
        assert_eq!(Quiet::Verbose.as_u8(), 0);
        assert_eq!(Quiet::SuppressOk.as_u8(), 1);
        assert_eq!(Quiet::Silent.as_u8(), 2);
        assert_eq!(CursorMovement::default(), CursorMovement::Advance);
        assert_eq!(CursorMovement::Stay.as_u8(), 1);
        assert_eq!(Blend::default(), Blend::AlphaBlend);
        assert_eq!(Blend::Overwrite.as_u8(), 1);
        assert_eq!(AnimationState::Stop.as_u8(), 1);
        assert_eq!(AnimationState::Loading.as_u8(), 2);
        assert_eq!(AnimationState::Run.as_u8(), 3);
    }

    #[test]
    fn actions_use_the_documented_characters() {
        assert_eq!(Action::default(), Action::Transmit);
        for (action, expected) in [
            (Action::Transmit, 't'),
            (Action::TransmitAndDisplay, 'T'),
            (Action::Query, 'q'),
            (Action::Put, 'p'),
            (Action::Delete, 'd'),
            (Action::Frame, 'f'),
            (Action::Animate, 'a'),
            (Action::Compose, 'c'),
        ] {
            assert_eq!(action.as_char(), expected);
            assert!(!action.describe().is_empty());
        }
    }

    #[test]
    fn z_below_backgrounds_is_int32_min_over_two() {
        assert_eq!(Z_BELOW_BACKGROUNDS, -1_073_741_824);
    }
}
