//! `structure`: colours are equal exactly when refinement cannot tell the
//! surroundings apart, depend on content and not on ids, are kept with what
//! they were computed from and say when the database has moved, come in
//! slices, spill past their memory, and are the same on both tiers; classes
//! group exactly the interchangeable nodes and merge nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use glider::graph::{Graph, OpenOptions};
use glider::query::{self, QueryResult};
use glider::value::Value;

// ------------------------------------------------------------------ helpers

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "glider-structure-{}-{}.db",
        name,
        std::process::id()
    ));
    cleanup(&p);
    p
}

fn cleanup(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(glider::store::lock_path(p));
    for suffix in ["-wal", "-data", "-tmp", "-hot"] {
        let mut d = p.as_os_str().to_os_string();
        d.push(suffix);
        let d = PathBuf::from(d);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_file(&d);
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

fn install(g: &mut Graph) {
    q(g, "CREATE EXTENSION IF NOT EXISTS structure");
}

fn text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        v => v.to_string(),
    }
}

fn int(v: &Value) -> u64 {
    v.as_i64().unwrap() as u64
}

/// `id -> colour` from a colours or read result.
fn colours(r: &QueryResult) -> BTreeMap<u64, String> {
    r.rows
        .iter()
        .map(|row| (int(&row[0]), text(&row[1])))
        .collect()
}

/// The partition a colouring makes: groups of ids, sorted.
fn partition(c: &BTreeMap<u64, String>) -> Vec<Vec<u64>> {
    let mut by: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for (id, col) in c {
        by.entry(col).or_default().push(*id);
    }
    let mut p: Vec<Vec<u64>> = by.into_values().collect();
    p.sort();
    p
}

/// Every row as text, for byte-for-byte comparison.
fn dump(r: &QueryResult) -> String {
    let mut s = format!("{:?}\n", r.columns);
    for row in &r.rows {
        let cells: Vec<String> = row.iter().map(|v| v.to_json()).collect();
        s.push_str(&cells.join("\t"));
        s.push('\n');
    }
    s
}

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A graph as plain data, to build in any order and to refine by hand.
#[derive(Clone)]
struct Data {
    /// (key, labels, props)
    nodes: Vec<(String, Vec<String>, Vec<(String, Value)>)>,
    /// (from key index, to key index, type)
    edges: Vec<(usize, usize, String)>,
}

fn random_data(seed: u64, n: usize, m: usize) -> Data {
    let mut r = Rng(seed);
    let labels = ["A", "B", "C"];
    let types = ["X", "Y", "Z"];
    let mut nodes = Vec::new();
    for i in 0..n {
        let mut ls = vec![labels[r.below(3) as usize].to_string()];
        if r.below(5) == 0 {
            ls.push(labels[r.below(3) as usize].to_string());
        }
        let mut props = vec![("key".to_string(), Value::Text(format!("k{i}")))];
        if r.below(2) == 0 {
            props.push(("p".to_string(), Value::Int(r.below(2) as i64)));
        }
        nodes.push((format!("k{i}"), ls, props));
    }
    let mut edges = Vec::new();
    for _ in 0..m {
        let a = r.below(n as u64) as usize;
        // Mostly short hops, sometimes a self-loop or a repeat.
        let b = if r.below(8) == 0 {
            a
        } else {
            r.below(n as u64) as usize
        };
        let t = types[r.below(3) as usize].to_string();
        edges.push((a, b, t.clone()));
        if r.below(10) == 0 {
            edges.push((a, b, t));
        }
    }
    Data { nodes, edges }
}

/// Build `d` into `g`, nodes in `order`, after `gap` nodes made and deleted
/// so ids differ from the order; returns key index -> node id.
fn build(g: &mut Graph, d: &Data, order: &[usize], gap: usize) -> Vec<u64> {
    for _ in 0..gap {
        let id = g.add_node(&["Gap".to_string()], vec![]).unwrap();
        g.delete_node(id).unwrap();
    }
    let mut ids = vec![0u64; d.nodes.len()];
    for &i in order {
        let (_, labels, props) = &d.nodes[i];
        ids[i] = g.add_node(labels, props.clone()).unwrap();
    }
    for (a, b, t) in &d.edges {
        g.add_edge(ids[*a], ids[*b], t, vec![]).unwrap();
    }
    ids
}

/// Weisfeiler-Lehman refinement done plainly: signatures compared in full,
/// renumbered each round. Returns the partition of all nodes by key index.
fn reference(d: &Data, depth: u32, dir: &str, types: &[&str], props: &[&str]) -> Vec<Vec<usize>> {
    let n = d.nodes.len();
    let mut colour: Vec<usize> = {
        let mut sigs: BTreeMap<String, usize> = BTreeMap::new();
        let mut out = Vec::new();
        for (_, labels, ps) in &d.nodes {
            let mut ls = labels.clone();
            ls.sort();
            ls.dedup();
            let mut sig = format!("{ls:?}");
            for p in props {
                let v = ps.iter().find(|(k, _)| k == p).map(|(_, v)| v.to_json());
                sig.push_str(&format!("|{p}={v:?}"));
            }
            let next = sigs.len();
            out.push(*sigs.entry(sig).or_insert(next));
        }
        out
    };
    let follows = |t: &str| types.is_empty() || types.contains(&t);
    for _ in 0..depth {
        let mut around: Vec<Vec<(char, String, usize)>> = vec![Vec::new(); n];
        for (a, b, t) in &d.edges {
            if !follows(t) {
                continue;
            }
            if dir == "out" || dir == "both" {
                around[*a].push(('>', t.clone(), colour[*b]));
            }
            if dir == "in" || dir == "both" {
                around[*b].push(('<', t.clone(), colour[*a]));
            }
        }
        let mut sigs: BTreeMap<(usize, Vec<(char, String, usize)>), usize> = BTreeMap::new();
        let mut next = Vec::with_capacity(n);
        for v in 0..n {
            around[v].sort();
            let key = (colour[v], std::mem::take(&mut around[v]));
            let k = sigs.len();
            next.push(*sigs.entry(key).or_insert(k));
        }
        colour = next;
    }
    let mut by: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (v, c) in colour.iter().enumerate() {
        by.entry(*c).or_default().push(v);
    }
    let mut p: Vec<Vec<usize>> = by.into_values().collect();
    p.sort();
    p
}

fn in_ids(p: Vec<Vec<usize>>, ids: &[u64]) -> Vec<Vec<u64>> {
    let mut out: Vec<Vec<u64>> = p
        .into_iter()
        .map(|g| {
            let mut v: Vec<u64> = g.into_iter().map(|i| ids[i]).collect();
            v.sort();
            v
        })
        .collect();
    out.sort();
    out
}

fn list(xs: &[&str]) -> String {
    let items: Vec<String> = xs.iter().map(|x| format!("'{x}'")).collect();
    format!("[{}]", items.join(", "))
}

// ------------------------------------------------------------------ colours

#[test]
fn colours_are_equal_exactly_when_refinement_cannot_tell_the_surroundings_apart() {
    for seed in 1..=6u64 {
        let d = random_data(seed, 60, 90);
        let mut g = Graph::memory();
        let order: Vec<usize> = (0..d.nodes.len()).collect();
        let ids = build(&mut g, &d, &order, 0);
        install(&mut g);
        for dir in ["out", "in", "both"] {
            for (types, props) in [
                (vec![], vec![]),
                (vec!["X", "Z"], vec![]),
                (vec![], vec!["p"]),
            ] {
                for depth in 0..=4u32 {
                    let r = q(
                        &mut g,
                        &format!(
                            "CALL structure.colors(depth: {depth}, dir: '{dir}', types: {}, props: {})",
                            list(&types),
                            list(&props)
                        ),
                    );
                    let got = partition(&colours(&r));
                    let want = in_ids(reference(&d, depth, dir, &types, &props), &ids);
                    assert_eq!(
                        got, want,
                        "seed {seed} dir {dir} types {types:?} props {props:?} depth {depth}"
                    );
                }
            }
        }
    }
}

#[test]
fn depth_zero_is_labels_and_named_properties_alone() {
    let mut g = Graph::memory();
    install(&mut g);
    q(&mut g, "CREATE (:A {name: 'one', code: 1}), (:A {name: 'two', code: 1}), (:A:B {name: 'three', code: 2}), (:B:A {name: 'four', code: 2.0})");
    let c = colours(&q(&mut g, "CALL structure.colors(depth: 0)"));
    // Names and codes are not looked at unless named.
    assert_eq!(partition(&c), vec![vec![1, 2], vec![3, 4]]);
    // Named, 2 and 2.0 are one value, as the engine compares them.
    let c = colours(&q(
        &mut g,
        "CALL structure.colors(depth: 0, props: ['code'])",
    ));
    assert_eq!(partition(&c), vec![vec![1, 2], vec![3, 4]]);
    let c = colours(&q(
        &mut g,
        "CALL structure.colors(depth: 0, props: ['name'])",
    ));
    assert_eq!(partition(&c).len(), 4);
}

#[test]
fn multiplicity_direction_and_type_each_tell_nodes_apart() {
    let mut g = Graph::memory();
    install(&mut g);
    // 1 -X-> 5 once, 2 -X-> 5 twice, 3 <-X- 5, 4 -Y-> 5.
    q(&mut g, "CREATE (:N), (:N), (:N), (:N), (:T)");
    for (a, b, t) in [
        (1, 5, "X"),
        (2, 5, "X"),
        (2, 5, "X"),
        (5, 3, "X"),
        (4, 5, "Y"),
    ] {
        g.add_edge(a, b, t, vec![]).unwrap();
    }
    let c = colours(&q(
        &mut g,
        "CALL structure.colors(depth: 1, dir: 'both', labels: ['N'])",
    ));
    assert_eq!(partition(&c).len(), 4, "{c:?}");
    // Out only: 3 has nothing going out, like nothing else; still four.
    let c = colours(&q(
        &mut g,
        "CALL structure.colors(depth: 1, dir: 'out', labels: ['N'])",
    ));
    assert_eq!(partition(&c).len(), 4, "{c:?}");
    // Following only Y: 1, 2 and 3 have no Y edges and look alike.
    let c = colours(&q(
        &mut g,
        "CALL structure.colors(depth: 1, types: ['Y'], labels: ['N'])",
    ));
    assert_eq!(partition(&c), vec![vec![1, 2, 3], vec![4]]);
}

#[test]
fn the_same_data_under_other_ids_gets_the_same_colours() {
    let d = random_data(11, 80, 140);
    let n = d.nodes.len();
    let forward: Vec<usize> = (0..n).collect();
    let mut shuffled = forward.clone();
    let mut r = Rng(99);
    for i in (1..n).rev() {
        shuffled.swap(i, r.below(i as u64 + 1) as usize);
    }
    let mut a = Graph::memory();
    let mut b = Graph::memory();
    let ids_a = build(&mut a, &d, &forward, 0);
    let ids_b = build(&mut b, &d, &shuffled, 37);
    assert_ne!(ids_a, ids_b);
    install(&mut a);
    install(&mut b);
    for args in [
        "depth: 3, dir: 'both', identity: 'key'",
        "depth: 'stable', dir: 'out', props: ['p'], identity: 'key'",
    ] {
        let by_key = |g: &mut Graph| -> BTreeMap<String, String> {
            q(g, &format!("CALL structure.colors({args})"))
                .rows
                .iter()
                .map(|row| (text(&row[2]), text(&row[1])))
                .collect()
        };
        let ca = by_key(&mut a);
        assert_eq!(ca.len(), n);
        assert_eq!(ca, by_key(&mut b), "{args}");
    }
    // Classes with identity: are the same rows but for the ids.
    for args in [
        "by: 'neighbours', dir: 'both', types: ['X'], identity: 'key', min_size: 1",
        "by: 'colors', depth: 2, identity: 'key'",
    ] {
        let strip = |g: &mut Graph| -> Vec<(i64, i64, String)> {
            q(g, &format!("CALL structure.classes({args})"))
                .rows
                .iter()
                .map(|row| {
                    (
                        row[0].as_i64().unwrap(),
                        row[1].as_i64().unwrap(),
                        row[4].to_json(),
                    )
                })
                .collect()
        };
        let ca = strip(&mut a);
        assert!(!ca.is_empty());
        assert_eq!(ca, strip(&mut b), "{args}");
    }
}

#[test]
fn stable_depth_stops_when_the_partition_stops_changing_and_states_its_cap() {
    let mut g = Graph::memory();
    install(&mut g);
    // A chain of 8: out-edges tell apart one more node each round.
    q(
        &mut g,
        "CREATE (:C), (:C), (:C), (:C), (:C), (:C), (:C), (:C)",
    );
    for i in 1..8u64 {
        g.add_edge(i, i + 1, "NEXT", vec![]).unwrap();
    }
    let r = q(&mut g, "CALL structure.colors(depth: 'stable')");
    assert_eq!(partition(&colours(&r)).len(), 8);
    let msg = r.message.clone().unwrap();
    assert!(
        msg.contains("stable: nothing changes past depth 7"),
        "{msg}"
    );
    // It is the fixed-depth colouring at that depth.
    let fixed = q(&mut g, "CALL structure.colors(depth: 7)");
    assert_eq!(colours(&r), colours(&fixed));
    // Capped below what it needs, it says so.
    let r = q(
        &mut g,
        "CALL structure.colors(depth: 'stable', max_depth: 3)",
    );
    assert_eq!(partition(&colours(&r)).len(), 4);
    let msg = r.message.unwrap();
    assert!(msg.contains("not stable after 3 rounds (the cap"), "{msg}");
    // Counts per depth, as rows.
    let s = q(
        &mut g,
        "CALL structure.colors(depth: 'stable', summary: true)",
    );
    let counts: Vec<i64> = s.rows.iter().map(|r| r[1].as_i64().unwrap()).collect();
    assert_eq!(counts, vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

// ------------------------------------------------------------------ classes

/// Interchangeable by neighbours, by hand.
fn reference_classes(d: &Data, keys: &[&str], dir: &str, types: &[&str]) -> BTreeSet<Vec<usize>> {
    let mut sig: Vec<(Vec<String>, Vec<String>, Vec<(char, String, usize)>)> = d
        .nodes
        .iter()
        .map(|(_, labels, ps)| {
            let mut ls = labels.clone();
            ls.sort();
            ls.dedup();
            let ks = keys
                .iter()
                .map(|k| {
                    format!(
                        "{:?}",
                        ps.iter().find(|(x, _)| x == k).map(|(_, v)| v.to_json())
                    )
                })
                .collect();
            (ls, ks, Vec::new())
        })
        .collect();
    for (a, b, t) in &d.edges {
        if !types.is_empty() && !types.contains(&t.as_str()) {
            continue;
        }
        if dir == "out" || dir == "both" {
            sig[*a].2.push(('>', t.clone(), *b));
        }
        if dir == "in" || dir == "both" {
            sig[*b].2.push(('<', t.clone(), *a));
        }
    }
    let mut by: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (v, s) in sig.iter_mut().enumerate() {
        s.2.sort();
        by.entry(format!("{s:?}")).or_default().push(v);
    }
    by.into_values().filter(|g| g.len() >= 2).collect()
}

#[test]
fn classes_by_neighbours_are_exactly_the_interchangeable_nodes() {
    for seed in 1..=5u64 {
        // Sparse, so many nodes share their few neighbours.
        let d = random_data(seed, 80, 50);
        let mut g = Graph::memory();
        let order: Vec<usize> = (0..d.nodes.len()).collect();
        let ids = build(&mut g, &d, &order, 0);
        install(&mut g);
        for (dir, types, keys) in [
            ("out", vec![], vec![]),
            ("both", vec!["X"], vec![]),
            ("in", vec![], vec!["p"]),
        ] {
            let r = q(
                &mut g,
                &format!(
                    "CALL structure.classes(by: 'neighbours', dir: '{dir}', types: {}, keys: {})",
                    list(&types),
                    list(&keys)
                ),
            );
            let got: BTreeSet<Vec<u64>> = r
                .rows
                .iter()
                .map(|row| match &row[3] {
                    Value::List(m) => m.iter().map(int).collect(),
                    _ => panic!(),
                })
                .collect();
            let want: BTreeSet<Vec<u64>> = reference_classes(&d, &keys, dir, &types)
                .into_iter()
                .map(|g| g.into_iter().map(|i| ids[i]).collect())
                .collect();
            assert_eq!(got, want, "seed {seed} {dir} {types:?} {keys:?}");
        }
    }
}

#[test]
fn classes_come_largest_first_with_ties_by_their_first_member_and_merge_nothing() {
    let mut g = Graph::memory();
    install(&mut g);
    // Hubs 1 and 2; leaves 3..=5 hang off 1, 6..=7 off 2, 8..=9 off 1 by Y.
    q(
        &mut g,
        "CREATE (:H), (:H), (:L), (:L), (:L), (:L), (:L), (:L), (:L)",
    );
    for (a, b, t) in [
        (3, 1, "X"),
        (4, 1, "X"),
        (5, 1, "X"),
        (6, 2, "X"),
        (7, 2, "X"),
        (8, 1, "Y"),
        (9, 1, "Y"),
    ] {
        g.add_edge(a, b, t, vec![]).unwrap();
    }
    let before = g.revision();
    let r = q(
        &mut g,
        "CALL structure.classes(by: 'neighbours', labels: ['L'])",
    );
    assert_eq!(r.columns, ["class", "size", "first", "members"]);
    let rows: Vec<(u64, u64, Vec<u64>)> = r
        .rows
        .iter()
        .map(|row| {
            let m = match &row[3] {
                Value::List(m) => m.iter().map(int).collect(),
                _ => panic!(),
            };
            (int(&row[0]), int(&row[1]), m)
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (1, 3, vec![3, 4, 5]),
            (2, 2, vec![6, 7]),
            (3, 2, vec![8, 9])
        ]
    );
    let msg = r.message.unwrap();
    assert!(
        msg.contains("not one thing, and nothing was merged"),
        "{msg}"
    );
    // Nothing was written: the graph and the revision are as they were.
    assert_eq!(g.revision(), before);
    assert_eq!(g.node_count(), 9);
    assert_eq!(g.edge_count(), 7);
    // By colours at depth 1 the hubs do not matter, only the edge types.
    let r = q(
        &mut g,
        "CALL structure.classes(by: 'colors', depth: 1, labels: ['L'])",
    );
    let sizes: Vec<u64> = r.rows.iter().map(|row| int(&row[1])).collect();
    assert_eq!(sizes, vec![5, 2]);
    let s = q(
        &mut g,
        "CALL structure.classes(by: 'neighbours', labels: ['L'], summary: true)",
    );
    let dist: Vec<(u64, u64)> = s.rows.iter().map(|r| (int(&r[0]), int(&r[1]))).collect();
    assert_eq!(dist, vec![(3, 1), (2, 2)]);
}

// --------------------------------------------------------- kept colourings

fn sample(g: &mut Graph) {
    let d = random_data(5, 40, 60);
    let order: Vec<usize> = (0..d.nodes.len()).collect();
    build(g, &d, &order, 0);
    install(g);
}

#[test]
fn without_store_or_write_nothing_is_written_and_the_revision_stays() {
    let mut g = Graph::memory();
    sample(&mut g);
    let before = g.revision();
    q(&mut g, "CALL structure.colors(depth: 3, dir: 'both')");
    q(
        &mut g,
        "CALL structure.colors(depth: 'stable', summary: true)",
    );
    q(&mut g, "CALL structure.classes(by: 'neighbours')");
    q(&mut g, "CALL structure.classes(by: 'colors')");
    q(&mut g, "CALL structure.stored()");
    assert_eq!(g.revision(), before);
    let mut keys = 0;
    g.ext_scan("structure", b"", &mut |_, _| {
        keys += 1;
        true
    });
    assert_eq!(keys, 0);
}

fn fresh(g: &mut Graph, name: &str) -> bool {
    let r = q(g, "CALL structure.stored()");
    let row = r
        .rows
        .iter()
        .find(|row| text(&row[0]) == name)
        .unwrap_or_else(|| panic!("{name} is not kept"));
    row[2] == Value::Bool(true)
}

/// Keeping, reading, staleness and dropping, on one graph.
fn kept_colourings_behave(g: &mut Graph) {
    let at = g.revision();
    let r = q(
        g,
        "CALL structure.colors(depth: 2, dir: 'both', store: 'a')",
    );
    let computed = colours(&r);
    assert!(g.revision() > at, "keeping one writes");
    let read = q(g, "CALL structure.read(name: 'a', fresh: 'require')");
    assert_eq!(colours(&read), computed);
    let msg = read.message.unwrap();
    assert!(msg.contains(&format!("computed at revision {at}")), "{msg}");
    assert!(msg.contains("fresh"), "{msg}");
    assert!(fresh(g, "a"));
    // The record says what it is.
    let s = q(g, "CALL structure.stored()");
    let row = &s.rows[0];
    assert_eq!(text(&row[1]), at.to_string());
    assert_eq!(text(&row[7]), glider::ext::structure::CODE);
    assert_eq!(
        text(&row[8]),
        r#"{"depth":2,"types":[],"dir":"both","props":[],"labels":[]}"#
    );
    // Keeping another and dropping it leaves the first fresh.
    q(g, "CALL structure.colors(depth: 1, store: 'b')");
    assert!(fresh(g, "a") && fresh(g, "b"));
    q(g, "CALL structure.drop(name: 'b')");
    assert!(fresh(g, "a"));
    assert_eq!(q(g, "CALL structure.stored()").rows.len(), 1);
    // A write anywhere else makes it stale, and require refuses it.
    q(g, "CREATE (:A {key: 'new'})");
    assert!(!fresh(g, "a"));
    let read = q(g, "CALL structure.read(name: 'a')");
    assert!(read.message.unwrap().contains("stale"));
    let e = err(g, "CALL structure.read(name: 'a', fresh: 'require')");
    assert!(e.contains("has changed since"), "{e}");
    let e = err(
        g,
        "CALL structure.classes(by: 'colors', colors: 'a', fresh: 'require')",
    );
    assert!(e.contains("has changed since"), "{e}");
    // Computed again, it is fresh again.
    q(
        g,
        "CALL structure.colors(depth: 2, dir: 'both', store: 'a')",
    );
    assert!(fresh(g, "a"));
    let by = q(
        g,
        "CALL structure.classes(by: 'colors', colors: 'a', fresh: 'require')",
    );
    let inline = q(
        g,
        "CALL structure.classes(by: 'colors', depth: 2, dir: 'both')",
    );
    assert_eq!(by.rows, inline.rows);
    // Inside an open transaction too.
    q(g, "BEGIN");
    q(g, "CALL structure.colors(depth: 1, store: 'c')");
    assert!(fresh(g, "c") && fresh(g, "a"));
    q(g, "COMMIT");
    assert!(fresh(g, "c") && fresh(g, "a"));
    q(g, "BEGIN");
    q(g, "CALL structure.colors(depth: 1, store: 'd')");
    q(g, "CREATE (:A {key: 'newer'})");
    assert!(!fresh(g, "d"));
    q(g, "ROLLBACK");
    let e = err(g, "CALL structure.read(name: 'd')");
    assert!(e.contains("no colouring is kept as 'd'"), "{e}");
    assert!(fresh(g, "a"));
}

#[test]
fn a_kept_colouring_says_whether_the_database_moved_since_in_memory() {
    let mut g = Graph::memory();
    sample(&mut g);
    kept_colourings_behave(&mut g);
}

#[test]
fn a_kept_colouring_says_whether_the_database_moved_since_with_a_log() {
    let mut g = Graph::memory();
    g.enable_log(1 << 30).unwrap();
    sample(&mut g);
    kept_colourings_behave(&mut g);
}

#[test]
fn a_kept_colouring_says_whether_the_database_moved_since_in_a_file_and_after_reopening() {
    let p = temp("kept");
    {
        let mut g = Graph::open_opts(&p, OpenOptions::default()).unwrap();
        sample(&mut g);
        kept_colourings_behave(&mut g);
    }
    let mut g = Graph::open_opts(&p, OpenOptions::default()).unwrap();
    assert!(fresh(&mut g, "a"));
    q(&mut g, "MATCH (n:A) SET n.touched = true");
    assert!(!fresh(&mut g, "a"));
    drop(g);
    cleanup(&p);
}

#[test]
fn bad_arguments_are_refused_with_what_to_do() {
    let mut g = Graph::memory();
    sample(&mut g);
    let e = err(&mut g, "CALL structure.colors(depth: 'deep')");
    assert!(e.contains("is not a depth"), "{e}");
    let e = err(&mut g, "CALL structure.classes(by: 'names')");
    assert!(e.contains("use \"neighbours\" or \"colors\""), "{e}");
    let e = err(&mut g, "CALL structure.read(name: 'none')");
    assert!(e.contains("CALL structure.stored() lists them"), "{e}");
    let e = err(&mut g, "CALL structure.colors(store: 'bad name')");
    assert!(e.contains("is not a colouring name"), "{e}");
    let e = err(&mut g, "CALL structure.colors(props: ['p'], write: 'p')");
    assert!(e.contains("write to another key"), "{e}");
}

#[test]
fn write_puts_each_colour_on_its_node() {
    let mut g = Graph::memory();
    sample(&mut g);
    let r = q(
        &mut g,
        "CALL structure.colors(depth: 2, labels: ['A'], write: 'shape')",
    );
    let c = colours(&r);
    assert!(!c.is_empty());
    for (id, col) in &c {
        assert_eq!(g.node_prop(*id, "shape"), Some(Value::Text(col.clone())));
    }
    let with = q(
        &mut g,
        "MATCH (n) WHERE n.shape IS NOT NULL RETURN count(n)",
    );
    assert_eq!(int(&with.rows[0][0]), c.len() as u64);
}

// ----------------------------------------------------------------- slices

#[test]
fn slices_of_rows_together_are_the_whole_result() {
    let mut g = Graph::memory();
    let d = random_data(8, 300, 200);
    let order: Vec<usize> = (0..d.nodes.len()).collect();
    build(&mut g, &d, &order, 0);
    install(&mut g);
    let whole = q(&mut g, "CALL structure.colors(depth: 2, store: 'k')");
    let read_whole = q(&mut g, "CALL structure.read(name: 'k')");
    assert_eq!(whole.rows, read_whole.rows);
    let classes_whole = q(
        &mut g,
        "CALL structure.classes(by: 'neighbours', min_size: 1)",
    );
    for limit in [1usize, 7, 50] {
        for proc in ["colors(depth: 2", "read(name: 'k'"] {
            let mut rows = Vec::new();
            let mut after = 0i64;
            loop {
                let r = q(
                    &mut g,
                    &format!("CALL structure.{proc}, after: {after}, limit: {limit})"),
                );
                if r.rows.is_empty() {
                    break;
                }
                assert!(r.rows.len() <= limit);
                after = r.rows.last().unwrap()[0].as_i64().unwrap();
                rows.extend(r.rows);
            }
            assert_eq!(rows, whole.rows, "{proc} by {limit}");
        }
        let mut rows = Vec::new();
        let mut after = 0i64;
        loop {
            let r = q(
                &mut g,
                &format!(
                    "CALL structure.classes(by: 'neighbours', min_size: 1, after: {after}, limit: {limit})"
                ),
            );
            if r.rows.is_empty() {
                break;
            }
            after = r.rows.last().unwrap()[0].as_i64().unwrap();
            rows.extend(r.rows);
        }
        assert_eq!(rows, classes_whole.rows, "classes by {limit}");
    }
}

// ------------------------------------------------------- bounds and tiers

#[test]
fn state_spills_past_the_memory_budget_and_nothing_changes() {
    let d = random_data(21, 6_000, 9_000);
    let order: Vec<usize> = (0..d.nodes.len()).collect();
    let mut big = Graph::memory();
    build(&mut big, &d, &order, 0);
    install(&mut big);
    let mut small = Graph::memory();
    build(&mut small, &d, &order, 0);
    install(&mut small);
    // 64 KiB of state for 6,000 nodes of 41 bytes each: it spills.
    small.set_work_mem(64 << 10);
    assert!(small.algorithm_memory() <= 64 << 10);
    glider::ooc::set_spill_shape(512, 4);
    for args in [
        "CALL structure.colors(depth: 3, dir: 'both')",
        "CALL structure.colors(depth: 'stable', summary: true)",
        "CALL structure.classes(by: 'neighbours', dir: 'in', min_size: 1)",
        "CALL structure.classes(by: 'colors', depth: 2)",
    ] {
        assert_eq!(
            dump(&q(&mut big, args)),
            dump(&q(&mut small, args)),
            "{args}"
        );
    }
}

#[test]
fn the_memory_and_paged_tiers_give_the_same_bytes() {
    let d = random_data(31, 500, 800);
    let order: Vec<usize> = (0..d.nodes.len()).collect();
    let p = temp("tiers");
    let mut file = Graph::open_opts(&p, OpenOptions::default()).unwrap();
    let mut mem = Graph::memory();
    for g in [&mut file, &mut mem] {
        build(g, &d, &order, 3);
        install(g);
    }
    for args in [
        "CALL structure.colors(depth: 3, dir: 'both', identity: 'key')",
        "CALL structure.colors(depth: 'stable', props: ['p'], summary: true)",
        "CALL structure.colors(depth: 2, store: 's')",
        "CALL structure.read(name: 's', identity: 'key')",
        "CALL structure.classes(by: 'neighbours', dir: 'both', types: ['X', 'Y'], identity: 'key', min_size: 1)",
        "CALL structure.classes(by: 'colors', colors: 's')",
    ] {
        let a = q(&mut file, args);
        let b = q(&mut mem, args);
        assert_eq!(dump(&a), dump(&b), "{args}");
        assert_eq!(
            a.message.unwrap().replace(char::is_numeric, ""),
            b.message.unwrap().replace(char::is_numeric, ""),
            "{args}"
        );
    }
    drop(file);
    cleanup(&p);
}

#[test]
fn check_finds_nothing_wrong_with_kept_colourings_even_when_stale() {
    let mut g = Graph::memory();
    sample(&mut g);
    q(&mut g, "CREATE EXTENSION check");
    q(&mut g, "CALL structure.colors(depth: 2, store: 'a')");
    q(&mut g, "CREATE (:A {key: 'later'})");
    for args in ["", ", limit: 1"] {
        let mut after = String::new();
        loop {
            let r = q(
                &mut g,
                &format!("CALL check.run(checks: ['derived']{args}{after})"),
            );
            assert!(
                r.rows.iter().all(|row| text(&row[1]) == "summary"),
                "{:?}",
                r.rows
            );
            let partial = r.rows.iter().find(|row| text(&row[2]) == "partial");
            match partial {
                Some(row) => after = format!(", after: '{}'", text(&row[3])),
                None => break,
            }
        }
    }
    // A record that does not decode is named.
    g.ext_write("structure", vec![(b"mbroken".to_vec(), vec![9])], vec![])
        .unwrap();
    let r = q(&mut g, "CALL check.run(checks: ['derived'])");
    assert!(
        r.rows
            .iter()
            .any(|row| text(&row[2]) == "structure.bad_record"),
        "{:?}",
        r.rows
    );
}
