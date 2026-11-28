//! `check`: is the graph whole.
//!
//! `glider <db> verify` proves that pages and trees are sound. This proves
//! that the graph those trees hold agrees with itself, and that what is
//! derived from it still matches it:
//!
//! ```text
//! CREATE EXTENSION check
//! CALL check.run()                                    the checks of the database
//! CALL check.run(checks: ["orphans"], labels: ["Tool"])
//! CALL check.run(checks: ["duplicates"], label: "Tool", keys: ["kind", "maker", "model"])
//! CALL check.run(checks: ["required"], label: "Station", type: "HAS_SIDE", dir: "out")
//! CALL check.run(limit: 100000)                       a slice; continue with after:
//! ```
//!
//! | check | finds |
//! |---|---|
//! | `endpoints` | an edge whose source or target node does not exist |
//! | `adjacency` | an edge missing from an endpoint's adjacency, or an adjacency entry that is not its edge's |
//! | `membership` | a label or type entry without its record, or a record missing from its label or type |
//! | `counters` | label, type, node, edge and index counts that differ from a scan; id counters behind an id in use |
//! | `indexes` | an index entry that does not match its node, or a node missing from an index it belongs in |
//! | `derived` | an installed extension's derived data that disagrees with the graph (each extension checks its own) |
//! | `duplicates` | parallel edges with the same endpoints, type and properties; nodes of `label:` equal on `keys:` |
//! | `orphans` | nodes without edges, of `labels:` when given |
//! | `required` | nodes of `label:` without an edge of `type:` in `dir:` |
//!
//! The first six are faults in the database (`severity: "fault"`) and run
//! when no `checks:` are named. The last three are facts about the data
//! that an application may consider faults (`severity: "candidate"`): they
//! run only when named. Nothing here writes: no node, edge or extension
//! record, and the revision is the same afterwards (D1).
//!
//! Rows are `check, severity, kind, id, detail`: `id` is `n12` for a node,
//! `e12` for an edge, or names the label, type, index or counter at fault.
//! Each check ends with a summary row (`severity: "summary"`, `kind`
//! `done` or `partial`, `detail` `{"records":N,"problems":M}`). Checks run in
//! the order of the table, each walking its trees in key order, so rows come
//! out the same on every run and on both tiers (D7).
//!
//! **Slices (D6).** `limit:` caps the records one call looks at (and the
//! rows it returns at 10,000). A call that stops early ends with the summary
//! of the check it stopped in, `kind: "partial"`, and its `id` is the
//! cursor: pass it back as `after:` with the same arguments and the next
//! call carries on from the next record. The pieces' rows together are
//! exactly a whole run's, and their records and problems add up to it.
//! State that a check carries from one record to the next (a running count)
//! travels in the cursor, so a call holds nothing between slices. Memory per
//! call is a few batches of records, and where a check must sort (parallel
//! edges of a hub, node duplicates) an external sort within
//! `algorithm_memory()`.

use std::collections::BTreeMap;

use super::{Args, Extension, Spec};
use crate::codec;
use crate::graph::{Error, Graph, RawTree, Result};
use crate::query::QueryResult;
use crate::storage::extsort::Sorter;
use crate::storage::keys;
use crate::value::Value;

pub struct Check;

const PROCEDURES: &[Spec] = &[Spec {
    name: "run",
    args: "(checks, after, limit, label, keys, labels, type, dir)",
    doc: "Check that the graph agrees with itself and with what is derived from it; rows check, severity, kind, id, detail; repairs nothing",
}];

/// Every check, in the order they run.
pub const CHECKS: &[&str] = &[
    "endpoints",
    "adjacency",
    "membership",
    "counters",
    "indexes",
    "derived",
    "duplicates",
    "orphans",
    "required",
];

/// How many of [`CHECKS`] run when none are named: the database's own.
const OF_THE_DATABASE: usize = 6;

/// Rows one call returns at most; past it the call stops as at its limit.
pub const MAX_ROWS: usize = 10_000;

/// Records read from a tree at a time.
const BATCH: usize = 512;
const BATCH_BYTES: usize = 1 << 20;

impl Extension for Check {
    fn name(&self) -> &str {
        "check"
    }
    fn version(&self) -> &str {
        "1.0"
    }
    fn summary(&self) -> &str {
        "is the graph whole: edges, adjacency, membership, counts, indexes and derived data checked against each other; repairs nothing"
    }
    fn procedures(&self) -> &[Spec] {
        PROCEDURES
    }
    fn call(&self, g: &mut Graph, procedure: &str, a: &Args) -> Result<QueryResult> {
        match procedure {
            "run" => run(g, a),
            _ => unreachable!("dispatch checks the procedure list"),
        }
    }
    fn check(&self, _g: &Graph, _probe: &mut Probe) -> Result<bool> {
        // Keeps nothing.
        Ok(true)
    }
}

// --------------------------------------------------------------- the probe

/// One thing an extension found wrong with its own derived data.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub kind: String,
    pub id: String,
    pub detail: String,
}

/// What `check` hands an extension asked to check its derived data
/// ([`Extension::check`]). The extension walks what it keeps, calls
/// [`look`](Probe::look) before each record and [`found`](Probe::found) for
/// each problem; when `look` says the slice is full it calls
/// [`stop_at`](Probe::stop_at) with whatever lets it continue, and gets it
/// back from [`after`](Probe::after) in the next call.
pub struct Probe {
    after: Option<Vec<u8>>,
    left: u64,
    rows_left: usize,
    looked: u64,
    findings: Vec<Finding>,
    stop: Option<Vec<u8>>,
    memory: u64,
}

impl Probe {
    /// A probe for a whole run, for tests and callers outside `check.run`.
    pub fn whole(memory: u64) -> Probe {
        Probe::new(None, u64::MAX, usize::MAX, memory)
    }

    fn new(after: Option<Vec<u8>>, left: u64, rows_left: usize, memory: u64) -> Probe {
        Probe {
            after,
            left,
            rows_left,
            looked: 0,
            findings: Vec::new(),
            stop: None,
            memory,
        }
    }

    /// Where the previous slice stopped, as given to [`stop_at`](Probe::stop_at);
    /// None: from the start.
    pub fn after(&self) -> Option<&[u8]> {
        self.after.as_deref()
    }

    /// Count one record about to be looked at. False: this slice is full;
    /// do not look at it, call [`stop_at`](Probe::stop_at) and return.
    pub fn look(&mut self) -> Result<bool> {
        // one statement step per record (the statement budget)
        crate::meter::step()?;
        if self.left == 0 || self.findings.len() >= self.rows_left {
            return Ok(false);
        }
        self.left -= 1;
        self.looked += 1;
        Ok(true)
    }

    pub fn found(&mut self, kind: &str, id: impl Into<String>, detail: impl Into<String>) {
        self.findings.push(Finding {
            kind: kind.to_string(),
            id: id.into(),
            detail: detail.into(),
        });
    }

    /// End this slice; the next one starts from `cursor`.
    pub fn stop_at(&mut self, cursor: Vec<u8>) {
        self.stop = Some(cursor);
    }

    /// Bytes the extension may hold for its check.
    pub fn memory(&self) -> u64 {
        self.memory
    }

    pub fn looked(&self) -> u64 {
        self.looked
    }

    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    pub fn stopped(&self) -> Option<&[u8]> {
        self.stop.as_deref()
    }
}

// -------------------------------------------------------------- the cursor

/// Where a slice stopped: the check, its phase, the last key it looked at
/// (None: the phase had not started), and what it was carrying.
#[derive(Clone, Debug, Default, PartialEq)]
struct Cursor {
    check: usize,
    phase: u32,
    key: Option<Vec<u8>>,
    carry: Vec<u64>,
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

impl Cursor {
    fn encode(&self) -> String {
        let key = match &self.key {
            Some(k) => hex(k),
            None => "-".into(),
        };
        let carry: Vec<String> = self.carry.iter().map(|c| c.to_string()).collect();
        format!(
            "{}:{}:{}:{}",
            CHECKS[self.check],
            self.phase,
            key,
            carry.join(",")
        )
    }

    fn decode(s: &str) -> Option<Cursor> {
        let mut parts = s.split(':');
        let name = parts.next()?;
        let check = CHECKS.iter().position(|c| *c == name)?;
        let phase = parts.next()?.parse().ok()?;
        let key = match parts.next()? {
            "-" => None,
            h => Some(unhex(h)?),
        };
        let carry = match parts.next()? {
            "" => Vec::new(),
            c => c
                .split(',')
                .map(|x| x.parse().ok())
                .collect::<Option<Vec<u64>>>()?,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Cursor {
            check,
            phase,
            key,
            carry,
        })
    }

    /// Carried value `i`, 0 when absent.
    fn get(&self, i: usize) -> u64 {
        self.carry.get(i).copied().unwrap_or(0)
    }

    fn set(&mut self, i: usize, v: u64) {
        if self.carry.len() <= i {
            self.carry.resize(i + 1, 0);
        }
        self.carry[i] = v;
    }

    /// On to the next phase, from its start.
    fn next_phase(&mut self) {
        self.phase += 1;
        self.key = None;
    }
}

// ----------------------------------------------------------------- the run

/// The data checks' arguments.
struct Opts {
    label: Option<String>,
    keys: Vec<String>,
    labels: Vec<String>,
    etype: Option<String>,
    dir: crate::graph::Dir,
}

/// One call's state.
struct Run<'g> {
    g: &'g Graph,
    left: u64,
    rows: Vec<Vec<Value>>,
    /// The check running, its records and problems in this call.
    check: usize,
    looked: u64,
    problems: u64,
    /// Bytes the check may hold (sorts spill past it).
    memory: u64,
}

/// How a phase ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flow {
    Done,
    Stopped,
}

impl Run<'_> {
    /// Count one record about to be looked at. False: the slice is full.
    fn look(&mut self) -> Result<bool> {
        // one statement step per record (the statement budget)
        crate::meter::step()?;
        if self.left == 0 || self.rows.len() >= MAX_ROWS {
            return Ok(false);
        }
        self.left -= 1;
        self.looked += 1;
        Ok(true)
    }

    fn row(&mut self, severity: &str, kind: &str, id: String, detail: String) {
        self.rows.push(vec![
            Value::Text(CHECKS[self.check].into()),
            Value::Text(severity.into()),
            Value::Text(kind.into()),
            Value::Text(id),
            Value::Text(detail),
        ]);
    }

    fn fault(&mut self, kind: &str, id: String, detail: String) {
        self.problems += 1;
        self.row("fault", kind, id, detail);
    }

    fn candidate(&mut self, kind: &str, id: String, detail: String) {
        self.problems += 1;
        self.row("candidate", kind, id, detail);
    }

    fn info(&mut self, kind: &str, id: String, detail: String) {
        self.row("info", kind, id, detail);
    }

    fn name(&self, id: u32) -> String {
        if (id as usize) < self.g.strings.len() {
            self.g.strings.name(id).to_string()
        } else {
            format!("#{id}")
        }
    }

    /// Walk `tree` under `prefix` from the cursor's key, a batch at a time,
    /// giving `f` each entry `look` lets through and moving the cursor past
    /// it.
    fn walk(
        &mut self,
        cur: &mut Cursor,
        tree: RawTree,
        prefix: &[u8],
        f: &mut dyn FnMut(&mut Run<'_>, &mut Cursor, &[u8], &[u8]) -> Result<()>,
    ) -> Result<Flow> {
        loop {
            let batch = self
                .g
                .raw_batch(tree, prefix, cur.key.as_deref(), BATCH, BATCH_BYTES);
            if batch.is_empty() {
                return Ok(Flow::Done);
            }
            for (k, v) in batch {
                if !self.look()? {
                    return Ok(Flow::Stopped);
                }
                f(self, cur, &k, &v)?;
                cur.key = Some(k);
            }
        }
    }
}

fn node_ref(id: u64) -> String {
    format!("n{id}")
}

fn edge_ref(id: u64) -> String {
    format!("e{id}")
}

fn u64_at(k: &[u8], at: usize) -> Option<u64> {
    (k.len() >= at + 8).then(|| keys::get_u64(k, at))
}

fn u32_at(k: &[u8], at: usize) -> Option<u32> {
    (k.len() >= at + 4).then(|| keys::get_u32(k, at))
}

fn count_val(v: &[u8]) -> u64 {
    match v.get(..8) {
        Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
        None => 0,
    }
}

fn cat_key(tag: u8, a: u32) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend_from_slice(&a.to_be_bytes());
    k
}

fn pair_key(a: u32, b: u64) -> Vec<u8> {
    let mut k = a.to_be_bytes().to_vec();
    k.extend_from_slice(&b.to_be_bytes());
    k
}

fn adj_key(node: u64, dir: u8, t: u32, edge: u64) -> Vec<u8> {
    let mut k = node.to_be_bytes().to_vec();
    k.push(dir);
    k.extend_from_slice(&t.to_be_bytes());
    k.extend_from_slice(&edge.to_be_bytes());
    k
}

fn varint(v: &[u8]) -> Option<u64> {
    codec::Reader::new(v).varint().ok()
}

fn json_counts(records: u64, problems: u64) -> String {
    format!("{{\"records\":{records},\"problems\":{problems}}}")
}

fn run(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let named = a.strs("checks");
    let mut selected = vec![false; CHECKS.len()];
    if named.is_empty() {
        for s in selected.iter_mut().take(OF_THE_DATABASE) {
            *s = true;
        }
    }
    for n in &named {
        let n = n.to_ascii_lowercase();
        match CHECKS.iter().position(|c| *c == n) {
            Some(i) => selected[i] = true,
            None => {
                return Err(Error::Msg(format!(
                    "check.run has no check '{n}'; the checks are {}",
                    CHECKS.join(", ")
                )))
            }
        }
    }
    let dir = match a.str("dir") {
        None => crate::graph::Dir::Out,
        Some(d) => crate::graph::Dir::parse(&d).ok_or_else(|| {
            Error::Msg(format!(
                "dir: '{d}' is not a direction; use out, in or both"
            ))
        })?,
    };
    let opts = Opts {
        label: a.str("label"),
        keys: a.strs("keys"),
        labels: a.strs("labels"),
        etype: a.str("type"),
        dir,
    };
    if selected[8] && (opts.label.is_none() || opts.etype.is_none()) {
        return Err(Error::Msg(
            "required needs label: and type: (and dir:, out by default): the nodes of a label that must have an edge of a type".into(),
        ));
    }
    if selected[6] && !opts.keys.is_empty() && opts.label.is_none() {
        return Err(Error::Msg(
            "duplicates compares the nodes of one label on keys:; give label: as well".into(),
        ));
    }
    let limit = match a.get("limit") {
        None | Some(Value::Null) => u64::MAX,
        Some(v) => match v.as_i64() {
            Some(n) if n >= 1 => n as u64,
            _ => {
                return Err(Error::Msg(format!(
                    "limit: {v} is not a number of records; give 1 or more"
                )))
            }
        },
    };
    let mut cur = match a.str("after") {
        None => None,
        Some(s) => Some(Cursor::decode(&s).ok_or_else(|| {
            Error::Msg(format!(
                "after: '{s}' is not a cursor; pass the id of the partial summary row check.run returned"
            ))
        })?),
    };
    if let Some(c) = &cur {
        if !selected[c.check] {
            return Err(Error::Msg(format!(
                "after: continues the {} check, which these checks: do not include; pass the same arguments as the call that returned it",
                CHECKS[c.check]
            )));
        }
    }
    let g: &Graph = g;
    let mut run = Run {
        g,
        left: limit,
        rows: Vec::new(),
        check: 0,
        looked: 0,
        problems: 0,
        memory: (g.algorithm_memory() / 2).max(64 << 10),
    };
    let (mut total, mut problems, mut ran) = (0u64, 0u64, 0usize);
    let mut stopped: Option<String> = None;
    for (i, &on) in selected.iter().enumerate() {
        if !on {
            continue;
        }
        let mut c = match cur.take() {
            Some(c) if c.check > i => {
                cur = Some(c);
                continue;
            }
            Some(c) => c,
            None => Cursor {
                check: i,
                ..Cursor::default()
            },
        };
        run.check = i;
        run.looked = 0;
        run.problems = 0;
        let flow = match i {
            0 => endpoints(&mut run, &mut c)?,
            1 => adjacency(&mut run, &mut c)?,
            2 => membership(&mut run, &mut c)?,
            3 => counters(&mut run, &mut c)?,
            4 => indexes(&mut run, &mut c)?,
            5 => derived(&mut run, &mut c)?,
            6 => duplicates(&mut run, &mut c, &opts)?,
            7 => orphans(&mut run, &mut c, &opts)?,
            _ => required(&mut run, &mut c, &opts)?,
        };
        ran += 1;
        total += run.looked;
        problems += run.problems;
        let counts = json_counts(run.looked, run.problems);
        if flow == Flow::Stopped {
            let at = c.encode();
            run.rows.push(vec![
                Value::Text(CHECKS[i].into()),
                Value::Text("summary".into()),
                Value::Text("partial".into()),
                Value::Text(at.clone()),
                Value::Text(counts),
            ]);
            stopped = Some(at);
            break;
        }
        run.rows.push(vec![
            Value::Text(CHECKS[i].into()),
            Value::Text("summary".into()),
            Value::Text("done".into()),
            Value::Null,
            Value::Text(counts),
        ]);
    }
    let mut r = QueryResult::table(
        vec![
            "check".into(),
            "severity".into(),
            "kind".into(),
            "id".into(),
            "detail".into(),
        ],
        run.rows,
    );
    r.message = Some(match stopped {
        Some(at) => format!(
            "stopped after {total} records ({problems} problems) at the slice's limit; CALL check.run again with after: \"{at}\" and the same arguments to continue"
        ),
        None => format!("{ran} checks looked at {total} records: {problems} problems"),
    });
    Ok(r)
}

// ---------------------------------------------------------------- endpoints

fn endpoints(run: &mut Run<'_>, cur: &mut Cursor) -> Result<Flow> {
    run.walk(cur, RawTree::Edges, &[], &mut |run, _, k, v| {
        let Some(id) = u64_at(k, 0) else {
            run.fault(
                "bad_record",
                hex(k),
                "an edge record with a malformed key".into(),
            );
            return Ok(());
        };
        let Some((from, to, t, _)) = Graph::raw_decode_edge(v) else {
            run.fault(
                "bad_record",
                edge_ref(id),
                format!("edge e{id} does not decode"),
            );
            return Ok(());
        };
        for (end, n, kind) in [
            ("starts at", from, "missing_source"),
            ("ends at", to, "missing_target"),
        ] {
            if run.g.raw_get(RawTree::Nodes, &n.to_be_bytes()).is_none() {
                let tn = run.name(t);
                run.fault(
                    kind,
                    edge_ref(id),
                    format!("edge e{id} [:{tn}] {end} n{n}, which does not exist"),
                );
            }
        }
        Ok(())
    })
}

// ---------------------------------------------------------------- adjacency

fn adjacency(run: &mut Run<'_>, cur: &mut Cursor) -> Result<Flow> {
    if cur.phase == 0 {
        // Each edge is listed by both its ends.
        let f = run.walk(cur, RawTree::Edges, &[], &mut |run, _, k, v| {
            let (Some(id), Some((from, to, t, _))) = (u64_at(k, 0), Graph::raw_decode_edge(v))
            else {
                return Ok(()); // endpoints reports it
            };
            for (node, dir, other, side) in [(from, 0u8, to, "source"), (to, 1, from, "target")] {
                let tn = run.name(t);
                match run.g.raw_get(RawTree::Adj, &adj_key(node, dir, t, id)) {
                    None => run.fault(
                        "missing_entry",
                        edge_ref(id),
                        format!("edge e{id} [:{tn}] is missing from the adjacency of its {side} n{node}"),
                    ),
                    Some(v) if varint(&v) != Some(other) => run.fault(
                        "wrong_entry",
                        edge_ref(id),
                        format!(
                            "the adjacency of n{node} lists edge e{id} [:{tn}] as reaching {}, not n{other}",
                            varint(&v).map(node_ref).unwrap_or_else(|| "nothing readable".into())
                        ),
                    ),
                    Some(_) => {}
                }
            }
            Ok(())
        })?;
        if f == Flow::Stopped {
            return Ok(f);
        }
        cur.next_phase();
    }
    // Each adjacency entry is its edge's.
    run.walk(cur, RawTree::Adj, &[], &mut |run, _, k, v| {
        let (Some(node), Some(&dir), Some(t), Some(edge)) =
            (u64_at(k, 0), k.get(8), u32_at(k, 9), u64_at(k, 13))
        else {
            run.fault("bad_entry", hex(k), "an adjacency entry with a malformed key".into());
            return Ok(());
        };
        let other = varint(v);
        let tn = run.name(t);
        let way = if dir == 0 { "outgoing" } else { "incoming" };
        match run
            .g
            .raw_get(RawTree::Edges, &edge.to_be_bytes())
            .and_then(|b| Graph::raw_decode_edge(&b))
        {
            None => run.fault(
                "no_edge",
                node_ref(node),
                format!("the adjacency of n{node} lists {way} edge e{edge} [:{tn}], which does not exist"),
            ),
            Some((from, to, et, _)) => {
                let ok = et == t
                    && k.len() == 21
                    && match dir {
                        0 => from == node && Some(to) == other,
                        1 => to == node && Some(from) == other,
                        _ => false,
                    };
                if !ok {
                    let real = run.name(et);
                    run.fault(
                        "stale_entry",
                        node_ref(node),
                        format!(
                            "the adjacency of n{node} lists {way} edge e{edge} [:{tn}], which is n{from} -[:{real}]-> n{to}"
                        ),
                    );
                }
            }
        }
        Ok(())
    })
}

// --------------------------------------------------------------- membership

fn membership(run: &mut Run<'_>, cur: &mut Cursor) -> Result<Flow> {
    loop {
        let f = match cur.phase {
            // Every label of a node is listed under the label.
            0 => run.walk(cur, RawTree::Nodes, &[], &mut |run, _, k, v| {
                let (Some(id), Some((labels, _))) = (u64_at(k, 0), Graph::raw_decode_node(v))
                else {
                    run.fault(
                        "bad_record",
                        hex(k),
                        "a node record that does not decode".into(),
                    );
                    return Ok(());
                };
                for l in labels {
                    if run.g.raw_get(RawTree::Labels, &pair_key(l, id)).is_none() {
                        let ln = run.name(l);
                        run.fault(
                            "missing_member",
                            node_ref(id),
                            format!("n{id} has label :{ln} but is not listed under it"),
                        );
                    }
                }
                Ok(())
            })?,
            // Everything listed under a label is a node with it.
            1 => run.walk(cur, RawTree::Labels, &[], &mut |run, _, k, _| {
                let (Some(l), Some(id)) = (u32_at(k, 0), u64_at(k, 4)) else {
                    run.fault(
                        "bad_entry",
                        hex(k),
                        "a label entry with a malformed key".into(),
                    );
                    return Ok(());
                };
                let ln = run.name(l);
                match run
                    .g
                    .raw_get(RawTree::Nodes, &id.to_be_bytes())
                    .and_then(|b| Graph::raw_decode_node(&b))
                {
                    None => run.fault(
                        "no_record",
                        node_ref(id),
                        format!("label :{ln} lists n{id}, which does not exist"),
                    ),
                    Some((labels, _)) if !labels.contains(&l) => run.fault(
                        "stale_member",
                        node_ref(id),
                        format!("label :{ln} lists n{id}, which does not have it"),
                    ),
                    Some(_) => {}
                }
                Ok(())
            })?,
            // Every edge is listed under its type.
            2 => run.walk(cur, RawTree::Edges, &[], &mut |run, _, k, v| {
                let (Some(id), Some((_, _, t, _))) = (u64_at(k, 0), Graph::raw_decode_edge(v))
                else {
                    return Ok(());
                };
                if run.g.raw_get(RawTree::Types, &pair_key(t, id)).is_none() {
                    let tn = run.name(t);
                    run.fault(
                        "missing_member",
                        edge_ref(id),
                        format!("e{id} has type :{tn} but is not listed under it"),
                    );
                }
                Ok(())
            })?,
            // Everything listed under a type is an edge of it.
            3 => run.walk(cur, RawTree::Types, &[], &mut |run, _, k, _| {
                let (Some(t), Some(id)) = (u32_at(k, 0), u64_at(k, 4)) else {
                    run.fault(
                        "bad_entry",
                        hex(k),
                        "a type entry with a malformed key".into(),
                    );
                    return Ok(());
                };
                let tn = run.name(t);
                match run
                    .g
                    .raw_get(RawTree::Edges, &id.to_be_bytes())
                    .and_then(|b| Graph::raw_decode_edge(&b))
                {
                    None => run.fault(
                        "no_record",
                        edge_ref(id),
                        format!("type :{tn} lists e{id}, which does not exist"),
                    ),
                    Some((_, _, et, _)) if et != t => run.fault(
                        "stale_member",
                        edge_ref(id),
                        format!("type :{tn} lists e{id}, which is of type :{}", run.name(et)),
                    ),
                    Some(_) => {}
                }
                Ok(())
            })?,
            _ => return Ok(Flow::Done),
        };
        if f == Flow::Stopped {
            return Ok(f);
        }
        cur.next_phase();
    }
}

// ----------------------------------------------------------------- counters

/// The catalog's count for a label (`L`) or type (`T`).
fn catalog_count(g: &Graph, tag: u8, id: u32) -> u64 {
    g.raw_get(RawTree::Catalog, &cat_key(tag, id))
        .map(|v| count_val(&v))
        .unwrap_or(0)
}

/// An index's catalog statistics: (distinct values, entries).
fn index_stats(g: &Graph, l: u32, k: u32) -> Option<(u64, u64)> {
    let mut key = vec![b'I'];
    key.extend_from_slice(&l.to_be_bytes());
    key.extend_from_slice(&k.to_be_bytes());
    let v = g.raw_get(RawTree::Catalog, &key)?;
    let d = u64::from_le_bytes(v.get(4..12)?.try_into().ok()?);
    let e = u64::from_le_bytes(v.get(12..20)?.try_into().ok()?);
    Some((d, e))
}

fn counters(run: &mut Run<'_>, cur: &mut Cursor) -> Result<Flow> {
    loop {
        let f = match cur.phase {
            // Members of each label and type, against the catalog's count.
            // Carries (label + 1, members so far).
            0 | 2 => {
                let (tree, tag, what) = if cur.phase == 0 {
                    (RawTree::Labels, b'L', "label")
                } else {
                    (RawTree::Types, b'T', "type")
                };
                let compare = move |run: &mut Run<'_>, id: u32, n: u64| {
                    let c = catalog_count(run.g, tag, id);
                    if c != n {
                        let name = run.name(id);
                        let things = if tag == b'L' { "nodes" } else { "edges" };
                        run.fault(
                            &format!("{what}_count"),
                            format!(":{name}"),
                            format!("the catalog counts {c} {things} with {what} :{name}; {n} are listed"),
                        );
                    }
                };
                let f = run.walk(cur, tree, &[], &mut |run, cur, k, _| {
                    let Some(id) = u32_at(k, 0) else {
                        return Ok(()); // membership reports it
                    };
                    if cur.get(0) != id as u64 + 1 {
                        if cur.get(0) > 0 {
                            compare(run, (cur.get(0) - 1) as u32, cur.get(1));
                        }
                        cur.set(0, id as u64 + 1);
                        cur.set(1, 0);
                    }
                    cur.set(1, cur.get(1) + 1);
                    Ok(())
                })?;
                if f == Flow::Done && cur.get(0) > 0 {
                    compare(run, (cur.get(0) - 1) as u32, cur.get(1));
                }
                f
            }
            // Catalog counts of labels and types that list nothing.
            1 | 3 => {
                let (tree, tag, what) = if cur.phase == 1 {
                    (RawTree::Labels, b'L', "label")
                } else {
                    (RawTree::Types, b'T', "type")
                };
                run.walk(cur, RawTree::Catalog, &[tag], &mut |run, _, k, v| {
                    let Some(id) = u32_at(k, 1) else {
                        return Ok(());
                    };
                    let c = count_val(v);
                    if c > 0
                        && run
                            .g
                            .raw_batch(tree, &id.to_be_bytes(), None, 1, BATCH_BYTES)
                            .is_empty()
                    {
                        let name = run.name(id);
                        run.fault(
                            &format!("{what}_count"),
                            format!(":{name}"),
                            format!("the catalog counts {c} with {what} :{name}; none are listed"),
                        );
                    }
                    Ok(())
                })?
            }
            // Nodes and edges, against their counts; the id counters past
            // every id. Carries (records, highest id + 1).
            4 | 5 => {
                let edges = cur.phase == 5;
                let tree = if edges {
                    RawTree::Edges
                } else {
                    RawTree::Nodes
                };
                let f = run.walk(cur, tree, &[], &mut |_, cur, k, _| {
                    cur.set(0, cur.get(0) + 1);
                    if let Some(id) = u64_at(k, 0) {
                        cur.set(1, cur.get(1).max(id.saturating_add(1)));
                    }
                    Ok(())
                })?;
                if f == Flow::Done {
                    let m = run.g.raw_meta();
                    let (count, next, what) = if edges {
                        (m.edges, m.next_edge, "edge")
                    } else {
                        (m.nodes, m.next_node, "node")
                    };
                    let (n, high) = (cur.get(0), cur.get(1));
                    if count != n {
                        run.fault(
                            &format!("{what}_count"),
                            format!("{what}s"),
                            format!("the {what} count is {count}; {n} {what}s exist"),
                        );
                    }
                    if high > 0 && next < high {
                        let p = if edges { "e" } else { "n" };
                        run.fault(
                            &format!("next_{what}"),
                            format!("next_{what}"),
                            format!(
                                "the next {what} id is {next}, but {p}{} exists: a new {what} would take an id in use",
                                high - 1
                            ),
                        );
                    }
                }
                f
            }
            // Entries and distinct values of each index, against its
            // statistics. Carries (index + 1, entries, distinct).
            6 => {
                let defs = run.g.raw_indexes();
                let compare = |run: &mut Run<'_>, ix: u32, entries: u64, distinct: u64| {
                    let Some(&(_, l, k)) = defs.iter().find(|d| d.0 == ix) else {
                        return; // indexes reports entries of no index
                    };
                    let (sd, se) = index_stats(run.g, l, k).unwrap_or((0, 0));
                    if (sd, se) != (distinct, entries) {
                        let name = format!(":{}({})", run.name(l), run.name(k));
                        run.fault(
                            "index_count",
                            name.clone(),
                            format!(
                                "index {name} counts {se} entries and {sd} distinct values; it holds {entries} and {distinct}"
                            ),
                        );
                    }
                };
                let f = run.walk(cur, RawTree::Props, &[], &mut |run, cur, k, v| {
                    let Some(ix) = u32_at(k, 0) else {
                        return Ok(());
                    };
                    if k.len() < 12 {
                        return Ok(());
                    }
                    let prev = cur.key.clone();
                    if cur.get(0) != ix as u64 + 1 {
                        if cur.get(0) > 0 {
                            compare(run, (cur.get(0) - 1) as u32, cur.get(1), cur.get(2));
                        }
                        cur.set(0, ix as u64 + 1);
                        cur.set(1, 0);
                        cur.set(2, 0);
                    }
                    cur.set(1, cur.get(1) + 1);
                    if fresh_value(run.g, &defs, k, v, prev.as_deref()) {
                        cur.set(2, cur.get(2) + 1);
                    }
                    Ok(())
                })?;
                if f == Flow::Done && cur.get(0) > 0 {
                    compare(run, (cur.get(0) - 1) as u32, cur.get(1), cur.get(2));
                }
                f
            }
            // Indexes that hold nothing, and the index id counter.
            7 => {
                let defs = run.g.raw_indexes();
                let next = run.g.raw_meta().next_index;
                let from = cur.get(0) as usize;
                for (i, &(ix, l, k)) in defs.iter().enumerate().skip(from) {
                    if !run.look()? {
                        return Ok(Flow::Stopped);
                    }
                    let name = format!(":{}({})", run.name(l), run.name(k));
                    if ix as u64 >= next {
                        run.fault(
                            "next_index",
                            name.clone(),
                            format!("index {name} has id {ix}, but the next index id is {next}"),
                        );
                    }
                    let empty = run
                        .g
                        .raw_batch(RawTree::Props, &ix.to_be_bytes(), None, 1, BATCH_BYTES)
                        .is_empty();
                    let (sd, se) = index_stats(run.g, l, k).unwrap_or((0, 0));
                    if empty && (sd, se) != (0, 0) {
                        run.fault(
                            "index_count",
                            name.clone(),
                            format!("index {name} counts {se} entries and {sd} distinct values; it holds none"),
                        );
                    }
                    cur.set(0, i as u64 + 1);
                }
                Flow::Done
            }
            _ => return Ok(Flow::Done),
        };
        if f == Flow::Stopped {
            return Ok(f);
        }
        cur.next_phase();
        cur.carry.clear();
    }
}

/// The full value of an index entry, read from its node.
fn entry_value(g: &Graph, defs: &[(u32, u32, u32)], k: &[u8]) -> Option<Value> {
    let ix = u32_at(k, 0)?;
    let node = u64_at(k, k.len().checked_sub(8)?)?;
    let &(_, _, key) = defs.iter().find(|d| d.0 == ix)?;
    let (_, props) = Graph::raw_decode_node(&g.raw_get(RawTree::Nodes, &node.to_be_bytes())?)?;
    props.into_iter().find(|(p, _)| *p == key).map(|(_, v)| v)
}

/// Whether an index entry holds a value no earlier entry of its index
/// holds. Entries are in value order, so a value stored whole is new when
/// the entry before has another; a value cut to fit a key shares its stored
/// prefix with other long values, so it is compared, read from the nodes,
/// with each earlier entry under the same prefix (as the engine does when
/// it counts).
fn fresh_value(
    g: &Graph,
    defs: &[(u32, u32, u32)],
    k: &[u8],
    v: &[u8],
    prev: Option<&[u8]>,
) -> bool {
    let prefix = &k[..k.len() - 8];
    let same_prefix = prev.is_some_and(|p| p.len() >= 8 && &p[..p.len() - 8] == prefix);
    if !same_prefix {
        return true;
    }
    if v.first() != Some(&1) {
        return false;
    }
    let Some(mine) = entry_value(g, defs, k) else {
        return true;
    };
    let mut after: Option<Vec<u8>> = None;
    loop {
        let batch = g.raw_batch(RawTree::Props, prefix, after.as_deref(), BATCH, BATCH_BYTES);
        if batch.is_empty() {
            return true;
        }
        for (ek, _) in batch {
            if ek.as_slice() >= k {
                return true;
            }
            if ek.len() == k.len()
                && entry_value(g, defs, &ek)
                    .is_some_and(|x| x.total_cmp(&mine) == std::cmp::Ordering::Equal)
            {
                return false;
            }
            after = Some(ek);
        }
    }
}

// ------------------------------------------------------------------ indexes

fn indexes(run: &mut Run<'_>, cur: &mut Cursor) -> Result<Flow> {
    let defs = run.g.raw_indexes();
    if cur.phase == 0 {
        // Every entry matches its node.
        let f = run.walk(cur, RawTree::Props, &[], &mut |run, _, k, v| {
            let (Some(ix), Some(node)) = (
                u32_at(k, 0),
                k.len().checked_sub(8).and_then(|at| u64_at(k, at)),
            ) else {
                run.fault(
                    "bad_entry",
                    hex(k),
                    "an index entry with a malformed key".into(),
                );
                return Ok(());
            };
            let Some(&(_, l, key)) = defs.iter().find(|d| d.0 == ix) else {
                run.fault(
                    "no_index",
                    node_ref(node),
                    format!("an entry for n{node} belongs to index #{ix}, which is not defined"),
                );
                return Ok(());
            };
            let name = format!(":{}({})", run.name(l), run.name(key));
            let rec = run
                .g
                .raw_get(RawTree::Nodes, &node.to_be_bytes())
                .and_then(|b| Graph::raw_decode_node(&b));
            let why = match rec {
                None => Some(format!("index {name} lists n{node}, which does not exist")),
                Some((labels, _)) if !labels.contains(&l) => Some(format!(
                    "index {name} lists n{node}, which does not have label :{}",
                    run.name(l)
                )),
                Some((_, props)) => match props.iter().find(|(p, _)| *p == key) {
                    None => Some(format!(
                        "index {name} lists n{node}, which has no {}",
                        run.name(key)
                    )),
                    Some((_, value)) => {
                        let (prefix, truncated) = run.g.raw_index_prefix(ix, value);
                        let stored = &k[..k.len() - 8];
                        if stored != prefix.as_slice() || v.first() != Some(&(truncated as u8)) {
                            Some(format!(
                                "index {name} lists n{node} under another value than its {} {}",
                                run.name(key),
                                short(value)
                            ))
                        } else {
                            None
                        }
                    }
                },
            };
            if let Some(why) = why {
                run.fault("stale_entry", node_ref(node), why);
            }
            Ok(())
        })?;
        if f == Flow::Stopped {
            return Ok(f);
        }
        cur.next_phase();
        cur.carry.clear();
    }
    // Every node an index covers is in it. Carries the index's position.
    let from = cur.get(0) as usize;
    for (i, &(ix, l, key)) in defs.iter().enumerate().skip(from) {
        if i as u64 != cur.get(0) {
            cur.set(0, i as u64);
            cur.key = None;
        }
        let name = format!(":{}({})", run.name(l), run.name(key));
        let f = run.walk(
            cur,
            RawTree::Labels,
            &l.to_be_bytes(),
            &mut |run, _, k, _| {
                let Some(node) = u64_at(k, 4) else {
                    return Ok(());
                };
                let Some((_, props)) = run
                    .g
                    .raw_get(RawTree::Nodes, &node.to_be_bytes())
                    .and_then(|b| Graph::raw_decode_node(&b))
                else {
                    return Ok(()); // membership reports it
                };
                if let Some((_, value)) = props.iter().find(|(p, _)| *p == key) {
                    let (mut entry, _) = run.g.raw_index_prefix(ix, value);
                    entry.extend_from_slice(&node.to_be_bytes());
                    if run.g.raw_get(RawTree::Props, &entry).is_none() {
                        run.fault(
                            "missing_entry",
                            node_ref(node),
                            format!(
                                "n{node} has {} {} but index {name} does not list it",
                                run.name(key),
                                short(value)
                            ),
                        );
                    }
                }
                Ok(())
            },
        )?;
        if f == Flow::Stopped {
            return Ok(f);
        }
    }
    Ok(Flow::Done)
}

/// A value for a message, cut short.
fn short(v: &Value) -> String {
    let s = match v {
        Value::Text(t) => format!("{t:?}"),
        v => v.to_string(),
    };
    if s.chars().count() > 48 {
        let mut t: String = s.chars().take(45).collect();
        t.push_str("...");
        t
    } else {
        s
    }
}

// ------------------------------------------------------------------ derived

fn derived(run: &mut Run<'_>, cur: &mut Cursor) -> Result<Flow> {
    let mut installed = run.g.extensions();
    installed.sort();
    let from = cur.get(0) as usize;
    for (i, (name, _)) in installed.iter().enumerate().skip(from) {
        let after = if i as u64 == cur.get(0) {
            cur.key.take()
        } else {
            None
        };
        cur.set(0, i as u64);
        cur.key = None;
        let Some(ext) = super::find(name) else {
            run.info(
                "not_available",
                name.clone(),
                format!("extension {name} is installed but not in this build, so its derived data cannot be checked"),
            );
            continue;
        };
        let mut probe = Probe::new(
            after,
            run.left,
            MAX_ROWS.saturating_sub(run.rows.len()),
            run.memory,
        );
        let checks = ext.check(run.g, &mut probe)?;
        run.left -= probe.looked.min(run.left);
        run.looked += probe.looked;
        if !checks {
            run.info(
                "not_checked",
                name.clone(),
                format!("extension {name} does not say how to check what it derives"),
            );
            continue;
        }
        for f in probe.findings {
            run.fault(&format!("{name}.{}", f.kind), f.id, f.detail);
        }
        if let Some(at) = probe.stop {
            cur.key = Some(at);
            return Ok(Flow::Stopped);
        }
    }
    Ok(Flow::Done)
}

// --------------------------------------------------------------- duplicates

/// An edge's properties in one canonical form: by key, values encoded.
fn props_key(props: &mut [(u32, Value)]) -> Vec<u8> {
    props.sort_by_key(|(k, _)| *k);
    let mut out = Vec::new();
    for (k, v) in props.iter() {
        out.extend_from_slice(&k.to_be_bytes());
        codec::put_value(&mut out, v);
    }
    out
}

fn sorter(run: &Run<'_>, name: &str) -> Result<Sorter> {
    Sorter::new(&run.g.temp_dir(), name, run.memory as usize).map_err(Error::Io)
}

fn duplicates(run: &mut Run<'_>, cur: &mut Cursor, o: &Opts) -> Result<Flow> {
    if cur.phase == 0 {
        // Parallel edges: per source node, its outgoing edges by (type,
        // target), and among those that share both, by properties.
        let f = run.walk(cur, RawTree::Nodes, &[], &mut |run, _, k, _| {
            let Some(node) = u64_at(k, 0) else {
                return Ok(());
            };
            let mut prefix = node.to_be_bytes().to_vec();
            prefix.push(0);
            let mut s = sorter(run, "check-out")?;
            let mut after: Option<Vec<u8>> = None;
            let mut any = 0usize;
            loop {
                let batch =
                    run.g
                        .raw_batch(RawTree::Adj, &prefix, after.as_deref(), BATCH, BATCH_BYTES);
                if batch.is_empty() {
                    break;
                }
                for (ak, av) in batch {
                    if let (Some(t), Some(e), Some(to)) =
                        (u32_at(&ak, 9), u64_at(&ak, 13), varint(&av))
                    {
                        let mut key = t.to_be_bytes().to_vec();
                        key.extend_from_slice(&to.to_be_bytes());
                        key.extend_from_slice(&e.to_be_bytes());
                        s.push(&key, &[]).map_err(Error::Io)?;
                        any += 1;
                    }
                    after = Some(ak);
                }
            }
            if any < 2 {
                return Ok(());
            }
            let mut sorted = s.finish().map_err(Error::Io)?;
            let mut group: Vec<u64> = Vec::new();
            let mut at: Option<[u8; 12]> = None;
            loop {
                let next = sorted.next().map_err(Error::Io)?;
                let pair: Option<[u8; 12]> =
                    next.as_ref().map(|(k, _)| k[..12].try_into().unwrap());
                if pair != at {
                    if group.len() > 1 {
                        let p = at.unwrap();
                        parallel(
                            run,
                            node,
                            u32::from_be_bytes(p[..4].try_into().unwrap()),
                            u64::from_be_bytes(p[4..].try_into().unwrap()),
                            &group,
                        )?;
                    }
                    group.clear();
                    at = pair;
                }
                match next {
                    Some((k, _)) => group.push(keys::get_u64(&k, 12)),
                    None => break,
                }
            }
            Ok(())
        })?;
        if f == Flow::Stopped {
            return Ok(f);
        }
        cur.next_phase();
        cur.carry.clear();
    }
    let (Some(label), false) = (&o.label, o.keys.is_empty()) else {
        return Ok(Flow::Done);
    };
    // Nodes of a label equal on the keys: sorted by their values, so equal
    // ones are adjacent. The sort is made again by every slice; a slice
    // bounds the records it reports, not the reading. Carries the first
    // member of the group the cursor is in.
    let Some(l) = run.g.strings.lookup(label) else {
        return Ok(Flow::Done);
    };
    let key_ids: Vec<Option<u32>> = o.keys.iter().map(|k| run.g.strings.lookup(k)).collect();
    if key_ids.iter().any(|k| k.is_none()) {
        return Ok(Flow::Done); // no node has every key
    }
    let key_ids: Vec<u32> = key_ids.into_iter().flatten().collect();
    let mut s = sorter(run, "check-dup")?;
    let mut after: Option<Vec<u8>> = None;
    loop {
        let batch = run.g.raw_batch(
            RawTree::Labels,
            &l.to_be_bytes(),
            after.as_deref(),
            BATCH,
            BATCH_BYTES,
        );
        if batch.is_empty() {
            break;
        }
        for (k, _) in batch {
            if let Some(node) = u64_at(&k, 4) {
                if let Some((_, props)) = run
                    .g
                    .raw_get(RawTree::Nodes, &node.to_be_bytes())
                    .and_then(|b| Graph::raw_decode_node(&b))
                {
                    let mut key = Vec::new();
                    let mut all = true;
                    for kid in &key_ids {
                        match props.iter().find(|(p, _)| p == kid) {
                            Some((_, v)) => keys::put_value(&mut key, v),
                            None => {
                                all = false;
                                break;
                            }
                        }
                    }
                    if all {
                        key.extend_from_slice(&node.to_be_bytes());
                        s.push(&key, &[]).map_err(Error::Io)?;
                    }
                }
            }
            after = Some(k);
        }
    }
    let mut sorted = s.finish().map_err(Error::Io)?;
    let keys_list = o.keys.join(", ");
    while let Some((k, _)) = sorted.next().map_err(Error::Io)? {
        if cur
            .key
            .as_ref()
            .is_some_and(|c| k.as_slice() <= c.as_slice())
        {
            continue;
        }
        if !run.look()? {
            return Ok(Flow::Stopped);
        }
        let node = keys::get_u64(&k, k.len() - 8);
        let same = cur
            .key
            .as_ref()
            .is_some_and(|c| c.len() == k.len() && c[..c.len() - 8] == k[..k.len() - 8]);
        if same {
            let first = cur.get(0);
            run.candidate(
                "duplicate_node",
                node_ref(node),
                format!("n{node} agrees with n{first} on {keys_list}"),
            );
        } else {
            cur.set(0, node);
        }
        cur.key = Some(k);
    }
    Ok(Flow::Done)
}

/// Report the edges of one (source, type, target) that repeat an earlier
/// one's properties.
fn parallel(run: &mut Run<'_>, from: u64, t: u32, to: u64, edges: &[u64]) -> Result<()> {
    let mut by_props: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    let tn = run.name(t);
    // Edges of one pair are few; a pair with very many is sorted instead.
    if edges.len() * 64 > run.memory as usize {
        let mut s = sorter(run, "check-pair")?;
        for &e in edges {
            if let Some((_, _, _, mut p)) = run
                .g
                .raw_get(RawTree::Edges, &e.to_be_bytes())
                .and_then(|b| Graph::raw_decode_edge(&b))
            {
                let mut key = props_key(&mut p);
                key.extend_from_slice(&e.to_be_bytes());
                s.push(&key, &[]).map_err(Error::Io)?;
            }
        }
        let mut sorted = s.finish().map_err(Error::Io)?;
        let mut prev: Option<(Vec<u8>, u64)> = None;
        while let Some((k, _)) = sorted.next().map_err(Error::Io)? {
            let e = keys::get_u64(&k, k.len() - 8);
            let pk = k[..k.len() - 8].to_vec();
            match &prev {
                Some((p, first)) if *p == pk => {
                    let first = *first;
                    run.candidate("parallel_edge", edge_ref(e), format!("e{e} repeats e{first}: n{from} -[:{tn}]-> n{to} with the same properties"));
                }
                _ => prev = Some((pk, e)),
            }
        }
        return Ok(());
    }
    for &e in edges {
        let Some((_, _, _, mut p)) = run
            .g
            .raw_get(RawTree::Edges, &e.to_be_bytes())
            .and_then(|b| Graph::raw_decode_edge(&b))
        else {
            continue;
        };
        let pk = props_key(&mut p);
        match by_props.get(&pk) {
            Some(&first) => run.candidate(
                "parallel_edge",
                edge_ref(e),
                format!("e{e} repeats e{first}: n{from} -[:{tn}]-> n{to} with the same properties"),
            ),
            None => {
                by_props.insert(pk, e);
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------- orphans, required

/// Walk the nodes of any of `labels` (every node when empty) in id order,
/// once each. The cursor's key is the last node id looked at.
fn each_node(
    run: &mut Run<'_>,
    cur: &mut Cursor,
    labels: &[u32],
    f: &mut dyn FnMut(&mut Run<'_>, u64),
) -> Result<Flow> {
    if labels.is_empty() {
        return run.walk(cur, RawTree::Nodes, &[], &mut |run, _, k, _| {
            if let Some(id) = u64_at(k, 0) {
                f(run, id);
            }
            Ok(())
        });
    }
    loop {
        let after = cur.key.as_deref().and_then(|k| u64_at(k, 0));
        // The next batch of each label after the cursor, merged; ids past
        // the end of a label's batch wait for the next round.
        let mut ids: Vec<u64> = Vec::new();
        let mut horizon = u64::MAX;
        for &l in labels {
            let from = after.map(|a| pair_key(l, a));
            let batch = run.g.raw_batch(
                RawTree::Labels,
                &l.to_be_bytes(),
                from.as_deref(),
                BATCH,
                BATCH_BYTES,
            );
            if batch.len() == BATCH {
                if let Some(last) = batch.last().and_then(|(k, _)| u64_at(k, 4)) {
                    horizon = horizon.min(last);
                }
            }
            ids.extend(batch.iter().filter_map(|(k, _)| u64_at(k, 4)));
        }
        ids.sort_unstable();
        ids.dedup();
        ids.retain(|&i| i <= horizon);
        if ids.is_empty() {
            return Ok(Flow::Done);
        }
        for id in ids {
            if !run.look()? {
                return Ok(Flow::Stopped);
            }
            f(run, id);
            cur.key = Some(id.to_be_bytes().to_vec());
        }
    }
}

fn orphans(run: &mut Run<'_>, cur: &mut Cursor, o: &Opts) -> Result<Flow> {
    let labels: Vec<u32> = o
        .labels
        .iter()
        .filter_map(|l| run.g.strings.lookup(l))
        .collect();
    if !o.labels.is_empty() && labels.is_empty() {
        return Ok(Flow::Done);
    }
    each_node(run, cur, &labels, &mut |run, id| {
        let edges = !run
            .g
            .raw_batch(RawTree::Adj, &id.to_be_bytes(), None, 1, BATCH_BYTES)
            .is_empty();
        if !edges {
            let labels = run.g.node_labels(id);
            let ls: String = labels.iter().map(|l| format!(":{l}")).collect();
            run.candidate("orphan", node_ref(id), format!("n{id}{ls} has no edges"));
        }
    })
}

fn required(run: &mut Run<'_>, cur: &mut Cursor, o: &Opts) -> Result<Flow> {
    let (Some(label), Some(etype)) = (&o.label, &o.etype) else {
        return Ok(Flow::Done);
    };
    let Some(l) = run.g.strings.lookup(label) else {
        return Ok(Flow::Done);
    };
    let t = run.g.strings.lookup(etype);
    let dirs: &[u8] = match o.dir {
        crate::graph::Dir::Out => &[0],
        crate::graph::Dir::In => &[1],
        crate::graph::Dir::Both => &[0, 1],
    };
    let way = match o.dir {
        crate::graph::Dir::Out => "outgoing ",
        crate::graph::Dir::In => "incoming ",
        crate::graph::Dir::Both => "",
    };
    let (label, etype) = (label.clone(), etype.clone());
    each_node(run, cur, &[l], &mut |run, id| {
        let has = t.is_some_and(|t| {
            dirs.iter().any(|&d| {
                let mut prefix = id.to_be_bytes().to_vec();
                prefix.push(d);
                prefix.extend_from_slice(&t.to_be_bytes());
                !run.g
                    .raw_batch(RawTree::Adj, &prefix, None, 1, BATCH_BYTES)
                    .is_empty()
            })
        });
        if !has {
            run.candidate(
                "missing_edge",
                node_ref(id),
                format!("n{id}:{label} has no {way}:{etype} edge"),
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_reads_back_as_written() {
        for c in [
            Cursor::default(),
            Cursor {
                check: 3,
                phase: 6,
                key: Some(vec![0, 1, 0xfe, 0xff]),
                carry: vec![1, 0, u64::MAX],
            },
        ] {
            assert_eq!(Cursor::decode(&c.encode()), Some(c));
        }
        assert_eq!(Cursor::decode("nothing:1:-:"), None);
        assert_eq!(Cursor::decode("counters:1:abc:"), None);
    }
}
