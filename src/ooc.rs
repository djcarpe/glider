//! Out-of-core state for algorithms: per-node arrays that live in memory
//! while they fit a budget, and in a temporary file behind a small block
//! cache when they do not.
//!
//! The budget is set around a computation with [`with_budget`]; outside one
//! (tests, small in-memory projections) every array is a plain `Vec`.
//! Algorithms are written once against [`StateVec`], so the same code runs
//! at any size, with identical results.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;

thread_local! {
    /// Bytes of state arrays still allowed in memory; None = unlimited.
    static BUDGET: Cell<Option<u64>> = const { Cell::new(None) };
    static TEMP_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    /// (block bytes, cached blocks) for arrays spilled from now on.
    static SHAPE: Cell<(usize, usize)> = const { Cell::new((BLOCK, CACHE_BLOCKS)) };
}

/// For tests: spill in `block`-byte blocks (a power of two, at least 8)
/// keeping `cached` of them per array, so small arrays exercise eviction.
#[doc(hidden)]
pub fn set_spill_shape(block: usize, cached: usize) {
    assert!(block.is_power_of_two() && block >= 8);
    SHAPE.with(|s| s.set((block, cached.max(1))));
}

/// Run `f` with at most `bytes` of algorithm state held in memory; arrays
/// beyond that spill to temporary files under `temp_dir`.
pub fn with_budget<R>(bytes: u64, temp_dir: PathBuf, f: impl FnOnce() -> R) -> R {
    let prev = BUDGET.with(|b| b.replace(Some(bytes)));
    let prev_dir = TEMP_DIR.with(|d| d.replace(Some(temp_dir)));
    let r = f();
    BUDGET.with(|b| b.set(prev));
    TEMP_DIR.with(|d| *d.borrow_mut() = prev_dir);
    r
}

/// Where spill files go: the directory set by [`with_budget`], else the
/// system temp directory.
pub fn temp_dir() -> PathBuf {
    TEMP_DIR
        .with(|d| d.borrow().clone())
        .unwrap_or_else(system_temp_dir)
}

/// This process's id, for naming temp files; 0 where there are no processes
/// (wasm32-unknown-unknown, where asking panics).
pub fn pid() -> u32 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        0
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        std::process::id()
    }
}

/// The system temp directory. wasm32-unknown-unknown has none (asking
/// panics there); a name is returned that nothing will create, since spills
/// only happen past a budget and in-memory graphs there have no budget.
pub fn system_temp_dir() -> PathBuf {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        PathBuf::from("/tmp")
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        std::env::temp_dir()
    }
}

/// Bytes of state still allowed in memory; None = unlimited.
pub fn budget_left() -> Option<u64> {
    BUDGET.with(|b| b.get())
}

/// Charge `bytes` held elsewhere (an in-memory projection, say) to the
/// budget, even past it; [`give`] returns them.
pub fn take(bytes: u64) {
    BUDGET.with(|b| {
        if let Some(left) = b.get() {
            b.set(Some(left.saturating_sub(bytes)));
        }
    });
}

pub fn give(bytes: u64) {
    release(bytes)
}

/// Take `bytes` from the budget if they fit.
fn reserve(bytes: u64) -> bool {
    BUDGET.with(|b| match b.get() {
        None => true,
        Some(left) if left >= bytes => {
            b.set(Some(left - bytes));
            true
        }
        Some(_) => false,
    })
}

fn release(bytes: u64) {
    BUDGET.with(|b| {
        if let Some(left) = b.get() {
            b.set(Some(left + bytes));
        }
    });
}

/// Plain values that can be stored as bytes.
pub trait Pod: Copy {
    const SIZE: usize;
    fn put(self, b: &mut [u8]);
    fn get(b: &[u8]) -> Self;
}

macro_rules! pod {
    ($($t:ty),*) => {$(
        impl Pod for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            #[inline]
            fn put(self, b: &mut [u8]) {
                b[..Self::SIZE].copy_from_slice(&self.to_le_bytes());
            }
            #[inline]
            fn get(b: &[u8]) -> Self {
                let mut a = [0u8; std::mem::size_of::<$t>()];
                a.copy_from_slice(&b[..Self::SIZE]);
                <$t>::from_le_bytes(a)
            }
        }
    )*};
}
pod!(u8, u32, u64, i64, f64, usize);

impl Pod for bool {
    const SIZE: usize = 1;
    fn put(self, b: &mut [u8]) {
        b[0] = self as u8;
    }
    fn get(b: &[u8]) -> Self {
        b[0] != 0
    }
}

impl<A: Pod, B: Pod> Pod for (A, B) {
    const SIZE: usize = A::SIZE + B::SIZE;
    fn put(self, b: &mut [u8]) {
        self.0.put(b);
        self.1.put(&mut b[A::SIZE..]);
    }
    fn get(b: &[u8]) -> Self {
        (A::get(b), B::get(&b[A::SIZE..]))
    }
}

impl<A: Pod, B: Pod, C: Pod> Pod for (A, B, C) {
    const SIZE: usize = A::SIZE + B::SIZE + C::SIZE;
    fn put(self, b: &mut [u8]) {
        self.0.put(b);
        self.1.put(&mut b[A::SIZE..]);
        self.2.put(&mut b[A::SIZE + B::SIZE..]);
    }
    fn get(b: &[u8]) -> Self {
        (A::get(b), B::get(&b[A::SIZE..]), C::get(&b[A::SIZE + B::SIZE..]))
    }
}

/// A temp file of `T`s, created on first write and removed on drop.
struct SpillFile {
    file: Option<File>,
    path: PathBuf,
}

impl SpillFile {
    fn new(kind: &str) -> SpillFile {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SpillFile {
            file: None,
            path: temp_dir().join(format!("glider-{kind}-{}-{n}.bin", pid())),
        }
    }

    fn write<T: Pod>(&mut self, at: u64, items: &[T]) {
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            self.file = Some(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&self.path)
                    .expect("create algorithm spill file"),
            );
        }
        let mut buf = vec![0u8; items.len() * T::SIZE];
        for (i, x) in items.iter().enumerate() {
            x.put(&mut buf[i * T::SIZE..]);
        }
        write_at(self.file.as_ref().unwrap(), &buf, at * T::SIZE as u64).expect("spill write");
    }

    fn read<T: Pod>(&self, at: u64, n: usize, out: &mut Vec<T>) {
        let mut buf = vec![0u8; n * T::SIZE];
        read_at(self.file.as_ref().expect("spill file"), &mut buf, at * T::SIZE as u64).expect("spill read");
        for i in 0..n {
            out.push(T::get(&buf[i * T::SIZE..]));
        }
    }
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Items held in memory per chunk by queues and stacks once they spill.
fn chunk_items(size: usize) -> usize {
    let (block, _) = SHAPE.with(|s| s.get());
    (block * 16 / size).max(2)
}

/// Memory for queues and stacks is taken from the budget this much at a
/// time; when the budget refuses, they start spilling.
const GROW_BYTES: u64 = 1 << 20;

/// Charge the budget for `items` of `size` in memory, in steps. False once
/// it refuses (the caller spills from then on).
fn grow(reserved: &mut u64, items: usize, size: usize) -> bool {
    let need = (items * size) as u64;
    while need > *reserved {
        let step = GROW_BYTES.max(size as u64);
        if !reserve(step) {
            return false;
        }
        *reserved += step;
    }
    true
}

/// A FIFO queue that keeps its head and tail in memory and the middle in
/// a temp file, so it can hold more than memory.
pub struct SpillQueue<T: Pod> {
    front: std::collections::VecDeque<T>,
    back: Vec<T>,
    file: SpillFile,
    /// Items in the file: [rpos, wpos).
    rpos: u64,
    wpos: u64,
    /// Items per chunk once spilling; usize::MAX while all in memory.
    chunk: usize,
    reserved: u64,
}

impl<T: Pod> Drop for SpillQueue<T> {
    fn drop(&mut self) {
        release(self.reserved);
    }
}

impl<T: Pod> Default for SpillQueue<T> {
    fn default() -> Self {
        SpillQueue::new()
    }
}

impl<T: Pod> SpillQueue<T> {
    pub fn new() -> SpillQueue<T> {
        SpillQueue {
            front: std::collections::VecDeque::new(),
            back: Vec::new(),
            file: SpillFile::new("queue"),
            rpos: 0,
            wpos: 0,
            chunk: usize::MAX,
            reserved: 0,
        }
    }

    pub fn len(&self) -> u64 {
        self.front.len() as u64 + (self.wpos - self.rpos) + self.back.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn push_back(&mut self, x: T) {
        if self.chunk == usize::MAX && !grow(&mut self.reserved, self.front.len() + 1, T::SIZE) {
            self.chunk = chunk_items(T::SIZE);
        }
        // Order: front, then file, then back.
        if self.back.is_empty() && self.wpos == self.rpos && self.front.len() < self.chunk {
            self.front.push_back(x);
            return;
        }
        self.back.push(x);
        if self.back.len() >= self.chunk {
            self.file.write(self.wpos, &self.back);
            self.wpos += self.back.len() as u64;
            self.back.clear();
        }
    }

    pub fn pop_front(&mut self) -> Option<T> {
        if let Some(x) = self.front.pop_front() {
            return Some(x);
        }
        if self.wpos > self.rpos {
            let n = ((self.wpos - self.rpos) as usize).min(self.chunk);
            let mut buf: Vec<T> = Vec::with_capacity(n);
            self.file.read(self.rpos, n, &mut buf);
            self.rpos += n as u64;
            if self.rpos == self.wpos {
                self.rpos = 0;
                self.wpos = 0;
            }
            self.front.extend(buf);
            return self.front.pop_front();
        }
        if self.back.is_empty() {
            return None;
        }
        self.front.extend(self.back.drain(..));
        self.front.pop_front()
    }
}

/// A LIFO stack whose bottom spills to a temp file in chunks.
pub struct SpillStack<T: Pod> {
    top: Vec<T>,
    file: SpillFile,
    /// Items in the file.
    spilled: u64,
    /// Items per chunk once spilling; usize::MAX while all in memory.
    chunk: usize,
    reserved: u64,
}

impl<T: Pod> Drop for SpillStack<T> {
    fn drop(&mut self) {
        release(self.reserved);
    }
}

impl<T: Pod> Default for SpillStack<T> {
    fn default() -> Self {
        SpillStack::new()
    }
}

impl<T: Pod> SpillStack<T> {
    pub fn new() -> SpillStack<T> {
        SpillStack {
            top: Vec::new(),
            file: SpillFile::new("stack"),
            spilled: 0,
            chunk: usize::MAX,
            reserved: 0,
        }
    }

    pub fn len(&self) -> u64 {
        self.spilled + self.top.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn push(&mut self, x: T) {
        if self.chunk == usize::MAX && !grow(&mut self.reserved, self.top.len() + 1, T::SIZE) {
            self.chunk = chunk_items(T::SIZE);
        }
        if self.chunk != usize::MAX && self.top.len() >= 2 * self.chunk {
            // Move the bottom chunk out.
            self.file.write(self.spilled, &self.top[..self.chunk]);
            self.spilled += self.chunk as u64;
            self.top.drain(..self.chunk);
        }
        self.top.push(x);
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.top.is_empty() && self.spilled > 0 {
            let n = (self.spilled as usize).min(self.chunk);
            self.spilled -= n as u64;
            self.file.read(self.spilled, n, &mut self.top);
        }
        self.top.pop()
    }

    pub fn last(&mut self) -> Option<T> {
        let x = self.pop()?;
        self.top.push(x);
        Some(x)
    }
}

const BLOCK: usize = 64 * 1024;
/// Blocks a spilled array keeps cached.
const CACHE_BLOCKS: usize = 256;

/// A fixed-length array of `T`, in memory or spilled to a file.
pub struct StateVec<T: Pod> {
    len: usize,
    mem: Option<Vec<T>>,
    disk: Option<Disk>,
    reserved: u64,
}

struct Disk {
    /// Created on the first write-back: arrays whose blocks all fit the
    /// cache never touch the disk.
    file: Option<File>,
    path: PathBuf,
    block: usize,
    cached: usize,
    init: Vec<u8>,
    blocks: HashMap<u64, (Vec<u8>, bool, u64)>,
    written: std::collections::HashSet<u64>,
    tick: u64,
}

impl Drop for Disk {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Disk {
    fn create(init: Vec<u8>) -> io::Result<Disk> {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = temp_dir().join(format!("glider-state-{}-{n}.bin", pid()));
        let (block, cached) = SHAPE.with(|s| s.get());
        Ok(Disk {
            file: None,
            path,
            block,
            cached,
            init,
            blocks: HashMap::new(),
            written: std::collections::HashSet::new(),
            tick: 0,
        })
    }

    fn file(&mut self) -> io::Result<&File> {
        if self.file.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            self.file = Some(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&self.path)?,
            );
        }
        Ok(self.file.as_ref().unwrap())
    }

    fn block(&mut self, b: u64) -> &mut (Vec<u8>, bool, u64) {
        self.tick += 1;
        let tick = self.tick;
        let size = self.block;
        if !self.blocks.contains_key(&b) {
            if self.blocks.len() >= self.cached {
                let victim = *self.blocks.iter().min_by_key(|(_, v)| v.2).map(|(k, _)| k).unwrap();
                let (data, dirty, _) = self.blocks.remove(&victim).unwrap();
                if dirty {
                    let f = self.file().expect("create algorithm spill file");
                    write_at(f, &data, victim * size as u64).expect("state spill write");
                    self.written.insert(victim);
                }
            }
            let data = if self.written.contains(&b) {
                let mut d = vec![0u8; size];
                let f = self.file.as_ref().expect("spill file");
                read_at(f, &mut d, b * size as u64).expect("state spill read");
                d
            } else {
                // Never written: every element holds the initial value.
                let mut d = Vec::with_capacity(size);
                while d.len() + self.init.len() <= size {
                    d.extend_from_slice(&self.init);
                }
                d.resize(size, 0);
                d
            };
            self.blocks.insert(b, (data, false, tick));
        }
        let e = self.blocks.get_mut(&b).unwrap();
        e.2 = tick;
        e
    }
}

fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        f.read_exact_at(buf, off)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = f.seek_read(&mut buf[done..], off + done as u64)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (f, buf, off);
        Err(io::Error::new(io::ErrorKind::Unsupported, "no positional io"))
    }
}

fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        f.write_all_at(buf, off)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            done += f.seek_write(&buf[done..], off + done as u64)?;
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (f, buf, off);
        Err(io::Error::new(io::ErrorKind::Unsupported, "no positional io"))
    }
}

impl<T: Pod> StateVec<T> {
    /// `len` copies of `init`: in memory if the budget allows, else spilled.
    pub fn new(len: usize, init: T) -> StateVec<T> {
        let bytes = (len as u64).saturating_mul(T::SIZE as u64);
        if reserve(bytes) {
            return StateVec {
                len,
                mem: Some(vec![init; len]),
                disk: None,
                reserved: bytes,
            };
        }
        let mut one = vec![0u8; T::SIZE];
        init.put(&mut one);
        StateVec {
            len,
            mem: None,
            disk: Some(Disk::create(one).expect("create algorithm spill file")),
            reserved: 0,
        }
    }

    pub fn from_vec(v: Vec<T>) -> StateVec<T> {
        StateVec {
            len: v.len(),
            mem: Some(v),
            disk: None,
            reserved: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_spilled(&self) -> bool {
        self.disk.is_some()
    }

    #[inline]
    pub fn get(&mut self, i: usize) -> T {
        if let Some(m) = &self.mem {
            return m[i];
        }
        let off = i * T::SIZE;
        let d = self.disk.as_mut().expect("disk");
        let (b, o) = ((off / d.block) as u64, off % d.block);
        // Elements never straddle blocks: sizes divide the block size.
        T::get(&d.block(b).0[o..o + T::SIZE])
    }

    #[inline]
    pub fn set(&mut self, i: usize, v: T) {
        if let Some(m) = &mut self.mem {
            m[i] = v;
            return;
        }
        let off = i * T::SIZE;
        let d = self.disk.as_mut().expect("disk");
        let (b, o) = ((off / d.block) as u64, off % d.block);
        let blk = d.block(b);
        v.put(&mut blk.0[o..o + T::SIZE]);
        blk.1 = true;
    }

    /// Fill every element with `v`.
    pub fn fill(&mut self, v: T) {
        if let Some(m) = &mut self.mem {
            for x in m.iter_mut() {
                *x = v;
            }
            return;
        }
        let mut one = vec![0u8; T::SIZE];
        v.put(&mut one);
        let d = self.disk.as_mut().expect("disk");
        d.init = one;
        d.blocks.clear();
        d.written.clear();
    }

    /// Swap contents with another array of the same length.
    pub fn swap(&mut self, other: &mut StateVec<T>) {
        std::mem::swap(self, other);
    }

    /// All elements, materialised.
    pub fn to_vec(&mut self) -> Vec<T> {
        if let Some(m) = &self.mem {
            return m.clone();
        }
        (0..self.len).map(|i| self.get(i)).collect()
    }
}

impl<T: Pod> Drop for StateVec<T> {
    fn drop(&mut self) {
        release(self.reserved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spilled_queues_and_stacks_keep_order() {
        let dir = std::env::temp_dir().join(format!("glider-ooc-q-{}", std::process::id()));
        set_spill_shape(64, 2);
        with_budget(0, dir.clone(), || {
            let mut q: SpillQueue<(u64, u32)> = SpillQueue::new();
            let mut model = std::collections::VecDeque::new();
            let mut s: SpillStack<u64> = SpillStack::new();
            let mut smodel = Vec::new();
            let mut x = 7u64;
            for i in 0..200_000u64 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                if x % 3 != 0 {
                    q.push_back((i, x as u32));
                    model.push_back((i, x as u32));
                    s.push(i);
                    smodel.push(i);
                } else {
                    assert_eq!(q.pop_front(), model.pop_front());
                    assert_eq!(s.pop(), smodel.pop());
                }
                assert_eq!(q.len(), model.len() as u64);
                assert_eq!(s.len(), smodel.len() as u64);
            }
            while let Some(v) = model.pop_front() {
                assert_eq!(q.pop_front(), Some(v));
            }
            assert_eq!(q.pop_front(), None);
            while let Some(v) = smodel.pop() {
                assert_eq!(s.last(), Some(v));
                assert_eq!(s.pop(), Some(v));
            }
            assert_eq!(s.pop(), None);
        });
        assert_eq!(std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0), 0, "spill files removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spilled_arrays_behave_like_vecs() {
        let dir = std::env::temp_dir().join(format!("glider-ooc-{}", std::process::id()));
        set_spill_shape(4096, 3);
        with_budget(1024, dir.clone(), || {
            let mut a: StateVec<f64> = StateVec::new(100_000, 1.5);
            let mut b: StateVec<u32> = StateVec::new(10, 7);
            assert!(a.is_spilled());
            assert!(!b.is_spilled());
            let mut x = 1u64;
            let mut model = vec![1.5f64; 100_000];
            for _ in 0..300_000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let i = (x % 100_000) as usize;
                let v = (x % 1000) as f64;
                a.set(i, v);
                model[i] = v;
                let j = ((x >> 20) % 100_000) as usize;
                assert_eq!(a.get(j), model[j]);
            }
            assert_eq!(a.to_vec(), model);
            a.fill(2.0);
            assert_eq!(a.get(99_999), 2.0);
            b.set(3, 9);
            assert_eq!(b.to_vec(), vec![7, 7, 7, 9, 7, 7, 7, 7, 7, 7]);
        });
        assert_eq!(std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0), 0, "spill files removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
