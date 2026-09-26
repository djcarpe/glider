//! Durability. One file: an optional snapshot image, then an append-only run
//! of CRC-checked records grouped into transactions by an explicit commit
//! marker.
//!
//! ```text
//! [header 64 B][image (see image.rs)][record][record]...[TX_END]...
//! ```
//!
//! The image is current state as of the last compaction, laid out to be read
//! in bulk. The records after it are the write-ahead log of everything since.
//! Only compaction writes an image, and it does so by writing a whole new file
//! and renaming it into place, so between compactions the file is strictly
//! append-only — which is what replication depends on.
//!
//! Crash behaviour: a torn tail (a half-written record, or ops with no commit
//! marker) is discarded on open and the file is truncated back to the last
//! committed byte offset. You never see a partial transaction.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::codec::{self, crc32, Reader};
use crate::value::Value;

pub const MAGIC: &[u8; 8] = b"GLIDER\x00\x01";
/// The magic this format shipped with before the rename. Still accepted on
/// open; a COMPACT rewrites the header with the current magic.
pub const MAGIC_LEGACY: &[u8; 8] = b"GRAPHLT\x01";
/// v2 added the generation id; v3 the snapshot image. v1 and v2 files still
/// open, by replaying their whole log; a COMPACT upgrades them.
pub const FORMAT_VERSION: u32 = 3;
/// v3: magic(8) version(4) flags(4) generation(16) image_len(8) log_start(8)
/// reserved(12) crc(4)
pub const HEADER_LEN: u64 = 64;
pub const HEADER_V2_LEN: u64 = 32;
pub const HEADER_V1_LEN: u64 = 16;
/// Header flag: an image follows the header.
const FLAG_IMAGE: u32 = 1;

const K_NODE_ADD: u8 = 0;
const K_NODE_DEL: u8 = 1;
const K_EDGE_ADD: u8 = 2;
const K_EDGE_DEL: u8 = 3;
const K_NODE_SET: u8 = 4;
const K_NODE_UNSET: u8 = 5;
const K_EDGE_SET: u8 = 6;
const K_EDGE_UNSET: u8 = 7;
const K_LABEL_ADD: u8 = 8;
const K_LABEL_DEL: u8 = 9;
const K_INDEX_ADD: u8 = 10;
const K_INDEX_DEL: u8 = 11;
const K_COUNTERS: u8 = 12;
const K_CLEAR: u8 = 13;
const K_TX_END: u8 = 255;

/// A single mutation. The log is a sequence of these; replaying them in order
/// reconstructs the graph exactly.
#[derive(Clone, Debug)]
pub enum Op {
    NodeAdd {
        id: u64,
        labels: Vec<String>,
        props: Vec<(String, Value)>,
    },
    NodeDel {
        id: u64,
    },
    EdgeAdd {
        id: u64,
        from: u64,
        to: u64,
        etype: String,
        props: Vec<(String, Value)>,
    },
    EdgeDel {
        id: u64,
    },
    NodeSet {
        id: u64,
        key: String,
        value: Value,
    },
    NodeUnset {
        id: u64,
        key: String,
    },
    EdgeSet {
        id: u64,
        key: String,
        value: Value,
    },
    EdgeUnset {
        id: u64,
        key: String,
    },
    LabelAdd {
        id: u64,
        label: String,
    },
    LabelDel {
        id: u64,
        label: String,
    },
    IndexAdd {
        label: String,
        key: String,
    },
    IndexDel {
        label: String,
        key: String,
    },
    Counters {
        next_node: u64,
        next_edge: u64,
    },
    Clear,
}

impl Op {
    fn kind(&self) -> u8 {
        match self {
            Op::NodeAdd { .. } => K_NODE_ADD,
            Op::NodeDel { .. } => K_NODE_DEL,
            Op::EdgeAdd { .. } => K_EDGE_ADD,
            Op::EdgeDel { .. } => K_EDGE_DEL,
            Op::NodeSet { .. } => K_NODE_SET,
            Op::NodeUnset { .. } => K_NODE_UNSET,
            Op::EdgeSet { .. } => K_EDGE_SET,
            Op::EdgeUnset { .. } => K_EDGE_UNSET,
            Op::LabelAdd { .. } => K_LABEL_ADD,
            Op::LabelDel { .. } => K_LABEL_DEL,
            Op::IndexAdd { .. } => K_INDEX_ADD,
            Op::IndexDel { .. } => K_INDEX_DEL,
            Op::Counters { .. } => K_COUNTERS,
            Op::Clear => K_CLEAR,
        }
    }

    fn encode_payload(&self, out: &mut Vec<u8>) {
        use codec::*;
        match self {
            Op::NodeAdd { id, labels, props } => {
                put_varint(out, *id);
                put_varint(out, labels.len() as u64);
                for l in labels {
                    put_str(out, l);
                }
                put_props(out, props);
            }
            Op::NodeDel { id } | Op::EdgeDel { id } => put_varint(out, *id),
            Op::EdgeAdd {
                id,
                from,
                to,
                etype,
                props,
            } => {
                put_varint(out, *id);
                put_varint(out, *from);
                put_varint(out, *to);
                put_str(out, etype);
                put_props(out, props);
            }
            Op::NodeSet { id, key, value } | Op::EdgeSet { id, key, value } => {
                put_varint(out, *id);
                put_str(out, key);
                put_value(out, value);
            }
            Op::NodeUnset { id, key } | Op::EdgeUnset { id, key } => {
                put_varint(out, *id);
                put_str(out, key);
            }
            Op::LabelAdd { id, label } | Op::LabelDel { id, label } => {
                put_varint(out, *id);
                put_str(out, label);
            }
            Op::IndexAdd { label, key } | Op::IndexDel { label, key } => {
                put_str(out, label);
                put_str(out, key);
            }
            Op::Counters {
                next_node,
                next_edge,
            } => {
                put_varint(out, *next_node);
                put_varint(out, *next_edge);
            }
            Op::Clear => {}
        }
    }

    fn decode(kind: u8, payload: &[u8]) -> Result<Op, String> {
        let mut r = Reader::new(payload);
        let op = match kind {
            K_NODE_ADD => {
                let id = r.varint()?;
                let n = r.varint()? as usize;
                let mut labels = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    labels.push(r.string()?);
                }
                Op::NodeAdd {
                    id,
                    labels,
                    props: read_props(&mut r)?,
                }
            }
            K_NODE_DEL => Op::NodeDel { id: r.varint()? },
            K_EDGE_ADD => Op::EdgeAdd {
                id: r.varint()?,
                from: r.varint()?,
                to: r.varint()?,
                etype: r.string()?,
                props: {
                    let p = read_props(&mut r)?;
                    p
                },
            },
            K_EDGE_DEL => Op::EdgeDel { id: r.varint()? },
            K_NODE_SET => Op::NodeSet {
                id: r.varint()?,
                key: r.string()?,
                value: r.value()?,
            },
            K_NODE_UNSET => Op::NodeUnset {
                id: r.varint()?,
                key: r.string()?,
            },
            K_EDGE_SET => Op::EdgeSet {
                id: r.varint()?,
                key: r.string()?,
                value: r.value()?,
            },
            K_EDGE_UNSET => Op::EdgeUnset {
                id: r.varint()?,
                key: r.string()?,
            },
            K_LABEL_ADD => Op::LabelAdd {
                id: r.varint()?,
                label: r.string()?,
            },
            K_LABEL_DEL => Op::LabelDel {
                id: r.varint()?,
                label: r.string()?,
            },
            K_INDEX_ADD => Op::IndexAdd {
                label: r.string()?,
                key: r.string()?,
            },
            K_INDEX_DEL => Op::IndexDel {
                label: r.string()?,
                key: r.string()?,
            },
            K_COUNTERS => Op::Counters {
                next_node: r.varint()?,
                next_edge: r.varint()?,
            },
            K_CLEAR => Op::Clear,
            other => return Err(format!("unknown record kind {}", other)),
        };
        Ok(op)
    }
}

impl Op {
    /// Check that a payload decodes, without allocating anything. Accepts
    /// exactly what `decode` accepts, so a transaction whose records all
    /// validate can be decoded and applied one record at a time with no
    /// chance of failing halfway.
    fn validate(kind: u8, payload: &[u8]) -> Result<(), String> {
        let mut r = Reader::new(payload);
        let props = |r: &mut Reader| -> Result<(), String> {
            let n = r.varint()? as usize;
            if n > r.remaining() + 1 {
                return Err("property count exceeds record".into());
            }
            for _ in 0..n {
                r.skip_str()?;
                r.skip_value()?;
            }
            Ok(())
        };
        match kind {
            K_NODE_ADD => {
                r.varint()?;
                let n = r.varint()?;
                for _ in 0..n {
                    r.skip_str()?;
                }
                props(&mut r)
            }
            K_NODE_DEL | K_EDGE_DEL => r.varint().map(|_| ()),
            K_EDGE_ADD => {
                r.varint()?;
                r.varint()?;
                r.varint()?;
                r.skip_str()?;
                props(&mut r)
            }
            K_NODE_SET | K_EDGE_SET => {
                r.varint()?;
                r.skip_str()?;
                r.skip_value()
            }
            K_NODE_UNSET | K_EDGE_UNSET | K_LABEL_ADD | K_LABEL_DEL => {
                r.varint()?;
                r.skip_str()
            }
            K_INDEX_ADD | K_INDEX_DEL => {
                r.skip_str()?;
                r.skip_str()
            }
            K_COUNTERS => {
                r.varint()?;
                r.varint().map(|_| ())
            }
            K_CLEAR => Ok(()),
            other => Err(format!("unknown record kind {}", other)),
        }
    }
}

fn put_props(out: &mut Vec<u8>, props: &[(String, Value)]) {
    codec::put_varint(out, props.len() as u64);
    for (k, v) in props {
        codec::put_str(out, k);
        codec::put_value(out, v);
    }
}

fn read_props(r: &mut Reader) -> Result<Vec<(String, Value)>, String> {
    let n = r.varint()? as usize;
    if n > r.remaining() + 1 {
        return Err("property count exceeds record".into());
    }
    let mut props = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        props.push((r.string()?, r.value()?));
    }
    Ok(props)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sync {
    /// fsync on every commit. Survives OS crash and power loss.
    Always,
    /// Hand bytes to the OS on commit, don't wait for the platter.
    /// Survives process crash, not power loss. This is the default.
    Normal,
    /// Buffer aggressively. Fastest, for bulk load.
    Off,
}

pub struct Store {
    path: PathBuf,
    file: BufWriter<File>,
    /// Byte offset of the end of the last durable commit.
    committed_len: u64,
    pending: Vec<u8>,
    pending_ops: u64,
    pub sync: Sync,
    pub records_written: u64,
    generation: [u8; 16],
    header_len: u64,
    /// Dropped last, after the file — releases the on-disk lock.
    _lock: Option<Lock>,
}

/// A database file that is locked and has had its header read, but whose log
/// has not been replayed yet. The caller loads the image (if any) between the
/// two steps.
pub struct Opening {
    path: PathBuf,
    file: File,
    header: FileHeader,
    sync: Sync,
    lock: Lock,
}

impl Opening {
    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    /// Replay every committed op after the image into `apply`, discard any
    /// torn tail, and hand back a store ready to append.
    pub fn replay<F: FnMut(Op)>(self, apply: &mut F) -> io::Result<Store> {
        let Opening {
            path,
            mut file,
            header,
            sync,
            lock,
        } = self;
        let header_len = header.header_len;
        let mut generation = header.generation;
        let committed_len = replay(&mut file, header_len, apply)?;

        // Discard any torn tail so the next append starts from a clean boundary.
        //
        // This also ends the current generation. Bytes above `committed_len`
        // may already have been replicated — under Sync::Normal the OS had
        // them even though we did not survive to commit them — and the next
        // write will put *different* bytes at those same offsets. A replica
        // that kept streaming into the same lineage would later restore a
        // file that is CRC-valid and wrong at the seam. A new generation makes
        // the discontinuity explicit, so replicas start a fresh lineage
        // instead of splicing two histories together.
        //
        // `committed_len` is never below `header_len`, so the image is never
        // cut into.
        if committed_len < file.metadata()?.len() {
            file.set_len(committed_len)?;
            match header.version {
                2 => {
                    generation = new_generation();
                    file.seek(SeekFrom::Start(16))?;
                    file.write_all(&generation)?;
                    file.sync_all()?;
                }
                3 => {
                    generation = new_generation();
                    file.seek(SeekFrom::Start(0))?;
                    file.write_all(&encode_header(&generation, header.image_len))?;
                    file.sync_all()?;
                }
                _ => {}
            }
        }
        file.seek(SeekFrom::Start(committed_len))?;

        Ok(Store {
            path,
            file: BufWriter::with_capacity(1 << 16, file),
            committed_len,
            pending: Vec::with_capacity(1 << 14),
            pending_ops: 0,
            sync,
            records_written: 0,
            generation,
            header_len,
            _lock: Some(lock),
        })
    }
}

impl Store {
    /// Open (creating if needed) and replay every committed op into `apply`.
    /// Refuses a file that carries a snapshot image, since the ops after an
    /// image are not the whole state; open those through `Graph`.
    pub fn open<F: FnMut(Op)>(path: &Path, sync: Sync, apply: F) -> io::Result<Store> {
        let mut apply = apply;
        let opening = Store::begin_open(path, sync, false)?;
        if opening.header().image_len > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file has a snapshot image; open it with Graph::open",
            ));
        }
        opening.replay(&mut apply)
    }

    /// Lock the file (creating it if needed) and read its header. `force`
    /// breaks a lock held by a process we cannot prove is gone. Use it when
    /// you know the previous writer is dead and the platform will not tell us
    /// so.
    pub fn begin_open(path: &Path, sync: Sync, force: bool) -> io::Result<Opening> {
        let lock = Lock::acquire(path, force)?;
        // A compaction that died before its rename leaves its temp file
        // behind. We hold the lock, so nobody else is writing it.
        let _ = std::fs::remove_file(compact_tmp_path(path));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        let len = file.metadata()?.len();
        let header = if len == 0 {
            let generation = new_generation();
            let bytes = encode_header(&generation, 0);
            file.write_all(&bytes)?;
            file.sync_all()?;
            // A new file is not durable until its directory entry is.
            sync_parent(path);
            parse_header(&bytes)?
        } else {
            read_header_from(&mut file)?
        };
        if header.header_len > file.metadata()?.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file is shorter than its header and snapshot image",
            ));
        }
        Ok(Opening {
            path: path.to_path_buf(),
            file,
            header,
            sync,
            lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_len(&self) -> u64 {
        self.committed_len + self.pending.len() as u64
    }

    pub fn push(&mut self, op: &Op) {
        encode_record(&mut self.pending, op.kind(), |buf| op.encode_payload(buf));
        self.pending_ops += 1;
    }

    pub fn pending_ops(&self) -> u64 {
        self.pending_ops
    }

    pub fn rollback(&mut self) {
        self.pending.clear();
        self.pending_ops = 0;
    }

    /// Make every pushed op durable as one atomic unit.
    pub fn commit(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        encode_record(&mut self.pending, K_TX_END, |_| {});
        let n = self.pending.len() as u64;
        self.file.write_all(&self.pending)?;
        match self.sync {
            Sync::Always => {
                self.file.flush()?;
                self.file.get_ref().sync_data()?;
            }
            Sync::Normal => self.file.flush()?,
            Sync::Off => {}
        }
        self.committed_len += n;
        self.records_written += self.pending_ops + 1;
        self.pending.clear();
        self.pending_ops = 0;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.get_ref().sync_data()
    }

    /// Rewrite the file as a snapshot image followed by an empty log.
    ///
    /// `write` puts the image bytes into a sibling temp file, positioned just
    /// past the header; the file is then fsynced and handed to `load`, which
    /// reads the image back — a check that what was written is what will be
    /// opened, and the caller's way to swap the new image in. Only then is
    /// the temp file renamed over the original. The rename is atomic, so a
    /// crash at any point leaves either the old database or the new one.
    pub fn compact_image<T>(
        &mut self,
        write: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>,
        load: impl FnOnce(&Path, &FileHeader) -> io::Result<T>,
    ) -> io::Result<T> {
        self.file.flush()?;
        let tmp = compact_tmp_path(&self.path);
        // A compaction rewrites every byte, so offsets from the old file mean
        // nothing now. That is exactly what a generation change is for:
        // replicas see it and start a fresh lineage instead of appending
        // segments onto a log that no longer exists.
        let generation = new_generation();
        let written = (|| -> io::Result<FileHeader> {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            let mut w = BufWriter::with_capacity(1 << 20, f);
            w.write_all(&[0u8; HEADER_LEN as usize])?;
            write(&mut w)?;
            let end = w.stream_position()?;
            let header = encode_header(&generation, end - HEADER_LEN);
            w.seek(SeekFrom::Start(0))?;
            w.write_all(&header)?;
            w.flush()?;
            w.get_ref().set_len(end)?;
            w.get_ref().sync_all()?;
            parse_header(&header)
        })();
        let loaded = written.and_then(|h| load(&tmp, &h).map(|t| (h, t)));
        let (header, out) = match loaded {
            Ok(x) => x,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };

        // On Unix this always succeeds. On Windows a virus scanner or the
        // search indexer can hold a transient handle on the destination and
        // make MoveFileEx fail with ACCESS_DENIED; it clears in milliseconds,
        // so retry rather than lose the compaction.
        let mut attempt = 0;
        loop {
            match std::fs::rename(&tmp, &self.path) {
                Ok(()) => break,
                Err(_) if attempt < 10 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20 * attempt));
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            }
        }
        // POSIX: the rename is not durable until the directory is synced. A
        // crash in this window can leave the old file, or the new one.
        sync_parent(&self.path);

        let mut file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        file.seek(SeekFrom::Start(header.header_len))?;
        self.file = BufWriter::with_capacity(1 << 16, file);
        self.committed_len = header.header_len;
        self.header_len = header.header_len;
        self.generation = generation;
        self.pending.clear();
        self.pending_ops = 0;
        self.records_written = 0;
        Ok(out)
    }
}

/// Where a compaction writes before renaming into place.
pub fn compact_tmp_path(db: &Path) -> PathBuf {
    let mut name = db.file_name().unwrap_or_default().to_os_string();
    name.push(".compact.tmp");
    db.with_file_name(name)
}

/// Writes a brand-new database file straight from a stream of ops, without
/// building a graph in memory first.
///
/// `Graph::open` and `import` both hold the whole graph, so the largest file
/// they can produce is bounded by RAM. A generator that streams through this
/// writer is not: memory stays flat at one buffer however big the file gets.
/// That is what makes files larger than the machine possible — useful for
/// stress tests, and for converting data that is bigger than any one host.
///
/// Nothing checks the ops. They are replayed verbatim on open, so they must
/// make sense in order: an edge's endpoints must already exist, ids must not
/// be reused. Ops between two `commit`s form one transaction; a file that is
/// cut short loses only its unfinished tail, exactly like a crashed writer.
pub struct LogWriter {
    file: BufWriter<File>,
    buf: Vec<u8>,
    bytes: u64,
    ops: u64,
    /// Ops pushed since the last commit, whether or not already flushed.
    open_tx: bool,
}

impl LogWriter {
    /// Create `path`, which must not already exist, and write the header.
    pub fn create(path: &Path) -> io::Result<LogWriter> {
        let f = OpenOptions::new().write(true).create_new(true).open(path)?;
        let mut file = BufWriter::with_capacity(1 << 20, f);
        let header = encode_header(&new_generation(), 0);
        file.write_all(&header)?;
        Ok(LogWriter {
            file,
            buf: Vec::with_capacity(1 << 20),
            bytes: header.len() as u64,
            ops: 0,
            open_tx: false,
        })
    }

    pub fn push(&mut self, op: &Op) -> io::Result<()> {
        let before = self.buf.len();
        encode_record(&mut self.buf, op.kind(), |b| op.encode_payload(b));
        self.bytes += (self.buf.len() - before) as u64;
        self.ops += 1;
        self.open_tx = true;
        if self.buf.len() >= 1 << 20 {
            self.file.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    /// End the current transaction. Cheap: no fsync until `finish`.
    pub fn commit(&mut self) -> io::Result<()> {
        let before = self.buf.len();
        encode_record(&mut self.buf, K_TX_END, |_| {});
        self.bytes += (self.buf.len() - before) as u64;
        self.file.write_all(&self.buf)?;
        self.buf.clear();
        self.open_tx = false;
        Ok(())
    }

    /// Bytes written so far, header included — the file's eventual size.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn ops(&self) -> u64 {
        self.ops
    }

    /// Commit whatever is pending and make the file durable.
    pub fn finish(mut self) -> io::Result<u64> {
        if self.open_tx {
            self.commit()?;
        }
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        Ok(self.bytes)
    }
}

fn encode_record<F: FnOnce(&mut Vec<u8>)>(out: &mut Vec<u8>, kind: u8, payload: F) {
    let start = out.len();
    out.push(kind);
    out.extend_from_slice(&0u32.to_le_bytes()); // length placeholder
    payload(out);
    let payload_len = (out.len() - start - 5) as u32;
    out[start + 1..start + 5].copy_from_slice(&payload_len.to_le_bytes());
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

/// Returns the byte offset just past the last complete, committed transaction.
fn replay<F: FnMut(Op)>(file: &mut File, header_len: u64, apply: &mut F) -> io::Result<u64> {
    file.seek(SeekFrom::Start(header_len))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(header_len + replay_records(&data, apply))
}

/// Apply every committed transaction in `data`, a run of records with the
/// header already stripped. Returns the length of the committed prefix; a
/// torn or corrupt tail is ignored, exactly as on open.
///
/// Two passes per transaction: the first checks every frame and validates
/// every payload without allocating, and only when the commit marker arrives
/// does the second decode and apply the records one at a time. Uncommitted
/// ops must not reach the graph, but that no longer means holding a whole
/// transaction's worth of decoded ops — for a bulk load that was a second
/// copy of the log in its most expensive form.
fn replay_records<F: FnMut(Op)>(data: &[u8], apply: &mut F) -> u64 {
    let mut pos = 0usize;
    let mut committed = 0u64;
    let mut tx_start = 0usize;

    while let Some((kind, body, end)) = frame(data, pos) {
        if kind == K_TX_END {
            let mut p = tx_start;
            while p < pos {
                let Some((k, payload, e)) = frame(data, p) else {
                    break;
                };
                match Op::decode(k, payload) {
                    Ok(op) => apply(op),
                    // Unreachable: validate accepts exactly what decode does.
                    Err(_) => debug_assert!(false, "validated record failed to decode"),
                }
                p = e;
            }
            committed = end as u64;
            tx_start = end;
        } else if Op::validate(kind, body).is_err() {
            break;
        }
        pos = end;
    }

    committed
}

/// The CRC-checked frame at `pos`: (kind, payload, end). `None` at a torn or
/// corrupt frame, or at the end of the data.
fn frame(data: &[u8], pos: usize) -> Option<(u8, &[u8], usize)> {
    if pos.checked_add(5)? > data.len() {
        return None;
    }
    let kind = data[pos];
    let len =
        u32::from_le_bytes([data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]]) as usize;
    let end = pos.checked_add(9)?.checked_add(len)?;
    if end > data.len() {
        return None;
    }
    let stored = u32::from_le_bytes([data[end - 4], data[end - 3], data[end - 2], data[end - 1]]);
    if crc32(&data[pos..pos + 5 + len]) != stored {
        return None;
    }
    Some((kind, &data[pos + 5..pos + 5 + len], end))
}

/// Replay a whole database file held in memory — header and all — without a
/// filesystem. This is how a `.gldb` reaches the wasm build: the host reads
/// the bytes and hands them over. Returns the committed length, as `open`
/// would have truncated the file to.
pub fn replay_bytes<F: FnMut(Op)>(bytes: &[u8], apply: &mut F) -> io::Result<u64> {
    let h = parse_header(bytes)?;
    let body = &bytes[h.header_len as usize..];
    Ok(h.header_len + replay_records(body, apply))
}

// ------------------------------------------------------- replication support

/// What a replicator needs to know about a file without opening the database.
#[derive(Clone, Copy, Debug)]
pub struct FileHeader {
    pub version: u32,
    /// Where the log records begin: after the header, and after the image
    /// if there is one. Offsets below this are never records.
    pub header_len: u64,
    /// Offset and length of the snapshot image. Length 0 means none.
    pub image_at: u64,
    pub image_len: u64,
    /// Identifies this lineage of the file. Changes on every compaction,
    /// because compaction rewrites every byte and invalidates every offset.
    /// All-zero means a v1 file, which predates the idea.
    pub generation: [u8; 16],
}

impl FileHeader {
    pub fn generation_hex(&self) -> String {
        hex16(&self.generation)
    }
    pub fn has_generation(&self) -> bool {
        self.generation != [0u8; 16]
    }
}

pub fn hex16(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn encode_header(generation: &[u8; 16], image_len: u64) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER_LEN as usize);
    header.extend_from_slice(MAGIC);
    codec::put_u32(&mut header, FORMAT_VERSION);
    codec::put_u32(&mut header, if image_len > 0 { FLAG_IMAGE } else { 0 });
    header.extend_from_slice(generation);
    codec::put_u64(&mut header, image_len);
    codec::put_u64(&mut header, HEADER_LEN + image_len);
    header.resize(60, 0);
    let crc = crc32(&header);
    codec::put_u32(&mut header, crc);
    header
}

/// A generation id. Not cryptographic — it exists to be *different* every
/// time, not to be unguessable. Seeded from the OS through RandomState, the
/// clock, and the pid, so two compactions a microsecond apart on the same
/// machine still differ.
fn new_generation() -> [u8; 16] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut out = [0u8; 16];

    let mut h = RandomState::new().build_hasher();
    h.write_u64(nanos);
    h.write_u32(std::process::id());
    let a = h.finish();

    let mut h2 = RandomState::new().build_hasher();
    h2.write_u64(a);
    h2.write_u64(&out as *const _ as u64); // ASLR
    let b = h2.finish();

    out[..8].copy_from_slice(&a.to_le_bytes());
    out[8..].copy_from_slice(&b.to_le_bytes());
    out
}

fn read_header_from(file: &mut File) -> io::Result<FileHeader> {
    file.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; HEADER_LEN as usize];
    let n = file.read(&mut head)?;
    parse_header(&head[..n])
}

/// Decode a header from the first bytes of a file. `head` may be shorter than
/// `HEADER_LEN` for a v1 or v2 file.
pub(crate) fn parse_header(head: &[u8]) -> io::Result<FileHeader> {
    let invalid = |m: String| io::Error::new(io::ErrorKind::InvalidData, m);
    let n = head.len();
    if n < HEADER_V1_LEN as usize {
        return Err(invalid("file is too short to be a glider database".into()));
    }
    if &head[0..8] != MAGIC && &head[0..8] != MAGIC_LEGACY {
        return Err(invalid("not a glider database (bad magic)".into()));
    }
    let version = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    let mut generation = [0u8; 16];
    match version {
        1 => Ok(FileHeader {
            version,
            header_len: HEADER_V1_LEN,
            image_at: 0,
            image_len: 0,
            generation,
        }),
        2 => {
            if n < HEADER_V2_LEN as usize {
                return Err(invalid("truncated v2 header".into()));
            }
            generation.copy_from_slice(&head[16..32]);
            Ok(FileHeader {
                version,
                header_len: HEADER_V2_LEN,
                image_at: 0,
                image_len: 0,
                generation,
            })
        }
        3 => {
            if n < HEADER_LEN as usize {
                return Err(invalid("truncated v3 header".into()));
            }
            let stored = u32::from_le_bytes([head[60], head[61], head[62], head[63]]);
            if crc32(&head[..60]) != stored {
                return Err(invalid("header checksum mismatch".into()));
            }
            generation.copy_from_slice(&head[16..32]);
            let image_len = u64::from_le_bytes(head[32..40].try_into().unwrap());
            let log_start = u64::from_le_bytes(head[40..48].try_into().unwrap());
            if HEADER_LEN.checked_add(image_len) != Some(log_start) {
                return Err(invalid("header image length and log start disagree".into()));
            }
            Ok(FileHeader {
                version,
                header_len: log_start,
                image_at: HEADER_LEN,
                image_len,
                generation,
            })
        }
        v => Err(invalid(format!(
            "unsupported format version {v} (this build reads up to {FORMAT_VERSION})"
        ))),
    }
}

/// Read a file's header without opening the database. This is what a
/// replicator in another process calls.
pub fn read_header(path: &Path) -> io::Result<FileHeader> {
    let mut f = File::open(path)?;
    read_header_from(&mut f)
}

/// Walk frames from `from` and return the offset just past the last complete,
/// committed transaction.
///
/// This is the safety valve for streaming: a writer in another process may be
/// mid-transaction, so the bytes at the end of the file are not necessarily
/// shippable. Frames are CRC-checked as we go, so a torn or corrupt tail stops
/// the scan rather than being replicated.
pub fn scan_committed_end(path: &Path, from: u64) -> io::Result<u64> {
    let mut f = File::open(path)?;
    // Offsets below `header_len` are the header and the snapshot image, not
    // records. Callers track offsets from 0 so that segment 0 carries both,
    // so clamp the scan start here rather than making every caller remember.
    let header = read_header_from(&mut f)?;
    let from = from.max(header.header_len);
    let len = f.metadata()?.len();
    if from >= len {
        return Ok(from);
    }
    f.seek(SeekFrom::Start(from))?;
    let mut data = Vec::with_capacity((len - from) as usize);
    f.read_to_end(&mut data)?;

    let mut pos = 0usize;
    let mut committed = from;
    while pos + 5 <= data.len() {
        let kind = data[pos];
        let plen = u32::from_le_bytes([data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]])
            as usize;
        let end = match pos.checked_add(9).and_then(|v| v.checked_add(plen)) {
            Some(e) if e <= data.len() => e,
            _ => break,
        };
        let stored =
            u32::from_le_bytes([data[end - 4], data[end - 3], data[end - 2], data[end - 1]]);
        if crc32(&data[pos..pos + 5 + plen]) != stored {
            break;
        }
        if kind == K_TX_END {
            committed = from + end as u64;
        }
        pos = end;
    }
    Ok(committed)
}

impl Store {
    pub fn generation(&self) -> [u8; 16] {
        self.generation
    }
    pub fn generation_hex(&self) -> String {
        hex16(&self.generation)
    }
    /// Byte offset of the end of the last durable commit. Everything below
    /// this is safe to replicate.
    pub fn committed_len(&self) -> u64 {
        self.committed_len
    }
    pub fn header_len(&self) -> u64 {
        self.header_len
    }
}

// --------------------------------------------------------------- durability

/// fsync the directory holding `path`, so a create or rename inside it
/// survives a crash. A no-op on Windows, where directories are not openable
/// this way and the guarantee comes from elsewhere; failures are ignored
/// because some filesystems refuse the call and the write itself succeeded.
fn sync_parent(path: &Path) {
    #[cfg(not(windows))]
    {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
        let dir = dir.unwrap_or_else(|| Path::new("."));
        if let Ok(handle) = File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    #[cfg(windows)]
    let _ = path;
}

// -------------------------------------------------------------------- locks

/// An advisory write lock: a sidecar file next to the database.
///
/// Two writers on one glider file corrupt it, and nothing in the format can
/// detect that after the fact. A lock file is not airtight — a `kill -9`
/// leaves one behind, and a network filesystem may not honour `create_new`
/// atomically — but it turns the overwhelmingly common accident (starting the
/// service twice) from silent corruption into a clear error.
struct Lock {
    path: PathBuf,
}

impl Lock {
    fn acquire(db: &Path, force: bool) -> io::Result<Lock> {
        let path = lock_path(db);

        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let _ = write!(
                        f,
                        "{{\"pid\":{},\"since\":{}}}\n",
                        std::process::id(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    );
                    let _ = f.sync_all();
                    return Ok(Lock { path });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(&path).unwrap_or_default();
                    let pid = holder
                        .split("\"pid\":")
                        .nth(1)
                        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
                        .and_then(|s| s.parse::<u32>().ok());

                    // If the platform can prove the holder is gone, take over
                    // rather than making a human do it.
                    let stale = force || pid.map(process_is_gone).unwrap_or(false);
                    if stale {
                        std::fs::remove_file(&path)?;
                        continue;
                    }

                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        match pid {
                            Some(p) => format!(
                                "{} is locked by pid {p}. If that process is gone, \
                                 delete {} or reopen with force.",
                                db.display(),
                                path.display()
                            ),
                            None => format!(
                                "{} is locked by another process ({} exists).",
                                db.display(),
                                path.display()
                            ),
                        },
                    ));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn lock_path(db: &Path) -> PathBuf {
    let mut name = db.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    db.with_file_name(name)
}

/// True only when we can *prove* the process is gone. Unknown means alive.
fn process_is_gone(pid: u32) -> bool {
    if pid == std::process::id() {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        !Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

// ----------------------------------------------------------------- integrity

pub struct VerifyReport {
    pub records: u64,
    pub transactions: u64,
    pub committed_len: u64,
    pub file_len: u64,
    /// Where the log stopped making sense, if it did. A bad offset equal to
    /// `committed_len` is an ordinary torn tail from a crash; anything lower
    /// means real corruption and data loss above it.
    pub bad_offset: Option<u64>,
    /// The snapshot image, if the file has one: what it holds, or why it
    /// failed its checks. Every section and every property chunk is read and
    /// checksummed.
    pub image: Option<Result<crate::image::ImageReport, String>>,
}

/// Walk every frame: check CRCs, decode every payload, count what is there.
/// Reads the file without locking it or writing to it, so it is safe to run
/// against a database another process has open.
pub fn verify(path: &Path) -> io::Result<VerifyReport> {
    let mut f = File::open(path)?;
    let header = read_header_from(&mut f)?;
    let file_len = f.metadata()?.len();
    f.seek(SeekFrom::Start(header.header_len))?;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;

    let mut pos = 0usize;
    let mut records = 0u64;
    let mut transactions = 0u64;
    let mut committed = header.header_len;
    let mut bad = None;

    while pos + 5 <= data.len() {
        let at = header.header_len + pos as u64;
        let kind = data[pos];
        let len = u32::from_le_bytes([data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]])
            as usize;
        let end = match pos.checked_add(9).and_then(|v| v.checked_add(len)) {
            Some(e) if e <= data.len() => e,
            _ => {
                bad = Some(at);
                break;
            }
        };
        let stored =
            u32::from_le_bytes([data[end - 4], data[end - 3], data[end - 2], data[end - 1]]);
        if crc32(&data[pos..pos + 5 + len]) != stored {
            bad = Some(at);
            break;
        }
        if kind == K_TX_END {
            transactions += 1;
            committed = header.header_len + end as u64;
        } else if Op::decode(kind, &data[pos + 5..pos + 5 + len]).is_ok() {
            records += 1;
        } else {
            bad = Some(at);
            break;
        }
        pos = end;
    }

    let image = (header.image_len > 0).then(|| {
        crate::image::verify_file(path, header.image_at, header.image_len)
            .map_err(|e| e.to_string())
    });

    Ok(VerifyReport {
        records,
        transactions,
        committed_len: committed,
        file_len,
        bad_offset: bad,
        image,
    })
}
