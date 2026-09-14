//! Durability. One file, append-only, CRC-checked records grouped into
//! transactions by an explicit commit marker.
//!
//! The whole graph lives in memory; this file is the write-ahead log *and* the
//! persistent form. `compact` rewrites it as the minimal set of records that
//! reproduce current state, which is the equivalent of a checkpoint.
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
/// v2 added the generation id. v1 files still open; a COMPACT upgrades them.
pub const FORMAT_VERSION: u32 = 2;
/// magic(8) version(4) flags(4) generation(16)
pub const HEADER_LEN: u64 = 32;
pub const HEADER_V1_LEN: u64 = 16;

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

impl Store {
    /// Open (creating if needed) and replay every committed op into `apply`.
    pub fn open<F: FnMut(Op)>(path: &Path, sync: Sync, apply: F) -> io::Result<Store> {
        let mut apply = apply;
        Store::open_with(path, sync, false, &mut apply)
    }

    /// `force` breaks a lock held by a process we cannot prove is gone. Use it
    /// when you know the previous writer is dead and the platform will not
    /// tell us so.
    pub fn open_with<F: FnMut(Op)>(
        path: &Path,
        sync: Sync,
        force: bool,
        apply: &mut F,
    ) -> io::Result<Store> {
        let lock = Lock::acquire(path, force)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        let len = file.metadata()?.len();
        let (mut generation, header_len) = if len == 0 {
            let generation = new_generation();
            file.write_all(&encode_header(&generation))?;
            file.sync_all()?;
            // A new file is not durable until its directory entry is.
            sync_parent(path);
            (generation, HEADER_LEN)
        } else {
            let h = read_header_from(&mut file)?;
            (h.generation, h.header_len)
        };

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
        if committed_len < file.metadata()?.len() {
            file.set_len(committed_len)?;
            if header_len == HEADER_LEN {
                generation = new_generation();
                file.seek(SeekFrom::Start(16))?;
                file.write_all(&generation)?;
                file.sync_all()?;
            }
        }
        file.seek(SeekFrom::Start(committed_len))?;

        Ok(Store {
            path: path.to_path_buf(),
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

    /// Rewrite the file as a minimal snapshot: write to a sibling temp file,
    /// fsync it, then rename over the original. The rename is atomic, so a
    /// crash mid-compaction leaves the old database intact.
    pub fn compact(&mut self, ops: &[Op]) -> io::Result<()> {
        self.file.flush()?;
        let tmp = self.path.with_extension("compact.tmp");
        {
            let f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            let mut w = BufWriter::with_capacity(1 << 20, f);
            // A compaction rewrites every byte, so offsets from the old file
            // mean nothing now. That is exactly what a generation change is
            // for: replicas see it and start a fresh lineage instead of
            // appending segments onto a log that no longer exists.
            let generation = new_generation();
            w.write_all(&encode_header(&generation))?;
            self.generation = generation;
            self.header_len = HEADER_LEN;

            let mut buf = Vec::with_capacity(1 << 16);
            for op in ops {
                encode_record(&mut buf, op.kind(), |b| op.encode_payload(b));
                if buf.len() > (1 << 20) {
                    w.write_all(&buf)?;
                    buf.clear();
                }
            }
            encode_record(&mut buf, K_TX_END, |_| {});
            w.write_all(&buf)?;
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        // On Unix this always succeeds. On Windows a virus scanner or the
        // search indexer can hold a transient handle on the destination and
        // make MoveFileEx fail with ACCESS_DENIED; it clears in milliseconds,
        // so retry rather than lose the compaction.
        let mut attempt = 0;
        loop {
            match std::fs::rename(&tmp, &self.path) {
                Ok(()) => break,
                Err(e) if attempt < 10 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20 * attempt));
                    let _ = e;
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            }
        }
        // POSIX: the rename is not durable until the directory is synced. A
        // crash in this window can leave the old file, or neither.
        sync_parent(&self.path);

        let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        let len = file.metadata()?.len();
        let mut file = file;
        file.seek(SeekFrom::Start(len))?;
        self.file = BufWriter::with_capacity(1 << 16, file);
        self.committed_len = len;
        self.pending.clear();
        self.pending_ops = 0;
        self.records_written = ops.len() as u64;
        Ok(())
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

    let mut pos = 0usize;
    let mut committed = header_len;
    let mut batch: Vec<Op> = Vec::new();

    while pos + 5 <= data.len() {
        let kind = data[pos];
        let len = u32::from_le_bytes([data[pos + 1], data[pos + 2], data[pos + 3], data[pos + 4]])
            as usize;
        let end = match pos.checked_add(9).and_then(|v| v.checked_add(len)) {
            Some(e) if e <= data.len() => e,
            _ => break, // truncated tail
        };
        let body = &data[pos..pos + 5 + len];
        let stored_crc =
            u32::from_le_bytes([data[end - 4], data[end - 3], data[end - 2], data[end - 1]]);
        if crc32(body) != stored_crc {
            break; // corrupt tail: stop here, everything before is still good
        }
        if kind == K_TX_END {
            for op in batch.drain(..) {
                apply(op);
            }
            committed = header_len + end as u64;
        } else {
            match Op::decode(kind, &data[pos + 5..pos + 5 + len]) {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }
        pos = end;
    }

    Ok(committed)
}

// ------------------------------------------------------- replication support

/// What a replicator needs to know about a file without opening the database.
#[derive(Clone, Copy, Debug)]
pub struct FileHeader {
    pub version: u32,
    pub header_len: u64,
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

fn encode_header(generation: &[u8; 16]) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER_LEN as usize);
    header.extend_from_slice(MAGIC);
    codec::put_u32(&mut header, FORMAT_VERSION);
    codec::put_u32(&mut header, 0);
    header.extend_from_slice(generation);
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
    if n < HEADER_V1_LEN as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file is too short to be a glider database",
        ));
    }
    if &head[0..8] != MAGIC && &head[0..8] != MAGIC_LEGACY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a glider database (bad magic)",
        ));
    }
    let version = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    match version {
        1 => Ok(FileHeader {
            version,
            header_len: HEADER_V1_LEN,
            generation: [0u8; 16],
        }),
        2 => {
            if n < HEADER_LEN as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated v2 header",
                ));
            }
            let mut generation = [0u8; 16];
            generation.copy_from_slice(&head[16..32]);
            Ok(FileHeader {
                version,
                header_len: HEADER_LEN,
                generation,
            })
        }
        v => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported format version {v}"),
        )),
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
    // Offset 0 is the header, not a record. Callers track offsets from 0 so
    // that segment 0 carries the header, so clamp the scan start here rather
    // than making every caller remember.
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

    Ok(VerifyReport {
        records,
        transactions,
        committed_len: committed,
        file_len,
        bad_offset: bad,
    })
}
