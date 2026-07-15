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
        // 事前チェック後は到達不能（ccm 0.5 の唯一の失敗はサイズ超過）。保険として残す。
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
        let pub_bytes: [u8; 65] = vk.to_encoded_point(false).as_bytes().try_into().unwrap();
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
}
