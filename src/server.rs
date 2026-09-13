//! A small HTTP front end, written against std::net so the binary stays
//! dependency-free. One thread per connection, one mutex around the graph:
//! readers and writers serialise, which is the same concurrency model SQLite
//! gives you in its default mode.
//!
//!   POST /query   body is the query text     -> JSON {columns, rows, message}
//!   GET  /stats                              -> JSON
//!   GET  /health                             -> ok
//!   GET  /                                   -> a tiny browser console

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::api;
use crate::graph::Graph;
use crate::query;
use crate::value::write_json_string;

pub fn serve(graph: Graph, addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    eprintln!("glider listening on http://{}", listener.local_addr()?);
    serve_on(listener, graph)
}

/// Serve on an already-bound listener. `glider <db> browser` binds first so it
/// can print and open the real URL — which matters when --addr asks for port 0
/// — before any request can race the listener.
pub fn serve_on(listener: TcpListener, graph: Graph) -> std::io::Result<()> {
    let shared = Arc::new(Mutex::new(graph));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept failed: {}", e);
                continue;
            }
        };
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, shared) {
                eprintln!("connection error: {}", e);
            }
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream, graph: Arc<Mutex<Graph>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.to_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }

    let mut body = vec![0u8; content_length.min(64 * 1024 * 1024)];
    if !body.is_empty() {
        reader.read_exact(&mut body)?;
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let (status, content_type, payload) = route(&method, &path, &body, &graph);

    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        payload.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.write_all(payload.as_bytes())?;
    stream.flush()
}

/// Take the graph lock, recovering from a poisoned mutex rather than
/// propagating a panic from one request into every later one.
fn lock(graph: &Arc<Mutex<Graph>>) -> std::sync::MutexGuard<'_, Graph> {
    match graph.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn route(
    method: &str,
    path: &str,
    body: &str,
    graph: &Arc<Mutex<Graph>>,
) -> (&'static str, &'static str, String) {
    let route = path.split('?').next().unwrap_or("/");
    match (method, route) {
        ("GET", "/health") => ("200 OK", "text/plain", "ok".to_string()),
        ("GET", "/") | ("GET", "/index.html") => {
            ("200 OK", "text/html; charset=utf-8", CONSOLE.to_string())
        }

        ("POST", "/api/query") => {
            let src = body.trim();
            if src.is_empty() {
                return ("400 Bad Request", "application/json", error_json("empty query"));
            }
            let mut g = lock(graph);
            match api::query_json(&mut g, src) {
                Ok(j) => ("200 OK", "application/json", j),
                Err(e) => ("400 Bad Request", "application/json", error_json(&e)),
            }
        }
        ("GET", "/api/schema") => {
            let mut g = lock(graph);
            match api::schema_json(&mut g) {
                Ok(j) => ("200 OK", "application/json", j),
                Err(e) => ("500 Internal Server Error", "application/json", error_json(&e)),
            }
        }
        ("GET", "/api/expand") => {
            let id = api::query_param(path, "id").and_then(|v| v.parse::<u64>().ok());
            let limit = api::query_param(path, "limit")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(50)
                .min(1000);
            let Some(id) = id else {
                return ("400 Bad Request", "application/json", error_json("expand needs ?id=<node id>"));
            };
            let g = lock(graph);
            match api::expand_json(&g, id, limit) {
                Ok(j) => ("200 OK", "application/json", j),
                Err(e) => ("404 Not Found", "application/json", error_json(&e)),
            }
        }
        ("GET", "/stats") | ("POST", "/query") => {
            let src = if route == "/stats" { "STATS" } else { body.trim() };
            if src.is_empty() {
                return ("400 Bad Request", "application/json", error_json("empty query"));
            }
            let mut g = lock(graph);
            match query::execute(&mut g, src) {
                Ok(r) => ("200 OK", "application/json", r.to_json()),
                Err(e) => (
                    "400 Bad Request",
                    "application/json",
                    error_json(&e.to_string()),
                ),
            }
        }
        ("OPTIONS", _) => ("204 No Content", "text/plain", String::new()),
        _ => (
            "404 Not Found",
            "application/json",
            error_json("no such endpoint"),
        ),
    }
}

fn error_json(msg: &str) -> String {
    let mut out = String::from("{\"error\":");
    write_json_string(msg, &mut out);
    out.push('}');
    out
}

/// The browser console, built from ui/ and committed as a single inlined
/// HTML file. Embedding it keeps `glider serve` a single binary with no static
/// file routing and no runtime dependency on Node — see ui/README.md for how to
/// rebuild it.
const CONSOLE: &str = include_str!("console.html");
