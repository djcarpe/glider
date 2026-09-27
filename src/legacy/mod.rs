//! The storage engine before pages: formats v1 and v2 (an append-only log)
//! and v3 (a snapshot image followed by a log). Kept so old files can be
//! read and migrated (`glider <db> migrate`), and as the reference the paged
//! engine is checked against in tests.

pub mod graph;
pub mod image;

use std::io::Read;
use std::path::Path;

/// If `path` is a legacy glider file, which kind.
pub fn detect(path: &Path) -> Option<&'static str> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 12];
    let n = f.read(&mut head).ok()?;
    if n < 12 {
        return None;
    }
    if &head[0..8] == b"GRAPHLT\x01" {
        return Some("format v1 (legacy)");
    }
    if &head[0..8] == b"GLIDER\x00\x01" {
        return Some(match u32::from_le_bytes([head[8], head[9], head[10], head[11]]) {
            1 => "format v1 (legacy)",
            2 => "format v2 (legacy log)",
            _ => "format v3 (legacy snapshot image)",
        });
    }
    None
}

/// Same as [`detect`], for bytes in memory.
pub fn detect_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && (&bytes[0..8] == b"GRAPHLT\x01" || &bytes[0..8] == b"GLIDER\x00\x01")
}
