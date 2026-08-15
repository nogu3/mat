//! Protocol state machines, codecs, and data model for the device side
//! (PASE responder, CASE responder, IM server). No tokio, no sockets, no
//! file I/O — see `net` for the platform layer. This discipline is checked
//! mechanically by `cargo check -p mat-device --no-default-features` in CI.

pub mod commissioning;
pub mod datamodel;
pub mod fabric_store;
pub mod pase;
