use super::*;

/// How long this device is willing to stay silent on a subscription before
/// it must send *something* (spec §8.10: the MaxInterval the device
/// answers with may be anywhere at or below the subscriber's requested
/// ceiling). The requested ceiling is clamped into this range: below the
/// floor a chatty controller would have this device sending keep-alives
/// faster than a battery-less-but-still-sequential runtime wants to; above
/// the ceiling a subscription would take too long to notice a dead peer.
const MIN_MAX_INTERVAL_S: u16 = 3;
const MAX_MAX_INTERVAL_S: u16 = 60;

/// A random non-zero `SubscriptionId` (spec §8.10.3). Not collision-checked
/// for the same reason the runtime's session ids aren't (see the comment at
/// the first `random_nonzero_u16()` call in `establish_session`): this
/// runtime holds at most one subscription at a time, so there is nothing to
/// collide with.
fn random_subscription_id() -> u32 {
    loop {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).expect("os rng");
        let v = u32::from_le_bytes(b);
        if v != 0 {
            return v;
        }
    }
}

/// Resolves once at the active subscription's next report deadline, or
/// never (`std::future::pending`) when nothing is subscribed — same
/// `select!`-branch shape as `mdns_retry_deadline`/
/// `fail_safe_expiry_deadline`, so a subscription's timer can't block
/// datagram serving and its absence simply makes the branch inert.
pub(super) async fn subscription_deadline(subscription: &Option<ActiveSubscription>) {
    match subscription {
        Some(sub) => tokio::time::sleep_until(sub.next_report_deadline()).await,
        None => std::future::pending().await,
    }
}

/// What `serve_subscribe_request` did to the node's single subscription slot.
pub(super) enum SubscribeOutcome {
    /// A new subscription is live (replaces whatever was there).
    Installed(ActiveSubscription),
    /// The request was accepted but the interaction failed partway
    /// (undecodable request, priming send failure, missing ack) — the peer
    /// asked to start over and the flow broke, so nothing is subscribed.
    TornDown,
    /// The request was refused up front with `StatusResponse(INVALID_ACTION)`
    /// (spec §8.10: no readable path) *and* it asked `KeepSubscriptions=true`
    /// — a refusal, not a restart, so the existing subscription (if any) is
    /// left alone, as chip does. A refusal with `KeepSubscriptions=false`
    /// tears the existing subscription down instead (`TornDown`) — chip
    /// applies that teardown *before* path validation, so it happens
    /// regardless of why the request was ultimately refused.
    Rejected,
}

/// Serves one `SubscribeRequest` end to end (spec §8.10): priming
/// ReportData (chunked, each chunk acknowledged with `StatusResponse(0)`
/// by the initiator) followed by a `SubscribeResponse`, all on the
/// requesting exchange, and returns a `SubscribeOutcome`: `Installed` with
/// the resulting `ActiveSubscription` on success; `TornDown` if any step
/// failed mid-flow, in which case this device simply has no subscription
/// and the initiator is free to retry from scratch (same
/// abort-and-let-the-initiator-retry policy `serve_read_request_chunked`
/// uses for chunked reads); or, if the request was refused up front with
/// `StatusResponse(INVALID_ACTION)` before any of that flow started,
/// `Rejected` when the request asked `KeepSubscriptions=true` (leaves any
/// existing subscription on this node untouched) or `TornDown` when it
/// asked `KeepSubscriptions=false` — chip tears down the subscriber's
/// existing subscriptions *before* path validation, so a refused request
/// with `KeepSubscriptions=false` still discards them.
///
/// Two ways this differs from a chunked read (both are why priming needs
/// its own flow rather than reusing `serve_read_request_chunked`): every
/// chunk carries the SubscriptionId and none of them suppress the
/// response — not even the last, because the interaction isn't over until
/// the `SubscribeResponse` goes out on the same exchange.
///
/// The subscription is registered by the caller only *after* all of that
/// completes, so a half-finished subscribe never leaves a live
/// subscription behind reporting to a controller that never got its
/// SubscribeResponse.
pub(super) async fn serve_subscribe_request(
    msg: &mat_controller::exchange::IncomingMessage,
    session: &mut SecureSession,
    fabric_index: u8,
    node: &mut Node,
) -> SubscribeOutcome {
    let Ok(req) = im::decode_subscribe_request(&msg.payload) else {
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            "SubscribeRequest dropped: undecodable"
        );
        return SubscribeOutcome::TornDown;
    };
    let subscription_id = random_subscription_id();
    // The device picks the MaxInterval it can actually honor, at or below
    // the requested ceiling (spec §8.10) — `SubscribeResponse` tells the
    // subscriber what it settled on.
    let max_interval_s = req
        .max_interval_ceiling_s
        .clamp(MIN_MAX_INTERVAL_S, MAX_MAX_INTERVAL_S);
    let max_interval = Duration::from_secs(u64::from(max_interval_s));
    // A floor above the interval we just settled on would starve the
    // subscription of its own keep-alives; clamp it down rather than
    // reject the (otherwise legal) request.
    let min_interval = Duration::from_secs(u64::from(req.min_interval_floor_s)).min(max_interval);

    let read_ctx = ReadCtx {
        fabric_index,
        fabric_filtered: req.fabric_filtered,
        subject: session_subject(session),
    };

    // spec §8.10 / chip `ParseAttributePaths`: a request none of whose
    // paths can yield anything this subject may read is refused outright
    // rather than answered with an empty priming report and a dead
    // subscription. (Concrete paths always count — their refusal shows up
    // as a status entry in the priming report; see
    // `Node::has_readable_path`.) An **event-only** request (empty
    // AttributeRequests, a non-empty EventRequests this subject may
    // receive) is just as legitimate a subscription, so either side
    // qualifying is enough — a request has to be readable in *neither* to
    // be refused.
    if !node.has_readable_path(&req.paths, &read_ctx)
        && !node.has_readable_event_path(&req.event_paths, &read_ctx)
    {
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            paths = ?req.paths,
            event_paths = ?req.event_paths,
            subject = ?read_ctx.subject,
            fabric_index,
            "SubscribeRequest rejected: no readable attribute or event path (INVALID_ACTION)"
        );
        let reply_result = session
            .reply_reliable(
                msg,
                PROTOCOL_ID_INTERACTION_MODEL,
                im::OPCODE_STATUS_RESPONSE,
                &im::encode_status_response(im::STATUS_INVALID_ACTION),
                &reply_cfg(),
            )
            .await;
        match &reply_result {
            Err(e) => {
                tracing::debug!(exchange_id = msg.proto.exchange_id, error = %e, "INVALID_ACTION StatusResponse not delivered");
            }
            Ok(Some(piggybacked)) => {
                // A peer message piggybacked on the ack — not consumed here
                // (this refusal has nothing more to do with it), but logged
                // so it's visibly dropped rather than silently ack-then-lost.
                tracing::debug!(
                    exchange_id = piggybacked.proto.exchange_id,
                    opcode = format_args!("0x{:02X}", piggybacked.proto.opcode),
                    "INVALID_ACTION StatusResponse ack carried a piggybacked peer message, discarded"
                );
            }
            Ok(None) => {}
        }
        // chip tears down the subscriber's existing subscriptions *before*
        // path validation, so a refusal only leaves them alone when the
        // request asked KeepSubscriptions=true; KeepSubscriptions=false
        // discards them here too, as chip does.
        return if req.keep_subscriptions {
            SubscribeOutcome::Rejected
        } else {
            SubscribeOutcome::TornDown
        };
    }

    // The EventNumber the priming report starts from: `EventFilterIB::
    // EventMin` if the request carried one, otherwise everything still in
    // the log (spec §8.9.2.4 — a fresh subscriber with no history asks for
    // 0 and gets whatever the device retained).
    let event_min = req.event_min.unwrap_or(0);
    let event_entries = node.event_entries(&req.event_paths, event_min, &read_ctx);
    // The event chunks are appended *after* the attribute ones (spec
    // §8.9.2.3's ReportData shape puts AttributeReports before
    // EventReports), so the attribute side must be told a trailer follows —
    // otherwise its last chunk would say `more_chunks=false` and the
    // subscriber would stop reading before the events arrived.
    let mut chunks = node.read_chunks(
        &req.paths,
        &read_ctx,
        REPORT_CHUNK_BUDGET,
        Some(subscription_id),
        !event_entries.is_empty(),
    );
    chunks.extend(chunk_events(
        &event_entries,
        REPORT_CHUNK_BUDGET,
        subscription_id,
    ));
    tracing::debug!(
        exchange_id = msg.proto.exchange_id,
        subscription_id,
        paths = ?req.paths,
        event_paths = ?req.event_paths,
        event_min,
        events = event_entries.len(),
        fabric_filtered = req.fabric_filtered,
        min_interval_floor_s = req.min_interval_floor_s,
        max_interval_ceiling_s = req.max_interval_ceiling_s,
        max_interval_s,
        chunks = chunks.len(),
        "SubscribeRequest"
    );

    for (i, chunk) in chunks.iter().enumerate() {
        let reply_result = session
            .reply_reliable(
                msg,
                PROTOCOL_ID_INTERACTION_MODEL,
                im::OPCODE_REPORT_DATA,
                chunk,
                &reply_cfg(),
            )
            .await;
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            subscription_id,
            chunk_index = i,
            payload_len = chunk.len(),
            ok = reply_result.is_ok(),
            error = reply_result.as_ref().err().map(|e| e.to_string()),
            "priming ReportData chunk sent"
        );
        let Ok(piggybacked) = reply_result else {
            return SubscribeOutcome::TornDown; // ack never came — exchange is dead, give up
        };
        if !await_peer_status_ok(session, piggybacked, msg.proto.exchange_id).await {
            return SubscribeOutcome::TornDown;
        }
    }

    let resp = im::encode_subscribe_response(subscription_id, max_interval_s);
    let reply_result = session
        .reply_reliable(
            msg,
            PROTOCOL_ID_INTERACTION_MODEL,
            im::OPCODE_SUBSCRIBE_RESPONSE,
            &resp,
            &reply_cfg(),
        )
        .await;
    tracing::debug!(
        exchange_id = msg.proto.exchange_id,
        subscription_id,
        ok = reply_result.is_ok(),
        error = reply_result.as_ref().err().map(|e| e.to_string()),
        "SubscribeResponse sent"
    );
    if reply_result.is_err() {
        return SubscribeOutcome::TornDown;
    }

    SubscribeOutcome::Installed(ActiveSubscription {
        id: subscription_id,
        paths: req.paths,
        fabric_filtered: req.fabric_filtered,
        min_interval,
        max_interval,
        // The priming report counts as this subscription's first report:
        // the keep-alive clock starts at the end of the subscribe
        // interaction, not before it.
        last_report_at: Instant::now(),
        dirty: Vec::new(),
        event_paths: req.event_paths,
        // Everything the priming report just delivered is behind us: the
        // next report starts at the number the *next* event will get, so
        // nothing is replayed and nothing is skipped.
        next_event: node.next_event_number(),
        pending_urgent: false,
    })
}

/// The priming report's event chunks, split under the same budget the
/// attribute side (`Node::read_chunks`) uses, and probed the same way: each
/// candidate batch is measured in its `more_chunks=true` shape (the larger
/// one), so a batch that only fits when encoded as the final chunk is never
/// let through.
///
/// The last chunk is `more_chunks=false, suppress_response=false` — the
/// events are the end of the *report*, but not of the interaction: a
/// `SubscribeResponse` still follows on the same exchange, so the
/// subscriber must answer this chunk with `StatusResponse(0)` too.
///
/// Empty `entries` produces no chunks at all (unlike `read_chunks`, which
/// always emits at least one): the attribute side has already sent the
/// report, and an event-less subscription must not add a stray empty one.
pub(super) fn chunk_events(
    entries: &[im::EventEntryOut],
    budget: usize,
    subscription_id: u32,
) -> Vec<Vec<u8>> {
    let mut batches: Vec<Vec<im::EventEntryOut>> = Vec::new();
    let mut current: Vec<im::EventEntryOut> = Vec::new();
    for e in entries {
        let mut candidate = current.clone();
        candidate.push(e.clone());
        if im::encode_report_data_full(&[], &candidate, false, Some(subscription_id), true).len()
            > budget
            && !current.is_empty()
        {
            batches.push(std::mem::take(&mut current));
            current.push(e.clone());
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        batches.push(current);
    }
    let last = batches.len().saturating_sub(1);
    batches
        .into_iter()
        .enumerate()
        .map(|(i, b)| im::encode_report_data_full(&[], &b, false, Some(subscription_id), i != last))
        .collect()
}

/// How many of `all` (oldest first) still fit in one unchunked dirty
/// report alongside `entries`, under `budget` — the prefix
/// `send_subscription_report` actually carries. Measured in the shape the
/// report is really sent in (`more_chunks=false`,
/// `suppress_response=false`), so a prefix that fits here fits on the wire.
///
/// `0` means not even the first event fits (attributes alone are already
/// at or over the budget): the caller sends what it has anyway and logs,
/// which is exactly what it did before events existed — the attribute side
/// of a dirty report has never been chunked.
///
/// Encoding is monotonic in the prefix length (each event only adds
/// bytes), so the first prefix over budget ends the search.
pub(super) fn fit_events(
    entries: &[im::ReportEntryOut],
    all: &[im::EventEntryOut],
    budget: usize,
    subscription_id: u32,
) -> usize {
    let mut fitted = 0;
    for n in 1..=all.len() {
        let len =
            im::encode_report_data_full(entries, &all[..n], false, Some(subscription_id), false)
                .len();
        if len > budget {
            break;
        }
        fitted = n;
    }
    fitted
}

/// Sends one subscription ReportData on a **new**, device-initiated
/// exchange (spec §8.10.3) and waits for the subscriber's
/// `StatusResponse(0)`. Carries the dirty attributes' current values plus
/// the events logged since the last acknowledged report, or no reports at
/// all when nothing changed — an empty ReportData is the keep-alive that
/// tells the subscriber the subscription is still alive
/// (`SecureSession::next_subscription_report` delivers it as such).
///
/// **Events are capped, not chunked** (`fit_events`): a dirty report is one
/// message, and the log can hand out up to `EventLog::DEFAULT_CAP` entries
/// at once (three multi-presses are 27 events — past the 1280B datagram
/// ceiling, which would fail the send and drop the subscription). So only
/// the longest prefix that fits `REPORT_CHUNK_BUDGET` goes out, oldest
/// first; `sub.next_event` then advances to *what was actually sent* + 1,
/// and `pending_urgent` is left as `note_events` set it whenever something
/// was left out (so an urgent remainder follows at the next min-interval
/// instead of waiting for the max-interval keep-alive) and cleared once a
/// report drained everything. Nothing is lost — the log holds it until it
/// is reported (or until the FIFO overruns, which is the pre-existing cap).
///
/// Returns `false` if the report couldn't be delivered or the subscriber
/// answered anything other than SUCCESS; the caller then drops the
/// subscription, which is the only sane response — MRP already retried the
/// send, so a failure here means the peer is gone or has forgotten this
/// subscription, and a real controller re-subscribes on its own.
///
/// Takes `&mut Node` even though it only reads: this future is held across
/// `await` points inside `serve_forever`'s `select!`, and `Device::run`'s task is
/// `tokio::spawn`ed — a shared `&Node` living across an await would make
/// the whole runtime future require `Node: Sync`, which `Box<dyn
/// ClusterHandler>` (declared `: Send`, not `: Sync`) is not.
pub(super) async fn send_subscription_report(
    session: &mut SecureSession,
    fabric_index: u8,
    node: &mut Node,
    sub: &mut ActiveSubscription,
) -> bool {
    let paths: Vec<mat_controller::im::AttrPathIn> = sub
        .dirty
        .iter()
        .map(
            |(endpoint, cluster, attribute)| mat_controller::im::AttrPathIn {
                endpoint: Some(*endpoint),
                cluster: Some(*cluster),
                attribute: Some(*attribute),
            },
        )
        .collect();
    // Same `IsFabricFiltered` the subscribe request asked for: every report
    // on a subscription is a continuation of that one read request, so a
    // dirty/keep-alive report must not widen what the priming report showed.
    let read_ctx = ReadCtx {
        fabric_index,
        fabric_filtered: sub.fabric_filtered,
        subject: session_subject(session),
    };
    // Values are read *now*, not captured when the change happened: the
    // report carries the attribute's current value (spec §8.10.2), so two
    // changes between reports collapse into one entry with the latest
    // value — which is also why `dirty` holds paths, not values.
    // `retain_reportable` drops the status entries a wildcard subscription
    // would otherwise get for attributes it may not read (see its doc).
    let entries = if paths.is_empty() {
        Vec::new()
    } else {
        crate::net::subscription::retain_reportable(sub, node.read_entries(&paths, &read_ctx))
    };
    // Everything logged since the last acknowledged report, filtered by the
    // subscription's own EventRequests and the session's ACL
    // (`Node::event_entries`). Empty `event_paths` (an attribute-only
    // subscription) yields nothing, so this report is byte-identical to
    // what it was before events existed.
    let all_events = node.event_entries(&sub.event_paths, sub.next_event, &read_ctx);
    // One message, `more_chunks=false`: the attribute side of a dirty
    // report is a handful of scalars, orders of magnitude below
    // `REPORT_CHUNK_BUDGET` (unlike priming, which can pull in whole
    // certificate attributes), but the event side is not bounded that way —
    // so it is capped to the prefix that fits (see this fn's doc).
    let fitted = fit_events(&entries, &all_events, REPORT_CHUNK_BUDGET, sub.id);
    let events = &all_events[..fitted];
    let left_out = all_events.len() - fitted;
    if left_out > 0 {
        tracing::debug!(
            subscription_id = sub.id,
            sent = fitted,
            left_out,
            budget = REPORT_CHUNK_BUDGET,
            "subscription report carries only the events that fit — the rest follow at the next min-interval"
        );
    }
    let payload = im::encode_report_data_full(&entries, events, false, Some(sub.id), false);
    if payload.len() > REPORT_CHUNK_BUDGET {
        // Not a hard failure (MRP/`seal` will just fail to send it, and the
        // subscription gets dropped below) — but a silent oversized report
        // is exactly the failure mode a future non-scalar subscribed
        // attribute would hit, so say so loudly enough to find in a log.
        // With `fit_events` in place the events can no longer be the cause
        // on their own: past this point the attributes alone are over.
        tracing::debug!(
            subscription_id = sub.id,
            payload_len = payload.len(),
            budget = REPORT_CHUNK_BUDGET,
            reports = entries.len(),
            events = events.len(),
            left_out,
            "subscription report exceeds the chunk budget — dirty reports are not chunked (see send_subscription_report)"
        );
    }
    let exchange_id = SecureSession::new_exchange_id();
    let send_result = session
        .send_reliable(
            exchange_id,
            PROTOCOL_ID_INTERACTION_MODEL,
            im::OPCODE_REPORT_DATA,
            &payload,
            &reply_cfg(),
        )
        .await;
    tracing::debug!(
        exchange_id,
        subscription_id = sub.id,
        reports = entries.len(),
        events = events.len(),
        keep_alive = entries.is_empty() && events.is_empty(),
        payload_len = payload.len(),
        ok = send_result.is_ok(),
        error = send_result.as_ref().err().map(|e| e.to_string()),
        "subscription ReportData sent"
    );
    let Ok(piggybacked) = send_result else {
        return false;
    };
    // Our own exchange this time (we're the initiator), so the plain
    // `recv` filter is the right one — unlike priming, which answers on
    // the *peer's* exchange (see `await_peer_status_ok`).
    let status_msg = match piggybacked {
        Some(m) => m,
        None => match session.recv(exchange_id, REPORT_STATUS_TIMEOUT).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(exchange_id, error = %e, "subscription report: no StatusResponse");
                return false;
            }
        },
    };
    if !is_status_response_ok(&status_msg) {
        return false;
    }

    sub.last_report_at = Instant::now();
    sub.dirty.clear();
    // Only now — the report was acknowledged, so these events are the
    // subscriber's. A failed/unacknowledged report leaves `next_event`
    // where it was, but that is moot: the caller drops the subscription.
    //
    // Advance past *what was sent*, not to the node's current high-water
    // mark: with the per-report cap the two differ, and jumping to the
    // latter would silently skip everything left out. When nothing was
    // selectable at all (`all_events` empty — an attribute-only
    // subscription, or a log whose entries this session's ACL hides), read
    // the node back instead: the subscriber has seen everything up to
    // *now*, not just what it was allowed to receive. `EventEntryOut::
    // Status` entries carry no number, so they never advance it.
    sub.next_event = match events.iter().rev().find_map(|e| match e {
        im::EventEntryOut::Data(d) => Some(d.event_number),
        im::EventEntryOut::Status { .. } => None,
    }) {
        Some(number) => number + 1,
        None if all_events.is_empty() => node.next_event_number(),
        None => sub.next_event,
    };
    // Events were left behind: stay in the urgent regime so the remainder
    // goes out at the min-interval rather than waiting for the keep-alive.
    sub.pending_urgent = left_out > 0 && sub.pending_urgent;
    true
}
