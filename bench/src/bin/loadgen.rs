//! Closed-loop HTTP load generator for `glider <db> serve`.
//!
//!   loadgen --addr 127.0.0.1:7878 --clients 8 --duration 30 --query-file q.txt
//!
//! Reports throughput and a latency distribution. The point of this tool in the
//! suite is the concurrency axis: glider serialises every request behind a
//! single `Mutex<Graph>`, so this is what quantifies the cost of that choice
//! rather than leaving it as an assertion in a README.
//!
//! Two details that would otherwise corrupt the numbers:
//!
//!   * glider replies `Connection: close`, so there is no keep-alive to reuse.
//!     Every request pays a fresh TCP handshake. That is a property of the
//!     server, not an artefact of the harness, so we measure it rather than
//!     working around it — but it does mean throughput here includes connection
//!     setup, and the report says so.
//!   * Latency is recorded per request into a per-thread vector and merged at
//!     the end. No locking on the hot path, so the generator does not become
//!     the bottleneck it is trying to measure.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// --------------------------------------------------------------- args

struct Args {
    addr: String,
    clients: usize,
    duration: Duration,
    warmup: Duration,
    queries: Vec<String>,
    label: String,
    json: bool,
}

const DEFAULT_QUERIES: &[&str] = &["MATCH (n:Person) RETURN count(n)"];

fn parse_args() -> Result<Args, String> {
    let mut addr = "127.0.0.1:7878".to_string();
    let mut clients = 1usize;
    let mut duration = 20u64;
    let mut warmup = 3u64;
    let mut query_file: Option<String> = None;
    let mut query: Option<String> = None;
    let mut label = String::new();
    let mut json = false;

    let argv: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let need = |i: usize| -> Result<String, String> {
            argv.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} needs a value", argv[i]))
        };
        match argv[i].as_str() {
            "--addr" => {
                addr = need(i)?;
                i += 2;
            }
            "--clients" | "-c" => {
                clients = need(i)?.parse().map_err(|_| "bad --clients".to_string())?;
                i += 2;
            }
            "--duration" | "-d" => {
                duration = need(i)?.parse().map_err(|_| "bad --duration".to_string())?;
                i += 2;
            }
            "--warmup" => {
                warmup = need(i)?.parse().map_err(|_| "bad --warmup".to_string())?;
                i += 2;
            }
            "--query-file" | "-f" => {
                query_file = Some(need(i)?);
                i += 2;
            }
            "--query" | "-q" => {
                query = Some(need(i)?);
                i += 2;
            }
            "--label" => {
                label = need(i)?;
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "-h" | "--help" => {
                eprintln!(
                    "loadgen --addr HOST:PORT [--clients N] [--duration S] [--warmup S]\n\
                     \t[--query Q | --query-file FILE] [--label NAME] [--json]\n\n\
                     --query-file takes one query per line; blank lines and # comments ignored.\n\
                     Clients pick queries round-robin from that list."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag {}", other)),
        }
    }

    let queries: Vec<String> = if let Some(path) = query_file {
        fs::read_to_string(&path)
            .map_err(|e| format!("{}: {}", path, e))?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect()
    } else if let Some(q) = query {
        vec![q]
    } else {
        DEFAULT_QUERIES.iter().map(|s| s.to_string()).collect()
    };

    if queries.is_empty() {
        return Err("no queries to run".into());
    }
    if clients == 0 {
        return Err("--clients must be > 0".into());
    }

    Ok(Args {
        addr,
        clients,
        duration: Duration::from_secs(duration),
        warmup: Duration::from_secs(warmup),
        queries,
        label,
        json,
    })
}

// --------------------------------------------------------------- one request

/// Send one POST /query and read the whole response. Returns the body length on
/// success so the caller can sanity-check that work actually happened, or an
/// error string on any failure.
fn one_request(addr: &std::net::SocketAddr, body: &str) -> Result<usize, String> {
    let mut s = TcpStream::connect_timeout(addr, Duration::from_secs(10))
        .map_err(|e| format!("connect: {}", e))?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|e| format!("timeout: {}", e))?;

    let req = format!(
        "POST /query HTTP/1.1\r\nHost: glider\r\nContent-Type: text/plain\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes())
        .map_err(|e| format!("write: {}", e))?;
    s.flush().map_err(|e| format!("flush: {}", e))?;

    // The server closes the connection when done, so read to EOF. That is also
    // why we do not need to parse Content-Length here.
    let mut buf = Vec::with_capacity(8192);
    s.read_to_end(&mut buf)
        .map_err(|e| format!("read: {}", e))?;
    if buf.is_empty() {
        return Err("empty response".into());
    }
    // Distinguish a 200 from a 400 so a workload of silently-failing queries
    // cannot masquerade as excellent throughput.
    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let first = head.lines().next().unwrap_or("");
    if !first.contains(" 200 ") {
        return Err(format!("http: {}", first.trim()));
    }
    Ok(buf.len())
}

// --------------------------------------------------------------- main

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("loadgen: {}", e);
            std::process::exit(2);
        }
    };
    let addr = match args.addr.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => {
            eprintln!("loadgen: cannot resolve {}", args.addr);
            std::process::exit(2);
        }
    };

    // Fail fast and loudly if the server is not up or the workload is invalid;
    // a benchmark that reports zeros because nothing connected is worse than
    // one that refuses to start.
    if let Err(e) = one_request(&addr, &args.queries[0]) {
        eprintln!("loadgen: probe request failed: {}", e);
        std::process::exit(1);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let queries = Arc::new(args.queries.clone());

    let mut handles = Vec::with_capacity(args.clients);
    for c in 0..args.clients {
        let stop = Arc::clone(&stop);
        let queries = Arc::clone(&queries);
        let warmup = args.warmup;
        handles.push(std::thread::spawn(move || {
            let mut lat: Vec<f64> = Vec::with_capacity(1 << 16);
            let mut errors = 0usize;
            let mut bytes = 0usize;
            // Stagger the starting query so concurrent clients are not all
            // running the identical statement in lockstep.
            let mut qi = c % queries.len();
            let began = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                let q = &queries[qi];
                qi = (qi + 1) % queries.len();

                let t0 = Instant::now();
                let r = one_request(&addr, q);
                let dt = t0.elapsed().as_secs_f64() * 1000.0;

                // Warmup requests are issued but not recorded: they pay for
                // page faults and first-touch allocation that would otherwise
                // land in the tail of the distribution.
                let measuring = began.elapsed() >= warmup;
                match r {
                    Ok(n) => {
                        if measuring {
                            lat.push(dt);
                            bytes += n;
                        }
                    }
                    Err(_) => {
                        if measuring {
                            errors += 1;
                        }
                    }
                }
            }
            (lat, errors, bytes)
        }));
    }

    let measure_started = Instant::now();
    std::thread::sleep(args.warmup + args.duration);
    stop.store(true, Ordering::Relaxed);
    let wall = measure_started.elapsed().as_secs_f64() - args.warmup.as_secs_f64();

    let mut all: Vec<f64> = Vec::new();
    let mut errors = 0usize;
    let mut bytes = 0usize;
    for h in handles {
        match h.join() {
            Ok((l, e, b)) => {
                all.extend_from_slice(&l);
                errors += e;
                bytes += b;
            }
            Err(_) => eprintln!("loadgen: a client thread panicked"),
        }
    }

    all.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = all.len();
    let pct = |p: f64| -> f64 {
        if n == 0 {
            return 0.0;
        }
        // Nearest-rank. With n in the thousands the choice of interpolation
        // rule is far below the run-to-run noise.
        let idx = ((p / 100.0) * n as f64).ceil() as usize;
        all[idx.saturating_sub(1).min(n - 1)]
    };
    let mean = if n == 0 {
        0.0
    } else {
        all.iter().sum::<f64>() / n as f64
    };
    let qps = if wall > 0.0 { n as f64 / wall } else { 0.0 };

    if args.json {
        println!(
            "{{\"label\":\"{}\",\"clients\":{},\"seconds\":{:.2},\"requests\":{},\"errors\":{},\
             \"qps\":{:.1},\"mean_ms\":{:.3},\"p50_ms\":{:.3},\"p95_ms\":{:.3},\"p99_ms\":{:.3},\
             \"max_ms\":{:.3},\"bytes\":{}}}",
            args.label.replace('"', "'"),
            args.clients,
            wall,
            n,
            errors,
            qps,
            mean,
            pct(50.0),
            pct(95.0),
            pct(99.0),
            all.last().copied().unwrap_or(0.0),
            bytes
        );
    } else {
        println!(
            "{:<28} clients={:<3} {:>9.1} q/s  mean {:>8.3}ms  p50 {:>8.3}  p95 {:>8.3}  p99 {:>8.3}  errors {}",
            args.label, args.clients, qps, mean, pct(50.0), pct(95.0), pct(99.0), errors
        );
    }

    if errors > 0 && n == 0 {
        std::process::exit(1);
    }
}
