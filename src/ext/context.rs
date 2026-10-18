//! `context`: graph retrieval for language models.
//!
//! A model asked about a knowledge graph needs a small, relevant, faithful
//! slice of it, in as few tokens as possible. Dumping query results as
//! JSON spends most of the budget on punctuation and repeated keys, and a
//! plain k-hop expansion fills it with whatever happens to be adjacent to
//! a hub. This extension does the work on the database side:
//!
//! - **`search`** finds entry points: BM25 over node text, streamed, top-k
//!   in bounded memory, with an exact-match shortcut through an index.
//! - **`retrieve`** ranks the neighbourhood of the seeds by personalised
//!   PageRank, computed by local push so it costs what it touches, not the
//!   graph's size; damps hubs instead of expanding through them; then grows
//!   a *connected* subgraph from the seeds in score order until a token
//!   budget is spent, keeping every edge among the chosen nodes. What was
//!   left out is reported (`+30 KNOWS`), so the model knows the context is
//!   partial and can ask for more instead of assuming absence.
//! - **`paths`** explains how two nodes connect: every shortest path up to
//!   `k`, preferring the specific route through low-degree nodes over the
//!   one through a hub.
//! - **`schema`** writes a schema card (labels, property types, enum-like
//!   values, relationship patterns) for the model to write queries against.
//! - **`card(n)`** and **`tokens(x)`** are functions, for use in queries.
//!
//! Output is a compact text form a model reads as easily as JSON at a
//! fraction of the tokens:
//!
//! ```text
//! # context: 3 nodes, 2 edges, ~41 tokens; seeds n1
//! n1:Person name="Ada Lovelace" born=1815
//! n2:Person name="Charles Babbage"
//! n7:Machine name="Analytical Engine"
//! n1 -KNEW-> n2
//! n1 -WROTE_ABOUT{year=1843}-> n7
//! # more: n2 +12 KNEW
//! ```
//!
//! `nN` is the node id: stable, and accepted back as `seeds: ["n2"]` or
//! `seen: [...]`. Nodes listed in `seen` (already in the model's context
//! from an earlier turn) are linked to but not described again.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, VecDeque};

use super::text;
use super::{Arg, Args, Extension, NodeImage, Spec};
use crate::graph::{Dir, Error, Graph, Result};
use crate::query::QueryResult;
use crate::types::{id_map, id_set, IdMap, IdSet};
use crate::value::{write_json_string, Value};

pub struct Context;

const PROCEDURES: &[Spec] = &[
    Spec {
        name: "retrieve",
        args: "(query, seeds, budget, depth, types, dir, labels, props, exclude, seen, format, alpha, normalize, max_chars, max_fanout)",
        doc: "A connected, relevance-ranked subgraph around the seeds (or the best search hits for query) that fits budget tokens",
    },
    Spec {
        name: "search",
        args: "(query, label, keys, top, scan)",
        doc: "Nodes whose text best matches query (BM25), for use as seeds; uses the text index when there is one",
    },
    Spec {
        name: "index",
        args: "(labels, keys)",
        doc: "Build the full-text index over these labels' text keys (all when omitted); writes keep it current",
    },
    Spec {
        name: "unindex",
        args: "()",
        doc: "Drop the full-text index",
    },
    Spec {
        name: "paths",
        args: "(from, to, k, depth, types, dir, budget, format)",
        doc: "Up to k shortest paths between two nodes, specific routes first, with the nodes on them",
    },
    Spec {
        name: "schema",
        args: "(sample)",
        doc: "A compact schema card: labels, property types, relationship patterns, indexes",
    },
];

const FUNCTIONS: &[Spec] = &[
    Spec {
        name: "card",
        args: "(node [, max_chars])",
        doc: "One node as a compact line: n12:Person name=\"Ada\" born=1815",
    },
    Spec {
        name: "tokens",
        args: "(value)",
        doc: "Estimated LLM tokens of a value's text",
    },
];

impl Extension for Context {
    fn name(&self) -> &str {
        "context"
    }
    fn version(&self) -> &str {
        "1.0"
    }
    fn summary(&self) -> &str {
        "token-budgeted graph retrieval for LLMs: search, ranked subgraphs, paths, schema cards"
    }
    fn procedures(&self) -> &[Spec] {
        PROCEDURES
    }
    fn functions(&self) -> &[Spec] {
        FUNCTIONS
    }

    fn call(&self, g: &mut Graph, procedure: &str, a: &Args) -> Result<QueryResult> {
        match procedure {
            "retrieve" => retrieve(g, a),
            "search" => {
                let q = a.str("query").ok_or_else(|| Error::Msg("search needs query:".into()))?;
                let hits = search(g, &q, &SearchOpts::from(g, a)?)?;
                let rows = hits
                    .hits
                    .iter()
                    .map(|&(id, s)| vec![Value::Int(id as i64), Value::Text(handle(id)), name_of(g, id), Value::Float(round(s))])
                    .collect();
                let mut r = QueryResult::table(vec!["id".into(), "ref".into(), "node".into(), "score".into()], rows);
                r.message = Some(if hits.indexed {
                    format!("{} matches, {} postings read from the text index", hits.matched, hits.scanned)
                } else {
                    format!("{} matches in {} nodes scanned (CALL context.index() makes this an index lookup)", hits.matched, hits.scanned)
                });
                Ok(r)
            }
            "index" => {
                let c = text::Config { labels: a.strs("labels").into_iter().chain(a.strs("label")).collect(), keys: a.strs("keys") };
                // No clock under wasm: the summary then leaves the time out.
                let t = crate::telemetry::Stopwatch::start();
                let flush = (g.algorithm_memory() / 128).max(1 << 14) as usize;
                let b = text::build(g, &c, flush)?;
                let took = t.elapsed_ns().map(|ns| format!(" in {:.1}s", ns as f64 / 1e9)).unwrap_or_default();
                let mut r = QueryResult::message(format!(
                    "indexed {} nodes: {} terms, {} postings{took}",
                    b.docs, b.terms, b.postings
                ));
                r.touched = b.docs as usize;
                Ok(r)
            }
            "unindex" => {
                text::drop_index(g)?;
                Ok(QueryResult::message("dropped the text index"))
            }
            "paths" => paths(g, a),
            "schema" => Ok(text_result(schema(g, a.usize("sample", 200)))),
            _ => unreachable!("dispatch checks the procedure list"),
        }
    }

    fn wants_writes(&self, g: &Graph) -> bool {
        text::config(g).is_some()
    }

    fn on_node_write(&self, g: &mut Graph, id: u64, before: Option<&NodeImage>, after: Option<&NodeImage>) -> Result<()> {
        let Some(c) = text::config(g) else { return Ok(()) };
        fn view(n: Option<&NodeImage>) -> Option<text::NodeView<'_>> {
            n.map(|n| (&n.labels[..], &n.props[..]))
        }
        text::on_node_write(g, &c, id, view(before), view(after))
    }

    fn eval(&self, g: &Graph, function: &str, args: &[Arg]) -> Value {
        match function {
            "card" => {
                let max = match args.get(1) {
                    Some(Arg::Value(v)) => v.as_i64().map(|v| v.max(8) as usize),
                    _ => None,
                };
                let r = Render { max_chars: max.unwrap_or(DEFAULT_MAX_CHARS), ..Render::default() };
                match args.first() {
                    Some(Arg::Node(id)) => Value::Text(r.card(g, *id)),
                    Some(Arg::Edge(id)) => Value::Text(r.edge_line(g, *id)),
                    Some(Arg::Value(v)) => match v.as_i64() {
                        Some(id) if g.node(id as u64).is_some() => Value::Text(r.card(g, id as u64)),
                        _ => Value::Null,
                    },
                    None => Value::Null,
                }
            }
            "tokens" => match args.first() {
                Some(Arg::Value(Value::Null)) | None => Value::Null,
                Some(Arg::Value(Value::Text(s))) => Value::Int(tokens(s) as i64),
                Some(Arg::Value(v)) => Value::Int(tokens(&v.to_string()) as i64),
                Some(Arg::Node(id)) => Value::Int(tokens(&Render::default().card(g, *id)) as i64),
                Some(Arg::Edge(id)) => Value::Int(tokens(&Render::default().edge_line(g, *id)) as i64),
            },
            _ => Value::Null,
        }
    }
}

// ------------------------------------------------------------------ tokens

/// Estimated tokens for `s` under a BPE tokenizer: a word costs about one
/// token per four characters, every other non-space character about one.
/// Within ~15% of cl100k/Claude on English and on this module's output; it
/// is for budgeting, so it errs high on symbols.
pub fn tokens(s: &str) -> usize {
    let mut n = 0usize;
    let mut run = 0usize;
    for c in s.chars() {
        if c.is_alphanumeric() {
            run += 1;
            continue;
        }
        n += run.div_ceil(4);
        run = 0;
        if !c.is_whitespace() {
            n += 1;
        }
    }
    n + run.div_ceil(4)
}

pub fn handle(id: u64) -> String {
    format!("n{id}")
}

fn round(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

// ------------------------------------------------------------------ render

const DEFAULT_MAX_CHARS: usize = 160;
/// A list of at least this many numbers is a vector: shown as `<vector N>`,
/// never spelled out. An embedding is thousands of tokens of no use to a
/// model reading text.
const VECTOR_MIN: usize = 8;
const MAX_LIST: usize = 10;
const MAX_PROPS: usize = 16;

/// How nodes and edges are written out.
#[derive(Clone, Debug)]
struct Render {
    /// Budget for, and write, JSON instead of the text form.
    json: bool,
    max_chars: usize,
    /// Only these property keys, when not empty.
    props: Vec<String>,
    /// Never these. Keys starting with `_` are always left out.
    exclude: Vec<String>,
}

impl Default for Render {
    fn default() -> Render {
        Render { json: false, max_chars: DEFAULT_MAX_CHARS, props: Vec::new(), exclude: Vec::new() }
    }
}

/// Keys that name a thing come first, so a truncated card still says what
/// it is.
pub(super) fn key_rank(k: &str) -> u8 {
    match k {
        "name" | "title" => 0,
        "label" | "key" | "summary" => 1,
        _ => 2,
    }
}

impl Render {
    fn from(a: &Args) -> Render {
        Render {
            json: a.str("format").is_some_and(|f| f.eq_ignore_ascii_case("json")),
            max_chars: a.usize("max_chars", DEFAULT_MAX_CHARS).max(8),
            props: a.strs("props"),
            exclude: a.strs("exclude"),
        }
    }

    fn keep(&self, k: &str) -> bool {
        !k.starts_with('_')
            && !self.exclude.iter().any(|e| e == k)
            && (self.props.is_empty() || self.props.iter().any(|p| p == k))
    }

    fn value(&self, v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::Float(f) => out.push_str(&float(*f)),
            Value::Text(s) => {
                if s.chars().count() > self.max_chars {
                    let cut = truncate(s, self.max_chars);
                    let mut t = String::with_capacity(cut.len() + 4);
                    t.push_str(cut);
                    t.push('…');
                    write_json_string(&t, out);
                } else {
                    write_json_string(s, out);
                }
            }
            Value::List(items) => {
                if items.len() >= VECTOR_MIN && items.iter().all(|x| matches!(x, Value::Int(_) | Value::Float(_))) {
                    out.push_str(&format!("<vector {}>", items.len()));
                    return;
                }
                out.push('[');
                for (i, x) in items.iter().take(MAX_LIST).enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.value(x, out);
                }
                if items.len() > MAX_LIST {
                    out.push_str(&format!(", …+{}", items.len() - MAX_LIST));
                }
                out.push(']');
            }
        }
    }

    fn props(&self, props: &[(String, Value)], out: &mut String, sep: &str) {
        let mut ps: Vec<&(String, Value)> = props.iter().filter(|(k, v)| self.keep(k) && *v != Value::Null).collect();
        ps.sort_by_key(|p| key_rank(&p.0));
        for (i, (k, v)) in ps.iter().take(MAX_PROPS).enumerate() {
            if i > 0 {
                out.push_str(sep);
            }
            out.push_str(k);
            out.push('=');
            self.value(v, out);
        }
        if ps.len() > MAX_PROPS {
            out.push_str(&format!("{sep}…+{} props", ps.len() - MAX_PROPS));
        }
    }

    /// `n12:Person:Author name="Ada" born=1815`
    fn card(&self, g: &Graph, id: u64) -> String {
        let mut s = handle(id);
        for l in g.node_labels(id) {
            s.push(':');
            s.push_str(&l);
        }
        let props = g.node_props(id);
        if props.iter().any(|(k, v)| self.keep(k) && *v != Value::Null) {
            s.push(' ');
            self.props(&props, &mut s, " ");
        }
        s
    }

    /// `{since=2019}` or nothing.
    fn edge_props(&self, g: &Graph, edge: u64) -> String {
        let props = g.edge_props(edge);
        if !props.iter().any(|(k, v)| self.keep(k) && *v != Value::Null) {
            return String::new();
        }
        let mut s = String::from("{");
        self.props(&props, &mut s, ", ");
        s.push('}');
        s
    }

    /// `n1 -KNOWS{since=2019}-> n2`, by stored direction.
    fn edge_line(&self, g: &Graph, edge: u64) -> String {
        match g.edge(edge) {
            Some(e) => format!(
                "{} -{}{}-> {}",
                handle(e.from),
                g.strings.name(e.etype),
                self.edge_props(g, edge),
                handle(e.to)
            ),
            None => String::new(),
        }
    }
}

fn float(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e15 {
        return format!("{f:.1}");
    }
    let a = f.abs();
    if (1e-4..1e9).contains(&a) {
        let s = format!("{f:.6}");
        let s = s.trim_end_matches('0');
        s.trim_end_matches('.').to_string()
    } else {
        format!("{f:.4e}")
    }
}

/// At most `max` characters of `s`, cut at a word boundary when one is near.
fn truncate(s: &str, max: usize) -> &str {
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    let head = &s[..end];
    match head.rfind(char::is_whitespace) {
        Some(i) if i * 3 > end * 2 => head[..i].trim_end(),
        _ => head,
    }
}

/// A node's short name, for result rows.
fn name_of(g: &Graph, id: u64) -> Value {
    for key in ["name", "title", "label", "key"] {
        if let Some(v) = g.node_prop(id, key) {
            return v;
        }
    }
    match g.node_labels(id).first() {
        Some(l) => Value::Text(format!(":{l}")),
        None => Value::Text(handle(id)),
    }
}

fn text_result(text: String) -> QueryResult {
    let t = tokens(&text) as i64;
    QueryResult::table(vec!["context".into(), "tokens".into()], vec![vec![Value::Text(text), Value::Int(t)]])
}

// ------------------------------------------------------------------ search

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "did", "do", "does", "for", "from", "has", "have", "how", "in",
    "is", "it", "its", "of", "on", "or", "that", "the", "their", "this", "to", "was", "were", "what", "when", "where",
    "which", "who", "whom", "why", "with",
];

/// Lowercase word stems: alphanumeric runs, a plural `s` dropped.
pub(super) fn terms(s: &str, mut f: impl FnMut(&str)) {
    let mut w = String::new();
    let mut flush = |w: &mut String| {
        if !w.is_empty() {
            let t = if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") { &w[..w.len() - 1] } else { &w[..] };
            f(t);
            w.clear();
        }
    };
    for c in s.chars() {
        if c.is_alphanumeric() {
            w.extend(c.to_lowercase());
        } else {
            flush(&mut w);
        }
    }
    flush(&mut w);
}

pub(super) fn query_terms(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    terms(q, |t| {
        if !STOPWORDS.contains(&t) && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    });
    if out.is_empty() {
        // A query of nothing but stopwords still means something.
        terms(q, |t| {
            if !out.iter().any(|x| x == t) {
                out.push(t.to_string());
            }
        });
    }
    out
}

struct SearchOpts {
    label: Option<u32>,
    label_name: Option<String>,
    keys: Vec<String>,
    top: usize,
    scan: usize,
}

impl SearchOpts {
    fn from(g: &Graph, a: &Args) -> Result<SearchOpts> {
        let label_name = a.str("label");
        let label = label_name.as_ref().map(|l| g.strings.lookup(l).unwrap_or(u32::MAX));
        Ok(SearchOpts { label, label_name, keys: a.strs("keys"), top: a.usize("top", 10).max(1), scan: a.usize("scan", usize::MAX) })
    }
}

struct Hits {
    hits: Vec<(u64, f64)>,
    /// Nodes scanned, or postings read from the index.
    scanned: usize,
    matched: usize,
    indexed: bool,
}

/// A candidate's term counts, kept until document frequencies are known.
struct Cand {
    id: u64,
    tf: Vec<f32>,
    len: u32,
    phrase: bool,
    covered: u32,
}

/// BM25 over the text properties of every node (or one label's), in one
/// streaming pass. Document frequencies are only known at the end, so the
/// pass keeps the candidates covering the most query terms — a few times
/// `top` of them — and ranks those exactly once the pass is done. A text
/// equal to the whole query (an entity's name, typically) ranks first;
/// with an index on `label(key)` those are found without scanning.
fn search(g: &Graph, q: &str, o: &SearchOpts) -> Result<Hits> {
    let qt = query_terms(q);
    if qt.is_empty() {
        return Err(Error::Msg("query: has no words to search for".into()));
    }
    let phrase = q.trim().to_lowercase();
    let mut exact: Vec<u64> = Vec::new();
    if let Some(l) = &o.label_name {
        for k in &o.keys {
            if g.has_index(l, k) {
                exact.extend(g.indexed_lookup(l, k, &Value::Text(q.trim().to_string())).unwrap_or_default());
            }
        }
    }
    let merge_exact = |scored: &mut Vec<(u64, f64)>, exact: &[u64]| {
        for &id in exact {
            match scored.iter_mut().find(|(x, _)| *x == id) {
                Some(e) => e.1 += 10.0,
                None => scored.push((id, 10.0)),
            }
        }
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(o.top);
    };
    // The index answers unless the caller asked for a bounded scan or for
    // keys the index may not cover.
    if o.scan == usize::MAX && o.keys.is_empty() && text::config(g).is_some() {
        let (mut scored, read, matched) = text::search(g, q, o.top, o.label);
        merge_exact(&mut scored, &exact);
        return Ok(Hits { hits: scored, scanned: read, matched, indexed: true });
    }
    let keep = (o.top * 8).max(256);
    let mut cands: Vec<Cand> = Vec::new();
    let mut df = vec![0u64; qt.len()];
    let (mut scanned, mut matched, mut total_len) = (0usize, 0usize, 0u64);
    let better = |a: &Cand, b: &Cand| {
        (b.phrase, b.covered)
            .cmp(&(a.phrase, a.covered))
            .then_with(|| b.tf.iter().sum::<f32>().total_cmp(&a.tf.iter().sum::<f32>()))
            .then_with(|| a.id.cmp(&b.id))
    };
    let mut from = 0u64;
    loop {
        let mut last = None;
        let r = g.scan_nodes(o.label, from, 4096, &mut |id| {
            last = Some(id);
            if scanned >= o.scan {
                return false;
            }
            scanned += 1;
            let Some(n) = g.node_shared(id) else { return true };
            let mut tf = vec![0f32; qt.len()];
            let mut len = 0u32;
            let mut phrase_hit = false;
            for (k, v) in n.props().iter() {
                let key = g.strings.name(*k);
                if key.starts_with('_') || (!o.keys.is_empty() && !o.keys.iter().any(|x| x == key)) {
                    continue;
                }
                // Names count double: a match there is about the node itself.
                let boost = if key_rank(key) == 0 { 2.0 } else { 1.0 };
                let mut texts = |s: &str| {
                    if boost > 1.0 && s.trim().to_lowercase() == phrase {
                        phrase_hit = true;
                    }
                    terms(s, |t| {
                        len += 1;
                        if let Some(i) = qt.iter().position(|x| x == t) {
                            tf[i] += boost;
                        }
                    });
                };
                match v {
                    Value::Text(s) => texts(s),
                    Value::List(items) => {
                        for x in items {
                            if let Value::Text(s) = x {
                                texts(s);
                            }
                        }
                    }
                    _ => {}
                }
            }
            total_len += len as u64;
            let covered = tf.iter().filter(|&&x| x > 0.0).count() as u32;
            if covered == 0 {
                return true;
            }
            matched += 1;
            for (i, &x) in tf.iter().enumerate() {
                if x > 0.0 {
                    df[i] += 1;
                }
            }
            cands.push(Cand { id, tf, len, phrase: phrase_hit, covered });
            if cands.len() >= keep * 2 {
                cands.sort_by(better);
                cands.truncate(keep);
            }
            true
        });
        match (r, last) {
            (Some((n, _)), Some(l)) if n == 4096 && scanned < o.scan => from = l + 1,
            _ => break,
        }
    }
    let n = scanned.max(1) as f64;
    let avg = (total_len as f64 / n).max(1.0);
    let (k1, b) = (1.2, 0.75);
    let mut scored: Vec<(u64, f64)> = cands
        .iter()
        .map(|c| {
            let mut s = 0.0;
            for (i, &f) in c.tf.iter().enumerate() {
                if f > 0.0 {
                    let idf = (1.0 + (n - df[i] as f64 + 0.5) / (df[i] as f64 + 0.5)).ln();
                    let f = f as f64;
                    s += idf * f * (k1 + 1.0) / (f + k1 * (1.0 - b + b * c.len as f64 / avg));
                }
            }
            if c.phrase {
                s += 10.0;
            }
            (c.id, s)
        })
        .collect();
    merge_exact(&mut scored, &exact);
    Ok(Hits { hits: scored, scanned, matched, indexed: false })
}

// ------------------------------------------------------------------ retrieve

/// Which edges a walk may follow.
struct Walk {
    dir: Dir,
    /// One type is a range scan; several are a filter.
    etype: Option<u32>,
    types: Option<Vec<u32>>,
    labels: Option<Vec<u32>>,
}

impl Walk {
    fn from(g: &Graph, a: &Args) -> Result<Walk> {
        let dir = match a.str("dir") {
            Some(s) => Dir::parse(&s).ok_or_else(|| Error::Msg(format!("bad dir '{s}': use out, in or both")))?,
            None => Dir::Both,
        };
        let ids = |names: Vec<String>| -> Option<Vec<u32>> {
            if names.is_empty() {
                return None;
            }
            Some(names.iter().map(|t| g.strings.lookup(t).unwrap_or(u32::MAX)).collect())
        };
        let types = ids(a.strs("types").into_iter().chain(a.strs("type")).collect());
        let etype = match &types {
            Some(t) if t.len() == 1 => Some(t[0]),
            _ => None,
        };
        Ok(Walk { dir, etype, types: if etype.is_some() { None } else { types }, labels: ids(a.strs("labels")) })
    }

    fn edge_ok(&self, t: u32) -> bool {
        self.types.as_ref().map_or(true, |ts| ts.contains(&t))
    }

    fn node_ok(&self, g: &Graph, id: u64) -> bool {
        match &self.labels {
            None => true,
            Some(ls) => ls.iter().any(|&l| g.has_label(id, l)),
        }
    }

    /// Neighbours along allowed edges, up to `cap` (+1, so the caller can
    /// tell it was cut). Each is (edge, other, type, outgoing).
    fn adj(&self, g: &Graph, id: u64, dir: Dir, cap: usize, out: &mut Vec<(u64, u64, u32, bool)>) {
        out.clear();
        let sides: &[(Dir, bool)] = match dir {
            Dir::Out => &[(Dir::Out, true)],
            Dir::In => &[(Dir::In, false)],
            Dir::Both => &[(Dir::Out, true), (Dir::In, false)],
        };
        for &(d, outgoing) in sides {
            let go = g.for_each_adj_while(id, d, self.etype, |a| {
                if self.edge_ok(a.etype) {
                    out.push((a.edge, a.other, a.etype, outgoing));
                }
                out.len() <= cap
            });
            if !go {
                return;
            }
        }
    }
}

/// Parameters of the local push.
#[derive(Clone, Copy)]
struct Push {
    alpha: f64,
    eps: f64,
    depth: u32,
    max_fanout: usize,
    cap: usize,
}

#[derive(Clone, Copy, Default)]
struct Mass {
    p: f64,
    r: f64,
    hops: u32,
    queued: bool,
    /// Neighbours along allowed edges, once scanned (capped at max_fanout + 1).
    deg: u32,
}

/// Approximate personalised PageRank from `seeds` by local push (Andersen,
/// Chung & Lang): residual mass moves from a node to its neighbours only
/// while it exceeds `eps`, so the work is about 1/(eps·alpha) pushes
/// whatever the size of the graph, and state is kept only for nodes that
/// receive mass. Nodes with more than `max_fanout` neighbours, and nodes
/// `depth` hops out, absorb what reaches them instead of passing it on:
/// a hub ranks as relevant without flooding the context with its
/// thousands of neighbours. Stops early, still a valid lower bound, past
/// `cap` remembered nodes.
fn ppr(g: &Graph, w: &Walk, seeds: &[(u64, f64)], o: &Push) -> (IdMap<Mass>, bool) {
    let Push { alpha, eps, depth, max_fanout, cap } = *o;
    let mut m: IdMap<Mass> = id_map();
    let mut q = VecDeque::new();
    let total: f64 = seeds.iter().map(|s| s.1).sum::<f64>().max(f64::MIN_POSITIVE);
    for &(s, wt) in seeds {
        let e = m.entry(s).or_default();
        e.r += wt / total;
        if !e.queued {
            e.queued = true;
            q.push_back(s);
        }
    }
    let mut adj = Vec::new();
    let mut capped = false;
    while let Some(u) = q.pop_front() {
        let Mass { r, hops, .. } = m[&u];
        let e = m.get_mut(&u).unwrap();
        e.queued = false;
        if r <= eps {
            continue;
        }
        if hops >= depth || capped {
            e.p += r;
            e.r = 0.0;
            continue;
        }
        w.adj(g, u, w.dir, max_fanout, &mut adj);
        let e = m.get_mut(&u).unwrap();
        e.deg = adj.len() as u32;
        if adj.is_empty() || adj.len() > max_fanout {
            e.p += r;
            e.r = 0.0;
            continue;
        }
        e.p += alpha * r;
        e.r = 0.0;
        let share = (1.0 - alpha) * r / adj.len() as f64;
        for &(_, v, _, _) in &adj {
            if !m.contains_key(&v) {
                if m.len() >= cap {
                    capped = true;
                    continue;
                }
                if !w.node_ok(g, v) {
                    continue;
                }
            }
            let e = m.entry(v).or_insert(Mass { hops: hops + 1, ..Mass::default() });
            e.hops = e.hops.min(hops + 1);
            e.r += share;
            if e.r > eps && !e.queued {
                e.queued = true;
                q.push_back(v);
            }
        }
    }
    (m, capped)
}

#[derive(PartialEq)]
struct Cand2(f64, u64, u64);
impl Eq for Cand2 {}
impl PartialOrd for Cand2 {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Cand2 {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0).then_with(|| o.1.cmp(&self.1)).then_with(|| o.2.cmp(&self.2))
    }
}

/// What a node contributes: its card (unless already seen) and the edge
/// lines, grouped as `n1 -KNOWS-> n2, n3` when they have no properties.
struct Picked {
    nodes: Vec<u64>,
    in_set: IdSet,
    edges: Vec<u64>,
    edge_set: IdSet,
    /// (from, type) groups that already have a line, for costing.
    groups: HashMap<(u64, u32), ()>,
    /// Per picked node and type: (edges seen in its scan, scan was cut).
    degree: IdMap<(Vec<(u32, usize)>, bool)>,
    used: usize,
}

impl Picked {
    fn edge_cost(&self, r: &Render, g: &Graph, edge: u64, from: u64, to: u64, t: u32) -> (usize, bool) {
        if r.json {
            return (tokens(&r.edge_json(g, edge)) + 1, false);
        }
        let props = r.edge_props(g, edge);
        if props.is_empty() && self.groups.contains_key(&(from, t)) {
            return (tokens(&format!(", {}", handle(to))), true);
        }
        (tokens(&format!("{} -{}{}-> {}", handle(from), g.strings.name(t), props, handle(to))) + 1, props.is_empty())
    }

    fn add_edge(&mut self, edge: u64, from: u64, t: u32, groupable: bool, cost: usize) {
        if self.edge_set.insert(edge) {
            self.edges.push(edge);
            self.used += cost;
            if groupable {
                self.groups.insert((from, t), ());
            }
        }
    }
}

fn retrieve(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let g: &Graph = g;
    let budget = a.usize("budget", 1000).max(32);
    let depth = a.usize("depth", 3) as u32;
    let alpha = a.f64("alpha", 0.2).clamp(0.01, 0.99);
    let max_fanout = a.usize("max_fanout", 500).max(1);
    // Residual mass below which a node is not pushed. The budget only has
    // room for about budget/10 nodes, so finer than that is wasted work.
    let eps = a.f64("epsilon", (0.2 / budget as f64).clamp(1e-5, 1e-3)).max(1e-9);
    let cap = (g.algorithm_memory() / 96).max(1 << 16) as usize;
    let format = a.str("format").unwrap_or_else(|| "text".into()).to_lowercase();
    if !matches!(format.as_str(), "text" | "json" | "rows") {
        return Err(Error::Msg(format!("bad format '{format}': use text, json or rows")));
    }
    let render = Render::from(a);
    let walk = Walk::from(g, a)?;
    let seen: IdSet = a.ids("seen")?.into_iter().collect();

    // Seeds: given, or the best search hits weighted by score.
    let mut seeds: Vec<(u64, f64)> = a.ids("seeds")?.into_iter().map(|id| (id, 1.0)).collect();
    let mut how = "seeds";
    if let Some(q) = a.str("query") {
        let mut o = SearchOpts::from(g, a)?;
        o.top = a.usize("top", 3).max(1);
        let hits = search(g, &q, &o)?;
        if seeds.is_empty() {
            how = "search";
        }
        seeds.extend(hits.hits);
    }
    seeds.retain(|(id, _)| g.node(*id).is_some());
    let mut uniq = id_set();
    seeds.retain(|(id, _)| uniq.insert(*id));
    if seeds.is_empty() {
        return Err(Error::Msg(if how == "search" || a.get("query").is_some() {
            "nothing matched query:; try other words, or pass seeds:".into()
        } else {
            "retrieve needs seeds: (node ids) or query: (text)".into()
        }));
    }

    let (mass, capped) = ppr(g, &walk, &seeds, &Push { alpha, eps, depth, max_fanout, cap });
    // Rank by mass over degree^beta: with beta > 0 a node that is
    // reachable from the seeds mostly because it is adjacent to everything
    // ranks below one the seeds reach through several routes.
    let beta = a.f64("normalize", 1.0).clamp(0.0, 1.0);
    let ranks: std::cell::RefCell<IdMap<f64>> = std::cell::RefCell::new(id_map());
    let score_of = |id: u64, deg_buf: &mut Vec<(u64, u64, u32, bool)>| -> f64 {
        let Some(m) = mass.get(&id) else { return 0.0 };
        let raw = m.p + m.r;
        if beta == 0.0 || raw == 0.0 {
            return raw;
        }
        if let Some(&s) = ranks.borrow().get(&id) {
            return s;
        }
        let d = if m.deg > 0 {
            m.deg as usize
        } else {
            walk.adj(g, id, walk.dir, max_fanout, deg_buf);
            deg_buf.len()
        };
        let s = raw / (d.max(1) as f64).powf(beta);
        ranks.borrow_mut().insert(id, s);
        s
    };
    let score = |id: u64| score_of(id, &mut Vec::new());

    // Grow a connected subgraph from the seeds, best-scoring frontier node
    // first, while the budget lasts.
    let header_reserve = 24 + seeds.len() * 3;
    // Room kept back for saying what was left out (text form only).
    let full = budget;
    let budget = if render.json { budget } else { budget - (budget / 12).max(8).min(budget / 2) };
    let mut pk = Picked {
        nodes: Vec::new(),
        in_set: id_set(),
        edges: Vec::new(),
        edge_set: id_set(),
        groups: HashMap::new(),
        degree: id_map(),
        used: header_reserve,
    };
    let mut frontier: BinaryHeap<Cand2> = BinaryHeap::new();
    let mut adj = Vec::new();
    let mut cards: IdMap<String> = id_map();
    let mut costs: IdMap<usize> = id_map();
    let mut card = |g: &Graph, id: u64| -> (String, usize) {
        let c = cards.entry(id).or_insert_with(|| render.card(g, id)).clone();
        let t = if seen.contains(&id) {
            0
        } else {
            *costs.entry(id).or_insert_with(|| if render.json { tokens(&render.node_json(g, id)) + 1 } else { tokens(&c) + 1 })
        };
        (c, t)
    };

    // Add `v` and scan it: edges to nodes already picked are added while
    // they fit; the rest of its neighbours join the frontier.
    let mut admit = |g: &Graph, pk: &mut Picked, frontier: &mut BinaryHeap<Cand2>, v: u64, card_cost: usize| {
        pk.nodes.push(v);
        pk.in_set.insert(v);
        pk.used += card_cost;
        walk.adj(g, v, Dir::Both, max_fanout, &mut adj);
        let cut = adj.len() > max_fanout;
        let mut by_type: Vec<(u32, usize)> = Vec::new();
        for &(edge, other, t, outgoing) in &adj {
            match by_type.iter_mut().find(|x| x.0 == t) {
                Some(x) => x.1 += 1,
                None => by_type.push((t, 1)),
            }
            if pk.in_set.contains(&other) {
                let (from, to) = if outgoing { (v, other) } else { (other, v) };
                let (c, groupable) = pk.edge_cost(&render, g, edge, from, to, t);
                if pk.used + c <= budget {
                    pk.add_edge(edge, from, t, groupable, c);
                }
                continue;
            }
            let follows = match walk.dir {
                Dir::Both => true,
                Dir::Out => outgoing,
                Dir::In => !outgoing,
            };
            let s = score(other);
            if follows && s > 0.0 {
                frontier.push(Cand2(s, other, edge));
            }
        }
        pk.degree.insert(v, (by_type, cut));
    };

    for (i, &(s, _)) in seeds.iter().enumerate() {
        let (_, c) = card(g, s);
        if i > 0 && pk.used + c > budget {
            break;
        }
        admit(g, &mut pk, &mut frontier, s, c);
    }
    let mut misses = 0;
    while let Some(Cand2(_, v, via)) = frontier.pop() {
        if pk.in_set.contains(&v) {
            continue;
        }
        let Some(e) = g.edge(via) else { continue };
        let (_, c) = card(g, v);
        let (ec, groupable) = pk.edge_cost(&render, g, via, e.from, e.to, e.etype);
        if pk.used + c + ec > budget {
            misses += 1;
            if misses > 64 || budget - pk.used.min(budget) < 4 {
                break;
            }
            continue;
        }
        misses = 0;
        pk.add_edge(via, e.from, e.etype, groupable, ec);
        admit(g, &mut pk, &mut frontier, v, c);
    }

    let reached = mass.len();
    let mut order: Vec<u64> = pk.nodes.clone();
    // Seeds first, as given; the rest by score.
    let nseeds = seeds.iter().filter(|(s, _)| pk.in_set.contains(s)).count();
    order[nseeds..].sort_by(|x, y| score(*y).total_cmp(&score(*x)).then_with(|| x.cmp(y)));

    // What was not shown: per node and type, edges beyond those included.
    let mut shown: HashMap<(u64, u32), usize> = HashMap::new();
    for &eid in &pk.edges {
        if let Some(e) = g.edge(eid) {
            *shown.entry((e.from, e.etype)).or_default() += 1;
            if e.to != e.from {
                *shown.entry((e.to, e.etype)).or_default() += 1;
            }
        }
    }
    // Per node: (type, edges not shown, the count is a lower bound).
    type Omitted = Vec<(u32, usize, bool)>;
    let mut more: Vec<(u64, Omitted)> = Vec::new();
    for &id in &order {
        let Some((types, cut)) = pk.degree.get(&id) else { continue };
        let mut m: Vec<(u32, usize, bool)> = types
            .iter()
            .filter_map(|&(t, n)| {
                let left = n.saturating_sub(shown.get(&(id, t)).copied().unwrap_or(0));
                (left > 0).then_some((t, left, *cut))
            })
            .collect();
        if !m.is_empty() {
            m.sort_by_key(|x| std::cmp::Reverse(x.1));
            more.push((id, m));
        }
    }

    let summary = format!(
        "{} of {} reached nodes, {} edges in ~{} tokens{}",
        pk.nodes.len(),
        reached,
        pk.edges.len(),
        pk.used,
        if capped { " (walk hit its memory cap)" } else { "" }
    );

    match format.as_str() {
        "rows" => {
            let rows = order
                .iter()
                .map(|&id| {
                    let (c, _) = card(g, id);
                    vec![
                        Value::Int(id as i64),
                        Value::Text(handle(id)),
                        Value::Float(round(score(id))),
                        Value::Int(mass.get(&id).map(|m| m.hops as i64).unwrap_or(0)),
                        Value::Text(c),
                    ]
                })
                .collect();
            let mut r = QueryResult::table(
                vec!["id".into(), "ref".into(), "score".into(), "hops".into(), "card".into()],
                rows,
            );
            r.message = Some(summary);
            Ok(r)
        }
        "json" => {
            let text = to_json(g, &render, &order, &pk.edges, &seen);
            let mut r = text_result(text);
            r.message = Some(summary);
            Ok(r)
        }
        _ => {
            let mut out = String::new();
            for &id in &order {
                if !seen.contains(&id) {
                    let (c, _) = card(g, id);
                    out.push_str(&c);
                    out.push('\n');
                }
            }
            write_edges(g, &render, &pk.edges, &mut out);
            // The omissions go last and only as far as the budget allows:
            // they are what tells the model the picture is partial.
            let mut body_tokens = header_reserve + tokens(&out);
            if !more.is_empty() {
                let mut line = String::from("# more:");
                let mut first = true;
                for (id, m) in &more {
                    let mut part = format!(" {}", handle(*id));
                    for (t, n, cut) in m.iter().take(3) {
                        part.push_str(&format!(" +{}{} {}", n, if *cut { "+" } else { "" }, g.strings.name(*t)));
                    }
                    if !first {
                        part.insert(0, ';');
                    }
                    let c = tokens(&part);
                    if body_tokens + c + 2 > full {
                        break;
                    }
                    body_tokens += c;
                    line.push_str(&part);
                    first = false;
                }
                if !first {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            let shown_seeds: Vec<String> = seeds.iter().filter(|(s, _)| pk.in_set.contains(s)).map(|(s, _)| handle(*s)).collect();
            let mut text = format!(
                "# context: {} nodes, {} edges; seeds {}\n",
                pk.nodes.len(),
                pk.edges.len(),
                shown_seeds.join(" ")
            );
            text.push_str(&out);
            let t = tokens(&text);
            let mut r = QueryResult::table(
                vec!["context".into(), "nodes".into(), "edges".into(), "tokens".into()],
                vec![vec![
                    Value::Text(text),
                    Value::Int(pk.nodes.len() as i64),
                    Value::Int(pk.edges.len() as i64),
                    Value::Int(t as i64),
                ]],
            );
            r.message = Some(summary);
            Ok(r)
        }
    }
}

/// Edge lines: by source node in first-seen order, property-less edges of
/// one type from one node folded onto one line.
fn write_edges(g: &Graph, r: &Render, edges: &[u64], out: &mut String) {
    let mut groups: Vec<((u64, u32), Vec<u64>)> = Vec::new();
    let mut at: HashMap<(u64, u32), usize> = HashMap::new();
    for &eid in edges {
        let Some(e) = g.edge(eid) else { continue };
        let props = r.edge_props(g, eid);
        if !props.is_empty() {
            out.push_str(&format!("{} -{}{}-> {}\n", handle(e.from), g.strings.name(e.etype), props, handle(e.to)));
            continue;
        }
        match at.get(&(e.from, e.etype)) {
            Some(&i) => groups[i].1.push(e.to),
            None => {
                at.insert((e.from, e.etype), groups.len());
                groups.push(((e.from, e.etype), vec![e.to]));
            }
        }
    }
    for ((from, t), tos) in groups {
        let tos: Vec<String> = tos.into_iter().map(handle).collect();
        out.push_str(&format!("{} -{}-> {}\n", handle(from), g.strings.name(t), tos.join(", ")));
    }
}

impl Render {
    fn props_json(&self, props: Vec<(String, Value)>, out: &mut String) {
        out.push('{');
        let mut first = true;
        for (k, v) in props.iter().filter(|(k, v)| self.keep(k) && *v != Value::Null) {
            if !first {
                out.push(',');
            }
            first = false;
            write_json_string(k, out);
            out.push(':');
            match v {
                // Truncated text is still a JSON string; a vector becomes
                // the string "<vector N>".
                Value::Text(_) => self.value(v, out),
                Value::List(_) => {
                    let mut s = String::new();
                    self.value(v, &mut s);
                    if s.starts_with('<') {
                        write_json_string(&s, out);
                    } else {
                        v.write_json(out);
                    }
                }
                _ => v.write_json(out),
            }
        }
        out.push('}');
    }

    fn node_json(&self, g: &Graph, id: u64) -> String {
        let mut out = format!("{{\"id\":{id},\"labels\":[");
        for (i, l) in g.node_labels(id).iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_string(l, &mut out);
        }
        out.push_str("],\"props\":");
        self.props_json(g.node_props(id), &mut out);
        out.push('}');
        out
    }

    fn edge_json(&self, g: &Graph, eid: u64) -> String {
        let Some(e) = g.edge(eid) else { return String::new() };
        let mut out = format!("{{\"id\":{eid},\"type\":");
        write_json_string(g.strings.name(e.etype), &mut out);
        out.push_str(&format!(",\"from\":{},\"to\":{},\"props\":", e.from, e.to));
        self.props_json(g.edge_props(eid), &mut out);
        out.push('}');
        out
    }
}

fn to_json(g: &Graph, r: &Render, nodes: &[u64], edges: &[u64], seen: &IdSet) -> String {
    let nodes: Vec<String> = nodes.iter().filter(|id| !seen.contains(id)).map(|&id| r.node_json(g, id)).collect();
    let edges: Vec<String> = edges.iter().map(|&e| r.edge_json(g, e)).filter(|s| !s.is_empty()).collect();
    format!("{{\"nodes\":[{}],\"edges\":[{}]}}", nodes.join(","), edges.join(","))
}

// ------------------------------------------------------------------ paths

/// One side of a bidirectional search: hops from its end, and every
/// predecessor at the shortest distance (with the edge used).
type Side = IdMap<(u32, Vec<(u64, u64)>)>;

fn paths(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let g: &Graph = g;
    let from = *a.ids("from")?.first().ok_or_else(|| Error::Msg("paths needs from:".into()))?;
    let to = *a.ids("to")?.first().ok_or_else(|| Error::Msg("paths needs to:".into()))?;
    if g.node(from).is_none() || g.node(to).is_none() {
        return Err(Error::Msg("from: and to: must be existing nodes".into()));
    }
    let k = a.usize("k", 3).max(1);
    let max_depth = a.usize("depth", 6) as u32;
    let budget = a.usize("budget", 1000);
    let walk = Walk::from(g, a)?;
    let render = Render::from(a);
    let cap = (g.algorithm_memory() / 96).max(1 << 16) as usize;
    let back = match walk.dir {
        Dir::Out => Dir::In,
        Dir::In => Dir::Out,
        Dir::Both => Dir::Both,
    };

    // Widen the smaller side one whole level at a time, so every shortest
    // predecessor is recorded, until the sides meet.
    let mut fwd: Side = id_map();
    let mut bwd: Side = id_map();
    fwd.insert(from, (0, Vec::new()));
    bwd.insert(to, (0, Vec::new()));
    let (mut ff, mut bf) = (vec![from], vec![to]);
    let (mut fd, mut bd) = (0u32, 0u32);
    let mut meet: Vec<u64> = if from == to { vec![from] } else { Vec::new() };
    let mut adj = Vec::new();
    while meet.is_empty() && !ff.is_empty() && !bf.is_empty() && fd + bd < max_depth {
        let forward = ff.len() <= bf.len();
        let (frontier, seen, other, d, depth) =
            if forward { (&mut ff, &mut fwd, &bwd, walk.dir, &mut fd) } else { (&mut bf, &mut bwd, &fwd, back, &mut bd) };
        let mut next = Vec::new();
        for &u in frontier.iter() {
            walk.adj(g, u, d, usize::MAX - 1, &mut adj);
            for &(edge, v, _, _) in &adj {
                match seen.get_mut(&v) {
                    Some((dv, preds)) => {
                        if *dv == *depth + 1 {
                            preds.push((u, edge));
                        }
                    }
                    None => {
                        if v != from && v != to && !walk.node_ok(g, v) {
                            continue;
                        }
                        seen.insert(v, (*depth + 1, vec![(u, edge)]));
                        next.push(v);
                    }
                }
            }
            if fwd_len(seen, other) > cap {
                return Err(Error::Msg(format!(
                    "path search outgrew its memory ({cap} nodes); narrow it with types:, labels: or a smaller depth:"
                )));
            }
        }
        *depth += 1;
        meet = next.iter().copied().filter(|v| other.contains_key(v)).collect();
        *frontier = next;
    }
    if meet.is_empty() {
        let mut r = text_result(format!("# no path from {} to {} within {max_depth} hops\n", handle(from), handle(to)));
        r.message = Some("no path".into());
        return Ok(r);
    }

    // Specific routes first: a path through low-degree nodes says more
    // than one through a hub everything touches.
    let mut deg: IdMap<usize> = id_map();
    let mut degree = |id: u64| *deg.entry(id).or_insert_with(|| g.degree_of(id, Dir::Both, None));
    meet.sort_by_key(|&m| (degree(m), m));

    fn walk_back(side: &Side, at: u64, k: usize, deg: &mut dyn FnMut(u64) -> usize, cur: &mut Vec<(u64, u64)>, out: &mut Vec<Vec<(u64, u64)>>) {
        if out.len() >= k {
            return;
        }
        let (_, preds) = &side[&at];
        if preds.is_empty() {
            out.push(cur.clone());
            return;
        }
        let mut ps = preds.clone();
        ps.sort_by_key(|&(p, e)| (deg(p), p, e));
        for (p, e) in ps {
            cur.push((p, e));
            walk_back(side, p, k, deg, cur, out);
            cur.pop();
            if out.len() >= k {
                return;
            }
        }
    }

    // Each path as its node sequence plus the edges between them.
    let mut found: Vec<(Vec<u64>, Vec<u64>)> = Vec::new();
    for &m in &meet {
        let mut heads = Vec::new();
        walk_back(&fwd, m, k, &mut degree, &mut Vec::new(), &mut heads);
        let mut tails = Vec::new();
        walk_back(&bwd, m, k, &mut degree, &mut Vec::new(), &mut tails);
        for h in &heads {
            for t in &tails {
                if found.len() >= k {
                    break;
                }
                // heads run m → from; tails run m → to.
                let mut nodes: Vec<u64> = h.iter().rev().map(|x| x.0).collect();
                let mut edges: Vec<u64> = h.iter().rev().map(|x| x.1).collect();
                nodes.push(m);
                for &(n, e) in t {
                    edges.push(e);
                    nodes.push(n);
                }
                found.push((nodes, edges));
            }
        }
        if found.len() >= k {
            break;
        }
    }

    let hops = found[0].1.len();
    let mut lines = String::new();
    for (nodes, edges) in &found {
        let mut s = handle(nodes[0]);
        for (i, &eid) in edges.iter().enumerate() {
            let next = nodes[i + 1];
            let Some(e) = g.edge(eid) else { continue };
            let t = g.strings.name(e.etype);
            let props = render.edge_props(g, eid);
            if e.from == nodes[i] {
                s.push_str(&format!(" -{t}{props}-> {}", handle(next)));
            } else {
                s.push_str(&format!(" <-{t}{props}- {}", handle(next)));
            }
        }
        lines.push_str(&s);
        lines.push('\n');
    }
    let mut text = format!(
        "# paths {} to {}: {} shortest, {hops} hops{}\n",
        handle(from),
        handle(to),
        found.len(),
        if found.len() >= k { " (more may exist)" } else { "" }
    );
    text.push_str(&lines);
    // The nodes on the paths, while the budget allows, in path order.
    let mut used = tokens(&text);
    let mut listed = id_set();
    for (nodes, _) in &found {
        for &n in nodes {
            if !listed.insert(n) {
                continue;
            }
            let c = render.card(g, n);
            let t = tokens(&c) + 1;
            if used + t > budget {
                continue;
            }
            used += t;
            text.push_str(&c);
            text.push('\n');
        }
    }
    if a.str("format").is_some_and(|f| f.eq_ignore_ascii_case("rows")) {
        let rows = found
            .iter()
            .enumerate()
            .map(|(i, (nodes, edges))| {
                vec![
                    Value::Int(i as i64),
                    Value::List(nodes.iter().map(|&n| Value::Int(n as i64)).collect()),
                    Value::List(edges.iter().map(|&e| Value::Int(e as i64)).collect()),
                    Value::Int(edges.len() as i64),
                ]
            })
            .collect();
        return Ok(QueryResult::table(vec!["path".into(), "nodes".into(), "edges".into(), "hops".into()], rows));
    }
    let mut r = text_result(text);
    r.message = Some(format!("{} paths of {hops} hops", found.len()));
    Ok(r)
}

fn fwd_len(a: &Side, b: &Side) -> usize {
    a.len() + b.len()
}

// ------------------------------------------------------------------ schema

fn type_name(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(_) => "bool".into(),
        Value::Int(_) => "int".into(),
        Value::Float(_) => "float".into(),
        Value::Text(_) => "text".into(),
        Value::List(items) => {
            if items.len() >= VECTOR_MIN && items.iter().all(|x| matches!(x, Value::Int(_) | Value::Float(_))) {
                format!("vector[{}]", items.len())
            } else {
                match items.first() {
                    Some(x) => format!("list<{}>", type_name(x)),
                    None => "list".into(),
                }
            }
        }
    }
}

/// Property keys of a sample, each with its type and — when the sample
/// shows only a few short distinct values — those values, which is what a
/// model needs to write `WHERE p.status = "open"` instead of guessing.
fn key_summary(samples: &[Vec<(String, Value)>]) -> String {
    let mut keys: Vec<(String, Vec<String>, Vec<String>, usize)> = Vec::new();
    for props in samples {
        for (k, v) in props {
            if k.starts_with('_') || *v == Value::Null {
                continue;
            }
            let i = match keys.iter().position(|x| x.0 == *k) {
                Some(i) => i,
                None => {
                    keys.push((k.clone(), Vec::new(), Vec::new(), 0));
                    keys.len() - 1
                }
            };
            let e = &mut keys[i];
            e.3 += 1;
            let t = type_name(v);
            if !e.1.contains(&t) {
                e.1.push(t);
            }
            if let Value::Text(s) = v {
                if s.chars().count() <= 24 && e.2.len() <= 6 && !e.2.contains(s) {
                    e.2.push(s.clone());
                }
            }
        }
    }
    keys.sort_by(|a, b| key_rank(&a.0).cmp(&key_rank(&b.0)).then(b.3.cmp(&a.3)).then(a.0.cmp(&b.0)));
    let n = samples.len();
    keys.iter()
        .map(|(k, types, vals, count)| {
            let mut s = format!("{k}:{}", types.join("|"));
            // Enum-like: few distinct short values over a fair sample.
            if types.len() == 1 && types[0] == "text" && !vals.is_empty() && vals.len() <= 5 && *count >= 8 && *count >= vals.len() * 3 {
                let quoted: Vec<String> = vals.iter().map(|v| format!("{v:?}")).collect();
                s.push_str(&format!(" in {{{}}}", quoted.join(",")));
            }
            if *count * 10 < n * 9 {
                s.push('?');
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn schema(g: &Graph, sample: usize) -> String {
    let st = g.stats();
    let sample = sample.clamp(1, 10_000);
    let mut out = format!("# schema: {} nodes, {} edges (types from a sample of {sample} per label; ? = optional)\n", st.nodes, st.edges);
    let mut labels = st.labels.clone();
    labels.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (l, n) in &labels {
        let Some(lid) = g.strings.lookup(l) else { continue };
        let ids = g.label_members_from(lid, 0, sample);
        let samples: Vec<Vec<(String, Value)>> = ids.iter().map(|&id| g.node_props(id)).collect();
        let keys = key_summary(&samples);
        out.push_str(&format!("(:{l}) {n}"));
        if !keys.is_empty() {
            out.push_str(" {");
            out.push_str(&keys);
            out.push('}');
        }
        out.push('\n');
    }
    let mut types = st.edge_types.clone();
    types.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (t, n) in &types {
        let Some(tid) = g.strings.lookup(t) else { continue };
        let ids = g.type_members_from(tid, 0, sample);
        let mut patterns: Vec<((String, String), usize)> = Vec::new();
        let mut samples = Vec::new();
        let first_label = |id: u64| g.node_labels(id).into_iter().next().map(|l| format!(":{l}")).unwrap_or_default();
        for &eid in &ids {
            let Some(e) = g.edge(eid) else { continue };
            let key = (first_label(e.from), first_label(e.to));
            match patterns.iter_mut().find(|p| p.0 == key) {
                Some(p) => p.1 += 1,
                None => patterns.push((key, 1)),
            }
            samples.push(g.edge_props(eid));
        }
        patterns.sort_by_key(|p| std::cmp::Reverse(p.1));
        let keys = key_summary(&samples);
        let props = if keys.is_empty() { String::new() } else { format!(" {{{keys}}}") };
        let total = ids.len().max(1);
        for ((fl, tl), c) in patterns.iter().take(4) {
            let est = if ids.len() < sample { *c } else { (*c as f64 / total as f64 * *n as f64).round() as usize };
            let approx = if ids.len() < sample { "" } else { "~" };
            out.push_str(&format!("({fl})-[:{t}{props}]->({tl}) {approx}{est}\n"));
        }
        if patterns.len() > 4 {
            out.push_str(&format!("(…)-[:{t}]->(…) +{} more patterns\n", patterns.len() - 4));
        }
    }
    if !st.indexes.is_empty() {
        let idx: Vec<String> = st.indexes.iter().map(|(l, k, _)| format!(":{l}({k})")).collect();
        out.push_str(&format!("# indexes: {}\n", idx.join(" ")));
    }
    let exts: Vec<String> = g.extensions().into_iter().map(|(n, v)| format!("{n} {v}")).collect();
    out.push_str(&format!("# extensions: {}\n", exts.join(", ")));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimates_are_in_the_right_range() {
        assert_eq!(tokens(""), 0);
        assert_eq!(tokens("hello"), 2);
        assert_eq!(tokens("n12 -KNOWS-> n14"), 7);
        // ~1.3 tokens per English word is the usual rule of thumb.
        let s = "The quick brown fox jumps over the lazy dog near the river bank";
        let t = tokens(s);
        assert!((13..=24).contains(&t), "{t}");
    }

    #[test]
    fn floats_render_short() {
        assert_eq!(float(1.0), "1.0");
        assert_eq!(float(0.123456789), "0.123457");
        assert_eq!(float(1e-9), "1.0000e-9");
    }

    #[test]
    fn truncation_prefers_word_boundaries() {
        assert_eq!(truncate("alpha beta gamma delta", 14), "alpha beta");
        assert_eq!(truncate("short", 14), "short");
    }

    #[test]
    fn query_terms_drop_stopwords_and_plurals() {
        assert_eq!(query_terms("Who wrote the papers on graphs?"), vec!["wrote", "paper", "graph"]);
        assert_eq!(query_terms("the"), vec!["the"]);
    }
}
