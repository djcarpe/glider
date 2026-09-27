//! pagebench: the storage engine alone — pager, B+tree, log — at scale, with
//! bounded memory. The gate for building the graph engine on top of it.
//!
//!   pagebench build     --db F --gb 100 [--cache-mb 1024] [--page 16384]
//!   pagebench lookups   --db F [--n 100000] [--cache-mb 1024]
//!   pagebench scan      --db F [--cache-mb 1024]
//!   pagebench update    --db F [--txns 2000] [--per 50] [--sync normal|always|off]
//!   pagebench crashloop --db F [--rounds 50] [--sync always|normal]
//!   pagebench memfill   [--max-mb 64]
//!
//! Keys are u64 (big-endian), values pseudo-random, 40–400 bytes, a pure
//! function of (key, version) so any read can be checked. Every command
//! prints one JSON line with timings, page-cache statistics and the process's
//! peak and current RSS.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use glider::storage::btree::{Builder, Tree};
use glider::storage::db::{Db, DbConfig};
use glider::storage::log::SyncMode;
use glider::storage::pager::{self, Pager};
use glider::storage::{SError, SResult};

const T: Tree = Tree { slot: 0, id: 1 };
/// Crash-loop bookkeeping key: holds the last committed transaction number.
const COUNTER: u64 = u64::MAX;

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The value for (key, version): 40–400 bytes, first 16 bytes identify it.
fn value(key: u64, version: u64) -> Vec<u8> {
    let h = mix(key ^ mix(version));
    let len = 40 + (h % 361) as usize;
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&key.to_le_bytes());
    v.extend_from_slice(&version.to_le_bytes());
    let mut x = h;
    while v.len() < len {
        x = mix(x);
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

fn check_value(key: u64, v: &[u8]) -> Result<u64, String> {
    if v.len() < 16 {
        return Err(format!("key {key}: short value"));
    }
    let k = u64::from_le_bytes(v[0..8].try_into().unwrap());
    let ver = u64::from_le_bytes(v[8..16].try_into().unwrap());
    if k != key || value(key, ver) != v {
        return Err(format!("key {key}: value does not match version {ver}"));
    }
    Ok(ver)
}

fn status(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
        })
        .unwrap_or(0)
}

fn mem_json() -> String {
    format!(
        "\"peak_rss_mb\":{:.1},\"rss_mb\":{:.1},\"rss_anon_mb\":{:.1}",
        status("VmHWM:") as f64 / 1024.0,
        status("VmRSS:") as f64 / 1024.0,
        status("RssAnon:") as f64 / 1024.0
    )
}

fn stats_json(p: &Pager) -> String {
    let s = p.stats();
    format!(
        "\"reads\":{},\"writes\":{},\"hits\":{},\"misses\":{},\"evictions\":{},\"overflow_frames\":{},\"copies\":{},\"checkpoints\":{},\"resident_pages\":{},\"allocated_pages\":{},\"high_water\":{}",
        s.reads, s.writes, s.hits, s.misses, s.evictions, s.overflow_frames, s.copies, s.checkpoints,
        s.resident_pages, s.allocated_pages, p.high_water()
    )
}

struct Args(Vec<String>);

impl Args {
    fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().position(|a| a == name).and_then(|i| self.0.get(i + 1)).map(|s| s.as_str())
    }
    fn num(&self, name: &str, default: u64) -> u64 {
        self.get(name).and_then(|v| v.parse().ok()).unwrap_or(default)
    }
    fn f(&self, name: &str, default: f64) -> f64 {
        self.get(name).and_then(|v| v.parse().ok()).unwrap_or(default)
    }
}

fn cfg(a: &Args) -> DbConfig {
    let sync = match a.get("--sync").unwrap_or("normal") {
        "always" => SyncMode::Always,
        "off" => SyncMode::Off,
        _ => SyncMode::Normal,
    };
    DbConfig {
        pager: pager::Config {
            page_size: a.num("--page", 16384) as usize,
            cache_bytes: a.num("--cache-mb", 1024) << 20,
            max_memory: u64::MAX,
            segment_bytes: a.num("--segment-mb", 64 << 10) << 20,
        },
        sync,
        checkpoint_bytes: a.num("--checkpoint-mb", 256) << 20,
        ..DbConfig::default()
    }
}

/// Replay one logged operation: [op][key u64 le][value...].
fn apply(p: &Pager, rec: &[u8]) -> SResult<()> {
    let key = u64::from_le_bytes(rec[1..9].try_into().unwrap()).to_be_bytes();
    match rec[0] {
        b'P' => T.put(p, &key, &rec[9..]),
        b'D' => T.delete(p, &key).map(|_| ()),
        _ => Err(SError::Corrupt("unknown record".into())),
    }
}

fn put_logged(db: &mut Db, key: u64, v: &[u8]) -> SResult<()> {
    let mut rec = Vec::with_capacity(9 + v.len());
    rec.push(b'P');
    rec.extend_from_slice(&key.to_le_bytes());
    rec.extend_from_slice(v);
    db.log(&rec)?;
    T.put(db.pager(), &key.to_be_bytes(), v)
}

fn open(a: &Args) -> SResult<Db> {
    let path = PathBuf::from(a.get("--db").expect("--db"));
    Ok(Db::open(&path, &cfg(a), &mut apply)?.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a = Args(args.clone());
    let r = match args.first().map(|s| s.as_str()) {
        Some("build") => build(&a),
        Some("lookups") => lookups(&a),
        Some("scan") => scan(&a),
        Some("update") => update(&a),
        Some("crashloop") => crashloop(&a),
        Some("crashchild") => crashchild(&a),
        Some("memfill") => memfill(&a),
        _ => {
            eprintln!("usage: pagebench build|lookups|scan|update|crashloop|memfill ...");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("pagebench: {e}");
        std::process::exit(1);
    }
}

fn build(a: &Args) -> SResult<()> {
    let path = PathBuf::from(a.get("--db").expect("--db"));
    if path.exists() {
        return Err(SError::Corrupt(format!("{} exists", path.display())));
    }
    let target = (a.f("--gb", 1.0) * (1u64 << 30) as f64) as u64;
    let t0 = Instant::now();
    let (mut db, _) = Db::open(&path, &cfg(a), &mut apply)?;
    let mut b = Builder::new(db.pager(), T, 100);
    let mut n = 0u64;
    let mut bytes = 0u64;
    let every = 10_000_000u64;
    while bytes < target {
        // Keys spaced by 2 so lookups of odd keys test misses.
        let v = value(n * 2, 0);
        b.push(&(n * 2).to_be_bytes(), &v)?;
        bytes += v.len() as u64 + 14;
        n += 1;
        if n % every == 0 {
            eprintln!(
                "  {n} entries, {:.1} GiB, {:.0} s, {}",
                bytes as f64 / (1u64 << 30) as f64,
                t0.elapsed().as_secs_f64(),
                mem_json()
            );
        }
    }
    b.finish()?;
    db.commit()?;
    db.checkpoint()?;
    let secs = t0.elapsed().as_secs_f64();
    let st = T.check(db.pager()).ok();
    let file = db.pager().high_water() * db.pager().page_size() as u64;
    println!(
        "{{\"cmd\":\"build\",\"entries\":{n},\"payload_bytes\":{bytes},\"file_bytes\":{file},\"seconds\":{secs:.2},\"mb_s\":{:.1},\"height\":{},\"leaves\":{},{},{}}}",
        file as f64 / 1048576.0 / secs,
        st.map(|s| s.height).unwrap_or(0),
        st.map(|s| s.leaves).unwrap_or(0),
        stats_json(db.pager()),
        mem_json()
    );
    Ok(())
}

/// Largest key in the tree, found by probing (keys are 0, 2, 4, …).
fn key_count(p: &Pager) -> SResult<u64> {
    let (mut lo, mut hi) = (0u64, 1u64 << 40);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if T.get(p, &(mid * 2).to_be_bytes())?.is_some() {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

fn lookups(a: &Args) -> SResult<()> {
    let t_open = Instant::now();
    let db = open(a)?;
    let open_s = t_open.elapsed().as_secs_f64();
    let p = db.pager();
    let n = key_count(p)?;
    let before = p.stats();
    let count = a.num("--n", 100_000);
    let mut lat = Vec::with_capacity(count as usize);
    let mut x = 0x1234_5678u64;
    let t0 = Instant::now();
    for i in 0..count {
        x = mix(x);
        let k = x % n;
        let key = if i % 10 == 0 { k * 2 + 1 } else { k * 2 };
        let t = Instant::now();
        let got = T.get(p, &key.to_be_bytes())?;
        lat.push(t.elapsed().as_nanos() as u64);
        match (key % 2, got) {
            (0, Some(v)) => {
                check_value(key, &v).map_err(SError::Corrupt)?;
            }
            (1, None) => {}
            (_, g) => return Err(SError::Corrupt(format!("key {key}: wrong presence {}", g.is_some()))),
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    lat.sort_unstable();
    let pct = |q: f64| lat[((lat.len() as f64 - 1.0) * q) as usize] as f64 / 1000.0;
    let after = p.stats();
    println!(
        "{{\"cmd\":\"lookups\",\"entries\":{n},\"lookups\":{count},\"open_s\":{open_s:.4},\"seconds\":{secs:.2},\"per_s\":{:.0},\"p50_us\":{:.1},\"p99_us\":{:.1},\"p999_us\":{:.1},\"disk_reads_per_lookup\":{:.3},\"height\":{},{},{}}}",
        count as f64 / secs,
        pct(0.5),
        pct(0.99),
        pct(0.999),
        (after.reads - before.reads) as f64 / count as f64,
        T.check(p).map(|s| s.height).unwrap_or(0),
        stats_json(p),
        mem_json()
    );
    Ok(())
}

fn scan(a: &Args) -> SResult<()> {
    let db = open(a)?;
    let p = db.pager();
    let t0 = Instant::now();
    let mut c = T.scan(p)?;
    let mut n = 0u64;
    let mut bytes = 0u64;
    let mut last: Option<u64> = None;
    while let Some((k, v)) = c.next()? {
        let key = u64::from_be_bytes(k.try_into().unwrap());
        if last.map(|l| key <= l).unwrap_or(false) {
            return Err(SError::Corrupt("scan out of order".into()));
        }
        last = Some(key);
        let v = v.load(p)?;
        bytes += v.len() as u64 + 8;
        n += 1;
    }
    let secs = t0.elapsed().as_secs_f64();
    let file = p.high_water() * p.page_size() as u64;
    println!(
        "{{\"cmd\":\"scan\",\"entries\":{n},\"seconds\":{secs:.2},\"entries_s\":{:.0},\"payload_mb_s\":{:.1},\"file_mb_s\":{:.1},{},{}}}",
        n as f64 / secs,
        bytes as f64 / 1048576.0 / secs,
        file as f64 / 1048576.0 / secs,
        stats_json(p),
        mem_json()
    );
    Ok(())
}

fn update(a: &Args) -> SResult<()> {
    let mut db = open(a)?;
    let n = key_count(db.pager())?;
    let txns = a.num("--txns", 2000);
    let per = a.num("--per", 50);
    let mut x = 0xDEAD_BEEFu64;
    let t0 = Instant::now();
    for t in 0..txns {
        for _ in 0..per {
            x = mix(x);
            let key = (x % n) * 2;
            put_logged(&mut db, key, &value(key, 1 + t))?;
        }
        db.commit()?;
    }
    let secs = t0.elapsed().as_secs_f64();
    let ops = txns * per;
    println!(
        "{{\"cmd\":\"update\",\"txns\":{txns},\"per\":{per},\"seconds\":{secs:.2},\"txn_s\":{:.0},\"ops_s\":{:.0},\"log_since_checkpoint\":{},{},{}}}",
        txns as f64 / secs,
        ops as f64 / secs,
        db.log_since_checkpoint(),
        stats_json(db.pager()),
        mem_json()
    );
    Ok(())
}

/// Child: commit transactions forever, printing each committed number.
fn crashchild(a: &Args) -> SResult<()> {
    let mut db = open(a)?;
    let mut last = db.pager();
    let start = T
        .get(last, &COUNTER.to_be_bytes())?
        .map(|v| u64::from_le_bytes(v[..8].try_into().unwrap()))
        .unwrap_or(0);
    let _ = &mut last;
    let per = a.num("--per", 40);
    let space = a.num("--keys", 200_000);
    let mut i = start;
    loop {
        i += 1;
        for j in 0..per {
            let key = mix(i.wrapping_mul(1000).wrapping_add(j)) % space;
            put_logged(&mut db, key, &value(key, i))?;
            if j % 7 == 3 {
                let mut rec = vec![b'D'];
                let dk = mix(i ^ j) % space;
                rec.extend_from_slice(&dk.to_le_bytes());
                db.log(&rec)?;
                T.delete(db.pager(), &dk.to_be_bytes())?;
            }
        }
        let mut cv = i.to_le_bytes().to_vec();
        cv.extend_from_slice(&[0u8; 8]);
        put_logged(&mut db, COUNTER, &cv)?;
        db.commit()?;
        println!("{i}");
    }
}

fn crashloop(a: &Args) -> SResult<()> {
    let path = PathBuf::from(a.get("--db").expect("--db"));
    let rounds = a.num("--rounds", 30);
    let exe = std::env::current_exe().map_err(SError::Io)?;
    let always = a.get("--sync") == Some("always");
    let mut x = 42u64;
    let mut total_recovery = 0f64;
    let mut max_recovery = 0f64;
    for round in 0..rounds {
        let mut child = Command::new(&exe)
            .arg("crashchild")
            .args(a.0[1..].iter())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(SError::Io)?;
        let out = child.stdout.take().unwrap();
        x = mix(x);
        let run = Duration::from_millis(100 + x % 900);
        let (tx, rx) = std::sync::mpsc::channel::<u64>();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Ok(n) = line.trim().parse::<u64>() {
                    let _ = tx.send(n);
                }
            }
        });
        std::thread::sleep(run);
        child.kill().map_err(SError::Io)?; // SIGKILL
        let _ = child.wait();
        let _ = reader.join();
        let acked = rx.try_iter().last().unwrap_or(0);

        // Recover and check.
        let t = Instant::now();
        let (db, rep) = Db::open(&path, &cfg(a), &mut apply)?;
        let rec_s = t.elapsed().as_secs_f64();
        total_recovery += rec_s;
        max_recovery = max_recovery.max(rec_s);
        let p = db.pager();
        let st = T.check(p)?;
        let counter = T
            .get(p, &COUNTER.to_be_bytes())?
            .map(|v| u64::from_le_bytes(v[..8].try_into().unwrap()))
            .unwrap_or(0);
        if always && counter < acked {
            return Err(SError::Corrupt(format!(
                "round {round}: acknowledged transaction {acked} lost (recovered {counter})"
            )));
        }
        // No trace of any transaction after the recovered one; the
        // recovered one is complete.
        let mut c = T.scan(p)?;
        let mut seen_last = 0u64;
        while let Some((k, v)) = c.next()? {
            let key = u64::from_be_bytes(k.try_into().unwrap());
            if key == COUNTER {
                continue;
            }
            let ver = check_value(key, &v.load(p)?).map_err(SError::Corrupt)?;
            if ver > counter {
                return Err(SError::Corrupt(format!(
                    "round {round}: key {key} has version {ver} from an uncommitted transaction (committed {counter})"
                )));
            }
            if ver == counter {
                seen_last += 1;
            }
        }
        if counter > 0 && seen_last == 0 {
            // Every key of the last transaction could have been deleted by
            // its own deletes, but not plausibly all 40.
            return Err(SError::Corrupt(format!("round {round}: transaction {counter} missing entirely")));
        }
        eprintln!(
            "  round {round}: killed after {} ms, acked {acked}, recovered {counter}, replayed {} txns in {:.3} s, {} entries",
            run.as_millis(),
            rep.replayed_transactions,
            rec_s,
            st.entries
        );
        drop(db);
    }
    println!(
        "{{\"cmd\":\"crashloop\",\"rounds\":{rounds},\"sync_always\":{always},\"mean_recovery_s\":{:.4},\"max_recovery_s\":{:.4},\"ok\":true}}",
        total_recovery / rounds as f64,
        max_recovery
    );
    Ok(())
}

/// A `:memory:` database fills to its limit, reports Full, rolls back, and
/// carries on.
fn memfill(a: &Args) -> SResult<()> {
    let max = a.num("--max-mb", 64) << 20;
    let mut db = Db::memory(16384, max);
    let mut committed = 0u64;
    let mut k = 0u64;
    let full = loop {
        let mut ok = true;
        let mut err = None;
        for _ in 0..1000 {
            if let Err(e) = T.put(db.pager(), &k.to_be_bytes(), &value(k, 0)) {
                ok = false;
                err = Some(e);
                break;
            }
            k += 1;
        }
        if ok {
            db.commit()?;
            committed = k;
        } else {
            db.rollback()?;
            break err.unwrap();
        }
    };
    let SError::Full(msg) = full else {
        return Err(full);
    };
    // Still usable: every committed key is there, and after deleting some,
    // inserts work again.
    let p = db.pager();
    let st = T.check(p)?;
    if st.entries != committed {
        return Err(SError::Corrupt(format!("{} entries after rollback, expected {committed}", st.entries)));
    }
    for i in 0..committed / 2 {
        T.delete(p, &i.to_be_bytes())?;
    }
    db.commit()?;
    for i in 0..1000u64 {
        T.put(db.pager(), &(committed + i).to_be_bytes(), &value(committed + i, 0))?;
    }
    db.commit()?;
    let (used, limit) = db.pager().memory_usage().unwrap();
    println!(
        "{{\"cmd\":\"memfill\",\"max_mb\":{},\"committed_entries\":{committed},\"full_message\":\"{msg}\",\"used_mb_after\":{:.1},\"limit_mb\":{:.1},{}}}",
        max >> 20,
        used as f64 / 1048576.0,
        limit as f64 / 1048576.0,
        mem_json()
    );
    Ok(())
}
