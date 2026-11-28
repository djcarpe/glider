//! A database's revision (docs/DERIVED.md, D2): a number that grows with
//! every committed transaction that changed something and with nothing else,
//! survives a close, a crash and a restore, and is the same on a replica.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use glider::graph::{Graph, OpenOptions};
use glider::query;
use glider::storage::log;
use glider::store::Sync;
use glider::stream::backend::{Backend, FileBackend};
use glider::stream::config::ReplicaConfig;
use glider::stream::memory::{self, MemoryReplicator};
use glider::stream::paged;
use glider::value::Value;

fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("glider-rev-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn open_small(path: &Path) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            page_size: 1024,
            cache_size: 64 << 10,
            // Checkpoint often, so logs are truncated and segments deleted.
            checkpoint_bytes: 8 << 10,
            wal_segment: 4 << 10,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

fn open_off(path: &Path) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            sync: Sync::Off,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

fn q(g: &mut Graph, src: &str) -> query::QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn stat(g: &mut Graph, name: &str) -> Option<Value> {
    q(g, "STATS")
        .rows
        .iter()
        .find(|r| r[0] == Value::Text(name.into()))
        .map(|r| r[1].clone())
}

/// The three kinds of graph a revision has to work for.
#[derive(Clone, Copy, Debug)]
enum Kind {
    File,
    Logged,
    Memory,
}

const KINDS: [Kind; 3] = [Kind::File, Kind::Logged, Kind::Memory];

fn make(kind: Kind, dir: &Path) -> Graph {
    match kind {
        Kind::File => open_small(&dir.join("db")),
        Kind::Logged => {
            let mut g = Graph::memory_with_page_size(1024, u64::MAX);
            g.enable_log(log::DEFAULT_LOG_CAP).unwrap();
            g
        }
        Kind::Memory => Graph::memory_with_page_size(1024, u64::MAX),
    }
}

/// Runs `f`, which must change the graph, and checks the revision grew.
fn grows(g: &mut Graph, what: &str, f: impl FnOnce(&mut Graph)) {
    let before = g.revision();
    f(g);
    let after = g.revision();
    assert!(after > before, "{what}: revision {before} -> {after}");
}

/// Runs `f`, which must not change the graph, and checks the revision held.
fn holds(g: &mut Graph, what: &str, f: impl FnOnce(&mut Graph)) {
    let before = g.revision();
    f(g);
    assert_eq!(g.revision(), before, "{what}");
}

fn every_kind_of_change(g: &mut Graph) {
    let mut a = 0;
    let mut b = 0;
    let mut e = 0;
    grows(g, "add a node", |g| {
        a = g
            .add_node(&["P".into()], vec![("x".into(), Value::Int(1))])
            .unwrap();
    });
    grows(g, "add another", |g| {
        b = g.add_node(&[], vec![]).unwrap();
    });
    grows(g, "add an edge", |g| {
        e = g.add_edge(a, b, "K", vec![]).unwrap();
    });
    grows(g, "set a node property", |g| {
        g.set_node_prop(a, "x", Value::Int(2)).unwrap()
    });
    grows(g, "an integer becomes the float it equals", |g| {
        g.set_node_prop(a, "x", Value::Float(2.0)).unwrap()
    });
    grows(g, "unset a node property", |g| {
        g.unset_node_prop(a, "x").unwrap()
    });
    grows(g, "set an edge property", |g| {
        g.set_edge_prop(e, "w", Value::Int(1)).unwrap()
    });
    grows(g, "unset an edge property", |g| {
        g.unset_edge_prop(e, "w").unwrap()
    });
    grows(g, "add a label", |g| g.add_label(b, "Q").unwrap());
    grows(g, "remove a label", |g| g.remove_label(b, "Q").unwrap());
    grows(g, "create an index", |g| g.create_index("P", "x").unwrap());
    grows(g, "drop an index", |g| g.drop_index("P", "x").unwrap());
    grows(g, "install an extension", |g| {
        q(g, "CREATE EXTENSION context");
    });
    grows(g, "an extension writes its data", |g| {
        g.ext_write("context", vec![(b"k".to_vec(), b"v".to_vec())], vec![])
            .unwrap()
    });
    grows(g, "an extension rewrites it", |g| {
        g.ext_write("context", vec![(b"k".to_vec(), b"w".to_vec())], vec![])
            .unwrap()
    });
    grows(g, "an extension deletes it", |g| {
        g.ext_write("context", vec![], vec![b"k".to_vec()]).unwrap()
    });
    grows(g, "drop the extension", |g| {
        q(g, "DROP EXTENSION context");
    });
    grows(g, "a statement", |g| {
        q(g, "CREATE (:S {n: 1})-[:R]->(:S {n: 2})");
    });
    grows(g, "a statement that sets", |g| {
        q(g, "MATCH (n:S) SET n.n = n.n + 10");
    });
    grows(g, "delete an edge", |g| assert!(g.delete_edge(e).unwrap()));
    grows(g, "delete a node", |g| assert!(g.delete_node(b).unwrap()));
    grows(g, "a transaction of several", |g| {
        q(g, "BEGIN");
        q(g, "CREATE (:T)");
        q(g, "CREATE (:T)");
        q(g, "COMMIT");
    });
    grows(g, "CLEAR", |g| {
        q(g, "CLEAR");
    });
    grows(g, "CLEAR of an empty graph", |g| {
        q(g, "CLEAR");
    });
}

#[test]
fn revision_grows_with_every_committed_change() {
    let root = temp("grows");
    for kind in KINDS {
        let dir = root.join(format!("{kind:?}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut g = make(kind, &dir);
        assert_eq!(g.revision(), 0, "{kind:?}: a new database is at 0");
        every_kind_of_change(&mut g);
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn nothing_changes(g: &mut Graph) {
    let a = g
        .add_node(&["P".into()], vec![("x".into(), Value::Int(1))])
        .unwrap();
    let b = g.add_node(&["P".into()], vec![]).unwrap();
    let e = g
        .add_edge(a, b, "K", vec![("w".into(), Value::Float(0.5))])
        .unwrap();
    g.create_index("P", "x").unwrap();
    q(g, "CREATE EXTENSION context");
    g.ext_write("context", vec![(b"k".to_vec(), b"v".to_vec())], vec![])
        .unwrap();
    holds(g, "set a property to the value it has", |g| {
        g.set_node_prop(a, "x", Value::Int(1)).unwrap()
    });
    holds(g, "set an edge property to the value it has", |g| {
        g.set_edge_prop(e, "w", Value::Float(0.5)).unwrap()
    });
    holds(g, "unset a property it does not have", |g| {
        g.unset_node_prop(a, "nope").unwrap()
    });
    holds(g, "unset an edge property it does not have", |g| {
        g.unset_edge_prop(e, "nope").unwrap()
    });
    holds(g, "add a label it has", |g| g.add_label(a, "P").unwrap());
    holds(g, "remove a label it lacks", |g| {
        g.remove_label(a, "Q").unwrap()
    });
    holds(g, "create an index that exists", |g| {
        g.create_index("P", "x").unwrap()
    });
    holds(g, "drop an index that does not exist", |g| {
        g.drop_index("P", "y").unwrap()
    });
    holds(g, "delete a node that does not exist", |g| {
        assert!(!g.delete_node(9999).unwrap())
    });
    holds(g, "an extension writes what it has", |g| {
        g.ext_write("context", vec![(b"k".to_vec(), b"v".to_vec())], vec![])
            .unwrap()
    });
    holds(g, "an extension deletes what it does not have", |g| {
        g.ext_write("context", vec![], vec![b"gone".to_vec()])
            .unwrap()
    });
    holds(g, "install what is installed", |g| {
        g.add_extension("context", "1.0").unwrap()
    });
    holds(g, "a statement that matches nothing", |g| {
        q(g, "MATCH (n:Nobody) SET n.x = 1");
    });
    holds(g, "a statement that sets what is set", |g| {
        q(g, "MATCH (n:P) WHERE n.x = 1 SET n.x = 1, n:P");
    });
    holds(g, "reads", |g| {
        q(g, "MATCH (n) RETURN count(n)");
        q(g, "STATS");
        q(g, "SCHEMA");
        q(g, "SHOW EXTENSIONS");
        let _ = g.node(a);
    });
    holds(g, "an empty commit", |g| {
        g.commit().unwrap();
        q(g, "BEGIN");
        q(g, "COMMIT");
    });
    holds(g, "a rolled-back transaction", |g| {
        q(g, "BEGIN");
        q(g, "CREATE (:Gone)");
        q(g, "MATCH (n:P) SET n.x = 42");
        q(g, "ROLLBACK");
    });
    holds(g, "a rolled-back API transaction", |g| {
        g.autocommit = false;
        g.add_node(&[], vec![]).unwrap();
        g.rollback().unwrap();
        g.autocommit = true;
    });
    holds(g, "a failed statement", |g| {
        assert!(query::execute(g, "CREATE (:A {x: 1}) WITH nonsense").is_err());
        assert!(g.set_node_prop(9999, "x", Value::Int(1)).is_err());
        // One that writes before it fails: a node with an edge cannot be
        // deleted without DETACH, and the statement leaves no trace.
        let src = format!("MATCH (n) WHERE id(n) = {a} SET n.gone = 1 DELETE n");
        assert!(query::execute(g, &src).is_err(), "{src}");
        assert_eq!(g.node_prop(a, "gone"), None);
        // Inside a transaction, a failed statement takes the transaction.
        q(g, "BEGIN");
        q(g, "CREATE (:Gone)");
        assert!(query::execute(g, &src).is_err());
        let _ = query::execute(g, "ROLLBACK");
        assert_eq!(g.label_count("Gone"), 0);
    });
    holds(g, "a checkpoint", |g| g.checkpoint().unwrap());
    holds(g, "COMPACT", |g| {
        q(g, "COMPACT");
    });
}

#[test]
fn revision_holds_for_everything_that_changes_nothing() {
    let root = temp("holds");
    for kind in KINDS {
        let dir = root.join(format!("{kind:?}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut g = make(kind, &dir);
        nothing_changes(&mut g);
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The same history gives the same revision on a file, on a memory graph
/// with a log and on one without: the last counts what a log would hold.
/// A log enabled later carries on from where it counted.
#[test]
fn every_kind_of_graph_counts_the_same_history_alike() {
    let root = temp("alike");
    let mut gs: Vec<Graph> = KINDS
        .iter()
        .map(|k| {
            let dir = root.join(format!("{k:?}"));
            std::fs::create_dir_all(&dir).unwrap();
            make(*k, &dir)
        })
        .collect();
    let steps = [
        "CREATE (:A {n: 1})-[:R {w: 2}]->(:B {s: \"x\"})",
        "MATCH (a:A) SET a.n = 2, a:C",
        "MATCH (a:A) SET a.n = 2",
        "INDEX ON :A(n)",
        "MATCH (b:B) REMOVE b.s",
        "MATCH ()-[r:R]->() DELETE r",
        "CLEAR",
        "CREATE (:A)",
    ];
    for s in steps {
        for g in gs.iter_mut() {
            q(g, s);
        }
        let revs: Vec<u64> = gs.iter().map(|g| g.revision()).collect();
        assert!(revs.iter().all(|r| *r == revs[0]), "after {s}: {revs:?}");
    }
    let mem = &mut gs[2];
    let before = mem.revision();
    mem.enable_log(log::DEFAULT_LOG_CAP).unwrap();
    assert_eq!(mem.revision(), before, "enabling a log keeps the revision");
    assert_eq!(mem.log_span(), Some((before, before)));
    for g in gs.iter_mut() {
        q(g, "CREATE (:Z)");
    }
    assert_eq!(gs[0].revision(), gs[2].revision());
    drop(gs);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn stats_reports_the_revision_and_the_generation() {
    let root = temp("stats");
    for kind in KINDS {
        let dir = root.join(format!("{kind:?}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut g = make(kind, &dir);
        q(&mut g, "CREATE (:A)");
        let rev = g.revision();
        assert!(rev > 0);
        assert_eq!(stat(&mut g, "revision"), Some(Value::Int(rev as i64)));
        let gen = glider::replica::generation_name(&g.generation());
        assert_eq!(gen.len(), 32);
        assert_eq!(stat(&mut g, "generation"), Some(Value::Text(gen.clone())));
        match kind {
            Kind::Memory => assert_eq!(gen, "0".repeat(32), "no log, no lineage"),
            _ => assert_ne!(gen, "0".repeat(32)),
        }
        let st = g.stats();
        assert_eq!((st.revision, st.generation), (rev, g.generation()));
        // What was reported before is reported as it was.
        assert_eq!(stat(&mut g, "nodes"), Some(Value::Int(1)));
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A clean close and reopen, and a crash before any checkpoint, both come
/// back at the revision of the last commit; uncommitted work is not counted.
#[test]
fn revision_survives_a_close_and_a_crash() {
    let root = temp("durable");
    let path = root.join("db");
    let mut g = open_small(&path);
    for i in 0..300 {
        q(
            &mut g,
            &format!("CREATE (:P {{i: {i}, pad: \"{}\"}})", "x".repeat(40)),
        );
    }
    let rev = g.revision();
    drop(g);
    assert_eq!(
        glider::storage::pager::read_superblock(&path)
            .unwrap()
            .wal_lsn,
        rev,
        "a clean close leaves the revision in the superblock"
    );
    let g = open_small(&path);
    assert_eq!(g.revision(), rev, "reopen");
    drop(g);
    let mut g = open_small(&path);
    assert_eq!(g.revision(), rev, "a reopen that wrote nothing");

    // A crash after commits that no checkpoint covers, with a transaction
    // open.
    g.set_checkpoint_bytes(u64::MAX);
    for i in 0..20 {
        q(
            &mut g,
            &format!("MATCH (n:P) WHERE n.i = {i} SET n.j = {i}"),
        );
    }
    let rev = g.revision();
    q(&mut g, "BEGIN");
    q(&mut g, "CREATE (:Uncommitted)");
    std::mem::forget(g);
    let mut g = Graph::open_forced(&path, Sync::Normal).unwrap();
    assert_eq!(g.revision(), rev, "recovery ends at the last commit");
    assert_eq!(g.label_count("Uncommitted"), 0);
    grows(&mut g, "writes after recovery", |g| {
        q(g, "CREATE (:After)");
    });
    drop(g);
    let _ = std::fs::remove_dir_all(&root);
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

fn tail_once(db: &Path, dir: &Path) {
    let opts = glider::wal::TailOptions {
        once: true,
        quiet: true,
        ..glider::wal::TailOptions::default()
    };
    glider::replica::tail(db, dir, &opts).unwrap();
}

/// A file replicated by `glider wal tail` and restored with `wal restore`
/// reports the source's revision at the point it was restored to.
#[test]
fn a_restored_file_reports_the_revision_of_the_point_restored_to() {
    let root = temp("file-replica");
    let db = root.join("db");
    let dir = root.join("replica");
    let mut g = open_small(&db);
    for i in 0..200 {
        q(&mut g, &format!("CREATE (:P {{i: {i}}})"));
    }
    drop(g);
    tail_once(&db, &dir);
    let mut g = open_small(&db);
    let early = g.revision();
    let early_dir = root.join("replica-early");
    copy_dir(&dir, &early_dir);
    for i in 0..200 {
        q(
            &mut g,
            &format!("MATCH (n:P) WHERE n.i = {i} SET n.k = {i}"),
        );
    }
    let late = g.revision();
    assert!(late > early);
    drop(g);
    tail_once(&db, &dir);

    let out = root.join("restored");
    glider::replica::restore(&dir, None, None, &out).unwrap();
    let r = open_small(&out);
    assert_eq!(r.revision(), late, "restored to the end");
    assert_eq!(r.generation(), open_small(&db).generation());
    drop(r);
    let out = root.join("restored-early");
    glider::replica::restore(&early_dir, None, None, &out).unwrap();
    let r = open_small(&out);
    assert_eq!(r.revision(), early, "restored to the earlier point");
    drop(r);
    let _ = std::fs::remove_dir_all(&root);
}

fn cfg() -> ReplicaConfig {
    ReplicaConfig {
        url: "test".into(),
        min_bytes: 0,
        snapshot_interval: std::time::Duration::from_secs(0),
        ..ReplicaConfig::default()
    }
}

/// A memory graph with a log, replicated through a backend, restored to
/// memory and to a file: both report the source's revision, and a replica
/// as it was earlier reports the earlier revision.
#[test]
fn a_restored_memory_replica_reports_the_revision_of_the_point_restored_to() {
    let root = temp("mem-replica");
    let store = root.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let backend: Arc<dyn Backend> = Arc::new(FileBackend {
        root: store.clone(),
    });
    let rep = MemoryReplicator::new(backend.clone(), cfg());
    let mut g = Graph::memory_with_page_size(1024, u64::MAX);
    g.enable_log(log::DEFAULT_LOG_CAP).unwrap();
    for i in 0..100 {
        q(&mut g, &format!("CREATE (:P {{i: {i}}})"));
    }
    let g = Mutex::new(g);
    rep.sync(&g, true, false).unwrap();
    let early = g.lock().unwrap().revision();
    let early_store = root.join("store-early");
    copy_dir(&store, &early_store);
    for i in 0..100 {
        q(
            &mut g.lock().unwrap(),
            &format!("MATCH (n:P) WHERE n.i = {i} SET n.k = {i}"),
        );
    }
    rep.sync(&g, false, false).unwrap();
    let late = g.lock().unwrap().revision();
    assert!(late > early);

    let (m, _) =
        memory::restore(backend.as_ref(), None, None, u64::MAX, log::DEFAULT_LOG_CAP).unwrap();
    assert_eq!(m.revision(), late, "restored to memory");
    assert_eq!(m.generation(), g.lock().unwrap().generation());
    let out = root.join("restored");
    paged::restore(backend.as_ref(), None, None, &out).unwrap();
    assert_eq!(open_small(&out).revision(), late, "restored to a file");

    let early_backend = FileBackend { root: early_store };
    let (m, _) =
        memory::restore(&early_backend, None, None, u64::MAX, log::DEFAULT_LOG_CAP).unwrap();
    assert_eq!(m.revision(), early, "restored to the earlier point");
    let _ = std::fs::remove_dir_all(&root);
}

/// A bulk load builds a file around the log; the loaded database is still
/// past the empty one the file began as, and stays there on reopen.
#[test]
fn a_bulk_loaded_database_is_past_revision_zero() {
    let root = temp("bulk");
    let path = root.join("db");
    let mut old = glider::legacy::graph::Graph::memory();
    let a = old.add_node(&["P".into()], vec![]).unwrap();
    let b = old.add_node(&["P".into()], vec![]).unwrap();
    old.add_edge(a, b, "K", vec![]).unwrap();
    let g = Graph::bulk_load(&path, &old, OpenOptions::default(), 1 << 20).unwrap();
    let rev = g.revision();
    assert!(rev > 0);
    assert_eq!(g.node_count(), 2);
    drop(g);
    assert_eq!(open_off(&path).revision(), rev);
    let _ = std::fs::remove_dir_all(&root);
}

/// Commits a second, on a file and in memory: one node added and one
/// property set per transaction. Run before and after a change to the write
/// path with `cargo test --release --test revision -- --ignored --nocapture`.
#[test]
#[ignore]
fn write_loop_timing() {
    const N: u64 = 50_000;
    let dir = temp("bench");
    for round in 0..3 {
        let path = dir.join(format!("w{round}.db"));
        let mut g = open_off(&path);
        let t = Instant::now();
        for i in 0..N {
            let id = g
                .add_node(&["P".into()], vec![("i".into(), Value::Int(i as i64))])
                .unwrap();
            g.set_node_prop(id, "j", Value::Int(i as i64 + 1)).unwrap();
        }
        let file = t.elapsed();
        drop(g);
        let mut m = Graph::memory();
        let t = Instant::now();
        for i in 0..N {
            let id = m
                .add_node(&["P".into()], vec![("i".into(), Value::Int(i as i64))])
                .unwrap();
            m.set_node_prop(id, "j", Value::Int(i as i64 + 1)).unwrap();
        }
        let mem = t.elapsed();
        println!(
            "round {round}: file {:.0} commits/s, memory {:.0} commits/s",
            2.0 * N as f64 / file.as_secs_f64(),
            2.0 * N as f64 / mem.as_secs_f64()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
