//! The hot set (docs/HOT.md): the pages a workload comes back to stay in
//! memory through scans many times the cache, follow their data through
//! copy-on-write, deletes and rollback, and never make the cache stall; a
//! hot size of zero is the engine without one, decision for decision.
//!
//! The process budget's side (another database's pressure, the pool's
//! bounds) is in `hot_budget.rs`: the budget is process-wide, and would
//! change every test in this file.

use std::path::{Path, PathBuf};

use glider::graph::{Dir, Graph, OpenOptions};
use glider::query;
use glider::storage::hot::{MIN_TICK, PROMOTE};
use glider::storage::page::{self, Page};
use glider::storage::pager::{Config, Pager, Stats};
use glider::store::Sync;
use glider::value::Value;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glider-hot-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("db")
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
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
}

// ------------------------------------------------------------- zero is today

/// A fixed workload straight through a pager with a 64-frame cache: point
/// reads skewed towards a few pages, whole passes, copy-on-write writes,
/// commits, rolled-back transactions and checkpoints. Returns the pager's
/// statistics and a fingerprint of exactly which reads missed, so that two
/// runs agree only if every eviction decision did.
fn pager_workload(p: &Pager) -> (Stats, u64) {
    let mut r = Rng(0x2545_F491_4F6C_DD1D);
    let mut pnos = Vec::new();
    for i in 0..400u64 {
        let a = p.alloc(page::KIND_LEAF, 1).unwrap();
        p.modify(a, |b| b[page::HEADER..].fill(i as u8)).unwrap();
        pnos.push(a);
    }
    p.set_root(0, pnos[0]);
    p.commit();
    p.checkpoint(0).unwrap();
    let mut fingerprint = 0xcbf2_9ce4_8422_2325u64;
    let mut step = 0u64;
    let mut read = |p: &Pager, pno: u64| {
        let before = p.stats().misses;
        p.get(pno).unwrap();
        step += 1;
        if p.stats().misses != before {
            for x in [step, pno] {
                fingerprint = (fingerprint ^ x).wrapping_mul(0x100_0000_01b3);
            }
        }
    };
    for round in 0..30u64 {
        for _ in 0..300 {
            let k = if r.below(10) < 7 {
                r.below(20)
            } else {
                r.below(pnos.len() as u64)
            };
            read(p, pnos[k as usize]);
        }
        if round % 3 == 0 {
            for &pno in &pnos {
                read(p, pno);
            }
        }
        for _ in 0..10 {
            let k = r.below(pnos.len() as u64) as usize;
            let w = p.writable(pnos[k]).unwrap();
            p.modify(w, |b| b[page::HEADER] = round as u8).unwrap();
            pnos[k] = w;
        }
        p.commit();
        if round % 7 == 6 {
            for _ in 0..5 {
                let k = r.below(pnos.len() as u64) as usize;
                let w = p.writable(pnos[k]).unwrap();
                p.modify(w, |b| b[page::HEADER] = 0xee).unwrap();
            }
            p.rollback();
        }
        if round % 10 == 9 {
            p.checkpoint(round).unwrap();
        }
    }
    (p.stats(), fingerprint)
}

/// The same idea one layer up, through the graph API (no query text, so a
/// change to the planner cannot move it): inserts, point reads, neighbour
/// lists, a pass over a label, property writes and checkpoints.
fn graph_workload(g: &mut Graph) -> Stats {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    g.autocommit = false;
    for i in 0..2000i64 {
        let label = if i % 3 == 0 { "A" } else { "B" };
        g.add_node(
            &[label.into()],
            vec![
                ("i".into(), Value::Int(i)),
                ("t".into(), Value::Text(format!("{i:>60}"))),
            ],
        )
        .unwrap();
    }
    for _ in 0..3000 {
        let (a, b) = (1 + r.below(2000), 1 + r.below(2000));
        g.add_edge(a, b, "E", vec![]).unwrap();
    }
    g.commit().unwrap();
    g.checkpoint().unwrap();
    let a = g.strings.lookup("A").unwrap();
    for round in 0..20 {
        for _ in 0..200 {
            g.node(1 + r.below(2000));
        }
        for _ in 0..50 {
            g.neighbors(1 + r.below(2000), Dir::Both, None);
        }
        for id in g.label_members_from(a, 0, usize::MAX) {
            if id % 10 == 0 {
                g.node(id);
            }
        }
        for _ in 0..20 {
            g.set_node_prop(1 + r.below(2000), "w", Value::Int(round))
                .unwrap();
        }
        g.commit().unwrap();
        if round % 5 == 4 {
            g.checkpoint().unwrap();
        }
    }
    g.autocommit = true;
    g.pager_stats()
}

/// Property 1: with no hot set the cache reads, hits, misses and evicts
/// exactly as it did before the hot set existed, down to which reads miss.
/// The numbers were taken from the engine before it had one (commit
/// daa234b), running these same two workloads.
#[test]
fn zero_hot_size_reads_hits_misses_and_evicts_as_before() {
    let path = temp("zero");
    let cfg = Config {
        page_size: 512,
        cache_bytes: 512 * 64,
        max_memory: u64::MAX,
        segment_bytes: 1 << 30,
    };
    let p = Pager::create(&path, &cfg, [9; 16]).unwrap();
    // Zero, and a size too small for one page, are no hot set.
    p.set_hot_size(0);
    p.set_hot_size(100);
    let (s, fingerprint) = pager_workload(&p);
    assert_eq!(
        (s.reads, s.writes, s.hits, s.misses, s.evictions),
        (6598, 705, 6722, 6598, 7243)
    );
    assert_eq!(
        (
            s.copies,
            s.commits,
            s.rollbacks,
            s.checkpoints,
            s.resident_pages
        ),
        (314, 31, 4, 4, 63)
    );
    assert_eq!(
        fingerprint, 0x6fc2_f788_0301_2999,
        "another page missed: an eviction decision changed"
    );
    assert_eq!(
        (s.hot_capacity, s.hot_pages, s.hot_hits, s.promotions),
        (0, 0, 0, 0)
    );
    drop(p);
    cleanup(&path);

    let path = temp("zero-graph");
    let mut g = Graph::open_opts(
        &path,
        OpenOptions {
            sync: Sync::Off,
            page_size: 1024,
            cache_size: 32 << 10,
            hot_size: 0,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let s = graph_workload(&mut g);
    assert_eq!(
        (s.reads, s.writes, s.hits, s.misses, s.evictions),
        (20458, 6279, 125368, 20458, 21544)
    );
    assert_eq!((s.copies, s.commits, s.checkpoints), (491, 22, 5));
    assert_eq!(s.hot_capacity, 0);
    drop(g);
    cleanup(&path);
}

// --------------------------------------------------- the rule, through a pager

/// A file pager with a cache of `frames` 512-byte frames, `pages` pages
/// written and checkpointed, and a hot set of `hot` frames turned on last,
/// so its clock starts at the next read. Its ticks are [`MIN_TICK`] reads.
fn pager_with(name: &str, frames: u64, pages: usize, hot: u64) -> (Pager, PathBuf, Vec<u64>) {
    let path = temp(name);
    let cfg = Config {
        page_size: 512,
        cache_bytes: 512 * frames,
        max_memory: u64::MAX,
        segment_bytes: 1 << 30,
    };
    let p = Pager::create(&path, &cfg, [3; 16]).unwrap();
    let mut pnos = Vec::new();
    for i in 0..pages {
        let a = p.alloc(page::KIND_LEAF, 1).unwrap();
        p.modify(a, |b| b[page::HEADER] = i as u8).unwrap();
        pnos.push(a);
    }
    p.set_root(0, pnos[0]);
    p.commit();
    p.checkpoint(0).unwrap();
    p.set_hot_size(512 * hot);
    (p, path, pnos)
}

/// Counts reads, and moves the clock on with filler: pages 10 to 29, a
/// different one each tick and the same one only every twenty ticks (more
/// than a halving apart), so filler never gets hot.
struct Clock {
    reads: u64,
}

impl Clock {
    fn read(&mut self, p: &Pager, pno: u64) {
        p.get(pno).unwrap();
        self.reads += 1;
    }
    /// Filler until the next read is read number `n`.
    fn until(&mut self, p: &Pager, pnos: &[u64], n: u64) {
        while self.reads < n {
            let filler = pnos[10 + (self.reads / MIN_TICK % 20) as usize];
            self.read(p, filler);
        }
    }
}

/// The rule for "often", at its edges, counting real pager reads: a page
/// read in three ticks is cold and in a fourth hot; one stretch of `2T + 1`
/// reads of a page, however placed, is three ticks at most and leaves it
/// cold, while `2T + 2` can reach a fourth; a hot page nobody reads cools
/// out as its heat halves to nothing, with nothing else wanting its place.
#[test]
fn the_rule_at_its_edges_through_pager_reads() {
    let (p, path, pnos) = pager_with("rule", 64, 40, 8);
    let t = MIN_TICK;
    let hot = |pno: u64| p.hot_pages().contains(&pno);
    let mut c = Clock { reads: 0 };
    for tick in 0..PROMOTE as u64 {
        c.until(&p, &pnos, tick * t);
        c.read(&p, pnos[0]);
        assert_eq!(hot(pnos[0]), tick + 1 == PROMOTE as u64, "tick {tick}");
    }
    // Reads of a hot page are hot hits.
    let hits = p.stats().hot_hits;
    c.read(&p, pnos[0]);
    assert_eq!(p.stats().hot_hits, hits + 1);
    // One stretch, from the last read of tick 7 through ticks 8 and 9.
    c.until(&p, &pnos, 8 * t - 1);
    for _ in 0..2 * t + 1 {
        c.read(&p, pnos[1]);
    }
    assert!(!hot(pnos[1]), "2T + 1 reads in a row are three ticks");
    // From the last read of tick 11, one read more reaches tick 14.
    c.until(&p, &pnos, 12 * t - 1);
    for _ in 0..2 * t + 2 {
        c.read(&p, pnos[2]);
    }
    assert!(hot(pnos[2]), "2T + 2 reads in a row reach a fourth tick");
    // Heat 4 halves as ticks 16, 32 and 48 begin: 2, 1, then nothing.
    c.until(&p, &pnos, 48 * t - 1);
    assert!(hot(pnos[0]) && hot(pnos[2]), "still hot before tick 48");
    c.until(&p, &pnos, 48 * t + 1);
    assert!(!hot(pnos[0]) && !hot(pnos[2]), "cooled out at tick 48");
    let s = p.stats();
    assert_eq!((s.hot_pages, s.promotions, s.demotions), (0, 2, 2));
    p.check_hot().unwrap();
    drop(p);
    cleanup(&path);
}

/// Property 8 through the pager: in a full hot set, a page hotter than the
/// coolest hot page takes its place, and one only as hot does not.
#[test]
fn a_hotter_page_displaces_the_coolest_through_pager_reads() {
    let (p, path, pnos) = pager_with("displace", 64, 40, 2);
    let t = MIN_TICK;
    let mut c = Clock { reads: 0 };
    // Page 0 read in ticks 0 to 4 (heat 5), page 1 in 1 to 4 (heat 4).
    for tick in 0..5 {
        c.until(&p, &pnos, tick * t);
        c.read(&p, pnos[0]);
        if tick >= 1 {
            c.read(&p, pnos[1]);
        }
    }
    assert_eq!(p.hot_pages(), vec![pnos[0], pnos[1]], "hottest first");
    // Page 2 at heat 4, as hot as page 1: it stays out.
    for tick in 5..9 {
        c.until(&p, &pnos, tick * t);
        c.read(&p, pnos[2]);
    }
    assert_eq!(p.hot_pages(), vec![pnos[0], pnos[1]]);
    // At heat 5 it is hotter, and page 1 gives way.
    c.until(&p, &pnos, 9 * t);
    c.read(&p, pnos[2]);
    assert_eq!(p.hot_pages(), vec![pnos[2], pnos[0]]);
    let s = p.stats();
    assert_eq!((s.hot_capacity, s.promotions, s.demotions), (2, 3, 1));
    p.check_hot().unwrap();
    drop(p);
    cleanup(&path);
}

/// Property 9: a cache whose cold frames are all held by readers evicts a
/// hot frame rather than fail; with a cold frame free it takes that one;
/// and with every frame held it fails exactly as it did before.
#[test]
fn nothing_stalls_when_every_cold_frame_is_held() {
    let (p, path, pnos) = pager_with("pinned", 16, 40, 8);
    p.evict_all().unwrap();
    // Pages 0 to 7 read round and round through four ticks: all hot.
    for _ in 0..4 * MIN_TICK / 8 + 1 {
        for &pno in &pnos[..8] {
            p.get(pno).unwrap();
        }
    }
    assert_eq!(p.stats().hot_pages, 8);
    // The other eight frames held by readers.
    let mut held: Vec<Page> = pnos[8..16].iter().map(|&n| p.get(n).unwrap()).collect();
    assert_eq!(p.stats().resident_pages, 16);
    p.get(pnos[20]).unwrap();
    assert_eq!(p.stats().hot_pages, 7, "a hot frame gave way");
    // Page 20 is in a cold frame nobody holds: the next read takes that.
    p.get(pnos[21]).unwrap();
    assert_eq!(
        p.stats().hot_pages,
        7,
        "a cold frame was free: hot pages stay"
    );
    // Every frame held: the error it was before there was a hot set.
    for pno in p.hot_pages() {
        held.push(p.get(pno).unwrap());
    }
    held.push(p.get(pnos[21]).unwrap());
    assert_eq!(p.stats().resident_pages, 16);
    let e = p.get(pnos[30]).unwrap_err();
    assert!(e.to_string().contains("page cache budget exhausted"), "{e}");
    drop(held);
    p.get(pnos[30]).unwrap();
    p.check_hot().unwrap();
    drop(p);
    cleanup(&path);
}

// ------------------------------------------------------------ graph workloads

const PEOPLE: u64 = 8;
const ITEMS: u64 = 12_000;

/// 1 KiB pages and a 64-frame cache; `hot` bytes of it may be hot.
fn open(path: &Path, hot: u64) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            sync: Sync::Off,
            page_size: 1024,
            cache_size: 64 << 10,
            hot_size: hot,
            ..OpenOptions::default()
        },
    )
    .unwrap()
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

/// Pages read by `f`.
fn reads(g: &mut Graph, f: impl FnOnce(&mut Graph)) -> u64 {
    let before = g.pager_stats().reads;
    f(g);
    g.pager_stats().reads - before
}

/// The kind of each page, as the file has it (1 KiB pages, one segment).
fn kinds(path: &Path, pnos: &[u64]) -> Vec<u8> {
    let file = std::fs::read(path).unwrap();
    pnos.iter().map(|&p| file[p as usize * 1024]).collect()
}

/// Property 3: the pages a workload returns to again and again stay through
/// label scans of many times the cache; after warm-up their lookups read
/// nothing, where without a hot set the same lookups after the same scans
/// read from disk.
#[test]
fn hot_keys_survive_label_scans_many_times_the_cache() {
    let mut lookup_reads = Vec::new();
    for hot in [0u64, 32 << 10] {
        let path = temp(&format!("scans-{hot}"));
        let mut g = open(&path, hot);
        build(&mut g, ITEMS);
        for _ in 0..40 {
            look_up_people(&mut g);
        }
        let mut total = 0;
        for _ in 0..4 {
            let scan = reads(&mut g, |g| {
                let q = "MATCH (n:Item) WHERE n.i >= 0 RETURN count(n)";
                let r = query::execute(g, q).unwrap();
                assert_eq!(r.rows[0][0], Value::Int(ITEMS as i64));
            });
            assert!(
                scan > 10 * 64,
                "the scan read {scan} pages: not many caches"
            );
            total += reads(&mut g, look_up_people);
        }
        if hot > 0 {
            let s = g.pager_stats();
            assert!(s.hot_pages > 0 && s.hot_hits > 0, "{s:?}");
            assert!(s.hot_pages <= s.hot_capacity);
            g.check_hot_set().unwrap();
        }
        lookup_reads.push(total);
        drop(g);
        cleanup(&path);
    }
    assert!(
        lookup_reads[0] > 0,
        "without a hot set the scans push the people out"
    );
    assert_eq!(lookup_reads[1], 0, "with one, their lookups read nothing");
}

/// Property 4: one pass over a large label, touching every leaf of it many
/// times, promotes no leaf: whatever it made hot is interior pages, the
/// upper levels every read went through. Once with each record read ten
/// times over, once as a query's scan, each from a fresh open.
#[test]
fn one_pass_over_a_label_makes_no_leaf_hot() {
    let path = temp("pass");
    let mut g = open(&path, 32 << 10);
    build(&mut g, ITEMS);
    drop(g);
    for pass in ["records", "scan"] {
        let mut g = open(&path, 32 << 10);
        assert!(g.hot_pages().is_empty(), "every open is cold");
        let item = g.strings.lookup("Item").unwrap();
        if pass == "records" {
            let mut from = 0;
            loop {
                let ids = g.label_members_from(item, from, 4096);
                // A leaf holds about fourteen records: each run of sixteen
                // ids read ten times, alternating so that the one-record
                // read cache cannot answer.
                for run in ids.chunks(16) {
                    for _ in 0..10 {
                        for &id in run {
                            assert!(g.node(id).is_some());
                        }
                    }
                }
                match ids.last() {
                    Some(&last) if ids.len() == 4096 => from = last + 1,
                    _ => break,
                }
            }
        } else {
            let q = "MATCH (n:Item) WHERE n.pad <> 'y' RETURN count(n)";
            let r = query::execute(&mut g, q).unwrap();
            assert_eq!(r.rows[0][0], Value::Int(ITEMS as i64));
        }
        let hot = g.hot_pages();
        // Read record by record, every read goes through the upper levels;
        // a cursor's scan reads each page once, and makes nothing hot.
        assert_eq!(!hot.is_empty(), pass == "records", "{pass}: {hot:?}");
        for (pno, kind) in hot.iter().zip(kinds(&path, &hot)) {
            assert_eq!(
                kind,
                page::KIND_INTERIOR,
                "{pass}: page {pno} of kind {kind} is hot after one pass"
            );
        }
        g.check_hot_set().unwrap();
    }
    cleanup(&path);
}

/// Property 7: a hot page that is written is still hot under its new
/// number, and its old number is not once freed; a hot page whose data is
/// deleted stops being hot; a rollback leaves only pages that exist hot,
/// and the pages it restores come back at their next read.
#[test]
fn hot_follows_the_data_through_writes_deletes_and_rollback() {
    let path = temp("follow");
    let mut g = open(&path, 32 << 10);
    build(&mut g, 2000);
    // A person whose biography overflows onto pages of its own.
    let bio = Value::Text("b".repeat(6000));
    let big = g
        .add_node(&["Person".into()], vec![("bio".into(), bio)])
        .unwrap();
    let warm = |g: &mut Graph| {
        for _ in 0..40 {
            look_up_people(g);
            assert!(g.node(big).is_some());
        }
    };
    warm(&mut g);
    g.checkpoint().unwrap();
    let before = g.hot_pages();
    let overflow = kinds(&path, &before)
        .iter()
        .filter(|k| **k == page::KIND_OVERFLOW)
        .count();
    assert!(overflow >= 5, "the biography's pages are hot: {overflow}");

    // A write: the leaf holding person 1, and the path above it, are
    // copied; the copies hold their places.
    g.set_node_prop(1, "seen", Value::Int(1)).unwrap();
    let after = g.hot_pages();
    assert_eq!(after.len(), before.len(), "copies took the places");
    let moved: Vec<u64> = before
        .iter()
        .copied()
        .filter(|p| !after.contains(p))
        .collect();
    assert!(!moved.is_empty(), "the path to person 1 has new numbers");
    g.check_hot_set().unwrap();
    assert_eq!(reads(&mut g, look_up_people), 0);

    // A delete: the biography's pages are freed, and are not hot.
    g.set_node_prop(big, "bio", Value::Text("short".into()))
        .unwrap();
    g.checkpoint().unwrap();
    let after = g.hot_pages();
    assert!(after.len() <= before.len() - overflow);
    assert!(kinds(&path, &after)
        .iter()
        .all(|k| *k != page::KIND_OVERFLOW));
    g.check_hot_set().unwrap();

    // A rollback: the copies it made are dropped, and with them their
    // places; the pages they were copied from are the data again.
    g.autocommit = false;
    for p in 1..=PEOPLE {
        g.set_node_prop(p, "draft", Value::Int(1)).unwrap();
    }
    g.rollback().unwrap();
    g.autocommit = true;
    g.check_hot_set().unwrap();
    assert!(g.hot_pages().len() < after.len(), "the copies' places went");
    assert_eq!(reads(&mut g, look_up_people), 0, "still in the cache");
    for _ in 0..10 {
        look_up_people(&mut g);
    }
    let again = g.hot_pages();
    for pno in &after {
        assert!(
            again.contains(pno),
            "page {pno} is hot again at its next read"
        );
    }
    g.check_hot_set().unwrap();
    drop(g);
    cleanup(&path);
}

/// Property 10: `pager::Stats`, `Graph::stats`, `STATS` and the telemetry
/// snapshot report the hot set; without one, `STATS` is as it was.
#[test]
fn stats_and_telemetry_report_the_hot_set() {
    for hot in [0u64, 32 << 10] {
        let path = temp(&format!("stats-{hot}"));
        let mut g = open(&path, hot);
        build(&mut g, 500);
        for _ in 0..40 {
            look_up_people(&mut g);
        }
        let st = g.stats();
        let ps = g.pager_stats();
        let m = g.telemetry();
        let r = query::execute(&mut g, "STATS").unwrap();
        let row = |name: &str| {
            r.rows
                .iter()
                .find(|row| row[0] == Value::Text(name.into()))
                .map(|row| row[1].clone())
        };
        assert_eq!(
            (st.hot_capacity, st.hot_pages, st.promotions, st.demotions),
            (ps.hot_capacity, ps.hot_pages, ps.promotions, ps.demotions)
        );
        assert!(ps.hot_hits >= st.hot_hits);
        assert_eq!((m.hot_pages, m.promotions), (ps.hot_pages, ps.promotions));
        assert_eq!(
            (m.prewarmed, ps.prewarmed),
            (0, 0),
            "nothing loads a list yet"
        );
        if hot == 0 {
            assert_eq!(ps.hot_capacity, 0);
            assert_eq!((row("hot_capacity"), row("hot_pages")), (None, None));
        } else {
            assert_eq!(ps.hot_capacity, 32);
            assert!(ps.hot_pages > 0 && ps.hot_hits > 0 && ps.promotions >= ps.hot_pages);
            assert_eq!(row("hot_capacity"), Some(Value::Int(32)));
            assert_eq!(row("hot_pages"), Some(Value::Int(ps.hot_pages as i64)));
            for name in ["hot_hits", "promotions", "demotions", "prewarmed"] {
                assert!(row(name).is_some(), "STATS has {name}");
            }
        }
        let json = m.to_json();
        for key in [
            "hot_pages",
            "hot_hits",
            "promotions",
            "demotions",
            "prewarmed",
        ] {
            assert!(json.contains(&format!("\"{key}\":")), "{json}");
        }
        let prom = glider::telemetry::prometheus(&[(g.telemetry_name(), m)]);
        for name in [
            "glider_db_cache_hot_resident{",
            "glider_db_cache_hot_hits_total{",
            "glider_db_cache_hot_promotions_total{",
            "glider_db_cache_hot_demotions_total{",
            "glider_db_cache_hot_prewarmed_total{",
        ] {
            assert!(prom.contains(name), "{name}");
        }
        drop(g);
        cleanup(&path);
    }
}
