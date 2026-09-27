//! External merge sort: sort more (key, value) records than fit in memory.
//!
//! Records accumulate in one flat buffer up to a byte budget; each full
//! buffer is sorted and written out as a run. Finishing merges the runs with
//! a k-way heap merge, streaming — memory stays at the budget plus one read
//! buffer per run, however many records went in. With more runs than
//! `max_fan_in`, runs are merged in rounds first.
//!
//! Keys compare as bytes. Records with equal keys come out in no particular
//! order.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub struct Sorter {
    dir: PathBuf,
    name: String,
    budget: usize,
    max_fan_in: usize,
    data: Vec<u8>,
    /// (offset into data, key length, value length)
    index: Vec<(u64, u32, u32)>,
    runs: Vec<PathBuf>,
    next_run: usize,
    records: u64,
}

fn put_varint(w: &mut impl Write, mut v: u64) -> io::Result<()> {
    let mut buf = [0u8; 10];
    let mut n = 0;
    while v >= 0x80 {
        buf[n] = (v as u8) | 0x80;
        v >>= 7;
        n += 1;
    }
    buf[n] = v as u8;
    w.write_all(&buf[..=n])
}

fn get_varint(r: &mut impl Read) -> io::Result<Option<u64>> {
    let mut v = 0u64;
    let mut shift = 0;
    let mut b = [0u8; 1];
    loop {
        if r.read(&mut b)? == 0 {
            return if shift == 0 {
                Ok(None)
            } else {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "torn sort run"))
            };
        }
        v |= ((b[0] & 0x7f) as u64) << shift;
        if b[0] & 0x80 == 0 {
            return Ok(Some(v));
        }
        shift += 7;
    }
}

impl Sorter {
    /// A sorter spilling into `dir` (created if needed), holding at most
    /// about `budget` bytes of records in memory.
    /// Nothing touches the filesystem until the first spill, so a sort that
    /// fits its budget works where there is none (wasm).
    pub fn new(dir: &Path, name: &str, budget: usize) -> io::Result<Sorter> {
        // Unique per process and sorter: several may share a directory.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Sorter {
            dir: dir.to_path_buf(),
            name: format!("glider-{name}-{}-{seq}", crate::ooc::pid()),
            budget: budget.max(64 << 10),
            max_fan_in: 128,
            data: Vec::new(),
            index: Vec::new(),
            runs: Vec::new(),
            next_run: 0,
            records: 0,
        })
    }

    /// For tests: merge in rounds once there are more than this many runs.
    pub fn with_fan_in(mut self, n: usize) -> Sorter {
        self.max_fan_in = n.max(2);
        self
    }

    pub fn push(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        let off = self.data.len() as u64;
        self.data.extend_from_slice(key);
        self.data.extend_from_slice(value);
        self.index.push((off, key.len() as u32, value.len() as u32));
        self.records += 1;
        if self.data.len() + self.index.len() * 16 >= self.budget {
            self.spill()?;
        }
        Ok(())
    }

    pub fn records(&self) -> u64 {
        self.records
    }

    fn key(&self, i: &(u64, u32, u32)) -> &[u8] {
        &self.data[i.0 as usize..i.0 as usize + i.1 as usize]
    }

    fn sort_buffer(&mut self) {
        let mut index = std::mem::take(&mut self.index);
        index.sort_unstable_by(|a, b| self.key(a).cmp(self.key(b)));
        self.index = index;
    }

    fn run_path(&mut self) -> PathBuf {
        self.next_run += 1;
        self.dir.join(format!("{}-{:06}.run", self.name, self.next_run))
    }

    fn spill(&mut self) -> io::Result<()> {
        if self.index.is_empty() {
            return Ok(());
        }
        self.sort_buffer();
        fs::create_dir_all(&self.dir)?;
        let path = self.run_path();
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&path)?);
        for &(off, kl, vl) in &self.index {
            let o = off as usize;
            put_varint(&mut w, kl as u64)?;
            put_varint(&mut w, vl as u64)?;
            w.write_all(&self.data[o..o + kl as usize + vl as usize])?;
        }
        w.flush()?;
        self.runs.push(path);
        self.data.clear();
        self.index.clear();
        Ok(())
    }

    /// Stop accepting records and return them in key order.
    pub fn finish(mut self) -> io::Result<Sorted> {
        if self.runs.is_empty() {
            self.sort_buffer();
            let index = std::mem::take(&mut self.index);
            return Ok(Sorted::Mem {
                data: std::mem::take(&mut self.data),
                index,
                at: 0,
            });
        }
        self.spill()?;
        // Merge in rounds until the fan-in fits.
        while self.runs.len() > self.max_fan_in {
            let batch: Vec<PathBuf> = self.runs.drain(..self.max_fan_in).collect();
            let out = self.run_path();
            let mut merged = Merge::open(&batch)?;
            let mut w = BufWriter::with_capacity(1 << 20, File::create(&out)?);
            while let Some((k, v)) = merged.next()? {
                put_varint(&mut w, k.len() as u64)?;
                put_varint(&mut w, v.len() as u64)?;
                w.write_all(&k)?;
                w.write_all(&v)?;
            }
            w.flush()?;
            drop(merged);
            for p in &batch {
                let _ = fs::remove_file(p);
            }
            self.runs.push(out);
        }
        let runs = std::mem::take(&mut self.runs);
        Ok(Sorted::Disk(Merge::open(&runs)?))
    }
}

impl Drop for Sorter {
    fn drop(&mut self) {
        for p in &self.runs {
            let _ = fs::remove_file(p);
        }
    }
}

/// Sorted output.
pub enum Sorted {
    Mem {
        data: Vec<u8>,
        index: Vec<(u64, u32, u32)>,
        at: usize,
    },
    Disk(Merge),
}

impl Sorted {
    /// The next record, in key order.
    pub fn next(&mut self) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        match self {
            Sorted::Mem { data, index, at } => {
                let Some(&(off, kl, vl)) = index.get(*at) else {
                    return Ok(None);
                };
                *at += 1;
                let o = off as usize;
                let k = data[o..o + kl as usize].to_vec();
                let v = data[o + kl as usize..o + kl as usize + vl as usize].to_vec();
                Ok(Some((k, v)))
            }
            Sorted::Disk(m) => m.next(),
        }
    }
}

struct RunReader {
    r: BufReader<File>,
    path: PathBuf,
}

impl RunReader {
    fn next(&mut self) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(kl) = get_varint(&mut self.r)? else {
            return Ok(None);
        };
        let vl = get_varint(&mut self.r)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "torn sort run"))?;
        let mut k = vec![0u8; kl as usize];
        self.r.read_exact(&mut k)?;
        let mut v = vec![0u8; vl as usize];
        self.r.read_exact(&mut v)?;
        Ok(Some((k, v)))
    }
}

struct HeapItem {
    key: Vec<u8>,
    val: Vec<u8>,
    run: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, o: &Self) -> bool {
        self.key == o.key && self.run == o.run
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, o: &Self) -> Ordering {
        // BinaryHeap is a max-heap: reverse for smallest first.
        o.key.cmp(&self.key).then(o.run.cmp(&self.run))
    }
}

/// A k-way merge over sorted run files. Deletes them when done.
pub struct Merge {
    readers: Vec<RunReader>,
    heap: BinaryHeap<HeapItem>,
}

impl Merge {
    fn open(runs: &[PathBuf]) -> io::Result<Merge> {
        let mut readers = Vec::with_capacity(runs.len());
        for p in runs {
            readers.push(RunReader {
                r: BufReader::with_capacity(256 << 10, File::open(p)?),
                path: p.clone(),
            });
        }
        let mut heap = BinaryHeap::with_capacity(readers.len());
        for (i, r) in readers.iter_mut().enumerate() {
            if let Some((key, val)) = r.next()? {
                heap.push(HeapItem { key, val, run: i });
            }
        }
        Ok(Merge { readers, heap })
    }

    pub fn next(&mut self) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let Some(top) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some((key, val)) = self.readers[top.run].next()? {
            self.heap.push(HeapItem { key, val, run: top.run });
        }
        Ok(Some((top.key, top.val)))
    }
}

impl Drop for Merge {
    fn drop(&mut self) {
        for r in &self.readers {
            let _ = fs::remove_file(&r.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_across_runs_and_merge_rounds() {
        let dir = std::env::temp_dir().join(format!("glider-extsort-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (budget, fan) in [(64 << 10, 128), (64 << 10, 3), (1 << 30, 128)] {
            let mut s = Sorter::new(&dir, "t", budget).unwrap().with_fan_in(fan);
            let mut x = 0x1234_5678_9abc_def1u64;
            let mut want = Vec::new();
            for i in 0..60_000u64 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let k = x.to_be_bytes().to_vec();
                let v = (i as u32).to_le_bytes().repeat((x % 5) as usize);
                s.push(&k, &v).unwrap();
                want.push((k, v));
            }
            want.sort();
            let mut got = Vec::new();
            let mut it = s.finish().unwrap();
            while let Some(kv) = it.next().unwrap() {
                got.push(kv);
            }
            assert_eq!(got, want, "budget {budget} fan-in {fan}");
        }
        // Runs are cleaned up.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
