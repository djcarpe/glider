//! The native OTLP path end to end: a fake collector on a local socket, the
//! exporter pointed at it, statements run with and without a trace context,
//! and the HTTP server's spans and /metrics. One test, because the exporter
//! is process-wide and installs once.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use glider::telemetry::{self, otlp, TraceContext};
use glider::Graph;

/// Accept POSTs forever, sending `(path, body)` for each and answering 200.
fn collector() -> (String, mpsc::Receiver<(String, String)>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let mut s = s.unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut len = 0;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
            let _ = tx.send((path, String::from_utf8(body).unwrap()));
        }
    });
    (url, rx)
}

/// Everything the collector received within `wait`.
fn drain(rx: &mpsc::Receiver<(String, String)>, wait: Duration) -> Vec<(String, String)> {
    let mut out = Vec::new();
    while let Ok(m) = rx.recv_timeout(wait) {
        out.push(m);
    }
    out
}

fn http(addr: &str, req: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn statements_and_requests_reach_an_otlp_collector() {
    let (url, rx) = collector();
    let vars = [
        ("OTEL_EXPORTER_OTLP_ENDPOINT", url.clone()),
        ("OTEL_SERVICE_NAME", "glider-test".to_string()),
        (
            "OTEL_RESOURCE_ATTRIBUTES",
            "deployment.environment=test".to_string(),
        ),
        // Long intervals: the test drives pushes with flush().
        ("OTEL_METRIC_EXPORT_INTERVAL", "600000".to_string()),
        ("OTEL_BSP_SCHEDULE_DELAY", "600000".to_string()),
    ];
    let cfg = otlp::Config::from_vars("glider", |k| {
        vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
    })
    .unwrap()
    .unwrap();
    otlp::install(cfg);

    // A statement under a caller's trace context is its child...
    let mut g = Graph::memory();
    let parent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    telemetry::set_context(TraceContext::parse(parent));
    glider::query(
        "CREATE (:Person {name:'Ada'})-[:KNOWS]->(:Person {name:'Bob'})",
        &mut g,
    )
    .unwrap();
    let r = glider::query("MATCH (p:Person) RETURN p.name", &mut g).unwrap();
    assert_eq!(r.rows.len(), 2);
    telemetry::set_context(None);

    // ...the thread's report describes the last one...
    let last = telemetry::last_op().unwrap();
    assert_eq!(last.op, "MATCH");
    assert_eq!(last.rows, 2);
    assert!(last.duration_ns.is_some());

    // ...and a failure is recorded as one.
    assert!(glider::query("MATCH (n RETURN n", &mut g).is_err());
    assert_eq!(telemetry::last_op().unwrap().op, "INVALID");
    assert!(telemetry::last_op().unwrap().error.is_some());

    otlp::flush();
    let got = drain(&rx, Duration::from_millis(500));
    let traces: Vec<&String> = got
        .iter()
        .filter(|(p, _)| p == "/v1/traces")
        .map(|(_, b)| b)
        .collect();
    let metrics: Vec<&String> = got
        .iter()
        .filter(|(p, _)| p == "/v1/metrics")
        .map(|(_, b)| b)
        .collect();
    assert_eq!(traces.len(), 1, "one span batch: {got:?}");
    assert_eq!(metrics.len(), 1, "one metrics push: {got:?}");
    let t = traces[0];
    assert!(t.contains("\"stringValue\":\"glider-test\""));
    assert!(t.contains("\"name\":\"glider MATCH\""));
    assert!(t.contains("\"name\":\"glider CREATE\""));
    assert!(t.contains("\"name\":\"glider INVALID\""));
    assert!(t.contains("\"traceId\":\"4bf92f3577b34da6a3ce929d0e0e4736\""));
    assert!(t.contains("\"parentSpanId\":\"00f067aa0ba902b7\""));
    assert!(t.contains(
        "\"key\":\"db.query.text\",\"value\":{\"stringValue\":\"MATCH (p:Person) RETURN p.name\"}"
    ));
    assert!(t.contains("\"status\":{\"code\":2"));
    let m = metrics[0];
    assert!(m.contains("\"name\":\"glider.queries\""));
    assert!(m.contains("\"name\":\"glider.query.duration\""));
    assert!(m.contains(&format!("\"stringValue\":\":memory:{}\"", g.telemetry_id())));
    assert!(m.contains("\"name\":\"glider.db.nodes\""));
    assert!(m.contains("\"deployment.environment\""));

    // A closed database stops being reported.
    let id = g.telemetry_id();
    drop(g);
    otlp::flush();
    let got = drain(&rx, Duration::from_millis(500));
    assert!(got
        .iter()
        .all(|(_, b)| !b.contains(&format!(":memory:{id}\""))));

    // The server: an incoming traceparent parents the request span, which
    // parents the statement spans; /metrics serves Prometheus text.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    std::thread::spawn(move || glider::server::serve_on(l, Graph::memory()));
    let q = "CREATE (:City {name:'Oslo'})";
    let resp = http(
        &addr,
        &format!(
            "POST /api/query HTTP/1.1\r\nHost: x\r\ntraceparent: 00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01\r\nContent-Length: {}\r\n\r\n{q}",
            q.len()
        ),
    );
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    let resp = http(&addr, "GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        resp.contains("# TYPE glider_queries_total counter"),
        "{resp}"
    );
    assert!(
        resp.contains("glider_db_nodes{glider_db=\":memory:"),
        "{resp}"
    );

    otlp::flush();
    let got = drain(&rx, Duration::from_millis(500));
    let t = got
        .iter()
        .find(|(p, _)| p == "/v1/traces")
        .map(|(_, b)| b.clone())
        .expect("server spans");
    assert!(t.contains("\"name\":\"POST /api/query\""), "{t}");
    assert!(t.contains("\"parentSpanId\":\"b7ad6b7169203331\""));
    assert!(t.contains("\"traceId\":\"0af7651916cd43dd8448eb211c80319c\""));
    assert!(t.contains("\"name\":\"glider CREATE\""));
    // The statement's parent is the request span, not the remote caller.
    let server_span = t
        .split("\"name\":\"POST /api/query\"")
        .next()
        .unwrap()
        .rsplit("\"spanId\":\"")
        .next()
        .unwrap()[..16]
        .to_string();
    assert!(
        t.contains(&format!("\"parentSpanId\":\"{server_span}\"")),
        "{t}"
    );
    assert_eq!(otlp::installed().unwrap().failures(), 0);
}
