//! CASE responder state machine (verifier/responder side, spec §4.14). Pure
//! — one opcode+payload in, one [`CaseOutput`] out, no tokio, no sockets, no
//! files.
//!
//! Lives in `mat-controller` (not `mat-device`) even though it's a
//! *device*-role state machine: `mat-device` depends on `mat-controller`,
//! never the other way around, and `mat-controller`'s own
//! `test_support::responder_task` (the test-only CASE responder this module
//! productionizes/replaces) needs to build on it too. This mirrors how
//! `spake2p` already hosts both the PASE prover and verifier roles in one
//! crate — protocol code belongs to the controller crate's protocol layer
//! regardless of which side of the wire it plays. `mat-device`'s
//! `core::case` is a thin wrapper around this module: it converts its own
//! `FabricEntry` (fabric persistence + AddNOC bookkeeping fields this crate
//! has no business knowing about) into this module's [`CaseFabric`] (just
//! the fields CASE itself needs) and re-exports [`CaseOutput`]/
//! [`CaseCoreError`] unchanged.
//!
//! Productionizes `test_support::responder_task` (a hand-rolled test-only
//! CASE responder proven against `case::establish` in
//! `case_self_handshake.rs`, and itself migrated onto this module — see that
//! function's doc comment): same wire constants (opcodes, TBE nonces, HKDF
//! info strings), same TBS/TBE framing (sender-eph before receiver-eph in
//! TBS2/TBS3), same transcript-hash boundaries (S2K salted with
//! SHA256(sigma1) alone; S3K over SHA256(sigma1||sigma2); SessionKeys over
//! SHA256(sigma1||sigma2||sigma3)) — ported by reusing the *same* HKDF
//! helpers the production initiator (`case`) uses
//! (`derive_sigma_key`/`derive_session_keys`), rather than an independent
//! re-derivation, since both roles now live in the same crate.
//!
//! Two additions over the test responder (mat-device M1's production
//! scope): 1. **Fabric selection**: Sigma1's destinationId is matched
//! against `fabric::case_destination_id` computed for *every* candidate
//! [`CaseFabric`] this device holds (the test responder always assumed
//! exactly one fabric). No match → [`CaseCoreError::NoSharedTrustRoots`].
//! 2. **Peer NOC chain verification**: unchanged in substance
//! (`cert::verify_noc_chain` against the *selected* fabric's root), plus a
//! defense-in-depth check that the peer's own NOC fabric id equals the
//! selected fabric's id (mirrors `case::establish`'s analogous
//! `PeerIdentityMismatch` check on the initiator side).
//!
//! State machine: `Idle --Sigma1--> AwaitSigma3 --Sigma3--> Established`. Any
//! opcode that doesn't match the current state (including replays and
//! out-of-order messages) is rejected with
//! [`CaseCoreError::UnexpectedMessage`] — the caller is expected to answer
//! with a StatusReport failure and drop the exchange; this core never
//! retries or recovers on its own. `AwaitSigma3`'s `CaseFabric`/transcript
//! byte-vectors force `State` to be non-`Copy` (unlike `pase::pase_context`-
//! style `Copy` states) — `handle_sigma3` takes ownership of the state via
//! `std::mem::replace(&mut self.state, State::Established)` up front, so any
//! early `Err` return leaves `self.state` at the terminal `Established`
//! rather than restoring `AwaitSigma3` (harmless: the "never retries"
//! contract already means a core that returned `Err` is expected to be
//! discarded by its caller, not resumed).

use sha2::{Digest, Sha256};

use crate::case::{
    derive_session_keys, derive_sigma_key, encode_status_report, eph_pub_bytes, random_p256_secret,
};
use crate::cert::{verify_noc_chain, CertError, MatterCert};
use crate::crypto::{decrypt_payload, encrypt_payload, sign_ecdsa_p256, verify_ecdsa_p256};
use crate::fabric::case_destination_id;
use crate::message::OPCODE_STATUS_REPORT;
use crate::session::SessionKeys;
use crate::tlv::{skip_container, Reader, Tag, Value, Writer};

// Wire opcodes (spec §4.14) — mirror of the ones in `case.rs`.
pub(crate) const OPCODE_SIGMA1: u8 = 0x30;
pub(crate) const OPCODE_SIGMA2: u8 = 0x31;
pub(crate) const OPCODE_SIGMA3: u8 = 0x32;

const TBE2_NONCE: &[u8; 13] = b"NCASE_Sigma2N";
const TBE3_NONCE: &[u8; 13] = b"NCASE_Sigma3N";
const INFO_S2K: &[u8] = b"Sigma2";
const INFO_S3K: &[u8] = b"Sigma3";

/// The subset of a fabric's material CASE-as-responder needs: enough to
/// answer `case_destination_id` for fabric selection, sign/verify TBS2/TBS3,
/// and encrypt/decrypt TBE2/TBE3. Deliberately narrower than `mat-device`'s
/// `FabricEntry` (which additionally carries `admin_subject` and
/// persistence bookkeeping this crate has no business knowing about) —
/// callers convert their own fabric-table entry into this shape.
#[derive(Clone)]
pub struct CaseFabric {
    pub fabric_index: u8,
    pub root_tlv: Vec<u8>,
    pub noc_tlv: Vec<u8>,
    pub icac_tlv: Option<Vec<u8>>,
    pub op_private_key: [u8; 32],
    pub ipk_operational: [u8; 16],
    pub node_id: u64,
    pub fabric_id: u64,
    pub root_public_key: [u8; 65],
}

/// One `on_message` step's result. `Reply` is a mid-handshake response
/// (Sigma2) the caller must send back reliably. `Established` is the
/// terminal StatusReport(success) plus the derived session keys, the peer's
/// (initiator's) session id and node id, and which fabric was selected — the
/// caller needs all of these to build a `SecureSession::new_device_role(...)`.
pub enum CaseOutput {
    Reply(Vec<u8>, u8),
    Established {
        reply: Vec<u8>,
        opcode: u8,
        keys: SessionKeys,
        peer_session_id: u16,
        peer_node_id: u64,
        fabric_index: u8,
    },
}

/// Protocol violation or verification failure. The caller (net driver)
/// answers with a StatusReport failure and drops the exchange — this core
/// never retries or recovers.
#[derive(Debug)]
pub enum CaseCoreError {
    /// Wrong opcode for the current state (covers replays/out-of-order
    /// messages too — the state machine only accepts the one opcode its
    /// current state expects).
    UnexpectedMessage(u8),
    /// A TLV field was missing, mistyped, or truncated. Carries a short
    /// static description of what was being decoded.
    Decode(&'static str),
    /// Sigma1's destinationId didn't match `case_destination_id` for any
    /// fabric this device holds (spec §4.14.2.1.2 / §4.11.3's
    /// `NO_SHARED_TRUST_ROOTS`).
    NoSharedTrustRoots,
    /// TBE3 decryption failed (wrong S3K, i.e. wrong shared secret or
    /// transcript, or a corrupted payload).
    Tbe3DecryptFailed,
    /// The initiator's NOC/ICAC chain didn't verify against the selected
    /// fabric's root.
    PeerCertInvalid(CertError),
    /// The initiator's own NOC chains to the selected fabric's root but
    /// carries a different fabric id (defense-in-depth — see module doc).
    PeerFabricMismatch,
    /// TBS3's signature didn't verify under the initiator's NOC public key.
    Sigma3SignatureInvalid,
    /// A signing/encryption operation failed (payload-size limits etc).
    Crypto(&'static str),
}

impl std::fmt::Display for CaseCoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaseCoreError::UnexpectedMessage(op) => {
                write!(f, "case: unexpected opcode 0x{op:02X} for current state")
            }
            CaseCoreError::Decode(what) => write!(f, "case: malformed message ({what})"),
            CaseCoreError::NoSharedTrustRoots => {
                write!(f, "case: sigma1 destination id matched no fabric")
            }
            CaseCoreError::Tbe3DecryptFailed => write!(
                f,
                "case: TBE3 decryption failed (wrong S3K or corrupted payload)"
            ),
            CaseCoreError::PeerCertInvalid(e) => {
                write!(f, "case: initiator certificate chain invalid: {e}")
            }
            CaseCoreError::PeerFabricMismatch => write!(
                f,
                "case: initiator NOC fabric id does not match the selected fabric"
            ),
            CaseCoreError::Sigma3SignatureInvalid => {
                write!(f, "case: TBS3 signature verification failed")
            }
            CaseCoreError::Crypto(what) => write!(f, "case: crypto error: {what}"),
        }
    }
}

impl std::error::Error for CaseCoreError {}

/// Data carried from Sigma1's handling into Sigma3's. `CaseFabric` (owned
/// TLV byte vectors) forces this to be non-`Copy` — see the module doc for
/// how `handle_sigma3` deals with that.
///
/// `large_enum_variant`: `Idle`/`Established` are zero-sized next to
/// `AwaitSigma3`'s transcript/fabric payload. Boxing `AwaitSigma3` would
/// trade one heap allocation per handshake for a few dozen bytes of stack
/// padding on `Idle`/`Established` — not worth it for a per-exchange,
/// non-hot-path struct.
#[allow(clippy::large_enum_variant)]
enum State {
    Idle,
    AwaitSigma3 {
        fabric: CaseFabric,
        shared: [u8; 32],
        sigma1_payload: Vec<u8>,
        sigma2_payload: Vec<u8>,
        responder_eph_pub: [u8; 65],
        initiator_eph_pub: [u8; 65],
        initiator_session_id: u16,
    },
    Established,
}

/// Pure CASE responder state machine (verifier/responder role). Feed it one
/// opcode+payload at a time via [`on_message`](Self::on_message); it never
/// touches I/O.
pub struct CaseResponderCore {
    fabrics: Vec<CaseFabric>,
    responder_session_id: u16,
    state: State,
}

impl CaseResponderCore {
    pub fn new(fabrics: Vec<CaseFabric>, responder_session_id: u16) -> Self {
        Self {
            fabrics,
            responder_session_id,
            state: State::Idle,
        }
    }

    /// Feeds one incoming opcode+payload, returns the response to send (or
    /// the terminal `Established` output). `Err` on any protocol violation
    /// or verification failure.
    pub fn on_message(&mut self, opcode: u8, payload: &[u8]) -> Result<CaseOutput, CaseCoreError> {
        match &self.state {
            State::Idle => {
                if opcode != OPCODE_SIGMA1 {
                    return Err(CaseCoreError::UnexpectedMessage(opcode));
                }
                self.handle_sigma1(payload)
            }
            State::AwaitSigma3 { .. } => {
                if opcode != OPCODE_SIGMA3 {
                    return Err(CaseCoreError::UnexpectedMessage(opcode));
                }
                self.handle_sigma3(payload)
            }
            State::Established => Err(CaseCoreError::UnexpectedMessage(opcode)),
        }
    }

    fn handle_sigma1(&mut self, payload: &[u8]) -> Result<CaseOutput, CaseCoreError> {
        let sigma1 = parse_sigma1(payload)?;

        let fabric = self
            .fabrics
            .iter()
            .find(|f| {
                case_destination_id(
                    &f.ipk_operational,
                    &sigma1.initiator_random,
                    &f.root_public_key,
                    f.fabric_id,
                    f.node_id,
                ) == sigma1.dest_id
            })
            .cloned()
            .ok_or(CaseCoreError::NoSharedTrustRoots)?;

        let resp_secret = random_p256_secret();
        let responder_eph_pub = eph_pub_bytes(&resp_secret);
        let mut responder_random = [0u8; 32];
        getrandom::getrandom(&mut responder_random).expect("os rng");

        let shared = ecdh(&resp_secret, &sigma1.initiator_eph_pub)?;
        let sigma1_hash = sha256(payload);

        let mut s2k_salt = Vec::with_capacity(16 + 32 + 65 + 32);
        s2k_salt.extend_from_slice(&fabric.ipk_operational);
        s2k_salt.extend_from_slice(&responder_random);
        s2k_salt.extend_from_slice(&responder_eph_pub);
        s2k_salt.extend_from_slice(&sigma1_hash);
        let s2k = derive_sigma_key(&shared, &s2k_salt, INFO_S2K);

        let tbs2 = encode_tbs(
            &fabric.noc_tlv,
            fabric.icac_tlv.as_deref(),
            &responder_eph_pub,
            &sigma1.initiator_eph_pub,
        );
        let sig2 = sign_ecdsa_p256(&fabric.op_private_key, &tbs2)
            .map_err(|_| CaseCoreError::Crypto("sigma2 signature"))?;
        // Sigma2Resume (spec §4.14.2's abbreviated-resume path) is out of M2
        // scope — this responder always answers with a full Sigma2, so the
        // resumption id it hands out is never redeemed. It still has to be
        // present and 16 bytes: chip's Sigma2 parser expects TBE2's tag 4
        // unconditionally (the top known Echo-interop risk this task fixes),
        // so we generate and embed one per handshake without persisting it
        // anywhere.
        let mut resumption_id = [0u8; 16];
        getrandom::getrandom(&mut resumption_id).expect("os rng");
        let tbe2 = encode_tbe(
            &fabric.noc_tlv,
            fabric.icac_tlv.as_deref(),
            &sig2,
            Some(&resumption_id),
        );
        let encrypted2 = encrypt_payload(&s2k, TBE2_NONCE, b"", &tbe2)
            .map_err(|_| CaseCoreError::Crypto("sigma2 payload too large"))?;

        let sigma2_payload = encode_sigma2(
            &responder_random,
            self.responder_session_id,
            &responder_eph_pub,
            &encrypted2,
        );

        self.state = State::AwaitSigma3 {
            fabric,
            shared,
            sigma1_payload: payload.to_vec(),
            sigma2_payload: sigma2_payload.clone(),
            responder_eph_pub,
            initiator_eph_pub: sigma1.initiator_eph_pub,
            initiator_session_id: sigma1.initiator_session_id,
        };

        Ok(CaseOutput::Reply(sigma2_payload, OPCODE_SIGMA2))
    }

    fn handle_sigma3(&mut self, payload: &[u8]) -> Result<CaseOutput, CaseCoreError> {
        // Takes ownership up front — see module doc on why an early `Err`
        // from here leaves `self.state` at `Established` rather than
        // restoring `AwaitSigma3`.
        let State::AwaitSigma3 {
            fabric,
            shared,
            sigma1_payload,
            sigma2_payload,
            responder_eph_pub,
            initiator_eph_pub,
            initiator_session_id,
        } = std::mem::replace(&mut self.state, State::Established)
        else {
            unreachable!("on_message only calls handle_sigma3 from AwaitSigma3");
        };

        let encrypted3 = parse_sigma3(payload)?;

        let mut s1s2 = Vec::with_capacity(sigma1_payload.len() + sigma2_payload.len());
        s1s2.extend_from_slice(&sigma1_payload);
        s1s2.extend_from_slice(&sigma2_payload);
        let sigma12_hash = sha256(&s1s2);
        let mut s3k_salt = Vec::with_capacity(16 + 32);
        s3k_salt.extend_from_slice(&fabric.ipk_operational);
        s3k_salt.extend_from_slice(&sigma12_hash);
        let s3k = derive_sigma_key(&shared, &s3k_salt, INFO_S3K);

        let tbe3 = decrypt_payload(&s3k, TBE3_NONCE, b"", &encrypted3)
            .map_err(|_| CaseCoreError::Tbe3DecryptFailed)?;
        let (init_noc_tlv, init_icac_tlv, sig3) = parse_tbe(&tbe3)?;

        let init_noc = MatterCert::parse(&init_noc_tlv).map_err(CaseCoreError::PeerCertInvalid)?;
        let init_icac = init_icac_tlv
            .as_deref()
            .map(MatterCert::parse)
            .transpose()
            .map_err(CaseCoreError::PeerCertInvalid)?;
        let root = MatterCert::parse(&fabric.root_tlv).map_err(CaseCoreError::PeerCertInvalid)?;
        verify_noc_chain(&init_noc, init_icac.as_ref(), &root)
            .map_err(CaseCoreError::PeerCertInvalid)?;

        let peer_node_id = init_noc.node_id().expect("verify_noc_chain guarantees ids");
        let peer_fabric_id = init_noc
            .fabric_id()
            .expect("verify_noc_chain guarantees ids");
        if peer_fabric_id != fabric.fabric_id {
            return Err(CaseCoreError::PeerFabricMismatch);
        }

        let tbs3 = encode_tbs(
            &init_noc_tlv,
            init_icac_tlv.as_deref(),
            &initiator_eph_pub,
            &responder_eph_pub,
        );
        verify_ecdsa_p256(&init_noc.pub_key, &tbs3, &sig3)
            .map_err(|_| CaseCoreError::Sigma3SignatureInvalid)?;

        let mut s1s2s3 = Vec::with_capacity(s1s2.len() + payload.len());
        s1s2s3.extend_from_slice(&s1s2);
        s1s2s3.extend_from_slice(payload);
        let final_hash = sha256(&s1s2s3);
        let keys = derive_session_keys(&shared, &fabric.ipk_operational, &final_hash);

        Ok(CaseOutput::Established {
            reply: encode_status_report(0, 0, 0), // general=SUCCESS, protocol id=0, code=0
            opcode: OPCODE_STATUS_REPORT,
            keys,
            peer_session_id: initiator_session_id,
            peer_node_id,
            fabric_index: fabric.fabric_index,
        })
    }
}

// --- wire codecs (this module's own copies, byte-identical to `case.rs`'s
// initiator-side Sigma1/Sigma3/TBE3 framing where the shapes coincide) ---

struct Sigma1Fields {
    initiator_random: [u8; 32],
    initiator_session_id: u16,
    dest_id: [u8; 32],
    initiator_eph_pub: [u8; 65],
}

/// Parses Sigma1: `struct{1: random, 2: session_id, 3: dest_id, 4: eph_pub,
/// ...}` (any optional fields past tag 4 — resumption, session params — are
/// ignored; `case::encode_sigma1` never sends them). Resumption fields (tag
/// 6/7) are deliberately tolerated and ignored — full-handshake fallback per
/// spec §4.14.2; Sigma2Resume is out of M2 scope.
fn parse_sigma1(payload: &[u8]) -> Result<Sigma1Fields, CaseCoreError> {
    let mut r = Reader::new(payload);
    match r
        .next()
        .map_err(|_| CaseCoreError::Decode("sigma1 tlv"))?
        .map(|e| e.value)
    {
        Some(Value::StructStart) => {}
        _ => return Err(CaseCoreError::Decode("sigma1 top-level struct")),
    }
    let mut random: Option<[u8; 32]> = None;
    let mut session_id: Option<u16> = None;
    let mut dest: Option<[u8; 32]> = None;
    let mut eph: Option<[u8; 65]> = None;
    loop {
        let el = r
            .next()
            .map_err(|_| CaseCoreError::Decode("sigma1 tlv"))?
            .ok_or(CaseCoreError::Decode("sigma1 truncated"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => {
                random = Some(
                    b.try_into()
                        .map_err(|_| CaseCoreError::Decode("sigma1 random length"))?,
                );
            }
            (Tag::Context(2), Value::Uint(v)) => {
                session_id =
                    Some(u16::try_from(v).map_err(|_| CaseCoreError::Decode("sigma1 session id"))?);
            }
            // initiatorSessionParams（tag 5 の struct）等のネストは丸ごと
            // 読み飛ばす — 中身の context tag をこのレベルの field と
            // 誤読しない（matter.js は必ず sessionParams を含める）。
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r).map_err(|_| CaseCoreError::Decode("sigma1 tlv"))?;
            }
            (Tag::Context(3), Value::Bytes(b)) => {
                dest = Some(
                    b.try_into()
                        .map_err(|_| CaseCoreError::Decode("sigma1 dest id length"))?,
                );
            }
            (Tag::Context(4), Value::Bytes(b)) => {
                eph = Some(
                    b.try_into()
                        .map_err(|_| CaseCoreError::Decode("sigma1 eph length"))?,
                );
            }
            _ => {}
        }
    }
    Ok(Sigma1Fields {
        initiator_random: random.ok_or(CaseCoreError::Decode("sigma1 random"))?,
        initiator_session_id: session_id.ok_or(CaseCoreError::Decode("sigma1 session id"))?,
        dest_id: dest.ok_or(CaseCoreError::Decode("sigma1 dest id"))?,
        initiator_eph_pub: eph.ok_or(CaseCoreError::Decode("sigma1 eph"))?,
    })
}

/// Encodes Sigma2: `struct{1: responder_random, 2: responder_session_id,
/// 3: responder_eph_pub, 4: encrypted2}`.
fn encode_sigma2(random: &[u8; 32], session_id: u16, eph: &[u8; 65], encrypted2: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), random);
    w.put_uint(Tag::Context(2), u64::from(session_id));
    w.put_bytes(Tag::Context(3), eph);
    w.put_bytes(Tag::Context(4), encrypted2);
    w.end_container();
    w.finish()
}

/// Extracts the single context-1 byte string from a Sigma3 payload
/// (`struct{1: encrypted3}`).
fn parse_sigma3(payload: &[u8]) -> Result<Vec<u8>, CaseCoreError> {
    let mut r = Reader::new(payload);
    match r
        .next()
        .map_err(|_| CaseCoreError::Decode("sigma3 tlv"))?
        .map(|e| e.value)
    {
        Some(Value::StructStart) => {}
        _ => return Err(CaseCoreError::Decode("sigma3 top-level struct")),
    }
    let mut enc: Option<Vec<u8>> = None;
    loop {
        let el = r
            .next()
            .map_err(|_| CaseCoreError::Decode("sigma3 tlv"))?
            .ok_or(CaseCoreError::Decode("sigma3 truncated"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => enc = Some(b.to_vec()),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r).map_err(|_| CaseCoreError::Decode("sigma3 tlv"))?;
            }
            _ => {}
        }
    }
    enc.ok_or(CaseCoreError::Decode("sigma3 missing encrypted3"))
}

/// `(noc_tlv, icac_tlv, signature)` — a decrypted TBE's fields.
type ParsedTbe = (Vec<u8>, Option<Vec<u8>>, [u8; 64]);

/// Parses a decrypted TBE payload: `struct{1: noc, [2: icac], 3: signature,
/// ...}` (any trailing optional fields — e.g. resumption id — are ignored).
fn parse_tbe(payload: &[u8]) -> Result<ParsedTbe, CaseCoreError> {
    let mut r = Reader::new(payload);
    match r
        .next()
        .map_err(|_| CaseCoreError::Decode("tbe tlv"))?
        .map(|e| e.value)
    {
        Some(Value::StructStart) => {}
        _ => return Err(CaseCoreError::Decode("tbe top-level struct")),
    }
    let mut noc: Option<Vec<u8>> = None;
    let mut icac: Option<Vec<u8>> = None;
    let mut sig: Option<[u8; 64]> = None;
    loop {
        let el = r
            .next()
            .map_err(|_| CaseCoreError::Decode("tbe tlv"))?
            .ok_or(CaseCoreError::Decode("tbe truncated"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => noc = Some(b.to_vec()),
            (Tag::Context(2), Value::Bytes(b)) => icac = Some(b.to_vec()),
            (Tag::Context(3), Value::Bytes(b)) => {
                sig = Some(
                    b.try_into()
                        .map_err(|_| CaseCoreError::Decode("tbe signature length"))?,
                );
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r).map_err(|_| CaseCoreError::Decode("tbe tlv"))?;
            }
            _ => {}
        }
    }
    Ok((
        noc.ok_or(CaseCoreError::Decode("tbe noc"))?,
        icac,
        sig.ok_or(CaseCoreError::Decode("tbe signature"))?,
    ))
}

/// TBS payload signed over in Sigma2/Sigma3:
/// `struct{1: noc, [2: icac], 3: sender_eph_pub, 4: receiver_eph_pub}`.
/// Byte-identical to `case.rs`'s private `encode_tbs` (sender before
/// receiver).
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

/// TBE plaintext (encrypted into Sigma2's `encrypted2`):
/// `struct{1: noc, [2: icac], 3: signature, [4: resumptionID]}` (spec
/// §4.14.2). `resumption_id` is `Some` only for TBE2 (TBS3/TBE3 have no
/// resumption id — the *initiator* builds TBE3 in `case.rs`, not this
/// module, and never passes one). Byte-identical shape to `case.rs`'s
/// private `encode_tbe3` when `resumption_id` is `None` (used there for
/// TBE3).
fn encode_tbe(
    noc: &[u8],
    icac: Option<&[u8]>,
    sig: &[u8; 64],
    resumption_id: Option<&[u8; 16]>,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), noc);
    if let Some(icac) = icac {
        w.put_bytes(Tag::Context(2), icac);
    }
    w.put_bytes(Tag::Context(3), sig);
    if let Some(id) = resumption_id {
        w.put_bytes(Tag::Context(4), id);
    }
    w.end_container();
    w.finish()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// ECDH between our (responder) ephemeral secret and the initiator's
/// ephemeral public key.
fn ecdh(secret: &p256::SecretKey, peer_pub: &[u8; 65]) -> Result<[u8; 32], CaseCoreError> {
    let pk = p256::PublicKey::from_sec1_bytes(peer_pub)
        .map_err(|_| CaseCoreError::Decode("initiator ephemeral key"))?;
    let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), pk.as_affine());
    let mut out = [0u8; 32];
    out.copy_from_slice(shared.raw_secret_bytes().as_slice());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::encode_sigma1;

    // Fixtures: node01_01 chained through ica01 to root01 (same fixtures
    // `test_support`/`case.rs`'s own tests use).
    const IPK: [u8; 16] = [0xCC; 16];
    const NODE01_NOC: &[u8] = include_bytes!("../tests/fixtures/node01_01_chip.bin");
    const NODE01_PRIV: &[u8] = include_bytes!("../tests/fixtures/node01_01_privkey.bin");
    const ICA01: &[u8] = include_bytes!("../tests/fixtures/ica01_chip.bin");
    const ROOT01_CHIP: &[u8] = include_bytes!("../tests/fixtures/root01_chip.bin");
    const ROOT01_PRIV: &[u8] = include_bytes!("../tests/fixtures/root01_privkey.bin");

    fn fabric() -> CaseFabric {
        let noc_cert = MatterCert::parse(NODE01_NOC).expect("parse node01_01 noc");
        let root_cert = MatterCert::parse(ROOT01_CHIP).expect("parse root01");
        CaseFabric {
            fabric_index: 1,
            root_tlv: ROOT01_CHIP.to_vec(),
            noc_tlv: NODE01_NOC.to_vec(),
            icac_tlv: Some(ICA01.to_vec()),
            op_private_key: NODE01_PRIV.try_into().expect("32 bytes"),
            ipk_operational: IPK,
            node_id: noc_cert.node_id().expect("node id"),
            fabric_id: noc_cert.fabric_id().expect("fabric id"),
            root_public_key: root_cert.pub_key,
        }
    }

    /// Manually decodes a Sigma2 payload's tag2 (responder session id) and
    /// tag3 (responder eph pub) — enough to check the core answered with the
    /// right shape without needing `case.rs`'s (still `pub(crate)`)
    /// `parse_sigma2`. Full crypto correctness (TBE2 decrypts, TBS2
    /// verifies, Sigma3/session-keys round-trip) is proven end-to-end
    /// against the *production* initiator in `mat-device`'s
    /// `tests/case_establish.rs` and, within this crate, by
    /// `case_self_handshake.rs` now that `test_support::responder_task`
    /// itself is built on this module.
    fn decode_sigma2_session_id_and_eph(payload: &[u8]) -> (u16, [u8; 65]) {
        let mut r = Reader::new(payload);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut session_id = None;
        let mut eph = None;
        loop {
            let el = r.next().unwrap().expect("truncated sigma2");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(2), Value::Uint(v)) => session_id = Some(v as u16),
                (Tag::Context(3), Value::Bytes(b)) => eph = Some(b.try_into().expect("65 bytes")),
                _ => {}
            }
        }
        (session_id.expect("session id"), eph.expect("eph"))
    }

    /// matter.js は Sigma1 に initiatorSessionParams（tag 5 の struct）を
    /// 含める。その中の tag 2 は SESSION_ACTIVE_INTERVAL（matter.js 既定
    /// 300ms）で、ネストを平坦に舐める旧実装は initiatorSessionId を 300 に
    /// 上書きしていた。実測: HA matter-server 1.1.7 からの commissioning で
    /// CASE 後の matv 発パケットが全て「Ignoring message for unknown
    /// session 0x012c(=300)」で捨てられ完了不能（2026-08-18）。
    #[test]
    fn parse_sigma1_ignores_nested_session_params() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[0x42; 32]);
        w.put_uint(Tag::Context(2), 0x1234);
        w.put_bytes(Tag::Context(3), &[0x24; 32]);
        w.put_bytes(Tag::Context(4), &[0x04; 65]);
        w.start_struct(Tag::Context(5)); // initiatorSessionParams
        w.put_uint(Tag::Context(1), 5000); // SESSION_IDLE_INTERVAL
        w.put_uint(Tag::Context(2), 300); // SESSION_ACTIVE_INTERVAL
        w.put_uint(Tag::Context(3), 4000); // SESSION_ACTIVE_THRESHOLD
        w.end_container();
        w.end_container();

        let parsed = parse_sigma1(&w.finish()).expect("sigma1 with session params parses");
        assert_eq!(
            parsed.initiator_session_id, 0x1234,
            "nested session params must not overwrite the session id"
        );
    }

    fn build_sigma1(fabric: &CaseFabric) -> Vec<u8> {
        let initiator_secret = random_p256_secret();
        let initiator_eph = eph_pub_bytes(&initiator_secret);
        let initiator_random = [0x42u8; 32];
        let dest_id = case_destination_id(
            &fabric.ipk_operational,
            &initiator_random,
            &fabric.root_public_key,
            fabric.fabric_id,
            fabric.node_id,
        );
        encode_sigma1(&initiator_random, 0x1234, &dest_id, &initiator_eph)
    }

    #[test]
    fn sigma1_gets_a_sigma2_reply_with_our_responder_session_id() {
        let f = fabric();
        let mut core = CaseResponderCore::new(vec![f.clone()], 0xB0B1);
        let sigma1 = build_sigma1(&f);

        let CaseOutput::Reply(sigma2_bytes, opcode) =
            core.on_message(OPCODE_SIGMA1, &sigma1).unwrap()
        else {
            panic!("expected Reply");
        };
        assert_eq!(opcode, OPCODE_SIGMA2);
        let (session_id, eph) = decode_sigma2_session_id_and_eph(&sigma2_bytes);
        assert_eq!(session_id, 0xB0B1);
        assert_ne!(eph, [0u8; 65]); // a real ephemeral point was generated
    }

    #[test]
    fn sigma2_tbe_carries_a_16_byte_resumption_id() {
        let f = fabric();
        let mut core = CaseResponderCore::new(vec![f.clone()], 0xB0B1);

        let initiator_secret = random_p256_secret();
        let initiator_eph = eph_pub_bytes(&initiator_secret);
        let initiator_random = [0x42u8; 32];
        let dest_id = case_destination_id(
            &f.ipk_operational,
            &initiator_random,
            &f.root_public_key,
            f.fabric_id,
            f.node_id,
        );
        let sigma1 = encode_sigma1(&initiator_random, 0x1234, &dest_id, &initiator_eph);
        let CaseOutput::Reply(sigma2, _) = core.on_message(OPCODE_SIGMA1, &sigma1).unwrap() else {
            panic!("expected Reply")
        };

        // Sigma2 から responder_random(1)/eph(3)/encrypted2(4) を取り出し S2K を導出
        let mut r = Reader::new(&sigma2);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let (mut rrand, mut reph, mut enc2) = (None, None, None);
        loop {
            let el = r.next().unwrap().expect("truncated sigma2");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(1), Value::Bytes(b)) => {
                    rrand = Some(<[u8; 32]>::try_from(b).unwrap())
                }
                (Tag::Context(3), Value::Bytes(b)) => reph = Some(<[u8; 65]>::try_from(b).unwrap()),
                (Tag::Context(4), Value::Bytes(b)) => enc2 = Some(b.to_vec()),
                _ => {}
            }
        }
        let (rrand, reph, enc2) = (rrand.unwrap(), reph.unwrap(), enc2.unwrap());
        let shared = ecdh(&initiator_secret, &reph).unwrap();
        let sigma1_hash = sha256(&sigma1);
        let mut s2k_salt = Vec::new();
        s2k_salt.extend_from_slice(&f.ipk_operational);
        s2k_salt.extend_from_slice(&rrand);
        s2k_salt.extend_from_slice(&reph);
        s2k_salt.extend_from_slice(&sigma1_hash);
        let s2k = derive_sigma_key(&shared, &s2k_salt, INFO_S2K);
        let tbe2 = decrypt_payload(&s2k, TBE2_NONCE, b"", &enc2).unwrap();

        // TBE2 の tag 4 = 16 byte resumption id
        let mut r = Reader::new(&tbe2);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut resumption = None;
        loop {
            let el = r.next().unwrap().expect("truncated tbe2");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(4), Value::Bytes(b)) => resumption = Some(b.to_vec()),
                _ => {}
            }
        }
        assert_eq!(resumption.expect("tbe2 resumption id").len(), 16);
    }

    #[test]
    fn resumption_sigma1_falls_back_to_full_sigma2() {
        let f = fabric();
        let mut core = CaseResponderCore::new(vec![f.clone()], 0xB0B1);
        let initiator_secret = random_p256_secret();
        let initiator_eph = eph_pub_bytes(&initiator_secret);
        let initiator_random = [0x42u8; 32];
        let dest_id = case_destination_id(
            &f.ipk_operational,
            &initiator_random,
            &f.root_public_key,
            f.fabric_id,
            f.node_id,
        );
        // encode_sigma1 相当 + resumptionID(6) + initiatorResumeMIC(7) を後置
        let sigma1 = {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_bytes(Tag::Context(1), &initiator_random);
            w.put_uint(Tag::Context(2), 0x1234);
            w.put_bytes(Tag::Context(3), &dest_id);
            w.put_bytes(Tag::Context(4), &initiator_eph);
            w.put_bytes(Tag::Context(6), &[0xAB; 16]); // 知らない resumptionID
            w.put_bytes(Tag::Context(7), &[0xCD; 16]); // resume MIC
            w.end_container();
            w.finish()
        };
        let CaseOutput::Reply(_, opcode) = core.on_message(OPCODE_SIGMA1, &sigma1).unwrap() else {
            panic!("expected full Sigma2 fallback")
        };
        assert_eq!(opcode, OPCODE_SIGMA2);
    }

    #[test]
    fn rejects_sigma1_with_no_matching_fabric() {
        let mut core = CaseResponderCore::new(vec![fabric()], 0xB0B1);

        let initiator_secret = random_p256_secret();
        let initiator_eph = eph_pub_bytes(&initiator_secret);
        // Destination id computed with the wrong fabric id -> no candidate matches.
        let bogus_dest = case_destination_id(&IPK, &[0x42u8; 32], &[0u8; 65], 0xDEAD, 0xBEEF);
        let sigma1 = encode_sigma1(&[0x42u8; 32], 0x1234, &bogus_dest, &initiator_eph);

        assert!(matches!(
            core.on_message(OPCODE_SIGMA1, &sigma1),
            Err(CaseCoreError::NoSharedTrustRoots)
        ));
    }

    #[test]
    fn rejects_sigma3_before_sigma1() {
        let mut core = CaseResponderCore::new(vec![fabric()], 0xB0B1);
        let bogus_sigma3 = {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_bytes(Tag::Context(1), b"not-real");
            w.end_container();
            w.finish()
        };
        assert!(matches!(
            core.on_message(OPCODE_SIGMA3, &bogus_sigma3),
            Err(CaseCoreError::UnexpectedMessage(op)) if op == OPCODE_SIGMA3
        ));
    }

    #[test]
    fn rejects_replayed_sigma1_after_transitioning_to_await_sigma3() {
        let f = fabric();
        let mut core = CaseResponderCore::new(vec![f.clone()], 0xB0B1);
        let sigma1 = build_sigma1(&f);
        core.on_message(OPCODE_SIGMA1, &sigma1).unwrap();
        assert!(matches!(
            core.on_message(OPCODE_SIGMA1, &sigma1),
            Err(CaseCoreError::UnexpectedMessage(op)) if op == OPCODE_SIGMA1
        ));
    }

    /// Drives a *real* Sigma3 (correct S3K, correct TBS3 signature) whose
    /// NOC chains cleanly to our root — `verify_noc_chain` alone would
    /// accept it — but whose subject carries a *different* fabric id than
    /// the fabric we selected in Sigma1/Sigma2. This is the case
    /// `PeerFabricMismatch` exists for (new authorization-relevant logic,
    /// not ported from `test_support`'s original hand-rolled responder — no
    /// prior test exercised it). Everything up to the fabric-id comparison
    /// must succeed for this test to be meaningful, so a broken/inverted
    /// comparison would go undetected without it.
    #[test]
    fn rejects_peer_noc_chaining_to_our_root_with_a_different_fabric_id() {
        let f = fabric();
        let mut core = CaseResponderCore::new(vec![f.clone()], 0xB0B1);

        // --- Sigma1 -> Sigma2, keeping the initiator's ephemeral secret
        // around (unlike `build_sigma1`) so we can carry the handshake
        // through to a real Sigma3 below. ---
        let initiator_secret = random_p256_secret();
        let initiator_eph = eph_pub_bytes(&initiator_secret);
        let initiator_random = [0x42u8; 32];
        let dest_id = case_destination_id(
            &f.ipk_operational,
            &initiator_random,
            &f.root_public_key,
            f.fabric_id,
            f.node_id,
        );
        let sigma1 = encode_sigma1(&initiator_random, 0x1234, &dest_id, &initiator_eph);
        let CaseOutput::Reply(sigma2, _opcode) = core.on_message(OPCODE_SIGMA1, &sigma1).unwrap()
        else {
            panic!("expected Reply");
        };
        let (_responder_session_id, responder_eph_pub) = decode_sigma2_session_id_and_eph(&sigma2);
        let shared = ecdh(&initiator_secret, &responder_eph_pub).expect("ecdh");

        // --- A second NOC, signed by the SAME root01 key (so
        // `verify_noc_chain` succeeds), but with a fabric id different from
        // `f.fabric_id` — the case `PeerFabricMismatch` guards against. ---
        let root_cert = MatterCert::parse(ROOT01_CHIP).expect("parse root01");
        let root_priv: [u8; 32] = ROOT01_PRIV.try_into().expect("32 bytes");
        let wrong_fabric_id = f.fabric_id.wrapping_add(1);
        let fake_op_secret = random_p256_secret();
        let fake_op_pub = eph_pub_bytes(&fake_op_secret);
        let fake_op_priv: [u8; 32] = fake_op_secret.to_bytes().into();
        let mut serial = [0u8; 8];
        getrandom::getrandom(&mut serial).expect("os rng");
        serial[0] &= 0x7F; // BER INTEGER minimal positive form
        let fake_noc = crate::cert::issue_noc(
            &fake_op_pub,
            0xAAAA_BBBB,
            wrong_fabric_id,
            &root_cert,
            &root_priv,
            &serial,
        )
        .expect("issue second noc under root01");
        // Sanity: this NOC really does chain to our root, so the failure we
        // assert below is specifically the fabric-id check, not an
        // (unrelated) chain-verification failure.
        verify_noc_chain(&fake_noc, None, &root_cert).expect("fake noc chains to root01");
        let fake_noc_tlv = fake_noc.to_tlv();

        // --- Real Sigma3: correct S3K, correct TBS3 signature under the
        // fake NOC's own key. ---
        let mut s1s2 = Vec::with_capacity(sigma1.len() + sigma2.len());
        s1s2.extend_from_slice(&sigma1);
        s1s2.extend_from_slice(&sigma2);
        let sigma12_hash = sha256(&s1s2);
        let mut s3k_salt = Vec::with_capacity(16 + 32);
        s3k_salt.extend_from_slice(&f.ipk_operational);
        s3k_salt.extend_from_slice(&sigma12_hash);
        let s3k = derive_sigma_key(&shared, &s3k_salt, INFO_S3K);

        let tbs3 = encode_tbs(&fake_noc_tlv, None, &initiator_eph, &responder_eph_pub);
        let sig3 = sign_ecdsa_p256(&fake_op_priv, &tbs3).expect("sign tbs3");
        let tbe3 = encode_tbe(&fake_noc_tlv, None, &sig3, None);
        let encrypted3 = encrypt_payload(&s3k, TBE3_NONCE, b"", &tbe3).expect("encrypt tbe3");
        let sigma3 = {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_bytes(Tag::Context(1), &encrypted3);
            w.end_container();
            w.finish()
        };

        assert!(matches!(
            core.on_message(OPCODE_SIGMA3, &sigma3),
            Err(CaseCoreError::PeerFabricMismatch)
        ));
    }
}
