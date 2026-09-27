//! Replication of a paged database: base snapshots taken under a hold while
//! the writer keeps writing and checkpointing, log shipping, a new base when
//! the log needed is gone, and restores that reproduce the source exactly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glider::graph::{Graph, OpenOptions};
use glider::query;
use glider::value::Value;
use glider::wal::TailOptions;

fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("glider-repl-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn open(path: &Path) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            page_size: 1024,
            cache_size: 64 << 10,
            // Checkpoint often, so logs are truncated and pages reused.
            checkpoint_bytes: 8 << 10,
            wal_segment: 4 << 10,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

fn once() -> TailOptions {
    TailOptions {
        once: true,
        quiet: true,
        ..TailOptions::default()
    }
}

struct Writer {
    n: u64,
}

impl Writer {
    /// A small transaction that adds, changes and deletes.
    fn step(&mut self, g: &mut Graph) {
        self.n += 1;
        let i = self.n;
        let a = g
            .add_node(&["P".into()], vec![("i".into(), Value::Int(i as i64)), ("pad".into(), Value::Text("x".repeat((i % 50) as usize)))])
            .unwrap();
        if i > 3 {
            let _ = g.add_edge(a, 1 + (i * 7919) % (a - 1), "K", vec![("w".into(), Value::Int(i as i64))]);
        }
        if i % 5 == 0 {
            let victim = 1 + (i * 104729) % a;
            let _ = g.delete_node(victim);
        }
        if i % 3 == 0 {
            let _ = g.set_node_prop(1 + (i * 31) % a, "touched", Value::Int(i as i64));
        }
        g.commit().unwrap();
    }
}

/// Run `tail --once` in a thread while the writer keeps committing (a base
/// needs the writer to checkpoint, which it does at its next commit).
fn tail_while_writing(g: &mut Graph, w: &mut Writer, db: &Path, dir: &Path) {
    let done = Arc::new(AtomicBool::new(false));
    let (db2, dir2, done2) = (db.to_path_buf(), dir.to_path_buf(), done.clone());
    let t = std::thread::spawn(move || {
        let r = glider::replica::tail(&db2, &dir2, &once());
        done2.store(true, Ordering::SeqCst);
        r
    });
    let start = Instant::now();
    while !done.load(Ordering::SeqCst) {
        w.step(g);
        std::thread::sleep(Duration::from_millis(2));
        assert!(start.elapsed() < Duration::from_secs(60), "tailer never finished");
    }
    t.join().unwrap().unwrap();
}

fn restore_and_compare(g: &Graph, dir: &Path, out: &Path) {
    let _ = std::fs::remove_file(out);
    glider::replica::restore(dir, None, None, out).unwrap();
    let r = Graph::open(out, glider::store::Sync::Normal).unwrap();
    r.verify_trees().unwrap();
    assert_eq!(query::export_jsonl(&r), query::export_jsonl(g), "restored graph equals the source");
    assert_eq!(r.node_count(), g.node_count());
    assert_eq!(r.edge_count(), g.edge_count());
}

#[test]
fn base_and_log_restore_the_source_exactly() {
    let root = temp("basic");
    let db = root.join("db");
    let dir = root.join("replica");
    let out = root.join("restored");
    let mut g = open(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..300 {
        w.step(&mut g);
    }
    // First run: a base, taken while the writer keeps going.
    tail_while_writing(&mut g, &mut w, &db, &dir);
    assert_eq!(glider::replica::bases(&dir, &gen(&db)).unwrap().len(), 1);
    for _ in 0..500 {
        w.step(&mut g);
    }
    // Later runs just ship the log, which the writer kept for us despite
    // many checkpoints.
    glider::replica::tail(&db, &dir, &once()).unwrap();
    assert_eq!(glider::replica::bases(&dir, &gen(&db)).unwrap().len(), 1, "no new base needed");
    restore_and_compare(&g, &dir, &out);

    for _ in 0..200 {
        w.step(&mut g);
    }
    glider::replica::tail(&db, &dir, &once()).unwrap();
    restore_and_compare(&g, &dir, &out);
    let st = glider::replica::verify(&dir).unwrap();
    assert_eq!(st.len(), 1);
    assert!(st[0].gap_at.is_none());
    drop(g);
    let _ = std::fs::remove_dir_all(&root);
}

fn gen(db: &Path) -> String {
    let sb = glider::storage::pager::read_superblock(db).unwrap();
    sb.generation.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn a_replica_that_fell_behind_takes_a_new_base() {
    let root = temp("behind");
    let db = root.join("db");
    let dir = root.join("replica");
    let out = root.join("restored");
    let mut g = open(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..200 {
        w.step(&mut g);
    }
    tail_while_writing(&mut g, &mut w, &db, &dir);
    // The replicator goes away and its pin with it: the writer checkpoints
    // the log it had not shipped away.
    glider::storage::db::remove_pin(&db);
    // The writer looks at the pin at most once a second.
    std::thread::sleep(Duration::from_millis(1100));
    for _ in 0..600 {
        w.step(&mut g);
    }
    tail_while_writing(&mut g, &mut w, &db, &dir);
    assert_eq!(glider::replica::bases(&dir, &gen(&db)).unwrap().len(), 2, "a second base");
    glider::replica::tail(&db, &dir, &once()).unwrap();
    restore_and_compare(&g, &dir, &out);
    drop(g);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_database_with_no_writer_is_copied_under_its_lock() {
    let root = temp("idle");
    let db = root.join("db");
    let dir = root.join("replica");
    let out = root.join("restored");
    {
        let mut g = open(&db);
        let mut w = Writer { n: 0 };
        for _ in 0..400 {
            w.step(&mut g);
        }
    }
    glider::replica::tail(&db, &dir, &once()).unwrap();
    let g = open(&db);
    restore_and_compare(&g, &dir, &out);
    drop(g);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn restore_as_of_stops_at_the_log_shipped_by_then() {
    let root = temp("asof");
    let db = root.join("db");
    let dir = root.join("replica");
    let out = root.join("restored");
    let mut g = open(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..100 {
        w.step(&mut g);
    }
    tail_while_writing(&mut g, &mut w, &db, &dir);
    glider::replica::tail(&db, &dir, &once()).unwrap();
    let snapshot = query::export_jsonl(&g);
    let cut = glider::wal::now_unix();
    std::thread::sleep(Duration::from_millis(1100));
    for _ in 0..100 {
        w.step(&mut g);
    }
    glider::replica::tail(&db, &dir, &once()).unwrap();
    glider::replica::restore(&dir, None, Some(cut), &out).unwrap();
    let r = Graph::open(&out, glider::store::Sync::Normal).unwrap();
    assert_eq!(query::export_jsonl(&r), snapshot);
    drop(g);
    let _ = std::fs::remove_dir_all(&root);
}
