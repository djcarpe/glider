//! Traversals that touch only what they reach.
//!
//! The algorithms in [`crate::algo`] run over a projection of the whole
//! graph: per-node state for every node, sized before the first step. That
//! is right for PageRank, which visits everything anyway, and wrong for a
//! BFS two hops deep from one person, which should cost what it visits.
//!
//! These walk the adjacency tree directly and keep state only for visited
//! nodes. They visit in the same order as the projection-based versions
//! (the projection is built from the same adjacency scan), so results are
//! identical. Each takes a cap on how many nodes it may remember; past it
//! they give up and return `None`, and the caller falls back to the
//! whole-graph algorithm, whose memory is bounded by spilling.

use std::collections::VecDeque;

use crate::graph::Graph;
use crate::types::{id_map, id_set, Dir};

/// One visited node: its id, hops from the start, and the node it was
/// reached from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Visit {
    pub node: u64,
    pub depth: u32,
    pub parent: Option<u64>,
}

/// Remembered state per visited node, for sizing the cap from a memory
/// budget: a hash-set slot plus a queue entry.
pub const BYTES_PER_NODE: u64 = 48;

/// Breadth-first from `from`, handing visits to `f` in order until it
/// returns false. `None` if more than `cap` nodes had to be remembered.
pub fn bfs(
    g: &Graph,
    from: u64,
    dir: Dir,
    etype: Option<u32>,
    max_depth: Option<u32>,
    cap: usize,
    f: &mut dyn FnMut(Visit) -> bool,
) -> Option<()> {
    let mut seen = id_set();
    let mut q = VecDeque::new();
    seen.insert(from);
    q.push_back(Visit { node: from, depth: 0, parent: None });
    while let Some(v) = q.pop_front() {
        if !f(v) {
            return Some(());
        }
        if max_depth.is_some_and(|m| v.depth >= m) {
            continue;
        }
        let mut over = false;
        g.for_each_adj_while(v.node, dir, etype, |a| {
            if seen.insert(a.other) {
                if seen.len() > cap {
                    over = true;
                    return false;
                }
                q.push_back(Visit { node: a.other, depth: v.depth + 1, parent: Some(v.node) });
            }
            true
        });
        if over {
            return None;
        }
    }
    Some(())
}

/// Depth-first from `from`, first neighbour first, as [`crate::algo::dfs_each`].
pub fn dfs(
    g: &Graph,
    from: u64,
    dir: Dir,
    etype: Option<u32>,
    max_depth: Option<u32>,
    cap: usize,
    f: &mut dyn FnMut(Visit) -> bool,
) -> Option<()> {
    let mut seen = id_set();
    let mut stack = vec![Visit { node: from, depth: 0, parent: None }];
    let mut next = Vec::new();
    while let Some(v) = stack.pop() {
        if !seen.insert(v.node) {
            continue;
        }
        if seen.len() > cap || stack.len() > cap {
            return None;
        }
        if !f(v) {
            return Some(());
        }
        if max_depth.is_some_and(|m| v.depth >= m) {
            continue;
        }
        next.clear();
        g.for_each_adj(v.node, dir, etype, |a| {
            if !seen.contains(&a.other) {
                next.push(Visit { node: a.other, depth: v.depth + 1, parent: Some(v.node) });
            }
        });
        stack.extend(next.drain(..).rev());
    }
    Some(())
}

/// Fewest hops from `from` to `to`: the node ids along one shortest path,
/// both ends included, or `Some(None)` when there is none. Searches from
/// both ends, always widening the smaller frontier, so it touches roughly
/// the square root of what a one-sided search would. `None` past `cap`.
pub fn shortest_path(g: &Graph, from: u64, to: u64, dir: Dir, etype: Option<u32>, cap: usize) -> Option<Option<Vec<u64>>> {
    if from == to {
        return Some(Some(vec![from]));
    }
    let back_dir = match dir {
        Dir::Out => Dir::In,
        Dir::In => Dir::Out,
        Dir::Both => Dir::Both,
    };
    // parent maps double as visited sets; u64::MAX marks an end.
    let mut fwd = id_map::<u64>();
    let mut bwd = id_map::<u64>();
    fwd.insert(from, u64::MAX);
    bwd.insert(to, u64::MAX);
    let mut ff = vec![from];
    let mut bf = vec![to];
    while !ff.is_empty() && !bf.is_empty() {
        let forward = ff.len() <= bf.len();
        let (frontier, seen, other, d) = if forward {
            (&mut ff, &mut fwd, &bwd, dir)
        } else {
            (&mut bf, &mut bwd, &fwd, back_dir)
        };
        let mut next = Vec::new();
        let mut meet = None;
        for &v in frontier.iter() {
            g.for_each_adj_while(v, d, etype, |a| {
                if seen.contains_key(&a.other) {
                    return true;
                }
                seen.insert(a.other, v);
                if other.contains_key(&a.other) {
                    meet = Some(a.other);
                    return false;
                }
                next.push(a.other);
                true
            });
            if meet.is_some() {
                break;
            }
            if fwd_len(seen, other) > cap {
                return None;
            }
        }
        if let Some(m) = meet {
            let mut path = Vec::new();
            let mut x = m;
            while x != u64::MAX {
                path.push(x);
                x = fwd[&x];
            }
            path.reverse();
            let mut x = bwd[&m];
            while x != u64::MAX {
                path.push(x);
                x = bwd[&x];
            }
            return Some(Some(path));
        }
        *frontier = next;
    }
    Some(None)
}

fn fwd_len(a: &crate::types::IdMap<u64>, b: &crate::types::IdMap<u64>) -> usize {
    a.len() + b.len()
}
