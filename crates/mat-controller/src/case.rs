//! CASE initiator state machine (Sigma1 -> Sigma2 verify -> Sigma3 -> StatusReport).
//!
//! Establishes a secured session with a peer already on our fabric (spec
//! §4.14). This module owns the wire encoding of Sigma1/2/3, transcript
//! hashing, NOC-chain / signature verification of the peer, and the HKDF
//! derivations feeding `session::SessionKeys`. Protocol code stays here —
//! callers only see `establish()` and a `SecureSession` on success.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use p256::elliptic_curve::sec1::ToEncodedPoint;
use sha2::{Digest, Sha256};

use crate::cert::{verify_noc_chain, MatterCert};
use crate::exchange::{ExchangeError, MrpConfig, UnsecuredExchange};
use crate::fabric::{case_destination_id, FabricCredentials};
use crate::message::{OPCODE_STATUS_REPORT, PROTOCOL_ID_SECURE_CHANNEL};
use crate::session::{SecureSession, SessionKeys};
use crate::tlv::{Reader, Tag, TlvError, Value, Writer};
use crate::transport::UdpTransport;

const OPCODE_CASE_SIGMA1: u8 = 0x30;
const OPCODE_CASE_SIGMA2: u8 = 0x31;
const OPCODE_CASE_SIGMA3: u8 = 0x32;
// StatusReport は message::OPCODE_STATUS_REPORT (0x40)

const TBE2_NONCE: &[u8; 13] = b"NCASE_Sigma2N";
const TBE3_NONCE: &[u8; 13] = b"NCASE_Sigma3N";
const INFO_S2K: &[u8] = b"Sigma2";
const INFO_S3K: &[u8] = b"Sigma3";
const INFO_SESSION_KEYS: &[u8] = b"SessionKeys";
const STATUS_SUCCESS: (u16, u32, u16) = (0, 0, 0); // (general, protocol id, code)

/// Wait budget for a real (non-ack) response once the previous message has
/// already been acknowledged — covers device-side TBE compute/verify time.
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// CASE establishment error. `Display` always names the sigma stage and
/// what was rejected, so callers (M4) have enough to map onto `mat` error
/// kinds without re-deriving context.
#[derive(Debug)]
pub enum CaseError {
    Exchange(ExchangeError),
    UnexpectedMessage {
        stage: &'static str,
        opcode: u8,
    },
    PeerStatus {
        stage: &'static str,
        general_code: u16,
        protocol_code: u16,
    },
    Sigma2NotAcked,
    Sigma2Malformed(&'static str),
    Tbe2DecryptFailed,
    PeerCertInvalid(crate::cert::CertError),
    PeerIdentityMismatch {
        expected_node_id: u64,
        cert_node_id: u64,
        expected_fabric_id: u64,
        cert_fabric_id: u64,
    },
    Sigma2SignatureInvalid,
    EstablishmentFailed {
        general_code: u16,
        protocol_code: u16,
    }, // StatusReport が success でない
    Crypto(&'static str),
}

impl std::fmt::Display for CaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaseError::Exchange(e) => write!(f, "case: exchange error: {e}"),
            CaseError::UnexpectedMessage { stage, opcode } => {
                write!(f, "case {stage}: unexpected message opcode 0x{opcode:02X}")
            }
            CaseError::PeerStatus {
                stage,
                general_code,
                protocol_code,
            } => write!(
                f,
                "case {stage}: peer rejected with StatusReport (general=0x{general_code:04X}, code=0x{protocol_code:04X})"
            ),
            CaseError::Sigma2NotAcked => {
                write!(f, "case sigma1: peer's response did not acknowledge Sigma1")
            }
            CaseError::Sigma2Malformed(what) => {
                write!(f, "case sigma2: malformed message ({what})")
            }
            CaseError::Tbe2DecryptFailed => write!(
                f,
                "case sigma2: TBE2 decryption failed (wrong S2K or corrupted payload)"
            ),
            CaseError::PeerCertInvalid(e) => {
                write!(f, "case sigma2: peer certificate chain invalid: {e}")
            }
            CaseError::PeerIdentityMismatch {
                expected_node_id,
                cert_node_id,
                expected_fabric_id,
                cert_fabric_id,
            } => write!(
                f,
                "case sigma2: peer identity mismatch (expected node {expected_node_id:#018x} / fabric {expected_fabric_id:#018x}, got node {cert_node_id:#018x} / fabric {cert_fabric_id:#018x})"
            ),
            CaseError::Sigma2SignatureInvalid => {
                write!(f, "case sigma2: TBS signature verification failed")
            }
            CaseError::EstablishmentFailed {
                general_code,
                protocol_code,
            } => write!(
                f,
                "case sigma3: peer StatusReport was not success (general=0x{general_code:04X}, code=0x{protocol_code:04X})"
            ),
            CaseError::Crypto(what) => write!(f, "case: crypto error: {what}"),
        }
    }
}

impl std::error::Error for CaseError {}

pub(crate) struct Sigma2 {
    pub responder_random: [u8; 32],
    pub responder_session_id: u16,
    pub responder_eph_pub: [u8; 65],
    pub encrypted2: Vec<u8>,
}

pub(crate) struct Tbe2 {
    pub noc: Vec<u8>,
    pub icac: Option<Vec<u8>>,
    pub signature: [u8; 64],
}

/// Encodes Sigma1: `struct{1: random, 2: session_id, 3: dest_id, 4: eph_pub}`.
/// No optional fields (resumption, session params) are sent.
pub(crate) fn encode_sigma1(
    random: &[u8; 32],
    session_id: u16,
    dest_id: &[u8; 32],
    eph_pub: &[u8; 65],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), random);
    w.put_uint(Tag::Context(2), u64::from(session_id));
    w.put_bytes(Tag::Context(3), dest_id);
    w.put_bytes(Tag::Context(4), eph_pub);
    w.end_container();
    w.finish()
}

/// Skips a container (struct/array/list) whose `StructStart`/`ArrayStart`/
/// `ListStart` element has already been consumed, up to and including its
/// matching `ContainerEnd`.
fn skip_container(r: &mut Reader<'_>) -> Result<(), TlvError> {
    let mut depth = 1usize;
    while depth > 0 {
        let el = r.next()?.ok_or(TlvError::Truncated)?;
        match el.value {
            Value::StructStart | Value::ArrayStart | Value::ListStart => depth += 1,
            Value::ContainerEnd => depth -= 1,
            _ => {}
        }
    }
    Ok(())
}

/// Parses Sigma2: `struct{1: responder_random, 2: responder_session_id,
/// 3: responder_eph_pub, 4: encrypted2, [5: session params (skipped)]}`.
pub(crate) fn parse_sigma2(payload: &[u8]) -> Result<Sigma2, CaseError> {
    let mut r = Reader::new(payload);
    match r
        .next()
        .map_err(|_| CaseError::Sigma2Malformed("tlv"))?
        .map(|e| e.value)
    {
        Some(Value::StructStart) => {}
        _ => return Err(CaseError::Sigma2Malformed("top-level struct")),
    }

    let mut responder_random: Option<[u8; 32]> = None;
    let mut responder_session_id: Option<u16> = None;
    let mut responder_eph_pub: Option<[u8; 65]> = None;
    let mut encrypted2: Option<Vec<u8>> = None;

    loop {
        let el = r
            .next()
            .map_err(|_| CaseError::Sigma2Malformed("tlv"))?
            .ok_or(CaseError::Sigma2Malformed("truncated"))?;
        match el.value {
            Value::ContainerEnd => break,
            Value::Bytes(b) if el.tag == Tag::Context(1) => {
                responder_random = Some(
                    b.try_into()
                        .map_err(|_| CaseError::Sigma2Malformed("responder random length"))?,
                );
            }
            Value::Uint(v) if el.tag == Tag::Context(2) => {
                responder_session_id = Some(
                    u16::try_from(v)
                        .map_err(|_| CaseError::Sigma2Malformed("responder session id"))?,
                );
            }
            Value::Bytes(b) if el.tag == Tag::Context(3) => {
                responder_eph_pub =
                    Some(b.try_into().map_err(|_| {
                        CaseError::Sigma2Malformed("responder ephemeral key length")
                    })?);
            }
            Value::Bytes(b) if el.tag == Tag::Context(4) => {
                encrypted2 = Some(b.to_vec());
            }
            Value::StructStart | Value::ArrayStart | Value::ListStart => {
                skip_container(&mut r).map_err(|_| CaseError::Sigma2Malformed("tlv"))?;
            }
            _ => {} // unknown/unsupported scalar field: ignore
        }
    }

    let responder_session_id =
        responder_session_id.ok_or(CaseError::Sigma2Malformed("responder session id"))?;
    if responder_session_id == 0 {
        return Err(CaseError::Sigma2Malformed(
            "responder session id must be non-zero",
        ));
    }

    Ok(Sigma2 {
        responder_random: responder_random.ok_or(CaseError::Sigma2Malformed("responder random"))?,
        responder_session_id,
        responder_eph_pub: responder_eph_pub
            .ok_or(CaseError::Sigma2Malformed("responder ephemeral key"))?,
        encrypted2: encrypted2.ok_or(CaseError::Sigma2Malformed("encrypted2"))?,
    })
}

/// Parses the decrypted TBE payload: `struct{1: noc, [2: icac], 3: signature,
/// [4: resumption id (ignored)]}`. Shared by Sigma2's TBE2 (this module calls
/// it via `decrypt_tbe2`).
fn parse_tbe(payload: &[u8]) -> Result<Tbe2, CaseError> {
    let mut r = Reader::new(payload);
    match r
        .next()
        .map_err(|_| CaseError::Sigma2Malformed("tlv"))?
        .map(|e| e.value)
    {
        Some(Value::StructStart) => {}
        _ => return Err(CaseError::Sigma2Malformed("tbe top-level struct")),
    }

    let mut noc: Option<Vec<u8>> = None;
    let mut icac: Option<Vec<u8>> = None;
    let mut signature: Option<[u8; 64]> = None;

    loop {
        let el = r
            .next()
            .map_err(|_| CaseError::Sigma2Malformed("tlv"))?
            .ok_or(CaseError::Sigma2Malformed("truncated tbe"))?;
        match el.value {
            Value::ContainerEnd => break,
            Value::Bytes(b) if el.tag == Tag::Context(1) => noc = Some(b.to_vec()),
            Value::Bytes(b) if el.tag == Tag::Context(2) => icac = Some(b.to_vec()),
            Value::Bytes(b) if el.tag == Tag::Context(3) => {
                signature = Some(
                    b.try_into()
                        .map_err(|_| CaseError::Sigma2Malformed("tbe signature length"))?,
                );
            }
            Value::StructStart | Value::ArrayStart | Value::ListStart => {
                skip_container(&mut r).map_err(|_| CaseError::Sigma2Malformed("tlv"))?;
            }
            _ => {} // e.g. resumption id: ignored
        }
    }

    Ok(Tbe2 {
        noc: noc.ok_or(CaseError::Sigma2Malformed("tbe noc"))?,
        icac,
        signature: signature.ok_or(CaseError::Sigma2Malformed("tbe signature"))?,
    })
}

/// Decrypts and parses Sigma2's TBE2 blob with the S2K key.
pub(crate) fn decrypt_tbe2(s2k: &[u8; 16], encrypted2: &[u8]) -> Result<Tbe2, CaseError> {
    let pt = crate::crypto::decrypt_payload(s2k, TBE2_NONCE, b"", encrypted2)
        .map_err(|_| CaseError::Tbe2DecryptFailed)?;
    parse_tbe(&pt)
}

/// Parses a StatusReport payload: 8 bytes LE `{general_code: u16,
/// protocol_id: u32, protocol_code: u16}` (spec §4.11.3).
pub(crate) fn parse_status_report(payload: &[u8]) -> Result<(u16, u32, u16), CaseError> {
    if payload.len() < 8 {
        return Err(CaseError::Sigma2Malformed("status report truncated"));
    }
    let general_code = u16::from_le_bytes(payload[0..2].try_into().expect("2 bytes"));
    let protocol_id = u32::from_le_bytes(payload[2..6].try_into().expect("4 bytes"));
    let protocol_code = u16::from_le_bytes(payload[6..8].try_into().expect("2 bytes"));
    Ok((general_code, protocol_id, protocol_code))
}

/// Encodes a StatusReport payload（[`parse_status_report`] の逆）: 8 バイト
/// LE `{general_code, protocol_id, protocol_code}`。initiator 側から handshake
/// を明示的に中断する（例: PASE 確認不一致）ときに使う — 送らずに黙って
/// exchange を破棄すると、responder は Pake3/Sigma3 待ちのタイムアウトまで
/// セッション確立スロットを保持し続けてしまう（spec §4.11.3 / §4.13.1.4）。
pub(crate) fn encode_status_report(
    general_code: u16,
    protocol_id: u32,
    protocol_code: u16,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8);
    buf.extend_from_slice(&general_code.to_le_bytes());
    buf.extend_from_slice(&protocol_id.to_le_bytes());
    buf.extend_from_slice(&protocol_code.to_le_bytes());
    buf
}

pub(crate) fn derive_sigma_key(shared: &[u8], salt: &[u8], info: &[u8]) -> [u8; 16] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), shared);
    let mut out = [0u8; 16];
    hk.expand(info, &mut out).expect("valid length");
    out
}

pub(crate) fn derive_session_keys(
    shared: &[u8],
    ipk: &[u8; 16],
    transcript: &[u8; 32],
) -> SessionKeys {
    let mut salt = Vec::with_capacity(48);
    salt.extend_from_slice(ipk);
    salt.extend_from_slice(transcript);
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&salt), shared);
    let mut okm = [0u8; 48];
    hk.expand(INFO_SESSION_KEYS, &mut okm)
        .expect("valid length");
    SessionKeys {
        i2r: okm[..16].try_into().expect("16"),
        r2i: okm[16..32].try_into().expect("16"),
        attestation_challenge: okm[32..].try_into().expect("16"),
    }
}

/// Generates a fresh non-zero P-256 secret key (rejects the ~0-probability
/// out-of-range case and retries with fresh randomness).
pub(crate) fn random_p256_secret() -> p256::SecretKey {
    loop {
        let mut b = [0u8; 32];
        getrandom::getrandom(&mut b).expect("os rng");
        if let Ok(sk) = p256::SecretKey::from_slice(&b) {
            return sk;
        }
    }
}

/// Generates a non-zero random u16 (session ids must not be zero, spec §4.5.2).
pub(crate) fn random_nonzero_u16() -> u16 {
    loop {
        let mut b = [0u8; 2];
        getrandom::getrandom(&mut b).expect("os rng");
        let v = u16::from_le_bytes(b);
        if v != 0 {
            return v;
        }
    }
}

fn eph_pub_bytes(secret: &p256::SecretKey) -> [u8; 65] {
    let point = secret.public_key().to_encoded_point(false);
    point
        .as_bytes()
        .try_into()
        .expect("uncompressed p256 point is 65 bytes")
}

/// ECDH between our ephemeral secret and the peer's ephemeral public key.
fn ecdh(secret: &p256::SecretKey, peer_pub: &[u8; 65]) -> Result<[u8; 32], CaseError> {
    let pk = p256::PublicKey::from_sec1_bytes(peer_pub)
        .map_err(|_| CaseError::Sigma2Malformed("responder ephemeral key"))?;
    let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), pk.as_affine());
    let mut out = [0u8; 32];
    out.copy_from_slice(shared.raw_secret_bytes().as_slice());
    Ok(out)
}

/// TBS payload signed over in Sigma2/Sigma3:
/// `struct{1: noc, [2: icac], 3: sender_eph_pub, 4: receiver_eph_pub}`.
fn encode_tbs(
    noc: &[u8],
    icac: Option<&[u8]>,
    sender_eph: &[u8; 65],
    receiver_eph: &[u8; 65],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), noc);
    if let Some(icac) = icac {
        w.put_bytes(Tag::Context(2), icac);
    }
    w.put_bytes(Tag::Context(3), sender_eph);
    w.put_bytes(Tag::Context(4), receiver_eph);
    w.end_container();
    w.finish()
}

/// TBE3 plaintext (encrypted into Sigma3):
/// `struct{1: noc, [2: icac], 3: signature}`.
fn encode_tbe3(noc: &[u8], icac: Option<&[u8]>, sig: &[u8; 64]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), noc);
    if let Some(icac) = icac {
        w.put_bytes(Tag::Context(2), icac);
    }
    w.put_bytes(Tag::Context(3), sig);
    w.end_container();
    w.finish()
}

/// Sigma3 wire payload: `struct{1: encrypted3}`.
fn encode_sigma3(encrypted3: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), encrypted3);
    w.end_container();
    w.finish()
}

/// Runs the CASE initiator handshake against `peer` and returns the
/// resulting secured session on success.
pub async fn establish(
    transport: Arc<UdpTransport>,
    peer: SocketAddr,
    creds: &FabricCredentials,
    peer_node_id: u64,
    cfg: &MrpConfig,
) -> Result<SecureSession, CaseError> {
    // 1. Material: initiator random / ephemeral key pair / local session id.
    let mut initiator_random = [0u8; 32];
    getrandom::getrandom(&mut initiator_random).expect("os rng");
    let eph_secret = random_p256_secret();
    let eph_pub = eph_pub_bytes(&eph_secret);
    let local_session_id = random_nonzero_u16();

    // 2. Sigma1.
    let dest_id = case_destination_id(
        &creds.ipk_operational,
        &initiator_random,
        &creds.root_public_key,
        creds.fabric_id,
        peer_node_id,
    );
    let sigma1 = encode_sigma1(&initiator_random, local_session_id, &dest_id, &eph_pub);
    let mut transcript = Sha256::new();
    transcript.update(&sigma1);

    let mut ex = UnsecuredExchange::new(&transport, peer);
    let resp = ex
        .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_CASE_SIGMA1, &sigma1, cfg)
        .await
        .map_err(CaseError::Exchange)?;
    let msg = match resp {
        Some(m) => {
            // The real response's ack must cover the Sigma1 we just sent.
            if m.proto.acked_counter != ex.last_sent_counter() {
                return Err(CaseError::Sigma2NotAcked);
            }
            m
        }
        None => {
            // A standalone ack for Sigma1 already arrived (and already
            // satisfied the ack requirement) — wait for the real Sigma2.
            ex.recv(RECV_TIMEOUT).await.map_err(CaseError::Exchange)?
        }
    };
    match msg.proto.opcode {
        OPCODE_CASE_SIGMA2 => {}
        OPCODE_STATUS_REPORT => {
            let (general_code, _protocol_id, protocol_code) = parse_status_report(&msg.payload)?;
            return Err(CaseError::PeerStatus {
                stage: "sigma1",
                general_code,
                protocol_code,
            });
        }
        op => {
            return Err(CaseError::UnexpectedMessage {
                stage: "sigma1",
                opcode: op,
            })
        }
    }

    // 3. Verify Sigma2.
    let sigma2 = parse_sigma2(&msg.payload)?;
    let shared = ecdh(&eph_secret, &sigma2.responder_eph_pub)?;
    let sigma1_hash: [u8; 32] = transcript.clone().finalize().into();
    let mut s2k_salt = Vec::with_capacity(16 + 32 + 65 + 32);
    s2k_salt.extend_from_slice(&creds.ipk_operational);
    s2k_salt.extend_from_slice(&sigma2.responder_random);
    s2k_salt.extend_from_slice(&sigma2.responder_eph_pub);
    s2k_salt.extend_from_slice(&sigma1_hash);
    let s2k = derive_sigma_key(&shared, &s2k_salt, INFO_S2K);
    // Salt computed against sigma1 alone; now fold Sigma2's raw payload in
    // for subsequent transcript hashes (same order chip-tool uses).
    transcript.update(&msg.payload);

    let tbe2 = decrypt_tbe2(&s2k, &sigma2.encrypted2)?;
    let peer_noc = MatterCert::parse(&tbe2.noc).map_err(CaseError::PeerCertInvalid)?;
    let peer_icac = tbe2
        .icac
        .as_deref()
        .map(MatterCert::parse)
        .transpose()
        .map_err(CaseError::PeerCertInvalid)?;
    let our_rcac = MatterCert::parse(&creds.rcac_tlv).map_err(CaseError::PeerCertInvalid)?;
    verify_noc_chain(&peer_noc, peer_icac.as_ref(), &our_rcac)
        .map_err(CaseError::PeerCertInvalid)?;
    let cert_node_id = peer_noc.node_id().expect("verify_noc_chain guarantees ids");
    let cert_fabric_id = peer_noc
        .fabric_id()
        .expect("verify_noc_chain guarantees ids");
    if cert_node_id != peer_node_id || cert_fabric_id != creds.fabric_id {
        return Err(CaseError::PeerIdentityMismatch {
            expected_node_id: peer_node_id,
            cert_node_id,
            expected_fabric_id: creds.fabric_id,
            cert_fabric_id,
        });
    }
    let tbs2 = encode_tbs(
        &tbe2.noc,
        tbe2.icac.as_deref(),
        &sigma2.responder_eph_pub,
        &eph_pub,
    );
    crate::crypto::verify_ecdsa_p256(&peer_noc.pub_key, &tbs2, &tbe2.signature)
        .map_err(|_| CaseError::Sigma2SignatureInvalid)?;

    // 4. Sigma3.
    let tbs3 = encode_tbs(
        &creds.noc_tlv,
        creds.icac_tlv.as_deref(),
        &eph_pub,
        &sigma2.responder_eph_pub,
    );
    let signature = crate::crypto::sign_ecdsa_p256(&creds.op_private_key, &tbs3)
        .map_err(|_| CaseError::Crypto("sigma3 signature"))?;
    let tbe3 = encode_tbe3(&creds.noc_tlv, creds.icac_tlv.as_deref(), &signature);
    let sigma2_hash: [u8; 32] = transcript.clone().finalize().into();
    let mut s3k_salt = Vec::with_capacity(48);
    s3k_salt.extend_from_slice(&creds.ipk_operational);
    s3k_salt.extend_from_slice(&sigma2_hash);
    let s3k = derive_sigma_key(&shared, &s3k_salt, INFO_S3K);
    let encrypted3 = crate::crypto::encrypt_payload(&s3k, TBE3_NONCE, b"", &tbe3)
        .map_err(|_| CaseError::Crypto("sigma3 payload too large"))?;
    let sigma3 = encode_sigma3(&encrypted3);
    transcript.update(&sigma3);

    let resp = ex
        .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_CASE_SIGMA3, &sigma3, cfg)
        .await
        .map_err(CaseError::Exchange)?;
    let msg = match resp {
        Some(m) => m,
        None => ex.recv(RECV_TIMEOUT).await.map_err(CaseError::Exchange)?,
    };
    if msg.proto.opcode != OPCODE_STATUS_REPORT {
        return Err(CaseError::UnexpectedMessage {
            stage: "sigma3",
            opcode: msg.proto.opcode,
        });
    }
    let (general_code, _protocol_id, protocol_code) = parse_status_report(&msg.payload)?;
    if (general_code, _protocol_id, protocol_code) != STATUS_SUCCESS {
        return Err(CaseError::EstablishmentFailed {
            general_code,
            protocol_code,
        });
    }

    // 5. Session keys.
    let final_hash: [u8; 32] = transcript.finalize().into();
    let keys = derive_session_keys(&shared, &creds.ipk_operational, &final_hash);
    Ok(SecureSession::new(
        transport,
        peer,
        local_session_id,
        sigma2.responder_session_id,
        keys,
        creds.node_id,
        peer_node_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn sigma1_has_spec_structure() {
        let random = [0xAB; 32];
        let dest = [0xCD; 32];
        let eph = [0x04; 65];
        let buf = encode_sigma1(&random, 0x0BB8, &dest, &eph);
        let mut r = Reader::new(&buf);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let e = r.next().unwrap().unwrap();
        assert_eq!((e.tag, e.value), (Tag::Context(1), Value::Bytes(&random)));
        let e = r.next().unwrap().unwrap();
        assert_eq!((e.tag, e.value), (Tag::Context(2), Value::Uint(0x0BB8)));
        let e = r.next().unwrap().unwrap();
        assert_eq!((e.tag, e.value), (Tag::Context(3), Value::Bytes(&dest)));
        let e = r.next().unwrap().unwrap();
        assert_eq!((e.tag, e.value), (Tag::Context(4), Value::Bytes(&eph)));
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);
        assert_eq!(r.next().unwrap(), None); // optional は送らない
    }

    #[test]
    fn parses_sigma2_and_skips_session_params() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[0x11; 32]);
        w.put_uint(Tag::Context(2), 0x1234);
        w.put_bytes(Tag::Context(3), &[0x22; 65]);
        w.put_bytes(Tag::Context(4), b"encrypted-blob");
        w.start_struct(Tag::Context(5)); // session params は読み飛ばす
        w.put_uint(Tag::Context(1), 5000);
        w.end_container();
        w.end_container();
        let s2 = parse_sigma2(&w.finish()).unwrap();
        assert_eq!(s2.responder_random, [0x11; 32]);
        assert_eq!(s2.responder_session_id, 0x1234);
        assert_eq!(s2.responder_eph_pub, [0x22; 65]);
        assert_eq!(s2.encrypted2, b"encrypted-blob");
        assert!(parse_sigma2(&[0x15, 0x18]).is_err()); // 必須欠落
    }

    #[test]
    fn parse_sigma2_rejects_zero_responder_session_id() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[0x11; 32]);
        w.put_uint(Tag::Context(2), 0); // responder session id = 0 は不正
        w.put_bytes(Tag::Context(3), &[0x22; 65]);
        w.put_bytes(Tag::Context(4), b"encrypted-blob");
        w.end_container();
        assert!(matches!(
            parse_sigma2(&w.finish()),
            Err(CaseError::Sigma2Malformed(_))
        ));
    }

    #[test]
    fn decrypts_and_parses_tbe2_roundtrip() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), b"noc-tlv");
        w.put_bytes(Tag::Context(3), &[0x77; 64]);
        w.put_bytes(Tag::Context(4), &[0x88; 16]);
        w.end_container();
        let key = [0x42; 16];
        let ct = crate::crypto::encrypt_payload(&key, TBE2_NONCE, b"", &w.finish()).unwrap();
        let tbe = decrypt_tbe2(&key, &ct).unwrap();
        assert_eq!(tbe.noc, b"noc-tlv");
        assert_eq!(tbe.icac, None);
        assert_eq!(tbe.signature, [0x77; 64]);
        assert!(matches!(
            decrypt_tbe2(&[0x00; 16], &ct),
            Err(CaseError::Tbe2DecryptFailed)
        ));
    }

    #[test]
    fn parses_status_report() {
        let ok = [0u8, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_status_report(&ok).unwrap(), (0, 0, 0));
        let busy = [1u8, 0, 0, 0, 0, 0, 4, 0]; // FAILURE / SC / BUSY
        assert_eq!(parse_status_report(&busy).unwrap(), (1, 0, 4));
        assert!(parse_status_report(&[0u8; 4]).is_err());
    }

    #[test]
    fn encode_status_report_round_trips_through_parse() {
        let buf = encode_status_report(1, 0, 4); // FAILURE / SC / BUSY
        assert_eq!(buf, [1u8, 0, 0, 0, 0, 0, 4, 0]);
        assert_eq!(parse_status_report(&buf).unwrap(), (1, 0, 4));
    }

    #[test]
    fn session_key_derivation_is_deterministic() {
        let keys = derive_session_keys(&[0x01; 32], &[0x02; 16], &[0x03; 32]);
        let again = derive_session_keys(&[0x01; 32], &[0x02; 16], &[0x03; 32]);
        assert_eq!(keys.i2r, again.i2r);
        assert_eq!(keys.r2i, again.r2i);
        assert_ne!(keys.i2r, keys.r2i);
    }
}
