//! `structure` templates over manufacturing graphs: every station design
//! is a template whose instances are exactly its stations without a
//! recorded deviation; smaller shapes inside stations are found and nested;
//! the long hierarchy path is a pattern; the plan is shorter than the plain
//! text, by exactly what it says; and every instance of the kept dictionary
//! expands to exactly what the graph holds.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufReader;

use glider::graph::Graph;
use glider::query::{self, QueryResult};
use glider::value::{parse_json, Json, Value};
use glider_bench::mfg::{generate, Config};
use glider_bench::structure::{
    expansion_exact, instance_sets, stations, template_recovery, types_arg, TEMPLATE_TYPES,
};

fn gen(seed: u64, nodes: u64) -> (Graph, Vec<Json>) {
    let (mut g, mut t, mut q) = (Vec::new(), Vec::new(), Vec::new());
    generate(
        &Config {
            seed,
            nodes,
            faults: false,
        },
        &mut g,
        &mut t,
        &mut q,
    )
    .unwrap();
    let mut db = Graph::memory();
    query::import_jsonl_from(&mut db, &mut BufReader::new(&g[..]), 1 << 20).unwrap();
    query::execute(&mut db, "CREATE EXTENSION structure").unwrap();
    let truth = String::from_utf8(t)
        .unwrap()
        .lines()
        .map(|l| parse_json(l).unwrap())
        .collect();
    (db, truth)
}

fn run(g: &mut Graph, src: &str) -> QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn cell<'a>(r: &'a QueryResult, row: usize, name: &str) -> &'a Value {
    let c = r.columns.iter().position(|c| c == name).unwrap();
    &r.rows[row][c]
}

fn all_instances(g: &mut Graph, args: &str) -> BTreeMap<String, BTreeSet<u64>> {
    let mut sets = BTreeMap::new();
    let mut after = 0;
    loop {
        let r = run(
            g,
            &format!(
                "CALL structure.templates({args}, instances: '*', grouped: true, after: {after})"
            ),
        );
        if r.rows.is_empty() {
            return sets;
        }
        after += instance_sets(&r.rows, &mut sets);
    }
}

#[test]
fn every_design_is_a_template_of_exactly_its_stations_without_a_deviation() {
    for (seed, nodes) in [(7, 5_000), (2, 30_000)] {
        let (mut g, truth) = gen(seed, nodes);
        let st = stations(&truth);
        let args = format!("types: {}, labels: ['Station']", types_arg(TEMPLATE_TYPES));
        let sets = all_instances(&mut g, &args);
        let rec = template_recovery(&sets, &st);
        assert!(rec.eligible > 2 && rec.deviated > 0, "{rec:?}");
        assert!(rec.exact(), "seed {seed}: {rec:?}");
    }
}

#[test]
fn smaller_shapes_are_found_nested_and_the_long_path_is_a_pattern() {
    let (mut g, _) = gen(3, 20_000);
    let h = types_arg(TEMPLATE_TYPES);
    let r = run(&mut g, &format!("CALL structure.templates(types: {h})"));
    let roots: BTreeSet<String> = (0..r.rows.len())
        .map(|i| cell(&r, i, "root").to_string())
        .collect();
    for l in ["Station", "Side", "WorkLocation", "WorkElement"] {
        assert!(roots.contains(l), "{roots:?}");
    }
    let p = run(&mut g, &format!("CALL structure.plan(types: {h})"));
    let nested = (1..p.rows.len())
        .filter(|&i| {
            *cell(&p, i, "chosen") == Value::Bool(true)
                && cell(&p, i, "nested_in").as_i64() > Some(0)
        })
        .count();
    assert!(nested > 0);
    let paths = run(
        &mut g,
        &format!("CALL structure.paths(types: {h}, limit: 1)"),
    );
    assert_eq!(
        cell(&paths, 0, "path").to_string(),
        "(:Enterprise) -HAS_PLANT-> (:Plant) -HAS_SHOP-> (:Shop) -HAS_LINE-> (:Line) -HAS_STATION-> (:Station) -HAS_SIDE-> (:Side) -HAS_LOCATION-> (:WorkLocation) -HAS_ELEMENT-> (:WorkElement) -HAS_INSTRUCTION-> (:WorkInstruction)"
    );
    let local = run(
        &mut g,
        "MATCH (:WorkElement)-[:HAS_INSTRUCTION]->(w:WorkInstruction) RETURN count(w)",
    );
    assert_eq!(cell(&paths, 0, "paths"), &local.rows[0][0]);
}

#[test]
fn the_plan_is_shorter_by_what_it_says_and_every_kept_instance_expands_exactly() {
    let (mut g, _) = gen(5, 15_000);
    let h = types_arg(TEMPLATE_TYPES);
    let plain = run(
        &mut g,
        &format!("CALL structure.plan(types: {h}, text: 'plain')"),
    );
    let compact = run(
        &mut g,
        &format!("CALL structure.plan(types: {h}, text: 'compact', store: 'mfg')"),
    );
    let (pt, ct) = (
        cell(&plain, 0, "text").to_string(),
        cell(&compact, 0, "text").to_string(),
    );
    assert_eq!(
        cell(&compact, 0, "plain_bytes").as_i64(),
        Some(pt.len() as i64)
    );
    assert_eq!(
        cell(&compact, 0, "use_bytes").as_i64(),
        Some(ct.len() as i64)
    );
    assert!(ct.len() * 3 < pt.len(), "{} vs {}", ct.len(), pt.len());
    let sets = all_instances(&mut g, "dictionary: 'mfg'");
    let (mut roots, mut exact) = (0, 0);
    for (id, members) in &sets {
        let list: Vec<String> = members.iter().map(|m| m.to_string()).collect();
        for chunk in list.chunks(1000) {
            let r = run(
                &mut g,
                &format!(
                    "CALL structure.expand(of: '{id}', root: [{}], dictionary: 'mfg')",
                    chunk.join(", ")
                ),
            );
            let (n, x) = expansion_exact(&g, &r);
            roots += n;
            exact += x;
        }
    }
    assert!(roots > 100);
    assert_eq!(roots, exact);
}
