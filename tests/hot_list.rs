//! The hot set remembered (docs/HOT.md): written beside the database at
//! each checkpoint and on close, loaded at the next open before the first
//! query, carried in a replica's base and laid beside a restored database.
//! The list is advice: one that is missing, damaged, another database's or
//! from a checkpoint the file never reached opens the database exactly as
//! no list does, and a page in it that is not the data it named is never
//! loaded.
//!
//! The process budget's side is in `hot_budget.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use glider::graph::{Dir, Graph, OpenOptions};
use glider::query;
use glider::storage::hotlist::{self, List};
use glider::storage::pager;
use glider::store::Sync;
use glider::stream::backend::{Backend, FileBackend};
use glider::stream::config::ReplicaConfig;
use glider::stream::paged::{self, PagedReplicator};
use glider::value::Value;
use glider::wal::TailOptions;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glider-hotl-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("db")
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

const PEOPLE: u64 = 8;
const ITEMS: u64 = 6000;
const HOT: u64 = 32 << 10;

/// 1 KiB pages and a 64-frame cache; `hot` bytes of it may be hot.
fn opts(hot: u64) -> OpenOptions {
    OpenOptions {
        sync: Sync::Normal,
        page_size: 1024,
        cache_size: 64 << 10,
        hot_size: hot,
        ..OpenOptions::default()
    }
}

fn open(path: &Path, hot: u64) -> Graph {
    Graph::open_opts(path, opts(hot)).unwrap()
}

/// Eight people who know three others each (ids 1 to 8), then `items`
/// items: a label many times the cache.
fn build(g: &mut Graph, items: u64) {
    g.autocommit = false;
    for i in 0..PEOPLE {
        let name = Value::Text(format!("p{i}"));
        g.add_node(&["Person".into()], vec![("name".into(), name)])
            .unwrap();
    }
    for i in 0..PEOPLE {
        for d in 1..=3 {
            g.add_edge(1 + i, 1 + (i + d) % PEOPLE, "KNOWS", vec![])
                .unwrap();
        }
    }
    for i in 0..items {
        let props = vec![
            ("i".into(), Value::Int(i as i64)),
            ("pad".into(), Value::Text("x".repeat(40))),
        ];
        g.add_node(&["Item".into()], props).unwrap();
        if i % 4000 == 3999 {
            g.commit().unwrap();
        }
    }
    g.commit().unwrap();
    g.autocommit = true;
    g.checkpoint().unwrap();
}

/// What a service asks all day: each person, everyone they know, and the
/// same through a query.
fn look_up_people(g: &mut Graph) {
    for p in 1..=PEOPLE {
        assert!(g.node(p).is_some());
        for a in g.neighbors(p, Dir::Both, None) {
            assert!(g.node(a.other).is_some());
        }
        let q = format!("MATCH (p)-[:KNOWS]->(q) WHERE id(p) = {p} RETURN q.name");
        assert_eq!(query::execute(g, &q).unwrap().rows.len(), 3);
    }
}

/// Warm the hot set the way a service would, then scan the items so the
/// cache holds little else of the people.
fn warm(g: &mut Graph) {
    for _ in 0..40 {
        look_up_people(g);
    }
    let q = "MATCH (n:Item) WHERE n.i >= 0 RETURN count(n)";
    assert_eq!(
        query::execute(g, q).unwrap().rows[0][0],
        Value::Int(ITEMS as i64)
    );
}

/// Pages read from disk by `f`.
fn reads(g: &mut Graph, f: impl FnOnce(&mut Graph)) -> u64 {
    let before = g.pager_stats().reads;
    f(g);
    g.pager_stats().reads - before
}

/// The epoch in each page's header, as the file has it (1 KiB pages, one
/// segment).
fn epochs(path: &Path, pnos: &[u64]) -> Vec<u64> {
    let file = std::fs::read(path).unwrap();
    pnos.iter()
        .map(|&p| {
            let at = p as usize * 1024 + 16;
            u64::from_le_bytes(file[at..at + 8].try_into().unwrap())
        })
        .collect()
}

fn list_of(path: &Path) -> Option<List> {
    hotlist::read(&hotlist::path(path), u64::MAX)
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

/// What an open did before the first query: pages read, pages resident,
/// pages hot, pages loaded from a list.
fn at_open(path: &Path, hot: u64) -> (Graph, [u64; 4]) {
    let g = open(path, hot);
    let s = g.pager_stats();
    let seen = [s.reads, s.resident_pages, s.hot_pages, s.prewarmed];
    (g, seen)
}

// ----------------------------------------------------------------- remembered

/// Property 1: a checkpoint and a close write the hot page numbers beside
/// the database, as of that checkpoint; the next open loads them before it
/// returns, and the first lookups of what was hot read nothing, where the
/// same open without the list reads them from disk.
#[test]
fn a_reopened_database_is_warm_on_what_was_hot() {
    let path = temp("warm");
    let mut g = open(&path, HOT);
    build(&mut g, ITEMS);
    warm(&mut g);

    // At a checkpoint: the hot set as it is, under that checkpoint's epoch.
    g.checkpoint().unwrap();
    let l = list_of(&path).expect("a checkpoint writes the list");
    let sb = pager::read_superblock(&path).unwrap();
    assert_eq!(
        (l.epoch, l.generation, l.page_size),
        (sb.epoch, sb.generation, 1024)
    );
    assert_eq!(l.pages, g.hot_pages());
    assert!(!l.pages.is_empty());

    // On close, after more use: the hot set as it is then. Commits with
    // nothing in them (the shell commits after every statement) move the
    // epoch on and leave nothing to checkpoint.
    warm(&mut g);
    for _ in 0..3 {
        g.commit().unwrap();
    }
    let hot = g.hot_pages();
    drop(g);
    let l = list_of(&path).expect("a close writes the list");
    assert_eq!(l.pages, hot, "hottest first, as the set had them");
    assert_eq!(l.epoch, pager::read_superblock(&path).unwrap().epoch);
    let len = std::fs::metadata(hotlist::path(&path)).unwrap().len();
    assert_eq!(len, 52 + 8 * hot.len() as u64, "proportional to the set");

    // Warm: loaded at open, and the first lookups read nothing.
    let (mut g, [_, _, hot_now, prewarmed]) = at_open(&path, HOT);
    assert_eq!(prewarmed, hot.len() as u64, "every page of the list");
    assert_eq!(hot_now, prewarmed);
    assert_eq!(sorted(g.hot_pages()), sorted(hot.clone()));
    g.check_hot_set().unwrap();
    let warm_reads = reads(&mut g, look_up_people);
    assert_eq!(warm_reads, 0, "the first lookups are served from memory");
    let st = g.stats();
    assert_eq!(st.prewarmed, prewarmed);
    drop(g);

    // Cold: the same open with no list.
    std::fs::remove_file(hotlist::path(&path)).unwrap();
    let (mut g, [.., prewarmed]) = at_open(&path, HOT);
    assert_eq!(prewarmed, 0);
    let cold_reads = reads(&mut g, look_up_people);
    assert!(cold_reads > 0, "without the list they come from disk");
    eprintln!("first lookups after a reopen: cold {cold_reads} reads, warm {warm_reads}");
    drop(g);
    cleanup(&path);
}

/// Property 2: a list that is missing, empty, truncated, damaged, another
/// database's, an earlier database's at the same path, or from a
/// checkpoint the file never reached, opens the database exactly as it
/// opens with no list: the same pages read, the same pages resident,
/// nothing hot, nothing loaded, and the same answers.
#[test]
fn a_list_that_is_not_this_databases_opens_as_no_list_does() {
    let path = temp("advice");
    let mut g = open(&path, HOT);
    build(&mut g, ITEMS);
    warm(&mut g);
    // Keep the file as it is at this checkpoint (an older copy, below).
    g.checkpoint().unwrap();
    let older = std::fs::read(&path).unwrap();
    warm(&mut g);
    g.set_node_prop(1, "seen", Value::Int(1)).unwrap();
    drop(g);
    let good = std::fs::read(hotlist::path(&path)).unwrap();
    let list = List::decode(&good).unwrap();
    assert!(!list.pages.is_empty());

    // Another database, with a list of its own.
    let other = temp("advice-other");
    let mut o = open(&other, HOT);
    build(&mut o, 500);
    warm_people_only(&mut o);
    drop(o);
    let foreign = std::fs::read(hotlist::path(&other)).unwrap();

    let expected = {
        let g = open(&path, 0);
        let e = query::export_jsonl(&g);
        drop(g);
        e
    };
    // With no list, from this state of the file.
    let baseline = |path: &Path| {
        let _ = std::fs::remove_file(hotlist::path(path));
        let (g, seen) = at_open(path, HOT);
        drop(g);
        seen
    };
    let check = |name: &str, path: &Path, list: Option<&[u8]>, want: [u64; 4]| {
        let file = hotlist::path(path);
        let _ = std::fs::remove_file(&file);
        if let Some(b) = list {
            std::fs::write(&file, b).unwrap();
        }
        let (mut g, seen) = at_open(path, HOT);
        assert_eq!(seen, want, "{name}: reads, resident, hot, prewarmed");
        g.check_hot_set().unwrap();
        g.verify_trees().unwrap();
        look_up_people(&mut g);
        drop(g);
    };

    let want = baseline(&path);
    assert_eq!((want[2], want[3]), (0, 0));
    let mut truncated = good.clone();
    truncated.truncate(good.len() / 2);
    let mut damaged = good.clone();
    let mid = 48 + 8 * (list.pages.len() / 2) + 3;
    damaged[mid] ^= 0x40;
    let mut longer = good.clone();
    longer.extend_from_slice(&7u64.to_le_bytes());
    let cases: Vec<(&str, Option<Vec<u8>>)> = vec![
        ("missing", None),
        ("empty", Some(Vec::new())),
        ("truncated", Some(truncated)),
        ("damaged", Some(damaged)),
        ("longer than it says", Some(longer)),
        ("another database's", Some(foreign)),
        (
            "of another page size",
            Some(
                List {
                    page_size: 2048,
                    ..list.clone()
                }
                .encode(),
            ),
        ),
    ];
    for (name, bytes) in &cases {
        check(name, &path, bytes.as_deref(), want);
    }
    let q = |path: &Path| query::export_jsonl(&open(path, 0));
    assert_eq!(q(&path), expected, "the same answers");

    // A checkpoint the file never reached: the database is put back to an
    // older copy of itself, and the list beside it is newer.
    let back = temp("advice-back");
    std::fs::write(&back, &older).unwrap();
    let want_back = baseline(&back);
    let newer = List {
        generation: pager::read_superblock(&back).unwrap().generation,
        ..list.clone()
    };
    assert!(newer.epoch > pager::read_superblock(&back).unwrap().epoch);
    check("from the future", &back, Some(&newer.encode()), want_back);

    // An earlier database at the same path: deleted and built again, a new
    // generation, with the old list still beside it.
    let again = temp("advice-again");
    let mut a = open(&again, HOT);
    build(&mut a, ITEMS);
    drop(a);
    let first_list = std::fs::read(hotlist::path(&again)).unwrap();
    for f in [
        again.clone(),
        PathBuf::from(format!("{}-wal", again.display())),
    ] {
        let _ = std::fs::remove_file(&f);
        let _ = std::fs::remove_dir_all(&f);
    }
    let mut a = open(&again, 0);
    build(&mut a, ITEMS);
    drop(a);
    let want_again = baseline(&again);
    check(
        "an earlier database's",
        &again,
        Some(&first_list),
        want_again,
    );

    for p in [&path, &other, &back, &again] {
        cleanup(p);
    }
}

fn warm_people_only(g: &mut Graph) {
    for _ in 0..40 {
        look_up_people(g);
    }
}

/// Property 2, page by page: a page in the list that is free now, past the
/// end of the file, or not a page at all is skipped without being read; one
/// whose number holds other data now (its epoch is after the list's) is
/// read, found out, and not kept. Only the rest are loaded.
#[test]
fn a_page_that_is_not_what_the_list_named_is_never_loaded() {
    let path = temp("pages");
    let mut g = open(&path, HOT);
    build(&mut g, ITEMS);
    warm(&mut g);
    g.checkpoint().unwrap();
    let before = g.hot_pages();
    // Rewrite person 1: the leaf holding it, and the path above, get new
    // numbers; the old ones are free after the next checkpoint.
    g.set_node_prop(1, "seen", Value::Int(1)).unwrap();
    let after = g.hot_pages();
    let moved: Vec<u64> = before
        .iter()
        .copied()
        .filter(|p| !after.contains(p))
        .collect();
    assert!(!moved.is_empty(), "the path to person 1 has new numbers");
    g.checkpoint().unwrap();
    drop(g);
    let sb = pager::read_superblock(&path).unwrap();
    let good = list_of(&path).unwrap();
    let live: Vec<u64> = good.pages.iter().take(5).copied().collect();
    assert!(live.iter().all(|p| !moved.contains(p)));

    // Free now, past the end, superblocks, and live pages.
    let mut pages = vec![sb.high_water + 3, u64::MAX, 0, 1];
    pages.extend(&moved);
    pages.extend(&live);
    let crafted = List {
        pages,
        ..good.clone()
    };
    // The same open as from a list of the live pages alone: nothing else
    // was read, nothing else is resident.
    let only_live = List {
        pages: live.clone(),
        ..good.clone()
    };
    hotlist::write(&hotlist::path(&path), &only_live).unwrap();
    let (g, want) = at_open(&path, HOT);
    drop(g);
    assert_eq!(want[3], live.len() as u64);
    hotlist::write(&hotlist::path(&path), &crafted).unwrap();
    let (g, seen) = at_open(&path, HOT);
    assert_eq!(seen, want, "reads, resident, hot, prewarmed");
    assert_eq!(sorted(g.hot_pages()), sorted(live.clone()));
    g.check_hot_set().unwrap();
    drop(g);

    // A list from an older checkpoint: the database was opened since
    // without a hot set (which writes no list) and changed. Pages whose
    // numbers hold other data now are read and not kept.
    let mut g = open(&path, HOT);
    warm(&mut g);
    drop(g);
    let old = list_of(&path).unwrap();
    let mut g = open(&path, 0);
    g.autocommit = false;
    for p in 1..=PEOPLE {
        g.set_node_prop(p, "moved", Value::Int(2)).unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    // Room for more: items deleted and new ones written, so freed numbers
    // are handed out again.
    for id in PEOPLE + 1..PEOPLE + 400 {
        g.delete_node(id).unwrap();
    }
    g.checkpoint().unwrap();
    for i in 0..400 {
        g.add_node(&["Item".into()], vec![("i".into(), Value::Int(i))])
            .unwrap();
    }
    drop(g);
    assert_eq!(
        list_of(&path),
        Some(old.clone()),
        "hot size 0 wrote nothing"
    );
    assert!(old.epoch < pager::read_superblock(&path).unwrap().epoch);
    let (mut g, [.., prewarmed]) = at_open(&path, HOT);
    let loaded = g.hot_pages();
    assert!(prewarmed > 0, "what is left of it is used");
    assert!(
        (prewarmed as usize) < old.pages.len(),
        "{prewarmed} of {} pages loaded: the rewritten ones are not",
        old.pages.len()
    );
    for (pno, epoch) in loaded.iter().zip(epochs(&path, &loaded)) {
        assert!(
            epoch <= old.epoch,
            "page {pno} of epoch {epoch} loaded from a list of epoch {}",
            old.epoch
        );
    }
    g.check_hot_set().unwrap();
    g.verify_trees().unwrap();
    look_up_people(&mut g);
    drop(g);
    cleanup(&path);
}

/// After a crash the list is the last checkpoint's, and the log replays
/// writes made after it: the pages those writes replaced are free once the
/// replay is folded in, and are not loaded; the rest are.
#[test]
fn after_a_crash_what_the_log_rewrote_is_skipped() {
    let path = temp("crash");
    let mut g = open(&path, HOT);
    build(&mut g, ITEMS);
    warm(&mut g);
    g.checkpoint().unwrap();
    let listed = list_of(&path).unwrap();
    g.autocommit = false;
    for p in 1..=PEOPLE {
        g.set_node_prop(p, "after", Value::Int(1)).unwrap();
    }
    g.commit().unwrap();
    let expected = query::export_jsonl(&g);
    std::mem::forget(g);
    let mut g = Graph::open_opts(
        &path,
        OpenOptions {
            force: true,
            ..opts(HOT)
        },
    )
    .unwrap();
    let s = g.pager_stats();
    assert!(s.prewarmed > 0);
    assert!(
        (s.prewarmed as usize) < listed.pages.len(),
        "the rewritten pages are free, and skipped"
    );
    for (pno, epoch) in g.hot_pages().iter().zip(epochs(&path, &g.hot_pages())) {
        assert!(epoch <= listed.epoch, "page {pno}");
    }
    assert_eq!(query::export_jsonl(&g), expected, "the log was replayed");
    g.check_hot_set().unwrap();
    g.verify_trees().unwrap();
    look_up_people(&mut g);
    drop(g);
    cleanup(&path);
}

// ---------------------------------------------------------------------- costs

/// Property 5 and 10: with hot size 0 nothing is written, and a list
/// beside the database is not read; with a smaller hot set than the list
/// was taken from, the hottest of it are loaded, as many as fit, and no
/// more pages are read.
#[test]
fn costs_are_bounded_by_the_hot_set_and_zero_is_none() {
    let path = temp("costs");
    let mut g = open(&path, 0);
    build(&mut g, ITEMS);
    warm(&mut g);
    g.checkpoint().unwrap();
    drop(g);
    assert!(!hotlist::path(&path).exists(), "hot size 0 writes no list");

    let mut g = open(&path, HOT);
    warm(&mut g);
    drop(g);
    let list = list_of(&path).unwrap();
    assert!(list.pages.len() > 4, "{:?}", list.pages);
    let bytes = std::fs::read(hotlist::path(&path)).unwrap();

    // Four places: the four hottest, and no more pages read.
    let _ = std::fs::remove_file(hotlist::path(&path));
    let (g, [base_reads, ..]) = at_open(&path, 4 << 10);
    drop(g);
    std::fs::write(hotlist::path(&path), &bytes).unwrap();
    let (g, [r, _, hot, prewarmed]) = at_open(&path, 4 << 10);
    assert_eq!((hot, prewarmed), (4, 4));
    assert!(r - base_reads <= 4, "at most the set's room is read");
    assert_eq!(
        sorted(g.hot_pages()),
        sorted(list.pages[..4].to_vec()),
        "the hottest"
    );
    drop(g);

    // Hot size 0 with a list beside it: the same open as without, and the
    // list is left as it was, even through writes and checkpoints.
    std::fs::write(hotlist::path(&path), &bytes).unwrap();
    let (g, with) = at_open(&path, 0);
    drop(g);
    let saved = std::fs::read(hotlist::path(&path)).unwrap();
    std::fs::remove_file(hotlist::path(&path)).unwrap();
    let (g, without) = at_open(&path, 0);
    drop(g);
    assert_eq!(with, without, "a list is not read without a hot set");
    std::fs::write(hotlist::path(&path), &saved).unwrap();
    let mut g = open(&path, 0);
    g.set_node_prop(2, "x", Value::Int(1)).unwrap();
    g.checkpoint().unwrap();
    drop(g);
    assert_eq!(std::fs::read(hotlist::path(&path)).unwrap(), bytes);
    cleanup(&path);
}

// -------------------------------------------------------------------- carried

fn once() -> TailOptions {
    TailOptions {
        once: true,
        quiet: true,
        ..TailOptions::default()
    }
}

fn gen_of(db: &Path) -> String {
    glider::replica::generation_hex(db).unwrap()
}

/// Open a restored database the way `glider wal restore` does (no hot
/// set: replay, check, close), then as a service would, with one; returns
/// what the second open loaded and what its first lookups read.
fn first_open_after_restore(out: &Path, expected: &str) -> (u64, u64) {
    let r = open(out, 0);
    r.verify_trees().unwrap();
    assert_eq!(query::export_jsonl(&r), expected, "restored as the source");
    drop(r);
    let mut r = open(out, HOT);
    let prewarmed = r.pager_stats().prewarmed;
    let lookups = reads(&mut r, look_up_people);
    r.check_hot_set().unwrap();
    drop(r);
    (prewarmed, lookups)
}

/// Property 3: a base carries the list of the checkpoint it copies, and a
/// restore lays it beside the database, so the first open with a hot set
/// is warm. A base without one restores as before, cold.
#[test]
fn a_base_carries_the_list_and_a_restored_database_opens_warm() {
    let path = temp("base");
    let dir = path.parent().unwrap().join("replica");
    let mut g = open(&path, HOT);
    build(&mut g, ITEMS);
    warm(&mut g);
    let expected = query::export_jsonl(&g);
    drop(g);
    let hot = list_of(&path).unwrap().pages;

    // No writer running: the tailer copies under the database's lock.
    glider::replica::tail(&path, &dir, &once()).unwrap();
    let gen = gen_of(&path);
    let base = glider::replica::bases(&dir, &gen).unwrap().pop().unwrap();
    let base_dir = dir
        .join(&gen)
        .join("base")
        .join(format!("{:016x}", base.lsn));
    let carried = List::decode(&std::fs::read(base_dir.join("hot")).unwrap()).unwrap();
    assert_eq!(carried.pages, hot);
    assert_eq!(
        carried.epoch, base.epoch,
        "the list of the checkpoint copied"
    );

    let out = path.parent().unwrap().join("restored");
    glider::replica::restore(&dir, None, None, &out).unwrap();
    assert!(hotlist::path(&out).exists(), "laid beside the database");
    let (prewarmed, warm_reads) = first_open_after_restore(&out, &expected);
    assert_eq!(prewarmed, hot.len() as u64);
    assert_eq!(warm_reads, 0, "warm on what the source found hot");

    // The same base without its list: restored as before, and cold.
    std::fs::remove_file(base_dir.join("hot")).unwrap();
    let out2 = path.parent().unwrap().join("restored-cold");
    glider::replica::restore(&dir, None, None, &out2).unwrap();
    assert!(!hotlist::path(&out2).exists());
    let (prewarmed, cold_reads) = first_open_after_restore(&out2, &expected);
    assert_eq!(prewarmed, 0);
    assert!(cold_reads > 0);
    eprintln!("first lookups after a restore: cold {cold_reads} reads, warm {warm_reads}");
    cleanup(&path);
}

/// Property 3 with a writer running: the base is taken under a hold the
/// writer checkpoints for, and carries a list of this database no newer
/// than that checkpoint.
#[test]
fn a_base_taken_from_a_live_writer_carries_a_list() {
    let path = temp("live");
    let dir = path.parent().unwrap().join("replica");
    let mut g = open(&path, HOT);
    build(&mut g, ITEMS);
    warm(&mut g);
    let (p2, d2) = (path.clone(), dir.clone());
    let t = std::thread::spawn(move || glider::replica::tail(&p2, &d2, &once()));
    let start = Instant::now();
    while !t.is_finished() {
        look_up_people(&mut g);
        g.poll_replication().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(start.elapsed() < Duration::from_secs(60), "no base");
    }
    t.join().unwrap().unwrap();
    let gen = gen_of(&path);
    let base = glider::replica::bases(&dir, &gen).unwrap().pop().unwrap();
    let base_dir = dir
        .join(&gen)
        .join("base")
        .join(format!("{:016x}", base.lsn));
    let carried = List::decode(&std::fs::read(base_dir.join("hot")).unwrap()).unwrap();
    assert!(carried.epoch <= base.epoch && !carried.pages.is_empty());
    let expected = query::export_jsonl(&g);
    drop(g);
    let out = path.parent().unwrap().join("restored");
    glider::replica::restore(&dir, None, None, &out).unwrap();
    let (prewarmed, lookups) = first_open_after_restore(&out, &expected);
    assert!(prewarmed > 0);
    assert_eq!(lookups, 0);
    cleanup(&path);
}

/// Property 3 through glider-stream: the list goes up with the base (before
/// its `base.json`), comes down with it, and the restored database's first
/// open with a hot set is warm.
#[test]
fn glider_stream_uploads_the_list_and_restores_warm() {
    let path = temp("stream");
    let root = path.parent().unwrap().to_path_buf();
    let mut g = open(&path, HOT);
    build(&mut g, ITEMS);
    warm(&mut g);
    let expected = query::export_jsonl(&g);
    drop(g);
    let hot = list_of(&path).unwrap().pages;
    let remote = Arc::new(FileBackend {
        root: root.join("remote"),
    });
    let cfg = ReplicaConfig {
        url: "test".into(),
        min_bytes: 0,
        sync_interval: Duration::from_millis(200),
        snapshot_interval: Duration::from_secs(0),
        ..ReplicaConfig::default()
    };
    let r = PagedReplicator::new(&path, root.join("spool"), remote.clone(), cfg);
    let report = r.sync(true, false).unwrap();
    assert_eq!(report.bases_uploaded, 1);
    let gen = gen_of(&path);
    let listing = paged::listing(remote.as_ref(), &gen).unwrap();
    let base = listing.bases.last().unwrap();
    let key = format!("{gen}/base/{:016x}/hot", base.lsn);
    let carried = List::decode(&remote.get(&key).unwrap()).unwrap();
    assert_eq!(carried.pages, hot);
    let spooled = root
        .join("spool")
        .join(&gen)
        .join("base")
        .join(format!("{:016x}", base.lsn));
    assert!(
        !spooled.join("hot").exists(),
        "an uploaded base keeps only base.json"
    );

    let out = root.join("restored");
    paged::restore(remote.as_ref(), None, None, &out).unwrap();
    assert!(hotlist::path(&out).exists());
    let (prewarmed, lookups) = first_open_after_restore(&out, &expected);
    assert_eq!(prewarmed, hot.len() as u64);
    assert_eq!(lookups, 0);
    cleanup(&path);
}
