//! A `:memory:` database replicated through a backend, and restored from
//! one: the objects a file's replication makes, made from memory, and
//! restored to either place. Every combination — memory to memory, memory
//! to a file, a file to memory, a file to a file — carries its lineage on: a
//! further pass from the restored side ships only what it wrote since. A
//! pass holds the database only to capture and to acknowledge; a failure at
//! any upload loses nothing; the log cap costs a base, never a transaction;
//! a replicator looks before it ships; a restore stops at a gap or a time
//! and refuses a damaged base. Random workloads, compared as `memory_image`
//! and `paged_stream` compare: every node and edge with its ids, labels and
//! properties, the indexes, a few queries, and every tree walked.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use glider::graph::{Graph, OpenOptions};
use glider::query;
use glider::storage::log;
use glider::stream::backend::{Backend, Entry, FileBackend};
use glider::stream::config::ReplicaConfig;
use glider::stream::memory::{self, MemoryReplicator};
use glider::stream::paged::{self, PagedReplicator, SyncReport};
use glider::types::Error;
use glider::value::Value;

fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("glider-mstream-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
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

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
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
    fn pick(&mut self, xs: &[u64]) -> Option<u64> {
        if xs.is_empty() {
            None
        } else {
            Some(xs[self.below(xs.len() as u64) as usize])
        }
    }
}

// ----------------------------------------------------------------- workloads

const LABELS: &[&str] = &["A", "B", "C"];
const TYPES: &[&str] = &["X", "Y"];
const KEYS: &[&str] = &["k", "name", "tag", "blob"];

fn value(r: &mut Rng) -> Value {
    match r.below(9) {
        0 => Value::Null,
        1 => Value::Bool(r.chance(50)),
        2 | 3 => Value::Int(r.below(6) as i64 - 2),
        4 => Value::Float([0.5, -0.0, 2.0, 1e300][r.below(4) as usize]),
        5 => Value::Text(["", "ada", "bob", "ünï"][r.below(4) as usize].into()),
        6 => Value::List(vec![Value::Int(1), Value::Text("x".into())]),
        // Long text: overflows a 1 KiB page.
        7 => Value::Text("x".repeat(300 + r.below(1500) as usize)),
        _ => Value::Int(r.below(1000) as i64),
    }
}

fn props(r: &mut Rng) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for k in KEYS {
        if r.chance(40) {
            out.push((k.to_string(), value(r)));
        }
    }
    out
}

/// One random write, against the graph as it stands.
fn write(g: &mut Graph, r: &mut Rng) {
    let (nodes, edges) = (g.node_ids(), g.edge_ids());
    let label = LABELS[r.below(3) as usize];
    let key = KEYS[r.below(KEYS.len() as u64) as usize];
    let done = match r.below(100) {
        0..=29 => {
            let labels: Vec<String> = LABELS
                .iter()
                .filter(|_| r.chance(45))
                .map(|s| s.to_string())
                .collect();
            g.add_node(&labels, props(r)).map(|_| ())
        }
        30..=49 => match (r.pick(&nodes), r.pick(&nodes)) {
            (Some(a), Some(b)) => g
                .add_edge(a, b, TYPES[r.below(2) as usize], props(r))
                .map(|_| ()),
            _ => Ok(()),
        },
        50..=55 => match r.pick(&nodes) {
            Some(n) => g.delete_node(n).map(|_| ()),
            None => Ok(()),
        },
        56..=61 => match r.pick(&edges) {
            Some(e) => g.delete_edge(e).map(|_| ()),
            None => Ok(()),
        },
        62..=73 => match r.pick(&nodes) {
            Some(n) => g.set_node_prop(n, key, value(r)),
            None => Ok(()),
        },
        74..=77 => match r.pick(&nodes) {
            Some(n) => g.unset_node_prop(n, key),
            None => Ok(()),
        },
        78..=83 => match r.pick(&edges) {
            Some(e) => g.set_edge_prop(e, key, value(r)),
            None => Ok(()),
        },
        84..=88 => match r.pick(&nodes) {
            Some(n) => g.add_label(n, label),
            None => Ok(()),
        },
        89..=92 => match r.pick(&nodes) {
            Some(n) => g.remove_label(n, label),
            None => Ok(()),
        },
        93..=95 => g.create_index(label, key),
        96..=97 => g.drop_index(label, key),
        _ => Ok(()),
    };
    done.unwrap();
}

/// `n` writes, some of them grouped in transactions, a few rolled back.
fn rounds(g: &mut Graph, r: &mut Rng, n: usize) {
    let mut left = n;
    while left > 0 {
        if r.chance(15) {
            g.autocommit = false;
            for _ in 0..1 + r.below(5) {
                write(g, r);
            }
            if r.chance(70) {
                g.commit().unwrap();
            } else {
                g.rollback().unwrap();
            }
            g.autocommit = true;
        } else {
            write(g, r);
        }
        left -= 1;
    }
}

/// Everything compared: every node and edge, with ids, labels and
/// properties; the indexes; label and type counts; a few queries. Every
/// tree is walked first.
fn view(g: &mut Graph) -> String {
    g.verify_trees().unwrap();
    let mut out = query::export_jsonl(g);
    out += &format!("indexes {:?}\n", g.indexes());
    let s = g.stats();
    out += &format!("stats {:?}\n", (s.nodes, s.edges, &s.labels, &s.edge_types));
    for q in [
        "MATCH (a:A)-[r:X]->(b) RETURN id(a), id(r), id(b), b.k ORDER BY id(r)",
        "MATCH (n:B {name: \"ada\"}) RETURN id(n) ORDER BY id(n)",
        "MATCH (n:C) RETURN n.tag, count(n) ORDER BY n.tag",
        "MATCH (a)-[*1..2]->(b) WHERE id(a) = 1 RETURN id(b) ORDER BY id(b)",
    ] {
        let got = query::execute(g, q)
            .map(|r| format!("{:?}", r.rows))
            .unwrap_or_else(|e| e.to_string());
        out += &format!("{q}: {got}\n");
    }
    out
}

/// The two views are the same, or the first line that differs says where.
fn same(want: &str, got: &str, ctx: &str) {
    if want == got {
        return;
    }
    for (i, (a, b)) in want.lines().zip(got.lines()).enumerate() {
        assert_eq!(a, b, "{ctx}: line {i} differs");
    }
    assert_eq!(
        want.lines().count(),
        got.lines().count(),
        "{ctx}: one view is longer"
    );
}

// ----------------------------------------------------------------- databases

fn cfg() -> ReplicaConfig {
    ReplicaConfig {
        url: "test".into(),
        min_bytes: 0,
        sync_interval: Duration::from_millis(200),
        // Bases only when a test asks for one.
        snapshot_interval: Duration::from_secs(0),
        ..ReplicaConfig::default()
    }
}

/// A memory graph with 1 KiB pages, so splits and overflow chains happen on
/// small graphs, and a log.
fn logged() -> Graph {
    let mut g = Graph::memory_with_page_size(1024, u64::MAX);
    g.enable_log(log::DEFAULT_LOG_CAP).unwrap();
    g
}

fn mem_replicator(backend: Arc<dyn Backend>) -> MemoryReplicator {
    let mut r = MemoryReplicator::new(backend, cfg());
    // Sixteen pages a segment file: a base of a small graph is several.
    r.base_segment_bytes = 16 << 10;
    r
}

fn open_file(path: &Path) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            page_size: 1024,
            cache_size: 64 << 10,
            segment_size: 16 << 10,
            checkpoint_bytes: 8 << 10,
            wal_segment: 4 << 10,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Memory,
    File,
}

/// A database of either kind, replicated to one backend.
enum Side {
    Memory {
        g: Mutex<Graph>,
        rep: MemoryReplicator,
    },
    File {
        path: PathBuf,
        g: Option<Graph>,
        rep: PagedReplicator,
    },
}

impl Side {
    fn new(kind: Kind, dir: &Path, backend: Arc<dyn Backend>) -> Side {
        std::fs::create_dir_all(dir).unwrap();
        match kind {
            Kind::Memory => Side::Memory {
                g: Mutex::new(logged()),
                rep: mem_replicator(backend),
            },
            Kind::File => {
                let path = dir.join("db");
                Side::File {
                    g: Some(open_file(&path)),
                    rep: PagedReplicator::new(&path, dir.join("spool"), backend, cfg()),
                    path,
                }
            }
        }
    }

    /// The backend's newest replica, restored to a database of `kind`,
    /// replicated back to the same backend by a replicator that knows
    /// nothing of it yet.
    fn restore(kind: Kind, dir: &Path, backend: Arc<dyn Backend>) -> Side {
        std::fs::create_dir_all(dir).unwrap();
        match kind {
            Kind::Memory => {
                let (g, _) =
                    memory::restore(backend.as_ref(), None, None, u64::MAX, log::DEFAULT_LOG_CAP)
                        .unwrap();
                Side::Memory {
                    g: Mutex::new(g),
                    rep: mem_replicator(backend),
                }
            }
            Kind::File => {
                let path = dir.join("db");
                paged::restore(backend.as_ref(), None, None, &path).unwrap();
                Side::File {
                    g: Some(open_file(&path)),
                    rep: PagedReplicator::new(&path, dir.join("spool"), backend, cfg()),
                    path,
                }
            }
        }
    }

    fn graph(&mut self) -> &mut Graph {
        match self {
            Side::Memory { g, .. } => g.get_mut().unwrap(),
            Side::File { path, g, .. } => g.get_or_insert_with(|| open_file(path)),
        }
    }

    fn generation(&mut self) -> String {
        glider::replica::generation_name(&self.graph().generation())
    }

    /// A pass while the database is in use: a file's writer keeps writing
    /// (a base of a file needs its writer to checkpoint under a hold).
    fn pass(&mut self, r: &mut Rng, force: bool) -> SyncReport {
        match self {
            Side::Memory { g, rep } => rep.sync(g, force, false).unwrap(),
            Side::File { path, g, rep } => {
                let g = g.get_or_insert_with(|| open_file(path));
                std::thread::scope(|sc| {
                    let h = sc.spawn(|| rep.sync(force, false));
                    let start = Instant::now();
                    while !h.is_finished() {
                        write(g, r);
                        std::thread::sleep(Duration::from_millis(2));
                        assert!(
                            start.elapsed() < Duration::from_secs(120),
                            "the pass never finished"
                        );
                    }
                    h.join().unwrap().unwrap()
                })
            }
        }
    }

    /// A pass with nothing being written, so the replica ends up holding
    /// everything committed. A file is closed for it: were a base wanted,
    /// it would be copied under the file's lock rather than waited for.
    fn settle(&mut self) -> SyncReport {
        match self {
            Side::Memory { g, rep } => rep.sync(g, false, false).unwrap(),
            Side::File { g, rep, .. } => {
                *g = None;
                rep.sync(false, false).unwrap()
            }
        }
    }
}

// ----------------------------------------------------------------- backends

/// A directory, recording the order things were put in it.
struct Recorded {
    inner: FileBackend,
    puts: Mutex<Vec<String>>,
}

impl Recorded {
    fn new(root: PathBuf) -> Recorded {
        Recorded {
            inner: FileBackend { root },
            puts: Mutex::new(Vec::new()),
        }
    }
}

impl Backend for Recorded {
    fn put(&self, key: &str, body: Vec<u8>) -> io::Result<()> {
        self.inner.put(key, body)?;
        self.puts.lock().unwrap().push(key.to_string());
        Ok(())
    }
    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        self.inner.get(key)
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        self.inner.delete(key)
    }
    fn list(&self, prefix: &str) -> io::Result<Vec<Entry>> {
        self.inner.list(prefix)
    }
    fn describe(&self) -> String {
        "recorded".into()
    }
}

/// Every base's `base.json` was put after every other file of the base.
fn base_json_last(puts: &[String]) {
    for (i, k) in puts.iter().enumerate() {
        if let Some(prefix) = k.strip_suffix("base.json") {
            assert!(
                puts[i + 1..].iter().all(|later| !later.starts_with(prefix)),
                "a file of {prefix} went up after its base.json"
            );
            assert!(
                puts[..i]
                    .iter()
                    .any(|earlier| *earlier == format!("{prefix}db")),
                "{prefix}base.json went up before its db"
            );
        }
    }
}

/// A directory whose next put matching a test fails.
struct Failing {
    inner: FileBackend,
    fail: Mutex<Option<Box<dyn Fn(&str) -> bool + Send>>>,
    puts: Mutex<Vec<String>>,
}

impl Failing {
    fn new(root: PathBuf) -> Failing {
        Failing {
            inner: FileBackend { root },
            fail: Mutex::new(None),
            puts: Mutex::new(Vec::new()),
        }
    }
    fn fail_next(&self, f: impl Fn(&str) -> bool + Send + 'static) {
        *self.fail.lock().unwrap() = Some(Box::new(f));
    }
}

impl Backend for Failing {
    fn put(&self, key: &str, body: Vec<u8>) -> io::Result<()> {
        let mut fail = self.fail.lock().unwrap();
        if fail.as_ref().map(|f| f(key)).unwrap_or(false) {
            *fail = None;
            return Err(io::Error::other(format!("injected failure putting {key}")));
        }
        drop(fail);
        self.inner.put(key, body)?;
        self.puts.lock().unwrap().push(key.to_string());
        Ok(())
    }
    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        self.inner.get(key)
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        self.inner.delete(key)
    }
    fn list(&self, prefix: &str) -> io::Result<Vec<Entry>> {
        self.inner.list(prefix)
    }
    fn describe(&self) -> String {
        "failing".into()
    }
}

// --------------------------------------------------------------------- tests

#[test]
fn a_memory_replica_is_laid_out_as_a_file_replica_is() {
    let root = temp("layout");
    let mut r = Rng::new(7);
    // A file's replica, for its shape.
    let files: Arc<dyn Backend> = Arc::new(FileBackend {
        root: root.join("files"),
    });
    let mut file = Side::new(Kind::File, &root.join("file"), files.clone());
    rounds(file.graph(), &mut r, 200);
    file.pass(&mut r, false);
    let fgen = file.generation();

    // A memory graph's.
    let mem = Arc::new(Recorded::new(root.join("mem")));
    let g = Mutex::new(logged());
    rounds(&mut g.lock().unwrap(), &mut r, 300);
    let rep = mem_replicator(mem.clone());
    let first = rep.sync(&g, false, false).unwrap();
    assert_eq!((first.bases_uploaded, first.segments_uploaded), (1, 0));
    let gen = first.generation.clone();
    assert_eq!(
        gen,
        glider::replica::generation_name(&g.lock().unwrap().generation())
    );
    for _ in 0..3 {
        rounds(&mut g.lock().unwrap(), &mut r, 60);
        let next = rep.sync(&g, false, false).unwrap();
        assert_eq!((next.bases_uploaded, next.segments_uploaded), (0, 1));
        assert_eq!(next.generation, gen);
    }

    // The same keys.
    let l = paged::listing(mem.as_ref(), &gen).unwrap();
    let base = paged::base_prefix(&gen, l.bases[0].lsn);
    let mut files_of_base = 0;
    for e in mem.list("").unwrap() {
        if let Some(rel) = e.key.strip_prefix(&base) {
            assert!(
                rel == "base.json" || paged::base_file_number(rel).is_some(),
                "{}",
                e.key
            );
            files_of_base += 1;
        } else {
            let name = e.key.strip_prefix(&format!("{gen}/segments/")).unwrap();
            let (offset, ts) = name.strip_suffix(".seg").unwrap().split_once('-').unwrap();
            assert_eq!(offset.len(), 16, "{}", e.key);
            u64::from_str_radix(offset, 16).unwrap();
            assert!(ts.parse::<u64>().unwrap() > 1_700_000_000);
        }
    }
    assert!(
        files_of_base > 3,
        "a base cut into several files: {files_of_base}"
    );
    // The same base.json, field for field.
    let names = |text: &str| -> Vec<String> {
        text.split('"')
            .skip(1)
            .step_by(2)
            .map(|s| s.to_string())
            .collect()
    };
    let fl = paged::listing(files.as_ref(), &fgen).unwrap();
    let file_meta = files
        .get(&format!(
            "{}base.json",
            paged::base_prefix(&fgen, fl.bases[0].lsn)
        ))
        .unwrap();
    let mem_meta = mem.get(&format!("{base}base.json")).unwrap();
    let (file_meta, mem_meta) = (
        String::from_utf8(file_meta).unwrap(),
        String::from_utf8(mem_meta).unwrap(),
    );
    assert_eq!(names(&file_meta), names(&mem_meta));
    assert!(mem_meta.ends_with("}\n"));
    // Whole transactions, reaching the last commit; base.json last.
    for s in &l.segments {
        assert!(log::is_whole_transactions(&mem.get(&s.key).unwrap()));
    }
    let (_, committed) = g.lock().unwrap().log_span().unwrap();
    assert_eq!(l.complete_to(), committed);
    base_json_last(&mem.puts.lock().unwrap());
    // Recognised and listed as a file's replica is.
    assert!(paged::is_paged_replica(mem.as_ref()).unwrap());
    assert_eq!(paged::generations(mem.as_ref()).unwrap(), vec![gen.clone()]);
    assert!(paged::describe(&l).contains("1 base(s)"));
    let _ = std::fs::remove_dir_all(&root);
}

/// A source of `from`'s kind written and replicated in rounds, restored to
/// `to`'s kind, written there and replicated back: the replica restores, to
/// either place, as the side that wrote last.
fn round_trip(seed: u64, from: Kind, to: Kind) {
    let ctx = format!("seed {seed}, {from:?} to {to:?}");
    let root = temp(&format!("trip-{seed}-{from:?}-{to:?}"));
    let backend: Arc<dyn Backend> = Arc::new(FileBackend {
        root: root.join("remote"),
    });
    let mut r = Rng::new(seed ^ from as u64 ^ ((to as u64) << 1));
    let mut src = Side::new(from, &root.join("src"), backend.clone());
    for i in 0..4 {
        rounds(src.graph(), &mut r, 50);
        let rep = src.pass(&mut r, i == 2);
        let base = i == 0 || i == 2;
        assert_eq!(rep.bases_uploaded, base as usize, "{ctx}: pass {i}");
    }
    rounds(src.graph(), &mut r, 50);
    src.settle();
    let gen = src.generation();
    let want = view(src.graph());
    drop(src);

    let mut dst = Side::restore(to, &root.join("dst"), backend.clone());
    same(&want, &view(dst.graph()), &format!("{ctx}: restored"));
    assert_eq!(dst.generation(), gen, "{ctx}: the replica's lineage");

    // Written to and replicated back: only what it wrote goes up, from
    // where the replica ended.
    for i in 0..2 {
        let before = paged::listing(backend.as_ref(), &gen).unwrap();
        rounds(dst.graph(), &mut r, 50);
        let on = dst.settle();
        assert_eq!(on.bases_uploaded, 0, "{ctx}: pass {i} took a base");
        assert!(on.segments_uploaded >= 1, "{ctx}: pass {i} shipped nothing");
        assert_eq!(on.generation, gen, "{ctx}: pass {i}");
        let after = paged::listing(backend.as_ref(), &gen).unwrap();
        assert_eq!(after.bases.len(), before.bases.len(), "{ctx}");
        let new: Vec<_> = after
            .segments
            .iter()
            .filter(|s| !before.segments.iter().any(|b| b.key == s.key))
            .collect();
        assert_eq!(
            new.first().map(|s| s.seg.offset),
            Some(before.complete_to()),
            "{ctx}: pass {i} did not carry on where the replica ended"
        );
        assert_eq!(after.furthest(), Some(after.complete_to()), "{ctx}");
    }
    let want = view(dst.graph());
    drop(dst);
    for kind in [Kind::Memory, Kind::File] {
        let mut back = Side::restore(kind, &root.join(format!("back-{kind:?}")), backend.clone());
        same(
            &want,
            &view(back.graph()),
            &format!("{ctx}: restored again, as {kind:?}"),
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn every_combination_restores_equal_and_carries_the_lineage_on() {
    for seed in seeds(1, 2) {
        for (from, to) in [
            (Kind::Memory, Kind::Memory),
            (Kind::Memory, Kind::File),
            (Kind::File, Kind::Memory),
            (Kind::File, Kind::File),
        ] {
            round_trip(seed, from, to);
        }
    }
}

/// A backend that takes its time over every put, and at each one checks
/// that the graph's lock can be taken: that nothing is uploaded while the
/// database is held.
struct Slow {
    inner: FileBackend,
    delay: Duration,
    graph: Arc<Mutex<Graph>>,
    held: AtomicUsize,
    windows: Mutex<Vec<(Instant, Instant)>>,
}

impl Backend for Slow {
    fn put(&self, key: &str, body: Vec<u8>) -> io::Result<()> {
        let start = Instant::now();
        let mut free = false;
        while start.elapsed() < self.delay {
            match self.graph.try_lock() {
                Ok(_) | Err(TryLockError::Poisoned(_)) => {
                    free = true;
                    break;
                }
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        if !free {
            self.held.fetch_add(1, Ordering::SeqCst);
        }
        std::thread::sleep(self.delay.saturating_sub(start.elapsed()));
        self.windows.lock().unwrap().push((start, Instant::now()));
        self.inner.put(key, body)
    }
    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        self.inner.get(key)
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        self.inner.delete(key)
    }
    fn list(&self, prefix: &str) -> io::Result<Vec<Entry>> {
        self.inner.list(prefix)
    }
    fn describe(&self) -> String {
        "slow".into()
    }
}

#[test]
fn a_pass_holds_the_database_only_to_capture_and_to_acknowledge() {
    let root = temp("lock");
    let graph = Arc::new(Mutex::new(logged()));
    let mut r = Rng::new(11);
    rounds(&mut graph.lock().unwrap(), &mut r, 250);
    let delay = Duration::from_millis(400);
    let slow = Arc::new(Slow {
        inner: FileBackend {
            root: root.join("remote"),
        },
        delay,
        graph: graph.clone(),
        held: AtomicUsize::new(0),
        windows: Mutex::new(Vec::new()),
    });
    let mut rep = mem_replicator(slow.clone());
    // A base of several files: several slow puts.
    rep.base_segment_bytes = 64 << 10;

    // A writer committing in a loop, as fast as it goes.
    let stop = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let writer = {
        let (graph, stop, commits) = (graph.clone(), stop.clone(), commits.clone());
        std::thread::spawn(move || {
            let mut i = 0i64;
            while !stop.load(Ordering::SeqCst) {
                graph
                    .lock()
                    .unwrap()
                    .add_node(&["W".into()], vec![("i".into(), Value::Int(i))])
                    .unwrap();
                commits.lock().unwrap().push(Instant::now());
                i += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    // Its normal rate, before any pass.
    std::thread::sleep(Duration::from_millis(1500));
    let rate = |from: Instant, to: Instant| -> f64 {
        let n = commits
            .lock()
            .unwrap()
            .iter()
            .filter(|t| **t >= from && **t < to)
            .count();
        n as f64 / (to - from).as_secs_f64()
    };
    let begun = commits.lock().unwrap()[0];
    let normal = rate(begun, Instant::now());

    let started = Instant::now();
    let report = rep.sync(&graph, false, false).unwrap();
    let ended = Instant::now();
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();

    assert_eq!(report.bases_uploaded, 1);
    let windows = slow.windows.lock().unwrap().clone();
    assert!(windows.len() >= 3, "{} uploads", windows.len());
    assert_eq!(
        slow.held.load(Ordering::SeqCst),
        0,
        "an upload happened while the database was held"
    );
    let times = commits.lock().unwrap().clone();
    for (i, (a, b)) in windows.iter().enumerate() {
        let n = times.iter().filter(|t| *t >= a && *t < b).count();
        assert!(n > 0, "no commit during upload {i}");
    }
    let mut gaps: Vec<Duration> = times
        .windows(2)
        .filter(|w| w[0] >= started && w[1] <= ended)
        .map(|w| w[1] - w[0])
        .collect();
    gaps.sort();
    let worst = gaps.last().copied().unwrap_or_default();
    assert!(
        worst < delay / 2,
        "the writer stalled for {worst:?} during the pass (an upload takes {delay:?})"
    );
    let during = rate(windows[0].0, windows.last().unwrap().1);
    assert!(
        during > normal / 3.0,
        "the writer committed {during:.0}/s during the uploads, {normal:.0}/s before"
    );
    eprintln!(
        "writer: {normal:.0} commits/s before the pass, {during:.0}/s during its {} uploads; \
         longest gap between commits during the pass {worst:?}",
        windows.len()
    );

    // And the replica is the graph, once the rest of the log follows.
    rep.sync(&graph, false, false).unwrap();
    let want = view(&mut graph.lock().unwrap());
    let (mut back, _) = memory::restore(slow.as_ref(), None, None, u64::MAX, 1 << 20).unwrap();
    same(&want, &view(&mut back), "restored");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_pass_that_fails_at_any_upload_records_nothing_and_the_next_completes_it() {
    for seed in seeds(2, 2) {
        let root = temp(&format!("fail-{seed}"));
        let backend = Arc::new(Failing::new(root.join("remote")));
        let g = Mutex::new(logged());
        let mut r = Rng::new(seed);
        rounds(&mut g.lock().unwrap(), &mut r, 250);
        let rep = mem_replicator(backend.clone());
        rep.sync(&g, false, false).unwrap();
        let gen = glider::replica::generation_name(&g.lock().unwrap().generation());
        // Every state the replica has held: what a restore may give.
        let mut held = vec![view(&mut g.lock().unwrap())];
        let restore = || memory::restore(backend.as_ref(), None, None, u64::MAX, 1 << 20);

        type Pick = fn(&str) -> bool;
        let cases: [(&str, bool, Pick); 4] = [
            ("a log segment", false, |k| k.contains("/segments/")),
            ("the first file of a base", true, |k| k.ends_with("/db")),
            ("a data segment of a base", true, |k| {
                k.ends_with("/data/00000002.seg")
            }),
            ("a base's base.json", true, |k| k.ends_with("/base.json")),
        ];
        for (what, force, pick) in cases {
            let ctx = format!("seed {seed}: failing {what}");
            rounds(&mut g.lock().unwrap(), &mut r, 80);
            let before = g.lock().unwrap().log_span().unwrap();
            let bases = paged::listing(backend.as_ref(), &gen).unwrap().bases.len();
            backend.fail_next(pick);
            assert!(rep.sync(&g, force, false).is_err(), "{ctx}: the pass fails");
            assert!(
                backend.fail.lock().unwrap().is_none(),
                "{ctx}: it was tried"
            );
            // Nothing recorded: the log is all still held.
            assert_eq!(g.lock().unwrap().log_span().unwrap(), before, "{ctx}");
            // The replica is as it was: an earlier state, no new base, and
            // nothing left of the one that failed.
            let l = paged::listing(backend.as_ref(), &gen).unwrap();
            assert_eq!(l.bases.len(), bases, "{ctx}");
            let (_, committed) = before;
            let failed = paged::base_prefix(&gen, committed);
            assert!(
                backend.list(&failed).unwrap().is_empty(),
                "{ctx}: the failed base left files"
            );
            let (mut back, _) = restore().unwrap();
            let got = view(&mut back);
            assert!(held.contains(&got), "{ctx}: restores to no state it held");
            // The next pass completes it, from where the replica ended: no
            // hole, nothing twice.
            let done = rep.sync(&g, force, false).unwrap();
            assert_eq!(
                (done.bases_uploaded, done.segments_uploaded),
                if force { (1, 0) } else { (0, 1) },
                "{ctx}"
            );
            let l = paged::listing(backend.as_ref(), &gen).unwrap();
            let (_, committed) = g.lock().unwrap().log_span().unwrap();
            assert_eq!(l.complete_to(), committed, "{ctx}");
            assert_eq!(l.furthest(), Some(committed), "{ctx}");
            let want = view(&mut g.lock().unwrap());
            let (mut back, _) = restore().unwrap();
            same(&want, &view(&mut back), &ctx);
            held.push(want);
        }
        base_json_last(&backend.puts.lock().unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn log_past_the_cap_between_passes_costs_a_base_and_never_a_transaction() {
    for seed in seeds(3, 2) {
        let root = temp(&format!("cap-{seed}"));
        let backend: Arc<dyn Backend> = Arc::new(FileBackend {
            root: root.join("remote"),
        });
        let mut graph = Graph::memory_with_page_size(1024, u64::MAX);
        let cap = 16 << 10;
        graph.enable_log(cap).unwrap();
        let g = Mutex::new(graph);
        let mut r = Rng::new(seed);
        let rep = mem_replicator(backend.clone());
        rounds(&mut g.lock().unwrap(), &mut r, 100);
        rep.sync(&g, false, false).unwrap();
        let gen = glider::replica::generation_name(&g.lock().unwrap().generation());
        for i in 0..6 {
            let ctx = format!("seed {seed} round {i}");
            let over = i % 2 == 1;
            let shipped = paged::listing(backend.as_ref(), &gen)
                .unwrap()
                .complete_to();
            // Under the cap, or well past it.
            loop {
                rounds(&mut g.lock().unwrap(), &mut r, 10);
                let (start, end) = g.lock().unwrap().log_span().unwrap();
                if over && start > shipped {
                    break; // dropped: the log the replica needs is gone
                }
                if !over && end - shipped > cap / 4 {
                    break;
                }
                assert!(end - shipped < 64 * cap, "{ctx}: the log never dropped");
            }
            let pass = rep.sync(&g, false, false).unwrap();
            assert_eq!(
                (pass.bases_uploaded, pass.segments_uploaded),
                if over { (1, 0) } else { (0, 1) },
                "{ctx}"
            );
            let want = view(&mut g.lock().unwrap());
            let (mut back, _) =
                memory::restore(backend.as_ref(), None, None, u64::MAX, 1 << 20).unwrap();
            same(&want, &view(&mut back), &ctx);
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn a_replicator_looks_at_the_backend_before_it_ships() {
    let root = temp("start");
    let a: Arc<dyn Backend> = Arc::new(FileBackend {
        root: root.join("a"),
    });
    let b: Arc<dyn Backend> = Arc::new(FileBackend {
        root: root.join("b"),
    });
    let mut r = Rng::new(5);
    let restore = |be: &dyn Backend, gen: Option<&str>, as_of: Option<u64>| {
        memory::restore(be, gen, as_of, u64::MAX, 1 << 20)
            .unwrap()
            .0
    };

    // Shipped to A for a while: its log there is let go of.
    let g = Mutex::new(logged());
    rounds(&mut g.lock().unwrap(), &mut r, 150);
    let to_a = mem_replicator(a.clone());
    to_a.sync(&g, false, false).unwrap();
    rounds(&mut g.lock().unwrap(), &mut r, 100);
    to_a.sync(&g, false, false).unwrap();
    let gen = glider::replica::generation_name(&g.lock().unwrap().generation());
    assert!(g.lock().unwrap().log_span().unwrap().0 > 0);

    // An empty prefix gets a base, whatever the database shipped elsewhere.
    let first = mem_replicator(b.clone()).sync(&g, false, false).unwrap();
    assert_eq!(
        (first.bases_uploaded, first.generation.as_str()),
        (1, gen.as_str())
    );
    let want = view(&mut g.lock().unwrap());
    same(&want, &view(&mut restore(b.as_ref(), None, None)), "B");

    // The prefix it was restored from gets only the new log; a new
    // replicator of the same graph against A, too.
    rounds(&mut g.lock().unwrap(), &mut r, 50);
    let again = mem_replicator(a.clone()).sync(&g, false, false).unwrap();
    assert_eq!((again.bases_uploaded, again.segments_uploaded), (0, 1));
    let t = glider::wal::now_unix();
    let at_t = view(&mut g.lock().unwrap());
    std::thread::sleep(Duration::from_millis(1100));
    let restored = Mutex::new(restore(a.as_ref(), None, None));
    rounds(&mut restored.lock().unwrap(), &mut r, 50);
    let on = mem_replicator(a.clone())
        .sync(&restored, false, false)
        .unwrap();
    assert_eq!((on.bases_uploaded, on.segments_uploaded), (0, 1));
    assert_eq!(on.generation, gen);
    let latest = view(&mut restored.lock().unwrap());

    // A prefix holding another generation gets a base of this one, and the
    // other is left as it was.
    let other = Mutex::new(logged());
    rounds(&mut other.lock().unwrap(), &mut r, 80);
    let theirs = mem_replicator(a.clone())
        .sync(&other, false, false)
        .unwrap();
    assert_eq!(theirs.bases_uploaded, 1);
    assert_ne!(theirs.generation, gen);
    let gens = paged::generations(a.as_ref()).unwrap();
    assert!(gens.contains(&gen) && gens.contains(&theirs.generation));
    let mut back = restore(a.as_ref(), Some(&gen), None);
    same(&latest, &view(&mut back), "this lineage, untouched");
    let want = view(&mut other.lock().unwrap());
    same(
        &want,
        &view(&mut restore(a.as_ref(), Some(&theirs.generation), None)),
        "the other lineage",
    );

    // Restored to an earlier point, written to, and shipped back: the
    // replica holds this generation past where the database is, so it
    // carries on as a lineage of its own, and the first is left whole.
    let early = Mutex::new(restore(a.as_ref(), Some(&gen), Some(t)));
    same(&at_t, &view(&mut early.lock().unwrap()), "as of t");
    rounds(&mut early.lock().unwrap(), &mut r, 40);
    // Keys carry the second: a second on, the fork is the most recently
    // active lineage, not tied with the other.
    std::thread::sleep(Duration::from_millis(1100));
    let fork = mem_replicator(a.clone())
        .sync(&early, false, false)
        .unwrap();
    assert_eq!(fork.bases_uploaded, 1, "a base of its own");
    assert_ne!(fork.generation, gen, "a lineage of its own");
    assert_eq!(
        fork.generation,
        glider::replica::generation_name(&early.lock().unwrap().generation())
    );
    let mine = view(&mut early.lock().unwrap());
    same(
        &mine,
        &view(&mut restore(a.as_ref(), Some(&fork.generation), None)),
        "the fork",
    );
    same(
        &latest,
        &view(&mut restore(a.as_ref(), Some(&gen), None)),
        "the first lineage",
    );
    // The most recently active lineage is what a plain restore gives.
    same(&mine, &view(&mut restore(a.as_ref(), None, None)), "latest");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_restore_stops_at_a_gap_or_a_time_and_refuses_a_damaged_base() {
    let root = temp("restore");
    let backend = Arc::new(FileBackend {
        root: root.join("remote"),
    });
    let g = Mutex::new(logged());
    let mut r = Rng::new(9);
    rounds(&mut g.lock().unwrap(), &mut r, 300);
    let rep = mem_replicator(backend.clone());
    rep.sync(&g, false, false).unwrap();
    let gen = glider::replica::generation_name(&g.lock().unwrap().generation());
    // Three segments of one transaction each, the last shipped a second
    // after the others.
    let mut states = vec![view(&mut g.lock().unwrap())];
    let mut t = 0;
    for i in 0..3i64 {
        if i == 2 {
            t = glider::wal::now_unix();
            std::thread::sleep(Duration::from_millis(1100));
        }
        g.lock()
            .unwrap()
            .add_node(&["S".into()], vec![("i".into(), Value::Int(i))])
            .unwrap();
        assert_eq!(rep.sync(&g, false, false).unwrap().segments_uploaded, 1);
        states.push(view(&mut g.lock().unwrap()));
    }
    let l = paged::listing(backend.as_ref(), &gen).unwrap();
    assert_eq!(l.segments.len(), 3);
    let restore = || memory::restore(backend.as_ref(), None, None, u64::MAX, 1 << 20);
    let check = |want: &str, ctx: &str| {
        let (mut back, _) = restore().unwrap();
        same(want, &view(&mut back), ctx);
    };
    check(&states[3], "everything");
    let (mut then, report) =
        memory::restore(backend.as_ref(), None, Some(t), u64::MAX, 1 << 20).unwrap();
    same(&states[2], &view(&mut then), "as of t");
    assert_eq!(
        (report.segments, report.through),
        (2, l.segments[2].seg.offset)
    );

    // Log: a damaged frame, or a segment cut short, ends the log there.
    let path = |key: &str| backend.root.join(key);
    let last = path(&l.segments[2].key);
    let good = std::fs::read(&last).unwrap();
    let mut bad = good.clone();
    bad[good.len() / 2] ^= 0x10;
    std::fs::write(&last, &bad).unwrap();
    check(&states[2], "a damaged segment");
    std::fs::write(&last, &good[..good.len() - 1]).unwrap();
    check(&states[2], "a segment cut short");
    std::fs::write(&last, &good).unwrap();
    // A gap ends it at the gap.
    let middle = path(&l.segments[1].key);
    let kept = std::fs::read(&middle).unwrap();
    std::fs::remove_file(&middle).unwrap();
    check(&states[1], "a gap");
    std::fs::write(&middle, &kept).unwrap();
    check(&states[3], "whole again");

    // A base that is not whole and intact is an error, never a graph.
    let base = paged::base_prefix(&gen, l.bases[0].lsn);
    let files: Vec<PathBuf> = backend
        .list(&base)
        .unwrap()
        .into_iter()
        .filter(|e| !e.key.ends_with("base.json"))
        .map(|e| path(&e.key))
        .collect();
    assert!(files.len() >= 3);
    let refused = |ctx: &str| match restore() {
        Ok(_) => panic!("{ctx}: restored"),
        Err(Error::Full(e)) => panic!("{ctx}: {e}"),
        Err(_) => {}
    };
    for f in &files {
        let good = std::fs::read(f).unwrap();
        // Every page damaged: some page of it is live.
        let mut bad = good.clone();
        for page in bad.chunks_mut(1024) {
            page[200] ^= 0x01;
        }
        std::fs::write(f, &bad).unwrap();
        refused(&format!("{} damaged", f.display()));
        std::fs::write(f, &good[..good.len() - 1024]).unwrap();
        refused(&format!("{} cut short", f.display()));
        std::fs::remove_file(f).unwrap();
        refused(&format!("{} missing", f.display()));
        std::fs::write(f, &good).unwrap();
    }
    // A base.json that does not describe its pages.
    let meta = path(&format!("{base}base.json"));
    let text = std::fs::read_to_string(&meta).unwrap();
    for (from, to) in [
        ("\"pages\":", "\"pages\":1"),
        ("\"page_size\":1024", "\"page_size\":2048"),
    ] {
        std::fs::write(&meta, text.replacen(from, to, 1)).unwrap();
        refused(&format!("base.json with {to}"));
    }
    // A base filed under another position.
    let moved = paged::base_prefix(&gen, l.bases[0].lsn + 1);
    for e in backend.list(&base).unwrap() {
        let rel = e.key.strip_prefix(&base).unwrap();
        let bytes = if rel == "base.json" {
            text.replacen(
                &format!("\"lsn\":{}", l.bases[0].lsn),
                &format!("\"lsn\":{}", l.bases[0].lsn + 1),
                1,
            )
            .into_bytes()
        } else {
            std::fs::read(path(&e.key)).unwrap()
        };
        backend.put(&format!("{moved}{rel}"), bytes).unwrap();
    }
    std::fs::write(&meta, &text).unwrap();
    std::fs::remove_file(&meta).unwrap();
    refused("a base filed at the wrong position");
    for e in backend.list(&moved).unwrap() {
        backend.delete(&e.key).unwrap();
    }
    std::fs::write(&meta, &text).unwrap();
    check(&states[3], "whole again");

    // Too big for the memory it may have: Full, whether the base or the log
    // after it is what does not fit. A graph may go SLACK_PAGES past its
    // max_memory, so this one is well past that: a base of some 200 KiB of
    // pages and as much again in the log after it.
    let big = Arc::new(FileBackend {
        root: root.join("big"),
    });
    let g = Mutex::new(logged());
    let rep = mem_replicator(big.clone());
    let fill = |g: &Mutex<Graph>, to: u64| {
        let mut g = g.lock().unwrap();
        let mut i = 0i64;
        while g.stats().memory.unwrap().0 < to {
            g.add_node(
                &["F".into()],
                vec![(
                    "blob".into(),
                    Value::Text(format!("{i}{}", "f".repeat(900))),
                )],
            )
            .unwrap();
            i += 1;
        }
        g.stats().memory.unwrap().0
    };
    let in_base = fill(&g, 200 << 10);
    assert_eq!(rep.sync(&g, false, false).unwrap().bases_uploaded, 1);
    let used = fill(&g, 400 << 10);
    assert_eq!(rep.sync(&g, false, false).unwrap().segments_uploaded, 1);
    let slack = glider::storage::pager::SLACK_PAGES * 1024;
    for (max, what) in [(in_base / 4, "the base"), (in_base + slack, "the log")] {
        match memory::restore(big.as_ref(), None, None, max, 1 << 20) {
            Err(Error::Full(_)) => {}
            Err(e) => panic!("max_memory {max}, {what} too big: {e}"),
            Ok(_) => panic!("max_memory {max}, {what} too big: restored {used} bytes of pages"),
        }
    }
    let (mut back, _) = memory::restore(big.as_ref(), None, None, used, 1 << 20).unwrap();
    same(&view(&mut g.lock().unwrap()), &view(&mut back), "just fits");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn retention_listing_and_lag_work_for_a_memory_replica() {
    let root = temp("retention");
    let backend: Arc<dyn Backend> = Arc::new(FileBackend {
        root: root.join("remote"),
    });
    let mut r = Rng::new(13);
    // Another lineage, gone idle.
    let idle = Mutex::new(logged());
    rounds(&mut idle.lock().unwrap(), &mut r, 50);
    let theirs = mem_replicator(backend.clone())
        .sync(&idle, false, false)
        .unwrap()
        .generation;

    let g = Mutex::new(logged());
    let mut rep = mem_replicator(backend.clone());
    rounds(&mut g.lock().unwrap(), &mut r, 150);
    let gen = rep.sync(&g, false, false).unwrap().generation;
    rounds(&mut g.lock().unwrap(), &mut r, 50);
    rep.sync(&g, false, false).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    rounds(&mut g.lock().unwrap(), &mut r, 50);
    assert_eq!(rep.sync(&g, true, false).unwrap().bases_uploaded, 1);
    rounds(&mut g.lock().unwrap(), &mut r, 50);
    rep.sync(&g, false, false).unwrap();

    let l = paged::listing(backend.as_ref(), &gen).unwrap();
    assert_eq!(l.bases.len(), 2);
    assert_eq!(paged::generations(backend.as_ref()).unwrap().len(), 2);
    // Lag: nothing after a pass, the log written since otherwise, and the
    // same whether reckoned from what the replicator knows or from a listing.
    let committed = |g: &Mutex<Graph>| g.lock().unwrap().log_span().unwrap().1;
    assert_eq!(rep.lag(&g.lock().unwrap()), Some(0));
    assert_eq!(memory::lag(&l, committed(&g)), 0);
    rounds(&mut g.lock().unwrap(), &mut r, 30);
    let behind = rep.lag(&g.lock().unwrap()).unwrap();
    assert!(behind > 0);
    let l = paged::listing(backend.as_ref(), &gen).unwrap();
    assert_eq!(memory::lag(&l, committed(&g)), behind);
    assert_eq!(rep.metrics.lag_bytes.load(Ordering::Relaxed), 0);
    assert_eq!(
        MemoryReplicator::new(backend.clone(), cfg()).lag(&g.lock().unwrap()),
        None
    );

    // Retention: the older base goes, the log below the newest with it, and
    // the idle lineage; never the newest base or the current lineage.
    rep.cfg.retention = Duration::from_secs(0);
    let deleted = rep.sync(&g, false, true).unwrap().deleted;
    assert!(deleted >= 3, "{deleted} deleted");
    let l = paged::listing(backend.as_ref(), &gen).unwrap();
    assert_eq!(l.bases.len(), 1, "the newest base is kept");
    assert!(l.segments.iter().all(|s| s.seg.end() > l.bases[0].lsn));
    assert_eq!(
        paged::generations(backend.as_ref()).unwrap(),
        vec![gen.clone()]
    );
    assert!(paged::listing(backend.as_ref(), &theirs)
        .unwrap()
        .bases
        .is_empty());
    assert!(rep.metrics.deletions.load(Ordering::Relaxed) >= deleted as u64);
    let want = view(&mut g.lock().unwrap());
    let (mut back, _) = memory::restore(backend.as_ref(), None, None, u64::MAX, 1 << 20).unwrap();
    same(&want, &view(&mut back), "after retention");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_base_of_a_64_mib_graph_is_captured_in_the_time_it_takes_to_copy_its_page_table() {
    let root = temp("capture");
    let backend: Arc<dyn Backend> = Arc::new(FileBackend {
        root: root.join("remote"),
    });
    // 64 MiB of 16 KiB pages: nodes carrying 4 KiB of text each.
    let mut graph = Graph::memory_with_limit(u64::MAX);
    graph.enable_log(log::DEFAULT_LOG_CAP).unwrap();
    let blob = |i: i64| Value::Text(format!("{i:08}{}", "x".repeat(4088)));
    graph.autocommit = false;
    let mut i = 0i64;
    while graph.stats().memory.unwrap().0 < 64 << 20 {
        graph
            .add_node(&["A".into()], vec![("blob".into(), blob(i))])
            .unwrap();
        i += 1;
        if i % 1000 == 0 {
            graph.commit().unwrap();
        }
    }
    graph.commit().unwrap();
    graph.autocommit = true;
    let (used, _) = graph.stats().memory.unwrap();
    let g = Mutex::new(graph);
    let rep = MemoryReplicator::new(backend.clone(), cfg());

    // A first pass looks before it captures.
    let look = rep.capture(&mut g.lock().unwrap(), false).unwrap();
    assert!(look.looks());
    let looked = rep.upload(look).unwrap();
    rep.acknowledge(&mut g.lock().unwrap(), looked);

    let mut held = Vec::new();
    for _ in 0..5 {
        let mut guard = g.lock().unwrap();
        let t = Instant::now();
        let c = rep.capture(&mut guard, true).unwrap();
        held.push(t.elapsed());
        drop(guard);
        assert!(c.is_base());
    }
    held.sort();
    let base = held[2];
    // Shipped, and the same graph comes back.
    let c = rep.capture(&mut g.lock().unwrap(), false).unwrap();
    let t = Instant::now();
    let u = rep.upload(c).unwrap();
    let upload = t.elapsed();
    let report = rep.acknowledge(&mut g.lock().unwrap(), u);
    assert_eq!(report.bases_uploaded, 1);
    assert!(report.bytes >= used);

    // A segment of over 32 MiB of committed log, for comparison.
    {
        let mut guard = g.lock().unwrap();
        guard.autocommit = false;
        for k in 0..8500i64 {
            guard
                .add_node(&["B".into()], vec![("blob".into(), blob(k))])
                .unwrap();
        }
        guard.commit().unwrap();
        guard.autocommit = true;
    }
    let mut guard = g.lock().unwrap();
    let t = Instant::now();
    let c = rep.capture(&mut guard, false).unwrap();
    let segment = t.elapsed();
    drop(guard);
    assert!(!c.is_base());
    let u = rep.upload(c).unwrap();
    let report = rep.acknowledge(&mut g.lock().unwrap(), u);
    assert!(report.bytes > 32 << 20);
    eprintln!(
        "capture under the lock: a base of {:.1} MiB of pages in {base:?} (median of 5, \
         from {:?} to {:?}); uploading it to a directory took {upload:?}; a segment of \
         {:.1} MiB of log in {segment:?}",
        used as f64 / 1048576.0,
        held[0],
        held[4],
        report.bytes as f64 / 1048576.0
    );
    assert!(
        base < Duration::from_millis(100),
        "capturing a base held the lock for {base:?}"
    );
    let want = query::export_jsonl(&g.lock().unwrap());
    let (back, _) = memory::restore(backend.as_ref(), None, None, u64::MAX, 1 << 20).unwrap();
    assert!(query::export_jsonl(&back) == want, "restored");
    let _ = std::fs::remove_dir_all(&root);
}
