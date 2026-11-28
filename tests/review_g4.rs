//! The independent review of the write hooks (G4), adopted as regression
//! tests: what a hook is told is true call by call; a hook may not write
//! the graph; a transaction the engine rolled back cannot be committed; a
//! CLEAR, even of an empty graph, counts as a change.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::{Arc, Once};

use glider::ext::{self, Args, BulkWrite, EdgeImage, Extension, NodeImage, Spec};
use glider::graph::Graph;
use glider::legacy::graph::Graph as Legacy;
use glider::query::{self, QueryResult};
use glider::store::Op;
use glider::value::Value;
use glider::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Node(u64, Option<String>, Option<String>),
    Edge(u64, Option<String>, Option<String>),
    Bulk,
}

thread_local! {
    static CALLS: RefCell<Vec<Call>> = const { RefCell::new(Vec::new()) };
    static DEPTH: Cell<u64> = const { Cell::new(0) };
    static MAX_DEPTH: Cell<u64> = const { Cell::new(0) };
    static GROW_LIMIT: Cell<u64> = const { Cell::new(0) };
    static FAIL_WITH: RefCell<Option<Error>> = const { RefCell::new(None) };
}

fn take_calls() -> Vec<Call> {
    CALLS.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

fn props_dbg(p: &[(String, Value)]) -> String {
    let mut p = p.to_vec();
    p.sort_by(|a, b| a.0.cmp(&b.0));
    format!("{p:?}")
}
fn node_dbg(labels: &[String], props: &[(String, Value)]) -> String {
    let mut l = labels.to_vec();
    l.sort();
    format!("{l:?} {}", props_dbg(props))
}
fn edge_dbg(from: u64, to: u64, t: &str, props: &[(String, Value)]) -> String {
    format!("{from}-{t}->{to} {}", props_dbg(props))
}

/// Records every call, in order, outside the database (so a sequence can
/// be checked call by call, not first-before/last-after).
struct Strict;
impl Extension for Strict {
    fn name(&self) -> &str {
        "strict"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "records every call in order"
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
        _: &mut Graph,
        id: u64,
        b: Option<&NodeImage>,
        a: Option<&NodeImage>,
    ) -> Result<()> {
        let f = |n: Option<&NodeImage>| n.map(|n| node_dbg(&n.labels, &n.props));
        CALLS.with(|c| c.borrow_mut().push(Call::Node(id, f(b), f(a))));
        Ok(())
    }
    fn on_edge_write(
        &self,
        _: &mut Graph,
        id: u64,
        b: Option<&EdgeImage>,
        a: Option<&EdgeImage>,
    ) -> Result<()> {
        if let Some(e) = FAIL_WITH.with(|f| f.borrow_mut().take()) {
            return Err(e);
        }
        let f = |e: Option<&EdgeImage>| e.map(|e| edge_dbg(e.from, e.to, &e.etype, &e.props));
        CALLS.with(|c| c.borrow_mut().push(Call::Edge(id, f(b), f(a))));
        Ok(())
    }
    fn on_bulk_write(&self, _: &mut Graph, _: BulkWrite) -> Result<()> {
        CALLS.with(|c| c.borrow_mut().push(Call::Bulk));
        Ok(())
    }
}

/// A hook that writes to the graph: every new :Grow node makes another.
struct Grower;
impl Extension for Grower {
    fn name(&self) -> &str {
        "grower"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "writes the graph from a hook"
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
        _: u64,
        b: Option<&NodeImage>,
        a: Option<&NodeImage>,
    ) -> Result<()> {
        if b.is_some() || !a.is_some_and(|n| n.labels.iter().any(|l| l == "Grow")) {
            return Ok(());
        }
        let d = DEPTH.with(|d| {
            d.set(d.get() + 1);
            d.get()
        });
        MAX_DEPTH.with(|m| m.set(m.get().max(d)));
        let limit = GROW_LIMIT.with(|l| l.get());
        let r = if d < limit {
            g.add_node(&["Grow".into()], vec![]).map(|_| ())
        } else {
            Ok(())
        };
        DEPTH.with(|d| d.set(d.get() - 1));
        r
    }
}

/// Follows edges only, does nothing: the cheapest edge-following extension.
struct Trivial;
impl Extension for Trivial {
    fn name(&self) -> &str {
        "trivial"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "follows edge writes and does nothing"
    }
    fn procedures(&self) -> &[Spec] {
        &[]
    }
    fn call(&self, _: &mut Graph, _: &str, _: &Args) -> Result<QueryResult> {
        unreachable!()
    }
    fn wants_edge_writes(&self, _: &Graph) -> bool {
        true
    }
}

fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        ext::register(Arc::new(Strict));
        ext::register(Arc::new(Grower));
        ext::register(Arc::new(Trivial));
    });
}

fn q(g: &mut Graph, src: &str) -> QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

type Obs = (BTreeMap<u64, String>, BTreeMap<u64, String>);
fn observe(g: &Graph) -> Obs {
    let n = g
        .node_ids()
        .into_iter()
        .map(|id| (id, node_dbg(&g.node_labels(id), &g.node_props(id))))
        .collect();
    let e = g
        .edge_ids()
        .into_iter()
        .map(|id| {
            let e = g.edge(id).unwrap();
            let t = g.edge_type_name(id).unwrap().to_string();
            (id, edge_dbg(e.from, e.to, &t, &g.edge_props(id)))
        })
        .collect();
    (n, e)
}

/// Replays the calls, one at a time, over the graph as it was: each call's
/// before must be the record's state at that moment, and the end state
/// must be the graph. Returns the problems found.
fn replay(before: &Obs, after: &Obs, calls: &[Call]) -> Vec<String> {
    let (mut n, mut e) = before.clone();
    let mut bad = Vec::new();
    for (i, c) in calls.iter().enumerate() {
        let (map, id, b, a, what) = match c {
            Call::Node(id, b, a) => (&mut n, *id, b, a, "node"),
            Call::Edge(id, b, a) => (&mut e, *id, b, a, "edge"),
            Call::Bulk => {
                n.clear();
                e.clear();
                continue;
            }
        };
        let now = map.get(&id).cloned();
        if &now != b {
            bad.push(format!(
                "call {i}: {what} {id} told before {b:?}, was {now:?}"
            ));
        }
        if b == a {
            bad.push(format!(
                "call {i}: {what} {id} told of a write that changed nothing"
            ));
        }
        match a {
            Some(v) => {
                map.insert(id, v.clone());
            }
            None => {
                map.remove(&id);
            }
        }
    }
    if n != after.0 {
        bad.push("nodes: replayed calls do not reach the graph".into());
    }
    if e != after.1 {
        bad.push("edges: replayed calls do not reach the graph".into());
    }
    bad
}

fn step(g: &mut Graph, name: &str, f: impl FnOnce(&mut Graph)) -> usize {
    take_calls();
    let before = observe(g);
    let rev = g.revision();
    f(g);
    let after = observe(g);
    let calls = take_calls();
    let bad = replay(&before, &after, &calls);
    assert!(bad.is_empty(), "{name}: {bad:#?}");
    assert_eq!(
        !calls.is_empty(),
        g.revision() != rev,
        "{name}: heard {} calls, revision {rev} -> {}",
        calls.len(),
        g.revision()
    );
    calls.len()
}

// --------------------------------------------------------------------------

/// What a hook is told, call by call, is what happened: self-loops,
/// parallel edges, nodes deleted together that share edges, several SETs
/// in one statement, a node with more edges than one detach batch, edges
/// and nodes overwritten by replayed ops, a legacy import over existing
/// ids, algorithm results, JSON Lines.
#[test]
fn reviewer_every_call_is_true_when_it_is_made() {
    register();
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION strict");
    step(&mut g, "create", |g| {
        q(
            g,
            r#"CREATE (a:P {name: "a"})-[:T {w: 1}]->(b:P {name: "b"})"#,
        );
        q(
            g,
            r#"MATCH (a:P {name: "a"}), (b:P {name: "b"}) CREATE (a)-[:T {w: 2}]->(b), (b)-[:U]->(a), (a)-[:SELF]->(a)"#,
        );
    });
    step(&mut g, "several sets at once", |g| {
        q(
            g,
            r#"MATCH (a:P {name: "a"}) SET a.x = 1, a.y = 2, a:L, a.x = 3"#,
        );
        q(g, r#"MATCH (a:P {name: "a"}) REMOVE a.x, a:L, a.zz"#);
        q(g, "MATCH ()-[r:T]->() SET r.w = 5, r.v = r.w");
        q(g, "MATCH ()-[r:SELF]->() SET r.s = 1");
        q(g, "MATCH ()-[r:SELF]->() REMOVE r.s, r.none");
    });
    step(&mut g, "two nodes sharing edges, one statement", |g| {
        q(
            g,
            "MATCH (a:P), (b:P) WHERE a.name = \"a\" AND b.name = \"b\" DETACH DELETE a, b",
        );
    });
    // A hub with more edges than one detach batch, self-loops among them.
    let hub = g.add_node(&["Hub".into()], vec![]).unwrap();
    let leaf = g.add_node(&["Leaf".into()], vec![]).unwrap();
    g.autocommit = false;
    for i in 0..9000u64 {
        if i % 997 == 0 {
            g.add_edge(hub, hub, "LOOP", vec![]).unwrap();
        } else if i % 2 == 0 {
            g.add_edge(hub, leaf, "OUT", vec![("i".into(), Value::Int(i as i64))])
                .unwrap();
        } else {
            g.add_edge(leaf, hub, "IN", vec![]).unwrap();
        }
    }
    g.commit().unwrap();
    g.autocommit = true;
    let n = step(&mut g, "delete a node with 9000 edges", |g| {
        q(g, "MATCH (h:Hub) DETACH DELETE h");
    });
    assert_eq!(n, 9001);
    // Replayed operations overwriting what exists.
    q(
        &mut g,
        "CREATE (:R {i: 1})-[:E {k: 1}]->(:R {i: 2}), (:R {i: 3})",
    );
    let rs = g.nodes_with_label("R");
    let e = g.edges_with_type("E")[0];
    step(&mut g, "apply_op overwrites", |g| {
        g.apply_op(Op::EdgeAdd {
            id: e,
            from: rs[2],
            to: rs[2],
            etype: "MOVED".into(),
            props: vec![("z".into(), Value::Int(9))],
        })
        .unwrap();
        g.apply_op(Op::NodeAdd {
            id: rs[0],
            labels: vec!["Over".into()],
            props: vec![],
        })
        .unwrap();
        g.apply_op(Op::NodeDel { id: rs[2] }).unwrap();
    });
    // A legacy import over existing ids.
    let mut l = Legacy::memory();
    for i in 0..30 {
        l.add_node(&["Old".into()], vec![("i".into(), Value::Int(i))])
            .unwrap();
    }
    for i in 1..30u64 {
        l.add_edge(i, i + 1, "OLD", vec![]).unwrap();
        l.add_edge(i, i, "OLDSELF", vec![]).unwrap();
    }
    step(&mut g, "legacy import over existing ids", |g| {
        g.import_legacy(&l).unwrap();
    });
    step(&mut g, "algorithm write", |g| {
        q(g, r#"CALL pagerank(iterations: 5, write: "rank")"#);
    });
    step(&mut g, "jsonl", |g| {
        query::import_jsonl(
            g,
            "{\"type\":\"node\",\"id\":1,\"labels\":[\"J\"]}\n{\"type\":\"node\",\"id\":2}\n{\"type\":\"edge\",\"from\":1,\"to\":1,\"label\":\"JS\"}\n{\"type\":\"edge\",\"from\":1,\"to\":2}\n",
        )
        .unwrap();
    });
    step(&mut g, "CLEAR", |g| {
        q(g, "CLEAR");
    });
}

/// A CLEAR counts as a change on an empty graph too: it resets the id
/// counters and wipes derived data, so it moves the revision and is heard.
#[test]
fn reviewer_clear_of_an_empty_graph_moves_the_revision_and_calls_the_bulk_hook() {
    register();
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION strict");
    let rev = g.revision();
    take_calls();
    q(&mut g, "CLEAR");
    let calls = take_calls();
    println!(
        "empty CLEAR: revision {rev} -> {}, calls {calls:?}",
        g.revision()
    );
    let mut plain = Graph::memory();
    let rev2 = plain.revision();
    q(&mut plain, "CLEAR");
    println!(
        "empty CLEAR, no extension: revision {rev2} -> {}",
        plain.revision()
    );
    assert!(g.revision() > rev, "an empty CLEAR moves the revision");
    assert_eq!(calls, vec![Call::Bulk], "an empty CLEAR is heard");
    assert!(plain.revision() > rev2);
}

/// A hook that writes to the graph is refused at once: no nesting, and the
/// statement fails with nothing left behind.
#[test]
fn reviewer_a_hook_that_writes_the_graph_is_refused_without_nesting() {
    register();
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION grower");
    GROW_LIMIT.with(|l| l.set(300));
    MAX_DEPTH.with(|m| m.set(0));
    let e = query::execute(&mut g, "CREATE (:Grow)")
        .err()
        .expect("refused");
    assert!(
        e.to_string().contains("may write only extension keyspaces"),
        "{e}"
    );
    let depth = MAX_DEPTH.with(|m| m.get());
    println!(
        "one CREATE made {} nodes; hooks nested {depth} deep",
        g.node_count()
    );
    assert_eq!(g.node_count(), 0);
    assert_eq!(depth, 1);
}

#[test]
#[ignore]
fn reviewer_child_hook_writes_forever() {
    register();
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION grower");
    GROW_LIMIT.with(|l| l.set(u64::MAX));
    let _ = query::execute(&mut g, "CREATE (:Grow)");
    println!("returned");
}

#[test]
fn reviewer_a_hook_that_writes_forever_leaves_the_process_alive() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "reviewer_child_hook_writes_forever",
            "--test-threads=1",
            "--nocapture",
        ])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    println!("child status: {:?}", out.status);
    println!(
        "child stderr: {}",
        err.lines()
            .filter(|l| l.contains("overflow") || l.contains("abort"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    assert!(out.status.success(), "the process died: {:?}", out.status);
}

/// With autocommit off, a failing hook rolls the whole open transaction
/// back and the error says so, whatever its kind; the block is spent, so
/// nothing after it runs and COMMIT commits nothing.
#[test]
fn reviewer_a_hook_failing_with_an_io_error_says_the_transaction_was_rolled_back_and_spends_it() {
    register();
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION strict");
    q(&mut g, "BEGIN");
    q(&mut g, "CREATE (:Earlier)");
    q(&mut g, "CREATE (:Earlier2)");
    FAIL_WITH.with(|f| {
        *f.borrow_mut() = Some(Error::Io(std::io::Error::other("hook storage unavailable")))
    });
    let e = query::execute(&mut g, "CREATE (:A)-[:E]->(:B)")
        .err()
        .expect("refused");
    assert!(matches!(e, Error::Io(_)), "the kind is kept: {e:?}");
    let after = query::execute(&mut g, "CREATE (:After)")
        .err()
        .expect("spent");
    assert!(
        after.to_string().contains("end it with ROLLBACK"),
        "{after}"
    );
    let commit = query::execute(&mut g, "COMMIT").err().expect("spent");
    assert!(
        commit.to_string().starts_with("nothing was committed"),
        "{commit}"
    );
    let labels: Vec<_> = g
        .node_ids()
        .into_iter()
        .flat_map(|id| g.node_labels(id))
        .collect();
    println!("error: {e}");
    println!("COMMIT said: {commit}; committed labels {labels:?}");
    assert!(labels.is_empty(), "{labels:?}");
    assert!(
        e.to_string().contains("rolled back"),
        "the caller was not told its transaction is gone: {e}"
    );
}

thread_local! {
    static GATED: Cell<u64> = const { Cell::new(0) };
    static HOG: Cell<bool> = const { Cell::new(false) };
}

/// Follows edges only while its configuration key `!on` is set; with HOG,
/// writes far more than a small memory graph can hold.
struct Gated;
impl Extension for Gated {
    fn name(&self) -> &str {
        "gated"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "follows edges while configured"
    }
    fn procedures(&self) -> &[Spec] {
        &[]
    }
    fn call(&self, _: &mut Graph, _: &str, _: &Args) -> Result<QueryResult> {
        unreachable!()
    }
    fn wants_edge_writes(&self, g: &Graph) -> bool {
        g.ext_get("gated", b"!on").is_some()
    }
    fn on_edge_write(
        &self,
        g: &mut Graph,
        id: u64,
        _: Option<&EdgeImage>,
        _: Option<&EdgeImage>,
    ) -> Result<()> {
        GATED.with(|c| c.set(c.get() + 1));
        if HOG.with(|h| h.get()) {
            for i in 0..4096u64 {
                g.ext_write(
                    "gated",
                    vec![(i.to_be_bytes().to_vec(), vec![7u8; 64 << 10])],
                    Vec::new(),
                )?;
            }
        }
        g.ext_write(
            "gated",
            vec![(id.to_be_bytes().to_vec(), Vec::new())],
            Vec::new(),
        )
    }
}

fn gated() -> u64 {
    GATED.with(|c| c.get())
}

#[test]
fn reviewer_the_hook_cache_follows_installs_drops_and_config_through_rollbacks() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| ext::register(Arc::new(Gated)));
    let mut g = Graph::memory();
    let a = g.add_node(&[], vec![]).unwrap();
    let on = |g: &mut Graph| {
        g.ext_write("gated", vec![(b"!on".to_vec(), Vec::new())], Vec::new())
            .unwrap()
    };
    let mut log = Vec::new();
    let mut probe = |g: &mut Graph, what: &str, want: bool| {
        let c = gated();
        g.add_edge(a, a, "X", vec![]).unwrap();
        let heard = gated() > c;
        log.push(format!("{what}: heard {heard}"));
        assert_eq!(heard, want, "{what}");
    };
    probe(&mut g, "not installed", false);
    q(&mut g, "BEGIN");
    q(&mut g, "CREATE EXTENSION gated");
    on(&mut g);
    probe(&mut g, "installed and on inside BEGIN", true);
    q(&mut g, "ROLLBACK");
    probe(&mut g, "after ROLLBACK of the install", false);
    q(&mut g, "CREATE EXTENSION gated");
    probe(&mut g, "installed, off", false);
    on(&mut g);
    probe(&mut g, "on", true);
    q(&mut g, "BEGIN");
    g.ext_write("gated", Vec::new(), vec![b"!on".to_vec()])
        .unwrap();
    probe(&mut g, "off inside BEGIN", false);
    q(&mut g, "ROLLBACK");
    probe(&mut g, "after ROLLBACK of off", true);
    q(&mut g, "BEGIN");
    q(&mut g, "DROP EXTENSION gated");
    probe(&mut g, "dropped inside BEGIN", false);
    q(&mut g, "ROLLBACK");
    probe(&mut g, "after ROLLBACK of the drop", true);
    q(&mut g, "CLEAR");
    let b = g.add_node(&[], vec![]).unwrap();
    let c = gated();
    g.add_edge(b, b, "X", vec![]).unwrap();
    log.push(format!("after CLEAR (config kept): heard {}", gated() > c));
    assert!(gated() > c);
    g.apply_op(Op::ExtWrite {
        puts: vec![],
        dels: vec![b"gated\0!on".to_vec()],
    })
    .unwrap();
    let c = gated();
    g.add_edge(b, b, "X", vec![]).unwrap();
    log.push(format!(
        "after a replayed ExtWrite turning it off: heard {}",
        gated() > c
    ));
    assert_eq!(gated(), c);
    println!("{}", log.join("\n"));
}

/// A hook that runs the graph out of memory inside BEGIN: the error, the
/// handle afterwards, and what COMMIT makes durable.
#[test]
fn reviewer_a_hook_out_of_memory_inside_begin_spends_the_block() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| ext::register(Arc::new(Gated)));
    let mut g = Graph::memory_with_limit(32 << 20);
    q(&mut g, "CREATE EXTENSION gated");
    g.ext_write("gated", vec![(b"!on".to_vec(), Vec::new())], Vec::new())
        .unwrap();
    q(&mut g, "CREATE (:Committed)");
    q(&mut g, "BEGIN");
    q(&mut g, "CREATE (:Earlier)");
    HOG.with(|h| h.set(true));
    let e = query::execute(&mut g, "CREATE (:A)-[:E]->(:B)")
        .err()
        .expect("full");
    HOG.with(|h| h.set(false));
    println!("error: {e} (Full: {})", matches!(e, Error::Full(_)));
    assert!(query::execute(&mut g, "CREATE (:After)-[:E]->(:After)").is_err());
    let c = query::execute(&mut g, "COMMIT").err().expect("spent");
    let labels: Vec<_> = g
        .node_ids()
        .into_iter()
        .flat_map(|id| g.node_labels(id))
        .collect();
    let keys = {
        let mut n = 0;
        g.ext_scan("gated", b"", &mut |_, _| {
            n += 1;
            true
        });
        n
    };
    println!(
        "COMMIT: {c}; labels {labels:?}; gated keys {keys}; edges {}",
        g.edge_count()
    );
    assert!(e.to_string().contains("rolled back"));
    // Nothing of the spent block commits; the handle is usable after it.
    assert_eq!(labels, vec!["Committed"]);
    assert_eq!(g.edge_count(), 0);
    q(&mut g, "CREATE (:After)-[:E]->(:After)");
    assert_eq!(g.edge_count(), 1);
}

/// Replayed ops on records that do not exist, with a key or label the
/// database has never seen: the revision moves; what do hooks hear?
#[test]
fn reviewer_replayed_ops_on_absent_records_with_new_names() {
    register();
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION strict");
    for op in [
        Op::EdgeSet {
            id: 777,
            key: "brand_new_key".into(),
            value: Value::Int(1),
        },
        Op::LabelAdd {
            id: 778,
            label: "BrandNewLabel".into(),
        },
        Op::NodeSet {
            id: 779,
            key: "another_new_key".into(),
            value: Value::Int(1),
        },
    ] {
        take_calls();
        let rev = g.revision();
        let what = format!("{op:?}");
        g.apply_op(op).unwrap();
        let calls = take_calls();
        println!(
            "{what}: revision {rev} -> {}, calls {calls:?}",
            g.revision()
        );
        // Nothing before and nothing after: nothing to report, even when a
        // newly seen name moved the revision.
        assert!(calls.is_empty(), "{what}: {calls:?}");
    }
}
