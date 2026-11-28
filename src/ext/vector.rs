//! `vector`: vectors beside the graph, in named spaces, searched exactly.
//!
//! ```text
//! CREATE EXTENSION vector
//! CALL vector.create(space: "text", dims: 384, metric: "cosine", of: "node",
//!                    labels: ["WorkInstruction"], model: "m", model_version: "3")
//! CALL vector.put(space: "text", ids: [1, 2], vectors: [[...], [...]], hashes: ["h1", "h2"])
//! CALL vector.search(space: "text", vector: [...], top: 10, labels: [...], min_score: 0.5)
//! CALL vector.similar(space: "text", id: 42, top: 10, in: "other")
//! CALL vector.stale(space: "text", limit: 1000)        what no longer matches its source
//! CALL vector.get(space: "text", ids: [1])             CALL vector.hashes(space: "text", ids: [1])
//! CALL vector.delete(space: "text", ids: [1])          CALL vector.spaces()
//! CALL vector.drop(space: "text")
//! ```
//!
//! A vector is derived data (docs/DERIVED.md, D1-D4): it is never the thing
//! itself, so a search returns ids and scores and what a node is comes from
//! the graph; and it says what it was derived from. A *space* records, once,
//! what makes two vectors comparable and where they came from: dimensions,
//! metric, whether vectors are stored normalised, what they are about
//! (`node`, `edge`, or `key`: a group or a whole graph named by the caller),
//! the labels whose nodes are expected to have one, the algorithm and its
//! configuration, the model and its version for vectors made outside, the
//! code version and a caller-supplied time. A definition is never altered:
//! a space is dropped and made again.
//!
//! **Storage.** Keyspace `vector`:
//!
//! | key | value |
//! |---|---|
//! | `!n` | the next space number (4 bytes) |
//! | `!d<name>` | a space's definition ([`Space`]) |
//! | `c<space>` | its count, bytes and the revision it last changed at ([`Stats`]) |
//! | `v<space><target>` | the vector: `dims` little-endian 32-bit floats, nothing else |
//! | `m<space><target>` | the revision it was written at (8 bytes), then the source hash, if any |
//!
//! `<space>` is the space's number (4 bytes, big-endian), `<target>` a node
//! or edge id (8 bytes, big-endian, so keys sort by id) or a key's bytes.
//! Definitions are configuration (`!`), so `CLEAR` keeps them and wipes the
//! vectors, and the counts are reset in the bulk hook. The vector and its
//! metadata are apart so that a search reads nothing but floats and a put
//! whose source hash is unchanged rewrites only the 8-byte revision.
//!
//! **Scores** are computed in 64-bit floats, element by element in index
//! order, with q the query (normalised to unit length when the metric is
//! `cosine` or the space is normalised) and s the stored 32-bit floats:
//! `cosine` is q.s / |s| (q.s in a normalised space), `dot` is q.s, `l2` is
//! -|q - s| (negated, so that for every metric a higher score is closer).
//! Results come best first, ties to the lower id (or key bytes).
//!
//! **Staleness (D4).** A vector carries the log position it was written at
//! ([`Graph::log_end`] before its record, as the change journal's entries
//! do). It is *stale* when the journal holds a change to what its space
//! follows (`follows: "self"`: its node or edge; `"edges"`: its node and
//! the edges at it, the journal's `adjacency` entries) after that
//! position, or when that position (or its space's) lies past the
//! database's; *unknown* when the journal is not installed, cannot answer
//! for its position, or (for a key) when anything in the graph changed
//! since; and *fresh* otherwise. A node of the space's labels with no
//! vector is *missing*. Changes farther away than a node's own edges are
//! not seen: that is where the engine's knowledge stops.
//!
//! **Whether a space is current** ([`current`]) is answered without
//! counting: `vector.complete`, and only it, counts the space and, when
//! everything is fresh, records the position it counted at and where that
//! record ends (`k<space>`). No question (search, similar, spaces, stale,
//! get, hashes) writes anything. A search reports
//! `yes` when nothing was written since that record, or when the journal
//! shows no change since that made a vector stale or left a node missing;
//! `no` naming one; `unknown` otherwise. `counts: true` counts, and with
//! `fresh: "require"` anything but current is an error (counting first
//! when it is not known).
//!
//! Every loop over vectors passes through [`step`], the one place the
//! statement budget (slice G2) is to be counted.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};

use super::changes::{self, Kind};
use super::{Args, BulkWrite, EdgeImage, Extension, NodeImage, Spec};
use crate::graph::{Error, Graph, Result};
use crate::query::QueryResult;
use crate::value::Value;

pub struct Vector;

/// The keyspace, and the extension's name.
pub const NS: &str = "vector";

/// The version of the code that stores vectors, recorded in a space when
/// the caller names no `code:` of its own (D3).
pub const CODE: &str = "vector/1";

/// Rows one call returns at most, and the most results a search keeps.
pub const MAX_ROWS: usize = 10_000;

/// Ids a `put`, `get`, `delete` or `hashes` call takes at most.
pub const MAX_BATCH: usize = 10_000;

/// The most dimensions a space may have.
pub const MAX_DIMS: usize = 65_536;

/// Records one `stale` call looks at, at most, before it returns a cursor.
pub const MAX_LOOK: u64 = 100_000;

const MAX_NAME: usize = 128;
const MAX_KEY: usize = 1_024;
const MAX_HASH: usize = 255;
/// Records copied out of a scan at a time, when each needs another read.
const SCAN_BATCH: usize = 512;
/// Records deleted per write by `drop`, and label members read at a time.
const BATCH: usize = 4_096;

const NEXT_KEY: &[u8] = b"!n";

const PROCEDURES: &[Spec] = &[
    Spec {
        name: "create",
        args: "(space, dims, metric, normalized, of, follows, labels, algorithm, config, model, model_version, code, at)",
        doc: "Make a named space: dims, metric (cosine, dot, l2), normalized, of (node, edge, key), follows (self, or edges for vectors of a node's surroundings), the labels expected to have a vector, and where its vectors come from",
    },
    Spec {
        name: "drop",
        args: "(space)",
        doc: "Forget a space and every vector in it",
    },
    Spec {
        name: "spaces",
        args: "(counts)",
        doc: "Every space with its definition, vectors, bytes, the revision it last changed at and whether it is current; counts: true adds stale, missing and unknown",
    },
    Spec {
        name: "put",
        args: "(space, ids | keys, vectors, hashes)",
        doc: "Store vectors (packed 32-bit floats) with the hash of their source; an unchanged hash only records that the vector is current",
    },
    Spec {
        name: "get",
        args: "(space, ids | keys)",
        doc: "Stored vectors with their revision, hash and state (fresh, stale, unknown)",
    },
    Spec {
        name: "hashes",
        args: "(space, ids | keys)",
        doc: "The stored source hashes, so a caller can tell which sources changed without the engine seeing them",
    },
    Spec {
        name: "delete",
        args: "(space, ids | keys)",
        doc: "Remove vectors",
    },
    Spec {
        name: "search",
        args: "(space, vector, top, labels, min_score, fresh, counts)",
        doc: "Exact search: every candidate scored, the best returned as id, score, state, ties to the lower id; says whether the space is current (yes, no, unknown) and, with counts: true, how many vectors are stale, missing or unknown",
    },
    Spec {
        name: "similar",
        args: "(space, id | key, top, in, labels, min_score, fresh, counts)",
        doc: "Search from a stored vector, in its space or in another whose dims, metric, algorithm, model and model version agree",
    },
    Spec {
        name: "complete",
        args: "(space)",
        doc: "Count the space and, only if every vector is fresh and none is missing, record it complete at that revision, so searches can say it is current without counting; otherwise report what stands in the way and record nothing",
    },
    Spec {
        name: "stale",
        args: "(space, after, limit)",
        doc: "In slices: vectors whose node or edge changed after they were written (stale), those the journal cannot vouch for (unknown), and nodes of the space's labels with none (missing)",
    },
];

// ----------------------------------------------------------------- the model

/// How two vectors are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    Cosine,
    Dot,
    L2,
}

impl Metric {
    pub fn name(self) -> &'static str {
        match self {
            Metric::Cosine => "cosine",
            Metric::Dot => "dot",
            Metric::L2 => "l2",
        }
    }
    pub fn parse(s: &str) -> Option<Metric> {
        [Metric::Cosine, Metric::Dot, Metric::L2]
            .into_iter()
            .find(|m| m.name() == s)
    }
    fn byte(self) -> u8 {
        match self {
            Metric::Cosine => 0,
            Metric::Dot => 1,
            Metric::L2 => 2,
        }
    }
    fn from_byte(b: u8) -> Option<Metric> {
        [Metric::Cosine, Metric::Dot, Metric::L2]
            .into_iter()
            .find(|m| m.byte() == b)
    }
}

/// What a space's vectors are about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Of {
    Node,
    Edge,
    /// A group of nodes or a whole graph, named by the caller with a string.
    Key,
}

impl Of {
    pub fn name(self) -> &'static str {
        match self {
            Of::Node => "node",
            Of::Edge => "edge",
            Of::Key => "key",
        }
    }
    pub fn parse(s: &str) -> Option<Of> {
        [Of::Node, Of::Edge, Of::Key]
            .into_iter()
            .find(|m| m.name() == s)
    }
    fn byte(self) -> u8 {
        match self {
            Of::Node => 0,
            Of::Edge => 1,
            Of::Key => 2,
        }
    }
    fn from_byte(b: u8) -> Option<Of> {
        [Of::Node, Of::Edge, Of::Key]
            .into_iter()
            .find(|m| m.byte() == b)
    }
}

/// What a space's vectors follow: what, when it changes, makes them stale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Follows {
    /// The node or edge itself: its labels, properties, existence (and an
    /// edge's endpoints and type). A vector of a node's own text.
    Itself,
    /// A node and its edges: also an edge added, removed or changed at the
    /// node. A vector computed from the node's surroundings. Node spaces
    /// only. Changes further away are not seen (see the module docs).
    Edges,
}

impl Follows {
    pub fn name(self) -> &'static str {
        match self {
            Follows::Itself => "self",
            Follows::Edges => "edges",
        }
    }
    pub fn parse(s: &str) -> Option<Follows> {
        [Follows::Itself, Follows::Edges]
            .into_iter()
            .find(|m| m.name() == s)
    }
    fn byte(self) -> u8 {
        match self {
            Follows::Itself => 0,
            Follows::Edges => 1,
        }
    }
    fn from_byte(b: u8) -> Option<Follows> {
        [Follows::Itself, Follows::Edges]
            .into_iter()
            .find(|m| m.byte() == b)
    }
}

impl Space {
    /// The journal kinds whose entries for a vector's id make it stale
    /// (none for a key space: any node or edge change may).
    pub fn kinds(&self) -> &'static [Kind] {
        match (self.of, self.follows) {
            (Of::Node, Follows::Itself) => &[Kind::Node],
            (Of::Node, Follows::Edges) => &[Kind::Node, Kind::Adjacency],
            (Of::Edge, _) => &[Kind::Edge],
            (Of::Key, _) => &[],
        }
    }
}

/// A space's definition: everything needed to know whether two vectors may
/// be compared and where they came from. Written once by `create`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Space {
    pub name: String,
    /// The number its records are filed under.
    pub sid: u32,
    pub dims: usize,
    pub metric: Metric,
    /// Vectors are stored at unit length (a put normalises them).
    pub normalized: bool,
    pub of: Of,
    /// What its vectors follow: what makes them stale.
    pub follows: Follows,
    /// Labels whose nodes are expected to have a vector (node spaces).
    pub labels: Vec<String>,
    pub algorithm: Option<String>,
    pub config: Option<String>,
    pub model: Option<String>,
    pub model_version: Option<String>,
    pub code: String,
    /// A time the caller supplied; the engine records no clock.
    pub at: Option<String>,
    /// The log position the space was made at.
    pub created: u64,
}

const DEF_VERSION: u8 = 1;

fn put_str(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => out.extend_from_slice(&u32::MAX.to_be_bytes()),
        Some(s) => {
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
    }
}

struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
    }
    fn opt_str(&mut self) -> Option<Option<String>> {
        let n = self.u32()?;
        if n == u32::MAX {
            return Some(None);
        }
        let b = self.take(n as usize)?;
        Some(Some(String::from_utf8(b.to_vec()).ok()?))
    }
}

impl Space {
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![DEF_VERSION];
        out.extend_from_slice(&self.sid.to_be_bytes());
        out.extend_from_slice(&(self.dims as u32).to_be_bytes());
        out.push(self.metric.byte());
        out.push(self.normalized as u8);
        out.push(self.of.byte());
        out.push(self.follows.byte());
        out.extend_from_slice(&self.created.to_be_bytes());
        for s in [
            self.algorithm.as_deref(),
            self.config.as_deref(),
            self.model.as_deref(),
            self.model_version.as_deref(),
            Some(self.code.as_str()),
            self.at.as_deref(),
        ] {
            put_str(&mut out, s);
        }
        out.extend_from_slice(&(self.labels.len() as u32).to_be_bytes());
        for l in &self.labels {
            put_str(&mut out, Some(l));
        }
        out
    }

    fn decode(name: &str, b: &[u8]) -> Option<Space> {
        let mut r = Rd { b, at: 0 };
        if r.u8()? != DEF_VERSION {
            return None;
        }
        let sid = r.u32()?;
        let dims = r.u32()? as usize;
        let metric = Metric::from_byte(r.u8()?)?;
        let normalized = match r.u8()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        let of = Of::from_byte(r.u8()?)?;
        let follows = Follows::from_byte(r.u8()?)?;
        let created = r.u64()?;
        let algorithm = r.opt_str()?;
        let config = r.opt_str()?;
        let model = r.opt_str()?;
        let model_version = r.opt_str()?;
        let code = r.opt_str()??;
        let at = r.opt_str()?;
        let n = r.u32()? as usize;
        let mut labels = Vec::new();
        for _ in 0..n {
            labels.push(r.opt_str()??);
        }
        if r.at != b.len() || dims == 0 || dims > MAX_DIMS {
            return None;
        }
        Some(Space {
            name: name.to_string(),
            sid,
            dims,
            metric,
            normalized,
            of,
            follows,
            labels,
            algorithm,
            config,
            model,
            model_version,
            code,
            at,
            created,
        })
    }

    /// The same definition, whatever number and position it was made at.
    fn same_as(&self, o: &Space) -> bool {
        Space {
            sid: 0,
            created: 0,
            ..self.clone()
        } == Space {
            sid: 0,
            created: 0,
            ..o.clone()
        }
    }

    /// Bytes one vector of this space holds in the keyspace with a source
    /// hash of `hash` bytes: both records' keys (with the namespace) and
    /// values.
    pub fn bytes_per_vector(&self, target: usize, hash: usize) -> u64 {
        (rec_bytes(5 + target, self.dims * 4) + rec_bytes(5 + target, 8 + hash)) as u64
    }

    /// Why vectors of `self` and `o` may not be compared, if they may not:
    /// they must agree on dimensions, metric, algorithm, model and model
    /// version.
    pub fn incomparable(&self, o: &Space) -> Option<String> {
        let mut diff = Vec::new();
        if self.dims != o.dims {
            diff.push(format!("dims {} and {}", self.dims, o.dims));
        }
        if self.metric != o.metric {
            diff.push(format!(
                "metric {} and {}",
                self.metric.name(),
                o.metric.name()
            ));
        }
        let show = |s: &Option<String>| s.clone().unwrap_or_else(|| "none".into());
        for (what, a, b) in [
            ("algorithm", &self.algorithm, &o.algorithm),
            ("model", &self.model, &o.model),
            ("model_version", &self.model_version, &o.model_version),
        ] {
            if a != b {
                diff.push(format!("{what} {} and {}", show(a), show(b)));
            }
        }
        (!diff.is_empty()).then(|| diff.join(", "))
    }
}

fn rec_bytes(key: usize, value: usize) -> usize {
    NS.len() + 1 + key + value
}

/// A space's running totals, kept beside its definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Stats {
    /// Vectors held.
    pub count: u64,
    /// Bytes their records hold: keys with the namespace, and values.
    pub bytes: u64,
    /// The log position of the last change to the space.
    pub changed: u64,
}

impl Stats {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        out.extend_from_slice(&self.count.to_be_bytes());
        out.extend_from_slice(&self.bytes.to_be_bytes());
        out.extend_from_slice(&self.changed.to_be_bytes());
        out
    }
    fn decode(b: &[u8]) -> Option<Stats> {
        let mut r = Rd { b, at: 0 };
        let s = Stats {
            count: r.u64()?,
            bytes: r.u64()?,
            changed: r.u64()?,
        };
        (r.at == b.len()).then_some(s)
    }
}

/// What a vector is about: a node or edge id, or a caller's key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Id(u64),
    Key(String),
}

impl Target {
    fn bytes(&self) -> Vec<u8> {
        match self {
            Target::Id(id) => id.to_be_bytes().to_vec(),
            Target::Key(k) => k.as_bytes().to_vec(),
        }
    }
    fn from_bytes(of: Of, b: &[u8]) -> Option<Target> {
        match of {
            Of::Key => Some(Target::Key(String::from_utf8(b.to_vec()).ok()?)),
            _ => Some(Target::Id(u64::from_be_bytes(b.try_into().ok()?))),
        }
    }
    fn value(&self) -> Value {
        match self {
            Target::Id(id) => Value::Int(*id as i64),
            Target::Key(k) => Value::Text(k.clone()),
        }
    }
    fn show(&self, of: Of) -> String {
        match (self, of) {
            (Target::Id(id), Of::Edge) => format!("edge {id}"),
            (Target::Id(id), _) => format!("node {id}"),
            (Target::Key(k), _) => format!("key '{k}'"),
        }
    }
}

/// Whether a vector still matches its source, as far as the engine can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fresh {
    Fresh,
    /// Its node or edge changed after it was written, or it lies past the
    /// database's position.
    Stale,
    /// The engine cannot know: no journal, or one that cannot answer for it.
    Unknown,
}

impl Fresh {
    pub fn name(self) -> &'static str {
        match self {
            Fresh::Fresh => "fresh",
            Fresh::Stale => "stale",
            Fresh::Unknown => "unknown",
        }
    }
}

/// A vector's metadata: the position it was written at and its source hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub revision: u64,
    pub hash: Option<String>,
}

impl Meta {
    fn encode(&self) -> Vec<u8> {
        let mut out = self.revision.to_be_bytes().to_vec();
        if let Some(h) = &self.hash {
            out.extend_from_slice(h.as_bytes());
        }
        out
    }
    fn decode(b: &[u8]) -> Option<Meta> {
        if b.len() < 8 {
            return None;
        }
        let hash = if b.len() == 8 {
            None
        } else {
            Some(String::from_utf8(b[8..].to_vec()).ok()?)
        };
        Some(Meta {
            revision: u64::from_be_bytes(b[..8].try_into().unwrap()),
            hash,
        })
    }
}

/// G2 (statement budgets): one statement step per vector looked at goes
/// here, the only place this extension's loops count.
#[inline]
fn step(_n: u64) -> Result<()> {
    Ok(())
}

fn def_key(name: &str) -> Vec<u8> {
    let mut k = b"!d".to_vec();
    k.extend_from_slice(name.as_bytes());
    k
}

fn rec_key(tag: u8, sid: u32, target: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(5 + target.len());
    k.push(tag);
    k.extend_from_slice(&sid.to_be_bytes());
    k.extend_from_slice(target);
    k
}

fn stats_key(sid: u32) -> Vec<u8> {
    rec_key(b'c', sid, &[])
}

// ------------------------------------------------------------- the Rust API

/// Every space, by name.
pub fn spaces(g: &Graph) -> Vec<Space> {
    let mut out = Vec::new();
    g.ext_scan(NS, b"!d", &mut |k, v| {
        if let Some(s) = std::str::from_utf8(&k[2..])
            .ok()
            .and_then(|name| Space::decode(name, v))
        {
            out.push(s);
        }
        true
    });
    out
}

/// One space by name.
pub fn space(g: &Graph, name: &str) -> Option<Space> {
    Space::decode(name, &g.ext_get(NS, &def_key(name))?)
}

fn space_or_err(g: &Graph, name: &str) -> Result<Space> {
    space(g, name).ok_or_else(|| {
        Error::Msg(format!(
            "no vector space named '{name}'; CALL vector.spaces() lists them, and CALL vector.create(space: '{name}', dims: ...) makes one"
        ))
    })
}

/// A space's totals (zero, changed at its creation, when none are kept).
pub fn stats(g: &Graph, s: &Space) -> Stats {
    g.ext_get(NS, &stats_key(s.sid))
        .and_then(|b| Stats::decode(&b))
        .unwrap_or(Stats {
            changed: s.created,
            ..Stats::default()
        })
}

/// A stored vector and its metadata.
pub fn get(g: &Graph, s: &Space, t: &Target) -> Option<(Vec<f32>, Meta)> {
    let tb = t.bytes();
    let meta = Meta::decode(&g.ext_get(NS, &rec_key(b'm', s.sid, &tb))?)?;
    let v = g.ext_get(NS, &rec_key(b'v', s.sid, &tb))?;
    Some((unpack(&v), meta))
}

fn unpack(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// One vector to store: `vector` may be None when `hash` equals the stored
/// one, which then only records that the stored vector is current.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub target: Target,
    pub vector: Option<Vec<f64>>,
    pub hash: Option<String>,
}

/// What a put did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Put {
    /// Vectors written (new or replaced).
    pub written: u64,
    /// Vectors whose hash matched: only their revision was rewritten.
    pub current: u64,
}

/// Check one vector against a space and pack it: the right length, every
/// element finite as a 32-bit float, normalised in a normalised space (a
/// zero vector is refused there and in a cosine space).
fn pack(s: &Space, what: &str, v: &[f64]) -> Result<Vec<u8>> {
    if v.len() != s.dims {
        return Err(Error::Msg(format!(
            "{what} has {} elements but space '{}' holds vectors of {}; give vectors of that length",
            v.len(),
            s.name,
            s.dims
        )));
    }
    for (i, x) in v.iter().enumerate() {
        if !(*x as f32).is_finite() {
            return Err(Error::Msg(format!(
                "{what} has element {i} = {x}, which is not a finite 32-bit float; give finite values"
            )));
        }
    }
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm == 0.0 && (s.normalized || s.metric == Metric::Cosine) {
        return Err(Error::Msg(format!(
            "{what} is all zeros, which has no direction for a {} space; give a vector with some element not zero",
            if s.normalized { "normalized" } else { "cosine" }
        )));
    }
    let scale = if s.normalized { 1.0 / norm } else { 1.0 };
    let mut out = Vec::with_capacity(4 * v.len());
    for x in v {
        let f = (x * scale) as f32;
        if !f.is_finite() {
            return Err(Error::Msg(format!(
                "{what} does not stay finite when normalised; give smaller values"
            )));
        }
        out.extend_from_slice(&f.to_le_bytes());
    }
    Ok(out)
}

fn exists(g: &Graph, of: Of, t: &Target) -> bool {
    match (of, t) {
        (Of::Node, Target::Id(id)) => g.node_shared(*id).is_some(),
        (Of::Edge, Target::Id(id)) => g.edge_shared(*id).is_some(),
        (Of::Key, Target::Key(_)) => true,
        _ => false,
    }
}

/// Store vectors in a space, all or none: every item is checked (its id
/// exists, its vector fits the space, its hash is short enough, no id
/// twice) before anything is written. Every vector of one call records the
/// same revision: the log's end before the call's one record.
pub fn put(g: &mut Graph, s: &Space, items: &[Item]) -> Result<Put> {
    if items.len() > MAX_BATCH {
        return Err(Error::Msg(format!(
            "{} vectors in one put; give at most {MAX_BATCH} a call",
            items.len()
        )));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut plan: Vec<(Vec<u8>, Option<Vec<u8>>, Option<String>)> = Vec::new();
    for it in items {
        let what = it.target.show(s.of);
        match (&it.target, s.of) {
            (Target::Key(k), Of::Key) if k.is_empty() || k.len() > MAX_KEY => {
                return Err(Error::Msg(format!(
                    "a key must be 1 to {MAX_KEY} bytes, got {}",
                    k.len()
                )))
            }
            (Target::Key(_), Of::Key) | (Target::Id(_), Of::Node | Of::Edge) => {}
            (_, Of::Key) => {
                return Err(Error::Msg(format!(
                    "space '{}' is about keys; pass keys: [...], not ids",
                    s.name
                )))
            }
            _ => {
                return Err(Error::Msg(format!(
                    "space '{}' is about {}s; pass ids: [...], not keys",
                    s.name,
                    s.of.name()
                )))
            }
        }
        if !seen.insert(it.target.clone()) {
            return Err(Error::Msg(format!(
                "{what} appears twice in one put; give each once"
            )));
        }
        if !exists(g, s.of, &it.target) {
            return Err(Error::Msg(format!(
                "{what} does not exist; a vector is about something in the graph"
            )));
        }
        if let Some(h) = &it.hash {
            if h.is_empty() || h.len() > MAX_HASH {
                return Err(Error::Msg(format!(
                    "the hash for {what} must be 1 to {MAX_HASH} bytes, got {}",
                    h.len()
                )));
            }
        }
        let packed = match &it.vector {
            Some(v) => Some(pack(s, &format!("the vector for {what}"), v)?),
            None => None,
        };
        plan.push((it.target.bytes(), packed, it.hash.clone()));
    }
    let rev = g.log_end();
    let mut st = stats(g, s);
    let mut puts = Vec::new();
    let mut done = Put::default();
    for (tb, packed, hash) in plan {
        let mk = rec_key(b'm', s.sid, &tb);
        let vk = rec_key(b'v', s.sid, &tb);
        let old = g.ext_get(NS, &mk).and_then(|b| Meta::decode(&b));
        let same = hash.is_some()
            && old.as_ref().is_some_and(|m| m.hash == hash)
            && g.ext_get(NS, &vk).is_some();
        let meta = Meta {
            revision: rev,
            hash: hash.clone(),
        }
        .encode();
        if same {
            puts.push((mk, meta));
            done.current += 1;
            continue;
        }
        let Some(packed) = packed else {
            let t = Target::from_bytes(s.of, &tb).unwrap();
            return Err(Error::Msg(format!(
                "no vector given for {} and its hash does not match the stored one; pass its vector",
                t.show(s.of)
            )));
        };
        match &old {
            Some(m) => {
                st.bytes = st.bytes
                    - rec_bytes(mk.len(), 8 + m.hash.as_ref().map_or(0, |h| h.len())) as u64
                    + rec_bytes(mk.len(), meta.len()) as u64;
            }
            None => {
                st.count += 1;
                st.bytes +=
                    (rec_bytes(vk.len(), packed.len()) + rec_bytes(mk.len(), meta.len())) as u64;
            }
        }
        puts.push((vk, packed));
        puts.push((mk, meta));
        done.written += 1;
    }
    if puts.is_empty() {
        return Ok(done);
    }
    st.changed = rev;
    puts.push((stats_key(s.sid), st.encode()));
    g.ext_write(NS, puts, Vec::new())?;
    Ok(done)
}

/// Remove the vectors of `targets` that exist; returns how many.
pub fn delete(g: &mut Graph, s: &Space, targets: &[Target]) -> Result<u64> {
    let mut st = stats(g, s);
    let mut dels = Vec::new();
    let mut n = 0;
    for t in targets {
        let tb = t.bytes();
        let mk = rec_key(b'm', s.sid, &tb);
        let Some(old) = g.ext_get(NS, &mk) else {
            continue;
        };
        let vk = rec_key(b'v', s.sid, &tb);
        st.count = st.count.saturating_sub(1);
        st.bytes = st.bytes.saturating_sub(
            (rec_bytes(mk.len(), old.len()) + rec_bytes(vk.len(), s.dims * 4)) as u64,
        );
        dels.push(mk);
        dels.push(vk);
        n += 1;
    }
    if n > 0 {
        st.changed = g.log_end();
        // A removed vector may leave a node of the labels without one: the
        // space is no longer known to be complete.
        dels.push(mark_key(s.sid));
        g.ext_write(NS, vec![(stats_key(s.sid), st.encode())], dels)?;
    }
    Ok(n)
}

/// Says, vector by vector, whether a space's vectors still match their
/// sources, from the change journal (see the module documentation).
pub struct Judge {
    now: u64,
    kinds: &'static [Kind],
    space_past: bool,
    /// The journal's floor, when it is installed and can answer.
    floor: Option<u64>,
    /// For key spaces, read the first time it is needed: the newest change
    /// to any node or edge since the floor (None: the journal cannot say).
    newest: std::cell::OnceCell<Option<u64>>,
}

impl Judge {
    pub fn new(g: &Graph, s: &Space) -> Judge {
        let now = g.log_end();
        let st = stats(g, s);
        let floor = changes::oldest(g)
            .map(|(f, _)| f)
            .filter(|&f| changes::answers(g, f));
        Judge {
            now,
            kinds: s.kinds(),
            space_past: s.created > now || st.changed > now,
            floor,
            newest: std::cell::OnceCell::new(),
        }
    }

    /// The state of the vector for `target` written at `rev`, and why.
    pub fn judge(&self, g: &Graph, target: &[u8], rev: u64) -> (Fresh, String) {
        let now = self.now;
        if rev > now || self.space_past {
            return (
                Fresh::Stale,
                format!("written at a position past this database's ({now}): positions here went back, so it cannot be trusted; put it again"),
            );
        }
        let Some(floor) = self.floor else {
            return (
                Fresh::Unknown,
                "the change journal is not installed or cannot answer; CREATE EXTENSION changes before putting vectors, then put them again".into(),
            );
        };
        if rev < floor {
            return (
                Fresh::Unknown,
                format!("written at revision {rev}, before the change journal's floor {floor}; put it again"),
            );
        }
        if !self.kinds.is_empty() {
            let id = u64::from_be_bytes(target.try_into().unwrap_or([0; 8]));
            for &kind in self.kinds {
                match changes::entry(g, kind, id) {
                    None => {}
                    Some(e) if e.revision > now => {
                        return (
                            Fresh::Unknown,
                            "the change journal holds a position past this database's; it cannot answer".into(),
                        )
                    }
                    Some(e) if e.revision > rev => {
                        return (
                            Fresh::Stale,
                            format!("its {} changed at revision {} after the vector was written at {rev}", kind.name(), e.revision),
                        )
                    }
                    Some(_) => {}
                }
            }
            return (Fresh::Fresh, String::new());
        }
        {
            let floor = self.floor.unwrap_or(0);
            match *self.newest.get_or_init(|| newest_change(g, floor)) {
                Some(n) if n <= rev => (Fresh::Fresh, String::new()),
                Some(n) => (
                    Fresh::Unknown,
                    format!("the graph changed at revision {n} after it was written at {rev}, and the engine cannot tell what a key's vector was made from"),
                ),
                None => (
                    Fresh::Unknown,
                    "the change journal cannot say what changed since it was written".into(),
                ),
            }
        }
    }
}

/// The newest revision of any node or edge change after `floor` (`floor`
/// itself when none), or None when the journal cannot say. Reads every
/// node and edge entry once.
fn newest_change(g: &Graph, floor: u64) -> Option<u64> {
    let mut newest = floor;
    let mut after = None;
    loop {
        let (list, more) =
            changes::since(g, floor, &[Kind::Node, Kind::Edge], after, MAX_ROWS).ok()?;
        step(list.len() as u64).ok()?;
        for e in &list {
            newest = newest.max(e.revision);
        }
        match (more, list.last()) {
            (true, Some(e)) => after = Some((e.kind, e.id)),
            _ => return Some(newest),
        }
    }
}

/// How a space's vectors stand against their sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Tally {
    pub fresh: u64,
    pub stale: u64,
    pub unknown: u64,
    /// Nodes of the space's labels with no vector.
    pub missing: u64,
}

impl Tally {
    pub fn clean(&self) -> bool {
        self.stale == 0 && self.unknown == 0 && self.missing == 0
    }
    fn say(&self) -> String {
        format!(
            "stale {}, missing {}, unknown {}",
            self.stale, self.missing, self.unknown
        )
    }
}

/// Visit records of one kind (`m` or `v`) of a space in key order after
/// `after` (a target), a batch at a time, until `f` returns false.
fn each_record(
    g: &Graph,
    tag: u8,
    s: &Space,
    after: Option<&[u8]>,
    f: &mut dyn FnMut(&[u8], &[u8]) -> Result<bool>,
) -> Result<()> {
    let prefix = rec_key(tag, s.sid, &[]);
    let mut from: Option<Vec<u8>> = after.map(|a| rec_key(tag, s.sid, a));
    loop {
        let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        g.ext_scan_after(NS, &prefix, from.as_deref(), &mut |k, v| {
            batch.push((k.to_vec(), v.to_vec()));
            batch.len() < SCAN_BATCH
        });
        let Some((last, _)) = batch.last() else {
            return Ok(());
        };
        let last = last.clone();
        for (k, v) in &batch {
            step(1)?;
            if !f(&k[5..], v)? {
                return Ok(());
            }
        }
        from = Some(last);
    }
}

/// The node ids of the space's labels, each once, in label order then id,
/// starting after `(label index, id)`, a batch at a time, until `f`
/// returns false. A node with several of the labels is visited under the
/// first.
fn each_member(
    g: &Graph,
    s: &Space,
    after: Option<(usize, u64)>,
    f: &mut dyn FnMut(usize, u64) -> Result<bool>,
) -> Result<()> {
    let ls: Vec<Option<u32>> = s.labels.iter().map(|l| g.strings.lookup(l)).collect();
    let start = after.map_or(0, |(i, _)| i);
    for (i, l) in ls.iter().enumerate().skip(start) {
        let Some(l) = *l else { continue };
        let mut from = match after {
            Some((j, id)) if j == i => id.saturating_add(1),
            _ => 0,
        };
        loop {
            let batch = g.label_members_from(l, from, BATCH);
            let Some(&last) = batch.last() else { break };
            for id in batch {
                step(1)?;
                let earlier = ls[..i].iter().flatten().any(|&e| g.has_label(id, e));
                if !earlier && !f(i, id)? {
                    return Ok(());
                }
            }
            if last == u64::MAX {
                break;
            }
            from = last + 1;
        }
    }
    Ok(())
}

fn has_vector(g: &Graph, s: &Space, id: u64) -> bool {
    g.ext_get(NS, &rec_key(b'm', s.sid, &id.to_be_bytes()))
        .is_some()
}

/// Count a space's vectors by state, and the nodes of its labels that have
/// none. Reads every vector's metadata and journal entry, and every member
/// of its labels.
pub fn tally(g: &Graph, s: &Space, judge: &Judge) -> Result<Tally> {
    let mut t = Tally::default();
    each_record(g, b'm', s, None, &mut |tb, v| {
        let rev = Meta::decode(v).map_or(u64::MAX, |m| m.revision);
        match judge.judge(g, tb, rev).0 {
            Fresh::Fresh => t.fresh += 1,
            Fresh::Stale => t.stale += 1,
            Fresh::Unknown => t.unknown += 1,
        }
        Ok(true)
    })?;
    each_member(g, s, None, &mut |_, id| {
        if !has_vector(g, s, id) {
            t.missing += 1;
        }
        Ok(true)
    })?;
    Ok(t)
}

// ------------------------------------------------- whether a space is current

fn mark_key(sid: u32) -> Vec<u8> {
    rec_key(b'k', sid, &[])
}

/// Whether a space is known to be entirely fresh, answered without counting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Current {
    /// Established complete (every vector fresh, no node of its labels
    /// missing) at `since`, and nothing it follows has made a vector stale
    /// or a node missing since.
    Yes { since: u64 },
    /// Established at `since`, and since then a change made a vector of
    /// it stale or a node of its labels lack one.
    No { since: u64, why: String },
    /// The engine has not established it, or cannot tell.
    Unknown { why: String },
}

impl Current {
    pub fn name(&self) -> &'static str {
        match self {
            Current::Yes { .. } => "yes",
            Current::No { .. } => "no",
            Current::Unknown { .. } => "unknown",
        }
    }
    fn since(&self) -> Option<u64> {
        match self {
            Current::Yes { since } | Current::No { since, .. } => Some(*since),
            Current::Unknown { .. } => None,
        }
    }
    fn say(&self) -> String {
        match self {
            Current::Yes { since } => format!(
                "space current: yes, established complete at revision {since} and nothing it follows changed it since"
            ),
            Current::No { since, why } => format!(
                "space current: no, established complete at revision {since}, but {why}; CALL vector.stale lists what to put again"
            ),
            Current::Unknown { why } => format!(
                "space current: unknown, {why}; counts: true counts it, and CALL vector.complete records it complete once it is fresh"
            ),
        }
    }
}

/// Whether nothing has been written since `end`, the end of the marker's
/// own record: the log ends there (its transaction still open), or one
/// commit marker later and that is committed.
fn at_mark_end(g: &Graph, end: u64) -> bool {
    let now = g.log_end();
    now == end || (now == g.revision() && now == end + crate::storage::log::FRAME_BYTES)
}

/// Record that a full count found the space entirely fresh at `at`: one
/// small record, which says where it ends so that a later read can see
/// nothing was written since without asking the journal.
fn establish(g: &mut Graph, s: &Space, at: u64) -> Result<()> {
    let key = mark_key(s.sid);
    let mut full = NS.as_bytes().to_vec();
    full.push(0);
    full.extend_from_slice(&key);
    let op = crate::store::Op::ExtWrite {
        puts: vec![(full, vec![0u8; 16])],
        dels: Vec::new(),
    };
    let end = g.log_end() + crate::storage::log::FRAME_BYTES + op.encode_record().len() as u64;
    let mut v = at.to_be_bytes().to_vec();
    v.extend_from_slice(&end.to_be_bytes());
    g.ext_write(NS, vec![(key, v)], Vec::new())
}

fn mark(g: &Graph, s: &Space) -> Option<(u64, u64)> {
    let v = g.ext_get(NS, &mark_key(s.sid))?;
    (v.len() == 16).then(|| {
        (
            u64::from_be_bytes(v[..8].try_into().unwrap()),
            u64::from_be_bytes(v[8..].try_into().unwrap()),
        )
    })
}

/// Whether the space is current, without counting it: from the record a
/// full count left when it found everything fresh, and the change journal
/// since. Free when nothing was written since that record; otherwise it
/// reads the journal's entries of the kinds the space follows (not the
/// space), looking up the vector of each id changed since.
pub fn current(g: &Graph, s: &Space) -> Result<Current> {
    let unknown = |why: &str| Ok(Current::Unknown { why: why.into() });
    let Some((since, end)) = mark(g, s) else {
        return unknown("not established complete since the space last lost a vector or was made");
    };
    let now = g.log_end();
    let st = stats(g, s);
    if since > now || end > now || s.created > now || st.changed > now {
        return unknown("its records lie past this database's position, so they cannot be trusted");
    }
    if at_mark_end(g, end) {
        return Ok(Current::Yes { since });
    }
    if !changes::answers(g, since) {
        return unknown("the change journal cannot answer for when it was established complete");
    }
    let kinds: &[Kind] = if s.of == Of::Key {
        &[Kind::Node, Kind::Edge]
    } else {
        s.kinds()
    };
    let lset: Vec<u32> = s
        .labels
        .iter()
        .filter_map(|l| g.strings.lookup(l))
        .collect();
    let mut after = None;
    loop {
        let (list, more) = changes::since(g, since, kinds, after, MAX_ROWS)?;
        step(list.len() as u64)?;
        for e in &list {
            if e.revision > now {
                return unknown("the change journal holds a position past this database's");
            }
            if s.of == Of::Key {
                return unknown(&format!(
                    "the graph changed at revision {} and the engine cannot tell what a key's vector was made from",
                    e.revision
                ));
            }
            let mk = rec_key(b'm', s.sid, &e.id.to_be_bytes());
            match g.ext_get(NS, &mk).and_then(|b| Meta::decode(&b)) {
                Some(m) if m.revision < e.revision => {
                    return Ok(Current::No {
                        since,
                        why: format!(
                            "the vector of {} is stale: its {} changed at revision {}",
                            Target::Id(e.id).show(s.of),
                            e.kind.name(),
                            e.revision
                        ),
                    })
                }
                Some(_) => {}
                None if e.kind == Kind::Node && lset.iter().any(|&l| g.has_label(e.id, l)) => {
                    return Ok(Current::No {
                        since,
                        why: format!("node {} has a label of the space and no vector", e.id),
                    })
                }
                None => {}
            }
        }
        match (more, list.last()) {
            (true, Some(e)) => after = Some((e.kind, e.id)),
            _ => return Ok(Current::Yes { since }),
        }
    }
}

/// A query prepared for a space: checked and, when the metric is cosine or
/// the space normalised, at unit length.
pub fn prepare(s: &Space, q: &[f64]) -> Result<Vec<f64>> {
    if q.len() != s.dims {
        return Err(Error::Msg(format!(
            "the query vector has {} elements but space '{}' holds vectors of {}; give a query of that length",
            q.len(),
            s.name,
            s.dims
        )));
    }
    if let Some((i, x)) = q.iter().enumerate().find(|(_, x)| !x.is_finite()) {
        return Err(Error::Msg(format!(
            "the query vector has element {i} = {x}, which is not finite; give finite values"
        )));
    }
    if s.metric == Metric::Cosine || s.normalized {
        let n = q.iter().map(|x| x * x).sum::<f64>().sqrt();
        if n == 0.0 || !n.is_finite() {
            return Err(Error::Msg(
                "the query vector is all zeros (or too large to normalise), which has no direction; give another".into(),
            ));
        }
        return Ok(q.iter().map(|x| x / n).collect());
    }
    Ok(q.to_vec())
}

/// The score of stored vector `s` (packed) for prepared query `q`; higher
/// is closer. See the module documentation for the definition.
pub fn score(metric: Metric, normalized: bool, q: &[f64], s: &[u8]) -> f64 {
    let x = |i: usize| f32::from_le_bytes(s[4 * i..4 * i + 4].try_into().unwrap()) as f64;
    match metric {
        Metric::Cosine if normalized => (0..q.len()).map(|i| q[i] * x(i)).sum(),
        Metric::Dot => (0..q.len()).map(|i| q[i] * x(i)).sum(),
        Metric::Cosine => {
            let (mut dot, mut ss) = (0.0f64, 0.0f64);
            for (i, qi) in q.iter().enumerate() {
                let xi = x(i);
                dot += qi * xi;
                ss += xi * xi;
            }
            if ss == 0.0 {
                0.0
            } else {
                dot / ss.sqrt()
            }
        }
        Metric::L2 => {
            let mut d = 0.0f64;
            for (i, qi) in q.iter().enumerate() {
                let e = qi - x(i);
                d += e * e;
            }
            -d.sqrt()
        }
    }
}

/// A candidate in the bounded heap: greater is worse, so the heap's top
/// is the one to drop.
struct Cand {
    score: f64,
    target: Vec<u8>,
}

impl PartialEq for Cand {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Cand {
    fn cmp(&self, o: &Self) -> Ordering {
        o.score
            .total_cmp(&self.score)
            .then_with(|| self.target.cmp(&o.target))
    }
}

/// The best `top` of everything offered, by score then target; memory is
/// `top` candidates whatever is offered.
struct Best {
    top: usize,
    min: Option<f64>,
    heap: BinaryHeap<Cand>,
    scored: u64,
}

impl Best {
    fn offer(&mut self, score: f64, target: &[u8]) {
        self.scored += 1;
        if self.min.is_some_and(|m| score < m) || self.top == 0 {
            return;
        }
        if self.heap.len() == self.top {
            let worst = self.heap.peek().unwrap();
            let better = match score.total_cmp(&worst.score) {
                Ordering::Greater => true,
                Ordering::Less => false,
                Ordering::Equal => target < worst.target.as_slice(),
            };
            if !better {
                return;
            }
            self.heap.pop();
        }
        self.heap.push(Cand {
            score,
            target: target.to_vec(),
        });
    }
}

/// What to search for besides the query.
#[derive(Clone, Debug, Default)]
pub struct Search {
    pub top: usize,
    /// Only nodes with one of these labels are candidates (node spaces).
    pub labels: Vec<String>,
    pub min_score: Option<f64>,
    /// A target not to list (the vector a `similar` started from).
    pub exclude: Option<Target>,
}

/// The best vectors for a prepared query, best first.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub rows: Vec<(Target, f64)>,
    /// Candidates scored.
    pub scored: u64,
}

/// Exact search: every candidate is scored, the best `top` kept in a
/// bounded heap. `q` must come from [`prepare`].
pub fn search(g: &Graph, s: &Space, q: &[f64], opts: &Search) -> Result<Found> {
    if !opts.labels.is_empty() && s.of != Of::Node {
        return Err(Error::Msg(format!(
            "labels: restricts the nodes of a node space, and space '{}' is about {}s",
            s.name,
            s.of.name()
        )));
    }
    let mut best = Best {
        top: opts.top.min(MAX_ROWS),
        min: opts.min_score,
        heap: BinaryHeap::new(),
        scored: 0,
    };
    let exclude = opts.exclude.as_ref().map(|t| t.bytes());
    let want = s.dims * 4;
    let bad = |t: &[u8]| {
        Error::Msg(format!(
            "space '{}' holds a vector of the wrong length for {}; CALL check.run(checks: ['derived']) names such records, and putting it again repairs it",
            s.name,
            Target::from_bytes(s.of, t).map_or_else(|| "a bad key".into(), |t| t.show(s.of))
        ))
    };
    if opts.labels.is_empty() {
        let mut err = None;
        g.ext_scan(NS, &rec_key(b'v', s.sid, &[]), &mut |k, v| {
            let t = &k[5..];
            if let Err(e) = step(1) {
                err = Some(e);
                return false;
            }
            if v.len() != want {
                err = Some(bad(t));
                return false;
            }
            if exclude.as_deref() != Some(t) {
                best.offer(score(s.metric, s.normalized, q, v), t);
            }
            true
        });
        if let Some(e) = err {
            return Err(e);
        }
    } else {
        let scope = Space {
            labels: opts.labels.clone(),
            ..s.clone()
        };
        each_member(g, &scope, None, &mut |_, id| {
            let t = id.to_be_bytes();
            if exclude.as_deref() == Some(&t[..]) {
                return Ok(true);
            }
            if let Some(v) = g.ext_get(NS, &rec_key(b'v', s.sid, &t)) {
                if v.len() != want {
                    return Err(bad(&t));
                }
                best.offer(score(s.metric, s.normalized, q, &v), &t);
            }
            Ok(true)
        })?;
    }
    let scored = best.scored;
    let rows = best
        .heap
        .into_sorted_vec()
        .into_iter()
        .map(|c| (Target::from_bytes(s.of, &c.target).unwrap(), c.score))
        .collect();
    Ok(Found { rows, scored })
}

// -------------------------------------------------------------- procedures

fn installed_spaces_want(g: &Graph, of: Option<Of>) -> bool {
    let mut any = false;
    g.ext_scan(NS, b"!d", &mut |k, v| {
        any = match (
            of,
            std::str::from_utf8(&k[2..])
                .ok()
                .and_then(|n| Space::decode(n, v)),
        ) {
            (None, _) => true,
            (Some(o), Some(s)) => s.of == o,
            (Some(_), None) => false,
        };
        !any
    });
    any
}

fn name_arg(a: &Args) -> Result<String> {
    let name = a
        .str("space")
        .ok_or_else(|| Error::Msg("space: name the vector space".into()))?;
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(Error::Msg(format!(
            "space: a name is 1 to {MAX_NAME} bytes"
        )));
    }
    Ok(name)
}

fn opt_text(a: &Args, key: &str) -> Option<String> {
    match a.get(key)? {
        Value::Null => None,
        Value::Text(t) => Some(t.clone()),
        v => Some(v.to_json()),
    }
}

fn create_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let name = name_arg(a)?;
    let dims = match a.get("dims").and_then(|v| v.as_i64()) {
        Some(d) if d >= 1 && d as usize <= MAX_DIMS => d as usize,
        _ => {
            return Err(Error::Msg(format!(
                "dims: give the number of elements a vector has, 1 to {MAX_DIMS}"
            )))
        }
    };
    let metric = match a.str("metric") {
        None => Metric::Cosine,
        Some(m) => Metric::parse(&m.to_ascii_lowercase())
            .ok_or_else(|| Error::Msg(format!("metric: '{m}' is not one of cosine, dot, l2")))?,
    };
    let of = match a.str("of") {
        None => Of::Node,
        Some(o) => Of::parse(&o.to_ascii_lowercase())
            .ok_or_else(|| Error::Msg(format!("of: '{o}' is not one of node, edge, key")))?,
    };
    let labels = a.strs("labels");
    if !labels.is_empty() && of != Of::Node {
        return Err(Error::Msg(
            "labels: names the labels whose nodes should have a vector, so it applies to spaces of nodes only".into(),
        ));
    }
    let follows = match a.str("follows") {
        None => Follows::Itself,
        Some(f) => Follows::parse(&f.to_ascii_lowercase())
            .ok_or_else(|| Error::Msg(format!("follows: '{f}' is not one of self, edges")))?,
    };
    if follows == Follows::Edges && of != Of::Node {
        return Err(Error::Msg(
            "follows: 'edges' says a node's vector changes with the node's edges, so it applies to spaces of nodes only".into(),
        ));
    }
    let mut def = Space {
        name: name.clone(),
        sid: 0,
        dims,
        metric,
        normalized: a.bool("normalized", false),
        of,
        follows,
        labels,
        algorithm: opt_text(a, "algorithm"),
        config: opt_text(a, "config"),
        model: opt_text(a, "model"),
        model_version: opt_text(a, "model_version"),
        code: opt_text(a, "code").unwrap_or_else(|| CODE.into()),
        at: opt_text(a, "at"),
        created: g.log_end(),
    };
    if let Some(old) = space(g, &name) {
        if old.same_as(&def) {
            return Ok(QueryResult::message(format!(
                "vector space '{name}' already exists with this definition"
            )));
        }
        return Err(Error::Msg(format!(
            "vector space '{name}' exists with another definition, and changing it would make its vectors incomparable; CALL vector.drop(space: '{name}') and create it again"
        )));
    }
    let sid = g
        .ext_get(NS, NEXT_KEY)
        .filter(|b| b.len() == 4)
        .map_or(0, |b| u32::from_be_bytes(b[..].try_into().unwrap()));
    if sid == u32::MAX {
        return Err(Error::Msg(
            "this database has made every space number there is; DROP EXTENSION vector and CREATE EXTENSION vector start again".into(),
        ));
    }
    def.sid = sid;
    let st = Stats {
        count: 0,
        bytes: 0,
        changed: def.created,
    };
    g.ext_write(
        NS,
        vec![
            (NEXT_KEY.to_vec(), (sid + 1).to_be_bytes().to_vec()),
            (def_key(&name), def.encode()),
            (stats_key(sid), st.encode()),
        ],
        Vec::new(),
    )?;
    let mut r = QueryResult::message(format!(
        "created vector space '{name}': {dims} dims, {}{}, of {}s",
        metric.name(),
        if def.normalized { ", normalized" } else { "" },
        of.name()
    ));
    r.touched = 1;
    Ok(r)
}

fn drop_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let name = name_arg(a)?;
    let s = space_or_err(g, &name)?;
    let mut n = 0u64;
    for tag in *b"mv" {
        loop {
            let mut dels = Vec::new();
            g.ext_scan(NS, &rec_key(tag, s.sid, &[]), &mut |k, _| {
                dels.push(k.to_vec());
                dels.len() < BATCH
            });
            if dels.is_empty() {
                break;
            }
            step(dels.len() as u64)?;
            if tag == b'v' {
                n += dels.len() as u64;
            }
            g.ext_write(NS, Vec::new(), dels)?;
        }
    }
    g.ext_write(NS, Vec::new(), vec![def_key(&name), stats_key(s.sid)])?;
    let mut r = QueryResult::message(format!("dropped vector space '{name}' and its {n} vectors"));
    r.touched = 1;
    Ok(r)
}

fn opt_value(s: &Option<String>) -> Value {
    s.clone().map(Value::Text).unwrap_or(Value::Null)
}

fn spaces_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let counts = a.bool("counts", false);
    let mut rows = Vec::new();
    for s in spaces(g) {
        let cur = current(g, &s)?;
        let mut tallied = [Value::Null, Value::Null, Value::Null];
        if counts {
            let t = tally(g, &s, &Judge::new(g, &s))?;
            tallied = [
                Value::Int(t.stale as i64),
                Value::Int(t.missing as i64),
                Value::Int(t.unknown as i64),
            ];
        }
        let st = stats(g, &s);
        let mut row = vec![
            Value::Text(s.name.clone()),
            Value::Int(s.dims as i64),
            Value::Text(s.metric.name().into()),
            Value::Bool(s.normalized),
            Value::Text(s.of.name().into()),
            Value::Text(s.follows.name().into()),
            Value::List(s.labels.iter().map(|l| Value::Text(l.clone())).collect()),
            opt_value(&s.algorithm),
            opt_value(&s.config),
            opt_value(&s.model),
            opt_value(&s.model_version),
            Value::Text(s.code.clone()),
            opt_value(&s.at),
            Value::Int(s.created as i64),
            Value::Int(st.count as i64),
            Value::Int(st.bytes as i64),
            Value::Int(st.changed as i64),
            Value::Text(cur.name().into()),
            cur.since().map_or(Value::Null, |v| Value::Int(v as i64)),
        ];
        row.extend(tallied);
        rows.push(row);
    }
    let cols = [
        "space",
        "dims",
        "metric",
        "normalized",
        "of",
        "follows",
        "labels",
        "algorithm",
        "config",
        "model",
        "model_version",
        "code",
        "at",
        "created",
        "vectors",
        "bytes",
        "revision",
        "current",
        "current_since",
        "stale",
        "missing",
        "unknown",
    ];
    let mut r = QueryResult::table(cols.iter().map(|c| c.to_string()).collect(), rows);
    r.message = Some(format!("at revision {}", g.revision()));
    Ok(r)
}

/// One id from an int, a float with no fraction, or an `n12` / `e12`
/// handle.
fn one_id(v: &Value) -> Option<u64> {
    match v {
        Value::Int(i) if *i >= 0 => Some(*i as u64),
        Value::Float(f) if *f >= 0.0 && f.fract() == 0.0 => Some(*f as u64),
        Value::Text(s) => {
            let s = s.trim();
            s.strip_prefix(['n', 'e']).unwrap_or(s).parse().ok()
        }
        _ => None,
    }
}

fn list_of(v: Option<&Value>) -> Vec<Value> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::List(l)) => l.clone(),
        Some(v) => vec![v.clone()],
    }
}

/// The targets of a call: `ids:` (or `id:`) for node and edge spaces,
/// `keys:` (or `key:`) for key spaces.
fn targets_arg(a: &Args, s: &Space, one: bool) -> Result<Vec<Target>> {
    let (idk, keyk) = if one { ("id", "key") } else { ("ids", "keys") };
    let out: Vec<Target> = if s.of == Of::Key {
        if a.get(idk).is_some() {
            return Err(Error::Msg(format!(
                "space '{}' is about keys; pass {keyk}:, not {idk}:",
                s.name
            )));
        }
        list_of(a.get(keyk))
            .into_iter()
            .map(|v| match v {
                Value::Text(t) => Ok(Target::Key(t)),
                v => Err(Error::Msg(format!("{keyk}: expected strings, got {v}"))),
            })
            .collect::<Result<_>>()?
    } else {
        if a.get(keyk).is_some() {
            return Err(Error::Msg(format!(
                "space '{}' is about {}s; pass {idk}:, not {keyk}:",
                s.name,
                s.of.name()
            )));
        }
        list_of(a.get(idk))
            .into_iter()
            .map(|v| {
                one_id(&v)
                    .map(Target::Id)
                    .ok_or_else(|| Error::Msg(format!("{idk}: expected ids, got {v}")))
            })
            .collect::<Result<_>>()?
    };
    if out.len() > MAX_BATCH {
        return Err(Error::Msg(format!(
            "{} ids in one call; give at most {MAX_BATCH}",
            out.len()
        )));
    }
    Ok(out)
}

fn floats(v: &Value, what: &str) -> Result<Vec<f64>> {
    match v {
        Value::List(l) => l
            .iter()
            .map(|x| {
                x.as_f64()
                    .filter(|_| !matches!(x, Value::Bool(_) | Value::Text(_)))
                    .ok_or_else(|| {
                        Error::Msg(format!("{what}: expected a list of numbers, found {x}"))
                    })
            })
            .collect(),
        v => Err(Error::Msg(format!(
            "{what}: expected a list of numbers, got {v}"
        ))),
    }
}

fn put_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let s = space_or_err(g, &name_arg(a)?)?;
    let targets = targets_arg(a, &s, false)?;
    let vectors = list_of(a.get("vectors"));
    let hashes = list_of(a.get("hashes"));
    if vectors.len() != targets.len() {
        return Err(Error::Msg(format!(
            "{} targets and {} vectors; give one vector per target (null where the hash is unchanged)",
            targets.len(),
            vectors.len()
        )));
    }
    if !hashes.is_empty() && hashes.len() != targets.len() {
        return Err(Error::Msg(format!(
            "{} targets and {} hashes; give one hash per target, or none",
            targets.len(),
            hashes.len()
        )));
    }
    let mut items = Vec::with_capacity(targets.len());
    for (i, t) in targets.into_iter().enumerate() {
        let vector = match &vectors[i] {
            Value::Null => None,
            v => Some(floats(v, &format!("vectors[{i}]"))?),
        };
        let hash = match hashes.get(i) {
            None | Some(Value::Null) => None,
            Some(Value::Text(h)) => Some(h.clone()),
            Some(v) => {
                return Err(Error::Msg(format!(
                    "hashes[{i}]: expected a string, got {v}"
                )))
            }
        };
        items.push(Item {
            target: t,
            vector,
            hash,
        });
    }
    let done = put(g, &s, &items)?;
    let mut r = QueryResult::message(format!(
        "{} vectors written, {} unchanged by hash and marked current, in space '{}' at position {}",
        done.written,
        done.current,
        s.name,
        g.log_end()
    ));
    r.touched = (done.written + done.current) as usize;
    Ok(r)
}

fn target_col(s: &Space) -> String {
    if s.of == Of::Key { "key" } else { "id" }.into()
}

fn get_proc(g: &mut Graph, a: &Args, with_vector: bool) -> Result<QueryResult> {
    let s = space_or_err(g, &name_arg(a)?)?;
    let targets = targets_arg(a, &s, false)?;
    let judge = Judge::new(g, &s);
    let mut rows = Vec::new();
    for t in targets {
        step(1)?;
        let tb = t.bytes();
        let meta = g
            .ext_get(NS, &rec_key(b'm', s.sid, &tb))
            .and_then(|b| Meta::decode(&b));
        let mut row = vec![t.value()];
        match meta {
            None => {
                row.extend([Value::Null, Value::Null, Value::Null]);
                if with_vector {
                    row.push(Value::Null);
                }
            }
            Some(m) => {
                row.push(Value::Int(m.revision as i64));
                row.push(opt_value(&m.hash));
                row.push(Value::Text(judge.judge(g, &tb, m.revision).0.name().into()));
                if with_vector {
                    row.push(match g.ext_get(NS, &rec_key(b'v', s.sid, &tb)) {
                        Some(v) => Value::List(
                            unpack(&v)
                                .into_iter()
                                .map(|x| Value::Float(x as f64))
                                .collect(),
                        ),
                        None => Value::Null,
                    });
                }
            }
        }
        rows.push(row);
    }
    let mut cols = vec![
        target_col(&s),
        "revision".into(),
        "hash".into(),
        "state".into(),
    ];
    if with_vector {
        cols.push("vector".into());
    }
    let mut r = QueryResult::table(cols, rows);
    r.message = Some(format!("at revision {}", g.revision()));
    Ok(r)
}

fn delete_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let s = space_or_err(g, &name_arg(a)?)?;
    let targets = targets_arg(a, &s, false)?;
    let n = delete(g, &s, &targets)?;
    let mut r = QueryResult::message(format!("deleted {n} vectors from space '{}'", s.name));
    r.touched = n as usize;
    Ok(r)
}

enum Freshness {
    Report,
    Require,
}

fn fresh_arg(a: &Args) -> Result<Freshness> {
    match a.str("fresh").as_deref() {
        None | Some("report") => Ok(Freshness::Report),
        Some("require") => Ok(Freshness::Require),
        Some(o) => Err(Error::Msg(format!(
            "fresh: '{o}' is not one of report, require"
        ))),
    }
}

fn search_opts(a: &Args) -> Result<Search> {
    let top = a.usize("top", 10);
    if top > MAX_ROWS {
        return Err(Error::Msg(format!("top: at most {MAX_ROWS}")));
    }
    let min_score = match a.get("min_score") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .ok_or_else(|| Error::Msg(format!("min_score: expected a number, got {v}")))?,
        ),
    };
    Ok(Search {
        top,
        labels: a.strs("labels"),
        min_score,
        exclude: None,
    })
}

/// Search, then say how the searched space stands (D4): each row's state,
/// and the space's counts in the message; with `fresh: "require"` anything
/// but clean is an error.
fn search_result(
    g: &mut Graph,
    s: &Space,
    q: &[f64],
    opts: &Search,
    fresh: Freshness,
    counts: bool,
    from: Option<(Fresh, String)>,
) -> Result<QueryResult> {
    let judge = Judge::new(g, s);
    let cur = current(g, s)?;
    let require = matches!(fresh, Freshness::Require);
    if require {
        if let Some((f, why)) = &from {
            if *f != Fresh::Fresh {
                return Err(Error::Msg(format!(
                    "the vector searched from is {}: {why}; with fresh: \"require\" put it again first",
                    f.name()
                )));
            }
        }
    }
    // The exact counts: asked for, or needed because the space is not
    // known to be current and freshness is required. A question never
    // writes: a clean count is reported, not recorded.
    let known = matches!(cur, Current::Yes { .. });
    let mut tally_note = String::new();
    if require && known {
        tally_note = "; fresh required: established from the record, without a count".into();
    }
    if counts || (require && !known) {
        let t = tally(g, s, &judge)?;
        if require && !t.clean() {
            return Err(Error::Msg(format!(
                "space '{}' is not fresh ({}); with fresh: \"require\" that is an error. CALL vector.stale(space: '{}') lists them: put them again",
                s.name,
                t.say(),
                s.name
            )));
        }
        tally_note = format!("; counted: {}", t.say());
        if require && !known {
            tally_note.push_str(&format!(
                "; fresh required and counted, since the space is not recorded complete: CALL vector.complete(space: '{}') records it, so that later searches need not count",
                s.name
            ));
        }
    }
    let found = search(g, s, q, opts)?;
    let mut rows = Vec::with_capacity(found.rows.len());
    for (t, sc) in &found.rows {
        let tb = t.bytes();
        let rev = g
            .ext_get(NS, &rec_key(b'm', s.sid, &tb))
            .and_then(|b| Meta::decode(&b))
            .map_or(u64::MAX, |m| m.revision);
        rows.push(vec![
            t.value(),
            Value::Float(*sc),
            Value::Text(judge.judge(g, &tb, rev).0.name().into()),
        ]);
    }
    let st = stats(g, s);
    let mut r = QueryResult::table(vec![target_col(s), "score".into(), "state".into()], rows);
    let from_note = match &from {
        Some((f, _)) if *f != Fresh::Fresh => format!("; the vector searched from is {}", f.name()),
        _ => String::new(),
    };
    r.message = Some(format!(
        "{} of {} vectors scored in space '{}' ({}, {} dims), exact; {}{}{}; at revision {}",
        found.scored,
        st.count,
        s.name,
        s.metric.name(),
        s.dims,
        cur.say(),
        tally_note,
        from_note,
        g.revision()
    ));
    Ok(r)
}

fn search_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let s = space_or_err(g, &name_arg(a)?)?;
    let q = floats(
        a.get("vector")
            .ok_or_else(|| Error::Msg("vector: give the query vector".into()))?,
        "vector",
    )?;
    let q = prepare(&s, &q)?;
    let opts = search_opts(a)?;
    search_result(
        g,
        &s,
        &q,
        &opts,
        fresh_arg(a)?,
        a.bool("counts", false),
        None,
    )
}

fn similar_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let from = space_or_err(g, &name_arg(a)?)?;
    let into = match a.str("in") {
        None => from.clone(),
        Some(n) => space_or_err(g, &n)?,
    };
    if let Some(why) = from.incomparable(&into) {
        return Err(Error::Msg(format!(
            "vectors of space '{}' cannot be compared with those of '{}': they differ in {why}; vectors from different models or shapes are never compared",
            from.name, into.name
        )));
    }
    let t = targets_arg(a, &from, true)?;
    let [t] = t.as_slice() else {
        return Err(Error::Msg(format!(
            "give one {} to search from",
            if from.of == Of::Key { "key:" } else { "id:" }
        )));
    };
    let Some((v, meta)) = get(g, &from, t) else {
        return Err(Error::Msg(format!(
            "{} has no vector in space '{}'; put one first",
            t.show(from.of),
            from.name
        )));
    };
    let state = Judge::new(g, &from).judge(g, &t.bytes(), meta.revision);
    let q: Vec<f64> = v.iter().map(|x| *x as f64).collect();
    let q = prepare(&into, &q)?;
    let mut opts = search_opts(a)?;
    if from.of == into.of {
        opts.exclude = Some(t.clone());
    }
    search_result(
        g,
        &into,
        &q,
        &opts,
        fresh_arg(a)?,
        a.bool("counts", false),
        Some(state),
    )
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

enum Cursor {
    Vectors(Option<Vec<u8>>),
    Missing(usize, u64),
}

fn cursor_arg(a: &Args) -> Result<Cursor> {
    let Some(c) = a.str("after") else {
        return Ok(Cursor::Vectors(None));
    };
    let bad = || Error::Msg(format!("after: '{c}' is not a cursor this procedure gave"));
    if let Some(h) = c.strip_prefix("vectors:") {
        return Ok(Cursor::Vectors(Some(unhex(h).ok_or_else(bad)?)));
    }
    if let Some(rest) = c.strip_prefix("missing:") {
        let (i, id) = rest.split_once(':').ok_or_else(bad)?;
        return Ok(Cursor::Missing(
            i.parse().map_err(|_| bad())?,
            id.parse().map_err(|_| bad())?,
        ));
    }
    Err(bad())
}

/// `vector.complete`: count the space and, only when every vector is
/// fresh and no node of its labels lacks one, record that it is complete
/// at the position it counted at. The one procedure that records it.
fn complete_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let s = space_or_err(g, &name_arg(a)?)?;
    let at = g.log_end();
    let t = tally(g, &s, &Judge::new(g, &s))?;
    let row = vec![
        Value::Text(s.name.clone()),
        Value::Bool(t.clean()),
        Value::Int(t.fresh as i64),
        Value::Int(t.stale as i64),
        Value::Int(t.missing as i64),
        Value::Int(t.unknown as i64),
    ];
    let mut r = QueryResult::table(
        ["space", "complete", "fresh", "stale", "missing", "unknown"]
            .iter()
            .map(|c| c.to_string())
            .collect(),
        vec![row],
    );
    if t.clean() {
        establish(g, &s, at)?;
        r.touched = 1;
        r.message = Some(format!(
            "space '{}' is complete: recorded at revision {at}; searches report it current until something it follows changes",
            s.name
        ));
    } else {
        r.message = Some(format!(
            "space '{}' is not complete ({}); nothing recorded. CALL vector.stale(space: '{}') lists what to put again",
            s.name,
            t.say(),
            s.name
        ));
    }
    Ok(r)
}

fn stale_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let s = space_or_err(g, &name_arg(a)?)?;
    let limit = a.usize("limit", 1_000).clamp(1, MAX_ROWS);
    let cursor = cursor_arg(a)?;
    let judge = Judge::new(g, &s);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut looked = 0u64;
    let mut next: Option<String> = None;
    if let Cursor::Vectors(after) = &cursor {
        each_record(g, b'm', &s, after.as_deref(), &mut |tb, v| {
            if rows.len() >= limit || looked >= MAX_LOOK {
                return Ok(false);
            }
            looked += 1;
            next = Some(format!("vectors:{}", hex(tb)));
            let rev = Meta::decode(v).map_or(u64::MAX, |m| m.revision);
            let (f, why) = judge.judge(g, tb, rev);
            if f != Fresh::Fresh {
                let t = Target::from_bytes(s.of, tb).map_or(Value::Text(hex(tb)), |t| t.value());
                rows.push(vec![
                    t,
                    Value::Text(f.name().into()),
                    Value::Int(rev as i64),
                    Value::Text(why),
                ]);
            }
            Ok(true)
        })?;
    }
    let vectors_done = rows.len() < limit && looked < MAX_LOOK;
    if vectors_done {
        let after = match cursor {
            Cursor::Missing(i, id) => Some((i, id)),
            Cursor::Vectors(_) => None,
        };
        next = after.map(|(i, id)| format!("missing:{i}:{id}"));
        let mut full = false;
        each_member(g, &s, after, &mut |i, id| {
            if rows.len() >= limit || looked >= MAX_LOOK {
                full = true;
                return Ok(false);
            }
            looked += 1;
            next = Some(format!("missing:{i}:{id}"));
            if !has_vector(g, &s, id) {
                rows.push(vec![
                    Value::Int(id as i64),
                    Value::Text("missing".into()),
                    Value::Null,
                    Value::Text(format!("it has label {} and no vector", s.labels[i])),
                ]);
            }
            Ok(true)
        })?;
        if !full {
            next = None;
        }
    }
    let mut r = QueryResult::table(
        vec![
            target_col(&s),
            "state".into(),
            "revision".into(),
            "why".into(),
        ],
        rows,
    );
    r.message = Some(match next {
        Some(c) => format!(
            "{} rows from {looked} looked at; more may follow: continue with after: \"{c}\"; at revision {}",
            r.rows.len(),
            g.revision()
        ),
        None => format!(
            "{} rows; that is all; at revision {}",
            r.rows.len(),
            g.revision()
        ),
    });
    Ok(r)
}

// ----------------------------------------------------------------- hooks

/// Remove the vectors a deleted node or edge had, in every space about it.
fn forget(g: &mut Graph, of: Of, id: u64) -> Result<()> {
    for s in spaces(g).into_iter().filter(|s| s.of == of) {
        delete(g, &s, &[Target::Id(id)])?;
    }
    Ok(())
}

impl Extension for Vector {
    fn name(&self) -> &str {
        NS
    }
    fn version(&self) -> &str {
        "1.0"
    }
    fn summary(&self) -> &str {
        "vectors beside the graph: named spaces of packed 32-bit floats, exact search, and which vectors no longer match their source"
    }
    fn procedures(&self) -> &[Spec] {
        PROCEDURES
    }
    fn call(&self, g: &mut Graph, procedure: &str, a: &Args) -> Result<QueryResult> {
        match procedure {
            "create" => create_proc(g, a),
            "drop" => drop_proc(g, a),
            "spaces" => spaces_proc(g, a),
            "put" => put_proc(g, a),
            "get" => get_proc(g, a, true),
            "hashes" => get_proc(g, a, false),
            "delete" => delete_proc(g, a),
            "search" => search_proc(g, a),
            "similar" => similar_proc(g, a),
            "stale" => stale_proc(g, a),
            "complete" => complete_proc(g, a),
            _ => unreachable!("dispatch checks the procedure list"),
        }
    }
    /// Any space: node deletes remove vectors, and `CLEAR` resets counts.
    fn wants_writes(&self, g: &Graph) -> bool {
        installed_spaces_want(g, None)
    }
    fn wants_edge_writes(&self, g: &Graph) -> bool {
        installed_spaces_want(g, Some(Of::Edge))
    }
    fn on_node_write(
        &self,
        g: &mut Graph,
        id: u64,
        before: Option<&NodeImage>,
        after: Option<&NodeImage>,
    ) -> Result<()> {
        if before.is_some() && after.is_none() {
            forget(g, Of::Node, id)?;
        }
        Ok(())
    }
    fn on_edge_write(
        &self,
        g: &mut Graph,
        id: u64,
        before: Option<&EdgeImage>,
        after: Option<&EdgeImage>,
    ) -> Result<()> {
        if before.is_some() && after.is_none() {
            forget(g, Of::Edge, id)?;
        }
        Ok(())
    }
    fn on_bulk_write(&self, g: &mut Graph, what: BulkWrite) -> Result<()> {
        // `CLEAR` has wiped every vector and count already (they are not
        // configuration) and kept the definitions: they now hold nothing.
        if what == BulkWrite::Clear {
            let now = g.log_end();
            let puts = spaces(g)
                .iter()
                .map(|s| {
                    let st = Stats {
                        count: 0,
                        bytes: 0,
                        changed: now,
                    };
                    (stats_key(s.sid), st.encode())
                })
                .collect();
            g.ext_write(NS, puts, Vec::new())?;
        }
        Ok(())
    }
    fn check(&self, g: &Graph, probe: &mut super::check::Probe) -> Result<bool> {
        check_vectors(g, probe)
    }
}

// ----------------------------------------------------------------- check

/// `check.run`'s `derived` check: every record decodes and belongs to a
/// space; a vector has the space's length and finite values, its metadata
/// beside it, and a node or edge that exists; no revision is past the
/// log's end; and, over a walk from the start in one slice, each space's
/// count and bytes are what its records add up to.
fn check_vectors(g: &Graph, probe: &mut super::check::Probe) -> Result<bool> {
    let whole = probe.after().is_none();
    let end = g.log_end();
    let by_sid: BTreeMap<u32, Space> = spaces(g).into_iter().map(|s| (s.sid, s)).collect();
    // Per space: vectors, metadata records, bytes, as found.
    let mut found: BTreeMap<u32, (u64, u64, u64)> = BTreeMap::new();
    let mut after: Option<Vec<u8>> = probe.after().map(|a| a.to_vec());
    loop {
        let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        g.ext_scan_after(NS, b"", after.as_deref(), &mut |k, v| {
            batch.push((k.to_vec(), v.to_vec()));
            batch.len() < SCAN_BATCH
        });
        if batch.is_empty() {
            break;
        }
        for (k, v) in batch {
            if !probe.look()? {
                probe.stop_at(after.unwrap_or_default());
                return Ok(true);
            }
            check_record(g, probe, &by_sid, &mut found, end, &k, &v);
            after = Some(k);
        }
    }
    if whole {
        for (sid, s) in &by_sid {
            let (vs, ms, bytes) = found.get(sid).copied().unwrap_or_default();
            let st = match g
                .ext_get(NS, &stats_key(*sid))
                .and_then(|b| Stats::decode(&b))
            {
                Some(st) => st,
                None => {
                    probe.found(
                        "missing",
                        format!("space {}", s.name),
                        "the space has no count record; drop it and create it again",
                    );
                    continue;
                }
            };
            if st.count != vs || st.count != ms || st.bytes != bytes {
                probe.found(
                    "count",
                    format!("space {}", s.name),
                    format!(
                        "the space records {} vectors and {} bytes, and holds {vs} vectors, {ms} metadata records and {bytes} bytes",
                        st.count, st.bytes
                    ),
                );
            }
        }
    }
    Ok(true)
}

fn check_record(
    g: &Graph,
    probe: &mut super::check::Probe,
    by_sid: &BTreeMap<u32, Space>,
    found: &mut BTreeMap<u32, (u64, u64, u64)>,
    end: u64,
    k: &[u8],
    v: &[u8],
) {
    let bad = |probe: &mut super::check::Probe, why: &str| {
        probe.found(
            "bad_record",
            hex(k),
            format!("{why}; drop the space and create it again, or DROP EXTENSION vector"),
        )
    };
    if k == NEXT_KEY {
        if v.len() != 4 {
            bad(probe, "the space counter does not decode");
        }
        return;
    }
    if let Some(name) = k.strip_prefix(b"!d") {
        match std::str::from_utf8(name)
            .ok()
            .and_then(|n| Space::decode(n, v))
        {
            None => bad(probe, "a space definition that does not decode"),
            Some(s) if s.created > end => probe.found(
                "future",
                format!("space {}", s.name),
                format!("made at position {} past the log's end at {end}", s.created),
            ),
            Some(_) => {}
        }
        return;
    }
    let (Some(&tag), Some(sid)) = (k.first(), k.get(1..5)) else {
        return bad(probe, "a record this extension does not write");
    };
    let sid = u32::from_be_bytes(sid.try_into().unwrap());
    let Some(s) = by_sid.get(&sid) else {
        return bad(probe, "a record of a space that does not exist");
    };
    let tb = &k[5..];
    let handle = || {
        Target::from_bytes(s.of, tb).map_or_else(
            || format!("space {} {}", s.name, hex(tb)),
            |t| match t {
                Target::Id(id) if s.of == Of::Edge => format!("e{id} in {}", s.name),
                Target::Id(id) => format!("n{id} in {}", s.name),
                Target::Key(key) => format!("{key} in {}", s.name),
            },
        )
    };
    match tag {
        b'k' if tb.is_empty() => {
            if v.len() != 16 {
                bad(probe, "a space's completeness record that does not decode");
            } else if u64::from_be_bytes(v[..8].try_into().unwrap()) > end
                || u64::from_be_bytes(v[8..].try_into().unwrap()) > end
            {
                probe.found(
                    "future",
                    format!("space {}", s.name),
                    format!("established complete at a position past the log's end at {end}"),
                );
            }
        }
        b'c' if tb.is_empty() => match Stats::decode(v) {
            None => bad(probe, "a space's count record that does not decode"),
            Some(st) if st.changed > end => probe.found(
                "future",
                format!("space {}", s.name),
                format!(
                    "changed at position {} past the log's end at {end}",
                    st.changed
                ),
            ),
            Some(_) => {}
        },
        b'v' | b'm' => {
            let Some(t) = Target::from_bytes(s.of, tb).filter(|_| s.of == Of::Key || tb.len() == 8)
            else {
                return bad(probe, "a vector record whose key does not decode");
            };
            let e = found.entry(sid).or_default();
            e.2 += rec_bytes(k.len(), v.len()) as u64;
            let other = if tag == b'v' { b'm' } else { b'v' };
            if g.ext_get(NS, &rec_key(other, sid, tb)).is_none() {
                probe.found(
                    "unpaired",
                    handle(),
                    if tag == b'v' {
                        "a vector with no metadata record"
                    } else {
                        "metadata with no vector"
                    },
                );
            }
            if tag == b'v' {
                e.0 += 1;
                if v.len() != s.dims * 4 {
                    probe.found(
                        "length",
                        handle(),
                        format!(
                            "{} bytes where the space's {} dims take {}",
                            v.len(),
                            s.dims,
                            s.dims * 4
                        ),
                    );
                } else if let Some(i) = unpack(v).iter().position(|x| !x.is_finite()) {
                    probe.found("not_finite", handle(), format!("element {i} is not finite"));
                }
            } else {
                e.1 += 1;
                match Meta::decode(v) {
                    None => bad(probe, "a vector's metadata that does not decode"),
                    Some(m) if m.revision > end => probe.found(
                        "future",
                        handle(),
                        format!(
                            "written at position {} past the log's end at {end}",
                            m.revision
                        ),
                    ),
                    Some(_) => {}
                }
                if !exists(g, s.of, &t) {
                    probe.found(
                        "orphan",
                        handle(),
                        format!("a vector for a {} that does not exist", s.of.name()),
                    );
                }
            }
        }
        _ => bad(probe, "a record this extension does not write"),
    }
}
