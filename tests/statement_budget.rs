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

// ---------------------------------------------------------------- procedures
//
// `CALL` keeps the same budget as a query: every built-in algorithm, the
// local walks and the extension procedures count one step per node taken
// up and one per edge looked at, and stop with the same error.

/// A DAG of `n` nodes, each with up to `k` edges to later nodes (so there
/// is no cycle and toposort, levels and reduce have work to do), and a few
/// words in its name for the text procedures.
fn dag(n: u64, k: u64) -> (Graph, Vec<u64>) {
    const WORDS: [&str; 12] = [
        "pump", "valve", "motor", "sensor", "line", "station", "tank", "bolt", "panel", "cable",
        "relay", "gauge",
    ];
    let mut g = Graph::memory();
    g.autocommit = false;
    let ids: Vec<u64> = (0..n)
        .map(|i| {
            let name = format!(
                "{} {} {i}",
                WORDS[(i % 12) as usize],
                WORDS[(i * 7 % 11) as usize]
            );
            g.add_node(
                &["N".into()],
                vec![
                    ("i".into(), Value::Int(i as i64)),
                    ("name".into(), Value::Text(name)),
                ],
            )
            .unwrap()
        })
        .collect();
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    for i in 0..n {
        for _ in 0..k {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let span = n - i - 1;
            if span == 0 {
                break;
            }
            let j = i + 1 + x % span.min(64);
            g.add_edge(
                ids[i as usize],
                ids[j as usize],
                "E",
                vec![("w".into(), Value::Int((x % 9 + 1) as i64))],
            )
            .unwrap();
        }
    }
    g.commit().unwrap();
    g.autocommit = true;
    (g, ids)
}

/// Every `CALL` that reaches `run_algorithm`, once per name.
fn algorithms(from: u64, to: u64) -> Vec<String> {
    let mut v: Vec<String> = [
        "pagerank()",
        "betweenness()",
        "closeness()",
        "closeness(weight: \"w\")",
        "degree()",
        "triangles()",
        "clustering()",
        "kcore()",
        "components()",
        "scc()",
        "communities()",
        "toposort()",
        "levels()",
        "reduce()",
        "cycle()",
        "mst(weight: \"w\")",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.push(format!(
        "shortestpath(from: {from}, to: {to}, dir: \"both\")"
    ));
    v.push(format!(
        "shortestpath(from: {from}, to: {to}, weight: \"w\")"
    ));
    v.push(format!("sssp(from: {from})"));
    v.push(format!("sssp(from: {from}, weight: \"w\")"));
    v.push(format!("bfs(from: {from}, dir: \"both\")"));
    v.push(format!("dfs(from: {from}, dir: \"both\")"));
    v.push(format!(
        "subgraph(from: {from}, depth: 1000, dir: \"both\")"
    ));
    v
}

/// `CALL name(args)` with `tier:` added.
fn on_tier(call: &str, tier: &str) -> String {
    let (name, args) = call.split_once('(').unwrap();
    let args = args.trim_end_matches(')');
    let sep = if args.is_empty() { "" } else { ", " };
    format!("CALL {name}({args}{sep}tier: \"{tier}\")")
}

#[test]
fn every_algorithm_stops_on_a_step_budget() {
    let (mut g, ids) = dag(3_000, 5);
    let (from, to) = (ids[0], ids[ids.len() - 1]);
    for call in algorithms(from, to) {
        for tier in ["auto", "mem", "ooc"] {
            let src = on_tier(&call, tier);
            let err = query::execute_capped(&mut g, &src, &[], budget(None, Some(500)))
                .err()
                .unwrap_or_else(|| panic!("{src} was not stopped"));
            assert!(
                err.to_string().contains("budget of 500 matching steps"),
                "{src}: {err}"
            );
        }
    }
    // the extensions' procedures too
    query::execute(&mut g, "CREATE EXTENSION context").unwrap();
    query::execute(&mut g, "CREATE EXTENSION check").unwrap();
    for src in [
        "CALL check.run()".to_string(),
        "CALL context.index()".to_string(),
        "CALL context.search(query: \"pump valve\", scan: 100000)".to_string(),
        format!("CALL context.retrieve(seeds: [{from}], budget: 100000)"),
        format!("CALL context.paths(from: {from}, to: {to}, depth: 1000)"),
        "CALL context.schema(sample: 10000)".to_string(),
    ] {
        let err = query::execute_capped(&mut g, &src, &[], budget(None, Some(500)))
            .err()
            .unwrap_or_else(|| panic!("{src} was not stopped"));
        assert!(
            err.to_string().contains("budget of 500 matching steps"),
            "{src}: {err}"
        );
    }
    // one node's neighbours are one list, but a list too
    let err = query::execute_capped(
        &mut g,
        &format!("CALL neighbors(from: {from})"),
        &[],
        budget(None, Some(3)),
    )
    .err()
    .expect("stopped");
    assert!(err.to_string().contains("matching steps"), "{err}");
    // and the graph answers straight after
    let r = query::execute(&mut g, "MATCH (n:N) RETURN count(n)").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(3_000));
}

#[test]
fn a_budget_that_is_not_reached_changes_no_result() {
    let (mut g, ids) = dag(1_500, 4);
    let (from, to) = (ids[0], ids[ids.len() - 1]);
    let roomy = QueryLimits {
        max_millis: Some(3_600_000),
        max_steps: Some(u64::MAX / 2),
        ..Default::default()
    };
    let mut calls: Vec<String> = Vec::new();
    for call in algorithms(from, to) {
        for tier in ["auto", "mem", "ooc"] {
            calls.push(on_tier(&call, tier));
        }
    }
    calls.push(format!("CALL neighbors(from: {from})"));
    for src in calls {
        let free = query::execute(&mut g, &src).unwrap();
        let metered = query::execute_capped(&mut g, &src, &[], roomy).unwrap();
        assert_eq!(free.to_json(), metered.to_json(), "{src}");
    }
}

/// Runs `src` under `limits`: the error and how long it took to come back.
fn timed(g: &mut Graph, src: &str, limits: QueryLimits) -> (Option<String>, Duration) {
    let t = Instant::now();
    let r = query::execute_capped(g, src, &[], limits);
    (r.err().map(|e| e.to_string()), t.elapsed())
}

/// Release builds get the large graph; debug builds, which run every loop
/// many times slower, a tenth of it, so the procedures still take seconds.
fn scale(n: u64) -> u64 {
    if cfg!(debug_assertions) {
        n / 10
    } else {
        n
    }
}

// A 50 ms budget on procedures that run for seconds without one (the
// first two would run for hours): each comes back with the budget's error
// well under a second later, and the graph answers straight after.
#[test]
fn fifty_milliseconds_stop_long_procedures_well_under_a_second() {
    let (mut g, ids) = dag(scale(200_000), 5);
    query::execute(&mut g, "CREATE EXTENSION context").unwrap();
    let from = ids[0];
    let long = [
        "CALL pagerank(iterations: 1000000, tolerance: 0)".to_string(),
        "CALL betweenness()".to_string(),
        // the whole graph, by the local walk and by the paged projection
        format!("CALL bfs(from: {from}, dir: \"both\")"),
        format!("CALL bfs(from: {from}, dir: \"both\", tier: \"ooc\")"),
        format!(
            "CALL context.retrieve(seeds: [{from}], alpha: 0.01, epsilon: 0.000000001, depth: 100000, budget: 100000000)"
        ),
        "CALL context.index()".to_string(),
    ];
    for src in &long {
        let (err, took) = timed(&mut g, src, budget(Some(50), None));
        let err = err.unwrap_or_else(|| panic!("{src} was not stopped"));
        assert!(err.contains("time budget of 50 ms"), "{src}: {err}");
        assert!(took < Duration::from_millis(300), "{src}: {took:?}");
        let r = query::execute(&mut g, "MATCH (n:N) WHERE n.i = 7 RETURN n.i").unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int(7)]]);
        if src.contains("index") {
            // the stopped build left no index behind: search scans
            let r = query::execute(&mut g, "CALL context.search(query: \"pump\", top: 1)").unwrap();
            assert!(r.message.unwrap().contains("nodes scanned"));
        }
        // and without a budget (well, with three seconds, so that the test
        // ends) it runs much longer
        let (_, free) = timed(&mut g, src, budget(Some(3_000), None));
        eprintln!("{src}: {free:?} unbudgeted, {took:?} with a 50 ms budget");
        assert!(took * 2 < free, "{src}: {took:?} against {free:?}");
    }
}

fn count(g: &mut Graph, src: &str) -> Value {
    let r = query::execute(g, src).unwrap();
    r.rows.first().map_or(Value::Int(0), |row| row[0].clone())
}

/// The fewest steps `src` needs: it is stopped with one fewer.
fn steps_needed(g: &mut Graph, src: &str) -> u64 {
    let (mut lo, mut hi) = (0u64, 1u64 << 40);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        match query::execute_capped(g, src, &[], budget(None, Some(mid))) {
            Ok(_) => hi = mid,
            Err(_) => lo = mid,
        }
    }
    hi
}

// Stopped at every point it can be (computing, or part-way through writing
// the results back), a `write:` procedure has written nothing.
#[test]
fn a_stopped_procedure_has_written_nothing() {
    let (mut g, ids) = dag(2_000, 4);
    let from = ids[0];
    for (src, prop) in [
        ("CALL pagerank(write: \"rank\")".to_string(), "rank"),
        ("CALL levels(write: \"level\")".to_string(), "level"),
        (format!("CALL sssp(from: {from}, write: \"d\")"), "d"),
    ] {
        let written = format!("MATCH (n:N) WHERE n.{prop} IS NOT NULL RETURN count(n)");
        let mut steps = 64u64;
        let mut stops = 0;
        while let Err(e) = query::execute_capped(&mut g, &src, &[], budget(None, Some(steps))) {
            assert!(e.to_string().contains("matching steps"), "{src}: {e}");
            assert_eq!(count(&mut g, &written), Value::Int(0), "{src} at {steps}");
            stops += 1;
            steps += steps / 4;
        }
        assert!(stops > 10, "{src}: {stops}");
        // the budget that was enough wrote everything
        assert_ne!(count(&mut g, &written), Value::Int(0), "{src}");
    }
}

// Inside BEGIN, a stopped statement that had written something takes the
// open transaction with it, so its half-written state cannot be committed;
// one that had written nothing leaves the transaction as it was.
#[test]
fn a_stopped_write_inside_a_transaction_rolls_it_back() {
    let (mut g, _) = dag(2_000, 4);
    let src = "CALL pagerank(write: \"rank\")";
    let needed = steps_needed(&mut g, src);
    query::execute(&mut g, "MATCH (n:N) REMOVE n.rank").unwrap();
    query::execute(&mut g, "BEGIN").unwrap();
    query::execute(&mut g, "CREATE (:Mark)").unwrap();
    // all of pagerank, and part of its writes
    let err = query::execute_capped(&mut g, src, &[], budget(None, Some(needed - 500)))
        .err()
        .expect("stopped");
    let err = err.to_string();
    assert!(err.contains("matching steps"), "{err}");
    assert!(err.contains("rolled back"), "{err}");
    assert!(g.autocommit, "the transaction is over");
    assert_eq!(
        count(&mut g, "MATCH (m:Mark) RETURN count(m)"),
        Value::Int(0)
    );
    assert_eq!(
        count(
            &mut g,
            "MATCH (n:N) WHERE n.rank IS NOT NULL RETURN count(n)"
        ),
        Value::Int(0)
    );

    query::execute(&mut g, "BEGIN").unwrap();
    query::execute(&mut g, "CREATE (:Mark)").unwrap();
    let err = query::execute_capped(&mut g, "CALL pagerank()", &[], budget(None, Some(1_000)))
        .err()
        .expect("stopped");
    assert!(!err.to_string().contains("rolled back"), "{err}");
    query::execute(&mut g, "COMMIT").unwrap();
    assert_eq!(
        count(&mut g, "MATCH (m:Mark) RETURN count(m)"),
        Value::Int(1)
    );
}
