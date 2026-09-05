//! Protocol state machines, codecs, and data model for the device side
//! (PASE responder, CASE responder, IM server). No tokio, no sockets, no
//! file I/O — see `net` for the platform layer. This discipline is checked
//! mechanically by `cargo check -p mat-device --no-default-features` in CI.

pub mod access_control;
pub mod bridge;
pub mod bridged_device_basic_information;
pub mod case;
pub mod commissioning;
pub mod datamodel;
pub mod fabric_store;
pub mod general_diagnostics;
pub mod group_invoke;
pub mod group_key_management;
pub mod group_membership;
pub mod group_privacy;
pub mod groups;
pub mod identify;
pub mod mdns_records;
pub mod network_commissioning;
pub mod onoff;
pub mod pase;
