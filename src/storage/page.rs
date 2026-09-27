//! The page: the unit of storage, caching and copy-on-write.
//!
//! Every page starts with the same 32-byte header:
//!
//! ```text
//!  0  u8   kind        (Free, Super, Leaf, Interior, Overflow, FreeList)
//!  1  u8   flags
//!  2  u16  nslots      cells on a B+tree page
//!  4  u16  cell_start  lowest byte of the cell area (cells grow down)
//!  6  u16  reserved
//!  8  u64  pno         the page's own number: catches misdirected writes
//! 16  u64  epoch       transaction epoch the page was written in (CoW)
//! 24  u32  tree        which tree owns it: catches stray pointers
//! 28  u32  crc         crc32 of the page with this field zeroed
//! ```
//!
//! The crc is computed when a page leaves memory and checked when it comes
//! back, so a `:memory:` database never pays for it.

use std::sync::Arc;

use crate::codec::Crc32;

pub const HEADER: usize = 32;
pub const MIN_PAGE: usize = 512;
/// Offsets inside a page are u16, so a page may not reach 64 KiB.
pub const MAX_PAGE: usize = 32 * 1024;
pub const DEFAULT_PAGE: usize = 16 * 1024;

pub const KIND_FREE: u8 = 0;
pub const KIND_SUPER: u8 = 1;
pub const KIND_LEAF: u8 = 2;
pub const KIND_INTERIOR: u8 = 3;
pub const KIND_OVERFLOW: u8 = 4;
pub const KIND_FREELIST: u8 = 5;

/// A page's bytes. Shared, immutable while shared: readers hold an `Arc`
/// clone, and a writer gets exclusive access with `Arc::get_mut` or copies.
pub type Page = Arc<[u8]>;

pub fn valid_page_size(n: usize) -> bool {
    (MIN_PAGE..=MAX_PAGE).contains(&n) && n.is_power_of_two()
}

#[inline]
pub fn get_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
#[inline]
pub fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn get_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
#[inline]
pub fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn get_u64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}
#[inline]
pub fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub fn kind(p: &[u8]) -> u8 {
    p[0]
}
#[inline]
pub fn nslots(p: &[u8]) -> usize {
    get_u16(p, 2) as usize
}
#[inline]
pub fn set_nslots(p: &mut [u8], n: usize) {
    put_u16(p, 2, n as u16)
}
/// Lowest byte of the cell area. Stored as 0 for "the page is empty", since
/// a 32 KiB page's size does not fit the field.
#[inline]
pub fn cell_start(p: &[u8]) -> usize {
    match get_u16(p, 4) {
        0 => p.len(),
        v => v as usize,
    }
}
#[inline]
pub fn set_cell_start(p: &mut [u8], v: usize) {
    put_u16(p, 4, if v >= p.len() { 0 } else { v as u16 })
}
#[inline]
pub fn pno(p: &[u8]) -> u64 {
    get_u64(p, 8)
}
#[inline]
pub fn epoch(p: &[u8]) -> u64 {
    get_u64(p, 16)
}
#[inline]
pub fn tree(p: &[u8]) -> u32 {
    get_u32(p, 24)
}

/// Write a fresh header.
pub fn init(p: &mut [u8], kind: u8, pno: u64, epoch: u64, tree: u32) {
    p[..HEADER].fill(0);
    p[0] = kind;
    set_cell_start(p, p.len());
    put_u64(p, 8, pno);
    put_u64(p, 16, epoch);
    put_u32(p, 24, tree);
}

/// Re-home a copied page: new number, new epoch.
pub fn rehome(p: &mut [u8], pno: u64, epoch: u64) {
    put_u64(p, 8, pno);
    put_u64(p, 16, epoch);
}

pub fn compute_crc(p: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(&p[..28]);
    c.update(&[0u8; 4]);
    c.update(&p[32..]);
    c.finish()
}

pub fn seal(p: &mut [u8]) {
    let c = compute_crc(p);
    put_u32(p, 28, c);
}

/// Check a page read from disk: checksum, and that it is the page asked for.
/// A never-written page (all zeros) is reported as `Ok(false)`.
pub fn check(p: &[u8], want_pno: u64) -> Result<bool, String> {
    if p.iter().all(|b| *b == 0) {
        return Ok(false);
    }
    if get_u32(p, 28) != compute_crc(p) {
        return Err(format!("page {want_pno}: checksum mismatch"));
    }
    if pno(p) != want_pno {
        return Err(format!(
            "page {want_pno}: holds page {} (misdirected write)",
            pno(p)
        ));
    }
    Ok(true)
}
