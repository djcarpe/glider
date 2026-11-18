//! hotbench: the hot set (docs/HOT.md) against the two workloads it is for,
//! with and without it. Each command prints one JSON line: the disk reads
//! of the timed lookups, and the median and 99th percentile of one lookup
//! (a person, everyone they know, and those people's records).
//!
//!   hotbench scans    --dir D [--people 1000] [--items 1000000] [--cache-mb 8]
//!                     [--hot-mb 0] [--rounds 10] [--lookups 2000] [--scan 1]
//!   hotbench pressure --dir D [--people 1000] [--items 400000] [--cache-mb 8]
//!                     [--hot-mb 0] [--pool-mb 12] [--rounds 10] [--lookups 2000]
//!
//! `scans`: one database. Its people are looked up until warm; then each
//! round is a full scan of a label many times the cache (`MATCH (n:Item)
//! WHERE n.i >= 0 RETURN count(n)`), followed by the timed lookups. With
//! `--scan 0` there are no scans: every lookup is a cache hit, which times
//! what the hot set's bookkeeping adds to a hit.
//!
//! `pressure`: two databases under one process budget of `--pool-mb`. A's
//! people are looked up until warm; then each round B, whose cache could
//! hold four times the budget, scans a label larger than the budget while A
//! sits idle, and then A's lookups are timed.
//!
//! Databases are built in D on first use and reused (delete D to rebuild);
//! the hot set is an open option, so one file serves both runs. The
//! operating system's page cache is not dropped, so most "disk" reads are
//! served from it: the counts are exact, the latencies understate a disk.

use std::path::{Path, PathBuf};
use std::time::Instant;

use glider::budget::{self, MemoryBudget};
use glider::graph::{Dir, Graph, OpenOptions};
use glider::query;
use glider::store::Sync;
use glider::value::Value;

struct Args(Vec<String>);

impl Args {
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .position(|a| a == name)
            .and_then(|i| self.0.get(i + 1))
            .map(|s| s.as_str())
    }
    fn num(&self, name: &str, default: u64) -> u64 {
        self.get(name)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn peak_rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
        / 1024.0
}

fn open(path: &Path, cache_mb: u64, hot_mb: u64) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            sync: Sync::Off,
            cache_size: cache_mb << 20,
            hot_size: hot_mb << 20,
            ..OpenOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `people` people (ids from 1), each knowing eight others, then `items`
/// items. Built once; an existing file is reused as it is.
fn build(path: &Path, people: u64, items: u64) {
    if path.exists() {
        return;
    }
    let t = Instant::now();
    let mut g = open(path, 64, 0);
    g.autocommit = false;
    for i in 0..people {
        let name = Value::Text(format!("person {i}"));
        g.add_node(&["Person".into()], vec![("name".into(), name)])
            .unwrap();
    }
    for i in 0..people {
        for k in 0..8 {
            let other = 1 + mix(i * 8 + k) % people;
            g.add_edge(1 + i, other, "KNOWS", vec![]).unwrap();
        }
    }
    for i in 0..items {
        let props = vec![
            ("i".into(), Value::Int(i as i64)),
            ("pad".into(), Value::Text(format!("{:040}", mix(i)))),
        ];
        g.add_node(&["Item".into()], props).unwrap();
        if i % 50_000 == 49_999 {
            g.commit().unwrap();
        }
    }
    g.commit().unwrap();
    g.autocommit = true;
    g.checkpoint().unwrap();
    eprintln!(
        "built {} ({people} people, {items} items, {} MiB) in {:.1} s",
        path.display(),
        g.file_len() >> 20,
        t.elapsed().as_secs_f64()
    );
}

/// One lookup: a person, everyone they know, and their records.
fn look_up(g: &Graph, person: u64) {
    let n = g.node(person).expect("a person");
    std::hint::black_box(&n);
    for a in g.neighbors(person, Dir::Both, None) {
        std::hint::black_box(g.node(a.other));
    }
}

/// Warm-up is the measured workload itself: lookups of people at random,
/// eight per person. (In id order instead, each page of people would be
/// read in one stretch per pass, like a scan, and would take as many passes
/// to become hot.)
fn warm(g: &Graph, people: u64) {
    let mut seed = 3;
    for _ in 0..8 * people {
        seed = mix(seed);
        look_up(g, 1 + seed % people);
    }
}

fn scan(g: &mut Graph) -> f64 {
    let t = Instant::now();
    let r = query::execute(g, "MATCH (n:Item) WHERE n.i >= 0 RETURN count(n)").unwrap();
    std::hint::black_box(&r.rows);
    t.elapsed().as_secs_f64() * 1e3
}

/// Times `n` lookups of random people; their durations in nanoseconds go
/// into `times`. Returns the pages they read.
fn timed(g: &Graph, people: u64, n: u64, seed: &mut u64, times: &mut Vec<u64>) -> u64 {
    let before = g.pager_stats().reads;
    for _ in 0..n {
        *seed = mix(*seed);
        let p = 1 + *seed % people;
        let t = Instant::now();
        look_up(g, p);
        times.push(t.elapsed().as_nanos() as u64);
    }
    g.pager_stats().reads - before
}

fn percentile(sorted: &[u64], p: usize) -> f64 {
    let i = (sorted.len() * p / 100).min(sorted.len().saturating_sub(1));
    sorted.get(i).copied().unwrap_or(0) as f64 / 1e3
}

struct Report {
    times: Vec<u64>,
    reads: u64,
    scans_ms: Vec<f64>,
}

fn print(cmd: &str, a: &Args, items: u64, g: &Graph, mut r: Report, extra: String) {
    r.times.sort_unstable();
    r.scans_ms.sort_by(|x, y| x.total_cmp(y));
    let s = g.pager_stats();
    let n = r.times.len().max(1) as f64;
    println!(
        "{{\"cmd\":\"{cmd}\",\"cache_mb\":{},\"hot_mb\":{},\"people\":{},\"items\":{},\"rounds\":{},\
         \"lookups\":{},\"lookup_reads\":{},\"reads_per_lookup\":{:.3},\"p50_us\":{:.1},\"p99_us\":{:.1},\
         \"scan_ms_median\":{:.1},\"hot_pages\":{},\"hot_capacity\":{},\"promotions\":{},\"demotions\":{},\
         \"hot_hits\":{},\"hits\":{},\"misses\":{},\"peak_rss_mb\":{:.1}{extra}}}",
        a.num("--cache-mb", 8),
        a.num("--hot-mb", 0),
        a.num("--people", 1000),
        items,
        a.num("--rounds", 10),
        r.times.len(),
        r.reads,
        r.reads as f64 / n,
        percentile(&r.times, 50),
        percentile(&r.times, 99),
        r.scans_ms.get(r.scans_ms.len() / 2).copied().unwrap_or(0.0),
        s.hot_pages,
        s.hot_capacity,
        s.promotions,
        s.demotions,
        s.hot_hits,
        s.hits,
        s.misses,
        peak_rss_mb(),
    );
}

fn scans(a: &Args, dir: &Path) {
    let (people, items) = (a.num("--people", 1000), a.num("--items", 1_000_000));
    let path = dir.join(format!("scans-{people}-{items}.gldb"));
    build(&path, people, items);
    let mut g = open(&path, a.num("--cache-mb", 8), a.num("--hot-mb", 0));
    warm(&g, people);
    let mut r = Report {
        times: Vec::new(),
        reads: 0,
        scans_ms: Vec::new(),
    };
    let mut seed = 7;
    for _ in 0..a.num("--rounds", 10) {
        if a.num("--scan", 1) > 0 {
            r.scans_ms.push(scan(&mut g));
        }
        r.reads += timed(
            &g,
            people,
            a.num("--lookups", 2000),
            &mut seed,
            &mut r.times,
        );
    }
    print("scans", a, items, &g, r, String::new());
}

fn pressure(a: &Args, dir: &Path) {
    let pool = a.num("--pool-mb", 12) << 20;
    let limit = 64 * pool;
    budget::configure(MemoryBudget {
        limit,
        budget: limit / 2,
        cache: pool,
        work: 64 << 20,
        beam: limit / 2 - pool - (64 << 20),
    });
    let people = a.num("--people", 1000);
    let items = a.num("--items", 400_000);
    let pa = dir.join(format!("pressure-a-{people}.gldb"));
    let pb = dir.join(format!("pressure-b-{items}.gldb"));
    build(&pa, people, 20_000);
    build(&pb, 0, items);
    let ga = open(&pa, a.num("--cache-mb", 8), a.num("--hot-mb", 0));
    let mut gb = open(&pb, 4 * (pool >> 20), 0);
    warm(&ga, people);
    let mut r = Report {
        times: Vec::new(),
        reads: 0,
        scans_ms: Vec::new(),
    };
    let mut seed = 11;
    let mut a_resident = Vec::new();
    for _ in 0..a.num("--rounds", 10) {
        r.scans_ms.push(scan(&mut gb));
        a_resident.push(ga.pager_stats().resident_pages);
        r.reads += timed(
            &ga,
            people,
            a.num("--lookups", 2000),
            &mut seed,
            &mut r.times,
        );
    }
    let b = budget::process().expect("configured");
    let extra = format!(
        ",\"pool_mb\":{},\"pool_used_mb\":{:.1},\"b_resident\":{},\"a_resident_after_b_min\":{}",
        pool >> 20,
        b.cache.used() as f64 / (1 << 20) as f64,
        gb.pager_stats().resident_pages,
        a_resident.iter().min().copied().unwrap_or(0),
    );
    print("pressure", a, items, &ga, r, extra);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a = Args(args);
    let Some(dir) = a.get("--dir").map(PathBuf::from) else {
        eprintln!("usage: hotbench scans|pressure --dir D [options]; see the source");
        std::process::exit(2);
    };
    std::fs::create_dir_all(&dir).unwrap();
    match a.0.first().map(|s| s.as_str()) {
        Some("scans") => scans(&a, &dir),
        Some("pressure") => pressure(&a, &dir),
        _ => {
            eprintln!("usage: hotbench scans|pressure --dir D [options]; see the source");
            std::process::exit(2);
        }
    }
}
