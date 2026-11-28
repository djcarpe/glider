//! A statement stopped by its budget, inside the transaction rules of the
//! write hooks: a block the engine rolls back is spent whatever the cause,
//! a stopped statement that wrote nothing leaves the block open, nothing a
//! hook wrote for a stopped statement survives, and one panic through both
//! a budgeted procedure and a hook leaves the thread and the handle clean.
use std::cell::Cell;
use std::sync::{Arc, Once};

use glider::ext::{self, Args, Extension, NodeImage, Spec};
use glider::query::{self, QueryLimits, QueryResult};
use glider::{Graph, Result, Value};

thread_local! {
    /// 1: the hook panics when a node gets a `rank`.
    static PANIC: Cell<u8> = const { Cell::new(0) };
}

/// Follows every node write into its own keyspace, as a derived index
/// would; can be told to panic; and has procedures that panic or run a
/// statement of their own.
struct Follow;
impl Extension for Follow {
    fn name(&self) -> &str {
        "follow"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "test extension that follows node writes"
    }
    fn procedures(&self) -> &[Spec] {
        const P: &[Spec] = &[
            Spec {
                name: "boom",
                args: "()",
                doc: "counts, then panics",
            },
            Spec {
                name: "nested",
                args: "(steps)",
                doc: "adds a node, then sets k on every node through a statement of its own, ignoring its error",
            },
        ];
        P
    }
    fn call(&self, g: &mut Graph, p: &str, a: &Args) -> Result<QueryResult> {
        match p {
            "boom" => {
                let _ = ext::steps(10);
                let _ = glider::ooc::StateVec::new(1000, 0u64);
                panic!("procedure bug");
            }
            _ => {
                let lim = QueryLimits {
                    max_steps: Some(a.usize("steps", 50) as u64),
                    ..Default::default()
                };
                g.add_node(&["Inner".into()], vec![])?;
                let r = query::execute_capped(g, "MATCH (n:N) SET n.k = 1", &[], lim);
                Ok(QueryResult::message(format!("inner: {:?}", r.err())))
            }
        }
    }
    fn wants_writes(&self, _: &Graph) -> bool {
        true
    }
    fn on_node_write(
        &self,
        g: &mut Graph,
        id: u64,
        _: Option<&NodeImage>,
        after: Option<&NodeImage>,
    ) -> Result<()> {
        let ranked = after.is_some_and(|n| n.props.iter().any(|(k, _)| k == "rank"));
        if ranked && PANIC.with(|p| p.get()) == 1 {
            panic!("hook bug");
        }
        let key = id.to_be_bytes().to_vec();
        match after {
            Some(_) => g.ext_write("follow", vec![(key, vec![1])], vec![]),
            None => g.ext_write("follow", vec![], vec![key]),
        }
    }
}

fn graph(n: usize) -> Graph {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| ext::register(Arc::new(Follow)));
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION follow");
    q(&mut g, "CREATE EXTENSION context");
    g.autocommit = false;
    let ids: Vec<u64> = (0..n)
        .map(|i| {
            g.add_node(
                &["N".into()],
                vec![
                    ("i".into(), Value::Int(i as i64)),
                    ("name".into(), Value::from(format!("pump {i}"))),
                ],
            )
            .unwrap()
        })
        .collect();
    for i in 1..n {
        g.add_edge(ids[i / 2], ids[i], "E", vec![]).unwrap();
        g.add_edge(ids[i], ids[(i * 31) % n], "F", vec![]).unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    q(&mut g, "CALL context.index()");
    g
}

fn q(g: &mut Graph, src: &str) -> QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn steps(n: u64) -> QueryLimits {
    QueryLimits {
        max_steps: Some(n),
        ..Default::default()
    }
}

/// Everything a statement or a hook could have left: the graph, both
/// extension keyspaces.
fn state(g: &Graph) -> (String, Vec<(Vec<u8>, Vec<u8>)>) {
    let mut ext = Vec::new();
    for ns in ["follow", "context"] {
        g.ext_scan(ns, b"", &mut |k, v| {
            ext.push((k.to_vec(), v.to_vec()));
            true
        });
    }
    (query::export_jsonl(g), ext)
}

const WRITE: &str = "CALL pagerank(write: \"rank\")";

/// The fewest steps `src` needs; it is stopped with fewer. Leaves nothing.
fn needed(g: &mut Graph, src: &str) -> u64 {
    let (mut lo, mut hi) = (0u64, 1u64 << 40);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if query::execute_capped(g, src, &[], steps(mid)).is_ok() {
            hi = mid;
            q(g, "MATCH (n) REMOVE n.rank");
        } else {
            lo = mid;
        }
    }
    hi
}

fn stopped_writing(g: &mut Graph, budget: u64) -> String {
    query::execute_capped(g, WRITE, &[], steps(budget))
        .err()
        .expect("stopped")
        .to_string()
}

// Stopped part-way through its writes (and its hooks' writes) inside a
// block, by BEGIN or by Graph::begin: the block is spent, as for a failed
// hook. The error says so, later statements and writes are refused, COMMIT
// says nothing was committed, nothing is left, and the next block is fine.
#[test]
fn a_stopped_write_spends_a_block_as_any_failure_does() {
    let mut g = graph(1_500);
    let late = needed(&mut g, WRITE) - 300;
    let base = state(&g);
    for api in [false, true] {
        if api {
            g.begin().unwrap();
            g.add_node(&["Mark".into()], vec![]).unwrap();
        } else {
            q(&mut g, "BEGIN");
            q(&mut g, "CREATE (:Mark)");
        }
        let e = stopped_writing(&mut g, late);
        assert!(e.contains("matching steps"), "{e}");
        assert!(e.contains("rolled back"), "{e}");
        assert!(g.spent().is_some(), "api {api}: the block is spent");
        assert!(query::execute(&mut g, "MATCH (n) RETURN count(n)").is_err());
        assert!(g.add_node(&["Late".into()], vec![]).is_err());
        let commit = if api {
            g.commit().err().map(|e| e.to_string())
        } else {
            query::execute(&mut g, "COMMIT")
                .err()
                .map(|e| e.to_string())
        };
        let commit = commit.expect("COMMIT of a spent block is an error");
        assert!(commit.contains("nothing was committed"), "{commit}");
        assert!(g.autocommit);
        assert!(state(&g) == base, "api {api}: something was left");
        // the spent state does not reach the next block
        if api {
            g.begin().unwrap();
            g.add_node(&["Next".into()], vec![]).unwrap();
            g.commit().unwrap();
        } else {
            q(&mut g, "BEGIN");
            q(&mut g, "CREATE (:Next)");
            q(&mut g, "COMMIT");
        }
        q(&mut g, "MATCH (n:Next) DELETE n");
    }
}

// A stopped statement that had written nothing leaves the block open and
// usable, by BEGIN or by Graph::begin; so does one stopped before its
// writes began.
#[test]
fn a_stopped_statement_that_wrote_nothing_leaves_the_block_open() {
    let mut g = graph(1_500);
    for api in [false, true] {
        for src in ["CALL pagerank()", WRITE] {
            if api {
                g.begin().unwrap();
                g.add_node(&["Mark".into()], vec![]).unwrap();
            } else {
                q(&mut g, "BEGIN");
                q(&mut g, "CREATE (:Mark)");
            }
            let e = query::execute_capped(&mut g, src, &[], steps(1_000))
                .err()
                .expect("stopped")
                .to_string();
            assert!(!e.contains("rolled back"), "{src}: {e}");
            assert!(g.spent().is_none());
            q(&mut g, "CREATE (:Mark)");
            if api {
                g.commit().unwrap();
            } else {
                q(&mut g, "COMMIT");
            }
            let r = q(&mut g, "MATCH (m:Mark) RETURN count(m)");
            assert_eq!(r.rows[0][0], Value::Int(2), "{src}");
            q(&mut g, "MATCH (m:Mark) DELETE m");
        }
    }
}

// In a transaction opened by assigning the field, a stopped write rolls
// that transaction back and says so, and the caller carries on in a new
// one: nothing is spent.
#[test]
fn a_stopped_write_in_a_field_opened_transaction_goes_on_in_a_new_one() {
    let mut g = graph(1_500);
    let late = needed(&mut g, WRITE) - 300;
    let base = state(&g);
    g.autocommit = false;
    g.add_node(&["Mark".into()], vec![]).unwrap();
    let e = stopped_writing(&mut g, late);
    assert!(e.contains("rolled back"), "{e}");
    assert!(g.spent().is_none());
    assert!(state(&g) == base, "the stopped write left something");
    g.add_node(&["After".into()], vec![]).unwrap();
    g.commit().unwrap();
    g.autocommit = true;
    let r = q(&mut g, "MATCH (m:Mark) RETURN count(m)");
    assert!(r.rows.is_empty() || r.rows[0][0] == Value::Int(0));
    let r = q(&mut g, "MATCH (m:After) RETURN count(m)");
    assert_eq!(r.rows[0][0], Value::Int(1));
}

// Stopped anywhere, in autocommit, a write: procedure leaves nothing of its
// own writes or of what the hooks wrote for them (the follower's keyspace,
// the text index), and the hooks run as before for the next statement.
#[test]
fn a_stopped_procedure_leaves_nothing_its_hooks_wrote() {
    let mut g = graph(1_000);
    let need = needed(&mut g, WRITE);
    let base = state(&g);
    let mut s = 1u64;
    let mut stops = 0;
    while s < need {
        let e = stopped_writing(&mut g, s);
        assert!(e.contains("matching steps"), "{e}");
        assert!(state(&g) == base, "stopped at {s}, something was left");
        stops += 1;
        s = if s < 64 { s * 2 } else { s + s / 4 + 1 };
    }
    assert!(stops > 20);
    // the hook state is clean: a write goes through, and is followed
    let id = g.add_node(&["After".into()], vec![]).unwrap();
    let mut followed = false;
    g.ext_scan("follow", &id.to_be_bytes(), &mut |_, _| {
        followed = true;
        false
    });
    assert!(followed, "the hook did not follow the next write");
}

// The outermost statement's rule meets the block's: a statement a procedure
// runs, stopped by its own budget, rolls nothing back by itself; stopped by
// the outer budget, it stops the outer statement, which spends the block.
#[test]
fn a_nested_stop_spends_a_block_only_through_the_outer_statement() {
    let mut g = graph(1_000);
    q(&mut g, "BEGIN");
    q(&mut g, "CREATE (:Mark)");
    // the inner statement stops on its own 50 steps; the outer goes on
    let r = query::execute_capped(&mut g, "CALL follow.nested(steps: 50)", &[], steps(1 << 40))
        .unwrap();
    assert!(r.message.unwrap().contains("matching steps"));
    assert!(g.spent().is_none(), "an inner statement spent the block");
    q(&mut g, "ROLLBACK");
    // the inner statement stops on the outer budget, after the procedure
    // had written: the outer statement is stopped, and the block is spent
    let big = "CALL follow.nested(steps: 1000000)";
    q(&mut g, "BEGIN");
    q(&mut g, "CREATE (:Mark)");
    let e = query::execute_capped(&mut g, big, &[], steps(300))
        .err()
        .expect("the outer statement stopped");
    assert!(e.to_string().contains("rolled back"), "{e}");
    assert!(g.spent().is_some());
    assert!(query::execute(&mut g, "COMMIT").is_err());
    let r = q(&mut g, "MATCH (m:Mark) RETURN count(m)");
    assert!(r.rows.is_empty() || r.rows[0][0] == Value::Int(0));
}

// One panic through both a budgeted procedure and a hook: a hook panicking
// inside CALL pagerank(write:) under a budget, and a procedure panicking
// while hook-following extensions are installed. Afterwards, on the same
// thread: no memory budget or spill directory, no meter, autocommit on,
// graph writes allowed (not "inside a hook"), and nothing of the
// statement left.
#[test]
fn one_panic_through_a_budget_and_a_hook_leaves_both_clean() {
    let mut g = graph(1_000);
    let base = state(&g);
    std::thread::spawn(move || {
        let dir = glider::ooc::temp_dir();
        for (panic, src) in [(1u8, WRITE), (0, "CALL follow.boom()")] {
            PANIC.with(|p| p.set(panic));
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                query::execute_capped(&mut g, src, &[], steps(1 << 40))
            }));
            PANIC.with(|p| p.set(0));
            assert!(r.is_err(), "{src} panicked");
            assert_eq!(
                glider::ooc::budget_left(),
                None,
                "{src}: the memory budget stayed"
            );
            assert_eq!(
                glider::ooc::temp_dir(),
                dir,
                "{src}: the spill directory stayed"
            );
            assert!(!glider::meter::stopped(), "{src}");
            glider::meter::steps(1_000_000).unwrap();
            assert!(g.autocommit, "{src}: autocommit left off");
            assert!(state(&g) == base, "{src}: the statement left something");
            let id = g
                .add_node(&["After".into()], vec![])
                .expect("a graph write");
            g.delete_node(id).unwrap();
        }
    })
    .join()
    .unwrap();
}
