//! Net-side CASE responder driver (feature `net`). Thin — owns the UDP
//! transport and `ResponderExchange` plumbing, delegates all protocol logic
//! to `crate::core::case::CaseResponderCore`. This is a minimal,
//! single-handshake driver (`run_case_once`), the CASE counterpart of
//! `crate::net::pase::run_pase_once`; Task 12 builds the real device runtime
//! (multi-exchange listener, commissioning window, etc). See that module's
//! doc comment for the `reply_reliable` / standalone-ack reliability quirk
//! this driver shares (same `ResponderExchange`-adoption strategy).
//!
//! Deliberately stops at the secured `SecureSession` handoff — it does not
//! serve any Interaction Model traffic itself (that's `core::datamodel` /
//! Task 12 territory). Callers that want to exercise the secured channel
//! (e.g. `tests/case_establish.rs`) drive the returned `SecureSession`
//! directly with its own `recv_request`/`reply_reliable` device-role helpers.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mat_controller::exchange::{ExchangeError, IncomingMessage, ResponderExchange};
use mat_controller::message::{OPCODE_STATUS_REPORT, PROTOCOL_ID_SECURE_CHANNEL};
use mat_controller::secure_channel::{
    encode_status_report, GENERAL_CODE_FAILURE, SC_PROTOCOL_CODE_INVALID_PARAMETER,
    SC_PROTOCOL_CODE_NO_SHARED_TRUST_ROOTS,
};
use mat_controller::session::SecureSession;
use mat_controller::transport::{Transport, UdpTransport};

use crate::core::case::{CaseCoreError, CaseOutput, CaseResponderCore};
use crate::core::fabric_store::FabricEntry;

/// Wait budget for `ex.recv(...)` once a reply has already been
/// standalone-acked — same rationale/value as `net::pase`'s `RECV_TIMEOUT`.
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors from driving one CASE responder handshake over the network.
/// Malformed/foreign datagrams aren't an error variant here (screened out
/// the same way `net::pase` does); only genuine I/O, exchange, and
/// protocol-level failures surface.
#[derive(Debug)]
pub enum NetCaseError {
    Io(std::io::Error),
    Exchange(ExchangeError),
    Core(CaseCoreError),
    /// The handshake completed but building the post-CASE `SecureSession`
    /// found no fabric entry matching the `CaseOutput::Established`
    /// `fabric_index` (unreachable in practice — the core only ever selects
    /// a fabric already present in `fabrics` — kept as a defensive variant
    /// rather than a `panic!`/`expect`).
    UnknownFabricIndex(u8),
}

impl std::fmt::Display for NetCaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetCaseError::Io(e) => write!(f, "case(net): transport error: {e}"),
            NetCaseError::Exchange(e) => write!(f, "case(net): exchange error: {e}"),
            NetCaseError::Core(e) => write!(f, "case(net): {e}"),
            NetCaseError::UnknownFabricIndex(idx) => write!(
                f,
                "case(net): established on unknown fabric index {idx} (bug)"
            ),
        }
    }
}

impl std::error::Error for NetCaseError {}

impl From<std::io::Error> for NetCaseError {
    fn from(e: std::io::Error) -> Self {
        NetCaseError::Io(e)
    }
}

impl From<ExchangeError> for NetCaseError {
    fn from(e: ExchangeError) -> Self {
        NetCaseError::Exchange(e)
    }
}

impl From<CaseCoreError> for NetCaseError {
    fn from(e: CaseCoreError) -> Self {
        NetCaseError::Core(e)
    }
}

/// StatusReport failure payload for `err`, mapping
/// `CaseCoreError::NoSharedTrustRoots` to its specific spec protocol code and
/// every other violation to the generic `INVALID_PARAMETER` catch-all.
fn status_report_failure(err: &CaseCoreError) -> Vec<u8> {
    let code = match err {
        CaseCoreError::NoSharedTrustRoots => SC_PROTOCOL_CODE_NO_SHARED_TRUST_ROOTS,
        _ => SC_PROTOCOL_CODE_INVALID_PARAMETER,
    };
    encode_status_report(
        GENERAL_CODE_FAILURE,
        u32::from(PROTOCOL_ID_SECURE_CHANNEL),
        code,
    )
}

/// Drives one CASE responder handshake to completion over `transport`:
/// waits for Sigma1, adopts a `ResponderExchange` on it, then feeds each
/// message into a `CaseResponderCore` (seeded with `fabrics`) and replies
/// until `Established`. Returns the resulting device-role `SecureSession`
/// plus which fabric index was selected. On any protocol violation, sends a
/// StatusReport failure (best-effort) and returns the error.
pub async fn run_case_once(
    transport: UdpTransport,
    fabrics: Vec<FabricEntry>,
    responder_session_id: u16,
) -> Result<(SecureSession, u8), NetCaseError> {
    let transport = Arc::new(Transport::Udp(Arc::new(transport)));
    let (first, peer) = crate::net::recv_first(&transport).await?;
    drive_established(transport, peer, first, fabrics, responder_session_id).await
}

/// Drives one CASE responder handshake to completion given an
/// **already-received** first message — the runtime (`net::runtime`) reads
/// every datagram itself to classify it (unsecured PASE/CASE opcode vs.
/// secured session traffic) before dispatching, so by the time it knows
/// this is a fresh Sigma1 the first datagram is already off the socket and
/// can't be handed to a fresh `recv_first`. Otherwise identical to
/// [`run_case_once`]'s inner loop (which now just calls this after its own
/// `recv_first`).
pub(crate) async fn drive_established(
    transport: Arc<Transport>,
    peer: SocketAddr,
    first: IncomingMessage,
    fabrics: Vec<FabricEntry>,
    responder_session_id: u16,
) -> Result<(SecureSession, u8), NetCaseError> {
    // Kept for the post-`Established` `local_node_id` lookup below —
    // `CaseResponderCore::new` takes ownership of `fabrics` itself.
    let fabrics_snapshot = fabrics.clone();
    let cfg = crate::net::fast_cfg();

    let mut core = CaseResponderCore::new(fabrics, responder_session_id);

    // One `ResponderExchange` for the whole handshake — see
    // `net::pase::run_pase_once`'s doc comment on why re-adopting per
    // message is unsafe.
    let mut ex = ResponderExchange::adopt(&transport, peer, &first);
    let mut incoming = first;

    loop {
        let output = match core.on_message(incoming.proto.opcode, &incoming.payload) {
            Ok(o) => o,
            Err(e) => {
                // Best-effort abort notification, same send-and-ignore
                // pattern as `net::pase::run_pase_once`.
                let _ = ex
                    .reply_final(
                        PROTOCOL_ID_SECURE_CHANNEL,
                        OPCODE_STATUS_REPORT,
                        &status_report_failure(&e),
                        &cfg,
                    )
                    .await;
                return Err(e.into());
            }
        };
        match output {
            CaseOutput::Reply(reply, opcode) => {
                let next = ex
                    .reply_reliable(PROTOCOL_ID_SECURE_CHANNEL, opcode, &reply, &cfg)
                    .await?;
                incoming = match next {
                    Some(msg) => msg,
                    None => ex.recv(RECV_TIMEOUT).await?,
                };
            }
            CaseOutput::Established {
                reply,
                opcode,
                keys,
                peer_session_id,
                peer_node_id,
                peer_cats,
                fabric_index,
            } => {
                ex.reply_final(PROTOCOL_ID_SECURE_CHANNEL, opcode, &reply, &cfg)
                    .await?;
                let local_node_id = fabrics_snapshot
                    .iter()
                    .find(|f| f.fabric_index == fabric_index)
                    .map(|f| f.node_id)
                    .ok_or(NetCaseError::UnknownFabricIndex(fabric_index))?;
                let session = SecureSession::new_device_role(
                    Arc::clone(&transport),
                    peer,
                    responder_session_id,
                    peer_session_id,
                    keys,
                    local_node_id,
                    peer_node_id,
                )
                .with_peer_cats(peer_cats);
                return Ok((session, fabric_index));
            }
        }
    }
}
