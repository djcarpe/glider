//! Command line front end.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use glider::graph::Graph;
use glider::query::{self, QueryResult};
use glider::store::Sync;
use glider::value::Value;

const USAGE: &str = r#"glider — an embeddable graph database in one binary

usage:
  glider <db>                      open an interactive shell
  glider <db> -c "<query>"         run one query and exit
  glider <db> -f <file.gql>        run every statement in a file
  glider <db> serve [--addr HOST:PORT]
  glider <db> browser [--addr HOST:PORT] [--no-open]
  glider <db> import <file.jsonl>
  glider <db> export [file.jsonl]
  glider <db> stats | verify | bench [n]
  glider <db> compact              checkpoint: fold the write-ahead log into the pages
  glider <db> migrate              convert a file from before paged storage

replication (see docs/REPLICATION.md):
  glider <db> wal tail --to <dir> [--exec CMD] [--interval S] [--once]
  glider <db> wal status --to <dir>
  glider wal verify --from <dir>
  glider wal restore --from <dir> --to <file> [--generation HEX] [--as-of T]

  <db> is a file path, or :memory: for a throwaway in-memory graph.

options:
  --sync always|normal|off   durability (default normal)
  --cache-size SIZE          page cache for a file-backed database (default
                             1G). RAM use stays near this however large the
                             database grows; the database is limited by disk
  --max-memory SIZE          :memory: only: the most memory the graph may use
                             (default: physical RAM). Past it, writes fail
                             cleanly and roll back
  --work-mem SIZE            memory algorithms may use before spilling to
                             <db>-tmp/ (default 256M; :memory: uses the
                             headroom under --max-memory)
  --checkpoint SIZE|off      fold the write-ahead log into the database pages
                             after this much log (default 256M); bounds crash
                             recovery time
  --force                    open despite a lock left by a dead writer
  --json                     print results as JSON instead of a table
  --addr HOST:PORT           bind address for serve/browser (default 127.0.0.1:7878)
  --no-open                  browser: start the server but do not open a browser
  -h, --help                 this text
  -V, --version              version

shell commands:  .help  .quit  .json  .table  .timer  .schema  .import <f>
                 .export [f]  .compact  .read <f>
"#;

struct Options {
    db: String,
    command: Option<String>,
    file: Option<PathBuf>,
    mode: Mode,
    sync: Sync,
    json: bool,
    addr: String,
    bench: usize,
    /// Break a lock left behind by a writer that is definitely gone.
    force: bool,
    /// `browser` only: skip launching the user's browser.
    no_open: bool,
    cache_size: u64,
    max_memory: Option<u64>,
    checkpoint_bytes: u64,
    work_mem: u64,
}

enum Mode {
    Shell,
    Serve,
    /// Serve, and open the console in the user's browser.
    Browser,
    Import(PathBuf),
    Export(Option<PathBuf>),
    Stats,
    Compact,
    Bench,
}

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", USAGE);
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("glider {}", glider::VERSION);
        return Ok(());
    }

    // `glider wal ...` and `glider <db> wal ...`.
    if let Some(at) = args.iter().position(|a| a == "wal") {
        return run_wal(&args, at);
    }

    if args.len() >= 2 && (args[1] == "verify" || args[1] == "migrate") && !Path::new(&args[0]).exists() {
        return Err(format!("{}: no such database", args[0]));
    }

    if args.len() >= 2 && args[1] == "migrate" {
        return migrate(&args[0]);
    }

    if args.len() >= 2 && args[1] == "verify" && glider::legacy::detect(Path::new(&args[0])).is_none() {
        return verify_paged(&args[0]);
    }

    if args.len() >= 2 && args[1] == "verify" {
        let r = glider::store::verify(Path::new(&args[0])).map_err(|e| e.to_string())?;
        println!("records       {}", r.records);
        println!("transactions  {}", r.transactions);
        println!("committed     {} bytes", r.committed_len);
        println!("file          {} bytes", r.file_len);
        match r.bad_offset {
            None => println!("integrity     ok"),
            Some(at) if at >= r.committed_len => println!(
                "integrity     ok; {} unusable bytes after the last commit at {} \
                 (an ordinary torn tail — discarded on next open)",
                r.file_len - at,
                at
            ),
            Some(at) => println!(
                "integrity     CORRUPT at offset {} — everything above it is lost, \
                 {} bytes are still good",
                at, at
            ),
        }
        match &r.image {
            None => println!("image         none (the whole log is replayed on open)"),
            Some(Ok(i)) => println!(
                "image         ok; {} bytes, {} nodes, {} edges, {} indexes",
                i.bytes, i.nodes, i.edges, i.indexes
            ),
            Some(Err(e)) => println!("image         CORRUPT: {}", e),
        }
        return Ok(());
    }

    let mut opts = Options {
        force: false,
        no_open: false,
        db: ":memory:".into(),
        command: None,
        file: None,
        mode: Mode::Shell,
        sync: Sync::Normal,
        json: false,
        addr: "127.0.0.1:7878".into(),
        bench: 50_000,
        cache_size: glider::storage::pager::DEFAULT_CACHE_BYTES,
        max_memory: None,
        checkpoint_bytes: 256 << 20,
        work_mem: glider::graph::DEFAULT_WORK_MEM,
    };

    let mut i = 0;
    let mut positional = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-c" | "--command" => {
                i += 1;
                opts.command = Some(args.get(i).ok_or("-c needs a query")?.clone());
            }
            "-f" | "--file" => {
                i += 1;
                opts.file = Some(PathBuf::from(args.get(i).ok_or("-f needs a path")?));
            }
            "--force" => opts.force = true,
            "--no-open" => opts.no_open = true,
            "--sync" => {
                i += 1;
                opts.sync = match args.get(i).map(|s| s.as_str()) {
                    Some("always") => Sync::Always,
                    Some("normal") => Sync::Normal,
                    Some("off") => Sync::Off,
                    _ => return Err("--sync takes always, normal or off".into()),
                };
            }
            "--cache-size" => {
                i += 1;
                opts.cache_size = parse_size(args.get(i).map(|s| s.as_str()).unwrap_or(""))
                    .ok_or("--cache-size takes a size, like 512M or 2G")?;
            }
            "--max-memory" => {
                i += 1;
                opts.max_memory = Some(
                    parse_size(args.get(i).map(|s| s.as_str()).unwrap_or(""))
                        .ok_or("--max-memory takes a size, like 512M or 4G")?,
                );
            }
            "--work-mem" => {
                i += 1;
                opts.work_mem = parse_size(args.get(i).map(|s| s.as_str()).unwrap_or(""))
                    .ok_or("--work-mem takes a size, like 256M or 1G")?;
            }
            "--checkpoint" => {
                i += 1;
                opts.checkpoint_bytes = match args.get(i).map(|s| s.as_str()) {
                    Some("off") => u64::MAX,
                    Some(v) => parse_size(v).ok_or("--checkpoint takes a size or off")?,
                    None => return Err("--checkpoint takes a size or off".into()),
                };
            }
            "--addr" => {
                i += 1;
                opts.addr = args.get(i).ok_or("--addr needs HOST:PORT")?.clone();
            }
            "--json" => opts.json = true,
            "serve" => opts.mode = Mode::Serve,
            "browser" | "ui" | "console" => opts.mode = Mode::Browser,
            "stats" => opts.mode = Mode::Stats,
            "compact" => opts.mode = Mode::Compact,
            "bench" => opts.mode = Mode::Bench,
            "import" => {
                i += 1;
                opts.mode = Mode::Import(PathBuf::from(args.get(i).ok_or("import needs a file")?));
            }
            "export" => {
                let next = args.get(i + 1).filter(|s| !s.starts_with('-')).cloned();
                if next.is_some() {
                    i += 1;
                }
                opts.mode = Mode::Export(next.map(PathBuf::from));
            }
            other => {
                if positional == 0 {
                    opts.db = other.to_string();
                    positional += 1;
                } else if matches!(opts.mode, Mode::Bench) {
                    opts.bench = other.parse().unwrap_or(opts.bench);
                } else {
                    return Err(format!("unexpected argument '{}'", other));
                }
            }
        }
        i += 1;
    }

    let mut graph = open_graph(&opts)?;

    if let Some(cmd) = &opts.command {
        return run_script(&mut graph, cmd, opts.json);
    }
    if let Some(path) = &opts.file {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
        return run_script(&mut graph, &text, opts.json);
    }

    match opts.mode {
        Mode::Serve => glider::server::serve(graph, &opts.addr).map_err(|e| e.to_string()),
        Mode::Browser => {
            // Bind first, then open the browser, so the page never races the
            // listener and lands on a connection-refused error.
            let listener = std::net::TcpListener::bind(&opts.addr)
                .map_err(|e| format!("{}: {}", opts.addr, e))?;
            let url = format!(
                "http://{}/",
                listener.local_addr().map_err(|e| e.to_string())?
            );
            if !opts.no_open {
                open_browser(&url);
            }
            println!("glider browser at {}", url);
            glider::server::serve_on(listener, graph).map_err(|e| e.to_string())
        }
        Mode::Stats => run_script(&mut graph, "STATS", opts.json),
        Mode::Compact => run_script(&mut graph, "COMPACT", opts.json),
        Mode::Import(path) => {
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
            let start = Instant::now();
            let (n, e) = query::import_jsonl(&mut graph, &text).map_err(|e| e.to_string())?;
            println!(
                "imported {} nodes and {} edges in {:.2}s",
                n,
                e,
                start.elapsed().as_secs_f64()
            );
            Ok(())
        }
        Mode::Export(path) => {
            export(&graph, path.as_deref()).map_err(|e| match &path {
                Some(p) => format!("{}: {}", p.display(), e),
                None => e.to_string(),
            })?;
            if let Some(p) = path {
                println!("exported to {}", p.display());
            }
            Ok(())
        }
        Mode::Bench => bench(&mut graph, opts.bench),
        Mode::Shell => shell(graph, opts.json),
    }
}

/// Stream the dump to a file or stdout without holding it in memory.
fn export(graph: &Graph, path: Option<&Path>) -> std::io::Result<()> {
    match path {
        Some(p) => {
            let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(p)?);
            query::export_jsonl_to(graph, &mut w)
        }
        None => {
            let mut w = std::io::BufWriter::with_capacity(1 << 20, std::io::stdout().lock());
            query::export_jsonl_to(graph, &mut w)
        }
    }
}

fn open_graph(opts: &Options) -> Result<Graph, String> {
    if opts.db == ":memory:" {
        return Ok(match opts.max_memory {
            Some(m) => Graph::memory_with_limit(m),
            None => Graph::memory(),
        });
    }
    let o = glider::OpenOptions {
        sync: opts.sync,
        force: opts.force,
        cache_size: opts.cache_size,
        checkpoint_bytes: opts.checkpoint_bytes,
        work_mem: opts.work_mem,
        ..glider::OpenOptions::default()
    };
    Graph::open_opts(std::path::Path::new(&opts.db), o).map_err(|e| e.to_string())
}

/// "512M", "2G", "64k", "1.5T" or plain bytes; binary units.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().last()? {
        (i, 'k' | 'K') => (&s[..i], 1u64 << 10),
        (i, 'm' | 'M') => (&s[..i], 1 << 20),
        (i, 'g' | 'G') => (&s[..i], 1 << 30),
        (i, 't' | 'T') => (&s[..i], 1 << 40),
        _ => (s, 1),
    };
    let n: f64 = num.trim().parse().ok()?;
    (n >= 0.0).then_some((n * mult as f64) as u64)
}

/// Convert a file from before paged storage (formats v1–v3) in place,
/// keeping the original as `<db>.legacy.bak`.
fn migrate(db: &str) -> Result<(), String> {
    let path = Path::new(db);
    let Some(kind) = glider::legacy::detect(path) else {
        return Err(format!("{db} is not a legacy glider file; nothing to migrate"));
    };
    let t = std::time::Instant::now();
    eprintln!("reading {db} ({kind}) ...");
    let old = glider::legacy::graph::Graph::open(path, Sync::Normal).map_err(|e| e.to_string())?;
    let tmp = format!("{db}.migrating");
    let _ = std::fs::remove_file(&tmp);
    let mut g = Graph::open(Path::new(&tmp), Sync::Off).map_err(|e| e.to_string())?;
    g.import_legacy(&old).map_err(|e| e.to_string())?;
    g.checkpoint().map_err(|e| e.to_string())?;
    let (n, e) = (g.node_count(), g.edge_count());
    drop(g);
    drop(old);
    let bak = format!("{db}.legacy.bak");
    std::fs::rename(path, &bak).map_err(|e| e.to_string())?;
    for (from, to) in [
        (tmp.clone(), db.to_string()),
        (format!("{tmp}-wal"), format!("{db}-wal")),
        (format!("{tmp}-data"), format!("{db}-data")),
    ] {
        if Path::new(&from).exists() {
            std::fs::rename(&from, &to).map_err(|e| e.to_string())?;
        }
    }
    let _ = std::fs::remove_file(format!("{tmp}.lock"));
    println!(
        "migrated {n} nodes and {e} edges in {:.1} s; the original is kept as {bak}",
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Integrity check of a paged database: every tree's order, bounds and
/// ownership, and every page checksum on the way.
fn verify_paged(db: &str) -> Result<(), String> {
    let g = Graph::open(Path::new(db), Sync::Normal).map_err(|e| e.to_string())?;
    let t = std::time::Instant::now();
    let entries = g.verify_trees().map_err(|e| e.to_string())?;
    let s = g.stats();
    println!("format        paged (v4)");
    println!("nodes         {}", s.nodes);
    println!("edges         {}", s.edges);
    println!("tree entries  {entries}");
    println!("file          {} bytes", s.file_bytes);
    println!("page size     {} bytes", s.page_size);
    println!("integrity     ok ({:.2} s)", t.elapsed().as_secs_f64());
    Ok(())
}

fn run_script(graph: &mut Graph, text: &str, json: bool) -> Result<(), String> {
    for stmt in split_statements(text) {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        match query::execute(graph, stmt) {
            Ok(r) => print_result(&r, json, false, 0.0),
            Err(e) => return Err(e.to_string()),
        }
    }
    graph.commit().map_err(|e| e.to_string())
}

/// Split on semicolons that aren't inside a string literal.
fn split_statements(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == '\\' {
                    if let Some(n) = chars.next() {
                        current.push(n);
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    current.push(c);
                } else if c == ';' {
                    out.push(std::mem::take(&mut current));
                } else {
                    current.push(c);
                }
            }
        }
    }
    out.push(current);
    out
}

// --------------------------------------------------------------------- shell

fn shell(mut graph: Graph, mut json: bool) -> Result<(), String> {
    let interactive = std::io::stdin().is_terminal();
    let mut timer = false;
    if interactive {
        println!(
            "glider {} — {} ({} nodes, {} edges)",
            glider::VERSION,
            graph
                .path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| ":memory:".into()),
            graph.node_count(),
            graph.edge_count()
        );
        println!("type HELP for the language, .help for shell commands, .quit to exit");
    }

    let stdin = std::io::stdin();
    let mut buffer = String::new();
    loop {
        if interactive {
            print!("{}", if buffer.is_empty() { "» " } else { "…  " });
            let _ = std::io::stdout().flush();
        }
        let mut line = String::new();
        if stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?
            == 0
        {
            break;
        }
        let trimmed = line.trim();

        if buffer.is_empty() && trimmed.starts_with('.') {
            match dot_command(&mut graph, trimmed, &mut json, &mut timer) {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    eprintln!("error: {}", e);
                    continue;
                }
            }
        }
        if trimmed.is_empty() && buffer.trim().is_empty() {
            continue;
        }

        buffer.push_str(&line);
        // A statement ends at a semicolon, or at a blank line in interactive use.
        let complete = buffer.contains(';') || (interactive && trimmed.is_empty());
        if !complete {
            continue;
        }

        let script = std::mem::take(&mut buffer);
        for stmt in split_statements(&script) {
            if stmt.trim().is_empty() {
                continue;
            }
            let start = Instant::now();
            match query::execute(&mut graph, stmt.trim()) {
                Ok(r) => print_result(&r, json, timer, start.elapsed().as_secs_f64()),
                Err(e) => {
                    eprintln!("error: {}", e);
                    // Scripts reading --json output get one line per statement.
                    if json {
                        println!("{{\"error\":{}}}", Value::Text(e.to_string()).to_json());
                    }
                }
            }
        }
    }
    graph.commit().map_err(|e| e.to_string())
}

fn dot_command(
    graph: &mut Graph,
    line: &str,
    json: &mut bool,
    timer: &mut bool,
) -> Result<bool, String> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next();
    match cmd {
        ".quit" | ".exit" | ".q" => return Ok(false),
        ".help" => print!("{}", USAGE),
        ".json" => {
            *json = true;
            println!("output: json");
        }
        ".table" => {
            *json = false;
            println!("output: table");
        }
        ".timer" => {
            *timer = !*timer;
            println!("timer: {}", if *timer { "on" } else { "off" });
        }
        ".schema" => match query::execute(graph, "SCHEMA") {
            Ok(r) => print_result(&r, *json, false, 0.0),
            Err(e) => return Err(e.to_string()),
        },
        ".compact" => match query::execute(graph, "COMPACT") {
            Ok(r) => print_result(&r, *json, false, 0.0),
            Err(e) => return Err(e.to_string()),
        },
        ".import" => {
            let path = arg.ok_or(".import needs a file")?;
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let (n, e) = query::import_jsonl(graph, &text).map_err(|e| e.to_string())?;
            println!("imported {} nodes, {} edges", n, e);
        }
        ".export" => {
            export(graph, arg.map(Path::new)).map_err(|e| e.to_string())?;
            if let Some(p) = arg {
                println!("exported to {}", p);
            }
        }
        ".read" => {
            let path = arg.ok_or(".read needs a file")?;
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            run_script(graph, &text, *json)?;
        }
        other => return Err(format!("unknown command {}", other)),
    }
    Ok(true)
}

// -------------------------------------------------------------------- output

fn print_result(r: &QueryResult, json: bool, timer: bool, secs: f64) {
    if json {
        println!("{}", r.to_json());
        if timer {
            println!("{{\"elapsed_ms\":{:.3}}}", secs * 1000.0);
        }
        return;
    }
    if let Some(m) = &r.message {
        println!("{}", m);
    }
    if !r.columns.is_empty() {
        print_table(&r.columns, &r.rows);
        println!(
            "{} row{}",
            r.rows.len(),
            if r.rows.len() == 1 { "" } else { "s" }
        );
    }
    if timer {
        println!("({:.3} ms)", secs * 1000.0);
    }
}

fn print_table(columns: &[String], rows: &[Vec<Value>]) {
    const MAX: usize = 64;
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(|v| truncate(&v.to_string(), MAX)).collect())
        .collect();

    let mut widths: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }

    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| pad(c, widths[i]))
        .collect();
    println!("{}", header.join("  "));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for row in &cells {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, *widths.get(i).unwrap_or(&0)))
            .collect();
        println!("{}", line.join("  "));
    }
}

fn pad(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - len))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.replace('\n', " ")
    } else {
        let head: String = s.chars().take(max - 1).collect();
        format!("{}…", head.replace('\n', " "))
    }
}

// ------------------------------------------------------------------- browser

/// Open a URL in the user's default browser, best effort.
///
/// Spawned detached and never waited on: if no browser is configured, or the
/// machine is headless, the server must still come up. The URL is always
/// printed so there is a path forward either way.
fn open_browser(url: &str) {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("open", &[])]
    } else if cfg!(target_os = "windows") {
        &[("cmd", &["/C", "start", ""])]
    } else {
        &[
            ("xdg-open", &[]),
            ("gio", &["open"]),
            ("sensible-browser", &[]),
        ]
    };

    for (cmd, prefix) in candidates {
        let mut c = std::process::Command::new(cmd);
        c.args(prefix.iter())
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if c.spawn().is_ok() {
            return;
        }
    }
    eprintln!(
        "could not open a browser automatically — open {} yourself",
        url
    );
}

// --------------------------------------------------------------------- bench

fn bench(graph: &mut Graph, n: usize) -> Result<(), String> {
    let edges_per_node = 4usize;
    println!("building a {}-node, ~{}-edge graph", n, n * edges_per_node);

    graph.autocommit = false;
    let start = Instant::now();
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let id = graph
            .add_node(
                &["Node".to_string()],
                vec![
                    ("name".into(), Value::Text(format!("n{}", i))),
                    ("seq".into(), Value::Int(i as i64)),
                ],
            )
            .map_err(|e| e.to_string())?;
        ids.push(id);
    }
    graph.commit().map_err(|e| e.to_string())?;
    let node_secs = start.elapsed().as_secs_f64();
    println!(
        "  nodes: {:.2}s  ({:.0}/s)",
        node_secs,
        n as f64 / node_secs.max(1e-9)
    );

    // Deterministic pseudo-random edges, no rand dependency.
    let start = Instant::now();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut edge_count = 0usize;
    for &from in &ids {
        for _ in 0..edges_per_node {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let to = ids[(state % ids.len() as u64) as usize];
            if to != from {
                graph
                    .add_edge(
                        from,
                        to,
                        "LINKS",
                        vec![("w".into(), Value::Int((state % 10) as i64))],
                    )
                    .map_err(|e| e.to_string())?;
                edge_count += 1;
            }
        }
    }
    graph.commit().map_err(|e| e.to_string())?;
    graph.autocommit = true;
    let edge_secs = start.elapsed().as_secs_f64();
    println!(
        "  edges: {:.2}s  ({:.0}/s)",
        edge_secs,
        edge_count as f64 / edge_secs.max(1e-9)
    );

    for q in [
        "CALL pagerank(iterations: 20, top: 3)",
        "CALL components(top: 3)",
        "CALL kcore(top: 3)",
        "CALL triangles(top: 3)",
    ] {
        let start = Instant::now();
        let r = query::execute(graph, q).map_err(|e| e.to_string())?;
        println!(
            "  {:<40} {:.3}s  {}",
            q,
            start.elapsed().as_secs_f64(),
            r.message.clone().unwrap_or_default()
        );
    }

    let start = Instant::now();
    let r = query::execute(
        graph,
        "MATCH (a)-[:LINKS]->(b)-[:LINKS]->(c) RETURN count(c) AS paths",
    )
    .map_err(|e| e.to_string())?;
    println!(
        "  {:<40} {:.3}s  {} two-hop paths",
        "two-hop pattern match",
        start.elapsed().as_secs_f64(),
        r.rows
            .first()
            .and_then(|row| row.first())
            .map(|v| v.to_string())
            .unwrap_or_default()
    );

    println!("  file size: {} bytes", graph.file_len());
    Ok(())
}

// Keep `Read` in scope for platforms where IsTerminal needs it.
#[allow(dead_code)]
fn _unused(_: &dyn Read) {}

// ------------------------------------------------------------- replication

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn run_wal(args: &[String], at: usize) -> Result<(), String> {
    let db = if at > 0 {
        Some(args[at - 1].clone())
    } else {
        None
    };
    let sub = args.get(at + 1).map(|s| s.as_str()).unwrap_or("");

    match sub {
        "tail" => {
            let db = db.ok_or("wal tail needs a database: glider <db> wal tail --to <dir>")?;
            let dir = flag(args, "--to").ok_or("wal tail needs --to <dir>")?;
            let mut opts = glider::wal::TailOptions {
                once: has(args, "--once"),
                quiet: has(args, "--quiet"),
                exec: flag(args, "--exec"),
                ..Default::default()
            };
            if let Some(s) = flag(args, "--interval") {
                let secs: f64 = s.parse().map_err(|_| "--interval takes seconds")?;
                opts.max_delay = std::time::Duration::from_secs_f64(secs);
                opts.poll = std::time::Duration::from_secs_f64((secs / 4.0).max(0.05));
            }
            if let Some(s) = flag(args, "--min-bytes") {
                opts.min_bytes = s.parse().map_err(|_| "--min-bytes takes a number")?;
            }
            if glider::replica::is_paged(Path::new(&db)) {
                glider::replica::tail(Path::new(&db), Path::new(&dir), &opts).map_err(|e| e.to_string())
            } else {
                glider::wal::tail(Path::new(&db), Path::new(&dir), &opts).map_err(|e| e.to_string())
            }
        }

        "status" => {
            let db = db.ok_or("wal status needs a database")?;
            let dir = flag(args, "--to")
                .or_else(|| flag(args, "--from"))
                .ok_or("wal status needs --to <dir>")?;
            if let Ok(sb) = glider::storage::pager::read_superblock(Path::new(&db)) {
                let gen: String = sb.generation.iter().map(|b| format!("{b:02x}")).collect();
                let local = match glider::storage::log::committed_end(Path::new(&db), sb.wal_lsn)
                    .map_err(|e| e.to_string())?
                {
                    glider::storage::log::Committed::To(e) => e,
                    glider::storage::log::Committed::Gone => sb.wal_lsn,
                };
                let bases = glider::replica::bases(Path::new(&dir), &gen).map_err(|e| e.to_string())?;
                let segs = glider::wal::segments(Path::new(&dir), &gen).map_err(|e| e.to_string())?;
                let replicated = bases
                    .last()
                    .map(|b| glider::replica::reach(&segs, b.lsn))
                    .unwrap_or(0);
                println!("database    {}", db);
                println!("generation  {}", gen);
                println!("committed   log to {}", local);
                match bases.last() {
                    Some(b) => println!(
                        "base        at log {} ({}), {} bases",
                        b.lsn,
                        glider::wal::fmt_unix(b.ts),
                        bases.len()
                    ),
                    None => println!("base        none yet"),
                }
                println!("replicated  log to {} in {} segments", replicated, segs.len());
                println!("lag         {} bytes", local.saturating_sub(replicated));
                if let Some(last) = segs.last() {
                    println!("last ship   {}", glider::wal::fmt_unix(last.ts));
                }
                return Ok(());
            }
            let header = glider::store::read_header(Path::new(&db)).map_err(|e| e.to_string())?;
            let local =
                glider::store::scan_committed_end(Path::new(&db), 0).map_err(|e| e.to_string())?;
            let gen = header.generation_hex();
            let segs = glider::wal::segments(Path::new(&dir), &gen).map_err(|e| e.to_string())?;
            let replicated = glider::wal::contiguous_end(&segs);
            println!("database    {}", db);
            println!(
                "generation  {}",
                if header.has_generation() {
                    gen
                } else {
                    "none (format v1 — run COMPACT to upgrade)".into()
                }
            );
            println!("committed   {} bytes", local);
            println!(
                "replicated  {} bytes in {} segments",
                replicated,
                segs.len()
            );
            println!("lag         {} bytes", local.saturating_sub(replicated));
            if let Some(last) = segs.last() {
                println!("last ship   {}", glider::wal::fmt_unix(last.ts));
            }
            Ok(())
        }

        "verify" => {
            let dir = flag(args, "--from")
                .or_else(|| flag(args, "--to"))
                .ok_or("wal verify needs --from <dir>")?;
            let paged = glider::replica::verify(Path::new(&dir)).map_err(|e| e.to_string())?;
            for g in &paged {
                println!(
                    "{}  {} bases (newest at log {}), {} log segments, {} bytes, log complete to {}{}",
                    g.generation,
                    g.bases.len(),
                    g.bases.last().map(|b| b.lsn).unwrap_or(0),
                    g.segments,
                    g.log_bytes,
                    g.complete_to,
                    match g.gap_at {
                        Some(at) => format!("  ** GAP at {} — later log is unusable **", at),
                        None => String::new(),
                    }
                );
                println!("    last activity {}", glider::wal::fmt_unix(g.last_ts));
            }
            let gens: Vec<_> = glider::wal::verify(Path::new(&dir))
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|g| !paged.iter().any(|p| p.generation == g.generation))
                .collect();
            if gens.is_empty() && !paged.is_empty() {
                return Ok(());
            }
            if gens.is_empty() {
                println!("no generations in {}", dir);
                return Ok(());
            }
            for g in gens {
                println!(
                    "{}  {} segments, {} bytes, complete to {}{}",
                    g.generation,
                    g.segments,
                    g.bytes,
                    g.complete_to,
                    match g.gap_at {
                        Some(at) => format!("  ** GAP at {} — everything after is unusable **", at),
                        None => String::new(),
                    }
                );
                println!(
                    "    {} .. {}",
                    glider::wal::fmt_unix(g.first_ts),
                    glider::wal::fmt_unix(g.last_ts)
                );
            }
            Ok(())
        }

        "restore" => {
            let dir = flag(args, "--from").ok_or("wal restore needs --from <dir>")?;
            let out = flag(args, "--to").ok_or("wal restore needs --to <file>")?;
            let generation = flag(args, "--generation");
            let as_of = match flag(args, "--as-of") {
                Some(s) => Some(
                    glider::wal::parse_as_of(&s, glider::wal::now_unix())
                        .ok_or("--as-of takes a unix timestamp or a relative offset like -30m")?,
                ),
                None => None,
            };
            if Path::new(&out).exists() {
                return Err(format!("{} already exists — refusing to overwrite", out));
            }
            let paged = glider::replica::generations(Path::new(&dir)).map_err(|e| e.to_string())?;
            let use_paged = match &generation {
                Some(g) => paged.contains(g),
                None => {
                    let legacy = glider::wal::generations(Path::new(&dir)).map_err(|e| e.to_string())?;
                    // The most recently active lineage decides.
                    legacy.last().map(|g| paged.contains(g)).unwrap_or(!paged.is_empty())
                }
            };
            if use_paged {
                let report = glider::replica::restore(
                    Path::new(&dir),
                    generation.as_deref(),
                    as_of,
                    Path::new(&out),
                )
                .map_err(|e| e.to_string())?;
                // Opening replays the log laid beside the base; a restore
                // that does not open is not a restore.
                let g = glider::Graph::open(Path::new(&out), Sync::Normal).map_err(|e| e.to_string())?;
                g.verify_trees().map_err(|e| e.to_string())?;
                println!(
                    "restored generation {} from the base at log {} ({}) and {} log segments, through log {}",
                    report.generation,
                    report.base.lsn,
                    glider::wal::fmt_unix(report.base.ts),
                    report.segments,
                    report.through
                );
                println!("{} nodes, {} edges", g.node_count(), g.edge_count());
                return Ok(());
            }
            let report = glider::wal::restore(
                Path::new(&dir),
                generation.as_deref(),
                as_of,
                Path::new(&out),
            )
            .map_err(|e| e.to_string())?;

            // A restore that does not open is not a restore.
            let g = glider::legacy::graph::Graph::open(Path::new(&out), Sync::Normal)
                .map_err(|e| e.to_string())?;
            println!(
                "restored {} bytes from {} segments of generation {}",
                report.bytes, report.segments, report.generation
            );
            println!("{} nodes, {} edges", g.node_count(), g.edge_count());
            drop(g);
            println!("(a database from before paged storage: convert it with `glider {out} migrate`)");
            Ok(())
        }

        other => Err(format!(
            "unknown wal command '{}' — try tail, status, verify or restore",
            other
        )),
    }
}
