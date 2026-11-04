//! glider-stream for paged databases: the paged tailer ships into a spool,
//! the stream layer uploads it to a backend, and a restore from that backend
//! reproduces the source exactly — through failed uploads, forced bases,
//! retention and point-in-time restores. The file backend stands in for S3
//! (the same `Backend` trait); tests/test_s3.py runs the binaries against a
//! signature-checking S3 server.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glider::graph::{Graph, OpenOptions};
use glider::query;
use glider::stream::backend::{Backend, Entry, FileBackend};
use glider::stream::config::ReplicaConfig;
use glider::stream::paged::{self, PagedReplicator};
use glider::value::Value;

fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("glider-pstream-{}-{}", name, std::process::id()));
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
            checkpoint_bytes: 8 << 10,
            wal_segment: 4 << 10,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

struct Writer {
    n: u64,
}

impl Writer {
    fn step(&mut self, g: &mut Graph) {
        self.n += 1;
        let i = self.n;
        let a = g
            .add_node(
                &["P".into()],
                vec![
                    ("i".into(), Value::Int(i as i64)),
                    ("pad".into(), Value::Text("x".repeat((i % 50) as usize))),
                ],
            )
            .unwrap();
        if i > 3 {
            let _ = g.add_edge(
                a,
                1 + (i * 7919) % (a - 1),
                "K",
                vec![("w".into(), Value::Int(i as i64))],
            );
        }
        if i % 5 == 0 {
            let _ = g.delete_node(1 + (i * 104729) % a);
        }
        if i % 3 == 0 {
            let _ = g.set_node_prop(1 + (i * 31) % a, "touched", Value::Int(i as i64));
        }
        g.commit().unwrap();
    }
}

/// A backend whose puts fail while `failing` is set.
struct Flaky {
    inner: FileBackend,
    failing: AtomicBool,
}

impl Backend for Flaky {
    fn put(&self, key: &str, body: Vec<u8>) -> io::Result<()> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected upload failure"));
        }
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
        "flaky".into()
    }
}

fn replicator(db: &Path, root: &Path, backend: Arc<dyn Backend>) -> PagedReplicator {
    let cfg = ReplicaConfig {
        url: "test".into(),
        min_bytes: 0,
        sync_interval: Duration::from_millis(200),
        // Bases only when a test asks for one.
        snapshot_interval: Duration::from_secs(0),
        ..ReplicaConfig::default()
    };
    PagedReplicator::new(db, root.join("spool"), backend, cfg)
}

/// One pass while the writer keeps committing: a base needs the writer to
/// checkpoint under the hold, which it does at its next commit.
fn sync_while_writing(
    g: &mut Graph,
    w: &mut Writer,
    r: &PagedReplicator,
    force: bool,
) -> io::Result<paged::SyncReport> {
    std::thread::scope(|sc| {
        let h = sc.spawn(|| r.sync(force, false));
        let start = Instant::now();
        while !h.is_finished() {
            w.step(g);
            std::thread::sleep(Duration::from_millis(2));
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "the pass never finished"
            );
        }
        h.join().unwrap()
    })
}

fn restore_and_compare(g: &Graph, backend: &dyn Backend, out: &Path) {
    let _ = std::fs::remove_file(out);
    paged::restore(backend, None, None, out).unwrap();
    let r = Graph::open(out, glider::store::Sync::Normal).unwrap();
    r.verify_trees().unwrap();
    assert_eq!(
        query::export_jsonl(&r),
        query::export_jsonl(g),
        "restored graph equals the source"
    );
}

#[test]
fn uploads_a_base_and_the_log_and_restores_the_source_exactly() {
    let root = temp("basic");
    let db = root.join("db");
    let remote = Arc::new(FileBackend {
        root: root.join("remote"),
    });
    let mut g = open(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..300 {
        w.step(&mut g);
    }
    let r = replicator(&db, &root, remote.clone());
    let first = sync_while_writing(&mut g, &mut w, &r, false).unwrap();
    assert_eq!(first.bases_uploaded, 1, "the first pass uploads a base");
    for _ in 0..500 {
        w.step(&mut g);
    }
    let second = r.sync(false, false).unwrap();
    assert_eq!(second.bases_uploaded, 0, "no new base needed");
    assert!(second.segments_uploaded > 0, "the log goes up");

    let gen = glider::replica::generation_hex(&db).unwrap();
    let l = paged::listing(remote.as_ref(), &gen).unwrap();
    assert_eq!(l.bases.len(), 1);
    assert_eq!(
        r.lag(&gen).unwrap(),
        0,
        "everything committed is in the backend"
    );
    // The spool keeps only base.json for an uploaded base.
    let spooled = root
        .join("spool")
        .join(&gen)
        .join("base")
        .join(format!("{:016x}", l.bases[0].lsn));
    assert!(spooled.join("base.json").exists() && !spooled.join("db").exists());

    restore_and_compare(&g, remote.as_ref(), &root.join("restored"));
}

#[test]
fn a_failed_upload_is_retried_on_the_next_pass_not_lost() {
    let root = temp("flaky");
    let db = root.join("db");
    let remote = Arc::new(Flaky {
        inner: FileBackend {
            root: root.join("remote"),
        },
        failing: AtomicBool::new(false),
    });
    let mut g = open(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..200 {
        w.step(&mut g);
    }
    let r = replicator(&db, &root, remote.clone());
    sync_while_writing(&mut g, &mut w, &r, false).unwrap();
    for _ in 0..300 {
        w.step(&mut g);
    }
    remote.failing.store(true, Ordering::SeqCst);
    assert!(r.sync(false, false).is_err(), "the upload fails");
    remote.failing.store(false, Ordering::SeqCst);
    for _ in 0..100 {
        w.step(&mut g);
    }
    let again = r.sync(false, false).unwrap();
    assert!(again.segments_uploaded > 0, "what failed goes up now");
    restore_and_compare(&g, remote.as_ref(), &root.join("restored"));
}

#[test]
fn forced_bases_and_retention_keep_the_replica_restorable() {
    let root = temp("retention");
    let db = root.join("db");
    let remote = Arc::new(FileBackend {
        root: root.join("remote"),
    });
    let mut g = open(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..200 {
        w.step(&mut g);
    }
    let mut r = replicator(&db, &root, remote.clone());
    sync_while_writing(&mut g, &mut w, &r, false).unwrap();
    for _ in 0..200 {
        w.step(&mut g);
    }
    std::thread::sleep(Duration::from_millis(1100));
    let forced = sync_while_writing(&mut g, &mut w, &r, true).unwrap();
    assert_eq!(forced.bases_uploaded, 1, "a forced base");
    let gen = glider::replica::generation_hex(&db).unwrap();
    assert_eq!(
        paged::listing(remote.as_ref(), &gen).unwrap().bases.len(),
        2
    );

    std::thread::sleep(Duration::from_millis(1100));
    r.cfg.retention = Duration::from_secs(0);
    let deleted = r.enforce_retention().unwrap();
    assert!(deleted >= 1, "the older base expires");
    let l = paged::listing(remote.as_ref(), &gen).unwrap();
    assert_eq!(l.bases.len(), 1, "the newest base is never expired");
    assert!(
        l.segments.iter().all(|s| s.seg.end() > l.bases[0].lsn),
        "no segment below the oldest base kept"
    );

    for _ in 0..50 {
        w.step(&mut g);
    }
    r.sync(false, false).unwrap();
    restore_and_compare(&g, remote.as_ref(), &root.join("restored"));
}

#[test]
fn restore_as_of_stops_at_the_log_shipped_by_then() {
    let root = temp("asof");
    let db = root.join("db");
    let remote = Arc::new(FileBackend {
        root: root.join("remote"),
    });
    let mut g = open(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..200 {
        w.step(&mut g);
    }
    let r = replicator(&db, &root, remote.clone());
    sync_while_writing(&mut g, &mut w, &r, false).unwrap();
    r.sync(false, false).unwrap();
    let then = query::export_jsonl(&g);
    let t = glider::wal::now_unix();
    std::thread::sleep(Duration::from_millis(1100));
    for _ in 0..300 {
        w.step(&mut g);
    }
    r.sync(false, false).unwrap();

    let out = root.join("then");
    paged::restore(remote.as_ref(), None, Some(t), &out).unwrap();
    let back = Graph::open(&out, glider::store::Sync::Normal).unwrap();
    back.verify_trees().unwrap();
    assert_eq!(query::export_jsonl(&back), then, "the graph as it was at t");
    restore_and_compare(&g, remote.as_ref(), &root.join("now"));
}

#[test]
fn a_paged_replica_is_recognised_and_listed() {
    let root = temp("listing");
    let db = root.join("db");
    let remote = Arc::new(FileBackend {
        root: root.join("remote"),
    });
    let mut g = open(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..100 {
        w.step(&mut g);
    }
    assert!(
        !paged::is_paged_replica(remote.as_ref()).unwrap(),
        "empty is not a replica"
    );
    let r = replicator(&db, &root, remote.clone());
    sync_while_writing(&mut g, &mut w, &r, false).unwrap();
    assert!(paged::is_paged_replica(remote.as_ref()).unwrap());
    let gens = paged::generations(remote.as_ref()).unwrap();
    assert_eq!(gens, vec![glider::replica::generation_hex(&db).unwrap()]);
    let line = paged::describe(&paged::listing(remote.as_ref(), &gens[0]).unwrap());
    assert!(line.contains("1 base(s)"), "{line}");
}
