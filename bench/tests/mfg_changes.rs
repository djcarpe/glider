//! The change journal over a manufacturing graph: after a mix of writes,
//! `changes.since` lists exactly the nodes, edges and adjacencies whose
//! exported state differs, and `check.run` finds nothing wrong with it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufReader;

use glider::ext::changes::{self, Kind};
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
            faults: true,
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

type State = BTreeMap<Kind, BTreeMap<u64, String>>;

fn state(g: &Graph) -> State {
    let mut s: State = BTreeMap::new();
    for id in g.node_ids() {
        let mut l = g.node_labels(id);
        l.sort();
        let mut p = g.node_props(id);
        p.sort_by(|a, b| a.0.cmp(&b.0));
        s.entry(Kind::Node)
            .or_default()
            .insert(id, format!("{l:?} {p:?}"));
    }
    let mut at: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for id in g.edge_ids() {
        let e = g.edge(id).unwrap();
        let mut p = g.edge_props(id);
        p.sort_by(|a, b| a.0.cmp(&b.0));
        let d = format!(
            "{id} {}-{}->{} {p:?}",
            e.from,
            g.edge_type_name(id).unwrap(),
            e.to
        );
        at.entry(e.from).or_default().push(d.clone());
        at.entry(e.to).or_default().push(d.clone());
        s.entry(Kind::Edge).or_default().insert(id, d);
    }
    s.insert(
        Kind::Adjacency,
        at.into_iter().map(|(n, v)| (n, v.join("|"))).collect(),
    );
    s
}

fn q(g: &mut Graph, src: &str) {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"));
}

#[test]
fn since_lists_exactly_what_a_mix_of_writes_changed_in_a_generated_graph() {
    let mut g = gen(7, 5_000);
    q(&mut g, "CREATE EXTENSION changes");
    let before = state(&g);
    let rev = g.revision();
    q(
        &mut g,
        "MATCH (w:WorkInstruction) WHERE w.revision = 'B' SET w.reviewed = true",
    );
    let of = |g: &Graph, label: &str| -> Vec<u64> {
        g.node_ids()
            .into_iter()
            .filter(|&n| g.node_labels(n).iter().any(|l| l == label))
            .collect()
    };
    // A whole station goes, with its edges.
    let station = of(&g, "Station")[0];
    g.delete_node(station).unwrap();
    let tools = of(&g, "Tool");
    for &t in &tools[..3] {
        g.add_label(t, "Retired").unwrap();
    }
    let elements = of(&g, "WorkElement");
    for (i, &w) in elements[..5].iter().enumerate() {
        g.add_edge(w, tools[i], "ALSO_USES", vec![]).unwrap();
    }
    let uses: Vec<u64> = g
        .edge_ids()
        .into_iter()
        .filter(|&e| g.edge_type_name(e) == Some("USES_TOOL"))
        .take(7)
        .collect();
    for e in uses {
        g.delete_edge(e).unwrap();
    }
    g.set_node_prop(1, "note", Value::Text("changed".into()))
        .unwrap();
    let after = state(&g);
    let (list, more) = changes::since(&g, rev, &[], None, usize::MAX).unwrap();
    assert!(!more);
    let mut listed: BTreeMap<Kind, BTreeSet<u64>> = BTreeMap::new();
    for e in &list {
        listed.entry(e.kind).or_default().insert(e.id);
    }
    let empty = BTreeMap::new();
    for kind in Kind::ALL {
        let (a, z) = (
            before.get(&kind).unwrap_or(&empty),
            after.get(&kind).unwrap_or(&empty),
        );
        let differ: BTreeSet<u64> = a
            .keys()
            .chain(z.keys())
            .filter(|id| a.get(id) != z.get(id))
            .copied()
            .collect();
        assert!(!differ.is_empty(), "{kind:?}: the writes changed nothing");
        // Exactly: these writes change every id they touch (no value is
        // set to what it was), and a deleted station's edges are all gone.
        assert_eq!(
            listed.get(&kind).cloned().unwrap_or_default(),
            differ,
            "{kind:?}"
        );
    }
    q(&mut g, "CREATE EXTENSION check");
    let r = query::execute(&mut g, "CALL check.run(checks: ['derived'])").unwrap();
    let faults: Vec<_> = r
        .rows
        .iter()
        .filter(|row| row[1] == Value::from("fault"))
        .collect();
    assert!(faults.is_empty(), "{faults:?}");
}
