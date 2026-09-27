//! Continuous replication of a paged database: base snapshots plus the
//! write-ahead log, in the Litestream mould.
//!
//! A replica directory holds, per database generation:
//!
//! ```text
//! <dir>/<generation>/base/<lsn>/db              pages of one checkpoint
//! <dir>/<generation>/base/<lsn>/data/*.seg      (its segment files, if any)
//! <dir>/<generation>/base/<lsn>/base.json       written last: the base is whole
//! <dir>/<generation>/segments/<lsn>.seg         log bytes from <lsn>, whole transactions
//! <dir>/<generation>/manifest.jsonl             when each segment was shipped
//! ```
//!
//! The replicator runs beside the writer, in its own process, and never
//! blocks it. It ships committed log as it appears, and takes a base at the
//! start and again whenever the log it needs has been checkpointed away.
//! A base is copied page by page while the writer carries on; that is safe
//! because the replicator first asks, through the pin file, for a *hold*:
//! the writer checkpoints and from then on keeps every page of that
//! checkpoint out of reuse until the copy is done (see `storage::db`).
//!
//! Restore takes the newest base at or before the target time, lays the log
//! shipped after it beside it, and opens the result, which replays that log.

use std::fs::{self, File};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::storage::db::{self, Pin};
use crate::storage::log::{self, Committed};
use crate::storage::page;
use crate::storage::pager::{self, SuperInfo};
use crate::wal::{self, Segment, TailOptions};

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

fn sio(e: crate::storage::SError) -> io::Error {
    match e {
        crate::storage::SError::Io(e) => e,
        e => other(e.to_string()),
    }
}

fn hex(g: &[u8; 16]) -> String {
    g.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `db` is a paged database (as opposed to a legacy log file).
pub fn is_paged(db: &Path) -> bool {
    pager::read_superblock(db).is_ok()
}

/// One base snapshot in a replica.
#[derive(Clone, Debug)]
pub struct Base {
    /// The log position it covers up to: replay starts here.
    pub lsn: u64,
    pub epoch: u64,
    pub ts: u64,
    pub bytes: u64,
}

fn base_root(dir: &Path, gen: &str) -> PathBuf {
    dir.join(gen).join("base")
}

fn field(line: &str, key: &str) -> Option<u64> {
    let at = line.find(&format!("\"{key}\""))? + key.len() + 2;
    let digits: String = line[at..]
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Complete bases of a generation, oldest first.
pub fn bases(dir: &Path, gen: &str) -> io::Result<Vec<Base>> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(base_root(dir, gen)) else {
        return Ok(out);
    };
    for e in rd.flatten() {
        let Ok(text) = fs::read_to_string(e.path().join("base.json")) else {
            continue;
        };
        let (Some(lsn), Some(epoch), Some(ts), Some(bytes)) =
            (field(&text, "lsn"), field(&text, "epoch"), field(&text, "ts"), field(&text, "bytes"))
        else {
            continue;
        };
        out.push(Base { lsn, epoch, ts, bytes });
    }
    out.sort_by_key(|b| (b.lsn, b.epoch));
    Ok(out)
}

/// How far the shipped log reaches, unbroken, from `from`.
pub fn reach(segs: &[Segment], from: u64) -> u64 {
    let mut end = from;
    for s in segs {
        if s.offset <= end && s.end() > end {
            end = s.end();
        }
    }
    end
}

// ------------------------------------------------------------------ tailing

struct Tailer<'a> {
    db: &'a Path,
    dir: &'a Path,
    opts: &'a TailOptions,
    gen: String,
    shipped: u64,
    nonce: u64,
}

impl Tailer<'_> {
    fn note(&self, msg: &str) {
        if !self.opts.quiet {
            eprintln!("[glider wal] {msg}");
        }
    }

    fn pin(&self, hold: bool) -> io::Result<()> {
        db::write_pin(
            self.db,
            Pin {
                nonce: self.nonce,
                hold,
                shipped: self.shipped,
            },
        )
    }

    /// Copy the pages of a held checkpoint into a new base.
    fn take_base(&mut self) -> io::Result<Base> {
        self.nonce = (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            ^ ((std::process::id() as u64) << 32))
            .max(1);
        self.pin(true)?;
        let started = Instant::now();
        let mut told = false;
        // With no writer running, the file cannot change: take its lock and
        // copy. Otherwise wait for the writer to checkpoint under the hold,
        // which it does at its next commit.
        let (sb, _lock) = loop {
            if let Ok(lock) = crate::store::Lock::acquire(self.db, false) {
                break (pager::read_superblock(self.db).map_err(sio)?, Some(lock));
            }
            let sb = pager::read_superblock(self.db).map_err(sio)?;
            if sb.hold && sb.pin_seen == self.nonce {
                break (sb, None);
            }
            if !told && started.elapsed() > Duration::from_secs(5) {
                self.note("waiting for the writer to checkpoint for a base snapshot (it does at its next commit)");
                told = true;
            }
            self.pin(true)?;
            std::thread::sleep(Duration::from_millis(100));
        };
        let held = _lock.is_none();
        let mut result = self.copy_base(&sb);
        if held {
            // The hold must have lasted the whole copy: the checkpoint in
            // force still carries it (or none has happened since).
            let now = pager::read_superblock(self.db).map_err(sio);
            let intact = now
                .map(|n| n.epoch == sb.epoch || (n.hold && n.pin_seen == self.nonce))
                .unwrap_or(false);
            if !intact {
                if let Ok(b) = &result {
                    let _ = fs::remove_dir_all(base_root(self.dir, &self.gen).join(format!("{:016x}", b.lsn)));
                }
                result = Err(other(
                    "the writer released its hold during the base copy (was the replicator stalled?)",
                ));
            }
        }
        // Whatever happened, release the hold.
        let released = self.pin(false);
        let base = result?;
        released?;
        Ok(base)
    }

    fn copy_base(&mut self, sb: &SuperInfo) -> io::Result<Base> {
        let ps = sb.page_size as u64;
        let name = format!("{:016x}", sb.wal_lsn);
        let root = base_root(self.dir, &self.gen);
        let tmp = root.join(format!("{name}.partial"));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("data"))?;
        let segs = sb.high_water.div_ceil(sb.seg_pages.max(1)).max(1) as usize;
        let mut bytes = 0u64;
        let mut refreshed = Instant::now();
        for seg in 0..segs {
            let first = seg as u64 * sb.seg_pages;
            let pages = (sb.high_water - first).min(sb.seg_pages);
            let src = pager::segment_path(self.db, seg);
            let dst = if seg == 0 {
                tmp.join("db")
            } else {
                tmp.join("data").join(format!("{seg:08}.seg"))
            };
            let mut from = File::open(&src)?;
            let mut to = io::BufWriter::with_capacity(1 << 20, File::create(&dst)?);
            let mut left = pages * ps;
            let mut buf = vec![0u8; 4 << 20];
            let mut at = 0u64;
            while left > 0 {
                let n = (left as usize).min(buf.len());
                // Pages past the end of a file were never written: zeros.
                let got = read_upto(&mut from, &mut buf[..n])?;
                buf[got..n].fill(0);
                if seg == 0 && at == 0 {
                    // The superblock of the held checkpoint, moved to slot
                    // 0, and nothing newer in slot 1.
                    let mut first = sb.bytes.clone();
                    let epoch = page::epoch(&first);
                    page::rehome(&mut first, 0, epoch);
                    page::seal(&mut first);
                    let ps = ps as usize;
                    buf[..ps].copy_from_slice(&first);
                    buf[ps..2 * ps].fill(0);
                }
                to.write_all(&buf[..n])?;
                left -= n as u64;
                at += n as u64;
                bytes += n as u64;
                if refreshed.elapsed() > Duration::from_secs(2) {
                    self.pin(true)?;
                    refreshed = Instant::now();
                }
            }
            to.flush()?;
            to.get_ref().sync_all()?;
        }
        let ts = wal::now_unix();
        let mut meta = File::create(tmp.join("base.json"))?;
        writeln!(
            meta,
            r#"{{"lsn":{},"epoch":{},"ts":{},"bytes":{},"page_size":{},"pages":{},"segment_pages":{}}}"#,
            sb.wal_lsn, sb.epoch, ts, bytes, ps, sb.high_water, sb.seg_pages
        )?;
        meta.sync_all()?;
        let fin = root.join(&name);
        let _ = fs::remove_dir_all(&fin);
        fs::rename(&tmp, &fin)?;
        Ok(Base {
            lsn: sb.wal_lsn,
            epoch: sb.epoch,
            ts,
            bytes,
        })
    }

    /// Ship committed log from `shipped` to `end` as one segment.
    fn ship(&mut self, end: u64) -> io::Result<Option<Segment>> {
        let seg_dir = self.dir.join(&self.gen).join("segments");
        fs::create_dir_all(&seg_dir)?;
        let from = self.shipped;
        let tmp = seg_dir.join(format!("{from:016x}.partial"));
        {
            let mut f = io::BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
            log::copy_range(self.db, from, end, &mut f)?;
            f.flush()?;
            f.get_ref().sync_all()?;
        }
        // What was read must be whole transactions (a concurrent rollback
        // can make a scan see frames that are gone by the copy).
        if !whole_transactions(&tmp)? {
            let _ = fs::remove_file(&tmp);
            return Ok(None);
        }
        fs::rename(&tmp, seg_dir.join(format!("{from:016x}.seg")))?;
        let ts = wal::now_unix();
        let len = end - from;
        let mut manifest = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(&self.gen).join("manifest.jsonl"))?;
        writeln!(manifest, r#"{{"offset":{from},"len":{len},"ts":{ts}}}"#)?;
        manifest.sync_data()?;
        self.shipped = end;
        Ok(Some(Segment {
            offset: from,
            len,
            ts,
        }))
    }
}

fn read_upto(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn whole_transactions(path: &Path) -> io::Result<bool> {
    let mut r = io::BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut last = crate::storage::log::K_TX_END;
    loop {
        if r.fill_buf()?.is_empty() {
            return Ok(last == crate::storage::log::K_TX_END);
        }
        let mut head = [0u8; 5];
        if r.read_exact(&mut head).is_err() {
            return Ok(false);
        }
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if len > 1 << 30 {
            return Ok(false);
        }
        let mut body = vec![0u8; len + 4];
        if r.read_exact(&mut body).is_err() {
            return Ok(false);
        }
        let mut c = crate::codec::Crc32::new();
        c.update(&head);
        c.update(&body[..len]);
        if c.finish() != u32::from_le_bytes([body[len], body[len + 1], body[len + 2], body[len + 3]]) {
            return Ok(false);
        }
        last = head[0];
    }
}

/// Follow a paged database: take a base, then ship committed log as it
/// appears, taking a new base whenever the log needed has gone.
pub fn tail(db_path: &Path, dir: &Path, opts: &TailOptions) -> io::Result<()> {
    let mut t = Tailer {
        db: db_path,
        dir,
        opts,
        gen: String::new(),
        shipped: 0,
        nonce: 0,
    };
    let mut last_ship = Instant::now();
    loop {
        let sb = match pager::read_superblock(db_path) {
            Ok(sb) => sb,
            Err(e) => {
                if opts.once {
                    return Err(sio(e));
                }
                t.note(&format!("waiting for {}: {e}", db_path.display()));
                std::thread::sleep(opts.poll);
                continue;
            }
        };
        let gen = hex(&sb.generation);
        let mut need_base = false;
        if gen != t.gen {
            t.gen = gen.clone();
            fs::create_dir_all(dir.join(&gen).join("segments"))?;
            match bases(dir, &gen)?.last() {
                Some(b) => t.shipped = reach(&wal::segments(dir, &gen)?, b.lsn),
                None => need_base = true,
            }
        }
        if !need_base {
            match log::committed_end(db_path, t.shipped)? {
                Committed::Gone => {
                    t.note("the log moved past the replica (checkpointed away): taking a new base");
                    need_base = true;
                }
                Committed::To(end) => {
                    let pending = end.saturating_sub(t.shipped);
                    let waited = last_ship.elapsed();
                    if pending > 0 && (pending >= opts.min_bytes || waited >= opts.max_delay || opts.once) {
                        if let Some(seg) = t.ship(end)? {
                            t.note(&format!(
                                "{gen} +{} bytes of log at {} ({})",
                                seg.len,
                                seg.offset,
                                wal::fmt_unix(seg.ts)
                            ));
                            if let Some(cmd) = &opts.exec {
                                run_hook(cmd, dir, &gen, "segments", &format!("{:016x}.seg", seg.offset), seg.offset, seg.len)?;
                            }
                            last_ship = Instant::now();
                        }
                    }
                }
            }
        }
        if need_base {
            let base = t.take_base()?;
            t.shipped = reach(&wal::segments(dir, &gen)?, base.lsn);
            t.note(&format!(
                "{gen} base at log {} (checkpoint {}, {} bytes)",
                base.lsn, base.epoch, base.bytes
            ));
            if let Some(cmd) = &opts.exec {
                run_hook(cmd, dir, &gen, "base", &format!("{:016x}", base.lsn), base.lsn, base.bytes)?;
            }
            continue;
        }
        t.pin(false)?;
        if opts.once {
            return Ok(());
        }
        std::thread::sleep(opts.poll);
    }
}

fn run_hook(cmd: &str, dir: &Path, gen: &str, kind: &str, name: &str, offset: u64, len: u64) -> io::Result<()> {
    let path = dir.join(gen).join(kind).join(name);
    let filled = cmd
        .replace("{path}", &path.to_string_lossy())
        .replace("{name}", name)
        .replace("{gen}", gen)
        .replace("{kind}", kind)
        .replace("{offset}", &offset.to_string())
        .replace("{len}", &len.to_string());
    let status = if cfg!(windows) {
        std::process::Command::new("cmd").arg("/C").arg(&filled).status()
    } else {
        std::process::Command::new("sh").arg("-c").arg(&filled).status()
    }?;
    if !status.success() {
        return Err(other(format!("replication hook failed ({status}): {filled}")));
    }
    Ok(())
}

// ------------------------------------------------------------------ inspect

/// Generations holding at least one base, most recently active last.
pub fn generations(dir: &Path) -> io::Result<Vec<String>> {
    let mut out: Vec<(u64, String)> = Vec::new();
    for g in wal::generations(dir)? {
        let b = bases(dir, &g)?;
        if b.is_empty() {
            continue;
        }
        let last_seg = wal::segments(dir, &g)?.last().map(|s| s.ts).unwrap_or(0);
        let last_base = b.iter().map(|x| x.ts).max().unwrap_or(0);
        out.push((last_seg.max(last_base), g));
    }
    out.sort();
    Ok(out.into_iter().map(|(_, g)| g).collect())
}

pub struct Status {
    pub generation: String,
    pub bases: Vec<Base>,
    pub segments: usize,
    pub log_bytes: u64,
    /// The log is whole from the newest base to here.
    pub complete_to: u64,
    /// A shipped segment beyond a gap (unusable).
    pub gap_at: Option<u64>,
    pub last_ts: u64,
}

pub fn verify(dir: &Path) -> io::Result<Vec<Status>> {
    let mut out = Vec::new();
    for gen in generations(dir)? {
        let b = bases(dir, &gen)?;
        let segs = wal::segments(dir, &gen)?;
        let from = b.last().map(|x| x.lsn).unwrap_or(0);
        let complete_to = reach(&segs, from);
        let gap_at = segs.iter().any(|s| s.offset > complete_to).then_some(complete_to);
        out.push(Status {
            generation: gen,
            segments: segs.len(),
            log_bytes: segs.iter().map(|s| s.len).sum(),
            complete_to,
            gap_at,
            last_ts: segs
                .last()
                .map(|s| s.ts)
                .unwrap_or(0)
                .max(b.iter().map(|x| x.ts).max().unwrap_or(0)),
            bases: b,
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------ restore

pub struct RestoreReport {
    pub generation: String,
    pub base: Base,
    pub segments: usize,
    /// The log position restored to.
    pub through: u64,
}

/// Rebuild a database at `out` from the newest base (at or before `as_of`)
/// and the log shipped after it (up to `as_of`). Open the result to finish:
/// opening replays that log.
pub fn restore(dir: &Path, generation: Option<&str>, as_of: Option<u64>, out: &Path) -> io::Result<RestoreReport> {
    let gen = match generation {
        Some(g) => g.to_string(),
        None => generations(dir)?
            .pop()
            .ok_or_else(|| other(format!("no replicated paged databases in {}", dir.display())))?,
    };
    let all = bases(dir, &gen)?;
    let base = all
        .iter()
        .rev()
        .find(|b| as_of.map(|t| b.ts <= t).unwrap_or(true))
        .cloned()
        .ok_or_else(|| other(format!("generation {gen} has no base at or before that time")))?;
    let src = base_root(dir, &gen).join(format!("{:016x}", base.lsn));

    // Pages.
    let _ = fs::remove_dir_all(log::wal_dir(out));
    let _ = fs::remove_dir_all(pager::segment_path(out, 1).parent().unwrap_or(out));
    fs::copy(src.join("db"), out)?;
    if let Ok(rd) = fs::read_dir(src.join("data")) {
        for e in rd.flatten() {
            let name = e.file_name();
            let dst = pager::segment_path(out, 1).parent().unwrap_or(out).join(&name);
            fs::create_dir_all(dst.parent().unwrap())?;
            fs::copy(e.path(), dst)?;
        }
    }

    // Log after the base, up to the first gap or the target time.
    let wal_dir = log::wal_dir(out);
    fs::create_dir_all(&wal_dir)?;
    let mut w = io::BufWriter::with_capacity(1 << 20, File::create(wal_dir.join(format!("{:016x}.wal", base.lsn)))?);
    let mut at = base.lsn;
    let mut used = 0usize;
    for s in wal::segments(dir, &gen)? {
        if s.end() <= at {
            continue;
        }
        if s.offset > at {
            break; // a gap: nothing after it is usable
        }
        if as_of.map(|t| s.ts > t).unwrap_or(false) {
            break;
        }
        let mut f = File::open(dir.join(&gen).join("segments").join(format!("{:016x}.seg", s.offset)))?;
        let mut skip = at - s.offset;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let from = (skip as usize).min(n);
            skip -= from as u64;
            w.write_all(&buf[from..n])?;
        }
        at = s.end();
        used += 1;
    }
    w.flush()?;
    w.get_ref().sync_all()?;
    Ok(RestoreReport {
        generation: gen,
        base,
        segments: used,
        through: at,
    })
}
