//! Structural utilities, in the shape of Erlang's `digraph_utils`.
//!
//! `algo` answers *how important* and *how far*: PageRank, centrality,
//! shortest paths. This module answers *what shape is this*: components,
//! orderings, reachability sets, acyclicity, trees. Same split the BEAM makes
//! between `digraph` and `digraph_utils`, and for the same reason — these are
//! the operations you reach for when the graph is a dependency graph, a
//! workflow, a call graph or a schema, rather than a network to rank.
//!
//! Everything here is read-only and deterministic: vertex lists come back
//! sorted, component lists sorted by their smallest member. Every traversal is
//! iterative, so a million-vertex chain will not blow the stack.
//!
//! ```no_run
//! use glider::{Graph, utils::Digraph};
//! # let g = Graph::memory();
//! let d = Digraph::new(&g);
//! if !d.is_acyclic() {
//!     for scc in d.cyclic_strong_components() {
//!         println!("cycle through {:?}", scc);
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::collections::HashSet;

use crate::algo;
use crate::graph::{Csr, Dir, Graph, Result};
use crate::value::Value;

/// A directed view of a graph, optionally restricted to one edge type.
///
/// Holds an out-CSR and an in-CSR over the same dense index space, so every
/// operation below is O(V+E) with no rebuild between calls. Build it once and
/// ask it many questions.
pub struct Digraph<'a> {
    pub graph: &'a Graph,
    out: Csr,
    inc: Csr,
}

impl<'a> Digraph<'a> {
    pub fn new(graph: &'a Graph) -> Digraph<'a> {
        Digraph::of_type(graph, None)
    }

    /// Restrict the view to one edge type. An unknown type yields an edgeless
    /// view rather than an error — the same answer, arrived at honestly.
    pub fn of_type(graph: &'a Graph, etype: Option<&str>) -> Digraph<'a> {
        let t = etype.map(|t| graph.strings.lookup(t).unwrap_or(u32::MAX));
        Digraph {
            graph,
            out: graph.csr(Dir::Out, t, None),
            inc: graph.csr(Dir::In, t, None),
        }
    }

    /// Every vertex, ascending. Dense index `i` in this module always means
    /// `vertices()[i]`.
    pub fn vertices(&self) -> &[u64] {
        &self.out.ids
    }

    pub fn vertex_count(&self) -> usize {
        self.out.len()
    }

    /// Edges in this view. Parallel edges count separately, as they do in
    /// `digraph`.
    pub fn edge_count(&self) -> usize {
        self.out.edge_count()
    }

    fn idx(&self, id: u64) -> Option<usize> {
        self.out.pos.get(&id).map(|p| *p as usize)
    }

    fn succ(&self, i: usize) -> &[u32] {
        let r = self.out.range(i);
        &self.out.adj[r]
    }

    fn pred(&self, i: usize) -> &[u32] {
        let r = self.inc.range(i);
        &self.inc.adj[r]
    }

    pub fn out_degree(&self, id: u64) -> usize {
        self.idx(id).map(|i| self.succ(i).len()).unwrap_or(0)
    }

    pub fn in_degree(&self, id: u64) -> usize {
        self.idx(id).map(|i| self.pred(i).len()).unwrap_or(0)
    }

    fn to_ids(&self, idx: impl IntoIterator<Item = usize>) -> Vec<u64> {
        let mut v: Vec<u64> = idx.into_iter().map(|i| self.out.ids[i]).collect();
        v.sort_unstable();
        v
    }

    // ------------------------------------------------------------ components

    /// Weakly connected components: edge direction ignored.
    pub fn components(&self) -> Vec<Vec<u64>> {
        let n = self.vertex_count();
        let mut seen = vec![false; n];
        let mut out = Vec::new();
        let mut stack = Vec::new();
        for start in 0..n {
            if seen[start] {
                continue;
            }
            seen[start] = true;
            stack.push(start);
            let mut group = Vec::new();
            while let Some(v) = stack.pop() {
                group.push(v);
                for &w in self.succ(v).iter().chain(self.pred(v).iter()) {
                    let w = w as usize;
                    if !seen[w] {
                        seen[w] = true;
                        stack.push(w);
                    }
                }
            }
            out.push(self.to_ids(group));
        }
        out.sort();
        out
    }

    /// Strongly connected components, including the singletons.
    pub fn strong_components(&self) -> Vec<Vec<u64>> {
        let (comp, count) = algo::strongly_connected(&self.out);
        let mut groups: Vec<Vec<usize>> = vec![Vec::new(); count as usize];
        for (i, c) in comp.iter().enumerate() {
            groups[*c as usize].push(i);
        }
        let mut out: Vec<Vec<u64>> = groups.into_iter().map(|g| self.to_ids(g)).collect();
        out.sort();
        out
    }

    /// Strong components that actually contain a cycle: more than one vertex,
    /// or a single vertex with a loop on it. These are exactly the components
    /// that make the graph non-acyclic, so this is the useful one when you are
    /// hunting a circular dependency.
    pub fn cyclic_strong_components(&self) -> Vec<Vec<u64>> {
        self.strong_components()
            .into_iter()
            .filter(|c| c.len() > 1 || self.has_loop(c[0]))
            .collect()
    }

    fn has_loop(&self, id: u64) -> bool {
        match self.idx(id) {
            Some(i) => self.succ(i).iter().any(|&w| w as usize == i),
            None => false,
        }
    }

    /// Vertices with an edge to themselves.
    pub fn loop_vertices(&self) -> Vec<u64> {
        (0..self.vertex_count())
            .filter(|&i| self.succ(i).iter().any(|&w| w as usize == i))
            .map(|i| self.out.ids[i])
            .collect()
    }

    /// The condensation: one vertex per strong component, one edge per pair of
    /// distinct components joined by any edge. Always acyclic — this is how
    /// you get a DAG out of a graph that has cycles in it.
    ///
    /// Returns a fresh in-memory graph. Each vertex is labelled `:Component`
    /// and carries `members` (the original ids) and `size`; edges are
    /// `:REACHES`.
    pub fn condensation(&self) -> Result<Graph> {
        let comps = self.strong_components();
        let mut comp_of: HashMap<u64, usize> = HashMap::new();
        for (ci, members) in comps.iter().enumerate() {
            for m in members {
                comp_of.insert(*m, ci);
            }
        }

        let mut cg = Graph::memory();
        let mut node_ids = Vec::with_capacity(comps.len());
        for members in &comps {
            let id = cg.add_node(
                &["Component".to_string()],
                vec![
                    (
                        "members".to_string(),
                        Value::List(members.iter().map(|m| Value::Int(*m as i64)).collect()),
                    ),
                    ("size".to_string(), Value::Int(members.len() as i64)),
                ],
            )?;
            node_ids.push(id);
        }

        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        for (i, &from) in self.out.ids.iter().enumerate() {
            let a = comp_of[&from];
            for &w in self.succ(i) {
                let b = comp_of[&self.out.ids[w as usize]];
                if a != b && seen.insert((a, b)) {
                    cg.add_edge(node_ids[a], node_ids[b], "REACHES", vec![])?;
                }
            }
        }
        Ok(cg)
    }

    // ------------------------------------------------------------- orderings

    /// Depth-first preorder over the whole graph, starting from each vertex in
    /// ascending id order.
    pub fn preorder(&self) -> Vec<u64> {
        self.dfs_order(true)
    }

    /// Depth-first postorder. Reverse it and you have a topological order,
    /// provided the graph is acyclic.
    pub fn postorder(&self) -> Vec<u64> {
        self.dfs_order(false)
    }

    fn dfs_order(&self, pre: bool) -> Vec<u64> {
        let n = self.vertex_count();
        let mut seen = vec![false; n];
        let mut order = Vec::with_capacity(n);
        // (vertex, how many of its successors we have already walked)
        let mut stack: Vec<(usize, usize)> = Vec::new();
        for start in 0..n {
            if seen[start] {
                continue;
            }
            seen[start] = true;
            if pre {
                order.push(self.out.ids[start]);
            }
            stack.push((start, 0));
            while let Some((v, k)) = stack.pop() {
                let succ = self.succ(v);
                if k < succ.len() {
                    stack.push((v, k + 1));
                    let w = succ[k] as usize;
                    if !seen[w] {
                        seen[w] = true;
                        if pre {
                            order.push(self.out.ids[w]);
                        }
                        stack.push((w, 0));
                    }
                } else if !pre {
                    order.push(self.out.ids[v]);
                }
            }
        }
        order
    }

    /// Topological order, or `None` if the graph has a cycle.
    pub fn topsort(&self) -> Option<Vec<u64>> {
        algo::topological_sort(&self.out).map(|idx| idx.into_iter().map(|i| self.out.ids[i]).collect())
    }

    pub fn is_acyclic(&self) -> bool {
        algo::topological_sort(&self.out).is_some()
    }

    // ---------------------------------------------------------- reachability

    /// Every vertex reachable from `from` by a path of length **zero** or
    /// more, so the sources themselves are always included.
    pub fn reachable(&self, from: &[u64]) -> Vec<u64> {
        self.walk(from, Dir::Out, true)
    }

    /// Every vertex reachable from `from` by a path of length **one** or more.
    /// A source appears only if it is genuinely reachable — i.e. it sits on a
    /// cycle or has a loop.
    pub fn reachable_neighbours(&self, from: &[u64]) -> Vec<u64> {
        self.walk(from, Dir::Out, false)
    }

    /// Every vertex that can reach `to`, by a path of length zero or more.
    pub fn reaching(&self, to: &[u64]) -> Vec<u64> {
        self.walk(to, Dir::In, true)
    }

    /// Every vertex that can reach `to` by a path of length one or more.
    pub fn reaching_neighbours(&self, to: &[u64]) -> Vec<u64> {
        self.walk(to, Dir::In, false)
    }

    fn walk(&self, seeds: &[u64], dir: Dir, reflexive: bool) -> Vec<u64> {
        let n = self.vertex_count();
        let mut seen = vec![false; n];
        let mut stack: Vec<usize> = Vec::new();
        let step = |i: usize| -> &[u32] {
            match dir {
                Dir::In => self.pred(i),
                _ => self.succ(i),
            }
        };

        for s in seeds {
            let Some(i) = self.idx(*s) else { continue };
            if reflexive {
                if !seen[i] {
                    seen[i] = true;
                    stack.push(i);
                }
            } else {
                // Start one hop out: the seed itself is only "reached" if some
                // path leads back to it.
                for &w in step(i) {
                    let w = w as usize;
                    if !seen[w] {
                        seen[w] = true;
                        stack.push(w);
                    }
                }
            }
        }

        let mut out = Vec::new();
        while let Some(v) = stack.pop() {
            out.push(v);
            for &w in step(v) {
                let w = w as usize;
                if !seen[w] {
                    seen[w] = true;
                    stack.push(w);
                }
            }
        }
        self.to_ids(out)
    }

    // ----------------------------------------------------------- shape tests

    /// Weakly connected with exactly `V - 1` edges.
    pub fn is_tree(&self) -> bool {
        let n = self.vertex_count();
        n > 0 && self.edge_count() == n - 1 && self.components().len() == 1
    }

    /// A tree with every edge pointing away from one root: `V - 1` edges, one
    /// vertex of in-degree 0, every other of in-degree exactly 1.
    pub fn is_arborescence(&self) -> bool {
        self.arborescence_root().is_some()
    }

    pub fn arborescence_root(&self) -> Option<u64> {
        let n = self.vertex_count();
        if n == 0 || self.edge_count() != n - 1 {
            return None;
        }
        let mut root = None;
        for i in 0..n {
            match self.pred(i).len() {
                1 => {}
                0 if root.is_none() => root = Some(self.out.ids[i]),
                _ => return None,
            }
        }
        root
    }

    // ------------------------------------------------- beyond digraph_utils

    /// Vertices nothing points at. The entry points of a dependency graph.
    pub fn roots(&self) -> Vec<u64> {
        (0..self.vertex_count())
            .filter(|&i| self.pred(i).is_empty())
            .map(|i| self.out.ids[i])
            .collect()
    }

    /// Vertices that point at nothing. The leaves.
    pub fn sinks(&self) -> Vec<u64> {
        (0..self.vertex_count())
            .filter(|&i| self.succ(i).is_empty())
            .map(|i| self.out.ids[i])
            .collect()
    }

    /// Vertices with no edges at all in this view.
    pub fn isolated(&self) -> Vec<u64> {
        (0..self.vertex_count())
            .filter(|&i| self.succ(i).is_empty() && self.pred(i).is_empty())
            .map(|i| self.out.ids[i])
            .collect()
    }

    /// The induced subgraph on `vertices`: those vertices, and every edge
    /// between two of them. Labels and properties are carried over; ids are
    /// not, so each vertex keeps its original id in the `_id` property.
    pub fn subgraph(&self, vertices: &[u64]) -> Result<Graph> {
        let keep: HashSet<u64> = vertices.iter().copied().collect();
        let mut sub = Graph::memory();
        let mut map: HashMap<u64, u64> = HashMap::new();

        let mut ordered: Vec<u64> = keep.iter().copied().collect();
        ordered.sort_unstable();

        for id in &ordered {
            let Some(node) = self.graph.node(*id) else {
                continue;
            };
            let labels: Vec<String> = node
                .labels
                .iter()
                .map(|l| self.graph.strings.name(*l).to_string())
                .collect();
            let mut props: Vec<(String, Value)> = node
                .props
                .iter()
                .map(|(k, v)| (self.graph.strings.name(*k).to_string(), v.clone()))
                .collect();
            props.push(("_id".to_string(), Value::Int(*id as i64)));
            map.insert(*id, sub.add_node(&labels, props)?);
        }

        for (i, from) in self.out.ids.iter().enumerate() {
            if !keep.contains(from) {
                continue;
            }
            let r = self.out.range(i);
            for (slot, &w) in self.out.adj[r.clone()].iter().enumerate() {
                let to = self.out.ids[w as usize];
                if !keep.contains(&to) {
                    continue;
                }
                let eid = self.out.eids[r.start + slot];
                let Some(edge) = self.graph.edge(eid) else {
                    continue;
                };
                let etype = self.graph.strings.name(edge.etype).to_string();
                let props: Vec<(String, Value)> = edge
                    .props
                    .iter()
                    .map(|(k, v)| (self.graph.strings.name(*k).to_string(), v.clone()))
                    .collect();
                sub.add_edge(map[from], map[&to], &etype, props)?;
            }
        }
        Ok(sub)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a -> b -> c -> a  (a cycle), d -> e (a stick), f (alone)
    fn fixture() -> (Graph, Vec<u64>) {
        let mut g = Graph::memory();
        let mut ids = Vec::new();
        for name in ["a", "b", "c", "d", "e", "f"] {
            ids.push(
                g.add_node(&["V".into()], vec![("name".into(), Value::Text(name.into()))])
                    .unwrap(),
            );
        }
        g.add_edge(ids[0], ids[1], "E", vec![]).unwrap();
        g.add_edge(ids[1], ids[2], "E", vec![]).unwrap();
        g.add_edge(ids[2], ids[0], "E", vec![]).unwrap();
        g.add_edge(ids[3], ids[4], "E", vec![]).unwrap();
        (g, ids)
    }

    #[test]
    fn components_are_weak_and_strong() {
        let (g, id) = fixture();
        let d = Digraph::new(&g);
        assert_eq!(
            d.components(),
            vec![vec![id[0], id[1], id[2]], vec![id[3], id[4]], vec![id[5]]]
        );
        assert_eq!(d.strong_components().len(), 4); // {a,b,c}, {d}, {e}, {f}
        assert_eq!(d.cyclic_strong_components(), vec![vec![id[0], id[1], id[2]]]);
        assert!(!d.is_acyclic());
        assert_eq!(d.topsort(), None);
    }

    #[test]
    fn reachability_is_reflexive_only_where_it_should_be() {
        let (g, id) = fixture();
        let d = Digraph::new(&g);
        // d -> e: d reaches itself only by a zero-length path.
        assert_eq!(d.reachable(&[id[3]]), vec![id[3], id[4]]);
        assert_eq!(d.reachable_neighbours(&[id[3]]), vec![id[4]]);
        // a is on a cycle, so it is its own neighbour.
        assert_eq!(
            d.reachable_neighbours(&[id[0]]),
            vec![id[0], id[1], id[2]]
        );
        assert_eq!(d.reaching(&[id[4]]), vec![id[3], id[4]]);
        assert_eq!(d.reaching_neighbours(&[id[4]]), vec![id[3]]);
        assert!(d.reachable(&[id[5]]) == vec![id[5]]);
        assert!(d.reachable_neighbours(&[id[5]]).is_empty());
    }

    #[test]
    fn condensation_is_a_dag() {
        let (g, _) = fixture();
        let d = Digraph::new(&g);
        let c = d.condensation().unwrap();
        assert_eq!(c.node_count(), 4);
        assert!(Digraph::new(&c).is_acyclic());
    }

    #[test]
    fn trees_and_arborescences() {
        let mut g = Graph::memory();
        let r = g.add_node(&["V".into()], vec![]).unwrap();
        let a = g.add_node(&["V".into()], vec![]).unwrap();
        let b = g.add_node(&["V".into()], vec![]).unwrap();
        g.add_edge(r, a, "E", vec![]).unwrap();
        g.add_edge(r, b, "E", vec![]).unwrap();
        let d = Digraph::new(&g);
        assert!(d.is_tree());
        assert!(d.is_arborescence());
        assert_eq!(d.arborescence_root(), Some(r));
        assert_eq!(d.roots(), vec![r]);
        assert_eq!(d.sinks(), vec![a, b]);
        drop(d);

        // Point both edges at the root instead: still a tree, no longer an
        // arborescence, because two vertices have in-degree 0.
        let mut g2 = Graph::memory();
        let r = g2.add_node(&["V".into()], vec![]).unwrap();
        let a = g2.add_node(&["V".into()], vec![]).unwrap();
        let b = g2.add_node(&["V".into()], vec![]).unwrap();
        g2.add_edge(a, r, "E", vec![]).unwrap();
        g2.add_edge(b, r, "E", vec![]).unwrap();
        let d2 = Digraph::new(&g2);
        assert!(d2.is_tree());
        assert!(!d2.is_arborescence());
    }

    #[test]
    fn orders_are_depth_first_and_iterative() {
        let mut g = Graph::memory();
        let mut prev = g.add_node(&["V".into()], vec![]).unwrap();
        let first = prev;
        for _ in 0..50_000 {
            let n = g.add_node(&["V".into()], vec![]).unwrap();
            g.add_edge(prev, n, "E", vec![]).unwrap();
            prev = n;
        }
        let d = Digraph::new(&g);
        let pre = d.preorder();
        let post = d.postorder();
        assert_eq!(pre.len(), 50_001);
        assert_eq!(pre[0], first);
        assert_eq!(post[post.len() - 1], first); // root finishes last
        assert_eq!(d.topsort().unwrap(), pre);
    }

    #[test]
    fn subgraph_is_induced() {
        let (g, id) = fixture();
        let d = Digraph::new(&g);
        let sub = d.subgraph(&[id[0], id[1]]).unwrap();
        assert_eq!(sub.node_count(), 2);
        assert_eq!(sub.edge_count(), 1); // a->b kept, b->c dropped
    }

    #[test]
    fn type_filter_narrows_the_view() {
        let mut g = Graph::memory();
        let a = g.add_node(&["V".into()], vec![]).unwrap();
        let b = g.add_node(&["V".into()], vec![]).unwrap();
        g.add_edge(a, b, "CALLS", vec![]).unwrap();
        g.add_edge(b, a, "MENTIONS", vec![]).unwrap();
        assert!(!Digraph::new(&g).is_acyclic());
        assert!(Digraph::of_type(&g, Some("CALLS")).is_acyclic());
        assert!(Digraph::of_type(&g, Some("NOPE")).isolated().len() == 2);
    }
}
