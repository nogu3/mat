//! Net-side PASE responder driver (feature `net`). Thin — owns the UDP
//! transport and `ResponderExchange` plumbing, delegates all protocol logic
//! to `crate::core::pase::PaseResponderCore`. This is a minimal, single-
//! handshake driver (`run_pase_once`); Task 12 builds the real device
//! runtime (multi-exchange listener, commissioning window, etc).
//!
//! Reliability quirk worth documenting: `ResponderExchange::reply_reliable`
//! sends our response and waits, but the peer's MRP layer eagerly fires a
//! *standalone* ack for our message the instant it arrives — before it has
//! computed its own next reply — so `reply_reliable` routinely returns
//! `Ok(None)` (only the standalone ack observed) rather than the peer's
//! actual next message. On `None` we fall back to `ex.recv(...)` (waits for
//! the next real message on the *same* exchange) rather than re-`adopt`ing
//! a fresh `ResponderExchange`: `adopt` reseeds `counter` with a brand new
//! `TxCounter::new_random()`, and a fresh random value has roughly even
//! odds of landing below the peer's already-established high-water mark —
//! its `RxWindow` then silently rejects our next reply as "too old" (while
//! still, misleadingly, acking it), so the peer's own MRP retransmits
//! instead of ever treating our reply as the answer. `ex.recv` keeps the
//! one `ResponderExchange` (and its single incrementing `TxCounter`) alive
//! for the whole handshake, avoiding that.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mat_controller::exchange::{ExchangeError, IncomingMessage, MrpConfig, ResponderExchange};
use mat_controller::message::{
    MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK, OPCODE_STATUS_REPORT,
    PROTOCOL_ID_SECURE_CHANNEL,
};
use mat_controller::session::SessionKeys;
use mat_controller::transport::{Transport, UdpTransport, MAX_DATAGRAM};

use crate::core::pase::{PaseCoreError, PaseOutput, PaseResponderCore, PaseVerifierConfig};

/// PBKDF parameters this minimal driver advertises. Matches
/// `mat_controller::test_support::pase_responder_task`'s fixture values.
const ITERATIONS: u32 = 1000;
const SALT: &[u8; 16] = b"SPAKE2P Key Salt";
/// Fixed (not randomized): this driver serves exactly one self-contained
/// handshake, so a collision-checked random value buys nothing here — same
/// rationale as `test_support::pase_responder_task`.
const RESPONDER_SESSION_ID: u16 = 0xB0B1;

/// SecureChannel protocol `GeneralStatusCode::FAILURE` (spec §4.11.3).
const GENERAL_CODE_FAILURE: u16 = 1;
/// SecureChannel protocol-specific `kInvalidParameter` (spec §4.11.3.1) —
/// mirrors what `mat_controller::pase::establish` sends/treats as failure
/// on confirmation mismatch / bad PBKDF params.
const SC_PROTOCOL_CODE_INVALID_PARAMETER: u16 = 2;

/// Wait budget for `ex.recv(...)` once a reply has already been
/// standalone-acked (i.e. the peer is actively working on the next real
/// message) — covers SPAKE2+ compute time on their side. Generous rather
/// than tight: this is a single-handshake test/dev driver, not a budget
/// other code depends on (contrast `mat_controller::pase::RECV_TIMEOUT`,
/// which the real controller's op-budget accounting is built around).
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Same values as `mat_controller::test_support::fast_cfg` — 50ms
/// intervals, no jitter.
fn retry_cfg() -> MrpConfig {
    MrpConfig {
        initial_interval: Duration::from_millis(50),
        active_interval: Duration::from_millis(50),
        max_retries: 2,
        backoff: 1.0,
        jitter: 0.0,
    }
}

/// Errors from driving one PASE responder handshake over the network.
/// Malformed/foreign datagrams aren't an error variant here — `recv_first`
/// and `ex.recv`'s screening silently skip them (matching
/// `mat_controller::test_support`'s responder loop and `screen()`'s own
/// DoS-hardening posture); only genuine I/O, exchange, and protocol-level
/// failures surface.
#[derive(Debug)]
pub enum NetPaseError {
    Io(std::io::Error),
    Exchange(ExchangeError),
    Core(PaseCoreError),
}

impl std::fmt::Display for NetPaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetPaseError::Io(e) => write!(f, "pase(net): transport error: {e}"),
            NetPaseError::Exchange(e) => write!(f, "pase(net): exchange error: {e}"),
            NetPaseError::Core(e) => write!(f, "pase(net): {e}"),
        }
    }
}

impl std::error::Error for NetPaseError {}

impl From<std::io::Error> for NetPaseError {
    fn from(e: std::io::Error) -> Self {
        NetPaseError::Io(e)
    }
}

impl From<ExchangeError> for NetPaseError {
    fn from(e: ExchangeError) -> Self {
        NetPaseError::Exchange(e)
    }
}

impl From<PaseCoreError> for NetPaseError {
    fn from(e: PaseCoreError) -> Self {
        NetPaseError::Core(e)
    }
}

/// StatusReport failure payload (8 bytes LE `{general_code, protocol_id,
/// protocol_code}`, spec §4.11.3) sent on any protocol violation the core
/// rejects. `encode_status_report`/`GENERAL_CODE_FAILURE`/
/// `SC_PROTOCOL_CODE_INVALID_PARAMETER` are `pub(crate)`/private to
/// `mat_controller::case`/`pase`, so this is a local 8-byte replica rather
/// than a shared helper.
fn status_report_failure() -> [u8; 8] {
    let mut buf = [0u8; 8];
    buf[0..2].copy_from_slice(&GENERAL_CODE_FAILURE.to_le_bytes());
    buf[2..6].copy_from_slice(&u32::from(PROTOCOL_ID_SECURE_CHANNEL).to_le_bytes());
    buf[6..8].copy_from_slice(&SC_PROTOCOL_CODE_INVALID_PARAMETER.to_le_bytes());
    buf
}

/// Reads the very first unsecured datagram from any sender — there's no
/// `ResponderExchange` yet to hand this off to (that's what `adopt` is
/// for), so this is a one-shot raw read, not a loop: PASE commissioning
/// windows serve one initiator at a time, and this driver handles exactly
/// one handshake. Malformed datagrams and standalone acks (which can't
/// legitimately arrive before any exchange exists) are skipped.
async fn recv_first(transport: &Transport) -> Result<(IncomingMessage, SocketAddr), NetPaseError> {
    loop {
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = transport.recv_from(&mut buf).await?;
        let Ok((header, off)) = MessageHeader::decode(&buf[..n]) else {
            continue;
        };
        if header.session_id != 0 || header.security_flags != 0 {
            continue;
        }
        let Ok((proto, body_off)) = ProtocolHeader::decode(&buf[off..n]) else {
            continue;
        };
        if !proto.initiator {
            continue;
        }
        if proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
            && proto.opcode == OPCODE_MRP_STANDALONE_ACK
        {
            continue;
        }
        return Ok((
            IncomingMessage {
                header,
                proto,
                payload: buf[off + body_off..n].to_vec(),
            },
            from,
        ));
    }
}

/// Drives one PASE responder handshake to completion over `transport`:
/// waits for PBKDFParamRequest, adopts a `ResponderExchange` on it, then
/// feeds each message into a `PaseResponderCore` and replies until
/// `Established`. Returns the derived `SessionKeys` on success. On any
/// protocol violation, sends a StatusReport failure (best-effort) and
/// returns the error.
pub async fn run_pase_once(
    transport: UdpTransport,
    passcode: u32,
) -> Result<SessionKeys, NetPaseError> {
    let transport = Transport::Udp(Arc::new(transport));

    // First message: no exchange/peer known yet.
    let (first, peer) = recv_first(&transport).await?;

    let (keys, _peer_session_id) = drive_established(
        &transport,
        peer,
        first,
        PaseVerifierConfig {
            passcode,
            salt: SALT.to_vec(),
            iterations: ITERATIONS,
            responder_session_id: RESPONDER_SESSION_ID,
        },
    )
    .await?;
    Ok(keys)
}

/// Drives one PASE responder handshake to completion given an
/// **already-received** first message — the runtime (`net::runtime`) reads
/// every datagram itself to classify it (unsecured PASE/CASE opcode vs.
/// secured session traffic) before dispatching, so by the time it knows
/// this is a fresh PASE attempt the first datagram is already off the
/// socket and can't be handed to a fresh `recv_first`. Otherwise identical
/// to [`run_pase_once`]'s inner loop, except it returns the peer's
/// (initiator's) session id alongside the derived keys — needed to build
/// `SecureSession::new_device_role(..)` — instead of discarding it.
pub(crate) async fn drive_established(
    transport: &Transport,
    peer: SocketAddr,
    first: IncomingMessage,
    config: PaseVerifierConfig,
) -> Result<(SessionKeys, u16), NetPaseError> {
    let cfg = retry_cfg();
    let mut core = PaseResponderCore::new(config);

    // One `ResponderExchange` for the whole handshake — see the module doc
    // comment on why re-adopting per message is unsafe.
    let mut ex = ResponderExchange::adopt(transport, peer, &first);
    let mut incoming = first;

    loop {
        let output = match core.on_message(incoming.proto.opcode, &incoming.payload) {
            Ok(o) => o,
            Err(e) => {
                // Best-effort abort notification (see pase::establish's own
                // ConfirmMismatch/bad-params branches for the same
                // send-and-ignore-the-result pattern) — we're already
                // unwinding to an error, so we don't retry the send.
                let _ = ex
                    .reply_final(
                        PROTOCOL_ID_SECURE_CHANNEL,
                        OPCODE_STATUS_REPORT,
                        &status_report_failure(),
                        &cfg,
                    )
                    .await;
                return Err(e.into());
            }
        };
        match output {
            PaseOutput::Reply(reply, opcode) => {
                let next = ex
                    .reply_reliable(PROTOCOL_ID_SECURE_CHANNEL, opcode, &reply, &cfg)
                    .await?;
                incoming = match next {
                    Some(msg) => msg,
                    None => ex.recv(RECV_TIMEOUT).await?,
                };
            }
            PaseOutput::Established {
                reply,
                opcode,
                keys,
                peer_session_id,
            } => {
                ex.reply_final(PROTOCOL_ID_SECURE_CHANNEL, opcode, &reply, &cfg)
                    .await?;
                return Ok((keys, peer_session_id));
            }
        }
    }
}
