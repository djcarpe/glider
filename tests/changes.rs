//! The change journal (`changes`, docs/DERIVED.md): for every committed
//! change to a node or an edge, by any path, it says which id changed, what
//! happened to it last and at what revision; it is exact at commit
//! boundaries, leaves nothing for rolled-back work, is bounded and says so
//! when it cannot answer, and is the same after recovery and on a replica.
//!
//! The witness is a second extension that follows every write and keeps an
//! append-only list of what each hook was told, under the log position it
//! was told at. Its keys start with `!`, so `CLEAR` keeps them, and they are
//! transactional, so rolled-back writes leave none.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};

use glider::ext::changes::{self, Change, Kind};
use glider::ext::{self, Args, BulkWrite, EdgeImage, Extension, NodeImage, Spec};
use glider::graph::{Graph, OpenOptions};
use glider::legacy::graph::Graph as Legacy;
use glider::query::{self, QueryResult};
use glider::store::{Op, Sync};
use glider::value::Value;
use glider::{Error, Result};

const WITNESS: &str = "witness";

thread_local! {
    /// The write paths the workload took on this thread.
    static PATHS: std::cell::RefCell<BTreeSet<&'static str>> = const {
        std::cell::RefCell::new(BTreeSet::new())
    };
}

/// Keeps, for every hook call, the log position it ran at and what it was
/// told: `n<id>`, `e<id>` and `a<id>` for each endpoint, or `clear`.
/// Refuses a node labelled `Fail`.
struct Witness;

fn witness(g: &mut Graph, told: String) -> Result<()> {
    let mut k = b"!w".to_vec();
    k.extend_from_slice(&g.log_end().to_be_bytes());
    g.ext_write(WITNESS, vec![(k, told.into_bytes())], Vec::new())
}

impl Extension for Witness {
    fn name(&self) -> &str {
        WITNESS
    }
    fn version(&self) -> &str {
        "1"
    }
    fn summary(&self) -> &str {
        "keeps what every hook was told"
    }
    fn procedures(&self) -> &[Spec] {
        &[]
    }
    fn call(&self, _: &mut Graph, _: &str, _: &Args) -> Result<QueryResult> {
        unreachable!()
    }
    fn wants_writes(&self, _: &Graph) -> bool {
        true
    }
    fn wants_edge_writes(&self, _: &Graph) -> bool {
        true
    }
    fn on_node_write(
        &self,
        g: &mut Graph,
        id: u64,
        before: Option<&NodeImage>,
        after: Option<&NodeImage>,
    ) -> Result<()> {
        if after.is_some_and(|a| a.labels.iter().any(|l| l == "Fail")) {
            return Err(Error::Msg("the witness refuses a Fail node".into()));
        }
        // A node that appears or goes changes what is at it, too.
        if before.is_some() != after.is_some() {
            return witness(g, format!("n{id} a{id}"));
        }
        witness(g, format!("n{id}"))
    }
    fn on_edge_write(
        &self,
        g: &mut Graph,
        id: u64,
        before: Option<&EdgeImage>,
        after: Option<&EdgeImage>,
    ) -> Result<()> {
        let mut told = format!("e{id}");
        for e in [before, after].into_iter().flatten() {
            told.push_str(&format!(" a{} a{}", e.from, e.to));
        }
        witness(g, told)
    }
    fn on_bulk_write(&self, g: &mut Graph, _: BulkWrite) -> Result<()> {
        witness(g, "clear".into())
    }
}

fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| ext::register(Arc::new(Witness)));
}

fn q(g: &mut Graph, src: &str) -> QueryResult {
    query::execute(g, src).unwrap_or_else(|e| panic!("{src}: {e}"))
}

fn temp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("glider-changes-{}-{}.db", name, std::process::id()));
    cleanup(&p);
    p
}

fn cleanup(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(glider::store::lock_path(p));
    for suffix in ["-wal", "-data", "-tmp"] {
        let mut d = p.as_os_str().to_os_string();
        d.push(suffix);
        let _ = std::fs::remove_dir_all(PathBuf::from(d));
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

// ---------------------------------------------------------------- the state

/// The graph as exported: every node, every edge, and for each node the
/// edges at it (an adjacency changes when an edge is added, removed or
/// altered at either end).
#[derive(Clone, Debug, PartialEq)]
struct Obs {
    nodes: BTreeMap<u64, String>,
    edges: BTreeMap<u64, String>,
    adjacency: BTreeMap<u64, String>,
}

fn props_dbg(p: &[(String, Value)]) -> String {
    let mut p = p.to_vec();
    p.sort_by(|a, b| a.0.cmp(&b.0));
    format!("{p:?}")
}

fn observe(g: &Graph) -> Obs {
    let mut nodes = BTreeMap::new();
    for id in g.node_ids() {
        let mut l = g.node_labels(id);
        l.sort();
        nodes.insert(id, format!("{l:?} {}", props_dbg(&g.node_props(id))));
    }
    let mut edges = BTreeMap::new();
    let mut at: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for id in g.edge_ids() {
        let e = g.edge(id).unwrap();
        let t = g.edge_type_name(id).unwrap().to_string();
        let s = format!("{}-{t}->{} {}", e.from, e.to, props_dbg(&g.edge_props(id)));
        at.entry(e.from).or_default().push(format!("{id}:{s}"));
        at.entry(e.to).or_default().push(format!("{id}:{s}"));
        edges.insert(id, s);
    }
    let adjacency = at.into_iter().map(|(n, v)| (n, v.join("|"))).collect();
    Obs {
        nodes,
        edges,
        adjacency,
    }
}

/// What the witness was told by hooks that ran between log positions
/// `lo` (exclusive) and `hi` (inclusive), by kind; and whether a bulk
/// write was among it.
fn told(g: &Graph, lo: u64, hi: u64) -> (BTreeMap<Kind, BTreeSet<u64>>, bool) {
    let mut out: BTreeMap<Kind, BTreeSet<u64>> = BTreeMap::new();
    let mut bulk = false;
    for (k, v) in keyspace(g, WITNESS) {
        let at = u64::from_be_bytes(k[2..10].try_into().unwrap());
        if at <= lo || at > hi {
            continue;
        }
        for w in String::from_utf8(v).unwrap().split(' ') {
            if w == "clear" {
                bulk = true;
                continue;
            }
            let kind = match &w[..1] {
                "n" => Kind::Node,
                "e" => Kind::Edge,
                _ => Kind::Adjacency,
            };
            out.entry(kind).or_default().insert(w[1..].parse().unwrap());
        }
    }
    (out, bulk)
}

fn differ(a: &BTreeMap<u64, String>, b: &BTreeMap<u64, String>) -> BTreeSet<u64> {
    a.keys()
        .chain(b.keys())
        .filter(|id| a.get(id) != b.get(id))
        .copied()
        .collect()
}

/// A commit boundary: the revision and the graph there.
struct Boundary {
    rev: u64,
    obs: Obs,
}

/// Check `since` for every earlier boundary, taken now (at the last
/// boundary): exact when it answers, a plain refusal naming the floor when
/// it cannot. Returns how many answered.
fn check_all(g: &mut Graph, bs: &[Boundary], what: &str) -> usize {
    let now = bs.last().unwrap();
    assert_eq!(g.revision(), now.rev);
    assert_eq!(g.log_end(), now.rev, "{what}: at a commit boundary");
    let state = changes::status(g).expect("installed");
    let mut answered = 0;
    for (i, b) in bs.iter().enumerate() {
        let r1 = b.rev;
        match changes::since(g, r1, &[], None, usize::MAX) {
            Err(e) => {
                let e = e.to_string();
                assert!(
                    r1 < state.floor,
                    "{what}: refused {r1} at floor {}: {e}",
                    state.floor
                );
                assert!(!changes::answers(g, r1));
                assert!(
                    e.contains(&format!("answers for revision {} and later", state.floor)),
                    "{what}: {e}"
                );
                assert!(changes::changed(g, r1, Kind::Node, &[1]).is_none());
                assert!(changes::changed_any(g, r1, Kind::Edge).is_none());
            }
            Ok((list, more)) => {
                answered += 1;
                assert!(!more);
                assert!(r1 >= state.floor, "{what}: answered {r1} below the floor");
                let mut listed: BTreeMap<Kind, BTreeSet<u64>> = BTreeMap::new();
                let mut seen = BTreeSet::new();
                let mut last = None;
                for e in &list {
                    assert!(
                        e.revision > r1 && e.revision < now.rev,
                        "{what}: {e:?} outside ({r1}, {})",
                        now.rev
                    );
                    let pos = (e.kind, e.id);
                    assert!(last < Some(pos), "{what}: out of order");
                    last = Some(pos);
                    assert!(seen.insert((e.kind, e.id)), "{what}: {e:?} listed twice");
                    let exists = match e.kind {
                        Kind::Edge => now.obs.edges.contains_key(&e.id),
                        _ => now.obs.nodes.contains_key(&e.id),
                    };
                    assert_eq!(
                        e.change == Change::Put,
                        exists,
                        "{what}: {e:?} says what happened last wrongly"
                    );
                    listed.entry(e.kind).or_default().insert(e.id);
                }
                // Every id whose exported state differs is listed.
                for (kind, a, z) in [
                    (Kind::Node, &b.obs.nodes, &now.obs.nodes),
                    (Kind::Edge, &b.obs.edges, &now.obs.edges),
                    (Kind::Adjacency, &b.obs.adjacency, &now.obs.adjacency),
                ] {
                    let empty = BTreeSet::new();
                    let got = listed.get(&kind).unwrap_or(&empty);
                    let missing: Vec<u64> = differ(a, z).difference(got).copied().collect();
                    assert!(
                        missing.is_empty(),
                        "{what}: since({r1}) at {} misses {kind:?} {missing:?}",
                        now.rev
                    );
                }
                // And only ids written in between.
                let (heard, bulk) = told(g, r1, now.rev);
                assert!(!bulk, "{what}: answered across a CLEAR");
                for (kind, ids) in &listed {
                    let empty = BTreeSet::new();
                    let extra: Vec<u64> = ids
                        .difference(heard.get(kind).unwrap_or(&empty))
                        .copied()
                        .collect();
                    assert!(
                        extra.is_empty(),
                        "{what}: since({r1}) lists {kind:?} {extra:?}, not written after it"
                    );
                }
                // The questions other extensions ask agree with the list.
                for kind in Kind::ALL {
                    let ids = listed.get(&kind).cloned().unwrap_or_default();
                    assert_eq!(
                        changes::changed_any(g, r1, kind),
                        Some(!ids.is_empty()),
                        "{what}: changed_any({r1}, {kind:?})"
                    );
                    let probe: Vec<u64> = (0..40).chain(ids.iter().copied()).collect();
                    let got = changes::changed(g, r1, kind, &probe).unwrap();
                    for (id, c) in probe.iter().zip(got) {
                        assert_eq!(c, ids.contains(id), "{what}: changed({r1}, {kind:?}, {id})");
                    }
                }
                // The statement, in slices of 7 and by kind, says the same.
                if i == 0 || i + 2 >= bs.len() || i % 11 == 0 {
                    assert_eq!(statement_since(g, r1, 7, ""), rows(&list));
                    let nodes: Vec<_> = list
                        .iter()
                        .filter(|e| e.kind == Kind::Node)
                        .copied()
                        .collect();
                    assert_eq!(statement_since(g, r1, 3, "kinds: ['node'], "), rows(&nodes));
                }
            }
        }
    }
    answered
}

fn rows(list: &[changes::Entry]) -> Vec<Vec<Value>> {
    list.iter()
        .map(|e| {
            vec![
                Value::Int(e.revision as i64),
                Value::Text(e.kind.name().into()),
                Value::Int(e.id as i64),
                Value::Text(e.change.name().into()),
            ]
        })
        .collect()
}

/// `CALL changes.since` in slices of `limit`, following the cursor its
/// message gives.
fn statement_since(g: &mut Graph, rev: u64, limit: usize, extra: &str) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    let mut after = String::new();
    loop {
        let r = q(
            g,
            &format!("CALL changes.since({extra}revision: {rev}, limit: {limit}{after})"),
        );
        assert!(r.rows.len() <= limit);
        out.extend(r.rows);
        let msg = r.message.unwrap();
        match msg.split("after: '").nth(1) {
            Some(rest) => after = format!(", after: '{}'", rest.trim_end_matches('\'')),
            None => {
                assert!(msg.contains("complete up to revision"), "{msg}");
                return out;
            }
        }
    }
}

// ----------------------------------------------------------- the workload

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick(&mut self, v: &[u64]) -> Option<u64> {
        (!v.is_empty()).then(|| v[self.below(v.len() as u64) as usize])
    }
}

fn legacy(seed: u64, nodes: u64, edges: u64) -> Legacy {
    let mut l = Legacy::memory();
    let mut r = Rng(seed | 1);
    for i in 0..nodes {
        l.add_node(&["L".to_string()], vec![("n".into(), Value::Int(i as i64))])
            .unwrap();
    }
    for _ in 0..edges {
        let a = 1 + r.below(nodes);
        let b = 1 + r.below(nodes);
        l.add_edge(
            a,
            b,
            "LL",
            vec![("w".into(), Value::Int(r.below(9) as i64))],
        )
        .unwrap();
    }
    l
}

/// One step of the random workload: a transaction by some write path, or
/// work that rolls back. Returns the step's name and whether it must have
/// left everything as it was.
fn step(g: &mut Graph, r: &mut Rng, allow_clear: bool) -> (&'static str, bool) {
    let nodes = g.node_ids();
    let edges = g.edge_ids();
    let n = |r: &mut Rng| r.pick(&nodes).unwrap_or(1);
    let v = |r: &mut Rng| Value::Int(r.below(4) as i64);
    match r.below(24) {
        0..=2 => {
            q(
                g,
                &format!(
                    "CREATE (a:P {{k: {}}})-[:K {{w: {}}}]->(b:Q {{k: {}}})",
                    r.below(5),
                    r.below(3),
                    r.below(5)
                ),
            );
            ("CREATE", false)
        }
        3 => {
            q(
                g,
                &format!(
                    "MATCH (n:P) WHERE n.k = {} SET n.v = {}",
                    r.below(5),
                    r.below(3)
                ),
            );
            ("SET", false)
        }
        4 => {
            q(
                g,
                &format!(
                    "MATCH ()-[e:K]->() WHERE e.w = {} SET e.x = {}",
                    r.below(3),
                    r.below(3)
                ),
            );
            ("SET edge", false)
        }
        5 => {
            q(
                g,
                &format!("MATCH (n) WHERE id(n) = {} DETACH DELETE n", n(r)),
            );
            ("DETACH DELETE", false)
        }
        6 => {
            let (a, b) = (n(r), n(r));
            q(
                g,
                &format!("MATCH (a), (b) WHERE id(a) = {a} AND id(b) = {b} CREATE (a)-[:M]->(b)"),
            );
            ("CREATE edge", false)
        }
        7..=9 => {
            // The graph API, call by call.
            let a = n(r);
            match r.below(8) {
                0 => {
                    let _ = g.add_node(&["A".into()], vec![("k".into(), v(r))]).unwrap();
                }
                1 if g.node(a).is_some() => {
                    let b = n(r);
                    if g.node(b).is_some() {
                        g.add_edge(a, b, "API", vec![("w".into(), v(r))]).unwrap();
                    }
                }
                2 if g.node(a).is_some() => g.set_node_prop(a, "k", v(r)).unwrap(),
                3 if g.node(a).is_some() => g.add_label(a, "X").unwrap(),
                4 if g.node(a).is_some() => g.remove_label(a, "X").unwrap(),
                5 => {
                    if let Some(e) = r.pick(&edges) {
                        g.set_edge_prop(e, "w", v(r)).unwrap();
                    }
                }
                6 => {
                    if let Some(e) = r.pick(&edges) {
                        g.delete_edge(e).unwrap();
                    }
                }
                _ => {
                    g.delete_node(a).unwrap();
                }
            }
            ("API", false)
        }
        10 => {
            g.begin().unwrap();
            for _ in 0..1 + r.below(5) {
                let a = g.add_node(&["B".into()], vec![]).unwrap();
                let b = n(r);
                if g.node(b).is_some() {
                    g.add_edge(a, b, "BB", vec![]).unwrap();
                }
                if r.below(2) == 0 {
                    g.set_node_prop(a, "k", v(r)).unwrap();
                }
            }
            if r.below(3) == 0 {
                g.delete_node(n(r)).unwrap();
            }
            g.commit().unwrap();
            ("begin block", false)
        }
        11 => {
            for src in [
                "BEGIN".to_string(),
                "CREATE (:T {t: 1})-[:TT]->(:T {t: 2})".into(),
                format!("MATCH (n:T) WHERE n.t = 1 SET n.t = {}", 10 + r.below(5)),
                "MATCH (b:T {t: 2}) DETACH DELETE b".into(),
                "COMMIT".into(),
            ] {
                q(g, &src);
            }
            ("BEGIN ... COMMIT", false)
        }
        12 => {
            let p = |s: String| (s, Vec::new());
            query::execute_batch(
                g,
                &[
                    p("CREATE (:Batch {i: 1})-[:BB]->(:Batch {i: 2})".into()),
                    p(format!(
                        "MATCH (b:Batch {{i: 1}}) SET b.i = {}",
                        3 + r.below(3)
                    )),
                    p("MATCH (b:Batch {i: 2}) DETACH DELETE b".into()),
                ],
            )
            .unwrap();
            ("batch", false)
        }
        13 => {
            let jsonl = format!(
                "{{\"type\":\"node\",\"key\":\"a\",\"labels\":[\"J\"],\"props\":{{\"k\":{}}}}}\n\
                 {{\"type\":\"node\",\"key\":\"b\",\"label\":\"J\"}}\n\
                 {{\"type\":\"edge\",\"from\":\"a\",\"to\":\"b\",\"label\":\"JJ\"}}\n",
                r.below(9)
            );
            query::import_jsonl(g, &jsonl).unwrap();
            ("import_jsonl", false)
        }
        14 => {
            // Replayed operations with explicit ids: a new node, an edge
            // replaced by one with other endpoints, a node deleted.
            let next = nodes.last().copied().unwrap_or(0) + 1;
            g.apply_op(Op::NodeAdd {
                id: next,
                labels: vec!["O".into()],
                props: vec![],
            })
            .unwrap();
            if let Some(e) = r.pick(&edges) {
                g.apply_op(Op::EdgeAdd {
                    id: e,
                    from: next,
                    to: n(r).min(next),
                    etype: "MOVED".into(),
                    props: vec![],
                })
                .unwrap();
            }
            g.apply_op(Op::NodeDel { id: n(r) }).unwrap();
            ("apply_op", false)
        }
        15 => {
            g.import_legacy(&legacy(r.next(), 6, 8)).unwrap();
            ("import_legacy", false)
        }
        16 if allow_clear && r.below(4) == 0 => {
            if r.below(2) == 0 {
                q(g, "CLEAR");
            } else {
                g.apply_op(Op::Clear).unwrap();
            }
            ("CLEAR", false)
        }
        17 => {
            for src in [
                "BEGIN",
                "CREATE (:Gone)-[:GONE]->(:Gone)",
                "MATCH (n:P) SET n.gone = 1",
                "CLEAR",
                "ROLLBACK",
            ] {
                q(g, src);
            }
            ("ROLLBACK", true)
        }
        18 => {
            assert!(query::execute(g, "CREATE (:Fine)-[:K]->(:Fail)").is_err());
            g.begin().unwrap();
            g.add_node(&["Earlier".into()], vec![]).unwrap();
            if let Some(e) = r.pick(&edges) {
                g.delete_edge(e).unwrap();
            }
            assert!(g.add_node(&["Fail".into()], vec![]).is_err());
            assert!(g.commit().is_err());
            ("failing hook", true)
        }
        19 => {
            // Writes that change nothing.
            if let Some(e) = r.pick(&edges) {
                if let Some(w) = g.edge_prop(e, "w") {
                    g.set_edge_prop(e, "w", w).unwrap();
                }
            }
            let a = n(r);
            if g.node(a).is_some() {
                g.unset_node_prop(a, "absent").unwrap();
            }
            g.apply_op(Op::EdgeDel { id: 9_999_999 }).unwrap();
            ("nothing", true)
        }
        20 => {
            q(g, &format!("INDEX ON :P(i{})", r.below(1000)));
            ("INDEX", false)
        }
        _ => {
            let a = g.add_node(&["Z".into()], vec![("z".into(), v(r))]).unwrap();
            if let Some(b) = r.pick(&nodes) {
                if g.node(b).is_some() {
                    g.add_edge(b, a, "Z", vec![]).unwrap();
                }
            }
            ("API pair", false)
        }
    }
}

/// Run the random workload with `steps` steps, checking `since` for every
/// pair of boundaries as it goes. Returns the boundaries' revisions, the
/// number of answered checks and of refusals.
fn workload(
    g: &mut Graph,
    seed: u64,
    steps: usize,
    cap: Option<u64>,
    what: &str,
) -> (Vec<u64>, usize, usize) {
    register();
    let clear = true;
    q(g, "CREATE EXTENSION witness");
    for i in 0..10 {
        q(
            g,
            &format!("CREATE (:P {{k: {i}}})-[:K {{w: 1}}]->(:Q {{k: {i}}})"),
        );
    }
    q(g, "CREATE EXTENSION changes");
    if let Some(c) = cap {
        q(g, &format!("CALL changes.cap(entries: {c})"));
    }
    let mut r = Rng(seed);
    let mut bs = vec![Boundary {
        rev: g.revision(),
        obs: observe(g),
    }];
    let (mut answered, mut refused) = (0, 0);
    for s in 0..steps {
        let before = (g.revision(), keyspace(g, changes::NS), observe(g));
        let (name, unchanged) = if s % 17 == 16 {
            // Release below an earlier boundary now and then.
            let below = bs[r.below(bs.len() as u64 / 2) as usize].rev;
            q(g, &format!("CALL changes.release(below: {below})"));
            ("release", false)
        } else if s % 50 == 30 && clear {
            // CLEAR, by statement and as a replayed operation.
            if (s / 50) % 2 == 0 {
                q(g, "CLEAR");
            } else {
                g.apply_op(Op::Clear).unwrap();
            }
            ("CLEAR", false)
        } else {
            step(g, &mut r, clear)
        };
        PATHS.with(|p| p.borrow_mut().insert(name));
        let what = format!("{what} step {s} {name}");
        if unchanged {
            assert_eq!(g.revision(), before.0, "{what}: the revision moved");
            assert_eq!(
                keyspace(g, changes::NS),
                before.1,
                "{what}: the journal changed"
            );
            assert_eq!(observe(g), before.2, "{what}: the graph changed");
        }
        bs.push(Boundary {
            rev: g.revision(),
            obs: observe(g),
        });
        let a = check_all(g, &bs, &what);
        answered += a;
        refused += bs.len() - a;
    }
    (bs.iter().map(|b| b.rev).collect(), answered, refused)
}

// ------------------------------------------------------------------ tests

#[test]
fn since_lists_exactly_what_changed_between_any_two_commit_boundaries() {
    let mut g = Graph::memory();
    let (revs, answered, refused) = workload(&mut g, 0x9e37_79b9, 220, None, "memory");
    assert!(revs.windows(2).filter(|w| w[0] != w[1]).count() > 100);
    assert!(answered > 2_000, "only {answered} pairs answered");
    // CLEAR and releases make some revisions unanswerable.
    assert!(refused > 100, "only {refused} refusals");
    let s = changes::status(&g).unwrap();
    assert!(s.entries > 50);
    let took = PATHS.with(|p| p.borrow().clone());
    for path in [
        "CREATE",
        "SET",
        "SET edge",
        "DETACH DELETE",
        "CREATE edge",
        "API",
        "begin block",
        "BEGIN ... COMMIT",
        "batch",
        "import_jsonl",
        "apply_op",
        "import_legacy",
        "CLEAR",
        "ROLLBACK",
        "failing hook",
        "nothing",
        "INDEX",
        "API pair",
        "release",
    ] {
        assert!(
            took.contains(path),
            "the workload never took {path}: {took:?}"
        );
    }
    eprintln!(
        "{answered} pairs answered, {refused} refused, {} entries",
        s.entries
    );
}

#[test]
fn a_small_cap_forgets_the_oldest_and_refuses_what_it_no_longer_holds() {
    let mut g = Graph::memory();
    let (_, answered, refused) = workload(&mut g, 0x1234_5678, 120, Some(40), "cap 40");
    let s = changes::status(&g).unwrap();
    assert!(s.entries <= 40, "{} entries past the cap", s.entries);
    assert_eq!(s.cap, 40);
    assert!(
        answered > 100 && refused > 100,
        "{answered} answered, {refused} refused"
    );
    // The cap survives CLEAR, and lowering it forgets at once.
    q(&mut g, "CLEAR");
    assert_eq!(changes::status(&g).unwrap().cap, 40);
    for i in 0..30 {
        q(&mut g, &format!("CREATE (:C {{i: {i}}})"));
    }
    let held = changes::status(&g).unwrap().entries;
    assert!(held > 16 && held <= 40, "{held}");
    let r = q(&mut g, "CALL changes.cap(entries: 16)");
    let m = r.message.unwrap();
    assert!(m.contains(&format!("{} forgotten", held - 16)), "{m}");
    let s = changes::status(&g).unwrap();
    assert_eq!((s.entries, s.cap, s.why), (16, 16, changes::Floor::Cap));
    let e = query::execute(&mut g, "CALL changes.cap(entries: 15)")
        .err()
        .unwrap();
    assert!(e.to_string().contains("at least 16"), "{e}");
    // Past the cap, a write forgets the oldest down to half of it, and
    // the journal never holds more than the cap.
    for i in 0..200 {
        q(&mut g, &format!("CREATE (:D {{i: {i}}})-[:DD]->(:D)"));
        let n = changes::status(&g).unwrap().entries;
        assert!(n <= 16, "{n} entries past the cap of 16");
    }
}

#[test]
fn the_journal_is_the_same_on_both_tiers() {
    let mut m = Graph::memory();
    let (mrevs, ..) = workload(&mut m, 77, 60, None, "memory");
    let path = temp("tiers");
    let mut f = Graph::open_opts(
        &path,
        OpenOptions {
            sync: Sync::Off,
            page_size: 1024,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    f.set_checkpoint_bytes(16 << 10);
    let (frevs, ..) = workload(&mut f, 77, 60, None, "file");
    assert_eq!(mrevs, frevs, "the same history has the same revisions");
    assert_eq!(keyspace(&m, changes::NS), keyspace(&f, changes::NS));
    for r in [mrevs[0], mrevs[20], mrevs[59]] {
        let a = query::execute(
            &mut m,
            &format!("CALL changes.since(revision: {r}, limit: 5)"),
        );
        let b = query::execute(
            &mut f,
            &format!("CALL changes.since(revision: {r}, limit: 5)"),
        );
        match (a, b) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a.rows, b.rows);
                assert_eq!(a.message, b.message);
            }
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
            _ => panic!("the tiers disagree at revision {r}"),
        }
    }
    drop(f);
    cleanup(&path);
}

#[test]
fn rolled_back_work_leaves_no_entry_including_a_block_the_engine_rolled_back() {
    register();
    let mut g = Graph::memory_with_limit(4 << 20);
    q(&mut g, "CREATE EXTENSION changes");
    q(&mut g, "CREATE EXTENSION witness");
    q(&mut g, "CREATE (:A)-[:E]->(:B)");
    let journal = keyspace(&g, changes::NS);
    let rev = g.revision();
    // The engine rolls back a block that outgrows max_memory: it is spent,
    // and nothing it wrote, hooks included, is left.
    g.begin().unwrap();
    let big = Value::Text("x".repeat(4096));
    let mut full = None;
    for i in 0..10_000 {
        match g.add_node(
            &["Big".into()],
            vec![("b".into(), big.clone()), ("i".into(), Value::Int(i))],
        ) {
            Ok(_) => {}
            Err(e) => {
                full = Some(e);
                break;
            }
        }
    }
    let e = full.expect("the block outgrew max_memory");
    assert!(matches!(e, Error::Full(_)), "{e}");
    assert!(g.spent().is_some());
    assert!(g.commit().is_err());
    assert_eq!(g.revision(), rev);
    assert_eq!(
        keyspace(&g, changes::NS),
        journal,
        "an entry survived the engine's rollback"
    );
    // ROLLBACK, a failing statement, a failing hook in a block.
    for src in ["BEGIN", "CREATE (:Gone)-[:GONE]->(:Gone)", "ROLLBACK"] {
        q(&mut g, src);
    }
    assert!(query::execute(&mut g, "CREATE (:A)-[:E]->(:Fail)").is_err());
    g.begin().unwrap();
    g.add_node(&["Earlier".into()], vec![]).unwrap();
    assert!(g.add_node(&["Fail".into()], vec![]).is_err());
    assert!(g.commit().is_err());
    assert_eq!(g.revision(), rev);
    assert_eq!(keyspace(&g, changes::NS), journal);
    assert_eq!(
        q(&mut g, &format!("CALL changes.since(revision: {rev})"))
            .rows
            .len(),
        0
    );
}

#[test]
fn since_says_plainly_when_it_cannot_answer_and_from_where_it_can() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE (:Before)");
    let r0 = g.revision();
    q(&mut g, "CREATE EXTENSION changes");
    let installed = g.revision();
    // Installed in a statement of its own, the journal answers from the
    // revision before it: nothing else was written in between.
    assert_eq!(changes::oldest(&g), Some((r0, changes::Floor::Installed)));
    assert!(changes::answers(&g, r0) && !changes::answers(&g, r0 - 1));
    let e = query::execute(&mut g, &format!("CALL changes.since(revision: {})", r0 - 1))
        .err()
        .unwrap();
    assert!(
        e.to_string()
            .contains(&format!("installed at revision {r0}")),
        "{e}"
    );
    assert!(
        e.to_string()
            .contains(&format!("CALL changes.since(revision: {r0})")),
        "{e}"
    );
    // Installed inside a block after a write, it cannot answer for the
    // revision before the block.
    let mut h = Graph::memory();
    let before = h.revision();
    for src in [
        "BEGIN",
        "CREATE (:Early)",
        "CREATE EXTENSION changes",
        "CREATE (:Late)",
        "COMMIT",
    ] {
        q(&mut h, src);
    }
    assert!(!changes::answers(&h, before));
    let now = h.revision();
    let list = q(&mut h, &format!("CALL changes.since(revision: {now})"));
    assert!(list.rows.is_empty());
    let floor = changes::oldest(&h).unwrap().0;
    let late = q(&mut h, &format!("CALL changes.since(revision: {floor})"));
    assert_eq!(late.rows.len(), 1, "{:?}", late.rows);

    // Released.
    for i in 0..5 {
        q(&mut g, &format!("CREATE (:N {{i: {i}}})"));
    }
    let mid = g.revision();
    q(&mut g, "MATCH (n:N {i: 0}) SET n.j = 1");
    q(&mut g, &format!("CALL changes.release(below: {mid})"));
    assert_eq!(changes::oldest(&g), Some((mid, changes::Floor::Released)));
    let e = query::execute(
        &mut g,
        &format!("CALL changes.since(revision: {installed})"),
    )
    .err()
    .unwrap();
    assert!(e.to_string().contains("were released"), "{e}");
    let left = q(&mut g, &format!("CALL changes.since(revision: {mid})"));
    assert_eq!(left.rows.len(), 1);
    // A release in slices ends where one whole release would.
    for i in 0..20 {
        q(&mut g, &format!("CREATE (:M {{i: {i}}})"));
    }
    let top = g.revision();
    let mut calls = 0;
    loop {
        calls += 1;
        let m = q(
            &mut g,
            &format!("CALL changes.release(below: {top}, limit: 6)"),
        )
        .message
        .unwrap();
        if !m.contains("more:") {
            break;
        }
    }
    assert_eq!(calls, 4);
    assert_eq!(changes::status(&g).unwrap().entries, 0);
    assert_eq!(changes::oldest(&g).unwrap().0, top);

    // Cleared: everything before must be rebuilt; it answers exactly after.
    q(&mut g, "CREATE (:K)");
    let pre = g.revision();
    q(&mut g, "CLEAR");
    let post = g.revision();
    let e = query::execute(&mut g, &format!("CALL changes.since(revision: {pre})"))
        .err()
        .unwrap();
    assert!(e.to_string().contains("cleared"), "{e}");
    assert!(changes::answers(&g, post));
    q(&mut g, "CREATE (:AfterClear)");
    assert_eq!(
        q(&mut g, &format!("CALL changes.since(revision: {post})"))
            .rows
            .len(),
        1
    );

    // Status.
    let s = q(&mut g, "CALL changes.status()");
    assert_eq!(
        s.columns,
        ["oldest", "why", "entries", "bytes", "cap", "revision"]
    );
    assert_eq!(s.rows[0][2], Value::Int(1));
    let stored: usize = keyspace(&g, changes::NS)
        .iter()
        .map(|(k, v)| changes::NS.len() + 1 + k.len() + v.len())
        .sum();
    assert_eq!(s.rows[0][3], Value::Int(stored as i64));
    assert_eq!(s.rows[0][4], Value::Int(changes::DEFAULT_CAP as i64));
    assert_eq!(s.rows[0][5], Value::Int(g.revision() as i64));
    // Arguments that make no sense say what to pass.
    for (src, says) in [
        ("CALL changes.since()", "needs revision"),
        ("CALL changes.since(revision: -1)", "a whole number"),
        (
            "CALL changes.since(revision: 0, kinds: ['vertex'])",
            "not a kind",
        ),
        (
            "CALL changes.since(revision: 0, after: 'x')",
            "not a position",
        ),
        ("CALL changes.release()", "needs below"),
    ] {
        let e = query::execute(&mut g, src).err().unwrap().to_string();
        assert!(e.contains(says), "{src}: {e}");
    }
}

#[test]
fn adjacency_follows_the_edges_at_a_node_and_whether_the_node_exists() {
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    let a = g.add_node(&["A".into()], vec![]).unwrap();
    let b = g.add_node(&["B".into()], vec![]).unwrap();
    let c = g.add_node(&["C".into()], vec![]).unwrap();
    let r0 = g.revision();
    let e = g.add_edge(a, b, "E", vec![]).unwrap();
    let got = |g: &Graph, since| {
        changes::since(g, since, &[], None, 100)
            .unwrap()
            .0
            .into_iter()
            .map(|e| (e.kind, e.id, e.change))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        got(&g, r0),
        [
            (Kind::Edge, e, Change::Put),
            (Kind::Adjacency, a, Change::Put),
            (Kind::Adjacency, b, Change::Put)
        ]
    );
    // An edge property is an adjacency change at both ends.
    let r1 = g.revision();
    g.set_edge_prop(e, "w", Value::Int(1)).unwrap();
    assert_eq!(got(&g, r1).len(), 3);
    // A replayed edge moved to other endpoints: both old and new ends.
    let r2 = g.revision();
    g.apply_op(Op::EdgeAdd {
        id: e,
        from: c,
        to: c,
        etype: "E".into(),
        props: vec![],
    })
    .unwrap();
    let ids: BTreeSet<u64> = got(&g, r2)
        .into_iter()
        .filter(|x| x.0 == Kind::Adjacency)
        .map(|x| x.1)
        .collect();
    assert_eq!(ids, [a, b, c].into_iter().collect());
    // Deleting a node: its edges first, then the node, and its adjacency
    // entry says it is gone.
    let r3 = g.revision();
    g.delete_node(c).unwrap();
    assert_eq!(
        got(&g, r3),
        [
            (Kind::Node, c, Change::Drop),
            (Kind::Edge, e, Change::Drop),
            (Kind::Adjacency, c, Change::Drop)
        ]
    );
    // A node property is not an adjacency change.
    let r4 = g.revision();
    g.set_node_prop(a, "k", Value::Int(1)).unwrap();
    assert_eq!(got(&g, r4), [(Kind::Node, a, Change::Put)]);
}

/// The writes the replay tests make, after the journal is installed.
fn some_writes(g: &mut Graph) {
    let mut r = Rng(5);
    for _ in 0..80 {
        step(g, &mut r, false);
    }
}

#[test]
fn recovery_and_replicas_hold_the_same_journal_without_running_hooks() {
    register();
    let setup = |g: &mut Graph| {
        q(g, "CREATE EXTENSION witness");
        q(g, "CREATE EXTENSION changes");
        q(g, "CREATE (:Before)-[:K]->(:Before)");
    };
    // A replica: a base taken after the install, then the log after it.
    let mut g = Graph::memory_with_page_size(1024, u64::MAX);
    g.enable_log(glider::storage::log::DEFAULT_LOG_CAP).unwrap();
    setup(&mut g);
    let snap = g.snapshot().unwrap();
    let mut parts: Vec<Vec<u8>> = Vec::new();
    snap.write_image(1 << 20, &mut |_, b| {
        parts.push(b);
        Ok(())
    })
    .unwrap();
    let from = snap.lsn();
    drop(snap);
    some_writes(&mut g);
    let (_, end) = g.log_span().unwrap();
    let log = g.log_range(from, end).unwrap();
    let live = keyspace(&g, changes::NS);
    let heard = keyspace(&g, WITNESS);
    assert!(live.len() > 100, "the journal holds {}", live.len());
    let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
    let (mut m, _) = Graph::from_replica(&refs, &[(from, &log)], u64::MAX, 1 << 20).unwrap();
    assert_eq!(m.revision(), g.revision());
    assert_eq!(keyspace(&m, changes::NS), live);
    // The witness heard nothing during replay: its list is the source's.
    assert_eq!(keyspace(&m, WITNESS), heard);
    let since = format!("CALL changes.since(revision: {from})");
    assert_eq!(q(&mut m, &since).rows, q(&mut g, &since).rows);

    // Crash recovery: the log replayed at open, with no checkpoint after
    // the writes.
    let path = temp("recovery");
    {
        let mut f = Graph::open(&path, Sync::Always).unwrap();
        f.set_checkpoint_bytes(u64::MAX);
        setup(&mut f);
        some_writes(&mut f);
        assert_eq!(keyspace(&f, changes::NS), live);
        std::mem::forget(f);
    }
    let mut f = Graph::open_forced(&path, Sync::Always).unwrap();
    assert_eq!(f.revision(), g.revision());
    assert_eq!(keyspace(&f, changes::NS), live);
    assert_eq!(keyspace(&f, WITNESS), heard);
    assert_eq!(q(&mut f, &since).rows, q(&mut g, &since).rows);
    drop(f);
    cleanup(&path);
}

fn derived_findings(g: &mut Graph) -> Vec<(String, String)> {
    q(g, "CREATE EXTENSION IF NOT EXISTS check");
    q(g, "CALL check.run(checks: ['derived'])")
        .rows
        .into_iter()
        .filter(|r| r[1] != Value::from("summary") && r[2].to_string().starts_with("changes."))
        .map(|r| (r[2].to_string(), r[3].to_string()))
        .collect()
}

#[test]
fn check_run_finds_what_is_wrong_with_the_journal_and_nothing_when_it_is_sound() {
    let mut g = Graph::memory();
    let _ = workload(&mut g, 3, 40, None, "check");
    assert_eq!(derived_findings(&mut g), Vec::<(String, String)>::new());
    // In slices too.
    q(&mut g, "CREATE EXTENSION IF NOT EXISTS check");
    let r = q(&mut g, "CALL check.run(checks: ['derived'], limit: 5)");
    assert!(r.rows.iter().any(|r| r[2] == Value::from("partial")));

    let end = g.log_end();
    let ghost = 1_000_000u64;
    let entry = |kind, id, revision, change| {
        changes::encode_entry(&changes::Entry {
            revision,
            kind,
            id,
            change,
        })
    };
    q(&mut g, "CALL changes.cap(entries: 16)");
    // A put for a node that does not exist, an edge entry past the log's
    // end, a key that does not decode, and more entries than the cap.
    let mut puts = vec![
        entry(Kind::Node, ghost, end, Change::Put),
        entry(Kind::Edge, ghost, end + 1000, Change::Drop),
        (vec![1, 9], b"junk".to_vec()),
    ];
    for i in 0..20 {
        puts.push(entry(Kind::Node, ghost + 1 + i, end, Change::Drop));
    }
    g.ext_write(changes::NS, puts, Vec::new()).unwrap();
    let found = derived_findings(&mut g);
    let kinds: BTreeSet<&str> = found.iter().map(|(k, _)| k.as_str()).collect();
    for k in [
        "changes.missing",
        "changes.future",
        "changes.bad_record",
        "changes.count",
    ] {
        assert!(kinds.contains(k), "{k} not found in {found:?}");
    }
    assert!(found.contains(&("changes.missing".into(), format!("n{ghost}"))));
    assert!(found.contains(&("changes.future".into(), format!("e{ghost}"))));
}

#[test]
fn a_kept_colouring_is_stale_only_when_a_node_or_an_edge_changed_given_the_journal() {
    let build = |g: &mut Graph| {
        q(g, "CREATE (:S)-[:H]->(:W), (:S)-[:H]->(:W)");
        q(g, "CREATE EXTENSION structure");
        q(g, "CALL structure.colors(depth: 2, store: 'c')");
    };
    let fresh = |g: &mut Graph| q(g, "CALL structure.stored()").rows[0][2] == Value::Bool(true);
    // Without the journal: any change makes it stale, an index too.
    let mut plain = Graph::memory();
    build(&mut plain);
    assert!(fresh(&mut plain));
    q(&mut plain, "INDEX ON :S(k)");
    assert!(
        !fresh(&mut plain),
        "without the journal an index makes it stale"
    );

    // With it: an index, another extension's writes, a write that changes
    // nothing, keeping another colouring: still fresh.
    let mut g = Graph::memory();
    q(&mut g, "CREATE EXTENSION changes");
    build(&mut g);
    assert!(fresh(&mut g));
    q(&mut g, "INDEX ON :S(k)");
    q(&mut g, "CREATE EXTENSION context");
    q(&mut g, "CALL structure.colors(depth: 1, store: 'other')");
    q(&mut g, "MATCH (n:S) SET n:S");
    assert!(fresh(&mut g), "nothing a colour reads changed");
    let r = q(&mut g, "CALL structure.read(name: 'c', fresh: 'require')");
    assert!(r.message.unwrap().contains("fresh"));
    // A node or an edge change makes it stale.
    q(&mut g, "MATCH (n:W) SET n.x = 1");
    assert!(!fresh(&mut g));
    q(&mut g, "CALL structure.colors(depth: 2, store: 'c')");
    assert!(fresh(&mut g));
    q(&mut g, "MATCH ()-[e:H]->() SET e.x = 1");
    assert!(!fresh(&mut g));
    // When the journal cannot answer for the colouring's revision, the
    // conservative answer comes back: here, the index makes it stale.
    q(&mut g, "CALL structure.colors(depth: 2, store: 'c')");
    let now = g.revision();
    q(&mut g, &format!("CALL changes.release(below: {now})"));
    assert!(
        !fresh(&mut g),
        "released past it: the journal's own writes count"
    );
    q(&mut g, "CALL structure.colors(depth: 2, store: 'c')");
    assert!(fresh(&mut g));
    q(&mut g, "DROP EXTENSION changes");
    assert!(!fresh(&mut g));
}
