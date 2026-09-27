//! The legacy engine (formats v1–v3), kept to read and migrate old files and
//! as the reference the paged engine is tested against.
//!
//! A labelled property graph: a snapshot image loaded from the
//! front of the file (`image`), plus an in-memory delta of everything changed
//! since, backed by the append-only log in `store`.
//!
//! Shape of the data:
//!   node  = id + set of labels + flat property map + adjacency
//!   edge  = id + from + to + one type + flat property map
//!
//! Labels, edge types and property keys are interned to `u32`, so the per-node
//! cost is a couple of small vectors rather than a pile of Strings.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use super::image::{self, Base, ChunkScratch, ImageSource, Residency};
use crate::store::{Op, Store, Sync};
pub use crate::types::*;
use crate::value::Value;

// -------------------------------------------------------------- graph types

/// A node held in the delta: created since the image was written, or copied
/// out of the image in order to change its labels or properties.
#[derive(Clone, Debug, Default)]
pub(crate) struct DNode {
    labels: Vec<u32>,
    props: Vec<(u32, Value)>,
}

/// An edge created since the image was written.
#[derive(Clone, Debug)]
pub(crate) struct DEdge {
    from: u64,
    to: u64,
    etype: u32,
    props: Vec<(u32, Value)>,
}

/// A read-only view of one node, wherever it lives. Cheap to copy; borrows
/// the graph, so it cannot outlive the next mutation.
#[derive(Clone, Copy)]
pub struct NodeRef<'g> {
    pub id: u64,
    src: NodeSrc<'g>,
}

#[derive(Clone, Copy)]
enum NodeSrc<'g> {
    Delta(&'g DNode),
    Base(&'g Base, u32),
}

impl<'g> NodeRef<'g> {
    /// Interned label ids.
    pub fn labels(&self) -> &'g [u32] {
        match self.src {
            NodeSrc::Delta(n) => &n.labels,
            NodeSrc::Base(b, s) => b.node_labels(s),
        }
    }

    pub fn has_label(&self, label: u32) -> bool {
        self.labels().contains(&label)
    }

    /// Every property, keyed by interned id. Borrowed when the node is in
    /// the delta, decoded when it is in the image.
    pub fn props(&self) -> Cow<'g, [(u32, Value)]> {
        match self.src {
            NodeSrc::Delta(n) => Cow::Borrowed(&n.props),
            NodeSrc::Base(b, s) => Cow::Owned(b.node_props(s)),
        }
    }

    /// One property. From the image this decodes only the matching value.
    pub fn prop(&self, key: u32) -> Option<Value> {
        match self.src {
            NodeSrc::Delta(n) => get_prop(&n.props, key).cloned(),
            NodeSrc::Base(b, s) => b.node_prop(s, key),
        }
    }
}

/// A read-only view of one edge. Endpoints and type are plain fields.
#[derive(Clone, Copy)]
pub struct EdgeRef<'g> {
    pub id: u64,
    pub from: u64,
    pub to: u64,
    pub etype: u32,
    src: EdgeSrc<'g>,
}

#[derive(Clone, Copy)]
enum EdgeSrc<'g> {
    Props(&'g [(u32, Value)]),
    Base(&'g Base, u32),
}

impl<'g> EdgeRef<'g> {
    pub fn props(&self) -> Cow<'g, [(u32, Value)]> {
        match self.src {
            EdgeSrc::Props(p) => Cow::Borrowed(p),
            EdgeSrc::Base(b, s) => Cow::Owned(b.edge_props(s)),
        }
    }

    pub fn prop(&self, key: u32) -> Option<Value> {
        match self.src {
            EdgeSrc::Props(p) => get_prop(p, key).cloned(),
            EdgeSrc::Base(b, s) => b.edge_prop(s, key),
        }
    }
}

fn set_prop(props: &mut Vec<(u32, Value)>, key: u32, value: Value) {
    match props.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value,
        None => props.push((key, value)),
    }
}

/// One bit per image slot.
#[derive(Clone, Default)]
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Bits {
        Bits(vec![0; n.div_ceil(64)])
    }

    #[inline]
    fn get(&self, i: u32) -> bool {
        self.0
            .get(i as usize / 64)
            .map(|w| (w >> (i % 64)) & 1 == 1)
            .unwrap_or(false)
    }

    /// Set a bit; true if it was clear before.
    fn set(&mut self, i: u32) -> bool {
        let w = &mut self.0[i as usize / 64];
        let bit = 1u64 << (i % 64);
        let was = *w & bit != 0;
        *w |= bit;
        !was
    }
}

// -------------------------------------------------------------------- graph

/// Rewrite the file once the log after the image grows past this, or past
/// the image itself if that is larger. Bounds open time to roughly one image
/// load plus one image's worth of replay.
pub const DEFAULT_AUTO_COMPACT: u64 = 64 << 20;

/// How to open a database file. `Default` is what `Graph::open` uses.
#[derive(Clone, Copy, Debug)]
pub struct OpenOptions {
    pub sync: Sync,
    /// Break a lock left behind by a writer that is definitely gone.
    pub force: bool,
    /// Where property values live once the image is loaded.
    pub residency: Residency,
    /// Compact automatically after a commit once the log tail exceeds this
    /// many bytes (or the image size, if larger). `None` turns it off.
    pub auto_compact: Option<u64>,
    /// Load and checksum the whole image at open, instead of each part on
    /// first use. Open then costs a full read of the file, but a damaged
    /// image is refused up front rather than reported by the query that
    /// first touches the damage.
    pub preload: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            sync: Sync::Normal,
            force: false,
            residency: Residency::Memory,
            auto_compact: Some(DEFAULT_AUTO_COMPACT),
            preload: false,
        }
    }
}

/// A property index: the part covered by the image (`base`, an index into
/// `Base::indexes`) plus a map for nodes in the delta. An index created after
/// the image has no base part and maps every member itself.
struct PIndex {
    base: Option<usize>,
    map: BTreeMap<VKey, Vec<u64>>,
}

/// The graph is two layers.
///
/// `base` is the snapshot image the last compaction wrote: flat columns,
/// loaded in bulk, never modified. Everything since lives in the *delta*:
/// nodes and edges created since, copies of image nodes whose labels or
/// properties changed, and bitmasks over image slots for what was changed or
/// deleted. Reads consult the delta first and fall through to the image.
///
/// A graph without an image — in memory, or a log that has never been
/// compacted — is an empty base and a delta holding everything, which is
/// exactly how the engine worked before images existed.
pub struct Graph {
    store: Option<Store>,
    pub strings: Interner,
    base: Base,
    /// New nodes, and copied image nodes (whose image slot is masked).
    nodes: IdMap<DNode>,
    /// New edges only. Image edges are changed in place through `edge_props`
    /// and `edge_dead`, since an edge's endpoints and type never change.
    edges: IdMap<DEdge>,
    /// Replacement property lists for image edges.
    edge_props: IdMap<Vec<(u32, Value)>>,
    /// Adjacency of delta edges, keyed by endpoint (image or delta node).
    out: IdMap<Vec<Adj>>,
    inc: IdMap<Vec<Adj>>,
    /// Image node slots that are deleted or copied into `nodes`.
    node_masked: Bits,
    masked_nodes: usize,
    edge_dead: Bits,
    dead_edges: usize,
    /// Label membership of delta nodes.
    label_index: HashMap<u32, IdSet>,
    /// Per label, how many image members are masked.
    label_masked: HashMap<u32, usize>,
    /// Type membership of delta edges.
    type_index: HashMap<u32, IdSet>,
    /// Per type, how many image edges are deleted.
    type_dead: HashMap<u32, usize>,
    prop_indexes: HashMap<(u32, u32), PIndex>,
    next_node: u64,
    next_edge: u64,
    uncommitted: u64,
    /// Commit after every mutating statement. Turn off for bulk loads.
    pub autocommit: bool,
    residency: Residency,
    auto_compact: Option<u64>,
    auto_compact_error: Option<String>,
}

impl Graph {
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

    /// Open with every knob exposed.
    ///
    /// Opening reads the snapshot image's header and directory — the rest of
    /// the image loads as it is first used — then replays only the log
    /// written after it. Replay builds state one record at a time and is the
    /// slow part, which is why the file is compacted automatically once the
    /// tail grows.
    pub fn open_opts(path: &Path, opts: OpenOptions) -> Result<Graph> {
        let opening = Store::begin_open(path, opts.sync, opts.force)?;
        let mut g = Graph::memory();
        g.residency = opts.residency;
        g.auto_compact = opts.auto_compact;
        let h = opening.header();
        if h.image_len > 0 {
            let (base, strings) = image::load_file(path, h.image_at, h.image_len, opts.residency)?;
            if opts.preload {
                base.preload().map_err(Error::Msg)?;
            }
            g.strings = Interner::from_list(strings);
            g.install_base(base);
        }
        // Apply each op as it is decoded, rather than collecting them first:
        // holding every decoded op at once cost more than the graph itself.
        let store = {
            let sink = &mut g;
            opening.replay(&mut |op| sink.apply_mem(&op))?
        };
        g.store = Some(store);
        Ok(g)
    }

    /// An in-memory graph loaded from the bytes of a database file. Nothing
    /// is written back: edits live only as long as the graph. This is how
    /// hosts without a filesystem (wasm) open a `.gldb`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Graph> {
        let mut g = Graph::memory();
        let h = crate::store::parse_header(bytes)?;
        if h.image_len > 0 {
            let image = usize::try_from(h.image_at)
                .ok()
                .zip(usize::try_from(h.image_at + h.image_len).ok())
                .and_then(|(a, b)| bytes.get(a..b))
                .ok_or_else(|| Error::Msg("image extends past the end of the bytes".into()))?;
            let (base, strings) = image::load_slice(image)?;
            g.strings = Interner::from_list(strings);
            g.install_base(base);
        }
        crate::store::replay_bytes(bytes, &mut |op| g.apply_mem(&op))?;
        Ok(g)
    }

    /// A purely in-memory graph. Nothing is persisted.
    pub fn memory() -> Graph {
        Graph {
            store: None,
            strings: Interner::default(),
            base: Base::empty(),
            nodes: id_map(),
            edges: id_map(),
            edge_props: id_map(),
            out: id_map(),
            inc: id_map(),
            node_masked: Bits::default(),
            masked_nodes: 0,
            edge_dead: Bits::default(),
            dead_edges: 0,
            label_index: HashMap::new(),
            label_masked: HashMap::new(),
            type_index: HashMap::new(),
            type_dead: HashMap::new(),
            prop_indexes: HashMap::new(),
            next_node: 1,
            next_edge: 1,
            uncommitted: 0,
            autocommit: true,
            residency: Residency::Memory,
            auto_compact: None,
            auto_compact_error: None,
        }
    }

    pub fn is_persistent(&self) -> bool {
        self.store.is_some()
    }

    pub fn path(&self) -> Option<&Path> {
        self.store.as_ref().map(|s| s.path())
    }

    pub fn file_len(&self) -> u64 {
        self.store.as_ref().map(|s| s.file_len()).unwrap_or(0)
    }

    pub fn set_sync(&mut self, sync: Sync) {
        if let Some(s) = &mut self.store {
            s.sync = sync;
        }
    }

    /// Compact automatically once the log tail passes `bytes` (or the image
    /// size, if larger). `None` turns it off.
    pub fn set_auto_compact(&mut self, bytes: Option<u64>) {
        self.auto_compact = bytes;
    }

    /// Force everything committed so far down to the platter, whatever the
    /// sync mode says. An open transaction is left open and unwritten.
    ///
    /// Call this when the host app is about to lose control of its own
    /// lifetime: iOS `sceneDidEnterBackground`, Android `onStop`. A suspended
    /// app can be killed without another callback, and under `Sync::Normal`
    /// the last commits are still sitting in the OS page cache.
    pub fn checkpoint(&mut self) -> Result<()> {
        if let Some(s) = &mut self.store {
            s.flush()?;
        }
        Ok(())
    }

    pub fn node_count(&self) -> usize {
        self.base.n() - self.masked_nodes + self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.base.m() - self.dead_edges + self.edges.len()
    }

    #[inline]
    fn base_live_node(&self, id: u64) -> Option<u32> {
        let s = self.base.node_slot(id)?;
        (!self.node_masked.get(s)).then_some(s)
    }

    #[inline]
    fn base_live_edge(&self, id: u64) -> Option<u32> {
        let s = self.base.edge_slot(id)?;
        (!self.edge_dead.get(s)).then_some(s)
    }

    #[inline]
    fn node_exists(&self, id: u64) -> bool {
        self.nodes.contains_key(&id) || self.base_live_node(id).is_some()
    }

    #[inline]
    fn edge_exists(&self, id: u64) -> bool {
        self.edges.contains_key(&id) || self.base_live_edge(id).is_some()
    }

    pub fn node(&self, id: u64) -> Option<NodeRef<'_>> {
        if let Some(n) = self.nodes.get(&id) {
            return Some(NodeRef {
                id,
                src: NodeSrc::Delta(n),
            });
        }
        let s = self.base_live_node(id)?;
        Some(NodeRef {
            id,
            src: NodeSrc::Base(&self.base, s),
        })
    }

    pub fn edge(&self, id: u64) -> Option<EdgeRef<'_>> {
        if let Some(e) = self.edges.get(&id) {
            return Some(EdgeRef {
                id,
                from: e.from,
                to: e.to,
                etype: e.etype,
                src: EdgeSrc::Props(&e.props),
            });
        }
        let s = self.base_live_edge(id)?;
        Some(EdgeRef {
            id,
            from: self.base.edge_from(s),
            to: self.base.edge_to(s),
            etype: self.base.edge_type(s),
            src: match self.edge_props.get(&id) {
                Some(p) => EdgeSrc::Props(p),
                None => EdgeSrc::Base(&self.base, s),
            },
        })
    }

    pub fn node_ids(&self) -> Vec<u64> {
        let mut v = Vec::with_capacity(self.node_count());
        let ids = self.base.node_ids();
        if self.masked_nodes == 0 {
            v.extend_from_slice(ids);
        } else {
            v.extend(
                ids.iter()
                    .enumerate()
                    .filter(|(s, _)| !self.node_masked.get(*s as u32))
                    .map(|(_, id)| *id),
            );
        }
        if !self.nodes.is_empty() {
            v.extend(self.nodes.keys().copied());
            v.sort_unstable();
        }
        v
    }

    pub fn edge_ids(&self) -> Vec<u64> {
        let mut v = Vec::with_capacity(self.edge_count());
        let ids = self.base.edge_ids();
        if self.dead_edges == 0 {
            v.extend_from_slice(ids);
        } else {
            v.extend(
                ids.iter()
                    .enumerate()
                    .filter(|(s, _)| !self.edge_dead.get(*s as u32))
                    .map(|(_, id)| *id),
            );
        }
        if !self.edges.is_empty() {
            v.extend(self.edges.keys().copied());
            v.sort_unstable();
        }
        v
    }

    /// The first damage found in the snapshot image, if any. Parts of the
    /// image load on first use, so this can appear after open; once it does,
    /// `query::execute` refuses to return results and compaction refuses to
    /// run, so damage is reported rather than served or rewritten.
    pub fn integrity_error(&self) -> Option<String> {
        self.base.integrity_error().map(|s| s.to_string())
    }

    /// Load and checksum every part of the image now.
    pub fn preload(&self) -> Result<()> {
        self.base.preload().map_err(Error::Msg)
    }

    // --------------------------------------------------------- transactions

    fn log(&mut self, op: &Op) {
        if let Some(s) = &mut self.store {
            s.push(op);
            self.uncommitted += 1;
        }
    }

    pub fn commit(&mut self) -> Result<()> {
        let mut wrote = false;
        if let Some(s) = &mut self.store {
            wrote = s.pending_ops() > 0;
            s.commit()?;
        }
        self.uncommitted = 0;
        if wrote {
            self.maybe_auto_compact();
        }
        Ok(())
    }

    /// Compaction after a commit is housekeeping: the commit already
    /// succeeded, so a failure here is recorded (see `stats`) rather than
    /// reported as a failed write. The old file is intact either way.
    ///
    /// Only a commit that appended something gets here, so reading a large
    /// old log never rewrites it; the first write does.
    fn maybe_auto_compact(&mut self) {
        let (Some(min), Some(s)) = (self.auto_compact, &self.store) else {
            return;
        };
        let tail = s.file_len().saturating_sub(s.header_len());
        if tail > min.max(self.base.image_bytes) {
            self.auto_compact_error = self.compact().err().map(|e| e.to_string());
        }
    }

    fn maybe_commit(&mut self) -> Result<()> {
        if self.autocommit {
            self.commit()
        } else {
            Ok(())
        }
    }

    pub fn uncommitted(&self) -> u64 {
        self.uncommitted
    }

    // ---------------------------------------------------------- mutation api

    pub fn add_node(&mut self, labels: &[String], props: Vec<(String, Value)>) -> Result<u64> {
        let id = self.next_node;
        let op = Op::NodeAdd {
            id,
            labels: labels.to_vec(),
            props,
        };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()?;
        Ok(id)
    }

    pub fn add_edge(
        &mut self,
        from: u64,
        to: u64,
        etype: &str,
        props: Vec<(String, Value)>,
    ) -> Result<u64> {
        if !self.node_exists(from) {
            return Err(Error::Msg(format!("no node {}", from)));
        }
        if !self.node_exists(to) {
            return Err(Error::Msg(format!("no node {}", to)));
        }
        let id = self.next_edge;
        let op = Op::EdgeAdd {
            id,
            from,
            to,
            etype: etype.to_string(),
            props,
        };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()?;
        Ok(id)
    }

    pub fn delete_node(&mut self, id: u64) -> Result<bool> {
        if !self.node_exists(id) {
            return Ok(false);
        }
        let op = Op::NodeDel { id };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()?;
        Ok(true)
    }

    pub fn delete_edge(&mut self, id: u64) -> Result<bool> {
        if !self.edge_exists(id) {
            return Ok(false);
        }
        let op = Op::EdgeDel { id };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()?;
        Ok(true)
    }

    pub fn set_node_prop(&mut self, id: u64, key: &str, value: Value) -> Result<()> {
        if !self.node_exists(id) {
            return Err(Error::Msg(format!("no node {}", id)));
        }
        let op = Op::NodeSet {
            id,
            key: key.to_string(),
            value,
        };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()
    }

    pub fn unset_node_prop(&mut self, id: u64, key: &str) -> Result<()> {
        let op = Op::NodeUnset {
            id,
            key: key.to_string(),
        };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()
    }

    pub fn set_edge_prop(&mut self, id: u64, key: &str, value: Value) -> Result<()> {
        if !self.edge_exists(id) {
            return Err(Error::Msg(format!("no edge {}", id)));
        }
        let op = Op::EdgeSet {
            id,
            key: key.to_string(),
            value,
        };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()
    }

    pub fn unset_edge_prop(&mut self, id: u64, key: &str) -> Result<()> {
        let op = Op::EdgeUnset {
            id,
            key: key.to_string(),
        };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()
    }

    pub fn add_label(&mut self, id: u64, label: &str) -> Result<()> {
        if !self.node_exists(id) {
            return Err(Error::Msg(format!("no node {}", id)));
        }
        let op = Op::LabelAdd {
            id,
            label: label.to_string(),
        };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()
    }

    pub fn remove_label(&mut self, id: u64, label: &str) -> Result<()> {
        let op = Op::LabelDel {
            id,
            label: label.to_string(),
        };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()
    }

    pub fn create_index(&mut self, label: &str, key: &str) -> Result<()> {
        let op = Op::IndexAdd {
            label: label.to_string(),
            key: key.to_string(),
        };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()
    }

    pub fn drop_index(&mut self, label: &str, key: &str) -> Result<()> {
        let op = Op::IndexDel {
            label: label.to_string(),
            key: key.to_string(),
        };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()
    }

    pub fn clear(&mut self) -> Result<()> {
        let op = Op::Clear;
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()
    }

    /// Every index as (label, key, distinct values currently indexed).
    pub fn indexes(&self) -> Vec<(String, String, usize)> {
        let mut out: Vec<(String, String, usize)> = self
            .prop_indexes
            .iter()
            .map(|((l, k), p)| {
                (
                    self.strings.name(*l).to_string(),
                    self.strings.name(*k).to_string(),
                    self.index_key_count(p),
                )
            })
            .collect();
        out.sort();
        out
    }

    fn index_key_count(&self, p: &PIndex) -> usize {
        let Some(i) = p.base else {
            return p.map.len();
        };
        let live = |slots: &[u32]| slots.iter().any(|s| !self.node_masked.get(*s));
        let base_keys = if self.masked_nodes == 0 {
            self.base.index_key_count(i)
        } else {
            self.base.index_buckets(i).filter(|b| live(b)).count()
        };
        base_keys
            + p.map
                .keys()
                .filter(|k| !live(self.base.index_lookup(i, &k.0)))
                .count()
    }

    pub(crate) fn index_defs(&self) -> Vec<(u32, u32)> {
        let mut v: Vec<(u32, u32)> = self.prop_indexes.keys().copied().collect();
        v.sort_unstable();
        v
    }

    // ------------------------------------------------------- memory mutation

    /// Swap in a freshly loaded image. The delta is emptied: everything it
    /// held is in the image now.
    fn install_base(&mut self, base: Base) {
        self.nodes = id_map();
        self.edges = id_map();
        self.edge_props = id_map();
        self.out = id_map();
        self.inc = id_map();
        self.node_masked = Bits::new(base.n());
        self.masked_nodes = 0;
        self.edge_dead = Bits::new(base.m());
        self.dead_edges = 0;
        self.label_index.clear();
        self.label_masked.clear();
        self.type_index.clear();
        self.type_dead.clear();
        self.prop_indexes = base
            .indexes
            .iter()
            .enumerate()
            .map(|(i, x)| {
                (
                    (x.label, x.key),
                    PIndex {
                        base: Some(i),
                        map: BTreeMap::new(),
                    },
                )
            })
            .collect();
        self.next_node = self.next_node.max(base.next_node);
        self.next_edge = self.next_edge.max(base.next_edge);
        self.base = base;
    }

    /// Mark an image node slot as no longer authoritative — deleted, or
    /// copied into the delta. Idempotent.
    fn mask_base_node(&mut self, s: u32) {
        if self.node_masked.set(s) {
            self.masked_nodes += 1;
            for l in self.base.node_labels(s) {
                *self.label_masked.entry(*l).or_default() += 1;
            }
        }
    }

    /// Make sure a node is in the delta, copying it out of the image if it is
    /// only there. False if there is no such node.
    fn cow_node(&mut self, id: u64) -> bool {
        if self.nodes.contains_key(&id) {
            return true;
        }
        let Some(s) = self.base_live_node(id) else {
            return false;
        };
        let labels = self.base.node_labels(s).to_vec();
        let props = self.base.node_props(s);
        self.mask_base_node(s);
        for l in &labels {
            self.label_index.entry(*l).or_insert_with(id_set).insert(id);
        }
        self.nodes.insert(id, DNode { labels, props });
        true
    }

    /// Every edge touching a node, from both layers. Self-loops appear twice.
    fn incident_edges(&self, id: u64) -> Vec<u64> {
        let mut v = Vec::new();
        self.for_each_adj(id, Dir::Both, None, |a| v.push(a.edge));
        v
    }

    /// Apply an op to in-memory state only. Used by both the public API and
    /// log replay, so there is exactly one implementation of each mutation.
    fn apply_mem(&mut self, op: &Op) {
        match op {
            Op::NodeAdd { id, labels, props } => {
                let label_ids: Vec<u32> = labels.iter().map(|l| self.strings.intern(l)).collect();
                let prop_ids: Vec<(u32, Value)> = props
                    .iter()
                    .map(|(k, v)| (self.strings.intern(k), v.clone()))
                    .collect();
                // Re-adding an id that exists replaces its labels and
                // properties. Normal use never does this; hand-built logs can.
                if self.node_exists(*id) {
                    self.deindex_node(*id);
                    if let Some(old) = self.nodes.remove(id) {
                        for l in old.labels {
                            if let Some(set) = self.label_index.get_mut(&l) {
                                set.remove(id);
                            }
                        }
                    }
                    if let Some(s) = self.base.node_slot(*id) {
                        self.mask_base_node(s);
                    }
                }
                for l in &label_ids {
                    self.label_index
                        .entry(*l)
                        .or_insert_with(id_set)
                        .insert(*id);
                }
                self.nodes.insert(
                    *id,
                    DNode {
                        labels: label_ids,
                        props: prop_ids,
                    },
                );
                if *id >= self.next_node {
                    self.next_node = id + 1;
                }
                self.index_node(*id);
            }
            Op::NodeDel { id } => {
                if !self.node_exists(*id) {
                    return;
                }
                self.deindex_node(*id);
                for e in self.incident_edges(*id) {
                    self.remove_edge_mem(e);
                }
                if let Some(n) = self.nodes.remove(id) {
                    for l in n.labels {
                        if let Some(set) = self.label_index.get_mut(&l) {
                            set.remove(id);
                        }
                    }
                }
                if let Some(s) = self.base.node_slot(*id) {
                    self.mask_base_node(s);
                }
                self.out.remove(id);
                self.inc.remove(id);
            }
            Op::EdgeAdd {
                id,
                from,
                to,
                etype,
                props,
            } => {
                if !self.node_exists(*from) || !self.node_exists(*to) {
                    return;
                }
                let t = self.strings.intern(etype);
                let prop_ids: Vec<(u32, Value)> = props
                    .iter()
                    .map(|(k, v)| (self.strings.intern(k), v.clone()))
                    .collect();
                self.out.entry(*from).or_default().push(Adj {
                    edge: *id,
                    other: *to,
                    etype: t,
                });
                self.inc.entry(*to).or_default().push(Adj {
                    edge: *id,
                    other: *from,
                    etype: t,
                });
                self.type_index.entry(t).or_insert_with(id_set).insert(*id);
                self.edges.insert(
                    *id,
                    DEdge {
                        from: *from,
                        to: *to,
                        etype: t,
                        props: prop_ids,
                    },
                );
                if *id >= self.next_edge {
                    self.next_edge = id + 1;
                }
            }
            Op::EdgeDel { id } => self.remove_edge_mem(*id),
            Op::NodeSet { id, key, value } => {
                let k = self.strings.intern(key);
                if !self.cow_node(*id) {
                    return;
                }
                self.deindex_node(*id);
                if let Some(n) = self.nodes.get_mut(id) {
                    set_prop(&mut n.props, k, value.clone());
                }
                self.index_node(*id);
            }
            Op::NodeUnset { id, key } => {
                let Some(k) = self.strings.lookup(key) else {
                    return;
                };
                if self.node(*id).and_then(|n| n.prop(k)).is_none() || !self.cow_node(*id) {
                    return;
                }
                self.deindex_node(*id);
                if let Some(n) = self.nodes.get_mut(id) {
                    n.props.retain(|(pk, _)| *pk != k);
                }
                self.index_node(*id);
            }
            Op::EdgeSet { id, key, value } => {
                let k = self.strings.intern(key);
                if let Some(e) = self.edges.get_mut(id) {
                    set_prop(&mut e.props, k, value.clone());
                } else if let Some(s) = self.base_live_edge(*id) {
                    let base = &self.base;
                    let props = self
                        .edge_props
                        .entry(*id)
                        .or_insert_with(|| base.edge_props(s));
                    set_prop(props, k, value.clone());
                }
            }
            Op::EdgeUnset { id, key } => {
                let Some(k) = self.strings.lookup(key) else {
                    return;
                };
                if let Some(e) = self.edges.get_mut(id) {
                    e.props.retain(|(pk, _)| *pk != k);
                } else if let Some(s) = self.base_live_edge(*id) {
                    if !self.edge_props.contains_key(id) && self.base.edge_prop(s, k).is_none() {
                        return;
                    }
                    let base = &self.base;
                    let props = self
                        .edge_props
                        .entry(*id)
                        .or_insert_with(|| base.edge_props(s));
                    props.retain(|(pk, _)| *pk != k);
                }
            }
            Op::LabelAdd { id, label } => {
                let l = self.strings.intern(label);
                if self.node(*id).map(|n| n.has_label(l)).unwrap_or(true) || !self.cow_node(*id) {
                    return;
                }
                self.deindex_node(*id);
                if let Some(n) = self.nodes.get_mut(id) {
                    n.labels.push(l);
                }
                self.label_index.entry(l).or_insert_with(id_set).insert(*id);
                self.index_node(*id);
            }
            Op::LabelDel { id, label } => {
                let Some(l) = self.strings.lookup(label) else {
                    return;
                };
                if !self.node(*id).map(|n| n.has_label(l)).unwrap_or(false) || !self.cow_node(*id) {
                    return;
                }
                self.deindex_node(*id);
                if let Some(n) = self.nodes.get_mut(id) {
                    n.labels.retain(|x| *x != l);
                }
                if let Some(set) = self.label_index.get_mut(&l) {
                    set.remove(id);
                }
                self.index_node(*id);
            }
            Op::IndexAdd { label, key } => {
                let l = self.strings.intern(label);
                let k = self.strings.intern(key);
                if self.prop_indexes.contains_key(&(l, k)) {
                    return;
                }
                let mut map: BTreeMap<VKey, Vec<u64>> = BTreeMap::new();
                for id in self.label_member_ids(l) {
                    if let Some(v) = self.node(id).and_then(|n| n.prop(k)) {
                        map.entry(VKey(v)).or_default().push(id);
                    }
                }
                self.prop_indexes.insert((l, k), PIndex { base: None, map });
            }
            Op::IndexDel { label, key } => {
                if let (Some(l), Some(k)) = (self.strings.lookup(label), self.strings.lookup(key)) {
                    self.prop_indexes.remove(&(l, k));
                }
            }
            Op::Counters {
                next_node,
                next_edge,
            } => {
                self.next_node = self.next_node.max(*next_node);
                self.next_edge = self.next_edge.max(*next_edge);
            }
            Op::Clear => {
                // Index definitions survive a clear; their contents do not.
                let defs = self.index_defs();
                self.install_base(Base::empty());
                for d in defs {
                    self.prop_indexes.insert(
                        d,
                        PIndex {
                            base: None,
                            map: BTreeMap::new(),
                        },
                    );
                }
                self.next_node = 1;
                self.next_edge = 1;
            }
        }
    }

    fn remove_edge_mem(&mut self, id: u64) {
        if let Some(e) = self.edges.remove(&id) {
            if let Some(list) = self.out.get_mut(&e.from) {
                list.retain(|a| a.edge != id);
            }
            if let Some(list) = self.inc.get_mut(&e.to) {
                list.retain(|a| a.edge != id);
            }
            if let Some(set) = self.type_index.get_mut(&e.etype) {
                set.remove(&id);
            }
            return;
        }
        if let Some(s) = self.base_live_edge(id) {
            self.edge_dead.set(s);
            self.dead_edges += 1;
            *self.type_dead.entry(self.base.edge_type(s)).or_default() += 1;
            self.edge_props.remove(&id);
        }
    }

    /// The (index, value) pairs a node contributes to property indexes.
    fn index_entries(&self, id: u64) -> Vec<((u32, u32), Value)> {
        let Some(node) = self.node(id) else {
            return Vec::new();
        };
        let delta = self.nodes.contains_key(&id);
        self.prop_indexes
            .iter()
            // An index with an image part covers image nodes itself; its map
            // holds only nodes that live in the delta.
            .filter(|(_, p)| delta || p.base.is_none())
            .filter(|((l, _), _)| node.has_label(*l))
            .filter_map(|((l, k), _)| node.prop(*k).map(|v| ((*l, *k), v)))
            .collect()
    }

    fn index_node(&mut self, id: u64) {
        for (key, v) in self.index_entries(id) {
            if let Some(p) = self.prop_indexes.get_mut(&key) {
                let list = p.map.entry(VKey(v)).or_default();
                if let Err(pos) = list.binary_search(&id) {
                    list.insert(pos, id);
                }
            }
        }
    }

    fn deindex_node(&mut self, id: u64) {
        for (key, v) in self.index_entries(id) {
            let Some(p) = self.prop_indexes.get_mut(&key) else {
                continue;
            };
            let vk = VKey(v);
            if let Some(list) = p.map.get_mut(&vk) {
                if let Ok(pos) = list.binary_search(&id) {
                    list.remove(pos);
                }
                if list.is_empty() {
                    p.map.remove(&vk);
                }
            }
        }
    }

    // -------------------------------------------------------------- lookups

    /// Property keys seen on up to `per` members of each label and each
    /// relationship type, sorted. A sample, not a census: it is for hints
    /// such as autocomplete, and must stay cheap on a graph of millions. The
    /// `""` label covers nodes regardless of label, so an unlabelled graph
    /// still has keys to offer.
    pub fn sample_keys(&self, per: usize) -> (KeySample, KeySample) {
        fn names(g: &Graph, ids: std::collections::BTreeSet<u32>) -> Vec<String> {
            let mut v: Vec<String> = ids
                .into_iter()
                .map(|k| g.strings.name(k).to_string())
                .collect();
            v.sort();
            v
        }
        let node_keys = |ids: &mut dyn Iterator<Item = u64>| {
            let mut keys = std::collections::BTreeSet::new();
            for id in ids {
                if let Some(n) = self.node(id) {
                    keys.extend(n.props().iter().map(|(k, _)| *k));
                }
            }
            keys
        };

        let mut nodes = Vec::new();
        let base_live = self
            .base
            .node_ids()
            .iter()
            .enumerate()
            .filter(|(s, _)| !self.node_masked.get(*s as u32))
            .map(|(_, id)| *id);
        let any = node_keys(&mut self.nodes.keys().copied().chain(base_live).take(per));
        nodes.push((String::new(), names(self, any)));
        for l in self.all_labels() {
            let mut ids = self.label_members(l).take(per);
            let keys = node_keys(&mut ids);
            nodes.push((self.strings.name(l).to_string(), names(self, keys)));
        }

        // Edge types: delta members first, then one pass over the image that
        // stops as soon as every type has its sample.
        let mut sample: HashMap<u32, Vec<u64>> = HashMap::new();
        for t in self.all_types() {
            let v: Vec<u64> = self
                .type_index
                .get(&t)
                .map(|s| s.iter().copied().take(per).collect())
                .unwrap_or_default();
            sample.insert(t, v);
        }
        let mut hungry = sample.values().filter(|v| v.len() < per).count();
        for (s, t) in self.base.edge_types().iter().enumerate() {
            if hungry == 0 {
                break;
            }
            if self.edge_dead.get(s as u32) {
                continue;
            }
            if let Some(v) = sample.get_mut(t) {
                if v.len() < per {
                    v.push(self.base.edge_id(s as u32));
                    if v.len() == per {
                        hungry -= 1;
                    }
                }
            }
        }
        let mut edges = Vec::new();
        for (t, ids) in sample {
            let mut keys = std::collections::BTreeSet::new();
            for id in ids {
                if let Some(e) = self.edge(id) {
                    keys.extend(e.props().iter().map(|(k, _)| *k));
                }
            }
            edges.push((self.strings.name(t).to_string(), names(self, keys)));
        }
        nodes.sort();
        edges.sort();
        (nodes, edges)
    }

    /// Labels with at least one live member.
    fn all_labels(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.base.labels().collect();
        v.extend(self.label_index.keys().copied());
        v.sort_unstable();
        v.dedup();
        v.retain(|l| self.label_count_id(*l) > 0);
        v
    }

    /// Edge types with at least one live edge.
    fn all_types(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.base.type_counts().keys().copied().collect();
        v.extend(self.type_index.keys().copied());
        v.sort_unstable();
        v.dedup();
        v.retain(|t| self.type_count_id(*t) > 0);
        v
    }

    /// Members of a label, delta first, in no particular order.
    fn label_members(&self, l: u32) -> impl Iterator<Item = u64> + '_ {
        let delta = self.label_index.get(&l).into_iter().flatten().copied();
        let base = self
            .base
            .label_members(l)
            .iter()
            .filter(move |s| !self.node_masked.get(**s))
            .map(move |s| self.base.node_id(*s));
        delta.chain(base)
    }

    /// Members of a label, ascending.
    pub(crate) fn label_member_ids(&self, l: u32) -> Vec<u64> {
        let mut v: Vec<u64> = self.label_members(l).collect();
        v.sort_unstable();
        v
    }

    fn label_count_id(&self, l: u32) -> usize {
        self.base.label_len(l) - self.label_masked.get(&l).copied().unwrap_or(0)
            + self.label_index.get(&l).map(|s| s.len()).unwrap_or(0)
    }

    fn type_count_id(&self, t: u32) -> usize {
        self.base.type_counts().get(&t).copied().unwrap_or(0)
            - self.type_dead.get(&t).copied().unwrap_or(0)
            + self.type_index.get(&t).map(|s| s.len()).unwrap_or(0)
    }

    pub fn nodes_with_label(&self, label: &str) -> Vec<u64> {
        match self.strings.lookup(label) {
            Some(l) => self.label_member_ids(l),
            None => Vec::new(),
        }
    }

    pub fn edges_with_type(&self, etype: &str) -> Vec<u64> {
        let Some(t) = self.strings.lookup(etype) else {
            return Vec::new();
        };
        let mut v: Vec<u64> = self
            .type_index
            .get(&t)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        if self.base.type_counts().contains_key(&t) {
            v.extend(
                self.base
                    .edge_types()
                    .iter()
                    .enumerate()
                    .filter(|(s, x)| **x == t && !self.edge_dead.get(*s as u32))
                    .map(|(s, _)| self.base.edge_id(s as u32)),
            );
        }
        v.sort_unstable();
        v
    }

    /// Image slots in an index bucket that are still authoritative.
    fn base_bucket(&self, p: &PIndex, value: &Value) -> impl Iterator<Item = u32> + '_ {
        let slots: &[u32] = match p.base {
            Some(i) => self.base.index_lookup(i, value),
            None => &[],
        };
        slots
            .iter()
            .copied()
            .filter(move |s| !self.node_masked.get(*s))
    }

    /// Exact-match lookup through a property index, if one exists.
    pub fn indexed_lookup(&self, label: &str, key: &str, value: &Value) -> Option<Vec<u64>> {
        let l = self.strings.lookup(label)?;
        let k = self.strings.lookup(key)?;
        let p = self.prop_indexes.get(&(l, k))?;
        let mut v: Vec<u64> = p.map.get(&VKey(value.clone())).cloned().unwrap_or_default();
        let before = v.len();
        v.extend(self.base_bucket(p, value).map(|s| self.base.node_id(s)));
        if before > 0 && v.len() > before {
            v.sort_unstable();
        }
        Some(v)
    }

    /// How many nodes carry a label, without materialising them. The planner
    /// asks this for every candidate anchor, so it must not allocate.
    pub fn label_count(&self, label: &str) -> usize {
        self.strings
            .lookup(label)
            .map(|l| self.label_count_id(l))
            .unwrap_or(0)
    }

    /// Size of one index bucket — the planner's estimate for an indexed
    /// equality. `None` means there is no index to use.
    pub fn index_count(&self, label: &str, key: &str, value: &Value) -> Option<usize> {
        let l = self.strings.lookup(label)?;
        let k = self.strings.lookup(key)?;
        let p = self.prop_indexes.get(&(l, k))?;
        let delta = p
            .map
            .get(&VKey(value.clone()))
            .map(|v| v.len())
            .unwrap_or(0);
        Some(delta + self.base_bucket(p, value).count())
    }

    pub fn has_index(&self, label: &str, key: &str) -> bool {
        match (self.strings.lookup(label), self.strings.lookup(key)) {
            (Some(l), Some(k)) => self.prop_indexes.contains_key(&(l, k)),
            _ => false,
        }
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

    /// Visit a node's adjacency without allocating: image edges first (in
    /// ascending edge id), then delta edges (in creation order, which is also
    /// ascending id). `Both` visits outgoing, then incoming.
    pub(crate) fn for_each_adj(
        &self,
        id: u64,
        dir: Dir,
        etype: Option<u32>,
        mut f: impl FnMut(Adj),
    ) {
        if !self.node_exists(id) {
            return;
        }
        let slot = self.base.node_slot(id);
        let sides: &[bool] = match dir {
            Dir::Out => &[true],
            Dir::In => &[false],
            Dir::Both => &[true, false],
        };
        for &out in sides {
            if let Some(s) = slot {
                for (nbr, e) in self.base.adj(s, out) {
                    if self.dead_edges > 0 && self.edge_dead.get(e) {
                        continue;
                    }
                    let t = self.base.edge_type(e);
                    if etype.map(|x| x != t).unwrap_or(false) {
                        continue;
                    }
                    f(Adj {
                        edge: self.base.edge_id(e),
                        other: self.base.node_id(nbr),
                        etype: t,
                    });
                }
            }
            let list = if out {
                self.out.get(&id)
            } else {
                self.inc.get(&id)
            };
            for a in list.into_iter().flatten() {
                if etype.map(|x| x == a.etype).unwrap_or(true) {
                    f(*a);
                }
            }
        }
    }

    /// Adjacency in a direction, optionally filtered to one edge type.
    pub fn neighbors(&self, id: u64, dir: Dir, etype: Option<u32>) -> Vec<Adj> {
        let mut out = Vec::new();
        self.for_each_adj(id, dir, etype, |a| out.push(a));
        out
    }

    pub fn degree(&self, id: u64, dir: Dir) -> usize {
        if !self.node_exists(id) {
            return 0;
        }
        if self.dead_edges == 0 {
            let slot = self.base.node_slot(id);
            let side = |out: bool| {
                slot.map(|s| self.base.degree(s, out)).unwrap_or(0)
                    + if out {
                        self.out.get(&id)
                    } else {
                        self.inc.get(&id)
                    }
                    .map(|l| l.len())
                    .unwrap_or(0)
            };
            return match dir {
                Dir::Out => side(true),
                Dir::In => side(false),
                Dir::Both => side(true) + side(false),
            };
        }
        let mut n = 0;
        self.for_each_adj(id, dir, None, |_| n += 1);
        n
    }

    // ------------------------------------------------------------ housekeeping

    /// Rewrite the file as a snapshot image of current state, with an empty
    /// log after it. Reclaims space from deletes and overwrites, and makes
    /// the next open a bulk load instead of a replay. Returns the file size
    /// before.
    pub fn compact(&mut self) -> Result<u64> {
        let before = self.file_len();
        if let Some(e) = self.integrity_error() {
            return Err(Error::Msg(format!(
                "refusing to compact a damaged image ({e}); restore from a replica \
                 or export what can still be read"
            )));
        }
        let Some(mut store) = self.store.take() else {
            return Ok(before);
        };
        let residency = self.residency;
        let res = store.commit().and_then(|_| {
            store.compact_image(
                |w| image::write(self, w).map(|_| ()),
                |path, h| {
                    // Read every byte back before the rename: what was
                    // written is what the next open will trust.
                    image::verify_file(path, h.image_at, h.image_len)?;
                    image::load_file(path, h.image_at, h.image_len, residency)
                },
            )
        });
        self.store = Some(store);
        let (base, _strings) = res?;
        self.uncommitted = 0;
        self.install_base(base);
        Ok(before)
    }

    /// Fold the delta into a fresh in-memory image, as a compaction would,
    /// without touching any file. For tests and benchmarks of the image
    /// path on graphs that have no file.
    #[doc(hidden)]
    pub fn rebase_in_memory(&mut self) -> Result<()> {
        let mut cur = std::io::Cursor::new(Vec::new());
        image::write(self, &mut cur)?;
        let (base, _strings) = image::load_slice(cur.get_ref())?;
        self.install_base(base);
        Ok(())
    }

    pub fn stats(&self) -> Stats {
        let label_counts: Vec<(String, usize)> = self
            .all_labels()
            .into_iter()
            .map(|l| (self.strings.name(l).to_string(), self.label_count_id(l)))
            .collect();
        let type_counts: Vec<(String, usize)> = self
            .all_types()
            .into_iter()
            .map(|t| (self.strings.name(t).to_string(), self.type_count_id(t)))
            .collect();
        let mut label_counts = label_counts;
        let mut type_counts = type_counts;
        label_counts.sort();
        type_counts.sort();
        let log_start = self.store.as_ref().map(|s| s.header_len()).unwrap_or(0);
        Stats {
            nodes: self.node_count(),
            edges: self.edge_count(),
            labels: label_counts,
            edge_types: type_counts,
            interned: self.strings.len(),
            file_bytes: self.file_len(),
            indexes: self.indexes(),
            image_bytes: self.base.image_bytes,
            tail_bytes: self.file_len().saturating_sub(log_start),
            props_on_disk: self.base.props_on_disk(),
            read_errors: self.base.read_errors(),
            auto_compact_error: self.auto_compact_error.clone(),
            integrity_error: self.integrity_error(),
        }
    }

    // ------------------------------------------------------------------ csr

    /// Flatten into compressed sparse row form: the layout every algorithm in
    /// `algo` runs over. Built fresh per call, which keeps mutation cheap and
    /// makes algorithm runs cache-friendly.
    pub fn csr(&self, dir: Dir, etype: Option<u32>, weight: Option<&str>) -> Csr {
        let clean = self.nodes.is_empty()
            && self.edges.is_empty()
            && self.edge_props.is_empty()
            && self.masked_nodes == 0
            && self.dead_edges == 0;
        if clean {
            return self.csr_from_image(dir, etype, weight);
        }
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
                let weight = match wkey {
                    Some(k) => self
                        .edge(a.edge)
                        .and_then(|e| e.prop(k))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(1.0),
                    None => 1.0,
                };
                w.push(weight);
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
}

impl Graph {
    /// `csr` when nothing has changed since the image: image slots are
    /// already dense indices in id order, so the projection is a filtered
    /// copy of the image's adjacency, with no per-node lookups.
    fn csr_from_image(&self, dir: Dir, etype: Option<u32>, weight: Option<&str>) -> Csr {
        let b = &self.base;
        let ids = b.node_ids().to_vec();
        let mut pos: IdMap<u32> = id_map();
        pos.reserve(ids.len());
        for (i, id) in ids.iter().enumerate() {
            pos.insert(*id, i as u32);
        }
        let wkey = weight.and_then(|w| self.strings.lookup(w));
        let sides: &[bool] = match dir {
            Dir::Out => &[true],
            Dir::In => &[false],
            Dir::Both => &[true, false],
        };
        let mut off = Vec::with_capacity(ids.len() + 1);
        let mut adj: Vec<u32> = Vec::with_capacity(b.m() * sides.len());
        let mut eids: Vec<u64> = Vec::with_capacity(b.m() * sides.len());
        let mut w: Vec<f64> = Vec::with_capacity(b.m() * sides.len());
        off.push(0u32);
        for s in 0..ids.len() as u32 {
            for &out in sides {
                for (nbr, e) in b.adj(s, out) {
                    if etype.map(|t| t != b.edge_type(e)).unwrap_or(false) {
                        continue;
                    }
                    adj.push(nbr);
                    eids.push(b.edge_id(e));
                    w.push(match wkey {
                        Some(k) => b.edge_prop(e, k).and_then(|v| v.as_f64()).unwrap_or(1.0),
                        None => 1.0,
                    });
                }
            }
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
}

pub struct Stats {
    pub nodes: usize,
    pub edges: usize,
    pub labels: Vec<(String, usize)>,
    pub edge_types: Vec<(String, usize)>,
    pub interned: usize,
    pub file_bytes: u64,
    pub indexes: Vec<(String, String, usize)>,
    /// Size of the snapshot image at the front of the file; 0 if none.
    pub image_bytes: u64,
    /// Log bytes after the image: what the next open has to replay.
    pub tail_bytes: u64,
    /// Whether property values are read from disk on demand.
    pub props_on_disk: bool,
    /// Property reads that failed (I/O or checksum) and read as empty.
    pub read_errors: u64,
    /// Why the last automatic compaction failed, if it did.
    pub auto_compact_error: Option<String>,
    /// The first damage found in the image, if any.
    pub integrity_error: Option<String>,
}

/// A graph is an image source: this is how compaction writes one, and
/// `image::build(path, &graph)` copies a graph into a new database file.
/// Image properties stream through a private chunk window rather than the
/// graph's cache, so writing does not load the whole image into memory.
impl ImageSource for Graph {
    fn strings(&self) -> Vec<String> {
        self.strings.all().to_vec()
    }

    fn next_ids(&self) -> (u64, u64) {
        (self.next_node, self.next_edge)
    }

    fn indexes(&self) -> Vec<(u32, u32)> {
        self.index_defs()
    }

    fn nodes(&self, f: &mut dyn FnMut(u64, &[u32], &[(u32, Value)])) -> std::io::Result<()> {
        let mut scratch = ChunkScratch::default();
        for id in self.node_ids() {
            if let Some(n) = self.nodes.get(&id) {
                f(id, &n.labels, &n.props);
            } else if let Some(s) = self.base_live_node(id) {
                let props = self.base.node_props_scratch(s, &mut scratch);
                f(id, self.base.node_labels(s), &props);
            }
        }
        Ok(())
    }

    fn edges(
        &self,
        f: &mut dyn FnMut(u64, u64, u64, u32, &[(u32, Value)]),
    ) -> std::io::Result<()> {
        let mut scratch = ChunkScratch::default();
        for id in self.edge_ids() {
            if let Some(e) = self.edges.get(&id) {
                f(id, e.from, e.to, e.etype, &e.props);
            } else if let Some(s) = self.base_live_edge(id) {
                let (from, to, t) = (self.base.edge_from(s), self.base.edge_to(s), self.base.edge_type(s));
                match self.edge_props.get(&id) {
                    Some(p) => f(id, from, to, t, p),
                    None => {
                        let props = self.base.edge_props_scratch(s, &mut scratch);
                        f(id, from, to, t, &props);
                    }
                }
            }
        }
        Ok(())
    }
}

