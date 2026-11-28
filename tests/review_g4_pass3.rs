//! The third review of the write hooks (G4), adopted as regression tests:
//! what a spent block's COMMIT reports matches what is durable, because an
//! operation that commits on its own is refused inside a block.

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Once};

use glider::ext::{self, Args, EdgeImage, Extension, NodeImage, Spec};
use glider::graph::Graph;
use glider::query::{self, QueryResult};
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

fn durable(g: &Graph) -> Vec<String> {
    // What a crash now would keep: the committed state, seen by a replica of the log.
    let mut v = labels(g);
    v.sort();
    v
}

/// A block opened with begin(): an import that commits in slices runs inside
/// it, then a later write fails. COMMIT says nothing was committed. What
/// is durable?
#[test]
fn pass3_nothing_was_committed_after_an_import_inside_a_block() {
    let path = std::env::temp_dir().join(format!("g4p3-import-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    register();
    {
        let mut g = Graph::open(&path, glider::store::Sync::Always).unwrap();
        q(&mut g, "CREATE EXTENSION probe2");
        mode("io");
        g.begin().unwrap();
        g.add_node(&["BlockA".into()], vec![]).unwrap();
        let e = query::import_jsonl(&mut g, "{\"type\":\"node\",\"labels\":[\"Imported\"]}\n")
            .expect_err("an import inside a block is refused");
        assert!(e.to_string().contains("end the block"), "{e}");
        let _ = g.add_node(&["Fail".into()], vec![]).unwrap_err();
        let c = g.commit().map_err(|e| e.to_string());
        println!("commit: {c:?}");
        assert!(c.unwrap_err().contains("nothing was committed"));
        std::mem::forget(g); // crash
    }
    mode("");
    let g = Graph::open_forced(&path, glider::store::Sync::Always).unwrap();
    let d = durable(&g);
    println!("after a crash, durable: {d:?}");
    drop(g);
    let _ = std::fs::remove_file(&path);
    assert!(
        d.is_empty(),
        "COMMIT said nothing was committed, but {d:?} is durable"
    );
}

/// The same with COMPACT inside the block.
#[test]
fn pass3_nothing_was_committed_after_compact_inside_a_block() {
    let mut g = graph();
    mode("io");
    q(&mut g, "BEGIN");
    q(&mut g, "CREATE (:BlockA)");
    let k = query::execute(&mut g, "COMPACT")
        .map(|r| r.message)
        .map_err(|e| e.to_string());
    assert!(k.as_ref().unwrap_err().contains("end the block"), "{k:?}");
    let _ = query::execute(&mut g, "CREATE (:Fail)")
        .err()
        .expect("refused");
    let c = query::execute(&mut g, "COMMIT")
        .err()
        .map(|e| e.to_string());
    mode("");
    println!("COMPACT: {k:?}\nCOMMIT: {c:?}\ncommitted: {:?}", labels(&g));
    assert!(
        labels(&g).is_empty(),
        "COMMIT said nothing was committed, but {:?} is",
        labels(&g)
    );
}
