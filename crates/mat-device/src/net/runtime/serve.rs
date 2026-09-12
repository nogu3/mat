use super::*;

/// One secured request's MRP retry budget while replying — generous (this
/// is a device answering, not a controller racing a user-visible deadline).
pub(super) fn reply_cfg() -> MrpConfig {
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
pub(super) const REPORT_CHUNK_BUDGET: usize = 900;

/// How long to wait for the peer's `StatusResponse` to a ReportData we
/// sent, when it didn't come piggybacked on the MRP ack — a LAN round-trip
/// to a controller/hub, same budget `serve_read_request_chunked` uses.
pub(super) const REPORT_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// What `serve_secured_message` should do once it's finished dispatching
/// and replying to one request — `Continue` (the overwhelming common case)
/// or `DropSession` (Task 6: a `RemoveFabric` this dispatch removed the
/// invoking session's own fabric — spec §2.5.11, removing a fabric SHALL
/// terminate any session associated with it). Propagated back up through
/// `drain_buffered_requests`/`serve_secured` to `on_secured_datagram`, which
/// is the only place that actually owns `current_session` and can set it to
/// `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServeOutcome {
    Continue,
    DropSession,
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
///
/// Returns `ServeOutcome::DropSession` (Task 6) if this datagram's dispatch
/// — or, when it wasn't, one of the buffered requests drained afterward —
/// contained a `RemoveFabric` that removed the invoking session's own
/// fabric (`remove_fabric_drops_session`); `on_secured_datagram` then sets
/// `current_session = None`. `DropSession` skips the buffered-request drain
/// below entirely (rather than draining what's left first): the session is
/// about to be torn down, so there is no longer anywhere to route replies
/// for whatever else `screen_with` buffered — and per spec §2.5.11 removing
/// a fabric SHALL terminate sessions associated with it, so continuing to
/// serve *other* requests on it, even briefly, would be wrong regardless.
pub(super) async fn serve_secured(
    buf: &[u8],
    from: SocketAddr,
    session: &mut SecureSession,
    fabric_index: u8,
    state: &mut ServeState<'_>,
) -> ServeOutcome {
    let msg = match session.deliver_request(buf, from).await {
        Ok(Some(msg)) => msg,
        Ok(None) => {
            tracing::debug!(
                peer = %from,
                "secured datagram dropped: deliver_request returned no message (standalone ack or screened-out)"
            );
            return ServeOutcome::Continue;
        }
        Err(e) => {
            tracing::debug!(error = %e, peer = %from, "secured datagram dropped: decrypt/screen failure");
            return ServeOutcome::Continue; // decrypt/screen failure — drop, don't kill the session on noise
        }
    };
    if serve_secured_message(msg, session, fabric_index, state).await == ServeOutcome::DropSession {
        return ServeOutcome::DropSession;
    }

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
    drain_buffered_requests(session, fabric_index, state).await
}

/// Serves every peer-initiated request `screen_with` buffered
/// (`peer_initiated`) while this side was busy waiting for an ack — see
/// `serve_secured`'s comment above the original call site for why dropping
/// them would be a permanent loss. Also run after a device-initiated
/// subscription report, whose own ack-wait reads the socket the same way.
/// Stops (returning `ServeOutcome::DropSession`) as soon as any drained
/// request drops the session — same reasoning as `serve_secured`'s doc
/// comment: nothing buffered after that point should still be served.
pub(super) async fn drain_buffered_requests(
    session: &mut SecureSession,
    fabric_index: u8,
    state: &mut ServeState<'_>,
) -> ServeOutcome {
    while let Some(buffered) = session.take_buffered_request() {
        if buffered.proto.protocol_id != PROTOCOL_ID_INTERACTION_MODEL {
            continue;
        }
        if serve_secured_message(buffered, session, fabric_index, state).await
            == ServeOutcome::DropSession
        {
            return ServeOutcome::DropSession;
        }
    }
    ServeOutcome::Continue
}

/// Dispatches one already-classified Interaction Model request (`msg`) to
/// `node`, replies, and reacts to the two commissioning milestones (see
/// `serve_secured`'s original doc comment for why: AddNOC success detected
/// by fabric count growth, CommissioningComplete by decoding the request).
/// Split out from `serve_secured` so both the datagram just read off the
/// socket and any buffered peer-initiated request drained afterward go
/// through the identical path. `fabric_index` is this session's fabric
/// index (0 for PASE) — threaded into `ReadCtx` for every `ReadRequest`.
///
/// Returns `ServeOutcome` (Task 6) — `DropSession` once, per dispatch
/// iteration, if a `RemoveFabric` handled this iteration removed the
/// invoking session's own fabric (see the `RemoveFabric` block near the
/// end of the loop body); every other exit path (non-IM traffic, Read/
/// Subscribe's own flows, a `handle_im` decode failure, or the ack-wait
/// loop simply running out of same-exchange follow-ups) is `Continue`.
async fn serve_secured_message(
    msg: mat_controller::exchange::IncomingMessage,
    session: &mut SecureSession,
    fabric_index: u8,
    state: &mut ServeState<'_>,
) -> ServeOutcome {
    // Destructured (rather than used through `state.*`) so the borrow
    // checker sees the four fields as independent borrows — the
    // subscription is updated while `node` is also borrowed.
    let ServeState {
        node,
        comm_server,
        mdns,
        subscription,
        window,
        config,
    } = state;
    let node: &mut Node = node;
    let comm_server: &CommissioningServer = comm_server;
    let mdns: Option<&MdnsCtx> = *mdns;
    let config: &DeviceConfig = config;

    // 同一 exchange の後続リクエストを処理し切るまで回るループ。Timed
    // Interaction（spec §8.9.4）では initiator が StatusResponse(SUCCESS) の
    // 受領後、**同じ exchange** で timed Invoke/Write を送ってくる（ack は
    // そこに piggyback）。`reply_reliable` はそれを `Ok(Some(msg))` として
    // 返すので、捨てずにここで続けて処理する（捨てると invoke は MRP ack
    // 済みのまま永遠に応答されず、Google Play Services スタックの
    // commissioning が中断する — 2026-08-18 実測）。
    let mut msg = msg;
    loop {
        if msg.proto.protocol_id != PROTOCOL_ID_INTERACTION_MODEL {
            // Secure-channel traffic on an established session (e.g. a
            // device-initiated-exchange StatusReport) — out of M1 scope.
            return ServeOutcome::Continue;
        }

        // ReadRequest gets its own chunk-aware flow (Task 6) instead of going
        // through `Node::handle_im` (whose `handle_read` always answers with a
        // single message) — see `serve_read_request_chunked`'s doc comment.
        // Reads never trigger the AddNOC/CommissioningComplete milestones
        // below (those are Invoke-only), so returning here is safe.
        if msg.proto.opcode == im::OPCODE_READ_REQUEST {
            serve_read_request_chunked(&msg, session, fabric_index, node).await;
            return ServeOutcome::Continue;
        }

        // SubscribeRequest likewise owns its whole interaction (priming chunks
        // + SubscribeResponse on this exchange, Task 12) rather than producing
        // one reply through `Node::handle_im`. Success installs the node's one
        // active subscription; failure anywhere in the flow leaves it with
        // none — including tearing down whatever was subscribed before, since
        // this same peer just asked to start over. An up-front `INVALID_ACTION`
        // refusal leaves the existing subscription alone only when the request
        // asked `KeepSubscriptions=true` (`SubscribeOutcome::Rejected`); with
        // `KeepSubscriptions=false` the old subscription is torn down first,
        // as chip does (`SubscribeOutcome::TornDown`).
        if msg.proto.opcode == im::OPCODE_SUBSCRIBE_REQUEST {
            match serve_subscribe_request(&msg, session, fabric_index, node).await {
                SubscribeOutcome::Installed(sub) => **subscription = Some(sub),
                SubscribeOutcome::TornDown => **subscription = None,
                SubscribeOutcome::Rejected => {}
            }
            return ServeOutcome::Continue;
        }

        tracing::debug!(
            im_opcode = format_args!("0x{:02X}", msg.proto.opcode),
            exchange_id = msg.proto.exchange_id,
            request = ?im::decode_invoke_request(&msg.payload)
                .ok()
                .map(|r| (r.endpoint, r.cluster, r.command)),
            "IM request"
        );

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
            fabric_index,
            subject: session_subject(session),
            ..InvokeCtx::default()
        };
        // Invoke/write dispatch: no `IsFabricFiltered` on the wire for those
        // requests, so use the fabric-filtered side — the same default the
        // read/subscribe decoders apply when the flag is absent.
        let read_ctx = ReadCtx {
            fabric_index,
            fabric_filtered: true,
            subject: session_subject(session),
        };
        let fabrics_before = comm_server.fabrics().len();
        let Ok(outcome) = node.handle_im(msg.proto.opcode, &msg.payload, &mut ctx, &read_ctx)
        else {
            return ServeOutcome::Continue;
        };
        let ImOutcome {
            opcode: resp_opcode,
            payload: resp_payload,
            changed,
        } = outcome;
        // Anything this request changed that the active subscription covers
        // becomes dirty — the `select!` report branch (`on_subscription_due`) picks it up at
        // the subscription's next deadline. Recorded *before* the reply is
        // sent so a change is never lost to a failing reply.
        if let Some(sub) = subscription.as_mut() {
            sub.note_outcome(&changed, node);
        }
        let reply_result = session
            .reply_reliable(
                &msg,
                PROTOCOL_ID_INTERACTION_MODEL,
                resp_opcode,
                &resp_payload,
                &reply_cfg(),
            )
            .await;
        tracing::debug!(
            resp_opcode = format_args!("0x{:02X}", resp_opcode),
            exchange_id = msg.proto.exchange_id,
            payload_len = resp_payload.len(),
            ok = reply_result.is_ok(),
            error = reply_result.as_ref().err().map(|e| e.to_string()),
            "IM reply sent"
        );

        advertise_added_fabric(comm_server, mdns, fabrics_before).await;
        reconcile_admin_window(comm_server, mdns, window, config).await;
        close_window_on_commissioning_complete(
            resp_opcode,
            req_cluster_command,
            &resp_payload,
            comm_server,
            mdns,
            window,
        )
        .await;
        if retire_removed_fabric(comm_server, mdns, fabric_index).await == ServeOutcome::DropSession
        {
            return ServeOutcome::DropSession;
        }

        // `reply_reliable` が実メッセージ（同一 exchange の後続リクエスト —
        // 典型は Timed Interaction の timed Invoke）を返したら、この iteration
        // と同じ経路で続けて処理する。ack 完了 (`Ok(None)`) と送信失敗 (`Err`)
        // はどちらもこのメッセージ列の終端。
        match reply_result {
            Ok(Some(next)) => msg = next,
            _ => return ServeOutcome::Continue,
        }
    }
}

/// Everything one secured message is allowed to touch, bundled so
/// `serve_secured`/`serve_secured_message` keep a readable arity as the
/// runtime grows state (Task 12 added the subscription). Built by
/// `NodeState::serve_state` from the runtime's owned `NodeState`; the
/// fields are destructured inside `serve_secured_message` so they stay
/// independent borrows.
pub(super) struct ServeState<'a> {
    pub(super) node: &'a mut Node,
    pub(super) comm_server: &'a CommissioningServer,
    /// `None` when `bring_up_mdns` hasn't succeeded (see `run`'s doc).
    pub(super) mdns: Option<&'a MdnsCtx>,
    /// The node's single active subscription, if any (see
    /// `net::subscription`'s module doc for why there's only one).
    pub(super) subscription: &'a mut Option<ActiveSubscription>,
    /// The commissioning window (Task 14, widened by Task 4). Mutated here
    /// by: a staged `WindowRequest` opening it (`Open -> EnhancedOpen`,
    /// `apply_window_request`); `CommissioningComplete` succeeding; and
    /// `RevokeCommissioning` having cleared the core's admin window out from
    /// under an `EnhancedOpen` runtime window — all three detected per
    /// dispatch iteration, same place as the AddNOC fabric-diff check. The
    /// 15-minute/`CommissioningTimeout`-elapsed close happens in
    /// `on_commissioning_window_expired`, outside any `ServeState`.
    pub(super) window: &'a mut CommissioningWindow,
    /// Boot-time identity (passcode/discriminator/vendor+product id) — Task
    /// 4 needs `config.discriminator`/`vendor_id`/`product_id` to rebuild
    /// the commissionable advert when a `WindowRequest` reopens the window,
    /// and `config.passcode` was already reachable via `establish_session`
    /// but not through `ServeState` until now (`pase_config_for_window` is
    /// called directly in `establish_session`, not from here — this is
    /// only for the mDNS-side advert rebuild).
    pub(super) config: &'a DeviceConfig,
}

/// How many mis-addressed peer-initiated messages `await_peer_status_ok`
/// will set aside while waiting for its StatusResponse. Matches
/// `SecureSession`'s own `peer_initiated` capacity: a peer that sends more
/// unrelated requests than that inside one chunk's status wait is flooding,
/// and the wait gives up rather than growing without bound.
const MAX_DEFERRED_REQUESTS: usize = 32;

/// Waits for the peer's `StatusResponse(0)` to a ReportData we just sent on
/// an exchange **the peer initiated** (a priming chunk, or a non-final read
/// chunk). `piggybacked` is whatever `reply_reliable` already had in hand —
/// real controllers (`SecureSession::subscribe_wildcard`, chip-tool) answer
/// with the StatusResponse itself rather than a standalone ack, so that's
/// the normal path; the fallback pulls messages with `recv_request`, whose
/// `AnyPeerInitiated` filter is the only one that can deliver on an
/// exchange we didn't initiate (plain `recv` requires `!initiator` and so
/// would sit here until it timed out).
///
/// **Nothing pulled here is ever discarded.** A message on another exchange
/// is a request the peer expects an answer to, and `screen_with` has
/// already MRP-acked it — dropping it would make the peer wait forever with
/// no retransmit to save it (the "ack-then-drop of cross-exchange secured
/// requests" class of bug). Such messages are set aside and handed back to
/// `SecureSession`'s buffer in their original order on the way out
/// (`requeue_buffered_request`), where `serve_secured`'s drain picks them
/// up once the chunked interaction finishes.
///
/// ## Why this loop terminates
///
/// Every iteration does exactly one of: return on a match, return on the
/// deadline, or move one message from `SecureSession`'s buffer/socket into
/// the local `deferred` vec — which is *not* fed back until the loop is
/// over, so a set-aside message can never be re-pulled within this call.
/// That leaves two bounded sources of iterations: the buffer, which is
/// finite (`MAX_PEER_INITIATED_BUFFER`) and strictly shrinks as it is
/// drained, and the socket, whose reads are bounded by `REPORT_STATUS_
/// TIMEOUT` (`recv_request` gets the *remaining* time, so buffered
/// messages — which return instantly — can't extend the total wait).
/// `MAX_DEFERRED_REQUESTS` is a third, redundant bound covering a peer that
/// floods new requests faster than they can be set aside.
pub(super) async fn await_peer_status_ok(
    session: &mut SecureSession,
    piggybacked: Option<mat_controller::exchange::IncomingMessage>,
    exchange_id: u16,
) -> bool {
    let deadline = Instant::now() + REPORT_STATUS_TIMEOUT;
    let mut candidate = piggybacked;
    let mut deferred: Vec<mat_controller::exchange::IncomingMessage> = Vec::new();
    let mut ok = false;

    loop {
        let msg = match candidate.take() {
            Some(m) => m,
            None => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    tracing::debug!(
                        exchange_id,
                        "chunk ack: timed out waiting for StatusResponse"
                    );
                    break;
                }
                match session.recv_request(remaining).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!(exchange_id, error = %e, "chunk ack: no StatusResponse");
                        break;
                    }
                }
            }
        };
        if msg.proto.exchange_id == exchange_id {
            ok = is_status_response_ok(&msg);
            break;
        }
        // Someone else's exchange: not ours to answer here, not ours to
        // throw away either.
        tracing::debug!(
            exchange_id,
            other_exchange_id = msg.proto.exchange_id,
            opcode = format_args!("0x{:02X}", msg.proto.opcode),
            "chunk ack: setting aside a request on another exchange"
        );
        deferred.push(msg);
        if deferred.len() >= MAX_DEFERRED_REQUESTS {
            tracing::debug!(
                exchange_id,
                "chunk ack: too many unrelated requests while waiting; giving up"
            );
            break;
        }
    }

    // Back to the front of the buffer, oldest first (push each to the
    // front in reverse, so the head ends up being the one pulled first).
    for msg in deferred.into_iter().rev() {
        session.requeue_buffered_request(msg);
    }
    ok
}

/// Whether `msg` is a `StatusResponse(SUCCESS)` — the acknowledgement
/// every non-suppressed ReportData expects (spec §8.9.2.3). Anything else
/// (wrong opcode, non-zero status, malformed payload) means the peer isn't
/// following the interaction and the caller gives up on it.
pub(super) fn is_status_response_ok(msg: &mat_controller::exchange::IncomingMessage) -> bool {
    if msg.proto.opcode != im::OPCODE_STATUS_RESPONSE {
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            opcode = format_args!("0x{:02X}", msg.proto.opcode),
            "expected StatusResponse, got another opcode"
        );
        return false;
    }
    match im::decode_status_response(&msg.payload) {
        Ok(0) => true,
        other => {
            tracing::debug!(
                exchange_id = msg.proto.exchange_id,
                status = ?other,
                "StatusResponse was not SUCCESS"
            );
            false
        }
    }
}

/// Serves one `ReadRequest` with `Node::read_chunks`'s chunked reply flow
/// (Task 6), bypassing `Node::handle_im`/`handle_read` (which only ever
/// return a single message) entirely. Mirrors `SecureSession::
/// subscribe_wildcard`'s priming-report chunk loop on the *initiator* side
/// (`mat_controller::session::subscribe::subscribe_wildcard`): every chunk
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
    let Ok(req) = im::decode_read_request_message(&msg.payload) else {
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            "ReadRequest dropped: undecodable"
        );
        return;
    };
    let paths = req.paths;
    let read_ctx = ReadCtx {
        fabric_index,
        fabric_filtered: req.fabric_filtered,
        subject: session_subject(session),
    };
    let chunks = node.read_chunks(&paths, &read_ctx, REPORT_CHUNK_BUDGET, None, false);
    let last_index = chunks.len().saturating_sub(1);
    tracing::debug!(
        exchange_id = msg.proto.exchange_id,
        ?paths,
        fabric_filtered = req.fabric_filtered,
        chunks = chunks.len(),
        "ReadRequest"
    );

    for (i, chunk) in chunks.into_iter().enumerate() {
        let is_last = i == last_index;
        let reply_result = session
            .reply_reliable(
                msg,
                PROTOCOL_ID_INTERACTION_MODEL,
                im::OPCODE_REPORT_DATA,
                &chunk,
                &reply_cfg(),
            )
            .await;
        tracing::debug!(
            resp_opcode = format_args!("0x{:02X}", im::OPCODE_REPORT_DATA),
            exchange_id = msg.proto.exchange_id,
            payload_len = chunk.len(),
            chunk_index = i,
            is_last,
            ok = reply_result.is_ok(),
            error = reply_result.as_ref().err().map(|e| e.to_string()),
            "IM reply sent (ReportData chunk)"
        );
        let Ok(piggybacked) = reply_result else {
            return; // ack never came — exchange is dead, give up
        };

        if is_last {
            return; // final chunk: no StatusResponse expected, done
        }

        // Same wait the subscription's priming loop does — this is the
        // peer's exchange, so `await_peer_status_ok`'s `recv_request`
        // fallback is what can actually deliver a StatusResponse that
        // didn't come piggybacked on the ack (plain `session.recv` filters
        // out messages on exchanges we didn't initiate and would sit here
        // until it timed out).
        if !await_peer_status_ok(session, piggybacked, msg.proto.exchange_id).await {
            return;
        }
    }
}

/// The ACL identity of a device-role CASE session: node id + CATs as read
/// off the peer's NOC by `net::case` (`SecureSession::peer_cats`). On a
/// PASE session both are their placeholders (node 0, no CATs), which the
/// fabric-0 bypass in `datamodel::acl_allows` never consults.
pub(super) fn session_subject(session: &SecureSession) -> Subject {
    Subject::new(session.peer_node_id(), session.peer_cats())
}
