//! Regression tests for the revision (docs/DERIVED.md, "Revision"), from its
//! review: crashes and partial failures, workloads of writes that change
//! nothing compared across recovery and replicas, a memory log enabled late,
//! and what holds for a copy restored and then written to.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
    let p = std::env::temp_dir().join(format!("glider-revreg-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn opts() -> OpenOptions {
    OpenOptions {
        page_size: 1024,
        cache_size: 64 << 10,
        checkpoint_bytes: 8 << 10,
        wal_segment: 4 << 10,
        ..OpenOptions::default()
    }
}

fn open_small(path: &Path) -> Graph {
    Graph::open_opts(path, opts()).unwrap()
}

fn q(g: &mut Graph, src: &str) -> query::QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

/// Everything a user can see, by name, in id order.
fn export(g: &Graph) -> String {
    let mut out = String::new();
    for id in g.node_ids() {
        let n = g.node(id).unwrap();
        let mut labels: Vec<String> = n
            .labels()
            .iter()
            .map(|l| g.strings.name(*l).to_string())
            .collect();
        labels.sort();
        let props: Vec<String> = n
            .props()
            .iter()
            .map(|(k, v)| format!("{}={:?}", g.strings.name(*k), v))
            .collect();
        out += &format!("N{id} {labels:?} {props:?}\n");
    }
    for id in g.edge_ids() {
        let e = g.edge(id).unwrap();
        let props: Vec<String> = e
            .props()
            .iter()
            .map(|(k, v)| format!("{}={:?}", g.strings.name(*k), v))
            .collect();
        out += &format!(
            "E{id} {}->{} {} {props:?}\n",
            e.from,
            e.to,
            g.strings.name(e.etype)
        );
    }
    out += &format!("indexes {:?}\n", g.indexes());
    out += &format!("extensions {:?}\n", g.extensions());
    let mut ext = Vec::new();
    g.ext_scan("context", b"", &mut |k, v| {
        ext.push((k.to_vec(), v.to_vec()));
        true
    });
    out += &format!("ext {ext:?}\n");
    out += &format!("counts {} {}\n", g.node_count(), g.edge_count());
    out
}

/// A workload with many writes that change nothing, mixed with real ones.
fn noop_workload(g: &mut Graph) {
    q(g, "CREATE EXTENSION context");
    q(g, "CALL context.index()");
    for i in 0..60 {
        q(
            g,
            &format!("CREATE (:P {{i: {i}, name: \"p{i}\", f: 1.5}})"),
        );
    }
    q(
        g,
        "MATCH (a:P), (b:P) WHERE a.i + 1 = b.i CREATE (a)-[:NEXT {w: 1}]->(b)",
    );
    for round in 0..4 {
        q(g, "MATCH (n:P) SET n.i = n.i, n.name = n.name, n:P");
        q(g, "MATCH ()-[r:NEXT]->() SET r.w = 1");
        q(g, "MATCH (n:P) REMOVE n.nope, n:Nope");
        q(g, "INDEX ON :P(i)");
        q(g, "CREATE EXTENSION IF NOT EXISTS context");
        q(
            g,
            &format!("MATCH (n:P) WHERE n.i = {round} SET n.name = \"q{round}\""),
        );
        q(g, "MATCH (n:Nobody) DETACH DELETE n");
        q(g, "MATCH (n:P) SET n.f = 1.5");
        q(g, "BEGIN");
        q(g, "MATCH (n:P) SET n.i = n.i");
        q(g, &format!("CREATE (:Q {{r: {round}}})"));
        q(g, "COMMIT");
        q(g, "BEGIN");
        q(g, "MATCH (n:P) SET n.gone = 1");
        q(g, "ROLLBACK");
    }
}

fn cfg() -> ReplicaConfig {
    ReplicaConfig {
        url: "test".into(),
        min_bytes: 0,
        snapshot_interval: std::time::Duration::from_secs(0),
        ..ReplicaConfig::default()
    }
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

/// A workload full of writes that change nothing, mixed with real ones: the
/// source, its crash recovery and a replica restored from base and log agree
/// in content, revision and next id.
#[test]
fn a_workload_of_writes_that_change_nothing_agrees_across_source_recovery_and_replica() {
    let root = temp("noop-file");
    let db = root.join("db");
    let dir = root.join("replica");
    let mut g = open_small(&db);
    q(&mut g, "CREATE (:Seed)");
    drop(g);
    tail_once(&db, &dir);
    let mut g = open_small(&db);
    g.set_checkpoint_bytes(u64::MAX);
    noop_workload(&mut g);
    let rev = g.revision();
    let want = export(&g);
    // Crash: copy the files with the log uncheckpointed.
    let crash = root.join("crash");
    std::fs::create_dir_all(&crash).unwrap();
    std::fs::copy(&db, crash.join("db")).unwrap();
    copy_dir(
        &glider::storage::log::wal_dir(&db),
        &glider::storage::log::wal_dir(&crash.join("db")),
    );
    std::mem::forget(g);
    let mut c = Graph::open_forced(&crash.join("db"), Sync::Normal).unwrap();
    assert_eq!(c.revision(), rev, "crash recovery revision");
    assert_eq!(export(&c), want, "crash recovery content");
    let mut g = Graph::open_forced(&db, Sync::Normal).unwrap();
    assert_eq!(g.revision(), rev);
    tail_once(&db, &dir);
    let out = root.join("restored");
    glider::replica::restore(&dir, None, None, &out).unwrap();
    let mut r = open_small(&out);
    assert_eq!(r.revision(), rev, "replica revision");
    assert_eq!(export(&r), want, "replica content");
    // The id counters agree too.
    let a = g.add_node(&[], vec![]).unwrap();
    let b = r.add_node(&[], vec![]).unwrap();
    let cc = c.add_node(&[], vec![]).unwrap();
    assert_eq!((a, a), (b, cc));
    assert_eq!(g.revision(), r.revision());
    drop((g, r, c));
    let _ = std::fs::remove_dir_all(&root);
}

/// A memory graph that committed without a log, then enabled one: a base
/// taken right after, and the log shipped later, restore to memory and to a
/// file matching the source.
#[test]
fn a_memory_log_enabled_after_unlogged_commits_replicates_exactly() {
    let root = temp("late-log");
    let store = root.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let backend: Arc<dyn Backend> = Arc::new(FileBackend {
        root: store.clone(),
    });
    let rep = MemoryReplicator::new(backend.clone(), cfg());
    let mut g = Graph::memory_with_page_size(1024, u64::MAX);
    for i in 0..30 {
        q(&mut g, &format!("CREATE (:Pre {{i: {i}}})"));
    }
    let before = g.revision();
    assert!(before > 0);
    g.enable_log(log::DEFAULT_LOG_CAP).unwrap();
    let g = Mutex::new(g);
    // A base right after the log was enabled.
    rep.sync(&g, true, false).unwrap();
    {
        let (m, _) =
            memory::restore(backend.as_ref(), None, None, u64::MAX, log::DEFAULT_LOG_CAP).unwrap();
        let src = g.lock().unwrap();
        assert_eq!(m.revision(), src.revision());
        assert_eq!(export(&m), export(&src));
    }
    noop_workload(&mut g.lock().unwrap());
    rep.sync(&g, false, false).unwrap();
    let src = g.lock().unwrap();
    let (mut m, _) =
        memory::restore(backend.as_ref(), None, None, u64::MAX, log::DEFAULT_LOG_CAP).unwrap();
    assert_eq!(m.revision(), src.revision(), "memory replica revision");
    assert_eq!(export(&m), export(&src), "memory replica content");
    let out = root.join("restored");
    paged::restore(backend.as_ref(), None, None, &out).unwrap();
    let f = open_small(&out);
    assert_eq!(f.revision(), src.revision(), "file restore revision");
    assert_eq!(export(&f), export(&src), "file restore content");
    drop(src);
    let a = g.lock().unwrap().add_node(&[], vec![]).unwrap();
    let b = m.add_node(&[], vec![]).unwrap();
    assert_eq!(a, b);
    drop(f);
    let _ = std::fs::remove_dir_all(&root);
}

/// A statement that runs out of memory part-way leaves content and revision
/// as they were; with no-op sets and a refused value later, a replica agrees.
#[test]
fn a_statement_that_runs_out_of_memory_leaves_the_revision_and_the_replica_agrees() {
    let root = temp("full");
    let store = root.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let backend: Arc<dyn Backend> = Arc::new(FileBackend {
        root: store.clone(),
    });
    let rep = MemoryReplicator::new(backend.clone(), cfg());
    let mut g = Graph::memory_with_page_size(1024, 4 << 20);
    g.enable_log(log::DEFAULT_LOG_CAP).unwrap();
    for i in 0..50 {
        q(&mut g, &format!("CREATE (:S {{i: {i}}})"));
    }
    let g = Mutex::new(g);
    rep.sync(&g, true, false).unwrap();
    {
        let mut g = g.lock().unwrap();
        let rev = g.revision();
        let state = export(&g);
        let pad = "x".repeat(500);
        let r = query::execute(
            &mut g,
            &format!(
                "MATCH (a:S), (b:S) CREATE (a)-[:R {{pad: \"{pad}\"}}]->(:B {{pad: \"{pad}\"}})"
            ),
        );
        println!("big statement: {:?}", r.as_ref().err());
        assert!(r.is_err(), "the statement should run out of memory");
        assert_eq!(g.revision(), rev);
        assert_eq!(export(&g), state);
        g.set_max_memory(u64::MAX);
        q(
            &mut g,
            "MATCH (n:S) WHERE n.i < 10 SET n.k = n.i, n.i = n.i",
        );
        // A failing value check part-way through a transaction.
        g.autocommit = false;
        let a = g.add_node(&["T".into()], vec![]).unwrap();
        let mut deep = Value::Int(1);
        for _ in 0..100 {
            deep = Value::List(vec![deep]);
        }
        assert!(g.set_node_prop(a, "d", deep).is_err());
        g.set_node_prop(a, "ok", Value::Int(1)).unwrap();
        g.commit().unwrap();
        g.autocommit = true;
    }
    rep.sync(&g, false, false).unwrap();
    let src = g.lock().unwrap();
    let (m, _) =
        memory::restore(backend.as_ref(), None, None, u64::MAX, log::DEFAULT_LOG_CAP).unwrap();
    assert_eq!(m.revision(), src.revision());
    assert_eq!(export(&m), export(&src));
    drop(src);
    let _ = std::fs::remove_dir_all(&root);
}

/// Inside BEGIN: no-op sets with real ones, a refused value, a checkpoint
/// asked for mid-transaction; then a statement that writes and fails; a crash
/// copy recovers to the same content and revision.
#[test]
fn a_transaction_with_a_refused_value_and_a_checkpoint_inside_recovers_exactly() {
    let root = temp("file-txn");
    let db = root.join("db");
    let mut g = open_small(&db);
    g.set_checkpoint_bytes(u64::MAX);
    for i in 0..30 {
        q(&mut g, &format!("CREATE (:S {{i: {i}}})"));
    }
    g.checkpoint().unwrap();
    q(&mut g, "BEGIN");
    q(&mut g, "MATCH (n:S) WHERE n.i < 5 SET n.i = n.i, n.k = 1");
    let mut deep = Value::Int(1);
    for _ in 0..100 {
        deep = Value::List(vec![deep]);
    }
    assert!(g.set_node_prop(1, "d", deep).is_err());
    g.checkpoint().unwrap();
    q(&mut g, "MATCH (n:S) WHERE n.i = 7 SET n.i = 7, n:L");
    q(&mut g, "COMMIT");
    // A statement that fails part-way after writing, autocommit.
    let src = "MATCH (n:S) WHERE n.i = 9 SET n.gone = 1 DELETE n";
    q(
        &mut g,
        "MATCH (a:S), (b:S) WHERE a.i = 9 AND b.i = 10 CREATE (a)-[:E]->(b)",
    );
    assert!(query::execute(&mut g, src).is_err());
    let rev = g.revision();
    let want = export(&g);
    let crash = root.join("crash");
    std::fs::create_dir_all(&crash).unwrap();
    std::fs::copy(&db, crash.join("db")).unwrap();
    copy_dir(&log::wal_dir(&db), &log::wal_dir(&crash.join("db")));
    let c = Graph::open_forced(&crash.join("db"), Sync::Normal).unwrap();
    assert_eq!(c.revision(), rev);
    assert_eq!(export(&c), want);
    drop((g, c));
    let _ = std::fs::remove_dir_all(&root);
}

/// Over a long history across segments and checkpoints, a memory graph
/// without a log counts exactly what a file and a logged memory graph log.
#[test]
fn a_memory_graph_without_a_log_counts_what_a_file_logs_over_a_long_history() {
    let root = temp("alike-long");
    let mut f = open_small(&root.join("db"));
    let mut m = Graph::memory_with_page_size(1024, u64::MAX);
    let mut l = Graph::memory_with_page_size(1024, u64::MAX);
    l.enable_log(log::DEFAULT_LOG_CAP).unwrap();
    for g in [&mut f, &mut m, &mut l] {
        noop_workload(g);
    }
    assert_eq!((f.revision(), f.revision()), (m.revision(), l.revision()));
    drop(f);
    let _ = std::fs::remove_dir_all(&root);
}

/// What holds around a restore: a copy restored to an earlier point and
/// then written to is past the point it was restored to, and the full-text
/// index restored with it follows the copy's own writes, agreeing with its
/// graph (as a rebuild from scratch would make it) and not with the
/// source's, which went another way.
#[test]
fn a_restored_copy_written_to_is_past_its_restore_point_and_its_derived_data_follows_it() {
    let root = temp("fork");
    let db = root.join("db");
    let dir = root.join("replica");
    let mut g = open_small(&db);
    q(&mut g, "CREATE EXTENSION context");
    for i in 0..20 {
        q(
            &mut g,
            &format!("CREATE (:P {{i: {i}, name: \"alpha {i}\"}})"),
        );
    }
    q(&mut g, "CALL context.index()");
    drop(g);
    tail_once(&db, &dir);
    let early_dir = root.join("replica-early");
    copy_dir(&dir, &early_dir);

    // The source goes one way.
    let mut g = open_small(&db);
    let point = g.revision();
    q(&mut g, "MATCH (n:P) WHERE n.i = 3 SET n.name = \"gamma\"");
    let source_ext = export(&g);
    drop(g);

    // A copy of the earlier point goes another.
    let out = root.join("restored");
    glider::replica::restore(&early_dir, None, None, &out).unwrap();
    let mut r = open_small(&out);
    assert_eq!(r.revision(), point, "restored to the point");
    q(&mut r, "MATCH (n:P) WHERE n.i = 4 SET n.name = \"delta\"");
    assert!(
        r.revision() > point,
        "a change takes the copy past its point"
    );
    let kept = export(&r);
    assert_ne!(kept, source_ext, "the copy and the source differ");
    // The copy's index is what a rebuild over the copy's own graph gives.
    q(&mut r, "CALL context.index()");
    assert_eq!(
        export(&r),
        kept,
        "the copy's derived data agrees with its graph"
    );
    // A search that matches nothing is an error that says so.
    let found = |g: &mut Graph, word: &str| {
        query::execute(
            g,
            &format!("CALL context.retrieve(query: \"{word}\", format: \"json\")"),
        )
        .map(|res| format!("{:?}", res.rows).contains(word))
        .unwrap_or(false)
    };
    assert!(found(&mut r, "delta"));
    assert!(
        !found(&mut r, "gamma"),
        "nothing of the source's later writes"
    );
    drop(r);
    let _ = std::fs::remove_dir_all(&root);
}

/// The same for a memory replica restored twice and written differently:
/// each copy is past the point it was restored to.
#[test]
fn memory_copies_written_differently_are_each_past_their_restore_point() {
    let root = temp("mem-fork");
    let store = root.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let backend: Arc<dyn Backend> = Arc::new(FileBackend {
        root: store.clone(),
    });
    let rep = MemoryReplicator::new(backend.clone(), cfg());
    let mut g = Graph::memory_with_page_size(1024, u64::MAX);
    g.enable_log(log::DEFAULT_LOG_CAP).unwrap();
    q(&mut g, "CREATE (:P {i: 1})");
    let point = g.revision();
    let g = Mutex::new(g);
    rep.sync(&g, true, false).unwrap();
    for v in [2, 3] {
        let (mut c, _) =
            memory::restore(backend.as_ref(), None, None, u64::MAX, log::DEFAULT_LOG_CAP).unwrap();
        assert_eq!(c.revision(), point);
        q(&mut c, &format!("MATCH (n:P) SET n.i = {v}"));
        assert!(c.revision() > point);
    }
    let _ = std::fs::remove_dir_all(&root);
}
