//! mfg-bench: the baseline over a manufacturing graph from `mfg-gen`.
//!
//!   mfg-bench --data DIR [--tiers memory,file] [--budgets 250,1000,4000]
//!             [--only stats,retrieve,...] [--out FILE] [--label NAME] [--detail]
//!
//! Loads DIR/graph.jsonl into glider (in memory, and as a file in DIR), then
//! runs each measurement in [`MEASURES`] and appends one JSON object per
//! line to FILE (default `bench/results/mfg.jsonl`). Every line carries
//! `bench`, `label`, `dataset`, `seed`, `nodes_target`, `tier` and
//! `measure`; the rest depends on the measurement. The lines also go to
//! stderr.
//!
//! A later slice adds a measurement by writing a `fn(&mut Run) -> Result`
//! and listing it in [`MEASURES`]; it gets the loaded graph, the questions
//! and `emit`. Formats and baseline numbers: bench/KNOWLEDGE.md.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions as FileOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use glider::graph::{Graph, OpenOptions};
use glider::query::{self, QueryResult};
use glider::value::{parse_json, Json, Value};
use glider_bench::mfg::Obj;

/// The measurements, in the order they run. Each sees the graph as the
/// previous one left it (`retrieve` installs the `context` extension and
/// its index).
const MEASURES: &[(&str, Measure)] = &[
    ("stats", stats),
    ("retrieve", retrieve),
    ("pagerank", pagerank),
    ("communities", communities),
];

type Measure = fn(&mut Run) -> Result<(), String>;

/// One question from questions.jsonl.
pub struct Question {
    pub id: String,
    pub kind: String,
    pub query: String,
    pub anchors: Vec<u64>,
    pub expected: BTreeSet<u64>,
}

/// What a measurement works with.
pub struct Run<'a> {
    pub g: Graph,
    pub tier: &'static str,
    pub questions: &'a [Question],
    pub budgets: Vec<usize>,
    pub detail: bool,
    meta: &'a Meta,
    out: &'a mut dyn Write,
}

struct Meta {
    label: String,
    dataset: String,
    seed: u64,
    nodes_target: u64,
}

impl Run<'_> {
    /// Write one result line: the common fields, then `fields`.
    pub fn emit(&mut self, measure: &str, fields: Obj) {
        let line = Obj::new()
            .s("bench", "mfg")
            .s("label", &self.meta.label)
            .s("dataset", &self.meta.dataset)
            .u("seed", self.meta.seed)
            .u("nodes_target", self.meta.nodes_target)
            .s("tier", self.tier)
            .s("measure", measure)
            .extend(fields)
            .done();
        writeln!(self.out, "{line}").expect("writing results");
    }

    /// Run a statement, timed in milliseconds.
    pub fn run(
        &mut self,
        src: &str,
        params: &[(String, Value)],
    ) -> Result<(QueryResult, f64), String> {
        let t = Instant::now();
        let r = query::execute_with(&mut self.g, src, params).map_err(|e| format!("{src}: {e}"))?;
        Ok((r, t.elapsed().as_secs_f64() * 1e3))
    }
}

fn arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1).cloned())
}

fn peak_rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| {
                l.split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok())
            })
        })
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

fn ids(j: &Json, k: &str) -> Vec<u64> {
    j.get(k)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
        .unwrap_or_default()
}

fn read_questions(path: &Path) -> Vec<Question> {
    let f = File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    BufReader::new(f)
        .lines()
        .map(|l| {
            let j = parse_json(&l.expect("reading questions")).expect("a question line");
            let s = |k: &str| j.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
            Question {
                id: s("id"),
                kind: s("kind"),
                query: s("query"),
                anchors: ids(&j, "anchors"),
                expected: ids(&j, "ids").into_iter().collect(),
            }
        })
        .collect()
}

fn read_meta(path: &Path, label: String) -> Meta {
    let f = File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let first = BufReader::new(f)
        .lines()
        .next()
        .expect("a truth file")
        .expect("reading truth");
    let j = parse_json(&first).expect("a truth line");
    let u = |k: &str| j.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let dataset = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Meta {
        label,
        dataset,
        seed: u("seed"),
        nodes_target: u("nodes_target"),
    }
}

/// Files of the database at `path`: the file itself and anything beside it
/// whose name starts with its name (log, segments, spill directory).
fn db_files(path: &Path) -> Vec<PathBuf> {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let dir = path.parent().unwrap();
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with(&name))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn bytes_on_disk(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = db_files(path);
    while let Some(p) = stack.pop() {
        match std::fs::metadata(&p) {
            Ok(m) if m.is_dir() => {
                if let Ok(rd) = std::fs::read_dir(&p) {
                    stack.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()));
                }
            }
            Ok(m) => total += m.len(),
            Err(_) => {}
        }
    }
    total
}

fn remove_db(path: &Path) {
    for f in db_files(path) {
        let _ = std::fs::remove_dir_all(&f);
        let _ = std::fs::remove_file(&f);
    }
}

fn load(tier: &'static str, data: &Path) -> Result<(Graph, Obj, Option<PathBuf>), String> {
    let (mut g, path) = match tier {
        "memory" => (Graph::memory(), None),
        _ => {
            let p = data.join("mfg-bench.gldb");
            remove_db(&p);
            let g = Graph::open_opts(&p, OpenOptions::default()).map_err(|e| e.to_string())?;
            (g, Some(p))
        }
    };
    let src = data.join("graph.jsonl");
    let jsonl_bytes = std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
    let mut r = BufReader::with_capacity(
        1 << 20,
        File::open(&src).map_err(|e| format!("{}: {e}", src.display()))?,
    );
    let t = Instant::now();
    let (n, e) = query::import_jsonl_from(&mut g, &mut r, 1 << 20).map_err(|e| e.to_string())?;
    let import_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    g.checkpoint().map_err(|e| e.to_string())?;
    let checkpoint_ms = t.elapsed().as_secs_f64() * 1e3;
    let mut fields = Obj::new()
        .u("nodes", n as u64)
        .u("edges", e as u64)
        .u("jsonl_bytes", jsonl_bytes)
        .i("import_ms", import_ms as i64)
        .i("checkpoint_ms", checkpoint_ms as i64)
        .u("bytes", g.file_len());
    if let Some(p) = &path {
        fields = fields.u("bytes_on_disk", bytes_on_disk(p));
    }
    fields = fields.u("rss_peak_mb", peak_rss_mb());
    Ok((g, fields, path))
}

// ------------------------------------------------------------ measurements

fn stats(run: &mut Run) -> Result<(), String> {
    let (r, ms) = run.run("STATS", &[])?;
    let mut o = Obj::new();
    for row in &r.rows {
        let (Some(Value::Text(k)), Some(v)) = (row.first(), row.get(1)) else {
            continue;
        };
        o = match v {
            Value::Int(i) => o.i(k, *i),
            Value::Text(s) => o.s(k, s),
            v => o.s(k, &v.to_string()),
        };
    }
    run.emit(
        "stats",
        Obj::new().i("ms", ms as i64).raw("stats", &o.done()),
    );
    Ok(())
}

/// Node ids of the cards in a `context.retrieve` text answer: lines that
/// begin with a handle and a colon (`n12:Label ...`).
fn card_ids(text: &str) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix('n') else {
            continue;
        };
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && rest[digits.len()..].starts_with(':') {
            if let Ok(id) = digits.parse() {
                out.insert(id);
            }
        }
    }
    out
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p).round() as usize]
}

#[derive(Default)]
struct Tally {
    n: usize,
    recall: f64,
    precision: f64,
    tokens: f64,
    shown: f64,
    ms: Vec<f64>,
}

/// What `context.retrieve` returns for each question, by kind and budget,
/// from the question's anchors (`seeds`) and from its words (`query`).
/// Recall: expected ids among the cards shown. Precision: expected ids
/// among the cards shown that are not anchors (0 when there are none).
fn retrieve(run: &mut Run) -> Result<(), String> {
    if run.g.extension_version("context").is_none() {
        run.run("CREATE EXTENSION context", &[])?;
    }
    let (r, ms) = run.run("CALL context.index()", &[])?;
    run.emit(
        "context.index",
        Obj::new()
            .i("ms", ms as i64)
            .s("message", r.message.as_deref().unwrap_or("")),
    );
    let mut kinds: Vec<String> = run.questions.iter().map(|q| q.kind.clone()).collect();
    kinds.sort();
    kinds.dedup();
    let questions = run.questions;
    for mode in ["seeds", "query"] {
        for budget in run.budgets.clone() {
            let mut tallies: Vec<(String, Tally)> = kinds
                .iter()
                .map(|k| (k.clone(), Tally::default()))
                .chain(std::iter::once(("all".to_string(), Tally::default())))
                .collect();
            for q in questions {
                if q.expected.is_empty() {
                    continue;
                }
                let b = ("b".to_string(), Value::Int(budget as i64));
                let (src, params) = if mode == "seeds" {
                    let seeds = q.anchors.iter().map(|&a| Value::Int(a as i64)).collect();
                    (
                        "CALL context.retrieve(seeds: $s, budget: $b)",
                        vec![("s".to_string(), Value::List(seeds)), b],
                    )
                } else {
                    (
                        "CALL context.retrieve(query: $q, budget: $b)",
                        vec![("q".to_string(), Value::Text(q.query.clone())), b],
                    )
                };
                let (r, ms) = run.run(src, &params)?;
                let row = r.rows.first();
                let text = match row.and_then(|row| row.first()) {
                    Some(Value::Text(t)) => t.clone(),
                    _ => String::new(),
                };
                let tokens = row
                    .and_then(|row| row.get(3))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as f64;
                let shown = card_ids(&text);
                let anchors: BTreeSet<u64> = q.anchors.iter().copied().collect();
                let hit = q.expected.iter().filter(|i| shown.contains(i)).count();
                let others = shown.iter().filter(|i| !anchors.contains(i)).count();
                let others_hit = q
                    .expected
                    .iter()
                    .filter(|i| shown.contains(i) && !anchors.contains(i))
                    .count();
                let recall = hit as f64 / q.expected.len() as f64;
                let precision = if others == 0 {
                    0.0
                } else {
                    others_hit as f64 / others as f64
                };
                if run.detail {
                    run.emit(
                        "retrieve.question",
                        Obj::new()
                            .s("mode", mode)
                            .u("budget", budget as u64)
                            .s("question", &q.id)
                            .s("kind", &q.kind)
                            .u("expected", q.expected.len() as u64)
                            .u("shown", shown.len() as u64)
                            .u("hit", hit as u64)
                            .i("tokens", tokens as i64)
                            .raw("ms", &format!("{ms:.3}")),
                    );
                }
                for (k, t) in tallies.iter_mut() {
                    if k == &q.kind || k == "all" {
                        t.n += 1;
                        t.recall += recall;
                        t.precision += precision;
                        t.tokens += tokens;
                        t.shown += shown.len() as f64;
                        t.ms.push(ms);
                    }
                }
            }
            for (k, mut t) in tallies {
                if t.n == 0 {
                    continue;
                }
                let n = t.n as f64;
                run.emit(
                    "retrieve",
                    Obj::new()
                        .s("mode", mode)
                        .u("budget", budget as u64)
                        .s("kind", &k)
                        .u("questions", t.n as u64)
                        .raw("recall", &format!("{:.3}", t.recall / n))
                        .raw("precision", &format!("{:.3}", t.precision / n))
                        .raw("tokens", &format!("{:.0}", t.tokens / n))
                        .raw("shown", &format!("{:.1}", t.shown / n))
                        .raw("ms_p50", &format!("{:.3}", pct(&mut t.ms, 0.5)))
                        .raw("ms_max", &format!("{:.3}", pct(&mut t.ms, 1.0))),
                );
            }
        }
    }
    Ok(())
}

fn algorithm(run: &mut Run, name: &str) -> Result<(), String> {
    let (r, ms) = run.run(&format!("CALL {name}(top: 10)"), &[])?;
    run.emit(
        name,
        Obj::new()
            .i("ms", ms as i64)
            .s("message", r.message.as_deref().unwrap_or("")),
    );
    Ok(())
}

fn pagerank(run: &mut Run) -> Result<(), String> {
    algorithm(run, "pagerank")
}

fn communities(run: &mut Run) -> Result<(), String> {
    algorithm(run, "communities")
}

// -------------------------------------------------------------------- main

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") || arg(&args, "--data").is_none() {
        eprintln!(
            "usage: mfg-bench --data DIR [--tiers memory,file] [--budgets 250,1000,4000] \
             [--only {}] [--out FILE] [--label NAME] [--detail]",
            MEASURES.iter().map(|m| m.0).collect::<Vec<_>>().join(",")
        );
        std::process::exit(2);
    }
    let data = PathBuf::from(arg(&args, "--data").unwrap());
    let tiers: Vec<String> = arg(&args, "--tiers")
        .unwrap_or_else(|| "memory,file".into())
        .split(',')
        .map(String::from)
        .collect();
    let budgets: Vec<usize> = arg(&args, "--budgets")
        .unwrap_or_else(|| "250,1000,4000".into())
        .split(',')
        .map(|b| b.parse().expect("--budgets: a list of numbers"))
        .collect();
    let only: Option<Vec<String>> =
        arg(&args, "--only").map(|s| s.split(',').map(String::from).collect());
    let out_path = arg(&args, "--out").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("results")
            .join("mfg.jsonl")
    });
    let label = arg(&args, "--label").unwrap_or_else(|| "baseline".into());
    let detail = args.iter().any(|a| a == "--detail");

    let questions = read_questions(&data.join("questions.jsonl"));
    let meta = read_meta(&data.join("truth.jsonl"), label);
    let mut buf: Vec<u8> = Vec::new();
    for tier in &tiers {
        let tier: &'static str = match tier.as_str() {
            "memory" => "memory",
            "file" => "file",
            other => panic!("unknown tier {other}: use memory or file"),
        };
        let (g, fields, path) = load(tier, &data).unwrap_or_else(|e| panic!("load: {e}"));
        let mut run = Run {
            g,
            tier,
            questions: &questions,
            budgets: budgets.clone(),
            detail,
            meta: &meta,
            out: &mut buf,
        };
        run.emit("load", fields);
        for (name, m) in MEASURES {
            if only.as_ref().is_some_and(|o| !o.iter().any(|x| x == name)) {
                continue;
            }
            m(&mut run).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        drop(run);
        if let Some(p) = path {
            remove_db(&p);
        }
    }
    let text = String::from_utf8(buf).unwrap();
    eprint!("{text}");
    if let Some(dir) = out_path.parent() {
        std::fs::create_dir_all(dir).expect("results directory");
    }
    let mut f = FileOptions::new()
        .create(true)
        .append(true)
        .open(&out_path)
        .unwrap_or_else(|e| panic!("{}: {e}", out_path.display()));
    f.write_all(text.as_bytes()).expect("writing results");
    eprintln!(
        "appended {} lines to {}",
        text.lines().count(),
        out_path.display()
    );
}
