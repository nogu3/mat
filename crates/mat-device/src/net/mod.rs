//! I/O layer (tokio, UDP transport, mDNS advertisement) built on top of
//! `core`. Gated behind the `net` feature (default on).

pub mod case;
pub mod endpoint_ledger;
pub mod group_rx;
pub mod mdns;
pub mod pase;
pub(crate) mod runtime;
pub mod store;
pub mod subscription;
