//! Little-endian integer helpers for reading and writing page layouts.

/// Read a little-endian u16 at `at`.
pub(crate) fn get_u16(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

/// Write `value` as a little-endian u16 at `at`.
pub(crate) fn put_u16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

/// Read a little-endian u32 at `at`.
pub(crate) fn get_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
}

/// Write `value` as a little-endian u32 at `at`.
pub(crate) fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
