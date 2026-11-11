//! Disk-spooled cursors, bounded import records and transactional write slices.
use super::*;
use crate::codec;
use std::fs::{File, OpenOptions as FileOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct TempFile {
    path: PathBuf,
}
impl TempFile {
    fn new(dir: &std::path::Path) -> Result<(Self, File)> {
        std::fs::create_dir_all(dir)?;
        loop {
            let path = dir.join(format!(
                "bounded-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match FileOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(f) => return Ok((Self { path }, f)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A stable snapshot of result values spooled on disk, with bounded batches.
/// Plain RETURN streams matches directly. Sorting, DISTINCT and aggregation
/// first run through the work_mem bounded projection. No graph lock is retained
/// between fetches; dropping the cursor removes its private spool file.
pub struct Cursor {
    reader: BufReader<File>,
    _file: TempFile,
    columns: Vec<String>,
    remaining: usize,
    limits: QueryLimits,
}
impl Cursor {
    pub fn open(
        g: &mut Graph,
        src: &str,
        params: &[(String, Value)],
        limits: QueryLimits,
    ) -> Result<Self> {
        if limits.max_rows == 0 || limits.max_bytes < 128 || limits.max_bytes > g.query_memory() {
            return Err(budget_error());
        }
        let (temp, file) = TempFile::new(&g.temp_dir())?;
        let mut writer = BufWriter::new(file);
        let mut remaining = 0;
        let mut emit = |row: Vec<Value>| -> Result<()> {
            let charge = row_bytes(&row);
            if charge > limits.max_bytes {
                return Err(budget_error());
            }
            let mut bytes = Vec::new();
            codec::put_value(&mut bytes, &Value::List(row));
            writer.write_all(&charge.to_le_bytes())?;
            writer.write_all(&(bytes.len() as u64).to_le_bytes())?;
            writer.write_all(&bytes)?;
            remaining += 1;
            Ok(())
        };
        let (stmt, funcs) = parse_checked(src, params)?;
        crate::ext::check_functions(g, &funcs)?;
        let columns = match &stmt {
            Stmt::Match {
                patterns,
                filter,
                tail:
                    Tail::Return {
                        kind,
                        distinct: false,
                        order,
                        skip,
                        limit,
                    },
            } if order.is_empty()
                && !matches!(kind, ReturnKind::Items(items) if items.iter().any(|(e,_)| e.is_aggregate())) =>
            {
                let mut projection = Projection::new(kind, false, order, 0, None, g.query_memory());
                let mut seen = 0usize;
                let mut taken = 0usize;
                match_all(g, patterns, 0, Vec::new(), filter.as_ref(), &mut |binds| {
                    projection.resolve(Some(&binds))?;
                    if limit.map(|n| taken >= n).unwrap_or(false) {
                        return Ok(false);
                    }
                    seen += 1;
                    if seen <= *skip {
                        return Ok(true);
                    }
                    let items = projection.items.as_ref().expect("resolved");
                    let row = items.iter().map(|(e, _)| eval(g, e, &binds)).collect();
                    emit(row)?;
                    taken += 1;
                    Ok(true)
                })?;
                projection.resolve(None)?;
                projection
                    .items
                    .unwrap()
                    .into_iter()
                    .map(|(_, name)| name)
                    .collect()
            }
            Stmt::Match {
                tail: Tail::Return { .. },
                ..
            } => {
                let result = execute_stmt(g, stmt)?;
                for row in result.rows {
                    emit(row)?;
                }
                result.columns
            }
            _ => {
                return Err(Error::Msg(
                    "cursors require a read-only MATCH RETURN".into(),
                ))
            }
        };
        if let Some(e) = g.integrity_error() {
            return Err(Error::Msg(e));
        }
        writer.flush()?;
        let mut file = writer.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        file.seek(SeekFrom::Start(0))?;
        Ok(Self {
            reader: BufReader::new(file),
            _file: temp,
            columns,
            remaining,
            limits,
        })
    }

    pub fn batch_workspace(&self) -> u64 {
        self.limits.max_bytes.saturating_mul(4)
    }

    pub fn next_batch(&mut self) -> Result<(QueryResult, bool)> {
        let mut rows = Vec::new();
        let mut used = 0u64;
        while self.remaining > 0 && rows.len() < self.limits.max_rows {
            let pos = self.reader.stream_position()?;
            let mut header = [0u8; 8];
            self.reader.read_exact(&mut header)?;
            let charge = u64::from_le_bytes(header);
            if used.saturating_add(charge) > self.limits.max_bytes {
                self.reader.seek(SeekFrom::Start(pos))?;
                break;
            }
            self.reader.read_exact(&mut header)?;
            let len = u64::from_le_bytes(header);
            if len > self.limits.max_bytes {
                return Err(budget_error());
            }
            let mut bytes = vec![0; len as usize];
            self.reader.read_exact(&mut bytes)?;
            let Value::List(row) = codec::Reader::new(&bytes).value().map_err(Error::Msg)? else {
                return Err(Error::Msg("invalid cursor spool".into()));
            };
            rows.push(row);
            self.remaining -= 1;
            used += charge;
        }
        Ok((
            QueryResult::table(self.columns.clone(), rows),
            self.remaining == 0,
        ))
    }
}

/// One atomic slice, with no statement-level autocommits. Call again between
/// slices to release the NIF mutex and let reads through. Errors roll back the
/// entire slice. Explicit transaction controls are rejected before any writes.
pub fn execute_batch(
    g: &mut Graph,
    statements: &[(String, Vec<(String, Value)>)],
) -> Result<usize> {
    if !g.autocommit || g.uncommitted() > 0 {
        return Err(Error::Msg(
            "batch cannot join an existing transaction".into(),
        ));
    }
    if statements.len() > 1000 {
        return Err(budget_error());
    }
    let bytes = statements
        .iter()
        .map(|(s, p)| {
            s.len() as u64
                + p.iter()
                    .map(|(k, v)| k.len() as u64 + v.memory_bytes())
                    .sum::<u64>()
        })
        .sum::<u64>();
    if bytes.saturating_mul(4) > g.query_memory() {
        return Err(budget_error());
    }
    let mut parsed = Vec::new();
    for (src, params) in statements {
        let (stmt, funcs) = parse_checked(src, params)?;
        crate::ext::check_functions(g, &funcs)?;
        match stmt {
            Stmt::Create(_)
            | Stmt::Match {
                tail: Tail::Set(_) | Tail::Remove(_) | Tail::Delete(..) | Tail::Create(_),
                ..
            } => parsed.push(stmt),
            _ => {
                return Err(Error::Msg(
                    "batch accepts only CREATE and MATCH writes".into(),
                ))
            }
        }
    }
    g.autocommit = false;
    let result = (|| {
        for stmt in parsed {
            execute_stmt(g, stmt)?;
        }
        g.commit()?;
        Ok(statements.len())
    })();
    if result.is_err() {
        let _ = g.rollback();
    }
    g.autocommit = true;
    result
}

/// Streaming JSONL import. The id/key remap is a disk B+tree, never a graph-sized
/// HashMap. Records are bounded before reading, and commit in <=1000-op slices.
/// A malformed record rolls back its current slice; prior slices stay committed.
/// Returns (nodes, edges). Nodes must precede edges that reference their keys.
pub fn import_jsonl_from(
    g: &mut Graph,
    reader: &mut impl BufRead,
    max_record_bytes: u64,
) -> Result<(usize, usize)> {
    if !g.autocommit || max_record_bytes == 0 || max_record_bytes > g.query_memory() / 8 {
        return Err(budget_error());
    }
    let (temp, file) = TempFile::new(&g.temp_dir())?;
    drop(file);
    let cfg = crate::storage::pager::Config {
        cache_bytes: 1 << 20,
        ..Default::default()
    };
    let map = crate::storage::pager::Pager::create(&temp.path, &cfg, [0; 16])?;
    let tree = crate::storage::btree::Tree { slot: 0, id: 1 };
    let mut nodes = 0usize;
    let mut edges = 0usize;
    g.autocommit = false;
    let result = (|| {
        loop {
            let mut line = Vec::new();
            let n = (&mut *reader)
                .take(max_record_bytes + 1)
                .read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            if n as u64 > max_record_bytes {
                return Err(budget_error());
            }
            let line = std::str::from_utf8(&line)
                .map_err(|e| Error::Msg(e.to_string()))?
                .trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let j = parse_json(line).map_err(Error::Msg)?;
            let props = match j.get("props") {
                Some(crate::value::Json::Object(fields)) => fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_value()))
                    .collect(),
                _ => Vec::new(),
            };
            let kind = j.get("type").and_then(|v| v.as_str()).unwrap_or("node");
            if matches!(kind, "edge" | "rel" | "relationship") {
                let endpoint = |key| -> Result<u64> {
                    let v = j
                        .get(key)
                        .ok_or_else(|| Error::Msg(format!("missing {key}")))?;
                    let key = v
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                        .ok_or_else(|| Error::Msg("bad endpoint".into()))?;
                    tree.get(&map, key.as_bytes())?
                        .and_then(|b| b.as_slice().try_into().ok().map(u64::from_le_bytes))
                        .or_else(|| v.as_u64())
                        .ok_or_else(|| Error::Msg(format!("unknown endpoint {key}")))
                };
                let from = endpoint("from")?;
                let to = endpoint("to")?;
                let etype = j
                    .get("label")
                    .or_else(|| j.get("etype"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("RELATED");
                g.add_edge(from, to, etype, props)?;
                edges += 1;
            } else {
                let labels = match j.get("labels") {
                    Some(crate::value::Json::Array(a)) => a
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect(),
                    _ => j
                        .get("label")
                        .and_then(|v| v.as_str())
                        .map(|l| vec![l.to_owned()])
                        .unwrap_or_default(),
                };
                let id = g.add_node(&labels, props)?;
                nodes += 1;
                if let Some(k) = j.get("key").and_then(|v| v.as_str()) {
                    tree.put(&map, k.as_bytes(), &id.to_le_bytes())?;
                }
                if let Some(id_key) = j.get("id").and_then(|v| v.as_u64()) {
                    tree.put(&map, id_key.to_string().as_bytes(), &id.to_le_bytes())?;
                }
            }
            if (nodes + edges) % 1000 == 0 {
                g.commit()?;
                map.commit();
                map.checkpoint(0)?;
            }
        }
        g.commit()?;
        Ok((nodes, edges))
    })();
    if result.is_err() {
        let _ = g.rollback();
    }
    g.autocommit = true;
    drop(map);
    result
}
