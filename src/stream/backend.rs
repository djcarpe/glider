//! Where replicas live.
//!
//! Three backends, one trait. `file://` for a local disk or an NFS mount,
//! `s3://` for anything S3-compatible reachable over plaintext, and `exec:`
//! for everything else — which is how you reach real AWS over TLS, or Backblaze,
//! or a tape robot, by handing the bytes to a command that already knows how.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use super::s3::S3;

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: String,
    pub size: u64,
}

pub trait Backend: Send + Sync {
    fn put(&self, key: &str, body: Vec<u8>) -> io::Result<()>;
    fn get(&self, key: &str) -> io::Result<Vec<u8>>;
    fn delete(&self, key: &str) -> io::Result<()>;
    fn list(&self, prefix: &str) -> io::Result<Vec<Entry>>;
    fn describe(&self) -> String;
}

// ---------------------------------------------------------------- filesystem

pub struct FileBackend {
    pub root: PathBuf,
}

impl Backend for FileBackend {
    fn put(&self, key: &str, body: Vec<u8>) -> io::Result<()> {
        let path = self.root.join(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write-then-rename: a reader never sees a half object.
        let tmp = path.with_extension("partial");
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, &path)
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.root.join(key))
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        match std::fs::remove_file(self.root.join(key)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<Entry>> {
        let mut out = Vec::new();
        walk(&self.root, &self.root, prefix, &mut out)?;
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn describe(&self) -> String {
        format!("file://{}", self.root.display())
    }
}

fn walk(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<Entry>) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(root, &path, prefix, out)?;
        } else {
            let key = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if key.starts_with(prefix) && !key.ends_with(".partial") {
                out.push(Entry {
                    key,
                    size: entry.metadata()?.len(),
                });
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------ s3

pub struct S3Backend {
    pub client: S3,
}

impl Backend for S3Backend {
    fn put(&self, key: &str, body: Vec<u8>) -> io::Result<()> {
        retry(3, || self.client.put(key, body.clone()))
    }
    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        retry(3, || self.client.get(key))
    }
    fn delete(&self, key: &str) -> io::Result<()> {
        retry(3, || self.client.delete(key))
    }
    fn list(&self, prefix: &str) -> io::Result<Vec<Entry>> {
        let objects = retry(3, || self.client.list(prefix))?;
        Ok(objects
            .into_iter()
            .map(|o| Entry {
                key: o.key,
                size: o.size,
            })
            .collect())
    }
    fn describe(&self) -> String {
        format!(
            "s3://{}/{} at {}:{}",
            self.client.bucket, self.client.prefix, self.client.host, self.client.port
        )
    }
}

/// Object storage fails transiently. Three tries with a widening gap is the
/// difference between a paged engineer and a log line.
fn retry<T>(times: u32, mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempt = 0;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(e),
            Err(e) => {
                attempt += 1;
                if attempt >= times {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(200 * (1 << attempt)));
            }
        }
    }
}

// ---------------------------------------------------------------------- exec

/// Delegates every operation to a shell command. This is the escape hatch for
/// TLS, for providers with their own auth, and for anything with a CLI.
///
/// Templates get `{key}` and `{path}` (a temp file holding the body for put,
/// or where get should leave the bytes).
pub struct ExecBackend {
    pub put: String,
    pub get: String,
    pub list: String,
    pub delete: Option<String>,
    pub label: String,
}

impl ExecBackend {
    /// Sensible defaults for the AWS CLI against a bucket URL.
    pub fn aws(url: &str) -> ExecBackend {
        let base = url.trim_end_matches('/').to_string();
        ExecBackend {
            put: format!("aws s3 cp {{path}} {base}/{{key}}"),
            get: format!("aws s3 cp {base}/{{key}} {{path}}"),
            list: format!("aws s3 ls --recursive {base}/"),
            delete: Some(format!("aws s3 rm {base}/{{key}}")),
            label: base,
        }
    }

    /// Temp paths must be unique per *operation*, not per process: the daemon
    /// runs one thread per (database, replica) pair inside a single pid, so a
    /// pid-keyed name lets two concurrent uploads overwrite each other.
    fn temp(&self, what: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "glider-{what}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn run(&self, template: &str, key: &str, path: &Path) -> io::Result<Vec<u8>> {
        let cmd = template
            .replace("{key}", key)
            .replace("{path}", &path.to_string_lossy());
        let output = if cfg!(windows) {
            std::process::Command::new("cmd")
                .arg("/C")
                .arg(&cmd)
                .output()
        } else {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .output()
        }?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "`{cmd}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }
}

impl Backend for ExecBackend {
    fn put(&self, key: &str, body: Vec<u8>) -> io::Result<()> {
        let tmp = self.temp("put");
        std::fs::write(&tmp, body)?;
        let r = self.run(&self.put, key, &tmp).map(|_| ());
        let _ = std::fs::remove_file(&tmp);
        r
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        let tmp = self.temp("get");
        let _ = std::fs::remove_file(&tmp);
        self.run(&self.get, key, &tmp)?;
        let body = std::fs::read(&tmp)?;
        let _ = std::fs::remove_file(&tmp);
        Ok(body)
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        match &self.delete {
            Some(t) => self.run(t, key, Path::new("")).map(|_| ()),
            None => Ok(()),
        }
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<Entry>> {
        let out = self.run(&self.list, prefix, Path::new(""))?;
        // `aws s3 ls --recursive` prints: DATE TIME SIZE KEY
        let mut entries = Vec::new();
        for line in String::from_utf8_lossy(&out).lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                let size = parts[2].parse().unwrap_or(0);
                let key = parts[3..].join(" ");
                if key.starts_with(prefix) {
                    entries.push(Entry { key, size });
                }
            }
        }
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(entries)
    }

    fn describe(&self) -> String {
        format!("exec {}", self.label)
    }
}

// ------------------------------------------------------------------ mtime

/// Timestamps come from the key name, not from backend metadata — object
/// stores disagree about what `LastModified` means after a copy, and a key is
/// the one thing every backend preserves exactly.
pub fn file_mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
