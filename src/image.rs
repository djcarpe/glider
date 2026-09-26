//! The snapshot image: current graph state as flat, offset-addressed columns.
//!
//! Replaying a log means building the graph one mutation at a time — a hash
//! insert and a couple of small allocations per record — and that construction
//! was 93% of open time. An image is instead a handful of arrays written as
//! they sit in memory, so opening one is a bulk read, a checksum and a
//! little-endian decode: no per-node allocation and no hashing.
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
//! PROP_INDEX            property indexes: sorted encoded keys, slot postings
//! NODE_PROPS/EDGE_PROPS property runs, the bulky cold part
//! *_PROPS_CRC           one crc32 per 64 KiB chunk of each props blob
//! ```
//!
//! The image is immutable. Changes made after it was written live in the
//! graph's delta overlay (see `graph.rs`) until the next compaction folds them
//! in.
//!
//! Property runs are the only part that may stay on disk
//! ([`Residency::OnDisk`]): they are read on demand with positional reads,
//! through a small chunk cache, and each chunk is checksummed as it arrives.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::codec::{self, Crc32, Reader};
use crate::graph::{Dir, Graph, VKey};
use crate::pread::PosFile;
use crate::value::Value;

pub const IMAGE_MAGIC: &[u8; 8] = b"GLIMG\x00\x00\x01";
pub const IMAGE_VERSION: u32 = 1;
const IMAGE_HEADER_LEN: u64 = 64;
const DIR_ENTRY_LEN: u64 = 32;
/// Granularity of property checksums, and of the on-disk property cache.
pub const PROP_CHUNK: u64 = 64 * 1024;
/// Hot sections are read in pieces this big, so a load never holds a second
/// full copy of a section in raw bytes alongside its decoded form.
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
const S_PROP_INDEX: u32 = 19;
const S_NODE_PROPS: u32 = 20;
const S_EDGE_PROPS: u32 = 21;
const S_NODE_PROPS_CRC: u32 = 22;
const S_EDGE_PROPS_CRC: u32 = 23;
const SECTION_COUNT: u32 = 23;

/// Where property runs live once an image is loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Residency {
    /// Read into memory at open, as raw bytes; decoded per lookup. RAM is
    /// roughly the size of the image. The default.
    #[default]
    Memory,
    /// Left on disk and read on demand through a cache of at most
    /// `cache_bytes`. Topology, labels and indexes are still in memory, so
    /// traversal stays fast; property-heavy scans pay for disk reads. For
    /// devices where RAM is the constraint.
    OnDisk { cache_bytes: usize },
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
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
                v[*id as usize] = slot as u32;
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

enum Blob {
    Mem(Vec<u8>),
    /// Absolute file offset and length, plus one crc per `PROP_CHUNK`.
    Disk {
        at: u64,
        len: u64,
        crcs: Vec<u32>,
    },
}

impl Blob {
    fn len(&self) -> u64 {
        match self {
            Blob::Mem(v) => v.len() as u64,
            Blob::Disk { len, .. } => *len,
        }
    }
}

struct DiskCache {
    file: PosFile,
    cap: usize,
    lru: Mutex<Lru>,
}

/// (which blob, chunk index)
type ChunkKey = (u8, u64);

#[derive(Default)]
struct Lru {
    /// chunk -> (bytes, last use)
    map: HashMap<ChunkKey, (Arc<Vec<u8>>, u64)>,
    bytes: usize,
    tick: u64,
}

// ------------------------------------------------------------------- base

pub(crate) struct BaseIndex {
    pub label: u32,
    pub key: u32,
    key_off: Vec<u64>,
    keys: Vec<u8>,
    post_off: Vec<u32>,
    post: Vec<u32>,
}

impl BaseIndex {
    pub fn key_count(&self) -> usize {
        self.key_off.len().saturating_sub(1)
    }

    fn key(&self, i: usize) -> Value {
        let run = &self.keys[self.key_off[i] as usize..self.key_off[i + 1] as usize];
        // Every key was checked to decode when the image was loaded.
        Reader::new(run).value().unwrap_or(Value::Null)
    }

    fn postings(&self, i: usize) -> &[u32] {
        &self.post[self.post_off[i] as usize..self.post_off[i + 1] as usize]
    }

    /// Node slots whose value equals `v`, ascending.
    pub fn lookup(&self, v: &Value) -> &[u32] {
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

    pub fn buckets(&self) -> impl Iterator<Item = &[u32]> + '_ {
        (0..self.key_count()).map(move |i| self.postings(i))
    }
}

/// The immutable part of a graph: everything the last compaction wrote.
pub(crate) struct Base {
    node_ids: Vec<u64>,
    node_slot: SlotIndex,
    node_label_off: Vec<u32>,
    node_labels: Vec<u32>,
    node_prop_off: Vec<u64>,
    out_off: Vec<u32>,
    out_nbr: Vec<u32>,
    out_edge: Vec<u32>,
    in_off: Vec<u32>,
    in_nbr: Vec<u32>,
    in_edge: Vec<u32>,
    edge_ids: Vec<u64>,
    edge_slot: SlotIndex,
    edge_from: Vec<u32>,
    edge_to: Vec<u32>,
    edge_type: Vec<u32>,
    edge_prop_off: Vec<u64>,
    label_off: Vec<u32>,
    label_nodes: Vec<u32>,
    type_counts: HashMap<u32, usize>,
    pub indexes: Vec<BaseIndex>,
    node_props: Blob,
    edge_props: Blob,
    disk: Option<DiskCache>,
    read_errors: AtomicU64,
    pub next_node: u64,
    pub next_edge: u64,
    pub image_bytes: u64,
}

impl Base {
    pub fn empty() -> Base {
        Base {
            node_ids: Vec::new(),
            node_slot: SlotIndex::Sorted,
            node_label_off: vec![0],
            node_labels: Vec::new(),
            node_prop_off: vec![0],
            out_off: vec![0],
            out_nbr: Vec::new(),
            out_edge: Vec::new(),
            in_off: vec![0],
            in_nbr: Vec::new(),
            in_edge: Vec::new(),
            edge_ids: Vec::new(),
            edge_slot: SlotIndex::Sorted,
            edge_from: Vec::new(),
            edge_to: Vec::new(),
            edge_type: Vec::new(),
            edge_prop_off: vec![0],
            label_off: vec![0],
            label_nodes: Vec::new(),
            type_counts: HashMap::new(),
            indexes: Vec::new(),
            node_props: Blob::Mem(Vec::new()),
            edge_props: Blob::Mem(Vec::new()),
            disk: None,
            read_errors: AtomicU64::new(0),
            next_node: 1,
            next_edge: 1,
            image_bytes: 0,
        }
    }

    #[inline]
    pub fn n(&self) -> usize {
        self.node_ids.len()
    }
    #[inline]
    pub fn m(&self) -> usize {
        self.edge_ids.len()
    }
    #[inline]
    pub fn node_slot(&self, id: u64) -> Option<u32> {
        if self.node_ids.is_empty() {
            return None;
        }
        self.node_slot.get(&self.node_ids, id)
    }
    #[inline]
    pub fn edge_slot(&self, id: u64) -> Option<u32> {
        if self.edge_ids.is_empty() {
            return None;
        }
        self.edge_slot.get(&self.edge_ids, id)
    }
    #[inline]
    pub fn node_id(&self, s: u32) -> u64 {
        self.node_ids[s as usize]
    }
    #[inline]
    pub fn edge_id(&self, s: u32) -> u64 {
        self.edge_ids[s as usize]
    }
    pub fn node_ids(&self) -> &[u64] {
        &self.node_ids
    }
    pub fn edge_ids(&self) -> &[u64] {
        &self.edge_ids
    }
    #[inline]
    pub fn node_labels(&self, s: u32) -> &[u32] {
        let s = s as usize;
        &self.node_labels[self.node_label_off[s] as usize..self.node_label_off[s + 1] as usize]
    }
    #[inline]
    pub fn edge_from(&self, s: u32) -> u64 {
        self.node_ids[self.edge_from[s as usize] as usize]
    }
    #[inline]
    pub fn edge_to(&self, s: u32) -> u64 {
        self.node_ids[self.edge_to[s as usize] as usize]
    }
    #[inline]
    pub fn edge_type(&self, s: u32) -> u32 {
        self.edge_type[s as usize]
    }
    pub fn edge_types(&self) -> &[u32] {
        &self.edge_type
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
        let r = off[s as usize] as usize..off[s as usize + 1] as usize;
        nbr[r.clone()].iter().copied().zip(edge[r].iter().copied())
    }

    #[inline]
    pub fn degree(&self, s: u32, out: bool) -> usize {
        let off = if out { &self.out_off } else { &self.in_off };
        (off[s as usize + 1] - off[s as usize]) as usize
    }

    /// Node slots carrying a label, ascending.
    pub fn label_members(&self, l: u32) -> &[u32] {
        let l = l as usize;
        if l + 1 >= self.label_off.len() {
            return &[];
        }
        &self.label_nodes[self.label_off[l] as usize..self.label_off[l + 1] as usize]
    }

    /// Labels with at least one member.
    pub fn labels(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.label_off.len().saturating_sub(1) as u32)
            .filter(move |l| !self.label_members(*l).is_empty())
    }

    pub fn type_counts(&self) -> &HashMap<u32, usize> {
        &self.type_counts
    }

    pub fn props_on_disk(&self) -> bool {
        self.disk.is_some()
    }

    pub fn read_errors(&self) -> u64 {
        self.read_errors.load(Ordering::Relaxed)
    }

    fn node_run(&self, s: u32) -> Cow<'_, [u8]> {
        let s = s as usize;
        let (a, b) = (self.node_prop_off[s], self.node_prop_off[s + 1]);
        self.run(0, &self.node_props, a, b)
    }

    fn edge_run(&self, s: u32) -> Cow<'_, [u8]> {
        let s = s as usize;
        let (a, b) = (self.edge_prop_off[s], self.edge_prop_off[s + 1]);
        self.run(1, &self.edge_props, a, b)
    }

    pub fn node_props(&self, s: u32) -> Vec<(u32, Value)> {
        let run = self.node_run(s);
        self.decode(codec::read_prop_ids(&run))
    }

    pub fn edge_props(&self, s: u32) -> Vec<(u32, Value)> {
        let run = self.edge_run(s);
        self.decode(codec::read_prop_ids(&run))
    }

    pub fn node_prop(&self, s: u32, key: u32) -> Option<Value> {
        let run = self.node_run(s);
        self.decode(codec::find_prop(&run, key))
    }

    pub fn edge_prop(&self, s: u32, key: u32) -> Option<Value> {
        let run = self.edge_run(s);
        self.decode(codec::find_prop(&run, key))
    }

    /// Accessors are infallible — a traversal cannot stop to handle an I/O
    /// error — so a run that fails to read or decode reads as empty and is
    /// counted. `stats` reports the count and `verify` finds the cause.
    fn decode<T: Default>(&self, r: Result<T, String>) -> T {
        r.unwrap_or_else(|_| {
            self.read_errors.fetch_add(1, Ordering::Relaxed);
            T::default()
        })
    }

    fn run<'a>(&'a self, which: u8, blob: &'a Blob, a: u64, b: u64) -> Cow<'a, [u8]> {
        match blob {
            Blob::Mem(v) => Cow::Borrowed(&v[a as usize..b as usize]),
            Blob::Disk { at, len, crcs } => {
                let Some(cache) = &self.disk else {
                    return Cow::Borrowed(&[]);
                };
                let mut out = Vec::with_capacity((b - a) as usize);
                let mut pos = a;
                while pos < b {
                    let chunk = pos / PROP_CHUNK;
                    let bytes = match cache.chunk(which, chunk, *at, *len, crcs) {
                        Ok(c) => c,
                        Err(_) => {
                            self.read_errors.fetch_add(1, Ordering::Relaxed);
                            return Cow::Owned(Vec::new());
                        }
                    };
                    let from = (pos - chunk * PROP_CHUNK) as usize;
                    let to = ((b - chunk * PROP_CHUNK) as usize).min(bytes.len());
                    out.extend_from_slice(&bytes[from..to]);
                    pos = chunk * PROP_CHUNK + to as u64;
                }
                Cow::Owned(out)
            }
        }
    }
}

impl DiskCache {
    fn chunk(
        &self,
        which: u8,
        idx: u64,
        at: u64,
        len: u64,
        crcs: &[u32],
    ) -> io::Result<Arc<Vec<u8>>> {
        {
            let mut lru = self.lru.lock().unwrap_or_else(|e| e.into_inner());
            lru.tick += 1;
            let tick = lru.tick;
            if let Some((bytes, used)) = lru.map.get_mut(&(which, idx)) {
                *used = tick;
                return Ok(bytes.clone());
            }
        }
        // Read outside the lock so a slow disk does not serialise readers.
        let start = idx * PROP_CHUNK;
        let n = (len - start).min(PROP_CHUNK) as usize;
        let mut buf = vec![0u8; n];
        self.file.read_exact_at(&mut buf, at + start)?;
        if crcs.get(idx as usize).copied() != Some(codec::crc32(&buf)) {
            return Err(bad("property chunk checksum mismatch"));
        }
        let bytes = Arc::new(buf);
        let mut lru = self.lru.lock().unwrap_or_else(|e| e.into_inner());
        lru.tick += 1;
        let tick = lru.tick;
        lru.bytes += n;
        if let Some((old, _)) = lru.map.insert((which, idx), (bytes.clone(), tick)) {
            lru.bytes -= old.len();
        }
        while lru.bytes > self.cap && lru.map.len() > 1 {
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
}

// ------------------------------------------------------------------ loading

/// Something to read image bytes out of: a file, or a slice already in memory.
trait Source {
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()>;
}

impl Source for PosFile {
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        self.read_exact_at(buf, off)
    }
}

impl Source for [u8] {
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        let start = usize::try_from(off).map_err(|_| bad("offset out of range"))?;
        let end = start
            .checked_add(buf.len())
            .filter(|e| *e <= self.len())
            .ok_or_else(|| bad("image is truncated"))?;
        buf.copy_from_slice(&self[start..end]);
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct Section {
    off: u64,
    len: u64,
    crc: u32,
}

/// What `verify` reports about an image.
#[derive(Clone, Debug)]
pub struct ImageReport {
    pub bytes: u64,
    pub nodes: u64,
    pub edges: u64,
    pub strings: u64,
    pub indexes: u64,
}

/// Load an image from a file. `at` is the image's offset in the file.
pub(crate) fn load_file(
    path: &Path,
    at: u64,
    len: u64,
    residency: Residency,
) -> io::Result<(Base, Vec<String>)> {
    let file = PosFile::open(path)?;
    let (mut base, strings) = load(&file, at, len, residency)?;
    if let Residency::OnDisk { cache_bytes } = residency {
        base.disk = Some(DiskCache {
            file,
            cap: cache_bytes.max(PROP_CHUNK as usize),
            lru: Mutex::new(Lru::default()),
        });
    }
    Ok((base, strings))
}

/// Load an image that is already in memory. Always `Residency::Memory`.
pub(crate) fn load_slice(image: &[u8]) -> io::Result<(Base, Vec<String>)> {
    load(image, 0, image.len() as u64, Residency::Memory)
}

/// Check an image end to end — every section and every property chunk —
/// without keeping it.
pub fn verify_file(path: &Path, at: u64, len: u64) -> io::Result<ImageReport> {
    let (base, strings) = load_file(path, at, len, Residency::Memory)?;
    Ok(ImageReport {
        bytes: len,
        nodes: base.n() as u64,
        edges: base.m() as u64,
        strings: strings.len() as u64,
        indexes: base.indexes.len() as u64,
    })
}

fn load<S: Source + ?Sized>(
    src: &S,
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
    let u32_at = |o: usize| u32::from_le_bytes(head[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(head[o..o + 8].try_into().unwrap());
    let version = u32_at(8);
    if version != IMAGE_VERSION {
        return Err(bad(format!("unsupported image version {version}")));
    }
    let nsec = u32_at(12) as u64;
    let n = u64_at(16);
    let m = u64_at(24);
    let next_node = u64_at(32);
    let next_edge = u64_at(40);
    let stored_crc = u32_at(60);
    if nsec > 1024 || IMAGE_HEADER_LEN + nsec * DIR_ENTRY_LEN > len {
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
        let kind = u32::from_le_bytes(e[0..4].try_into().unwrap());
        let s = Section {
            off: u64::from_le_bytes(e[8..16].try_into().unwrap()),
            len: u64::from_le_bytes(e[16..24].try_into().unwrap()),
            crc: u32::from_le_bytes(e[24..28].try_into().unwrap()),
        };
        if s.off
            .checked_add(s.len)
            .map(|end| end > len)
            .unwrap_or(true)
        {
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

    let n = n as usize;
    let m = m as usize;
    let rd = Loader { src, at };

    let strings = parse_strings(&rd.bytes(sec(S_STRINGS)?)?)?;
    let ns = strings.len();

    let node_ids = rd.u64s(sec(S_NODE_IDS)?)?;
    let node_label_off = rd.u32s(sec(S_NODE_LABEL_OFF)?)?;
    let node_labels = rd.u32s(sec(S_NODE_LABELS)?)?;
    let node_prop_off = rd.u64s(sec(S_NODE_PROP_OFF)?)?;
    let out_off = rd.u32s(sec(S_OUT_OFF)?)?;
    let out_nbr = rd.u32s(sec(S_OUT_NBR)?)?;
    let out_edge = rd.u32s(sec(S_OUT_EDGE)?)?;
    let in_off = rd.u32s(sec(S_IN_OFF)?)?;
    let in_nbr = rd.u32s(sec(S_IN_NBR)?)?;
    let in_edge = rd.u32s(sec(S_IN_EDGE)?)?;
    let edge_ids = rd.u64s(sec(S_EDGE_IDS)?)?;
    let edge_from = rd.u32s(sec(S_EDGE_FROM)?)?;
    let edge_to = rd.u32s(sec(S_EDGE_TO)?)?;
    let edge_type = rd.u32s(sec(S_EDGE_TYPE)?)?;
    let edge_prop_off = rd.u64s(sec(S_EDGE_PROP_OFF)?)?;
    let label_off = rd.u32s(sec(S_LABEL_OFF)?)?;
    let label_nodes = rd.u32s(sec(S_LABEL_NODES)?)?;
    let indexes = parse_indexes(&rd.bytes(sec(S_PROP_INDEX)?)?, ns, n)?;
    let node_crcs = rd.u32s(sec(S_NODE_PROPS_CRC)?)?;
    let edge_crcs = rd.u32s(sec(S_EDGE_PROPS_CRC)?)?;
    let node_props = rd.blob(sec(S_NODE_PROPS)?, node_crcs, residency)?;
    let edge_props = rd.blob(sec(S_EDGE_PROPS)?, edge_crcs, residency)?;

    // Structure. Safe Rust would panic on a bad index anyway; checking here
    // turns a corrupt file into an error at open instead of a crash later.
    let chk = |ok: bool, what: &str| {
        if ok {
            Ok(())
        } else {
            Err(bad(format!("image: bad {what}")))
        }
    };
    chk(
        node_ids.len() == n && node_ids.windows(2).all(|w| w[0] < w[1]),
        "node ids",
    )?;
    chk(
        edge_ids.len() == m && edge_ids.windows(2).all(|w| w[0] < w[1]),
        "edge ids",
    )?;
    chk(
        offsets_ok(&node_label_off, n, node_labels.len() as u64),
        "node label offsets",
    )?;
    chk(
        node_labels.iter().all(|l| (*l as usize) < ns),
        "node labels",
    )?;
    chk(
        offsets_ok64(&node_prop_off, n, node_props.len()),
        "node property offsets",
    )?;
    for (off, nbr, edge, what) in [
        (&out_off, &out_nbr, &out_edge, "outgoing adjacency"),
        (&in_off, &in_nbr, &in_edge, "incoming adjacency"),
    ] {
        chk(
            offsets_ok(off, n, m as u64)
                && nbr.len() == m
                && edge.len() == m
                && nbr.iter().all(|s| (*s as usize) < n)
                && edge.iter().all(|s| (*s as usize) < m),
            what,
        )?;
    }
    chk(
        edge_from.len() == m
            && edge_to.len() == m
            && edge_from
                .iter()
                .chain(edge_to.iter())
                .all(|s| (*s as usize) < n),
        "edge endpoints",
    )?;
    chk(
        edge_type.len() == m && edge_type.iter().all(|t| (*t as usize) < ns),
        "edge types",
    )?;
    chk(
        offsets_ok64(&edge_prop_off, m, edge_props.len()),
        "edge property offsets",
    )?;
    chk(
        offsets_ok(&label_off, ns, label_nodes.len() as u64)
            && label_nodes.iter().all(|s| (*s as usize) < n),
        "label postings",
    )?;

    let mut type_counts: HashMap<u32, usize> = HashMap::new();
    for t in &edge_type {
        *type_counts.entry(*t).or_default() += 1;
    }

    let base = Base {
        node_slot: SlotIndex::build(&node_ids),
        edge_slot: SlotIndex::build(&edge_ids),
        node_ids,
        node_label_off,
        node_labels,
        node_prop_off,
        out_off,
        out_nbr,
        out_edge,
        in_off,
        in_nbr,
        in_edge,
        edge_ids,
        edge_from,
        edge_to,
        edge_type,
        edge_prop_off,
        label_off,
        label_nodes,
        type_counts,
        indexes,
        node_props,
        edge_props,
        disk: None,
        read_errors: AtomicU64::new(0),
        next_node,
        next_edge,
        image_bytes: len,
    };
    Ok((base, strings))
}

fn offsets_ok(off: &[u32], count: usize, total: u64) -> bool {
    off.len() == count + 1
        && off[0] == 0
        && off.windows(2).all(|w| w[0] <= w[1])
        && *off.last().unwrap() as u64 == total
}

fn offsets_ok64(off: &[u64], count: usize, total: u64) -> bool {
    off.len() == count + 1
        && off[0] == 0
        && off.windows(2).all(|w| w[0] <= w[1])
        && *off.last().unwrap() == total
}

struct Loader<'a, S: Source + ?Sized> {
    src: &'a S,
    at: u64,
}

impl<S: Source + ?Sized> Loader<'_, S> {
    /// Stream a section through `f` in `READ_CHUNK` pieces, checking its crc.
    fn stream(&self, s: Section, mut f: impl FnMut(&[u8])) -> io::Result<()> {
        let mut buf = vec![0u8; (s.len as usize).min(READ_CHUNK)];
        let mut crc = Crc32::new();
        let mut done = 0u64;
        while done < s.len {
            let k = ((s.len - done) as usize).min(READ_CHUNK);
            self.src.read_at(&mut buf[..k], self.at + s.off + done)?;
            crc.update(&buf[..k]);
            f(&buf[..k]);
            done += k as u64;
        }
        if crc.finish() != s.crc {
            return Err(bad("image section checksum mismatch"));
        }
        Ok(())
    }

    fn bytes(&self, s: Section) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(s.len as usize);
        self.stream(s, |c| out.extend_from_slice(c))?;
        Ok(out)
    }

    fn u32s(&self, s: Section) -> io::Result<Vec<u32>> {
        if s.len % 4 != 0 {
            return Err(bad("misaligned u32 section"));
        }
        let mut out = Vec::with_capacity((s.len / 4) as usize);
        // READ_CHUNK is a multiple of 8, so chunks never split a word.
        self.stream(s, |c| {
            out.extend(
                c.chunks_exact(4)
                    .map(|w| u32::from_le_bytes(w.try_into().unwrap())),
            )
        })?;
        Ok(out)
    }

    fn u64s(&self, s: Section) -> io::Result<Vec<u64>> {
        if s.len % 8 != 0 {
            return Err(bad("misaligned u64 section"));
        }
        let mut out = Vec::with_capacity((s.len / 8) as usize);
        self.stream(s, |c| {
            out.extend(
                c.chunks_exact(8)
                    .map(|w| u64::from_le_bytes(w.try_into().unwrap())),
            )
        })?;
        Ok(out)
    }

    /// A property blob, checked chunk by chunk against its crc list. In
    /// memory mode the whole blob is read and checked now; on disk each chunk
    /// is checked when it is first read.
    fn blob(&self, s: Section, crcs: Vec<u32>, residency: Residency) -> io::Result<Blob> {
        if crcs.len() as u64 != s.len.div_ceil(PROP_CHUNK) {
            return Err(bad("property checksum count does not match blob"));
        }
        match residency {
            Residency::OnDisk { .. } => Ok(Blob::Disk {
                at: self.at + s.off,
                len: s.len,
                crcs,
            }),
            Residency::Memory => {
                let mut out = vec![0u8; s.len as usize];
                for (i, chunk) in out.chunks_mut(PROP_CHUNK as usize).enumerate() {
                    self.src
                        .read_at(chunk, self.at + s.off + i as u64 * PROP_CHUNK)?;
                    if codec::crc32(chunk) != crcs[i] {
                        return Err(bad("property chunk checksum mismatch"));
                    }
                }
                Ok(Blob::Mem(out))
            }
        }
    }
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

fn parse_indexes(b: &[u8], ns: usize, n: usize) -> io::Result<Vec<BaseIndex>> {
    let mut r = Reader::new(b);
    let count = r.u32().map_err(bad)?;
    let mut out = Vec::new();
    let take = |r: &mut Reader, len: usize| -> io::Result<Vec<u8>> {
        if r.remaining() < len {
            return Err(bad("index section truncated"));
        }
        let v = r.buf[r.pos..r.pos + len].to_vec();
        r.pos += len;
        Ok(v)
    };
    for _ in 0..count {
        let label = r.u32().map_err(bad)?;
        let key = r.u32().map_err(bad)?;
        let nkeys = r.u32().map_err(bad)? as usize;
        let npost = r.u32().map_err(bad)? as usize;
        let keys_len = r.u32().map_err(bad)? as usize;
        if (label as usize) >= ns || (key as usize) >= ns || nkeys > b.len() || npost > n {
            return Err(bad("index header out of range"));
        }
        let words = |v: Vec<u8>| -> Vec<u32> {
            v.chunks_exact(4)
                .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
                .collect()
        };
        let key_off: Vec<u64> = words(take(&mut r, 4 * (nkeys + 1))?)
            .into_iter()
            .map(u64::from)
            .collect();
        let keys = take(&mut r, keys_len)?;
        let post_off = words(take(&mut r, 4 * (nkeys + 1))?);
        let post = words(take(&mut r, 4 * npost)?);
        let idx = BaseIndex {
            label,
            key,
            key_off,
            keys,
            post_off,
            post,
        };
        let ok = offsets_ok64(&idx.key_off, nkeys, keys_len as u64)
            && offsets_ok(&idx.post_off, nkeys, npost as u64)
            && idx.post.iter().all(|s| (*s as usize) < n)
            && (0..nkeys).all(|i| {
                let run = &idx.keys[idx.key_off[i] as usize..idx.key_off[i + 1] as usize];
                let mut kr = Reader::new(run);
                kr.skip_value().is_ok() && kr.remaining() == 0
            });
        if !ok {
            return Err(bad("image: bad property index"));
        }
        out.push(idx);
    }
    Ok(out)
}

// ------------------------------------------------------------------ writing

/// Streams sections out, recording where each landed.
struct SectionWriter<'w, W: Write + Seek> {
    w: &'w mut W,
    start: u64,
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
        format!("graph is too large for a v1 image: {what} exceeds 2^32"),
    )
}

/// Write the current state of `g` as an image at the writer's position.
/// Returns the image length. The writer is left positioned at its end.
pub(crate) fn write<W: Write + Seek>(g: &Graph, w: &mut W) -> io::Result<u64> {
    let start = w.stream_position()?;
    let dir_len = SECTION_COUNT as u64 * DIR_ENTRY_LEN;
    w.write_all(&vec![0u8; (IMAGE_HEADER_LEN + dir_len) as usize])?;
    let mut sw = SectionWriter {
        w,
        start,
        pos: IMAGE_HEADER_LEN + dir_len,
        dir: Vec::new(),
        cur: None,
        chunks: None,
    };

    let node_ids = g.node_ids();
    let edge_ids = g.edge_ids();
    let (n, m) = (node_ids.len(), edge_ids.len());
    if n >= u32::MAX as usize || m >= u32::MAX as usize {
        return Err(too_big("node or edge count"));
    }
    let nslot = SlotIndex::build(&node_ids);
    let eslot = SlotIndex::build(&edge_ids);
    let node_slot = |id: u64| {
        nslot
            .get(&node_ids, id)
            .ok_or_else(|| bad(format!("edge refers to missing node {id}")))
    };
    let edge_slot = |id: u64| {
        eslot
            .get(&edge_ids, id)
            .ok_or_else(|| bad(format!("adjacency refers to missing edge {id}")))
    };

    // Strings, verbatim and in id order, so interned ids mean the same thing
    // before and after.
    let strings = g.strings.all();
    let ns = strings.len();
    {
        let mut b = Vec::new();
        codec::put_u32(&mut b, ns as u32);
        let mut off = 0u64;
        codec::put_u32(&mut b, 0);
        for s in strings {
            off += s.len() as u64;
            if off > u32::MAX as u64 {
                return Err(too_big("string table"));
            }
            codec::put_u32(&mut b, off as u32);
        }
        for s in strings {
            b.extend_from_slice(s.as_bytes());
        }
        sw.bytes(S_STRINGS, &b)?;
    }
    sw.u64s(S_NODE_IDS, &node_ids)?;

    // Nodes: labels into columns, properties streamed into their blob.
    let mut label_off = Vec::with_capacity(n + 1);
    let mut labels: Vec<u32> = Vec::new();
    let mut prop_off = Vec::with_capacity(n + 1);
    label_off.push(0u32);
    prop_off.push(0u64);
    let mut run = Vec::new();
    let mut blob_len = 0u64;
    sw.begin(S_NODE_PROPS, true);
    for id in &node_ids {
        let node = g
            .node(*id)
            .ok_or_else(|| bad("node vanished while writing"))?;
        labels.extend_from_slice(node.labels());
        if labels.len() > u32::MAX as usize {
            return Err(too_big("label list"));
        }
        label_off.push(labels.len() as u32);
        run.clear();
        codec::put_prop_ids(&mut run, &node.props());
        sw.put(&run)?;
        blob_len += run.len() as u64;
        prop_off.push(blob_len);
    }
    let node_crcs = sw.end()?;
    sw.u32s(S_NODE_PROPS_CRC, &node_crcs)?;
    sw.u32s(S_NODE_LABEL_OFF, &label_off)?;
    sw.u32s(S_NODE_LABELS, &labels)?;
    sw.u64s(S_NODE_PROP_OFF, &prop_off)?;
    drop(prop_off);

    // Label postings, built from the label column in one pass. Slots are
    // visited in order, so every posting list comes out sorted.
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

    // Adjacency, in the order the graph reports it (ascending edge id).
    for (dir, s_off, s_nbr, s_edge) in [
        (Dir::Out, S_OUT_OFF, S_OUT_NBR, S_OUT_EDGE),
        (Dir::In, S_IN_OFF, S_IN_NBR, S_IN_EDGE),
    ] {
        let mut off = Vec::with_capacity(n + 1);
        let mut nbr = Vec::with_capacity(m);
        let mut edge = Vec::with_capacity(m);
        off.push(0u32);
        let mut err = None;
        for id in &node_ids {
            g.for_each_adj(*id, dir, None, |a| {
                if err.is_some() {
                    return;
                }
                match (node_slot(a.other), edge_slot(a.edge)) {
                    (Ok(o), Ok(e)) => {
                        nbr.push(o);
                        edge.push(e);
                    }
                    (Err(x), _) | (_, Err(x)) => err = Some(x),
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
            off.push(nbr.len() as u32);
        }
        if nbr.len() != m {
            return Err(bad("adjacency does not account for every edge"));
        }
        sw.u32s(s_off, &off)?;
        sw.u32s(s_nbr, &nbr)?;
        sw.u32s(s_edge, &edge)?;
    }

    // Edges.
    let mut from = Vec::with_capacity(m);
    let mut to = Vec::with_capacity(m);
    let mut etype = Vec::with_capacity(m);
    let mut prop_off = Vec::with_capacity(m + 1);
    prop_off.push(0u64);
    blob_len = 0;
    sw.begin(S_EDGE_PROPS, true);
    for id in &edge_ids {
        let e = g
            .edge(*id)
            .ok_or_else(|| bad("edge vanished while writing"))?;
        from.push(node_slot(e.from)?);
        to.push(node_slot(e.to)?);
        etype.push(e.etype);
        run.clear();
        codec::put_prop_ids(&mut run, &e.props());
        sw.put(&run)?;
        blob_len += run.len() as u64;
        prop_off.push(blob_len);
    }
    let edge_crcs = sw.end()?;
    sw.u32s(S_EDGE_PROPS_CRC, &edge_crcs)?;
    sw.u64s(S_EDGE_IDS, &edge_ids)?;
    sw.u32s(S_EDGE_FROM, &from)?;
    sw.u32s(S_EDGE_TO, &to)?;
    sw.u32s(S_EDGE_TYPE, &etype)?;
    sw.u64s(S_EDGE_PROP_OFF, &prop_off)?;
    drop((from, to, etype, prop_off));

    // Property indexes: every (label, key) pair defined, keys sorted by the
    // same total order the in-memory BTreeMap uses.
    {
        let defs = g.index_defs();
        let mut b = Vec::new();
        codec::put_u32(&mut b, defs.len() as u32);
        for (l, k) in defs {
            let mut map: BTreeMap<VKey, Vec<u32>> = BTreeMap::new();
            for id in g.label_member_ids(l) {
                if let Some(v) = g.node(id).and_then(|nr| nr.prop(k)) {
                    map.entry(VKey(v)).or_default().push(node_slot(id)?);
                }
            }
            let mut keys = Vec::new();
            let mut key_off = vec![0u32];
            let mut post_off = vec![0u32];
            let mut post: Vec<u32> = Vec::new();
            for (key, mut slots) in map {
                codec::put_value(&mut keys, &key.0);
                if keys.len() > u32::MAX as usize {
                    return Err(too_big("index keys"));
                }
                key_off.push(keys.len() as u32);
                slots.sort_unstable();
                post.extend_from_slice(&slots);
                post_off.push(post.len() as u32);
            }
            codec::put_u32(&mut b, l);
            codec::put_u32(&mut b, k);
            codec::put_u32(&mut b, (key_off.len() - 1) as u32);
            codec::put_u32(&mut b, post.len() as u32);
            codec::put_u32(&mut b, keys.len() as u32);
            for x in &key_off {
                codec::put_u32(&mut b, *x);
            }
            b.extend_from_slice(&keys);
            for x in post_off.iter().chain(post.iter()) {
                codec::put_u32(&mut b, *x);
            }
        }
        sw.bytes(S_PROP_INDEX, &b)?;
    }

    // Header and directory, now that every section's place is known.
    let end = sw.pos;
    let mut dir = Vec::with_capacity(dir_len as usize);
    for (kind, s) in &sw.dir {
        codec::put_u32(&mut dir, *kind);
        codec::put_u32(&mut dir, 0);
        codec::put_u64(&mut dir, s.off);
        codec::put_u64(&mut dir, s.len);
        codec::put_u32(&mut dir, s.crc);
        codec::put_u32(&mut dir, 0);
    }
    debug_assert_eq!(dir.len() as u64, dir_len);
    let mut head = Vec::with_capacity(IMAGE_HEADER_LEN as usize);
    head.extend_from_slice(IMAGE_MAGIC);
    codec::put_u32(&mut head, IMAGE_VERSION);
    codec::put_u32(&mut head, sw.dir.len() as u32);
    codec::put_u64(&mut head, n as u64);
    codec::put_u64(&mut head, m as u64);
    let (next_node, next_edge) = g.next_ids();
    codec::put_u64(&mut head, next_node);
    codec::put_u64(&mut head, next_edge);
    head.resize(60, 0);
    let mut crc = Crc32::new();
    crc.update(&head);
    crc.update(&dir);
    codec::put_u32(&mut head, crc.finish());

    let start = sw.start;
    let w = sw.w;
    w.seek(SeekFrom::Start(start))?;
    w.write_all(&head)?;
    w.write_all(&dir)?;
    w.seek(SeekFrom::Start(start + end))?;
    Ok(end)
}
