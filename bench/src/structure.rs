//! What `structure` should find in a manufacturing graph, read from its
//! truth, and how what it found compares: shared by `mfg-bench` and the
//! tests.

use std::collections::{BTreeMap, BTreeSet};

use glider::query::QueryResult;
use glider::value::{Json, Value};

use crate::checks::Pair;

/// The edge types that make a station what it is: the hierarchy below it
/// and what its work elements use from the catalogue, followed outwards.
pub const DESIGN_TYPES: &[&str] = &[
    "HAS_PLANT",
    "HAS_SHOP",
    "HAS_LINE",
    "HAS_STATION",
    "HAS_SIDE",
    "HAS_LOCATION",
    "HAS_ELEMENT",
    "HAS_INSTRUCTION",
    "USES_TOOL",
    "CONSUMES",
    "FOLLOWS",
    "DEPENDS_ON",
];

/// The depth at which a station's colour sees its work elements' tools:
/// station, side, location, element, then what the element uses.
pub const DESIGN_DEPTH: u32 = 4;

/// The `types:` list for a `CALL`.
pub fn types_arg(types: &[&str]) -> String {
    let items: Vec<String> = types.iter().map(|t| format!("'{t}'")).collect();
    format!("[{}]", items.join(", "))
}

/// Per label, the edge types and direction that say what a catalogue entry
/// hangs off, for `classes(by: "neighbours")`: what it is supplied by and
/// applies to, and for an instruction, the work element it belongs to. A
/// planted duplicate hangs off the same nodes as its original; nothing uses
/// it, so the edges that come in from users are left out, except
/// `HAS_INSTRUCTION`, which ties a local instruction to its own element.
pub fn duplicate_types(label: &str) -> (&'static [&'static str], &'static str) {
    match label {
        "Tool" => (&["SUPPLIED_BY"], "out"),
        "Part" => (&["SUPPLIED_BY", "APPLIES_TO"], "out"),
        _ => (&["APPLIES_TO", "ISSUED_BY", "HAS_INSTRUCTION"], "both"),
    }
}

/// A station of the truth: its id, its design and whether it deviates.
#[derive(Clone, Copy, Debug)]
pub struct Station {
    pub id: u64,
    pub design: u64,
    pub deviated: bool,
}

pub fn stations(truth: &[Json]) -> Vec<Station> {
    truth
        .iter()
        .filter(|r| r.get("kind").and_then(|v| v.as_str()) == Some("station"))
        .map(|r| Station {
            id: r.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
            design: r.get("design").and_then(|v| v.as_u64()).unwrap_or(0),
            deviated: r
                .get("deviations")
                .and_then(|v| v.as_array())
                .is_some_and(|a| !a.is_empty()),
        })
        .collect()
}

/// How well station colours recover the designs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Recovery {
    pub designs: usize,
    /// Designs whose stations without a deviation all share one colour.
    pub whole: usize,
    /// Designs whose colour no station of another design has.
    pub alone: usize,
    pub deviated: usize,
    /// Deviated stations whose colour is not their design's.
    pub apart: usize,
    /// Distinct colours among the stations.
    pub colours: usize,
}

impl Recovery {
    pub fn exact(&self) -> bool {
        self.whole == self.designs && self.alone == self.designs && self.apart == self.deviated
    }
}

/// Score station colours (`id -> colour`) against the designs.
pub fn recovery(colours: &BTreeMap<u64, String>, stations: &[Station]) -> Recovery {
    let mut per: BTreeMap<u64, BTreeSet<&str>> = BTreeMap::new();
    for s in stations.iter().filter(|s| !s.deviated) {
        if let Some(c) = colours.get(&s.id) {
            per.entry(s.design).or_default().insert(c);
        }
    }
    let mut r = Recovery {
        designs: per.len(),
        colours: stations
            .iter()
            .filter_map(|s| colours.get(&s.id))
            .collect::<BTreeSet<_>>()
            .len(),
        ..Recovery::default()
    };
    let design_colour: BTreeMap<u64, &str> = per
        .iter()
        .filter(|(_, cs)| cs.len() == 1)
        .map(|(d, cs)| (*d, *cs.iter().next().unwrap()))
        .collect();
    r.whole = design_colour.len();
    for (d, c) in &design_colour {
        let shared = stations
            .iter()
            .any(|s| s.design != *d && colours.get(&s.id).map(|x| x.as_str()) == Some(c));
        if !shared {
            r.alone += 1;
        }
    }
    for s in stations.iter().filter(|s| s.deviated) {
        r.deviated += 1;
        let mine = colours.get(&s.id).map(|x| x.as_str());
        if mine.is_some() && mine != design_colour.get(&s.design).copied() {
            r.apart += 1;
        }
    }
    r
}

/// `id -> colour` from `structure.colors` or `structure.read` rows.
pub fn colours(r: &QueryResult) -> BTreeMap<u64, String> {
    r.rows
        .iter()
        .filter_map(|row| match (&row[0], &row[1]) {
            (Value::Int(id), Value::Text(c)) => Some((*id as u64, c.clone())),
            _ => None,
        })
        .collect()
}

/// Every pair of members within each class of a `structure.classes` result.
pub fn class_pairs(r: &QueryResult) -> BTreeSet<Pair> {
    let mut out = BTreeSet::new();
    for row in &r.rows {
        let Some(Value::List(m)) = row.get(3) else {
            continue;
        };
        let ids: Vec<u64> = m
            .iter()
            .filter_map(|v| v.as_i64())
            .map(|v| v as u64)
            .collect();
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                out.insert((*a.min(b), *a.max(b)));
            }
        }
    }
    out
}

/// Class sizes and how many classes have each, from a `summary: true`
/// result.
pub fn sizes(r: &QueryResult) -> Vec<(u64, u64)> {
    r.rows
        .iter()
        .filter_map(|row| Some((row[0].as_i64()? as u64, row[1].as_i64()? as u64)))
        .collect()
}

/// Every pair within the groups that `pairs` join: two duplicates of one
/// original are duplicates of each other, which a class says and the truth
/// (each against its original) leaves implied.
pub fn closure(pairs: &BTreeSet<Pair>) -> BTreeSet<Pair> {
    let mut group: BTreeMap<u64, u64> = BTreeMap::new();
    fn root(group: &mut BTreeMap<u64, u64>, x: u64) -> u64 {
        let mut r = x;
        while let Some(&p) = group.get(&r) {
            if p == r {
                break;
            }
            r = p;
        }
        group.insert(x, r);
        r
    }
    for &(a, b) in pairs {
        let (ra, rb) = (root(&mut group, a), root(&mut group, b));
        group.insert(ra.max(rb), ra.min(rb));
        group.entry(ra.min(rb)).or_insert(ra.min(rb));
    }
    let ids: Vec<u64> = group.keys().copied().collect();
    let mut members: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for id in ids {
        let r = root(&mut group, id);
        members.entry(r).or_default().push(id);
    }
    let mut out = BTreeSet::new();
    for m in members.values() {
        for (i, a) in m.iter().enumerate() {
            for b in &m[i + 1..] {
                out.insert((*a.min(b), *a.max(b)));
            }
        }
    }
    out
}
