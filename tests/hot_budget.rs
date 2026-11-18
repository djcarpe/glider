//! The hot set under one process budget (`budget::configure`, as
//! memory_budget.rs uses it): another database's need for memory takes cold
//! frames first, wherever they are, and hot ones only when cold ones are not
//! enough; the bounds hold for several databases at every step; and a hot
//! list loaded at open stays inside the budget. Its own file, because the
//! budget is process-wide and set once.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use glider::budget::{self, MemoryBudget, ProcessBudget};
use glider::graph::{Dir, Graph, OpenOptions};
use glider::query;
use glider::storage::{hotlist, pager};
use glider::store::Sync;
use glider::value::Value;

static SERIAL: Mutex<()> = Mutex::new(());

const MIB: u64 = 1 << 20;
/// What the pool charges a frame of a 1 KiB page: the page and 512 bytes
/// of bookkeeping.
const CHARGE: u64 = 1024 + 512;
/// The process's cache pool: four hundred frames.
const POOL: u64 = 400 * CHARGE;

fn process() -> &'static ProcessBudget {
    let b = budget::configure(MemoryBudget {
        limit: 256 * MIB,
        budget: 200 * MIB,
        cache: POOL,
        work: 32 * MIB,
        beam: 200 * MIB - POOL - 32 * MIB,
    });
    assert_eq!(b.config.cache, POOL, "the budget was configured here first");
    b
}

/// Puts the pool back to its full size, even when a test fails.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        pager::resize_shared_cache(POOL);
    }
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glider-hotb-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("db")
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// 1 KiB pages; a cache of `cache` bytes, `hot` of them for the hot set.
fn open(path: &Path, cache: u64, hot: u64) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            sync: Sync::Off,
            page_size: 1024,
            cache_size: cache,
            hot_size: hot,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

const PEOPLE: u64 = 8;

/// Eight people who know three others each, then `items` items.
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
    }
    g.commit().unwrap();
    g.autocommit = true;
    g.checkpoint().unwrap();
}

/// Each person and everyone they know.
fn look_up_people(g: &mut Graph) {
    for p in 1..=PEOPLE {
        assert!(g.node(p).is_some());
        for a in g.neighbors(p, Dir::Both, None) {
            assert!(g.node(a.other).is_some());
        }
    }
}

fn scan(g: &mut Graph) -> i64 {
    let r = query::execute(g, "MATCH (n:Item) WHERE n.i >= 0 RETURN count(n)").unwrap();
    match r.rows[0][0] {
        Value::Int(n) => n,
        ref v => panic!("count is {v:?}"),
    }
}

fn reads(g: &mut Graph, f: impl FnOnce(&mut Graph)) -> u64 {
    let before = g.pager_stats().reads;
    f(g);
    g.pager_stats().reads - before
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n.max(1)
    }
}

/// Property 5: database A has a warm hot set and sits idle; database B
/// then needs the whole budget. Afterwards A's hot pages are all still
/// resident and its lookups of them read nothing, while its cold pages are
/// gone. Without a hot set, B empties A, as before.
#[test]
fn another_databases_pressure_leaves_hot_pages_and_takes_cold_ones() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let b = process();
    let _restore = Restore;
    let mut lookups = Vec::new();
    for hot in [0u64, 64 << 10] {
        let (pa, pb) = (temp(&format!("a-{hot}")), temp(&format!("b-{hot}")));
        let mut a = open(&pa, 256 << 10, hot);
        build(&mut a, 3000);
        for _ in 0..40 {
            look_up_people(&mut a);
        }
        let warm = a.pager_stats();
        assert!(b.cache.used() <= b.cache.limit());
        // B's cache could hold ten times the whole pool.
        let mut bdb = open(&pb, 4 << 20, 0);
        build(&mut bdb, 12_000);
        for _ in 0..3 {
            assert_eq!(scan(&mut bdb), 12_000);
            assert!(b.cache.used() <= b.cache.limit());
        }
        let s = bdb.pager_stats();
        assert!(
            s.resident_pages * CHARGE > POOL / 2,
            "B took most of the pool: {} frames",
            s.resident_pages
        );
        let after = a.pager_stats();
        if hot > 0 {
            assert!(warm.hot_pages > 0);
            assert_eq!(after.hot_pages, warm.hot_pages, "A's hot pages stayed");
            assert_eq!(
                after.resident_pages, after.hot_pages,
                "A's cold pages went to B"
            );
            assert!(warm.resident_pages > warm.hot_pages);
            a.check_hot_set().unwrap();
        } else {
            assert_eq!(after.resident_pages, 0, "B emptied A");
        }
        lookups.push(reads(&mut a, look_up_people));
        assert!(b.cache.used() <= b.cache.limit());
        drop(a);
        drop(bdb);
        cleanup(&pa);
        cleanup(&pb);
    }
    assert!(lookups[0] > 0, "without a hot set A starts again from disk");
    assert_eq!(lookups[1], 0, "with one, A's lookups read nothing");
}

/// Property 6: several databases with hot sets (one page; half the cache;
/// more than half asked for) against a small pool, through lookups, scans,
/// writes and checkpoints: at every step the pool is within its limit,
/// every cache within its frames, every hot set within half its cache and
/// its hot size. A cut below what the hot sets hold shrinks them; and a
/// database with no frame of its own, in a pool of nothing but hot frames,
/// takes the coolest rather than fail.
#[test]
fn bounds_hold_for_several_databases_with_hot_sets_at_every_step() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let b = process();
    let _restore = Restore;
    // (cache bytes, hot bytes asked for)
    let shapes = [
        (64u64 << 10, 1u64 << 10),
        (128 << 10, 64 << 10),
        (256 << 10, 1 << 20),
    ];
    let mut dbs: Vec<(PathBuf, Graph)> = Vec::new();
    for (n, (cache, hot)) in shapes.iter().enumerate() {
        let path = temp(&format!("bounds-{n}"));
        let mut g = open(&path, *cache, *hot);
        build(&mut g, 1500 + 500 * n as u64);
        dbs.push((path, g));
    }
    let check = |dbs: &[(PathBuf, Graph)], step: &str| {
        assert!(
            b.cache.used() <= b.cache.limit(),
            "{step}: the pool holds {} of {}",
            b.cache.used(),
            b.cache.limit()
        );
        for ((_, g), (cache, hot)) in dbs.iter().zip(shapes) {
            let s = g.pager_stats();
            let frames = cache / 1024;
            assert!(s.resident_pages <= frames, "{step}: {s:?}");
            assert!(s.hot_pages <= s.hot_capacity, "{step}: {s:?}");
            assert!(s.hot_capacity <= frames / 2, "{step}: {s:?}");
            assert!(s.hot_capacity <= hot / 1024, "{step}: {s:?}");
            g.check_hot_set().unwrap_or_else(|e| panic!("{step}: {e}"));
        }
    };
    check(&dbs, "built");
    let caps: Vec<u64> = dbs
        .iter()
        .map(|(_, g)| g.pager_stats().hot_capacity)
        .collect();
    assert_eq!(
        caps,
        vec![1, 64, 128],
        "one page; half; half, not the 1 MiB asked"
    );
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    for step in 0..300 {
        let (_, g) = &mut dbs[r.below(3) as usize];
        match r.below(10) {
            0..=5 => look_up_people(g),
            6 | 7 => {
                scan(g);
            }
            8 => {
                g.autocommit = false;
                for _ in 0..5 {
                    let id = 1 + r.below(1500);
                    g.set_node_prop(id, "w", Value::Int(step)).unwrap();
                }
                g.commit().unwrap();
                g.autocommit = true;
            }
            _ => g.checkpoint().unwrap(),
        }
        check(&dbs, &format!("step {step}"));
    }
    let hot: u64 = dbs.iter().map(|(_, g)| g.pager_stats().hot_pages).sum();
    assert!(hot > 16, "the hot sets are warm: {hot}");

    // A limit below what the hot sets hold.
    let limit = (hot - 6) * CHARGE;
    let used = pager::resize_shared_cache(limit).unwrap();
    assert!(used <= limit, "{used} over {limit}");
    let left: u64 = dbs.iter().map(|(_, g)| g.pager_stats().hot_pages).sum();
    assert!(
        left * CHARGE <= limit && left < hot,
        "{left} of {hot} hot pages"
    );
    check(&dbs, "cut");
    for (_, g) in dbs.iter_mut() {
        look_up_people(g);
        scan(g);
    }
    check(&dbs, "working inside the cut");

    // Nothing but hot frames left, and a database with none of its own.
    pager::resize_shared_cache(CHARGE * 8);
    check(&dbs, "cut to eight frames");
    let pd = temp("bounds-new");
    let mut d = open(&pd, 64 << 10, 0);
    let id = d.add_node(&["Item".into()], vec![]).unwrap();
    assert!(d.node(id).is_some());
    assert!(b.cache.used() <= b.cache.limit());
    drop(d);
    cleanup(&pd);

    pager::resize_shared_cache(POOL);
    for (_, g) in dbs.iter_mut() {
        look_up_people(g);
    }
    check(&dbs, "restored");
    for (path, g) in dbs {
        drop(g);
        cleanup(&path);
    }
}

/// Property 6 of the remembered hot set: loading a list at open under a
/// small process budget never takes the pool past its limit, takes no hot
/// frame of another database, and loads what fits; with room, it loads the
/// whole list.
#[test]
fn loading_a_list_under_a_small_budget_stays_inside_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let b = process();
    let _restore = Restore;
    let (pa, pb) = (temp("list-a"), temp("list-b"));
    let mut a = open(&pa, 256 << 10, 64 << 10);
    build(&mut a, 3000);
    for _ in 0..40 {
        look_up_people(&mut a);
    }
    drop(a);
    let listed = hotlist::read(&hotlist::path(&pa), u64::MAX)
        .unwrap()
        .pages
        .len() as u64;
    assert!(listed > 8, "{listed} pages listed");
    let a = open(&pa, 256 << 10, 64 << 10);
    assert_eq!(a.pager_stats().prewarmed, listed, "with room, all of it");
    drop(a);

    // B: a warm hot set, and its scans fill the pool.
    let mut bdb = open(&pb, 4 << 20, 64 << 10);
    build(&mut bdb, 12_000);
    for _ in 0..40 {
        look_up_people(&mut bdb);
    }
    scan(&mut bdb);
    let b_hot = bdb.hot_pages();
    assert!(!b_hot.is_empty());
    // Room for B's hot frames and eight more: B's cold ones go.
    let limit = (b_hot.len() as u64 + 8) * CHARGE;
    let used = pager::resize_shared_cache(limit).unwrap();
    assert!(used <= limit, "{used} over {limit}");

    let mut a = open(&pa, 256 << 10, 64 << 10);
    assert!(
        b.cache.used() <= b.cache.limit(),
        "the pool holds {} of {}",
        b.cache.used(),
        b.cache.limit()
    );
    let s = a.pager_stats();
    assert_eq!(s.prewarmed, 8, "what fits: {s:?}");
    assert_eq!(bdb.hot_pages(), b_hot, "B's hot frames are B's");
    a.check_hot_set().unwrap();
    look_up_people(&mut a);
    assert!(b.cache.used() <= b.cache.limit());
    drop(a);
    drop(bdb);
    cleanup(&pa);
    cleanup(&pb);
}
