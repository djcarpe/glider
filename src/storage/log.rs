//! The write-ahead log: on disk for a file-backed database, in memory for a
//! `:memory:` one that is replicated.
//!
//! Logical records — whatever payload the layer above hands in (graph ops,
//! for glider) — framed as the original log format framed them: `kind u8 |
//! len u32 | payload | crc32`, with a commit marker ending each transaction.
//! Segments live in `<db>-wal/`, named by the log position (LSN, a byte
//! offset across all segments) they start at.
//!
//! A checkpoint makes the pages durable and records the LSN replay should
//! start from; segments wholly before it are deleted. Recovery replays
//! committed transactions from there and truncates a torn tail, so a crash
//! never exposes part of a transaction.
//!
//! Replay holds at most `max_txn_buffer` bytes of one transaction in memory;
//! a larger transaction is validated in a first pass and re-read to apply,
//! so recovery memory is bounded however large a transaction was.
//!
//! A `:memory:` database has nothing to recover, so it keeps no log — until
//! it is replicated. Then a [`MemLog`] holds the frames a file's log would
//! hold, at the same positions, until a replicator has shipped them. What a
//! replica holds comes back as bytes, and [`replay_shipped`] replays it by
//! the rules recovery replays a file's log by.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::codec::{crc32, Crc32};

pub const K_DATA: u8 = 1;
pub const K_TX_END: u8 = 255;
pub const DEFAULT_SEGMENT: u64 = 64 << 20;
/// Committed log a [`MemLog`] keeps for its replicator, by default, before
/// it drops it all and the replicator takes a new base instead.
pub const DEFAULT_LOG_CAP: u64 = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// fdatasync on every commit.
    Always,
    /// Hand to the OS on every commit.
    Normal,
    /// Buffer.
    Off,
}

pub struct Wal {
    dir: PathBuf,
    seg_bytes: u64,
    /// Open segment: its start LSN, and a buffered writer on it.
    cur_start: u64,
    cur: Option<io::BufWriter<fs::File>>,
    /// LSN of the next byte to write.
    lsn: u64,
    /// LSN just past the last commit marker.
    committed: u64,
    sync: SyncMode,
    pending_records: u64,
}

fn seg_name(lsn: u64) -> String {
    format!("{lsn:016x}.wal")
}

pub fn wal_dir(db: &Path) -> PathBuf {
    let mut d = db.as_os_str().to_os_string();
    d.push("-wal");
    PathBuf::from(d)
}

fn list_segments(dir: &Path) -> io::Result<Vec<u64>> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if let Some(hex) = name.strip_suffix(".wal") {
                if let Ok(lsn) = u64::from_str_radix(hex, 16) {
                    v.push(lsn);
                }
            }
        }
    }
    v.sort_unstable();
    Ok(v)
}

/// What a frame adds around its payload: the kind, the length and the
/// checksum. A commit marker is a frame with no payload.
pub const FRAME_BYTES: u64 = 9;

fn frame(out: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    let start = out.len();
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// One step of replay: a record of a committed transaction, or the end of
/// that transaction.
pub enum Replay<'a> {
    Record(&'a [u8]),
    Commit,
}

/// What recovery found.
#[derive(Debug, Default, Clone, Copy)]
pub struct Recovery {
    pub transactions: u64,
    pub records: u64,
    /// LSN just past the last committed transaction.
    pub end: u64,
    /// Bytes discarded after it (a torn or uncommitted tail).
    pub discarded: u64,
}

impl Wal {
    /// Open the log of `db`, replay every committed transaction from `from`
    /// into `apply` (one call per record; `commit` after each transaction),
    /// truncate anything after the last commit, and position for appending.
    pub fn open(
        db: &Path,
        from: u64,
        sync: SyncMode,
        seg_bytes: u64,
        max_txn_buffer: usize,
        apply: &mut dyn FnMut(Replay<'_>) -> io::Result<()>,
    ) -> io::Result<(Wal, Recovery)> {
        let dir = wal_dir(db);
        fs::create_dir_all(&dir)?;
        let segs = list_segments(&dir)?;
        let mut rec = Recovery {
            end: from,
            ..Default::default()
        };
        // Segments that cover `from` onwards, in order.
        let start_idx = segs.iter().rposition(|s| *s <= from).unwrap_or(0);
        let relevant: Vec<u64> = segs[start_idx..].to_vec();

        // Stream records across segments.
        let mut reader = SegReader::new(&dir, relevant.clone());
        reader.seek(from)?;
        let mut tx: Vec<Vec<u8>> = Vec::new();
        let mut tx_bytes = 0usize;
        let mut tx_start = from;
        let mut spilled = false;
        loop {
            let at = reader.lsn;
            let Some((kind, payload)) = reader.next_record()? else {
                break;
            };
            if kind == K_TX_END {
                if spilled {
                    // Re-read from the start of the transaction and apply.
                    let mut again = SegReader::new(&dir, relevant.clone());
                    again.seek(tx_start)?;
                    while again.lsn < at {
                        let Some((k, p)) = again.next_record()? else {
                            break;
                        };
                        if k == K_DATA {
                            apply(Replay::Record(&p))?;
                        }
                    }
                } else {
                    for p in tx.drain(..) {
                        apply(Replay::Record(&p))?;
                    }
                }
                apply(Replay::Commit)?;
                rec.transactions += 1;
                rec.end = reader.lsn;
                tx.clear();
                tx_bytes = 0;
                spilled = false;
                tx_start = reader.lsn;
            } else {
                rec.records += 1;
                if !spilled {
                    tx_bytes += payload.len();
                    if tx_bytes > max_txn_buffer {
                        spilled = true;
                        tx.clear();
                    } else {
                        tx.push(payload);
                    }
                }
            }
        }
        // Truncate the tail: the segment holding `end` is cut there, later
        // segments are removed.
        let mut end_seg = from;
        for s in &segs {
            if *s <= rec.end {
                end_seg = *s;
            }
        }
        let mut total_after = 0u64;
        for s in &segs {
            let path = dir.join(seg_name(*s));
            let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if *s > rec.end {
                total_after += len;
                fs::remove_file(&path)?;
            } else if *s == end_seg {
                let keep = rec.end - s;
                if len > keep {
                    total_after += len - keep;
                    fs::OpenOptions::new()
                        .write(true)
                        .open(&path)?
                        .set_len(keep)?;
                }
            }
        }
        rec.discarded = total_after;
        let mut w = Wal {
            dir,
            seg_bytes: seg_bytes.max(64),
            cur_start: end_seg,
            cur: None,
            lsn: rec.end,
            committed: rec.end,
            sync,
            pending_records: 0,
        };
        w.open_segment(end_seg)?;
        Ok((w, rec))
    }

    fn open_segment(&mut self, start: u64) -> io::Result<()> {
        let path = self.dir.join(seg_name(start));
        let f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        self.cur_start = start;
        self.cur = Some(io::BufWriter::with_capacity(1 << 20, f));
        Ok(())
    }

    fn write_bytes(&mut self, b: &[u8]) -> io::Result<()> {
        if self.lsn - self.cur_start >= self.seg_bytes {
            // Roll to a new segment at a record boundary.
            if let Some(mut w) = self.cur.take() {
                w.flush()?;
                w.get_ref().sync_data()?;
            }
            self.open_segment(self.lsn)?;
        }
        self.cur.as_mut().expect("open").write_all(b)?;
        self.lsn += b.len() as u64;
        Ok(())
    }

    pub fn append(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut buf = Vec::with_capacity(payload.len() + 9);
        frame(&mut buf, K_DATA, payload);
        self.pending_records += 1;
        self.write_bytes(&buf)
    }

    /// End the transaction: write the commit marker and sync per the mode.
    pub fn commit(&mut self) -> io::Result<()> {
        if self.pending_records == 0 {
            return Ok(());
        }
        let mut buf = Vec::with_capacity(9);
        frame(&mut buf, K_TX_END, &[]);
        self.write_bytes(&buf)?;
        let w = self.cur.as_mut().expect("open");
        match self.sync {
            SyncMode::Always => {
                w.flush()?;
                w.get_ref().sync_data()?;
            }
            SyncMode::Normal => w.flush()?,
            SyncMode::Off => {}
        }
        self.committed = self.lsn;
        self.pending_records = 0;
        Ok(())
    }

    /// Records written since the last commit are abandoned: the log is cut
    /// back to the last commit marker.
    pub fn rollback(&mut self) -> io::Result<()> {
        if self.pending_records == 0 {
            return Ok(());
        }
        if let Some(mut w) = self.cur.take() {
            w.flush()?;
        }
        // Remove segments started after the commit point, cut the rest.
        for s in list_segments(&self.dir)? {
            let path = self.dir.join(seg_name(s));
            if s > self.committed {
                fs::remove_file(&path)?;
            } else if s <= self.committed && self.committed - s < fs::metadata(&path)?.len() {
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)?
                    .set_len(self.committed - s)?;
            }
        }
        let start = list_segments(&self.dir)?
            .into_iter()
            .rfind(|s| *s <= self.committed)
            .unwrap_or(self.committed);
        self.lsn = self.committed;
        self.pending_records = 0;
        self.open_segment(start)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if let Some(w) = self.cur.as_mut() {
            w.flush()?;
            w.get_ref().sync_data()?;
        }
        Ok(())
    }

    pub fn set_sync(&mut self, s: SyncMode) {
        self.sync = s;
    }

    pub fn committed_lsn(&self) -> u64 {
        self.committed
    }

    /// Bytes of log since `lsn`.
    pub fn bytes_since(&self, lsn: u64) -> u64 {
        self.lsn.saturating_sub(lsn)
    }

    /// Delete segments that end at or before `lsn` (a checkpoint covers
    /// them). The open segment is kept.
    pub fn truncate_before(&mut self, lsn: u64) -> io::Result<()> {
        let segs = list_segments(&self.dir)?;
        for (i, s) in segs.iter().enumerate() {
            let next = segs.get(i + 1).copied();
            if *s != self.cur_start && next.map(|n| n <= lsn).unwrap_or(false) {
                fs::remove_file(self.dir.join(seg_name(*s)))?;
            }
        }
        Ok(())
    }
}

/// Where a reader of the log (a replicator) stands.
#[derive(Debug, PartialEq, Eq)]
pub enum Committed {
    /// Committed transactions run from the asked position to this LSN.
    To(u64),
    /// The log no longer holds the asked position: a checkpoint deleted it.
    Gone,
}

/// LSNs of the segments of `db`'s log, ascending.
pub fn segments(db: &Path) -> io::Result<Vec<u64>> {
    list_segments(&wal_dir(db))
}

/// How far committed transactions reach from `from` in `db`'s log. Safe
/// alongside a live writer: a frame still being written fails its CRC and
/// ends the scan.
pub fn committed_end(db: &Path, from: u64) -> io::Result<Committed> {
    let dir = wal_dir(db);
    let segs = list_segments(&dir)?;
    if segs.first().map(|s| *s > from).unwrap_or(true) {
        return Ok(Committed::Gone);
    }
    let mut r = SegReader::new(&dir, segs);
    if let Err(e) = r.seek(from) {
        return if e.kind() == io::ErrorKind::NotFound {
            Ok(Committed::Gone)
        } else {
            Err(e)
        };
    }
    let mut end = from;
    while let Some((kind, _)) = r.next_record()? {
        if kind == K_TX_END {
            end = r.lsn;
        }
    }
    Ok(Committed::To(end))
}

/// Whether a committed transaction of `db`'s log, as it is on disk, ends at
/// `pos`. Segments start at frame boundaries, so the frames are read from
/// the first segment's start. A replicator asks before it carries on
/// shipping from a position it did not ship itself — the end of the log a
/// database was restored from — because a position past the log, or inside
/// a transaction, would otherwise read as committed log that has merely
/// not grown yet. (A checkpoint's own position is a commit too, whether or
/// not the log still reaches it: the caller knows that one.)
pub fn commit_at(db: &Path, pos: u64) -> io::Result<bool> {
    let dir = wal_dir(db);
    let segs = list_segments(&dir)?;
    let Some(&first) = segs.first() else {
        return Ok(false);
    };
    if first >= pos {
        return Ok(false);
    }
    let mut r = SegReader::new(&dir, segs);
    if let Err(e) = r.seek(first) {
        return if e.kind() == io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(e)
        };
    }
    while let Some((kind, _)) = r.next_record()? {
        if r.lsn > pos {
            break;
        }
        if r.lsn == pos {
            return Ok(kind == K_TX_END);
        }
    }
    Ok(false)
}

/// Copy the log bytes `[from, to)` of `db` into `out`.
pub fn copy_range(db: &Path, from: u64, to: u64, out: &mut dyn Write) -> io::Result<()> {
    let dir = wal_dir(db);
    let segs = list_segments(&dir)?;
    let mut at = from;
    for (i, s) in segs.iter().enumerate() {
        if at >= to {
            break;
        }
        let next = segs.get(i + 1).copied().unwrap_or(u64::MAX);
        if next <= at || *s > at {
            continue;
        }
        let mut f = fs::File::open(dir.join(seg_name(*s)))?;
        f.seek(SeekFrom::Start(at - s))?;
        let want = to.min(next) - at;
        let n = io::copy(&mut (&mut f).take(want), out)?;
        at += n;
        if n < want {
            break;
        }
    }
    if at != to {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("log bytes {at}..{to} are missing"),
        ));
    }
    Ok(())
}

/// Check that `bytes`, log from some commit boundary, is whole committed
/// transactions: every frame intact and the last one a commit marker.
pub fn is_whole_transactions(bytes: &[u8]) -> bool {
    let mut at = 0usize;
    let mut last = 0u8;
    while at < bytes.len() {
        if at + 5 > bytes.len() {
            return false;
        }
        let len = u32::from_le_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]])
            as usize;
        let end = at + 5 + len + 4;
        if end > bytes.len() {
            return false;
        }
        let stored = u32::from_le_bytes([
            bytes[end - 4],
            bytes[end - 3],
            bytes[end - 2],
            bytes[end - 1],
        ]);
        if crc32(&bytes[at..at + 5 + len]) != stored {
            return false;
        }
        last = bytes[at];
        at = end;
    }
    bytes.is_empty() || last == K_TX_END
}

// ------------------------------------------------------------- in memory

/// A [`MemLog`] grows and shrinks a chunk of this many bytes at a time.
const CHUNK: usize = 64 << 10;

/// The log of a replicated `:memory:` database: the frames a file's [`Wal`]
/// would write, at the same positions, kept in memory until a replicator
/// has shipped them.
///
/// Commit and rollback mean what they mean on disk: a commit marker ends
/// each transaction, a rollback cuts the log back to the last one, and
/// nothing uncommitted is ever readable as committed. A replicator asks how
/// far committed log reaches from where it has shipped to
/// ([`MemLog::committed_end`]), copies a range whose ends are commits, and
/// so whole transactions ([`MemLog::copy_range`]), and once that is safely
/// elsewhere lets go of it ([`MemLog::release`]).
///
/// Memory follows what has not been let go of, not what was ever written:
/// the bytes live in fixed-size chunks, and a chunk is freed as soon as
/// everything in it is. Committed log nobody has let go of is kept up to
/// `cap` bytes; the commit that takes it past that drops all of it, and a
/// replicator asking for it is told it is [`Committed::Gone`], as a file's
/// replicator is when a checkpoint deleted the log it needed. The answer is
/// the same: take a new base. Only committed log counts against the cap,
/// and the open transaction is never dropped: a rollback cuts it back, and a
/// commit makes it committed log like any other.
pub struct MemLog {
    /// Log bytes from `held` to `lsn`, in chunks of [`CHUNK`] bytes, all
    /// full but the last.
    chunks: VecDeque<Vec<u8>>,
    /// Position of the first byte of the first chunk.
    held: u64,
    /// Where a reader may still start: below it, log has been let go of or
    /// dropped. Always a commit position (or where the log began).
    start: u64,
    /// Position of the next byte.
    lsn: u64,
    /// Position just past the last commit marker.
    committed: u64,
    pending_records: u64,
    /// Committed log past `start` beyond this is dropped at commit.
    cap: u64,
}

impl MemLog {
    /// An empty log whose first byte goes at `start` (where the database's
    /// pages are: 0 for a new one, where replay ended for a restored one),
    /// keeping at most `cap` bytes of committed log nobody has let go of.
    pub fn new(start: u64, cap: u64) -> MemLog {
        MemLog {
            chunks: VecDeque::new(),
            held: start,
            start,
            lsn: start,
            committed: start,
            pending_records: 0,
            cap,
        }
    }

    fn push(&mut self, mut b: &[u8]) {
        self.lsn += b.len() as u64;
        while !b.is_empty() {
            let room = match self.chunks.back() {
                Some(c) if c.len() < CHUNK => CHUNK - c.len(),
                _ => {
                    self.chunks.push_back(Vec::new());
                    CHUNK
                }
            };
            let n = b.len().min(room);
            if let Some(c) = self.chunks.back_mut() {
                if c.capacity() - c.len() < n {
                    // Double, never past a chunk: a small log stays small.
                    let want = (c.capacity() * 2).max(c.len() + n).clamp(256, CHUNK);
                    c.reserve_exact(want - c.len());
                }
                c.extend_from_slice(&b[..n]);
            }
            b = &b[n..];
        }
    }

    /// One frame, laid out as [`frame`] lays it out for the file.
    fn frame(&mut self, kind: u8, payload: &[u8]) {
        let mut head = [kind, 0, 0, 0, 0];
        head[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let mut crc = Crc32::new();
        crc.update(&head);
        crc.update(payload);
        self.push(&head);
        self.push(payload);
        self.push(&crc.finish().to_le_bytes());
    }

    pub fn append(&mut self, payload: &[u8]) {
        self.frame(K_DATA, payload);
        self.pending_records += 1;
    }

    /// End the transaction with a commit marker. Then, if the committed log
    /// nobody has let go of is past the cap, drop it all.
    pub fn commit(&mut self) {
        if self.pending_records == 0 {
            return;
        }
        self.frame(K_TX_END, &[]);
        self.committed = self.lsn;
        self.pending_records = 0;
        if self.committed - self.start > self.cap {
            self.let_go(self.committed);
        }
    }

    /// Records appended since the last commit are abandoned: the log is cut
    /// back to the last commit marker.
    pub fn rollback(&mut self) {
        if self.pending_records == 0 {
            return;
        }
        let keep = (self.committed - self.held) as usize;
        let (whole, part) = (keep / CHUNK, keep % CHUNK);
        if part == 0 {
            self.chunks.truncate(whole);
        } else {
            self.chunks.truncate(whole + 1);
            if let Some(c) = self.chunks.back_mut() {
                c.truncate(part);
            }
        }
        self.lsn = self.committed;
        self.pending_records = 0;
    }

    /// Where the log a reader may still ask for starts.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Position just past the last commit marker.
    pub fn committed_lsn(&self) -> u64 {
        self.committed
    }

    pub fn cap(&self) -> u64 {
        self.cap
    }

    /// Bytes the log occupies in memory.
    pub fn memory_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.capacity() as u64).sum()
    }

    /// How far committed transactions reach from `from`. A position this
    /// log does not hold as a commit — below what it keeps, past its last
    /// commit, or inside a transaction — is [`Committed::Gone`]: whatever a
    /// replicator believed it had shipped, the safe answer is a new base.
    pub fn committed_end(&self, from: u64) -> Committed {
        if self.is_commit(from) {
            Committed::To(self.committed)
        } else {
            Committed::Gone
        }
    }

    /// Copy the log bytes `[from, to)` into `out`. Both ends must be
    /// commits this log holds, so the bytes are whole transactions.
    pub fn copy_range(&self, from: u64, to: u64, out: &mut dyn Write) -> io::Result<()> {
        if from < self.start {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("log bytes {from}..{} are gone", self.start),
            ));
        }
        if from > to || !self.is_commit(from) || !self.is_commit(to) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "log bytes {from}..{to} are not whole transactions of the log held ({}..{})",
                    self.start, self.committed
                ),
            ));
        }
        let mut at = from;
        while at < to {
            let (i, off) = self.locate(at);
            let Some(c) = self.chunks.get(i) else { break };
            let n = c.len().saturating_sub(off).min((to - at) as usize);
            if n == 0 {
                break;
            }
            out.write_all(&c[off..off + n])?;
            at += n as u64;
        }
        if at != to {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("log bytes {at}..{to} are missing"),
            ));
        }
        Ok(())
    }

    /// Everything below `below` is shipped: forget it. The log is let go of
    /// up to the last commit at or before `below`, never into the open
    /// transaction; whole chunks below that are freed.
    pub fn release(&mut self, below: u64) {
        let to = if below >= self.committed {
            self.committed
        } else {
            self.last_commit_at_or_before(below)
        };
        self.let_go(to);
    }

    /// Readers may no longer ask for anything below `to`, a commit position.
    fn let_go(&mut self, to: u64) {
        if to <= self.start {
            return;
        }
        self.start = to;
        while let Some(c) = self.chunks.front() {
            let end = self.held + c.len() as u64;
            if end > to {
                break;
            }
            self.held = end;
            self.chunks.pop_front();
        }
    }

    /// Chunk and offset of position `at`, which the log holds.
    fn locate(&self, at: u64) -> (usize, usize) {
        let rel = (at - self.held) as usize;
        (rel / CHUNK, rel % CHUNK)
    }

    /// Copy the bytes at `at` into `buf`; false if they are not all held.
    fn read_at(&self, at: u64, buf: &mut [u8]) -> bool {
        if at < self.held || at + buf.len() as u64 > self.lsn {
            return false;
        }
        let (mut i, mut off) = self.locate(at);
        let mut done = 0;
        while done < buf.len() {
            let Some(c) = self.chunks.get(i) else {
                return false;
            };
            let n = c.len().saturating_sub(off).min(buf.len() - done);
            if n == 0 {
                return false;
            }
            buf[done..done + n].copy_from_slice(&c[off..off + n]);
            done += n;
            i += 1;
            off = 0;
        }
        true
    }

    /// The last commit at or before `pos`, found by walking frame headers
    /// from `start`. The log wrote every frame itself, so lengths need no
    /// checking; only a position other than `start` or the last commit
    /// costs a walk.
    fn last_commit_at_or_before(&self, pos: u64) -> u64 {
        let limit = pos.min(self.committed);
        let (mut at, mut last) = (self.start, self.start);
        let mut head = [0u8; 5];
        while at < limit && self.read_at(at, &mut head) {
            let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as u64;
            let next = at + 9 + len;
            if next > limit {
                break;
            }
            if head[0] == K_TX_END {
                last = next;
            }
            at = next;
        }
        last
    }

    fn is_commit(&self, pos: u64) -> bool {
        pos == self.start
            || pos == self.committed
            || (pos > self.start
                && pos < self.committed
                && self.last_commit_at_or_before(pos) == pos)
    }
}

/// Log shipped to a replica, as one stream: the segments' bytes end to end
/// from a position, borrowed where they lie.
struct Shipped<'a> {
    /// (position of the first byte, bytes): contiguous, ascending.
    pieces: Vec<(u64, &'a [u8])>,
    /// Position just past the last byte.
    end: u64,
}

impl<'a> Shipped<'a> {
    /// Lay `segments` end to end from `from` as `replica::restore` does: by
    /// position, skipping what is already covered, reading an overlapping
    /// segment from where the stream has got to, stopping at a gap.
    fn new(from: u64, segments: &[(u64, &'a [u8])]) -> Shipped<'a> {
        let mut order = segments.to_vec();
        order.sort_by_key(|s| s.0);
        let mut pieces = Vec::new();
        let mut at = from;
        for (off, bytes) in order {
            let end = off.saturating_add(bytes.len() as u64);
            if end <= at {
                continue;
            }
            if off > at {
                break; // a gap: nothing after it is usable
            }
            pieces.push((at, &bytes[(at - off) as usize..]));
            at = end;
        }
        Shipped { pieces, end: at }
    }

    /// The bytes `[at, at + n)`: borrowed when one piece holds them all.
    fn bytes(&self, at: u64, n: usize) -> Option<Cow<'a, [u8]>> {
        if at.checked_add(n as u64)? > self.end {
            return None;
        }
        let mut i = self.pieces.partition_point(|p| p.0 <= at).checked_sub(1)?;
        let (first, b) = self.pieces[i];
        let off = (at - first) as usize;
        if off + n <= b.len() {
            return Some(Cow::Borrowed(&b[off..off + n]));
        }
        let mut out = Vec::with_capacity(n);
        out.extend_from_slice(&b[off..]);
        while out.len() < n {
            i += 1;
            let (_, b) = *self.pieces.get(i)?;
            let take = b.len().min(n - out.len());
            out.extend_from_slice(&b[..take]);
        }
        Some(Cow::Owned(out))
    }

    /// The frame at `at`: (kind, payload, position after it). None at the
    /// end of the stream, or, when `check`, at a torn or damaged frame.
    fn frame(&self, at: u64, check: bool) -> Option<(u8, Cow<'a, [u8]>, u64)> {
        let head = self.bytes(at, 5)?;
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if len > 1 << 30 {
            return None;
        }
        let body = self.bytes(at + 5, len + 4)?;
        if check {
            let mut crc = Crc32::new();
            crc.update(&head);
            crc.update(&body[..len]);
            let stored =
                u32::from_le_bytes([body[len], body[len + 1], body[len + 2], body[len + 3]]);
            if crc.finish() != stored {
                return None;
            }
        }
        let payload = match body {
            Cow::Borrowed(b) => Cow::Borrowed(&b[..len]),
            Cow::Owned(mut v) => {
                v.truncate(len);
                Cow::Owned(v)
            }
        };
        Some((head[0], payload, at + 9 + len as u64))
    }
}

/// Replay log a replica shipped, from its bytes, by the rules a file's log
/// is replayed by on open ([`Wal::open`]).
///
/// `segments` are pieces of log, each with the position it starts at, in
/// any order. They are read as one stream from `from` (where the base's
/// pages are), laid end to end as `replica::restore` lays them out: by
/// position, what the stream already covers skipped, an overlapping segment
/// read from where the stream has got to, and a segment that starts past
/// the end of the stream — a gap — ending it. Only committed transactions
/// are applied, in order, each whole: every frame of a transaction is
/// checked before the first of its records reaches `apply`, and a torn or
/// damaged frame ends replay at the commit before it.
///
/// The result says where replay ended (`end`, from which a log written
/// afterwards carries on), the transactions and records applied, and the
/// bytes of the stream after `end` that were not. Nothing is copied but a
/// frame that straddles two segments.
pub fn replay_shipped<E>(
    from: u64,
    segments: &[(u64, &[u8])],
    apply: &mut dyn FnMut(Replay<'_>) -> Result<(), E>,
) -> Result<Recovery, E> {
    let log = Shipped::new(from, segments);
    let mut rec = Recovery {
        end: from,
        ..Default::default()
    };
    loop {
        // The transaction starting at `end`: every frame checked, up to its
        // commit marker.
        let mut at = rec.end;
        let mut records = 0u64;
        let commit = loop {
            match log.frame(at, true) {
                None => break None,
                Some((K_TX_END, _, next)) => break Some(next),
                Some((kind, _, next)) => {
                    records += (kind == K_DATA) as u64;
                    at = next;
                }
            }
        };
        let Some(next) = commit else { break };
        // Whole and intact: apply it.
        let mut at = rec.end;
        while let Some((kind, payload, after)) = log.frame(at, false) {
            if kind == K_TX_END {
                break;
            }
            if kind == K_DATA {
                apply(Replay::Record(&payload))?;
            }
            at = after;
        }
        apply(Replay::Commit)?;
        rec.transactions += 1;
        rec.records += records;
        rec.end = next;
    }
    rec.discarded = log.end - rec.end;
    Ok(rec)
}

/// Reads records sequentially across segment files.
struct SegReader {
    dir: PathBuf,
    segs: Vec<u64>,
    idx: usize,
    file: Option<io::BufReader<fs::File>>,
    /// LSN of the next byte.
    lsn: u64,
}

impl SegReader {
    fn new(dir: &Path, segs: Vec<u64>) -> SegReader {
        SegReader {
            dir: dir.to_path_buf(),
            segs,
            idx: 0,
            file: None,
            lsn: 0,
        }
    }

    fn seek(&mut self, lsn: u64) -> io::Result<()> {
        self.lsn = lsn;
        self.idx = self.segs.iter().rposition(|s| *s <= lsn).unwrap_or(0);
        self.open_current(lsn)
    }

    fn open_current(&mut self, lsn: u64) -> io::Result<()> {
        self.file = None;
        let Some(&s) = self.segs.get(self.idx) else {
            return Ok(());
        };
        let mut f = fs::File::open(self.dir.join(seg_name(s)))?;
        f.seek(SeekFrom::Start(lsn.saturating_sub(s)))?;
        self.file = Some(io::BufReader::with_capacity(1 << 20, f));
        Ok(())
    }

    /// The next CRC-valid record, or None at the end or at a torn/corrupt
    /// frame.
    fn next_record(&mut self) -> io::Result<Option<(u8, Vec<u8>)>> {
        loop {
            let Some(f) = self.file.as_mut() else {
                return Ok(None);
            };
            let mut head = [0u8; 5];
            match read_full(f, &mut head)? {
                0 => {
                    // End of this segment: continue in the next one if it
                    // starts exactly here.
                    let next = self.idx + 1;
                    if self.segs.get(next).copied() == Some(self.lsn) {
                        self.idx = next;
                        self.open_current(self.lsn)?;
                        continue;
                    }
                    return Ok(None);
                }
                5 => {}
                _ => return Ok(None),
            }
            let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
            if len > 1 << 30 {
                return Ok(None);
            }
            let mut body = vec![0u8; len + 4];
            if read_full(f, &mut body)? != len + 4 {
                return Ok(None);
            }
            let mut c = crate::codec::Crc32::new();
            c.update(&head);
            c.update(&body[..len]);
            let stored =
                u32::from_le_bytes([body[len], body[len + 1], body[len + 2], body[len + 3]]);
            if c.finish() != stored {
                return Ok(None);
            }
            self.lsn += 9 + len as u64;
            body.truncate(len);
            return Ok(Some((head[0], body)));
        }
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("glider-wal-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(wal_dir(&p));
        p
    }

    fn replay(db: &Path, from: u64, seg: u64, buf: usize) -> (Vec<Vec<u8>>, Recovery, Wal) {
        let mut got = Vec::new();
        let mut txn = Vec::new();
        let (w, r) = Wal::open(db, from, SyncMode::Normal, seg, buf, &mut |e| {
            match e {
                Replay::Record(p) => txn.push(p.to_vec()),
                Replay::Commit => got.append(&mut txn),
            }
            Ok(())
        })
        .unwrap();
        (got, r, w)
    }

    #[test]
    fn committed_transactions_replay_and_torn_tails_vanish() {
        let db = tmp("replay");
        {
            let (_, _, mut w) = replay(&db, 0, 256, 1 << 20);
            for t in 0..20u8 {
                for i in 0..5u8 {
                    w.append(&[t, i, 42]).unwrap();
                }
                w.commit().unwrap();
            }
            // An uncommitted tail.
            w.append(&[99, 99]).unwrap();
            w.flush().unwrap();
        }
        // 256-byte segments: many files.
        assert!(list_segments(&wal_dir(&db)).unwrap().len() > 3);
        // Replay with a buffer too small for one transaction: the spill
        // path re-reads.
        for buf in [1 << 20, 4] {
            let (got, rec, _) = replay(&db, 0, 256, buf);
            assert_eq!(rec.transactions, 20, "buf {buf}");
            assert_eq!(got.len(), 100);
            assert_eq!(got[0], vec![0, 0, 42]);
            assert_eq!(got[99], vec![19, 4, 42]);
        }
        // The uncommitted record was cut off by the first replay.
        let (_, rec, mut w) = replay(&db, 0, 256, 1 << 20);
        assert_eq!(rec.discarded, 0);
        // Appending continues cleanly after recovery, and replay from a
        // checkpoint LSN skips what it covers.
        let mid = w.committed_lsn();
        w.append(&[7]).unwrap();
        w.commit().unwrap();
        w.truncate_before(mid).unwrap();
        drop(w);
        let (got, rec, _) = replay(&db, mid, 256, 1 << 20);
        assert_eq!(rec.transactions, 1);
        assert_eq!(got, vec![vec![7u8]]);
        let _ = fs::remove_dir_all(wal_dir(&db));
    }

    #[test]
    fn rollback_cuts_the_log_back_to_the_last_commit() {
        let db = tmp("rollback");
        let (_, _, mut w) = replay(&db, 0, 64, 1 << 20);
        w.append(&[1]).unwrap();
        w.commit().unwrap();
        for _ in 0..40 {
            w.append(&[2; 10]).unwrap();
        }
        w.rollback().unwrap();
        w.append(&[3]).unwrap();
        w.commit().unwrap();
        drop(w);
        let (got, rec, _) = replay(&db, 0, 64, 1 << 20);
        assert_eq!(got, vec![vec![1u8], vec![3u8]]);
        assert_eq!(rec.transactions, 2);
        let _ = fs::remove_dir_all(wal_dir(&db));
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    fn mem_range(m: &MemLog, from: u64, to: u64) -> Vec<u8> {
        let mut v = Vec::new();
        m.copy_range(from, to, &mut v).unwrap();
        v
    }

    fn file_range(db: &Path, from: u64, to: u64) -> Vec<u8> {
        let mut v = Vec::new();
        copy_range(db, from, to, &mut v).unwrap();
        v
    }

    #[test]
    fn a_memory_log_writes_the_bytes_a_file_log_writes_at_the_same_positions() {
        for seed in 1..=3u64 {
            let db = tmp(&format!("memlog-{seed}"));
            // A log that does not start at zero, as after a checkpoint.
            let (_, _, mut w) = replay(&db, 0, 64 << 10, 1 << 20);
            w.append(b"before").unwrap();
            w.commit().unwrap();
            let start = w.committed_lsn();
            let mut m = MemLog::new(start, u64::MAX);
            let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut commits = vec![start];
            for step in 0..300u64 {
                // Mostly small transactions; some of thousands of records;
                // now and then a record larger than a chunk.
                let records = match r.below(20) {
                    0 => 2000 + r.below(3000),
                    1 => 0,
                    _ => 1 + r.below(12),
                };
                for i in 0..records {
                    let len = match r.below(4000) {
                        0 => CHUNK + r.below(3 * CHUNK as u64) as usize,
                        k if k < 40 => r.below(4000) as usize,
                        _ => r.below(60) as usize,
                    };
                    let payload: Vec<u8> = (0..len).map(|j| (j as u64 ^ i ^ step) as u8).collect();
                    w.append(&payload).unwrap();
                    m.append(&payload);
                }
                if r.below(4) == 0 {
                    w.rollback().unwrap();
                    m.rollback();
                } else {
                    w.commit().unwrap();
                    m.commit();
                }
                assert_eq!(
                    m.committed_lsn(),
                    w.committed_lsn(),
                    "seed {seed} step {step}"
                );
                if commits.last() != Some(&m.committed_lsn()) {
                    commits.push(m.committed_lsn());
                }
            }
            w.flush().unwrap();
            let end = m.committed_lsn();
            assert!(end - start > 1 << 20, "a log of some size: {}", end - start);
            assert_eq!(file_range(&db, start, end), mem_range(&m, start, end));
            for _ in 0..40 {
                let a = commits[r.below(commits.len() as u64) as usize];
                let b = commits[r.below(commits.len() as u64) as usize];
                let (a, b) = (a.min(b), a.max(b));
                let bytes = mem_range(&m, a, b);
                assert_eq!(file_range(&db, a, b), bytes, "seed {seed} {a}..{b}");
                assert!(is_whole_transactions(&bytes));
                assert_eq!(committed_end(&db, a).unwrap(), m.committed_end(a));
            }
            let _ = fs::remove_dir_all(wal_dir(&db));
        }
    }

    #[test]
    fn a_memory_log_answers_only_at_commits() {
        let mut m = MemLog::new(100, u64::MAX);
        let mut commits = vec![100];
        for t in 0..20u8 {
            m.append(&[t; 100]);
            m.append(&[t; 3]);
            m.commit();
            commits.push(m.committed_lsn());
        }
        let end = m.committed_lsn();
        for &c in &commits {
            assert_eq!(m.committed_end(c), Committed::To(end));
        }
        // Below the log, inside a transaction, past the last commit: gone.
        for at in [0, 99, 101, commits[3] + 9, end + 1, end + 1000] {
            assert_eq!(m.committed_end(at), Committed::Gone, "at {at}");
        }
        // A range must start and end at commits.
        let mut sink = Vec::new();
        assert!(m.copy_range(commits[2], commits[5], &mut sink).is_ok());
        assert!(m.copy_range(commits[2] + 1, commits[5], &mut sink).is_err());
        assert!(m.copy_range(commits[2], commits[5] - 1, &mut sink).is_err());
        assert!(m.copy_range(commits[5], commits[2], &mut sink).is_err());
        assert!(m.copy_range(commits[2], end + 9, &mut sink).is_err());
        // An open transaction is not committed log.
        m.append(&[7; 10]);
        assert_eq!(m.committed_end(end), Committed::To(end));
        assert!(m.copy_range(end, end + 19, &mut sink).is_err());
        m.rollback();
        assert_eq!(m.committed_lsn(), end);
    }

    #[test]
    fn a_memory_log_lets_go_of_what_is_shipped_and_drops_what_outgrows_its_cap() {
        // Each transaction is a 100-byte record (109) and a commit (9).
        let mut m = MemLog::new(0, 1000);
        let mut commits = vec![0u64];
        for t in 0..5u8 {
            m.append(&[t; 100]);
            m.commit();
            commits.push(m.committed_lsn());
        }
        // Let go of a position inside a transaction: only up to the commit
        // before it.
        m.release(commits[2] + 50);
        assert_eq!(m.start(), commits[2]);
        assert_eq!(m.committed_end(commits[1]), Committed::Gone);
        assert_eq!(m.committed_end(commits[2]), Committed::To(commits[5]));
        let mut sink = Vec::new();
        let e = m.copy_range(commits[1], commits[5], &mut sink).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        // 3 transactions are held (354 bytes); 6 more is past the cap of
        // 1000, and the commit that does it drops everything.
        for t in 0..6u8 {
            m.append(&[t; 100]);
            m.commit();
            let held = m.committed_lsn() - m.start();
            assert!(held <= 1000, "{held} bytes of committed log kept");
        }
        let end = m.committed_lsn();
        assert_eq!(m.start(), end, "all of it dropped");
        assert_eq!(m.committed_end(commits[2]), Committed::Gone);
        assert_eq!(m.committed_end(end), Committed::To(end));
        assert_eq!(m.memory_bytes(), 0);
        // And it carries on from there.
        m.append(&[9; 10]);
        m.commit();
        assert_eq!(mem_range(&m, end, m.committed_lsn()).len(), 28);
    }

    #[test]
    fn a_memory_log_never_drops_the_open_transaction() {
        let mut m = MemLog::new(0, 1000);
        m.append(&[1; 50]);
        m.commit();
        let c = m.committed_lsn();
        // Ten times the cap, uncommitted: all kept, and nothing committed
        // is dropped for it.
        for i in 0..100u8 {
            m.append(&[i; 100]);
        }
        assert_eq!(m.committed_end(0), Committed::To(c));
        // Shipping everything committed lets go of the committed log only.
        m.release(u64::MAX);
        assert_eq!(m.start(), c);
        assert!(m.memory_bytes() >= 100 * 109);
        // Rolled back, it is gone; committed, it is committed log.
        m.rollback();
        assert_eq!(m.committed_lsn(), c);
        m.append(&[2; 100]);
        m.commit();
        let one = mem_range(&m, c, m.committed_lsn());
        assert_eq!(one.len(), 118);
        assert!(is_whole_transactions(&one));
    }

    #[test]
    fn a_memory_log_that_is_shipped_regularly_stays_flat() {
        let mut m = MemLog::new(0, 64 << 20);
        let mut written = 0u64;
        for i in 0..40_000u64 {
            m.append(&[i as u8; 200]);
            m.append(&[(i >> 8) as u8; 31]);
            m.commit();
            written = m.committed_lsn();
            if i % 50 == 49 {
                let shipped = mem_range(&m, m.start(), written);
                assert!(is_whole_transactions(&shipped));
                m.release(written);
            }
            assert!(
                m.memory_bytes() <= 2 * CHUNK as u64,
                "{} bytes held after {written} written",
                m.memory_bytes()
            );
        }
        assert!(written > 100 * CHUNK as u64);
    }

    /// Records replay delivers, per committed transaction.
    fn shipped(from: u64, segs: &[(u64, &[u8])]) -> (Vec<Vec<u8>>, Recovery) {
        let mut got = Vec::new();
        let mut txn = Vec::new();
        let rec = replay_shipped(from, segs, &mut |e| -> Result<(), ()> {
            match e {
                Replay::Record(p) => txn.push(p.to_vec()),
                Replay::Commit => got.append(&mut txn),
            }
            Ok(())
        })
        .unwrap();
        assert!(txn.is_empty(), "a partial transaction reached apply");
        (got, rec)
    }

    /// `bytes` (log from `at`) cut into pieces of `size`.
    fn cut(at: u64, bytes: &[u8], size: usize) -> Vec<(u64, &[u8])> {
        bytes
            .chunks(size)
            .enumerate()
            .map(|(i, c)| (at + (i * size) as u64, c))
            .collect()
    }

    #[test]
    fn shipped_log_replays_as_the_file_log_does() {
        let db = tmp("shipped");
        let mut commits = vec![0u64];
        {
            let (_, _, mut w) = replay(&db, 0, 1 << 20, 1 << 20);
            for t in 0..40u8 {
                for i in 0..1 + t % 5 {
                    w.append(&vec![t; 1 + (t as usize * 37 + i as usize) % 300])
                        .unwrap();
                }
                if t % 7 == 3 {
                    w.rollback().unwrap();
                } else {
                    w.commit().unwrap();
                    commits.push(w.committed_lsn());
                }
            }
            // An uncommitted tail, as a crash leaves it.
            w.append(&[99; 20]).unwrap();
            w.flush().unwrap();
        }
        let bytes = fs::read(wal_dir(&db).join(seg_name(0))).unwrap();
        let end = *commits.last().unwrap();
        assert_eq!(bytes.len() as u64, end + 29);
        // What the file's own replay applies.
        let (want, wrec, _) = replay(&db, 0, 1 << 20, 1 << 20);
        assert_eq!(wrec.end, end);
        // The same bytes, shipped whole or in pieces small enough that
        // frames straddle them.
        for size in [bytes.len(), 1000, 64, 7, 1] {
            let (got, rec) = shipped(0, &cut(0, &bytes, size));
            assert_eq!(got, want, "pieces of {size}");
            assert_eq!(rec.end, end);
            assert_eq!(rec.transactions, wrec.transactions);
            assert_eq!(rec.discarded, 29);
        }
        // From a later commit, with segments that overlap and arrive out of
        // order: as from the file.
        let mid = commits[10];
        let segs = vec![
            (commits[20], &bytes[commits[20] as usize..]),
            (0, &bytes[..commits[15] as usize]),
            (
                commits[12],
                &bytes[commits[12] as usize..commits[25] as usize],
            ),
        ];
        let (got, rec) = shipped(mid, &segs);
        let (want, _, _) = replay(&db, 0, 1 << 20, 1 << 20);
        let (before, _) = shipped(0, &cut(0, &bytes[..mid as usize], 100));
        assert_eq!(got, want[before.len()..].to_vec());
        assert_eq!(rec.end, end);
        // A gap ends replay at the commit before it.
        let gap = vec![
            (0, &bytes[..commits[8] as usize]),
            (commits[9], &bytes[commits[9] as usize..]),
        ];
        let (_, rec) = shipped(0, &gap);
        assert_eq!(rec.end, commits[8]);
        assert_eq!(rec.discarded, 0);
        // A damaged byte ends replay at the commit before its transaction,
        // and nothing of that transaction is applied.
        for at in [commits[5] + 3, commits[6] - 1, commits[5] + 7] {
            let mut bad = bytes.clone();
            bad[at as usize] ^= 0x40;
            let (got, rec) = shipped(0, &cut(0, &bad, 333));
            assert_eq!(rec.end, commits[5], "damage at {at}");
            let (before, _) = shipped(0, &[(0, &bytes[..commits[5] as usize])]);
            assert_eq!(got, before);
            assert_eq!(rec.discarded, bytes.len() as u64 - commits[5]);
        }
        // Nothing shipped past the base: nothing replayed.
        let (got, rec) = shipped(end, &cut(0, &bytes[..end as usize], 500));
        assert!(got.is_empty());
        assert_eq!((rec.end, rec.transactions), (end, 0));
        let _ = fs::remove_dir_all(wal_dir(&db));
    }
}
