//! The replication engine: sync, snapshot, retention, restore.
//!
//! ## Object layout
//!
//! ```text
//! <generation>/segments/<offset:016x>-<unix>.seg     bytes [offset, offset+len)
//! <generation>/snapshots/<offset:016x>-<unix>.snap   bytes [0, offset)
//! ```
//!
//! The timestamp lives in the key because it is the one piece of metadata
//! every backend preserves exactly. `LastModified` changes when an object is
//! copied or lifecycle-transitioned, and a point-in-time restore that lands on
//! the wrong hour because of a storage-class change is a bad afternoon.
//!
//! ## Why a snapshot is just a prefix
//!
//! Litestream snapshots the SQLite database and ships WAL frames relative to
//! it. Glider's file is already a log, so the equivalent of a snapshot is
//! simply *the first N bytes of the file, as one object*. That has a pleasant
//! consequence: a snapshot plus the segments after it reconstructs the file by
//! concatenation — no replay, no special case — and segments below the
//! snapshot can be deleted because the snapshot already contains them.
//!
//! What it does not do is shrink anything. Reclaiming space needs a `COMPACT`
//! by the writing process, which mints a new generation; retention then
//! expires the old one. Replication cannot do that for you without becoming a
//! second writer, and two writers on one file is how databases die.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::backend::Backend;
use super::config::ReplicaConfig;
use crate::store;
use crate::wal::{fmt_unix, now_unix};

// ------------------------------------------------------------------- metrics

#[derive(Default)]
pub struct Metrics {
    pub segments: AtomicU64,
    pub bytes: AtomicU64,
    pub snapshots: AtomicU64,
    pub deletions: AtomicU64,
    pub errors: AtomicU64,
    pub lag_bytes: AtomicU64,
    pub last_sync: AtomicU64,
}

impl Metrics {
    pub fn prometheus(&self, db: &str, replica: &str) -> String {
        let l = format!("{{db=\"{db}\",replica=\"{replica}\"}}");
        let g = |n: &str, help: &str, kind: &str, v: u64| {
            format!("# HELP glider_{n} {help}\n# TYPE glider_{n} {kind}\nglider_{n}{l} {v}\n")
        };
        [
            g("segments_total", "Segments shipped.", "counter", self.segments.load(Ordering::Relaxed)),
            g("bytes_total", "Bytes shipped.", "counter", self.bytes.load(Ordering::Relaxed)),
            g("snapshots_total", "Snapshots written.", "counter", self.snapshots.load(Ordering::Relaxed)),
            g("deletions_total", "Objects removed by retention.", "counter", self.deletions.load(Ordering::Relaxed)),
            g("errors_total", "Replication errors.", "counter", self.errors.load(Ordering::Relaxed)),
            g("lag_bytes", "Committed bytes not yet replicated.", "gauge", self.lag_bytes.load(Ordering::Relaxed)),
            g("last_sync_timestamp_seconds", "Unix time of the last successful sync.", "gauge", self.last_sync.load(Ordering::Relaxed)),
        ]
        .concat()
    }
}

// ---------------------------------------------------------------------- keys

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectKey {
    pub offset: u64,
    pub ts: u64,
}

fn seg_key(gen: &str, offset: u64, ts: u64) -> String {
    format!("{gen}/segments/{offset:016x}-{ts}.seg")
}

fn snap_key(gen: &str, offset: u64, ts: u64) -> String {
    format!("{gen}/snapshots/{offset:016x}-{ts}.snap")
}

/// `.../0000000000001000-1757692800.seg` -> offset 4096, ts 1757692800.
fn parse_key(key: &str, suffix: &str) -> Option<ObjectKey> {
    let name = key.rsplit('/').next()?;
    let stem = name.strip_suffix(suffix)?;
    let (off, ts) = match stem.split_once('-') {
        Some((o, t)) => (o, t.parse().ok()?),
        None => (stem, 0),
    };
    Some(ObjectKey {
        offset: u64::from_str_radix(off, 16).ok()?,
        ts,
    })
}

pub fn generations(backend: &dyn Backend) -> io::Result<Vec<String>> {
    let mut gens: Vec<String> = backend
        .list("")?
        .into_iter()
        .filter_map(|e| e.key.split('/').next().map(|s| s.to_string()))
        .filter(|g| g.len() == 32 && g.chars().all(|c| c.is_ascii_hexdigit()))
        .collect();
    gens.sort();
    gens.dedup();
    Ok(gens)
}

pub struct Listing {
    pub segments: Vec<(ObjectKey, u64)>,
    pub snapshots: Vec<(ObjectKey, u64)>,
}

pub fn listing(backend: &dyn Backend, gen: &str) -> io::Result<Listing> {
    let mut segments = Vec::new();
    let mut snapshots = Vec::new();
    for e in backend.list(&format!("{gen}/"))? {
        if let Some(k) = parse_key(&e.key, ".seg") {
            segments.push((k, e.size));
        } else if let Some(k) = parse_key(&e.key, ".snap") {
            snapshots.push((k, e.size));
        }
    }
    segments.sort_by_key(|(k, _)| k.offset);
    snapshots.sort_by_key(|(k, _)| k.offset);
    Ok(Listing {
        segments,
        snapshots,
    })
}

/// How far an unbroken run of objects reaches from zero, counting a snapshot
/// as covering everything below it.
pub fn contiguous_end(l: &Listing) -> u64 {
    let mut end = l.snapshots.last().map(|(k, _)| k.offset).unwrap_or(0);
    loop {
        let next = l
            .segments
            .iter()
            .find(|(k, _)| k.offset == end)
            .map(|(_, size)| *size);
        match next {
            Some(size) => end += size,
            None => break,
        }
    }
    end
}

// ---------------------------------------------------------------- replicator

pub struct Replicator {
    pub db: PathBuf,
    pub backend: Arc<dyn Backend>,
    pub cfg: ReplicaConfig,
    pub metrics: Arc<Metrics>,
    pub verbose: bool,
    generation: Option<String>,
    shipped: u64,
    last_snapshot: Instant,
    pending_since: Option<Instant>,
}

pub struct Synced {
    pub generation: String,
    pub shipped_bytes: u64,
    pub segments: usize,
    pub snapshot: bool,
}

impl Replicator {
    pub fn new(db: &Path, backend: Arc<dyn Backend>, cfg: ReplicaConfig) -> Replicator {
        Replicator {
            db: db.to_path_buf(),
            backend,
            cfg,
            metrics: Arc::new(Metrics::default()),
            verbose: true,
            generation: None,
            shipped: 0,
            last_snapshot: Instant::now(),
            pending_since: None,
        }
    }

    fn log(&self, msg: &str) {
        if self.verbose {
            println!(
                "[{}] {} {}",
                fmt_unix(now_unix()),
                self.db
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
                msg
            );
        }
    }

    /// One pass: adopt the current generation, ship what is committed, take a
    /// snapshot if it is time, enforce retention if asked.
    ///
    /// `force` ignores the batching thresholds. The daemon leaves it off so
    /// that small writes accumulate into reasonable objects; one-shot commands
    /// turn it on, because "sync now" should mean now.
    pub fn sync(
        &mut self,
        force: bool,
        force_snapshot: bool,
        enforce_retention: bool,
    ) -> io::Result<Synced> {
        let header = store::read_header(&self.db)?;
        if !header.has_generation() {
            return Err(io::Error::other(
                "database is format v1 and has no generation id — run `glider <db> compact` once to upgrade",
            ));
        }
        let gen = header.generation_hex();

        if self.generation.as_deref() != Some(gen.as_str()) {
            // New lineage, or first run against this replica. Find out what the
            // replica already holds so a restart does not re-ship everything.
            let l = listing(self.backend.as_ref(), &gen)?;
            self.shipped = contiguous_end(&l);
            if self.generation.is_some() {
                self.log(&format!("generation -> {gen} (compaction upstream)"));
            } else {
                self.log(&format!(
                    "generation {gen}, replica holds {} bytes -> {}",
                    self.shipped,
                    self.backend.describe()
                ));
            }
            self.generation = Some(gen.clone());
            self.last_snapshot = Instant::now();
        }

        let end = store::scan_committed_end(&self.db, self.shipped)?;
        let pending = end.saturating_sub(self.shipped);
        self.metrics.lag_bytes.store(pending, Ordering::Relaxed);

        if pending > 0 && self.pending_since.is_none() {
            self.pending_since = Some(Instant::now());
        }
        let waited = self.pending_since.map(|t| t.elapsed()).unwrap_or_default();
        let due = force || pending >= self.cfg.min_bytes || waited >= self.cfg.sync_interval;

        let mut shipped_now = 0;
        let mut segments = 0;
        if pending > 0 && due {
            let ts = now_unix();
            let body = read_range(&self.db, self.shipped, end)?;
            self.backend.put(&seg_key(&gen, self.shipped, ts), body)?;
            self.log(&format!("+{pending} bytes at offset {}", self.shipped));
            self.metrics.segments.fetch_add(1, Ordering::Relaxed);
            self.metrics.bytes.fetch_add(pending, Ordering::Relaxed);
            self.metrics.last_sync.store(ts, Ordering::Relaxed);
            self.shipped = end;
            // Re-measure rather than assuming zero: the writer has very likely
            // committed more while that PUT was in flight, and a lag gauge
            // that under-reports is worthless precisely when it matters.
            let now = store::scan_committed_end(&self.db, self.shipped).unwrap_or(self.shipped);
            self.metrics
                .lag_bytes
                .store(now.saturating_sub(self.shipped), Ordering::Relaxed);
            self.pending_since = None;
            shipped_now = pending;
            segments = 1;
        }

        let mut snapshot = false;
        if (force_snapshot || self.last_snapshot.elapsed() >= self.cfg.snapshot_interval)
            && self.shipped > 0
        {
            self.snapshot(&gen)?;
            self.last_snapshot = Instant::now();
            snapshot = true;
        }

        if enforce_retention && self.cfg.retention_enabled {
            self.enforce_retention()?;
        }

        Ok(Synced {
            generation: gen,
            shipped_bytes: shipped_now,
            segments,
            snapshot,
        })
    }

    /// Write bytes `[0, shipped)` as a single object. Everything below it can
    /// then be expired.
    pub fn snapshot(&self, gen: &str) -> io::Result<()> {
        let body = read_range(&self.db, 0, self.shipped)?;
        let ts = now_unix();
        self.backend.put(&snap_key(gen, self.shipped, ts), body)?;
        self.metrics.snapshots.fetch_add(1, Ordering::Relaxed);
        self.log(&format!("snapshot at offset {}", self.shipped));
        Ok(())
    }

    /// Delete what is provably redundant:
    ///
    /// * segments wholly below the newest snapshot, which contains them
    /// * older snapshots, never the newest
    /// * whole generations that have aged out, once the live one has a
    ///   snapshot of its own to restore from
    ///
    /// Nothing inside the retention window is removed.
    pub fn enforce_retention(&self) -> io::Result<usize> {
        let cutoff = now_unix().saturating_sub(self.cfg.retention.as_secs());
        let all = generations(self.backend.as_ref())?;
        let current = self.generation.clone().unwrap_or_default();
        let mut removed = 0usize;

        let current_restorable = !listing(self.backend.as_ref(), &current)?.snapshots.is_empty();

        for gen in &all {
            let l = listing(self.backend.as_ref(), gen)?;

            if gen != &current {
                let newest = l
                    .segments
                    .iter()
                    .map(|(k, _)| k.ts)
                    .chain(l.snapshots.iter().map(|(k, _)| k.ts))
                    .max()
                    .unwrap_or(0);
                if newest < cutoff && current_restorable {
                    for (k, _) in l.segments.iter() {
                        self.backend.delete(&seg_key(gen, k.offset, k.ts))?;
                        removed += 1;
                    }
                    for (k, _) in l.snapshots.iter() {
                        self.backend.delete(&snap_key(gen, k.offset, k.ts))?;
                        removed += 1;
                    }
                    self.log(&format!("retention: dropped generation {gen}"));
                }
                continue;
            }

            let Some(keep) = l.snapshots.last().copied() else {
                continue;
            };
            for (k, size) in l.segments.iter() {
                if k.offset + size <= keep.0.offset && k.ts < cutoff {
                    self.backend.delete(&seg_key(gen, k.offset, k.ts))?;
                    removed += 1;
                }
            }
            for (k, _) in l.snapshots.iter() {
                if k.offset != keep.0.offset && k.ts < cutoff {
                    self.backend.delete(&snap_key(gen, k.offset, k.ts))?;
                    removed += 1;
                }
            }
        }

        if removed > 0 {
            self.metrics
                .deletions
                .fetch_add(removed as u64, Ordering::Relaxed);
            self.log(&format!("retention: removed {removed} objects"));
        }
        Ok(removed)
    }

    /// Sync until `stop` is set.
    pub fn run(&mut self, stop: Arc<AtomicBool>) -> io::Result<()> {
        let mut last_retention = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let retention_due = last_retention.elapsed() >= Duration::from_secs(3600);
            if let Err(e) = self.sync(false, false, retention_due) {
                self.metrics.errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("[glider-stream] {}: {e}", self.db.display());
            }
            if retention_due {
                last_retention = Instant::now();
            }
            std::thread::sleep(self.cfg.sync_interval.min(Duration::from_secs(1)));
        }
        Ok(())
    }

    pub fn position(&self) -> (Option<&str>, u64) {
        (self.generation.as_deref(), self.shipped)
    }
}

fn read_range(path: &Path, from: u64, to: u64) -> io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(from))?;
    let mut buf = vec![0u8; (to - from) as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

// ------------------------------------------------------------------- restore

pub struct RestoreReport {
    pub generation: String,
    pub snapshot: Option<u64>,
    pub segments: usize,
    pub bytes: u64,
}

/// Rebuild a database from a replica.
///
/// Picks the newest snapshot at or before the target time, then applies every
/// segment after it in order, stopping at the target or at the first gap. A
/// gap ends the restore rather than being skipped — a database assembled
/// across a hole is one that fails later, somewhere less obvious.
pub fn restore(
    backend: &dyn Backend,
    generation: Option<&str>,
    as_of: Option<u64>,
    out_path: &Path,
) -> io::Result<RestoreReport> {
    use std::io::Write;

    let gens = generations(backend)?;
    if gens.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no generations found in {}", backend.describe()),
        ));
    }

    let gen = match generation {
        Some(g) => g.to_string(),
        None => {
            let mut best = (0u64, gens[0].clone());
            for g in &gens {
                let l = listing(backend, g)?;
                let newest = l
                    .segments
                    .iter()
                    .map(|(k, _)| k.ts)
                    .chain(l.snapshots.iter().map(|(k, _)| k.ts))
                    .filter(|ts| as_of.map(|limit| *ts <= limit).unwrap_or(true))
                    .max()
                    .unwrap_or(0);
                if newest >= best.0 {
                    best = (newest, g.clone());
                }
            }
            best.1
        }
    };

    let l = listing(backend, &gen)?;
    let base = l
        .snapshots
        .iter()
        .filter(|(k, _)| as_of.map(|limit| k.ts <= limit).unwrap_or(true))
        .max_by_key(|(k, _)| k.offset)
        .map(|(k, _)| *k);

    let mut out = std::fs::File::create(out_path)?;
    let mut end = 0u64;
    let mut used = 0usize;

    if let Some(base) = base {
        let body = backend.get(&snap_key(&gen, base.offset, base.ts))?;
        out.write_all(&body)?;
        end = base.offset;
    }

    let mut stopped_at_target = false;
    while let Some((k, size)) = l.segments.iter().find(|(k, _)| k.offset == end).copied() {
        if let Some(limit) = as_of {
            if k.ts > limit {
                stopped_at_target = true;
                break;
            }
        }
        let body = backend.get(&seg_key(&gen, k.offset, k.ts))?;
        out.write_all(&body)?;
        end += size;
        used += 1;
    }

    // Running out of segments at the end of the log is normal. Running out
    // with more segments sitting *above* the hole means an object is missing,
    // and quietly returning the prefix would hand back a database that opens
    // cleanly and has lost data without saying so.
    if !stopped_at_target {
        if let Some((next, _)) = l.segments.iter().find(|(k, _)| k.offset > end) {
            drop(out);
            let _ = std::fs::remove_file(out_path);
            return Err(io::Error::other(format!(
                "generation {gen} has a gap: bytes end at {end}, but the next segment \
                 starts at {}. {} later segment(s) cannot be applied across it.",
                next.offset,
                l.segments.iter().filter(|(k, _)| k.offset > end).count()
            )));
        }
    }

    out.sync_all()?;
    drop(out);

    if end == 0 {
        let _ = std::fs::remove_file(out_path);
        return Err(io::Error::other(format!(
            "generation {gen} has nothing to restore at that point in time"
        )));
    }

    // Anything that does not start with a valid header is not a restore.
    store::read_header(out_path)
        .map_err(|e| io::Error::other(format!("restored file is not a glider database: {e}")))?;

    Ok(RestoreReport {
        generation: gen,
        snapshot: base.map(|b| b.offset),
        segments: used,
        bytes: end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_round_trip() {
        let k = seg_key("abc", 4096, 1_757_692_800);
        assert_eq!(k, "abc/segments/0000000000001000-1757692800.seg");
        let parsed = parse_key(&k, ".seg").unwrap();
        assert_eq!(parsed.offset, 4096);
        assert_eq!(parsed.ts, 1_757_692_800);
        assert!(parse_key(&k, ".snap").is_none());
    }

    /// A missing object must fail the restore. Returning the prefix would hand
    /// back a database that opens cleanly and has silently lost data.
    #[test]
    fn a_missing_segment_fails_the_restore() {
        use crate::graph::Graph;
        use crate::store::Sync;
        use crate::stream::backend::FileBackend;
        use crate::stream::config::ReplicaConfig;

        let dir = std::env::temp_dir().join(format!("glider-gap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("g.gldb");
        let replica = dir.join("replica");

        let backend = std::sync::Arc::new(FileBackend {
            root: replica.clone(),
        });
        let cfg = ReplicaConfig {
            min_bytes: 0,
            ..Default::default()
        };
        let mut r = Replicator::new(&db, backend.clone(), cfg);
        r.verbose = false;

        // Three separate flushes, so three separate segments.
        for i in 0..3 {
            let mut g = Graph::open(&db, Sync::Always).unwrap();
            g.add_node(&["N".into()], vec![("i".into(), crate::value::Value::Int(i))])
                .unwrap();
            g.commit().unwrap();
            drop(g);
            r.sync(true, false, false).unwrap();
        }

        let gen = crate::store::read_header(&db).unwrap().generation_hex();
        let segs = listing(backend.as_ref(), &gen).unwrap().segments;
        assert!(segs.len() >= 3, "expected several segments, got {}", segs.len());

        // Lose the middle one, the way a failed upload or a lifecycle rule would.
        let victim = segs[1].0;
        backend
            .delete(&seg_key(&gen, victim.offset, victim.ts))
            .unwrap();

        let out = dir.join("restored.gldb");
        let err = match restore(backend.as_ref(), Some(&gen), None, &out) {
            Ok(_) => panic!("a gap must not restore quietly"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("gap"), "unexpected error: {err}");
        assert!(
            !out.exists(),
            "a failed restore must not leave a half database behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn contiguity_counts_a_snapshot_as_coverage() {
        let l = Listing {
            snapshots: vec![(ObjectKey { offset: 100, ts: 1 }, 100)],
            segments: vec![
                (ObjectKey { offset: 100, ts: 2 }, 50),
                (ObjectKey { offset: 150, ts: 3 }, 25),
                // Nothing starts at 175, so 200 is unreachable.
                (ObjectKey { offset: 200, ts: 4 }, 10),
            ],
        };
        assert_eq!(contiguous_end(&l), 175);
    }
}
