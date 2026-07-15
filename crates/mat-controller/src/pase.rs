//! PASE initiator state machine (PBKDFParamRequest/Response -> PAKE1/2/3 ->
//! StatusReport, spec §4.13).
//!
//! Establishes an unauthenticated (node id 0) secured session with a device
//! that is in commissioning window, using SPAKE2+ over the setup passcode.
//! Wire encoding, transcript-context hashing, and the SessionKeys derivation
//! live here — callers only see `establish()` and a `SecureSession` on
//! success. Mirrors `case::establish`'s exchange pattern (ack verification,
//! standalone-ack-then-recv fallback, StatusReport rejection branch).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::case::{encode_status_report, parse_status_report, random_nonzero_u16};
use crate::exchange::{ExchangeError, MrpConfig, UnsecuredExchange};
use crate::message::{OPCODE_STATUS_REPORT, PROTOCOL_ID_SECURE_CHANNEL};
use crate::session::{SecureSession, SessionKeys};
use crate::spake2p::{self, SpakeError};
use crate::tlv::{Reader, Tag, TlvError, Value, Writer};
use crate::transport::Transport;

/// PBKDFParamRequest/Response opcodes (spec §4.13.1.2). `pub` (not
/// `pub(crate)`) — reused by `tests/btp_pase_plumbing.rs` (a separate crate)
/// so it doesn't have to duplicate these as magic numbers (M6b Task6).
pub const OPCODE_PBKDF_PARAM_REQUEST: u8 = 0x20;
pub const OPCODE_PBKDF_PARAM_RESPONSE: u8 = 0x21;
pub(crate) const OPCODE_PASE_PAKE1: u8 = 0x22;
pub(crate) const OPCODE_PASE_PAKE2: u8 = 0x23;
pub(crate) const OPCODE_PASE_PAKE3: u8 = 0x24;

/// Matter PAKE context string prefix for commissioning (spec §4.13.1.2):
/// `Context = Crypto_Hash("CHIP PAKE V1 Commissioning" || PBKDFParamRequest
/// || PBKDFParamResponse)`.
const PAKE_CONTEXT_PREFIX: &[u8] = b"CHIP PAKE V1 Commissioning";
const INFO_SESSION_KEYS: &[u8] = b"SessionKeys";
const STATUS_SUCCESS: (u16, u16) = (0, 0); // (general_code, protocol_code)
/// SecureChannel protocol の `GeneralStatusCode::FAILURE`（spec §4.11.3 表）。
const GENERAL_CODE_FAILURE: u16 = 1;
/// SecureChannel protocol 固有コード `kInvalidParameter`（spec §4.11.3.1
/// 表・SPAKE2+ 確認不一致など、handshake データ自体が拒否される場合）。
const SC_PROTOCOL_CODE_INVALID_PARAMETER: u16 = 2;

/// Wait budget for a real (non-ack) response once the previous message has
/// already been acknowledged — covers device-side PBKDF/SPAKE2+ compute time.
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// PASE establishment error. `Display` names the stage and what was
/// rejected, so callers can decide recovery (e.g. retry with a fresh
/// passcode on `ConfirmMismatch`).
#[derive(Debug)]
pub enum PaseError {
    Exchange(ExchangeError),
    Malformed(&'static str),
    Spake(SpakeError),
    /// SPAKE2+ 確認メッセージ不一致。passcode 不一致の代表形（spec §3.10 手順3）。
    ConfirmMismatch,
    StatusReport {
        general_code: u16,
        protocol_code: u16,
    },
    NotAcked,
}

impl std::fmt::Display for PaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PaseError::Exchange(e) => write!(f, "pase: exchange error: {e}"),
            PaseError::Malformed(what) => write!(f, "pase: malformed message ({what})"),
            PaseError::Spake(e) => write!(f, "pase: {e}"),
            PaseError::ConfirmMismatch => {
                write!(f, "pase: confirmation mismatch (wrong passcode?)")
            }
            PaseError::StatusReport {
                general_code,
                protocol_code,
            } => write!(
                f,
                "pase: peer rejected with StatusReport (general=0x{general_code:04X}, code=0x{protocol_code:04X})"
            ),
            PaseError::NotAcked => {
                write!(f, "pase: peer's response did not acknowledge our message")
            }
        }
    }
}

impl std::error::Error for PaseError {}

/// Skips a container (struct/array/list) whose `StructStart`/`ArrayStart`/
/// `ListStart` element has already been consumed, up to and including its
/// matching `ContainerEnd`. Local copy of `case::skip_container` (that one
/// is module-private and this file must not widen case.rs's surface beyond
/// `random_nonzero_u16`).
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

/// Encodes PBKDFParamRequest: `struct{1: initiatorRandom[32],
/// 2: initiatorSessionId, 3: passcodeId=0, 4: hasPBKDFParameters=false}`.
/// The optional SessionParams (tag 5) is never sent.
pub(crate) fn encode_pbkdf_param_request(initiator_random: &[u8; 32], session_id: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), initiator_random);
    w.put_uint(Tag::Context(2), u64::from(session_id));
    w.put_uint(Tag::Context(3), 0); // passcodeId: 常に0 (spec §4.13.1.2)
    w.put_bool(Tag::Context(4), false); // hasPBKDFParameters
    w.end_container();
    w.finish()
}

/// Decoded PBKDFParamResponse fields we actually need.
pub(crate) struct PbkdfParamResponse {
    pub responder_session_id: u16,
    pub iterations: u32,
    pub salt: Vec<u8>,
}

/// Parses PBKDFParamResponse: `struct{1: initiatorRandom (ignored),
/// 2: responderRandom[32] (ignored), 3: responderSessionId,
/// 4: struct{1: iterations, 2: salt}, [5: SessionParams (skipped)]}`.
pub(crate) fn decode_pbkdf_param_response(payload: &[u8]) -> Result<PbkdfParamResponse, PaseError> {
    let mut r = Reader::new(payload);
    match r
        .next()
        .map_err(|_| PaseError::Malformed("tlv"))?
        .map(|e| e.value)
    {
        Some(Value::StructStart) => {}
        _ => return Err(PaseError::Malformed("top-level struct")),
    }

    let mut responder_session_id: Option<u16> = None;
    let mut iterations: Option<u32> = None;
    let mut salt: Option<Vec<u8>> = None;

    loop {
        let el = r
            .next()
            .map_err(|_| PaseError::Malformed("tlv"))?
            .ok_or(PaseError::Malformed("truncated"))?;
        match el.value {
            Value::ContainerEnd => break,
            Value::Uint(v) if el.tag == Tag::Context(3) => {
                responder_session_id = Some(
                    u16::try_from(v).map_err(|_| PaseError::Malformed("responder session id"))?,
                );
            }
            Value::StructStart if el.tag == Tag::Context(4) => loop {
                let inner = r
                    .next()
                    .map_err(|_| PaseError::Malformed("tlv"))?
                    .ok_or(PaseError::Malformed("truncated pbkdf parameters"))?;
                match inner.value {
                    Value::ContainerEnd => break,
                    Value::Uint(v) if inner.tag == Tag::Context(1) => {
                        iterations =
                            Some(u32::try_from(v).map_err(|_| PaseError::Malformed("iterations"))?);
                    }
                    Value::Bytes(b) if inner.tag == Tag::Context(2) => {
                        salt = Some(b.to_vec());
                    }
                    Value::StructStart | Value::ArrayStart | Value::ListStart => {
                        skip_container(&mut r).map_err(|_| PaseError::Malformed("tlv"))?;
                    }
                    _ => {} // e.g. tag 3 (bitrate for OWF hash, unused)
                }
            },
            Value::StructStart | Value::ArrayStart | Value::ListStart => {
                skip_container(&mut r).map_err(|_| PaseError::Malformed("tlv"))?;
            }
            _ => {} // e.g. tags 1/2 (randoms): ignored
        }
    }

    Ok(PbkdfParamResponse {
        responder_session_id: responder_session_id
            .ok_or(PaseError::Malformed("responder session id"))?,
        iterations: iterations.ok_or(PaseError::Malformed("iterations"))?,
        salt: salt.ok_or(PaseError::Malformed("salt"))?,
    })
}

/// Encodes Pake1: `struct{1: pA[65]}`.
pub(crate) fn encode_pake1(p_a: &[u8; 65]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), p_a);
    w.end_container();
    w.finish()
}

/// Parses Pake2: `struct{1: pB[65], 2: cB[32]}`.
pub(crate) fn decode_pake2(payload: &[u8]) -> Result<([u8; 65], [u8; 32]), PaseError> {
    let mut r = Reader::new(payload);
    match r
        .next()
        .map_err(|_| PaseError::Malformed("tlv"))?
        .map(|e| e.value)
    {
        Some(Value::StructStart) => {}
        _ => return Err(PaseError::Malformed("top-level struct")),
    }

    let mut p_b: Option<[u8; 65]> = None;
    let mut c_b: Option<[u8; 32]> = None;

    loop {
        let el = r
            .next()
            .map_err(|_| PaseError::Malformed("tlv"))?
            .ok_or(PaseError::Malformed("truncated"))?;
        match el.value {
            Value::ContainerEnd => break,
            Value::Bytes(b) if el.tag == Tag::Context(1) => {
                p_b = Some(
                    b.try_into()
                        .map_err(|_| PaseError::Malformed("pB length"))?,
                );
            }
            Value::Bytes(b) if el.tag == Tag::Context(2) => {
                c_b = Some(
                    b.try_into()
                        .map_err(|_| PaseError::Malformed("cB length"))?,
                );
            }
            Value::StructStart | Value::ArrayStart | Value::ListStart => {
                skip_container(&mut r).map_err(|_| PaseError::Malformed("tlv"))?;
            }
            _ => {}
        }
    }

    Ok((
        p_b.ok_or(PaseError::Malformed("pB"))?,
        c_b.ok_or(PaseError::Malformed("cB"))?,
    ))
}

/// Encodes Pake3: `struct{1: cA[32]}`.
pub(crate) fn encode_pake3(c_a: &[u8; 32]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), c_a);
    w.end_container();
    w.finish()
}

/// Runs the PASE initiator handshake against `peer` and returns the
/// resulting secured session on success. Both sides use node id 0 — PASE
/// sessions are unauthenticated at the Matter fabric layer (spec §4.13).
pub async fn establish(
    transport: Arc<Transport>,
    peer: SocketAddr,
    passcode: u32,
    cfg: &MrpConfig,
) -> Result<SecureSession, PaseError> {
    // 1. Material: initiator random / local session id.
    let mut initiator_random = [0u8; 32];
    getrandom::getrandom(&mut initiator_random).expect("os rng");
    let local_session_id = random_nonzero_u16();

    // 2. PBKDFParamRequest / Response.
    let req = encode_pbkdf_param_request(&initiator_random, local_session_id);
    let mut ex = UnsecuredExchange::new(&transport, peer);
    let resp = ex
        .send_reliable(
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_PBKDF_PARAM_REQUEST,
            &req,
            cfg,
        )
        .await
        .map_err(PaseError::Exchange)?;
    let msg = match resp {
        Some(m) => {
            // On a reliable transport (BTP) MRP is disabled, so the peer's
            // response carries no piggybacked ack — skip the ack check there.
            // Over UDP the real response must ack the request we just sent.
            if !transport.is_reliable() && m.proto.acked_counter != ex.last_sent_counter() {
                return Err(PaseError::NotAcked);
            }
            m
        }
        None => ex.recv(RECV_TIMEOUT).await.map_err(PaseError::Exchange)?,
    };
    match msg.proto.opcode {
        OPCODE_PBKDF_PARAM_RESPONSE => {}
        OPCODE_STATUS_REPORT => {
            let (general_code, _protocol_id, protocol_code) = parse_status_report(&msg.payload)
                .map_err(|_| PaseError::Malformed("status report"))?;
            return Err(PaseError::StatusReport {
                general_code,
                protocol_code,
            });
        }
        _ => {
            return Err(PaseError::Malformed(
                "unexpected opcode after PBKDFParamRequest",
            ))
        }
    }
    let resp_payload = msg.payload;
    let resp = decode_pbkdf_param_response(&resp_payload)?;

    // 3. PAKE context (spec §4.13.1.2).
    let mut hasher = Sha256::new();
    hasher.update(PAKE_CONTEXT_PREFIX);
    hasher.update(&req);
    hasher.update(&resp_payload);
    let context: [u8; 32] = hasher.finalize().into();

    // 4. Pake1 / Pake2.
    let (w0, w1) = spake2p::derive_w0_w1(passcode, &resp.salt, resp.iterations);
    let prover = spake2p::Spake2pProver::new(w0, w1);
    let pake1 = encode_pake1(&prover.p_a());
    let resp2 = ex
        .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_PASE_PAKE1, &pake1, cfg)
        .await
        .map_err(PaseError::Exchange)?;
    let msg2 = match resp2 {
        Some(m) => {
            // On a reliable transport (BTP) MRP is disabled, so the peer's
            // response carries no piggybacked ack — skip the ack check there.
            // Over UDP the real response must ack the request we just sent.
            if !transport.is_reliable() && m.proto.acked_counter != ex.last_sent_counter() {
                return Err(PaseError::NotAcked);
            }
            m
        }
        None => ex.recv(RECV_TIMEOUT).await.map_err(PaseError::Exchange)?,
    };
    match msg2.proto.opcode {
        OPCODE_PASE_PAKE2 => {}
        OPCODE_STATUS_REPORT => {
            let (general_code, _protocol_id, protocol_code) = parse_status_report(&msg2.payload)
                .map_err(|_| PaseError::Malformed("status report"))?;
            return Err(PaseError::StatusReport {
                general_code,
                protocol_code,
            });
        }
        _ => return Err(PaseError::Malformed("unexpected opcode after Pake1")),
    }
    let (p_b, c_b) = decode_pake2(&msg2.payload)?;

    // 5. SPAKE2+ completion / confirmation check / Pake3.
    let shared = prover
        .finish(&p_b, &context, b"", b"")
        .map_err(PaseError::Spake)?;
    if shared.expected_c_b != c_b {
        // 確認不一致（passcode 違い）を黙って諦めると、responder は Pake3 待ちの
        // まま PASE セッション確立スロットを保持し続け、直後の再試行が PBKDF
        // param request すら処理されずに固まる（実機 E2E で発見）。StatusReport
        // で明示的に中断を通知して responder 側のスロットを解放させる。ここは
        // 呼び出し側に既に ConfirmMismatch を返す途中なので、`send_reliable` の
        // MRP 再送ループ（最悪 ~4.7s）で呼び出しをブロックしたくない —
        // `send_once` で一度だけ送って ack は待たない（R フラグは立てるので
        // 相手の MRP は通常どおり ack/再送要求できる）。送達に失敗しても無視する。
        let sr = encode_status_report(
            GENERAL_CODE_FAILURE,
            u32::from(PROTOCOL_ID_SECURE_CHANNEL),
            SC_PROTOCOL_CODE_INVALID_PARAMETER,
        );
        let _ = ex
            .send_once(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_STATUS_REPORT, &sr)
            .await;
        return Err(PaseError::ConfirmMismatch);
    }
    let pake3 = encode_pake3(&shared.c_a);
    let resp3 = ex
        .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_PASE_PAKE3, &pake3, cfg)
        .await
        .map_err(PaseError::Exchange)?;
    let msg3 = match resp3 {
        Some(m) => m,
        None => ex.recv(RECV_TIMEOUT).await.map_err(PaseError::Exchange)?,
    };
    if msg3.proto.opcode != OPCODE_STATUS_REPORT {
        return Err(PaseError::Malformed("expected StatusReport after Pake3"));
    }
    let (general_code, _protocol_id, protocol_code) =
        parse_status_report(&msg3.payload).map_err(|_| PaseError::Malformed("status report"))?;
    if (general_code, protocol_code) != STATUS_SUCCESS {
        return Err(PaseError::StatusReport {
            general_code,
            protocol_code,
        });
    }

    // 6. Session keys: HKDF-SHA256(salt=[], ikm=Ke, info="SessionKeys") 48B
    // (spec §4.13.2.3) — note the ikm is Ke (16B, TT hash's second half),
    // not the full SPAKE2+ shared secret.
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&[]), &shared.k_e);
    let mut okm = [0u8; 48];
    hk.expand(INFO_SESSION_KEYS, &mut okm)
        .expect("valid length");
    let keys = SessionKeys {
        i2r: okm[..16].try_into().expect("16"),
        r2i: okm[16..32].try_into().expect("16"),
        attestation_challenge: okm[32..].try_into().expect("16"),
    };

    // 7. PASE sessions are unauthenticated: both sides use node id 0.
    Ok(SecureSession::new(
        transport,
        peer,
        local_session_id,
        resp.responder_session_id,
        keys,
        0,
        0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value};

    #[test]
    fn pbkdf_param_request_shape() {
        let req = encode_pbkdf_param_request(&[7u8; 32], 0x1234);
        let mut r = Reader::new(&req);
        // struct{1: rand[32], 2: session_id, 3: passcode_id=0, 4: has_params=false}
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(1));
        assert!(matches!(e.value, Value::Bytes(b) if b == [7u8; 32]));
        let e = r.next().unwrap().unwrap();
        assert!(matches!(e.value, Value::Uint(0x1234)));
        let e = r.next().unwrap().unwrap();
        assert!(matches!(e.value, Value::Uint(0)));
        let e = r.next().unwrap().unwrap();
        assert!(matches!(e.value, Value::Bool(false)));
    }

    #[test]
    fn parses_pbkdf_param_response() {
        // レスポンダの応答を Writer で合成して decode を検証
        let mut w = crate::tlv::Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[7u8; 32]);
        w.put_bytes(Tag::Context(2), &[8u8; 32]);
        w.put_uint(Tag::Context(3), 0xBEEF);
        w.start_struct(Tag::Context(4));
        w.put_uint(Tag::Context(1), 1000);
        w.put_bytes(Tag::Context(2), b"0123456789abcdef");
        w.end_container();
        w.end_container();
        let resp = decode_pbkdf_param_response(&w.finish()).unwrap();
        assert_eq!(resp.responder_session_id, 0xBEEF);
        assert_eq!(resp.iterations, 1000);
        assert_eq!(resp.salt, b"0123456789abcdef");
    }

    #[test]
    fn pake1_and_pake3_shape() {
        let p1 = encode_pake1(&[3u8; 65]);
        let mut r = Reader::new(&p1);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Bytes(b) if b.len() == 65));
        let p3 = encode_pake3(&[4u8; 32]);
        let mut r = Reader::new(&p3);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Bytes(b) if b == [4u8; 32]));
    }

    #[test]
    fn parses_pake2() {
        let mut w = crate::tlv::Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[9u8; 65]);
        w.put_bytes(Tag::Context(2), &[10u8; 32]);
        w.end_container();
        let (p_b, c_b) = decode_pake2(&w.finish()).unwrap();
        assert_eq!(p_b, [9u8; 65]);
        assert_eq!(c_b, [10u8; 32]);
    }

    /// Drives `establish` against a fake device over loopback UDP up through
    /// Pake2 with a deliberately wrong `cB`, so the initiator takes the
    /// `ConfirmMismatch` branch. Asserts the fake device then receives the
    /// abort StatusReport (FAILURE / SecureChannel / kInvalidParameter) on
    /// the same exchange, and that `establish` returns `ConfirmMismatch`.
    ///
    /// The fake device doesn't need real SPAKE2+ verifier math: since the
    /// test wants a *mismatch*, any valid P-256 point works for `pB` (it
    /// only needs to pass `Spake2pProver::finish`'s point-validity check)
    /// and `cB` can just be garbage bytes.
    #[tokio::test]
    async fn confirm_mismatch_sends_abort_status_report() {
        use crate::message::{Destination, MessageHeader, ProtocolHeader};
        use crate::transport::{UdpTransport, MAX_DATAGRAM};
        use p256::elliptic_curve::sec1::ToEncodedPoint;

        fn fast_cfg() -> MrpConfig {
            MrpConfig {
                initial_interval: Duration::from_millis(50),
                max_retries: 2,
                backoff: 1.0,
            }
        }

        /// A syntactically valid (on-curve, non-identity) P-256 point that is
        /// *not* the real SPAKE2+ shareV — good enough to reach the cB check.
        fn random_point() -> [u8; 65] {
            loop {
                let mut b = [0u8; 32];
                getrandom::getrandom(&mut b).expect("os rng");
                if let Ok(sk) = p256::SecretKey::from_slice(&b) {
                    return sk
                        .public_key()
                        .to_encoded_point(false)
                        .as_bytes()
                        .try_into()
                        .expect("uncompressed p256 point is 65 bytes");
                }
            }
        }

        fn build_unsecured(
            counter: u32,
            opcode: u8,
            exchange_id: u16,
            acked_counter: Option<u32>,
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
                needs_ack: false,
                acked_counter,
                opcode,
                exchange_id,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
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
                .expect("fake device timed out waiting for a datagram")
                .expect("recv_from io error");
            (buf[..n].to_vec(), from)
        }

        /// Decodes an unsecured (session id 0) datagram into its headers +
        /// payload, or `None` if malformed.
        fn decode_unsecured(buf: &[u8]) -> Option<(MessageHeader, ProtocolHeader, Vec<u8>)> {
            let (h, off) = MessageHeader::decode(buf).ok()?;
            if h.session_id != 0 {
                return None;
            }
            let (p, boff) = ProtocolHeader::decode(&buf[off..]).ok()?;
            Some((h, p, buf[off + boff..].to_vec()))
        }

        let responder_transport = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        let responder_addr = responder_transport.local_addr().unwrap();
        let initiator_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )));

        let cfg = fast_cfg();
        let establish_task = {
            let transport = Arc::clone(&initiator_transport);
            let cfg = cfg.clone();
            tokio::spawn(async move { establish(transport, responder_addr, 20202021, &cfg).await })
        };

        // --- PBKDFParamRequest -> PBKDFParamResponse ---
        let (req_buf, initiator_addr) = recv_dg(&responder_transport).await;
        let (req_header, req_proto, _req_payload) =
            decode_unsecured(&req_buf).expect("valid PBKDFParamRequest datagram");
        assert_eq!(req_proto.opcode, OPCODE_PBKDF_PARAM_REQUEST);

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[1u8; 32]); // initiatorRandom echo (ignored)
        w.put_bytes(Tag::Context(2), &[2u8; 32]); // responderRandom (ignored)
        w.put_uint(Tag::Context(3), 0xBEEF); // responderSessionId
        w.start_struct(Tag::Context(4));
        w.put_uint(Tag::Context(1), 1000); // iterations
        w.put_bytes(Tag::Context(2), b"0123456789abcdef"); // salt
        w.end_container();
        w.end_container();
        let resp_dg = build_unsecured(
            100,
            OPCODE_PBKDF_PARAM_RESPONSE,
            req_proto.exchange_id,
            Some(req_header.message_counter),
            &w.finish(),
        );
        responder_transport
            .send_to(&resp_dg, initiator_addr)
            .await
            .unwrap();

        // --- Pake1 -> Pake2 (deliberately wrong cB) ---
        let (pake1_buf, _) = recv_dg(&responder_transport).await;
        let (pake1_header, pake1_proto, _pake1_payload) =
            decode_unsecured(&pake1_buf).expect("valid Pake1 datagram");
        assert_eq!(pake1_proto.opcode, OPCODE_PASE_PAKE1);

        let p_b = random_point();
        let c_b = [0xEEu8; 32]; // deliberately wrong confirmation -> ConfirmMismatch
        let mut w2 = Writer::new();
        w2.start_struct(Tag::Anonymous);
        w2.put_bytes(Tag::Context(1), &p_b);
        w2.put_bytes(Tag::Context(2), &c_b);
        w2.end_container();
        let pake2_dg = build_unsecured(
            101,
            OPCODE_PASE_PAKE2,
            pake1_proto.exchange_id,
            Some(pake1_header.message_counter),
            &w2.finish(),
        );
        responder_transport
            .send_to(&pake2_dg, initiator_addr)
            .await
            .unwrap();

        // --- Expect the abort StatusReport on the same exchange ---
        let (abort_buf, _) = recv_dg(&responder_transport).await;
        let (_abort_header, abort_proto, abort_payload) =
            decode_unsecured(&abort_buf).expect("valid abort datagram");
        assert_eq!(abort_proto.opcode, OPCODE_STATUS_REPORT);
        assert_eq!(abort_proto.protocol_id, PROTOCOL_ID_SECURE_CHANNEL);
        assert_eq!(abort_proto.exchange_id, pake1_proto.exchange_id);
        let (general_code, protocol_id, protocol_code) =
            parse_status_report(&abort_payload).expect("well-formed StatusReport");
        assert_eq!(general_code, GENERAL_CODE_FAILURE);
        assert_eq!(protocol_id, u32::from(PROTOCOL_ID_SECURE_CHANNEL));
        assert_eq!(protocol_code, SC_PROTOCOL_CODE_INVALID_PARAMETER);

        let result = establish_task.await.expect("establish task panicked");
        assert!(matches!(result, Err(PaseError::ConfirmMismatch)));
    }
}
