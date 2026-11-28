//! The compact notation read by a decoder written from its description
//! (`docs/EXTENSIONS.md`, "The notation, `glider-compact/1`") and the
//! dictionary's legend alone, not from the renderer: an independent review
//! wrote it, and it is kept here as the test that the documented notation
//! is enough to read any text the writer produces. Each review finding is a
//! test of the property it broke.

// Written by the reviewer as an independent reading; kept as written.
#![allow(
    clippy::needless_range_loop,
    clippy::too_many_arguments,
    clippy::manual_div_ceil
)]

use std::collections::{BTreeMap, HashMap};

use glider::graph::Graph;
use glider::query::{self, QueryResult};
use glider::value::Value;

// ------------------------------------------------------------------ helpers

fn q(g: &mut Graph, src: &str) -> QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn col(r: &QueryResult, name: &str) -> usize {
    r.columns
        .iter()
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("no column {name}"))
}

fn text_of(r: &QueryResult) -> String {
    match &r.rows[0][col(r, "text")] {
        Value::Text(s) => s.clone(),
        v => panic!("no text: {v:?}"),
    }
}

fn s(x: &str) -> String {
    x.to_string()
}

fn t(x: &str) -> Value {
    Value::Text(x.to_string())
}

fn install(g: &mut Graph) {
    q(g, "CREATE EXTENSION IF NOT EXISTS structure");
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

/// A value as a string equal exactly when type and value are equal.
fn ex(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => format!("b{b}"),
        Value::Int(i) => format!("i{i}"),
        Value::Float(f) => {
            if f.is_nan() {
                "fNaN".into()
            } else {
                format!("f{}", f.to_bits())
            }
        }
        Value::Text(s) => format!("t{s:?}"),
        Value::List(xs) => format!("[{}]", xs.iter().map(ex).collect::<Vec<_>>().join(",")),
    }
}

type Props = BTreeMap<String, String>;

/// A graph as sets: nodes by handle, edges as a multiset.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
struct Snap {
    nodes: BTreeMap<String, (Vec<String>, Props)>,
    edges: BTreeMap<(String, String, String, Vec<(String, String)>), usize>,
}

impl Snap {
    fn node(&mut self, h: String, mut labels: Vec<String>, props: Vec<(String, Value)>) {
        labels.sort();
        labels.dedup();
        let p: Props = props.iter().map(|(k, v)| (k.clone(), ex(v))).collect();
        assert!(
            self.nodes.insert(h.clone(), (labels, p)).is_none(),
            "node {h} is written twice"
        );
    }
    fn edge(&mut self, from: String, ty: String, to: String, props: Vec<(String, Value)>) {
        let mut p: Vec<(String, String)> = props.iter().map(|(k, v)| (k.clone(), ex(v))).collect();
        p.sort();
        *self.edges.entry((from, ty, to, p)).or_default() += 1;
    }
    fn edge_count(&self) -> usize {
        self.edges.values().sum()
    }
}

fn snap(g: &Graph) -> Snap {
    let mut sn = Snap::default();
    for id in g.node_ids() {
        sn.node(format!("n{id}"), g.node_labels(id), g.node_props(id));
    }
    for e in g.edge_ids() {
        let r = g.edge(e).unwrap();
        sn.edge(
            format!("n{}", r.from),
            g.strings.name(r.etype).to_string(),
            format!("n{}", r.to),
            g.edge_props(e),
        );
    }
    sn
}

/// The graph as a text by identity names it (`docs/EXTENSIONS.md`,
/// "Handles"): the first of `keys` a node has, with its value, when no
/// other node of `scope` has that key and value; otherwise its id.
fn snap_id(g: &Graph, keys: &[&str], scope: Option<&[u64]>) -> (Snap, HashMap<u64, String>) {
    let ids: Vec<u64> = match scope {
        Some(sc) => sc.to_vec(),
        None => g.node_ids(),
    };
    let first = |id: u64| -> Option<(String, Value)> {
        let props = g.node_props(id);
        keys.iter().find_map(|k| {
            props
                .iter()
                .find(|(pk, v)| pk == k && *v != Value::Null)
                .map(|(pk, v)| (pk.clone(), v.clone()))
        })
    };
    let mut count: HashMap<String, usize> = HashMap::new();
    for &id in &ids {
        if let Some((k, v)) = first(id) {
            *count.entry(format!("@{k}={}", ex(&v))).or_default() += 1;
        }
    }
    let handle = |id: u64| -> String {
        match first(id) {
            Some((k, v)) => {
                let h = format!("@{k}={}", ex(&v));
                if count.get(&h) == Some(&1) {
                    h
                } else {
                    format!("n{id}")
                }
            }
            None => format!("n{id}"),
        }
    };
    let mut sn = Snap::default();
    let mut names = HashMap::new();
    for id in g.node_ids() {
        let h = handle(id);
        names.insert(id, h.clone());
        sn.node(h, g.node_labels(id), g.node_props(id));
    }
    for e in g.edge_ids() {
        let r = g.edge(e).unwrap();
        sn.edge(
            handle(r.from),
            g.strings.name(r.etype).to_string(),
            handle(r.to),
            g.edge_props(e),
        );
    }
    (sn, names)
}

/// The same graph in a database file, records in the same order.
fn on_file(src: &Graph, name: &str) -> (Graph, std::path::PathBuf) {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-notation-{name}-{}.db", std::process::id()));
    cleanup(&p);
    let mut g = Graph::open_opts(&p, glider::graph::OpenOptions::default()).unwrap();
    let mut map = HashMap::new();
    for id in src.node_ids() {
        map.insert(
            id,
            g.add_node(&src.node_labels(id), src.node_props(id))
                .unwrap(),
        );
    }
    for e in src.edge_ids() {
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
    (g, p)
}

fn cleanup(p: &std::path::Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(glider::store::lock_path(p));
    for suffix in ["-wal", "-data", "-tmp", "-hot"] {
        let mut d = p.as_os_str().to_os_string();
        d.push(suffix);
        let d = std::path::PathBuf::from(d);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&d);
    }
}

// ------------------------------------------------------------- the decoder

struct Lex<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Lex<'a> {
    fn new(s: &'a str) -> Lex<'a> {
        Lex {
            b: s.as_bytes(),
            i: 0,
        }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn rest(&self) -> &'a str {
        std::str::from_utf8(&self.b[self.i..]).unwrap()
    }
    fn eat(&mut self, s: &str) -> bool {
        if self.b[self.i..].starts_with(s.as_bytes()) {
            self.i += s.len();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, s: &str) {
        assert!(self.eat(s), "expected {s:?} at {:?}", self.rest());
    }
    fn done(&self) -> bool {
        self.i >= self.b.len()
    }
    fn name(&mut self) -> String {
        if self.eat("`") {
            let mut out = String::new();
            loop {
                if self.eat("``") {
                    out.push('`');
                } else if self.eat("`") {
                    return out;
                } else {
                    let c = self.rest().chars().next().expect("unterminated name");
                    out.push(c);
                    self.i += c.len_utf8();
                }
            }
        }
        let st = self.i;
        while let Some(c) = self.peek() {
            if c == b'_' || c.is_ascii_alphanumeric() {
                self.i += 1;
            } else {
                break;
            }
        }
        assert!(self.i > st, "expected a name at {:?}", self.rest());
        std::str::from_utf8(&self.b[st..self.i])
            .unwrap()
            .to_string()
    }
    fn string(&mut self) -> String {
        self.expect("\"");
        let mut out = String::new();
        loop {
            let c = self.rest().chars().next().expect("unterminated string");
            self.i += c.len_utf8();
            match c {
                '"' => return out,
                '\\' => {
                    let e = self.peek().unwrap();
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let h = std::str::from_utf8(&self.b[self.i..self.i + 4]).unwrap();
                            self.i += 4;
                            let mut cp = u32::from_str_radix(h, 16).unwrap();
                            if (0xd800..0xdc00).contains(&cp) {
                                self.expect("\\u");
                                let h2 = std::str::from_utf8(&self.b[self.i..self.i + 4]).unwrap();
                                self.i += 4;
                                let lo = u32::from_str_radix(h2, 16).unwrap();
                                cp = 0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00);
                            }
                            out.push(char::from_u32(cp).unwrap());
                        }
                        x => panic!("bad escape {}", x as char),
                    }
                }
                c => out.push(c),
            }
        }
    }
    fn value(&mut self) -> Value {
        match self.peek() {
            Some(b'"') => Value::Text(self.string()),
            Some(b'[') => {
                self.i += 1;
                let mut xs = Vec::new();
                if self.eat("]") {
                    return Value::List(xs);
                }
                loop {
                    xs.push(self.value());
                    if self.eat("]") {
                        return Value::List(xs);
                    }
                    self.expect(", ");
                }
            }
            _ => {
                for (w, v) in [
                    ("true", Value::Bool(true)),
                    ("false", Value::Bool(false)),
                    ("null", Value::Null),
                    ("NaN", Value::Float(f64::NAN)),
                    ("inf", Value::Float(f64::INFINITY)),
                    ("-inf", Value::Float(f64::NEG_INFINITY)),
                ] {
                    if self.eat(w) {
                        return v;
                    }
                }
                let st = self.i;
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || b"+-.eE".contains(&c) {
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                let w = std::str::from_utf8(&self.b[st..self.i]).unwrap();
                assert!(!w.is_empty(), "expected a value at {:?}", self.rest());
                if w.contains(['.', 'e', 'E']) {
                    Value::Float(w.parse().unwrap())
                } else {
                    Value::Int(w.parse().unwrap())
                }
            }
        }
    }
    /// `n12` or `@key=value`, as the handle string a snapshot uses.
    fn handle(&mut self) -> String {
        if self.eat("@") {
            let k = self.name();
            self.expect("=");
            let v = self.value();
            return format!("@{k}={}", ex(&v));
        }
        self.expect("n");
        let st = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        format!("n{}", std::str::from_utf8(&self.b[st..self.i]).unwrap())
    }
}

#[derive(Clone, Debug)]
enum Spec {
    Const(Value),
    Slot(usize),
    /// Value of the same key at position `base`, whole (`None`) or its
    /// first N bytes, then the suffix.
    Derived(usize, Option<usize>, String),
}

#[derive(Clone, Debug)]
enum Tgt {
    Pos(usize),
    Handle(String),
}

#[derive(Clone, Debug, Default)]
struct Def {
    labels: Vec<Vec<String>>,
    props: Vec<Vec<(String, Spec)>>,
    edges: Vec<(usize, String, Tgt, Vec<(String, Spec)>)>,
    slots: usize,
}

/// One property spec as a definition writes it; `base` is the root of the
/// definition the line is in.
fn spec(l: &mut Lex, base: usize, slots: &mut usize) -> Spec {
    if l.eat("$") {
        *slots += 1;
        return Spec::Slot(*slots - 1);
    }
    if l.eat("~") {
        let st = l.i;
        while l.peek().is_some_and(|c| c.is_ascii_digit()) {
            l.i += 1;
        }
        let n = std::str::from_utf8(&l.b[st..l.i]).unwrap();
        let n = (!n.is_empty()).then(|| n.parse().unwrap());
        return Spec::Derived(base, n, l.string());
    }
    Spec::Const(l.value())
}

/// A definition's body lines into a flat definition, nested templates
/// inlined.
fn parse_def(lines: &[&str], defs: &HashMap<String, Def>) -> Def {
    let mut d = Def::default();
    for line in lines {
        let mut l = Lex::new(line);
        l.expect(" .");
        let st = l.i;
        while l.peek().is_some_and(|c| c.is_ascii_digit()) {
            l.i += 1;
        }
        let k: usize = std::str::from_utf8(&l.b[st..l.i]).unwrap().parse().unwrap();
        let p = k - 1;
        if l.eat(" -") {
            // An edge line.
            let ty = l.name();
            let mut props = Vec::new();
            if l.eat("{") {
                loop {
                    let key = l.name();
                    l.expect("=");
                    props.push((key, spec(&mut l, 0, &mut d.slots)));
                    if l.eat("}") {
                        break;
                    }
                    l.expect(", ");
                }
            }
            l.expect("-> ");
            loop {
                let tg = if l.eat(".") {
                    let st = l.i;
                    while l.peek().is_some_and(|c| c.is_ascii_digit()) {
                        l.i += 1;
                    }
                    Tgt::Pos(
                        std::str::from_utf8(&l.b[st..l.i])
                            .unwrap()
                            .parse::<usize>()
                            .unwrap()
                            - 1,
                    )
                } else {
                    Tgt::Handle(l.handle())
                };
                d.edges.push((p, ty.clone(), tg, props.clone()));
                if !l.eat(", ") {
                    break;
                }
            }
            assert!(l.done(), "left over: {:?}", l.rest());
        } else if l.eat("=") {
            // A nested template from position p on.
            let name = l.name();
            let u = defs
                .get(&name)
                .unwrap_or_else(|| panic!("{name} is used before it is defined"))
                .clone();
            assert_eq!(d.labels.len(), p, "nested {name} not at the next position");
            // Given values of its root.
            let mut given: Vec<(String, Spec)> = Vec::new();
            let mut dummy = 0;
            while l.eat(" ") {
                let key = l.name();
                l.expect("=");
                given.push((key, spec(&mut l, 0, &mut dummy)));
            }
            assert!(l.done(), "left over: {:?}", l.rest());
            // Which of u's slots the given values fill.
            let mut drop_slots: Vec<usize> = Vec::new();
            for (key, _) in &given {
                let (_, sp) = u.props[0]
                    .iter()
                    .find(|(k, _)| k == key)
                    .unwrap_or_else(|| {
                        panic!("{key} given for {name}'s root, which has no such key")
                    });
                match sp {
                    Spec::Slot(i) => drop_slots.push(*i),
                    other => panic!("{key} given for {name}'s root where it is {other:?}"),
                }
            }
            let mut map: Vec<Option<usize>> = Vec::new();
            for i in 0..u.slots {
                if drop_slots.contains(&i) {
                    map.push(None);
                } else {
                    map.push(Some(d.slots));
                    d.slots += 1;
                }
            }
            let shift = |sp: &Spec| match sp {
                Spec::Const(v) => Spec::Const(v.clone()),
                Spec::Slot(i) => match map[*i] {
                    Some(j) => Spec::Slot(j),
                    // Replaced by the given value below.
                    None => Spec::Const(Value::Null),
                },
                Spec::Derived(b, n, sfx) => Spec::Derived(b + p, *n, sfx.clone()),
            };
            for q in 0..u.labels.len() {
                d.labels.push(u.labels[q].clone());
                let mut ps: Vec<(String, Spec)> = u.props[q]
                    .iter()
                    .map(|(k, sp)| (k.clone(), shift(sp)))
                    .collect();
                if q == 0 {
                    for (key, gsp) in &given {
                        let e = ps.iter_mut().find(|(k, _)| k == key).unwrap();
                        e.1 = gsp.clone();
                    }
                }
                d.props.push(ps);
            }
            for (src, ty, tg, props) in &u.edges {
                let tg = match tg {
                    Tgt::Pos(q) => Tgt::Pos(q + p),
                    Tgt::Handle(h) => Tgt::Handle(h.clone()),
                };
                d.edges.push((
                    src + p,
                    ty.clone(),
                    tg,
                    props.iter().map(|(k, sp)| (k.clone(), shift(sp))).collect(),
                ));
            }
        } else {
            assert_eq!(d.labels.len(), p, "position .{k} out of order");
            let mut labels = Vec::new();
            while l.eat(":") {
                labels.push(l.name());
            }
            let mut props = Vec::new();
            while l.eat(" ") {
                let key = l.name();
                l.expect("=");
                props.push((key, spec(&mut l, 0, &mut d.slots)));
            }
            assert!(l.done(), "left over: {:?}", l.rest());
            d.labels.push(labels);
            d.props.push(props);
        }
    }
    d
}

#[derive(Clone, Debug)]
struct Pat {
    /// (type, physical edge goes the way the run is read)
    steps: Vec<(String, bool)>,
}

fn parse_pattern(s: &str) -> Pat {
    let mut steps = Vec::new();
    for tok in s.split(' ') {
        if let Some(x) = tok.strip_prefix("<-").and_then(|x| x.strip_suffix('-')) {
            steps.push((x.trim_matches('`').to_string(), false));
        } else if let Some(x) = tok.strip_prefix('-').and_then(|x| x.strip_suffix("->")) {
            steps.push((x.trim_matches('`').to_string(), true));
        }
    }
    Pat { steps }
}

fn resolve(d: &Def, slots: &[Value]) -> Vec<Vec<(String, Value)>> {
    let mut out: Vec<Vec<(String, Value)>> = Vec::new();
    for p in 0..d.props.len() {
        let mut ps = Vec::new();
        for (k, sp) in &d.props[p] {
            let v = match sp {
                Spec::Const(v) => v.clone(),
                Spec::Slot(i) => slots[*i].clone(),
                Spec::Derived(b, n, sfx) => {
                    let base = out[*b]
                        .iter()
                        .find(|(bk, _)| bk == k)
                        .map(|(_, v)| v.clone())
                        .unwrap_or_else(|| panic!("derived {k} has no base value"));
                    let Value::Text(r) = base else {
                        panic!("derived from a non-text")
                    };
                    let head = match n {
                        None => r.as_str(),
                        Some(n) => &r[..*n],
                    };
                    Value::Text(format!("{head}{sfx}"))
                }
            };
            ps.push((k.clone(), v));
        }
        out.push(ps);
    }
    out
}

fn edge_spec(sp: &Spec, slots: &[Value]) -> Value {
    match sp {
        Spec::Const(v) => v.clone(),
        Spec::Slot(i) => slots[*i].clone(),
        Spec::Derived(..) => panic!("an edge value derived"),
    }
}

/// Reads a whole text (no identity) back as a snapshot. `dictionary` is
/// the text of a kept dictionary, for a text that names one.
fn decode(text: &str, dictionary: Option<&str>) -> Snap {
    let mut defs: HashMap<String, Def> = HashMap::new();
    let mut pats: HashMap<String, Pat> = HashMap::new();
    let mut sn = Snap::default();
    let mut all: Vec<&str> = Vec::new();
    if let Some(d) = dictionary {
        all.extend(d.lines());
    }
    all.extend(text.lines());
    // The header: `identity k1, k2` names the keys `@` handles carry.
    let head = text.lines().next().unwrap_or("");
    assert!(
        head.starts_with("# glider-compact/1"),
        "no header: {head:?}"
    );
    let keys: Vec<String> = match head.split_once(" identity ") {
        Some((_, ks)) => {
            let mut l = Lex::new(ks);
            let mut v = vec![l.name()];
            while l.eat(", ") {
                v.push(l.name());
            }
            v
        }
        None => Vec::new(),
    };
    let mut i = 0;
    while i < all.len() {
        let line = all[i];
        i += 1;
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let first = line.split(' ').next().unwrap();
        let is_t = first.len() > 1
            && first.starts_with('T')
            && first[1..].bytes().all(|c| c.is_ascii_digit());
        let is_p = first.len() > 1
            && first.starts_with('P')
            && first[1..].bytes().all(|c| c.is_ascii_digit());
        if is_t && line[first.len()..].starts_with(" #") {
            let mut body = Vec::new();
            while i < all.len() && all[i].starts_with(' ') {
                body.push(all[i]);
                i += 1;
            }
            let d = parse_def(&body, &defs);
            defs.insert(first.to_string(), d);
            continue;
        }
        if is_p && line[first.len()..].starts_with(" #") {
            let rest = &line[first.len() + 2..];
            let pat = rest.split_once(' ').map(|x| x.1).unwrap_or("");
            pats.insert(first.to_string(), parse_pattern(pat));
            continue;
        }
        if is_t {
            // A use.
            let d = defs
                .get(first)
                .unwrap_or_else(|| panic!("{first} not defined"))
                .clone();
            let mut l = Lex::new(&line[first.len()..]);
            let n = d.labels.len();
            let mut ids: Vec<String> = Vec::new();
            let mut by_pos: HashMap<usize, String> = HashMap::new();
            if !keys.is_empty() {
                // With identity: `.k=nN` for the positions written by id.
                if l.eat(" .") {
                    loop {
                        let st = l.i;
                        while l.peek().is_some_and(|c| c.is_ascii_digit()) {
                            l.i += 1;
                        }
                        let k: usize = std::str::from_utf8(&l.b[st..l.i]).unwrap().parse().unwrap();
                        l.expect("=");
                        by_pos.insert(k - 1, l.handle());
                        if !l.eat(",.") {
                            break;
                        }
                    }
                }
            } else if l.eat(" n") {
                let st = l.i;
                while l.peek().is_some_and(|c| c != b' ') {
                    l.i += 1;
                }
                let list = std::str::from_utf8(&l.b[st..l.i]).unwrap();
                for part in list.split(',') {
                    let part = part.trim_start_matches('n');
                    if let Some((a, b)) = part.split_once("..") {
                        let (a, b): (u64, u64) = (a.parse().unwrap(), b.parse().unwrap());
                        for x in a..=b {
                            ids.push(format!("n{x}"));
                        }
                    } else {
                        ids.push(format!("n{part}"));
                    }
                }
            }
            let mut slots = Vec::new();
            while l.eat(" ") {
                slots.push(l.value());
            }
            assert!(l.done(), "left over in use: {:?}", l.rest());
            assert_eq!(slots.len(), d.slots, "use {line:?}: slot count");
            let mut vals = resolve(&d, &slots);
            if !keys.is_empty() {
                // Every node not listed by id is written by its identity:
                // the first of the header's keys it has, with its value.
                ids = (0..n)
                    .map(|p| match by_pos.get(&p) {
                        Some(h) => h.clone(),
                        None => {
                            let (k, v) = keys
                                .iter()
                                .find_map(|k| {
                                    vals[p].iter().find(|(pk, v)| pk == k && *v != Value::Null)
                                })
                                .unwrap_or_else(|| {
                                    panic!(
                                        "use {line:?}: position .{} has no id and no identity",
                                        p + 1
                                    )
                                });
                            format!("@{k}={}", ex(v))
                        }
                    })
                    .collect();
            }
            let _ = &mut vals;
            assert_eq!(
                ids.len(),
                n,
                "use {line:?}: {} ids for {n} positions",
                ids.len()
            );
            for p in 0..n {
                sn.node(ids[p].clone(), d.labels[p].clone(), vals[p].clone());
            }
            for (src, ty, tg, props) in &d.edges {
                let to = match tg {
                    Tgt::Pos(q) => ids[*q].clone(),
                    Tgt::Handle(h) => h.clone(),
                };
                let ps = props
                    .iter()
                    .map(|(k, sp)| (k.clone(), edge_spec(sp, &slots)))
                    .collect();
                sn.edge(ids[*src].clone(), ty.clone(), to, ps);
            }
            continue;
        }
        // A card, an edge line or a chain use.
        let mut l = Lex::new(line);
        let h = l.handle();
        if l.eat(" -") {
            let ty = l.name();
            let mut props = Vec::new();
            if l.eat("{") {
                loop {
                    let key = l.name();
                    l.expect("=");
                    props.push((key, l.value()));
                    if l.eat("}") {
                        break;
                    }
                    l.expect(", ");
                }
            }
            l.expect("-> ");
            let mut tos = vec![l.handle()];
            if l.eat(" [") {
                // A chain use.
                let pat = pats
                    .get(&ty)
                    .unwrap_or_else(|| panic!("pattern {ty} not defined"))
                    .clone();
                let mut interior = Vec::new();
                loop {
                    interior.push(l.handle());
                    if l.eat("]") {
                        break;
                    }
                    l.expect(" ");
                }
                let mut run = vec![h.clone()];
                run.extend(interior);
                run.push(tos[0].clone());
                assert_eq!(
                    run.len() - 1,
                    pat.steps.len(),
                    "chain {line:?} against its pattern"
                );
                for (j, (ety, fwd)) in pat.steps.iter().enumerate() {
                    let (a, b) = (run[j].clone(), run[j + 1].clone());
                    if *fwd {
                        sn.edge(a, ety.clone(), b, vec![]);
                    } else {
                        sn.edge(b, ety.clone(), a, vec![]);
                    }
                }
                assert!(l.done());
                continue;
            }
            while l.eat(", ") {
                tos.push(l.handle());
            }
            assert!(l.done(), "left over in edge line: {:?}", l.rest());
            for to in tos {
                sn.edge(h.clone(), ty.clone(), to, props.clone());
            }
            continue;
        }
        let mut labels = Vec::new();
        while l.eat(":") {
            labels.push(l.name());
        }
        let mut props = Vec::new();
        // A card by identity leaves off the property its handle carries.
        if let Some(kv) = line.strip_prefix('@') {
            let mut y = Lex::new(kv);
            let key = y.name();
            y.expect("=");
            props.push((key, y.value()));
        }
        while l.eat(" ") {
            let key = l.name();
            l.expect("=");
            props.push((key, l.value()));
        }
        assert!(l.done(), "left over in card: {:?}", l.rest());
        sn.node(h, labels, props);
    }
    sn
}

fn plan_text(g: &mut Graph, args: &str, which: &str) -> String {
    text_of(&q(
        g,
        &format!("CALL structure.plan({args}, text: '{which}')"),
    ))
}

/// Both texts of the whole graph read back as the graph.
fn reads_back(g: &mut Graph, args: &str) -> (Snap, Snap, Snap) {
    let want = snap(g);
    let plain = decode(&plan_text(g, args, "plain"), None);
    let compact = decode(&plan_text(g, args, "compact"), None);
    (want, plain, compact)
}

fn diff(a: &Snap, b: &Snap) -> String {
    let mut out = String::new();
    for (k, v) in &a.edges {
        let w = b.edges.get(k).copied().unwrap_or(0);
        if w != *v {
            out.push_str(&format!("edge {k:?}: graph {v}, text {w}\n"));
        }
    }
    for (k, w) in &b.edges {
        if !a.edges.contains_key(k) {
            out.push_str(&format!("edge {k:?}: graph 0, text {w}\n"));
        }
    }
    for (k, v) in &a.nodes {
        if b.nodes.get(k) != Some(v) {
            out.push_str(&format!(
                "node {k}: graph {v:?}, text {:?}\n",
                b.nodes.get(k)
            ));
        }
    }
    for k in b.nodes.keys() {
        if !a.nodes.contains_key(k) {
            out.push_str(&format!("node {k} only in the text\n"));
        }
    }
    out
}

// ------------------------------------------------------- the decoder itself

#[test]
fn reviewer_decoder_reads_the_plain_and_compact_text_of_an_out_hierarchy() {
    // A sanity check of this decoder: stations with sides and locations
    // along an out hierarchy, a shared tool, edge properties.
    let mut g = Graph::memory();
    install(&mut g);
    let tool = g
        .add_node(&[s("Tool")], vec![(s("code"), t("TL"))])
        .unwrap();
    for c in 0..6 {
        let st = g
            .add_node(
                &[s("Station")],
                vec![
                    (s("code"), t(&format!("ST{c}"))),
                    (s("name"), t("A long station name that repeats")),
                ],
            )
            .unwrap();
        for side in ["L", "R"] {
            let sd = g
                .add_node(
                    &[s("Side")],
                    vec![
                        (s("code"), t(&format!("ST{c}{side}"))),
                        (s("name"), t("Side with a long constant name")),
                    ],
                )
                .unwrap();
            g.add_edge(st, sd, "HAS", vec![(s("w"), Value::Int(c as i64))])
                .unwrap();
            for k in 0..3 {
                let l = g
                    .add_node(
                        &[s("Loc")],
                        vec![
                            (s("code"), t(&format!("ST{c}{side}{k}"))),
                            (s("name"), t(&format!("Location number {k} long constant"))),
                        ],
                    )
                    .unwrap();
                g.add_edge(sd, l, "HAS", vec![]).unwrap();
                g.add_edge(l, tool, "USES", vec![]).unwrap();
            }
        }
    }
    let (want, plain, compact) = reads_back(&mut g, "types: ['HAS']");
    assert_eq!(plain, want, "{}", diff(&want, &plain));
    assert_eq!(compact, want, "{}", diff(&want, &compact));
}

// ------------------------------------------------------------- findings

/// Stations made of sides made of locations, along a hierarchy read
/// `dir: "in"` (each child points at its parent).
fn dir_in_plant(g: &mut Graph, copies: usize) {
    for c in 0..copies {
        let st = g
            .add_node(
                &[s("Station")],
                vec![
                    (s("code"), t(&format!("ST{c}"))),
                    (s("name"), t("A long station name that repeats")),
                ],
            )
            .unwrap();
        for side in ["L", "R"] {
            let sd = g
                .add_node(
                    &[s("Side")],
                    vec![
                        (s("code"), t(&format!("ST{c}{side}"))),
                        (s("name"), t("Side with a long constant name")),
                    ],
                )
                .unwrap();
            g.add_edge(sd, st, "PART_OF", vec![]).unwrap();
            for k in 0..3 {
                let l = g
                    .add_node(
                        &[s("Loc")],
                        vec![
                            (s("code"), t(&format!("ST{c}{side}{k}"))),
                            (s("name"), t(&format!("Location number {k} long constant"))),
                        ],
                    )
                    .unwrap();
                g.add_edge(l, sd, "PART_OF", vec![]).unwrap();
            }
        }
    }
}

#[test]
fn under_dir_in_a_nested_roots_edge_to_its_owner_is_written() {
    let mut g = Graph::memory();
    install(&mut g);
    dir_in_plant(&mut g, 6);
    let args = "types: ['PART_OF'], dir: 'in'";
    let compact = plan_text(&mut g, args, "compact");
    println!("{compact}");
    let (want, plain, got) = reads_back(&mut g, args);
    assert_eq!(plain, want, "plain: {}", diff(&want, &plain));
    println!(
        "graph edges {}, compact text edges {}",
        want.edge_count(),
        got.edge_count()
    );
    assert!(got == want, "compact:\n{}", diff(&want, &got));
}

/// Under `dir: "in"`: top <- x1 <- x2 <- x3 <- b, b with two children; b
/// is a template root (labels: ['B']), the x's a chain from top to b.
fn dir_in_chain_into_root(g: &mut Graph, copies: usize) {
    for c in 0..copies {
        let top = g
            .add_node(&[s("Top")], vec![(s("code"), t(&format!("TOP{c}")))])
            .unwrap();
        let mut prev = top;
        for k in 0..3 {
            let x = g
                .add_node(&[s("X")], vec![(s("code"), t(&format!("X{c}{k}")))])
                .unwrap();
            g.add_edge(x, prev, "PART_OF", vec![]).unwrap();
            prev = x;
        }
        let b = g
            .add_node(
                &[s("B")],
                vec![
                    (s("code"), t(&format!("B{c}"))),
                    (s("name"), t("A root with a long constant name")),
                ],
            )
            .unwrap();
        g.add_edge(b, prev, "PART_OF", vec![]).unwrap();
        for k in 0..2 {
            let d = g
                .add_node(
                    &[s("D")],
                    vec![
                        (s("code"), t(&format!("B{c}{k}"))),
                        (
                            s("name"),
                            t(&format!("A child {k} with a long constant name")),
                        ),
                    ],
                )
                .unwrap();
            g.add_edge(d, b, "PART_OF", vec![]).unwrap();
        }
    }
}

#[test]
fn under_dir_in_a_chain_never_writes_an_edge_a_use_writes() {
    let mut g = Graph::memory();
    install(&mut g);
    dir_in_chain_into_root(&mut g, 6);
    let args = "types: ['PART_OF'], dir: 'in', labels: ['B']";
    let compact = plan_text(&mut g, args, "compact");
    println!("{compact}");
    let (want, plain, got) = reads_back(&mut g, args);
    assert!(plain == want, "plain: {}", diff(&want, &plain));
    println!(
        "graph edges {}, compact text edges {}",
        want.edge_count(),
        got.edge_count()
    );
    assert!(got == want, "compact:\n{}", diff(&want, &got));
}

// ------------------------------------------------------------- fuzzing

/// Awkward random graphs: designs (some made of smaller designs), copied
/// many times with values that vary in some places, nodes with two
/// parents, self-loops, parallel edges, edge properties, references to a
/// pool, cycles along the hierarchy, isolated nodes, odd values.
fn awkward(seed: u64, dir_in: bool) -> Graph {
    let mut r = Rng(seed);
    let mut g = Graph::memory();
    let pool: Vec<u64> = (0..4)
        .map(|i| {
            g.add_node(&[s("Pool")], vec![(s("key"), t(&format!("p{i}")))])
                .unwrap()
        })
        .collect();
    let link = |g: &mut Graph, parent: u64, child: u64, props: Vec<(String, Value)>| {
        if dir_in {
            g.add_edge(child, parent, "H", props).unwrap();
        } else {
            g.add_edge(parent, child, "H", props).unwrap();
        }
    };
    let odd = [
        t(""),
        t("T1 n1..3 \"x\""),
        t("a\nb"),
        t("$"),
        t("~\"x\""),
        t(".2=T1"),
        t("@\"code\""),
        t("\u{0}\u{1f}\"\\"),
        t("ünï ☃ 字"),
        Value::Int(1),
        Value::Float(1.0),
        Value::Float(-0.0),
        Value::List(vec![Value::Float(0.5); 40]),
        Value::List(vec![]),
        Value::Bool(false),
    ];
    // Small designs, then larger ones that contain copies of them.
    struct D {
        labels: Vec<&'static str>,
        parents: Vec<usize>,
        consts: Vec<Value>,
        nested: Vec<(usize, usize)>, // (attach under node, small design)
        refs: Vec<(usize, usize)>,
        internal: Vec<(usize, usize, bool)>,
    }
    let mut designs: Vec<D> = Vec::new();
    for d in 0..6 {
        let n = 1 + r.below(5) as usize;
        let small = d < 3;
        designs.push(D {
            labels: (0..n)
                .map(|_| ["A", "B", "C"][r.below(3) as usize])
                .collect(),
            parents: (1..n).map(|i| r.below(i as u64) as usize).collect(),
            consts: (0..n)
                .map(|_| odd[r.below(odd.len() as u64) as usize].clone())
                .collect(),
            nested: if small {
                vec![]
            } else {
                (0..1 + r.below(3))
                    .map(|_| (r.below(n as u64) as usize, r.below(3) as usize))
                    .collect()
            },
            refs: (0..r.below(3))
                .map(|_| (r.below(n as u64) as usize, r.below(4) as usize))
                .collect(),
            internal: (0..r.below(3))
                .map(|_| {
                    (
                        r.below(n as u64) as usize,
                        r.below(n as u64) as usize,
                        r.below(2) == 0,
                    )
                })
                .collect(),
        });
    }
    let mut all: Vec<u64> = Vec::new();
    // Build one copy of design `d`; returns its root.
    fn build(
        g: &mut Graph,
        r: &mut Rng,
        designs: &[D],
        d: usize,
        copy: u64,
        pool: &[u64],
        odd: &[Value],
        link: &dyn Fn(&mut Graph, u64, u64, Vec<(String, Value)>),
        all: &mut Vec<u64>,
        serial: &mut u64,
    ) -> u64 {
        let de = &designs[d];
        *serial += 1;
        let code = format!("D{d}-{copy}-{serial}");
        let mut ids = Vec::new();
        for i in 0..de.labels.len() {
            let mut props = vec![
                (
                    s("code"),
                    t(&if i == 0 {
                        code.clone()
                    } else {
                        format!("{code}.{i}")
                    }),
                ),
                (s("c"), de.consts[i].clone()),
            ];
            match r.below(4) {
                0 => props.push((s("v"), odd[r.below(odd.len() as u64) as usize].clone())),
                1 => props.push((s("_hidden"), Value::Int(r.below(2) as i64))),
                _ => {}
            }
            ids.push(g.add_node(&[s(de.labels[i])], props).unwrap());
        }
        all.extend(&ids);
        for i in 1..ids.len() {
            let props = if r.below(4) == 0 {
                vec![(s("w"), Value::Int(r.below(2) as i64))]
            } else {
                vec![]
            };
            link(g, ids[de.parents[i - 1]], ids[i], props);
        }
        for &(i, p) in &de.refs {
            g.add_edge(ids[i], pool[p], "R", vec![]).unwrap();
        }
        for &(a, b, twice) in &de.internal {
            g.add_edge(ids[a], ids[b], "I", vec![(s("k"), Value::Int(1))])
                .unwrap();
            if twice {
                g.add_edge(ids[a], ids[b], "I", vec![]).unwrap();
            }
        }
        for &(at, sd) in &de.nested {
            let sub = build(
                g,
                r,
                designs,
                sd,
                copy * 10 + at as u64,
                pool,
                odd,
                link,
                all,
                serial,
            );
            link(g, ids[at], sub, vec![]);
        }
        ids[0]
    }
    let mut roots = Vec::new();
    let mut serial = 0u64;
    for d in 0..designs.len() {
        for copy in 0..2 + r.below(5) {
            roots.push(build(
                &mut g,
                &mut r,
                &designs,
                d,
                copy,
                &pool,
                &odd,
                &link,
                &mut all,
                &mut serial,
            ));
        }
    }
    // Hang some roots under others.
    for i in 1..roots.len() {
        if r.below(3) == 0 {
            let p = roots[r.below(i as u64) as usize];
            link(&mut g, p, roots[i], vec![]);
        }
    }
    // Noise: a second parent, a self-loop, a hierarchy cycle, isolated nodes.
    for _ in 0..3 {
        let a = all[r.below(all.len() as u64) as usize];
        let b = all[r.below(all.len() as u64) as usize];
        match r.below(4) {
            0 => link(&mut g, a, b, vec![]),
            1 => {
                g.add_edge(a, a, "H", vec![]).unwrap();
            }
            2 => {
                g.add_edge(a, b, "X", vec![(s("p"), t("T2 n1"))]).unwrap();
            }
            _ => {
                link(&mut g, a, b, vec![]);
                link(&mut g, b, a, vec![]);
            }
        }
    }
    for i in 0..2 {
        g.add_node(&[s("Lone")], vec![(s("code"), t(&format!("lone{i}")))])
            .unwrap();
    }
    g.add_node(&[], vec![(s("odd key"), t("x")), (s("code"), t("odd"))])
        .unwrap();
    install(&mut g);
    g
}

#[test]
fn fuzz_compact_text_reads_back_as_the_graph() {
    let mut failures = Vec::new();
    let mut checked = 0;
    // Every direction the procedures accept, the same 300 random graphs
    // each; by tokens and by bytes, by id and by identity; in memory, and
    // (by tokens) from a database file.
    for seed in 0..300u64 {
        for dir_in in [false, true] {
            for (by, ident, file) in [
                ("tokens", false, false),
                ("bytes", false, false),
                ("tokens", true, false),
                ("bytes", true, false),
                ("tokens", false, true),
                ("tokens", true, true),
            ] {
                let mem = awkward(seed, dir_in);
                let (mut g, path) = if file {
                    let (g, p) = on_file(&mem, &format!("fuzz{seed}"));
                    (g, Some(p))
                } else {
                    (mem, None)
                };
                let dir = if dir_in { "in" } else { "out" };
                let mut args = format!("types: ['H'], dir: '{dir}', by: '{by}', min_nodes: 1");
                let want = if ident {
                    args.push_str(", identity: ['code', 'key']");
                    snap_id(&g, &["code", "key"], None).0
                } else {
                    snap(&g)
                };
                let ctext = plan_text(&mut g, &args, "compact");
                let got = std::panic::catch_unwind(|| decode(&ctext, None));
                checked += 1;
                let ctx = format!("seed {seed} dir {dir} by {by} identity {ident} file {file}");
                match got {
                    Ok(got) if got == want => {}
                    Ok(got) => failures.push(format!("{ctx}:\n{}", diff(&want, &got))),
                    Err(_) => failures.push(format!("{ctx}: does not decode\n{ctext}")),
                }
                drop(g);
                if let Some(p) = path {
                    cleanup(&p);
                }
            }
        }
    }
    let ins = failures.iter().filter(|f| f.contains("dir in")).count();
    println!(
        "{} of {checked} texts differ from the graph ({ins} of them dir in)",
        failures.len()
    );
    let kinds = |pat: &str| failures.iter().filter(|f| f.contains(pat)).count();
    println!(
        "with a missing edge {}, with an extra edge {}, with an edge only in the text {}, with a node wrong {}, not decodable {}",
        kinds(", text 0"),
        kinds(", text 2"),
        kinds("graph 0"),
        kinds("node "),
        kinds("does not decode")
    );
    for f in failures.iter().filter(|f| f.contains("dir out")).take(3) {
        println!("{f}");
    }
    for f in failures.iter().filter(|f| f.contains("dir in")).take(2) {
        println!("{f}");
    }
    assert!(failures.is_empty());
}

fn ids_arg(ids: &[u64]) -> String {
    format!(
        "[{}]",
        ids.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Everything under `root` along out-edges of type H (a few levels).
fn below(g: &Graph, root: u64) -> Vec<u64> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() && out.len() < 200 {
        let v = out[i];
        i += 1;
        for e in g.edge_ids() {
            let r = g.edge(e).unwrap();
            if r.from == v && g.strings.name(r.etype) == "H" && !out.contains(&r.to) {
                out.push(r.to);
            }
        }
    }
    out
}

/// One random change to the graph.
fn mutate(g: &mut Graph, r: &mut Rng) -> String {
    let nodes = g.node_ids();
    let edges = g.edge_ids();
    let v = nodes[r.below(nodes.len() as u64) as usize];
    match r.below(6) {
        0 => {
            g.set_node_prop(v, "c", Value::Int(99)).unwrap();
            format!("set c on n{v}")
        }
        1 => {
            let e = edges[r.below(edges.len() as u64) as usize];
            g.delete_edge(e).unwrap();
            format!("deleted edge {e}")
        }
        2 => {
            let w = nodes[r.below(nodes.len() as u64) as usize];
            g.add_edge(v, w, "R", vec![]).unwrap();
            format!("added n{v} -R-> n{w}")
        }
        3 => {
            g.delete_node(v).unwrap();
            format!("deleted n{v}")
        }
        4 => {
            let w = g.add_node(&[s("A")], vec![(s("code"), t("new"))]).unwrap();
            g.add_edge(v, w, "H", vec![]).unwrap();
            format!("added child n{w} under n{v}")
        }
        _ => {
            // Point a reference at another pool node.
            let refs: Vec<u64> = edges
                .iter()
                .copied()
                .filter(|&e| g.strings.name(g.edge(e).unwrap().etype) == "R")
                .collect();
            if let Some(&e) = refs.first() {
                let er = g.edge(e).unwrap();
                g.delete_edge(e).unwrap();
                let w = nodes[r.below(4) as usize];
                g.add_edge(er.from, w, "R", vec![]).unwrap();
                format!("moved reference of n{} to n{w}", er.from)
            } else {
                "nothing".into()
            }
        }
    }
}

#[test]
fn fuzz_selections_with_a_kept_dictionary() {
    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0;
    let mut with_uses = 0;
    for seed in 0..120u64 {
        let mut g = awkward(seed, false);
        let mut r = Rng(seed ^ 0x5eed);
        let base = "types: ['H'], min_nodes: 1";
        q(&mut g, &format!("CALL structure.plan({base}, store: 'd')"));
        let dict = text_of(&q(
            &mut g,
            "CALL structure.plan(dictionary: 'd', text: 'dictionary')",
        ));
        let mut history = Vec::new();
        for round in 0..8 {
            if round > 0 && r.below(2) == 0 {
                history.push(mutate(&mut g, &mut r));
            }
            let nodes = g.node_ids();
            let pick = nodes[r.below(nodes.len() as u64) as usize];
            let mut sel = match r.below(5) {
                0 => below(&g, pick),
                1 => below(&g, pick)[1..].to_vec(),
                2 => {
                    let b = below(&g, pick);
                    b[..(b.len() + 1) / 2].to_vec()
                }
                3 => nodes.iter().copied().filter(|_| r.below(3) == 0).collect(),
                _ => {
                    let mut b = below(&g, pick);
                    b.push(pick);
                    b.push(nodes[r.below(nodes.len() as u64) as usize]);
                    b.push(999_999);
                    b
                }
            };
            if sel.is_empty() {
                sel.push(pick);
            }
            let a = ids_arg(&sel);
            let local = text_of(&q(
                &mut g,
                &format!("CALL structure.plan(dictionary: 'd', nodes: {a}, text: 'compact')"),
            ));
            let whole = text_of(&q(&mut g, &format!("CALL structure.plan(dictionary: 'd', nodes: {a}, find: 'graph', text: 'compact')")));
            let plain = text_of(&q(
                &mut g,
                &format!("CALL structure.plan({base}, nodes: {a}, text: 'plain')"),
            ));
            checked += 1;
            if local.contains("\nT") {
                with_uses += 1;
            }
            let ctx = format!("seed {seed} round {round} after {history:?} selection {a}");
            if local != whole {
                failures.push(format!("{ctx}: the local text differs from the whole-graph one\nlocal:\n{local}\nwhole:\n{whole}"));
                continue;
            }
            let want = decode(&plain, None);
            let d = dict.clone();
            let lc = local.clone();
            match std::panic::catch_unwind(move || decode(&lc, Some(&d))) {
                Ok(got) if got == want => {}
                Ok(got) => failures.push(format!(
                    "{ctx}: the compact text reads back otherwise than the plain\n{}\n{local}",
                    diff(&want, &got)
                )),
                Err(_) => failures.push(format!("{ctx}: does not decode\n{local}")),
            }
        }
    }
    println!(
        "{} of {checked} selections fail ({with_uses} of them use the dictionary)",
        failures.len()
    );
    for f in failures.iter().take(3) {
        println!("{f}\n----");
    }
    assert!(failures.is_empty());
}

// ------------------------------------------------------------- identity

/// Stations of two sides, each side using a shared tool; codes on every
/// node.
fn stations_with_tool(g: &mut Graph, copies: usize) -> u64 {
    let tool = g
        .add_node(
            &[s("Tool")],
            vec![(s("code"), t("TL-01")), (s("name"), t("Torque wrench"))],
        )
        .unwrap();
    for c in 0..copies {
        let st = g
            .add_node(
                &[s("Station")],
                vec![
                    (s("code"), t(&format!("ST{c}"))),
                    (s("name"), t("A long station name that repeats")),
                ],
            )
            .unwrap();
        for side in ["L", "R"] {
            let sd = g
                .add_node(
                    &[s("Side")],
                    vec![
                        (s("code"), t(&format!("ST{c}{side}"))),
                        (s("name"), t("Side with a long constant name")),
                    ],
                )
                .unwrap();
            g.add_edge(st, sd, "HAS", vec![]).unwrap();
            g.add_edge(sd, tool, "USES", vec![]).unwrap();
        }
    }
    tool
}

#[test]
fn a_stale_dictionary_never_names_a_node_by_an_identity_it_no_longer_has() {
    let mut g = Graph::memory();
    install(&mut g);
    let tool = stations_with_tool(&mut g, 6);
    let args = "types: ['HAS'], identity: 'code'";
    q(&mut g, &format!("CALL structure.plan({args}, store: 'd')"));
    // The tool is renamed; the stations are untouched.
    g.set_node_prop(tool, "code", t("TL-99")).unwrap();
    let station = 2u64;
    let sel = ids_arg(&below_any(&g, station, "HAS"));
    let dict = text_of(&q(
        &mut g,
        "CALL structure.plan(dictionary: 'd', text: 'dictionary')",
    ));
    let r = q(
        &mut g,
        &format!("CALL structure.plan(dictionary: 'd', nodes: {sel}, text: 'compact')"),
    );
    let compact = text_of(&r);
    let plain = plan_text(&mut g, &format!("{args}, nodes: {sel}"), "plain");
    println!("dictionary (kept before the rename):\n{dict}\ncompact text of station n{station} now:\n{compact}\nplain text now:\n{plain}");
    let fresh: Vec<String> = q(&mut g, "CALL structure.dictionaries()")
        .rows
        .iter()
        .map(|row| format!("{:?}", row))
        .collect();
    println!("dictionaries(): {fresh:?}");
    // The kept definition says the sides use @code="TL-01", which no node
    // is any more: the station is written plainly, and the text with its
    // dictionary says what the plain text says.
    assert!(plain.contains("@code=\"TL-99\""));
    assert!(dict.contains("@code=\"TL-01\""));
    assert!(
        decode(&compact, Some(&dict)) == decode(&plain, None),
        "compact:\n{compact}"
    );
    assert!(!compact.lines().any(|l| l.starts_with('T')), "{compact}");
}

fn below_any(g: &Graph, root: u64, ty: &str) -> Vec<u64> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let v = out[i];
        i += 1;
        for e in g.edge_ids() {
            let r = g.edge(e).unwrap();
            if r.from == v && g.strings.name(r.etype) == ty && !out.contains(&r.to) {
                out.push(r.to);
            }
        }
    }
    out
}

#[test]
fn identity_handles_name_their_key_and_exactly_one_node() {
    let mut g = Graph::memory();
    install(&mut g);
    // Two different nodes: one whose code is "5", one whose number is "5".
    let a = g
        .add_node(
            &[s("Part")],
            vec![(s("code"), t("5")), (s("name"), t("bolt"))],
        )
        .unwrap();
    let b = g
        .add_node(
            &[s("Part")],
            vec![(s("number"), t("5")), (s("name"), t("nut"))],
        )
        .unwrap();
    let c = g.add_node(&[s("Part")], vec![(s("code"), t("7"))]).unwrap();
    g.add_edge(c, a, "NEXT", vec![]).unwrap();
    g.add_edge(c, b, "NEXT", vec![]).unwrap();
    // Two nodes with the same code: written by id, code on the card.
    let d = g
        .add_node(
            &[s("Part")],
            vec![(s("code"), t("9")), (s("name"), t("washer"))],
        )
        .unwrap();
    let e = g
        .add_node(
            &[s("Part")],
            vec![(s("code"), t("9")), (s("name"), t("spring"))],
        )
        .unwrap();
    g.add_edge(c, d, "NEXT", vec![]).unwrap();
    g.add_edge(c, e, "NEXT", vec![]).unwrap();
    let plain = plan_text(
        &mut g,
        "types: ['NEXT'], identity: ['code', 'number']",
        "plain",
    );
    println!("{plain}");
    assert!(plain.starts_with("# glider-compact/1 identity code, number\n"));
    assert!(plain.contains("@code=\"5\":Part") && plain.contains("@number=\"5\":Part"));
    assert!(plain.contains(&format!("n{d}:Part name=\"washer\" code=\"9\"")));
    assert!(plain.contains(&format!("n{e}:Part name=\"spring\" code=\"9\"")));
    let (want, _) = snap_id(&g, &["code", "number"], None);
    assert!(
        decode(&plain, None) == want,
        "{}",
        diff(&want, &decode(&plain, None))
    );
}

#[test]
fn fuzz_the_plans_saving_is_plain_minus_compact_and_every_chosen_row_pays() {
    use glider::ext::context::tokens;
    let mut failures = Vec::new();
    let mut checked = 0;
    for seed in 0..150u64 {
        for by in ["tokens", "bytes"] {
            let mut g = awkward(seed, false);
            let mut r = Rng(seed + 77);
            let nodes = g.node_ids();
            let sels: Vec<Option<Vec<u64>>> = vec![
                None,
                Some(below(&g, nodes[r.below(nodes.len() as u64) as usize])),
                Some(nodes.iter().copied().filter(|_| r.below(2) == 0).collect()),
            ];
            for sel in sels {
                let mut args = format!("types: ['H'], by: '{by}', min_nodes: 1");
                if let Some(sel) = &sel {
                    args.push_str(&format!(", nodes: {}", ids_arg(sel)));
                }
                let p = plan_text(&mut g, &args, "plain");
                let rc = q(
                    &mut g,
                    &format!("CALL structure.plan({args}, text: 'compact')"),
                );
                let c = text_of(&rc);
                let tot = &rc.rows[0];
                let get = |name: &str| tot[col(&rc, name)].as_i64().unwrap();
                checked += 1;
                let ctx = format!(
                    "seed {seed} by {by} nodes {:?}",
                    sel.as_ref().map(|s| s.len())
                );
                if get("saving_bytes") != p.len() as i64 - c.len() as i64
                    || get("saving_tokens") != tokens(&p) as i64 - tokens(&c) as i64
                {
                    failures.push(format!("{ctx}: reported saving is not plain minus compact"));
                }
                if c.len() > p.len() && by == "bytes" || tokens(&c) > tokens(&p) && by == "tokens" {
                    failures.push(format!("{ctx}: compact longer than plain"));
                }
                for row in &rc.rows[1..] {
                    if row[col(&rc, "chosen")] == Value::Bool(true) {
                        let sv = row[col(
                            &rc,
                            if by == "bytes" {
                                "saving_bytes"
                            } else {
                                "saving_tokens"
                            },
                        )]
                        .as_i64()
                        .unwrap();
                        if sv <= 0 {
                            failures.push(format!(
                                "{ctx}: a chosen {} row saves {sv}: {row:?}",
                                row[0]
                            ));
                        }
                    }
                }
            }
        }
    }
    let k = |p: &str| failures.iter().filter(|f| f.contains(p)).count();
    println!(
        "{} problems in {checked} plans: by tokens {}, by bytes {}, saving misreported {}, compact longer {}",
        failures.len(),
        k("by tokens"),
        k("by bytes"),
        k("not plain minus"),
        k("compact longer")
    );
    for f in failures.iter().take(5) {
        println!("{f}");
    }
    assert!(failures.is_empty());
}

/// The same data inserted in reverse order after a gap of other nodes
/// (deleted again), so every id differs.
fn rebuilt(src: &Graph) -> Graph {
    let mut g = Graph::memory();
    for _ in 0..37 {
        let x = g.add_node(&[s("Gap")], vec![]).unwrap();
        g.delete_node(x).unwrap();
    }
    let mut map = HashMap::new();
    let mut ids = src.node_ids();
    ids.reverse();
    for id in ids {
        let n = g
            .add_node(&src.node_labels(id), src.node_props(id))
            .unwrap();
        map.insert(id, n);
    }
    let mut es = src.edge_ids();
    es.reverse();
    for e in es {
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
fn fuzz_the_same_data_under_other_ids_gives_the_same_dictionary_plan_and_text() {
    let mut failures = Vec::new();
    for seed in 0..150u64 {
        let mut a = awkward(seed, false);
        let mut b = rebuilt(&a);
        let args = "types: ['H'], min_nodes: 1, identity: ['code', 'key']";
        let mut outs = Vec::new();
        for g in [&mut a, &mut b] {
            let d = plan_text(g, args, "dictionary");
            let c = plan_text(g, args, "compact");
            let rows = q(g, &format!("CALL structure.plan({args})"));
            let rows: Vec<String> = rows.rows.iter().map(|r| format!("{r:?}")).collect();
            let tm = q(g, &format!("CALL structure.templates({args})"));
            let tm: Vec<String> = tm.rows.iter().map(|r| format!("{r:?}")).collect();
            outs.push((d, c, rows, tm));
        }
        let (x, y) = (&outs[0], &outs[1]);
        if x.0 != y.0 {
            let (xl, yl): (Vec<&str>, Vec<&str>) = (x.0.lines().collect(), y.0.lines().collect());
            let only_x: Vec<&&str> = xl.iter().filter(|l| !yl.contains(l)).collect();
            let only_y: Vec<&&str> = yl.iter().filter(|l| !xl.contains(l)).collect();
            failures.push(format!(
                "seed {seed}: dictionaries differ ({} and {} lines); only in the first {only_x:?}; only in the second {only_y:?}",
                xl.len(),
                yl.len()
            ));
        } else if x.1 != y.1 {
            let (xl, yl): (Vec<&str>, Vec<&str>) = (x.1.lines().collect(), y.1.lines().collect());
            let only_x: Vec<&&str> = xl.iter().filter(|l| !yl.contains(l)).take(6).collect();
            let only_y: Vec<&&str> = yl.iter().filter(|l| !xl.contains(l)).take(6).collect();
            let i = (0..xl.len().min(yl.len()))
                .find(|&i| xl[i] != yl[i])
                .unwrap_or(xl.len().min(yl.len()));
            failures.push(format!(
                "seed {seed}: compact texts differ; only in the first {only_x:?}; only in the second {only_y:?}; from line {i}: {:?} against {:?}",
                &xl[i..(i + 3).min(xl.len())],
                &yl[i..(i + 3).min(yl.len())]
            ));
        } else if x.2 != y.2 {
            failures.push(format!("seed {seed}: plan rows differ"));
        } else if x.3 != y.3 {
            failures.push(format!("seed {seed}: template rows differ"));
        }
    }
    println!("{} of 150 differ", failures.len());
    for f in failures.iter().take(2) {
        println!("{f}");
    }
    assert!(failures.is_empty());
}

/// Tops, each with two chains of three interior nodes; `swap` inserts the
/// second chain of each top before the first.
fn two_chains_per_top(swap: bool) -> Graph {
    let mut g = Graph::memory();
    for c in 0..8 {
        let top = g
            .add_node(&[s("Top")], vec![(s("code"), t(&format!("top{c}")))])
            .unwrap();
        let order: Vec<usize> = if swap { vec![1, 0] } else { vec![0, 1] };
        for k in order {
            let mut prev = top;
            for j in 0..4 {
                let x = g
                    .add_node(&[s("X")], vec![(s("code"), t(&format!("top{c}-{k}-{j}")))])
                    .unwrap();
                g.add_edge(prev, x, "H", vec![]).unwrap();
                prev = x;
            }
        }
    }
    install(&mut g);
    g
}

#[test]
fn chain_uses_and_patterns_depend_on_content_not_ids() {
    // No templates (min_count: is out of reach): only chains.
    let args = "types: ['H'], identity: 'code', min_count: 1000";
    let mut a = two_chains_per_top(false);
    let mut b = two_chains_per_top(true);
    let (ta, tb) = (
        plan_text(&mut a, args, "compact"),
        plan_text(&mut b, args, "compact"),
    );
    let first = |t: &str| {
        t.lines()
            .find(|l| l.starts_with("@\"top0\" -P"))
            .unwrap_or("")
            .to_string()
    };
    println!(
        "first chain use of top0, one insertion order: {}\nthe other: {}",
        first(&ta),
        first(&tb)
    );
    assert_eq!(snap_by_code(&a), snap_by_code(&b), "the same data");
    assert_eq!(ta, tb, "the same data under other ids gives another text");
}

fn snap_by_code(g: &Graph) -> Vec<String> {
    let code = |id: u64| format!("{:?}", g.node_prop(id, "code"));
    let mut v: Vec<String> = g
        .edge_ids()
        .iter()
        .map(|&e| {
            let r = g.edge(e).unwrap();
            format!(
                "{} {} {}",
                code(r.from),
                g.strings.name(r.etype),
                code(r.to)
            )
        })
        .collect();
    v.sort();
    v
}

#[test]
fn bounds_a_deep_chain_and_a_wide_hub() {
    use std::time::Instant;
    // A chain 200,000 deep.
    let mut g = Graph::memory();
    let mut prev = g
        .add_node(&[s("N")], vec![(s("i"), Value::Int(0))])
        .unwrap();
    for i in 1..200_000 {
        let x = g
            .add_node(&[s("N")], vec![(s("i"), Value::Int(i))])
            .unwrap();
        g.add_edge(prev, x, "H", vec![]).unwrap();
        prev = x;
    }
    install(&mut g);
    for call in [
        "CALL structure.templates(types: ['H'])",
        "CALL structure.paths(types: ['H'])",
        "CALL structure.paths(types: ['H'], kind: 'chains')",
        "CALL structure.plan(types: ['H'])",
        "CALL structure.expand(of: 'chain:2', types: ['H'])",
    ] {
        let t0 = Instant::now();
        let r = query::execute(&mut g, call);
        println!(
            "deep chain: {call}: {:?} in {:?}",
            r.as_ref().map(|r| r.rows.len()).map_err(|e| e.to_string()),
            t0.elapsed()
        );
    }
    // A hub with 200,000 children, each with two leaves.
    let mut g = Graph::memory();
    let hub = g.add_node(&[s("Hub")], vec![]).unwrap();
    for i in 0..200_000 {
        let c = g
            .add_node(
                &[s("C")],
                vec![
                    (s("code"), t(&format!("c{i}"))),
                    (s("name"), t("a constant name")),
                ],
            )
            .unwrap();
        g.add_edge(hub, c, "H", vec![]).unwrap();
        for k in 0..2 {
            let l = g
                .add_node(&[s("L")], vec![(s("code"), t(&format!("c{i}-{k}")))])
                .unwrap();
            g.add_edge(c, l, "H", vec![]).unwrap();
        }
    }
    install(&mut g);
    for call in [
        "CALL structure.templates(types: ['H'])",
        "CALL structure.plan(types: ['H'], store: 'd')",
        "CALL structure.plan(dictionary: 'd', nodes: [1, 2, 3, 4], text: 'compact')",
        "CALL structure.plan(dictionary: 'd', nodes: [1], text: 'compact')",
    ] {
        let t0 = Instant::now();
        let r = query::execute(&mut g, call);
        println!(
            "hub: {call}: {:?} in {:?}",
            r.as_ref().map(|r| r.rows.len()).map_err(|e| e.to_string()),
            t0.elapsed()
        );
    }
}

#[test]
fn kept_dictionary_kept_again_under_the_same_name_holds_only_the_new_one() {
    for seed in 0..40u64 {
        let mut g = awkward(seed, false);
        q(
            &mut g,
            "CALL structure.plan(types: ['H'], min_nodes: 1, store: 'd')",
        );
        let d1 = text_of(&q(
            &mut g,
            "CALL structure.plan(dictionary: 'd', text: 'dictionary')",
        ));
        let r2 = q(
            &mut g,
            "CALL structure.plan(types: ['H', 'I'], min_nodes: 3, store: 'd', text: 'dictionary')",
        );
        let d2 = text_of(&r2);
        let back = text_of(&q(
            &mut g,
            "CALL structure.plan(dictionary: 'd', text: 'dictionary')",
        ));
        if d2.is_empty() {
            // Nothing chosen: text: 'dictionary' is empty, what is kept is the legend alone.
            continue;
        }
        assert_eq!(back, d2, "seed {seed}");
        // Every definition the kept dictionary holds is one the new text has.
        let listed = q(&mut g, "CALL structure.templates(dictionary: 'd')");
        let defs = d2.lines().filter(|l| l.starts_with('T')).count();
        assert!(
            listed.rows.len() <= defs,
            "seed {seed}: {} rows for {defs} definitions (first dictionary had {})",
            listed.rows.len(),
            d1.lines().filter(|l| l.starts_with('T')).count()
        );
        // And writing with it reads back.
        let c = plan_text(&mut g, "dictionary: 'd'", "compact");
        let want = snap(&g);
        let got = decode(&c, Some(&d2));
        assert!(got == want, "seed {seed}: {}", diff(&want, &got));
    }
}

const MFG_TYPES: &str = "['HAS_PLANT', 'HAS_SHOP', 'HAS_LINE', 'HAS_STATION', 'HAS_SIDE', 'HAS_LOCATION', 'HAS_ELEMENT', 'HAS_INSTRUCTION']";

fn mfg(n: &str) -> Option<Graph> {
    let base = std::env::var("GLIDER_BENCH_DATA").unwrap_or_else(|_| {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../glider-bench-data").to_string()
    });
    let p = format!("{base}/mfg/{n}/graph.jsonl");
    let f = std::fs::File::open(&p).ok()?;
    let mut g = Graph::memory();
    query::import_jsonl_from(&mut g, &mut std::io::BufReader::new(f), 1 << 20).unwrap();
    install(&mut g);
    Some(g)
}

#[test]
fn manufacturing_graph_compact_text_reads_back_and_identity_handles_are_unique() {
    // GLIDER_NOTATION_SIZES lists other datasets (the 1M one is read back
    // this way, on demand: its text is 291 MB).
    let sizes =
        std::env::var("GLIDER_NOTATION_SIZES").unwrap_or_else(|_| "n10000-s1,n100000-s1".into());
    for n in sizes.split(',') {
        let Some(mut g) = mfg(n) else {
            println!("{n}: no data");
            continue;
        };
        let args = format!("types: {MFG_TYPES}");
        let want = snap(&g);
        let c = plan_text(&mut g, &args, "compact");
        let got = decode(&c, None);
        println!(
            "{n}: {} nodes, {} edges; compact {} bytes; reads back exactly: {}",
            want.nodes.len(),
            want.edge_count(),
            c.len(),
            got == want
        );
        assert!(
            got == want,
            "{}",
            diff(&want, &got)
                .lines()
                .take(10)
                .collect::<Vec<_>>()
                .join("\n")
        );
        drop(c);
        // And by identity, every node named by code or number.
        let ida = format!("{args}, identity: ['code', 'number']");
        let c = plan_text(&mut g, &ida, "compact");
        let (want_id, _) = snap_id(&g, &["code", "number"], None);
        let got = decode(&c, None);
        println!("{n}: by identity, reads back exactly: {}", got == want_id);
        assert!(
            got == want_id,
            "{}",
            diff(&want_id, &got)
                .lines()
                .take(10)
                .collect::<Vec<_>>()
                .join("\n")
        );
        drop(c);
        // Handles under identity: ['code', 'number'].
        let mut by: HashMap<String, usize> = HashMap::new();
        let mut none = 0;
        for id in g.node_ids() {
            let h = g
                .node_prop(id, "code")
                .or_else(|| g.node_prop(id, "number"));
            match h {
                Some(v) => *by.entry(ex(&v)).or_default() += 1,
                None => none += 1,
            }
        }
        let shared = by.values().filter(|&&k| k > 1).count();
        println!("{n}: identity ['code', 'number']: {shared} handles shared by two or more nodes, {none} nodes without one");
        if n == "n10000-s1" {
            // A station with the dictionary, against its plain text.
            q(&mut g, &format!("CALL structure.plan({args}, store: 'd')"));
            let dict = text_of(&q(
                &mut g,
                "CALL structure.plan(dictionary: 'd', text: 'dictionary')",
            ));
            let st: Vec<u64> = g
                .node_ids()
                .into_iter()
                .filter(|&id| g.node_labels(id).contains(&s("Station")))
                .take(5)
                .collect();
            for root in st {
                let sel = ids_arg(&below_typed(&g, root));
                let c = text_of(&q(
                    &mut g,
                    &format!("CALL structure.plan(dictionary: 'd', nodes: {sel}, text: 'compact')"),
                ));
                let p = plan_text(&mut g, &format!("{args}, nodes: {sel}"), "plain");
                assert!(
                    decode(&c, Some(&dict)) == decode(&p, None),
                    "station n{root}"
                );
            }
        }
    }
}

fn below_typed(g: &Graph, root: u64) -> Vec<u64> {
    let types = ["HAS_SIDE", "HAS_LOCATION", "HAS_ELEMENT", "HAS_INSTRUCTION"];
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let v = out[i];
        i += 1;
        let mut adj = Vec::new();
        g.for_each_adj(v, glider::graph::Dir::Out, None, |a| {
            adj.push((a.other, a.etype))
        });
        for (o, ty) in adj {
            if types.contains(&g.strings.name(ty)) && !out.contains(&o) {
                out.push(o);
            }
        }
    }
    out
}

#[test]
fn the_dictionary_text_returned_is_what_is_kept_even_when_nothing_is_chosen() {
    let mut g = Graph::memory();
    install(&mut g);
    q(
        &mut g,
        "CREATE (:A {k: 'one'})-[:H]->(:B {k: 'uno'}), (:A {k: 'two'})-[:H]->(:B {k: 'dos'})",
    );
    let r = q(
        &mut g,
        "CALL structure.plan(types: ['H'], store: 'd', text: 'dictionary')",
    );
    assert_eq!(r.rows[0][col(&r, "chosen")], Value::Bool(false));
    let returned = text_of(&r);
    let kept = text_of(&q(
        &mut g,
        "CALL structure.plan(dictionary: 'd', text: 'dictionary')",
    ));
    assert!(!returned.is_empty());
    assert_eq!(returned, kept);
    let d = q(&mut g, "CALL structure.dictionaries()");
    assert_eq!(r.rows[0][col(&r, "id")], d.rows[0][col(&d, "hash")]);
}

#[test]
fn a_kept_dictionary_is_stale_only_when_a_node_or_an_edge_changed_given_the_journal() {
    let fresh =
        |g: &mut Graph| q(g, "CALL structure.dictionaries()").rows[0][2] == Value::Bool(true);
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    install(&mut g);
    dir_in_plant(&mut g, 4);
    q(
        &mut g,
        "CALL structure.plan(types: ['PART_OF'], dir: 'in', store: 'd')",
    );
    assert!(fresh(&mut g));
    // An index, another extension, a kept colouring, a write that changes
    // nothing: still fresh, and require accepts it.
    q(&mut g, "INDEX ON :Station(code)");
    q(&mut g, "CREATE EXTENSION context");
    q(&mut g, "CALL structure.colors(depth: 1, store: 'c')");
    q(&mut g, "MATCH (n:Station) SET n:Station");
    assert!(fresh(&mut g));
    q(
        &mut g,
        "CALL structure.plan(dictionary: 'd', fresh: 'require')",
    );
    // A node change makes it stale.
    q(&mut g, "MATCH (n:Loc) SET n.x = 1");
    assert!(!fresh(&mut g));
    let e = match query::execute(
        &mut g,
        "CALL structure.plan(dictionary: 'd', fresh: 'require')",
    ) {
        Ok(_) => panic!("a stale dictionary was accepted"),
        Err(e) => e.to_string(),
    };
    assert!(e.contains("has changed since"), "{e}");
    // Without the journal, the conservative answer: an index makes it stale.
    let mut h = Graph::memory();
    install(&mut h);
    dir_in_plant(&mut h, 4);
    q(
        &mut h,
        "CALL structure.plan(types: ['PART_OF'], dir: 'in', store: 'd')",
    );
    q(&mut h, "INDEX ON :Station(code)");
    assert!(!fresh(&mut h));
}
