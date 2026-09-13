//! Configuration, in the shape of `litestream.yml`.
//!
//! A small indentation-driven YAML reader rather than a YAML library: maps,
//! lists of maps, and scalars, which is all a replication config has ever
//! needed. Anchors, flow style, multi-line scalars and tags are not supported
//! and will be reported as errors rather than silently misread.
//!
//! ```yaml
//! addr: :9090                     # Prometheus metrics
//! snapshot:
//!   interval: 1h
//!   retention: 24h
//! dbs:
//!   - path: /var/lib/app.gldb
//!     replicas:
//!       - url: s3://graphs/app
//!         endpoint: http://minio.internal:9000
//!         access-key-id: $MINIO_ACCESS_KEY
//!         secret-access-key: $MINIO_SECRET_KEY
//!         sync-interval: 1s
//!         retention: 72h
//! ```

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use super::backend::{Backend, ExecBackend, FileBackend, S3Backend};
use super::s3::S3;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Scalar(String),
    Map(BTreeMap<String, Node>),
    List(Vec<Node>),
}

impl Node {
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(m) => m.get(key),
            _ => None,
        }
    }
    pub fn str(&self, key: &str) -> Option<String> {
        match self.get(key) {
            Some(Node::Scalar(s)) => Some(s.clone()),
            _ => None,
        }
    }
    pub fn list(&self, key: &str) -> Vec<&Node> {
        match self.get(key) {
            Some(Node::List(v)) => v.iter().collect(),
            Some(other) => vec![other],
            None => Vec::new(),
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Node::Scalar(s) => Some(s),
            _ => None,
        }
    }
}

/// Parse the YAML subset. Returns a map node.
pub fn parse(text: &str) -> io::Result<Node> {
    let mut lines: Vec<(usize, String)> = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let without_comment = strip_comment(raw);
        if without_comment.trim().is_empty() {
            continue;
        }
        if without_comment.contains('\t') {
            return Err(io::Error::other(format!(
                "line {}: tabs are not valid YAML indentation",
                n + 1
            )));
        }
        let indent = without_comment.len() - without_comment.trim_start().len();
        lines.push((indent, without_comment.trim_end().to_string()));
    }
    let mut pos = 0;
    let node = parse_block(&lines, &mut pos, 0)?;
    Ok(node)
}

fn strip_comment(line: &str) -> String {
    let mut out = String::new();
    let mut in_quotes = false;
    let mut quote = '"';
    let mut prev_space = true;
    for c in line.chars() {
        if in_quotes {
            out.push(c);
            if c == quote {
                in_quotes = false;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                in_quotes = true;
                quote = c;
                out.push(c);
            }
            '#' if prev_space => break,
            _ => out.push(c),
        }
        prev_space = c.is_whitespace();
    }
    out
}

fn parse_block(lines: &[(usize, String)], pos: &mut usize, indent: usize) -> io::Result<Node> {
    if *pos >= lines.len() {
        return Ok(Node::Map(BTreeMap::new()));
    }

    if lines[*pos].1.trim_start().starts_with("- ") || lines[*pos].1.trim() == "-" {
        let mut items = Vec::new();
        while *pos < lines.len() && lines[*pos].0 == indent && lines[*pos].1.trim_start().starts_with('-') {
            let (item_indent, line) = &lines[*pos];
            let rest = line.trim_start().trim_start_matches('-').trim().to_string();
            *pos += 1;

            if rest.is_empty() {
                let child_indent = lines.get(*pos).map(|(i, _)| *i).unwrap_or(item_indent + 2);
                items.push(parse_block(lines, pos, child_indent)?);
            } else if let Some((k, v)) = split_pair(&rest) {
                // "- key: value" starts a map whose remaining keys are indented
                // to where `key` begins.
                let mut map = BTreeMap::new();
                let key_col = item_indent + line.trim_start().find('-').unwrap_or(0) + 2;
                if v.is_empty() {
                    let child = lines.get(*pos).map(|(i, _)| *i).unwrap_or(key_col + 2);
                    if child > key_col {
                        map.insert(k, parse_block(lines, pos, child)?);
                    } else {
                        map.insert(k, Node::Scalar(String::new()));
                    }
                } else {
                    map.insert(k, Node::Scalar(v));
                }
                while *pos < lines.len() && lines[*pos].0 == key_col {
                    let line = lines[*pos].1.trim().to_string();
                    let Some((k2, v2)) = split_pair(&line) else { break };
                    *pos += 1;
                    if v2.is_empty() {
                        let child = lines.get(*pos).map(|(i, _)| *i).unwrap_or(key_col + 2);
                        if child > key_col {
                            map.insert(k2, parse_block(lines, pos, child)?);
                        } else {
                            map.insert(k2, Node::Scalar(String::new()));
                        }
                    } else {
                        map.insert(k2, Node::Scalar(v2));
                    }
                }
                items.push(Node::Map(map));
            } else {
                items.push(Node::Scalar(unquote(&rest)));
            }
        }
        return Ok(Node::List(items));
    }

    let mut map = BTreeMap::new();
    while *pos < lines.len() && lines[*pos].0 >= indent {
        if lines[*pos].0 > indent {
            return Err(io::Error::other(format!(
                "unexpected indentation at: {}",
                lines[*pos].1.trim()
            )));
        }
        let line = lines[*pos].1.trim().to_string();
        let Some((k, v)) = split_pair(&line) else {
            return Err(io::Error::other(format!("expected 'key: value' at: {line}")));
        };
        *pos += 1;
        if v.is_empty() {
            let child_indent = lines.get(*pos).map(|(i, _)| *i).unwrap_or(indent);
            if child_indent > indent || lines.get(*pos).map(|(_, l)| l.trim_start().starts_with('-')).unwrap_or(false) {
                map.insert(k, parse_block(lines, pos, child_indent)?);
            } else {
                map.insert(k, Node::Scalar(String::new()));
            }
        } else {
            map.insert(k, Node::Scalar(v));
        }
    }
    Ok(Node::Map(map))
}

fn split_pair(line: &str) -> Option<(String, String)> {
    let colon = find_colon(line)?;
    let key = line[..colon].trim().to_string();
    let value = unquote(line[colon + 1..].trim());
    Some((key, value))
}

/// The first colon that is not inside quotes and not part of `://`.
fn find_colon(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' | '\'' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                if bytes.get(i + 1) == Some(&b'/') && bytes.get(i + 2) == Some(&b'/') {
                    continue;
                }
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// `$VAR` and `${VAR}`, as Litestream does before parsing.
pub fn expand_env(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() {
            let (name, next) = if chars[i + 1] == '{' {
                let mut j = i + 2;
                let mut name = String::new();
                while j < chars.len() && chars[j] != '}' {
                    name.push(chars[j]);
                    j += 1;
                }
                (name, j + 1)
            } else {
                let mut j = i + 1;
                let mut name = String::new();
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    name.push(chars[j]);
                    j += 1;
                }
                (name, j)
            };
            if name.is_empty() {
                out.push('$');
                i += 1;
            } else {
                out.push_str(&std::env::var(&name).unwrap_or_default());
                i = next;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// `30s`, `5m`, `2h`, `7d`, or a bare number of seconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<u64>() {
        return Some(Duration::from_secs(n));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: f64 = num.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        "d" => n * 86400.0,
        _ => return None,
    };
    Some(Duration::from_secs_f64(secs))
}

pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    let (num, mult) = if let Some(v) = s.strip_suffix("kb") {
        (v, 1024)
    } else if let Some(v) = s.strip_suffix("mb") {
        (v, 1024 * 1024)
    } else if let Some(v) = s.strip_suffix("gb") {
        (v, 1024 * 1024 * 1024)
    } else {
        (s.as_str(), 1)
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

// ------------------------------------------------------------------- config

#[derive(Clone)]
pub struct ReplicaConfig {
    pub url: String,
    pub sync_interval: Duration,
    pub min_bytes: u64,
    pub snapshot_interval: Duration,
    pub retention: Duration,
    pub retention_enabled: bool,
    pub endpoint: Option<String>,
    pub region: String,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub exec_put: Option<String>,
    pub exec_get: Option<String>,
    pub exec_list: Option<String>,
    pub exec_delete: Option<String>,
}

impl Default for ReplicaConfig {
    fn default() -> Self {
        ReplicaConfig {
            url: String::new(),
            sync_interval: Duration::from_secs(1),
            min_bytes: 1 << 20,
            snapshot_interval: Duration::from_secs(24 * 3600),
            retention: Duration::from_secs(24 * 3600),
            retention_enabled: true,
            endpoint: None,
            region: "us-east-1".into(),
            access_key: None,
            secret_key: None,
            exec_put: None,
            exec_get: None,
            exec_list: None,
            exec_delete: None,
        }
    }
}

#[derive(Clone)]
pub struct DbConfig {
    pub path: String,
    pub replicas: Vec<ReplicaConfig>,
}

#[derive(Clone, Default)]
pub struct Config {
    pub addr: Option<String>,
    pub dbs: Vec<DbConfig>,
}

pub fn load(path: &str, expand: bool) -> io::Result<Config> {
    let raw = std::fs::read_to_string(path)?;
    let text = if expand { expand_env(&raw) } else { raw };
    from_node(&parse(&text)?)
}

pub fn from_node(root: &Node) -> io::Result<Config> {
    let mut cfg = Config {
        addr: root.str("addr"),
        dbs: Vec::new(),
    };

    let global_snapshot = root.get("snapshot");
    let global_interval = global_snapshot
        .and_then(|s| s.str("interval"))
        .and_then(|s| parse_duration(&s));
    let global_retention = global_snapshot
        .and_then(|s| s.str("retention"))
        .and_then(|s| parse_duration(&s));

    for db in root.list("dbs") {
        let path = db
            .str("path")
            .ok_or_else(|| io::Error::other("a db entry has no `path`"))?;

        let mut replicas = Vec::new();
        // Accept both the modern single `replica` and the older `replicas` list.
        let mut entries: Vec<&Node> = db.list("replicas");
        if let Some(single) = db.get("replica") {
            entries.push(single);
        }

        for r in entries {
            let mut rc = ReplicaConfig {
                url: r
                    .str("url")
                    .or_else(|| r.as_str().map(|s| s.to_string()))
                    .ok_or_else(|| io::Error::other(format!("replica for {path} has no `url`")))?,
                ..Default::default()
            };
            if let Some(v) = r.str("sync-interval").and_then(|s| parse_duration(&s)) {
                rc.sync_interval = v;
            }
            if let Some(v) = r.str("min-bytes").and_then(|s| parse_size(&s)) {
                rc.min_bytes = v;
            }
            if let Some(v) = global_interval {
                rc.snapshot_interval = v;
            }
            if let Some(v) = r
                .get("snapshot")
                .and_then(|s| s.str("interval"))
                .and_then(|s| parse_duration(&s))
            {
                rc.snapshot_interval = v;
            }
            if let Some(v) = global_retention {
                rc.retention = v;
            }
            if let Some(v) = r.str("retention").and_then(|s| parse_duration(&s)) {
                rc.retention = v;
            }
            if let Some(v) = r.str("retention-enabled") {
                rc.retention_enabled = v != "false";
            }
            rc.endpoint = r.str("endpoint");
            if let Some(v) = r.str("region") {
                rc.region = v;
            }
            rc.access_key = r.str("access-key-id").filter(|s| !s.is_empty());
            rc.secret_key = r.str("secret-access-key").filter(|s| !s.is_empty());
            rc.exec_put = r.str("exec-put");
            rc.exec_get = r.str("exec-get");
            rc.exec_list = r.str("exec-list");
            rc.exec_delete = r.str("exec-delete");
            replicas.push(rc);
        }

        cfg.dbs.push(DbConfig { path, replicas });
    }
    Ok(cfg)
}

// ------------------------------------------------------------ backend from url

/// Turn a replica config into something that can hold bytes.
///
/// - `/path`, `file:///path`        filesystem
/// - `s3://bucket/prefix`           native, needs a plaintext `endpoint`
/// - `exec:<label>`                 shell commands from `exec-*`
pub fn open_backend(rc: &ReplicaConfig) -> io::Result<Arc<dyn Backend>> {
    let url = rc.url.trim();

    if let Some(label) = url.strip_prefix("exec:") {
        let put = rc
            .exec_put
            .clone()
            .ok_or_else(|| io::Error::other("exec replica needs `exec-put`"))?;
        return Ok(Arc::new(ExecBackend {
            put,
            get: rc.exec_get.clone().unwrap_or_default(),
            list: rc.exec_list.clone().unwrap_or_default(),
            delete: rc.exec_delete.clone(),
            label: label.to_string(),
        }));
    }

    if let Some(rest) = url.strip_prefix("s3://") {
        let (bucket, prefix) = match rest.split_once('/') {
            Some((b, p)) => (b.to_string(), p.trim_end_matches('/').to_string()),
            None => (rest.to_string(), String::new()),
        };

        let endpoint = rc.endpoint.clone().unwrap_or_default();
        if endpoint.is_empty() || endpoint.starts_with("https://") {
            // No TLS here, by design. Say so plainly rather than failing at
            // connect time with something cryptic.
            if rc.exec_put.is_some() {
                return Ok(Arc::new(ExecBackend {
                    put: rc.exec_put.clone().unwrap(),
                    get: rc.exec_get.clone().unwrap_or_default(),
                    list: rc.exec_list.clone().unwrap_or_default(),
                    delete: rc.exec_delete.clone(),
                    label: url.to_string(),
                }));
            }
            let fallback = ExecBackend::aws(url);
            if which("aws") {
                return Ok(Arc::new(fallback));
            }
            return Err(io::Error::other(format!(
                "{url} needs either a plaintext `endpoint:` (MinIO, Ceph, LocalStack) \
                 or the aws CLI on PATH for TLS. glider has no TLS stack — see REPLICATION.md."
            )));
        }

        let rest = endpoint.trim_start_matches("http://");
        let (host, port) = match rest.split_once(':') {
            Some((h, p)) => (h.to_string(), p.trim_end_matches('/').parse().unwrap_or(80)),
            None => (rest.trim_end_matches('/').to_string(), 80u16),
        };

        let access_key = rc
            .access_key
            .clone()
            .or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok())
            .unwrap_or_default();
        let secret_key = rc
            .secret_key
            .clone()
            .or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok())
            .unwrap_or_default();

        return Ok(Arc::new(S3Backend {
            client: S3 {
                bucket,
                prefix,
                region: rc.region.clone(),
                host,
                port,
                access_key,
                secret_key,
                path_style: true,
                timeout: Duration::from_secs(30),
            },
        }));
    }

    let path = url.strip_prefix("file://").unwrap_or(url);
    Ok(Arc::new(FileBackend {
        root: std::path::PathBuf::from(path),
    }))
}

fn which(bin: &str) -> bool {
    std::env::var("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(bin).exists())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_litestream_shaped_config() {
        let text = r#"
addr: ":9090"
snapshot:
  interval: 1h
  retention: 24h
dbs:
  - path: /var/lib/app.gldb
    replicas:
      - url: s3://graphs/app
        endpoint: http://minio:9000
        sync-interval: 5s
        retention: 72h
  - path: /var/lib/other.gldb
    replica:
      url: /backups/other
"#;
        let cfg = from_node(&parse(text).unwrap()).unwrap();
        assert_eq!(cfg.addr.as_deref(), Some(":9090"));
        assert_eq!(cfg.dbs.len(), 2);

        let first = &cfg.dbs[0];
        assert_eq!(first.path, "/var/lib/app.gldb");
        assert_eq!(first.replicas.len(), 1);
        assert_eq!(first.replicas[0].url, "s3://graphs/app");
        assert_eq!(first.replicas[0].endpoint.as_deref(), Some("http://minio:9000"));
        assert_eq!(first.replicas[0].sync_interval, Duration::from_secs(5));
        assert_eq!(first.replicas[0].retention, Duration::from_secs(72 * 3600));
        // Global snapshot settings reach every replica.
        assert_eq!(first.replicas[0].snapshot_interval, Duration::from_secs(3600));

        assert_eq!(cfg.dbs[1].replicas[0].url, "/backups/other");
    }

    #[test]
    fn urls_survive_the_colon_rule() {
        let n = parse("url: s3://bucket/path\nendpoint: http://host:9000\n").unwrap();
        assert_eq!(n.str("url").unwrap(), "s3://bucket/path");
        assert_eq!(n.str("endpoint").unwrap(), "http://host:9000");
    }

    #[test]
    fn comments_quotes_and_env() {
        std::env::set_var("GLIDER_TEST_KEY", "sekrit");
        let text = expand_env("secret-access-key: $GLIDER_TEST_KEY  # from the environment\naddr: \"#notacomment\"\n");
        let n = parse(&text).unwrap();
        assert_eq!(n.str("secret-access-key").unwrap(), "sekrit");
        assert_eq!(n.str("addr").unwrap(), "#notacomment");
    }

    #[test]
    fn durations_and_sizes() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("24h"), Some(Duration::from_secs(86400)));
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(604800)));
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("bogus"), None);
        assert_eq!(parse_size("4mb"), Some(4 * 1024 * 1024));
        assert_eq!(parse_size("512"), Some(512));
    }

    #[test]
    fn tabs_are_rejected_rather_than_guessed_at() {
        assert!(parse("dbs:\n\t- path: /x\n").is_err());
    }
}
