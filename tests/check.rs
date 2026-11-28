//! `check.run`: finds each kind of fault in an otherwise sound database,
//! names the record at fault, says nothing about a sound database of any
//! shape the engine builds, changes nothing, and gives the same answer in
//! one call or in slices, on both tiers.
//!
//! The graph API cannot write a fault, so the tests plant them below it:
//! the database file is opened with the storage layer (`storage::db::Db`)
//! and one tree entry or counter is written there, as a bug in the engine
//! or a foreign writer would leave it; a bulk load from a source that
//! breaks its contract (an edge to a node it never lists); or, for the
//! text index, `Graph::ext_write` into the `context` keyspace and a write
//! path that does not call the extension hooks (`Graph::apply_op`).

use std::path::{Path, PathBuf};

use glider::graph::{Graph, OpenOptions};
use glider::query::{self, QueryResult};
use glider::storage::btree::Tree;
use glider::storage::db::{Db, DbConfig};
use glider::storage::pager::Pager;
use glider::storage::SError;
use glider::store::Op;
use glider::value::Value;

// ------------------------------------------------------------------ helpers

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-check-{}-{}.db", name, std::process::id()));
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

fn text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// A row as (check, severity, kind, id, detail).
type Row = (String, String, String, String, String);

fn rows(r: &QueryResult) -> Vec<Row> {
    assert_eq!(r.columns, ["check", "severity", "kind", "id", "detail"]);
    r.rows
        .iter()
        .map(|r| {
            (
                text(&r[0]),
                text(&r[1]),
                text(&r[2]),
                text(&r[3]),
                text(&r[4]),
            )
        })
        .collect()
}

/// `CALL check.run(<args>)`, installing the extension first.
fn check(g: &mut Graph, args: &str) -> Vec<Row> {
    q(g, "CREATE EXTENSION IF NOT EXISTS check");
    rows(&q(g, &format!("CALL check.run({args})")))
}

/// The rows that report something (not summaries).
fn found(rows: &[Row]) -> Vec<Row> {
    rows.iter().filter(|r| r.1 != "summary").cloned().collect()
}

/// (records, problems) of a summary row's detail.
fn counts(detail: &str) -> (u64, u64) {
    let num = |k: &str| -> u64 {
        let at = detail.find(&format!("\"{k}\":")).unwrap() + k.len() + 3;
        detail[at..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap()
    };
    (num("records"), num("problems"))
}

fn summaries(rows: &[Row]) -> Vec<(String, String, u64, u64)> {
    rows.iter()
        .filter(|r| r.1 == "summary")
        .map(|r| {
            let (a, b) = counts(&r.4);
            (r.0.clone(), r.2.clone(), a, b)
        })
        .collect()
}

fn assert_sound(rows: &[Row], what: &str) {
    let bad = found(rows);
    assert!(bad.is_empty(), "{what}: expected silence, got {bad:#?}");
    for (c, kind, _, p) in summaries(rows) {
        assert_eq!((kind.as_str(), p), ("done", 0), "{what}: {c}");
    }
}

/// The faults of one check: (kind, id).
fn faults_of(rows: &[Row], check: &str) -> Vec<(String, String)> {
    rows.iter()
        .filter(|r| r.0 == check && r.1 != "summary")
        .map(|r| (r.2.clone(), r.3.clone()))
        .collect()
}

/// A small graph with labels, types, properties, an index and a hub.
fn people(g: &mut Graph) {
    q(g, "INDEX ON :Person(name)");
    for i in 0..30 {
        q(
            g,
            &format!("CREATE (:Person {{name: 'p{i}', age: {}}})", i % 7),
        );
    }
    q(g, "CREATE (:City {name: 'Here'})");
    for i in 0..29 {
        q(
            g,
            &format!(
                "MATCH (a:Person {{name: 'p{i}'}}), (b:Person {{name: 'p{}'}}) CREATE (a)-[:KNOWS {{since: {i}}}]->(b)",
                i + 1
            ),
        );
    }
    q(
        g,
        "MATCH (p:Person), (c:City) WHERE p.age = 3 CREATE (p)-[:LIVES_IN]->(c)",
    );
}

fn file_graph(path: &Path) -> Graph {
    Graph::open_opts(path, OpenOptions::default()).unwrap()
}

/// Close the database, write below the graph with `f`, and reopen it.
fn plant(path: &Path, g: Graph, f: impl FnOnce(&Pager)) -> Graph {
    drop(g);
    {
        let (mut db, _) = Db::open(path, &DbConfig::default(), &mut |_, _| {
            Err(SError::Corrupt(
                "a cleanly closed database has no log to replay".into(),
            ))
        })
        .unwrap();
        f(db.pager());
        db.pager().commit();
        db.checkpoint().unwrap();
    }
    file_graph(path)
}

// The trees and counters, as `src/graph.rs` lays them out.
const CATALOG: Tree = Tree { slot: 0, id: 1 };
const ADJ: Tree = Tree { slot: 3, id: 4 };
const LABELS: Tree = Tree { slot: 4, id: 5 };
const TYPES: Tree = Tree { slot: 5, id: 6 };
const PROPS: Tree = Tree { slot: 6, id: 7 };
const M_NEXT_NODE: usize = 0;
const M_NODES: usize = 2;

fn adj_key(node: u64, dir: u8, t: u32, edge: u64) -> Vec<u8> {
    let mut k = node.to_be_bytes().to_vec();
    k.push(dir);
    k.extend_from_slice(&t.to_be_bytes());
    k.extend_from_slice(&edge.to_be_bytes());
    k
}

fn pair_key(a: u32, b: u64) -> Vec<u8> {
    let mut k = a.to_be_bytes().to_vec();
    k.extend_from_slice(&b.to_be_bytes());
    k
}

fn varint(v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    glider::codec::put_varint(&mut out, v);
    out
}

/// The same check on the file and on the same bytes loaded into memory:
/// the rows must be the same (D7).
fn both_tiers(path: &Path, g: Graph, args: &str) -> (Graph, Vec<Row>) {
    let mut g = g;
    let file_rows = check(&mut g, args);
    g.checkpoint().unwrap();
    drop(g);
    let bytes = std::fs::read(path).unwrap();
    let mut m = Graph::from_bytes(&bytes).unwrap();
    let mem_rows = check(&mut m, args);
    assert_eq!(file_rows, mem_rows, "the tiers disagree");
    (file_graph(path), file_rows)
}

// ------------------------------------------------------- sound databases

/// Every way the engine's own tests shape a database: built by statements
/// and by the API, with deletes, relabels, self-loops, parallel edges, long
/// indexed values, indexes made before and after their data and dropped,
/// CLEAR and rebuilt, a small page size, a bulk load, an import and a text
/// index kept by its hooks.
fn shapes() -> Vec<(&'static str, Graph)> {
    let mut out = Vec::new();
    out.push(("empty", Graph::memory()));

    let mut g = Graph::memory();
    people(&mut g);
    out.push(("people", g));

    let mut g = Graph::memory_with_page_size(512, 1 << 30);
    people(&mut g);
    q(&mut g, "INDEX ON :Person(bio)");
    let long = "lorem ipsum dolor sit amet ".repeat(40);
    for i in 0..12 {
        // Long values that share a prefix, several of them equal: the
        // index cuts them to fit a key.
        q(
            &mut g,
            &format!(
                "MATCH (p:Person {{name: 'p{i}'}}) SET p.bio = '{long}{}'",
                i % 4
            ),
        );
    }
    q(&mut g, "MATCH (p:Person {name: 'p2'}) SET p.bio = 'short'");
    q(&mut g, "MATCH (p:Person {name: 'p3'}) REMOVE p.bio");
    q(&mut g, "MATCH (p:Person {name: 'p4'}) SET p:Former");
    q(&mut g, "MATCH (p:Former) REMOVE p:Person");
    q(&mut g, "MATCH (p:Former) SET p:Person");
    q(&mut g, "MATCH (p:Person {name: 'p5'}) DETACH DELETE p");
    q(&mut g, "MATCH ()-[r:KNOWS {since: 10}]->() DELETE r");
    q(
        &mut g,
        "MATCH (a:Person {name: 'p6'}) CREATE (a)-[:KNOWS {since: 1}]->(a), (a)-[:KNOWS]->(a), (a)-[:KNOWS]->(a)",
    );
    q(&mut g, "INDEX ON :City(name)");
    q(&mut g, "DROP INDEX ON :Person(name)");
    q(&mut g, "MATCH (p:Person) WHERE p.age = 1 SET p.age = 1.0");
    out.push(("edited, small pages", g));

    let mut g = Graph::memory();
    people(&mut g);
    q(&mut g, "CLEAR");
    people(&mut g);
    out.push(("cleared and rebuilt", g));

    let mut g = Graph::memory();
    let a = g.add_node(&["A".into()], vec![]).unwrap();
    let b = g
        .add_node(&["A".into(), "B".into()], vec![("x".into(), Value::Int(1))])
        .unwrap();
    g.add_edge(a, b, "R", vec![]).unwrap();
    let e = g.add_edge(b, a, "R", vec![]).unwrap();
    g.set_edge_prop(e, "w", Value::Float(0.5)).unwrap();
    g.unset_edge_prop(e, "w").unwrap();
    g.remove_label(b, "A").unwrap();
    g.delete_node(a).unwrap();
    g.create_index("B", "x").unwrap();
    out.push(("by the API", g));

    let mut g = Graph::memory();
    let jsonl = "{\"type\":\"node\",\"id\":5,\"labels\":[\"T\"],\"props\":{\"name\":\"five\"}}\n\
                 {\"type\":\"node\",\"id\":9,\"labels\":[\"T\"],\"props\":{\"name\":\"nine\"}}\n\
                 {\"type\":\"edge\",\"id\":3,\"label\":\"L\",\"from\":5,\"to\":9,\"props\":{}}\n";
    query::import_jsonl(&mut g, jsonl).unwrap();
    out.push(("imported", g));

    let mut g = Graph::memory();
    people(&mut g);
    q(&mut g, "CREATE EXTENSION context");
    q(&mut g, "CALL context.index(labels: ['Person', 'City'])");
    q(
        &mut g,
        "MATCH (p:Person {name: 'p1'}) SET p.name = 'renamed one', p.note = 'notes notes'",
    );
    q(&mut g, "MATCH (p:Person {name: 'p2'}) DETACH DELETE p");
    q(
        &mut g,
        "CREATE (:Person {name: 'newcomer'}), (:Thing {name: 'unindexed'})",
    );
    q(&mut g, "MATCH (p:Person {name: 'p7'}) REMOVE p:Person");
    out.push(("with a text index", g));
    out
}

#[test]
fn a_sound_database_of_every_shape_has_no_faults() {
    for (what, mut g) in shapes() {
        let r = check(&mut g, "");
        assert_sound(&r, what);
        let names: Vec<String> = summaries(&r).into_iter().map(|s| s.0).collect();
        assert_eq!(
            names,
            [
                "endpoints",
                "adjacency",
                "membership",
                "counters",
                "indexes",
                "derived"
            ],
            "{what}: with no checks named, the database's own run"
        );
    }
}

/// A bulk-load source: nodes and edges in id order.
struct Source {
    strings: Vec<String>,
    nodes: Vec<(u64, Vec<u32>, Vec<(u32, Value)>)>,
    edges: Vec<(u64, u64, u64, u32)>,
    indexes: Vec<(u32, u32)>,
}

impl glider::legacy::image::ImageSource for Source {
    fn strings(&self) -> Vec<String> {
        self.strings.clone()
    }
    fn next_ids(&self) -> (u64, u64) {
        (
            self.nodes.last().map_or(1, |n| n.0 + 1),
            self.edges.last().map_or(1, |e| e.0 + 1),
        )
    }
    fn indexes(&self) -> Vec<(u32, u32)> {
        self.indexes.clone()
    }
    fn nodes(&self, f: &mut dyn FnMut(u64, &[u32], &[(u32, Value)])) -> std::io::Result<()> {
        for (id, l, p) in &self.nodes {
            f(*id, l, p);
        }
        Ok(())
    }
    fn edges(&self, f: &mut dyn FnMut(u64, u64, u64, u32, &[(u32, Value)])) -> std::io::Result<()> {
        for &(id, a, b, t) in &self.edges {
            f(id, a, b, t, &[]);
        }
        Ok(())
    }
}

fn source(edges: Vec<(u64, u64, u64, u32)>) -> Source {
    Source {
        strings: vec!["N".into(), "name".into(), "R".into()],
        nodes: (1..=6)
            .map(|i| (i, vec![0], vec![(1, Value::Text(format!("n{}", i % 3)))]))
            .collect(),
        edges,
        indexes: vec![(0, 1)],
    }
}

#[test]
fn a_bulk_loaded_database_is_sound() {
    let path = temp("bulk-sound");
    let src = source(vec![(1, 1, 2, 2), (2, 2, 3, 2), (3, 3, 3, 2), (7, 6, 1, 2)]);
    let g = Graph::bulk_load(&path, &src, OpenOptions::default(), 1 << 20).unwrap();
    let (_g, r) = both_tiers(&path, g, "");
    assert_sound(&r, "bulk load");
    cleanup(&path);
}

// --------------------------------------------- each fault, found and named

#[test]
fn endpoints_finds_an_edge_to_a_node_that_does_not_exist() {
    // A bulk-load source that breaks its contract: edge 2 ends at node 99,
    // which it never lists. The load does not check; `check` does.
    let path = temp("endpoints");
    let src = source(vec![(1, 1, 2, 2), (2, 2, 99, 2), (3, 3, 4, 2)]);
    let g = Graph::bulk_load(&path, &src, OpenOptions::default(), 1 << 20).unwrap();
    let (_g, r) = both_tiers(&path, g, "");
    assert_eq!(
        found(&r),
        vec![(
            "endpoints".into(),
            "fault".into(),
            "missing_target".into(),
            "e2".into(),
            "edge e2 [:R] ends at n99, which does not exist".into()
        )]
    );
    assert_eq!(summaries(&r)[0], ("endpoints".into(), "done".into(), 3, 1));
    cleanup(&path);
}

/// A sound file database of `people`, closed; its ids of interest.
fn sound_file(name: &str) -> (PathBuf, Graph) {
    let path = temp(name);
    let mut g = file_graph(&path);
    people(&mut g);
    assert_sound(&check(&mut g, ""), "before planting");
    (path, g)
}

fn knows(g: &Graph) -> (u64, u64, u64, u32) {
    let t = g.strings.lookup("KNOWS").unwrap();
    let e = g.edges_with_type("KNOWS")[4];
    let r = g.edge(e).unwrap();
    (e, r.from, r.to, t)
}

#[test]
fn adjacency_finds_a_missing_entry_an_entry_without_its_edge_and_a_wrong_end() {
    let (path, g) = sound_file("adjacency");
    let (e, from, to, t) = knows(&g);
    let other = g.edges_with_type("KNOWS")[7];
    let o = g.edge(other).unwrap();
    let g = plant(&path, g, |p| {
        // Edge e is gone from its source's adjacency.
        ADJ.delete(p, &adj_key(from, 0, t, e)).unwrap();
        // An entry for an edge that was never written.
        ADJ.put(p, &adj_key(to, 0, t, 777), &varint(from)).unwrap();
        // Another edge's incoming entry names the wrong far end.
        ADJ.put(p, &adj_key(o.to, 1, t, other), &varint(to))
            .unwrap();
    });
    let (_g, r) = both_tiers(&path, g, "");
    let mut got = faults_of(&r, "adjacency");
    got.sort();
    let mut want = vec![
        ("missing_entry".to_string(), format!("e{e}")),
        ("no_edge".to_string(), format!("n{to}")),
        ("wrong_entry".to_string(), format!("e{other}")),
        ("stale_entry".to_string(), format!("n{}", o.to)),
    ];
    want.sort();
    assert_eq!(got, want);
    for c in ["endpoints", "membership", "counters", "indexes", "derived"] {
        assert!(faults_of(&r, c).is_empty(), "{c}: {r:#?}");
    }
    cleanup(&path);
}

#[test]
fn membership_finds_records_missing_from_their_label_and_type_and_entries_without_records() {
    let (path, g) = sound_file("membership");
    let person = g.strings.lookup("Person").unwrap();
    let n = g.nodes_with_label("Person")[3];
    let (e, _, _, t) = knows(&g);
    let g = plant(&path, g, |p| {
        LABELS.delete(p, &pair_key(person, n)).unwrap();
        LABELS.put(p, &pair_key(person, 4242), &[]).unwrap();
        TYPES.delete(p, &pair_key(t, e)).unwrap();
        TYPES.put(p, &pair_key(t, 9191), &[]).unwrap();
    });
    let (_g, r) = both_tiers(&path, g, "");
    assert_eq!(
        faults_of(&r, "membership"),
        vec![
            ("missing_member".into(), format!("n{n}")),
            ("no_record".into(), "n4242".into()),
            ("missing_member".into(), format!("e{e}")),
            ("no_record".into(), "e9191".into()),
        ]
    );
    // The label still holds as many entries as the catalog counts (one
    // gone, one added), so the counts agree; nothing else is wrong.
    for c in ["endpoints", "adjacency", "counters", "indexes", "derived"] {
        assert!(faults_of(&r, c).is_empty(), "{c}: {r:#?}");
    }
    cleanup(&path);
}

#[test]
fn counters_finds_label_and_node_counts_an_id_counter_and_index_statistics_that_are_wrong() {
    let (path, g) = sound_file("counters");
    let city = g.strings.lookup("City").unwrap();
    let person = g.strings.lookup("Person").unwrap();
    let name = g.strings.lookup("name").unwrap();
    let nodes = g.node_count() as u64;
    let g = plant(&path, g, |p| {
        let mut k = vec![b'L'];
        k.extend_from_slice(&city.to_be_bytes());
        CATALOG.put(p, &k, &5u64.to_le_bytes()).unwrap();
        p.set_meta(M_NODES, nodes + 2);
        p.set_meta(M_NEXT_NODE, 10);
        let mut k = vec![b'I'];
        k.extend_from_slice(&person.to_be_bytes());
        k.extend_from_slice(&name.to_be_bytes());
        let v = CATALOG.get(p, &k).unwrap().unwrap();
        let mut v = v.clone();
        v[4..12].copy_from_slice(&3u64.to_le_bytes());
        CATALOG.put(p, &k, &v).unwrap();
    });
    let (_g, r) = both_tiers(&path, g, "");
    assert_eq!(
        faults_of(&r, "counters"),
        vec![
            ("label_count".into(), ":City".into()),
            ("node_count".into(), "nodes".into()),
            ("next_node".into(), "next_node".into()),
            ("index_count".into(), ":Person(name)".into()),
        ]
    );
    for c in ["endpoints", "adjacency", "membership", "indexes", "derived"] {
        assert!(faults_of(&r, c).is_empty(), "{c}: {r:#?}");
    }
    cleanup(&path);
}

#[test]
fn indexes_finds_an_entry_that_does_not_match_its_node_and_a_node_left_out() {
    let (path, mut g) = sound_file("indexes");
    let p3 = g.nodes_with_label("Person")[3];
    let p8 = g.nodes_with_label("Person")[8];
    // Two entries swapped by value: n(p3) listed under p8's name and the
    // other way round would be a pair of faults; one entry moved is one.
    let (ix, _, _) = g.raw_indexes()[0];
    let (old, _) = g.raw_index_prefix(ix, &Value::from("p3"));
    let (new, _) = g.raw_index_prefix(ix, &Value::from("p3 the elder"));
    // A node of the index's label that is missing from it.
    let (gone, _) = g.raw_index_prefix(ix, &Value::from("p8"));
    q(&mut g, "MATCH (n) RETURN count(n)");
    let g = plant(&path, g, |p| {
        let mut k = old.clone();
        k.extend_from_slice(&p3.to_be_bytes());
        PROPS.delete(p, &k).unwrap();
        let mut k = new.clone();
        k.extend_from_slice(&p3.to_be_bytes());
        PROPS.put(p, &k, &[0]).unwrap();
        let mut k = gone.clone();
        k.extend_from_slice(&p8.to_be_bytes());
        PROPS.delete(p, &k).unwrap();
    });
    let (_g, r) = both_tiers(&path, g, "");
    assert_eq!(
        faults_of(&r, "indexes"),
        vec![
            ("stale_entry".into(), format!("n{p3}")),
            ("missing_entry".into(), format!("n{p3}")),
            ("missing_entry".into(), format!("n{p8}")),
        ]
    );
    // The statistics now count one entry more than the index holds.
    assert_eq!(
        faults_of(&r, "counters"),
        vec![("index_count".into(), ":Person(name)".into())]
    );
    cleanup(&path);
}

// ------------------------------------------------------- derived data

/// Run a write as a path below the hooks would have: afterwards the text
/// index is put back as it was before. Every path the engine has keeps the
/// index current (rule D5), so this is the only way to make it stale.
fn below_the_hooks(g: &mut Graph, f: impl FnOnce(&mut Graph)) {
    let scan = |g: &Graph| {
        let mut out = std::collections::BTreeMap::new();
        g.ext_scan("context", b"", &mut |k, v| {
            out.insert(k.to_vec(), v.to_vec());
            true
        });
        out
    };
    let before = scan(g);
    f(g);
    let after = scan(g);
    let dels = after
        .keys()
        .filter(|k| !before.contains_key(*k))
        .cloned()
        .collect();
    let puts = before
        .into_iter()
        .filter(|(k, v)| after.get(k) != Some(v))
        .collect();
    g.ext_write("context", puts, dels).unwrap();
}

fn indexed() -> Graph {
    let mut g = Graph::memory();
    people(&mut g);
    q(
        &mut g,
        "MATCH (p:Person {name: 'p1'}) SET p.note = 'torque the bolts'",
    );
    q(&mut g, "CREATE EXTENSION context");
    q(&mut g, "CALL context.index(labels: ['Person'])");
    g
}

fn derived(g: &mut Graph) -> Vec<(String, String)> {
    let r = check(g, "checks: ['derived']");
    faults_of(&r, "derived")
}

fn posting(term: &str, node: u64) -> Vec<u8> {
    let mut k = vec![b'p'];
    k.extend_from_slice(term.as_bytes());
    k.push(0);
    k.extend_from_slice(&node.to_be_bytes());
    k
}

#[test]
fn the_text_index_agrees_with_a_rebuild_after_writes_through_its_hooks() {
    let mut g = indexed();
    assert!(derived(&mut g).is_empty());
    q(
        &mut g,
        "MATCH (p:Person {name: 'p3'}) SET p.name = 'p3 p3 renamed'",
    );
    q(&mut g, "MATCH (p:Person {name: 'p4'}) DETACH DELETE p");
    q(&mut g, "MATCH (p:Person {name: 'p5'}) REMOVE p:Person");
    q(
        &mut g,
        "CREATE (:Person {name: 'late arrival', note: 'torque'})",
    );
    assert!(derived(&mut g).is_empty());
    let r = check(&mut g, "checks: ['derived']");
    let (_, _, records, problems) = summaries(&r)[0].clone();
    assert!(records > 30, "it looked at the index: {records}");
    assert_eq!(problems, 0);
}

#[test]
fn the_text_index_check_names_each_node_that_differs_from_a_rebuild() {
    let mut g = indexed();
    let p1 = g.nodes_with_label("Person")[1];
    let p2 = g.nodes_with_label("Person")[2];
    let city = g.nodes_with_label("City")[0];
    // A posting lost, a term the node does not have, a node the index does
    // not cover listed under a term.
    g.ext_write("context", vec![], vec![posting("torque", p1)])
        .unwrap();
    g.ext_write(
        "context",
        vec![
            (posting("zebra", p2), varint(1)),
            (posting("here", city), varint(2)),
        ],
        vec![],
    )
    .unwrap();
    // A node written by a path that runs no hooks: never indexed.
    let id = g.raw_meta().next_node;
    below_the_hooks(&mut g, |g| {
        g.apply_op(Op::NodeAdd {
            id,
            labels: vec!["Person".into()],
            props: vec![("name".into(), Value::from("unhooked"))],
        })
        .unwrap();
    });
    let got = derived(&mut g);
    assert_eq!(
        got,
        vec![
            ("context.stale_node".into(), format!("n{p1}")),
            ("context.stale_node".into(), format!("n{id}")),
            // The postings, term by term: a term's count is compared when
            // the next term starts; terms with no postings come after.
            ("context.orphan_posting".into(), format!("n{city}")),
            ("context.document_frequency".into(), "here".into()),
            ("context.document_frequency".into(), "zebra".into()),
            ("context.document_frequency".into(), "torque".into()),
            ("context.statistics".into(), "context".into()),
            ("context.extra_posting".into(), format!("n{p2}")),
        ],
        "{got:#?}"
    );
    // A rebuild makes it right again.
    q(&mut g, "CALL context.index(labels: ['Person'])");
    assert!(derived(&mut g).is_empty());
}

#[test]
fn the_text_index_check_names_a_node_that_is_gone() {
    let mut g = indexed();
    let p6 = g.nodes_with_label("Person")[6];
    // Deleted below the hooks: its postings and length stay behind.
    below_the_hooks(&mut g, |g| g.apply_op(Op::NodeDel { id: p6 }).unwrap());
    // Named once, by its length entry; its postings are its, so they are
    // not named again; the totals count it.
    assert_eq!(
        derived(&mut g),
        vec![
            ("context.gone_node".into(), format!("n{p6}")),
            ("context.statistics".into(), "context".into()),
        ]
    );
}

#[test]
fn an_extension_that_cannot_check_is_reported_as_unchecked_not_as_a_fault() {
    struct Silent;
    impl glider::ext::Extension for Silent {
        fn name(&self) -> &str {
            "silent_check_test"
        }
        fn version(&self) -> &str {
            "1"
        }
        fn summary(&self) -> &str {
            "keeps something it cannot check"
        }
        fn procedures(&self) -> &[glider::ext::Spec] {
            &[]
        }
        fn call(
            &self,
            _g: &mut Graph,
            _p: &str,
            _a: &glider::ext::Args,
        ) -> glider::Result<QueryResult> {
            Ok(QueryResult::message(""))
        }
    }
    glider::ext::register(std::sync::Arc::new(Silent));
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION silent_check_test");
    let r = check(&mut g, "");
    assert_eq!(
        found(&r),
        vec![(
            "derived".into(),
            "info".into(),
            "not_checked".into(),
            "silent_check_test".into(),
            "extension silent_check_test does not say how to check what it derives".into()
        )]
    );
    assert!(summaries(&r).iter().all(|s| s.3 == 0));
}

// ------------------------------------------------------- data checks

fn catalogue() -> Graph {
    let mut g = Graph::memory();
    q(
        &mut g,
        "CREATE (:Tool {code: 'T1', kind: 'wrench', maker: 'Acme', spec: 10}), \
                (:Tool {code: 'T2', kind: 'wrench', maker: 'Acme', spec: 12}), \
                (:Tool {code: 'T3', kind: 'wrench', maker: 'Acme', spec: 10}), \
                (:Tool {code: 'T4', kind: 'drill', maker: 'Acme', spec: 10}), \
                (:Tool {code: 'T5', kind: 'wrench', maker: 'Acme', spec: 10.0}), \
                (:Tool {code: 'T6', kind: 'wrench', maker: 'Acme'}), \
                (:Station {code: 'S1'}), (:Station {code: 'S2'}), (:Station {code: 'S3'}), \
                (:Part {code: 'P1'})",
    );
    q(
        &mut g,
        "MATCH (s:Station {code: 'S1'}), (t:Tool {code: 'T1'}) CREATE (s)-[:USES]->(t), (s)-[:USES]->(t), (s)-[:USES {n: 1}]->(t), (s)-[:USES]->(t)",
    );
    q(
        &mut g,
        "MATCH (s:Station {code: 'S2'}), (t:Tool {code: 'T2'}) CREATE (s)-[:USES]->(t), (s)-[:HOLDS]->(t)",
    );
    g
}

fn id_of(g: &mut Graph, label: &str, code: &str) -> u64 {
    let r = q(
        g,
        &format!("MATCH (n:{label} {{code: '{code}'}}) RETURN id(n)"),
    );
    r.rows[0][0].as_i64().unwrap() as u64
}

#[test]
fn data_checks_run_only_when_named_and_report_candidates() {
    let mut g = catalogue();
    let r = check(&mut g, "");
    assert_sound(&r, "the database itself is sound");

    let r = check(&mut g, "checks: ['duplicates']");
    let uses = g.edges_with_type("USES");
    let got = faults_of(&r, "duplicates");
    // Four parallel S1 -USES-> T1: three without properties (one repeats
    // twice), one with n: 1. S2 -USES-> and -HOLDS-> T2 differ by type.
    assert_eq!(
        got,
        vec![
            ("parallel_edge".into(), format!("e{}", uses[1])),
            ("parallel_edge".into(), format!("e{}", uses[3])),
        ]
    );
    assert!(r
        .iter()
        .filter(|r| r.1 != "summary")
        .all(|r| r.1 == "candidate"));

    let r = check(
        &mut g,
        "checks: ['duplicates'], label: 'Tool', keys: ['kind', 'maker', 'spec']",
    );
    let (t1, t3, t5) = (
        id_of(&mut g, "Tool", "T1"),
        id_of(&mut g, "Tool", "T3"),
        id_of(&mut g, "Tool", "T5"),
    );
    let nodes: Vec<Row> = found(&r)
        .into_iter()
        .filter(|r| r.2 == "duplicate_node")
        .collect();
    // T1, T3 and T5 agree (10 and 10.0 are the same number, as the index
    // compares them); T6 lacks spec and is not compared; T2, T4 differ.
    assert_eq!(
        nodes.iter().map(|r| r.3.clone()).collect::<Vec<_>>(),
        vec![format!("n{t3}"), format!("n{t5}")]
    );
    assert!(nodes[0].4.contains(&format!("n{t1}")), "{nodes:?}");

    let r = check(&mut g, "checks: ['orphans']");
    let orphans: Vec<String> = faults_of(&r, "orphans").into_iter().map(|f| f.1).collect();
    let want: Vec<String> = [
        ("Tool", "T3"),
        ("Tool", "T4"),
        ("Tool", "T5"),
        ("Tool", "T6"),
        ("Station", "S3"),
        ("Part", "P1"),
    ]
    .iter()
    .map(|(l, c)| id_of(&mut g, l, c))
    .map(|id| format!("n{id}"))
    .collect();
    let mut want = want;
    want.sort_by_key(|s| s[1..].parse::<u64>().unwrap());
    assert_eq!(orphans, want);

    let r = check(&mut g, "checks: ['orphans'], labels: ['Station', 'Part']");
    let orphans: Vec<String> = faults_of(&r, "orphans").into_iter().map(|f| f.1).collect();
    assert_eq!(
        orphans,
        vec![
            format!("n{}", id_of(&mut g, "Station", "S3")),
            format!("n{}", id_of(&mut g, "Part", "P1"))
        ]
    );

    let r = check(
        &mut g,
        "checks: ['required'], label: 'Station', type: 'USES', dir: 'out'",
    );
    assert_eq!(
        faults_of(&r, "required"),
        vec![(
            "missing_edge".into(),
            format!("n{}", id_of(&mut g, "Station", "S3"))
        )]
    );
    let r = check(
        &mut g,
        "checks: ['required'], label: 'Tool', type: 'USES', dir: 'in'",
    );
    assert_eq!(faults_of(&r, "required").len(), 4);
    let r = check(
        &mut g,
        "checks: ['required'], label: 'Tool', type: 'HOLDS', dir: 'both'",
    );
    assert_eq!(faults_of(&r, "required").len(), 5);
}

#[test]
fn arguments_that_cannot_be_right_are_errors_that_say_what_to_do() {
    let mut g = catalogue();
    q(&mut g, "CREATE EXTENSION check");
    assert!(
        err(&mut g, "CALL check.run(checks: ['everything'])").contains("the checks are endpoints")
    );
    assert!(err(
        &mut g,
        "CALL check.run(checks: ['required'], label: 'Station')"
    )
    .contains("needs label: and type:"));
    assert!(err(
        &mut g,
        "CALL check.run(checks: ['duplicates'], keys: ['code'])"
    )
    .contains("give label:"));
    assert!(err(&mut g, "CALL check.run(limit: 0)").contains("give 1 or more"));
    assert!(err(&mut g, "CALL check.run(after: 'nonsense')").contains("not a cursor"));
    assert!(err(
        &mut g,
        "CALL check.run(checks: ['endpoints'], after: 'orphans:0:-:')"
    )
    .contains("same arguments"));
    assert!(err(
        &mut g,
        "CALL check.run(checks: ['required'], label: 'S', type: 'T', dir: 'up')"
    )
    .contains("out, in or both"));
}

// ------------------------------------------------------- nothing changes

fn snapshot(g: &mut Graph) -> (u64, String, Vec<(Vec<u8>, Vec<u8>)>) {
    let mut out = Vec::new();
    glider::export_jsonl_to(g, &mut out).unwrap();
    let mut ext = Vec::new();
    for (name, _) in g.extensions() {
        g.ext_scan(&name, &[], &mut |k, v| {
            ext.push((k.to_vec(), v.to_vec()));
            true
        });
    }
    (g.revision(), String::from_utf8(out).unwrap(), ext)
}

#[test]
fn checking_changes_nothing_and_leaves_the_revision_where_it_was() {
    for faulty in [false, true] {
        let mut g = indexed();
        q(&mut g, "CREATE EXTENSION check");
        if faulty {
            let p1 = g.nodes_with_label("Person")[1];
            g.ext_write("context", vec![], vec![posting("torque", p1)])
                .unwrap();
            q(
                &mut g,
                "MATCH (s:Person {name: 'p9'}) CREATE (s)-[:KNOWS]->(s), (s)-[:KNOWS]->(s)",
            );
        }
        let before = snapshot(&mut g);
        let all = "checks: ['endpoints', 'adjacency', 'membership', 'counters', 'indexes', 'derived', 'duplicates', 'orphans', 'required'], label: 'Person', keys: ['age'], type: 'KNOWS'";
        let r = check(&mut g, all);
        assert_eq!(!found(&r).is_empty(), faulty || !found(&r).is_empty());
        check(&mut g, &format!("{all}, limit: 7"));
        assert_eq!(snapshot(&mut g), before, "faulty: {faulty}");
    }
    // And on a file, across a reopen.
    let path = temp("unchanged");
    let mut g = file_graph(&path);
    people(&mut g);
    q(&mut g, "CREATE EXTENSION check");
    let rev = g.revision();
    check(&mut g, "");
    assert_eq!(g.revision(), rev);
    drop(g);
    let g = file_graph(&path);
    assert_eq!(g.revision(), rev);
    cleanup(&path);
}

// ------------------------------------------------------- slices

/// Run in slices of `limit` records, following the cursor, and return every
/// row of every slice, and the number of calls.
fn in_slices(g: &mut Graph, args: &str, limit: u64) -> (Vec<Row>, usize) {
    let mut all = Vec::new();
    let mut after: Option<String> = None;
    let mut calls = 0;
    loop {
        let a = match &after {
            Some(c) => format!("{args}, limit: {limit}, after: '{c}'"),
            None => format!("{args}, limit: {limit}"),
        };
        let a = a.trim_start_matches(", ").to_string();
        let r = check(g, &a);
        calls += 1;
        after = r
            .iter()
            .find(|r| r.1 == "summary" && r.2 == "partial")
            .map(|r| r.3.clone());
        all.extend(r);
        if after.is_none() {
            return (all, calls);
        }
        assert!(calls < 100_000, "slices do not end");
    }
}

/// A database with faults for every check, in several places.
fn faulty() -> Graph {
    let mut g = indexed();
    let mut ps = g.nodes_with_label("Person");
    ps.sort();
    g.ext_write("context", vec![], vec![posting("torque", ps[1])])
        .unwrap();
    g.ext_write(
        "context",
        vec![(posting("zebra", ps[2]), varint(1))],
        vec![],
    )
    .unwrap();
    g.apply_op(Op::NodeDel { id: ps[6] }).unwrap();
    q(
        &mut g,
        "MATCH (s:Person {name: 'p9'}) CREATE (s)-[:KNOWS]->(s), (s)-[:KNOWS]->(s)",
    );
    q(
        &mut g,
        "MATCH (s:Person {name: 'p12'}) CREATE (s)-[:KNOWS]->(s), (s)-[:KNOWS]->(s)",
    );
    q(
        &mut g,
        "CREATE (:Person {name: 'p3', age: 3}), (:Lonely), (:Lonely)",
    );
    g
}

#[test]
fn slices_together_report_exactly_what_one_whole_run_reports() {
    let args = "checks: ['endpoints', 'adjacency', 'membership', 'counters', 'indexes', 'derived', 'duplicates', 'orphans', 'required'], label: 'Person', keys: ['name'], type: 'LIVES_IN'";
    let mut g = faulty();
    let whole = check(&mut g, args);
    assert!(found(&whole).len() > 10, "{whole:#?}");
    let total = |rows: &[Row]| -> Vec<(String, u64, u64)> {
        let mut by: std::collections::BTreeMap<String, (u64, u64)> = Default::default();
        for (c, _, r, p) in summaries(rows) {
            let e = by.entry(c).or_default();
            e.0 += r;
            e.1 += p;
        }
        by.into_iter().map(|(c, (r, p))| (c, r, p)).collect()
    };
    for limit in [1, 2, 7, 50, 1000] {
        let (pieces, calls) = in_slices(&mut g, args, limit);
        assert_eq!(found(&pieces), found(&whole), "limit {limit}");
        assert_eq!(total(&pieces), total(&whole), "limit {limit}");
        if limit == 1 {
            let records: u64 = total(&whole).iter().map(|t| t.1).sum();
            assert!(
                calls as u64 >= records,
                "{calls} calls for {records} records"
            );
        }
    }
}

#[test]
fn a_hub_is_checked_within_a_small_memory_budget_with_the_same_answer() {
    // A node with thousands of parallel edges: its outgoing edges are
    // sorted outside memory when they do not fit.
    let path = temp("hub");
    let mut g = file_graph(&path);
    g.autocommit = false;
    let hub = g.add_node(&["Hub".into()], vec![]).unwrap();
    let mut spokes = Vec::new();
    for i in 0..400 {
        spokes.push(
            g.add_node(&["Spoke".into()], vec![("i".into(), Value::Int(i))])
                .unwrap(),
        );
    }
    for round in 0..12 {
        for (i, &s) in spokes.iter().enumerate() {
            let props = if (i + round) % 5 == 0 {
                vec![("round".into(), Value::Int(round as i64))]
            } else {
                vec![]
            };
            g.add_edge(hub, s, "TO", props).unwrap();
        }
    }
    g.commit().unwrap();
    let args = "checks: ['duplicates']";
    let roomy = check(&mut g, args);
    g.set_work_mem(64 << 10);
    let tight = check(&mut g, args);
    assert_eq!(roomy, tight);
    assert!(found(&roomy).len() > 3000, "{}", found(&roomy).len());
    drop(g);
    cleanup(&path);
}

#[test]
fn the_rows_cap_ends_a_slice_and_the_cursor_carries_on() {
    let mut g = Graph::memory();
    g.autocommit = false;
    for _ in 0..(glider::ext::check::MAX_ROWS + 500) {
        g.add_node(&["Lonely".into()], vec![]).unwrap();
    }
    g.commit().unwrap();
    let r = check(&mut g, "checks: ['orphans']");
    assert_eq!(r.len(), glider::ext::check::MAX_ROWS + 1);
    let last = r.last().unwrap();
    assert_eq!((last.1.as_str(), last.2.as_str()), ("summary", "partial"));
    let (all, calls) = in_slices(&mut g, "checks: ['orphans']", u64::MAX / 2);
    assert_eq!(calls, 2);
    assert_eq!(found(&all).len(), glider::ext::check::MAX_ROWS + 500);
}

#[test]
fn keys_cut_to_fit_are_counted_as_the_engine_counts_them() {
    // Long values sharing a prefix longer than a key: the distinct count
    // compares whole values, read from the nodes.
    let mut g = Graph::memory_with_page_size(512, 1 << 30);
    q(&mut g, "INDEX ON :Doc(body)");
    let long = "x".repeat(600);
    for i in 0..9 {
        q(
            &mut g,
            &format!("CREATE (:Doc {{body: '{long}{}'}})", i % 3),
        );
    }
    let r = check(&mut g, "checks: ['counters', 'indexes']");
    assert_sound(&r, "long values");
    assert_eq!(g.indexes()[0].2, 3);
}

#[test]
fn the_help_lists_the_procedure() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION check");
    let r = q(&mut g, "CALL check.help()");
    assert!(r.rows.iter().any(|r| text(&r[1]).starts_with("check.run(")));
}
