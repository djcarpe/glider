//! The engine. A labelled property graph held in memory, backed by the
//! append-only log in `store`.
//!
//! Shape of the data:
//!   node  = id + set of labels + flat property map + adjacency lists
//!   edge  = id + from + to + one type + flat property map
//!
//! Labels, edge types and property keys are interned to `u32`, so the per-node
//! cost is a couple of small vectors rather than a pile of Strings.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::path::Path;

use crate::store::{Op, Store, Sync};
use crate::value::Value;

// ------------------------------------------------------------ id-keyed maps

/// Hashing u64 ids with SipHash is a waste; ids are already dense and unique.
#[derive(Default)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ *b as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn write_u64(&mut self, v: u64) {
        let mut x = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x >> 29;
        self.0 = x;
    }
    fn write_u32(&mut self, v: u32) {
        self.write_u64(v as u64);
    }
    fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }
}

pub type IdBuild = BuildHasherDefault<IdHasher>;
pub type IdMap<V> = HashMap<u64, V, IdBuild>;
pub type IdSet = HashSet<u64, IdBuild>;

pub fn id_map<V>() -> IdMap<V> {
    HashMap::default()
}
pub fn id_set() -> IdSet {
    HashSet::default()
}

// ---------------------------------------------------------------- interning

#[derive(Default)]
pub struct Interner {
    map: HashMap<String, u32>,
    list: Vec<String>,
}

impl Interner {
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(id) = self.map.get(s) {
            return *id;
        }
        let id = self.list.len() as u32;
        self.list.push(s.to_string());
        self.map.insert(s.to_string(), id);
        id
    }

    pub fn lookup(&self, s: &str) -> Option<u32> {
        self.map.get(s).copied()
    }

    pub fn name(&self, id: u32) -> &str {
        self.list
            .get(id as usize)
            .map(|s| s.as_str())
            .unwrap_or("?")
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

// -------------------------------------------------------------- graph types

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adj {
    pub edge: u64,
    pub other: u64,
    pub etype: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub id: u64,
    pub labels: Vec<u32>,
    pub props: Vec<(u32, Value)>,
    pub out: Vec<Adj>,
    pub inc: Vec<Adj>,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub id: u64,
    pub from: u64,
    pub to: u64,
    pub etype: u32,
    pub props: Vec<(u32, Value)>,
}

pub fn get_prop<'a>(props: &'a [(u32, Value)], key: u32) -> Option<&'a Value> {
    props.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
}

fn set_prop(props: &mut Vec<(u32, Value)>, key: u32, value: Value) {
    match props.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value,
        None => props.push((key, value)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Out,
    In,
    Both,
}

impl Dir {
    pub fn parse(s: &str) -> Option<Dir> {
        match s.to_ascii_lowercase().as_str() {
            "out" | "outgoing" | ">" => Some(Dir::Out),
            "in" | "incoming" | "<" => Some(Dir::In),
            "both" | "any" | "undirected" | "-" => Some(Dir::Both),
            _ => None,
        }
    }
}

/// Newtype so `Value` can key a BTreeMap despite floats.
#[derive(Clone, Debug)]
pub struct VKey(pub Value);

impl PartialEq for VKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == std::cmp::Ordering::Equal
    }
}
impl Eq for VKey {}
impl PartialOrd for VKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for VKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

// -------------------------------------------------------------------- error

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Msg(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {}", e),
            Error::Msg(m) => write!(f, "{}", m),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<String> for Error {
    fn from(e: String) -> Self {
        Error::Msg(e)
    }
}

impl From<&str> for Error {
    fn from(e: &str) -> Self {
        Error::Msg(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// -------------------------------------------------------------------- graph

pub struct Graph {
    store: Option<Store>,
    pub strings: Interner,
    nodes: IdMap<Node>,
    edges: IdMap<Edge>,
    label_index: HashMap<u32, IdSet>,
    type_index: HashMap<u32, IdSet>,
    prop_indexes: HashMap<(u32, u32), BTreeMap<VKey, Vec<u64>>>,
    next_node: u64,
    next_edge: u64,
    uncommitted: u64,
    /// Commit after every mutating statement. Turn off for bulk loads.
    pub autocommit: bool,
}

impl Graph {
    /// Open a database file, replaying it into memory. Creates it if absent.
    pub fn open(path: &Path, sync: Sync) -> Result<Graph> {
        Graph::open_with(path, sync, false)
    }

    /// Open even if a lock file is present. Only when you know the previous
    /// writer is dead — two live writers corrupt the file.
    pub fn open_forced(path: &Path, sync: Sync) -> Result<Graph> {
        Graph::open_with(path, sync, true)
    }

    fn open_with(path: &Path, sync: Sync, force: bool) -> Result<Graph> {
        let mut g = Graph::memory();
        // Apply each op as it is decoded. Collecting them into a Vec first
        // meant holding every historical mutation in memory at once — each one
        // carrying its own allocated label and property strings — before any of
        // it reached the graph. On a 159 MB log that intermediate cost more
        // than everything else in the open put together.
        let store = {
            let sink = &mut g;
            Store::open_with(path, sync, force, &mut |op| sink.apply_mem(&op))?
        };
        g.store = Some(store);
        Ok(g)
    }

    /// A purely in-memory graph. Nothing is persisted.
    pub fn memory() -> Graph {
        Graph {
            store: None,
            strings: Interner::default(),
            nodes: id_map(),
            edges: id_map(),
            label_index: HashMap::new(),
            type_index: HashMap::new(),
            prop_indexes: HashMap::new(),
            next_node: 1,
            next_edge: 1,
            uncommitted: 0,
            autocommit: true,
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
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn node(&self, id: u64) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn edge(&self, id: u64) -> Option<&Edge> {
        self.edges.get(&id)
    }

    pub fn node_ids(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.nodes.keys().copied().collect();
        v.sort_unstable();
        v
    }

    pub fn edge_ids(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.edges.keys().copied().collect();
        v.sort_unstable();
        v
    }

    // --------------------------------------------------------- transactions

    fn log(&mut self, op: &Op) {
        if let Some(s) = &mut self.store {
            s.push(op);
            self.uncommitted += 1;
        }
    }

    pub fn commit(&mut self) -> Result<()> {
        if let Some(s) = &mut self.store {
            s.commit()?;
        }
        self.uncommitted = 0;
        Ok(())
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
        if !self.nodes.contains_key(&from) {
            return Err(Error::Msg(format!("no node {}", from)));
        }
        if !self.nodes.contains_key(&to) {
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
        if !self.nodes.contains_key(&id) {
            return Ok(false);
        }
        let op = Op::NodeDel { id };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()?;
        Ok(true)
    }

    pub fn delete_edge(&mut self, id: u64) -> Result<bool> {
        if !self.edges.contains_key(&id) {
            return Ok(false);
        }
        let op = Op::EdgeDel { id };
        self.apply_mem(&op);
        self.log(&op);
        self.maybe_commit()?;
        Ok(true)
    }

    pub fn set_node_prop(&mut self, id: u64, key: &str, value: Value) -> Result<()> {
        if !self.nodes.contains_key(&id) {
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
        if !self.edges.contains_key(&id) {
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
        if !self.nodes.contains_key(&id) {
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

    pub fn indexes(&self) -> Vec<(String, String, usize)> {
        let mut out: Vec<(String, String, usize)> = self
            .prop_indexes
            .iter()
            .map(|((l, k), m)| {
                (
                    self.strings.name(*l).to_string(),
                    self.strings.name(*k).to_string(),
                    m.len(),
                )
            })
            .collect();
        out.sort();
        out
    }

    // ------------------------------------------------------- memory mutation

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
                for l in &label_ids {
                    self.label_index
                        .entry(*l)
                        .or_insert_with(id_set)
                        .insert(*id);
                }
                self.nodes.insert(
                    *id,
                    Node {
                        id: *id,
                        labels: label_ids,
                        props: prop_ids,
                        out: Vec::new(),
                        inc: Vec::new(),
                    },
                );
                if *id >= self.next_node {
                    self.next_node = id + 1;
                }
                self.index_node(*id);
            }
            Op::NodeDel { id } => {
                self.deindex_node(*id);
                let incident: Vec<u64> = match self.nodes.get(id) {
                    Some(n) => n.out.iter().chain(n.inc.iter()).map(|a| a.edge).collect(),
                    None => return,
                };
                for e in incident {
                    self.remove_edge_mem(e);
                }
                if let Some(n) = self.nodes.remove(id) {
                    for l in n.labels {
                        if let Some(set) = self.label_index.get_mut(&l) {
                            set.remove(id);
                        }
                    }
                }
            }
            Op::EdgeAdd {
                id,
                from,
                to,
                etype,
                props,
            } => {
                if !self.nodes.contains_key(from) || !self.nodes.contains_key(to) {
                    return;
                }
                let t = self.strings.intern(etype);
                let prop_ids: Vec<(u32, Value)> = props
                    .iter()
                    .map(|(k, v)| (self.strings.intern(k), v.clone()))
                    .collect();
                if let Some(n) = self.nodes.get_mut(from) {
                    n.out.push(Adj {
                        edge: *id,
                        other: *to,
                        etype: t,
                    });
                }
                if let Some(n) = self.nodes.get_mut(to) {
                    n.inc.push(Adj {
                        edge: *id,
                        other: *from,
                        etype: t,
                    });
                }
                self.type_index.entry(t).or_insert_with(id_set).insert(*id);
                self.edges.insert(
                    *id,
                    Edge {
                        id: *id,
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
                self.deindex_node(*id);
                if let Some(n) = self.nodes.get_mut(id) {
                    set_prop(&mut n.props, k, value.clone());
                }
                self.index_node(*id);
            }
            Op::NodeUnset { id, key } => {
                if let Some(k) = self.strings.lookup(key) {
                    self.deindex_node(*id);
                    if let Some(n) = self.nodes.get_mut(id) {
                        n.props.retain(|(pk, _)| *pk != k);
                    }
                    self.index_node(*id);
                }
            }
            Op::EdgeSet { id, key, value } => {
                let k = self.strings.intern(key);
                if let Some(e) = self.edges.get_mut(id) {
                    set_prop(&mut e.props, k, value.clone());
                }
            }
            Op::EdgeUnset { id, key } => {
                if let Some(k) = self.strings.lookup(key) {
                    if let Some(e) = self.edges.get_mut(id) {
                        e.props.retain(|(pk, _)| *pk != k);
                    }
                }
            }
            Op::LabelAdd { id, label } => {
                let l = self.strings.intern(label);
                self.deindex_node(*id);
                if let Some(n) = self.nodes.get_mut(id) {
                    if !n.labels.contains(&l) {
                        n.labels.push(l);
                    }
                }
                self.label_index.entry(l).or_insert_with(id_set).insert(*id);
                self.index_node(*id);
            }
            Op::LabelDel { id, label } => {
                if let Some(l) = self.strings.lookup(label) {
                    self.deindex_node(*id);
                    if let Some(n) = self.nodes.get_mut(id) {
                        n.labels.retain(|x| *x != l);
                    }
                    if let Some(set) = self.label_index.get_mut(&l) {
                        set.remove(id);
                    }
                    self.index_node(*id);
                }
            }
            Op::IndexAdd { label, key } => {
                let l = self.strings.intern(label);
                let k = self.strings.intern(key);
                if self.prop_indexes.contains_key(&(l, k)) {
                    return;
                }
                let mut map: BTreeMap<VKey, Vec<u64>> = BTreeMap::new();
                if let Some(ids) = self.label_index.get(&l) {
                    for id in ids {
                        if let Some(n) = self.nodes.get(id) {
                            if let Some(v) = get_prop(&n.props, k) {
                                map.entry(VKey(v.clone())).or_default().push(*id);
                            }
                        }
                    }
                }
                for list in map.values_mut() {
                    list.sort_unstable();
                }
                self.prop_indexes.insert((l, k), map);
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
                self.nodes.clear();
                self.edges.clear();
                self.label_index.clear();
                self.type_index.clear();
                for m in self.prop_indexes.values_mut() {
                    m.clear();
                }
                self.next_node = 1;
                self.next_edge = 1;
            }
        }
    }

    fn remove_edge_mem(&mut self, id: u64) {
        let Some(e) = self.edges.remove(&id) else {
            return;
        };
        if let Some(n) = self.nodes.get_mut(&e.from) {
            n.out.retain(|a| a.edge != id);
        }
        if let Some(n) = self.nodes.get_mut(&e.to) {
            n.inc.retain(|a| a.edge != id);
        }
        if let Some(set) = self.type_index.get_mut(&e.etype) {
            set.remove(&id);
        }
    }

    fn index_node(&mut self, id: u64) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        for ((l, k), map) in self.prop_indexes.iter_mut() {
            if !node.labels.contains(l) {
                continue;
            }
            if let Some(v) = get_prop(&node.props, *k) {
                let list = map.entry(VKey(v.clone())).or_default();
                if let Err(pos) = list.binary_search(&id) {
                    list.insert(pos, id);
                }
            }
        }
    }

    fn deindex_node(&mut self, id: u64) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        for ((l, k), map) in self.prop_indexes.iter_mut() {
            if !node.labels.contains(l) {
                continue;
            }
            if let Some(v) = get_prop(&node.props, *k) {
                if let Some(list) = map.get_mut(&VKey(v.clone())) {
                    if let Ok(pos) = list.binary_search(&id) {
                        list.remove(pos);
                    }
                    if list.is_empty() {
                        map.remove(&VKey(v.clone()));
                    }
                }
            }
        }
    }

    // -------------------------------------------------------------- lookups

    pub fn nodes_with_label(&self, label: &str) -> Vec<u64> {
        match self
            .strings
            .lookup(label)
            .and_then(|l| self.label_index.get(&l))
        {
            Some(set) => {
                let mut v: Vec<u64> = set.iter().copied().collect();
                v.sort_unstable();
                v
            }
            None => Vec::new(),
        }
    }

    pub fn edges_with_type(&self, etype: &str) -> Vec<u64> {
        match self
            .strings
            .lookup(etype)
            .and_then(|t| self.type_index.get(&t))
        {
            Some(set) => {
                let mut v: Vec<u64> = set.iter().copied().collect();
                v.sort_unstable();
                v
            }
            None => Vec::new(),
        }
    }

    /// Exact-match lookup through a property index, if one exists.
    pub fn indexed_lookup(&self, label: &str, key: &str, value: &Value) -> Option<Vec<u64>> {
        let l = self.strings.lookup(label)?;
        let k = self.strings.lookup(key)?;
        let map = self.prop_indexes.get(&(l, k))?;
        Some(map.get(&VKey(value.clone())).cloned().unwrap_or_default())
    }

    /// How many nodes carry a label, without materialising them. The planner
    /// asks this for every candidate anchor, so it must not allocate.
    pub fn label_count(&self, label: &str) -> usize {
        self.strings
            .lookup(label)
            .and_then(|l| self.label_index.get(&l))
            .map(|set| set.len())
            .unwrap_or(0)
    }

    /// Size of one index bucket — the planner's estimate for an indexed
    /// equality. `None` means there is no index to use.
    pub fn index_count(&self, label: &str, key: &str, value: &Value) -> Option<usize> {
        let l = self.strings.lookup(label)?;
        let k = self.strings.lookup(key)?;
        let map = self.prop_indexes.get(&(l, k))?;
        Some(map.get(&VKey(value.clone())).map(|v| v.len()).unwrap_or(0))
    }

    pub fn has_index(&self, label: &str, key: &str) -> bool {
        match (self.strings.lookup(label), self.strings.lookup(key)) {
            (Some(l), Some(k)) => self.prop_indexes.contains_key(&(l, k)),
            _ => false,
        }
    }

    pub fn node_prop(&self, id: u64, key: &str) -> Option<&Value> {
        let k = self.strings.lookup(key)?;
        get_prop(&self.nodes.get(&id)?.props, k)
    }

    pub fn edge_prop(&self, id: u64, key: &str) -> Option<&Value> {
        let k = self.strings.lookup(key)?;
        get_prop(&self.edges.get(&id)?.props, k)
    }

    pub fn node_labels(&self, id: u64) -> Vec<String> {
        match self.nodes.get(&id) {
            Some(n) => n
                .labels
                .iter()
                .map(|l| self.strings.name(*l).to_string())
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn has_label(&self, id: u64, label: u32) -> bool {
        self.nodes
            .get(&id)
            .map(|n| n.labels.contains(&label))
            .unwrap_or(false)
    }

    pub fn node_props(&self, id: u64) -> Vec<(String, Value)> {
        match self.nodes.get(&id) {
            Some(n) => n
                .props
                .iter()
                .map(|(k, v)| (self.strings.name(*k).to_string(), v.clone()))
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn edge_props(&self, id: u64) -> Vec<(String, Value)> {
        match self.edges.get(&id) {
            Some(e) => e
                .props
                .iter()
                .map(|(k, v)| (self.strings.name(*k).to_string(), v.clone()))
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn edge_type_name(&self, id: u64) -> Option<&str> {
        self.edges.get(&id).map(|e| self.strings.name(e.etype))
    }

    /// Adjacency in a direction, optionally filtered to one edge type.
    pub fn neighbors(&self, id: u64, dir: Dir, etype: Option<u32>) -> Vec<Adj> {
        let Some(n) = self.nodes.get(&id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut push = |list: &Vec<Adj>| {
            for a in list {
                if etype.map(|t| t == a.etype).unwrap_or(true) {
                    out.push(*a);
                }
            }
        };
        match dir {
            Dir::Out => push(&n.out),
            Dir::In => push(&n.inc),
            Dir::Both => {
                push(&n.out);
                push(&n.inc);
            }
        }
        out
    }

    pub fn degree(&self, id: u64, dir: Dir) -> usize {
        match self.nodes.get(&id) {
            Some(n) => match dir {
                Dir::Out => n.out.len(),
                Dir::In => n.inc.len(),
                Dir::Both => n.out.len() + n.inc.len(),
            },
            None => 0,
        }
    }

    // ------------------------------------------------------------ housekeeping

    /// Every op needed to rebuild current state from an empty file.
    pub fn snapshot_ops(&self) -> Vec<Op> {
        let mut ops = Vec::with_capacity(self.nodes.len() + self.edges.len() + 8);
        for (label, key, _) in self.indexes() {
            ops.push(Op::IndexAdd { label, key });
        }
        for id in self.node_ids() {
            let n = &self.nodes[&id];
            ops.push(Op::NodeAdd {
                id,
                labels: n
                    .labels
                    .iter()
                    .map(|l| self.strings.name(*l).to_string())
                    .collect(),
                props: n
                    .props
                    .iter()
                    .map(|(k, v)| (self.strings.name(*k).to_string(), v.clone()))
                    .collect(),
            });
        }
        for id in self.edge_ids() {
            let e = &self.edges[&id];
            ops.push(Op::EdgeAdd {
                id,
                from: e.from,
                to: e.to,
                etype: self.strings.name(e.etype).to_string(),
                props: e
                    .props
                    .iter()
                    .map(|(k, v)| (self.strings.name(*k).to_string(), v.clone()))
                    .collect(),
            });
        }
        ops.push(Op::Counters {
            next_node: self.next_node,
            next_edge: self.next_edge,
        });
        ops
    }

    /// Rewrite the log as a minimal snapshot. Reclaims space from deletes and
    /// overwritten properties.
    pub fn compact(&mut self) -> Result<u64> {
        let before = self.file_len();
        let ops = self.snapshot_ops();
        if let Some(s) = &mut self.store {
            s.commit()?;
            s.compact(&ops)?;
        }
        Ok(before)
    }

    pub fn stats(&self) -> Stats {
        let mut label_counts: Vec<(String, usize)> = self
            .label_index
            .iter()
            .filter(|(_, s)| !s.is_empty())
            .map(|(l, s)| (self.strings.name(*l).to_string(), s.len()))
            .collect();
        label_counts.sort();
        let mut type_counts: Vec<(String, usize)> = self
            .type_index
            .iter()
            .filter(|(_, s)| !s.is_empty())
            .map(|(t, s)| (self.strings.name(*t).to_string(), s.len()))
            .collect();
        type_counts.sort();
        Stats {
            nodes: self.nodes.len(),
            edges: self.edges.len(),
            labels: label_counts,
            edge_types: type_counts,
            interned: self.strings.len(),
            file_bytes: self.file_len(),
            indexes: self.indexes(),
        }
    }

    // ------------------------------------------------------------------ csr

    /// Flatten into compressed sparse row form: the layout every algorithm in
    /// `algo` runs over. Built fresh per call, which keeps mutation cheap and
    /// makes algorithm runs cache-friendly.
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
            let node = &self.nodes[id];
            let emit =
                |list: &Vec<Adj>, adj: &mut Vec<u32>, eids: &mut Vec<u64>, w: &mut Vec<f64>| {
                    for a in list {
                        if let Some(t) = etype {
                            if a.etype != t {
                                continue;
                            }
                        }
                        let Some(p) = pos.get(&a.other) else { continue };
                        adj.push(*p);
                        eids.push(a.edge);
                        let weight = match wkey {
                            Some(k) => self
                                .edges
                                .get(&a.edge)
                                .and_then(|e| get_prop(&e.props, k))
                                .and_then(|v| v.as_f64())
                                .unwrap_or(1.0),
                            None => 1.0,
                        };
                        w.push(weight);
                    }
                };
            match dir {
                Dir::Out => emit(&node.out, &mut adj, &mut eids, &mut w),
                Dir::In => emit(&node.inc, &mut adj, &mut eids, &mut w),
                Dir::Both => {
                    emit(&node.out, &mut adj, &mut eids, &mut w);
                    emit(&node.inc, &mut adj, &mut eids, &mut w);
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
}

/// Compressed sparse row view of the graph, with dense indices 0..n.
pub struct Csr {
    /// dense index -> node id
    pub ids: Vec<u64>,
    /// node id -> dense index
    pub pos: IdMap<u32>,
    pub off: Vec<u32>,
    pub adj: Vec<u32>,
    pub eids: Vec<u64>,
    pub weights: Vec<f64>,
}

impl Csr {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn edge_count(&self) -> usize {
        self.adj.len()
    }

    #[inline]
    pub fn range(&self, v: usize) -> std::ops::Range<usize> {
        self.off[v] as usize..self.off[v + 1] as usize
    }

    #[inline]
    pub fn out(&self, v: usize) -> &[u32] {
        &self.adj[self.range(v)]
    }

    #[inline]
    pub fn degree(&self, v: usize) -> usize {
        (self.off[v + 1] - self.off[v]) as usize
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.pos.get(&id).map(|v| *v as usize)
    }

    /// Reverse adjacency, for algorithms that need to walk backwards.
    pub fn reversed(&self) -> Csr {
        let n = self.len();
        let mut counts = vec![0u32; n + 1];
        for &t in &self.adj {
            counts[t as usize + 1] += 1;
        }
        for i in 0..n {
            counts[i + 1] += counts[i];
        }
        let mut cursor = counts.clone();
        let mut adj = vec![0u32; self.adj.len()];
        let mut eids = vec![0u64; self.adj.len()];
        let mut weights = vec![0.0f64; self.adj.len()];
        for v in 0..n {
            for i in self.range(v) {
                let t = self.adj[i] as usize;
                let slot = cursor[t] as usize;
                adj[slot] = v as u32;
                eids[slot] = self.eids[i];
                weights[slot] = self.weights[i];
                cursor[t] += 1;
            }
        }
        Csr {
            ids: self.ids.clone(),
            pos: self.pos.clone(),
            off: counts,
            adj,
            eids,
            weights,
        }
    }
}
