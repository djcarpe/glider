//! Differential tests for the paged engine.
//!
//! The same random mutations run against an *oracle* — the legacy in-memory
//! engine, an independent implementation — and a *subject*: the paged engine
//! with tiny pages and, when file-backed, a tiny page cache, so splits,
//! overflow chains, copy-on-write and eviction happen constantly. Every
//! observable answer must agree, in order, after every batch.
//!
//! Subjects also checkpoint and reopen at random, run transactions that are
//! then rolled back (which must leave no trace), and, in one mode, run into
//! their `max_memory` limit (the failing operation must leave no trace
//! either). A third graph, a plain paged `:memory:` twin fed the same
//! operations, is compared with the subject through the query engine.

use std::path::PathBuf;

use glider::graph::{Dir, Graph, OpenOptions};
use glider::legacy::graph::Graph as Legacy;
use glider::store::Sync;
use glider::types::Error;
use glider::value::Value;
use glider::query;

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-engine-{}-{}.db", name, std::process::id()));
    cleanup(&p);
    p
}

fn cleanup(p: &std::path::Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(glider::store::lock_path(p));
    for suffix in ["-wal", "-data"] {
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
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> Option<T> {
        if xs.is_empty() {
            None
        } else {
            Some(xs[self.below(xs.len() as u64) as usize])
        }
    }
}

const LABELS: &[&str] = &["A", "B", "C"];
const TYPES: &[&str] = &["X", "Y"];
const KEYS: &[&str] = &["k", "name", "w", "tag", "blob"];

fn value(r: &mut Rng) -> Value {
    match r.below(10) {
        0 => Value::Null,
        1 => Value::Bool(r.chance(50)),
        // A small range, so index buckets collide.
        2 | 3 => Value::Int(r.below(6) as i64 - 2),
        4 => Value::Float([0.5, -0.0, 2.0, f64::NAN, 1e300][r.below(5) as usize]),
        5 => Value::Int([i64::MIN, i64::MAX][r.below(2) as usize]),
        6 => Value::Text(["", "ada", "bob", "ünï", "a\"b"][r.below(5) as usize].into()),
        7 => Value::List(vec![Value::Int(1), Value::Text("x".into())]),
        // Long text: overflows a 1 KiB page, and truncates in an index key.
        8 => Value::Text(format!("{}-{}", "x".repeat(400 + r.below(2000) as usize), r.below(3))),
        _ => Value::List(vec![Value::List(vec![Value::Null]), Value::Float(-1.5)]),
    }
}

fn props(r: &mut Rng) -> Vec<(String, Value)> {
    let mut v = Vec::new();
    for k in KEYS {
        if r.chance(40) {
            v.push((k.to_string(), value(r)));
        }
    }
    v
}

/// One mutation, as data, so it can be applied to several graphs.
#[derive(Clone, Debug)]
enum Act {
    AddNode(Vec<String>, Vec<(String, Value)>),
    AddEdge(u64, u64, &'static str, Vec<(String, Value)>),
    DelNode(u64),
    DelEdge(u64),
    SetNode(u64, &'static str, Value),
    UnsetNode(u64, &'static str),
    SetEdge(u64, &'static str, Value),
    UnsetEdge(u64, &'static str),
    AddLabel(u64, &'static str),
    DelLabel(u64, &'static str),
    Index(&'static str, &'static str),
    DropIndex(&'static str, &'static str),
    Clear,
}

fn gen(r: &mut Rng, nodes: &[u64], edges: &[u64]) -> Option<Act> {
    let label = LABELS[r.below(3) as usize];
    let key = KEYS[r.below(KEYS.len() as u64) as usize];
    Some(match r.below(100) {
        0..=24 => {
            let labels = LABELS.iter().filter(|_| r.chance(45)).map(|s| s.to_string()).collect();
            Act::AddNode(labels, props(r))
        }
        25..=44 => Act::AddEdge(r.pick(nodes)?, r.pick(nodes)?, TYPES[r.below(2) as usize], props(r)),
        45..=50 => Act::DelNode(r.pick(nodes)?),
        51..=56 => Act::DelEdge(r.pick(edges)?),
        57..=68 => Act::SetNode(r.pick(nodes)?, key, value(r)),
        69..=72 => Act::UnsetNode(r.pick(nodes)?, key),
        73..=78 => Act::SetEdge(r.pick(edges)?, key, value(r)),
        79..=81 => Act::UnsetEdge(r.pick(edges)?, key),
        82..=86 => Act::AddLabel(r.pick(nodes)?, label),
        87..=90 => Act::DelLabel(r.pick(nodes)?, label),
        91..=94 => Act::Index(label, key),
        95..=96 => Act::DropIndex(label, key),
        97 if r.chance(10) => Act::Clear,
        _ => return None,
    })
}

/// The API both engines share, for this test.
trait G {
    fn act(&mut self, a: &Act) -> Result<(), Error>;
    fn observe(&self) -> Vec<(String, String)>;
}

macro_rules! impl_g {
    ($t:ty) => {
        impl G for $t {
            fn act(&mut self, a: &Act) -> Result<(), Error> {
                match a.clone() {
                    Act::AddNode(l, p) => self.add_node(&l, p).map(|_| ()),
                    Act::AddEdge(x, y, t, p) => self.add_edge(x, y, t, p).map(|_| ()),
                    Act::DelNode(n) => self.delete_node(n).map(|_| ()),
                    Act::DelEdge(e) => self.delete_edge(e).map(|_| ()),
                    Act::SetNode(n, k, v) => self.set_node_prop(n, k, v),
                    Act::UnsetNode(n, k) => self.unset_node_prop(n, k),
                    Act::SetEdge(e, k, v) => self.set_edge_prop(e, k, v),
                    Act::UnsetEdge(e, k) => self.unset_edge_prop(e, k),
                    Act::AddLabel(n, l) => self.add_label(n, l),
                    Act::DelLabel(n, l) => self.remove_label(n, l),
                    Act::Index(l, k) => self.create_index(l, k),
                    Act::DropIndex(l, k) => self.drop_index(l, k),
                    Act::Clear => self.clear(),
                }
            }
            fn observe(&self) -> Vec<(String, String)> {
                let g = self;
                let mut out = Vec::new();
                let mut put = |w: String, v: String| out.push((w, v));
                let nodes = g.node_ids();
                let edges = g.edge_ids();
                put("node_ids".into(), dbg(&nodes));
                put("edge_ids".into(), dbg(&edges));
                put("counts".into(), dbg((g.node_count(), g.edge_count())));
                let tname = |t: u32| g.strings.name(t).to_string();
                for &n in &nodes {
                    let mut labels = g.node_labels(n);
                    labels.sort();
                    put(format!("labels {n}"), dbg(labels));
                    put(format!("props {n}"), sorted(g.node_props(n)));
                    for k in KEYS {
                        put(format!("prop {n}.{k}"), dbg(g.node_prop(n, k)));
                    }
                    // Adjacency order is not part of the contract (the paged
                    // engine groups a node's edges by type), so compare sets.
                    for dir in [Dir::Out, Dir::In, Dir::Both] {
                        let mut adj: Vec<_> = g
                            .neighbors(n, dir, None)
                            .iter()
                            .map(|a| (a.edge, a.other, tname(a.etype)))
                            .collect();
                        adj.sort();
                        put(format!("adj {n} {dir:?}"), dbg(adj));
                        put(format!("deg {n} {dir:?}"), dbg(g.degree(n, dir)));
                    }
                    if let Some(t) = g.strings.lookup("X") {
                        let mut adj: Vec<_> = g.neighbors(n, Dir::Both, Some(t)).iter().map(|a| a.edge).collect();
                        adj.sort();
                        put(format!("adj {n} X"), dbg(adj));
                    }
                }
                for &e in &edges {
                    let r = g.edge(e).unwrap();
                    put(format!("edge {e}"), dbg((r.from, r.to, g.edge_type_name(e))));
                    put(format!("eprops {e}"), sorted(g.edge_props(e)));
                    for k in KEYS {
                        put(format!("eprop {e}.{k}"), dbg(g.edge_prop(e, k)));
                    }
                }
                for l in LABELS {
                    put(format!("label {l}"), dbg(g.nodes_with_label(l)));
                    put(format!("label_count {l}"), dbg(g.label_count(l)));
                    for k in KEYS {
                        put(format!("has_index {l}.{k}"), dbg(g.has_index(l, k)));
                        if !g.has_index(l, k) {
                            continue;
                        }
                        let probes = [
                            Value::Null,
                            Value::Int(0),
                            Value::Int(1),
                            Value::Float(2.0),
                            Value::Text("ada".into()),
                            Value::Bool(true),
                            Value::Text(format!("{}-1", "x".repeat(500))),
                        ];
                        for v in probes {
                            put(
                                format!("lookup {l}.{k}={v:?}"),
                                dbg((g.indexed_lookup(l, k, &v), g.index_count(l, k, &v))),
                            );
                        }
                    }
                }
                for t in TYPES {
                    put(format!("type {t}"), dbg(g.edges_with_type(t)));
                }
                let s = g.stats();
                put("stats".into(), dbg((s.nodes, s.edges, &s.labels, &s.edge_types, &s.indexes)));
                let (sample, _) = g.sample_keys(usize::MAX);
                put("sample_keys".into(), dbg(sample));
                let csr = g.csr(Dir::Out, None, Some("w"));
                // Each node's neighbour list, in edge-id order.
                let mut rows = Vec::new();
                for v in 0..csr.ids.len() {
                    let (a, b) = (csr.off[v] as usize, csr.off[v + 1] as usize);
                    let mut seg: Vec<_> = (a..b)
                        .map(|i| (csr.eids[i], csr.adj[i], csr.weights[i].to_bits()))
                        .collect();
                    seg.sort();
                    rows.push(seg);
                }
                put("csr".into(), dbg((&csr.ids, &csr.off, rows)));
                out
            }
        }
    };
}

impl_g!(Graph);
impl_g!(Legacy);

fn dbg<T: std::fmt::Debug>(x: T) -> String {
    format!("{:?}", x)
}

fn sorted(mut p: Vec<(String, Value)>) -> String {
    p.sort_by(|a, b| a.0.cmp(&b.0));
    dbg(p)
}

fn assert_same(a: &dyn G, b: &dyn G, ctx: &str) {
    let x = a.observe();
    let y = b.observe();
    for (p, q) in x.iter().zip(y.iter()) {
        assert_eq!(p.0, q.0, "{ctx}: snapshots diverged in shape");
        assert_eq!(p.1, q.1, "{ctx}: {} differs", p.0);
    }
    assert_eq!(x.len(), y.len(), "{ctx}: snapshot lengths differ");
}

/// Read-only queries, compared between two paged graphs.
fn queries_same(a: &mut Graph, b: &mut Graph, ctx: &str) {
    for q in [
        "MATCH (a:A)-[r:X]->(b) RETURN id(a), id(r), id(b), b.k ORDER BY id(r)",
        "MATCH (n) WHERE n.k = 0 RETURN id(n) ORDER BY id(n)",
        "MATCH (n:B {name: \"ada\"}) RETURN id(n) ORDER BY id(n)",
        "MATCH (n:C) RETURN n.tag, count(n) ORDER BY n.tag",
        "MATCH (a)-[*1..3]->(b) WHERE id(a) = 1 RETURN id(b) ORDER BY id(b)",
        "CALL pagerank(iterations: 5, top: 5)",
        "CALL wcc(top: 3)",
    ] {
        let x = query::execute(a, q).map(|r| dbg(&r.rows)).map_err(|e| e.to_string());
        let y = query::execute(b, q).map(|r| dbg(&r.rows)).map_err(|e| e.to_string());
        assert_eq!(x, y, "{ctx}: {q}");
    }
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    /// `:memory:`, 1 KiB pages.
    Memory,
    /// File-backed, 1 KiB pages, a 16-page cache: constant eviction.
    File,
    /// As File, plus random checkpoints and reopens.
    Reopen,
    /// `:memory:` against a tight `max_memory`.
    Full,
}

fn open_file(path: &std::path::Path) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            sync: Sync::Off,
            page_size: 1024,
            cache_size: 16 * 1024,
            segment_size: 256 * 1024,
            checkpoint_bytes: 64 * 1024,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

/// Returns how many operations hit the memory limit.
fn run(seed: u64, mode: Mode, steps: usize) -> u64 {
    let path = temp(&format!("{mode:?}-{seed}"));
    let mut oracle = Legacy::memory();
    let mut twin = Graph::memory();
    let mut subject = match mode {
        Mode::Memory => Graph::memory_with_page_size(1024, u64::MAX),
        Mode::Full => Graph::memory_with_page_size(1024, 24 * 1024),
        Mode::File | Mode::Reopen => open_file(&path),
    };
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut fulls = 0u64;

    for i in 0..steps {
        let nodes = oracle.node_ids();
        let edges = oracle.edge_ids();
        if let Some(a) = gen(&mut r, &nodes, &edges) {
            match subject.act(&a) {
                Ok(()) => {
                    oracle.act(&a).unwrap();
                    twin.act(&a).unwrap();
                }
                Err(Error::Full(_)) if matches!(mode, Mode::Full) => {
                    // Rolled back: the others must not see it either. Make
                    // room so the run keeps going.
                    fulls += 1;
                    if let Some(n) = r.pick(&nodes) {
                        let del = Act::DelNode(n);
                        if subject.act(&del).is_ok() {
                            oracle.act(&del).unwrap();
                            twin.act(&del).unwrap();
                        }
                    }
                }
                Err(e) => panic!("seed {seed} {mode:?} step {i}: {a:?}: {e}"),
            }
        }
        // A transaction that is rolled back leaves nothing behind.
        if r.chance(3) {
            subject.autocommit = false;
            for _ in 0..1 + r.below(8) {
                let (n, e) = (subject.node_ids(), subject.edge_ids());
                if let Some(a) = gen(&mut r, &n, &e) {
                    let _ = subject.act(&a);
                }
            }
            subject.rollback().unwrap();
            subject.autocommit = true;
        }
        if matches!(mode, Mode::Reopen) && r.chance(3) {
            if r.chance(50) {
                subject.checkpoint().unwrap();
            }
            drop(subject);
            subject = open_file(&path);
        }
        if i % 40 == 39 {
            let ctx = format!("seed {seed} {mode:?} step {i}");
            assert_same(&oracle, &subject, &ctx);
            queries_same(&mut subject, &mut twin, &ctx);
            subject.verify_trees().unwrap();
        }
    }
    let ctx = format!("seed {seed} {mode:?} end");
    assert_same(&oracle, &subject, &ctx);
    // A full graph accepts writes again once its limit is raised.
    subject.set_max_memory(u64::MAX);
    assert_eq!(
        oracle.add_node(&[], vec![]).unwrap(),
        subject.add_node(&[], vec![]).unwrap(),
        "{ctx}: next id"
    );
    drop(subject);
    cleanup(&path);
    fulls
}

/// `GLIDER_DIFF_SCALE=50 cargo test --release --test engine_differential`
/// runs fifty times the seeds, for a soak.
fn seeds(first: u64, count: u64) -> std::ops::Range<u64> {
    let scale: u64 = std::env::var("GLIDER_DIFF_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let first = first * 1_000_000;
    first..first + count * scale.max(1)
}

#[test]
fn memory_graph_matches_oracle() {
    for seed in seeds(1, 4) {
        run(seed, Mode::Memory, 400);
    }
}

#[test]
fn file_graph_with_a_tiny_cache_matches_oracle() {
    for seed in seeds(2, 3) {
        run(seed, Mode::File, 400);
    }
}

#[test]
fn reopened_graph_matches_oracle() {
    for seed in seeds(3, 3) {
        run(seed, Mode::Reopen, 400);
    }
}

#[test]
fn a_full_memory_graph_rolls_back_and_matches_oracle() {
    let mut fulls = 0;
    for seed in seeds(4, 3) {
        fulls += run(seed, Mode::Full, 600);
    }
    assert!(fulls > 0, "the memory limit was never reached");
}

/// A graph bulk-loaded from a stream (external sorts, bottom-up builds)
/// answers exactly like the graph it was streamed from.
#[test]
fn bulk_load_matches_its_source() {
    for seed in seeds(5, 4) {
        let mut oracle = Legacy::memory();
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        for _ in 0..500 {
            let (n, e) = (oracle.node_ids(), oracle.edge_ids());
            if let Some(a) = gen(&mut r, &n, &e) {
                if !matches!(a, Act::Clear) {
                    oracle.act(&a).unwrap();
                }
            }
        }
        let path = temp(&format!("bulk-{seed}"));
        let opts = OpenOptions {
            sync: Sync::Off,
            page_size: 1024,
            cache_size: 32 * 1024,
            ..OpenOptions::default()
        };
        // A tiny work_mem forces every sort to spill and merge.
        let mut g = Graph::bulk_load(&path, &oracle, opts.clone(), 64 << 10).unwrap();
        let ctx = format!("seed {seed} bulk");
        assert_same(&oracle, &g, &ctx);
        g.verify_trees().unwrap();
        // It is an ordinary database afterwards: writable, reopenable.
        let a = Act::AddNode(vec!["A".into()], vec![("k".into(), Value::Int(0))]);
        oracle.act(&a).unwrap();
        g.act(&a).unwrap();
        drop(g);
        let g = Graph::open_opts(&path, opts).unwrap();
        assert_same(&oracle, &g, &format!("{ctx} reopened"));
        drop(g);
        cleanup(&path);
    }
}
