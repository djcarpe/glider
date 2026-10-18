//! Extensions: install state is part of the database (logged, recovered,
//! transactional), and the `context` extension's output keeps its promises
//! — within budget, connected, faithful to the graph, deterministic.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use glider::ext::context::tokens;
use glider::graph::Graph;
use glider::query::{self, QueryResult};
use glider::store::Sync;
use glider::value::{parse_json, Json, Value};

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-ext-{}-{}.db", name, std::process::id()));
    cleanup(&p);
    p
}

fn cleanup(p: &std::path::Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(glider::store::lock_path(p));
    for suffix in ["-wal", "-data", "-tmp"] {
        let mut d = p.as_os_str().to_os_string();
        d.push(suffix);
        let _ = std::fs::remove_dir_all(PathBuf::from(d));
    }
}

fn q(g: &mut Graph, src: &str) -> QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn err(g: &mut Graph, src: &str) -> String {
    match query::execute(g, src) {
        Ok(_) => panic!("{src}: expected an error"),
        Err(e) => e.to_string(),
    }
}

fn text(r: &QueryResult) -> String {
    match &r.rows[0][0] {
        Value::Text(s) => s.clone(),
        v => panic!("not text: {v:?}"),
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const WORDS: &[&str] = &[
    "graph", "engine", "river", "lattice", "quantum", "harbor", "violet", "copper", "signal", "meadow", "falcon",
    "ember", "orbit", "cipher", "willow", "granite",
];

/// Communities of people and documents with text, cross-links, and one hub
/// everything points at.
fn knowledge_graph(seed: u64, people: usize) -> Graph {
    let mut g = Graph::memory();
    let mut r = Rng(seed | 1);
    g.autocommit = false;
    let hub = g.add_node(&["Country".into()], vec![("name".into(), Value::from("Everywhere"))]).unwrap();
    let mut ids = Vec::new();
    for i in 0..people {
        let w1 = WORDS[r.below(WORDS.len() as u64) as usize];
        let w2 = WORDS[r.below(WORDS.len() as u64) as usize];
        let id = g
            .add_node(
                &["Person".into()],
                vec![
                    ("name".into(), Value::from(format!("Person {i} {w1}"))),
                    ("bio".into(), Value::from(format!("Writes about {w1} and {w2}. {}", "Long text. ".repeat(r.below(30) as usize)))),
                    ("embedding".into(), Value::List((0..16).map(|k| Value::Float(k as f64 / 16.0)).collect())),
                    ("age".into(), Value::Int(20 + r.below(60) as i64)),
                ],
            )
            .unwrap();
        g.add_edge(id, hub, "CITIZEN_OF", vec![]).unwrap();
        ids.push(id);
    }
    for (i, &a) in ids.iter().enumerate() {
        let community = i / 20;
        for _ in 0..4 {
            let j = community * 20 + r.below(20) as usize;
            if j < ids.len() && ids[j] != a {
                g.add_edge(a, ids[j], "KNOWS", vec![("since".into(), Value::Int(2000 + r.below(24) as i64))]).unwrap();
            }
        }
        if r.below(4) == 0 {
            let b = ids[r.below(ids.len() as u64) as usize];
            g.add_edge(a, b, "MET", vec![]).unwrap();
        }
    }
    for d in 0..people / 2 {
        let w = WORDS[r.below(WORDS.len() as u64) as usize];
        let doc = g
            .add_node(&["Doc".into()], vec![("title".into(), Value::from(format!("On the {w} problem, part {d}")))])
            .unwrap();
        let author = ids[r.below(ids.len() as u64) as usize];
        g.add_edge(author, doc, "AUTHORED", vec![]).unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    g
}

fn installed(seed: u64, people: usize) -> Graph {
    let mut g = knowledge_graph(seed, people);
    q(&mut g, "CREATE EXTENSION context");
    g
}

// ------------------------------------------------------------- lifecycle

#[test]
fn procedures_need_the_extension_installed() {
    let mut g = knowledge_graph(1, 40);
    let e = err(&mut g, "CALL context.retrieve(seeds: [2])");
    assert!(e.contains("CREATE EXTENSION context"), "{e}");
    let e = err(&mut g, "MATCH (p:Person) RETURN context.card(p)");
    assert!(e.contains("not installed"), "{e}");
    q(&mut g, "CREATE EXTENSION context");
    q(&mut g, "CALL context.retrieve(seeds: [2])");
    q(&mut g, "MATCH (p:Person) RETURN context.card(p) LIMIT 1");
}

#[test]
fn mistakes_are_errors_not_nulls() {
    let mut g = installed(1, 40);
    assert!(err(&mut g, "CREATE EXTENSION nosuch").contains("no extension named 'nosuch'"));
    assert!(err(&mut g, "CALL nosuch.thing()").contains("no extension named"));
    assert!(err(&mut g, "CALL context.nope()").contains("no procedure 'nope'"));
    assert!(err(&mut g, "MATCH (p) RETURN context.nope(p)").contains("no function 'nope'"));
    assert!(err(&mut g, "CREATE EXTENSION context").contains("already installed"));
    assert!(q(&mut g, "CREATE EXTENSION IF NOT EXISTS context").message.unwrap().contains("already installed"));
    assert!(err(&mut g, "CALL context.retrieve(budget: 100)").contains("seeds"));
    assert!(err(&mut g, "CALL context.retrieve(seeds: [true])").contains("expected node ids"));
}

#[test]
fn show_and_drop() {
    let mut g = knowledge_graph(1, 10);
    let r = q(&mut g, "SHOW EXTENSIONS");
    let row = r.rows.iter().find(|r| r[0] == Value::from("context")).unwrap();
    assert_eq!(row[2], Value::Null);
    q(&mut g, "CREATE EXTENSION context");
    let r = q(&mut g, "SHOW EXTENSIONS");
    let row = r.rows.iter().find(|r| r[0] == Value::from("context")).unwrap();
    assert_eq!(row[2], Value::from("1.0"));
    q(&mut g, "DROP EXTENSION context");
    assert!(g.extensions().is_empty());
    assert!(err(&mut g, "DROP EXTENSION context").contains("not installed"));
    q(&mut g, "DROP EXTENSION IF EXISTS context");
    assert!(err(&mut g, "ALTER EXTENSION context UPDATE").contains("not installed"));
    q(&mut g, "CREATE EXTENSION context");
    assert!(q(&mut g, "ALTER EXTENSION context UPDATE").message.unwrap().contains("already at 1.0"));
}

#[test]
fn install_rolls_back_with_its_transaction() {
    let mut g = knowledge_graph(1, 10);
    q(&mut g, "BEGIN");
    q(&mut g, "CREATE EXTENSION context");
    assert_eq!(g.extension_version("context").as_deref(), Some("1.0"));
    q(&mut g, "ROLLBACK");
    assert_eq!(g.extension_version("context"), None);
    // CLEAR wipes data, not configuration, as with index definitions.
    q(&mut g, "CREATE EXTENSION context");
    q(&mut g, "CLEAR");
    assert_eq!(g.extension_version("context").as_deref(), Some("1.0"));
}

#[test]
fn install_survives_reopen_and_crash_recovery() {
    let path = temp("persist");
    {
        let mut g = Graph::open(&path, Sync::Always).unwrap();
        q(&mut g, "CREATE (:Person {name: \"Ada\"})");
        q(&mut g, "CREATE EXTENSION context");
    }
    {
        let mut g = Graph::open(&path, Sync::Always).unwrap();
        assert_eq!(g.extensions(), vec![("context".to_string(), "1.0".to_string())]);
        q(&mut g, "CALL context.retrieve(seeds: [1])");
        // Drop it, then crash before any checkpoint: recovery replays the
        // drop from the log.
        g.set_checkpoint_bytes(u64::MAX);
        q(&mut g, "DROP EXTENSION context");
        std::mem::forget(g);
    }
    {
        let g = Graph::open_forced(&path, Sync::Always).unwrap();
        assert!(g.extensions().is_empty());
    }
    cleanup(&path);
}

// ------------------------------------------------------------- retrieve

/// The JSON form, parsed: node ids, and edges as (id, from, to).
fn retrieved(g: &mut Graph, args: &str) -> (Vec<u64>, Vec<(u64, u64, u64)>, String) {
    let r = q(g, &format!("CALL context.retrieve({args}, format: \"json\")"));
    let t = text(&r);
    let j = parse_json(&t).unwrap();
    let nodes = j.get("nodes").unwrap().as_array().unwrap().iter().map(|n| n.get("id").unwrap().as_u64().unwrap()).collect();
    let edges = j
        .get("edges")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|e: &Json| {
            (e.get("id").unwrap().as_u64().unwrap(), e.get("from").unwrap().as_u64().unwrap(), e.get("to").unwrap().as_u64().unwrap())
        })
        .collect();
    (nodes, edges, t)
}

#[test]
fn retrieval_is_within_budget_connected_and_faithful() {
    let mut g = installed(7, 400);
    let mut rng = Rng(99);
    for _ in 0..40 {
        let seed = 2 + rng.below(400);
        for budget in [120usize, 400, 1500] {
            for format in ["text", "json"] {
                let r = q(&mut g, &format!("CALL context.retrieve(seeds: [{seed}], budget: {budget}, format: \"{format}\")"));
                let t = tokens(&text(&r));
                // The seed's own card is always shown; past it, the budget holds.
                let seed_card = tokens(&text(&q(&mut g, &format!("MATCH (n) WHERE id(n) = {seed} RETURN context.card(n)"))));
                assert!(t <= budget.max(seed_card * 3), "seed {seed} budget {budget} {format}: {t} tokens");
            }
            let (nodes, edges, _) = retrieved(&mut g, &format!("seeds: [{seed}], budget: {budget}"));
            let set: HashSet<u64> = nodes.iter().copied().collect();
            assert!(set.contains(&seed));
            // Every edge is real, and between nodes that are shown.
            for &(id, from, to) in &edges {
                let e = g.edge(id).expect("edge exists");
                assert_eq!((e.from, e.to), (from, to));
                assert!(set.contains(&from) && set.contains(&to));
            }
            // Connected: every node reaches the seed through shown edges.
            let mut adj: HashMap<u64, Vec<u64>> = HashMap::new();
            for &(_, a, b) in &edges {
                adj.entry(a).or_default().push(b);
                adj.entry(b).or_default().push(a);
            }
            let mut reach = HashSet::from([seed]);
            let mut stack = vec![seed];
            while let Some(v) = stack.pop() {
                for &w in adj.get(&v).into_iter().flatten() {
                    if reach.insert(w) {
                        stack.push(w);
                    }
                }
            }
            assert_eq!(reach, set, "seed {seed} budget {budget}: not connected");
        }
    }
}

#[test]
fn more_budget_never_shows_less() {
    let mut g = installed(3, 300);
    for seed in [5u64, 50, 150] {
        let mut prev = 0;
        for budget in [100, 200, 400, 800, 1600, 3200] {
            let (nodes, _, _) = retrieved(&mut g, &format!("seeds: [{seed}], budget: {budget}"));
            assert!(nodes.len() >= prev, "seed {seed}: {budget} gave {} < {prev}", nodes.len());
            prev = nodes.len();
        }
    }
}

#[test]
fn retrieval_is_deterministic() {
    let mut g = installed(5, 300);
    let a = text(&q(&mut g, "CALL context.retrieve(query: \"river engine\", budget: 600)"));
    let b = text(&q(&mut g, "CALL context.retrieve(query: \"river engine\", budget: 600)"));
    assert_eq!(a, b);
}

#[test]
fn text_is_cheaper_than_json_and_vectors_are_elided() {
    let mut g = installed(5, 200);
    let t = text(&q(&mut g, "CALL context.retrieve(seeds: [10], budget: 100000)"));
    let (_, _, j) = retrieved(&mut g, "seeds: [10], budget: 100000");
    assert!(tokens(&t) * 2 < tokens(&j), "text {} json {}", tokens(&t), tokens(&j));
    assert!(t.contains("embedding=<vector 16>"), "{t}");
    // Long text is cut, and says so.
    let card = text(&q(&mut g, "MATCH (n) WHERE id(n) = 10 RETURN context.card(n, 20)"));
    assert!(card.contains('…'), "{card}");
}

#[test]
fn seen_nodes_are_linked_but_not_described_again() {
    let mut g = installed(5, 200);
    let first = text(&q(&mut g, "CALL context.retrieve(seeds: [\"n10\"], budget: 300)"));
    let ids: Vec<String> = first
        .lines()
        .filter(|l| l.starts_with('n') && l.contains(':'))
        .map(|l| l.split(':').next().unwrap().to_string())
        .collect();
    assert!(ids.len() > 1);
    let seen = ids.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ");
    let second = text(&q(&mut g, &format!("CALL context.retrieve(seeds: [\"n10\"], seen: [{seen}], budget: 300)")));
    for id in &ids {
        assert!(!second.lines().any(|l| l.starts_with(&format!("{id}:"))), "{id} described again:\n{second}");
    }
    // The space goes to nodes the model has not seen yet.
    let new_cards = second.lines().filter(|l| l.starts_with('n') && l.contains(':')).count();
    assert!(new_cards > 0, "{second}");
}

#[test]
fn hubs_are_ranked_but_not_expanded() {
    let mut g = installed(5, 600);
    // The hub (node 1) has 600 citizens; a small fan-out limit stops the
    // walk going through it, so what is retrieved stays local.
    let (nodes, _, _) = retrieved(&mut g, "seeds: [10], budget: 4000, max_fanout: 100");
    let r = q(&mut g, "CALL context.retrieve(seeds: [10], budget: 4000, max_fanout: 100, format: \"rows\")");
    let hub = r.rows.iter().find(|row| row[0] == Value::Int(1));
    assert!(hub.is_some(), "the hub is relevant and should be listed");
    // Most retrieved people are in the seed's community (ids 2..=21) or one
    // KNOWS hop beyond, not a sample of the hub's 600 citizens.
    assert!(nodes.len() < 200, "{}", nodes.len());
    let t = text(&q(&mut g, "CALL context.retrieve(seeds: [10], budget: 300, max_fanout: 100)"));
    assert!(t.contains("# more:"), "omissions are reported:\n{t}");
}

#[test]
fn types_and_labels_restrict_the_walk() {
    let mut g = installed(5, 200);
    let (nodes, edges, _) = retrieved(&mut g, "seeds: [10], budget: 2000, types: [\"KNOWS\"]");
    for &(id, _, _) in &edges {
        assert_eq!(g.edge_type_name(id), Some("KNOWS"));
    }
    for &n in &nodes {
        assert_eq!(g.node_labels(n), vec!["Person".to_string()]);
    }
    let (nodes, _, _) = retrieved(&mut g, "seeds: [10], budget: 2000, labels: [\"Person\"]");
    for &n in &nodes {
        assert_eq!(g.node_labels(n), vec!["Person".to_string()]);
    }
}

// ------------------------------------------------------------- search

#[test]
fn search_finds_names_first() {
    let mut g = installed(5, 200);
    let name = match g.node_prop(42, "name") {
        Some(Value::Text(s)) => s,
        v => panic!("{v:?}"),
    };
    let r = q(&mut g, &format!("CALL context.search(query: \"{name}\", top: 3)"));
    assert_eq!(r.rows[0][0], Value::Int(42), "{:?}", r.rows);
    // Every hit actually contains a query word.
    let r = q(&mut g, "CALL context.search(query: \"copper\", top: 50)");
    assert!(!r.rows.is_empty());
    for row in &r.rows {
        let id = row[0].as_i64().unwrap() as u64;
        let text: String = g.node_props(id).iter().map(|(_, v)| v.to_string().to_lowercase()).collect();
        assert!(text.contains("copper"), "{id}: {text}");
    }
}

#[test]
fn search_uses_an_index_for_exact_names() {
    let mut g = installed(5, 200);
    q(&mut g, "INDEX ON :Person(name)");
    let name = g.node_prop(77, "name").unwrap().to_string();
    let r = q(&mut g, &format!("CALL context.search(query: \"{name}\", label: \"Person\", keys: [\"name\"], top: 1)"));
    assert_eq!(r.rows[0][0], Value::Int(77));
}

// ------------------------------------------------------------- paths

#[test]
fn paths_are_real_shortest_and_specific_first() {
    let mut g = installed(11, 300);
    let mut rng = Rng(5);
    for _ in 0..30 {
        let a = 2 + rng.below(300);
        let b = 2 + rng.below(300);
        let r = q(&mut g, &format!("CALL context.paths(from: {a}, to: {b}, k: 4, format: \"rows\")"));
        let shortest = glider::traverse::shortest_path(&g, a, b, glider::Dir::Both, None, 1 << 20).unwrap();
        match shortest {
            None => assert!(r.rows.is_empty()),
            Some(p) => {
                assert!(!r.rows.is_empty());
                for row in &r.rows {
                    let Value::List(nodes) = &row[1] else { panic!() };
                    let Value::List(edges) = &row[2] else { panic!() };
                    assert_eq!(edges.len(), p.len() - 1, "{a}->{b} not shortest");
                    assert_eq!(nodes.first(), Some(&Value::Int(a as i64)));
                    assert_eq!(nodes.last(), Some(&Value::Int(b as i64)));
                    for (i, e) in edges.iter().enumerate() {
                        let e = g.edge(e.as_i64().unwrap() as u64).unwrap();
                        let (x, y) = (nodes[i].as_i64().unwrap() as u64, nodes[i + 1].as_i64().unwrap() as u64);
                        assert!((e.from, e.to) == (x, y) || (e.from, e.to) == (y, x), "edge does not join its nodes");
                    }
                }
                // Distinct paths.
                let mut all: Vec<&Value> = r.rows.iter().map(|row| &row[2]).collect();
                let n = all.len();
                all.dedup();
                assert_eq!(all.len(), n);
            }
        }
    }
    // Through the hub is 2 hops between any two citizens, but a direct
    // KNOWS is 1, and the text form shows direction.
    let t = text(&q(&mut g, "CALL context.paths(from: 2, to: 3)"));
    assert!(t.starts_with("# paths n2 to n3"), "{t}");
}

// ------------------------------------------------------------- schema

#[test]
fn schema_card_describes_the_graph() {
    let mut g = installed(5, 100);
    q(&mut g, "MATCH (p:Person) SET p.status = \"active\"");
    let t = text(&q(&mut g, "CALL context.schema()"));
    assert!(t.contains("(:Person) 100 {name:text"), "{t}");
    assert!(t.contains("embedding:vector[16]"), "{t}");
    assert!(t.contains("status:text in {\"active\"}"), "{t}");
    assert!(t.contains("(:Person)-[:KNOWS {since:int}]->(:Person)"), "{t}");
    assert!(t.contains("(:Person)-[:CITIZEN_OF]->(:Country) 100"), "{t}");
    assert!(tokens(&t) < 300, "{}", tokens(&t));
}

// ------------------------------------------------------------- registration

struct Echo;

impl glider::ext::Extension for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn version(&self) -> &str {
        "0.1"
    }
    fn summary(&self) -> &str {
        "test extension"
    }
    fn procedures(&self) -> &[glider::ext::Spec] {
        &[glider::ext::Spec { name: "say", args: "(word)", doc: "says it" }]
    }
    fn functions(&self) -> &[glider::ext::Spec] {
        &[glider::ext::Spec { name: "twice", args: "(x)", doc: "x twice" }]
    }
    fn call(&self, g: &mut Graph, _: &str, a: &glider::ext::Args) -> glider::Result<QueryResult> {
        // Procedures write inside the statement's transaction.
        g.add_node(&["Said".into()], vec![("word".into(), Value::from(a.str("word").unwrap_or_default()))])?;
        Ok(QueryResult::message("said"))
    }
    fn eval(&self, _: &Graph, _: &str, args: &[glider::ext::Arg]) -> Value {
        match args.first() {
            Some(glider::ext::Arg::Value(Value::Int(i))) => Value::Int(i * 2),
            Some(glider::ext::Arg::Node(id)) => Value::Int(*id as i64 * 2),
            _ => Value::Null,
        }
    }
    fn install(&self, g: &mut Graph) -> glider::Result<()> {
        g.create_index("Said", "word")
    }
}

#[test]
fn embedders_can_register_their_own() {
    glider::ext::register(std::sync::Arc::new(Echo));
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION echo");
    assert!(g.has_index("Said", "word"), "install hook ran");
    q(&mut g, "CALL echo.say(word: \"hi\")");
    assert_eq!(q(&mut g, "MATCH (s:Said) RETURN s.word").rows, vec![vec![Value::from("hi")]]);
    assert_eq!(q(&mut g, "MATCH (s:Said) RETURN echo.twice(21), echo.twice(s)").rows, vec![vec![Value::Int(42), Value::Int(2)]]);
    let help = q(&mut g, "CALL echo.help()");
    assert_eq!(help.rows.len(), 2);
}

/// Communities of 50, each member also linked to 2 random members anywhere
/// and to one of 10 hubs, all with one edge type: only structure says what
/// is relevant. Retrieval must find the seed's community better than a
/// breadth-first expansion showing as many nodes.
#[test]
fn retrieval_beats_breadth_first_on_planted_communities() {
    let (n, c) = (5000u64, 50u64);
    let mut g = Graph::memory();
    g.autocommit = false;
    let mut r = Rng(17);
    for h in 0..10 {
        g.add_node(&["Topic".into()], vec![("name".into(), Value::from(format!("Topic {h}")))]).unwrap();
    }
    for i in 0..n {
        g.add_node(&["Entity".into()], vec![("name".into(), Value::from(format!("entity {i}")))]).unwrap();
    }
    let id = |i: u64| 11 + i;
    for i in 0..n {
        for _ in 0..5 {
            let j = i / c * c + r.below(c);
            if j != i {
                g.add_edge(id(i), id(j), "REL", vec![]).unwrap();
            }
        }
        for _ in 0..2 {
            g.add_edge(id(i), id(r.below(n)), "REL", vec![]).unwrap();
        }
        g.add_edge(id(i), 1 + r.below(10), "REL", vec![]).unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    q(&mut g, "CREATE EXTENSION context");
    let (mut ours, mut bfs_p) = (0.0, 0.0);
    let seeds = 40;
    for k in 0..seeds {
        let i = (k * 977) % n;
        let seed = id(i);
        let in_comm = |x: u64| x > 10 && (x - 11) / c == i / c;
        let rows = q(&mut g, &format!("CALL context.retrieve(seeds: [{seed}], budget: 1500, format: \"rows\")"));
        let nodes: Vec<u64> = rows.rows.iter().map(|r| r[0].as_i64().unwrap() as u64).collect();
        let others: Vec<u64> = nodes.iter().copied().filter(|&x| x != seed).collect();
        // Hits over what could have been hits: 49 others are in the community.
        let best = others.len().min(c as usize - 1) as f64;
        ours += others.iter().filter(|&&x| in_comm(x)).count() as f64 / best;
        let mut walk = Vec::new();
        glider::traverse::bfs(&g, seed, glider::Dir::Both, None, None, usize::MAX, &mut |v| {
            if v.node != seed {
                walk.push(v.node);
            }
            walk.len() < others.len()
        });
        bfs_p += walk.iter().filter(|&&x| in_comm(x)).count() as f64 / best;
    }
    let (ours, bfs_p) = (ours / seeds as f64, bfs_p / seeds as f64);
    eprintln!("retrieve {ours:.3}, bfs {bfs_p:.3}");
    assert!(ours > 0.8, "precision {ours:.3}");
    assert!(ours > bfs_p + 0.15, "retrieve {ours:.3} vs bfs {bfs_p:.3}");
}

// ------------------------------------------------------------- text index

/// Everything in the extension's keyspace, for comparing index states.
fn keyspace(g: &Graph) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    g.ext_scan("context", b"", &mut |k, v| {
        out.push((k.to_vec(), v.to_vec()));
        true
    });
    out
}

#[test]
fn the_index_kept_by_writes_equals_one_rebuilt_from_scratch() {
    let mut g = installed(21, 150);
    q(&mut g, "CALL context.index(labels: [\"Person\", \"Doc\"])");
    let mut r = Rng(3);
    for step in 0..400 {
        let id = 2 + r.below(260);
        let w = WORDS[r.below(WORDS.len() as u64) as usize];
        let src = match r.below(8) {
            0 => format!("MATCH (n) WHERE id(n) = {id} SET n.bio = \"now about {w} and {w}\""),
            1 => format!("MATCH (n) WHERE id(n) = {id} SET n.name = \"{w} {step}\""),
            2 => format!("MATCH (n) WHERE id(n) = {id} REMOVE n.bio"),
            3 => format!("MATCH (n) WHERE id(n) = {id} DETACH DELETE n"),
            4 => format!("CREATE (:Doc {{title: \"{w} notes {step}\", tags: [\"{w}\", \"misc\"]}})"),
            5 => format!("MATCH (n) WHERE id(n) = {id} SET n:Doc"),
            6 => format!("MATCH (n) WHERE id(n) = {id} REMOVE n:Person"),
            _ => format!("CREATE (:Other {{name: \"{w} outside\"}})"),
        };
        q(&mut g, &src);
        if step % 50 == 0 {
            // A rolled-back write must leave no trace in the index either.
            q(&mut g, "BEGIN");
            q(&mut g, &format!("CREATE (:Doc {{title: \"rolled back {w}\"}})"));
            q(&mut g, "ROLLBACK");
        }
    }
    let live = keyspace(&g);
    q(&mut g, "CALL context.index(labels: [\"Person\", \"Doc\"])");
    let rebuilt = keyspace(&g);
    assert_eq!(live.len(), rebuilt.len());
    assert!(live == rebuilt, "incremental index drifted from a rebuild");
}

#[test]
fn index_and_scan_agree_on_what_matches() {
    let mut g = installed(8, 300);
    let scan = q(&mut g, "CALL context.search(query: \"copper harbor\", top: 1000, scan: 1000000)");
    q(&mut g, "CALL context.index()");
    let idx = q(&mut g, "CALL context.search(query: \"copper harbor\", top: 1000)");
    assert!(idx.message.as_deref().unwrap().contains("text index"), "{:?}", idx.message);
    let ids = |r: &QueryResult| r.rows.iter().map(|r| r[0].as_i64().unwrap()).collect::<HashSet<_>>();
    assert_eq!(ids(&scan), ids(&idx));
    // A name looked up by its words comes first either way.
    let name = g.node_prop(42, "name").unwrap();
    let Value::Text(name) = name else { panic!() };
    let r = q(&mut g, &format!("CALL context.search(query: \"{name}\", top: 3)"));
    assert_eq!(r.rows[0][0], Value::Int(42));
    // And retrieve finds its seeds through it.
    let t = text(&q(&mut g, &format!("CALL context.retrieve(query: \"{name}\", top: 1, budget: 200)")));
    assert!(t.contains("seeds n42"), "{t}");
}

#[test]
fn the_index_survives_crashes_clears_and_drops() {
    let path = temp("textindex");
    {
        let mut g = Graph::open(&path, Sync::Always).unwrap();
        q(&mut g, "CREATE EXTENSION context");
        q(&mut g, "CREATE (:Doc {title: \"basalt fjord\"})");
        q(&mut g, "CALL context.index()");
        g.set_checkpoint_bytes(u64::MAX);
        // Written through the API with autocommit on: the node and its
        // postings commit together.
        g.add_node(&["Doc".into()], vec![("title".into(), Value::from("garnet quartz"))]).unwrap();
        std::mem::forget(g);
    }
    {
        let mut g = Graph::open_forced(&path, Sync::Always).unwrap();
        let r = q(&mut g, "CALL context.search(query: \"quartz\")");
        assert_eq!(r.rows.len(), 1, "recovered postings");
        let before = keyspace(&g);
        q(&mut g, "CALL context.index()");
        assert_eq!(before, keyspace(&g));
        // CLEAR removes postings, keeps the configuration, and indexing goes on.
        q(&mut g, "CLEAR");
        assert!(q(&mut g, "CALL context.search(query: \"quartz\")").rows.is_empty());
        q(&mut g, "CREATE (:Doc {title: \"quartz again\"})");
        assert_eq!(q(&mut g, "CALL context.search(query: \"quartz\")").rows.len(), 1);
        // Dropping the extension removes everything it stored.
        q(&mut g, "DROP EXTENSION context");
        assert!(keyspace(&g).is_empty());
        q(&mut g, "CREATE (:Doc {title: \"quartz once more\"})");
        assert!(keyspace(&g).is_empty(), "no hooks once dropped");
    }
    cleanup(&path);
}
