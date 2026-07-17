//! native commissioning（M8c-1）。setup code のパース・発見・経路選択
//! （mDNS → BLE）・ErrorKind 写像を担う薄いラッパー。プロトコル本体は
//! mat-controller（M6a `commission_on_network` / M6b `commission_ble_thread`）。
//!
//! フォールバック境界（spec 案A）: ワイヤ未接触（資材構築失敗・デバイス
//! 未発見）だけが `Unavailable` = chip-tool へフォールバック可。PASE 開始後
//! の失敗は Err — chip-tool での自動再実行は二重 commission を招くため
//! 呼び出し側でもフォールバックしないこと。

use mat_controller::commissioning::{
    self, CommissionError, CommissionParams, CommissionTarget, CommissioningFabric,
};
use mat_controller::dnssd;
use mat_controller::fabric;
use mat_controller::kvs;
use mat_controller::setup_code;
use mat_controller::transport::UdpTransport;
use mat_core::error::{ErrorKind, MatError};

use crate::NativeConfig;

/// [`commission`] への入力一式。
pub struct CommissionRequest {
    pub setup_code: String,
    pub device_node_id: u64,
    /// hex デコード済みの Thread active operational dataset（BLE 経路用）。
    pub thread_dataset: Option<Vec<u8>>,
    pub paa_dir: Option<std::path::PathBuf>,
    pub cd_signer_dir: Option<std::path::PathBuf>,
}

/// [`commission`] の結果。native が引き受けられなかった場合と成功の 2 値
/// ——PASE 開始後の失敗は `Err` に載る（この enum には現れない）。
pub enum CommissionAttempt {
    /// native で完了（成功）。
    Done,
    /// native では引き受けられない（資材構築失敗 / デバイス未発見 =
    /// ワイヤ未接触）。理由付き — 呼び出し側は warn を出して chip-tool へ
    /// フォールバックする。
    Unavailable(String),
}

/// setup code をパースした発見キー。QR は 12bit long、manual は 4bit short。
enum Code {
    Qr { passcode: u32, long: u16 },
    Manual { passcode: u32, short: u8 },
}

fn parse_code(s: &str) -> Result<Code, MatError> {
    if s.starts_with("MT:") {
        let p = setup_code::parse_qr(s)
            .map_err(|e| MatError::new(ErrorKind::Other, format!("invalid QR payload: {e}")))?;
        Ok(Code::Qr {
            passcode: p.passcode,
            long: p.discriminator,
        })
    } else {
        let m = setup_code::parse_manual_code(s)
            .map_err(|e| MatError::new(ErrorKind::Other, format!("invalid manual code: {e}")))?;
        Ok(Code::Manual {
            passcode: m.passcode,
            short: m.short_discriminator,
        })
    }
}

/// `CommissionError` → mat の `ErrorKind`（spec の写像。発見の空振り
/// （`Discovery`）は呼び出し側が `Unavailable` に写すためここには来ない）。
fn kind_of(e: &CommissionError) -> ErrorKind {
    match e {
        CommissionError::Timeout(_) => ErrorKind::Timeout,
        CommissionError::Attestation(_) => ErrorKind::DeviceRejected,
        CommissionError::Noc(_) | CommissionError::CommandStatus { .. } => {
            ErrorKind::DeviceRejected
        }
        CommissionError::NetworkConfig { .. } => ErrorKind::Unreachable,
        CommissionError::Malformed { .. } | CommissionError::Csr(_) => ErrorKind::ParseError,
        _ => ErrorKind::CommissionFailed,
    }
}

fn commission_error(e: CommissionError) -> MatError {
    MatError::new(kind_of(&e), format!("native commissioning failed: {e}"))
}

/// manual code の short discriminator で commissionable 一覧から一意に選ぶ。
/// 0 件 = `Ok(None)`（未発見 → フォールバック）、1 件 = `Ok(Some(_))`、2 件
/// 以上 = `Err`（曖昧 — chip-tool でも同じ曖昧さなのでフォールバックしない）。
fn pick_by_short_strict(
    list: &[dnssd::CommissionableInstance],
    short: u8,
) -> Result<Option<&dnssd::CommissionableInstance>, MatError> {
    let mut it = list
        .iter()
        .filter(|c| c.discriminator.is_some_and(|d| (d >> 8) as u8 == short));
    let first = it.next();
    if it.next().is_some() {
        return Err(MatError::new(
            ErrorKind::CommissionFailed,
            format!("ambiguous short discriminator {short}: multiple commissionable devices"),
        ));
    }
    Ok(first)
}

/// `pick_by_short_strict` の非曖昧ケースだけを見る薄いラッパー（0件→None,
/// 1件→Some）。`commission` 本体は曖昧検出も必要なので常に
/// `pick_by_short_strict` を直接呼ぶ — この関数はテスト専用のため
/// `cfg(test)`。
#[cfg(test)]
fn pick_by_short(
    list: &[dnssd::CommissionableInstance],
    short: u8,
) -> Option<&dnssd::CommissionableInstance> {
    pick_by_short_strict(list, short).ok().flatten()
}

pub async fn commission(
    cfg: &NativeConfig,
    req: &CommissionRequest,
) -> Result<CommissionAttempt, MatError> {
    let code = parse_code(&req.setup_code)?;

    // 資材構築（未接触 — 失敗は Unavailable = フォールバック可）。
    let scope_id = match dnssd::iface_index(&cfg.iface) {
        Ok(s) => s,
        Err(e) => return Ok(CommissionAttempt::Unavailable(format!("iface: {e}"))),
    };
    let alpha = cfg.store.join("chip_tool_config.alpha.ini");
    let main_ini = cfg.store.join("chip_tool_config.ini");
    let materials =
        match kvs::read_self_issue_materials(&alpha, &main_ini, cfg.fabric_index, cfg.issuer_index)
        {
            Ok(m) => m,
            Err(e) => return Ok(CommissionAttempt::Unavailable(format!("kvs: {e}"))),
        };
    // epoch IPK ガード（Task 2）: この fabric の epoch が chip-tool 既定
    // 定数であることを、KVS の導出済み operational との KDF 一致で検証。
    // 不一致（非 chip-tool fabric / IPK ローテーション済み）は native では
    // 引き受けない。root 公開鍵が要るため一度 `FabricCredentials` を組む
    // ——`kvs::SelfIssueMaterials` は既に `#[derive(Clone)]`
    // 済み（秘密鍵を持つ型に Clone をここで新規に足すわけではない）ので、
    // 安価な INI 再読みではなく `materials.clone()` で賄う。
    let creds = match fabric::FabricCredentials::from_self_issued(materials.clone()) {
        Ok(c) => c,
        Err(e) => return Ok(CommissionAttempt::Unavailable(format!("self-issue: {e}"))),
    };
    if !fabric::verify_default_ipk_epoch(
        &creds.root_public_key,
        creds.fabric_id,
        &creds.ipk_operational,
    ) {
        return Ok(CommissionAttempt::Unavailable(
            "fabric IPK is not the chip-tool default epoch; native commission unsupported until M8c-3".into(),
        ));
    }
    let commissioning_fabric =
        CommissioningFabric::from_materials(materials, fabric::CHIP_TOOL_DEFAULT_IPK_EPOCH);

    // 発見と経路選択（mDNS → BLE）。
    let (passcode, target) = match code {
        Code::Qr { passcode, long } => {
            match dnssd::resolve_commissionable(scope_id, long, std::time::Duration::from_secs(5))
                .await
            {
                Ok(_) => (passcode, CommissionTarget::Discriminator(long)),
                Err(dnssd::DnssdError::Timeout { .. }) => {
                    // mDNS に居ない → BLE を試す（ble ビルド + dataset 必須）。
                    return ble_path(&commissioning_fabric, req, passcode, long, scope_id).await;
                }
                Err(e) => return Ok(CommissionAttempt::Unavailable(format!("mdns: {e}"))),
            }
        }
        Code::Manual { passcode, short } => {
            let list = match dnssd::browse_commissionable(scope_id, dnssd::BROWSE_WINDOW).await {
                Ok(l) => l,
                Err(e) => return Ok(CommissionAttempt::Unavailable(format!("mdns: {e}"))),
            };
            match pick_by_short_strict(&list, short)? {
                Some(c) => {
                    let Some(addr) = c.addresses.first() else {
                        return Ok(CommissionAttempt::Unavailable(
                            "commissionable found but no address resolved".into(),
                        ));
                    };
                    let port = c.port.unwrap_or(5540);
                    let scope = if (addr.segments()[0] & 0xffc0) == 0xfe80 {
                        scope_id
                    } else {
                        0
                    };
                    (
                        passcode,
                        CommissionTarget::Addr(std::net::SocketAddr::V6(
                            std::net::SocketAddrV6::new(*addr, port, 0, scope),
                        )),
                    )
                }
                // manual code は BLE 経路なし（scan は 12bit 完全一致 —
                // BLE で commission したい場合は QR を使う）。
                None => {
                    return Ok(CommissionAttempt::Unavailable(
                        "not found via mDNS (manual code cannot use BLE; use the QR payload)"
                            .into(),
                    ))
                }
            }
        }
    };

    // UDP bind はローカルのエフェメラルポート取得のみ — ワイヤ未接触なので
    // 失敗は Unavailable（chip-tool フォールバック可）。on-network 実行は
    // ここから先（commission_on_network 呼び出し）が実ワイヤ接触で、そちらの
    // 失敗は Err。
    let transport = match UdpTransport::bind().await {
        Ok(t) => std::sync::Arc::new(t),
        Err(e) => return Ok(CommissionAttempt::Unavailable(format!("udp bind: {e}"))),
    };
    let dev = match commissioning::commission_on_network(
        transport,
        &commissioning_fabric,
        CommissionParams {
            passcode,
            target,
            device_node_id: req.device_node_id,
            paa_dir: req.paa_dir.as_deref(),
            cd_signer_dir: req.cd_signer_dir.as_deref(),
            scope_id,
        },
    )
    .await
    {
        Ok(d) => d,
        // 内部 resolve（PASE より前 — ワイヤ未接触）での空振り。事前 resolve
        // 成功後の狭い競合窓だが、規則どおり Unavailable = chip-tool
        // フォールバック可に倒す。
        Err(CommissionError::Discovery(e)) => {
            return Ok(CommissionAttempt::Unavailable(format!(
                "commissionable disappeared before PASE: {e}"
            )))
        }
        Err(other) => return Err(commission_error(other)),
    };
    tracing::info!(
        node_id = dev.node_id,
        fabric_index = ?dev.fabric_index,
        "commission executed (native on-network)"
    );
    Ok(CommissionAttempt::Done)
}

/// BLE 経路（feature "ble"）。scan の空振りは `commission_ble_thread` 内部で
/// 一意に `CommissionError::Ble { step: "scan", .. }` として表現される
/// ——実装（`crates/mat-controller/src/commissioning.rs` の
/// `commission_ble_thread`）は `ble::find_commissionable` の失敗だけを
/// `ble_err("scan")` で包み、それ以外の BLE/BTP 失敗（`bluez-session` /
/// `adapter` / `gatt` / `btp-handshake` / `udp-bind`）には別の `step` 文字列
/// を使う。この一意性が実装から直接確認できたため、マーカー文字列で
/// Err→Unavailable を往復させる小細工ではなく、返ってきた
/// `CommissionError` をそのまま match する（`live_commission_ble.rs` が
/// 検証する `find_commissionable` → `open_link` → `commission_btp_thread` の
/// 分離フローに立ち入る必要はない——`commission_ble_thread` が同じことを
/// 内部でやってくれる）。
#[cfg(feature = "ble")]
async fn ble_path(
    fabric: &CommissioningFabric,
    req: &CommissionRequest,
    passcode: u32,
    long: u16,
    scope_id: u32,
) -> Result<CommissionAttempt, MatError> {
    let Some(dataset) = req.thread_dataset.as_deref() else {
        return Ok(CommissionAttempt::Unavailable(
            "not found via mDNS and no --thread-dataset for the BLE path".into(),
        ));
    };
    match commissioning::commission_ble_thread(
        fabric,
        commissioning::BleThreadParams {
            passcode,
            discriminator: long,
            thread_dataset: dataset,
            device_node_id: req.device_node_id,
            paa_dir: req.paa_dir.as_deref(),
            cd_signer_dir: req.cd_signer_dir.as_deref(),
            scope_id,
        },
    )
    .await
    {
        Ok(dev) => {
            tracing::info!(
                node_id = dev.node_id,
                fabric_index = ?dev.fabric_index,
                "commission executed (native ble-thread)"
            );
            Ok(CommissionAttempt::Done)
        }
        // BLE scan の空振り（デバイスが見えない）はワイヤ未接触。
        Err(CommissionError::Ble {
            step: "scan",
            detail,
        }) => Ok(CommissionAttempt::Unavailable(format!(
            "ble scan: {detail}"
        ))),
        Err(other) => Err(commission_error(other)),
    }
}

#[cfg(not(feature = "ble"))]
async fn ble_path(
    _fabric: &CommissioningFabric,
    _req: &CommissionRequest,
    _passcode: u32,
    _long: u16,
    _scope_id: u32,
) -> Result<CommissionAttempt, MatError> {
    Ok(CommissionAttempt::Unavailable(
        "not found via mDNS; this build has no BLE support (feature \"ble\")".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_mapping_follows_spec() {
        use mat_controller::commissioning::CommissionError as E;
        use mat_core::error::ErrorKind;
        assert_eq!(kind_of(&E::Timeout("pase")), ErrorKind::Timeout);
        assert_eq!(
            kind_of(&E::CommandStatus {
                step: "add-noc",
                code: 1
            }),
            ErrorKind::DeviceRejected
        );
        assert_eq!(
            kind_of(&E::Malformed {
                step: "csr",
                detail: "bad tlv"
            }),
            ErrorKind::ParseError
        );
        assert_eq!(
            kind_of(&E::NetworkConfig {
                step: "connect-network",
                status: 1,
                debug_text: None
            }),
            ErrorKind::Unreachable
        );
        // attestation 失敗 = デバイス拒否相当（純正でないデバイス等）。
        assert_eq!(
            kind_of(&E::Attestation(
                mat_controller::attestation::AttestationError::Nonce
            )),
            ErrorKind::DeviceRejected
        );
        assert_eq!(kind_of(&E::Noc(1)), ErrorKind::DeviceRejected);
        assert_eq!(kind_of(&E::Csr("bad")), ErrorKind::ParseError);
        // catch-all（マッチしないその他 variant）= CommissionFailed。
        // Ble は Timeout/Attestation/Noc/CommandStatus/NetworkConfig/
        // Malformed/Csr のいずれの腕にも当たらないため catch-all に落ちる。
        assert_eq!(
            kind_of(&E::Ble {
                step: "gatt",
                detail: "x".into()
            }),
            ErrorKind::CommissionFailed
        );
    }

    #[test]
    fn manual_code_short_filter_selects_unique_match() {
        let list = vec![
            fake_commissionable(0x0800), // short 8
            fake_commissionable(0x0F00), // short 15
        ];
        assert_eq!(pick_by_short(&list, 8).unwrap().discriminator, Some(0x0800));
        assert!(pick_by_short(&list, 1).is_none()); // 0 件
    }

    #[test]
    fn manual_code_short_filter_rejects_ambiguous() {
        let list = vec![fake_commissionable(0x0801), fake_commissionable(0x08FF)];
        // 同一 short (8) が 2 台 → 曖昧。Err 側で報告する設計。
        assert!(pick_by_short_strict(&list, 8).is_err());
    }

    fn fake_commissionable(d: u32) -> mat_controller::dnssd::CommissionableInstance {
        mat_controller::dnssd::CommissionableInstance {
            hostname: None,
            port: Some(5540),
            addresses: vec!["fd00::1".parse().unwrap()],
            discriminator: Some(d),
            vendor_id: None,
            product_id: None,
        }
    }
}
