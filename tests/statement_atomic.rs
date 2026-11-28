//! A statement is all or nothing even when a procedure runs it inside
//! another, and a panic is a failure of the statement it unwinds through:
//! from the second independent review of the procedure budgets, in each of
//! the four kinds of transaction (autocommit, BEGIN, Graph::begin, a
//! transaction opened by assigning `autocommit`).
use glider::ext::{self, Args, EdgeImage, Extension, NodeImage, Spec};
use glider::query::{self, QueryLimits, QueryResult};
use glider::{Graph, Result, Value};
use std::sync::Arc;
use std::time::Instant;

struct P2;
impl Extension for P2 {
    fn name(&self) -> &str {
        "p2"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "reviewer extension"
    }
    fn procedures(&self) -> &[Spec] {
        const P: &[Spec] = &[
            Spec {
                name: "swallow",
                args: "(steps)",
                doc: "inner SET under its own budget, error ignored",
            },
            Spec {
                name: "halfpanic",
                args: "()",
                doc: "sets x on half the nodes, then panics",
            },
            Spec {
                name: "edges",
                args: "()",
                doc: "links each node to the next through a statement",
            },
            Spec {
                name: "halferr",
                args: "()",
                doc: "sets x on half the nodes, then fails",
            },
        ];
        P
    }
    fn call(&self, g: &mut Graph, p: &str, a: &Args) -> Result<QueryResult> {
        match p {
            "swallow" => {
                let lim = QueryLimits {
                    max_steps: Some(a.usize("steps", 3000) as u64),
                    ..Default::default()
                };
                let src = a
                    .str("src")
                    .unwrap_or_else(|| "MATCH (n:N) SET n.x = 1".to_string());
                let r = query::execute_capped(g, &src, &[], lim);
                Ok(QueryResult::message(format!(
                    "inner said: {:?}",
                    r.err().map(|e| e.to_string())
                )))
            }
            "halfpanic" => {
                let ids = g.node_ids();
                for id in &ids[..ids.len() / 2] {
                    g.set_node_prop(*id, "x", Value::Int(1))?;
                }
                panic!("procedure bug half way through");
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
            _ => {
                query::execute(
                    g,
                    "MATCH (a:N), (b:N) WHERE b.i = a.i + 1 CREATE (a)-[:L]->(b)",
                )?;
                Ok(QueryResult::message("linked"))
            }
        }
    }
}

/// Follows edge writes into its own keyspace.
struct EdgeLog;
impl Extension for EdgeLog {
    fn name(&self) -> &str {
        "edgelog"
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "follows edge writes"
    }
    fn procedures(&self) -> &[Spec] {
        &[]
    }
    fn call(&self, _: &mut Graph, _: &str, _: &Args) -> Result<QueryResult> {
        Ok(QueryResult::message("none"))
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
        _: u64,
        _: Option<&NodeImage>,
        _: Option<&NodeImage>,
    ) -> Result<()> {
        Ok(())
    }
    fn on_edge_write(
        &self,
        g: &mut Graph,
        id: u64,
        _: Option<&EdgeImage>,
        after: Option<&EdgeImage>,
    ) -> Result<()> {
        let k = id.to_be_bytes().to_vec();
        if after.is_some() {
            g.ext_write("edgelog", vec![(k, b"1".to_vec())], vec![])
        } else {
            g.ext_write("edgelog", vec![], vec![k])
        }
    }
}

fn graph(n: usize) -> Graph {
    ext::register(Arc::new(P2));
    ext::register(Arc::new(EdgeLog));
    let mut g = Graph::memory();
    g.autocommit = false;
    for i in 0..n {
        g.add_node(
            &["N".into()],
            vec![
                ("i".into(), Value::Int(i as i64)),
                (
                    "name".into(),
                    Value::from(format!("pump {} valve {}", i % 13, i % 7)),
                ),
            ],
        )
        .unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    query::execute(&mut g, "CREATE EXTENSION p2").unwrap();
    g
}

fn count(g: &mut Graph, src: &str) -> i64 {
    query::execute(g, src)
        .unwrap()
        .rows
        .first()
        .and_then(|r| r[0].as_i64())
        .unwrap_or(0)
}

fn steps(n: u64) -> QueryLimits {
    QueryLimits {
        max_steps: Some(n),
        ..Default::default()
    }
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Auto,
    BeginStmt,
    BeginApi,
    Field,
}
const MODES: [Mode; 4] = [Mode::Auto, Mode::BeginStmt, Mode::BeginApi, Mode::Field];

fn open(g: &mut Graph, m: Mode) {
    match m {
        Mode::Auto => {}
        Mode::BeginStmt => {
            query::execute(g, "BEGIN").unwrap();
        }
        Mode::BeginApi => g.begin().unwrap(),
        Mode::Field => g.autocommit = false,
    }
}

fn close(g: &mut Graph, m: Mode) -> String {
    let r = match m {
        Mode::Auto => return "autocommit".into(),
        Mode::BeginStmt => query::execute(g, "COMMIT").map(|_| ()),
        Mode::BeginApi | Mode::Field => g.commit(),
    };
    if matches!(m, Mode::Field) {
        g.autocommit = true;
    }
    match r {
        Ok(()) => "committed".into(),
        Err(e) => format!("commit refused: {e}"),
    }
}

// An inner statement stopped part-way by its own budget, its error swallowed
// by the procedure: none of it is committed.
#[test]
fn a_swallowed_inner_stop_commits_nothing_of_the_statement() {
    let mut partial = Vec::new();
    for m in MODES {
        for budget in (8_000u64..=20_000).step_by(500) {
            let mut g = graph(2_000);
            open(&mut g, m);
            let r = query::execute(
                &mut g,
                &format!("CALL p2.swallow(steps: {budget}, src: \"CALL pagerank(write: 'x')\")"),
            );
            let msg = format!(
                "{:?}",
                r.as_ref()
                    .map(|r| r.message.clone())
                    .map_err(|e| e.to_string())
            );
            let c = close(&mut g, m);
            let x = count(&mut g, "MATCH (n:N) WHERE n.x IS NOT NULL RETURN count(n)");
            eprintln!(
                "{m:?} inner budget {budget}: CALL -> {msg}; {c}; nodes with x after: {x} of 2000"
            );
            if x > 0 && x < 2_000 {
                partial.push((m, budget, x));
            }
        }
    }
    assert!(
        partial.is_empty(),
        "part of a stopped statement was committed: {partial:?}"
    );
}

// An inner statement that fails after writing (no budget involved), its
// error swallowed by the procedure that ran it: the outer statement fails,
// says which inner statement failed, and none of it is committed.
#[test]
fn a_swallowed_inner_failure_fails_the_outer_statement_as_a_whole() {
    let mut partial = Vec::new();
    for m in MODES {
        let mut g = graph(2_000);
        open(&mut g, m);
        let r = query::execute(
            &mut g,
            "CALL p2.swallow(steps: 100000000, src: \"CALL p2.halferr()\")",
        );
        let msg = format!(
            "{:?}",
            r.as_ref()
                .map(|r| r.message.clone())
                .map_err(|e| e.to_string())
        );
        let e = r.err().expect("the outer statement failed").to_string();
        assert!(
            e.contains("CALL p2.halferr()") && e.contains("procedure failed half way through"),
            "{m:?}: {e}"
        );
        let c = close(&mut g, m);
        let x = count(&mut g, "MATCH (n:N) WHERE n.x IS NOT NULL RETURN count(n)");
        eprintln!("{m:?}: CALL -> {msg}; {c}; nodes with x after: {x} of 2000");
        if x > 0 && x < 2_000 {
            partial.push((m, x));
        }
    }
    assert!(
        partial.is_empty(),
        "part of a failed statement was committed: {partial:?}"
    );
}

// A panic in a statement inside a transaction: the caller can commit none
// of it, and a block is spent.
#[test]
fn a_panic_inside_a_transaction_commits_nothing_of_the_statement() {
    let mut partial = Vec::new();
    for m in MODES {
        let mut g = graph(2_000);
        open(&mut g, m);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            query::execute(&mut g, "CALL p2.halfpanic()").map(|_| ())
        }));
        assert!(r.is_err(), "it panicked");
        let spent = g.spent().map(str::to_string);
        if matches!(m, Mode::BeginStmt | Mode::BeginApi) {
            assert!(spent.is_some(), "{m:?}: the block is not spent");
        }
        let c = close(&mut g, m);
        let x = count(&mut g, "MATCH (n:N) WHERE n.x IS NOT NULL RETURN count(n)");
        eprintln!("{m:?}: after the panic spent={spent:?}; {c}; nodes with x: {x} of 2000");
        if x > 0 {
            partial.push((m, x));
        }
    }
    assert!(
        partial.is_empty(),
        "part of a panicked statement was committed: {partial:?}"
    );
}

fn state(g: &Graph) -> (String, Vec<(Vec<u8>, Vec<u8>)>, Vec<(Vec<u8>, Vec<u8>)>) {
    let scan = |ns: &str| {
        let mut out = Vec::new();
        g.ext_scan(ns, b"", &mut |k, v| {
            out.push((k.to_vec(), v.to_vec()));
            true
        });
        out
    };
    (query::export_jsonl(g), scan("context"), scan("edgelog"))
}

// Budget stops while hooks write (the text index on `name`, an extension
// that follows edges), at many stop points, in each kind of transaction.
#[test]
fn budget_stops_with_hooks_leave_nothing_in_any_transaction() {
    let mut g = graph(600);
    for s in [
        "CREATE EXTENSION context",
        "CREATE EXTENSION edgelog",
        "CALL context.index(keys: [\"name\"])",
    ] {
        query::execute(&mut g, s).unwrap();
    }
    let search = "CALL context.search(query: \"pump 3\")";
    let answer = format!("{:?}", query::execute(&mut g, search).unwrap().rows);
    let base = state(&g);
    let mut tried = 0;
    let mut bad = Vec::new();
    for src in [
        "CALL pagerank(write: \"name\")",
        "CALL p2.edges()",
        "MATCH (n:N) SET n.name = 'x ' + n.i",
    ] {
        // the steps the full statement needs
        let (mut lo, mut hi) = (0u64, 1u64 << 32);
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            let before = state(&g);
            match query::execute_capped(&mut g, src, &[], steps(mid)) {
                Ok(_) => {
                    hi = mid;
                    // put g back: rebuild from scratch
                    g = graph(600);
                    for s in [
                        "CREATE EXTENSION context",
                        "CREATE EXTENSION edgelog",
                        "CALL context.index(keys: [\"name\"])",
                    ] {
                        query::execute(&mut g, s).unwrap();
                    }
                }
                Err(_) => {
                    lo = mid;
                    assert!(state(&g) == before);
                }
            }
        }
        let need = hi;
        let mut s = 1u64;
        while s < need {
            for m in MODES {
                open(&mut g, m);
                let e = query::execute_capped(&mut g, src, &[], steps(s))
                    .err()
                    .expect("stopped")
                    .to_string();
                let c = close(&mut g, m);
                if g.spent().is_some() || !g.autocommit {
                    bad.push(format!(
                        "{src} {m:?} at {s}: handle left in a transaction ({c})"
                    ));
                    let _ = g.rollback();
                    g.autocommit = true;
                }
                if state(&g) != base {
                    bad.push(format!("{src} {m:?} at {s}: left something ({e}; {c})"));
                }
                let now = format!("{:?}", query::execute(&mut g, search).unwrap().rows);
                if now != answer {
                    bad.push(format!(
                        "{src} {m:?} at {s}: the text index answers differently"
                    ));
                }
                tried += 1;
            }
            s = if s < 64 { s * 2 } else { s + s / 4 + 1 };
        }
        eprintln!("{src}: needs {need} steps");
    }
    // and the hooks still follow the next write
    query::execute(&mut g, "MATCH (n:N) WHERE n.i = 3 SET n.name = 'zebra'").unwrap();
    let z = query::execute(&mut g, "CALL context.search(query: \"zebra\")").unwrap();
    eprintln!("stopped {tried} times; then zebra found: {}", z.rows.len());
    assert!(
        !z.rows.is_empty(),
        "the text hook did not follow the next write"
    );
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// Point 5: the in-memory projection build is counted by nodes only. On a
// dense graph, how much work does a tiny step budget let through?
#[test]
#[ignore]
fn a_step_budget_bounds_a_dense_projection_build_by_its_nodes_only() {
    let mut g = Graph::memory();
    g.autocommit = false;
    let ids: Vec<u64> = (0..100)
        .map(|_| g.add_node(&["N".into()], vec![]).unwrap())
        .collect();
    let mut x = 7u64;
    for _ in 0..3_000_000u64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        g.add_edge(
            ids[(x % 100) as usize],
            ids[((x >> 9) % 100) as usize],
            "E",
            vec![],
        )
        .unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    for (src, budget) in [
        ("CALL degree(tier: \"mem\", top: 1)", 200u64),
        ("CALL pagerank(tier: \"mem\", top: 1)", 200),
        ("CALL pagerank(tier: \"mem\", top: 1)", 1),
        ("CALL degree(tier: \"ooc\", top: 1)", 200),
    ] {
        let t = Instant::now();
        let r = query::execute_capped(&mut g, src, &[], steps(budget));
        eprintln!(
            "{src} max_steps {budget}: {:?} after {:?} (3,000,000 edges read)",
            r.map(|_| "finished").map_err(|e| e.to_string()),
            t.elapsed()
        );
    }
    let t = Instant::now();
    query::execute(&mut g, "CALL degree(tier: \"mem\", top: 1)").unwrap();
    eprintln!("unbudgeted degree: {:?}", t.elapsed());
}

// Point 4's exceptions: per-node array allocation and resets are not cut.
// A graph of a great many nodes and few edges, a 50 ms budget.
#[test]
#[ignore]
fn fifty_ms_on_five_million_nodes_and_few_edges() {
    let mut g = Graph::memory();
    g.autocommit = false;
    let n = 5_000_000u64;
    let mut prev = None;
    for i in 0..n {
        let id = g.add_node(&[], vec![]).unwrap();
        if i % 1000 == 0 {
            if let Some(p) = prev {
                g.add_edge(p, id, "E", vec![]).unwrap();
            }
            prev = Some(id);
        }
    }
    g.commit().unwrap();
    g.autocommit = true;
    let mut worst = (std::time::Duration::ZERO, String::new());
    for ms in [50u64, 150, 300, 600, 1200, 2400] {
        let lim = QueryLimits {
            max_millis: Some(ms),
            ..Default::default()
        };
        for src in [
            "CALL degree(top: 1)",
            "CALL pagerank(top: 1)",
            "CALL pagerank(top: 1, tier: \"ooc\")",
            "CALL betweenness(samples: 3, top: 1)",
            "CALL closeness(top: 1)",
            "CALL components(top: 1)",
            "CALL communities(top: 1)",
            "CALL kcore(top: 1)",
            "CALL triangles(top: 1)",
            "CALL scc(top: 1)",
            "CALL levels(top: 1)",
            "CALL mst(top: 1)",
            "CALL sssp(from: 1, top: 1)",
            "CALL toposort(top: 1)",
        ] {
            let t = Instant::now();
            let r = query::execute_capped(&mut g, src, &[], lim);
            let took = t.elapsed();
            let over = took.saturating_sub(std::time::Duration::from_millis(ms));
            if over > worst.0 {
                worst = (over, format!("{src} at {ms} ms"));
            }
            eprintln!(
                "{ms:>5} ms budget {took:>12.1?}  {src} -> {:?}",
                r.map(|_| "finished").map_err(|e| e.to_string())
            );
        }
    }
    eprintln!("worst overrun {:?}: {}", worst.0, worst.1);
}
