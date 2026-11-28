//! Path patterns and chains.
//!
//! A *path pattern* is a sequence of labels and edge types that paths
//! follow: `(:Plant) -HAS_SHOP-> (:Shop) -HAS_LINE-> (:Line)`. Paths start
//! at the nodes of `labels:` (or, without it, at nodes no followed edge
//! reaches) and follow the edges of `types:` in `dir:`, at most `max_hops:`
//! of them. Counting is by sequence, level by level, with the paths that
//! reach one node along one sequence merged into a count, so the work is
//! bounded by nodes times distinct sequences per level, never by the
//! number of paths.
//!
//! A *chain* is a run of nodes that each have exactly one followed edge in
//! and one out (no self-loop), between two nodes that do not: `a -> x -> y
//! -> b`. It can be written as one hop with its interior listed, and
//! `expand(of: "chain:<first interior id>")` gives the interior back. A
//! chain is named by its first interior node, so chains come in id order
//! without any state kept between slices.

use std::collections::HashMap;

use super::notation;
use super::templates::{part_columns, part_rows, Hier, Part};
use super::{limit_arg, sip128, step};
use crate::ext::Args;
use crate::graph::{Dir, Error, Graph, Result};
use crate::query::QueryResult;
use crate::value::Value;

/// Hops a pattern may have when `max_hops:` is not given.
pub const MAX_HOPS: usize = 12;
/// Distinct sequences one call may count; past it the call is refused.
pub const MAX_SEQUENCES: usize = 100_000;
/// Interior nodes listed per chain row.
const INTERIOR: usize = 1_000;

fn labels_text(g: &Graph, id: u64) -> String {
    let mut s = String::from("(");
    if let Some(n) = notation::node_text(g, id) {
        for l in &n.labels {
            s.push(':');
            notation::name(l, &mut s);
        }
    }
    s.push(')');
    s
}

fn arrow(dir: Dir, t: &str) -> String {
    let mut n = String::new();
    notation::name(t, &mut n);
    match dir {
        Dir::In => format!(" <-{n}- "),
        _ => format!(" -{n}-> "),
    }
}

fn back(dir: Dir) -> Dir {
    match dir {
        Dir::Out => Dir::In,
        _ => Dir::Out,
    }
}

fn max_hops(a: &Args) -> Result<usize> {
    match a.get("max_hops") {
        None | Some(Value::Null) => Ok(MAX_HOPS),
        Some(v) => match v.as_i64() {
            Some(n) if (1..=64).contains(&n) => Ok(n as usize),
            _ => Err(Error::Msg(format!(
                "max_hops: {v} is not a number of hops; give 1 to 64"
            ))),
        },
    }
}

pub fn paths(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let h = Hier::from(g, a)?;
    match a.str("kind").map(|k| k.to_ascii_lowercase()).as_deref() {
        None | Some("patterns") | Some("pattern") => patterns(g, &h, a),
        Some("chains") | Some("chain") => chains(g, &h, a),
        Some(k) => Err(Error::Msg(format!(
            "kind: '{k}' is not a kind of path; use \"patterns\" or \"chains\""
        ))),
    }
}

struct Seq {
    parent: u128,
    piece: String,
    hops: u32,
    paths: u64,
    ends: u64,
    extended: bool,
}

fn patterns(g: &Graph, h: &Hier, a: &Args) -> Result<QueryResult> {
    let hops = max_hops(a)?;
    let limit = limit_arg(a)?;
    let after = a.usize("after", 0);
    let min_count = h.min_count as u64;
    let label_ids: Vec<u32> = h
        .labels
        .iter()
        .filter_map(|l| g.strings.lookup(l))
        .collect();
    let mut seqs: HashMap<u128, Seq> = HashMap::new();
    let mut level = super::sorter(g, "structure-paths")?;
    let mut starts = 0u64;
    let mut failed: Option<Error> = None;
    g.scan_nodes(None, 0, usize::MAX, &mut |id| {
        if let Err(e) = step(1) {
            failed = Some(e);
            return false;
        }
        let start = if h.labels.is_empty() {
            let mut reached = false;
            g.for_each_adj(id, back(h.dir), None, |e| {
                reached |= h.followed(e.etype) && e.other != id;
            });
            !reached
        } else {
            label_ids.iter().any(|&l| g.has_label(id, l))
        };
        if !start {
            return true;
        }
        let piece = labels_text(g, id);
        let s = sip128(piece.as_bytes());
        seqs.entry(s).or_insert(Seq {
            parent: 0,
            piece,
            hops: 0,
            paths: 0,
            ends: 0,
            extended: false,
        });
        let mut k = s.to_be_bytes().to_vec();
        k.extend_from_slice(&id.to_be_bytes());
        if let Err(e) = level.push(&k, &1u64.to_be_bytes()) {
            failed = Some(Error::Io(e));
            return false;
        }
        starts += 1;
        true
    });
    if let Some(e) = failed {
        return Err(e);
    }
    for depth in 0..=hops {
        let mut sorted = level.finish().map_err(Error::Io)?;
        let mut next = super::sorter(g, "structure-paths")?;
        let mut cur: Option<(Vec<u8>, u64)> = None;
        loop {
            let item = sorted.next().map_err(Error::Io)?;
            let same = match (&cur, &item) {
                (Some((k, _)), Some((k2, _))) => k == k2,
                _ => false,
            };
            if same {
                let c = u64::from_be_bytes(item.unwrap().1[..8].try_into().unwrap());
                cur.as_mut().unwrap().1 = cur.as_ref().unwrap().1.saturating_add(c);
                continue;
            }
            if let Some((k, count)) = cur.take() {
                let s = u128::from_be_bytes(k[..16].try_into().unwrap());
                let id = u64::from_be_bytes(k[16..24].try_into().unwrap());
                let e = seqs.get_mut(&s).unwrap();
                e.paths = e.paths.saturating_add(count);
                e.ends += 1;
                if depth < hops {
                    let mut out: Vec<(u32, u64)> = Vec::new();
                    g.for_each_adj(id, h.dir, None, |e| {
                        if h.followed(e.etype) {
                            out.push((e.etype, e.other));
                        }
                    });
                    step(1 + out.len() as u64)?;
                    for (t, other) in out {
                        let piece = format!(
                            "{}{}",
                            arrow(h.dir, g.strings.name(t)),
                            labels_text(g, other)
                        );
                        let mut b = s.to_be_bytes().to_vec();
                        b.extend_from_slice(piece.as_bytes());
                        let ns = sip128(&b);
                        if !seqs.contains_key(&ns) {
                            if seqs.len() == MAX_SEQUENCES {
                                return Err(Error::Msg(format!(
                                    "more than {MAX_SEQUENCES} distinct label and type sequences; give fewer types:, start labels: or a smaller max_hops:"
                                )));
                            }
                            seqs.insert(
                                ns,
                                Seq {
                                    parent: s,
                                    piece,
                                    hops: depth as u32 + 1,
                                    paths: 0,
                                    ends: 0,
                                    extended: false,
                                },
                            );
                        }
                        let mut k = ns.to_be_bytes().to_vec();
                        k.extend_from_slice(&other.to_be_bytes());
                        next.push(&k, &count.to_be_bytes()).map_err(Error::Io)?;
                    }
                }
            }
            match item {
                Some((k, v)) => cur = Some((k, u64::from_be_bytes(v[..8].try_into().unwrap()))),
                None => break,
            }
        }
        level = next;
    }
    // A pattern is maximal when no one-hop longer pattern is followed as
    // often as asked.
    let keys: Vec<u128> = seqs.keys().copied().collect();
    for k in &keys {
        let (p, paths, hops) = {
            let s = &seqs[k];
            (s.parent, s.paths, s.hops)
        };
        if hops > 0 && paths >= min_count {
            if let Some(ps) = seqs.get_mut(&p) {
                ps.extended = true;
            }
        }
    }
    let text = |k: u128| -> String {
        let mut parts = Vec::new();
        let mut at = k;
        loop {
            let s = &seqs[&at];
            parts.push(s.piece.clone());
            if s.hops == 0 {
                break;
            }
            at = s.parent;
        }
        parts.reverse();
        parts.concat()
    };
    let mut found: Vec<(bool, u32, u64, String, u128, u64)> = keys
        .iter()
        .filter(|k| seqs[k].hops > 0 && seqs[k].paths >= min_count)
        .map(|k| {
            let s = &seqs[k];
            (!s.extended, s.hops, s.paths, text(*k), *k, s.ends)
        })
        .collect();
    found.sort_by(|a, b| {
        (b.0, b.1, b.2)
            .cmp(&(a.0, a.1, a.2))
            .then_with(|| a.3.cmp(&b.3))
    });
    let total = found.len();
    let rows: Vec<Vec<Value>> = found
        .into_iter()
        .enumerate()
        .skip(after)
        .take(limit)
        .map(|(i, (maximal, hops, paths, text, k, ends))| {
            vec![
                Value::Int(i as i64 + 1),
                Value::Text(format!("{:016x}", (k >> 64) as u64)),
                Value::Text(text),
                Value::Int(hops as i64),
                Value::Int(paths.min(i64::MAX as u64) as i64),
                Value::Int(ends as i64),
                Value::Bool(maximal),
            ]
        })
        .collect();
    let shown = rows.len();
    let mut r = QueryResult::table(
        [
            "rank", "pattern", "path", "hops", "paths", "ends", "maximal",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        rows,
    );
    let mut msg = format!(
        "{total} label and type sequences followed by {min_count} or more paths from {starts} start nodes, up to {hops} hops; paths counts the paths that follow each, ends the distinct nodes they end at"
    );
    if after + shown < total {
        msg.push_str(&format!(
            "; more: CALL structure.paths again with after: {} and the same arguments",
            after + shown
        ));
    }
    r.message = Some(msg);
    Ok(r)
}

// ------------------------------------------------------------------ chains

/// The one followed edge into `v` and the one out of it, when it has
/// exactly those: (edge in, node before, edge out, node after).
pub fn interior(g: &Graph, h: &Hier, v: u64) -> Option<(u64, u64, u64, u64)> {
    let mut fwd = (0u32, 0u64, 0u64);
    g.for_each_adj_while(v, h.dir, None, |a| {
        if h.followed(a.etype) {
            fwd = (fwd.0 + 1, a.edge, a.other);
        }
        fwd.0 < 2
    });
    if fwd.0 != 1 || fwd.2 == v {
        return None;
    }
    let mut bwd = (0u32, 0u64, 0u64);
    g.for_each_adj_while(v, back(h.dir), None, |a| {
        if h.followed(a.etype) {
            bwd = (bwd.0 + 1, a.edge, a.other);
        }
        bwd.0 < 2
    });
    if bwd.0 != 1 || bwd.2 == v {
        return None;
    }
    Some((bwd.1, bwd.2, fwd.1, fwd.2))
}

/// A run of interior nodes between two that are not.
#[derive(Clone, Debug)]
pub struct Chain {
    pub from: u64,
    pub to: u64,
    pub interior: Vec<u64>,
    /// The edges in order, from `from` to `to`.
    pub edges: Vec<u64>,
    pub types: Vec<String>,
    pub labels: Vec<Vec<String>>,
}

impl Chain {
    /// The pattern it follows: types and interior labels.
    pub fn pattern(&self, dir: Dir) -> String {
        let mut s = String::new();
        for (i, t) in self.types.iter().enumerate() {
            s.push_str(arrow(dir, t).trim());
            if i < self.labels.len() {
                s.push_str(" (");
                for l in &self.labels[i] {
                    s.push(':');
                    notation::name(l, &mut s);
                }
                s.push_str(") ");
            }
        }
        s
    }

    pub fn pattern_id(&self, dir: Dir) -> String {
        format!(
            "{:016x}",
            (sip128(self.pattern(dir).as_bytes()) >> 64) as u64
        )
    }

    pub fn props_free(&self, g: &Graph) -> bool {
        self.edges.iter().all(|&e| g.edge_props(e).is_empty())
    }
}

/// The chain whose first interior node is `head`, if it is one.
pub fn chain_at(g: &Graph, h: &Hier, head: u64) -> Result<Option<Chain>> {
    let Some((e_in, before, mut e_out, mut next)) = interior(g, h, head) else {
        return Ok(None);
    };
    if interior(g, h, before).is_some() {
        return Ok(None);
    }
    let name = |e: u64| {
        g.edge(e)
            .map(|r| g.strings.name(r.etype).to_string())
            .unwrap_or_default()
    };
    let labels = |v: u64| {
        notation::node_text(g, v)
            .map(|n| n.labels)
            .unwrap_or_default()
    };
    let mut c = Chain {
        from: before,
        to: 0,
        interior: vec![head],
        edges: vec![e_in],
        types: vec![name(e_in)],
        labels: vec![labels(head)],
    };
    loop {
        step(1)?;
        c.edges.push(e_out);
        c.types.push(name(e_out));
        match interior(g, h, next) {
            Some((_, _, eo, nx)) if next != head => {
                c.interior.push(next);
                c.labels.push(labels(next));
                e_out = eo;
                next = nx;
            }
            _ => {
                c.to = next;
                return Ok(Some(c));
            }
        }
    }
}

fn chains(g: &Graph, h: &Hier, a: &Args) -> Result<QueryResult> {
    let limit = limit_arg(a)?;
    let after = a.ids("after")?.first().copied().unwrap_or(0);
    let mut rows = Vec::new();
    let mut more = false;
    let mut failed: Option<Error> = None;
    g.scan_nodes(None, after.saturating_add(1), usize::MAX, &mut |id| {
        if rows.len() == limit {
            more = true;
            return false;
        }
        let c = match step(1).and_then(|_| chain_at(g, h, id)) {
            Ok(c) => c,
            Err(e) => {
                failed = Some(e);
                return false;
            }
        };
        if let Some(c) = c {
            rows.push(vec![
                Value::Text(format!("chain:{id}")),
                Value::Text(c.pattern_id(h.dir)),
                Value::Text(c.pattern(h.dir)),
                Value::Int(c.edges.len() as i64),
                Value::Int(c.from as i64),
                Value::Int(c.to as i64),
                Value::Int(c.interior.len() as i64),
                Value::List(
                    c.interior
                        .iter()
                        .take(INTERIOR)
                        .map(|&x| Value::Int(x as i64))
                        .collect(),
                ),
            ]);
        }
        true
    });
    if let Some(e) = failed {
        return Err(e);
    }
    let mut r = QueryResult::table(
        [
            "chain", "pattern", "path", "hops", "from", "to", "interior", "nodes",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        rows,
    );
    let mut msg = format!(
        "{} chains: runs of nodes with one followed edge in and one out, each writable as one hop with its interior listed; CALL structure.expand(of: 'chain:<id>') gives the interior back",
        r.rows.len()
    );
    if more {
        let last = r
            .rows
            .last()
            .map(|row| row[0].to_string())
            .unwrap_or_default();
        msg.push_str(&format!(
            "; more: CALL structure.paths again with kind: 'chains', after: {} and the same arguments",
            last.trim_start_matches("chain:")
        ));
    }
    r.message = Some(msg);
    Ok(r)
}

/// `expand(of: "chain:<id>")`: the interior nodes and the edges of the run.
pub fn expand_chain(g: &Graph, h: &Hier, head: u64) -> Result<QueryResult> {
    let c = chain_at(g, h, head)?.ok_or_else(|| {
        Error::Msg(format!(
            "node {head} does not start a chain along these types: it needs one followed edge in from a node that is not in a chain and one out; CALL structure.paths(kind: 'chains', ...) lists them"
        ))
    })?;
    let mut parts = Vec::new();
    for &x in &c.interior {
        let n = notation::node_text(g, x).unwrap();
        parts.push(Part::Node {
            id: x,
            labels: n.labels,
            props: n.props,
        });
    }
    for &e in &c.edges {
        let er = g.edge(e).unwrap();
        let mut props = g.edge_props(e);
        notation::sort_props(&mut props);
        parts.push(Part::Edge {
            from: er.from,
            etype: g.strings.name(er.etype).to_string(),
            to: er.to,
            props,
        });
    }
    let mut r = QueryResult::table(part_columns(), part_rows(parts));
    r.message = Some(format!(
        "chain:{head} from node {} to node {}: {} interior nodes and {} edges ({})",
        c.from,
        c.to,
        c.interior.len(),
        c.edges.len(),
        c.pattern(h.dir)
    ));
    Ok(r)
}
