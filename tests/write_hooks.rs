//! No write goes unnoticed (rule D5 in docs/DERIVED.md): every change to a
//! node or an edge, by every path the engine has, reaches the extensions
//! that follow writes inside the writing transaction, either record by
//! record or as one bulk call.
//!
//! The witness is a recording extension. For every id a hook reports it
//! keeps the first image it was shown before and the last image it was
//! shown after; a bulk call leaves a mark. After each write path runs, the
//! graph before and after are compared: every node or edge that differs
//! must have been reported, and every report must agree with the graph as
//! it was before and as it is after.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};

use glider::ext::{self, Args, BulkWrite, EdgeImage, Extension, NodeImage, Spec};
use glider::graph::{Graph, OpenOptions};
use glider::legacy::graph::Graph as Legacy;
use glider::query::{self, QueryResult};
use glider::store::{Op, Sync};
use glider::value::Value;
use glider::{Error, Result};

const NS: &str = "recorder";

thread_local! {
    /// Hook calls on this thread: replay must not add to it.
    static CALLS: Cell<u64> = const { Cell::new(0) };
}

fn calls() -> u64 {
    CALLS.with(|c| c.get())
}

fn sorted<T: Ord + Clone>(v: &[T]) -> Vec<T> {
    let mut v = v.to_vec();
    v.sort();
    v
}

fn props_dbg(p: &[(String, Value)]) -> String {
    let mut p = p.to_vec();
    p.sort_by(|a, b| a.0.cmp(&b.0));
    format!("{p:?}")
}

fn node_dbg(labels: &[String], props: &[(String, Value)]) -> String {
    format!("{:?} {}", sorted(labels), props_dbg(props))
}

fn edge_dbg(from: u64, to: u64, etype: &str, props: &[(String, Value)]) -> String {
    format!("{from}-{etype}->{to} {}", props_dbg(props))
}

fn opt(s: Option<String>) -> String {
    s.unwrap_or_else(|| "absent".into())
}

/// Follows every write; refuses a node labelled `Fail`, an edge typed
/// `FAIL`, and the deletion of an edge carrying `keep`.
struct Recorder;

fn record(g: &mut Graph, kind: u8, id: u64, before: String, after: String) -> Result<()> {
    CALLS.with(|c| c.set(c.get() + 1));
    let mut key = vec![kind];
    key.extend_from_slice(&id.to_be_bytes());
    let first = match g.ext_get(NS, &key) {
        Some(v) => {
            let at = v.iter().position(|&b| b == 0).unwrap();
            String::from_utf8(v[..at].to_vec()).unwrap()
        }
        None => before,
    };
    let mut val = first.into_bytes();
    val.push(0);
    val.extend_from_slice(after.as_bytes());
    g.ext_write(NS, vec![(key, val)], Vec::new())
}

impl Extension for Recorder {
    fn name(&self) -> &str {
        NS
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "records every write it is told about"
    }
    fn procedures(&self) -> &[Spec] {
        &[]
    }
    fn call(&self, _: &mut Graph, _: &str, _: &Args) -> Result<QueryResult> {
        unreachable!()
    }
    fn wants_writes(&self, _: &Graph) -> bool {
        true
    }
    fn wants_edge_writes(&self, _: &Graph) -> bool {
        true
    }
    fn on_node_write(
        &self,
        g: &mut Graph,
        id: u64,
        before: Option<&NodeImage>,
        after: Option<&NodeImage>,
    ) -> Result<()> {
        if after.is_some_and(|n| n.labels.iter().any(|l| l == "Fail")) {
            return Err(Error::Msg("the recorder refuses :Fail nodes".into()));
        }
        let img = |n: Option<&NodeImage>| opt(n.map(|n| node_dbg(&n.labels, &n.props)));
        record(g, b'n', id, img(before), img(after))
    }
    fn on_edge_write(
        &self,
        g: &mut Graph,
        id: u64,
        before: Option<&EdgeImage>,
        after: Option<&EdgeImage>,
    ) -> Result<()> {
        if after.is_some_and(|e| e.etype == "FAIL") {
            return Err(Error::Msg("the recorder refuses FAIL edges".into()));
        }
        if after.is_none() && before.is_some_and(|e| e.props.iter().any(|(k, _)| k == "keep")) {
            return Err(Error::Msg(
                "the recorder refuses to delete a kept edge".into(),
            ));
        }
        let img =
            |e: Option<&EdgeImage>| opt(e.map(|e| edge_dbg(e.from, e.to, &e.etype, &e.props)));
        record(g, b'e', id, img(before), img(after))
    }
    fn on_bulk_write(&self, g: &mut Graph, what: BulkWrite) -> Result<()> {
        CALLS.with(|c| c.set(c.get() + 1));
        assert_eq!(what, BulkWrite::Clear);
        if g.ext_get(NS, b"!failbulk").is_some() {
            return Err(Error::Msg("the recorder refuses this clear".into()));
        }
        g.ext_write(NS, vec![(b"bulk".to_vec(), Vec::new())], Vec::new())
    }
}

/// Follows node writes only; an edge hook call would be a bug.
struct NodesOnly;

impl Extension for NodesOnly {
    fn name(&self) -> &str {
        "nodesonly"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "follows node writes and nothing else"
    }
    fn procedures(&self) -> &[Spec] {
        &[]
    }
    fn call(&self, _: &mut Graph, _: &str, _: &Args) -> Result<QueryResult> {
        unreachable!()
    }
    fn wants_writes(&self, _: &Graph) -> bool {
        true
    }
    fn on_node_write(
        &self,
        g: &mut Graph,
        id: u64,
        _: Option<&NodeImage>,
        _: Option<&NodeImage>,
    ) -> Result<()> {
        g.ext_write(
            "nodesonly",
            vec![(id.to_be_bytes().to_vec(), Vec::new())],
            Vec::new(),
        )
    }
    fn on_edge_write(
        &self,
        _: &mut Graph,
        id: u64,
        _: Option<&EdgeImage>,
        _: Option<&EdgeImage>,
    ) -> Result<()> {
        panic!("edge {id} reported to an extension that did not ask");
    }
}

fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        ext::register(Arc::new(Recorder));
        ext::register(Arc::new(NodesOnly));
    });
}

fn q(g: &mut Graph, src: &str) -> QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-hooks-{}-{}.db", name, std::process::id()));
    cleanup(&p);
    p
}

fn cleanup(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(glider::store::lock_path(p));
    for suffix in ["-wal", "-data", "-tmp"] {
        let mut d = p.as_os_str().to_os_string();
        d.push(suffix);
        let _ = std::fs::remove_dir_all(PathBuf::from(d));
    }
}

// ------------------------------------------------------------- accounting

/// The whole graph, as the recorder renders records.
#[derive(Debug, PartialEq)]
struct Obs {
    nodes: BTreeMap<u64, String>,
    edges: BTreeMap<u64, String>,
}

fn observe(g: &Graph) -> Obs {
    let nodes = g
        .node_ids()
        .into_iter()
        .map(|id| (id, node_dbg(&g.node_labels(id), &g.node_props(id))))
        .collect();
    let edges = g
        .edge_ids()
        .into_iter()
        .map(|id| {
            let e = g.edge(id).unwrap();
            let t = g.edge_type_name(id).unwrap().to_string();
            (id, edge_dbg(e.from, e.to, &t, &g.edge_props(id)))
        })
        .collect();
    Obs { nodes, edges }
}

#[derive(Debug, Default, PartialEq)]
struct Recorded {
    nodes: BTreeMap<u64, (String, String)>,
    edges: BTreeMap<u64, (String, String)>,
    bulk: bool,
}

fn keyspace(g: &Graph, ns: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    g.ext_scan(ns, b"", &mut |k, v| {
        out.push((k.to_vec(), v.to_vec()));
        true
    });
    out
}

fn recorded(g: &Graph) -> Recorded {
    let mut r = Recorded::default();
    for (k, v) in keyspace(g, NS) {
        if k == b"!failbulk" {
            panic!("the bulk failure switch survived");
        }
        if k == b"bulk" {
            r.bulk = true;
            continue;
        }
        let id = u64::from_be_bytes(k[1..9].try_into().unwrap());
        let at = v.iter().position(|&b| b == 0).unwrap();
        let pair = (
            String::from_utf8(v[..at].to_vec()).unwrap(),
            String::from_utf8(v[at + 1..].to_vec()).unwrap(),
        );
        match k[0] {
            b'n' => r.nodes.insert(id, pair),
            b'e' => r.edges.insert(id, pair),
            _ => panic!("unexpected key {k:?}"),
        };
    }
    r
}

fn forget_records(g: &mut Graph) {
    let keys: Vec<Vec<u8>> = keyspace(g, NS).into_iter().map(|(k, _)| k).collect();
    g.ext_write(NS, Vec::new(), keys).unwrap();
    assert_eq!(recorded(g), Recorded::default());
}

/// What was recorded accounts for every difference between `before` and
/// `after`, and says nothing the graph contradicts.
fn account(
    what: &str,
    before: &BTreeMap<u64, String>,
    after: &BTreeMap<u64, String>,
    rec: &BTreeMap<u64, (String, String)>,
) -> usize {
    let ids: BTreeSet<u64> = before.keys().chain(after.keys()).copied().collect();
    let mut changed = 0;
    for id in ids {
        let (b, a) = (before.get(&id), after.get(&id));
        if b != a {
            changed += 1;
            assert!(rec.contains_key(&id), "{what} {id} changed unreported");
        }
    }
    for (id, (rb, ra)) in rec {
        assert_eq!(
            rb,
            &opt(before.get(id).cloned()),
            "{what} {id}: the first image before is not what the graph held"
        );
        assert_eq!(
            ra,
            &opt(after.get(id).cloned()),
            "{what} {id}: the last image after is not what the graph holds"
        );
    }
    changed
}

/// Run one write path and check the recorder heard about all of it.
/// Returns how many nodes and edges it changed.
fn path(g: &mut Graph, name: &str, f: impl FnOnce(&mut Graph)) -> usize {
    forget_records(g);
    let before = observe(g);
    f(g);
    let after = observe(g);
    let rec = recorded(g);
    if rec.bulk {
        // One call that says everything may have changed covers anything.
        return usize::from(before != after);
    }
    account(
        &format!("{name}: node"),
        &before.nodes,
        &after.nodes,
        &rec.nodes,
    ) + account(
        &format!("{name}: edge"),
        &before.edges,
        &after.edges,
        &rec.edges,
    )
}

/// Run a write that must leave no trace: neither in the graph nor in what
/// the recorder kept.
fn rolled_back(g: &mut Graph, name: &str, f: impl FnOnce(&mut Graph)) {
    forget_records(g);
    let before = observe(g);
    f(g);
    assert_eq!(observe(g), before, "{name}: the graph changed");
    assert_eq!(
        recorded(g),
        Recorded::default(),
        "{name}: a record survived"
    );
}

fn legacy(seed: u64, nodes: u64, edges: u64) -> Legacy {
    let mut l = Legacy::memory();
    let mut x = seed | 1;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for i in 0..nodes {
        let label = ["P", "Q", "R"][(next() % 3) as usize];
        l.add_node(
            &[label.to_string()],
            vec![
                ("name".into(), Value::Text(format!("{label}{seed}-{i}"))),
                ("n".into(), Value::Int(i as i64)),
            ],
        )
        .unwrap();
    }
    for _ in 0..edges {
        let a = 1 + next() % nodes;
        let b = 1 + next() % nodes;
        let t = ["K", "L"][(next() % 2) as usize];
        l.add_edge(a, b, t, vec![("w".into(), Value::Int((next() % 9) as i64))])
            .unwrap();
    }
    l
}

// ------------------------------------------------------------------ tests

#[test]
fn every_write_path_is_accounted_for_by_the_hooks() {
    register();
    let db = temp("every-path");
    let opts = OpenOptions {
        sync: Sync::Off,
        page_size: 1024,
        ..OpenOptions::default()
    };
    // Bulk load builds a new database, so nothing can be installed in it
    // yet: there is no derived state for it to leave behind.
    let mut g = Graph::bulk_load(&db, &legacy(1, 40, 80), opts, 1 << 20).unwrap();
    assert_eq!(g.node_count(), 40);
    assert!(g.extensions().is_empty());
    q(&mut g, "CREATE EXTENSION recorder");
    let mut changed = Vec::new();
    let mut run = |g: &mut Graph, name: &str, f: &dyn Fn(&mut Graph)| {
        let n = path(g, name, f);
        changed.push((name.to_string(), n));
    };

    // Statements of every kind that writes.
    for src in [
        "CREATE (a:P {name: \"x\"})-[:K {w: 1}]->(b:P {name: \"y\"})",
        "MATCH (a:P {name: \"x\"})-[r:K]->(b) SET r.w = 2, r.v = \"v\", a.age = 3",
        "MATCH ()-[r:K]->() WHERE r.w = 2 REMOVE r.w",
        "MATCH (n:P) WHERE n.n IS NOT NULL AND n.n < 5 SET n:S",
        "MATCH (n:S) REMOVE n:S, n.name",
        "MATCH (n:P {name: \"x\"}) CREATE (n)-[:SELF {s: 1}]->(n)",
        "MATCH (n:P {name: \"x\"}) DELETE n",
        "MATCH (n:Q) WHERE n.n IS NOT NULL AND n.n < 8 DETACH DELETE n",
        "MATCH ()-[r:L]->() WHERE r.w > 6 DELETE r",
        "CREATE (:U {i: 1})-[:UL]->(:V {i: 1}), (:U {i: 2})-[:UL]->(:V {i: 2})",
        "MATCH (u:U), (v:V) WHERE u.i = v.i CREATE (v)-[:BACK]->(u)",
        "CALL pagerank(iterations: 5, write: \"pr\")",
        "INDEX ON :P(name)",
    ] {
        run(&mut g, src, &|g| {
            q(g, src);
        });
    }
    run(&mut g, "BEGIN ... COMMIT", &|g| {
        for src in [
            "BEGIN",
            "CREATE (:T {t: 1})-[:TT]->(:T {t: 2})",
            "MATCH (a:T {t: 1}) SET a.t = 10",
            "MATCH ()-[r:TT]->() SET r.x = 1",
            "MATCH (b:T {t: 2}) DETACH DELETE b",
            "COMMIT",
        ] {
            q(g, src);
        }
    });

    // The graph API, call by call.
    run(&mut g, "API", &|g| {
        let a = g
            .add_node(&["A".into()], vec![("k".into(), Value::Int(1))])
            .unwrap();
        let b = g.add_node(&["A".into()], vec![]).unwrap();
        let e = g
            .add_edge(a, b, "AB", vec![("w".into(), Value::Int(1))])
            .unwrap();
        let f = g.add_edge(b, b, "LOOP", vec![]).unwrap();
        g.add_edge(b, a, "BA", vec![]).unwrap();
        g.set_node_prop(a, "k", Value::Int(2)).unwrap();
        g.unset_node_prop(a, "k").unwrap();
        g.unset_node_prop(a, "missing").unwrap();
        g.set_edge_prop(e, "w", Value::Int(2)).unwrap();
        g.unset_edge_prop(e, "w").unwrap();
        g.unset_edge_prop(e, "missing").unwrap();
        g.add_label(a, "B").unwrap();
        g.remove_label(a, "A").unwrap();
        g.delete_edge(f).unwrap();
        g.delete_node(b).unwrap();
    });

    // A batch of statements in one transaction.
    run(&mut g, "batch", &|g| {
        let p = |s: &str| (s.to_string(), Vec::new());
        query::execute_batch(
            g,
            &[
                p("CREATE (:Batch {i: 1})-[:BB]->(:Batch {i: 2})"),
                p("MATCH (b:Batch {i: 1}) SET b.i = 3"),
                p("MATCH (b:Batch {i: 2}) DETACH DELETE b"),
            ],
        )
        .unwrap();
    });

    // JSON Lines, from a string and streamed from a file.
    let jsonl = "{\"type\":\"node\",\"key\":\"a\",\"labels\":[\"J\"],\"props\":{\"name\":\"ja\"}}\n\
                 {\"type\":\"node\",\"key\":\"b\",\"label\":\"J\"}\n\
                 {\"type\":\"edge\",\"from\":\"a\",\"to\":\"b\",\"label\":\"JJ\",\"props\":{\"w\":1}}\n";
    run(&mut g, "import_jsonl", &|g| {
        query::import_jsonl(g, jsonl).unwrap();
    });
    let file = temp("import.jsonl");
    std::fs::File::create(&file)
        .unwrap()
        .write_all(jsonl.as_bytes())
        .unwrap();
    run(&mut g, "import_jsonl_from a file", &|g| {
        let f = std::fs::File::open(&file).unwrap();
        query::import_jsonl_from(g, &mut std::io::BufReader::new(f), 1 << 20).unwrap();
    });

    // Replayed operations applied by a loader, every kind, with explicit
    // ids: new ones, and ones that overwrite what is there.
    run(&mut g, "apply_op", &|g| {
        let next = g.node_ids().last().unwrap() + 1;
        let e1 = g.edge_ids()[0];
        let n1 = g.node_ids()[0];
        let victim = *g
            .node_ids()
            .iter()
            .find(|&&n| g.degree(n, glider::Dir::Both) > 0)
            .unwrap();
        let s = |x: &str| x.to_string();
        for op in [
            Op::NodeAdd {
                id: next,
                labels: vec![s("O")],
                props: vec![(s("o"), Value::Int(1))],
            },
            Op::NodeAdd {
                id: n1,
                labels: vec![s("Over")],
                props: vec![(s("over"), Value::Bool(true))],
            },
            Op::NodeSet {
                id: next,
                key: s("o"),
                value: Value::Int(2),
            },
            Op::NodeUnset {
                id: next,
                key: s("o"),
            },
            Op::LabelAdd {
                id: next,
                label: s("O2"),
            },
            Op::LabelDel {
                id: next,
                label: s("O"),
            },
            Op::EdgeAdd {
                id: 10_000,
                from: next,
                to: n1,
                etype: s("OE"),
                props: vec![],
            },
            Op::EdgeAdd {
                id: e1,
                from: n1,
                to: next,
                etype: s("REPLACED"),
                props: vec![(s("r"), Value::Int(1))],
            },
            Op::EdgeSet {
                id: 10_000,
                key: s("x"),
                value: Value::Int(1),
            },
            Op::EdgeUnset {
                id: e1,
                key: s("r"),
            },
            Op::EdgeDel { id: 10_000 },
            Op::NodeDel { id: victim },
            Op::Counters {
                next_node: next + 100,
                next_edge: 20_000,
            },
            Op::IndexAdd {
                label: s("O2"),
                key: s("o"),
            },
        ] {
            g.apply_op(op).unwrap();
        }
    });

    // A legacy graph imported over this one: its ids land on existing
    // nodes and edges and replace them.
    run(&mut g, "import_legacy", &|g| {
        g.import_legacy(&legacy(2, 30, 50)).unwrap();
    });

    // CLEAR, by statement and as a replayed operation.
    run(&mut g, "CLEAR", &|g| {
        q(g, "CLEAR");
    });
    q(&mut g, "CREATE (:Again)-[:AA]->(:Again)");
    run(&mut g, "apply_op Clear", &|g| {
        g.apply_op(Op::Clear).unwrap();
    });
    q(
        &mut g,
        "CREATE (a:Again {i: 1})-[:AA {keep: true}]->(b:Again {i: 2})",
    );

    // Writes that roll back leave nothing recorded.
    rolled_back(&mut g, "ROLLBACK", |g| {
        for src in [
            "BEGIN",
            "CREATE (:Gone)-[:GONE]->(:Gone)",
            "MATCH (a:Again {i: 1}) SET a.x = 1",
            "MATCH ()-[r:AA]->() SET r.y = 2",
            "CLEAR",
            "ROLLBACK",
        ] {
            q(g, src);
        }
    });
    rolled_back(&mut g, "API rollback", |g| {
        g.autocommit = false;
        let a = g.add_node(&["Gone".into()], vec![]).unwrap();
        g.add_edge(a, 1, "GONE", vec![]).unwrap();
        g.add_edge(1, a, "GONE", vec![]).unwrap();
        g.set_node_prop(1, "x", Value::Int(1)).unwrap();
        g.delete_node(a).unwrap();
        g.rollback().unwrap();
        g.autocommit = true;
    });
    rolled_back(&mut g, "failing statement", |g| {
        assert!(query::execute(g, "CREATE (:Fine)-[:FAIL]->(:Fine)").is_err());
        assert!(query::execute(g, "MATCH (n:Again) DETACH DELETE n").is_err());
    });
    // A hook that fails while autocommit is off: the write it was told
    // about must not become durable without what the hook would have
    // written. The open transaction is rolled back and the error says so;
    // a COMMIT after it commits nothing of it.
    let refused = |e: Error| {
        let e = e.to_string();
        assert!(e.contains("the recorder refuses"), "{e}");
        assert!(e.contains("the transaction was rolled back"), "{e}");
    };
    rolled_back(&mut g, "API, autocommit off", |g| {
        let fails: [&dyn Fn(&mut Graph) -> Result<()>; 6] = [
            &|g| g.add_node(&["Fail".into()], vec![]).map(|_| ()),
            &|g| g.add_label(1, "Fail"),
            &|g| g.add_edge(1, 2, "FAIL", vec![]).map(|_| ()),
            &|g| g.delete_edge(g.edge_ids()[0]).map(|_| ()),
            &|g| g.delete_node(1).map(|_| ()),
            &|g| {
                g.ext_write(NS, vec![(b"!failbulk".to_vec(), Vec::new())], Vec::new())?;
                g.clear()
            },
        ];
        for fail in fails {
            g.autocommit = false;
            g.add_node(&["Earlier".into()], vec![]).unwrap();
            refused(fail(g).unwrap_err());
            g.commit().unwrap();
            g.autocommit = true;
        }
    });
    rolled_back(&mut g, "statements inside BEGIN", |g| {
        for bad in [
            "CREATE (:Fail)",
            "CREATE (:Fine)-[:FAIL]->(:Fine)",
            "MATCH ()-[r:AA]->() DELETE r",
            "MATCH (n:Again) DETACH DELETE n",
            "CLEAR",
        ] {
            q(g, "BEGIN");
            q(g, "CREATE (:Earlier)-[:E]->(:Earlier)");
            if bad == "CLEAR" {
                g.ext_write(NS, vec![(b"!failbulk".to_vec(), Vec::new())], Vec::new())
                    .unwrap();
            }
            refused(query::execute(g, bad).err().expect(bad));
            q(g, "COMMIT");
        }
    });
    rolled_back(&mut g, "CLEAR, autocommit on", |g| {
        g.ext_write(NS, vec![(b"!failbulk".to_vec(), Vec::new())], Vec::new())
            .unwrap();
        let e = query::execute(g, "CLEAR").err().expect("refused");
        assert!(e.to_string().contains("the recorder refuses"), "{e}");
        g.ext_write(NS, Vec::new(), vec![b"!failbulk".to_vec()])
            .unwrap();
    });

    let total: usize = changed.iter().map(|(_, n)| n).sum();
    for (name, n) in &changed {
        // Everything but the index definition changed something.
        assert!(
            *n > 0 || name.starts_with("INDEX"),
            "{name} changed nothing"
        );
    }
    assert!(total > 150, "only {total} changes exercised");
    drop(g);
    cleanup(&db);
    let _ = std::fs::remove_file(&file);
}

#[test]
fn an_extension_that_does_not_ask_for_edge_writes_is_not_called() {
    register();
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION nodesonly");
    q(&mut g, "CREATE (a:A)-[:E {w: 1}]->(b:A)");
    q(&mut g, "MATCH ()-[r:E]->() SET r.w = 2");
    q(&mut g, "MATCH ()-[r:E]->() REMOVE r.w");
    q(&mut g, "MATCH (a:A) CREATE (a)-[:F]->(a)");
    q(&mut g, "MATCH ()-[r:F]->() DELETE r");
    q(&mut g, "MATCH (a:A) DETACH DELETE a");
    g.apply_op(Op::EdgeAdd {
        id: 99,
        from: 1,
        to: 1,
        etype: "X".into(),
        props: vec![],
    })
    .unwrap();
    // It heard about both nodes.
    assert_eq!(keyspace(&g, "nodesonly").len(), 2);
}

#[test]
fn deleting_a_node_reports_each_of_its_edges_whichever_way_it_is_deleted() {
    register();
    let deletes: [(&str, &dyn Fn(&mut Graph, u64)); 4] = [
        ("API", &|g, id| {
            g.delete_node(id).unwrap();
        }),
        ("DELETE", &|g, id| {
            q(g, &format!("MATCH (n) WHERE id(n) = {id} DELETE n"));
        }),
        ("DETACH DELETE", &|g, id| {
            q(g, &format!("MATCH (n) WHERE id(n) = {id} DETACH DELETE n"));
        }),
        ("apply_op", &|g, id| g.apply_op(Op::NodeDel { id }).unwrap()),
    ];
    for (name, delete) in deletes {
        let mut g = Graph::memory();
        q(&mut g, "CREATE EXTENSION recorder");
        let hub = g.add_node(&["H".into()], vec![]).unwrap();
        let mut edges = BTreeSet::new();
        // More edges than one detach batch, a self-loop, both directions.
        for i in 0..5000 {
            let other = g.add_node(&[], vec![]).unwrap();
            let e = if i % 2 == 0 {
                g.add_edge(hub, other, "OUT", vec![]).unwrap()
            } else {
                g.add_edge(other, hub, "IN", vec![]).unwrap()
            };
            edges.insert(e);
        }
        edges.insert(g.add_edge(hub, hub, "LOOP", vec![]).unwrap());
        forget_records(&mut g);
        delete(&mut g, hub);
        let rec = recorded(&g);
        assert_eq!(
            rec.edges.keys().copied().collect::<BTreeSet<_>>(),
            edges,
            "{name}: every edge of the deleted node is reported"
        );
        assert!(rec.edges.values().all(|(_, after)| after == "absent"));
        assert_eq!(rec.nodes.keys().copied().collect::<Vec<_>>(), vec![hub]);
        assert_eq!(g.edge_count(), 0, "{name}");
    }
}

#[test]
fn a_failing_edge_hook_fails_the_statement_and_rolls_it_back() {
    register();
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION recorder");
    q(
        &mut g,
        "CREATE (a:A {i: 1})-[:KEPT {keep: true}]->(b:A {i: 2})",
    );
    let before = observe(&g);
    let records = keyspace(&g, NS);
    for src in [
        "CREATE (:A {i: 3})-[:FAIL]->(:A {i: 4})",
        "MATCH (a:A {i: 1}), (b:A {i: 2}) CREATE (a)-[:OK]->(b), (a)-[:FAIL]->(b)",
        "MATCH ()-[r:KEPT]->() DELETE r",
        "MATCH (a:A {i: 1}) DETACH DELETE a",
        "MATCH (a:A {i: 1}) SET a.x = 1, a:Fail",
    ] {
        let Err(e) = query::execute(&mut g, src) else {
            panic!("{src}: expected the hook to refuse it");
        };
        let e = e.to_string();
        assert!(e.contains("the recorder refuses"), "{src}: {e}");
        assert_eq!(observe(&g), before, "{src}");
        assert_eq!(keyspace(&g, NS), records, "{src}");
    }
    // The API, with autocommit, rolls back the same way.
    assert!(g.add_edge(1, 2, "FAIL", vec![]).is_err());
    assert!(g.delete_node(1).is_err());
    assert_eq!(observe(&g), before);
    assert_eq!(keyspace(&g, NS), records);
}

/// The writes the replay tests make, after the recorder is installed.
fn some_writes(g: &mut Graph) {
    for i in 1..=40 {
        q(g, &format!("CREATE (:N {{i: {i}}})"));
    }
    for src in [
        "MATCH (a:N), (b:N) WHERE b.i = a.i + 1 CREATE (a)-[:NEXT {d: 1}]->(b)",
        "MATCH ()-[r:NEXT]->() WHERE r.d = 1 SET r.d = 2",
        "MATCH (a:N) WHERE a.i > 4 AND a.i < 9 DETACH DELETE a",
        "MATCH ()-[r:NEXT]->(b) WHERE b.i > 30 DELETE r",
        "MATCH (a:N) WHERE a.i = 1 SET a:First",
    ] {
        q(g, src);
    }
    let n = g.add_node(&["Api".into()], vec![]).unwrap();
    g.add_edge(n, 2, "API", vec![]).unwrap();
    g.delete_node(3).unwrap();
}

#[test]
fn recovery_and_replicas_replay_what_the_hooks_wrote_without_running_them() {
    register();
    // A replica: a base taken after the install, then the log after it.
    let mut g = Graph::memory_with_page_size(1024, u64::MAX);
    g.enable_log(glider::storage::log::DEFAULT_LOG_CAP).unwrap();
    q(&mut g, "CREATE EXTENSION recorder");
    q(&mut g, "CREATE (:Before)");
    let snap = g.snapshot().unwrap();
    let mut parts: Vec<Vec<u8>> = Vec::new();
    snap.write_image(1 << 20, &mut |_, b| {
        parts.push(b);
        Ok(())
    })
    .unwrap();
    let from = snap.lsn();
    drop(snap);
    some_writes(&mut g);
    let (_, end) = g.log_span().unwrap();
    let log = g.log_range(from, end).unwrap();
    let live = keyspace(&g, NS);
    assert!(live.len() > 40, "the hooks recorded {}", live.len());
    let before = calls();
    let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
    let (m, _) = Graph::from_replica(&refs, &[(from, &log)], u64::MAX, 1 << 20).unwrap();
    assert_eq!(calls(), before, "replay ran a hook");
    assert_eq!(observe(&m), observe(&g));
    assert_eq!(keyspace(&m, NS), live);

    // Running the hooks again on the same writes gives the same state.
    let mut again = Graph::memory();
    q(&mut again, "CREATE EXTENSION recorder");
    q(&mut again, "CREATE (:Before)");
    some_writes(&mut again);
    assert_eq!(keyspace(&again, NS), live);

    // Crash recovery: the log replayed at open, with no checkpoint after
    // the writes.
    let path = temp("recovery");
    {
        let mut f = Graph::open(&path, Sync::Always).unwrap();
        f.set_checkpoint_bytes(u64::MAX);
        q(&mut f, "CREATE EXTENSION recorder");
        q(&mut f, "CREATE (:Before)");
        some_writes(&mut f);
        assert_eq!(keyspace(&f, NS), live);
        std::mem::forget(f);
    }
    let before = calls();
    let f = Graph::open_forced(&path, Sync::Always).unwrap();
    assert_eq!(calls(), before, "recovery ran a hook");
    assert_eq!(observe(&f), observe(&g));
    assert_eq!(keyspace(&f, NS), live);
    drop(f);
    cleanup(&path);
}

/// Before edge and bulk hooks, a replayed operation or a legacy import
/// changed nodes without telling the text index, which then answered from
/// what the graph no longer held.
#[test]
fn the_text_index_stays_right_through_replayed_operations_imports_and_clear() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION context");
    q(
        &mut g,
        "CREATE (:P {name: \"copper harbor\"}), (:P {name: \"violet meadow\"})",
    );
    q(&mut g, "CALL context.index()");
    let s = |x: &str| x.to_string();
    for op in [
        Op::NodeAdd {
            id: 10,
            labels: vec![s("P")],
            props: vec![(s("name"), Value::Text(s("granite falcon")))],
        },
        Op::NodeSet {
            id: 1,
            key: s("name"),
            value: Value::Text(s("ember orbit")),
        },
        Op::NodeAdd {
            id: 2,
            labels: vec![s("P")],
            props: vec![(s("bio"), Value::Text(s("signal lattice")))],
        },
        Op::NodeDel { id: 10 },
    ] {
        g.apply_op(op).unwrap();
    }
    g.import_legacy(&legacy(3, 20, 10)).unwrap();
    let check = |g: &mut Graph, ctx: &str| {
        let live = keyspace(g, "context");
        q(g, "CALL context.index()");
        assert!(live == keyspace(g, "context"), "{ctx}: the index drifted");
    };
    check(&mut g, "after apply_op and import_legacy");
    let r = q(&mut g, "CALL context.search(query: \"copper\")");
    assert!(r.rows.is_empty(), "a replaced name is still found");
    q(&mut g, "CLEAR");
    q(&mut g, "CREATE (:P {name: \"willow cipher\"})");
    check(&mut g, "after CLEAR");
}
