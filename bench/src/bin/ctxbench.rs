//! ctxbench: what the `context` extension costs and what it buys.
//!
//!   ctxbench quality [--nodes 200000] [--seeds 200] [--budget 1500]
//!   ctxbench scale --db big.gldb [--cache-mb 256] [--seeds 50]
//!
//! **quality** builds a graph whose right answer is known: communities of
//! 50 densely linked entities, every entity also linked to 2 random
//! entities anywhere (noise) and to one of 10 hubs. All edges have the
//! same type, so nothing but structure separates signal from noise. From
//! a seed, the relevant context is its community. It compares, at the same
//! token budget and the same compact rendering:
//!
//! - `context.retrieve` (personalised PageRank, hub damping, connected growth)
//! - the same without hub damping (`max_fanout` unlimited)
//! - breadth-first expansion (what "neighbours, then their neighbours" gives)
//!
//! and, for tokens, what a model would get from the usual tool call: a
//! Cypher neighbourhood query returned as JSON rows.
//!
//! **scale** opens an existing database (from `scale-gen --paged`) with a
//! small page cache and times retrieve, paths, search and schema from
//! random seeds, with page reads, cache misses and peak RSS.
//!
//! Output: one JSON object per line, then a summary on stderr.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use glider::ext::context::tokens;
use glider::graph::{Graph, OpenOptions};
use glider::query;
use glider::value::{parse_json, Value};
use glider::Dir;

struct R(u64);

impl R {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut x = self.0;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const WORDS: &[&str] = &[
    "alpha", "basalt", "cobalt", "delta", "ember", "fjord", "garnet", "harbor", "indigo", "juniper", "krypton", "lumen",
    "meridian", "nebula", "onyx", "prism", "quartz", "rune", "sierra", "tundra",
];

fn peak_rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM:")).map(|l| l.to_string()))
        .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1).cloned())
}

fn text(r: &query::QueryResult) -> String {
    match r.rows.first().and_then(|row| row.first()) {
        Some(Value::Text(s)) => s.clone(),
        _ => String::new(),
    }
}

fn pct(xs: &mut [f64], p: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.total_cmp(b));
    xs[((xs.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("quality") => quality(&args),
        Some("scale") => scale(&args),
        _ => {
            eprintln!("usage: ctxbench quality [--nodes N] [--seeds S] [--budget B]\n       ctxbench scale --db PATH [--cache-mb M] [--seeds S]");
            std::process::exit(2);
        }
    }
}

// ------------------------------------------------------------------ quality

const COMMUNITY: u64 = 50;
const HUBS: u64 = 10;

fn quality(args: &[String]) {
    let n: u64 = arg(args, "--nodes").and_then(|v| v.parse().ok()).unwrap_or(200_000);
    let seeds: usize = arg(args, "--seeds").and_then(|v| v.parse().ok()).unwrap_or(200);
    let budgets: Vec<usize> = arg(args, "--budget")
        .map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_else(|| vec![500, 1500, 4000]);

    // Extra retrieve arguments, for trying settings: --extra "alpha: 0.1"
    let extra = arg(args, "--extra").map(|e| format!(", {e}")).unwrap_or_default();
    let t0 = Instant::now();
    let mut g = Graph::memory();
    g.autocommit = false;
    let mut r = R(7);
    // Hubs are ids 1..=HUBS; entity i (0-based) is id HUBS + 1 + i, in
    // community i / COMMUNITY.
    for h in 0..HUBS {
        g.add_node(&["Topic".into()], vec![("name".into(), Value::from(format!("Topic {h}")))]).unwrap();
    }
    for i in 0..n {
        let w = WORDS[r.below(WORDS.len() as u64) as usize];
        g.add_node(
            &["Entity".into()],
            vec![
                ("name".into(), Value::from(format!("{w} {i}"))),
                ("note".into(), Value::from(format!("entity {i} of group {}", i / COMMUNITY))),
            ],
        )
        .unwrap();
    }
    let id = |i: u64| HUBS + 1 + i;
    for i in 0..n {
        let c = i / COMMUNITY;
        let size = COMMUNITY.min(n - c * COMMUNITY);
        for _ in 0..5 {
            let j = c * COMMUNITY + r.below(size);
            if j != i {
                g.add_edge(id(i), id(j), "REL", vec![]).unwrap();
            }
        }
        for _ in 0..2 {
            g.add_edge(id(i), id(r.below(n)), "REL", vec![]).unwrap();
        }
        g.add_edge(id(i), 1 + r.below(HUBS), "REL", vec![]).unwrap();
        if i % 100_000 == 99_999 {
            g.commit().unwrap();
        }
    }
    g.commit().unwrap();
    g.autocommit = true;
    query::execute(&mut g, "CREATE EXTENSION context").unwrap();
    let build_s = t0.elapsed().as_secs_f64();
    eprintln!("built {} nodes, {} edges in {build_s:.1}s", g.node_count(), g.edge_count());

    let community_of = |node: u64| -> Option<u64> { (node > HUBS).then(|| (node - HUBS - 1) / COMMUNITY) };

    for &budget in &budgets {
        #[derive(Default)]
        struct Acc {
            precision: Vec<f64>,
            recall: Vec<f64>,
            nodes: Vec<f64>,
            tokens: Vec<f64>,
            ms: Vec<f64>,
        }
        let mut acc: Vec<(&str, Acc)> = vec![
            ("context.retrieve", Acc::default()),
            ("retrieve, no hub damping", Acc::default()),
            ("bfs expansion", Acc::default()),
            ("cypher 1-hop JSON", Acc::default()),
            ("cypher 2-hop JSON", Acc::default()),
        ];
        let mut rs = R(1000 + budget as u64);
        for _ in 0..seeds {
            let i = rs.below(n);
            let seed = id(i);
            let c = i / COMMUNITY;
            let truth = COMMUNITY.min(n - c * COMMUNITY) - 1;
            let score = |nodes: &[u64]| -> (f64, f64) {
                let others: Vec<u64> = nodes.iter().copied().filter(|&x| x != seed).collect();
                let hit = others.iter().filter(|&&x| community_of(x) == Some(c)).count() as f64;
                (if others.is_empty() { 0.0 } else { hit / others.len() as f64 }, hit / truth as f64)
            };
            let rows_nodes = |r: &query::QueryResult| -> Vec<u64> {
                r.rows.iter().filter_map(|row| row.first().and_then(|v| v.as_i64()).map(|v| v as u64)).collect()
            };

            // context.retrieve, two ways.
            let mut picked = 0usize;
            for (k, fanout) in [(0usize, 500usize), (1, usize::MAX / 2)] {
                let t = Instant::now();
                let src = format!("CALL context.retrieve(seeds: [{seed}], budget: {budget}, max_fanout: {fanout}{extra})");
                let res = query::execute(&mut g, &src).unwrap();
                let ms = t.elapsed().as_secs_f64() * 1e3;
                let txt = text(&res);
                let rows = query::execute(&mut g, &format!("CALL context.retrieve(seeds: [{seed}], budget: {budget}, max_fanout: {fanout}, format: \"rows\"{extra})")).unwrap();
                let nodes = rows_nodes(&rows);
                if k == 0 {
                    picked = nodes.len();
                }
                let (p, rc) = score(&nodes);
                let a = &mut acc[k].1;
                a.precision.push(p);
                a.recall.push(rc);
                a.nodes.push(nodes.len() as f64);
                a.tokens.push(tokens(&txt) as f64);
                a.ms.push(ms);
            }

            // BFS: the first nodes a breadth-first walk reaches, as many as
            // retrieve showed, rendered the same way (cards + edges among them).
            let t = Instant::now();
            let mut bfs = Vec::new();
            glider::traverse::bfs(&g, seed, Dir::Both, None, Some(3), usize::MAX, &mut |v| {
                bfs.push(v.node);
                bfs.len() < picked.max(1)
            });
            let ms = t.elapsed().as_secs_f64() * 1e3;
            let set: HashSet<u64> = bfs.iter().copied().collect();
            let mut out = String::new();
            for &v in &bfs {
                let r = query::execute(&mut g, &format!("MATCH (n) WHERE id(n) = {v} RETURN context.card(n)")).unwrap();
                out.push_str(&text(&r));
                out.push('\n');
                g.for_each_adj(v, Dir::Out, None, |a| {
                    if set.contains(&a.other) {
                        out.push_str(&format!("n{v} -REL-> n{}\n", a.other));
                    }
                });
            }
            let (p, rc) = score(&bfs);
            let a = &mut acc[2].1;
            a.precision.push(p);
            a.recall.push(rc);
            a.nodes.push(bfs.len() as f64);
            a.tokens.push(tokens(&out) as f64);
            a.ms.push(ms);

            // What a model gets from a plain neighbourhood query.
            for (k, pat) in [(3usize, "(s)-[r]-(m)"), (4, "(s)-[r]-()-[r2]-(m)")] {
                let t = Instant::now();
                let src = format!("MATCH {pat} WHERE id(s) = {seed} RETURN s, r, m");
                let json = glider::api::query_json(&mut g, &src).unwrap();
                let ms = t.elapsed().as_secs_f64() * 1e3;
                let ids = query::execute(&mut g, &format!("MATCH {pat} WHERE id(s) = {seed} RETURN DISTINCT id(m)")).unwrap();
                let nodes = rows_nodes(&ids);
                let (p, rc) = score(&nodes);
                let a = &mut acc[k].1;
                a.precision.push(p);
                a.recall.push(rc);
                a.nodes.push(nodes.len() as f64);
                a.tokens.push(tokens(&json) as f64);
                a.ms.push(ms);
            }
        }
        for (name, a) in &mut acc {
            let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
            let line = format!(
                "{{\"mode\":\"quality\",\"nodes\":{n},\"budget\":{budget},\"method\":\"{name}\",\"precision\":{:.4},\"recall\":{:.4},\"shown\":{:.1},\"tokens_mean\":{:.0},\"tokens_p95\":{:.0},\"ms_p50\":{:.3},\"ms_p95\":{:.3},\"seeds\":{seeds}}}",
                mean(&a.precision),
                mean(&a.recall),
                mean(&a.nodes),
                mean(&a.tokens),
                pct(&mut a.tokens, 0.95),
                pct(&mut a.ms, 0.5),
                pct(&mut a.ms, 0.95)
            );
            println!("{line}");
            eprintln!(
                "budget {budget:>5}  {name:<26} precision {:.3}  recall {:.3}  nodes {:>7.1}  tokens {:>8.0}  p50 {:.2} ms",
                mean(&a.precision),
                mean(&a.recall),
                mean(&a.nodes),
                mean(&a.tokens),
                pct(&mut a.ms, 0.5)
            );
        }
    }
    eprintln!("peak RSS {:.0} MB", peak_rss_mb());
}

// ------------------------------------------------------------------ scale

fn scale(args: &[String]) {
    let db = PathBuf::from(arg(args, "--db").expect("--db PATH"));
    let cache_mb: u64 = arg(args, "--cache-mb").and_then(|v| v.parse().ok()).unwrap_or(256);
    let seeds: usize = arg(args, "--seeds").and_then(|v| v.parse().ok()).unwrap_or(50);
    let label = arg(args, "--label").unwrap_or_else(|| "cold".into());
    let opts = OpenOptions { cache_size: cache_mb << 20, ..OpenOptions::default() };
    let t = Instant::now();
    let mut g = Graph::open_opts(&db, opts).expect("open");
    let open_ms = t.elapsed().as_secs_f64() * 1e3;
    if g.extension_version("context").is_none() {
        query::execute(&mut g, "CREATE EXTENSION context").unwrap();
    }
    let (nodes, edges) = (g.node_count(), g.edge_count());
    let max_id = g.stats().nodes as u64 * 2;
    let size_gib = g.file_len() as f64 / (1u64 << 30) as f64;
    eprintln!("{}: {nodes} nodes, {edges} edges, {size_gib:.2} GiB, open {open_ms:.1} ms, cache {cache_mb} MiB", db.display());

    let mut rng = R(42);
    let mut pick = |g: &Graph| loop {
        let id = 1 + rng.below(max_id);
        if g.node(id).is_some() {
            return id;
        }
    };
    let emit = |op: &str, ms: &mut Vec<f64>, misses: &mut Vec<f64>, extra: String| {
        let line = format!(
            "{{\"mode\":\"scale\",\"db\":\"{}\",\"gib\":{size_gib:.2},\"nodes\":{nodes},\"edges\":{edges},\"cache_mb\":{cache_mb},\"cache\":\"{label}\",\"op\":\"{op}\",\"n\":{},\"ms_p50\":{:.3},\"ms_p95\":{:.3},\"ms_max\":{:.3},\"misses_p50\":{:.0},\"misses_p95\":{:.0}{extra}}}",
            db.file_name().unwrap().to_string_lossy(),
            ms.len(),
            pct(ms, 0.5),
            pct(ms, 0.95),
            pct(ms, 1.0),
            pct(misses, 0.5),
            pct(misses, 0.95)
        );
        println!("{line}");
        eprintln!("  {op:<28} p50 {:>9.2} ms  p95 {:>9.2} ms  misses p50 {:>6.0}{extra}", pct(ms, 0.5), pct(ms, 0.95), pct(misses, 0.5));
    };
    let run = |g: &mut Graph, src: &str| -> (f64, f64, query::QueryResult) {
        let before = g.pager_stats();
        let t = Instant::now();
        let r = query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"));
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let after = g.pager_stats();
        (ms, after.misses.saturating_sub(before.misses) as f64, r)
    };

    for budget in [500usize, 2000, 8000] {
        let (mut ms, mut mi, mut tok, mut shown, mut reached) = (vec![], vec![], vec![], vec![], vec![]);
        for _ in 0..seeds {
            let s = pick(&g);
            let (t, m, r) = run(&mut g, &format!("CALL context.retrieve(seeds: [{s}], budget: {budget})"));
            ms.push(t);
            mi.push(m);
            tok.push(r.rows[0][3].as_i64().unwrap_or(0) as f64);
            shown.push(r.rows[0][1].as_i64().unwrap_or(0) as f64);
            let msg = r.message.unwrap_or_default();
            reached.push(msg.split(" of ").nth(1).and_then(|x| x.split_whitespace().next()).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0));
        }
        let extra = format!(
            ",\"budget\":{budget},\"tokens_p50\":{:.0},\"tokens_max\":{:.0},\"shown_p50\":{:.0},\"reached_p50\":{:.0},\"reached_max\":{:.0}",
            pct(&mut tok, 0.5),
            pct(&mut tok, 1.0),
            pct(&mut shown, 0.5),
            pct(&mut reached, 0.5),
            pct(&mut reached, 1.0)
        );
        emit(&format!("retrieve budget {budget}"), &mut ms, &mut mi, extra);
    }

    // The plain alternative at this size: one hop as JSON rows.
    {
        let (mut ms, mut mi, mut tok) = (vec![], vec![], vec![]);
        for _ in 0..seeds {
            let s = pick(&g);
            let before = g.pager_stats();
            let t = Instant::now();
            let json = glider::api::query_json(&mut g, &format!("MATCH (s)-[r]-(m) WHERE id(s) = {s} RETURN s, r, m")).unwrap();
            ms.push(t.elapsed().as_secs_f64() * 1e3);
            mi.push(g.pager_stats().misses.saturating_sub(before.misses) as f64);
            tok.push(tokens(&json) as f64);
        }
        let extra = format!(",\"tokens_p50\":{:.0},\"tokens_max\":{:.0}", pct(&mut tok, 0.5), pct(&mut tok, 1.0));
        emit("cypher 1-hop JSON", &mut ms, &mut mi, extra);
    }

    {
        let (mut ms, mut mi, mut found, mut hops) = (vec![], vec![], 0usize, vec![]);
        for _ in 0..seeds.min(30) {
            let (a, b) = (pick(&g), pick(&g));
            let (t, m, r) = run(&mut g, &format!("CALL context.paths(from: {a}, to: {b}, k: 3, depth: 8)"));
            ms.push(t);
            mi.push(m);
            let msg = r.message.unwrap_or_default();
            if msg != "no path" {
                found += 1;
                hops.push(msg.split(" of ").nth(1).and_then(|x| x.split_whitespace().next()).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0));
            }
        }
        let extra = format!(",\"found\":{found},\"hops_p50\":{:.0}", pct(&mut hops, 0.5));
        emit("paths k=3", &mut ms, &mut mi, extra);
    }

    {
        let (t, m, r) = run(&mut g, "CALL context.schema()");
        let tok = r.rows[0][1].as_i64().unwrap_or(0);
        emit("schema", &mut vec![t], &mut vec![m], format!(",\"tokens\":{tok}"));
    }

    // Search without an index is a scan; bound it the way an agent would.
    const QUERIES: &[&str] = &["copper lantern", "glacier kernel harbour", "quiet thunder", "marble atlas", "London engineer"];
    {
        let (mut ms, mut mi) = (vec![], vec![]);
        for w in QUERIES {
            let (t, m, _) = run(&mut g, &format!("CALL context.search(query: \"{w}\", scan: 100000, top: 5)"));
            ms.push(t);
            mi.push(m);
        }
        emit("search, scan 100k nodes", &mut ms, &mut mi, String::new());
    }
    // Then through the text index, built here if the database has none.
    let index_labels = arg(args, "--index-labels").unwrap_or_else(|| "Document,Product".into());
    if glider::ext::text::config(&g).is_none() {
        let labels = index_labels.split(',').map(|l| format!("\"{l}\"")).collect::<Vec<_>>().join(", ");
        let (t, m, r) = run(&mut g, &format!("CALL context.index(labels: [{labels}])"));
        let msg = r.message.unwrap_or_default().replace('"', "'");
        emit("index build", &mut vec![t], &mut vec![m], format!(",\"labels\":\"{index_labels}\",\"result\":\"{msg}\""));
    }
    {
        let (mut ms, mut mi, mut read) = (vec![], vec![], vec![]);
        for _ in 0..3 {
            for w in QUERIES {
                let (t, m, r) = run(&mut g, &format!("CALL context.search(query: \"{w}\", top: 5)"));
                ms.push(t);
                mi.push(m);
                read.push(r.message.unwrap_or_default().split(", ").nth(1).and_then(|x| x.split_whitespace().next()).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0));
            }
        }
        let extra = format!(",\"postings_p50\":{:.0}", pct(&mut read, 0.5));
        emit("search, text index", &mut ms, &mut mi, extra);
        let (mut ms, mut mi) = (vec![], vec![]);
        // Words that occur in the indexed labels' text.
        for w in &QUERIES[..4] {
            let (t, m, _) = run(&mut g, &format!("CALL context.retrieve(query: \"{w}\", budget: 2000)"));
            ms.push(t);
            mi.push(m);
        }
        emit("retrieve from query, 2000", &mut ms, &mut mi, String::new());
        // A write now also maintains the index.
        let (mut ms, mut mi) = (vec![], vec![]);
        for i in 0..200 {
            let (t, m, _) = run(&mut g, &format!("CREATE (:Document {{title: \"copper lantern note {i}\", body: \"quiet thunder over the glacier\"}})"));
            ms.push(t);
            mi.push(m);
        }
        emit("indexed write (CREATE)", &mut ms, &mut mi, String::new());
    }

    let _ = parse_json("{}");
    println!("{{\"mode\":\"scale\",\"db\":\"{}\",\"cache\":\"{label}\",\"op\":\"peak_rss\",\"open_ms\":{open_ms:.1},\"peak_rss_mb\":{:.0}}}", db.file_name().unwrap().to_string_lossy(), peak_rss_mb());
    eprintln!("  peak RSS {:.0} MB", peak_rss_mb());
    drop(g);
}
