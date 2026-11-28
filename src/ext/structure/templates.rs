//! Templates: shapes that repeat along a hierarchy, each said once.
//!
//! A *hierarchy* is the edges of `types:` followed in `dir:`. A node is
//! *owned* by the node one such edge comes from when exactly one does (and
//! it is not itself); owners that go round in a circle are cut, so the
//! owned nodes make a forest. The *shape* rooted at a node is the node and
//! everything it owns, down the forest, with every edge going out of those
//! nodes: an edge to a node of the shape is *internal* (written by
//! position), any other is a *reference* (written by the node it reaches,
//! which is shared, not copied). Under `dir: "in"` the root's own edge to
//! its owner belongs to the owner's shape and is written beside a use.
//!
//! Nodes of a shape are numbered in preorder, children in an order fixed
//! by content (edge type, then a hash of the child's shape, then its own
//! values), so the same data under other ids is numbered the same way and
//! siblings line up alike in every instance.
//! Two roots are *instances of one template* when their shapes are the
//! same: the same labels, property keys, edges, positions and referenced
//! nodes. A property whose value is the same in every instance is a
//! constant of the template; a text value that is, in every instance, the
//! root's value of the same key (or a fixed prefix of it) followed by the
//! same text is derived from the root; anything else is a *slot*, which
//! each instance fills. A template never says that two things are one: an
//! instance is its own nodes, all of them, and [`rebuild`] gives them back
//! exactly from the definition and the instance's slot values (D1).
//!
//! Grouping is by a 128-bit hash of the shape, and every member is then
//! compared with the first in full, so no instance rests on the odds of a
//! collision.

use std::collections::HashMap;

use super::notation::{self, node_text, Cost, Names, NodeText};
use super::{followed_types, index_of, limit_arg, names as sorted_names, sip128, step, Colour};
use crate::ext::Args;
use crate::graph::{Dir, Error, Graph, Result};
use crate::ooc::{SpillQueue, StateVec};
use crate::query::QueryResult;
use crate::storage::extsort::Sorter;
use crate::value::Value;

/// No node: the owner of a root, the parent of position 0.
pub const NONE: u64 = u64::MAX;
const NO_POS: u32 = u32::MAX;

/// Nodes a template may have when `max_nodes:` is not given.
pub const MAX_NODES: usize = 4_096;
/// Templates one call may hold; past it the call is refused.
pub const MAX_TEMPLATES: usize = 100_000;

/// What the hierarchy is and which shapes count.
#[derive(Clone, Debug)]
pub struct Hier {
    pub types: Vec<String>,
    follow: Vec<Option<u128>>,
    /// The followed types' ids when `types:` names them (empty: every
    /// type), so a node's followed edges are read by type, not by scanning
    /// all of them.
    type_ids: Vec<u32>,
    pub dir: Dir,
    /// Roots are nodes of these labels; empty: every node.
    pub labels: Vec<String>,
    pub min_nodes: usize,
    pub min_count: usize,
    pub max_nodes: usize,
    pub names: Names,
}

impl Hier {
    pub fn from(g: &Graph, a: &Args) -> Result<Hier> {
        let dir = match a.str("dir").map(|d| d.to_ascii_lowercase()).as_deref() {
            None | Some("out") => Dir::Out,
            Some("in") => Dir::In,
            Some(d) => {
                return Err(Error::Msg(format!(
                    "dir: '{d}' is not a direction a hierarchy can be followed in; use \"out\" (from parent to child) or \"in\""
                )))
            }
        };
        let mut labels = sorted_names(a, "labels");
        labels.extend(a.str("label"));
        labels.sort();
        labels.dedup();
        let count = |key: &str, default: usize, min: usize| -> Result<usize> {
            match a.get(key) {
                None | Some(Value::Null) => Ok(default),
                Some(v) => match v.as_i64() {
                    Some(n) if n >= min as i64 && n <= 1_000_000 => Ok(n as usize),
                    _ => Err(Error::Msg(format!(
                        "{key}: {v} is not a count; give {min} or more"
                    ))),
                },
            }
        };
        let types = sorted_names(a, "types");
        Ok(Hier {
            type_ids: type_ids(g, &types),
            follow: followed_types(g, &types),
            types,
            dir,
            labels,
            min_nodes: count("min_nodes", 2, 1)?,
            min_count: count("min_count", 2, 2)?,
            max_nodes: count("max_nodes", MAX_NODES, 1)?,
            names: Names {
                identity: a.strs("identity"),
            },
        })
    }

    /// A hierarchy as a kept dictionary records it; [`Hier::rebind`] makes
    /// it usable on a graph.
    #[allow(clippy::too_many_arguments)]
    pub fn parts(
        types: Vec<String>,
        dir: Dir,
        labels: Vec<String>,
        min_nodes: usize,
        min_count: usize,
        max_nodes: usize,
        identity: Vec<String>,
    ) -> Hier {
        Hier {
            types,
            follow: Vec::new(),
            type_ids: Vec::new(),
            dir,
            labels,
            min_nodes,
            min_count,
            max_nodes,
            names: Names { identity },
        }
    }

    /// The same hierarchy with its edge types looked up in `g`.
    pub fn rebind(&self, g: &Graph) -> Hier {
        let mut h = self.clone();
        h.follow = followed_types(g, &h.types);
        h.type_ids = type_ids(g, &h.types);
        h
    }

    /// The arguments as recorded with a kept dictionary.
    pub fn json(&self) -> String {
        format!(
            "{{\"types\":{},\"dir\":\"{}\",\"labels\":{},\"min_nodes\":{},\"min_count\":{},\"max_nodes\":{},\"identity\":{}}}",
            super::json_list(&self.types),
            super::dir_name(self.dir),
            super::json_list(&self.labels),
            self.min_nodes,
            self.min_count,
            self.max_nodes,
            super::json_list(&self.names.identity)
        )
    }

    pub fn followed(&self, t: u32) -> bool {
        matches!(self.follow.get(t as usize), Some(Some(_)))
    }

    fn type_hash(&self, t: u32) -> u128 {
        self.follow
            .get(t as usize)
            .copied()
            .flatten()
            .unwrap_or_default()
    }

    /// Visit `v`'s followed edges in `dir`, stopping when `f` says so:
    /// by type when `types:` names them, so the cost is the followed edges,
    /// not every edge of the node.
    pub fn each_followed(
        &self,
        g: &Graph,
        v: u64,
        dir: Dir,
        f: &mut dyn FnMut(crate::types::Adj) -> bool,
    ) {
        if self.types.is_empty() {
            g.for_each_adj_while(v, dir, None, &mut *f);
            return;
        }
        for &t in &self.type_ids {
            if !g.for_each_adj_while(v, dir, Some(t), &mut *f) {
                return;
            }
        }
    }

    /// The direction edges reach a child from: against the one followed.
    fn back(&self) -> Dir {
        match self.dir {
            Dir::Out => Dir::In,
            _ => Dir::Out,
        }
    }
}

/// The interned ids of the types named (those the graph has).
fn type_ids(g: &Graph, types: &[String]) -> Vec<u32> {
    let mut v: Vec<u32> = types.iter().filter_map(|t| g.strings.lookup(t)).collect();
    v.sort_unstable();
    v
}

// ------------------------------------------------------------ the forest

/// Who owns whom, how big each shape is, and hashes of each shape and of
/// its values, for ordering children by content.
pub struct Forest {
    pub ids: StateVec<u64>,
    pub len: usize,
    owner: StateVec<u64>,
    pub size: StateVec<u64>,
    shape: StateVec<Colour>,
    content: StateVec<Colour>,
}

/// A value as bytes equal exactly when the values are the same value of
/// the same type (`1` and `1.0` differ: a template gives back what was
/// stored). Lists are walked with a stack.
pub fn exact(out: &mut Vec<u8>, v: &Value) {
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
                        out.push(b']');
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
            Value::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(&i.to_be_bytes());
            }
            Value::Float(f) => {
                out.push(b'f');
                let bits = if f.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    f.to_bits()
                };
                out.extend_from_slice(&bits.to_be_bytes());
            }
            Value::Text(s) => {
                out.push(b't');
                super::put_str(out, s);
            }
            Value::List(items) => {
                out.push(b'l');
                out.extend_from_slice(&(items.len() as u32).to_be_bytes());
                stack.push(items.iter());
            }
        }
    }
}

pub fn same(a: &Value, b: &Value) -> bool {
    let (mut x, mut y) = (Vec::new(), Vec::new());
    exact(&mut x, a);
    exact(&mut y, b);
    x == y
}

fn put_hash(out: &mut Vec<u8>, h: u128) {
    out.extend_from_slice(&h.to_be_bytes());
}

fn split(h: u128) -> Colour {
    super::split(h)
}

fn join(c: Colour) -> u128 {
    ((c.0 as u128) << 64) | c.1 as u128
}

/// A child before it is hashed: (node, edge, edge type).
type Kid0 = (u64, u64, u32);

/// A child as its parent's hashes see it: (edge, edge type, the child's
/// shape hash, its content hash, its size).
type Kid = (u64, u32, Colour, Colour, u64);

/// A node's shape hash (labels, keys, and its children's types, edge keys
/// and shapes, as a multiset), content hash (the same with every value)
/// and size, from its children's.
fn hashes(g: &Graph, h: &Hier, id: u64, kids: &[Kid]) -> (Colour, Colour, u64) {
    let mut own_s = vec![b'S'];
    let mut own_c = vec![b'C'];
    if let Some(n) = node_text(g, id) {
        for b in [&mut own_s, &mut own_c] {
            b.extend_from_slice(&(n.labels.len() as u32).to_be_bytes());
            for l in &n.labels {
                super::put_str(b, l);
            }
        }
        for (k, val) in &n.props {
            super::put_str(&mut own_s, k);
            super::put_str(&mut own_c, k);
            exact(&mut own_c, val);
        }
    }
    let (mut sum_s, mut sum_c, mut size) = (0u128, 0u128, 1u64);
    for &(edge, t, shape, content, csize) in kids {
        let mut es = vec![b'E'];
        put_hash(&mut es, h.type_hash(t));
        let mut ec = es.clone();
        let mut props = g.edge_props(edge);
        notation::sort_props(&mut props);
        for (k, val) in &props {
            super::put_str(&mut es, k);
            super::put_str(&mut ec, k);
            exact(&mut ec, val);
        }
        put_hash(&mut es, join(shape));
        put_hash(&mut ec, join(content));
        sum_s = sum_s.wrapping_add(sip128(&es));
        sum_c = sum_c.wrapping_add(sip128(&ec));
        size = size.saturating_add(csize);
    }
    let count = kids.len() as u64;
    for (b, sum) in [(&mut own_s, sum_s), (&mut own_c, sum_c)] {
        b.extend_from_slice(&count.to_be_bytes());
        put_hash(b, sum);
    }
    (split(sip128(&own_s)), split(sip128(&own_c)), size)
}

/// The one node a followed edge reaches `v` from, when exactly one does.
fn sole_owner(g: &Graph, h: &Hier, v: u64) -> u64 {
    let (mut count, mut other) = (0u32, NONE);
    h.each_followed(g, v, h.back(), &mut |a| {
        if h.followed(a.etype) {
            count += 1;
            other = a.other;
        }
        count < 2
    });
    if count == 1 && other != v {
        other
    } else {
        NONE
    }
}

impl Forest {
    pub fn build(g: &Graph, h: &Hier) -> Result<Forest> {
        let n = g.node_count();
        let mut ids = StateVec::new(n, 0u64);
        let mut len = 0usize;
        g.scan_nodes(None, 0, usize::MAX, &mut |id| {
            if len == n {
                return false;
            }
            ids.set(len, id);
            len += 1;
            true
        });
        step(len as u64)?;
        let mut owner = StateVec::new(len, NONE);
        for v in 0..len {
            let id = ids.get(v);
            let (mut count, mut other) = (0u32, NONE);
            g.for_each_adj(id, h.back(), None, |a| {
                if h.followed(a.etype) {
                    count += 1;
                    other = a.other;
                }
            });
            step(1 + count as u64)?;
            if count == 1 && other != id && index_of(&mut ids, len, other).is_some() {
                owner.set(v, other);
            }
        }
        // Cut owners that go round in a circle: each node has one owner at
        // most, so following owners from any node either stops or loops.
        let mut stamp = StateVec::new(len, 0u64);
        for i in 0..len {
            if stamp.get(i) != 0 {
                continue;
            }
            let mark = i as u64 + 1;
            let mut x = i;
            let cycle = loop {
                step(1)?;
                let s = stamp.get(x);
                if s == mark {
                    break Some(x);
                }
                if s != 0 {
                    break None;
                }
                stamp.set(x, mark);
                let o = owner.get(x);
                if o == NONE {
                    break None;
                }
                x = index_of(&mut ids, len, o).unwrap();
            };
            if let Some(c) = cycle {
                let mut y = c;
                loop {
                    let o = owner.get(y);
                    owner.set(y, NONE);
                    y = index_of(&mut ids, len, o).unwrap();
                    if y == c {
                        break;
                    }
                }
            }
        }
        drop(stamp);
        // Bottom-up: a node is ready when every node it owns is done.
        let mut pending = StateVec::new(len, 0u64);
        for v in 0..len {
            let o = owner.get(v);
            if o != NONE {
                let p = index_of(&mut ids, len, o).unwrap();
                let x = pending.get(p);
                pending.set(p, x + 1);
            }
        }
        let mut ready: SpillQueue<u64> = SpillQueue::new();
        for v in 0..len {
            if pending.get(v) == 0 {
                ready.push_back(v as u64);
            }
        }
        let mut f = Forest {
            ids,
            len,
            owner,
            size: StateVec::new(len, 0u64),
            shape: StateVec::new(len, (0u64, 0u64)),
            content: StateVec::new(len, (0u64, 0u64)),
        };
        while let Some(v) = ready.pop_front() {
            let v = v as usize;
            let id = f.ids.get(v);
            let mut kids: Vec<(u64, u64, u32)> = Vec::new();
            g.for_each_adj(id, h.dir, None, |a| {
                if h.followed(a.etype) && a.other != id {
                    kids.push((a.other, a.edge, a.etype));
                }
            });
            step(1 + kids.len() as u64)?;
            let mut done: Vec<Kid> = Vec::with_capacity(kids.len());
            for (c, edge, t) in kids {
                let Some(p) = f.pos(c) else { continue };
                if f.owner.get(p) != id {
                    continue;
                }
                done.push((edge, t, f.shape.get(p), f.content.get(p), f.size.get(p)));
            }
            let (sh, co, size) = hashes(g, h, id, &done);
            f.shape.set(v, sh);
            f.content.set(v, co);
            f.size.set(v, size);
            let o = f.owner.get(v);
            if o != NONE {
                let p = f.pos(o).unwrap();
                let left = pending.get(p) - 1;
                pending.set(p, left);
                if left == 0 {
                    ready.push_back(p as u64);
                }
            }
        }
        Ok(f)
    }

    /// The forest under one root alone, the same there as [`Forest::build`]
    /// makes it for the whole graph: what `expand` needs, without reading
    /// the rest of the graph. Ownership is decided by each node's own edges;
    /// an owner that goes round in a circle is cut, as the whole forest cuts
    /// it, when the root is on that circle (no node below a root that is not
    /// on a circle can be).
    pub fn local(g: &Graph, h: &Hier, root: u64, cap: usize) -> Result<Forest> {
        // Is the root on a circle of owners?
        let mut circle: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut x = root;
        let root_owner = sole_owner(g, h, root);
        loop {
            step(1)?;
            let o = sole_owner(g, h, x);
            if o == NONE || !seen.insert(x) {
                break;
            }
            if o == root {
                // Back at the root: everything on the way is the circle.
                let mut y = root;
                loop {
                    circle.insert(y);
                    y = sole_owner(g, h, y);
                    if y == root {
                        break;
                    }
                }
                break;
            }
            x = o;
        }
        // The nodes under it, with parents.
        let mut order: Vec<(u64, u64, u64, u32)> = Vec::new(); // (node, parent, edge, type)
        let mut stack: Vec<(u64, u64, u64, u32)> = vec![(root, NONE, 0, 0)];
        while let Some((v, par, e, t)) = stack.pop() {
            order.push((v, par, e, t));
            if order.len() > cap {
                return Err(Error::Msg(format!(
                    "the shape under node {root} has more than {cap} nodes; give a smaller max_nodes: or other types:"
                )));
            }
            let mut kids = Vec::new();
            g.for_each_adj(v, h.dir, None, |a| {
                if h.followed(a.etype) && a.other != v && a.other != root {
                    kids.push((a.other, a.edge, a.etype));
                }
            });
            step(1 + kids.len() as u64)?;
            for (c, e, t) in kids {
                if !circle.contains(&c) && sole_owner(g, h, c) == v {
                    stack.push((c, v, e, t));
                }
            }
        }
        // Bottom-up, children before parents: reverse preorder.
        let mut hashed: HashMap<u64, (Colour, Colour, u64)> = HashMap::with_capacity(order.len());
        let mut kids_of: HashMap<u64, Vec<Kid>> = HashMap::new();
        for &(v, par, e, t) in order.iter().rev() {
            let kids = kids_of.remove(&v).unwrap_or_default();
            let hv = hashes(g, h, v, &kids);
            hashed.insert(v, hv);
            if par != NONE {
                kids_of
                    .entry(par)
                    .or_default()
                    .push((e, t, hv.0, hv.1, hv.2));
            }
        }
        let mut rows: Vec<(u64, u64)> = order.iter().map(|&(v, par, _, _)| (v, par)).collect();
        rows.sort_unstable();
        let len = rows.len();
        let owner: Vec<u64> = rows
            .iter()
            .map(|&(v, par)| {
                if v == root {
                    if circle.is_empty() {
                        root_owner
                    } else {
                        NONE
                    }
                } else {
                    par
                }
            })
            .collect();
        Ok(Forest {
            ids: StateVec::from_vec(rows.iter().map(|r| r.0).collect()),
            len,
            owner: StateVec::from_vec(owner),
            size: StateVec::from_vec(rows.iter().map(|r| hashed[&r.0].2).collect()),
            shape: StateVec::from_vec(rows.iter().map(|r| hashed[&r.0].0).collect()),
            content: StateVec::from_vec(rows.iter().map(|r| hashed[&r.0].1).collect()),
        })
    }

    /// The forest over a selection of nodes alone: for each selected node
    /// whose whole shape is selected, its owner, size and hashes, the same
    /// as [`Forest::build`] makes them for the whole graph; a node with an
    /// owned node outside the selection gets size `u64::MAX`, so it is never
    /// a root here. Reads the selection, its edges and the owners above it,
    /// never the rest of the graph.
    pub fn part(g: &Graph, h: &Hier, part: &[u64]) -> Result<Forest> {
        let mut ids: Vec<u64> = part.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let inside: std::collections::HashSet<u64> = ids.iter().copied().collect();
        // Owners, and which nodes are on a circle of owners, following each
        // node's owners upwards once.
        let mut owner: HashMap<u64, u64> = HashMap::new();
        let mut circle: HashMap<u64, bool> = HashMap::new();
        for &v in &ids {
            let mut path: Vec<u64> = Vec::new();
            let mut at: HashMap<u64, usize> = HashMap::new();
            let mut x = v;
            loop {
                step(1)?;
                if circle.contains_key(&x) {
                    break;
                }
                if let Some(&i) = at.get(&x) {
                    for &y in &path[i..] {
                        circle.insert(y, true);
                    }
                    break;
                }
                at.insert(x, path.len());
                path.push(x);
                let o = *owner.entry(x).or_insert_with(|| sole_owner(g, h, x));
                if o == NONE {
                    break;
                }
                x = o;
            }
            for y in path {
                circle.entry(y).or_insert(false);
            }
        }
        let eff = |x: u64, owner: &HashMap<u64, u64>, circle: &HashMap<u64, bool>| -> u64 {
            if circle.get(&x).copied().unwrap_or(false) {
                NONE
            } else {
                owner.get(&x).copied().unwrap_or(NONE)
            }
        };
        // Each selected node's owned children; a child outside the
        // selection leaves its owner (and everything above it) incomplete.
        let mut kids: HashMap<u64, Vec<Kid0>> = HashMap::new();
        let mut incomplete: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for &v in &ids {
            let mut found: Vec<(u64, u64, u32)> = Vec::new();
            h.each_followed(g, v, h.dir, &mut |a| {
                if h.followed(a.etype) && a.other != v {
                    found.push((a.other, a.edge, a.etype));
                }
                true
            });
            step(1 + found.len() as u64)?;
            for (c, e, t) in found {
                let owned = if inside.contains(&c) {
                    eff(c, &owner, &circle) == v
                } else {
                    !circle.get(&c).copied().unwrap_or(false) && sole_owner(g, h, c) == v
                };
                if !owned {
                    continue;
                }
                if inside.contains(&c) {
                    kids.entry(v).or_default().push((c, e, t));
                } else {
                    incomplete.insert(v);
                }
            }
        }
        // Incomplete reaches up through owners in the selection.
        let mut up: Vec<u64> = incomplete.iter().copied().collect();
        while let Some(x) = up.pop() {
            let o = eff(x, &owner, &circle);
            if o != NONE && inside.contains(&o) && incomplete.insert(o) {
                up.push(o);
            }
        }
        // Bottom-up over the complete ones.
        let mut pending: HashMap<u64, usize> = HashMap::new();
        let mut ready: Vec<u64> = Vec::new();
        for &v in &ids {
            if incomplete.contains(&v) {
                continue;
            }
            let n = kids.get(&v).map_or(0, |k| k.len());
            if n == 0 {
                ready.push(v);
            } else {
                pending.insert(v, n);
            }
        }
        let mut hashed: HashMap<u64, (Colour, Colour, u64)> = HashMap::new();
        while let Some(v) = ready.pop() {
            let mut ks: Vec<Kid> = Vec::new();
            for &(c, e, t) in kids.get(&v).map(|k| k.as_slice()).unwrap_or(&[]) {
                let hc = hashed[&c];
                ks.push((e, t, hc.0, hc.1, hc.2));
            }
            hashed.insert(v, hashes(g, h, v, &ks));
            let o = eff(v, &owner, &circle);
            if o != NONE {
                if let Some(n) = pending.get_mut(&o) {
                    *n -= 1;
                    if *n == 0 {
                        pending.remove(&o);
                        ready.push(o);
                    }
                }
            }
        }
        let len = ids.len();
        let row = |v: &u64| hashed.get(v).copied().unwrap_or(((0, 0), (0, 0), u64::MAX));
        Ok(Forest {
            owner: StateVec::from_vec(ids.iter().map(|&v| eff(v, &owner, &circle)).collect()),
            size: StateVec::from_vec(ids.iter().map(|v| row(v).2).collect()),
            shape: StateVec::from_vec(ids.iter().map(|v| row(v).0).collect()),
            content: StateVec::from_vec(ids.iter().map(|v| row(v).1).collect()),
            ids: StateVec::from_vec(ids),
            len,
        })
    }

    pub fn shape_of(&mut self, id: u64) -> Option<u128> {
        let p = self.pos(id)?;
        Some(join(self.shape.get(p)))
    }

    pub fn pos(&mut self, id: u64) -> Option<usize> {
        index_of(&mut self.ids, self.len, id)
    }

    pub fn owner_of(&mut self, id: u64) -> u64 {
        match self.pos(id) {
            Some(p) => self.owner.get(p),
            None => NONE,
        }
    }
}

// ------------------------------------------------------------- instances

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum To {
    /// A node of the shape, by position.
    Pos(u32),
    /// A node outside it.
    Node(u64),
}

/// An edge going out of a node of a shape.
#[derive(Clone, Debug)]
pub struct IEdge {
    pub src: u32,
    pub etype: String,
    pub to: To,
    pub props: Vec<(String, Value)>,
    /// The hierarchy edge that makes its target a node of the shape.
    pub tree: bool,
}

/// A shape as found under one root: its nodes in preorder and its edges.
pub struct Inst {
    pub ids: Vec<u64>,
    pub parent: Vec<u32>,
    pub nodes: Vec<NodeText>,
    pub edges: Vec<IEdge>,
    /// Edges from the shape's nodes that are not part of it: under
    /// `dir: "in"`, the root's own edge to its owner.
    pub loose: Vec<u64>,
}

fn edge_key(e: &IEdge) -> Vec<u8> {
    let mut k = Vec::new();
    k.extend_from_slice(&e.src.to_be_bytes());
    super::put_str(&mut k, &e.etype);
    match e.to {
        To::Pos(p) => {
            k.push(0);
            k.extend_from_slice(&(p as u64).to_be_bytes());
        }
        To::Node(n) => {
            k.push(1);
            k.extend_from_slice(&n.to_be_bytes());
        }
    }
    for (key, v) in &e.props {
        super::put_str(&mut k, key);
        exact(&mut k, v);
    }
    k
}

/// The nodes of the shape under `root` in preorder, with each one's
/// parent position: children ordered by edge type, then by a hash of their
/// shape, then by their own values, then by a hash of everything under
/// them, then by id (only fully alike siblings get that far).
pub fn order(g: &Graph, f: &mut Forest, h: &Hier, root: u64) -> Result<(Vec<u64>, Vec<u32>)> {
    let (ids, parent, _) = ordered(g, f, h, root)?;
    Ok((ids, parent))
}

/// [`order`], with each node as read for it.
fn ordered(
    g: &Graph,
    f: &mut Forest,
    h: &Hier,
    root: u64,
) -> Result<(Vec<u64>, Vec<u32>, Vec<NodeText>)> {
    let gone = |v: u64| Error::Msg(format!("node {v} is gone; nothing can be written for it"));
    let mut ids: Vec<u64> = Vec::new();
    let mut parent: Vec<u32> = Vec::new();
    let mut texts: Vec<NodeText> = Vec::new();
    let first = node_text(g, root).ok_or_else(|| gone(root))?;
    let mut stack: Vec<(u64, u32, NodeText)> = vec![(root, NO_POS, first)];
    while let Some((v, par, text)) = stack.pop() {
        let p = ids.len() as u32;
        ids.push(v);
        parent.push(par);
        texts.push(text);
        if ids.len() > h.max_nodes.max(1) * 4 + 1024 {
            return Err(Error::Msg(format!(
                "the shape under node {root} has more than {} nodes; give a smaller max_nodes: or other types:",
                h.max_nodes
            )));
        }
        let mut found: Vec<(u64, u32)> = Vec::new();
        g.for_each_adj(v, h.dir, None, |a| {
            if h.followed(a.etype) && a.other != v {
                found.push((a.other, a.etype));
            }
        });
        step(1 + found.len() as u64)?;
        let mut kids: Vec<(&str, u128, Vec<u8>, u128, u64, NodeText)> = Vec::new();
        for (c, t) in found {
            let Some(cp) = f.pos(c) else { continue };
            if f.owner.get(cp) != v {
                continue;
            }
            // The child's own values in the order cards write them, so
            // that siblings line up the same way in every instance (the
            // element named "Fasten" before the one named "Inspect").
            let n = node_text(g, c).ok_or_else(|| gone(c))?;
            let mut own = Vec::new();
            for (k, val) in &n.props {
                super::put_str(&mut own, k);
                exact(&mut own, val);
            }
            kids.push((
                g.strings.name(t),
                join(f.shape.get(cp)),
                own,
                join(f.content.get(cp)),
                c,
                n,
            ));
        }
        kids.sort_by(|a, b| (a.0, a.1, &a.2, a.3, a.4).cmp(&(b.0, b.1, &b.2, b.3, b.4)));
        for (_, _, _, _, c, n) in kids.into_iter().rev() {
            stack.push((c, p, n));
        }
    }
    Ok((ids, parent, texts))
}

/// The shape under `root`, in preorder.
pub fn walk(g: &Graph, f: &mut Forest, h: &Hier, root: u64) -> Result<Inst> {
    let (ids, parent, nodes) = ordered(g, f, h, root)?;
    let mut pos: HashMap<u64, u32> = HashMap::with_capacity(ids.len());
    for (p, &v) in ids.iter().enumerate() {
        pos.insert(v, p as u32);
    }
    let mut edges: Vec<IEdge> = Vec::new();
    let mut loose: Vec<u64> = Vec::new();
    let root_owner = f.owner_of(root);
    for p in 0..ids.len() {
        let id = ids[p];
        let mut adj = Vec::new();
        g.for_each_adj(id, Dir::Out, None, |a| adj.push(a));
        step(adj.len() as u64)?;
        for a in adj {
            let followed = h.followed(a.etype);
            let target = pos.get(&a.other).copied();
            let tree = followed
                && match h.dir {
                    Dir::Out => target.is_some_and(|q| q != 0 && parent[q as usize] == p as u32),
                    _ => p != 0 && target == Some(parent[p]),
                };
            if !tree && p == 0 && followed && h.dir == Dir::In && a.other == root_owner {
                loose.push(a.edge);
                continue;
            }
            let mut props = g.edge_props(a.edge);
            notation::sort_props(&mut props);
            edges.push(IEdge {
                src: p as u32,
                etype: g.strings.name(a.etype).to_string(),
                to: match target {
                    Some(q) => To::Pos(q),
                    None => To::Node(a.other),
                },
                props,
                tree,
            });
        }
    }
    edges.sort_by_cached_key(edge_key);
    Ok(Inst {
        ids,
        parent,
        nodes,
        edges,
        loose,
    })
}

/// Everything that must be the same for two shapes to be instances of one
/// template, values aside: positions, labels, keys, edges and the nodes
/// referred to.
pub fn structure(inst: &Inst) -> Vec<u8> {
    let mut b = vec![1u8];
    b.extend_from_slice(&(inst.ids.len() as u32).to_be_bytes());
    for (p, n) in inst.nodes.iter().enumerate() {
        b.extend_from_slice(&inst.parent[p].to_be_bytes());
        b.extend_from_slice(&(n.labels.len() as u32).to_be_bytes());
        for l in &n.labels {
            super::put_str(&mut b, l);
        }
        b.extend_from_slice(&(n.props.len() as u32).to_be_bytes());
        for (k, _) in &n.props {
            super::put_str(&mut b, k);
        }
    }
    b.extend_from_slice(&(inst.edges.len() as u32).to_be_bytes());
    for e in &inst.edges {
        b.extend_from_slice(&e.src.to_be_bytes());
        super::put_str(&mut b, &e.etype);
        match e.to {
            To::Pos(p) => {
                b.push(0);
                b.extend_from_slice(&p.to_be_bytes());
            }
            To::Node(n) => {
                b.push(1);
                b.extend_from_slice(&n.to_be_bytes());
            }
        }
        b.push(e.tree as u8);
        b.extend_from_slice(&(e.props.len() as u32).to_be_bytes());
        for (k, _) in &e.props {
            super::put_str(&mut b, k);
        }
    }
    b
}

// ------------------------------------------------------------- templates

/// How a template has a property.
#[derive(Clone, Debug, PartialEq)]
pub enum Prop {
    /// The same value in every instance.
    Const(Value),
    /// Each instance's own.
    Slot,
    /// The value of the same key at position `base` (a template's root),
    /// its first `k` bytes (all of it when `full`), then `suffix`.
    Derived {
        base: u32,
        k: u32,
        full: bool,
        suffix: String,
    },
}

#[derive(Clone, Debug)]
pub struct TEdge {
    pub src: u32,
    pub etype: String,
    pub to: To,
    pub props: Vec<(String, Prop)>,
    pub tree: bool,
}

/// Where a slot's value goes: a node's property (`edge` is [`NO_SLOT_EDGE`])
/// or an edge's, by index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotRef {
    pub pos: u32,
    pub edge: u32,
    pub key: u32,
}

pub const NO_SLOT_EDGE: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub struct Template {
    /// Depends on content only (D7).
    pub id: String,
    /// The hash of [`structure`], by which instances are found.
    pub group: u128,
    /// The forest's shape hash of its root: what a root must have before
    /// it is walked and compared.
    pub shape: u128,
    pub structure: Vec<u8>,
    pub parent: Vec<u32>,
    pub labels: Vec<Vec<String>>,
    pub props: Vec<Vec<(String, Prop)>>,
    pub edges: Vec<TEdge>,
    /// Where another template is used for a part of this one: (position,
    /// index of that template in the list this one belongs to).
    pub nested: Vec<(u32, usize)>,
    /// Slots in the order a use writes their values.
    pub slots: Vec<SlotRef>,
    /// Its instances: `count` roots from `start` in the list of roots.
    pub start: usize,
    pub count: usize,
}

impl Template {
    pub fn nodes(&self) -> usize {
        self.parent.len()
    }
    pub fn root_label(&self) -> String {
        self.labels
            .first()
            .and_then(|l| l.first())
            .cloned()
            .unwrap_or_default()
    }
}

/// What one property is across the instances seen so far.
struct Track {
    first: Value,
    same: bool,
    /// Derived from the root: (prefix length, whether the whole root value,
    /// suffix), for the whole-value and the prefix forms.
    full: Option<String>,
    prefix: Option<(usize, String)>,
}

fn lcp(a: &str, b: &str) -> usize {
    let mut n = 0;
    for ((i, x), y) in a.char_indices().zip(b.chars()) {
        if x != y {
            return i;
        }
        n = i + x.len_utf8();
    }
    n
}

impl Track {
    fn new(v: &Value, root: Option<&Value>) -> Track {
        let (mut full, mut prefix) = (None, None);
        if let (Value::Text(s), Some(Value::Text(r))) = (v, root) {
            let k = lcp(r, s);
            if k == r.len() {
                full = Some(s[k..].to_string());
            }
            prefix = Some((k, s[k..].to_string()));
        }
        Track {
            first: v.clone(),
            same: true,
            full,
            prefix,
        }
    }

    fn see(&mut self, v: &Value, root: Option<&Value>) {
        if self.same && !same(&self.first, v) {
            self.same = false;
        }
        let (s, r) = match (v, root) {
            (Value::Text(s), Some(Value::Text(r))) => (s.as_str(), r.as_str()),
            _ => {
                self.full = None;
                self.prefix = None;
                return;
            }
        };
        if let Some(sfx) = &self.full {
            if !(s.len() == r.len() + sfx.len() && s.starts_with(r) && s.ends_with(sfx.as_str())) {
                self.full = None;
            }
        }
        if let Some((k, sfx)) = &self.prefix {
            let ok = r.len() >= *k
                && r.is_char_boundary(*k)
                && s.len() == k + sfx.len()
                && s.starts_with(&r[..*k])
                && s.ends_with(sfx.as_str());
            if !ok {
                self.prefix = None;
            }
        }
    }

    fn prop(self, derive: bool) -> Prop {
        if self.same {
            return Prop::Const(self.first);
        }
        if derive {
            if let Some(suffix) = self.full {
                return Prop::Derived {
                    base: 0,
                    k: 0,
                    full: true,
                    suffix,
                };
            }
            if let Some((k, suffix)) = self.prefix {
                return Prop::Derived {
                    base: 0,
                    k: k as u32,
                    full: false,
                    suffix,
                };
            }
        }
        Prop::Slot
    }
}

/// Property tracks for a whole shape, fed one instance at a time.
struct Tracks {
    nodes: Vec<Vec<Track>>,
    edges: Vec<Vec<Track>>,
}

fn root_value<'a>(inst: &'a Inst, key: &str) -> Option<&'a Value> {
    inst.nodes[0]
        .props
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

impl Tracks {
    fn new(inst: &Inst) -> Tracks {
        Tracks {
            nodes: inst
                .nodes
                .iter()
                .enumerate()
                .map(|(p, n)| {
                    n.props
                        .iter()
                        .map(|(k, v)| Track::new(v, (p > 0).then(|| root_value(inst, k)).flatten()))
                        .collect()
                })
                .collect(),
            edges: inst
                .edges
                .iter()
                .map(|e| e.props.iter().map(|(_, v)| Track::new(v, None)).collect())
                .collect(),
        }
    }

    fn see(&mut self, inst: &Inst) {
        for (p, n) in inst.nodes.iter().enumerate() {
            for (i, (k, v)) in n.props.iter().enumerate() {
                let r = (p > 0).then(|| root_value(inst, k)).flatten();
                self.nodes[p][i].see(v, r);
            }
        }
        for (j, e) in inst.edges.iter().enumerate() {
            for (i, (_, v)) in e.props.iter().enumerate() {
                self.edges[j][i].see(v, None);
            }
        }
    }
}

/// A node outside a shape as a template's id sees it: by identity value
/// when it has one, else by its labels and values (D7).
fn ref_key(g: &Graph, names: &Names, id: u64) -> Vec<u8> {
    let mut b = Vec::new();
    if let Some(n) = node_text(g, id) {
        if let Some(k) = names.identity_key(&n.props) {
            b.push(b'i');
            super::put_str(&mut b, k);
            exact(&mut b, &n.props.iter().find(|(pk, _)| pk == k).unwrap().1);
            return b;
        }
        b.push(b'c');
        for l in &n.labels {
            super::put_str(&mut b, l);
        }
        for (k, v) in &n.props {
            super::put_str(&mut b, k);
            exact(&mut b, v);
        }
    }
    b
}

fn prop_bytes(b: &mut Vec<u8>, k: &str, p: &Prop) {
    super::put_str(b, k);
    match p {
        Prop::Const(v) => {
            b.push(b'c');
            exact(b, v);
        }
        Prop::Slot => b.push(b's'),
        Prop::Derived {
            base,
            k,
            full,
            suffix,
        } => {
            b.push(b'd');
            b.extend_from_slice(&base.to_be_bytes());
            b.extend_from_slice(&k.to_be_bytes());
            b.push(*full as u8);
            super::put_str(b, suffix);
        }
    }
}

/// A template's id: a hash of its definition with referenced nodes by
/// content, so the same data under other ids has the same templates.
fn template_id(g: &Graph, names: &Names, t: &Template) -> String {
    let mut b = vec![b'T'];
    b.extend_from_slice(&(t.nodes() as u32).to_be_bytes());
    for p in 0..t.nodes() {
        b.extend_from_slice(&t.parent[p].to_be_bytes());
        b.extend_from_slice(&(t.labels[p].len() as u32).to_be_bytes());
        for l in &t.labels[p] {
            super::put_str(&mut b, l);
        }
        b.extend_from_slice(&(t.props[p].len() as u32).to_be_bytes());
        for (k, pr) in &t.props[p] {
            prop_bytes(&mut b, k, pr);
        }
    }
    let mut sum = 0u128;
    for e in &t.edges {
        let mut x = vec![b'e'];
        x.extend_from_slice(&e.src.to_be_bytes());
        super::put_str(&mut x, &e.etype);
        x.push(e.tree as u8);
        match e.to {
            To::Pos(p) => {
                x.push(0);
                x.extend_from_slice(&p.to_be_bytes());
            }
            To::Node(n) => {
                x.push(1);
                x.extend_from_slice(&ref_key(g, names, n));
            }
        }
        for (k, pr) in &e.props {
            prop_bytes(&mut x, k, pr);
        }
        sum = sum.wrapping_add(sip128(&x));
    }
    b.extend_from_slice(&(t.edges.len() as u32).to_be_bytes());
    put_hash(&mut b, sum);
    format!("{:032x}", sip128(&b))
}

/// The template a set of instances makes, from its first instance and the
/// tracks of all of them.
fn make(
    g: &Graph,
    names: &Names,
    first: &Inst,
    structure: Vec<u8>,
    tracks: Tracks,
    shape: u128,
) -> Template {
    let props = first
        .nodes
        .iter()
        .zip(tracks.nodes)
        .enumerate()
        .map(|(p, (n, ts))| {
            n.props
                .iter()
                .zip(ts)
                .map(|((k, _), t)| (k.clone(), t.prop(p > 0)))
                .collect()
        })
        .collect();
    let edges = first
        .edges
        .iter()
        .zip(tracks.edges)
        .map(|(e, ts)| TEdge {
            src: e.src,
            etype: e.etype.clone(),
            to: e.to,
            props: e
                .props
                .iter()
                .zip(ts)
                .map(|((k, _), t)| (k.clone(), t.prop(false)))
                .collect(),
            tree: e.tree,
        })
        .collect();
    let mut t = Template {
        id: String::new(),
        group: sip128(&structure),
        shape,
        structure,
        parent: first.parent.clone(),
        labels: first.nodes.iter().map(|n| n.labels.clone()).collect(),
        props,
        edges,
        nested: Vec::new(),
        slots: Vec::new(),
        start: 0,
        count: 0,
    };
    t.id = template_id(g, names, &t);
    t.slots = definition(g, names, &t, &[], "T", &|_| String::new()).1;
    t
}

/// The position ranges taken by nested templates: (start, end exclusive,
/// template index).
pub fn nested_ranges(t: &Template, all: &[Template]) -> Vec<(u32, u32, usize)> {
    let mut r: Vec<(u32, u32, usize)> = t
        .nested
        .iter()
        .map(|&(p, u)| (p, p + all[u].nodes() as u32, u))
        .collect();
    r.sort();
    r
}

/// Whether an instance has exactly what the template says, and if not the
/// first thing that differs.
pub fn matches(
    t: &Template,
    inst: &Inst,
    structure_bytes: &[u8],
) -> std::result::Result<(), String> {
    if structure_bytes != t.structure.as_slice() {
        return Err("its shape is not the template's (nodes, labels, keys, edges or the nodes it refers to differ)".into());
    }
    let vals = resolve(t, inst, &inst.ids);
    for (p, n) in inst.nodes.iter().enumerate() {
        for (i, (k, v)) in n.props.iter().enumerate() {
            if !same(&vals.0[p][i], v) {
                return Err(format!(
                    "node {} (position .{}) has {k}={} where the template has {}",
                    inst.ids[p],
                    p + 1,
                    lit(v),
                    lit(&vals.0[p][i])
                ));
            }
        }
    }
    for (j, e) in inst.edges.iter().enumerate() {
        for (i, (k, v)) in e.props.iter().enumerate() {
            if !same(&vals.1[j][i], v) {
                return Err(format!(
                    "an edge {} from position .{} has {k}={} where the template has {}",
                    e.etype,
                    e.src + 1,
                    lit(v),
                    lit(&vals.1[j][i])
                ));
            }
        }
    }
    Ok(())
}

fn lit(v: &Value) -> String {
    let mut s = String::new();
    notation::value(v, &mut s);
    s
}

/// The slot values an instance writes, in the template's order.
pub fn slot_values(t: &Template, inst: &Inst) -> Vec<Value> {
    t.slots
        .iter()
        .map(|s| {
            if s.edge == NO_SLOT_EDGE {
                inst.nodes[s.pos as usize].props[s.key as usize].1.clone()
            } else {
                inst.edges[s.edge as usize].props[s.key as usize].1.clone()
            }
        })
        .collect()
}

/// Every property value of an instance as the template and the instance's
/// slot values say it is: (per node, per edge).
type Resolved = (Vec<Vec<Value>>, Vec<Vec<Value>>);

fn resolve_from(t: &Template, slots: &[Value]) -> Resolved {
    let mut nodes: Vec<Vec<Option<Value>>> = t.props.iter().map(|p| vec![None; p.len()]).collect();
    let mut edges: Vec<Vec<Option<Value>>> =
        t.edges.iter().map(|e| vec![None; e.props.len()]).collect();
    for (s, v) in t.slots.iter().zip(slots) {
        if s.edge == NO_SLOT_EDGE {
            nodes[s.pos as usize][s.key as usize] = Some(v.clone());
        } else {
            edges[s.edge as usize][s.key as usize] = Some(v.clone());
        }
    }
    // In preorder a template's root comes before what is derived from it.
    for p in 0..t.props.len() {
        for i in 0..t.props[p].len() {
            let (k, pr) = &t.props[p][i];
            let v = match pr {
                Prop::Const(v) => v.clone(),
                Prop::Slot => nodes[p][i].clone().unwrap_or(Value::Null),
                Prop::Derived {
                    base,
                    k: n,
                    full,
                    suffix,
                } => {
                    let b = *base as usize;
                    let r = t.props[b]
                        .iter()
                        .position(|(bk, _)| bk == k)
                        .and_then(|j| nodes[b][j].clone());
                    match r {
                        Some(Value::Text(r)) => {
                            let head = if *full {
                                r.as_str()
                            } else {
                                r.get(..*n as usize).unwrap_or("")
                            };
                            Value::Text(format!("{head}{suffix}"))
                        }
                        _ => Value::Null,
                    }
                }
            };
            nodes[p][i] = Some(v);
        }
    }
    for (j, e) in t.edges.iter().enumerate() {
        for (i, (_, pr)) in e.props.iter().enumerate() {
            if let Prop::Const(v) = pr {
                edges[j][i] = Some(v.clone());
            }
        }
    }
    let unwrap = |v: Vec<Vec<Option<Value>>>| {
        v.into_iter()
            .map(|x| x.into_iter().map(|y| y.unwrap_or(Value::Null)).collect())
            .collect()
    };
    (unwrap(nodes), unwrap(edges))
}

fn resolve(t: &Template, inst: &Inst, _ids: &[u64]) -> Resolved {
    resolve_from(t, &slot_values(t, inst))
}

/// A node or an edge given back by [`rebuild`].
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Node {
        id: u64,
        labels: Vec<String>,
        props: Vec<(String, Value)>,
    },
    Edge {
        from: u64,
        etype: String,
        to: u64,
        props: Vec<(String, Value)>,
    },
}

/// An instance's nodes and edges from the template alone, the instance's
/// node ids (in position order) and its slot values.
pub fn rebuild(t: &Template, ids: &[u64], slots: &[Value]) -> Vec<Part> {
    let (nv, ev) = resolve_from(t, slots);
    let mut out = Vec::with_capacity(ids.len() + t.edges.len());
    for p in 0..t.nodes() {
        out.push(Part::Node {
            id: ids[p],
            labels: t.labels[p].clone(),
            props: t.props[p]
                .iter()
                .zip(&nv[p])
                .map(|((k, _), v)| (k.clone(), v.clone()))
                .collect(),
        });
    }
    for (j, e) in t.edges.iter().enumerate() {
        out.push(Part::Edge {
            from: ids[e.src as usize],
            etype: e.etype.clone(),
            to: match e.to {
                To::Pos(q) => ids[q as usize],
                To::Node(n) => n,
            },
            props: e
                .props
                .iter()
                .zip(&ev[j])
                .map(|((k, _), v)| (k.clone(), v.clone()))
                .collect(),
        });
    }
    out
}

// ------------------------------------------------------------ writing

/// A template's definition, as a dictionary holds it, with the slot order
/// that reading it gives. `name` is how uses call it (`T3`); `short` names
/// the templates nested in it.
pub fn definition(
    g: &Graph,
    names: &Names,
    t: &Template,
    all: &[Template],
    name: &str,
    short: &dyn Fn(usize) -> String,
) -> (String, Vec<SlotRef>) {
    let mut s = String::new();
    let mut slots: Vec<SlotRef> = Vec::new();
    s.push_str(name);
    s.push_str(" #");
    s.push_str(&t.id[..16]);
    s.push('\n');
    let ranges = nested_ranges(t, all);
    let inside = |p: u32| ranges.iter().any(|&(a, b, _)| p > a && p < b);
    let mut p = 0u32;
    while (p as usize) < t.nodes() {
        if let Some(&(a, b, u)) = ranges.iter().find(|r| r.0 == p) {
            s.push_str(&format!(" .{}={}", a + 1, short(u)));
            let ut = &all[u];
            // Values of the nested root that this template gives itself
            // (derived from its own root, or constant here) where the nested
            // template has a slot: written after it, `.3=T2 code=~"-L"`.
            let given =
                |i: usize| ut.props[0][i].1 == Prop::Slot && t.props[a as usize][i].1 != Prop::Slot;
            for (i, (k, pr)) in t.props[a as usize].iter().enumerate() {
                if given(i) {
                    s.push(' ');
                    notation::name(k, &mut s);
                    s.push('=');
                    prop_text(pr, &mut s);
                }
            }
            s.push('\n');
            // The nested template's slots, at its positions here.
            let first_here = edge_starts(t);
            let first_there = edge_starts(ut);
            for r in &ut.slots {
                if r.pos == 0 && r.edge == NO_SLOT_EDGE && given(r.key as usize) {
                    continue;
                }
                slots.push(SlotRef {
                    pos: r.pos + a,
                    edge: if r.edge == NO_SLOT_EDGE {
                        NO_SLOT_EDGE
                    } else {
                        let src = ut.edges[r.edge as usize].src;
                        first_here[(src + a) as usize] + (r.edge - first_there[src as usize])
                    },
                    key: r.key,
                });
            }
            p = b;
            continue;
        }
        s.push_str(&format!(" .{}", p + 1));
        for l in &t.labels[p as usize] {
            s.push(':');
            notation::name(l, &mut s);
        }
        for (i, (k, pr)) in t.props[p as usize].iter().enumerate() {
            s.push(' ');
            notation::name(k, &mut s);
            s.push('=');
            prop_text(pr, &mut s);
            if *pr == Prop::Slot {
                slots.push(SlotRef {
                    pos: p,
                    edge: NO_SLOT_EDGE,
                    key: i as u32,
                });
            }
        }
        s.push('\n');
        p += 1;
    }
    // Edges from the positions this definition writes itself, per source
    // as plain text writes them.
    let mut by_src: Vec<Vec<usize>> = vec![Vec::new(); t.nodes()];
    for (j, e) in t.edges.iter().enumerate() {
        let own = !inside(e.src) && !ranges.iter().any(|r| r.0 == e.src);
        if own {
            by_src[e.src as usize].push(j);
        }
    }
    let target = |to: To| match to {
        To::Pos(q) => format!(".{}", q + 1),
        To::Node(n) => names.handle(g, n),
    };
    for (src, list) in by_src.iter().enumerate() {
        let mut lines: Vec<(String, bool, String, String, usize)> = list
            .iter()
            .map(|&j| {
                let e = &t.edges[j];
                let mut ps = String::new();
                if !e.props.is_empty() {
                    ps.push('{');
                    for (i, (k, pr)) in e.props.iter().enumerate() {
                        if i > 0 {
                            ps.push_str(", ");
                        }
                        notation::name(k, &mut ps);
                        ps.push('=');
                        prop_text(pr, &mut ps);
                    }
                    ps.push('}');
                }
                (e.etype.clone(), !e.props.is_empty(), target(e.to), ps, j)
            })
            .collect();
        lines.sort();
        let mut i = 0;
        while i < lines.len() {
            let (ty, propped, to, ps, j) = &lines[i];
            s.push_str(&format!(" .{} -", src + 1));
            notation::name(ty, &mut s);
            if *propped {
                s.push_str(ps);
                s.push_str("-> ");
                s.push_str(to);
                for (k, (_, pr)) in t.edges[*j].props.iter().enumerate() {
                    if *pr == Prop::Slot {
                        slots.push(SlotRef {
                            pos: src as u32,
                            edge: *j as u32,
                            key: k as u32,
                        });
                    }
                }
                i += 1;
            } else {
                s.push_str("-> ");
                let mut first = true;
                while i < lines.len() && lines[i].0 == *ty && !lines[i].1 {
                    if !first {
                        s.push_str(", ");
                    }
                    first = false;
                    s.push_str(&lines[i].2);
                    i += 1;
                }
            }
            s.push('\n');
        }
    }
    (s, slots)
}

/// For each position, the index of its first edge (edges are sorted by
/// source position); one past the end for the last.
fn edge_starts(t: &Template) -> Vec<u32> {
    let mut v = vec![0u32; t.nodes() + 1];
    let mut j = 0usize;
    for (p, slot) in v.iter_mut().enumerate() {
        while j < t.edges.len() && (t.edges[j].src as usize) < p {
            j += 1;
        }
        *slot = j as u32;
    }
    v
}

fn prop_text(p: &Prop, out: &mut String) {
    match p {
        Prop::Const(v) => notation::value(v, out),
        Prop::Slot => out.push('$'),
        Prop::Derived {
            k, full, suffix, ..
        } => {
            out.push('~');
            if !full {
                out.push_str(&k.to_string());
            }
            notation::value(&Value::Text(suffix.clone()), out);
        }
    }
}

/// A use: `T3 n242..281 "P01" 50 ...`, then any edge from its nodes that
/// is not part of the shape. Ids are left out for nodes written by
/// identity, whose handle the slots or the template already give.
pub fn use_text(g: &Graph, names: &Names, t: &Template, inst: &Inst, name: &str) -> String {
    let mut s = String::from(name);
    let listed: Vec<u64> = if names.identity.is_empty() {
        inst.ids.clone()
    } else {
        inst.ids
            .iter()
            .zip(&inst.nodes)
            .filter(|(_, n)| names.identity_key(&n.props).is_none())
            .map(|(id, _)| *id)
            .collect()
    };
    if !listed.is_empty() {
        s.push(' ');
        if names.identity.is_empty() {
            notation::id_list(&listed, &mut s);
        } else {
            let hs: Vec<String> = listed.iter().map(|id| format!("n{id}")).collect();
            s.push_str(&hs.join(","));
        }
    }
    for v in slot_values(t, inst) {
        s.push(' ');
        notation::value(&v, &mut s);
    }
    s.push('\n');
    if !inst.loose.is_empty() {
        let mut by: HashMap<u64, Vec<notation::EdgeText>> = HashMap::new();
        for &e in &inst.loose {
            if let Some(er) = g.edge(e) {
                let mut props = g.edge_props(e);
                notation::sort_props(&mut props);
                by.entry(er.from).or_default().push(notation::EdgeText {
                    etype: g.strings.name(er.etype).to_string(),
                    to: names.handle(g, er.to),
                    props,
                });
            }
        }
        let mut keys: Vec<u64> = by.keys().copied().collect();
        keys.sort();
        for k in keys {
            let from = names.handle(g, k);
            notation::edge_lines(&from, by.remove(&k).unwrap(), &mut s);
        }
    }
    s
}

/// The plain text of an instance's nodes.
pub fn plain_text(g: &Graph, names: &Names, inst: &Inst) -> String {
    let mut s = String::new();
    for &id in &inst.ids {
        notation::plain_block(g, names, id, &|_| false, &mut s);
    }
    s
}

// ------------------------------------------------------------ finding

/// Templates found in a graph, with their instances.
pub struct Found {
    pub forest: Forest,
    pub templates: Vec<Template>,
    /// Every template's instance roots, `start..start+count` each.
    pub roots: StateVec<u64>,
    /// Per root: the use's cost and the plain text's cost it replaces.
    pub use_cost: StateVec<(u64, u64)>,
    pub plain_cost: StateVec<(u64, u64)>,
    /// Per template: its definition's cost, flat.
    pub def_cost: Vec<Cost>,
    /// Roots looked at.
    pub candidates: u64,
}

/// How members of a group are ordered: by identity value when given, then
/// id.
pub fn order_key(g: &Graph, names: &Names, id: u64) -> Vec<u8> {
    let mut k = Vec::new();
    if !names.identity.is_empty() {
        if let Some(n) = node_text(g, id) {
            if let Some(key) = names.identity_key(&n.props) {
                exact(&mut k, &n.props.iter().find(|(pk, _)| pk == key).unwrap().1);
            }
        }
    }
    k.push(0xff);
    k.extend_from_slice(&id.to_be_bytes());
    k
}

fn sorter(g: &Graph, name: &str) -> Result<Sorter> {
    super::sorter(g, name)
}

/// Whether node `id` may be a root.
fn may_root(g: &Graph, label_ids: &[u32], id: u64) -> bool {
    label_ids.is_empty() || label_ids.iter().any(|&l| g.has_label(id, l))
}

/// Find every template: walk each candidate root once to group by shape,
/// then each group's members twice, to find its constants and slots
/// (comparing every member with the first in full) and to price each use.
pub fn find(g: &Graph, h: &Hier) -> Result<Found> {
    let mut forest = Forest::build(g, h)?;
    let label_ids: Vec<u32> = h
        .labels
        .iter()
        .filter_map(|l| g.strings.lookup(l))
        .collect();
    if !h.labels.is_empty() && label_ids.is_empty() {
        return Ok(Found {
            forest,
            templates: Vec::new(),
            roots: StateVec::new(0, 0),
            use_cost: StateVec::new(0, (0, 0)),
            plain_cost: StateVec::new(0, (0, 0)),
            def_cost: Vec::new(),
            candidates: 0,
        });
    }
    // Roots whose forest shape is not repeated cannot be in a template:
    // only the others are walked.
    let mut by_shape = sorter(g, "structure-template-shapes")?;
    let mut looked = 0u64;
    for v in 0..forest.len {
        step(1)?;
        let size = forest.size.get(v) as usize;
        if size < h.min_nodes || size > h.max_nodes {
            continue;
        }
        let id = forest.ids.get(v);
        if !may_root(g, &label_ids, id) {
            continue;
        }
        let mut k = join(forest.shape.get(v)).to_be_bytes().to_vec();
        k.extend_from_slice(&id.to_be_bytes());
        by_shape.push(&k, &[]).map_err(Error::Io)?;
        looked += 1;
    }
    let mut s = sorter(g, "structure-templates")?;
    let mut candidates = 0u64;
    let mut shapes = by_shape.finish().map_err(Error::Io)?;
    let mut run: SpillQueue<u64> = SpillQueue::new();
    let mut at: Option<Vec<u8>> = None;
    loop {
        let next = shapes.next().map_err(Error::Io)?;
        let head = next.as_ref().map(|(k, _)| k[..16].to_vec());
        if head != at {
            if run.len() >= h.min_count as u64 {
                while let Some(id) = run.pop_front() {
                    let inst = walk(g, &mut forest, h, id)?;
                    let mut k = sip128(&structure(&inst)).to_be_bytes().to_vec();
                    k.extend_from_slice(&order_key(g, &h.names, id));
                    s.push(&k, &id.to_be_bytes()).map_err(Error::Io)?;
                    candidates += 1;
                }
            }
            run = SpillQueue::new();
            at = head;
        }
        match next {
            Some((k, _)) => run.push_back(u64::from_be_bytes(k[16..24].try_into().unwrap())),
            None => break,
        }
    }
    let mut roots = StateVec::new(candidates as usize, 0u64);
    let mut use_cost = StateVec::new(candidates as usize, (0u64, 0u64));
    let mut plain_cost = StateVec::new(candidates as usize, (0u64, 0u64));
    let mut templates: Vec<Template> = Vec::new();
    let mut def_cost: Vec<Cost> = Vec::new();
    let mut filled = 0usize;
    let mut sorted = s.finish().map_err(Error::Io)?;
    let mut run: SpillQueue<u64> = SpillQueue::new();
    let mut at: Option<Vec<u8>> = None;
    loop {
        let next = sorted.next().map_err(Error::Io)?;
        let head = next.as_ref().map(|(k, _)| k[..16].to_vec());
        if head != at {
            if run.len() >= h.min_count as u64 {
                if let Some(t) = group(
                    g,
                    &mut forest,
                    h,
                    &mut run,
                    &mut roots,
                    &mut use_cost,
                    &mut plain_cost,
                    &mut filled,
                )? {
                    if templates.len() == MAX_TEMPLATES {
                        return Err(Error::Msg(format!(
                            "more than {MAX_TEMPLATES} templates; narrow them with labels:, types: or a larger min_count: or min_nodes:"
                        )));
                    }
                    def_cost.push(Cost::of(
                        &definition(g, &h.names, &t, &[], "T", &|_| String::new()).0,
                    ));
                    templates.push(t);
                }
            }
            run = SpillQueue::new();
            at = head;
        }
        match next {
            Some((_, v)) => run.push_back(u64::from_be_bytes(v[..8].try_into().unwrap())),
            None => break,
        }
    }
    // Two different templates can share an id only when they refer to
    // different nodes that are alike in content: number them apart, in
    // the order of their first instance.
    let mut order: Vec<usize> = (0..templates.len()).collect();
    let firsts: Vec<Vec<u8>> = templates
        .iter()
        .map(|t| order_key(g, &h.names, roots.get(t.start)))
        .collect();
    order.sort_by(|&a, &b| (&templates[a].id, &firsts[a]).cmp(&(&templates[b].id, &firsts[b])));
    let mut seen = 1;
    for w in 1..order.len() {
        if templates[order[w]]
            .id
            .starts_with(&templates[order[w - 1]].id[..32])
        {
            seen += 1;
            let id = format!("{}.{seen}", &templates[order[w]].id[..32]);
            templates[order[w]].id = id;
        } else {
            seen = 1;
        }
    }
    Ok(Found {
        forest,
        templates,
        roots,
        use_cost,
        plain_cost,
        def_cost,
        candidates: looked,
    })
}

/// One group of roots with the same shape hash: its template, when at
/// least `min_count` of them really are the same shape.
#[allow(clippy::too_many_arguments)]
fn group(
    g: &Graph,
    f: &mut Forest,
    h: &Hier,
    run: &mut SpillQueue<u64>,
    roots: &mut StateVec<u64>,
    use_cost: &mut StateVec<(u64, u64)>,
    plain_cost: &mut StateVec<(u64, u64)>,
    filled: &mut usize,
) -> Result<Option<Template>> {
    let mut members: SpillQueue<u64> = SpillQueue::new();
    let mut first: Option<(Inst, Vec<u8>, Tracks)> = None;
    while let Some(id) = run.pop_front() {
        let inst = walk(g, f, h, id)?;
        let st = structure(&inst);
        match first.as_mut() {
            None => {
                let tr = Tracks::new(&inst);
                first = Some((inst, st, tr));
            }
            Some((_, fs, tr)) => {
                if st != *fs {
                    continue;
                }
                tr.see(&inst);
            }
        }
        members.push_back(id);
    }
    if members.len() < h.min_count as u64 {
        return Ok(None);
    }
    let (inst0, st0, tracks) = first.unwrap();
    let shape = f.shape_of(inst0.ids[0]).unwrap_or_default();
    let mut t = make(g, &h.names, &inst0, st0, tracks, shape);
    t.start = *filled;
    while let Some(id) = members.pop_front() {
        let inst = walk(g, f, h, id)?;
        roots.set(*filled, id);
        let u = Cost::of(&use_text(g, &h.names, &t, &inst, "T"));
        let p = Cost::of(&plain_text(g, &h.names, &inst));
        use_cost.set(*filled, (u.bytes, u.tokens));
        plain_cost.set(*filled, (p.bytes, p.tokens));
        *filled += 1;
    }
    t.count = *filled - t.start;
    Ok(Some(t))
}

/// Estimated saving of a template used for all its instances alone:
/// (bytes, tokens).
pub fn saving(found: &mut Found, i: usize) -> (i64, i64) {
    let t = &found.templates[i];
    let (mut b, mut k) = (
        -(found.def_cost[i].bytes as i64),
        -(found.def_cost[i].tokens as i64),
    );
    for r in t.start..t.start + t.count {
        let u = found.use_cost.get(r);
        let p = found.plain_cost.get(r);
        b += p.0 as i64 - u.0 as i64;
        k += p.1 as i64 - u.1 as i64;
    }
    (b, k)
}

// ------------------------------------------------------------ procedures

fn template_order(found: &mut Found) -> Vec<(usize, (i64, i64))> {
    let mut v: Vec<(usize, (i64, i64))> = (0..found.templates.len())
        .map(|i| (i, saving(found, i)))
        .collect();
    v.sort_by(|a, b| {
        b.1 .1
            .cmp(&a.1 .1)
            .then_with(|| found.templates[a.0].id.cmp(&found.templates[b.0].id))
    });
    v
}

pub fn templates(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let limit = limit_arg(a)?;
    let (h, mut found) = match super::store_name(a, "dictionary")? {
        // The kept dictionary's templates, as the graph has them now.
        Some(d) => {
            let k = super::plan::load(g, &d, super::fresh_arg(a)?)?;
            let h = k.meta.hier.rebind(g);
            let found = super::plan::find_kept(g, &h, &k.dict)?;
            (h, found)
        }
        None => {
            let h = Hier::from(g, a)?;
            let found = find(g, &h)?;
            (h, found)
        }
    };
    if let Some(of) = a.str("instances") {
        if of == "*" && a.bool("grouped", false) {
            return grouped_instances(&h, g, &mut found, a);
        }
        if of == "*" {
            return all_instances(g, &h, &mut found, a, limit);
        }
        return instances(g, &h, &mut found, &of, a, limit);
    }
    let after = a.usize("after", 0);
    let order = template_order(&mut found);
    let mut rows = Vec::new();
    for (rank, (i, (sb, st))) in order.iter().enumerate().skip(after).take(limit) {
        step(1)?;
        let t = &found.templates[*i];
        let mut plain = (0i64, 0i64);
        for r in t.start..t.start + t.count {
            let p = found.plain_cost.get(r);
            plain.0 += p.0 as i64;
            plain.1 += p.1 as i64;
        }
        let def = definition(g, &h.names, t, &[], "T", &|_| String::new()).0;
        rows.push(vec![
            Value::Int(rank as i64 + 1),
            Value::Text(t.id.clone()),
            Value::Text(t.root_label()),
            Value::Int(t.nodes() as i64),
            Value::Int(t.edges.len() as i64),
            Value::Int(t.slots.len() as i64),
            Value::Int(t.count as i64),
            Value::Int(plain.1),
            Value::Int(*sb),
            Value::Int(*st),
            Value::Text(def),
        ]);
    }
    let shown = rows.len();
    let mut r = QueryResult::table(
        [
            "rank",
            "template",
            "root",
            "nodes",
            "edges",
            "slots",
            "instances",
            "plain_tokens",
            "saving_bytes",
            "saving_tokens",
            "definition",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        rows,
    );
    let covered: usize = found.templates.iter().map(|t| t.count).sum();
    let mut msg = format!(
        "{} templates of {} or more nodes repeated {} or more times, with {covered} instances among {} roots looked at; an instance keeps every node it has, and CALL structure.expand(of: <template>, root: <id>) gives them back",
        found.templates.len(),
        h.min_nodes,
        h.min_count,
        found.candidates
    );
    if after + shown < order.len() {
        msg.push_str(&format!(
            "; more: CALL structure.templates again with after: {} and the same arguments",
            after + shown
        ));
    }
    r.message = Some(msg);
    Ok(r)
}

/// The template a caller names: its full id, or the 16 hex digits a
/// definition shows.
pub fn named(found: &Found, of: &str) -> Option<usize> {
    let of = of.trim().trim_start_matches('#');
    found.templates.iter().position(|t| t.id == of).or_else(|| {
        let hits: Vec<usize> = found
            .templates
            .iter()
            .enumerate()
            .filter(|(_, t)| of.len() >= 16 && t.id.starts_with(of))
            .map(|(i, _)| i)
            .collect();
        (hits.len() == 1).then(|| hits[0])
    })
}

fn instances(
    g: &Graph,
    h: &Hier,
    found: &mut Found,
    of: &str,
    a: &Args,
    limit: usize,
) -> Result<QueryResult> {
    let i = named(found, of).ok_or_else(|| {
        Error::Msg(format!(
            "no template '{of}' among those these arguments find; CALL structure.templates with the same arguments lists them"
        ))
    })?;
    let after = a.ids("after")?.first().copied().unwrap_or(0);
    let t = found.templates[i].clone();
    let mut members: Vec<(Vec<u8>, u64)> = (t.start..t.start + t.count)
        .map(|r| {
            let id = found.roots.get(r);
            (order_key(g, &h.names, id), id)
        })
        .collect();
    members.sort();
    let mut rows = Vec::new();
    let mut more = false;
    let mut past = after == 0;
    for (_, id) in members {
        if !past {
            past = id == after;
            continue;
        }
        if rows.len() == limit {
            more = true;
            break;
        }
        step(1)?;
        let inst = walk(g, &mut found.forest, h, id)?;
        rows.push(vec![
            Value::Text(t.id.clone()),
            Value::Int(id as i64),
            Value::Text(h.names.handle(g, id)),
            Value::List(slot_values(&t, &inst)),
        ]);
    }
    let mut r = QueryResult::table(
        ["template", "root", "handle", "slots"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        rows,
    );
    let mut msg = format!("{} instances of template {}", t.count, t.id);
    if more {
        let last = r.rows.last().and_then(|row| row[1].as_i64()).unwrap_or(0);
        msg.push_str(&format!(
            "; more: CALL structure.templates again with instances: '{}', after: {last}",
            t.id
        ));
    }
    r.message = Some(msg);
    Ok(r)
}

/// Roots one `grouped: true` call lists at most.
pub const MAX_GROUPED: usize = 100_000;

/// `instances: "*", grouped: true`: one row per template, `template,
/// instances, roots`, its roots in member order, at most [`MAX_GROUPED`]
/// roots a call; `after:` is the number of roots already read. For a
/// caller that wants every instance of many templates in few calls.
fn grouped_instances(h: &Hier, g: &Graph, found: &mut Found, a: &Args) -> Result<QueryResult> {
    let after = a.usize("after", 0);
    let order = template_order(found);
    let total: usize = found.templates.iter().map(|t| t.count).sum();
    let mut rows = Vec::new();
    let (mut seen, mut shown) = (0usize, 0usize);
    for (i, _) in order {
        if shown == MAX_GROUPED {
            break;
        }
        let t = &found.templates[i];
        if seen + t.count <= after {
            seen += t.count;
            continue;
        }
        let mut members: Vec<(Vec<u8>, u64)> = (t.start..t.start + t.count)
            .map(|r| {
                let id = found.roots.get(r);
                (order_key(g, &h.names, id), id)
            })
            .collect();
        members.sort();
        step(members.len() as u64)?;
        let skip = after.saturating_sub(seen);
        let take = (MAX_GROUPED - shown).min(members.len() - skip);
        let roots: Vec<Value> = members[skip..skip + take]
            .iter()
            .map(|(_, id)| Value::Int(*id as i64))
            .collect();
        seen += skip + take;
        shown += take;
        rows.push(vec![
            Value::Text(t.id.clone()),
            Value::Int(t.count as i64),
            Value::List(roots),
        ]);
    }
    let mut r = QueryResult::table(
        ["template", "instances", "roots"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        rows,
    );
    let mut msg = format!("{total} instances of {} templates", found.templates.len());
    if after + shown < total {
        msg.push_str(&format!(
            "; more: CALL structure.templates again with instances: '*', grouped: true, after: {} and the same arguments",
            after + shown
        ));
    }
    r.message = Some(msg);
    Ok(r)
}

/// `instances: "*"`: every template's instances, template by template in
/// the order `templates` lists them, each in member order; `after:` is the
/// number of rows already read.
fn all_instances(
    g: &Graph,
    h: &Hier,
    found: &mut Found,
    a: &Args,
    limit: usize,
) -> Result<QueryResult> {
    let after = a.usize("after", 0);
    let order = template_order(found);
    let mut rows = Vec::new();
    let mut seen = 0usize;
    let total: usize = found.templates.iter().map(|t| t.count).sum();
    'out: for (i, _) in order {
        let t = found.templates[i].clone();
        if seen + t.count <= after {
            seen += t.count;
            continue;
        }
        let mut members: Vec<(Vec<u8>, u64)> = (t.start..t.start + t.count)
            .map(|r| {
                let id = found.roots.get(r);
                (order_key(g, &h.names, id), id)
            })
            .collect();
        members.sort();
        for (_, id) in members {
            if seen < after {
                seen += 1;
                continue;
            }
            if rows.len() == limit {
                break 'out;
            }
            step(1)?;
            let inst = walk(g, &mut found.forest, h, id)?;
            rows.push(vec![
                Value::Text(t.id.clone()),
                Value::Int(id as i64),
                Value::Text(h.names.handle(g, id)),
                Value::List(slot_values(&t, &inst)),
            ]);
            seen += 1;
        }
    }
    let shown = rows.len();
    let mut r = QueryResult::table(
        ["template", "root", "handle", "slots"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        rows,
    );
    let mut msg = format!("{total} instances of {} templates", found.templates.len());
    if after + shown < total {
        msg.push_str(&format!(
            "; more: CALL structure.templates again with instances: '*', after: {} and the same arguments",
            after + shown
        ));
    }
    r.message = Some(msg);
    Ok(r)
}

/// Rows for the parts [`rebuild`] gives back.
pub fn part_rows(parts: Vec<Part>) -> Vec<Vec<Value>> {
    let pairs = |props: Vec<(String, Value)>| {
        Value::List(
            props
                .into_iter()
                .map(|(k, v)| Value::List(vec![Value::Text(k), v]))
                .collect(),
        )
    };
    parts
        .into_iter()
        .map(|p| match p {
            Part::Node { id, labels, props } => vec![
                Value::Text("node".into()),
                Value::Int(id as i64),
                Value::List(labels.into_iter().map(Value::Text).collect()),
                Value::Null,
                Value::Null,
                Value::Null,
                pairs(props),
            ],
            Part::Edge {
                from,
                etype,
                to,
                props,
            } => vec![
                Value::Text("edge".into()),
                Value::Null,
                Value::Null,
                Value::Text(etype),
                Value::Int(from as i64),
                Value::Int(to as i64),
                pairs(props),
            ],
        })
        .collect()
}

pub fn part_columns() -> Vec<String> {
    ["kind", "id", "labels", "type", "from", "to", "props"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Roots one `expand` call takes at most.
pub const MAX_EXPAND: usize = 1_000;

/// `expand(of: <template>, root: <id or ids>)` against templates found in
/// the call (the same arguments as `templates`) or a kept dictionary: each
/// instance rebuilt from the definition and its slot values alone, after
/// checking in full that the root really is an instance.
pub fn expand_template(g: &Graph, h: &Hier, t: &Template, roots: &[u64]) -> Result<QueryResult> {
    if roots.len() > MAX_EXPAND {
        return Err(Error::Msg(format!(
            "root: {} roots is more than one call expands; give at most {MAX_EXPAND} at a time",
            roots.len()
        )));
    }
    let mut rows = Vec::new();
    let (mut nodes, mut edges, mut slots) = (0usize, 0usize, 0usize);
    for &root in roots {
        let mut f = Forest::local(g, h, root, h.max_nodes.max(1) * 4 + 1024)?;
        let inst = walk(g, &mut f, h, root)?;
        if let Err(why) = matches(t, &inst, &structure(&inst)) {
            return Err(Error::Msg(format!(
                "node {root} is not an instance of template {}: {why}; it is written plainly",
                t.id
            )));
        }
        let vals = slot_values(t, &inst);
        let parts = rebuild(t, &inst.ids, &vals);
        nodes += inst.ids.len();
        edges += parts.len() - inst.ids.len();
        slots += vals.len();
        for mut row in part_rows(parts) {
            row.push(Value::Int(root as i64));
            rows.push(row);
        }
    }
    let mut columns = part_columns();
    columns.push("root".into());
    let mut r = QueryResult::table(columns, rows);
    r.message = Some(format!(
        "{} instances of template {}: {nodes} nodes and {edges} edges, rebuilt from the definition and {slots} slot values",
        roots.len(),
        t.id,
    ));
    Ok(r)
}

/// `t` with template `ui` of `all` used for its part from position `p`:
/// that part's properties are then as the nested template has them
/// (derived values from its own root), its slots are that template's, and
/// what was nested inside the part before is the nested template's to say.
pub fn nest(
    g: &Graph,
    names: &Names,
    t: &Template,
    p: u32,
    ui: usize,
    all: &[Template],
) -> Template {
    let u = &all[ui];
    let mut t2 = t.clone();
    let end = p + u.nodes() as u32;
    t2.nested.retain(|&(q, _)| q < p || q >= end);
    t2.nested.push((p, ui));
    t2.nested.sort();
    for q in 0..u.nodes() {
        let before = t.props[p as usize + q].clone();
        t2.props[p as usize + q] = u.props[q]
            .iter()
            .enumerate()
            .map(|(i, (k, pr))| {
                // The nested root keeps what this template already knows
                // of it where the nested template has a slot.
                if q == 0 && *pr == Prop::Slot && before[i].1 != Prop::Slot {
                    return before[i].clone();
                }
                let pr = match pr {
                    Prop::Derived {
                        base,
                        k,
                        full,
                        suffix,
                    } => Prop::Derived {
                        base: base + p,
                        k: *k,
                        full: *full,
                        suffix: suffix.clone(),
                    },
                    x => x.clone(),
                };
                (k.clone(), pr)
            })
            .collect();
    }
    let ft = edge_starts(t);
    let fu = edge_starts(u);
    for (j, e) in u.edges.iter().enumerate() {
        let tj = ft[(e.src + p) as usize] + (j as u32 - fu[e.src as usize]);
        t2.edges[tj as usize].props = e.props.clone();
    }
    t2.slots = definition(g, names, &t2, all, "T", &|_| "T".into()).1;
    t2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forest_state_spills_past_its_budget_with_the_same_hashes() {
        let mut g = Graph::memory();
        let mut parents = Vec::new();
        for i in 0..4_000u64 {
            let id = g
                .add_node(
                    &[["A", "B"][(i % 2) as usize].to_string()],
                    vec![("i".into(), Value::Int((i % 5) as i64))],
                )
                .unwrap();
            if i > 0 {
                let p: u64 = parents[(i as usize * 7 + 3) % parents.len()];
                g.add_edge(p, id, "H", vec![]).unwrap();
            }
            parents.push(id);
        }
        let a = Args(vec![(
            "types".into(),
            Value::List(vec![Value::Text("H".into())]),
        )]);
        let h = Hier::from(&g, &a).unwrap();
        let mut free = Forest::build(&g, &h).unwrap();
        assert!(!free.ids.is_spilled() && !free.shape.is_spilled());
        // 16 KiB for 4,000 nodes of 56 bytes: the per-node state spills.
        let mut tight = crate::ooc::with_budget(16 << 10, g.temp_dir(), || {
            let f = Forest::build(&g, &h).unwrap();
            assert!(f.ids.is_spilled() && f.shape.is_spilled() && f.content.is_spilled());
            f
        });
        for v in 0..free.len {
            assert_eq!(free.shape.get(v), tight.shape.get(v));
            assert_eq!(free.content.get(v), tight.content.get(v));
            assert_eq!(free.size.get(v), tight.size.get(v));
            assert_eq!(free.owner.get(v), tight.owner.get(v));
        }
        // The forest under one root alone is the same there.
        let root = free.ids.get(0);
        let mut local = Forest::local(&g, &h, root, 100_000).unwrap();
        assert_eq!(local.shape_of(root), free.shape_of(root));
    }
}
