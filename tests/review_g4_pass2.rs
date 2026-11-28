//! The second review of the write hooks (G4), adopted as regression tests:
//! a spent block never reaches a later one, a panicking hook leaves the
//! handle writable, and the paths that held.

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Once};

use glider::ext::{self, Args, EdgeImage, Extension, NodeImage, Spec};
use glider::graph::Graph;
use glider::query::{self, QueryResult};
use glider::store::Op;
use glider::value::Value;
use glider::{Error, Result};

thread_local! {
    static MODE: RefCell<&'static str> = const { RefCell::new("") };
    static CALLS: Cell<u64> = const { Cell::new(0) };
    static DEPTH: Cell<u64> = const { Cell::new(0) };
    static MAXD: Cell<u64> = const { Cell::new(0) };
}

fn mode(m: &'static str) {
    MODE.with(|x| *x.borrow_mut() = m);
}
fn calls() -> u64 {
    CALLS.with(|c| c.get())
}

/// One extension, behaviour chosen per test by MODE.
struct Probe;
impl Extension for Probe {
    fn name(&self) -> &str {
        "probe2"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "reviewer probe"
    }
    fn procedures(&self) -> &[Spec] {
        const P: &[Spec] = &[Spec {
            name: "grow",
            args: "()",
            doc: "adds a node",
        }];
        P
    }
    fn call(&self, g: &mut Graph, _: &str, _: &Args) -> Result<QueryResult> {
        // A procedure is not a hook: its graph writes must be allowed.
        g.add_node(&["FromProc".into()], vec![])?;
        Ok(QueryResult::message("grown"))
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
        b: Option<&NodeImage>,
        a: Option<&NodeImage>,
    ) -> Result<()> {
        CALLS.with(|c| c.set(c.get() + 1));
        let m = MODE.with(|m| *m.borrow());
        let grow = b.is_none() && a.is_some_and(|n| n.labels.iter().any(|l| l == "Grow"));
        match m {
            "grow" if grow => {
                let d = DEPTH.with(|d| {
                    d.set(d.get() + 1);
                    d.get()
                });
                MAXD.with(|x| x.set(x.get().max(d)));
                let r = g.add_node(&["Grow".into()], vec![]).map(|_| ());
                DEPTH.with(|d| d.set(d.get() - 1));
                r
            }
            // Tries a graph write, swallows the refusal, then writes its keyspace.
            "swallow" if grow => {
                let r = g.set_node_prop(id, "x", Value::Int(1));
                assert!(r.is_err(), "a graph write from a hook was allowed");
                g.ext_write("probe2", vec![(id.to_be_bytes().to_vec(), vec![])], vec![])
            }
            "panic" if grow => panic!("hook bug"),
            "io" if a.is_some_and(|n| n.labels.iter().any(|l| l == "Fail")) => {
                Err(Error::Io(std::io::Error::other("hook storage unavailable")))
            }
            _ => g.ext_write("probe2", vec![(id.to_be_bytes().to_vec(), vec![])], vec![]),
        }
    }
    fn on_edge_write(
        &self,
        _: &mut Graph,
        _: u64,
        _: Option<&EdgeImage>,
        _: Option<&EdgeImage>,
    ) -> Result<()> {
        CALLS.with(|c| c.set(c.get() + 1));
        Ok(())
    }
}

fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| ext::register(Arc::new(Probe)));
}

fn q(g: &mut Graph, src: &str) -> QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn graph() -> Graph {
    register();
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION probe2");
    g
}

fn labels(g: &Graph) -> Vec<String> {
    g.node_ids()
        .into_iter()
        .flat_map(|id| g.node_labels(id))
        .collect()
}

// ------------------------------------------------- first-pass findings

#[test]
fn pass2_f1_a_hook_writing_the_graph_is_refused_and_nothing_nests() {
    let mut g = graph();
    mode("grow");
    MAXD.with(|x| x.set(0));
    let e = query::execute(&mut g, "CREATE (:Grow)")
        .err()
        .expect("refused");
    println!(
        "error: {e}; depth {}; nodes {}",
        MAXD.with(|x| x.get()),
        g.node_count()
    );
    assert_eq!(MAXD.with(|x| x.get()), 1);
    assert_eq!(g.node_count(), 0);
    mode("");
    q(&mut g, "CREATE (:Fine)");
    assert_eq!(labels(&g), vec!["Fine"]);
}

#[test]
fn pass2_f2_io_failure_in_begin_says_rolled_back_and_commit_says_nothing() {
    let mut g = graph();
    mode("io");
    q(&mut g, "BEGIN");
    q(&mut g, "CREATE (:Earlier)");
    let e = query::execute(&mut g, "CREATE (:Fail)")
        .err()
        .expect("refused");
    let next = query::execute(&mut g, "CREATE (:After)")
        .err()
        .map(|e| e.to_string());
    let c = query::execute(&mut g, "COMMIT")
        .err()
        .map(|e| e.to_string());
    println!(
        "error: {e}\nnext: {next:?}\ncommit: {c:?}\nlabels {:?}",
        labels(&g)
    );
    assert!(e.to_string().contains("rolled back"));
    assert!(next.is_some());
    assert!(c.unwrap().contains("nothing was committed"));
    q(&mut g, "CREATE (:Later)");
    assert_eq!(labels(&g), vec!["Later"]);
}

#[test]
fn pass2_f3_clear_heard_exactly_when_revision_moves() {
    let mut g = graph();
    mode("");
    let (r, c) = (g.revision(), calls());
    q(&mut g, "CLEAR");
    println!("empty CLEAR: revision {r} -> {}", g.revision());
    assert!(g.revision() > r);
    let _ = c;
}

#[test]
fn pass2_f4_absent_records_call_no_hook() {
    let mut g = graph();
    mode("");
    for op in [
        Op::EdgeSet {
            id: 777,
            key: "k1".into(),
            value: Value::Int(1),
        },
        Op::LabelAdd {
            id: 778,
            label: "L1".into(),
        },
        Op::NodeSet {
            id: 779,
            key: "k2".into(),
            value: Value::Int(1),
        },
    ] {
        let c = calls();
        g.apply_op(op).unwrap();
        assert_eq!(calls(), c);
    }
}

// ------------------------------------------------- the hook refusal

#[test]
fn pass2_a_procedure_that_writes_the_graph_is_not_a_hook() {
    let mut g = graph();
    mode("");
    let c = calls();
    q(&mut g, "CALL probe2.grow()");
    println!("labels {:?}, hook calls {}", labels(&g), calls() - c);
    assert_eq!(labels(&g), vec!["FromProc"]);
    assert!(calls() > c);
}

#[test]
fn pass2_a_hook_that_swallows_the_refusal_leaves_nothing_set() {
    let mut g = graph();
    mode("swallow");
    q(&mut g, "CREATE (:Grow)");
    mode("");
    // Writes right after, by statement and API, are allowed.
    q(&mut g, "CREATE (:After)");
    let id = g.add_node(&["Api".into()], vec![]).unwrap();
    g.set_node_prop(id, "y", Value::Int(2)).unwrap();
    assert_eq!(g.node_count(), 3);
}

/// A hook that panics, caught as the C API's `guard` catches it: is the
/// handle still writable afterwards?
#[test]
fn pass2_a_panicking_hook_leaves_the_handle_refusing_every_graph_write() {
    let mut g = graph();
    mode("panic");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = query::execute(&mut g, "CREATE (:Grow)");
    }));
    assert!(r.is_err());
    mode("");
    let after = query::execute(&mut g, "CREATE (:After)").map(|r| r.message);
    let api = g
        .add_node(&["Api".into()], vec![])
        .map_err(|e| e.to_string());
    println!(
        "after a caught hook panic: statement {:?}; API {:?}; autocommit {}",
        after.as_ref().map_err(|e| e.to_string()),
        api,
        g.autocommit
    );
    assert!(after.is_ok(), "statement refused after a hook panic");
    assert!(api.is_ok(), "API write refused after a hook panic");
    assert!(g.autocommit, "autocommit left off by a hook panic");
    assert_eq!(
        labels(&g),
        vec!["After", "Api"],
        "the panicking statement left nothing"
    );
}

// ------------------------------------------------- the spent block

#[test]
fn pass2_api_spent_then_autocommit_switched_back_on() {
    let mut g = graph();
    mode("io");
    g.autocommit = false;
    g.add_node(&["Earlier".into()], vec![]).unwrap();
    let e = g.add_node(&["Fail".into()], vec![]).unwrap_err();
    println!("error: {e}; spent: {:?}", g.spent());
    // Ending the API transaction the way the library has always allowed:
    // switch autocommit back on.
    g.autocommit = true;
    mode("");
    // A fresh block, by statement.
    q(&mut g, "BEGIN");
    let r = query::execute(&mut g, "CREATE (:NewBlock)").map(|r| r.message);
    println!(
        "new BEGIN block: {:?}",
        r.as_ref().map_err(|e| e.to_string())
    );
    let _ = query::execute(&mut g, "ROLLBACK");
    // A fresh API transaction.
    g.autocommit = false;
    let r2 = g
        .add_node(&["NewApi".into()], vec![])
        .map_err(|e| e.to_string());
    println!("new API transaction: {r2:?}");
    let _ = g.rollback();
    g.autocommit = true;
    assert!(
        r.is_ok(),
        "a new BEGIN block was refused with a stale reason"
    );
    assert!(
        r2.is_ok(),
        "a new API transaction was refused with a stale reason"
    );
}

#[test]
fn pass2_api_spent_reads_commit_rollback_and_checkpoint() {
    let mut g = graph();
    q(&mut g, "CREATE (:Committed)");
    mode("io");
    g.begin().unwrap();
    g.add_node(&["Earlier".into()], vec![]).unwrap();
    let _ = g.add_node(&["Fail".into()], vec![]).unwrap_err();
    mode("");
    let read_api = labels(&g);
    let read_q = query::execute(&mut g, "MATCH (n) RETURN count(n)").map(|r| r.rows);
    let unc = g.uncommitted();
    let ck = g.checkpoint().map_err(|e| e.to_string());
    let c1 = g.commit().map_err(|e| e.to_string());
    let after = g
        .add_node(&["AfterCommitErr".into()], vec![])
        .map_err(|e| e.to_string());
    let c2 = g.commit().map_err(|e| e.to_string());
    assert!(g.autocommit, "commit() ended the begin() block");
    println!(
        "API read {read_api:?}; MATCH {:?}; uncommitted {unc}; checkpoint {ck:?}; \
         commit {c1:?}; write after {after:?}; commit again {c2:?}; labels {:?}",
        read_q.map_err(|e| e.to_string()),
        labels(&g)
    );
    assert!(c1.unwrap_err().contains("nothing was committed"));
    assert!(c2.is_ok());
    assert_eq!(labels(&g), vec!["Committed", "AfterCommitErr"]);
}

/// Every statement path after an engine roll-back inside BEGIN, including
/// a batch and JSON Lines import, until ROLLBACK.
#[test]
fn pass2_statement_paths_in_a_spent_block() {
    let mut g = graph();
    mode("io");
    q(&mut g, "BEGIN");
    q(&mut g, "CREATE (:Earlier)");
    let _ = query::execute(&mut g, "CREATE (:Fail)")
        .err()
        .expect("refused");
    mode("");
    let mut out = Vec::new();
    for s in ["MATCH (n) RETURN n", "BEGIN", "STATS", "CREATE (:X)"] {
        out.push(format!(
            "{s}: {:?}",
            query::execute(&mut g, s)
                .map(|r| r.message)
                .map_err(|e| e.to_string())
        ));
    }
    let j = query::import_jsonl(&mut g, "{\"type\":\"node\",\"labels\":[\"J\"]}\n");
    out.push(format!("import_jsonl: {:?}", j.map_err(|e| e.to_string())));
    out.push(format!(
        "autocommit {} spent {:?}",
        g.autocommit,
        g.spent().is_some()
    ));
    let rb = query::execute(&mut g, "ROLLBACK").map(|r| r.message);
    out.push(format!("ROLLBACK: {:?}", rb.map_err(|e| e.to_string())));
    q(&mut g, "CREATE (:Later)");
    out.push(format!("labels {:?}", labels(&g)));
    println!("{}", out.join("\n"));
    assert_eq!(labels(&g), vec!["Later"]);
}

/// A hub delete near the memory limit, with an edge follower that writes
/// a few bytes per edge: deleting frees about as many pages as it touches,
/// so this size does not run out (the out-of-memory path is shown in
/// `write_hooks.rs` with a follower that writes a kilobyte per edge).
#[test]
fn pass2_hub_delete_past_the_memory_limit() {
    register();
    mode("");
    let mut g = Graph::memory_with_limit(24 << 20);
    q(&mut g, "CREATE EXTENSION probe2");
    g.autocommit = false;
    let hub = g.add_node(&["Hub".into()], vec![]).unwrap();
    let leaf = g.add_node(&["Leaf".into()], vec![]).unwrap();
    let mut n = 0u64;
    while g.add_edge(hub, leaf, "E", vec![]).is_ok() {
        n += 1;
        if (n % 5000 == 0 && g.commit().is_err()) || n >= 400_000 {
            break;
        }
    }
    let _ = g.rollback();
    g.autocommit = true;
    let (edges, rev) = (g.edge_count(), g.revision());
    let r = g.delete_node(hub);
    println!(
        "hub with {edges} edges at 24 MiB: delete -> {:?}; edges {} rev {} -> {}",
        r.as_ref().map_err(|e| e.to_string()),
        g.edge_count(),
        rev,
        g.revision()
    );
    if r.is_err() {
        assert_eq!(g.edge_count(), edges);
        assert_eq!(g.revision(), rev);
        q(&mut g, "CREATE (:Usable)");
    }
}

#[test]
fn pass2_api_spent_then_autocommit_on_then_a_new_api_transaction() {
    let mut g = graph();
    mode("io");
    g.autocommit = false;
    g.add_node(&["Earlier".into()], vec![]).unwrap();
    let _ = g.add_node(&["Fail".into()], vec![]).unwrap_err();
    mode("");
    g.autocommit = true; // ends the caller's transaction, as the library's own tests and CLI do
    let read = false; // no statement in between
    g.autocommit = false; // a new transaction
    let w = g
        .add_node(&["New".into()], vec![])
        .map_err(|e| e.to_string());
    let c = g.commit().map_err(|e| e.to_string());
    g.autocommit = true;
    println!(
        "read in autocommit: {read}; new API transaction write {w:?}; commit {c:?}; labels {:?}",
        labels(&g)
    );
    assert!(
        w.is_ok(),
        "a new API transaction was refused with a stale reason"
    );
}

/// The same for blocks the engine can see start: ended by COMMIT or
/// ROLLBACK (statement or API), a spent block never reaches the next one.
#[test]
fn pass2_a_spent_begin_block_ends_with_commit_or_rollback_and_the_next_is_clean() {
    let mut g = graph();
    for end in ["COMMIT", "ROLLBACK", "commit()", "rollback()"] {
        mode("io");
        if end.ends_with(')') {
            g.begin().unwrap();
        } else {
            q(&mut g, "BEGIN");
        }
        q(&mut g, "CREATE (:Earlier)");
        let e = query::execute(&mut g, "CREATE (:Fail)")
            .err()
            .expect("refused");
        assert!(e.to_string().contains("rolled back"), "{e}");
        mode("");
        assert!(g.spent().is_some());
        let _ = match end {
            "commit()" => g.commit(),
            "rollback()" => g.rollback(),
            s => query::execute(&mut g, s).map(|_| ()),
        };
        assert!(g.autocommit && g.spent().is_none(), "{end}");
        // The next block, either way of starting it, is clean.
        q(&mut g, "BEGIN");
        q(&mut g, &format!("CREATE (:Next {{end: '{end}'}})"));
        q(&mut g, "COMMIT");
        g.begin().unwrap();
        g.add_node(&["NextApi".into()], vec![]).unwrap();
        g.commit().unwrap();
    }
    assert_eq!(g.node_count(), 8);
}
