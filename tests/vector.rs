//! Vectors beside the graph (`vector`, docs/DERIVED.md): named spaces that
//! record what makes their vectors comparable, packed 32-bit floats, exact
//! search equal to a brute-force computation on both tiers, vectors that go
//! with their node or edge, and staleness that is reported, never hidden.

use std::path::{Path, PathBuf};

use glider::ext;
use glider::ext::check::Probe;
use glider::ext::vector::{self, Fresh, Target};
use glider::graph::Graph;
use glider::query::{self, QueryResult};
use glider::store::{Op, Sync};
use glider::value::Value;

fn q(g: &mut Graph, src: &str) -> QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn qp(g: &mut Graph, src: &str, params: &[(&str, Value)]) -> QueryResult {
    let p: Vec<(String, Value)> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    query::execute_with(g, src, &p).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn err(g: &mut Graph, src: &str, params: &[(&str, Value)]) -> String {
    let p: Vec<(String, Value)> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    match query::execute_with(g, src, &p) {
        Ok(r) => panic!("{src} should fail, gave {:?}", r.message),
        Err(e) => e.to_string(),
    }
}

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-vector-{}-{}.db", name, std::process::id()));
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

fn keyspace(g: &Graph, ns: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    g.ext_scan(ns, b"", &mut |k, v| {
        out.push((k.to_vec(), v.to_vec()));
        true
    });
    out
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// A float in [-1, 1) on a grid of 1/8, so ties happen.
    fn coarse(&mut self) -> f64 {
        (self.next() % 16) as f64 / 8.0 - 1.0
    }
    fn fine(&mut self) -> f64 {
        (self.next() % 2_000_001) as f64 / 1e6 - 1.0
    }
}

fn vlist(v: &[f64]) -> Value {
    Value::List(v.iter().map(|x| Value::Float(*x)).collect())
}

fn ilist(ids: &[u64]) -> Value {
    Value::List(ids.iter().map(|i| Value::Int(*i as i64)).collect())
}

fn put(g: &mut Graph, space: &str, ids: &[u64], vs: &[Vec<f64>]) -> QueryResult {
    qp(
        g,
        &format!("CALL vector.put(space: '{space}', ids: $ids, vectors: $vs)"),
        &[
            ("ids", ilist(ids)),
            ("vs", Value::List(vs.iter().map(|v| vlist(v)).collect())),
        ],
    )
}

fn nodes(g: &mut Graph, label: &str, n: usize) -> Vec<u64> {
    (0..n)
        .map(|i| {
            g.add_node(
                &[label.to_string()],
                vec![("i".into(), Value::Int(i as i64))],
            )
            .unwrap()
        })
        .collect()
}

fn space_row(g: &mut Graph, name: &str) -> Vec<(String, Value)> {
    let r = q(g, "CALL vector.spaces()");
    let row = r
        .rows
        .iter()
        .find(|row| row[0] == Value::from(name))
        .unwrap_or_else(|| panic!("no space {name}"))
        .clone();
    r.columns.into_iter().zip(row).collect()
}

fn field(row: &[(String, Value)], k: &str) -> Value {
    row.iter().find(|(c, _)| c == k).unwrap().1.clone()
}

// ------------------------------------------------------------ brute force

fn f32s(v: &[f64], normalized: bool) -> Vec<f64> {
    let n = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    let s = if normalized { 1.0 / n } else { 1.0 };
    v.iter().map(|x| ((x * s) as f32) as f64).collect()
}

/// The score as the documentation defines it, computed here on its own.
fn brute_score(metric: &str, normalized: bool, q: &[f64], stored: &[f64]) -> f64 {
    let qn = if metric == "cosine" || normalized {
        let n = q.iter().map(|x| x * x).sum::<f64>().sqrt();
        q.iter().map(|x| x / n).collect::<Vec<_>>()
    } else {
        q.to_vec()
    };
    let mut dot = 0.0f64;
    for i in 0..qn.len() {
        dot += qn[i] * stored[i];
    }
    match metric {
        "dot" => dot,
        "cosine" if normalized => dot,
        "cosine" => {
            let mut ss = 0.0f64;
            for x in stored {
                ss += x * x;
            }
            dot / ss.sqrt()
        }
        _ => {
            let mut d = 0.0f64;
            for i in 0..qn.len() {
                let e = qn[i] - stored[i];
                d += e * e;
            }
            -d.sqrt()
        }
    }
}

fn brute(
    metric: &str,
    normalized: bool,
    q: &[f64],
    all: &[(u64, Vec<f64>)],
    top: usize,
    min: Option<f64>,
) -> Vec<(u64, f64)> {
    let mut s: Vec<(u64, f64)> = all
        .iter()
        .map(|(id, v)| {
            (
                *id,
                brute_score(metric, normalized, q, &f32s(v, normalized)),
            )
        })
        .filter(|(_, sc)| min.map_or(true, |m| *sc >= m))
        .collect();
    s.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    s.truncate(top);
    s
}

fn rows_of(r: &QueryResult) -> Vec<(u64, f64)> {
    r.rows
        .iter()
        .map(|row| {
            let Value::Float(sc) = row[1] else {
                panic!("score {:?}", row[1])
            };
            (row[0].as_i64().unwrap() as u64, sc)
        })
        .collect()
}

// ------------------------------------------------------------------ tests

#[test]
fn a_space_records_once_what_makes_its_vectors_comparable_and_where_they_came_from() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION vector");
    let created_at = g.log_end();
    q(
        &mut g,
        "CALL vector.create(space: 'text', dims: 4, metric: 'dot', normalized: true, of: 'node', labels: ['Doc'], algorithm: 'mean-pool', config: '{\"layers\":2}', model: 'mini', model_version: '3', code: 'app/7', at: '2026-11-28T10:00:00Z')",
    );
    let row = space_row(&mut g, "text");
    assert_eq!(field(&row, "dims"), Value::Int(4));
    assert_eq!(field(&row, "metric"), Value::from("dot"));
    assert_eq!(field(&row, "normalized"), Value::Bool(true));
    assert_eq!(field(&row, "of"), Value::from("node"));
    assert_eq!(field(&row, "labels"), Value::List(vec![Value::from("Doc")]));
    assert_eq!(field(&row, "algorithm"), Value::from("mean-pool"));
    assert_eq!(field(&row, "config"), Value::from("{\"layers\":2}"));
    assert_eq!(field(&row, "model"), Value::from("mini"));
    assert_eq!(field(&row, "model_version"), Value::from("3"));
    assert_eq!(field(&row, "code"), Value::from("app/7"));
    assert_eq!(field(&row, "at"), Value::from("2026-11-28T10:00:00Z"));
    assert_eq!(field(&row, "created"), Value::Int(created_at as i64));
    assert_eq!(field(&row, "vectors"), Value::Int(0));
    assert_eq!(field(&row, "bytes"), Value::Int(0));
    assert_eq!(field(&row, "revision"), Value::Int(created_at as i64));

    // Defaults: cosine, not normalised, of nodes, this code's version.
    q(&mut g, "CALL vector.create(space: 'plain', dims: 3)");
    let row = space_row(&mut g, "plain");
    assert_eq!(field(&row, "metric"), Value::from("cosine"));
    assert_eq!(field(&row, "normalized"), Value::Bool(false));
    assert_eq!(field(&row, "of"), Value::from("node"));
    assert_eq!(field(&row, "code"), Value::from(vector::CODE));
    assert_eq!(field(&row, "model"), Value::Null);

    let ids = nodes(&mut g, "Doc", 3);
    let r0 = g.log_end();
    put(
        &mut g,
        "text",
        &ids,
        &[
            vec![1.0, 0.0, 0.0, 0.0],
            vec![0.0, 2.0, 0.0, 0.0],
            vec![0.0, 0.0, 0.0, 3.0],
        ],
    );
    let row = space_row(&mut g, "text");
    assert_eq!(field(&row, "vectors"), Value::Int(3));
    assert_eq!(field(&row, "revision"), Value::Int(r0 as i64));
    let s = vector::space(&g, "text").unwrap();
    assert_eq!(
        field(&row, "bytes"),
        Value::Int(3 * s.bytes_per_vector(8, 0) as i64)
    );

    // The same definition again is nothing; another one is refused.
    let r = q(&mut g, "CALL vector.create(space: 'plain', dims: 3)");
    assert!(r.message.unwrap().contains("already exists"));
    for other in [
        "dims: 4",
        "dims: 3, metric: 'l2'",
        "dims: 3, normalized: true",
        "dims: 3, model: 'other'",
        "dims: 3, of: 'edge'",
        "dims: 3, labels: ['Doc']",
    ] {
        let e = err(
            &mut g,
            &format!("CALL vector.create(space: 'plain', {other})"),
            &[],
        );
        assert!(
            e.contains("vector.drop") && e.contains("incomparable"),
            "{other}: {e}"
        );
    }
    // Dropped and made again, it holds nothing.
    q(&mut g, "CALL vector.drop(space: 'text')");
    assert!(vector::space(&g, "text").is_none());
    q(&mut g, "CALL vector.create(space: 'text', dims: 2)");
    let row = space_row(&mut g, "text");
    assert_eq!(field(&row, "vectors"), Value::Int(0));
    assert_eq!(
        keyspace(&g, vector::NS)
            .iter()
            .filter(|(k, _)| k[0] == b'v')
            .count(),
        0
    );

    for bad in [
        "CALL vector.create(space: 'x')",
        "CALL vector.create(space: 'x', dims: 0)",
        "CALL vector.create(space: 'x', dims: 2, metric: 'manhattan')",
        "CALL vector.create(space: 'x', dims: 2, of: 'graph')",
        "CALL vector.create(space: 'x', dims: 2, of: 'edge', labels: ['A'])",
        "CALL vector.create(dims: 2)",
    ] {
        err(&mut g, bad, &[]);
    }
}

#[test]
fn vectors_are_packed_32_bit_floats_close_to_four_bytes_a_dimension() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION vector");
    for dims in [64usize, 384, 768] {
        let name = format!("d{dims}");
        q(
            &mut g,
            &format!("CALL vector.create(space: '{name}', dims: {dims}, metric: 'l2')"),
        );
        let ids = nodes(&mut g, "N", 10);
        let mut r = Rng(dims as u64);
        let vs: Vec<Vec<f64>> = (0..10)
            .map(|_| (0..dims).map(|_| r.fine()).collect())
            .collect();
        put(&mut g, &name, &ids, &vs);
        let s = vector::space(&g, &name).unwrap();
        let per = s.bytes_per_vector(8, 0);
        assert_eq!(
            field(&space_row(&mut g, &name), "bytes"),
            Value::Int(10 * per as i64)
        );
        // 4 bytes a dimension and 48 bytes a vector, whatever its length.
        assert_eq!(per, 4 * dims as u64 + 48);
        eprintln!(
            "{dims} dims: {per} bytes a vector, {:.3} a dimension",
            per as f64 / dims as f64
        );
        // Each value is exactly the floats, packed, and reads back as f32.
        for (id, v) in ids.iter().zip(&vs) {
            let (got, meta) = vector::get(&g, &s, &Target::Id(*id)).unwrap();
            let want: Vec<f32> = v.iter().map(|x| *x as f32).collect();
            assert_eq!(got, want);
            assert!(meta.hash.is_none());
        }
        let vk: Vec<_> = keyspace(&g, vector::NS)
            .into_iter()
            .filter(|(k, _)| k[0] == b'v' && k[1..5] == s.sid.to_be_bytes())
            .collect();
        assert_eq!(vk.len(), 10);
        assert!(vk.iter().all(|(_, v)| v.len() == 4 * dims));
    }
}

#[test]
fn put_refuses_what_does_not_fit_and_writes_nothing_when_it_does() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION vector");
    q(&mut g, "CALL vector.create(space: 'c', dims: 3)");
    q(
        &mut g,
        "CALL vector.create(space: 'n', dims: 3, metric: 'dot', normalized: true)",
    );
    q(
        &mut g,
        "CALL vector.create(space: 'k', dims: 3, metric: 'l2', of: 'key')",
    );
    let ids = nodes(&mut g, "N", 2);
    let before = keyspace(&g, vector::NS);
    let rev = g.revision();
    let cases: Vec<(&str, Value, Value, &str)> = vec![
        (
            "c",
            ilist(&ids),
            Value::List(vec![vlist(&[1.0, 2.0]), vlist(&[1.0, 2.0, 3.0])]),
            "2 elements",
        ),
        (
            "c",
            ilist(&ids),
            Value::List(vec![vlist(&[1.0, 2.0, 3.0]), vlist(&[1.0, 1e39, 3.0])]),
            "not a finite",
        ),
        (
            "c",
            ilist(&[ids[0], 999]),
            Value::List(vec![vlist(&[1.0, 2.0, 3.0]), vlist(&[1.0, 2.0, 3.0])]),
            "does not exist",
        ),
        (
            "c",
            ilist(&[ids[0], ids[0]]),
            Value::List(vec![vlist(&[1.0, 2.0, 3.0]), vlist(&[1.0, 2.0, 3.0])]),
            "twice",
        ),
        (
            "c",
            ilist(&ids),
            Value::List(vec![vlist(&[1.0, 2.0, 3.0]), vlist(&[0.0, 0.0, 0.0])]),
            "all zeros",
        ),
        (
            "n",
            ilist(&ids),
            Value::List(vec![vlist(&[1.0, 2.0, 3.0]), vlist(&[0.0, 0.0, 0.0])]),
            "all zeros",
        ),
        (
            "c",
            ilist(&ids),
            Value::List(vec![vlist(&[1.0, 2.0, 3.0])]),
            "one vector per target",
        ),
        (
            "c",
            ilist(&ids),
            Value::List(vec![vlist(&[1.0, 2.0, 3.0]), Value::Null]),
            "no vector given",
        ),
        (
            "c",
            ilist(&ids),
            Value::List(vec![
                vlist(&[1.0, 2.0, 3.0]),
                Value::List(vec![Value::from("a"), Value::Int(1), Value::Int(1)]),
            ]),
            "list of numbers",
        ),
        (
            "k",
            ilist(&ids),
            Value::List(vec![vlist(&[1.0, 2.0, 3.0]), vlist(&[1.0, 2.0, 3.0])]),
            "keys",
        ),
    ];
    for (space, ids_v, vs, want) in cases {
        let e = err(
            &mut g,
            &format!("CALL vector.put(space: '{space}', ids: $ids, vectors: $vs)"),
            &[("ids", ids_v), ("vs", vs)],
        );
        assert!(e.contains(want), "{want}: {e}");
        assert_eq!(
            keyspace(&g, vector::NS),
            before,
            "{want}: something was written"
        );
        assert_eq!(g.revision(), rev);
    }
    // A normalised space stores unit vectors; a dot space with zeros is fine.
    put(
        &mut g,
        "n",
        &ids,
        &[vec![3.0, 0.0, 4.0], vec![0.0, 0.5, 0.0]],
    );
    let s = vector::space(&g, "n").unwrap();
    let (v, _) = vector::get(&g, &s, &Target::Id(ids[0])).unwrap();
    assert_eq!(v, vec![0.6f32, 0.0, 0.8]);
    q(
        &mut g,
        "CALL vector.create(space: 'd', dims: 3, metric: 'dot')",
    );
    put(
        &mut g,
        "d",
        &ids,
        &[vec![0.0, 0.0, 0.0], vec![1.0, 1.0, 1.0]],
    );
    // Keys in a key space; an empty key is refused.
    qp(
        &mut g,
        "CALL vector.put(space: 'k', keys: ['line-1', 'whole'], vectors: $vs)",
        &[(
            "vs",
            Value::List(vec![vlist(&[1.0, 2.0, 3.0]), vlist(&[0.0, 0.0, 0.0])]),
        )],
    );
    let e = err(
        &mut g,
        "CALL vector.put(space: 'k', keys: [''], vectors: [[1,2,3]])",
        &[],
    );
    assert!(e.contains("key must be"), "{e}");
}

#[test]
fn a_put_with_an_unchanged_hash_only_records_that_the_vector_is_current() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    q(&mut g, "CREATE EXTENSION vector");
    q(
        &mut g,
        "CALL vector.create(space: 't', dims: 384, model: 'm')",
    );
    let ids = nodes(&mut g, "Doc", 2);
    let mut r = Rng(9);
    let vs: Vec<Vec<f64>> = (0..2)
        .map(|_| (0..384).map(|_| r.fine()).collect())
        .collect();
    let all = Value::List(vs.iter().map(|v| vlist(v)).collect());
    qp(
        &mut g,
        "CALL vector.put(space: 't', ids: $ids, vectors: $vs, hashes: ['a', 'b'])",
        &[("ids", ilist(&ids)), ("vs", all.clone())],
    );
    let s = vector::space(&g, "t").unwrap();
    let (v0, m0) = vector::get(&g, &s, &Target::Id(ids[0])).unwrap();
    assert_eq!(m0.hash.as_deref(), Some("a"));
    let bytes = field(&space_row(&mut g, "t"), "bytes");

    // The node's text changes, but its source hash does not.
    g.set_node_prop(ids[0], "text", Value::from("reworded"))
        .unwrap();
    let st = q(&mut g, "CALL vector.stale(space: 't')");
    assert_eq!(st.rows.len(), 1);
    assert_eq!(st.rows[0][1], Value::from("stale"));

    // With the same hash and no vector: only the revision is rewritten.
    let before = g.revision();
    let r = q(
        &mut g,
        &format!(
            "CALL vector.put(space: 't', ids: [{}], vectors: [null], hashes: ['a'])",
            ids[0]
        ),
    );
    assert!(r.message.unwrap().contains("1 unchanged by hash"));
    let cost = g.revision() - before;
    let (v1, m1) = vector::get(&g, &s, &Target::Id(ids[0])).unwrap();
    assert_eq!(v1, v0);
    assert!(m1.revision > m0.revision);
    assert_eq!(field(&space_row(&mut g, "t"), "bytes"), bytes);
    assert_eq!(q(&mut g, "CALL vector.stale(space: 't')").rows.len(), 0);

    // With the same hash and the vector given: still only the revision.
    let before = g.revision();
    qp(
        &mut g,
        &format!(
            "CALL vector.put(space: 't', ids: [{}], vectors: $vs, hashes: ['b'])",
            ids[1]
        ),
        &[("vs", Value::List(vec![vlist(&vs[1])]))],
    );
    let same_cost = g.revision() - before;
    // A changed hash rewrites the vector.
    let before = g.revision();
    qp(
        &mut g,
        &format!(
            "CALL vector.put(space: 't', ids: [{}], vectors: $vs, hashes: ['b2'])",
            ids[1]
        ),
        &[("vs", Value::List(vec![vlist(&vs[0])]))],
    );
    let full_cost = g.revision() - before;
    eprintln!("log bytes: hash unchanged {cost} / {same_cost}, rewritten {full_cost}");
    assert!(cost < 200 && same_cost < 200, "{cost} {same_cost}");
    assert!(full_cost > 4 * 384, "{full_cost}");
    let h = q(
        &mut g,
        &format!(
            "CALL vector.hashes(space: 't', ids: [{}, {}, 999])",
            ids[0], ids[1]
        ),
    );
    assert_eq!(h.columns, vec!["id", "revision", "hash", "state"]);
    assert_eq!(h.rows[0][2], Value::from("a"));
    assert_eq!(h.rows[1][2], Value::from("b2"));
    assert_eq!(h.rows[2][1], Value::Null);
    assert_eq!(h.rows[2][2], Value::Null);
}

#[test]
fn get_and_delete_complete_the_surface() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION vector");
    q(
        &mut g,
        "CALL vector.create(space: 's', dims: 2, metric: 'l2')",
    );
    let ids = nodes(&mut g, "N", 3);
    put(
        &mut g,
        "s",
        &ids,
        &[vec![0.5, 1.0], vec![2.0, -1.0], vec![0.0, 0.0]],
    );
    let r = q(
        &mut g,
        &format!(
            "CALL vector.get(space: 's', ids: ['n{}', {}])",
            ids[1], ids[2]
        ),
    );
    assert_eq!(r.columns, vec!["id", "revision", "hash", "state", "vector"]);
    assert_eq!(r.rows[0][4], vlist(&[2.0, -1.0]));
    assert_eq!(r.rows[0][3], Value::from("unknown"));
    let r = q(
        &mut g,
        &format!(
            "CALL vector.delete(space: 's', ids: [{}, {}, 999])",
            ids[0], ids[1]
        ),
    );
    assert!(r.message.unwrap().contains("deleted 2"));
    assert_eq!(field(&space_row(&mut g, "s"), "vectors"), Value::Int(1));
    let r = q(
        &mut g,
        &format!("CALL vector.get(space: 's', ids: [{}])", ids[0]),
    );
    assert_eq!(r.rows[0][4], Value::Null);
    let e = err(&mut g, "CALL vector.get(space: 'nope', ids: [1])", &[]);
    assert!(e.contains("vector.create"), "{e}");
}

/// Every metric, normalised or not, with ties, over memory and file:
/// the rows are the brute-force top, score for score, and the same bytes
/// on both tiers.
#[test]
fn search_is_exact_and_the_same_as_brute_force_on_both_tiers() {
    let path = temp("exact");
    let mut file = Graph::open(&path, Sync::Off).unwrap();
    let mut mem = Graph::memory();
    let dims = 8;
    let mut results: Vec<Vec<Vec<QueryResult>>> = vec![Vec::new(), Vec::new()];
    for (ti, g) in [&mut mem, &mut file].into_iter().enumerate() {
        q(g, "CREATE EXTENSION vector");
        let a = nodes(g, "A", 300);
        let b = nodes(g, "B", 200);
        let mut r = Rng(42);
        let mut all: Vec<(u64, Vec<f64>)> = Vec::new();
        for &id in a.iter().chain(&b) {
            // A coarse grid makes equal vectors, so ties must go to the
            // lower id; a few repeat an earlier vector exactly.
            let v: Vec<f64> = if id % 7 == 0 && !all.is_empty() {
                all[(id as usize * 31) % all.len()].1.clone()
            } else {
                let mut v: Vec<f64> = (0..dims).map(|_| r.coarse()).collect();
                if v.iter().all(|x| *x == 0.0) {
                    v[0] = 1.0;
                }
                v
            };
            all.push((id, v));
        }
        for (metric, normalized) in [
            ("cosine", false),
            ("cosine", true),
            ("dot", false),
            ("dot", true),
            ("l2", false),
            ("l2", true),
        ] {
            let name = format!("{metric}{}", if normalized { "_n" } else { "" });
            q(g, &format!("CALL vector.create(space: '{name}', dims: {dims}, metric: '{metric}', normalized: {normalized})"));
            for chunk in all.chunks(128) {
                let ids: Vec<u64> = chunk.iter().map(|(i, _)| *i).collect();
                let vs: Vec<Vec<f64>> = chunk.iter().map(|(_, v)| v.clone()).collect();
                put(g, &name, &ids, &vs);
            }
            let mut per = Vec::new();
            for k in 0..12 {
                let qv: Vec<f64> = (0..dims)
                    .map(|_| r.coarse() + if k == 0 { 0.0 } else { 0.01 })
                    .collect();
                let top = [1, 5, 10, 50, 600][k % 5];
                let got = qp(
                    g,
                    &format!("CALL vector.search(space: '{name}', vector: $q, top: {top})"),
                    &[("q", vlist(&qv))],
                );
                assert_eq!(
                    rows_of(&got),
                    brute(metric, normalized, &qv, &all, top, None),
                    "{name} k {k}"
                );
                assert!(got
                    .message
                    .as_ref()
                    .unwrap()
                    .contains("500 of 500 vectors scored"));
                // Restricted to a label, and with a floor on the score.
                let min = brute(metric, normalized, &qv, &all, 20, None)
                    .last()
                    .unwrap()
                    .1;
                let got_b = qp(g, &format!("CALL vector.search(space: '{name}', vector: $q, top: {top}, labels: ['B'], min_score: $m)"), &[("q", vlist(&qv)), ("m", Value::Float(min))]);
                let only_b: Vec<(u64, Vec<f64>)> = all
                    .iter()
                    .filter(|(id, _)| b.contains(id))
                    .cloned()
                    .collect();
                assert_eq!(
                    rows_of(&got_b),
                    brute(metric, normalized, &qv, &only_b, top, Some(min)),
                    "{name} labels k {k}"
                );
                per.push(got);
                per.push(got_b);
            }
            results[ti].push(per);
        }
    }
    // The same rows on both tiers (the messages name the same counts and
    // revisions too, since both graphs had the same history).
    for (m, f) in results[0].iter().zip(&results[1]) {
        for (a, b) in m.iter().zip(f) {
            assert_eq!(a.columns, b.columns);
            assert_eq!(a.rows, b.rows);
        }
    }
    assert_eq!(keyspace(&mem, vector::NS), keyspace(&file, vector::NS));
    drop(file);
    cleanup(&path);
}

#[test]
fn search_refuses_a_query_that_does_not_fit_the_space() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION vector");
    q(&mut g, "CALL vector.create(space: 's', dims: 3)");
    q(
        &mut g,
        "CALL vector.create(space: 'e', dims: 3, of: 'edge')",
    );
    let e = err(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 2])",
        &[],
    );
    assert!(e.contains("2 elements") && e.contains("3"), "{e}");
    let e = err(
        &mut g,
        "CALL vector.search(space: 's', vector: [0, 0, 0])",
        &[],
    );
    assert!(e.contains("all zeros"), "{e}");
    let e = err(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 'x', 0])",
        &[],
    );
    assert!(e.contains("numbers"), "{e}");
    let e = err(
        &mut g,
        "CALL vector.search(space: 'e', vector: [1, 1, 0], labels: ['A'])",
        &[],
    );
    assert!(e.contains("labels"), "{e}");
    let e = err(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 1, 0], fresh: 'maybe')",
        &[],
    );
    assert!(e.contains("report, require"), "{e}");
    // An empty space: no rows, said plainly.
    let r = q(&mut g, "CALL vector.search(space: 's', vector: [1, 1, 0])");
    assert!(r.rows.is_empty());
    assert!(r.message.unwrap().contains("0 of 0 vectors scored"));
}

#[test]
fn similar_searches_from_a_stored_vector_and_across_spaces_only_when_they_agree() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION vector");
    q(
        &mut g,
        "CALL vector.create(space: 'a', dims: 2, model: 'm', model_version: '1')",
    );
    q(
        &mut g,
        "CALL vector.create(space: 'b', dims: 2, model: 'm', model_version: '1', labels: ['N'])",
    );
    q(
        &mut g,
        "CALL vector.create(space: 'other_model', dims: 2, model: 'n', model_version: '1')",
    );
    q(
        &mut g,
        "CALL vector.create(space: 'other_version', dims: 2, model: 'm', model_version: '2')",
    );
    q(
        &mut g,
        "CALL vector.create(space: 'other_dims', dims: 3, model: 'm', model_version: '1')",
    );
    q(&mut g, "CALL vector.create(space: 'other_metric', dims: 2, metric: 'l2', model: 'm', model_version: '1')");
    q(&mut g, "CALL vector.create(space: 'other_algo', dims: 2, algorithm: 'x', model: 'm', model_version: '1')");
    let ids = nodes(&mut g, "N", 4);
    put(
        &mut g,
        "a",
        &ids,
        &[
            vec![1.0, 0.0],
            vec![0.9, 0.1],
            vec![0.0, 1.0],
            vec![1.0, 0.0],
        ],
    );
    put(&mut g, "b", &ids[..2], &[vec![0.0, 1.0], vec![1.0, 0.05]]);
    // From a stored vector, itself not listed; the tie goes to the lower id.
    let r = q(
        &mut g,
        &format!("CALL vector.similar(space: 'a', id: {}, top: 3)", ids[0]),
    );
    let got: Vec<i64> = r.rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
    assert_eq!(got, vec![ids[3] as i64, ids[1] as i64, ids[2] as i64]);
    assert_eq!(r.rows[0][1], Value::Float(1.0));
    // Into another space that agrees; its missing vectors are counted.
    let r = q(
        &mut g,
        &format!(
            "CALL vector.similar(space: 'a', id: {}, in: 'b', counts: true)",
            ids[2]
        ),
    );
    assert_eq!(r.rows[0][0], Value::Int(ids[0] as i64));
    assert_eq!(r.rows.len(), 2);
    assert!(r.message.unwrap().contains("missing 2"));
    for other in [
        "other_model",
        "other_version",
        "other_dims",
        "other_metric",
        "other_algo",
    ] {
        let e = err(
            &mut g,
            &format!(
                "CALL vector.similar(space: 'a', id: {}, in: '{other}')",
                ids[0]
            ),
            &[],
        );
        assert!(
            e.contains("cannot be compared") && e.contains("differ in"),
            "{other}: {e}"
        );
    }
    let e = err(
        &mut g,
        &format!("CALL vector.similar(space: 'b', id: {})", ids[3]),
        &[],
    );
    assert!(e.contains("has no vector"), "{e}");
}

#[test]
fn a_deleted_node_or_edge_takes_its_vectors_with_it_in_the_same_transaction() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION vector");
    q(&mut g, "CALL vector.create(space: 'n1', dims: 2)");
    q(
        &mut g,
        "CALL vector.create(space: 'n2', dims: 2, metric: 'l2')",
    );
    q(
        &mut g,
        "CALL vector.create(space: 'e', dims: 2, of: 'edge')",
    );
    q(
        &mut g,
        "CREATE (:A {i: 1})-[:K]->(:A {i: 2})-[:K]->(:A {i: 3})",
    );
    let ids = g.node_ids();
    let edges = g.edge_ids();
    put(
        &mut g,
        "n1",
        &ids,
        &[vec![1.0, 0.0], vec![0.0, 1.0], vec![1.0, 1.0]],
    );
    put(&mut g, "n2", &ids[..2], &[vec![1.0, 0.0], vec![0.0, 1.0]]);
    qp(
        &mut g,
        "CALL vector.put(space: 'e', ids: $ids, vectors: [[1, 0], [0, 1]])",
        &[("ids", ilist(&edges))],
    );
    let before = keyspace(&g, vector::NS);

    // A rolled-back delete leaves every vector where it was.
    q(&mut g, "BEGIN");
    q(&mut g, "MATCH (a:A {i: 2}) DETACH DELETE a");
    q(&mut g, "ROLLBACK");
    assert_eq!(keyspace(&g, vector::NS), before);

    // Deleting the middle node takes its vectors and its edges' vectors.
    let rev = g.revision();
    q(&mut g, "MATCH (a:A {i: 2}) DETACH DELETE a");
    assert!(g.revision() > rev);
    let n1 = vector::space(&g, "n1").unwrap();
    let n2 = vector::space(&g, "n2").unwrap();
    let e = vector::space(&g, "e").unwrap();
    assert!(vector::get(&g, &n1, &Target::Id(ids[1])).is_none());
    assert!(vector::get(&g, &n2, &Target::Id(ids[1])).is_none());
    assert!(vector::get(&g, &n1, &Target::Id(ids[0])).is_some());
    assert!(edges
        .iter()
        .all(|id| vector::get(&g, &e, &Target::Id(*id)).is_none()));
    assert_eq!(field(&space_row(&mut g, "n1"), "vectors"), Value::Int(2));
    assert_eq!(field(&space_row(&mut g, "n2"), "vectors"), Value::Int(1));
    assert_eq!(field(&space_row(&mut g, "e"), "vectors"), Value::Int(0));

    // The API and a replayed operation do the same.
    g.delete_node(ids[0]).unwrap();
    g.apply_op(Op::NodeDel { id: ids[2] }).unwrap();
    assert_eq!(field(&space_row(&mut g, "n1"), "vectors"), Value::Int(0));
    assert_eq!(field(&space_row(&mut g, "n1"), "bytes"), Value::Int(0));
    assert_eq!(check_findings(&g), Vec::<(String, String)>::new());
}

#[test]
fn clear_removes_the_vectors_and_keeps_the_spaces_which_then_hold_nothing() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION vector");
    q(
        &mut g,
        "CALL vector.create(space: 's', dims: 2, labels: ['N'])",
    );
    q(&mut g, "CALL vector.create(space: 'k', dims: 2, of: 'key')");
    let ids = nodes(&mut g, "N", 3);
    put(
        &mut g,
        "s",
        &ids,
        &[vec![1.0, 0.0], vec![0.0, 1.0], vec![1.0, 1.0]],
    );
    q(
        &mut g,
        "CALL vector.put(space: 'k', keys: ['all'], vectors: [[1, 2]])",
    );
    q(&mut g, "CLEAR");
    let at = g.revision();
    for name in ["s", "k"] {
        let row = space_row(&mut g, name);
        assert_eq!(field(&row, "vectors"), Value::Int(0));
        assert_eq!(field(&row, "bytes"), Value::Int(0));
        assert!(field(&row, "revision").as_i64().unwrap() as u64 <= at);
        assert!(field(&row, "revision").as_i64().unwrap() as u64 > ids[0]);
    }
    assert!(keyspace(&g, vector::NS)
        .iter()
        .all(|(k, _)| k[0] == b'!' || k[0] == b'c'));
    // Ids start again at 1 after CLEAR; the new node 1 has no old vector.
    let fresh = nodes(&mut g, "N", 1);
    assert_eq!(fresh[0], ids[0]);
    let r = q(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 0], counts: true)",
    );
    assert!(r.rows.is_empty());
    assert!(r.message.unwrap().contains("missing 1"));
    assert_eq!(check_findings(&g), Vec::<(String, String)>::new());
}

fn stale_all(g: &mut Graph, space: &str, limit: usize) -> (Vec<Vec<Value>>, usize) {
    let mut rows = Vec::new();
    let mut after: Option<String> = None;
    let mut calls = 0;
    loop {
        calls += 1;
        let src = match &after {
            None => format!("CALL vector.stale(space: '{space}', limit: {limit})"),
            Some(a) => format!("CALL vector.stale(space: '{space}', limit: {limit}, after: '{a}')"),
        };
        let r = q(g, &src);
        assert!(r.rows.len() <= limit);
        rows.extend(r.rows);
        let msg = r.message.unwrap();
        match msg.split("after: \"").nth(1) {
            Some(rest) => after = Some(rest.split('"').next().unwrap().to_string()),
            None => return (rows, calls),
        }
        assert!(calls < 1000);
    }
}

#[test]
fn staleness_is_reported_from_the_journal_and_never_hidden() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION vector");
    q(
        &mut g,
        "CALL vector.create(space: 's', dims: 2, labels: ['Doc', 'Note'])",
    );
    let docs = nodes(&mut g, "Doc", 6);
    let notes = nodes(&mut g, "Note", 2);
    // Without the journal the engine cannot know: every vector is unknown.
    put(&mut g, "s", &docs[..2], &[vec![1.0, 0.0], vec![0.0, 1.0]]);
    let (rows, _) = stale_all(&mut g, "s", 100);
    let states: Vec<String> = rows.iter().map(|r| r[1].to_string()).collect();
    assert_eq!(states.iter().filter(|s| *s == "unknown").count(), 2);
    assert_eq!(states.iter().filter(|s| *s == "missing").count(), 6);
    let r = q(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 0], counts: true)",
    );
    assert!(
        r.message
            .as_ref()
            .unwrap()
            .contains("stale 0, missing 6, unknown 2"),
        "{:?}",
        r.message
    );
    assert!(r.rows.iter().all(|row| row[2] == Value::from("unknown")));
    let e = err(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 0], fresh: 'require')",
        &[],
    );
    assert!(e.contains("not fresh") && e.contains("vector.stale"), "{e}");

    // With the journal, vectors put after it are fresh until their node
    // changes; the ones from before it stay unknown until put again.
    q(&mut g, "CREATE EXTENSION changes");
    put(
        &mut g,
        "s",
        &docs[2..],
        &[
            vec![1.0, 1.0],
            vec![1.0, 2.0],
            vec![2.0, 1.0],
            vec![3.0, 1.0],
        ],
    );
    put(&mut g, "s", &notes, &[vec![1.0, 0.5], vec![0.5, 1.0]]);
    let (rows, _) = stale_all(&mut g, "s", 100);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r[1] == Value::from("unknown")));
    put(&mut g, "s", &docs[..2], &[vec![1.0, 0.0], vec![0.0, 1.0]]);
    assert_eq!(stale_all(&mut g, "s", 100).0.len(), 0);
    q(&mut g, "CALL vector.complete(space: 's')");
    let r = q(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 0], fresh: 'require')",
    );
    assert!(r.message.unwrap().contains("space current: yes"));

    // A changed node makes its vector stale; an edge does not change the
    // node itself; a new node of the labels is missing.
    g.set_node_prop(docs[3], "text", Value::from("new"))
        .unwrap();
    q(
        &mut g,
        &format!(
            "MATCH (a), (b) WHERE id(a) = {} AND id(b) = {} CREATE (a)-[:K]->(b)",
            docs[4], docs[5]
        ),
    );
    let extra = nodes(&mut g, "Note", 1)[0];
    g.add_label(docs[5], "Note").unwrap();
    let (rows, _) = stale_all(&mut g, "s", 100);
    let got: Vec<(i64, String)> = rows
        .iter()
        .map(|r| (r[0].as_i64().unwrap(), r[1].to_string()))
        .collect();
    assert_eq!(
        got,
        vec![
            (docs[3] as i64, "stale".into()),
            (docs[5] as i64, "stale".into()),
            (extra as i64, "missing".into())
        ]
    );
    assert!(rows[0][3].to_string().contains("changed at revision"));
    let r = q(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 1], top: 10, counts: true)",
    );
    assert!(r
        .message
        .as_ref()
        .unwrap()
        .contains("stale 2, missing 1, unknown 0"));
    let st: Vec<(i64, String)> = r
        .rows
        .iter()
        .map(|r| (r[0].as_i64().unwrap(), r[2].to_string()))
        .collect();
    for (id, s) in &st {
        let want = if *id == docs[3] as i64 || *id == docs[5] as i64 {
            "stale"
        } else {
            "fresh"
        };
        assert_eq!(s, want, "{id}");
    }
    err(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 0], fresh: 'require')",
        &[],
    );
    let e = err(
        &mut g,
        &format!(
            "CALL vector.similar(space: 's', id: {}, fresh: 'require')",
            docs[3]
        ),
        &[],
    );
    assert!(e.contains("searched from is stale"), "{e}");

    // A change and a put in one transaction: a put after the change is
    // fresh, a change after the put makes it stale.
    q(&mut g, "BEGIN");
    g.set_node_prop(docs[0], "text", Value::from("v2")).unwrap();
    put(&mut g, "s", &docs[..2], &[vec![1.0, 0.0], vec![0.0, 1.0]]);
    g.set_node_prop(docs[1], "text", Value::from("v2")).unwrap();
    q(&mut g, "COMMIT");
    let (rows, _) = stale_all(&mut g, "s", 100);
    let ids: Vec<i64> = rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
    assert!(ids.contains(&(docs[1] as i64)) && !ids.contains(&(docs[0] as i64)));

    // In slices of any size the rows are the same as in one call.
    let (whole, _) = stale_all(&mut g, "s", 10_000);
    for limit in [1, 2, 3] {
        let (sliced, calls) = stale_all(&mut g, "s", limit);
        assert_eq!(sliced, whole, "limit {limit}");
        assert!(calls >= whole.len() / limit);
    }

    // The journal released past a vector's revision: unknown, not fresh.
    let now = g.revision();
    q(&mut g, &format!("CALL changes.release(below: {now})"));
    let (rows, _) = stale_all(&mut g, "s", 100);
    let unknown = rows
        .iter()
        .filter(|r| r[1] == Value::from("unknown"))
        .count();
    assert_eq!(
        unknown as i64,
        field(&space_row(&mut g, "s"), "vectors").as_i64().unwrap()
    );
}

#[test]
fn key_and_edge_spaces_say_what_the_journal_lets_them_say() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    q(&mut g, "CREATE EXTENSION vector");
    q(&mut g, "CALL vector.create(space: 'k', dims: 2, of: 'key')");
    q(
        &mut g,
        "CALL vector.create(space: 'e', dims: 2, of: 'edge')",
    );
    q(&mut g, "CREATE (:A)-[:K {w: 1}]->(:A)-[:K {w: 2}]->(:A)");
    let edges = g.edge_ids();
    q(
        &mut g,
        "CALL vector.put(space: 'k', keys: ['line-1', 'whole'], vectors: [[1, 0], [0, 1]])",
    );
    qp(
        &mut g,
        "CALL vector.put(space: 'e', ids: $ids, vectors: [[1, 0], [0, 1]])",
        &[("ids", ilist(&edges))],
    );
    assert!(stale_all(&mut g, "k", 10).0.is_empty());
    let r = q(&mut g, "CALL vector.search(space: 'k', vector: [1, 0.1])");
    assert_eq!(r.columns[0], "key");
    assert_eq!(r.rows[0][0], Value::from("line-1"));
    // An edge changes: its vector is stale; a key's vector cannot be told
    // apart from the change, so it is unknown, never fresh.
    g.set_edge_prop(edges[1], "w", Value::Int(3)).unwrap();
    let (rows, _) = stale_all(&mut g, "e", 10);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(edges[1] as i64));
    assert_eq!(rows[0][1], Value::from("stale"));
    let (rows, _) = stale_all(&mut g, "k", 10);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r[1] == Value::from("unknown")));
    let r = q(&mut g, "CALL vector.similar(space: 'k', key: 'whole')");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::from("line-1"));
}

/// A graph whose vectors were written further along than a second graph's
/// position: copied into it, they are stale, never fresh.
#[test]
fn a_vector_or_space_past_the_databases_position_is_stale() {
    let mut ahead = Graph::memory();
    q(&mut ahead, "CREATE EXTENSION changes");
    q(&mut ahead, "CREATE EXTENSION vector");
    let ids = nodes(&mut ahead, "N", 3);
    for i in 0..200 {
        ahead.set_node_prop(ids[0], "i", Value::Int(i)).unwrap();
    }
    q(&mut ahead, "CALL vector.create(space: 's', dims: 2)");
    put(
        &mut ahead,
        "s",
        &ids[1..],
        &[vec![1.0, 0.0], vec![0.0, 1.0]],
    );

    let mut behind = Graph::memory();
    q(&mut behind, "CREATE EXTENSION changes");
    q(&mut behind, "CREATE EXTENSION vector");
    nodes(&mut behind, "N", 3);
    assert!(behind.log_end() < ahead.log_end());
    let copy = keyspace(&ahead, vector::NS);
    behind.ext_write(vector::NS, copy, Vec::new()).unwrap();
    assert!(behind.log_end() < vector::space(&behind, "s").unwrap().created);
    let (rows, _) = stale_all(&mut behind, "s", 10);
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|r| r[1] == Value::from("stale") && r[3].to_string().contains("past this database")));
    let r = q(
        &mut behind,
        "CALL vector.search(space: 's', vector: [1, 0], counts: true)",
    );
    assert!(r.message.unwrap().contains("stale 2"));
    let found = check_findings(&behind);
    assert!(found.iter().any(|(k, _)| k == "vector.future"), "{found:?}");
}

fn some_vector_work(g: &mut Graph) {
    q(g, "CREATE EXTENSION changes");
    q(g, "CREATE EXTENSION vector");
    q(g, "CALL vector.create(space: 's', dims: 16, labels: ['N'])");
    let ids = nodes(g, "N", 60);
    let mut r = Rng(3);
    let vs: Vec<Vec<f64>> = (0..50)
        .map(|_| (0..16).map(|_| r.fine()).collect())
        .collect();
    put(g, "s", &ids[..50], &vs);
    for id in &ids[..5] {
        g.set_node_prop(*id, "changed", Value::Bool(true)).unwrap();
    }
    g.delete_node(ids[7]).unwrap();
}

fn answers(g: &mut Graph) -> (Vec<Vec<Value>>, Vec<Vec<Value>>, Option<String>) {
    let s = q(
        g,
        "CALL vector.search(space: 's', vector: [1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1], top: 20)",
    );
    let st = stale_all(g, "s", 7).0;
    (s.rows, st, s.message)
}

#[test]
fn an_image_a_replica_and_recovery_hold_the_same_vectors_and_answers() {
    // A file, checkpointed, loaded from its bytes.
    let path = temp("image");
    let mut f = Graph::open(&path, Sync::Always).unwrap();
    some_vector_work(&mut f);
    f.checkpoint().unwrap();
    let live = keyspace(&f, vector::NS);
    let want = answers(&mut f);
    let bytes = std::fs::read(&path).unwrap();
    let mut c = Graph::from_bytes(&bytes).unwrap();
    assert_eq!(c.revision(), f.revision());
    assert_eq!(keyspace(&c, vector::NS), live);
    assert_eq!(answers(&mut c), want);
    drop(f);
    cleanup(&path);

    // A memory graph: a snapshot image, and a replica of base and log.
    let mut g = Graph::memory_with_page_size(1024, u64::MAX);
    g.enable_log(glider::storage::log::DEFAULT_LOG_CAP).unwrap();
    q(&mut g, "CREATE (:Before)");
    let snap = g.snapshot().unwrap();
    let mut parts: Vec<Vec<u8>> = Vec::new();
    snap.write_image(1 << 20, &mut |_, b| {
        parts.push(b);
        Ok(())
    })
    .unwrap();
    let from = snap.lsn();
    drop(snap);
    some_vector_work(&mut g);
    let live = keyspace(&g, vector::NS);
    let want = answers(&mut g);
    let (_, end) = g.log_span().unwrap();
    let log = g.log_range(from, end).unwrap();
    let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
    let (mut m, _) = Graph::from_replica(&refs, &[(from, &log)], u64::MAX, 1 << 20).unwrap();
    assert_eq!(m.revision(), g.revision());
    assert_eq!(keyspace(&m, vector::NS), live);
    assert_eq!(answers(&mut m), want);
    let snap = g.snapshot().unwrap();
    let mut image: Vec<u8> = Vec::new();
    snap.write_image(1 << 20, &mut |_, b| {
        image.extend_from_slice(&b);
        Ok(())
    })
    .unwrap();
    drop(snap);
    let mut c = Graph::from_bytes(&image).unwrap();
    assert_eq!(keyspace(&c, vector::NS), live);
    assert_eq!(answers(&mut c), want);

    // Crash recovery: the log replayed at open, nothing checkpointed.
    let path = temp("recovery");
    {
        let mut f = Graph::open(&path, Sync::Always).unwrap();
        f.set_checkpoint_bytes(u64::MAX);
        q(&mut f, "CREATE (:Before)");
        some_vector_work(&mut f);
        assert_eq!(keyspace(&f, vector::NS), live);
        std::mem::forget(f);
    }
    let mut f = Graph::open_forced(&path, Sync::Always).unwrap();
    assert_eq!(f.revision(), g.revision());
    assert_eq!(keyspace(&f, vector::NS), live);
    assert_eq!(answers(&mut f), want);
    drop(f);
    cleanup(&path);
}

fn check_findings(g: &Graph) -> Vec<(String, String)> {
    let mut probe = Probe::whole(1 << 20);
    assert!(ext::find("vector").unwrap().check(g, &mut probe).unwrap());
    probe
        .findings()
        .iter()
        .map(|f| (format!("vector.{}", f.kind), f.id.clone()))
        .collect()
}

#[test]
fn check_run_finds_what_is_wrong_with_the_vectors_and_nothing_when_they_are_sound() {
    let mut g = Graph::memory();
    some_vector_work(&mut g);
    q(
        &mut g,
        "CALL vector.create(space: 'e', dims: 2, of: 'edge')",
    );
    assert_eq!(check_findings(&g), Vec::<(String, String)>::new());
    q(&mut g, "CREATE EXTENSION check");
    let r = q(&mut g, "CALL check.run(checks: ['derived'])");
    assert!(
        r.rows
            .iter()
            .all(|row| row[1] == Value::from("summary")
                || !row[2].to_string().starts_with("vector.")),
        "{:?}",
        r.rows
    );

    let s = vector::space(&g, "s").unwrap();
    let id = g.node_ids()[20];
    let key = |tag: u8, id: u64| {
        let mut k = vec![tag];
        k.extend_from_slice(&s.sid.to_be_bytes());
        k.extend_from_slice(&id.to_be_bytes());
        k
    };
    let mut nan = vec![0u8; 64];
    nan[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
    let future = (g.log_end() + 1_000_000).to_be_bytes().to_vec();
    let rev = g.log_end().to_be_bytes().to_vec();
    let ghost = 1_000_000u64;
    let cases: Vec<(&str, Vec<(Vec<u8>, Vec<u8>)>, Vec<Vec<u8>>)> = vec![
        (
            "vector.length",
            vec![(key(b'v', id), vec![0u8; 12])],
            vec![],
        ),
        ("vector.not_finite", vec![(key(b'v', id), nan)], vec![]),
        ("vector.future", vec![(key(b'm', id), future)], vec![]),
        (
            "vector.orphan",
            vec![
                (key(b'm', ghost), rev.clone()),
                (key(b'v', ghost), vec![0u8; 64]),
            ],
            vec![],
        ),
        ("vector.unpaired", vec![], vec![key(b'm', id)]),
        (
            "vector.count",
            vec![
                (key(b'm', ghost), rev.clone()),
                (key(b'v', ghost), vec![0u8; 64]),
            ],
            vec![],
        ),
        (
            "vector.bad_record",
            vec![(b"zzz".to_vec(), vec![1])],
            vec![],
        ),
    ];
    for (want, puts, dels) in cases {
        let before = keyspace(&g, vector::NS);
        g.ext_write(vector::NS, puts, dels).unwrap();
        let found = check_findings(&g);
        assert!(found.iter().any(|(k, _)| k == want), "{want}: {found:?}");
        // Put it back as it was.
        let after = keyspace(&g, vector::NS);
        let dels: Vec<Vec<u8>> = after
            .iter()
            .map(|(k, _)| k.clone())
            .filter(|k| !before.iter().any(|(b, _)| b == k))
            .collect();
        g.ext_write(vector::NS, before, dels).unwrap();
        assert_eq!(
            check_findings(&g),
            Vec::<(String, String)>::new(),
            "after {want}"
        );
    }
    // The statement reports them under the derived check.
    g.ext_write(vector::NS, vec![(key(b'v', id), vec![0u8; 12])], vec![])
        .unwrap();
    let r = q(&mut g, "CALL check.run(checks: ['derived'])");
    assert!(
        r.rows
            .iter()
            .any(|row| row[2] == Value::from("vector.length")),
        "{:?}",
        r.rows
    );
    // A search over such a record is an error that says what to do.
    let e = err(
        &mut g,
        "CALL vector.search(space: 's', vector: [1,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1])",
        &[],
    );
    assert!(e.contains("wrong length") && e.contains("check.run"), "{e}");
}

#[test]
fn a_database_without_vectors_pays_nothing_and_spaces_follow_writes_only_when_they_exist() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION vector");
    let ext = ext::find("vector").unwrap();
    assert!(!ext.wants_writes(&g) && !ext.wants_edge_writes(&g));
    q(&mut g, "CALL vector.create(space: 'n', dims: 2)");
    assert!(ext.wants_writes(&g) && !ext.wants_edge_writes(&g));
    q(
        &mut g,
        "CALL vector.create(space: 'e', dims: 2, of: 'edge')",
    );
    assert!(ext.wants_edge_writes(&g));
    q(&mut g, "CALL vector.drop(space: 'n')");
    q(&mut g, "CALL vector.drop(space: 'e')");
    assert!(!ext.wants_writes(&g));
    // A search reads nothing but a space's own vectors: a judge without a
    // journal says unknown for any of them.
    let s = vector::space(&g, "n");
    assert!(s.is_none());
    q(&mut g, "CALL vector.create(space: 'n', dims: 2)");
    let s = vector::space(&g, "n").unwrap();
    let id = nodes(&mut g, "N", 1)[0];
    put(&mut g, "n", &[id], &[vec![1.0, 0.0]]);
    let (_, m) = vector::get(&g, &s, &Target::Id(id)).unwrap();
    let j = vector::Judge::new(&g, &s);
    assert_eq!(j.judge(&g, &id.to_be_bytes(), m.revision).0, Fresh::Unknown);
}

fn current_of(g: &mut Graph, space: &str) -> String {
    field(&space_row(g, space), "current").to_string()
}

fn search_msg(g: &mut Graph, space: &str, extra: &str) -> String {
    q(
        g,
        &format!("CALL vector.search(space: '{space}', vector: [1, 0]{extra})"),
    )
    .message
    .unwrap()
}

#[test]
fn a_space_of_surroundings_is_stale_when_an_edge_at_its_node_changes() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    q(&mut g, "CREATE EXTENSION vector");
    q(&mut g, "CALL vector.create(space: 'text', dims: 2)");
    q(
        &mut g,
        "CALL vector.create(space: 'shape', dims: 2, follows: 'edges')",
    );
    let row = space_row(&mut g, "shape");
    assert_eq!(field(&row, "follows"), Value::from("edges"));
    assert_eq!(
        field(&space_row(&mut g, "text"), "follows"),
        Value::from("self")
    );
    for bad in [
        "CALL vector.create(space: 'x', dims: 2, of: 'edge', follows: 'edges')",
        "CALL vector.create(space: 'x', dims: 2, of: 'key', follows: 'edges')",
        "CALL vector.create(space: 'x', dims: 2, follows: 'neighbours')",
    ] {
        err(&mut g, bad, &[]);
    }
    // The same name with another `follows` is another definition.
    let e = err(
        &mut g,
        "CALL vector.create(space: 'text', dims: 2, follows: 'edges')",
        &[],
    );
    assert!(e.contains("incomparable"), "{e}");
    q(&mut g, "CREATE (:N {i: 0})-[:K {w: 1}]->(:N {i: 1})-[:K {w: 1}]->(:N {i: 2}), (:N {i: 3}), (:N {i: 4})");
    let ids = g.node_ids();
    let edges = g.edge_ids();
    let vs: Vec<Vec<f64>> = (0..5).map(|i| vec![1.0, i as f64]).collect();
    put(&mut g, "text", &ids, &vs);
    put(&mut g, "shape", &ids, &vs);
    assert!(stale_all(&mut g, "shape", 10).0.is_empty());
    // An edge added between 3 and 4, one changed (0-1), one removed (1-2).
    q(
        &mut g,
        "MATCH (a:N {i: 3}), (b:N {i: 4}) CREATE (a)-[:K]->(b)",
    );
    g.set_edge_prop(edges[0], "w", Value::Int(2)).unwrap();
    g.delete_edge(edges[1]).unwrap();
    let stale = |g: &mut Graph, s: &str| -> Vec<i64> {
        stale_all(g, s, 10)
            .0
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect()
    };
    assert_eq!(stale(&mut g, "text"), Vec::<i64>::new());
    assert_eq!(
        stale(&mut g, "shape"),
        ids.iter().map(|i| *i as i64).collect::<Vec<_>>()
    );
    let why = stale_all(&mut g, "shape", 10).0[0][3].to_string();
    assert!(why.contains("adjacency"), "{why}");
    // Its own properties make both stale.
    put(&mut g, "shape", &ids, &vs);
    g.set_node_prop(ids[2], "i", Value::Int(20)).unwrap();
    assert_eq!(stale(&mut g, "text"), vec![ids[2] as i64]);
    assert_eq!(stale(&mut g, "shape"), vec![ids[2] as i64]);
}

#[test]
fn a_search_says_cheaply_whether_its_space_is_current_and_never_claims_what_it_has_not_established()
{
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    q(&mut g, "CREATE EXTENSION vector");
    q(
        &mut g,
        "CALL vector.create(space: 's', dims: 2, labels: ['Doc'])",
    );
    let docs = nodes(&mut g, "Doc", 4);
    put(
        &mut g,
        "s",
        &docs,
        &[
            vec![1.0, 0.0],
            vec![0.0, 1.0],
            vec![1.0, 1.0],
            vec![2.0, 1.0],
        ],
    );
    // Everything is fresh, but nothing has counted it: unknown, not yes.
    let m = search_msg(&mut g, "s", "");
    assert!(
        m.contains("space current: unknown") && !m.contains("counted"),
        "{m}"
    );
    assert_eq!(current_of(&mut g, "s"), "unknown");
    // Counting reports it and records nothing; `complete` records it.
    let m = search_msg(&mut g, "s", ", counts: true");
    assert!(m.contains("counted: stale 0, missing 0, unknown 0"), "{m}");
    assert!(m.contains("space current: unknown"), "{m}");
    let r = q(&mut g, "CALL vector.complete(space: 's')");
    assert_eq!(r.rows[0][1], Value::Bool(true));
    let m = search_msg(&mut g, "s", "");
    assert!(
        m.contains("space current: yes, established complete at revision"),
        "{m}"
    );
    let since = field(&space_row(&mut g, "s"), "current_since")
        .as_i64()
        .unwrap();
    // `require` then needs no count.
    let m = search_msg(&mut g, "s", ", fresh: 'require'");
    assert!(
        m.contains("space current: yes") && !m.contains("counted"),
        "{m}"
    );
    assert!(m.contains("established from the record"), "{m}");
    // A write that touches nothing of it: still current, from the journal.
    let other = nodes(&mut g, "Other", 1)[0];
    g.set_node_prop(other, "x", Value::Int(1)).unwrap();
    assert!(search_msg(&mut g, "s", "").contains("space current: yes"));
    // A new vector is fresh: still current, and still since the same point.
    let extra = nodes(&mut g, "Doc", 1)[0];
    let m = search_msg(&mut g, "s", "");
    assert!(
        m.contains("space current: no") && m.contains(&format!("node {extra} has a label")),
        "{m}"
    );
    put(&mut g, "s", &[extra], &[vec![3.0, 1.0]]);
    assert!(search_msg(&mut g, "s", "").contains("space current: yes"));
    assert_eq!(
        field(&space_row(&mut g, "s"), "current_since")
            .as_i64()
            .unwrap(),
        since
    );
    // A change to one of its nodes: no, naming it; put again: yes.
    g.set_node_prop(docs[1], "text", Value::from("new"))
        .unwrap();
    let m = search_msg(&mut g, "s", "");
    assert!(
        m.contains("space current: no") && m.contains(&format!("node {}", docs[1])),
        "{m}"
    );
    assert_eq!(current_of(&mut g, "s"), "no");
    err(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 0], fresh: 'require')",
        &[],
    );
    put(&mut g, "s", &docs[1..2], &[vec![0.0, 1.0]]);
    assert!(search_msg(&mut g, "s", "").contains("space current: yes"));
    // A vector deleted: no longer known complete.
    q(
        &mut g,
        &format!("CALL vector.delete(space: 's', ids: [{extra}])"),
    );
    assert!(search_msg(&mut g, "s", "").contains("space current: unknown"));
    let e = err(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 0], fresh: 'require')",
        &[],
    );
    assert!(e.contains("missing 1"), "{e}");
    put(&mut g, "s", &[extra], &[vec![3.0, 1.0]]);
    // `require` counts when it must, says so, and records nothing.
    let m = search_msg(&mut g, "s", ", fresh: 'require'");
    assert!(m.contains("counted: stale 0, missing 0, unknown 0"), "{m}");
    assert!(m.contains("CALL vector.complete"), "{m}");
    assert!(search_msg(&mut g, "s", "").contains("space current: unknown"));
    q(&mut g, "CALL vector.complete(space: 's')");
    assert!(search_msg(&mut g, "s", "").contains("space current: yes"));
    // spaces() counts on request.
    q(
        &mut g,
        &format!("MATCH (n) WHERE id(n) = {} SET n.text = 'x'", docs[0]),
    );
    let r = q(&mut g, "CALL vector.spaces(counts: true)");
    let row: Vec<(String, Value)> = r.columns.into_iter().zip(r.rows[0].clone()).collect();
    assert_eq!(field(&row, "stale"), Value::Int(1));
    assert_eq!(field(&row, "missing"), Value::Int(0));
    assert_eq!(field(&row, "current"), Value::from("no"));
    assert_eq!(field(&space_row(&mut g, "s"), "stale"), Value::Null);
}

#[test]
fn the_record_that_a_space_is_current_is_not_trusted_where_it_must_not_be() {
    let setup = |g: &mut Graph| {
        q(g, "CREATE EXTENSION changes");
        q(g, "CREATE EXTENSION vector");
        q(
            g,
            "CALL vector.create(space: 's', dims: 2, labels: ['Doc'])",
        );
        let docs = nodes(g, "Doc", 3);
        put(
            g,
            "s",
            &docs,
            &[vec![1.0, 0.0], vec![0.0, 1.0], vec![1.0, 1.0]],
        );
        q(g, "CALL vector.complete(space: 's')");
        assert_eq!(current_of(g, "s"), "yes");
        docs
    };
    // A floor raised past it: the journal cannot vouch any more.
    let mut g = Graph::memory();
    setup(&mut g);
    nodes(&mut g, "Other", 1);
    let now = g.revision();
    q(&mut g, &format!("CALL changes.release(below: {now})"));
    assert_eq!(current_of(&mut g, "s"), "unknown");

    // The journal dropped, a change made, the journal installed again.
    let mut g = Graph::memory();
    let docs = setup(&mut g);
    q(&mut g, "DROP EXTENSION changes");
    g.set_node_prop(docs[0], "text", Value::from("unseen"))
        .unwrap();
    q(&mut g, "CREATE EXTENSION changes");
    let m = search_msg(&mut g, "s", "");
    assert!(m.contains("space current: unknown"), "{m}");
    err(
        &mut g,
        "CALL vector.search(space: 's', vector: [1, 0], fresh: 'require')",
        &[],
    );

    // CLEAR: the record goes with the vectors.
    let mut g = Graph::memory();
    setup(&mut g);
    q(&mut g, "CLEAR");
    assert_eq!(current_of(&mut g, "s"), "unknown");

    // A put that fails inside a block: the block rolls back, record and all.
    let mut g = Graph::memory();
    let docs = setup(&mut g);
    q(
        &mut g,
        &format!("CALL vector.delete(space: 's', ids: [{}])", docs[2]),
    );
    assert_eq!(current_of(&mut g, "s"), "unknown");
    q(&mut g, "BEGIN");
    put(&mut g, "s", &docs[2..], &[vec![1.0, 1.0]]);
    q(&mut g, "CALL vector.complete(space: 's')");
    assert_eq!(current_of(&mut g, "s"), "yes");
    err(
        &mut g,
        &format!(
            "CALL vector.put(space: 's', ids: [{}], vectors: [[1, 2, 3]])",
            docs[0]
        ),
        &[],
    );
    let _ = query::execute(&mut g, "ROLLBACK");
    assert_eq!(current_of(&mut g, "s"), "unknown");
    // ... and one that fails on its own leaves what was established.
    put(&mut g, "s", &docs[2..], &[vec![1.0, 1.0]]);
    q(&mut g, "CALL vector.complete(space: 's')");
    err(
        &mut g,
        &format!(
            "CALL vector.put(space: 's', ids: [{}, 999], vectors: [[1, 2], [1, 2]])",
            docs[0]
        ),
        &[],
    );
    assert_eq!(current_of(&mut g, "s"), "yes");

    // Records copied from a graph further along: past this position.
    let mut ahead = Graph::memory();
    setup(&mut ahead);
    let mut behind = Graph::memory();
    q(&mut behind, "CREATE EXTENSION changes");
    q(&mut behind, "CREATE EXTENSION vector");
    nodes(&mut behind, "Doc", 3);
    behind
        .ext_write(vector::NS, keyspace(&ahead, vector::NS), Vec::new())
        .unwrap();
    assert!(behind.log_end() < ahead.log_end());
    assert_eq!(current_of(&mut behind, "s"), "unknown");

    // An image and a replica answer as their source: yes, then no after a
    // change on both.
    let path = temp("current");
    let mut f = Graph::open(&path, Sync::Always).unwrap();
    let docs = setup(&mut f);
    f.checkpoint().unwrap();
    let mut c = Graph::from_bytes(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(current_of(&mut c, "s"), current_of(&mut f, "s"));
    g = Graph::memory_with_page_size(1024, u64::MAX);
    g.enable_log(glider::storage::log::DEFAULT_LOG_CAP).unwrap();
    q(&mut g, "CREATE (:Before)");
    let snap = g.snapshot().unwrap();
    let mut parts: Vec<Vec<u8>> = Vec::new();
    snap.write_image(1 << 20, &mut |_, b| {
        parts.push(b);
        Ok(())
    })
    .unwrap();
    let from = snap.lsn();
    drop(snap);
    let gd = setup(&mut g);
    g.set_node_prop(gd[1], "text", Value::from("changed"))
        .unwrap();
    let (_, end) = g.log_span().unwrap();
    let log = g.log_range(from, end).unwrap();
    let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
    let (mut m, _) = Graph::from_replica(&refs, &[(from, &log)], u64::MAX, 1 << 20).unwrap();
    assert_eq!(current_of(&mut g, "s"), "no");
    assert_eq!(current_of(&mut m, "s"), "no");
    c.set_node_prop(docs[0], "text", Value::from("copy"))
        .unwrap();
    assert_eq!(current_of(&mut c, "s"), "no");
    drop(f);
    cleanup(&path);
}

#[test]
fn no_question_writes_whatever_state_the_space_is_in() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    q(&mut g, "CREATE EXTENSION vector");
    q(
        &mut g,
        "CALL vector.create(space: 's', dims: 2, labels: ['Doc'])",
    );
    q(&mut g, "CALL vector.create(space: 't', dims: 2)");
    let docs = nodes(&mut g, "Doc", 3);
    let vs = [vec![1.0, 0.0], vec![0.0, 1.0], vec![1.0, 1.0]];
    put(&mut g, "s", &docs, &vs);
    put(&mut g, "t", &docs, &vs);
    let questions = [
        "CALL vector.search(space: 's', vector: [1, 0])".to_string(),
        "CALL vector.search(space: 's', vector: [1, 0], fresh: 'report')".to_string(),
        "CALL vector.search(space: 's', vector: [1, 0], counts: true)".to_string(),
        "CALL vector.search(space: 's', vector: [1, 0], fresh: 'require')".to_string(),
        format!("CALL vector.similar(space: 's', id: {}, in: 't')", docs[0]),
        format!(
            "CALL vector.similar(space: 's', id: {}, counts: true)",
            docs[0]
        ),
        format!(
            "CALL vector.similar(space: 's', id: {}, fresh: 'require')",
            docs[0]
        ),
        "CALL vector.spaces()".to_string(),
        "CALL vector.spaces(counts: true)".to_string(),
        "CALL vector.stale(space: 's')".to_string(),
        "CALL vector.stale(space: 's', limit: 1)".to_string(),
        format!("CALL vector.get(space: 's', ids: [{}])", docs[0]),
        format!("CALL vector.hashes(space: 's', ids: [{}])", docs[0]),
    ];
    let ask_all = |g: &mut Graph, state: &str| {
        assert_eq!(current_of(g, "s"), state);
        for src in &questions {
            let (ks, rev, end) = (keyspace(g, vector::NS), g.revision(), g.log_end());
            // A refused `require` is still a question.
            let _ = query::execute(g, src);
            assert_eq!(keyspace(g, vector::NS), ks, "{state}: {src} wrote");
            assert_eq!(
                (g.revision(), g.log_end()),
                (rev, end),
                "{state}: {src} moved the revision"
            );
        }
        assert_eq!(current_of(g, "s"), state);
    };
    // Fresh but never recorded, then current, then changed since.
    ask_all(&mut g, "unknown");
    q(&mut g, "CALL vector.complete(space: 's')");
    ask_all(&mut g, "yes");
    g.set_node_prop(docs[1], "text", Value::from("changed"))
        .unwrap();
    ask_all(&mut g, "no");
    // `complete` on a space that is not complete records nothing.
    let before = keyspace(&g, vector::NS);
    let rev = g.revision();
    let r = q(&mut g, "CALL vector.complete(space: 's')");
    assert_eq!(r.rows[0][1], Value::Bool(false));
    assert_eq!(r.rows[0][3], Value::Int(1));
    assert!(r.message.unwrap().contains("nothing recorded"));
    assert_eq!(keyspace(&g, vector::NS), before);
    assert_eq!(g.revision(), rev);
    // Puts and deletes never record completeness on their own.
    put(&mut g, "s", &docs[1..2], &vs[1..2]);
    assert_eq!(current_of(&mut g, "s"), "yes");
    q(
        &mut g,
        &format!("CALL vector.delete(space: 's', ids: [{}])", docs[2]),
    );
    put(&mut g, "s", &docs[2..], &vs[2..]);
    assert_eq!(current_of(&mut g, "s"), "unknown");
    ask_all(&mut g, "unknown");
}
