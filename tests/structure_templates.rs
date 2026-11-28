//! `structure` templates, paths, plans and expansion: a repeated shape is
//! said once and nothing is lost. Every instance expands to exactly its
//! nodes, edges and values; instances and what they do not cover are the
//! graph; a station that deviates is not an instance of its design; the
//! compact text reads back as the graph; the plan's saving is plain minus
//! compact, measured; and all of it depends on content, not ids, and is the
//! same on both tiers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use glider::ext::context::tokens;
use glider::graph::{Graph, OpenOptions};
use glider::query::{self, QueryResult};
use glider::value::Value;

// ------------------------------------------------------------------ helpers

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "glider-templates-{}-{}.db",
        name,
        std::process::id()
    ));
    cleanup(&p);
    p
}

fn cleanup(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(glider::store::lock_path(p));
    for suffix in ["-wal", "-data", "-tmp", "-hot"] {
        let mut d = p.as_os_str().to_os_string();
        d.push(suffix);
        let d = PathBuf::from(d);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&d);
    }
}

fn q(g: &mut Graph, src: &str) -> QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn err(g: &mut Graph, src: &str) -> String {
    match query::execute(g, src) {
        Ok(_) => panic!("{src}: expected an error"),
        Err(e) => e.to_string(),
    }
}

fn install(g: &mut Graph) {
    q(g, "CREATE EXTENSION IF NOT EXISTS structure");
}

fn text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        v => v.to_string(),
    }
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("not a number: {v:?}"))
}

fn col(r: &QueryResult, name: &str) -> usize {
    r.columns
        .iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("no column {name} in {:?}", r.columns))
}

fn dump(r: &QueryResult) -> String {
    let mut s = format!("{:?}\n", r.columns);
    for row in &r.rows {
        let cells: Vec<String> = row.iter().map(|v| v.to_json()).collect();
        s.push_str(&cells.join("\t"));
        s.push('\n');
    }
    s
}

fn list(xs: &[&str]) -> String {
    let items: Vec<String> = xs.iter().map(|x| format!("'{x}'")).collect();
    format!("[{}]", items.join(", "))
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A value as bytes that differ when the values differ in type or value.
fn exact(v: &Value) -> String {
    match v {
        Value::Float(f) => format!("f{:?}", f.to_bits()),
        Value::Int(i) => format!("i{i}"),
        Value::List(items) => format!(
            "[{}]",
            items.iter().map(exact).collect::<Vec<_>>().join(",")
        ),
        v => format!("{}{}", std::mem::discriminant(v).hash_code(), v.to_json()),
    }
}

trait HashCode {
    fn hash_code(&self) -> u64;
}

impl<T> HashCode for std::mem::Discriminant<T> {
    fn hash_code(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

/// A graph as sets: node -> (labels, props), and the multiset of edges.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
struct Snapshot {
    nodes: BTreeMap<String, (BTreeSet<String>, BTreeMap<String, String>)>,
    edges: BTreeMap<(String, String, String, String), usize>,
}

impl Snapshot {
    fn node(&mut self, h: String, labels: Vec<String>, props: Vec<(String, Value)>) {
        let prev = self.nodes.insert(
            h.clone(),
            (
                labels.into_iter().collect(),
                props.into_iter().map(|(k, v)| (k, exact(&v))).collect(),
            ),
        );
        assert!(prev.is_none(), "node {h} written twice");
    }
    fn edge(&mut self, from: String, t: String, to: String, props: Vec<(String, Value)>) {
        let mut ps: Vec<(String, String)> =
            props.into_iter().map(|(k, v)| (k, exact(&v))).collect();
        ps.sort();
        *self
            .edges
            .entry((from, t, to, format!("{ps:?}")))
            .or_default() += 1;
    }
}

/// The graph as it is, nodes named by id (or by `@`-handle on `identity`).
fn snapshot(g: &Graph, identity: Option<&str>) -> Snapshot {
    let handle = |id: u64| -> String {
        if let Some(k) = identity {
            if let Some(v) = g.node_prop(id, k) {
                let mut s = String::from("@");
                s.push_str(&lit(&v));
                return s;
            }
        }
        format!("n{id}")
    };
    let mut s = Snapshot::default();
    for id in g.node_ids() {
        s.node(handle(id), g.node_labels(id), g.node_props(id));
    }
    for e in g.edge_ids() {
        let r = g.edge(e).unwrap();
        s.edge(
            handle(r.from),
            g.strings.name(r.etype).to_string(),
            handle(r.to),
            g.edge_props(e),
        );
    }
    s
}

/// How the notation writes a value (as `context` and the engine do: JSON
/// strings, floats with a point).
fn lit(v: &Value) -> String {
    match v {
        Value::Float(f) => {
            let s = format!("{f:?}");
            if s.contains(['.', 'e', 'E', 'N', 'n']) {
                s
            } else {
                format!("{s}.0")
            }
        }
        Value::Text(_) => v.to_json(),
        Value::List(items) => format!("[{}]", items.iter().map(lit).collect::<Vec<_>>().join(", ")),
        v => v.to_json(),
    }
}

// ------------------------------------------------------------- a decoder

/// Reads `glider-compact/1` back: a value, a name, a whole text.
struct Lex<'a> {
    s: &'a [u8],
    at: usize,
}

impl<'a> Lex<'a> {
    fn new(s: &'a str) -> Lex<'a> {
        Lex {
            s: s.as_bytes(),
            at: 0,
        }
    }
    fn rest(&self) -> &'a str {
        std::str::from_utf8(&self.s[self.at..]).unwrap()
    }
    fn peek(&self) -> Option<u8> {
        self.s.get(self.at).copied()
    }
    fn eat(&mut self, t: &str) -> bool {
        if self.rest().starts_with(t) {
            self.at += t.len();
            true
        } else {
            false
        }
    }
    fn ws(&mut self) {
        while self.peek() == Some(b' ') {
            self.at += 1;
        }
    }
    fn name(&mut self) -> String {
        if self.peek() == Some(b'`') {
            self.at += 1;
            let mut out = String::new();
            loop {
                if self.eat("``") {
                    out.push('`');
                } else if self.eat("`") {
                    return out;
                } else {
                    let c = self.rest().chars().next().unwrap();
                    out.push(c);
                    self.at += c.len_utf8();
                }
            }
        }
        let start = self.at;
        while let Some(b) = self.peek() {
            if b == b'_' || b.is_ascii_alphanumeric() {
                self.at += 1;
            } else {
                break;
            }
        }
        std::str::from_utf8(&self.s[start..self.at])
            .unwrap()
            .to_string()
    }
    fn string(&mut self) -> String {
        // A JSON string: find its end, then let the JSON parser read it.
        let start = self.at;
        assert_eq!(self.peek(), Some(b'"'));
        self.at += 1;
        loop {
            match self.peek().unwrap() {
                b'\\' => self.at += 2,
                b'"' => {
                    self.at += 1;
                    break;
                }
                _ => self.at += 1,
            }
        }
        let src = std::str::from_utf8(&self.s[start..self.at]).unwrap();
        match glider::value::parse_json(src).unwrap() {
            glider::value::Json::Text(s) => s,
            other => panic!("{other:?}"),
        }
    }
    fn value(&mut self) -> Value {
        match self.peek().unwrap() {
            b'"' => Value::Text(self.string()),
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    if self.eat("]") {
                        return Value::List(items);
                    }
                    items.push(self.value());
                    self.ws();
                    self.eat(",");
                }
            }
            _ => {
                for (w, v) in [
                    ("null", Value::Null),
                    ("true", Value::Bool(true)),
                    ("false", Value::Bool(false)),
                    ("NaN", Value::Float(f64::NAN)),
                    ("-inf", Value::Float(f64::NEG_INFINITY)),
                    ("inf", Value::Float(f64::INFINITY)),
                ] {
                    if self.eat(w) {
                        return v;
                    }
                }
                let start = self.at;
                while let Some(b) = self.peek() {
                    if b.is_ascii_digit() || b"+-.eE".contains(&b) {
                        self.at += 1;
                    } else {
                        break;
                    }
                }
                let t = std::str::from_utf8(&self.s[start..self.at]).unwrap();
                if t.contains(['.', 'e', 'E']) {
                    Value::Float(t.parse().unwrap())
                } else {
                    Value::Int(
                        t.parse()
                            .unwrap_or_else(|_| panic!("not a value at {}", self.rest())),
                    )
                }
            }
        }
    }
    /// `n12` or `@<value>`.
    fn handle(&mut self) -> String {
        if self.eat("@") {
            let v = self.value();
            return format!("@{}", lit(&v));
        }
        assert!(self.eat("n"), "a handle at {:?}", self.rest());
        let start = self.at;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.at += 1;
        }
        format!("n{}", std::str::from_utf8(&self.s[start..self.at]).unwrap())
    }
    /// `{k=v, ...}` with values or template marks.
    fn props(&mut self) -> Vec<(String, PVal)> {
        let mut out = Vec::new();
        if !self.eat("{") {
            return out;
        }
        loop {
            if self.eat("}") {
                return out;
            }
            let k = self.name();
            assert!(self.eat("="));
            out.push((k, self.pval()));
            self.eat(", ");
        }
    }
    fn pval(&mut self) -> PVal {
        if self.eat("$") {
            return PVal::Slot;
        }
        if self.eat("~") {
            let start = self.at;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.at += 1;
            }
            let k = std::str::from_utf8(&self.s[start..self.at]).unwrap();
            let k = (!k.is_empty()).then(|| k.parse::<usize>().unwrap());
            let suffix = self.string();
            return PVal::Derived(k, suffix);
        }
        PVal::Lit(self.value())
    }
}

#[derive(Clone, Debug)]
enum PVal {
    Lit(Value),
    Slot,
    Derived(Option<usize>, String),
}

#[derive(Clone, Debug)]
enum Target {
    Pos(usize),
    Handle(String),
}

#[derive(Clone, Debug, Default)]
struct TDef {
    /// Position (0-based) -> card or nested template name.
    cards: BTreeMap<usize, (Vec<String>, Vec<(String, PVal)>)>,
    nested: BTreeMap<usize, String>,
    given: BTreeMap<usize, Vec<(String, PVal)>>,
    edges: Vec<(usize, String, Vec<(String, PVal)>, Target)>,
}

/// A template with what is nested in it written in: nodes (labels, props,
/// base), edges, and where each slot goes, in order.
#[derive(Clone, Debug, Default)]
struct Flat {
    nodes: Vec<(Vec<String>, Vec<(String, PVal)>, usize)>,
    edges: Vec<(usize, String, Vec<(String, PVal)>, Target)>,
    slots: Vec<(bool, usize, usize)>,
}

fn flatten(defs: &HashMap<String, TDef>, name: &str) -> Flat {
    let d = &defs[name];
    let mut f = Flat::default();
    let mut p = 0usize;
    let last = d
        .cards
        .keys()
        .chain(d.nested.keys())
        .copied()
        .max()
        .unwrap_or(0);
    while p <= last {
        if let Some(m) = d.nested.get(&p) {
            let sub = flatten(defs, m);
            let base_n = f.nodes.len();
            let base_e = f.edges.len();
            for (l, ps, b) in sub.nodes {
                f.nodes.push((l, ps, b + base_n));
            }
            for (s, t, ps, to) in sub.edges {
                let to = match to {
                    Target::Pos(q) => Target::Pos(q + base_n),
                    h => h,
                };
                f.edges.push((s + base_n, t, ps, to));
            }
            let given = d.given.get(&p).cloned().unwrap_or_default();
            for (key, pv) in given {
                let props = &mut f.nodes[base_n].1;
                let i = props.iter().position(|(k, _)| *k == key).unwrap();
                props[i].1 = pv;
                // Derived here means from this template's root.
                f.nodes[base_n].2 = 0;
            }
            for (edge, i, k) in sub.slots {
                if !edge && i == 0 && !matches!(f.nodes[base_n].1[k].1, PVal::Slot) {
                    continue;
                }
                f.slots
                    .push((edge, if edge { i + base_e } else { i + base_n }, k));
            }
            p = f.nodes.len();
            continue;
        }
        let (l, ps) = d
            .cards
            .get(&p)
            .cloned()
            .unwrap_or_else(|| panic!("{name} has no .{}", p + 1));
        for (k, (_, pv)) in ps.iter().enumerate() {
            if matches!(pv, PVal::Slot) {
                f.slots.push((false, p, k));
            }
        }
        f.nodes.push((l, ps, 0));
        p += 1;
    }
    for (s, t, ps, to) in &d.edges {
        let at = f.edges.len();
        for (k, (_, pv)) in ps.iter().enumerate() {
            if matches!(pv, PVal::Slot) {
                f.slots.push((true, at, k));
            }
        }
        f.edges.push((*s, t.clone(), ps.clone(), to.clone()));
    }
    f
}

/// Read a whole text (plain or compact, with or without its dictionary)
/// back into a snapshot. `dictionary` is a kept dictionary's text when the
/// body names one it does not carry.
fn decode(text: &str, dictionary: Option<&str>, identity: Option<&str>) -> Snapshot {
    let mut defs: HashMap<String, TDef> = HashMap::new();
    let mut pats: HashMap<String, Vec<String>> = HashMap::new();
    let mut s = Snapshot::default();
    let mut all = String::new();
    let mut lines = text.lines();
    let head = lines.next().unwrap();
    assert!(head.starts_with("# glider-compact/1"), "{head}");
    if let Some(d) = dictionary {
        all.push_str(d);
    }
    for l in lines {
        all.push_str(l);
        all.push('\n');
    }
    let mut cur: Option<String> = None;
    for line in all.lines() {
        if line.starts_with('#') {
            cur = None;
            continue;
        }
        let mut x = Lex::new(line);
        if line.starts_with(' ') {
            // A line of the definition being read.
            let d = defs.get_mut(cur.as_ref().unwrap()).unwrap();
            x.ws();
            assert!(x.eat("."));
            let p: usize = x.name().parse::<usize>().unwrap() - 1;
            if x.eat("=") {
                d.nested.insert(p, x.name());
                // Values of the nested root this definition gives itself.
                let mut given = Vec::new();
                while x.eat(" ") {
                    let k = x.name();
                    assert!(x.eat("="));
                    given.push((k, x.pval()));
                }
                d.given.insert(p, given);
            } else if x.eat(" -") {
                let t = x.name();
                let ps = x.props();
                assert!(x.eat("-> "));
                loop {
                    let to = if x.eat(".") {
                        Target::Pos(x.name().parse::<usize>().unwrap() - 1)
                    } else {
                        Target::Handle(x.handle())
                    };
                    d.edges.push((p, t.clone(), ps.clone(), to));
                    if !x.eat(", ") {
                        break;
                    }
                }
            } else {
                let mut labels = Vec::new();
                while x.eat(":") {
                    labels.push(x.name());
                }
                let mut ps = Vec::new();
                while x.eat(" ") {
                    let k = x.name();
                    assert!(x.eat("="));
                    ps.push((k, x.pval()));
                }
                d.cards.insert(p, (labels, ps));
            }
            continue;
        }
        if line.starts_with('T') && line.contains(" #") && !line.contains(" n")
            || line.starts_with('T') && line.split(' ').nth(1).is_some_and(|w| w.starts_with('#'))
        {
            let name = line.split(' ').next().unwrap().to_string();
            defs.insert(name.clone(), TDef::default());
            cur = Some(name);
            continue;
        }
        cur = None;
        if line.starts_with('P') && line.split(' ').nth(1).is_some_and(|w| w.starts_with('#')) {
            let name = line.split(' ').next().unwrap().to_string();
            let mut types = Vec::new();
            let mut y = Lex::new(line);
            while !y.rest().is_empty() {
                if y.eat("-") && !y.rest().starts_with('>') {
                    let t = y.name();
                    if y.eat("->") {
                        types.push(t);
                    }
                } else {
                    y.at += 1;
                }
            }
            pats.insert(name, types);
            continue;
        }
        if line.starts_with('T') {
            // A use.
            let name = x.name();
            let f = flatten(&defs, &name);
            x.ws();
            let mut listed: Vec<String> = Vec::new();
            if x.peek() == Some(b'n') && x.s.get(x.at + 1).is_some_and(|b| b.is_ascii_digit()) {
                x.at += 1;
                loop {
                    let start = x.at;
                    while x.peek().is_some_and(|b| b.is_ascii_digit()) {
                        x.at += 1;
                    }
                    let a: u64 = std::str::from_utf8(&x.s[start..x.at])
                        .unwrap()
                        .parse()
                        .unwrap();
                    if x.eat("..") {
                        let start = x.at;
                        while x.peek().is_some_and(|b| b.is_ascii_digit()) {
                            x.at += 1;
                        }
                        let b: u64 = std::str::from_utf8(&x.s[start..x.at])
                            .unwrap()
                            .parse()
                            .unwrap();
                        listed.extend((a..=b).map(|i| format!("n{i}")));
                    } else {
                        listed.push(format!("n{a}"));
                    }
                    if !x.eat(",") {
                        break;
                    }
                    x.eat("n");
                }
            }
            let mut vals = Vec::new();
            loop {
                x.ws();
                if x.peek().is_none() {
                    break;
                }
                vals.push(x.value());
            }
            assert_eq!(vals.len(), f.slots.len(), "{line}");
            // Resolve every value.
            let mut node_vals: Vec<Vec<Option<Value>>> =
                f.nodes.iter().map(|n| vec![None; n.1.len()]).collect();
            let mut edge_vals: Vec<Vec<Option<Value>>> =
                f.edges.iter().map(|e| vec![None; e.2.len()]).collect();
            for ((edge, i, k), v) in f.slots.iter().zip(vals) {
                if *edge {
                    edge_vals[*i][*k] = Some(v);
                } else {
                    node_vals[*i][*k] = Some(v);
                }
            }
            for p in 0..f.nodes.len() {
                for k in 0..f.nodes[p].1.len() {
                    let (key, pv) = &f.nodes[p].1[k];
                    let v = match pv {
                        PVal::Lit(v) => v.clone(),
                        PVal::Slot => node_vals[p][k].clone().unwrap(),
                        PVal::Derived(n, sfx) => {
                            let b = f.nodes[p].2;
                            let j = f.nodes[b].1.iter().position(|(bk, _)| bk == key).unwrap();
                            let Some(Value::Text(r)) = node_vals[b][j].clone() else {
                                panic!("derived from a non-text")
                            };
                            let head = match n {
                                Some(n) => r[..*n].to_string(),
                                None => r,
                            };
                            Value::Text(format!("{head}{sfx}"))
                        }
                    };
                    node_vals[p][k] = Some(v);
                }
            }
            let mut next = listed.into_iter();
            let handles: Vec<String> = (0..f.nodes.len())
                .map(|p| {
                    if let Some(key) = identity {
                        if let Some(j) = f.nodes[p].1.iter().position(|(k, _)| k == key) {
                            return format!("@{}", lit(node_vals[p][j].as_ref().unwrap()));
                        }
                    }
                    next.next().expect("an id for every node")
                })
                .collect();
            for p in 0..f.nodes.len() {
                s.node(
                    handles[p].clone(),
                    f.nodes[p].0.clone(),
                    f.nodes[p]
                        .1
                        .iter()
                        .zip(&node_vals[p])
                        .map(|((k, _), v)| (k.clone(), v.clone().unwrap()))
                        .collect(),
                );
            }
            for (j, (src, t, ps, to)) in f.edges.iter().enumerate() {
                let props = ps
                    .iter()
                    .enumerate()
                    .map(|(k, (key, pv))| {
                        let v = match pv {
                            PVal::Lit(v) => v.clone(),
                            _ => edge_vals[j][k].clone().unwrap(),
                        };
                        (key.clone(), v)
                    })
                    .collect();
                let to = match to {
                    Target::Pos(q) => handles[*q].clone(),
                    Target::Handle(h) => h.clone(),
                };
                s.edge(handles[*src].clone(), t.clone(), to, props);
            }
            continue;
        }
        // A plain card, edge line or chain.
        let h = x.handle();
        if x.eat(" -") {
            let t = x.name();
            if let Some(types) = pats.get(&t) {
                assert!(x.eat("-> "));
                let to = x.handle();
                assert!(x.eat(" ["));
                let mut path = vec![h.clone()];
                loop {
                    path.push(x.handle());
                    if !x.eat(" ") {
                        break;
                    }
                }
                assert!(x.eat("]"));
                path.push(to);
                for (i, ty) in types.iter().enumerate() {
                    s.edge(path[i].clone(), ty.clone(), path[i + 1].clone(), vec![]);
                }
                continue;
            }
            let ps: Vec<(String, Value)> = x
                .props()
                .into_iter()
                .map(|(k, v)| match v {
                    PVal::Lit(v) => (k, v),
                    _ => panic!("a mark in plain text"),
                })
                .collect();
            assert!(x.eat("-> "));
            loop {
                let to = x.handle();
                s.edge(h.clone(), t.clone(), to, ps.clone());
                if !x.eat(", ") {
                    break;
                }
            }
            assert!(x.rest().is_empty(), "{line}");
            continue;
        }
        let mut labels = Vec::new();
        while x.eat(":") {
            labels.push(x.name());
        }
        let mut ps = Vec::new();
        if let (Some(key), Some(v)) = (identity, h.strip_prefix('@')) {
            let mut y = Lex::new(v);
            ps.push((key.to_string(), y.value()));
        }
        while x.eat(" ") {
            let k = x.name();
            assert!(x.eat("="));
            ps.push((k, x.value()));
        }
        assert!(x.rest().is_empty(), "{line}");
        s.node(h, labels, ps);
    }
    s
}

// ------------------------------------------------------ a plant to find

const HIER: &[&str] = &[
    "HAS_LINE",
    "HAS_STATION",
    "HAS_SIDE",
    "HAS_LOCATION",
    "HAS_ELEMENT",
    "HAS_INSTRUCTION",
];

struct Truth {
    /// station id -> (design, deviated)
    stations: BTreeMap<u64, (usize, bool)>,
}

/// A small plant: lines of stations drawn from a few designs, each design
/// fixing sides, locations, elements, instructions and the tools they use;
/// every station has its own codes, numbers, times and dates. One station
/// in five misses one of its design's tools, at an element of its own.
fn plant(seed: u64, stations: usize, designs: usize) -> (Graph, Truth) {
    let mut r = Rng(seed);
    let mut g = Graph::memory();
    let s = |x: &str| x.to_string();
    let t = |x: &str| Value::Text(x.to_string());
    let tools: Vec<u64> = (0..8)
        .map(|i| {
            g.add_node(
                &[s("Tool")],
                vec![
                    (s("code"), t(&format!("TL-{i:02}"))),
                    (s("name"), t(&format!("Tool {i}"))),
                ],
            )
            .unwrap()
        })
        .collect();
    // A design: per side, per location, the elements and their tools.
    let mut shapes: Vec<Vec<Vec<Vec<(String, String, Vec<usize>)>>>> = Vec::new();
    for d in 0..designs {
        let sides = 1 + (d % 2);
        let mut ds = Vec::new();
        for sd in 0..sides {
            let mut locs = Vec::new();
            for lc in 0..1 + r.below(2) as usize {
                let mut els = Vec::new();
                for e in 0..1 + r.below(3) as usize {
                    let name = format!("Element d{d} s{sd} l{lc} e{e}");
                    let text = format!(
                        "Fasten the part {} to {} Nm, design {d}",
                        r.below(100),
                        r.below(90)
                    );
                    let mut ts: Vec<usize> = (0..1 + r.below(2) as usize)
                        .map(|_| r.below(8) as usize)
                        .collect();
                    ts.sort();
                    ts.dedup();
                    els.push((name, text, ts));
                }
                locs.push(els);
            }
            ds.push(locs);
        }
        shapes.push(ds);
    }
    let plant = g
        .add_node(
            &[s("Plant")],
            vec![(s("code"), t("P01")), (s("name"), t("Plant one"))],
        )
        .unwrap();
    let mut truth = Truth {
        stations: BTreeMap::new(),
    };
    let lines = 2;
    for l in 0..lines {
        let lcode = format!("P01-L{}", l + 1);
        let line = g
            .add_node(
                &[s("Line")],
                vec![
                    (s("code"), t(&lcode)),
                    (s("name"), t(&format!("Line {}", l + 1))),
                ],
            )
            .unwrap();
        g.add_edge(plant, line, "HAS_LINE", vec![]).unwrap();
        for k in 0..stations / lines {
            let idx = l * (stations / lines) + k;
            let d = (r.below(designs as u64 * 2) as usize).min(designs - 1);
            let deviate = r.below(5) == 0;
            let scode = format!("{lcode}-ST{:03}", (k + 1) * 10);
            let st = g
                .add_node(
                    &[s("Station")],
                    vec![
                        (s("code"), t(&scode)),
                        (
                            s("name"),
                            t(&format!("Station {:03} design {d}", (k + 1) * 10)),
                        ),
                        (s("number"), Value::Int(k as i64 + 1)),
                        (s("takt_seconds"), Value::Int(30 + r.below(90) as i64)),
                    ],
                )
                .unwrap();
            g.add_edge(line, st, "HAS_STATION", vec![]).unwrap();
            let mut seq = 0;
            let mut dropped = false;
            let elements: usize = shapes[d].iter().flatten().map(|l| l.len()).sum();
            let drop_at = r.below(elements as u64) as i64 + 1;
            for (sd, locs) in shapes[d].iter().enumerate() {
                // Work depends on earlier work of the same side.
                let mut prev: Option<u64> = None;
                let side = g
                    .add_node(
                        &[s("Side")],
                        vec![
                            (s("code"), t(&format!("{scode}-{}", ["L", "R"][sd]))),
                            (s("name"), t(["Left", "Right"][sd])),
                        ],
                    )
                    .unwrap();
                g.add_edge(st, side, "HAS_SIDE", vec![]).unwrap();
                for (lc, els) in locs.iter().enumerate() {
                    let loc = g
                        .add_node(
                            &[s("WorkLocation")],
                            vec![
                                (
                                    s("code"),
                                    t(&format!("{scode}-{}{}", ["L", "R"][sd], lc + 1)),
                                ),
                                (s("name"), t(&format!("Location {lc}"))),
                            ],
                        )
                        .unwrap();
                    g.add_edge(side, loc, "HAS_LOCATION", vec![]).unwrap();
                    for (name, txt, ts) in els {
                        seq += 1;
                        let el = g
                            .add_node(
                                &[s("WorkElement")],
                                vec![
                                    (s("code"), t(&format!("{scode}-WE{seq:02}"))),
                                    (s("name"), t(name)),
                                    (s("seq"), Value::Int(seq)),
                                    (s("std_time_seconds"), Value::Int(10 + r.below(80) as i64)),
                                ],
                            )
                            .unwrap();
                        g.add_edge(loc, el, "HAS_ELEMENT", vec![]).unwrap();
                        let wi = g
                            .add_node(
                                &[s("WorkInstruction")],
                                vec![
                                    (s("number"), t(&format!("WI-{idx:04}-{seq:02}A"))),
                                    (s("text"), t(txt)),
                                    (s("revision"), t("A")),
                                ],
                            )
                            .unwrap();
                        g.add_edge(
                            el,
                            wi,
                            "HAS_INSTRUCTION",
                            vec![(
                                s("from"),
                                t(&format!("2024-0{}-1{}", 1 + r.below(9), r.below(10))),
                            )],
                        )
                        .unwrap();
                        for (i, &tl) in ts.iter().enumerate() {
                            if deviate && seq == drop_at && i == 0 {
                                dropped = true;
                                continue;
                            }
                            g.add_edge(el, tools[tl], "USES_TOOL", vec![]).unwrap();
                        }
                        if let Some(p) = prev {
                            g.add_edge(el, p, "DEPENDS_ON", vec![]).unwrap();
                        }
                        prev = Some(el);
                    }
                }
            }
            truth.stations.insert(st, (d, deviate && dropped));
        }
    }
    install(&mut g);
    (g, truth)
}

fn station_templates(g: &mut Graph) -> QueryResult {
    q(
        g,
        &format!(
            "CALL structure.templates(types: {}, labels: ['Station'])",
            list(HIER)
        ),
    )
}

fn instances_of(g: &mut Graph, args: &str, id: &str) -> Vec<(u64, Vec<Value>)> {
    let mut out = Vec::new();
    let mut after = 0i64;
    loop {
        let r = q(
            g,
            &format!(
                "CALL structure.templates({args}, instances: '{id}', after: {after}, limit: 7)"
            ),
        );
        if r.rows.is_empty() {
            return out;
        }
        for row in &r.rows {
            let Value::List(slots) = &row[3] else {
                panic!()
            };
            out.push((int(&row[1]) as u64, slots.clone()));
        }
        after = int(&r.rows.last().unwrap()[1]);
    }
}

// ------------------------------------------------------------- templates

#[test]
fn every_design_is_a_template_whose_instances_are_its_stations_without_a_deviation() {
    for seed in [1u64, 2, 3] {
        let (mut g, truth) = plant(seed, 40, 4);
        let r = station_templates(&mut g);
        let args = format!("types: {}, labels: ['Station']", list(HIER));
        let mut by_design: BTreeMap<usize, BTreeSet<u64>> = BTreeMap::new();
        for (&st, &(d, dev)) in &truth.stations {
            if !dev {
                by_design.entry(d).or_default().insert(st);
            }
        }
        let mut found: Vec<BTreeSet<u64>> = Vec::new();
        for row in &r.rows {
            let id = text(&row[col(&r, "template")]);
            let members: BTreeSet<u64> = instances_of(&mut g, &args, &id)
                .into_iter()
                .map(|(s, _)| s)
                .collect();
            assert_eq!(members.len() as i64, int(&row[col(&r, "instances")]));
            found.push(members);
        }
        for (d, members) in &by_design {
            if members.len() < 2 {
                continue;
            }
            assert!(
                found.contains(members),
                "seed {seed}: design {d}'s stations {members:?} are not one template's instances: {found:?}"
            );
        }
        // No template mixes a deviating station with its design.
        for members in &found {
            let designs: BTreeSet<(usize, bool)> =
                members.iter().map(|s| truth.stations[s]).collect();
            assert_eq!(designs.len(), 1, "seed {seed}: {designs:?}");
        }
    }
}

#[test]
fn a_template_keeps_constants_derives_codes_from_its_root_and_leaves_the_rest_as_slots() {
    let (mut g, _) = plant(4, 30, 3);
    let r = station_templates(&mut g);
    let def = text(&r.rows[0][col(&r, "definition")]);
    // The root's own values are slots; a side's code is the station's code
    // and a suffix; the side's name is the design's.
    assert!(
        def.contains(".1:Station name=$ code=$ number=$ takt_seconds=$"),
        "{def}"
    );
    assert!(def.contains(":Side name=\"Left\" code=~\"-L\""), "{def}");
    assert!(def.contains("code=~\"-WE01\""), "{def}");
    // Instructions keep their text, each its own number; the date is the
    // edge's slot; tools are referred to, not copied.
    assert!(
        def.contains("number=$ revision=\"A\" text=\"Fasten"),
        "{def}"
    );
    assert!(def.contains("-HAS_INSTRUCTION{from=$}->"), "{def}");
    assert!(def.contains("-USES_TOOL-> n"), "{def}");
    assert!(!def.contains(":Tool"), "{def}");
}

/// Every node and edge an expansion gives back, as a snapshot.
fn expansion(r: &QueryResult) -> Snapshot {
    let mut s = Snapshot::default();
    let pairs = |v: &Value| -> Vec<(String, Value)> {
        let Value::List(ps) = v else { panic!() };
        ps.iter()
            .map(|p| {
                let Value::List(kv) = p else { panic!() };
                (text(&kv[0]), kv[1].clone())
            })
            .collect()
    };
    for row in &r.rows {
        match text(&row[0]).as_str() {
            "node" => {
                let Value::List(ls) = &row[2] else { panic!() };
                s.node(
                    format!("n{}", int(&row[1])),
                    ls.iter().map(text).collect(),
                    pairs(&row[6]),
                );
            }
            _ => s.edge(
                format!("n{}", int(&row[4])),
                text(&row[3]),
                format!("n{}", int(&row[5])),
                pairs(&row[6]),
            ),
        }
    }
    s
}

/// An expansion of many roots, one snapshot per root.
fn by_root(r: &QueryResult) -> BTreeMap<u64, Snapshot> {
    let at = col(r, "root");
    let mut per: BTreeMap<u64, QueryResult> = BTreeMap::new();
    for row in &r.rows {
        per.entry(int(&row[at]) as u64)
            .or_insert_with(|| QueryResult::table(r.columns.clone(), Vec::new()))
            .rows
            .push(row.clone());
    }
    per.into_iter().map(|(k, v)| (k, expansion(&v))).collect()
}

/// The part of the graph an instance covers, read from the graph: the
/// nodes given and every edge going out of them, except those named.
fn covered(g: &Graph, nodes: &BTreeSet<u64>, except: &BTreeSet<u64>) -> Snapshot {
    let mut s = Snapshot::default();
    for &id in nodes {
        s.node(format!("n{id}"), g.node_labels(id), g.node_props(id));
    }
    for e in g.edge_ids() {
        let r = g.edge(e).unwrap();
        if nodes.contains(&r.from) && !except.contains(&e) {
            s.edge(
                format!("n{}", r.from),
                g.strings.name(r.etype).to_string(),
                format!("n{}", r.to),
                g.edge_props(e),
            );
        }
    }
    s
}

fn node_set(s: &Snapshot) -> BTreeSet<u64> {
    s.nodes.keys().map(|h| h[1..].parse().unwrap()).collect()
}

#[test]
fn every_instance_expands_to_exactly_its_nodes_edges_and_values() {
    let (mut g, _) = plant(5, 40, 4);
    for labels in [
        "['Station']",
        "['Side', 'WorkLocation', 'WorkElement']",
        "[]",
    ] {
        let args = format!("types: {}, labels: {labels}", list(HIER));
        let r = q(&mut g, &format!("CALL structure.templates({args})"));
        assert!(!r.rows.is_empty(), "{labels}");
        for row in &r.rows {
            let id = text(&row[1]);
            let roots: Vec<u64> = instances_of(&mut g, &args, &id)
                .into_iter()
                .map(|(r, _)| r)
                .collect();
            let list: Vec<String> = roots.iter().map(|r| r.to_string()).collect();
            let e = q(
                &mut g,
                &format!(
                    "CALL structure.expand(of: '{id}', root: [{}], {args})",
                    list.join(", ")
                ),
            );
            for (root, got) in by_root(&e) {
                let want = covered(&g, &node_set(&got), &BTreeSet::new());
                assert_eq!(got, want, "{id} at {root}");
                assert!(node_set(&got).contains(&root));
            }
            assert_eq!(by_root(&e).len(), roots.len());
        }
    }
}

#[test]
fn a_station_that_deviates_is_refused_as_an_instance_of_its_design() {
    let (mut g, truth) = plant(6, 40, 3);
    let args = format!("types: {}, labels: ['Station']", list(HIER));
    let r = q(&mut g, &format!("CALL structure.templates({args})"));
    let mut checked = 0;
    for row in &r.rows {
        let id = text(&row[1]);
        let members = instances_of(&mut g, &args, &id);
        let (design, deviated) = truth.stations[&members[0].0];
        if deviated {
            continue;
        }
        for (&st, &(d, dev)) in &truth.stations {
            if d == design && dev {
                let e = err(
                    &mut g,
                    &format!("CALL structure.expand(of: '{id}', root: {st}, {args})"),
                );
                assert!(e.contains("is not an instance of template"), "{e}");
                checked += 1;
            }
        }
    }
    assert!(checked > 0);
}

// --------------------------------------------------------- random graphs

/// A random forest of repeated little designs, with values that repeat,
/// vary, or are derived from the root, internal edges, references to a
/// shared pool, edges with properties, a few mutated copies, and noise.
fn random_forest(seed: u64, dir_in: bool) -> Graph {
    let mut r = Rng(seed);
    let mut g = Graph::memory();
    let s = |x: &str| x.to_string();
    let pool: Vec<u64> = (0..6)
        .map(|i| {
            g.add_node(&[s("Pool")], vec![(s("key"), Value::Text(format!("p{i}")))])
                .unwrap()
        })
        .collect();
    let parent_of = |g: &mut Graph, parent: u64, child: u64, props: Vec<(String, Value)>| {
        if dir_in {
            g.add_edge(child, parent, "H", props).unwrap();
        } else {
            g.add_edge(parent, child, "H", props).unwrap();
        }
    };
    let mut roots: Vec<u64> = Vec::new();
    for d in 0..5u64 {
        // A design: node count, parent of each, labels, constant values.
        let n = 2 + r.below(6) as usize;
        let parents: Vec<usize> = (1..n).map(|i| r.below(i as u64) as usize).collect();
        let labels: Vec<&str> = (0..n)
            .map(|_| ["A", "B", "C"][r.below(3) as usize])
            .collect();
        let consts: Vec<i64> = (0..n).map(|_| r.below(4) as i64).collect();
        let refs: Vec<Option<usize>> = (0..n)
            .map(|_| (r.below(2) == 0).then(|| r.below(6) as usize))
            .collect();
        let internal: Vec<Option<(usize, usize)>> = (0..2)
            .map(|_| (n > 2).then(|| (r.below(n as u64) as usize, r.below(n as u64) as usize)))
            .collect();
        for copy in 0..2 + r.below(5) {
            let mutate = r.below(6) == 0;
            let root_code = format!("D{d}-{copy}-{}", r.below(1000));
            let mut ids = Vec::new();
            for i in 0..n {
                let mut props = vec![
                    (
                        s("key"),
                        Value::Text(if i == 0 {
                            root_code.clone()
                        } else {
                            format!("{root_code}/{i}")
                        }),
                    ),
                    (s("c"), Value::Int(consts[i])),
                ];
                if r.below(3) == 0 {
                    props.push((s("v"), Value::Int(r.below(3) as i64)));
                } else {
                    props.push((s("v"), Value::Float(consts[i] as f64)));
                }
                if r.below(4) == 0 && i > 0 {
                    props.push((
                        s("rare"),
                        Value::List(vec![Value::Text("x".into()), Value::Int(1)]),
                    ));
                }
                ids.push(g.add_node(&[s(labels[i])], props).unwrap());
            }
            for i in 1..n {
                let props = if r.below(3) == 0 {
                    vec![(s("w"), Value::Int(r.below(2) as i64))]
                } else {
                    vec![]
                };
                parent_of(&mut g, ids[parents[i - 1]], ids[i], props);
            }
            for i in 0..n {
                if let Some(p) = refs[i] {
                    if !(mutate && i == n - 1) {
                        g.add_edge(ids[i], pool[p], "R", vec![]).unwrap();
                    }
                }
            }
            for (a, b) in internal.iter().flatten() {
                g.add_edge(ids[*a], ids[*b], "I", vec![(s("k"), Value::Int(1))])
                    .unwrap();
                if r.below(5) == 0 {
                    g.add_edge(ids[*a], ids[*b], "I", vec![(s("k"), Value::Int(1))])
                        .unwrap();
                }
            }
            roots.push(ids[0]);
        }
    }
    // Hang some designs under others, and add noise edges.
    for i in 1..roots.len() {
        if r.below(3) == 0 {
            let p = roots[r.below(i as u64) as usize];
            parent_of(&mut g, p, roots[i], vec![]);
        }
    }
    for _ in 0..5 {
        let a = r.below(g.node_count() as u64) + 1;
        let b = r.below(g.node_count() as u64) + 1;
        let _ = g.add_edge(a, b, "X", vec![]);
    }
    install(&mut g);
    g
}

/// The outermost instances of the templates chosen, each expanded, and
/// everything else read plainly, are the graph exactly.
fn instances_and_the_rest_are_the_graph(g: &mut Graph, args: &str, pick: &dyn Fn(usize) -> bool) {
    let r = q(g, &format!("CALL structure.templates({args})"));
    let mut chosen: Vec<(u64, String)> = Vec::new();
    for (i, row) in r.rows.iter().enumerate() {
        if pick(i) {
            let id = text(&row[1]);
            for (root, _) in instances_of(g, args, &id) {
                chosen.push((root, id.clone()));
            }
        }
    }
    let mut expanded: Vec<(Snapshot, BTreeSet<u64>)> = Vec::new();
    let mut per: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (root, id) in &chosen {
        per.entry(id.clone()).or_default().push(root.to_string());
    }
    for (id, roots) in per {
        let e = q(
            g,
            &format!(
                "CALL structure.expand(of: '{id}', root: [{}], {args})",
                roots.join(", ")
            ),
        );
        for (_, s) in by_root(&e) {
            let nodes = node_set(&s);
            expanded.push((s, nodes));
        }
    }
    // Outermost: drop instances inside another chosen one.
    let keep: Vec<usize> = (0..expanded.len())
        .filter(|&i| {
            !(0..expanded.len()).any(|j| {
                j != i
                    && expanded[j].1.len() > expanded[i].1.len()
                    && expanded[i].1.is_subset(&expanded[j].1)
            })
        })
        .collect();
    let mut whole = Snapshot::default();
    let mut in_instances = BTreeSet::new();
    let mut edges_covered = 0usize;
    for &i in &keep {
        let (s, nodes) = &expanded[i];
        for (h, (l, p)) in &s.nodes {
            assert!(
                whole
                    .nodes
                    .insert(h.clone(), (l.clone(), p.clone()))
                    .is_none(),
                "{h} twice"
            );
        }
        for (k, c) in &s.edges {
            *whole.edges.entry(k.clone()).or_default() += c;
            edges_covered += c;
        }
        in_instances.extend(nodes.iter().copied());
    }
    // Everything else, plainly: other nodes, and edges an instance does not
    // write (from outside any, or a root's own edge to its owner).
    let all = snapshot(g, None);
    let mut rest_edges = 0usize;
    for id in g.node_ids() {
        if !in_instances.contains(&id) {
            whole.node(format!("n{id}"), g.node_labels(id), g.node_props(id));
        }
    }
    let total: usize = all.edges.values().sum();
    for (k, c) in &all.edges {
        let have = whole.edges.get(k).copied().unwrap_or(0);
        assert!(
            have <= *c,
            "an instance wrote an edge that is not there: {k:?}"
        );
        rest_edges += c - have;
        whole.edges.insert(k.clone(), *c);
    }
    assert_eq!(edges_covered + rest_edges, total);
    assert_eq!(whole, all);
}

#[test]
fn on_random_graphs_instances_and_what_they_do_not_cover_are_the_graph() {
    for seed in 1..=12u64 {
        for dir_in in [false, true] {
            let mut g = random_forest(seed, dir_in);
            let dir = if dir_in { "in" } else { "out" };
            for (types, min_nodes) in [("['H']", 2), ("['H', 'I']", 2), ("['H']", 1)] {
                let args = format!("types: {types}, dir: '{dir}', min_nodes: {min_nodes}");
                for pick in [0u64, 1, 2] {
                    instances_and_the_rest_are_the_graph(&mut g, &args, &|i| match pick {
                        0 => true,
                        1 => i % 2 == 0,
                        _ => i % 3 == 1,
                    });
                }
            }
        }
    }
}

#[test]
fn on_random_graphs_every_instance_expands_to_what_the_graph_holds() {
    for seed in 20..=30u64 {
        for dir_in in [false, true] {
            let mut g = random_forest(seed, dir_in);
            let dir = if dir_in { "in" } else { "out" };
            let args = format!("types: ['H'], dir: '{dir}'");
            let r = q(&mut g, &format!("CALL structure.templates({args})"));
            for row in &r.rows {
                let id = text(&row[1]);
                for (root, _) in instances_of(&mut g, &args, &id) {
                    let e = q(
                        &mut g,
                        &format!("CALL structure.expand(of: '{id}', root: {root}, {args})"),
                    );
                    let got = expansion(&e);
                    // Under dir: in the root's edge to its owner is not the
                    // instance's.
                    let mut except = BTreeSet::new();
                    if dir_in {
                        for eid in g.edge_ids() {
                            let er = g.edge(eid).unwrap();
                            if er.from == root
                                && g.strings.name(er.etype) == "H"
                                && !node_set(&got).contains(&er.to)
                            {
                                except.insert(eid);
                            }
                        }
                    }
                    assert_eq!(
                        got,
                        covered(&g, &node_set(&got), &except),
                        "seed {seed} {dir} {id} at {root}"
                    );
                }
            }
        }
    }
}

// ------------------------------------------------------------------ paths

#[test]
fn path_patterns_are_the_sequences_paths_follow_with_their_counts() {
    let (mut g, truth) = plant(7, 20, 3);
    let r = q(
        &mut g,
        &format!("CALL structure.paths(types: {})", list(HIER)),
    );
    let path = col(&r, "path");
    let rows: Vec<(String, i64, bool)> = r
        .rows
        .iter()
        .map(|row| {
            (
                text(&row[path]),
                int(&row[col(&r, "paths")]),
                row[col(&r, "maximal")] == Value::Bool(true),
            )
        })
        .collect();
    let long = "(:Plant) -HAS_LINE-> (:Line) -HAS_STATION-> (:Station) -HAS_SIDE-> (:Side) -HAS_LOCATION-> (:WorkLocation) -HAS_ELEMENT-> (:WorkElement) -HAS_INSTRUCTION-> (:WorkInstruction)";
    let (_, count, maximal) = rows
        .iter()
        .find(|r| r.0 == long)
        .unwrap_or_else(|| panic!("{rows:#?}"));
    assert!(*maximal);
    let instructions = q(&mut g, "MATCH (w:WorkInstruction) RETURN count(w)");
    assert_eq!(*count, int(&instructions.rows[0][0]));
    // The first rows are the longest maximal ones.
    assert_eq!(rows[0].0, long);
    let stations = rows
        .iter()
        .find(|r| r.0 == "(:Plant) -HAS_LINE-> (:Line) -HAS_STATION-> (:Station)")
        .unwrap();
    assert_eq!(stations.1 as usize, truth.stations.len());
    assert!(!stations.2);
}

#[test]
fn chains_have_one_way_in_and_out_and_expand_gives_their_interior_back() {
    let mut g = Graph::memory();
    install(&mut g);
    // a -> x -> y -> b, b -> c, b -> d; e -> f -> g (f interior); a hub.
    q(&mut g, "CREATE (:A {n: 'a'}), (:X {n: 'x'}), (:Y {n: 'y'}), (:B {n: 'b'}), (:C), (:C), (:E), (:F {n: 'f'}), (:G)");
    for (a, b) in [(1, 2), (2, 3), (3, 4), (4, 5), (4, 6), (7, 8), (8, 9)] {
        g.add_edge(a, b, "H", vec![]).unwrap();
    }
    let r = q(&mut g, "CALL structure.paths(kind: 'chains', types: ['H'])");
    let chains: Vec<(String, i64, i64, String)> = r
        .rows
        .iter()
        .map(|row| (text(&row[0]), int(&row[4]), int(&row[5]), text(&row[2])))
        .collect();
    assert_eq!(
        chains,
        vec![
            ("chain:2".into(), 1, 4, "-H-> (:X) -H-> (:Y) -H->".into()),
            ("chain:8".into(), 7, 9, "-H-> (:F) -H->".into()),
        ]
    );
    let e = q(&mut g, "CALL structure.expand(of: 'chain:2', types: ['H'])");
    let s = expansion(&e);
    assert_eq!(node_set(&s), [2, 3].into_iter().collect());
    assert_eq!(s.edges.len(), 3);
    let e = err(&mut g, "CALL structure.expand(of: 'chain:3', types: ['H'])");
    assert!(e.contains("does not start a chain"), "{e}");
    // In slices.
    let a = q(
        &mut g,
        "CALL structure.paths(kind: 'chains', types: ['H'], limit: 1)",
    );
    assert_eq!(a.rows.len(), 1);
    let b = q(
        &mut g,
        "CALL structure.paths(kind: 'chains', types: ['H'], after: 2)",
    );
    assert_eq!(text(&b.rows[0][0]), "chain:8");
}

// ------------------------------------------------------------------- plan

fn plan(g: &mut Graph, args: &str) -> QueryResult {
    q(g, &format!("CALL structure.plan({args})"))
}

fn total(r: &QueryResult) -> &Vec<Value> {
    assert_eq!(text(&r.rows[0][0]), "total");
    &r.rows[0]
}

#[test]
fn the_plans_saving_is_exactly_plain_minus_compact_as_written() {
    for seed in [1u64, 8] {
        let (mut g, _) = plant(seed, 40, 4);
        let args = format!("types: {}", list(HIER));
        let p = plan(&mut g, &format!("{args}, text: 'plain'"));
        let c = plan(&mut g, &format!("{args}, text: 'compact'"));
        let (pt, ct) = (text(&total(&p)[14]), text(&total(&c)[14]));
        let t = total(&c);
        assert_eq!(int(&t[col(&c, "plain_bytes")]) as usize, pt.len());
        assert_eq!(int(&t[col(&c, "plain_tokens")]) as usize, tokens(&pt));
        assert_eq!(int(&t[col(&c, "use_bytes")]) as usize, ct.len());
        assert_eq!(int(&t[col(&c, "use_tokens")]) as usize, tokens(&ct));
        assert_eq!(
            int(&t[col(&c, "saving_tokens")]) as usize,
            tokens(&pt) - tokens(&ct)
        );
        assert!(
            ct.len() * 4 < pt.len() * 3,
            "compact {} plain {}",
            ct.len(),
            pt.len()
        );
        // The rows add up: uses against what they replace, less the
        // dictionary, is the saving.
        let mut sum = 0i64;
        for row in &c.rows[1..] {
            if row[col(&c, "chosen")] == Value::Bool(true) {
                sum += int(&row[col(&c, "plain_tokens")]) - int(&row[col(&c, "use_tokens")]);
            }
        }
        let header = tokens("# glider-compact/1\n") as i64;
        let dict_head = tokens(&format!("# glider-compact/1 dictionary {}\n", text(&t[1]))) as i64
            + tokens("# body\n") as i64;
        assert_eq!(
            sum - int(&t[col(&c, "define_tokens")]) - dict_head + header,
            int(&t[col(&c, "saving_tokens")])
        );
        // Every chosen candidate pays for itself.
        for row in &c.rows[1..] {
            if row[col(&c, "chosen")] == Value::Bool(true) {
                assert!(int(&row[col(&c, "saving_tokens")]) > 0, "{row:?}");
            }
        }
    }
}

#[test]
fn the_compact_text_reads_back_as_the_graph_node_for_node_and_edge_for_edge() {
    for seed in [1u64, 2, 9] {
        let (mut g, _) = plant(seed, 40, 4);
        let args = format!("types: {}", list(HIER));
        let want = snapshot(&g, None);
        for t in ["plain", "compact"] {
            let r = plan(&mut g, &format!("{args}, text: '{t}'"));
            let got = decode(&text(&total(&r)[14]), None, None);
            assert_eq!(got, want, "seed {seed} {t}");
        }
        for seed2 in [3u64, 4] {
            let mut g = random_forest(seed2 + seed, seed2 == 4);
            let dir = if seed2 == 4 { "in" } else { "out" };
            let want = snapshot(&g, None);
            let r = plan(
                &mut g,
                &format!("types: ['H'], dir: '{dir}', by: 'bytes', text: 'compact'"),
            );
            assert_eq!(
                decode(&text(&total(&r)[14]), None, None),
                want,
                "random {seed2}"
            );
        }
    }
}

#[test]
fn smaller_shapes_are_nested_in_station_definitions_and_reused() {
    let (mut g, _) = plant(10, 60, 3);
    let args = format!("types: {}", list(HIER));
    let r = plan(&mut g, &format!("{args}, text: 'dictionary'"));
    let dict = text(&total(&r)[14]);
    assert!(dict.contains("=T"), "nothing nested:\n{dict}");
    let nested: Vec<&Vec<Value>> = r.rows[1..]
        .iter()
        .filter(|row| {
            row[col(&r, "chosen")] == Value::Bool(true) && int(&row[col(&r, "nested_in")]) > 0
        })
        .collect();
    assert!(!nested.is_empty(), "{dict}\n{}", dump(&r));
    // Still exact.
    let c = plan(&mut g, &format!("{args}, text: 'compact'"));
    assert_eq!(
        decode(&text(&total(&c)[14]), None, None),
        snapshot(&g, None)
    );
}

#[test]
fn a_candidate_that_does_not_pay_for_itself_is_not_used() {
    let mut g = Graph::memory();
    install(&mut g);
    // Two two-node shapes whose every value differs: a template would be
    // all slots and cost more than it saves.
    q(
        &mut g,
        "CREATE (a:A {k: 'one'})-[:H]->(:B {k: 'uno'}), (c:A {k: 'two'})-[:H]->(:B {k: 'dos'})",
    );
    let r = q(&mut g, "CALL structure.templates(types: ['H'])");
    assert_eq!(r.rows.len(), 1);
    assert!(int(&r.rows[0][col(&r, "saving_tokens")]) <= 0);
    let p = plan(&mut g, "types: ['H'], text: 'compact'");
    let t = total(&p);
    assert_eq!(t[col(&p, "chosen")], Value::Bool(false));
    assert_eq!(int(&t[col(&p, "saving_bytes")]), 0);
    assert_eq!(t[col(&p, "use_bytes")], t[col(&p, "plain_bytes")]);
    assert!(p.rows[1..]
        .iter()
        .all(|row| row[col(&p, "chosen")] == Value::Bool(false)));
    let plain = plan(&mut g, "types: ['H'], text: 'plain'");
    assert_eq!(total(&plain)[14], t[14]);
}

#[test]
fn a_plan_of_some_nodes_writes_only_them() {
    let (mut g, truth) = plant(11, 40, 3);
    let some: Vec<u64> = truth.stations.keys().take(10).copied().collect();
    // Each station's own nodes.
    let mut nodes = BTreeSet::new();
    let mut stack: Vec<u64> = some.clone();
    while let Some(v) = stack.pop() {
        nodes.insert(v);
        for e in g.edge_ids() {
            let r = g.edge(e).unwrap();
            if r.from == v && HIER[2..].contains(&g.strings.name(r.etype)) {
                stack.push(r.to);
            }
        }
    }
    let ids: Vec<String> = nodes.iter().map(|n| n.to_string()).collect();
    let r = plan(
        &mut g,
        &format!(
            "types: {}, nodes: [{}], text: 'compact'",
            list(HIER),
            ids.join(", ")
        ),
    );
    let got = decode(&text(&total(&r)[14]), None, None);
    assert_eq!(node_set(&got), nodes);
    let all = snapshot(&g, None);
    for (k, c) in &got.edges {
        assert_eq!(all.edges.get(k), Some(c));
    }
    assert!(int(&total(&r)[col(&r, "saving_tokens")]) > 0);
}

// ------------------------------------------------------ kept dictionaries

#[test]
fn a_kept_dictionary_is_named_by_its_hash_and_says_when_the_database_moved() {
    let (mut g, _) = plant(12, 40, 3);
    let args = format!("types: {}", list(HIER));
    let before = g.revision();
    let kept = plan(
        &mut g,
        &format!("{args}, store: 'plant', text: 'dictionary'"),
    );
    assert!(g.revision() > before);
    let dict = text(&total(&kept)[14]);
    let hash = text(&total(&kept)[1]);
    let d = q(&mut g, "CALL structure.dictionaries()");
    assert_eq!(d.rows.len(), 1);
    assert_eq!(text(&d.rows[0][0]), "plant");
    assert_eq!(int(&d.rows[0][1]) as u64, before);
    assert_eq!(d.rows[0][2], Value::Bool(true));
    assert_eq!(text(&d.rows[0][3]), hash);
    assert_eq!(text(&d.rows[0][8]), glider::ext::structure::TEMPLATES_CODE);
    assert!(
        text(&d.rows[0][9]).contains("\"types\":[\"HAS_ELEMENT\""),
        "{:?}",
        d.rows[0][9]
    );
    // Kept colourings and dictionaries leave each other fresh.
    q(&mut g, "CALL structure.colors(depth: 1, store: 'c')");
    assert_eq!(
        q(&mut g, "CALL structure.dictionaries()").rows[0][2],
        Value::Bool(true)
    );
    // A text written with it names it by hash and does not repeat it.
    let used = plan(
        &mut g,
        "dictionary: 'plant', text: 'compact', fresh: 'require'",
    );
    let body = text(&total(&used)[14]);
    assert!(
        body.starts_with(&format!("# glider-compact/1 dictionary {hash}\n")),
        "{}",
        &body[..80]
    );
    assert!(!body.contains(" #"));
    assert_eq!(decode(&body, Some(&dict), None), snapshot(&g, None));
    let inline = plan(&mut g, &format!("{args}, text: 'compact'"));
    assert!(
        int(&total(&used)[col(&used, "use_tokens")])
            < int(&total(&inline)[col(&inline, "use_tokens")])
    );
    // Expansion with it.
    let t1 = used.rows[1..]
        .iter()
        .find(|row| row[col(&used, "chosen")] == Value::Bool(true) && text(&row[0]) == "template")
        .unwrap();
    let id = text(&t1[1]);
    let r = q(
        &mut g,
        &format!("CALL structure.templates({args}, instances: '{id}', limit: 1)"),
    );
    let root = int(&r.rows[0][1]);
    let e = q(
        &mut g,
        &format!("CALL structure.expand(of: '{id}', root: {root}, dictionary: 'plant')"),
    );
    let s = expansion(&e);
    assert_eq!(s, covered(&g, &node_set(&s), &BTreeSet::new()));
    // A change anywhere makes it stale; require refuses; uses still exact.
    q(&mut g, "CREATE (:Note {text: 'later'})");
    assert_eq!(
        q(&mut g, "CALL structure.dictionaries()").rows[0][2],
        Value::Bool(false)
    );
    let e = err(
        &mut g,
        "CALL structure.plan(dictionary: 'plant', fresh: 'require')",
    );
    assert!(e.contains("has changed since"), "{e}");
    let stale = plan(&mut g, "dictionary: 'plant', text: 'compact'");
    assert!(stale.message.as_ref().unwrap().contains("stale"));
    assert_eq!(
        decode(&text(&total(&stale)[14]), Some(&dict), None),
        snapshot(&g, None)
    );
    // Dropped, it is gone; the colouring stays.
    q(&mut g, "CALL structure.drop(dictionary: 'plant')");
    assert!(q(&mut g, "CALL structure.dictionaries()").rows.is_empty());
    assert_eq!(q(&mut g, "CALL structure.stored()").rows.len(), 1);
    let e = err(&mut g, "CALL structure.plan(dictionary: 'plant')");
    assert!(e.contains("no dictionary is kept as 'plant'"), "{e}");
    // check.run finds nothing wrong with a kept dictionary.
    plan(&mut g, &format!("{args}, store: 'again'"));
    q(&mut g, "CREATE EXTENSION check");
    let r = q(&mut g, "CALL check.run(checks: ['derived'])");
    assert!(
        r.rows.iter().all(|row| text(&row[1]) == "summary"),
        "{:?}",
        r.rows
    );
}

#[test]
fn without_store_nothing_is_written_and_the_revision_stays() {
    let (mut g, _) = plant(13, 20, 3);
    let before = g.revision();
    let args = format!("types: {}", list(HIER));
    q(&mut g, &format!("CALL structure.templates({args})"));
    q(
        &mut g,
        &format!("CALL structure.paths(types: {})", list(HIER)),
    );
    q(&mut g, &format!("CALL structure.plan({args})"));
    assert_eq!(g.revision(), before);
}

// ------------------------------------------------------- content, tiers

/// The plant's data rebuilt in another order after `gap` nodes made and
/// deleted, so every id differs.
fn rebuild_elsewhere(src: &Graph, gap: usize, reverse: bool) -> Graph {
    let mut g = Graph::memory();
    for _ in 0..gap {
        let id = g.add_node(&["Gap".to_string()], vec![]).unwrap();
        g.delete_node(id).unwrap();
    }
    let mut ids = src.node_ids();
    if reverse {
        ids.reverse();
    }
    let mut map = HashMap::new();
    for id in ids {
        map.insert(
            id,
            g.add_node(&src.node_labels(id), src.node_props(id))
                .unwrap(),
        );
    }
    let mut edges = src.edge_ids();
    if reverse {
        edges.reverse();
    }
    for e in edges {
        let r = src.edge(e).unwrap();
        g.add_edge(
            map[&r.from],
            map[&r.to],
            src.strings.name(r.etype),
            src.edge_props(e),
        )
        .unwrap();
    }
    install(&mut g);
    g
}

#[test]
fn the_same_data_under_other_ids_has_the_same_templates_dictionary_and_plan() {
    let (mut a, _) = plant(14, 40, 4);
    let mut b = rebuild_elsewhere(&a, 17, true);
    let args = format!("types: {}, identity: 'code'", list(HIER));
    // Instructions have no code: name them by number too.
    let args2 = format!("types: {}, identity: ['code', 'number']", list(HIER));
    let ids = |r: &QueryResult| -> Vec<(String, i64)> {
        r.rows
            .iter()
            .map(|row| (text(&row[1]), int(&row[6])))
            .collect()
    };
    for args in [&args, &args2] {
        let ta = q(&mut a, &format!("CALL structure.templates({args})"));
        let tb = q(&mut b, &format!("CALL structure.templates({args})"));
        assert_eq!(ids(&ta), ids(&tb), "{args}");
    }
    // With every node named by identity, the plan and the dictionary are
    // the same.
    let pa = plan(&mut a, &format!("{args2}, text: 'dictionary'"));
    let pb = plan(&mut b, &format!("{args2}, text: 'dictionary'"));
    assert_eq!(dump(&pa), dump(&pb));
    // With every node named by identity, the whole text is the same.
    let ca = plan(&mut a, &format!("{args2}, text: 'compact'"));
    let cb = plan(&mut b, &format!("{args2}, text: 'compact'"));
    assert_eq!(dump(&ca), dump(&cb));
    let tx = text(&total(&ca)[14]);
    assert!(
        !tx.contains(" n1") && tx.contains("@\"P01-L1-ST010\""),
        "{}",
        &tx[..400]
    );
    // Template ids alone do not need identity.
    let ra = q(
        &mut a,
        &format!("CALL structure.templates(types: {})", list(HIER)),
    );
    let rb = q(
        &mut b,
        &format!("CALL structure.templates(types: {})", list(HIER)),
    );
    let ids =
        |r: &QueryResult| -> BTreeSet<String> { r.rows.iter().map(|row| text(&row[1])).collect() };
    assert_eq!(ids(&ra), ids(&rb));
}

#[test]
fn the_compact_text_by_identity_reads_back_as_the_graph() {
    let mut g = random_forest(40, false);
    let r = plan(
        &mut g,
        "types: ['H'], identity: 'key', by: 'bytes', text: 'compact'",
    );
    assert_eq!(
        decode(&text(&total(&r)[14]), None, Some("key")),
        snapshot(&g, Some("key"))
    );
}

#[test]
fn the_memory_and_paged_tiers_give_the_same_bytes() {
    let (mem, _) = plant(15, 40, 4);
    let p = temp("tiers");
    let mut file = Graph::open_opts(&p, OpenOptions::default()).unwrap();
    // The same records in the same order: the same ids.
    for id in mem.node_ids() {
        let got = file
            .add_node(&mem.node_labels(id), mem.node_props(id))
            .unwrap();
        assert_eq!(got, id);
    }
    for e in mem.edge_ids() {
        let r = mem.edge(e).unwrap();
        file.add_edge(r.from, r.to, mem.strings.name(r.etype), mem.edge_props(e))
            .unwrap();
    }
    install(&mut file);
    let mut mem = mem;
    let h = list(HIER);
    for args in [
        format!("CALL structure.templates(types: {h})"),
        format!("CALL structure.templates(types: {h}, labels: ['Side'], min_count: 3)"),
        format!("CALL structure.paths(types: {h})"),
        "CALL structure.paths(kind: 'chains', types: ['HAS_SIDE', 'HAS_LOCATION'])".to_string(),
        format!("CALL structure.plan(types: {h}, text: 'compact')"),
        format!("CALL structure.plan(types: {h}, by: 'bytes', store: 'd')"),
        "CALL structure.plan(dictionary: 'd', text: 'compact')".to_string(),
    ] {
        let a = q(&mut file, &args);
        let b = q(&mut mem, &args);
        assert_eq!(dump(&a), dump(&b), "{args}");
        assert_eq!(
            a.message.unwrap().replace(char::is_numeric, ""),
            b.message.unwrap().replace(char::is_numeric, ""),
            "{args}"
        );
    }
    drop(file);
    cleanup(&p);
}

#[test]
fn state_spills_past_the_memory_budget_and_nothing_changes() {
    let (mut big, _) = plant(16, 200, 5);
    let mut small = rebuild_elsewhere(&big, 0, false);
    small.set_work_mem(64 << 10);
    glider::ooc::set_spill_shape(512, 4);
    let h = list(HIER);
    for args in [
        format!("CALL structure.templates(types: {h})"),
        format!("CALL structure.paths(types: {h})"),
        format!("CALL structure.plan(types: {h})"),
    ] {
        assert_eq!(
            dump(&q(&mut big, &args)),
            dump(&q(&mut small, &args)),
            "{args}"
        );
    }
}

#[test]
fn slices_of_templates_and_of_plan_rows_together_are_the_whole() {
    let (mut g, _) = plant(17, 40, 4);
    let h = list(HIER);
    let whole = q(&mut g, &format!("CALL structure.templates(types: {h})"));
    for limit in [1usize, 3, 7] {
        let mut rows = Vec::new();
        let mut after = 0;
        loop {
            let r = q(
                &mut g,
                &format!("CALL structure.templates(types: {h}, after: {after}, limit: {limit})"),
            );
            if r.rows.is_empty() {
                break;
            }
            after = int(&r.rows.last().unwrap()[0]);
            rows.extend(r.rows);
        }
        assert_eq!(rows, whole.rows, "by {limit}");
    }
    let whole = plan(&mut g, &format!("types: {h}"));
    let mut rows = Vec::new();
    let first = plan(&mut g, &format!("types: {h}, limit: 4"));
    rows.extend(first.rows.clone());
    let mut after = 4;
    loop {
        let r = plan(&mut g, &format!("types: {h}, after: {after}, limit: 4"));
        if r.rows.is_empty() {
            break;
        }
        after += r.rows.len();
        rows.extend(r.rows);
    }
    assert_eq!(rows, whole.rows);
}

#[test]
fn bad_arguments_are_refused_with_what_to_do() {
    let (mut g, _) = plant(18, 10, 2);
    for (args, want) in [
        ("CALL structure.templates(dir: 'both')", "use \"out\""),
        ("CALL structure.templates(min_count: 1)", "give 2 or more"),
        ("CALL structure.plan(by: 'words')", "use \"tokens\""),
        ("CALL structure.plan(lambda: -1)", "give a number 0 or more"),
        ("CALL structure.plan(text: 'html')", "use \"compact\""),
        ("CALL structure.expand(of: 'abc')", "needs root:"),
        (
            "CALL structure.expand(of: 'abc', root: 1)",
            "CALL structure.templates with the same arguments",
        ),
        (
            "CALL structure.expand(of: 'chain:x')",
            "does not name a chain",
        ),
        ("CALL structure.paths(kind: 'loops')", "use \"patterns\""),
        ("CALL structure.paths(max_hops: 0)", "give 1 to 64"),
        (
            "CALL structure.drop(dictionary: 'none')",
            "no dictionary is kept as 'none'",
        ),
    ] {
        let e = err(&mut g, args);
        assert!(e.contains(want), "{args}: {e}");
    }
}

#[test]
fn a_selection_written_with_a_kept_dictionary_is_the_text_the_whole_graph_path_writes() {
    let (mut g, truth) = plant(19, 60, 4);
    let h = list(HIER);
    plan(&mut g, &format!("types: {h}, store: 'k'"));
    let dict = text(&total(&plan(&mut g, "dictionary: 'k', text: 'dictionary'"))[14]);
    let n = g.node_count() as u64;
    let mut r = Rng(5);
    let stations: Vec<u64> = truth.stations.keys().copied().collect();
    let mut selections: Vec<Vec<u64>> = Vec::new();
    // Whole stations, a line's worth, scattered nodes, and a mix.
    for s in stations.iter().take(3) {
        let mut v = vec![*s];
        let mut stack = vec![*s];
        while let Some(x) = stack.pop() {
            for e in g.edge_ids() {
                let er = g.edge(e).unwrap();
                if er.from == x && HIER[2..].contains(&g.strings.name(er.etype)) {
                    v.push(er.to);
                    stack.push(er.to);
                }
            }
        }
        selections.push(v);
    }
    selections.push((0..80).map(|_| 1 + r.below(n)).collect());
    let mut mix = selections[0].clone();
    mix.extend(selections[3].iter().copied());
    mix.extend(selections[1].iter().skip(3).copied());
    selections.push(mix);
    selections.push((1..=n).collect());
    for (i, sel) in selections.iter().enumerate() {
        let ids: Vec<String> = sel.iter().map(|x| x.to_string()).collect();
        let args = format!(
            "dictionary: 'k', nodes: [{}], text: 'compact'",
            ids.join(", ")
        );
        let local = plan(&mut g, &args);
        let whole = plan(&mut g, &format!("{args}, find: 'graph'"));
        assert_eq!(total(&local), total(&whole), "selection {i}");
        let t = text(&total(&local)[14]);
        assert!(
            t.starts_with(&format!(
                "# glider-compact/1 dictionary {}",
                text(&total(&local)[1])
            )) || total(&local)[3] == Value::Bool(false)
        );
        let got = decode(&t, Some(&dict), None);
        let all = snapshot(&g, None);
        for (k, v) in &got.nodes {
            assert_eq!(all.nodes.get(k), Some(v), "selection {i}: {k}");
        }
        for (k, c) in &got.edges {
            assert_eq!(all.edges.get(k), Some(c), "selection {i}: {k:?}");
        }
    }
    // After a change, the selection is still written from the graph as it
    // is now: the changed station is no longer an instance and is written
    // plainly, the same both ways; require refuses.
    let st = selections[0][0];
    q(
        &mut g,
        &format!(
            "MATCH (s) WHERE id(s) = {} SET s.takt_seconds = 'changed'",
            selections[0][1]
        ),
    );
    let ids: Vec<String> = selections[0].iter().map(|x| x.to_string()).collect();
    let args = format!(
        "dictionary: 'k', nodes: [{}], text: 'compact'",
        ids.join(", ")
    );
    let local = plan(&mut g, &args);
    assert_eq!(
        total(&local),
        total(&plan(&mut g, &format!("{args}, find: 'graph'")))
    );
    assert!(local.message.as_ref().unwrap().contains("stale"));
    let got = decode(&text(&total(&local)[14]), Some(&dict), None);
    assert_eq!(
        got.nodes[&format!("n{}", selections[0][1])],
        snapshot(&g, None).nodes[&format!("n{}", selections[0][1])]
    );
    let e = err(
        &mut g,
        &format!("CALL structure.plan({args}, fresh: 'require')"),
    );
    assert!(e.contains("has changed since"), "{e} {st}");
}

#[test]
fn a_dictionary_kept_inside_a_transaction_block_goes_with_a_rollback_and_stays_with_a_commit() {
    let (mut g, _) = plant(20, 20, 3);
    let h = list(HIER);
    g.begin().unwrap();
    plan(&mut g, &format!("types: {h}, store: 'gone'"));
    assert_eq!(
        q(&mut g, "CALL structure.dictionaries()").rows[0][2],
        Value::Bool(true)
    );
    g.rollback().unwrap();
    assert!(q(&mut g, "CALL structure.dictionaries()").rows.is_empty());
    let e = err(&mut g, "CALL structure.plan(dictionary: 'gone')");
    assert!(e.contains("no dictionary is kept as 'gone'"), "{e}");
    g.begin().unwrap();
    plan(&mut g, &format!("types: {h}, store: 'kept'"));
    g.commit().unwrap();
    let d = q(&mut g, "CALL structure.dictionaries()");
    assert_eq!(d.rows.len(), 1);
    assert_eq!(d.rows[0][2], Value::Bool(true));
    plan(&mut g, "dictionary: 'kept', fresh: 'require'");
}
