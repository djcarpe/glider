//! `glider-stream` — continuous replication for glider databases.
//!
//! Command surface deliberately mirrors Litestream, because that is the muscle
//! memory people already have:
//!
//! ```text
//! glider-stream replicate [-config PATH] [-once] [-force-snapshot] [-enforce-retention]
//! glider-stream replicate DB REPLICA_URL
//! glider-stream restore -o OUT [-timestamp T] [-generation G] [-if-replica-exists] REPLICA_URL
//! glider-stream databases | generations | segments | snapshots | status | sync | version
//! ```

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use glider::stream::config::{self, Config, DbConfig, ReplicaConfig};
use glider::stream::replicator::{self, Metrics, Replicator};

const USAGE: &str = r#"glider-stream — continuous replication for glider databases

usage:
  glider-stream replicate [options]                 replicate everything in the config
  glider-stream replicate <db> <replica-url>        replicate one database, no config
  glider-stream restore -o <out> <replica-url>      rebuild a database from a replica
  glider-stream databases                           list configured databases
  glider-stream generations <replica-url>           lineages held in a replica
  glider-stream segments <replica-url>              segments held in a replica
  glider-stream snapshots <replica-url>             snapshots held in a replica
  glider-stream status                              replication position per database
  glider-stream sync                                one pass over the config, then exit
  glider-stream version

replica urls:
  /var/backups/app            a directory
  file:///var/backups/app     the same thing, spelled out
  s3://bucket/prefix          S3-compatible; needs -endpoint for plaintext, or the
                              aws CLI on PATH for TLS

options:
  -config PATH        config file (default /etc/glider-stream.yml)
  -no-expand-env      do not substitute $VARS in the config
  -once               replicate what is pending, then exit
  -force-snapshot     take a snapshot even if one is not due
  -enforce-retention  run retention this pass
  -o PATH             output file for restore
  -timestamp T        restore as of a unix time, or a relative offset like -30m
  -generation HEX     restore a specific lineage
  -if-replica-exists  exit 0 rather than failing when the replica is empty
  -endpoint URL       S3 endpoint, e.g. http://minio.internal:9000
  -region NAME        S3 region (default us-east-1)
  -addr HOST:PORT     serve Prometheus metrics while replicating
  -v                  verbose

credentials come from AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY, or the config.
"#;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return;
    }
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name || a == &format!("-{name}"))
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Positional arguments, minus the subcommand and minus every `-flag value`.
fn positionals(args: &[String]) -> Vec<String> {
    let takes_value = [
        "-config",
        "-o",
        "-timestamp",
        "-generation",
        "-endpoint",
        "-region",
        "-addr",
    ];
    let mut out = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if takes_value.contains(&a.as_str()) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

fn replica_from_args(url: &str, args: &[String]) -> ReplicaConfig {
    ReplicaConfig {
        url: url.to_string(),
        endpoint: flag(args, "-endpoint"),
        region: flag(args, "-region").unwrap_or_else(|| "us-east-1".into()),
        min_bytes: 0,
        sync_interval: Duration::from_secs(1),
        ..Default::default()
    }
}

fn load_config(args: &[String]) -> io::Result<Config> {
    let path = flag(args, "-config").unwrap_or_else(|| "/etc/glider-stream.yml".into());
    if !Path::new(&path).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no config at {path} — pass -config PATH, or give a db and url directly"),
        ));
    }
    config::load(&path, !has(args, "-no-expand-env"))
}

fn run(args: &[String]) -> io::Result<()> {
    let verbose = has(args, "-v");
    match args[0].as_str() {
        "version" => {
            println!("glider-stream {}", glider::VERSION);
            Ok(())
        }

        "replicate" => {
            let pos = positionals(args);
            let cfg = if pos.len() >= 2 {
                Config {
                    addr: flag(args, "-addr"),
                    dbs: vec![DbConfig {
                        path: pos[0].clone(),
                        replicas: vec![replica_from_args(&pos[1], args)],
                    }],
                }
            } else {
                let mut c = load_config(args)?;
                if let Some(a) = flag(args, "-addr") {
                    c.addr = Some(a);
                }
                c
            };
            replicate(cfg, args, verbose)
        }

        "sync" => {
            let cfg = load_config(args)?;
            for db in &cfg.dbs {
                for rc in &db.replicas {
                    let backend = config::open_backend(rc)?;
                    let mut r = Replicator::new(Path::new(&db.path), backend, rc.clone());
                    r.verbose = true;
                    r.sync(
                        true,
                        has(args, "-force-snapshot"),
                        has(args, "-enforce-retention"),
                    )?;
                }
            }
            Ok(())
        }

        "restore" => {
            let pos = positionals(args);
            let url = pos
                .first()
                .cloned()
                .ok_or_else(|| io::Error::other("restore needs a replica url"))?;
            let out = flag(args, "-o").ok_or_else(|| io::Error::other("restore needs -o PATH"))?;
            if Path::new(&out).exists() {
                return Err(io::Error::other(format!(
                    "{out} already exists — refusing to overwrite"
                )));
            }
            let as_of = match flag(args, "-timestamp") {
                Some(t) => Some(
                    glider::wal::parse_as_of(&t, glider::wal::now_unix())
                        .ok_or_else(|| io::Error::other("-timestamp takes a unix time or -30m"))?,
                ),
                None => None,
            };

            let backend = config::open_backend(&replica_from_args(&url, args))?;
            let report = match replicator::restore(
                backend.as_ref(),
                flag(args, "-generation").as_deref(),
                as_of,
                Path::new(&out),
            ) {
                Ok(r) => r,
                Err(e)
                    if e.kind() == io::ErrorKind::NotFound && has(args, "-if-replica-exists") =>
                {
                    println!("no backups found in {url} — nothing to restore");
                    return Ok(());
                }
                Err(e) => return Err(e),
            };

            // Open it. A restore that does not open is not a restore.
            let g = glider::Graph::open(Path::new(&out), glider::Sync::Normal)
                .map_err(|e| io::Error::other(e.to_string()))?;
            println!(
                "restored generation {} — {} bytes, {} segments{}",
                report.generation,
                report.bytes,
                report.segments,
                match report.snapshot {
                    Some(o) => format!(", from a snapshot at offset {o}"),
                    None => String::new(),
                }
            );
            println!("{} nodes, {} edges", g.node_count(), g.edge_count());
            Ok(())
        }

        "databases" => {
            let cfg = load_config(args)?;
            for db in &cfg.dbs {
                for rc in &db.replicas {
                    println!("{}\t{}", db.path, rc.url);
                }
                if db.replicas.is_empty() {
                    println!("{}\t(no replicas)", db.path);
                }
            }
            Ok(())
        }

        "generations" | "segments" | "snapshots" => {
            let pos = positionals(args);
            let url = pos
                .first()
                .cloned()
                .ok_or_else(|| io::Error::other("needs a replica url"))?;
            let backend = config::open_backend(&replica_from_args(&url, args))?;
            let gens = replicator::generations(backend.as_ref())?;
            if gens.is_empty() {
                println!("no generations in {url}");
                return Ok(());
            }
            for gen in gens {
                let l = replicator::listing(backend.as_ref(), &gen)?;
                match args[0].as_str() {
                    "generations" => {
                        let bytes: u64 = l.segments.iter().map(|(_, s)| s).sum();
                        let newest = l
                            .segments
                            .iter()
                            .chain(l.snapshots.iter())
                            .map(|(k, _)| k.ts)
                            .max()
                            .unwrap_or(0);
                        println!(
                            "{gen}  {} segments ({bytes} bytes), {} snapshots, complete to {}, last {}",
                            l.segments.len(),
                            l.snapshots.len(),
                            replicator::contiguous_end(&l),
                            glider::wal::fmt_unix(newest)
                        );
                    }
                    "segments" => {
                        for (k, size) in &l.segments {
                            println!(
                                "{gen}  offset {:>12}  {:>10} bytes  {}",
                                k.offset,
                                size,
                                glider::wal::fmt_unix(k.ts)
                            );
                        }
                    }
                    _ => {
                        for (k, size) in &l.snapshots {
                            println!(
                                "{gen}  through {:>12}  {:>10} bytes  {}",
                                k.offset,
                                size,
                                glider::wal::fmt_unix(k.ts)
                            );
                        }
                    }
                }
            }
            Ok(())
        }

        "status" => {
            let cfg = load_config(args)?;
            for db in &cfg.dbs {
                let local = glider::store::scan_committed_end(Path::new(&db.path), 0).unwrap_or(0);
                let gen = glider::store::read_header(Path::new(&db.path))
                    .map(|h| h.generation_hex())
                    .unwrap_or_else(|_| "unreadable".into());
                for rc in &db.replicas {
                    let backend = config::open_backend(rc)?;
                    let l = replicator::listing(backend.as_ref(), &gen)?;
                    let replicated = replicator::contiguous_end(&l);
                    println!(
                        "{}\n  generation {gen}\n  committed  {local}\n  replicated {replicated} ({})\n  lag        {} bytes",
                        db.path,
                        rc.url,
                        local.saturating_sub(replicated)
                    );
                }
            }
            Ok(())
        }

        other => Err(io::Error::other(format!(
            "unknown command '{other}' — see --help"
        ))),
    }
}

// ----------------------------------------------------------------- replicate

fn replicate(cfg: Config, args: &[String], verbose: bool) -> io::Result<()> {
    let once = has(args, "-once");
    let force_snapshot = has(args, "-force-snapshot");
    let enforce_retention = has(args, "-enforce-retention");

    if cfg.dbs.is_empty() {
        return Err(io::Error::other("no databases configured"));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    let mut exported: Vec<(String, String, Arc<Metrics>)> = Vec::new();

    for db in cfg.dbs {
        for rc in db.replicas {
            let backend = config::open_backend(&rc)?;
            let mut r = Replicator::new(Path::new(&db.path), backend, rc.clone());
            r.verbose = true;

            if once {
                match r.sync(true, force_snapshot, enforce_retention) {
                    Ok(s) => println!(
                        "{}: generation {} — {} bytes shipped{}",
                        db.path,
                        s.generation,
                        s.shipped_bytes,
                        if s.snapshot { ", snapshot taken" } else { "" }
                    ),
                    Err(e) => eprintln!("error: {}: {e}", db.path),
                }
                continue;
            }

            exported.push((db.path.clone(), rc.url.clone(), r.metrics.clone()));
            let stop = stop.clone();
            handles.push(std::thread::spawn(move || {
                if let Err(e) = r.run(stop) {
                    eprintln!("replicator stopped: {e}");
                }
            }));
        }
    }

    if once {
        return Ok(());
    }

    if let Some(addr) = cfg.addr {
        let addr = if addr.starts_with(':') {
            format!("127.0.0.1{addr}")
        } else {
            addr
        };
        let metrics = exported.clone();
        std::thread::spawn(move || serve_metrics(&addr, metrics));
    }
    if verbose {
        println!("replicating {} target(s); ctrl-c to stop", handles.len());
    }

    for h in handles {
        let _ = h.join();
    }
    stop.store(true, Ordering::Relaxed);
    Ok(())
}

/// A Prometheus scrape endpoint. Same minimal HTTP shape the database's own
/// server uses — enough to answer a scraper, and nothing more.
fn serve_metrics(addr: &str, metrics: Vec<(String, String, Arc<Metrics>)>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = match std::net::TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("metrics: cannot bind {addr}: {e}");
            return;
        }
    };
    println!("metrics on http://{addr}/metrics");

    for stream in listener.incoming().flatten() {
        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        let body: String = metrics
            .iter()
            .map(|(db, replica, m)| m.prometheus(db, replica))
            .collect();
        let mut stream = stream;
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
    }
}
