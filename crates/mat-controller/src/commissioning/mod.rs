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

mod ble_thread;
mod codec;
mod commissioning_fabric;
mod device_codec;
mod flow;
mod tlv_fields;
mod window;

pub use ble_thread::*;
pub use codec::*;
pub use commissioning_fabric::*;
pub use device_codec::*;
pub use flow::*;
pub use window::*;

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
pub const CMD_UPDATE_FABRIC_LABEL: u32 = 0x09; // resp NOCResponse 0x08
pub const CMD_REMOVE_FABRIC: u32 = 0x0A; // resp NOCResponse 0x08
pub const CMD_ADD_TRUSTED_ROOT: u32 = 0x0B; // 応答は NOCResponse ではなく status

// --- Administrator Commissioning cluster (spec §11.19) ---

pub const CLUSTER_ADMIN_COMMISSIONING: u32 = 0x003C;
pub const CMD_OPEN_COMMISSIONING_WINDOW: u32 = 0x00; // timed 必須
pub const CMD_REVOKE_COMMISSIONING: u32 = 0x02; // timed 必須、フィールド無し

/// CertificateChainRequest の CertificateType（spec §11.17.6.4）: DAC。
pub const CERT_TYPE_DAC: u8 = 1;
/// CertificateChainRequest の CertificateType（spec §11.17.6.4）: PAI。
pub const CERT_TYPE_PAI: u8 = 2;

// --- Network Commissioning cluster (spec §11.9) ---

pub const CLUSTER_NETWORK_COMMISSIONING: u32 = 0x0031;
pub const CMD_ADD_OR_UPDATE_THREAD: u32 = 0x03; // resp NetworkConfigResponse 0x05
pub const CMD_CONNECT_NETWORK: u32 = 0x06; // resp ConnectNetworkResponse 0x07

// --- errors ---

/// commissioning フロー全体のエラー。呼び出し側 `mat-native::commission`
/// （M8c-1 Task4 `kind_of`）がこれを `ErrorKind` / exit code へ写像する:
///
/// | variant                                               | kind                | exit |
/// |--------------------------------------------------------|---------------------|------|
/// | `Timeout(_)` / `Pase(Exchange(Timeout))` /               | `timeout`           | 3    |
/// |   `Case(Exchange(Timeout))` / `Session(Timeout)`         |                     |      |
/// | `Attestation(_)` / `Noc(_)` / `CommandStatus { .. }` /   | `device_rejected`   | 4    |
/// |   `Pase(ConfirmMismatch)`（passcode 不一致）/            |                     |      |
/// |   `Pase(StatusReport)` / `Case(PeerStatus)` /            |                     |      |
/// |   `Case(Sigma2SignatureInvalid)` /                       |                     |      |
/// |   `Case(EstablishmentFailed)`（Sigma3 の拒否）           |                     |      |
/// | `NetworkConfig { .. }`                                   | `unreachable`       | 5    |
/// | `Malformed { .. }` / `Csr(_)`                            | `parse_error`       | 1    |
/// | `Discovery(_)`                                           | 常に PASE 前（`commission_on_network` 内の唯一の発生箇所は step 1 の対象 resolve のみ。BLE フローの PASE 後 discovery は `Timeout` に写る）—ワイヤ未接触なので `mat-native` 側で `unreachable` のハードエラーにし（M8c-3: chip-tool フォールバック撤去）、`kind_of` を経由しない | ―     |
/// | 上記以外すべて（`Pase` / `Case` / `Session` 等の上記で     | `commission_failed` | 1    |
/// | 分離した variant を除く、`Cert` / `Fabric` / `Ble` 等）     |                     |      |
///
/// `Ble { step: "scan", .. }` は `kind_of` を経由しない — `find_commissionable`
/// の空振り（デバイスが見えない）はワイヤ未接触なので、呼び出し側
/// (`mat-native::commission` の `ble_path`) が個別に検出して `unreachable`
/// のハードエラーにする（M8c-3: chip-tool フォールバック撤去）。BLE/BTP の
/// それ以外の失敗（`bluez-session` / `adapter` / `gatt` / `btp-handshake` /
/// `udp-bind`）は他の variant と同様 `commission_failed`。
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
    /// 呼び出し側引数の値域違反。デバイスへ invoke を送る前に検出する。
    InvalidArgument {
        what: &'static str,
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
            CommissionError::InvalidArgument { what } => {
                write!(f, "commissioning: invalid argument: {what}")
            }
        }
    }
}

impl std::error::Error for CommissionError {}

/// **注意（`mat-native` との結合）**: `CommissionError::Discovery(_)` は
/// `mat-native::commission::is_dead_end` で「デバイス側に状態が無い ＝ 別の
/// transport で再試行してよい」と分類される。したがって **PASE 成功後**の
/// コードで dnssd 呼び出しに `?` を書くと、その失敗が黙って経路リトライの
/// 対象になり、failsafe 中の機体を別経路で再駆動しうる。
/// 現状の post-PASE の operational 解決（下の `commission_ble_thread` 内、
/// 明示 `match` で `CommissionError::Timeout(_)` に畳んでいる箇所）は
/// この理由で `?` を使っていない。post-PASE に dnssd を足すときは同じ規律で。
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

    #[test]
    fn commission_error_display_names_each_variant() {
        let cases: Vec<(CommissionError, &str)> = vec![
            (CommissionError::Csr("bad csr"), "csr error: bad csr"),
            (CommissionError::Noc(0x0B), "NOCResponse status 0x0B"),
            (
                CommissionError::CommandStatus {
                    step: "arm",
                    code: 2,
                },
                "arm errorCode 0x02",
            ),
            (
                CommissionError::Malformed {
                    step: "s",
                    detail: "d",
                },
                "s: malformed (d)",
            ),
            (CommissionError::Timeout("resolve"), "timeout (resolve)"),
            (
                CommissionError::Ble {
                    step: "scan",
                    detail: "no adapter".into(),
                },
                "ble scan: no adapter",
            ),
            (
                CommissionError::NetworkConfig {
                    step: "connect",
                    status: 4,
                    debug_text: None,
                },
                "connect NetworkingStatus 0x04",
            ),
            (
                CommissionError::NetworkConfig {
                    step: "connect",
                    status: 4,
                    debug_text: Some("no route".into()),
                },
                "connect NetworkingStatus 0x04 (no route)",
            ),
            (
                CommissionError::InvalidArgument {
                    what: "discriminator",
                },
                "invalid argument: discriminator",
            ),
            (
                CommissionError::Case(crate::case::CaseError::Sigma2NotAcked),
                "case error:",
            ),
        ];
        for (err, needle) in cases {
            let text = err.to_string();
            assert!(text.starts_with("commissioning: "), "{text}");
            assert!(text.contains(needle), "{text} should contain {needle}");
        }
    }
}
