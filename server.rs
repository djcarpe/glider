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

use crate::graph::Graph;
use crate::query;
use crate::value::write_json_string;

pub fn serve(graph: Graph, addr: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    eprintln!("glider listening on http://{}", local);
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

fn route(
    method: &str,
    path: &str,
    body: &str,
    graph: &Arc<Mutex<Graph>>,
) -> (&'static str, &'static str, String) {
    let route = path.split('?').next().unwrap_or("/");
    match (method, route) {
        ("GET", "/health") => ("200 OK", "text/plain", "ok".to_string()),
        ("GET", "/") => ("200 OK", "text/html; charset=utf-8", CONSOLE.to_string()),
        ("GET", "/stats") | ("POST", "/query") => {
            let src = if route == "/stats" { "STATS" } else { body.trim() };
            if src.is_empty() {
                return ("400 Bad Request", "application/json", error_json("empty query"));
            }
            let mut g = match graph.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
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

const CONSOLE: &str = r#"<!doctype html>
<meta charset="utf-8"><title>glider</title>
<style>
 body{font:14px ui-monospace,SFMono-Regular,Menlo,monospace;margin:0;background:#101215;color:#dfe3e8}
 header{padding:10px 16px;border-bottom:1px solid #23272e;color:#8b949e}
 main{padding:16px;display:flex;flex-direction:column;gap:12px;height:calc(100vh - 80px)}
 textarea{background:#171a1f;color:#dfe3e8;border:1px solid #2b313a;border-radius:6px;padding:10px;height:110px;resize:vertical;font:inherit}
 button{background:#2f6feb;color:#fff;border:0;border-radius:6px;padding:8px 14px;cursor:pointer;align-self:flex-start}
 #out{flex:1;overflow:auto;white-space:pre;background:#171a1f;border:1px solid #2b313a;border-radius:6px;padding:10px}
 table{border-collapse:collapse}td,th{border-bottom:1px solid #2b313a;padding:3px 14px 3px 0;text-align:left}
 th{color:#8b949e;font-weight:400}
</style>
<header>glider console &mdash; ctrl+enter to run</header>
<main>
 <textarea id="q" placeholder="MATCH (n) RETURN n LIMIT 10"></textarea>
 <button onclick="run()">Run</button>
 <div id="out"></div>
</main>
<script>
async function run(){
 const out=document.getElementById('out');
 out.textContent='running...';
 try{
  const r=await fetch('/query',{method:'POST',body:document.getElementById('q').value});
  const j=await r.json();
  if(j.error){out.textContent='error: '+j.error;return}
  let html='';
  if(j.message)html+='<div style="color:#8b949e">'+esc(j.message)+'</div>';
  if(j.columns.length){
   html+='<table><tr>'+j.columns.map(c=>'<th>'+esc(c)+'</th>').join('')+'</tr>';
   for(const row of j.rows)html+='<tr>'+row.map(v=>'<td>'+esc(String(v))+'</td>').join('')+'</tr>';
   html+='</table><div style="color:#8b949e">'+j.rows.length+' rows</div>';
  }
  out.innerHTML=html||'ok';
 }catch(e){out.textContent=String(e)}
}
function esc(s){return s.replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]))}
document.addEventListener('keydown',e=>{if(e.ctrlKey&&e.key==='Enter')run()});
</script>
"#;
