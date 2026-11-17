//! A statement's own budget (`QueryLimits::max_millis`, `max_steps`): matching
//! stops with an error once it is spent, whatever the statement would have
//! cost, and the graph is usable straight after.
use glider::{
    query::{self, Cursor, QueryLimits},
    Graph, Value,
};
use std::time::{Duration, Instant};

fn graph(n: u64) -> Graph {
    let mut g = Graph::memory();
    g.autocommit = false;
    for i in 0..n {
        g.add_node(&["N".into()], vec![("i".into(), Value::Int(i as i64))])
            .unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    g
}

fn budget(millis: Option<u64>, steps: Option<u64>) -> QueryLimits {
    QueryLimits {
        max_millis: millis,
        max_steps: steps,
        ..Default::default()
    }
}

// 4,000 × 4,000 = 16 million pairs, filtered to nothing: seconds unbounded
const HEAVY: &str = "MATCH (a:N), (b:N) WHERE a.i = b.i + 100000 RETURN count(*)";

#[test]
fn a_time_budget_stops_a_heavy_statement() {
    let mut g = graph(4_000);
    let t = Instant::now();
    let err = query::execute_capped(&mut g, HEAVY, &[], budget(Some(100), None))
        .err()
        .expect("stopped");
    assert!(err.to_string().contains("time budget of 100 ms"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    // and the graph answers straight after
    let r = query::execute(&mut g, "MATCH (n:N) RETURN count(n)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(4_000));
}

#[test]
fn a_cursor_stops_on_its_budget_too() {
    let mut g = graph(4_000);
    let t = Instant::now();
    let err = Cursor::open(&mut g, HEAVY, &[], budget(Some(100), None))
        .err()
        .expect("stopped");
    assert!(err.to_string().contains("time budget"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(2));
    let err = Cursor::open(
        &mut g,
        "MATCH (a:N)-[*1..6]-(b) RETURN b",
        &[],
        budget(None, Some(10)),
    );
    assert!(err.is_ok() || err.err().unwrap().to_string().contains("steps"));
}

#[test]
fn a_step_budget_stops_a_statement_on_any_target() {
    let mut g = graph(1_000);
    let err = query::execute_capped(&mut g, HEAVY, &[], budget(None, Some(10_000)))
        .err()
        .expect("stopped");
    assert!(
        err.to_string().contains("budget of 10000 matching steps"),
        "{err}"
    );
}

#[test]
fn a_statement_within_its_budget_answers() {
    let mut g = graph(1_000);
    let r = query::execute_capped(
        &mut g,
        "MATCH (n:N) WHERE n.i < 10 RETURN count(n)",
        &[],
        budget(Some(5_000), Some(1_000_000)),
    )
    .unwrap();
    assert_eq!(r.rows[0][0], Value::Int(10));
    let mut c = Cursor::open(
        &mut g,
        "MATCH (n:N) RETURN n.i",
        &[],
        budget(Some(5_000), None),
    )
    .unwrap();
    let (batch, _) = c.next_batch().unwrap();
    assert!(!batch.rows.is_empty());
    // without a budget nothing changes
    assert!(query::execute_capped(
        &mut g,
        "MATCH (n:N) RETURN n.i",
        &[],
        QueryLimits::default()
    )
    .is_ok());
}
