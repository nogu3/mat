//! native commissioning（M8c-1）。setup code のパース・発見・経路選択
//! （mDNS → BLE）・ErrorKind 写像を担う薄いラッパー。プロトコル本体は
//! mat-controller（M6a `commission_on_network` / M6b `commission_ble_thread`）。
//!
//! M8c-3（chip-tool 撤去）: native が commission の唯一の経路。従来
//! `CommissionAttempt::Unavailable`（ワイヤ未接触 → chip-tool フォールバック可）
//! だった分岐はすべてハードエラー化した（`commission` は `Result<(), MatError>`）:
//! - 発見の空振り（mDNS/BLE miss・manual code 0 件・PASE 直前の消失）→ `unreachable`。
//! - KVS/資材/epoch 系 → `store_missing` / `store_parse`。
//! - iface / UDP bind などローカル資材 → `other`。
//! - PASE 開始後の失敗 → `kind_of` 写像（従来どおり）。

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
/// （`Discovery`）は `commission` 本体で `unreachable` に写すためここには来ない）。
///
/// v1 品質修正 1: 旧 `_ => CommissionFailed` が Pase/Case/Session を全部吸収して
/// いたのを分離 — timeout 系（MRP 再送尽き）は `Timeout`(exit 3)、デバイスの明示
/// 拒否（passcode 不一致 = SPAKE2+ confirm 不一致 / PASE・CASE の StatusReport 拒否 /
/// Sigma2 署名不正）は `DeviceRejected`(exit 4)。残余のみ `CommissionFailed`。
fn kind_of(e: &CommissionError) -> ErrorKind {
    use mat_controller::case::CaseError;
    use mat_controller::exchange::ExchangeError;
    use mat_controller::pase::PaseError;
    use mat_controller::session::SessionError;
    match e {
        CommissionError::Timeout(_)
        | CommissionError::Pase(PaseError::Exchange(ExchangeError::Timeout))
        | CommissionError::Case(CaseError::Exchange(ExchangeError::Timeout))
        | CommissionError::Session(SessionError::Timeout) => ErrorKind::Timeout,
        CommissionError::Pase(PaseError::ConfirmMismatch | PaseError::StatusReport { .. })
        | CommissionError::Case(CaseError::PeerStatus { .. } | CaseError::Sigma2SignatureInvalid) => {
            ErrorKind::DeviceRejected
        }
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
/// 0 件 = `Ok(None)`（未発見 → 呼び出し側で `unreachable`）、1 件 = `Ok(Some(_))`、
/// 2 件以上 = `Err`（曖昧 = `commission_failed`）。
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

/// epoch IPK 解決（M8c-3 spec 設計 2）: ① KVS の mat-epoch キー →
/// ② 無ければ chip-tool 既定定数を KDF ガードで検証し、その場で KVS へ
/// 採用永続（adopt）→ 使用。③ 不一致は store_parse ハードエラー。
/// この解決はフォールバックさせない（採用永続の書込失敗も hard error —
/// flock 規律は M8c-2 と同じ）。M8c-1 の「不一致 → Unavailable（フォール
/// バック）」からの挙動変更（spec 承認済み）。
///
/// ネットワークに触れないため（KVS の読み書きのみ）ユニットテスト可能 —
/// `commission()` 全体を走らせると mDNS に出てしまうのでここへ切り出した。
fn resolve_ipk_epoch(
    main_ini: &std::path::Path,
    fabric_index: u8,
    creds: &fabric::FabricCredentials,
) -> Result<[u8; 16], MatError> {
    match kvs::read_mat_ipk_epoch(main_ini, fabric_index) {
        Err(e) => Err(MatError::new(
            ErrorKind::StoreParse,
            format!("kvs ipk epoch: {e}"),
        )),
        Ok(Some(epoch)) => {
            // 永続済み epoch と KVS operational の整合を毎回検証（片方だけ
            // 書き換わった不整合ストアで commission しない）。
            let cfid = fabric::compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
            if fabric::derive_ipk_operational(&epoch, &cfid) != creds.ipk_operational {
                return Err(MatError::new(
                    ErrorKind::StoreParse,
                    "kvs ipk epoch does not derive the stored operational key (inconsistent store)"
                        .to_string(),
                ));
            }
            Ok(epoch)
        }
        Ok(None) => {
            if !fabric::verify_default_ipk_epoch(
                &creds.root_public_key,
                creds.fabric_id,
                &creds.ipk_operational,
            ) {
                return Err(MatError::new(
                    ErrorKind::StoreParse,
                    "fabric IPK epoch unknown: not persisted and not the chip-tool default (rotated or foreign fabric)".to_string(),
                ));
            }
            kvs::write_mat_ipk_epoch(main_ini, fabric_index, &fabric::CHIP_TOOL_DEFAULT_IPK_EPOCH)
                .map_err(|e| {
                    MatError::new(
                        ErrorKind::StoreParse,
                        format!("kvs ipk epoch adopt write: {e}"),
                    )
                })?;
            tracing::info!(fabric_index, "ipk epoch adopted (kvs)");
            Ok(fabric::CHIP_TOOL_DEFAULT_IPK_EPOCH)
        }
    }
}

pub async fn commission(cfg: &NativeConfig, req: &CommissionRequest) -> Result<(), MatError> {
    let code = parse_code(&req.setup_code)?;

    // 資材構築（ローカル — M8c-3 で失敗は種別ごとのハードエラー）。
    let scope_id = dnssd::iface_index(&cfg.iface).map_err(|e| {
        MatError::new(
            ErrorKind::Other,
            format!("native commissioning: resolve iface {:?}: {e}", cfg.iface),
        )
    })?;
    let alpha = cfg.store.join("chip_tool_config.alpha.ini");
    let main_ini = cfg.store.join("chip_tool_config.ini");
    let materials =
        match kvs::read_self_issue_materials(&alpha, &main_ini, cfg.fabric_index, cfg.issuer_index)
        {
            Ok(m) => m,
            // KVS 資材が読めない = fabric 未 bootstrap → store_missing。
            Err(e) => {
                return Err(MatError::new(
                    ErrorKind::StoreMissing,
                    format!(
                        "native commissioning: read KVS credentials: {e} — run `mat fabric init`"
                    ),
                ))
            }
        };
    // epoch IPK 解決（M8c-3）には fabric の root 公開鍵が要るため一度
    // `FabricCredentials` を組む——`kvs::SelfIssueMaterials` は既に
    // `#[derive(Clone)]` 済み（秘密鍵を持つ型に Clone をここで新規に足す
    // わけではない）ので、安価な INI 再読みではなく `materials.clone()`
    // で賄う。
    let creds = match fabric::FabricCredentials::from_self_issued(materials.clone()) {
        Ok(c) => c,
        // 資材はあるが NOC を組めない = 壊れた/不整合な store → store_parse。
        Err(e) => {
            return Err(MatError::new(
                ErrorKind::StoreParse,
                format!("native commissioning: self-issue NOC: {e}"),
            ))
        }
    };
    // 不一致（非 chip-tool fabric / IPK ローテーション済み）は Err —
    // フォールバックしない（resolve_ipk_epoch のドキュメント参照）。
    let ipk_epoch = resolve_ipk_epoch(&main_ini, cfg.fabric_index, &creds)?;
    let commissioning_fabric = CommissioningFabric::from_materials(materials, ipk_epoch);

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
                Err(e) => {
                    return Err(MatError::new(
                        ErrorKind::Unreachable,
                        format!("native commissioning: mdns resolve: {e}"),
                    ))
                }
            }
        }
        Code::Manual { passcode, short } => {
            let list = match dnssd::browse_commissionable(scope_id, dnssd::BROWSE_WINDOW).await {
                Ok(l) => l,
                Err(e) => {
                    return Err(MatError::new(
                        ErrorKind::Unreachable,
                        format!("native commissioning: mdns browse: {e}"),
                    ))
                }
            };
            match pick_by_short_strict(&list, short)? {
                Some(c) => {
                    let Some(addr) = c.addresses.first() else {
                        return Err(MatError::new(
                            ErrorKind::Unreachable,
                            "native commissioning: commissionable found but no address resolved"
                                .to_string(),
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
                    return Err(MatError::new(
                        ErrorKind::Unreachable,
                        "native commissioning: not found via mDNS (manual code cannot use BLE; use the QR payload)"
                            .to_string(),
                    ))
                }
            }
        }
    };

    // UDP bind はローカルのエフェメラルポート取得のみ。M8c-3 で失敗は other。
    // on-network 実行はここから先（commission_on_network 呼び出し）が実ワイヤ
    // 接触で、そちらの失敗は `kind_of` 写像。
    let transport = match UdpTransport::bind().await {
        Ok(t) => std::sync::Arc::new(t),
        Err(e) => {
            return Err(MatError::new(
                ErrorKind::Other,
                format!("native commissioning: udp bind: {e}"),
            ))
        }
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
        // 内部 resolve（PASE より前）での空振り。事前 resolve 成功後の狭い競合窓
        // だが発見の空振りなので unreachable に倒す（M8c-3: フォールバック撤去）。
        Err(CommissionError::Discovery(e)) => {
            return Err(MatError::new(
                ErrorKind::Unreachable,
                format!("native commissioning: commissionable disappeared before PASE: {e}"),
            ))
        }
        Err(other) => return Err(commission_error(other)),
    };
    tracing::info!(
        node_id = dev.node_id,
        fabric_index = ?dev.fabric_index,
        "commission executed (native on-network)"
    );
    Ok(())
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
) -> Result<(), MatError> {
    let Some(dataset) = req.thread_dataset.as_deref() else {
        return Err(MatError::new(
            ErrorKind::Unreachable,
            "native commissioning: not found via mDNS and no --thread-dataset for the BLE path"
                .to_string(),
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
            Ok(())
        }
        // BLE scan の空振り（デバイスが見えない）= 発見の空振り → unreachable。
        Err(CommissionError::Ble {
            step: "scan",
            detail,
        }) => Err(MatError::new(
            ErrorKind::Unreachable,
            format!("native commissioning: ble scan: {detail}"),
        )),
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
) -> Result<(), MatError> {
    // mDNS miss + BLE 未コンパイル → unreachable（detail に ble feature を明記）。
    Err(MatError::new(
        ErrorKind::Unreachable,
        "native commissioning: not found via mDNS; this build has no BLE support (feature \"ble\")"
            .to_string(),
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

    /// v1 品質修正 1: `_ => CommissionFailed` が吸収していた Pase/Case/Session の
    /// うち、timeout 系 → `Timeout`(exit 3)、デバイス明示拒否系 → `DeviceRejected`
    /// (exit 4) に分離されること。
    #[test]
    fn kind_of_splits_timeout_and_rejection_out_of_commission_failed() {
        use mat_controller::case::CaseError;
        use mat_controller::exchange::ExchangeError;
        use mat_controller::pase::PaseError;
        use mat_controller::session::SessionError;

        // timeout 系（MRP 再送尽き）→ Timeout
        assert_eq!(
            kind_of(&CommissionError::Pase(PaseError::Exchange(
                ExchangeError::Timeout
            ))),
            ErrorKind::Timeout
        );
        assert_eq!(
            kind_of(&CommissionError::Case(CaseError::Exchange(
                ExchangeError::Timeout
            ))),
            ErrorKind::Timeout
        );
        assert_eq!(
            kind_of(&CommissionError::Session(SessionError::Timeout)),
            ErrorKind::Timeout
        );

        // 拒否系 → DeviceRejected
        assert_eq!(
            kind_of(&CommissionError::Pase(PaseError::ConfirmMismatch)),
            ErrorKind::DeviceRejected
        );
        assert_eq!(
            kind_of(&CommissionError::Pase(PaseError::StatusReport {
                general_code: 1,
                protocol_code: 0,
            })),
            ErrorKind::DeviceRejected
        );
        assert_eq!(
            kind_of(&CommissionError::Case(CaseError::PeerStatus {
                stage: "sigma2",
                general_code: 1,
                protocol_code: 0,
            })),
            ErrorKind::DeviceRejected
        );
        assert_eq!(
            kind_of(&CommissionError::Case(CaseError::Sigma2SignatureInvalid)),
            ErrorKind::DeviceRejected
        );

        // 上記以外の Pase/Case/Session は従来どおり CommissionFailed の残余
        assert_eq!(
            kind_of(&CommissionError::Pase(PaseError::NotAcked)),
            ErrorKind::CommissionFailed
        );
        assert_eq!(
            kind_of(&CommissionError::Case(CaseError::Tbe2DecryptFailed)),
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

    // ---- M8c-3 Task 6: resolve_ipk_epoch ----

    const FAKE_ROOT_PUB: [u8; 65] = [0xAA; 65];
    const FAKE_FABRIC_ID: u64 = 0x1122_3344_5566_7788;

    /// テスト専用の最小 `FabricCredentials`: `resolve_ipk_epoch` は
    /// `root_public_key` / `fabric_id` / `ipk_operational` しか見ないため、
    /// cert/opkey 系フィールドはダミーで埋める（証明書パースを経由しない）。
    fn fake_creds(ipk_operational: [u8; 16]) -> fabric::FabricCredentials {
        fabric::FabricCredentials {
            rcac_tlv: vec![],
            icac_tlv: None,
            noc_tlv: vec![],
            op_public_key: [0u8; 65],
            op_private_key: [0u8; 32],
            ipk_operational,
            node_id: 1,
            fabric_id: FAKE_FABRIC_ID,
            root_public_key: FAKE_ROOT_PUB,
        }
    }

    fn tmp_main_ini() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chip_tool_config.ini");
        std::fs::write(&p, "[Default]\n").unwrap();
        (dir, p)
    }

    #[test]
    fn resolve_ipk_epoch_adopts_and_persists_default_when_key_absent() {
        let (_dir, ini) = tmp_main_ini();
        let cfid = fabric::compressed_fabric_id(&FAKE_ROOT_PUB, FAKE_FABRIC_ID);
        let ipk_operational =
            fabric::derive_ipk_operational(&fabric::CHIP_TOOL_DEFAULT_IPK_EPOCH, &cfid);
        let creds = fake_creds(ipk_operational);

        let epoch = resolve_ipk_epoch(&ini, 1, &creds).unwrap();
        assert_eq!(epoch, fabric::CHIP_TOOL_DEFAULT_IPK_EPOCH);
        // KVS へ採用永続されている。
        assert_eq!(
            kvs::read_mat_ipk_epoch(&ini, 1).unwrap(),
            Some(fabric::CHIP_TOOL_DEFAULT_IPK_EPOCH)
        );
    }

    #[test]
    fn resolve_ipk_epoch_reads_persisted_epoch_without_rewriting() {
        let (_dir, ini) = tmp_main_ini();
        // 定数とは別のランダム epoch を先に永続しておく。
        let epoch = [0x11u8; 16];
        kvs::write_mat_ipk_epoch(&ini, 1, &epoch).unwrap();
        let cfid = fabric::compressed_fabric_id(&FAKE_ROOT_PUB, FAKE_FABRIC_ID);
        let ipk_operational = fabric::derive_ipk_operational(&epoch, &cfid);
        let creds = fake_creds(ipk_operational);

        let before = std::fs::read_to_string(&ini).unwrap();
        let got = resolve_ipk_epoch(&ini, 1, &creds).unwrap();
        assert_eq!(got, epoch);
        // 追記なし（採用永続の書込は起きない）。
        let after = std::fs::read_to_string(&ini).unwrap();
        assert_eq!(before, after, "read path must not rewrite the KVS");
    }

    #[test]
    fn resolve_ipk_epoch_rejects_operational_unrelated_to_default() {
        let (_dir, ini) = tmp_main_ini();
        // epoch キー無し + operational が定数由来でも永続済みでもない値。
        let creds = fake_creds([0x99u8; 16]);

        let err = resolve_ipk_epoch(&ini, 1, &creds).unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreParse);
        // 採用永続もされない。
        assert_eq!(kvs::read_mat_ipk_epoch(&ini, 1).unwrap(), None);
    }
}
