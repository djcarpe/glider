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
    // Simulate a crash mid-write by appending garbage to the write-ahead
    // log, where an interrupted commit would leave it.
    {
        use std::io::Write;
        let mut wal = path.as_os_str().to_os_string();
        wal.push("-wal");
        let seg = std::fs::read_dir(PathBuf::from(wal))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .max()
            .expect("a log segment");
        let mut f = std::fs::OpenOptions::new().append(true).open(&seg).unwrap();
        f.write_all(&[1u8, 200, 0, 0, 0, 7, 7, 7]).unwrap();
    }
    {
        let g = Graph::open(&path, Sync::Always).unwrap();
        assert_eq!(g.node_count(), 1, "committed data must survive a torn tail");
    }
    std::fs::remove_file(&path).ok();
}

#[test]
fn opens_from_file_bytes_without_touching_disk() {
    let path = temp("bytes");
    {
        let mut g = Graph::open(&path, Sync::Always).unwrap();
        q(
            &mut g,
            r#"CREATE (a:Person {name:"Ada"})-[:KNOWS {since:2019}]->(b:Person {name:"Bob"})"#,
        );
        q(
            &mut g,
            r#"MATCH (n:Person {name:"Bob"}) SET n.city = "Paris""#,
        );
    }
    let mut bytes = std::fs::read(&path).unwrap();
    // A torn tail is ignored here exactly as it is on open.
    bytes.extend_from_slice(&[0u8, 200, 0, 0, 0, 7, 7, 7]);

    let mut g = Graph::from_bytes(&bytes).unwrap();
    assert!(!g.is_persistent());
    assert_eq!(g.node_count(), 2);
    let r = q(
        &mut g,
        r#"MATCH (a)-[r:KNOWS]->(b) RETURN a.name, r.since, b.city"#,
    );
    assert_eq!(r.rows.len(), 1);
    // New ids continue past the file's, rather than colliding with them.
    q(&mut g, r#"CREATE (:Person {name:"Cai"})"#);
    assert_eq!(g.node_count(), 3);

    assert!(Graph::from_bytes(b"not a database at all").is_err());
    assert!(Graph::from_bytes(&[]).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn log_writer_streams_a_file_that_opens() {
    use glider::store::{LogWriter, Op};
    let path = temp("logwriter");
    let mut w = LogWriter::create(&path).unwrap();
    for id in 1..=3u64 {
        w.push(&Op::NodeAdd {
            id,
            labels: vec!["Person".into()],
            props: vec![("n".into(), Value::Int(id as i64))],
        })
        .unwrap();
    }
    w.push(&Op::EdgeAdd {
        id: 1,
        from: 1,
        to: 2,
        etype: "KNOWS".into(),
        props: vec![],
    })
    .unwrap();
    w.commit().unwrap();
    // An unfinished transaction is flushed by `finish` as a committed one.
    w.push(&Op::NodeDel { id: 3 }).unwrap();
    let bytes = w.finish().unwrap();
    assert_eq!(bytes, std::fs::metadata(&path).unwrap().len());
    // It refuses to clobber an existing file.
    assert!(LogWriter::create(&path).is_err());

    // A streamed log is the legacy format: the paged engine asks for a
    // migration, the legacy engine reads it.
    assert!(Graph::open(&path, Sync::Always).is_err());
    let mut g = glider::legacy::graph::Graph::open(&path, Sync::Always).unwrap();
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 1);
    // The engine carries on numbering after the streamed ids.
    assert_eq!(g.add_node(&["Person".into()], vec![]).unwrap(), 4);
    assert_eq!(g.node_count(), 3);
    drop(g);
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
fn compaction_preserves_state_and_reuses_space() {
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
    g.compact().unwrap();
    let before = g.file_len();
    // Churn again: freed pages are reused, so the file does not grow.
    for i in 191..=200u64 {
        for r in 0..5 {
            g.set_node_prop(i, "i", Value::Int(r)).unwrap();
        }
    }
    g.compact().unwrap();
    let after = g.file_len();
    assert!(after <= before, "freed pages must be reused: {before} -> {after}");
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
    let (mut tri, total) = algo::triangles(&both);
    assert_eq!(total, 1);
    let clustering = algo::clustering(&both, &mut tri).to_vec();
    let tri = tri.to_vec();
    assert_eq!(tri[0], 1);
    assert_eq!(tri[3], 0);
    assert!(
        (clustering[1] - 1.0).abs() < 1e-9,
        "node 2 closes its only triangle"
    );
    assert!((clustering[0] - (1.0 / 3.0)).abs() < 1e-9);

    let cores = algo::core_numbers(&both).to_vec();
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
    let mut pr = algo::pagerank(&out, 0.85, 100, 1e-12);
    let sum: f64 = pr.scores.to_vec().iter().sum();
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
    assert_eq!(count, n, "a chain has n singleton SCCs");
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
    let bc = algo::betweenness(&both, None, false).to_vec();
    assert!(bc[0] > 0.0, "the hub is on every path");
    for b in &bc[1..5] {
        assert_eq!(*b, 0.0, "leaves are on no shortest path");
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
fn negative_literals_stay_integers() {
    let mut g = Graph::memory();
    q(&mut g, r#"CREATE (n:T {delta:-5, temp:-2.5})"#);
    assert_eq!(g.node_prop(1, "delta"), Some(Value::Int(-5)));
    assert_eq!(g.node_prop(1, "temp"), Some(Value::Float(-2.5)));
    q(&mut g, r#"MATCH (n:T) SET n.delta = -7"#);
    assert_eq!(g.node_prop(1, "delta"), Some(Value::Int(-7)));
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

    let mut g = glider::legacy::graph::Graph::open(&path, glider::Sync::Always).unwrap();
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

    let g = glider::legacy::graph::Graph::open(&path, glider::Sync::Always).unwrap();
    assert_eq!(g.node_count(), 1, "committed data survives");
    drop(g);

    let after = glider::store::read_header(&path).unwrap().generation_hex();
    assert_ne!(before, after, "a discarded tail must end the lineage");

    // A clean reopen must NOT churn the generation.
    let g = glider::legacy::graph::Graph::open(&path, glider::Sync::Always).unwrap();
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

    // Both people on both sides. Not the reflexive pairs: (p)->(t)<-(p)
    // would use p's one IN relationship twice, which a match never does.
    let found = rows(
        &mut g,
        "MATCH (p:Person)-[:IN]->(t:Team)<-[:IN]-(q:Person) RETURN p.name, q.name ORDER BY p.name",
    );
    assert_eq!(found.len(), 2);
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
        let mut g = glider::legacy::graph::Graph::open(&path, Sync::Always).unwrap();
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

#[test]
fn rollback_discards_the_open_transaction() {
    let path = temp("rollback-stmt");
    {
        let mut g = Graph::open(&path, Sync::Normal).unwrap();
        q(&mut g, "CREATE (:Keep {n: 1})");
        q(&mut g, "INDEX ON :Keep(n)");
        q(&mut g, "BEGIN");
        q(&mut g, "CREATE (:Gone {n: 2})");
        q(&mut g, "MATCH (k:Keep) SET k.n = 99");
        q(&mut g, "INDEX ON :Gone(n)");
        let r = q(&mut g, "ROLLBACK");
        assert!(r.message.unwrap().starts_with("rolled back"));
        assert!(g.autocommit, "ROLLBACK ends the transaction");
        let r = q(&mut g, "MATCH (n) RETURN labels(n), n.n");
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][1], Value::Int(1));
        assert!(!g.has_index("Gone", "n"), "schema changes roll back too");
        assert_eq!(q(&mut g, "MATCH (k:Keep {n: 1}) RETURN count(k)").rows[0][0], Value::Int(1));
        q(&mut g, "CREATE (:After)");
    }
    let mut g = Graph::open(&path, Sync::Normal).unwrap();
    let r = q(&mut g, "MATCH (n) RETURN labels(n)");
    assert_eq!(r.rows.len(), 2, "nothing rolled back comes back on reopen");
}

#[test]
fn parameters_are_values_never_syntax() {
    let mut g = Graph::memory();
    let p = |pairs: &[(&str, Value)]| -> Vec<(String, Value)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    };
    let evil = Value::from("x\"}) DETACH DELETE n //");
    query::execute_with(&mut g, "CREATE (:P {name: $n, tags: $t, age: $a})",
        &p(&[("n", evil.clone()), ("t", Value::List(vec![Value::Int(1), Value::Null, Value::from("a")])), ("a", Value::Int(-4))])).unwrap();
    query::execute_with(&mut g, "CREATE (:P {name: $n, age: $a})", &p(&[("n", Value::from("Bob")), ("a", Value::Float(2.5))])).unwrap();
    q(&mut g, "INDEX ON :P(name)");
    let r = query::execute_with(&mut g, "MATCH (x:P {name: $n}) RETURN x.name, x.tags, x.age", &p(&[("n", evil.clone())])).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], evil);
    assert_eq!(r.rows[0][2], Value::Int(-4));
    let r = query::execute_with(&mut g, "MATCH (x:P) WHERE x.age > $min AND x.name IN $names RETURN x.name",
        &p(&[("min", Value::Int(0)), ("names", Value::List(vec![Value::from("Bob")]))])).unwrap();
    assert_eq!(r.rows, vec![vec![Value::from("Bob")]]);
    let e = query::execute_with(&mut g, "MATCH (x:P {name: $nope}) RETURN x", &[]).err().unwrap();
    assert!(e.to_string().contains("missing parameter $nope"));
    assert_eq!(q(&mut g, "MATCH (x:P) RETURN count(x)").rows[0][0], Value::Int(2));
}

/// The local traversals (default) and the whole-graph ones (`tier:`) must
/// give the same rows: same visit order, depths and parents. Shortest paths
/// may pick a different path of the same length.
#[test]
fn local_traversals_match_whole_graph_algorithms() {
    let mut g = Graph::memory();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    q(&mut g, "BEGIN");
    for i in 0..300 {
        q(&mut g, &format!("CREATE (:N {{i: {i}}})"));
    }
    for _ in 0..900 {
        let (a, b) = (rnd(300), rnd(300));
        let t = ["A", "B", "C"][rnd(3) as usize];
        q(&mut g, &format!("MATCH (a:N {{i: {a}}}), (b:N {{i: {b}}}) CREATE (a)-[:{t}]->(b)"));
    }
    q(&mut g, "COMMIT");
    let id_of = |g: &mut Graph, i: u64| match q(g, &format!("MATCH (n:N {{i: {i}}}) RETURN id(n)")).rows[0][0] {
        Value::Int(v) => v,
        _ => unreachable!(),
    };
    for (k, start) in [0u64, 7, 150, 299].into_iter().enumerate() {
        let from = id_of(&mut g, start);
        for dir in ["out", "in", "both"] {
            for ty in ["", ", type: \"A\""] {
                for algo in ["bfs", "dfs"] {
                    for depth in ["", ", depth: 2"] {
                        let base = format!("CALL {algo}(from: {from}, dir: \"{dir}\"{ty}{depth}");
                        let local = q(&mut g, &format!("{base})"));
                        let whole = q(&mut g, &format!("{base}, tier: \"mem\")"));
                        assert_eq!(local.rows, whole.rows, "{base}");
                    }
                }
                let base = format!("CALL subgraph(from: {from}, dir: \"{dir}\"{ty}, depth: 3");
                assert_eq!(q(&mut g, &format!("{base})")).rows, q(&mut g, &format!("{base}, tier: \"mem\")")).rows, "{base}");

                let to = id_of(&mut g, (start * 37 + k as u64 * 11 + 5) % 300);
                let base = format!("CALL shortestpath(from: {from}, to: {to}, dir: \"{dir}\"{ty}");
                let local = q(&mut g, &format!("{base})"));
                let whole = q(&mut g, &format!("{base}, tier: \"mem\")"));
                assert_eq!(local.message, whole.message, "{base}");
                assert_eq!(local.rows.len(), whole.rows.len(), "{base}");
                // A real path: every step follows an edge of the right type
                // in the right direction.
                let ids: Vec<u64> = local.rows.iter().map(|r| match r[1] { Value::Int(v) => v as u64, _ => 0 }).collect();
                let d = match dir { "out" => Dir::Out, "in" => Dir::In, _ => Dir::Both };
                let t = if ty.is_empty() { None } else { g.strings.lookup("A") };
                for w in ids.windows(2) {
                    assert!(g.neighbors(w[0], d, t).iter().any(|a| a.other == w[1]), "{base}: {w:?} is not an edge");
                }
            }
        }
    }
}

/// Edge-type anchors and counter-answered counts are plans, not semantics:
/// the rows must match the node-anchored, enumerated answers exactly.
#[test]
fn edge_anchors_and_counter_counts_match_the_general_plans() {
    use std::sync::atomic::Ordering;
    let mut g = Graph::memory();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    q(&mut g, "BEGIN");
    q(&mut g, "INDEX ON :N(k)");
    for i in 0..400 {
        let label = if i % 3 == 0 { ":N:M" } else { ":N" };
        q(&mut g, &format!("CREATE (n{label} {{i: {i}, k: {}}})", i % 7));
    }
    for _ in 0..1200 {
        let (a, b) = (rnd(400), if rnd(10) == 0 { u64::MAX } else { rnd(400) });
        let b = if b == u64::MAX { a } else { b }; // some self-loops
        let t = ["A", "A", "A", "B", "R"][rnd(5) as usize];
        q(&mut g, &format!("MATCH (a:N {{i: {a}}}), (b:N {{i: {b}}}) CREATE (a)-[:{t} {{w: {}}}]->(b)", rnd(4)));
    }
    q(&mut g, "COMMIT");

    let reads = [
        "MATCH ()-[r:R]->() RETURN id(r) ORDER BY id(r)",
        "MATCH (a)<-[r:R]-(b) RETURN id(a), id(r), id(b) ORDER BY id(r), id(a)",
        "MATCH (a)-[r:R]-(b) RETURN id(a), id(r), id(b) ORDER BY id(r), id(a)",
        "MATCH (e)-[:R]->(e) RETURN id(e) ORDER BY id(e)",
        "MATCH (a:M)-[r:R {w: 2}]->(b) RETURN id(a), id(b) ORDER BY id(a), id(b)",
        "MATCH (a)-[:A]->(b)-[:R]->(c)<-[:B]-(d) RETURN id(a), id(b), id(c), id(d) ORDER BY id(a), id(b), id(c), id(d)",
        "MATCH (x)-[:R]->(y) WHERE y.i > 200 RETURN count(*)",
        "MATCH ()-[r:R]->(b:M) RETURN b.k, count(r) ORDER BY b.k",
    ];
    let plans = |g: &mut Graph| reads.iter().map(|s| q(g, s).rows).collect::<Vec<_>>();
    let explain = q(&mut g, &format!("EXPLAIN {}", reads[0]));
    assert!(format!("{:?}", explain.rows).contains("edges :R"), "the rare type should anchor: {:?}", explain.rows);
    glider::query::EDGE_ANCHORS.store(false, Ordering::Relaxed);
    let by_nodes = plans(&mut g);
    glider::query::EDGE_ANCHORS.store(true, Ordering::Relaxed);
    let by_edges = plans(&mut g);
    for ((a, b), s) in by_nodes.iter().zip(&by_edges).zip(reads) {
        assert_eq!(a, b, "{s}");
    }

    // Counts from counters vs the same counts enumerated (WHERE true forces
    // the general path).
    for s in [
        "MATCH (n) RETURN count(n)",
        "MATCH (n:M) RETURN count(*)",
        "MATCH (n:N {k: 3}) RETURN count(n)",
        "MATCH (n:N {k: 99}) RETURN count(n)",
        "MATCH ()-[r:A]->() RETURN count(r)",
        "MATCH (a)<-[r:B]-(b) RETURN count(a)",
        "MATCH ()-[r]->() RETURN count(*)",
        "MATCH ()-[r:NOPE]->() RETURN count(r)",
        "MATCH (n:N {k: 3})-[:A]->(x) RETURN count(x)",
        "MATCH (n:N {k: 3})<-[r:A]-() RETURN count(r)",
        "MATCH (n:N {k: 3})-[:A]-(x) RETURN count(*)",
        "MATCH (n:N {k: 99})-[:A]->(x) RETURN count(x)",
    ] {
        let fast = q(&mut g, s);
        let slow = q(&mut g, &s.replace(" RETURN", " WHERE true RETURN"));
        assert_eq!((fast.columns.clone(), fast.rows.clone()), (slow.columns, slow.rows), "{s}");
    }
}

/// Cypher's relationship uniqueness: one match never uses the same
/// relationship twice, so A->B->A->B needs two distinct A->B edges.
#[test]
fn a_match_never_reuses_a_relationship() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE (a:P {n: 1})-[:K]->(b:P {n: 2})");
    q(&mut g, "MATCH (a:P {n: 1}), (b:P {n: 2}) CREATE (b)-[:K]->(a)");
    let n = |g: &mut Graph, s: &str| match q(g, s).rows.first().map(|r| r[0].clone()) {
        Some(Value::Int(v)) => v,
        _ => 0,
    };
    // a->b, b->a, then a->b again would reuse the first edge.
    assert_eq!(n(&mut g, "MATCH (x:P {n: 1})-[:K]->()-[:K]->()-[:K]->(y) RETURN count(y)"), 0);
    assert_eq!(n(&mut g, "MATCH (x:P {n: 1})-[:K]->()-[:K]->(y) RETURN count(y)"), 1);
    // With a second a->b edge, both orders of the two parallel edges match.
    q(&mut g, "MATCH (a:P {n: 1}), (b:P {n: 2}) CREATE (a)-[:K]->(b)");
    assert_eq!(n(&mut g, "MATCH (x:P {n: 1})-[:K]->()-[:K]->()-[:K]->(y) RETURN count(y)"), 2);
    // An edge anchor counts as used too.
    assert_eq!(n(&mut g, "MATCH (x)-[:K]->()-[:K]->()-[:K]->(y) WHERE x.n = 1 RETURN count(y)"), 2);
}

#[test]
fn subscripts_group_and_limit_and_trailing_tokens_are_errors() {
    let path = temp("subscripts");
    let mut g = Graph::open(&path, Sync::Always).unwrap();
    for i in 0..30 {
        let label = if i % 3 == 0 { "Order" } else if i % 3 == 1 { "Customer" } else { "Venue" };
        q(&mut g, &format!("CREATE (n:{} {{i: {}, tags: [\"a\", \"b\", \"c\"]}})", label, i));
    }

    // a subscript no longer ends the RETURN: grouping, ORDER BY and LIMIT all apply
    let r = q(&mut g, "MATCH (n) RETURN labels(n)[0] AS label, count(n) AS nodes ORDER BY nodes DESC LIMIT 2");
    assert_eq!(r.columns, vec!["label".to_string(), "nodes".to_string()]);
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][1], Value::Int(10));
    assert!(matches!(&r.rows[0][0], Value::Text(_)));

    let r = q(&mut g, "MATCH (n:Order) RETURN n.tags[0], n.tags[-1], n.tags[1..], n.tags[..2], [10, 20, 30][1] LIMIT 1");
    assert_eq!(r.rows[0][0], Value::Text("a".into()));
    assert_eq!(r.rows[0][1], Value::Text("c".into()));
    assert_eq!(r.rows[0][2], Value::List(vec![Value::Text("b".into()), Value::Text("c".into())]));
    assert_eq!(r.rows[0][3], Value::List(vec![Value::Text("a".into()), Value::Text("b".into())]));
    assert_eq!(r.rows[0][4], Value::Int(20));
    let r = q(&mut g, "MATCH (n:Order) RETURN n.tags[9] LIMIT 1");
    assert_eq!(r.rows[0][0], Value::Null);

    // leftovers after the statement are an error, not silently dropped
    let err = match query::execute(&mut g, "MATCH (n) RETURN labels(n)[0] AS label LIMIT 10, count(n) AS nodes") {
        Ok(_) => panic!("a statement with leftovers ran"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("unexpected ','"), "{}", err);
    assert!(query::execute(&mut g, "MATCH (n) RETURN n LIMIT 5 garbage").is_err());
    let r = q(&mut g, "MATCH (n) RETURN n LIMIT 10");
    assert_eq!(r.rows.len(), 10);
    let _ = std::fs::remove_file(&path);
}
