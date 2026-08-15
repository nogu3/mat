//! I/O layer (tokio, UDP transport, mDNS advertisement) built on top of
//! `core`. Gated behind the `net` feature (default on).

pub mod case;
pub mod mdns;
pub mod pase;
pub mod store;
