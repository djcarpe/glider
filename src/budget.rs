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
pub fn configure(config: MemoryBudget) -> &'static ProcessBudget {
    PROCESS.get_or_init(|| ProcessBudget {
        cache: Arc::new(Pool::new(config.cache)),
        work: Arc::new(Pool::new(config.work)),
        config,
    })
}
pub fn process() -> Option<&'static ProcessBudget> {
    PROCESS.get()
}
pub fn ensure() -> &'static ProcessBudget {
    configure(MemoryBudget::detect())
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
