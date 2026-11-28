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
use std::time::Instant;

use glider::graph::Graph;
use glider::query::{self, QueryLimits};
use glider::Value;

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
