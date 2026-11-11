//! Read replicas: a follower restores the primary's replica from a backend
//! and keeps applying the log it ships, and must equal the primary after
//! every step — through reopening, log the follower missed (retention past
//! it), and a primary that starts a new generation. The file backend stands
//! in for S3, as in tests/paged_stream.rs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glider::follow::{Follower, Plan};
use glider::graph::{Graph, OpenOptions};
use glider::query;
use glider::stream::backend::{Backend, FileBackend};
use glider::stream::config::ReplicaConfig;
use glider::stream::paged::{self, PagedReplicator};
use glider::value::Value;

fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("glider-follow-{}-{}", name, std::process::id()));
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

fn open(path: &Path) -> Graph {
    Graph::open_opts(path, opts()).unwrap()
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
            let _ = g.add_edge(a, 1 + (i * 7919) % (a - 1), "K", vec![("w".into(), Value::Int(i as i64))]);
        }
        if i % 5 == 0 {
            let _ = g.delete_node(1 + (i * 104729) % a);
        }
        if i % 3 == 0 {
            let _ = g.set_node_prop(1 + (i * 31) % a, "touched", Value::Int(i as i64));
        }
        if i == 40 {
            g.create_index("P", "i").unwrap();
        }
        g.commit().unwrap();
    }
}

fn replicator(db: &Path, root: &Path, backend: Arc<dyn Backend>) -> PagedReplicator {
    let cfg = ReplicaConfig {
        url: "test".into(),
        min_bytes: 0,
        sync_interval: Duration::from_millis(200),
        snapshot_interval: Duration::from_secs(0),
        ..ReplicaConfig::default()
    };
    PagedReplicator::new(db, root.join("spool"), backend, cfg)
}

/// One pass while the writer keeps committing (a base needs the writer to
/// checkpoint under the replicator's hold, which it does on commit).
fn sync_while_writing(g: &mut Graph, w: &mut Writer, r: &PagedReplicator, force: bool) {
    std::thread::scope(|sc| {
        let h = sc.spawn(|| r.sync(force, false));
        let start = Instant::now();
        while !h.is_finished() {
            w.step(g);
            std::thread::sleep(Duration::from_millis(2));
            assert!(start.elapsed() < Duration::from_secs(60), "the pass never finished");
        }
        h.join().unwrap().unwrap();
    });
}

/// Step the follower until it has nothing left to apply.
fn catch_up(f: &mut Follower, g: &mut Graph) {
    for _ in 0..1000 {
        match f.plan(&g.generation_hex(), g.log_position()).unwrap() {
            Plan::Idle { .. } => return,
            _ => {
                if let Some(fresh) = f.step(g).unwrap() {
                    *g = fresh;
                }
            }
        }
    }
    panic!("the follower never caught up");
}

fn same(primary: &Graph, replica: &Graph, when: &str) {
    replica.verify_trees().unwrap();
    assert_eq!(query::export_jsonl(replica), query::export_jsonl(primary), "replica equals primary {when}");
    assert_eq!(replica.indexes(), primary.indexes(), "same indexes {when}");
}

fn setup(name: &str) -> (PathBuf, PathBuf, Arc<FileBackend>, Graph, Writer, PagedReplicator) {
    let root = temp(name);
    let db = root.join("primary.gldb");
    let remote = Arc::new(FileBackend { root: root.join("remote") });
    let mut g = open(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..200 {
        w.step(&mut g);
    }
    let r = replicator(&db, &root, remote.clone());
    sync_while_writing(&mut g, &mut w, &r, false);
    r.sync(false, false).unwrap();
    (root, db, remote, g, w, r)
}

#[test]
fn a_follower_applies_the_shipped_log_and_equals_the_primary() {
    let (root, _db, remote, mut g, mut w, r) = setup("basic");
    let mut f = Follower::new(remote.clone(), &root.join("replica"), opts());
    let mut rep = f.open().unwrap();
    catch_up(&mut f, &mut rep);
    same(&g, &rep, "after the first restore");
    assert_eq!(rep.log_position(), g.log_position(), "same position in the same log");

    // More writes, shipped in several passes, applied in several steps.
    for round in 0..4 {
        for _ in 0..150 {
            w.step(&mut g);
        }
        r.sync(false, false).unwrap();
        catch_up(&mut f, &mut rep);
        same(&g, &rep, &format!("after round {round}"));
        assert_eq!(rep.log_position(), g.log_position());
    }

    // Writes are refused, and say why.
    let err = query::execute(&mut rep, "CREATE (:P {i: -1})").unwrap_err().to_string();
    assert!(err.contains("read-only"), "{err}");
    assert!(rep.add_node(&["P".into()], vec![]).is_err());
    // Reads work.
    let n = query::execute(&mut rep, "MATCH (p:P) RETURN count(p)").unwrap();
    assert_eq!(n.rows, query::execute(&mut g, "MATCH (p:P) RETURN count(p)").unwrap().rows);
}

#[test]
fn a_follower_resumes_from_its_own_log_after_reopening() {
    let (root, _db, remote, mut g, mut w, r) = setup("reopen");
    let dir = root.join("replica");
    let mut f = Follower::new(remote.clone(), &dir, opts());
    let mut rep = f.open().unwrap();
    catch_up(&mut f, &mut rep);
    let pos = rep.log_position();
    drop(rep);

    for _ in 0..300 {
        w.step(&mut g);
    }
    r.sync(false, false).unwrap();

    // The same copy, not a new restore.
    let mut f = Follower::new(remote.clone(), &dir, opts());
    let mut rep = f.open().unwrap();
    assert_eq!(rep.log_position(), pos, "reopened at its position");
    let copies = std::fs::read_dir(&dir).unwrap().filter(|e| {
        e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".gldb")
    }).count();
    assert_eq!(copies, 1, "no second copy");
    catch_up(&mut f, &mut rep);
    same(&g, &rep, "after reopening and catching up");
}

#[test]
fn a_follower_restores_when_the_log_it_needs_is_gone() {
    let (root, _db, remote, mut g, mut w, r) = setup("gap");
    let mut f = Follower::new(remote.clone(), &root.join("replica"), opts());
    let mut rep = f.open().unwrap();
    catch_up(&mut f, &mut rep);
    let behind = rep.log_position();

    // The primary moves on, a new base is taken, and the log below it is
    // deleted before the follower read it (what retention does).
    for _ in 0..300 {
        w.step(&mut g);
    }
    sync_while_writing(&mut g, &mut w, &r, true);
    r.sync(false, false).unwrap();
    let gen = g.generation_hex();
    let newest = paged::listing(remote.as_ref(), &gen).unwrap().bases.last().unwrap().lsn;
    assert!(newest > behind);
    for e in remote.list(&format!("{gen}/segments/")).unwrap() {
        let s = paged::listing(remote.as_ref(), &gen).unwrap();
        if let Some(seg) = s.segments.iter().find(|x| x.key == e.key) {
            if seg.seg.end() <= newest {
                remote.delete(&e.key).unwrap();
            }
        }
    }

    match f.plan(&rep.generation_hex(), rep.log_position()).unwrap() {
        Plan::Restore { reason, .. } => assert!(reason.contains("gone"), "{reason}"),
        other => panic!("expected a restore, got {other:?}"),
    }
    catch_up(&mut f, &mut rep);
    same(&g, &rep, "after restoring past the gap");
}

#[test]
fn a_follower_moves_to_a_new_generation_of_the_primary() {
    let (root, _db, remote, mut g, mut w, r) = setup("generation");
    let mut f = Follower::new(remote.clone(), &root.join("replica"), opts());
    f.generation_check = Duration::ZERO;
    let mut rep = f.open().unwrap();
    catch_up(&mut f, &mut rep);
    same(&g, &rep, "before");

    // A new lineage replicating to the same place: a primary rebuilt from
    // scratch (a migrate, a restore, a torn-tail recovery all mint one).
    drop(r);
    let db2 = root.join("primary2.gldb");
    let mut g2 = open(&db2);
    let mut w2 = Writer { n: 1000 };
    for _ in 0..120 {
        w2.step(&mut g2);
    }
    let r2 = replicator(&db2, &root.join("second"), remote.clone());
    sync_while_writing(&mut g2, &mut w2, &r2, false);
    r2.sync(false, false).unwrap();
    assert_ne!(g2.generation_hex(), g.generation_hex());
    let _ = &mut w;

    catch_up(&mut f, &mut rep);
    assert_eq!(rep.generation_hex(), g2.generation_hex(), "followed the new generation");
    same(&g2, &rep, "after moving to the new generation");
}
