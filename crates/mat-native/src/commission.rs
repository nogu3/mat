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
    /// 経路指定（既定 `Auto`）。
    pub transport: Transport,
}

/// setup code をパースした発見キー。QR は 12bit long、manual は 4bit short。
enum Code {
    Qr { passcode: u32, long: u16 },
    Manual { passcode: u32, short: u8 },
}

/// commission の経路指定。`mat` の `--transport` / `MAT_TRANSPORT` に対応する。
/// clap には依存しない（`ValueEnum` は `crates/mat/src/cli.rs` 側）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    /// mDNS で見つかればまず on-network、PASE が MRP を使い切ったら BLE。
    #[default]
    Auto,
    /// mDNS のみ。BLE には落ちない。
    OnNetwork,
    /// mDNS を一切引かず BLE 直行。
    Ble,
}

/// mDNS を引いた結果。I/O は呼び出し側（`discover`）が済ませてから渡す
/// ——`plan_routes` を純関数に保つため。
enum Discovered {
    /// `--transport ble`: mDNS を引いていない。
    NotConsulted,
    Hit {
        target: CommissionTarget,
        /// ログ・エラー要約用のラベル（解決したアドレス等）。
        label: String,
    },
    Miss,
}

/// 試す経路の候補。
#[derive(Debug)]
enum Route {
    OnNetwork {
        target: CommissionTarget,
        label: String,
    },
    Ble,
}

impl Route {
    fn label(&self) -> String {
        match self {
            Route::OnNetwork { label, .. } => label.clone(),
            Route::Ble => "ble".to_string(),
        }
    }
}

/// 試す順番を決める純関数。mDNS の I/O は含まない。
///
/// manual code は 4bit short discriminator しか持たず、BLE scan（12bit 完全
/// 一致）に使えないため BLE を候補に入れない。
///
/// `ble_usable` は「この実行で BLE 経路が実際に走れるか」（`ble` feature 付き
/// ビルド **かつ** `--thread-dataset` あり）。呼び出し側が計算して渡す
/// ——この関数を純関数（feature フラグもリクエストも読まない）に保つため。
/// hit 側の後詰めだけがこのフラグで閉じる: 走れない BLE を計画に載せると
/// 「mDNS で見つからなかった」と嘘をつく `unreachable`（`ble_path` の文言）で
/// on-network の `timeout` を上書きしてしまい、exit code が 3 → 5 に化ける。
/// **miss 側は従来どおり無条件で `Route::Ble` を計画する** — 走れないときの
/// 文言（`... no --thread-dataset for the BLE path` /
/// `... this build has no BLE support`）が現状の唯一の診断であり、1 バイトも
/// 変えない。
fn plan_routes(
    code: &Code,
    transport: Transport,
    discovered: &Discovered,
    ble_usable: bool,
) -> Result<Vec<Route>, MatError> {
    let is_qr = matches!(code, Code::Qr { .. });

    if transport == Transport::Ble {
        if !is_qr {
            return Err(MatError::new(
                ErrorKind::Other,
                "native commissioning: --transport ble requires the QR payload \
                 (manual code has no long discriminator) — should have been rejected by the CLI"
                    .to_string(),
            ));
        }
        return Ok(vec![Route::Ble]);
    }

    match discovered {
        Discovered::Hit { target, label } => {
            let mut routes = vec![Route::OnNetwork {
                target: *target,
                label: label.clone(),
            }];
            // auto かつ QR、かつ BLE が実際に走れるときだけ後詰めする。これが
            // 本修正の中心: mDNS のレコードは「存在」しか保証しないので、
            // 届かなかったときの逃げ道を計画に含めておく。走れない BLE を
            // 載せない理由は上の doc コメント参照。
            if transport == Transport::Auto && is_qr && ble_usable {
                routes.push(Route::Ble);
            }
            Ok(routes)
        }
        Discovered::Miss if is_qr && transport == Transport::Auto => Ok(vec![Route::Ble]),
        Discovered::Miss if is_qr => Err(MatError::new(
            ErrorKind::Unreachable,
            "native commissioning: not found via mDNS (--transport on-network does not fall back to BLE)"
                .to_string(),
        )),
        Discovered::Miss => Err(MatError::new(
            ErrorKind::Unreachable,
            "native commissioning: not found via mDNS (manual code cannot use BLE; use the QR payload)"
                .to_string(),
        )),
        Discovered::NotConsulted => Err(MatError::new(
            ErrorKind::Other,
            "native commissioning: mdns was not consulted outside --transport ble (internal)"
                .to_string(),
        )),
    }
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
        | CommissionError::Case(
            // EstablishmentFailed は Sigma3 完了 StatusReport が非成功 =
            // PeerStatus（Sigma1/2 段階の拒否）と同じイベントクラス
            // （v1 最終レビュー指摘で追加 — 1.0.0 契約凍結前に揃える）。
            CaseError::PeerStatus { .. }
            | CaseError::Sigma2SignatureInvalid
            | CaseError::EstablishmentFailed { .. },
        ) => ErrorKind::DeviceRejected,
        CommissionError::Attestation(_) => ErrorKind::DeviceRejected,
        CommissionError::Noc(_) | CommissionError::CommandStatus { .. } => {
            ErrorKind::DeviceRejected
        }
        CommissionError::NetworkConfig { .. } => ErrorKind::Unreachable,
        CommissionError::Malformed { .. } | CommissionError::Csr(_) => ErrorKind::ParseError,
        _ => ErrorKind::CommissionFailed,
    }
}

/// 「この失敗なら次の経路を試してよい」の判定。
///
/// **安全性の根拠は「PASE 完了前に failsafe は一切 arm されていない」**という
/// 一点である（PASE そのものが 1 往復だから、ではない —— PASE は 3 往復で、
/// `Pake1`（`pase.rs:330`）も `Pake3`（`pase.rs:384`）も `Exchange(Timeout)` に
/// 写るし、`ex.recv(RECV_TIMEOUT)`（`pase.rs:343` / `:389`）はデバイスが我々に
/// ack を返した **後** にしか走らない）。ここで拾う 3 つはいずれも
/// `pase::establish` が `Ok` を返す前に出るものであり、`ArmFailSafe` は
/// `run_credential_steps` の最初のステップ（`commissioning.rs:629`）＝
/// `pase::establish` の後にしか走らない。したがって failsafe も fabric 状態も
/// まだ存在せず、別経路でやり直しても中途状態と衝突しない。
///
/// 対象は 3 つ：
///
/// 1. ターゲット解決段階（`commissioning.rs:855` → `Timeout("no usable address")`）
///    — `pase::establish` 呼び出し前。
///
/// 2. PASE の MRP 再送尽き（`Pase(Exchange(Timeout))`）。
///
/// 3. mDNS 解決後 PASE 開始前の喪失（`Discovery(_)`）。
///
/// **既知の副作用（バグではない）**: PASE 後半（Pake1/Pake3 送信後）で中断すると、
/// responder 側は PASE 確立スロットを保持したままになることがある
/// （`pase.rs:363-372` の実機 E2E 知見。`ConfirmMismatch` では明示 StatusReport で
/// 解放させているが、`Exchange(Timeout)` は相手が無言な経路なので通知できない）。
/// つまり Pake1/Pake3 の timeout でフォールバックすると、BLE 経路が「PASE を
/// 受け付けないデバイス」を掴む可能性がある —— その場合の代償は BLE scan
/// 〜30 秒とその後の PASE 失敗であり、デバイス側に不可逆な状態は残らない。
///
/// **重要: `Timeout("operational discovery after thread join")`（`commissioning.rs:1072`）
/// と `Timeout("no usable operational address")`（`commissioning.rs:1078`）は意図的に
/// 除外している。これら 2 つは BLE 経路で PASE 成功・Thread 参加後に出るもので、
/// デバイスが 120 秒 failsafe 中に我々の部分状態を持つ。ここで別経路を試すと
/// 部分状態と衝突して不可逆ダメージになる — この検査がその防止線。
fn is_dead_end(e: &CommissionError) -> bool {
    use mat_controller::exchange::ExchangeError;
    use mat_controller::pase::PaseError;
    matches!(
        e,
        // ターゲット解決段階（commissioning.rs:855）— PASE 以前なので
        // デバイス側に状態は無い。
        CommissionError::Timeout("no usable address")
            | CommissionError::Pase(PaseError::Exchange(ExchangeError::Timeout))
            | CommissionError::Discovery(_)
    )
}

fn commission_error(e: CommissionError) -> MatError {
    MatError::new(kind_of(&e), format!("native commissioning failed: {e}"))
}

/// 経路要約に載せるための短縮。`MatError::detail` は経路ごとに
/// `native commissioning...` の接頭辞を持つので、並べるときは畳む。
fn short_detail(d: &str) -> &str {
    d.strip_prefix("native commissioning failed: ")
        .or_else(|| d.strip_prefix("native commissioning: "))
        .unwrap_or(d)
}

/// 全経路が尽きたときのエラー。`kind` は最後に試した経路のものを採り、
/// `detail` に経路ごとの結果を並べる。候補が 1 本だったときは現状の
/// `MatError` をそのまま返す（表現の互換性）。
fn compose_failure(failures: &[String], last: MatError) -> MatError {
    if failures.len() <= 1 {
        return last;
    }
    MatError::new(
        last.kind,
        format!(
            "native commissioning: all routes failed — {}",
            failures.join("; ")
        ),
    )
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

/// mDNS 発見（I/O）。結果を `Discovered` に畳んで `plan_routes` に渡す。
/// resolve/browse の失敗（timeout 以外）は現状どおりその場で `unreachable`。
async fn discover(code: &Code, scope_id: u32) -> Result<Discovered, MatError> {
    match code {
        Code::Qr { long, .. } => {
            match dnssd::resolve_commissionable(scope_id, *long, std::time::Duration::from_secs(5))
                .await
            {
                Ok(rn) => {
                    let label = match rn.addresses.first() {
                        Some(a) => format!("on-network({a})"),
                        None => format!("on-network(disc {long})"),
                    };
                    Ok(Discovered::Hit {
                        // 現状どおり Discriminator を渡す（commission_on_network が
                        // 内部で再 resolve する）。label は表示専用。
                        target: CommissionTarget::Discriminator(*long),
                        label,
                    })
                }
                Err(dnssd::DnssdError::Timeout { .. }) => Ok(Discovered::Miss),
                Err(e) => Err(MatError::new(
                    ErrorKind::Unreachable,
                    format!("native commissioning: mdns resolve: {e}"),
                )),
            }
        }
        Code::Manual { short, .. } => {
            let list = match dnssd::browse_commissionable(scope_id, dnssd::BROWSE_WINDOW).await {
                Ok(l) => l,
                Err(e) => {
                    return Err(MatError::new(
                        ErrorKind::Unreachable,
                        format!("native commissioning: mdns browse: {e}"),
                    ))
                }
            };
            match pick_by_short_strict(&list, *short)? {
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
                    Ok(Discovered::Hit {
                        target: CommissionTarget::Addr(std::net::SocketAddr::V6(
                            std::net::SocketAddrV6::new(*addr, port, 0, scope),
                        )),
                        label: format!("on-network({addr})"),
                    })
                }
                None => Ok(Discovered::Miss),
            }
        }
    }
}

/// 1 経路の実行結果。`dead_end` が true なら次の経路へ進んでよい。
struct RouteFail {
    err: MatError,
    dead_end: bool,
}

async fn run_route(
    route: &Route,
    fabric: &CommissioningFabric,
    req: &CommissionRequest,
    scope_id: u32,
    passcode: u32,
    long: Option<u16>,
) -> Result<(), RouteFail> {
    match route {
        Route::OnNetwork { target, .. } => {
            let transport = match UdpTransport::bind().await {
                Ok(t) => std::sync::Arc::new(t),
                Err(e) => {
                    // ローカルの bind 失敗は経路に依存しない（BLE 経路も最後に
                    // UDP を bind する）。別経路へ進んでも同じ理由で落ちるので
                    // dead_end ではない ＝ 即打ち切り。
                    return Err(RouteFail {
                        err: MatError::new(
                            ErrorKind::Other,
                            format!("native commissioning: udp bind: {e}"),
                        ),
                        dead_end: false,
                    });
                }
            };
            match commissioning::commission_on_network(
                transport,
                fabric,
                CommissionParams {
                    passcode,
                    target: *target,
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
                        "commission executed (native on-network)"
                    );
                    Ok(())
                }
                Err(other) => {
                    // 次の経路へ進めるかどうかの判定は常に `is_dead_end` のみが
                    // 決める（`Discovery(_)` は常に dead end）。`other` を借用
                    // している間に判定を終えてから、専用文言のための特別扱い
                    // （`Discovery`）だけをメッセージ生成側で行う。
                    let dead_end = is_dead_end(&other);
                    let err = match other {
                        // 内部 resolve（PASE より前）での空振り。事前 resolve
                        // 成功後の狭い競合窓だが発見の空振りなので unreachable。
                        CommissionError::Discovery(e) => MatError::new(
                            ErrorKind::Unreachable,
                            format!(
                                "native commissioning: commissionable disappeared before PASE: {e}"
                            ),
                        ),
                        other => commission_error(other),
                    };
                    Err(RouteFail { err, dead_end })
                }
            }
        }
        Route::Ble => {
            // BLE は QR コードでしか計画に載らない（plan_routes）。ライブラリ
            // コードなので不変条件が破れても panic はしない（内部エラー扱い）。
            let Some(long) = long else {
                return Err(RouteFail {
                    err: MatError::new(
                        ErrorKind::Other,
                        "native commissioning: ble route planned without a long discriminator \
                         (internal)"
                            .to_string(),
                    ),
                    dead_end: false,
                });
            };
            ble_path(fabric, req, passcode, long, scope_id).await
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

    // 発見（I/O）。--transport ble は mDNS を一切引かない。
    let discovered = if req.transport == Transport::Ble {
        Discovered::NotConsulted
    } else {
        discover(&code, scope_id).await?
    };

    // BLE 経路が実際に走れる条件（feature ビルド + Thread dataset）。片方でも
    // 欠けると `ble_path` は「mDNS で見つからなかった」と読める unreachable を
    // 返すため、hit 後の後詰め候補には載せない（plan_routes の doc 参照）。
    let ble_feature = cfg!(feature = "ble");
    let has_dataset = req.thread_dataset.is_some();
    let ble_usable = ble_feature && has_dataset;

    let routes = plan_routes(&code, req.transport, &discovered, ble_usable)?;
    let (passcode, long) = match code {
        Code::Qr { passcode, long } => (passcode, Some(long)),
        Code::Manual { passcode, .. } => (passcode, None),
    };

    // mDNS が hit した auto + QR なのに BLE を後詰めできなかったとき、その理由を
    // 運用者に伝える（`--thread-dataset` を足せば 2 本目の経路が開く、という
    // 情報は stderr でしか出ない — stdout の JSON は広げない）。
    if !ble_usable
        && req.transport == Transport::Auto
        && long.is_some()
        && matches!(discovered, Discovered::Hit { .. })
    {
        tracing::info!(
            ble_feature,
            thread_dataset = has_dataset,
            "on-network only: BLE fallback not planned \
             (needs a build with the \"ble\" feature and --thread-dataset)"
        );
    }

    let plan: Vec<String> = routes.iter().map(Route::label).collect();
    tracing::info!(
        transport = ?req.transport,
        routes = ?plan,
        "commission route plan"
    );

    run_plan(&routes, |i| {
        run_route(
            &routes[i],
            &commissioning_fabric,
            req,
            scope_id,
            passcode,
            long,
        )
    })
    .await
}

/// 経路計画を順に実行する。`run` は 1 経路ぶんの実行（本番では `run_route`、
/// テストでは台本化した `RouteFail` を返すクロージャ）を注入する
/// ——ネットワークに触れずにフォールバックの分岐そのものを固定するため。
///
/// 進み方の規則は 1 つだけ: **`dead_end` かつ次の候補があるときだけ次へ進む**。
/// それ以外は即打ち切り（`compose_failure` で要約）。
async fn run_plan<F, Fut>(routes: &[Route], mut run: F) -> Result<(), MatError>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<(), RouteFail>>,
{
    let mut failures: Vec<String> = Vec::new();
    for (i, route) in routes.iter().enumerate() {
        let label = route.label();
        match run(i).await {
            Ok(()) => return Ok(()),
            Err(RouteFail { err, dead_end }) => {
                failures.push(format!("{label}: {}", short_detail(&err.detail)));
                let next = routes.get(i + 1);
                match (dead_end, next) {
                    (true, Some(n)) => {
                        tracing::warn!(
                            from = %label,
                            to = %n.label(),
                            reason = %err.detail,
                            "route dead end — falling back"
                        );
                        continue;
                    }
                    _ => return Err(compose_failure(&failures, err)),
                }
            }
        }
    }
    // plan_routes は空の Vec を返さない（返すなら Err）。
    Err(MatError::new(
        ErrorKind::Other,
        "native commissioning: empty route plan (internal)".to_string(),
    ))
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
///
/// 戻り値が `RouteFail` なのは、`dead_end` の判定を on-network 腕とまったく
/// 同じ 1 箇所（`is_dead_end`）から導くため。BLE 経路も
/// `Pase(Exchange(Timeout))` を出しうる（`commissioning.rs:962`）。
#[cfg(feature = "ble")]
async fn ble_path(
    fabric: &CommissioningFabric,
    req: &CommissionRequest,
    passcode: u32,
    long: u16,
    scope_id: u32,
) -> Result<(), RouteFail> {
    let Some(dataset) = req.thread_dataset.as_deref() else {
        return Err(RouteFail {
            err: MatError::new(
                ErrorKind::Unreachable,
                "native commissioning: not found via mDNS and no --thread-dataset for the BLE path"
                    .to_string(),
            ),
            // 資材が無い＝この経路は走れない。別経路の追加根拠にはならない。
            dead_end: false,
        });
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
        Err(other) => {
            // on-network 腕と同じ順序: 借用中に dead_end を確定させてから、
            // 専用文言（scan 空振り）だけをメッセージ生成側で特別扱いする。
            let dead_end = is_dead_end(&other);
            let err = match other {
                // BLE scan の空振り（デバイスが見えない）= 発見の空振り → unreachable。
                CommissionError::Ble {
                    step: "scan",
                    detail,
                } => MatError::new(
                    ErrorKind::Unreachable,
                    format!("native commissioning: ble scan: {detail}"),
                ),
                other => commission_error(other),
            };
            Err(RouteFail { err, dead_end })
        }
    }
}

#[cfg(not(feature = "ble"))]
async fn ble_path(
    _fabric: &CommissioningFabric,
    _req: &CommissionRequest,
    _passcode: u32,
    _long: u16,
    _scope_id: u32,
) -> Result<(), RouteFail> {
    // mDNS miss + BLE 未コンパイル → unreachable（detail に ble feature を明記）。
    // この経路は走っていないので dead_end ではない。
    Err(RouteFail {
        err: MatError::new(
            ErrorKind::Unreachable,
            "native commissioning: not found via mDNS; this build has no BLE support (feature \"ble\")"
                .to_string(),
        ),
        dead_end: false,
    })
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
        // Sigma3 完了 StatusReport の非成功も PeerStatus と同じ拒否クラス。
        assert_eq!(
            kind_of(&CommissionError::Case(CaseError::EstablishmentFailed {
                general_code: 1,
                protocol_code: 0,
            })),
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

    // ---- Task 1: plan_routes ----

    fn qr() -> Code {
        Code::Qr {
            passcode: 20202021,
            long: 1082,
        }
    }

    fn manual() -> Code {
        Code::Manual {
            passcode: 20202021,
            short: 4,
        }
    }

    fn hit() -> Discovered {
        Discovered::Hit {
            target: CommissionTarget::Discriminator(1082),
            label: "on-network(2001:db8::1)".to_string(),
        }
    }

    /// BLE が走れるビルド・資材が揃っている前提の `ble_usable`。
    const BLE_OK: bool = true;
    /// `ble` feature 無しビルド、または `--thread-dataset` 未指定。
    const BLE_UNUSABLE: bool = false;

    #[test]
    fn plan_routes_auto_qr_hit_falls_back_to_ble() {
        let routes = plan_routes(&qr(), Transport::Auto, &hit(), BLE_OK).unwrap();
        assert_eq!(routes.len(), 2);
        assert!(matches!(routes[0], Route::OnNetwork { .. }));
        assert!(matches!(routes[1], Route::Ble));
    }

    /// 回帰固定（最終レビュー指摘 1）: BLE が走れないビルド／資材のときに hit 後
    /// の後詰めをすると、on-network の `timeout`(exit 3) が「mDNS で見つから
    /// なかった」と嘘をつく `unreachable`(exit 5) に上書きされる。載せない。
    #[test]
    fn plan_routes_auto_qr_hit_omits_ble_when_it_cannot_run() {
        let routes = plan_routes(&qr(), Transport::Auto, &hit(), BLE_UNUSABLE).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0], Route::OnNetwork { .. }));
    }

    #[test]
    fn plan_routes_auto_qr_miss_is_ble_only() {
        let routes = plan_routes(&qr(), Transport::Auto, &Discovered::Miss, BLE_OK).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0], Route::Ble));
    }

    /// miss 側は `ble_usable` で閉じない: 走れない理由（dataset 無し / feature
    /// 無し）を述べる `ble_path` の文言が唯一の診断なので、従来どおり計画に
    /// 載せて実行させる。
    #[test]
    fn plan_routes_auto_qr_miss_still_plans_ble_when_it_cannot_run() {
        let routes = plan_routes(&qr(), Transport::Auto, &Discovered::Miss, BLE_UNUSABLE).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0], Route::Ble));
    }

    #[test]
    fn plan_routes_auto_manual_hit_is_on_network_only() {
        // manual code は long discriminator を持たず BLE scan に使えない。
        let routes = plan_routes(&manual(), Transport::Auto, &hit(), BLE_OK).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0], Route::OnNetwork { .. }));
    }

    #[test]
    fn plan_routes_auto_manual_miss_is_unreachable_with_qr_hint() {
        let e = plan_routes(&manual(), Transport::Auto, &Discovered::Miss, BLE_OK).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unreachable);
        assert!(e.detail.contains("manual code cannot use BLE"));
    }

    #[test]
    fn plan_routes_on_network_qr_hit_never_includes_ble() {
        let routes = plan_routes(&qr(), Transport::OnNetwork, &hit(), BLE_OK).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0], Route::OnNetwork { .. }));
    }

    #[test]
    fn plan_routes_on_network_qr_miss_is_unreachable() {
        let e = plan_routes(&qr(), Transport::OnNetwork, &Discovered::Miss, BLE_OK).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unreachable);
        assert!(e.detail.contains("on-network"));
    }

    #[test]
    fn plan_routes_on_network_manual_miss_is_unreachable_with_qr_hint() {
        let e =
            plan_routes(&manual(), Transport::OnNetwork, &Discovered::Miss, BLE_OK).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unreachable);
        assert!(e.detail.contains("manual code cannot use BLE"));
    }

    #[test]
    fn plan_routes_ble_qr_skips_mdns_entirely() {
        let routes = plan_routes(&qr(), Transport::Ble, &Discovered::NotConsulted, BLE_OK).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0], Route::Ble));
    }

    #[test]
    fn plan_routes_ble_manual_is_internal_error() {
        // CLI 層が exit 2 で弾く組み合わせ。native まで来たら内部エラー。
        let e =
            plan_routes(&manual(), Transport::Ble, &Discovered::NotConsulted, BLE_OK).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Other);
    }

    // ---- Task 2: is_dead_end ----

    #[test]
    fn dead_end_is_limited_to_pase_and_predelivery() {
        use mat_controller::commissioning::CommissionError as E;
        use mat_controller::exchange::ExchangeError;
        use mat_controller::pase::PaseError;

        // ターゲット解決段階（commissioning.rs:855）— PASE 以前なのでデバイス側に状態は無い。
        assert!(is_dead_end(&E::Timeout("no usable address")));
        // PASE の MRP 使い切り = 宛先が無言。デバイス側に状態は無い → 次の経路へ。
        assert!(is_dead_end(&E::Pase(PaseError::Exchange(
            ExchangeError::Timeout
        ))));
        // 事前 resolve 成功後・PASE 開始前の喪失。ArmFailSafe 以前なので状態は無い。
        assert!(is_dead_end(&E::Discovery(
            mat_controller::dnssd::DnssdError::Timeout {
                instance: "x".into()
            }
        )));
    }

    #[test]
    fn case_timeout_is_not_a_dead_end() {
        use mat_controller::case::CaseError;
        use mat_controller::commissioning::CommissionError as E;
        use mat_controller::exchange::ExchangeError;

        // CASE まで来ているならデバイスは failsafe 中に我々の部分状態を持つ。
        // kind_of では Timeout に写るが、フォールバックしてはいけない。
        let e = E::Case(CaseError::Exchange(ExchangeError::Timeout));
        assert_eq!(kind_of(&e), ErrorKind::Timeout);
        assert!(!is_dead_end(&e));
    }

    #[test]
    fn device_rejection_is_not_a_dead_end() {
        use mat_controller::commissioning::CommissionError as E;
        use mat_controller::pase::PaseError;

        // 拒否は「届いている」証拠。別経路でも同じ理由で拒否される。
        assert!(!is_dead_end(&E::Pase(PaseError::ConfirmMismatch)));
        assert!(!is_dead_end(&E::CommandStatus {
            step: "add-noc",
            code: 1
        }));
    }

    #[test]
    fn post_pase_timeouts_are_not_dead_ends() {
        use mat_controller::commissioning::CommissionError as E;

        // BLE 経路で PASE 成功後・Thread 参加後に出る 2 つ
        // （commissioning.rs:1072 / 1078）。デバイスは failsafe 中に我々の
        // 部分状態を持つので、別経路で再駆動してはならない。
        assert!(!is_dead_end(&E::Timeout(
            "operational discovery after thread join"
        )));
        assert!(!is_dead_end(&E::Timeout("no usable operational address")));
    }

    // ---- Task 3: compose_failure / short_detail ----

    // ---- 最終レビュー指摘 2: run_plan（フォールバック・ループ本体）----
    //
    // ここまでのテストは「計画が何か」を固定するだけで、「計画どおりに実行
    // されるか」を一切固定していなかった（フォールバックを丸ごと殺す変異を
    // 入れても全部 green だった）。以下は **どの経路が実際に呼ばれたか** を
    // 記録して固定する。

    fn on_network_route() -> Route {
        Route::OnNetwork {
            target: CommissionTarget::Discriminator(1082),
            label: "on-network(2001:db8::1)".to_string(),
        }
    }

    fn route_fail(kind: ErrorKind, detail: &str, dead_end: bool) -> RouteFail {
        RouteFail {
            err: MatError::new(kind, detail.to_string()),
            dead_end,
        }
    }

    #[tokio::test]
    async fn run_plan_advances_to_the_next_route_on_a_dead_end() {
        let routes = vec![on_network_route(), Route::Ble];
        let calls = std::cell::RefCell::new(Vec::new());

        let out = run_plan(&routes, |i| {
            calls.borrow_mut().push(i);
            async move {
                if i == 0 {
                    Err(route_fail(
                        ErrorKind::Timeout,
                        "native commissioning failed: pase error: no acknowledgement",
                        true,
                    ))
                } else {
                    Ok(())
                }
            }
        })
        .await;

        assert!(out.is_ok(), "2 本目の成功が返るはず: {out:?}");
        assert_eq!(*calls.borrow(), vec![0, 1], "2 本目が実際に試されること");
    }

    #[tokio::test]
    async fn run_plan_stops_at_the_first_route_when_it_is_not_a_dead_end() {
        let routes = vec![on_network_route(), Route::Ble];
        let calls = std::cell::RefCell::new(Vec::new());

        let out = run_plan(&routes, |i| {
            calls.borrow_mut().push(i);
            async move {
                Err(route_fail(
                    ErrorKind::DeviceRejected,
                    "native commissioning failed: pase error: confirmation mismatch",
                    false,
                ))
            }
        })
        .await;

        let err = out.unwrap_err();
        assert_eq!(*calls.borrow(), vec![0], "2 本目を試してはいけない");
        // 1 本しか試していないので現状（フォールバック導入前）と 1 文字も変わらない。
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
        assert_eq!(
            err.detail,
            "native commissioning failed: pase error: confirmation mismatch"
        );
    }

    #[tokio::test]
    async fn run_plan_stops_on_success_without_touching_later_routes() {
        let routes = vec![on_network_route(), Route::Ble];
        let calls = std::cell::RefCell::new(Vec::new());

        let out = run_plan(&routes, |i| {
            calls.borrow_mut().push(i);
            async move { Ok(()) }
        })
        .await;

        assert!(out.is_ok());
        assert_eq!(*calls.borrow(), vec![0], "成功したら以降は試さない");
    }

    #[tokio::test]
    async fn run_plan_composes_every_failure_and_keeps_the_last_kind() {
        let routes = vec![on_network_route(), Route::Ble];
        let calls = std::cell::RefCell::new(Vec::new());

        let out = run_plan(&routes, |i| {
            calls.borrow_mut().push(i);
            async move {
                if i == 0 {
                    Err(route_fail(
                        ErrorKind::Timeout,
                        "native commissioning failed: pase error: no acknowledgement",
                        true,
                    ))
                } else {
                    Err(route_fail(
                        ErrorKind::Unreachable,
                        "native commissioning: ble scan: not seen",
                        false,
                    ))
                }
            }
        })
        .await;

        let err = out.unwrap_err();
        assert_eq!(*calls.borrow(), vec![0, 1]);
        // kind は最後に試した経路のもの。
        assert_eq!(err.kind, ErrorKind::Unreachable);
        assert!(err
            .detail
            .starts_with("native commissioning: all routes failed — "));
        assert!(err
            .detail
            .contains("on-network(2001:db8::1): pase error: no acknowledgement"));
        assert!(err.detail.contains("ble: ble scan: not seen"));
    }

    /// 候補が 1 本のときは `compose_failure` を通っても現状と 1 文字も変わらない
    /// ——`compose_failure` を直接叩くのではなく、ループ本体を通して固定する。
    #[tokio::test]
    async fn single_route_failure_is_reported_verbatim() {
        let routes = vec![Route::Ble];
        let calls = std::cell::RefCell::new(Vec::new());

        // dead_end=true でも「次が無い」ので即返る（唯一の経路が dead end に
        // なるのは miss + BLE 経路のケース）。
        let out = run_plan(&routes, |i| {
            calls.borrow_mut().push(i);
            async move {
                Err(route_fail(
                    ErrorKind::Timeout,
                    "native commissioning failed: x",
                    true,
                ))
            }
        })
        .await;

        let err = out.unwrap_err();
        assert_eq!(*calls.borrow(), vec![0]);
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert_eq!(err.detail, "native commissioning failed: x");
    }

    #[test]
    fn multi_route_failure_lists_every_route() {
        let last = MatError::new(
            ErrorKind::Unreachable,
            "native commissioning: no dataset".to_string(),
        );
        let out = compose_failure(
            &[
                "on-network(2001:db8::1): pase timed out".to_string(),
                "ble: no dataset".to_string(),
            ],
            last,
        );
        // kind は最後に試した経路のものを採る。
        assert_eq!(out.kind, ErrorKind::Unreachable);
        assert!(out
            .detail
            .starts_with("native commissioning: all routes failed — "));
        assert!(out
            .detail
            .contains("on-network(2001:db8::1): pase timed out"));
        assert!(out.detail.contains("ble: no dataset"));
    }

    #[test]
    fn short_detail_strips_known_prefixes() {
        assert_eq!(short_detail("native commissioning failed: boom"), "boom");
        assert_eq!(short_detail("native commissioning: boom"), "boom");
        assert_eq!(short_detail("boom"), "boom");
    }
}
