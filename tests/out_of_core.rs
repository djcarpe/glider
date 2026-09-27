//! Out-of-core execution gives the same answers as in-memory execution:
//! algorithms (below) and write queries whose match sets spill.
//!
//! Algorithms give the same answers in memory and out of core.
//!
//! Every CALL runs three ways on the same random graph: over an in-memory
//! projection (`tier: "mem"`), and over the page store (`tier: "ooc"`) with
//! a working-memory budget so small that every per-node array spills to a
//! temp file — once on a file-backed graph with a tiny page cache, once on a
//! `:memory:` graph that is nearly full. The rows must match exactly.

use std::path::PathBuf;

use glider::graph::{Graph, OpenOptions};
use glider::query::{self, QueryResult};
use glider::value::Value;

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-tiers-{}-{}.db", name, std::process::id()));
    cleanup(&p);
    p
}

fn cleanup(p: &std::path::Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(glider::store::lock_path(p));
    for suffix in ["-wal", "-data", "-tmp"] {
        let mut d = p.as_os_str().to_os_string();
        d.push(suffix);
        let _ = std::fs::remove_dir_all(PathBuf::from(d));
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Hubs, self-loops, parallel edges, two edge types, weights (some
/// missing), and holes in the id space from deletions.
fn build(g: &mut Graph, seed: u64, n: u64, m: u64) {
    let mut r = Rng(seed);
    g.autocommit = false;
    for i in 0..n {
        let props = vec![("name".to_string(), Value::Text(format!("n{i}")))];
        g.add_node(&["N".into()], props).unwrap();
    }
    for _ in 0..m {
        let from = if r.below(4) == 0 { 1 + r.below(3) } else { 1 + r.below(n) };
        let to = if r.below(10) == 0 { from } else { 1 + r.below(n) };
        let t = if r.below(3) == 0 { "B" } else { "A" };
        let props = if r.below(5) == 0 {
            vec![]
        } else {
            vec![("w".to_string(), Value::Float(1.0 + r.below(9) as f64))]
        };
        g.add_edge(from, to, t, props).unwrap();
    }
    for _ in 0..n / 20 {
        let _ = g.delete_node(4 + r.below(n - 4));
    }
    g.commit().unwrap();
}

fn calls(first: u64, second: u64) -> Vec<String> {
    let mut v: Vec<String> = [
        "pagerank(iterations: 15)",
        "pagerank(iterations: 15, type: \"A\")",
        "betweenness(samples: 12)",
        "betweenness(dir: \"both\", samples: 7, normalize: false)",
        "closeness()",
        "closeness(weight: \"w\")",
        "degree()",
        "degree(dir: \"in\")",
        "triangles()",
        "clustering()",
        "kcore()",
        "wcc()",
        "wcc(type: \"B\")",
        "scc()",
        "labelprop()",
        "toposort()",
        "cycle()",
        "mst(weight: \"w\")",
        "mst()",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for (a, b) in [(first, second), (second, first)] {
        v.push(format!("shortestpath(from: {a}, to: {b})"));
        v.push(format!("shortestpath(from: {a}, to: {b}, weight: \"w\")"));
        v.push(format!("sssp(from: {a})"));
        v.push(format!("sssp(from: {a}, weight: \"w\", top: 20)"));
        v.push(format!("bfs(from: {a})"));
        v.push(format!("dfs(from: {a}, depth: 3)"));
        v.push(format!("subgraph(from: {a}, depth: 2)"));
    }
    v
}

fn run(g: &mut Graph, call: &str, tier: &str) -> String {
    let src = if call.ends_with("()") {
        format!("CALL {}tier: \"{tier}\")", &call[..call.len() - 1])
    } else {
        format!("CALL {}, tier: \"{tier}\")", &call[..call.len() - 1])
    };
    match query::execute(g, &src) {
        Ok(r) => show(&r),
        Err(e) => format!("error: {e}"),
    }
}

fn show(r: &QueryResult) -> String {
    let msg = r.message.clone().unwrap_or_default().replace(" (out of core)", "");
    format!("{:?} {:?} {}", r.columns, r.rows, msg)
}

fn first_two(g: &Graph) -> (u64, u64) {
    let ids = g.node_ids();
    (ids[3], ids[ids.len() / 2])
}

#[test]
fn file_backed_out_of_core_matches_in_memory() {
    // Unoptimised builds are ~10x slower: a smaller graph still covers it.
    let (n, m) = if cfg!(debug_assertions) { (250, 900) } else { (700, 2400) };
    for seed in [7u64, 12345] {
        let path = temp(&format!("file{seed}"));
        let mut g = Graph::open_opts(
            &path,
            OpenOptions {
                page_size: 1024,
                cache_size: 64 << 10,
                work_mem: 16 << 10,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        build(&mut g, seed, n, m);
        // Tiny spill blocks and a tiny block cache: spilled arrays go to disk
        // and come back constantly.
        glider::ooc::set_spill_shape(256, 3);
        let (a, b) = first_two(&g);
        for call in calls(a, b) {
            let mem = run(&mut g, &call, "mem");
            let ooc = run(&mut g, &call, "ooc");
            assert_eq!(mem, ooc, "seed {seed}: {call}");
            // And auto picks the paged tier here, with the same answer.
            let auto = run(&mut g, &call, "auto");
            assert_eq!(mem, auto, "seed {seed}: {call} (auto)");
        }
        // Spill files are gone.
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push("-tmp");
        let left = std::fs::read_dir(PathBuf::from(tmp)).map(|d| d.count()).unwrap_or(0);
        assert_eq!(left, 0, "spill files cleaned up");
        drop(g);
        cleanup(&path);
    }
}

#[test]
fn memory_graph_near_its_limit_spills_instead_of_failing() {
    let mut g = Graph::memory_with_page_size(1024, 64 << 20);
    build(&mut g, 4242, 1000, 3500);
    // Leave almost no headroom: algorithm state must go to temp files.
    glider::ooc::set_spill_shape(256, 2);
    let used = g.stats().memory.unwrap().0;
    g.set_max_memory(used + (32 << 10));
    let (a, b) = first_two(&g);
    let mut reference = Graph::memory();
    build(&mut reference, 4242, 1000, 3500);
    for call in calls(a, b) {
        let want = run(&mut reference, &call, "mem");
        let got = run(&mut g, &call, "auto");
        assert_eq!(want, got, "{call}");
    }
}

#[test]
fn write_back_streams_to_every_node() {
    let path = temp("write");
    let mut g = Graph::open_opts(
        &path,
        OpenOptions {
            page_size: 1024,
            cache_size: 64 << 10,
            work_mem: 16 << 10,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    build(&mut g, 5, 1500, 4000);
    query::execute(&mut g, "CALL pagerank(iterations: 10, write: \"pr\", top: 3, tier: \"ooc\")").unwrap();
    let mut m = Graph::memory();
    build(&mut m, 5, 1500, 4000);
    query::execute(&mut m, "CALL pagerank(iterations: 10, write: \"pr\", tier: \"mem\")").unwrap();
    for id in m.node_ids() {
        assert_eq!(g.node_prop(id, "pr"), m.node_prop(id, "pr"), "node {id}");
    }
    drop(g);
    cleanup(&path);
}

#[test]
fn write_queries_spool_their_matches_to_disk() {
    let path = temp("spool");
    let mut g = Graph::open_opts(
        &path,
        OpenOptions {
            page_size: 1024,
            cache_size: 64 << 10,
            work_mem: 0,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    glider::ooc::set_spill_shape(64, 2);
    let mut m = Graph::memory();
    build(&mut g, 31, 900, 3000);
    build(&mut m, 31, 900, 3000);
    let writes = [
        "MATCH (n:N) SET n.seen = 1",
        "MATCH (a)-[e:A]->(b) SET e.hop = 2, b:Reached",
        "MATCH (a)-[e:B]->(b) WHERE a.seen = 1 REMOVE b:N",
        "MATCH (a:Reached) CREATE (a)-[:C]->(x:New {k: 1})",
        "MATCH (a)-[e:A]->(b) WHERE e.w > 5 DELETE e",
        "MATCH (a:New) DETACH DELETE a",
        "MATCH (a)-[:B]->(b) DETACH DELETE b",
    ];
    for w in writes {
        let x = query::execute(&mut g, w).map(|r| show(&r)).unwrap_or_else(|e| format!("error: {e}"));
        let y = query::execute(&mut m, w).map(|r| show(&r)).unwrap_or_else(|e| format!("error: {e}"));
        assert_eq!(x, y, "{w}");
        for q in [
            "MATCH (n) RETURN count(n)",
            "MATCH ()-[e]->() RETURN count(e)",
            "MATCH (n:Reached) RETURN n.name, n.seen ORDER BY n.name LIMIT 50",
            "MATCH (a)-[e]->(b) RETURN id(e), type(e), e.hop ORDER BY id(e) LIMIT 50",
        ] {
            let x = query::execute(&mut g, q).map(|r| show(&r)).unwrap();
            let y = query::execute(&mut m, q).map(|r| show(&r)).unwrap();
            assert_eq!(x, y, "after {w}: {q}");
        }
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push("-tmp");
    let left = std::fs::read_dir(PathBuf::from(tmp)).map(|d| d.count()).unwrap_or(0);
    assert_eq!(left, 0, "spool files cleaned up");
    drop(g);
    cleanup(&path);
}
