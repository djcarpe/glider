use std::path::PathBuf;

use glider::graph::{Dir, Graph};
use glider::store::Sync;
use glider::value::Value;
use glider::{algo, query};

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-test-{}-{}.gldb", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn q(g: &mut Graph, src: &str) -> glider::QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("query `{}` failed: {}", src, e))
}

#[test]
fn persists_and_reopens() {
    let path = temp("persist");
    {
        let mut g = Graph::open(&path, Sync::Always).unwrap();
        let a = g
            .add_node(
                &["Person".into()],
                vec![("name".into(), Value::from("Ada"))],
            )
            .unwrap();
        let b = g
            .add_node(
                &["Person".into()],
                vec![("name".into(), Value::from("Bob"))],
            )
            .unwrap();
        g.add_edge(a, b, "KNOWS", vec![("since".into(), Value::Int(2020))])
            .unwrap();
        g.commit().unwrap();
    }
    {
        let g = Graph::open(&path, Sync::Always).unwrap();
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
        assert_eq!(g.node_prop(1, "name").unwrap().to_string(), "Ada");
        assert_eq!(g.edge_prop(1, "since").unwrap().as_i64(), Some(2020));
        assert_eq!(g.degree(1, Dir::Out), 1);
        assert_eq!(g.degree(2, Dir::In), 1);
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn torn_tail_is_discarded() {
    let path = temp("torn");
    {
        let mut g = Graph::open(&path, Sync::Always).unwrap();
        g.add_node(&["A".into()], vec![("x".into(), Value::Int(1))])
            .unwrap();
        g.commit().unwrap();
    }
    // Simulate a crash mid-write by appending garbage.
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&[0u8, 200, 0, 0, 0, 7, 7, 7]).unwrap();
    }
    {
        let g = Graph::open(&path, Sync::Always).unwrap();
        assert_eq!(g.node_count(), 1, "committed data must survive a torn tail");
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn uncommitted_transaction_is_not_visible_after_reopen() {
    let path = temp("tx");
    {
        let mut g = Graph::open(&path, Sync::Always).unwrap();
        g.add_node(&["Kept".into()], vec![]).unwrap();
        g.commit().unwrap();
        g.autocommit = false;
        g.add_node(&["Dropped".into()], vec![]).unwrap();
        // No commit. Drop the graph without flushing the transaction.
        std::mem::forget(g);
    }
    {
        // `mem::forget` above simulates a crash, which also leaks the write
        // lock. A real crash ends the process and the lock is detectably
        // stale; here the holder pid is this very test runner, so say so
        // explicitly rather than teaching the lock to trust itself.
        let g = Graph::open_forced(&path, Sync::Always).unwrap();
        assert_eq!(g.node_count(), 1);
        assert_eq!(g.node_labels(1), vec!["Kept".to_string()]);
    }
    std::fs::remove_file(glider::store::lock_path(&path)).ok();
    std::fs::remove_file(&path).ok();
}

#[test]
fn compaction_preserves_state_and_shrinks() {
    let path = temp("compact");
    let mut g = Graph::open(&path, Sync::Normal).unwrap();
    for i in 0..200 {
        g.add_node(&["N".into()], vec![("i".into(), Value::Int(i))])
            .unwrap();
    }
    // Churn: overwrite properties many times, then delete most nodes.
    for i in 1..=200u64 {
        for r in 0..5 {
            g.set_node_prop(i, "i", Value::Int(r)).unwrap();
        }
    }
    for i in 1..=190u64 {
        g.delete_node(i).unwrap();
    }
    g.commit().unwrap();
    let before = g.file_len();
    g.compact().unwrap();
    let after = g.file_len();
    assert!(
        after < before,
        "compaction should shrink: {} -> {}",
        before,
        after
    );
    assert_eq!(g.node_count(), 10);
    drop(g);

    let g = Graph::open(&path, Sync::Normal).unwrap();
    assert_eq!(g.node_count(), 10);
    assert_eq!(g.node_prop(200, "i").unwrap().as_i64(), Some(4));
    std::fs::remove_file(&path).ok();
}

#[test]
fn deleting_a_node_detaches_its_edges() {
    let mut g = Graph::memory();
    let a = g.add_node(&["A".into()], vec![]).unwrap();
    let b = g.add_node(&["B".into()], vec![]).unwrap();
    let c = g.add_node(&["C".into()], vec![]).unwrap();
    g.add_edge(a, b, "E", vec![]).unwrap();
    g.add_edge(b, c, "E", vec![]).unwrap();
    assert_eq!(g.edge_count(), 2);
    g.delete_node(b).unwrap();
    assert_eq!(g.edge_count(), 0);
    assert_eq!(g.degree(a, Dir::Both), 0);
    assert_eq!(g.degree(c, Dir::Both), 0);
}

#[test]
fn query_create_match_and_aggregate() {
    let mut g = Graph::memory();
    q(&mut g, r#"CREATE (a:Person {name:"Ada", age:36})"#);
    q(&mut g, r#"CREATE (b:Person {name:"Bob", age:41})"#);
    q(&mut g, r#"CREATE (c:Person {name:"Cy", age:29})"#);
    q(
        &mut g,
        r#"MATCH (a:Person),(b:Person) WHERE a.name="Ada" AND b.name="Bob" CREATE (a)-[:KNOWS {since:2020}]->(b)"#,
    );
    q(
        &mut g,
        r#"MATCH (a:Person),(b:Person) WHERE a.name="Ada" AND b.name="Cy" CREATE (a)-[:KNOWS]->(b)"#,
    );

    let r = q(
        &mut g,
        r#"MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name, count(b) AS n"#,
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0].to_string(), "Ada");
    assert_eq!(r.rows[0][1].as_i64(), Some(2));

    let r = q(
        &mut g,
        r#"MATCH (p:Person) WHERE p.age > 30 RETURN p.name ORDER BY p.name DESC"#,
    );
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0].to_string(), "Bob");
    assert_eq!(r.rows[1][0].to_string(), "Ada");

    let r = q(
        &mut g,
        r#"MATCH (p:Person) RETURN avg(p.age) AS a, max(p.age) AS m"#,
    );
    assert_eq!(r.rows[0][1].as_i64(), Some(41));

    let r = q(
        &mut g,
        r#"MATCH (p:Person) WHERE p.name CONTAINS "o" RETURN p.name"#,
    );
    assert_eq!(r.rows.len(), 1);

    let r = q(
        &mut g,
        r#"MATCH (p:Person) WHERE p.name IN ["Ada","Cy"] RETURN p.name"#,
    );
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn query_variable_length_paths() {
    let mut g = Graph::memory();
    q(
        &mut g,
        r#"CREATE (:N {n:1})-[:R]->(:N {n:2})-[:R]->(:N {n:3})-[:R]->(:N {n:4})"#,
    );
    let r = q(
        &mut g,
        r#"MATCH (a:N {n:1})-[:R*1..2]->(b) RETURN b.n ORDER BY b.n"#,
    );
    assert_eq!(r.rows.len(), 2);
    let r = q(
        &mut g,
        r#"MATCH (a:N {n:1})-[:R*1..3]->(b) RETURN b.n ORDER BY b.n"#,
    );
    assert_eq!(r.rows.len(), 3);
    let r = q(&mut g, r#"MATCH (a:N {n:1})-[:R*2..2]->(b) RETURN b.n"#);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0].as_i64(), Some(3));
}

#[test]
fn query_set_remove_delete() {
    let mut g = Graph::memory();
    q(&mut g, r#"CREATE (a:P {name:"x"})"#);
    q(&mut g, r#"MATCH (n:P) SET n.age = 7, n:Tagged"#);
    assert_eq!(g.node_prop(1, "age").unwrap().as_i64(), Some(7));
    assert!(g.node_labels(1).contains(&"Tagged".to_string()));

    q(&mut g, r#"MATCH (n:P) REMOVE n.age"#);
    assert!(g.node_prop(1, "age").is_none());

    q(&mut g, r#"MATCH (n) WHERE id(n) = 1 DETACH DELETE n"#);
    assert_eq!(g.node_count(), 0);
}

#[test]
fn index_is_used_and_maintained() {
    let mut g = Graph::memory();
    q(&mut g, "INDEX ON :Person(email)");
    for i in 0..50 {
        q(
            &mut g,
            &format!(r#"CREATE (:Person {{email:"u{}@x.com", n:{}}})"#, i, i),
        );
    }
    assert!(g.has_index("Person", "email"));
    let hits = g
        .indexed_lookup("Person", "email", &Value::from("u7@x.com"))
        .unwrap();
    assert_eq!(hits.len(), 1);

    // Updating the indexed property moves the entry.
    q(
        &mut g,
        r#"MATCH (p:Person {email:"u7@x.com"}) SET p.email = "moved@x.com""#,
    );
    assert!(g
        .indexed_lookup("Person", "email", &Value::from("u7@x.com"))
        .unwrap()
        .is_empty());
    assert_eq!(
        g.indexed_lookup("Person", "email", &Value::from("moved@x.com"))
            .unwrap()
            .len(),
        1
    );

    // Deleting removes it.
    q(
        &mut g,
        r#"MATCH (p:Person {email:"moved@x.com"}) DETACH DELETE p"#,
    );
    assert!(g
        .indexed_lookup("Person", "email", &Value::from("moved@x.com"))
        .unwrap()
        .is_empty());
}

#[test]
fn shortest_path_weighted_and_unweighted() {
    let mut g = Graph::memory();
    // 1 -> 2 -> 4 costs 2 hops but weight 10; 1 -> 3 -> 4 costs 2 hops weight 2.
    for n in 1..=4 {
        g.add_node(&["N".into()], vec![("n".into(), Value::Int(n))])
            .unwrap();
    }
    g.add_edge(1, 2, "E", vec![("w".into(), Value::Int(5))])
        .unwrap();
    g.add_edge(2, 4, "E", vec![("w".into(), Value::Int(5))])
        .unwrap();
    g.add_edge(1, 3, "E", vec![("w".into(), Value::Int(1))])
        .unwrap();
    g.add_edge(3, 4, "E", vec![("w".into(), Value::Int(1))])
        .unwrap();
    g.add_edge(1, 4, "E", vec![("w".into(), Value::Int(99))])
        .unwrap();

    let r = q(&mut g, "CALL shortestpath(from: 1, to: 4)");
    assert_eq!(r.rows.len(), 2, "unweighted should take the direct edge");

    let r = q(&mut g, r#"CALL shortestpath(from: 1, to: 4, weight: "w")"#);
    assert_eq!(r.rows.len(), 3);
    assert_eq!(
        r.rows[1][1].as_i64(),
        Some(3),
        "should route through node 3"
    );
}

#[test]
fn algorithms_agree_with_hand_computed_values() {
    let mut g = Graph::memory();
    // A triangle 1-2-3 plus a pendant 4 hanging off 1.
    for _ in 0..4 {
        g.add_node(&["N".into()], vec![]).unwrap();
    }
    g.add_edge(1, 2, "E", vec![]).unwrap();
    g.add_edge(2, 3, "E", vec![]).unwrap();
    g.add_edge(3, 1, "E", vec![]).unwrap();
    g.add_edge(1, 4, "E", vec![]).unwrap();

    let both = g.csr(Dir::Both, None, None);
    let (tri, total) = algo::triangles(&both);
    assert_eq!(total, 1);
    assert_eq!(tri[0], 1);
    assert_eq!(tri[3], 0);

    let clustering = algo::clustering(&both, &tri);
    assert!(
        (clustering[1] - 1.0).abs() < 1e-9,
        "node 2 closes its only triangle"
    );
    assert!((clustering[0] - (1.0 / 3.0)).abs() < 1e-9);

    let cores = algo::core_numbers(&both);
    assert_eq!(cores[0], 2);
    assert_eq!(cores[3], 1);

    let (_, ncomp) = algo::components(&both);
    assert_eq!(ncomp, 1);

    // Directed cycle 1->2->3->1 is one SCC; node 4 is its own.
    let out = g.csr(Dir::Out, None, None);
    let (_, nscc) = algo::strongly_connected(&out);
    assert_eq!(nscc, 2);
    assert!(
        algo::topological_sort(&out).is_none(),
        "cycle blocks topo sort"
    );
    assert!(algo::find_cycle(&out).is_some());

    // PageRank sums to 1 and the pendant with no out-edges still gets mass.
    let pr = algo::pagerank(&out, 0.85, 100, 1e-12);
    let sum: f64 = pr.scores.iter().sum();
    assert!(
        (sum - 1.0).abs() < 1e-6,
        "pagerank must be a distribution, got {}",
        sum
    );
}

#[test]
fn scc_handles_deep_chains_without_stack_overflow() {
    let mut g = Graph::memory();
    g.autocommit = false;
    let n = 100_000u64;
    for _ in 0..n {
        g.add_node(&["N".into()], vec![]).unwrap();
    }
    for i in 1..n {
        g.add_edge(i, i + 1, "E", vec![]).unwrap();
    }
    let csr = g.csr(Dir::Out, None, None);
    let (_, count) = algo::strongly_connected(&csr);
    assert_eq!(count, n as u32, "a chain has n singleton SCCs");
    let order = algo::topological_sort(&csr).expect("a chain is acyclic");
    assert_eq!(order.len(), n as usize);
}

#[test]
fn betweenness_on_a_star_graph() {
    let mut g = Graph::memory();
    for _ in 0..5 {
        g.add_node(&["N".into()], vec![]).unwrap();
    }
    for leaf in 2..=5u64 {
        g.add_edge(1, leaf, "E", vec![]).unwrap();
    }
    let both = g.csr(Dir::Both, None, None);
    let bc = algo::betweenness(&both, None, false);
    assert!(bc[0] > 0.0, "the hub is on every path");
    for leaf in 1..5 {
        assert_eq!(bc[leaf], 0.0, "leaves are on no shortest path");
    }
}

#[test]
fn mst_picks_the_cheap_edges() {
    let mut g = Graph::memory();
    for _ in 0..3 {
        g.add_node(&["N".into()], vec![]).unwrap();
    }
    g.add_edge(1, 2, "E", vec![("w".into(), Value::Int(1))])
        .unwrap();
    g.add_edge(2, 3, "E", vec![("w".into(), Value::Int(1))])
        .unwrap();
    g.add_edge(1, 3, "E", vec![("w".into(), Value::Int(50))])
        .unwrap();
    let both = g.csr(Dir::Both, None, Some("w"));
    let (edges, total) = algo::minimum_spanning_forest(&both);
    assert_eq!(edges.len(), 2);
    assert_eq!(total, 2.0);
}

#[test]
fn write_back_stores_scores_as_properties() {
    let mut g = Graph::memory();
    q(&mut g, r#"CREATE (:P {name:"a"})-[:R]->(:P {name:"b"})"#);
    q(&mut g, r#"CALL pagerank(iterations: 10, write: "rank")"#);
    assert!(g.node_prop(1, "rank").is_some());
    assert!(g.node_prop(2, "rank").unwrap().as_f64().unwrap() > 0.0);

    let r = q(&mut g, "MATCH (p:P) RETURN p.name ORDER BY p.name LIMIT 1");
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn jsonl_round_trip() {
    let mut g = Graph::memory();
    q(
        &mut g,
        r#"CREATE (a:Person {name:"Ada", age:36})-[:KNOWS {since:2020}]->(b:Person {name:"Bob"})"#,
    );
    let dump = query::export_jsonl(&g);

    let mut g2 = Graph::memory();
    let (n, e) = query::import_jsonl(&mut g2, &dump).unwrap();
    assert_eq!(n, 2);
    assert_eq!(e, 1);
    assert_eq!(g2.node_count(), 2);
    assert_eq!(g2.edge_count(), 1);
    assert_eq!(g2.node_prop(1, "name").unwrap().to_string(), "Ada");
    assert_eq!(g2.edge_prop(1, "since").unwrap().as_i64(), Some(2020));
    assert_eq!(g2.edge_type_name(1), Some("KNOWS"));
}

#[test]
fn value_ordering_is_total_and_sane() {
    let vals = vec![
        Value::Null,
        Value::Bool(false),
        Value::Int(-5),
        Value::Float(0.5),
        Value::Int(10),
        Value::Text("a".into()),
    ];
    for i in 0..vals.len() {
        for j in 0..vals.len() {
            let a = vals[i].total_cmp(&vals[j]);
            let b = vals[j].total_cmp(&vals[i]);
            assert_eq!(a, b.reverse(), "ordering must be antisymmetric");
        }
    }
    assert!(Value::Int(2).total_cmp(&Value::Float(2.5)) == std::cmp::Ordering::Less);
    assert!(Value::Int(2) == Value::Float(2.0));
}

#[test]
fn bad_queries_report_errors_instead_of_panicking() {
    let mut g = Graph::memory();
    for bad in [
        "MATCH (",
        "MATCH (n) RETURN",
        "CREATE (a)-[]->(b)",
        "CALL nosuchalgorithm()",
        "MATCH (n) RETURN n ORDER BY x.y",
        "SELECT * FROM nodes",
        "CALL shortestpath(from: 999, to: 1)",
        "MATCH (n) WHERE n.x = RETURN n",
    ] {
        assert!(
            query::execute(&mut g, bad).is_err(),
            "`{}` should be an error",
            bad
        );
    }
}

/// A crash that leaves a torn tail must end the generation. Bytes above the
/// last commit may already sit in a replica, and the next write will put
/// different bytes at those offsets — a replica that kept appending to the
/// same lineage would restore a file that is CRC-valid and wrong.
#[test]
fn discarding_a_torn_tail_starts_a_new_generation() {
    let dir = std::env::temp_dir().join(format!("glider-torn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.gldb");

    let mut g = glider::Graph::open(&path, glider::Sync::Always).unwrap();
    g.add_node(&["N".into()], vec![]).unwrap();
    g.commit().unwrap();
    drop(g);
    let before = glider::store::read_header(&path).unwrap().generation_hex();

    // A writer caught mid-transaction: bytes past the last commit marker.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(&[9u8; 96]).unwrap();
    f.sync_all().unwrap();

    let g = glider::Graph::open(&path, glider::Sync::Always).unwrap();
    assert_eq!(g.node_count(), 1, "committed data survives");
    drop(g);

    let after = glider::store::read_header(&path).unwrap().generation_hex();
    assert_ne!(before, after, "a discarded tail must end the lineage");

    // A clean reopen must NOT churn the generation.
    let g = glider::Graph::open(&path, glider::Sync::Always).unwrap();
    drop(g);
    assert_eq!(
        glider::store::read_header(&path).unwrap().generation_hex(),
        after,
        "a clean open leaves the generation alone"
    );
}

#[test]
fn a_second_writer_is_refused_rather_than_corrupting() {
    let path = temp("lock");
    let first = Graph::open(&path, Sync::Normal).unwrap();

    let err = match Graph::open(&path, Sync::Normal) {
        Ok(_) => panic!("a second writer was allowed in"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("locked"), "unexpected error: {err}");
    assert!(
        err.contains(&std::process::id().to_string()),
        "the error should name the holder: {err}"
    );

    // Forcing past it works, for when you know the holder is dead.
    assert!(Graph::open_forced(&path, Sync::Normal).is_ok());

    drop(first);
    // Released on drop, so the next open is clean.
    assert!(Graph::open(&path, Sync::Normal).is_ok());
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(glider::store::lock_path(&path)).ok();
}

#[test]
fn a_lock_left_by_a_dead_process_is_reclaimed() {
    let path = temp("stalelock");
    {
        let _g = Graph::open(&path, Sync::Normal).unwrap();
    }
    // A lock naming a pid that cannot be running.
    std::fs::write(
        glider::store::lock_path(&path),
        b"{\"pid\":4294967290,\"since\":0}\n",
    )
    .unwrap();

    let opened = Graph::open(&path, Sync::Normal).is_ok();
    if cfg!(target_os = "linux") {
        // We can prove the holder is gone, so take over without asking.
        assert!(opened, "a provably dead holder should not block an open");
    } else {
        // Elsewhere, unknown means alive: refuse and let the human decide.
        assert!(!opened);
    }
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(glider::store::lock_path(&path)).ok();
}

// ------------------------------------------------------------------ planning

fn rows(g: &mut Graph, q: &str) -> Vec<Vec<glider::Value>> {
    glider::execute(g, q).unwrap().rows
}

#[test]
fn the_planner_anchors_on_the_selective_end() {
    let mut g = Graph::memory();
    glider::execute(&mut g, "INDEX ON :Person(email)").unwrap();
    for i in 0..200 {
        glider::execute(
            &mut g,
            &format!(r#"CREATE (:Person {{name:"p{i}", email:"p{i}@x.io"}})"#),
        )
        .unwrap();
    }
    glider::execute(
        &mut g,
        r#"MATCH (a:Person {name:"p0"}), (b:Person {name:"p1"}) CREATE (a)-[:KNOWS]->(b)"#,
    )
    .unwrap();

    // The selective side is written second; the plan must still start there.
    let plan = rows(
        &mut g,
        r#"EXPLAIN MATCH (a:Person)-[:KNOWS]->(b:Person {email:"p1@x.io"}) RETURN a"#,
    );
    let anchor = format!("{:?}", plan[0]);
    assert!(anchor.contains("index :Person(email)"), "{anchor}");
    assert!(anchor.contains("(b:Person)"), "{anchor}");
    let expand = format!("{:?}", plan[1]);
    assert!(expand.contains("reversed"), "{expand}");

    // And the answer is the same as the forward spelling.
    let backwards = rows(
        &mut g,
        r#"MATCH (a:Person)-[:KNOWS]->(b:Person {email:"p1@x.io"}) RETURN a.name"#,
    );
    let forwards = rows(
        &mut g,
        r#"MATCH (b:Person {email:"p1@x.io"})<-[:KNOWS]-(a:Person) RETURN a.name"#,
    );
    assert_eq!(backwards, forwards);
    assert_eq!(backwards.len(), 1);
}

#[test]
fn an_id_predicate_is_pushed_into_the_anchor() {
    let mut g = Graph::memory();
    for i in 0..50 {
        glider::execute(&mut g, &format!("CREATE (:N {{i:{i}}})")).unwrap();
    }
    glider::execute(
        &mut g,
        "MATCH (a:N {i:0}), (b:N {i:1}) CREATE (a)-[:R]->(b)",
    )
    .unwrap();

    let plan = rows(
        &mut g,
        "EXPLAIN MATCH (a)-[:R]->(b) WHERE id(a) = 1 RETURN b",
    );
    let text = format!("{plan:?}");
    assert!(text.contains("id(a) predicate"), "{text}");
    assert!(text.contains("pushed into anchor"), "{text}");

    // An OR is not a pin: the predicate need not hold, so it must not anchor.
    let plan = rows(
        &mut g,
        "EXPLAIN MATCH (a)-[:R]->(b) WHERE id(a) = 1 OR id(a) = 2 RETURN b",
    );
    assert!(!format!("{plan:?}").contains("id(a) predicate"));
}

#[test]
fn a_middle_anchor_expands_in_both_directions() {
    let mut g = Graph::memory();
    glider::execute(
        &mut g,
        r#"CREATE (:Person {name:"p"}), (:Person {name:"q"}), (:Team {name:"infra"})"#,
    )
    .unwrap();
    for who in ["p", "q"] {
        glider::execute(
            &mut g,
            &format!(r#"MATCH (a:Person {{name:"{who}"}}), (t:Team) CREATE (a)-[:IN]->(t)"#),
        )
        .unwrap();
    }

    let plan = rows(
        &mut g,
        "EXPLAIN MATCH (p:Person)-[:IN]->(t:Team)<-[:IN]-(q:Person) RETURN p, q",
    );
    assert!(
        format!("{:?}", plan[0]).contains("(t:Team)"),
        "anchor should be the Team"
    );
    assert_eq!(plan.len(), 3, "one anchor and two expansions");

    // Both people on both sides, including the reflexive pairs.
    let found = rows(
        &mut g,
        "MATCH (p:Person)-[:IN]->(t:Team)<-[:IN]-(q:Person) RETURN p.name, q.name",
    );
    assert_eq!(found.len(), 4);
}

#[test]
fn explain_reports_without_executing() {
    let mut g = Graph::memory();
    glider::execute(&mut g, r#"CREATE (:N {v:1})"#).unwrap();
    let before = g.node_count();

    let r = glider::execute(&mut g, "EXPLAIN MATCH (n:N) SET n.v = 99").unwrap();
    assert!(!r.rows.is_empty(), "a plan should come back");
    assert_eq!(g.node_count(), before);

    let v = rows(&mut g, "MATCH (n:N) RETURN n.v");
    assert_eq!(v[0][0], glider::Value::Int(1), "SET must not have run");
}

#[test]
fn verify_distinguishes_a_torn_tail_from_corruption() {
    let path = temp("verify");
    {
        let mut g = Graph::open(&path, Sync::Always).unwrap();
        for i in 0..20 {
            g.add_node(&["N".into()], vec![("i".into(), glider::Value::Int(i))])
                .unwrap();
        }
        g.commit().unwrap();
    }
    let clean = glider::store::verify(&path).unwrap();
    assert_eq!(clean.records, 20);
    assert!(clean.bad_offset.is_none());
    assert_eq!(clean.committed_len, clean.file_len);

    // Junk after the last commit is an ordinary torn tail: reported at or
    // above the commit point, so nothing committed was lost.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(&[0xAB; 40]).unwrap();
    drop(f);
    let torn = glider::store::verify(&path).unwrap();
    let at = torn.bad_offset.expect("should report where it stopped");
    assert!(
        at >= torn.committed_len,
        "a torn tail must not implicate committed data"
    );
    assert_eq!(torn.records, 20);

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(glider::store::lock_path(&path)).ok();
}
