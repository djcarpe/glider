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

// Candidates rejected before any binding still count: for each of 30,000
// `a`, a label scan of 30,000 `b` that all fail the property test, 900
// million checks with no match ever made.
fn functions(n: u64) -> Graph {
    let mut g = Graph::memory();
    g.autocommit = false;
    for i in 0..n {
        g.add_node(
            &["Function".into()],
            vec![("name".into(), Value::Text(format!("fn_{i}")))],
        )
        .unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    g
}

const REJECTING: &str = "MATCH (a:Function), (b:Function {name:'zzz'}) RETURN count(*)";

#[test]
fn rejected_candidates_count_against_the_time_budget() {
    let mut g = functions(30_000);
    let t = Instant::now();
    let err = query::execute_capped(&mut g, REJECTING, &[], budget(Some(1_000), None))
        .err()
        .expect("stopped");
    assert!(err.to_string().contains("time budget of 1000 ms"), "{err}");
    assert!(
        t.elapsed() < Duration::from_millis(1_300),
        "{:?}",
        t.elapsed()
    );
    let t = Instant::now();
    let err = Cursor::open(&mut g, REJECTING, &[], budget(Some(1_000), None))
        .err()
        .expect("stopped");
    assert!(err.to_string().contains("time budget"), "{err}");
    assert!(
        t.elapsed() < Duration::from_millis(1_300),
        "{:?}",
        t.elapsed()
    );
}

#[test]
fn rejected_candidates_count_against_the_step_budget() {
    let mut g = functions(30_000);
    let t = Instant::now();
    let err = query::execute_capped(&mut g, REJECTING, &[], budget(None, Some(100_000)))
        .err()
        .expect("stopped");
    assert!(err.to_string().contains("100000 matching steps"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

#[test]
fn edges_scanned_and_neighbours_rejected_count_too() {
    // a typed scan of edges whose properties never match, and a walk from
    // every node whose neighbours never match: no binding is ever made
    let mut g = functions(2_000);
    for i in 0..2_000u64 {
        query::execute(
            &mut g,
            &format!("MATCH (a:Function {{name: 'fn_{i}'}}), (b:Function {{name: 'fn_{}'}}) CREATE (a)-[:CALLS {{w: 1}}]->(b)", (i + 1) % 2_000),
        )
        .unwrap();
    }
    let err = query::execute_capped(
        &mut g,
        "MATCH (a:Function), (x)-[:CALLS {w: 2}]->(y) RETURN count(*)",
        &[],
        budget(None, Some(50_000)),
    )
    .err()
    .expect("stopped");
    assert!(err.to_string().contains("matching steps"), "{err}");
    let err = query::execute_capped(
        &mut g,
        "MATCH (a:Function), (b:Function)-->(c {name: 'zzz'}) RETURN count(*)",
        &[],
        budget(None, Some(50_000)),
    )
    .err()
    .expect("stopped");
    assert!(err.to_string().contains("matching steps"), "{err}");
}

// Not a budget: a cursor spools nodes as JSON, and a whole float must come
// back a float (it came back 3 for 3.0).
#[test]
fn whole_floats_survive_the_cursor_spool() {
    let mut g = Graph::memory();
    query::execute(&mut g, "CREATE (:Bar {length: 3.0, w: 2.5, n: 4})").unwrap();
    let mut c = Cursor::open(
        &mut g,
        "MATCH (b:Bar) RETURN b",
        &[],
        QueryLimits::default(),
    )
    .unwrap();
    let (batch, _) = c.next_batch().unwrap();
    let Value::Text(json) = &batch.rows[0][0] else {
        panic!("{:?}", batch.rows[0][0])
    };
    assert!(json.contains("\"length\":3.0"), "{json}");
    assert!(json.contains("\"n\":4"), "{json}");
    assert!(json.contains("\"w\":2.5"), "{json}");
}
