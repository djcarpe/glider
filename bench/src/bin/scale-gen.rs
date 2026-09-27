//! scale-gen: deterministic graphs of any size, with deliberately varied
//! relationship structure, written either as a snapshot image or as a log.
//!
//!   scale-gen --size 10GiB --out big.gldb              # image, via image::build
//!   scale-gen --nodes 1200000 --log --out big-log.gldb # the same graph, as a log
//!
//! Every node and edge is a pure function of (seed, id), so the generator
//! never holds the graph: the image builder visits it in passes, and the log
//! writer streams it once. Given the same `--nodes` and `--seed`, `--log` and
//! image mode describe exactly the same graph — replaying the log and
//! compacting it yields the same image, byte for byte, apart from the
//! generation id in the file header.
//!
//! Relationship regimes, chosen to exercise different traversal shapes:
//!
//! | type                | shape                                                |
//! |---------------------|------------------------------------------------------|
//! | KNOWS               | communities of 1000, plus power-law hubs             |
//! | FOLLOWS             | reciprocal pairs                                     |
//! | WORKS_AT            | many-to-one, power-law company popularity            |
//! | REVIEWED            | bipartite person -> product, with text               |
//! | SUBSIDIARY_OF       | company tree, branching 4                            |
//! | IN_CATEGORY         | product -> leaf-biased category                      |
//! | PARENT_OF           | category tree, branching 6, ~7 levels                |
//! | SIMILAR             | dense 5-cliques of products, both directions         |
//! | PLACED              | customer -> order, heavy-tailed customers            |
//! | CONTAINS            | order -> product, 1-8 lines, repeats (multi-edges)   |
//! | AUTHORED            | person -> document                                   |
//! | CITES               | document DAG towards older documents, power-law      |
//! | MENTIONS            | document -> person / company / product (mixed)       |
//! | NEXT                | one long chain through every event                   |
//! | RETRY               | self-loops on every 50th event                       |
//! | TRIGGERED_BY        | event -> person                                      |
//!
//! About 1% of ids are holes, as deletes would leave. Values cover every
//! type glider stores: ints (including extremes), floats, text with
//! non-ASCII, bools, nulls, and lists (including nested and mixed).

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

use glider::legacy::image::{self, ImageSource};
use glider::store::{LogWriter, Op};
use glider::value::Value;

// ----------------------------------------------------------------- random

#[inline]
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

struct R(u64);

impl R {
    fn new(seed: u64, id: u64, salt: u64) -> R {
        R(mix(seed ^ mix(id.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ salt)))
    }
    #[inline]
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.0)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
    /// Heavy-tailed count: mostly small, occasionally huge.
    fn pareto(&mut self, min: f64, alpha: f64, cap: u64) -> u64 {
        let u = self.unit().max(1e-12);
        ((min / u.powf(1.0 / alpha)) as u64).min(cap)
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
}

const WORDS: &[&str] = &[
    "graph", "edge", "node", "river", "stone", "lantern", "harbour", "signal", "orbit",
    "meadow", "copper", "velvet", "thunder", "quiet", "ember", "atlas", "cipher", "delta",
    "fjord", "glacier", "horizon", "island", "jasmine", "kernel", "lattice", "marble",
    "nectar", "oasis", "pixel", "quartz", "rhythm", "saffron", "tundra", "umbra", "vertex",
    "willow", "xenon", "yonder", "zephyr", "naïve", "Zürich", "café", "Øresund", "São",
    "über", "東京", "데이터", "Ελλάδα", "Москва", "मुंबई", "🚀", "🌍", "façade", "jalapeño",
];
const COUNTRIES: &[&str] = &[
    "GB", "US", "DE", "FR", "JP", "BR", "IN", "CN", "NG", "ZA", "MX", "CA", "AU", "ES", "IT",
    "SE", "NO", "KR", "AR", "EG", "TR", "PL", "NL", "BE", "CH", "AT", "IE", "PT", "GR", "FI",
];
const CITIES: &[&str] = &[
    "London", "Paris", "Tokyo", "São Paulo", "Mumbai", "Lagos", "Zürich", "Toronto",
    "Sydney", "Berlin", "Seoul", "Cairo", "Oslo", "Madrid", "Mexico City", "Montréal",
];
const INDUSTRIES: &[&str] = &[
    "software", "retail", "energy", "health", "logistics", "finance", "media", "farming",
];
const STATUSES: &[&str] = &["placed", "paid", "shipped", "delivered", "returned", "cancelled"];
const LANGS: &[&str] = &["en", "fr", "de", "ja", "pt", "hi", "ar", "日本語"];
const EVENT_KINDS: &[&str] = &["click", "view", "purchase", "login", "error", "retry"];
const ROLES: &[&str] = &["engineer", "manager", "analyst", "designer", "director", "intern"];
const CHANNELS: &[&str] = &["web", "app", "phone", "store"];

fn text(r: &mut R, min_words: u64, max_words: u64) -> String {
    let n = min_words + r.below(max_words - min_words + 1);
    let mut s = String::with_capacity(n as usize * 7);
    for i in 0..n {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(r.pick(WORDS));
    }
    s
}

fn date(r: &mut R) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        1995 + r.below(31),
        1 + r.below(12),
        1 + r.below(28)
    )
}

// ------------------------------------------------------------------ layout

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Person,
    Company,
    Product,
    Order,
    Document,
    Category,
    Event,
}

const KINDS: [(Kind, f64); 7] = [
    (Kind::Person, 0.40),
    (Kind::Company, 0.015),
    (Kind::Product, 0.08),
    (Kind::Order, 0.25),
    (Kind::Document, 0.13),
    (Kind::Category, 0.005),
    (Kind::Event, 0.11),
];

const INDEXES: &[(&str, &str)] = &[
    ("Person", "email"),
    ("Person", "country"),
    ("Person", "age"),
    ("Product", "sku"),
    ("Order", "ref"),
    ("Event", "seq"),
];

/// Every label, type and key the generator can emit — what the string table
/// must end up holding.
const NODE_STRINGS: &[&str] = &[
    "Person", "Employee", "VIP", "Company", "Product", "Discontinued", "Order", "Document",
    "Draft", "Category", "Event", "name", "email", "age", "country", "city", "joined",
    "score", "active", "tags", "bio", "nickname", "balance", "industry", "founded",
    "revenue", "hq", "sku", "price", "stock", "attrs", "rating", "ref", "total", "status",
    "placed", "items", "gift", "title", "body", "lang", "words", "embedding", "depth",
    "seq", "kind", "ts", "payload",
];
const EDGE_STRINGS: &[&str] = &[
    "KNOWS", "since", "weight", "FOLLOWS", "WORKS_AT", "role", "REVIEWED", "rating", "text",
    "SUBSIDIARY_OF", "stake", "IN_CATEGORY", "PARENT_OF", "SIMILAR", "score", "PLACED",
    "channel", "CONTAINS", "qty", "price", "AUTHORED", "CITES", "MENTIONS", "offset", "NEXT",
    "gap", "RETRY", "attempt", "TRIGGERED_BY",
];

struct Gen {
    seed: u64,
    /// Ids run 1..=n; a few are holes.
    n: u64,
    ranges: [(u64, u64); 7],
}

type Props = Vec<(&'static str, Value)>;

impl Gen {
    fn new(n: u64, seed: u64) -> Gen {
        let n = n.max(1000);
        let mut ranges = [(0, 0); 7];
        let mut start = 1u64;
        for (i, (_, frac)) in KINDS.iter().enumerate() {
            let len = if i == KINDS.len() - 1 {
                n + 1 - start
            } else {
                ((n as f64 * frac) as u64).max(20)
            };
            ranges[i] = (start, start + len);
            start += len;
        }
        Gen { seed, n, ranges }
    }

    fn exists(&self, id: u64) -> bool {
        if id == 0 || id > self.n {
            return false;
        }
        // Block starts and the last id always exist; ~1% elsewhere are holes.
        if id == self.n || self.ranges.iter().any(|(s, _)| *s == id) {
            return true;
        }
        mix(self.seed ^ id.wrapping_mul(0xA24B_AED4_963E_E407)) % 97 != 0
    }

    fn kind(&self, id: u64) -> Kind {
        for (i, (s, e)) in self.ranges.iter().enumerate() {
            if id >= *s && id < *e {
                return KINDS[i].0;
            }
        }
        Kind::Event
    }

    fn range(&self, k: Kind) -> (u64, u64) {
        self.ranges[KINDS.iter().position(|(x, _)| *x == k).unwrap()]
    }

    /// The existing member of `k` at or after fraction `f` of its block.
    fn member(&self, k: Kind, f: f64) -> u64 {
        let (s, e) = self.range(k);
        let len = e - s;
        let mut id = s + ((len as f64 * f) as u64).min(len - 1);
        while !self.exists(id) {
            id = if id + 1 < e { id + 1 } else { s };
        }
        id
    }

    fn uniform(&self, k: Kind, r: &mut R) -> u64 {
        self.member(k, r.unit())
    }

    /// Power-law choice: low offsets in the block are hubs.
    fn popular(&self, k: Kind, r: &mut R, skew: f64) -> u64 {
        self.member(k, r.unit().powf(skew))
    }

    // ------------------------------------------------------------- nodes

    /// A node's labels and properties. With `values` false the values are
    /// all Null — enough for working out the string table cheaply.
    fn node(&self, id: u64, values: bool, labels: &mut Vec<&'static str>, props: &mut Props) {
        labels.clear();
        props.clear();
        let mut r = R::new(self.seed, id, 1);
        // Values are always computed, even when discarded: they draw from
        // the same random stream as later decisions, which must not change.
        macro_rules! put {
            ($k:expr, $v:expr) => {{
                let v = $v;
                props.push(($k, if values { v } else { Value::Null }))
            }};
        }
        match self.kind(id) {
            Kind::Person => {
                labels.push("Person");
                if r.chance(0.6) {
                    labels.push("Employee");
                }
                if r.chance(0.01) {
                    labels.push("VIP");
                }
                put!("name", Value::Text(format!("{} {}", r.pick(WORDS), r.pick(WORDS))));
                put!("email", Value::Text(format!("user{id:010}@example.org")));
                put!("age", Value::Int(18 + r.below(70) as i64));
                put!("country", Value::Text(r.pick(COUNTRIES).into()));
                put!("city", Value::Text(r.pick(CITIES).into()));
                put!("joined", Value::Text(date(&mut r)));
                put!("score", Value::Float(r.unit() * 100.0));
                put!("active", Value::Bool(r.chance(0.8)));
                let ntags = r.below(5);
                put!(
                    "tags",
                    Value::List((0..ntags).map(|_| Value::Text(r.pick(WORDS).into())).collect())
                );
                put!("bio", Value::Text(text(&mut r, 8, 60)));
                if r.chance(0.5) {
                    put!("nickname", if r.chance(0.2) { Value::Null } else { Value::Text(r.pick(WORDS).into()) });
                }
                if r.chance(0.05) {
                    put!("balance", Value::Int([i64::MIN, i64::MAX, -1, 0][r.below(4) as usize]));
                }
            }
            Kind::Company => {
                labels.push("Company");
                put!("name", Value::Text(format!("{} {} Ltd", r.pick(WORDS), r.pick(WORDS))));
                put!("industry", Value::Text(r.pick(INDUSTRIES).into()));
                put!("founded", Value::Int(1850 + r.below(175) as i64));
                put!("revenue", Value::Float(r.unit() * 1e9));
                put!("hq", Value::Text(r.pick(CITIES).into()));
            }
            Kind::Product => {
                labels.push("Product");
                if r.chance(0.05) {
                    labels.push("Discontinued");
                }
                put!("sku", Value::Text(format!("SKU-{id:010}")));
                put!("name", Value::Text(text(&mut r, 2, 4)));
                put!("price", Value::Float((r.below(100_000) as f64) / 100.0));
                put!("stock", Value::Int(r.below(10_000) as i64 - 50));
                put!(
                    "attrs",
                    Value::List(vec![
                        Value::Int(r.below(10) as i64),
                        Value::Text(r.pick(WORDS).into()),
                        Value::Bool(r.chance(0.5)),
                        Value::List(vec![Value::Float(r.unit()), Value::Null]),
                    ])
                );
                put!("rating", if r.chance(0.3) { Value::Null } else { Value::Float(1.0 + r.unit() * 4.0) });
            }
            Kind::Order => {
                labels.push("Order");
                put!("ref", Value::Text(format!("ORD-{id:010}")));
                put!("total", Value::Float((r.below(200_000) as f64) / 100.0));
                put!("status", Value::Text(r.pick(STATUSES).into()));
                put!("placed", Value::Text(date(&mut r)));
                put!("items", Value::Int(1 + r.below(8) as i64));
                put!("gift", Value::Bool(r.chance(0.1)));
            }
            Kind::Document => {
                labels.push("Document");
                if r.chance(0.1) {
                    labels.push("Draft");
                }
                put!("title", Value::Text(text(&mut r, 3, 9)));
                put!("body", Value::Text(text(&mut r, 40, 400)));
                put!("lang", Value::Text(r.pick(LANGS).into()));
                put!("score", Value::Float(r.unit()));
                put!("words", Value::Int(r.below(5000) as i64));
                put!("embedding", Value::List((0..8).map(|_| Value::Float(r.unit() * 2.0 - 1.0)).collect()));
            }
            Kind::Category => {
                labels.push("Category");
                let (s, _) = self.range(Kind::Category);
                put!("name", Value::Text(format!("{} {}", r.pick(WORDS), id - s)));
                put!("depth", Value::Int(self.category_depth(id) as i64));
            }
            Kind::Event => {
                labels.push("Event");
                put!("seq", Value::Int(id as i64));
                put!("kind", Value::Text(r.pick(EVENT_KINDS).into()));
                put!("ts", Value::Int(1_600_000_000 + id as i64 * 7));
                put!("payload", Value::Text(text(&mut r, 1, 12)));
            }
        }
    }

    fn category_parent(&self, id: u64) -> Option<u64> {
        let (s, _) = self.range(Kind::Category);
        let i = id - s;
        (i > 0).then(|| {
            let mut p = s + (i - 1) / 6;
            while !self.exists(p) {
                p -= 1;
            }
            p
        })
    }

    fn category_depth(&self, id: u64) -> u64 {
        let mut d = 0;
        let mut cur = id;
        while let Some(p) = self.category_parent(cur) {
            d += 1;
            cur = p;
        }
        d
    }

    // ------------------------------------------------------------- edges

    /// The edges a node owns, in a fixed order. Edge ids are assigned in
    /// the order all nodes' edges are visited.
    fn edges(&self, id: u64, values: bool, out: &mut Vec<(u64, u64, &'static str, Props)>) {
        out.clear();
        let mut r = R::new(self.seed, id, 2);
        macro_rules! e {
            ($from:expr, $to:expr, $t:expr, [$($k:expr => $v:expr),*]) => {{
                #[allow(unused_mut)]
                let mut props: Props = Vec::new();
                $(
                    let v = $v;
                    props.push(($k, if values { v } else { Value::Null }));
                )*
                out.push(($from, $to, $t, props));
            }};
        }
        match self.kind(id) {
            Kind::Person => {
                let (ps, pe) = self.range(Kind::Person);
                // KNOWS: mostly within a community of 1000, some to hubs.
                let deg = r.pareto(2.0, 1.3, 5000);
                let base = ps + (id - ps) / 1000 * 1000;
                for _ in 0..deg {
                    let to = if r.chance(0.8) {
                        let span = (pe - base).min(1000);
                        self.member(Kind::Person, ((base - ps) + r.below(span)) as f64 / (pe - ps) as f64)
                    } else {
                        self.popular(Kind::Person, &mut r, 3.0)
                    };
                    e!(id, to, "KNOWS", ["since" => Value::Int(1990 + r.below(36) as i64), "weight" => Value::Float(r.unit())]);
                }
                // FOLLOWS: reciprocal pairs (2k, 2k+1) where the pair agrees.
                let partner = if id % 2 == 0 { id + 1 } else { id - 1 };
                if partner >= ps && partner < pe && self.exists(partner) && mix(self.seed ^ (id / 2)) % 3 == 0 {
                    e!(id, partner, "FOLLOWS", []);
                }
                if r.chance(0.6) {
                    let c = self.popular(Kind::Company, &mut r, 2.0);
                    e!(id, c, "WORKS_AT", ["role" => Value::Text(r.pick(ROLES).into()), "since" => Value::Int(2000 + r.below(26) as i64)]);
                }
                if r.chance(0.2) {
                    for _ in 0..1 + r.below(5) {
                        let p = self.uniform(Kind::Product, &mut r);
                        e!(id, p, "REVIEWED", ["rating" => Value::Int(1 + r.below(5) as i64), "text" => Value::Text(text(&mut r, 5, 30))]);
                    }
                }
            }
            Kind::Company => {
                let (s, _) = self.range(Kind::Company);
                let i = id - s;
                if i > 0 {
                    let mut p = s + (i - 1) / 4;
                    while !self.exists(p) {
                        p -= 1;
                    }
                    e!(id, p, "SUBSIDIARY_OF", ["stake" => Value::Float(r.unit())]);
                }
            }
            Kind::Product => {
                // Leaf-biased category.
                let c = self.member(Kind::Category, 0.5 + r.unit() * 0.5);
                e!(id, c, "IN_CATEGORY", []);
                // A dense clique of five consecutive products.
                let (s, e_) = self.range(Kind::Product);
                let g = s + (id - s) / 5 * 5;
                for other in g..(g + 5).min(e_) {
                    if other != id && self.exists(other) {
                        e!(id, other, "SIMILAR", ["score" => Value::Float(r.unit())]);
                    }
                }
            }
            Kind::Order => {
                let buyer = self.popular(Kind::Person, &mut r, 2.0);
                e!(buyer, id, "PLACED", ["channel" => Value::Text(r.pick(CHANNELS).into())]);
                // Lines may repeat a product: multi-edges.
                let lines = 1 + r.below(8);
                let first = self.uniform(Kind::Product, &mut r);
                for _ in 0..lines {
                    let p = if r.chance(0.25) { first } else { self.uniform(Kind::Product, &mut r) };
                    e!(id, p, "CONTAINS", ["qty" => Value::Int(1 + r.below(5) as i64), "price" => Value::Float((r.below(50_000) as f64) / 100.0)]);
                }
            }
            Kind::Document => {
                let author = self.popular(Kind::Person, &mut r, 1.5);
                e!(author, id, "AUTHORED", []);
                let (s, _) = self.range(Kind::Document);
                if id > s {
                    for _ in 0..r.pareto(1.0, 1.5, 200) {
                        // Older documents, skewed towards the oldest.
                        let f = r.unit().powf(2.0) * ((id - s) as f64 / (self.range(Kind::Document).1 - s) as f64);
                        let to = self.member(Kind::Document, f);
                        if to < id {
                            e!(id, to, "CITES", []);
                        }
                    }
                }
                for _ in 0..r.below(7) {
                    let k = [Kind::Person, Kind::Company, Kind::Product][r.below(3) as usize];
                    let to = self.uniform(k, &mut r);
                    e!(id, to, "MENTIONS", ["offset" => Value::Int(r.below(4000) as i64)]);
                }
            }
            Kind::Category => {
                if let Some(p) = self.category_parent(id) {
                    e!(p, id, "PARENT_OF", []);
                }
            }
            Kind::Event => {
                let (_, e_) = self.range(Kind::Event);
                let mut next = id + 1;
                while next < e_ && !self.exists(next) {
                    next += 1;
                }
                if next < e_ {
                    e!(id, next, "NEXT", ["gap" => Value::Int((next - id) as i64)]);
                }
                if id % 50 == 0 {
                    e!(id, id, "RETRY", ["attempt" => Value::Int(1 + r.below(3) as i64)]);
                }
                if r.chance(0.3) {
                    let p = self.popular(Kind::Person, &mut r, 2.0);
                    e!(id, p, "TRIGGERED_BY", []);
                }
            }
        }
    }

    /// The string table a log replay would build: index strings first, then
    /// first appearances in node order, then in edge order.
    fn string_table(&self) -> Vec<String> {
        let mut seen: HashMap<&'static str, u32> = HashMap::new();
        let mut list: Vec<String> = Vec::new();
        let mut intern = |s: &'static str, seen: &mut HashMap<&'static str, u32>| {
            if !seen.contains_key(s) {
                seen.insert(s, list.len() as u32);
                list.push(s.to_string());
            }
        };
        for (l, k) in INDEXES {
            intern(l, &mut seen);
            intern(k, &mut seen);
        }
        let mut labels = Vec::new();
        let mut props = Vec::new();
        let node_done = |seen: &HashMap<&str, u32>| NODE_STRINGS.iter().all(|s| seen.contains_key(s));
        for id in 1..=self.n {
            if node_done(&seen) {
                break;
            }
            if !self.exists(id) {
                continue;
            }
            self.node(id, false, &mut labels, &mut props);
            for l in &labels {
                intern(l, &mut seen);
            }
            for (k, _) in &props {
                intern(k, &mut seen);
            }
        }
        let edge_done = |seen: &HashMap<&str, u32>| EDGE_STRINGS.iter().all(|s| seen.contains_key(s));
        let mut edges = Vec::new();
        for id in 1..=self.n {
            if edge_done(&seen) {
                break;
            }
            if !self.exists(id) {
                continue;
            }
            self.edges(id, false, &mut edges);
            for (_, _, t, props) in &edges {
                intern(t, &mut seen);
                for (k, _) in props {
                    intern(k, &mut seen);
                }
            }
        }
        list
    }
}

// --------------------------------------------------------- image source

struct Source<'a> {
    g: &'a Gen,
    strings: Vec<String>,
    ids: HashMap<String, u32>,
    edge_count: std::cell::Cell<u64>,
}

impl Source<'_> {
    fn id(&self, s: &str) -> u32 {
        self.ids[s]
    }
}

impl ImageSource for Source<'_> {
    fn strings(&self) -> Vec<String> {
        self.strings.clone()
    }

    fn next_ids(&self) -> (u64, u64) {
        (self.g.n + 1, self.edge_count.get() + 1)
    }

    fn indexes(&self) -> Vec<(u32, u32)> {
        INDEXES.iter().map(|(l, k)| (self.id(l), self.id(k))).collect()
    }

    fn nodes(&self, f: &mut dyn FnMut(u64, &[u32], &[(u32, Value)])) -> io::Result<()> {
        let (mut labels, mut props) = (Vec::new(), Vec::new());
        let (mut lids, mut pids): (Vec<u32>, Vec<(u32, Value)>) = (Vec::new(), Vec::new());
        for id in 1..=self.g.n {
            if !self.g.exists(id) {
                continue;
            }
            self.g.node(id, true, &mut labels, &mut props);
            lids.clear();
            lids.extend(labels.iter().map(|l| self.id(l)));
            pids.clear();
            pids.extend(props.drain(..).map(|(k, v)| (self.id(k), v)));
            f(id, &lids, &pids);
        }
        Ok(())
    }

    fn edges(&self, f: &mut dyn FnMut(u64, u64, u64, u32, &[(u32, Value)])) -> io::Result<()> {
        let mut eid = 0u64;
        let mut buf = Vec::new();
        let mut pids: Vec<(u32, Value)> = Vec::new();
        for id in 1..=self.g.n {
            if !self.g.exists(id) {
                continue;
            }
            self.g.edges(id, true, &mut buf);
            for (from, to, t, props) in buf.drain(..) {
                eid += 1;
                pids.clear();
                pids.extend(props.into_iter().map(|(k, v)| (self.id(k), v)));
                f(eid, from, to, self.id(t), &pids);
            }
        }
        self.edge_count.set(eid);
        Ok(())
    }
}

fn owned(props: Props) -> Vec<(String, Value)> {
    props.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// The same graph loaded into the paged engine through its own write path,
/// in large transactions.
fn write_paged(g: &Gen, out: &PathBuf, cache_mb: u64) -> io::Result<(u64, u64, u64)> {
    let err = |e: glider::types::Error| io::Error::new(io::ErrorKind::Other, e.to_string());
    let mut db = glider::graph::Graph::open_opts(
        out,
        glider::graph::OpenOptions {
            sync: glider::store::Sync::Off,
            cache_size: cache_mb << 20,
            checkpoint_bytes: 1 << 30,
            ..glider::graph::OpenOptions::default()
        },
    )
    .map_err(err)?;
    db.autocommit = false;
    for (l, k) in INDEXES {
        db.create_index(l, k).map_err(err)?;
    }
    let (mut labels, mut props) = (Vec::new(), Vec::new());
    let (mut nodes, mut edges) = (0u64, 0u64);
    let t = Instant::now();
    for id in 1..=g.n {
        if !g.exists(id) {
            continue;
        }
        g.node(id, true, &mut labels, &mut props);
        db.apply_op(Op::NodeAdd {
            id,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            props: owned(std::mem::take(&mut props)),
        })
        .map_err(err)?;
        nodes += 1;
        if nodes % 100_000 == 0 {
            db.commit().map_err(err)?;
        }
        if nodes % 1_000_000 == 0 {
            eprintln!("  {nodes} nodes, {:.0} s", t.elapsed().as_secs_f64());
        }
    }
    db.commit().map_err(err)?;
    let mut buf = Vec::new();
    for id in 1..=g.n {
        if !g.exists(id) {
            continue;
        }
        g.edges(id, true, &mut buf);
        for (from, to, et, props) in buf.drain(..) {
            edges += 1;
            db.apply_op(Op::EdgeAdd {
                id: edges,
                from,
                to,
                etype: et.to_string(),
                props: owned(props),
            })
            .map_err(err)?;
            if edges % 100_000 == 0 {
                db.commit().map_err(err)?;
            }
            if edges % 5_000_000 == 0 {
                eprintln!("  {edges} edges, {:.0} s", t.elapsed().as_secs_f64());
            }
        }
    }
    db.commit().map_err(err)?;
    db.checkpoint().map_err(err)?;
    let bytes = db.file_len();
    Ok((nodes, edges, bytes))
}

/// The same graph bulk-loaded into the paged engine: external sorts and
/// bottom-up tree builds, no per-edge B+tree inserts.
fn bulk_paged(g: &Gen, out: &PathBuf, cache_mb: u64, work_mb: u64) -> io::Result<(u64, u64, u64)> {
    let strings = g.string_table();
    let ids = strings.iter().enumerate().map(|(i, s)| (s.clone(), i as u32)).collect();
    let src = Source {
        g,
        strings,
        ids,
        edge_count: std::cell::Cell::new(0),
    };
    let db = glider::graph::Graph::bulk_load(
        out,
        &src,
        glider::graph::OpenOptions {
            sync: glider::store::Sync::Off,
            cache_size: cache_mb << 20,
            ..glider::graph::OpenOptions::default()
        },
        (work_mb << 20) as usize,
    )
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    Ok((db.node_count() as u64, db.edge_count() as u64, db.file_len()))
}

/// The same graph as a log: index definitions, every node, every edge.
fn write_log(g: &Gen, out: &PathBuf, v2: bool) -> io::Result<(u64, u64, u64)> {
    let mut w = if v2 {
        LogWriter::create_v2(out)?
    } else {
        LogWriter::create(out)?
    };
    for (l, k) in INDEXES {
        w.push(&Op::IndexAdd {
            label: l.to_string(),
            key: k.to_string(),
        })?;
    }
    w.commit()?;
    let (mut labels, mut props) = (Vec::new(), Vec::new());
    let (mut nodes, mut edges) = (0u64, 0u64);
    for id in 1..=g.n {
        if !g.exists(id) {
            continue;
        }
        g.node(id, true, &mut labels, &mut props);
        w.push(&Op::NodeAdd {
            id,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            props: owned(std::mem::take(&mut props)),
        })?;
        nodes += 1;
        if nodes % 5000 == 0 {
            w.commit()?;
        }
    }
    w.commit()?;
    let mut buf = Vec::new();
    for id in 1..=g.n {
        if !g.exists(id) {
            continue;
        }
        g.edges(id, true, &mut buf);
        for (from, to, t, props) in buf.drain(..) {
            edges += 1;
            w.push(&Op::EdgeAdd {
                id: edges,
                from,
                to,
                etype: t.to_string(),
                props: owned(props),
            })?;
            if edges % 5000 == 0 {
                w.commit()?;
            }
        }
    }
    let bytes = w.finish()?;
    Ok((nodes, edges, bytes))
}

fn build_image(g: &Gen, out: &PathBuf) -> io::Result<u64> {
    let strings = g.string_table();
    let ids = strings
        .iter()
        .enumerate()
        .map(|(i, s)| (s.clone(), i as u32))
        .collect();
    let src = Source {
        g,
        strings,
        ids,
        edge_count: std::cell::Cell::new(0),
    };
    // Edges are visited once, and next_ids is read after, so the edge
    // count is known by the time the header is written.
    image::build(out, &src)
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = [
        ("TiB", 1u64 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
    ]
    .iter()
    .find_map(|(suf, m)| s.strip_suffix(suf).map(|n| (n, *m)))
    .unwrap_or((s, 1));
    num.trim().parse::<f64>().ok().map(|n| (n * mult as f64) as u64)
}

const USAGE: &str = "usage: scale-gen (--size <1GiB|...> | --nodes N) --out <file> [--log [--v2] | --paged | --paged-insert] [--cache-mb N] [--work-mb N] [--seed N]";

fn main() {
    if let Err(e) = run() {
        eprintln!("scale-gen: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut size = None;
    let mut nodes = None;
    let mut out = None;
    let mut log = false;
    let mut v2 = false;
    let mut paged = false;
    let mut paged_insert = false;
    let mut work_mb = 512u64;
    let mut cache_mb = 1024u64;
    let mut seed = 42u64;
    let mut i = 0;
    while i < args.len() {
        let next = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i).cloned().ok_or_else(|| USAGE.to_string())
        };
        match args[i].as_str() {
            "--size" => size = Some(parse_size(&next(&mut i)?).ok_or("bad --size")?),
            "--nodes" => nodes = Some(next(&mut i)?.parse::<u64>().map_err(|_| "bad --nodes")?),
            "--out" => out = Some(PathBuf::from(next(&mut i)?)),
            "--seed" => seed = next(&mut i)?.parse().map_err(|_| "bad --seed")?,
            "--log" => log = true,
            "--v2" => v2 = true,
            "--paged" => paged = true,
            "--paged-insert" => paged_insert = true,
            "--work-mb" => work_mb = next(&mut i)?.parse().map_err(|_| "bad --work-mb")?,
            "--cache-mb" => cache_mb = next(&mut i)?.parse().map_err(|_| "bad --cache-mb")?,
            _ => return Err(USAGE.into()),
        }
        i += 1;
    }
    let out = out.ok_or(USAGE)?;
    if out.exists() {
        return Err(format!("{} exists; refusing to overwrite", out.display()));
    }

    let n = match (nodes, size) {
        (Some(n), _) => n,
        (None, Some(target)) => {
            // Calibrate: build a small image and scale by bytes per id.
            let probe_n = 200_000;
            let probe = out.with_extension("probe.tmp");
            let _ = std::fs::remove_file(&probe);
            let bytes = build_image(&Gen::new(probe_n, seed), &probe).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(&probe);
            let per_id = bytes as f64 / probe_n as f64;
            let n = (target as f64 / per_id) as u64;
            eprintln!("calibrated: {per_id:.0} bytes per id -> {n} ids for {target} bytes");
            n
        }
        (None, None) => return Err(USAGE.into()),
    };

    let g = Gen::new(n, seed);
    let t = Instant::now();
    if paged || paged_insert {
        let (nodes, edges, bytes) = if paged {
            bulk_paged(&g, &out, cache_mb, work_mb)
        } else {
            write_paged(&g, &out, cache_mb)
        }
        .map_err(|e| e.to_string())?;
        println!(
            "{{\"mode\":\"{}\",\"ids\":{n},\"nodes\":{nodes},\"edges\":{edges},\"bytes\":{bytes},\"seconds\":{:.3}}}",
            if paged { "paged-bulk" } else { "paged-insert" },
            t.elapsed().as_secs_f64()
        );
    } else if log {
        let (nodes, edges, bytes) = write_log(&g, &out, v2).map_err(|e| e.to_string())?;
        println!(
            "{{\"mode\":\"log\",\"ids\":{n},\"nodes\":{nodes},\"edges\":{edges},\"bytes\":{bytes},\"seconds\":{:.3}}}",
            t.elapsed().as_secs_f64()
        );
    } else {
        let bytes = build_image(&g, &out).map_err(|e| e.to_string())?;
        println!(
            "{{\"mode\":\"image\",\"ids\":{n},\"bytes\":{bytes},\"seconds\":{:.3}}}",
            t.elapsed().as_secs_f64()
        );
    }
    Ok(())
}
