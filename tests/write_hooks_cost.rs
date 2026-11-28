//! What edge write hooks cost a database that does not ask for them: edge
//! insertion and deletion throughput, with no extension installed and with
//! one installed that follows node writes only. Run with
//! `cargo test --release --test write_hooks_cost -- --ignored --nocapture`.

use std::time::Instant;

use glider::graph::Graph;
use glider::query;

const NODES: u64 = 20_000;
const EDGES: u64 = 400_000;
const ROUNDS: usize = 7;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn round(g: &mut Graph) -> (f64, f64) {
    g.autocommit = false;
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut ids = Vec::with_capacity(EDGES as usize);
    let t = Instant::now();
    for i in 0..EDGES {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let a = 1 + x % NODES;
        let b = 1 + (x >> 32) % NODES;
        ids.push(g.add_edge(a, b, "LINK", vec![]).unwrap());
        if i % 10_000 == 9_999 {
            g.commit().unwrap();
        }
    }
    g.commit().unwrap();
    let add = EDGES as f64 / t.elapsed().as_secs_f64();
    let t = Instant::now();
    for (i, id) in ids.into_iter().enumerate() {
        g.delete_edge(id).unwrap();
        if i % 10_000 == 9_999 {
            g.commit().unwrap();
        }
    }
    g.commit().unwrap();
    let del = EDGES as f64 / t.elapsed().as_secs_f64();
    g.autocommit = true;
    (add, del)
}

fn measure(name: &str, g: &mut Graph) {
    let (mut adds, mut dels) = (Vec::new(), Vec::new());
    round(g); // warm up
    for _ in 0..ROUNDS {
        let (a, d) = round(g);
        adds.push(a);
        dels.push(d);
    }
    println!(
        "{name}: add {:.0} edges/s, delete {:.0} edges/s (median of {ROUNDS})",
        median(adds),
        median(dels)
    );
}

fn graph() -> Graph {
    let mut g = Graph::memory();
    g.autocommit = false;
    for i in 0..NODES {
        g.add_node(&["N".into()], vec![("name".into(), format!("n{i}").into())])
            .unwrap();
    }
    g.commit().unwrap();
    g.autocommit = true;
    g
}

#[test]
#[ignore]
fn edge_write_throughput_without_edge_hooks() {
    let mut g = graph();
    measure("no extension", &mut g);
    let mut g = graph();
    query::execute(&mut g, "CREATE EXTENSION context").unwrap();
    query::execute(&mut g, "CALL context.index()").unwrap();
    measure("context with a text index (node hooks only)", &mut g);
}
