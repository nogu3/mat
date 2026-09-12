//! コミッショナー側の Commissioning コマンド codec: リクエスト builder と
//! レスポンス decoder（General / Operational Credentials / Administrator /
//! Network Commissioning）。

use crate::tlv::{Tag, Writer};

use super::tlv_fields::{required, scan_struct_fields, take_bytes, take_uint, take_utf8};
use super::CommissionError;

// --- builders ---
//
// すべて anonymous struct + context tag の CommandFields 1 個を返す。
// `im::encode_invoke_request` の `fields_tlv` 契約（完全な TLV 1 要素、タグ
// は呼び出し側で再付与される）を満たす形。

/// ArmFailSafeRequest（spec §11.10.6.2）: `{0: ExpiryLengthSeconds, 1:
/// Breadcrumb}`。
pub fn encode_arm_fail_safe(expiry_s: u16, breadcrumb: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(expiry_s));
    w.put_uint(Tag::Context(1), breadcrumb);
    w.end_container();
    w.finish()
}

/// SetRegulatoryConfigRequest（spec §11.10.6.4）: `{0: NewRegulatoryConfig,
/// 1: CountryCode, 2: Breadcrumb}`。
pub fn encode_set_regulatory_config(config: u8, country: &str, breadcrumb: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(config));
    w.put_str(Tag::Context(1), country);
    w.put_uint(Tag::Context(2), breadcrumb);
    w.end_container();
    w.finish()
}

/// AttestationRequest（spec §11.17.6.7）: `{0: AttestationNonce}`。
pub fn encode_attestation_request(nonce: &[u8; 32]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), nonce);
    w.end_container();
    w.finish()
}

/// CertificateChainRequest（spec §11.17.6.4）: `{0: CertificateType}`
/// （`CERT_TYPE_DAC` / `CERT_TYPE_PAI`）。
pub fn encode_cert_chain_request(cert_type: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(cert_type));
    w.end_container();
    w.finish()
}

/// CSRRequest（spec §11.17.6.9）: `{0: CSRNonce}`（`isForUpdateNOC` は使わな
/// い ので省略——このフローは初回コミッショニングのみ扱う）。
pub fn encode_csr_request(nonce: &[u8; 32]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), nonce);
    w.end_container();
    w.finish()
}

/// AddTrustedRootCertificate（spec §11.17.6.11）: `{0: RootCACertificate}`。
/// `rcac_tlv` はそれ自体 Matter-TLV 証明書だが、コマンドフィールド上は
/// octet string（証明書の生バイト列を包んだ bytes）として渡す——ネストした
/// TLV 要素として埋め込むのではない。
pub fn encode_add_trusted_root(rcac_tlv: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), rcac_tlv);
    w.end_container();
    w.finish()
}

/// AddNOC（spec §11.17.6.13）: `{0: NOCValue, 2: IPKValue, 3:
/// CaseAdminSubject, 4: AdminVendorId}`。tag1（ICACValue）は意図的に省略——
/// このコントローラが発行する fabric は root が直接 NOC に署名する 2-cert
/// チェーンで ICAC を持たない（`cert::issue_noc` のドキュメント参照）。
pub fn encode_add_noc(
    noc_tlv: &[u8],
    ipk_epoch: &[u8; 16],
    case_admin_subject: u64,
    admin_vendor_id: u16,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), noc_tlv);
    w.put_bytes(Tag::Context(2), ipk_epoch);
    w.put_uint(Tag::Context(3), case_admin_subject);
    w.put_uint(Tag::Context(4), u64::from(admin_vendor_id));
    w.end_container();
    w.finish()
}

/// UpdateFabricLabel（spec §11.17.6.11）: `{0: Label}`。fabric-scoped —
/// operates on the invoking session's own fabric (no `FabricIndex` field;
/// unlike `RemoveFabric` this command can't target a different fabric). No
/// production caller in this workspace yet (an admin app, not
/// `mat-controller`'s own commissioning flow, would send this) — kept `pub`
/// rather than `#[cfg(test)]` so `mat-device`'s test module (a separate
/// crate) can use it too, same as the other `encode_*` builders here.
pub fn encode_update_fabric_label(label: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_str(Tag::Context(0), label);
    w.end_container();
    w.finish()
}

/// RemoveFabric（spec §11.17.6.15）: `{0: FabricIndex}`。
pub fn encode_remove_fabric(fabric_index: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(fabric_index));
    w.end_container();
    w.finish()
}

/// AddOrUpdateThreadNetwork（spec §11.9.7.3）: `{0: OperationalDataset, 1:
/// Breadcrumb}`。dataset は OTBR の `dataset active -x` が返す Thread TLV
/// 生バイト列そのまま。
pub fn encode_add_or_update_thread_network(dataset: &[u8], breadcrumb: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), dataset);
    w.put_uint(Tag::Context(1), breadcrumb);
    w.end_container();
    w.finish()
}

/// ConnectNetwork（spec §11.9.7.9）: `{0: NetworkID, 1: Breadcrumb}`。Thread
/// の NetworkID は dataset 中の Extended PAN ID（8 バイト）。
pub fn encode_connect_network(network_id: &[u8], breadcrumb: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), network_id);
    w.put_uint(Tag::Context(1), breadcrumb);
    w.end_container();
    w.finish()
}

/// OpenCommissioningWindow（spec §11.19.8.1）: `{0: CommissioningTimeout, 1:
/// PAKEPasscodeVerifier, 2: Discriminator, 3: Iterations, 4: Salt}`。timed
/// invoke 必須のコマンド（Task 10 が `im::encode_invoke_request_timed` で
/// 送る）。
pub fn encode_open_commissioning_window(
    timeout_s: u16,
    verifier: &[u8; 97],
    discriminator: u16,
    iterations: u32,
    salt: &[u8],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(timeout_s));
    w.put_bytes(Tag::Context(1), verifier);
    w.put_uint(Tag::Context(2), u64::from(discriminator));
    w.put_uint(Tag::Context(3), u64::from(iterations));
    w.put_bytes(Tag::Context(4), salt);
    w.end_container();
    w.finish()
}

// --- decoders ---
//
// 入力の `fields` は InvokeResponse の CommandFields TLV 1 要素（先頭タグは
// `Tag::Anonymous` に付け替え済み——`im::InvokeResponseData::fields_tlv` の
// 契約）。欠落フィールドは `CommissionError::Malformed` にする。

/// ArmFailSafeResponse / SetRegulatoryConfigResponse / CommissioningComplete
/// Response（spec §11.10.6.3 / .5 / .7）共通の `{0: ErrorCode, 1: DebugText}`
/// 形。`DebugText` は spec 上 optional なので欠落時は空文字列にする。戻り値
/// は `(errorCode, debugText)`。
pub fn decode_commissioning_status_response(
    fields: &[u8],
) -> Result<(u8, String), CommissionError> {
    let step = "commissioning_status_response";
    let mut map = scan_struct_fields(fields, step)?;
    let error_code = required(
        take_uint::<u8>(&mut map, 0, step, "errorCode out of range")?,
        step,
        "missing errorCode",
    )?;
    let debug_text = take_utf8(&mut map, 1).unwrap_or_default();
    Ok((error_code, debug_text))
}

/// AttestationResponse（spec §11.17.6.8）: `{0: AttestationElements, 1:
/// AttestationSignature}`。戻り値は `(elements, signature)`。
pub fn decode_attestation_response(fields: &[u8]) -> Result<(Vec<u8>, [u8; 64]), CommissionError> {
    let step = "attestation_response";
    let mut map = scan_struct_fields(fields, step)?;
    let elements = required(take_bytes(&mut map, 0), step, "missing elements")?;
    let sig_bytes = required(take_bytes(&mut map, 1), step, "missing signature")?;
    let signature: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| CommissionError::Malformed {
            step,
            detail: "signature length",
        })?;
    Ok((elements, signature))
}

/// CertificateChainResponse（spec §11.17.6.5）: `{0: Certificate}`（DAC ま
/// たは PAI の X.509 DER をそのまま bytes に包んだもの）。
pub fn decode_cert_chain_response(fields: &[u8]) -> Result<Vec<u8>, CommissionError> {
    let step = "cert_chain_response";
    let mut map = scan_struct_fields(fields, step)?;
    required(take_bytes(&mut map, 0), step, "missing certificate")
}

/// CSRResponse（spec §11.17.6.10）: `{0: NOCSRElements, 1:
/// AttestationSignature}`。戻り値は `(nocsr_elements, signature)`——
/// `nocsr_elements` は生の TLV バイト列のまま返す（中身は
/// `parse_nocsr_elements` が読む）。
pub fn decode_csr_response(fields: &[u8]) -> Result<(Vec<u8>, [u8; 64]), CommissionError> {
    let step = "csr_response";
    let mut map = scan_struct_fields(fields, step)?;
    let elements = required(take_bytes(&mut map, 0), step, "missing nocsr elements")?;
    let sig_bytes = required(take_bytes(&mut map, 1), step, "missing signature")?;
    let signature: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| CommissionError::Malformed {
            step,
            detail: "signature length",
        })?;
    Ok((elements, signature))
}

/// NOCSRElements（spec §11.17.6.10.1）: `{1: csr, 2: CSRNonce, 3/4: vendor
/// reserved(optional)}`。戻り値は `(csr_der, csr_nonce)`。vendor reserved
/// フィールドが付いていても無視する。
pub fn parse_nocsr_elements(elements: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CommissionError> {
    let step = "nocsr_elements";
    let mut map = scan_struct_fields(elements, step)?;
    let csr = required(take_bytes(&mut map, 1), step, "missing csr")?;
    let nonce = required(take_bytes(&mut map, 2), step, "missing csr nonce")?;
    Ok((csr, nonce))
}

/// NOCResponse（spec §11.17.6.14, AddNOC / RemoveFabric 共通の応答）:
/// `{0: StatusCode, 1: FabricIndex(optional), 2: DebugText(optional)}`。戻
/// り値は `(statusCode, fabricIndex)`。
pub fn decode_noc_response(fields: &[u8]) -> Result<(u8, Option<u8>), CommissionError> {
    let step = "noc_response";
    let mut map = scan_struct_fields(fields, step)?;
    let status = required(
        take_uint::<u8>(&mut map, 0, step, "statusCode out of range")?,
        step,
        "missing statusCode",
    )?;
    let fabric_index = take_uint::<u8>(&mut map, 1, step, "fabricIndex out of range")?;
    Ok((status, fabric_index))
}

/// NetworkConfigResponse（spec §11.9.7.6）: `{0: NetworkingStatus, 1:
/// DebugText(optional), 2: NetworkIndex(optional)}`。戻り値は
/// `(networkingStatus, debugText)`。NetworkIndex は現状使わないので
/// `scan_struct_fields` が拾っても読み捨てる。
pub fn decode_network_config_response(
    fields: &[u8],
) -> Result<(u8, Option<String>), CommissionError> {
    let step = "network_config_response";
    let mut map = scan_struct_fields(fields, step)?;
    let status = required(
        take_uint::<u8>(&mut map, 0, step, "networkingStatus out of range")?,
        step,
        "missing networkingStatus",
    )?;
    let debug_text = take_utf8(&mut map, 1);
    Ok((status, debug_text))
}

/// ConnectNetworkResponse（spec §11.9.7.9 応答）: `{0: NetworkingStatus, 1:
/// DebugText(optional), 2: ErrorValue(optional, signed int32)}`。戻り値は
/// `(networkingStatus, debugText)`。ErrorValue は `Value::Int` として
/// `scan_struct_fields` の catch-all で読み捨てられるので tag2 の有無どち
/// らでも decode できる。
pub fn decode_connect_network_response(
    fields: &[u8],
) -> Result<(u8, Option<String>), CommissionError> {
    let step = "connect_network_response";
    let mut map = scan_struct_fields(fields, step)?;
    let status = required(
        take_uint::<u8>(&mut map, 0, step, "networkingStatus out of range")?,
        step,
        "missing networkingStatus",
    )?;
    let debug_text = take_utf8(&mut map, 1);
    Ok((status, debug_text))
}

/// Thread operational dataset（MeshCoP TLV 列）から Extended PAN ID
/// （type 2, len 8）を取り出す。ConnectNetwork の NetworkID に使う。純関数
/// ——OTBR が返す生バイト列を直接舐めるので、境界外アクセスは一切せず
/// `checked_add` で長さ計算をガードする（壊れた TLV は panic ではなく
/// `None`）。
pub fn thread_ext_pan_id(dataset: &[u8]) -> Option<[u8; 8]> {
    let mut i = 0usize;
    while i + 2 <= dataset.len() {
        let (t, l) = (dataset[i], usize::from(dataset[i + 1]));
        let end = i.checked_add(2)?.checked_add(l)?;
        if end > dataset.len() {
            return None;
        }
        if t == 2 && l == 8 {
            return dataset[i + 2..end].try_into().ok();
        }
        i = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn arm_fail_safe_fields_shape() {
        let f = encode_arm_fail_safe(120, 1);
        let mut r = Reader::new(&f);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Uint(120)));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Uint(1)));
    }

    #[test]
    fn add_noc_fields_shape() {
        let f = encode_add_noc(b"noc", &[9u8; 16], 0x1_0001, 0xFFF1);
        let mut r = Reader::new(&f);
        r.next().unwrap(); // struct
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Bytes(b) if b == b"noc"));
        // tag1 (ICAC) は無いこと
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(2)); // IPKValue
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::Uint(0x1_0001)
        ));
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::Uint(0xFFF1)
        ));
    }

    #[test]
    fn decodes_noc_response_and_status_response() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0);
        w.put_uint(Tag::Context(1), 3);
        w.end_container();
        let (status, idx) = decode_noc_response(&w.finish()).unwrap();
        assert_eq!((status, idx), (0, Some(3)));

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0);
        w.put_str(Tag::Context(1), "");
        w.end_container();
        let (code, _) = decode_commissioning_status_response(&w.finish()).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn decodes_attestation_and_csr_responses() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(0), b"elements");
        w.put_bytes(Tag::Context(1), &[0xAB; 64]);
        w.end_container();
        let (el, sig) = decode_attestation_response(&w.finish()).unwrap();
        assert_eq!(el, b"elements");
        assert_eq!(sig, [0xAB; 64]);

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), b"csr-der");
        w.put_bytes(Tag::Context(2), &[7u8; 32]);
        w.end_container();
        let (csr, nonce) = parse_nocsr_elements(&w.finish()).unwrap();
        assert_eq!(csr, b"csr-der");
        assert_eq!(nonce, vec![7u8; 32]);
    }

    #[test]
    fn open_window_fields_shape() {
        let f = encode_open_commissioning_window(180, &[1u8; 97], 0xABC, 1000, &[2u8; 32]);
        let mut r = Reader::new(&f);
        r.next().unwrap(); // struct
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Uint(180)));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Bytes(b) if b.len() == 97));
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::Uint(0xABC)
        ));
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::Uint(1000)
        ));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Bytes(b) if b.len() == 32));
    }

    // --- Task 4: NetworkCommissioning TLV / Thread dataset ---

    #[test]
    fn thread_dataset_ext_pan_id_extracts_type2() {
        // MeshCoP TLV: ActiveTimestamp(14,len8) + ExtPanId(2,len8) + Channel(0,len3)
        let mut ds = vec![0x0E, 0x08, 0, 0, 0, 0, 0, 1, 0, 0];
        ds.extend_from_slice(&[0x02, 0x08, 0xDE, 0xAD, 0x00, 0xBE, 0xEF, 0x00, 0xCA, 0xFE]);
        ds.extend_from_slice(&[0x00, 0x03, 0x00, 0x00, 0x0F]);
        assert_eq!(
            thread_ext_pan_id(&ds),
            Some([0xDE, 0xAD, 0x00, 0xBE, 0xEF, 0x00, 0xCA, 0xFE])
        );
        // ExtPanId なし / 壊れた TLV は None
        assert_eq!(thread_ext_pan_id(&ds[..10]), None);
        assert_eq!(thread_ext_pan_id(&[0x02, 0x09, 0x00]), None);
    }

    #[test]
    fn thread_ext_pan_id_boundary_cases() {
        // type 2 だが長さ != 8 は読み飛ばし、後続の正しい type 2 を拾う。
        let mut ds = vec![0x02, 0x04, 0xAA, 0xBB, 0xCC, 0xDD];
        ds.extend_from_slice(&[0x02, 0x08, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(thread_ext_pan_id(&ds), Some([1, 2, 3, 4, 5, 6, 7, 8]));
        // TLV がバッファ末尾ちょうどで終わる正常形。
        let exact = [0x00, 0x01, 0xFF, 0x02, 0x08, 8, 7, 6, 5, 4, 3, 2, 1];
        assert_eq!(thread_ext_pan_id(&exact), Some([8, 7, 6, 5, 4, 3, 2, 1]));
        // 長さが残りを超える壊れ TLV は None（panic しない）。
        assert_eq!(thread_ext_pan_id(&[0x00, 0xFF, 0x01]), None);
        // 空 / ヘッダ未満。
        assert_eq!(thread_ext_pan_id(&[]), None);
        assert_eq!(thread_ext_pan_id(&[0x02]), None);
        // 長さ 0 の TLV を挟んでも走査が止まらない。
        let with_zero = [0x03, 0x00, 0x02, 0x08, 1, 1, 2, 2, 3, 3, 4, 4];
        assert_eq!(
            thread_ext_pan_id(&with_zero),
            Some([1, 1, 2, 2, 3, 3, 4, 4])
        );
    }

    #[test]
    fn network_commissioning_encoders_shape() {
        // AddOrUpdateThreadNetwork {0: dataset, 1: breadcrumb}
        let f = encode_add_or_update_thread_network(&[0xAA, 0xBB], 3);
        let mut r = Reader::new(&f);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(0));
        assert!(matches!(e.value, Value::Bytes(b) if b == [0xAA, 0xBB]));
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(1));
        assert!(matches!(e.value, Value::Uint(3)));

        // ConnectNetwork {0: networkID, 1: breadcrumb}
        let f2 = encode_connect_network(&[1, 2, 3, 4, 5, 6, 7, 8], 4);
        let mut r2 = Reader::new(&f2);
        assert!(matches!(
            r2.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        let e = r2.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(0));
        assert!(matches!(e.value, Value::Bytes(b) if b == [1, 2, 3, 4, 5, 6, 7, 8]));
        let e = r2.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(1));
        assert!(matches!(e.value, Value::Uint(4)));
    }

    #[test]
    fn connect_network_response_decodes_status_and_text() {
        // {0: status=0, 1: "ok", 2: errorValue} を Writer で作って decode
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0);
        w.put_str(Tag::Context(1), "ok");
        w.put_int(Tag::Context(2), 0);
        w.end_container();
        let (status, text) = decode_connect_network_response(&w.finish()).unwrap();
        assert_eq!(status, 0);
        assert_eq!(text.as_deref(), Some("ok"));

        // tag2 (ErrorValue) 省略でも decode できる
        let mut w2 = Writer::new();
        w2.start_struct(Tag::Anonymous);
        w2.put_uint(Tag::Context(0), 1);
        w2.end_container();
        let (status2, text2) = decode_connect_network_response(&w2.finish()).unwrap();
        assert_eq!(status2, 1);
        assert_eq!(text2, None);
    }

    #[test]
    fn network_config_response_decodes_status_and_text() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0);
        w.put_str(Tag::Context(1), "added");
        w.put_uint(Tag::Context(2), 0); // NetworkIndex
        w.end_container();
        let (status, text) = decode_network_config_response(&w.finish()).unwrap();
        assert_eq!(status, 0);
        assert_eq!(text.as_deref(), Some("added"));

        // tag1/tag2 省略でも decode できる
        let mut w2 = Writer::new();
        w2.start_struct(Tag::Anonymous);
        w2.put_uint(Tag::Context(0), 5);
        w2.end_container();
        let (status2, text2) = decode_network_config_response(&w2.finish()).unwrap();
        assert_eq!(status2, 5);
        assert_eq!(text2, None);
    }

    #[test]
    fn signature_carrying_responses_reject_wrong_signature_length() {
        for (name, dec) in [
            (
                "attestation",
                decode_attestation_response
                    as fn(&[u8]) -> Result<(Vec<u8>, [u8; 64]), CommissionError>,
            ),
            ("csr", decode_csr_response),
        ] {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_bytes(Tag::Context(0), b"elements");
            w.put_bytes(Tag::Context(1), &[0u8; 63]);
            w.end_container();
            match dec(&w.finish()) {
                Err(CommissionError::Malformed { detail, .. }) => {
                    assert_eq!(detail, "signature length", "{name}")
                }
                other => panic!("{name}: expected Malformed, got {other:?}"),
            }
        }
    }
}
