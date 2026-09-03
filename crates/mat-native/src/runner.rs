//! セッション取得戦略の差し替え点（監査④）。
//!
//! `mat`（確立 → 1 op → close、設計ルール 4）と `matd`（per-node warm slot、
//! Timeout で 1 回だけ再確立）の違いは `NodeRunner::with_node` の実装だけ。
//! その上の `run_node` / `provision` / `grant` は両経路共通。

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;

use mat_core::body;
use mat_core::error::MatError;

use crate::op::{run_node_op, NodeOp, ProvisionParams};
use crate::{Engine, NodeConn};

/// 「node_id のセッションを取り、`f` を呼ぶ」だけを抽象化する。`Fn` なのは
/// matd が Timeout 後に再確立して `f` を 2 回目に呼ぶため。closure 環境の
/// 借用は返す Future に持ち込めない（`'a` = conn の借用のみ）— 値は
/// `async move` ブロックへ move する。
#[async_trait]
pub trait NodeRunner: Sync {
    async fn with_node<T, F>(
        &self,
        node_id: u64,
        deadline: Option<Instant>,
        f: F,
    ) -> Result<T, MatError>
    where
        T: Send,
        F: for<'a> Fn(
                &'a mut Box<dyn NodeConn>,
            )
                -> Pin<Box<dyn Future<Output = Result<T, MatError>> + Send + 'a>>
            + Send
            + Sync;
}

/// one-shot 直経路: 確立 → `f` → close → `after_close(node_id)`（`mat` は
/// ここで matd への node_touched ヒントを撃つ、Issue #20）。`deadline` は
/// 無視 — 直経路の予算は呼び出し側が future 全体に timeout を掛ける
/// （Issue #22 の「超過時も hint だけ撃つ」を保つ）。Timeout でも再確立しない。
pub struct OneShotRunner<'e, H: Fn(u64) + Send + Sync> {
    engine: &'e Engine,
    after_close: H,
}

impl<'e, H: Fn(u64) + Send + Sync> OneShotRunner<'e, H> {
    pub fn new(engine: &'e Engine, after_close: H) -> Self {
        Self {
            engine,
            after_close,
        }
    }
}

#[async_trait]
impl<H: Fn(u64) + Send + Sync> NodeRunner for OneShotRunner<'_, H> {
    async fn with_node<T, F>(
        &self,
        node_id: u64,
        _deadline: Option<Instant>,
        f: F,
    ) -> Result<T, MatError>
    where
        T: Send,
        F: for<'a> Fn(
                &'a mut Box<dyn NodeConn>,
            )
                -> Pin<Box<dyn Future<Output = Result<T, MatError>> + Send + 'a>>
            + Send
            + Sync,
    {
        let mut conn = self.engine.establisher.establish(node_id).await?;
        // 成否によらず close してから返す（Issue #20: 放置セッションは FP300 系の
        // 常駐購読を黙殺する）。
        let result = f(&mut conn).await;
        conn.close().await;
        (self.after_close)(node_id);
        result
    }
}

/// 単一ノード op を runner のセッション戦略で実行し、成功 body を返す。
pub async fn run_node(
    r: &impl NodeRunner,
    op: &NodeOp,
    deadline: Option<Instant>,
) -> Result<Value, MatError> {
    let op = op.clone();
    r.with_node(op.node_id, deadline, move |c| {
        let op = op.clone();
        Box::pin(async move { run_node_op(c.as_mut(), &op).await })
    })
    .await
}

/// `group provision`: コントローラ側 group state（KVS）→ 各ノードへデバイス側
/// 4 ステップ（unicast, acknowledged）。最初の失敗で停止する。`note` は経路
/// 依存の案内文（直経路 = KVS 直書き + matd 再起動案内、matd = None）。
/// provision は deadline 対象外（常に無制限）。
pub async fn provision(
    r: &impl NodeRunner,
    engine: &Engine,
    p: &ProvisionParams,
    note: Option<&str>,
) -> Result<Value, MatError> {
    let Some(gs) = &engine.group_settings else {
        return Err(MatError::group_ctx_unconfigured());
    };
    let epoch_key_hex = mat_core::group::resolve_epoch_key(p.epoch_key.as_deref())?;
    let epoch_key = crate::ops::epoch_key_from_hex(&epoch_key_hex)?;
    crate::group_settings::write_group_provision(
        gs,
        p.group_id,
        p.keyset_id,
        &p.name,
        &epoch_key,
        p.rebind,
    )?;
    for &node_id in &p.node_ids {
        let np = crate::ops::ProvisionNodeParams {
            group_id: p.group_id,
            keyset_id: p.keyset_id,
            name: p.name.clone(),
            endpoint: p.endpoint,
            epoch_key,
        };
        r.with_node(node_id, None, move |c| {
            let np = np.clone();
            Box::pin(async move { crate::ops::provision_node(c.as_mut(), &np).await })
        })
        .await
        .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?;
    }
    tracing::info!(
        group_id = p.group_id,
        keyset_id = p.keyset_id,
        "group provision executed"
    );
    Ok(body::group_provision_success(
        p.group_id,
        p.keyset_id,
        &p.name,
        p.endpoint,
        &p.node_ids,
        note,
    ))
}

/// `group grant`: 各ノードへ ACL read-merge-write のみ。
pub async fn grant(
    r: &impl NodeRunner,
    group_id: u16,
    node_ids: &[u64],
) -> Result<Value, MatError> {
    let mut updated: Vec<u64> = Vec::new();
    let mut unchanged: Vec<u64> = Vec::new();
    for &node_id in node_ids {
        let changed = r
            .with_node(node_id, None, move |c| {
                Box::pin(async move { crate::ops::ensure_group_acl(c.as_mut(), group_id).await })
            })
            .await
            .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?;
        if changed {
            updated.push(node_id);
        } else {
            unchanged.push(node_id);
        }
    }
    tracing::info!(group_id, "group grant executed");
    Ok(body::group_grant_success(
        group_id, node_ids, &updated, &unchanged,
    ))
}

/// `group remove`: 各ノードでデバイス側 4 ステップ（最初の失敗で停止、
/// コントローラ KVS は触らない）→ 全ノード成功後にコントローラ KVS から撤収。
pub async fn remove_group(
    r: &impl NodeRunner,
    engine: &Engine,
    group_id: u16,
    endpoint: u16,
    node_ids: &[u64],
) -> Result<Value, MatError> {
    let Some(gs) = &engine.group_settings else {
        return Err(MatError::group_ctx_unconfigured());
    };
    let mut nodes = Vec::with_capacity(node_ids.len());
    for &node_id in node_ids {
        let p = crate::ops::RemoveGroupNodeParams { group_id, endpoint };
        let rep = r
            .with_node(node_id, None, move |c| {
                let p = p.clone();
                Box::pin(async move { crate::ops::remove_group_node(c.as_mut(), &p).await })
            })
            .await
            .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?;
        nodes.push((
            node_id,
            rep.acl_removed,
            rep.group_removed,
            rep.keymap_removed,
            rep.keyset_removed,
        ));
    }
    // デバイス側は既に外れている。コントローラ KVS にその group が無い
    // （= None）のは撤収済み / 別プロセスが先に消した状態で、ここでエラーに
    // しても復旧の役には立たない — `controller.group_removed: false` で載せる。
    let (controller_removed, keyset_removed) =
        match crate::group_settings::remove_group(gs, group_id)? {
            Some(keyset_removed) => (true, keyset_removed),
            None => (false, false),
        };
    tracing::info!(group_id, nodes = node_ids.len(), "group remove executed");
    Ok(body::group_remove_success(
        group_id,
        endpoint,
        &nodes,
        controller_removed,
        keyset_removed,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::NodeOpKind;
    use crate::test_support::{FakeConn, FakeEstablisher};
    use mat_core::error::ErrorKind;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    fn read_onoff_op() -> NodeOp {
        NodeOp {
            node_id: 5,
            kind: NodeOpKind::read(1, "onoff", "on-off").unwrap(),
        }
    }

    #[tokio::test]
    async fn one_shot_closes_and_calls_after_close_on_success() {
        let est = FakeEstablisher::default();
        let close_calls = Arc::clone(&est.conn_close_calls);
        let engine = Engine::with_parts(Box::new(est), None);
        let hinted = AtomicU64::new(0);
        let runner = OneShotRunner::new(&engine, |id| hinted.store(id, Ordering::SeqCst));
        let body = run_node(
            &runner,
            &NodeOp {
                node_id: 5,
                kind: NodeOpKind::On { endpoint: 1 },
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(body["status"], "success");
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hinted.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn one_shot_closes_on_failure_and_does_not_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: Arc::clone(&calls),
            fail_first_send: true,
            fail_kind: ErrorKind::Timeout,
            ..Default::default()
        };
        let close_calls = Arc::clone(&est.conn_close_calls);
        let engine = Engine::with_parts(Box::new(est), None);
        let runner = OneShotRunner::new(&engine, |_| {});
        let err = run_node(&runner, &read_onoff_op(), None)
            .await
            .expect_err("timeout must surface");
        assert_eq!(err.kind, ErrorKind::Timeout);
        // one-shot は再確立しない（確立直後の session が stale なことはない）。
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    }

    /// `ops::provision_node` / `ensure_group_acl` が読む group-key-map / acl に
    /// 妥当な JSON（空リスト／管理者エントリのみ）を返す establisher。
    struct ScriptedEstablisher;
    #[async_trait::async_trait]
    impl crate::Establisher for ScriptedEstablisher {
        async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            Ok(Box::new(
                FakeConn::scripted()
                    .with_read(0, 0x003F, 0x0000, serde_json::json!([]))
                    .with_read(
                        0,
                        0x001F,
                        0x0000,
                        serde_json::json!([{"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]),
                    ),
            ))
        }
    }

    fn params(epoch_key: Option<String>) -> ProvisionParams {
        ProvisionParams {
            group_id: 99,
            node_ids: vec![5],
            keyset_id: 99,
            name: "e2e".into(),
            endpoint: 1,
            epoch_key,
            rebind: false,
        }
    }

    #[tokio::test]
    async fn provision_writes_controller_state_and_builds_body_with_note() {
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let mut engine = Engine::with_parts(Box::new(ScriptedEstablisher), None);
        engine.group_settings = Some(crate::group_settings::GroupSettingsCtx {
            main_ini: ini.clone(),
            fabric_index: 2,
            cfid: [7u8; 8],
        });
        let runner = OneShotRunner::new(&engine, |_| {});
        let body = provision(
            &runner,
            &engine,
            &params(Some("42".repeat(16))),
            Some("restart matd"),
        )
        .await
        .unwrap();
        assert!(mat_controller::kvs::read_group_credentials(&ini, 2, 99).is_ok());
        assert_eq!(body["status"], "provisioned");
        assert_eq!(body["nodes"], serde_json::json!([5]));
        assert_eq!(body["note"], "restart matd");
        // 2 回目は同一 group/keyset を再 provision する — rebind:false のままだと
        // `write_keymap` が `DuplicateBind` を返す（mat-controller
        // `duplicate_bind_without_rebind_is_error` と同じ規律）。note 無しの
        // body 形を確認したいだけなので rebind:true で再結線する。
        let second = ProvisionParams {
            rebind: true,
            ..params(None)
        };
        let body = provision(&runner, &engine, &second, None).await.unwrap();
        assert!(body.get("note").is_none());
    }

    #[tokio::test]
    async fn provision_hard_errors_when_group_settings_ctx_missing() {
        let engine = Engine::with_parts(Box::new(ScriptedEstablisher), None);
        let runner = OneShotRunner::new(&engine, |_| {});
        let err = provision(&runner, &engine, &params(None), None)
            .await
            .expect_err("missing group_settings ctx must hard-error");
        assert_eq!(err.kind, ErrorKind::Other);
    }

    /// `ScriptedEstablisher` に加えて、group-key-map write（`provision_node`
    /// で最初に `fail_first_send` を尊重する呼び出し — `read_json`/`invoke`
    /// は尊重しない）を `fail_kind` で失敗させる establisher。素の
    /// `FakeEstablisher` だと group-key-map read が非配列フォールバックで
    /// 先に `ParseError` になり、意図した `fail_kind` まで到達できない。
    struct ScriptedFailingEstablisher;
    #[async_trait::async_trait]
    impl crate::Establisher for ScriptedFailingEstablisher {
        async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            let mut conn = FakeConn::scripted()
                .with_read(0, 0x003F, 0x0000, serde_json::json!([]))
                .with_read(
                    0,
                    0x001F,
                    0x0000,
                    serde_json::json!([{"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]),
                );
            conn.fail_first_send = true;
            conn.fail_kind = ErrorKind::DeviceRejected;
            Ok(Box::new(conn))
        }
    }

    #[tokio::test]
    async fn provision_prefixes_node_errors_with_node_id() {
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let mut engine = Engine::with_parts(Box::new(ScriptedFailingEstablisher), None);
        engine.group_settings = Some(crate::group_settings::GroupSettingsCtx {
            main_ini: ini,
            fabric_index: 2,
            cfid: [7u8; 8],
        });
        let runner = OneShotRunner::new(&engine, |_| {});
        let err = provision(&runner, &engine, &params(None), None)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
        assert!(err.detail.starts_with("node 5: "), "{}", err.detail);
    }

    /// `remove_group` 用フィクスチャ: group 1 / keyset 42 を書いた
    /// コントローラ KVS を持つ Engine と、その INI パス。
    fn engine_with_provisioned_group(
        est: impl crate::Establisher + 'static,
        dir: &tempfile::TempDir,
    ) -> (Engine, std::path::PathBuf) {
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let gs = crate::group_settings::GroupSettingsCtx {
            main_ini: ini.clone(),
            fabric_index: 2,
            cfid: [7u8; 8],
        };
        crate::group_settings::write_group_provision(&gs, 1, 42, "grp1", &[0x42; 16], false)
            .unwrap();
        let mut engine = Engine::with_parts(Box::new(est), None);
        engine.group_settings = Some(gs);
        (engine, ini)
    }

    /// ACL に group 1 の Group エントリを返し、最初の `write_tlv`（= ACL
    /// 全置換）を `Unreachable` で落とす establisher。
    struct RemoveFailingEstablisher;
    #[async_trait::async_trait]
    impl crate::Establisher for RemoveFailingEstablisher {
        async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            let mut conn = FakeConn::scripted().with_read(
                0,
                0x001F,
                0x0000,
                serde_json::json!([
                    {"1": 5, "2": 2, "3": [112233], "4": null, "254": 1},
                    {"1": 3, "2": 3, "3": [1], "4": null, "254": 1}
                ]),
            );
            conn.fail_first_send = true;
            conn.fail_kind = ErrorKind::Unreachable;
            Ok(Box::new(conn))
        }
    }

    /// 撤収 4 ステップが全部通る establisher（ACL に group 1、group-key-map に
    /// (1, 42)、RemoveGroupResponse は status 0）。
    struct RemoveOkEstablisher;
    #[async_trait::async_trait]
    impl crate::Establisher for RemoveOkEstablisher {
        async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            let mut w = mat_controller::tlv::Writer::new();
            w.start_struct(mat_controller::tlv::Tag::Anonymous);
            w.put_uint(mat_controller::tlv::Tag::Context(0), 0); // status = SUCCESS
            w.put_uint(mat_controller::tlv::Tag::Context(1), 1); // groupID
            w.end_container();
            Ok(Box::new(
                FakeConn::scripted()
                    .with_read(
                        0,
                        0x001F,
                        0x0000,
                        serde_json::json!([
                            {"1": 5, "2": 2, "3": [112233], "4": null, "254": 1},
                            {"1": 3, "2": 3, "3": [1], "4": null, "254": 1}
                        ]),
                    )
                    .with_read(
                        0,
                        0x003F,
                        0x0000,
                        serde_json::json!([{"1": 1, "2": 42, "254": 1}]),
                    )
                    .with_invoke_response(1, 0x0004, 0x0003, w.finish()),
            ))
        }
    }

    #[tokio::test]
    async fn remove_group_stops_at_first_node_failure_and_leaves_controller_kvs() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, ini) = engine_with_provisioned_group(RemoveFailingEstablisher, &dir);
        let runner = OneShotRunner::new(&engine, |_| {});
        let err = remove_group(&runner, &engine, 1, 1, &[5])
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unreachable);
        assert!(err.detail.starts_with("node 5: "), "{}", err.detail);
        let t = mat_controller::group_settings::read_groups(&ini, 2).unwrap();
        assert_eq!(t.groups.len(), 1, "KVS は無変更");
    }

    #[tokio::test]
    async fn remove_group_reports_per_node_steps_and_drops_controller_state() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, ini) = engine_with_provisioned_group(RemoveOkEstablisher, &dir);
        let runner = OneShotRunner::new(&engine, |_| {});
        let body = remove_group(&runner, &engine, 1, 1, &[5]).await.unwrap();
        assert_eq!(body["status"], "removed");
        assert_eq!(
            body["nodes"],
            serde_json::json!([{ "node_id": 5, "acl_removed": true, "group_removed": true,
                                 "keymap_removed": true, "keyset_removed": true }])
        );
        assert_eq!(
            body["controller"],
            serde_json::json!({ "group_removed": true, "keyset_removed": true })
        );
        let t = mat_controller::group_settings::read_groups(&ini, 2).unwrap();
        assert!(t.groups.is_empty(), "コントローラ KVS から撤収済み");
    }

    /// F4: デバイス側 4 ステップが済んだ後にコントローラ KVS がその group を
    /// 持っていなくてもハードエラーにしない（デバイスはもう外れているので、
    /// エラーで落とすと「片付いていないように見える」だけで復旧の役に立たない）。
    /// `controller.group_removed: false` として body に載せる。
    #[tokio::test]
    async fn remove_group_reports_false_when_controller_kvs_lacks_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let mut engine = Engine::with_parts(Box::new(RemoveOkEstablisher), None);
        // provision せずに ctx だけ持たせる = KVS にその group が無い状態。
        engine.group_settings = Some(crate::group_settings::GroupSettingsCtx {
            main_ini: ini,
            fabric_index: 2,
            cfid: [7u8; 8],
        });
        let runner = OneShotRunner::new(&engine, |_| {});
        let body = remove_group(&runner, &engine, 1, 1, &[5]).await.unwrap();
        assert_eq!(body["status"], "removed");
        assert_eq!(
            body["controller"],
            serde_json::json!({ "group_removed": false, "keyset_removed": false })
        );
        // デバイス側の 4 ステップは通っている。
        assert_eq!(body["nodes"][0]["group_removed"], true);
    }

    #[tokio::test]
    async fn remove_group_hard_errors_when_group_settings_ctx_missing() {
        let engine = Engine::with_parts(Box::new(RemoveOkEstablisher), None);
        let runner = OneShotRunner::new(&engine, |_| {});
        let err = remove_group(&runner, &engine, 1, 1, &[5])
            .await
            .expect_err("missing group_settings ctx must hard-error");
        assert_eq!(err.kind, ErrorKind::Other);
    }

    #[tokio::test]
    async fn grant_reports_updated_nodes() {
        let engine = Engine::with_parts(Box::new(ScriptedEstablisher), None);
        let runner = OneShotRunner::new(&engine, |_| {});
        let body = grant(&runner, 10, &[5]).await.unwrap();
        assert_eq!(body["status"], "granted");
        assert_eq!(body["updated"], serde_json::json!([5]));
        assert_eq!(body["unchanged"], serde_json::json!([]));
    }
}
