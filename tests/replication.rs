//! Replication of a paged database: base snapshots taken under a hold while
//! the writer keeps writing and checkpointing, log shipping, a new base when
//! the log needed is gone, and restores that reproduce the source exactly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glider::graph::{Graph, OpenOptions};
use glider::query;
use glider::storage::db::{read_pin, remove_pin, write_pin, Pin};
use glider::storage::pager::read_superblock;
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
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "tailer never finished"
        );
    }
    t.join().unwrap().unwrap();
}

fn restore_and_compare(g: &Graph, dir: &Path, out: &Path) {
    let _ = std::fs::remove_file(out);
    glider::replica::restore(dir, None, None, out).unwrap();
    let r = Graph::open(out, glider::store::Sync::Normal).unwrap();
    r.verify_trees().unwrap();
    assert_eq!(
        query::export_jsonl(&r),
        query::export_jsonl(g),
        "restored graph equals the source"
    );
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
    assert_eq!(
        glider::replica::bases(&dir, &gen(&db)).unwrap().len(),
        1,
        "no new base needed"
    );
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
    let sb = read_superblock(db).unwrap();
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
    assert_eq!(
        glider::replica::bases(&dir, &gen(&db)).unwrap().len(),
        2,
        "a second base"
    );
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

// ------------------------------------------------- a base from an idle writer
//
// A writer that has the database open and does nothing but poll for the
// replicator, as `Glider.Replication.sync` does every 100 ms, gives a base
// whatever it did before it went idle. Every wait has a deadline: a base that
// never comes fails the test instead of stalling the suite.

/// A polled writer acts on the pin at the poll, so a base takes about the
/// tailer's own tenth of a second; past this it is never coming.
const IDLE_DEADLINE: Duration = Duration::from_secs(30);

/// Checkpoints only where a test asks for one, so a test says exactly what
/// the writer did before it went idle.
fn open_idle(path: &Path) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            page_size: 1024,
            cache_size: 64 << 10,
            checkpoint_bytes: u64::MAX,
            wal_segment: 4 << 10,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

/// Take a base into `dir` (a new one even if `dir` has one, if `force`, as
/// glider-stream's forced snapshot asks) while the writer only polls for
/// the replicator, every 100 ms.
fn base_from_idle_writer(g: &mut Graph, db: &Path, dir: &Path, force: bool) {
    let (db2, dir2) = (db.to_path_buf(), dir.to_path_buf());
    let tailer = std::thread::spawn(move || {
        let first = std::cell::Cell::new(force);
        glider::replica::tail_with(&db2, &dir2, &once(), &|_| first.replace(false))
    });
    let start = Instant::now();
    while !tailer.is_finished() {
        assert!(
            start.elapsed() < IDLE_DEADLINE,
            "no base of {} into {} from an idle writer polled for {IDLE_DEADLINE:?}",
            db.display(),
            dir.display()
        );
        g.poll_replication().unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    tailer.join().unwrap().unwrap();
}

/// Let the writer act on the pin as it stands: a poll looks at once.
fn writer_looks_at_the_pin(g: &mut Graph) {
    g.poll_replication().unwrap();
}

#[test]
fn an_idle_writer_gives_a_base_before_its_first_checkpoint() {
    let root = temp("idle-new");
    let db = root.join("db");
    let mut g = open_idle(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..200 {
        w.step(&mut g);
    }
    base_from_idle_writer(&mut g, &db, &root.join("replica"), false);
    restore_and_compare(&g, &root.join("replica"), &root.join("restored"));
    drop(g);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_idle_writer_gives_a_base_after_explicit_checkpoints() {
    // Checkpoints alternate between the two superblock slots: one, two and
    // three of them leave either slot in force.
    for checkpoints in 1..=3 {
        let root = temp(&format!("idle-checkpointed-{checkpoints}"));
        let db = root.join("db");
        let mut g = open_idle(&db);
        let mut w = Writer { n: 0 };
        for _ in 0..200 {
            w.step(&mut g);
        }
        for _ in 0..checkpoints {
            g.checkpoint().unwrap();
        }
        base_from_idle_writer(&mut g, &db, &root.join("replica"), false);
        restore_and_compare(&g, &root.join("replica"), &root.join("restored"));
        drop(g);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn an_idle_writer_gives_a_base_after_a_clean_reopen() {
    // Closed cleanly (a checkpoint) once or twice, then reopened and idle.
    for sessions in 1..=2 {
        let root = temp(&format!("idle-reopened-{sessions}"));
        let db = root.join("db");
        let mut w = Writer { n: 0 };
        for _ in 0..sessions {
            let mut g = open_idle(&db);
            for _ in 0..100 {
                w.step(&mut g);
            }
        }
        let mut g = open_idle(&db);
        base_from_idle_writer(&mut g, &db, &root.join("replica"), false);
        restore_and_compare(&g, &root.join("replica"), &root.join("restored"));
        drop(g);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn an_idle_writer_gives_a_base_of_a_database_restored_from_a_replica() {
    // A restored database starts from a base taken under a hold. Opening it
    // releases that hold and, when log was shipped after the base, replays
    // and folds it in: one checkpoint or two, with nothing committed between.
    for replayed in [true, false] {
        let root = temp(&format!("idle-restored-{replayed}"));
        let src = root.join("src");
        let replica = root.join("replica");
        let mut g = open(&src);
        let mut w = Writer { n: 0 };
        for _ in 0..200 {
            w.step(&mut g);
        }
        tail_while_writing(&mut g, &mut w, &src, &replica);
        for _ in 0..100 {
            w.step(&mut g);
        }
        glider::replica::tail(&src, &replica, &once()).unwrap();
        let db = root.join("restored");
        glider::replica::restore(&replica, None, None, &db).unwrap();
        if !replayed {
            // Restored as of the base itself: no log after it.
            let wal = glider::storage::log::wal_dir(&db);
            for e in std::fs::read_dir(wal).unwrap().flatten() {
                std::fs::remove_file(e.path()).unwrap();
            }
        }
        let mut r = open_idle(&db);
        if replayed {
            assert_eq!(query::export_jsonl(&r), query::export_jsonl(&g));
        }
        drop(g);
        base_from_idle_writer(&mut r, &db, &root.join("replica2"), false);
        restore_and_compare(&r, &root.join("replica2"), &root.join("again"));
        drop(r);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn an_idle_writer_gives_base_after_base_with_nothing_committed_between() {
    // A base, its hold released, then another with no commit in between:
    // for a second replica, then a forced one (a snapshot) for the first.
    for checkpoints in 0..=1 {
        let root = temp(&format!("idle-again-{checkpoints}"));
        let db = root.join("db");
        let (first, second) = (root.join("replica"), root.join("replica2"));
        let mut g = open_idle(&db);
        let mut w = Writer { n: 0 };
        for _ in 0..200 {
            w.step(&mut g);
        }
        for _ in 0..checkpoints {
            g.checkpoint().unwrap();
        }
        base_from_idle_writer(&mut g, &db, &first, false);
        restore_and_compare(&g, &first, &root.join("restored"));
        writer_looks_at_the_pin(&mut g);
        base_from_idle_writer(&mut g, &db, &second, false);
        restore_and_compare(&g, &second, &root.join("restored2"));
        writer_looks_at_the_pin(&mut g);
        base_from_idle_writer(&mut g, &db, &first, true);
        restore_and_compare(&g, &first, &root.join("restored3"));
        drop(g);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn an_idle_writer_gives_a_base_after_its_replicator_died_holding() {
    // A replicator asked for a hold and died. Its pin is removed, and the
    // writer lets the hold go at its next look (a pin older than PIN_TTL
    // reads as no pin at all, so a lapsed one is the same to the writer);
    // or a new replicator asks first, over the dead one's pin.
    for checkpoints in 0..=1 {
        for released in [true, false] {
            let root = temp(&format!("idle-orphaned-{checkpoints}-{released}"));
            let db = root.join("db");
            let mut g = open_idle(&db);
            let mut w = Writer { n: 0 };
            for _ in 0..200 {
                w.step(&mut g);
            }
            for _ in 0..checkpoints {
                g.checkpoint().unwrap();
            }
            let dead = Pin {
                nonce: 42,
                hold: true,
                shipped: 0,
            };
            write_pin(&db, dead).unwrap();
            writer_looks_at_the_pin(&mut g);
            let sb = read_superblock(&db).unwrap();
            assert!(sb.hold && sb.pin_seen == 42, "the writer holds for it");
            if released {
                remove_pin(&db);
                writer_looks_at_the_pin(&mut g);
                assert!(!read_superblock(&db).unwrap().hold, "and lets go");
            }
            base_from_idle_writer(&mut g, &db, &root.join("replica"), false);
            restore_and_compare(&g, &root.join("replica"), &root.join("restored"));
            drop(g);
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}

/// Open `db`, and ask the writer, idle but for a poll every `poll`, for a
/// base into `dir`: the first poll after the request must take the hold,
/// the first after the base must let it go, and the base restores to the
/// source. Returns how long after that first poll the tailer began to copy.
fn base_from_a_writer_just_opened(db: &Path, dir: &Path, out: &Path, poll: Duration) -> Duration {
    let mut g = open_idle(db);
    let (db2, dir2) = (db.to_path_buf(), dir.to_path_buf());
    let tailer = std::thread::spawn(move || glider::replica::tail(&db2, &dir2, &once()));
    // Not polled until the replicator has asked, so the next poll is the
    // first after the request.
    let start = Instant::now();
    let nonce = loop {
        if let Some(p) = read_pin(db).filter(|p| p.hold) {
            break p.nonce;
        }
        assert!(start.elapsed() < IDLE_DEADLINE, "the replicator asked");
        std::thread::sleep(Duration::from_millis(1));
    };
    g.poll_replication().unwrap();
    let held = Instant::now();
    let sb = read_superblock(db).unwrap();
    assert!(
        sb.hold && sb.pin_seen == nonce,
        "the first poll takes the hold"
    );
    // The tailer's copy starts by making the base's directory.
    let bases = dir
        .join(glider::replica::generation_hex(db).unwrap())
        .join("base");
    let mut copying = None;
    let mut polled = Instant::now();
    while !tailer.is_finished() {
        assert!(held.elapsed() < IDLE_DEADLINE, "no base");
        let begun = std::fs::read_dir(&bases).map(|mut d| d.next().is_some());
        if copying.is_none() && begun.unwrap_or(false) {
            copying = Some(Instant::now());
        }
        if polled.elapsed() >= poll {
            g.poll_replication().unwrap();
            polled = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let copying = copying.unwrap_or_else(Instant::now);
    tailer.join().unwrap().unwrap();
    g.poll_replication().unwrap();
    assert!(
        !read_superblock(db).unwrap().hold,
        "the next poll lets it go"
    );
    restore_and_compare(&g, dir, out);
    copying - held
}

#[test]
fn an_idle_writer_polled_often_gives_a_base_within_a_few_polls() {
    // An explicit poll acts on the pin then and there; only a commit looks
    // at most once a second. A writer just opened has just looked, so a
    // hold left for the next once-a-second look would come a second late
    // however often the writer is polled.
    let root = temp("idle-polled");
    let db = root.join("db");
    {
        let mut g = open_idle(&db);
        let mut w = Writer { n: 0 };
        for _ in 0..200 {
            w.step(&mut g);
        }
    }
    // From the poll that takes the hold until the tailer, which looks for
    // its hold every 100 ms, begins to copy: twenty polls of 25 ms, five of
    // its looks. The copy itself is the disk's to time, and a busy disk can
    // stall anything for a moment, so the writer gets three tries.
    let within = Duration::from_millis(500);
    let mut took = Vec::new();
    for n in 0..3 {
        let (dir, out) = (
            root.join(format!("replica{n}")),
            root.join(format!("out{n}")),
        );
        took.push(base_from_a_writer_just_opened(
            &db,
            &dir,
            &out,
            Duration::from_millis(25),
        ));
        if took[n] < within {
            break;
        }
    }
    assert!(
        took.iter().any(|t| *t < within),
        "bases under way {took:?} after the first poll"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ------------------------------------------- a crash around idle checkpoints

/// The two superblock pages, as they are on disk now.
fn superblocks(db: &Path) -> [Vec<u8>; 2] {
    let ps = read_superblock(db).unwrap().page_size;
    let b = std::fs::read(db).unwrap();
    [b[..ps].to_vec(), b[ps..2 * ps].to_vec()]
}

/// What a crash now would leave: the database file, its segment files and
/// its log with the replicator's pin, but no lock.
fn crash_copy(db: &Path, to: &Path) {
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(db, to).unwrap();
    let dirs = |db: &Path| {
        let data = glider::storage::pager::segment_path(db, 1);
        [
            glider::storage::log::wal_dir(db),
            data.parent().unwrap().to_path_buf(),
        ]
    };
    for (from, to) in dirs(db).into_iter().zip(dirs(to)) {
        let Ok(rd) = std::fs::read_dir(&from) else {
            continue;
        };
        std::fs::create_dir_all(&to).unwrap();
        for e in rd.flatten() {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

/// Pages below the high water of the checkpoint `db` opens at that are in
/// no tree, not in the free-list chain and not listed free or held: lost
/// to reuse for good. (A page both used and listed free counts twice and
/// drives this negative.)
fn unaccounted_pages(db: &Path) -> i64 {
    use glider::storage::btree::Tree;
    use glider::storage::page;
    use glider::storage::pager::{Pager, ROOTS};
    let p = Pager::open(db, 1 << 20).unwrap();
    let mut used = 0u64;
    for slot in 0..ROOTS {
        let root = p.root(slot);
        if root == 0 {
            continue;
        }
        // Every page of a tree carries the tree's id.
        let id = page::tree(&p.get(root).unwrap());
        let st = Tree { slot, id }.check(&p).unwrap();
        used += st.leaves + st.interiors + st.overflow_pages;
    }
    // The chain the superblock in force points to: its head is at byte 88
    // of the superblock, and each chain page links the next at byte 32.
    let sb = read_superblock(db).unwrap();
    let mut chain = 0u64;
    let mut next = page::get_u64(&sb.bytes, 88);
    while next != 0 {
        let pg = p.get(next).unwrap();
        assert_eq!(page::kind(&pg), page::KIND_FREELIST, "chain page {next}");
        chain += 1;
        next = page::get_u64(&pg, 32);
    }
    (p.high_water() - 2) as i64 - (used + chain + p.free_pages()) as i64
}

/// Crash copies of a database taken after each thing its writer does, each
/// reopened and checked against the writer as it was at that moment.
struct Crashes {
    db: PathBuf,
    dir: PathBuf,
    last: [Vec<u8>; 2],
    newest: u64,
    n: usize,
}

impl Crashes {
    fn new(db: &Path, dir: &Path) -> Crashes {
        Crashes {
            db: db.to_path_buf(),
            dir: dir.to_path_buf(),
            last: superblocks(db),
            newest: read_superblock(db).unwrap().slot,
            n: 0,
        }
    }

    /// A crash now. If the writer has just checkpointed with nothing
    /// committed since its last checkpoint (`idle`), also a crash before
    /// that checkpoint's superblock landed, and one that tore it: both
    /// leave the checkpoint before in force, with the same contents.
    fn now(&mut self, g: &Graph, what: &str, idle: bool) {
        let now = superblocks(&self.db);
        let written: Vec<usize> = (0..2).filter(|&s| now[s] != self.last[s]).collect();
        assert!(written.len() <= 1, "{what}: one checkpoint at a time");
        assert!(!idle || written.len() == 1, "{what}: a checkpoint");
        if let Some(&slot) = written.first() {
            self.newest = slot as u64;
        }
        let want = query::export_jsonl(g);
        self.check(&format!("{what}, then a crash"), None, &want);
        if let Some(&slot) = written.first() {
            if idle {
                let old = self.last[slot].clone();
                self.check(
                    &format!("{what}, a crash before its superblock landed"),
                    Some((slot, old.clone())),
                    &want,
                );
                // Written up to the last byte it changes, and no further.
                let new = &now[slot];
                let cut = (0..new.len()).rev().find(|&i| new[i] != old[i]).unwrap();
                let mut torn = new.clone();
                torn[cut..].copy_from_slice(&old[cut..]);
                self.check(
                    &format!("{what}, a crash that tore its superblock"),
                    Some((slot, torn)),
                    &want,
                );
            }
        }
        self.last = now;
    }

    /// Reopen a crash copy, with `patch` over one superblock slot: the
    /// superblock in force is the newest whole one, every page is
    /// accounted for, every tree checks, and it holds what the source held.
    fn check(&mut self, what: &str, patch: Option<(usize, Vec<u8>)>, want: &str) {
        self.n += 1;
        let copy = self.dir.join(format!("crash-{}", self.n)).join("db");
        crash_copy(&self.db, &copy);
        let mut newest = self.newest;
        if let Some((slot, bytes)) = patch {
            let mut img = std::fs::read(&copy).unwrap();
            img[slot * bytes.len()..(slot + 1) * bytes.len()].copy_from_slice(&bytes);
            std::fs::write(&copy, &img).unwrap();
            newest = 1 - slot as u64;
        }
        assert_eq!(
            read_superblock(&copy).unwrap().slot,
            newest,
            "{what}: the newest superblock is in force"
        );
        assert_eq!(unaccounted_pages(&copy), 0, "{what}: pages leaked");
        let r = open_idle(&copy);
        r.verify_trees().unwrap();
        assert_eq!(
            query::export_jsonl(&r),
            want,
            "{what}: what the source held"
        );
        drop(r);
        let _ = std::fs::remove_dir_all(copy.parent().unwrap());
    }
}

#[test]
fn a_crash_around_idle_checkpoints_reopens_the_newest_and_loses_nothing() {
    let root = temp("idle-crash");
    let db = root.join("db");
    let mut g = open_idle(&db);
    let mut w = Writer { n: 0 };
    for _ in 0..300 {
        w.step(&mut g);
    }
    g.checkpoint().unwrap();
    // Pages freed since the checkpoint wait for the next one.
    for _ in 0..100 {
        w.step(&mut g);
    }
    let mut crash = Crashes::new(&db, &root);
    crash.now(&g, "writes", false);
    g.checkpoint().unwrap();
    crash.now(&g, "a checkpoint", false);
    g.checkpoint().unwrap();
    crash.now(&g, "a checkpoint with nothing committed", true);

    let pin = |nonce, hold| Pin {
        nonce,
        hold,
        shipped: 0,
    };
    write_pin(&db, pin(7, true)).unwrap();
    writer_looks_at_the_pin(&mut g);
    crash.now(&g, "a hold", true);
    write_pin(&db, pin(7, false)).unwrap();
    writer_looks_at_the_pin(&mut g);
    crash.now(&g, "its release", true);
    // Writes reuse what the release freed, through a cache that evicts.
    for _ in 0..50 {
        w.step(&mut g);
    }
    crash.now(&g, "writes after the release", false);

    write_pin(&db, pin(8, true)).unwrap();
    writer_looks_at_the_pin(&mut g);
    crash.now(&g, "a hold after writes", false);
    remove_pin(&db);
    writer_looks_at_the_pin(&mut g);
    crash.now(&g, "its pin removed", true);
    write_pin(&db, pin(9, true)).unwrap();
    writer_looks_at_the_pin(&mut g);
    crash.now(&g, "another hold", true);
    g.checkpoint().unwrap();
    crash.now(&g, "a checkpoint under the hold", true);
    for _ in 0..50 {
        w.step(&mut g);
    }
    crash.now(&g, "writes under the hold", false);
    remove_pin(&db);
    writer_looks_at_the_pin(&mut g);
    crash.now(&g, "its pin removed after writes", false);
    drop(g);
    let _ = std::fs::remove_dir_all(&root);
}
