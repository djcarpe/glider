//! `changes`: which nodes and edges changed, and at what revision.
//!
//! ```text
//! CREATE EXTENSION changes                     start a journal at the current revision
//! CALL changes.since(revision: 1042)           ids changed after a revision, by kind and id
//! CALL changes.since(revision: 1042, kinds: ["node"], after: "node:7", limit: 1000)
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
//! **One record per change, nothing else.** A write adds one extension
//! record holding one entry per (kind, id) it changed, written blind: no
//! index by revision, no count, no state rewritten. Keys are
//! `<kind><length><id, big-endian, no leading zeros>`, so they sort by kind
//! then id, and values `<change><revision, big-endian, no leading zeros>`.
//! `since` scans the entries in key order; the order of its rows is kind
//! (node, edge, adjacency), then id.
//!
//! **Bounds.** The journal holds at most `cap` entries (default
//! [`DEFAULT_CAP`], at least [`MIN_CAP`]). Without a count it cannot see
//! the cap coming one write at a time, so the state (`s`) names a log
//! position, `look_at`: every entry costs at least [`MIN_ENTRY_LOG`] bytes
//! of its own record and a write makes at most [`MAX_PER_WRITE`], so after
//! counting at most `n` entries at position p the journal cannot pass three
//! quarters of the cap before p + MIN_ENTRY_LOG x (3/4 cap - n - 5). From
//! there each write takes one step of a sweep ([`SWEEP`] entries: forget
//! those at or below the floor, count the rest, rewrite the state); a
//! sweep ends below the cap, and at its end the floor rises when more than
//! half the cap may be held. No write pays more than one step. Raising the
//! floor is what makes forgetting safe: `since(R)` with R below the floor
//! is an error that says why and names the floor, never a partial list.
//!
//! **Positions past the database's.** The journal never answers while its
//! state, its floor or an entry it reads lies past [`Graph::log_end`], or
//! for a revision past it; the next write starts it again ([`Mode::Purging`]).
//!
//! **Keys** (keyspace `changes`): `!cap` -> the cap (configuration, so it
//! survives `CLEAR`); `s` -> the state ([`State`]); every other key is an
//! entry.
//!
//! Every loop over entries passes through [`step`], the one place the
//! statement budget (slice G2) is to be counted.

use super::{Args, EdgeImage, Extension, NodeImage, Spec};
use crate::graph::{Error, Graph, Result};
use crate::query::QueryResult;
use crate::value::Value;

pub struct Changes;

/// The keyspace, and the extension's name.
pub const NS: &str = "changes";

/// Entries the journal holds when no cap was set.
pub const DEFAULT_CAP: u64 = 1_000_000;

/// The smallest cap: with it, a sweep started at three quarters of the
/// cap ends below it (see the module documentation).
pub const MIN_CAP: u64 = 64;

/// Entries a write looks at, at most, while the journal sweeps or purges.
pub const SWEEP: usize = 64;

/// The most entries one write makes: an edge moved by a replayed `EdgeAdd`
/// over an existing id, itself and four endpoints.
pub const MAX_PER_WRITE: u64 = 5;

/// The fewest log bytes an entry costs in its record: the key's length,
/// the namespace and its separator, kind, length and at least one byte of
/// id, the value's length, the change and at least one byte of revision.
pub const MIN_ENTRY_LOG: u64 = 1 + NS.len() as u64 + 1 + 3 + 1 + 2;

/// Rows one `since` call returns at most.
pub const MAX_ROWS: usize = 10_000;

/// Entries deleted per write by upkeep, `release` and `cap`.
const BATCH: usize = 4_096;

/// Buckets of the revision histogram that chooses what the cap forgets.
const BUCKETS: usize = 4_096;

const CAP_KEY: &[u8] = b"!cap";
const STATE_KEY: &[u8] = b"s";

const PROCEDURES: &[Spec] = &[
    Spec {
        name: "since",
        args: "(revision, kinds, after, limit)",
        doc: "Nodes, edges and adjacencies changed after a revision, once each with its latest change (put or drop), by kind then id; an error when the journal cannot answer for it",
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
        format!("{}:{}", self.kind.name(), self.id)
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

/// What the journal is doing about its bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Recording; nothing to do until the log reaches `look_at`.
    Idle,
    /// Recording, and walking its entries [`SWEEP`] at a time per write:
    /// forgetting those at or below the floor, counting the rest.
    Sweeping,
    /// Positions went back below what it held: recording nothing,
    /// forgetting every entry [`SWEEP`] at a time per write, answering
    /// nothing until it starts again at the position where it is empty.
    Purging,
}

/// Buckets of the revision histogram a sweep keeps to choose a new floor.
pub const SWEEP_BUCKETS: usize = 16;

/// The journal's state: one record, rewritten by install, `CLEAR`,
/// `release`, `cap`, and by a write only when it starts a sweep, takes a
/// sweep's step or ends one (and once when positions are found to have
/// gone back). An ordinary write never rewrites it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub cap: u64,
    /// The oldest revision it answers for: `since(R)` needs R >= floor.
    pub floor: u64,
    pub why: Floor,
    /// The log position from which the next write starts a sweep.
    pub look_at: u64,
    /// The log position at which this record was written: never above the
    /// database's, unless positions went back.
    pub at: u64,
    pub mode: Mode,
    /// The current sweep: where it started, how many writes it has taken,
    /// how many entries above the floor it counted, the last key it looked
    /// at, and the counted entries' revisions over (floor, start].
    pub sweep_start: u64,
    pub sweep_writes: u64,
    pub sweep_live: u64,
    pub cursor: Option<[u8; 10]>,
    pub cursor_len: u8,
    pub hist: [u64; SWEEP_BUCKETS],
}

const STATE_LEN: usize = 1 + 8 * 5 + 1 + 1 + 8 * 3 + 1 + 10 + 8 * SWEEP_BUCKETS;

impl State {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(STATE_LEN);
        out.push(3u8);
        for v in [
            self.cap,
            self.floor,
            self.look_at,
            self.at,
            self.sweep_start,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.push(self.why.byte());
        out.push(match self.mode {
            Mode::Idle => 0,
            Mode::Sweeping => 1,
            Mode::Purging => 2,
        });
        for v in [self.sweep_writes, self.sweep_live, 0] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.push(if self.cursor.is_some() {
            self.cursor_len
        } else {
            0xff
        });
        out.extend_from_slice(&self.cursor.unwrap_or([0; 10]));
        for h in self.hist {
            out.extend_from_slice(&h.to_be_bytes());
        }
        out
    }

    fn decode(b: &[u8]) -> Option<State> {
        if b.len() != STATE_LEN || b[0] != 3 {
            return None;
        }
        let mode = match b[42] {
            0 => Mode::Idle,
            1 => Mode::Sweeping,
            2 => Mode::Purging,
            _ => return None,
        };
        let cl = b[67];
        let cursor = (cl != 0xff).then(|| b[68..78].try_into().unwrap());
        if cl != 0xff && cl > 10 {
            return None;
        }
        let mut hist = [0u64; SWEEP_BUCKETS];
        for (i, h) in hist.iter_mut().enumerate() {
            *h = u64_at(b, 78 + 8 * i);
        }
        Some(State {
            cap: u64_at(b, 1),
            floor: u64_at(b, 9),
            look_at: u64_at(b, 17),
            at: u64_at(b, 25),
            sweep_start: u64_at(b, 33),
            why: Floor::of(b[41])?,
            mode,
            sweep_writes: u64_at(b, 43),
            sweep_live: u64_at(b, 51),
            cursor,
            cursor_len: if cl == 0xff { 0 } else { cl },
            hist,
        })
    }

    /// The point from which a sweep must start so that the journal, after
    /// counting at most `n` entries at log position `pos`, stays under the
    /// cap through the sweep (see the module documentation).
    fn next_look(&mut self, pos: u64, n: u64) {
        let room = sweep_threshold(self.cap).saturating_sub(n + MAX_PER_WRITE);
        self.look_at = pos.saturating_add(MIN_ENTRY_LOG * room);
    }

    fn cursor_key(&self) -> Option<Vec<u8>> {
        self.cursor.map(|c| c[..self.cursor_len as usize].to_vec())
    }

    fn idle(&mut self) {
        self.mode = Mode::Idle;
        self.sweep_start = 0;
        self.sweep_writes = 0;
        self.sweep_live = 0;
        self.cursor = None;
        self.cursor_len = 0;
        self.hist = [0; SWEEP_BUCKETS];
    }
}

/// Where a sweep starts: three quarters of the cap.
fn sweep_threshold(cap: u64) -> u64 {
    cap / 4 * 3
}

/// What `status` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub entries: u64,
    /// Keys (with the namespace the tree stores them under) and values.
    pub bytes: u64,
    pub cap: u64,
    pub floor: u64,
    pub why: Floor,
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

/// Big-endian with no leading zero bytes (at least one byte).
fn put_min(out: &mut Vec<u8>, v: u64) {
    let b = v.to_be_bytes();
    let skip = (v.leading_zeros() / 8).min(7) as usize;
    out.extend_from_slice(&b[skip..]);
}

fn get_min(b: &[u8]) -> Option<u64> {
    if b.is_empty() || b.len() > 8 {
        return None;
    }
    Some(b.iter().fold(0u64, |a, &x| (a << 8) | x as u64))
}

fn entry_key(kind: Kind, id: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(10);
    k.push(kind.byte());
    k.push(0);
    put_min(&mut k, id);
    k[1] = (k.len() - 2) as u8;
    k
}

fn entry_value(rev: u64, change: Change) -> Vec<u8> {
    let mut v = Vec::with_capacity(9);
    v.push(change.byte());
    put_min(&mut v, rev);
    v
}

/// One entry as stored: its key and value. For tests that plant faults.
pub fn encode_entry(e: &Entry) -> (Vec<u8>, Vec<u8>) {
    (entry_key(e.kind, e.id), entry_value(e.revision, e.change))
}

fn decode_entry(k: &[u8], v: &[u8]) -> Option<Entry> {
    if k.len() < 3 || k[1] as usize != k.len() - 2 || v.len() < 2 {
        return None;
    }
    let id = get_min(&k[2..])?;
    // The shortest form only, so one id has one key.
    if k.len() > 3 && k[2] == 0 {
        return None;
    }
    Some(Entry {
        revision: get_min(&v[1..])?,
        kind: Kind::of(k[0])?,
        id,
        change: Change::of(v[0])?,
    })
}

fn is_entry_key(k: &[u8]) -> bool {
    k.first().is_some_and(|&b| b <= 2)
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

/// A fresh state at the log's end, knowing nothing before it, that sweeps
/// once the journal could hold three quarters of the cap.
fn fresh(g: &Graph, why: Floor) -> State {
    let mut s = State {
        cap: cap_config(g),
        floor: g.log_end(),
        why,
        look_at: 0,
        at: g.log_end(),
        mode: Mode::Idle,
        sweep_start: 0,
        sweep_writes: 0,
        sweep_live: 0,
        cursor: None,
        cursor_len: 0,
        hist: [0; SWEEP_BUCKETS],
    };
    s.next_look(g.log_end(), 0);
    s
}

/// The state record, written with `at` set to the log's end.
fn state_put(g: &Graph, s: &mut State) -> (Vec<u8>, Vec<u8>) {
    s.at = g.log_end();
    (STATE_KEY.to_vec(), s.encode())
}

fn state(g: &Graph) -> Option<State> {
    State::decode(&g.ext_get(NS, STATE_KEY)?)
}

/// Walk the entries of `kinds` (all when empty) in key order, starting
/// after `after`, until `f` returns false. An entry that does not decode
/// is passed as None.
fn walk(
    g: &Graph,
    kinds: &[Kind],
    after: Option<&[u8]>,
    f: &mut dyn FnMut(&[u8], Option<Entry>) -> bool,
) {
    let all = [Kind::Node, Kind::Edge, Kind::Adjacency];
    let mut list: Vec<Kind> = if kinds.is_empty() {
        all.to_vec()
    } else {
        all.iter().copied().filter(|k| kinds.contains(k)).collect()
    };
    if let Some(a) = after {
        list.retain(|k| a.first().map_or(true, |&b| k.byte() >= b));
    }
    let mut go = true;
    for k in list {
        if !go {
            return;
        }
        let prefix = [k.byte()];
        let from = after.filter(|a| a.first() == Some(&k.byte()));
        g.ext_scan_after(NS, &prefix, from, &mut |key, v| {
            go = f(key, decode_entry(key, v));
            go
        });
    }
}

// ------------------------------------------------------------- the Rust API

/// The journal's state, or None when it is not installed (or its state is
/// unreadable, which `check.run` reports).
pub fn state_of(g: &Graph) -> Option<State> {
    if !installed(g) {
        return None;
    }
    state(g)
}

/// Entries, bytes, cap and floor, counted by a scan; None when the journal
/// is not installed.
pub fn status(g: &Graph) -> Option<Status> {
    let s = state_of(g)?;
    let (mut entries, mut bytes, mut looked) = (0u64, 0u64, 0u64);
    g.ext_scan(NS, b"", &mut |k, v| {
        looked += 1;
        if is_entry_key(k) {
            entries += 1;
        }
        bytes += (NS.len() + 1 + k.len() + v.len()) as u64;
        true
    });
    step(looked).ok()?;
    Some(Status {
        entries,
        bytes,
        cap: s.cap,
        floor: s.floor,
        why: s.why,
    })
}

/// The oldest revision the journal can answer for, and why it is that one;
/// None when it is not installed.
pub fn oldest(g: &Graph) -> Option<(u64, Floor)> {
    state_of(g).map(|s| (s.floor, s.why))
}

/// Whether the journal is installed and can answer for `since`: every
/// change after it is still listed. Never when `since` is above the
/// database's position (it comes from another line of history), when the
/// journal's state was written at a position above it (positions went
/// back: an image of a later state, or a copy), or while it purges.
pub fn answers(g: &Graph, since: u64) -> bool {
    refusal(g, since).is_none()
}

/// Why the journal cannot answer for `since`, if it cannot.
fn refusal(g: &Graph, since: u64) -> Option<String> {
    let Some(s) = state_of(g) else {
        return Some(if installed(g) {
            "the change journal's state does not decode; CALL check.run(checks: [\"derived\"]) reports it, and DROP EXTENSION changes then CREATE EXTENSION changes starts it again".into()
        } else {
            "extension 'changes' is not installed in this database; run CREATE EXTENSION changes"
                .into()
        });
    };
    let now = g.log_end();
    if s.mode == Mode::Purging || s.at > now || s.floor > now {
        return Some(format!(
            "the change journal cannot answer: it holds positions past this database's ({now}), so positions here went back (a copy of a later state, or another line of history); it is forgetting them and starts again with the next writes. Rebuild what was derived from a scan of the graph, then ask from the revision CALL changes.status() reports"
        ));
    }
    if since > now {
        return Some(format!(
            "the change journal cannot answer for revision {since}: this database is at {now}, so that revision belongs to another line of history or a later state of a copy. Rebuild what was derived from a scan of the graph"
        ));
    }
    if since < s.floor {
        return Some(format!(
            "the change journal cannot answer for revision {since}: {}; it answers for revision {} and later. Rebuild what was derived from a scan of the graph, then ask CALL changes.since(revision: {}) or later",
            s.why.why(s.floor, s.cap),
            s.floor,
            s.floor
        ));
    }
    None
}

/// The latest change to one id of `kind`, if the journal holds one.
pub fn entry(g: &Graph, kind: Kind, id: u64) -> Option<Entry> {
    let k = entry_key(kind, id);
    decode_entry(&k, &g.ext_get(NS, &k)?)
}

/// Which of `ids` (of `kind`) changed after revision `since`, one bool
/// each; None when the journal is not installed or cannot answer for
/// `since`, so the caller must assume all of them did. One lookup an id.
pub fn changed(g: &Graph, since: u64, kind: Kind, ids: &[u64]) -> Option<Vec<bool>> {
    if !answers(g, since) {
        return None;
    }
    let now = g.log_end();
    let mut out = Vec::with_capacity(ids.len());
    for &id in ids {
        match entry(g, kind, id) {
            // A position past the database's: positions went back.
            Some(e) if e.revision > now => return None,
            e => out.push(e.is_some_and(|e| e.revision > since)),
        }
    }
    step(ids.len() as u64).ok()?;
    Some(out)
}

/// Whether anything of `kind` changed after revision `since`; None when
/// the journal is not installed or cannot answer for `since`. Scans the
/// entries of the kind until it finds one.
pub fn changed_any(g: &Graph, since: u64, kind: Kind) -> Option<bool> {
    if !answers(g, since) {
        return None;
    }
    let now = g.log_end();
    let mut found = false;
    let mut past = false;
    let mut looked = 0u64;
    walk(g, &[kind], None, &mut |_, e| {
        looked += 1;
        // An entry that does not decode might be anything.
        found = e.map_or(true, |e| e.revision > since);
        past = e.is_some_and(|e| e.revision > now);
        !found
    });
    step(looked).ok()?;
    (!past).then_some(found)
}

/// The entries changed after revision `since`, by kind then id, starting
/// after `after` (an entry's (kind, id), from [`Entry::cursor`]), of
/// `kinds` (all when empty), at most `limit`; and whether more follow. An
/// error when the journal is not installed or cannot answer for `since`.
pub fn since(
    g: &Graph,
    since: u64,
    kinds: &[Kind],
    after: Option<(Kind, u64)>,
    limit: usize,
) -> Result<(Vec<Entry>, bool)> {
    if let Some(why) = refusal(g, since) {
        return Err(Error::Msg(why));
    }
    let now = g.log_end();
    let start = after.map(|(k, id)| entry_key(k, id));
    let mut out = Vec::new();
    let mut more = false;
    let mut bad = false;
    let mut past = false;
    let mut looked = 0u64;
    walk(g, kinds, start.as_deref(), &mut |_, e| {
        looked += 1;
        let Some(e) = e else {
            bad = true;
            return false;
        };
        if e.revision > now {
            past = true;
            return false;
        }
        if e.revision <= since {
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
    if past {
        return Err(Error::Msg(format!(
            "the change journal cannot answer: it holds a change at a position past this database's ({now}), so positions here went back; it forgets what it holds and starts again with the next writes. Rebuild what was derived from a scan of the graph"
        )));
    }
    if bad {
        return Err(Error::Msg(
            "the change journal holds an entry that does not decode; CALL check.run(checks: [\"derived\"]) names it".into(),
        ));
    }
    Ok((out, more))
}

// --------------------------------------------------------------- recording

/// Record `entries` (kind, id, change) at the log's end, in one write,
/// with this write's share of the journal's upkeep: at most [`SWEEP`]
/// entries looked at and forgotten, and the state rewritten, only while it
/// sweeps or purges.
fn note(g: &mut Graph, entries: &[(Kind, u64, Change)]) -> Result<()> {
    note_seen(g, entries, false)
}

/// The state as every writer of it must start from: when it is missing or
/// does not decode, or holds a position past the database's (positions
/// went back), a restart ([`Mode::Purging`]) begins, and whoever writes the
/// state next writes it purging, so the evidence is never lost. Returns
/// whether the restart begins here.
fn load(g: &Graph) -> (State, bool) {
    let now = g.log_end();
    match state(g) {
        None => {
            let mut s = fresh(g, Floor::Reset);
            s.mode = Mode::Purging;
            (s, true)
        }
        Some(mut s) if s.mode != Mode::Purging && (s.at > now || s.floor > now) => {
            begin_restart(&mut s);
            (s, true)
        }
        Some(s) => (s, false),
    }
}

/// Forget it all, a slice per write, answering nothing meanwhile. Only the
/// restart's own end (an empty journal) sets the floor and records again.
fn begin_restart(s: &mut State) {
    s.idle();
    s.mode = Mode::Purging;
    s.why = Floor::Reset;
}

/// [`note`], told whether the write has already met an entry past the
/// database's position (the evidence that begins a restart).
fn note_seen(g: &mut Graph, entries: &[(Kind, u64, Change)], seen_past: bool) -> Result<()> {
    let rev = g.log_end();
    let (mut s, began) = load(g);
    let mut dels: Vec<Vec<u8>> = Vec::new();
    let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(entries.len() + 1);
    let mut rewrite = began;
    if seen_past && s.mode != Mode::Purging {
        begin_restart(&mut s);
        rewrite = true;
    }
    match s.mode {
        Mode::Purging => {
            let after = s.cursor_key();
            let (keys, ended) = slice(g, after.as_deref());
            dels.extend(keys.iter().map(|(k, _)| k.clone()));
            if ended {
                // Empty (this write's deletes included): start again here.
                // This write's own changes lie at or below the new floor.
                let cap = s.cap;
                s = fresh(g, Floor::Reset);
                s.cap = cap;
                s.next_look(g.log_end(), 0);
            } else {
                set_cursor(&mut s, keys.last().map(|(k, _)| k.as_slice()));
            }
            puts.push(state_put(g, &mut s));
            return g.ext_write(NS, puts, dels);
        }
        Mode::Idle if rev >= s.look_at => {
            s.mode = Mode::Sweeping;
            s.sweep_start = rev;
            s.sweep_writes = 0;
            s.sweep_live = 0;
            s.cursor = None;
            s.hist = [0; SWEEP_BUCKETS];
            sweep_step(g, &mut s, rev, &mut dels);
            rewrite = true;
        }
        Mode::Sweeping => {
            sweep_step(g, &mut s, rev, &mut dels);
            rewrite = true;
        }
        Mode::Idle => {}
    }
    if s.mode == Mode::Purging {
        // Evidence met in this write (a sweep, or the entry a node delete
        // reads): record nothing; the restart takes its slices from the
        // next write on.
        dels.clear();
        puts.push(state_put(g, &mut s));
        return g.ext_write(NS, puts, dels);
    }
    step(entries.len() as u64)?;
    for &(kind, id, change) in entries {
        puts.push((entry_key(kind, id), entry_value(rev, change)));
    }
    if rewrite {
        puts.push(state_put(g, &mut s));
    }
    g.ext_write(NS, puts, dels)
}

/// Whether any entry lies past the database's position: the evidence of a
/// journal from another history. One walk of the journal, for the calls
/// that walk it anyway (`release`, `cap`).
fn any_past(g: &Graph) -> bool {
    let now = g.log_end();
    let (mut past, mut looked) = (false, 0u64);
    walk(g, &[], None, &mut |_, e| {
        looked += 1;
        past = e.is_some_and(|e| e.revision > now);
        !past
    });
    let _ = step(looked);
    past
}

fn set_cursor(s: &mut State, key: Option<&[u8]>) {
    match key {
        Some(k) if k.len() <= 10 => {
            let mut c = [0u8; 10];
            c[..k.len()].copy_from_slice(k);
            s.cursor = Some(c);
            s.cursor_len = k.len() as u8;
        }
        // Not an entry key (none longer is written); restart from the top.
        _ => {
            s.cursor = None;
            s.cursor_len = 0;
        }
    }
}

/// The next [`SWEEP`] entries after `after`, and whether they are the last.
fn slice(g: &Graph, after: Option<&[u8]>) -> (Vec<(Vec<u8>, Option<Entry>)>, bool) {
    let mut out = Vec::with_capacity(SWEEP);
    let mut ended = true;
    walk(g, &[], after, &mut |k, e| {
        if out.len() == SWEEP {
            ended = false;
            return false;
        }
        out.push((k.to_vec(), e));
        true
    });
    let _ = step(out.len() as u64);
    (out, ended)
}

/// One write's step of a sweep: look at the next [`SWEEP`] entries,
/// forget those at or below the floor, count the rest; at the end, raise
/// the floor if more than half the cap may be held, and say when to sweep
/// again.
fn sweep_step(g: &Graph, s: &mut State, rev: u64, dels: &mut Vec<Vec<u8>>) {
    let after = s.cursor_key();
    let (keys, ended) = slice(g, after.as_deref());
    s.sweep_writes += 1;
    let lo = s.floor;
    let width = (s.sweep_start.saturating_sub(lo) / SWEEP_BUCKETS as u64) + 1;
    for (k, e) in &keys {
        match e {
            // An entry past the database's position: positions went back.
            Some(e) if e.revision > rev => {
                begin_restart(s);
                return;
            }
            Some(e) if e.revision <= s.floor => dels.push(k.clone()),
            Some(e) => {
                s.sweep_live += 1;
                if e.revision <= s.sweep_start {
                    let b = ((e.revision - lo - 1) / width) as usize;
                    s.hist[b.min(SWEEP_BUCKETS - 1)] += 1;
                }
            }
            // Kept and counted; check.run names it.
            None => s.sweep_live += 1,
        }
    }
    if !ended {
        set_cursor(s, keys.last().map(|(k, _)| k.as_slice()));
        return;
    }
    // Entries written during the sweep may lie behind its cursor: count
    // each write's most.
    let held = s.sweep_live + MAX_PER_WRITE * s.sweep_writes;
    let keep = s.cap / 2;
    let mut again = false;
    if held > keep {
        let need = held - keep;
        let (mut cum, mut cut) = (0u64, s.sweep_start);
        for (b, c) in s.hist.iter().enumerate() {
            cum += c;
            if cum >= need {
                cut = (lo + (b as u64 + 1) * width).min(s.sweep_start);
                break;
            }
        }
        if cut > s.floor {
            s.floor = cut;
            s.why = Floor::Cap;
        }
        // Sweep again at once, to forget what is now below the floor.
        again = true;
    }
    s.idle();
    if again {
        s.look_at = rev;
    } else {
        s.next_look(rev, held);
    }
}

/// `cap`'s immediate trim: count the entries, forget those at or below the
/// floor, and when more than `keep` remain raise the floor and forget the
/// oldest down to `keep` or fewer; then write the state, idle. The whole
/// journal in one call, which is what the caller of `cap` asked for.
/// Returns how many were forgotten.
fn trim_now(g: &mut Graph, s: &mut State, keep: u64) -> Result<u64> {
    let mut gone = forget_below(g, s.floor, u64::MAX)?;
    let (mut n, mut hi) = (0u64, s.floor);
    walk(g, &[], None, &mut |_, e| {
        n += 1;
        if let Some(e) = e {
            hi = hi.max(e.revision);
        }
        true
    });
    step(n)?;
    if n > keep && hi > s.floor {
        let lo = s.floor + 1;
        let width = ((hi - lo) / BUCKETS as u64) + 1;
        let mut hist = vec![0u64; BUCKETS];
        walk(g, &[], None, &mut |_, e| {
            if let Some(e) = e {
                hist[((e.revision.max(lo) - lo) / width) as usize] += 1;
            }
            true
        });
        step(n)?;
        let need = n - keep;
        let (mut cum, mut cut) = (0u64, hi);
        for (b, c) in hist.iter().enumerate() {
            cum += c;
            if cum >= need {
                cut = (lo + (b as u64 + 1) * width - 1).min(hi);
                break;
            }
        }
        s.floor = cut;
        s.why = Floor::Cap;
        let more = forget_below(g, cut, u64::MAX)?;
        gone += more;
        n -= more;
    }
    s.idle();
    s.next_look(g.log_end(), n);
    let put = state_put(g, s);
    g.ext_write(NS, vec![put], Vec::new())?;
    Ok(gone)
}

/// Forget entries at or below `below` (at most `limit`), a batch per
/// write, walking every entry once. Returns how many went.
fn forget_below(g: &mut Graph, below: u64, limit: u64) -> Result<u64> {
    let mut gone = 0u64;
    let mut after: Option<Vec<u8>> = None;
    loop {
        let room = (limit - gone).min(BATCH as u64) as usize;
        if room == 0 {
            return Ok(gone);
        }
        let mut dels: Vec<Vec<u8>> = Vec::new();
        let mut last: Option<Vec<u8>> = None;
        let mut looked = 0u64;
        let mut ended = true;
        walk(g, &[], after.as_deref(), &mut |k, e| {
            looked += 1;
            last = Some(k.to_vec());
            if e.is_some_and(|e| e.revision <= below) {
                dels.push(k.to_vec());
            }
            if dels.len() == room || looked >= 16 * BATCH as u64 {
                ended = false;
                return false;
            }
            true
        });
        step(looked)?;
        gone += dels.len() as u64;
        if !dels.is_empty() {
            g.ext_write(NS, Vec::new(), dels)?;
        }
        if ended {
            return Ok(gone);
        }
        after = last;
    }
}

/// The adjacency entry's change for node `n`: put while it exists.
fn adjacency(g: &Graph, n: u64) -> Change {
    if g.node(n).is_some() {
        Change::Put
    } else {
        Change::Drop
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

fn after_arg(a: &Args) -> Result<Option<(Kind, u64)>> {
    let Some(s) = a.str("after") else {
        return Ok(None);
    };
    let bad = || {
        Error::Msg(format!(
            "after: '{s}' is not a position; pass back the one the previous call's message gave (kind:id)"
        ))
    };
    let mut parts = s.split(':');
    let k = parts.next().and_then(Kind::parse).ok_or_else(bad)?;
    let id = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
    if parts.next().is_some() {
        return Err(bad());
    }
    Ok(Some((k, id)))
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
    let mut out = QueryResult::table(
        ["oldest", "why", "entries", "bytes", "cap", "revision"]
            .iter()
            .map(|c| c.to_string())
            .collect(),
        vec![vec![
            Value::Int(s.floor as i64),
            Value::Text(s.why.name().into()),
            Value::Int(s.entries as i64),
            Value::Int(s.bytes as i64),
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
    // The floor goes up first: from then on nothing at or below it is
    // consulted, so forgetting it can be done in slices.
    // A restart under way stays one: the floor may rise, nothing more.
    let (mut s, _) = load(g);
    if s.mode != Mode::Purging && any_past(g) {
        begin_restart(&mut s);
    }
    let raise = below > s.floor;
    if raise {
        s.floor = below;
        if s.mode != Mode::Purging {
            s.why = Floor::Released;
        }
    }
    if raise || s.mode == Mode::Purging {
        let put = state_put(g, &mut s);
        g.ext_write(NS, vec![put], Vec::new())?;
    }
    let gone = forget_below(g, s.floor, limit)?;
    let left = gone == limit && forget_below_count(g, s.floor) > 0;
    let mut out = QueryResult::message(if left {
        format!(
            "released {gone} entries; the journal answers for revision {} and later; more: CALL changes.release(below: {below}) again to forget the rest",
            s.floor
        )
    } else {
        format!(
            "released {gone} entries; the journal answers for revision {} and later",
            s.floor
        )
    });
    out.touched = gone as usize;
    Ok(out)
}

/// Whether any entry at or below `below` is left (1) or not (0).
fn forget_below_count(g: &Graph, below: u64) -> u64 {
    let (mut found, mut looked) = (0, 0u64);
    walk(g, &[], None, &mut |_, e| {
        looked += 1;
        if e.is_some_and(|e| e.revision <= below) {
            found = 1;
            return false;
        }
        true
    });
    let _ = step(looked);
    found
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
    let (mut s, _) = load(g);
    s.cap = cap;
    if s.mode != Mode::Purging && any_past(g) {
        begin_restart(&mut s);
    }
    if s.mode == Mode::Purging {
        // A restart under way: the cap takes effect, and the restart goes
        // on; only its own end makes the journal answer again.
        let put = state_put(g, &mut s);
        g.ext_write(NS, vec![put], Vec::new())?;
        let mut out = QueryResult::message(format!(
            "the journal holds at most {cap} entries; it is starting again (positions here went back, or its state was lost) and answers nothing until the writes that follow have emptied it"
        ));
        out.touched = 1;
        return Ok(out);
    }
    let gone = trim_now(g, &mut s, cap)?;
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
        let mut s = fresh(g, Floor::Installed);
        let put = state_put(g, &mut s);
        g.ext_write(NS, vec![put], Vec::new())
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
        // A node that goes or comes back: its adjacency entry, if it has
        // one, follows, so a `put` never names a node that is gone.
        if before.is_some() != after.is_some() {
            if let Some(e) = entry(g, Kind::Adjacency, id) {
                let past = e.revision > g.log_end();
                return note_seen(
                    g,
                    &[(Kind::Node, id, change), (Kind::Adjacency, id, change)],
                    past,
                );
            }
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
        // The endpoints before and after, each once (an edge moved by a
        // replayed add has four).
        let mut ends = [0u64; 4];
        let mut n = 0;
        for e in [before, after].into_iter().flatten() {
            for end in [e.from, e.to] {
                if !ends[..n].contains(&end) {
                    ends[n] = end;
                    n += 1;
                }
            }
        }
        let mut entries = [(Kind::Edge, id, change); 5];
        for (i, &end) in ends[..n].iter().enumerate() {
            entries[1 + i] = (Kind::Adjacency, end, adjacency(g, end));
        }
        let entries = &entries[..1 + n];
        note(g, entries)
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
        let mut s = fresh(g, Floor::Cleared);
        let put = state_put(g, &mut s);
        g.ext_write(NS, vec![put], Vec::new())
    }
    fn check(&self, g: &Graph, probe: &mut super::check::Probe) -> Result<bool> {
        check_journal(g, probe)
    }
}

/// `check.run`'s `derived` check: every record decodes; a `put` above the
/// floor names an id that exists; no entry and no floor is past the log's
/// end; and, over a walk from the start in one slice, the journal holds no
/// more than its cap.
fn check_journal(g: &Graph, probe: &mut super::check::Probe) -> Result<bool> {
    let whole = probe.after().is_none();
    let end = g.log_end();
    let st = state(g);
    let floor = st.map(|s| s.floor).unwrap_or(0);
    let mut after: Option<Vec<u8>> = probe.after().map(|a| a.to_vec());
    let mut counted = 0u64;
    let mut seen_state = false;
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
            if is_entry_key(&k) {
                match decode_entry(&k, &v) {
                    None => probe.found("bad_record", hex(&k), "a journal entry that does not decode; DROP EXTENSION changes and CREATE EXTENSION changes start the journal again"),
                    Some(e) => {
                        counted += 1;
                        let handle = handle(e.kind, e.id);
                        if e.revision > end {
                            probe.found("future", handle.clone(), format!("an entry at revision {} is past the log's end at {end}", e.revision));
                        }
                        if e.revision > floor && e.change == Change::Put && !exists(g, e.kind, e.id) {
                            probe.found("missing", handle, format!("a put entry for a {} that does not exist", e.kind.name()));
                        }
                    }
                }
            } else if k == STATE_KEY {
                seen_state = true;
                match State::decode(&v) {
                    None => probe.found("bad_record", "state", "the journal's state does not decode; DROP EXTENSION changes and CREATE EXTENSION changes start the journal again"),
                    Some(s) if s.floor > end || s.at > end => probe.found("future", "state", format!("the journal's floor {} or the position {} its state was written at is past the log's end at {end}", s.floor, s.at)),
                    Some(_) => {}
                }
            } else if !(k == CAP_KEY && v.len() == 8) {
                probe.found("bad_record", hex(&k), "a record the journal does not write; DROP EXTENSION changes and CREATE EXTENSION changes start the journal again");
            }
            after = Some(k);
        }
    }
    if whole {
        if !seen_state {
            probe.found("missing", "state", "the journal has no state record; DROP EXTENSION changes and CREATE EXTENSION changes start it again");
        } else if let Some(s) = st {
            if counted > s.cap {
                probe.found(
                    "count",
                    "state",
                    format!(
                        "the journal holds {counted} entries, past its cap of {}",
                        s.cap
                    ),
                );
            }
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
    fn records_read_back_as_written_and_keys_sort_by_kind_then_id() {
        let mut s = State {
            cap: 9,
            floor: 1234,
            why: Floor::Cap,
            look_at: 77,
            at: 5,
            mode: Mode::Sweeping,
            sweep_start: 1200,
            sweep_writes: 3,
            sweep_live: 40,
            cursor: None,
            cursor_len: 0,
            hist: [7; SWEEP_BUCKETS],
        };
        assert_eq!(State::decode(&s.encode()), Some(s));
        set_cursor(&mut s, Some(&[1, 2, 0, 9]));
        assert_eq!(State::decode(&s.encode()), Some(s));
        let mut keys = Vec::new();
        for kind in Kind::ALL {
            for id in [0u64, 1, 255, 256, 65_535, 1 << 40, u64::MAX] {
                for rev in [0u64, 1, 1 << 33, u64::MAX] {
                    let e = Entry {
                        revision: rev,
                        kind,
                        id,
                        change: Change::Drop,
                    };
                    let (k, v) = encode_entry(&e);
                    assert_eq!(decode_entry(&k, &v), Some(e));
                    assert!(is_entry_key(&k));
                }
                keys.push(((kind, id), entry_key(kind, id)));
            }
        }
        let mut by_key = keys.clone();
        by_key.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(by_key, keys, "key order is (kind, id) order");
        // The fewest log bytes an entry takes in a record.
        let (k, v) = encode_entry(&Entry {
            revision: 0,
            kind: Kind::Node,
            id: 0,
            change: Change::Put,
        });
        assert_eq!(
            1 + NS.len() + 1 + k.len() + 1 + v.len(),
            MIN_ENTRY_LOG as usize
        );
    }
}
