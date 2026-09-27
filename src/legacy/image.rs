//! The snapshot image: current graph state as flat, offset-addressed columns.
//!
//! Replaying a log means building the graph one mutation at a time — a hash
//! insert and a couple of small allocations per record — and that construction
//! was 93% of open time. An image is instead a handful of arrays written as
//! they sit in memory. Opening one reads only its header, directory and a few
//! small tables; every column is loaded, checksummed and validated the first
//! time something touches it, and property values a 64 KiB chunk at a time.
//! Open cost is therefore independent of the image's size, and a query pays
//! only for the parts of the file it reads — the same bargain SQLite makes
//! with its pages.
//!
//! Every live node and edge gets a *slot*, its position in ascending id order.
//! Each table is one column indexed by slot:
//!
//! ```text
//! image header (64 B)   magic, version, section count, n, m, next ids, crc
//! directory             per section: kind, offset, length, crc32
//! STRINGS               the interner, in id order, so string ids survive
//! NODE_IDS              u64[n], ascending
//! NODE_LABEL_OFF/LABELS u32[n+1] offsets into u32 string ids
//! NODE_PROP_OFF         u64[n+1] offsets into NODE_PROPS
//! OUT_OFF/NBR/EDGE      CSR: u32[n+1], neighbour slot u32[m], edge slot u32[m]
//! IN_OFF/NBR/EDGE       the same for incoming edges
//! EDGE_IDS              u64[m], ascending
//! EDGE_FROM/TO/TYPE     u32[m] node slots and string id
//! EDGE_PROP_OFF         u64[m+1] offsets into EDGE_PROPS
//! LABEL_OFF/NODES       per string id, the node slots carrying that label
//! TYPE_COUNTS           edges per type, so counting them reads nothing big
//! INDEX_DIR             per property index: label, key, sizes
//! INDEX_BODY + i        per index: sorted encoded keys, slot postings
//! NODE_PROPS/EDGE_PROPS property runs, the bulky cold part
//! *_PROPS_CRC           one crc32 per 64 KiB chunk of each props blob
//! ```
//!
//! (Version 1 images kept all indexes in one PROP_INDEX section and had no
//! TYPE_COUNTS; they still load, with those parts read at open.)
//!
//! The image is immutable. Changes made after it was written live in the
//! graph's delta overlay (see `graph.rs`) until the next compaction folds them
//! in.
//!
//! **Integrity.** Every byte of an image is covered by a checksum, checked
//! when it is loaded. Accessors cannot fail — a traversal has nowhere to put
//! an I/O error — so a section that fails its check is recorded as the
//! graph's integrity error and read as zeros of the right shape, which never
//! panics. `query::execute` refuses to return results from a graph with an
//! integrity error, and compaction refuses to run, so damage is reported and
//! never rewritten into a fresh image. `OpenOptions::preload` checks
//! everything at open instead; `glider <db> verify` checks without opening.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::codec::{self, Crc32, Reader};
use crate::types::VKey;
use crate::pread::PosFile;
use crate::value::Value;

pub const IMAGE_MAGIC: &[u8; 8] = b"GLIMG\x00\x00\x01";
pub const IMAGE_VERSION: u32 = 2;
const IMAGE_HEADER_LEN: u64 = 64;
const DIR_ENTRY_LEN: u64 = 32;
/// Granularity of property checksums and property loading.
pub const PROP_CHUNK: u64 = 64 * 1024;
/// Columns are read in pieces this big, so a load never holds a second full
/// copy of a section in raw bytes alongside its decoded form.
const READ_CHUNK: usize = 4 << 20;

const S_STRINGS: u32 = 1;
const S_NODE_IDS: u32 = 2;
const S_NODE_LABEL_OFF: u32 = 3;
const S_NODE_LABELS: u32 = 4;
const S_NODE_PROP_OFF: u32 = 5;
const S_OUT_OFF: u32 = 6;
const S_OUT_NBR: u32 = 7;
const S_OUT_EDGE: u32 = 8;
const S_IN_OFF: u32 = 9;
const S_IN_NBR: u32 = 10;
const S_IN_EDGE: u32 = 11;
const S_EDGE_IDS: u32 = 12;
const S_EDGE_FROM: u32 = 13;
const S_EDGE_TO: u32 = 14;
const S_EDGE_TYPE: u32 = 15;
const S_EDGE_PROP_OFF: u32 = 16;
const S_LABEL_OFF: u32 = 17;
const S_LABEL_NODES: u32 = 18;
/// Version 1 only: every index in one section.
const S_PROP_INDEX: u32 = 19;
const S_NODE_PROPS: u32 = 20;
const S_EDGE_PROPS: u32 = 21;
const S_NODE_PROPS_CRC: u32 = 22;
const S_EDGE_PROPS_CRC: u32 = 23;
const S_TYPE_COUNTS: u32 = 24;
const S_INDEX_DIR: u32 = 25;
/// Index `i`'s body is section `S_INDEX_BODY + i`.
const S_INDEX_BODY: u32 = 1000;
/// Sections in every version-2 image, before the per-index bodies.
const FIXED_SECTIONS: u32 = 24;

/// Where property values live once an image is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Residency {
    /// Loaded into memory a 64 KiB chunk at a time, as queries first touch
    /// them, and kept. Reads after the first are as fast as memory. The
    /// default.
    #[default]
    Memory,
    /// Read from disk on demand through a cache of at most `cache_bytes`.
    /// Topology, labels and indexes are still held in memory once touched,
    /// so traversal stays fast; property-heavy scans pay for disk reads. For
    /// devices, or images larger than RAM.
    OnDisk { cache_bytes: usize },
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

// ------------------------------------------------------------ sections

#[derive(Clone, Copy, Default, Debug)]
struct Section {
    off: u64,
    len: u64,
    crc: u32,
}

/// Something to read image bytes out of: a file, or a slice in memory.
trait Source {
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()>;
}

impl Source for PosFile {
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        self.read_exact_at(buf, off)
    }
}

struct Bytes<'a>(&'a [u8]);

impl Source for Bytes<'_> {
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        let start = usize::try_from(off).map_err(|_| bad("offset out of range"))?;
        let end = start
            .checked_add(buf.len())
            .filter(|e| *e <= self.0.len())
            .ok_or_else(|| bad("image is truncated"))?;
        buf.copy_from_slice(&self.0[start..end]);
        Ok(())
    }
}

/// Fixed-width little-endian column elements.
trait Word: Copy + Default + Send + Sync + 'static {
    const SIZE: usize;
    fn read_le(b: &[u8]) -> Self;
    fn widen(self) -> u64;
}

impl Word for u32 {
    const SIZE: usize = 4;
    fn read_le(b: &[u8]) -> Self {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
    fn widen(self) -> u64 {
        self as u64
    }
}

impl Word for u64 {
    const SIZE: usize = 8;
    fn read_le(b: &[u8]) -> Self {
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    }
    fn widen(self) -> u64 {
        self
    }
}

/// Structural rule a column must satisfy, checked as it streams in.
#[derive(Clone, Copy, Debug)]
enum Check {
    None,
    /// Strictly ascending (ids).
    Ascending,
    /// Offsets: start at 0, never decrease, end at `total`.
    Offsets { total: u64 },
    /// Every value below the bound (slots, string ids).
    Below(u64),
}

struct Validator {
    check: Check,
    seen: u64,
    prev: u64,
    ok: bool,
}

impl Validator {
    fn new(check: Check) -> Validator {
        Validator {
            check,
            seen: 0,
            prev: 0,
            ok: true,
        }
    }

    #[inline]
    fn push(&mut self, v: u64) {
        match self.check {
            Check::None => {}
            Check::Ascending => self.ok &= self.seen == 0 || v > self.prev,
            Check::Offsets { .. } => self.ok &= if self.seen == 0 { v == 0 } else { v >= self.prev },
            Check::Below(b) => self.ok &= v < b,
        }
        self.prev = v;
        self.seen += 1;
    }

    fn finish(&self) -> bool {
        self.ok
            && match self.check {
                Check::Offsets { total } => self.seen > 0 && self.prev == total,
                _ => true,
            }
    }
}

/// One column of the image, loaded on first use.
struct Col<T> {
    kind: u32,
    sec: Section,
    count: u64,
    check: Check,
    cell: OnceLock<Vec<T>>,
}

impl<T: Word> Col<T> {
    fn ready(v: Vec<T>) -> Col<T> {
        Col {
            kind: 0,
            sec: Section::default(),
            count: v.len() as u64,
            check: Check::None,
            cell: OnceLock::from(v),
        }
    }

    /// Read, checksum and validate the column. `keep` false streams it
    /// through the checks without holding it, for verification.
    fn read(&self, src: &dyn Source, at: u64, keep: bool) -> Result<Vec<T>, String> {
        let what = || format!("image section {}", self.kind);
        if self.sec.len != self.count * T::SIZE as u64 {
            return Err(format!("{}: length does not match its count", what()));
        }
        let mut out = Vec::with_capacity(if keep { self.count as usize } else { 0 });
        let mut v = Validator::new(self.check);
        let mut buf = vec![0u8; (self.sec.len as usize).min(READ_CHUNK)];
        let mut crc = Crc32::new();
        let mut done = 0u64;
        while done < self.sec.len {
            let k = ((self.sec.len - done) as usize).min(READ_CHUNK);
            src.read_at(&mut buf[..k], at + self.sec.off + done)
                .map_err(|e| format!("{}: {e}", what()))?;
            crc.update(&buf[..k]);
            // READ_CHUNK is a multiple of 8, so a chunk never splits a word.
            for w in buf[..k].chunks_exact(T::SIZE) {
                let x = T::read_le(w);
                v.push(x.widen());
                if keep {
                    out.push(x);
                }
            }
            done += k as u64;
        }
        if crc.finish() != self.sec.crc {
            return Err(format!("{}: checksum mismatch", what()));
        }
        if !v.finish() {
            return Err(format!("{}: structure is invalid", what()));
        }
        Ok(out)
    }
}

// ------------------------------------------------------------- slot lookup

/// id -> slot. Ids are mostly dense, so a direct table usually costs less
/// than a hash map would; sparse ids fall back to binary search.
enum SlotIndex {
    Dense(Vec<u32>),
    Sorted,
}

impl SlotIndex {
    fn build(ids: &[u64]) -> SlotIndex {
        let max = ids.last().copied().unwrap_or(0);
        if max <= 2 * ids.len() as u64 + 1024 {
            let mut v = vec![u32::MAX; max as usize + 1];
            for (slot, id) in ids.iter().enumerate() {
                if let Some(x) = v.get_mut(*id as usize) {
                    *x = slot as u32;
                }
            }
            SlotIndex::Dense(v)
        } else {
            SlotIndex::Sorted
        }
    }

    #[inline]
    fn get(&self, ids: &[u64], id: u64) -> Option<u32> {
        match self {
            SlotIndex::Dense(v) => {
                let s = *v.get(usize::try_from(id).ok()?)?;
                (s != u32::MAX).then_some(s)
            }
            SlotIndex::Sorted => ids.binary_search(&id).ok().map(|s| s as u32),
        }
    }
}

// ----------------------------------------------------------- property store

struct Props {
    sec: Section,
    crcs: Col<u32>,
    /// `Residency::Memory`: every chunk, loaded on first touch and kept.
    mem: Option<Vec<OnceLock<Box<[u8]>>>>,
}

impl Props {
    fn empty() -> Props {
        Props {
            sec: Section::default(),
            crcs: Col::ready(Vec::new()),
            mem: Some(Vec::new()),
        }
    }

    fn chunks(&self) -> u64 {
        self.sec.len.div_ceil(PROP_CHUNK)
    }
}

/// (which blob, chunk index)
type ChunkKey = (u8, u64);

struct DiskCache {
    cap: usize,
    lru: Mutex<Lru>,
}

#[derive(Default)]
struct Lru {
    /// chunk -> (bytes, last use)
    map: HashMap<ChunkKey, (Arc<Vec<u8>>, u64)>,
    bytes: usize,
    tick: u64,
}

/// A caller-held window onto one property chunk, for reading runs in order
/// without filling the graph's cache: compaction streams every property
/// through this.
#[derive(Default)]
pub(crate) struct ChunkScratch {
    held: Option<(u8, u64, Vec<u8>)>,
}

// --------------------------------------------------------------- indexes

pub(crate) struct BaseIndex {
    pub label: u32,
    pub key: u32,
    nkeys: u64,
    npost: u64,
    keys_len: u64,
    body_sec: Section,
    kind: u32,
    body: OnceLock<IndexBody>,
}

#[derive(Default)]
struct IndexBody {
    key_off: Vec<u64>,
    keys: Vec<u8>,
    post_off: Vec<u32>,
    post: Vec<u32>,
}

impl IndexBody {
    fn key_count(&self) -> usize {
        self.key_off.len().saturating_sub(1)
    }

    fn key(&self, i: usize) -> Value {
        let run = self
            .keys
            .get(self.key_off[i] as usize..self.key_off[i + 1] as usize)
            .unwrap_or(&[]);
        // Every key was checked to decode when the body was loaded.
        Reader::new(run).value().unwrap_or(Value::Null)
    }

    fn postings(&self, i: usize) -> &[u32] {
        self.post
            .get(self.post_off[i] as usize..self.post_off[i + 1] as usize)
            .unwrap_or(&[])
    }

    fn lookup(&self, v: &Value) -> &[u32] {
        let (mut lo, mut hi) = (0usize, self.key_count());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.key(mid).total_cmp(v) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return self.postings(mid),
            }
        }
        &[]
    }

    /// Parse a body laid out as: key_off u32[nkeys+1], keys, post_off
    /// u32[nkeys+1], post u32[npost]; validating everything.
    fn parse(
        r: &mut Reader,
        nkeys: usize,
        npost: usize,
        keys_len: usize,
        n: usize,
    ) -> Result<IndexBody, String> {
        let mut take = |len: usize| -> Result<&[u8], String> {
            if r.remaining() < len {
                return Err("index body truncated".into());
            }
            let s = &r.buf[r.pos..r.pos + len];
            r.pos += len;
            Ok(s)
        };
        let words = |b: &[u8]| -> Vec<u32> { b.chunks_exact(4).map(<u32 as Word>::read_le).collect() };
        let key_off: Vec<u64> = words(take(4 * (nkeys + 1))?)
            .into_iter()
            .map(u64::from)
            .collect();
        let keys = take(keys_len)?.to_vec();
        let post_off = words(take(4 * (nkeys + 1))?);
        let post = words(take(4 * npost)?);
        let body = IndexBody {
            key_off,
            keys,
            post_off,
            post,
        };
        let offsets = |o: &[u64], total: u64| {
            o.len() == nkeys + 1
                && o[0] == 0
                && o.windows(2).all(|w| w[0] <= w[1])
                && *o.last().unwrap() == total
        };
        let post_off64: Vec<u64> = body.post_off.iter().map(|x| *x as u64).collect();
        let ok = offsets(&body.key_off, keys_len as u64)
            && offsets(&post_off64, npost as u64)
            && body.post.iter().all(|s| (*s as usize) < n)
            && (0..nkeys).all(|i| {
                let run = &body.keys[body.key_off[i] as usize..body.key_off[i + 1] as usize];
                let mut kr = Reader::new(run);
                kr.skip_value().is_ok() && kr.remaining() == 0
            });
        if ok {
            Ok(body)
        } else {
            Err("property index is invalid".into())
        }
    }
}

// ------------------------------------------------------------------- base

/// The immutable part of a graph: everything the last compaction wrote.
pub(crate) struct Base {
    n: usize,
    m: usize,
    /// Where lazily loaded parts come from. `None` once everything is in
    /// memory (an image loaded from bytes).
    file: Option<PosFile>,
    /// The image's offset in `file`.
    at: u64,
    node_ids: Col<u64>,
    node_slot: OnceLock<SlotIndex>,
    node_label_off: Col<u32>,
    node_labels: Col<u32>,
    node_prop_off: Col<u64>,
    out_off: Col<u32>,
    out_nbr: Col<u32>,
    out_edge: Col<u32>,
    in_off: Col<u32>,
    in_nbr: Col<u32>,
    in_edge: Col<u32>,
    edge_ids: Col<u64>,
    edge_slot: OnceLock<SlotIndex>,
    edge_from: Col<u32>,
    edge_to: Col<u32>,
    edge_type: Col<u32>,
    edge_prop_off: Col<u64>,
    /// Read at open: small, and what label counts come from.
    label_off: Vec<u32>,
    label_nodes: Col<u32>,
    /// Read at open from TYPE_COUNTS, or computed on first use for a
    /// version-1 image.
    type_counts: OnceLock<HashMap<u32, usize>>,
    pub indexes: Vec<BaseIndex>,
    node_props: Props,
    edge_props: Props,
    disk: Option<DiskCache>,
    error: OnceLock<String>,
    read_errors: AtomicU64,
    pub next_node: u64,
    pub next_edge: u64,
    pub image_bytes: u64,
}

impl Base {
    pub fn empty() -> Base {
        Base {
            n: 0,
            m: 0,
            file: None,
            at: 0,
            node_ids: Col::ready(Vec::new()),
            node_slot: OnceLock::from(SlotIndex::Sorted),
            node_label_off: Col::ready(vec![0]),
            node_labels: Col::ready(Vec::new()),
            node_prop_off: Col::ready(vec![0]),
            out_off: Col::ready(vec![0]),
            out_nbr: Col::ready(Vec::new()),
            out_edge: Col::ready(Vec::new()),
            in_off: Col::ready(vec![0]),
            in_nbr: Col::ready(Vec::new()),
            in_edge: Col::ready(Vec::new()),
            edge_ids: Col::ready(Vec::new()),
            edge_slot: OnceLock::from(SlotIndex::Sorted),
            edge_from: Col::ready(Vec::new()),
            edge_to: Col::ready(Vec::new()),
            edge_type: Col::ready(Vec::new()),
            edge_prop_off: Col::ready(vec![0]),
            label_off: vec![0],
            label_nodes: Col::ready(Vec::new()),
            type_counts: OnceLock::from(HashMap::new()),
            indexes: Vec::new(),
            node_props: Props::empty(),
            edge_props: Props::empty(),
            disk: None,
            error: OnceLock::new(),
            read_errors: AtomicU64::new(0),
            next_node: 1,
            next_edge: 1,
            image_bytes: 0,
        }
    }

    // ------------------------------------------------------ lazy loading

    fn src(&self) -> Option<&dyn Source> {
        self.file.as_ref().map(|f| f as &dyn Source)
    }

    /// Record damage. The first message is kept; every one is counted.
    fn fail(&self, msg: String) {
        self.read_errors.fetch_add(1, Ordering::Relaxed);
        let _ = self.error.set(msg);
    }

    fn load<T: Word>(&self, c: &Col<T>, src: Option<&dyn Source>) -> Vec<T> {
        let r = match src {
            Some(s) => c.read(s, self.at, true),
            None => Err(format!("image section {} is not loaded", c.kind)),
        };
        r.unwrap_or_else(|e| {
            self.fail(e);
            vec![T::default(); c.count as usize]
        })
    }

    #[inline]
    fn col<'a, T: Word>(&'a self, c: &'a Col<T>) -> &'a [T] {
        c.cell.get_or_init(|| self.load(c, self.src()))
    }

    fn index_body(&self, i: usize, src: Option<&dyn Source>) -> &IndexBody {
        let x = &self.indexes[i];
        x.body.get_or_init(|| {
            let r = (|| -> Result<IndexBody, String> {
                let src = src.ok_or("index is not loaded")?;
                let mut buf = vec![0u8; x.body_sec.len as usize];
                src.read_at(&mut buf, self.at + x.body_sec.off)
                    .map_err(|e| e.to_string())?;
                if codec::crc32(&buf) != x.body_sec.crc {
                    return Err("checksum mismatch".into());
                }
                let mut r = Reader::new(&buf);
                let body = IndexBody::parse(
                    &mut r,
                    x.nkeys as usize,
                    x.npost as usize,
                    x.keys_len as usize,
                    self.n,
                )?;
                if r.remaining() != 0 {
                    return Err("trailing bytes".into());
                }
                Ok(body)
            })();
            r.unwrap_or_else(|e| {
                self.fail(format!("image section {}: {e}", x.kind));
                IndexBody {
                    key_off: vec![0],
                    post_off: vec![0],
                    ..IndexBody::default()
                }
            })
        })
    }

    fn read_chunk(&self, p: &Props, i: u64, src: &dyn Source, crcs: &[u32]) -> io::Result<Vec<u8>> {
        let start = i * PROP_CHUNK;
        let n = (p.sec.len - start).min(PROP_CHUNK) as usize;
        let mut buf = vec![0u8; n];
        src.read_at(&mut buf, self.at + p.sec.off + start)?;
        if crcs.get(i as usize).copied() != Some(codec::crc32(&buf)) {
            return Err(bad("property chunk checksum mismatch"));
        }
        Ok(buf)
    }

    fn mem_chunk<'a>(&'a self, p: &'a Props, i: u64, src: Option<&dyn Source>) -> &'a [u8] {
        let Some(mem) = &p.mem else { return &[] };
        let Some(cell) = mem.get(i as usize) else {
            return &[];
        };
        cell.get_or_init(|| {
            let crcs = self.col(&p.crcs);
            let r = match src {
                Some(s) => self.read_chunk(p, i, s, crcs),
                None => Err(bad("property chunk is not loaded")),
            };
            r.map(|v| v.into_boxed_slice()).unwrap_or_else(|e| {
                self.fail(format!("property chunk {i}: {e}"));
                let len = (p.sec.len - i * PROP_CHUNK).min(PROP_CHUNK) as usize;
                vec![0u8; len].into_boxed_slice()
            })
        })
    }

    /// Load, check and keep everything, from `src` (or the file). With
    /// `props` false, property chunks are left alone. In on-disk residency
    /// property chunks are checked but not kept. Returns the first
    /// integrity error, if any.
    fn fill(&self, src: Option<&dyn Source>, props: bool) -> Result<(), String> {
        let src = src.or(self.src());
        macro_rules! force {
            ($($c:ident),*) => {$(
                self.$c.cell.get_or_init(|| self.load(&self.$c, src));
            )*};
        }
        force!(node_label_off, node_labels, out_off, out_nbr, out_edge, in_off, in_nbr, in_edge);
        force!(edge_from, edge_to, edge_type, label_nodes);
        force!(node_ids, node_prop_off, edge_ids, edge_prop_off);
        for i in 0..self.indexes.len() {
            self.index_body(i, src);
        }
        if props {
            for (which, p) in [(0u8, &self.node_props), (1, &self.edge_props)] {
                p.crcs.cell.get_or_init(|| self.load(&p.crcs, src));
                for i in 0..p.chunks() {
                    if p.mem.is_some() {
                        self.mem_chunk(p, i, src);
                    } else if let Some(s) = src {
                        if let Err(e) = self.read_chunk(p, i, s, self.col(&p.crcs)) {
                            self.fail(format!("property blob {which} chunk {i}: {e}"));
                        }
                    }
                }
            }
        }
        match self.error.get() {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Load and check everything now rather than on first use.
    pub fn preload(&self) -> Result<(), String> {
        self.fill(None, true)
    }

    // ---------------------------------------------------------- accessors

    #[inline]
    pub fn n(&self) -> usize {
        self.n
    }
    #[inline]
    pub fn m(&self) -> usize {
        self.m
    }
    #[inline]
    pub fn node_slot(&self, id: u64) -> Option<u32> {
        if self.n == 0 {
            return None;
        }
        let ids = self.col(&self.node_ids);
        self.node_slot
            .get_or_init(|| SlotIndex::build(ids))
            .get(ids, id)
    }
    #[inline]
    pub fn edge_slot(&self, id: u64) -> Option<u32> {
        if self.m == 0 {
            return None;
        }
        let ids = self.col(&self.edge_ids);
        self.edge_slot
            .get_or_init(|| SlotIndex::build(ids))
            .get(ids, id)
    }
    #[inline]
    pub fn node_id(&self, s: u32) -> u64 {
        self.col(&self.node_ids).get(s as usize).copied().unwrap_or(0)
    }
    #[inline]
    pub fn edge_id(&self, s: u32) -> u64 {
        self.col(&self.edge_ids).get(s as usize).copied().unwrap_or(0)
    }
    pub fn node_ids(&self) -> &[u64] {
        self.col(&self.node_ids)
    }
    pub fn edge_ids(&self) -> &[u64] {
        self.col(&self.edge_ids)
    }
    #[inline]
    pub fn node_labels(&self, s: u32) -> &[u32] {
        let off = self.col(&self.node_label_off);
        let s = s as usize;
        match (off.get(s), off.get(s + 1)) {
            (Some(a), Some(b)) => self
                .col(&self.node_labels)
                .get(*a as usize..*b as usize)
                .unwrap_or(&[]),
            _ => &[],
        }
    }
    #[inline]
    pub fn edge_from(&self, s: u32) -> u64 {
        let v = self.col(&self.edge_from).get(s as usize).copied();
        self.node_id(v.unwrap_or(0))
    }
    #[inline]
    pub fn edge_to(&self, s: u32) -> u64 {
        let v = self.col(&self.edge_to).get(s as usize).copied();
        self.node_id(v.unwrap_or(0))
    }
    #[inline]
    pub fn edge_type(&self, s: u32) -> u32 {
        self.col(&self.edge_type).get(s as usize).copied().unwrap_or(0)
    }
    pub fn edge_types(&self) -> &[u32] {
        self.col(&self.edge_type)
    }

    /// Adjacency run of a node slot: `(neighbour slot, edge slot)` pairs, in
    /// ascending edge id order.
    #[inline]
    pub fn adj(&self, s: u32, out: bool) -> impl Iterator<Item = (u32, u32)> + '_ {
        let (off, nbr, edge) = if out {
            (&self.out_off, &self.out_nbr, &self.out_edge)
        } else {
            (&self.in_off, &self.in_nbr, &self.in_edge)
        };
        let off = self.col(off);
        let r = match (off.get(s as usize), off.get(s as usize + 1)) {
            (Some(a), Some(b)) => *a as usize..*b as usize,
            _ => 0..0,
        };
        let nbr = self.col(nbr).get(r.clone()).unwrap_or(&[]);
        let edge = self.col(edge).get(r).unwrap_or(&[]);
        nbr.iter().copied().zip(edge.iter().copied())
    }

    #[inline]
    pub fn degree(&self, s: u32, out: bool) -> usize {
        let off = self.col(if out { &self.out_off } else { &self.in_off });
        match (off.get(s as usize), off.get(s as usize + 1)) {
            (Some(a), Some(b)) => b.saturating_sub(*a) as usize,
            _ => 0,
        }
    }

    /// Node slots carrying a label, ascending.
    pub fn label_members(&self, l: u32) -> &[u32] {
        let l = l as usize;
        match (self.label_off.get(l), self.label_off.get(l + 1)) {
            (Some(a), Some(b)) if a < b => self
                .col(&self.label_nodes)
                .get(*a as usize..*b as usize)
                .unwrap_or(&[]),
            _ => &[],
        }
    }

    /// How many image nodes carry a label. Reads nothing lazily.
    pub fn label_len(&self, l: u32) -> usize {
        let l = l as usize;
        match (self.label_off.get(l), self.label_off.get(l + 1)) {
            (Some(a), Some(b)) => b.saturating_sub(*a) as usize,
            _ => 0,
        }
    }

    /// Labels with at least one member.
    pub fn labels(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.label_off.len().saturating_sub(1) as u32).filter(move |l| self.label_len(*l) > 0)
    }

    pub fn type_counts(&self) -> &HashMap<u32, usize> {
        self.type_counts.get_or_init(|| {
            let mut m: HashMap<u32, usize> = HashMap::new();
            for t in self.col(&self.edge_type) {
                *m.entry(*t).or_default() += 1;
            }
            m
        })
    }

    pub fn index_key_count(&self, i: usize) -> usize {
        self.indexes[i].nkeys as usize
    }

    pub fn index_lookup(&self, i: usize, v: &Value) -> &[u32] {
        self.index_body(i, self.src()).lookup(v)
    }

    pub fn index_buckets(&self, i: usize) -> impl Iterator<Item = &[u32]> + '_ {
        let b = self.index_body(i, self.src());
        (0..b.key_count()).map(move |k| b.postings(k))
    }

    pub fn props_on_disk(&self) -> bool {
        self.disk.is_some()
    }

    pub fn read_errors(&self) -> u64 {
        self.read_errors.load(Ordering::Relaxed)
    }

    pub fn integrity_error(&self) -> Option<&str> {
        self.error.get().map(|s| s.as_str())
    }

    fn prop_range(&self, which: u8, s: u32) -> (u64, u64) {
        let off = self.col(if which == 0 {
            &self.node_prop_off
        } else {
            &self.edge_prop_off
        });
        match (off.get(s as usize), off.get(s as usize + 1)) {
            (Some(a), Some(b)) if a <= b => (*a, *b),
            _ => (0, 0),
        }
    }

    fn run(&self, which: u8, s: u32) -> Cow<'_, [u8]> {
        let (a, b) = self.prop_range(which, s);
        if a == b {
            return Cow::Borrowed(&[]);
        }
        let p = if which == 0 {
            &self.node_props
        } else {
            &self.edge_props
        };
        let (c0, c1) = (a / PROP_CHUNK, (b - 1) / PROP_CHUNK);
        if p.mem.is_some() {
            if c0 == c1 {
                let chunk = self.mem_chunk(p, c0, self.src());
                let base = c0 * PROP_CHUNK;
                return Cow::Borrowed(
                    chunk
                        .get((a - base) as usize..(b - base) as usize)
                        .unwrap_or(&[]),
                );
            }
            let mut out = Vec::with_capacity((b - a) as usize);
            for c in c0..=c1 {
                let chunk = self.mem_chunk(p, c, self.src());
                let base = c * PROP_CHUNK;
                let from = a.max(base) - base;
                let to = (b.min(base + PROP_CHUNK) - base) as usize;
                out.extend_from_slice(chunk.get(from as usize..to).unwrap_or(&[]));
            }
            return Cow::Owned(out);
        }
        let mut out = Vec::with_capacity((b - a) as usize);
        for c in c0..=c1 {
            let Ok(chunk) = self.disk_chunk(which, p, c) else {
                return Cow::Owned(Vec::new());
            };
            let base = c * PROP_CHUNK;
            let from = a.max(base) - base;
            let to = (b.min(base + PROP_CHUNK) - base) as usize;
            out.extend_from_slice(chunk.get(from as usize..to).unwrap_or(&[]));
        }
        Cow::Owned(out)
    }

    fn disk_chunk(&self, which: u8, p: &Props, idx: u64) -> io::Result<Arc<Vec<u8>>> {
        let (Some(cache), Some(file)) = (&self.disk, &self.file) else {
            return Err(bad("no property file"));
        };
        {
            let mut lru = cache.lru.lock().unwrap_or_else(|e| e.into_inner());
            lru.tick += 1;
            let tick = lru.tick;
            if let Some((bytes, used)) = lru.map.get_mut(&(which, idx)) {
                *used = tick;
                return Ok(bytes.clone());
            }
        }
        // Read outside the lock so a slow disk does not serialise readers.
        let bytes = match self.read_chunk(p, idx, file, self.col(&p.crcs)) {
            Ok(b) => Arc::new(b),
            Err(e) => {
                self.fail(format!("property blob {which} chunk {idx}: {e}"));
                return Err(e);
            }
        };
        let n = bytes.len();
        let mut lru = cache.lru.lock().unwrap_or_else(|e| e.into_inner());
        lru.tick += 1;
        let tick = lru.tick;
        lru.bytes += n;
        if let Some((old, _)) = lru.map.insert((which, idx), (bytes.clone(), tick)) {
            lru.bytes -= old.len();
        }
        while lru.bytes > cache.cap && lru.map.len() > 1 {
            let victim = lru
                .map
                .iter()
                .filter(|(k, _)| **k != (which, idx))
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| *k);
            let Some(k) = victim else { break };
            if let Some((old, _)) = lru.map.remove(&k) {
                lru.bytes -= old.len();
            }
        }
        Ok(bytes)
    }

    /// A property run read through `scratch` instead of the graph's cache.
    /// For streaming every property in slot order, as compaction does.
    fn run_scratch(&self, which: u8, s: u32, scratch: &mut ChunkScratch) -> Vec<u8> {
        let (a, b) = self.prop_range(which, s);
        let mut out = Vec::with_capacity((b - a) as usize);
        if a == b {
            return out;
        }
        let p = if which == 0 {
            &self.node_props
        } else {
            &self.edge_props
        };
        for c in a / PROP_CHUNK..=(b - 1) / PROP_CHUNK {
            let base = c * PROP_CHUNK;
            let from = (a.max(base) - base) as usize;
            let to = (b.min(base + PROP_CHUNK) - base) as usize;
            // Already resident: use it.
            if let Some(chunk) = p
                .mem
                .as_ref()
                .and_then(|m| m.get(c as usize))
                .and_then(|cell| cell.get())
            {
                out.extend_from_slice(chunk.get(from..to).unwrap_or(&[]));
                continue;
            }
            let hit = matches!(&scratch.held, Some((w, i, _)) if *w == which && *i == c);
            if !hit {
                let bytes = match self.src() {
                    Some(src) => self.read_chunk(p, c, src, self.col(&p.crcs)),
                    None => Err(bad("property chunk is not loaded")),
                };
                match bytes {
                    Ok(v) => scratch.held = Some((which, c, v)),
                    Err(e) => {
                        self.fail(format!("property blob {which} chunk {c}: {e}"));
                        return Vec::new();
                    }
                }
            }
            if let Some((_, _, v)) = &scratch.held {
                out.extend_from_slice(v.get(from..to).unwrap_or(&[]));
            }
        }
        out
    }

    pub fn node_props(&self, s: u32) -> Vec<(u32, Value)> {
        let run = self.run(0, s);
        self.decode(codec::read_prop_ids(&run))
    }

    pub fn edge_props(&self, s: u32) -> Vec<(u32, Value)> {
        let run = self.run(1, s);
        self.decode(codec::read_prop_ids(&run))
    }

    pub fn node_prop(&self, s: u32, key: u32) -> Option<Value> {
        let run = self.run(0, s);
        self.decode(codec::find_prop(&run, key))
    }

    pub fn edge_prop(&self, s: u32, key: u32) -> Option<Value> {
        let run = self.run(1, s);
        self.decode(codec::find_prop(&run, key))
    }

    pub(crate) fn node_props_scratch(&self, s: u32, scratch: &mut ChunkScratch) -> Vec<(u32, Value)> {
        let run = self.run_scratch(0, s, scratch);
        self.decode(codec::read_prop_ids(&run))
    }

    pub(crate) fn edge_props_scratch(&self, s: u32, scratch: &mut ChunkScratch) -> Vec<(u32, Value)> {
        let run = self.run_scratch(1, s, scratch);
        self.decode(codec::read_prop_ids(&run))
    }

    fn decode<T: Default>(&self, r: Result<T, String>) -> T {
        r.unwrap_or_else(|e| {
            self.fail(format!("property run does not decode: {e}"));
            T::default()
        })
    }
}

// ------------------------------------------------------------------ opening

/// What `verify` reports about an image.
#[derive(Clone, Debug)]
pub struct ImageReport {
    pub bytes: u64,
    pub nodes: u64,
    pub edges: u64,
    pub strings: u64,
    pub indexes: u64,
}

/// Open an image in a file. Reads the header, directory and small tables
/// now; everything else on first use. `at` is the image's offset.
pub(crate) fn load_file(
    path: &Path,
    at: u64,
    len: u64,
    residency: Residency,
) -> io::Result<(Base, Vec<String>)> {
    let file = PosFile::open(path)?;
    let (mut base, strings) = open(&file, at, len, residency)?;
    if let Residency::OnDisk { cache_bytes } = residency {
        base.disk = Some(DiskCache {
            cap: cache_bytes.max(PROP_CHUNK as usize),
            lru: Mutex::new(Lru::default()),
        });
    }
    base.file = Some(file);
    Ok((base, strings))
}

/// Load an image that is already in memory, entirely and checked, since the
/// bytes are not kept.
pub(crate) fn load_slice(image: &[u8]) -> io::Result<(Base, Vec<String>)> {
    let src = Bytes(image);
    let (base, strings) = open(&src, 0, image.len() as u64, Residency::Memory)?;
    base.fill(Some(&src), true).map_err(bad)?;
    Ok((base, strings))
}

/// Check an image end to end — every section and every property chunk —
/// streaming, so memory stays at one section.
pub fn verify_file(path: &Path, at: u64, len: u64) -> io::Result<ImageReport> {
    let file = PosFile::open(path)?;
    let (base, strings) = open(&file, at, len, Residency::OnDisk { cache_bytes: 0 })?;
    let check = |c: &dyn Fn() -> Result<(), String>| c().map_err(bad);
    macro_rules! stream {
        ($($c:ident),*) => {$(
            check(&|| base.$c.read(&file, at, false).map(|_| ()))?;
        )*};
    }
    stream!(node_ids, node_label_off, node_labels, node_prop_off);
    stream!(out_off, out_nbr, out_edge, in_off, in_nbr, in_edge);
    stream!(edge_ids, edge_from, edge_to, edge_type, edge_prop_off, label_nodes);
    for i in 0..base.indexes.len() {
        base.index_body(i, Some(&file));
    }
    for p in [&base.node_props, &base.edge_props] {
        let crcs = p.crcs.read(&file, at, true).map_err(bad)?;
        for i in 0..p.chunks() {
            base.read_chunk(p, i, &file, &crcs)?;
        }
    }
    if let Some(e) = base.integrity_error() {
        return Err(bad(e.to_string()));
    }
    Ok(ImageReport {
        bytes: len,
        nodes: base.n() as u64,
        edges: base.m() as u64,
        strings: strings.len() as u64,
        indexes: base.indexes.len() as u64,
    })
}

fn open(
    src: &dyn Source,
    at: u64,
    len: u64,
    residency: Residency,
) -> io::Result<(Base, Vec<String>)> {
    if len < IMAGE_HEADER_LEN {
        return Err(bad("image is too short"));
    }
    let mut head = [0u8; IMAGE_HEADER_LEN as usize];
    src.read_at(&mut head, at)?;
    if &head[0..8] != IMAGE_MAGIC {
        return Err(bad("not a glider image (bad magic)"));
    }
    let u32_at = |o: usize| <u32 as Word>::read_le(&head[o..o + 4]);
    let u64_at = |o: usize| <u64 as Word>::read_le(&head[o..o + 8]);
    let version = u32_at(8);
    if version != 1 && version != IMAGE_VERSION {
        return Err(bad(format!("unsupported image version {version}")));
    }
    let nsec = u32_at(12) as u64;
    let n = u64_at(16);
    let m = u64_at(24);
    let next_node = u64_at(32);
    let next_edge = u64_at(40);
    let stored_crc = u32_at(60);
    if nsec > 1 << 20 || IMAGE_HEADER_LEN + nsec * DIR_ENTRY_LEN > len {
        return Err(bad("image directory out of range"));
    }
    if n >= u32::MAX as u64 || m >= u32::MAX as u64 {
        return Err(bad("image counts out of range"));
    }
    let mut dir = vec![0u8; (nsec * DIR_ENTRY_LEN) as usize];
    src.read_at(&mut dir, at + IMAGE_HEADER_LEN)?;
    let mut crc = Crc32::new();
    crc.update(&head[..60]);
    crc.update(&dir);
    if crc.finish() != stored_crc {
        return Err(bad("image header checksum mismatch"));
    }

    let mut sections: HashMap<u32, Section> = HashMap::new();
    for e in dir.chunks_exact(DIR_ENTRY_LEN as usize) {
        let kind = <u32 as Word>::read_le(&e[0..4]);
        let s = Section {
            off: <u64 as Word>::read_le(&e[8..16]),
            len: <u64 as Word>::read_le(&e[16..24]),
            crc: <u32 as Word>::read_le(&e[24..28]),
        };
        if s.off.checked_add(s.len).map(|end| end > len).unwrap_or(true) {
            return Err(bad(format!("image section {kind} out of range")));
        }
        sections.insert(kind, s);
    }
    let sec = |kind: u32| -> io::Result<Section> {
        sections
            .get(&kind)
            .copied()
            .ok_or_else(|| bad(format!("image is missing section {kind}")))
    };
    // Read a small section whole, checked.
    let small = |kind: u32| -> io::Result<Vec<u8>> {
        let s = sec(kind)?;
        let mut b = vec![0u8; s.len as usize];
        src.read_at(&mut b, at + s.off)?;
        if codec::crc32(&b) != s.crc {
            return Err(bad(format!("image section {kind}: checksum mismatch")));
        }
        Ok(b)
    };

    let n = n as usize;
    let m = m as usize;
    let strings = parse_strings(&small(S_STRINGS)?)?;
    let ns = strings.len() as u64;
    let (nu, mu) = (n as u64, m as u64);
    let words = |kind: u32, size: u64| -> io::Result<u64> { Ok(sec(kind)?.len / size) };

    let col32 = |kind: u32, count: u64, check: Check| -> io::Result<Col<u32>> {
        Ok(Col {
            kind,
            sec: sec(kind)?,
            count,
            check,
            cell: OnceLock::new(),
        })
    };
    let col64 = |kind: u32, count: u64, check: Check| -> io::Result<Col<u64>> {
        Ok(Col {
            kind,
            sec: sec(kind)?,
            count,
            check,
            cell: OnceLock::new(),
        })
    };

    let node_props_len = sec(S_NODE_PROPS)?.len;
    let edge_props_len = sec(S_EDGE_PROPS)?.len;
    let n_labels = words(S_NODE_LABELS, 4)?;
    let n_label_nodes = words(S_LABEL_NODES, 4)?;

    // The label offsets are small and are what label counts come from, so
    // they are read now.
    let label_off_col = col32(S_LABEL_OFF, ns + 1, Check::Offsets { total: n_label_nodes })?;
    let label_off = label_off_col.read(src, at, true).map_err(bad)?;

    let props = |kind: u32, crc_kind: u32| -> io::Result<Props> {
        let s = sec(kind)?;
        let chunks = s.len.div_ceil(PROP_CHUNK);
        Ok(Props {
            sec: s,
            crcs: col32(crc_kind, chunks, Check::None)?,
            mem: (residency == Residency::Memory)
                .then(|| (0..chunks).map(|_| OnceLock::new()).collect()),
        })
    };

    let mut base = Base {
        n,
        m,
        file: None,
        at,
        node_ids: col64(S_NODE_IDS, nu, Check::Ascending)?,
        node_slot: OnceLock::new(),
        node_label_off: col32(S_NODE_LABEL_OFF, nu + 1, Check::Offsets { total: n_labels })?,
        node_labels: col32(S_NODE_LABELS, n_labels, Check::Below(ns))?,
        node_prop_off: col64(S_NODE_PROP_OFF, nu + 1, Check::Offsets { total: node_props_len })?,
        out_off: col32(S_OUT_OFF, nu + 1, Check::Offsets { total: mu })?,
        out_nbr: col32(S_OUT_NBR, mu, Check::Below(nu))?,
        out_edge: col32(S_OUT_EDGE, mu, Check::Below(mu))?,
        in_off: col32(S_IN_OFF, nu + 1, Check::Offsets { total: mu })?,
        in_nbr: col32(S_IN_NBR, mu, Check::Below(nu))?,
        in_edge: col32(S_IN_EDGE, mu, Check::Below(mu))?,
        edge_ids: col64(S_EDGE_IDS, mu, Check::Ascending)?,
        edge_slot: OnceLock::new(),
        edge_from: col32(S_EDGE_FROM, mu, Check::Below(nu))?,
        edge_to: col32(S_EDGE_TO, mu, Check::Below(nu))?,
        edge_type: col32(S_EDGE_TYPE, mu, Check::Below(ns))?,
        edge_prop_off: col64(S_EDGE_PROP_OFF, mu + 1, Check::Offsets { total: edge_props_len })?,
        label_off,
        label_nodes: col32(S_LABEL_NODES, n_label_nodes, Check::Below(nu))?,
        type_counts: OnceLock::new(),
        indexes: Vec::new(),
        node_props: props(S_NODE_PROPS, S_NODE_PROPS_CRC)?,
        edge_props: props(S_EDGE_PROPS, S_EDGE_PROPS_CRC)?,
        disk: None,
        error: OnceLock::new(),
        read_errors: AtomicU64::new(0),
        next_node,
        next_edge,
        image_bytes: len,
    };

    if sections.contains_key(&S_TYPE_COUNTS) {
        let b = small(S_TYPE_COUNTS)?;
        let mut r = Reader::new(&b);
        let count = r.u32().map_err(bad)?;
        let mut tc = HashMap::new();
        let mut total = 0u64;
        for _ in 0..count {
            let t = r.u32().map_err(bad)?;
            let c = r.u32().map_err(bad)? as u64 | (r.u32().map_err(bad)? as u64) << 32;
            if t as u64 >= ns {
                return Err(bad("image: bad type counts"));
            }
            total += c;
            tc.insert(t, c as usize);
        }
        if total != mu {
            return Err(bad("image: type counts do not add up"));
        }
        let _ = base.type_counts.set(tc);
    }

    if version == 1 {
        base.indexes = parse_v1_indexes(&small(S_PROP_INDEX)?, ns as usize, n)?;
    } else {
        let b = small(S_INDEX_DIR)?;
        let mut r = Reader::new(&b);
        let count = r.u32().map_err(bad)?;
        for i in 0..count {
            let label = r.u32().map_err(bad)?;
            let key = r.u32().map_err(bad)?;
            let nkeys = r.u32().map_err(bad)? as u64;
            let npost = r.u32().map_err(bad)? as u64;
            let keys_len = r.u32().map_err(bad)? as u64;
            let kind = S_INDEX_BODY + i;
            let body_sec = sec(kind)?;
            if label as u64 >= ns
                || key as u64 >= ns
                || npost > nu
                || body_sec.len != 8 * (nkeys + 1) + keys_len + 4 * npost
            {
                return Err(bad("image: bad index directory"));
            }
            base.indexes.push(BaseIndex {
                label,
                key,
                nkeys,
                npost,
                keys_len,
                body_sec,
                kind,
                body: OnceLock::new(),
            });
        }
    }
    Ok((base, strings))
}

fn parse_strings(b: &[u8]) -> io::Result<Vec<String>> {
    let mut r = Reader::new(b);
    let count = r.u32().map_err(bad)? as usize;
    if count > b.len() {
        return Err(bad("string count out of range"));
    }
    let mut offs = Vec::with_capacity(count + 1);
    for _ in 0..=count {
        offs.push(r.u32().map_err(bad)? as usize);
    }
    let body = &b[r.pos..];
    let mut out = Vec::with_capacity(count);
    for w in offs.windows(2) {
        if w[0] > w[1] || w[1] > body.len() {
            return Err(bad("string offsets out of range"));
        }
        let s = std::str::from_utf8(&body[w[0]..w[1]]).map_err(|_| bad("string is not utf-8"))?;
        out.push(s.to_string());
    }
    Ok(out)
}

/// Version-1 images: every index in one section, each with its own header.
fn parse_v1_indexes(b: &[u8], ns: usize, n: usize) -> io::Result<Vec<BaseIndex>> {
    let mut r = Reader::new(b);
    let count = r.u32().map_err(bad)?;
    let mut out = Vec::new();
    for _ in 0..count {
        let label = r.u32().map_err(bad)?;
        let key = r.u32().map_err(bad)?;
        let nkeys = r.u32().map_err(bad)? as usize;
        let npost = r.u32().map_err(bad)? as usize;
        let keys_len = r.u32().map_err(bad)? as usize;
        if label as usize >= ns || key as usize >= ns || nkeys > b.len() || npost > n {
            return Err(bad("index header out of range"));
        }
        let body = IndexBody::parse(&mut r, nkeys, npost, keys_len, n).map_err(bad)?;
        out.push(BaseIndex {
            label,
            key,
            nkeys: nkeys as u64,
            npost: npost as u64,
            keys_len: keys_len as u64,
            body_sec: Section::default(),
            kind: S_PROP_INDEX,
            body: OnceLock::from(body),
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------ writing

/// A graph to write as an image, visited in passes. Implement this to
/// build a database file straight from your own data with [`build`],
/// without holding a graph in memory: the builder keeps only a few integers
/// per node and edge, and streams property values straight to disk.
///
/// String ids are indexes into [`strings`](ImageSource::strings). Labels,
/// edge types and property keys all refer to it.
pub trait ImageSource {
    /// Every string a label, edge type or property key refers to, in id
    /// order.
    fn strings(&self) -> Vec<String>;
    /// The ids the graph will hand out next: `(next_node, next_edge)`.
    /// Must exceed every id visited.
    fn next_ids(&self) -> (u64, u64);
    /// Property indexes to build, as `(label, key)` string ids.
    fn indexes(&self) -> Vec<(u32, u32)>;
    /// Visit every node in ascending id order: id, labels, properties.
    fn nodes(&self, f: &mut dyn FnMut(u64, &[u32], &[(u32, Value)])) -> io::Result<()>;
    /// Visit every edge in ascending id order: id, from, to, type,
    /// properties. Endpoints must be nodes that `nodes` visits.
    fn edges(&self, f: &mut dyn FnMut(u64, u64, u64, u32, &[(u32, Value)])) -> io::Result<()>;
}

/// Streams sections out, recording where each landed.
struct SectionWriter<'w, W: Write + Seek> {
    w: &'w mut W,
    pos: u64,
    dir: Vec<(u32, Section)>,
    cur: Option<(u32, u64, Crc32)>,
    /// Per-`PROP_CHUNK` crcs of the current section, when asked for.
    chunks: Option<(Vec<u32>, Crc32, u64)>,
}

impl<W: Write + Seek> SectionWriter<'_, W> {
    fn begin(&mut self, kind: u32, chunked: bool) {
        self.cur = Some((kind, self.pos, Crc32::new()));
        self.chunks = chunked.then(|| (Vec::new(), Crc32::new(), 0));
    }

    fn put(&mut self, mut data: &[u8]) -> io::Result<()> {
        self.w.write_all(data)?;
        self.pos += data.len() as u64;
        if let Some((_, _, crc)) = &mut self.cur {
            crc.update(data);
        }
        if let Some((list, crc, fill)) = &mut self.chunks {
            while !data.is_empty() {
                let k = ((PROP_CHUNK - *fill) as usize).min(data.len());
                crc.update(&data[..k]);
                *fill += k as u64;
                data = &data[k..];
                if *fill == PROP_CHUNK {
                    list.push(std::mem::take(crc).finish());
                    *crc = Crc32::new();
                    *fill = 0;
                }
            }
        }
        Ok(())
    }

    /// Close the current section. Returns the chunk crcs if the section was
    /// chunked. Sections are packed with no padding between them, so every
    /// byte of an image is covered by some checksum.
    fn end(&mut self) -> io::Result<Vec<u32>> {
        let (kind, off, crc) = self.cur.take().expect("section open");
        let len = self.pos - off;
        self.dir.push((
            kind,
            Section {
                off,
                len,
                crc: crc.finish(),
            },
        ));
        Ok(match self.chunks.take() {
            Some((mut list, crc, fill)) => {
                if fill > 0 {
                    list.push(crc.finish());
                }
                list
            }
            None => Vec::new(),
        })
    }

    fn u32s(&mut self, kind: u32, v: &[u32]) -> io::Result<()> {
        self.begin(kind, false);
        let mut buf = Vec::with_capacity(READ_CHUNK.min(v.len() * 4));
        for chunk in v.chunks(READ_CHUNK / 4) {
            buf.clear();
            for x in chunk {
                buf.extend_from_slice(&x.to_le_bytes());
            }
            self.put(&buf)?;
        }
        self.end().map(|_| ())
    }

    fn u64s(&mut self, kind: u32, v: &[u64]) -> io::Result<()> {
        self.begin(kind, false);
        let mut buf = Vec::with_capacity(READ_CHUNK.min(v.len() * 8));
        for chunk in v.chunks(READ_CHUNK / 8) {
            buf.clear();
            for x in chunk {
                buf.extend_from_slice(&x.to_le_bytes());
            }
            self.put(&buf)?;
        }
        self.end().map(|_| ())
    }

    fn bytes(&mut self, kind: u32, v: &[u8]) -> io::Result<()> {
        self.begin(kind, false);
        self.put(v)?;
        self.end().map(|_| ())
    }
}

fn too_big(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("graph is too large for an image: {what} exceeds 2^32"),
    )
}

/// Write `src` as an image at the writer's position. Returns the image
/// length; the writer is left positioned at its end.
///
/// Memory: a few integers per node (ids, label and property offsets,
/// degrees), 24 bytes per edge (endpoints and both adjacency directions),
/// and the values of indexed properties. Property runs stream straight out.
pub fn write<W: Write + Seek>(src: &dyn ImageSource, w: &mut W) -> io::Result<u64> {
    let start = w.stream_position()?;
    let index_defs = src.indexes();
    let nsec = FIXED_SECTIONS as u64 + index_defs.len() as u64;
    let dir_len = nsec * DIR_ENTRY_LEN;
    w.write_all(&vec![0u8; (IMAGE_HEADER_LEN + dir_len) as usize])?;
    let mut sw = SectionWriter {
        w,
        pos: IMAGE_HEADER_LEN + dir_len,
        dir: Vec::new(),
        cur: None,
        chunks: None,
    };

    // Strings, verbatim and in id order, so interned ids mean the same thing
    // before and after.
    let strings = src.strings();
    let ns = strings.len();
    {
        let mut b = Vec::new();
        codec::put_u32(&mut b, ns as u32);
        let mut off = 0u64;
        codec::put_u32(&mut b, 0);
        for s in &strings {
            off += s.len() as u64;
            if off > u32::MAX as u64 {
                return Err(too_big("string table"));
            }
            codec::put_u32(&mut b, off as u32);
        }
        for s in &strings {
            b.extend_from_slice(s.as_bytes());
        }
        sw.bytes(S_STRINGS, &b)?;
    }
    let check_str = |id: u32, what: &str| {
        if (id as usize) < ns {
            Ok(())
        } else {
            Err(bad(format!("{what} refers to string {id}, which does not exist")))
        }
    };

    // Nodes: ids and labels into columns, properties streamed into their
    // blob, indexed values collected for sorting.
    let mut node_ids: Vec<u64> = Vec::new();
    let mut label_off = vec![0u32];
    let mut labels: Vec<u32> = Vec::new();
    let mut prop_off = vec![0u64];
    let mut idx_vals: Vec<Vec<(VKey, u32)>> = vec![Vec::new(); index_defs.len()];
    let mut run = Vec::new();
    let mut blob_len = 0u64;
    let mut err: Option<io::Error> = None;
    sw.begin(S_NODE_PROPS, true);
    src.nodes(&mut |id, ls, props| {
        if err.is_some() {
            return;
        }
        let r = (|| -> io::Result<()> {
            if node_ids.last().is_some_and(|last| *last >= id) {
                return Err(bad("nodes must be visited in ascending id order"));
            }
            let slot = node_ids.len() as u32;
            node_ids.push(id);
            for l in ls {
                check_str(*l, "a label")?;
            }
            labels.extend_from_slice(ls);
            if labels.len() > u32::MAX as usize {
                return Err(too_big("label list"));
            }
            label_off.push(labels.len() as u32);
            for (k, _) in props {
                check_str(*k, "a property key")?;
            }
            for (i, (l, k)) in index_defs.iter().enumerate() {
                if ls.contains(l) {
                    if let Some((_, v)) = props.iter().find(|(pk, _)| pk == k) {
                        idx_vals[i].push((VKey(v.clone()), slot));
                    }
                }
            }
            run.clear();
            codec::put_prop_ids(&mut run, props);
            sw.put(&run)?;
            blob_len += run.len() as u64;
            prop_off.push(blob_len);
            Ok(())
        })();
        err = r.err();
    })?;
    if let Some(e) = err.take() {
        return Err(e);
    }
    let node_crcs = sw.end()?;
    let n = node_ids.len();
    if n >= u32::MAX as usize {
        return Err(too_big("node count"));
    }
    sw.u32s(S_NODE_PROPS_CRC, &node_crcs)?;
    sw.u64s(S_NODE_IDS, &node_ids)?;
    sw.u32s(S_NODE_LABEL_OFF, &label_off)?;
    sw.u32s(S_NODE_LABELS, &labels)?;
    sw.u64s(S_NODE_PROP_OFF, &prop_off)?;
    drop(prop_off);

    // Label postings, from the label column in one pass. Slots are visited
    // in order, so every posting list comes out sorted.
    {
        let mut off = vec![0u32; ns + 1];
        for l in &labels {
            off[*l as usize + 1] += 1;
        }
        for i in 0..ns {
            off[i + 1] += off[i];
        }
        let mut cursor = off.clone();
        let mut post = vec![0u32; labels.len()];
        for s in 0..n {
            for l in &labels[label_off[s] as usize..label_off[s + 1] as usize] {
                post[cursor[*l as usize] as usize] = s as u32;
                cursor[*l as usize] += 1;
            }
        }
        sw.u32s(S_LABEL_OFF, &off)?;
        sw.u32s(S_LABEL_NODES, &post)?;
    }
    drop(labels);
    drop(label_off);

    // Edges: streamed columns, endpoints kept (as slots) for adjacency.
    let nslot = SlotIndex::build(&node_ids);
    let mut edge_ids: Vec<u64> = Vec::new();
    let mut from: Vec<u32> = Vec::new();
    let mut to: Vec<u32> = Vec::new();
    let mut etype: Vec<u32> = Vec::new();
    let mut prop_off = vec![0u64];
    let mut type_counts: BTreeMap<u32, u64> = BTreeMap::new();
    blob_len = 0;
    sw.begin(S_EDGE_PROPS, true);
    src.edges(&mut |id, f, t, ty, props| {
        if err.is_some() {
            return;
        }
        let r = (|| -> io::Result<()> {
            if edge_ids.last().is_some_and(|last| *last >= id) {
                return Err(bad("edges must be visited in ascending id order"));
            }
            let slot = |x: u64| {
                nslot
                    .get(&node_ids, x)
                    .ok_or_else(|| bad(format!("edge {id} refers to missing node {x}")))
            };
            from.push(slot(f)?);
            to.push(slot(t)?);
            check_str(ty, "an edge type")?;
            etype.push(ty);
            *type_counts.entry(ty).or_default() += 1;
            edge_ids.push(id);
            for (k, _) in props {
                check_str(*k, "a property key")?;
            }
            run.clear();
            codec::put_prop_ids(&mut run, props);
            sw.put(&run)?;
            blob_len += run.len() as u64;
            prop_off.push(blob_len);
            Ok(())
        })();
        err = r.err();
    })?;
    if let Some(e) = err.take() {
        return Err(e);
    }
    let edge_crcs = sw.end()?;
    let m = edge_ids.len();
    if m >= u32::MAX as usize {
        return Err(too_big("edge count"));
    }
    sw.u32s(S_EDGE_PROPS_CRC, &edge_crcs)?;
    sw.u64s(S_EDGE_IDS, &edge_ids)?;
    sw.u64s(S_EDGE_PROP_OFF, &prop_off)?;
    drop(prop_off);
    drop(edge_ids);
    sw.u32s(S_EDGE_TYPE, &etype)?;
    drop(etype);
    {
        let mut b = Vec::new();
        codec::put_u32(&mut b, type_counts.len() as u32);
        for (t, c) in &type_counts {
            codec::put_u32(&mut b, *t);
            codec::put_u64(&mut b, *c);
        }
        sw.bytes(S_TYPE_COUNTS, &b)?;
    }

    // Adjacency by counting sort over edge slots, which are visited in
    // ascending id order, so each node's run comes out in ascending edge id.
    for (out, s_off, s_nbr, s_edge) in [
        (true, S_OUT_OFF, S_OUT_NBR, S_OUT_EDGE),
        (false, S_IN_OFF, S_IN_NBR, S_IN_EDGE),
    ] {
        let (key, other) = if out { (&from, &to) } else { (&to, &from) };
        let mut off = vec![0u32; n + 1];
        for k in key.iter() {
            off[*k as usize + 1] += 1;
        }
        for i in 0..n {
            off[i + 1] += off[i];
        }
        let mut cursor = off.clone();
        let mut nbr = vec![0u32; m];
        let mut edge = vec![0u32; m];
        for e in 0..m {
            let k = key[e] as usize;
            let at = cursor[k] as usize;
            nbr[at] = other[e];
            edge[at] = e as u32;
            cursor[k] += 1;
        }
        sw.u32s(s_off, &off)?;
        sw.u32s(s_nbr, &nbr)?;
        sw.u32s(s_edge, &edge)?;
    }
    sw.u32s(S_EDGE_FROM, &from)?;
    sw.u32s(S_EDGE_TO, &to)?;
    drop((from, to));

    // Property indexes: keys sorted by the same total order the in-memory
    // BTreeMap uses; a stable sort keeps each bucket's slots ascending.
    {
        let mut dir = Vec::new();
        codec::put_u32(&mut dir, index_defs.len() as u32);
        for (i, ((l, k), mut vals)) in index_defs.iter().zip(idx_vals).enumerate() {
            vals.sort_by(|a, b| a.0.cmp(&b.0));
            let mut keys = Vec::new();
            let mut key_off = vec![0u32];
            let mut post_off = vec![0u32];
            let mut post: Vec<u32> = Vec::with_capacity(vals.len());
            let mut j = 0;
            while j < vals.len() {
                codec::put_value(&mut keys, &vals[j].0 .0);
                if keys.len() > u32::MAX as usize {
                    return Err(too_big("index keys"));
                }
                key_off.push(keys.len() as u32);
                let mut e = j;
                while e < vals.len() && vals[e].0 == vals[j].0 {
                    post.push(vals[e].1);
                    e += 1;
                }
                post_off.push(post.len() as u32);
                j = e;
            }
            codec::put_u32(&mut dir, *l);
            codec::put_u32(&mut dir, *k);
            codec::put_u32(&mut dir, (key_off.len() - 1) as u32);
            codec::put_u32(&mut dir, post.len() as u32);
            codec::put_u32(&mut dir, keys.len() as u32);
            let mut body = Vec::with_capacity(8 * key_off.len() + keys.len() + 4 * post.len());
            for x in &key_off {
                codec::put_u32(&mut body, *x);
            }
            body.extend_from_slice(&keys);
            for x in post_off.iter().chain(post.iter()) {
                codec::put_u32(&mut body, *x);
            }
            sw.bytes(S_INDEX_BODY + i as u32, &body)?;
        }
        sw.bytes(S_INDEX_DIR, &dir)?;
    }

    // Header and directory, now that every section's place is known.
    let end = sw.pos;
    debug_assert_eq!(sw.dir.len() as u64, nsec);
    let mut dir = Vec::with_capacity(dir_len as usize);
    for (kind, s) in &sw.dir {
        codec::put_u32(&mut dir, *kind);
        codec::put_u32(&mut dir, 0);
        codec::put_u64(&mut dir, s.off);
        codec::put_u64(&mut dir, s.len);
        codec::put_u32(&mut dir, s.crc);
        codec::put_u32(&mut dir, 0);
    }
    let mut head = Vec::with_capacity(IMAGE_HEADER_LEN as usize);
    head.extend_from_slice(IMAGE_MAGIC);
    codec::put_u32(&mut head, IMAGE_VERSION);
    codec::put_u32(&mut head, sw.dir.len() as u32);
    codec::put_u64(&mut head, n as u64);
    codec::put_u64(&mut head, m as u64);
    let (next_node, next_edge) = src.next_ids();
    let max_node = node_ids.last().copied().unwrap_or(0);
    codec::put_u64(&mut head, next_node.max(max_node + 1));
    codec::put_u64(&mut head, next_edge.max(1));
    head.resize(60, 0);
    let mut crc = Crc32::new();
    crc.update(&head);
    crc.update(&dir);
    codec::put_u32(&mut head, crc.finish());

    let w = sw.w;
    w.seek(SeekFrom::Start(start))?;
    w.write_all(&head)?;
    w.write_all(&dir)?;
    w.seek(SeekFrom::Start(start + end))?;
    Ok(end)
}

/// Build a complete database file at `path` (which must not exist) from
/// `src`: header, image, empty log. The result opens with `Graph::open`
/// like any compacted database. This is the way to make a database larger
/// than RAM would allow building through the graph API.
pub fn build(path: &Path, src: &dyn ImageSource) -> io::Result<u64> {
    crate::store::write_image_file(path, |w| write(src, w).map(|_| ()))
}
