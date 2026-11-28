//! The manufacturing generator: deterministic, and its truth is true.
//!
//! Each test generates a small graph in memory, imports it into an empty
//! glider database and checks a claim of the truth or question file
//! against what the database holds.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufReader;

use glider::graph::Graph;
use glider::value::{parse_json, Json, Value};
use glider::Dir;
use glider_bench::mfg::{generate, Config, Summary, MEMBER_CAP};

struct Gen {
    graph: String,
    truth: Vec<Json>,
    questions: Vec<Json>,
    summary: Summary,
}

fn gen(seed: u64, nodes: u64, faults: bool) -> Gen {
    let (mut g, mut t, mut q) = (Vec::new(), Vec::new(), Vec::new());
    let summary = generate(
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
    let lines = |b: Vec<u8>| -> Vec<Json> {
        String::from_utf8(b)
            .unwrap()
            .lines()
            .map(|l| parse_json(l).unwrap())
            .collect()
    };
    Gen {
        graph: String::from_utf8(g).unwrap(),
        truth: lines(t),
        questions: lines(q),
        summary,
    }
}

fn load(text: &str) -> Graph {
    let mut g = Graph::memory();
    let mut r = BufReader::new(text.as_bytes());
    glider::query::import_jsonl_from(&mut g, &mut r, 1 << 20).unwrap();
    g
}

fn kind<'a>(truth: &'a [Json], k: &str) -> Vec<&'a Json> {
    truth
        .iter()
        .filter(|r| r.get("kind").and_then(|v| v.as_str()) == Some(k))
        .collect()
}

fn u(j: &Json, k: &str) -> u64 {
    j.get(k)
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("{k} missing in {j:?}"))
}

fn ids(j: &Json, k: &str) -> Vec<u64> {
    j.get(k)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|x| x.as_u64().unwrap()).collect())
        .unwrap_or_default()
}

fn strs(j: &Json, k: &str) -> Vec<String> {
    j.get(k)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

fn prop(g: &Graph, id: u64, key: &str) -> Value {
    g.node_props(id)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
        .unwrap_or(Value::Null)
}

fn text(g: &Graph, id: u64, key: &str) -> String {
    match prop(g, id, key) {
        Value::Text(s) => s,
        v => panic!("node {id}: {key} is {v:?}"),
    }
}

/// Out-neighbours over edges of one type, in adjacency order.
fn out(g: &Graph, id: u64, ty: &str) -> Vec<u64> {
    g.neighbors(id, Dir::Out, None)
        .into_iter()
        .filter(|a| g.edge_type_name(a.edge) == Some(ty))
        .map(|a| a.other)
        .collect()
}

fn out_edges(g: &Graph, id: u64, ty: &str) -> Vec<(u64, u64)> {
    g.neighbors(id, Dir::Out, None)
        .into_iter()
        .filter(|a| g.edge_type_name(a.edge) == Some(ty))
        .map(|a| (a.edge, a.other))
        .collect()
}

fn edge_text(g: &Graph, e: u64, key: &str) -> Option<String> {
    g.edge_props(e)
        .into_iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| match v {
            Value::Text(s) => Some(s),
            _ => None,
        })
}

fn elements(g: &Graph, station: u64) -> Vec<u64> {
    let mut v = Vec::new();
    for side in out(g, station, "HAS_SIDE") {
        for loc in out(g, side, "HAS_LOCATION") {
            v.extend(out(g, loc, "HAS_ELEMENT"));
        }
    }
    v
}

// ------------------------------------------------------------ determinism

#[test]
fn the_same_seed_and_size_give_the_same_bytes() {
    let run = |seed, nodes, faults| {
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
        (g, t, q)
    };
    for (seed, nodes, faults) in [(1, 2_000, false), (7, 5_000, true), (3, 300, false)] {
        let a = run(seed, nodes, faults);
        let b = run(seed, nodes, faults);
        assert!(a == b, "seed {seed}, {nodes} nodes: two runs differ");
    }
    // A known checksum, so that a change on another machine or toolchain
    // shows up as a failure here rather than as a silently different graph.
    let (g, t, q) = run(1, 2_000, false);
    let fnv = |b: &[u8]| {
        b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| {
            (h ^ x as u64).wrapping_mul(0x100_0000_01b3)
        })
    };
    let sums = (fnv(&g), fnv(&t), fnv(&q));
    assert_eq!(
        sums, EXPECTED_SUMS,
        "the output for seed 1 at 2000 nodes changed"
    );
}

/// FNV-1a of graph, truth and questions for seed 1 at 2000 nodes. A change
/// to the generator that is meant changes these, and says so in its commit.
const EXPECTED_SUMS: (u64, u64, u64) = (
    8_922_604_983_121_239_283,
    16_597_291_470_742_759_343,
    5_420_188_016_926_548_620,
);

#[test]
fn another_seed_gives_another_graph_of_the_same_shape() {
    let a = gen(1, 20_000, false);
    let b = gen(2, 20_000, false);
    assert_ne!(a.graph, b.graph);
    let shares = |x: &Gen| -> BTreeMap<String, f64> {
        let s = kind(&x.truth, "summary")[0];
        let total = u(s, "nodes") as f64;
        s.get("labels")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l.get("label").unwrap().as_str().unwrap().to_string(),
                    u(l, "count") as f64 / total,
                )
            })
            .collect()
    };
    let (sa, sb) = (shares(&a), shares(&b));
    assert_eq!(sa.keys().collect::<Vec<_>>(), sb.keys().collect::<Vec<_>>());
    for (label, x) in &sa {
        let y = sb[label];
        // Every label holds a similar share of the nodes, within a third
        // of its size or half a percentage point.
        assert!(
            (x - y).abs() <= (x.max(y) / 3.0).max(0.005),
            "{label}: {x:.4} of nodes with seed 1, {y:.4} with seed 2"
        );
    }
    let near = |p: u64, q: u64| (p as f64 / q as f64 - 1.0).abs() < 0.1;
    assert!(near(a.summary.nodes, b.summary.nodes));
    assert!(near(a.summary.edges, b.summary.edges));
}

#[test]
fn one_parameter_sets_the_size_from_hundreds_of_nodes_up() {
    for n in [300u64, 3_000, 30_000] {
        let x = gen(5, n, false);
        let got = x.summary.nodes as f64;
        assert!(
            (got / n as f64 - 1.0).abs() < 0.15,
            "asked for {n} nodes, got {got}"
        );
    }
}

// ----------------------------------------------------------------- import

#[test]
fn the_graph_imports_cleanly_and_the_counts_match() {
    let x = gen(1, 3_000, true);
    let g = load(&x.graph);
    assert_eq!(g.node_count() as u64, x.summary.nodes);
    assert_eq!(g.edge_count() as u64, x.summary.edges);
    let s = kind(&x.truth, "summary")[0];
    assert_eq!(u(s, "nodes"), x.summary.nodes);
    for l in s.get("labels").unwrap().as_array().unwrap() {
        let label = l.get("label").unwrap().as_str().unwrap();
        assert_eq!(
            g.nodes_with_label(label).len() as u64,
            u(l, "count"),
            "{label}"
        );
    }
    let stats = g.stats();
    for t in s.get("types").unwrap().as_array().unwrap() {
        let ty = t.get("type").unwrap().as_str().unwrap();
        let n = stats
            .edge_types
            .iter()
            .find(|(k, _)| k == ty)
            .map(|(_, n)| *n as u64);
        assert_eq!(n, Some(u(t, "count")), "{ty}");
    }
    // The id written for each node is the id it gets.
    for line in x
        .graph
        .lines()
        .filter(|l| l.contains("\"type\":\"node\""))
        .take(500)
    {
        let j = parse_json(line).unwrap();
        let id = u(&j, "id");
        let label = &ids_labels(&j)[0];
        assert_eq!(&g.node_labels(id), &vec![label.clone()]);
    }
    // The in-memory import the CLI uses agrees.
    let mut h = Graph::memory();
    let (n, e) = glider::import_jsonl(&mut h, &x.graph).unwrap();
    assert_eq!((n as u64, e as u64), (x.summary.nodes, x.summary.edges));
    assert_eq!(glider::export_jsonl(&g), glider::export_jsonl(&h));
}

fn ids_labels(j: &Json) -> Vec<String> {
    strs(j, "labels")
}

#[test]
fn every_id_in_the_truth_and_the_questions_exists_in_the_graph() {
    let x = gen(4, 3_000, true);
    let g = load(&x.graph);
    let mut checked = 0;
    let mut node = |id: u64, what: &str| {
        assert!(
            g.node(id).is_some(),
            "{what}: node {id} is not in the graph"
        );
        checked += 1;
    };
    for r in &x.truth {
        let k = r.get("kind").unwrap().as_str().unwrap();
        for key in ["id", "a", "b", "line", "from", "to"] {
            if let Some(v) = r.get(key).and_then(|v| v.as_u64()) {
                node(v, k);
            }
        }
        for key in ["members", "order"] {
            for v in ids(r, key) {
                node(v, k);
            }
        }
        if k == "station" || k == "design" {
            for key in ["tools", "parts", "standards"] {
                for v in ids(r, key) {
                    node(v, k);
                }
            }
        }
        for d in r
            .get("deviations")
            .and_then(|v| v.as_array())
            .unwrap_or(&[])
        {
            for key in ["element", "location", "instruction", "tool", "depends_on"] {
                if let Some(v) = d.get(key).and_then(|v| v.as_u64()) {
                    node(v, "deviation");
                }
            }
        }
        if k == "fault" {
            for e in ids(r, "edges") {
                assert!(g.edge(e).is_some(), "fault: edge {e} is not in the graph");
            }
        }
    }
    for q in &x.questions {
        for v in ids(q, "anchors").into_iter().chain(ids(q, "ids")) {
            node(v, "question");
        }
    }
    assert!(checked > 2_000, "only {checked} ids checked");
    // Labels the truth names are the labels the graph holds.
    for r in kind(&x.truth, "duplicate")
        .into_iter()
        .chain(kind(&x.truth, "lookalike"))
    {
        let label = r.get("label").unwrap().as_str().unwrap();
        for v in [u(r, "a"), u(r, "b")] {
            assert_eq!(g.node_labels(v), vec![label.to_string()]);
        }
    }
    for r in kind(&x.truth, "station") {
        assert_eq!(g.node_labels(u(r, "id")), vec!["Station".to_string()]);
    }
}

// ------------------------------------------------------- duplicates, look-alikes

fn words(s: &str) -> BTreeSet<String> {
    s.to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect()
}

fn squash(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '.')
        .collect()
}

#[test]
fn planted_duplicates_agree_and_lookalikes_really_differ() {
    let x = gen(2, 5_000, false);
    let g = load(&x.graph);
    let dups = kind(&x.truth, "duplicate");
    let looks = kind(&x.truth, "lookalike");
    let labels = |v: &[&Json]| -> BTreeSet<String> {
        v.iter()
            .map(|r| r.get("label").unwrap().as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        labels(&dups).len(),
        3,
        "duplicates of tools, parts and instructions"
    );
    assert_eq!(
        labels(&looks).len(),
        3,
        "look-alike tools, parts and instructions"
    );
    for r in &dups {
        let (a, b) = (u(r, "a"), u(r, "b"));
        assert_ne!(a, b);
        for k in strs(r, "agree_on") {
            assert_eq!(prop(&g, a, &k), prop(&g, b, &k), "duplicate {a}/{b} on {k}");
            assert_ne!(prop(&g, a, &k), Value::Null);
        }
        for k in strs(r, "differ_on") {
            assert_ne!(prop(&g, a, &k), prop(&g, b, &k), "duplicate {a}/{b} on {k}");
        }
        // The name is the same name, written another way.
        let key = if g.node_labels(a)[0] == "WorkInstruction" {
            "title"
        } else {
            "name"
        };
        assert_eq!(squash(&text(&g, a, key)), squash(&text(&g, b, key)));
        // And the two are connected the same way.
        let around = |id: u64| -> Vec<(String, u64)> {
            let mut v: Vec<_> = g
                .neighbors(id, Dir::Out, None)
                .into_iter()
                .map(|e| (g.edge_type_name(e.edge).unwrap().to_string(), e.other))
                .collect();
            v.sort();
            v
        };
        assert_eq!(around(a), around(b));
    }
    for r in &looks {
        let (a, b) = (u(r, "a"), u(r, "b"));
        for k in strs(r, "differ_on") {
            assert_ne!(
                prop(&g, a, &k),
                prop(&g, b, &k),
                "look-alike {a}/{b} on {k}"
            );
        }
        let key = if g.node_labels(a)[0] == "WorkInstruction" {
            "title"
        } else {
            "name"
        };
        let (wa, wb) = (words(&text(&g, a, key)), words(&text(&g, b, key)));
        let jac = wa.intersection(&wb).count() as f64 / wa.union(&wb).count() as f64;
        assert!(
            jac >= 0.5,
            "look-alike {a}/{b} do not look alike: {wa:?} {wb:?}"
        );
    }
}

// ------------------------------------------------------------------ designs

/// What a station is, structurally: its hierarchy by position (names of
/// sides, locations and elements, which repeat across copies of a design)
/// and the shared things each element uses. Identity (codes, numbers,
/// dates, times) is left out.
fn signature(g: &Graph, station: u64, skip: &BTreeSet<u64>, add: &[(u64, u64)]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut path: BTreeMap<u64, String> = BTreeMap::new();
    for side in out(g, station, "HAS_SIDE") {
        let sp = text(g, side, "name");
        lines.push(format!("side {sp}"));
        for loc in out(g, side, "HAS_LOCATION") {
            let lp = format!("{sp}/{}", text(g, loc, "name"));
            lines.push(format!("location {lp}"));
            for we in out(g, loc, "HAS_ELEMENT") {
                if !skip.contains(&we) {
                    path.insert(we, format!("{lp}/{}", text(g, we, "name")));
                }
            }
        }
    }
    for (&we, p) in &path {
        lines.push(format!("element {p} seq {:?}", prop(g, we, "seq")));
        for t in out(g, we, "USES_TOOL") {
            lines.push(format!("{p} USES_TOOL {t}"));
        }
        for (e, part) in out_edges(g, we, "CONSUMES") {
            let q = g.edge_props(e);
            lines.push(format!("{p} CONSUMES {part} {q:?}"));
        }
        for s in out(g, we, "FOLLOWS") {
            lines.push(format!("{p} FOLLOWS {s}"));
        }
        for d in out(g, we, "DEPENDS_ON") {
            if let Some(dp) = path.get(&d) {
                lines.push(format!("{p} DEPENDS_ON {dp}"));
            }
        }
        for wi in out(g, we, "HAS_INSTRUCTION") {
            lines.push(format!(
                "{p} HAS_INSTRUCTION {} {} {}",
                text(g, wi, "revision"),
                text(g, wi, "title"),
                text(g, wi, "text")
            ));
        }
    }
    for &(we, tool) in add {
        lines.push(format!("{} USES_TOOL {tool}", path[&we]));
    }
    lines.sort();
    lines
}

#[test]
fn stations_of_one_design_are_identical_except_for_their_recorded_deviations() {
    let x = gen(3, 8_000, false);
    let g = load(&x.graph);
    let stations = kind(&x.truth, "station");
    let mut clean: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    let mut clean_count: BTreeMap<u64, usize> = BTreeMap::new();
    for r in &stations {
        if ids(r, "order").is_empty() {
            continue;
        }
        if r.get("deviations").unwrap().as_array().unwrap().is_empty() {
            let d = u(r, "design");
            let sig = signature(&g, u(r, "id"), &BTreeSet::new(), &[]);
            match clean.get(&d) {
                Some(s) => assert_eq!(s, &sig, "station {} differs from its design", u(r, "id")),
                None => {
                    clean.insert(d, sig);
                }
            }
            *clean_count.entry(d).or_insert(0) += 1;
        }
    }
    assert!(clean_count.values().filter(|&&n| n >= 2).count() >= 3);
    let mut seen = BTreeMap::new();
    for r in &stations {
        let devs = r.get("deviations").unwrap().as_array().unwrap();
        if devs.is_empty() {
            continue;
        }
        let d = u(r, "design");
        let Some(reference) = clean.get(&d) else {
            continue;
        };
        let id = u(r, "id");
        let mut skip = BTreeSet::new();
        let mut add = Vec::new();
        for dev in devs {
            let ty = dev.get("type").unwrap().as_str().unwrap();
            *seen.entry(ty.to_string()).or_insert(0) += 1;
            match ty {
                "extra_element" => {
                    skip.insert(u(dev, "element"));
                }
                "missing_tool" => add.push((u(dev, "element"), u(dev, "tool"))),
                other => panic!("unknown deviation {other}"),
            }
        }
        let raw = signature(&g, id, &BTreeSet::new(), &[]);
        assert_ne!(
            &raw, reference,
            "station {id} has a deviation the structure does not show"
        );
        let undone = signature(&g, id, &skip, &add);
        assert_eq!(
            &undone, reference,
            "station {id} differs beyond its recorded deviations"
        );
    }
    assert!(
        seen.get("extra_element").copied().unwrap_or(0) >= 2,
        "{seen:?}"
    );
    assert!(
        seen.get("missing_tool").copied().unwrap_or(0) >= 2,
        "{seen:?}"
    );
    // Every node keeps its own identity: no two stations, elements or
    // instructions share a code or number.
    for (label, key) in [
        ("Station", "code"),
        ("WorkElement", "code"),
        ("WorkInstruction", "number"),
    ] {
        let mut set = BTreeSet::new();
        for id in g.nodes_with_label(label) {
            assert!(set.insert(text(&g, id, key)), "{label} {key} repeats");
        }
    }
}

#[test]
fn the_dependency_order_is_a_topological_order_of_each_station() {
    let x = gen(6, 4_000, false);
    let g = load(&x.graph);
    let mut edges = 0;
    for r in kind(&x.truth, "station") {
        let order = ids(r, "order");
        let mut actual = elements(&g, u(r, "id"));
        actual.sort_unstable();
        let mut listed = order.clone();
        listed.sort_unstable();
        assert_eq!(listed, actual, "the order lists the station's elements");
        let pos: BTreeMap<u64, usize> = order.iter().enumerate().map(|(i, &e)| (e, i)).collect();
        for &we in &order {
            for dep in out(&g, we, "DEPENDS_ON") {
                let p = pos
                    .get(&dep)
                    .expect("a dependency stays inside its station");
                assert!(*p < pos[&we], "{we} depends on {dep}, listed after it");
                edges += 1;
            }
        }
    }
    assert!(edges > 50);
}

// ----------------------------------------------------------------- answers

#[test]
fn every_kind_of_question_is_asked_and_its_answer_follows_from_the_graph() {
    let x = gen(1, 6_000, false);
    let g = load(&x.graph);
    let stations = kind(&x.truth, "station");
    let groups = kind(&x.truth, "topic_group");
    let mut kinds = BTreeMap::new();
    for q in &x.questions {
        let k = q.get("kind").unwrap().as_str().unwrap();
        *kinds.entry(k.to_string()).or_insert(0) += 1;
        let anchors = ids(q, "anchors");
        let want = ids(q, "ids");
        let names = strs(q, "names");
        assert!(!want.is_empty(), "{q:?}");
        let got: Vec<u64> = match k {
            "lookup" => {
                let mut s = BTreeSet::new();
                for we in elements(&g, anchors[0]) {
                    s.extend(out(&g, we, "USES_TOOL"));
                }
                s.into_iter().collect()
            }
            "multihop" => {
                let (line, model) = (anchors[0], anchors[1]);
                let mut s = BTreeSet::new();
                for st in out(&g, line, "HAS_STATION") {
                    for we in elements(&g, st) {
                        for wi in out(&g, we, "FOLLOWS") {
                            if out(&g, wi, "APPLIES_TO").contains(&model) {
                                s.insert(wi);
                            }
                        }
                    }
                }
                s.into_iter().collect()
            }
            "aggregate" => {
                let design = stations
                    .iter()
                    .find(|r| u(r, "id") == anchors[0])
                    .map(|r| u(r, "design"))
                    .unwrap();
                let same: Vec<u64> = stations
                    .iter()
                    .filter(|r| u(r, "design") == design)
                    .map(|r| u(r, "id"))
                    .collect();
                assert_eq!(u(q, "count"), same.len() as u64);
                assert!(matches!(
                    q.get("complete"),
                    Some(Json::Bool(c)) if *c == (same.len() <= MEMBER_CAP)
                ));
                same.into_iter().take(MEMBER_CAP).collect()
            }
            "temporal" => {
                let at = q.get("at").unwrap().as_str().unwrap().to_string();
                out_edges(&g, anchors[0], "HAS_INSTRUCTION")
                    .into_iter()
                    .filter(|&(e, _)| {
                        let from = edge_text(&g, e, "from").unwrap();
                        let to = edge_text(&g, e, "to");
                        from <= at && to.map_or(true, |t| at < t)
                    })
                    .map(|(_, wi)| wi)
                    .collect()
            }
            "similarity" => {
                let group = groups
                    .iter()
                    .find(|r| ids(r, "members").contains(&anchors[0]))
                    .unwrap();
                ids(group, "members")
                    .into_iter()
                    .filter(|&m| m != anchors[0])
                    .collect()
            }
            other => panic!("unknown kind {other}"),
        };
        assert_eq!(want, got, "{}", q.get("text").unwrap().as_str().unwrap());
        if k != "aggregate" {
            assert_eq!(names.len(), want.len());
            for (id, name) in want.iter().zip(&names) {
                let key = if k == "lookup" { "name" } else { "title" };
                assert_eq!(&text(&g, *id, key), name);
            }
        }
    }
    for k in ["lookup", "multihop", "aggregate", "temporal", "similarity"] {
        assert!(
            kinds.get(k).copied().unwrap_or(0) >= 4,
            "too few {k} questions: {kinds:?}"
        );
    }
}

#[test]
fn instructions_of_one_topic_read_alike_and_of_two_topics_do_not() {
    let x = gen(1, 6_000, false);
    let g = load(&x.graph);
    let topics: Vec<(u64, String)> = kind(&x.truth, "topic")
        .into_iter()
        .map(|r| {
            (
                u(r, "id"),
                r.get("topic").unwrap().as_str().unwrap().to_string(),
            )
        })
        .collect();
    // Every instruction in the graph has a topic in the truth.
    let wi = g.nodes_with_label("WorkInstruction");
    assert_eq!(wi.len(), topics.len());
    let w: BTreeMap<u64, BTreeSet<String>> = topics
        .iter()
        .map(|(id, _)| {
            let t = text(&g, *id, "text").to_lowercase();
            let set = t
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| s.len() > 3)
                .map(String::from)
                .collect();
            (*id, set)
        })
        .collect();
    let jac = |a: u64, b: u64| {
        let (x, y) = (&w[&a], &w[&b]);
        x.intersection(y).count() as f64 / x.union(y).count() as f64
    };
    let (mut same, mut diff, mut ns, mut nd) = (0.0, 0.0, 0, 0);
    let step = (topics.len() / 300).max(1);
    let sample: Vec<&(u64, String)> = topics.iter().step_by(step).collect();
    for (i, a) in sample.iter().enumerate() {
        for b in &sample[i + 1..] {
            if text(&g, a.0, "text") == text(&g, b.0, "text") {
                continue;
            }
            if a.1 == b.1 {
                same += jac(a.0, b.0);
                ns += 1;
            } else {
                diff += jac(a.0, b.0);
                nd += 1;
            }
        }
    }
    let (same, diff) = (same / ns as f64, diff / nd as f64);
    assert!(
        same > 2.0 * diff,
        "same topic {same:.3}, different topics {diff:.3}"
    );
    // A topic group is closer still.
    let mut grp = 0.0;
    let mut n = 0;
    for r in kind(&x.truth, "topic_group") {
        let m = ids(r, "members");
        for i in 0..m.len() {
            for j in i + 1..m.len() {
                grp += jac(m[i], m[j]);
                n += 1;
            }
        }
    }
    assert!(
        grp / n as f64 > same,
        "group {:.3}, topic {same:.3}",
        grp / n as f64
    );
}

// -------------------------------------------------------- sharing, faults

#[test]
fn tool_sharing_in_the_truth_is_what_the_graph_shows() {
    let x = gen(8, 6_000, false);
    let g = load(&x.graph);
    let station_line: BTreeMap<u64, u64> = kind(&x.truth, "station")
        .into_iter()
        .map(|r| (u(r, "id"), u(r, "line")))
        .collect();
    let mut shared = 0;
    for r in kind(&x.truth, "tool") {
        let tool = u(r, "id");
        let users: Vec<u64> = g
            .neighbors(tool, Dir::In, None)
            .into_iter()
            .filter(|a| g.edge_type_name(a.edge) == Some("USES_TOOL"))
            .map(|a| a.other)
            .collect();
        let mut stations = BTreeSet::new();
        for &we in &users {
            let loc = g
                .neighbors(we, Dir::In, None)
                .into_iter()
                .find(|a| g.edge_type_name(a.edge) == Some("HAS_ELEMENT"))
                .unwrap()
                .other;
            let side = g.neighbors(loc, Dir::In, None)[0].other;
            stations.insert(g.neighbors(side, Dir::In, None)[0].other);
        }
        let lines: BTreeSet<u64> = stations.iter().map(|s| station_line[s]).collect();
        assert_eq!(u(r, "elements"), users.len() as u64, "tool {tool}");
        assert_eq!(u(r, "stations"), stations.len() as u64, "tool {tool}");
        assert_eq!(u(r, "lines"), lines.len() as u64, "tool {tool}");
        if lines.len() > 1 {
            shared += 1;
        }
    }
    assert!(shared >= 10, "tools shared across lines: {shared}");
}

fn orphans(g: &Graph) -> BTreeSet<u64> {
    let mut v = BTreeSet::new();
    let mut from = 0;
    loop {
        let ids = g.nodes_from(from, 4096);
        let Some(&last) = ids.last() else { break };
        for id in ids {
            if g.degree(id, Dir::Both) == 0 {
                v.insert(id);
            }
        }
        from = last + 1;
    }
    v
}

fn parallel(g: &Graph) -> BTreeSet<Vec<u64>> {
    let mut groups: BTreeMap<(u64, u64, String, String), Vec<u64>> = BTreeMap::new();
    let mut from = 0;
    loop {
        let ids = g.edges_from(from, 4096);
        let Some(&last) = ids.last() else { break };
        for id in ids {
            let e = g.edge(id).unwrap();
            let key = (
                e.from,
                e.to,
                g.edge_type_name(id).unwrap().to_string(),
                format!("{:?}", g.edge_props(id)),
            );
            groups.entry(key).or_default().push(id);
        }
        from = last + 1;
    }
    groups.into_values().filter(|v| v.len() > 1).collect()
}

#[test]
fn faults_are_planted_only_when_asked_for_and_all_of_them_are_listed() {
    let clean = gen(9, 6_000, false);
    let g = load(&clean.graph);
    assert!(
        orphans(&g).is_empty(),
        "orphans without --faults: {:?}",
        orphans(&g)
    );
    assert!(parallel(&g).is_empty(), "parallel edges without --faults");
    assert!(kind(&clean.truth, "fault").is_empty());

    let x = gen(9, 6_000, true);
    let g = load(&x.graph);
    let faults = kind(&x.truth, "fault");
    let listed_orphans: BTreeSet<u64> = faults
        .iter()
        .filter(|r| r.get("fault").unwrap().as_str() == Some("orphan"))
        .map(|r| u(r, "id"))
        .collect();
    let listed_edges: BTreeSet<Vec<u64>> = faults
        .iter()
        .filter(|r| r.get("fault").unwrap().as_str() == Some("parallel_edge"))
        .map(|r| ids(r, "edges"))
        .collect();
    assert!(listed_orphans.len() >= 3 && listed_edges.len() >= 3);
    assert_eq!(orphans(&g), listed_orphans);
    assert_eq!(parallel(&g), listed_edges);
    // Planting faults leaves the rest of the graph as it was: the same
    // nodes, in the same order.
    let nodes = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| l.contains("\"type\":\"node\""))
            .map(String::from)
            .collect()
    };
    let (a, b) = (nodes(&clean.graph), nodes(&x.graph));
    assert_eq!(a[..], b[..a.len()]);
}
