//! Offline CASE self-handshake (mandatory quality gate, plan Task 6).
//!
//! Drives the real CASE initiator (`case::establish`) against a test-only
//! CASE *responder* over loopback UDP, then performs one secured IM read.
//! This is the first *executable* coverage of the CASE crypto-ordering core
//! — transcript boundaries (S2K salted with SHA256(sigma1) alone, sigma2
//! folded in only afterwards; S3K over SHA256(sigma1||sigma2); SessionKeys
//! over SHA256(sigma1||sigma2||sigma3)), the S2K/S3K/SessionKeys HKDF
//! derivations, TBS2/TBS3 orientation (sender-eph before receiver-eph), the
//! i2r/r2i key split, and the Sigma1/2/3 + StatusReport wire framing — none
//! of which the (device-blocked) live E2E can currently exercise.
//!
//! The responder is the mirror image of `case::establish`: same constants
//! (TBE nonces `NCASE_Sigma2N`/`NCASE_Sigma3N`, HKDF info strings, opcodes,
//! SecureChannel protocol id), initiator/responder roles swapped. Its
//! identity is fixture `node01_01` (NOC + private key) chaining through
//! `ica01` to `root01`; the initiator is a fresh self-issued NOC that trusts
//! the same `root01`. IPK and fabric id are shared across both sides (a CASE
//! requirement) by parsing them from `node01_01`.
//!
//! RESIDUAL RISK (documented per the plan): the responder re-defines the
//! CASE constants and derivations independently, but if a constant, an HKDF
//! info string, or a nonce were mirrored *identically wrong* on BOTH the
//! initiator and this responder, the handshake would still succeed here and
//! the defect would stay invisible until a real device. What this test DOES
//! catch is any orientation, ordering, key-split, or framing bug (those are
//! asymmetric between the two roles, so a mistake on one side breaks the
//! handshake). It is not a substitute for on-wire interop, only the best
//! pre-live guard.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use mat_controller::case;
use mat_controller::cert::{verify_noc_chain, MatterCert};
use mat_controller::crypto::{open_message, seal_message, sign_ecdsa_p256, verify_ecdsa_p256};
use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::FabricCredentials;
use mat_controller::im::{self, ImValue};
use mat_controller::kvs::SelfIssueMaterials;
use mat_controller::message::{Destination, MessageHeader, ProtocolHeader};
use mat_controller::tlv::{Reader, Tag, Value, Writer};
use mat_controller::transport::{Transport, UdpTransport, MAX_DATAGRAM};

// CASE constants — mirror of the (crate-private) ones in `case.rs`.
const OPCODE_SIGMA1: u8 = 0x30;
const OPCODE_SIGMA2: u8 = 0x31;
const OPCODE_SIGMA3: u8 = 0x32;
const OPCODE_STATUS_REPORT: u8 = 0x40;
const PROTO_SECURE_CHANNEL: u16 = 0x0000;
const TBE2_NONCE: &[u8; 13] = b"NCASE_Sigma2N";
const TBE3_NONCE: &[u8; 13] = b"NCASE_Sigma3N";
const INFO_S2K: &[u8] = b"Sigma2";
const INFO_S3K: &[u8] = b"Sigma3";
const INFO_SESSION_KEYS: &[u8] = b"SessionKeys";

// Shared fabric material. IPK must be identical on both sides.
const IPK: [u8; 16] = [0xCC; 16];
// Initiator's chosen operational node id (self-issued NOC subject).
const INITIATOR_NODE_ID: u64 = 0x1B669;

// Fixtures: responder identity chain + root private material for the
// initiator's self-issued NOC.
const NODE01_NOC: &[u8] = include_bytes!("fixtures/node01_01_chip.bin");
const NODE01_PRIV: &[u8] = include_bytes!("fixtures/node01_01_privkey.bin");
const ICA01: &[u8] = include_bytes!("fixtures/ica01_chip.bin");
const ROOT01_CHIP: &[u8] = include_bytes!("fixtures/root01_chip.bin");
const ROOT01_PRIV: &[u8] = include_bytes!("fixtures/root01_privkey.bin");

fn fast_cfg() -> MrpConfig {
    MrpConfig {
        initial_interval: Duration::from_millis(50),
        max_retries: 3,
        backoff: 1.0,
    }
}

// --- crypto helpers (re-implemented; the `case.rs` versions are pub(crate)) ---

fn hkdf16(shared: &[u8], salt: &[u8], info: &[u8]) -> [u8; 16] {
    let hk = hkdf::Hkdf::<Sha256>::new(Some(salt), shared);
    let mut out = [0u8; 16];
    hk.expand(info, &mut out).expect("valid length");
    out
}

fn hkdf48(shared: &[u8], salt: &[u8], info: &[u8]) -> [u8; 48] {
    let hk = hkdf::Hkdf::<Sha256>::new(Some(salt), shared);
    let mut out = [0u8; 48];
    hk.expand(info, &mut out).expect("valid length");
    out
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

fn random_secret() -> p256::SecretKey {
    loop {
        let mut b = [0u8; 32];
        getrandom::getrandom(&mut b).expect("os rng");
        if let Ok(sk) = p256::SecretKey::from_slice(&b) {
            return sk;
        }
    }
}

fn eph_pub(sk: &p256::SecretKey) -> [u8; 65] {
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    sk.public_key()
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .expect("uncompressed p256 point is 65 bytes")
}

fn ecdh(sk: &p256::SecretKey, peer_pub: &[u8; 65]) -> [u8; 32] {
    let pk = p256::PublicKey::from_sec1_bytes(peer_pub).expect("valid peer eph pub");
    let shared = p256::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
    let mut out = [0u8; 32];
    out.copy_from_slice(shared.raw_secret_bytes().as_slice());
    out
}

/// TBS payload: `struct{1: noc, [2: icac], 3: sender_eph, 4: receiver_eph}`.
/// Byte-identical to `case.rs`'s `encode_tbs` (sender before receiver).
fn encode_tbs(noc: &[u8], icac: Option<&[u8]>, sender: &[u8; 65], receiver: &[u8; 65]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), noc);
    if let Some(i) = icac {
        w.put_bytes(Tag::Context(2), i);
    }
    w.put_bytes(Tag::Context(3), sender);
    w.put_bytes(Tag::Context(4), receiver);
    w.end_container();
    w.finish()
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

struct Sigma1 {
    payload: Vec<u8>,
    exchange_id: u16,
    counter: u32,
    initiator_eph: [u8; 65],
    initiator_session_id: u16,
}

fn parse_sigma1(payload: &[u8]) -> Sigma1Fields {
    let mut r = Reader::new(payload);
    assert_eq!(
        r.next().unwrap().unwrap().value,
        Value::StructStart,
        "sigma1 top-level struct"
    );
    let mut eph: Option<[u8; 65]> = None;
    let mut session_id: Option<u16> = None;
    loop {
        let el = r.next().unwrap().expect("sigma1 truncated");
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(2), Value::Uint(v)) => session_id = Some(v as u16),
            (Tag::Context(4), Value::Bytes(b)) => {
                eph = Some(b.try_into().expect("initiator eph is 65 bytes"))
            }
            _ => {}
        }
    }
    Sigma1Fields {
        initiator_eph: eph.expect("sigma1 missing eph pub"),
        initiator_session_id: session_id.expect("sigma1 missing session id"),
    }
}

struct Sigma1Fields {
    initiator_eph: [u8; 65],
    initiator_session_id: u16,
}

/// Extracts the single context-1 byte string from a Sigma3 payload
/// (`struct{1: encrypted3}`).
fn parse_sigma3_encrypted(payload: &[u8]) -> Vec<u8> {
    let mut r = Reader::new(payload);
    assert_eq!(
        r.next().unwrap().unwrap().value,
        Value::StructStart,
        "sigma3 top-level struct"
    );
    let mut enc: Option<Vec<u8>> = None;
    loop {
        let el = r.next().unwrap().expect("sigma3 truncated");
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => enc = Some(b.to_vec()),
            _ => {}
        }
    }
    enc.expect("sigma3 missing encrypted3")
}

/// Parses a decrypted TBE (`struct{1: noc, [2: icac], 3: signature, ..}`).
fn parse_tbe(payload: &[u8]) -> (Vec<u8>, Option<Vec<u8>>, [u8; 64]) {
    let mut r = Reader::new(payload);
    assert_eq!(
        r.next().unwrap().unwrap().value,
        Value::StructStart,
        "tbe top-level struct"
    );
    let mut noc: Option<Vec<u8>> = None;
    let mut icac: Option<Vec<u8>> = None;
    let mut sig: Option<[u8; 64]> = None;
    loop {
        let el = r.next().unwrap().expect("tbe truncated");
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => noc = Some(b.to_vec()),
            (Tag::Context(2), Value::Bytes(b)) => icac = Some(b.to_vec()),
            (Tag::Context(3), Value::Bytes(b)) => {
                sig = Some(b.try_into().expect("tbe signature is 64 bytes"))
            }
            _ => {}
        }
    }
    (
        noc.expect("tbe missing noc"),
        icac,
        sig.expect("tbe missing signature"),
    )
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

#[tokio::test]
async fn case_establishes_and_reads_over_loopback() {
    // Responder identity: node01_01 (+ ica01 + root01). Parse its node/fabric
    // id so the initiator's self-issued NOC shares the same fabric.
    let noc_cert = MatterCert::parse(NODE01_NOC).expect("parse node01_01 NOC");
    let responder_node_id = noc_cert.node_id().expect("node id");
    let responder_fabric_id = noc_cert.fabric_id().expect("fabric id");
    // Sanity: node01_01 chains to root01 through ica01 (so the initiator, which
    // trusts root01, will accept it in Sigma2).
    let ica_cert = MatterCert::parse(ICA01).unwrap();
    let root_cert = MatterCert::parse(ROOT01_CHIP).unwrap();
    verify_noc_chain(&noc_cert, Some(&ica_cert), &root_cert).expect("fixture chain");

    // Responder socket first, so we can hand its address to the initiator.
    let responder_transport = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap();
    let responder_addr = responder_transport.local_addr().unwrap();

    let op_priv: [u8; 32] = NODE01_PRIV.try_into().unwrap();
    let responder = tokio::spawn(responder_task(
        responder_transport,
        INITIATOR_NODE_ID,
        responder_node_id,
        NODE01_NOC.to_vec(),
        ICA01.to_vec(),
        op_priv,
        ROOT01_CHIP.to_vec(),
    ));

    // Initiator: fresh self-issued NOC under root01, same IPK and fabric id.
    let materials = SelfIssueMaterials {
        rcac: ROOT01_CHIP.to_vec(),
        root_private_key: ROOT01_PRIV.try_into().unwrap(),
        ipk_operational: IPK,
        node_id: INITIATOR_NODE_ID,
        fabric_id: responder_fabric_id,
    };
    let creds = FabricCredentials::from_self_issued(materials).expect("self-issued creds");

    let initiator_transport = Arc::new(Transport::Udp(Arc::new(
        UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap(),
    )));

    let cfg = fast_cfg();
    let mut session = case::establish(
        Arc::clone(&initiator_transport),
        responder_addr,
        &creds,
        responder_node_id,
        &cfg,
    )
    .await
    .expect("CASE establish should succeed over loopback");

    let value = session
        .read_attribute(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &cfg)
        .await
        .expect("secured read should succeed");
    assert_eq!(value, ImValue::Bool(false));

    responder.await.expect("responder task panicked");
}

/// The test-only CASE responder: the mirror of `case::establish` with the
/// initiator/responder roles swapped. Receives Sigma1, builds and sends
/// Sigma2, verifies Sigma3, sends a success StatusReport, derives the session
/// keys, and serves one secured IM ReadRequest with ReportData(on-off=false).
#[allow(clippy::too_many_arguments)]
async fn responder_task(
    transport: UdpTransport,
    initiator_node_id: u64,
    responder_node_id: u64,
    noc_tlv: Vec<u8>,
    icac_tlv: Vec<u8>,
    op_priv: [u8; 32],
    root_tlv: Vec<u8>,
) {
    // --- Sigma1 ---
    let (sigma1, initiator_addr) = loop {
        let (buf, from) = recv_dg(&transport).await;
        let Some((p, payload)) = decode_unsecured(&buf) else {
            continue;
        };
        if p.opcode != OPCODE_SIGMA1 || !p.initiator {
            continue;
        }
        let (h, _) = MessageHeader::decode(&buf).unwrap();
        let f = parse_sigma1(&payload);
        break (
            Sigma1 {
                payload,
                exchange_id: p.exchange_id,
                counter: h.message_counter,
                initiator_eph: f.initiator_eph,
                initiator_session_id: f.initiator_session_id,
            },
            from,
        );
    };

    // Responder ephemeral material.
    let resp_secret = random_secret();
    let resp_eph_pub = eph_pub(&resp_secret);
    let mut resp_random = [0u8; 32];
    getrandom::getrandom(&mut resp_random).expect("os rng");
    let resp_session_id: u16 = {
        let mut b = [0u8; 2];
        loop {
            getrandom::getrandom(&mut b).expect("os rng");
            let v = u16::from_le_bytes(b);
            if v != 0 {
                break v;
            }
        }
    };

    let shared = ecdh(&resp_secret, &sigma1.initiator_eph);
    let sigma1_hash = sha256(&sigma1.payload);

    // S2K over SHA256(sigma1) alone.
    let mut s2k_salt = Vec::new();
    s2k_salt.extend_from_slice(&IPK);
    s2k_salt.extend_from_slice(&resp_random);
    s2k_salt.extend_from_slice(&resp_eph_pub);
    s2k_salt.extend_from_slice(&sigma1_hash);
    let s2k = hkdf16(&shared, &s2k_salt, INFO_S2K);

    // TBS2: sender = responder eph, receiver = initiator eph.
    let tbs2 = encode_tbs(
        &noc_tlv,
        Some(&icac_tlv),
        &resp_eph_pub,
        &sigma1.initiator_eph,
    );
    let sig2 = sign_ecdsa_p256(&op_priv, &tbs2).expect("sign tbs2");
    let tbe2 = {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &noc_tlv);
        w.put_bytes(Tag::Context(2), &icac_tlv);
        w.put_bytes(Tag::Context(3), &sig2);
        w.put_bytes(Tag::Context(4), &[]);
        w.end_container();
        w.finish()
    };
    let encrypted2 = mat_controller::crypto::encrypt_payload(&s2k, TBE2_NONCE, b"", &tbe2)
        .expect("encrypt tbe2");
    let sigma2_payload = {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &resp_random);
        w.put_uint(Tag::Context(2), u64::from(resp_session_id));
        w.put_bytes(Tag::Context(3), &resp_eph_pub);
        w.put_bytes(Tag::Context(4), &encrypted2);
        w.end_container();
        w.finish()
    };
    let sigma2_dg = build_unsecured(
        100,
        OPCODE_SIGMA2,
        sigma1.exchange_id,
        Some(sigma1.counter),
        false,
        &sigma2_payload,
    );
    transport
        .send_to(&sigma2_dg, initiator_addr)
        .await
        .expect("send sigma2");

    // --- Sigma3 --- (skip retransmitted Sigma1 / standalone acks)
    let sigma3 = loop {
        let (buf, _from) = recv_dg(&transport).await;
        let Some((p, payload)) = decode_unsecured(&buf) else {
            continue;
        };
        if p.opcode != OPCODE_SIGMA3 {
            continue;
        }
        let (h, _) = MessageHeader::decode(&buf).unwrap();
        break (payload, h.message_counter, p.exchange_id);
    };
    let (sigma3_payload, sigma3_counter, sigma3_exchange) = sigma3;

    // S3K over SHA256(sigma1 || sigma2).
    let mut s1s2 = Vec::new();
    s1s2.extend_from_slice(&sigma1.payload);
    s1s2.extend_from_slice(&sigma2_payload);
    let sigma12_hash = sha256(&s1s2);
    let mut s3k_salt = Vec::new();
    s3k_salt.extend_from_slice(&IPK);
    s3k_salt.extend_from_slice(&sigma12_hash);
    let s3k = hkdf16(&shared, &s3k_salt, INFO_S3K);

    // Decrypt TBE3, verify the initiator's NOC chain + TBS3 signature.
    let encrypted3 = parse_sigma3_encrypted(&sigma3_payload);
    let tbe3 = mat_controller::crypto::decrypt_payload(&s3k, TBE3_NONCE, b"", &encrypted3)
        .expect("decrypt tbe3 (S3K derivation / transcript mismatch if this fails)");
    let (init_noc_tlv, init_icac_tlv, sig3) = parse_tbe(&tbe3);
    let init_noc = MatterCert::parse(&init_noc_tlv).expect("parse initiator NOC");
    let init_icac = init_icac_tlv
        .as_deref()
        .map(MatterCert::parse)
        .transpose()
        .expect("parse initiator ICAC");
    let root = MatterCert::parse(&root_tlv).expect("parse root");
    verify_noc_chain(&init_noc, init_icac.as_ref(), &root).expect("initiator NOC chains to root");
    // TBS3: sender = initiator eph, receiver = responder eph.
    let tbs3 = encode_tbs(
        &init_noc_tlv,
        init_icac_tlv.as_deref(),
        &sigma1.initiator_eph,
        &resp_eph_pub,
    );
    verify_ecdsa_p256(&init_noc.pub_key, &tbs3, &sig3).expect("TBS3 signature valid");

    // --- StatusReport(success) ---
    let status = [0u8; 8]; // general=0, protocol id=0, code=0
    let status_dg = build_unsecured(
        101,
        OPCODE_STATUS_REPORT,
        sigma3_exchange,
        Some(sigma3_counter),
        false,
        &status,
    );
    transport
        .send_to(&status_dg, initiator_addr)
        .await
        .expect("send status report");

    // --- Session keys ---
    let mut s1s2s3 = Vec::new();
    s1s2s3.extend_from_slice(&sigma1.payload);
    s1s2s3.extend_from_slice(&sigma2_payload);
    s1s2s3.extend_from_slice(&sigma3_payload);
    let final_hash = sha256(&s1s2s3);
    let mut sk_salt = Vec::new();
    sk_salt.extend_from_slice(&IPK);
    sk_salt.extend_from_slice(&final_hash);
    let okm = hkdf48(&shared, &sk_salt, INFO_SESSION_KEYS);
    let i2r: [u8; 16] = okm[..16].try_into().unwrap();
    let r2i: [u8; 16] = okm[16..32].try_into().unwrap();
    let _attestation: [u8; 16] = okm[32..].try_into().unwrap();

    // --- Serve one secured IM ReadRequest with ReportData(on-off=false) ---
    let (read_exchange, read_counter) = loop {
        let (buf, _from) = recv_dg(&transport).await;
        let (mh, _) = match MessageHeader::decode(&buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if mh.session_id != resp_session_id {
            continue; // unsecured acks (session id 0) etc.
        }
        // Initiator sealed with i2r; nonce uses the initiator's node id.
        let (h, p, _payload) = match open_message(&i2r, &buf, initiator_node_id) {
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
        session_id: sigma1.initiator_session_id, // seal toward the initiator's session
        security_flags: 0,
        message_counter: 1000,
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
    // Responder→initiator messages are sealed with r2i; the nonce uses the
    // responder's node id (which the initiator passed to `establish`).
    let report_dg =
        seal_message(&r2i, &header, &proto, &report, responder_node_id).expect("seal report data");
    transport
        .send_to(&report_dg, initiator_addr)
        .await
        .expect("send report data");
}
