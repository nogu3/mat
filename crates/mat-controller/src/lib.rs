//! From-scratch Matter controller library (Phase 5 backend).
//!
//! Protocol implementation lives here and only here — mat CLI / matd
//! command layers never speak TLV / CASE / crypto directly.
//! M1 scope: TLV codec, message layer, session crypto, MRP.

pub mod asn1;
pub mod attestation;
pub mod btp;
pub mod case;
pub mod cert;
pub mod commissioning;
pub mod counter;
pub mod crypto;
pub mod dnssd;
pub mod exchange;
pub mod fabric;
pub mod group;
pub mod im;
pub mod kvs;
pub mod message;
pub mod pase;
pub mod session;
pub mod setup_code;
pub mod spake2p;
pub mod tlv;
pub mod transport;
pub mod x509;
