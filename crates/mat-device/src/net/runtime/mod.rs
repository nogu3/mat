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
//!
//! ## Groupcast (spec §4.15, Task 7)
//!
//! A second, independent UDP socket (`net::group_rx::GroupSocket`) receives
//! group-session datagrams — Matter's fixed multicast port 5540, bound
//! `SO_REUSEPORT` alongside the unicast socket rather than shared with it,
//! since multicast join/leave is per-socket state the unicast path has no
//! business carrying. `sync_group_joins` runs at the top of every loop
//! iteration (desired = each fabric's own `GroupMembershipStore` groups, via
//! `group_rx::desired_group_addrs`) so an `AddGroup`, `RemoveFabric`, or
//! fail-safe rollback that changes membership is picked up on the very next
//! spin, with no dedicated event plumbing. The `select!` grows a `grecv`
//! branch reading that socket (`group_rx::group_recv`, which is
//! `std::future::pending` when the socket never bound); a decoded datagram
//! is classified by `group_rx::classify_group_datagram` and, on success,
//! applied via `Node::handle_group_invoke` under a `Subject::group(..)` —
//! never a response, per spec §4.15's fire-and-forget contract. The unicast
//! socket, in turn, drops any datagram whose `security_flags` says group
//! session right after header decode (`SESSION_TYPE_MASK`) — group traffic
//! is the group socket's job even if it happens to also reach the unicast
//! one.

use std::collections::HashMap;
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
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

use crate::core::access_control::Subject;
use crate::core::commissioning::{CommissioningServer, WindowRequest};
use crate::core::datamodel::{ImOutcome, InvokeCtx, Node, ReadCtx};
use crate::core::fabric_store::FabricEntry;
use crate::core::mdns_records::{CommissionableAdvert, OperationalAdvert};
use crate::core::pase::{PaseSecret, PaseVerifierConfig};
use crate::device::{DeviceConfig, DeviceError};
use crate::net::group_rx::{
    classify_group_datagram, desired_group_addrs, group_recv, GroupReplayGuard, GroupRx,
    GroupRxDeps, SESSION_TYPE_MASK,
};
use crate::net::mdns::MdnsAdvertiser;
use crate::net::stimulus::{StimulusApplyError, StimulusIntake, StimulusRequest};
use crate::net::subscription::ActiveSubscription;

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
    node: Node,
    comm_server: CommissioningServer,
    group: GroupRx,
    stimuli: StimulusIntake,
) -> Result<(), DeviceError> {
    Runtime::boot(
        transport,
        local_addr,
        config,
        node,
        comm_server,
        group,
        stimuli,
    )
    .await
    .serve_forever()
    .await
}

/// The node-side state every secured message may touch, owned here and
/// lent out as a `ServeState` (the borrowed view `serve_secured`/
/// `serve_secured_message` take) via `serve_state`. Kept as its own struct
/// rather than flattened into `Runtime` so a `Runtime` method can borrow
/// `self.current_session` mutably *and* build a `ServeState` from
/// `self.state` in the same expression — disjoint fields, so the borrow
/// checker allows it; a `serve_state(&mut self)` on `Runtime` itself would
/// borrow all of `Runtime` and conflict with the session borrow.
struct NodeState {
    node: Node,
    comm_server: CommissioningServer,
    /// `None` while `bring_up_mdns` hasn't succeeded (see `run`'s doc).
    mdns: Option<MdnsCtx>,
    /// The node's single active subscription (spec §8.10, Task 12). Tied to
    /// the session that created it: a new PASE/CASE session drops it
    /// (`Runtime::install_session`), since its reports could only ever go
    /// out over the session it was subscribed on.
    subscription: Option<ActiveSubscription>,
    window: CommissioningWindow,
    config: DeviceConfig,
    /// `[[device]]` の `id` → その device が生えている endpoint 番号
    /// （`net::endpoint_ledger` の採番結果）。刺激は名前で来るので
    /// （`net::stimulus`）、ここで番号へ落とす。
    endpoint_by_device: HashMap<String, u16>,
    /// このプロセスが `boot` した時刻。イベントの `SystemTimestamp`
    /// （spec §8.9.2.6 — 「起動からの経過ミリ秒」）の基準で、`core` が
    /// 時計を持てない（I/O-free）ぶんをここが埋める。
    started_at: Instant,
}

impl NodeState {
    /// イベントに刻む `SystemTimestamp`（spec §8.9.2.6）= 起動からの
    /// 経過ミリ秒。`Node::stimulate` へ渡す唯一の時刻。
    fn system_timestamp_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// The `ServeState` view of this state — what one secured message (or
    /// one drained buffered request) is allowed to touch.
    fn serve_state(&mut self) -> ServeState<'_> {
        ServeState {
            node: &mut self.node,
            comm_server: &self.comm_server,
            mdns: self.mdns.as_ref(),
            subscription: &mut self.subscription,
            window: &mut self.window,
            config: &self.config,
        }
    }
}

/// Everything `serve_forever`'s loop carries between iterations, so the
/// per-branch handlers (`on_*`) can be plain methods instead of one
/// 400-line `select!` body. Built once by `boot`, driven forever by
/// `serve_forever`.
struct Runtime {
    transport: Arc<Transport>,
    port: u16,
    /// A fresh random PASE salt each boot — see `boot`.
    pase_salt: [u8; 16],
    mdns_retry: Option<MdnsRetry>,
    /// The current secured session: `(local_session_id, session,
    /// fabric_index)`. Third element: the session's fabric index (spec
    /// §7.9) — `0` for PASE (no fabric yet), the CASE-selected fabric
    /// otherwise. Carried through to every `ReadRequest` this session
    /// serves via `ReadCtx` (`serve_secured`/`serve_secured_message`), so
    /// e.g. Operational Credentials' `CurrentFabricIndex` reflects the
    /// reading session, not a hardcoded value.
    current_session: Option<(u16, SecureSession, u8)>,
    replay: GroupReplayGuard,
    group: GroupRx,
    /// 外から届く刺激（`net::stimulus`）の受信側。`select!` の 1 分岐。
    stimuli: mpsc::Receiver<StimulusRequest>,
    /// 送信ハンドルが全部 drop されて `stimuli.recv()` が `None` を
    /// 返し続けるようになったか。`select!` の `Some(..)` パターンは
    /// `None` では不成立 → そのままだと分岐が即座に再評価されて
    /// ビジーループになるので、以後この分岐自体を落とす。
    stimuli_closed: bool,
    state: NodeState,
}

impl Runtime {
    /// Boot-time setup: the PASE salt, the commissioning window's boot
    /// policy, and the first (best-effort) `bring_up_mdns` attempt.
    async fn boot(
        transport: Arc<Transport>,
        local_addr: SocketAddr,
        config: DeviceConfig,
        node: Node,
        comm_server: CommissioningServer,
        group: GroupRx,
        stimuli: StimulusIntake,
    ) -> Self {
        let port = local_addr.port();
        // A fresh random PASE salt each boot (spec §3.9 permits any salt; a
        // fixed one is weak against a precomputed rainbow table across every
        // device running this firmware). Generated once here and reused for
        // every PASE attempt this run serves — it only needs to be consistent
        // within one handshake (it round-trips to the peer in
        // PBKDFParamResponse), not secret or per-attempt.
        let mut pase_salt = [0u8; 16];
        getrandom::fill(&mut pase_salt).expect("os rng");
        // Commissioning window boot-time policy (Task 14, `CommissioningWindow`'s
        // doc comment): open only for a device with no fabric yet — one already
        // on disk means this device was commissioned in an earlier run, so a
        // fresh PASE attempt now has no business succeeding. Decided *before*
        // the first `bring_up_mdns` call (fix round 1, review item 1) — that
        // call needs `window.is_open()` to know whether to publish a
        // commissionable advert at all.
        let window = if comm_server.fabrics().is_empty() {
            CommissioningWindow::Open {
                until: Instant::now() + COMMISSIONING_WINDOW_DURATION,
            }
        } else {
            CommissioningWindow::Closed
        };
        let mut mdns_ctx: Option<MdnsCtx> = None;
        let mut mdns_retry: Option<MdnsRetry> = None;
        match bring_up_mdns(&config, port, &comm_server, &window).await {
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
        Self {
            transport,
            port,
            pase_salt,
            mdns_retry,
            current_session: None,
            replay: GroupReplayGuard::new(),
            group,
            stimuli: stimuli.requests,
            stimuli_closed: false,
            state: NodeState {
                node,
                comm_server,
                mdns: mdns_ctx,
                subscription: None,
                window,
                config,
                endpoint_by_device: stimuli.endpoint_by_device,
                started_at: Instant::now(),
            },
        }
    }

    /// The receive loop: `sync_group_joins` then one `select!` over the
    /// unicast socket, the mDNS retry timer, the subscription report
    /// deadline, the commissioning-window deadline, the fail-safe deadline,
    /// and the group socket. Never returns on its own (`run`'s doc).
    async fn serve_forever(&mut self) -> Result<(), DeviceError> {
        let mut buf = [0u8; MAX_DATAGRAM];
        let mut gbuf = [0u8; MAX_DATAGRAM];
        loop {
            sync_group_joins(&mut self.group, &self.state.comm_server);
            tokio::select! {
                recv = self.transport.recv_from(&mut buf) => {
                    let (n, peer) = match recv {
                        Ok(v) => v,
                        Err(_) => continue, // best-effort responder — a transient recv error isn't fatal
                    };
                    self.on_unicast_datagram(&buf[..n], peer).await;
                }
                () = mdns_retry_deadline(&self.mdns_retry) => self.on_mdns_retry().await,
                () = subscription_deadline(&self.state.subscription) => self.on_subscription_due().await,
                () = commissioning_window_deadline(&self.state.window) => {
                    self.on_commissioning_window_expired().await
                }
                () = fail_safe_expiry_deadline(&self.state.comm_server) => {
                    self.on_fail_safe_expired().await
                }
                grecv = group_recv(&self.group.socket, &mut gbuf) => {
                    let Ok((n, from)) = grecv else { continue };
                    self.on_group_datagram(&gbuf[..n], from);
                }
                // 外からの刺激（`net::stimulus`）。送信ハンドルが全部 drop
                // されると `recv()` は `None` を返し続け、`Some(..)` パターンが
                // 恒久的に不成立になる = この分岐が毎周回すぐ完了して
                // ビジーループ化するので、そのとき `stimuli_closed` を立てて
                // 分岐前条件で自分を外す（`Device::run` が受信側を握って
                // いる限り起きないが、ハンドルを捨てた埋め込み利用でも
                // CPU を焼かないため）。
                req = self.stimuli.recv(), if !self.stimuli_closed => {
                    match req {
                        Some(req) => self.on_stimulus(req),
                        None => self.stimuli_closed = true,
                    }
                }
            }
        }
    }

    /// The active subscription's report deadline: send the dirty
    /// attributes (or an empty keep-alive) on a device-initiated exchange,
    /// drop the subscription if the report isn't acknowledged, then serve
    /// whatever requests were buffered during that ack-wait.
    async fn on_subscription_due(&mut self) {
        // The active subscription is due for a report: the dirty
        // attributes' current values, or an empty keep-alive. Both
        // go out on a fresh device-initiated exchange and both must
        // be acknowledged — anything else drops the subscription
        // (`send_subscription_report`'s doc comment).
        //
        // No reentrancy hazard against the datagram path (`on_secured_datagram`):
        // `select!` runs exactly one branch to completion per
        // iteration, so while this one awaits its StatusResponse it
        // is `SecureSession`'s own socket read — not the loop's —
        // that consumes datagrams. Requests arriving meanwhile are
        // buffered by `screen_with` and served by the drain below,
        // exactly like the ones landing during a reply's ack-wait.
        let delivered = match (
            self.state.subscription.as_mut(),
            self.current_session.as_mut(),
        ) {
            (Some(sub), Some((_, session, fabric_index))) => {
                send_subscription_report(session, *fabric_index, &mut self.state.node, sub).await
            }
            // A subscription that outlived its session has nothing
            // to report over — drop it.
            _ => false,
        };
        if !delivered {
            tracing::debug!(
                subscription_id = self.state.subscription.as_ref().map(|s| s.id),
                "subscription dropped: report was not acknowledged"
            );
            self.state.subscription = None;
        }
        let mut drop_session = false;
        if let Some((_, session, fabric_index)) = self.current_session.as_mut() {
            drop_session =
                drain_buffered_requests(session, *fabric_index, &mut self.state.serve_state())
                    .await
                    == ServeOutcome::DropSession;
        }
        // Task 6: same reasoning as the datagram path (`on_secured_datagram`) — a
        // buffered `RemoveFabric` piggybacked on this session's own
        // fabric ends the session too, not just one arriving as a
        // fresh datagram.
        if drop_session {
            self.current_session = None;
        }
    }

    /// One datagram off the unicast socket: decode the message header,
    /// drop group-session traffic (the group socket serves those), then
    /// route unsecured traffic to `on_unsecured_datagram` and secured
    /// traffic to the current session.
    async fn on_unicast_datagram(&mut self, datagram: &[u8], peer: SocketAddr) {
        let n = datagram.len();
        let Ok((header, off)) = MessageHeader::decode(datagram) else {
            tracing::debug!(peer = %peer, len = n, "datagram dropped: header decode failed");
            return;
        };
        if header.security_flags & SESSION_TYPE_MASK != 0 {
            tracing::debug!(peer = %peer, security_flags = header.security_flags, "group-session datagram on the unicast socket dropped (the group socket serves those)");
            return;
        }
        if header.session_id == 0 && header.security_flags == 0 {
            self.on_unsecured_datagram(header, &datagram[off..], peer)
                .await;
            return;
        }

        self.on_secured_datagram(datagram, &header, peer).await;
    }

    /// IM dispatch for one secured datagram: it must belong to the current
    /// session (sequential, one-at-a-time — see module doc), then
    /// `serve_secured` decrypts/screens it, serves the Interaction Model
    /// request, and drains any cross-exchange requests buffered meanwhile.
    /// A `RemoveFabric` that removed this session's own fabric ends the
    /// session (Task 6). Takes the raw datagram as well as the
    /// already-decoded header because `serve_secured` re-decodes it from
    /// the wire bytes.
    async fn on_secured_datagram(
        &mut self,
        datagram: &[u8],
        header: &MessageHeader,
        peer: SocketAddr,
    ) {
        // Secured traffic: only ever the current session (sequential,
        // one-at-a-time — see module doc).
        let Some((sid, session, fabric_index)) = self.current_session.as_mut() else {
            tracing::debug!(
                session_id = header.session_id,
                peer = %peer,
                "secured datagram dropped: no session established"
            );
            return;
        };
        if header.session_id != *sid {
            tracing::debug!(
                session_id = header.session_id,
                current_session_id = *sid,
                peer = %peer,
                "secured datagram dropped: session id does not match the current session"
            );
            return;
        }
        let outcome = serve_secured(
            datagram,
            peer,
            session,
            *fabric_index,
            &mut self.state.serve_state(),
        )
        .await;
        // Task 6: a `RemoveFabric` that removed this session's own
        // fabric — `session`/`fabric_index` (borrowed out of
        // `current_session` above) are no longer used past this
        // point in this iteration, so this is the first place the
        // borrow checker lets `current_session` be reassigned.
        if outcome == ServeOutcome::DropSession {
            self.current_session = None;
        }
    }

    /// An unsecured (`session_id == 0`) datagram: decode the protocol
    /// header, drop what can't start a session (non-initiator, standalone
    /// ack), classify (`classify_unsecured`) and admit (`admit_unsecured`)
    /// it, then hand a PASE/CASE opener to `establish_session`. `body` is
    /// the datagram from the protocol header onward (`MessageHeader::
    /// decode`'s `off`).
    async fn on_unsecured_datagram(
        &mut self,
        header: MessageHeader,
        body: &[u8],
        peer: SocketAddr,
    ) {
        let Ok((proto, body_off)) = ProtocolHeader::decode(body) else {
            tracing::debug!(peer = %peer, "unsecured datagram dropped: protocol header decode failed");
            return;
        };
        if !proto.initiator {
            tracing::debug!(
                peer = %peer,
                exchange_id = proto.exchange_id,
                opcode = format_args!("0x{:02X}", proto.opcode),
                "unsecured datagram dropped: not an initiator message"
            );
            return;
        }
        if proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
            && proto.opcode == OPCODE_MRP_STANDALONE_ACK
        {
            tracing::debug!(
                peer = %peer,
                exchange_id = proto.exchange_id,
                "unsecured datagram dropped: standalone MRP ack (no session to route it to)"
            );
            return;
        }
        let first = mat_controller::exchange::IncomingMessage {
            header,
            proto,
            payload: body[body_off..].to_vec(),
        };
        let flow = classify_unsecured(proto.protocol_id, proto.opcode);
        tracing::debug!(
            opcode = format_args!("0x{:02X}", proto.opcode),
            protocol_id = format_args!("0x{:04X}", proto.protocol_id),
            exchange_id = proto.exchange_id,
            peer = %peer,
            peer_node_id = ?header.source_node_id,
            ?flow,
            "unsecured datagram received"
        );
        let Some(flow) = admit_unsecured(flow, self.state.window.is_open()) else {
            tracing::debug!(
                peer = %peer,
                exchange_id = proto.exchange_id,
                "PASE datagram dropped: commissioning window closed"
            );
            return;
        };
        self.establish_session(flow, first, peer).await;
    }

    /// Session establishment: drive one PASE (`net::pase`) or CASE
    /// (`net::case`) handshake to completion for `first`, the opener just
    /// received, and on success make the result the current session
    /// (`install_session`). `UnsecuredFlow::Ignore` is a no-op.
    async fn establish_session(
        &mut self,
        flow: UnsecuredFlow,
        first: mat_controller::exchange::IncomingMessage,
        peer: SocketAddr,
    ) {
        match flow {
            UnsecuredFlow::Pase => {
                // Not collision-checked against a previous session: this
                // runtime keeps at most one "current session" alive at a
                // time (see module doc), so the only way a collision could
                // matter is astronomically unlikely (1/65535) and even then
                // just means the old session's next datagram gets fed into
                // the new one's `SecureSession` — a screen-level session-id
                // match with a peer address that no longer matches would
                // drop it harmlessly (`screen_with`'s `from != self.peer`
                // check).
                let local_session_id = mat_controller::case::random_nonzero_u16();
                let outcome = crate::net::pase::drive_established(
                    &self.transport,
                    peer,
                    first,
                    pase_config_for_window(
                        &self.state.window,
                        self.state.config.passcode,
                        &self.pase_salt,
                        local_session_id,
                    ),
                )
                .await;
                match outcome {
                    Ok((keys, peer_session_id)) => {
                        tracing::debug!(
                            local_session_id,
                            peer_session_id,
                            peer = %peer,
                            "PASE established"
                        );
                        let session = SecureSession::new_device_role(
                            Arc::clone(&self.transport),
                            peer,
                            local_session_id,
                            peer_session_id,
                            keys,
                            0, // PASE: both sides are node id 0 (spec §4.13)
                            0,
                        );
                        self.install_session(local_session_id, session, 0); // PASE: no fabric yet
                    }
                    // Established failure: best-effort responder, nothing
                    // more to do — the initiator's own retry/StatusReport
                    // handling covers it (logged so a failing
                    // interop run says *where* it stopped).
                    Err(e) => tracing::debug!(error = %e, peer = %peer, "PASE failed"),
                }
            }
            UnsecuredFlow::Case => {
                let local_session_id = mat_controller::case::random_nonzero_u16();
                // IPK rotation 後は keyset 0 の全 epoch から候補を展開する
                // （`core::case::expand_ipk_candidates` の doc 参照）。
                let fabrics = crate::core::case::expand_ipk_candidates(
                    self.state.comm_server.fabrics(),
                    &self.group.gk_store,
                );
                let outcome = crate::net::case::drive_established(
                    Arc::clone(&self.transport),
                    peer,
                    first,
                    fabrics,
                    local_session_id,
                )
                .await;
                match outcome {
                    Ok((session, fabric_index)) => {
                        tracing::debug!(
                            local_session_id,
                            fabric_index,
                            peer = %peer,
                            "CASE established"
                        );
                        self.install_session(local_session_id, session, fabric_index);
                    }
                    Err(e) => tracing::debug!(error = %e, peer = %peer, "CASE failed"),
                }
            }
            UnsecuredFlow::Ignore => {}
        }
    }

    /// Makes `session` the current session, replacing whatever was there.
    /// The active subscription (if any) belonged to the replaced session
    /// and is dropped with it — its reports could only ever have gone out
    /// over that session.
    fn install_session(&mut self, local_session_id: u16, session: SecureSession, fabric_index: u8) {
        self.current_session = Some((local_session_id, session, fabric_index));
        self.state.subscription = None;
    }

    /// The mDNS retry timer fired (`MdnsRetry`): try `bring_up_mdns` again.
    async fn on_mdns_retry(&mut self) {
        // `window.is_open()` read *now*, not whatever it was when
        // this retry was scheduled (fix round 1, review item 1):
        // the window may have closed (15-minute expiry or
        // `CommissioningComplete`) while mDNS was still down, and a
        // stale "was open when scheduled" read would let this retry
        // revive a commissionable advert for a window that already
        // sent its goodbye.
        match bring_up_mdns(
            &self.state.config,
            self.port,
            &self.state.comm_server,
            &self.state.window,
        )
        .await
        {
            Ok(ctx) => {
                tracing::info!("mDNS advertiser came up on retry");
                self.state.mdns = Some(ctx);
                self.mdns_retry = None;
            }
            Err(e) => {
                // Warned once already (either at startup, above, or
                // on the very first retry — from here on this is
                // expected/repetitive noise for a device on a
                // genuinely bad interface, hence debug not warn.
                tracing::debug!(error = %e, "mDNS retry attempt failed, will retry again");
                if let Some(state) = self.mdns_retry.as_mut() {
                    state.schedule_next();
                }
            }
        }
    }

    /// The boot window's 15-minute bound or an ECM window's
    /// `CommissioningTimeout` elapsed: close the window and pull the
    /// commissionable advert.
    async fn on_commissioning_window_expired(&mut self) {
        // Spec §5.4.2.3's 15-minute PASE window upper bound (boot
        // window) or spec §11.19.8.1's `CommissioningTimeout` (ECM
        // window, Task 4) has elapsed: close the window (no more
        // PASE admitted — see `admit_unsecured`) and send the same
        // commissionable-advert goodbye `CommissioningComplete`
        // sends on success (`set_commissionable(None)`, goodbye
        // wired since Task 8). No fabric rollback here — unlike
        // fail-safe expiry, an already-*established* PASE session
        // (if one happened to be mid-flight right at the deadline)
        // is left alone; this branch only stops *new* PASE attempts
        // from being admitted going forward.
        tracing::info!("commissioning window expired — closing");
        self.state.window = CommissioningWindow::Closed;
        if let Some(ctx) = self.state.mdns.as_ref() {
            ctx.mdns.set_commissionable(None).await;
        }
        // Task 4: this timer-driven close is the runtime noticing
        // on its own — unlike `CommissioningComplete`/`Revoke`
        // (dispatched IM commands the core cluster handler already
        // reacts to), nothing on the core side knows the deadline
        // just passed, so the runtime must tell it explicitly to
        // keep the AC cluster's `WindowStatus` attribute honest. A
        // no-op if this was the boot window (never opened an admin
        // window in the first place).
        self.state.comm_server.close_admin_window();
    }

    /// The fail-safe timer lapsed without `CommissioningComplete`: roll
    /// back the uncommitted fabric (if any) and its operational advert.
    async fn on_fail_safe_expired(&mut self) {
        // `expire_fail_safe` is the one primitive that both decides
        // "was there actually something to roll back" and does the
        // rollback — `Some(entry)` only for the fabric an
        // uncommitted `AddNOC` installed within this now-lapsed
        // window (see its doc comment). Nothing to do here beyond
        // that if it returns `None` (e.g. a plain `ArmFailSafe`
        // that never called `AddNOC`, or this branch racing another
        // caller that already consumed the same expiry) — the
        // `select!` loop simply comes back around, and
        // `fail_safe_expiry_deadline` reads as "no window open"
        // (`std::future::pending`) on the next iteration.
        let expired = self.state.comm_server.expire_fail_safe();
        tracing::debug!(
            rolled_back_fabric_index = ?expired.as_ref().map(|e| e.fabric_index),
            "fail-safe expiry deadline fired"
        );
        if let Some(entry) = expired {
            tracing::info!(
                fabric_id = entry.fabric_id,
                node_id = entry.node_id,
                "fail-safe expired without CommissioningComplete — rolling back fabric and its mDNS advert"
            );
            if let Some(ctx) = self.state.mdns.as_ref() {
                ctx.retire_operational(&entry).await;
            }
        }
    }

    /// One datagram off the group socket: authenticate/classify it
    /// (`classify_group_datagram`), apply the invokes, and mark what changed
    /// for the active subscription.
    fn on_group_datagram(&mut self, datagram: &[u8], from: SocketAddr) {
        let fabrics = self.state.comm_server.fabrics();
        let deps = GroupRxDeps {
            fabrics: &fabrics,
            gk_store: &self.group.gk_store,
            membership: &self.group.membership,
        };
        match classify_group_datagram(datagram, &deps, &mut self.replay) {
            Ok(batch) => {
                let mut ctx = InvokeCtx {
                    fabric_index: batch.fabric_index,
                    subject: Subject::group(batch.group_id),
                    ..InvokeCtx::default()
                };
                let changed =
                    self.state
                        .node
                        .handle_group_invoke(&batch.endpoints, &batch.invokes, &mut ctx);
                tracing::debug!(peer = %from, fabric_index = batch.fabric_index, group_id = batch.group_id, source_node_id = batch.source_node_id, endpoints = ?batch.endpoints, changed = changed.len(), "groupcast invoke applied");
                if let Some(sub) = self.state.subscription.as_mut() {
                    sub.note_outcome(&changed, &self.state.node);
                }
            }
            Err(reason) => {
                tracing::debug!(peer = %from, len = datagram.len(), ?reason, "groupcast datagram dropped")
            }
        }
    }

    /// One external stimulus (`net::stimulus`): resolve the device id to an
    /// endpoint, apply it to the node, and tell the active subscription
    /// what it produced — attribute changes (`note_changed`) *and* new
    /// events (`note_events`), the latter deciding whether the next report
    /// is due at the min-interval floor. The `oneshot` reply carries the
    /// outcome back to whoever sent the stimulus.
    ///
    /// Same shape as `on_group_datagram`: a change applied from a non-IM
    /// source, followed by the subscription bookkeeping. `self.state.node`
    /// and `self.state.subscription` are disjoint fields, so both can be
    /// borrowed at once.
    fn on_stimulus(&mut self, req: StimulusRequest) {
        let result = match self.state.endpoint_by_device.get(&req.device_id).copied() {
            None => Err(StimulusApplyError::UnknownDevice(req.device_id.clone())),
            Some(endpoint) => {
                let ts = self.state.system_timestamp_ms();
                match self.state.node.stimulate(endpoint, &req.stimulus, ts) {
                    Ok(out) => {
                        tracing::debug!(
                            device = %req.device_id,
                            endpoint,
                            stimulus = ?req.stimulus,
                            changed = out.changed.len(),
                            events = out.event_numbers.len(),
                            "stimulus applied"
                        );
                        if let Some(sub) = self.state.subscription.as_mut() {
                            sub.note_outcome(&out.changed, &self.state.node);
                        }
                        Ok(out)
                    }
                    Err(e) => {
                        tracing::debug!(
                            device = %req.device_id,
                            endpoint,
                            stimulus = ?req.stimulus,
                            error = %e,
                            "stimulus refused"
                        );
                        Err(StimulusApplyError::Node(e))
                    }
                }
            }
        };
        // The sender may have given up waiting (dropped its `oneshot`
        // receiver) — the stimulus still happened, so this is not an error.
        let _ = req.reply.send(result);
    }
}

/// Multicast join/leave against `group.socket`, differenced from the last
/// call by `GroupSocket::sync_joins` itself — a no-op when membership hasn't
/// changed since the previous iteration. Run at the top of every `run` loop
/// iteration (before `select!`) so an `AddGroup`, `RemoveFabric`, or
/// fail-safe rollback that changed the set of joined groups is picked up on
/// the very next spin, without any dedicated event plumbing from those call
/// sites back into this loop. A `None` socket (bind failed at `Device::new`
/// time) makes this a no-op.
fn sync_group_joins(group: &mut GroupRx, comm_server: &CommissioningServer) {
    if let Some(sock) = group.socket.as_mut() {
        sock.sync_joins(&desired_group_addrs(
            &comm_server.fabrics(),
            &group.membership,
        ));
    }
}

mod classify;
mod mdns_up;
mod serve;
mod subscribe;
mod window;

#[cfg(test)]
use classify::OPCODE_CASE_SIGMA1;
use classify::{admit_unsecured, classify_unsecured, remove_fabric_drops_session, UnsecuredFlow};
use mdns_up::{
    advertise_added_fabric, bring_up_mdns, close_window_on_commissioning_complete,
    mdns_retry_deadline, reconcile_admin_window, retire_removed_fabric, MdnsCtx, MdnsRetry,
};
#[cfg(test)]
use mdns_up::{
    MDNS_RETRY_BACKOFF_THRESHOLD, MDNS_RETRY_INTERVAL_INITIAL, MDNS_RETRY_INTERVAL_LONG,
};
use serve::{
    await_peer_status_ok, drain_buffered_requests, is_status_response_ok, reply_cfg, serve_secured,
    session_subject, ServeOutcome, ServeState, REPORT_CHUNK_BUDGET, REPORT_STATUS_TIMEOUT,
};
#[cfg(test)]
use subscribe::{chunk_events, fit_events};
use subscribe::{
    send_subscription_report, serve_subscribe_request, subscription_deadline, SubscribeOutcome,
};
use window::{
    admin_window_action, advert_params_for_window, apply_window_request,
    commissioning_window_deadline, fail_safe_expiry_deadline, pase_config_for_window,
    AdminWindowAction, CommissioningWindow, COMMISSIONING_WINDOW_DURATION,
};

#[cfg(test)]
mod socket_tests;
#[cfg(test)]
mod tests;
