//! A `:memory:` database, replicated: its log, its bases, and the way back.
//!
//! A memory graph given a log logs exactly what a file logs, at the same
//! positions. A snapshot taken at any moment — between transactions or
//! inside one — writes an image that opens as a file and equals the graph
//! as it was then. Images, and a file's own files, load back into memory;
//! and log shipped after a base, replayed into memory, gives the graph that
//! opening the same files gives. The method is `engine_differential`'s:
//! random operations, and every observable read compared.

use std::path::{Path, PathBuf};

use glider::graph::{Dir, Graph, OpenOptions};
use glider::query;
use glider::storage::log::{self, Committed};
use glider::storage::pager::{self, Pager, Snapshot};
use glider::store::Sync;
use glider::types::Error;
use glider::value::Value;

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "glider-memimg-{}-{}.gldb",
        name,
        std::process::id()
    ));
    cleanup(&p);
    p
}

fn cleanup(p: &Path) {
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

/// `GLIDER_DIFF_SCALE=50` runs fifty times the seeds, as for
/// `engine_differential`.
fn seeds(first: u64, count: u64) -> std::ops::Range<u64> {
    let scale: u64 = std::env::var("GLIDER_DIFF_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let first = first * 1_000_000;
    first..first + count * scale.max(1)
}

// ------------------------------------------------------------- operations

const LABELS: &[&str] = &["A", "B", "C"];
const TYPES: &[&str] = &["X", "Y"];
const KEYS: &[&str] = &["k", "name", "w", "tag", "blob"];

fn value(r: &mut Rng) -> Value {
    match r.below(10) {
        0 => Value::Null,
        1 => Value::Bool(r.chance(50)),
        2 | 3 => Value::Int(r.below(6) as i64 - 2),
        4 => Value::Float([0.5, -0.0, 2.0, f64::NAN, 1e300][r.below(5) as usize]),
        5 => Value::Int([i64::MIN, i64::MAX][r.below(2) as usize]),
        6 => Value::Text(["", "ada", "bob", "ünï", "a\"b"][r.below(5) as usize].into()),
        7 => Value::List(vec![Value::Int(1), Value::Text("x".into())]),
        // Long text: overflows a 1 KiB page, and truncates in an index key.
        8 => Value::Text(format!(
            "{}-{}",
            "x".repeat(400 + r.below(2000) as usize),
            r.below(3)
        )),
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
            let labels = LABELS
                .iter()
                .filter(|_| r.chance(45))
                .map(|s| s.to_string())
                .collect();
            Act::AddNode(labels, props(r))
        }
        25..=44 => Act::AddEdge(
            r.pick(nodes)?,
            r.pick(nodes)?,
            TYPES[r.below(2) as usize],
            props(r),
        ),
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

fn act(g: &mut Graph, a: &Act) -> Result<(), Error> {
    match a.clone() {
        Act::AddNode(l, p) => g.add_node(&l, p).map(|_| ()),
        Act::AddEdge(x, y, t, p) => g.add_edge(x, y, t, p).map(|_| ()),
        Act::DelNode(n) => g.delete_node(n).map(|_| ()),
        Act::DelEdge(e) => g.delete_edge(e).map(|_| ()),
        Act::SetNode(n, k, v) => g.set_node_prop(n, k, v),
        Act::UnsetNode(n, k) => g.unset_node_prop(n, k),
        Act::SetEdge(e, k, v) => g.set_edge_prop(e, k, v),
        Act::UnsetEdge(e, k) => g.unset_edge_prop(e, k),
        Act::AddLabel(n, l) => g.add_label(n, l),
        Act::DelLabel(n, l) => g.remove_label(n, l),
        Act::Index(l, k) => g.create_index(l, k),
        Act::DropIndex(l, k) => g.drop_index(l, k),
        Act::Clear => g.clear(),
    }
}

/// One random operation, on the graph as it stands (its open transaction
/// included).
fn step(g: &mut Graph, r: &mut Rng) -> Option<Act> {
    let (nodes, edges) = (g.node_ids(), g.edge_ids());
    let a = gen(r, &nodes, &edges)?;
    act(g, &a).unwrap_or_else(|e| panic!("{a:?}: {e}"));
    Some(a)
}

// ------------------------------------------------------------ observation

/// Everything a graph answers: the reads `engine_differential` compares,
/// and a few queries through the query engine.
type View = (Vec<(String, String)>, Vec<String>);

fn dbg<T: std::fmt::Debug>(x: T) -> String {
    format!("{:?}", x)
}

fn sorted(mut p: Vec<(String, Value)>) -> String {
    p.sort_by(|a, b| a.0.cmp(&b.0));
    dbg(p)
}

fn observe(g: &Graph) -> Vec<(String, String)> {
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
            let mut adj: Vec<_> = g
                .neighbors(n, Dir::Both, Some(t))
                .iter()
                .map(|a| a.edge)
                .collect();
            adj.sort();
            put(format!("adj {n} X"), dbg(adj));
        }
    }
    for &e in &edges {
        let r = g.edge(e).unwrap();
        put(
            format!("edge {e}"),
            dbg((r.from, r.to, g.edge_type_name(e))),
        );
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
    put(
        "stats".into(),
        dbg((s.nodes, s.edges, &s.labels, &s.edge_types, &s.indexes)),
    );
    let (sample, _) = g.sample_keys(usize::MAX);
    put("sample_keys".into(), dbg(sample));
    let csr = g.csr(Dir::Out, None, Some("w"));
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

fn view(g: &mut Graph) -> View {
    let queries = [
        "MATCH (a:A)-[r:X]->(b) RETURN id(a), id(r), id(b), b.k ORDER BY id(r)",
        "MATCH (n) WHERE n.k = 0 RETURN id(n) ORDER BY id(n)",
        "MATCH (n:B {name: \"ada\"}) RETURN id(n) ORDER BY id(n)",
        "MATCH (n:C) RETURN n.tag, count(n) ORDER BY n.tag",
        "MATCH (a)-[*1..3]->(b) WHERE id(a) = 1 RETURN id(b) ORDER BY id(b)",
        "CALL pagerank(iterations: 5, top: 5)",
        "CALL wcc(top: 3)",
    ]
    .iter()
    .map(|q| {
        query::execute(g, q)
            .map(|r| dbg(&r.rows))
            .unwrap_or_else(|e| e.to_string())
    })
    .collect();
    (observe(g), queries)
}

fn assert_view(want: &View, g: &mut Graph, ctx: &str) {
    assert!(g.integrity_error().is_none(), "{ctx}: a read hit damage");
    let got = view(g);
    for (p, q) in want.0.iter().zip(got.0.iter()) {
        assert_eq!(p.0, q.0, "{ctx}: diverged in shape");
        assert_eq!(p.1, q.1, "{ctx}: {} differs", p.0);
    }
    assert_eq!(
        want.0.len(),
        got.0.len(),
        "{ctx}: observation lengths differ"
    );
    assert_eq!(want.1, got.1, "{ctx}: queries differ");
    assert!(g.integrity_error().is_none(), "{ctx}: a read hit damage");
}

// ------------------------------------------------------------------ files

fn open_file(path: &Path, sync: Sync) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            sync,
            page_size: 1024,
            cache_size: 16 * 1024,
            // Eight pages a segment: a small database spans many files.
            segment_size: 8 * 1024,
            // Checkpoints only when a test asks for one, so log piles up
            // after them.
            checkpoint_bytes: 1 << 40,
            wal_segment: 8 << 10,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

/// Lay an image out as a database file and its segment files.
fn write_image(s: &Snapshot, seg_pages: u64, path: &Path) -> usize {
    cleanup(path);
    let mut segments = 0;
    s.write_image(seg_pages, &mut |n, bytes| {
        assert_eq!(n, segments, "segments arrive in order");
        let f = pager::segment_path(path, n);
        if let Some(dir) = f.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(f, bytes)?;
        segments += 1;
        Ok(())
    })
    .unwrap();
    segments
}

/// A file database's segment files, read as bytes: as many as its
/// superblock's extent covers (a segment never written reads as empty).
fn file_parts(path: &Path) -> Vec<Vec<u8>> {
    let sb = pager::read_superblock(path).unwrap();
    let n = sb.high_water.div_ceil(sb.seg_pages) as usize;
    (0..n)
        .map(|i| std::fs::read(pager::segment_path(path, i)).unwrap_or_default())
        .collect()
}

/// A file database's log segments, read as bytes, with their positions.
fn wal_segments(path: &Path) -> Vec<(u64, Vec<u8>)> {
    log::segments(path)
        .unwrap()
        .into_iter()
        .map(|at| {
            let f = log::wal_dir(path).join(format!("{at:016x}.wal"));
            (at, std::fs::read(f).unwrap())
        })
        .collect()
}

fn refs(parts: &[Vec<u8>]) -> Vec<&[u8]> {
    parts.iter().map(|p| p.as_slice()).collect()
}

fn seg_refs(segs: &[(u64, Vec<u8>)]) -> Vec<(u64, &[u8])> {
    segs.iter().map(|(at, b)| (*at, b.as_slice())).collect()
}

/// A memory graph with 1 KiB pages, so splits and overflow chains happen on
/// small graphs, and a log.
fn logged_memory() -> Graph {
    let mut g = Graph::memory_with_page_size(1024, u64::MAX);
    g.enable_log(log::DEFAULT_LOG_CAP).unwrap();
    g
}

// ------------------------------------------------------------------ tests

#[test]
fn a_memory_graph_given_a_log_logs_what_a_file_logs_and_is_otherwise_unchanged() {
    for seed in seeds(1, 2) {
        let path = temp(&format!("same-{seed}"));
        let mut file = open_file(&path, Sync::Normal);
        let mut logged = logged_memory();
        let mut plain = Graph::memory_with_page_size(1024, u64::MAX);
        assert_eq!(plain.log_span(), None, "no log unless asked");
        assert_ne!(logged.generation(), [0; 16], "a log brings a generation");
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        // Index upkeep walks a HashMap, whose order differs from one graph
        // to the next, so once there are indexes two graphs fed the same
        // operations may split their pages differently (the same contents,
        // laid out apart) with or without a log. So indexes come only after
        // the first 60 steps, and pages are compared until the first one.
        let (mut indexed, mut pages_compared) = (false, 0);
        for i in 0..300 {
            // The same operations, in the same transactions, on all three.
            let txn = r.chance(30);
            let commit = r.chance(70);
            for g in [&mut file, &mut logged, &mut plain] {
                g.autocommit = !txn;
            }
            for _ in 0..if txn { 1 + r.below(6) } else { 1 } {
                let (n, e) = (logged.node_ids(), logged.edge_ids());
                let Some(a) = gen(&mut r, &n, &e) else {
                    continue;
                };
                if matches!(a, Act::Index(..)) {
                    if i < 60 {
                        continue;
                    }
                    indexed = true;
                }
                for g in [&mut file, &mut logged, &mut plain] {
                    act(g, &a).unwrap();
                }
            }
            if txn {
                for g in [&mut file, &mut logged, &mut plain] {
                    if commit {
                        g.commit().unwrap();
                    } else {
                        g.rollback().unwrap();
                    }
                    g.autocommit = true;
                }
            }
            // Same positions, same bytes.
            let Committed::To(end) = log::committed_end(&path, 0).unwrap() else {
                panic!("the file's log is gone");
            };
            let (start, committed) = logged.log_span().unwrap();
            assert_eq!((start, committed), (0, end), "seed {seed} step {i}");
            // Logging changes no page.
            if !indexed {
                assert_eq!(
                    logged.stats().memory,
                    plain.stats().memory,
                    "seed {seed} step {i}"
                );
                pages_compared += 1;
            }
            if i % 25 == 24 {
                let mut bytes = Vec::new();
                log::copy_range(&path, 0, end, &mut bytes).unwrap();
                assert_eq!(
                    logged.log_range(0, end).unwrap(),
                    bytes,
                    "seed {seed} step {i}"
                );
                assert!(log::is_whole_transactions(&bytes));
                let want = view(&mut plain);
                assert_view(&want, &mut logged, &format!("seed {seed} step {i}"));
            }
        }
        assert!(
            pages_compared >= 60,
            "seed {seed}: pages compared {pages_compared} times"
        );
        drop(file);
        cleanup(&path);
    }
}

#[test]
fn snapshots_taken_at_random_moments_are_the_graph_as_it_was_then() {
    for seed in seeds(2, 3) {
        let mut g = logged_memory();
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let path = temp(&format!("snap-{seed}"));
        let mut taken = 0;
        let mut inside = 0;
        // Take a snapshot and check its image three ways: opened as a file,
        // loaded back into memory, and (one segment) by the browser's
        // reader.
        let check = |g: &Graph, want: &View, r: &mut Rng, ctx: &str| {
            let s = g.snapshot().unwrap();
            assert_eq!(
                s.lsn(),
                g.log_span().unwrap().1,
                "{ctx}: at the last commit"
            );
            assert_eq!(s.generation(), g.generation());
            let seg_pages = [4, 16, 1 << 20][r.below(3) as usize];
            let segments = write_image(&s, seg_pages, &path);
            drop(s);
            let mut f = open_file(&path, Sync::Off);
            f.verify_trees().unwrap();
            assert_view(want, &mut f, &format!("{ctx}: as a file"));
            assert_eq!(f.generation(), g.generation());
            drop(f);
            let parts = file_parts(&path);
            assert_eq!(parts.len(), segments);
            let (mut m, rec) = Graph::from_replica(&refs(&parts), &[], u64::MAX, 1 << 20).unwrap();
            m.verify_trees().unwrap();
            assert_view(want, &mut m, &format!("{ctx}: back in memory"));
            assert_eq!(
                (rec.end, m.generation()),
                (g.log_span().unwrap().1, g.generation())
            );
            if segments == 1 {
                let mut b = Graph::from_bytes(&parts[0]).unwrap();
                assert_view(want, &mut b, &format!("{ctx}: from bytes"));
            }
            cleanup(&path);
        };
        for i in 0..300 {
            let ctx = format!("seed {seed} step {i}");
            if r.chance(25) {
                // A transaction, committed or not, with snapshots inside it:
                // they hold the state it started from.
                let before = view(&mut g);
                g.autocommit = false;
                let commit = r.chance(60);
                for _ in 0..1 + r.below(6) {
                    step(&mut g, &mut r);
                    if r.chance(10) {
                        check(&g, &before, &mut r, &format!("{ctx} inside"));
                        taken += 1;
                        inside += 1;
                    }
                }
                if commit {
                    g.commit().unwrap();
                } else {
                    g.rollback().unwrap();
                }
                g.autocommit = true;
            } else {
                step(&mut g, &mut r);
            }
            if r.chance(4) {
                let now = view(&mut g);
                check(&g, &now, &mut r, &ctx);
                taken += 1;
            }
        }
        assert!(
            taken >= 10 && inside >= 2,
            "seed {seed}: {taken} snapshots, {inside} inside"
        );
    }
}

#[test]
fn an_image_opened_as_a_file_fills_its_free_pages_before_it_grows() {
    let mut g = logged_memory();
    for i in 0..600i64 {
        g.add_node(
            &["A".into()],
            vec![
                ("k".into(), Value::Int(i)),
                ("pad".into(), Value::Text("p".repeat(200))),
            ],
        )
        .unwrap();
    }
    // The oldest go: whole leaves empty, below pages still in use.
    for id in g.node_ids() {
        if id <= 480 {
            g.delete_node(id).unwrap();
        }
    }
    let want = view(&mut g);
    let path = temp("reuse");
    write_image(&g.snapshot().unwrap(), 64, &path);
    let free = Pager::open(&path, 1 << 20).unwrap().free_pages();
    assert!(free >= 60, "the deletes left {free} pages free");
    let mut f = open_file(&path, Sync::Off);
    assert_view(&want, &mut f, "opened");
    let extent = f.file_len();
    // Writes that need fewer pages than are free: copy-on-write takes them
    // from the free list, and the file does not grow.
    for i in 0..3i64 {
        f.add_node(&["B".into()], vec![("k".into(), Value::Int(i))])
            .unwrap();
    }
    assert!(f.pager_stats().copies >= 3);
    assert_eq!(f.file_len(), extent, "the file grew while pages were free");
    f.verify_trees().unwrap();
    drop(f);
    cleanup(&path);
}

#[test]
fn a_file_databases_own_files_load_into_memory() {
    for seed in seeds(3, 2) {
        let path = temp(&format!("files-{seed}"));
        // A backup's hold, asked for before the writer opens (a new file's
        // log directory starts empty, so the file is made first): every
        // checkpoint then keeps the pages it frees, marked held.
        drop(open_file(&path, Sync::Off));
        if seed % 2 == 0 {
            glider::storage::db::write_pin(
                &path,
                glider::storage::db::Pin {
                    nonce: 7,
                    hold: true,
                    shipped: 0,
                },
            )
            .unwrap();
        }
        let mut g = open_file(&path, Sync::Off);
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut loaded = 0;
        for i in 0..300 {
            step(&mut g, &mut r);
            if r.chance(5) {
                g.checkpoint().unwrap();
                let parts = file_parts(&path);
                let want = view(&mut g);
                let (mut m, _) =
                    Graph::from_replica(&refs(&parts), &[], u64::MAX, 1 << 20).unwrap();
                m.verify_trees().unwrap();
                assert_view(&want, &mut m, &format!("seed {seed} step {i}"));
                assert_eq!(m.generation(), g.generation());
                loaded += 1;
            }
        }
        assert!(loaded >= 5);
        let sb = pager::read_superblock(&path).unwrap();
        assert!(sb.high_water > 2 * sb.seg_pages, "several segment files");
        assert_eq!(sb.hold, seed % 2 == 0);
        drop(g);
        cleanup(&path);
    }
}

#[test]
fn shipped_log_replayed_into_memory_equals_opening_the_same_files() {
    for seed in seeds(4, 3) {
        let path = temp(&format!("ship-{seed}"));
        let copy = temp(&format!("ship-copy-{seed}"));
        // Sync::Normal: each commit is on disk to be read.
        let mut g = open_file(&path, Sync::Normal);
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let (mut compared, mut replayed) = (0, 0);
        for i in 0..300 {
            if r.chance(20) {
                g.autocommit = false;
                for _ in 0..1 + r.below(5) {
                    step(&mut g, &mut r);
                }
                if r.chance(70) {
                    g.commit().unwrap();
                } else {
                    g.rollback().unwrap();
                }
                g.autocommit = true;
            } else {
                step(&mut g, &mut r);
            }
            if r.chance(3) {
                g.checkpoint().unwrap();
            }
            if !r.chance(7) {
                continue;
            }
            // The files as they are: the last checkpoint, and log after it.
            let ctx = format!("seed {seed} step {i}");
            let parts = file_parts(&path);
            let wal = wal_segments(&path);
            let want = view(&mut g);
            let from = pager::read_superblock(&path).unwrap().wal_lsn;
            let Committed::To(end) = log::committed_end(&path, from).unwrap() else {
                panic!("{ctx}: the log after the checkpoint is gone");
            };
            let (mut m, rec) =
                Graph::from_replica(&refs(&parts), &seg_refs(&wal), u64::MAX, 1 << 20).unwrap();
            m.verify_trees().unwrap();
            assert_view(&want, &mut m, &format!("{ctx}: replayed into memory"));
            assert_eq!(rec.end, end, "{ctx}");
            assert_eq!(m.log_span(), Some((end, end)));
            assert_eq!(m.generation(), g.generation());
            replayed += rec.transactions;
            // The same files, opened.
            for (n, b) in parts.iter().enumerate() {
                let f = pager::segment_path(&copy, n);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(f, b).unwrap();
            }
            std::fs::create_dir_all(log::wal_dir(&copy)).unwrap();
            for (at, b) in &wal {
                std::fs::write(log::wal_dir(&copy).join(format!("{at:016x}.wal")), b).unwrap();
            }
            let mut f = open_file(&copy, Sync::Off);
            assert_view(&want, &mut f, &format!("{ctx}: the file opened"));
            drop(f);
            cleanup(&copy);
            compared += 1;
        }
        assert!(compared >= 10, "seed {seed}: {compared} comparisons");
        assert!(
            replayed > 0,
            "seed {seed}: never any log after a checkpoint"
        );
        drop(g);
        cleanup(&path);
    }
}

#[test]
fn a_restored_graph_carries_the_lineage_and_the_log_on() {
    // A memory graph's base and the log it shipped after it.
    let mut src = logged_memory();
    let mut r = Rng(0x5EED);
    for _ in 0..150 {
        step(&mut src, &mut r);
    }
    let path = temp("carry");
    let base = src.snapshot().unwrap();
    let at = base.lsn();
    write_image(&base, 16, &path);
    drop(base);
    let parts = file_parts(&path);
    cleanup(&path);
    for _ in 0..150 {
        step(&mut src, &mut r);
    }
    let (_, shipped_to) = src.log_span().unwrap();
    let shipped = src.log_range(at, shipped_to).unwrap();
    src.release_log(shipped_to);
    let want = view(&mut src);

    let (mut m, rec) =
        Graph::from_replica(&refs(&parts), &[(at, &shipped[..])], u64::MAX, 1 << 20).unwrap();
    assert_view(&want, &mut m, "restored");
    assert_eq!((rec.end, rec.discarded), (shipped_to, 0));
    assert_eq!(m.generation(), src.generation(), "the replica's lineage");
    assert_eq!(m.log_span(), Some((shipped_to, shipped_to)));
    m.enable_log(1).unwrap();
    assert_eq!(m.generation(), src.generation(), "and it stays");

    // Written to, it logs from where the replay ended: its log follows the
    // shipped log, and the base plus both replays to it.
    for _ in 0..150 {
        step(&mut m, &mut r);
    }
    let (start, end) = m.log_span().unwrap();
    assert_eq!(start, shipped_to);
    assert!(end > start);
    let more = m.log_range(start, end).unwrap();
    assert!(log::is_whole_transactions(&more));
    let want = view(&mut m);
    let (mut again, rec) = Graph::from_replica(
        &refs(&parts),
        &[(at, &shipped[..]), (shipped_to, &more[..])],
        u64::MAX,
        1 << 20,
    )
    .unwrap();
    assert_eq!(rec.end, end);
    assert_view(&want, &mut again, "base and both logs");

    // And a snapshot of it is a base on the same lineage, as a file.
    let s = m.snapshot().unwrap();
    assert_eq!(s.lsn(), end);
    write_image(&s, 1 << 20, &path);
    drop(s);
    let mut f = open_file(&path, Sync::Off);
    f.verify_trees().unwrap();
    assert_view(&want, &mut f, "its snapshot as a file");
    assert_eq!(f.generation(), src.generation());
    drop(f);
    cleanup(&path);
}

#[test]
fn a_gap_in_the_shipped_log_ends_replay_there() {
    let mut g = logged_memory();
    let mut r = Rng(0x6A9);
    for _ in 0..50 {
        step(&mut g, &mut r);
    }
    let path = temp("gap");
    let base = g.snapshot().unwrap();
    let at = base.lsn();
    write_image(&base, 1 << 20, &path);
    drop(base);
    // The base is shipped: the log below it can go.
    g.release_log(at);
    let parts = file_parts(&path);
    cleanup(&path);
    // Three segments, shipped one after another.
    let mut segs: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut views = Vec::new();
    for _ in 0..3 {
        for _ in 0..40 {
            step(&mut g, &mut r);
        }
        let (start, end) = g.log_span().unwrap();
        assert_eq!(g.log_committed_end(start).unwrap(), Committed::To(end));
        segs.push((start, g.log_range(start, end).unwrap()));
        g.release_log(end);
        assert_eq!(g.log_committed_end(start).unwrap(), Committed::Gone);
        views.push(view(&mut g));
    }
    let restore = |segs: &[(u64, Vec<u8>)]| {
        Graph::from_replica(&refs(&parts), &seg_refs(segs), u64::MAX, 1 << 20).unwrap()
    };
    let (mut all, rec) = restore(&segs);
    assert_view(&views[2], &mut all, "all three");
    assert_eq!(rec.end, g.log_span().unwrap().1);
    // The second missing: replay stops at the end of the first, and the
    // third, past the gap, is not used.
    let (mut cut, rec) = restore(&[segs[0].clone(), segs[2].clone()]);
    assert_view(&views[0], &mut cut, "a gap after the first");
    assert_eq!(rec.end, segs[1].0);
    assert_eq!(cut.log_span(), Some((segs[1].0, segs[1].0)));
    // The first missing: nothing replays.
    let (mut none, rec) = restore(&segs[1..]);
    assert_eq!(rec.transactions, 0);
    assert_eq!(rec.end, at);
    let (mut base_only, _) = restore(&[]);
    let want = view(&mut base_only);
    assert_view(&want, &mut none, "a gap at the base");
}

#[test]
fn a_log_starts_between_transactions_and_only_in_memory() {
    let path = temp("enable");
    let mut file = open_file(&path, Sync::Off);
    assert!(file.enable_log(1 << 20).is_err(), "a file has its own log");
    assert!(file.snapshot().is_err());
    assert!(file.log_committed_end(0).is_err());
    drop(file);
    cleanup(&path);

    let mut g = Graph::memory_with_page_size(1024, u64::MAX);
    g.add_node(&["A".into()], vec![]).unwrap();
    assert_eq!(g.generation(), [0; 16]);
    g.autocommit = false;
    g.add_node(&["A".into()], vec![]).unwrap();
    assert!(g.enable_log(1 << 20).is_err(), "half a transaction logged");
    assert_eq!(g.log_span(), None);
    g.rollback().unwrap();
    g.autocommit = true;
    // The log starts where the graph's revision is: past the commit made
    // without a log, so the revision never goes back.
    let start = g.revision();
    assert!(start > 0);
    g.enable_log(1 << 20).unwrap();
    let lineage = g.generation();
    assert_ne!(lineage, [0; 16]);
    assert_eq!(g.log_span(), Some((start, start)));
    g.add_node(&["A".into()], vec![]).unwrap();
    let (_, end) = g.log_span().unwrap();
    // Enabled again, nothing changes.
    g.enable_log(1).unwrap();
    assert_eq!(
        (g.generation(), g.log_span()),
        (lineage, Some((start, end)))
    );
    assert_eq!(g.log_committed_end(start).unwrap(), Committed::To(end));
}

#[test]
fn the_log_is_dropped_past_its_cap_and_a_restore_that_does_not_fit_is_full() {
    // A log nobody ships outgrows its cap: what it held is gone.
    let mut g = Graph::memory_with_page_size(1024, u64::MAX);
    g.enable_log(4096).unwrap();
    let (start, _) = g.log_span().unwrap();
    for i in 0..200i64 {
        g.add_node(&["A".into()], vec![("k".into(), Value::Int(i))])
            .unwrap();
    }
    assert_eq!(g.log_committed_end(start).unwrap(), Committed::Gone);
    let (start, end) = g.log_span().unwrap();
    assert!(end - start <= 4096);
    // A graph without a log has none of this.
    let mut plain = Graph::memory();
    assert!(plain.snapshot().is_err());
    assert!(plain.log_range(0, 0).is_err());
    plain.release_log(10);
    assert_eq!(plain.log_span(), None);

    // A base too big for the memory it may have.
    let mut g = logged_memory();
    let path = temp("full");
    for i in 0..2000i64 {
        g.add_node(
            &["A".into()],
            vec![("pad".into(), Value::Text(format!("{i:0>300}")))],
        )
        .unwrap();
    }
    let s = g.snapshot().unwrap();
    write_image(&s, 1 << 20, &path);
    let parts = file_parts(&path);
    cleanup(&path);
    let need = g.stats().memory.unwrap().0;
    assert!(matches!(
        Graph::from_replica(&refs(&parts), &[], need / 2, 1 << 20),
        Err(Error::Full(_))
    ));
    // A base that fits, and log after it that does not.
    let (_, from) = g.log_span().unwrap();
    for i in 0..2000i64 {
        g.add_node(
            &["B".into()],
            vec![("pad".into(), Value::Text(format!("{i:0>300}")))],
        )
        .unwrap();
    }
    let (_, to) = g.log_span().unwrap();
    let log = g.log_range(from, to).unwrap();
    assert!(matches!(
        Graph::from_replica(&refs(&parts), &[(from, &log[..])], need + need / 4, 1 << 20),
        Err(Error::Full(_))
    ));
    let (m, _) =
        Graph::from_replica(&refs(&parts), &[(from, &log[..])], u64::MAX, 1 << 20).unwrap();
    assert_eq!(m.node_count(), g.node_count());
}
