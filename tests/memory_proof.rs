//! The native memory proof of engineering/graph-memory-budget.
//!
//! Bulk-loads a graph several times larger than its page cache, with a
//! 64 MiB cache and 32 MiB work_mem inside a 256 MiB cgroup's budget, then checks
//! the process's peak RSS (VmHWM) after every step against a cap of half that
//! cgroup: the load, a reopen, label counts, a cursor over every node,
//! ORDER BY … LIMIT, a refused whole-graph sort and collect, and a streaming
//! export to a file. The graph on disk and the export must each be more than
//! twice the cap. The one test in this file runs alone in its own process, so
//! VmHWM is this workload's peak.
//!
//! Sizes scale with the environment; the defaults take about 1.5 minutes in a
//! debug build:
//!
//!     GLIDER_PROOF_NODES=4000000 GLIDER_PROOF_CAP_MIB=128 \
//!       cargo test --release --test memory_proof -- --nocapture
//!
//! The same binary also runs under a real cgroup limit:
//!
//!     systemd-run --user --wait --pty -p MemoryMax=128M -p MemorySwapMax=0 \
//!       target/debug/deps/memory_proof-<hash> --nocapture
#![cfg(target_os = "linux")]

use glider::budget::{self, MemoryBudget};
use glider::query::{self, Cursor, QueryLimits};
use glider::{Graph, OpenOptions, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

const MIB: u64 = 1 << 20;
const CACHE: u64 = 64 * MIB;
const WORK_MEM: u64 = 32 * MIB;

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A `/proc/self/status` field in bytes (VmHWM, VmRSS).
fn status(field: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix(field)?.strip_prefix(':'))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or_else(|| panic!("no {field} in /proc/self/status"))
}

fn disk_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let meta = entry.metadata().unwrap();
        total += if meta.is_dir() {
            disk_bytes(&entry.path())
        } else {
            meta.len()
        };
    }
    total
}

struct Proof {
    cap: u64,
    started: Instant,
}

impl Proof {
    /// Peak RSS so far, which must stay under the cap after every step.
    fn step(&mut self, name: &str, detail: String) {
        let hwm = status("VmHWM");
        println!(
            "memory-proof {name:<16} VmHWM {:>4} MiB  VmRSS {:>4} MiB  {:>6.1}s  {detail}",
            hwm / MIB,
            status("VmRSS") / MIB,
            self.started.elapsed().as_secs_f64(),
        );
        assert!(
            hwm < self.cap,
            "{name}: peak RSS {} MiB is over the {} MiB cap",
            hwm / MIB,
            self.cap / MIB
        );
        self.started = Instant::now();
    }
}

fn open(path: &Path) -> Graph {
    Graph::open_opts(
        path,
        OpenOptions {
            cache_size: CACHE,
            work_mem: WORK_MEM,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn peak_rss_stays_under_the_cap_on_a_graph_larger_than_memory() {
    let nodes = env("GLIDER_PROOF_NODES", 400_000);
    let edges = nodes * 2;
    let cap = env("GLIDER_PROOF_CAP_MIB", 128) * MIB;
    // The cache and query workspace of a 256 MiB cgroup, pinned so the proof
    // does not depend on the machine running it.
    let limit = 256 * MIB;
    let b = budget::configure(MemoryBudget {
        limit,
        budget: limit / 5 * 4,
        cache: CACHE,
        work: 2 * WORK_MEM,
        beam: limit / 5 * 4 - CACHE - 2 * WORK_MEM,
    });
    assert_eq!(
        b.cache.limit(),
        CACHE,
        "the process budget was configured first"
    );

    let dir: PathBuf =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("memory-proof-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("graph.gldb");
    let mut proof = Proof {
        cap,
        started: Instant::now(),
    };
    proof.step("start", format!("cap {} MiB", cap / MIB));

    // Load: 512-byte payloads, committed in slices as a sync stream would.
    {
        let mut g = open(&path);
        let payload = "p".repeat(512);
        g.autocommit = false;
        for i in 0..nodes {
            let label = if i % 4 == 0 { "Hub" } else { "Item" };
            g.add_node(
                &[label.into()],
                vec![
                    ("i".into(), Value::Int(i as i64)),
                    ("score".into(), Value::Int(((i * 7919) % 1_000_003) as i64)),
                    ("payload".into(), Value::Text(payload.clone())),
                ],
            )
            .unwrap();
            if i % 20_000 == 19_999 {
                g.commit().unwrap();
            }
        }
        g.commit().unwrap();
        proof.step("load nodes", format!("{nodes} nodes"));
        // Edges arrive in node order with nearby targets, as a sync stream of
        // an org graph would. Uniformly random endpoints make every edge two
        // cache misses, and the load, not the bound, the slow part.
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for e in 0..edges {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let from = 1 + e / 2;
            let to = 1 + (from + x % 4096) % nodes;
            g.add_edge(
                from,
                to,
                "LINKS",
                vec![("w".into(), Value::Int((e % 10) as i64))],
            )
            .unwrap();
            if e % 20_000 == 19_999 {
                g.commit().unwrap();
            }
        }
        g.commit().unwrap();
        g.autocommit = true;
        g.checkpoint().unwrap();
    }
    let on_disk = disk_bytes(&dir);
    proof.step(
        "load edges",
        format!(
            "{nodes} nodes, {edges} edges, {} MiB on disk",
            on_disk / MIB
        ),
    );
    assert!(
        on_disk > 2 * cap,
        "the graph ({} MiB) must be larger than the memory cap",
        on_disk / MIB
    );

    let mut g = open(&path);
    proof.step("open", format!("{} nodes", g.node_count()));

    let r = query::execute(&mut g, "MATCH (n:Item) RETURN count(n)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int((nodes - nodes.div_ceil(4)) as i64));
    // A filter defeats the counters: every node page goes through the cache.
    let r = query::execute(
        &mut g,
        "MATCH (n:Item) WHERE n.score > 500000 RETURN count(n)",
    )
    .unwrap();
    let expected = (0..nodes)
        .filter(|i| i % 4 != 0 && (i * 7919) % 1_000_003 > 500_000)
        .count();
    assert_eq!(r.rows[0][0], Value::Int(expected as i64));
    proof.step("label count", format!("{expected} filtered"));

    // Every node, in capped batches from a disk spool.
    let limits = QueryLimits {
        max_rows: 1000,
        max_bytes: MIB,
    };
    let mut cursor = Cursor::open(&mut g, "MATCH (n) RETURN n", &[], limits).unwrap();
    let (mut rows, mut batches) = (0u64, 0u64);
    loop {
        let (batch, done) = cursor.next_batch().unwrap();
        assert!(batch.rows.len() <= limits.max_rows);
        rows += batch.rows.len() as u64;
        batches += 1;
        if done {
            break;
        }
    }
    drop(cursor);
    assert_eq!(rows, nodes);
    proof.step(
        "cursor RETURN n",
        format!("{rows} rows in {batches} batches"),
    );

    let r = query::execute(
        &mut g,
        "MATCH (n:Item) RETURN n.i, n.score ORDER BY n.score DESC LIMIT 10",
    )
    .unwrap();
    assert_eq!(r.rows.len(), 10);
    let top = (0..nodes)
        .filter(|i| i % 4 != 0)
        .map(|i| ((i * 7919) % 1_000_003) as i64)
        .max()
        .unwrap();
    assert_eq!(r.rows[0][1], Value::Int(top));
    proof.step("ORDER BY LIMIT", format!("top score {top}"));

    // Materializing everything is refused, not grown into.
    for q in [
        "MATCH (n) RETURN n.payload ORDER BY n.score",
        "MATCH (n) RETURN collect(n.payload)",
    ] {
        match query::execute(&mut g, q) {
            Ok(r) => panic!("{q}: materialized {} rows past work_mem", r.rows.len()),
            Err(e) => assert!(e.to_string().contains("work_mem"), "{q}: {e}"),
        }
    }
    proof.step("refused", "unbounded sort and collect".into());

    let export = dir.join("export.jsonl");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&export).unwrap());
    query::export_jsonl_to(&g, &mut out).unwrap();
    out.flush().unwrap();
    drop(out);
    let exported = std::fs::metadata(&export).unwrap().len();
    assert!(exported > 2 * cap);
    proof.step("export", format!("{} MiB of JSONL", exported / MIB));

    drop(g);
    std::fs::remove_dir_all(&dir).unwrap();
    println!(
        "memory-proof peak RSS {} MiB under a {} MiB cap (cache {} MiB, work_mem {} MiB)",
        status("VmHWM") / MIB,
        cap / MIB,
        CACHE / MIB,
        WORK_MEM / MIB
    );
}
