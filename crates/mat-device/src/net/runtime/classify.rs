use super::*;

/// Sigma1's opcode (spec §4.14) — `case_responder::OPCODE_SIGMA1` is
/// `pub(crate)` to mat-controller (see `core::case`'s test module for the
/// same literal), so the runtime's classifier uses the wire value directly.
pub(super) const OPCODE_CASE_SIGMA1: u8 = 0x30;

/// One classified unsecured datagram's destination flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnsecuredFlow {
    Pase,
    Case,
    /// Not a flow this runtime starts (foreign protocol id, or a
    /// SecureChannel opcode we don't originate a session from — e.g. a
    /// stray `StatusReport`/standalone-ack with no matching exchange).
    Ignore,
}

/// Pure classifier — kept separate from the loop so it's unit-testable
/// without a socket (brief: "単体テストの datagram classifier if it's
/// nontrivial" — the opcode ranges below aren't obvious from the wire
/// consts alone, so it earns its own test).
pub(super) fn classify_unsecured(protocol_id: u16, opcode: u8) -> UnsecuredFlow {
    if protocol_id != PROTOCOL_ID_SECURE_CHANNEL {
        return UnsecuredFlow::Ignore;
    }
    match opcode {
        OPCODE_PBKDF_PARAM_REQUEST
        | OPCODE_PBKDF_PARAM_RESPONSE
        | OPCODE_PASE_PAKE1
        | OPCODE_PASE_PAKE2
        | OPCODE_PASE_PAKE3 => UnsecuredFlow::Pase,
        OPCODE_CASE_SIGMA1 => UnsecuredFlow::Case,
        _ => UnsecuredFlow::Ignore,
    }
}

/// The commissioning window's admission decision (Task 14) for one already-
/// classified unsecured flow: `None` means "drop it — no response, no
/// session start", `Some` means "let `run`'s existing match handle it
/// unchanged". Kept as a pure function separate from the `select!` loop for
/// the same reason `classify_unsecured` is: unit-testable without a socket.
///
/// - `UnsecuredFlow::Pase` is gated on `window_open` — a closed window
///   (spec §5.4.2.3's 15-minute PASE upper bound elapsed, or
///   `CommissioningComplete` already closed it, or the device booted with a
///   fabric already installed) must refuse *every* PASE opcode with total
///   silence, not a `StatusReport` (申し送り 7 項: this runtime's DoS-
///   hardening posture treats a closed window exactly like every other
///   "nothing to route this to" drop elsewhere in this module — see the
///   module doc's wire-classification list).
/// - `UnsecuredFlow::Case` is never gated: CASE is how an already-
///   commissioned controller reconnects on every subsequent boot, spec's
///   commissioning window only bounds *PASE* (§5.4.2.3), and closing CASE
///   too would brick every fabric already on the device.
/// - `UnsecuredFlow::Ignore` passes through unchanged — it was never a flow
///   this runtime starts in the first place (see its variant doc), so the
///   window has nothing to say about it.
pub(super) fn admit_unsecured(flow: UnsecuredFlow, window_open: bool) -> Option<UnsecuredFlow> {
    match flow {
        UnsecuredFlow::Pase if !window_open => None,
        other => Some(other),
    }
}

/// Whether a `RemoveFabric` that just removed `removed_fabric_index` from
/// the store should end the current secured session — true iff the
/// removed fabric is the one `session_fabric_index` (this session's own,
/// carried alongside `SecureSession` in `Runtime`'s `current_session`)
/// authenticated against. Extracted as a pure function (mirrors
/// `admin_window_action` above) so this one-line decision has a unit test
/// that doesn't need a socket harness — `serve_secured_message`'s
/// `RemoveFabric` block below only calls it and acts on the result.
///
/// A PASE session (`session_fabric_index == 0`) can never match: `0` isn't
/// a valid fabric index (spec §2.5.1, fabric indices are 1-based), and
/// `RemoveFabric` itself is only reachable post-`AddNOC` in practice, but
/// nothing here assumes that — a `0 == 0` false-positive would be wrong
/// (there is no "fabric 0" to remove), so this only fires for handled
/// FabricIndex bytes, which `decode_remove_fabric` already restricts to
/// values `FabricStore` actually assigned (1+).
pub(super) fn remove_fabric_drops_session(
    removed_fabric_index: u8,
    session_fabric_index: u8,
) -> bool {
    removed_fabric_index == session_fabric_index && session_fabric_index != 0
}
