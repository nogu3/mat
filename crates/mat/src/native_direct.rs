//! 直経路 = `OneShotRunner`。op の実体は `mat_native::op` / `runner`（matd と
//! 共有）。ここに残るのは store チェック・engine 構築・予算・emit と diag 補助。
//!
//! warm セッションは持たない: 確立 → 1 op → 破棄（設計ルール 4）。matd と違い
//! Timeout 再確立はしない（確立直後の session が stale なことはない）。
//! matd 稼働中は matd が優先される（main.rs の経路順）— ここに来るのは直経路
//! （`--matd` 強制なし、または matd 未応答）のみ。

use std::path::Path;

use mat_core::error::MatError;
use mat_core::output;
use mat_core::store::Store;
use mat_native::runner::OneShotRunner;
use mat_native::{Engine, NativeConfig};

use mat_native::op::{NodeOp, NodeOpKind};

use crate::device_op::DeviceOp;

pub(crate) struct Config<'a> {
    pub iface: &'a str,
    pub thread_iface: Option<mat_native::ThreadIfaceChoice>,
    pub fabric_index: u8,
    pub issuer_index: u8,
}

/// 直経路 provision の note（KVS を直接書いたので matd の warm 状態は古い）。
const PROVISION_NOTE: &str =
    "controller group state written natively to kvs; if matd is running, restart it to reload group state";

/// `node_touched` ヒントを撃ってはいけない op か。
///
/// `unpair`（`RemoveFabric`）だけが該当する。ヒントは matd に「このノードを
/// 今触ったので warm セッションを張り直せ」と伝えるものだが、fabric から
/// 外した直後のノードに対して撃つと matd が再購読 → CASE 失敗 → backoff を
/// 台帳 rescan（最大 `LEDGER_RESCAN_INTERVAL` = 60s）まで回し続ける。
/// 消えるノードなので黙って落とすのが正しい。
fn suppresses_node_touched_hint(op: &DeviceOp) -> bool {
    matches!(
        op,
        DeviceOp::Node(NodeOp {
            kind: NodeOpKind::RemoveFabric,
            ..
        })
    )
}

/// この op に使う `node_touched` ヒント（抑止対象なら no-op）。
fn hint_for(op: &DeviceOp) -> fn(u64) {
    if suppresses_node_touched_hint(op) {
        |_| {}
    } else {
        crate::matd_client::hint_node_touched
    }
}

/// 直経路 native の入口。`execute` で成功 body を得て stdout へ emit する。
pub(crate) fn run(
    op: &DeviceOp,
    store_path: &Path,
    cfg: &Config,
    op_timeout_ms: u64,
) -> Result<(), MatError> {
    let body = execute(op, store_path, cfg, op_timeout_ms)?;
    output::emit(body);
    Ok(())
}

/// store / commission チェック → engine 構築 → `OneShotRunner`（確立 → 1 op →
/// close → matd へ node_touched ヒント）で 1 op を実行し、成功 body（timestamp
/// 抜き）を返す。emit しない — `unpair` のように body を別の JSON へ合成する
/// 呼び手のための単位。
pub(crate) fn execute(
    op: &DeviceOp,
    store_path: &Path,
    cfg: &Config,
    op_timeout_ms: u64,
) -> Result<serde_json::Value, MatError> {
    let store = Store::open(store_path)?;
    // group 送信 / bump は特定ノード宛ではないため require_node をしない。
    // provision / grant / remove は「1 つでも未 commission なら exit 11」。
    let node_id = match op {
        DeviceOp::Node(n) => Some(n.node_id),
        DeviceOp::GroupProvision(p) => {
            for &id in &p.node_ids {
                store.require_node(id)?;
            }
            None
        }
        DeviceOp::GroupGrant { node_ids, .. } | DeviceOp::GroupRemove { node_ids, .. } => {
            for &id in node_ids {
                store.require_node(id)?;
            }
            None
        }
        DeviceOp::Group(_) | DeviceOp::GroupBump => None,
    };
    if let Some(id) = node_id {
        store.require_node(id)?;
    }
    // CLI 指定 epoch key はバックエンド接触前に検証する（不正入力に fail-fast）。
    if let DeviceOp::GroupProvision(p) = op {
        if let Some(k) = &p.epoch_key {
            mat_core::group::resolve_epoch_key(Some(k))?;
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            MatError::new(
                mat_core::error::ErrorKind::Other,
                format!("tokio runtime: {e}"),
            )
        })?;
    let body = rt.block_on(async {
        let native_cfg = NativeConfig {
            store: store.root().to_path_buf(),
            iface: cfg.iface.to_string(),
            thread_iface: cfg.thread_iface.clone(),
            fabric_index: cfg.fabric_index,
            issuer_index: cfg.issuer_index,
        };
        let engine = Engine::build(&native_cfg)
            .await
            .map_err(map_engine_build_error)?;
        let budget = op.budget_applies();
        if budget && op_timeout_ms > 0 {
            // 直経路にも matd 経路と同じ予算セマンティクス（exit 3）。
            run_op_with_deadline(
                run_with_engine(&engine, op),
                op_timeout_ms,
                node_id,
                hint_for(op),
            )
            .await
        } else {
            run_with_engine(&engine, op).await
        }
    })?;
    // group_id は Group/GroupProvision/GroupGrant/GroupRemove のみ Some（GroupBump は
    // 特定 group 宛ではない）。node_id は上で計算済みの値をそのまま使う。
    let group_id: Option<u16> = match op {
        DeviceOp::Group(g) => Some(g.group_id),
        DeviceOp::GroupProvision(p) => Some(p.group_id),
        DeviceOp::GroupGrant { group_id, .. } | DeviceOp::GroupRemove { group_id, .. } => {
            Some(*group_id)
        }
        DeviceOp::Node(_) | DeviceOp::GroupBump => None,
    };
    tracing::info!(
        op = op.name(),
        node_id,
        group_id,
        "op executed (native direct)"
    );
    Ok(body)
}

/// engine 上で 1 op を実行し成功 body を返す（emit しない — テスト可能な単位）。
async fn run_with_engine(engine: &Engine, op: &DeviceOp) -> Result<serde_json::Value, MatError> {
    // ヒント送信はブロッキング std I/O だが、ここは one-shot CLI の `block_on`
    // 直下で他に走る非同期タスクが無いため async 化する価値が無い（旧 `finish_conn` の doc を継承）。
    let runner = OneShotRunner::new(engine, hint_for(op));
    match op {
        DeviceOp::Node(n) => mat_native::runner::run_node(&runner, n, None).await,
        DeviceOp::Group(g) => mat_native::op::run_group_op(engine, g).await,
        DeviceOp::GroupProvision(p) => {
            mat_native::runner::provision(&runner, engine, p, Some(PROVISION_NOTE)).await
        }
        DeviceOp::GroupGrant { group_id, node_ids } => {
            mat_native::runner::grant(&runner, *group_id, node_ids).await
        }
        DeviceOp::GroupRemove {
            group_id,
            endpoint,
            node_ids,
        } => {
            mat_native::runner::remove_group(&runner, engine, *group_id, *endpoint, node_ids).await
        }
        DeviceOp::GroupBump => mat_native::op::run_group_bump(engine).await,
    }
}

/// budget 対象 op への deadline 適用。超過時は future ごと drop される
/// ため `OneShotRunner` の close+hint が走らない（Issue #22）。close はセッション
/// 所有権が future と共に drop 済みで送れないが、`hint`（実体は
/// `matd_client::hint_node_touched`）だけ撃てば常駐購読側の救済は成立する。
async fn run_op_with_deadline<T, F>(
    fut: F,
    op_timeout_ms: u64,
    node_id: Option<u64>,
    hint: impl FnOnce(u64),
) -> Result<T, MatError>
where
    F: std::future::Future<Output = Result<T, MatError>>,
{
    match tokio::time::timeout(std::time::Duration::from_millis(op_timeout_ms), fut).await {
        Ok(r) => r,
        Err(_) => {
            match node_id {
                Some(id) => hint(id),
                // budget_applies() は現状すべて単一ノード op なのでここは
                // 到達しない。将来 multi-node op が対象化されたときに
                // ヒントが黙って消えないよう痕跡だけ残す。
                None => tracing::debug!("op deadline exceeded without node_id; hint skipped"),
            }
            Err(MatError::new(
                mat_core::error::ErrorKind::Timeout,
                format!("op deadline exceeded after {op_timeout_ms}ms (direct path)"),
            ))
        }
    }
}

/// エンジン構築失敗（M8c-3: chip-tool フォールバック撤去後のハードエラー化）。
/// `Engine::build` は KVS 資材の読取失敗を `store_missing` に写す（`mat-native`
/// 参照 — Io/NotFound と parse の細分化は将来）。ここでは store_missing に
/// 「`mat fabric init` で資材を作れ」の誘導を足して返す。他 kind はそのまま伝播。
fn map_engine_build_error(mut e: MatError) -> MatError {
    if e.kind == mat_core::error::ErrorKind::StoreMissing && !e.detail.contains("mat fabric init") {
        e.detail = format!(
            "{} — run `mat fabric init` to bootstrap the credential store",
            e.detail
        );
    }
    e
}

/// `mat diag node` の IM 部分（operational チェック + thread シグナル）を
/// native で実行した結果（M8c-2）。CFID はログパースではなく fabric 資材
/// から直接計算するため、native 経路では cfid_unavailable の系が消える。
pub(crate) struct DiagImProbe {
    pub resolved: bool,
    pub op_kind: Option<mat_core::error::ErrorKind>,
    pub self_cfid: String,
    pub thread: Result<mat_core::diag::ThreadCheck, mat_core::error::ErrorKind>,
}

/// `diag_im_probe` の入口。M8c-3（chip-tool 撤去）: エンジン構築失敗は
/// フォールバックせずハードエラー化（`run` の build 失敗と同じ写像 —
/// store_missing に `mat fabric init` 誘導を付す）。
pub(crate) fn diag_im_probe(
    cfg: &Config<'_>,
    store_root: &Path,
    node_id: u64,
    endpoint: u16,
) -> Result<DiagImProbe, MatError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            MatError::new(
                mat_core::error::ErrorKind::Other,
                format!("tokio runtime: {e}"),
            )
        })?;
    rt.block_on(async {
        let native_cfg = NativeConfig {
            store: store_root.to_path_buf(),
            iface: cfg.iface.to_string(),
            thread_iface: cfg.thread_iface.clone(),
            fabric_index: cfg.fabric_index,
            issuer_index: cfg.issuer_index,
        };
        let engine = Engine::build(&native_cfg)
            .await
            .map_err(map_engine_build_error)?;
        Ok(diag_im_with_engine(&engine, node_id, endpoint).await)
    })
}

async fn diag_im_with_engine(engine: &Engine, node_id: u64, endpoint: u16) -> DiagImProbe {
    use mat_core::ids::{resolve_attribute, resolve_cluster};
    // cfid は build 済みエンジンでは常に Some（with_parts 注入時のみ呼び出し側が保証）。
    let cfid = engine
        .group_settings
        .as_ref()
        .map(|g| g.cfid)
        .expect("built engine always carries group_settings");
    let self_cfid = format!("{:016X}", u64::from_be_bytes(cfid));

    // descriptor / parts-list は mat-core::ids 表から解決する（プロトコル知識を
    // 重複ハードコードしない）。表に無ければここで Other へフォールバックする —
    // 現行の名前表には常に載っているため通常到達しない。
    let descriptor_parts =
        resolve_cluster("descriptor").zip(resolve_attribute(0x001D, "parts-list").map(|a| a.id));

    let (resolved, op_kind, thread) = match engine.establisher.establish(node_id).await {
        Err(e) => (false, Some(e.kind), Err(e.kind)),
        Ok(mut conn) => {
            let (resolved, op_kind) = match descriptor_parts {
                None => (false, Some(mat_core::error::ErrorKind::Other)),
                Some((cluster, attr)) => match conn.read_json(0, cluster, attr).await {
                    Ok(_) => (true, None),
                    Err(e) => (false, Some(e.kind)),
                },
            };
            // thread シグナルの field-id 知識（NEIGHBOR_TABLE_FIELDS 等）は
            // mat-native::ops に閉じている（CLAUDE.md 設計ルール1）。
            let thread = match mat_native::ops::diag_thread(&mut *conn, endpoint).await {
                Err(e) => Err(e.kind),
                Ok(snap) => mat_native::ops::thread_check_from_snapshot(&snap).map_err(|e| e.kind),
            };
            // 成否によらず close してから返す（Issue #20）。establish 自体の
            // 失敗（Err 腕）はセッションが無いので close 不要。
            conn.close().await;
            crate::matd_client::hint_node_touched(node_id);
            (resolved, op_kind, thread)
        }
    };
    tracing::info!(node_id, "diag node executed (native)");
    DiagImProbe {
        resolved,
        op_kind,
        self_cfid,
        thread,
    }
}

/// `mat diag mesh` の per-node 収集結果 1 件。
pub(crate) struct MeshProbeItem {
    pub node_id: u64,
    pub result: Result<mat_core::mesh::ProbeData, MatError>,
}

/// `mat diag mesh` の収集: engine を 1 度構築し、各対象ノードへ逐次
/// CASE → cluster 53（diag_thread）+ cluster 0x33（thread_identity）。
/// per-node の失敗は item の Err に畳む（部分結果）。エンジン構築失敗のみ
/// ハードエラー。
pub(crate) fn diag_mesh_probe(
    cfg: &Config<'_>,
    store_root: &Path,
    targets: &[u64],
) -> Result<Vec<MeshProbeItem>, MatError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            MatError::new(
                mat_core::error::ErrorKind::Other,
                format!("tokio runtime: {e}"),
            )
        })?;
    rt.block_on(async {
        let native_cfg = NativeConfig {
            store: store_root.to_path_buf(),
            iface: cfg.iface.to_string(),
            thread_iface: cfg.thread_iface.clone(),
            fabric_index: cfg.fabric_index,
            issuer_index: cfg.issuer_index,
        };
        let engine = Engine::build(&native_cfg)
            .await
            .map_err(map_engine_build_error)?;
        let mut out = Vec::new();
        for &node_id in targets {
            let result = mesh_probe_one(&engine, node_id).await;
            if let Err(e) = &result {
                tracing::warn!(node_id, kind = ?e.kind, detail = %e.detail,
                    "mesh probe failed for node; continuing");
            }
            out.push(MeshProbeItem { node_id, result });
        }
        Ok(out)
    })
}

/// 1 ノード分: CASE 確立 → cluster 53 → cluster 0x33。0x33 は補助情報なので
/// 読めなくても成功扱い（identity=None、warn ログのみ）。
async fn mesh_probe_one(
    engine: &Engine,
    node_id: u64,
) -> Result<mat_core::mesh::ProbeData, MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    // 成否によらず close してから返す（Issue #20）。
    let result = async {
        let snap = mat_native::ops::diag_thread(&mut *conn, 0).await?;
        let identity = match mat_native::ops::thread_identity(&mut *conn, 0).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(node_id, kind = ?e.kind,
                    "network-interfaces read failed; continuing without self-identity");
                None
            }
        };
        tracing::info!(
            node_id,
            has_identity = identity.is_some(),
            "mesh probe executed (native direct)"
        );
        Ok(mat_core::mesh::ProbeData {
            thread: snap.fields,
            identity,
        })
    }
    .await;
    conn.close().await;
    crate::matd_client::hint_node_touched(node_id);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `diag_im_with_engine` の scripted establisher: parts-list（descriptor,
    /// ep0）は既定応答（`FakeConn` の未登録フォールバック `json!(1)`）で
    /// resolved=true になる。thread シグナルは `mat_native::ops::diag_thread`
    /// 経由（`read_cluster` の wildcard read 1発）に変わったため、
    /// neighbor-table（0x0035/0x0007, ep1）は `with_cluster` で構造体配列を
    /// 明示応答する（field id "5" = Lqi、ops.rs の `NEIGHBOR_TABLE_FIELDS` で
    /// 改名される）。
    struct ScriptedImEstablisher;
    #[async_trait::async_trait]
    impl mat_native::Establisher for ScriptedImEstablisher {
        async fn establish(
            &self,
            _node_id: u64,
        ) -> Result<Box<dyn mat_native::NodeConn>, MatError> {
            use mat_native::test_support::FakeConn;
            Ok(Box::new(FakeConn::scripted().with_cluster(
                1,
                0x0035,
                vec![(0x0007, serde_json::json!([{"5": 200}, {"5": 100}]))],
            )))
        }
    }

    struct FailingImEstablisher;
    #[async_trait::async_trait]
    impl mat_native::Establisher for FailingImEstablisher {
        async fn establish(
            &self,
            _node_id: u64,
        ) -> Result<Box<dyn mat_native::NodeConn>, MatError> {
            Err(MatError::new(
                mat_core::error::ErrorKind::Unreachable,
                "fake unreachable",
            ))
        }
    }

    fn failing_establisher() -> FailingImEstablisher {
        FailingImEstablisher
    }

    #[tokio::test]
    async fn diag_im_with_engine_reads_operational_and_thread_natively() {
        let mut engine = Engine::with_parts(Box::new(ScriptedImEstablisher), None);
        engine.group_settings = Some(mat_native::group_settings::GroupSettingsCtx {
            main_ini: std::path::PathBuf::from("/nonexistent"),
            fabric_index: 2,
            cfid: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
        });
        let p = diag_im_with_engine(&engine, 5, 1).await;
        assert!(p.resolved);
        assert_eq!(p.op_kind, None);
        assert_eq!(p.self_cfid, "1122334455667788");
        let t = p.thread.expect("thread check");
        assert!(t.neighbor_count >= 1);
        assert_eq!(t.best_lqi, Some(200));
    }

    #[tokio::test]
    async fn diag_im_with_engine_reports_establish_failure_as_unresolved() {
        let mut engine = Engine::with_parts(Box::new(failing_establisher()), None);
        engine.group_settings = Some(mat_native::group_settings::GroupSettingsCtx {
            main_ini: std::path::PathBuf::from("/nonexistent"),
            fabric_index: 2,
            cfid: [1u8; 8],
        });
        let p = diag_im_with_engine(&engine, 5, 1).await;
        assert!(!p.resolved);
        assert_eq!(p.op_kind, Some(mat_core::error::ErrorKind::Unreachable));
        assert!(p.thread.is_err());
    }

    /// F6: `unpair`（RemoveFabric）だけは node_touched ヒントを撃たない。
    /// 撃つと matd が外したばかりのノードへ再購読し、次の台帳 rescan（最大
    /// 60s）まで CASE 失敗の backoff ノイズを出す。
    #[test]
    fn remove_fabric_suppresses_the_node_touched_hint() {
        let unpair = DeviceOp::Node(NodeOp {
            node_id: 24,
            kind: NodeOpKind::RemoveFabric,
        });
        assert!(suppresses_node_touched_hint(&unpair));
        // no-op であること（生きたヒントなら env 依存の socket 接続を試みる）。
        let hint = hint_for(&unpair);
        hint(24);

        // 他の op はヒントを撃つ（= 抑止対象ではない）。
        let read = DeviceOp::Node(NodeOp {
            node_id: 24,
            kind: NodeOpKind::read(1, "onoff", "on-off").unwrap(),
        });
        assert!(!suppresses_node_touched_hint(&read));
        assert!(!suppresses_node_touched_hint(&DeviceOp::GroupBump));
    }

    /// Issue #22: deadline 超過で future ごと drop されると `OneShotRunner` の
    /// close+hint が走らない。Err 腕で node_touched ヒントだけは撃つこと
    /// （close はセッション所有権が drop 済みで送れない）。
    #[test]
    fn op_deadline_fires_node_touched_hint_on_timeout() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let hinted = std::cell::RefCell::new(None);
        let err = rt
            .block_on(run_op_with_deadline(
                std::future::pending::<Result<(), MatError>>(),
                10,
                Some(42),
                |id| *hinted.borrow_mut() = Some(id),
            ))
            .unwrap_err();
        assert!(matches!(err.kind, mat_core::error::ErrorKind::Timeout));
        assert_eq!(*hinted.borrow(), Some(42));
    }

    /// deadline 内に完了した op は結果をそのまま返し、ヒントは撃たない
    /// （完了時のヒントは `OneShotRunner` の責務 — 二重送信しない)。
    #[test]
    fn op_deadline_passes_through_completion_without_hint() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let hinted = std::cell::RefCell::new(None);
        let result = rt.block_on(run_op_with_deadline(
            std::future::ready(Ok(serde_json::json!({}))),
            10_000,
            Some(42),
            |id| *hinted.borrow_mut() = Some(id),
        ));
        assert!(result.is_ok());
        assert_eq!(*hinted.borrow(), None);
    }

    /// `classify` が出す DeviceOp を engine ごと直経路の実行関数に通し、
    /// FakeConn 応答で最後まで body が返ることを保証する。
    #[tokio::test]
    async fn run_with_engine_completes_for_read_open_window_group_invoke_and_bump() {
        use crate::cli::{Command, GroupCommand};
        use crate::device_op::{classify, Dispatch};
        use mat_core::alias::{EndpointRef, GroupRef, NodeRef};
        use mat_native::test_support::FakeEstablisher;
        let engine = Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        let read = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            attribute: Some("current-level".into()),
        };
        let Dispatch::Device(op) = classify(&read).unwrap() else {
            panic!("device op")
        };
        let body = run_with_engine(&engine, &op).await.unwrap();
        assert_eq!(body["cluster"], "levelcontrol");
        let ow = Command::OpenWindow {
            node_id: NodeRef::Id(5),
            timeout: 180,
            iteration: 1000,
            discriminator: Some(3840),
        };
        let Dispatch::Device(op) = classify(&ow).unwrap() else {
            panic!("device op")
        };
        assert!(run_with_engine(&engine, &op).await.unwrap()["qr_payload"].is_string());
        // group ctx 未構成はハードエラー（Other）。
        let toggle = Command::Group {
            action: GroupCommand::Invoke {
                group_id: GroupRef::Id(10),
                cluster: "onoff".into(),
                command: "toggle".into(),
                args: vec![],
                endpoint: 1,
            },
        };
        let Dispatch::Device(op) = classify(&toggle).unwrap() else {
            panic!("device op")
        };
        assert_eq!(
            run_with_engine(&engine, &op).await.unwrap_err().kind,
            mat_core::error::ErrorKind::Other
        );
        let Dispatch::Device(op) = classify(&Command::Group {
            action: GroupCommand::Bump,
        })
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            run_with_engine(&engine, &op).await.unwrap_err().kind,
            mat_core::error::ErrorKind::Other
        );
    }

    /// `group provision` の直経路成功 body には `PROVISION_NOTE`（KVS 直書き +
    /// matd 再起動案内）が付く（matd 経路は `note: None`）。
    #[tokio::test]
    async fn run_with_engine_provision_attaches_direct_path_note() {
        use mat_native::group_settings::GroupSettingsCtx;
        use mat_native::op::ProvisionParams;
        use mat_native::test_support::FakeConn;
        use serde_json::json;

        struct ScriptedEstablisher;
        #[async_trait::async_trait]
        impl mat_native::Establisher for ScriptedEstablisher {
            async fn establish(
                &self,
                _node_id: u64,
            ) -> Result<Box<dyn mat_native::NodeConn>, MatError> {
                Ok(Box::new(
                    FakeConn::scripted()
                        .with_read(0, 0x003F, 0x0000, json!([]))
                        .with_read(
                            0,
                            0x001F,
                            0x0000,
                            json!([{"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]),
                        ),
                ))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let mut engine = Engine::with_parts(Box::new(ScriptedEstablisher), None);
        engine.group_settings = Some(GroupSettingsCtx {
            main_ini: ini,
            fabric_index: 2,
            cfid: [7u8; 8],
        });

        let op = DeviceOp::GroupProvision(ProvisionParams {
            group_id: 99,
            node_ids: vec![5],
            keyset_id: 99,
            name: "e2e".into(),
            endpoint: 1,
            epoch_key: Some("42".repeat(16)),
            rebind: false,
        });
        let body = run_with_engine(&engine, &op).await.unwrap();
        assert_eq!(body["status"], "provisioned");
        assert_eq!(body["note"], PROVISION_NOTE);
    }
}
