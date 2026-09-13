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
//!   GET  /api/schema  -> {labels:[{name,count}], edge_types:[...], indexes:[...]}
//!   GET  /api/expand  -> {graph:{nodes,edges}}   neighbours of one node

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
    out.push_str("],\"props\":{");
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

    Ok(format!(
        "{{\"labels\":[{}],\"edge_types\":[{}],\"indexes\":[{}]}}",
        labels, etypes, indexes
    ))
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

    for a in g.neighbors(id, Dir::Both, None).into_iter().take(limit) {
        proj.edges.insert(a.edge);
        proj.nodes.insert(a.other);
    }

    let mut out = String::from("{");
    proj.write(g, &mut out);
    out.push('}');
    Ok(out)
}

/// Parse `?id=..&limit=..` off a request path.
pub fn query_param(path: &str, key: &str) -> Option<String> {
    let qs = path.split_once('?')?.1;
    for pair in qs.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
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
        assert!(!j.contains("\\\"labels\\\""), "entities were escaped as text: {}", j);
    }

    #[test]
    fn graph_payload_is_deduplicated() {
        let mut g = fixture();
        // Ada appears in both rows; she must still be one node in the payload.
        query::execute(&mut g, r#"MATCH (a:Person {name:"Ada"}) CREATE (a)-[:KNOWS]->(:Person {name:"Cai"})"#).unwrap();
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
            &format!(r#"CREATE (:Decoy {{trap:"{}"}})"#, decoy.replace('"', "\\\"")),
        )
        .unwrap();

        let j = query_json(&mut g, "MATCH (d:Decoy) RETURN d.trap").unwrap();
        let graph = j.split("\"graph\":").nth(1).unwrap();
        assert_eq!(graph.matches("\"_e\":\"node\"").count(), 0, "decoy was drawn: {}", graph);
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
        assert_eq!(query_param("/api/expand?id=7&limit=3", "id").as_deref(), Some("7"));
        assert_eq!(query_param("/api/expand?id=7&limit=3", "limit").as_deref(), Some("3"));
        assert_eq!(query_param("/api/expand", "id"), None);
        assert_eq!(query_param("/api/expand?id=7", "missing"), None);
    }

    #[test]
    fn a_syntax_error_is_an_error_not_a_panic() {
        let mut g = fixture();
        assert!(query_json(&mut g, "MATCH ((((").is_err());
    }
}
