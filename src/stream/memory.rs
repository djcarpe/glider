//! glider-stream for `:memory:` databases.
//!
//! A memory database given a log (`Graph::enable_log`) replicates to a
//! [`Backend`] as the same objects a file replicates as ([`super::paged`]):
//! bases whose `base.json` goes up last, and segments of whole transactions
//! named by the log position they start at and the time they were shipped.
//! A replica does not say where its pages were. [`restore`] loads one into
//! memory whichever kind of database wrote it, and `paged::restore` lays one
//! out as a file, so a graph flown from memory lands on disk and the other
//! way round, on one lineage.
//!
//! ## A pass
//!
//! There is no spool: what a pass ships goes up from memory. A pass is three
//! steps, and holds the database for the first and the last only.
//!
//! 1. **Capture** ([`MemoryReplicator::capture`]), under the database's
//!    lock: decide, then take either a snapshot of the committed state — a
//!    copy of the page table, every page shared, in the time it takes to
//!    copy that many pointers (`storage::pager::Snapshot`) — or the
//!    committed log from where the replica ends to the last commit.
//! 2. **Upload** ([`MemoryReplicator::upload`]), with the lock released: a
//!    base's image one segment file at a time, `base.json` last, or the log
//!    as one segment. The writer carries on meanwhile; a page it changes is
//!    a new page, and the snapshot keeps the old one until it is dropped.
//! 3. **Acknowledge** ([`MemoryReplicator::acknowledge`]), under the lock
//!    again: record what the replica now holds, which lets the log below it
//!    go.
//!
//! [`MemoryReplicator::sync`] runs the three over a `Mutex<Graph>`, and
//! [`MemoryReplicator::sync_with`] over whatever lock an embedder keeps its
//! graph behind. A pass that fails part way has recorded nothing: the log
//! is all still held, and the next pass ships it again, or takes a base
//! again. A base that failed part way is taken down, `base.json` first, as
//! far as the backend allows; whatever stays has no `base.json` and is never
//! read, and a base at that position later replaces it.
//!
//! ## Starting against a backend
//!
//! A replicator knows nothing of a backend until it has looked. Its first
//! capture asks for a look; the upload step lists what the backend holds of
//! the database's generation, and the next capture decides from that:
//!
//! - nothing of the generation (an empty prefix, or one holding other
//!   lineages): a base of this one, whatever the database has shipped
//!   elsewhere;
//! - a base, the log whole after it to a position the database's log holds
//!   as a commit, and nothing past that: this database's own history (it
//!   was restored from here, or shipped it here before), so the pass ships
//!   only the log after it;
//! - anything else: a base. When something of the generation lies past the
//!   database's last commit — it was restored from an earlier point, or
//!   from short of a gap or a damaged frame, and has a history of its own
//!   since — a base of that generation would be older than what is there,
//!   and its log would land among the other history's. So the database
//!   takes a new generation first, and the replica holds the two lineages
//!   side by side.
//!
//! Positions are the only test of a shared history. Two databases restored
//! from one replica and both shipping back to it defeat that, as two writers
//! on one file defeat a lock: one writer per lineage.
//!
//! ## Restore
//!
//! [`restore`] reads what `paged::restore` reads — the newest whole base at
//! or before a time and the log shipped after it, stopping at a gap or the
//! time ([`paged::restore_plan`]) — into memory instead of onto a disk, and
//! builds the graph with `Graph::from_replica`: every page of the base
//! checked against the superblock that names it, the log replayed by the
//! rules a file's log is replayed by on open. The graph carries the
//! replica's lineage on, with a log that starts where the replay ended, so a
//! replicator pointed back at that replica ships only what is written next.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use super::backend::Backend;
use super::config::ReplicaConfig;
use super::paged::{self, Listing, SyncReport};
use super::replicator::Metrics;
use crate::graph::Graph;
use crate::replica::{self, RestoreReport};
use crate::storage::log::Committed;
use crate::storage::pager::{self, Snapshot};
use crate::types::{Error, Result};

/// Bytes of pages in each segment file of a base written from memory, so no
/// object is larger: a file's segments are 64 GiB, which is no size for one
/// PUT.
pub const BASE_SEGMENT_BYTES: u64 = 64 << 20;

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

fn to_io(e: Error) -> io::Error {
    match e {
        Error::Io(e) => e,
        e => other(e.to_string()),
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The time, for keys and `base.json`. wasm has no clock (std panics when
/// asked), so there a memory database can be restored but not replicated.
fn now() -> io::Result<u64> {
    if cfg!(target_family = "wasm") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "replicating needs a clock, which this platform lacks",
        ));
    }
    Ok(crate::wal::now_unix())
}

/// What a replicator knows the backend holds of one generation.
#[derive(Clone, Debug)]
struct Remote {
    generation: [u8; 16],
    /// The newest whole base: its log position and when it was taken.
    base: Option<(u64, u64)>,
    /// The log is whole from the newest base to here.
    complete_to: u64,
    /// The furthest anything of the generation reaches, gaps or not; None
    /// when there is nothing of it.
    furthest: Option<u64>,
    /// Where this replicator appends log, once it knows the replica's log
    /// to here is the database's own.
    append_at: Option<u64>,
}

impl Remote {
    fn of(generation: [u8; 16], l: &Listing) -> Remote {
        Remote {
            generation,
            base: l.bases.last().map(|b| (b.lsn, b.ts)),
            complete_to: l.complete_to(),
            furthest: l.furthest(),
            append_at: None,
        }
    }

    fn nothing(generation: [u8; 16]) -> Remote {
        Remote {
            generation,
            base: None,
            complete_to: 0,
            furthest: None,
            append_at: None,
        }
    }
}

/// What a capture took, for [`MemoryReplicator::upload`]. It owns all of
/// it — a base's pages by reference — so the database's lock can go.
pub struct Capture(Captured);

enum Captured {
    /// The replicator has not looked at the backend for this generation.
    Look([u8; 16]),
    /// Nothing to ship.
    Idle([u8; 16]),
    /// The committed state, for a base (boxed: a page table's handle is
    /// large beside the others).
    Base(Box<Snapshot>),
    /// Committed log from `from`: whole transactions.
    Segment {
        generation: [u8; 16],
        from: u64,
        bytes: Vec<u8>,
    },
}

impl Capture {
    /// A first pass's capture: the upload learns what the backend holds,
    /// and the caller captures again ([`MemoryReplicator::sync_with`] does).
    pub fn looks(&self) -> bool {
        matches!(self.0, Captured::Look(_))
    }

    /// Whether it is a base.
    pub fn is_base(&self) -> bool {
        matches!(self.0, Captured::Base(_))
    }
}

/// What an upload did, for [`MemoryReplicator::acknowledge`].
pub struct Uploaded(Done);

enum Done {
    Looked(Remote),
    Nothing([u8; 16]),
    Base {
        generation: [u8; 16],
        lsn: u64,
        ts: u64,
        /// 0 when a whole base of that state was there already.
        bytes: u64,
    },
    Segment {
        generation: [u8; 16],
        from: u64,
        to: u64,
    },
}

/// Replicates one memory database to one replica.
pub struct MemoryReplicator {
    pub backend: Arc<dyn Backend>,
    pub cfg: ReplicaConfig,
    pub metrics: Arc<Metrics>,
    /// Bytes of pages in each segment file of a base
    /// ([`BASE_SEGMENT_BYTES`] unless set).
    pub base_segment_bytes: u64,
    pub verbose: bool,
    remote: Mutex<Option<Remote>>,
    /// One [`MemoryReplicator::sync_with`] at a time.
    pass: Mutex<()>,
}

impl MemoryReplicator {
    pub fn new(backend: Arc<dyn Backend>, cfg: ReplicaConfig) -> MemoryReplicator {
        MemoryReplicator {
            backend,
            cfg,
            metrics: Arc::new(Metrics::default()),
            base_segment_bytes: BASE_SEGMENT_BYTES,
            verbose: false,
            remote: Mutex::new(None),
            pass: Mutex::new(()),
        }
    }

    fn note(&self, msg: &str) {
        if self.verbose {
            eprintln!("[glider-stream] {}: {msg}", self.backend.describe());
        }
    }

    /// Step 1, under the database's lock: decide what this pass ships and
    /// take it. A base (forced by `force_base`, due by
    /// `snapshot.interval`, or because the replica has none or the log it
    /// needs is gone) is a snapshot of the committed state, taken in the
    /// time it takes to copy the page table; otherwise the committed log
    /// from where the replica ends. Either may be taken in the middle of a
    /// transaction: neither includes it. A forced base when the newest one
    /// is of the state as it is now is that base: nothing is captured.
    pub fn capture(&self, g: &mut Graph, force_base: bool) -> io::Result<Capture> {
        let Some((_, committed)) = g.log_span() else {
            return Err(other(if g.is_persistent() {
                "a file database replicates from its files: stream::paged::PagedReplicator"
            } else {
                "this graph keeps no log in memory: give it one (Graph::enable_log) before replicating it"
            }));
        };
        let generation = g.generation();
        let mut remote = lock(&self.remote);
        let r = match remote.as_mut() {
            Some(r) if r.generation == generation => r,
            _ => return Ok(Capture(Captured::Look(generation))),
        };
        // Just after a look: whether the replica's log is the database's
        // own, to carry on from.
        if r.append_at.is_none()
            && r.base.is_some()
            && r.furthest == Some(r.complete_to)
            && matches!(g.log_committed_end(r.complete_to), Ok(Committed::To(_)))
        {
            r.append_at = Some(r.complete_to);
        }
        let every = self.cfg.snapshot_interval.as_secs();
        let due = match r.base {
            None => true,
            Some((_, ts)) => every > 0 && now()?.saturating_sub(ts) >= every,
        };
        if let (Some(at), false, false) = (r.append_at, force_base, due) {
            match g.log_committed_end(at).map_err(to_io)? {
                Committed::To(end) if end > at => {
                    let bytes = g.log_range(at, end).map_err(to_io)?;
                    return Ok(Capture(Captured::Segment {
                        generation,
                        from: at,
                        bytes,
                    }));
                }
                Committed::To(_) => return Ok(Capture(Captured::Idle(generation))),
                // The log the replica needs has been dropped: a base.
                Committed::Gone => {}
            }
        }
        // A base of the committed state, unless the replica's newest is of
        // exactly that state already.
        if r.base.map(|(lsn, _)| lsn) == Some(committed) && r.furthest == Some(committed) {
            r.append_at = Some(committed);
            return Ok(Capture(Captured::Idle(generation)));
        }
        if r.furthest.map(|f| f > committed).unwrap_or(false) {
            // Something of this generation lies past where the database is:
            // a history the database does not share. A base of it here would
            // be older than that, and its log would land among the other
            // history's. A lineage of its own instead.
            let was = replica::generation_name(&generation);
            let fresh = g.new_lineage().map_err(to_io)?;
            self.note(&format!(
                "{was} reaches log {} there, past this database's {committed}: it carries on as {}",
                r.furthest.unwrap_or(0),
                replica::generation_name(&fresh)
            ));
            *r = Remote::nothing(fresh);
        }
        Ok(Capture(Captured::Base(Box::new(
            g.snapshot().map_err(to_io)?,
        ))))
    }

    /// Step 2, without the lock: put what was captured in the backend — a
    /// base's image one segment file at a time, then its `base.json`; or a
    /// segment of log — or, on a first pass, list what the backend holds.
    /// On failure nothing is recorded, and a base's files are taken down
    /// again, `base.json` first, as far as the backend allows.
    pub fn upload(&self, c: Capture) -> io::Result<Uploaded> {
        match c.0 {
            Captured::Look(generation) => {
                let l = paged::listing(
                    self.backend.as_ref(),
                    &replica::generation_name(&generation),
                )?;
                Ok(Uploaded(Done::Looked(Remote::of(generation, &l))))
            }
            Captured::Idle(generation) => Ok(Uploaded(Done::Nothing(generation))),
            Captured::Segment {
                generation,
                from,
                bytes,
            } => {
                let gen = replica::generation_name(&generation);
                let to = from + bytes.len() as u64;
                self.backend
                    .put(&paged::segment_key(&gen, from, now()?), bytes)?;
                Ok(Uploaded(Done::Segment {
                    generation,
                    from,
                    to,
                }))
            }
            Captured::Base(s) => self.upload_base(&s),
        }
    }

    fn upload_base(&self, s: &Snapshot) -> io::Result<Uploaded> {
        let generation = s.generation();
        let gen = replica::generation_name(&generation);
        let lsn = s.lsn();
        let prefix = paged::base_prefix(&gen, lsn);
        let meta_key = format!("{prefix}base.json");
        let there = self.backend.list(&prefix)?;
        if there.iter().any(|e| e.key == meta_key) {
            // A whole base at this position of this lineage is this state,
            // put there by a pass whose acknowledgement never came. It is
            // left as it is: a base is never uploaded over.
            let meta = String::from_utf8_lossy(&self.backend.get(&meta_key)?).into_owned();
            let ts = paged::field(&meta, "ts").unwrap_or(0);
            return Ok(Uploaded(Done::Base {
                generation,
                lsn,
                ts,
                bytes: 0,
            }));
        }
        let ps = s.page_size() as u64;
        let seg_pages = (self.base_segment_bytes / ps).max(4);
        let mut sent: Vec<String> = Vec::new();
        let image = s.write_image(seg_pages, &mut |n, body| {
            let key = format!("{prefix}{}", paged::base_file(n));
            self.backend.put(&key, body)?;
            sent.push(key);
            Ok(())
        });
        let result = image.and_then(|bytes| {
            // An earlier attempt at this position may have left files this
            // base does not have; they go before the base is whole.
            for e in &there {
                if !sent.contains(&e.key) {
                    self.backend.delete(&e.key)?;
                }
            }
            let ts = now()?;
            let meta = replica::base_json(lsn, s.epoch(), ts, bytes, ps, s.high_water(), seg_pages);
            self.backend.put(&meta_key, meta.into_bytes())?;
            Ok((ts, bytes))
        });
        match result {
            Ok((ts, bytes)) => Ok(Uploaded(Done::Base {
                generation,
                lsn,
                ts,
                bytes,
            })),
            Err(e) => {
                // base.json first: its put may have landed though it said it
                // failed. Only once it is surely gone may the rest go.
                if self.backend.delete(&meta_key).is_ok() {
                    for key in &sent {
                        let _ = self.backend.delete(key);
                    }
                }
                Err(e)
            }
        }
    }

    /// Step 3, under the lock again: record what the replica now holds, and
    /// let the database's log below it go. Returns what the pass did.
    pub fn acknowledge(&self, g: &mut Graph, u: Uploaded) -> SyncReport {
        let mut remote = lock(&self.remote);
        let mut report = SyncReport::default();
        let generation = match u.0 {
            Done::Looked(r) => {
                let generation = r.generation;
                *remote = Some(r);
                generation
            }
            Done::Nothing(generation) => generation,
            Done::Base {
                generation,
                lsn,
                ts,
                bytes,
            } => {
                *remote = Some(Remote {
                    generation,
                    base: Some((lsn, ts)),
                    complete_to: lsn,
                    furthest: Some(lsn),
                    append_at: Some(lsn),
                });
                if bytes > 0 {
                    report.bases_uploaded = 1;
                    report.bytes = bytes;
                    self.metrics.snapshots.fetch_add(1, Ordering::Relaxed);
                    self.metrics.bytes.fetch_add(bytes, Ordering::Relaxed);
                    self.note(&format!(
                        "{} base at log {lsn} uploaded ({bytes} bytes)",
                        replica::generation_name(&generation)
                    ));
                }
                if g.generation() == generation {
                    g.release_log(lsn);
                }
                generation
            }
            Done::Segment {
                generation,
                from,
                to,
            } => {
                if let Some(r) = remote.as_mut().filter(|r| r.generation == generation) {
                    // Passes of one replicator may overlap: only the one
                    // that carries the replica on moves it.
                    if r.append_at == Some(from) {
                        r.append_at = Some(to);
                        r.complete_to = to;
                        r.furthest = r.furthest.max(Some(to));
                    }
                }
                report.segments_uploaded = 1;
                report.bytes = to - from;
                self.metrics.segments.fetch_add(1, Ordering::Relaxed);
                self.metrics.bytes.fetch_add(to - from, Ordering::Relaxed);
                if g.generation() == generation {
                    g.release_log(to);
                }
                generation
            }
        };
        report.generation = replica::generation_name(&generation);
        if let Some(lag) = lag_of(remote.as_ref(), g) {
            self.metrics.lag_bytes.store(lag, Ordering::Relaxed);
        }
        report
    }

    /// A whole pass for an embedder that keeps its graph behind a lock of
    /// its own: `graph` takes that lock, hands the graph to what it is
    /// given, and lets go. It is called for each capture and each
    /// acknowledgement, never around an upload. Retention, when asked for
    /// (and enabled), runs afterwards, without the graph.
    pub fn sync_with(
        &self,
        graph: &mut dyn FnMut(&mut dyn FnMut(&mut Graph)) -> io::Result<()>,
        force_base: bool,
        retention: bool,
    ) -> io::Result<SyncReport> {
        let _pass = lock(&self.pass);
        let mut report = SyncReport::default();
        // A first pass looks before it captures: twice round.
        for _ in 0..2 {
            let mut captured = None;
            graph(&mut |g| captured = Some(self.capture(g, force_base)))?;
            let c = captured.ok_or_else(|| other("the graph was not handed over"))??;
            let looked = c.looks();
            let mut uploaded = Some(self.upload(c)?);
            let mut acknowledged = None;
            graph(&mut |g| {
                if let Some(u) = uploaded.take() {
                    acknowledged = Some(self.acknowledge(g, u));
                }
            })?;
            report = acknowledged.ok_or_else(|| other("the graph was not handed back"))?;
            if !looked {
                break;
            }
        }
        if retention && self.cfg.retention_enabled {
            report.deleted = self.enforce_retention(&report.generation)?;
        }
        if let Ok(t) = now() {
            self.metrics.last_sync.store(t, Ordering::Relaxed);
        }
        Ok(report)
    }

    /// A whole pass over a graph behind a `Mutex`: locked to capture and to
    /// acknowledge, not to upload.
    pub fn sync(
        &self,
        g: &Mutex<Graph>,
        force_base: bool,
        retention: bool,
    ) -> io::Result<SyncReport> {
        self.sync_with(
            &mut |f| {
                f(&mut lock(g));
                Ok(())
            },
            force_base,
            retention,
        )
    }

    /// Committed log the replica does not hold yet, by what this replicator
    /// knows of it: None before it has looked.
    pub fn lag(&self, g: &Graph) -> Option<u64> {
        lag_of(lock(&self.remote).as_ref(), g)
    }

    /// Retention for this replica, the database being on `generation`
    /// (spelled as keys spell it): see [`paged::expire`].
    pub fn enforce_retention(&self, generation: &str) -> io::Result<usize> {
        let deleted = paged::expire(self.backend.as_ref(), generation, self.cfg.retention)?;
        self.metrics
            .deletions
            .fetch_add(deleted as u64, Ordering::Relaxed);
        Ok(deleted)
    }
}

fn lag_of(r: Option<&Remote>, g: &Graph) -> Option<u64> {
    let r = r.filter(|r| r.generation == g.generation())?;
    let (_, committed) = g.log_span()?;
    Some(committed.saturating_sub(if r.base.is_some() { r.complete_to } else { 0 }))
}

/// Committed log a replica does not hold, for a memory database whose last
/// commit is at `committed`, from a listing of its generation: everything
/// when there is no base to restore from.
pub fn lag(l: &Listing, committed: u64) -> u64 {
    committed.saturating_sub(if l.bases.is_empty() {
        0
    } else {
        l.complete_to()
    })
}

// ------------------------------------------------------------------- restore

/// A memory graph from the backend: the newest base at or before `as_of`
/// and the log shipped after it, up to `as_of` and stopping at a gap, as
/// `paged::restore` reads them, replayed into at most `max_memory` bytes of
/// pages. The graph is on the replica's lineage, logging on from where the
/// replay ended, keeping at most `log_cap` bytes of committed log nobody has
/// let go of.
///
/// A base that is not whole and intact — a file of it missing, cut short or
/// damaged, a `base.json` that does not describe its pages — is an error. A
/// damaged or cut-short segment of log ends the log there, as a torn tail
/// ends a file's on open. A replica that does not fit in `max_memory` is
/// [`Error::Full`], and nothing of it stays allocated. A restore holds the
/// base's bytes while it loads them: about twice the base, at its peak.
pub fn restore(
    backend: &dyn Backend,
    generation: Option<&str>,
    as_of: Option<u64>,
    max_memory: u64,
    log_cap: u64,
) -> Result<(Graph, RestoreReport)> {
    let (gen, base, shipped) = paged::restore_plan(backend, generation, as_of)?;
    let prefix = paged::base_prefix(&gen, base.lsn);
    let meta = String::from_utf8_lossy(&backend.get(&format!("{prefix}base.json"))?).into_owned();
    let mut files: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    for e in backend.list(&prefix)? {
        let Some(rel) = e.key.strip_prefix(&prefix) else {
            continue;
        };
        if rel == "base.json" {
            continue;
        }
        let Some(n) = paged::base_file_number(rel) else {
            return Err(Error::Msg(format!(
                "refusing an odd key in the replica: {}",
                e.key
            )));
        };
        files.insert(n, backend.get(&e.key)?);
    }
    let described = (
        paged::field(&meta, "page_size"),
        paged::field(&meta, "pages"),
        paged::field(&meta, "segment_pages"),
    );
    let count = match described {
        (_, Some(pages), Some(seg)) if seg > 0 => pages.div_ceil(seg).max(1),
        _ => files.len() as u64,
    };
    let mut parts: Vec<&[u8]> = Vec::new();
    for n in 0..count {
        let Some(bytes) = usize::try_from(n).ok().and_then(|n| files.get(&n)) else {
            return Err(Error::Msg(format!(
                "the base at log {} of generation {gen} has no {}",
                base.lsn,
                paged::base_file(n as usize)
            )));
        };
        parts.push(bytes);
    }
    // The pages are what the key and base.json say they are, or this is
    // not the base it claims to be.
    let sb = pager::superblock_of(parts.first().copied().unwrap_or(&[]))?;
    let holds = replica::generation_name(&sb.generation);
    if holds != gen || sb.wal_lsn != base.lsn {
        return Err(Error::Msg(format!(
            "the base filed at log {} of generation {gen} holds log {} of generation {holds}",
            base.lsn, sb.wal_lsn
        )));
    }
    if let (Some(ps), Some(pages), Some(seg)) = described {
        if (ps, pages, seg) != (sb.page_size as u64, sb.high_water, sb.seg_pages) {
            return Err(Error::Msg(format!(
                "the base at log {} of generation {gen} is {pages} pages of {ps} bytes in segments \
                 of {seg} by its base.json, and {} of {} in {} by its pages",
                base.lsn, sb.high_water, sb.page_size, sb.seg_pages
            )));
        }
    }
    // Every file of a base is its segment's pages up to the high-water
    // mark, whichever kind of database wrote it, so a file of any other
    // length was cut short or added to. A short one could otherwise pass
    // when what it lost happened to be free pages: refused all the same.
    let ps = sb.page_size as u64;
    let seg = sb.seg_pages.max(1);
    let whole = sb.high_water.div_ceil(seg).max(1);
    if files.len() as u64 != whole {
        return Err(Error::Msg(format!(
            "the base at log {} of generation {gen} has {} files; its pages need {whole}",
            base.lsn,
            files.len()
        )));
    }
    let mut total = 0u64;
    for (n, part) in parts.iter().enumerate() {
        let pages = sb.high_water.saturating_sub(n as u64 * seg).min(seg);
        if part.len() as u64 != pages * ps {
            return Err(Error::Msg(format!(
                "{} of the base at log {} of generation {gen} is {} bytes; its pages are {}",
                paged::base_file(n),
                base.lsn,
                part.len(),
                pages * ps
            )));
        }
        total += part.len() as u64;
    }
    if paged::field(&meta, "bytes").is_some_and(|b| b != total) {
        return Err(Error::Msg(format!(
            "the base at log {} of generation {gen} is {total} bytes; its base.json says otherwise",
            base.lsn
        )));
    }
    let mut log: Vec<(u64, Vec<u8>)> = Vec::with_capacity(shipped.len());
    for s in &shipped {
        log.push((s.seg.offset, backend.get(&s.key)?));
    }
    let log: Vec<(u64, &[u8])> = log.iter().map(|(at, b)| (*at, b.as_slice())).collect();
    let (g, rec) = Graph::from_replica(&parts, &log, max_memory, log_cap)?;
    Ok((
        g,
        RestoreReport {
            generation: gen,
            base,
            segments: shipped.len(),
            through: rec.end,
        },
    ))
}
