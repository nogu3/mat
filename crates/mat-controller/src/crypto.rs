//! AES-128-CCM session crypto and nonce construction (spec §4.7).

use aes::Aes128;
use ccm::aead::{Aead, KeyInit, Payload};
use ccm::consts::{U13, U16};
use ccm::Ccm;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};

use crate::message::{MessageError, MessageHeader, ProtocolHeader};

type Aes128Ccm = Ccm<Aes128, U16, U13>;

/// MIC (auth tag) length for Matter secured messages.
pub const MIC_LEN: usize = 16;

/// CCM with a 13-byte nonce (L = 2) caps a single payload at 2^16 - 1 bytes.
const MAX_CCM_PAYLOAD: usize = 65535;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    AuthFailed,
    PayloadTooLarge,
    BadKey,
    BadSignature,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::AuthFailed => write!(f, "message authentication failed"),
            CryptoError::PayloadTooLarge => write!(
                f,
                "payload exceeds AES-CCM limit of {MAX_CCM_PAYLOAD} bytes"
            ),
            CryptoError::BadKey => write!(f, "invalid ec key"),
            CryptoError::BadSignature => write!(f, "ecdsa signature verification failed"),
        }
    }
}

impl std::error::Error for CryptoError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    Message(MessageError),
    Crypto(CryptoError),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Message(e) => e.fmt(f),
            OpenError::Crypto(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<MessageError> for OpenError {
    fn from(e: MessageError) -> Self {
        OpenError::Message(e)
    }
}

impl From<CryptoError> for OpenError {
    fn from(e: CryptoError) -> Self {
        OpenError::Crypto(e)
    }
}

/// ECDSA-P256 sign over SHA-256(message) (p256 default). Returns raw r||s (64B).
pub fn sign_ecdsa_p256(private_key: &[u8; 32], message: &[u8]) -> Result<[u8; 64], CryptoError> {
    let key = SigningKey::from_slice(private_key).map_err(|_| CryptoError::BadKey)?;
    let sig: Signature = key.sign(message);
    Ok(sig.to_bytes().into())
}

/// Verify a raw r||s (64B) ECDSA-P256 signature over SHA-256(message).
pub fn verify_ecdsa_p256(
    public_key: &[u8; 65],
    message: &[u8],
    signature: &[u8; 64],
) -> Result<(), CryptoError> {
    let key = VerifyingKey::from_sec1_bytes(public_key).map_err(|_| CryptoError::BadKey)?;
    let sig = Signature::from_slice(signature).map_err(|_| CryptoError::BadSignature)?;
    key.verify(message, &sig)
        .map_err(|_| CryptoError::BadSignature)
}

/// Nonce = security flags (1B) || message counter (4B LE) || source node id (8B LE).
pub fn build_nonce(security_flags: u8, message_counter: u32, source_node_id: u64) -> [u8; 13] {
    let mut n = [0u8; 13];
    n[0] = security_flags;
    n[1..5].copy_from_slice(&message_counter.to_le_bytes());
    n[5..13].copy_from_slice(&source_node_id.to_le_bytes());
    n
}

pub fn encrypt_payload(
    key: &[u8; 16],
    nonce: &[u8; 13],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if plaintext.len() > MAX_CCM_PAYLOAD {
        return Err(CryptoError::PayloadTooLarge);
    }
    Aes128Ccm::new(key.into())
        .encrypt(
            nonce.into(),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        // 事前チェック後は到達不能（ccm 0.6 の唯一の失敗はサイズ超過）。保険として残す。
        .map_err(|_| CryptoError::PayloadTooLarge)
}

pub fn decrypt_payload(
    key: &[u8; 16],
    nonce: &[u8; 13],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if ciphertext.len() > MAX_CCM_PAYLOAD + MIC_LEN {
        return Err(CryptoError::PayloadTooLarge);
    }
    Aes128Ccm::new(key.into())
        .decrypt(
            nonce.into(),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| CryptoError::AuthFailed)
}

/// Builds a complete secured datagram: plain header || CCM(protocol header || payload).
/// The `session_source_node_id` is the sender's node id, used in the nonce only when
/// the header carries no source node id (header wins).
pub fn seal_message(
    key: &[u8; 16],
    header: &MessageHeader,
    proto: &ProtocolHeader,
    payload: &[u8],
    session_source_node_id: u64,
) -> Result<Vec<u8>, CryptoError> {
    let header_bytes = header.encoded();
    let nonce_node = header.source_node_id.unwrap_or(session_source_node_id);
    let nonce = build_nonce(header.security_flags, header.message_counter, nonce_node);
    let mut plaintext = Vec::with_capacity(payload.len() + 12);
    proto.encode(&mut plaintext);
    plaintext.extend_from_slice(payload);
    let ct = encrypt_payload(key, &nonce, &header_bytes, &plaintext)?;
    let mut out = header_bytes;
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Opens a secured datagram; returns headers and the decrypted app payload.
/// The `session_source_node_id` is the peer's (sender's) node id, used in the nonce only when
/// the header carries no source node id (header wins).
pub fn open_message(
    key: &[u8; 16],
    datagram: &[u8],
    session_source_node_id: u64,
) -> Result<(MessageHeader, ProtocolHeader, Vec<u8>), OpenError> {
    let (header, payload_off) = MessageHeader::decode(datagram)?;
    let nonce_node = header.source_node_id.unwrap_or(session_source_node_id);
    let nonce = build_nonce(header.security_flags, header.message_counter, nonce_node);
    let aad = &datagram[..payload_off];
    let plaintext = decrypt_payload(key, &nonce, aad, &datagram[payload_off..])?;
    let (proto, body_off) = ProtocolHeader::decode(&plaintext)?;
    Ok((header, proto, plaintext[body_off..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Destination, MessageHeader, ProtocolHeader};

    const KEY: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
        0x0F,
    ];

    #[test]
    fn builds_nonce_layout() {
        let n = build_nonce(0x00, 0x1122_3344, 0x8877_6655_4433_2211);
        assert_eq!(
            n,
            [0x00, 0x44, 0x33, 0x22, 0x11, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
    }

    #[test]
    fn roundtrips_payload() {
        let nonce = build_nonce(0, 1, 42);
        let aad = b"header-bytes";
        let ct = encrypt_payload(&KEY, &nonce, aad, b"hello matter").unwrap();
        assert_eq!(ct.len(), b"hello matter".len() + MIC_LEN);
        let pt = decrypt_payload(&KEY, &nonce, aad, &ct).unwrap();
        assert_eq!(pt, b"hello matter");
    }

    #[test]
    fn rejects_tampered_ciphertext_and_aad() {
        let nonce = build_nonce(0, 1, 42);
        let mut ct = encrypt_payload(&KEY, &nonce, b"aad", b"payload").unwrap();
        ct[0] ^= 0x01;
        assert!(decrypt_payload(&KEY, &nonce, b"aad", &ct).is_err());
        let ct = encrypt_payload(&KEY, &nonce, b"aad", b"payload").unwrap();
        assert!(decrypt_payload(&KEY, &nonce, b"AAD", &ct).is_err());
    }

    #[test]
    fn seals_and_opens_message() {
        let header = MessageHeader {
            session_id: 0x0BB8,
            security_flags: 0,
            message_counter: 0x0100_0001,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: true,
            acked_counter: None,
            opcode: 0x08,
            exchange_id: 0x1234,
            protocol_id: crate::message::PROTOCOL_ID_INTERACTION_MODEL,
            vendor_id: None,
        };
        let datagram = seal_message(&KEY, &header, &proto, b"im-payload", 0xAAAA).unwrap();
        // ヘッダ 8B は平文のまま先頭に載る
        assert_eq!(&datagram[..8], header.encoded().as_slice());
        let (h2, p2, body) = open_message(&KEY, &datagram, 0xAAAA).unwrap();
        assert_eq!(h2, header);
        assert_eq!(p2, proto);
        assert_eq!(body, b"im-payload");
        // nonce の node id が違えば開かない
        assert!(open_message(&KEY, &datagram, 0xBBBB).is_err());
    }

    #[test]
    fn rejects_oversized_payload() {
        let nonce = build_nonce(0, 1, 42);
        let big = vec![0u8; 65536];
        assert_eq!(
            encrypt_payload(&KEY, &nonce, b"", &big),
            Err(CryptoError::PayloadTooLarge)
        );
    }

    #[test]
    fn nonce_prefers_header_source_node_id() {
        let header = MessageHeader {
            session_id: 1,
            security_flags: 0,
            message_counter: 42,
            source_node_id: Some(0x1111),
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: false,
            acked_counter: None,
            opcode: 0x01,
            exchange_id: 2,
            protocol_id: crate::message::PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        };
        // seal 側の session 引数と食い違っていても、ヘッダの source node id が
        // nonce に使われるため、open 側も（別の session 引数で）開ける。
        let datagram = seal_message(&KEY, &header, &proto, b"payload", 0x2222).unwrap();
        let (h2, p2, body) = open_message(&KEY, &datagram, 0x3333).unwrap();
        assert_eq!(h2, header);
        assert_eq!(p2, proto);
        assert_eq!(body, b"payload");
    }

    #[test]
    fn ecdsa_sign_verify_roundtrip() {
        // 既知の p256 テスト鍵（RustCrypto でその場生成）
        use p256::ecdsa::SigningKey;

        let sk = SigningKey::from_slice(&[0x11u8; 32]).unwrap();
        let priv_bytes: [u8; 32] = sk.to_bytes().into();
        let vk = sk.verifying_key();
        let pub_bytes: [u8; 65] = vk.to_sec1_point(false).as_bytes().try_into().unwrap();
        let msg = b"attestation over TBS bytes";
        let sig = sign_ecdsa_p256(&priv_bytes, msg).unwrap();
        verify_ecdsa_p256(&pub_bytes, msg, &sig).unwrap();
        // 改ざんメッセージは失敗
        assert!(verify_ecdsa_p256(&pub_bytes, b"other", &sig).is_err());
        // 不正鍵は BadKey
        assert!(matches!(
            verify_ecdsa_p256(&[0u8; 65], msg, &sig),
            Err(CryptoError::BadKey)
        ));
    }

    /// RustCrypto 依存を上げても暗号文が 1 バイトも変わらないことを固定する
    /// ゴールデン（2026-09-09、aes 0.8 / ccm 0.5 で採取、新系列でも同値）。
    #[test]
    fn golden_ccm_ciphertext_is_stable() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let nonce: [u8; 13] = core::array::from_fn(|i| 0xA0 + i as u8);
        let ct = encrypt_payload(
            &key,
            &nonce,
            b"matter-aad",
            b"the quick brown fox jumps over the lazy dog",
        )
        .unwrap();
        const EXPECTED: [u8; 43 + 16] = [
            0x2d, 0xc5, 0x25, 0xf4, 0x06, 0xdb, 0x75, 0x83, 0x2f, 0xb5, 0xf7, 0x0a, 0xdb, 0xce,
            0x7c, 0xcc, 0x52, 0xe5, 0xf4, 0xf4, 0xe1, 0x9b, 0xb0, 0xe2, 0x66, 0xa9, 0xc0, 0x22,
            0xea, 0xe2, 0xab, 0xa9, 0xa3, 0xf1, 0xfb, 0x53, 0x37, 0xd6, 0xa2, 0x64, 0x1f, 0x8c,
            0x01, 0xbf, 0xc6, 0x73, 0x0c, 0xf7, 0xcc, 0x5e, 0x1d, 0x68, 0x18, 0x9a, 0x1e, 0x86,
            0xdf, 0x14, 0x9f,
        ];
        assert_eq!(ct, EXPECTED);
    }

    /// ECDSA は RFC 6979 決定的署名なので依存を上げても同じ r||s になる
    /// （2026-09-09、p256 0.13 で採取、新系列でも同値）。
    #[test]
    fn golden_ecdsa_signature_is_stable() {
        let sk: [u8; 32] = core::array::from_fn(|i| 0x11 + i as u8);
        let sig = sign_ecdsa_p256(&sk, b"tbs-message").unwrap();
        const EXPECTED: [u8; 64] = [
            0x22, 0x7a, 0xb5, 0xd5, 0x00, 0x7a, 0xcc, 0x93, 0x0b, 0xcb, 0xa6, 0x05, 0x60, 0xb4,
            0xc2, 0xd3, 0xee, 0x6b, 0xae, 0x62, 0x5c, 0xdb, 0xf2, 0x7c, 0x37, 0xda, 0x27, 0x78,
            0x60, 0x98, 0xc9, 0x1f, 0xb8, 0x55, 0x8d, 0x6b, 0x0c, 0x5d, 0x7b, 0xe3, 0xb9, 0x6b,
            0xb9, 0xf7, 0x5a, 0x03, 0x17, 0x2a, 0x71, 0x43, 0xa7, 0xe9, 0x06, 0x8a, 0xd6, 0x12,
            0xff, 0x13, 0x0d, 0xda, 0xa5, 0x83, 0xd1, 0x29,
        ];
        assert_eq!(sig, EXPECTED);
    }
}
