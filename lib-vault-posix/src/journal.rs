//! The write journal's on-disk format (D63).
//!
//! A journal is `.vault-journal-<name>` beside the object: a 32-byte header followed by the
//! object's complete new content. While writing, the header is all zeros; `close()` fills it
//! in, marking the journal complete:
//!
//! | bytes  | field                                   |
//! |--------|-----------------------------------------|
//! | 0..8   | magic `LVJRNL01`                        |
//! | 8..16  | content length, little-endian `u64`     |
//! | 16..20 | CRC-32 (IEEE) of the content, LE `u32`  |
//! | 20     | `1` = complete                          |
//! | 21..32 | reserved, zero                          |
//!
//! A complete journal (magic present, length and CRC match the content) holds the full new
//! object and can be copied over it by hand; an incomplete one can be deleted.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;

pub(crate) const HEADER_LEN: usize = 32;
const MAGIC: &[u8; 8] = b"LVJRNL01";

/// The all-zero header written when the journal is created.
pub(crate) fn placeholder() -> [u8; HEADER_LEN] {
    [0; HEADER_LEN]
}

/// Writes the final header, marking the journal complete.
pub(crate) fn complete(file: &File, len: u64, crc: u32) -> io::Result<()> {
    let mut header = [0u8; HEADER_LEN];
    header[0..8].copy_from_slice(MAGIC);
    header[8..16].copy_from_slice(&len.to_le_bytes());
    header[16..20].copy_from_slice(&crc.to_le_bytes());
    header[20] = 1;
    file.write_all_at(&header, 0)
}

/// Copies the journal's content into `object`.
pub(crate) fn copy_content(journal: &File, object: &mut File) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut offset = HEADER_LEN as u64;
    loop {
        let n = journal.read_at(&mut buf, offset)?;
        if n == 0 {
            return Ok(());
        }
        object.write_all(&buf[..n])?;
        offset += n as u64;
    }
}

/// CRC-32 (IEEE 802.3), computed bitwise: journals are checksummed once per write, so speed
/// isn't critical, and it avoids a dependency.
pub(crate) struct Crc32(u32);

impl Crc32 {
    pub(crate) fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }

    pub(crate) fn update(&mut self, data: &[u8]) {
        let mut crc = self.0;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
            }
        }
        self.0 = crc;
    }

    pub(crate) fn finish(&self) -> u32 {
        !self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        let mut crc = Crc32::new();
        crc.update(b"1234");
        crc.update(b"56789");
        assert_eq!(crc.finish(), 0xCBF4_3926);
    }
}
