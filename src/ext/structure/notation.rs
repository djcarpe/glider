//! How a graph is written down as text, plainly and with a dictionary:
//! the notation `glider-compact/1`.
//!
//! It builds on how `context` writes nodes and edges (`n12:Label k=v`,
//! `n1 -TYPE{k=v}-> n2`, property-less edges of one type from one node on
//! one line), with two differences that make it a faithful encoding rather
//! than a summary: nothing is cut short (every property, every value in
//! full, lists and floats written so they read back as they were), and the
//! order of everything is fixed (labels by name, properties by
//! `context`'s key rank and then by name, edge lines by type and target).
//!
//! The plain text of a set of nodes is one *block* per node, in order: its
//! card, then its outgoing edges. A block ends with a newline, and the
//! token estimate counts nothing across a newline, so the length of a text
//! in bytes and in tokens is exactly the sum over its blocks: a choice made
//! for one block can be priced on that block alone.
//!
//! ```text
//! # glider-compact/1
//! n242:Station code="P01-S1-L1-ST010" name="Station 010" number=1
//! n242 -HAS_SIDE-> n243, n265
//! ```
//!
//! With `identity:` a node is written by its identity value (`@"P01-S1"`)
//! instead of its id, and that property is not repeated on its card; a node
//! without one keeps its id. The dictionary, templates and their uses are
//! written by [`super::templates`] and [`super::plan`] in the same
//! notation.

use crate::ext::context::{key_rank, tokens};
use crate::graph::Graph;
use crate::value::{write_json_string, Value};

/// The version of the notation, written at the head of every text.
pub const NOTATION: &str = "glider-compact/1";

/// Bytes and estimated tokens of some text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    pub bytes: u64,
    pub tokens: u64,
}

impl Cost {
    pub fn of(s: &str) -> Cost {
        Cost {
            bytes: s.len() as u64,
            tokens: tokens(s) as u64,
        }
    }
    pub fn add(&mut self, c: Cost) {
        self.bytes += c.bytes;
        self.tokens += c.tokens;
    }
    /// The figure a choice is made by.
    pub fn by(&self, tokens: bool) -> i64 {
        if tokens {
            self.tokens as i64
        } else {
            self.bytes as i64
        }
    }
}

/// A sink for text: counts what passes, and keeps it when asked to.
pub struct Sink {
    pub cost: Cost,
    pub keep: Option<String>,
}

impl Sink {
    pub fn new(keep: bool) -> Sink {
        Sink {
            cost: Cost::default(),
            keep: keep.then(String::new),
        }
    }
    /// One or more whole lines, each ending in a newline.
    pub fn put(&mut self, s: &str) {
        self.cost.add(Cost::of(s));
        if let Some(k) = self.keep.as_mut() {
            k.push_str(s);
        }
    }
}

/// A label, key or type name: bare when it is a plain word, otherwise in
/// backticks with a backtick doubled.
pub fn name(s: &str, out: &mut String) {
    let plain = !s.is_empty()
        && s.bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()));
    if plain {
        out.push_str(s);
    } else {
        out.push('`');
        out.push_str(&s.replace('`', "``"));
        out.push('`');
    }
}

/// A value, in full, so that it reads back as exactly this value: text as
/// a JSON string, an integer in digits, a float always with a point or an
/// exponent (`1.0`, `1e300`, `NaN`, `inf`), lists in brackets. Lists are
/// walked with a stack.
pub fn value(v: &Value, out: &mut String) {
    let mut stack: Vec<(std::slice::Iter<'_, Value>, bool)> = Vec::new();
    let mut next = Some(v);
    loop {
        let v = match next.take() {
            Some(v) => v,
            None => match stack.last_mut() {
                None => return,
                Some((it, first)) => match it.next() {
                    Some(v) => {
                        if !*first {
                            out.push_str(", ");
                        }
                        *first = false;
                        v
                    }
                    None => {
                        out.push(']');
                        stack.pop();
                        continue;
                    }
                },
            },
        };
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::Float(f) => {
                let s = format!("{f:?}");
                let marked = s.contains(['.', 'e', 'E', 'N', 'n']);
                out.push_str(&s);
                if !marked {
                    out.push_str(".0");
                }
            }
            Value::Text(s) => write_json_string(s, out),
            Value::List(items) => {
                out.push('[');
                stack.push((items.iter(), true));
            }
        }
    }
}

/// Properties in the order every text uses: names first (`context`'s key
/// rank), then by key.
pub fn sort_props(props: &mut [(String, Value)]) {
    props.sort_by(|a, b| (key_rank(&a.0), &a.0).cmp(&(key_rank(&b.0), &b.0)));
}

/// How nodes are named in a text.
#[derive(Clone, Debug, Default)]
pub struct Names {
    /// Properties that identify a node, the first one a node has winning.
    pub identity: Vec<String>,
}

impl Names {
    /// The identity key a node with these properties is written by.
    pub fn identity_key<'a>(&self, props: &'a [(String, Value)]) -> Option<&'a str> {
        self.identity.iter().find_map(|k| {
            props
                .iter()
                .find(|(pk, v)| pk == k && *v != Value::Null)
                .map(|(pk, _)| pk.as_str())
        })
    }

    /// `n12`, or `@"P01-S1"` by identity.
    pub fn handle_of(&self, id: u64, props: &[(String, Value)]) -> String {
        if let Some(k) = self.identity_key(props) {
            let v = &props.iter().find(|(pk, _)| pk == k).unwrap().1;
            let mut s = String::from("@");
            value(v, &mut s);
            return s;
        }
        format!("n{id}")
    }

    pub fn handle(&self, g: &Graph, id: u64) -> String {
        if self.identity.is_empty() {
            return format!("n{id}");
        }
        let props = g.node_props(id);
        self.handle_of(id, &props)
    }
}

/// A node as the plain text reads it.
pub struct NodeText {
    pub labels: Vec<String>,
    pub props: Vec<(String, Value)>,
}

pub fn node_text(g: &Graph, id: u64) -> Option<NodeText> {
    let node = g.node_shared(id)?;
    let mut labels: Vec<String> = node
        .labels()
        .iter()
        .map(|&l| g.strings.name(l).to_string())
        .collect();
    labels.sort();
    labels.dedup();
    let mut props: Vec<(String, Value)> = node
        .props()
        .iter()
        .map(|(k, v)| (g.strings.name(*k).to_string(), v.clone()))
        .collect();
    sort_props(&mut props);
    Some(NodeText { labels, props })
}

/// `{k=v, ...}` or nothing.
pub fn edge_props(props: &[(String, Value)], out: &mut String) {
    if props.is_empty() {
        return;
    }
    out.push('{');
    for (i, (k, v)) in props.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        name(k, out);
        out.push('=');
        value(v, out);
    }
    out.push('}');
}

/// `handle:Label:Other k=v ...` and a newline; the identity property the
/// handle carries is left off.
pub fn card(names: &Names, id: u64, n: &NodeText, out: &mut String) {
    let handle = names.handle_of(id, &n.props);
    let skip = names.identity_key(&n.props).map(str::to_string);
    out.push_str(&handle);
    for l in &n.labels {
        out.push(':');
        name(l, out);
    }
    for (k, v) in &n.props {
        if Some(k) == skip.as_ref() {
            continue;
        }
        out.push(' ');
        name(k, out);
        out.push('=');
        value(v, out);
    }
    out.push('\n');
}

/// One outgoing edge as an edge line needs it.
pub struct EdgeText {
    pub etype: String,
    pub to: String,
    pub props: Vec<(String, Value)>,
}

/// The outgoing edges of `id`, minus those in `omit`, as lines: by type;
/// for each type, the property-less edges folded onto one line (targets in
/// handle order), then one line per edge with properties.
pub fn edge_lines(from: &str, mut edges: Vec<EdgeText>, out: &mut String) {
    edges.sort_by(|a, b| {
        (&a.etype, !a.props.is_empty(), &a.to)
            .cmp(&(&b.etype, !b.props.is_empty(), &b.to))
            .then_with(|| {
                let (mut x, mut y) = (String::new(), String::new());
                edge_props(&a.props, &mut x);
                edge_props(&b.props, &mut y);
                x.cmp(&y)
            })
    });
    let mut i = 0;
    while i < edges.len() {
        let e = &edges[i];
        if e.props.is_empty() {
            let mut j = i;
            out.push_str(from);
            out.push_str(" -");
            name(&e.etype, out);
            out.push_str("-> ");
            while j < edges.len() && edges[j].etype == e.etype && edges[j].props.is_empty() {
                if j > i {
                    out.push_str(", ");
                }
                out.push_str(&edges[j].to);
                j += 1;
            }
            out.push('\n');
            i = j;
        } else {
            out.push_str(from);
            out.push_str(" -");
            name(&e.etype, out);
            edge_props(&e.props, out);
            out.push_str("-> ");
            out.push_str(&e.to);
            out.push('\n');
            i += 1;
        }
    }
}

/// The outgoing edges of a node, read for writing, without those in
/// `omit`.
pub fn out_edges(g: &Graph, names: &Names, id: u64, omit: &dyn Fn(u64) -> bool) -> Vec<EdgeText> {
    let mut adj = Vec::new();
    g.for_each_adj(id, crate::graph::Dir::Out, None, |a| adj.push(a));
    let mut out = Vec::with_capacity(adj.len());
    for a in adj {
        if omit(a.edge) {
            continue;
        }
        let mut props = g.edge_props(a.edge);
        sort_props(&mut props);
        out.push(EdgeText {
            etype: g.strings.name(a.etype).to_string(),
            to: names.handle(g, a.other),
            props,
        });
    }
    out
}

/// A node's plain block: its card and its outgoing edges.
pub fn plain_block(
    g: &Graph,
    names: &Names,
    id: u64,
    omit: &dyn Fn(u64) -> bool,
    out: &mut String,
) {
    let Some(n) = node_text(g, id) else { return };
    card(names, id, &n, out);
    let from = names.handle_of(id, &n.props);
    edge_lines(&from, out_edges(g, names, id, omit), out);
}

/// Ids in order, consecutive runs written `a..b`: `n242..281,290`.
pub fn id_list(ids: &[u64], out: &mut String) {
    out.push('n');
    let mut i = 0;
    let mut first = true;
    while i < ids.len() {
        let mut j = i;
        while j + 1 < ids.len() && ids[j + 1] == ids[j] + 1 {
            j += 1;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&ids[i].to_string());
        if j > i {
            out.push_str("..");
            out.push_str(&ids[j].to_string());
        }
        i = j + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_written_so_they_read_back_as_they_were() {
        let mut s = String::new();
        for v in [
            Value::Int(1),
            Value::Float(1.0),
            Value::Float(1e300),
            Value::Float(f64::NAN),
            Value::Float(-0.5),
            Value::Text("a \"b\"\n".into()),
            Value::List(vec![Value::Int(1), Value::List(vec![]), Value::Null]),
        ] {
            value(&v, &mut s);
            s.push(' ');
        }
        assert_eq!(s, "1 1.0 1e300 NaN -0.5 \"a \\\"b\\\"\\n\" [1, [], null] ");
    }

    #[test]
    fn names_that_are_not_words_are_quoted() {
        let mut s = String::new();
        for n in ["Station", "HAS_SIDE", "two words", "1st", "a`b"] {
            name(n, &mut s);
            s.push(' ');
        }
        assert_eq!(s, "Station HAS_SIDE `two words` `1st` `a``b` ");
    }

    #[test]
    fn id_lists_fold_runs() {
        let mut s = String::new();
        id_list(&[242, 243, 244, 250, 252, 253], &mut s);
        assert_eq!(s, "n242..244,250,252..253");
    }
}
