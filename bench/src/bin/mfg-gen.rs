//! mfg-gen: a manufacturing graph whose truth is known.
//!
//!   mfg-gen --nodes 100k [--seed 1] [--faults] [--out DIR]
//!
//! Writes `graph.jsonl` (for `glider <db> import`), `truth.jsonl` and
//! `questions.jsonl` into DIR, by default
//! `$GLIDER_BENCH_DATA/mfg/n<nodes>-s<seed>[-faults]` (or
//! `../glider-bench-data/...` beside the repository). The same arguments
//! give the same bytes on every run and machine. Formats: bench/KNOWLEDGE.md.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

use glider_bench::mfg::{generate, Config};

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1).cloned())
}

/// `250`, `10k`, `2m`.
fn count(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mul) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1_000),
        'm' => (&s[..s.len() - 1], 1_000_000),
        _ => (&s[..], 1),
    };
    num.parse::<u64>().ok().map(|n| n * mul)
}

pub fn data_dir() -> PathBuf {
    match std::env::var_os("GLIDER_BENCH_DATA") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("glider-bench-data"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("usage: mfg-gen --nodes N [--seed S] [--faults] [--out DIR]");
        return;
    }
    let nodes = arg(&args, "--nodes")
        .map(|v| count(&v).unwrap_or_else(|| panic!("--nodes: not a count: {v}")))
        .unwrap_or(10_000);
    let seed: u64 = arg(&args, "--seed")
        .map(|v| v.parse().expect("--seed: a number"))
        .unwrap_or(1);
    let faults = args.iter().any(|a| a == "--faults");
    let out = arg(&args, "--out").map(PathBuf::from).unwrap_or_else(|| {
        data_dir().join("mfg").join(format!(
            "n{nodes}-s{seed}{}",
            if faults { "-faults" } else { "" }
        ))
    });
    std::fs::create_dir_all(&out).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
    let open = |name: &str| {
        let p = out.join(name);
        BufWriter::with_capacity(
            1 << 20,
            File::create(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())),
        )
    };
    let (mut g, mut t, mut q) = (
        open("graph.jsonl"),
        open("truth.jsonl"),
        open("questions.jsonl"),
    );
    let start = Instant::now();
    let cfg = Config {
        seed,
        nodes,
        faults,
    };
    let s = generate(&cfg, &mut g, &mut t, &mut q).expect("writing");
    for w in [&mut g, &mut t, &mut q] {
        w.flush().expect("flush");
    }
    eprintln!(
        "{}: {} nodes, {} edges, {} stations on {} lines, {} designs, {} truth records, {} questions in {:.2}s",
        out.display(),
        s.nodes,
        s.edges,
        s.stations,
        s.lines,
        s.designs,
        s.truth_records,
        s.questions,
        start.elapsed().as_secs_f64()
    );
}
