//! A database: a pager, and for a file-backed one the write-ahead log and the
//! lock, bound together with the rules that make them one durable thing.
//!
//! * **Commit** writes the transaction's commit marker to the log (synced as
//!   the sync mode says), then makes the pager's copy-on-write state the
//!   committed one. A crash after the log write replays the transaction.
//! * **Rollback** cuts the log back and drops the transaction's pages.
//! * **Checkpoint** (automatic, every `checkpoint_bytes` of log) makes the
//!   committed pages durable under a new superblock and deletes the log it
//!   covers, so recovery never replays more than that much.
//! * **Recovery** on open replays committed transactions logged after the
//!   last checkpoint through the caller's `apply`, which re-executes them
//!   against the pages — the same code path that ran them the first time.
//!
//! * **Replication** is cooperative. A replicator (a separate process) leaves
//!   a pin file in the log directory saying how far it has shipped the log,
//!   and whether it is copying a base snapshot. The writer looks at it on
//!   commit, at most once a second, and whenever it is polled: it keeps the
//!   log the replicator has not shipped yet, and for a snapshot it
//!   checkpoints at once under a *hold* (see the pager), so the
//!   checkpoint's pages stay put while they are copied. A pin not refreshed
//!   for [`PIN_TTL`] is ignored: a dead replicator cannot make the log or
//!   the file grow forever.
//!
//! * **A memory database** keeps no log and pays nothing for one, until it
//!   is replicated. Then it is given a [`MemLog`] ([`Db::enable_log`]):
//!   every record a file would log goes there, at the positions a file
//!   would give it, and a replicator ships it, taking bases from
//!   [`Db::snapshot`]. A memory database is restored from what it shipped
//!   by [`Db::from_replica`]: a base's pages, the log after them replayed,
//!   and a log that carries on where the replay ended.
//!
//! What a log record means is up to the layer above; this layer only moves
//! bytes and keeps them transactional.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::log::{self, MemLog, Recovery, Replay, SyncMode, Wal};
use super::pager::{self, Pager, Snapshot};
use super::{SError, SResult};
use crate::store::{new_generation, Lock};

#[derive(Clone, Debug)]
pub struct DbConfig {
    pub pager: pager::Config,
    pub sync: SyncMode,
    /// Checkpoint after this many bytes of log.
    pub checkpoint_bytes: u64,
    pub wal_segment: u64,
    /// Replay holds at most this much of one transaction in memory.
    pub replay_buffer: usize,
    /// Break a lock left by a writer known to be dead.
    pub force: bool,
}

impl Default for DbConfig {
    fn default() -> Self {
        DbConfig {
            pager: pager::Config::default(),
            sync: SyncMode::Normal,
            checkpoint_bytes: 256 << 20,
            wal_segment: super::log::DEFAULT_SEGMENT,
            replay_buffer: 64 << 20,
            force: false,
        }
    }
}

/// A replicator's pin, in `<db>-wal/replica.pin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pin {
    /// Identifies one snapshot request.
    pub nonce: u64,
    /// A snapshot is being copied: hold the checkpoint it copies.
    pub hold: bool,
    /// The log is shipped up to here: keep everything after it.
    pub shipped: u64,
}

/// A pin older than this is ignored.
pub const PIN_TTL: Duration = Duration::from_secs(600);

pub fn pin_path(db: &Path) -> PathBuf {
    super::log::wal_dir(db).join("replica.pin")
}

/// The pin, if there is one and it is fresh.
pub fn read_pin(db: &Path) -> Option<Pin> {
    let path = pin_path(db);
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .unwrap_or(Duration::ZERO);
    if age > PIN_TTL {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let mut it = text.split_whitespace().map(|x| x.parse::<u64>());
    Some(Pin {
        nonce: it.next()?.ok()?,
        hold: it.next()?.ok()? != 0,
        shipped: it.next()?.ok()?,
    })
}

/// Write (or refresh) the pin atomically.
pub fn write_pin(db: &Path, pin: Pin) -> std::io::Result<()> {
    let path = pin_path(db);
    let tmp = path.with_extension("pin-tmp");
    std::fs::write(
        &tmp,
        format!("{} {} {}\n", pin.nonce, pin.hold as u8, pin.shipped),
    )?;
    std::fs::rename(&tmp, &path)
}

pub fn remove_pin(db: &Path) {
    let _ = std::fs::remove_file(pin_path(db));
}

fn unsupported(msg: &str) -> SError {
    SError::Io(io::Error::new(io::ErrorKind::Unsupported, msg))
}

/// A new lineage. One is made from the clock and the process id, which a
/// wasm host has neither of (std panics when asked): there a memory
/// database's log has to come with a replica's lineage.
fn mint_generation() -> SResult<[u8; 16]> {
    if cfg!(target_family = "wasm") {
        return Err(unsupported(
            "a new generation needs a clock and a process id, which this platform lacks",
        ));
    }
    Ok(new_generation())
}

pub struct Db {
    pager: Pager,
    wal: Option<Wal>,
    /// A memory database's log, once it is given one.
    mem_log: Option<MemLog>,
    path: Option<PathBuf>,
    checkpoint_bytes: u64,
    ckpt_lsn: u64,
    /// Log a replicator still needs starts here.
    keep_from: Option<u64>,
    /// The pager's hold state as of the last checkpoint.
    hold_at_checkpoint: (bool, u64),
    pin_checked: Option<Instant>,
    /// Declared last so it is dropped last.
    _lock: Option<Lock>,
}

/// What opening found.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenReport {
    pub created: bool,
    pub replayed_transactions: u64,
    pub replayed_records: u64,
    pub discarded_bytes: u64,
}

impl Db {
    /// An in-memory database.
    pub fn memory(page_size: usize, max_memory: u64) -> Db {
        Db {
            pager: Pager::memory(page_size, max_memory),
            wal: None,
            mem_log: None,
            path: None,
            checkpoint_bytes: u64::MAX,
            ckpt_lsn: 0,
            keep_from: None,
            hold_at_checkpoint: (false, 0),
            pin_checked: None,
            _lock: None,
        }
    }

    /// An in-memory database loaded from a database file's bytes (its state
    /// as of its last checkpoint).
    pub fn from_image(bytes: &[u8], max_memory: u64) -> SResult<Db> {
        Ok(Db {
            pager: Pager::from_image(bytes, max_memory)?,
            wal: None,
            mem_log: None,
            path: None,
            checkpoint_bytes: u64::MAX,
            ckpt_lsn: 0,
            keep_from: None,
            hold_at_checkpoint: (false, 0),
            pin_checked: None,
            _lock: None,
        })
    }

    /// A memory database restored from a replica: the pages of a base, from
    /// its segment files' bytes in order ([`Pager::from_parts`], keeping
    /// its lineage), then the log shipped after it, each segment with the
    /// position it starts at, replayed through `apply` as [`Db::open`]
    /// replays a file's log ([`log::replay_shipped`]: whole committed
    /// transactions, in order, stopping at a gap or a torn or damaged
    /// frame). The database keeps a log from where the replay ended, with
    /// at most `log_cap` bytes of committed log nobody has let go of, so
    /// its positions carry the replica's on. Nothing is left behind by a
    /// restore that fails: a base or a replay that does not fit in
    /// `max_memory` is [`SError::Full`].
    pub fn from_replica(
        parts: &[&[u8]],
        segments: &[(u64, &[u8])],
        max_memory: u64,
        log_cap: u64,
        apply: &mut dyn FnMut(&Pager, &[u8]) -> SResult<()>,
    ) -> SResult<(Db, Recovery)> {
        let pager = Pager::from_parts(parts, max_memory, true)?;
        if pager.generation() == [0; 16] {
            pager.set_generation(mint_generation()?);
        }
        let rec = log::replay_shipped(pager.wal_lsn(), segments, &mut |e| match e {
            Replay::Record(b) => apply(&pager, b),
            Replay::Commit => {
                pager.commit();
                Ok(())
            }
        })?;
        let db = Db {
            pager,
            wal: None,
            mem_log: Some(MemLog::new(rec.end, log_cap)),
            path: None,
            checkpoint_bytes: u64::MAX,
            ckpt_lsn: 0,
            keep_from: None,
            hold_at_checkpoint: (false, 0),
            pin_checked: None,
            _lock: None,
        };
        Ok((db, rec))
    }

    /// Open (creating if absent) the database at `path`. Committed
    /// transactions in the log after the last checkpoint are replayed:
    /// `apply` gets each record, and must re-execute it against the pager.
    pub fn open(
        path: &Path,
        cfg: &DbConfig,
        apply: &mut dyn FnMut(&Pager, &[u8]) -> SResult<()>,
    ) -> SResult<(Db, OpenReport)> {
        let lock = Lock::acquire(path, cfg.force)?;
        let exists = std::fs::metadata(path)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        let mut report = OpenReport::default();
        let pager = if exists {
            Pager::open(path, cfg.pager.cache_bytes)?
        } else {
            // A fresh database: stale log or data from an earlier, deleted
            // file must not be replayed into it.
            let _ = std::fs::remove_dir_all(super::log::wal_dir(path));
            let _ = std::fs::remove_dir_all(pager::segment_path(path, 1).parent().unwrap_or(path));
            report.created = true;
            let p = Pager::create(path, &cfg.pager, new_generation())?;
            crate::store::sync_parent(path);
            p
        };
        let from = pager.wal_lsn();
        let mut err: Option<SError> = None;
        let (wal, rec) = Wal::open(
            path,
            from,
            cfg.sync,
            cfg.wal_segment,
            cfg.replay_buffer,
            &mut |e| {
                if err.is_some() {
                    return Ok(());
                }
                let r = match e {
                    Replay::Record(b) => apply(&pager, b),
                    Replay::Commit => {
                        pager.commit();
                        Ok(())
                    }
                };
                if let Err(e) = r {
                    err = Some(e);
                }
                Ok(())
            },
        )?;
        if let Some(e) = err {
            return Err(e);
        }
        report.replayed_transactions = rec.transactions;
        report.replayed_records = rec.records;
        report.discarded_bytes = rec.discarded;
        let hold_at_checkpoint = pager.hold();
        let mut db = Db {
            pager,
            wal: Some(wal),
            mem_log: None,
            path: Some(path.to_path_buf()),
            checkpoint_bytes: cfg.checkpoint_bytes,
            ckpt_lsn: from,
            keep_from: None,
            hold_at_checkpoint,
            pin_checked: None,
            _lock: Some(lock),
        };
        db.poll_pin(true)?;
        if rec.transactions > 0 {
            // Fold the replayed log into the pages, so the next open starts
            // clean.
            db.checkpoint()?;
        }
        Ok((db, report))
    }

    pub fn pager(&self) -> &Pager {
        &self.pager
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_memory(&self) -> bool {
        self.wal.is_none()
    }

    /// Give a memory database a log from now on, keeping at most `cap`
    /// bytes of committed log nobody has let go of. It starts where the
    /// pages are (their `wal_lsn`: 0 for a new database, the image's for
    /// one loaded keeping its lineage), and the database gets a generation
    /// if it has none. Only between transactions: a log that began inside
    /// one would hold half of it. A database that has a log already keeps
    /// it as it is; a file database has its own, on disk.
    pub fn enable_log(&mut self, cap: u64) -> SResult<()> {
        if self.wal.is_some() {
            return Err(unsupported("a file database keeps its log on disk"));
        }
        if self.mem_log.is_some() {
            return Ok(());
        }
        if self.pager.in_txn() {
            return Err(unsupported("a log can only start between transactions"));
        }
        if self.pager.generation() == [0; 16] {
            self.pager.set_generation(mint_generation()?);
        }
        self.mem_log = Some(MemLog::new(self.pager.wal_lsn(), cap));
        Ok(())
    }

    /// Start a new lineage: a memory database with a log takes a new
    /// generation, its log carrying on at the same positions. A replicator
    /// does this when the replica it ships to holds the database's own
    /// generation past where the database is — the database was restored
    /// from an earlier point and has written since — so the two histories
    /// cannot share one generation's positions.
    pub(crate) fn new_lineage(&mut self) -> SResult<[u8; 16]> {
        if self.mem_log.is_none() {
            return Err(unsupported(
                "only a memory database with a log takes a new lineage",
            ));
        }
        let generation = mint_generation()?;
        self.pager.set_generation(generation);
        Ok(generation)
    }

    /// A memory database's log, if it has one.
    pub fn mem_log(&self) -> Option<&MemLog> {
        self.mem_log.as_ref()
    }

    /// Everything below `below` is shipped: a memory database's log lets
    /// go of it ([`MemLog::release`]).
    pub fn release_log(&mut self, below: u64) {
        if let Some(m) = &mut self.mem_log {
            m.release(below);
        }
    }

    /// The committed state of a memory database with a log, at its last
    /// commit: what a base is made from ([`Pager::snapshot`]).
    pub fn snapshot(&self) -> SResult<Snapshot> {
        let Some(m) = &self.mem_log else {
            return Err(unsupported(
                "only a memory database with a log is snapshotted for a base",
            ));
        };
        self.pager.snapshot(m.committed_lsn())
    }

    /// Log a record of the open transaction: to the file's log, or a memory
    /// database's if it has one (a no-op otherwise).
    pub fn log(&mut self, payload: &[u8]) -> SResult<()> {
        if let Some(w) = &mut self.wal {
            w.append(payload)?;
        } else if let Some(m) = &mut self.mem_log {
            m.append(payload);
        }
        Ok(())
    }

    /// Commit the open transaction, then checkpoint if the log has grown
    /// past the threshold.
    pub fn commit(&mut self) -> SResult<()> {
        if let Some(w) = &mut self.wal {
            w.commit()?;
        }
        if let Some(m) = &mut self.mem_log {
            m.commit();
        }
        self.pager.commit();
        if let Some(w) = &self.wal {
            if w.bytes_since(self.ckpt_lsn) >= self.checkpoint_bytes {
                self.checkpoint()?;
            }
        }
        self.poll_pin(false)
    }

    /// Look at the replicator's pin (at most once a second unless `now`)
    /// and act on it: keep unshipped log, start or release a hold. Call
    /// between transactions.
    pub fn poll_pin(&mut self, now: bool) -> SResult<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !now
            && self
                .pin_checked
                .map(|t| t.elapsed() < Duration::from_secs(1))
                .unwrap_or(false)
        {
            return Ok(());
        }
        self.pin_checked = Some(Instant::now());
        let pin = read_pin(path);
        let (holding, seen) = self.pager.hold();
        let mut checkpoint = false;
        self.keep_from = pin.map(|p| p.shipped);
        match pin {
            // A new snapshot request: hold, and checkpoint now so the
            // replicator has a held checkpoint to copy.
            Some(p) if p.hold && p.nonce != seen => {
                self.pager.set_hold(true, p.nonce);
                checkpoint = true;
            }
            // Still copying under the hold we gave.
            Some(p) if p.hold => {}
            // Done copying, or the replicator went away: release.
            _ if holding => {
                self.pager.set_hold(false, 0);
                checkpoint = true;
            }
            _ => {}
        }
        if checkpoint && !self.pager.in_txn() {
            self.checkpoint()?;
        }
        Ok(())
    }

    pub fn rollback(&mut self) -> SResult<()> {
        if let Some(w) = &mut self.wal {
            w.rollback()?;
        }
        if let Some(m) = &mut self.mem_log {
            m.rollback();
        }
        self.pager.rollback();
        Ok(())
    }

    /// Make everything committed durable in the pages and drop the log that
    /// covered it. Call between transactions.
    pub fn checkpoint(&mut self) -> SResult<()> {
        let Some(w) = &mut self.wal else {
            return Ok(());
        };
        // The log must be on disk up to the commit point before the
        // superblock claims to cover it.
        w.flush()?;
        let lsn = w.committed_lsn();
        self.pager.checkpoint(lsn)?;
        self.hold_at_checkpoint = self.pager.hold();
        // Keep what a replicator has yet to ship.
        w.truncate_before(lsn.min(self.keep_from.unwrap_or(u64::MAX)))?;
        self.ckpt_lsn = lsn;
        Ok(())
    }

    /// Force the log to disk, whatever the sync mode.
    pub fn flush(&mut self) -> SResult<()> {
        if let Some(w) = &mut self.wal {
            w.flush()?;
        }
        Ok(())
    }

    pub fn set_checkpoint_bytes(&mut self, b: u64) {
        self.checkpoint_bytes = b;
    }

    pub fn set_sync(&mut self, s: SyncMode) {
        if let Some(w) = &mut self.wal {
            w.set_sync(s);
        }
    }

    /// Log bytes a crash right now would replay.
    pub fn log_since_checkpoint(&self) -> u64 {
        self.wal
            .as_ref()
            .map(|w| w.bytes_since(self.ckpt_lsn))
            .unwrap_or(0)
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // A clean close checkpoints, so the next open replays nothing. A
        // failure here loses nothing: the log still has it. Nothing to fold
        // in (a read-only session), nothing to write.
        let idle = self
            .wal
            .as_ref()
            .map(|w| w.committed_lsn() == self.ckpt_lsn)
            .unwrap_or(true)
            && self.pager.hold() == self.hold_at_checkpoint;
        if self.wal.is_some() && !self.pager.in_txn() && !idle {
            let _ = self.checkpoint();
        }
    }
}
