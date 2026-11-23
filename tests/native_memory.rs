//! Native memory outside the pools: what a replication peer's whole-graph
//! reads and base copies allocate, and whether freed memory goes back.
//!
//! A peer catching another up serves it the highest seq per origin over every
//! node and edge, how many entities lie past a cursor, pages of them, and a
//! base copy of the database (ragentic's catch-up, `Jido.Context.Replay`).
//! Every one of those must cost the page cache plus a fixed few MiB, however
//! large the graph: a base copy once read the whole database file into
//! memory, which at 2,000,000 nodes OOM-killed a 512 MiB pod.
//!
//! And memory those operations free must not stay resident: glibc keeps what
//! is freed in the middle of a heap, so a shrunk cache or a scan that cycled
//! it held its high-water mark for good, past the governor's limit, and the
//! peer answered `:busy` to every later request.
//!
//! One test, run alone in its own process: the allocator counts are
//! process-wide.
#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use glider::budget::{self, MemoryBudget};
use glider::query::{self, Cursor, QueryLimits};
use glider::stream::config::{open_backend, ReplicaConfig};
use glider::stream::paged::PagedReplicator;
use glider::{Graph, OpenOptions};

const MIB: u64 = 1 << 20;
const CACHE: u64 = 2 * MIB;
const WORK_MEM: u64 = 8 * MIB;

// Bytes allocated and not freed, and the most there were since the last reset.
struct Counting;
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
fn grew(by: u64) {
    let now = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            grew(l.size() as u64);
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
                grew((n - l.size()) as u64);
            } else {
                LIVE.fetch_sub((l.size() - n) as u64, Ordering::Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static ALLOC: Counting = Counting;

/// The most bytes live during `f`, above what was live before it.
fn peak_during<R>(f: impl FnOnce() -> R) -> (u64, R) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let r = f();
    (PEAK.load(Ordering::Relaxed).saturating_sub(before), r)
}

fn status(field: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix(field)?.strip_prefix(':'))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap()
}

fn open(path: &Path, cache: u64) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            cache_size: cache,
            work_mem: WORK_MEM,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

/// `nodes` nodes shaped as Jido.Context writes them (a stamp on every node
/// and edge), and an edge from every even node to the next.
fn load(path: &Path, nodes: u64) {
    let mut g = open(path, CACHE);
    query::execute(&mut g, "INDEX ON :Ctx(_key)").unwrap();
    let mut batch = Vec::new();
    for i in 0..nodes {
        let stamp = format!(
            r#"_seq: {}, _origin: "a@host", _ts: 1, _topic: "t""#,
            i / 300 + 1
        );
        batch.push((
            format!(r#"CREATE (n:Ctx:Thing {{_key: "k{i}", i: {i}, name: "thing {i}", {stamp}}})"#),
            vec![],
        ));
        if i % 2 == 1 {
            batch.push((
                format!(
                    r#"MATCH (a:Ctx {{_key: "k{}"}}), (b:Ctx {{_key: "k{i}"}}) CREATE (a)-[:NEXT {{{stamp}}}]->(b)"#,
                    i - 1
                ),
                vec![],
            ));
        }
        if batch.len() >= 450 {
            query::execute_batch(&mut g, &batch).unwrap();
            batch.clear();
        }
    }
    query::execute_batch(&mut g, &batch).unwrap();
    g.checkpoint().unwrap();
}

const READS: [&str; 6] = [
    "MATCH (n:Ctx) RETURN n._origin, max(n._seq)",
    "MATCH (a:Ctx)-[r]->(b:Ctx) RETURN r._origin, max(r._seq)",
    r#"MATCH (n:Ctx) WHERE n._origin = "a@host" AND n._seq > -1 RETURN count(n)"#,
    r#"MATCH (a:Ctx)-[r]->(b:Ctx) WHERE r._origin = "a@host" AND r._seq > -1 RETURN count(r)"#,
    r#"MATCH (n:Ctx) WHERE n._origin = "a@host" AND n._seq > 3 RETURN n._seq, n, n._key ORDER BY n._seq, n._key LIMIT 1000"#,
    "MATCH (n:Thing) RETURN n.i",
];

/// Peak native bytes of each catch-up read and of a base copy, over a graph
/// of `nodes` nodes, and the database file's size.
fn serve(dir: &Path, nodes: u64) -> (Vec<u64>, u64) {
    let path = dir.join(format!("g{nodes}.gldb"));
    load(&path, nodes);
    let mut g = open(&path, CACHE);
    let limits = QueryLimits {
        max_rows: usize::MAX,
        max_bytes: 4 * MIB,
        ..Default::default()
    };
    let mut peaks = Vec::new();
    for q in READS.iter().take(5) {
        let (peak, r) = peak_during(|| query::execute_capped(&mut g, q, &[], limits));
        let rows = r.unwrap().rows;
        assert!(!rows.is_empty(), "{q}: no rows");
        peaks.push(peak);
    }
    // a whole label through a disk-spooled cursor
    let (peak, n) = peak_during(|| {
        let mut c = Cursor::open(
            &mut g,
            READS[5],
            &[],
            QueryLimits {
                max_rows: 1000,
                max_bytes: MIB,
                ..Default::default()
            },
        )
        .unwrap();
        let mut n = 0;
        loop {
            let (batch, done) = c.next_batch().unwrap();
            n += batch.rows.len() as u64;
            if done {
                break n;
            }
        }
    });
    assert_eq!(n, nodes);
    peaks.push(peak);
    // a base copy to a directory, as a peer takes one for a catch-up
    let out = dir.join(format!("base{nodes}"));
    let rc = ReplicaConfig {
        url: format!("file://{}", out.display()),
        min_bytes: 0,
        ..Default::default()
    };
    let r = PagedReplicator::new(
        &path,
        dir.join(format!("spool{nodes}")),
        open_backend(&rc).unwrap(),
        rc,
    );
    let (peak, report) = peak_during(|| {
        std::thread::scope(|s| {
            let copy = s.spawn(|| r.sync(true, false));
            while !copy.is_finished() {
                g.poll_replication().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            copy.join().unwrap().unwrap()
        })
    });
    assert_eq!(report.bases_uploaded, 1);
    peaks.push(peak);
    let file = std::fs::metadata(&path).unwrap().len();
    drop(g);
    (peaks, file)
}

#[test]
fn whole_graph_reads_and_base_copies_cost_the_cache_not_the_graph_and_free_memory_goes_back() {
    let limit = 256 * MIB;
    budget::configure(MemoryBudget {
        limit,
        budget: limit / 5 * 4,
        cache: 32 * MIB,
        work: 2 * WORK_MEM,
        beam: limit / 5 * 4 - 32 * MIB - 2 * WORK_MEM,
    });
    let dir: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("native-memory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 1. The catch-up reads and a base copy, at two sizes ten times apart.
    let (small, _) = serve(&dir, 20_000);
    let (large, file) = serve(&dir, 200_000);
    let names = [
        "watermarks",
        "edge watermarks",
        "count",
        "edge count",
        "page",
        "cursor",
        "base copy",
    ];
    // the cache, a copy buffer and its writer, and a few MiB of statement
    let bound = CACHE + 8 * MIB;
    assert!(
        file > bound * 2,
        "the database ({file} bytes) must dwarf the bound"
    );
    for (i, name) in names.iter().enumerate() {
        println!(
            "native-memory {name:<16} peak {:>6} KiB at 20k nodes, {:>6} KiB at 200k ({} KiB file)",
            small[i] >> 10,
            large[i] >> 10,
            file >> 10
        );
        assert!(
            large[i] < bound,
            "{name}: {} KiB at 200k nodes",
            large[i] >> 10
        );
        assert!(
            large[i] < small[i] + 2 * MIB,
            "{name} grows with the graph: {} KiB at 20k nodes, {} KiB at 200k",
            small[i] >> 10,
            large[i] >> 10
        );
    }

    // 2. A scan that cycles the cache from another thread reuses the
    //    cache's memory. The frames this thread read in live in its malloc
    //    arena; a scan on another thread evicts them all. Freed there and
    //    allocated anew in the scanning thread's arena, the cache would be
    //    held twice until something trimmed it (query::execute does not).
    if budget::native().is_some() {
        let path = dir.join("g200000.gldb");
        let g = std::sync::Mutex::new(open(&path, 8 * MIB));
        query::execute(&mut g.lock().unwrap(), READS[0]).unwrap();
        budget::release();
        let before = status("RssAnon");
        std::thread::scope(|s| {
            let g = &g;
            s.spawn(move || {
                query::execute(&mut g.lock().unwrap(), READS[1]).unwrap();
                query::execute(&mut g.lock().unwrap(), READS[0]).unwrap();
            });
        });
        let after = status("RssAnon");
        println!(
            "native-memory cross-thread scan: RssAnon {} -> {} KiB with an 8 MiB cache",
            before >> 10,
            after >> 10
        );
        assert!(
            after < before + 3 * MIB,
            "a scan from another thread held {} KiB more",
            after.saturating_sub(before) >> 10
        );
    }

    // 3. Memory freed goes back to the OS. The cache grows a MiB at a time
    //    from several threads (as dirty schedulers fill it), with something
    //    small and long-lived allocated between the steps (as everything
    //    else a process does is), then is shrunk, as the governor shrinks it
    //    under pressure. glibc can give back only the top of a heap by
    //    itself; what the cache gave up lies under those small blocks.
    if budget::native().is_none() {
        return; // not glibc: nothing to ask
    }
    let path = dir.join("g200000.gldb");
    let g = std::sync::Mutex::new(open(&path, 32 * MIB));
    let mut keep = Vec::new();
    glider::storage::pager::resize_shared_cache(MIB);
    for step in 2..=28u64 {
        glider::storage::pager::resize_shared_cache(step * MIB);
        // the small block allocated by the thread that grew the cache: in
        // its arena, after the frames
        let kept = std::thread::scope(|s| {
            let g = &g;
            s.spawn(move || {
                let mut g = g.lock().unwrap();
                query::execute(&mut g, READS[(step % 2) as usize]).unwrap();
                vec![step as u8; 200]
            })
            .join()
            .unwrap()
        });
        keep.push(kept);
    }
    let full = budget::process().unwrap().cache.used();
    assert!(
        full > 20 * MIB,
        "the scans filled {} KiB of cache",
        full >> 10
    );
    let rss = status("RssAnon");
    let released = budget::released();
    glider::storage::pager::resize_shared_cache(MIB);
    let freed = full - budget::process().unwrap().cache.used();
    let after = status("RssAnon");
    println!(
        "native-memory shrink: cache {} -> {} KiB, RssAnon {} -> {} KiB, {} KiB released",
        full >> 10,
        (full - freed) >> 10,
        rss >> 10,
        after >> 10,
        (budget::released() - released) >> 10
    );
    assert!(
        rss.saturating_sub(after) > freed / 2,
        "RssAnon went from {} to {} KiB after {} KiB of cache was given up",
        rss >> 10,
        after >> 10,
        freed >> 10
    );
    drop(keep);

    // 4. And where glibc would keep it: 32 MiB of page-sized blocks freed
    //    from between small blocks that stay, in one thread's arena. Free,
    //    it stays resident; released, it goes.
    std::thread::spawn(|| {
        let mut pages = Vec::new();
        let mut keep = Vec::new();
        let base = status("RssAnon");
        for i in 0..2048u32 {
            pages.push(vec![1u8; 16 << 10]);
            keep.push(vec![i as u8; 64]);
        }
        let full = status("RssAnon");
        drop(pages);
        let freed = status("RssAnon");
        budget::release();
        let released = status("RssAnon");
        println!(
            "native-memory release: RssAnon {} KiB full, {} KiB freed, {} KiB released",
            full >> 10,
            freed >> 10,
            released >> 10
        );
        // glibc gives some back by itself (whole free stretches at the top
        // of a heap), never all of it
        assert!(
            freed > base + 8 * MIB,
            "glibc gave the blocks back by itself: this proves nothing"
        );
        assert!(
            freed.saturating_sub(released) > 8 * MIB,
            "release gave back only {} KiB",
            freed.saturating_sub(released) >> 10
        );
        drop(keep);
    })
    .join()
    .unwrap();
    drop(g);
    let _ = std::fs::remove_dir_all(&dir);
}
