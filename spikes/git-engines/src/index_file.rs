//! Just enough of git's index format (Documentation/gitformat-index.txt) to
//! tell its version and which extensions it carries, read independently of
//! both libraries under test.

use anyhow::Result;
use anyhow::bail;

pub struct Shape {
    pub version: u32,
    /// Extension signatures, in file order.
    pub extensions: Vec<String>,
}

/// `hash_len` is 20 for SHA-1 repositories, 32 for SHA-256 ones.
pub fn shape(data: &[u8], hash_len: usize) -> Result<Shape> {
    if data.len() < 12 + hash_len || &data[..4] != b"DIRC" {
        bail!("not an index");
    }
    let be32 = |at: usize| u32::from_be_bytes(data[at..at + 4].try_into().unwrap());
    let version = be32(4);
    let count = be32(8) as usize;
    let mut at = 12;
    let mut previous_path_len = 0usize;
    for _ in 0..count {
        let start = at;
        // Ten 32-bit stat fields, the object id, then 16 bits of flags.
        let flags_at = at + 40 + hash_len;
        let flags = u16::from_be_bytes(data[flags_at..flags_at + 2].try_into().unwrap());
        at = flags_at + 2;
        if version >= 3 && flags & 0x4000 != 0 {
            at += 2;
        }
        if version == 4 {
            // Path compression: how much of the previous path to drop, then
            // the rest of this one, NUL-terminated.
            let mut byte = data[at];
            at += 1;
            let mut strip = (byte & 0x7f) as usize;
            while byte & 0x80 != 0 {
                byte = data[at];
                at += 1;
                strip = ((strip + 1) << 7) | (byte & 0x7f) as usize;
            }
            let end = at + data[at..].iter().position(|&b| b == 0).unwrap();
            previous_path_len = previous_path_len - strip + (end - at);
            at = end + 1;
        } else {
            let end = at + data[at..].iter().position(|&b| b == 0).unwrap();
            // Entries are padded with NULs to a multiple of eight bytes.
            let len = end - start + 1;
            at = start + len.div_ceil(8) * 8;
        }
    }
    let mut extensions = Vec::new();
    while at + 8 <= data.len() - hash_len {
        extensions.push(String::from_utf8_lossy(&data[at..at + 4]).into_owned());
        at += 8 + be32(at + 4) as usize;
    }
    Ok(Shape {
        version,
        extensions,
    })
}
