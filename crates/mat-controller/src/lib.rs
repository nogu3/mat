//! From-scratch Matter controller library (Phase 5 backend).
//!
//! Protocol implementation lives here and only here — mat CLI / matd
//! command layers never speak TLV / CASE / crypto directly.
//! M1 scope: TLV codec, message layer, session crypto, MRP.

pub mod counter;
pub mod crypto;
pub mod exchange;
pub mod message;
pub mod tlv;
pub mod transport;
