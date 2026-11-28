//! mfg-bench: the baseline over a manufacturing graph from `mfg-gen`.
//!
//!   mfg-bench --data DIR [--tiers memory,file] [--budgets 250,1000,4000]
//!             [--only stats,retrieve,check,structure,templates,...] [--out FILE] [--label NAME] [--detail]
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
    ("templates", templates),
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

/// A statement with parameters, timed, with the memory it added.
fn measured_with(
    run: &mut Run,
    src: &str,
    params: &[(String, Value)],
) -> Result<(QueryResult, f64, u64, u64), String> {
    let (before, _) = rss_mb();
    reset_peak();
    let (r, ms) = run.run(src, params)?;
    let (_, peak) = rss_mb();
    Ok((r, ms, before, peak.saturating_sub(before)))
}

fn cell(r: &QueryResult, row: usize, name: &str) -> Value {
    let c = r.columns.iter().position(|c| c == name).unwrap_or(0);
    r.rows
        .get(row)
        .and_then(|x| x.get(c))
        .cloned()
        .unwrap_or(Value::Null)
}

fn num(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

/// Every `(template, root)` that `templates(instances: '*')` lists, read in
/// slices of the row cap.
fn all_instances(
    run: &mut Run,
    args: &str,
) -> Result<(std::collections::BTreeMap<String, BTreeSet<u64>>, f64, u64), String> {
    let mut sets = std::collections::BTreeMap::new();
    let mut after = 0usize;
    let mut ms = 0.0;
    let mut calls = 0;
    loop {
        let (r, t) = run.run(
            &format!(
                "CALL structure.templates({args}, instances: '*', grouped: true, after: {after})"
            ),
            &[],
        )?;
        ms += t;
        calls += 1;
        if r.rows.is_empty() {
            return Ok((sets, ms, calls));
        }
        after += shapes::instance_sets(&r.rows, &mut sets);
    }
}

/// `templates`: station templates scored against the designs; every
/// repeated shape and the ones nested in what the plan keeps; the long
/// hierarchy path and the chains; the plan of the whole graph, kept, and
/// of one station and one line, alone and with the kept dictionary;
/// every instance of the kept dictionary expanded and compared with the
/// graph.
fn templates(run: &mut Run) -> Result<(), String> {
    run.run("CREATE EXTENSION IF NOT EXISTS structure", &[])?;
    let types = shapes::types_arg(shapes::TEMPLATE_TYPES);
    let hier = format!("types: {types}");
    // Station templates against the designs.
    let stations_args = format!("{hier}, labels: ['Station']");
    let (r, ms, before, added) = measured(
        run,
        &format!("CALL structure.templates({stations_args}, limit: 1)"),
    )?;
    let (sets, list_ms, calls) = all_instances(run, &stations_args)?;
    let rec = shapes::template_recovery(&sets, run.stations);
    run.emit(
        "structure.templates",
        Obj::new()
            .s("what", "design recovery")
            .i("ms", ms as i64)
            .u("rss_before_mb", before)
            .u("rss_added_mb", added)
            .s("message", r.message.as_deref().unwrap_or(""))
            .u("templates", rec.templates as u64)
            .u("designs", rec.designs as u64)
            .u("designs_eligible", rec.eligible as u64)
            .u("designs_recovered", rec.recovered as u64)
            .u("deviated", rec.deviated as u64)
            .u("deviated_in_design", rec.deviated_in_design as u64)
            .u("deviation_templates", rec.other_templates as u64)
            .b("exact", rec.exact())
            .i("instances_ms", list_ms as i64)
            .u("instances_calls", calls),
    );
    // Every repeated shape, by root label.
    let (r, ms, before, added) = measured(
        run,
        &format!("CALL structure.templates({hier}, limit: 10000)"),
    )?;
    let mut by_label: std::collections::BTreeMap<String, (u64, u64, u64)> = Default::default();
    for i in 0..r.rows.len() {
        let e = by_label.entry(cell(&r, i, "root").to_string()).or_default();
        e.0 += 1;
        e.1 += num(&cell(&r, i, "instances")) as u64;
        e.2 = e.2.max(num(&cell(&r, i, "nodes")) as u64);
    }
    let labels: Vec<String> = by_label
        .iter()
        .map(|(l, (t, n, m))| format!("[\"{l}\",{t},{n},{m}]"))
        .collect();
    run.emit(
        "structure.templates",
        Obj::new()
            .s("what", "all shapes")
            .i("ms", ms as i64)
            .u("rss_before_mb", before)
            .u("rss_added_mb", added)
            .u("templates", r.rows.len() as u64)
            .raw("by_root", &format!("[{}]", labels.join(",")))
            .s("message", r.message.as_deref().unwrap_or("")),
    );
    // Path patterns and chains.
    let (r, ms, before, added) = measured(run, &format!("CALL structure.paths({hier}, limit: 5)"))?;
    run.emit(
        "structure.paths",
        Obj::new()
            .s("what", "patterns")
            .i("ms", ms as i64)
            .u("rss_before_mb", before)
            .u("rss_added_mb", added)
            .s("top", &cell(&r, 0, "path").to_string())
            .u("top_hops", num(&cell(&r, 0, "hops")) as u64)
            .u("top_paths", num(&cell(&r, 0, "paths")) as u64)
            .b("top_maximal", cell(&r, 0, "maximal") == Value::Bool(true))
            .s("message", r.message.as_deref().unwrap_or("")),
    );
    let (r, ms, _, _) = measured(
        run,
        &format!("CALL structure.paths({hier}, kind: 'chains')"),
    )?;
    run.emit(
        "structure.paths",
        Obj::new()
            .s("what", "chains")
            .i("ms", ms as i64)
            .u("chains_first_slice", r.rows.len() as u64)
            .s("first", &cell(&r, 0, "path").to_string()),
    );
    // The whole graph, planned and kept.
    let (r, ms, before, added) = measured(
        run,
        &format!("CALL structure.plan({hier}, store: 'mfg', limit: 10000)"),
    )?;
    let chosen: Vec<usize> = (1..r.rows.len())
        .filter(|&i| cell(&r, i, "chosen") == Value::Bool(true))
        .collect();
    let kind = |i: usize| cell(&r, i, "kind").to_string();
    let t_chosen = chosen.iter().filter(|&&i| kind(i) == "template").count();
    let p_chosen = chosen.iter().filter(|&&i| kind(i) == "pattern").count();
    let nested = chosen
        .iter()
        .filter(|&&i| num(&cell(&r, i, "nested_in")) > 0)
        .count();
    let nested_uses: i64 = chosen
        .iter()
        .filter(|&&i| num(&cell(&r, i, "nested_in")) > 0)
        .map(|&i| num(&cell(&r, i, "uses")))
        .sum();
    let (kept, _) = run.run("CALL structure.dictionaries()", &[])?;
    let stored = num(&cell(&kept, 0, "stored_bytes"));
    run.emit(
        "structure.plan",
        Obj::new()
            .s("what", "whole graph")
            .i("kept_stored_bytes", stored)
            .i("ms", ms as i64)
            .u("rss_before_mb", before)
            .u("rss_added_mb", added)
            .i("plain_bytes", num(&cell(&r, 0, "plain_bytes")))
            .i("plain_tokens", num(&cell(&r, 0, "plain_tokens")))
            .i("compact_bytes", num(&cell(&r, 0, "use_bytes")))
            .i("compact_tokens", num(&cell(&r, 0, "use_tokens")))
            .i("dictionary_bytes", num(&cell(&r, 0, "define_bytes")))
            .i("dictionary_tokens", num(&cell(&r, 0, "define_tokens")))
            .i("saving_tokens", num(&cell(&r, 0, "saving_tokens")))
            .i("uses", num(&cell(&r, 0, "uses")))
            .u("templates_chosen", t_chosen as u64)
            .u("templates_nested", nested as u64)
            .i("nested_standalone_uses", nested_uses)
            .u("patterns_chosen", p_chosen as u64)
            .u("candidates", r.rows.len() as u64 - 1)
            .s("dictionary", &cell(&r, 0, "id").to_string()),
    );
    // One station and one line: alone, and with the kept dictionary.
    let station = run
        .stations
        .iter()
        .find(|s| !s.deviated)
        .map(|s| s.id)
        .ok_or("no station")?;
    let mut line = None;
    run.g
        .for_each_adj(station, glider::graph::Dir::In, None, |a| {
            line = Some(a.other);
        });
    let line = line.ok_or("a station with no line")?;
    let below = &shapes::TEMPLATE_TYPES[4..];
    let mut line_types = vec!["HAS_STATION"];
    line_types.extend_from_slice(below);
    // A few hundred nodes scattered over the graph: ids from a fixed
    // stream, spread over the whole id range.
    let top = run.g.node_ids().last().copied().unwrap_or(1);
    let mut z = 0x9e37_79b9_7f4a_7c15u64;
    let mut scattered: Vec<u64> = Vec::new();
    while scattered.len() < 300 {
        z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut x = z;
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        let id = 1 + (x ^ (x >> 31)) % top;
        if run.g.node_shared(id).is_some() {
            scattered.push(id);
        }
    }
    for (what, nodes) in [
        ("one station", shapes::subtree(&run.g, station, below)),
        ("one line", shapes::subtree(&run.g, line, &line_types)),
        ("300 scattered nodes", scattered),
    ] {
        let ids = Value::List(nodes.iter().map(|&n| Value::Int(n as i64)).collect());
        let params = vec![("nodes".to_string(), ids)];
        let (alone, ms_alone, _, _) = measured_with(
            run,
            &format!("CALL structure.plan({hier}, nodes: $nodes, limit: 1)"),
            &params,
        )?;
        // With the kept dictionary, from the selection alone: five runs.
        let mut times = Vec::new();
        let (mut kept, mut added) = (None, 0);
        for _ in 0..5 {
            let (r, ms, _, add) = measured_with(
                run,
                "CALL structure.plan(dictionary: 'mfg', nodes: $nodes, text: 'compact', limit: 1)",
                &params,
            )?;
            times.push(ms);
            added = added.max(add);
            kept = Some(r);
        }
        let kept = kept.unwrap();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // The same selection found over the whole graph: the same text.
        let (whole, ms_whole, _, _) = measured_with(
            run,
            "CALL structure.plan(dictionary: 'mfg', nodes: $nodes, text: 'compact', limit: 1, find: 'graph')",
            &params,
        )?;
        let same = cell(&kept, 0, "text") == cell(&whole, 0, "text");
        run.emit(
            "structure.plan",
            Obj::new()
                .s("what", what)
                .u("nodes", nodes.len() as u64)
                .i("plain_bytes", num(&cell(&alone, 0, "plain_bytes")))
                .i("plain_tokens", num(&cell(&alone, 0, "plain_tokens")))
                .i("alone_bytes", num(&cell(&alone, 0, "use_bytes")))
                .i("alone_tokens", num(&cell(&alone, 0, "use_tokens")))
                .b(
                    "alone_chosen",
                    cell(&alone, 0, "chosen") == Value::Bool(true),
                )
                .i("kept_bytes", num(&cell(&kept, 0, "use_bytes")))
                .i("kept_tokens", num(&cell(&kept, 0, "use_tokens")))
                .i("kept_uses", num(&cell(&kept, 0, "uses")))
                .i("ms_alone", ms_alone as i64)
                .raw("ms_kept_min", &format!("{:.2}", times[0]))
                .raw("ms_kept_median", &format!("{:.2}", times[2]))
                .u("kept_rss_added_mb", added)
                .i("ms_whole_graph_path", ms_whole as i64)
                .b("same_text_as_whole_graph_path", same),
        );
    }
    // Every instance of the kept dictionary, expanded and compared.
    let (sets, list_ms, calls) = all_instances(run, "dictionary: 'mfg'")?;
    let (before, _) = rss_mb();
    reset_peak();
    let t = Instant::now();
    let (mut roots, mut exact, mut expand_calls) = (0usize, 0usize, 0u64);
    for (id, members) in &sets {
        let members: Vec<u64> = members.iter().copied().collect();
        for chunk in members.chunks(1000) {
            let list = Value::List(chunk.iter().map(|&n| Value::Int(n as i64)).collect());
            let (r, _) = run.run(
                &format!("CALL structure.expand(of: '{id}', root: $roots, dictionary: 'mfg')"),
                &[("roots".to_string(), list)],
            )?;
            expand_calls += 1;
            let (n, x) = shapes::expansion_exact(&run.g, &r);
            roots += n;
            exact += x;
        }
    }
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let (_, peak) = rss_mb();
    run.emit(
        "structure.expand",
        Obj::new()
            .s("what", "every instance of the kept dictionary")
            .u("templates", sets.len() as u64)
            .u("instances", roots as u64)
            .u("exact", exact as u64)
            .b("all_exact", roots == exact)
            .i("ms", ms as i64)
            .u("calls", expand_calls)
            .u("rss_before_mb", before)
            .u("rss_added_mb", peak.saturating_sub(before))
            .i("list_ms", list_ms as i64)
            .u("list_calls", calls),
    );
    Ok(())
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
