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

use crate::cert::{self, MatterCert};
use crate::fabric::{self, FabricCredentials};
use crate::kvs::SelfIssueMaterials;
use crate::tlv::{Element, Reader, Tag, Value, Writer};

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

/// ArmFailSafeResponse / SetRegulatoryConfigResponse / CommissioningComplete
/// Response（spec §11.10.6.3 / .5 / .7）共通の `{0: ErrorCode, 1: DebugText}`
/// 形。`DebugText` は spec 上 optional なので欠落時は空文字列にする。戻り値
/// は `(errorCode, debugText)`。
pub fn decode_commissioning_status_response(
    fields: &[u8],
) -> Result<(u8, String), CommissionError> {
    let step = "commissioning_status_response";
    let mut r = Reader::new(fields);
    expect_struct(&mut r, step)?;
    let mut error_code: Option<u8> = None;
    let mut debug_text = String::new();
    loop {
        let el = next_el(&mut r, step)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => {
                error_code = Some(u8::try_from(v).map_err(|_| CommissionError::Malformed {
                    step,
                    detail: "errorCode out of range",
                })?);
            }
            (Tag::Context(1), Value::Utf8(s)) => debug_text = s.to_string(),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r, step)?;
            }
            _ => {}
        }
    }
    let error_code = error_code.ok_or(CommissionError::Malformed {
        step,
        detail: "missing errorCode",
    })?;
    Ok((error_code, debug_text))
}

/// AttestationResponse（spec §11.17.6.8）: `{0: AttestationElements, 1:
/// AttestationSignature}`。戻り値は `(elements, signature)`。
pub fn decode_attestation_response(fields: &[u8]) -> Result<(Vec<u8>, [u8; 64]), CommissionError> {
    let step = "attestation_response";
    let mut r = Reader::new(fields);
    expect_struct(&mut r, step)?;
    let mut elements: Option<Vec<u8>> = None;
    let mut signature: Option<[u8; 64]> = None;
    loop {
        let el = next_el(&mut r, step)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Bytes(b)) => elements = Some(b.to_vec()),
            (Tag::Context(1), Value::Bytes(b)) => {
                signature = Some(b.try_into().map_err(|_| CommissionError::Malformed {
                    step,
                    detail: "signature length",
                })?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r, step)?;
            }
            _ => {}
        }
    }
    Ok((
        elements.ok_or(CommissionError::Malformed {
            step,
            detail: "missing elements",
        })?,
        signature.ok_or(CommissionError::Malformed {
            step,
            detail: "missing signature",
        })?,
    ))
}

/// CertificateChainResponse（spec §11.17.6.5）: `{0: Certificate}`（DAC ま
/// たは PAI の X.509 DER をそのまま bytes に包んだもの）。
pub fn decode_cert_chain_response(fields: &[u8]) -> Result<Vec<u8>, CommissionError> {
    let step = "cert_chain_response";
    let mut r = Reader::new(fields);
    expect_struct(&mut r, step)?;
    let mut certificate: Option<Vec<u8>> = None;
    loop {
        let el = next_el(&mut r, step)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Bytes(b)) => certificate = Some(b.to_vec()),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r, step)?;
            }
            _ => {}
        }
    }
    certificate.ok_or(CommissionError::Malformed {
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
    let mut r = Reader::new(fields);
    expect_struct(&mut r, step)?;
    let mut elements: Option<Vec<u8>> = None;
    let mut signature: Option<[u8; 64]> = None;
    loop {
        let el = next_el(&mut r, step)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Bytes(b)) => elements = Some(b.to_vec()),
            (Tag::Context(1), Value::Bytes(b)) => {
                signature = Some(b.try_into().map_err(|_| CommissionError::Malformed {
                    step,
                    detail: "signature length",
                })?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r, step)?;
            }
            _ => {}
        }
    }
    Ok((
        elements.ok_or(CommissionError::Malformed {
            step,
            detail: "missing nocsr elements",
        })?,
        signature.ok_or(CommissionError::Malformed {
            step,
            detail: "missing signature",
        })?,
    ))
}

/// NOCSRElements（spec §11.17.6.10.1）: `{1: csr, 2: CSRNonce, 3/4: vendor
/// reserved(optional)}`。戻り値は `(csr_der, csr_nonce)`。vendor reserved
/// フィールドが付いていても無視する。
pub fn parse_nocsr_elements(elements: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CommissionError> {
    let step = "nocsr_elements";
    let mut r = Reader::new(elements);
    expect_struct(&mut r, step)?;
    let mut csr: Option<Vec<u8>> = None;
    let mut nonce: Option<Vec<u8>> = None;
    loop {
        let el = next_el(&mut r, step)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => csr = Some(b.to_vec()),
            (Tag::Context(2), Value::Bytes(b)) => nonce = Some(b.to_vec()),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r, step)?;
            }
            _ => {}
        }
    }
    Ok((
        csr.ok_or(CommissionError::Malformed {
            step,
            detail: "missing csr",
        })?,
        nonce.ok_or(CommissionError::Malformed {
            step,
            detail: "missing csr nonce",
        })?,
    ))
}

/// NOCResponse（spec §11.17.6.14, AddNOC / RemoveFabric 共通の応答）:
/// `{0: StatusCode, 1: FabricIndex(optional), 2: DebugText(optional)}`。戻
/// り値は `(statusCode, fabricIndex)`。
pub fn decode_noc_response(fields: &[u8]) -> Result<(u8, Option<u8>), CommissionError> {
    let step = "noc_response";
    let mut r = Reader::new(fields);
    expect_struct(&mut r, step)?;
    let mut status: Option<u8> = None;
    let mut fabric_index: Option<u8> = None;
    loop {
        let el = next_el(&mut r, step)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => {
                status = Some(u8::try_from(v).map_err(|_| CommissionError::Malformed {
                    step,
                    detail: "statusCode out of range",
                })?);
            }
            (Tag::Context(1), Value::Uint(v)) => {
                fabric_index = Some(u8::try_from(v).map_err(|_| CommissionError::Malformed {
                    step,
                    detail: "fabricIndex out of range",
                })?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r, step)?;
            }
            _ => {}
        }
    }
    Ok((
        status.ok_or(CommissionError::Malformed {
            step,
            detail: "missing statusCode",
        })?,
        fabric_index,
    ))
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
}
