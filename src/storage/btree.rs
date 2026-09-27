//! Copy-on-write B+trees over byte keys.
//!
//! Keys compare as plain bytes (see [`super::keys`] for encodings that make
//! that the right order). Values are bytes; one too large to sit in a leaf
//! goes to a chain of overflow pages.
//!
//! Every write goes through [`Pager::writable`], so a transaction copies the
//! path from the root to the leaf the first time it touches it and modifies
//! its own copies after that. The committed tree is never touched, which is
//! what makes rollback free. A tree is identified by its root slot in the
//! pager; writes update the slot, and rollback restores it.
//!
//! Page layout (after the 32-byte header):
//!
//! ```text
//! leaf:      slot array (u16 offsets, in key order) ... cells
//!            cell = flags u8 | klen varint | vlen varint | key | value
//!            flags & 1: value is (total_len u64, first overflow page u64)
//! interior:  leftmost child u64 | slot array ... cells
//!            cell = klen varint | key | child u64   (child holds keys >= key)
//! overflow:  next u64 | len u32 | pad u32 | payload
//! ```
//!
//! Deletes remove a page once it is empty rather than rebalancing; the tree
//! stays correct and COMPACT rebuilds it densely.

use std::borrow::Cow;

use super::page::{self, Page, HEADER, KIND_INTERIOR, KIND_LEAF, KIND_OVERFLOW};
use super::pager::Pager;
use super::{corrupt, SResult};

/// A tree: the pager root slot holding its root page, and its id (stamped in
/// every page it owns). Root 0 means empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tree {
    pub slot: usize,
    pub id: u32,
}

const LEAF_BASE: usize = HEADER;
const INT_BASE: usize = HEADER + 8;
const F_OVERFLOW: u8 = 1;
const OVF_BASE: usize = HEADER + 16;

/// Largest key a tree accepts.
pub fn max_key(ps: usize) -> usize {
    ps / 8
}

/// Largest cell kept inline; bigger values overflow.
fn max_cell(ps: usize) -> usize {
    (ps - INT_BASE) / 4 - 2
}

fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(b: &[u8], at: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = b[*at];
        *at += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

// ------------------------------------------------------------ page access

fn base(p: &[u8]) -> usize {
    if page::kind(p) == KIND_INTERIOR {
        INT_BASE
    } else {
        LEAF_BASE
    }
}

#[inline]
fn cell_off(p: &[u8], i: usize) -> usize {
    page::get_u16(p, base(p) + 2 * i) as usize
}

/// A leaf cell, parsed.
struct LeafCell<'a> {
    key: &'a [u8],
    overflow: bool,
    value: &'a [u8],
    /// Bytes the cell occupies.
    size: usize,
}

fn leaf_cell(p: &[u8], i: usize) -> LeafCell<'_> {
    let off = cell_off(p, i);
    let mut at = off + 1;
    let flags = p[off];
    let klen = get_varint(p, &mut at) as usize;
    let vlen = get_varint(p, &mut at) as usize;
    let key = &p[at..at + klen];
    let value = &p[at + klen..at + klen + vlen];
    LeafCell {
        key,
        overflow: flags & F_OVERFLOW != 0,
        value,
        size: at + klen + vlen - off,
    }
}

fn int_cell(p: &[u8], i: usize) -> (&[u8], u64, usize) {
    let off = cell_off(p, i);
    let mut at = off;
    let klen = get_varint(p, &mut at) as usize;
    let key = &p[at..at + klen];
    let child = page::get_u64(p, at + klen);
    (key, child, at + klen + 8 - off)
}

fn key_at(p: &[u8], i: usize) -> &[u8] {
    if page::kind(p) == KIND_INTERIOR {
        int_cell(p, i).0
    } else {
        leaf_cell(p, i).key
    }
}

/// Binary search: Ok(i) if key i equals `key`, else Err(insertion point).
fn search(p: &[u8], key: &[u8]) -> Result<usize, usize> {
    let (mut lo, mut hi) = (0usize, page::nslots(p));
    while lo < hi {
        let mid = (lo + hi) / 2;
        match key_at(p, mid).cmp(key) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok(mid),
        }
    }
    Err(lo)
}

/// Child position in an interior page for `key`: 0 is the leftmost child,
/// c > 0 is the child of cell c-1.
fn child_pos(p: &[u8], key: &[u8]) -> usize {
    match search(p, key) {
        Ok(i) => i + 1,
        Err(i) => i,
    }
}

fn child_at(p: &[u8], c: usize) -> u64 {
    if c == 0 {
        page::get_u64(p, HEADER)
    } else {
        int_cell(p, c - 1).1
    }
}

fn free_space(p: &[u8]) -> usize {
    page::cell_start(p) - (base(p) + 2 * page::nslots(p))
}

/// Raw cell bytes of every slot, in order.
fn cells(p: &[u8]) -> Vec<Vec<u8>> {
    let leaf = page::kind(p) == KIND_LEAF;
    (0..page::nslots(p))
        .map(|i| {
            let off = cell_off(p, i);
            let size = if leaf {
                leaf_cell(p, i).size
            } else {
                int_cell(p, i).2
            };
            p[off..off + size].to_vec()
        })
        .collect()
}

/// Rewrite a page's slot array and cells from scratch.
fn write_cells(p: &mut [u8], cells: &[Vec<u8>]) {
    let b = base(p);
    let mut end = p.len();
    for (i, c) in cells.iter().enumerate() {
        end -= c.len();
        p[end..end + c.len()].copy_from_slice(c);
        page::put_u16(p, b + 2 * i, end as u16);
    }
    page::set_nslots(p, cells.len());
    page::set_cell_start(p, end);
}

fn cells_fit(ps: usize, base: usize, cells: &[Vec<u8>]) -> bool {
    base + cells.iter().map(|c| c.len() + 2).sum::<usize>() <= ps
}

/// Insert a cell at slot `pos` if it fits (compacting first if that helps).
fn try_insert(p: &mut [u8], pos: usize, cell: &[u8]) -> bool {
    let n = page::nslots(p);
    if free_space(p) < cell.len() + 2 {
        let mut cs = cells(p);
        if !cells_fit(p.len(), base(p), &cs) || base(p) + cs.iter().map(|c| c.len() + 2).sum::<usize>() + cell.len() + 2 > p.len() {
            return false;
        }
        cs.insert(pos, cell.to_vec());
        write_cells(p, &cs);
        return true;
    }
    let b = base(p);
    let start = page::cell_start(p) - cell.len();
    p[start..start + cell.len()].copy_from_slice(cell);
    // Shift slots right of pos.
    p.copy_within(b + 2 * pos..b + 2 * n, b + 2 * pos + 2);
    page::put_u16(p, b + 2 * pos, start as u16);
    page::set_nslots(p, n + 1);
    page::set_cell_start(p, start);
    true
}

fn remove_slot(p: &mut [u8], pos: usize) {
    let n = page::nslots(p);
    let b = base(p);
    p.copy_within(b + 2 * pos + 2..b + 2 * n, b + 2 * pos);
    page::set_nslots(p, n - 1);
    // The cell's bytes become a hole, reclaimed by the next compaction.
}

fn leaf_cell_bytes(key: &[u8], value: &[u8], overflow: bool) -> Vec<u8> {
    let mut c = Vec::with_capacity(key.len() + value.len() + 6);
    c.push(if overflow { F_OVERFLOW } else { 0 });
    put_varint(&mut c, key.len() as u64);
    put_varint(&mut c, value.len() as u64);
    c.extend_from_slice(key);
    c.extend_from_slice(value);
    c
}

fn int_cell_bytes(key: &[u8], child: u64) -> Vec<u8> {
    let mut c = Vec::with_capacity(key.len() + 10);
    put_varint(&mut c, key.len() as u64);
    c.extend_from_slice(key);
    c.extend_from_slice(&child.to_le_bytes());
    c
}

fn int_cell_parts(c: &[u8]) -> (Vec<u8>, u64) {
    let mut at = 0;
    let klen = get_varint(c, &mut at) as usize;
    (c[at..at + klen].to_vec(), page::get_u64(c, at + klen))
}

fn leaf_cell_key(c: &[u8]) -> Vec<u8> {
    let mut at = 1;
    let klen = get_varint(c, &mut at) as usize;
    let _ = get_varint(c, &mut at);
    c[at..at + klen].to_vec()
}

// ------------------------------------------------------------- overflow

fn write_overflow(p: &Pager, tree: u32, data: &[u8]) -> SResult<u64> {
    let ps = p.page_size();
    let per = ps - OVF_BASE;
    let chunks: Vec<&[u8]> = data.chunks(per).collect();
    // Written back to front so each page knows its successor.
    let mut next = 0u64;
    for chunk in chunks.iter().rev() {
        let pno = p.alloc(KIND_OVERFLOW, tree)?;
        p.modify(pno, |b| {
            page::put_u64(b, HEADER, next);
            page::put_u32(b, HEADER + 8, chunk.len() as u32);
            b[OVF_BASE..OVF_BASE + chunk.len()].copy_from_slice(chunk);
        })?;
        next = pno;
    }
    Ok(next)
}

fn read_overflow(p: &Pager, mut pno: u64, len: usize) -> SResult<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    while pno != 0 && out.len() < len {
        let pg = p.get(pno)?;
        if page::kind(&pg) != KIND_OVERFLOW {
            return corrupt(format!("page {pno} is not an overflow page"));
        }
        let n = page::get_u32(&pg, HEADER + 8) as usize;
        if OVF_BASE + n > pg.len() {
            return corrupt(format!("overflow page {pno} overruns"));
        }
        out.extend_from_slice(&pg[OVF_BASE..OVF_BASE + n]);
        pno = page::get_u64(&pg, HEADER);
    }
    if out.len() != len {
        return corrupt("overflow chain is short");
    }
    Ok(out)
}

fn free_overflow(p: &Pager, mut pno: u64) -> SResult<()> {
    while pno != 0 {
        let pg = p.get(pno)?;
        let next = page::get_u64(&pg, HEADER);
        p.free(pno)?;
        pno = next;
    }
    Ok(())
}

fn overflow_ref(v: &[u8]) -> (usize, u64) {
    (page::get_u64(v, 0) as usize, page::get_u64(v, 8))
}

/// Build the leaf cell for (key, value), spilling a large value.
fn make_cell(p: &Pager, tree: u32, key: &[u8], value: &[u8]) -> SResult<Vec<u8>> {
    let ps = p.page_size();
    if key.len() > max_key(ps) {
        return corrupt(format!(
            "key of {} bytes exceeds the {}-byte limit",
            key.len(),
            max_key(ps)
        ));
    }
    let inline = 1 + varint_len(key.len() as u64) + varint_len(value.len() as u64) + key.len() + value.len();
    if inline <= max_cell(ps) {
        return Ok(leaf_cell_bytes(key, value, false));
    }
    let first = write_overflow(p, tree, value)?;
    let mut r = Vec::with_capacity(16);
    r.extend_from_slice(&(value.len() as u64).to_le_bytes());
    r.extend_from_slice(&first.to_le_bytes());
    Ok(leaf_cell_bytes(key, &r, true))
}

// ------------------------------------------------------------------ reads

impl Tree {
    pub fn root(&self, p: &Pager) -> u64 {
        p.root(self.slot)
    }

    /// The value stored under `key`.
    pub fn get(&self, p: &Pager, key: &[u8]) -> SResult<Option<Vec<u8>>> {
        let mut pno = self.root(p);
        if pno == 0 {
            return Ok(None);
        }
        loop {
            let pg = p.get(pno)?;
            match page::kind(&pg) {
                KIND_INTERIOR => pno = child_at(&pg, child_pos(&pg, key)),
                KIND_LEAF => {
                    return match search(&pg, key) {
                        Ok(i) => {
                            let c = leaf_cell(&pg, i);
                            if c.overflow {
                                let (len, first) = overflow_ref(c.value);
                                Ok(Some(read_overflow(p, first, len)?))
                            } else {
                                Ok(Some(c.value.to_vec()))
                            }
                        }
                        Err(_) => Ok(None),
                    };
                }
                k => return corrupt(format!("page {pno}: unexpected kind {k} in tree {}", self.id)),
            }
        }
    }

    pub fn contains(&self, p: &Pager, key: &[u8]) -> SResult<bool> {
        Ok(self.get(p, key)?.is_some())
    }

    /// A cursor at the first key >= `from`.
    pub fn seek<'p>(&self, p: &'p Pager, from: &[u8]) -> SResult<Cursor<'p>> {
        Cursor::seek(p, self.root(p), from)
    }

    /// A cursor over the whole tree.
    pub fn scan<'p>(&self, p: &'p Pager) -> SResult<Cursor<'p>> {
        Cursor::seek(p, self.root(p), &[])
    }

    // -------------------------------------------------------------- writes

    /// Insert or replace.
    pub fn put(&self, p: &Pager, key: &[u8], value: &[u8]) -> SResult<()> {
        let cell = make_cell(p, self.id, key, value)?;
        let root = self.root(p);
        if root == 0 {
            let leaf = p.alloc(KIND_LEAF, self.id)?;
            p.modify(leaf, |b| {
                try_insert(b, 0, &cell);
            })?;
            p.set_root(self.slot, leaf);
            return Ok(());
        }
        match self.insert_rec(p, root, key, &cell)? {
            Up::Done(r) => {
                if r != root {
                    p.set_root(self.slot, r);
                }
            }
            Up::Split(l, sep, r) => {
                let nr = p.alloc(KIND_INTERIOR, self.id)?;
                p.modify(nr, |b| {
                    page::put_u64(b, HEADER, l);
                    try_insert(b, 0, &int_cell_bytes(&sep, r));
                })?;
                p.set_root(self.slot, nr);
            }
        }
        Ok(())
    }

    fn insert_rec(&self, p: &Pager, pno: u64, key: &[u8], cell: &[u8]) -> SResult<Up> {
        let pg = p.get(pno)?;
        match page::kind(&pg) {
            KIND_LEAF => {
                let found = search(&pg, key);
                if let Ok(i) = found {
                    let c = leaf_cell(&pg, i);
                    if c.overflow {
                        free_overflow(p, overflow_ref(c.value).1)?;
                    }
                }
                drop(pg);
                let w = p.writable(pno)?;
                let ps = p.page_size();
                let split = p.modify(w, |b| -> Option<(Vec<Vec<u8>>, usize)> {
                    let pos = match found {
                        Ok(i) => {
                            remove_slot(b, i);
                            i
                        }
                        Err(i) => i,
                    };
                    if try_insert(b, pos, cell) {
                        return None;
                    }
                    let mut cs = cells(b);
                    cs.insert(pos, cell.to_vec());
                    Some((cs, pos))
                })?;
                let Some((cs, pos)) = split else {
                    return Ok(Up::Done(w));
                };
                // Appending at the right edge keeps the left page full:
                // monotonic keys (ids) then pack pages completely.
                let mid = if pos == cs.len() - 1 {
                    cs.len() - 1
                } else {
                    split_point(&cs, ps)
                };
                let (left, right) = cs.split_at(mid);
                let sep = leaf_cell_key(&right[0]);
                let r = p.alloc(KIND_LEAF, self.id)?;
                p.modify(w, |b| write_cells(b, left))?;
                p.modify(r, |b| write_cells(b, right))?;
                Ok(Up::Split(w, sep, r))
            }
            KIND_INTERIOR => {
                let c = child_pos(&pg, key);
                let child = child_at(&pg, c);
                drop(pg);
                match self.insert_rec(p, child, key, cell)? {
                    Up::Done(nc) if nc == child => Ok(Up::Done(pno)),
                    Up::Done(nc) => {
                        let w = p.writable(pno)?;
                        p.modify(w, |b| set_child(b, c, nc))?;
                        Ok(Up::Done(w))
                    }
                    Up::Split(l, sep, r) => {
                        let w = p.writable(pno)?;
                        let ps = p.page_size();
                        let icell = int_cell_bytes(&sep, r);
                        let split = p.modify(w, |b| {
                            set_child(b, c, l);
                            if try_insert(b, c, &icell) {
                                return None;
                            }
                            let mut cs = cells(b);
                            cs.insert(c, icell.clone());
                            Some((cs, page::get_u64(b, HEADER)))
                        })?;
                        let Some((cs, leftmost)) = split else {
                            return Ok(Up::Done(w));
                        };
                        let mid = if c == cs.len() - 1 {
                            cs.len() - 1
                        } else {
                            split_point(&cs, ps)
                        };
                        let (up_key, up_child) = int_cell_parts(&cs[mid]);
                        let left = &cs[..mid];
                        let right = &cs[mid + 1..];
                        let r = p.alloc(KIND_INTERIOR, self.id)?;
                        p.modify(w, |b| {
                            page::put_u64(b, HEADER, leftmost);
                            write_cells(b, left);
                        })?;
                        p.modify(r, |b| {
                            page::put_u64(b, HEADER, up_child);
                            write_cells(b, right);
                        })?;
                        Ok(Up::Split(w, up_key, r))
                    }
                }
            }
            k => corrupt(format!("page {pno}: unexpected kind {k} in tree {}", self.id)),
        }
    }

    /// Remove `key`. Returns whether it was there.
    pub fn delete(&self, p: &Pager, key: &[u8]) -> SResult<bool> {
        let root = self.root(p);
        if root == 0 {
            return Ok(false);
        }
        match self.delete_rec(p, root, key)? {
            Del::NotFound => Ok(false),
            Del::Empty => {
                p.set_root(self.slot, 0);
                Ok(true)
            }
            Del::Done(mut r) => {
                // Collapse a root with a single child.
                loop {
                    let pg = p.get(r)?;
                    if page::kind(&pg) == KIND_INTERIOR && page::nslots(&pg) == 0 {
                        let only = page::get_u64(&pg, HEADER);
                        p.free(r)?;
                        r = only;
                    } else {
                        break;
                    }
                }
                p.set_root(self.slot, r);
                Ok(true)
            }
        }
    }

    fn delete_rec(&self, p: &Pager, pno: u64, key: &[u8]) -> SResult<Del> {
        let pg = p.get(pno)?;
        match page::kind(&pg) {
            KIND_LEAF => {
                let Ok(i) = search(&pg, key) else {
                    return Ok(Del::NotFound);
                };
                let c = leaf_cell(&pg, i);
                if c.overflow {
                    free_overflow(p, overflow_ref(c.value).1)?;
                }
                let last = page::nslots(&pg) == 1;
                drop(pg);
                if last {
                    p.free(pno)?;
                    return Ok(Del::Empty);
                }
                let w = p.writable(pno)?;
                p.modify(w, |b| remove_slot(b, i))?;
                Ok(Del::Done(w))
            }
            KIND_INTERIOR => {
                let c = child_pos(&pg, key);
                let child = child_at(&pg, c);
                let n = page::nslots(&pg);
                drop(pg);
                match self.delete_rec(p, child, key)? {
                    Del::NotFound => Ok(Del::NotFound),
                    Del::Done(nc) if nc == child => Ok(Del::Done(pno)),
                    Del::Done(nc) => {
                        let w = p.writable(pno)?;
                        p.modify(w, |b| set_child(b, c, nc))?;
                        Ok(Del::Done(w))
                    }
                    Del::Empty => {
                        if n == 0 {
                            p.free(pno)?;
                            return Ok(Del::Empty);
                        }
                        let w = p.writable(pno)?;
                        p.modify(w, |b| {
                            if c == 0 {
                                let first = int_cell(b, 0).1;
                                page::put_u64(b, HEADER, first);
                                remove_slot(b, 0);
                            } else {
                                remove_slot(b, c - 1);
                            }
                        })?;
                        Ok(Del::Done(w))
                    }
                }
            }
            k => corrupt(format!("page {pno}: unexpected kind {k} in tree {}", self.id)),
        }
    }

    /// Free every page of the tree and empty it.
    pub fn clear(&self, p: &Pager) -> SResult<()> {
        let root = self.root(p);
        if root != 0 {
            free_subtree(p, root)?;
            p.set_root(self.slot, 0);
        }
        Ok(())
    }

    /// Entries, pages and height, walking the whole tree. For tests, verify
    /// and stats.
    pub fn check(&self, p: &Pager) -> SResult<TreeStats> {
        let mut st = TreeStats::default();
        let root = self.root(p);
        if root != 0 {
            check_rec(p, self.id, root, None, None, 1, &mut st)?;
        }
        Ok(st)
    }
}

fn set_child(b: &mut [u8], c: usize, child: u64) {
    if c == 0 {
        page::put_u64(b, HEADER, child);
    } else {
        let off = cell_off(b, c - 1);
        let mut at = off;
        let klen = get_varint(b, &mut at) as usize;
        page::put_u64(b, at + klen, child);
    }
}

fn split_point(cs: &[Vec<u8>], _ps: usize) -> usize {
    let total: usize = cs.iter().map(|c| c.len() + 2).sum();
    let mut acc = 0;
    for (i, c) in cs.iter().enumerate() {
        acc += c.len() + 2;
        if acc * 2 >= total {
            return (i + 1).clamp(1, cs.len() - 1);
        }
    }
    cs.len() / 2
}

enum Up {
    Done(u64),
    Split(u64, Vec<u8>, u64),
}

enum Del {
    NotFound,
    Done(u64),
    Empty,
}

fn free_subtree(p: &Pager, pno: u64) -> SResult<()> {
    let pg = p.get(pno)?;
    match page::kind(&pg) {
        KIND_INTERIOR => {
            for c in 0..=page::nslots(&pg) {
                free_subtree(p, child_at(&pg, c))?;
            }
        }
        KIND_LEAF => {
            for i in 0..page::nslots(&pg) {
                let c = leaf_cell(&pg, i);
                if c.overflow {
                    free_overflow(p, overflow_ref(c.value).1)?;
                }
            }
        }
        _ => {}
    }
    p.free(pno)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TreeStats {
    pub entries: u64,
    pub leaves: u64,
    pub interiors: u64,
    pub overflow_pages: u64,
    pub height: u32,
}

fn check_rec(
    p: &Pager,
    id: u32,
    pno: u64,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    depth: u32,
    st: &mut TreeStats,
) -> SResult<()> {
    let pg = p.get(pno)?;
    if page::tree(&pg) != id {
        return corrupt(format!("page {pno} belongs to tree {}, not {id}", page::tree(&pg)));
    }
    let n = page::nslots(&pg);
    for i in 0..n {
        let k = key_at(&pg, i);
        if i > 0 && key_at(&pg, i - 1) >= k {
            return corrupt(format!("page {pno}: keys out of order"));
        }
        if lo.map(|l| k < l).unwrap_or(false) || hi.map(|h| k >= h).unwrap_or(false) {
            return corrupt(format!("page {pno}: key outside its parent's range"));
        }
    }
    match page::kind(&pg) {
        KIND_LEAF => {
            st.leaves += 1;
            st.entries += n as u64;
            st.height = st.height.max(depth);
            for i in 0..n {
                let c = leaf_cell(&pg, i);
                if c.overflow {
                    let (len, first) = overflow_ref(c.value);
                    read_overflow(p, first, len)?;
                    st.overflow_pages += len.div_ceil(p.page_size() - OVF_BASE) as u64;
                }
            }
        }
        KIND_INTERIOR => {
            st.interiors += 1;
            for c in 0..=n {
                let clo = if c == 0 { lo } else { Some(key_at(&pg, c - 1)) };
                let chi = if c == n { hi } else { Some(key_at(&pg, c)) };
                check_rec(p, id, child_at(&pg, c), clo, chi, depth + 1, st)?;
            }
        }
        k => return corrupt(format!("page {pno}: unexpected kind {k}")),
    }
    Ok(())
}

// ----------------------------------------------------------------- cursor

/// A position in a tree. Holds the pages on its path, so it reads a
/// consistent snapshot even while pages are copied under it.
pub struct Cursor<'p> {
    p: &'p Pager,
    /// Interior pages above the leaf and the child position taken in each.
    stack: Vec<(Page, usize)>,
    leaf: Option<Page>,
    idx: usize,
    started: bool,
}

/// A value as stored: inline, or in an overflow chain.
pub enum Val<'a> {
    Inline(&'a [u8]),
    Overflow { len: usize, first: u64 },
}

impl Val<'_> {
    pub fn load(&self, p: &Pager) -> SResult<Cow<'_, [u8]>> {
        match self {
            Val::Inline(b) => Ok(Cow::Borrowed(b)),
            Val::Overflow { len, first } => Ok(Cow::Owned(read_overflow(p, *first, *len)?)),
        }
    }
}

impl<'p> Cursor<'p> {
    fn seek(p: &'p Pager, root: u64, from: &[u8]) -> SResult<Cursor<'p>> {
        let mut cur = Cursor {
            p,
            stack: Vec::new(),
            leaf: None,
            idx: 0,
            started: false,
        };
        if root == 0 {
            return Ok(cur);
        }
        let mut pno = root;
        loop {
            let pg = p.get(pno)?;
            match page::kind(&pg) {
                KIND_INTERIOR => {
                    let c = child_pos(&pg, from);
                    let next = child_at(&pg, c);
                    cur.stack.push((pg, c));
                    pno = next;
                }
                KIND_LEAF => {
                    cur.idx = match search(&pg, from) {
                        Ok(i) | Err(i) => i,
                    };
                    cur.leaf = Some(pg);
                    return Ok(cur);
                }
                k => return corrupt(format!("page {pno}: unexpected kind {k}")),
            }
        }
    }

    /// Move to the next leaf, if any.
    fn next_leaf(&mut self) -> SResult<bool> {
        loop {
            let Some((pg, c)) = self.stack.pop() else {
                self.leaf = None;
                return Ok(false);
            };
            if c < page::nslots(&pg) {
                let mut child = child_at(&pg, c + 1);
                self.stack.push((pg, c + 1));
                // Leftmost path down.
                loop {
                    let cp = self.p.get(child)?;
                    if page::kind(&cp) == KIND_INTERIOR {
                        let next = child_at(&cp, 0);
                        self.stack.push((cp, 0));
                        child = next;
                    } else {
                        self.leaf = Some(cp);
                        self.idx = 0;
                        return Ok(true);
                    }
                }
            }
        }
    }

    /// Advance and return the next entry (the first one on the first call).
    pub fn next(&mut self) -> SResult<Option<(&[u8], Val<'_>)>> {
        if self.started {
            self.idx += 1;
        }
        self.started = true;
        loop {
            let Some(leaf) = &self.leaf else {
                return Ok(None);
            };
            if self.idx < page::nslots(leaf) {
                break;
            }
            if !self.next_leaf()? {
                return Ok(None);
            }
        }
        let leaf = self.leaf.as_ref().expect("positioned");
        let c = leaf_cell(leaf, self.idx);
        let v = if c.overflow {
            let (len, first) = overflow_ref(c.value);
            Val::Overflow { len, first }
        } else {
            Val::Inline(c.value)
        };
        Ok(Some((c.key, v)))
    }
}

// ---------------------------------------------------------------- builder

/// Builds a tree bottom-up from entries in strictly increasing key order:
/// every page is written once, full, and pages at each level are allocated
/// in order, so a freshly built tree reads sequentially.
pub struct Builder<'p> {
    p: &'p Pager,
    tree: Tree,
    /// Per level: (page being filled, its cells, first key, leftmost child).
    levels: Vec<Level>,
    last: Option<Vec<u8>>,
    fill: usize,
}

struct Level {
    cells: Vec<Vec<u8>>,
    bytes: usize,
    first_key: Vec<u8>,
    leftmost: u64,
}

impl<'p> Builder<'p> {
    /// `fill` is the fraction of a page to fill, in percent (100 for
    /// append-only data).
    pub fn new(p: &'p Pager, tree: Tree, fill: usize) -> Builder<'p> {
        Builder {
            p,
            tree,
            levels: Vec::new(),
            last: None,
            fill: fill.clamp(50, 100),
        }
    }

    pub fn push(&mut self, key: &[u8], value: &[u8]) -> SResult<()> {
        if let Some(l) = &self.last {
            if key <= l.as_slice() {
                return corrupt("builder keys must be strictly increasing");
            }
        }
        self.last = Some(key.to_vec());
        let cell = make_cell(self.p, self.tree.id, key, value)?;
        self.add(0, key, cell)
    }

    fn capacity(&self, level: usize) -> usize {
        let base = if level == 0 { LEAF_BASE } else { INT_BASE };
        (self.p.page_size() - base) * self.fill / 100
    }

    fn add(&mut self, level: usize, key: &[u8], cell: Vec<u8>) -> SResult<()> {
        if self.levels.len() <= level {
            self.levels.push(Level {
                cells: Vec::new(),
                bytes: 0,
                first_key: Vec::new(),
                leftmost: 0,
            });
        }
        let cap = self.capacity(level);
        let full = {
            let l = &self.levels[level];
            !l.cells.is_empty() && l.bytes + cell.len() + 2 > cap && (level > 0 || l.cells.len() >= 2)
                || (level > 0 && l.leftmost != 0 && l.bytes + cell.len() + 2 > cap)
        };
        if full {
            self.flush(level)?;
        }
        let l = &mut self.levels[level];
        if level > 0 && l.leftmost == 0 && l.cells.is_empty() {
            // First child of a fresh interior page becomes its leftmost.
            let (_, child) = int_cell_parts(&cell);
            l.leftmost = child;
            l.first_key = key.to_vec();
            return Ok(());
        }
        if l.cells.is_empty() && level == 0 {
            l.first_key = key.to_vec();
        }
        l.bytes += cell.len() + 2;
        l.cells.push(cell);
        Ok(())
    }

    /// Write the page being filled at `level` and pass it up.
    fn flush(&mut self, level: usize) -> SResult<()> {
        let l = std::mem::replace(
            &mut self.levels[level],
            Level {
                cells: Vec::new(),
                bytes: 0,
                first_key: Vec::new(),
                leftmost: 0,
            },
        );
        let kind = if level == 0 { KIND_LEAF } else { KIND_INTERIOR };
        let pno = self.p.alloc(kind, self.tree.id)?;
        self.p.modify(pno, |b| {
            if level > 0 {
                page::put_u64(b, HEADER, l.leftmost);
            }
            write_cells(b, &l.cells);
        })?;
        let up = int_cell_bytes(&l.first_key, pno);
        self.add(level + 1, &l.first_key, up)
    }

    /// Finish and install the tree (which must have been empty).
    pub fn finish(mut self) -> SResult<()> {
        if self.levels.is_empty() {
            return Ok(());
        }
        let mut level = 0;
        loop {
            let top = level + 1 == self.levels.len();
            let l = &self.levels[level];
            let only_child = level > 0 && l.cells.is_empty() && l.leftmost != 0;
            if top && only_child {
                // An interior level holding one child: that child is the root.
                let root = l.leftmost;
                if self.tree.root(self.p) != 0 {
                    return corrupt("builder target tree is not empty");
                }
                self.p.set_root(self.tree.slot, root);
                return Ok(());
            }
            if top && level == 0 && !l.cells.is_empty() {
                // Everything fit in one leaf.
                let cells = std::mem::take(&mut self.levels[0].cells);
                let pno = self.p.alloc(KIND_LEAF, self.tree.id)?;
                self.p.modify(pno, |b| write_cells(b, &cells))?;
                self.p.set_root(self.tree.slot, pno);
                return Ok(());
            }
            let empty = l.cells.is_empty() && l.leftmost == 0;
            if !empty {
                self.flush(level)?;
            }
            level += 1;
        }
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn contents(p: &Pager, t: Tree) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut c = t.scan(p).unwrap();
        let mut out = Vec::new();
        while let Some((k, v)) = c.next().unwrap() {
            let v = v.load(p).unwrap().into_owned();
            out.push((k.to_vec(), v));
        }
        out
    }

    fn val(r: &mut Rng) -> Vec<u8> {
        let n = match r.next() % 20 {
            0 => 700 + (r.next() % 3000) as usize, // overflows at 512-byte pages
            _ => (r.next() % 40) as usize,
        };
        (0..n).map(|i| (i as u64 ^ r.next()) as u8).collect()
    }

    /// Random puts, deletes, commits and rollbacks against a BTreeMap, with
    /// 512-byte pages so splits and overflow chains happen constantly.
    #[test]
    fn matches_a_btreemap_through_commits_and_rollbacks() {
        for seed in 1..=5u64 {
            let p = Pager::memory(512, u64::MAX);
            let t = Tree { slot: 0, id: 1 };
            let mut committed: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            let mut cur = committed.clone();
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            for step in 0..6000 {
                let k = (r.next() % 1500).to_be_bytes().to_vec();
                match r.next() % 10 {
                    0..=5 => {
                        let v = val(&mut r);
                        t.put(&p, &k, &v).unwrap();
                        cur.insert(k, v);
                    }
                    6..=7 => {
                        let had = t.delete(&p, &k).unwrap();
                        assert_eq!(had, cur.remove(&k).is_some());
                    }
                    8 => {
                        p.commit();
                        committed = cur.clone();
                    }
                    _ => {
                        if r.next() % 4 == 0 {
                            p.rollback();
                            cur = committed.clone();
                        }
                    }
                }
                if step % 500 == 0 {
                    let got = contents(&p, t);
                    let want: Vec<_> = cur.iter().map(|(a, b)| (a.clone(), b.clone())).collect();
                    assert_eq!(got, want, "seed {seed} step {step}");
                    let st = t.check(&p).unwrap();
                    assert_eq!(st.entries as usize, cur.len());
                    for (k, v) in cur.iter().take(50) {
                        assert_eq!(t.get(&p, k).unwrap().as_ref(), Some(v));
                    }
                }
            }
            // Seek lands on the first key >= target.
            let target = 700u64.to_be_bytes();
            let mut c = t.seek(&p, &target).unwrap();
            let got = c.next().unwrap().map(|(k, _)| k.to_vec());
            let want = cur.range(target.to_vec()..).next().map(|(k, _)| k.clone());
            assert_eq!(got, want);
            // Deleting everything empties the tree and frees every page.
            p.commit();
            let keys: Vec<_> = cur.keys().cloned().collect();
            for k in keys {
                assert!(t.delete(&p, &k).unwrap());
            }
            p.commit();
            assert_eq!(t.root(&p), 0);
            assert_eq!(p.memory_usage().unwrap().0, 0, "seed {seed}: pages leaked");
        }
    }

    #[test]
    fn builder_matches_incremental_inserts_and_packs_pages() {
        let p = Pager::memory(512, u64::MAX);
        let built = Tree { slot: 0, id: 1 };
        let grown = Tree { slot: 1, id: 2 };
        let mut r = Rng(77);
        let mut b = Builder::new(&p, built, 100);
        let mut want = Vec::new();
        for i in 0..5000u64 {
            let k = (i * 3).to_be_bytes().to_vec();
            let v = val(&mut r);
            b.push(&k, &v).unwrap();
            grown.put(&p, &k, &v).unwrap();
            want.push((k, v));
        }
        b.finish().unwrap();
        assert_eq!(contents(&p, built), want);
        assert_eq!(contents(&p, grown), want);
        let (sb, sg) = (built.check(&p).unwrap(), grown.check(&p).unwrap());
        assert_eq!(sb.entries, 5000);
        // Appends split at the right edge, so incremental growth packs
        // leaves nearly as tightly as the bulk build.
        assert!(sg.leaves as f64 <= sb.leaves as f64 * 1.15, "{sg:?} vs {sb:?}");
        assert!(built.get(&p, &(4998 * 3u64).to_be_bytes()).unwrap().is_some());
        assert!(built.get(&p, &1u64.to_be_bytes()).unwrap().is_none());
    }

    #[test]
    fn builder_handles_tiny_and_single_leaf_trees() {
        for n in [0u64, 1, 2, 3, 10] {
            let p = Pager::memory(512, u64::MAX);
            let t = Tree { slot: 0, id: 1 };
            let mut b = Builder::new(&p, t, 100);
            for i in 0..n {
                b.push(&i.to_be_bytes(), b"v").unwrap();
            }
            b.finish().unwrap();
            assert_eq!(contents(&p, t).len() as u64, n);
            assert_eq!(t.check(&p).unwrap().entries, n);
        }
    }

    /// A backup copies the pages of one checkpoint slowly, while the writer
    /// carries on. Under a hold, taking that checkpoint's superblock plus
    /// every page as it is *afterwards* still reads as that checkpoint;
    /// without one, reuse wrecks it.
    #[test]
    fn a_hold_keeps_a_checkpoint_readable_from_a_slow_copy() {
        use crate::storage::pager::Config;
        for held in [true, false] {
            let dir = std::env::temp_dir().join(format!("glider-hold-{}-{held}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("db");
            let cfg = Config {
                page_size: 512,
                cache_bytes: 512 * 16,
                max_memory: u64::MAX,
                segment_bytes: 1 << 30,
            };
            let p = Pager::create(&path, &cfg, [3; 16]).unwrap();
            let t = Tree { slot: 0, id: 1 };
            let mut r = Rng(9);
            let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            let churn = |p: &Pager, r: &mut Rng, model: &mut BTreeMap<Vec<u8>, Vec<u8>>, n: usize| {
                for _ in 0..n {
                    let k = (r.next() % 2000).to_be_bytes().to_vec();
                    if r.next() % 4 == 0 {
                        t.delete(p, &k).unwrap();
                        model.remove(&k);
                    } else {
                        let v = val(r);
                        t.put(p, &k, &v).unwrap();
                        model.insert(k, v);
                    }
                }
                p.commit();
            };
            churn(&p, &mut r, &mut model, 3000);
            p.checkpoint(0).unwrap();
            if held {
                p.set_hold(true, 77);
            }
            p.checkpoint(0).unwrap();
            let snap = model.clone();
            // The superblock in force now.
            let img = std::fs::read(&path).unwrap();
            let sb = |slot: usize| &img[slot * 512..(slot + 1) * 512];
            let epoch = |b: &[u8]| page::get_u64(b, 48);
            let slot = if epoch(sb(0)) >= epoch(sb(1)) { 0 } else { 1 };
            let saved = sb(slot).to_vec();
            if held {
                assert_eq!(page::get_u64(&saved, 96), 77, "the superblock names the hold");
            }
            for _ in 0..20 {
                churn(&p, &mut r, &mut model, 300);
                p.checkpoint(0).unwrap();
            }
            // The slow copy: every page as it is now, the old superblock.
            let mut copy = std::fs::read(&path).unwrap();
            copy[slot * 512..(slot + 1) * 512].copy_from_slice(&saved);
            copy[(1 - slot) * 512..(2 - slot) * 512].fill(0);
            let copy_path = dir.join("copy");
            std::fs::write(&copy_path, &copy).unwrap();
            let read = Pager::open(&copy_path, 512 * 16).and_then(|c| {
                t.check(&c)?;
                let mut cur = t.scan(&c)?;
                let mut out = BTreeMap::new();
                while let Some((k, v)) = cur.next()? {
                    out.insert(k.to_vec(), v.load(&c)?.into_owned());
                }
                Ok(out)
            });
            if held {
                assert_eq!(read.unwrap(), snap, "held checkpoint reads back intact");
                assert!(p.held_pages() > 0);
                // Released: held pages go back into use.
                p.set_hold(false, 0);
                p.checkpoint(0).unwrap();
                assert_eq!(p.held_pages(), 0);
                let hw = p.high_water();
                for _ in 0..5 {
                    churn(&p, &mut r, &mut model, 300);
                    p.checkpoint(0).unwrap();
                }
                assert!(p.high_water() <= hw + 16, "released pages are reused");
                drop(p);
                let again = Pager::open(&path, 512 * 16).unwrap();
                assert_eq!(contents(&again, t), model.into_iter().collect::<Vec<_>>());
            } else {
                assert!(read.map(|x| x != snap).unwrap_or(true), "without a hold the copy is torn");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
