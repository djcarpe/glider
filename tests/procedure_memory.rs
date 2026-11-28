//! A procedure stopped by its statement budget gives back what it had
//! taken: the memory it allocated, and the spill files it made.
//!
//! One test in its own binary, so that the allocator counts only it.
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use glider::graph::{Graph, OpenOptions};
use glider::query::{self, QueryLimits};
use glider::Value;

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
}
#[global_allocator]
static A: Counting = Counting;

fn build(g: &mut Graph, n: u64) {
    g.autocommit = false;
    let ids: Vec<u64> = (0..n)
        .map(|i| {
            g.add_node(&["N".into()], vec![("i".into(), Value::Int(i as i64))])
                .unwrap()
        })
        .collect();
    for i in 0..n as usize {
        for k in 1..=4 {
            g.add_edge(ids[i], ids[(i * 7 + k * 13) % n as usize], "E", vec![])
                .unwrap();
        }
    }
    g.commit().unwrap();
    g.autocommit = true;
}

fn stopped(g: &mut Graph, src: &str) {
    let limits = QueryLimits {
        max_millis: Some(50),
        ..Default::default()
    };
    let err = query::execute_capped(g, src, &[], limits)
        .err()
        .expect("stopped");
    assert!(err.to_string().contains("time budget"), "{err}");
}

fn files_in(dir: &PathBuf) -> usize {
    std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
}

#[test]
fn a_stopped_procedure_gives_back_its_memory_and_its_spill_files() {
    const LONG: &str = "CALL pagerank(iterations: 1000000, tolerance: 0)";
    // in memory: the projection and the per-node arrays
    let mut g = Graph::memory();
    build(&mut g, 50_000);
    stopped(&mut g, LONG); // first use: whatever is set up once
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    stopped(&mut g, LONG);
    let held = PEAK.load(Ordering::Relaxed) - before;
    let after = LIVE.load(Ordering::Relaxed);
    assert!(held > 4 << 20, "it held {held} bytes while running");
    assert!(
        after <= before + (64 << 10),
        "{} bytes not given back",
        after.saturating_sub(before)
    );
    drop(g);

    // on a file with 64 KiB of work_mem: the arrays spill, and go
    let mut path = std::env::temp_dir();
    path.push(format!("glider-procmem-{}.db", std::process::id()));
    let opts = OpenOptions {
        work_mem: 64 << 10,
        ..Default::default()
    };
    let mut g = Graph::open_opts(&path, opts).unwrap();
    build(&mut g, 20_000);
    let tmp = g.temp_dir();
    stopped(
        &mut g,
        "CALL pagerank(iterations: 1000000, tolerance: 0, tier: \"ooc\")",
    );
    stopped(&mut g, "CALL betweenness(tier: \"ooc\")");
    assert_eq!(files_in(&tmp), 0, "spill files left in {}", tmp.display());
    let r = query::execute(&mut g, "MATCH (n:N) RETURN count(n)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(20_000));
    drop(g);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(glider::store::lock_path(&path));
    for suffix in ["-wal", "-data", "-tmp"] {
        let mut d = path.as_os_str().to_os_string();
        d.push(suffix);
        let _ = std::fs::remove_dir_all(PathBuf::from(d));
    }
}
