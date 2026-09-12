use super::tests::test_config;
use super::*;

use mat_controller::crypto::{open_message, seal_message};
use mat_controller::message::Destination;
use mat_controller::session::SessionKeys;
use mat_controller::tlv::{Tag, Writer};
use mat_controller::transport::UdpTransport;

use crate::core::datamodel::{ClusterHandler, InvokeReply};
use crate::core::fabric_store::FabricStore;

const LOCAL_SID: u16 = 0xAAAA; // device's own session id
const PEER_SID: u16 = 0xBBBB; // controller's session id
const CTRL_NODE: u64 = 1;
const DEV_NODE: u64 = 2;
const I2R: [u8; 16] = [0x11; 16];
const R2I: [u8; 16] = [0x22; 16];

/// 600-byte `bytes` attribute so two of them exceed `REPORT_CHUNK_BUDGET`;
/// cluster ids far outside any real range.
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
    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }
}

/// Full-wildcard ReadRequest (every field of the one AttributePathIB
/// omitted) — `mat_controller::im` has no public encoder for this shape
/// (its `encode_read_request*` helpers all pin at least endpoint+cluster),
/// so built by hand the same way `datamodel.rs`'s test-only
/// `encode_read_request_paths` does.
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

struct DevicePair {
    ctrl: Arc<Transport>,
    dev_transport: Arc<Transport>,
    dev_addr: SocketAddr,
    /// Device-role session already pointed at the controller's address.
    session: SecureSession,
}

/// Two loopback UDP sockets plus the device-role `SecureSession` every
/// socket test starts from (`SessionKeys` are the fixed `I2R`/`R2I`).
async fn device_pair() -> DevicePair {
    let bind = || async {
        Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )))
    };
    let ctrl = bind().await;
    let ctrl_addr = ctrl.local_addr().unwrap();
    let dev_transport = bind().await;
    let dev_addr = dev_transport.local_addr().unwrap();
    let session = SecureSession::new_device_role(
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
    DevicePair {
        ctrl,
        dev_transport,
        dev_addr,
        session,
    }
}

/// Controller role: mirror image of the device's ids (see
/// `SecureSession::new_device_role`'s doc for the key swap).
fn controller_session(pair: &DevicePair) -> SecureSession {
    SecureSession::new(
        Arc::clone(&pair.ctrl),
        pair.dev_addr,
        PEER_SID,
        LOCAL_SID,
        SessionKeys {
            i2r: I2R,
            r2i: R2I,
            attestation_challenge: [0; 16],
        },
        CTRL_NODE,
        DEV_NODE,
    )
}

fn root_node_with_fat_clusters(n: u32) -> Node {
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    for i in 0..n {
        node.add_cluster(
            0,
            Box::new(FatHandler {
                cluster: 0x9999_0000 + i,
            }),
        );
    }
    node
}

fn empty_comm_server() -> CommissioningServer {
    let dev = mat_controller::x509::generate_dev_attestation(0xFFF1, 0x8000).unwrap();
    CommissioningServer::new(dev, FabricStore::new())
}

/// One controller-sealed datagram (`session_id = LOCAL_SID`, initiator
/// side), bumping `counter` — what the two hand-rolled `send` closures did.
fn seal_from_controller(
    counter: &mut u32,
    opcode: u8,
    protocol_id: u16,
    exchange_id: u16,
    needs_ack: bool,
    acked: Option<u32>,
    payload: &[u8],
) -> Vec<u8> {
    let header = MessageHeader {
        session_id: LOCAL_SID,
        security_flags: 0,
        message_counter: *counter,
        source_node_id: None,
        destination: Destination::None,
    };
    let proto = ProtocolHeader {
        initiator: true,
        needs_ack,
        acked_counter: acked,
        opcode,
        exchange_id,
        protocol_id,
        vendor_id: None,
    };
    *counter += 1;
    seal_message(&I2R, &header, &proto, payload, CTRL_NODE).unwrap()
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
/// hand the same way `on_secured_datagram` calls it.
#[tokio::test]
async fn serve_secured_drains_and_serves_a_cross_exchange_piggybacked_request() {
    const REQ_EXCHANGE: u16 = 0x10;
    const NEW_EXCHANGE: u16 = 0x20;

    let mut pair = device_pair().await;
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    let comm_server = empty_comm_server();

    // Controller side, run concurrently with `serve_secured` below: send
    // the first ReadRequest, wait for its reply, then — instead of a
    // standalone ack — send a second ReadRequest on a *different*
    // exchange that piggybacks the first reply's ack, and finally wait
    // for its own reply too.
    let ctrl_task = tokio::spawn(async move {
        let mut counter = 10u32;
        let req1 = im::encode_read_request(
            0,
            mat_controller::im::CLUSTER_BASIC_INFORMATION,
            mat_controller::im::ATTR_DATA_MODEL_REVISION,
        );
        let dg1 = seal_from_controller(
            &mut counter,
            im::OPCODE_READ_REQUEST,
            PROTOCOL_ID_INTERACTION_MODEL,
            REQ_EXCHANGE,
            false,
            None,
            &req1,
        );
        pair.ctrl.send_to(&dg1, pair.dev_addr).await.unwrap();

        let mut buf = [0u8; MAX_DATAGRAM];
        let (n1, from) = pair.ctrl.recv_from(&mut buf).await.unwrap();
        let (h1, p1, _) = open_message(&R2I, &buf[..n1], DEV_NODE).unwrap();
        assert_eq!(p1.exchange_id, REQ_EXCHANGE);
        assert_eq!(p1.opcode, im::OPCODE_REPORT_DATA);
        assert!(p1.needs_ack);

        let req2 = im::encode_read_request(
            0,
            mat_controller::im::CLUSTER_BASIC_INFORMATION,
            mat_controller::im::ATTR_VENDOR_ID,
        );
        let dg2 = seal_from_controller(
            &mut counter,
            im::OPCODE_READ_REQUEST,
            PROTOCOL_ID_INTERACTION_MODEL,
            NEW_EXCHANGE,
            false,
            Some(h1.message_counter), // piggyback: acks dg1's reply
            &req2,
        );
        pair.ctrl.send_to(&dg2, from).await.unwrap();

        // Proof the drain loop actually served dg2 (not just buffered
        // it): a real ReportData for VendorID must arrive on
        // NEW_EXCHANGE.
        let (n2, from2) = pair.ctrl.recv_from(&mut buf).await.unwrap();
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
        let ack_dg = seal_from_controller(
            &mut counter,
            OPCODE_MRP_STANDALONE_ACK,
            PROTOCOL_ID_SECURE_CHANNEL,
            NEW_EXCHANGE,
            false,
            Some(h2.message_counter),
            &[],
        );
        pair.ctrl.send_to(&ack_dg, from2).await.unwrap();
    });

    // Device side: read dg1 off the (real) socket exactly like `run`'s
    // loop does, then hand it to `serve_secured` — whose internal
    // `reply_reliable` ack-wait is what actually reads dg2 off the
    // socket and resolves it via the cross-exchange piggyback ack,
    // which is what makes the drain loop kick in afterward.
    let mut buf = [0u8; MAX_DATAGRAM];
    let (n, from) = pair.dev_transport.recv_from(&mut buf).await.unwrap();
    serve_secured(
        &buf[..n],
        from,
        &mut pair.session,
        0,
        &mut ServeState {
            node: &mut node,
            comm_server: &comm_server,
            mdns: None,
            subscription: &mut None,
            window: &mut CommissioningWindow::Closed,
            config: &test_config(),
        },
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
/// loop already exercises from the other end
/// (`mat_controller::session::subscribe::subscribe_wildcard`).
#[tokio::test]
async fn read_request_chunked_flow_round_trips_two_or_more_chunks() {
    const REQ_EXCHANGE: u16 = 0x30;

    let mut pair = device_pair().await;
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
    let comm_server = empty_comm_server();

    let ctrl_task = tokio::spawn(async move {
        let mut counter = 10u32;
        let req = encode_full_wildcard_read_request();
        let dg = seal_from_controller(
            &mut counter,
            im::OPCODE_READ_REQUEST,
            PROTOCOL_ID_INTERACTION_MODEL,
            REQ_EXCHANGE,
            false,
            None,
            &req,
        );
        pair.ctrl.send_to(&dg, pair.dev_addr).await.unwrap();

        let mut chunk_count = 0usize;
        let mut buf = [0u8; MAX_DATAGRAM];
        loop {
            let (n, from) = pair.ctrl.recv_from(&mut buf).await.unwrap();
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
                let dg = seal_from_controller(
                    &mut counter,
                    im::OPCODE_STATUS_RESPONSE,
                    PROTOCOL_ID_INTERACTION_MODEL,
                    REQ_EXCHANGE,
                    false,
                    Some(h.message_counter),
                    &ok,
                );
                pair.ctrl.send_to(&dg, peer).await.unwrap();
            } else {
                assert!(rd.suppress_response);
                // Final chunk: no StatusResponse expected from us, but
                // still ack it (standalone) so the runtime's own
                // `reply_reliable` for this last send completes
                // promptly instead of exhausting its retry budget.
                let ack_dg = seal_from_controller(
                    &mut counter,
                    OPCODE_MRP_STANDALONE_ACK,
                    PROTOCOL_ID_SECURE_CHANNEL,
                    REQ_EXCHANGE,
                    false,
                    Some(h.message_counter),
                    &[],
                );
                pair.ctrl.send_to(&ack_dg, peer).await.unwrap();
                break;
            }
        }
        chunk_count
    });

    let mut buf = [0u8; MAX_DATAGRAM];
    let (n, from) = pair.dev_transport.recv_from(&mut buf).await.unwrap();
    serve_secured(
        &buf[..n],
        from,
        &mut pair.session,
        0,
        &mut ServeState {
            node: &mut node,
            comm_server: &comm_server,
            mdns: None,
            subscription: &mut None,
            window: &mut CommissioningWindow::Closed,
            config: &test_config(),
        },
    )
    .await;

    let chunk_count = ctrl_task.await.unwrap();
    assert!(chunk_count >= 2, "expected 2+ chunks, got {chunk_count}");
}
// ── Task 12: chunked subscription priming ───────────────────────────

/// Task 6's homework, settled here: priming a subscription against a
/// `Node` fat enough to blow past `REPORT_CHUNK_BUDGET` must come back
/// as several `ReportData` chunks and still complete with a
/// `SubscribeResponse`.
///
/// Unlike `read_request_chunked_flow_round_trips_two_or_more_chunks`
/// above (which hand-rolls the controller at the datagram level,
/// because `mat` has no chunk-aware read), the controller here is the
/// real `SecureSession::subscribe_wildcard` — the same code path a
/// commissioned `mat`/`matd` uses against real devices. It is
/// therefore the authority on the wire contract: it acknowledges each
/// priming chunk with `StatusResponse(0)` on the subscribe exchange and
/// insists the `SubscribeResponse` follow on that same exchange, so a
/// device that suppressed the final chunk's response, forgot the
/// SubscriptionId, or answered on a fresh exchange would fail here.
///
/// Only the device's *session* is built by hand (no PASE/CASE — that's
/// `subscribe_loop.rs`'s job); everything above the session is real.
#[tokio::test]
async fn subscription_priming_round_trips_multiple_chunks() {
    let mut pair = device_pair().await;
    let mut ctrl = controller_session(&pair);

    let mut node = root_node_with_fat_clusters(3);
    let comm_server = empty_comm_server();

    let ctrl_task = tokio::spawn(async move {
        let cfg = crate::net::fast_cfg();
        // Full wildcard (`clusters` empty) so every fat attribute is
        // primed.
        ctrl.subscribe_wildcard(0, 30, false, &[], &cfg).await
    });

    // One datagram in: `serve_secured` drives the entire subscribe
    // interaction (every chunk plus the SubscribeResponse) from here,
    // reading the controller's StatusResponses off the socket itself.
    let mut subscription: Option<ActiveSubscription> = None;
    let mut buf = [0u8; MAX_DATAGRAM];
    let (n, from) = pair.dev_transport.recv_from(&mut buf).await.unwrap();
    serve_secured(
        &buf[..n],
        from,
        &mut pair.session,
        0,
        &mut ServeState {
            node: &mut node,
            comm_server: &comm_server,
            mdns: None,
            subscription: &mut subscription,
            window: &mut CommissioningWindow::Closed,
            config: &test_config(),
        },
    )
    .await;

    let (sr, priming) = ctrl_task.await.unwrap().expect("subscribe should complete");
    assert!(
        priming.len() >= 2,
        "expected priming to arrive in 2+ chunks, got {}",
        priming.len()
    );
    for (i, chunk) in priming.iter().enumerate() {
        assert_eq!(
            chunk.subscription_id,
            Some(sr.subscription_id),
            "priming chunk {i} must carry the SubscriptionId"
        );
        assert!(
            !chunk.suppress_response,
            "priming chunk {i} must not suppress the response"
        );
    }
    let sub = subscription.expect("a completed subscribe must register the subscription");
    assert_eq!(sub.id, sr.subscription_id);
    assert_eq!(sub.max_interval, Duration::from_secs(30));
}
/// Fix round 1 (code review): a request that lands on *another*
/// exchange while the device is waiting for a chunk's
/// `StatusResponse(0)` must survive that wait and still be served.
///
/// `screen_with` MRP-acks every authenticated request the moment it
/// decodes it, delivery filter or not — so a request pulled out of the
/// buffer by the status wait and then thrown away is gone for good: the
/// peer has its ack and will never retransmit ("ack-then-drop of
/// cross-exchange secured requests"). `await_peer_status_ok` therefore
/// sets such messages aside and hands them back
/// (`SecureSession::requeue_buffered_request`) for `serve_secured`'s
/// drain.
///
/// Drives the exact interleaving by hand: the controller answers the
/// first chunk with a *standalone* ack (forcing the fallback wait
/// instead of the piggybacked fast path), then squeezes a ReadRequest
/// on a second exchange in before the chunk's StatusResponse. The read
/// must still complete, and the second exchange must still get its
/// ReportData.
///
/// This also exercises the wait loop's termination: every *subsequent*
/// chunk's status wait pulls that same requeued request out of the
/// buffer again, sets it aside again, and has to fall through to the
/// socket for the real StatusResponse. A loop that re-consumed its own
/// set-aside messages would spin here, and one that gave up on them
/// would stall the read — both show up as the controller's 20s
/// "device went silent" timeout rather than a passing test.
#[tokio::test]
async fn a_request_interleaved_into_a_chunk_status_wait_is_not_lost() {
    const EX_READ: u16 = 0x40;
    const EX_OTHER: u16 = 0x41;

    let mut pair = device_pair().await;
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
    let comm_server = empty_comm_server();

    let ctrl_task = tokio::spawn(async move {
        let mut counter = 10u32;

        let dg = seal_from_controller(
            &mut counter,
            im::OPCODE_READ_REQUEST,
            PROTOCOL_ID_INTERACTION_MODEL,
            EX_READ,
            false,
            None,
            &encode_full_wildcard_read_request(),
        );
        pair.ctrl.send_to(&dg, pair.dev_addr).await.unwrap();

        let mut buf = [0u8; MAX_DATAGRAM];
        let mut chunks = 0usize;
        let mut interleaved_sent = false;
        let mut interleaved_answered = false;
        let mut read_done = false;

        // One loop for both exchanges: the device's answer to the
        // interleaved read can only arrive after the chunked read
        // finishes (the drain runs last), but the loop doesn't assume
        // that ordering.
        while !(read_done && interleaved_answered) {
            let (n, from) =
                tokio::time::timeout(Duration::from_secs(20), pair.ctrl.recv_from(&mut buf))
                    .await
                    .expect("device went silent")
                    .unwrap();
            let (h, p, payload) = open_message(&R2I, &buf[..n], DEV_NODE).unwrap();
            if p.protocol_id == PROTOCOL_ID_SECURE_CHANNEL {
                continue; // the device's own standalone acks
            }
            assert_eq!(p.opcode, im::OPCODE_REPORT_DATA);

            if p.exchange_id == EX_OTHER {
                let rd = im::decode_report_data_message(&payload).unwrap();
                assert_eq!(
                    rd.reports[0].attribute,
                    Some(mat_controller::im::ATTR_VENDOR_ID),
                    "the interleaved read must be answered, not dropped"
                );
                interleaved_answered = true;
                let ack = seal_from_controller(
                    &mut counter,
                    OPCODE_MRP_STANDALONE_ACK,
                    PROTOCOL_ID_SECURE_CHANNEL,
                    EX_OTHER,
                    false,
                    Some(h.message_counter),
                    &[],
                );
                pair.ctrl.send_to(&ack, from).await.unwrap();
                continue;
            }

            assert_eq!(p.exchange_id, EX_READ);
            let rd = im::decode_report_data_message(&payload).unwrap();
            chunks += 1;

            // Always a *standalone* ack first — never a piggybacked
            // StatusResponse — so the device has to take
            // `await_peer_status_ok`'s fallback wait.
            let ack = seal_from_controller(
                &mut counter,
                OPCODE_MRP_STANDALONE_ACK,
                PROTOCOL_ID_SECURE_CHANNEL,
                EX_READ,
                false,
                Some(h.message_counter),
                &[],
            );
            pair.ctrl.send_to(&ack, from).await.unwrap();

            if !rd.more_chunks {
                read_done = true;
                continue;
            }

            // ...and, exactly once, a request on another exchange
            // squeezed in while the device is inside that wait.
            if !interleaved_sent {
                interleaved_sent = true;
                let other = seal_from_controller(
                    &mut counter,
                    im::OPCODE_READ_REQUEST,
                    PROTOCOL_ID_INTERACTION_MODEL,
                    EX_OTHER,
                    true, // needs_ack: this is the message screen_with acks then buffers
                    None,
                    &im::encode_read_request(
                        0,
                        mat_controller::im::CLUSTER_BASIC_INFORMATION,
                        mat_controller::im::ATTR_VENDOR_ID,
                    ),
                );
                pair.ctrl.send_to(&other, from).await.unwrap();
            }

            let ok = seal_from_controller(
                &mut counter,
                im::OPCODE_STATUS_RESPONSE,
                PROTOCOL_ID_INTERACTION_MODEL,
                EX_READ,
                false,
                None,
                &im::encode_status_response(0),
            );
            pair.ctrl.send_to(&ok, from).await.unwrap();
        }
        (chunks, interleaved_answered)
    });

    let mut buf = [0u8; MAX_DATAGRAM];
    let (n, from) = pair.dev_transport.recv_from(&mut buf).await.unwrap();
    serve_secured(
        &buf[..n],
        from,
        &mut pair.session,
        0,
        &mut ServeState {
            node: &mut node,
            comm_server: &comm_server,
            mdns: None,
            subscription: &mut None,
            window: &mut CommissioningWindow::Closed,
            config: &test_config(),
        },
    )
    .await;

    let (chunks, interleaved_answered) = tokio::time::timeout(Duration::from_secs(30), ctrl_task)
        .await
        .expect("controller task hung — an interleaved request was probably dropped")
        .unwrap();
    assert!(
        chunks >= 2,
        "expected a chunked read, got {chunks} chunk(s)"
    );
    assert!(interleaved_answered);
}

/// Timed Interaction（spec §8.9.4）の後続リクエスト: initiator は
/// TimedRequest → StatusResponse(SUCCESS) 受領後、**同一 exchange** で
/// timed Invoke を送る（StatusResponse への ack はそこに piggyback）。
/// `reply_reliable` はこの後続リクエストを `Ok(Some(msg))` として返すが、
/// 旧 `serve_secured_message` は戻り値を捨てていたため invoke は MRP ack
/// だけされて永遠に応答されず、Google Play Services スタック（Android HA
/// アプリ経由の commissioning）が 45 秒タイムアウトで中断していた
/// （2026-08-18 実測）。
#[tokio::test]
async fn a_timed_invoke_on_the_same_exchange_is_served_not_dropped() {
    const EX: u16 = 0x50;

    let mut pair = device_pair().await;
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    let comm_server = empty_comm_server();

    let ctrl_task = tokio::spawn(async move {
        let mut counter = 10u32;

        // TimedRequest: struct{0: timeout-ms, 255: revision}
        let timed_payload = {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(0), 300);
            w.put_uint(Tag::Context(255), u64::from(im::IM_REVISION));
            w.end_container();
            w.finish()
        };
        let dg = seal_from_controller(
            &mut counter,
            im::OPCODE_TIMED_REQUEST,
            PROTOCOL_ID_INTERACTION_MODEL,
            EX,
            true,
            None,
            &timed_payload,
        );
        pair.ctrl.send_to(&dg, pair.dev_addr).await.unwrap();

        let mut buf = [0u8; MAX_DATAGRAM];
        let mut invoke_sent = false;
        loop {
            let (n, from) =
                tokio::time::timeout(Duration::from_secs(20), pair.ctrl.recv_from(&mut buf))
                    .await
                    .expect("device went silent — the timed invoke was probably dropped")
                    .unwrap();
            let (h, p, payload) = open_message(&R2I, &buf[..n], DEV_NODE).unwrap();
            if p.protocol_id == PROTOCOL_ID_SECURE_CHANNEL {
                continue; // the device's own standalone acks
            }
            if p.opcode == im::OPCODE_STATUS_RESPONSE {
                assert_eq!(
                    im::decode_status_response(&payload).unwrap(),
                    im::STATUS_SUCCESS
                );
                assert!(!invoke_sent, "one StatusResponse expected");
                invoke_sent = true;
                // 後続の timed invoke: 同一 exchange、StatusResponse への
                // ack を piggyback（standalone ack は送らない — 実機の
                // chip スタックの挙動に合わせる）。
                let invoke = seal_from_controller(
                    &mut counter,
                    im::OPCODE_INVOKE_REQUEST,
                    PROTOCOL_ID_INTERACTION_MODEL,
                    EX,
                    true,
                    Some(h.message_counter),
                    &im::encode_invoke_request(0, im::CLUSTER_BASIC_INFORMATION, 0x7F, None),
                );
                pair.ctrl.send_to(&invoke, from).await.unwrap();
                continue;
            }
            assert_eq!(
                p.opcode,
                im::OPCODE_INVOKE_RESPONSE,
                "the timed invoke must be answered with an InvokeResponse"
            );
            let out = im::decode_invoke_response(&payload).unwrap();
            assert_eq!(out.status, im::STATUS_UNSUPPORTED_COMMAND);
            let ack = seal_from_controller(
                &mut counter,
                OPCODE_MRP_STANDALONE_ACK,
                PROTOCOL_ID_SECURE_CHANNEL,
                EX,
                false,
                Some(h.message_counter),
                &[],
            );
            pair.ctrl.send_to(&ack, from).await.unwrap();
            return;
        }
    });

    let mut buf = [0u8; MAX_DATAGRAM];
    let (n, from) = pair.dev_transport.recv_from(&mut buf).await.unwrap();
    serve_secured(
        &buf[..n],
        from,
        &mut pair.session,
        0,
        &mut ServeState {
            node: &mut node,
            comm_server: &comm_server,
            mdns: None,
            subscription: &mut None,
            window: &mut CommissioningWindow::Closed,
            config: &test_config(),
        },
    )
    .await;

    tokio::time::timeout(Duration::from_secs(30), ctrl_task)
        .await
        .expect("controller task hung — the same-exchange timed invoke was dropped")
        .unwrap();
}
