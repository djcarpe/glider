//! Graph algorithms, written once against [`Adjacency`] and run either over
//! an in-memory [`Csr`] projection (fast) or over the page store itself
//! (any size). Per-node state lives in [`StateVec`]s, which spill to disk
//! when a memory budget is set and they do not fit it, so the same code
//! runs out of core with identical results.
//!
//! Everything is iterative: no recursion, so a million-node chain won't blow
//! the stack. Indices are dense positions, not node ids; a node's position
//! is its rank in ascending id order, and its neighbours come in the same
//! order in every view.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};

use crate::graph::Csr;
use crate::ooc::{SpillQueue, SpillStack, StateVec};

/// A graph as the algorithms see it: dense positions `0..len()`, and each
/// position's neighbours, in a fixed order.
pub trait Adjacency {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn degree(&self, v: usize) -> usize;
    /// Visit `v`'s neighbours in order: (position, weight, edge id).
    fn each(&self, v: usize, f: &mut dyn FnMut(usize, f64, u64));
    /// `v`'s neighbour positions, in order.
    fn targets(&self, v: usize) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.degree(v));
        self.each(v, &mut |t, _, _| out.push(t));
        out
    }
    fn has_negative_weights(&self) -> bool {
        (0..self.len()).any(|v| {
            let mut neg = false;
            self.each(v, &mut |_, w, _| neg |= w < 0.0);
            neg
        })
    }
}

impl Adjacency for Csr {
    fn len(&self) -> usize {
        Csr::len(self)
    }
    #[inline]
    fn degree(&self, v: usize) -> usize {
        Csr::degree(self, v)
    }
    #[inline]
    fn each(&self, v: usize, f: &mut dyn FnMut(usize, f64, u64)) {
        for i in self.range(v) {
            f(self.adj[i] as usize, self.weights[i], self.eids[i]);
        }
    }
    fn targets(&self, v: usize) -> Vec<usize> {
        self.out(v).iter().map(|t| *t as usize).collect()
    }
    fn has_negative_weights(&self) -> bool {
        self.weights.iter().any(|w| *w < 0.0)
    }
}

/// f64 ordered for use in a heap. Smaller comes out first (min-heap via Reverse).
#[derive(PartialEq)]
struct Weighted(f64, usize);

impl Eq for Weighted {}
impl PartialOrd for Weighted {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Weighted {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed: BinaryHeap is a max-heap, we want the smallest distance.
        other
            .0
            .total_cmp(&self.0)
            .then_with(|| other.1.cmp(&self.1))
    }
}

/// Neighbours of `v` other than itself, sorted and deduplicated: the
/// undirected, simple-graph view some algorithms need.
fn simple_nbrs<A: Adjacency + ?Sized>(a: &A, v: usize) -> Vec<usize> {
    let mut list: Vec<usize> = a.targets(v).into_iter().filter(|t| *t != v).collect();
    list.sort_unstable();
    list.dedup();
    list
}

// ----------------------------------------------------------------- traversal

pub struct Visit {
    pub node: usize,
    pub depth: u32,
    pub parent: Option<usize>,
}

/// Breadth-first from `source`, handing each visit to `f` in order until
/// it returns false.
pub fn bfs_each<A: Adjacency + ?Sized>(
    a: &A,
    source: usize,
    max_depth: Option<u32>,
    f: &mut dyn FnMut(Visit) -> bool,
) {
    let n = a.len();
    if source >= n {
        return;
    }
    let mut seen = StateVec::new(n, false);
    let mut q: SpillQueue<(u64, u32, u64)> = SpillQueue::new();
    seen.set(source, true);
    q.push_back((source as u64, 0, NO_PARENT));
    while let Some((v, d, parent)) = q.pop_front() {
        let v = v as usize;
        let visit = Visit {
            node: v,
            depth: d,
            parent: (parent != NO_PARENT).then_some(parent as usize),
        };
        if !f(visit) {
            return;
        }
        if let Some(limit) = max_depth {
            if d >= limit {
                continue;
            }
        }
        a.each(v, &mut |t, _, _| {
            if !seen.get(t) {
                seen.set(t, true);
                q.push_back((t as u64, d + 1, v as u64));
            }
        });
    }
}

pub fn bfs<A: Adjacency + ?Sized>(a: &A, source: usize, max_depth: Option<u32>) -> Vec<Visit> {
    let mut out = Vec::new();
    bfs_each(a, source, max_depth, &mut |v| {
        out.push(v);
        true
    });
    out
}

/// Depth-first from `source` (neighbours in order), handing each visit to
/// `f` until it returns false.
pub fn dfs_each<A: Adjacency + ?Sized>(
    a: &A,
    source: usize,
    max_depth: Option<u32>,
    f: &mut dyn FnMut(Visit) -> bool,
) {
    let n = a.len();
    if source >= n {
        return;
    }
    let mut seen = StateVec::new(n, false);
    let mut stack: SpillStack<(u64, u32, u64)> = SpillStack::new();
    stack.push((source as u64, 0, NO_PARENT));
    while let Some((v, d, parent)) = stack.pop() {
        let v = v as usize;
        if seen.get(v) {
            continue;
        }
        seen.set(v, true);
        let visit = Visit {
            node: v,
            depth: d,
            parent: (parent != NO_PARENT).then_some(parent as usize),
        };
        if !f(visit) {
            return;
        }
        if let Some(limit) = max_depth {
            if d >= limit {
                continue;
            }
        }
        // Reverse so the first neighbour is explored first.
        for t in a.targets(v).into_iter().rev() {
            if !seen.get(t) {
                stack.push((t as u64, d + 1, v as u64));
            }
        }
    }
}

pub fn dfs<A: Adjacency + ?Sized>(a: &A, source: usize, max_depth: Option<u32>) -> Vec<Visit> {
    let mut out = Vec::new();
    dfs_each(a, source, max_depth, &mut |v| {
        out.push(v);
        true
    });
    out
}

/// Every node within `depth` hops of `source`, excluding the source.
pub fn k_hop<A: Adjacency + ?Sized>(a: &A, source: usize, depth: u32) -> Vec<(usize, u32)> {
    let mut out = Vec::new();
    bfs_each(a, source, Some(depth), &mut |v| {
        if v.depth > 0 {
            out.push((v.node, v.depth));
        }
        true
    });
    out
}

// ------------------------------------------------------------ shortest paths

pub struct Paths {
    pub dist: StateVec<f64>,
    pub parent: StateVec<u64>,
}

pub const NO_PARENT: u64 = u64::MAX;

impl Paths {
    fn new(n: usize) -> Paths {
        Paths {
            dist: StateVec::new(n, f64::INFINITY),
            parent: StateVec::new(n, NO_PARENT),
        }
    }

    pub fn path_to(&mut self, target: usize) -> Option<Vec<usize>> {
        if target >= self.dist.len() || !self.dist.get(target).is_finite() {
            return None;
        }
        let mut path = vec![target];
        let mut cur = target;
        while self.parent.get(cur) != NO_PARENT {
            cur = self.parent.get(cur) as usize;
            path.push(cur);
            if path.len() > self.dist.len() {
                return None; // defensive: cycle in parent chain
            }
        }
        path.reverse();
        Some(path)
    }
}

/// Unweighted single-source shortest paths (hop count).
pub fn bfs_paths<A: Adjacency + ?Sized>(a: &A, source: usize) -> Paths {
    let n = a.len();
    let mut p = Paths::new(n);
    if source >= n {
        return p;
    }
    p.dist.set(source, 0.0);
    let mut q: SpillQueue<u64> = SpillQueue::new();
    q.push_back(source as u64);
    while let Some(v) = q.pop_front() {
        let v = v as usize;
        let dv = p.dist.get(v);
        a.each(v, &mut |t, _, _| {
            if !p.dist.get(t).is_finite() {
                p.dist.set(t, dv + 1.0);
                p.parent.set(t, v as u64);
                q.push_back(t as u64);
            }
        });
    }
    p
}

/// Weighted single-source shortest paths. Negative weights are rejected by the
/// caller; here they'd simply produce wrong answers, as with any Dijkstra.
pub fn dijkstra<A: Adjacency + ?Sized>(a: &A, source: usize) -> Paths {
    let n = a.len();
    let mut p = Paths::new(n);
    if source >= n {
        return p;
    }
    let mut heap = BinaryHeap::new();
    p.dist.set(source, 0.0);
    heap.push(Weighted(0.0, source));
    while let Some(Weighted(d, v)) = heap.pop() {
        if d > p.dist.get(v) {
            continue; // stale entry
        }
        a.each(v, &mut |t, w, _| {
            let nd = d + w;
            if nd < p.dist.get(t) {
                p.dist.set(t, nd);
                p.parent.set(t, v as u64);
                heap.push(Weighted(nd, t));
            }
        });
    }
    p
}

pub fn has_negative_weights<A: Adjacency + ?Sized>(a: &A) -> bool {
    a.has_negative_weights()
}

/// A* with a caller-supplied admissible heuristic. Falls back to Dijkstra
/// behaviour when the heuristic is zero everywhere.
pub fn astar<A: Adjacency + ?Sized, H: Fn(usize) -> f64>(
    a: &A,
    source: usize,
    target: usize,
    h: H,
) -> Option<Vec<usize>> {
    let n = a.len();
    if source >= n || target >= n {
        return None;
    }
    let mut p = Paths::new(n);
    let mut heap = BinaryHeap::new();
    p.dist.set(source, 0.0);
    heap.push(Weighted(h(source), source));
    while let Some(Weighted(_, v)) = heap.pop() {
        if v == target {
            break;
        }
        let gv = p.dist.get(v);
        a.each(v, &mut |t, w, _| {
            let ng = gv + w;
            if ng < p.dist.get(t) {
                p.dist.set(t, ng);
                p.parent.set(t, v as u64);
                heap.push(Weighted(ng + h(t), t));
            }
        });
    }
    p.path_to(target)
}

// -------------------------------------------------------------- centralities

pub struct PageRank {
    pub scores: StateVec<f64>,
    pub iterations: u32,
    pub delta: f64,
}

pub fn pagerank<A: Adjacency + ?Sized>(a: &A, damping: f64, max_iter: u32, tolerance: f64) -> PageRank {
    let n = a.len();
    if n == 0 {
        return PageRank {
            scores: StateVec::from_vec(Vec::new()),
            iterations: 0,
            delta: 0.0,
        };
    }
    let base = 1.0 / n as f64;
    let mut rank = StateVec::new(n, base);
    let mut next = StateVec::new(n, 0.0f64);
    // Out-degrees don't change between iterations: count them once.
    let mut deg = StateVec::new(n, 0u64);
    for v in 0..n {
        deg.set(v, a.degree(v) as u64);
    }
    let mut iterations = 0;
    let mut delta = 0.0;

    for it in 0..max_iter {
        iterations = it + 1;
        // Dangling nodes redistribute their mass evenly.
        let mut dangling = 0.0;
        for v in 0..n {
            if deg.get(v) == 0 {
                dangling += rank.get(v);
            }
        }
        let leak = damping * dangling * base;
        next.fill((1.0 - damping) * base + leak);
        for v in 0..n {
            let d = deg.get(v);
            if d == 0 {
                continue;
            }
            let share = damping * rank.get(v) / d as f64;
            a.each(v, &mut |t, _, _| {
                let x = next.get(t);
                next.set(t, x + share);
            });
        }
        delta = 0.0;
        for v in 0..n {
            delta += (rank.get(v) - next.get(v)).abs();
        }
        rank.swap(&mut next);
        if delta < tolerance {
            break;
        }
    }

    PageRank {
        scores: rank,
        iterations,
        delta,
    }
}

/// Brandes' algorithm, unweighted. O(n*m) — fine for graphs up to ~1e5 edges,
/// slow above that; sample with `sources` to approximate.
///
/// `rev` is the reverse of `a` (the same view with every edge flipped): the
/// backward pass walks a node's predecessors through it instead of keeping
/// per-node predecessor lists, so memory stays O(n).
pub fn betweenness_with<A: Adjacency + ?Sized, R: Adjacency + ?Sized>(
    a: &A,
    rev: &R,
    sources: Option<&[usize]>,
    normalize: bool,
) -> StateVec<f64> {
    let n = a.len();
    let mut score = StateVec::new(n, 0.0f64);
    let mut sigma = StateVec::new(n, 0.0f64);
    let mut dist = StateVec::new(n, -1i64);
    let mut delta = StateVec::new(n, 0.0f64);
    let mut order = StateVec::new(n, 0u64);

    let mut run = |s: usize| {
        if s >= n {
            return;
        }
        sigma.fill(0.0);
        dist.fill(-1);
        delta.fill(0.0);
        let mut len = 0usize;
        let mut q: SpillQueue<u64> = SpillQueue::new();
        sigma.set(s, 1.0);
        dist.set(s, 0);
        q.push_back(s as u64);
        while let Some(v) = q.pop_front() {
            let v = v as usize;
            order.set(len, v as u64);
            len += 1;
            let (dv, sv) = (dist.get(v), sigma.get(v));
            a.each(v, &mut |t, _, _| {
                if dist.get(t) < 0 {
                    dist.set(t, dv + 1);
                    q.push_back(t as u64);
                }
                if dist.get(t) == dv + 1 {
                    let x = sigma.get(t);
                    sigma.set(t, x + sv);
                }
            });
        }
        for i in (0..len).rev() {
            let w = order.get(i) as usize;
            let coeff = (1.0 + delta.get(w)) / sigma.get(w);
            let dw = dist.get(w);
            // w's predecessors: nodes one level up with an edge into w, once
            // per such edge, as the forward pass counted them.
            rev.each(w, &mut |v, _, _| {
                if dist.get(v) == dw - 1 && dw > 0 {
                    let x = delta.get(v);
                    delta.set(v, x + sigma.get(v) * coeff);
                }
            });
            if w != s {
                let x = score.get(w);
                score.set(w, x + delta.get(w));
            }
        }
    };
    match sources {
        Some(srcs) => {
            for &s in srcs {
                run(s);
            }
        }
        None => {
            for s in 0..n {
                run(s);
            }
        }
    }

    if normalize && n > 2 {
        let scale = 1.0 / ((n - 1) * (n - 2)) as f64;
        for v in 0..n {
            let x = score.get(v);
            score.set(v, x * scale);
        }
    }
    score
}

/// Brandes over an in-memory projection (the reverse is built here).
pub fn betweenness(csr: &Csr, sources: Option<&[usize]>, normalize: bool) -> StateVec<f64> {
    let rev = csr.reversed();
    betweenness_with(csr, &rev, sources, normalize)
}

/// Closeness centrality: inverse of mean distance to reachable nodes, scaled by
/// the reachable fraction (Wasserman-Faust), so disconnected graphs behave.
pub fn closeness<A: Adjacency + ?Sized>(a: &A, weighted: bool) -> StateVec<f64> {
    let n = a.len();
    let mut out = StateVec::new(n, 0.0f64);
    for s in 0..n {
        let mut paths = if weighted {
            dijkstra(a, s)
        } else {
            bfs_paths(a, s)
        };
        let mut total = 0.0;
        let mut reached = 0usize;
        for v in 0..n {
            let d = paths.dist.get(v);
            if v != s && d.is_finite() {
                total += d;
                reached += 1;
            }
        }
        out.set(
            s,
            if reached > 0 && total > 0.0 {
                (reached as f64 / total) * (reached as f64 / (n.saturating_sub(1)).max(1) as f64)
            } else {
                0.0
            },
        );
    }
    out
}

pub fn degree_centrality<A: Adjacency + ?Sized>(a: &A) -> StateVec<f64> {
    let n = a.len();
    let denom = (n.saturating_sub(1)).max(1) as f64;
    let mut out = StateVec::new(n, 0.0f64);
    for v in 0..n {
        out.set(v, a.degree(v) as f64 / denom);
    }
    out
}

// ----------------------------------------------------------------- structure

pub const NONE: u64 = u64::MAX;

/// Connected components over the given view. Pass a `Dir::Both` view for
/// weakly connected components of a directed graph.
pub fn components<A: Adjacency + ?Sized>(a: &A) -> (StateVec<u64>, u64) {
    let n = a.len();
    let mut comp = StateVec::new(n, NONE);
    let mut count = 0u64;
    let mut q: SpillQueue<u64> = SpillQueue::new();
    for s in 0..n {
        if comp.get(s) != NONE {
            continue;
        }
        comp.set(s, count);
        q.push_back(s as u64);
        while let Some(v) = q.pop_front() {
            a.each(v as usize, &mut |t, _, _| {
                if comp.get(t) == NONE {
                    comp.set(t, count);
                    q.push_back(t as u64);
                }
            });
        }
        count += 1;
    }
    (comp, count)
}

/// The call stack of an iterative DFS: (node, next neighbour) frames on a
/// spilling stack, with the neighbour lists of the topmost frames cached
/// within a byte limit and re-read for frames below it.
struct DfsFrames {
    frames: SpillStack<(u64, u64)>,
    /// Neighbour lists of the topmost `cache.len()` frames, oldest first.
    cache: VecDeque<Vec<usize>>,
    cache_bytes: usize,
    limit: usize,
}

impl DfsFrames {
    fn new() -> DfsFrames {
        let limit = match crate::ooc::budget_left() {
            None => usize::MAX,
            Some(b) => ((b / 4) as usize).clamp(4 << 10, 64 << 20),
        };
        DfsFrames {
            frames: SpillStack::new(),
            cache: VecDeque::new(),
            cache_bytes: 0,
            limit,
        }
    }

    fn push<A: Adjacency + ?Sized>(&mut self, a: &A, v: usize) {
        self.frames.push((v as u64, 0));
        self.cache_in(a.targets(v));
    }

    fn cache_in(&mut self, nbrs: Vec<usize>) {
        self.cache_bytes += nbrs.len() * 8;
        self.cache.push_back(nbrs);
        while self.cache_bytes > self.limit && self.cache.len() > 1 {
            let old = self.cache.pop_front().unwrap();
            self.cache_bytes -= old.len() * 8;
        }
    }

    /// The top frame's node and its next unvisited neighbour, if any
    /// (advancing past it).
    fn next<A: Adjacency + ?Sized>(&mut self, a: &A) -> Option<(usize, Option<usize>)> {
        let (v, i) = self.frames.pop()?;
        if self.cache.is_empty() {
            self.cache_in(a.targets(v as usize));
        }
        let nbrs = self.cache.back().unwrap();
        let w = nbrs.get(i as usize).copied();
        self.frames.push((v, if w.is_some() { i + 1 } else { i }));
        Some((v as usize, w))
    }

    /// Drop the top frame.
    fn pop(&mut self) {
        self.frames.pop();
        // The cache covers the topmost frames, so if anything is cached the
        // top frame is.
        if let Some(old) = self.cache.pop_back() {
            self.cache_bytes -= old.len() * 8;
        }
    }

    fn top(&mut self) -> Option<usize> {
        self.frames.last().map(|(v, _)| v as usize)
    }
}

/// Tarjan's strongly connected components, iterative.
pub fn strongly_connected<A: Adjacency + ?Sized>(a: &A) -> (StateVec<u64>, u64) {
    let n = a.len();
    let mut index = StateVec::new(n, NONE);
    let mut low = StateVec::new(n, 0u64);
    let mut on_stack = StateVec::new(n, false);
    let mut comp = StateVec::new(n, NONE);
    let mut stack: SpillStack<u64> = SpillStack::new();
    let mut next_index = 0u64;
    let mut count = 0u64;
    let mut call = DfsFrames::new();

    for root in 0..n {
        if index.get(root) != NONE {
            continue;
        }
        call.push(a, root);
        index.set(root, next_index);
        low.set(root, next_index);
        next_index += 1;
        stack.push(root as u64);
        on_stack.set(root, true);

        while let Some((v, next)) = call.next(a) {
            if let Some(w) = next {
                if index.get(w) == NONE {
                    index.set(w, next_index);
                    low.set(w, next_index);
                    next_index += 1;
                    stack.push(w as u64);
                    on_stack.set(w, true);
                    call.push(a, w);
                } else if on_stack.get(w) {
                    let l = low.get(v).min(index.get(w));
                    low.set(v, l);
                }
            } else {
                if low.get(v) == index.get(v) {
                    while let Some(w) = stack.pop() {
                        let w = w as usize;
                        on_stack.set(w, false);
                        comp.set(w, count);
                        if w == v {
                            break;
                        }
                    }
                    count += 1;
                }
                call.pop();
                if let Some(pv) = call.top() {
                    let l = low.get(pv).min(low.get(v));
                    low.set(pv, l);
                }
            }
        }
    }
    (comp, count)
}

/// Sorted, deduplicated neighbour lists, cached least-recently-used within
/// a byte limit (a quarter of the budget; unlimited without one).
struct NbrCache {
    lists: std::collections::HashMap<usize, (std::rc::Rc<Vec<usize>>, u64)>,
    order: std::collections::BTreeMap<u64, usize>,
    bytes: usize,
    limit: usize,
    tick: u64,
}

impl NbrCache {
    fn new() -> NbrCache {
        NbrCache {
            lists: std::collections::HashMap::new(),
            order: std::collections::BTreeMap::new(),
            bytes: 0,
            limit: crate::ooc::budget_left().map(|b| (b / 4) as usize).unwrap_or(usize::MAX),
            tick: 0,
        }
    }

    fn get<A: Adjacency + ?Sized>(&mut self, a: &A, v: usize) -> std::rc::Rc<Vec<usize>> {
        self.tick += 1;
        if let Some((list, t)) = self.lists.get_mut(&v) {
            self.order.remove(t);
            *t = self.tick;
            self.order.insert(self.tick, v);
            return list.clone();
        }
        let list = std::rc::Rc::new(simple_nbrs(a, v));
        let size = list.len() * 8 + 64;
        while self.bytes + size > self.limit {
            let Some((_, old)) = self.order.pop_first() else { break };
            if let Some((l, _)) = self.lists.remove(&old) {
                self.bytes -= l.len() * 8 + 64;
            }
        }
        if self.bytes + size <= self.limit {
            self.bytes += size;
            self.lists.insert(v, (list.clone(), self.tick));
            self.order.insert(self.tick, v);
        }
        list
    }
}

/// Triangle count per node, on an undirected view. Returns (per-node, total).
/// Neighbour lists are fetched as needed, through a bounded cache, rather
/// than all held at once.
pub fn triangles<A: Adjacency + ?Sized>(a: &A) -> (StateVec<u64>, u64) {
    let n = a.len();
    let mut counts = StateVec::new(n, 0u64);
    let mut total = 0u64;
    let mut cache = NbrCache::new();
    for v in 0..n {
        let nv = cache.get(a, v);
        for &u in nv.iter() {
            if u <= v {
                continue;
            }
            let nu = cache.get(a, u);
            // Intersect the two sorted neighbour lists.
            let (mut i, mut j) = (0usize, 0usize);
            while i < nv.len() && j < nu.len() {
                match nv[i].cmp(&nu[j]) {
                    Ordering::Less => i += 1,
                    Ordering::Greater => j += 1,
                    Ordering::Equal => {
                        let w = nv[i];
                        if w > u {
                            for x in [v, u, w] {
                                let c = counts.get(x);
                                counts.set(x, c + 1);
                            }
                            total += 1;
                        }
                        i += 1;
                        j += 1;
                    }
                }
            }
        }
    }
    (counts, total)
}

/// Local clustering coefficient, given triangle counts from `triangles`.
pub fn clustering<A: Adjacency + ?Sized>(a: &A, tri: &mut StateVec<u64>) -> StateVec<f64> {
    let n = a.len();
    let mut out = StateVec::new(n, 0.0f64);
    for v in 0..n {
        let k = simple_nbrs(a, v).len() as f64;
        out.set(
            v,
            if k > 1.0 {
                2.0 * tri.get(v) as f64 / (k * (k - 1.0))
            } else {
                0.0
            },
        );
    }
    out
}

/// k-core decomposition (Batagelj-Zaveršnik): the core number of each node.
pub fn core_numbers<A: Adjacency + ?Sized>(a: &A) -> StateVec<u64> {
    let n = a.len();
    let mut deg = StateVec::new(n, 0u64);
    let mut max_deg = 0usize;
    for v in 0..n {
        let d = a.degree(v);
        deg.set(v, d as u64);
        max_deg = max_deg.max(d);
    }

    let mut bin = vec![0usize; max_deg + 2];
    for v in 0..n {
        bin[deg.get(v) as usize] += 1;
    }
    let mut start = 0usize;
    for b in bin.iter_mut() {
        let c = *b;
        *b = start;
        start += c;
    }
    let mut pos = StateVec::new(n, 0u64);
    let mut vert = StateVec::new(n, 0u64);
    for v in 0..n {
        let d = deg.get(v) as usize;
        pos.set(v, bin[d] as u64);
        vert.set(bin[d], v as u64);
        bin[d] += 1;
    }
    for d in (1..bin.len()).rev() {
        bin[d] = bin[d - 1];
    }
    bin[0] = 0;

    for i in 0..n {
        let v = vert.get(i) as usize;
        let mut seen = a.targets(v);
        seen.sort_unstable();
        seen.dedup();
        for u in seen {
            let (du, dv) = (deg.get(u), deg.get(v));
            if du > dv {
                let du = du as usize;
                let pu = pos.get(u) as usize;
                let pw = bin[du];
                let w = vert.get(pw) as usize;
                if u != w {
                    pos.set(u, pw as u64);
                    pos.set(w, pu as u64);
                    vert.set(pu, w as u64);
                    vert.set(pw, u as u64);
                }
                bin[du] += 1;
                deg.set(u, du as u64 - 1);
            }
        }
    }
    deg
}

/// Kahn's topological sort: the order as positions. `None` when the graph
/// has a cycle.
pub fn topological_sort<A: Adjacency + ?Sized>(a: &A) -> Option<StateVec<u64>> {
    let n = a.len();
    let mut indeg = StateVec::new(n, 0u64);
    for v in 0..n {
        a.each(v, &mut |t, _, _| {
            let x = indeg.get(t);
            indeg.set(t, x + 1);
        });
    }
    let mut q: SpillQueue<u64> = SpillQueue::new();
    for v in 0..n {
        if indeg.get(v) == 0 {
            q.push_back(v as u64);
        }
    }
    let mut order = StateVec::new(n, 0u64);
    let mut len = 0usize;
    while let Some(v) = q.pop_front() {
        order.set(len, v);
        len += 1;
        a.each(v as usize, &mut |t, _, _| {
            let x = indeg.get(t) - 1;
            indeg.set(t, x);
            if x == 0 {
                q.push_back(t as u64);
            }
        });
    }
    if len == n {
        Some(order)
    } else {
        None
    }
}

pub fn find_cycle<A: Adjacency + ?Sized>(a: &A) -> Option<Vec<usize>> {
    let n = a.len();
    // 0 = unvisited, 1 = on stack, 2 = done
    let mut state = StateVec::new(n, 0u8);
    let mut parent = StateVec::new(n, NO_PARENT);
    for root in 0..n {
        if state.get(root) != 0 {
            continue;
        }
        let mut call = DfsFrames::new();
        call.push(a, root);
        state.set(root, 1);
        while let Some((v, next)) = call.next(a) {
            match next {
                Some(w) => match state.get(w) {
                    0 => {
                        state.set(w, 1);
                        parent.set(w, v as u64);
                        call.push(a, w);
                    }
                    1 => {
                        // Walk back from v to w to recover the cycle.
                        let mut cycle = vec![w];
                        let mut cur = v;
                        while cur != w {
                            cycle.push(cur);
                            let p = parent.get(cur);
                            if p == NO_PARENT {
                                break;
                            }
                            cur = p as usize;
                        }
                        cycle.push(w);
                        cycle.reverse();
                        return Some(cycle);
                    }
                    _ => {}
                },
                None => {
                    state.set(v, 2);
                    call.pop();
                }
            }
        }
    }
    None
}

/// Label propagation communities. Deterministic: ties break toward the
/// smallest community id, and nodes are swept in index order.
pub fn label_propagation<A: Adjacency + ?Sized>(a: &A, max_iter: u32) -> (StateVec<u64>, u64) {
    let n = a.len();
    let mut labels = StateVec::new(n, 0u64);
    for v in 0..n {
        labels.set(v, v as u64);
    }
    for _ in 0..max_iter {
        let mut changed = false;
        for v in 0..n {
            if a.degree(v) == 0 {
                continue;
            }
            let mut tally: Vec<(u64, u32)> = Vec::new();
            a.each(v, &mut |t, _, _| {
                let l = labels.get(t);
                match tally.iter_mut().find(|(lab, _)| *lab == l) {
                    Some(slot) => slot.1 += 1,
                    None => tally.push((l, 1)),
                }
            });
            if let Some(&(best, _)) = tally
                .iter()
                .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            {
                if labels.get(v) != best {
                    labels.set(v, best);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Renumber densely, in order of first appearance. Labels are positions,
    // so the map is an array.
    let mut remap = StateVec::new(n, NONE);
    let mut count = 0u64;
    for v in 0..n {
        let l = labels.get(v) as usize;
        let mut m = remap.get(l);
        if m == NONE {
            m = count;
            remap.set(l, m);
            count += 1;
        }
        labels.set(v, m);
    }
    (labels, count)
}

/// Kruskal minimum spanning forest over an undirected weighted view.
/// Returns the chosen edge ids and the total weight. Candidate edges are
/// sorted externally, so their number is not bounded by memory.
pub fn minimum_spanning_forest<A: Adjacency + ?Sized>(a: &A) -> (Vec<u64>, f64) {
    use crate::storage::extsort::Sorter;
    let n = a.len();
    let dir = crate::ooc::temp_dir();
    let mut sorter = Sorter::new(&dir, "mst", 64 << 20).expect("mst sort");
    let mut seq = 0u64;
    for v in 0..n {
        a.each(v, &mut |t, w, eid| {
            if v < t {
                // Key: weight in total order, then arrival (a stable sort).
                let bits = w.to_bits();
                let k = if bits >> 63 == 1 { !bits } else { bits ^ (1 << 63) };
                let mut key = Vec::with_capacity(16);
                key.extend_from_slice(&k.to_be_bytes());
                key.extend_from_slice(&seq.to_be_bytes());
                let mut val = Vec::with_capacity(24);
                val.extend_from_slice(&(v as u64).to_le_bytes());
                val.extend_from_slice(&(t as u64).to_le_bytes());
                val.extend_from_slice(&eid.to_le_bytes());
                sorter.push(&key, &val).expect("mst sort");
                seq += 1;
            }
        });
    }
    let mut parent = StateVec::new(n, 0u64);
    for v in 0..n {
        parent.set(v, v as u64);
    }
    fn find(parent: &mut StateVec<u64>, mut x: usize) -> usize {
        loop {
            let p = parent.get(x) as usize;
            if p == x {
                return x;
            }
            let gp = parent.get(p);
            parent.set(x, gp);
            x = gp as usize;
        }
    }

    let mut chosen = Vec::new();
    let mut total = 0.0;
    let mut it = sorter.finish().expect("mst sort");
    while let Some((key, val)) = it.next().expect("mst sort") {
        let k = u64::from_be_bytes(key[..8].try_into().unwrap());
        let bits = if k >> 63 == 1 { k ^ (1 << 63) } else { !k };
        let w = f64::from_bits(bits);
        let v = u64::from_le_bytes(val[0..8].try_into().unwrap()) as usize;
        let t = u64::from_le_bytes(val[8..16].try_into().unwrap()) as usize;
        let eid = u64::from_le_bytes(val[16..24].try_into().unwrap());
        let (ra, rb) = (find(&mut parent, v), find(&mut parent, t));
        if ra != rb {
            parent.set(ra, rb as u64);
            chosen.push(eid);
            total += w;
        }
    }
    (chosen, total)
}
