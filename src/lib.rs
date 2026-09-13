//! glider — an embeddable property-graph database in a single binary.
//!
//! Use it as a library:
//!
//! ```no_run
//! use glider::{Graph, Sync, Value};
//!
//! let mut g = Graph::open(std::path::Path::new("social.gldb"), Sync::Normal).unwrap();
//! let ada = g.add_node(&["Person".into()], vec![("name".into(), Value::from("Ada"))]).unwrap();
//! let bob = g.add_node(&["Person".into()], vec![("name".into(), Value::from("Bob"))]).unwrap();
//! g.add_edge(ada, bob, "KNOWS", vec![]).unwrap();
//!
//! let result = glider::query("MATCH (a)-[:KNOWS]->(b) RETURN b.name", &mut g).unwrap();
//! println!("{:?}", result.columns);
//! ```
//!
//! Or as a process: `glider social.gldb` for a shell, `glider social.gldb
//! serve` for HTTP.

pub mod algo;
pub mod codec;
pub mod ffi;
pub mod graph;
pub mod query;
pub mod server;
pub mod store;
pub mod stream;
pub mod utils;
pub mod value;
pub mod wal;

pub use graph::{Csr, Dir, Error, Graph, Node, Result, Stats};
pub use query::{execute, export_jsonl, import_jsonl, QueryResult};
pub use store::Sync;
pub use value::Value;

/// Run one statement against a graph.
pub fn query(src: &str, g: &mut Graph) -> Result<QueryResult> {
    query::execute(g, src)
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
