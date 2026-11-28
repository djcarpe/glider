//! A manufacturing graph whose truth is known.
//!
//! [`generate`] writes three JSON Lines streams from a seed and a target
//! node count:
//!
//! - the graph, in the format `glider <db> import` reads (`{"type":"node",
//!   "id":..}` and `{"type":"edge",..}` lines; node ids are assigned in the
//!   order written, starting at 1, so importing into an empty database gives
//!   every node the id written here, and edges likewise);
//! - the truth: which stations share a design and how each deviates, planted
//!   duplicates and look-alikes, topic groups, tool sharing, dependency
//!   order, planted faults;
//! - a fixed set of questions with expected answers drawn from the truth.
//!
//! Everything is a function of `(seed, nodes, faults)`: no clock, no hash
//! map iteration, no floating point in the output. Memory is bounded by the
//! catalogue and the plan (one small record per line), never by the number
//! of stations: a station's design and deviations are drawn from its own
//! random stream, and the graph is written as it is drawn. The format is
//! described in `bench/KNOWLEDGE.md`.

pub mod vocab;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::ops::Range;

use vocab::*;

/// Version of the three formats. Bumped when a reader would misread.
pub const FORMAT: u32 = 1;

/// How many questions of each kind, at most.
pub const PER_KIND: usize = 8;

/// Stations of one design whose member ids an aggregate question lists.
pub const MEMBER_CAP: usize = 200;

#[derive(Clone, Debug)]
pub struct Config {
    pub seed: u64,
    /// Roughly how many nodes to write; the result is within a few percent
    /// above a few thousand nodes.
    pub nodes: u64,
    /// Plant integrity faults (orphans, duplicate parallel edges) and list
    /// them in the truth.
    pub faults: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub nodes: u64,
    pub edges: u64,
    pub stations: u64,
    pub lines: u64,
    pub designs: usize,
    pub questions: usize,
    pub truth_records: u64,
}

// ----------------------------------------------------------------- random

#[inline]
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

// One random stream per purpose, so that adding a draw to one (planting
// faults, say) leaves every other stream as it was.
const S_PLAN: u64 = 1;
const S_CATALOG: u64 = 2;
const S_DESIGN: u64 = 3;
const S_STATION: u64 = 4;
const S_LINE: u64 = 5;
const S_FAULT: u64 = 6;
const S_QUESTION: u64 = 7;

/// splitmix64, seeded per (seed, stream, index).
pub struct R(u64);

impl R {
    pub fn new(seed: u64, stream: u64, i: u64) -> R {
        R(mix(
            mix(seed ^ stream.wrapping_mul(0xD6E8_FEB8_6659_FD93)) ^ i
        ))
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut x = self.0;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
    fn index(&mut self, n: usize) -> usize {
        self.below(n as u64) as usize
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }
    fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.index(xs.len())]
    }
    /// `k` distinct numbers below `n`, ascending (Floyd's algorithm: memory
    /// in `k`, not `n`).
    fn sample(&mut self, n: u64, k: usize) -> Vec<u64> {
        let k = (k as u64).min(n);
        let mut set = BTreeSet::new();
        for j in n - k..n {
            let t = self.below(j + 1);
            if !set.insert(t) {
                set.insert(j);
            }
        }
        set.into_iter().collect()
    }
    fn shuffle<T>(&mut self, xs: &mut [T]) {
        for i in (1..xs.len()).rev() {
            let j = self.index(i + 1);
            xs.swap(i, j);
        }
    }
}

// ------------------------------------------------------------------ dates

/// 2022-01-01 as days since 1970-01-01.
pub const DAY0: i64 = 18_993;

/// `YYYY-MM-DD` for days since 1970-01-01 (proleptic Gregorian).
pub fn date(z: i64) -> String {
    // H. Hinnant's civil_from_days.
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

// ------------------------------------------------------------------- json

fn esc(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A JSON object written field by field, in the order given.
pub struct Obj {
    s: String,
    first: bool,
}

impl Default for Obj {
    fn default() -> Self {
        Obj::new()
    }
}

impl Obj {
    pub fn new() -> Obj {
        Obj {
            s: String::from("{"),
            first: true,
        }
    }
    fn key(&mut self, k: &str) {
        if !self.first {
            self.s.push(',');
        }
        self.first = false;
        esc(k, &mut self.s);
        self.s.push(':');
    }
    pub fn s(mut self, k: &str, v: &str) -> Obj {
        self.key(k);
        esc(v, &mut self.s);
        self
    }
    pub fn i(mut self, k: &str, v: i64) -> Obj {
        self.key(k);
        self.s.push_str(&v.to_string());
        self
    }
    pub fn u(self, k: &str, v: u64) -> Obj {
        self.i(k, v as i64)
    }
    pub fn b(mut self, k: &str, v: bool) -> Obj {
        self.key(k);
        self.s.push_str(if v { "true" } else { "false" });
        self
    }
    pub fn ids(mut self, k: &str, v: &[u64]) -> Obj {
        self.key(k);
        self.s.push('[');
        for (i, x) in v.iter().enumerate() {
            if i > 0 {
                self.s.push(',');
            }
            self.s.push_str(&x.to_string());
        }
        self.s.push(']');
        self
    }
    pub fn strs<S: AsRef<str>>(mut self, k: &str, v: &[S]) -> Obj {
        self.key(k);
        self.s.push('[');
        for (i, x) in v.iter().enumerate() {
            if i > 0 {
                self.s.push(',');
            }
            esc(x.as_ref(), &mut self.s);
        }
        self.s.push(']');
        self
    }
    /// A value that is already JSON.
    pub fn raw(mut self, k: &str, v: &str) -> Obj {
        self.key(k);
        self.s.push_str(v);
        self
    }
    /// The fields of `other`, after these.
    pub fn extend(mut self, other: Obj) -> Obj {
        if !other.first {
            if !self.first {
                self.s.push(',');
            }
            self.first = false;
            self.s.push_str(&other.s[1..]);
        }
        self
    }
    pub fn done(mut self) -> String {
        self.s.push('}');
        self.s
    }
}

fn raw_list(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

// --------------------------------------------------------------- the text

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn spec_value(r: &mut R, topic: usize) -> i64 {
    let (_, lo, hi, step) = TOPICS[topic].spec;
    lo + step * r.range(0, (hi - lo) / step)
}

/// Title and text of an instruction about `topic` on `comp` at `spec`.
/// Two instructions of one topic share its verbs, objects and terms; with
/// the same component and specification they read nearly the same; across
/// topics they share only the frames and openers.
fn instruction(r: &mut R, topic: usize, comp: usize, spec: i64) -> (String, String) {
    let t = &TOPICS[topic];
    let c = COMPONENTS[comp];
    let verb = r.pick(t.verbs);
    let obj = r.pick(t.objects);
    let title = format!("{verb} {c} {obj}");
    let mut text = format!("{verb} the {c} {obj} to {spec} {}.", t.spec.0);
    let mut frames = r.sample(FRAMES.len() as u64, 3);
    r.shuffle(&mut frames);
    for f in frames {
        let a = r.index(t.terms.len());
        let b = (a + 1 + r.index(t.terms.len() - 1)) % t.terms.len();
        let s = FRAMES[f as usize]
            .replace("{o}", *r.pick(OPENERS))
            .replace("{a}", t.terms[a])
            .replace("{b}", t.terms[b])
            .replace("{t}", *r.pick(t.tools))
            .replace("{c}", c);
        text.push(' ');
        text.push_str(&s);
    }
    text.push(' ');
    text.push_str(&cap(*r.pick(t.checks)));
    text.push('.');
    (title, text)
}

/// The same name written by someone else: case, spacing or abbreviation.
fn variant(r: &mut R, name: &str) -> String {
    for _ in 0..4 {
        let v = match r.below(4) {
            0 => name.to_uppercase(),
            1 => name.to_lowercase(),
            2 => {
                // Drop the space between a number and its unit: "25 Nm" -> "25Nm".
                let words: Vec<&str> = name.split(' ').collect();
                let mut out = String::new();
                for (i, w) in words.iter().enumerate() {
                    if i > 0 {
                        let prev = words[i - 1];
                        if !prev.chars().last().is_some_and(|c| c.is_ascii_digit()) {
                            out.push(' ');
                        }
                    }
                    out.push_str(w);
                }
                out
            }
            _ => name.split(' ').map(cap).collect::<Vec<_>>().join(" "),
        };
        if v != name {
            return v;
        }
    }
    name.to_uppercase() + "."
}

// ---------------------------------------------------------------- catalog

struct Model {
    id: u64,
    name: String,
    code: String,
    body: &'static str,
    launched: i64,
}

struct Tool {
    id: u64,
    code: String,
    name: String,
    kind: &'static str,
    topic: usize,
    maker: &'static str,
    model: String,
    spec: String,
    supplier: usize,
    dup_of: Option<usize>,
}

/// (model index, from day, to day)
type Effect = (usize, i64, Option<i64>);

struct Part {
    id: u64,
    code: String,
    name: String,
    part_number: String,
    material: &'static str,
    size: String,
    comp: usize,
    supplier: usize,
    models: Vec<Effect>,
    dup_of: Option<usize>,
}

struct Std {
    id: u64,
    number: String,
    title: String,
    text: String,
    topic: usize,
    comp: usize,
    cluster: usize,
    models: Vec<Effect>,
    dup_of: Option<usize>,
}

struct Catalog {
    enterprise: u64,
    suppliers: Vec<u64>,
    models: Vec<Model>,
    tools: Vec<Tool>,
    parts: Vec<Part>,
    stds: Vec<Std>,
    clusters: Vec<Vec<usize>>,
    /// (a, b, label, differ_on) planted look-alikes, by catalogue index.
    lookalikes: Vec<(usize, usize, &'static str, &'static [&'static str])>,
    tools_by_topic: Vec<Vec<usize>>,
    parts_by_comp: Vec<Vec<usize>>,
    stds_by_topic: Vec<Vec<usize>>,
}

fn effects(r: &mut R, models: usize) -> Vec<Effect> {
    let k = 1 + r.index(3.min(models));
    let mut out: Vec<Effect> = r
        .sample(models as u64, k)
        .into_iter()
        .map(|m| {
            let from = DAY0 + r.range(0, 700);
            let to = if r.chance(400) {
                Some(from + r.range(180, 900))
            } else {
                None
            };
            (m as usize, from, to)
        })
        .collect();
    out.sort_by_key(|e| e.0);
    out
}

fn part_name(kind: usize, d: i64, l: i64, comp: usize) -> (String, String) {
    let pat = PART_KINDS[kind].0;
    let name = pat
        .replace("{d}", &d.to_string())
        .replace("{l}", &l.to_string())
        .replace("{c}", COMPONENTS[comp]);
    let size = if pat.contains("{d}x{l}") {
        format!("M{d}x{l}")
    } else if pat.contains("{d}") {
        format!("M{d}")
    } else if pat.contains("{l}") {
        format!("{l} mm")
    } else {
        COMPONENTS[comp].to_string()
    };
    (name, size)
}

const TOOL_DIFF: &[&str] = &["model", "spec"];
const PART_DIFF: &[&str] = &["part_number", "size"];

fn catalog(seed: u64, designs: usize) -> Catalog {
    let mut r = R::new(seed, S_CATALOG, 0);
    let n_sup = (4 + designs).min(SUPPLIERS.len());
    let n_models = (4 + designs).min(MODEL_NAMES.len());
    let models = (0..n_models)
        .map(|i| Model {
            id: 0,
            name: format!("{} {}", MODEL_NAMES[i], BODIES[i % BODIES.len()]),
            code: format!("MD-{:02}", i + 1),
            body: BODIES[i % BODIES.len()],
            launched: DAY0 - 400 + r.range(0, 600),
        })
        .collect();

    // Tools: every topic has one before any has two.
    let mut tools: Vec<Tool> = Vec::new();
    let mut lookalikes = Vec::new();
    let n_tools = 12 + 8 * designs;
    for i in 0..n_tools {
        let topic = if i < TOPICS.len() {
            i
        } else {
            r.index(TOPICS.len())
        };
        let t = &TOPICS[topic];
        let kind = *r.pick(t.tools);
        let maker = *r.pick(MAKERS);
        let spec = spec_value(&mut r, topic);
        tools.push(Tool {
            id: 0,
            code: String::new(),
            name: format!("{} {} {}", cap(kind), spec, t.spec.0),
            kind,
            topic,
            maker,
            model: format!("{}-{:04}", maker[..3].to_uppercase(), r.range(1000, 9999)),
            spec: format!("{} {}", spec, t.spec.0),
            supplier: if i < n_sup { i } else { r.index(n_sup) },
            dup_of: None,
        });
    }
    // Look-alikes: same kind and maker, another model and specification.
    for _ in 0..(n_tools / 12).max(1) {
        let b = r.index(n_tools);
        let (topic, kind, maker, supplier) = (
            tools[b].topic,
            tools[b].kind,
            tools[b].maker,
            tools[b].supplier,
        );
        let t = &TOPICS[topic];
        let base: i64 = tools[b].spec.split(' ').next().unwrap().parse().unwrap();
        let mut spec = base + t.spec.3 * r.range(1, 3);
        if spec > t.spec.2 {
            spec = base - t.spec.3 * r.range(1, 3);
        }
        let mut model = tools[b].model.clone();
        while model == tools[b].model {
            model = format!("{}-{:04}", maker[..3].to_uppercase(), r.range(1000, 9999));
        }
        lookalikes.push((b, tools.len(), "Tool", TOOL_DIFF));
        tools.push(Tool {
            id: 0,
            code: String::new(),
            name: format!("{} {} {}", cap(kind), spec, t.spec.0),
            kind,
            topic,
            maker,
            model,
            spec: format!("{} {}", spec, t.spec.0),
            supplier,
            dup_of: None,
        });
    }
    // Duplicates: the same tool entered twice.
    let originals = tools.len();
    for _ in 0..(originals / 20).max(1) {
        let b = r.index(originals);
        let name = variant(&mut r, &tools[b].name);
        let t = &tools[b];
        let dup = Tool {
            id: 0,
            code: String::new(),
            name,
            kind: t.kind,
            topic: t.topic,
            maker: t.maker,
            model: t.model.clone(),
            spec: t.spec.clone(),
            supplier: t.supplier,
            dup_of: Some(b),
        };
        tools.push(dup);
    }
    for (i, t) in tools.iter_mut().enumerate() {
        t.code = format!("TL-{:05}", i + 1);
    }

    // Parts.
    let n_parts = 16 + 10 * designs;
    let mut parts: Vec<Part> = Vec::new();
    let mut part_shape = Vec::new();
    for i in 0..n_parts {
        let kind = r.index(PART_KINDS.len());
        let d = *r.pick(&[5i64, 6, 8, 10, 12]);
        let l = *r.pick(&[10i64, 12, 16, 20, 25, 30, 35, 40]);
        let comp = r.index(COMPONENTS.len());
        let (name, size) = part_name(kind, d, l, comp);
        part_shape.push((kind, d, l));
        parts.push(Part {
            id: 0,
            code: String::new(),
            name,
            part_number: format!("PN-{:05}-{:02}", 10_000 + (i * 7919) % 90_000, r.below(100)),
            material: *r.pick(PART_KINDS[kind].1),
            size,
            comp,
            supplier: if i < n_sup { i } else { r.index(n_sup) },
            models: effects(&mut r, n_models),
            dup_of: None,
        });
    }
    // Look-alikes: the same kind of part in another size.
    let mut extra = 0;
    for _ in 0..(n_parts / 12).max(1) {
        let b = r.index(n_parts);
        let (kind, d, l) = part_shape[b];
        if !PART_KINDS[kind].0.contains("{l}") {
            continue;
        }
        let l2 = if l >= 35 { l - 5 } else { l + 5 };
        let (name, size) = part_name(kind, d, l2, parts[b].comp);
        let i = n_parts + extra;
        extra += 1;
        lookalikes.push((b, parts.len(), "Part", PART_DIFF));
        let p = Part {
            id: 0,
            code: String::new(),
            name,
            part_number: format!("PN-{:05}-{:02}", 10_000 + (i * 7919) % 90_000, r.below(100)),
            material: parts[b].material,
            size,
            comp: parts[b].comp,
            supplier: parts[b].supplier,
            models: effects(&mut r, n_models),
            dup_of: None,
        };
        parts.push(p);
    }
    let originals = parts.len();
    for _ in 0..(originals / 20).max(1) {
        let b = r.index(originals);
        let name = variant(&mut r, &parts[b].name);
        let p = &parts[b];
        let dup = Part {
            id: 0,
            code: String::new(),
            name,
            part_number: p.part_number.clone(),
            material: p.material,
            size: p.size.clone(),
            comp: p.comp,
            supplier: p.supplier,
            models: p.models.clone(),
            dup_of: Some(b),
        };
        parts.push(dup);
    }
    for (i, p) in parts.iter_mut().enumerate() {
        p.code = format!("IT-{:05}", i + 1);
    }

    // Standard instructions, in clusters about one thing each.
    let n_clusters = 3 + 2 * designs;
    let mut stds: Vec<Std> = Vec::new();
    let mut clusters = Vec::new();
    for c in 0..n_clusters {
        let topic = if c < TOPICS.len() {
            c
        } else {
            r.index(TOPICS.len())
        };
        let comp = r.index(COMPONENTS.len());
        let spec = spec_value(&mut r, topic);
        let size = r.range(3, 6) as usize;
        let mut members: Vec<usize> = Vec::new();
        for _ in 0..size {
            let mut tries = 0;
            let (title, text) = loop {
                let (title, text) = instruction(&mut r, topic, comp, spec);
                tries += 1;
                if tries > 10 || members.iter().all(|&m| stds[m].text != text) {
                    break (title, text);
                }
            };
            members.push(stds.len());
            stds.push(Std {
                id: 0,
                number: String::new(),
                title,
                text,
                topic,
                comp,
                cluster: c,
                models: effects(&mut r, n_models),
                dup_of: None,
            });
        }
        clusters.push(members);
    }
    let originals = stds.len();
    for _ in 0..(originals / 12).max(1) {
        let b = r.index(originals);
        let title = variant(&mut r, &stds[b].title);
        let s = &stds[b];
        let dup = Std {
            id: 0,
            number: String::new(),
            title,
            text: s.text.clone(),
            topic: s.topic,
            comp: s.comp,
            cluster: s.cluster,
            models: s.models.clone(),
            dup_of: Some(b),
        };
        clusters[s.cluster].push(stds.len());
        stds.push(dup);
    }
    for (i, s) in stds.iter_mut().enumerate() {
        s.number = format!("SWI-{:05}", i + 1);
    }

    let mut tools_by_topic = vec![Vec::new(); TOPICS.len()];
    for (i, t) in tools.iter().enumerate() {
        if t.dup_of.is_none() {
            tools_by_topic[t.topic].push(i);
        }
    }
    let mut parts_by_comp = vec![Vec::new(); COMPONENTS.len()];
    for (i, p) in parts.iter().enumerate() {
        if p.dup_of.is_none() {
            parts_by_comp[p.comp].push(i);
        }
    }
    let mut stds_by_topic = vec![Vec::new(); TOPICS.len()];
    for (i, s) in stds.iter().enumerate() {
        if s.dup_of.is_none() {
            stds_by_topic[s.topic].push(i);
        }
    }
    Catalog {
        enterprise: 0,
        suppliers: vec![0; n_sup],
        models,
        tools,
        parts,
        stds,
        clusters,
        lookalikes,
        tools_by_topic,
        parts_by_comp,
        stds_by_topic,
    }
}

// ---------------------------------------------------------------- designs

struct ElemD {
    name: String,
    topic: usize,
    tools: Vec<usize>,
    parts: Vec<(usize, i64)>,
    std: Option<usize>,
    /// Earlier elements of the same station this one waits for.
    deps: Vec<usize>,
    /// (title, text) of revision A and, when revised, B.
    wi: Vec<(String, String)>,
    std_time: i64,
}

struct LocD {
    name: String,
    elems: Range<usize>,
}

struct SideD {
    name: &'static str,
    locs: Range<usize>,
}

struct Design {
    name: String,
    sides: Vec<SideD>,
    locs: Vec<LocD>,
    elems: Vec<ElemD>,
    /// Every tool any element uses, ascending.
    tools: Vec<usize>,
    nodes: u64,
}

fn design(seed: u64, d: usize, cat: &Catalog) -> Design {
    let mut r = R::new(seed, S_DESIGN, d as u64);
    let comp = r.index(COMPONENTS.len());
    let comp2 = r.index(COMPONENTS.len());
    let topics: Vec<usize> = r
        .sample(TOPICS.len() as u64, 3)
        .into_iter()
        .map(|t| t as usize)
        .collect();
    let main = TOPICS[topics[0]].name;
    let name = format!("{} {} cell", cap(COMPONENTS[comp]), main);
    let side_names: &[&'static str] = if r.chance(250) {
        &["Centre"]
    } else {
        &["Left", "Right"]
    };
    let mut sides = Vec::new();
    let mut locs: Vec<LocD> = Vec::new();
    let mut elems: Vec<ElemD> = Vec::new();
    for &side in side_names {
        let first_loc = locs.len();
        let n_locs = r.range(1, 3) as usize;
        let mut words = r.sample(LOCATION_WORDS.len() as u64, n_locs);
        r.shuffle(&mut words);
        for w in words {
            let lcomp = if r.chance(600) { comp } else { comp2 };
            let lname = format!("{} {}", cap(COMPONENTS[lcomp]), LOCATION_WORDS[w as usize]);
            let first = elems.len();
            let n = r.range(2, 4) as usize;
            for _ in 0..n {
                let topic = if r.chance(750) {
                    *r.pick(&topics)
                } else {
                    r.index(TOPICS.len())
                };
                let ecomp = if r.chance(700) {
                    lcomp
                } else {
                    r.index(COMPONENTS.len())
                };
                let t = &TOPICS[topic];
                let mut ename = format!(
                    "{} {} {}",
                    r.pick(t.verbs),
                    COMPONENTS[ecomp],
                    r.pick(t.objects)
                );
                let mut k = 2;
                while elems[first..].iter().any(|e| e.name == ename) {
                    ename = format!("{} {} {} {k}", t.verbs[0], COMPONENTS[ecomp], t.objects[0]);
                    k += 1;
                }
                let pool = &cat.tools_by_topic[topic];
                let k = 1 + r.index(2);
                let mut tools: Vec<usize> = r
                    .sample(pool.len() as u64, k)
                    .into_iter()
                    .map(|i| pool[i as usize])
                    .collect();
                tools.sort_unstable();
                let pool: Vec<usize> = if cat.parts_by_comp[ecomp].is_empty() {
                    (0..cat.parts.len())
                        .filter(|&p| cat.parts[p].dup_of.is_none())
                        .collect()
                } else {
                    cat.parts_by_comp[ecomp].clone()
                };
                let k = r.index(3);
                let mut parts: Vec<(usize, i64)> = r
                    .sample(pool.len() as u64, k)
                    .into_iter()
                    .map(|i| (pool[i as usize], r.range(1, 8)))
                    .collect();
                parts.sort_unstable();
                let std = if r.chance(500) && !cat.stds_by_topic[topic].is_empty() {
                    Some(*r.pick(&cat.stds_by_topic[topic]))
                } else {
                    None
                };
                let i = elems.len();
                let lo = i.saturating_sub(5);
                let k = r.index(3);
                let mut deps: Vec<usize> = r
                    .sample((i - lo) as u64, k)
                    .into_iter()
                    .map(|x| lo + x as usize)
                    .collect();
                deps.sort_unstable();
                let revs = if r.chance(400) { 2 } else { 1 };
                let mut wi = Vec::new();
                let spec = spec_value(&mut r, topic);
                let first_text = instruction(&mut r, topic, ecomp, spec);
                let title = first_text.0.clone();
                wi.push(first_text);
                if revs == 2 {
                    let spec = spec_value(&mut r, topic);
                    let (_, text) = instruction(&mut r, topic, ecomp, spec);
                    wi.push((title, text));
                }
                elems.push(ElemD {
                    name: ename,
                    topic,
                    tools,
                    parts,
                    std,
                    deps,
                    wi,
                    std_time: r.range(12, 90),
                });
            }
            locs.push(LocD {
                name: lname,
                elems: first..elems.len(),
            });
        }
        sides.push(SideD {
            name: side,
            locs: first_loc..locs.len(),
        });
    }
    let tools: BTreeSet<usize> = elems.iter().flat_map(|e| e.tools.iter().copied()).collect();
    let nodes = 1
        + sides.len() as u64
        + locs.len() as u64
        + elems.iter().map(|e| 1 + e.wi.len() as u64).sum::<u64>();
    Design {
        name,
        sides,
        locs,
        elems,
        tools: tools.into_iter().collect(),
        nodes,
    }
}

// ------------------------------------------------------------------- plan

struct LineP {
    plant: usize,
    shop: usize,
    number: usize,
    /// Global index of its first station, and how many.
    first: u64,
    stations: u64,
    models: Vec<usize>,
}

struct Plan {
    designs: Vec<Design>,
    weights: Vec<u64>,
    cat: Catalog,
    /// plant -> shop -> line indices
    plants: Vec<Vec<Vec<usize>>>,
    lines: Vec<LineP>,
    stations: u64,
    design_counts: Vec<u64>,
}

enum Dev {
    /// An extra element at the end of location `loc` (an index into the
    /// design's locations), waiting for element `dep`.
    Extra {
        loc: usize,
        topic: usize,
        comp: usize,
        tool: usize,
        dep: usize,
    },
    /// Element `elem` lacks one of its design's tools.
    Missing { elem: usize, tool: usize },
}

/// A station's design and deviation: a function of the seed and the
/// station's global index alone, so it can be drawn twice (once to count,
/// once to write) without being kept.
fn draw(
    seed: u64,
    s: u64,
    plan_weights: &[u64],
    designs: &[Design],
    cat: &Catalog,
) -> (usize, Option<Dev>, R) {
    let mut r = R::new(seed, S_STATION, s);
    let total: u64 = plan_weights.iter().sum();
    let mut x = r.below(total);
    let mut d = 0;
    while x >= plan_weights[d] {
        x -= plan_weights[d];
        d += 1;
    }
    let des = &designs[d];
    let dev = if r.chance(125) {
        if r.chance(500) {
            let loc = r.index(des.locs.len());
            let topic = r.index(TOPICS.len());
            let pool = &cat.tools_by_topic[topic];
            Some(Dev::Extra {
                loc,
                topic,
                comp: r.index(COMPONENTS.len()),
                tool: pool[r.index(pool.len())],
                dep: r.index(des.locs[loc].elems.end),
            })
        } else {
            let elem = r.index(des.elems.len());
            let tool = *r.pick(&des.elems[elem].tools);
            Some(Dev::Missing { elem, tool })
        }
    } else {
        None
    };
    (d, dev, r)
}

fn isqrt(n: u64) -> u64 {
    let mut x = (n as f64).sqrt() as u64;
    while x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x
}

/// How many designs a graph of about `stations` stations has: a handful,
/// growing slowly, never more than 24.
pub fn design_count(stations: u64) -> usize {
    (2 + isqrt(stations) / 3).clamp(3, 24) as usize
}

fn plan(cfg: &Config) -> Plan {
    let seed = cfg.seed;
    let guess = (cfg.nodes.saturating_sub(150) / 45).max(2);
    let n_designs = design_count(guess);
    let cat = catalog(seed, n_designs);
    let designs: Vec<Design> = (0..n_designs).map(|d| design(seed, d, &cat)).collect();
    let weights: Vec<u64> = (0..n_designs).map(|d| 1000 / (d as u64 + 1)).collect();
    let total_w: u64 = weights.iter().sum();
    let avg = designs
        .iter()
        .zip(&weights)
        .map(|(d, w)| d.nodes * w)
        .sum::<u64>()
        / total_w;
    let cat_nodes = 1
        + cat.suppliers.len()
        + cat.models.len()
        + cat.tools.len()
        + cat.parts.len()
        + cat.stds.len();
    // Per station: its own nodes, about 1.5 work orders, and its share of
    // plants, shops and lines.
    let stations = (cfg.nodes.saturating_sub(cat_nodes as u64) * 10 / (avg * 10 + 17)).max(1);

    let mut r = R::new(seed, S_PLAN, 0);
    let mut plants = Vec::new();
    let mut lines = Vec::new();
    let mut placed = 0u64;
    while placed < stations {
        let p = plants.len();
        let mut shops = Vec::new();
        for s in 0..r.range(1, 4) as usize {
            if placed >= stations {
                break;
            }
            let mut ls = Vec::new();
            for l in 0..r.range(1, 4) as usize {
                if placed >= stations {
                    break;
                }
                let n = (r.range(4, 16) as u64).min(stations - placed);
                let k = r.range(1, 3) as usize;
                let mut models: Vec<usize> = r
                    .sample(cat.models.len() as u64, k)
                    .into_iter()
                    .map(|m| m as usize)
                    .collect();
                models.sort_unstable();
                ls.push(lines.len());
                lines.push(LineP {
                    plant: p,
                    shop: s,
                    number: l + 1,
                    first: placed,
                    stations: n,
                    models,
                });
                placed += n;
            }
            shops.push(ls);
        }
        plants.push(shops);
    }
    let mut design_counts = vec![0u64; n_designs];
    for s in 0..stations {
        let (d, _, _) = draw(seed, s, &weights, &designs, &cat);
        design_counts[d] += 1;
    }
    Plan {
        designs,
        weights,
        cat,
        plants,
        lines,
        stations,
        design_counts,
    }
}

// ---------------------------------------------------------------- writing

const PLANT_NAMES: &[&str] = &[
    "Ashford",
    "Brightwater",
    "Calder",
    "Dunmore Vale",
    "Easton",
    "Fairhaven",
    "Glenrock",
    "Hollins",
];
const SHOP_NAMES: &[&str] = &[
    "Body shop",
    "Paint shop",
    "Trim shop",
    "Final assembly",
    "Powertrain shop",
    "Chassis shop",
];

struct Out<'a> {
    g: &'a mut dyn Write,
    t: &'a mut dyn Write,
    next_node: u64,
    next_edge: u64,
    labels: BTreeMap<&'static str, u64>,
    types: BTreeMap<&'static str, u64>,
    truth: u64,
    line: String,
}

impl<'a> Out<'a> {
    fn node(&mut self, label: &'static str, props: Obj) -> io::Result<u64> {
        let id = self.next_node;
        self.next_node += 1;
        *self.labels.entry(label).or_insert(0) += 1;
        self.line.clear();
        self.line.push_str("{\"type\":\"node\",\"id\":");
        self.line.push_str(&id.to_string());
        self.line.push_str(",\"labels\":[");
        esc(label, &mut self.line);
        self.line.push_str("],\"props\":");
        self.line.push_str(&props.done());
        self.line.push_str("}\n");
        self.g.write_all(self.line.as_bytes())?;
        Ok(id)
    }

    fn edge(&mut self, ty: &'static str, from: u64, to: u64, props: Obj) -> io::Result<u64> {
        let id = self.next_edge;
        self.next_edge += 1;
        *self.types.entry(ty).or_insert(0) += 1;
        self.line.clear();
        self.line.push_str("{\"type\":\"edge\",\"id\":");
        self.line.push_str(&id.to_string());
        self.line.push_str(",\"label\":");
        esc(ty, &mut self.line);
        self.line
            .push_str(&format!(",\"from\":{from},\"to\":{to},\"props\":"));
        self.line.push_str(&props.done());
        self.line.push_str("}\n");
        self.g.write_all(self.line.as_bytes())?;
        Ok(id)
    }

    fn truth(&mut self, rec: Obj) -> io::Result<()> {
        self.truth += 1;
        let mut s = rec.done();
        s.push('\n');
        self.t.write_all(s.as_bytes())
    }
}

fn effect_props(e: &Effect) -> Obj {
    let o = Obj::new().s("from", &date(e.1));
    match e.2 {
        Some(to) => o.s("to", &date(to)),
        None => o,
    }
}

/// One expected answer: ids and names, ascending by id.
#[derive(Clone)]
struct Answer {
    ids: Vec<u64>,
    names: Vec<String>,
}

impl Answer {
    fn new(mut pairs: Vec<(u64, String)>) -> Answer {
        pairs.sort();
        pairs.dedup();
        Answer {
            ids: pairs.iter().map(|p| p.0).collect(),
            names: pairs.into_iter().map(|p| p.1).collect(),
        }
    }
}

struct Question {
    kind: &'static str,
    text: String,
    query: String,
    anchors: Vec<u64>,
    answer: Answer,
    count: u64,
    complete: bool,
    /// For a temporal question, the date asked about.
    at: Option<String>,
}

#[derive(Default)]
struct Targets {
    lookup: BTreeSet<u64>,
    aggregate: BTreeSet<u64>,
    temporal: BTreeSet<u64>,
    multihop: BTreeSet<usize>,
    /// Per target design, the ids of its first stations.
    members: BTreeMap<usize, Vec<u64>>,
    /// Per target line: standards its elements follow.
    line_stds: BTreeMap<usize, BTreeSet<usize>>,
    /// Per target line: its node id and code.
    line_ids: BTreeMap<usize, (u64, String)>,
    lookup_q: Vec<(u64, Question)>,
    aggregate_q: Vec<(u64, u64, String, usize)>,
    temporal_q: Vec<(u64, Question)>,
}

/// Write the graph, the truth and the questions. Returns what was written.
pub fn generate(
    cfg: &Config,
    graph: &mut dyn Write,
    truth: &mut dyn Write,
    questions: &mut dyn Write,
) -> io::Result<Summary> {
    let mut plan = plan(cfg);
    let seed = cfg.seed;
    let mut o = Out {
        g: graph,
        t: truth,
        next_node: 1,
        next_edge: 1,
        labels: BTreeMap::new(),
        types: BTreeMap::new(),
        truth: 0,
        line: String::new(),
    };

    let mut qr = R::new(seed, S_QUESTION, 0);
    let mut tg = Targets {
        lookup: qr.sample(plan.stations, PER_KIND).into_iter().collect(),
        aggregate: qr.sample(plan.stations, PER_KIND).into_iter().collect(),
        temporal: qr.sample(plan.stations, PER_KIND).into_iter().collect(),
        multihop: qr
            .sample(plan.lines.len() as u64, PER_KIND)
            .into_iter()
            .map(|l| l as usize)
            .collect(),
        ..Targets::default()
    };
    for &s in &tg.aggregate {
        let (d, _, _) = draw(seed, s, &plan.weights, &plan.designs, &plan.cat);
        tg.members.entry(d).or_default();
    }
    for &l in &tg.multihop {
        tg.line_stds.insert(l, BTreeSet::new());
    }

    o.truth(
        Obj::new()
            .s("kind", "meta")
            .u("format", FORMAT as u64)
            .u("seed", seed)
            .u("nodes_target", cfg.nodes)
            .b("faults", cfg.faults)
            .u("stations", plan.stations)
            .u("lines", plan.lines.len() as u64)
            .u("designs", plan.designs.len() as u64),
    )?;

    write_catalog(&mut o, &mut plan.cat)?;
    let cat = &plan.cat;
    for (d, des) in plan.designs.iter().enumerate() {
        let tools: Vec<u64> = des.tools.iter().map(|&t| cat.tools[t].id).collect();
        let mut parts: BTreeSet<u64> = BTreeSet::new();
        let mut stds: BTreeSet<u64> = BTreeSet::new();
        for e in &des.elems {
            parts.extend(e.parts.iter().map(|p| cat.parts[p.0].id));
            stds.extend(e.std.iter().map(|&s| cat.stds[s].id));
        }
        let parts: Vec<u64> = parts.into_iter().collect();
        let stds: Vec<u64> = stds.into_iter().collect();
        o.truth(
            Obj::new()
                .s("kind", "design")
                .u("design", d as u64)
                .s("name", &des.name)
                .u("stations", plan.design_counts[d])
                .u("sides", des.sides.len() as u64)
                .u("locations", des.locs.len() as u64)
                .u("elements", des.elems.len() as u64)
                .u("nodes", des.nodes)
                .ids("tools", &tools)
                .ids("parts", &parts)
                .ids("standards", &stds),
        )?;
    }

    // Tool sharing, counted as the stations are written.
    let nt = cat.tools.len();
    let mut tool_elems = vec![0u64; nt];
    let mut tool_stations = vec![0u64; nt];
    let mut tool_lines = vec![0u64; nt];
    let mut tool_last = vec![(u64::MAX, usize::MAX); nt];
    let mut tool_designs = vec![BTreeSet::new(); nt];
    for (d, des) in plan.designs.iter().enumerate() {
        for &t in &des.tools {
            tool_designs[t].insert(d as u64);
        }
    }
    let mut faults: Vec<Obj> = Vec::new();
    let mut lookalike_pairs: BTreeMap<(usize, usize), u64> = BTreeMap::new();

    let enterprise = cat.enterprise;
    for (p, shops) in plan.plants.iter().enumerate() {
        let pcode = format!("P{:02}", p + 1);
        let pname = if p < PLANT_NAMES.len() {
            format!("{} plant", PLANT_NAMES[p])
        } else {
            format!("Plant {}", p + 1)
        };
        let plant = o.node(
            "Plant",
            Obj::new()
                .s("name", &pname)
                .s("code", &pcode)
                .s("country", COUNTRIES[p % COUNTRIES.len()]),
        )?;
        o.edge("HAS_PLANT", enterprise, plant, Obj::new())?;
        for (s, lines) in shops.iter().enumerate() {
            let scode = format!("{pcode}-S{}", s + 1);
            let shop = o.node(
                "Shop",
                Obj::new()
                    .s(
                        "name",
                        &format!("{} {}", SHOP_NAMES[s % SHOP_NAMES.len()], s + 1),
                    )
                    .s("code", &scode),
            )?;
            o.edge("HAS_SHOP", plant, shop, Obj::new())?;
            for &l in lines {
                let lp = &plan.lines[l];
                debug_assert_eq!((lp.plant, lp.shop), (p, s));
                let lcode = format!("{scode}-L{}", lp.number);
                let mut lr = R::new(seed, S_LINE, l as u64);
                let line = o.node(
                    "Line",
                    Obj::new()
                        .s(
                            "name",
                            &format!(
                                "Line {} {}",
                                lp.number,
                                cap(COMPONENTS[lr.index(COMPONENTS.len())])
                            ),
                        )
                        .s("code", &lcode)
                        .i("rate_per_hour", lr.range(20, 70)),
                )?;
                o.edge("HAS_LINE", shop, line, Obj::new())?;
                if tg.line_stds.contains_key(&l) {
                    tg.line_ids.insert(l, (line, lcode.clone()));
                }
                for &m in &lp.models {
                    let from = DAY0 + lr.range(0, 300);
                    o.edge(
                        "BUILDS",
                        line,
                        cat.models[m].id,
                        Obj::new().s("from", &date(from)),
                    )?;
                }
                for k in 0..lp.stations {
                    let s_idx = lp.first + k;
                    write_station(
                        &mut o,
                        &plan,
                        seed,
                        cfg.faults,
                        s_idx,
                        k,
                        l,
                        line,
                        &lcode,
                        &mut tg,
                        &mut faults,
                        &mut lookalike_pairs,
                        &mut |t: usize, st: u64| {
                            tool_elems[t] += 1;
                            if tool_last[t].0 != st {
                                tool_stations[t] += 1;
                                tool_last[t].0 = st;
                            }
                            if tool_last[t].1 != l {
                                tool_lines[t] += 1;
                                tool_last[t].1 = l;
                            }
                        },
                    )?;
                }
                // Work orders for the line.
                let n = (lp.stations * 3).div_ceil(2);
                for w in 0..n {
                    let m = lp.models[lr.index(lp.models.len())];
                    let wo = o.node(
                        "WorkOrder",
                        Obj::new()
                            .s("number", &format!("WO-{}-{:05}", lcode, w + 1))
                            .i("quantity", lr.range(1, 40) * 10)
                            .s("due", &date(DAY0 + lr.range(30, 1400)))
                            .s("status", *lr.pick(STATUSES)),
                    )?;
                    o.edge("FOR_MODEL", wo, cat.models[m].id, Obj::new())?;
                    o.edge("ON_LINE", wo, line, Obj::new())?;
                }
            }
        }
    }

    // Orphans: nodes that belong to nothing.
    if cfg.faults {
        let mut fr = R::new(seed, S_FAULT, u64::MAX);
        let n = 1 + plan.stations / 40;
        for i in 0..n {
            let (label, props): (&'static str, Obj) = match i % 3 {
                0 => (
                    "Tool",
                    Obj::new().s("code", &format!("TL-9{:04}", i)).s(
                        "name",
                        &format!("{} {}", cap(TOPICS[fr.index(TOPICS.len())].tools[0]), i),
                    ),
                ),
                1 => (
                    "Part",
                    Obj::new()
                        .s("code", &format!("IT-9{:04}", i))
                        .s("name", &format!("Spare clip {}", fr.range(10, 99))),
                ),
                _ => (
                    "WorkInstruction",
                    Obj::new()
                        .s("number", &format!("WI-ORPHAN-{:04}", i))
                        .s("title", "Draft instruction")
                        .s("scope", "local"),
                ),
            };
            let id = o.node(label, props)?;
            faults.push(
                Obj::new()
                    .s("kind", "fault")
                    .s("fault", "orphan")
                    .u("id", id)
                    .s("label", label),
            );
        }
    }

    // Tool sharing.
    let cat = &plan.cat;
    for (t, tool) in cat.tools.iter().enumerate() {
        let designs: Vec<u64> = tool_designs[t].iter().copied().collect();
        o.truth(
            Obj::new()
                .s("kind", "tool")
                .u("id", tool.id)
                .s("name", &tool.name)
                .ids("designs", &designs)
                .u("elements", tool_elems[t])
                .u("stations", tool_stations[t])
                .u("lines", tool_lines[t]),
        )?;
    }
    for f in faults {
        o.truth(f)?;
    }

    // Questions.
    let mut qs: Vec<Question> = Vec::new();
    for (_, q) in std::mem::take(&mut tg.lookup_q) {
        qs.push(q);
    }
    for (l, set) in &tg.line_stds {
        let lp = &plan.lines[*l];
        let mut models = lp.models.clone();
        let mut lr = R::new(seed, S_QUESTION, 1_000_000 + *l as u64);
        lr.shuffle(&mut models);
        let (line_id, lcode) = tg.line_ids[l].clone();
        for m in models {
            let ans: Vec<(u64, String)> = set
                .iter()
                .filter(|&&s| cat.stds[s].models.iter().any(|e| e.0 == m))
                .map(|&s| (cat.stds[s].id, cat.stds[s].title.clone()))
                .collect();
            if ans.is_empty() {
                continue;
            }
            let answer = Answer::new(ans);
            qs.push(Question {
                kind: "multihop",
                text: format!(
                    "Which standard work instructions apply to model {} on line {}?",
                    cat.models[m].name, lcode
                ),
                query: format!("{} {}", cat.models[m].name, lcode),
                anchors: vec![line_id, cat.models[m].id],
                count: answer.ids.len() as u64,
                answer,
                complete: true,
                at: None,
            });
            break;
        }
    }
    for (_, sid, code, d) in std::mem::take(&mut tg.aggregate_q) {
        let members = &tg.members[&d];
        let count = plan.design_counts[d];
        qs.push(Question {
            kind: "aggregate",
            text: format!(
                "How many stations, {code} included, are built to the same design as station {code}?"
            ),
            query: code.clone(),
            anchors: vec![sid],
            answer: Answer {
                ids: members.clone(),
                names: Vec::new(),
            },
            count,
            complete: count as usize <= MEMBER_CAP,
            at: None,
        });
    }
    for (_, q) in std::mem::take(&mut tg.temporal_q) {
        qs.push(q);
    }
    let picks = qr.sample(cat.clusters.len() as u64, PER_KIND);
    for c in picks {
        let members = &cat.clusters[c as usize];
        let me = &cat.stds[members[0]];
        let ans: Vec<(u64, String)> = members[1..]
            .iter()
            .map(|&s| (cat.stds[s].id, cat.stds[s].title.clone()))
            .collect();
        let answer = Answer::new(ans);
        qs.push(Question {
            kind: "similarity",
            text: format!(
                "Which standard work instructions are about the same thing as {} ({})?",
                me.number, me.title
            ),
            query: me.title.clone(),
            anchors: vec![me.id],
            count: answer.ids.len() as u64,
            answer,
            complete: true,
            at: None,
        });
    }
    let mut per_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for q in &qs {
        let n = per_kind.entry(q.kind).or_insert(0);
        *n += 1;
        let mut o = Obj::new()
            .s("id", &format!("{}-{}", q.kind, n))
            .s("kind", q.kind)
            .s("text", &q.text)
            .s("query", &q.query)
            .ids("anchors", &q.anchors);
        if let Some(at) = &q.at {
            o = o.s("at", at);
        }
        let mut s = o
            .ids("ids", &q.answer.ids)
            .strs("names", &q.answer.names)
            .u("count", q.count)
            .b("complete", q.complete)
            .done();
        s.push('\n');
        questions.write_all(s.as_bytes())?;
    }

    let nodes = o.next_node - 1;
    let edges = o.next_edge - 1;
    let labels: Vec<String> = o
        .labels
        .iter()
        .map(|(k, v)| Obj::new().s("label", k).u("count", *v).done())
        .collect();
    let types: Vec<String> = o
        .types
        .iter()
        .map(|(k, v)| Obj::new().s("type", k).u("count", *v).done())
        .collect();
    o.truth(
        Obj::new()
            .s("kind", "summary")
            .u("nodes", nodes)
            .u("edges", edges)
            .u("questions", qs.len() as u64)
            .raw("labels", &raw_list(&labels))
            .raw("types", &raw_list(&types)),
    )?;
    Ok(Summary {
        nodes,
        edges,
        stations: plan.stations,
        lines: plan.lines.len() as u64,
        designs: plan.designs.len(),
        questions: qs.len(),
        truth_records: o.truth,
    })
}

fn write_catalog(o: &mut Out, cat: &mut Catalog) -> io::Result<()> {
    cat.enterprise = o.node(
        "Enterprise",
        Obj::new().s("name", "Kestrel Motor Works").s("code", "KMW"),
    )?;
    let ent = cat.enterprise;
    for i in 0..cat.suppliers.len() {
        cat.suppliers[i] = o.node(
            "Supplier",
            Obj::new()
                .s("name", SUPPLIERS[i])
                .s("code", &format!("SUP-{:03}", i + 1))
                .s("country", COUNTRIES[(i * 3) % COUNTRIES.len()]),
        )?;
    }
    for m in cat.models.iter_mut() {
        m.id = o.node(
            "ProductModel",
            Obj::new()
                .s("name", &m.name)
                .s("code", &m.code)
                .s("body", m.body)
                .s("launched", &date(m.launched)),
        )?;
        o.edge("OFFERED_BY", m.id, ent, Obj::new())?;
    }
    for t in cat.tools.iter_mut() {
        t.id = o.node(
            "Tool",
            Obj::new()
                .s("name", &t.name)
                .s("code", &t.code)
                .s("kind", t.kind)
                .s("maker", t.maker)
                .s("model", &t.model)
                .s("spec", &t.spec),
        )?;
        o.edge("SUPPLIED_BY", t.id, cat.suppliers[t.supplier], Obj::new())?;
    }
    for p in cat.parts.iter_mut() {
        p.id = o.node(
            "Part",
            Obj::new()
                .s("name", &p.name)
                .s("code", &p.code)
                .s("part_number", &p.part_number)
                .s("material", p.material)
                .s("size", &p.size),
        )?;
        o.edge("SUPPLIED_BY", p.id, cat.suppliers[p.supplier], Obj::new())?;
        for e in &p.models {
            o.edge("APPLIES_TO", p.id, cat.models[e.0].id, effect_props(e))?;
        }
    }
    for s in cat.stds.iter_mut() {
        s.id = o.node(
            "WorkInstruction",
            Obj::new()
                .s("number", &s.number)
                .s("title", &s.title)
                .s("text", &s.text)
                .s("revision", "A")
                .s("scope", "standard"),
        )?;
        o.edge("ISSUED_BY", s.id, ent, Obj::new())?;
        for e in &s.models {
            o.edge("APPLIES_TO", s.id, cat.models[e.0].id, effect_props(e))?;
        }
    }
    // Truth about the catalogue.
    let dup = |label: &str, a: u64, b: u64, agree: &[&str], differ: &[&str]| {
        Obj::new()
            .s("kind", "duplicate")
            .s("label", label)
            .u("a", a)
            .u("b", b)
            .strs("agree_on", agree)
            .strs("differ_on", differ)
    };
    for t in &cat.tools {
        if let Some(b) = t.dup_of {
            o.truth(dup(
                "Tool",
                cat.tools[b].id,
                t.id,
                &["kind", "maker", "model", "spec"],
                &["code", "name"],
            ))?;
        }
    }
    for p in &cat.parts {
        if let Some(b) = p.dup_of {
            o.truth(dup(
                "Part",
                cat.parts[b].id,
                p.id,
                &["part_number", "material", "size"],
                &["code", "name"],
            ))?;
        }
    }
    for s in &cat.stds {
        if let Some(b) = s.dup_of {
            o.truth(dup(
                "WorkInstruction",
                cat.stds[b].id,
                s.id,
                &["text", "revision", "scope"],
                &["number", "title"],
            ))?;
        }
    }
    for &(a, b, label, differ) in &cat.lookalikes {
        let (ia, ib) = match label {
            "Tool" => (cat.tools[a].id, cat.tools[b].id),
            _ => (cat.parts[a].id, cat.parts[b].id),
        };
        o.truth(
            Obj::new()
                .s("kind", "lookalike")
                .s("label", label)
                .u("a", ia)
                .u("b", ib)
                .strs("differ_on", differ)
                .s("why", "same kind and maker or shape, another specification"),
        )?;
    }
    for (c, members) in cat.clusters.iter().enumerate() {
        let ids: Vec<u64> = members.iter().map(|&m| cat.stds[m].id).collect();
        let first = &cat.stds[members[0]];
        o.truth(
            Obj::new()
                .s("kind", "topic_group")
                .s("group", &format!("G{:03}", c + 1))
                .s("topic", TOPICS[first.topic].name)
                .s("component", COMPONENTS[first.comp])
                .ids("members", &ids),
        )?;
    }
    for s in &cat.stds {
        o.truth(
            Obj::new()
                .s("kind", "topic")
                .u("id", s.id)
                .s("topic", TOPICS[s.topic].name)
                .s("group", &format!("G{:03}", s.cluster + 1)),
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_station(
    o: &mut Out,
    plan: &Plan,
    seed: u64,
    plant_faults: bool,
    s: u64,
    k: u64,
    l: usize,
    line: u64,
    lcode: &str,
    tg: &mut Targets,
    faults: &mut Vec<Obj>,
    lookalikes: &mut BTreeMap<(usize, usize), u64>,
    use_tool: &mut dyn FnMut(usize, u64),
) -> io::Result<()> {
    let cat = &plan.cat;
    let (d, dev, mut r) = draw(seed, s, &plan.weights, &plan.designs, cat);
    let des = &plan.designs[d];
    let mut fr = R::new(seed, S_FAULT, s);
    let dup_edge = plant_faults && fr.chance(80);
    let code = format!("{lcode}-ST{:03}", (k + 1) * 10);
    let name = format!("Station {:03} {}", (k + 1) * 10, des.name);
    let commissioned = DAY0 + r.range(0, 900);
    let station = o.node(
        "Station",
        Obj::new()
            .s("name", &name)
            .s("code", &code)
            .u("number", s + 1)
            .i("takt_seconds", r.range(45, 120))
            .s("commissioned", &date(commissioned)),
    )?;
    o.edge("HAS_STATION", line, station, Obj::new())?;

    let mut elem_ids: Vec<u64> = vec![0; des.elems.len()];
    let mut order: Vec<u64> = Vec::new();
    let mut tools_used: BTreeSet<usize> = BTreeSet::new();
    let mut deviations: Vec<String> = Vec::new();
    let mut first_tool_edge: Option<(u64, u64, u64)> = None;
    // Temporal target: an element with two revisions if there is one.
    let temporal_elem = if tg.temporal.contains(&s) {
        let two: Vec<usize> = (0..des.elems.len())
            .filter(|&e| des.elems[e].wi.len() == 2)
            .collect();
        Some(if two.is_empty() {
            0
        } else {
            two[r.index(two.len())]
        })
    } else {
        None
    };
    let mut temporal_q: Option<Question> = None;

    for side in &des.sides {
        let letter = &side.name[..1];
        let side_id = o.node(
            "Side",
            Obj::new()
                .s("name", side.name)
                .s("code", &format!("{code}-{letter}")),
        )?;
        o.edge("HAS_SIDE", station, side_id, Obj::new())?;
        for li in side.locs.clone() {
            let loc = &des.locs[li];
            let loc_id = o.node(
                "WorkLocation",
                Obj::new().s("name", &loc.name).s(
                    "code",
                    &format!("{code}-{letter}{}", li - side.locs.start + 1),
                ),
            )?;
            o.edge("HAS_LOCATION", side_id, loc_id, Obj::new())?;
            for e in loc.elems.clone() {
                let el = &des.elems[e];
                let ecode = format!("{code}-WE{:02}", e + 1);
                let we = o.node(
                    "WorkElement",
                    Obj::new()
                        .s("name", &el.name)
                        .s("code", &ecode)
                        .u("seq", e as u64 + 1)
                        .i("std_time_seconds", el.std_time + r.range(-3, 3)),
                )?;
                elem_ids[e] = we;
                order.push(we);
                o.edge("HAS_ELEMENT", loc_id, we, Obj::new())?;
                // Revisions of its own instruction.
                let a_from = commissioned + r.range(0, 30);
                let b_from = a_from + r.range(60, 700);
                let mut wi_ids = Vec::new();
                for (rev, (title, text)) in el.wi.iter().enumerate() {
                    let letter = if rev == 0 { "A" } else { "B" };
                    let wi = o.node(
                        "WorkInstruction",
                        Obj::new()
                            .s("number", &format!("WI-{:06}-{:02}{letter}", s + 1, e + 1))
                            .s("title", title)
                            .s("text", text)
                            .s("revision", letter)
                            .s("scope", "local"),
                    )?;
                    let props = if rev == 0 {
                        let p = Obj::new().s("from", &date(a_from));
                        if el.wi.len() == 2 {
                            p.s("to", &date(b_from))
                        } else {
                            p
                        }
                    } else {
                        Obj::new().s("from", &date(b_from))
                    };
                    o.edge("HAS_INSTRUCTION", we, wi, props)?;
                    o.truth(
                        Obj::new()
                            .s("kind", "topic")
                            .u("id", wi)
                            .s("topic", TOPICS[el.topic].name),
                    )?;
                    wi_ids.push((wi, title.clone(), letter));
                    // Look-alikes that are different things: the same
                    // instruction of one design at two stations.
                    let key = (d, e * 2 + rev);
                    match lookalikes.get(&key) {
                        None => {
                            lookalikes.insert(key, wi);
                        }
                        Some(&first) if first != 0 => {
                            o.truth(
                                Obj::new()
                                    .s("kind", "lookalike")
                                    .s("label", "WorkInstruction")
                                    .u("a", first)
                                    .u("b", wi)
                                    .strs("differ_on", &["number"])
                                    .s("why", "the same design element at two stations"),
                            )?;
                            lookalikes.insert(key, 0);
                        }
                        _ => {}
                    }
                }
                if temporal_elem == Some(e) {
                    let (day, pick) = if wi_ids.len() == 2 && r.chance(500) {
                        (b_from + r.range(1, 200), 1)
                    } else if wi_ids.len() == 2 {
                        (a_from + r.range(0, b_from - a_from - 1), 0)
                    } else {
                        (a_from + r.range(1, 400), 0)
                    };
                    let (wid, title, _) = &wi_ids[pick];
                    temporal_q = Some(Question {
                        kind: "temporal",
                        text: format!(
                            "Which work instruction was in effect for work element {ecode} on {}?",
                            date(day)
                        ),
                        query: ecode.clone(),
                        anchors: vec![we],
                        answer: Answer::new(vec![(*wid, title.clone())]),
                        count: 1,
                        complete: true,
                        at: Some(date(day)),
                    });
                }
                // What it uses.
                for &t in &el.tools {
                    if let Some(Dev::Missing { elem, tool }) = &dev {
                        if *elem == e && *tool == t {
                            continue;
                        }
                    }
                    let eid = o.edge("USES_TOOL", we, cat.tools[t].id, Obj::new())?;
                    use_tool(t, s);
                    tools_used.insert(t);
                    if first_tool_edge.is_none() {
                        first_tool_edge = Some((eid, we, cat.tools[t].id));
                        if dup_edge {
                            let e2 = o.edge("USES_TOOL", we, cat.tools[t].id, Obj::new())?;
                            faults.push(
                                Obj::new()
                                    .s("kind", "fault")
                                    .s("fault", "parallel_edge")
                                    .s("type", "USES_TOOL")
                                    .u("from", we)
                                    .u("to", cat.tools[t].id)
                                    .ids("edges", &[eid, e2]),
                            );
                        }
                    }
                }
                for &(p, qty) in &el.parts {
                    o.edge(
                        "CONSUMES",
                        we,
                        cat.parts[p].id,
                        Obj::new().i("quantity", qty),
                    )?;
                }
                if let Some(st) = el.std {
                    o.edge("FOLLOWS", we, cat.stds[st].id, Obj::new())?;
                    if let Some(set) = tg.line_stds.get_mut(&l) {
                        set.insert(st);
                    }
                }
                for &dep in &el.deps {
                    o.edge("DEPENDS_ON", we, elem_ids[dep], Obj::new())?;
                }
            }
            if let Some(Dev::Extra {
                loc,
                topic,
                comp,
                tool,
                dep,
            }) = &dev
            {
                if *loc != li {
                    continue;
                }
                let t = &TOPICS[*topic];
                let mut ename = format!(
                    "{} {} {}",
                    r.pick(t.verbs),
                    COMPONENTS[*comp],
                    r.pick(t.objects)
                );
                while des.elems[loc_range(des, li)]
                    .iter()
                    .any(|x| x.name == ename)
                {
                    ename.push_str(" extra");
                }
                let seq = des.elems.len() + 1;
                let ecode = format!("{code}-WE{seq:02}");
                let we = o.node(
                    "WorkElement",
                    Obj::new()
                        .s("name", &ename)
                        .s("code", &ecode)
                        .u("seq", seq as u64)
                        .i("std_time_seconds", r.range(12, 90)),
                )?;
                order.push(we);
                o.edge("HAS_ELEMENT", loc_id, we, Obj::new())?;
                let spec = spec_value(&mut r, *topic);
                let (title, text) = instruction(&mut r, *topic, *comp, spec);
                let wi = o.node(
                    "WorkInstruction",
                    Obj::new()
                        .s("number", &format!("WI-{:06}-{seq:02}A", s + 1))
                        .s("title", &title)
                        .s("text", &text)
                        .s("revision", "A")
                        .s("scope", "local"),
                )?;
                o.edge(
                    "HAS_INSTRUCTION",
                    we,
                    wi,
                    Obj::new().s("from", &date(commissioned + r.range(30, 400))),
                )?;
                o.truth(Obj::new().s("kind", "topic").u("id", wi).s("topic", t.name))?;
                o.edge("USES_TOOL", we, cat.tools[*tool].id, Obj::new())?;
                use_tool(*tool, s);
                tools_used.insert(*tool);
                o.edge("DEPENDS_ON", we, elem_ids[*dep], Obj::new())?;
                deviations.push(
                    Obj::new()
                        .s("type", "extra_element")
                        .u("element", we)
                        .u("location", loc_id)
                        .u("instruction", wi)
                        .u("tool", cat.tools[*tool].id)
                        .u("depends_on", elem_ids[*dep])
                        .done(),
                );
            }
        }
    }
    if let Some(Dev::Missing { elem, tool }) = &dev {
        deviations.push(
            Obj::new()
                .s("type", "missing_tool")
                .u("element", elem_ids[*elem])
                .u("tool", cat.tools[*tool].id)
                .done(),
        );
    }
    let tools: Vec<u64> = tools_used.iter().map(|&t| cat.tools[t].id).collect();
    o.truth(
        Obj::new()
            .s("kind", "station")
            .u("id", station)
            .s("code", &code)
            .u("design", d as u64)
            .u("line", line)
            .raw("deviations", &raw_list(&deviations))
            .ids("order", &order)
            .ids("tools", &tools),
    )?;

    if tg.lookup.contains(&s) {
        let ans: Vec<(u64, String)> = tools_used
            .iter()
            .map(|&t| (cat.tools[t].id, cat.tools[t].name.clone()))
            .collect();
        let answer = Answer::new(ans);
        tg.lookup_q.push((
            s,
            Question {
                kind: "lookup",
                text: format!("Which tools does station {code} use?"),
                query: code.clone(),
                anchors: vec![station],
                count: answer.ids.len() as u64,
                answer,
                complete: true,
                at: None,
            },
        ));
    }
    if let Some(m) = tg.members.get_mut(&d) {
        if m.len() < MEMBER_CAP {
            m.push(station);
        }
    }
    if tg.aggregate.contains(&s) {
        tg.aggregate_q.push((s, station, code.clone(), d));
    }
    if let Some(q) = temporal_q {
        tg.temporal_q.push((s, q));
    }
    Ok(())
}

fn loc_range(des: &Design, li: usize) -> Range<usize> {
    des.locs[li].elems.clone()
}
