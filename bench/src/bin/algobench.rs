//! What a whole-graph procedure costs, end to end through `CALL`: for
//! measuring that counting steps against the statement budget is cheap
//! (docs/DERIVED.md, "Budgets for procedures").
//!
//!   algobench [nodes] [edges] [runs]
//!
//! Builds an in-memory graph of `nodes` nodes and `edges` random edges
//! (default 200,000 and 1,200,000, from a fixed seed), then runs each
//! statement `runs` times (default 7) and prints the fastest and the median
//! run in milliseconds. With `BUDGET=1` every statement runs under a step
//! budget too large to be reached, so the meter is counting but never stops.
//! `ONLY=<name>` runs only the statements whose name starts with it.
//!
//! With `CORE=1` it instead times the algorithms themselves on one
//! in-memory projection, the engine's against a copy of the code as it was
//! before it counted steps (below), interleaved run by run in one process
//! so that the machine's load falls on both alike.
use std::time::Instant;

use glider::algo::{self, Adjacency};
use glider::graph::Graph;
use glider::ooc::{SpillQueue, StateVec};
use glider::query::{self, QueryLimits};
use glider::{Dir, Value};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: u64| args.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let (nodes, edges, runs) = (arg(1, 200_000), arg(2, 1_200_000), arg(3, 7) as usize);
    let budget = std::env::var("BUDGET").map(|v| v == "1").unwrap_or(false);

    let mut g = Graph::memory();
    g.autocommit = false;
    let t = Instant::now();
    let ids: Vec<u64> = (0..nodes)
        .map(|i| {
            g.add_node(&["N".into()], vec![("i".into(), Value::Int(i as i64))])
                .unwrap()
        })
        .collect();
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..edges {
        let (a, b) = (next() % nodes, next() % nodes);
        g.add_edge(ids[a as usize], ids[b as usize], "E", vec![])
            .unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    eprintln!(
        "graph: {nodes} nodes, {edges} edges in {:.1} s{}",
        t.elapsed().as_secs_f64(),
        if budget { "; under a step budget" } else { "" }
    );

    if std::env::var("CORE").map(|v| v == "1").unwrap_or(false) {
        core(&g, runs);
        return;
    }
    let limits = QueryLimits {
        max_rows: usize::MAX,
        max_bytes: u64::MAX,
        max_millis: None,
        max_steps: budget.then_some(u64::MAX / 2),
    };
    let from = ids[0];
    let bfs_walk = format!("CALL bfs(from: {from}, dir: \"both\", top: 100000000)");
    let bfs_mem = format!("CALL bfs(from: {from}, dir: \"both\", tier: \"mem\", top: 100000000)");
    let statements = [
        ("degree", "CALL degree(top: 10)"),
        ("pagerank", "CALL pagerank(top: 10)"),
        (
            "pagerank ooc",
            "CALL pagerank(top: 10, tier: \"ooc\", iterations: 5)",
        ),
        ("bfs walk", bfs_walk.as_str()),
        ("bfs mem", bfs_mem.as_str()),
        ("components", "CALL components(top: 10)"),
    ];
    let only = std::env::var("ONLY").ok();
    for (name, src) in statements {
        if only.as_deref().is_some_and(|o| !name.starts_with(o)) {
            continue;
        }
        let mut times = Vec::new();
        let mut rows = 0;
        for _ in 0..runs {
            let t = Instant::now();
            let r = query::execute_capped(&mut g, src, &[], limits).unwrap();
            times.push(t.elapsed().as_secs_f64() * 1e3);
            rows = r.rows.len();
        }
        times.sort_by(|a, b| a.total_cmp(b));
        println!(
            "{name:<14} min {:>8.1} ms  median {:>8.1} ms  ({rows} rows)",
            times[0],
            times[times.len() / 2]
        );
    }
}

/// Fastest of `runs` interleaved runs of `old` and `new`, in milliseconds.
fn race(runs: usize, old: &mut dyn FnMut() -> f64, new: &mut dyn FnMut() -> f64) -> (f64, f64) {
    let (mut a, mut b) = (f64::MAX, f64::MAX);
    let (mut x, mut y) = (0.0, 0.0);
    for _ in 0..runs {
        let t = Instant::now();
        x += old();
        a = a.min(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        y += new();
        b = b.min(t.elapsed().as_secs_f64() * 1e3);
    }
    assert_eq!(x, y, "the two must compute the same");
    (a, b)
}

fn core(g: &Graph, runs: usize) {
    let out = g.csr(Dir::Out, None, None);
    let both = g.csr(Dir::Both, None, None);
    let report = |name: &str, (a, b): (f64, f64)| {
        println!(
            "{name:<14} before {a:>8.1} ms  after {b:>8.1} ms  ({:+.1}%)",
            (b / a - 1.0) * 100.0
        )
    };
    report(
        "pagerank",
        race(
            runs,
            &mut || old_pagerank(&out, 0.85, 20).get(0),
            &mut || algo::pagerank(&out, 0.85, 20, 0.0).scores.get(0),
        ),
    );
    if std::env::var("AA").is_ok() {
        // the old code against itself: how much the order of the race says
        report(
            "pagerank A/A",
            race(
                runs,
                &mut || old_pagerank(&out, 0.85, 20).get(0),
                &mut || old_pagerank(&out, 0.85, 20).get(0),
            ),
        );
    }
    report(
        "bfs",
        race(runs, &mut || old_bfs(&both, 0) as f64, &mut || {
            let mut n = 0;
            algo::bfs_each(&both, 0, None, &mut |_| {
                n += 1;
                true
            });
            n as f64
        }),
    );
    report(
        "components",
        race(runs, &mut || old_components(&both) as f64, &mut || {
            algo::components(&both).1 as f64
        }),
    );
}

// The algorithms as they were before they counted steps, for `CORE=1`.

fn old_pagerank<A: Adjacency + ?Sized>(a: &A, damping: f64, max_iter: u32) -> StateVec<f64> {
    let n = a.len();
    let base = 1.0 / n as f64;
    let mut rank = StateVec::new(n, base);
    let mut next = StateVec::new(n, 0.0f64);
    let mut deg = StateVec::new(n, 0u64);
    for v in 0..n {
        deg.set(v, a.degree(v) as u64);
    }
    for _ in 0..max_iter {
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
        let mut delta = 0.0;
        for v in 0..n {
            delta += (rank.get(v) - next.get(v)).abs();
        }
        rank.swap(&mut next);
        if delta < 0.0 {
            break;
        }
    }
    rank
}

fn old_bfs<A: Adjacency + ?Sized>(a: &A, source: usize) -> usize {
    let n = a.len();
    let mut seen = StateVec::new(n, false);
    let mut q: SpillQueue<(u64, u32, u64)> = SpillQueue::new();
    seen.set(source, true);
    q.push_back((source as u64, 0, u64::MAX));
    let mut visits = 0;
    while let Some((v, d, _)) = q.pop_front() {
        let v = v as usize;
        visits += 1;
        a.each(v, &mut |t, _, _| {
            if !seen.get(t) {
                seen.set(t, true);
                q.push_back((t as u64, d + 1, v as u64));
            }
        });
    }
    visits
}

fn old_components<A: Adjacency + ?Sized>(a: &A) -> u64 {
    let n = a.len();
    let mut comp = StateVec::new(n, u64::MAX);
    let mut count = 0u64;
    let mut q: SpillQueue<u64> = SpillQueue::new();
    for s in 0..n {
        if comp.get(s) != u64::MAX {
            continue;
        }
        comp.set(s, count);
        q.push_back(s as u64);
        while let Some(v) = q.pop_front() {
            a.each(v as usize, &mut |t, _, _| {
                if comp.get(t) == u64::MAX {
                    comp.set(t, count);
                    q.push_back(t as u64);
                }
            });
        }
        count += 1;
    }
    count
}
