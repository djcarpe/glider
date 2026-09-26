//! Positional reads: read `n` bytes at offset `o` without moving a shared
//! cursor, so many readers can share one handle.
//!
//! Safe std only. Unix has `FileExt::read_exact_at` (pread), Windows has
//! `FileExt::seek_read`, and anything else (wasi, and the fs-less wasm
//! targets, where no file can be opened in the first place) falls back to a
//! seek and a read under a lock.

use std::fs::File;
use std::io;
use std::path::Path;

pub struct PosFile {
    #[cfg(any(unix, windows))]
    file: File,
    #[cfg(not(any(unix, windows)))]
    file: std::sync::Mutex<File>,
}

impl PosFile {
    pub fn open(path: &Path) -> io::Result<PosFile> {
        let file = File::open(path)?;
        #[cfg(any(unix, windows))]
        return Ok(PosFile { file });
        #[cfg(not(any(unix, windows)))]
        return Ok(PosFile {
            file: std::sync::Mutex::new(file),
        });
    }

    pub fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_exact_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0;
            while done < buf.len() {
                match self.file.seek_read(&mut buf[done..], offset + done as u64) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "failed to fill whole buffer",
                        ))
                    }
                    Ok(n) => done += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
            f.seek(SeekFrom::Start(offset))?;
            f.read_exact(buf)
        }
    }
}
