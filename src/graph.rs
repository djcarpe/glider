//! The graph engine: a labelled property graph stored in B+trees on pages.
//!
//! Everything lives in the page store — in RAM for `:memory:`, on disk
//! otherwise — so capacity is RAM (up to `max_memory`) or disk respectively,
//! and a file-backed graph's memory use is a bounded page cache however large
//! the graph grows.
//!
//! Trees:
//!
//! ```text
//! catalog  'S' id            -> interned string
//!          'I' label key     -> index id, distinct values, entries
//!          'L' label         -> nodes with the label
//!          'T' type          -> edges of the type
//! nodes    id                -> labels, properties
//! edges    id                -> from, to, type, properties
//! adj      node dir edge     -> type, other end     (dir 0 out, 1 in)
//! labels   label node        -> ()
//! types    type edge         -> ()
//! props    index value node  -> truncated?          (order-preserving value)
//! ```
//!
//! Every mutation is an [`Op`]: logged to the write-ahead log, then applied
//! to the trees by [`apply`] — the same function recovery replays through.
//! A failure part-way (the graph is full, the disk is full) rolls the whole
//! transaction back, so the graph is never left half-changed.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use crate::codec;
use crate::storage::btree::Tree;
use crate::storage::db::{Db, DbConfig};
use crate::storage::keys;
use crate::storage::log::SyncMode;
use crate::storage::pager::{self, Pager};
use crate::storage::{SError, SResult};
use crate::store::{Op, Sync};
use crate::value::Value;

pub use crate::types::{
    get_prop, id_map, id_set, Adj, Csr, Dir, Error, IdBuild, IdHasher, IdMap, IdSet, Interner,
    KeySample, Result, VKey,
};

const CATALOG: Tree = Tree { slot: 0, id: 1 };
const NODES: Tree = Tree { slot: 1, id: 2 };
const EDGES: Tree = Tree { slot: 2, id: 3 };
const ADJ: Tree = Tree { slot: 3, id: 4 };
const LABELS: Tree = Tree { slot: 4, id: 5 };
const TYPES: Tree = Tree { slot: 5, id: 6 };
const PROPS: Tree = Tree { slot: 6, id: 7 };

const M_NEXT_NODE: usize = 0;
const M_NEXT_EDGE: usize = 1;
const M_NODES: usize = 2;
const M_EDGES: usize = 3;
const M_NEXT_INDEX: usize = 4;

const OUT: u8 = 0;
const IN: u8 = 1;

/// Scans that modify what they scan work in batches of this many ids.
const BATCH: usize = 4096;
/// `index_count` stops counting here; the planner only needs to know which
/// side is smaller.
const COUNT_CAP: usize = 100_000;

// ---------------------------------------------------------------- options

/// How to open a database. `Default` is what `Graph::open` uses.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    pub sync: Sync,
    /// Break a lock left behind by a writer that is definitely gone.
    pub force: bool,
    /// File-backed: bytes of page cache. RAM use is bounded by this plus
    /// query working memory, however large the database.
    pub cache_size: u64,
    /// `:memory:`: the most memory the graph may occupy. Default: the
    /// machine's physical memory.
    pub max_memory: u64,
    /// Page size for a new database (a power of two, 512..=32768).
    pub page_size: usize,
    /// Size of each segment file of a new database.
    pub segment_size: u64,
    /// Checkpoint after this many bytes of write-ahead log.
    pub checkpoint_bytes: u64,
    /// File-backed: memory algorithms may use for their projection and
    /// per-node state before spilling to `<db>-tmp/`. (`:memory:` uses the
    /// headroom left under `max_memory`.)
    pub work_mem: u64,
    /// Size of each write-ahead log segment file; the log is deleted a
    /// whole segment at a time once checkpointed (and shipped, when
    /// replicating).
    pub wal_segment: u64,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            sync: Sync::Normal,
            force: false,
            cache_size: pager::DEFAULT_CACHE_BYTES,
            max_memory: pager::physical_memory(),
            page_size: crate::storage::page::DEFAULT_PAGE,
            segment_size: pager::DEFAULT_SEGMENT_BYTES,
            checkpoint_bytes: 256 << 20,
            work_mem: DEFAULT_WORK_MEM,
            wal_segment: crate::storage::log::DEFAULT_SEGMENT,
        }
    }
}

/// Default working memory for algorithms on a file-backed graph.
pub const DEFAULT_WORK_MEM: u64 = 256 << 20;

fn sync_mode(s: Sync) -> SyncMode {
    match s {
        Sync::Always => SyncMode::Always,
        Sync::Normal => SyncMode::Normal,
        Sync::Off => SyncMode::Off,
    }
}

impl From<SError> for Error {
    fn from(e: SError) -> Self {
        match e {
            SError::Io(e) => Error::Io(e),
            SError::Full(m) => Error::Full(m),
            SError::Corrupt(m) => Error::Msg(format!("database is damaged: {m}")),
        }
    }
}

// ------------------------------------------------------------------ views

/// One node, read out of the store.
#[derive(Clone, Debug)]
pub struct NodeRef {
    pub id: u64,
    labels: Vec<u32>,
    props: Vec<u8>,
}

impl NodeRef {
    pub fn labels(&self) -> &[u32] {
        &self.labels
    }
    pub fn has_label(&self, l: u32) -> bool {
        self.labels.contains(&l)
    }
    pub fn props(&self) -> Cow<'_, [(u32, Value)]> {
        Cow::Owned(codec::read_prop_ids(&self.props).unwrap_or_default())
    }
    pub fn prop(&self, key: u32) -> Option<Value> {
        codec::find_prop(&self.props, key).ok().flatten()
    }
}

/// One edge, read out of the store.
#[derive(Clone, Debug)]
pub struct EdgeRef {
    pub id: u64,
    pub from: u64,
    pub to: u64,
    pub etype: u32,
    props: Vec<u8>,
}

impl EdgeRef {
    pub fn props(&self) -> Cow<'_, [(u32, Value)]> {
        Cow::Owned(codec::read_prop_ids(&self.props).unwrap_or_default())
    }
    pub fn prop(&self, key: u32) -> Option<Value> {
        codec::find_prop(&self.props, key).ok().flatten()
    }
}

// --------------------------------------------------------------- records

fn node_key(id: u64) -> [u8; 8] {
    id.to_be_bytes()
}

fn encode_node(labels: &[u32], props: &[(u32, Value)]) -> Vec<u8> {
    let mut b = Vec::new();
    codec::put_varint(&mut b, labels.len() as u64);
    for l in labels {
        codec::put_varint(&mut b, *l as u64);
    }
    codec::put_prop_ids(&mut b, props);
    b
}

fn decode_node(b: &[u8]) -> SResult<(Vec<u32>, Vec<u8>)> {
    let mut r = codec::Reader::new(b);
    let n = r.varint().map_err(SError::Corrupt)? as usize;
    let mut labels = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        labels.push(r.varint().map_err(SError::Corrupt)? as u32);
    }
    Ok((labels, b[r.pos..].to_vec()))
}

fn encode_edge(from: u64, to: u64, t: u32, props: &[(u32, Value)]) -> Vec<u8> {
    let mut b = Vec::new();
    codec::put_varint(&mut b, from);
    codec::put_varint(&mut b, to);
    codec::put_varint(&mut b, t as u64);
    codec::put_prop_ids(&mut b, props);
    b
}

fn decode_edge(b: &[u8]) -> SResult<(u64, u64, u32, Vec<u8>)> {
    let mut r = codec::Reader::new(b);
    let from = r.varint().map_err(SError::Corrupt)?;
    let to = r.varint().map_err(SError::Corrupt)?;
    let t = r.varint().map_err(SError::Corrupt)? as u32;
    Ok((from, to, t, b[r.pos..].to_vec()))
}

fn props_of(run: &[u8]) -> SResult<Vec<(u32, Value)>> {
    codec::read_prop_ids(run).map_err(SError::Corrupt)
}

fn adj_key(node: u64, dir: u8, edge: u64) -> [u8; 17] {
    let mut k = [0u8; 17];
    k[..8].copy_from_slice(&node.to_be_bytes());
    k[8] = dir;
    k[9..].copy_from_slice(&edge.to_be_bytes());
    k
}

fn adj_val(t: u32, other: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(10);
    codec::put_varint(&mut v, t as u64);
    codec::put_varint(&mut v, other);
    v
}

fn pair_key(a: u32, b: u64) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[..4].copy_from_slice(&a.to_be_bytes());
    k[4..].copy_from_slice(&b.to_be_bytes());
    k
}

fn cat_key(tag: u8, a: u32) -> [u8; 5] {
    let mut k = [tag, 0, 0, 0, 0];
    k[1..].copy_from_slice(&a.to_be_bytes());
    k
}

fn cat_key2(tag: u8, a: u32, b: u32) -> [u8; 9] {
    let mut k = [tag, 0, 0, 0, 0, 0, 0, 0, 0];
    k[1..5].copy_from_slice(&a.to_be_bytes());
    k[5..].copy_from_slice(&b.to_be_bytes());
    k
}

fn get_count(p: &Pager, key: &[u8]) -> SResult<u64> {
    Ok(CATALOG
        .get(p, key)?
        .map(|v| u64::from_le_bytes(v[..8].try_into().unwrap()))
        .unwrap_or(0))
}

fn add_count(p: &Pager, key: &[u8], delta: i64) -> SResult<()> {
    let n = (get_count(p, key)? as i64 + delta).max(0) as u64;
    if n == 0 {
        CATALOG.delete(p, key)?;
    } else {
        CATALOG.put(p, key, &n.to_le_bytes())?;
    }
    Ok(())
}

// ----------------------------------------------------------- index keys

/// A property index definition: its id, and what it covers.
#[derive(Clone, Copy, Debug)]
struct IndexDef {
    id: u32,
}

fn index_limit(p: &Pager) -> usize {
    crate::storage::btree::max_key(p.page_size()) - 4 - 8 - 1
}

/// The key prefix for (index, value): index id, then the value's
/// order-preserving encoding, cut to fit. Returns (prefix, truncated).
fn idx_prefix(p: &Pager, index: u32, v: &Value) -> (Vec<u8>, bool) {
    let mut k = Vec::with_capacity(32);
    k.extend_from_slice(&index.to_be_bytes());
    keys::put_value(&mut k, v);
    let limit = 4 + index_limit(p);
    if k.len() > limit {
        k.truncate(limit);
        (k, true)
    } else {
        (k, false)
    }
}

/// Index stats in the catalog: id, distinct values, entries.
fn index_stats(p: &Pager, l: u32, k: u32) -> SResult<Option<(u32, u64, u64)>> {
    Ok(CATALOG.get(p, &cat_key2(b'I', l, k))?.map(|v| {
        (
            u32::from_le_bytes(v[0..4].try_into().unwrap()),
            u64::from_le_bytes(v[4..12].try_into().unwrap()),
            u64::from_le_bytes(v[12..20].try_into().unwrap()),
        )
    }))
}

fn put_index_stats(p: &Pager, l: u32, k: u32, id: u32, distinct: u64, entries: u64) -> SResult<()> {
    let mut v = Vec::with_capacity(20);
    v.extend_from_slice(&id.to_le_bytes());
    v.extend_from_slice(&distinct.to_le_bytes());
    v.extend_from_slice(&entries.to_le_bytes());
    CATALOG.put(p, &cat_key2(b'I', l, k), &v)
}

// ---------------------------------------------------------- engine state

/// Index definitions, by (label, key).
type Indexes = HashMap<(u32, u32), IndexDef>;

/// What the engine keeps in memory besides pages: the interned strings (a
/// schema-sized table) and the index definitions. Both are rebuilt from the
/// catalog on open and after a rollback.
struct Schema<'a> {
    strings: &'a mut Interner,
    indexes: &'a mut Indexes,
}

fn load_schema(p: &Pager) -> SResult<(Interner, Indexes)> {
    {
        let mut list = Vec::new();
        let mut c = CATALOG.seek(p, &[b'S'])?;
        while let Some((k, v)) = c.next()? {
            if k[0] != b'S' {
                break;
            }
            let id = keys::get_u32(k, 1) as usize;
            if id != list.len() {
                return Err(SError::Corrupt(format!("string table has a gap at {id}")));
            }
            let s = v.load(p)?;
            list.push(String::from_utf8(s.into_owned()).map_err(|_| SError::Corrupt("string is not utf-8".into()))?);
        }
        let mut indexes = HashMap::new();
        let mut c = CATALOG.seek(p, &[b'I'])?;
        while let Some((k, v)) = c.next()? {
            if k[0] != b'I' {
                break;
            }
            let l = keys::get_u32(k, 1);
            let key = keys::get_u32(k, 5);
            let v = v.load(p)?;
            indexes.insert((l, key), IndexDef { id: u32::from_le_bytes(v[0..4].try_into().unwrap()) });
        }
        Ok((Interner::from_list(list), indexes))
    }
}

impl Schema<'_> {
    fn intern(&mut self, p: &Pager, s: &str) -> SResult<u32> {
        if let Some(id) = self.strings.lookup(s) {
            return Ok(id);
        }
        let id = self.strings.intern(s);
        CATALOG.put(p, &cat_key(b'S', id), s.as_bytes())?;
        Ok(id)
    }

    /// Index entries (index id, value) a node with these labels and props
    /// contributes.
    fn entries(&self, labels: &[u32], props: &[(u32, Value)]) -> Vec<((u32, u32), u32, Value)> {
        let mut out = Vec::new();
        if self.indexes.is_empty() {
            return out;
        }
        for (&(l, k), def) in self.indexes.iter() {
            if labels.contains(&l) {
                if let Some(v) = get_prop(props, k) {
                    out.push(((l, k), def.id, v.clone()));
                }
            }
        }
        out
    }
}

/// Does any node other than `except` hold exactly `v` in this index? For a
/// value short enough to be stored whole, any entry under its prefix is
/// that value; a long, truncated one shares its prefix with other long
/// values, so each candidate is checked against the node itself.
fn value_indexed(p: &Pager, index: u32, key: u32, v: &Value, except: u64) -> SResult<bool> {
    let (prefix, truncated) = idx_prefix(p, index, v);
    let mut c = PROPS.seek(p, &prefix)?;
    while let Some((k, _)) = c.next()? {
        if !k.starts_with(&prefix) {
            return Ok(false);
        }
        if k.len() != prefix.len() + 8 {
            continue;
        }
        let node = keys::get_u64(k, prefix.len());
        if node == except {
            continue;
        }
        if !truncated {
            return Ok(true);
        }
        let same = node_record(p, node)?
            .and_then(|(_, run)| codec::find_prop(&run, key).ok().flatten())
            .map(|x| x.total_cmp(v) == std::cmp::Ordering::Equal)
            .unwrap_or(false);
        if same {
            return Ok(true);
        }
    }
    Ok(false)
}

fn idx_insert(p: &Pager, lk: (u32, u32), index: u32, v: &Value, node: u64) -> SResult<()> {
    let (prefix, truncated) = idx_prefix(p, index, v);
    let mut key = prefix;
    key.extend_from_slice(&node.to_be_bytes());
    if PROPS.contains(p, &key)? {
        return Ok(());
    }
    let fresh = !value_indexed(p, index, lk.1, v, node)?;
    PROPS.put(p, &key, &[truncated as u8])?;
    if let Some((id, d, e)) = index_stats(p, lk.0, lk.1)? {
        put_index_stats(p, lk.0, lk.1, id, d + fresh as u64, e + 1)?;
    }
    Ok(())
}

fn idx_remove(p: &Pager, lk: (u32, u32), index: u32, v: &Value, node: u64) -> SResult<()> {
    let (prefix, _) = idx_prefix(p, index, v);
    let mut key = prefix;
    key.extend_from_slice(&node.to_be_bytes());
    if !PROPS.delete(p, &key)? {
        return Ok(());
    }
    // The node's record may already hold its new value, so compare against
    // `v` itself rather than re-reading this node.
    let gone = !value_indexed(p, index, lk.1, v, node)?;
    if let Some((id, d, e)) = index_stats(p, lk.0, lk.1)? {
        put_index_stats(p, lk.0, lk.1, id, d.saturating_sub(gone as u64), e.saturating_sub(1))?;
    }
    Ok(())
}

fn node_record(p: &Pager, id: u64) -> SResult<Option<(Vec<u32>, Vec<u8>)>> {
    match NODES.get(p, &node_key(id))? {
        Some(b) => Ok(Some(decode_node(&b)?)),
        None => Ok(None),
    }
}

fn edge_record(p: &Pager, id: u64) -> SResult<Option<(u64, u64, u32, Vec<u8>)>> {
    match EDGES.get(p, &node_key(id))? {
        Some(b) => Ok(Some(decode_edge(&b)?)),
        None => Ok(None),
    }
}

/// Write a node's new state, keeping labels, counts and indexes in step
/// with what it had before.
fn store_node(
    p: &Pager,
    sch: &Schema<'_>,
    id: u64,
    old: Option<(&[u32], &[(u32, Value)])>,
    labels: &[u32],
    props: &[(u32, Value)],
) -> SResult<()> {
    if let Some((ol, op)) = old {
        for (lk, ix, v) in sch.entries(ol, op) {
            idx_remove(p, lk, ix, &v, id)?;
        }
        for l in ol {
            if !labels.contains(l) {
                LABELS.delete(p, &pair_key(*l, id))?;
                add_count(p, &cat_key(b'L', *l), -1)?;
            }
        }
    }
    for l in labels {
        if !old.map(|(ol, _)| ol.contains(l)).unwrap_or(false) {
            LABELS.put(p, &pair_key(*l, id), &[])?;
            add_count(p, &cat_key(b'L', *l), 1)?;
        }
    }
    NODES.put(p, &node_key(id), &encode_node(labels, props))?;
    for (lk, ix, v) in sch.entries(labels, props) {
        idx_insert(p, lk, ix, &v, id)?;
    }
    Ok(())
}

fn remove_edge(p: &Pager, id: u64) -> SResult<bool> {
    let Some((from, to, t, _)) = edge_record(p, id)? else {
        return Ok(false);
    };
    ADJ.delete(p, &adj_key(from, OUT, id))?;
    ADJ.delete(p, &adj_key(to, IN, id))?;
    TYPES.delete(p, &pair_key(t, id))?;
    add_count(p, &cat_key(b'T', t), -1)?;
    EDGES.delete(p, &node_key(id))?;
    p.set_meta(M_EDGES, p.meta(M_EDGES).saturating_sub(1));
    Ok(true)
}

fn dedup(labels: &mut Vec<u32>) {
    let mut seen = BTreeSet::new();
    labels.retain(|l| seen.insert(*l));
}

fn set_prop(props: &mut Vec<(u32, Value)>, key: u32, value: Value) {
    match props.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value,
        None => props.push((key, value)),
    }
}

/// Apply one op to the trees. The single implementation of every mutation:
/// the API calls it, and so does recovery.
fn apply(p: &Pager, sch: &mut Schema<'_>, op: &Op) -> SResult<()> {
    match op {
        Op::NodeAdd { id, labels, props } => {
            let mut lids = Vec::with_capacity(labels.len());
            for l in labels {
                lids.push(sch.intern(p, l)?);
            }
            dedup(&mut lids);
            let mut pids = Vec::with_capacity(props.len());
            for (k, v) in props {
                let k = sch.intern(p, k)?;
                set_prop(&mut pids, k, v.clone());
            }
            let old = node_record(p, *id)?;
            let old_props = match &old {
                Some((_, run)) => Some(props_of(run)?),
                None => None,
            };
            if old.is_none() {
                p.set_meta(M_NODES, p.meta(M_NODES) + 1);
            }
            let old_ref = old
                .as_ref()
                .map(|(l, _)| (l.as_slice(), old_props.as_deref().unwrap_or(&[])));
            store_node(p, sch, *id, old_ref, &lids, &pids)?;
            if *id >= p.meta(M_NEXT_NODE) {
                p.set_meta(M_NEXT_NODE, id + 1);
            }
        }
        Op::NodeDel { id } => {
            let Some((labels, run)) = node_record(p, *id)? else {
                return Ok(());
            };
            // Detach, a batch at a time: never modify the adjacency tree
            // while a cursor is walking it.
            loop {
                let mut batch: Vec<Vec<u8>> = Vec::with_capacity(BATCH);
                {
                    let prefix = id.to_be_bytes();
                    let mut c = ADJ.seek(p, &prefix)?;
                    while let Some((k, _)) = c.next()? {
                        if !k.starts_with(&prefix) || batch.len() == BATCH {
                            break;
                        }
                        batch.push(k.to_vec());
                    }
                }
                if batch.is_empty() {
                    break;
                }
                for k in batch {
                    // An entry whose edge is already gone would otherwise
                    // be found again forever: remove it directly.
                    if !remove_edge(p, keys::get_u64(&k, 9))? {
                        ADJ.delete(p, &k)?;
                    }
                }
            }
            let props = props_of(&run)?;
            for (lk, ix, v) in sch.entries(&labels, &props) {
                idx_remove(p, lk, ix, &v, *id)?;
            }
            for l in &labels {
                LABELS.delete(p, &pair_key(*l, *id))?;
                add_count(p, &cat_key(b'L', *l), -1)?;
            }
            NODES.delete(p, &node_key(*id))?;
            p.set_meta(M_NODES, p.meta(M_NODES).saturating_sub(1));
        }
        Op::EdgeAdd {
            id,
            from,
            to,
            etype,
            props,
        } => {
            if !NODES.contains(p, &node_key(*from))? || !NODES.contains(p, &node_key(*to))? {
                return Ok(());
            }
            let t = sch.intern(p, etype)?;
            let mut pids = Vec::with_capacity(props.len());
            for (k, v) in props {
                let k = sch.intern(p, k)?;
                set_prop(&mut pids, k, v.clone());
            }
            remove_edge(p, *id)?;
            EDGES.put(p, &node_key(*id), &encode_edge(*from, *to, t, &pids))?;
            ADJ.put(p, &adj_key(*from, OUT, *id), &adj_val(t, *to))?;
            ADJ.put(p, &adj_key(*to, IN, *id), &adj_val(t, *from))?;
            TYPES.put(p, &pair_key(t, *id), &[])?;
            add_count(p, &cat_key(b'T', t), 1)?;
            p.set_meta(M_EDGES, p.meta(M_EDGES) + 1);
            if *id >= p.meta(M_NEXT_EDGE) {
                p.set_meta(M_NEXT_EDGE, id + 1);
            }
        }
        Op::EdgeDel { id } => {
            remove_edge(p, *id)?;
        }
        Op::NodeSet { id, key, value } => {
            let k = sch.intern(p, key)?;
            let Some((labels, run)) = node_record(p, *id)? else {
                return Ok(());
            };
            let old = props_of(&run)?;
            let mut new = old.clone();
            set_prop(&mut new, k, value.clone());
            store_node(p, sch, *id, Some((&labels, &old)), &labels, &new)?;
        }
        Op::NodeUnset { id, key } => {
            let Some(k) = sch.strings.lookup(key) else {
                return Ok(());
            };
            let Some((labels, run)) = node_record(p, *id)? else {
                return Ok(());
            };
            let old = props_of(&run)?;
            if get_prop(&old, k).is_none() {
                return Ok(());
            }
            let new: Vec<_> = old.iter().filter(|(pk, _)| *pk != k).cloned().collect();
            store_node(p, sch, *id, Some((&labels, &old)), &labels, &new)?;
        }
        Op::EdgeSet { id, key, value } => {
            let k = sch.intern(p, key)?;
            let Some((from, to, t, run)) = edge_record(p, *id)? else {
                return Ok(());
            };
            let mut props = props_of(&run)?;
            set_prop(&mut props, k, value.clone());
            EDGES.put(p, &node_key(*id), &encode_edge(from, to, t, &props))?;
        }
        Op::EdgeUnset { id, key } => {
            let Some(k) = sch.strings.lookup(key) else {
                return Ok(());
            };
            let Some((from, to, t, run)) = edge_record(p, *id)? else {
                return Ok(());
            };
            let mut props = props_of(&run)?;
            let before = props.len();
            props.retain(|(pk, _)| *pk != k);
            if props.len() != before {
                EDGES.put(p, &node_key(*id), &encode_edge(from, to, t, &props))?;
            }
        }
        Op::LabelAdd { id, label } => {
            let l = sch.intern(p, label)?;
            let Some((labels, run)) = node_record(p, *id)? else {
                return Ok(());
            };
            if labels.contains(&l) {
                return Ok(());
            }
            let props = props_of(&run)?;
            let mut new = labels.clone();
            new.push(l);
            store_node(p, sch, *id, Some((&labels, &props)), &new, &props)?;
        }
        Op::LabelDel { id, label } => {
            let Some(l) = sch.strings.lookup(label) else {
                return Ok(());
            };
            let Some((labels, run)) = node_record(p, *id)? else {
                return Ok(());
            };
            if !labels.contains(&l) {
                return Ok(());
            }
            let props = props_of(&run)?;
            let new: Vec<u32> = labels.iter().copied().filter(|x| *x != l).collect();
            store_node(p, sch, *id, Some((&labels, &props)), &new, &props)?;
        }
        Op::IndexAdd { label, key } => {
            let l = sch.intern(p, label)?;
            let k = sch.intern(p, key)?;
            if sch.indexes.contains_key(&(l, k)) {
                return Ok(());
            }
            let id = p.meta(M_NEXT_INDEX) as u32;
            p.set_meta(M_NEXT_INDEX, id as u64 + 1);
            put_index_stats(p, l, k, id, 0, 0)?;
            sch.indexes.insert((l, k), IndexDef { id });
            // Build it from the label's members. The label tree is not
            // modified meanwhile, so one cursor walks it throughout.
            let prefix = l.to_be_bytes();
            let mut c = LABELS.seek(p, &prefix)?;
            while let Some((key, _)) = c.next()? {
                if !key.starts_with(&prefix) {
                    break;
                }
                let node = keys::get_u64(key, 4);
                if let Some((_, run)) = node_record(p, node)? {
                    if let Some(v) = codec::find_prop(&run, k).map_err(SError::Corrupt)? {
                        idx_insert(p, (l, k), id, &v, node)?;
                    }
                }
            }
        }
        Op::IndexDel { label, key } => {
            let (Some(l), Some(k)) = (sch.strings.lookup(label), sch.strings.lookup(key)) else {
                return Ok(());
            };
            let Some(def) = sch.indexes.remove(&(l, k)) else {
                return Ok(());
            };
            delete_prefix(p, PROPS, &def.id.to_be_bytes())?;
            CATALOG.delete(p, &cat_key2(b'I', l, k))?;
        }
        Op::Counters {
            next_node,
            next_edge,
        } => {
            p.set_meta(M_NEXT_NODE, p.meta(M_NEXT_NODE).max(*next_node));
            p.set_meta(M_NEXT_EDGE, p.meta(M_NEXT_EDGE).max(*next_edge));
        }
        Op::Clear => {
            for t in [NODES, EDGES, ADJ, LABELS, TYPES, PROPS] {
                t.clear(p)?;
            }
            delete_prefix(p, CATALOG, &[b'L'])?;
            delete_prefix(p, CATALOG, &[b'T'])?;
            let defs: Vec<_> = sch.indexes.iter().map(|(lk, d)| (*lk, d.id)).collect();
            for ((l, k), id) in defs {
                put_index_stats(p, l, k, id, 0, 0)?;
            }
            for m in [M_NEXT_NODE, M_NEXT_EDGE] {
                p.set_meta(m, 1);
            }
            p.set_meta(M_NODES, 0);
            p.set_meta(M_EDGES, 0);
        }
    }
    Ok(())
}

/// Delete every key with `prefix`, a batch at a time.
fn delete_prefix(p: &Pager, t: Tree, prefix: &[u8]) -> SResult<()> {
    loop {
        let mut batch = Vec::new();
        {
            let mut c = t.seek(p, prefix)?;
            while let Some((k, _)) = c.next()? {
                if !k.starts_with(prefix) || batch.len() == BATCH {
                    break;
                }
                batch.push(k.to_vec());
            }
        }
        if batch.is_empty() {
            return Ok(());
        }
        for k in batch {
            t.delete(p, &k)?;
        }
    }
}

fn op_record(op: &Op) -> Vec<u8> {
    op.encode_record()
}

fn replay(p: &Pager, sch: &mut Option<(Interner, Indexes)>, rec: &[u8]) -> SResult<()> {
    let op = Op::decode_record(rec).map_err(SError::Corrupt)?;
    if sch.is_none() {
        *sch = Some(load_schema(p)?);
    }
    let (strings, indexes) = sch.as_mut().expect("loaded");
    apply(p, &mut Schema { strings, indexes }, &op)
}

// ------------------------------------------------------------------ graph

pub struct Graph {
    db: Db,
    /// Labels, edge types and property keys, interned to u32.
    pub strings: Interner,
    indexes: Indexes,
    /// Commit after every mutating statement. Turn off for bulk loads.
    pub autocommit: bool,
    uncommitted: u64,
    /// The first storage error a read ran into (reads cannot return one).
    read_error: std::sync::Mutex<Option<String>>,
    work_mem: u64,
}

impl Graph {
    fn init(db: Db) -> Result<Graph> {
        let p = db.pager();
        if p.meta(M_NEXT_NODE) == 0 {
            p.set_meta(M_NEXT_NODE, 1);
            p.set_meta(M_NEXT_EDGE, 1);
            p.commit();
        }
        let (strings, indexes) = load_schema(p)?;
        Ok(Graph {
            db,
            strings,
            indexes,
            autocommit: true,
            uncommitted: 0,
            read_error: std::sync::Mutex::new(None),
            work_mem: DEFAULT_WORK_MEM,
        })
    }

    /// A graph in memory, allowed to grow to the machine's physical memory.
    pub fn memory() -> Graph {
        Graph::memory_with_limit(pager::physical_memory())
    }

    /// A graph in memory that may occupy at most `max_memory` bytes. Past
    /// that, writes fail with [`Error::Full`] and are rolled back; the graph
    /// stays usable.
    pub fn memory_with_limit(max_memory: u64) -> Graph {
        Graph::init(Db::memory(crate::storage::page::DEFAULT_PAGE, max_memory)).expect("empty memory graph")
    }

    /// A memory graph with a chosen page size: for tests that want splits
    /// and overflow to happen on small graphs.
    #[doc(hidden)]
    pub fn memory_with_page_size(page_size: usize, max_memory: u64) -> Graph {
        Graph::init(Db::memory(page_size, max_memory)).expect("empty memory graph")
    }

    /// Open a database file, creating it if absent.
    pub fn open(path: &Path, sync: Sync) -> Result<Graph> {
        Graph::open_opts(
            path,
            OpenOptions {
                sync,
                ..OpenOptions::default()
            },
        )
    }

    /// Open even if a lock file is present. Only when you know the previous
    /// writer is dead — two live writers corrupt the file.
    pub fn open_forced(path: &Path, sync: Sync) -> Result<Graph> {
        Graph::open_opts(
            path,
            OpenOptions {
                sync,
                force: true,
                ..OpenOptions::default()
            },
        )
    }

    pub fn open_opts(path: &Path, opts: OpenOptions) -> Result<Graph> {
        if let Some(kind) = crate::legacy::detect(path) {
            return Err(Error::Msg(format!(
                "{} is a {kind} glider database; convert it with `glider {} migrate`",
                path.display(),
                path.display()
            )));
        }
        let cfg = DbConfig {
            pager: pager::Config {
                page_size: opts.page_size,
                cache_bytes: opts.cache_size,
                max_memory: opts.max_memory,
                segment_bytes: opts.segment_size,
            },
            sync: sync_mode(opts.sync),
            checkpoint_bytes: opts.checkpoint_bytes,
            force: opts.force,
            wal_segment: opts.wal_segment,
            ..DbConfig::default()
        };
        let mut sch: Option<(Interner, Indexes)> = None;
        let (db, _) = Db::open(path, &cfg, &mut |p, rec| replay(p, &mut sch, rec))?;
        let mut g = Graph::init(db)?;
        g.work_mem = opts.work_mem;
        Ok(g)
    }

    pub fn is_persistent(&self) -> bool {
        !self.db.is_memory()
    }

    pub fn path(&self) -> Option<&Path> {
        self.db.path()
    }

    fn pager(&self) -> &Pager {
        self.db.pager()
    }

    /// Bytes the database occupies: the file (and segments) for a
    /// file-backed graph, pages held for an in-memory one.
    pub fn file_len(&self) -> u64 {
        let p = self.pager();
        match p.memory_usage() {
            Some((used, _)) => used,
            None => p.high_water() * p.page_size() as u64,
        }
    }

    pub fn set_sync(&mut self, sync: Sync) {
        self.db.set_sync(sync_mode(sync));
    }

    /// Checkpoint automatically after this many bytes of log.
    pub fn set_checkpoint_bytes(&mut self, bytes: u64) {
        self.db.set_checkpoint_bytes(bytes);
    }

    /// An in-memory graph loaded from the bytes of a database file, for
    /// hosts without a filesystem (wasm). Edits live only in memory.
    pub fn from_bytes(bytes: &[u8]) -> Result<Graph> {
        if crate::legacy::detect_bytes(bytes) {
            let old = crate::legacy::graph::Graph::from_bytes(bytes)?;
            let mut g = Graph::memory();
            g.import_legacy(&old)?;
            return Ok(g);
        }
        if bytes.len() < crate::storage::page::MIN_PAGE * 2 {
            return Err(Error::Msg(format!(
                "file is too short to be a glider database ({} bytes)",
                bytes.len()
            )));
        }
        Graph::init(Db::from_image(bytes, pager::physical_memory())?)
    }

    /// Copy everything from a legacy-engine graph into this one, keeping
    /// ids: nodes, edges, indexes and id counters. Commits as it goes, so
    /// memory stays bounded by the source graph, not the copy.
    pub fn import_legacy(&mut self, old: &crate::legacy::graph::Graph) -> Result<()> {
        let auto = self.autocommit;
        self.autocommit = false;
        let r = (|| -> Result<()> {
            for (l, k, _) in old.indexes() {
                self.exec(Op::IndexAdd { label: l, key: k })?;
            }
            let mut n = 0u64;
            for id in old.node_ids() {
                self.exec(Op::NodeAdd {
                    id,
                    labels: old.node_labels(id),
                    props: old.node_props(id),
                })?;
                n += 1;
                if n % 50_000 == 0 {
                    self.commit()?;
                }
            }
            for id in old.edge_ids() {
                let Some(e) = old.edge(id) else { continue };
                self.exec(Op::EdgeAdd {
                    id,
                    from: e.from,
                    to: e.to,
                    etype: old.edge_type_name(id).unwrap_or("").to_string(),
                    props: old.edge_props(id),
                })?;
                n += 1;
                if n % 50_000 == 0 {
                    self.commit()?;
                }
            }
            let (next_node, next_edge) = crate::legacy::image::ImageSource::next_ids(old);
            self.exec(Op::Counters { next_node, next_edge })?;
            self.commit()
        })();
        self.autocommit = auto;
        r
    }

    /// For a `:memory:` graph: change the memory limit.
    pub fn set_max_memory(&mut self, bytes: u64) {
        self.pager().set_max_memory(bytes);
    }

    /// Force everything committed so far down to the platter, whatever the
    /// sync mode says, and fold the log into the pages.
    pub fn checkpoint(&mut self) -> Result<()> {
        if self.pager().in_txn() {
            self.db.flush()?;
            return Ok(());
        }
        self.db.checkpoint()?;
        Ok(())
    }

    pub fn node_count(&self) -> usize {
        self.pager().meta(M_NODES) as usize
    }

    pub fn edge_count(&self) -> usize {
        self.pager().meta(M_EDGES) as usize
    }

    // ------------------------------------------------------------- errors

    /// A storage error on a read path. Reads return plain values, so a
    /// damaged page reads as absent and is recorded here; `query::execute`
    /// reports it instead of returning results.
    fn note<T: Default>(&self, r: SResult<T>) -> T {
        r.unwrap_or_else(|e| {
            let mut slot = self.read_error.lock().unwrap_or_else(|x| x.into_inner());
            if slot.is_none() {
                *slot = Some(e.to_string());
            }
            T::default()
        })
    }

    /// The first storage error a read hit, if any (cleared when read).
    pub fn integrity_error(&self) -> Option<String> {
        self.read_error.lock().unwrap_or_else(|x| x.into_inner()).take()
    }

    // -------------------------------------------------------------- reads

    pub fn node(&self, id: u64) -> Option<NodeRef> {
        self.note(node_record(self.pager(), id).map(|r| {
            r.map(|(labels, props)| NodeRef { id, labels, props })
        }))
    }

    pub fn edge(&self, id: u64) -> Option<EdgeRef> {
        self.note(edge_record(self.pager(), id).map(|r| {
            r.map(|(from, to, etype, props)| EdgeRef {
                id,
                from,
                to,
                etype,
                props,
            })
        }))
    }

    fn keys_of(&self, t: Tree, prefix: &[u8], at: usize) -> Vec<u64> {
        self.note((|| -> SResult<Vec<u64>> {
            let mut out = Vec::new();
            let mut c = t.seek(self.pager(), prefix)?;
            while let Some((k, _)) = c.next()? {
                if !k.starts_with(prefix) {
                    break;
                }
                out.push(keys::get_u64(k, at));
            }
            Ok(out)
        })())
    }

    /// Every node id, ascending. Materialises the list; for very large
    /// graphs prefer [`Graph::nodes_from`].
    pub fn node_ids(&self) -> Vec<u64> {
        self.keys_of(NODES, &[], 0)
    }

    pub fn edge_ids(&self) -> Vec<u64> {
        self.keys_of(EDGES, &[], 0)
    }

    /// Up to `limit` node ids at or after `from`, ascending: pages through
    /// the graph without materialising it.
    pub fn nodes_from(&self, from: u64, limit: usize) -> Vec<u64> {
        self.note((|| -> SResult<Vec<u64>> {
            let mut out = Vec::new();
            let mut c = NODES.seek(self.pager(), &node_key(from))?;
            while let Some((k, _)) = c.next()? {
                if out.len() >= limit {
                    break;
                }
                out.push(keys::get_u64(k, 0));
            }
            Ok(out)
        })())
    }

    pub fn edges_from(&self, from: u64, limit: usize) -> Vec<u64> {
        self.note((|| -> SResult<Vec<u64>> {
            let mut out = Vec::new();
            let mut c = EDGES.seek(self.pager(), &node_key(from))?;
            while let Some((k, _)) = c.next()? {
                if out.len() >= limit {
                    break;
                }
                out.push(keys::get_u64(k, 0));
            }
            Ok(out)
        })())
    }

    /// Visit a node's adjacency: outgoing then incoming, each in ascending
    /// edge id.
    pub fn for_each_adj(&self, id: u64, dir: Dir, etype: Option<u32>, mut f: impl FnMut(Adj)) {
        let sides: &[u8] = match dir {
            Dir::Out => &[OUT],
            Dir::In => &[IN],
            Dir::Both => &[OUT, IN],
        };
        for &d in sides {
            let r = (|| -> SResult<()> {
                let mut prefix = [0u8; 9];
                prefix[..8].copy_from_slice(&id.to_be_bytes());
                prefix[8] = d;
                let mut c = ADJ.seek(self.pager(), &prefix)?;
                while let Some((k, v)) = c.next()? {
                    if !k.starts_with(&prefix) {
                        break;
                    }
                    let v = match v {
                        crate::storage::btree::Val::Inline(b) => b,
                        _ => return Err(SError::Corrupt("adjacency value overflowed".into())),
                    };
                    let mut r = codec::Reader::new(v);
                    let t = r.varint().map_err(SError::Corrupt)? as u32;
                    if etype.map(|x| x != t).unwrap_or(false) {
                        continue;
                    }
                    let other = r.varint().map_err(SError::Corrupt)?;
                    f(Adj {
                        edge: keys::get_u64(k, 9),
                        other,
                        etype: t,
                    });
                }
                Ok(())
            })();
            self.note(r);
        }
    }

    pub fn neighbors(&self, id: u64, dir: Dir, etype: Option<u32>) -> Vec<Adj> {
        let mut out = Vec::new();
        self.for_each_adj(id, dir, etype, |a| out.push(a));
        out
    }

    pub fn degree(&self, id: u64, dir: Dir) -> usize {
        let mut n = 0;
        self.for_each_adj(id, dir, None, |_| n += 1);
        n
    }

    pub fn nodes_with_label(&self, label: &str) -> Vec<u64> {
        match self.strings.lookup(label) {
            Some(l) => self.label_member_ids(l),
            None => Vec::new(),
        }
    }

    /// Up to `limit` members of a label with id >= `from`, ascending.
    pub fn label_members_from(&self, l: u32, from: u64, limit: usize) -> Vec<u64> {
        self.note((|| -> SResult<Vec<u64>> {
            let prefix = l.to_be_bytes();
            let mut out = Vec::new();
            let mut c = LABELS.seek(self.pager(), &pair_key(l, from))?;
            while let Some((k, _)) = c.next()? {
                if !k.starts_with(&prefix) || out.len() >= limit {
                    break;
                }
                out.push(keys::get_u64(k, 4));
            }
            Ok(out)
        })())
    }

    /// Up to `limit` edges of a type with id >= `from`, ascending.
    pub fn type_members_from(&self, t: u32, from: u64, limit: usize) -> Vec<u64> {
        self.note((|| -> SResult<Vec<u64>> {
            let prefix = t.to_be_bytes();
            let mut out = Vec::new();
            let mut c = TYPES.seek(self.pager(), &pair_key(t, from))?;
            while let Some((k, _)) = c.next()? {
                if !k.starts_with(&prefix) || out.len() >= limit {
                    break;
                }
                out.push(keys::get_u64(k, 4));
            }
            Ok(out)
        })())
    }

    /// How many edges have a type. Reads one counter.
    pub fn type_count(&self, etype: &str) -> usize {
        match self.strings.lookup(etype) {
            Some(t) => self.note(get_count(self.pager(), &cat_key(b'T', t))) as usize,
            None => 0,
        }
    }

    /// At most `limit` adjacency entries, in `for_each_adj` order, without
    /// reading the rest of a hub's list.
    pub fn neighbors_limited(&self, id: u64, dir: Dir, limit: usize) -> Vec<Adj> {
        let mut out = Vec::new();
        let sides: &[Dir] = match dir {
            Dir::Both => &[Dir::Out, Dir::In],
            d => std::slice::from_ref(match d {
                Dir::Out => &Dir::Out,
                _ => &Dir::In,
            }),
        };
        for d in sides {
            let r = (|| -> SResult<()> {
                let mut prefix = [0u8; 9];
                prefix[..8].copy_from_slice(&id.to_be_bytes());
                prefix[8] = if matches!(d, Dir::Out) { OUT } else { IN };
                let mut c = ADJ.seek(self.pager(), &prefix)?;
                while let Some((k, v)) = c.next()? {
                    if !k.starts_with(&prefix) || out.len() >= limit {
                        break;
                    }
                    let v = v.load(self.pager())?;
                    let mut r = codec::Reader::new(&v);
                    let etype = r.varint().map_err(SError::Corrupt)? as u32;
                    let other = r.varint().map_err(SError::Corrupt)?;
                    out.push(Adj {
                        edge: keys::get_u64(k, 9),
                        other,
                        etype,
                    });
                }
                Ok(())
            })();
            self.note(r);
        }
        out
    }

    pub fn label_member_ids(&self, l: u32) -> Vec<u64> {
        self.keys_of(LABELS, &l.to_be_bytes(), 4)
    }

    pub fn edges_with_type(&self, etype: &str) -> Vec<u64> {
        match self.strings.lookup(etype) {
            Some(t) => self.keys_of(TYPES, &t.to_be_bytes(), 4),
            None => Vec::new(),
        }
    }

    /// Node ids an index maps `value` to, ascending; None without an index.
    pub fn indexed_lookup(&self, label: &str, key: &str, value: &Value) -> Option<Vec<u64>> {
        let l = self.strings.lookup(label)?;
        let k = self.strings.lookup(key)?;
        let def = *self.indexes.get(&(l, k))?;
        Some(self.note(self.index_scan(def.id, k, value, usize::MAX)))
    }

    fn index_scan(&self, index: u32, key: u32, value: &Value, cap: usize) -> SResult<Vec<u64>> {
        let p = self.pager();
        let (prefix, truncated) = idx_prefix(p, index, value);
        let mut out = Vec::new();
        let mut c = PROPS.seek(p, &prefix)?;
        while let Some((k, _)) = c.next()? {
            if !k.starts_with(&prefix) || out.len() >= cap {
                break;
            }
            if k.len() != prefix.len() + 8 {
                continue;
            }
            let node = keys::get_u64(k, prefix.len());
            if truncated {
                // Several long values share a cut-down key: check this one.
                let same = node_record(p, node)?
                    .and_then(|(_, run)| codec::find_prop(&run, key).ok().flatten())
                    .map(|v| v.total_cmp(value) == std::cmp::Ordering::Equal)
                    .unwrap_or(false);
                if !same {
                    continue;
                }
            }
            out.push(node);
        }
        Ok(out)
    }

    /// How many nodes carry a label. Reads one counter.
    pub fn label_count(&self, label: &str) -> usize {
        match self.strings.lookup(label) {
            Some(l) => self.note(get_count(self.pager(), &cat_key(b'L', l))) as usize,
            None => 0,
        }
    }

    /// Size of one index bucket, counted up to a cap: the planner's
    /// estimate. None without an index.
    pub fn index_count(&self, label: &str, key: &str, value: &Value) -> Option<usize> {
        let l = self.strings.lookup(label)?;
        let k = self.strings.lookup(key)?;
        let def = *self.indexes.get(&(l, k))?;
        Some(self.note(self.index_scan(def.id, k, value, COUNT_CAP)).len())
    }

    pub fn has_index(&self, label: &str, key: &str) -> bool {
        match (self.strings.lookup(label), self.strings.lookup(key)) {
            (Some(l), Some(k)) => self.indexes.contains_key(&(l, k)),
            _ => false,
        }
    }

    /// Every index as (label, key, distinct values indexed).
    pub fn indexes(&self) -> Vec<(String, String, usize)> {
        let mut out: Vec<_> = self
            .indexes
            .keys()
            .map(|(l, k)| {
                let distinct = self
                    .note(index_stats(self.pager(), *l, *k))
                    .map(|(_, d, _)| d)
                    .unwrap_or(0);
                (
                    self.strings.name(*l).to_string(),
                    self.strings.name(*k).to_string(),
                    distinct as usize,
                )
            })
            .collect();
        out.sort();
        out
    }

    pub fn node_prop(&self, id: u64, key: &str) -> Option<Value> {
        let k = self.strings.lookup(key)?;
        self.node(id)?.prop(k)
    }

    pub fn edge_prop(&self, id: u64, key: &str) -> Option<Value> {
        let k = self.strings.lookup(key)?;
        self.edge(id)?.prop(k)
    }

    pub fn node_labels(&self, id: u64) -> Vec<String> {
        match self.node(id) {
            Some(n) => n
                .labels()
                .iter()
                .map(|l| self.strings.name(*l).to_string())
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn has_label(&self, id: u64, label: u32) -> bool {
        self.node(id).map(|n| n.has_label(label)).unwrap_or(false)
    }

    pub fn node_props(&self, id: u64) -> Vec<(String, Value)> {
        match self.node(id) {
            Some(n) => n
                .props()
                .iter()
                .map(|(k, v)| (self.strings.name(*k).to_string(), v.clone()))
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn edge_props(&self, id: u64) -> Vec<(String, Value)> {
        match self.edge(id) {
            Some(e) => e
                .props()
                .iter()
                .map(|(k, v)| (self.strings.name(*k).to_string(), v.clone()))
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn edge_type_name(&self, id: u64) -> Option<&str> {
        let t = self.edge(id)?.etype;
        Some(self.strings.name(t))
    }

    /// Property keys seen on up to `per` members of each label and each
    /// relationship type, sorted. A sample, not a census: it is for hints
    /// such as autocomplete, and stays cheap on a graph of any size.
    pub fn sample_keys(&self, per: usize) -> (KeySample, KeySample) {
        let names = |ids: BTreeSet<u32>| -> Vec<String> {
            let mut v: Vec<String> = ids.into_iter().map(|k| self.strings.name(k).to_string()).collect();
            v.sort();
            v
        };
        let node_keys = |ids: Vec<u64>| {
            let mut keys = BTreeSet::new();
            for id in ids {
                if let Some(n) = self.node(id) {
                    keys.extend(n.props().iter().map(|(k, _)| *k));
                }
            }
            keys
        };
        let first = |t: Tree, prefix: &[u8], at: usize| -> Vec<u64> {
            self.note((|| -> SResult<Vec<u64>> {
                let mut out = Vec::new();
                let mut c = t.seek(self.pager(), prefix)?;
                while let Some((k, _)) = c.next()? {
                    if !k.starts_with(prefix) || out.len() >= per {
                        break;
                    }
                    out.push(keys::get_u64(k, at));
                }
                Ok(out)
            })())
        };
        let mut nodes = vec![(String::new(), names(node_keys(self.nodes_from(0, per))))];
        for (l, _) in self.counts(b'L') {
            let ids = first(LABELS, &l.to_be_bytes(), 4);
            nodes.push((self.strings.name(l).to_string(), names(node_keys(ids))));
        }
        let mut edges = Vec::new();
        for (t, _) in self.counts(b'T') {
            let ids = first(TYPES, &t.to_be_bytes(), 4);
            let mut keys = BTreeSet::new();
            for id in ids {
                if let Some(e) = self.edge(id) {
                    keys.extend(e.props().iter().map(|(k, _)| *k));
                }
            }
            edges.push((self.strings.name(t).to_string(), names(keys)));
        }
        nodes.sort();
        edges.sort();
        (nodes, edges)
    }

    /// (id, count) for every label ('L') or type ('T') in the catalog.
    fn counts(&self, tag: u8) -> Vec<(u32, u64)> {
        self.note((|| -> SResult<Vec<(u32, u64)>> {
            let mut out = Vec::new();
            let mut c = CATALOG.seek(self.pager(), &[tag])?;
            while let Some((k, v)) = c.next()? {
                if k[0] != tag {
                    break;
                }
                let n = u64::from_le_bytes(v.load(self.pager())?[..8].try_into().unwrap());
                out.push((keys::get_u32(k, 1), n));
            }
            Ok(out)
        })())
    }

    // ------------------------------------------------------- transactions

    pub fn uncommitted(&self) -> u64 {
        self.uncommitted
    }

    /// Make the open transaction durable (per the sync mode) and visible as
    /// the committed state.
    pub fn commit(&mut self) -> Result<()> {
        if let Err(e) = self.db.commit() {
            self.rollback_quietly();
            return Err(e.into());
        }
        self.uncommitted = 0;
        Ok(())
    }

    /// Abandon everything since the last commit.
    pub fn rollback(&mut self) -> Result<()> {
        self.db.rollback()?;
        (self.strings, self.indexes) = load_schema(self.pager())?;
        self.uncommitted = 0;
        Ok(())
    }

    fn rollback_quietly(&mut self) {
        let _ = self.db.rollback();
        if let Ok(s) = load_schema(self.pager()) {
            (self.strings, self.indexes) = s;
        }
        self.uncommitted = 0;
    }

    fn maybe_commit(&mut self) -> Result<()> {
        if self.autocommit {
            self.commit()
        } else {
            Ok(())
        }
    }

    /// Log and apply one op. A failure rolls the whole open transaction
    /// back, so nothing is left half-done.
    fn exec(&mut self, op: Op) -> Result<()> {
        let rec = op_record(&op);
        let r = self
            .db
            .log(&rec)
            .and_then(|_| {
                let mut sch = Schema {
                    strings: &mut self.strings,
                    indexes: &mut self.indexes,
                };
                apply(self.db.pager(), &mut sch, &op)
            });
        if let Err(e) = r {
            self.rollback_quietly();
            let e: Error = e.into();
            return Err(match e {
                Error::Full(m) => Error::Full(format!("{m} (the transaction was rolled back)")),
                other => other,
            });
        }
        self.uncommitted += 1;
        self.maybe_commit()
    }

    fn node_exists(&self, id: u64) -> bool {
        self.note(NODES.contains(self.pager(), &node_key(id)))
    }

    fn edge_exists(&self, id: u64) -> bool {
        self.note(EDGES.contains(self.pager(), &node_key(id)))
    }

    // ---------------------------------------------------------- bulk load

    /// Build a new database at `path` from a stream of nodes and edges, far
    /// faster than inserting them one by one: nodes and edges arrive in id
    /// order and go straight into bottom-up tree builders; adjacency, label,
    /// type and index entries go through external sorts (spilling to
    /// `<db>-tmp/` in `work_mem`-sized runs) and are built the same way.
    /// Memory stays near `cache_size + work_mem` at any size.
    ///
    /// `path` must not exist. The load bypasses the write-ahead log: if it
    /// is interrupted, delete the file and run it again.
    pub fn bulk_load(
        path: &Path,
        src: &dyn crate::legacy::image::ImageSource,
        opts: OpenOptions,
        work_mem: usize,
    ) -> Result<Graph> {
        use crate::storage::btree::Builder;
        use crate::storage::extsort::Sorter;
        if std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false) {
            return Err(Error::Msg(format!("{} already exists", path.display())));
        }
        let mut g = Graph::open_opts(path, opts)?;
        let tmp = {
            let mut t = path.as_os_str().to_os_string();
            t.push("-tmp");
            std::path::PathBuf::from(t)
        };
        let io = |e: std::io::Error| Error::Io(e);
        let r = (|| -> Result<()> {
            let p = g.db.pager();
            // Strings and index definitions.
            let strings = src.strings();
            for (i, s) in strings.iter().enumerate() {
                CATALOG.put(p, &cat_key(b'S', i as u32), s.as_bytes())?;
            }
            let defs = src.indexes();
            let per = (work_mem / 4).max(1 << 20);
            let mut labels_s = Sorter::new(&tmp, "labels", per).map_err(io)?;
            let mut types_s = Sorter::new(&tmp, "types", per).map_err(io)?;
            let mut adj_s = Sorter::new(&tmp, "adj", per).map_err(io)?;
            let mut props_s = Sorter::new(&tmp, "props", per).map_err(io)?;
            let mut label_counts: HashMap<u32, u64> = HashMap::new();
            let mut type_counts: HashMap<u32, u64> = HashMap::new();
            let mut err: Option<Error> = None;
            let (mut n, mut m, mut max_node, mut max_edge) = (0u64, 0u64, 0u64, 0u64);

            // Nodes.
            let mut b = Builder::new(p, NODES, 100);
            src.nodes(&mut |id, labels, props| {
                if err.is_some() {
                    return;
                }
                let r = (|| -> Result<()> {
                    let mut ls = labels.to_vec();
                    dedup(&mut ls);
                    b.push(&node_key(id), &encode_node(&ls, props))?;
                    for l in &ls {
                        labels_s.push(&pair_key(*l, id), &[]).map_err(io)?;
                        *label_counts.entry(*l).or_default() += 1;
                    }
                    for (i, (l, k)) in defs.iter().enumerate() {
                        if !ls.contains(l) {
                            continue;
                        }
                        if let Some(v) = get_prop(props, *k) {
                            let (mut key, truncated) = idx_prefix(p, i as u32, v);
                            key.extend_from_slice(&id.to_be_bytes());
                            let mut val = vec![truncated as u8];
                            if truncated {
                                keys::put_value(&mut val, v);
                            }
                            props_s.push(&key, &val).map_err(io)?;
                        }
                    }
                    n += 1;
                    max_node = max_node.max(id);
                    if n % 1_000_000 == 0 {
                        p.commit();
                    }
                    Ok(())
                })();
                if let Err(e) = r {
                    err = Some(e);
                }
            })
            .map_err(io)?;
            if let Some(e) = err.take() {
                return Err(e);
            }
            b.finish()?;
            p.commit();

            // Edges.
            let mut b = Builder::new(p, EDGES, 100);
            src.edges(&mut |id, from, to, t, props| {
                if err.is_some() {
                    return;
                }
                let r = (|| -> Result<()> {
                    b.push(&node_key(id), &encode_edge(from, to, t, props))?;
                    adj_s.push(&adj_key(from, OUT, id), &adj_val(t, to)).map_err(io)?;
                    adj_s.push(&adj_key(to, IN, id), &adj_val(t, from)).map_err(io)?;
                    types_s.push(&pair_key(t, id), &[]).map_err(io)?;
                    *type_counts.entry(t).or_default() += 1;
                    m += 1;
                    max_edge = max_edge.max(id);
                    if m % 1_000_000 == 0 {
                        p.commit();
                    }
                    Ok(())
                })();
                if let Err(e) = r {
                    err = Some(e);
                }
            })
            .map_err(io)?;
            if let Some(e) = err.take() {
                return Err(e);
            }
            b.finish()?;
            p.commit();

            // Sorted trees.
            for (sorter, tree) in [(adj_s, ADJ), (labels_s, LABELS), (types_s, TYPES)] {
                let mut it = sorter.finish().map_err(io)?;
                let mut b = Builder::new(p, tree, 100);
                let mut k = 0u64;
                while let Some((key, val)) = it.next().map_err(io)? {
                    b.push(&key, &val)?;
                    k += 1;
                    if k % 1_000_000 == 0 {
                        p.commit();
                    }
                }
                b.finish()?;
                p.commit();
            }
            // Property indexes, counting distinct values per index on the
            // way (entries arrive grouped by index, then value).
            let mut stats: Vec<(u64, u64)> = vec![(0, 0); defs.len()];
            {
                let mut it = props_s.finish().map_err(io)?;
                let mut b = Builder::new(p, PROPS, 100);
                let mut last: Option<(u32, Vec<u8>)> = None;
                let mut k = 0u64;
                while let Some((key, val)) = it.next().map_err(io)? {
                    let index = keys::get_u32(&key, 0);
                    // The value's identity: its full encoding.
                    let ident = if val.first() == Some(&1) {
                        val[1..].to_vec()
                    } else {
                        key[4..key.len() - 8].to_vec()
                    };
                    let st = &mut stats[index as usize];
                    st.1 += 1;
                    if last.as_ref().map(|(i, v)| *i != index || *v != ident).unwrap_or(true) {
                        st.0 += 1;
                    }
                    last = Some((index, ident));
                    b.push(&key, &val[..1])?;
                    k += 1;
                    if k % 1_000_000 == 0 {
                        p.commit();
                    }
                }
                b.finish()?;
            }
            for (i, (l, k)) in defs.iter().enumerate() {
                put_index_stats(p, *l, *k, i as u32, stats[i].0, stats[i].1)?;
            }
            for (l, c) in &label_counts {
                CATALOG.put(p, &cat_key(b'L', *l), &c.to_le_bytes())?;
            }
            for (t, c) in &type_counts {
                CATALOG.put(p, &cat_key(b'T', *t), &c.to_le_bytes())?;
            }
            let (next_node, next_edge) = src.next_ids();
            p.set_meta(M_NEXT_NODE, next_node.max(max_node + 1));
            p.set_meta(M_NEXT_EDGE, next_edge.max(max_edge + 1));
            p.set_meta(M_NODES, n);
            p.set_meta(M_EDGES, m);
            p.set_meta(M_NEXT_INDEX, defs.len() as u64);
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        r?;
        g.db.commit()?;
        g.db.checkpoint()?;
        (g.strings, g.indexes) = load_schema(g.db.pager())?;
        Ok(g)
    }

    // ---------------------------------------------------------- mutations

    /// Apply one logged operation with its explicit ids: for loaders and
    /// migrations that must reproduce ids exactly.
    #[doc(hidden)]
    pub fn apply_op(&mut self, op: Op) -> Result<()> {
        self.exec(op)
    }

    pub fn add_node(&mut self, labels: &[String], props: Vec<(String, Value)>) -> Result<u64> {
        let id = self.pager().meta(M_NEXT_NODE);
        self.exec(Op::NodeAdd {
            id,
            labels: labels.to_vec(),
            props,
        })?;
        Ok(id)
    }

    pub fn add_edge(&mut self, from: u64, to: u64, etype: &str, props: Vec<(String, Value)>) -> Result<u64> {
        if !self.node_exists(from) {
            return Err(Error::Msg(format!("no node {}", from)));
        }
        if !self.node_exists(to) {
            return Err(Error::Msg(format!("no node {}", to)));
        }
        let id = self.pager().meta(M_NEXT_EDGE);
        self.exec(Op::EdgeAdd {
            id,
            from,
            to,
            etype: etype.to_string(),
            props,
        })?;
        Ok(id)
    }

    pub fn delete_node(&mut self, id: u64) -> Result<bool> {
        if !self.node_exists(id) {
            return Ok(false);
        }
        self.exec(Op::NodeDel { id })?;
        Ok(true)
    }

    pub fn delete_edge(&mut self, id: u64) -> Result<bool> {
        if !self.edge_exists(id) {
            return Ok(false);
        }
        self.exec(Op::EdgeDel { id })?;
        Ok(true)
    }

    pub fn set_node_prop(&mut self, id: u64, key: &str, value: Value) -> Result<()> {
        if !self.node_exists(id) {
            return Err(Error::Msg(format!("no node {}", id)));
        }
        self.exec(Op::NodeSet {
            id,
            key: key.to_string(),
            value,
        })
    }

    pub fn unset_node_prop(&mut self, id: u64, key: &str) -> Result<()> {
        self.exec(Op::NodeUnset {
            id,
            key: key.to_string(),
        })
    }

    pub fn set_edge_prop(&mut self, id: u64, key: &str, value: Value) -> Result<()> {
        if !self.edge_exists(id) {
            return Err(Error::Msg(format!("no edge {}", id)));
        }
        self.exec(Op::EdgeSet {
            id,
            key: key.to_string(),
            value,
        })
    }

    pub fn unset_edge_prop(&mut self, id: u64, key: &str) -> Result<()> {
        self.exec(Op::EdgeUnset {
            id,
            key: key.to_string(),
        })
    }

    pub fn add_label(&mut self, id: u64, label: &str) -> Result<()> {
        if !self.node_exists(id) {
            return Err(Error::Msg(format!("no node {}", id)));
        }
        self.exec(Op::LabelAdd {
            id,
            label: label.to_string(),
        })
    }

    pub fn remove_label(&mut self, id: u64, label: &str) -> Result<()> {
        self.exec(Op::LabelDel {
            id,
            label: label.to_string(),
        })
    }

    pub fn create_index(&mut self, label: &str, key: &str) -> Result<()> {
        self.exec(Op::IndexAdd {
            label: label.to_string(),
            key: key.to_string(),
        })
    }

    pub fn drop_index(&mut self, label: &str, key: &str) -> Result<()> {
        self.exec(Op::IndexDel {
            label: label.to_string(),
            key: key.to_string(),
        })
    }

    pub fn clear(&mut self) -> Result<()> {
        self.exec(Op::Clear)
    }

    /// Reclaim space: folds the log into the pages. (Pages freed by deletes
    /// are reused by later writes; the file does not shrink.)
    pub fn compact(&mut self) -> Result<u64> {
        let before = self.file_len();
        self.commit()?;
        self.db.checkpoint()?;
        Ok(before)
    }

    // -------------------------------------------------------------- stats

    pub fn stats(&self) -> Stats {
        let name = |x: u32| self.strings.name(x).to_string();
        let mut labels: Vec<(String, usize)> = self
            .counts(b'L')
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .map(|(l, n)| (name(l), n as usize))
            .collect();
        let mut edge_types: Vec<(String, usize)> = self
            .counts(b'T')
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .map(|(t, n)| (name(t), n as usize))
            .collect();
        labels.sort();
        edge_types.sort();
        let ps = self.pager().stats();
        Stats {
            nodes: self.node_count(),
            edges: self.edge_count(),
            labels,
            edge_types,
            interned: self.strings.len(),
            file_bytes: self.file_len(),
            indexes: self.indexes(),
            memory: self.pager().memory_usage(),
            cache_pages: ps.resident_pages,
            page_size: self.pager().page_size() as u64,
            log_bytes: self.db.log_since_checkpoint(),
            integrity_error: None,
        }
    }

    // ---------------------------------------------------------------- csr

    /// Flatten into compressed sparse row form for the in-memory algorithm
    /// tier. Holds the whole projection in RAM.
    pub fn csr(&self, dir: Dir, etype: Option<u32>, weight: Option<&str>) -> Csr {
        let ids = self.node_ids();
        let mut pos: IdMap<u32> = id_map();
        pos.reserve(ids.len());
        for (i, id) in ids.iter().enumerate() {
            pos.insert(*id, i as u32);
        }
        let wkey = weight.and_then(|w| self.strings.lookup(w));
        let mut off = Vec::with_capacity(ids.len() + 1);
        let mut adj: Vec<u32> = Vec::new();
        let mut eids: Vec<u64> = Vec::new();
        let mut w: Vec<f64> = Vec::new();
        off.push(0u32);
        for id in &ids {
            self.for_each_adj(*id, dir, etype, |a| {
                let Some(p) = pos.get(&a.other) else { return };
                adj.push(*p);
                eids.push(a.edge);
                w.push(match wkey {
                    Some(k) => self
                        .edge(a.edge)
                        .and_then(|e| e.prop(k))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(1.0),
                    None => 1.0,
                });
            });
            off.push(adj.len() as u32);
        }
        Csr {
            ids,
            pos,
            off,
            adj,
            eids,
            weights: w,
        }
    }

    /// Memory an algorithm may hold right now before it spills: `work_mem`
    /// for a file, the headroom under `max_memory` for `:memory:`.
    pub fn algorithm_memory(&self) -> u64 {
        match self.pager().memory_usage() {
            Some((used, max)) => max.saturating_sub(used),
            None => self.work_mem,
        }
    }

    /// Act on a replicator's pin now (keep unshipped log, start or end a
    /// hold for a base snapshot) instead of at the next commit. For a
    /// writer that may sit idle; does nothing inside a transaction.
    pub fn poll_replication(&mut self) -> Result<()> {
        if self.uncommitted > 0 {
            return Ok(());
        }
        Ok(self.db.poll_pin(false)?)
    }

    pub fn set_work_mem(&mut self, bytes: u64) {
        self.work_mem = bytes;
    }

    /// Where algorithms and queries spill: `<db>-tmp/` beside a file,
    /// the system temp directory for `:memory:`.
    pub fn temp_dir(&self) -> std::path::PathBuf {
        match self.path() {
            Some(p) => {
                let mut t = p.as_os_str().to_owned();
                t.push("-tmp");
                t.into()
            }
            // Spill files are uniquely named and removed when dropped.
            None => crate::ooc::system_temp_dir(),
        }
    }

    /// Roughly what `csr()` would occupy for this projection.
    pub fn csr_bytes(&self, dir: Dir) -> u64 {
        let n = self.node_count() as u64;
        let m = self.edge_count() as u64 * if dir == Dir::Both { 2 } else { 1 };
        // ids + position map (with hash overhead) + offsets; per entry a
        // target, an edge id and a weight.
        n * (8 + 24 + 4) + m * (4 + 8 + 8)
    }

    /// The graph as algorithms see it. In memory (a [`Csr`]) when it fits
    /// the algorithm budget (see [`crate::ooc::with_budget`]), else read
    /// from the pages as it goes. `tier` forces one or the other.
    pub fn projection(&self, dir: Dir, etype: Option<u32>, weight: Option<&str>, tier: Tier) -> Projection<'_> {
        let est = self.csr_bytes(dir);
        let mem = match tier {
            Tier::Mem => true,
            Tier::Ooc => false,
            // Leave at least as much again for per-node state.
            Tier::Auto => crate::ooc::budget_left().map(|b| est.saturating_mul(2) <= b).unwrap_or(true),
        };
        if mem {
            crate::ooc::take(est);
            return Projection::Mem(self.csr(dir, etype, weight), est);
        }
        Projection::Paged(PagedView::new(self, dir, etype, weight))
    }

    /// Structural check of every tree: order, bounds, ownership, overflow
    /// chains. Returns the number of entries seen.
    pub fn verify_trees(&self) -> Result<u64> {
        let mut n = 0;
        for t in [CATALOG, NODES, EDGES, ADJ, LABELS, TYPES, PROPS] {
            n += t.check(self.pager())?.entries;
        }
        Ok(n)
    }

    /// Page-cache and transaction statistics.
    pub fn pager_stats(&self) -> pager::Stats {
        self.pager().stats()
    }
}

pub struct Stats {
    pub nodes: usize,
    pub edges: usize,
    pub labels: Vec<(String, usize)>,
    pub edge_types: Vec<(String, usize)>,
    pub interned: usize,
    pub file_bytes: u64,
    pub indexes: Vec<(String, String, usize)>,
    /// `:memory:`: (bytes of pages held, max_memory).
    pub memory: Option<(u64, u64)>,
    /// Pages resident in memory right now.
    pub cache_pages: u64,
    pub page_size: u64,
    /// Log a crash right now would replay.
    pub log_bytes: u64,
    pub integrity_error: Option<String>,
}

// ------------------------------------------------------ algorithm projections

/// Which representation an algorithm runs over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// In memory when it fits the budget, else from the pages.
    Auto,
    Mem,
    Ooc,
}

impl Tier {
    pub fn parse(s: &str) -> Option<Tier> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Tier::Auto),
            "mem" | "memory" => Some(Tier::Mem),
            "ooc" | "disk" => Some(Tier::Ooc),
            _ => None,
        }
    }
}

/// A graph as the algorithms see it: an in-memory [`Csr`] or a view that
/// reads adjacency from the page store as it goes. Both number nodes by
/// ascending id and list neighbours in the same order, so every algorithm
/// gives the same answer on either.
pub enum Projection<'g> {
    /// The CSR and the bytes it charged to the algorithm budget.
    Mem(Csr, u64),
    Paged(PagedView<'g>),
}

impl Projection<'_> {
    pub fn is_paged(&self) -> bool {
        matches!(self, Projection::Paged(_))
    }

    /// The node id at dense position `v`.
    pub fn id(&self, v: usize) -> u64 {
        match self {
            Projection::Mem(c, _) => c.ids[v],
            Projection::Paged(p) => p.ids.borrow_mut().get(v),
        }
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        match self {
            Projection::Mem(c, _) => c.index_of(id),
            Projection::Paged(p) => p.index_of(id),
        }
    }

    /// The same nodes with every edge reversed.
    pub fn reversed(&self) -> Projection<'_> {
        match self {
            Projection::Mem(c, _) => {
                let est = (c.adj.len() * 20 + c.ids.len() * 36) as u64;
                crate::ooc::take(est);
                Projection::Mem(c.reversed(), est)
            }
            Projection::Paged(p) => Projection::Paged(PagedView {
                g: p.g,
                dir: match p.dir {
                    Dir::Out => Dir::In,
                    Dir::In => Dir::Out,
                    Dir::Both => Dir::Both,
                },
                etype: p.etype,
                wkey: p.wkey,
                ids: p.ids.clone(),
                len: p.len,
            }),
        }
    }

    /// Dense position -> node id, detached from the graph (so the graph can
    /// be written to while results stream out).
    pub fn into_ids(mut self) -> crate::ooc::StateVec<u64> {
        let rc = match &mut self {
            Projection::Mem(c, _) => return crate::ooc::StateVec::from_vec(std::mem::take(&mut c.ids)),
            Projection::Paged(p) => p.ids.clone(),
        };
        drop(self);
        match std::rc::Rc::try_unwrap(rc) {
            Ok(cell) => cell.into_inner(),
            Err(rc) => {
                let mut ids = rc.borrow_mut();
                let mut out = crate::ooc::StateVec::new(ids.len(), 0u64);
                for i in 0..ids.len() {
                    out.set(i, ids.get(i));
                }
                out
            }
        }
    }
}

impl Drop for Projection<'_> {
    fn drop(&mut self) {
        if let Projection::Mem(_, est) = self {
            crate::ooc::give(*est);
        }
    }
}

impl crate::algo::Adjacency for Projection<'_> {
    fn len(&self) -> usize {
        match self {
            Projection::Mem(c, _) => c.len(),
            Projection::Paged(p) => p.len,
        }
    }
    fn degree(&self, v: usize) -> usize {
        match self {
            Projection::Mem(c, _) => c.degree(v),
            Projection::Paged(p) => p.degree(v),
        }
    }
    fn each(&self, v: usize, f: &mut dyn FnMut(usize, f64, u64)) {
        match self {
            Projection::Mem(c, _) => c.each(v, f),
            Projection::Paged(p) => p.each(v, f),
        }
    }
    fn targets(&self, v: usize) -> Vec<usize> {
        match self {
            Projection::Mem(c, _) => crate::algo::Adjacency::targets(c, v),
            Projection::Paged(p) => {
                let mut out = Vec::new();
                p.each(v, &mut |t, _, _| out.push(t));
                out
            }
        }
    }
    fn has_negative_weights(&self) -> bool {
        match self {
            Projection::Mem(c, _) => crate::algo::Adjacency::has_negative_weights(c),
            Projection::Paged(p) => (0..p.len).any(|v| {
                let mut neg = false;
                p.each(v, &mut |_, w, _| neg |= w < 0.0);
                neg
            }),
        }
    }
}

/// Adjacency read straight from the ADJ tree. Holds only the dense
/// position -> id array (a [`StateVec`](crate::ooc::StateVec), so it spills
/// too); everything else comes through the page cache.
pub struct PagedView<'g> {
    g: &'g Graph,
    dir: Dir,
    etype: Option<u32>,
    wkey: Option<u32>,
    ids: std::rc::Rc<std::cell::RefCell<crate::ooc::StateVec<u64>>>,
    len: usize,
}

impl<'g> PagedView<'g> {
    pub fn new(g: &'g Graph, dir: Dir, etype: Option<u32>, weight: Option<&str>) -> PagedView<'g> {
        // The node count sizes the array; a damaged file whose tree
        // disagrees is cut or padded to it (integrity_error reports it).
        let n = g.node_count();
        let mut ids = crate::ooc::StateVec::new(n, 0u64);
        let mut len = 0usize;
        let mut from = 0u64;
        'scan: loop {
            let page = g.nodes_from(from, 4096);
            let Some(&last) = page.last() else { break };
            for id in page {
                if len == n {
                    break 'scan;
                }
                ids.set(len, id);
                len += 1;
            }
            from = last + 1;
        }
        let len = len.min(n);
        PagedView {
            g,
            dir,
            etype,
            wkey: weight.and_then(|w| g.strings.lookup(w)),
            ids: std::rc::Rc::new(std::cell::RefCell::new(ids)),
            len,
        }
    }

    /// Binary search of the ascending id array, trying the dense guess
    /// first (ids are usually close to contiguous).
    pub fn index_of(&self, id: u64) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let mut ids = self.ids.borrow_mut();
        let first = ids.get(0);
        if id < first {
            return None;
        }
        let guess = id - first;
        if guess < self.len as u64 && ids.get(guess as usize) == id {
            return Some(guess as usize);
        }
        let (mut lo, mut hi) = (0usize, self.len.min(guess as usize + 1));
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match ids.get(mid).cmp(&id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    fn degree(&self, v: usize) -> usize {
        let id = self.ids.borrow_mut().get(v);
        let mut n = 0;
        self.g.for_each_adj(id, self.dir, self.etype, |_| n += 1);
        n
    }

    fn each(&self, v: usize, f: &mut dyn FnMut(usize, f64, u64)) {
        let id = self.ids.borrow_mut().get(v);
        let mut list: Vec<(u64, u64)> = Vec::new();
        self.g.for_each_adj(id, self.dir, self.etype, |a| list.push((a.other, a.edge)));
        for (other, edge) in list {
            let Some(t) = self.index_of(other) else { continue };
            let w = match self.wkey {
                Some(k) => self
                    .g
                    .edge(edge)
                    .and_then(|e| e.prop(k))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0),
                None => 1.0,
            };
            f(t, w, edge);
        }
    }
}
