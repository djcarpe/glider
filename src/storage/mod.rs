//! The paged storage engine.
//!
//! One engine and one page format serve both kinds of database, as SQLite
//! does:
//!
//! * `:memory:` keeps its pages in RAM. It grows until it reaches
//!   `max_memory` (by default, the machine's physical memory), and then
//!   reports [`SError::Full`] — the transaction that hit the limit is rolled
//!   back and the graph stays usable.
//! * A file-backed database keeps its pages on disk, in the database file and
//!   in segment files next to it, so it grows to the size of the disk. RAM
//!   holds a bounded page cache and bounded working memory, independent of
//!   how large the data gets.
//!
//! Layers, bottom up: [`page`] (the page header and checksums), [`pager`]
//! (page allocation, copy-on-write transactions, the memory store, the file
//! store and its cache, checkpoints), [`btree`] (copy-on-write B+trees over
//! byte keys), [`keys`] (order-preserving key encodings) and [`log`] (the
//! write-ahead log).

pub mod btree;
pub mod db;
pub mod extsort;
pub mod keys;
pub mod log;
pub mod page;
pub mod pager;

use std::fmt;
use std::io;

/// Storage errors. `Full` is recoverable: the failing transaction is rolled
/// back and the database stays consistent.
#[derive(Debug)]
pub enum SError {
    Io(io::Error),
    /// Out of room: the memory limit of a `:memory:` database, or the disk.
    Full(String),
    /// A page failed its checksum or structural checks.
    Corrupt(String),
}

pub type SResult<T> = Result<T, SError>;

impl fmt::Display for SError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SError::Io(e) => write!(f, "io error: {e}"),
            SError::Full(m) => write!(f, "{m}"),
            SError::Corrupt(m) => write!(f, "database is damaged: {m}"),
        }
    }
}

impl std::error::Error for SError {}

impl From<io::Error> for SError {
    fn from(e: io::Error) -> Self {
        if is_disk_full(&e) {
            SError::Full(format!("disk is full: {e}"))
        } else {
            SError::Io(e)
        }
    }
}

fn is_disk_full(e: &io::Error) -> bool {
    match e.raw_os_error() {
        // ENOSPC (Linux, macOS, BSDs), EDQUOT; ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL.
        #[cfg(unix)]
        Some(28) | Some(122) | Some(69) => true,
        #[cfg(windows)]
        Some(112) | Some(39) => true,
        _ => false,
    }
}

pub(crate) fn corrupt<T>(msg: impl Into<String>) -> SResult<T> {
    Err(SError::Corrupt(msg.into()))
}
