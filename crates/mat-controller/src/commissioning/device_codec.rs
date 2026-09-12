//! デバイス側 codec（mat-device の CommissioningServer が使う）。
//!
//! --- device-side（Task 9）: リクエスト decoder / レスポンス encoder ---
//!
//! 上のブロックの逆方向: mat-device のコミッショニングサーバ
//! (`mat_device::core::commissioning::CommissioningServer`) が、受け取った
//! `*Request` の CommandFields をここで decode し、`*Response` をここで
//! encode してコミッショナー側へ返す。TLV タグ割り当ては対応する上の
//! encoder/decoder と完全に同じ形（同じコマンドの表と裏）。

use crate::tlv::{Tag, Writer};

use super::tlv_fields::{required, scan_struct_fields, take_bytes, take_u64, take_uint, take_utf8};
use super::CommissionError;

/// `fields` から struct 直下の 32 バイト nonce（タグ `tag`）を取り出す。
/// AttestationRequest/CSRRequest はどちらも `{0: nonce(32)}` という同じ形
/// なので実装を共有する。
fn decode_nonce32(fields: &[u8], tag: u8, step: &'static str) -> Result<[u8; 32], CommissionError> {
    let mut map = scan_struct_fields(fields, step)?;
    let bytes = required(take_bytes(&mut map, tag), step, "missing nonce")?;
    bytes.try_into().map_err(|_| CommissionError::Malformed {
        step,
        detail: "nonce length",
    })
}

/// ArmFailSafeRequest（spec §11.10.6.2）: `{0: ExpiryLengthSeconds, 1:
/// Breadcrumb}`。デバイス側 decoder（逆方向は [`super::encode_arm_fail_safe`]）。
/// 戻り値は `(expiry_seconds, breadcrumb)`——`Breadcrumb` は spec上 optional
/// なので欠落時は 0。
pub fn decode_arm_fail_safe(fields: &[u8]) -> Result<(u16, u64), CommissionError> {
    let step = "arm_fail_safe_request";
    let mut map = scan_struct_fields(fields, step)?;
    let expiry = required(
        take_uint::<u16>(&mut map, 0, step, "expiry out of range")?,
        step,
        "missing expiry",
    )?;
    let breadcrumb = take_u64(&mut map, 1).unwrap_or(0);
    Ok((expiry, breadcrumb))
}

/// SetRegulatoryConfigRequest（spec §11.10.6.4）: `{0: NewRegulatoryConfig,
/// 1: CountryCode, 2: Breadcrumb}`。デバイス側 decoder（逆方向は
/// [`super::encode_set_regulatory_config`]）。戻り値は `(config, country,
/// breadcrumb)`。
pub fn decode_set_regulatory_config(fields: &[u8]) -> Result<(u8, String, u64), CommissionError> {
    let step = "set_regulatory_config_request";
    let mut map = scan_struct_fields(fields, step)?;
    let config = required(
        take_uint::<u8>(&mut map, 0, step, "config out of range")?,
        step,
        "missing config",
    )?;
    let country = required(take_utf8(&mut map, 1), step, "missing country")?;
    let breadcrumb = take_u64(&mut map, 2).unwrap_or(0);
    Ok((config, country, breadcrumb))
}

/// AttestationRequest（spec §11.17.6.7）: `{0: AttestationNonce}`。デバイス
/// 側 decoder（逆方向は [`super::encode_attestation_request`]）。
pub fn decode_attestation_request(fields: &[u8]) -> Result<[u8; 32], CommissionError> {
    decode_nonce32(fields, 0, "attestation_request")
}

/// CertificateChainRequest（spec §11.17.6.4）: `{0: CertificateType}`
/// （`CERT_TYPE_DAC`/`CERT_TYPE_PAI`）。デバイス側 decoder（逆方向は
/// [`super::encode_cert_chain_request`]）。
pub fn decode_cert_chain_request(fields: &[u8]) -> Result<u8, CommissionError> {
    let step = "cert_chain_request";
    let mut map = scan_struct_fields(fields, step)?;
    required(
        take_uint::<u8>(&mut map, 0, step, "cert type out of range")?,
        step,
        "missing cert type",
    )
}

/// CSRRequest（spec §11.17.6.9）: `{0: CSRNonce}`。デバイス側 decoder（逆
/// 方向は [`super::encode_csr_request`]）。
pub fn decode_csr_request(fields: &[u8]) -> Result<[u8; 32], CommissionError> {
    decode_nonce32(fields, 0, "csr_request")
}

/// AddTrustedRootCertificate（spec §11.17.6.11）: `{0: RootCACertificate}`。
/// デバイス側 decoder（逆方向は [`super::encode_add_trusted_root`]）。
pub fn decode_add_trusted_root(fields: &[u8]) -> Result<Vec<u8>, CommissionError> {
    let step = "add_trusted_root_request";
    let mut map = scan_struct_fields(fields, step)?;
    required(take_bytes(&mut map, 0), step, "missing rcac")
}

/// [`decode_add_noc`]'s decoded fields: `(noc_tlv, icac_tlv, ipk_epoch,
/// case_admin_subject, admin_vendor_id)`.
pub type AddNocFields = (Vec<u8>, Option<Vec<u8>>, [u8; 16], u64, u16);

/// AddNOC（spec §11.17.6.13）: `{0: NOCValue, 1: ICACValue(optional), 2:
/// IPKValue, 3: CaseAdminSubject, 4: AdminVendorId}`。デバイス側 decoder
/// （逆方向は [`super::encode_add_noc`] — あちらは tag1（ICACValue）を意図的に
/// 省略するが、この decoder は spec どおり optional として受理する）。戻り
/// 値は [`AddNocFields`]。
pub fn decode_add_noc(fields: &[u8]) -> Result<AddNocFields, CommissionError> {
    let step = "add_noc_request";
    let mut map = scan_struct_fields(fields, step)?;
    let noc = required(take_bytes(&mut map, 0), step, "missing noc")?;
    let icac = take_bytes(&mut map, 1);
    let ipk_bytes = required(take_bytes(&mut map, 2), step, "missing ipk")?;
    let ipk: [u8; 16] = ipk_bytes
        .try_into()
        .map_err(|_| CommissionError::Malformed {
            step,
            detail: "ipk length",
        })?;
    let case_admin_subject = required(take_u64(&mut map, 3), step, "missing case admin subject")?;
    let admin_vendor_id = required(
        take_uint::<u16>(&mut map, 4, step, "admin vendor id out of range")?,
        step,
        "missing admin vendor id",
    )?;
    Ok((noc, icac, ipk, case_admin_subject, admin_vendor_id))
}

/// UpdateFabricLabel（spec §11.17.6.11）: `{0: Label}`。デバイス側 decoder
/// （逆方向は [`super::encode_update_fabric_label`]）。`Label` は spec 上最大 32
/// 文字（§11.17.5.20 `FabricDescriptorStruct` の `Label` フィールドと同じ
/// 制約）——超過は `CommissionError::Malformed` にする。呼び出し元
/// （`mat_device::core::commissioning`）は他の decode エラーと同じく
/// `let Ok(..) = decode_update_fabric_label(..) else { .. STATUS_INVALID_
/// COMMAND }` の形で INVALID_COMMAND にマップする。
pub fn decode_update_fabric_label(fields: &[u8]) -> Result<String, CommissionError> {
    let step = "update_fabric_label_request";
    let mut map = scan_struct_fields(fields, step)?;
    let label = required(take_utf8(&mut map, 0), step, "missing label")?;
    if label.len() > 32 {
        return Err(CommissionError::Malformed {
            step,
            detail: "label too long",
        });
    }
    Ok(label)
}

/// RemoveFabric（spec §11.17.6.15）: `{0: FabricIndex}`。デバイス側 decoder
/// （逆方向は [`super::encode_remove_fabric`]）。`UpdateFabricLabel` と異なり
/// `FabricIndex` を明示するのは、削除対象が呼び出しセッション自身の
/// fabric とは限らないから（Android がハンドオフ後に自分の一時 fabric
/// を名指しで消すのが典型ケース）。
pub fn decode_remove_fabric(fields: &[u8]) -> Result<u8, CommissionError> {
    let step = "remove_fabric_request";
    let mut map = scan_struct_fields(fields, step)?;
    required(
        take_uint::<u8>(&mut map, 0, step, "fabric index out of range")?,
        step,
        "missing fabric index",
    )
}

/// [`decode_open_commissioning_window`]'s decoded fields: `(timeout_s,
/// verifier, discriminator, iterations, salt)`.
pub type OpenCommissioningWindowFields = (u16, Vec<u8>, u16, u32, Vec<u8>);

/// OpenCommissioningWindow（spec §11.19.8.1）: `{0: CommissioningTimeout, 1:
/// PAKEPasscodeVerifier, 2: Discriminator, 3: Iterations, 4: Salt}`。デバイ
/// ス側 decoder（逆方向は [`super::encode_open_commissioning_window`]）。戻り値は
/// [`OpenCommissioningWindowFields`] — 範囲検証（verifier 長 97 /
/// iterations 1000..=100000 / salt 長 16..=32 / timeout 180..=900）はここ
/// では行わない（デバイス側ハンドラが Busy/PAKEParameterError/
/// INVALID_COMMAND を出し分けるため）。
pub fn decode_open_commissioning_window(
    fields: &[u8],
) -> Result<OpenCommissioningWindowFields, CommissionError> {
    let step = "open_commissioning_window_request";
    let mut map = scan_struct_fields(fields, step)?;
    let timeout_s = required(
        take_uint::<u16>(&mut map, 0, step, "timeout out of range")?,
        step,
        "missing timeout",
    )?;
    let verifier = required(take_bytes(&mut map, 1), step, "missing verifier")?;
    let discriminator = required(
        take_uint::<u16>(&mut map, 2, step, "discriminator out of range")?,
        step,
        "missing discriminator",
    )?;
    let iterations = required(
        take_uint::<u32>(&mut map, 3, step, "iterations out of range")?,
        step,
        "missing iterations",
    )?;
    let salt = required(take_bytes(&mut map, 4), step, "missing salt")?;
    Ok((timeout_s, verifier, discriminator, iterations, salt))
}

/// `*CommissioningResponse`（ArmFailSafeResponse / SetRegulatoryConfig
/// Response / CommissioningCompleteResponse 共通、spec §11.10.6.3 / .5 /
/// .7）: `{0: ErrorCode, 1: DebugText}`。デバイス側 encoder（逆方向は
/// [`super::decode_commissioning_status_response`]）。
///
/// `DebugText` は spec 上 **mandatory**（空でもタグを省略できない）。
/// 我々の decoder は欠落を空文字列で埋める寛容な実装なので自己往復では
/// 差が出ないが、chip は ArmFailSafeResponse を読む時点で次の必須フィールド
/// を探しに行き `CHIP Error 0x00000021: End of TLV` で commissioning ごと
/// 落とす（M2 ゲート 1 の実測 —
/// `docs/superpowers/plans/m2-chip-tool-probe.md`）。空文字列でも必ず書く。
pub fn encode_commissioning_status_response(error_code: u8, debug_text: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(error_code));
    w.put_str(Tag::Context(1), debug_text);
    w.end_container();
    w.finish()
}

/// AttestationResponse（spec §11.17.6.8）: `{0: AttestationElements, 1:
/// AttestationSignature}`。デバイス側 encoder（逆方向は
/// [`super::decode_attestation_response`]）。
pub fn encode_attestation_response(elements: &[u8], signature: &[u8; 64]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), elements);
    w.put_bytes(Tag::Context(1), signature);
    w.end_container();
    w.finish()
}

/// CertificateChainResponse（spec §11.17.6.5）: `{0: Certificate}`。デバイ
/// ス側 encoder（逆方向は [`super::decode_cert_chain_response`]）。
pub fn encode_cert_chain_response(cert_der: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), cert_der);
    w.end_container();
    w.finish()
}

/// NOCSRElements（spec §11.17.6.10.1）: `{1: csr, 2: CSRNonce}`。デバイス側
/// encoder（逆方向は [`super::parse_nocsr_elements`]）——vendor reserved フィール
/// ド（tag3/4）は出さない。
pub fn encode_nocsr_elements(csr_der: &[u8], nonce: &[u8; 32]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(1), csr_der);
    w.put_bytes(Tag::Context(2), nonce);
    w.end_container();
    w.finish()
}

/// CSRResponse（spec §11.17.6.10）: `{0: NOCSRElements, 1:
/// AttestationSignature}`。デバイス側 encoder（逆方向は
/// [`super::decode_csr_response`]）。`nocsr_elements` は [`encode_nocsr_elements`]
/// の出力（生 TLV バイト列）をそのまま渡す。
pub fn encode_csr_response(nocsr_elements: &[u8], signature: &[u8; 64]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), nocsr_elements);
    w.put_bytes(Tag::Context(1), signature);
    w.end_container();
    w.finish()
}

/// NOCResponse（spec §11.17.6.14, AddNOC / RemoveFabric 共通の応答）:
/// `{0: StatusCode, 1: FabricIndex(optional), 2: DebugText(optional)}`。
/// デバイス側 encoder（逆方向は [`super::decode_noc_response`]）。
pub fn encode_noc_response(status: u8, fabric_index: Option<u8>) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(status));
    if let Some(idx) = fabric_index {
        w.put_uint(Tag::Context(1), u64::from(idx));
    }
    w.end_container();
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::super::{
        decode_attestation_response, decode_cert_chain_response,
        decode_commissioning_status_response, decode_csr_response, decode_noc_response,
        encode_add_noc, encode_add_trusted_root, encode_arm_fail_safe, encode_attestation_request,
        encode_cert_chain_request, encode_csr_request, encode_open_commissioning_window,
        encode_remove_fabric, encode_set_regulatory_config, encode_update_fabric_label,
        parse_nocsr_elements, CERT_TYPE_DAC, CERT_TYPE_PAI,
    };
    use super::*;
    use crate::tlv::{Tag, Writer};

    // --- Task 9: デバイス側 decoder / encoder ↔ 既存の逆方向のラウンドトリップ ---

    #[test]
    fn decode_arm_fail_safe_roundtrips_with_encoder() {
        let (expiry, breadcrumb) = decode_arm_fail_safe(&encode_arm_fail_safe(120, 1)).unwrap();
        assert_eq!((expiry, breadcrumb), (120, 1));
    }

    #[test]
    fn decode_set_regulatory_config_roundtrips_with_encoder() {
        let (config, country, breadcrumb) =
            decode_set_regulatory_config(&encode_set_regulatory_config(2, "XX", 2)).unwrap();
        assert_eq!((config, country.as_str(), breadcrumb), (2, "XX", 2));
    }

    #[test]
    fn decode_attestation_request_roundtrips_with_encoder() {
        let nonce = [5u8; 32];
        assert_eq!(
            decode_attestation_request(&encode_attestation_request(&nonce)).unwrap(),
            nonce
        );
    }

    #[test]
    fn decode_cert_chain_request_roundtrips_with_encoder() {
        assert_eq!(
            decode_cert_chain_request(&encode_cert_chain_request(CERT_TYPE_DAC)).unwrap(),
            CERT_TYPE_DAC
        );
        assert_eq!(
            decode_cert_chain_request(&encode_cert_chain_request(CERT_TYPE_PAI)).unwrap(),
            CERT_TYPE_PAI
        );
    }

    #[test]
    fn decode_csr_request_roundtrips_with_encoder() {
        let nonce = [6u8; 32];
        assert_eq!(
            decode_csr_request(&encode_csr_request(&nonce)).unwrap(),
            nonce
        );
    }

    #[test]
    fn decode_add_trusted_root_roundtrips_with_encoder() {
        assert_eq!(
            decode_add_trusted_root(&encode_add_trusted_root(b"rcac-tlv")).unwrap(),
            b"rcac-tlv"
        );
    }

    #[test]
    fn decode_add_noc_roundtrips_with_encoder() {
        let (noc, icac, ipk, subj, vendor) =
            decode_add_noc(&encode_add_noc(b"noc-tlv", &[9u8; 16], 0x1_0001, 0xFFF1)).unwrap();
        assert_eq!(noc, b"noc-tlv");
        assert_eq!(icac, None); // encode_add_noc は tag1 を意図的に省略する
        assert_eq!(ipk, [9u8; 16]);
        assert_eq!(subj, 0x1_0001);
        assert_eq!(vendor, 0xFFF1);
    }

    #[test]
    fn encode_commissioning_status_response_roundtrips_with_decoder() {
        let (code, text) =
            decode_commissioning_status_response(&encode_commissioning_status_response(0, "ok"))
                .unwrap();
        assert_eq!((code, text.as_str()), (0, "ok"));
        let (code2, text2) =
            decode_commissioning_status_response(&encode_commissioning_status_response(3, ""))
                .unwrap();
        assert_eq!((code2, text2.as_str()), (3, ""));
    }

    /// DebugText は spec §11.10.6.3/.5/.7 で **mandatory** — 空文字列でも
    /// タグ 1 を省略してはいけない。我々の decoder は欠落を許すので
    /// roundtrip テストだけでは検出できず、chip 側だけが
    /// `CHIP Error 0x00000021: End of TLV` で落ちた（M2 ゲート 1 の実測。
    /// `docs/superpowers/plans/m2-chip-tool-probe.md`）ため、ワイヤ形状を
    /// 直接検査する。
    #[test]
    fn commissioning_status_response_always_writes_debug_text() {
        let fields = encode_commissioning_status_response(0, "");
        let map = scan_struct_fields(&fields, "test").expect("struct");
        assert!(
            map.contains_key(&1),
            "DebugText (tag 1) must be present even when empty: {fields:02X?}"
        );
    }

    #[test]
    fn encode_attestation_response_roundtrips_with_decoder() {
        let (el, sig) =
            decode_attestation_response(&encode_attestation_response(b"elements", &[0xAB; 64]))
                .unwrap();
        assert_eq!(el, b"elements");
        assert_eq!(sig, [0xAB; 64]);
    }

    #[test]
    fn encode_cert_chain_response_roundtrips_with_decoder() {
        assert_eq!(
            decode_cert_chain_response(&encode_cert_chain_response(b"der-bytes")).unwrap(),
            b"der-bytes"
        );
    }

    #[test]
    fn encode_nocsr_elements_roundtrips_with_parser() {
        let nonce = [7u8; 32];
        let (csr, parsed_nonce) =
            parse_nocsr_elements(&encode_nocsr_elements(b"csr-der", &nonce)).unwrap();
        assert_eq!(csr, b"csr-der");
        assert_eq!(parsed_nonce, nonce.to_vec());
    }

    #[test]
    fn encode_csr_response_roundtrips_with_decoder() {
        let elements = encode_nocsr_elements(b"csr-der", &[7u8; 32]);
        let (el, sig) = decode_csr_response(&encode_csr_response(&elements, &[0xCD; 64])).unwrap();
        assert_eq!(el, elements);
        assert_eq!(sig, [0xCD; 64]);
    }

    #[test]
    fn encode_noc_response_roundtrips_with_decoder() {
        let (status, idx) = decode_noc_response(&encode_noc_response(0, Some(1))).unwrap();
        assert_eq!((status, idx), (0, Some(1)));
        let (status2, idx2) = decode_noc_response(&encode_noc_response(3, None)).unwrap();
        assert_eq!((status2, idx2), (3, None));
    }

    #[test]
    fn decode_update_fabric_label_roundtrips_with_encoder() {
        assert_eq!(
            decode_update_fabric_label(&encode_update_fabric_label("Alexa-1")).unwrap(),
            "Alexa-1"
        );
        assert_eq!(
            decode_update_fabric_label(&encode_update_fabric_label("")).unwrap(),
            ""
        );
    }

    #[test]
    fn decode_update_fabric_label_rejects_over_32_bytes() {
        let label = "x".repeat(33);
        let err = decode_update_fabric_label(&encode_update_fabric_label(&label)).unwrap_err();
        assert!(matches!(err, CommissionError::Malformed { .. }));
    }

    #[test]
    fn decode_update_fabric_label_accepts_exactly_32_bytes() {
        let label = "x".repeat(32);
        assert_eq!(
            decode_update_fabric_label(&encode_update_fabric_label(&label)).unwrap(),
            label
        );
    }

    #[test]
    fn decode_remove_fabric_roundtrips_with_encoder() {
        assert_eq!(decode_remove_fabric(&encode_remove_fabric(1)).unwrap(), 1);
        assert_eq!(decode_remove_fabric(&encode_remove_fabric(9)).unwrap(), 9);
    }

    #[test]
    fn decode_remove_fabric_rejects_missing_fabric_index() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.end_container();
        let err = decode_remove_fabric(&w.finish()).unwrap_err();
        assert!(matches!(err, CommissionError::Malformed { .. }));
    }

    /// `{0: u16, 1: bytes(97), 2: u16, 3: u32, 4: bytes}` を組む小ヘルパ。
    /// `skip` に入れたタグは省略する（欠損分岐のテスト用）。
    fn open_window_fields(skip: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        if !skip.contains(&0) {
            w.put_uint(Tag::Context(0), 180);
        }
        if !skip.contains(&1) {
            w.put_bytes(Tag::Context(1), &[7u8; 97]);
        }
        if !skip.contains(&2) {
            w.put_uint(Tag::Context(2), 0xABC);
        }
        if !skip.contains(&3) {
            w.put_uint(Tag::Context(3), 1000);
        }
        if !skip.contains(&4) {
            w.put_bytes(Tag::Context(4), &[9u8; 16]);
        }
        w.end_container();
        w.finish()
    }

    #[test]
    fn decode_open_commissioning_window_roundtrips_with_encoder() {
        let fields = encode_open_commissioning_window(180, &[7u8; 97], 0xABC, 1000, &[9u8; 16]);
        let (timeout_s, verifier, disc, iters, salt) =
            decode_open_commissioning_window(&fields).expect("decode");
        assert_eq!(timeout_s, 180);
        assert_eq!(verifier, vec![7u8; 97]);
        assert_eq!(disc, 0xABC);
        assert_eq!(iters, 1000);
        assert_eq!(salt, vec![9u8; 16]);
    }

    #[test]
    fn decode_open_commissioning_window_reports_each_missing_field() {
        let expected = [
            (0u8, "missing timeout"),
            (1, "missing verifier"),
            (2, "missing discriminator"),
            (3, "missing iterations"),
            (4, "missing salt"),
        ];
        for (tag, detail) in expected {
            match decode_open_commissioning_window(&open_window_fields(&[tag])) {
                Err(CommissionError::Malformed { detail: d, .. }) => {
                    assert_eq!(d, detail, "tag {tag}")
                }
                other => panic!("tag {tag}: expected Malformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn decode_open_commissioning_window_rejects_iterations_over_u32() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 180);
        w.put_bytes(Tag::Context(1), &[7u8; 97]);
        w.put_uint(Tag::Context(2), 0xABC);
        w.put_uint(Tag::Context(3), 1u64 << 32);
        w.put_bytes(Tag::Context(4), &[9u8; 16]);
        w.end_container();
        match decode_open_commissioning_window(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => {
                assert_eq!(detail, "iterations out of range")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn decode_add_noc_accepts_icac_and_rejects_bad_ipk_length() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(0), b"noc");
        w.put_bytes(Tag::Context(1), b"icac");
        w.put_bytes(Tag::Context(2), &[0xAA; 16]);
        w.put_uint(Tag::Context(3), 0x1122);
        w.put_uint(Tag::Context(4), 0xFFF1);
        w.end_container();
        let (noc, icac, ipk, subject, vid) = decode_add_noc(&w.finish()).unwrap();
        assert_eq!(noc, b"noc");
        assert_eq!(icac.as_deref(), Some(&b"icac"[..]));
        assert_eq!(ipk, [0xAA; 16]);
        assert_eq!((subject, vid), (0x1122, 0xFFF1));

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(0), b"noc");
        w.put_bytes(Tag::Context(2), &[0xAA; 15]);
        w.put_uint(Tag::Context(3), 1);
        w.put_uint(Tag::Context(4), 1);
        w.end_container();
        match decode_add_noc(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => assert_eq!(detail, "ipk length"),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }
}
