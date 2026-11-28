//! Two databases on one thread: a statement a procedure runs on another
//! database is that database's own outermost statement. It commits or rolls
//! back there under that database's rules, and its failure is reported to
//! the procedure that ran it, never to the caller's statement. The caller's
//! time and step budget bounds the work it starts on the other database
//! too: the budget is the caller's patience, not a property of a database.
//!
//! The procedures reach the other database through a raw pointer the test
//! sets, as an embedder holding several handles would reach them; the
//! caller's handle is not used while the other one runs.
#![allow(clippy::not_unsafe_ptr_arg_deref)]
use std::cell::Cell;
use std::sync::{Arc, Once};

use glider::ext::{self, Args, Extension, Spec};
use glider::query::{self, QueryLimits, QueryResult};
use glider::{Graph, Result, Value};

thread_local! {
    /// The database `link.run` runs its statement on.
    static PEER: Cell<*mut Graph> = const { Cell::new(std::ptr::null_mut()) };
    /// The database `link.back` runs its statement on: the one whose
    /// procedure last called `link.run`.
    static HOME: Cell<*mut Graph> = const { Cell::new(std::ptr::null_mut()) };
}

struct Link;
impl Extension for Link {
    fn name(&self) -> &str {
        "link"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "test extension: statements on another database"
    }
    fn procedures(&self) -> &[Spec] {
        const P: &[Spec] = &[
            Spec {
                name: "run",
                args: "(src, steps)",
                doc: "writes a Caller node here, then runs src on the peer, handling its error",
            },
            Spec {
                name: "back",
                args: "(src, steps)",
                doc: "writes a Caller node here, then runs src on the database that called",
            },
            Spec {
                name: "halferr",
                args: "()",
                doc: "sets x on half the nodes, then fails",
            },
            Spec {
                name: "halfpanic",
                args: "()",
                doc: "sets x on half the nodes, then panics",
            },
        ];
        P
    }
    fn call(&self, g: &mut Graph, p: &str, a: &Args) -> Result<QueryResult> {
        match p {
            "run" | "back" => {
                g.add_node(&["Caller".into()], vec![])?;
                let target = if p == "run" {
                    HOME.with(|h| h.set(g as *mut Graph));
                    PEER.with(|x| x.get())
                } else {
                    HOME.with(|h| h.get())
                };
                // SAFETY: the target is another live handle the test owns,
                // not used by anyone else while this statement runs on it.
                let target = unsafe { &mut *target };
                let src = a.str("src").unwrap_or_default();
                let lim = QueryLimits {
                    max_steps: a.get("steps").and_then(|v| v.as_i64()).map(|v| v as u64),
                    ..Default::default()
                };
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    query::execute_capped(target, &src, &[], lim)
                }));
                let said = match r {
                    Ok(Ok(_)) => "ok".to_string(),
                    Ok(Err(e)) => format!("error: {e}"),
                    Err(_) => "panic".to_string(),
                };
                Ok(QueryResult::message(said))
            }
            "halferr" | "halfpanic" => {
                let ids = g.node_ids();
                let n = ids
                    .iter()
                    .filter(|id| g.node_labels(**id).contains(&"N".to_string()))
                    .count();
                for id in ids.iter().take(n / 2) {
                    g.set_node_prop(*id, "x", Value::Int(1))?;
                }
                if p == "halfpanic" {
                    panic!("procedure bug half way through");
                }
                Err(glider::Error::Msg(
                    "procedure failed half way through".into(),
                ))
            }
            _ => unreachable!(),
        }
    }
}

fn graph(n: usize) -> Graph {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| ext::register(Arc::new(Link)));
    let mut g = Graph::memory();
    g.autocommit = false;
    let ids: Vec<u64> = (0..n)
        .map(|i| {
            g.add_node(&["N".into()], vec![("i".into(), Value::Int(i as i64))])
                .unwrap()
        })
        .collect();
    for i in 1..n {
        g.add_edge(ids[i / 2], ids[i], "E", vec![]).unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    query::execute(&mut g, "CREATE EXTENSION link").unwrap();
    g
}

fn count(g: &mut Graph, src: &str) -> i64 {
    let r = query::execute(g, src).unwrap();
    r.rows.first().and_then(|r| r[0].as_i64()).unwrap_or(0)
}

/// Nodes with `x`, `Caller` nodes, `Done` nodes; and that nothing is left
/// pending on the handle.
fn seen(g: &mut Graph) -> (i64, i64, i64) {
    assert_eq!(g.uncommitted(), 0, "left pending");
    assert!(g.autocommit, "left in a transaction");
    (
        count(g, "MATCH (n:N) WHERE n.x IS NOT NULL RETURN count(n)"),
        count(g, "MATCH (n:Caller) RETURN count(n)"),
        count(g, "MATCH (n:Done) RETURN count(n)"),
    )
}

fn call(g: &mut Graph, src: &str, steps: Option<u64>) -> std::result::Result<String, String> {
    let lim = QueryLimits {
        max_steps: steps,
        ..Default::default()
    };
    query::execute_capped(g, src, &[], lim)
        .map(|r| r.message.unwrap_or_default())
        .map_err(|e| e.to_string())
}

const HEAVY: &str = "CALL pagerank(write: \\\"x\\\")";

// Two levels, both directions (a's procedure on b, b's procedure on a): the
// statement on the other database commits there when it succeeds, is rolled
// back there when it fails after writing, panics, or is stopped by its own
// budget, and the caller's statement goes on and commits.
#[test]
fn a_statement_on_another_database_is_that_databases_own() {
    for flip in [false, true] {
        let (mut a, mut b) = (graph(400), graph(400));
        let (caller, other) = if flip {
            (&mut b, &mut a)
        } else {
            (&mut a, &mut b)
        };
        PEER.with(|p| p.set(other as *mut Graph));
        let cases = [
            ("CREATE (:Done)".to_string(), None, "ok", (0, 0, 1)),
            // (the Done of the first case stays)
            ("CALL link.halferr()".to_string(), None, "error", (0, 0, 1)),
            (
                "CALL link.halfpanic()".to_string(),
                None,
                "panic",
                (0, 0, 1),
            ),
            (HEAVY.to_string(), Some(2_000), "budget of 2000", (0, 0, 1)),
        ];
        for (i, (src, steps, said, on_other)) in cases.into_iter().enumerate() {
            let steps = steps.map(|s| format!(", steps: {s}")).unwrap_or_default();
            let r = call(
                caller,
                &format!("CALL link.run(src: \"{src}\"{steps})"),
                None,
            )
            .unwrap_or_else(|e| panic!("{src}: the caller was blamed: {e}"));
            assert!(r.contains(said), "{src}: the procedure heard {r}");
            let (x, _, done) = seen(other);
            assert_eq!(
                (x, done),
                (on_other.0, on_other.2),
                "{src} on the other database"
            );
            let (x, callers, _) = seen(caller);
            assert_eq!(
                (x, callers),
                (0, i as i64 + 1),
                "{src}: the caller's own write"
            );
        }
    }
}

// The caller's budget bounds what it starts on the other database: spent
// there, it stops the statement there (rolled back there) and the caller's
// statement too (rolled back here).
#[test]
fn the_callers_budget_bounds_its_work_on_another_database() {
    let (mut a, mut b) = (graph(400), graph(400));
    PEER.with(|p| p.set(&mut b as *mut Graph));
    let e = call(
        &mut a,
        &format!("CALL link.run(src: \"{HEAVY}\")"),
        Some(3_000),
    )
    .expect_err("stopped");
    assert!(e.contains("budget of 3000 matching steps"), "{e}");
    assert_eq!(seen(&mut b), (0, 0, 0));
    assert_eq!(seen(&mut a), (0, 0, 0));
}

// Three levels: a's procedure runs a statement on b, whose procedure runs
// one on a again. That last one is inside a's statement: its failure after
// writing fails a's statement as a whole, while b's statement, which
// handled the error, commits on b. Success commits on both.
#[test]
fn three_levels_a_on_b_on_a() {
    let cases = [
        ("CREATE (:Done)", "ok", true),
        ("CALL link.halferr()", "error", false),
        ("CALL link.halfpanic()", "panic", false),
    ];
    for (deep, said, whole) in cases {
        let (mut a, mut b) = (graph(400), graph(400));
        PEER.with(|p| p.set(&mut b as *mut Graph));
        let inner = format!("CALL link.back(src: \\\"{deep}\\\")");
        let r = call(&mut a, &format!("CALL link.run(src: \"{inner}\")"), None);
        // b's statement ran the statement on a and heard how it went
        assert_eq!(seen(&mut b).1, 1, "{deep}: b's statement is b's own");
        if whole {
            assert_eq!(r.as_deref(), Ok("ok"), "{deep}");
            assert_eq!(seen(&mut a), (0, 1, 1), "{deep}");
        } else {
            let e = r.expect_err("a's statement fails as a whole");
            assert!(e.contains("statement failed as a whole"), "{deep}: {e}");
            assert!(
                e.contains(if said == "panic" {
                    "panicked"
                } else {
                    "failed after writing"
                }),
                "{e}"
            );
            assert_eq!(
                seen(&mut a),
                (0, 0, 0),
                "{deep}: something of a's statement was left"
            );
        }
    }
    // and a budget spent three levels down stops all three
    let (mut a, mut b) = (graph(400), graph(400));
    PEER.with(|p| p.set(&mut b as *mut Graph));
    let inner = format!(
        "CALL link.back(src: \\\"{}\\\")",
        HEAVY.replace("\\\"", "'")
    );
    let e = call(
        &mut a,
        &format!("CALL link.run(src: \"{inner}\")"),
        Some(3_000),
    )
    .expect_err("stopped");
    assert!(e.contains("budget of 3000 matching steps"), "{e}");
    assert_eq!(seen(&mut a), (0, 0, 0));
    assert_eq!(seen(&mut b), (0, 0, 0));
}

// What two slices keep per thread holds with two databases on it: after a
// statement on b inside a's statement panics, the memory budget, the
// meter, and each handle's own state (autocommit, the hook state, the
// nesting) are clean on both.
#[test]
fn per_thread_state_is_clean_on_both_databases() {
    let (mut a, mut b) = (graph(400), graph(400));
    PEER.with(|p| p.set(&mut b as *mut Graph));
    std::thread::spawn(move || {
        PEER.with(|p| p.set(&mut b as *mut Graph));
        let dir = glider::ooc::temp_dir();
        let r = call(
            &mut a,
            "CALL link.run(src: \"CALL link.halfpanic()\")",
            Some(1 << 40),
        );
        assert_eq!(r.as_deref(), Ok("panic"));
        assert_eq!(glider::ooc::budget_left(), None);
        assert_eq!(glider::ooc::temp_dir(), dir);
        assert!(!glider::meter::stopped());
        for g in [&mut a, &mut b] {
            assert!(g.autocommit);
            let id = g.add_node(&["After".into()], vec![]).unwrap();
            g.delete_node(id).unwrap();
            // a statement here is an outermost one again: it commits
            query::execute(g, "CREATE (:After)").unwrap();
            assert_eq!(g.uncommitted(), 0);
        }
    })
    .join()
    .unwrap();
}

fn hooked(n: usize) -> Graph {
    let mut g = graph(n);
    for s in [
        "CREATE EXTENSION changes",
        "CREATE EXTENSION context",
        "CALL context.index()",
    ] {
        query::execute(&mut g, s).unwrap();
    }
    g
}

/// What the hooks keep: the journal's and the text index's keyspaces.
fn kept(g: &Graph) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    for ns in ["changes", "context"] {
        g.ext_scan(ns, b"", &mut |k, v| {
            out.push((k.to_vec(), v.to_vec()));
            true
        });
    }
    out
}

fn found(g: &mut Graph, word: &str) -> usize {
    let r = query::execute(g, &format!("CALL context.search(query: \"{word}\")")).unwrap();
    r.rows.len()
}

// The journal's and the text index's hooks, running inside a statement a
// procedure runs (on the other database, or on the caller's own three
// levels down), follow the same rules as in an outermost statement: what
// a statement that commits wrote is followed, and nothing they wrote for a
// statement that failed is left, on either database.
#[test]
fn hooks_inside_an_inner_statement_follow_the_outermost_rules() {
    for flip in [false, true] {
        let (mut a, mut b) = (hooked(300), hooked(300));
        let (caller, other) = if flip {
            (&mut b, &mut a)
        } else {
            (&mut a, &mut b)
        };
        PEER.with(|p| p.set(other as *mut Graph));
        // a write on the other database, through the caller's procedure
        let rev = other.revision();
        let r = call(
            caller,
            "CALL link.run(src: \"CREATE (:Done {name: 'zebra'})\")",
            None,
        );
        assert_eq!(r.as_deref(), Ok("ok"));
        assert_eq!(found(other, "zebra"), 1, "the text index followed it");
        let since = query::execute(other, &format!("CALL changes.since(revision: {rev})")).unwrap();
        assert!(!since.rows.is_empty(), "the journal followed it");
        // a failure there leaves nothing of what its hooks wrote
        let (other_kept, caller_kept) = (kept(other), kept(caller));
        let r = call(caller, "CALL link.run(src: \"CALL link.halferr()\")", None);
        assert!(r.unwrap().contains("error"));
        assert!(
            kept(other) == other_kept,
            "hooks' writes of a failed statement stayed"
        );
        // the caller's own Caller node was followed by its hooks
        assert!(kept(caller) != caller_kept);
        // three levels: a write on the caller's own database inside its
        // statement is followed; a failure there fails the caller's
        // statement as a whole and leaves none of the hooks' writes
        let r = call(
            caller,
            "CALL link.run(src: \"CALL link.back(src: \\\"CREATE (:Done {name: 'yak'})\\\")\")",
            None,
        );
        assert_eq!(r.as_deref(), Ok("ok"));
        assert_eq!(found(caller, "yak"), 1);
        let caller_kept = kept(caller);
        let r = call(
            caller,
            "CALL link.run(src: \"CALL link.back(src: \\\"CALL link.halferr()\\\")\")",
            None,
        );
        assert!(r.unwrap_err().contains("statement failed as a whole"));
        assert!(
            kept(caller) == caller_kept,
            "hooks' writes of a failed statement stayed"
        );
        assert_eq!(seen(caller).0, 0);
        assert_eq!(seen(other).0, 0);
    }
}
