//! groupcast の privacy 処理（spec §4.8.3 Message Privacy、§4.16.2 Privacy Key）。
//!
//! chip SDK（`Crypto::AES_CTR_crypt` / `Crypto::DeriveGroupPrivacyKey` /
//! `CryptoContext::BuildPrivacyNonce`、2026-09-05 に master で確認）と
//! バイト一致させるための事実:
//! - 難読化は AES-CTR だが、SDK は **AES-CCM 暗号化（AAD 無し）の出力から
//!   タグを捨てる**ことで CTR を得ている → counter block は CCM の
//!   `0x01 ‖ nonce(13) ‖ 0x0001` 起点。mat-controller の `encrypt_payload` で
//!   同じものが作れるので依存追加なし。
//! - Privacy Key = HKDF-SHA256(ikm = operational group key, salt = 空,
//!   info = "PrivacyKey", 16 バイト)。
//! - Privacy Nonce = session id（big-endian 2 バイト）‖ MIC[5..16]（11 バイト）。
//! - 難読化区間 = message header の offset 4（message counter 先頭）から
//!   header 末尾（destination まで）。message flags / session id / security
//!   flags（P ビット含む）は平文で、AAD と CCM nonce にはそのまま使う。
//! - Message Extensions（X フラグ）は非対応（`MessageHeader::decode` も読まない）。
use mat_controller::crypto::{encrypt_payload, MIC_LEN};
use mat_controller::message::MessageHeader;

/// security flags の P ビット（spec §4.4.1.4）。
pub const PRIVACY_FLAG: u8 = 0x80;
/// 難読化区間の先頭 = message flags(1) + session id(2) + security flags(1)。
pub const PRIVACY_HEADER_OFFSET: usize = 4;

/// Privacy Key = HKDF-SHA256(operational group key, salt = 空, "PrivacyKey")
/// — SDK `Crypto::DeriveGroupPrivacyKey`。
pub fn derive_privacy_key(operational_key: &[u8; 16]) -> [u8; 16] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, operational_key);
    let mut out = [0u8; 16];
    hk.expand(b"PrivacyKey", &mut out)
        .expect("16 bytes is a valid hkdf-sha256 output length");
    out
}

/// Privacy Nonce = session id（BE 2 バイト）‖ MIC[5..16] — SDK
/// `CryptoContext::BuildPrivacyNonce`（offset 5・長さ 11）。
pub fn privacy_nonce(session_id: u16, mic: &[u8; 16]) -> [u8; 13] {
    let mut n = [0u8; 13];
    n[..2].copy_from_slice(&session_id.to_be_bytes());
    n[2..].copy_from_slice(&mic[5..16]);
    n
}

/// AES-CTR（対称: 暗号化も復号も同じ）。SDK `AES_CTR_crypt` と同じく CCM
/// 暗号化（AAD 無し）の先頭 `data.len()` バイト = keystream XOR。
/// `encrypt_payload` の唯一の失敗はサイズ超過で、区間は最長 20 バイト
/// （counter 4 + source 8 + destination node 8）なので到達しない — 万一
/// `Err` なら `data` を変えずに戻る（復号に失敗した datagram は後段の
/// `open_message` が MIC 不一致で落とす）。
pub fn privacy_crypt(key: &[u8; 16], nonce: &[u8; 13], data: &mut [u8]) {
    if let Ok(ct) = encrypt_payload(key, nonce, &[], data) {
        data.copy_from_slice(&ct[..data.len()]);
    } else {
        debug_assert!(false, "privacy region exceeds the CCM payload limit");
    }
}

/// `datagram` の header 難読化区間 `[PRIVACY_HEADER_OFFSET, payload_off)` に
/// `operational_key` 由来の privacy keystream を当てる（対称なので難読化・
/// 復号の両方）。header 長は message flags だけで決まるので、値が難読化
/// されていても `MessageHeader::decode` の返す offset は正しい。短すぎて
/// header + MIC が入らなければ `false`（何もしない）。
fn crypt_header_in_place(datagram: &mut [u8], operational_key: &[u8; 16]) -> bool {
    let Ok((_, payload_off)) = MessageHeader::decode(datagram) else {
        return false;
    };
    if datagram.len() < payload_off + MIC_LEN || payload_off <= PRIVACY_HEADER_OFFSET {
        return false;
    }
    let session_id = u16::from_le_bytes([datagram[1], datagram[2]]);
    let mut mic = [0u8; MIC_LEN];
    mic.copy_from_slice(&datagram[datagram.len() - MIC_LEN..]);
    let key = derive_privacy_key(operational_key);
    let nonce = privacy_nonce(session_id, &mic);
    privacy_crypt(
        &key,
        &nonce,
        &mut datagram[PRIVACY_HEADER_OFFSET..payload_off],
    );
    true
}

/// 受信側: 難読化された header を復号したコピーを返す（`None` = 短すぎ）。
/// security flags の P ビットは**落とさない** — 後段の `open_message` は
/// wire の security flags を AAD / nonce に使う（SDK も同じ）。
pub fn deobfuscate_header(datagram: &[u8], operational_key: &[u8; 16]) -> Option<Vec<u8>> {
    let mut copy = datagram.to_vec();
    crypt_header_in_place(&mut copy, operational_key).then_some(copy)
}

/// 送信側（テスト・将来の matv 送出用）: `seal_message` 済みの datagram の
/// header を in-place で難読化する。呼び出し側が security flags に
/// `PRIVACY_FLAG` を立てて封じておくこと（AAD に含まれるため後から
/// 立てられない）。
pub fn obfuscate_header(datagram: &mut [u8], operational_key: &[u8; 16]) -> bool {
    crypt_header_in_place(datagram, operational_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::message::{Destination, ProtocolHeader};

    const OP: [u8; 16] = [0x5A; 16];

    #[test]
    fn privacy_key_is_deterministic_and_differs_from_the_operational_key() {
        let a = derive_privacy_key(&OP);
        let b = derive_privacy_key(&OP);
        assert_eq!(a, b);
        assert_ne!(a, OP);
        assert_ne!(derive_privacy_key(&[0x5B; 16]), a);
    }

    #[test]
    fn privacy_nonce_is_big_endian_session_id_then_mic_tail() {
        let mic: [u8; 16] = core::array::from_fn(|i| i as u8);
        let n = privacy_nonce(0x1234, &mic);
        assert_eq!(n[0..2], [0x12, 0x34]);
        assert_eq!(n[2..], mic[5..16]);
    }

    #[test]
    fn privacy_crypt_is_an_involution_and_key_dependent() {
        let key = derive_privacy_key(&OP);
        let nonce = privacy_nonce(0xBEEF, &[0xCC; 16]);
        let plain: Vec<u8> = (0..14u8).collect(); // counter(4)+source(8)+group(2)
        let mut buf = plain.clone();
        privacy_crypt(&key, &nonce, &mut buf);
        assert_ne!(buf, plain);
        privacy_crypt(&key, &nonce, &mut buf);
        assert_eq!(buf, plain);
        let mut other = plain.clone();
        privacy_crypt(&derive_privacy_key(&[1u8; 16]), &nonce, &mut other);
        privacy_crypt(&key, &nonce, &mut other);
        assert_ne!(other, plain);
    }

    /// CCM keystream ピン: `encrypt_payload(key, nonce, aad=[], data)` の先頭
    /// `len` バイトと一致する（= SDK `AES_CTR_crypt` の定義そのもの）。
    #[test]
    fn privacy_crypt_equals_ccm_ciphertext_without_the_tag() {
        let key = [3u8; 16];
        let nonce = [4u8; 13];
        let data = [9u8; 20];
        let mut buf = data;
        privacy_crypt(&key, &nonce, &mut buf);
        let ccm = encrypt_payload(&key, &nonce, &[], &data).unwrap();
        assert_eq!(buf[..], ccm[..20]);
        assert_eq!(ccm.len(), 20 + MIC_LEN);
    }

    fn group_datagram_bytes(security_flags: u8) -> Vec<u8> {
        let header = MessageHeader {
            session_id: 0x0102,
            security_flags,
            message_counter: 0x11223344,
            source_node_id: Some(0x0A0B0C0D0E0F1011),
            destination: Destination::Group(0x000A),
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: false,
            acked_counter: None,
            opcode: 0x08,
            exchange_id: 1,
            protocol_id: 1,
            vendor_id: None,
        };
        mat_controller::crypto::seal_message(&OP, &header, &proto, &[1, 2, 3], 0).unwrap()
    }

    #[test]
    fn obfuscate_then_deobfuscate_restores_the_header_and_touches_nothing_else() {
        let plain = group_datagram_bytes(0x01 | PRIVACY_FLAG);
        let mut wire = plain.clone();
        assert!(obfuscate_header(&mut wire, &OP));
        // 区間 [4, 18) だけ変わり、flags/session/secflags と payload/MIC は不変
        assert_eq!(wire[..4], plain[..4]);
        assert_ne!(wire[4..18], plain[4..18]);
        assert_eq!(wire[18..], plain[18..]);
        let back = deobfuscate_header(&wire, &OP).unwrap();
        assert_eq!(back, plain);
        // 復号後は open_message がそのまま通る（AAD/nonce に P ビットが残る）
        let (h, _, body) = mat_controller::crypto::open_message(&OP, &back, 0).unwrap();
        assert_eq!(h.source_node_id, Some(0x0A0B0C0D0E0F1011));
        assert_eq!(body, vec![1, 2, 3]);
    }

    #[test]
    fn deobfuscate_rejects_datagrams_too_short_for_header_plus_mic() {
        let plain = group_datagram_bytes(0x81);
        assert!(deobfuscate_header(&plain[..18 + MIC_LEN - 1], &OP).is_none());
        assert!(deobfuscate_header(&[0u8; 3], &OP).is_none());
        assert!(!obfuscate_header(&mut [0u8; 3], &OP));
    }
}
