//! Read replicas: a database that follows another's log from object storage.
//!
//! The primary replicates as glider-stream ships it — base snapshots plus
//! whole transactions of write-ahead log (`src/stream/paged.rs`). A follower
//! restores the newest base into a directory of its own, then keeps polling
//! the replica for log past its position and applies it with
//! [`Graph::apply_shipped`]: the bytes are appended to its own log
//! unchanged and replayed into its pages. So the follower's log is a
//! byte-identical copy of the primary's at the same positions, its position
//! in the primary's log is its own commit point, and a follower that
//! crashes reopens where it was by ordinary recovery.
//!
//! The follower is a separate copy, not a reader of the primary's file: it
//! works on another machine, needs no shared disk, and the primary never
//! waits for it. It refuses writes ([`Graph::set_read_only`]).
//!
//! It starts over from a fresh base when the log it needs is gone: the
//! primary began a new generation (a compaction, a torn-tail recovery, a
//! restore), or retention deleted log older than the newest base before the
//! follower read it. The new copy is built beside the old one while the old
//! one keeps serving, then swapped in.
//!
//! ```text
//! <dir>/<generation>-<n>.gldb      the copy in use (and its -wal/, -data/)
//! ```

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io};

use crate::graph::{Graph, OpenOptions};
use crate::stream::backend::Backend;
use crate::stream::paged;

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// What a follower reports about itself (`GET /replica`).
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub source: String,
    pub generation: String,
    /// This copy's position in the primary's log.
    pub position: u64,
    /// How far the replica's log reaches, unbroken, as of the last poll.
    pub available: u64,
    pub applied_bytes: u64,
    pub applied_transactions: u64,
    pub restores: u64,
    pub last_poll: u64,
    /// When the copy last moved forward (or was restored).
    pub last_change: u64,
    pub error: Option<String>,
}

impl Status {
    pub fn lag_bytes(&self) -> u64 {
        self.available.saturating_sub(self.position)
    }

    pub fn to_json(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("{{\"source\":{},", json_str(&self.source)));
        s.push_str(&format!("\"generation\":{},", json_str(&self.generation)));
        s.push_str(&format!(
            "\"position\":{},\"available\":{},\"lag_bytes\":{},",
            self.position,
            self.available,
            self.lag_bytes()
        ));
        s.push_str(&format!(
            "\"applied_bytes\":{},\"applied_transactions\":{},\"restores\":{},",
            self.applied_bytes, self.applied_transactions, self.restores
        ));
        s.push_str(&format!("\"last_poll\":{},\"last_change\":{},", self.last_poll, self.last_change));
        s.push_str(&format!(
            "\"error\":{}}}",
            self.error.as_deref().map(json_str).unwrap_or_else(|| "null".into())
        ));
        s
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::new();
    crate::value::write_json_string(s, &mut out);
    out
}

/// The status of the follower running in this process, if there is one.
fn global() -> &'static Mutex<Option<Status>> {
    static S: OnceLock<Mutex<Option<Status>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// `GET /replica`: this process's follower status, or None when it is not a
/// replica.
pub fn status_json() -> Option<String> {
    global().lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(Status::to_json)
}

/// What one poll found to do.
#[derive(Debug)]
pub enum Plan {
    /// Up to date with what the replica has.
    Idle { available: u64 },
    /// Log to apply, from exactly the current position.
    Apply { bytes: Vec<u8>, available: u64 },
    /// The log we need is gone: start over from a base of this generation.
    Restore { generation: String, reason: String },
}

pub struct Follower {
    backend: Arc<dyn Backend>,
    dir: PathBuf,
    opts: OpenOptions,
    source: String,
    /// At most this much log is applied per step, so a far-behind follower
    /// does not hold the graph for long while catching up.
    pub max_step: u64,
    /// How often to check for a newer generation when nothing else changed.
    pub generation_check: Duration,
    last_generation_check: Option<Instant>,
}

impl Follower {
    pub fn new(backend: Arc<dyn Backend>, dir: &Path, opts: OpenOptions) -> Follower {
        let source = backend.describe();
        Follower {
            backend,
            dir: dir.to_path_buf(),
            opts,
            source,
            max_step: 64 << 20,
            generation_check: Duration::from_secs(30),
            last_generation_check: None,
        }
    }

    fn read_only_reason(&self) -> String {
        format!("a replica following {}", self.source)
    }

    /// Open the local copy, restoring one from the replica if there is none.
    pub fn open(&self) -> io::Result<Graph> {
        fs::create_dir_all(&self.dir)?;
        let local = self.local_copies()?;
        let g = match local.last() {
            Some(path) => {
                let mut g = Graph::open_opts(path, self.opts.clone()).map_err(|e| other(e.to_string()))?;
                g.set_read_only(Some(self.read_only_reason()));
                g
            }
            None => self.restore(None)?,
        };
        self.publish(|s| {
            s.source = self.source.clone();
            s.generation = g.generation_hex();
            s.position = g.log_position();
            s.last_change = now_unix();
        });
        Ok(g)
    }

    /// Local copies, oldest first (by the number in their name).
    fn local_copies(&self) -> io::Result<Vec<PathBuf>> {
        let mut out: Vec<(u64, PathBuf)> = Vec::new();
        for e in fs::read_dir(&self.dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            let Some(stem) = name.strip_suffix(".gldb") else { continue };
            let Some((gen, n)) = stem.rsplit_once('-') else { continue };
            if gen.len() != 32 || !gen.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            if let Ok(n) = n.parse::<u64>() {
                out.push((n, e.path()));
            }
        }
        out.sort();
        Ok(out.into_iter().map(|(_, p)| p).collect())
    }

    /// Build a fresh copy from the newest base of `generation` (or of the
    /// newest generation) and the log after it, and open it read-only.
    pub fn restore(&self, generation: Option<&str>) -> io::Result<Graph> {
        fs::create_dir_all(&self.dir)?;
        let gen = match generation {
            Some(g) => g.to_string(),
            None => paged::generations(self.backend.as_ref())?
                .pop()
                .ok_or_else(|| other(format!("no paged replica in {}", self.source)))?,
        };
        let n = self
            .local_copies()?
            .iter()
            .filter_map(|p| {
                p.file_stem()?.to_str()?.rsplit_once('-')?.1.parse::<u64>().ok()
            })
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);
        let out = self.dir.join(format!("{gen}-{n}.gldb"));
        remove_copy(&out);
        paged::restore(self.backend.as_ref(), Some(&gen), None, &out).map_err(|e| {
            remove_copy(&out);
            e
        })?;
        let mut g = Graph::open_opts(&out, self.opts.clone()).map_err(|e| {
            remove_copy(&out);
            other(e.to_string())
        })?;
        g.set_read_only(Some(self.read_only_reason()));
        self.publish(|s| {
            s.restores += 1;
            s.generation = g.generation_hex();
            s.position = g.log_position();
            s.last_change = now_unix();
        });
        Ok(g)
    }

    /// Delete every local copy except `keep`.
    pub fn remove_others(&self, keep: &Path) {
        if let Ok(copies) = self.local_copies() {
            for p in copies {
                if p != keep {
                    remove_copy(&p);
                }
            }
        }
    }

    /// Decide what to do next for a copy of `generation` at `position`.
    /// Talks to the backend only; the graph is not needed (or locked).
    pub fn plan(&mut self, generation: &str, position: u64) -> io::Result<Plan> {
        let prefix = format!("{generation}/segments/");
        let mut segs: Vec<paged::RemoteSegment> = self
            .backend
            .list(&prefix)?
            .into_iter()
            .filter_map(|e| paged::parse_segment_key(generation, &e.key, e.size))
            .collect();
        segs.sort_by_key(|s| s.seg.offset);
        let available = crate::replica::reach(
            &segs.iter().map(|s| s.seg).collect::<Vec<_>>(),
            position,
        )
        .max(position);

        // Log to apply: from exactly `position`, contiguous, up to max_step.
        let mut bytes: Vec<u8> = Vec::new();
        let mut at = position;
        let mut gap = false;
        for s in &segs {
            if s.seg.end() <= at {
                continue;
            }
            if s.seg.offset > at {
                gap = true;
                break;
            }
            let body = self.backend.get(&s.key)?;
            if body.len() as u64 != s.seg.len {
                return Err(other(format!(
                    "{} is {} bytes, listed as {}",
                    s.key,
                    body.len(),
                    s.seg.len
                )));
            }
            bytes.extend_from_slice(&body[(at - s.seg.offset) as usize..]);
            at = s.seg.end();
            if bytes.len() as u64 >= self.max_step {
                break;
            }
        }
        if !bytes.is_empty() {
            return Ok(Plan::Apply { bytes, available });
        }

        // Nothing to apply. Is the log we need gone?
        let due = self
            .last_generation_check
            .map(|t| t.elapsed() >= self.generation_check)
            .unwrap_or(true);
        if gap || due {
            self.last_generation_check = Some(Instant::now());
            let listing = paged::listing(self.backend.as_ref(), generation)?;
            if let Some(b) = listing.bases.last() {
                if gap && b.lsn > position {
                    return Ok(Plan::Restore {
                        generation: generation.to_string(),
                        reason: format!(
                            "the log from {position} is gone; the newest base is at {}",
                            b.lsn
                        ),
                    });
                }
            }
            if let Some(newest) = paged::generations(self.backend.as_ref())?.pop() {
                if newest != generation {
                    return Ok(Plan::Restore {
                        generation: newest.clone(),
                        reason: format!("the primary started generation {newest}"),
                    });
                }
            }
        }
        Ok(Plan::Idle { available })
    }

    /// One step against `g`, which the caller holds: plan, then apply or
    /// restore. A restore returns the new graph for the caller to swap in.
    /// For a server, prefer [`Follower::run`], which keeps the network and
    /// the restore outside the graph's lock.
    pub fn step(&mut self, g: &mut Graph) -> io::Result<Option<Graph>> {
        let (gen, pos) = (g.generation_hex(), g.log_position());
        match self.plan(&gen, pos)? {
            Plan::Idle { available } => {
                self.publish(|s| {
                    s.available = available;
                    s.last_poll = now_unix();
                    s.error = None;
                });
                Ok(None)
            }
            Plan::Apply { bytes, available } => {
                self.apply(g, &bytes, available)?;
                Ok(None)
            }
            Plan::Restore { generation, .. } => self.restore(Some(&generation)).map(Some),
        }
    }

    fn apply(&self, g: &mut Graph, bytes: &[u8], available: u64) -> io::Result<()> {
        let mut txns = 0u64;
        crate::storage::log::for_each_record(bytes, &mut |r| {
            if r.is_none() {
                txns += 1;
            }
            Ok(())
        })?;
        let at = g.apply_shipped(bytes).map_err(|e| other(e.to_string()))?;
        self.publish(|s| {
            s.position = at;
            s.available = available.max(at);
            s.applied_bytes += bytes.len() as u64;
            s.applied_transactions += txns;
            s.last_poll = now_unix();
            s.last_change = now_unix();
            s.error = None;
        });
        Ok(())
    }

    /// Follow for ever on the shared graph a server is serving, polling
    /// every `interval`. The network and any restore happen outside the
    /// lock; applying a step holds it briefly. Returns only if `stop` says.
    pub fn run(mut self, shared: Arc<Mutex<Graph>>, interval: Duration, stop: &dyn Fn() -> bool) {
        while !stop() {
            let (gen, pos) = {
                let g = lock(&shared);
                (g.generation_hex(), g.log_position())
            };
            let r = match self.plan(&gen, pos) {
                Ok(Plan::Idle { available }) => {
                    self.publish(|s| {
                        s.available = available;
                        s.last_poll = now_unix();
                        s.error = None;
                    });
                    std::thread::sleep(interval);
                    Ok(())
                }
                Ok(Plan::Apply { bytes, available }) => {
                    let mut g = lock(&shared);
                    if g.generation_hex() == gen && g.log_position() == pos {
                        self.apply(&mut g, &bytes, available)
                    } else {
                        Ok(()) // moved under us; plan again
                    }
                }
                Ok(Plan::Restore { generation, reason }) => {
                    eprintln!("replica: restoring: {reason}");
                    self.restore(Some(&generation)).map(|fresh| {
                        let path = fresh.path().map(Path::to_path_buf);
                        let old = std::mem::replace(&mut *lock(&shared), fresh);
                        drop(old);
                        if let Some(p) = path {
                            self.remove_others(&p);
                        }
                    })
                }
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                eprintln!("replica: {e}");
                self.publish(|s| {
                    s.error = Some(e.to_string());
                    s.last_poll = now_unix();
                });
                std::thread::sleep(interval.max(Duration::from_secs(1)));
            }
        }
    }

    fn publish(&self, f: impl FnOnce(&mut Status)) {
        let mut st = global().lock().unwrap_or_else(|e| e.into_inner());
        let s = st.get_or_insert_with(Status::default);
        if s.source.is_empty() {
            s.source = self.source.clone();
        }
        f(s);
    }
}

fn lock(m: &Mutex<Graph>) -> std::sync::MutexGuard<'_, Graph> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Remove a copy and everything beside it.
fn remove_copy(db: &Path) {
    let _ = fs::remove_file(db);
    for suffix in ["-wal", "-data", "-tmp", ".restoring"] {
        let mut p = db.as_os_str().to_os_string();
        p.push(suffix);
        let _ = fs::remove_dir_all(PathBuf::from(p));
    }
    let _ = fs::remove_file(crate::store::lock_path(db));
}
