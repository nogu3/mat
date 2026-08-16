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
use std::time::Duration;

use tokio::time::Instant;

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
use crate::core::datamodel::{InvokeCtx, Node, ReadCtx};
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

/// The payload budget for one `ReportData` chunk (Task 6:
/// `Node::read_chunks`'s `budget` argument for real reads). `MAX_DATAGRAM`
/// (1280B, `mat_controller::transport`) is the hard ceiling on one UDP
/// datagram; `REPORT_CHUNK_BUDGET` leaves headroom below it for everything
/// `read_chunks` itself doesn't account for — the Matter message header,
/// the IM protocol header, and the AES-CCM 16B MIC/tag that
/// `SecureSession::seal` adds after encoding — so an encoded chunk at or
/// under this budget still fits in one real datagram once sealed. Echo/
/// chip full-wildcard reads pull in Operational Credentials' NOCs/
/// TrustedRootCertificates (certificates, ~500B class each) — well past
/// this budget on their own, which is exactly why chunking exists.
const REPORT_CHUNK_BUDGET: usize = 900;

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

/// `bring_up_mdns` retry backoff state, kept only while mDNS hasn't come up
/// yet (review fix round 1, item 1): a boot-time failure — e.g. IPv6
/// Duplicate Address Detection not finished yet on real hardware — must not
/// leave the device permanently invisible to discovery while it otherwise
/// looks up and would happily answer a PASE it never gets to see. Policy is
/// deliberately simple (not adaptive/jittered): retry every
/// `MDNS_RETRY_INTERVAL_INITIAL` for the first `MDNS_RETRY_BACKOFF_THRESHOLD`
/// of failures, then every `MDNS_RETRY_INTERVAL_LONG` — enough to recover
/// quickly from a transient startup race without hammering a genuinely bad
/// interface name forever.
struct MdnsRetry {
    first_failure_at: Instant,
    next_attempt_at: Instant,
}

/// Every 5s while mDNS has been down for less than a minute...
const MDNS_RETRY_INTERVAL_INITIAL: Duration = Duration::from_secs(5);
/// ...then every 60s after that.
const MDNS_RETRY_INTERVAL_LONG: Duration = Duration::from_secs(60);
const MDNS_RETRY_BACKOFF_THRESHOLD: Duration = Duration::from_secs(60);

impl MdnsRetry {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            first_failure_at: now,
            next_attempt_at: now + MDNS_RETRY_INTERVAL_INITIAL,
        }
    }

    /// Schedules the next attempt after another failure.
    fn schedule_next(&mut self) {
        let interval = if self.first_failure_at.elapsed() < MDNS_RETRY_BACKOFF_THRESHOLD {
            MDNS_RETRY_INTERVAL_INITIAL
        } else {
            MDNS_RETRY_INTERVAL_LONG
        };
        self.next_attempt_at = Instant::now() + interval;
    }
}

/// Resolves once at `retry.next_attempt_at`, or never (`std::future::pending`)
/// when there's no retry pending (mDNS already up, or never attempted).
/// Used as a `tokio::select!` branch alongside `recv_from` so the retry
/// timer can't block datagram serving — a `None` retry state simply makes
/// this branch inert instead of needing `select!`'s `if` precondition
/// syntax (simpler to reason about with a value that's re-borrowed fresh
/// every loop iteration).
async fn mdns_retry_deadline(retry: &Option<MdnsRetry>) {
    match retry {
        Some(r) => tokio::time::sleep_until(r.next_attempt_at).await,
        None => std::future::pending().await,
    }
}

/// Runs the device: binds nothing itself (the caller already bound
/// `transport`/`local_addr` — `Device::new` does that synchronously, see
/// its doc comment for why); brings up mDNS best-effort (see
/// `bring_up_mdns`'s doc comment — a failure there is logged and retried in
/// the background per `MdnsRetry`, never fatal); then serves datagrams
/// forever — this only returns early if a caller-supplied future it's
/// raced against elsewhere completes first (it never returns on its own;
/// see `Device::run`'s doc comment for the exact contract).
pub(crate) async fn run(
    transport: Arc<Transport>,
    local_addr: SocketAddr,
    config: DeviceConfig,
    mut node: Node,
    comm_server: CommissioningServer,
) -> Result<(), DeviceError> {
    let port = local_addr.port();
    let mut mdns_ctx: Option<MdnsCtx> = None;
    let mut mdns_retry: Option<MdnsRetry> = None;
    match bring_up_mdns(&config, port, &comm_server).await {
        Ok(ctx) => mdns_ctx = Some(ctx),
        Err(e) => {
            tracing::warn!(
                error = %e,
                iface = %config.iface,
                "mDNS advertiser did not come up — device still serves PASE/CASE/IM to peers that already have its address; retrying in the background"
            );
            mdns_retry = Some(MdnsRetry::new());
        }
    }

    // Third tuple element: the session's fabric index (spec §7.9) — `0` for
    // PASE (no fabric yet), the CASE-selected fabric otherwise. Carried
    // through to every `ReadRequest` this session serves via `ReadCtx`
    // (`serve_secured`/`serve_secured_message`), so e.g. Operational
    // Credentials' `CurrentFabricIndex` reflects the reading session, not a
    // hardcoded value.
    let mut current_session: Option<(u16, SecureSession, u8)> = None;
    let mut buf = [0u8; MAX_DATAGRAM];
    loop {
        tokio::select! {
            recv = transport.recv_from(&mut buf) => {
                let (n, peer) = match recv {
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
                                current_session = Some((local_session_id, session, 0)); // PASE: no fabric yet
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
                            if let Ok((session, fabric_index)) = outcome {
                                current_session = Some((local_session_id, session, fabric_index));
                            }
                        }
                        UnsecuredFlow::Ignore => {}
                    }
                    continue;
                }

                // Secured traffic: only ever the current session (sequential,
                // one-at-a-time — see module doc).
                let Some((sid, session, fabric_index)) = current_session.as_mut() else {
                    continue;
                };
                if header.session_id != *sid {
                    continue;
                }
                serve_secured(
                    &buf[..n],
                    peer,
                    session,
                    *fabric_index,
                    &mut node,
                    &comm_server,
                    mdns_ctx.as_ref(),
                )
                .await;
            }
            () = mdns_retry_deadline(&mdns_retry) => {
                match bring_up_mdns(&config, port, &comm_server).await {
                    Ok(ctx) => {
                        tracing::info!("mDNS advertiser came up on retry");
                        mdns_ctx = Some(ctx);
                        mdns_retry = None;
                    }
                    Err(e) => {
                        // Warned once already (either at startup, above, or
                        // on the very first retry — from here on this is
                        // expected/repetitive noise for a device on a
                        // genuinely bad interface, hence debug not warn.
                        tracing::debug!(error = %e, "mDNS retry attempt failed, will retry again");
                        if let Some(state) = mdns_retry.as_mut() {
                            state.schedule_next();
                        }
                    }
                }
            }
        }
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
    fabric_index: u8,
    node: &mut Node,
    comm_server: &CommissioningServer,
    mdns: Option<&MdnsCtx>,
) {
    let msg = match session.deliver_request(buf, from).await {
        Ok(Some(msg)) => msg,
        Ok(None) => return,
        Err(_) => return, // decrypt/screen failure — drop, don't kill the session on noise
    };
    serve_secured_message(msg, session, fabric_index, node, comm_server, mdns).await;

    // While `reply_reliable` (inside `serve_secured_message`) was waiting
    // for the ack of *that* reply, a new peer-initiated request on a
    // *different* exchange may have arrived — real controllers/commissioners
    // commonly piggyback their ack on the very next request rather than
    // sending a standalone one. `screen_with` still acks and buffers it
    // (`peer_initiated`) even though it failed that wait's `PeerExchange`
    // filter, so it must be served here rather than silently lost (review
    // fix: "ack-then-drop of cross-exchange secured requests"). Draining in
    // a loop (not just once) covers the same thing happening again while
    // *this* reply's own ack-wait is in flight.
    while let Some(buffered) = session.take_buffered_request() {
        if buffered.proto.protocol_id != PROTOCOL_ID_INTERACTION_MODEL {
            continue;
        }
        serve_secured_message(buffered, session, fabric_index, node, comm_server, mdns).await;
    }
}

/// Dispatches one already-classified Interaction Model request (`msg`) to
/// `node`, replies, and reacts to the two commissioning milestones (see
/// `serve_secured`'s original doc comment for why: AddNOC success detected
/// by fabric count growth, CommissioningComplete by decoding the request).
/// Split out from `serve_secured` so both the datagram just read off the
/// socket and any buffered peer-initiated request drained afterward go
/// through the identical path. `fabric_index` is this session's fabric
/// index (0 for PASE) — threaded into `ReadCtx` for every `ReadRequest`.
async fn serve_secured_message(
    msg: mat_controller::exchange::IncomingMessage,
    session: &mut SecureSession,
    fabric_index: u8,
    node: &mut Node,
    comm_server: &CommissioningServer,
    mdns: Option<&MdnsCtx>,
) {
    if msg.proto.protocol_id != PROTOCOL_ID_INTERACTION_MODEL {
        // Secure-channel traffic on an established session (e.g. a
        // device-initiated-exchange StatusReport) — out of M1 scope.
        return;
    }

    // ReadRequest gets its own chunk-aware flow (Task 6) instead of going
    // through `Node::handle_im` (whose `handle_read` always answers with a
    // single message) — see `serve_read_request_chunked`'s doc comment.
    // Reads never trigger the AddNOC/CommissioningComplete milestones
    // below (those are Invoke-only), so returning here is safe.
    if msg.proto.opcode == im::OPCODE_READ_REQUEST {
        serve_read_request_chunked(&msg, session, fabric_index, node).await;
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
    let read_ctx = ReadCtx { fabric_index };
    let fabrics_before = comm_server.fabrics().len();
    let Ok((resp_opcode, resp_payload)) =
        node.handle_im(msg.proto.opcode, &msg.payload, &mut ctx, &read_ctx)
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

/// Serves one `ReadRequest` with `Node::read_chunks`'s chunked reply flow
/// (Task 6), bypassing `Node::handle_im`/`handle_read` (which only ever
/// return a single message) entirely. Mirrors `SecureSession::
/// subscribe_wildcard`'s priming-report chunk loop on the *initiator* side
/// (`crates/mat-controller/src/session.rs`, around line 1329): every chunk
/// but the last is sent `more_chunks=true, suppress_response=false` and
/// this side then waits for the peer's `StatusResponse(0)` on the same
/// exchange before sending the next one; the last chunk is sent
/// `more_chunks=false, suppress_response=true` and nothing further is
/// awaited — exactly what `handle_read`'s old single-message reply always
/// sent, so a read whose data fits in one chunk (`Node::read_chunks`
/// returns exactly one chunk) behaves identically to before Task 6.
///
/// `reply_reliable`'s `Some(msg)`/`None` return mirrors `send_reliable`'s:
/// if the peer piggybacks its real `StatusResponse` on the MRP ack instead
/// of sending a standalone one, `reply_reliable` already has it in hand;
/// otherwise a separate `session.recv` on the same exchange (bounded to 5s
/// — this is a LAN round-trip to a controller/hub, not a WAN call) waits
/// for it. Any failure along the way — the reply itself failing to send,
/// a wrong opcode, a non-zero status, a malformed StatusResponse, or a
/// timeout — aborts the remaining chunks rather than retrying or looping:
/// the exchange is effectively dead at that point, and the initiator sees
/// an incomplete read it can retry from scratch.
async fn serve_read_request_chunked(
    msg: &mat_controller::exchange::IncomingMessage,
    session: &mut SecureSession,
    fabric_index: u8,
    node: &mut Node,
) {
    let Ok(paths) = im::decode_read_request(&msg.payload) else {
        return;
    };
    let read_ctx = ReadCtx { fabric_index };
    let chunks = node.read_chunks(&paths, &read_ctx, REPORT_CHUNK_BUDGET);
    let last_index = chunks.len().saturating_sub(1);

    for (i, chunk) in chunks.into_iter().enumerate() {
        let is_last = i == last_index;
        let Ok(piggybacked) = session
            .reply_reliable(
                msg,
                PROTOCOL_ID_INTERACTION_MODEL,
                im::OPCODE_REPORT_DATA,
                &chunk,
                &reply_cfg(),
            )
            .await
        else {
            return; // ack never came — exchange is dead, give up
        };

        if is_last {
            return; // final chunk: no StatusResponse expected, done
        }

        let status_msg = match piggybacked {
            Some(m) => m,
            None => match session
                .recv(msg.proto.exchange_id, Duration::from_secs(5))
                .await
            {
                Ok(m) => m,
                Err(_) => return, // timed out waiting for StatusResponse(0)
            },
        };
        if status_msg.proto.opcode != im::OPCODE_STATUS_RESPONSE {
            return;
        }
        match im::decode_status_response(&status_msg.payload) {
            Ok(0) => continue, // ack for this chunk — send the next one
            _ => return,       // non-zero status or malformed reply
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

    // ── mDNS retry backoff (review fix round 1, item 1) ────────────────

    #[tokio::test(start_paused = true)]
    async fn mdns_retry_schedules_the_initial_interval_first() {
        let retry = MdnsRetry::new();
        assert_eq!(
            retry.next_attempt_at - retry.first_failure_at,
            MDNS_RETRY_INTERVAL_INITIAL
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mdns_retry_keeps_the_short_interval_before_the_threshold() {
        let mut retry = MdnsRetry::new();
        // Advance to just before the backoff threshold and fail again —
        // still short-interval territory.
        tokio::time::advance(MDNS_RETRY_BACKOFF_THRESHOLD - Duration::from_secs(1)).await;
        retry.schedule_next();
        assert_eq!(
            retry.next_attempt_at - Instant::now(),
            MDNS_RETRY_INTERVAL_INITIAL
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mdns_retry_switches_to_the_long_interval_past_the_threshold() {
        let mut retry = MdnsRetry::new();
        tokio::time::advance(MDNS_RETRY_BACKOFF_THRESHOLD + Duration::from_secs(1)).await;
        retry.schedule_next();
        assert_eq!(
            retry.next_attempt_at - Instant::now(),
            MDNS_RETRY_INTERVAL_LONG
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mdns_retry_deadline_never_resolves_when_no_retry_pending() {
        let none: Option<MdnsRetry> = None;
        // If this ever resolved, the test would hang until its own harness
        // timeout — so this is really "doesn't hang" plus a bounded race
        // against a short sleep to make that assertion concrete.
        tokio::select! {
            () = mdns_retry_deadline(&none) => panic!("deadline resolved with no retry pending"),
            () = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    }

    // ── review fix: cross-exchange piggyback ack must not lose a request ──

    /// Runtime-level companion to `mat_controller::session`'s
    /// `reply_reliable_completes_via_cross_exchange_piggyback_ack`: proves
    /// `serve_secured`'s drain loop doesn't just retain a request buffered
    /// while the first reply's `reply_reliable` was waiting on its
    /// (piggybacked, cross-exchange) ack — it actually dispatches it through
    /// `Node::handle_im` and replies, exactly like a datagram read fresh off
    /// the socket. No mDNS, no commissioning — a bare `Node`/
    /// `CommissioningServer` pair driven directly, `serve_secured` called by
    /// hand the same way `run`'s loop calls it.
    #[tokio::test]
    async fn serve_secured_drains_and_serves_a_cross_exchange_piggybacked_request() {
        use mat_controller::crypto::{open_message, seal_message};
        use mat_controller::message::Destination;
        use mat_controller::session::SessionKeys;
        use mat_controller::transport::UdpTransport;

        use crate::core::fabric_store::FabricStore;

        const LOCAL_SID: u16 = 0xAAAA; // device's own session id
        const PEER_SID: u16 = 0xBBBB; // controller's session id
        const CTRL_NODE: u64 = 1;
        const DEV_NODE: u64 = 2;
        const I2R: [u8; 16] = [0x11; 16];
        const R2I: [u8; 16] = [0x22; 16];
        const REQ_EXCHANGE: u16 = 0x10;
        const NEW_EXCHANGE: u16 = 0x20;

        let controller = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        let ctrl_addr = controller.local_addr().unwrap();
        let dev_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )));
        let dev_addr = dev_transport.local_addr().unwrap();

        let mut session = SecureSession::new_device_role(
            Arc::clone(&dev_transport),
            ctrl_addr,
            LOCAL_SID,
            PEER_SID,
            SessionKeys {
                i2r: I2R,
                r2i: R2I,
                attestation_challenge: [0; 16],
            },
            DEV_NODE,
            CTRL_NODE,
        );
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let comm_server = CommissioningServer::new(dev, FabricStore::new());

        // Controller side, run concurrently with `serve_secured` below: send
        // the first ReadRequest, wait for its reply, then — instead of a
        // standalone ack — send a second ReadRequest on a *different*
        // exchange that piggybacks the first reply's ack, and finally wait
        // for its own reply too.
        let ctrl_task = tokio::spawn(async move {
            let req1 = im::encode_read_request(
                0,
                mat_controller::im::CLUSTER_BASIC_INFORMATION,
                mat_controller::im::ATTR_DATA_MODEL_REVISION,
            );
            let header1 = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 10,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto1 = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: None,
                opcode: im::OPCODE_READ_REQUEST,
                exchange_id: REQ_EXCHANGE,
                protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
                vendor_id: None,
            };
            let dg1 = seal_message(&I2R, &header1, &proto1, &req1, CTRL_NODE).unwrap();
            controller.send_to(&dg1, dev_addr).await.unwrap();

            let mut buf = [0u8; MAX_DATAGRAM];
            let (n1, from) = controller.recv_from(&mut buf).await.unwrap();
            let (h1, p1, _) = open_message(&R2I, &buf[..n1], DEV_NODE).unwrap();
            assert_eq!(p1.exchange_id, REQ_EXCHANGE);
            assert_eq!(p1.opcode, im::OPCODE_REPORT_DATA);
            assert!(p1.needs_ack);

            let req2 = im::encode_read_request(
                0,
                mat_controller::im::CLUSTER_BASIC_INFORMATION,
                mat_controller::im::ATTR_VENDOR_ID,
            );
            let header2 = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 11,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto2 = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: Some(h1.message_counter), // piggyback: acks dg1's reply
                opcode: im::OPCODE_READ_REQUEST,
                exchange_id: NEW_EXCHANGE,
                protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
                vendor_id: None,
            };
            let dg2 = seal_message(&I2R, &header2, &proto2, &req2, CTRL_NODE).unwrap();
            controller.send_to(&dg2, from).await.unwrap();

            // Proof the drain loop actually served dg2 (not just buffered
            // it): a real ReportData for VendorID must arrive on
            // NEW_EXCHANGE.
            let (n2, from2) = controller.recv_from(&mut buf).await.unwrap();
            let (h2, p2, payload2) = open_message(&R2I, &buf[..n2], DEV_NODE).unwrap();
            assert_eq!(p2.exchange_id, NEW_EXCHANGE);
            assert_eq!(p2.opcode, im::OPCODE_REPORT_DATA);
            let rd = im::decode_report_data_message(&payload2).unwrap();
            assert_eq!(
                rd.reports[0].attribute,
                Some(mat_controller::im::ATTR_VENDOR_ID)
            );
            assert_eq!(rd.reports[0].data, Some(serde_json::json!(0xFFF1)));

            // Ack this second reply too, so `serve_secured`'s own
            // `reply_reliable` for it completes promptly instead of
            // exhausting `MrpConfig::default()`'s retry budget.
            let ack_header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 12,
                source_node_id: None,
                destination: Destination::None,
            };
            let ack_proto = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: Some(h2.message_counter),
                opcode: OPCODE_MRP_STANDALONE_ACK,
                exchange_id: NEW_EXCHANGE,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                vendor_id: None,
            };
            let ack_dg = seal_message(&I2R, &ack_header, &ack_proto, &[], CTRL_NODE).unwrap();
            controller.send_to(&ack_dg, from2).await.unwrap();
        });

        // Device side: read dg1 off the (real) socket exactly like `run`'s
        // loop does, then hand it to `serve_secured` — whose internal
        // `reply_reliable` ack-wait is what actually reads dg2 off the
        // socket and resolves it via the cross-exchange piggyback ack,
        // which is what makes the drain loop kick in afterward.
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = dev_transport.recv_from(&mut buf).await.unwrap();
        serve_secured(
            &buf[..n],
            from,
            &mut session,
            0,
            &mut node,
            &comm_server,
            None,
        )
        .await;

        ctrl_task.await.unwrap();
    }

    // ── Task 6: chunked ReadRequest reply flow ──────────────────────────

    /// A device-role closed-loop drive of `serve_read_request_chunked`
    /// (Task 6): a full-wildcard read against a `Node` carrying two ~600B
    /// attributes (each alone under `REPORT_CHUNK_BUDGET`, but the two
    /// together well past it) must come back as 2+ `ReportData` chunks,
    /// each non-final one answered with `StatusResponse(0)` on the same
    /// exchange before the next is sent — `mat` (`read_attribute`) has no
    /// chunk support to drive this against (brief's Step 4), so this test
    /// plays the controller role by hand at the raw-datagram level, the
    /// same technique `serve_secured_drains_and_serves_a_cross_exchange_
    /// piggybacked_request` above uses. `read_chunks`'s own split/flag
    /// correctness is covered by `datamodel.rs`'s unit tests; this test's
    /// job is only proving the runtime's send-chunk/await-StatusResponse
    /// loop actually round-trips over real sockets — the initiator-side
    /// counterpart to what `SecureSession::subscribe_wildcard`'s priming
    /// loop already exercises from the other end (`session.rs`, around
    /// line 1329).
    #[tokio::test]
    async fn read_request_chunked_flow_round_trips_two_or_more_chunks() {
        use mat_controller::crypto::{open_message, seal_message};
        use mat_controller::message::Destination;
        use mat_controller::session::SessionKeys;
        use mat_controller::tlv::{Tag, Writer};
        use mat_controller::transport::UdpTransport;

        use crate::core::datamodel::{ClusterHandler, InvokeReply};
        use crate::core::fabric_store::FabricStore;

        const LOCAL_SID: u16 = 0xAAAA;
        const PEER_SID: u16 = 0xBBBB;
        const CTRL_NODE: u64 = 1;
        const DEV_NODE: u64 = 2;
        const I2R: [u8; 16] = [0x11; 16];
        const R2I: [u8; 16] = [0x22; 16];
        const REQ_EXCHANGE: u16 = 0x30;

        /// Test-only cluster exposing one ~600B attribute — two of these
        /// registered on the node force `read_chunks` to split (two
        /// together exceed `REPORT_CHUNK_BUDGET`, though neither alone
        /// does). Cluster id is far outside any real cluster id range.
        struct FatHandler {
            cluster: u32,
        }
        impl ClusterHandler for FatHandler {
            fn cluster_id(&self) -> u32 {
                self.cluster
            }
            fn attributes(&self) -> Vec<u32> {
                vec![1]
            }
            fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
                if attribute != 1 {
                    return None;
                }
                let mut w = Writer::new();
                w.put_bytes(Tag::Anonymous, &[0xCD; 600]);
                Some(w.finish())
            }
            fn invoke(
                &mut self,
                _command: u32,
                _fields_tlv: &[u8],
                _ctx: &mut InvokeCtx,
            ) -> InvokeReply {
                InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
            }
        }

        /// Full-wildcard ReadRequest (every field of the one
        /// AttributePathIB omitted) — `mat_controller::im` has no public
        /// encoder for this shape (its `encode_read_request*` helpers all
        /// pin at least endpoint+cluster), so built by hand the same way
        /// `datamodel.rs`'s test-only `encode_read_request_paths` does.
        fn encode_full_wildcard_read_request() -> Vec<u8> {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.start_array(Tag::Context(0)); // AttributeRequests
            w.start_list(Tag::Anonymous); // AttributePathIB, all wildcard
            w.end_container();
            w.end_container(); // AttributeRequests
            w.put_bool(Tag::Context(3), true); // IsFabricFiltered
            w.put_uint(Tag::Context(255), u64::from(im::IM_REVISION));
            w.end_container();
            w.finish()
        }

        let controller = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        let dev_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )));
        let dev_addr = dev_transport.local_addr().unwrap();

        let mut session = SecureSession::new_device_role(
            Arc::clone(&dev_transport),
            controller.local_addr().unwrap(),
            LOCAL_SID,
            PEER_SID,
            SessionKeys {
                i2r: I2R,
                r2i: R2I,
                attestation_challenge: [0; 16],
            },
            DEV_NODE,
            CTRL_NODE,
        );
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        node.add_cluster(
            0,
            Box::new(FatHandler {
                cluster: 0x9999_0001,
            }),
        );
        node.add_cluster(
            0,
            Box::new(FatHandler {
                cluster: 0x9999_0002,
            }),
        );
        let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let comm_server = CommissioningServer::new(dev, FabricStore::new());

        let ctrl_task = tokio::spawn(async move {
            let req = encode_full_wildcard_read_request();
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 10,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: None,
                opcode: im::OPCODE_READ_REQUEST,
                exchange_id: REQ_EXCHANGE,
                protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
                vendor_id: None,
            };
            let dg = seal_message(&I2R, &header, &proto, &req, CTRL_NODE).unwrap();
            controller.send_to(&dg, dev_addr).await.unwrap();

            let mut counter = 11u32;
            let mut chunk_count = 0usize;
            let mut buf = [0u8; MAX_DATAGRAM];
            loop {
                let (n, from) = controller.recv_from(&mut buf).await.unwrap();
                let peer = from;
                let (h, p, payload) = open_message(&R2I, &buf[..n], DEV_NODE).unwrap();
                assert_eq!(p.exchange_id, REQ_EXCHANGE);
                assert_eq!(p.opcode, im::OPCODE_REPORT_DATA);
                let rd = im::decode_report_data_message(&payload).unwrap();
                chunk_count += 1;

                if rd.more_chunks {
                    assert!(!rd.suppress_response);
                    // Reply with StatusResponse(0) on the same exchange —
                    // `serve_read_request_chunked`'s `reply_reliable` for
                    // this chunk resolves on any non-standalone-ack
                    // message on the exchange (same idiom
                    // `send_reliable`/`SecureSession::subscribe_wildcard`
                    // use), so this single reply both acks the chunk and
                    // is what the runtime's `session.recv` StatusResponse
                    // wait is looking for.
                    let ok = im::encode_status_response(0);
                    let resp_header = MessageHeader {
                        session_id: LOCAL_SID,
                        security_flags: 0,
                        message_counter: counter,
                        source_node_id: None,
                        destination: Destination::None,
                    };
                    let resp_proto = ProtocolHeader {
                        initiator: true,
                        needs_ack: false,
                        acked_counter: Some(h.message_counter),
                        opcode: im::OPCODE_STATUS_RESPONSE,
                        exchange_id: REQ_EXCHANGE,
                        protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
                        vendor_id: None,
                    };
                    let dg = seal_message(&I2R, &resp_header, &resp_proto, &ok, CTRL_NODE).unwrap();
                    controller.send_to(&dg, peer).await.unwrap();
                    counter += 1;
                } else {
                    assert!(rd.suppress_response);
                    // Final chunk: no StatusResponse expected from us, but
                    // still ack it (standalone) so the runtime's own
                    // `reply_reliable` for this last send completes
                    // promptly instead of exhausting its retry budget.
                    let ack_header = MessageHeader {
                        session_id: LOCAL_SID,
                        security_flags: 0,
                        message_counter: counter,
                        source_node_id: None,
                        destination: Destination::None,
                    };
                    let ack_proto = ProtocolHeader {
                        initiator: true,
                        needs_ack: false,
                        acked_counter: Some(h.message_counter),
                        opcode: OPCODE_MRP_STANDALONE_ACK,
                        exchange_id: REQ_EXCHANGE,
                        protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                        vendor_id: None,
                    };
                    let ack_dg =
                        seal_message(&I2R, &ack_header, &ack_proto, &[], CTRL_NODE).unwrap();
                    controller.send_to(&ack_dg, peer).await.unwrap();
                    break;
                }
            }
            chunk_count
        });

        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = dev_transport.recv_from(&mut buf).await.unwrap();
        serve_secured(
            &buf[..n],
            from,
            &mut session,
            0,
            &mut node,
            &comm_server,
            None,
        )
        .await;

        let chunk_count = ctrl_task.await.unwrap();
        assert!(chunk_count >= 2, "expected 2+ chunks, got {chunk_count}");
    }
}
