//! CASE initiator state machine (Sigma1 -> Sigma2 verify -> Sigma3 -> StatusReport).
//!
//! Establishes a secured session with a peer already on our fabric (spec
//! §4.14). This module owns the transcript hashing, NOC-chain / signature
//! verification of the peer, and the HKDF derivations feeding
//! `session::SessionKeys`; the Sigma1/2/3 / TBS / TBE wire encoding it
//! shares with the responder role lives in the private `wire` submodule.
//! Protocol code stays here — callers only see `establish()` and a
//! `SecureSession` on success.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use p256::elliptic_curve::sec1::ToSec1Point;
use sha2::{Digest, Sha256};

use crate::cert::{verify_noc_chain, MatterCert};
use crate::exchange::{ExchangeError, MrpConfig, UnsecuredExchange};
use crate::fabric::{case_destination_id, FabricCredentials};
use crate::message::{OPCODE_STATUS_REPORT, PROTOCOL_ID_SECURE_CHANNEL};
use crate::race::race_staggered;
use crate::secure_channel::{
    StatusReportTruncated, OPCODE_SIGMA1, OPCODE_SIGMA2, OPCODE_SIGMA3, STATUS_REPORT_SUCCESS,
};
use crate::session::{SecureSession, SessionKeys};
use crate::transport::{Transport, UdpTransport};

pub(crate) use crate::secure_channel::SC_PROTOCOL_CODE_CLOSE_SESSION;
/// StatusReport codec — lives in [`crate::secure_channel`]. `encode_status_report`
/// is re-exported here because `mat-device`'s CASE net driver
/// (`mat-device/src/net/case.rs`) imports it from `case`; `parse_status_report`
/// is re-exported alongside it for symmetry and for this crate's own callers.
pub use crate::secure_channel::{encode_status_report, parse_status_report};

pub(crate) mod wire;

/// Sigma1 encoder — lives in `case::wire` (one copy, shared with the responder
/// role); re-exported so `mat-device`'s CASE tests keep importing it from
/// here.
pub use wire::encode_sigma1;
pub(crate) use wire::{Sigma2, Tbe as Tbe2};

/// CASE ハンドシェイク各往復の応答待ち。op 予算設計の成分。
pub const RECV_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// A StatusReport reply that was too short to decode. `stage` is the
    /// message it answered (`"sigma1"` or `"sigma3"`).
    StatusReportMalformed {
        stage: &'static str,
    },
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
            CaseError::StatusReportMalformed { stage } => {
                write!(f, "case {stage}: malformed StatusReport (truncated)")
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

/// Parses Sigma2 through [`wire::parse_sigma2`], labelling the failure as
/// this role's [`CaseError::Sigma2Malformed`].
pub(crate) fn parse_sigma2(payload: &[u8]) -> Result<Sigma2, CaseError> {
    wire::parse_sigma2(payload).map_err(CaseError::Sigma2Malformed)
}

/// Decrypts and parses Sigma2's TBE2 blob with the S2K key.
pub(crate) fn decrypt_tbe2(s2k: &[u8; 16], encrypted2: &[u8]) -> Result<Tbe2, CaseError> {
    let pt = crate::crypto::decrypt_payload(s2k, wire::TBE2_NONCE, b"", encrypted2)
        .map_err(|_| CaseError::Tbe2DecryptFailed)?;
    wire::parse_tbe(&pt).map_err(CaseError::Sigma2Malformed)
}

/// `pub`（Task 10）: mat-device の CASE responder core が S2K/S3K をこの関数で
/// 導出する（HKDF salt/info の取り違えを防ぐため initiator 側と同一実装を共有）。
pub fn derive_sigma_key(shared: &[u8], salt: &[u8], info: &[u8]) -> [u8; 16] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), shared);
    let mut out = [0u8; 16];
    hk.expand(info, &mut out).expect("valid length");
    out
}

/// `pub`（Task 10）: mat-device の CASE responder core がセッション鍵をこの
/// 関数で導出する（initiator 側と同一実装を共有）。
pub fn derive_session_keys(shared: &[u8], ipk: &[u8; 16], transcript: &[u8; 32]) -> SessionKeys {
    let mut salt = Vec::with_capacity(48);
    salt.extend_from_slice(ipk);
    salt.extend_from_slice(transcript);
    crate::secure_channel::session_keys_from_hkdf(&salt, shared)
}

/// Generates a fresh non-zero P-256 secret key (rejects the ~0-probability
/// out-of-range case and retries with fresh randomness). `pub`（Task 10）:
/// shared with mat-device's CASE responder core (ephemeral keypair) and its
/// integration test (device operational keypair).
pub fn random_p256_secret() -> p256::SecretKey {
    loop {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).expect("os rng");
        if let Ok(sk) = p256::SecretKey::from_slice(&b) {
            return sk;
        }
    }
}

/// Generates a non-zero random u16 (session ids must not be zero, spec §4.5.2).
/// `pub`（Task 10）: shared with mat-device's CASE responder net driver
/// (though its `responder_session_id` today comes from a fixed config value —
/// kept here for future randomized-session-id use, same rationale as
/// `random_p256_secret`).
pub fn random_nonzero_u16() -> u16 {
    loop {
        let mut b = [0u8; 2];
        getrandom::fill(&mut b).expect("os rng");
        let v = u16::from_le_bytes(b);
        if v != 0 {
            return v;
        }
    }
}

/// `secret` の SEC1 uncompressed 公開鍵（65 バイト）。CASE の一時鍵専用に
/// 見えるが実質「p256 secret → uncompressed pubkey」の唯一の変換なので、
/// `commissioning::write_kvs_bootstrap`（M8c-3）の使い捨て admin op 鍵にも
/// 再利用する。`pub`（Task 10）: mat-device の CASE responder core（ephemeral
/// 鍵）とその統合テスト（device operational 鍵）でも再利用する。
pub fn eph_pub_bytes(secret: &p256::SecretKey) -> [u8; 65] {
    let point = secret.public_key().to_sec1_point(false);
    point
        .as_bytes()
        .try_into()
        .expect("uncompressed p256 point is 65 bytes")
}

/// Runs the CASE initiator handshake against `peer` and returns the
/// resulting secured session on success.
pub async fn establish(
    transport: Arc<Transport>,
    peer: SocketAddr,
    creds: &FabricCredentials,
    peer_node_id: u64,
    cfg: &MrpConfig,
) -> Result<SecureSession, CaseError> {
    // 1. Material: initiator random / ephemeral key pair / local session id.
    let mut initiator_random = [0u8; 32];
    getrandom::fill(&mut initiator_random).expect("os rng");
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
        .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_SIGMA1, &sigma1, cfg)
        .await
        .map_err(CaseError::Exchange)?;
    let msg = match resp {
        Some(m) => {
            // The real response's ack must cover the Sigma1 we just sent —
            // but only over UDP. On a reliable transport (BTP) MRP is
            // disabled and the peer sends no piggybacked ack.
            if !transport.is_reliable() && m.proto.acked_counter != ex.last_sent_counter() {
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
        OPCODE_SIGMA2 => {}
        OPCODE_STATUS_REPORT => {
            let (general_code, _protocol_id, protocol_code) = parse_status_report(&msg.payload)
                .map_err(|StatusReportTruncated| CaseError::StatusReportMalformed {
                    stage: "sigma1",
                })?;
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
    let shared = wire::ecdh(&eph_secret, &sigma2.responder_eph_pub)
        .ok_or(CaseError::Sigma2Malformed("responder ephemeral key"))?;
    let sigma1_hash: [u8; 32] = transcript.clone().finalize().into();
    let s2k = derive_sigma_key(
        &shared,
        &wire::s2k_salt(
            &creds.ipk_operational,
            &sigma2.responder_random,
            &sigma2.responder_eph_pub,
            &sigma1_hash,
        ),
        wire::INFO_S2K,
    );
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
    let tbs2 = wire::encode_tbs(
        &tbe2.noc,
        tbe2.icac.as_deref(),
        &sigma2.responder_eph_pub,
        &eph_pub,
    );
    crate::crypto::verify_ecdsa_p256(&peer_noc.pub_key, &tbs2, &tbe2.signature)
        .map_err(|_| CaseError::Sigma2SignatureInvalid)?;

    // 4. Sigma3.
    let tbs3 = wire::encode_tbs(
        &creds.noc_tlv,
        creds.icac_tlv.as_deref(),
        &eph_pub,
        &sigma2.responder_eph_pub,
    );
    let signature = crate::crypto::sign_ecdsa_p256(&creds.op_private_key, &tbs3)
        .map_err(|_| CaseError::Crypto("sigma3 signature"))?;
    let tbe3 = wire::encode_tbe(&creds.noc_tlv, creds.icac_tlv.as_deref(), &signature, None);
    let sigma2_hash: [u8; 32] = transcript.clone().finalize().into();
    let s3k = derive_sigma_key(
        &shared,
        &wire::s3k_salt(&creds.ipk_operational, &sigma2_hash),
        wire::INFO_S3K,
    );
    let encrypted3 = crate::crypto::encrypt_payload(&s3k, wire::TBE3_NONCE, b"", &tbe3)
        .map_err(|_| CaseError::Crypto("sigma3 payload too large"))?;
    let sigma3 = wire::encode_sigma3(&encrypted3);
    transcript.update(&sigma3);

    let resp = ex
        .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_SIGMA3, &sigma3, cfg)
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
    let (general_code, _protocol_id, protocol_code) = parse_status_report(&msg.payload)
        .map_err(|StatusReportTruncated| CaseError::StatusReportMalformed { stage: "sigma3" })?;
    if (general_code, _protocol_id, protocol_code) != STATUS_REPORT_SUCCESS {
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

/// 候補アドレスを順に起動する間隔（Happy Eyeballs の stagger）。RFC 8305 の
/// 250ms より長めに取り、健全な先頭アドレスの Sigma2 が返る前に 2 本目の
/// Sigma1 を撃って chip SDK デバイスの BUSY 応答を誘発しにくくしている。
/// 死んだ先頭アドレス 1 本の損失はこの値（従来は MRP 予算いっぱい、
/// SII=5000ms なら ~80 秒）。
pub const RACE_STAGGER: Duration = Duration::from_millis(500);

/// [`establish_any`] の成功結果。
pub struct Established {
    pub session: SecureSession,
    /// 勝った候補アドレス。
    pub peer: SocketAddr,
    /// 勝った試行の専用ソケットの bind アドレス（wildcard bind なら `[::]:port`）。
    /// `ss -uanp` 等とのポート突合用 — IP 部分は bind アドレスそのもので、
    /// 実際の発信元 IP（OS が経路で選ぶもの）とは限らない。
    pub local: Option<SocketAddr>,
}

/// [`establish_any`] のエラー。
#[derive(Debug)]
pub enum EstablishAnyError {
    /// 候補が空（resolve は成功したが AAAA が 1 本も無い）。
    NoAddresses,
    /// 全候補が失敗。候補順。
    AllFailed(Vec<(SocketAddr, CaseError)>),
    /// 試行用ソケットの bind 失敗（1 本でも失敗したらその場で全体エラー）。
    Bind(std::io::Error),
}

impl std::fmt::Display for EstablishAnyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EstablishAnyError::NoAddresses => write!(f, "no addresses"),
            EstablishAnyError::AllFailed(list) => {
                write!(f, "CASE failed on all {} address(es): ", list.len())?;
                for (i, (peer, err)) in list.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{peer}: {err}")?;
                }
                Ok(())
            }
            EstablishAnyError::Bind(e) => write!(f, "bind udp: {e}"),
        }
    }
}

impl std::error::Error for EstablishAnyError {}

/// 複数の候補アドレスへ CASE を Happy Eyeballs 方式で確立する: 候補ごとに
/// **専用の** UDP ソケットを bind し、`stagger` 間隔で [`establish`] を起動、
/// 最初に成功した試行を採用して残りを drop する（[`crate::race`]）。
///
/// 専用ソケットが必須なのは、unsecured exchange の screening が自分の exchange
/// 以外のデータグラムを捨てるため — 1 ソケットを共有すると並行試行が互いの
/// 応答を吸って落とす。同一ノードへの並行 Sigma1 自体は安全（local session
/// id / exchange id / source node id は試行ごとにランダム）。
///
/// 所要時間の上限は設けない（MRP 予算は仕様どおり使い切らせる）。全体は
/// 呼び出し側の op deadline が縛る。
pub async fn establish_any(
    peers: &[SocketAddr],
    creds: &FabricCredentials,
    peer_node_id: u64,
    cfg: &MrpConfig,
    stagger: Duration,
) -> Result<Established, EstablishAnyError> {
    if peers.is_empty() {
        return Err(EstablishAnyError::NoAddresses);
    }
    let mut attempts: Vec<(SocketAddr, Arc<Transport>)> = Vec::with_capacity(peers.len());
    for peer in peers {
        let udp = UdpTransport::bind()
            .await
            .map_err(EstablishAnyError::Bind)?;
        attempts.push((*peer, Arc::new(Transport::Udp(Arc::new(udp)))));
    }

    let outcome = race_staggered(attempts, stagger, |(peer, transport)| async move {
        let local = transport.local_addr().ok();
        match establish(transport, peer, creds, peer_node_id, cfg).await {
            Ok(session) => Ok((session, local)),
            Err(e) => {
                tracing::debug!(%peer, error = %e, "CASE attempt failed");
                Err((peer, e))
            }
        }
    })
    .await;

    match outcome {
        Ok((idx, (session, local))) => Ok(Established {
            session,
            peer: peers[idx],
            local,
        }),
        Err(list) => Err(EstablishAnyError::AllFailed(list)),
    }
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
        let ct = crate::crypto::encrypt_payload(&key, wire::TBE2_NONCE, b"", &w.finish()).unwrap();
        let tbe = decrypt_tbe2(&key, &ct).unwrap();
        assert_eq!(tbe.noc, b"noc-tlv");
        assert_eq!(tbe.icac, None);
        assert_eq!(tbe.signature, [0x77; 64]);
        assert!(matches!(
            decrypt_tbe2(&[0x00; 16], &ct),
            Err(CaseError::Tbe2DecryptFailed)
        ));
    }

    /// A truncated StatusReport after Sigma3 must be labelled with the
    /// sigma3 stage, not "sigma2" (audit 2026-09-12 bug candidate).
    #[test]
    fn status_report_malformed_names_its_stage() {
        let e = CaseError::StatusReportMalformed { stage: "sigma3" };
        assert_eq!(
            e.to_string(),
            "case sigma3: malformed StatusReport (truncated)"
        );
    }

    #[test]
    fn session_key_derivation_is_deterministic() {
        let keys = derive_session_keys(&[0x01; 32], &[0x02; 16], &[0x03; 32]);
        let again = derive_session_keys(&[0x01; 32], &[0x02; 16], &[0x03; 32]);
        assert_eq!(keys.i2r, again.i2r);
        assert_eq!(keys.r2i, again.r2i);
        assert_ne!(keys.i2r, keys.r2i);
    }

    /// CASE セッション鍵（HKDF-SHA256、spec §4.14.2.6）のゴールデン
    /// （2026-09-09、hkdf 0.12 で採取、新系列でも同値）。
    #[test]
    fn golden_session_keys_are_stable() {
        let shared: [u8; 32] = core::array::from_fn(|i| 0x30 + i as u8);
        let ipk: [u8; 16] = core::array::from_fn(|i| 0x50 + i as u8);
        let transcript: [u8; 32] = core::array::from_fn(|i| 0x70 + i as u8);
        let k = derive_session_keys(&shared, &ipk, &transcript);
        assert_eq!(
            k.i2r,
            [
                0x6e, 0xba, 0xaa, 0x00, 0xef, 0xe6, 0xf8, 0xdc, 0xae, 0xa9, 0xb6, 0xab, 0x35, 0x9d,
                0x9d, 0xb9,
            ]
        );
        assert_eq!(
            k.r2i,
            [
                0x07, 0xc9, 0x3a, 0x07, 0x28, 0x91, 0x4d, 0x1d, 0x62, 0x91, 0x14, 0x5e, 0xba, 0x10,
                0x26, 0x3b,
            ]
        );
        assert_eq!(
            k.attestation_challenge,
            [
                0x60, 0xaf, 0xc3, 0x96, 0x93, 0x0a, 0xe4, 0x6f, 0x8d, 0xc8, 0x6e, 0x4a, 0x38, 0xc3,
                0xa9, 0xf4,
            ]
        );
    }
}
