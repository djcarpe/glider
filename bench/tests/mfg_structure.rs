//! `structure` over manufacturing graphs: station colours recover the
//! designs and set apart every deviation, and classes by neighbours are
//! exactly the planted duplicates, none of the look-alikes.

use std::io::BufReader;

use glider::graph::Graph;
use glider::query::{self, QueryResult};
use glider::value::{parse_json, Json};
use glider_bench::checks::{planted, Score};
use glider_bench::mfg::{generate, Config};
use glider_bench::structure::{
    class_pairs, closure, colours, duplicate_types, recovery, stations, types_arg, DESIGN_DEPTH,
    DESIGN_TYPES,
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

#[test]
fn station_colours_recover_the_designs_and_set_apart_every_deviation() {
    for (seed, nodes) in [(7, 5_000), (2, 30_000)] {
        let (mut g, truth) = gen(seed, nodes);
        let st = stations(&truth);
        assert!(st.iter().any(|s| s.deviated));
        let types = types_arg(DESIGN_TYPES);
        for depth in 1..=DESIGN_DEPTH + 1 {
            // All stations in slices of the row cap.
            let mut c = std::collections::BTreeMap::new();
            let mut after = 0;
            loop {
                let r = run(
                    &mut g,
                    &format!(
                        "CALL structure.colors(depth: {depth}, types: {types}, dir: 'out', labels: ['Station'], after: {after})"
                    ),
                );
                if r.rows.is_empty() {
                    break;
                }
                after = r.rows.last().unwrap()[0].as_i64().unwrap();
                c.extend(colours(&r));
            }
            let rec = recovery(&c, &st);
            if depth < DESIGN_DEPTH {
                // A missing tool is four hops down: not seen yet.
                assert!(!rec.exact(), "seed {seed} depth {depth}: {rec:?}");
            } else {
                assert!(rec.exact(), "seed {seed} depth {depth}: {rec:?}");
            }
        }
    }
}

#[test]
fn classes_by_neighbours_are_the_planted_duplicates_and_no_lookalikes() {
    for (seed, nodes) in [(7, 20_000), (3, 40_000)] {
        let (mut g, truth) = gen(seed, nodes);
        let p = planted(&truth);
        for label in ["Tool", "Part", "WorkInstruction"] {
            let (keys, pairs) = &p.duplicates[label];
            let pairs = &closure(pairs);
            let keys: Vec<&str> = keys.iter().map(|k| k.as_str()).collect();
            let (types, dir) = duplicate_types(label);
            let r = run(
                &mut g,
                &format!(
                    "CALL structure.classes(by: 'neighbours', labels: ['{label}'], keys: {}, types: {}, dir: '{dir}')",
                    types_arg(&keys),
                    types_arg(types)
                ),
            );
            let found = class_pairs(&r);
            let s = Score::of(pairs, &found);
            assert!(
                s.exact(),
                "seed {seed} {label}: {s:?} {pairs:?} vs {found:?}"
            );
            let look = p.lookalikes.get(label).cloned().unwrap_or_default();
            assert!(!look.is_empty(), "{label} has look-alikes");
            assert_eq!(look.intersection(&found).count(), 0, "seed {seed} {label}");
        }
    }
}
