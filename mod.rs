//! `glider-stream`: continuous replication to object storage.
//!
//! The engine ships segments; this is everything around that — backends,
//! configuration, snapshots, retention, metrics, and restore. Kept in its own
//! module tree so the core database never grows a network stack it does not
//! need, and so the `glider` binary stays what it is.

pub mod backend;
pub mod config;
pub mod hash;
pub mod http;
pub mod replicator;
pub mod s3;
