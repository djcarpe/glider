//! `changes`: which nodes and edges changed, and at what revision.
//!
//! ```text
//! CREATE EXTENSION changes                     start a journal at the current revision
//! CALL changes.since(revision: 1042)           ids changed after a revision, oldest change first
//! CALL changes.since(revision: 1042, kinds: ["node"], after: "1100:node:7", limit: 1000)
//! CALL changes.status()                        what it can answer for, entries, bytes, cap
//! CALL changes.release(below: 1500)            forget changes at or below a revision
//! CALL changes.cap(entries: 100000)            bound the journal
//! ```
//!
//! For every committed change to a node or an edge, by any path, the
//! journal holds an entry for that id: what it is (`node` or `edge`), what
//! happened to it last (`put`: it exists with what it has now; `drop`: it
//! is gone) and the revision of that change. A node whose edges changed
//! (an edge added, removed or altered at either end) also has an
//! `adjacency` entry, whose change is `put` while the node exists and
//! `drop` once it is gone. An id appears once per kind, with its latest
//! change. Old values are not kept: the graph has the current ones.
//!
//! **What revision an entry carries.** A transaction's final revision is
//! not known while its hooks run, so an entry carries
//! [`Graph::log_end`] as the hook saw it: the log position just past the
//! record of the write it describes. Every record of a transaction lies
//! after the revision committed before it began and before its own commit
//! marker, so a change committed by the transaction that took the database
//! from revision R1 to R2 carries a revision r with R1 < r < R2. Hence, for
//! a caller who reads `revision` at commit boundaries, `since(R1)` taken at
//! R2 lists exactly the ids written by the transactions committed after R1
//! (each once, at its latest change), and nothing written at or before R1.
//!
//! **Bounds.** The journal holds at most `cap` entries (default
//! [`DEFAULT_CAP`], at least [`MIN_CAP`]); past it the oldest entries are
//! forgotten. `release(below:)` forgets entries at or below a revision.
//! Either raises the journal's *floor*, the oldest revision it can answer
//! for; so do its installation (it knows nothing before it) and `CLEAR`
//! (everything before it must be rebuilt). `since(R)` with R below the
//! floor is an error that says why and names the floor: it never returns a
//! partial list as if it were whole.
//!
//! **Cost.** Each node or edge write adds one extension record to the
//! log (one `ext_write` with the entries, their revision index and the
//! journal's state), and reads the state and the entries it replaces.
//!
//! **Keys** (keyspace `changes`): `!cap` -> the cap (configuration, so it
//! survives `CLEAR`); `s` -> the state ([`State`]); `i<kind><id>` ->
//! revision, change; `r<revision><kind><id>` -> change, the index `since`
//! reads in order. Kinds are 0 node, 1 edge, 2 adjacency.
//!
//! Every loop over entries passes through [`step`], the one place the
//! statement budget (slice G2) is to be counted.

use std::collections::BTreeSet;

use super::{Args, EdgeImage, Extension, NodeImage, Spec};
use crate::graph::{Error, Graph, Result};
use crate::query::QueryResult;
use crate::value::Value;

pub struct Changes;

/// The keyspace, and the extension's name.
pub const NS: &str = "changes";

/// Entries the journal holds when no cap was set.
pub const DEFAULT_CAP: u64 = 1_000_000;

/// The smallest cap: one write makes at most this many entries (an edge
/// moved by a replayed `EdgeAdd` over an existing id: itself and four
/// endpoints), so eviction never has to forget the entries it is writing.
pub const MIN_CAP: u64 = 8;

/// Rows one `since` call returns at most.
pub const MAX_ROWS: usize = 10_000;

/// Entries deleted per write by `release` and `cap`.
const BATCH: usize = 4_096;

/// Bytes the keyspace holds per entry: its `i` and `r` records, keys
/// (with the namespace the tree stores them under) and values.
pub const ENTRY_BYTES: u64 = 2 * (NS.len() as u64 + 1) + (1 + 1 + 8 + 9) + (1 + 8 + 1 + 8 + 1);

const CAP_KEY: &[u8] = b"!cap";
const STATE_KEY: &[u8] = b"s";

const PROCEDURES: &[Spec] = &[
    Spec {
        name: "since",
        args: "(revision, kinds, after, limit)",
        doc: "Nodes, edges and adjacencies changed after a revision, once each with its latest change (put or drop), oldest first; an error when the journal cannot answer for it",
    },
    Spec {
        name: "status",
        args: "()",
        doc: "The oldest revision the journal answers for and why, its entries, bytes and cap, and the current revision",
    },
    Spec {
        name: "release",
        args: "(below, limit)",
        doc: "Forget changes at or below a revision; the journal then answers only from it",
    },
    Spec {
        name: "cap",
        args: "(entries)",
        doc: "Set the most entries the journal holds; past it the oldest are forgotten",
    },
];

/// What an entry is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A node's labels or properties, or whether it exists.
    Node,
    /// An edge: its endpoints, type or properties, or whether it exists.
    Edge,
    /// The edges at a node: one added, removed or altered at either end.
    Adjacency,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Node, Kind::Edge, Kind::Adjacency];

    fn byte(self) -> u8 {
        match self {
            Kind::Node => 0,
            Kind::Edge => 1,
            Kind::Adjacency => 2,
        }
    }

    fn of(b: u8) -> Option<Kind> {
        match b {
            0 => Some(Kind::Node),
            1 => Some(Kind::Edge),
            2 => Some(Kind::Adjacency),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Node => "node",
            Kind::Edge => "edge",
            Kind::Adjacency => "adjacency",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }
}

/// What happened to an id last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// It exists, with what the graph holds now.
    Put,
    /// It is gone.
    Drop,
}

impl Change {
    fn byte(self) -> u8 {
        match self {
            Change::Drop => 0,
            Change::Put => 1,
        }
    }

    fn of(b: u8) -> Option<Change> {
        match b {
            0 => Some(Change::Drop),
            1 => Some(Change::Put),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Change::Put => "put",
            Change::Drop => "drop",
        }
    }
}

/// One entry: an id's latest change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The log position just past the change's record (see the module
    /// documentation): above the revision committed before its
    /// transaction, below the one its transaction committed at.
    pub revision: u64,
    pub kind: Kind,
    pub id: u64,
    pub change: Change,
}

impl Entry {
    /// Where a slice that ended at this entry continues, for `after:`.
    pub fn cursor(&self) -> String {
        format!("{}:{}:{}", self.revision, self.kind.name(), self.id)
    }
}

/// Why the journal cannot answer for revisions below its floor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Floor {
    /// It was installed there and knows nothing before.
    Installed,
    /// `release` forgot what was at or below it.
    Released,
    /// The cap pushed out entries at or below it.
    Cap,
    /// The graph was cleared (or another bulk write made) there.
    Cleared,
    /// Its state was missing and was started again there.
    Reset,
}

impl Floor {
    fn byte(self) -> u8 {
        match self {
            Floor::Installed => 0,
            Floor::Released => 1,
            Floor::Cap => 2,
            Floor::Cleared => 3,
            Floor::Reset => 4,
        }
    }

    fn of(b: u8) -> Option<Floor> {
        match b {
            0 => Some(Floor::Installed),
            1 => Some(Floor::Released),
            2 => Some(Floor::Cap),
            3 => Some(Floor::Cleared),
            4 => Some(Floor::Reset),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Floor::Installed => "installed",
            Floor::Released => "released",
            Floor::Cap => "cap",
            Floor::Cleared => "cleared",
            Floor::Reset => "reset",
        }
    }

    fn why(self, floor: u64, cap: u64) -> String {
        match self {
            Floor::Installed => format!("it was installed at revision {floor} and knows nothing before"),
            Floor::Released => format!("changes at or below revision {floor} were released"),
            Floor::Cap => format!("its cap of {cap} entries pushed out changes at or below revision {floor}"),
            Floor::Cleared => format!("the graph was cleared at revision {floor}, so everything before it must be rebuilt"),
            Floor::Reset => format!("its state was lost and started again at revision {floor}"),
        }
    }
}

/// The journal's state, one record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    /// Entries held (`i` records).
    pub entries: u64,
    pub cap: u64,
    /// The oldest revision it answers for: `since(R)` needs R >= floor.
    pub floor: u64,
    pub why: Floor,
    /// The latest revision of a change of each kind (0: none since the
    /// floor), for [`changed_any`].
    pub latest: [u64; 3],
}

impl State {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(50);
        out.push(1u8);
        out.extend_from_slice(&self.entries.to_be_bytes());
        out.extend_from_slice(&self.cap.to_be_bytes());
        out.extend_from_slice(&self.floor.to_be_bytes());
        out.push(self.why.byte());
        for l in self.latest {
            out.extend_from_slice(&l.to_be_bytes());
        }
        out
    }

    fn decode(b: &[u8]) -> Option<State> {
        if b.len() != 50 || b[0] != 1 {
            return None;
        }
        Some(State {
            entries: u64_at(b, 1),
            cap: u64_at(b, 9),
            floor: u64_at(b, 17),
            why: Floor::of(b[25])?,
            latest: [u64_at(b, 26), u64_at(b, 34), u64_at(b, 42)],
        })
    }
}

/// G2 (statement budgets): one statement step per entry looked at goes
/// here, the only place this extension's loops count.
#[inline]
fn step(_n: u64) -> Result<()> {
    Ok(())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

fn id_key(kind: Kind, id: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(10);
    k.push(b'i');
    k.push(kind.byte());
    k.extend_from_slice(&id.to_be_bytes());
    k
}

fn rev_key(rev: u64, kind: Kind, id: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(18);
    k.push(b'r');
    k.extend_from_slice(&rev.to_be_bytes());
    k.push(kind.byte());
    k.extend_from_slice(&id.to_be_bytes());
    k
}

fn id_value(rev: u64, change: Change) -> Vec<u8> {
    let mut v = rev.to_be_bytes().to_vec();
    v.push(change.byte());
    v
}

/// An `i` record: (kind, id) -> (revision, change).
fn decode_id(k: &[u8], v: &[u8]) -> Option<Entry> {
    if k.len() != 10 || k[0] != b'i' || v.len() != 9 {
        return None;
    }
    Some(Entry {
        revision: u64_at(v, 0),
        kind: Kind::of(k[1])?,
        id: u64_at(k, 2),
        change: Change::of(v[8])?,
    })
}

/// An `r` record: (revision, kind, id) -> change.
fn decode_rev(k: &[u8], v: &[u8]) -> Option<Entry> {
    if k.len() != 18 || k[0] != b'r' || v.len() != 1 {
        return None;
    }
    Some(Entry {
        revision: u64_at(k, 1),
        kind: Kind::of(k[9])?,
        id: u64_at(k, 10),
        change: Change::of(v[0])?,
    })
}

fn installed(g: &Graph) -> bool {
    g.extension_version(NS).is_some()
}

fn cap_config(g: &Graph) -> u64 {
    g.ext_get(NS, CAP_KEY)
        .filter(|v| v.len() == 8)
        .map(|v| u64_at(&v, 0))
        .unwrap_or(DEFAULT_CAP)
}

/// The state as kept; a missing or unreadable one starts again at the
/// log's end, knowing nothing before it.
fn state_or_reset(g: &Graph) -> State {
    g.ext_get(NS, STATE_KEY)
        .and_then(|v| State::decode(&v))
        .unwrap_or_else(|| State {
            entries: 0,
            cap: cap_config(g),
            floor: g.log_end(),
            why: Floor::Reset,
            latest: [0; 3],
        })
}

// ------------------------------------------------------------- the Rust API

/// The journal's state, or None when it is not installed (or its state is
/// unreadable, which `check.run` reports).
pub fn status(g: &Graph) -> Option<State> {
    if !installed(g) {
        return None;
    }
    State::decode(&g.ext_get(NS, STATE_KEY)?)
}

/// The oldest revision the journal can answer for, and why it is that one;
/// None when it is not installed.
pub fn oldest(g: &Graph) -> Option<(u64, Floor)> {
    status(g).map(|s| (s.floor, s.why))
}

/// Whether the journal is installed and can answer for `since`: every
/// change after it is still listed.
pub fn answers(g: &Graph, since: u64) -> bool {
    status(g).is_some_and(|s| since >= s.floor)
}

/// The latest change to one id of `kind`, if the journal holds one.
pub fn entry(g: &Graph, kind: Kind, id: u64) -> Option<Entry> {
    let v = g.ext_get(NS, &id_key(kind, id))?;
    decode_id(&id_key(kind, id), &v)
}

/// Which of `ids` (of `kind`) changed after revision `since`, one bool
/// each; None when the journal is not installed or cannot answer for
/// `since`, so the caller must assume all of them did.
pub fn changed(g: &Graph, since: u64, kind: Kind, ids: &[u64]) -> Option<Vec<bool>> {
    if !answers(g, since) {
        return None;
    }
    let mut out = Vec::with_capacity(ids.len());
    for &id in ids {
        out.push(entry(g, kind, id).is_some_and(|e| e.revision > since));
    }
    Some(out)
}

/// Whether anything of `kind` changed after revision `since`; None when
/// the journal is not installed or cannot answer for `since`.
pub fn changed_any(g: &Graph, since: u64, kind: Kind) -> Option<bool> {
    let s = status(g)?;
    (since >= s.floor).then(|| s.latest[kind.byte() as usize] > since)
}

/// The entries changed after revision `since`, oldest change first (by
/// revision, then kind, then id), starting after `after` (an entry's
/// position, from [`Entry::cursor`]), of `kinds` (all when empty), at most
/// `limit`; and whether more follow. An error when the journal is not
/// installed or cannot answer for `since`.
pub fn since(
    g: &Graph,
    since: u64,
    kinds: &[Kind],
    after: Option<(u64, Kind, u64)>,
    limit: usize,
) -> Result<(Vec<Entry>, bool)> {
    let Some(s) = status(g) else {
        return Err(Error::Msg(if installed(g) {
            "the change journal's state does not decode; CALL check.run(checks: [\"derived\"]) reports it, and DROP EXTENSION changes then CREATE EXTENSION changes starts it again".into()
        } else {
            "extension 'changes' is not installed in this database; run CREATE EXTENSION changes"
                .into()
        }));
    };
    if since < s.floor {
        return Err(Error::Msg(format!(
            "the change journal cannot answer for revision {since}: {}; it answers for revision {} and later. Rebuild what was derived from a scan of the graph, then ask CALL changes.since(revision: {}) or later",
            s.why.why(s.floor, s.cap),
            s.floor,
            s.floor
        )));
    }
    let start = match after {
        Some((r, k, id)) if r > since => rev_key(r, k, id),
        // Past every entry at or below `since`.
        _ => {
            let mut k = vec![b'r'];
            k.extend_from_slice(&since.to_be_bytes());
            k.extend_from_slice(&[0xff; 9]);
            k
        }
    };
    let mut out = Vec::new();
    let mut more = false;
    let mut bad = false;
    let mut looked = 0u64;
    g.ext_scan_after(NS, b"r", Some(&start), &mut |k, v| {
        looked += 1;
        let Some(e) = decode_rev(k, v) else {
            bad = true;
            return false;
        };
        if !kinds.is_empty() && !kinds.contains(&e.kind) {
            return true;
        }
        if out.len() == limit {
            more = true;
            return false;
        }
        out.push(e);
        true
    });
    step(looked)?;
    if bad {
        return Err(Error::Msg(
            "the change journal holds an entry that does not decode; CALL check.run(checks: [\"derived\"]) names it".into(),
        ));
    }
    Ok((out, more))
}

// --------------------------------------------------------------- recording

/// Record `entries` (kind, id, change) at the log's end: one write with the
/// entries, their revision index and the state, forgetting the oldest
/// entries past the cap.
fn note(g: &mut Graph, entries: &[(Kind, u64, Change)]) -> Result<()> {
    let mut s = state_or_reset(g);
    let rev = g.log_end();
    let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(2 * entries.len() + 1);
    let mut dels: Vec<Vec<u8>> = Vec::new();
    let mut replaced: Vec<Vec<u8>> = Vec::new();
    for &(kind, id, change) in entries {
        step(1)?;
        let ik = id_key(kind, id);
        match g.ext_get(NS, &ik).and_then(|v| decode_id(&ik, &v)) {
            Some(old) => {
                let rk = rev_key(old.revision, kind, id);
                if old.revision != rev {
                    dels.push(rk.clone());
                }
                replaced.push(rk);
            }
            None => s.entries += 1,
        }
        puts.push((ik, id_value(rev, change)));
        puts.push((rev_key(rev, kind, id), vec![change.byte()]));
        s.latest[kind.byte() as usize] = rev;
    }
    if s.entries > s.cap {
        // The oldest entries that are not being rewritten here. A write
        // makes at most MIN_CAP entries, so with entries <= cap before it
        // there are always enough of them.
        let excess = (s.entries - s.cap) as usize;
        let mut gone: Vec<Entry> = Vec::with_capacity(excess);
        g.ext_scan(NS, b"r", &mut |k, v| {
            if replaced.iter().any(|r| r == k) {
                return true;
            }
            if let Some(e) = decode_rev(k, v) {
                gone.push(e);
            }
            gone.len() < excess
        });
        step(gone.len() as u64)?;
        for e in &gone {
            dels.push(rev_key(e.revision, e.kind, e.id));
            dels.push(id_key(e.kind, e.id));
            s.entries -= 1;
            if e.revision >= s.floor {
                s.floor = e.revision;
                s.why = Floor::Cap;
            }
        }
    }
    puts.push((STATE_KEY.to_vec(), s.encode()));
    g.ext_write(NS, puts, dels)
}

/// The adjacency entry's change for node `n`: put while it exists.
fn adjacency(g: &Graph, n: u64) -> Change {
    if g.node(n).is_some() {
        Change::Put
    } else {
        Change::Drop
    }
}

/// Forget entries in key order from `start` while `take` says so, a batch
/// per write, raising the floor to the revision of each one forgotten.
/// Returns how many went and whether the walk ended by `take` (or a
/// limit) rather than by running out.
fn forget(
    g: &mut Graph,
    s: &mut State,
    why: Floor,
    limit: u64,
    take: &dyn Fn(&Entry) -> bool,
) -> Result<(u64, bool)> {
    let mut gone = 0u64;
    loop {
        let room = (limit - gone).min(BATCH as u64) as usize;
        if room == 0 {
            return Ok((gone, true));
        }
        let mut batch: Vec<Entry> = Vec::new();
        let mut stopped = false;
        g.ext_scan(NS, b"r", &mut |k, v| {
            match decode_rev(k, v) {
                Some(e) if take(&e) => batch.push(e),
                Some(_) => {
                    stopped = true;
                    return false;
                }
                // An undecodable entry stops the walk; check.run names it.
                None => {
                    stopped = true;
                    return false;
                }
            }
            batch.len() < room
        });
        step(batch.len() as u64)?;
        let mut dels = Vec::with_capacity(2 * batch.len());
        for e in &batch {
            dels.push(rev_key(e.revision, e.kind, e.id));
            dels.push(id_key(e.kind, e.id));
            s.entries = s.entries.saturating_sub(1);
            if e.revision >= s.floor {
                s.floor = e.revision;
                s.why = why;
            }
        }
        gone += batch.len() as u64;
        let done = stopped || batch.len() < room;
        if !batch.is_empty() || done {
            g.ext_write(NS, vec![(STATE_KEY.to_vec(), s.encode())], dels)?;
        }
        if stopped {
            return Ok((gone, true));
        }
        if batch.len() < room {
            return Ok((gone, false));
        }
    }
}

// --------------------------------------------------------------- procedures

fn revision_arg(a: &Args, key: &str, what: &str) -> Result<u64> {
    match a.get(key) {
        Some(Value::Int(i)) if *i >= 0 => Ok(*i as u64),
        Some(Value::Float(f)) if *f >= 0.0 && f.fract() == 0.0 => Ok(*f as u64),
        None | Some(Value::Null) => Err(Error::Msg(format!(
            "{what} needs {key}: a revision, as STATS or CALL changes.status() reports it"
        ))),
        Some(v) => Err(Error::Msg(format!(
            "{key}: expected a revision (a whole number at least 0), got {v}"
        ))),
    }
}

fn limit_arg(a: &Args, max: usize) -> Result<usize> {
    match a.get("limit") {
        None | Some(Value::Null) => Ok(max),
        Some(Value::Int(i)) if *i >= 1 => Ok((*i as u64).min(max as u64) as usize),
        Some(v) => Err(Error::Msg(format!(
            "limit: expected a whole number from 1 to {max}, got {v}"
        ))),
    }
}

fn kinds_arg(a: &Args) -> Result<Vec<Kind>> {
    let mut out = Vec::new();
    for s in a.strs("kinds") {
        let k = Kind::parse(&s.to_ascii_lowercase()).ok_or_else(|| {
            Error::Msg(format!(
                "kinds: '{s}' is not a kind; use \"node\", \"edge\" or \"adjacency\""
            ))
        })?;
        if !out.contains(&k) {
            out.push(k);
        }
    }
    Ok(out)
}

fn after_arg(a: &Args) -> Result<Option<(u64, Kind, u64)>> {
    let Some(s) = a.str("after") else {
        return Ok(None);
    };
    let bad = || {
        Error::Msg(format!(
            "after: '{s}' is not a position; pass back the one the previous call's message gave (revision:kind:id)"
        ))
    };
    let mut parts = s.split(':');
    let r = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    let k = parts.next().and_then(Kind::parse).ok_or_else(bad)?;
    let id = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    if parts.next().is_some() {
        return Err(bad());
    }
    Ok(Some((r, k, id)))
}

fn since_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let r = revision_arg(a, "revision", "since")?;
    let kinds = kinds_arg(a)?;
    let after = after_arg(a)?;
    let limit = limit_arg(a, MAX_ROWS)?;
    let (list, more) = since(g, r, &kinds, after, limit)?;
    let rows = list
        .iter()
        .map(|e| {
            vec![
                Value::Int(e.revision as i64),
                Value::Text(e.kind.name().into()),
                Value::Int(e.id as i64),
                Value::Text(e.change.name().into()),
            ]
        })
        .collect();
    let mut msg = format!("{} changes after revision {r}", list.len());
    match (more, list.last()) {
        (true, Some(last)) => msg.push_str(&format!(
            "; more: CALL changes.since again with the same revision and after: '{}'",
            last.cursor()
        )),
        _ => msg.push_str(&format!(", complete up to revision {}", g.log_end())),
    }
    let mut out = QueryResult::table(
        vec![
            "revision".into(),
            "kind".into(),
            "id".into(),
            "change".into(),
        ],
        rows,
    );
    out.message = Some(msg);
    Ok(out)
}

fn status_proc(g: &mut Graph) -> Result<QueryResult> {
    let s = status(g).ok_or_else(|| {
        Error::Msg("the change journal's state does not decode; CALL check.run(checks: [\"derived\"]) reports it".into())
    })?;
    let bytes = s.entries * ENTRY_BYTES + (NS.len() as u64 + 1 + STATE_KEY.len() as u64 + 50);
    let mut out = QueryResult::table(
        ["oldest", "why", "entries", "bytes", "cap", "revision"]
            .iter()
            .map(|c| c.to_string())
            .collect(),
        vec![vec![
            Value::Int(s.floor as i64),
            Value::Text(s.why.name().into()),
            Value::Int(s.entries as i64),
            Value::Int(bytes as i64),
            Value::Int(s.cap as i64),
            Value::Int(g.revision() as i64),
        ]],
    );
    out.message = Some(format!(
        "the journal answers for revision {} and later ({}); {} entries of at most {}",
        s.floor,
        s.why.why(s.floor, s.cap),
        s.entries,
        s.cap
    ));
    Ok(out)
}

fn release_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let below = revision_arg(a, "below", "release")?.min(g.log_end());
    let limit = match a.get("limit") {
        None | Some(Value::Null) => u64::MAX,
        Some(Value::Int(i)) if *i >= 1 => *i as u64,
        Some(v) => {
            return Err(Error::Msg(format!(
                "limit: expected a whole number at least 1, got {v}"
            )))
        }
    };
    let mut s = state_or_reset(g);
    let (gone, cut) = forget(g, &mut s, Floor::Released, limit, &|e| e.revision <= below)?;
    // Stopped by the limit with more at or below `below` left: the floor is
    // where it got to. Otherwise everything at or below `below` is gone.
    let finished = !cut || entry_at_or_below(g, below).is_none();
    if finished && below > s.floor {
        s.floor = below;
        s.why = Floor::Released;
        g.ext_write(NS, vec![(STATE_KEY.to_vec(), s.encode())], Vec::new())?;
    }
    let mut out = QueryResult::message(if finished {
        format!(
            "released {gone} entries; the journal answers for revision {} and later",
            s.floor
        )
    } else {
        format!(
            "released {gone} entries, up to revision {}; more: CALL changes.release(below: {below}) again",
            s.floor
        )
    });
    out.touched = gone as usize;
    Ok(out)
}

fn entry_at_or_below(g: &Graph, below: u64) -> Option<Entry> {
    let mut first = None;
    g.ext_scan(NS, b"r", &mut |k, v| {
        first = decode_rev(k, v);
        false
    });
    first.filter(|e| e.revision <= below)
}

fn cap_proc(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let cap = match a.get("entries") {
        Some(Value::Int(i)) if *i >= MIN_CAP as i64 => *i as u64,
        v => {
            return Err(Error::Msg(format!(
                "cap needs entries: a whole number at least {MIN_CAP}, got {}",
                v.map(|v| v.to_string()).unwrap_or_else(|| "nothing".into())
            )))
        }
    };
    g.ext_write(
        NS,
        vec![(CAP_KEY.to_vec(), cap.to_be_bytes().to_vec())],
        Vec::new(),
    )?;
    let mut s = state_or_reset(g);
    s.cap = cap;
    let excess = s.entries.saturating_sub(cap);
    let (gone, _) = forget(g, &mut s, Floor::Cap, excess, &|_| true)?;
    g.ext_write(NS, vec![(STATE_KEY.to_vec(), s.encode())], Vec::new())?;
    let mut out = QueryResult::message(format!(
        "the journal holds at most {cap} entries; {gone} forgotten, it answers for revision {} and later",
        s.floor
    ));
    out.touched = 1 + gone as usize;
    Ok(out)
}

impl Extension for Changes {
    fn name(&self) -> &str {
        NS
    }
    fn version(&self) -> &str {
        "1.0"
    }
    fn summary(&self) -> &str {
        "which nodes and edges changed since a revision: a bounded journal of ids and their latest change, for rebuilding what was derived from them"
    }
    fn procedures(&self) -> &[Spec] {
        PROCEDURES
    }
    fn call(&self, g: &mut Graph, procedure: &str, a: &Args) -> Result<QueryResult> {
        match procedure {
            "since" => since_proc(g, a),
            "status" => status_proc(g),
            "release" => release_proc(g, a),
            "cap" => cap_proc(g, a),
            _ => unreachable!("dispatch checks the procedure list"),
        }
    }
    fn install(&self, g: &mut Graph) -> Result<()> {
        let s = State {
            entries: 0,
            cap: cap_config(g),
            floor: g.log_end(),
            why: Floor::Installed,
            latest: [0; 3],
        };
        g.ext_write(NS, vec![(STATE_KEY.to_vec(), s.encode())], Vec::new())
    }
    fn wants_writes(&self, _g: &Graph) -> bool {
        true
    }
    fn wants_edge_writes(&self, _g: &Graph) -> bool {
        true
    }
    fn on_node_write(
        &self,
        g: &mut Graph,
        id: u64,
        before: Option<&NodeImage>,
        after: Option<&NodeImage>,
    ) -> Result<()> {
        let change = if after.is_some() {
            Change::Put
        } else {
            Change::Drop
        };
        // A node that appears or goes: its adjacency entry, if it has one,
        // follows, so a `put` never names a node that is gone.
        if before.is_some() != after.is_some() && entry(g, Kind::Adjacency, id).is_some() {
            return note(
                g,
                &[(Kind::Node, id, change), (Kind::Adjacency, id, change)],
            );
        }
        note(g, &[(Kind::Node, id, change)])
    }
    fn on_edge_write(
        &self,
        g: &mut Graph,
        id: u64,
        before: Option<&EdgeImage>,
        after: Option<&EdgeImage>,
    ) -> Result<()> {
        let change = if after.is_some() {
            Change::Put
        } else {
            Change::Drop
        };
        let mut ends = BTreeSet::new();
        for e in [before, after].into_iter().flatten() {
            ends.insert(e.from);
            ends.insert(e.to);
        }
        let mut entries = vec![(Kind::Edge, id, change)];
        for n in ends {
            entries.push((Kind::Adjacency, n, adjacency(g, n)));
        }
        note(g, &entries)
    }
    fn on_bulk_write(&self, g: &mut Graph, _what: super::BulkWrite) -> Result<()> {
        // CLEAR has already wiped every entry with the data; a bulk write
        // of a kind not known here may not have, so whatever is left goes.
        // Either way the journal answers from here on.
        loop {
            let mut dels = Vec::new();
            g.ext_scan(NS, b"", &mut |k, _| {
                if k != CAP_KEY && k != STATE_KEY {
                    dels.push(k.to_vec());
                }
                dels.len() < BATCH
            });
            if dels.is_empty() {
                break;
            }
            step(dels.len() as u64)?;
            g.ext_write(NS, Vec::new(), dels)?;
        }
        let s = State {
            entries: 0,
            cap: cap_config(g),
            floor: g.log_end(),
            why: Floor::Cleared,
            latest: [0; 3],
        };
        g.ext_write(NS, vec![(STATE_KEY.to_vec(), s.encode())], Vec::new())
    }
    fn check(&self, g: &Graph, probe: &mut super::check::Probe) -> Result<bool> {
        check_journal(g, probe)
    }
}

/// `check.run`'s `derived` check: every record decodes; every `i` entry
/// has its `r` entry and the other way round; a `put` names an id that
/// exists; no entry is past the log's end; and, over a whole walk, the
/// state's count is the number of entries.
fn check_journal(g: &Graph, probe: &mut super::check::Probe) -> Result<bool> {
    let whole = probe.after().is_none();
    let end = g.log_end();
    let mut after: Option<Vec<u8>> = probe.after().map(|a| a.to_vec());
    let mut counted = 0u64;
    let mut state: Option<Option<State>> = None;
    loop {
        let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        g.ext_scan_after(NS, b"", after.as_deref(), &mut |k, v| {
            batch.push((k.to_vec(), v.to_vec()));
            batch.len() < 512
        });
        if batch.is_empty() {
            break;
        }
        for (k, v) in batch {
            if !probe.look()? {
                probe.stop_at(after.unwrap_or_default());
                return Ok(true);
            }
            let name = hex(&k);
            match k.first() {
                Some(b'i') => match decode_id(&k, &v) {
                    None => probe.found("bad_record", name, "a journal entry that does not decode; DROP EXTENSION changes and CREATE EXTENSION changes start the journal again"),
                    Some(e) => {
                        counted += 1;
                        let handle = handle(e.kind, e.id);
                        if e.revision > end {
                            probe.found("future", handle.clone(), format!("an entry at revision {} is past the log's end at {end}", e.revision));
                        }
                        if e.change == Change::Put && !exists(g, e.kind, e.id) {
                            probe.found("missing", handle.clone(), format!("a put entry for a {} that does not exist", e.kind.name()));
                        }
                        let rk = rev_key(e.revision, e.kind, e.id);
                        if g.ext_get(NS, &rk).as_deref() != Some(&[e.change.byte()][..]) {
                            probe.found("unindexed", handle, "an entry missing from the revision index, so since() would not list it");
                        }
                    }
                },
                Some(b'r') => match decode_rev(&k, &v) {
                    None => probe.found("bad_record", name, "a revision index entry that does not decode; DROP EXTENSION changes and CREATE EXTENSION changes start the journal again"),
                    Some(e) => {
                        let ik = id_key(e.kind, e.id);
                        let held = g.ext_get(NS, &ik).and_then(|v| decode_id(&ik, &v));
                        if held != Some(e) {
                            probe.found("stray", handle(e.kind, e.id), format!("a revision index entry at {} that is not the id's latest change", e.revision));
                        }
                    }
                },
                Some(b's') if k == STATE_KEY => match State::decode(&v) {
                    None => {
                        state = Some(None);
                        probe.found("bad_record", name, "the journal's state does not decode; DROP EXTENSION changes and CREATE EXTENSION changes start the journal again");
                    }
                    Some(s) => {
                        state = Some(Some(s));
                        if s.floor > end {
                            probe.found("future", "state", format!("the journal's floor {} is past the log's end at {end}", s.floor));
                        }
                    }
                },
                Some(b'!') if k == CAP_KEY && v.len() == 8 => {}
                _ => probe.found("bad_record", name, "a record the journal does not write; DROP EXTENSION changes and CREATE EXTENSION changes start the journal again"),
            }
            after = Some(k);
        }
    }
    if whole {
        match state {
            None => probe.found("missing", "state", "the journal has no state record; DROP EXTENSION changes and CREATE EXTENSION changes start it again"),
            Some(Some(s)) if s.entries != counted => probe.found(
                "count",
                "state",
                format!("the state counts {} entries and the journal holds {counted}", s.entries),
            ),
            _ => {}
        }
    }
    Ok(true)
}

fn exists(g: &Graph, kind: Kind, id: u64) -> bool {
    match kind {
        Kind::Node | Kind::Adjacency => g.node(id).is_some(),
        Kind::Edge => g.edge(id).is_some(),
    }
}

fn handle(kind: Kind, id: u64) -> String {
    match kind {
        Kind::Edge => format!("e{id}"),
        _ => format!("n{id}"),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_read_back_as_written() {
        let s = State {
            entries: 7,
            cap: 9,
            floor: 1234,
            why: Floor::Cap,
            latest: [1, 2, 3],
        };
        assert_eq!(State::decode(&s.encode()), Some(s));
        let e = Entry {
            revision: 99,
            kind: Kind::Adjacency,
            id: 5,
            change: Change::Drop,
        };
        let ik = id_key(e.kind, e.id);
        assert_eq!(decode_id(&ik, &id_value(e.revision, e.change)), Some(e));
        let rk = rev_key(e.revision, e.kind, e.id);
        assert_eq!(decode_rev(&rk, &[e.change.byte()]), Some(e));
        assert_eq!(
            ENTRY_BYTES,
            2 * (NS.len() as u64 + 1) + ik.len() as u64 + 9 + rk.len() as u64 + 1
        );
    }
}
