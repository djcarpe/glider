//! Process memory admission. The embedding application reserves the BEAM share;
//! Glider shares cache and query workspace across every database in the process.
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy, Debug)]
pub struct MemoryBudget {
    pub limit: u64,
    pub budget: u64,
    pub cache: u64,
    pub work: u64,
    pub beam: u64,
}

impl MemoryBudget {
    pub fn from_limit(limit: u64, budget: Option<u64>) -> Self {
        let budget = budget.unwrap_or(limit / 5 * 4).min(limit / 5 * 4);
        let cache = budget / 4;
        let work = budget / 8;
        Self {
            limit,
            budget,
            cache,
            work,
            beam: budget - cache - work,
        }
    }

    pub fn detect() -> Self {
        let limit = memory_limit(Path::new("/sys/fs/cgroup"), Path::new("/proc"));
        let configured = std::env::var("GLIDER_MEMORY_BUDGET_BYTES")
            .ok()
            .and_then(|s| s.parse().ok());
        Self::from_limit(limit, configured)
    }
}

/// Respect nested cgroup v2 limits, including a host-mounted hierarchy. Unlimited
/// cgroups fall back to physical RAM. The smallest ancestor limit always wins.
pub fn memory_limit(cgroup: &Path, proc: &Path) -> u64 {
    let mut limit = std::fs::read_to_string(proc.join("meminfo"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|line| {
                line.strip_prefix("MemTotal:")
                    .and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok())
                    .map(|n| n * 1024)
            })
        })
        .unwrap_or(1 << 30);
    let relative = std::fs::read_to_string(proc.join("self/cgroup"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("0::")
                    .map(|p| p.trim_start_matches('/').to_owned())
            })
        })
        .unwrap_or_default();
    let leaf = cgroup.join(relative);
    for path in leaf.ancestors().take_while(|p| p.starts_with(cgroup)) {
        if let Ok(s) = std::fs::read_to_string(path.join("memory.max")) {
            if let Ok(n) = s.trim().parse::<u64>() {
                limit = limit.min(n);
            }
        }
    }
    limit
}

/// A shared byte semaphore. Reservations are released even on query failure.
#[derive(Debug)]
pub struct Pool {
    limit: AtomicU64,
    used: AtomicU64,
}

impl Pool {
    pub fn new(limit: u64) -> Self {
        Self {
            limit: AtomicU64::new(limit),
            used: AtomicU64::new(0),
        }
    }
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Acquire)
    }
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }
    pub fn resize(&self, limit: u64) {
        self.limit.store(limit, Ordering::Release);
    }
    pub fn reserve(&self, bytes: u64) -> bool {
        let mut used = self.used();
        loop {
            let Some(next) = used.checked_add(bytes).filter(|n| *n <= self.limit()) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => used = actual,
            }
        }
    }
    pub fn release(&self, bytes: u64) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
    }
    pub fn lease(self: &Arc<Self>, bytes: u64) -> Option<Lease> {
        self.reserve(bytes).then(|| Lease {
            pool: self.clone(),
            bytes,
        })
    }
}

pub struct Lease {
    pool: Arc<Pool>,
    bytes: u64,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.pool.release(self.bytes);
    }
}

pub struct ProcessBudget {
    pub config: MemoryBudget,
    pub cache: Arc<Pool>,
    pub work: Arc<Pool>,
}

static PROCESS: OnceLock<ProcessBudget> = OnceLock::new();

/// Called before opening databases by the NIF. Standalone callers may opt in.
///
/// A process that opts in to a budget also gets an allocator that gives
/// memory back ([`tune_allocator`]).
pub fn configure(config: MemoryBudget) -> &'static ProcessBudget {
    PROCESS.get_or_init(|| {
        tune_allocator();
        ProcessBudget {
            cache: Arc::new(Pool::new(config.cache)),
            work: Arc::new(Pool::new(config.work)),
            config,
        }
    })
}

/// glibc's defaults suit a short-lived program, not a long-running process
/// with a memory limit: a freed buffer stays resident.
///
/// * The mmap threshold is dynamic: once a block of N bytes is freed, every
///   later block under N comes from a heap, where freeing it returns nothing.
///   A copy buffer or a sort run of a few MiB is enough to move it. Fixed at
///   128 KiB, a big buffer is mapped on its own and unmapped when freed.
/// * Every thread gets an arena of its own, up to eight per core: the page
///   cache's frames and every query's temporaries spread over as many heaps
///   as dirty schedulers ever ran Glider, each fragmented on its own. Two
///   are enough: a database is used by one thread at a time.
/// * A heap is given back only above 128 KiB free at its top; fixed, so
///   it does not grow with the mmap threshold either.
///
/// `GLIDER_MALLOC_TUNE=0` leaves the allocator as it is.
pub fn tune_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        if std::env::var("GLIDER_MALLOC_TUNE").is_ok_and(|v| v == "0") {
            return;
        }
        // SAFETY: mallopt only changes the allocator's tunables.
        unsafe {
            sys::mallopt(sys::M_MMAP_THRESHOLD, 128 << 10);
            sys::mallopt(sys::M_TRIM_THRESHOLD, 128 << 10);
            sys::mallopt(sys::M_ARENA_MAX, 2);
        }
    }
}

/// After a statement, a batch, a copy or a cache shrink: give the allocator's
/// free memory back to the OS when it holds more than an eighth of the
/// query workspace (at least 4 MiB) free. A no-op outside a process budget.
pub fn after_operation(force: bool) {
    if let Some(b) = process() {
        release_if_over((b.config.work / 8).max(4 << 20), force);
    }
}
pub fn process() -> Option<&'static ProcessBudget> {
    PROCESS.get()
}
pub fn ensure() -> &'static ProcessBudget {
    configure(MemoryBudget::detect())
}

/// What the C allocator holds for the process: everything Glider allocates
/// outside its pools lands here (and so does whatever else in the process
/// calls `malloc`). `None` where the allocator cannot say (not glibc).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Native {
    /// Address space the allocator has from the OS: its arenas plus large
    /// blocks it mapped on their own. Not all resident: [`release`] gives
    /// free pages inside an arena back without shrinking it.
    pub held: u64,
    /// Of those, bytes handed out and not yet freed: what a governor counts.
    pub used: u64,
    /// Freed and kept by the allocator for reuse, resident until [`release`]
    /// gives the whole pages among it back (glibc still counts those here).
    pub free: u64,
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod sys {
    // glibc's own layout of `struct mallinfo2` (glibc >= 2.33).
    #[repr(C)]
    #[derive(Default)]
    pub struct MallInfo2 {
        pub arena: usize,
        pub ordblks: usize,
        pub smblks: usize,
        pub hblks: usize,
        pub hblkhd: usize,
        pub usmblks: usize,
        pub fsmblks: usize,
        pub uordblks: usize,
        pub fordblks: usize,
        pub keepcost: usize,
    }
    pub const M_TRIM_THRESHOLD: i32 = -1;
    pub const M_MMAP_THRESHOLD: i32 = -3;
    pub const M_ARENA_MAX: i32 = -8;
    extern "C" {
        pub fn mallinfo2() -> MallInfo2;
        pub fn malloc_trim(pad: usize) -> i32;
        pub fn mallopt(param: i32, value: i32) -> i32;
    }
}

/// The C allocator's account of native memory now.
pub fn native() -> Option<Native> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallinfo2 takes no arguments and returns a plain struct.
        let m = unsafe { sys::mallinfo2() };
        Some(Native {
            held: (m.arena + m.hblkhd) as u64,
            used: (m.uordblks + m.hblkhd) as u64,
            free: m.fordblks as u64,
        })
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        None
    }
}

/// Give memory the allocator holds free back to the OS, in every arena (not
/// only at the top of each heap, where it would give it back by itself).
/// Returns the bytes no longer resident.
pub fn release() -> u64 {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let before = resident();
        // SAFETY: malloc_trim is thread-safe; it locks each arena in turn.
        unsafe { sys::malloc_trim(0) };
        let gone = before.saturating_sub(resident());
        RELEASED.fetch_add(gone, Ordering::Relaxed);
        if let Some(n) = native() {
            // glibc still counts the pages it gave back as free: what is
            // free from here on is what counts towards the next trim.
            FREE_AFTER_TRIM.store(n.free, Ordering::Relaxed);
        }
        gone
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        0
    }
}

/// The process's resident bytes (`/proc/self/statm`), 0 where unknown.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn resident() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * 4096)
}

static RELEASED: AtomicU64 = AtomicU64::new(0);
static LAST_CHECK: AtomicU64 = AtomicU64::new(0);
static FREE_AFTER_TRIM: AtomicU64 = AtomicU64::new(0);

/// Bytes [`release`] has given back to the OS since the process started.
pub fn released() -> u64 {
    RELEASED.load(Ordering::Relaxed)
}

/// After an operation that may have freed a lot (a scan that cycled the
/// page cache, a sort, a copy, a cache shrink): when the allocator has
/// freed more than `threshold` bytes since it last gave memory back, give
/// it back. Cheap enough to call after every statement: unless `force`, it
/// looks at most every 100 ms.
pub fn release_if_over(threshold: u64, force: bool) -> u64 {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let last = LAST_CHECK.load(Ordering::Relaxed);
        if !force && now.saturating_sub(last) < 100 {
            return 0;
        }
        LAST_CHECK.store(now, Ordering::Relaxed);
        let Some(n) = native() else { return 0 };
        // Pages given back and handed out again are resident once more: the
        // mark only comes down.
        let mark = FREE_AFTER_TRIM
            .fetch_min(n.free, Ordering::Relaxed)
            .min(n.free);
        if n.free.saturating_sub(mark) > threshold {
            release()
        } else {
            0
        }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = (threshold, force);
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn leases_release_and_limits_cannot_be_exceeded() {
        let pool = Arc::new(Pool::new(100));
        let lease = pool.lease(80).unwrap();
        assert!(pool.lease(21).is_none());
        pool.resize(50);
        assert!(pool.lease(1).is_none());
        drop(lease);
        assert_eq!(pool.used(), 0);
        assert!(pool.lease(50).is_some());
    }
    #[test]
    fn shares_fit_under_eighty_percent() {
        let b = MemoryBudget::from_limit(256 << 20, Some(u64::MAX));
        assert_eq!(b.cache + b.work + b.beam, b.budget);
        assert!(b.budget <= b.limit / 5 * 4);
    }
    #[test]
    fn nested_cgroup_uses_smallest_limit_and_unlimited_falls_back() {
        let dir = std::env::temp_dir().join(format!("glider-budget-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("proc/self")).unwrap();
        std::fs::create_dir_all(dir.join("cg/a/b")).unwrap();
        std::fs::write(dir.join("proc/meminfo"), "MemTotal: 1048576 kB\n").unwrap();
        std::fs::write(dir.join("proc/self/cgroup"), "0::/a/b\n").unwrap();
        std::fs::write(dir.join("cg/memory.max"), "max").unwrap();
        std::fs::write(dir.join("cg/a/memory.max"), "268435456").unwrap();
        std::fs::write(dir.join("cg/a/b/memory.max"), "536870912").unwrap();
        assert_eq!(memory_limit(&dir.join("cg"), &dir.join("proc")), 256 << 20);
        std::fs::write(dir.join("cg/a/memory.max"), "max").unwrap();
        std::fs::write(dir.join("cg/a/b/memory.max"), "max").unwrap();
        assert_eq!(memory_limit(&dir.join("cg"), &dir.join("proc")), 1 << 30);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
