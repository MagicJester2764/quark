//! Wayland's wire format.
//!
//! `[object: u32][opcode: u16 | size: u16][arguments]`, every argument aligned
//! to four bytes. A string is a length, then that many bytes *including* a NUL,
//! then padding to four; an array is the same without the NUL. The size in the
//! header counts the header itself.
//!
//! Small enough to get right, and unforgiving enough that getting it wrong
//! looks like a bug somewhere else entirely — which is why it is tested against
//! bytes libwayland actually produced rather than against its own idea of them.

/// Bytes before the first argument.
pub const HEADER: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub object: u32,
    pub opcode: u16,
    /// Including the header.
    pub size: u16,
}

pub fn parse_header(b: &[u8]) -> Option<Header> {
    if b.len() < HEADER {
        return None;
    }
    let object = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let word = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let size = (word >> 16) as u16;
    // A size that does not cover its own header is malformed, and believing it
    // would advance a read cursor by less than nothing — which is a loop that
    // never ends rather than a message that never parses.
    if (size as usize) < HEADER {
        return None;
    }
    Some(Header { object, opcode: (word & 0xFFFF) as u16, size })
}

pub fn put_header(b: &mut [u8], h: Header) {
    b[..4].copy_from_slice(&h.object.to_le_bytes());
    b[4..8].copy_from_slice(&(((h.size as u32) << 16) | h.opcode as u32).to_le_bytes());
}

pub fn get_u32(b: &[u8], at: usize) -> Option<u32> {
    if at + 4 > b.len() {
        return None;
    }
    Some(u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]))
}

pub fn get_i32(b: &[u8], at: usize) -> Option<i32> {
    get_u32(b, at).map(|v| v as i32)
}

pub fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Round up to the next multiple of four.
pub fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// The string at `at`, and how many bytes it occupied including its padding.
///
/// The slice returned excludes the NUL the length counts.
pub fn get_str(b: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let len = get_u32(b, at)? as usize;
    if len == 0 {
        return Some((&b[..0], 4));
    }
    let start = at + 4;
    if start + len > b.len() {
        return None;
    }
    Some((&b[start..start + len - 1], 4 + pad4(len)))
}

/// Write a string, returning how many bytes it took.
pub fn put_str(b: &mut [u8], at: usize, s: &[u8]) -> usize {
    let len = s.len() + 1;
    put_u32(b, at, len as u32);
    b[at + 4..at + 4 + s.len()].copy_from_slice(s);
    for i in s.len()..pad4(len) {
        b[at + 4 + i] = 0;
    }
    4 + pad4(len)
}
