//! Positional I/O: read or write `n` bytes at offset `o` without moving a
//! shared cursor, so one handle serves every page.
//!
//! Safe std only. Unix has `FileExt::read_exact_at` / `write_all_at`
//! (pread/pwrite), Windows has `FileExt::seek_read` / `seek_write`, and
//! anything else (wasi, and the fs-less wasm targets, where no file can be
//! opened in the first place) falls back to a seek and a read or write under
//! a lock.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub struct PosFile {
    #[cfg(any(unix, windows))]
    file: File,
    #[cfg(not(any(unix, windows)))]
    file: std::sync::Mutex<File>,
}

impl PosFile {
    /// Open read-only.
    pub fn open(path: &Path) -> io::Result<PosFile> {
        Ok(PosFile::wrap(File::open(path)?))
    }

    /// Open read-write, creating the file if it does not exist.
    pub fn open_rw(path: &Path) -> io::Result<PosFile> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(PosFile::wrap(f))
    }

    fn wrap(file: File) -> PosFile {
        #[cfg(any(unix, windows))]
        return PosFile { file };
        #[cfg(not(any(unix, windows)))]
        return PosFile {
            file: std::sync::Mutex::new(file),
        };
    }

    pub fn len(&self) -> io::Result<u64> {
        #[cfg(any(unix, windows))]
        return Ok(self.file.metadata()?.len());
        #[cfg(not(any(unix, windows)))]
        return Ok(self
            .file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .metadata()?
            .len());
    }

    pub fn sync_data(&self) -> io::Result<()> {
        #[cfg(any(unix, windows))]
        return self.file.sync_data();
        #[cfg(not(any(unix, windows)))]
        return self
            .file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sync_data();
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

    pub fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.write_all_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let mut done = 0;
            while done < buf.len() {
                match self.file.seek_write(&buf[done..], offset + done as u64) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write whole buffer",
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
            use std::io::{Seek, SeekFrom, Write};
            let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
            f.seek(SeekFrom::Start(offset))?;
            f.write_all(buf)
        }
    }
}
