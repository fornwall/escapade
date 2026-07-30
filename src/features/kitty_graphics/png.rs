//! Just enough PNG to know how big it is.
//!
//! `f=100` sends a PNG file to the terminal verbatim, and the terminal reads the
//! dimensions out of the file itself. So the only reason escapade needs to look
//! inside a PNG is to decide how many cells the image should occupy — and that
//! answer is 8 bytes at a fixed offset.
//!
//! A PNG is an 8-byte signature followed by chunks, and the specification
//! requires IHDR to be the *first* chunk. IHDR's payload starts with a 4-byte
//! big-endian width and a 4-byte big-endian height:
//!
//! ```text
//! offset  0   8 bytes   signature: 89 50 4E 47 0D 0A 1A 0A
//! offset  8   4 bytes   chunk length (13 for IHDR)
//! offset 12   4 bytes   chunk type: "IHDR"
//! offset 16   4 bytes   width
//! offset 20   4 bytes   height
//! ```
//!
//! Which is why there is no PNG decoder in this crate's dependency tree: pulling
//! one in to learn two integers, and then re-encoding what we already had, would
//! be slower and lossier than sending the original bytes.
//!
//! Reference: <https://www.w3.org/TR/png-3/#11IHDR>

use anyhow::{Result, bail};

/// The 8-byte PNG signature. The high bit in the first byte catches transfers
/// that stripped it, and the CR-LF pair catches newline translation.
pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Offset of IHDR's width field.
const WIDTH_OFFSET: usize = 16;

/// Smallest possible PNG prefix that still contains the dimensions.
const MINIMUM_LEN: usize = WIDTH_OFFSET + 8;

/// Whether `bytes` starts with the PNG signature.
#[must_use]
pub fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&SIGNATURE)
}

/// Reads `(width, height)` from a PNG's IHDR chunk.
///
/// # Errors
///
/// Fails if the signature is missing, if the file is truncated before IHDR, if
/// the first chunk is not IHDR, or if either dimension is zero — all of which
/// mean the terminal would reject the file too.
pub fn dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    if !is_png(bytes) {
        bail!("not a PNG: the file does not start with the PNG signature");
    }
    if bytes.len() < MINIMUM_LEN {
        bail!(
            "truncated PNG: {} bytes, but the IHDR chunk ends at byte {MINIMUM_LEN}",
            bytes.len()
        );
    }
    if &bytes[12..16] != b"IHDR" {
        bail!("malformed PNG: the first chunk is not IHDR");
    }

    let width = be_u32(&bytes[WIDTH_OFFSET..WIDTH_OFFSET + 4]);
    let height = be_u32(&bytes[WIDTH_OFFSET + 4..WIDTH_OFFSET + 8]);
    if width == 0 || height == 0 {
        bail!("malformed PNG: IHDR declares a {width}x{height} image");
    }
    Ok((width, height))
}

/// Reads a big-endian `u32` from exactly four bytes.
fn be_u32(bytes: &[u8]) -> u32 {
    let mut value = [0u8; 4];
    // The slice length is guaranteed by every caller; a short slice would leave
    // the leading bytes zero rather than panicking.
    let len = bytes.len().min(4);
    value[4 - len..].copy_from_slice(&bytes[..len]);
    u32::from_be_bytes(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Builds the first 24 bytes of a PNG declaring the given dimensions.
    fn header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = SIGNATURE.to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn reads_the_dimensions_out_of_ihdr() {
        assert_eq!(dimensions(&header(1920, 1080)).unwrap(), (1920, 1080));
        assert_eq!(dimensions(&header(1, 1)).unwrap(), (1, 1));
        // A dimension that needs all four bytes, to prove the endianness.
        assert_eq!(dimensions(&header(0x0001_0203, 7)).unwrap(), (66051, 7));
    }

    #[test]
    fn trailing_chunks_are_irrelevant() {
        let mut png = header(16, 32);
        png.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(b"\x00\x00\x00\x00IDATwhatever");
        assert_eq!(dimensions(&png).unwrap(), (16, 32));
    }

    #[test]
    fn rejects_anything_that_is_not_a_png() {
        let error = dimensions(b"\xff\xd8\xff\xe0JFIF and so on").unwrap_err();
        assert!(error.to_string().contains("not a PNG"), "{error}");
        assert!(!is_png(b"PNG"));
        assert!(is_png(&header(1, 1)));
    }

    #[test]
    fn rejects_a_truncated_header() {
        let png = header(4, 4);
        let error = dimensions(&png[..20]).unwrap_err();
        assert!(error.to_string().contains("truncated"), "{error}");
    }

    #[test]
    fn rejects_a_first_chunk_that_is_not_ihdr() {
        let mut png = SIGNATURE.to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"iTXt");
        png.extend_from_slice(&[0; 8]);
        let error = dimensions(&png).unwrap_err();
        assert!(error.to_string().contains("not IHDR"), "{error}");
    }

    #[test]
    fn rejects_zero_dimensions() {
        for (width, height) in [(0, 10), (10, 0), (0, 0)] {
            assert!(dimensions(&header(width, height)).is_err());
        }
    }
}
