//! Types both storage engines share: id-keyed maps, the string interner,
//! adjacency entries, directions, the value key used for ordering, errors and
//! the CSR projection algorithms run over.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

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

    /// Every interned string, in id order.
    pub fn all(&self) -> &[String] {
        &self.list
    }

    /// An interner holding exactly `list`, with ids in list order.
    pub(crate) fn from_list(list: Vec<String>) -> Interner {
        let map = list
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i as u32))
            .collect();
        Interner { map, list }
    }
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adj {
    pub edge: u64,
    pub other: u64,
    pub etype: u32,
}


pub fn get_prop(props: &[(u32, Value)], key: u32) -> Option<&Value> {
    props.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
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
    /// Out of room: a `:memory:` graph at its `max_memory`, or a full disk.
    /// The transaction that hit it was rolled back; the graph is usable.
    Full(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {}", e),
            Error::Msg(m) => write!(f, "{}", m),
            Error::Full(m) => write!(f, "{}", m),
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

/// Property keys per label or relationship type, from `Graph::sample_keys`.
pub type KeySample = Vec<(String, Vec<String>)>;


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
