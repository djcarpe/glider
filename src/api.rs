//! Typed JSON API behind the browser console.
//!
//! The older `/query` endpoint hands back whatever `QueryResult` holds, and
//! `Value` has no entity variant — `node_value` renders a node to
//! `Value::Text` containing JSON. That is fine for a table but useless for
//! drawing: a text property whose content happens to look like an object is
//! indistinguishable from a real node, so a client that parses cells
//! optimistically will draw garbage.
//!
//! So the entity check here is exact rather than heuristic. A cell is only
//! treated as a node or relationship when it parses out an id AND the canonical
//! re-serialisation of that live entity is byte-identical to the cell. A string
//! property can only pass that test by being a faithful rendering of a real
//! entity, in which case drawing it is the right answer anyway.
//!
//!   POST /api/query   -> {columns, rows, graph:{nodes,edges}, ms, message, touched}
//!   GET  /api/schema  -> {nodes, edges, labels:[{name,count}], edge_types:[...], indexes:[...]}
//!   GET  /api/expand  -> {graph:{nodes,edges}}   neighbours of one node
//!   GET  /api/nodes   -> {nodes:[..], next, total}          a page of nodes
//!   GET  /api/edges   -> {edges:[..], nodes:[..], next, total}  a page of edges
//!
//! The two page endpoints back the explorer. They are cursor-paged by id
//! rather than offset-paged, so walking a million-node graph fifty at a time
//! costs O(page) per request instead of O(offset), and a node created or
//! deleted between pages shifts nothing.

use std::collections::BTreeSet;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use crate::graph::{Dir, Graph};
use crate::query::{self, QueryResult};
use crate::value::{write_json_string, Value};

// --------------------------------------------------------------- entities

/// Write one node as `{"_e":"node","id":..,"labels":[..],"props":{..}}`.
fn write_node(g: &Graph, id: u64, out: &mut String) {
    out.push_str("{\"_e\":\"node\",\"id\":");
    out.push_str(&id.to_string());
    out.push_str(",\"labels\":[");
    for (i, l) in g.node_labels(id).iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(l, out);
    }
    out.push_str("],\"degree\":");
    out.push_str(&g.degree(id, Dir::Both).to_string());
    out.push_str(",\"props\":{");
    for (i, (k, v)) in g.node_props(id).iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(k, out);
        out.push(':');
        v.write_json(out);
    }
    out.push_str("}}");
}

/// Write one relationship as
/// `{"_e":"rel","id":..,"type":"..","from":..,"to":..,"props":{..}}`.
fn write_edge(g: &Graph, id: u64, out: &mut String) {
    let Some(e) = g.edge(id) else {
        out.push_str("null");
        return;
    };
    out.push_str("{\"_e\":\"rel\",\"id\":");
    out.push_str(&id.to_string());
    out.push_str(",\"type\":");
    write_json_string(g.edge_type_name(id).unwrap_or(""), out);
    out.push_str(",\"from\":");
    out.push_str(&e.from.to_string());
    out.push_str(",\"to\":");
    out.push_str(&e.to.to_string());
    out.push_str(",\"props\":{");
    for (i, (k, v)) in g.edge_props(id).iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(k, out);
        out.push(':');
        v.write_json(out);
    }
    out.push_str("}}");
}

/// What a result cell turned out to be.
///
/// Public so that language bindings (see the Elixir NIF) can classify cells
/// the same way the HTTP API does, rather than each re-deriving the rule and
/// getting it subtly wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entity {
    Node(u64),
    Edge(u64),
}

/// Leading `{"id":<digits>` of an entity rendering, if present.
fn leading_id(s: &str) -> Option<u64> {
    let rest = s.strip_prefix("{\"id\":")?;
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

/// Decide whether a cell is a live entity. See the module note: the test is
/// byte-equality against the canonical rendering, not a shape guess.
pub fn classify(g: &Graph, v: &Value) -> Option<Entity> {
    entity_of(g, v)
}

fn entity_of(g: &Graph, v: &Value) -> Option<Entity> {
    let Value::Text(s) = v else { return None };
    if !s.starts_with("{\"id\":") {
        return None;
    }
    let id = leading_id(s)?;

    if s.contains("\"labels\":") && g.node(id).is_some() {
        if let Value::Text(canon) = query::node_value(g, id) {
            if canon == *s {
                return Some(Entity::Node(id));
            }
        }
    }
    if s.contains("\"from\":") && g.edge(id).is_some() {
        if let Value::Text(canon) = query::edge_value(g, id) {
            if canon == *s {
                return Some(Entity::Edge(id));
            }
        }
    }
    None
}

// ------------------------------------------------------------ graph payload

/// Accumulates the deduplicated node/edge set that the graph view draws.
#[derive(Default)]
struct Projection {
    nodes: BTreeSet<u64>,
    edges: BTreeSet<u64>,
}

impl Projection {
    fn add(&mut self, e: Entity) {
        match e {
            Entity::Node(id) => {
                self.nodes.insert(id);
            }
            Entity::Edge(id) => {
                self.edges.insert(id);
            }
        }
    }

    /// An edge whose endpoints are absent cannot be drawn, so pull them in.
    /// This is why `MATCH ()-[r]->() RETURN r` still renders as a graph.
    fn close_over_endpoints(&mut self, g: &Graph) {
        let ids: Vec<u64> = self.edges.iter().copied().collect();
        for eid in ids {
            if let Some(e) = g.edge(eid) {
                self.nodes.insert(e.from);
                self.nodes.insert(e.to);
            }
        }
    }

    fn write(&self, g: &Graph, out: &mut String) {
        out.push_str("\"graph\":{\"nodes\":[");
        for (i, id) in self.nodes.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_node(g, *id, out);
        }
        out.push_str("],\"edges\":[");
        for (i, id) in self.edges.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_edge(g, *id, out);
        }
        out.push_str("]}");
    }
}

// ------------------------------------------------------------------- query

/// Start a timer, where the platform has one.
///
/// `wasm32-unknown-unknown` has no clock: `Instant::now()` traps there rather
/// than returning something useless. So under wasm we report 0 and leave
/// timing to the host, which has `performance.now()` and can wrap the call.
#[cfg(not(target_arch = "wasm32"))]
fn timer() -> Option<Instant> {
    Some(Instant::now())
}

#[cfg(target_arch = "wasm32")]
fn timer() -> Option<()> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn elapsed_ms(t: Option<Instant>) -> f64 {
    t.map(|s| s.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0)
}

#[cfg(target_arch = "wasm32")]
fn elapsed_ms(_t: Option<()>) -> f64 {
    0.0
}

/// Run a query and render the typed response.
pub fn query_json(g: &mut Graph, src: &str) -> Result<String, String> {
    let started = timer();
    let r: QueryResult = query::execute(g, src).map_err(|e| e.to_string())?;
    let ms = elapsed_ms(started);

    let mut proj = Projection::default();
    for row in &r.rows {
        for v in row {
            if let Some(e) = entity_of(g, v) {
                proj.add(e);
            }
        }
    }
    proj.close_over_endpoints(g);

    let mut out = String::with_capacity(4096);
    out.push_str("{\"columns\":[");
    for (i, c) in r.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(c, &mut out);
    }
    out.push_str("],\"rows\":[");
    for (i, row) in r.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, v) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            match entity_of(g, v) {
                Some(Entity::Node(id)) => write_node(g, id, &mut out),
                Some(Entity::Edge(id)) => write_edge(g, id, &mut out),
                None => v.write_json(&mut out),
            }
        }
        out.push(']');
    }
    out.push_str("],");
    proj.write(g, &mut out);
    if let Some(m) = &r.message {
        out.push_str(",\"message\":");
        write_json_string(m, &mut out);
    }
    out.push_str(&format!(",\"touched\":{},\"ms\":{:.4}", r.touched, ms));
    // The engine's report on the statement (same thread, just recorded):
    // operation, pages read and hit. See docs/OBSERVABILITY.md.
    if let Some(op) = crate::telemetry::last_op() {
        out.push_str(",\"op\":");
        out.push_str(&op.to_json());
    }
    out.push('}');
    Ok(out)
}

// ------------------------------------------------------------------ schema

/// Labels, relationship types and indexes with counts, for the sidebar.
/// Reuses the `SCHEMA` statement so there is one definition of what the schema
/// is, rather than a second one that drifts.
pub fn schema_json(g: &mut Graph) -> Result<String, String> {
    let r = query::execute(g, "SCHEMA").map_err(|e| e.to_string())?;

    let mut labels = String::new();
    let mut etypes = String::new();
    let mut indexes = String::new();

    for row in &r.rows {
        let kind = match row.first() {
            Some(Value::Text(s)) => s.as_str(),
            _ => continue,
        };
        let name = match row.get(1) {
            Some(Value::Text(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => continue,
        };
        let count = match row.get(2) {
            Some(Value::Int(n)) => *n,
            _ => 0,
        };

        let bucket = match kind {
            "label" => &mut labels,
            "edge_type" => &mut etypes,
            _ => &mut indexes,
        };
        if !bucket.is_empty() {
            bucket.push(',');
        }
        bucket.push_str("{\"name\":");
        write_json_string(&name, bucket);
        bucket.push_str(&format!(",\"count\":{}}}", count));
    }

    // Property keys by label and by relationship type, sampled, so the
    // editor can complete `n.` without a query of its own.
    let (nk, ek) = g.sample_keys(KEY_SAMPLE);

    // Label counts overlap (a node may carry several), so the explorer needs
    // the true totals too.
    Ok(format!(
        "{{\"nodes\":{},\"edges\":{},\"labels\":[{}],\"edge_types\":[{}],\"indexes\":[{}],\"node_keys\":{},\"edge_keys\":{}}}",
        g.node_count(),
        g.edge_count(),
        labels,
        etypes,
        indexes,
        keys_json(&nk),
        keys_json(&ek)
    ))
}

/// How many members of each label or type `schema_json` inspects for keys.
const KEY_SAMPLE: usize = 200;

/// `{"Person":["age","name"],...}`
fn keys_json(groups: &[(String, Vec<String>)]) -> String {
    let mut out = String::from("{");
    for (i, (name, keys)) in groups.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(name, &mut out);
        out.push_str(":[");
        for (j, k) in keys.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            write_json_string(k, &mut out);
        }
        out.push(']');
    }
    out.push('}');
    out
}

// ------------------------------------------------------------------ expand

/// Neighbours of one node, in both directions, capped. Backs double-click to
/// expand in the graph view.
pub fn expand_json(g: &Graph, id: u64, limit: usize) -> Result<String, String> {
    if g.node(id).is_none() {
        return Err(format!("no node with id {}", id));
    }
    let mut proj = Projection::default();
    proj.nodes.insert(id);

    for a in g.neighbors_limited(id, Dir::Both, limit) {
        proj.edges.insert(a.edge);
        proj.nodes.insert(a.other);
    }

    let mut out = String::from("{");
    proj.write(g, &mut out);
    out.push('}');
    Ok(out)
}

// ----------------------------------------------------------------- explore

/// Case-insensitive substring test. `needle` is already lowercased. The ASCII
/// path avoids allocating a lowercased copy of every property value in the
/// graph on each keystroke, which is what a scan of a large graph would
/// otherwise spend most of its time doing.
fn contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.is_ascii() && needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
    } else {
        hay.to_lowercase().contains(needle)
    }
}

fn value_matches(v: &Value, needle: &str) -> bool {
    match v {
        Value::Null => false,
        Value::Text(s) => contains_ci(s, needle),
        Value::List(items) => items.iter().any(|i| value_matches(i, needle)),
        other => contains_ci(&other.to_string(), needle),
    }
}

/// Does a node match free text? By id exactly, or by any label or property
/// value containing it.
fn node_matches(g: &Graph, id: u64, needle: &str) -> bool {
    let Some(n) = g.node(id) else { return false };
    if id.to_string() == needle {
        return true;
    }
    if n.labels()
        .iter()
        .any(|l| contains_ci(g.strings.name(*l), needle))
    {
        return true;
    }
    n.props().iter().any(|(_, v)| value_matches(v, needle))
}

fn edge_matches(g: &Graph, id: u64, needle: &str) -> bool {
    let Some(e) = g.edge(id) else { return false };
    if id.to_string() == needle {
        return true;
    }
    if contains_ci(g.strings.name(e.etype), needle) {
        return true;
    }
    e.props().iter().any(|(_, v)| value_matches(v, needle))
}

fn needle_of(q: Option<&str>) -> Option<String> {
    q.map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty())
}

/// Walk sorted `ids` from the cursor, keeping those that pass `keep`, and
/// return (page, next cursor). The cursor is the id to start *from*, so page
/// one is `from = 0` and `next` is simply the first id not yet delivered.
/// Most candidates one page request examines while filtering. A rare
/// search term therefore returns a short page and a cursor to continue from,
/// instead of scanning the whole graph in one request.
const SCAN_BUDGET: usize = 200_000;

/// Walk ids from `from` upwards through `fetch` (which returns up to n ids
/// >= a cursor, ascending), keeping those `keep` accepts, until `limit` are
/// found. Returns them and the cursor to continue from, if any.
fn page<F: Fn(u64) -> bool>(
    fetch: &dyn Fn(u64, usize) -> Vec<u64>,
    from: u64,
    limit: usize,
    keep: F,
) -> (Vec<u64>, Option<u64>) {
    let mut out = Vec::with_capacity(limit.min(1024));
    let mut cursor = from;
    let mut scanned = 0usize;
    loop {
        let batch = fetch(cursor, 1024);
        let n = batch.len();
        for id in batch {
            scanned += 1;
            cursor = id + 1;
            if keep(id) {
                // The cursor is the next match, so the next page starts on it.
                if out.len() == limit {
                    return (out, Some(id));
                }
                out.push(id);
            }
            if scanned >= SCAN_BUDGET {
                return (out, Some(cursor));
            }
        }
        if n < 1024 {
            return (out, None);
        }
    }
}

/// A page of nodes: `{"nodes":[..],"next":<id>|null,"total":N}`.
///
/// `total` counts the candidates *before* the text filter — the label's size,
/// or the whole graph — because counting matches would mean scanning
/// everything on each request, which is exactly what paging is avoiding.
pub fn nodes_json(
    g: &Graph,
    label: Option<&str>,
    q: Option<&str>,
    from: u64,
    limit: usize,
) -> String {
    let label = label.filter(|l| !l.is_empty());
    let (fetch, total): (Box<dyn Fn(u64, usize) -> Vec<u64>>, usize) = match label {
        Some(l) => match g.strings.lookup(l) {
            Some(lid) => (Box::new(move |c, n| g.label_members_from(lid, c, n)), g.label_count(l)),
            None => (Box::new(|_, _| Vec::new()), 0),
        },
        None => (Box::new(|c, n| g.nodes_from(c, n)), g.node_count()),
    };
    let needle = needle_of(q);
    let (hits, next) = page(&*fetch, from, limit, |id| match &needle {
        Some(n) => node_matches(g, id, n),
        None => true,
    });

    let mut out = String::with_capacity(hits.len() * 160 + 64);
    out.push_str("{\"nodes\":[");
    for (i, id) in hits.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_node(g, *id, &mut out);
    }
    out.push_str("],");
    write_page_tail(next, total, &mut out);
    out
}

/// A page of edges: `{"edges":[..],"nodes":[..],"next":<id>|null,"total":N}`.
/// Endpoints ride along so a row can read "Ada —KNOWS→ Bob" without a second
/// request per edge.
pub fn edges_json(
    g: &Graph,
    etype: Option<&str>,
    q: Option<&str>,
    from: u64,
    limit: usize,
) -> String {
    let etype = etype.filter(|t| !t.is_empty());
    let (fetch, total): (Box<dyn Fn(u64, usize) -> Vec<u64>>, usize) = match etype {
        Some(t) => match g.strings.lookup(t) {
            Some(tid) => (Box::new(move |c, n| g.type_members_from(tid, c, n)), g.type_count(t)),
            None => (Box::new(|_, _| Vec::new()), 0),
        },
        None => (Box::new(|c, n| g.edges_from(c, n)), g.edge_count()),
    };
    let needle = needle_of(q);
    let (hits, next) = page(&*fetch, from, limit, |id| match &needle {
        Some(n) => edge_matches(g, id, n),
        None => true,
    });

    let mut endpoints = BTreeSet::new();
    let mut out = String::with_capacity(hits.len() * 260 + 64);
    out.push_str("{\"edges\":[");
    for (i, id) in hits.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_edge(g, *id, &mut out);
        if let Some(e) = g.edge(*id) {
            endpoints.insert(e.from);
            endpoints.insert(e.to);
        }
    }
    out.push_str("],\"nodes\":[");
    for (i, id) in endpoints.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_node(g, *id, &mut out);
    }
    out.push_str("],");
    write_page_tail(next, total, &mut out);
    out
}

fn write_page_tail(next: Option<u64>, total: usize, out: &mut String) {
    out.push_str("\"next\":");
    match next {
        Some(id) => out.push_str(&id.to_string()),
        None => out.push_str("null"),
    }
    out.push_str(&format!(",\"total\":{}}}", total));
}

/// Parse `?id=..&limit=..` off a request path. Values are percent-decoded,
/// with `+` as space, so a search for "New York" or "a&b" arrives intact.
pub fn query_param(path: &str, key: &str) -> Option<String> {
    let qs = path.split_once('?')?.1;
    for pair in qs.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;

    fn fixture() -> Graph {
        let mut g = Graph::memory();
        query::execute(
            &mut g,
            r#"CREATE (a:Person {name:"Ada"})-[:KNOWS {since:2019}]->(b:Person {name:"Bob"})"#,
        )
        .expect("create");
        g
    }

    #[test]
    fn entities_come_back_typed_not_as_json_text() {
        let mut g = fixture();
        let j = query_json(&mut g, "MATCH (a)-[r]->(b) RETURN a, r, b").unwrap();
        // Typed markers, and the props are nested objects rather than escaped
        // strings — this is the whole reason /api/query exists.
        assert!(j.contains("\"_e\":\"node\""), "no node marker in {}", j);
        assert!(j.contains("\"_e\":\"rel\""), "no rel marker in {}", j);
        assert!(j.contains("\"since\":2019"));
        assert!(
            !j.contains("\\\"labels\\\""),
            "entities were escaped as text: {}",
            j
        );
    }

    #[test]
    fn graph_payload_is_deduplicated() {
        let mut g = fixture();
        // Ada appears in both rows; she must still be one node in the payload.
        query::execute(
            &mut g,
            r#"MATCH (a:Person {name:"Ada"}) CREATE (a)-[:KNOWS]->(:Person {name:"Cai"})"#,
        )
        .unwrap();
        let j = query_json(&mut g, "MATCH (a)-[r]->(b) RETURN a, r, b").unwrap();
        let graph = j.split("\"graph\":").nth(1).unwrap();
        assert_eq!(graph.matches("\"_e\":\"node\"").count(), 3, "{}", graph);
        assert_eq!(graph.matches("\"_e\":\"rel\"").count(), 2, "{}", graph);
    }

    #[test]
    fn returning_only_a_relationship_still_draws() {
        let mut g = fixture();
        let j = query_json(&mut g, "MATCH ()-[r]->() RETURN r").unwrap();
        let graph = j.split("\"graph\":").nth(1).unwrap();
        // Endpoints are pulled in even though the query never returned them.
        assert_eq!(graph.matches("\"_e\":\"node\"").count(), 2, "{}", graph);
    }

    #[test]
    fn a_text_property_that_mimics_a_node_is_not_drawn() {
        // The failure this design exists to prevent: Value has no entity
        // variant, so a naive client cannot tell a node from a string that
        // looks like one. Byte-equality against the live entity rejects it.
        let mut g = fixture();
        let decoy = r#"{"id":0,"labels":["Person"],"props":{"name":"Mallory"}}"#;
        query::execute(
            &mut g,
            &format!(
                r#"CREATE (:Decoy {{trap:"{}"}})"#,
                decoy.replace('"', "\\\"")
            ),
        )
        .unwrap();

        let j = query_json(&mut g, "MATCH (d:Decoy) RETURN d.trap").unwrap();
        let graph = j.split("\"graph\":").nth(1).unwrap();
        assert_eq!(
            graph.matches("\"_e\":\"node\"").count(),
            0,
            "decoy was drawn: {}",
            graph
        );
    }

    #[test]
    fn a_genuine_entity_rendering_is_accepted() {
        // The mirror of the test above: the exact canonical rendering of a real
        // node must still be recognised, or the check is simply refusing
        // everything.
        let g = fixture();
        let id = g.node_ids()[0];
        let v = query::node_value(&g, id);
        assert!(entity_of(&g, &v).is_some(), "canonical node was rejected");
        let e = query::edge_value(&g, g.edge_ids()[0]);
        assert!(entity_of(&g, &e).is_some(), "canonical edge was rejected");
    }

    #[test]
    fn schema_reports_labels_and_types_with_counts() {
        let mut g = fixture();
        let j = schema_json(&mut g).unwrap();
        assert!(j.contains("\"name\":\"Person\""), "{}", j);
        assert!(j.contains("\"name\":\"KNOWS\""), "{}", j);
        assert!(j.contains("\"count\":2"), "{}", j);
        assert!(j.contains("\"nodes\":2,\"edges\":1"), "{}", j);
        assert!(j.contains("\"node_keys\":{\"\":[\"name\"]"), "{}", j);
    }

    #[test]
    fn expand_returns_neighbours_and_rejects_unknown_ids() {
        let g = fixture();
        let id = g.node_ids()[0];
        let j = expand_json(&g, id, 10).unwrap();
        assert_eq!(j.matches("\"_e\":\"node\"").count(), 2, "{}", j);
        assert_eq!(j.matches("\"_e\":\"rel\"").count(), 1, "{}", j);
        assert!(expand_json(&g, 9_999_999, 10).is_err());
    }

    #[test]
    fn query_params_parse() {
        assert_eq!(
            query_param("/api/expand?id=7&limit=3", "id").as_deref(),
            Some("7")
        );
        assert_eq!(
            query_param("/api/expand?id=7&limit=3", "limit").as_deref(),
            Some("3")
        );
        assert_eq!(query_param("/api/expand", "id"), None);
        assert_eq!(query_param("/api/expand?id=7", "missing"), None);
    }

    #[test]
    fn query_params_are_percent_decoded() {
        assert_eq!(
            query_param("/api/nodes?q=New+York&label=City", "q").as_deref(),
            Some("New York")
        );
        assert_eq!(
            query_param("/api/nodes?q=a%26b%3Dc", "q").as_deref(),
            Some("a&b=c")
        );
        // Malformed escapes are kept literally rather than rejected.
        assert_eq!(
            query_param("/api/nodes?q=100%", "q").as_deref(),
            Some("100%")
        );
        assert_eq!(query_param("/api/nodes?q=%zz", "q").as_deref(), Some("%zz"));
    }

    fn explore_fixture() -> Graph {
        let mut g = Graph::memory();
        for i in 0..7 {
            query::execute(
                &mut g,
                &format!(
                    r#"CREATE (:Person {{name:"P{}", city:"{}"}})"#,
                    i,
                    if i % 2 == 0 { "London" } else { "Paris" }
                ),
            )
            .unwrap();
        }
        query::execute(&mut g, r#"CREATE (:City {name:"London", pop:8900000})"#).unwrap();
        query::execute(
            &mut g,
            r#"MATCH (a:Person {name:"P0"}), (b:Person {name:"P1"}) CREATE (a)-[:KNOWS {since:2019}]->(b)"#,
        )
        .unwrap();
        query::execute(
            &mut g,
            r#"MATCH (a:Person {name:"P1"}), (c:City) CREATE (a)-[:LIVES_IN]->(c)"#,
        )
        .unwrap();
        g
    }

    fn ids_in(json: &str, key: &str) -> Vec<u64> {
        // Pull `"id":N` out of the entities in one top-level array of the
        // payload. The array ends where the next top-level key begins.
        let arr = json.split(&format!("\"{}\":[", key)).nth(1).unwrap();
        let end = ["],\"nodes\":[", "],\"next\":"]
            .iter()
            .filter_map(|k| arr.find(k))
            .min()
            .unwrap_or(arr.len());
        arr[..end]
            .split("\"id\":")
            .skip(1)
            .map(|s| {
                s.split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn nodes_page_by_cursor_and_report_the_next_one() {
        let g = explore_fixture();
        let p1 = nodes_json(&g, None, None, 0, 3);
        assert_eq!(ids_in(&p1, "nodes"), vec![1, 2, 3], "{}", p1);
        assert!(p1.contains("\"next\":4"), "{}", p1);
        assert!(p1.contains("\"total\":8"), "{}", p1);

        let p2 = nodes_json(&g, None, None, 4, 3);
        assert_eq!(ids_in(&p2, "nodes"), vec![4, 5, 6]);
        let p3 = nodes_json(&g, None, None, 7, 3);
        assert_eq!(ids_in(&p3, "nodes"), vec![7, 8]);
        assert!(p3.contains("\"next\":null"), "{}", p3);
    }

    #[test]
    fn nodes_filter_by_label_and_text() {
        let g = explore_fixture();
        let j = nodes_json(&g, Some("City"), None, 0, 10);
        assert_eq!(ids_in(&j, "nodes"), vec![8]);
        assert!(j.contains("\"total\":1"));

        // Text is case-insensitive and reaches into property values...
        let j = nodes_json(&g, None, Some("paris"), 0, 10);
        assert_eq!(ids_in(&j, "nodes"), vec![2, 4, 6]);
        // ...labels...
        let j = nodes_json(&g, None, Some("cit"), 0, 10);
        assert_eq!(ids_in(&j, "nodes"), vec![8]);
        // ...numbers, and the id itself.
        let j = nodes_json(&g, None, Some("8900000"), 0, 10);
        assert_eq!(ids_in(&j, "nodes"), vec![8]);
        // Node 3 matches by id; node 4 is "P3" and matches by name.
        let j = nodes_json(&g, None, Some("3"), 0, 10);
        assert_eq!(ids_in(&j, "nodes"), vec![3, 4]);

        // Filtered pages still cursor correctly: the cursor is the next match.
        let j = nodes_json(&g, Some("Person"), Some("london"), 0, 2);
        assert_eq!(ids_in(&j, "nodes"), vec![1, 3]);
        assert!(j.contains("\"next\":5"), "{}", j);
        assert!(j.contains("\"degree\":1"), "degree missing: {}", j);
    }

    #[test]
    fn edges_page_with_their_endpoints() {
        let g = explore_fixture();
        let j = edges_json(&g, None, None, 0, 10);
        assert_eq!(ids_in(&j, "edges"), vec![1, 2]);
        assert_eq!(ids_in(&j, "nodes"), vec![1, 2, 8], "{}", j);
        assert!(j.contains("\"next\":null"));

        let j = edges_json(&g, Some("LIVES_IN"), None, 0, 10);
        assert_eq!(ids_in(&j, "edges"), vec![2]);
        let j = edges_json(&g, None, Some("2019"), 0, 10);
        assert_eq!(ids_in(&j, "edges"), vec![1]);
        let j = edges_json(&g, None, Some("knows"), 0, 10);
        assert_eq!(ids_in(&j, "edges"), vec![1]);
        let j = edges_json(&g, None, None, 2, 1);
        assert_eq!(ids_in(&j, "edges"), vec![2]);
    }

    #[test]
    fn a_syntax_error_is_an_error_not_a_panic() {
        let mut g = fixture();
        assert!(query_json(&mut g, "MATCH ((((").is_err());
    }
}
