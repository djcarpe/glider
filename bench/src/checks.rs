//! What `CALL check.run()` should find in a manufacturing graph, read from
//! its truth, and how what it found compares: shared by `mfg-bench` and the
//! tests.

use std::collections::{BTreeMap, BTreeSet};

use glider::query::QueryResult;
use glider::value::{Json, Value};

/// A pair of ids: (the first, the one that repeats it).
pub type Pair = (u64, u64);

/// The facts the generator planted.
#[derive(Debug, Default)]
pub struct Planted {
    pub orphans: BTreeSet<u64>,
    /// Parallel edges as (the first edge, the one that repeats it).
    pub parallel: BTreeSet<Pair>,
    /// Per label: the properties its duplicates agree on, and the pairs.
    pub duplicates: BTreeMap<String, (Vec<String>, BTreeSet<Pair>)>,
    /// Per label: pairs that look alike and are different things.
    pub lookalikes: BTreeMap<String, BTreeSet<Pair>>,
}

fn s<'a>(j: &'a Json, k: &str) -> &'a str {
    j.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn u(j: &Json, k: &str) -> u64 {
    j.get(k).and_then(|v| v.as_u64()).unwrap_or(0)
}

fn list<'a>(j: &'a Json, k: &str) -> Vec<&'a Json> {
    j.get(k)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

fn ordered(a: u64, b: u64) -> Pair {
    (a.min(b), a.max(b))
}

pub fn planted(truth: &[Json]) -> Planted {
    let mut p = Planted::default();
    for r in truth {
        match (s(r, "kind"), s(r, "fault")) {
            ("fault", "orphan") => {
                p.orphans.insert(u(r, "id"));
            }
            ("fault", "parallel_edge") => {
                let e: Vec<u64> = list(r, "edges").iter().filter_map(|x| x.as_u64()).collect();
                for later in e.iter().skip(1) {
                    p.parallel.insert((e[0], *later));
                }
            }
            ("duplicate", _) => {
                let agree: Vec<String> = list(r, "agree_on")
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect();
                let e = p
                    .duplicates
                    .entry(s(r, "label").to_string())
                    .or_insert_with(|| (agree.clone(), BTreeSet::new()));
                e.1.insert(ordered(u(r, "a"), u(r, "b")));
            }
            ("lookalike", _) => {
                p.lookalikes
                    .entry(s(r, "label").to_string())
                    .or_default()
                    .insert(ordered(u(r, "a"), u(r, "b")));
            }
            _ => {}
        }
    }
    p
}

fn text(v: &Value) -> &str {
    match v {
        Value::Text(t) => t,
        _ => "",
    }
}

fn handle(s: &str) -> Option<u64> {
    s.get(1..)?.parse().ok()
}

/// The first `n123` or `e123` handle in a detail that is not `skip`.
fn other(detail: &str, skip: &str) -> Option<u64> {
    detail
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| *w != skip && w.len() > 1)
        .find(|w| (w.starts_with('n') || w.starts_with('e')) && handle(w).is_some())
        .and_then(handle)
}

/// What a `check.run` result reports, by kind of finding.
#[derive(Debug, Default)]
pub struct Found {
    pub faults: usize,
    pub orphans: BTreeSet<u64>,
    pub parallel: BTreeSet<Pair>,
    pub duplicates: BTreeSet<Pair>,
    pub missing_edges: BTreeSet<u64>,
}

pub fn found(r: &QueryResult) -> Found {
    let mut f = Found::default();
    for row in &r.rows {
        let (sev, kind, id, detail) = (text(&row[1]), text(&row[2]), text(&row[3]), text(&row[4]));
        let n = handle(id);
        match (sev, kind) {
            ("fault", _) => f.faults += 1,
            ("candidate", "orphan") => f.orphans.extend(n),
            ("candidate", "parallel_edge") => {
                if let (Some(later), Some(first)) = (n, other(detail, id)) {
                    f.parallel.insert((first, later));
                }
            }
            ("candidate", "duplicate_node") => {
                if let (Some(later), Some(first)) = (n, other(detail, id)) {
                    f.duplicates.insert(ordered(first, later));
                }
            }
            ("candidate", "missing_edge") => f.missing_edges.extend(n),
            _ => {}
        }
    }
    f
}

/// How a found set compares with a planted one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Score {
    pub planted: usize,
    pub found: usize,
    /// Planted and found.
    pub hit: usize,
}

impl Score {
    pub fn of<T: Ord>(planted: &BTreeSet<T>, found: &BTreeSet<T>) -> Score {
        Score {
            planted: planted.len(),
            found: found.len(),
            hit: planted.intersection(found).count(),
        }
    }
    /// Every planted one found and nothing else.
    pub fn exact(&self) -> bool {
        self.hit == self.planted && self.hit == self.found
    }
}
