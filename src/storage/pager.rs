//! Pages: where they live, how they are cached, and how transactions keep
//! them consistent.
//!
//! **Copy-on-write per transaction.** Every transaction has an epoch. A page
//! written in the current epoch may be modified in place; a page from an
//! earlier epoch is copied to a fresh page number first ([`Pager::writable`])
//! and the old one is freed when the transaction commits. So the committed
//! state is never touched by an open transaction, and:
//!
//! * **rollback** drops the pages the transaction allocated and restores the
//!   committed tree roots — nothing to undo;
//! * **`Full`** (out of memory, out of disk) is recoverable: roll back and
//!   carry on;
//! * **a checkpoint** only has to flush dirty pages and write a superblock
//!   naming the committed roots.
//!
//! **Two stores.** A `:memory:` database keeps every page in a vector, and
//! counts them against `max_memory`. A file-backed database keeps pages on
//! disk — the database file itself, then numbered segment files in
//! `<db>-data/` — behind a CLOCK cache of fixed size. Dirty pages may be
//! evicted at any time ("steal"): every dirty page belongs to an epoch newer
//! than the last checkpoint, so nothing the durable superblock refers to is
//! ever overwritten. That is what keeps memory bounded however large a
//! transaction gets.
//!
//! **The hot set.** A file-backed cache may keep part of itself, up to
//! `hot_size` bytes and never more than half of it, for pages used *often*
//! ([`Pager::set_hot_size`]). The sweep passes over them, and when the
//! process's budget is short, other databases' cold frames go before any
//! hot one. What "often" is, and why it costs a lookup constant work, is in
//! [`super::hot`]; the pager tells it of every frame read, filled, emptied
//! or moved, and of every page copied or freed. [`Pager::hot_list`] and
//! [`Pager::prewarm`] are the two ends of remembering it across opens
//! ([`super::hotlist`]).
//!
//! **Reuse.** A page freed by a commit is reusable immediately unless the
//! durable (checkpointed) state still refers to it; those wait for the next
//! checkpoint. The free list is written, at each checkpoint, as a chain of
//! pages that the superblock points to.
//!
//! **Holds.** While a backup copies the pages of one checkpoint, the writer
//! is asked to *hold* them: pages freed after it stay out of reuse (listed
//! in the free-list chain, marked held) until the hold is released, so the
//! copy stays consistent however many checkpoints happen meanwhile. The
//! superblock records the hold and the nonce of the request it honours.
//!
//! Pages 0 and 1 are the two superblocks, written alternately so a torn
//! superblock write always leaves the previous one intact.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use super::page::{self, Page};
use super::{corrupt, SError, SResult};
use crate::graph::IdBuild;
use crate::pread::PosFile;

pub const MAGIC: &[u8; 8] = b"GLIDERPG";
pub const FORMAT: u32 = 4;
/// Root slots in the superblock. Upper layers assign their meaning.
pub const ROOTS: usize = 16;
/// Counter slots in the superblock (next ids, counts, …).
pub const METAS: usize = 16;
pub const DEFAULT_SEGMENT_BYTES: u64 = 64 << 30;
pub const DEFAULT_CACHE_BYTES: u64 = 1 << 30;
/// How far past `max_memory` a `:memory:` database may go, in pages, so that
/// a full database can still delete (copy-on-write copies before it frees).
pub const SLACK_PAGES: u64 = 64;

/// How a pager is configured when created.
#[derive(Clone, Debug)]
pub struct Config {
    pub page_size: usize,
    /// File-backed: bytes of page cache.
    pub cache_bytes: u64,
    /// `:memory:`: the most bytes of pages it may hold.
    pub max_memory: u64,
    /// File-backed: bytes per segment file.
    pub segment_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            page_size: page::DEFAULT_PAGE,
            cache_bytes: DEFAULT_CACHE_BYTES,
            max_memory: physical_memory(),
            segment_bytes: DEFAULT_SEGMENT_BYTES,
        }
    }
}

/// Total physical memory, where the platform says; unlimited otherwise.
pub fn physical_memory() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    let kb: u64 = rest
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .parse()
                        .unwrap_or(0);
                    if kb > 0 {
                        return kb * 1024;
                    }
                }
            }
        }
    }
    u64::MAX
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub reads: u64,
    pub writes: u64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    /// Frames allocated beyond the cache size because every frame was pinned.
    pub overflow_frames: u64,
    pub copies: u64,
    pub commits: u64,
    pub rollbacks: u64,
    pub checkpoints: u64,
    /// Pages currently held in memory (all of them for `:memory:`).
    pub resident_pages: u64,
    /// Pages in use by the committed state and the open transaction.
    pub allocated_pages: u64,
    /// Most pages the hot set may hold; 0 without one.
    pub hot_capacity: u64,
    /// Pages in the hot set now (all of them resident).
    pub hot_pages: u64,
    /// Lookups that found their page hot.
    pub hot_hits: u64,
    /// Pages that joined the hot set.
    pub promotions: u64,
    /// Pages that left it: cooled, displaced by a hotter page, freed, or
    /// taken for the process's budget.
    pub demotions: u64,
    /// Pages made hot at open from the list of a previous one.
    pub prewarmed: u64,
}

pub struct Pager {
    ps: usize,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    ps: usize,
    store: Store,
    /// Epoch of the open transaction. The last committed state is epoch-1.
    epoch: u64,
    /// Epoch of the last checkpoint (file); pages born at or before it are
    /// referenced by the durable superblock.
    durable: u64,
    /// Next page number never handed out.
    hw: u64,
    /// Reusable now.
    free: Vec<u64>,
    /// Freed, but the durable state still refers to them: reusable after the
    /// next checkpoint.
    pending: Vec<u64>,
    /// Pages of the durable free-list chain itself.
    chain: Vec<u64>,
    /// Freed while a backup holds the durable state: kept out of reuse.
    held: Vec<u64>,
    /// A backup hold is in force (applies at the next checkpoint).
    hold: bool,
    /// Nonce of the hold request last honoured.
    pin_seen: u64,
    txn_alloc: Vec<u64>,
    txn_free: Vec<(u64, u64)>,
    roots: [u64; ROOTS],
    committed_roots: [u64; ROOTS],
    meta: [u64; METAS],
    committed_meta: [u64; METAS],
    generation: [u8; 16],
    wal_lsn: u64,
    /// Slot (0 or 1) of the superblock currently in force; the next
    /// checkpoint writes the other one.
    sb_slot: u64,
    stats: Stats,
}

enum Store {
    Mem(Mem),
    File(FileStore),
}

struct Mem {
    pages: Vec<Option<Page>>,
    used: u64,
    max: u64,
}

struct FileStore {
    base: PathBuf,
    seg_pages: u64,
    segs: Vec<Option<PosFile>>,
    cache: Cache,
    /// Segments written since the last sync.
    touched: Vec<bool>,
}

struct Cache {
    cap: usize,
    frames: Vec<Frame>,
    map: HashMap<u64, usize, IdBuild>,
    hand: usize,
    pool: Option<Arc<crate::budget::Pool>>,
    charge: u64,
    /// Frames kept for pages used often. None, the default, is the cache
    /// exactly as it is without one: nothing counted, nothing kept.
    hot: Option<Box<super::hot::Hot>>,
}

struct Frame {
    pno: u64,
    data: Page,
    dirty: bool,
    usage: u8,
}

fn new_page(ps: usize) -> Vec<u8> {
    vec![0u8; ps]
}

impl Pager {
    // ------------------------------------------------------------ opening

    /// A `:memory:` pager holding at most `max_memory` bytes of pages.
    pub fn memory(page_size: usize, max_memory: u64) -> Pager {
        assert!(page::valid_page_size(page_size));
        let inner = Inner {
            ps: page_size,
            store: Store::Mem(Mem {
                pages: vec![None, None],
                used: 0,
                max: max_memory,
            }),
            epoch: 1,
            durable: 0,
            hw: 2,
            free: Vec::new(),
            pending: Vec::new(),
            chain: Vec::new(),
            held: Vec::new(),
            hold: false,
            pin_seen: 0,
            txn_alloc: Vec::new(),
            txn_free: Vec::new(),
            roots: [0; ROOTS],
            committed_roots: [0; ROOTS],
            meta: [0; METAS],
            committed_meta: [0; METAS],
            generation: [0; 16],
            wal_lsn: 0,
            sb_slot: 0,
            stats: Stats::default(),
        };
        Pager {
            ps: page_size,
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Create a new file-backed database at `path`, which must not exist or
    /// be empty. Writes the initial superblocks.
    pub fn create(path: &Path, cfg: &Config, generation: [u8; 16]) -> SResult<Pager> {
        if !page::valid_page_size(cfg.page_size) {
            return Err(SError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "page size {} is not a power of two in 512..=32768",
                    cfg.page_size
                ),
            )));
        }
        let f = PosFile::open_rw(path)?;
        if f.len()? != 0 {
            return corrupt(format!("{} is not empty", path.display()));
        }
        let ps = cfg.page_size;
        let p = Pager {
            ps,
            inner: Arc::new(Mutex::new(Inner {
                ps,
                store: Store::File(FileStore {
                    base: path.to_path_buf(),
                    seg_pages: (cfg.segment_bytes / ps as u64).max(4),
                    segs: vec![Some(f)],
                    cache: Cache::new(cfg.cache_bytes, ps),
                    touched: vec![true],
                }),
                epoch: 1,
                durable: 0,
                hw: 2,
                free: Vec::new(),
                pending: Vec::new(),
                chain: Vec::new(),
                held: Vec::new(),
                hold: false,
                pin_seen: 0,
                txn_alloc: Vec::new(),
                txn_free: Vec::new(),
                roots: [0; ROOTS],
                committed_roots: [0; ROOTS],
                meta: [0; METAS],
                committed_meta: [0; METAS],
                generation,
                wal_lsn: 0,
                sb_slot: 0,
                stats: Stats::default(),
            })),
        };
        // Two superblocks so the first checkpoint has somewhere to go.
        {
            let mut g = p.lock();
            g.write_superblock(0)?;
            g.write_superblock(1)?;
            g.sync_all()?;
        }
        register_cache(&p);
        Ok(p)
    }

    /// Open an existing file-backed database.
    pub fn open(path: &Path, cache_bytes: u64) -> SResult<Pager> {
        let f = PosFile::open_rw(path)?;
        let mut best: Option<(u64, Vec<u8>)> = None;
        // The page size is in the superblock; read with the largest page
        // size and trim, since a superblock is at the front of its page.
        let mut probe = vec![0u8; page::MIN_PAGE];
        f.read_exact_at(&mut probe, 0).map_err(|_| {
            SError::Corrupt("file is too short to be a paged glider database".into())
        })?;
        if &probe[32..40] != MAGIC {
            return corrupt("not a paged glider database (bad magic)");
        }
        let ps = page::get_u32(&probe, 44) as usize;
        if !page::valid_page_size(ps) {
            return corrupt(format!("bad page size {ps}"));
        }
        for slot in 0..2u64 {
            let mut b = new_page(ps);
            if f.read_exact_at(&mut b, slot * ps as u64).is_err() {
                continue;
            }
            if page::check(&b, slot).unwrap_or(false)
                && page::kind(&b) == page::KIND_SUPER
                && &b[32..40] == MAGIC
                && best
                    .as_ref()
                    .map(|(_, x)| sb_epoch(&b) > sb_epoch(x))
                    .unwrap_or(true)
            {
                best = Some((slot, b));
            }
        }
        let Some((sb_slot, sb)) = best else {
            return corrupt("no valid superblock");
        };
        if page::get_u32(&sb, 40) != FORMAT {
            return corrupt(format!("unsupported format {}", page::get_u32(&sb, 40)));
        }
        let durable = sb_epoch(&sb);
        let mut generation = [0u8; 16];
        generation.copy_from_slice(&sb[56..72]);
        let wal_lsn = page::get_u64(&sb, 72);
        let hw = page::get_u64(&sb, 80);
        let free_head = page::get_u64(&sb, 88);
        let pin_seen = page::get_u64(&sb, SB_PIN);
        let hold = page::get_u64(&sb, SB_HOLD) != 0;
        let seg_pages = page::get_u64(&sb, 104);
        let mut roots = [0u64; ROOTS];
        let mut meta = [0u64; METAS];
        for (i, r) in roots.iter_mut().enumerate() {
            *r = page::get_u64(&sb, 112 + 8 * i);
        }
        for (i, m) in meta.iter_mut().enumerate() {
            *m = page::get_u64(&sb, 112 + 8 * ROOTS + 8 * i);
        }
        let p = Pager {
            ps,
            inner: Arc::new(Mutex::new(Inner {
                ps,
                store: Store::File(FileStore {
                    base: path.to_path_buf(),
                    seg_pages,
                    segs: vec![Some(f)],
                    cache: Cache::new(cache_bytes, ps),
                    touched: vec![false],
                }),
                epoch: durable + 1,
                durable,
                hw,
                free: Vec::new(),
                pending: Vec::new(),
                chain: Vec::new(),
                held: Vec::new(),
                hold: false,
                pin_seen: 0,
                txn_alloc: Vec::new(),
                txn_free: Vec::new(),
                roots,
                committed_roots: roots,
                meta,
                committed_meta: meta,
                generation,
                wal_lsn,
                sb_slot,
                stats: Stats::default(),
            })),
        };
        // Load the free list.
        {
            let mut g = p.lock();
            g.pin_seen = pin_seen;
            g.hold = hold;
            let mut next = free_head;
            let mut guard = 0u64;
            while next != 0 {
                guard += 1;
                if guard > hw {
                    return corrupt("free-list chain loops");
                }
                let pg = g.read_page(next)?;
                if page::kind(&pg) != page::KIND_FREELIST {
                    return corrupt(format!("page {next} is not a free-list page"));
                }
                g.chain.push(next);
                let n = page::get_u32(&pg, 40) as usize;
                if 48 + n * 8 > pg.len() {
                    return corrupt("free-list page overflows");
                }
                for i in 0..n {
                    let entry = page::get_u64(&pg, 48 + 8 * i);
                    let pno = entry & !HELD;
                    if pno < 2 || pno >= hw {
                        return corrupt(format!("free list names page {pno}"));
                    }
                    if entry & HELD != 0 {
                        g.held.push(pno);
                    } else {
                        g.free.push(pno);
                    }
                }
                next = page::get_u64(&pg, 32);
            }
        }
        register_cache(&p);
        Ok(p)
    }

    /// A `:memory:` pager holding the pages of a database file's bytes, as of
    /// its last checkpoint. For hosts without a filesystem (wasm). Only a
    /// single-file database (no segment files) can be loaded this way.
    pub fn from_image(bytes: &[u8], max_memory: u64) -> SResult<Pager> {
        if bytes.len() < page::MIN_PAGE || &bytes[32..40] != MAGIC {
            return corrupt("not a paged glider database");
        }
        let ps = page::get_u32(bytes, 44) as usize;
        if !page::valid_page_size(ps) || bytes.len() < 2 * ps {
            return corrupt("bad page size or truncated file");
        }
        let mut best: Option<&[u8]> = None;
        for slot in 0..2usize {
            let b = &bytes[slot * ps..(slot + 1) * ps];
            if page::check(b, slot as u64).unwrap_or(false)
                && &b[32..40] == MAGIC
                && best.map(|x| sb_epoch(b) > sb_epoch(x)).unwrap_or(true)
            {
                best = Some(b);
            }
        }
        let Some(sb) = best else {
            return corrupt("no valid superblock");
        };
        let hw = page::get_u64(sb, 80);
        if hw > page::get_u64(sb, 104) || (hw as usize) * ps > bytes.len() {
            return corrupt(
                "the database spans segment files or is truncated; open it from its path",
            );
        }
        let mut roots = [0u64; ROOTS];
        let mut meta = [0u64; METAS];
        for (i, r) in roots.iter_mut().enumerate() {
            *r = page::get_u64(sb, 112 + 8 * i);
        }
        for (i, m) in meta.iter_mut().enumerate() {
            *m = page::get_u64(sb, 112 + 8 * ROOTS + 8 * i);
        }
        // Free pages (the chain, and what it lists) are not loaded.
        let mut free: Vec<u64> = Vec::new();
        let mut chain: Vec<u64> = Vec::new();
        let mut next = page::get_u64(sb, 88);
        while next != 0 {
            if next >= hw || chain.len() as u64 > hw {
                return corrupt("bad free-list chain");
            }
            let pg = &bytes[next as usize * ps..(next as usize + 1) * ps];
            if !page::check(pg, next).unwrap_or(false) {
                return corrupt(format!("free-list page {next} is damaged"));
            }
            chain.push(next);
            let n = page::get_u32(pg, 40) as usize;
            for i in 0..n.min((ps - 48) / 8) {
                // Held pages are simply free in a copy.
                free.push(page::get_u64(pg, 48 + 8 * i) & !HELD);
            }
            next = page::get_u64(pg, 32);
        }
        let skip: std::collections::HashSet<u64> =
            free.iter().chain(chain.iter()).copied().collect();
        let mut pages: Vec<Option<Page>> = vec![None; hw as usize];
        let mut used = 0u64;
        for pno in 2..hw {
            if skip.contains(&pno) {
                continue;
            }
            let b = &bytes[pno as usize * ps..(pno as usize + 1) * ps];
            match page::check(b, pno) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => return corrupt(e),
            }
            pages[pno as usize] = Some(Arc::from(b));
            used += ps as u64;
        }
        free.extend(chain);
        let durable = sb_epoch(sb);
        let inner = Inner {
            ps,
            store: Store::Mem(Mem {
                pages,
                used,
                max: max_memory,
            }),
            epoch: durable + 1,
            durable: 0,
            hw,
            free,
            pending: Vec::new(),
            chain: Vec::new(),
            held: Vec::new(),
            hold: false,
            pin_seen: 0,
            txn_alloc: Vec::new(),
            txn_free: Vec::new(),
            roots,
            committed_roots: roots,
            meta,
            committed_meta: meta,
            generation: [0; 16],
            wal_lsn: 0,
            sb_slot: 0,
            stats: Stats::default(),
        };
        Ok(Pager {
            ps,
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ------------------------------------------------------------ pages

    pub fn page_size(&self) -> usize {
        self.ps
    }

    pub fn is_memory(&self) -> bool {
        matches!(self.lock().store, Store::Mem(_))
    }

    /// The current transaction's epoch. Pages carrying it are writable in
    /// place.
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Read a page.
    pub fn get(&self, pno: u64) -> SResult<Page> {
        self.lock().get(pno)
    }

    /// A new, empty page of `kind` owned by `tree`, in the current epoch.
    pub fn alloc(&self, kind: u8, tree: u32) -> SResult<u64> {
        self.lock().alloc(kind, tree)
    }

    /// Make `pno` writable in this transaction: the same page if it already
    /// belongs to this epoch, otherwise a copy at a new page number (the old
    /// one is freed on commit). Returns the page number to write to.
    pub fn writable(&self, pno: u64) -> SResult<u64> {
        let mut g = self.lock();
        let pg = g.get(pno)?;
        let epoch = g.epoch;
        if page::epoch(&pg) == epoch {
            return Ok(pno);
        }
        let new = g.alloc_pno()?;
        let mut b = pg.to_vec();
        page::rehome(&mut b, new, epoch);
        g.install(new, b.into(), true)?;
        // Hot follows the data: the copy takes the page's heat and place.
        if let Store::File(f) = &mut g.store {
            if let (Some(h), Some(&a), Some(&b)) = (
                f.cache.hot.as_deref_mut(),
                f.cache.map.get(&pno),
                f.cache.map.get(&new),
            ) {
                h.inherit(a, b);
            }
        }
        g.txn_free.push((pno, page::epoch(&pg)));
        g.stats.copies += 1;
        Ok(new)
    }

    /// Modify a page of the current epoch in place.
    pub fn modify<R>(&self, pno: u64, f: impl FnOnce(&mut [u8]) -> R) -> SResult<R> {
        self.lock().modify(pno, f)
    }

    /// Free a page (on commit).
    pub fn free(&self, pno: u64) -> SResult<()> {
        let mut g = self.lock();
        let born = page::epoch(&g.get(pno)?);
        g.txn_free.push((pno, born));
        Ok(())
    }

    // ------------------------------------------------------------ roots

    pub fn root(&self, i: usize) -> u64 {
        self.lock().roots[i]
    }
    pub fn set_root(&self, i: usize, pno: u64) {
        self.lock().roots[i] = pno;
    }
    pub fn meta(&self, i: usize) -> u64 {
        self.lock().meta[i]
    }
    pub fn set_meta(&self, i: usize, v: u64) {
        self.lock().meta[i] = v;
    }
    pub fn generation(&self) -> [u8; 16] {
        self.lock().generation
    }
    /// WAL position the last checkpoint covers: replay starts here.
    pub fn wal_lsn(&self) -> u64 {
        self.lock().wal_lsn
    }

    // ------------------------------------------------------ transactions

    /// Whether the open transaction has changed anything.
    pub fn in_txn(&self) -> bool {
        let g = self.lock();
        !g.txn_alloc.is_empty()
            || !g.txn_free.is_empty()
            || g.roots != g.committed_roots
            || g.meta != g.committed_meta
    }

    /// Make the open transaction the committed state.
    pub fn commit(&self) {
        let mut g = self.lock();
        let durable = g.durable;
        let frees = std::mem::take(&mut g.txn_free);
        for (pno, born) in frees {
            g.release(pno, born > durable);
        }
        g.txn_alloc.clear();
        g.committed_roots = g.roots;
        g.committed_meta = g.meta;
        g.epoch += 1;
        g.stats.commits += 1;
    }

    /// Abandon the open transaction: every page it allocated is dropped, the
    /// committed roots and counters come back.
    pub fn rollback(&self) {
        let mut g = self.lock();
        let allocs = std::mem::take(&mut g.txn_alloc);
        for pno in allocs {
            g.release(pno, true);
        }
        g.txn_free.clear();
        g.roots = g.committed_roots;
        g.meta = g.committed_meta;
        g.stats.rollbacks += 1;
    }

    /// Make the committed state durable (file-backed only; a no-op in
    /// memory). Must be called between transactions. `wal_lsn` is where log
    /// replay should start after this checkpoint.
    pub fn checkpoint(&self, wal_lsn: u64) -> SResult<()> {
        let mut g = self.lock();
        if matches!(g.store, Store::Mem(_)) {
            return Ok(());
        }
        if !g.txn_alloc.is_empty() || !g.txn_free.is_empty() || g.roots != g.committed_roots {
            return corrupt("checkpoint inside an open transaction");
        }
        g.checkpoint(wal_lsn)
    }

    pub fn stats(&self) -> Stats {
        let g = self.lock();
        let mut s = g.stats;
        s.allocated_pages =
            g.hw.saturating_sub(2 + (g.free.len() + g.pending.len() + g.held.len()) as u64);
        s.resident_pages = match &g.store {
            Store::Mem(m) => m.used / g.ps as u64,
            Store::File(f) => f.cache.frames.len() as u64,
        };
        if let Store::File(f) = &g.store {
            if let Some(h) = f.cache.hot.as_deref() {
                s.hot_capacity = h.capacity() as u64;
                s.hot_pages = h.len() as u64;
                s.hot_hits = h.hits;
                s.promotions = h.promotions;
                s.demotions = h.demotions;
                s.prewarmed = h.prewarmed;
            }
        }
        s
    }

    /// Keep up to `bytes` of a file-backed database's cache for pages used
    /// often, never more than half of it (see [`super::hot`]); 0 turns it
    /// off. Shrinking it lets the coolest pages go. A `:memory:` database
    /// has every page in memory already and ignores it.
    pub fn set_hot_size(&self, bytes: u64) {
        let mut g = self.lock();
        let ps = g.ps as u64;
        let Store::File(f) = &mut g.store else {
            return;
        };
        let cap = usize::try_from(bytes / ps)
            .unwrap_or(usize::MAX)
            .min(f.cache.cap / 2);
        match f.cache.hot.as_deref_mut() {
            Some(h) => h.resize(cap),
            None if cap > 0 => {
                let h = super::hot::Hot::new(cap, f.cache.cap, f.cache.frames.len());
                f.cache.hot = Some(Box::new(h));
            }
            None => {}
        }
    }

    /// Ticks of `lookups` lookups instead of the cache's own length, from
    /// the next lookup on (see [`super::hot`]). For tests.
    #[doc(hidden)]
    pub fn set_hot_tick(&self, lookups: u64) {
        if let Store::File(f) = &mut self.lock().store {
            if let Some(h) = f.cache.hot.as_deref_mut() {
                h.set_tick(lookups);
            }
        }
    }

    /// The page numbers in the hot set, hottest first: what a later open
    /// would want back.
    pub fn hot_pages(&self) -> Vec<u64> {
        let g = self.lock();
        let Store::File(f) = &g.store else {
            return Vec::new();
        };
        match f.cache.hot.as_deref() {
            Some(h) => h
                .frames()
                .into_iter()
                .filter_map(|i| f.cache.frames.get(i).map(|fr| fr.pno))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Make a page that is in the cache hot now, with the heat of a page
    /// just promoted: for loading a hot set remembered from before. Read the
    /// page first. False if it is not in the cache, there is no hot set, or
    /// the set is full of pages at least as hot.
    pub fn mark_hot(&self, pno: u64) -> bool {
        let mut g = self.lock();
        let Store::File(f) = &mut g.store else {
            return false;
        };
        let Some(&i) = f.cache.map.get(&pno) else {
            return false;
        };
        f.cache.hot.as_deref_mut().is_some_and(|h| h.mark(i))
    }

    /// The epoch of the checkpoint in force: the one a crash would reopen.
    pub fn durable_epoch(&self) -> u64 {
        self.lock().durable
    }

    /// The epoch the next checkpoint will write: the committed state's.
    pub fn committed_epoch(&self) -> u64 {
        self.lock().epoch - 1
    }

    /// The hot set as a list to keep beside the database (see
    /// [`super::hotlist`]), or None without one, under the epoch of the
    /// checkpoint in force. Taken between transactions, every page in it is
    /// a page of the committed state.
    pub fn hot_list(&self) -> Option<super::hotlist::List> {
        let has = matches!(&self.lock().store, Store::File(f) if f.cache.hot.is_some());
        if !has {
            return None;
        }
        let pages = self.hot_pages();
        let g = self.lock();
        Some(super::hotlist::List {
            page_size: g.ps as u32,
            generation: g.generation,
            epoch: g.durable,
            pages,
        })
    }

    /// Load pages remembered as hot by a list taken at checkpoint `as_of`,
    /// hottest first, and make them hot; returns how many were. At most as
    /// many as the hot set has room for are looked at, so at most that many
    /// pages are read. A page is skipped, unread, when it is not a page of
    /// the database now (past the high-water mark or in a free list), and
    /// after reading when its epoch is after `as_of` (its number holds other
    /// data now). Loading takes no hot frame, of this database or another,
    /// and no frame the process's budget does not have: it stops instead.
    /// Call it at open, before the first query.
    pub fn prewarm(&self, pnos: &[u64], as_of: u64) -> u64 {
        let mut g = self.lock();
        let g = &mut *g;
        let Store::File(f) = &mut g.store else {
            return 0;
        };
        let Some(h) = f.cache.hot.as_deref() else {
            return 0;
        };
        let room = h.capacity().saturating_sub(h.len());
        let mut want: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for &pno in pnos {
            if want.len() >= room {
                break;
            }
            if (2..g.hw).contains(&pno) {
                want.insert(pno);
            }
        }
        for pno in g
            .free
            .iter()
            .chain(&g.pending)
            .chain(&g.held)
            .chain(&g.chain)
        {
            want.remove(pno);
        }
        let mut want: Vec<u64> = want.into_iter().collect();
        // Page order: sequential reads where the pages are contiguous.
        want.sort_unstable();
        // Frames that may be evicted for a page: cold, and held by nobody.
        // At open nothing holds a page; readers can only let go of more.
        let mut spare = f
            .cache
            .frames
            .iter()
            .enumerate()
            .filter(|(i, fr)| !h.is_hot(*i) && Arc::strong_count(&fr.data) == 1)
            .count();
        let mut made = 0u64;
        for pno in want {
            let Store::File(f) = &mut g.store else {
                break;
            };
            let at = match f.cache.map.get(&pno) {
                Some(&i) if page::epoch(&f.cache.frames[i].data) <= as_of => Some(i),
                Some(_) => continue,
                None => None,
            };
            let i = match at {
                Some(i) => i,
                None => {
                    let Ok(b) = g.read_page(pno) else {
                        continue;
                    };
                    if page::epoch(&b) > as_of {
                        continue;
                    }
                    let Store::File(f) = &mut g.store else {
                        break;
                    };
                    if f.cache.frames.len() < f.cache.cap && f.cache.reserve_frame() {
                        f.cache.push(pno, b.into(), false)
                    } else if spare > 0 {
                        // The sweep takes a cold frame; one exists.
                        spare -= 1;
                        match g.cache_insert(pno, b.into(), false) {
                            Ok(i) => i,
                            Err(_) => break,
                        }
                    } else {
                        break;
                    }
                }
            };
            let Store::File(f) = &mut g.store else {
                break;
            };
            let Some(h) = f.cache.hot.as_deref_mut() else {
                break;
            };
            if !h.mark(i) {
                break;
            }
            if at.is_some() {
                // A cold frame of its own became hot: one fewer to evict.
                spare = spare.saturating_sub(1);
            }
            h.prewarmed += 1;
            made += 1;
        }
        made
    }

    /// The hot set's bookkeeping holds together, and every hot page is a
    /// page of the database: resident, below the high-water mark, and in
    /// no free list. For tests; call it between transactions.
    #[doc(hidden)]
    pub fn check_hot(&self) -> Result<(), String> {
        let g = self.lock();
        let Store::File(f) = &g.store else {
            return Ok(());
        };
        let Some(h) = f.cache.hot.as_deref() else {
            return Ok(());
        };
        h.check(f.cache.frames.len())?;
        if h.capacity() > f.cache.cap / 2 {
            return Err(format!(
                "room for {} hot pages in a {}-frame cache",
                h.capacity(),
                f.cache.cap
            ));
        }
        let gone: std::collections::HashSet<u64> = g
            .free
            .iter()
            .chain(&g.pending)
            .chain(&g.held)
            .chain(&g.chain)
            .copied()
            .collect();
        for i in h.frames() {
            let pno = f.cache.frames[i].pno;
            if f.cache.map.get(&pno) != Some(&i) {
                return Err(format!("hot page {pno} is not where the cache says"));
            }
            if pno < 2 || pno >= g.hw || gone.contains(&pno) {
                return Err(format!("hot page {pno} is not a page of the database"));
            }
        }
        Ok(())
    }

    /// Bytes of pages held for a `:memory:` database; its limit.
    pub fn memory_usage(&self) -> Option<(u64, u64)> {
        match &self.lock().store {
            Store::Mem(m) => Some((m.used, m.max)),
            Store::File(_) => None,
        }
    }

    pub fn set_max_memory(&self, max: u64) {
        if let Store::Mem(m) = &mut self.lock().store {
            m.max = max;
        }
    }

    /// Drop every clean page from the cache (file-backed), hot ones too: for
    /// tests and benchmarks of cold reads. Dirty pages are written first.
    pub fn evict_all(&self) -> SResult<()> {
        let mut g = self.lock();
        g.flush_dirty()?;
        if let Store::File(f) = &mut g.store {
            f.cache.clear();
        }
        Ok(())
    }

    /// Pages ever handed out, including free ones: the file's extent.
    pub fn high_water(&self) -> u64 {
        self.lock().hw
    }

    pub fn free_pages(&self) -> u64 {
        let g = self.lock();
        (g.free.len() + g.pending.len() + g.held.len()) as u64
    }

    /// Ask for (or release) a backup hold, applied at the next checkpoint.
    /// `nonce` identifies the request; the superblock records it once
    /// honoured.
    pub fn set_hold(&self, hold: bool, nonce: u64) {
        let mut g = self.lock();
        g.hold = hold;
        if hold {
            g.pin_seen = nonce;
        }
    }

    /// (hold in force, nonce of the request last honoured).
    pub fn hold(&self) -> (bool, u64) {
        let g = self.lock();
        (g.hold, g.pin_seen)
    }

    /// Pages kept out of reuse by a hold.
    pub fn held_pages(&self) -> u64 {
        self.lock().held.len() as u64
    }
}

/// The superblock in force in a database file, read without opening it
/// (for replicators, which run beside the writer).
#[derive(Clone, Debug)]
pub struct SuperInfo {
    pub slot: u64,
    pub bytes: Vec<u8>,
    pub page_size: usize,
    pub epoch: u64,
    pub generation: [u8; 16],
    pub wal_lsn: u64,
    /// Pages ever handed out: the extent of the database.
    pub high_water: u64,
    pub seg_pages: u64,
    /// Nonce of the hold request last honoured, and whether it is in force.
    pub pin_seen: u64,
    pub hold: bool,
}

pub fn read_superblock(path: &Path) -> SResult<SuperInfo> {
    let f = PosFile::open(path)?;
    let mut probe = vec![0u8; page::MIN_PAGE];
    f.read_exact_at(&mut probe, 0)
        .map_err(|_| SError::Corrupt("file is too short to be a paged glider database".into()))?;
    if &probe[32..40] != MAGIC {
        return corrupt("not a paged glider database (bad magic)");
    }
    let ps = page::get_u32(&probe, 44) as usize;
    if !page::valid_page_size(ps) {
        return corrupt(format!("bad page size {ps}"));
    }
    let mut best: Option<(u64, Vec<u8>)> = None;
    for slot in 0..2u64 {
        let mut b = new_page(ps);
        if f.read_exact_at(&mut b, slot * ps as u64).is_err() {
            continue;
        }
        if page::check(&b, slot).unwrap_or(false)
            && page::kind(&b) == page::KIND_SUPER
            && &b[32..40] == MAGIC
            && best
                .as_ref()
                .map(|(_, x)| sb_epoch(&b) > sb_epoch(x))
                .unwrap_or(true)
        {
            best = Some((slot, b));
        }
    }
    let Some((slot, b)) = best else {
        return corrupt("no valid superblock");
    };
    let mut generation = [0u8; 16];
    generation.copy_from_slice(&b[56..72]);
    Ok(SuperInfo {
        slot,
        page_size: ps,
        epoch: sb_epoch(&b),
        generation,
        wal_lsn: page::get_u64(&b, 72),
        high_water: page::get_u64(&b, 80),
        seg_pages: page::get_u64(&b, 104),
        pin_seen: page::get_u64(&b, SB_PIN),
        hold: page::get_u64(&b, SB_HOLD) != 0,
        bytes: b.to_vec(),
    })
}

/// Marks a held page in the free-list chain (page numbers stay below 2^63).
const HELD: u64 = 1 << 63;
/// Superblock offsets: the hold nonce last honoured, and whether a hold is
/// in force.
const SB_PIN: usize = 96;
const SB_HOLD: usize = 112 + 8 * (ROOTS + METAS);

fn sb_epoch(b: &[u8]) -> u64 {
    page::get_u64(b, 48)
}

impl Inner {
    fn get(&mut self, pno: u64) -> SResult<Page> {
        match &mut self.store {
            Store::Mem(m) => match m.pages.get(pno as usize) {
                Some(Some(p)) => {
                    self.stats.hits += 1;
                    Ok(p.clone())
                }
                _ => corrupt(format!("page {pno} does not exist")),
            },
            Store::File(f) => {
                if let Some(&i) = f.cache.map.get(&pno) {
                    let fr = &mut f.cache.frames[i];
                    fr.usage = fr.usage.saturating_add(1).min(3);
                    self.stats.hits += 1;
                    let data = fr.data.clone();
                    if let Some(h) = f.cache.hot.as_deref_mut() {
                        h.touch(i);
                    }
                    return Ok(data);
                }
                self.stats.misses += 1;
                let data: Page = self.read_page(pno)?.into();
                let i = self.cache_insert(pno, data.clone(), false)?;
                if let Store::File(f) = &mut self.store {
                    if let Some(h) = f.cache.hot.as_deref_mut() {
                        h.touch(i);
                    }
                }
                Ok(data)
            }
        }
    }

    /// Read a page from disk, checked. Does not touch the cache.
    fn read_page(&mut self, pno: u64) -> SResult<Vec<u8>> {
        let ps = self.ps;
        let Store::File(f) = &mut self.store else {
            return corrupt("read_page on a memory store");
        };
        let (file, off) = f.locate(pno, ps, false)?;
        let mut b = new_page(ps);
        file.read_exact_at(&mut b, off)
            .map_err(|e| SError::Corrupt(format!("page {pno}: {e}")))?;
        self.stats.reads += 1;
        match page::check(&b, pno) {
            Ok(true) => Ok(b),
            Ok(false) => corrupt(format!("page {pno} was never written")),
            Err(e) => corrupt(e),
        }
    }

    fn alloc_pno(&mut self) -> SResult<u64> {
        let ps = self.ps as u64;
        if let Store::Mem(m) = &self.store {
            // A transaction may use the pages it is releasing (they are
            // freed when it commits), plus a little slack: otherwise a full
            // graph could not even delete, since copy-on-write needs a new
            // page before the old one goes. Committed usage never exceeds
            // max_memory by more than the slack.
            let allowance = (self.txn_free.len() as u64 + SLACK_PAGES) * ps;
            if m.used + ps > m.max.saturating_add(allowance) {
                return Err(SError::Full(format!(
                    "graph is full: {:.1} MiB in use, max_memory is {:.1} MiB",
                    m.used as f64 / 1048576.0,
                    m.max as f64 / 1048576.0
                )));
            }
        }
        let pno = match self.free.pop() {
            Some(p) => p,
            None => {
                let p = self.hw;
                self.hw += 1;
                p
            }
        };
        self.txn_alloc.push(pno);
        Ok(pno)
    }

    fn alloc(&mut self, kind: u8, tree: u32) -> SResult<u64> {
        let pno = self.alloc_pno()?;
        let mut b = new_page(self.ps);
        page::init(&mut b, kind, pno, self.epoch, tree);
        self.install(pno, b.into(), true)?;
        Ok(pno)
    }

    /// Put page content at `pno` (in memory, or in the cache as dirty).
    fn install(&mut self, pno: u64, data: Page, dirty: bool) -> SResult<()> {
        let ps = self.ps as u64;
        match &mut self.store {
            Store::Mem(m) => {
                let i = pno as usize;
                if m.pages.len() <= i {
                    m.pages.resize(i + 1, None);
                }
                if m.pages[i].is_none() {
                    m.used += ps;
                }
                m.pages[i] = Some(data);
                Ok(())
            }
            Store::File(_) => {
                self.cache_insert(pno, data, dirty)?;
                Ok(())
            }
        }
    }

    /// Put `data` in the cache as page `pno`; returns its frame.
    fn cache_insert(&mut self, pno: u64, data: Page, dirty: bool) -> SResult<usize> {
        let ps = self.ps;
        let Store::File(f) = &mut self.store else {
            unreachable!()
        };
        if let Some(&i) = f.cache.map.get(&pno) {
            let fr = &mut f.cache.frames[i];
            fr.data = data;
            fr.dirty |= dirty;
            fr.usage = 3;
            // New contents: all they share with the old is the number.
            if let Some(h) = f.cache.hot.as_deref_mut() {
                h.forget(i);
            }
            return Ok(i);
        }
        if f.cache.frames.len() < f.cache.cap && f.cache.reserve_frame() {
            return Ok(f.cache.push(pno, data, dirty));
        }
        // CLOCK: find a victim that nobody holds. Hot frames are not the
        // sweep's to take.
        let n = f.cache.frames.len();
        let mut victim = None;
        for _ in 0..(n * 4) {
            let h = f.cache.hand;
            f.cache.hand = (h + 1) % n;
            if f.cache.hot.as_deref().is_some_and(|hot| hot.is_hot(h)) {
                continue;
            }
            let fr = &mut f.cache.frames[h];
            if Arc::strong_count(&fr.data) > 1 {
                continue;
            }
            if fr.usage > 0 {
                fr.usage -= 1;
                continue;
            }
            victim = Some(h);
            break;
        }
        // Every frame nobody holds is hot: the coolest goes, as any frame
        // would have before there was a hot set, rather than nothing.
        if victim.is_none() {
            let frames = &f.cache.frames;
            victim = f.cache.hot.as_deref().and_then(|h| {
                h.coolest(|i| {
                    frames
                        .get(i)
                        .is_some_and(|fr| Arc::strong_count(&fr.data) == 1)
                })
            });
        }
        // Nothing of its own to give: room from other databases' hot frames,
        // their cold ones having gone already.
        if victim.is_none()
            && f.cache.frames.len() < f.cache.cap
            && f.cache.reserve_frame_from_hot()
        {
            return Ok(f.cache.push(pno, data, dirty));
        }
        let Some(v) = victim else {
            return Err(SError::Full(
                "page cache budget exhausted (all frames pinned or other databases busy)".into(),
            ));
        };
        let old_pno = f.cache.frames[v].pno;
        if f.cache.frames[v].dirty {
            let mut b = f.cache.frames[v].data.to_vec();
            page::seal(&mut b);
            let (file, off) = f.locate(old_pno, ps, true)?;
            file.write_all_at(&b, off)?;
            self.stats.writes += 1;
        }
        f.cache.map.remove(&old_pno);
        f.cache.frames[v] = Frame {
            pno,
            data,
            dirty,
            usage: 1,
        };
        f.cache.map.insert(pno, v);
        if let Some(h) = f.cache.hot.as_deref_mut() {
            h.forget(v);
        }
        self.stats.evictions += 1;
        Ok(v)
    }

    fn modify<R>(&mut self, pno: u64, f: impl FnOnce(&mut [u8]) -> R) -> SResult<R> {
        let epoch = self.epoch;
        let slot: &mut Page = match &mut self.store {
            Store::Mem(m) => match m.pages.get_mut(pno as usize) {
                Some(Some(p)) => p,
                _ => return corrupt(format!("page {pno} does not exist")),
            },
            Store::File(fs) => {
                if !fs.cache.map.contains_key(&pno) {
                    // Load it (it may have been evicted after being written).
                    let data: Page = self.read_page(pno)?.into();
                    self.cache_insert(pno, data, false)?;
                }
                let Store::File(fs) = &mut self.store else {
                    unreachable!()
                };
                let i = fs.cache.map[&pno];
                let fr = &mut fs.cache.frames[i];
                fr.dirty = true;
                fr.usage = 3;
                &mut fr.data
            }
        };
        if page::epoch(slot) != epoch {
            return corrupt(format!("page {pno} modified outside its epoch"));
        }
        if Arc::get_mut(slot).is_none() {
            // A reader still holds the old bytes; give it its snapshot and
            // take a copy.
            *slot = Arc::from(&slot[..]);
        }
        let buf = Arc::get_mut(slot).expect("unique after copy");
        Ok(f(buf))
    }

    /// Return a page to circulation: now, or after the next checkpoint.
    fn release(&mut self, pno: u64, now: bool) {
        let ps = self.ps as u64;
        match &mut self.store {
            Store::Mem(m) => {
                if let Some(slot) = m.pages.get_mut(pno as usize) {
                    if slot.take().is_some() {
                        m.used -= ps;
                    }
                }
                self.free.push(pno);
            }
            Store::File(f) => {
                let at = f.cache.map.get(&pno).copied();
                if let (Some(i), Some(h)) = (at, f.cache.hot.as_deref_mut()) {
                    // Its heat goes with it: the number may come back as
                    // other data.
                    h.forget(i);
                }
                if now {
                    // Its cached content (if any) is garbage now.
                    if let Some(i) = at {
                        f.cache.frames[i].dirty = false;
                    }
                    self.free.push(pno);
                } else {
                    self.pending.push(pno);
                }
            }
        }
    }

    fn flush_dirty(&mut self) -> SResult<()> {
        let ps = self.ps;
        let Store::File(f) = &mut self.store else {
            return Ok(());
        };
        // Write in page order: sequential I/O where pages are contiguous.
        let mut dirty: Vec<usize> = (0..f.cache.frames.len())
            .filter(|i| f.cache.frames[*i].dirty)
            .collect();
        dirty.sort_by_key(|i| f.cache.frames[*i].pno);
        for i in dirty {
            let pno = f.cache.frames[i].pno;
            let mut b = f.cache.frames[i].data.to_vec();
            page::seal(&mut b);
            let (file, off) = f.locate(pno, ps, true)?;
            file.write_all_at(&b, off)?;
            f.cache.frames[i].dirty = false;
            self.stats.writes += 1;
        }
        Ok(())
    }

    fn sync_all(&mut self) -> SResult<()> {
        let Store::File(f) = &mut self.store else {
            return Ok(());
        };
        for (i, seg) in f.segs.iter().enumerate() {
            if let (Some(s), true) = (seg, f.touched.get(i).copied().unwrap_or(false)) {
                s.sync_data()?;
            }
        }
        for t in f.touched.iter_mut() {
            *t = false;
        }
        Ok(())
    }

    fn write_superblock(&mut self, slot: u64) -> SResult<()> {
        let ps = self.ps;
        let mut b = new_page(ps);
        page::init(&mut b, page::KIND_SUPER, slot, 0, 0);
        b[32..40].copy_from_slice(MAGIC);
        page::put_u32(&mut b, 40, FORMAT);
        page::put_u32(&mut b, 44, ps as u32);
        page::put_u64(&mut b, 48, self.durable);
        b[56..72].copy_from_slice(&self.generation);
        page::put_u64(&mut b, 72, self.wal_lsn);
        page::put_u64(&mut b, 80, self.hw);
        page::put_u64(&mut b, 88, self.chain.first().copied().unwrap_or(0));
        page::put_u64(&mut b, SB_PIN, self.pin_seen);
        page::put_u64(&mut b, SB_HOLD, self.hold as u64);
        let Store::File(f) = &mut self.store else {
            return Ok(());
        };
        page::put_u64(&mut b, 104, f.seg_pages);
        for (i, r) in self.committed_roots.iter().enumerate() {
            page::put_u64(&mut b, 112 + 8 * i, *r);
        }
        for (i, m) in self.committed_meta.iter().enumerate() {
            page::put_u64(&mut b, 112 + 8 * ROOTS + 8 * i, *m);
        }
        page::seal(&mut b);
        let (file, off) = f.locate(slot, ps, true)?;
        file.write_all_at(&b, off)?;
        self.stats.writes += 1;
        Ok(())
    }

    fn checkpoint(&mut self, wal_lsn: u64) -> SResult<()> {
        let ps = self.ps;
        // 1. Everything dirty goes to disk. All of it is newer than the
        //    durable state, so none of it overwrites anything that state uses.
        self.flush_dirty()?;

        // 2. The new free list: what is free now, what was waiting for this
        //    checkpoint, and the old chain's own pages. Its pages come out of
        //    `free` (never referenced by the durable state), so a crash before
        //    the superblock lands leaves the old chain intact.
        //    Under a hold, what the durable state used (pending, the old
        //    chain) is held rather than freed; without one, held pages are
        //    released.
        let mut held: Vec<u64> = self.held.clone();
        let mut list: Vec<u64> = Vec::new();
        list.extend_from_slice(&self.free);
        if self.hold {
            held.extend_from_slice(&self.pending);
            held.extend_from_slice(&self.chain);
        } else {
            list.extend_from_slice(&self.pending);
            list.extend_from_slice(&self.chain);
            list.append(&mut held);
        }
        list.sort_unstable();
        list.dedup();
        held.sort_unstable();
        held.dedup();
        let cap = (ps - 48) / 8;
        let mut chain: Vec<u64> = Vec::new();
        // Take chain pages from the list itself, from pages that were
        // already free (not pending, not old chain).
        let reusable_now: std::collections::HashSet<u64> = {
            let mut s: std::collections::HashSet<u64> = list.iter().copied().collect();
            for p in self
                .pending
                .iter()
                .chain(self.chain.iter())
                .chain(self.held.iter())
            {
                s.remove(p);
            }
            s
        };
        loop {
            // Pages picked for the chain have left `list`, so `list` is
            // exactly what the chain must hold.
            let need = (list.len() + held.len()).div_ceil(cap);
            if chain.len() >= need {
                break;
            }
            // Prefer a page that is free right now; otherwise extend the file.
            let pick = list
                .iter()
                .rposition(|p| reusable_now.contains(p) && !chain.contains(p));
            let pno = match pick {
                Some(i) => list.remove(i),
                None => {
                    let p = self.hw;
                    self.hw += 1;
                    p
                }
            };
            chain.push(pno);
        }
        chain.sort_unstable();
        let mut rest = list.iter().copied().chain(held.iter().map(|p| p | HELD));
        for (i, pno) in chain.iter().enumerate() {
            let mut b = new_page(ps);
            page::init(&mut b, page::KIND_FREELIST, *pno, self.epoch - 1, 0);
            let next = chain.get(i + 1).copied().unwrap_or(0);
            page::put_u64(&mut b, 32, next);
            let mut n = 0usize;
            for p in rest.by_ref() {
                page::put_u64(&mut b, 48 + 8 * n, p);
                n += 1;
                if n == cap {
                    break;
                }
            }
            page::put_u32(&mut b, 40, n as u32);
            page::seal(&mut b);
            let Store::File(f) = &mut self.store else {
                unreachable!()
            };
            let (file, off) = f.locate(*pno, ps, true)?;
            file.write_all_at(&b, off)?;
            // Never serve a stale cached copy of a page now in the chain.
            if let Some(&i) = f.cache.map.get(pno) {
                f.cache.remove(i);
                f.cache.hand = 0;
            }
            self.stats.writes += 1;
        }
        debug_assert!(rest.next().is_none());

        // 3. Data durable, then the superblock that names it.
        self.sync_all()?;
        let old_durable = self.durable;
        self.durable = self.epoch - 1;
        self.wal_lsn = wal_lsn;
        let old_chain = std::mem::replace(&mut self.chain, chain);
        // Always the slot not in force: a torn write leaves the other intact.
        let slot = 1 - self.sb_slot;
        if let Err(e) = self.write_superblock(slot).and_then(|_| self.sync_all()) {
            self.durable = old_durable;
            self.chain = old_chain;
            return Err(e);
        }
        self.sb_slot = slot;
        // 4. The new free list is live.
        self.pending.clear();
        self.free = list;
        self.held = held;
        self.stats.checkpoints += 1;
        Ok(())
    }
}

impl FileStore {
    /// The file and byte offset holding `pno`, opening or creating the
    /// segment file as needed.
    fn locate(&mut self, pno: u64, ps: usize, write: bool) -> SResult<(&PosFile, u64)> {
        let seg = (pno / self.seg_pages) as usize;
        let off = (pno % self.seg_pages) * ps as u64;
        if self.segs.len() <= seg {
            self.segs.resize_with(seg + 1, || None);
            self.touched.resize(seg + 1, false);
        }
        if self.segs[seg].is_none() {
            let path = segment_path(&self.base, seg);
            if seg > 0 {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
            }
            self.segs[seg] = Some(PosFile::open_rw(&path)?);
        }
        if write {
            self.touched[seg] = true;
        }
        Ok((self.segs[seg].as_ref().expect("opened"), off))
    }
}

/// Segment 0 is the database file itself; the rest live in `<db>-data/`.
pub fn segment_path(base: &Path, seg: usize) -> PathBuf {
    if seg == 0 {
        return base.to_path_buf();
    }
    let mut dir = base.as_os_str().to_os_string();
    dir.push("-data");
    PathBuf::from(dir).join(format!("{seg:08}.seg"))
}

impl Cache {
    fn new(bytes: u64, ps: usize) -> Cache {
        let cap = ((bytes / ps as u64) as usize).max(16);
        Cache {
            cap,
            frames: Vec::new(),
            map: HashMap::default(),
            hand: 0,
            pool: crate::budget::process().map(|b| b.cache.clone()),
            // Payload plus conservative Arc/Vec/hash-table bookkeeping.
            charge: ps as u64 + 512,
            hot: None,
        }
    }

    /// A frame's worth of the process's pool, if it has one: free, or from
    /// other databases' cold frames.
    fn reserve_frame(&self) -> bool {
        let Some(pool) = &self.pool else { return true };
        if pool.reserve(self.charge) {
            return true;
        }
        reclaim_caches(pool, self.charge, Take::Cold);
        pool.reserve(self.charge)
    }
    /// The last resort, for a cache with nothing of its own to evict: a
    /// frame's worth from other databases' hot frames, their cold ones
    /// having gone in [`Cache::reserve_frame`]. Where no database has a hot
    /// set this takes nothing, as before there were any.
    fn reserve_frame_from_hot(&self) -> bool {
        let Some(pool) = &self.pool else { return false };
        reclaim_caches(pool, self.charge, Take::Hot);
        pool.reserve(self.charge)
    }
    fn release_frame(&self) {
        if let Some(pool) = &self.pool {
            pool.release(self.charge);
        }
    }
    fn clear(&mut self) {
        if let Some(pool) = &self.pool {
            pool.release(self.frames.len() as u64 * self.charge);
        }
        self.frames = Vec::new();
        self.map = HashMap::default();
        self.hand = 0;
        if let Some(h) = self.hot.as_deref_mut() {
            h.clear();
        }
    }
    /// A new frame for `pno`, already paid for.
    fn push(&mut self, pno: u64, data: Page, dirty: bool) -> usize {
        self.frames.push(Frame {
            pno,
            data,
            dirty,
            usage: 1,
        });
        let i = self.frames.len() - 1;
        self.map.insert(pno, i);
        if let Some(h) = self.hot.as_deref_mut() {
            h.push();
        }
        i
    }
    /// Drop frame `i`, which must be clean and unheld, and give its memory
    /// back. The last frame takes its place.
    fn remove(&mut self, i: usize) {
        if i >= self.frames.len() {
            return;
        }
        if let Some(h) = self.hot.as_deref_mut() {
            h.swap_remove(i);
        }
        let gone = self.frames.swap_remove(i);
        self.map.remove(&gone.pno);
        if let Some(moved) = self.frames.get(i) {
            self.map.insert(moved.pno, i);
        }
        if self.hand >= self.frames.len() {
            self.hand = 0;
        }
        self.release_frame();
    }
}

impl Inner {
    /// Reclaim, from an idle database: write what is dirty, then give up
    /// every cold frame. Without a hot set that is the whole cache, as it
    /// was before there was one.
    fn shed_cold(&mut self) {
        if self.flush_dirty().is_err() {
            return;
        }
        let Store::File(f) = &mut self.store else {
            return;
        };
        if f.cache.hot.is_none() {
            f.cache.clear();
            return;
        }
        let mut i = 0;
        while i < f.cache.frames.len() {
            if f.cache.hot.as_deref().is_some_and(|h| h.is_hot(i)) {
                i += 1;
            } else {
                f.cache.remove(i);
            }
        }
    }

    /// Reclaim, from an idle database, when cold frames were not enough:
    /// hot frames, coolest first, until the pool has room for `needed`.
    fn shed_hot(&mut self, pool: &crate::budget::Pool, needed: u64) {
        if pool.used().saturating_add(needed) <= pool.limit() || self.flush_dirty().is_err() {
            return;
        }
        let Store::File(f) = &mut self.store else {
            return;
        };
        while pool.used().saturating_add(needed) > pool.limit() {
            let Some(i) = f.cache.hot.as_deref().and_then(|h| h.coolest(|_| true)) else {
                return;
            };
            f.cache.remove(i);
        }
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        if let Some(pool) = &self.pool {
            pool.release(self.frames.len() as u64 * self.charge);
        }
    }
}

/// What [`reclaim_caches`] may take from idle databases.
#[derive(Clone, Copy)]
enum Take {
    /// Cold frames: every frame of a database without a hot set.
    Cold,
    /// Hot frames, coolest first.
    Hot,
    /// Cold frames, then hot ones if those were not enough.
    Both,
}

static CACHES: OnceLock<Mutex<Vec<Weak<Mutex<Inner>>>>> = OnceLock::new();

fn register_cache(pager: &Pager) {
    if crate::budget::process().is_some() {
        let mut caches = CACHES
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        caches.retain(|c| c.strong_count() > 0);
        caches.push(Arc::downgrade(&pager.inner));
    }
}

// Never wait for another pager while holding one: concurrent opens and queries
// may reclaim each other's idle frames without a lock-order cycle.
//
// Cold frames go first, from every idle database; hot frames only when those
// were not enough and `take` says to look further. A database that wants room
// to grow does not take another's hot frames: it evicts its own cold ones.
fn reclaim_caches(pool: &crate::budget::Pool, needed: u64, take: Take) {
    let Some(registry) = CACHES.get() else { return };
    let caches = registry.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let passes: &[bool] = match take {
        Take::Cold => &[false],
        Take::Hot => &[true],
        Take::Both => &[false, true],
    };
    for &take_hot in passes {
        for cache in &caches {
            if pool.used().saturating_add(needed) <= pool.limit() {
                return;
            }
            let Some(inner) = cache.upgrade() else {
                continue;
            };
            let Ok(mut inner) = inner.try_lock() else {
                continue;
            };
            let evictable = match &inner.store {
                Store::File(f) => f
                    .cache
                    .frames
                    .iter()
                    .all(|f| Arc::strong_count(&f.data) == 1),
                _ => false,
            };
            let has_hot = matches!(&inner.store, Store::File(f) if f.cache.hot.is_some());
            if !evictable || (take_hot && !has_hot) {
                continue;
            }
            if take_hot {
                // Its cold frames first, in case it was busy when the cold
                // pass came by.
                inner.shed_cold();
                inner.shed_hot(pool, needed);
            } else {
                inner.shed_cold();
            }
        }
    }
}

/// Governor hook. Busy/pinned frames are reclaimed on the next admission;
/// the returned usage makes a pending shrink observable. A limit below what
/// the hot sets hold takes hot frames too, the coolest first.
pub fn resize_shared_cache(bytes: u64) -> Option<u64> {
    let budget = crate::budget::process()?;
    budget.cache.resize(bytes.min(budget.config.cache));
    reclaim_caches(&budget.cache, 0, Take::Both);
    Some(budget.cache.used())
}

// ---------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::HEADER;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("glider-pager-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir_all(segment_path(&p, 1).parent().unwrap());
        p
    }

    fn fill(p: &Pager, pno: u64, byte: u8) {
        p.modify(pno, |b| b[HEADER..].fill(byte)).unwrap();
    }

    #[test]
    fn memory_store_enforces_its_limit_and_rolls_back() {
        let p = Pager::memory(512, 512 * 10);
        for _ in 0..10 + SLACK_PAGES {
            p.alloc(page::KIND_LEAF, 1).unwrap();
        }
        assert!(matches!(p.alloc(page::KIND_LEAF, 1), Err(SError::Full(_))));
        p.rollback();
        assert_eq!(p.memory_usage().unwrap().0, 0);
        let a = p.alloc(page::KIND_LEAF, 1).unwrap();
        p.set_root(0, a);
        p.commit();
        assert_eq!(p.memory_usage().unwrap().0, 512);
    }

    #[test]
    fn copy_on_write_keeps_the_committed_page_until_commit() {
        let p = Pager::memory(512, u64::MAX);
        let a = p.alloc(page::KIND_LEAF, 1).unwrap();
        fill(&p, a, 7);
        p.set_root(0, a);
        p.commit();
        let b = p.writable(a).unwrap();
        assert_ne!(a, b);
        fill(&p, b, 9);
        assert_eq!(p.get(a).unwrap()[HEADER], 7);
        p.rollback();
        assert_eq!(p.root(0), a);
        assert_eq!(p.get(a).unwrap()[HEADER], 7);
        // Second try, committed this time: the old page is released.
        let b = p.writable(a).unwrap();
        fill(&p, b, 9);
        p.set_root(0, b);
        p.commit();
        assert!(p.get(a).is_err());
        assert_eq!(p.get(b).unwrap()[HEADER], 9);
    }

    #[test]
    fn file_store_survives_reopen_and_evicts_under_a_tiny_cache() {
        let path = tmp("reopen");
        let cfg = Config {
            page_size: 512,
            cache_bytes: 512 * 16,
            max_memory: u64::MAX,
            segment_bytes: 512 * 64,
        };
        let mut pnos = Vec::new();
        {
            let p = Pager::create(&path, &cfg, [7; 16]).unwrap();
            for i in 0..500u64 {
                let a = p.alloc(page::KIND_LEAF, 1).unwrap();
                fill(&p, a, (i % 251) as u8);
                pnos.push(a);
            }
            p.set_root(0, pnos[0]);
            p.set_meta(0, 42);
            p.commit();
            p.checkpoint(1234).unwrap();
            assert!(p.stats().evictions > 0, "a 16-page cache must evict");
            // Uncheckpointed changes are not durable.
            p.set_meta(0, 99);
            p.commit();
        }
        let p = Pager::open(&path, 512 * 16).unwrap();
        assert_eq!(p.meta(0), 42);
        assert_eq!(p.root(0), pnos[0]);
        assert_eq!(p.wal_lsn(), 1234);
        assert_eq!(p.generation(), [7; 16]);
        for (i, a) in pnos.iter().enumerate() {
            assert_eq!(p.get(*a).unwrap()[HEADER + 5], (i % 251) as u8, "page {a}");
        }
        // 500 pages at 64 per segment: segment files exist.
        assert!(segment_path(&path, 7).exists());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(segment_path(&path, 1).parent().unwrap());
    }

    #[test]
    fn freed_pages_are_reused_only_when_the_durable_state_is_done_with_them() {
        let path = tmp("reuse");
        let cfg = Config {
            page_size: 512,
            cache_bytes: 512 * 64,
            max_memory: u64::MAX,
            segment_bytes: 1 << 30,
        };
        let p = Pager::create(&path, &cfg, [1; 16]).unwrap();
        let a = p.alloc(page::KIND_LEAF, 1).unwrap();
        p.set_root(0, a);
        p.commit();
        p.checkpoint(0).unwrap();
        // Replace `a`: its copy is new, `a` is referenced by the durable
        // superblock, so it must not be reused before the next checkpoint.
        let b = p.writable(a).unwrap();
        p.set_root(0, b);
        p.commit();
        let c = p.alloc(page::KIND_LEAF, 1).unwrap();
        assert_ne!(c, a, "a durable page was reused before a checkpoint");
        p.commit();
        p.checkpoint(0).unwrap();
        // After the checkpoint, `a` is free, and the free list survives.
        drop(p);
        let p = Pager::open(&path, 512 * 64).unwrap();
        let mut got = Vec::new();
        for _ in 0..8 {
            got.push(p.alloc(page::KIND_LEAF, 1).unwrap());
        }
        assert!(got.contains(&a), "{a} not reused; got {got:?}");
        let _ = std::fs::remove_file(&path);
    }
}
