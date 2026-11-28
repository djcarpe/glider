//! `CALL check.run()` over manufacturing graphs: the database checks say
//! nothing, and the data checks report exactly what the generator planted.

use std::io::BufReader;

use glider::graph::Graph;
use glider::query::{self, QueryResult};
use glider::value::{parse_json, Json};
use glider_bench::checks::{found, planted, Score};
use glider_bench::mfg::{generate, Config};

fn gen(seed: u64, nodes: u64, faults: bool) -> (Graph, Vec<Json>) {
    let (mut g, mut t, mut q) = (Vec::new(), Vec::new(), Vec::new());
    generate(
        &Config {
            seed,
            nodes,
            faults,
        },
        &mut g,
        &mut t,
        &mut q,
    )
    .unwrap();
    let mut db = Graph::memory();
    query::import_jsonl_from(&mut db, &mut BufReader::new(&g[..]), 1 << 20).unwrap();
    let truth = String::from_utf8(t)
        .unwrap()
        .lines()
        .map(|l| parse_json(l).unwrap())
        .collect();
    (db, truth)
}

fn run(g: &mut Graph, args: &str) -> QueryResult {
    query::execute(g, "CREATE EXTENSION IF NOT EXISTS check").unwrap();
    query::execute(g, &format!("CALL check.run({args})")).unwrap()
}

#[test]
fn the_database_checks_find_nothing_wrong_with_a_generated_graph() {
    for faults in [false, true] {
        let (mut g, _) = gen(7, 5_000, faults);
        query::execute(&mut g, "CREATE EXTENSION context").unwrap();
        query::execute(&mut g, "CALL context.index()").unwrap();
        let r = run(&mut g, "");
        assert_eq!(found(&r).faults, 0, "faults: {faults}: {:?}", r.rows);
    }
}

#[test]
fn orphans_and_parallel_edges_are_exactly_the_planted_faults() {
    for (seed, nodes) in [(7, 5_000), (2, 20_000)] {
        let (mut g, truth) = gen(seed, nodes, true);
        let p = planted(&truth);
        assert!(p.orphans.len() >= 3 && p.parallel.len() >= 3);
        let f = found(&run(&mut g, "checks: ['orphans', 'duplicates']"));
        assert!(
            Score::of(&p.orphans, &f.orphans).exact(),
            "orphans {:?} vs {:?}",
            p.orphans,
            f.orphans
        );
        assert!(
            Score::of(&p.parallel, &f.parallel).exact(),
            "parallel {:?} vs {:?}",
            p.parallel,
            f.parallel
        );
        // Without faults there are none to find.
        let (mut g, _) = gen(seed, nodes, false);
        let f = found(&run(&mut g, "checks: ['orphans', 'duplicates']"));
        assert!(f.orphans.is_empty() && f.parallel.is_empty());
    }
}

#[test]
fn duplicates_on_the_properties_they_agree_on_are_the_planted_pairs_and_no_lookalikes() {
    let (mut g, truth) = gen(7, 20_000, false);
    let p = planted(&truth);
    for label in ["Tool", "Part"] {
        let (keys, pairs) = &p.duplicates[label];
        assert!(!pairs.is_empty(), "{label}");
        let keys: Vec<String> = keys.iter().map(|k| format!("'{k}'")).collect();
        let r = run(
            &mut g,
            &format!(
                "checks: ['duplicates'], label: '{label}', keys: [{}]",
                keys.join(", ")
            ),
        );
        let f = found(&r);
        assert!(
            Score::of(pairs, &f.duplicates).exact(),
            "{label}: {pairs:?} vs {:?}",
            f.duplicates
        );
        let look = p.lookalikes.get(label).cloned().unwrap_or_default();
        assert!(!look.is_empty(), "{label} has look-alikes");
        assert_eq!(look.intersection(&f.duplicates).count(), 0, "{label}");
    }
}
