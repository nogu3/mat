//! The device runtime: a single `[::]:{port}` UDP socket serving PASE
//! commissioning, CASE, and (post-commissioning) secured Interaction Model
//! traffic — sequentially, one session at a time (spec's own design
//! principle for a constrained device; also the simplest correct thing to
//! build for M1).
//!
//! `Device::new` (`crate::device`) does all the synchronous setup (bind the
//! socket, generate a dev attestation chain, load/create the fabric store,
//! build the `Node`/`CommissioningServer`); this module is just the async
//! loop `Device::run` hands off to.
//!
//! ## Wire classification (one `recv_from` per iteration)
//!
//! Every datagram is read exactly once, right here, and classified by
//! `MessageHeader`/`ProtocolHeader`:
//! - unsecured (`session_id == 0`) + a PASE opcode (0x20-0x24) → hand off to
//!   `net::pase::drive_established` (a fresh `PaseResponderCore` self-rejects
//!   anything that isn't `PBKDFParamRequest` as the first message of a new
//!   attempt, so routing *any* PASE opcode here is safe, not just 0x20).
//! - unsecured + Sigma1 (0x30) → `net::case::drive_established`.
//! - secured, `session_id` matching the current session → fed into that
//!   `SecureSession` via `deliver_request` (a small addition to
//!   `mat_controller::session::SecureSession` for exactly this "I already
//!   read the datagram myself" case — see its doc comment) and then
//!   `Node::handle_im`.
//! - anything else (foreign secured session id, undecodable, standalone ack
//!   with no session) is silently dropped, matching every other responder
//!   in this workspace's DoS-hardening posture.
//!
//! Establishing a new PASE or CASE session replaces whatever the "current
//! session" was — this runtime never serves two peers at once (a second
//! commissioner's first datagram during an in-flight session is simply not
//! classified as a new attempt yet, since its opcode still routes to the
//! PASE/CASE handlers, which will just start a *second* concurrent
//! exchange... note below).
//!
//! Fail-safe expiry needs no special handling here: `CommissioningServer`
//! checks `is_armed()` on every gated command itself (`core::commissioning`)
//! and answers `STATUS_FAILSAFE_REQUIRED` once it lapses — this runtime just
//! forwards whatever `Node::handle_im` returns.

use std::net::{Ipv6Addr, SocketAddr};
use std::sync::Arc;

use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::compressed_fabric_id;
use mat_controller::im;
use mat_controller::message::{
    MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK, PROTOCOL_ID_INTERACTION_MODEL,
    PROTOCOL_ID_SECURE_CHANNEL,
};
use mat_controller::pase::{
    OPCODE_PASE_PAKE1, OPCODE_PASE_PAKE2, OPCODE_PASE_PAKE3, OPCODE_PBKDF_PARAM_REQUEST,
    OPCODE_PBKDF_PARAM_RESPONSE,
};
use mat_controller::session::SecureSession;
use mat_controller::transport::{Transport, MAX_DATAGRAM};

use crate::core::commissioning::CommissioningServer;
use crate::core::datamodel::{InvokeCtx, Node};
use crate::core::fabric_store::FabricEntry;
use crate::core::mdns_records::{CommissionableAdvert, OperationalAdvert};
use crate::core::pase::PaseVerifierConfig;
use crate::device::{DeviceConfig, DeviceError};
use crate::net::mdns::MdnsAdvertiser;

/// PBKDF parameters this runtime advertises — same fixture values
/// `net::pase::run_pase_once` and `mat_controller::test_support`'s own
/// responder use (spec §3.9 legal, not spec-mandated exact numbers).
const ITERATIONS: u32 = 1000;
const SALT: &[u8; 16] = b"SPAKE2P Key Salt";

/// Sigma1's opcode (spec §4.14) — `case_responder::OPCODE_SIGMA1` is
/// `pub(crate)` to mat-controller (see `core::case`'s test module for the
/// same literal), so the runtime's classifier uses the wire value directly.
const OPCODE_CASE_SIGMA1: u8 = 0x30;

/// One secured request's MRP retry budget while replying — generous (this
/// is a device answering, not a controller racing a user-visible deadline).
fn reply_cfg() -> MrpConfig {
    MrpConfig::default()
}

/// One classified unsecured datagram's destination flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnsecuredFlow {
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
fn classify_unsecured(protocol_id: u16, opcode: u8) -> UnsecuredFlow {
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

/// A random non-zero u16 — local (responder) session id for a fresh PASE or
/// CASE attempt. Not collision-checked against a previous session: this
/// runtime keeps at most one "current session" alive at a time (see module
/// doc), so the only way a collision could matter is astronomically
/// unlikely (1/65535) and even then just means the old session's next
/// datagram gets fed into the new one's `SecureSession` — a screen-level
/// session-id match with a peer address that no longer matches would drop
/// it harmlessly (`screen_with`'s `from != self.peer` check).
fn random_session_id() -> u16 {
    loop {
        let mut b = [0u8; 2];
        getrandom::getrandom(&mut b).expect("os rng");
        let v = u16::from_le_bytes(b);
        if v != 0 {
            return v;
        }
    }
}

/// Random 64-bit hex instance/hostname (spec §4.3.1: the commissionable
/// service's instance name SHOULD be random). Reused as both the mDNS
/// instance name and the hostname (`<name>.local`) — legal, and simplest
/// for M1 (a real hostname-vs-instance split buys nothing here since both
/// ultimately resolve to the same one address this device advertises).
fn random_hex_name() -> String {
    let mut b = [0u8; 8];
    getrandom::getrandom(&mut b).expect("os rng");
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Reads `/proc/net/if_inet6` for `iface`'s link-local (scope 0x20) IPv6
/// address — same parsing technique `mat_native::iface_select::scan` uses
/// for the same file, duplicated locally rather than shared (that helper
/// lives in a different crate and returns iface *names*, not addresses).
/// Linux-specific; this runtime targets Linux (jarvis) like the rest of the
/// `net` feature (raw `socket2`/`/proc` usage throughout `net::mdns`).
fn iface_link_local_addr(iface: &str) -> Result<Ipv6Addr, DeviceError> {
    let content = std::fs::read_to_string("/proc/net/if_inet6").map_err(DeviceError::Io)?;
    for line in content.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 6 && cols[3] == "20" && cols[5] == iface {
            let hex = cols[0];
            if hex.len() != 32 {
                continue;
            }
            let mut bytes = [0u8; 16];
            for (i, byte) in bytes.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                    .map_err(|_| DeviceError::Iface(format!("bad if_inet6 hex for {iface}")))?;
            }
            return Ok(Ipv6Addr::from(bytes));
        }
    }
    Err(DeviceError::Iface(format!(
        "no link-local ipv6 address on interface {iface}"
    )))
}

/// Builds the `OperationalAdvert` for one installed fabric entry.
fn operational_advert(
    entry: &FabricEntry,
    hostname: &str,
    port: u16,
    addr_v6: Ipv6Addr,
) -> OperationalAdvert {
    OperationalAdvert {
        compressed_fabric_id: compressed_fabric_id(&entry.root_public_key, entry.fabric_id),
        node_id: entry.node_id,
        hostname: hostname.to_string(),
        port,
        addr_v6,
    }
}

/// The mDNS advertiser plus everything needed to build adverts for it
/// (hostname/port/address are fixed for the process lifetime once
/// resolved). Kept together, behind one `Option`, because bringing up mDNS
/// is best-effort — see `run`'s doc comment on why a device that can't
/// join the multicast group must still serve PASE/CASE/IM traffic to a
/// peer that already knows its address.
struct MdnsCtx {
    mdns: Arc<MdnsAdvertiser>,
    hostname: String,
    port: u16,
    addr_v6: Ipv6Addr,
}

/// Brings up the mDNS advertiser: resolves `config.iface`, spawns the
/// advertiser, sets the commissionable advert, and republishes every
/// fabric already on disk (the restart path: a second `Device::new` over
/// the same `store_dir` reloads `comm_server.fabrics()` from disk, and this
/// makes sure those fabrics are still discoverable operationally after the
/// restart). Failure (bad interface name, no link-local address, socket
/// bind failure) is reported via `Err` so `run` can log it — but `run`
/// itself treats that as *non-fatal*: mDNS is how a real controller finds
/// this device, but a device unreachable by discovery still MUST answer a
/// peer that already has its address (exactly `direct_drive_*`'s test
/// setup, and not unrealistic — e.g. a controller with a cached address).
async fn bring_up_mdns(
    config: &DeviceConfig,
    port: u16,
    comm_server: &CommissioningServer,
) -> Result<MdnsCtx, DeviceError> {
    let scope_id =
        mat_controller::dnssd::iface_index(&config.iface).map_err(DeviceError::IfaceIndex)?;
    let addr_v6 = iface_link_local_addr(&config.iface)?;
    let hostname = random_hex_name();

    let mdns = MdnsAdvertiser::spawn(scope_id)
        .await
        .map_err(DeviceError::Io)?;
    mdns.set_commissionable(Some(CommissionableAdvert {
        instance: random_hex_name(),
        hostname: hostname.clone(),
        discriminator: config.discriminator,
        vendor_id: config.vendor_id,
        product_id: config.product_id,
        port,
        addr_v6,
    }));
    for entry in comm_server.fabrics() {
        mdns.add_operational(operational_advert(&entry, &hostname, port, addr_v6));
    }

    Ok(MdnsCtx {
        mdns,
        hostname,
        port,
        addr_v6,
    })
}

/// Runs the device: binds nothing itself (the caller already bound
/// `transport`/`local_addr` — `Device::new` does that synchronously, see
/// its doc comment for why); brings up mDNS best-effort (see
/// `bring_up_mdns`'s doc comment — a failure there is logged, not fatal);
/// then serves datagrams forever.
pub(crate) async fn run(
    transport: Arc<Transport>,
    local_addr: SocketAddr,
    config: DeviceConfig,
    mut node: Node,
    comm_server: CommissioningServer,
) -> Result<(), DeviceError> {
    let port = local_addr.port();
    let mdns_ctx = match bring_up_mdns(&config, port, &comm_server).await {
        Ok(ctx) => Some(ctx),
        Err(e) => {
            tracing::warn!(
                error = %e,
                iface = %config.iface,
                "mDNS advertiser did not come up — device still serves PASE/CASE/IM to peers that already have its address"
            );
            None
        }
    };

    let mut current_session: Option<(u16, SecureSession)> = None;
    let mut buf = [0u8; MAX_DATAGRAM];
    loop {
        let (n, peer) = match transport.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(_) => continue, // best-effort responder — a transient recv error isn't fatal
        };
        let Ok((header, off)) = MessageHeader::decode(&buf[..n]) else {
            continue;
        };
        if header.session_id == 0 && header.security_flags == 0 {
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
            let first = mat_controller::exchange::IncomingMessage {
                header,
                proto,
                payload: buf[off + body_off..n].to_vec(),
            };
            match classify_unsecured(proto.protocol_id, proto.opcode) {
                UnsecuredFlow::Pase => {
                    let local_session_id = random_session_id();
                    let outcome = crate::net::pase::drive_established(
                        &transport,
                        peer,
                        first,
                        PaseVerifierConfig {
                            passcode: config.passcode,
                            salt: SALT.to_vec(),
                            iterations: ITERATIONS,
                            responder_session_id: local_session_id,
                        },
                    )
                    .await;
                    if let Ok((keys, peer_session_id)) = outcome {
                        let session = SecureSession::new_device_role(
                            Arc::clone(&transport),
                            peer,
                            local_session_id,
                            peer_session_id,
                            keys,
                            0, // PASE: both sides are node id 0 (spec §4.13)
                            0,
                        );
                        current_session = Some((local_session_id, session));
                    }
                    // Established failure: best-effort responder, nothing
                    // more to do — the initiator's own retry/StatusReport
                    // handling covers it.
                }
                UnsecuredFlow::Case => {
                    let local_session_id = random_session_id();
                    let fabrics = comm_server.fabrics();
                    let outcome = crate::net::case::drive_established(
                        Arc::clone(&transport),
                        peer,
                        first,
                        fabrics,
                        local_session_id,
                    )
                    .await;
                    if let Ok((session, _fabric_index)) = outcome {
                        current_session = Some((local_session_id, session));
                    }
                }
                UnsecuredFlow::Ignore => {}
            }
            continue;
        }

        // Secured traffic: only ever the current session (sequential,
        // one-at-a-time — see module doc).
        let Some((sid, session)) = current_session.as_mut() else {
            continue;
        };
        if header.session_id != *sid {
            continue;
        }
        serve_secured(
            &buf[..n],
            peer,
            session,
            &mut node,
            &comm_server,
            mdns_ctx.as_ref(),
        )
        .await;
    }
}

/// Handles one datagram already known to be secured traffic for `session`:
/// decrypt/screen it, dispatch Interaction Model requests to `node`, reply,
/// and react to the two commissioning milestones the brief calls out —
/// AddNOC success (detected by `comm_server.fabrics()` growing across the
/// call, rather than decoding which command it was — robust to *any*
/// gated command eventually installing a fabric, not just this one) and
/// CommissioningComplete (detected by decoding the request we're about to
/// serve, cheap and side-effect-free, purely for this notification).
/// `mdns` is `None` when `bring_up_mdns` failed at startup (`run`'s doc
/// comment) — commissioning still completes, just without an operational
/// advert or a commissionable-window teardown to publish.
async fn serve_secured(
    buf: &[u8],
    from: SocketAddr,
    session: &mut SecureSession,
    node: &mut Node,
    comm_server: &CommissioningServer,
    mdns: Option<&MdnsCtx>,
) {
    let msg = match session.deliver_request(buf, from).await {
        Ok(Some(msg)) => msg,
        Ok(None) => return,
        Err(_) => return, // decrypt/screen failure — drop, don't kill the session on noise
    };
    if msg.proto.protocol_id != PROTOCOL_ID_INTERACTION_MODEL {
        // Secure-channel traffic on an established session (e.g. a
        // device-initiated-exchange StatusReport) — out of M1 scope.
        return;
    }

    // Only meaningful for CommissioningComplete detection below; a decode
    // failure here just means we won't recognize that milestone (the
    // dispatch to `node.handle_im` below still runs against the raw bytes
    // regardless, so a malformed request is still answered/rejected
    // normally).
    let req_cluster_command = im::decode_invoke_request(&msg.payload)
        .ok()
        .map(|r| (r.cluster, r.command));

    let mut ctx = InvokeCtx {
        attestation_challenge: session.attestation_challenge(),
    };
    let fabrics_before = comm_server.fabrics().len();
    let Ok((resp_opcode, resp_payload)) = node.handle_im(msg.proto.opcode, &msg.payload, &mut ctx)
    else {
        return;
    };
    let _ = session
        .reply_reliable(
            &msg,
            PROTOCOL_ID_INTERACTION_MODEL,
            resp_opcode,
            &resp_payload,
            &reply_cfg(),
        )
        .await;

    // AddNOC success: a fabric appeared that wasn't there before this call.
    let fabrics_after = comm_server.fabrics();
    if fabrics_after.len() > fabrics_before {
        if let (Some(entry), Some(ctx)) = (fabrics_after.last(), mdns) {
            ctx.mdns.add_operational(operational_advert(
                entry,
                &ctx.hostname,
                ctx.port,
                ctx.addr_v6,
            ));
        }
    }

    // CommissioningComplete success: stop advertising commissionable.
    if resp_opcode == im::OPCODE_INVOKE_RESPONSE {
        if let Some((cluster, command)) = req_cluster_command {
            if cluster == mat_controller::commissioning::CLUSTER_GENERAL_COMMISSIONING
                && command == mat_controller::commissioning::CMD_COMMISSIONING_COMPLETE
            {
                if let Ok(outcome) = im::decode_invoke_response(&resp_payload) {
                    if outcome.status == im::STATUS_SUCCESS {
                        if let Some(ctx) = mdns {
                            ctx.mdns.set_commissionable(None);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_pase_opcodes() {
        for op in [
            OPCODE_PBKDF_PARAM_REQUEST,
            OPCODE_PBKDF_PARAM_RESPONSE,
            OPCODE_PASE_PAKE1,
            OPCODE_PASE_PAKE2,
            OPCODE_PASE_PAKE3,
        ] {
            assert_eq!(
                classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, op),
                UnsecuredFlow::Pase,
                "opcode 0x{op:02X} should classify as Pase"
            );
        }
    }

    #[test]
    fn classifies_case_sigma1() {
        assert_eq!(
            classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_CASE_SIGMA1),
            UnsecuredFlow::Case
        );
    }

    #[test]
    fn ignores_foreign_protocol_id() {
        assert_eq!(
            classify_unsecured(PROTOCOL_ID_INTERACTION_MODEL, OPCODE_PBKDF_PARAM_REQUEST),
            UnsecuredFlow::Ignore
        );
    }

    #[test]
    fn ignores_unknown_secure_channel_opcode() {
        // e.g. OPCODE_STATUS_REPORT (0x40) or OPCODE_MRP_STANDALONE_ACK
        // (0x10) reaching the classifier as a "first" datagram — the main
        // loop actually filters standalone acks before classifying, but
        // the classifier itself should still be inert on them (defense in
        // depth / doesn't assume its caller's pre-filtering).
        assert_eq!(
            classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, 0x40),
            UnsecuredFlow::Ignore
        );
        assert_eq!(
            classify_unsecured(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_MRP_STANDALONE_ACK),
            UnsecuredFlow::Ignore
        );
    }

    #[test]
    fn random_session_id_is_never_zero() {
        for _ in 0..1000 {
            assert_ne!(random_session_id(), 0);
        }
    }
}
