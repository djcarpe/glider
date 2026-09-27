//! The write-ahead log of a file-backed database.
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

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::codec::crc32;

pub const K_DATA: u8 = 1;
pub const K_TX_END: u8 = 255;
pub const DEFAULT_SEGMENT: u64 = 64 << 20;

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
                    fs::OpenOptions::new().write(true).open(&path)?.set_len(keep)?;
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
            .filter(|s| *s <= self.committed)
            .next_back()
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
        let len = u32::from_le_bytes([bytes[at + 1], bytes[at + 2], bytes[at + 3], bytes[at + 4]]) as usize;
        let end = at + 5 + len + 4;
        if end > bytes.len() {
            return false;
        }
        let stored = u32::from_le_bytes([bytes[end - 4], bytes[end - 3], bytes[end - 2], bytes[end - 1]]);
        if crc32(&bytes[at..at + 5 + len]) != stored {
            return false;
        }
        last = bytes[at];
        at = end;
    }
    bytes.is_empty() || last == K_TX_END
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
            let stored = u32::from_le_bytes([body[len], body[len + 1], body[len + 2], body[len + 3]]);
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
        let (w, r) = Wal::open(
            db,
            from,
            SyncMode::Normal,
            seg,
            buf,
            &mut |e| {
                match e {
                    Replay::Record(p) => txn.push(p.to_vec()),
                    Replay::Commit => got.append(&mut txn),
                }
                Ok(())
            },
        )
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
}
