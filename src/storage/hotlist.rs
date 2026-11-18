//! The hot set, remembered: the list of hot page numbers kept beside a
//! database (`<db>-hot`), so that the next open, or a database restored
//! from a replica's base, starts warm on what was used often.
//!
//! The list is **advice**. The database file never refers to it, a missing
//! or unreadable one is the same as none, and nothing in it is believed
//! without a check:
//!
//! * **Whose it is.** It names the database's generation and page size; a
//!   list of another database, or of an earlier database at the same path,
//!   is not read further.
//! * **When it was taken.** It names the epoch of the checkpoint it was
//!   taken at. A list from a checkpoint newer than the one the database
//!   opened from describes a state this file never reached (an older copy
//!   put back in its place) and is not used.
//! * **What each page still is.** Pages are copy-on-write: a page number,
//!   once freed and handed out again, carries the epoch of its new data. So
//!   a listed page is loaded only if it is a page of the database now (below
//!   the high-water mark, in no free list) and its own epoch is not after
//!   the list's, which means it holds exactly what it held when the list
//!   was taken. A list from an older checkpoint (the database was opened
//!   since without a hot set, or by a build that does not keep one) is
//!   worth what is left of it, page by page.
//!
//! Nothing here can make an answer wrong: a loaded page is a page of the
//! database, read through the cache like any other.
//!
//! ## Format
//!
//! Little-endian, as pages are:
//!
//! ```text
//! 0    8   magic "GLIDERHT"
//! 8    4   version (1)
//! 12   4   page size
//! 16   16  generation
//! 32   8   epoch of the checkpoint it was taken at
//! 40   8   n, the number of pages
//! 48   8n  page numbers, hottest first
//! 48+8n 4  CRC-32 of everything before it
//! ```
//!
//! Written whole to `<db>-hot.tmp` and renamed over `<db>-hot`, so a reader
//! sees the old list or the new one, never half of one. It is not synced:
//! after a power loss it may be missing or empty, which the checksum and
//! the length catch, and the database opens cold.

use std::path::{Path, PathBuf};

use super::page;

const MAGIC: &[u8; 8] = b"GLIDERHT";
const VERSION: u32 = 1;
const HEADER: usize = 48;

/// A hot set, as remembered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct List {
    pub page_size: u32,
    pub generation: [u8; 16],
    /// The epoch of the checkpoint the list was taken at: the superblock's.
    pub epoch: u64,
    /// Hottest first.
    pub pages: Vec<u64>,
}

/// Where a database's list lives: `<db>-hot`.
pub fn path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_os_string();
    p.push("-hot");
    PathBuf::from(p)
}

impl List {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; HEADER + 8 * self.pages.len() + 4];
        b[0..8].copy_from_slice(MAGIC);
        page::put_u32(&mut b, 8, VERSION);
        page::put_u32(&mut b, 12, self.page_size);
        b[16..32].copy_from_slice(&self.generation);
        page::put_u64(&mut b, 32, self.epoch);
        page::put_u64(&mut b, 40, self.pages.len() as u64);
        for (i, p) in self.pages.iter().enumerate() {
            page::put_u64(&mut b, HEADER + 8 * i, *p);
        }
        let end = b.len() - 4;
        let crc = crate::codec::crc32(&b[..end]);
        page::put_u32(&mut b, end, crc);
        b
    }

    /// A list, if `b` is one whole and undamaged.
    pub fn decode(b: &[u8]) -> Option<List> {
        if b.len() < HEADER + 4 || &b[0..8] != MAGIC || page::get_u32(b, 8) != VERSION {
            return None;
        }
        let n = page::get_u64(b, 40);
        if n > b.len() as u64 || (n as usize) * 8 + HEADER + 4 != b.len() {
            return None;
        }
        let end = b.len() - 4;
        if crate::codec::crc32(&b[..end]) != page::get_u32(b, end) {
            return None;
        }
        let mut generation = [0u8; 16];
        generation.copy_from_slice(&b[16..32]);
        Some(List {
            page_size: page::get_u32(b, 12),
            generation,
            epoch: page::get_u64(b, 32),
            pages: (0..n as usize)
                .map(|i| page::get_u64(b, HEADER + 8 * i))
                .collect(),
        })
    }
}

/// Read the list at `file`, if there is a whole one naming at most
/// `max_pages` pages (a database's high-water mark bounds any list of its
/// own; a longer file is not read).
pub fn read(file: &Path, max_pages: u64) -> Option<List> {
    let len = std::fs::metadata(file).ok()?.len();
    if len > (HEADER as u64 + 4).saturating_add(max_pages.saturating_mul(8)) {
        return None;
    }
    List::decode(&std::fs::read(file).ok()?)
}

/// Write `list` to `file`, replacing what was there in one step.
pub fn write(file: &Path, list: &List) -> std::io::Result<()> {
    let mut tmp = file.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, list.encode())?;
    std::fs::rename(&tmp, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_round_trips_and_any_damage_is_no_list() {
        let l = List {
            page_size: 1024,
            generation: [7; 16],
            epoch: 42,
            pages: vec![9, 3, 1 << 40, 17],
        };
        let b = l.encode();
        assert_eq!(b.len(), HEADER + 8 * 4 + 4);
        assert_eq!(List::decode(&b), Some(l.clone()));
        for cut in 0..b.len() {
            assert_eq!(List::decode(&b[..cut]), None, "cut at {cut}");
        }
        for at in 0..b.len() {
            let mut d = b.clone();
            d[at] ^= 0x10;
            assert_eq!(List::decode(&d), None, "byte {at} flipped");
        }
        let mut longer = b.clone();
        longer.extend_from_slice(&[0; 8]);
        assert_eq!(List::decode(&longer), None);
        let empty = List {
            pages: Vec::new(),
            ..l
        };
        assert_eq!(List::decode(&empty.encode()), Some(empty));
    }
}
