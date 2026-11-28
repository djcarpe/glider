//! Review of G5 (adopted), second pass: the cap under writes that make far more
//! entries than a sweep step looks at, checked after every single write.

use glider::ext::changes;
use glider::graph::Graph;
use glider::query;
use glider::value::Value;

fn q(g: &mut Graph, s: &str) {
    query::execute(g, s).unwrap_or_else(|e| panic!("{s}: {e}"));
}

#[test]
fn the_cap_holds_after_every_write_of_a_hub_built_and_deleted_in_one_transaction() {
    for cap in [64u64, 65, 100, 1000] {
        let mut g = Graph::memory();
        q(&mut g, "CREATE EXTENSION changes");
        q(&mut g, &format!("CALL changes.cap(entries: {cap})"));
        for _ in 0..3000 {
            g.add_node(&["N".into()], vec![]).unwrap();
        }
        let mut most = 0;
        g.begin().unwrap();
        let h = g.add_node(&["Hub".into()], vec![]).unwrap();
        let ids = g.node_ids();
        for (i, &b) in ids.iter().enumerate() {
            g.add_edge(h, b, "E", vec![]).unwrap();
            g.set_node_prop(b, "x", Value::Int(i as i64)).unwrap();
            let n = changes::status(&g).unwrap().entries;
            most = most.max(n);
            assert!(n <= cap, "cap {cap}: {n} entries after write {i}");
        }
        g.commit().unwrap();
        q(&mut g, "MATCH (h:Hub) DETACH DELETE h");
        let n = changes::status(&g).unwrap().entries;
        q(&mut g, "MATCH (n:N) SET n.y = 1");
        let m = changes::status(&g).unwrap().entries;
        eprintln!("cap {cap}: at most {most} during the hub; {n} after its delete; {m} after a 3,000-node SET");
        assert!(n <= cap && m <= cap);
    }
}
