//! mfg-bench: the baseline over a manufacturing graph from `mfg-gen`.
//!
//!   mfg-bench --data DIR [--tiers memory,file] [--budgets 250,1000,4000]
//!             [--only stats,retrieve,check,structure,changes,...] [--out FILE] [--label NAME] [--detail]
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
use glider_bench::checks::{self, Planted, Score};
use glider_bench::mfg::Obj;
use glider_bench::structure::{self as shapes, Station};

/// The measurements, in the order they run. Each sees the graph as the
/// previous one left it (`retrieve` installs the `context` extension and
/// its index).
const MEASURES: &[(&str, Measure)] = &[
    ("stats", stats),
    ("retrieve", retrieve),
    ("pagerank", pagerank),
    ("communities", communities),
    ("check", check),
    ("structure", structure),
    ("changes", changes),
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
    /// What the generator planted, from truth.jsonl.
    pub planted: &'a Planted,
    /// Every station's design and whether it deviates, from truth.jsonl.
    pub stations: &'a [Station],
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

/// The planted facts of truth.jsonl, reading only the lines about them.
fn read_planted(path: &Path) -> Planted {
    let f = File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let lines: Vec<Json> = BufReader::new(f)
        .lines()
        .map(|l| l.expect("reading truth"))
        .filter(|l| {
            l.starts_with("{\"kind\":\"fault\"")
                || l.starts_with("{\"kind\":\"duplicate\"")
                || l.starts_with("{\"kind\":\"lookalike\"")
        })
        .map(|l| parse_json(&l).expect("a truth line"))
        .collect();
    checks::planted(&lines)
}

/// The stations of truth.jsonl.
fn read_stations(path: &Path) -> Vec<Station> {
    let f = File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let lines: Vec<Json> = BufReader::new(f)
        .lines()
        .map(|l| l.expect("reading truth"))
        .filter(|l| l.starts_with("{\"kind\":\"station\""))
        .map(|l| parse_json(&l).expect("a truth line"))
        .collect();
    shapes::stations(&lines)
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

/// Resident memory now and its peak since the last reset, in MiB; the
/// reset (Linux: `clear_refs` 5) makes the peak the peak of what follows.
fn rss_mb() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        s.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kb| kb / 1024)
            .unwrap_or(0)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn reset_peak() {
    let _ = std::fs::write("/proc/self/clear_refs", "5");
}

/// `check.run`: each database check and each data check on its own, timed,
/// with the memory it added (peak resident during the call less resident
/// before), and the data checks scored against what was planted; then the
/// database checks in slices of 100,000 records.
fn check(run: &mut Run) -> Result<(), String> {
    run.run("CREATE EXTENSION IF NOT EXISTS check", &[])?;
    // The generated graph has no property index; give the index checks two
    // to read, one of them over long text that is cut to fit its keys.
    let (_, ms) = run.run("INDEX ON :WorkElement(code)", &[])?;
    let (_, ms2) = run.run("INDEX ON :WorkInstruction(text)", &[])?;
    run.emit(
        "check.indexes",
        Obj::new()
            .i("ms", (ms + ms2) as i64)
            .s("indexes", ":WorkElement(code), :WorkInstruction(text)"),
    );
    let p = run.planted;
    let mut cases: Vec<(String, String)> = [
        "endpoints",
        "adjacency",
        "membership",
        "counters",
        "indexes",
        "derived",
    ]
    .iter()
    .map(|c| (c.to_string(), format!("checks: ['{c}']")))
    .collect();
    cases.push(("all".into(), String::new()));
    cases.push(("orphans".into(), "checks: ['orphans']".into()));
    cases.push(("duplicates".into(), "checks: ['duplicates']".into()));
    for (label, (keys, _)) in &p.duplicates {
        let keys: Vec<String> = keys.iter().map(|k| format!("'{k}'")).collect();
        cases.push((
            format!("duplicates:{label}"),
            format!(
                "checks: ['duplicates'], label: '{label}', keys: [{}]",
                keys.join(", ")
            ),
        ));
    }
    cases.push((
        "required".into(),
        "checks: ['required'], label: 'Station', type: 'HAS_SIDE', dir: 'out'".into(),
    ));
    for (name, args) in cases {
        let (before, _) = rss_mb();
        reset_peak();
        let (r, ms) = run.run(&format!("CALL check.run({args})"), &[])?;
        let (_, peak) = rss_mb();
        let mut records = 0u64;
        let mut problems = 0u64;
        for row in &r.rows {
            if row[1] == Value::from("summary") {
                if let Value::Text(d) = &row[4] {
                    let j = parse_json(d).map_err(|e| e.to_string())?;
                    records += j.get("records").and_then(|v| v.as_u64()).unwrap_or(0);
                    problems += j.get("problems").and_then(|v| v.as_u64()).unwrap_or(0);
                }
            }
        }
        let f = checks::found(&r);
        let mut o = Obj::new()
            .s("check", &name)
            .i("ms", ms as i64)
            .u("records", records)
            .u("problems", problems)
            .u("faults", f.faults as u64)
            .u("rss_before_mb", before)
            .u("rss_added_mb", peak.saturating_sub(before));
        let score = |o: Obj, s: Score| {
            o.u("planted", s.planted as u64)
                .u("found", s.found as u64)
                .u("hit", s.hit as u64)
                .b("exact", s.exact())
        };
        if name == "orphans" {
            o = score(o, Score::of(&p.orphans, &f.orphans));
        } else if name == "duplicates" {
            o = score(o, Score::of(&p.parallel, &f.parallel));
        } else if let Some(label) = name.strip_prefix("duplicates:") {
            o = score(o, Score::of(&p.duplicates[label].1, &f.duplicates));
            let look = p.lookalikes.get(label).cloned().unwrap_or_default();
            o = o.u(
                "lookalikes_reported",
                look.intersection(&f.duplicates).count() as u64,
            );
        }
        run.emit("check", o);
    }
    // The database checks again, in slices.
    let (before, _) = rss_mb();
    reset_peak();
    let t = Instant::now();
    let (mut calls, mut rows) = (0u64, 0u64);
    let mut after: Option<String> = None;
    loop {
        let src = match &after {
            Some(c) => format!("CALL check.run(limit: 100000, after: '{c}')"),
            None => "CALL check.run(limit: 100000)".into(),
        };
        let (r, _) = run.run(&src, &[])?;
        calls += 1;
        rows += r
            .rows
            .iter()
            .filter(|row| row[1] != Value::from("summary"))
            .count() as u64;
        after = r
            .rows
            .iter()
            .find(|row| row[2] == Value::from("partial"))
            .and_then(|row| match &row[3] {
                Value::Text(c) => Some(c.clone()),
                _ => None,
            });
        if after.is_none() {
            break;
        }
    }
    let (_, peak) = rss_mb();
    run.emit(
        "check",
        Obj::new()
            .s("check", "all, slices of 100000")
            .i("ms", (t.elapsed().as_secs_f64() * 1e3) as i64)
            .u("calls", calls)
            .u("rows", rows)
            .u("rss_added_mb", peak.saturating_sub(before)),
    );
    Ok(())
}

/// Run a statement and measure it: (result, ms, resident before, peak
/// added during it), in MiB.
fn measured(run: &mut Run, src: &str) -> Result<(QueryResult, f64, u64, u64), String> {
    let (before, _) = rss_mb();
    reset_peak();
    let (r, ms) = run.run(src, &[])?;
    let (_, peak) = rss_mb();
    Ok((r, ms, before, peak.saturating_sub(before)))
}

/// `structure`: station colours over the hierarchy and the catalogue,
/// depth by depth; at each depth up to the design depth, kept and read back
/// in slices and scored against the designs; a colouring until stable; classes by
/// neighbours of the catalogue labels scored against the planted
/// duplicates and look-alikes; and classes by the kept colouring.
fn structure(run: &mut Run) -> Result<(), String> {
    run.run("CREATE EXTENSION IF NOT EXISTS structure", &[])?;
    let types = shapes::types_arg(shapes::DESIGN_TYPES);
    let base = format!("types: {types}, dir: 'out', labels: ['Station']");
    // Colours per depth, all nodes and stations.
    let (r, ms, before, added) = measured(
        run,
        &format!(
            "CALL structure.colors(depth: {}, {base}, summary: true)",
            shapes::DESIGN_DEPTH + 1
        ),
    )?;
    let counts: Vec<String> = r
        .rows
        .iter()
        .map(|row| format!("[{},{},{}]", row[0], row[1], row[2]))
        .collect();
    run.emit(
        "structure.colors",
        Obj::new()
            .s("what", "colours per depth")
            .u("depth", shapes::DESIGN_DEPTH as u64 + 1)
            .i("ms", ms as i64)
            .u("rss_before_mb", before)
            .u("rss_added_mb", added)
            .raw("depths", &format!("[{}]", counts.join(","))),
    );
    // The design depth, kept, then read back a slice at a time and scored;
    // and the depths before it, scored the same way.
    for depth in 1..=shapes::DESIGN_DEPTH {
        let name = format!("depth{depth}");
        let (r, ms, before, added) = measured(
            run,
            &format!("CALL structure.colors(depth: {depth}, {base}, limit: 1, store: '{name}')"),
        )?;
        let mut c = std::collections::BTreeMap::new();
        let mut after = 0i64;
        let t = Instant::now();
        let mut slices = 0u64;
        loop {
            let src =
                format!("CALL structure.read(name: '{name}', fresh: 'require', after: {after})");
            let (r, _) = run.run(&src, &[])?;
            if r.rows.is_empty() {
                break;
            }
            slices += 1;
            after = r.rows.last().unwrap()[0].as_i64().unwrap_or(0);
            c.extend(shapes::colours(&r));
        }
        let read_ms = t.elapsed().as_secs_f64() * 1e3;
        let rec = shapes::recovery(&c, run.stations);
        let o = Obj::new()
            .s("what", "design recovery")
            .u("depth", depth as u64)
            .i("ms", ms as i64)
            .u("rss_before_mb", before)
            .u("rss_added_mb", added)
            .s("message", r.message.as_deref().unwrap_or(""))
            .u("stations", c.len() as u64)
            .u("station_colours", rec.colours as u64)
            .u("designs", rec.designs as u64)
            .u("designs_whole", rec.whole as u64)
            .u("designs_alone", rec.alone as u64)
            .u("deviated", rec.deviated as u64)
            .u("deviated_apart", rec.apart as u64)
            .b("exact", rec.exact())
            .s("kept", &name)
            .i("read_ms", read_ms as i64)
            .u("read_slices", slices);
        run.emit("structure.colors", o);
    }
    // Until nothing changes.
    let (r, ms, before, added) = measured(
        run,
        &format!("CALL structure.colors(depth: 'stable', {base}, summary: true)"),
    )?;
    run.emit(
        "structure.colors",
        Obj::new()
            .s("what", "stable")
            .i("ms", ms as i64)
            .u("rss_before_mb", before)
            .u("rss_added_mb", added)
            .u("depths", r.rows.len() as u64)
            .s("message", r.message.as_deref().unwrap_or("")),
    );
    // Classes by neighbours of the catalogue, against the truth.
    let p = run.planted;
    for (label, (keys, pairs)) in &p.duplicates {
        let keys: Vec<&str> = keys.iter().map(|k| k.as_str()).collect();
        let (types, dir) = shapes::duplicate_types(label);
        let args = format!(
            "by: 'neighbours', labels: ['{label}'], keys: {}, types: {}, dir: '{dir}'",
            shapes::types_arg(&keys),
            shapes::types_arg(types)
        );
        let (r, ms, before, added) = measured(run, &format!("CALL structure.classes({args})"))?;
        let found = shapes::class_pairs(&r);
        let planted = shapes::closure(pairs);
        let s = Score::of(&planted, &found);
        let look = p.lookalikes.get(label).cloned().unwrap_or_default();
        let (sizes, _) = run.run(
            &format!("CALL structure.classes({args}, min_size: 1, summary: true)"),
            &[],
        )?;
        let dist: Vec<String> = shapes::sizes(&sizes)
            .iter()
            .map(|(s, n)| format!("[{s},{n}]"))
            .collect();
        run.emit(
            "structure.classes",
            Obj::new()
                .s("by", "neighbours")
                .s("label", label)
                .i("ms", ms as i64)
                .u("rss_before_mb", before)
                .u("rss_added_mb", added)
                .u("classes", r.rows.len() as u64)
                .u("planted", s.planted as u64)
                .u("found", s.found as u64)
                .u("hit", s.hit as u64)
                .b("exact", s.exact())
                .u("lookalikes", look.len() as u64)
                .u(
                    "lookalikes_reported",
                    look.intersection(&found).count() as u64,
                )
                .raw("sizes", &format!("[{}]", dist.join(","))),
        );
    }
    // Classes of stations by the kept colouring: the designs.
    let (r, ms, before, added) = measured(
        run,
        &format!(
            "CALL structure.classes(by: 'colors', colors: 'depth{}', fresh: 'require', min_size: 1, summary: true)",
            shapes::DESIGN_DEPTH
        ),
    )?;
    let dist: Vec<String> = shapes::sizes(&r)
        .iter()
        .map(|(s, n)| format!("[{s},{n}]"))
        .collect();
    run.emit(
        "structure.classes",
        Obj::new()
            .s("by", "colors")
            .s("label", "Station")
            .i("ms", ms as i64)
            .u("rss_before_mb", before)
            .u("rss_added_mb", added)
            .raw("sizes", &format!("[{}]", dist.join(","))),
    );
    Ok(())
}

// -------------------------------------------------------------------- main

/// Writes to time with and without the journal: node property sets, edge
/// adds, edge deletes, committed every `COMMIT_EVERY`.
const WRITES: u64 = 100_000;
const COMMIT_EVERY: u64 = 10_000;

/// One timed pass of `n` writes of one kind; returns writes a second and
/// log bytes a write. `round` makes every value new, so each write changes
/// something.
fn write_pass(
    g: &mut Graph,
    kind: &str,
    n: u64,
    round: u64,
    edges: &mut Vec<u64>,
) -> Result<(f64, f64), String> {
    let nodes = g.node_count() as u64;
    let rev = g.revision();
    let t = Instant::now();
    g.autocommit = false;
    let mut x = 0x9e37_79b9_7f4a_7c15u64 ^ round;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        1 + x % nodes
    };
    for i in 0..n {
        let r = match kind {
            "node set" => g.set_node_prop(1 + i % nodes, "cx", Value::Int((round * n + i) as i64)),
            "edge add" => g
                .add_edge(next(), next(), "CHANGED", vec![])
                .map(|e| edges.push(e)),
            _ => g.delete_edge(edges[i as usize]).map(|_| ()),
        };
        r.map_err(|e| format!("{kind}: {e}"))?;
        if (i + 1) % COMMIT_EVERY == 0 {
            g.commit().map_err(|e| e.to_string())?;
        }
    }
    g.commit().map_err(|e| e.to_string())?;
    g.autocommit = true;
    if kind == "edge delete" {
        edges.clear();
    }
    let s = t.elapsed().as_secs_f64();
    Ok((n as f64 / s, (g.revision() - rev) as f64 / n as f64))
}

/// `changes`: node and edge write throughput without and with the journal,
/// alternated (two rounds each), the log bytes a write and the bytes an
/// entry; then `since` over the whole journal in slices, `since` near its
/// end, and the questions other extensions ask.
fn changes(run: &mut Run) -> Result<(), String> {
    let n = WRITES.min(run.g.node_count() as u64);
    let mut edges = Vec::with_capacity(n as usize);
    let mut round = 0u64;
    for journal in [false, true, false, true] {
        if journal {
            run.run("CREATE EXTENSION changes", &[])?;
            run.run(&format!("CALL changes.cap(entries: {})", 10 * n), &[])?;
        }
        let from = run.g.revision();
        for kind in ["node set", "edge add", "edge delete"] {
            round += 1;
            let (rate, log) = write_pass(&mut run.g, kind, n, round, &mut edges)?;
            run.emit(
                "changes.write",
                Obj::new()
                    .s("write", kind)
                    .b("journal", journal)
                    .u("round", round)
                    .u("writes", n)
                    .u("per_s", rate as u64)
                    .raw("log_bytes_per_write", &format!("{log:.1}")),
            );
        }
        if !journal {
            continue;
        }
        let (r, _) = run.run("CALL changes.status()", &[])?;
        let entries = r.rows[0][2].as_i64().unwrap_or(0) as u64;
        let bytes = r.rows[0][3].as_i64().unwrap_or(0) as u64;
        // since over everything, in slices of 10,000, by statement.
        let t = Instant::now();
        let (mut rows, mut calls) = (0u64, 0u64);
        let mut after = String::new();
        loop {
            let (r, _) = run.run(&format!("CALL changes.since(revision: {from}{after})"), &[])?;
            calls += 1;
            rows += r.rows.len() as u64;
            match r
                .message
                .as_deref()
                .and_then(|m| m.split("after: '").nth(1))
            {
                Some(c) => after = format!(", after: '{}'", c.trim_end_matches('\'')),
                None => break,
            }
        }
        let all_ms = t.elapsed().as_secs_f64() * 1e3;
        // since near the end: the last commit's changes.
        let last = run.g.revision();
        run.g
            .set_node_prop(1, "cx", Value::Int(-1))
            .map_err(|e| e.to_string())?;
        let (r, tail_ms) = run.run(&format!("CALL changes.since(revision: {last})"), &[])?;
        // The Rust questions: 1,000 ids, and whether anything changed.
        let ids: Vec<u64> = (1..=1_000).collect();
        let t = Instant::now();
        let hit =
            glider::ext::changes::changed(&run.g, from, glider::ext::changes::Kind::Node, &ids)
                .map(|v| v.iter().filter(|c| **c).count())
                .unwrap_or(0);
        let changed_us = t.elapsed().as_secs_f64() * 1e6;
        let t = Instant::now();
        let any = glider::ext::changes::changed_any(&run.g, last, glider::ext::changes::Kind::Edge);
        let any_us = t.elapsed().as_secs_f64() * 1e6;
        run.emit(
            "changes.read",
            Obj::new()
                .u("entries", entries)
                .u("bytes", bytes)
                .raw(
                    "bytes_per_entry",
                    &format!("{:.1}", bytes as f64 / entries.max(1) as f64),
                )
                .u("since_rows", rows)
                .u("since_calls", calls)
                .raw("since_all_ms", &format!("{all_ms:.1}"))
                .u("since_tail_rows", r.rows.len() as u64)
                .raw("since_tail_ms", &format!("{tail_ms:.2}"))
                .u("changed_ids", ids.len() as u64)
                .u("changed_hit", hit as u64)
                .raw("changed_us", &format!("{changed_us:.0}"))
                .b("changed_any_edge", any == Some(true))
                .raw("changed_any_us", &format!("{any_us:.1}")),
        );
        run.run("DROP EXTENSION changes", &[])?;
    }
    Ok(())
}

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
    let planted = read_planted(&data.join("truth.jsonl"));
    let stations = read_stations(&data.join("truth.jsonl"));
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
            planted: &planted,
            stations: &stations,
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
