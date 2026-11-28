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

// ---------------------------------------------------------------- templates

/// The hierarchy templates follow: plant down to the work instructions.
/// What a work element uses (tools, parts, standards) and what it depends
/// on are not hierarchy: they are written as references and internal
/// edges of the shapes.
pub const TEMPLATE_TYPES: &[&str] = &[
    "HAS_PLANT",
    "HAS_SHOP",
    "HAS_LINE",
    "HAS_STATION",
    "HAS_SIDE",
    "HAS_LOCATION",
    "HAS_ELEMENT",
    "HAS_INSTRUCTION",
];

/// How well station templates recover the designs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TemplateRecovery {
    pub designs: usize,
    /// Designs with at least two stations without a deviation: the ones a
    /// template (of two or more instances) can say.
    pub eligible: usize,
    /// Eligible designs whose stations without a deviation are exactly one
    /// template's instances.
    pub recovered: usize,
    pub deviated: usize,
    /// Deviated stations that are instances of their design's template:
    /// must be none.
    pub deviated_in_design: usize,
    /// Templates of stations that are not a design's: stations that
    /// deviate the same way.
    pub other_templates: usize,
    pub templates: usize,
}

impl TemplateRecovery {
    pub fn exact(&self) -> bool {
        self.recovered == self.eligible && self.deviated_in_design == 0
    }
}

/// Score `template -> station instances` against the designs.
pub fn template_recovery(
    instances: &BTreeMap<String, BTreeSet<u64>>,
    stations: &[Station],
) -> TemplateRecovery {
    let mut clean: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    let mut designs = BTreeSet::new();
    for s in stations {
        designs.insert(s.design);
        if !s.deviated {
            clean.entry(s.design).or_default().insert(s.id);
        }
    }
    let mut r = TemplateRecovery {
        designs: designs.len(),
        deviated: stations.iter().filter(|s| s.deviated).count(),
        templates: instances.len(),
        ..TemplateRecovery::default()
    };
    let sets: BTreeSet<&BTreeSet<u64>> = instances.values().collect();
    let mut design_sets: BTreeSet<&BTreeSet<u64>> = BTreeSet::new();
    for members in clean.values() {
        if members.len() >= 2 {
            r.eligible += 1;
            if sets.contains(members) {
                r.recovered += 1;
                design_sets.insert(members);
            }
        }
    }
    let design_of: BTreeMap<u64, (u64, bool)> = stations
        .iter()
        .map(|s| (s.id, (s.design, s.deviated)))
        .collect();
    for members in instances.values() {
        let has_clean = members
            .iter()
            .any(|m| design_of.get(m).is_some_and(|d| !d.1));
        if has_clean {
            r.deviated_in_design += members
                .iter()
                .filter(|m| design_of.get(m).is_some_and(|d| d.1))
                .count();
        } else {
            r.other_templates += 1;
        }
    }
    r
}

/// `template -> roots` from `templates(instances: '*', grouped: true)`
/// rows; returns how many roots they listed.
pub fn instance_sets(rows: &[Vec<Value>], into: &mut BTreeMap<String, BTreeSet<u64>>) -> usize {
    let mut n = 0;
    for row in rows {
        if let (Value::Text(t), Some(Value::List(roots))) = (&row[0], row.get(2)) {
            let e = into.entry(t.clone()).or_default();
            for r in roots {
                if let Value::Int(r) = r {
                    e.insert(*r as u64);
                    n += 1;
                }
            }
        }
    }
    n
}

fn exact_value(v: &Value) -> String {
    match v {
        Value::Int(i) => format!("i{i}"),
        Value::Float(f) => format!("f{}", f.to_bits()),
        Value::List(items) => format!(
            "[{}]",
            items.iter().map(exact_value).collect::<Vec<_>>().join(",")
        ),
        v => format!("{:?}", v),
    }
}

fn props_key(props: &[(String, Value)]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = props
        .iter()
        .map(|(k, v)| (k.clone(), exact_value(v)))
        .collect();
    v.sort();
    v
}

/// Whether `expand` rows (several roots, with a `root` column) are exactly
/// the instances in the graph: per root, its nodes with their labels and
/// values, and the edges going out of them. Returns (roots, roots exact).
pub fn expansion_exact(g: &glider::graph::Graph, r: &QueryResult) -> (usize, usize) {
    type Edge = (u64, String, u64, Vec<(String, String)>);
    let at = r.columns.iter().position(|c| c == "root").unwrap_or(7);
    let pairs = |v: &Value| -> Vec<(String, Value)> {
        match v {
            Value::List(ps) => ps
                .iter()
                .filter_map(|p| match p {
                    Value::List(kv) if kv.len() == 2 => Some((kv[0].to_string(), kv[1].clone())),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    };
    let mut per: BTreeMap<
        u64,
        (
            BTreeMap<u64, (Vec<String>, Vec<(String, String)>)>,
            Vec<Edge>,
        ),
    > = BTreeMap::new();
    for row in &r.rows {
        let root = row[at].as_i64().unwrap_or(0) as u64;
        let e = per.entry(root).or_default();
        if row[0] == Value::Text("node".into()) {
            let mut labels: Vec<String> = match &row[2] {
                Value::List(l) => l.iter().map(|x| x.to_string()).collect(),
                _ => Vec::new(),
            };
            labels.sort();
            e.0.insert(
                row[1].as_i64().unwrap_or(0) as u64,
                (labels, props_key(&pairs(&row[6]))),
            );
        } else {
            e.1.push((
                row[4].as_i64().unwrap_or(0) as u64,
                row[3].to_string(),
                row[5].as_i64().unwrap_or(0) as u64,
                props_key(&pairs(&row[6])),
            ));
        }
    }
    let mut exact = 0;
    let roots = per.len();
    for (_, (nodes, mut edges)) in per {
        let mut ok = true;
        let mut want: Vec<Edge> = Vec::new();
        for (&id, (labels, props)) in &nodes {
            let mut gl = g.node_labels(id);
            gl.sort();
            if &gl != labels || &props_key(&g.node_props(id)) != props {
                ok = false;
            }
            g.for_each_adj(id, glider::graph::Dir::Out, None, |a| {
                want.push((
                    id,
                    g.strings.name(a.etype).to_string(),
                    a.other,
                    props_key(&g.edge_props(a.edge)),
                ));
            });
        }
        want.sort();
        edges.sort();
        if ok && want == edges {
            exact += 1;
        }
    }
    (roots, exact)
}

/// The nodes under `root` along `types`, outwards, root first.
pub fn subtree(g: &glider::graph::Graph, root: u64, types: &[&str]) -> Vec<u64> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(v) = stack.pop() {
        out.push(v);
        g.for_each_adj(v, glider::graph::Dir::Out, None, |a| {
            if types.contains(&g.strings.name(a.etype)) {
                stack.push(a.other);
            }
        });
    }
    out
}
