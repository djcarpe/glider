//! Graph algorithms. Everything here runs on the dense `Csr` view and is
//! written iteratively: no recursion, so a million-node chain won't blow the
//! stack. Indices are dense positions, not node ids; callers map back via
//! `csr.ids[i]`.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};

use crate::graph::Csr;

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
        other.0.total_cmp(&self.0).then_with(|| other.1.cmp(&self.1))
    }
}

// ----------------------------------------------------------------- traversal

pub struct Visit {
    pub node: usize,
    pub depth: u32,
    pub parent: Option<usize>,
}

pub fn bfs(csr: &Csr, source: usize, max_depth: Option<u32>) -> Vec<Visit> {
    let n = csr.len();
    let mut seen = vec![false; n];
    let mut out = Vec::new();
    if source >= n {
        return out;
    }
    let mut q = VecDeque::new();
    seen[source] = true;
    q.push_back((source, 0u32, None));
    while let Some((v, d, parent)) = q.pop_front() {
        out.push(Visit {
            node: v,
            depth: d,
            parent,
        });
        if let Some(limit) = max_depth {
            if d >= limit {
                continue;
            }
        }
        for &t in csr.out(v) {
            let t = t as usize;
            if !seen[t] {
                seen[t] = true;
                q.push_back((t, d + 1, Some(v)));
            }
        }
    }
    out
}

pub fn dfs(csr: &Csr, source: usize, max_depth: Option<u32>) -> Vec<Visit> {
    let n = csr.len();
    let mut seen = vec![false; n];
    let mut out = Vec::new();
    if source >= n {
        return out;
    }
    let mut stack = vec![(source, 0u32, None)];
    while let Some((v, d, parent)) = stack.pop() {
        if seen[v] {
            continue;
        }
        seen[v] = true;
        out.push(Visit {
            node: v,
            depth: d,
            parent,
        });
        if let Some(limit) = max_depth {
            if d >= limit {
                continue;
            }
        }
        // Reverse so the first neighbour is explored first.
        for &t in csr.out(v).iter().rev() {
            let t = t as usize;
            if !seen[t] {
                stack.push((t, d + 1, Some(v)));
            }
        }
    }
    out
}

/// Every node within `depth` hops of `source`, excluding the source.
pub fn k_hop(csr: &Csr, source: usize, depth: u32) -> Vec<(usize, u32)> {
    bfs(csr, source, Some(depth))
        .into_iter()
        .filter(|v| v.depth > 0)
        .map(|v| (v.node, v.depth))
        .collect()
}

// ------------------------------------------------------------ shortest paths

pub struct Paths {
    pub dist: Vec<f64>,
    pub parent: Vec<u32>,
}

pub const NO_PARENT: u32 = u32::MAX;

impl Paths {
    pub fn path_to(&self, target: usize) -> Option<Vec<usize>> {
        if target >= self.dist.len() || !self.dist[target].is_finite() {
            return None;
        }
        let mut path = vec![target];
        let mut cur = target;
        while self.parent[cur] != NO_PARENT {
            cur = self.parent[cur] as usize;
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
pub fn bfs_paths(csr: &Csr, source: usize) -> Paths {
    let n = csr.len();
    let mut dist = vec![f64::INFINITY; n];
    let mut parent = vec![NO_PARENT; n];
    if source >= n {
        return Paths { dist, parent };
    }
    dist[source] = 0.0;
    let mut q = VecDeque::new();
    q.push_back(source);
    while let Some(v) = q.pop_front() {
        for &t in csr.out(v) {
            let t = t as usize;
            if !dist[t].is_finite() {
                dist[t] = dist[v] + 1.0;
                parent[t] = v as u32;
                q.push_back(t);
            }
        }
    }
    Paths { dist, parent }
}

/// Weighted single-source shortest paths. Negative weights are rejected by the
/// caller; here they'd simply produce wrong answers, as with any Dijkstra.
pub fn dijkstra(csr: &Csr, source: usize) -> Paths {
    let n = csr.len();
    let mut dist = vec![f64::INFINITY; n];
    let mut parent = vec![NO_PARENT; n];
    if source >= n {
        return Paths { dist, parent };
    }
    let mut heap = BinaryHeap::new();
    dist[source] = 0.0;
    heap.push(Weighted(0.0, source));
    while let Some(Weighted(d, v)) = heap.pop() {
        if d > dist[v] {
            continue; // stale entry
        }
        for i in csr.range(v) {
            let t = csr.adj[i] as usize;
            let nd = d + csr.weights[i];
            if nd < dist[t] {
                dist[t] = nd;
                parent[t] = v as u32;
                heap.push(Weighted(nd, t));
            }
        }
    }
    Paths { dist, parent }
}

pub fn has_negative_weights(csr: &Csr) -> bool {
    csr.weights.iter().any(|w| *w < 0.0)
}

/// A* with a caller-supplied admissible heuristic. Falls back to Dijkstra
/// behaviour when the heuristic is zero everywhere.
pub fn astar<H: Fn(usize) -> f64>(csr: &Csr, source: usize, target: usize, h: H) -> Option<Vec<usize>> {
    let n = csr.len();
    if source >= n || target >= n {
        return None;
    }
    let mut g = vec![f64::INFINITY; n];
    let mut parent = vec![NO_PARENT; n];
    let mut heap = BinaryHeap::new();
    g[source] = 0.0;
    heap.push(Weighted(h(source), source));
    while let Some(Weighted(_, v)) = heap.pop() {
        if v == target {
            break;
        }
        for i in csr.range(v) {
            let t = csr.adj[i] as usize;
            let ng = g[v] + csr.weights[i];
            if ng < g[t] {
                g[t] = ng;
                parent[t] = v as u32;
                heap.push(Weighted(ng + h(t), t));
            }
        }
    }
    Paths { dist: g, parent }.path_to(target)
}

// -------------------------------------------------------------- centralities

pub struct PageRank {
    pub scores: Vec<f64>,
    pub iterations: u32,
    pub delta: f64,
}

pub fn pagerank(csr: &Csr, damping: f64, max_iter: u32, tolerance: f64) -> PageRank {
    let n = csr.len();
    if n == 0 {
        return PageRank {
            scores: Vec::new(),
            iterations: 0,
            delta: 0.0,
        };
    }
    let base = 1.0 / n as f64;
    let mut rank = vec![base; n];
    let mut next = vec![0.0f64; n];
    let mut iterations = 0;
    let mut delta = 0.0;

    for it in 0..max_iter {
        iterations = it + 1;
        // Dangling nodes redistribute their mass evenly.
        let mut dangling = 0.0;
        for v in 0..n {
            if csr.degree(v) == 0 {
                dangling += rank[v];
            }
        }
        let leak = damping * dangling * base;
        for slot in next.iter_mut() {
            *slot = (1.0 - damping) * base + leak;
        }
        for v in 0..n {
            let deg = csr.degree(v);
            if deg == 0 {
                continue;
            }
            let share = damping * rank[v] / deg as f64;
            for &t in csr.out(v) {
                next[t as usize] += share;
            }
        }
        delta = rank
            .iter()
            .zip(next.iter())
            .map(|(a, b)| (a - b).abs())
            .sum::<f64>();
        std::mem::swap(&mut rank, &mut next);
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
pub fn betweenness(csr: &Csr, sources: Option<&[usize]>, normalize: bool) -> Vec<f64> {
    let n = csr.len();
    let mut score = vec![0.0f64; n];
    let all: Vec<usize> = (0..n).collect();
    let srcs = sources.unwrap_or(&all);

    let mut sigma = vec![0.0f64; n];
    let mut dist = vec![-1i64; n];
    let mut delta = vec![0.0f64; n];
    let mut preds: Vec<Vec<u32>> = vec![Vec::new(); n];

    for &s in srcs {
        if s >= n {
            continue;
        }
        for v in 0..n {
            sigma[v] = 0.0;
            dist[v] = -1;
            delta[v] = 0.0;
            preds[v].clear();
        }
        let mut order: Vec<usize> = Vec::with_capacity(n);
        let mut q = VecDeque::new();
        sigma[s] = 1.0;
        dist[s] = 0;
        q.push_back(s);
        while let Some(v) = q.pop_front() {
            order.push(v);
            for &t in csr.out(v) {
                let t = t as usize;
                if dist[t] < 0 {
                    dist[t] = dist[v] + 1;
                    q.push_back(t);
                }
                if dist[t] == dist[v] + 1 {
                    sigma[t] += sigma[v];
                    preds[t].push(v as u32);
                }
            }
        }
        for &w in order.iter().rev() {
            let coeff = (1.0 + delta[w]) / sigma[w];
            for &v in &preds[w] {
                let v = v as usize;
                delta[v] += sigma[v] * coeff;
            }
            if w != s {
                score[w] += delta[w];
            }
        }
    }

    if normalize && n > 2 {
        let scale = 1.0 / ((n - 1) * (n - 2)) as f64;
        for s in score.iter_mut() {
            *s *= scale;
        }
    }
    score
}

/// Closeness centrality: inverse of mean distance to reachable nodes, scaled by
/// the reachable fraction (Wasserman-Faust), so disconnected graphs behave.
pub fn closeness(csr: &Csr, weighted: bool) -> Vec<f64> {
    let n = csr.len();
    let mut out = vec![0.0f64; n];
    for s in 0..n {
        let paths = if weighted { dijkstra(csr, s) } else { bfs_paths(csr, s) };
        let mut total = 0.0;
        let mut reached = 0usize;
        for (v, d) in paths.dist.iter().enumerate() {
            if v != s && d.is_finite() {
                total += *d;
                reached += 1;
            }
        }
        out[s] = if reached > 0 && total > 0.0 {
            (reached as f64 / total) * (reached as f64 / (n.saturating_sub(1)).max(1) as f64)
        } else {
            0.0
        };
    }
    out
}

pub fn degree_centrality(csr: &Csr) -> Vec<f64> {
    let n = csr.len();
    let denom = (n.saturating_sub(1)).max(1) as f64;
    (0..n).map(|v| csr.degree(v) as f64 / denom).collect()
}

// ----------------------------------------------------------------- structure

/// Connected components over the given view. Pass a `Dir::Both` CSR for weakly
/// connected components of a directed graph.
pub fn components(csr: &Csr) -> (Vec<u32>, u32) {
    let n = csr.len();
    let mut comp = vec![u32::MAX; n];
    let mut count = 0u32;
    let mut q = VecDeque::new();
    for s in 0..n {
        if comp[s] != u32::MAX {
            continue;
        }
        comp[s] = count;
        q.push_back(s);
        while let Some(v) = q.pop_front() {
            for &t in csr.out(v) {
                let t = t as usize;
                if comp[t] == u32::MAX {
                    comp[t] = count;
                    q.push_back(t);
                }
            }
        }
        count += 1;
    }
    (comp, count)
}

/// Tarjan's strongly connected components, iterative.
pub fn strongly_connected(csr: &Csr) -> (Vec<u32>, u32) {
    let n = csr.len();
    let mut index = vec![u32::MAX; n];
    let mut low = vec![0u32; n];
    let mut on_stack = vec![false; n];
    let mut comp = vec![u32::MAX; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0u32;
    let mut count = 0u32;
    // (node, position in its adjacency range)
    let mut call: Vec<(usize, usize)> = Vec::new();

    for root in 0..n {
        if index[root] != u32::MAX {
            continue;
        }
        call.push((root, csr.off[root] as usize));
        index[root] = next_index;
        low[root] = next_index;
        next_index += 1;
        stack.push(root);
        on_stack[root] = true;

        while let Some((v, i)) = call.pop() {
            if i < csr.off[v + 1] as usize {
                call.push((v, i + 1));
                let w = csr.adj[i] as usize;
                if index[w] == u32::MAX {
                    index[w] = next_index;
                    low[w] = next_index;
                    next_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    call.push((w, csr.off[w] as usize));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                if low[v] == index[v] {
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp[w] = count;
                        if w == v {
                            break;
                        }
                    }
                    count += 1;
                }
                if let Some(&(parent, _)) = call.last() {
                    low[parent] = low[parent].min(low[v]);
                }
            }
        }
    }
    (comp, count)
}

/// Triangle count per node, on an undirected view. Returns (per-node, total).
pub fn triangles(csr: &Csr) -> (Vec<u64>, u64) {
    let n = csr.len();
    // Deduplicated, sorted neighbour sets — the CSR may contain parallel edges.
    let mut nbrs: Vec<Vec<u32>> = Vec::with_capacity(n);
    for v in 0..n {
        let mut list: Vec<u32> = csr.out(v).iter().copied().filter(|t| *t as usize != v).collect();
        list.sort_unstable();
        list.dedup();
        nbrs.push(list);
    }
    let mut counts = vec![0u64; n];
    let mut total = 0u64;
    for v in 0..n {
        for &u in &nbrs[v] {
            let u = u as usize;
            if u <= v {
                continue;
            }
            // Intersect the two sorted neighbour lists.
            let (mut i, mut j) = (0usize, 0usize);
            let (a, b) = (&nbrs[v], &nbrs[u]);
            while i < a.len() && j < b.len() {
                match a[i].cmp(&b[j]) {
                    Ordering::Less => i += 1,
                    Ordering::Greater => j += 1,
                    Ordering::Equal => {
                        let w = a[i] as usize;
                        if w > u {
                            counts[v] += 1;
                            counts[u] += 1;
                            counts[w] += 1;
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
pub fn clustering(csr: &Csr, tri: &[u64]) -> Vec<f64> {
    let n = csr.len();
    let mut out = vec![0.0; n];
    for v in 0..n {
        let mut list: Vec<u32> = csr.out(v).iter().copied().filter(|t| *t as usize != v).collect();
        list.sort_unstable();
        list.dedup();
        let k = list.len() as f64;
        out[v] = if k > 1.0 {
            2.0 * tri[v] as f64 / (k * (k - 1.0))
        } else {
            0.0
        };
    }
    out
}

/// k-core decomposition (Batagelj-Zaveršnik): the core number of each node.
pub fn core_numbers(csr: &Csr) -> Vec<u32> {
    let n = csr.len();
    let mut deg: Vec<u32> = (0..n).map(|v| csr.degree(v) as u32).collect();
    let max_deg = deg.iter().copied().max().unwrap_or(0) as usize;

    let mut bin = vec![0usize; max_deg + 2];
    for d in &deg {
        bin[*d as usize] += 1;
    }
    let mut start = 0usize;
    for b in bin.iter_mut() {
        let c = *b;
        *b = start;
        start += c;
    }
    let mut pos = vec![0usize; n];
    let mut vert = vec![0usize; n];
    for v in 0..n {
        pos[v] = bin[deg[v] as usize];
        vert[pos[v]] = v;
        bin[deg[v] as usize] += 1;
    }
    for d in (1..bin.len()).rev() {
        bin[d] = bin[d - 1];
    }
    bin[0] = 0;

    for i in 0..n {
        let v = vert[i];
        let mut seen: Vec<u32> = csr.out(v).to_vec();
        seen.sort_unstable();
        seen.dedup();
        for &u in &seen {
            let u = u as usize;
            if deg[u] > deg[v] {
                let du = deg[u] as usize;
                let pu = pos[u];
                let pw = bin[du];
                let w = vert[pw];
                if u != w {
                    pos[u] = pw;
                    pos[w] = pu;
                    vert[pu] = w;
                    vert[pw] = u;
                }
                bin[du] += 1;
                deg[u] -= 1;
            }
        }
    }
    deg
}

/// Kahn's topological sort. `None` when the graph has a cycle.
pub fn topological_sort(csr: &Csr) -> Option<Vec<usize>> {
    let n = csr.len();
    let mut indeg = vec![0u32; n];
    for v in 0..n {
        for &t in csr.out(v) {
            indeg[t as usize] += 1;
        }
    }
    let mut q: VecDeque<usize> = (0..n).filter(|v| indeg[*v] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(v) = q.pop_front() {
        order.push(v);
        for &t in csr.out(v) {
            let t = t as usize;
            indeg[t] -= 1;
            if indeg[t] == 0 {
                q.push_back(t);
            }
        }
    }
    if order.len() == n {
        Some(order)
    } else {
        None
    }
}

pub fn find_cycle(csr: &Csr) -> Option<Vec<usize>> {
    let n = csr.len();
    // 0 = unvisited, 1 = on stack, 2 = done
    let mut state = vec![0u8; n];
    let mut parent = vec![NO_PARENT; n];
    for root in 0..n {
        if state[root] != 0 {
            continue;
        }
        let mut call: Vec<(usize, usize)> = vec![(root, csr.off[root] as usize)];
        state[root] = 1;
        while let Some((v, i)) = call.pop() {
            if i < csr.off[v + 1] as usize {
                call.push((v, i + 1));
                let w = csr.adj[i] as usize;
                if state[w] == 0 {
                    state[w] = 1;
                    parent[w] = v as u32;
                    call.push((w, csr.off[w] as usize));
                } else if state[w] == 1 {
                    // Walk back from v to w to recover the cycle.
                    let mut cycle = vec![w];
                    let mut cur = v;
                    while cur != w {
                        cycle.push(cur);
                        if parent[cur] == NO_PARENT {
                            break;
                        }
                        cur = parent[cur] as usize;
                    }
                    cycle.push(w);
                    cycle.reverse();
                    return Some(cycle);
                }
            } else {
                state[v] = 2;
            }
        }
    }
    None
}

/// Label propagation communities. Deterministic: ties break toward the
/// smallest community id, and nodes are swept in index order.
pub fn label_propagation(csr: &Csr, max_iter: u32) -> (Vec<u32>, u32) {
    let n = csr.len();
    let mut labels: Vec<u32> = (0..n as u32).collect();
    for _ in 0..max_iter {
        let mut changed = false;
        for v in 0..n {
            if csr.degree(v) == 0 {
                continue;
            }
            let mut tally: Vec<(u32, u32)> = Vec::new();
            for &t in csr.out(v) {
                let l = labels[t as usize];
                match tally.iter_mut().find(|(lab, _)| *lab == l) {
                    Some(slot) => slot.1 += 1,
                    None => tally.push((l, 1)),
                }
            }
            if let Some(&(best, _)) = tally
                .iter()
                .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            {
                if labels[v] != best {
                    labels[v] = best;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Renumber densely.
    let mut remap: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for l in labels.iter_mut() {
        let next = remap.len() as u32;
        *l = *remap.entry(*l).or_insert(next);
    }
    let count = remap.len() as u32;
    (labels, count)
}

/// Kruskal minimum spanning forest over an undirected weighted view.
/// Returns the chosen edge ids and the total weight.
pub fn minimum_spanning_forest(csr: &Csr) -> (Vec<u64>, f64) {
    let n = csr.len();
    let mut candidates: Vec<(f64, usize, usize, u64)> = Vec::new();
    for v in 0..n {
        for i in csr.range(v) {
            let t = csr.adj[i] as usize;
            if v < t {
                candidates.push((csr.weights[i], v, t, csr.eids[i]));
            }
        }
    }
    candidates.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut Vec<usize>, mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }

    let mut chosen = Vec::new();
    let mut total = 0.0;
    for (w, a, b, eid) in candidates {
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent[ra] = rb;
            chosen.push(eid);
            total += w;
        }
    }
    (chosen, total)
}
