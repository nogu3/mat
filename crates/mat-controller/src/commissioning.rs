//! Commissioning コマンド codec と使い捨て第二 fabric の素材（M6a Task9）。
//!
//! 対象クラスタ:
//! - General Commissioning（spec §11.10）: ArmFailSafe / SetRegulatoryConfig /
//!   CommissioningComplete。
//! - Node Operational Credentials（spec §11.17）: AttestationRequest /
//!   CertificateChainRequest / CSRRequest / AddTrustedRootCertificate /
//!   AddNOC / RemoveFabric。
//! - Administrator Commissioning（spec §11.19）: OpenCommissioningWindow。
//!
//! この module は「コマンド payload の builder / decoder」と「使い捨て第二
//! fabric（controller 自身がここでコミッショニング用に生成し、KVS には永続
//! 化しない root 証明書一式）」だけを持つ。ステップの順序制御（PASE 確立 →
//! ArmFailSafe → attestation → CSR → NOC 発行 → AddTrustedRoot → AddNOC →
//! CommissioningComplete）は Task 10 の役割で、ここでは扱わない。

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::attestation;
use crate::case;
use crate::cert::{self, MatterCert};
use crate::crypto;
use crate::dnssd;
use crate::exchange::MrpConfig;
use crate::fabric::{self, FabricCredentials};
use crate::im;
use crate::kvs::SelfIssueMaterials;
use crate::pase;
use crate::session::SecureSession;
use crate::tlv::{Element, Reader, Tag, Value, Writer};
use crate::transport::{Transport, UdpTransport};
use crate::x509;

// --- General Commissioning cluster (spec §11.10) ---

pub const CLUSTER_GENERAL_COMMISSIONING: u32 = 0x0030;
pub const CMD_ARM_FAIL_SAFE: u32 = 0x00; // resp 0x01
pub const CMD_SET_REGULATORY_CONFIG: u32 = 0x02; // resp 0x03
pub const CMD_COMMISSIONING_COMPLETE: u32 = 0x04; // resp 0x05

// --- Node Operational Credentials cluster (spec §11.17) ---

pub const CLUSTER_OPERATIONAL_CREDENTIALS: u32 = 0x003E;
pub const CMD_ATTESTATION_REQUEST: u32 = 0x00; // resp 0x01
pub const CMD_CERT_CHAIN_REQUEST: u32 = 0x02; // resp 0x03
pub const CMD_CSR_REQUEST: u32 = 0x04; // resp 0x05
pub const CMD_ADD_NOC: u32 = 0x06; // resp NOCResponse 0x08
pub const CMD_REMOVE_FABRIC: u32 = 0x0A; // resp NOCResponse 0x08
pub const CMD_ADD_TRUSTED_ROOT: u32 = 0x0B; // 応答は NOCResponse ではなく status

// --- Administrator Commissioning cluster (spec §11.19) ---

pub const CLUSTER_ADMIN_COMMISSIONING: u32 = 0x003C;
pub const CMD_OPEN_COMMISSIONING_WINDOW: u32 = 0x00; // timed 必須

/// CertificateChainRequest の CertificateType（spec §11.17.6.4）: DAC。
pub const CERT_TYPE_DAC: u8 = 1;
/// CertificateChainRequest の CertificateType（spec §11.17.6.4）: PAI。
pub const CERT_TYPE_PAI: u8 = 2;

// --- Network Commissioning cluster (spec §11.9) ---

pub const CLUSTER_NETWORK_COMMISSIONING: u32 = 0x0031;
pub const CMD_ADD_OR_UPDATE_THREAD: u32 = 0x03; // resp NetworkConfigResponse 0x05
pub const CMD_CONNECT_NETWORK: u32 = 0x06; // resp ConnectNetworkResponse 0x07

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

/// `fields` の次要素を読み、TLV 復号エラー / 末尾切れをまとめて
/// `CommissionError::Malformed` に変換する。
fn next_el<'a>(r: &mut Reader<'a>, step: &'static str) -> Result<Element<'a>, CommissionError> {
    r.next()
        .map_err(|_| CommissionError::Malformed {
            step,
            detail: "tlv decode error",
        })?
        .ok_or(CommissionError::Malformed {
            step,
            detail: "truncated",
        })
}

/// 先頭要素が struct start であることを確認して読み捨てる。
fn expect_struct(r: &mut Reader, step: &'static str) -> Result<(), CommissionError> {
    match next_el(r, step)?.value {
        Value::StructStart => Ok(()),
        _ => Err(CommissionError::Malformed {
            step,
            detail: "expected struct",
        }),
    }
}

/// 未知のタグに付随するコンテナを、対応する `ContainerEnd` まで読み飛ばす
/// （深さ 1 の状態、つまり start 要素は読み終わっている前提）。
fn skip_container(r: &mut Reader, step: &'static str) -> Result<(), CommissionError> {
    let mut depth = 1usize;
    while depth > 0 {
        match next_el(r, step)?.value {
            Value::StructStart | Value::ArrayStart | Value::ListStart => depth += 1,
            Value::ContainerEnd => depth -= 1,
            _ => {}
        }
    }
    Ok(())
}

/// [`scan_struct_fields`] が集める TLV leaf 要素の値。以下の decoder が読む
/// 応答はすべて「フラットな struct 直下に leaf だけが並ぶ」形（ネストした
/// struct/array/list を持つフィールドは無い）なのでコンテナ型は持たない
/// ——コンテナタグは `skip_container` で読み飛ばされ、この enum には現れない。
enum FieldValue {
    Uint(u64),
    Bytes(Vec<u8>),
    Utf8(String),
}

/// `fields` の TLV struct 1 段をスキャンし、直下の leaf 要素を
/// `{contextTag: value}` に集める（同じタグが複数回現れたら後勝ち——1 回の
/// ループで都度代入していた旧実装と同じ挙動）。Task 9 で書かれた 6 個の
/// decoder はどれも「struct を開いて直下のタグ別 leaf を拾い、知らない
/// コンテナは読み飛ばす」という同型のループだった。ここに 1 箇所へ集約し、
/// 各 decoder は `take_*` でタグを引くだけにする。
fn scan_struct_fields(
    fields: &[u8],
    step: &'static str,
) -> Result<BTreeMap<u8, FieldValue>, CommissionError> {
    let mut r = Reader::new(fields);
    expect_struct(&mut r, step)?;
    let mut out = BTreeMap::new();
    loop {
        let el = next_el(&mut r, step)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(t), Value::Uint(v)) => {
                out.insert(t, FieldValue::Uint(v));
            }
            (Tag::Context(t), Value::Bytes(b)) => {
                out.insert(t, FieldValue::Bytes(b.to_vec()));
            }
            (Tag::Context(t), Value::Utf8(s)) => {
                out.insert(t, FieldValue::Utf8(s.to_string()));
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r, step)?;
            }
            _ => {}
        }
    }
    Ok(out)
}

/// `map` からタグ `tag` を u8 として取り出す。タグが無い、または値が
/// `Uint` 以外の型なら「無かった」扱いで `Ok(None)`（旧実装で型不一致の
/// 分岐が黙って読み捨てられていたのと同じ）。`Uint` ではあるが u8 に収まら
/// ない場合だけ `range_detail` で `Malformed` を返す。
fn take_u8(
    map: &mut BTreeMap<u8, FieldValue>,
    tag: u8,
    step: &'static str,
    range_detail: &'static str,
) -> Result<Option<u8>, CommissionError> {
    match map.remove(&tag) {
        Some(FieldValue::Uint(v)) => Ok(Some(u8::try_from(v).map_err(|_| {
            CommissionError::Malformed {
                step,
                detail: range_detail,
            }
        })?)),
        _ => Ok(None),
    }
}

/// `map` からタグ `tag` を `Vec<u8>` として取り出す（型不一致は「無かっ
/// た」扱い、[`take_u8`] と同じ方針）。
fn take_bytes(map: &mut BTreeMap<u8, FieldValue>, tag: u8) -> Option<Vec<u8>> {
    match map.remove(&tag) {
        Some(FieldValue::Bytes(b)) => Some(b),
        _ => None,
    }
}

/// `map` からタグ `tag` を `String` として取り出す（型不一致は「無かっ
/// た」扱い、[`take_u8`] と同じ方針）。
fn take_utf8(map: &mut BTreeMap<u8, FieldValue>, tag: u8) -> Option<String> {
    match map.remove(&tag) {
        Some(FieldValue::Utf8(s)) => Some(s),
        _ => None,
    }
}

/// ArmFailSafeResponse / SetRegulatoryConfigResponse / CommissioningComplete
/// Response（spec §11.10.6.3 / .5 / .7）共通の `{0: ErrorCode, 1: DebugText}`
/// 形。`DebugText` は spec 上 optional なので欠落時は空文字列にする。戻り値
/// は `(errorCode, debugText)`。
pub fn decode_commissioning_status_response(
    fields: &[u8],
) -> Result<(u8, String), CommissionError> {
    let step = "commissioning_status_response";
    let mut map = scan_struct_fields(fields, step)?;
    let error_code = take_u8(&mut map, 0, step, "errorCode out of range")?.ok_or(
        CommissionError::Malformed {
            step,
            detail: "missing errorCode",
        },
    )?;
    let debug_text = take_utf8(&mut map, 1).unwrap_or_default();
    Ok((error_code, debug_text))
}

/// AttestationResponse（spec §11.17.6.8）: `{0: AttestationElements, 1:
/// AttestationSignature}`。戻り値は `(elements, signature)`。
pub fn decode_attestation_response(fields: &[u8]) -> Result<(Vec<u8>, [u8; 64]), CommissionError> {
    let step = "attestation_response";
    let mut map = scan_struct_fields(fields, step)?;
    let elements = take_bytes(&mut map, 0).ok_or(CommissionError::Malformed {
        step,
        detail: "missing elements",
    })?;
    let sig_bytes = take_bytes(&mut map, 1).ok_or(CommissionError::Malformed {
        step,
        detail: "missing signature",
    })?;
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
    take_bytes(&mut map, 0).ok_or(CommissionError::Malformed {
        step,
        detail: "missing certificate",
    })
}

/// CSRResponse（spec §11.17.6.10）: `{0: NOCSRElements, 1:
/// AttestationSignature}`。戻り値は `(nocsr_elements, signature)`——
/// `nocsr_elements` は生の TLV バイト列のまま返す（中身は
/// `parse_nocsr_elements` が読む）。
pub fn decode_csr_response(fields: &[u8]) -> Result<(Vec<u8>, [u8; 64]), CommissionError> {
    let step = "csr_response";
    let mut map = scan_struct_fields(fields, step)?;
    let elements = take_bytes(&mut map, 0).ok_or(CommissionError::Malformed {
        step,
        detail: "missing nocsr elements",
    })?;
    let sig_bytes = take_bytes(&mut map, 1).ok_or(CommissionError::Malformed {
        step,
        detail: "missing signature",
    })?;
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
    let csr = take_bytes(&mut map, 1).ok_or(CommissionError::Malformed {
        step,
        detail: "missing csr",
    })?;
    let nonce = take_bytes(&mut map, 2).ok_or(CommissionError::Malformed {
        step,
        detail: "missing csr nonce",
    })?;
    Ok((csr, nonce))
}

/// NOCResponse（spec §11.17.6.14, AddNOC / RemoveFabric 共通の応答）:
/// `{0: StatusCode, 1: FabricIndex(optional), 2: DebugText(optional)}`。戻
/// り値は `(statusCode, fabricIndex)`。
pub fn decode_noc_response(fields: &[u8]) -> Result<(u8, Option<u8>), CommissionError> {
    let step = "noc_response";
    let mut map = scan_struct_fields(fields, step)?;
    let status = take_u8(&mut map, 0, step, "statusCode out of range")?.ok_or(
        CommissionError::Malformed {
            step,
            detail: "missing statusCode",
        },
    )?;
    let fabric_index = take_u8(&mut map, 1, step, "fabricIndex out of range")?;
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
    let status = take_u8(&mut map, 0, step, "networkingStatus out of range")?.ok_or(
        CommissionError::Malformed {
            step,
            detail: "missing networkingStatus",
        },
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
    let status = take_u8(&mut map, 0, step, "networkingStatus out of range")?.ok_or(
        CommissionError::Malformed {
            step,
            detail: "missing networkingStatus",
        },
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

// --- ステップマシン（Task 10） ---
//
// 順序: ターゲット解決 → PASE → ArmFailSafe(必須) → SetRegulatoryConfig(任
// 意、失敗は warn で続行) → attestation(厳格) → CSR → NOC 発行 →
// AddTrustedRootCertificate → AddNOC → 新 fabric で CASE(リトライ) →
// CommissioningComplete。failsafe は明示 disarm しない——失敗時は 120s の
// 期限切れに任せる（spec 決定 6: 中断ハンドラを持たない一発フロー）。

/// commissioning 対象デバイスの指定方法。
pub enum CommissionTarget {
    /// アドレス既知（ローカル E2E、または呼び出し側が別途探索済み）。
    Addr(SocketAddr),
    /// `_matterc` browse で long discriminator から探索する。
    Discriminator(u16),
}

/// `commission_on_network` の入力一式。
pub struct CommissionParams<'a> {
    pub passcode: u32,
    pub target: CommissionTarget,
    pub device_node_id: u64,
    /// PAA 信頼ストアのディレクトリ（`*.der`）。`None` は「PAA なし」——
    /// attestation チェーン検証は必ず失敗する（PAA 必須運用、警告なしで
    /// 弱めない）。
    pub paa_dir: Option<&'a std::path::Path>,
    /// CD signer 証明書ストアのディレクトリ。CD 検証は warn のみ（spec
    /// 決定どおり戻り値には影響しない）ので `None` でも commissioning 自体
    /// は続行できる。
    pub cd_signer_dir: Option<&'a std::path::Path>,
    /// mDNS / link-local アドレス用の interface index。`Addr` 直指定なら
    /// リンクローカルでなければ `0` で構わない。
    pub scope_id: u32,
}

/// commissioning 完了後のデバイス。
pub struct CommissionedDevice {
    pub node_id: u64,
    pub fabric_index: Option<u8>,
    /// 新 fabric 上の operational CASE セッション（CommissioningComplete
    /// 送信済み）。呼び出し側はこれをそのまま以後の操作に使い回せる。
    pub session: SecureSession,
}

/// `InvokeResponseData` から command fields TLV を取り出す。応答の
/// `status` が非ゼロならその時点で `CommandStatus` に、fields が無ければ
/// `Malformed` にする。
fn fields_of<'a>(
    step: &'static str,
    resp: &'a im::InvokeResponseData,
) -> Result<&'a [u8], CommissionError> {
    if resp.status != 0 {
        return Err(CommissionError::CommandStatus {
            step,
            code: resp.status,
        });
    }
    resp.fields_tlv
        .as_deref()
        .ok_or(CommissionError::Malformed {
            step,
            detail: "no command fields",
        })
}

/// `{0: errorCode, 1: debugText}` 型（ArmFailSafeResponse などの共通形）の
/// 応答を検査し、`errorCode` が非ゼロなら `CommandStatus` にする。
fn check_commissioning_response(
    step: &'static str,
    resp: &im::InvokeResponseData,
) -> Result<(), CommissionError> {
    let (code, _text) = decode_commissioning_status_response(fields_of(step, resp)?)?;
    if code != 0 {
        return Err(CommissionError::CommandStatus { step, code });
    }
    Ok(())
}

/// CertificateChainRequest を送って証明書 DER を取り出す（DAC / PAI 共
/// 通、`cert_type` で切り替え）。
async fn request_cert(
    step: &'static str,
    session: &mut SecureSession,
    cert_type: u8,
    cfg: &MrpConfig,
) -> Result<Vec<u8>, CommissionError> {
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CERT_CHAIN_REQUEST,
            Some(&encode_cert_chain_request(cert_type)),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    decode_cert_chain_response(fields_of(step, &resp)?)
}

/// 共有ステップ 3〜7（ArmFailSafe → SetRegulatoryConfig(任意) →
/// attestation(厳格) → CSR → NOC 発行 → AddTrustedRootCertificate →
/// AddNOC）。`commission_on_network`（PASE over UDP）と
/// `commission_btp_thread`（PASE over BTP）の両方から呼ばれる — セッション
/// が UDP か Reliable(BTP) かに関わらず同一（`SecureSession` はどちらの
/// transport の上でも同じ invoke インタフェースを持つ）。戻り値は AddNOC が
/// 返した fabric index（spec 上 optional）。
async fn run_credential_steps(
    session: &mut SecureSession,
    fabric: &CommissioningFabric,
    device_node_id: u64,
    paa_dir: Option<&std::path::Path>,
    cd_signer_dir: Option<&std::path::Path>,
    cfg: &MrpConfig,
) -> Result<Option<u8>, CommissionError> {
    let challenge = session.attestation_challenge();

    // 3. ArmFailSafe(120s)（必須）。
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            Some(&encode_arm_fail_safe(120, 1)),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    check_commissioning_response("arm-fail-safe", &resp)?;

    // 4. SetRegulatoryConfig（任意 — spec 決定 7: 失敗は warn で続行）。
    match session
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_SET_REGULATORY_CONFIG,
            Some(&encode_set_regulatory_config(2, "XX", 2)),
            None,
            cfg,
        )
        .await
    {
        Ok(resp) => {
            if let Err(e) = check_commissioning_response("set-regulatory", &resp) {
                tracing::warn!(error = %e, "SetRegulatoryConfig rejected — continuing");
            }
        }
        Err(e) => tracing::warn!(error = %e, "SetRegulatoryConfig failed — continuing"),
    }

    // 5. attestation（厳格）。
    let mut nonce = [0u8; 32];
    getrandom::getrandom(&mut nonce).expect("os rng");
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ATTESTATION_REQUEST,
            Some(&encode_attestation_request(&nonce)),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (elements, att_sig) = decode_attestation_response(fields_of("attestation", &resp)?)?;
    let dac = request_cert("dac", session, CERT_TYPE_DAC, cfg).await?;
    let pai = request_cert("pai", session, CERT_TYPE_PAI, cfg).await?;
    let paa = match paa_dir {
        Some(d) => attestation::load_der_dir(d).map_err(CommissionError::Attestation)?,
        None => Vec::new(), // 空 → チェーン検証は必ず失敗する（PAA 必須運用）
    };
    let cd_signers = match cd_signer_dir {
        Some(d) => attestation::load_der_dir(d).map_err(CommissionError::Attestation)?,
        None => Vec::new(),
    };
    attestation::verify_device_attestation(
        &dac,
        &pai,
        &paa,
        &cd_signers,
        &elements,
        &att_sig,
        &nonce,
        &challenge,
    )
    .map_err(CommissionError::Attestation)?;

    // 6. CSR → NOC 発行。
    let mut csr_nonce = [0u8; 32];
    getrandom::getrandom(&mut csr_nonce).expect("os rng");
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            Some(&encode_csr_request(&csr_nonce)),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (nocsr_elements, nocsr_sig) = decode_csr_response(fields_of("csr", &resp)?)?;
    // NOCSR 署名も DAC 鍵で elements||challenge に対して（spec §11.17.5.6）。
    {
        let dac_cert = x509::parse_x509(&dac).map_err(|_| CommissionError::Csr("dac reparse"))?;
        let mut msg = nocsr_elements.clone();
        msg.extend_from_slice(&challenge);
        crypto::verify_ecdsa_p256(&dac_cert.public_key, &msg, &nocsr_sig)
            .map_err(|_| CommissionError::Csr("nocsr signature"))?;
    }
    let (csr_der, returned_nonce) = parse_nocsr_elements(&nocsr_elements)?;
    if returned_nonce != csr_nonce {
        return Err(CommissionError::Csr("csr nonce mismatch"));
    }
    let device_pub = x509::parse_csr(&csr_der).map_err(|_| CommissionError::Csr("csr parse"))?;
    let noc_tlv = fabric.issue_device_noc(&device_pub, device_node_id)?;

    // 7. AddTrustedRootCertificate → AddNOC。
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_TRUSTED_ROOT,
            Some(&encode_add_trusted_root(&fabric.rcac_tlv)),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    if resp.status != 0 {
        return Err(CommissionError::CommandStatus {
            step: "add-trusted-root",
            code: resp.status,
        });
    }
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            Some(&encode_add_noc(
                &noc_tlv,
                &fabric.ipk_epoch,
                fabric.admin_node_id,
                0xFFF1,
            )),
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (noc_status, fabric_index) = decode_noc_response(fields_of("add-noc", &resp)?)?;
    if noc_status != 0 {
        return Err(CommissionError::Noc(noc_status));
    }

    Ok(fabric_index)
}

/// 共有ステップ 8〜9（新 fabric で CASE（リトライ）→ CommissioningComplete）。
/// `commission_on_network` / `commission_btp_thread` の双方とも、資格情報
/// ステップ完了後は同一アドレスへ operational discovery 抜きで直接 CASE を
/// 試みる（PASE と同一アドレスなら再解決不要。BTP 経由の場合は呼び出し側
/// が mDNS operational discovery 済みのアドレスを渡す）。
///
/// 実装ノート（brief からの適応）: brief は第 1 引数を `Arc<UdpTransport>`
/// と書いていたが、`case::establish` 自体が `Arc<Transport>` を取るため
/// ここでも `Arc<Transport>` を直接受ける — 呼び出し側で
/// `Arc<UdpTransport>` を都度 `Transport::Udp` に包んでから渡す一段階の
/// 手間を省く。戻り値も `(SecureSession, ())` ではなく `SecureSession` のみ
/// ——fabric index は `run_credential_steps` の戻り値であり、呼び出し側が
/// 組み立てる `CommissionedDevice` の方で合流させる。
async fn operational_case_and_complete(
    transport: Arc<Transport>,
    peer: SocketAddr,
    fabric: &CommissioningFabric,
    device_node_id: u64,
    cfg: &MrpConfig,
) -> Result<SecureSession, CommissionError> {
    // 8. 新 fabric で CASE（同一アドレスへ直接。AddNOC 直後は fabric 起動待
    //    ちが必要なことがあるためリトライ、全体 ~30s / failsafe 120s 内）。
    let creds = fabric.admin_credentials()?;
    let mut session = None;
    let mut last = None;
    for _ in 0..6 {
        match case::establish(Arc::clone(&transport), peer, &creds, device_node_id, cfg).await {
            Ok(s) => {
                session = Some(s);
                break;
            }
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    let mut session =
        session.ok_or_else(|| CommissionError::Case(last.expect("at least one try")))?;

    // 9. CommissioningComplete（CASE 上で）。
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            None,
            None,
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    check_commissioning_response("commissioning-complete", &resp)?;

    Ok(session)
}

/// on-network commissioning のステップマシン本体（spec §5.5 全体フロー）。
///
/// `fabric` はこのコミッショニングだけで使い捨てる第二 fabric（呼び出し側
/// が事前に [`CommissioningFabric::generate`] しておく）。成功すると新
/// fabric 上の CASE セッションを持つ [`CommissionedDevice`] を返す。
pub async fn commission_on_network(
    transport: Arc<UdpTransport>,
    fabric: &CommissioningFabric,
    params: CommissionParams<'_>,
) -> Result<CommissionedDevice, CommissionError> {
    // M6b: 内部の pase/case は `Arc<Transport>` を取る（BTP 対応の土台）。
    // 公開シグネチャは既存呼び出し側（M6a）互換のため `Arc<UdpTransport>` の
    // まま維持し、ここで一度だけ wrap する。
    let transport: Arc<Transport> = Arc::new(Transport::Udp(Arc::clone(&transport)));

    // 1. ターゲット解決。
    let (peer, cfg) = match params.target {
        CommissionTarget::Addr(a) => (a, MrpConfig::default()),
        CommissionTarget::Discriminator(d) => {
            let node = dnssd::resolve_commissionable(params.scope_id, d, Duration::from_secs(15))
                .await
                .map_err(CommissionError::Discovery)?;
            let addr = node
                .socket_addrs(params.scope_id)
                .into_iter()
                .next()
                .ok_or(CommissionError::Timeout("no usable address"))?;
            (addr, node.mrp_config())
        }
    };

    // 2. PASE。
    let mut pase = pase::establish(Arc::clone(&transport), peer, params.passcode, &cfg)
        .await
        .map_err(CommissionError::Pase)?;

    // 3〜7. 資格情報ステップ（run_credential_steps に集約——M6b Task6）。
    let fabric_index = run_credential_steps(
        &mut pase,
        fabric,
        params.device_node_id,
        params.paa_dir,
        params.cd_signer_dir,
        &cfg,
    )
    .await?;

    // 8〜9. CASE リトライ + CommissioningComplete（operational_case_and_complete
    // に集約——M6b Task6）。
    let session = operational_case_and_complete(
        Arc::clone(&transport),
        peer,
        fabric,
        params.device_node_id,
        &cfg,
    )
    .await?;

    Ok(CommissionedDevice {
        node_id: params.device_node_id,
        fabric_index,
        session,
    })
}

// --- BTP/BLE Thread commissioning（M6b Task6） ---

/// `commission_ble_thread` / `commission_btp_thread` の入力一式。
pub struct BleThreadParams<'a> {
    pub passcode: u32,
    pub discriminator: u16,
    /// OTBR の active operational dataset（Thread TLV 生バイト列）。
    pub thread_dataset: &'a [u8],
    pub device_node_id: u64,
    pub paa_dir: Option<&'a std::path::Path>,
    pub cd_signer_dir: Option<&'a std::path::Path>,
    /// Thread 参加後の operational mDNS 発見に使う iface index。
    pub scope_id: u32,
}

/// BTP リンク上で工場出荷デバイスを commission する（spec M6b 決定 3）。
///
/// リンク（[`crate::btp::GattLink`]）は確立済みで渡される——BLE スキャン /
/// 接続は `commission_ble_thread`（feature "ble"）が行う。この関数自体は
/// cfg なし・モックの `GattLink` だけでテストできる（`tests/btp_pase_plumbing.rs`
/// が実際に PASE の最初のメッセージまで通す）。
pub async fn commission_btp_thread(
    link: crate::btp::GattLink,
    fabric: &CommissioningFabric,
    params: BleThreadParams<'_>,
) -> Result<CommissionedDevice, CommissionError> {
    let cfg = MrpConfig::default();
    let xpan = thread_ext_pan_id(params.thread_dataset).ok_or(CommissionError::Malformed {
        step: "thread-dataset",
        detail: "no extended pan id (type 2) in dataset",
    })?;

    // 1. BTP handshake → Transport::Reliable。
    let (btp_params, transport) = crate::btp::connect(link, crate::btp::PROPOSED_WINDOW)
        .await
        .map_err(|e| CommissionError::Ble {
            step: "btp-handshake",
            detail: e.to_string(),
        })?;
    tracing::info!(
        segment = btp_params.segment_size,
        window = btp_params.window_size,
        "btp session established"
    );
    let transport = Arc::new(transport);

    // 2. PASE over BTP（Reliable transport 上では MRP は自動 off——
    //    宛先は擬似 peer の `transport::RELIABLE_PEER`）。
    let mut pase = pase::establish(
        Arc::clone(&transport),
        crate::transport::RELIABLE_PEER,
        params.passcode,
        &cfg,
    )
    .await
    .map_err(CommissionError::Pase)?;

    // 3〜7. 資格情報ステップ（on-network commissioning と共通）。
    let fabric_index = run_credential_steps(
        &mut pase,
        fabric,
        params.device_node_id,
        params.paa_dir,
        params.cd_signer_dir,
        &cfg,
    )
    .await?;

    // 4. Thread dataset 書き込み（AddOrUpdateThreadNetwork, breadcrumb=5）。
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_NETWORK_COMMISSIONING,
            CMD_ADD_OR_UPDATE_THREAD,
            Some(&encode_add_or_update_thread_network(
                params.thread_dataset,
                5,
            )),
            None,
            &cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (status, text) = decode_network_config_response(fields_of("add-thread-network", &resp)?)?;
    if status != 0 {
        return Err(CommissionError::NetworkConfig {
            step: "add-thread-network",
            status,
            debug_text: text,
        });
    }

    // 5. failsafe 仕切り直し（spec 決定 5: Thread 参加 + operational 発見が
    //    120s を超えないよう ConnectNetwork 直前に再アーム、breadcrumb=6）。
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            Some(&encode_arm_fail_safe(120, 6)),
            None,
            &cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    check_commissioning_response("re-arm-fail-safe", &resp)?;

    // 6. ConnectNetwork（Thread join は遅い——応答待ちだけ長い budget で。
    //    breadcrumb=7）。
    let connect_cfg = MrpConfig {
        initial_interval: Duration::from_secs(60),
        max_retries: 0,
        backoff: 1.0,
    };
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_NETWORK_COMMISSIONING,
            CMD_CONNECT_NETWORK,
            Some(&encode_connect_network(&xpan, 7)),
            None,
            &connect_cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (status, text) = decode_connect_network_response(fields_of("connect-network", &resp)?)?;
    if status != 0 {
        return Err(CommissionError::NetworkConfig {
            step: "connect-network",
            status,
            debug_text: text,
        });
    }

    // 7. BTP 経路を手放す（BleConnection の切断は呼び出し側の責務）。以後は IP。
    drop(pase);
    drop(transport);

    // 8. operational 発見（リトライ）→ CASE → CommissioningComplete。
    let rcac = MatterCert::parse(&fabric.rcac_tlv)?;
    let cfid = fabric::compressed_fabric_id(&rcac.pub_key, fabric.fabric_id);
    let mut resolved = None;
    let mut last_err = None;
    for _ in 0..12 {
        match dnssd::resolve_operational(
            params.scope_id,
            &cfid,
            params.device_node_id,
            Duration::from_secs(5),
        )
        .await
        {
            Ok(node) => {
                resolved = Some(node);
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    let node = resolved.ok_or_else(|| {
        tracing::warn!(error = ?last_err, "operational advertise did not appear");
        CommissionError::Timeout("operational discovery after thread join")
    })?;
    let addr = node
        .socket_addrs(params.scope_id)
        .into_iter()
        .next()
        .ok_or(CommissionError::Timeout("no usable operational address"))?;
    let udp = UdpTransport::bind()
        .await
        .map_err(|e| CommissionError::Ble {
            step: "udp-bind",
            detail: e.to_string(),
        })?;
    let session = operational_case_and_complete(
        Arc::new(Transport::Udp(Arc::new(udp))),
        addr,
        fabric,
        params.device_node_id,
        &node.mrp_config(),
    )
    .await?;

    Ok(CommissionedDevice {
        node_id: params.device_node_id,
        fabric_index,
        session,
    })
}

/// BLE スキャンから完了までの一括フロー（実機用）。scan → GATT 接続 →
/// [`commission_btp_thread`] へ委譲 → 成否に関わらず GATT を切断する
/// （[`crate::ble::BleConnection`] は drop だけでは切断されない——ドキュメ
/// ント参照）。そのため結果は切断より前に確保しておく。
#[cfg(feature = "ble")]
pub async fn commission_ble_thread(
    fabric: &CommissioningFabric,
    params: BleThreadParams<'_>,
) -> Result<CommissionedDevice, CommissionError> {
    let ble_err = |step: &'static str| {
        move |e: crate::btp::BtpError| CommissionError::Ble {
            step,
            detail: e.to_string(),
        }
    };
    let session = bluer::Session::new()
        .await
        .map_err(|e| CommissionError::Ble {
            step: "bluez-session",
            detail: e.to_string(),
        })?;
    let adapter = session
        .default_adapter()
        .await
        .map_err(|e| CommissionError::Ble {
            step: "adapter",
            detail: e.to_string(),
        })?;
    let device =
        crate::ble::find_commissionable(&adapter, params.discriminator, Duration::from_secs(30))
            .await
            .map_err(ble_err("scan"))?;
    let (link, conn) = crate::ble::open_link(&device)
        .await
        .map_err(ble_err("gatt"))?;
    let result = commission_btp_thread(link, fabric, params).await;
    conn.disconnect().await; // 成否に関わらず GATT を畳む
    result
}

// --- open-window（Task 10） ---

/// spec §5.1.3.1 の「trivial/attack-prone」として禁止される setup passcode
/// の一覧。native window open では毎回ランダムに引き直して避ける。
pub const INVALID_PASSCODES: [u32; 12] = [
    0, 11111111, 22222222, 33333333, 44444444, 55555555, 66666666, 77777777, 88888888, 99999999,
    12345678, 87654321,
];

/// `1..=99_999_998` の範囲かつ [`INVALID_PASSCODES`] に含まれない passcode
/// を引き直しながら返す。
fn random_valid_passcode() -> u32 {
    loop {
        let mut b = [0u8; 4];
        getrandom::getrandom(&mut b).expect("os rng");
        let candidate = u32::from_le_bytes(b) % 99_999_998 + 1; // 1..=99_999_998
        if !INVALID_PASSCODES.contains(&candidate) {
            return candidate;
        }
    }
}

/// [`open_commissioning_window`] が生成した一時 window の設定コード一式。
pub struct OpenedWindow {
    pub passcode: u32,
    /// 12-bit discriminator。
    pub discriminator: u16,
    pub manual_code: String,
    pub qr_payload: String,
    pub window_timeout_s: u16,
}

/// Enhanced Commissioning Method（spec §5.5）で一時的な commissioning
/// window を開く。既存の operational CASE セッション上で
/// `AdministratorCommissioning::OpenCommissioningWindow` を送る——PASE は使
/// わない（対象デバイスは既にこの fabric にコミッショニング済み）。
pub async fn open_commissioning_window(
    session: &mut SecureSession,
    timeout_s: u16,
    cfg: &MrpConfig,
) -> Result<OpenedWindow, CommissionError> {
    let passcode = random_valid_passcode();
    let mut disc_b = [0u8; 2];
    getrandom::getrandom(&mut disc_b).expect("os rng");
    let discriminator = u16::from_le_bytes(disc_b) & 0x0FFF;
    let mut salt = [0u8; 32];
    getrandom::getrandom(&mut salt).expect("os rng");
    let iterations = 1000u32; // spec §3.9: PBKDF_MINIMUM=1000
    let verifier = crate::spake2p::compute_verifier(passcode, &salt, iterations);
    // OpenCommissioningWindow は timed invoke 必須（spec §11.19.8.1）。
    let resp = session
        .invoke_for_data(
            0,
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            Some(&encode_open_commissioning_window(
                timeout_s,
                &verifier,
                discriminator,
                iterations,
                &salt,
            )),
            Some(10_000),
            cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    if resp.status != 0 {
        return Err(CommissionError::CommandStatus {
            step: "open-window",
            code: resp.status,
        });
    }
    Ok(OpenedWindow {
        passcode,
        discriminator,
        manual_code: build_manual_code(passcode, discriminator),
        qr_payload: build_window_qr(passcode, discriminator),
        window_timeout_s: timeout_s,
    })
}

fn build_manual_code(passcode: u32, discriminator12: u16) -> String {
    crate::setup_code::encode_manual_code(passcode, (discriminator12 >> 8) as u8)
}

fn build_window_qr(passcode: u32, discriminator12: u16) -> String {
    crate::setup_code::encode_qr(&crate::setup_code::SetupPayload {
        version: 0,
        vendor_id: 0, // ECM window の QR は VID/PID 不定で良い
        product_id: 0,
        custom_flow: 0,
        discovery_capabilities: 0x04, // on-network
        discriminator: discriminator12,
        passcode,
    })
}

// --- 使い捨て第二 fabric ---

/// 使い捨て第二 fabric の素材（spec 決定 4: 永続化しない、呼び出し側が
/// 生成して持つ）。1 回のコミッショニングの寿命だけ生きる root 証明書 +
/// root 秘密鍵 + epoch IPK。`Drop` 時に KVS には一切書かれない——このフロー
/// が使う fabric は commissioning が終わった時点で controller のプロセス
/// メモリ上にしか存在しない使い捨てである。
pub struct CommissioningFabric {
    pub rcac_tlv: Vec<u8>,
    root_private_key: [u8; 32],
    pub fabric_id: u64,
    pub ipk_epoch: [u8; 16],
    pub admin_node_id: u64,
}

/// 手動 `Debug`: root 秘密鍵と epoch IPK はどちらも秘匿情報。
/// `fabric::FabricCredentials` の redaction 方針を踏襲する——このリポジトリ
/// は公開なので、エラー文脈やテスト失敗出力での `{:?}` 経由の意図しない
/// 漏洩を避ける。
impl std::fmt::Debug for CommissioningFabric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommissioningFabric")
            .field("rcac_tlv_len", &self.rcac_tlv.len())
            .field("root_private_key", &"[REDACTED]")
            .field("fabric_id", &self.fabric_id)
            .field("ipk_epoch", &"[REDACTED]")
            .field("admin_node_id", &self.admin_node_id)
            .finish()
    }
}

impl CommissioningFabric {
    /// 新しい self-signed RCAC と 16 バイトの epoch IPK を生成する。
    pub fn generate(fabric_id: u64, admin_node_id: u64) -> Result<Self, CommissionError> {
        let (rcac, root_private_key) = cert::generate_rcac()?;
        let mut ipk_epoch = [0u8; 16];
        getrandom::getrandom(&mut ipk_epoch).map_err(|_| CommissionError::Malformed {
            step: "commissioning_fabric_generate",
            detail: "os rng failure",
        })?;
        Ok(Self {
            rcac_tlv: rcac.to_tlv(),
            root_private_key,
            fabric_id,
            ipk_epoch,
            admin_node_id,
        })
    }

    /// controller 自身の CASE 用 credentials（NOC 自己発行を再利用）。
    ///
    /// AddNOC でデバイスに渡す IPK は **epoch** 側（`self.ipk_epoch`、
    /// fabric 全体で共有される groupKeySet の鍵そのもの）。対して CASE の
    /// destination id 計算に使うのは **operational** 側——
    /// `fabric::derive_ipk_operational(&self.ipk_epoch, &cfid)` で epoch か
    /// ら導出する別物で、`FabricCredentials` にはこちらを積む。取り違える
    /// と CASE の宛先 id 計算がデバイス側と食い違う。
    pub fn admin_credentials(&self) -> Result<FabricCredentials, CommissionError> {
        let rcac = MatterCert::parse(&self.rcac_tlv)?;
        let cfid = fabric::compressed_fabric_id(&rcac.pub_key, self.fabric_id);
        let ipk_operational = fabric::derive_ipk_operational(&self.ipk_epoch, &cfid);
        let materials = SelfIssueMaterials {
            rcac: self.rcac_tlv.clone(),
            root_private_key: self.root_private_key,
            ipk_operational,
            node_id: self.admin_node_id,
            fabric_id: self.fabric_id,
        };
        Ok(FabricCredentials::from_self_issued(materials)?)
    }

    /// CSR の公開鍵にデバイス NOC を発行して TLV で返す。
    pub fn issue_device_noc(
        &self,
        op_public_key: &[u8; 65],
        node_id: u64,
    ) -> Result<Vec<u8>, CommissionError> {
        let rcac = MatterCert::parse(&self.rcac_tlv)?;
        let mut serial = [0u8; 8];
        getrandom::getrandom(&mut serial).map_err(|_| CommissionError::Malformed {
            step: "issue_device_noc",
            detail: "os rng failure",
        })?;
        serial[0] &= 0x7F; // BER INTEGER の最小正表現を維持
        let noc = cert::issue_noc(
            op_public_key,
            node_id,
            self.fabric_id,
            &rcac,
            &self.root_private_key,
            &serial,
        )?;
        Ok(noc.to_tlv())
    }
}

// --- errors ---

/// commissioning フロー全体のエラー。呼び出し側（CLI 層）は spec 決定 5 の
/// 対応表でこれを `ErrorKind` / exit code にマップする:
///
/// | variant                                             | kind               | exit |
/// |------------------------------------------------------|--------------------|------|
/// | `Attestation(_)` / `Pase(PaseError::ConfirmMismatch)` | `device_rejected`  | 4    |
/// | `Discovery(_)` / `Timeout(_)`                         | `timeout`          | 3    |
/// | `Case(_)`                                             | `session_failed`   | 6    |
/// | 上記以外すべて（`Pase` の非 ConfirmMismatch variant、  | `commission_failed`| 1    |
/// | `Csr` / `Noc` / `CommandStatus` / `Malformed` /       |                    |      |
/// | `Cert` / `Fabric` / `Session`）                       |                    |      |
#[derive(Debug)]
pub enum CommissionError {
    Discovery(crate::dnssd::DnssdError),
    Pase(crate::pase::PaseError),
    Session(crate::session::SessionError),
    Attestation(crate::attestation::AttestationError),
    Csr(&'static str),
    /// NOCResponse の statusCode が成功 (0) でない。
    Noc(u8),
    /// `*CommissioningResponse` の errorCode が成功 (0) でない。
    CommandStatus {
        step: &'static str,
        code: u8,
    },
    /// TLV の形が期待と違う（欠落フィールド・型不一致など）。
    Malformed {
        step: &'static str,
        detail: &'static str,
    },
    Cert(crate::cert::CertError),
    Fabric(crate::fabric::FabricError),
    Case(crate::case::CaseError),
    Timeout(&'static str),
    /// BLE / BTP 層の失敗（scan / connect / gatt / btp）。
    Ble {
        step: &'static str,
        detail: String,
    },
    /// NetworkCommissioning 応答の NetworkingStatus が成功 (0) でない。
    NetworkConfig {
        step: &'static str,
        status: u8,
        debug_text: Option<String>,
    },
}

impl std::fmt::Display for CommissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommissionError::Discovery(e) => write!(f, "commissioning: discovery error: {e}"),
            CommissionError::Pase(e) => write!(f, "commissioning: pase error: {e}"),
            CommissionError::Session(e) => write!(f, "commissioning: session error: {e}"),
            CommissionError::Attestation(e) => write!(f, "commissioning: attestation error: {e}"),
            CommissionError::Csr(msg) => write!(f, "commissioning: csr error: {msg}"),
            CommissionError::Noc(code) => {
                write!(f, "commissioning: NOCResponse status 0x{code:02X}")
            }
            CommissionError::CommandStatus { step, code } => {
                write!(f, "commissioning: {step} errorCode 0x{code:02X}")
            }
            CommissionError::Malformed { step, detail } => {
                write!(f, "commissioning: {step}: malformed ({detail})")
            }
            CommissionError::Cert(e) => write!(f, "commissioning: certificate error: {e}"),
            CommissionError::Fabric(e) => write!(f, "commissioning: fabric error: {e}"),
            CommissionError::Case(e) => write!(f, "commissioning: case error: {e}"),
            CommissionError::Timeout(step) => write!(f, "commissioning: timeout ({step})"),
            CommissionError::Ble { step, detail } => {
                write!(f, "commissioning: ble {step}: {detail}")
            }
            CommissionError::NetworkConfig {
                step,
                status,
                debug_text,
            } => {
                write!(f, "commissioning: {step} NetworkingStatus 0x{status:02X}")?;
                if let Some(t) = debug_text {
                    write!(f, " ({t})")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for CommissionError {}

impl From<crate::dnssd::DnssdError> for CommissionError {
    fn from(e: crate::dnssd::DnssdError) -> Self {
        CommissionError::Discovery(e)
    }
}

impl From<crate::pase::PaseError> for CommissionError {
    fn from(e: crate::pase::PaseError) -> Self {
        CommissionError::Pase(e)
    }
}

impl From<crate::session::SessionError> for CommissionError {
    fn from(e: crate::session::SessionError) -> Self {
        CommissionError::Session(e)
    }
}

impl From<crate::attestation::AttestationError> for CommissionError {
    fn from(e: crate::attestation::AttestationError) -> Self {
        CommissionError::Attestation(e)
    }
}

impl From<crate::cert::CertError> for CommissionError {
    fn from(e: crate::cert::CertError) -> Self {
        CommissionError::Cert(e)
    }
}

impl From<crate::fabric::FabricError> for CommissionError {
    fn from(e: crate::fabric::FabricError) -> Self {
        CommissionError::Fabric(e)
    }
}

impl From<crate::case::CaseError> for CommissionError {
    fn from(e: crate::case::CaseError) -> Self {
        CommissionError::Case(e)
    }
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
    fn commissioning_fabric_issues_valid_credentials() {
        let fab = CommissioningFabric::generate(0xFAB1, 0x1_0001).unwrap();
        let creds = fab.admin_credentials().unwrap();
        assert_eq!(creds.fabric_id, 0xFAB1);
        assert_eq!(creds.node_id, 0x1_0001);
        // デバイス NOC も同じ root でチェーン検証が通る
        let dev = crate::case::random_p256_secret();
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let dev_pub: [u8; 65] = dev
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .unwrap();
        let noc_tlv = fab.issue_device_noc(&dev_pub, 0x2_0001).unwrap();
        let noc = crate::cert::MatterCert::parse(&noc_tlv).unwrap();
        let rcac = crate::cert::MatterCert::parse(&fab.rcac_tlv).unwrap();
        crate::cert::verify_noc_chain(&noc, None, &rcac).unwrap();
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

    // --- Task 10: open-window の純関数部分（フロー全体は Task 11 のライブ
    // E2E が検証する — ここでは PASE/CASE を伴わない部分だけを unit test する）。

    #[test]
    fn random_passcode_is_valid() {
        for _ in 0..64 {
            let p = random_valid_passcode();
            assert!((1..=99_999_998).contains(&p));
            assert!(!INVALID_PASSCODES.contains(&p));
        }
    }

    #[test]
    fn opened_window_setup_codes_are_consistent() {
        let w = OpenedWindow {
            passcode: 20202021,
            discriminator: 3840,
            manual_code: build_manual_code(20202021, 3840),
            qr_payload: build_window_qr(20202021, 3840),
            window_timeout_s: 180,
        };
        let m = crate::setup_code::parse_manual_code(&w.manual_code).unwrap();
        assert_eq!(m.passcode, 20202021);
        assert_eq!(u16::from(m.short_discriminator), 3840 >> 8);
        let q = crate::setup_code::parse_qr(&w.qr_payload).unwrap();
        assert_eq!(q.passcode, 20202021);
        assert_eq!(q.discriminator, 3840);
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
}
