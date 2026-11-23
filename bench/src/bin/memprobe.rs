//! Native memory of the whole-graph reads a replication peer serves, measured:
//! the watermark and count scans a catch-up asks for first, and a base copy.
//!
//!   memprobe load  <db> <nodes> [limit_mb]       build a Ctx-shaped graph (Jido.Context's)
//!   memprobe probe <db> [base dir] [limit_mb]    run the catch-up reads and a base copy
//!
//! `limit_mb` (default 512) is the cgroup limit the process budget is cut
//! from; `ORIGIN` names the writer the counts filter on, `NO_SNAPSHOT` skips
//! the base copy, `THREADS` (default 10) is how many threads a load spreads
//! its batches over.
//!
//! After every step it prints what the allocator holds live (and the peak), and
//! what the process holds resident (VmRSS, RssAnon) and the C allocator keeps
//! (`glider::budget::native`). Numbers from it are in docs/BENCHMARKS.md.
//! Every statement runs on a thread of its own, as a NIF call lands on whichever
//! dirty scheduler is free.
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use glider::graph::{Graph, OpenOptions};
use glider::query;

struct Counting;
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size() as u64, Ordering::Relaxed) + l.size() as u64;
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            if n >= l.size() {
                let now = LIVE.fetch_add((n - l.size()) as u64, Ordering::Relaxed)
                    + (n - l.size()) as u64;
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub((l.size() - n) as u64, Ordering::Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static A: Counting = Counting;

fn status(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix(key)
                    .and_then(|r| r.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            })
        })
        .unwrap_or(0)
        * 1024
}

fn mib(b: u64) -> f64 {
    (b as f64 / 1048576.0 * 10.0).round() / 10.0
}

fn report(step: &str) {
    let peak = PEAK.swap(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let b = glider::budget::ensure();
    let n = glider::budget::native().unwrap_or_default();
    println!(
        "{step:<28} live {:>7} MiB  step peak {:>7} MiB  rss {:>7} anon {:>7} hwm {:>7} MiB  cache {:>6} work {:>5} MiB  malloc: held {:>6} used {:>6} free {:>6} MiB",
        mib(LIVE.load(Ordering::Relaxed)),
        mib(peak),
        mib(status("VmRSS:")),
        mib(status("RssAnon:")),
        mib(status("VmHWM:")),
        mib(b.cache.used()),
        mib(b.work.used()),
        mib(n.held),
        mib(n.used),
        mib(n.free),
    );
}

fn open(path: &Path) -> Graph {
    let budget = glider::budget::ensure();
    // as the NIF opens a graph for Jido.Context (work_mem 32 MiB asked)
    let opts = OpenOptions {
        cache_size: budget.config.cache,
        work_mem: (32u64 << 20).min(budget.config.work / 2),
        ..OpenOptions::default()
    };
    Graph::open_opts(path, opts).expect("open")
}

/// One statement as the NIF's `query/4` runs it, on a thread of its own.
fn run(g: &mut Graph, q: &str) -> query::QueryResult {
    std::thread::scope(|s| {
        s.spawn(|| {
            let _lease = glider::budget::ensure()
                .work
                .lease(g.query_memory())
                .expect("workspace");
            let limits = query::QueryLimits {
                max_bytes: (8u64 << 20).min(g.query_memory() / 4),
                ..Default::default()
            };
            query::execute_capped(g, q, &[], limits).expect(q)
        })
        .join()
        .unwrap()
    })
}

fn load(path: &Path, nodes: u64) {
    let mut g = open(path);
    for s in ["INDEX ON :Ctx(_key)", "INDEX ON :CtxTomb(_key)"] {
        query::execute(&mut g, s).unwrap();
    }
    let mut batch: Vec<(String, Vec<(String, glider::value::Value)>)> = Vec::new();
    let mut seq = 1u64;
    // as the NIF's write_batch lands on whichever of the dirty schedulers is free
    let threads = std::env::var("THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10usize);
    let gm = std::sync::Mutex::new(g);
    let mut n = 0usize;
    let flush = |gm: &std::sync::Mutex<Graph>,
                 batch: &mut Vec<(String, Vec<(String, glider::value::Value)>)>,
                 seq: &mut u64,
                 n: &mut usize| {
        if !batch.is_empty() {
            let b = std::mem::take(batch);
            *n += 1;
            let k = *n % threads;
            std::thread::scope(|s| {
                std::thread::Builder::new()
                    .name(format!("dirty{k}"))
                    .spawn_scoped(s, || {
                        query::execute_batch(&mut gm.lock().unwrap(), &b).unwrap()
                    })
                    .unwrap()
                    .join()
                    .unwrap();
            });
            *seq += 1;
            if *n % 500 == 0 {
                report(&format!("batch {n}"));
            }
        }
    };
    for i in 0..nodes {
        let stamp = format!(
            r#"_seq: {seq}, _origin: "mem_a@127.0.0.1", _ts: 1763900000000, _topic: "graph""#
        );
        batch.push((
            format!(
                r#"CREATE (n:Ctx:Thing {{_key: "ws:lab/Thing/t{i}", i: {i}, name: "thing {i}", {stamp}}})"#
            ),
            vec![],
        ));
        if i % 2 == 1 {
            batch.push((
                format!(
                    r#"MATCH (a:Ctx {{_key: "ws:lab/Thing/t{}"}}), (b:Ctx {{_key: "ws:lab/Thing/t{i}"}}) CREATE (a)-[:NEXT {{{stamp}}}]->(b)"#,
                    i - 1
                ),
                vec![],
            ));
        }
        if batch.len() >= 600 {
            flush(&gm, &mut batch, &mut seq, &mut n);
        }
    }
    flush(&gm, &mut batch, &mut seq, &mut n);
    let mut g = gm.into_inner().unwrap();
    g.checkpoint().unwrap();
    report("loaded");
}

fn probe(path: &Path, out: &Path, snapshot: bool) {
    report("start");
    let mut g = open(path);
    report("opened");
    let watermarks = [
        "MATCH (n:Ctx) RETURN n._origin, max(n._seq)",
        "MATCH (t:CtxTomb) RETURN t._origin, max(t._seq)",
        "MATCH (a:Ctx)-[r]->(b:Ctx) RETURN r._origin, max(r._seq)",
    ];
    for (i, q) in watermarks.iter().enumerate() {
        let r = run(&mut g, q);
        report(&format!("watermark {i} {:?}", r.rows));
    }
    let o = std::env::var("ORIGIN").unwrap_or_else(|_| "mem_a@127.0.0.1".into());
    let counts = [
        format!(r#"MATCH (n:Ctx) WHERE n._origin = "{o}" AND n._seq > -1 RETURN count(n)"#),
        format!(r#"MATCH (n:CtxTomb) WHERE n._origin = "{o}" AND n._seq > -1 RETURN count(n)"#),
        format!(
            r#"MATCH (a:Ctx)-[r]->(b:Ctx) WHERE r._origin = "{o}" AND r._seq > -1 RETURN count(r)"#
        ),
    ];
    for (i, q) in counts.iter().enumerate() {
        let r = run(&mut g, q);
        report(&format!(
            "count {i} = {:?}",
            r.rows.first().map(|r| r[0].clone())
        ));
    }
    let r = run(
        &mut g,
        &format!(
            r#"MATCH (n:Ctx) WHERE n._origin = "{o}" AND n._seq > -1 RETURN n._seq, n, n._key ORDER BY n._seq, n._key LIMIT 1000"#
        ),
    );
    report(&format!("page ({} rows)", r.rows.len()));
    if snapshot {
        let _ = std::fs::remove_dir_all(out);
        let spool = out.with_extension("spool");
        let _ = std::fs::remove_dir_all(&spool);
        let url = format!("file://{}", out.display());
        let rc = glider::stream::config::ReplicaConfig {
            url: url.clone(),
            min_bytes: 0,
            spool: Some(spool.display().to_string()),
            ..Default::default()
        };
        let backend = glider::stream::config::open_backend(&rc).unwrap();
        let r = glider::stream::paged::PagedReplicator::new(path, spool, backend, rc);
        // the writer polls for replication while the base is copied
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            let h = s.spawn(|| {
                let rep = r.sync(true, false);
                done.store(true, Ordering::Release);
                rep
            });
            while !done.load(Ordering::Acquire) {
                let _ = g.poll_replication();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let rep = h.join().unwrap().unwrap();
            report(&format!("base copy ({} bytes)", rep.bytes));
        });
    }
    drop(g);
    report("closed");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let limit = args
        .get(4)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(512)
        << 20;
    glider::budget::configure(glider::budget::MemoryBudget::from_limit(limit, None));
    match args.get(1).map(String::as_str) {
        Some("load") => load(Path::new(&args[2]), args[3].parse().unwrap()),
        Some("probe") => {
            let db = PathBuf::from(&args[2]);
            let out = PathBuf::from(
                args.get(3)
                    .cloned()
                    .unwrap_or_else(|| format!("{}.base", db.display())),
            );
            probe(&db, &out, std::env::var("NO_SNAPSHOT").is_err())
        }
        _ => eprintln!("memprobe load <db> <nodes> | memprobe probe <db> [out] [limit_mb]"),
    }
}
