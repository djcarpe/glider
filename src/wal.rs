//! Continuous replication, in the Litestream mould.
//!
//! Litestream works by tailing SQLite's `-wal` file and copying frames off to
//! object storage. Glider needs no equivalent trick, because the database file
//! *is* the write-ahead log: every mutation is a CRC-framed record appended at
//! the end, and nothing is ever rewritten in place. So replication is byte
//! ranges. A replica is the file, in pieces:
//!
//! ```text
//! <dir>/<generation>/segments/0000000000000000.seg   [0, 4096)   <- includes the header
//! <dir>/<generation>/segments/0000000000001000.seg   [4096, 9000)
//! <dir>/<generation>/manifest.jsonl
//! ```
//!
//! `cat segments/*.seg > restored.gldb` is a valid restore. The tooling here
//! adds the parts that matter in practice: never shipping a half-written
//! transaction, noticing when a compaction invalidates every offset, and
//! stopping at a point in time.
//!
//! Two rules make the whole thing safe:
//!
//! 1. **Only ship up to a transaction boundary.** [`store::scan_committed_end`]
//!    walks frames, checks CRCs, and reports the offset after the last commit
//!    marker. Bytes past that belong to a transaction still in flight.
//! 2. **A compaction starts a new generation.** Compaction rewrites the file
//!    from scratch, so old offsets mean nothing. The generation id in the
//!    header changes, the tailer notices, and it begins a fresh lineage rather
//!    than appending onto a log that no longer exists.
//!
//! Glider does not talk to S3 itself — that would mean an HTTP client, TLS and
//! SigV4, and the whole point of this codebase is that it has no dependencies.
//! It writes segments to a directory and will run a command for each one, so
//! `aws s3 cp`, `mc cp`, `rclone`, `restic` or a shell script does the actual
//! shipping.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::store;

pub struct TailOptions {
    /// How often to look at the file.
    pub poll: Duration,
    /// Ship as soon as this many unshipped bytes exist.
    pub min_bytes: u64,
    /// Ship anyway after this long, however few bytes there are.
    pub max_delay: Duration,
    /// Shell command run per segment. `{path} {name} {gen} {offset} {len}`
    /// are substituted.
    pub exec: Option<String>,
    /// Ship what is there and return, instead of looping. For cron.
    pub once: bool,
    pub quiet: bool,
}

impl Default for TailOptions {
    fn default() -> Self {
        TailOptions {
            poll: Duration::from_secs(1),
            min_bytes: 1 << 20,
            max_delay: Duration::from_secs(10),
            exec: None,
            once: false,
            quiet: false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub offset: u64,
    pub len: u64,
    pub ts: u64,
}

impl Segment {
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }
}

// ------------------------------------------------------------------- tailing

/// Follow a database file and copy new committed bytes into `dir`.
///
/// Safe to run in a separate process from the writer — it opens the file
/// read-only and never assumes it is the only reader.
pub fn tail(db: &Path, dir: &Path, opts: &TailOptions) -> io::Result<()> {
    let mut generation: Option<String> = None;
    let mut shipped: u64 = 0;
    let mut last_ship = SystemTime::now();

    loop {
        let header = match store::read_header(db) {
            Ok(h) => h,
            Err(e) => {
                if opts.once {
                    return Err(e);
                }
                note(opts, &format!("waiting for {}: {}", db.display(), e));
                std::thread::sleep(opts.poll);
                continue;
            }
        };

        if !header.has_generation() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "this database predates generation ids (format v1). \
                 Run `glider <db> compact` once to upgrade it, then start replicating.",
            ));
        }

        let gen = header.generation_hex();
        if generation.as_deref() != Some(gen.as_str()) {
            // First pass, or a compaction happened. Either way this is a new
            // lineage: find out how much of it we already hold.
            let existing = segments(dir, &gen)?;
            shipped = contiguous_end(&existing);
            if generation.is_some() {
                note(
                    opts,
                    &format!("generation changed to {gen} (compaction) — starting a new lineage"),
                );
            }
            fs::create_dir_all(dir.join(&gen).join("segments"))?;
            generation = Some(gen.clone());
        }

        let end = store::scan_committed_end(db, shipped.max(0))?;
        let pending = end.saturating_sub(shipped);
        let waited = last_ship.elapsed().unwrap_or_default();

        if pending > 0 && (pending >= opts.min_bytes || waited >= opts.max_delay || opts.once) {
            let seg = ship(db, dir, &gen, shipped, end)?;
            note(
                opts,
                &format!(
                    "{gen} +{} bytes at offset {} ({})",
                    seg.len,
                    seg.offset,
                    fmt_unix(seg.ts)
                ),
            );
            if let Some(cmd) = &opts.exec {
                run_hook(cmd, dir, &gen, &seg)?;
            }
            shipped = end;
            last_ship = SystemTime::now();
        }

        if opts.once {
            return Ok(());
        }
        std::thread::sleep(opts.poll);
    }
}

/// Copy `[from, to)` out of the database into a new segment file.
fn ship(db: &Path, dir: &Path, gen: &str, from: u64, to: u64) -> io::Result<Segment> {
    let len = to - from;
    let mut src = File::open(db)?;
    src.seek(SeekFrom::Start(from))?;
    let mut buf = vec![0u8; len as usize];
    src.read_exact(&mut buf)?;

    let seg_dir = dir.join(gen).join("segments");
    fs::create_dir_all(&seg_dir)?;
    // Write to a temp name and rename, so a reader never sees a half segment.
    let tmp = seg_dir.join(format!("{from:016x}.partial"));
    let final_path = seg_dir.join(format!("{from:016x}.seg"));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&buf)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &final_path)?;

    let ts = now_unix();
    let mut manifest = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(gen).join("manifest.jsonl"))?;
    writeln!(manifest, r#"{{"offset":{from},"len":{len},"ts":{ts}}}"#)?;
    manifest.sync_data()?;

    Ok(Segment {
        offset: from,
        len,
        ts,
    })
}

fn run_hook(cmd: &str, dir: &Path, gen: &str, seg: &Segment) -> io::Result<()> {
    let name = format!("{:016x}.seg", seg.offset);
    let path = dir.join(gen).join("segments").join(&name);
    let filled = cmd
        .replace("{path}", &path.to_string_lossy())
        .replace("{name}", &name)
        .replace("{gen}", gen)
        .replace("{offset}", &seg.offset.to_string())
        .replace("{len}", &seg.len.to_string());

    let status = if cfg!(windows) {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg(&filled)
            .status()
    } else {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(&filled)
            .status()
    }?;

    if !status.success() {
        return Err(io::Error::other(format!(
            "replication hook failed ({status}): {filled}"
        )));
    }
    Ok(())
}

// ------------------------------------------------------------------ inspect

/// Every generation in a replica directory, newest activity last.
pub fn generations(dir: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.len() == 32 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                out.push(name);
            }
        }
    }
    out.sort_by_key(|g| {
        segments(dir, g)
            .ok()
            .and_then(|s| s.last().map(|x| x.ts))
            .unwrap_or(0)
    });
    Ok(out)
}

/// Segments of one generation, sorted by offset, with timestamps from the
/// manifest where available and file mtime otherwise.
pub fn segments(dir: &Path, gen: &str) -> io::Result<Vec<Segment>> {
    let seg_dir = dir.join(gen).join("segments");
    let mut out: Vec<Segment> = Vec::new();
    if !seg_dir.exists() {
        return Ok(out);
    }

    let times = manifest_times(&dir.join(gen).join("manifest.jsonl"));

    for entry in fs::read_dir(&seg_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(hex) = name.strip_suffix(".seg") else {
            continue;
        };
        // Accept both `<offset>.seg` (written here) and `<offset>-<ts>.seg`
        // (written by glider-stream), so the two tools read each other's
        // replicas.
        let (hex, name_ts) = match hex.split_once('-') {
            Some((o, t)) => (o, t.parse::<u64>().ok()),
            None => (hex, None),
        };
        let Ok(offset) = u64::from_str_radix(hex, 16) else {
            continue;
        };
        let meta = entry.metadata()?;
        let ts = name_ts
            .or_else(|| times.iter().find(|(o, _)| *o == offset).map(|(_, t)| *t))
            .unwrap_or_else(|| {
                meta.modified()
                    .ok()
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            });
        out.push(Segment {
            offset,
            len: meta.len(),
            ts,
        });
    }
    out.sort_by_key(|s| s.offset);
    Ok(out)
}

fn manifest_times(path: &Path) -> Vec<(u64, u64)> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let field = |line: &str, key: &str| -> Option<u64> {
        let at = line.find(key)? + key.len();
        let rest = &line[at..];
        let digits: String = rest
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse().ok()
    };
    text.lines()
        .filter_map(|l| Some((field(l, "\"offset\"")?, field(l, "\"ts\"")?)))
        .collect()
}

/// How far an unbroken run of segments reaches from offset 0. A gap — a
/// segment that never made it to the replica — stops the count, because
/// everything after it is unusable.
pub fn contiguous_end(segs: &[Segment]) -> u64 {
    let mut end = 0u64;
    for s in segs {
        if s.offset != end {
            break;
        }
        end = s.end();
    }
    end
}

pub struct GenerationStatus {
    pub generation: String,
    pub segments: usize,
    pub bytes: u64,
    pub complete_to: u64,
    pub gap_at: Option<u64>,
    pub first_ts: u64,
    pub last_ts: u64,
}

pub fn verify(dir: &Path) -> io::Result<Vec<GenerationStatus>> {
    let mut out = Vec::new();
    for gen in generations(dir)? {
        let segs = segments(dir, &gen)?;
        let complete_to = contiguous_end(&segs);
        let gap_at = segs
            .iter()
            .find(|s| s.offset > complete_to)
            .map(|_| complete_to);
        out.push(GenerationStatus {
            generation: gen,
            segments: segs.len(),
            bytes: segs.iter().map(|s| s.len).sum(),
            complete_to,
            gap_at,
            first_ts: segs.first().map(|s| s.ts).unwrap_or(0),
            last_ts: segs.last().map(|s| s.ts).unwrap_or(0),
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------ restore

pub struct RestoreReport {
    pub generation: String,
    pub segments: usize,
    pub bytes: u64,
    pub through: u64,
}

/// Rebuild a database file from a replica directory.
///
/// With no `as_of`, restores everything. With one, stops at the last segment
/// shipped at or before that unix timestamp — so recovery granularity is the
/// segment interval, not the transaction.
pub fn restore(
    dir: &Path,
    generation: Option<&str>,
    as_of: Option<u64>,
    out_path: &Path,
) -> io::Result<RestoreReport> {
    let gen = match generation {
        Some(g) => g.to_string(),
        None => generations(dir)?
            .pop()
            .ok_or_else(|| io::Error::other(format!("no generations in {}", dir.display())))?,
    };

    let segs = segments(dir, &gen)?;
    if segs.is_empty() {
        return Err(io::Error::other(format!(
            "generation {gen} has no segments"
        )));
    }
    if segs[0].offset != 0 {
        return Err(io::Error::other(format!(
            "generation {gen} is missing its first segment — cannot restore from the middle of a log"
        )));
    }

    let mut out = File::create(out_path)?;
    let mut end = 0u64;
    let mut used = 0usize;

    for seg in &segs {
        if seg.offset != end {
            return Err(io::Error::other(format!(
                "gap in generation {gen}: have bytes up to {end}, next segment starts at {}",
                seg.offset
            )));
        }
        if let Some(limit) = as_of {
            if seg.ts > limit {
                break;
            }
        }
        let path = dir
            .join(&gen)
            .join("segments")
            .join(format!("{:016x}.seg", seg.offset));
        let mut f = File::open(&path)?;
        io::copy(&mut f, &mut out)?;
        end = seg.end();
        used += 1;
    }

    out.sync_all()?;
    Ok(RestoreReport {
        generation: gen,
        segments: used,
        bytes: end,
        through: end,
    })
}

// -------------------------------------------------------------------- timing

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Accepts a unix timestamp, or a relative offset like `-30m`, `-2h`, `-7d`.
pub fn parse_as_of(s: &str, now: u64) -> Option<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let rest = s.strip_prefix('-')?;
    let (digits, unit) = rest.split_at(rest.len().checked_sub(1)?);
    let n: u64 = digits.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return None,
    };
    Some(now.saturating_sub(secs))
}

/// `2026-09-12 16:40:03Z`, without pulling in a date library.
pub fn fmt_unix(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let tod = secs % 86400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn note(opts: &TailOptions, msg: &str) {
    if !opts.quiet {
        eprintln!("[glider wal] {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::store::Sync;
    use crate::value::Value;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("glider-wal-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_nodes(g: &mut Graph, n: usize) {
        for i in 0..n {
            g.add_node(&["N".into()], vec![("i".into(), Value::Int(i as i64))])
                .unwrap();
        }
        g.commit().unwrap();
    }

    #[test]
    fn ships_segments_and_restores_byte_identical() {
        let dir = tmpdir("roundtrip");
        let db = dir.join("a.gldb");
        let replica = dir.join("replica");

        let mut g = Graph::open(&db, Sync::Always).unwrap();
        write_nodes(&mut g, 100);
        drop(g);

        let opts = TailOptions {
            once: true,
            min_bytes: 0,
            quiet: true,
            ..Default::default()
        };
        tail(&db, &replica, &opts).unwrap();

        // More writes, shipped as a second segment.
        let mut g = Graph::open(&db, Sync::Always).unwrap();
        write_nodes(&mut g, 50);
        drop(g);
        tail(&db, &replica, &opts).unwrap();

        let out = dir.join("restored.gldb");
        let report = restore(&replica, None, None, &out).unwrap();
        assert_eq!(report.segments, 2);
        assert_eq!(fs::read(&db).unwrap(), fs::read(&out).unwrap());

        let restored = Graph::open(&out, Sync::Normal).unwrap();
        assert_eq!(restored.node_count(), 150);
    }

    #[test]
    fn never_ships_an_uncommitted_tail() {
        let dir = tmpdir("torn");
        let db = dir.join("b.gldb");
        let replica = dir.join("replica");

        let mut g = Graph::open(&db, Sync::Always).unwrap();
        write_nodes(&mut g, 10);
        let committed = g.file_len();
        drop(g);

        // Simulate a writer caught mid-transaction: junk past the last commit.
        let mut f = OpenOptions::new().append(true).open(&db).unwrap();
        f.write_all(&[7u8; 64]).unwrap();
        f.sync_all().unwrap();

        let end = store::scan_committed_end(&db, 0).unwrap();
        assert_eq!(end, committed, "scan must stop at the last commit marker");

        tail(
            &db,
            &replica,
            &TailOptions {
                once: true,
                min_bytes: 0,
                quiet: true,
                ..Default::default()
            },
        )
        .unwrap();
        let gen = store::read_header(&db).unwrap().generation_hex();
        assert_eq!(
            contiguous_end(&segments(&replica, &gen).unwrap()),
            committed
        );
    }

    #[test]
    fn compaction_starts_a_new_generation() {
        let dir = tmpdir("gen");
        let db = dir.join("c.gldb");
        let replica = dir.join("replica");
        let opts = TailOptions {
            once: true,
            min_bytes: 0,
            quiet: true,
            ..Default::default()
        };

        let mut g = Graph::open(&db, Sync::Always).unwrap();
        write_nodes(&mut g, 40);
        drop(g);
        tail(&db, &replica, &opts).unwrap();
        let first = store::read_header(&db).unwrap().generation_hex();

        let mut g = Graph::open(&db, Sync::Always).unwrap();
        g.compact().unwrap();
        drop(g);
        let second = store::read_header(&db).unwrap().generation_hex();
        assert_ne!(first, second, "compaction must mint a new generation");

        tail(&db, &replica, &opts).unwrap();
        assert_eq!(generations(&replica).unwrap().len(), 2);

        // Both lineages restore, independently.
        let out = dir.join("from-new.gldb");
        restore(&replica, Some(&second), None, &out).unwrap();
        assert_eq!(Graph::open(&out, Sync::Normal).unwrap().node_count(), 40);

        let old = dir.join("from-old.gldb");
        restore(&replica, Some(&first), None, &old).unwrap();
        assert_eq!(Graph::open(&old, Sync::Normal).unwrap().node_count(), 40);
    }

    #[test]
    fn point_in_time_stops_where_asked() {
        let dir = tmpdir("pitr");
        let db = dir.join("d.gldb");
        let replica = dir.join("replica");
        let opts = TailOptions {
            once: true,
            min_bytes: 0,
            quiet: true,
            ..Default::default()
        };

        let mut g = Graph::open(&db, Sync::Always).unwrap();
        write_nodes(&mut g, 10);
        drop(g);
        tail(&db, &replica, &opts).unwrap();

        let cutoff = now_unix();
        std::thread::sleep(Duration::from_millis(1100));

        let mut g = Graph::open(&db, Sync::Always).unwrap();
        write_nodes(&mut g, 10);
        drop(g);
        tail(&db, &replica, &opts).unwrap();

        let out = dir.join("pitr.gldb");
        restore(&replica, None, Some(cutoff), &out).unwrap();
        assert_eq!(Graph::open(&out, Sync::Normal).unwrap().node_count(), 10);
    }

    #[test]
    fn relative_times_and_formatting() {
        assert_eq!(parse_as_of("-30m", 10_000), Some(10_000 - 1800));
        assert_eq!(parse_as_of("-2h", 10_000), Some(10_000 - 7200));
        assert_eq!(parse_as_of("1757692800", 0), Some(1_757_692_800));
        assert_eq!(parse_as_of("nonsense", 0), None);
        assert_eq!(fmt_unix(0), "1970-01-01 00:00:00Z");
        assert_eq!(fmt_unix(1_757_692_800), "2025-09-12 16:00:00Z");
    }
}
