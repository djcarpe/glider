//! The `context` extension's full-text index: an inverted index in the
//! extension's own keyspace, built once by `CALL context.index()` and kept
//! current by the node write hook, in the writing transaction.
//!
//! ```text
//! !idx              -> labels, keys                (configuration)
//! s                 -> documents, total length     (for BM25)
//! f term            -> documents containing term
//! p term 0 node     -> weighted term frequency     (the postings)
//! l node            -> document length
//! ```
//!
//! Postings for one term are one range scan, in node order. A query reads
//! the postings of its terms rarest first, scores with BM25 without length
//! normalisation, and re-ranks the best few hundred exactly. Memory is the
//! accumulator for the postings read, capped per term.

use std::collections::HashMap;

use super::context::{key_rank, query_terms, terms};
use crate::codec::{put_str, put_varint, Reader};
use crate::graph::{Graph, Result};
use crate::types::id_map;
use crate::value::Value;

pub const NS: &str = "context";
const CONFIG: &[u8] = b"!idx";
const MAX_TERM: usize = 64;
/// Postings read per query term at most. A term in more documents than
/// this says little about any of them, and BM25 weights it near zero.
pub const MAX_POSTINGS: usize = 50_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// Index nodes with any of these labels; every node when empty.
    pub labels: Vec<String>,
    /// Index these text keys; every text property when empty.
    pub keys: Vec<String>,
}

impl Config {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_varint(&mut out, self.labels.len() as u64);
        for l in &self.labels {
            put_str(&mut out, l);
        }
        put_varint(&mut out, self.keys.len() as u64);
        for k in &self.keys {
            put_str(&mut out, k);
        }
        out
    }

    fn decode(b: &[u8]) -> Option<Config> {
        let mut r = Reader::new(b);
        let mut c = Config::default();
        for _ in 0..r.varint().ok()? {
            c.labels.push(r.string().ok()?);
        }
        for _ in 0..r.varint().ok()? {
            c.keys.push(r.string().ok()?);
        }
        Some(c)
    }
}

pub fn config(g: &Graph) -> Option<Config> {
    Config::decode(&g.ext_get(NS, CONFIG)?)
}

/// A node's terms with weighted frequencies (names count double), and its
/// length in words; `None` when the index does not cover the node.
pub fn doc_terms(c: &Config, labels: &[String], props: &[(String, Value)]) -> Option<(HashMap<String, u32>, u32)> {
    if !c.labels.is_empty() && !labels.iter().any(|l| c.labels.contains(l)) {
        return None;
    }
    let mut tf: HashMap<String, u32> = HashMap::new();
    let mut len = 0u32;
    for (k, v) in props {
        if k.starts_with('_') || (!c.keys.is_empty() && !c.keys.contains(k)) {
            continue;
        }
        let boost = if key_rank(k) == 0 { 2 } else { 1 };
        let mut text = |s: &str| {
            terms(s, |t| {
                len += 1;
                if t.len() <= MAX_TERM {
                    *tf.entry(t.to_string()).or_default() += boost;
                }
            })
        };
        match v {
            Value::Text(s) => text(s),
            Value::List(items) => {
                for x in items {
                    if let Value::Text(s) = x {
                        text(s);
                    }
                }
            }
            _ => {}
        }
    }
    if tf.is_empty() {
        return None;
    }
    Some((tf, len))
}

fn varint(b: &[u8]) -> u64 {
    Reader::new(b).varint().unwrap_or(0)
}

fn enc(v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, v);
    out
}

fn posting_key(term: &str, node: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(term.len() + 10);
    k.push(b'p');
    k.extend_from_slice(term.as_bytes());
    k.push(0);
    k.extend_from_slice(&node.to_be_bytes());
    k
}

fn df_key(term: &str) -> Vec<u8> {
    let mut k = vec![b'f'];
    k.extend_from_slice(term.as_bytes());
    k
}

fn len_key(node: u64) -> Vec<u8> {
    let mut k = vec![b'l'];
    k.extend_from_slice(&node.to_be_bytes());
    k
}

fn stats(g: &Graph) -> (u64, u64) {
    match g.ext_get(NS, b"s") {
        Some(b) => {
            let mut r = Reader::new(&b);
            (r.varint().unwrap_or(0), r.varint().unwrap_or(0))
        }
        None => (0, 0),
    }
}

fn stats_val(docs: u64, total: u64) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, docs);
    put_varint(&mut out, total);
    out
}

/// Keep the index right after one node changed: delete the postings it no
/// longer has, write the ones it now has, adjust document frequencies and
/// totals. One logged record.
/// A node's labels and properties, borrowed.
pub type NodeView<'a> = (&'a [String], &'a [(String, Value)]);

pub fn on_node_write(g: &mut Graph, c: &Config, id: u64, before: Option<NodeView<'_>>, after: Option<NodeView<'_>>) -> Result<()> {
    let old = before.and_then(|(l, p)| doc_terms(c, l, p));
    let new = after.and_then(|(l, p)| doc_terms(c, l, p));
    if old == new {
        return Ok(());
    }
    let (mut puts, mut dels) = (Vec::new(), Vec::new());
    let (mut docs, mut total) = stats(g);
    let empty = HashMap::new();
    let (old_tf, old_len) = old.as_ref().map(|(t, l)| (t, *l)).unwrap_or((&empty, 0));
    let (new_tf, new_len) = new.as_ref().map(|(t, l)| (t, *l)).unwrap_or((&empty, 0));
    for t in old_tf.keys().filter(|t| !new_tf.contains_key(*t)) {
        dels.push(posting_key(t, id));
        let df = g.ext_get(NS, &df_key(t)).map(|b| varint(&b)).unwrap_or(0);
        if df <= 1 {
            dels.push(df_key(t));
        } else {
            puts.push((df_key(t), enc(df - 1)));
        }
    }
    for (t, &f) in new_tf {
        match old_tf.get(t) {
            Some(&o) if o == f => {}
            Some(_) => puts.push((posting_key(t, id), enc(f as u64))),
            None => {
                puts.push((posting_key(t, id), enc(f as u64)));
                let df = g.ext_get(NS, &df_key(t)).map(|b| varint(&b)).unwrap_or(0);
                puts.push((df_key(t), enc(df + 1)));
            }
        }
    }
    if old.is_some() {
        docs = docs.saturating_sub(1);
        total = total.saturating_sub(old_len as u64);
    }
    if new.is_some() {
        docs += 1;
        total += new_len as u64;
        puts.push((len_key(id), enc(new_len as u64)));
    } else {
        dels.push(len_key(id));
    }
    puts.push((b"s".to_vec(), stats_val(docs, total)));
    g.ext_write(NS, puts, dels)
}

/// What `CALL context.index()` reports.
pub struct Built {
    pub docs: u64,
    pub terms: u64,
    pub postings: u64,
}

fn clear_data(g: &mut Graph) -> Result<()> {
    for prefix in [&b"p"[..], b"f", b"l", b"s"] {
        loop {
            let mut batch = Vec::new();
            g.ext_scan(NS, prefix, &mut |k, _| {
                batch.push(k.to_vec());
                batch.len() < 4096
            });
            if batch.is_empty() {
                break;
            }
            g.ext_write(NS, Vec::new(), batch)?;
        }
    }
    Ok(())
}

/// (Re)build the index over every covered node. Document frequencies are
/// counted in memory and flushed into the tree when the table grows past
/// `flush_terms`, so memory is bounded by vocabulary, not graph size.
pub fn build(g: &mut Graph, c: &Config, flush_terms: usize) -> Result<Built> {
    clear_data(g)?;
    g.ext_write(NS, vec![(CONFIG.to_vec(), c.encode())], Vec::new())?;
    let label_ids: Vec<u32> = c.labels.iter().filter_map(|l| g.strings.lookup(l)).collect();
    if !c.labels.is_empty() && label_ids.is_empty() {
        g.ext_write(NS, vec![(b"s".to_vec(), stats_val(0, 0))], Vec::new())?;
        return Ok(Built { docs: 0, terms: 0, postings: 0 });
    }
    let mut df: HashMap<String, u64> = HashMap::new();
    let (mut docs, mut total, mut postings) = (0u64, 0u64, 0u64);
    let mut seen = id_map::<()>();
    let flush_df = |g: &mut Graph, df: &mut HashMap<String, u64>| -> Result<()> {
        let mut puts = Vec::with_capacity(df.len());
        for (t, n) in df.drain() {
            let had = g.ext_get(NS, &df_key(&t)).map(|b| varint(&b)).unwrap_or(0);
            puts.push((df_key(&t), enc(had + n)));
            if puts.len() >= 4096 {
                g.ext_write(NS, std::mem::take(&mut puts), Vec::new())?;
            }
        }
        g.ext_write(NS, puts, Vec::new())
    };
    // One pass per label (or one over all nodes); a node in two indexed
    // labels is indexed once.
    let passes: Vec<Option<u32>> = if label_ids.is_empty() { vec![None] } else { label_ids.into_iter().map(Some).collect() };
    let multi = passes.len() > 1;
    for label in passes {
        let mut from = 0u64;
        loop {
            let mut batch: Vec<u64> = Vec::new();
            let r = g.scan_nodes(label, from, 2048, &mut |id| {
                batch.push(id);
                true
            });
            let Some(&last) = batch.last() else { break };
            let mut puts = Vec::new();
            for id in &batch {
                if multi && seen.insert(*id, ()).is_some() {
                    continue;
                }
                let labels = g.node_labels(*id);
                let props = g.node_props(*id);
                let Some((tf, len)) = doc_terms(c, &labels, &props) else { continue };
                docs += 1;
                total += len as u64;
                for (t, f) in tf {
                    puts.push((posting_key(&t, *id), enc(f as u64)));
                    *df.entry(t).or_default() += 1;
                    postings += 1;
                }
                puts.push((len_key(*id), enc(len as u64)));
            }
            g.ext_write(NS, puts, Vec::new())?;
            if df.len() > flush_terms {
                flush_df(g, &mut df)?;
            }
            match r {
                Some((2048, _)) => from = last + 1,
                _ => break,
            }
        }
    }
    flush_df(g, &mut df)?;
    g.ext_write(NS, vec![(b"s".to_vec(), stats_val(docs, total))], Vec::new())?;
    let mut terms = 0u64;
    g.ext_scan(NS, b"f", &mut |_, _| {
        terms += 1;
        true
    });
    Ok(Built { docs, terms, postings })
}

pub fn drop_index(g: &mut Graph) -> Result<()> {
    clear_data(g)?;
    g.ext_write(NS, Vec::new(), vec![CONFIG.to_vec()])
}

/// BM25 through the index: (node, score) best first, the postings read,
/// and how many documents matched any term.
pub fn search(g: &Graph, q: &str, top: usize, label: Option<u32>) -> (Vec<(u64, f64)>, usize, usize) {
    let qt = query_terms(q);
    let (docs, total) = stats(g);
    let n = docs.max(1) as f64;
    let avg = (total as f64 / n).max(1.0);
    let (k1, b) = (1.2, 0.75);
    let mut terms: Vec<(String, u64)> = qt
        .into_iter()
        .filter_map(|t| {
            let df = g.ext_get(NS, &df_key(&t)).map(|b| varint(&b))?;
            Some((t, df))
        })
        .collect();
    terms.sort_by_key(|t| t.1);
    let mut acc = id_map::<(f64, Vec<(f64, u32)>)>();
    let mut read = 0usize;
    for (t, df) in &terms {
        let idf = (1.0 + (n - *df as f64 + 0.5) / (*df as f64 + 0.5)).ln();
        let mut prefix = vec![b'p'];
        prefix.extend_from_slice(t.as_bytes());
        prefix.push(0);
        let mut count = 0usize;
        g.ext_scan(NS, &prefix, &mut |k, v| {
            let node = u64::from_be_bytes(k[k.len() - 8..].try_into().unwrap());
            let f = varint(v) as u32;
            let e = acc.entry(node).or_insert((0.0, Vec::new()));
            // Without length normalisation for now; exact below.
            e.0 += idf * f as f64 * (k1 + 1.0) / (f as f64 + k1);
            e.1.push((idf, f));
            count += 1;
            count < MAX_POSTINGS
        });
        read += count;
    }
    let matched = acc.len();
    // (node, partial score, (idf, tf) per matched term)
    type Scored = (u64, f64, Vec<(f64, u32)>);
    let mut cands: Vec<Scored> = acc.into_iter().map(|(id, (s, tf))| (id, s, tf)).collect();
    cands.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    cands.truncate((top * 16).max(256));
    let phrase = q.trim().to_lowercase();
    let mut out: Vec<(u64, f64)> = cands
        .into_iter()
        .filter(|(id, _, _)| label.map_or(true, |l| g.has_label(*id, l)))
        .map(|(id, _, tf)| {
            let len = g.ext_get(NS, &len_key(id)).map(|b| varint(&b)).unwrap_or(avg as u64) as f64;
            let mut s: f64 = tf.iter().map(|&(idf, f)| idf * f as f64 * (k1 + 1.0) / (f as f64 + k1 * (1.0 - b + b * len / avg))).sum();
            for key in ["name", "title"] {
                if let Some(Value::Text(v)) = g.node_prop(id, key) {
                    if v.trim().to_lowercase() == phrase {
                        s += 10.0;
                    }
                }
            }
            (id, s)
        })
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(top);
    (out, read, matched)
}
