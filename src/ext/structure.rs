//! `structure`: which nodes are the same shape.
//!
//! Graphs made by people repeat themselves: forty stations built to one
//! design, each with the same sides, locations, work elements and tools.
//! This extension decides which nodes have the same surroundings, the
//! primitive that finding repeated shapes stands on:
//!
//! ```text
//! CREATE EXTENSION structure
//! CALL structure.colors(depth: 4, types: ["HAS_SIDE", ...], dir: "out")
//! CALL structure.colors(depth: "stable", max_depth: 32, labels: ["Station"])
//! CALL structure.colors(depth: 4, store: "designs")        keep it
//! CALL structure.read(name: "designs", fresh: "require")   read it back
//! CALL structure.classes(by: "neighbours", labels: ["Tool"], keys: ["kind", "model"])
//! CALL structure.classes(by: "colors", colors: "designs")
//! CALL structure.stored()                                  what is kept
//! CALL structure.drop(name: "designs")
//! ```
//!
//! **Colours** are Weisfeiler-Lehman refinement. At depth 0 a node's colour
//! is a hash of its labels and of the values of the properties named in
//! `props:` (none by default, so names and codes do not make every node
//! unique). At depth k+1 it is a hash of its depth-k colour and the
//! multiset of (direction, edge type, depth-k colour of the node at the
//! other end) over its edges of `types:` (all when not given) in `dir:`
//! (`out` by default; `both` counts the two directions apart). Labels, edge
//! types and property keys enter by name and values by content, never by
//! id, so the same data under other node ids gets the same colours (D7).
//!
//! The hash is SipHash-2-4 with 128-bit output under a fixed key; the
//! multiset of a node's neighbours is the sum, modulo 2^128, of one hash per
//! edge, so neither order nor memory per node depends on its degree. Equal
//! surroundings always get equal colours: a colour is a function of the
//! surroundings. Two different surroundings share one only by a collision:
//! for colours treated as random 128-bit values, the chance that any two of
//! a billion nodes collide at one depth is below n^2 / 2^129, about 1.5e-21.
//! The sum weakens that only for multiplicities near 2^40 and above
//! (a difference of c = 2^j times an odd number in one neighbour's count is
//! missed with chance 2^-(128-j)), still below 1e-9 at a billion nodes with
//! a trillion edges.
//!
//! **Classes** group interchangeable nodes. `by: "neighbours"` groups nodes
//! with the same labels, the same values on `keys:` (an absent key is a
//! value of its own) and exactly the same multiset of (direction, edge type,
//! neighbour) over `types:` in `dir:`. That is exact: candidates are found by
//! a hash and then compared in full (see [`same_neighbours`]). `by: "colors"`
//! groups the nodes of a colouring, stored or computed in the call, by
//! colour. Classes come largest first; among equal sizes, by the smallest
//! member for `neighbours` (by `identity:` when given) and by colour for
//! `colors`. A class is a set of distinct nodes that the structure cannot
//! tell apart; it is never evidence that they are one thing, and nothing
//! here merges, relabels or connects anything (D1).
//!
//! **Kept colourings (D3, D4).** `store: "<name>"` keeps a colouring in this
//! extension's keyspace with the log position it was computed at, its
//! arguments and the code that wrote it. Reading it says whether the
//! database has moved since; `fresh: "require"` makes a moved database an
//! error. The extension's own writes do not count as a move: every write it
//! makes ends with a record of the stretch of log that holds nothing but its
//! own writes, so storing or dropping one colouring leaves the others fresh,
//! and anything else written anywhere (a node, an edge, another extension)
//! makes every kept colouring stale. Without `store:` or `write:` nothing is
//! written and the revision does not move.
//!
//! **Bounds (D6).** Per-node state (ids, two colour arrays, the selection)
//! is `ooc::StateVec`s inside half of `algorithm_memory()`, spilling past
//! it; distinct colours are counted, and classes are formed, by external
//! sorts in a quarter of it. Rows come in slices (`after:`, `limit:`, at
//! most [`MAX_ROWS`] a call). A colouring is a whole-graph computation, so
//! a slice of a colouring computed in the call bounds what it returns, not
//! what it reads; a kept colouring is read in true slices.
//!
//! Every loop over nodes or edges passes through [`step`], the one place
//! the statement budget (slice G2) is to be counted.

use super::{Args, Extension, Spec};
use crate::graph::{Dir, Error, Graph, Result};
use crate::ooc::{self, SpillQueue, StateVec};
use crate::query::QueryResult;
use crate::storage::extsort::Sorter;
use crate::store::Op;
use crate::value::Value;

pub struct Structure;

/// The keyspace.
const NS: &str = "structure";

/// The version of the code that writes colourings, recorded with each (D3).
/// A change to how colours are computed changes it.
pub const CODE: &str = "structure-colors/1";

/// Rows one call returns at most.
pub const MAX_ROWS: usize = 10_000;

/// Depth for `depth: "stable"` when `max_depth:` is not given.
pub const STABLE_CAP: u32 = 32;

/// Members listed per class when `members:` is not given.
const MEMBERS: usize = 1_000;

/// Records per write to the keyspace.
const WRITE_BATCH: usize = 4_096;

const PROCEDURES: &[Spec] = &[
    Spec {
        name: "colors",
        args: "(depth, max_depth, types, dir, props, labels, identity, store, write, after, limit, summary)",
        doc: "Weisfeiler-Lehman colours: equal exactly when the surroundings to depth are indistinguishable by labels, props and typed directed edges; rows id, color",
    },
    Spec {
        name: "classes",
        args: "(by, labels, keys, types, dir, colors, identity, min_size, members, after, limit, summary, fresh, depth, props)",
        doc: "Interchangeable nodes, largest class first: by 'neighbours' (same labels, keys and typed neighbours) or 'colors'; members are distinct nodes, never merged",
    },
    Spec {
        name: "read",
        args: "(name, after, limit, identity, fresh)",
        doc: "A kept colouring, in id order, saying whether the database moved since it was computed",
    },
    Spec {
        name: "stored",
        args: "()",
        doc: "The kept colourings: name, revision, fresh, depth, stable, nodes, colors, code, args",
    },
    Spec {
        name: "drop",
        args: "(name)",
        doc: "Forget a kept colouring",
    },
];

impl Extension for Structure {
    fn name(&self) -> &str {
        "structure"
    }
    fn version(&self) -> &str {
        "1.0"
    }
    fn summary(&self) -> &str {
        "which nodes are the same shape: Weisfeiler-Lehman colours and classes of interchangeable nodes; reports, never merges"
    }
    fn procedures(&self) -> &[Spec] {
        PROCEDURES
    }
    fn call(&self, g: &mut Graph, procedure: &str, a: &Args) -> Result<QueryResult> {
        let memory = (g.algorithm_memory() / 2).max(64 << 10);
        let dir = g.temp_dir();
        ooc::with_budget(memory, dir, || match procedure {
            "colors" => colors(g, a),
            "classes" => classes(g, a),
            "read" => read(g, a),
            "stored" => stored(g),
            "drop" => drop_colouring(g, a),
            _ => unreachable!("dispatch checks the procedure list"),
        })
    }
    fn check(&self, g: &Graph, probe: &mut super::check::Probe) -> Result<bool> {
        // A kept colouring is not maintained as the graph changes: going
        // stale is what it is expected to do, and `read` says so (D4). What
        // can be wrong with it as kept is a record that does not decode.
        let mut after: Option<Vec<u8>> = probe.after().map(|a| a.to_vec());
        loop {
            let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            g.ext_scan_after(NS, b"", after.as_deref(), &mut |k, v| {
                batch.push((k.to_vec(), v.to_vec()));
                batch.len() < 512
            });
            if batch.is_empty() {
                return Ok(true);
            }
            for (k, v) in batch {
                if !probe.look()? {
                    // Empty before the first record: no key is empty, so
                    // the next slice starts at the first.
                    probe.stop_at(after.unwrap_or_default());
                    return Ok(true);
                }
                let sound = match k.first() {
                    Some(b'n') => k.len() > 9 && v.len() == 16,
                    Some(b'm') => Meta::decode(&v).is_some(),
                    Some(b'w') => k == CHAIN && v.len() == 16,
                    _ => false,
                };
                if !sound {
                    probe.found("bad_record", hex(&k), "a kept colouring's record that does not decode; drop the colouring and compute it again");
                }
                after = Some(k);
            }
        }
    }
}

/// One statement step per node or edge looked at ([`crate::meter`]): the
/// only place this extension's loops count, but for the scans of one
/// node's edges, which count each edge as they go so that a hub's stops
/// with the statement.
#[inline]
fn step(n: u64) -> Result<()> {
    crate::meter::steps(n)
}

// ------------------------------------------------------------- the hash

/// SipHash-2-4 with 128-bit output (Aumasson and Bernstein), under a fixed
/// key so that colours are the same in every database and every process.
pub fn sip128(data: &[u8]) -> u128 {
    const K0: u64 = 0x676c_6964_6572_2d77; // "glider-w"
    const K1: u64 = 0x6c2d_636f_6c6f_7572; // "l-colour"
    sip128_keyed(K0, K1, data)
}

fn sip128_keyed(k0: u64, k1: u64, data: &[u8]) -> u128 {
    let mut v0 = k0 ^ 0x736f_6d65_7073_6575;
    let mut v1 = k1 ^ 0x646f_7261_6e64_6f6d ^ 0xee;
    let mut v2 = k0 ^ 0x6c79_6765_6e65_7261;
    let mut v3 = k1 ^ 0x7465_6462_7974_6573;
    macro_rules! round {
        () => {
            v0 = v0.wrapping_add(v1);
            v1 = v1.rotate_left(13);
            v1 ^= v0;
            v0 = v0.rotate_left(32);
            v2 = v2.wrapping_add(v3);
            v3 = v3.rotate_left(16);
            v3 ^= v2;
            v0 = v0.wrapping_add(v3);
            v3 = v3.rotate_left(21);
            v3 ^= v0;
            v2 = v2.wrapping_add(v1);
            v1 = v1.rotate_left(17);
            v1 ^= v2;
            v2 = v2.rotate_left(32);
        };
    }
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        let m = u64::from_le_bytes(c.try_into().unwrap());
        v3 ^= m;
        round!();
        round!();
        v0 ^= m;
    }
    let mut last = [0u8; 8];
    let rest = chunks.remainder();
    last[..rest.len()].copy_from_slice(rest);
    let b = u64::from_le_bytes(last) | ((data.len() as u64) << 56);
    v3 ^= b;
    round!();
    round!();
    v0 ^= b;
    v2 ^= 0xee;
    round!();
    round!();
    round!();
    round!();
    let lo = v0 ^ v1 ^ v2 ^ v3;
    v1 ^= 0xdd;
    round!();
    round!();
    round!();
    round!();
    let hi = v0 ^ v1 ^ v2 ^ v3;
    (lo as u128) | ((hi as u128) << 64)
}

type Colour = (u64, u64);

fn split(h: u128) -> Colour {
    ((h >> 64) as u64, h as u64)
}

fn join(c: Colour) -> u128 {
    ((c.0 as u128) << 64) | c.1 as u128
}

fn colour_bytes(c: Colour) -> [u8; 16] {
    join(c).to_be_bytes()
}

pub fn colour_hex(c: Colour) -> String {
    format!("{:032x}", join(c))
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn u64_at(k: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(k[at..at + 8].try_into().unwrap())
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// A value as bytes that are equal exactly when the values are equal as
/// the engine compares them (`1` equals `1.0`), in an order that is the
/// same everywhere. Lists are walked with a stack, not by recursion.
pub fn canonical(out: &mut Vec<u8>, v: Option<&Value>) {
    let Some(v) = v else {
        out.push(b'a');
        return;
    };
    let mut stack: Vec<std::slice::Iter<'_, Value>> = Vec::new();
    let mut next = Some(v);
    loop {
        let v = match next.take() {
            Some(v) => v,
            None => match stack.last_mut() {
                None => return,
                Some(it) => match it.next() {
                    Some(v) => v,
                    None => {
                        stack.pop();
                        continue;
                    }
                },
            },
        };
        match v {
            Value::Null => out.push(b'n'),
            Value::Bool(b) => {
                out.push(b'b');
                out.push(*b as u8);
            }
            Value::Int(i) => int(out, *i),
            Value::Float(f) => {
                let f = *f;
                if f.is_finite() && f.fract() == 0.0 && (-9.2e18..9.2e18).contains(&f) {
                    int(out, f as i64);
                } else {
                    out.push(b'f');
                    let bits = if f.is_nan() {
                        f64::NAN.to_bits()
                    } else {
                        f.to_bits()
                    };
                    out.extend_from_slice(&bits.to_be_bytes());
                }
            }
            Value::Text(s) => {
                out.push(b't');
                put_str(out, s);
            }
            Value::List(items) => {
                out.push(b'l');
                out.extend_from_slice(&(items.len() as u32).to_be_bytes());
                stack.push(items.iter());
            }
        }
    }
}

fn int(out: &mut Vec<u8>, i: i64) {
    out.push(b'i');
    out.extend_from_slice(&((i as u64) ^ (1 << 63)).to_be_bytes());
}

// ------------------------------------------------------------- arguments

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Depth {
    Fixed(u32),
    /// Until the partition stops changing, at most this deep.
    Stable(u32),
}

/// What a colouring is computed from: everything that changes its colours.
#[derive(Clone, Debug, PartialEq)]
struct ColorArgs {
    depth: Depth,
    /// Sorted, distinct; empty: every type.
    types: Vec<String>,
    dir: Dir,
    /// Sorted, distinct.
    props: Vec<String>,
    /// Sorted, distinct; empty: every node.
    labels: Vec<String>,
}

fn names(a: &Args, key: &str) -> Vec<String> {
    let mut v = a.strs(key);
    v.sort();
    v.dedup();
    v
}

fn dir_arg(a: &Args) -> Result<Dir> {
    match a.str("dir") {
        None => Ok(Dir::Out),
        Some(d) => Dir::parse(&d).ok_or_else(|| {
            Error::Msg(format!(
                "dir: '{d}' is not a direction; use out, in or both"
            ))
        }),
    }
}

fn dir_name(d: Dir) -> &'static str {
    match d {
        Dir::Out => "out",
        Dir::In => "in",
        Dir::Both => "both",
    }
}

fn json_list(v: &[String]) -> String {
    let items: Vec<String> = v.iter().map(|s| Value::Text(s.clone()).to_json()).collect();
    format!("[{}]", items.join(","))
}

impl ColorArgs {
    fn from(a: &Args) -> Result<ColorArgs> {
        let cap = match a.get("max_depth") {
            None | Some(Value::Null) => STABLE_CAP,
            Some(v) => match v.as_i64() {
                Some(n) if (0..=1024).contains(&n) => n as u32,
                _ => {
                    return Err(Error::Msg(format!(
                        "max_depth: {v} is not a depth; give 0 to 1024"
                    )))
                }
            },
        };
        let depth = match a.get("depth") {
            None | Some(Value::Null) => Depth::Fixed(3),
            Some(Value::Text(t)) if t.eq_ignore_ascii_case("stable") => Depth::Stable(cap),
            Some(v) => match v.as_i64() {
                Some(n) if (0..=1024).contains(&n) => Depth::Fixed(n as u32),
                _ => {
                    return Err(Error::Msg(format!(
                        "depth: {v} is not a depth; give 0 to 1024, or \"stable\" to refine until nothing changes"
                    )))
                }
            },
        };
        let mut labels = names(a, "labels");
        labels.extend(a.str("label"));
        labels.sort();
        labels.dedup();
        Ok(ColorArgs {
            depth,
            types: names(a, "types"),
            dir: dir_arg(a)?,
            props: names(a, "props"),
            labels,
        })
    }

    /// The arguments as recorded with a kept colouring.
    fn json(&self) -> String {
        let depth = match self.depth {
            Depth::Fixed(d) => d.to_string(),
            Depth::Stable(cap) => format!("\"stable\",\"max_depth\":{cap}"),
        };
        format!(
            "{{\"depth\":{depth},\"types\":{},\"dir\":\"{}\",\"props\":{},\"labels\":{}}}",
            json_list(&self.types),
            dir_name(self.dir),
            json_list(&self.props),
            json_list(&self.labels)
        )
    }
}

fn limit_arg(a: &Args) -> Result<usize> {
    match a.get("limit") {
        None | Some(Value::Null) => Ok(MAX_ROWS),
        Some(v) => match v.as_i64() {
            Some(n) if n >= 1 => Ok((n as usize).min(MAX_ROWS)),
            _ => Err(Error::Msg(format!(
                "limit: {v} is not a number of rows; give 1 or more"
            ))),
        },
    }
}

fn after_id(a: &Args) -> Result<u64> {
    Ok(a.ids("after")?.first().copied().unwrap_or(0))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fresh {
    Report,
    Require,
}

fn fresh_arg(a: &Args) -> Result<Fresh> {
    match a.str("fresh").map(|s| s.to_ascii_lowercase()).as_deref() {
        None | Some("report") | Some("allow") => Ok(Fresh::Report),
        Some("require") => Ok(Fresh::Require),
        Some(other) => Err(Error::Msg(format!(
            "fresh: '{other}' is not a choice; use \"report\" (say whether it is stale) or \"require\" (refuse a stale one)"
        ))),
    }
}

fn store_name(a: &Args, key: &str) -> Result<Option<String>> {
    let Some(n) = a.str(key) else {
        return Ok(None);
    };
    if n.is_empty()
        || n.len() > 64
        || !n
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
    {
        return Err(Error::Msg(format!(
            "{key}: '{n}' is not a colouring name; use 1 to 64 letters, digits, '_', '-' or '.'"
        )));
    }
    Ok(Some(n))
}

// --------------------------------------------------------- node positions

/// The dense position of `id` in the ascending id array, trying the dense
/// guess first (ids are usually close to contiguous).
fn index_of(ids: &mut StateVec<u64>, len: usize, id: u64) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let first = ids.get(0);
    if id < first {
        return None;
    }
    let guess = id - first;
    if guess < len as u64 && ids.get(guess as usize) == id {
        return Some(guess as usize);
    }
    let (mut lo, mut hi) = (0usize, len.min(guess as usize + 1));
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

/// The first position whose id is greater than `after`.
fn first_after(ids: &mut StateVec<u64>, len: usize, after: u64) -> usize {
    let (mut lo, mut hi) = (0usize, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if ids.get(mid) <= after {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn sorter(g: &Graph, name: &str) -> Result<Sorter> {
    let budget = (g.algorithm_memory() / 4).max(64 << 10) as usize;
    Sorter::new(&g.temp_dir(), name, budget).map_err(Error::Io)
}

// ---------------------------------------------------------- the colouring

/// A colouring of every node, with the nodes it was asked about marked.
struct Colouring {
    ids: StateVec<u64>,
    colors: StateVec<Colour>,
    selected: StateVec<bool>,
    len: usize,
    /// The depth of the colours held.
    depth: u32,
    /// The depth from which the partition no longer changed, when a round
    /// showed it.
    stable: Option<u32>,
    /// Per depth from 0 to `depth`: distinct colours over every node, and
    /// over the selected ones.
    counts: Vec<(u64, u64)>,
}

/// Distinct colours over every node and over the selected ones.
fn count(
    g: &Graph,
    len: usize,
    colors: &mut StateVec<Colour>,
    sel: &mut StateVec<bool>,
) -> Result<(u64, u64)> {
    let mut s = sorter(g, "structure-count")?;
    let mut key = [0u8; 17];
    for v in 0..len {
        step(1)?;
        key[..16].copy_from_slice(&colour_bytes(colors.get(v)));
        key[16] = sel.get(v) as u8;
        s.push(&key, &[]).map_err(Error::Io)?;
    }
    let mut sorted = s.finish().map_err(Error::Io)?;
    let (mut all, mut some) = (0u64, 0u64);
    let mut last: Option<Vec<u8>> = None;
    let mut last_sel: Option<Vec<u8>> = None;
    while let Some((k, _)) = sorted.next().map_err(Error::Io)? {
        if last.as_deref() != Some(&k[..16]) {
            all += 1;
            last = Some(k[..16].to_vec());
        }
        if k[16] == 1 && last_sel.as_deref() != Some(&k[..16]) {
            some += 1;
            last_sel = Some(k[..16].to_vec());
        }
    }
    Ok((all, some))
}

/// The hash of a type's name, for each interned string that is a type the
/// colouring follows; None for the rest.
fn followed_types(g: &Graph, types: &[String]) -> Vec<Option<u128>> {
    (0..g.strings.len() as u32)
        .map(|t| {
            let name = g.strings.name(t);
            if types.is_empty() || types.binary_search_by(|x| x.as_str().cmp(name)).is_ok() {
                let mut b = vec![b'T'];
                put_str(&mut b, name);
                Some(sip128(&b))
            } else {
                None
            }
        })
        .collect()
}

fn sides(dir: Dir) -> &'static [(u8, Dir)] {
    match dir {
        Dir::Out => &[(b'>', Dir::Out)],
        Dir::In => &[(b'<', Dir::In)],
        Dir::Both => &[(b'>', Dir::Out), (b'<', Dir::In)],
    }
}

fn compute(g: &Graph, ca: &ColorArgs) -> Result<Colouring> {
    let n = g.node_count();
    let mut ids = StateVec::new(n, 0u64);
    let mut cur = StateVec::new(n, (0u64, 0u64));
    let mut sel = StateVec::new(n, false);
    let label_ids: Vec<u32> = ca
        .labels
        .iter()
        .filter_map(|l| g.strings.lookup(l))
        .collect();
    let prop_ids: Vec<Option<u32>> = ca.props.iter().map(|p| g.strings.lookup(p)).collect();

    // Depth 0: labels and the named properties, by name and content.
    let mut len = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    let mut failed: Option<Error> = None;
    g.scan_nodes(None, 0, usize::MAX, &mut |id| {
        if len == n {
            return false;
        }
        if let Err(e) = step(1) {
            failed = Some(e);
            return false;
        }
        let Some(node) = g.node_shared(id) else {
            return true;
        };
        names.clear();
        names.extend(node.labels().iter().map(|&l| g.strings.name(l)));
        names.sort_unstable();
        names.dedup();
        buf.clear();
        buf.push(b'0');
        buf.extend_from_slice(&(names.len() as u32).to_be_bytes());
        for l in &names {
            put_str(&mut buf, l);
        }
        for (name, key) in ca.props.iter().zip(&prop_ids) {
            put_str(&mut buf, name);
            let v = key.and_then(|k| node.prop(k));
            canonical(&mut buf, v.as_ref());
        }
        ids.set(len, id);
        cur.set(len, split(sip128(&buf)));
        let picked = ca.labels.is_empty() || node.labels().iter().any(|l| label_ids.contains(l));
        sel.set(len, picked);
        len += 1;
        true
    });
    if let Some(e) = failed {
        return Err(e);
    }

    let types = followed_types(g, &ca.types);
    let mut counts = vec![count(g, len, &mut cur, &mut sel)?];
    let mut depth = 0u32;
    let mut stable = None;
    let mut elem = [0u8; 33];
    let mut node = [0u8; 41];
    loop {
        match ca.depth {
            Depth::Fixed(d) if depth >= d => break,
            Depth::Stable(cap) if depth >= cap => break,
            _ => {}
        }
        let mut next = StateVec::new(len, (0u64, 0u64));
        for v in 0..len {
            let id = ids.get(v);
            let mut sum = 0u128;
            let mut edges = 0u64;
            let mut tally = crate::meter::Tally::new();
            for &(tag, d) in sides(ca.dir) {
                g.for_each_adj_while(id, d, None, |a| {
                    if tally.spent(1) {
                        return false;
                    }
                    let Some(Some(th)) = types.get(a.etype as usize) else {
                        return true;
                    };
                    let Some(p) = index_of(&mut ids, len, a.other) else {
                        return true;
                    };
                    elem[0] = tag;
                    elem[1..17].copy_from_slice(&th.to_be_bytes());
                    elem[17..].copy_from_slice(&colour_bytes(cur.get(p)));
                    sum = sum.wrapping_add(sip128(&elem));
                    edges += 1;
                    true
                });
            }
            tally.flush();
            step(1)?;
            node[0] = b'r';
            node[1..17].copy_from_slice(&colour_bytes(cur.get(v)));
            node[17..25].copy_from_slice(&edges.to_be_bytes());
            node[25..].copy_from_slice(&sum.to_be_bytes());
            next.set(v, split(sip128(&node)));
        }
        let c = count(g, len, &mut next, &mut sel)?;
        let unchanged = c.0 == counts[counts.len() - 1].0;
        if unchanged && stable.is_none() {
            stable = Some(depth);
        }
        if unchanged && matches!(ca.depth, Depth::Stable(_)) {
            // The colours one round back already make this partition.
            break;
        }
        cur = next;
        counts.push(c);
        depth += 1;
    }
    Ok(Colouring {
        ids,
        colors: cur,
        selected: sel,
        len,
        depth,
        stable,
        counts,
    })
}

fn counts_text(counts: &[(u64, u64)]) -> String {
    counts
        .iter()
        .enumerate()
        .map(|(d, (all, sel))| format!("{d}: {sel}/{all}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn identity_value(g: &Graph, key: Option<&str>, id: u64) -> Value {
    match key {
        Some(k) => g.node_prop(id, k).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

fn colors(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let ca = ColorArgs::from(a)?;
    let store = store_name(a, "store")?;
    let write = a.str("write");
    if let Some(w) = &write {
        if ca.props.iter().any(|p| p == w) {
            return Err(Error::Msg(format!(
                "write: '{w}' is one of props:, so writing it would change the colours it records; write to another key"
            )));
        }
    }
    let identity = a.str("identity");
    let limit = limit_arg(a)?;
    let after = after_id(a)?;
    let summary = a.bool("summary", false);
    let mut col = compute(g, &ca)?;

    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut next: Option<u64> = None;
    if summary {
        for (d, (all, sel)) in col.counts.iter().enumerate() {
            rows.push(vec![
                Value::Int(d as i64),
                Value::Int(*all as i64),
                Value::Int(*sel as i64),
            ]);
        }
    } else {
        let from = first_after(&mut col.ids, col.len, after);
        for v in from..col.len {
            step(1)?;
            if !col.selected.get(v) {
                continue;
            }
            let id = col.ids.get(v);
            if rows.len() == limit {
                next = Some(rows.last().and_then(|r| r[0].as_i64()).unwrap_or(0) as u64);
                break;
            }
            let mut row = vec![
                Value::Int(id as i64),
                Value::Text(colour_hex(col.colors.get(v))),
            ];
            if identity.is_some() {
                row.push(identity_value(g, identity.as_deref(), id));
            }
            rows.push(row);
        }
    }

    let mut touched = 0u64;
    if let Some(key) = &write {
        for v in 0..col.len {
            step(1)?;
            if col.selected.get(v) {
                let id = col.ids.get(v);
                g.set_node_prop(id, key, Value::Text(colour_hex(col.colors.get(v))))?;
                touched += 1;
            }
        }
    }
    if let Some(name) = &store {
        touched += keep(g, name, &ca, &mut col)?;
    }

    let columns: Vec<String> = if summary {
        vec!["depth".into(), "colors".into(), "selected".into()]
    } else if identity.is_some() {
        vec!["id".into(), "color".into(), "identity".into()]
    } else {
        vec!["id".into(), "color".into()]
    };
    let selected = col.counts.last().map(|c| c.1).unwrap_or(0);
    let mut msg = format!(
        "{} colours among the {} nodes asked about at depth {} ({} nodes in all; colours per depth, asked/all: {})",
        selected,
        count_selected(&mut col)?,
        col.depth,
        col.len,
        counts_text(&col.counts)
    );
    match (ca.depth, col.stable) {
        (Depth::Stable(_), Some(d)) => {
            msg.push_str(&format!("; stable: nothing changes past depth {d}"))
        }
        (Depth::Stable(cap), None) => msg.push_str(&format!(
            "; not stable after {cap} rounds (the cap; raise max_depth:)"
        )),
        (_, Some(d)) => msg.push_str(&format!("; the partition stops changing at depth {d}")),
        _ => {}
    }
    if let Some(name) = &store {
        msg.push_str(&format!("; kept as '{name}'"));
    }
    if let Some(n) = next {
        msg.push_str(&format!(
            "; more: CALL structure.colors again with after: {n} and the same arguments"
        ));
    }
    let mut r = QueryResult::table(columns, rows);
    r.message = Some(msg);
    r.touched = touched as usize;
    Ok(r)
}

fn count_selected(col: &mut Colouring) -> Result<u64> {
    let mut n = 0;
    for v in 0..col.len {
        n += col.selected.get(v) as u64;
    }
    Ok(n)
}

// ------------------------------------------------------- kept colourings

/// What is kept about a colouring beside its colours.
#[derive(Clone, Debug, PartialEq)]
struct Meta {
    /// The log position it was computed at: the graph as everything logged
    /// before it left it.
    at: u64,
    depth: u32,
    stable: Option<u32>,
    nodes: u64,
    colors: u64,
    code: String,
    args: String,
}

impl Meta {
    fn encode(&self) -> Vec<u8> {
        let mut out = vec![1u8];
        out.extend_from_slice(&self.at.to_be_bytes());
        out.extend_from_slice(&self.depth.to_be_bytes());
        out.extend_from_slice(&self.stable.map(|s| s as i64).unwrap_or(-1).to_be_bytes());
        out.extend_from_slice(&self.nodes.to_be_bytes());
        out.extend_from_slice(&self.colors.to_be_bytes());
        put_str(&mut out, &self.code);
        put_str(&mut out, &self.args);
        out
    }

    fn decode(b: &[u8]) -> Option<Meta> {
        fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
            if b.len() < n {
                return None;
            }
            let (x, rest) = b.split_at(n);
            *b = rest;
            Some(x)
        }
        fn s(b: &mut &[u8]) -> Option<String> {
            let n = u32::from_be_bytes(take(b, 4)?.try_into().ok()?) as usize;
            String::from_utf8(take(b, n)?.to_vec()).ok()
        }
        let mut b = b;
        if take(&mut b, 1)? != [1] {
            return None;
        }
        let at = u64::from_be_bytes(take(&mut b, 8)?.try_into().ok()?);
        let depth = u32::from_be_bytes(take(&mut b, 4)?.try_into().ok()?);
        let stable = i64::from_be_bytes(take(&mut b, 8)?.try_into().ok()?);
        let nodes = u64::from_be_bytes(take(&mut b, 8)?.try_into().ok()?);
        let colors = u64::from_be_bytes(take(&mut b, 8)?.try_into().ok()?);
        let code = s(&mut b)?;
        let args = s(&mut b)?;
        b.is_empty().then_some(Meta {
            at,
            depth,
            stable: (stable >= 0).then_some(stable as u32),
            nodes,
            colors,
            code,
            args,
        })
    }
}

fn meta_key(name: &str) -> Vec<u8> {
    let mut k = vec![b'm'];
    k.extend_from_slice(name.as_bytes());
    k
}

fn colour_prefix(name: &str) -> Vec<u8> {
    let mut k = vec![b'n'];
    k.extend_from_slice(name.as_bytes());
    k.push(0);
    k
}

const CHAIN: &[u8] = b"w";

/// The stretch of log that holds nothing but this extension's writes:
/// (where it starts, where its last record ends).
fn chain(g: &Graph) -> Option<(u64, u64)> {
    let v = g.ext_get(NS, CHAIN)?;
    (v.len() == 16).then(|| (u64_at(&v, 0), u64_at(&v, 8)))
}

/// Whether nothing but this extension has written since `end`, the end of
/// its last record: the log ends there (the transaction is still open), or
/// it ends one commit marker later and that is committed.
fn at_chain_end(g: &Graph, end: u64) -> bool {
    let now = g.log_end();
    now == end || (now == g.revision() && now == end + crate::storage::log::FRAME_BYTES)
}

/// Whether a colouring computed at log position `at` still describes the
/// graph: nothing but this extension's own writes since.
fn is_fresh(g: &Graph, at: u64) -> bool {
    match chain(g) {
        Some((from, end)) => from <= at && at_chain_end(g, end),
        None => false,
    }
}

fn full_key(key: &[u8]) -> Vec<u8> {
    let mut k = NS.as_bytes().to_vec();
    k.push(0);
    k.extend_from_slice(key);
    k
}

/// Where this extension's writes in this call start, and whether they
/// carry on the stretch its earlier writes left.
fn begin(g: &Graph) -> (u64, u64) {
    let start = g.log_end();
    let from = match chain(g) {
        Some((from, end)) if at_chain_end(g, end) => from,
        _ => start,
    };
    (start, from)
}

/// The last write of a call: `puts` and `dels` with the chain record, which
/// says where that very write ends. The record's size does not depend on
/// its value, so where it ends is known before it is written.
fn seal(
    g: &mut Graph,
    from: u64,
    mut puts: Vec<(Vec<u8>, Vec<u8>)>,
    dels: Vec<Vec<u8>>,
) -> Result<()> {
    puts.push((CHAIN.to_vec(), vec![0u8; 16]));
    let op = Op::ExtWrite {
        puts: puts.iter().map(|(k, v)| (full_key(k), v.clone())).collect(),
        dels: dels.iter().map(|k| full_key(k)).collect(),
    };
    let size = op.encode_record().len() as u64;
    let end = g.log_end() + crate::storage::log::FRAME_BYTES + size;
    let mut w = from.to_be_bytes().to_vec();
    w.extend_from_slice(&end.to_be_bytes());
    puts.last_mut().unwrap().1 = w;
    g.ext_write(NS, puts, dels)?;
    // A log that frames records otherwise leaves the chain short of the
    // log's end, and every kept colouring reads as stale: wrong on the safe
    // side, never fresh when it is not.
    debug_assert_eq!(g.log_end(), end, "a record is its encoding and one frame");
    Ok(())
}

/// Delete every colour of colouring `name`, a batch at a time.
fn forget_colours(g: &mut Graph, name: &str) -> Result<u64> {
    let prefix = colour_prefix(name);
    let mut gone = 0u64;
    loop {
        let mut keys: Vec<Vec<u8>> = Vec::new();
        g.ext_scan(NS, &prefix, &mut |k, _| {
            keys.push(k.to_vec());
            keys.len() < WRITE_BATCH
        });
        if keys.is_empty() {
            return Ok(gone);
        }
        step(keys.len() as u64)?;
        gone += keys.len() as u64;
        g.ext_write(NS, Vec::new(), keys)?;
    }
}

fn keep(g: &mut Graph, name: &str, ca: &ColorArgs, col: &mut Colouring) -> Result<u64> {
    let (start, from) = begin(g);
    forget_colours(g, name)?;
    let prefix = colour_prefix(name);
    let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut kept = 0u64;
    for v in 0..col.len {
        step(1)?;
        if !col.selected.get(v) {
            continue;
        }
        let mut k = prefix.clone();
        k.extend_from_slice(&col.ids.get(v).to_be_bytes());
        puts.push((k, colour_bytes(col.colors.get(v)).to_vec()));
        kept += 1;
        if puts.len() == WRITE_BATCH {
            g.ext_write(NS, std::mem::take(&mut puts), Vec::new())?;
        }
    }
    g.ext_write(NS, puts, Vec::new())?;
    let meta = Meta {
        at: start,
        depth: col.depth,
        stable: col.stable,
        nodes: kept,
        colors: col.counts.last().map(|c| c.1).unwrap_or(0),
        code: CODE.into(),
        args: ca.json(),
    };
    seal(g, from, vec![(meta_key(name), meta.encode())], Vec::new())?;
    Ok(kept + 1)
}

fn find_meta(g: &Graph, name: &str) -> Result<Meta> {
    let v = g.ext_get(NS, &meta_key(name)).ok_or_else(|| {
        Error::Msg(format!(
            "no colouring is kept as '{name}'; CALL structure.stored() lists them, and CALL structure.colors(store: '{name}', ...) keeps one"
        ))
    })?;
    Meta::decode(&v).ok_or_else(|| {
        Error::Msg(format!(
            "the colouring kept as '{name}' does not decode; drop it and compute it again"
        ))
    })
}

/// The kept colouring's metadata, refusing it when stale and `fresh:
/// "require"` was asked for.
fn open_kept(g: &Graph, name: &str, fresh: Fresh) -> Result<(Meta, bool)> {
    let m = find_meta(g, name)?;
    let ok = is_fresh(g, m.at);
    if !ok && fresh == Fresh::Require {
        return Err(Error::Msg(format!(
            "the colouring kept as '{name}' was computed at revision {} and the database has changed since; compute it again with CALL structure.colors(store: '{name}', ...), or read it with fresh: \"report\"",
            m.at
        )));
    }
    Ok((m, ok))
}

fn read(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let name = store_name(a, "name")?.ok_or_else(|| {
        Error::Msg(
            "read needs name: the colouring to read; CALL structure.stored() lists them".into(),
        )
    })?;
    let (m, fresh) = open_kept(g, &name, fresh_arg(a)?)?;
    let limit = limit_arg(a)?;
    let after = after_id(a)?;
    let identity = a.str("identity");
    let prefix = colour_prefix(&name);
    let mut from = prefix.clone();
    from.extend_from_slice(&after.to_be_bytes());
    let mut got: Vec<(u64, Colour)> = Vec::new();
    let mut more = false;
    g.ext_scan_after(NS, &prefix, Some(&from), &mut |k, v| {
        if got.len() == limit {
            more = true;
            return false;
        }
        if k.len() == prefix.len() + 8 && v.len() == 16 {
            got.push((u64_at(k, prefix.len()), (u64_at(v, 0), u64_at(v, 8))));
        }
        true
    });
    step(got.len() as u64)?;
    let mut rows = Vec::with_capacity(got.len());
    for (id, c) in &got {
        let mut row = vec![Value::Int(*id as i64), Value::Text(colour_hex(*c))];
        if identity.is_some() {
            row.push(identity_value(g, identity.as_deref(), *id));
        }
        rows.push(row);
    }
    let mut columns: Vec<String> = vec!["id".into(), "color".into()];
    if identity.is_some() {
        columns.push("identity".into());
    }
    let mut msg = format!(
        "colouring '{name}': {} nodes, {} colours at depth {}, computed at revision {}; {}",
        m.nodes,
        m.colors,
        m.depth,
        m.at,
        if fresh {
            "fresh: nothing has changed since"
        } else {
            "stale: the database has changed since"
        }
    );
    if more {
        if let Some((last, _)) = got.last() {
            msg.push_str(&format!(
                "; more: CALL structure.read again with after: {last}"
            ));
        }
    }
    let mut r = QueryResult::table(columns, rows);
    r.message = Some(msg);
    Ok(r)
}

fn stored(g: &mut Graph) -> Result<QueryResult> {
    let mut metas: Vec<(String, Vec<u8>)> = Vec::new();
    g.ext_scan(NS, b"m", &mut |k, v| {
        metas.push((String::from_utf8_lossy(&k[1..]).into_owned(), v.to_vec()));
        true
    });
    let mut rows = Vec::new();
    for (name, v) in metas {
        let Some(m) = Meta::decode(&v) else {
            rows.push(vec![
                Value::Text(name),
                Value::Null,
                Value::Bool(false),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Text("unreadable".into()),
                Value::Null,
            ]);
            continue;
        };
        rows.push(vec![
            Value::Text(name),
            Value::Int(m.at as i64),
            Value::Bool(is_fresh(g, m.at)),
            Value::Int(m.depth as i64),
            m.stable
                .map(|s| Value::Int(s as i64))
                .unwrap_or(Value::Null),
            Value::Int(m.nodes as i64),
            Value::Int(m.colors as i64),
            Value::Text(m.code),
            Value::Text(m.args),
        ]);
    }
    let n = rows.len();
    let mut r = QueryResult::table(
        [
            "name", "revision", "fresh", "depth", "stable", "nodes", "colors", "code", "args",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        rows,
    );
    r.message = Some(format!("{n} kept colourings"));
    Ok(r)
}

fn drop_colouring(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let name = store_name(a, "name")?
        .ok_or_else(|| Error::Msg("drop needs name: the colouring to forget".into()))?;
    find_meta(g, &name)?;
    let (_, from) = begin(g);
    let gone = forget_colours(g, &name)?;
    seal(g, from, Vec::new(), vec![meta_key(&name)])?;
    let mut r = QueryResult::message(format!("dropped the colouring '{name}' ({gone} colours)"));
    r.touched = 1;
    Ok(r)
}

// ---------------------------------------------------------------- classes

/// How members of a class are ordered and the class's tie is broken:
/// by the identity property's value when given (content, so the same in
/// every database holding the same data), then by id.
fn member_key(g: &Graph, identity: Option<&str>, id: u64) -> Vec<u8> {
    let mut k = Vec::new();
    if let Some(key) = identity {
        canonical(&mut k, g.node_prop(id, key).as_ref());
    }
    k.extend_from_slice(&id.to_be_bytes());
    k
}

/// A node's labels (by name) and values on `keys`, as bytes.
fn own_part(g: &Graph, id: u64, keys: &[(String, Option<u32>)], out: &mut Vec<u8>) -> bool {
    let Some(node) = g.node_shared(id) else {
        return false;
    };
    let mut names: Vec<&str> = node.labels().iter().map(|&l| g.strings.name(l)).collect();
    names.sort_unstable();
    names.dedup();
    out.extend_from_slice(&(names.len() as u32).to_be_bytes());
    for l in names {
        put_str(out, l);
    }
    for (_, k) in keys {
        let v = k.and_then(|k| node.prop(k));
        canonical(out, v.as_ref());
    }
    true
}

struct Neighbours<'a> {
    types: &'a [Option<u128>],
    dir: Dir,
}

impl Neighbours<'_> {
    /// The multiset of (direction, type, neighbour) as a hash and a count.
    fn hash(&self, g: &Graph, id: u64) -> (u128, u64) {
        let mut sum = 0u128;
        let mut n = 0u64;
        let mut e = [0u8; 13];
        let mut tally = crate::meter::Tally::new();
        for &(tag, d) in sides(self.dir) {
            g.for_each_adj_while(id, d, None, |a| {
                if tally.spent(1) {
                    return false;
                }
                if !matches!(self.types.get(a.etype as usize), Some(Some(_))) {
                    return true;
                }
                e[0] = tag;
                e[1..5].copy_from_slice(&a.etype.to_be_bytes());
                e[5..].copy_from_slice(&a.other.to_be_bytes());
                sum = sum.wrapping_add(sip128(&e));
                n += 1;
                true
            });
        }
        tally.flush();
        (sum, n)
    }

    /// The same multiset, sorted, in full.
    fn list(&self, g: &Graph, id: u64) -> Vec<(u8, u32, u64)> {
        let mut v = Vec::new();
        let mut tally = crate::meter::Tally::new();
        for &(tag, d) in sides(self.dir) {
            g.for_each_adj_while(id, d, None, |a| {
                if tally.spent(1) {
                    return false;
                }
                if matches!(self.types.get(a.etype as usize), Some(Some(_))) {
                    v.push((tag, a.etype, a.other));
                }
                true
            });
        }
        tally.flush();
        crate::meter::sort(&mut v);
        v
    }
}

/// Whether two nodes are interchangeable by neighbours, compared in full:
/// the same labels and key values, and the same sorted multiset of typed
/// neighbours.
fn same_neighbours(
    g: &Graph,
    nb: &Neighbours<'_>,
    keys: &[(String, Option<u32>)],
    a: u64,
    a_list: &[(u8, u32, u64)],
    b: u64,
) -> bool {
    let (mut x, mut y) = (Vec::new(), Vec::new());
    own_part(g, a, keys, &mut x);
    own_part(g, b, keys, &mut y);
    x == y && nb.list(g, b) == a_list
}

/// The nodes of `labels` (every node when empty), each once, in id order.
fn each_node(g: &Graph, labels: &[String], f: &mut dyn FnMut(u64) -> Result<()>) -> Result<()> {
    let mut failed: Option<Error> = None;
    if labels.is_empty() {
        g.scan_nodes(None, 0, usize::MAX, &mut |id| match f(id) {
            Ok(()) => true,
            Err(e) => {
                failed = Some(e);
                false
            }
        });
    } else {
        // Each label's members; a node with an earlier label of the list
        // was visited with it.
        let ls: Vec<u32> = labels.iter().filter_map(|l| g.strings.lookup(l)).collect();
        for (i, &l) in ls.iter().enumerate() {
            let earlier = &ls[..i];
            g.scan_nodes(Some(l), 0, usize::MAX, &mut |id| {
                if !earlier.is_empty() && earlier.iter().any(|&e| g.has_label(id, e)) {
                    return true;
                }
                match f(id) {
                    Ok(()) => true,
                    Err(e) => {
                        failed = Some(e);
                        false
                    }
                }
            });
            if failed.is_some() {
                break;
            }
        }
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Classes go into this sort largest first, then by their tie, then by
/// member: (u64::MAX - size, tie length, tie, member key) -> id.
fn class_key(size: u64, tie: &[u8], member: &[u8]) -> Vec<u8> {
    let mut k = (u64::MAX - size).to_be_bytes().to_vec();
    k.extend_from_slice(&(tie.len() as u32).to_be_bytes());
    k.extend_from_slice(tie);
    k.extend_from_slice(member);
    k
}

fn class_prefix_len(k: &[u8]) -> usize {
    12 + u32::from_be_bytes(k[8..12].try_into().unwrap()) as usize
}

fn classes(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let by = a
        .str("by")
        .unwrap_or_else(|| "neighbours".into())
        .to_ascii_lowercase();
    let identity = a.str("identity");
    let min_size = a.usize("min_size", 2).max(1) as u64;
    let members_cap = a.usize("members", MEMBERS);
    let limit = limit_arg(a)?;
    let after = a.usize("after", 0) as u64;
    let summary = a.bool("summary", false);
    let mut out = sorter(g, "structure-classes")?;
    let mut nodes = 0u64;
    let mut note = String::new();
    match by.as_str() {
        "neighbours" | "neighbors" => {
            let mut labels = names(a, "labels");
            labels.extend(a.str("label"));
            labels.sort();
            labels.dedup();
            let keys: Vec<(String, Option<u32>)> = a
                .strs("keys")
                .into_iter()
                .map(|k| {
                    let id = g.strings.lookup(&k);
                    (k, id)
                })
                .collect();
            let types = followed_types(g, &names(a, "types"));
            let nb = Neighbours {
                types: &types,
                dir: dir_arg(a)?,
            };
            nodes = by_neighbours(
                g,
                &labels,
                &keys,
                &nb,
                identity.as_deref(),
                min_size,
                &mut out,
            )?;
        }
        "colors" | "colours" => {
            let mut s = sorter(g, "structure-by-colour")?;
            if let Some(name) = store_name(a, "colors")? {
                let (m, fresh) = open_kept(g, &name, fresh_arg(a)?)?;
                note = format!(
                    "; by the colouring kept as '{name}' (depth {}, revision {}, {})",
                    m.depth,
                    m.at,
                    if fresh { "fresh" } else { "stale" }
                );
                let prefix = colour_prefix(&name);
                let mut after: Option<Vec<u8>> = None;
                loop {
                    let mut batch: Vec<(u64, Vec<u8>)> = Vec::new();
                    let mut last: Option<Vec<u8>> = None;
                    g.ext_scan_after(NS, &prefix, after.as_deref(), &mut |k, v| {
                        if k.len() == prefix.len() + 8 && v.len() == 16 {
                            batch.push((u64_at(k, prefix.len()), v.to_vec()));
                        }
                        last = Some(k.to_vec());
                        batch.len() < WRITE_BATCH
                    });
                    if last.is_some() {
                        after = last;
                    }
                    if batch.is_empty() {
                        break;
                    }
                    step(batch.len() as u64)?;
                    for (id, c) in batch {
                        let mut k = c;
                        k.extend_from_slice(&member_key(g, identity.as_deref(), id));
                        s.push(&k, &id.to_be_bytes()).map_err(Error::Io)?;
                        nodes += 1;
                    }
                }
            } else {
                let ca = ColorArgs::from(a)?;
                let mut col = compute(g, &ca)?;
                note = format!("; by colours at depth {}", col.depth);
                for v in 0..col.len {
                    step(1)?;
                    if !col.selected.get(v) {
                        continue;
                    }
                    let id = col.ids.get(v);
                    let mut k = colour_bytes(col.colors.get(v)).to_vec();
                    k.extend_from_slice(&member_key(g, identity.as_deref(), id));
                    s.push(&k, &id.to_be_bytes()).map_err(Error::Io)?;
                    nodes += 1;
                }
            }
            by_colour(g, s, identity.as_deref(), min_size, &mut out)?;
        }
        other => {
            return Err(Error::Msg(format!(
                "by: '{other}' is not a way to form classes; use \"neighbours\" or \"colors\""
            )))
        }
    }
    emit(
        g,
        out,
        nodes,
        min_size,
        members_cap,
        identity.as_deref(),
        after,
        limit,
        summary,
        &note,
    )
}

/// Group nodes by neighbours: a sort on (labels and keys hash, neighbour
/// hash, count, member), each run of equal hashes then compared in full
/// and split where they differ, so the classes are exact.
fn by_neighbours(
    g: &Graph,
    labels: &[String],
    keys: &[(String, Option<u32>)],
    nb: &Neighbours<'_>,
    identity: Option<&str>,
    min_size: u64,
    out: &mut Sorter,
) -> Result<u64> {
    let mut s = sorter(g, "structure-by-neighbours")?;
    let mut nodes = 0u64;
    let mut own = Vec::new();
    each_node(g, labels, &mut |id| {
        own.clear();
        if !own_part(g, id, keys, &mut own) {
            return Ok(());
        }
        let (sum, n) = nb.hash(g, id);
        step(1)?;
        let mut k = sip128(&own).to_be_bytes().to_vec();
        k.extend_from_slice(&sum.to_be_bytes());
        k.extend_from_slice(&n.to_be_bytes());
        k.extend_from_slice(&member_key(g, identity, id));
        s.push(&k, &id.to_be_bytes()).map_err(Error::Io)?;
        nodes += 1;
        Ok(())
    })?;
    let mut sorted = s.finish().map_err(Error::Io)?;
    // A run of equal hashes: (member key, id), in member order.
    let mut run: SpillQueue<u64> = SpillQueue::new();
    let mut at: Option<Vec<u8>> = None;
    loop {
        let next = sorted.next().map_err(Error::Io)?;
        let head = next.as_ref().map(|(k, _)| k[..40].to_vec());
        if head != at {
            if run.len() >= min_size {
                split_run(g, nb, keys, identity, &mut run, min_size, out)?;
            }
            run = SpillQueue::new();
            at = head;
        }
        match next {
            Some((_, v)) => run.push_back(u64_at(&v, 0)),
            None => break,
        }
    }
    Ok(nodes)
}

/// Compare a run of nodes with equal hashes in full, and send each group of
/// truly interchangeable ones (of `min_size` or more) to `out`. Members are
/// compared with the first member of each group found so far; when every
/// member matches the first, as hashing makes all but certain, that is one
/// pass.
fn split_run(
    g: &Graph,
    nb: &Neighbours<'_>,
    keys: &[(String, Option<u32>)],
    identity: Option<&str>,
    run: &mut SpillQueue<u64>,
    min_size: u64,
    out: &mut Sorter,
) -> Result<()> {
    // Representatives: (id, its neighbour list, members so far).
    let mut reps: Vec<(u64, Vec<(u8, u32, u64)>, SpillQueue<u64>)> = Vec::new();
    while let Some(id) = run.pop_front() {
        step(1)?;
        let mut placed = false;
        for (rep, list, members) in reps.iter_mut() {
            if same_neighbours(g, nb, keys, *rep, list, id) {
                members.push_back(id);
                placed = true;
                break;
            }
        }
        if !placed {
            let list = nb.list(g, id);
            let mut q = SpillQueue::new();
            q.push_back(id);
            reps.push((id, list, q));
        }
    }
    for (rep, _, mut members) in reps {
        let size = members.len();
        if size < min_size {
            continue;
        }
        let tie = member_key(g, identity, rep);
        while let Some(id) = members.pop_front() {
            out.push(
                &class_key(size, &tie, &member_key(g, identity, id)),
                &id.to_be_bytes(),
            )
            .map_err(Error::Io)?;
        }
    }
    Ok(())
}

/// Group a sort of (colour, member) by colour; a class's tie is its colour.
fn by_colour(
    g: &Graph,
    s: Sorter,
    identity: Option<&str>,
    min_size: u64,
    out: &mut Sorter,
) -> Result<()> {
    let mut sorted = s.finish().map_err(Error::Io)?;
    let mut run: SpillQueue<u64> = SpillQueue::new();
    let mut at: Option<Vec<u8>> = None;
    loop {
        let next = sorted.next().map_err(Error::Io)?;
        let head = next.as_ref().map(|(k, _)| k[..16].to_vec());
        if head != at {
            let size = run.len();
            if size >= min_size {
                let tie = at.clone().unwrap_or_default();
                while let Some(id) = run.pop_front() {
                    out.push(
                        &class_key(size, &tie, &member_key(g, identity, id)),
                        &id.to_be_bytes(),
                    )
                    .map_err(Error::Io)?;
                }
            }
            run = SpillQueue::new();
            at = head;
        }
        match next {
            Some((_, v)) => {
                step(1)?;
                run.push_back(u64_at(&v, 0));
            }
            None => break,
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit(
    g: &Graph,
    out: Sorter,
    nodes: u64,
    min_size: u64,
    members_cap: usize,
    identity: Option<&str>,
    after: u64,
    limit: usize,
    summary: bool,
    note: &str,
) -> Result<QueryResult> {
    let mut sorted = out.finish().map_err(Error::Io)?;
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut sizes: Vec<(u64, u64)> = Vec::new(); // (size, classes), largest first
    let mut rank = 0u64;
    let mut in_classes = 0u64;
    let mut at: Option<Vec<u8>> = None;
    let mut cur: Option<(u64, Vec<Value>, Vec<Value>)> = None; // (size, members, identities)
    let mut more = false;
    let flush =
        |cur: &mut Option<(u64, Vec<Value>, Vec<Value>)>, rank: u64, rows: &mut Vec<Vec<Value>>| {
            if let Some((size, members, idents)) = cur.take() {
                let first = members.first().cloned().unwrap_or(Value::Null);
                let mut row = vec![
                    Value::Int(rank as i64),
                    Value::Int(size as i64),
                    first,
                    Value::List(members),
                ];
                if identity.is_some() {
                    row.push(Value::List(idents));
                }
                rows.push(row);
            }
        };
    loop {
        let next = sorted.next().map_err(Error::Io)?;
        let head = next
            .as_ref()
            .map(|(k, _)| k[..class_prefix_len(k)].to_vec());
        if head != at {
            if cur.is_some() {
                flush(&mut cur, rank, &mut rows);
            }
            at = head.clone();
            if let Some(h) = &head {
                rank += 1;
                let size = u64::MAX - u64_at(h, 0);
                in_classes += size;
                match sizes.last_mut() {
                    Some((s, n)) if *s == size => *n += 1,
                    _ => sizes.push((size, 1)),
                }
                if !summary && rank > after {
                    if rows.len() == limit {
                        more = true;
                    } else if !more {
                        cur = Some((size, Vec::new(), Vec::new()));
                    }
                }
            }
        }
        let Some((_, v)) = next else { break };
        step(1)?;
        if let Some((_, members, idents)) = cur.as_mut() {
            if members.len() < members_cap {
                let id = u64_at(&v, 0);
                members.push(Value::Int(id as i64));
                if identity.is_some() {
                    idents.push(identity_value(g, identity, id));
                }
            }
        }
    }
    let classes = rank;
    let columns: Vec<String> = if summary {
        rows = sizes
            .iter()
            .map(|(s, n)| vec![Value::Int(*s as i64), Value::Int(*n as i64)])
            .collect();
        vec!["size".into(), "classes".into()]
    } else {
        let mut c: Vec<String> = ["class", "size", "first", "members"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if identity.is_some() {
            c.push("identities".into());
        }
        c
    };
    let mut msg = format!(
        "{classes} classes of {min_size} or more interchangeable nodes hold {in_classes} of the {nodes} nodes looked at{note}; members are distinct nodes that the structure cannot tell apart, not one thing, and nothing was merged"
    );
    if more {
        let last = rows.last().and_then(|r| r[0].as_i64()).unwrap_or(0);
        msg.push_str(&format!(
            "; more: CALL structure.classes again with after: {last} and the same arguments"
        ));
    }
    let mut r = QueryResult::table(columns, rows);
    r.message = Some(msg);
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siphash_128_matches_the_reference_vectors() {
        // vectors_sip128 of the SipHash reference implementation (key
        // 00..0f, message 00..(n-1)); the longer ones were checked against
        // a separate implementation whose 64-bit output matches the
        // reference's 64-bit vectors.
        let key: Vec<u8> = (0u8..16).collect();
        let k0 = u64::from_le_bytes(key[..8].try_into().unwrap());
        let k1 = u64::from_le_bytes(key[8..].try_into().unwrap());
        let out = |n: u8| {
            let msg: Vec<u8> = (0..n).collect();
            let h = sip128_keyed(k0, k1, &msg);
            let mut b = (h as u64).to_le_bytes().to_vec();
            b.extend_from_slice(&((h >> 64) as u64).to_le_bytes());
            hex(&b)
        };
        assert_eq!(out(0), "a3817f04ba25a8e66df67214c7550293");
        assert_eq!(out(1), "da87c1d86b99af44347659119b22fc45");
        assert_eq!(out(8), "3b62a9ba6258f5610f83e264f31497b4");
        assert_eq!(out(15), "5493e99933b0a8117e08ec0f97cfc3d9");
        assert_eq!(out(16), "6ee2a4ca67b054bbfd3315bf85230577");
        assert_eq!(out(63), "5150d1772f50834a503e069a973fbd7c");
    }

    #[test]
    fn canonical_values_equal_exactly_when_the_engine_says_so() {
        let enc = |v: &Value| {
            let mut b = Vec::new();
            canonical(&mut b, Some(v));
            b
        };
        assert_eq!(enc(&Value::Int(1)), enc(&Value::Float(1.0)));
        assert_ne!(enc(&Value::Int(1)), enc(&Value::Float(1.5)));
        assert_ne!(enc(&Value::Text("1".into())), enc(&Value::Int(1)));
        let mut absent = Vec::new();
        canonical(&mut absent, None);
        assert_ne!(absent, enc(&Value::Null));
        let nested = Value::List(vec![
            Value::Int(1),
            Value::List(vec![Value::Text("a".into())]),
            Value::Int(2),
        ]);
        let flat = Value::List(vec![Value::Int(1), Value::Text("a".into()), Value::Int(2)]);
        assert_ne!(enc(&nested), enc(&flat));
        assert!(enc(&Value::Int(-5)) < enc(&Value::Int(3)));
    }

    #[test]
    fn per_node_state_spills_past_its_budget_with_the_same_colours() {
        let mut g = Graph::memory();
        let mut prev = None;
        for i in 0..5_000u64 {
            let id = g
                .add_node(
                    &["N".to_string()],
                    vec![("i".into(), Value::Int((i % 7) as i64))],
                )
                .unwrap();
            if let Some(p) = prev {
                g.add_edge(p, id, "NEXT", vec![]).unwrap();
            }
            prev = Some(id);
        }
        let ca = ColorArgs {
            depth: Depth::Fixed(3),
            types: Vec::new(),
            dir: Dir::Both,
            props: vec!["i".into()],
            labels: Vec::new(),
        };
        let mut free = compute(&g, &ca).unwrap();
        assert!(!free.ids.is_spilled() && !free.colors.is_spilled());
        // 16 KiB for 5,000 nodes: the id and colour arrays spill.
        let mut tight = ooc::with_budget(16 << 10, g.temp_dir(), || {
            let c = compute(&g, &ca).unwrap();
            assert!(c.ids.is_spilled() && c.colors.is_spilled());
            c
        });
        assert_eq!(free.counts, tight.counts);
        for v in 0..free.len {
            assert_eq!(free.colors.get(v), tight.colors.get(v));
        }
    }

    #[test]
    fn a_kept_colouring_record_reads_back_as_written() {
        let m = Meta {
            at: 12345,
            depth: 4,
            stable: Some(3),
            nodes: 10,
            colors: 4,
            code: CODE.into(),
            args: "{}".into(),
        };
        assert_eq!(Meta::decode(&m.encode()), Some(m.clone()));
        let none = Meta { stable: None, ..m };
        assert_eq!(Meta::decode(&none.encode()), Some(none));
    }
}
