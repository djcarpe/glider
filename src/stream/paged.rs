//! glider-stream for paged databases.
//!
//! The paged tailer ([`crate::replica`]) does the part that is easy to get
//! wrong: it holds a checkpoint while it copies a base, ships only whole
//! transactions, and keeps the writer from checkpointing away log it has not
//! shipped. It writes into a local *spool*, exactly as `glider <db> wal tail
//! --to <dir>` does. This module uploads what the tailer completes to a
//! [`Backend`] — S3, a directory, or an `exec:` command — and restores from
//! one.
//!
//! ## Object layout
//!
//! ```text
//! <generation>/base/<lsn:016x>/db                 pages of one checkpoint
//! <generation>/base/<lsn:016x>/data/<n:08>.seg    its segment files, if any
//! <generation>/base/<lsn:016x>/hot                its hot list, if it had one
//! <generation>/base/<lsn:016x>/base.json          uploaded last: the base is whole
//! <generation>/segments/<offset:016x>-<unix>.seg  log bytes from <offset>, whole transactions
//! ```
//!
//! The same shape as the spool, with the shipping time in each segment's key
//! (the one piece of metadata every backend keeps exactly; see
//! [`super::replicator`]). `glider wal` reads both spellings.
//!
//! ## Upload is a reconciliation, not a callback
//!
//! After every tailer pass the spool is compared with what has been uploaded
//! and the difference goes up: base files first and `base.json` last, then
//! segments. A failed upload leaves the item pending for the next pass, so a
//! flaky network delays replication but never loses a segment. Uploaded
//! segments are listed in `<spool>/<generation>/uploaded`; an uploaded base
//! keeps only its `base.json` in the spool (the tailer reads nothing else),
//! and segments wholly below the newest base are dropped from the spool.
//!
//! ## Restore
//!
//! Download the newest base at or before the target time and the log shipped
//! after it into a scratch directory laid out like a spool, then run the same
//! restore `glider wal restore` runs. The result is opened by the caller,
//! which replays the log and checks every tree.
//!
//! ## Carrying on after a restore
//!
//! A database restored from a replica keeps its generation, and its log
//! carries on from where the replica's ended. Its spool is new, so the
//! tailer would take a base; instead, a pass that finds the spool without a
//! base of the database's generation looks at the backend first, and when
//! the backend holds a base of it, with whole log up to a position in the
//! database's own history and nothing past that, it writes the backend's
//! state into the spool as if this spool had shipped it: the base's
//! `base.json`, and the log after it, marked uploaded. The tailer then
//! ships only what was written since. Anything less certain and the tailer
//! takes a base, as it always has.
//!
//! A position is in the database's history when it is the last checkpoint,
//! a commit the log on disk still holds, or the pin's: [`restore`] leaves a
//! pin at the end of the log the restored file replays on open, so its
//! writer keeps the log from there through checkpoints until a replicator
//! takes the pin over. A pin lapses, as any pin does, after
//! `storage::db::PIN_TTL`; a replicator started later than that takes a
//! base.
//!
//! The same keys, the same `base.json` and the same retention serve a
//! `:memory:` database's replica ([`super::memory`]), which is the same
//! objects shipped from memory instead of from a spool.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::backend::Backend;
use super::config::ReplicaConfig;
use super::replicator::Metrics;
use crate::replica::{self, Base, RestoreReport};
use crate::storage::log::{self, Committed};
use crate::storage::{db, pager};
use crate::wal::{self, fmt_unix, now_unix, Segment, TailOptions};

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

fn is_gen(name: &str) -> bool {
    name.len() == 32 && name.chars().all(|c| c.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------- keys

/// Where the files of the base at log `lsn` live: `<gen>/base/<lsn:016x>/`.
pub fn base_prefix(gen: &str, lsn: u64) -> String {
    format!("{gen}/base/{lsn:016x}/")
}

/// A base's segment file `n` under its prefix: `db` for the database file,
/// `data/<n:08>.seg` for the segment files beside it.
pub fn base_file(n: usize) -> String {
    if n == 0 {
        "db".into()
    } else {
        format!("data/{n:08}.seg")
    }
}

/// Which segment file of a base `rel` (a key under its prefix) is, if any.
pub fn base_file_number(rel: &str) -> Option<usize> {
    if rel == "db" {
        return Some(0);
    }
    let digits = rel.strip_prefix("data/")?.strip_suffix(".seg")?;
    if digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|n| *n > 0)
}

/// The key of the segment of log from `offset`, shipped at `ts`.
pub fn segment_key(gen: &str, offset: u64, ts: u64) -> String {
    format!("{gen}/segments/{offset:016x}-{ts}.seg")
}

/// A number field of a `base.json`.
pub(crate) fn field(text: &str, key: &str) -> Option<u64> {
    let at = text.find(&format!("\"{key}\""))? + key.len() + 2;
    let digits: String = text[at..]
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn base_of(text: &str) -> Option<Base> {
    Some(Base {
        lsn: field(text, "lsn")?,
        epoch: field(text, "epoch")?,
        ts: field(text, "ts")?,
        bytes: field(text, "bytes")?,
    })
}

/// Where a replica's spool goes when the config does not say: beside the
/// database, one directory per replica url.
pub fn default_spool(db: &Path, url: &str) -> PathBuf {
    // FNV-1a: stable, short, and not worth a dependency.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let mut name = db.as_os_str().to_owned();
    name.push("-stream");
    PathBuf::from(name).join(format!("{h:016x}"))
}

// ------------------------------------------------------------------- listing

/// A segment as it sits in the backend.
#[derive(Clone, Debug)]
pub struct RemoteSegment {
    pub key: String,
    pub seg: Segment,
}

/// One generation of a paged replica, as the backend has it.
#[derive(Clone, Debug, Default)]
pub struct Listing {
    pub generation: String,
    /// Whole bases (their `base.json` is there), oldest first.
    pub bases: Vec<Base>,
    /// Segments by offset.
    pub segments: Vec<RemoteSegment>,
}

impl Listing {
    /// How far the log reaches, unbroken, from the newest base.
    pub fn complete_to(&self) -> u64 {
        let from = self.bases.last().map(|b| b.lsn).unwrap_or(0);
        let segs: Vec<Segment> = self.segments.iter().map(|s| s.seg).collect();
        replica::reach(&segs, from)
    }

    /// The furthest log position anything of the generation reaches — a
    /// segment's end, or a base — across gaps too; None when the backend
    /// holds nothing of it. A replicator appends to a replica only where
    /// this is also how far the log is whole ([`Listing::complete_to`]).
    pub fn furthest(&self) -> Option<u64> {
        self.segments
            .iter()
            .map(|s| s.seg.end())
            .chain(self.bases.iter().map(|b| b.lsn))
            .max()
    }

    pub fn last_ts(&self) -> u64 {
        let s = self.segments.iter().map(|s| s.seg.ts).max().unwrap_or(0);
        s.max(self.bases.iter().map(|b| b.ts).max().unwrap_or(0))
    }

    pub fn log_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.seg.len).sum()
    }
}

fn parse_segment_key(gen: &str, key: &str, size: u64) -> Option<RemoteSegment> {
    let name = key
        .strip_prefix(&format!("{gen}/segments/"))?
        .strip_suffix(".seg")?;
    let (hex, ts) = name.split_once('-')?;
    Some(RemoteSegment {
        key: key.to_string(),
        seg: Segment {
            offset: u64::from_str_radix(hex, 16).ok()?,
            len: size,
            ts: ts.parse().ok()?,
        },
    })
}

/// Everything the backend holds for `gen`.
pub fn listing(backend: &dyn Backend, gen: &str) -> io::Result<Listing> {
    let mut out = Listing {
        generation: gen.to_string(),
        ..Listing::default()
    };
    for e in backend.list(&format!("{gen}/base/"))? {
        if e.key.ends_with("/base.json") {
            let text = String::from_utf8_lossy(&backend.get(&e.key)?).to_string();
            if let Some(b) = base_of(&text) {
                out.bases.push(b);
            }
        }
    }
    out.bases.sort_by_key(|b| (b.lsn, b.epoch));
    for e in backend.list(&format!("{gen}/segments/"))? {
        if let Some(s) = parse_segment_key(gen, &e.key, e.size) {
            out.segments.push(s);
        }
    }
    out.segments.sort_by_key(|s| s.seg.offset);
    Ok(out)
}

/// Generations of paged replicas in the backend (those with at least one
/// whole base), least recently active first.
pub fn generations(backend: &dyn Backend) -> io::Result<Vec<String>> {
    let mut gens = BTreeSet::new();
    for e in backend.list("")? {
        let mut parts = e.key.splitn(3, '/');
        if let (Some(g), Some("base"), Some(rest)) = (parts.next(), parts.next(), parts.next()) {
            if is_gen(g) && rest.ends_with("/base.json") {
                gens.insert(g.to_string());
            }
        }
    }
    let mut out: Vec<(u64, String)> = Vec::new();
    for g in gens {
        let l = listing(backend, &g)?;
        out.push((l.last_ts(), g));
    }
    out.sort();
    Ok(out.into_iter().map(|(_, g)| g).collect())
}

/// Whether the backend holds a paged replica (rather than a pre-paged one).
pub fn is_paged_replica(backend: &dyn Backend) -> io::Result<bool> {
    Ok(backend.list("")?.iter().any(|e| {
        let mut parts = e.key.splitn(3, '/');
        matches!((parts.next(), parts.next(), parts.next()),
            (Some(g), Some("base"), Some(rest)) if is_gen(g) && rest.ends_with("/base.json"))
    }))
}

// ----------------------------------------------------------------- replicate

/// What one pass did.
#[derive(Debug, Default)]
pub struct SyncReport {
    pub generation: String,
    pub bases_uploaded: usize,
    pub segments_uploaded: usize,
    pub bytes: u64,
    pub deleted: usize,
}

pub struct PagedReplicator {
    pub db: PathBuf,
    pub spool: PathBuf,
    pub backend: Arc<dyn Backend>,
    pub cfg: ReplicaConfig,
    pub metrics: Arc<Metrics>,
    pub verbose: bool,
}

impl PagedReplicator {
    pub fn new(
        db: &Path,
        spool: PathBuf,
        backend: Arc<dyn Backend>,
        cfg: ReplicaConfig,
    ) -> PagedReplicator {
        PagedReplicator {
            db: db.to_path_buf(),
            spool,
            backend,
            cfg,
            metrics: Arc::new(Metrics::default()),
            verbose: false,
        }
    }

    fn note(&self, msg: &str) {
        if self.verbose {
            eprintln!("[glider-stream] {}: {msg}", self.db.display());
        }
    }

    /// One pass: let the tailer ship into the spool, then upload what it
    /// completed. `force_base` takes a new base now; `retention` enforces
    /// the configured retention afterwards.
    pub fn sync(&self, force_base: bool, retention: bool) -> io::Result<SyncReport> {
        fs::create_dir_all(&self.spool)?;
        // Failing to look only costs a base, which is what the tailer would
        // take anyway.
        if let Err(e) = self.adopt() {
            self.note(&format!(
                "could not look at the backend before tailing: {e}"
            ));
        }
        let opts = TailOptions {
            poll: Duration::from_millis(100),
            min_bytes: self.cfg.min_bytes,
            max_delay: self.cfg.sync_interval,
            exec: None,
            once: true,
            quiet: !self.verbose,
        };
        let every = self.cfg.snapshot_interval.as_secs();
        let first = std::cell::Cell::new(force_base);
        let due = |b: &Base| {
            if first.replace(false) {
                return true;
            }
            every > 0 && now_unix().saturating_sub(b.ts) >= every
        };
        let tailed = replica::tail_with(&self.db, &self.spool, &opts, &due);
        // Upload whatever the tailer completed, even if this pass failed
        // part way: it is all whole.
        let mut report = self.upload()?;
        tailed?;
        if retention && self.cfg.retention_enabled {
            report.deleted = self.enforce_retention()?;
        }
        self.metrics.last_sync.store(now_unix(), Ordering::Relaxed);
        if let Ok(lag) = self.lag(&report.generation) {
            self.metrics.lag_bytes.store(lag, Ordering::Relaxed);
        }
        Ok(report)
    }

    /// Upload the spool's whole bases and segments the backend does not have.
    pub fn upload(&self) -> io::Result<SyncReport> {
        let mut report = SyncReport::default();
        let gens = wal::generations(&self.spool)?;
        let current = replica::generation_hex(&self.db).ok();
        for gen in gens {
            if let Some(c) = &current {
                if *c == gen {
                    report.generation = gen.clone();
                }
            }
            // Bases: those whose pages are still in the spool are not up yet.
            for b in replica::bases(&self.spool, &gen)? {
                let dir = self
                    .spool
                    .join(&gen)
                    .join("base")
                    .join(format!("{:016x}", b.lsn));
                if !dir.join("db").exists() {
                    continue;
                }
                let key = base_prefix(&gen, b.lsn);
                let mut files = vec![("db".to_string(), dir.join("db"))];
                if dir.join("hot").is_file() {
                    files.push(("hot".to_string(), dir.join("hot")));
                }
                if let Ok(rd) = fs::read_dir(dir.join("data")) {
                    let mut data: Vec<_> = rd
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect();
                    data.sort();
                    for name in data {
                        files.push((format!("data/{name}"), dir.join("data").join(&name)));
                    }
                }
                for (rel, path) in &files {
                    let body = fs::read(path)?;
                    report.bytes += body.len() as u64;
                    self.backend.put(&format!("{key}{rel}"), body)?;
                }
                // Last: from now on the base is whole in the backend too.
                self.backend
                    .put(&format!("{key}base.json"), fs::read(dir.join("base.json"))?)?;
                // The tailer reads only base.json; the pages can go.
                let _ = fs::remove_file(dir.join("db"));
                let _ = fs::remove_file(dir.join("hot"));
                let _ = fs::remove_dir_all(dir.join("data"));
                report.bases_uploaded += 1;
                self.metrics.snapshots.fetch_add(1, Ordering::Relaxed);
                self.note(&format!(
                    "{gen} base at log {} uploaded ({} bytes)",
                    b.lsn, b.bytes
                ));
            }
            // Segments, in order.
            let done_path = self.spool.join(&gen).join("uploaded");
            let mut done: BTreeSet<u64> = fs::read_to_string(&done_path)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| u64::from_str_radix(l.trim(), 16).ok())
                .collect();
            for s in wal::segments(&self.spool, &gen)? {
                if done.contains(&s.offset) {
                    continue;
                }
                let local = self
                    .spool
                    .join(&gen)
                    .join("segments")
                    .join(format!("{:016x}.seg", s.offset));
                let body = fs::read(&local)?;
                let len = body.len() as u64;
                self.backend.put(&segment_key(&gen, s.offset, s.ts), body)?;
                let mut f = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&done_path)?;
                writeln!(f, "{:016x}", s.offset)?;
                f.sync_data()?;
                done.insert(s.offset);
                report.segments_uploaded += 1;
                report.bytes += len;
                self.metrics.segments.fetch_add(1, Ordering::Relaxed);
                self.metrics.bytes.fetch_add(len, Ordering::Relaxed);
            }
            // The spool keeps only the log from its newest base on.
            if let Some(newest) = replica::bases(&self.spool, &gen)?.last() {
                for s in wal::segments(&self.spool, &gen)? {
                    if s.end() <= newest.lsn && done.contains(&s.offset) {
                        let _ = fs::remove_file(
                            self.spool
                                .join(&gen)
                                .join("segments")
                                .join(format!("{:016x}.seg", s.offset)),
                        );
                    }
                }
            }
        }
        Ok(report)
    }

    /// Committed log not yet in the backend, in bytes.
    pub fn lag(&self, gen: &str) -> io::Result<u64> {
        if gen.is_empty() {
            return Ok(0);
        }
        let l = listing(self.backend.as_ref(), gen)?;
        let replicated = l.complete_to();
        Ok(match log::committed_end(&self.db, replicated)? {
            Committed::To(end) => end.saturating_sub(replicated),
            Committed::Gone => 0,
        })
    }

    /// Delete bases older than the retention period (never the newest), the
    /// segments wholly below the oldest base kept, and generations that have
    /// been idle longer than the retention period (never the current one).
    pub fn enforce_retention(&self) -> io::Result<usize> {
        let current = replica::generation_hex(&self.db).unwrap_or_default();
        let deleted = expire(self.backend.as_ref(), &current, self.cfg.retention)?;
        self.metrics
            .deletions
            .fetch_add(deleted as u64, Ordering::Relaxed);
        Ok(deleted)
    }

    /// When the spool holds no base of the database's generation but the
    /// backend does, write the backend's state into the spool, so the
    /// tailer carries on shipping where the replica ends instead of taking
    /// a base (see "Carrying on after a restore" above). Only when that is
    /// certain: a whole base, whole log after it to a position nothing in
    /// the backend reaches past, and that position the database's last
    /// checkpoint, a commit its log still holds, or where a restore left it
    /// (its pin). Says whether it did.
    fn adopt(&self) -> io::Result<bool> {
        let Ok(sb) = pager::read_superblock(&self.db) else {
            return Ok(false);
        };
        let gen = replica::generation_name(&sb.generation);
        if !replica::bases(&self.spool, &gen)?.is_empty() {
            return Ok(false);
        }
        let l = listing(self.backend.as_ref(), &gen)?;
        let Some(base) = l.bases.last().cloned() else {
            return Ok(false);
        };
        let to = l.complete_to();
        let restored_to = db::read_pin(&self.db).map(|p| p.shipped) == Some(to);
        if l.furthest() != Some(to)
            || (to != sb.wal_lsn && !restored_to && !log::commit_at(&self.db, to)?)
        {
            return Ok(false);
        }
        let dir = self.spool.join(&gen);
        let segs = dir.join("segments");
        fs::create_dir_all(&segs)?;
        // The log first: a spool's base counts once its base.json is there,
        // and a pass cut short here adopts again from the start.
        let mut manifest = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("manifest.jsonl"))?;
        let mut done = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("uploaded"))?;
        let mut at = base.lsn;
        for s in &l.segments {
            if s.seg.end() <= at {
                continue;
            }
            if s.seg.offset > at {
                break;
            }
            let path = segs.join(format!("{:016x}.seg", s.seg.offset));
            let tmp = path.with_extension("partial");
            fs::write(&tmp, self.backend.get(&s.key)?)?;
            fs::rename(&tmp, &path)?;
            writeln!(
                manifest,
                r#"{{"offset":{},"len":{},"ts":{}}}"#,
                s.seg.offset, s.seg.len, s.seg.ts
            )?;
            writeln!(done, "{:016x}", s.seg.offset)?;
            at = s.seg.end();
        }
        manifest.sync_data()?;
        done.sync_data()?;
        let meta = self
            .backend
            .get(&format!("{}base.json", base_prefix(&gen, base.lsn)))?;
        let bdir = dir.join("base").join(format!("{:016x}", base.lsn));
        fs::create_dir_all(&bdir)?;
        fs::write(bdir.join("base.json.partial"), meta)?;
        fs::rename(bdir.join("base.json.partial"), bdir.join("base.json"))?;
        self.note(&format!(
            "{gen} carries on the replica in {}: its base at log {} and log to {to}",
            self.backend.describe(),
            base.lsn
        ));
        Ok(true)
    }

    /// Replicate until `stop`: a pass every sync interval, retention hourly.
    pub fn run(&self, stop: Arc<AtomicBool>) -> io::Result<()> {
        let mut last_retention = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let retention = last_retention.elapsed() >= Duration::from_secs(3600);
            match self.sync(false, retention) {
                Ok(r) => {
                    if retention {
                        last_retention = Instant::now();
                    }
                    if r.bases_uploaded + r.segments_uploaded > 0 {
                        self.note(&format!(
                            "{} +{} base(s), +{} segment(s), {} bytes",
                            r.generation, r.bases_uploaded, r.segments_uploaded, r.bytes
                        ));
                    }
                }
                Err(e) => {
                    self.metrics.errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("[glider-stream] {}: {e} (retrying)", self.db.display());
                }
            }
            std::thread::sleep(self.cfg.sync_interval.max(Duration::from_millis(200)));
        }
        Ok(())
    }
}

// ----------------------------------------------------------------- retention

/// Retention for one replica, whatever kind of database it is of: delete
/// bases older than `retention` (never the newest), the segments wholly
/// below the oldest base kept, and generations idle longer than `retention`
/// — never `current`, the generation the database is on. A base loses its
/// `base.json` first, so a restore never picks one half deleted. Returns
/// how many objects went, a base counting as one.
pub fn expire(backend: &dyn Backend, current: &str, retention: Duration) -> io::Result<usize> {
    let keep_after = now_unix().saturating_sub(retention.as_secs());
    let mut deleted = 0;
    for gen in generations(backend)? {
        let l = listing(backend, &gen)?;
        if gen != current && l.last_ts() < keep_after {
            for e in backend.list(&format!("{gen}/"))? {
                backend.delete(&e.key)?;
                deleted += 1;
            }
            continue;
        }
        let Some(newest) = l.bases.last() else {
            continue;
        };
        let kept: Vec<&Base> = l
            .bases
            .iter()
            .filter(|b| b.ts >= keep_after || b.lsn == newest.lsn)
            .collect();
        let floor = kept.iter().map(|b| b.lsn).min().unwrap_or(newest.lsn);
        for b in &l.bases {
            if kept.iter().any(|k| k.lsn == b.lsn) {
                continue;
            }
            // base.json first: from then on the base is not whole, so a
            // restore never picks it half deleted.
            let prefix = base_prefix(&gen, b.lsn);
            backend.delete(&format!("{prefix}base.json"))?;
            for e in backend.list(&prefix)? {
                backend.delete(&e.key)?;
            }
            deleted += 1;
        }
        for s in &l.segments {
            if s.seg.end() <= floor {
                backend.delete(&s.key)?;
                deleted += 1;
            }
        }
    }
    Ok(deleted)
}

// ------------------------------------------------------------------- restore

/// What a restore reads, to a file or into memory alike: the generation
/// (`generation`, or the most recently active), its newest whole base taken
/// at or before `as_of`, and the segments shipped after that base, in
/// order, up to the first gap or the first shipped after `as_of`.
pub fn restore_plan(
    backend: &dyn Backend,
    generation: Option<&str>,
    as_of: Option<u64>,
) -> io::Result<(String, Base, Vec<RemoteSegment>)> {
    let gen = match generation {
        Some(g) => g.to_string(),
        None => generations(backend)?.pop().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no paged replica in {}", backend.describe()),
            )
        })?,
    };
    let l = listing(backend, &gen)?;
    let base = l
        .bases
        .iter()
        .rev()
        .find(|b| as_of.map(|t| b.ts <= t).unwrap_or(true))
        .cloned()
        .ok_or_else(|| {
            other(format!(
                "generation {gen} has no base at or before that time"
            ))
        })?;
    let mut segments = Vec::new();
    let mut at = base.lsn;
    for s in &l.segments {
        if s.seg.end() <= at {
            continue;
        }
        if s.seg.offset > at || as_of.map(|t| s.seg.ts > t).unwrap_or(false) {
            break;
        }
        segments.push(s.clone());
        at = s.seg.end();
    }
    Ok((gen, base, segments))
}

/// Rebuild a paged database at `out` from the backend: the newest base at or
/// before `as_of` and the log shipped after it, up to `as_of`. Open the
/// result to finish (opening replays the log).
pub fn restore(
    backend: &dyn Backend,
    generation: Option<&str>,
    as_of: Option<u64>,
    out: &Path,
) -> io::Result<RestoreReport> {
    let (gen, base, shipped) = restore_plan(backend, generation, as_of)?;

    let mut scratch = out.as_os_str().to_owned();
    scratch.push(".restoring");
    let scratch = PathBuf::from(scratch);
    let _ = fs::remove_dir_all(&scratch);
    let result = (|| {
        // The base.
        let prefix = base_prefix(&gen, base.lsn);
        let local = scratch
            .join(&gen)
            .join("base")
            .join(format!("{:016x}", base.lsn));
        for e in backend.list(&prefix)? {
            let Some(rel) = e.key.strip_prefix(&prefix) else {
                continue;
            };
            if rel
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            {
                return Err(other(format!(
                    "refusing an odd key in the replica: {}",
                    e.key
                )));
            }
            let path = local.join(rel);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, backend.get(&e.key)?)?;
        }
        // The log after it, up to the first gap or the target time.
        let segs = scratch.join(&gen).join("segments");
        fs::create_dir_all(&segs)?;
        let mut manifest = fs::File::create(scratch.join(&gen).join("manifest.jsonl"))?;
        for s in &shipped {
            fs::write(
                segs.join(format!("{:016x}.seg", s.seg.offset)),
                backend.get(&s.key)?,
            )?;
            writeln!(
                manifest,
                r#"{{"offset":{},"len":{},"ts":{}}}"#,
                s.seg.offset, s.seg.len, s.seg.ts
            )?;
        }
        manifest.sync_all()?;
        let report = replica::restore(&scratch, Some(&gen), as_of, out)?;
        // Where opening it will have replayed to, as a pin: the writer
        // keeps its log from there, so a replicator pointed back at this
        // replica can see the database's history carries the replica's on
        // (see "Carrying on after a restore" above), even after checkpoints.
        // The pin lapses as any pin does; then it takes a base.
        if let Committed::To(end) = log::committed_end(out, report.base.lsn)? {
            db::write_pin(
                out,
                db::Pin {
                    nonce: 0,
                    hold: false,
                    shipped: end,
                },
            )?;
        }
        Ok(report)
    })();
    let _ = fs::remove_dir_all(&scratch);
    result
}

/// A line per generation, for `glider-stream generations`.
pub fn describe(l: &Listing) -> String {
    let newest = l.bases.last();
    format!(
        "{}  {} base(s){}, {} segments ({} bytes), complete to {}, last {}",
        l.generation,
        l.bases.len(),
        newest
            .map(|b| format!(" (newest at log {})", b.lsn))
            .unwrap_or_default(),
        l.segments.len(),
        l.log_bytes(),
        l.complete_to(),
        fmt_unix(l.last_ts())
    )
}
