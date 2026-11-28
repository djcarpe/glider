//! Vectors over a manufacturing graph: a space over every work
//! instruction, searched exactly (the same rows as a brute-force ranking),
//! with exactly the changed and deleted instructions reported, and nothing
//! for `check.run` to find.

use std::io::BufReader;

use glider::ext::check::Probe;
use glider::ext::vector::{self, Target};
use glider::graph::Graph;
use glider::query;
use glider::value::Value;
use glider_bench::mfg::{generate, Config};

fn gen(seed: u64, nodes: u64) -> Graph {
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
    db
}

fn x(r: &mut u64) -> f64 {
    *r ^= *r << 13;
    *r ^= *r >> 7;
    *r ^= *r << 17;
    (*r >> 11) as f64 / (1u64 << 52) as f64 - 1.0
}

#[test]
fn work_instructions_are_found_exactly_and_their_changes_reported() {
    let mut g = gen(3, 3_000);
    for s in [
        "CREATE EXTENSION changes",
        "CREATE EXTENSION vector",
        "CALL vector.create(space: 'wi', dims: 32, labels: ['WorkInstruction'], model: 'synthetic', model_version: '1')",
    ] {
        query::execute(&mut g, s).unwrap();
    }
    let ids = g.nodes_with_label("WorkInstruction");
    assert!(ids.len() > 500, "{}", ids.len());
    let mut r = 7u64;
    let vs: Vec<Vec<f64>> = ids
        .iter()
        .map(|_| (0..32).map(|_| x(&mut r)).collect())
        .collect();
    for (chunk, vchunk) in ids.chunks(500).zip(vs.chunks(500)) {
        let p = vec![
            (
                "ids".to_string(),
                Value::List(chunk.iter().map(|&i| Value::Int(i as i64)).collect()),
            ),
            (
                "vs".to_string(),
                Value::List(
                    vchunk
                        .iter()
                        .map(|v| Value::List(v.iter().map(|f| Value::Float(*f)).collect()))
                        .collect(),
                ),
            ),
        ];
        query::execute_with(
            &mut g,
            "CALL vector.put(space: 'wi', ids: $ids, vectors: $vs)",
            &p,
        )
        .unwrap();
    }
    // Exact: the rows are a brute-force cosine ranking of the stored floats.
    let s = vector::space(&g, "wi").unwrap();
    let qv: Vec<f64> = (0..32).map(|_| x(&mut r)).collect();
    let qn: f64 = qv.iter().map(|a| a * a).sum::<f64>().sqrt();
    let mut want: Vec<(u64, f64)> = ids
        .iter()
        .map(|&id| {
            let (v, _) = vector::get(&g, &s, &Target::Id(id)).unwrap();
            let (mut dot, mut ss) = (0.0f64, 0.0f64);
            for i in 0..32 {
                dot += qv[i] / qn * v[i] as f64;
                ss += v[i] as f64 * v[i] as f64;
            }
            (id, dot / ss.sqrt())
        })
        .collect();
    want.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    want.truncate(25);
    let p = vec![(
        "q".to_string(),
        Value::List(qv.iter().map(|f| Value::Float(*f)).collect()),
    )];
    let got = query::execute_with(
        &mut g,
        "CALL vector.search(space: 'wi', vector: $q, top: 25, fresh: 'require')",
        &p,
    )
    .unwrap();
    let got: Vec<(u64, f64)> = got
        .rows
        .iter()
        .map(|row| (row[0].as_i64().unwrap() as u64, row[1].as_f64().unwrap()))
        .collect();
    assert_eq!(got, want);

    // Change some instructions, delete others: stale lists exactly the
    // changed ones, and the deleted ones' vectors are gone.
    let changed: Vec<u64> = ids.iter().step_by(37).copied().collect();
    for &id in &changed {
        g.set_node_prop(id, "text", Value::from("rewritten"))
            .unwrap();
    }
    let deleted: Vec<u64> = ids
        .iter()
        .skip(5)
        .step_by(41)
        .filter(|i| !changed.contains(i))
        .copied()
        .collect();
    for &id in &deleted {
        query::execute(
            &mut g,
            &format!("MATCH (n) WHERE id(n) = {id} DETACH DELETE n"),
        )
        .unwrap();
    }
    let st = query::execute(&mut g, "CALL vector.stale(space: 'wi', limit: 10000)").unwrap();
    let stale: Vec<u64> = st
        .rows
        .iter()
        .map(|r| r[0].as_i64().unwrap() as u64)
        .collect();
    assert_eq!(stale, changed);
    assert!(st.rows.iter().all(|r| r[1] == Value::from("stale")));
    for &id in &deleted {
        assert!(vector::get(&g, &s, &Target::Id(id)).is_none());
    }
    let count = vector::stats(&g, &s).count;
    assert_eq!(count as usize, ids.len() - deleted.len());
    let mut probe = Probe::whole(1 << 20);
    assert!(glider::ext::find("vector")
        .unwrap()
        .check(&g, &mut probe)
        .unwrap());
    assert!(probe.findings().is_empty(), "{:?}", probe.findings().len());
}
