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
//!   and whether it is copying a base snapshot. The writer looks at it at
//!   most once a second, on commit: it keeps the log the replicator has not
//!   shipped yet, and for a snapshot it checkpoints at once under a *hold*
//!   (see the pager), so the checkpoint's pages stay put while they are
//!   copied. A pin not refreshed for [`PIN_TTL`] is ignored: a dead
//!   replicator cannot make the log or the file grow forever.
//!
//! * **The hot set** (when the pager has one) is written down beside the
//!   database at every checkpoint and on a clean close, and loaded back
//!   from there by [`Db::prewarm`] at the next open, after recovery. See
//!   `storage::hotlist`: the list is advice, checked before it is used.
//!
//! What a log record means is up to the layer above; this layer only moves
//! bytes and keeps them transactional.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::log::{Replay, SyncMode, Wal};
use super::pager::{self, Pager};
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

pub struct Db {
    pager: Pager,
    wal: Option<Wal>,
    path: Option<PathBuf>,
    checkpoint_bytes: u64,
    ckpt_lsn: u64,
    /// Log a replicator still needs starts here.
    keep_from: Option<u64>,
    /// The pager's hold state as of the last checkpoint.
    hold_at_checkpoint: (bool, u64),
    pin_checked: Option<Instant>,
    /// The epoch of the checkpoint the database opened from, before any
    /// replay: a hot list taken after it is not this file's.
    opened_epoch: u64,
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
            path: None,
            checkpoint_bytes: u64::MAX,
            ckpt_lsn: 0,
            keep_from: None,
            hold_at_checkpoint: (false, 0),
            pin_checked: None,
            opened_epoch: 0,
            _lock: None,
        }
    }

    /// An in-memory database loaded from a database file's bytes (its state
    /// as of its last checkpoint).
    pub fn from_image(bytes: &[u8], max_memory: u64) -> SResult<Db> {
        Ok(Db {
            pager: Pager::from_image(bytes, max_memory)?,
            wal: None,
            path: None,
            checkpoint_bytes: u64::MAX,
            ckpt_lsn: 0,
            keep_from: None,
            hold_at_checkpoint: (false, 0),
            pin_checked: None,
            opened_epoch: 0,
            _lock: None,
        })
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
        let opened_epoch = pager.durable_epoch();
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
            path: Some(path.to_path_buf()),
            checkpoint_bytes: cfg.checkpoint_bytes,
            ckpt_lsn: from,
            keep_from: None,
            hold_at_checkpoint,
            pin_checked: None,
            opened_epoch,
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

    /// Log a record of the open transaction (no-op in memory).
    pub fn log(&mut self, payload: &[u8]) -> SResult<()> {
        if let Some(w) = &mut self.wal {
            w.append(payload)?;
        }
        Ok(())
    }

    /// Commit the open transaction, then checkpoint if the log has grown
    /// past the threshold.
    pub fn commit(&mut self) -> SResult<()> {
        if let Some(w) = &mut self.wal {
            w.commit()?;
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
        // The hot list first: whoever sees the new superblock (a replicator
        // copying it) finds its list. Should the checkpoint fail, the list
        // names a checkpoint the file never reached, and is not used.
        self.save_hot(Some(self.pager.committed_epoch()));
        let Some(w) = &mut self.wal else {
            return Ok(());
        };
        self.pager.checkpoint(lsn)?;
        self.hold_at_checkpoint = self.pager.hold();
        // Keep what a replicator has yet to ship.
        w.truncate_before(lsn.min(self.keep_from.unwrap_or(u64::MAX)))?;
        self.ckpt_lsn = lsn;
        Ok(())
    }

    /// Write the hot set down beside the database (`<db>-hot`), as of the
    /// checkpoint `epoch` is about to write, or with None the one in force
    /// (idle, between transactions, nothing to fold in). Advice only: a
    /// failure costs the next open its warmth, nothing else. Without a hot
    /// set nothing is written.
    fn save_hot(&self, epoch: Option<u64>) {
        let (Some(path), Some(mut list)) = (&self.path, self.pager.hot_list()) else {
            return;
        };
        if let Some(e) = epoch {
            list.epoch = e;
        }
        let _ = super::hotlist::write(&super::hotlist::path(path), &list);
    }

    /// Load the hot set a previous open (or the base this database was
    /// restored from) left beside it, if the pager has a hot set to load it
    /// into and the list is this database's and not from a checkpoint it
    /// never reached. Returns the pages made hot. See [`super::hotlist`].
    pub fn prewarm(&self) -> u64 {
        let Some(path) = &self.path else {
            return 0;
        };
        if self.pager.hot_list().is_none() {
            return 0;
        }
        let file = super::hotlist::path(path);
        let Some(list) = super::hotlist::read(&file, self.pager.high_water()) else {
            return 0;
        };
        if list.generation != self.pager.generation()
            || list.page_size as usize != self.pager.page_size()
            || list.epoch > self.opened_epoch
        {
            return 0;
        }
        self.pager.prewarm(&list.pages, list.epoch)
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
        if self.wal.is_some() && !self.pager.in_txn() {
            if !idle {
                let _ = self.checkpoint();
            } else {
                // Nothing to fold in, but what is hot may have changed.
                self.save_hot(None);
            }
        }
    }
}
