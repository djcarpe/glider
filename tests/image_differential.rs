//! Differential tests for the snapshot image.
//!
//! A graph is an image plus a delta, and every read merges the two. The way to
//! trust that merge is to run the same random mutations against an *oracle*
//! (a memory graph that never has an image, so everything lives in the delta)
//! and a *subject* that folds its delta into an image at random moments — by
//! rebasing in memory, by compacting to disk, or by compacting and reopening —
//! and to require that every observable answer agrees, in order, after every
//! batch.

use std::path::PathBuf;

use glider::graph::{Dir, Graph};
use glider::store::Sync;
use glider::value::Value;
use glider::{query, OpenOptions, Residency};

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-diff-{}-{}.gldb", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(glider::store::lock_path(&p));
    p
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
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> Option<&'a T> {
        if xs.is_empty() {
            None
        } else {
            Some(&xs[self.below(xs.len() as u64) as usize])
        }
    }
}

const LABELS: &[&str] = &["A", "B", "C"];
const TYPES: &[&str] = &["X", "Y"];
const KEYS: &[&str] = &["k", "name", "w", "tag"];

fn value(r: &mut Rng) -> Value {
    match r.below(9) {
        0 => Value::Null,
        1 => Value::Bool(r.chance(50)),
        // A small range, so index buckets collide.
        2 | 3 => Value::Int(r.below(6) as i64 - 2),
        4 => Value::Float([0.5, -0.0, 2.0, f64::NAN, 1e300][r.below(5) as usize]),
        5 => Value::Int([i64::MIN, i64::MAX][r.below(2) as usize]),
        6 => Value::Text(["", "ada", "bob", "ünï", "a\"b"][r.below(5) as usize].into()),
        7 => Value::List(vec![Value::Int(1), Value::Text("x".into())]),
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

/// One random mutation, applied identically to both graphs. Ids line up
/// because both graphs allocate them the same way.
fn step(r: &mut Rng, graphs: &mut [&mut Graph]) {
    let nodes = graphs[0].node_ids();
    let edges = graphs[0].edge_ids();
    let choice = r.below(100);
    let label = LABELS[r.below(3) as usize];
    let key = KEYS[r.below(4) as usize];
    let v = value(r);
    macro_rules! each {
        ($g:ident => $e:expr) => {
            for $g in graphs.iter_mut() {
                $e;
            }
        };
    }
    match choice {
        0..=24 => {
            let labels: Vec<String> = LABELS
                .iter()
                .filter(|_| r.chance(45))
                .map(|s| s.to_string())
                .collect();
            let p = props(r);
            each!(g => g.add_node(&labels, p.clone()).unwrap());
        }
        25..=44 => {
            let (Some(&a), Some(&b)) = (r.pick(&nodes), r.pick(&nodes)) else {
                return;
            };
            let t = TYPES[r.below(2) as usize];
            let p = props(r);
            each!(g => g.add_edge(a, b, t, p.clone()).unwrap());
        }
        45..=50 => {
            let Some(&n) = r.pick(&nodes) else { return };
            each!(g => g.delete_node(n).unwrap());
        }
        51..=56 => {
            let Some(&e) = r.pick(&edges) else { return };
            each!(g => g.delete_edge(e).unwrap());
        }
        57..=68 => {
            let Some(&n) = r.pick(&nodes) else { return };
            each!(g => g.set_node_prop(n, key, v.clone()).unwrap());
        }
        69..=72 => {
            let Some(&n) = r.pick(&nodes) else { return };
            each!(g => g.unset_node_prop(n, key).unwrap());
        }
        73..=78 => {
            let Some(&e) = r.pick(&edges) else { return };
            each!(g => g.set_edge_prop(e, key, v.clone()).unwrap());
        }
        79..=81 => {
            let Some(&e) = r.pick(&edges) else { return };
            each!(g => g.unset_edge_prop(e, key).unwrap());
        }
        82..=86 => {
            let Some(&n) = r.pick(&nodes) else { return };
            each!(g => g.add_label(n, label).unwrap());
        }
        87..=90 => {
            let Some(&n) = r.pick(&nodes) else { return };
            each!(g => g.remove_label(n, label).unwrap());
        }
        91..=94 => each!(g => g.create_index(label, key).unwrap()),
        95..=96 => each!(g => g.drop_index(label, key).unwrap()),
        97 => {
            // Through the query engine, so its read paths see a merged graph.
            let q = format!("MATCH (n:{label}) SET n.{key} = 1");
            each!(g => query::execute(g, &q).unwrap());
        }
        98 if r.chance(15) => each!(g => g.clear().unwrap()),
        _ => {
            let q = format!("MATCH (a:{label})-[r]->(b) WHERE a.k = 0 DELETE r");
            each!(g => query::execute(g, &q).unwrap());
        }
    }
}

fn dbg<T: std::fmt::Debug>(x: T) -> String {
    format!("{:?}", x)
}

fn sorted_props(mut p: Vec<(String, Value)>) -> String {
    p.sort_by(|a, b| a.0.cmp(&b.0));
    dbg(p)
}

/// Everything observable about a graph, as one comparable string per item.
fn snapshot(g: &mut Graph) -> Vec<(String, String)> {
    // Queries first: `execute` takes `&mut`, the reads below only `&`.
    let mut queries = Vec::new();
    for q in [
        "MATCH (a:A)-[r:X]->(b) RETURN id(a), id(r), id(b), b.k ORDER BY id(r)",
        "MATCH (n) WHERE n.k = 0 RETURN id(n) ORDER BY id(n)",
        "MATCH (n:B {name: \"ada\"}) RETURN id(n) ORDER BY id(n)",
        "MATCH (n:C) RETURN n.tag, count(n) ORDER BY n.tag",
    ] {
        let r = query::execute(g, q).unwrap_or_else(|e| panic!("{q}: {e}"));
        queries.push((q.to_string(), dbg(&r.rows)));
    }
    let g: &Graph = g;
    let mut out = Vec::new();
    let mut put = |what: String, v: String| out.push((what, v));

    let nodes = g.node_ids();
    let edges = g.edge_ids();
    put("node_ids".into(), dbg(&nodes));
    put("edge_ids".into(), dbg(&edges));
    put("counts".into(), dbg((g.node_count(), g.edge_count())));
    for &n in &nodes {
        let mut labels = g.node_labels(n);
        labels.sort();
        put(format!("labels {n}"), dbg(labels));
        put(format!("props {n}"), sorted_props(g.node_props(n)));
        for k in KEYS {
            put(format!("prop {n}.{k}"), dbg(g.node_prop(n, k)));
        }
        for dir in [Dir::Out, Dir::In, Dir::Both] {
            put(format!("adj {n} {dir:?}"), dbg(g.neighbors(n, dir, None)));
            put(format!("deg {n} {dir:?}"), dbg(g.degree(n, dir)));
        }
        if let Some(t) = g.strings.lookup("X") {
            put(
                format!("adj {n} X"),
                dbg(g.neighbors(n, Dir::Both, Some(t))),
            );
        }
    }
    for &e in &edges {
        let r = g.edge(e).unwrap();
        put(
            format!("edge {e}"),
            dbg((r.from, r.to, g.edge_type_name(e))),
        );
        put(format!("eprops {e}"), sorted_props(g.edge_props(e)));
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
            let mut probe = vec![Value::Null, Value::Int(0), Value::Int(1), Value::Float(2.0)];
            probe.push(Value::Text("ada".into()));
            probe.push(Value::Bool(true));
            for v in probe {
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
    put(
        "stats".into(),
        dbg((s.nodes, s.edges, &s.labels, &s.edge_types, &s.indexes)),
    );
    let (sample_nodes, _) = g.sample_keys(usize::MAX);
    put("sample_keys".into(), dbg(sample_nodes));
    let csr = g.csr(Dir::Out, None, Some("w"));
    put("csr".into(), dbg((&csr.ids, &csr.off, &csr.adj, &csr.eids)));
    put(
        "csr weights".into(),
        dbg(csr.weights.iter().map(|w| w.to_bits()).collect::<Vec<_>>()),
    );
    out.extend(queries);
    out
}

fn assert_same(oracle: &mut Graph, subject: &mut Graph, ctx: &str) {
    let a = snapshot(oracle);
    let b = snapshot(subject);
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.0, y.0, "{ctx}: snapshots diverged in shape");
        assert_eq!(x.1, y.1, "{ctx}: {} differs", x.0);
    }
    assert_eq!(a.len(), b.len(), "{ctx}: snapshot lengths differ");
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Memory graph, delta folded into an in-memory image.
    Rebase,
    /// File graph, compacted to disk.
    Compact,
    /// File graph, compacted and reopened, properties in memory.
    Reopen,
    /// As `Reopen`, with properties left on disk behind a tiny cache.
    ReopenOnDisk,
}

fn run(seed: u64, mode: Mode, steps: usize) {
    let path = temp(&format!("{mode:?}-{seed}"));
    let opts = |residency| OpenOptions {
        sync: Sync::Off,
        residency,
        auto_compact: None,
        ..OpenOptions::default()
    };
    let residency = match mode {
        Mode::ReopenOnDisk => Residency::OnDisk { cache_bytes: 1 },
        _ => Residency::Memory,
    };
    let mut oracle = Graph::memory();
    let mut subject = match mode {
        Mode::Rebase => Graph::memory(),
        _ => Graph::open_opts(&path, opts(residency)).unwrap(),
    };
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);

    for i in 0..steps {
        step(&mut r, &mut [&mut oracle, &mut subject]);
        if r.chance(4) {
            match mode {
                Mode::Rebase => subject.rebase_in_memory().unwrap(),
                Mode::Compact => {
                    subject.compact().unwrap();
                }
                Mode::Reopen | Mode::ReopenOnDisk => {
                    if r.chance(50) {
                        subject.compact().unwrap();
                    }
                    subject.commit().unwrap();
                    drop(subject);
                    subject = Graph::open_opts(&path, opts(residency)).unwrap();
                }
            }
        }
        if i % 25 == 24 {
            assert_same(
                &mut oracle,
                &mut subject,
                &format!("seed {seed} {mode:?} step {i}"),
            );
        }
    }
    assert_same(
        &mut oracle,
        &mut subject,
        &format!("seed {seed} {mode:?} end"),
    );
    // The next id handed out agrees too.
    assert_eq!(
        oracle.add_node(&[], vec![]).unwrap(),
        subject.add_node(&[], vec![]).unwrap()
    );
    if !matches!(mode, Mode::Rebase) {
        subject.compact().unwrap();
        drop(subject);
        let v = glider::store::verify(&path).unwrap();
        assert!(v.bad_offset.is_none());
        assert!(matches!(v.image, Some(Ok(_))), "{:?}", v.image);
        let _ = std::fs::remove_file(&path);
    }
}

/// `GLIDER_DIFF_SCALE=50 cargo test --release --test image_differential`
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
fn rebased_graph_matches_oracle() {
    for seed in seeds(1, 6) {
        run(seed, Mode::Rebase, 400);
    }
}

#[test]
fn compacted_graph_matches_oracle() {
    for seed in seeds(2, 4) {
        run(seed, Mode::Compact, 400);
    }
}

#[test]
fn reopened_graph_matches_oracle() {
    for seed in seeds(3, 4) {
        run(seed, Mode::Reopen, 400);
    }
}

#[test]
fn reopened_graph_with_props_on_disk_matches_oracle() {
    for seed in seeds(4, 3) {
        run(seed, Mode::ReopenOnDisk, 300);
    }
}
