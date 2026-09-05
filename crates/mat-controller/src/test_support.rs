//! Test-only CASE / PASE responders for the initiator-side handshake tests.
//! **Test-only**: gated behind the `test-responder` feature, never compiled
//! into a production build — consumed via the `dev-dependencies`
//! self-reference from this crate's own integration tests and by the sibling
//! crates' test builds.
//!
//! `responder_task` (CASE) drives `case_responder::CaseResponderCore` — the
//! same pure state machine `mat-device`'s production responder wraps — so
//! protocol/crypto correctness lives there (proven by its own unit tests and
//! by `mat-device`'s `tests/case_establish.rs`). What stays hand-rolled here
//! is only the raw-socket loop: unsecured framing, opcode filtering, MRP
//! piggyback acks, and one secured IM read served after establishment. The
//! value of the test is that the production *initiator* (`case::establish`)
//! is exercised against that responder over a real socket
//! (`tests/case_self_handshake.rs`).
//!
//! Its identity is fixture `node01_01` (NOC + private key) chaining through
//! `ica01` to `root01`; the initiator is a fresh self-issued NOC that trusts
//! the same `root01`. IPK and fabric id are shared across both sides (a CASE
//! requirement) by parsing them from `node01_01`.
//!
//! `pase_responder_task` (PASE) is the narrower counterpart — see its doc
//! comment for the residual-risk note on reusing `spake2p`'s verifier.

use std::net::SocketAddr;
use std::time::Duration;

use sha2::Sha256;

use crate::case_responder::{
    CaseFabric, CaseOutput, CaseResponderCore, OPCODE_SIGMA1, OPCODE_SIGMA3,
};
use crate::cert::MatterCert;
use crate::crypto::{open_message, seal_message};
use crate::exchange::MrpConfig;
use crate::im;
use crate::message::{Destination, MessageHeader, ProtocolHeader, OPCODE_STATUS_REPORT};
use crate::tlv::{Tag, Writer};
use crate::transport::{UdpTransport, MAX_DATAGRAM};

const PROTO_SECURE_CHANNEL: u16 = 0x0000;
/// spec §4.13.2.3 — mirror of the crate-private one in `pase.rs`.
const INFO_SESSION_KEYS: &[u8] = b"SessionKeys";

// Shared fabric material. IPK must be identical on both sides.
pub const IPK: [u8; 16] = [0xCC; 16];
// Initiator's chosen operational node id (self-issued NOC subject).
pub const INITIATOR_NODE_ID: u64 = 0x1B669;

// Fixtures: responder identity chain + root private material for the
// initiator's self-issued NOC.
pub const NODE01_NOC: &[u8] = include_bytes!("../tests/fixtures/node01_01_chip.bin");
pub const NODE01_PRIV: &[u8] = include_bytes!("../tests/fixtures/node01_01_privkey.bin");
pub const ICA01: &[u8] = include_bytes!("../tests/fixtures/ica01_chip.bin");
pub const ROOT01_CHIP: &[u8] = include_bytes!("../tests/fixtures/root01_chip.bin");
pub const ROOT01_PRIV: &[u8] = include_bytes!("../tests/fixtures/root01_privkey.bin");

pub fn fast_cfg() -> MrpConfig {
    MrpConfig {
        initial_interval: Duration::from_millis(50),
        active_interval: Duration::from_millis(50),
        max_retries: 3,
        backoff: 1.0,
        jitter: 0.0,
    }
}

// --- crypto helper (still needed: PASE session keys below;
// `case_responder::CaseResponderCore` handles CASE's own HKDF derivations
// internally now) ---

fn hkdf48(shared: &[u8], salt: &[u8], info: &[u8]) -> [u8; 48] {
    let hk = hkdf::Hkdf::<Sha256>::new(Some(salt), shared);
    let mut out = [0u8; 48];
    hk.expand(info, &mut out).expect("valid length");
    out
}

// --- unsecured framing helpers for the responder ---

fn build_unsecured(
    counter: u32,
    opcode: u8,
    exchange_id: u16,
    acked_counter: Option<u32>,
    needs_ack: bool,
    payload: &[u8],
) -> Vec<u8> {
    let header = MessageHeader {
        session_id: 0,
        security_flags: 0,
        message_counter: counter,
        source_node_id: None,
        destination: Destination::None,
    };
    let proto = ProtocolHeader {
        initiator: false,
        needs_ack,
        acked_counter,
        opcode,
        exchange_id,
        protocol_id: PROTO_SECURE_CHANNEL,
        vendor_id: None,
    };
    let mut buf = header.encoded();
    proto.encode(&mut buf);
    buf.extend_from_slice(payload);
    buf
}

async fn recv_dg(t: &UdpTransport) -> (Vec<u8>, SocketAddr) {
    let mut buf = [0u8; MAX_DATAGRAM];
    let (n, from) = tokio::time::timeout(Duration::from_secs(5), t.recv_from(&mut buf))
        .await
        .expect("responder timed out waiting for a datagram")
        .expect("responder recv_from io error");
    (buf[..n].to_vec(), from)
}

/// Decodes an *unsecured* datagram (session id 0) into its protocol header
/// and app payload, or `None` if it isn't a well-formed unsecured message.
fn decode_unsecured(buf: &[u8]) -> Option<(ProtocolHeader, Vec<u8>)> {
    let (h, off) = MessageHeader::decode(buf).ok()?;
    if h.session_id != 0 {
        return None;
    }
    let (p, boff) = ProtocolHeader::decode(&buf[off..]).ok()?;
    Some((p, buf[off + boff..].to_vec()))
}

/// Waits for the initiator's next *unsecured* message with `opcode` (skipping
/// MRP retransmits, standalone acks, and anything else on the socket).
/// Returns the protocol header, the app payload, the message counter (for
/// the piggyback ack) and the source address.
async fn recv_unsecured(
    transport: &UdpTransport,
    opcode: u8,
) -> (ProtocolHeader, Vec<u8>, u32, SocketAddr) {
    loop {
        let (buf, from) = recv_dg(transport).await;
        let Some((p, payload)) = decode_unsecured(&buf) else {
            continue;
        };
        if p.opcode != opcode || !p.initiator {
            continue;
        }
        let (h, _) = MessageHeader::decode(&buf).unwrap();
        return (p, payload, h.message_counter, from);
    }
}

/// The established session as seen from the responder, for
/// [`serve_one_read`]. Nonce node ids differ per protocol: CASE opens the
/// initiator's messages with the initiator's node id and seals replies with
/// the responder's; PASE uses 0 for both (spec §4.13, unauthenticated).
struct EstablishedSession<'a> {
    our_session_id: u16,
    peer_session_id: u16,
    i2r: &'a [u8; 16],
    r2i: &'a [u8; 16],
    open_node_id: u64,
    seal_node_id: u64,
    message_counter: u32,
}

/// Serves exactly one secured IM ReadRequest on the established session with
/// ReportData(on-off=false), piggybacking the ack for the request. Datagrams
/// on other sessions (unsecured acks etc.) and anything that fails to open
/// are skipped.
async fn serve_one_read(
    transport: &UdpTransport,
    initiator_addr: SocketAddr,
    s: EstablishedSession<'_>,
) {
    let (read_exchange, read_counter) = loop {
        let (buf, _from) = recv_dg(transport).await;
        let (mh, _) = match MessageHeader::decode(&buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if mh.session_id != s.our_session_id {
            continue;
        }
        let (h, p, _payload) = match open_message(s.i2r, &buf, s.open_node_id) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if p.protocol_id != im::PROTOCOL_ID_IM || p.opcode != im::OPCODE_READ_REQUEST {
            continue;
        }
        break (p.exchange_id, h.message_counter);
    };

    let report = report_data_false_suppressed();
    let header = MessageHeader {
        session_id: s.peer_session_id, // seal toward the initiator's session
        security_flags: 0,
        message_counter: s.message_counter,
        source_node_id: None,
        destination: Destination::None,
    };
    let proto = ProtocolHeader {
        initiator: false, // initiator opened this exchange; we are the responder of it
        needs_ack: true,
        acked_counter: Some(read_counter), // piggyback ack for the ReadRequest
        opcode: im::OPCODE_REPORT_DATA,
        exchange_id: read_exchange,
        protocol_id: im::PROTOCOL_ID_IM,
        vendor_id: None,
    };
    let report_dg =
        seal_message(s.r2i, &header, &proto, &report, s.seal_node_id).expect("seal report data");
    transport
        .send_to(&report_dg, initiator_addr)
        .await
        .expect("send report data");
}

/// ReportData for onoff `OnOff` = false, `SuppressResponse` = true (so the
/// initiator's `read_attribute` won't send a closing StatusResponse).
fn report_data_false_suppressed() -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(1)); // AttributeReportIBs
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(1)); // AttributeData
    w.put_uint(Tag::Context(0), 1); // DataVersion
    w.start_list(Tag::Context(1)); // Path
    w.put_uint(Tag::Context(2), 1); // endpoint
    w.put_uint(Tag::Context(3), u64::from(im::CLUSTER_ON_OFF));
    w.put_uint(Tag::Context(4), u64::from(im::ATTR_ON_OFF));
    w.end_container();
    w.put_bool(Tag::Context(2), false); // Data = false
    w.end_container();
    w.end_container();
    w.end_container();
    w.put_bool(Tag::Context(4), true); // SuppressResponse
    w.put_uint(Tag::Context(255), u64::from(im::IM_REVISION));
    w.end_container();
    w.finish()
}

/// The test-only CASE responder: the mirror of `case::establish` with the
/// initiator/responder roles swapped, driven through `case_responder::
/// CaseResponderCore` (Task 10 — see the module doc comment for the
/// migration rationale). Receives Sigma1, builds and sends Sigma2, verifies
/// Sigma3, sends a success StatusReport, and serves one secured IM
/// ReadRequest with ReportData(on-off=false).
///
/// Returns the initiator's observed source `SocketAddr` (the address this
/// responder actually saw the initiator's datagrams arrive from) — callers
/// can assert it against the initiator's own bound local address as a
/// sanity check on socket identity.
#[allow(clippy::too_many_arguments)]
pub async fn responder_task(
    transport: UdpTransport,
    initiator_node_id: u64,
    responder_node_id: u64,
    noc_tlv: Vec<u8>,
    icac_tlv: Vec<u8>,
    op_priv: [u8; 32],
    root_tlv: Vec<u8>,
) -> SocketAddr {
    let fabric_id = MatterCert::parse(&noc_tlv)
        .expect("parse responder noc")
        .fabric_id()
        .expect("noc fabric id");
    let root_public_key = MatterCert::parse(&root_tlv).expect("parse root").pub_key;
    let fabric = CaseFabric {
        fabric_index: 1,
        root_tlv,
        noc_tlv,
        icac_tlv: Some(icac_tlv),
        op_private_key: op_priv,
        ipk_operational: IPK,
        node_id: responder_node_id,
        fabric_id,
        root_public_key,
    };
    let resp_session_id = crate::case::random_nonzero_u16();
    let mut core = CaseResponderCore::new(vec![fabric], resp_session_id);

    // --- Sigma1 -> Sigma2 ---
    let (p, payload, counter, initiator_addr) = recv_unsecured(&transport, OPCODE_SIGMA1).await;
    let CaseOutput::Reply(sigma2_payload, opcode) = core
        .on_message(OPCODE_SIGMA1, &payload)
        .expect("sigma1 handling failed")
    else {
        panic!("expected Reply after Sigma1");
    };
    let sigma2_dg = build_unsecured(
        100,
        opcode,
        p.exchange_id,
        Some(counter),
        false,
        &sigma2_payload,
    );
    transport
        .send_to(&sigma2_dg, initiator_addr)
        .await
        .expect("send sigma2");

    // --- Sigma3 --- (skip retransmitted Sigma1 / standalone acks)
    let (p, payload, counter, _) = recv_unsecured(&transport, OPCODE_SIGMA3).await;
    let CaseOutput::Established {
        reply,
        opcode,
        keys,
        peer_session_id,
        peer_node_id: _,
        peer_cats: _,
        fabric_index: _,
    } = core
        .on_message(OPCODE_SIGMA3, &payload)
        .expect("sigma3 handling failed (S3K derivation / transcript / chain verification)")
    else {
        panic!("expected Established after Sigma3");
    };
    let status_dg = build_unsecured(101, opcode, p.exchange_id, Some(counter), false, &reply);
    transport
        .send_to(&status_dg, initiator_addr)
        .await
        .expect("send status report");

    // --- Serve one secured IM ReadRequest with ReportData(on-off=false) ---
    // Initiator sealed with i2r; nonce uses the initiator's node id.
    // Responder→initiator messages are sealed with r2i; the nonce uses the
    // responder's node id (which the initiator passed to `establish`).
    serve_one_read(
        &transport,
        initiator_addr,
        EstablishedSession {
            our_session_id: resp_session_id,
            peer_session_id,
            i2r: &keys.i2r,
            r2i: &keys.r2i,
            open_node_id: initiator_node_id,
            seal_node_id: responder_node_id,
            message_counter: 1000,
        },
    )
    .await;
    initiator_addr
}

// ============================================================================
// PASE responder（SPAKE2+ verifier 役）— audit Tier 5
// ============================================================================

/// The test-only PASE responder: the mirror of `pase::establish` with the
/// roles swapped, using the real SPAKE2+ *verifier* role
/// (`spake2p::Spake2pVerifier`: Y = y·P + w0·N, Z = y·(X − w0·M), V = y·L
/// with L = w1·P). Serves PBKDFParamRequest → PBKDFParamResponse →
/// Pake1/2/3 → StatusReport(success), then answers one secured IM
/// ReadRequest with ReportData(on-off=false). PASE sessions are
/// unauthenticated: both nonce node ids are 0 (spec §4.13).
///
/// RESIDUAL RISK: this responder calls `spake2p::Spake2pVerifier` — the same
/// production verifier-role type the initiator's `Spake2pProver` is proven
/// to agree with in `prover_and_verifier_agree` (`spake2p.rs`) — so a defect
/// *inside* their shared math or key schedule would affect both roles
/// identically and stay invisible here (the CASE responder has the same
/// shape via `CaseResponderCore`). What this test DOES catch is PASE
/// wire-protocol bugs: opcode/tag framing, PBKDFParamRequest/Response and
/// Pake1/2/3 message layout, confirmation-direction (cA vs cB) wiring, and
/// the session-key handoff into the secured IM exchange — it is not a
/// substitute for the RFC 9383 test vectors (`rfc9383_p256_vector`) or
/// on-wire interop for math-level bugs.
///
/// Returns the initiator's observed source `SocketAddr` (same contract as
/// `responder_task`).
pub async fn pase_responder_task(transport: UdpTransport, passcode: u32) -> SocketAddr {
    use crate::pase::{
        self, OPCODE_PASE_PAKE1, OPCODE_PASE_PAKE2, OPCODE_PASE_PAKE3, OPCODE_PBKDF_PARAM_REQUEST,
        OPCODE_PBKDF_PARAM_RESPONSE,
    };
    use crate::spake2p;

    const ITERATIONS: u32 = 1000;
    const SALT: &[u8; 16] = b"SPAKE2P Key Salt";

    // --- PBKDFParamRequest ---
    let (p, req_payload, req_counter, initiator_addr) =
        recv_unsecured(&transport, OPCODE_PBKDF_PARAM_REQUEST).await;
    let req_exchange = p.exchange_id;
    let initiator_session_id = pase::decode_pbkdf_param_request(&req_payload)
        .expect("pbkdf request malformed")
        .initiator_session_id;

    // Fixed (not randomized like CASE's `responder_task`): this responder
    // serves exactly one self-contained test run, so a collision-checked
    // random nonzero value would add ceremony without buying anything here.
    let resp_session_id: u16 = 0xB0B1;

    // --- PBKDFParamResponse ---
    let resp_payload = pase::encode_pbkdf_param_response(
        &[0u8; 32], // initiatorRandom echo（initiator は無視）
        &[1u8; 32], // responderRandom（同上）
        resp_session_id,
        ITERATIONS,
        SALT,
    );
    let resp_dg = build_unsecured(
        200,
        OPCODE_PBKDF_PARAM_RESPONSE,
        req_exchange,
        Some(req_counter),
        false,
        &resp_payload,
    );
    transport
        .send_to(&resp_dg, initiator_addr)
        .await
        .expect("send pbkdf param response");

    // --- PAKE context（spec §4.13.1.2、pase.rs と同じ構成）---
    let context = pase::pake_context(&req_payload, &resp_payload);

    // --- Pake1 ---（PBKDFParamRequest の MRP 再送などは recv_unsecured が無視）
    let (p, payload, pake1_counter, _) = recv_unsecured(&transport, OPCODE_PASE_PAKE1).await;
    let pake1_exchange = p.exchange_id;
    let p_a = pase::decode_pake1(&payload).expect("pake1 malformed");

    // --- SPAKE2+ verifier 計算 ---
    let verifier = spake2p::Spake2pVerifier::from_passcode(passcode, SALT, ITERATIONS);
    let p_b = verifier.p_b();
    let shared = verifier
        .finish(&p_a, &context, b"", b"")
        .expect("pA on curve");
    let k_e = shared.k_e;
    let c_b = shared.c_b;
    let expected_c_a = shared.expected_c_a;

    // --- Pake2 ---
    let pake2_payload = pase::encode_pake2(&p_b, &c_b);
    let pake2_dg = build_unsecured(
        201,
        OPCODE_PASE_PAKE2,
        pake1_exchange,
        Some(pake1_counter),
        false,
        &pake2_payload,
    );
    transport
        .send_to(&pake2_dg, initiator_addr)
        .await
        .expect("send pake2");

    // --- Pake3（cA 検証）---
    let (p, payload, pake3_counter, _) = recv_unsecured(&transport, OPCODE_PASE_PAKE3).await;
    let pake3_exchange = p.exchange_id;
    let c_a = pase::decode_pake3(&payload).expect("pake3 malformed");
    assert_eq!(c_a, expected_c_a, "initiator cA mismatch (transcript bug?)");

    // --- StatusReport(success) ---
    let status = [0u8; 8]; // general=0, protocol id=0, code=0
    let status_dg = build_unsecured(
        202,
        OPCODE_STATUS_REPORT,
        pake3_exchange,
        Some(pake3_counter),
        false,
        &status,
    );
    transport
        .send_to(&status_dg, initiator_addr)
        .await
        .expect("send pase status report");

    // --- SessionKeys（spec §4.13.2.3: HKDF(salt=[], ikm=Ke, "SessionKeys")）---
    let okm = hkdf48(&k_e, &[], INFO_SESSION_KEYS);
    let i2r: [u8; 16] = okm[..16].try_into().unwrap();
    let r2i: [u8; 16] = okm[16..32].try_into().unwrap();

    // --- Serve one secured IM ReadRequest（PASE は両側 node id 0）---
    serve_one_read(
        &transport,
        initiator_addr,
        EstablishedSession {
            our_session_id: resp_session_id,
            peer_session_id: initiator_session_id,
            i2r: &i2r,
            r2i: &r2i,
            open_node_id: 0,
            seal_node_id: 0,
            message_counter: 2000,
        },
    )
    .await;
    initiator_addr
}
