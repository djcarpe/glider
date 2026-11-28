//! From the third independent review of the procedure budgets: a statement
//! a procedure runs on another database is that database's own, the
//! inner-failure record never outlives its statement, and transaction
//! control cannot split a statement from inside it.
use glider::ext::{self, Args, Extension, Spec};
use glider::query::{self, QueryResult};
use glider::{Graph, Result, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The "other" database a procedure writes to, as an audit-log extension
/// would hold one.
static OTHER: Mutex<Option<Graph>> = Mutex::new(None);

struct Cross;
impl Extension for Cross {
    fn name(&self) -> &str {
        "cross"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "reviewer extension: statements on another database"
    }
    fn procedures(&self) -> &[Spec] {
        const P: &[Spec] = &[
            Spec {
                name: "other",
                args: "(mode)",
                doc: "runs a statement on the other database",
            },
            Spec {
                name: "halferr",
                args: "()",
                doc: "sets x on half the nodes, then fails",
            },
            Spec {
                name: "panicafter",
                args: "()",
                doc: "an inner statement fails after writing, then a panic",
            },
            Spec {
                name: "commitmid",
                args: "()",
                doc: "writes half, runs COMMIT, then fails",
            },
            Spec {
                name: "rollbackmid",
                args: "()",
                doc: "writes x on half, runs ROLLBACK, writes y on the rest",
            },
            Spec {
                name: "control",
                args: "()",
                doc: "tries every kind of transaction control on its own graph",
            },
        ];
        P
    }
    fn call(&self, g: &mut Graph, p: &str, a: &Args) -> Result<QueryResult> {
        match p {
            "other" => {
                let mut lock = OTHER.lock().unwrap();
                let b = lock.as_mut().expect("other database");
                let mode = a.str("mode").unwrap_or_default();
                let r = match mode.as_str() {
                    "ok" => query::execute(b, "CREATE (:FromA)").map(|_| ()),
                    _ => query::execute(b, "CALL cross.halferr()").map(|_| ()),
                };
                // the procedure handles the other database's error itself
                Ok(QueryResult::message(format!(
                    "other said: {:?}",
                    r.err().map(|e| e.to_string())
                )))
            }
            "halferr" => {
                let ids = g.node_ids();
                for id in &ids[..ids.len() / 2] {
                    g.set_node_prop(*id, "x", Value::Int(1))?;
                }
                Err(glider::Error::Msg(
                    "procedure failed half way through".into(),
                ))
            }
            "panicafter" => {
                let _ = query::execute(g, "CALL cross.halferr()");
                panic!("bug after an inner failure");
            }
            "commitmid" => {
                let ids = g.node_ids();
                for id in &ids[..ids.len() / 2] {
                    g.set_node_prop(*id, "x", Value::Int(1))?;
                }
                let c = query::execute(g, "COMMIT");
                Err(glider::Error::Msg(format!(
                    "failed after COMMIT said {:?}",
                    c.map(|r| r.message)
                )))
            }
            "rollbackmid" => {
                let ids = g.node_ids();
                let half = ids.len() / 2;
                for id in &ids[..half] {
                    g.set_node_prop(*id, "x", Value::Int(1))?;
                }
                let r = query::execute(g, "ROLLBACK");
                for id in &ids[half..] {
                    g.set_node_prop(*id, "y", Value::Int(1))?;
                }
                Ok(QueryResult::message(format!(
                    "ROLLBACK said {:?}",
                    r.map(|r| r.message).map_err(|e| e.to_string())
                )))
            }
            "control" => {
                let mut said = Vec::new();
                for stmt in ["BEGIN", "COMMIT", "ROLLBACK"] {
                    said.push(match query::execute(g, stmt) {
                        Ok(_) => format!("{stmt}: ran"),
                        Err(e) => format!("{stmt}: refused ({e})"),
                    });
                }
                let calls: [(&str, Result<()>); 3] = [
                    ("begin()", g.begin()),
                    ("commit()", g.commit()),
                    ("rollback()", g.rollback()),
                ];
                for (what, r) in calls {
                    said.push(match r {
                        Ok(()) => format!("{what}: ran"),
                        Err(e) => format!("{what}: refused ({e})"),
                    });
                }
                Ok(QueryResult::message(said.join("; ")))
            }
            _ => Err(glider::Error::Msg("no such procedure".into())),
        }
    }
}

fn nodes(g: &mut Graph, n: usize) {
    g.autocommit = false;
    for i in 0..n {
        g.add_node(&["N".into()], vec![("i".into(), Value::Int(i as i64))])
            .unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    query::execute(g, "CREATE EXTENSION cross").unwrap();
}

fn count(g: &mut Graph, src: &str) -> i64 {
    query::execute(g, src)
        .unwrap()
        .rows
        .first()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(0)
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("glider-cross-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// A procedure on database a runs a statement on database b, which succeeds:
// b's write is committed when that statement ends, durably.
#[test]
fn a_statement_on_another_database_is_committed_there() {
    ext::register(Arc::new(Cross));
    let d = dir("ok");
    let bpath = d.join("b.glider");
    let mut a = Graph::memory();
    nodes(&mut a, 10);
    let mut b = Graph::open(&bpath, glider::Sync::Always).unwrap();
    nodes(&mut b, 10);
    *OTHER.lock().unwrap() = Some(b);
    let r = query::execute(&mut a, "CALL cross.other(mode: \"ok\")");
    eprintln!(
        "CALL on a: {:?}",
        r.as_ref()
            .map(|r| r.message.clone())
            .map_err(|e| e.to_string())
    );
    let b = OTHER.lock().unwrap().take().unwrap();
    eprintln!(
        "b after the procedure: autocommit {}, uncommitted {}",
        b.autocommit,
        b.uncommitted()
    );
    // no further statement on b: the handle is closed as an embedder would
    drop(b);
    let seen = -1;
    let mut b = Graph::open(&bpath, glider::Sync::Always).unwrap();
    let durable = count(&mut b, "MATCH (n:FromA) RETURN count(n)");
    eprintln!("FromA visible on b before close: {seen}; after reopening b: {durable}");
    assert_eq!(
        durable, 1,
        "a statement that succeeded on b was not committed"
    );
    drop(b);
    let _ = std::fs::remove_dir_all(&d);
}

// The statement on b fails after writing and the procedure handles it:
// b's half-statement is rolled back on b, and a is not blamed.
#[test]
fn a_failure_on_another_database_is_that_databases_alone() {
    ext::register(Arc::new(Cross));
    let mut a = Graph::memory();
    nodes(&mut a, 10);
    let mut b = Graph::memory();
    nodes(&mut b, 2_000);
    *OTHER.lock().unwrap() = Some(b);
    query::execute(&mut a, "CREATE (:Mine)").unwrap();
    let r = query::execute(&mut a, "CALL cross.other(mode: \"fail\")");
    eprintln!(
        "CALL on a: {:?}",
        r.as_ref()
            .map(|r| r.message.clone())
            .map_err(|e| e.to_string())
    );
    let mut b = OTHER.lock().unwrap().take().unwrap();
    eprintln!(
        "b after the procedure: autocommit {}, uncommitted {}",
        b.autocommit,
        b.uncommitted()
    );
    // an unrelated statement on b
    query::execute(&mut b, "CREATE (:Unrelated)").unwrap();
    let x = count(&mut b, "MATCH (n:N) WHERE n.x IS NOT NULL RETURN count(n)");
    eprintln!("b: nodes with x after an unrelated write: {x} of 2000");
    let blamed = r.is_err();
    assert!(!blamed, "a's statement failed for b's error");
    assert_eq!(
        x, 0,
        "half of b's failed statement was committed by a later unrelated one"
    );
}

// After an outermost statement panics with an inner failure recorded, or
// fails itself, the next unrelated statement on the thread is not failed.
#[test]
fn the_failure_record_does_not_outlive_its_statement() {
    ext::register(Arc::new(Cross));
    let mut a = Graph::memory();
    nodes(&mut a, 100);
    let mut b = Graph::memory();
    nodes(&mut b, 100);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        query::execute(&mut a, "CALL cross.panicafter()")
    }));
    assert!(r.is_err());
    let x = count(&mut a, "MATCH (n:N) WHERE n.x IS NOT NULL RETURN count(n)");
    eprintln!(
        "a after the panic: x on {x} nodes; autocommit {}",
        a.autocommit
    );
    assert_eq!(x, 0);
    for g in [&mut a, &mut b] {
        query::execute(g, "CREATE (:After)").unwrap();
        assert!(query::execute(g, "CALL nope.nothing()").is_err());
        query::execute(g, "CREATE (:After)").unwrap();
    }
    let mut inner = Graph::memory();
    nodes(&mut inner, 10);
    // and on another thread, each statement fresh
    std::thread::scope(|s| {
        s.spawn(|| {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                query::execute(&mut a, "CALL cross.panicafter()")
            }));
            query::execute(&mut b, "CREATE (:After)").unwrap();
        });
    });
    eprintln!("statements after the panic and the error ran normally");
}

// A procedure that runs COMMIT or ROLLBACK as a statement inside the CALL
// is refused, and the CALL stays whole either way.
#[test]
fn transaction_control_inside_a_statement_is_refused_and_the_call_stays_whole() {
    ext::register(Arc::new(Cross));
    let mut a = Graph::memory();
    nodes(&mut a, 2_000);
    let r = query::execute(&mut a, "CALL cross.commitmid()");
    let x = count(&mut a, "MATCH (n:N) WHERE n.x IS NOT NULL RETURN count(n)");
    assert!(x == 0 || x == 2_000, "half of a failed CALL was committed");
    assert!(a.autocommit);
    eprintln!(
        "commitmid: {:?}; x committed on {x} of 2000",
        r.map(|r| r.message).map_err(|e| e.to_string())
    );
    let mut b = Graph::memory();
    nodes(&mut b, 2_000);
    let r = query::execute(&mut b, "CALL cross.rollbackmid()");
    let x = count(&mut b, "MATCH (n:N) WHERE n.x IS NOT NULL RETURN count(n)");
    let y = count(&mut b, "MATCH (n:N) WHERE n.y IS NOT NULL RETURN count(n)");
    eprintln!(
        "rollbackmid: {:?}; committed x on {x}, y on {y} of 2000",
        r.map(|r| r.message).map_err(|e| e.to_string())
    );
    assert!(
        (x, y) == (0, 0) || (x, y) == (1_000, 1_000),
        "half of a CALL was committed"
    );
    // the refusal says why, for the statements and the library calls
    let mut c = Graph::memory();
    nodes(&mut c, 10);
    let r = query::execute(&mut c, "CALL cross.control()").unwrap();
    let said = r.message.unwrap();
    for what in [
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "begin()",
        "commit()",
        "rollback()",
    ] {
        assert!(
            said.contains(&format!("{what}: refused")),
            "{what} was not refused: {said}"
        );
    }
    assert!(
        said.contains("cannot run inside a statement on the same database"),
        "{said}"
    );
}
