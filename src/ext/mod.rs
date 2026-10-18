//! Extensions: optional, namespaced bundles of procedures and functions that
//! a database opts into, the way PostgreSQL databases opt into `timescaledb`
//! or `vector`.
//!
//! ```text
//! CREATE EXTENSION context                 install into this database
//! CREATE EXTENSION IF NOT EXISTS context
//! SHOW EXTENSIONS                          what is available, what is installed
//! CALL context.help()                      an extension's procedures and functions
//! CALL context.retrieve(query: "ada lovelace", budget: 800)
//! MATCH (p:Person) RETURN context.card(p)  extension functions inside queries
//! ALTER EXTENSION context UPDATE           record the running code's version
//! DROP EXTENSION context
//! ```
//!
//! The code for an extension is compiled in, as everything in glider is: no
//! dynamic loading, no unsafe, no dependencies. What is per database is
//! whether it is *installed*: `CREATE EXTENSION` writes a catalog entry
//! through the write-ahead log, so it survives recovery, replicates, and a
//! database that never installs an extension never pays for or exposes it.
//!
//! Embedders add their own with [`register`]: implement [`Extension`] and
//! its procedures become `CALL <name>.<procedure>(...)` and its functions
//! `<name>.<function>(...)` in any expression, in every database that
//! installs it.

pub mod context;
pub mod text;

use std::sync::{Arc, OnceLock, RwLock};

use crate::graph::{Error, Graph, Result};
use crate::query::QueryResult;
use crate::value::Value;

/// One procedure an extension offers, for `CALL <ext>.help()` and errors.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub name: &'static str,
    /// The argument list as a user types it, e.g. `"(seeds, budget, depth)"`.
    pub args: &'static str,
    pub doc: &'static str,
}

/// An argument to an extension function: a bound node or edge, so the
/// function can read the graph, or any other value.
#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    Node(u64),
    Edge(u64),
    Value(Value),
}

/// A node as a write hook sees it, before or after the write.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeImage {
    pub labels: Vec<String>,
    pub props: Vec<(String, Value)>,
}

/// What an extension implements. Only `name`, `version`, `summary`,
/// `procedures` and `call` are required.
pub trait Extension: Send + Sync {
    /// The namespace: lowercase, `[a-z0-9_]`.
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    /// One line for `SHOW EXTENSIONS`.
    fn summary(&self) -> &str;
    fn procedures(&self) -> &[Spec];
    fn functions(&self) -> &[Spec] {
        &[]
    }
    /// Run `CALL <name>.<procedure>(args)`. The procedure is one listed by
    /// [`procedures`](Extension::procedures). Runs inside the statement's
    /// transaction: writes commit with it or roll back with it.
    fn call(&self, g: &mut Graph, procedure: &str, args: &Args) -> Result<QueryResult>;
    /// Evaluate `<name>.<function>(args)` for one row.
    fn eval(&self, _g: &Graph, _function: &str, _args: &[Arg]) -> Value {
        Value::Null
    }
    /// Whether [`on_node_write`](Extension::on_node_write) should run in
    /// this database. Asked again after the extension writes configuration
    /// (`!` keys) and after installs and rollbacks.
    fn wants_writes(&self, _g: &Graph) -> bool {
        false
    }
    /// Called after every node write made through the graph API (queries
    /// included), in the same transaction, with the node before and after
    /// (`None`: absent). Writes it makes — usually [`Graph::ext_write`] —
    /// are logged like any other, so recovery and replicas replay them
    /// without running the hook again. Bulk loads do not call it; an
    /// extension rebuilds from a scan instead.
    fn on_node_write(&self, _g: &mut Graph, _id: u64, _before: Option<&NodeImage>, _after: Option<&NodeImage>) -> Result<()> {
        Ok(())
    }
    /// Run once by `CREATE EXTENSION`, in the same transaction that records
    /// the install: create indexes, seed configuration.
    fn install(&self, _g: &mut Graph) -> Result<()> {
        Ok(())
    }
    /// Run by `DROP EXTENSION` before the install record is removed.
    fn uninstall(&self, _g: &mut Graph) -> Result<()> {
        Ok(())
    }
    /// Run by `ALTER EXTENSION <name> UPDATE` when the database recorded an
    /// older version than this code.
    fn upgrade(&self, _g: &mut Graph, _from: &str) -> Result<()> {
        Ok(())
    }
}

fn registry() -> &'static RwLock<Vec<Arc<dyn Extension>>> {
    static REGISTRY: OnceLock<RwLock<Vec<Arc<dyn Extension>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(vec![Arc::new(context::Context) as Arc<dyn Extension>]))
}

/// Make an extension available to every graph in this process. One with the
/// same name is replaced. Databases still have to `CREATE EXTENSION` it.
pub fn register(ext: Arc<dyn Extension>) {
    let mut list = registry().write().unwrap_or_else(|e| e.into_inner());
    list.retain(|e| e.name() != ext.name());
    list.push(ext);
    list.sort_by(|a, b| a.name().cmp(b.name()));
}

/// Every extension compiled in or registered, by name.
pub fn available() -> Vec<Arc<dyn Extension>> {
    registry().read().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn find(name: &str) -> Option<Arc<dyn Extension>> {
    let name = name.to_ascii_lowercase();
    available().into_iter().find(|e| e.name() == name)
}

fn names() -> String {
    available().iter().map(|e| e.name().to_string()).collect::<Vec<_>>().join(", ")
}

/// The extension a qualified name belongs to, checked to be installed in
/// this database. `context.retrieve` → the `context` extension.
pub fn resolve(g: &Graph, qualified: &str) -> Result<(Arc<dyn Extension>, String)> {
    let (ns, member) = qualified
        .split_once('.')
        .ok_or_else(|| Error::Msg(format!("'{qualified}' is not an extension member")))?;
    let ext = find(ns).ok_or_else(|| {
        Error::Msg(format!("no extension named '{ns}' (available: {})", names()))
    })?;
    if g.extension_version(ext.name()).is_none() {
        return Err(Error::Msg(format!(
            "extension '{}' is not installed in this database; run CREATE EXTENSION {}",
            ext.name(),
            ext.name()
        )));
    }
    Ok((ext, member.to_ascii_lowercase()))
}

/// Check, before a statement runs, that every extension function it uses
/// exists and is installed: a typo must be an error, not a column of nulls.
pub fn check_functions(g: &Graph, qualified: &[String]) -> Result<()> {
    for q in qualified {
        let (ext, f) = resolve(g, q)?;
        if !ext.functions().iter().any(|s| s.name == f) {
            return Err(Error::Msg(format!(
                "extension '{}' has no function '{f}'; CALL {}.help() lists them",
                ext.name(),
                ext.name()
            )));
        }
    }
    Ok(())
}

/// `CALL <ext>.<procedure>(...)`.
pub fn call(g: &mut Graph, qualified: &str, args: &[(String, Value)]) -> Result<QueryResult> {
    let (ext, proc) = resolve(g, qualified)?;
    if proc == "help" {
        return Ok(help(ext.as_ref()));
    }
    if !ext.procedures().iter().any(|s| s.name == proc) {
        return Err(Error::Msg(format!(
            "extension '{}' has no procedure '{proc}'; CALL {}.help() lists them",
            ext.name(),
            ext.name()
        )));
    }
    ext.call(g, &proc, &Args(args.to_vec()))
}

/// Evaluate an extension function. Installation was checked when the
/// statement started.
pub fn eval(g: &Graph, qualified: &str, args: &[Arg]) -> Value {
    match resolve(g, qualified) {
        Ok((ext, f)) => ext.eval(g, &f, args),
        Err(_) => Value::Null,
    }
}

fn help(ext: &dyn Extension) -> QueryResult {
    let mut rows = Vec::new();
    for (kind, list) in [("procedure", ext.procedures()), ("function", ext.functions())] {
        for s in list {
            rows.push(vec![
                Value::Text(kind.into()),
                Value::Text(format!("{}.{}{}", ext.name(), s.name, s.args)),
                Value::Text(s.doc.into()),
            ]);
        }
    }
    let mut r = QueryResult::table(vec!["kind".into(), "signature".into(), "description".into()], rows);
    r.message = Some(format!("{} {}: {}", ext.name(), ext.version(), ext.summary()));
    r
}

/// `SHOW EXTENSIONS`: everything available, and what this database has.
pub fn show(g: &Graph) -> QueryResult {
    let installed = g.extensions();
    let mut rows = Vec::new();
    for e in available() {
        let v = installed.iter().find(|(n, _)| n == e.name()).map(|(_, v)| v.clone());
        rows.push(vec![
            Value::Text(e.name().into()),
            Value::Text(e.version().into()),
            v.map(Value::Text).unwrap_or(Value::Null),
            Value::Text(e.summary().into()),
        ]);
    }
    // Installed in this database but not compiled into this binary: say so
    // rather than hide it.
    for (n, v) in installed {
        if find(&n).is_none() {
            rows.push(vec![
                Value::Text(n),
                Value::Null,
                Value::Text(v),
                Value::Text("not available in this build".into()),
            ]);
        }
    }
    QueryResult::table(
        vec!["name".into(), "available".into(), "installed".into(), "summary".into()],
        rows,
    )
}

pub fn create(g: &mut Graph, name: &str, if_not_exists: bool) -> Result<QueryResult> {
    let ext = find(name).ok_or_else(|| {
        Error::Msg(format!("no extension named '{name}' (available: {})", names()))
    })?;
    if let Some(v) = g.extension_version(ext.name()) {
        if if_not_exists {
            return Ok(QueryResult::message(format!("extension {} {v} is already installed", ext.name())));
        }
        return Err(Error::Msg(format!("extension {} is already installed (version {v})", ext.name())));
    }
    ext.install(g)?;
    g.add_extension(ext.name(), ext.version())?;
    let mut r = QueryResult::message(format!(
        "installed {} {}: {}. CALL {}.help() lists what it adds",
        ext.name(),
        ext.version(),
        ext.summary(),
        ext.name()
    ));
    r.touched = 1;
    Ok(r)
}

pub fn drop(g: &mut Graph, name: &str, if_exists: bool) -> Result<QueryResult> {
    let name = name.to_ascii_lowercase();
    if g.extension_version(&name).is_none() {
        if if_exists {
            return Ok(QueryResult::message(format!("extension {name} is not installed")));
        }
        return Err(Error::Msg(format!("extension {name} is not installed")));
    }
    if let Some(ext) = find(&name) {
        ext.uninstall(g)?;
    }
    g.drop_extension(&name)?;
    let mut r = QueryResult::message(format!("dropped extension {name}"));
    r.touched = 1;
    Ok(r)
}

pub fn update(g: &mut Graph, name: &str) -> Result<QueryResult> {
    let ext = find(name).ok_or_else(|| Error::Msg(format!("no extension named '{name}'")))?;
    let Some(from) = g.extension_version(ext.name()) else {
        return Err(Error::Msg(format!("extension {} is not installed", ext.name())));
    };
    if from == ext.version() {
        return Ok(QueryResult::message(format!("extension {} is already at {from}", ext.name())));
    }
    ext.upgrade(g, &from)?;
    g.add_extension(ext.name(), ext.version())?;
    Ok(QueryResult::message(format!("updated {} from {from} to {}", ext.name(), ext.version())))
}

// ------------------------------------------------------------------ arguments

/// Named arguments to a procedure: `CALL x.y(key: value, ...)`. Keys are
/// lowercase.
#[derive(Clone, Debug, Default)]
pub struct Args(pub Vec<(String, Value)>);

impl Args {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn f64(&self, key: &str, default: f64) -> f64 {
        self.get(key).and_then(|v| v.as_f64()).unwrap_or(default)
    }
    pub fn usize(&self, key: &str, default: usize) -> usize {
        self.get(key).and_then(|v| v.as_i64()).map(|v| v.max(0) as usize).unwrap_or(default)
    }
    pub fn str(&self, key: &str) -> Option<String> {
        match self.get(key)? {
            Value::Null => None,
            v => Some(v.to_string()),
        }
    }
    pub fn bool(&self, key: &str, default: bool) -> bool {
        self.get(key).map(|v| v.truthy()).unwrap_or(default)
    }
    /// A string or a list of strings, as a list.
    pub fn strs(&self, key: &str) -> Vec<String> {
        match self.get(key) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::List(items)) => items.iter().map(|v| v.to_string()).collect(),
            Some(v) => vec![v.to_string()],
        }
    }
    /// Node ids from an int, a list of them, or the `n12` handles that
    /// extension output uses, so a model can pass back what it was shown.
    pub fn ids(&self, key: &str) -> Result<Vec<u64>> {
        fn one(v: &Value) -> Option<u64> {
            match v {
                Value::Int(i) if *i >= 0 => Some(*i as u64),
                Value::Float(f) if *f >= 0.0 && f.fract() == 0.0 => Some(*f as u64),
                Value::Text(s) => {
                    let s = s.trim();
                    s.strip_prefix('n').unwrap_or(s).parse().ok()
                }
                _ => None,
            }
        }
        let bad = |v: &Value| Error::Msg(format!("{key}: expected node ids, got {v}"));
        match self.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::List(items)) => items.iter().map(|v| one(v).ok_or_else(|| bad(v))).collect(),
            Some(v) => Ok(vec![one(v).ok_or_else(|| bad(v))?]),
        }
    }
}
