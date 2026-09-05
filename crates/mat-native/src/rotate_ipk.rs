//! `mat fabric rotate-ipk` — IPK（keyset 0）の epoch ローテーション。直経路専用
//! （matd プロトコルには載せない、`commission` / `unpair` と同じ）。
//!
//! 状態は KVS の mat 名前空間 3 キー（`ipk-epoch` = 現行 / `ipk-epoch-next` =
//! 配布中（pending）/ `ipk-epoch-prev` = 直前）だけ。流れ:
//! 1. 現行 epoch を解決（`commission::resolve_ipk_epoch`）。
//! 2. pending ならその new epoch で再開、無ければ生成して**デバイスに触る前に**
//!    永続（途中クラッシュしても同じ鍵で再開、3 本目は生まれない）。
//! 3. 各ノードへ逐次: 現行 IPK で CASE → `KeySetWrite(0, {現行@1, 新@2})` →
//!    新 IPK で CASE を張り直して受理を実証（KeySetRead は鍵を返さない）。
//!    失敗しても続行し、per-node 結果を積む。
//! 4. 全ノード ok のときだけ commit（`group_settings::commit_ipk_rotation`、
//!    1 KvsTxn）。1 つでも失敗なら pending のまま（controller は現行 epoch で
//!    整合、デバイスは両 epoch を受理する）。
//!
//! デバイス側の旧 epoch は次回ローテーションの `{現行, 新}` 上書きで消える
//! （rolling 2 epoch）。取り残しノードは `CatchUp`（prev で CASE →
//! `{prev@1, 現行@2}` → 現行で実証）。設計: docs/superpowers/specs/
//! 2026-09-05-ipk-rotation-design.md。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use mat_controller::fabric;
use mat_controller::group_settings::{self, GroupSettingsError};
use mat_controller::kvs::{self, read_mat_ipk_epoch_slot, IpkEpochSlot, KvsError};
use mat_core::error::{ErrorKind, MatError};

use crate::{Establisher, NativeConfig, OneShotResolver, Resolver};

/// epoch 鍵 → その IPK で CASE を張る確立器を作る関数（`RotateCtx::make_establisher`
/// の型。clippy::type_complexity 対策の別名）。
type MakeEstablisher =
    Box<dyn Fn(&[u8; 16]) -> Result<Box<dyn Establisher>, MatError> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateMode {
    Rotate,
    CatchUp,
    Abort,
}

pub struct RotateIpkParams {
    pub node_ids: Vec<u64>,
    pub mode: RotateMode,
    /// 1 ノード分（書込 + 実証 CASE）の予算。0 = 無制限。
    pub per_node_timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateStatus {
    Rotated,
    Pending,
    CaughtUp,
    CatchUpIncomplete,
    Aborted,
    Idle,
}

impl RotateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RotateStatus::Rotated => "rotated",
            RotateStatus::Pending => "pending",
            RotateStatus::CaughtUp => "caught_up",
            RotateStatus::CatchUpIncomplete => "catch_up_incomplete",
            RotateStatus::Aborted => "aborted",
            RotateStatus::Idle => "idle",
        }
    }
}

#[derive(Debug)]
pub struct NodeOutcome {
    pub node_id: u64,
    pub error: Option<MatError>,
}

#[derive(Debug)]
pub struct RotateOutcome {
    pub status: RotateStatus,
    pub nodes: Vec<NodeOutcome>,
}

/// `ErrorKind` に `as_str` が無いための代替（`mat-core::body::diag_thread_success`
/// と同じ手法 — serde の snake_case 表現をそのまま文字列化する）。
fn kind_str(kind: ErrorKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl RotateOutcome {
    /// stdout 用 body（`timestamp` は `output::emit` が付ける）。鍵素材は載せない。
    pub fn body(&self, fabric_index: u8) -> Value {
        let nodes: Vec<Value> = self
            .nodes
            .iter()
            .map(|n| match &n.error {
                None => json!({ "node_id": n.node_id, "status": "ok" }),
                Some(e) => json!({
                    "node_id": n.node_id,
                    "status": "failed",
                    "error": e.to_json()["error"].clone(),
                }),
            })
            .collect();
        let mut body = json!({
            "fabric_index": fabric_index,
            "status": self.status.as_str(),
            "nodes": nodes,
        });
        if let Some(note) = self.note() {
            body["note"] = json!(note);
        }
        body
    }

    fn note(&self) -> Option<&'static str> {
        match self.status {
            RotateStatus::Rotated => Some(
                "if matd is running, restart it before the next rotation to load the new IPK; \
                 nodes left out of --nodes need `mat fabric rotate-ipk --catch-up --nodes <N>`",
            ),
            RotateStatus::Pending => Some(
                "no controller-side change yet; re-run `mat fabric rotate-ipk` with the same nodes \
                 to retry, or with --nodes <subset> to commit without the failed ones \
                 (catch them up later with --catch-up)",
            ),
            RotateStatus::CatchUpIncomplete => Some(
                "failed nodes may be two epochs behind; if --catch-up keeps failing with \
                 session_failed, re-commission them",
            ),
            _ => None,
        }
    }

    /// 部分失敗（pending / catch_up_incomplete）のとき stderr へ出す error。
    /// kind は最初に失敗したノードのもの、detail に失敗ノードを列挙する。
    pub fn partial_error(&self) -> Option<MatError> {
        let what = match self.status {
            RotateStatus::Pending => "ipk rotation pending",
            RotateStatus::CatchUpIncomplete => "ipk catch-up incomplete",
            _ => return None,
        };
        let failed: Vec<(&NodeOutcome, &MatError)> = self
            .nodes
            .iter()
            .filter_map(|n| n.error.as_ref().map(|e| (n, e)))
            .collect();
        let (_, first) = failed.first()?;
        let list = failed
            .iter()
            .map(|(n, e)| format!("node {}: {}", n.node_id, kind_str(e.kind)))
            .collect::<Vec<_>>()
            .join(", ");
        Some(MatError::new(
            first.kind,
            format!(
                "{what}: {} of {} nodes failed ({list}); see stdout for per-node detail",
                failed.len(),
                self.nodes.len()
            ),
        ))
    }
}

/// 実行に必要な材料。`run` が `NativeConfig` から組み立て、テストは fake 確立器を
/// 注入する。
pub struct RotateCtx {
    pub main_ini: PathBuf,
    pub fabric_index: u8,
    pub cfid: [u8; 8],
    pub cur_epoch: [u8; 16],
    /// epoch 鍵 → その IPK で CASE を張る確立器。
    pub make_establisher: MakeEstablisher,
}

pub async fn run(cfg: &NativeConfig, p: &RotateIpkParams) -> Result<RotateOutcome, MatError> {
    let main_ini = cfg.store.join(kvs::MAIN_INI_FILE);
    let fabric_index = cfg.fabric_index;
    // spec §3.6: --abort は pending epoch の削除だけで完結する。資格情報や
    // ipk-epoch <-> k/0 の整合が壊れたストアでも pending を消せる必要がある
    // （そういうストアを直すための第一手が --abort なことが多い）ため、
    // 資格情報の読出しより前でこの分岐を処理し、abort_only 以外には一切
    // 触れない。
    if p.mode == RotateMode::Abort {
        return abort_only(&main_ini, fabric_index);
    }
    let creds = crate::load_fabric_credentials(cfg)?;
    let cfid = fabric::compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
    let cur_epoch = crate::commission::resolve_ipk_epoch(&main_ini, fabric_index, &creds)?;
    let resolver: Arc<dyn Resolver> = Arc::new(OneShotResolver);
    let cfg = cfg.clone();
    let make = move |epoch: &[u8; 16]| {
        let mut c = creds.clone();
        c.ipk_operational = fabric::derive_ipk_operational(epoch, &cfid);
        crate::case_establisher(&cfg, c, Arc::clone(&resolver))
    };
    let ctx = RotateCtx {
        main_ini,
        fabric_index,
        cfid,
        cur_epoch,
        make_establisher: Box::new(make),
    };
    run_with(&ctx, p).await
}

pub async fn run_with(ctx: &RotateCtx, p: &RotateIpkParams) -> Result<RotateOutcome, MatError> {
    match p.mode {
        RotateMode::Abort => abort(ctx),
        RotateMode::Rotate => rotate(ctx, p).await,
        RotateMode::CatchUp => catch_up(ctx, p).await,
    }
}

fn read_slot(ctx: &RotateCtx, slot: IpkEpochSlot) -> Result<Option<[u8; 16]>, MatError> {
    read_mat_ipk_epoch_slot(&ctx.main_ini, ctx.fabric_index, slot).map_err(|e| {
        MatError::new(
            ErrorKind::StoreParse,
            format!("kvs ipk epoch ({slot:?}): {e}"),
        )
    })
}

fn map_gs_err(e: GroupSettingsError) -> MatError {
    match e {
        GroupSettingsError::Kvs(KvsError::Locked) => MatError::new(
            ErrorKind::Other,
            "controller kvs is locked by another process (concurrent rotate-ipk / provision?)",
        ),
        GroupSettingsError::Corrupt { key, reason } => MatError::new(
            ErrorKind::StoreParse,
            format!("inconsistent ipk rotation state at {key}: {reason}"),
        ),
        other => MatError::new(
            ErrorKind::Other,
            format!("controller kvs ipk rotation write failed: {other}"),
        ),
    }
}

/// CSPRNG の新 epoch（現行と一致したら引き直す）。
///
/// 鍵素材は format! に渡さない: `crate::ops::epoch_key_from_hex` のエラー経路
/// は不正 hex をそのまま detail に埋め込む（呼び出し側のバグ検出用に鍵を
/// 見せる設計）ため、生成直後の内部鍵の decode には使わず、ここで自前に
/// decode して失敗時は固定文言のみを返す。
fn fresh_epoch(cur: &[u8; 16]) -> Result<[u8; 16], MatError> {
    loop {
        let e = decode_generated_epoch_hex(&mat_core::group::generate_epoch_key())?;
        if e != *cur {
            return Ok(e);
        }
    }
}

/// `generate_epoch_key` が返す 32 桁 hex を `[u8;16]` へ。壊れているのは
/// 呼び出し側（mat-core 側の生成ロジック）のバグだが、鍵バイトそのものは
/// 絶対に detail へ出さない（固定文言のみ）。
fn decode_generated_epoch_hex(hex: &str) -> Result<[u8; 16], MatError> {
    const BAD_HEX: &str = "generated epoch key is not 32 hex chars (internal)";
    if hex.len() != 32 {
        return Err(MatError::new(ErrorKind::Other, BAD_HEX));
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| MatError::new(ErrorKind::Other, BAD_HEX))?;
    }
    Ok(out)
}

async fn rotate(ctx: &RotateCtx, p: &RotateIpkParams) -> Result<RotateOutcome, MatError> {
    let next = match read_slot(ctx, IpkEpochSlot::Next)? {
        Some(n) => {
            tracing::info!(
                fabric_index = ctx.fabric_index,
                "resuming pending ipk rotation"
            );
            n
        }
        None => {
            let n = fresh_epoch(&ctx.cur_epoch)?;
            group_settings::begin_ipk_rotation(&ctx.main_ini, ctx.fabric_index, &n)
                .map_err(map_gs_err)?;
            tracing::info!(
                fabric_index = ctx.fabric_index,
                "ipk rotation started (next epoch persisted)"
            );
            n
        }
    };
    let est_cur = (ctx.make_establisher)(&ctx.cur_epoch)?;
    let est_next = (ctx.make_establisher)(&next)?;
    let epochs = [(ctx.cur_epoch, 1u64), (next, 2u64)];
    let nodes = distribute(
        &*est_cur,
        &*est_next,
        &epochs,
        &p.node_ids,
        p.per_node_timeout_ms,
    )
    .await;
    if nodes.iter().any(|n| n.error.is_some()) {
        return Ok(RotateOutcome {
            status: RotateStatus::Pending,
            nodes,
        });
    }
    if p.node_ids.is_empty() {
        tracing::warn!("no commissioned nodes; committing ipk rotation without distributing");
    }
    group_settings::commit_ipk_rotation(
        &ctx.main_ini,
        ctx.fabric_index,
        &ctx.cfid,
        &ctx.cur_epoch,
        &next,
    )
    .map_err(map_gs_err)?;
    tracing::info!(
        fabric_index = ctx.fabric_index,
        nodes = nodes.len(),
        "ipk rotation committed"
    );
    Ok(RotateOutcome {
        status: RotateStatus::Rotated,
        nodes,
    })
}

async fn catch_up(ctx: &RotateCtx, p: &RotateIpkParams) -> Result<RotateOutcome, MatError> {
    let prev = read_slot(ctx, IpkEpochSlot::Prev)?.ok_or_else(|| {
        MatError::new(
            ErrorKind::Other,
            "no previous ipk epoch recorded for this fabric (rotate-ipk has never committed here; nothing to catch up from)",
        )
    })?;
    if read_slot(ctx, IpkEpochSlot::Next)?.is_some() {
        return Err(MatError::new(
            ErrorKind::Other,
            "an ipk rotation is pending; finish it (re-run rotate-ipk) or --abort it before --catch-up",
        ));
    }
    let est_prev = (ctx.make_establisher)(&prev)?;
    let est_cur = (ctx.make_establisher)(&ctx.cur_epoch)?;
    let epochs = [(prev, 1u64), (ctx.cur_epoch, 2u64)];
    let nodes = distribute(
        &*est_prev,
        &*est_cur,
        &epochs,
        &p.node_ids,
        p.per_node_timeout_ms,
    )
    .await;
    let status = if nodes.iter().any(|n| n.error.is_some()) {
        RotateStatus::CatchUpIncomplete
    } else {
        RotateStatus::CaughtUp
    };
    tracing::info!(
        fabric_index = ctx.fabric_index,
        status = status.as_str(),
        "ipk catch-up executed"
    );
    Ok(RotateOutcome { status, nodes })
}

fn abort(ctx: &RotateCtx) -> Result<RotateOutcome, MatError> {
    abort_only(&ctx.main_ini, ctx.fabric_index)
}

/// `--abort` の本体: `main_ini` + `fabric_index` だけで完結する（資格情報にも
/// `RotateCtx` の他のフィールドにも触れない）純粋な操作。`run()` はこれを
/// 資格情報の読出しより前に直接呼ぶ — `run_with`/`abort` 経由（テストが
/// `RotateCtx` を組み立てる経路）と同じ動作にするため、ロジックはここ 1 箇所。
fn abort_only(main_ini: &std::path::Path, fabric_index: u8) -> Result<RotateOutcome, MatError> {
    let removed = group_settings::abort_ipk_rotation(main_ini, fabric_index).map_err(map_gs_err)?;
    let status = if removed {
        RotateStatus::Aborted
    } else {
        RotateStatus::Idle
    };
    tracing::info!(
        fabric_index,
        status = status.as_str(),
        "ipk rotation abort executed"
    );
    Ok(RotateOutcome {
        status,
        nodes: Vec::new(),
    })
}

/// 各ノードへ逐次: `write_with` で CASE → KeySetWrite(0, epochs) → close →
/// `verify_with` で CASE（受理の実証）→ close。失敗しても続行。
async fn distribute(
    write_with: &dyn Establisher,
    verify_with: &dyn Establisher,
    epochs: &[([u8; 16], u64)],
    node_ids: &[u64],
    timeout_ms: u64,
) -> Vec<NodeOutcome> {
    let mut out = Vec::with_capacity(node_ids.len());
    for &node_id in node_ids {
        let step = one_node(write_with, verify_with, epochs, node_id);
        let result = if timeout_ms > 0 {
            match tokio::time::timeout(Duration::from_millis(timeout_ms), step).await {
                Ok(r) => r,
                // タイムアウト時は in-flight の `step` future をここで drop
                // する（tokio::time::timeout の仕様）ので、conn.close() は
                // 呼ばれない。直経路は one-shot（次回 run で新規に確立し直す
                // だけ）で、デバイス側も自分でセッションをタイムアウトさせる
                // ため、close せずに手放すのはここでは許容している。
                Err(_) => Err(MatError::new(
                    ErrorKind::Timeout,
                    format!(
                        "node {node_id}: ipk rotation step (key-set-write + verify-case) exceeded {timeout_ms} ms"
                    ),
                )),
            }
        } else {
            step.await
        };
        match &result {
            Ok(()) => tracing::info!(node_id, "ipk keyset written and verified"),
            Err(e) => {
                tracing::warn!(node_id, kind = ?e.kind, detail = %e.detail, "ipk keyset step failed")
            }
        }
        out.push(NodeOutcome {
            node_id,
            error: result.err(),
        });
    }
    out
}

async fn one_node(
    write_with: &dyn Establisher,
    verify_with: &dyn Establisher,
    epochs: &[([u8; 16], u64)],
    node_id: u64,
) -> Result<(), MatError> {
    let mut conn = write_with
        .establish(node_id)
        .await
        .map_err(|e| step_err(node_id, "establish", e))?;
    let written = crate::ops::write_ipk_keyset(conn.as_mut(), epochs).await;
    conn.close().await;
    written.map_err(|e| step_err(node_id, "", e))?;
    let mut conn = verify_with
        .establish(node_id)
        .await
        .map_err(|e| step_err(node_id, "verify-case", e))?;
    conn.close().await;
    Ok(())
}

fn step_err(node_id: u64, step: &str, e: MatError) -> MatError {
    let detail = if step.is_empty() {
        format!("node {node_id}: {}", e.detail)
    } else {
        format!("node {node_id}: {step}: {}", e.detail)
    };
    MatError::new(e.kind, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use mat_controller::fabric::{derive_group_session_id, derive_ipk_operational};
    use mat_controller::kvs::{mat_ipk_epoch_key, mat_ipk_epoch_slot_key, KvsTxn, MAIN_INI_FILE};
    use mat_controller::tlv::{Tag, Writer};
    use mat_core::error::ErrorKind;

    use crate::NodeConn;

    /// controller KVS の `f/<idx>/k/0`（mat 1 スロット形、`group_settings::
    /// serialize_keyset(0, 1, hash, key, 0xFFFF)` と同じバイト列）。
    fn ipk_keyset_blob(hash: u16, key: &[u8; 16]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0); // policy
        w.put_uint(Tag::Context(2), 1); // keys_count
        w.start_array(Tag::Context(3));
        for i in 0..3 {
            w.start_struct(Tag::Anonymous);
            if i == 0 {
                w.put_uint(Tag::Context(4), 1);
                w.put_uint(Tag::Context(5), u64::from(hash));
                w.put_bytes(Tag::Context(6), key);
            } else {
                w.put_uint(Tag::Context(4), 0);
                w.put_uint(Tag::Context(5), 0);
                w.put_bytes(Tag::Context(6), &[0u8; 16]);
            }
            w.end_container();
        }
        w.end_container();
        w.put_uint(Tag::Context(7), 0xFFFF);
        w.end_container();
        w.finish()
    }

    fn k0(h: &Harness) -> Vec<u8> {
        KvsTxn::open(&h.ctx.main_ini)
            .unwrap()
            .get("f/2/k/0")
            .unwrap()
            .unwrap()
    }

    use crate::test_support::FakeConn;

    const CFID: [u8; 8] = [7; 8];
    const CUR: [u8; 16] = [0x0C; 16];

    /// ノード id ごとに establish の成否を決め、払い出した conn の invoke 失敗も
    /// ノードごとに指定できる fake。`by_epoch` で「どの epoch 用の確立器か」を
    /// 記録し、テストが「E_cur で書いて E_next で実証した」順序を主張できる。
    struct NodeFake {
        label: &'static str,
        establish_fail: HashMap<u64, ErrorKind>,
        invoke_fail: HashMap<u64, ErrorKind>,
        log: std::sync::Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Establisher for NodeFake {
        async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:establish:{node_id}", self.label));
            if let Some(kind) = self.establish_fail.get(&node_id) {
                return Err(MatError::new(
                    *kind,
                    format!("fake establish failed for {node_id}"),
                ));
            }
            let fail = self.invoke_fail.get(&node_id).copied();
            Ok(Box::new(FakeConn {
                fail_first_send: fail.is_some(),
                fail_kind: fail.unwrap_or(ErrorKind::Timeout),
                ..FakeConn::scripted()
            }))
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        ctx: RotateCtx,
        log: std::sync::Arc<Mutex<Vec<String>>>,
        made: std::sync::Arc<AtomicUsize>,
    }

    /// k/0（mat 1 スロット形）+ ipk-epoch=CUR の INI を持つ RotateCtx。
    /// `establish_fail` / `invoke_fail` は全確立器に共通で適用する。
    fn harness(
        establish_fail: HashMap<u64, ErrorKind>,
        invoke_fail: HashMap<u64, ErrorKind>,
    ) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let main_ini = dir.path().join(MAIN_INI_FILE);
        std::fs::write(&main_ini, "[Default]\n").unwrap();
        let op = derive_ipk_operational(&CUR, &CFID);
        let mut txn = KvsTxn::open(&main_ini).unwrap();
        txn.set(
            "f/2/k/0",
            &ipk_keyset_blob(derive_group_session_id(&op), &op),
        );
        txn.set(&mat_ipk_epoch_key(2), &CUR);
        txn.commit().unwrap();

        let log = std::sync::Arc::new(Mutex::new(Vec::new()));
        let made = std::sync::Arc::new(AtomicUsize::new(0));
        let (log2, made2) = (std::sync::Arc::clone(&log), std::sync::Arc::clone(&made));
        let ctx = RotateCtx {
            main_ini,
            fabric_index: 2,
            cfid: CFID,
            cur_epoch: CUR,
            make_establisher: Box::new(move |epoch: &[u8; 16]| {
                made2.fetch_add(1, Ordering::SeqCst);
                // どの epoch 用かはログのラベルで見分ける。CUR 以外は "other"。
                let label = if *epoch == CUR { "cur" } else { "other" };
                Ok(Box::new(NodeFake {
                    label,
                    establish_fail: establish_fail.clone(),
                    invoke_fail: invoke_fail.clone(),
                    log: std::sync::Arc::clone(&log2),
                }) as Box<dyn Establisher>)
            }),
        };
        Harness {
            _dir: dir,
            ctx,
            log,
            made,
        }
    }

    fn slot(h: &Harness, s: IpkEpochSlot) -> Option<[u8; 16]> {
        read_mat_ipk_epoch_slot(&h.ctx.main_ini, 2, s).unwrap()
    }

    fn params(nodes: &[u64], mode: RotateMode) -> RotateIpkParams {
        RotateIpkParams {
            node_ids: nodes.to_vec(),
            mode,
            per_node_timeout_ms: 0,
        }
    }

    #[tokio::test]
    async fn rotate_all_ok_commits_and_records_prev() {
        let h = harness(HashMap::new(), HashMap::new());
        let out = run_with(&h.ctx, &params(&[5, 6], RotateMode::Rotate))
            .await
            .unwrap();
        assert_eq!(out.status, RotateStatus::Rotated);
        assert!(out.nodes.iter().all(|n| n.error.is_none()));
        assert_eq!(
            h.log.lock().unwrap().as_slice(),
            &[
                "cur:establish:5",
                "other:establish:5",
                "cur:establish:6",
                "other:establish:6",
            ]
        );
        let next = slot(&h, IpkEpochSlot::Current).unwrap();
        assert_ne!(next, CUR);
        assert_eq!(slot(&h, IpkEpochSlot::Prev), Some(CUR));
        assert_eq!(slot(&h, IpkEpochSlot::Next), None);
        let op_next = derive_ipk_operational(&next, &CFID);
        assert_eq!(
            k0(&h),
            ipk_keyset_blob(derive_group_session_id(&op_next), &op_next)
        );
        let body = out.body(2);
        assert_eq!(body["status"], "rotated");
        assert_eq!(
            body["nodes"][0],
            serde_json::json!({"node_id": 5, "status": "ok"})
        );
        assert!(body["note"].as_str().unwrap().contains("restart"));
        assert!(out.partial_error().is_none());
        // 鍵素材は body に出ない。
        let next_hex: String = next.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!body.to_string().to_lowercase().contains(&next_hex));
    }

    #[tokio::test]
    async fn rotate_with_one_failure_stays_pending_and_is_resumable() {
        let h = harness(
            HashMap::from([(6u64, ErrorKind::Unreachable)]),
            HashMap::new(),
        );
        let out = run_with(&h.ctx, &params(&[5, 6, 7], RotateMode::Rotate))
            .await
            .unwrap();
        assert_eq!(out.status, RotateStatus::Pending);
        assert_eq!(out.nodes.len(), 3, "失敗しても続行して全ノードを回る");
        assert!(out.nodes[0].error.is_none() && out.nodes[2].error.is_none());
        let e6 = out.nodes[1].error.as_ref().unwrap();
        assert_eq!(e6.kind, ErrorKind::Unreachable);
        assert!(
            e6.detail.starts_with("node 6: establish: "),
            "{}",
            e6.detail
        );
        // KVS: next だけ書かれ、current / k/0 は不変。
        let next = slot(&h, IpkEpochSlot::Next).unwrap();
        assert_eq!(slot(&h, IpkEpochSlot::Current), Some(CUR));
        assert_eq!(slot(&h, IpkEpochSlot::Prev), None);
        let op_cur = derive_ipk_operational(&CUR, &CFID);
        assert_eq!(
            k0(&h),
            ipk_keyset_blob(derive_group_session_id(&op_cur), &op_cur)
        );
        // stderr 用エラー: 最初に失敗したノードの kind、失敗ノード列挙。
        let pe = out.partial_error().unwrap();
        assert_eq!(pe.kind, ErrorKind::Unreachable);
        assert!(
            pe.detail.contains("1 of 3 nodes failed") && pe.detail.contains("node 6"),
            "{}",
            pe.detail
        );
        assert_eq!(out.body(2)["nodes"][1]["error"]["kind"], "unreachable");

        // resume: 同じ next を使い（新鍵を作らない）、成功で commit。
        let Harness {
            _dir,
            ctx,
            log,
            made,
        } = h;
        let h2 = Harness {
            _dir,
            ctx: harness_ctx_reusing(&ctx),
            log,
            made,
        };
        let out = run_with(&h2.ctx, &params(&[6], RotateMode::Rotate))
            .await
            .unwrap();
        assert_eq!(out.status, RotateStatus::Rotated);
        assert_eq!(slot(&h2, IpkEpochSlot::Current), Some(next));
        assert_eq!(slot(&h2, IpkEpochSlot::Prev), Some(CUR));
    }

    /// 同じ INI を指す、失敗設定なしの RotateCtx（resume テスト用）。
    fn harness_ctx_reusing(prev: &RotateCtx) -> RotateCtx {
        let log = std::sync::Arc::new(Mutex::new(Vec::new()));
        RotateCtx {
            main_ini: prev.main_ini.clone(),
            fabric_index: prev.fabric_index,
            cfid: prev.cfid,
            cur_epoch: prev.cur_epoch,
            make_establisher: Box::new(move |epoch: &[u8; 16]| {
                let label = if *epoch == CUR { "cur" } else { "other" };
                Ok(Box::new(NodeFake {
                    label,
                    establish_fail: HashMap::new(),
                    invoke_fail: HashMap::new(),
                    log: std::sync::Arc::clone(&log),
                }) as Box<dyn Establisher>)
            }),
        }
    }

    #[tokio::test]
    async fn rotate_key_set_write_rejection_is_failed_node() {
        let h = harness(
            HashMap::new(),
            HashMap::from([(5u64, ErrorKind::DeviceRejected)]),
        );
        let out = run_with(&h.ctx, &params(&[5], RotateMode::Rotate))
            .await
            .unwrap();
        assert_eq!(out.status, RotateStatus::Pending);
        let e = out.nodes[0].error.as_ref().unwrap();
        assert_eq!(e.kind, ErrorKind::DeviceRejected);
        assert!(
            e.detail.starts_with("node 5: key-set-write (ipk): "),
            "{}",
            e.detail
        );
        // 書込に失敗したら実証 CASE は張らない。
        assert_eq!(h.log.lock().unwrap().as_slice(), &["cur:establish:5"]);
    }

    #[tokio::test]
    async fn rotate_verify_case_failure_is_failed_node_with_step_name() {
        // "other"（= E_next）側だけ establish を落とす: 書込は成功、実証 CASE が失敗。
        let h = harness(HashMap::new(), HashMap::new());
        let log = std::sync::Arc::clone(&h.log);
        let ctx = RotateCtx {
            main_ini: h.ctx.main_ini.clone(),
            fabric_index: 2,
            cfid: CFID,
            cur_epoch: CUR,
            make_establisher: Box::new(move |epoch: &[u8; 16]| {
                let is_cur = *epoch == CUR;
                Ok(Box::new(NodeFake {
                    label: if is_cur { "cur" } else { "other" },
                    establish_fail: if is_cur {
                        HashMap::new()
                    } else {
                        HashMap::from([(5u64, ErrorKind::SessionFailed)])
                    },
                    invoke_fail: HashMap::new(),
                    log: std::sync::Arc::clone(&log),
                }) as Box<dyn Establisher>)
            }),
        };
        let out = run_with(&ctx, &params(&[5], RotateMode::Rotate))
            .await
            .unwrap();
        assert_eq!(out.status, RotateStatus::Pending);
        let e = out.nodes[0].error.as_ref().unwrap();
        assert_eq!(e.kind, ErrorKind::SessionFailed);
        assert!(
            e.detail.starts_with("node 5: verify-case: "),
            "{}",
            e.detail
        );
    }

    #[tokio::test]
    async fn rotate_with_no_nodes_commits_immediately() {
        let h = harness(HashMap::new(), HashMap::new());
        let out = run_with(&h.ctx, &params(&[], RotateMode::Rotate))
            .await
            .unwrap();
        assert_eq!(out.status, RotateStatus::Rotated);
        assert!(out.nodes.is_empty());
        assert_eq!(slot(&h, IpkEpochSlot::Prev), Some(CUR));
    }

    #[tokio::test]
    async fn rotate_per_node_timeout_marks_node_timeout_and_continues() {
        let h = harness(HashMap::new(), HashMap::new());
        let log = std::sync::Arc::clone(&h.log);
        let ctx = RotateCtx {
            main_ini: h.ctx.main_ini.clone(),
            fabric_index: 2,
            cfid: CFID,
            cur_epoch: CUR,
            make_establisher: Box::new(move |_epoch: &[u8; 16]| {
                Ok(Box::new(SlowFake {
                    log: std::sync::Arc::clone(&log),
                }) as Box<dyn Establisher>)
            }),
        };
        let p = RotateIpkParams {
            node_ids: vec![5, 6],
            mode: RotateMode::Rotate,
            per_node_timeout_ms: 50,
        };
        let out = run_with(&ctx, &p).await.unwrap();
        assert_eq!(out.status, RotateStatus::Pending);
        assert_eq!(out.nodes.len(), 2);
        assert!(out
            .nodes
            .iter()
            .all(|n| n.error.as_ref().unwrap().kind == ErrorKind::Timeout));
    }

    /// establish に 1 秒かかる fake（per-node timeout の検証用）。
    struct SlowFake {
        log: std::sync::Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Establisher for SlowFake {
        async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("slow:establish:{node_id}"));
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Ok(Box::new(FakeConn::scripted()))
        }
    }

    #[tokio::test]
    async fn catch_up_uses_prev_to_write_and_cur_to_verify() {
        let h = harness(HashMap::new(), HashMap::new());
        // prev を仕込む（commit 済みの状態）。
        {
            let mut txn = KvsTxn::open(&h.ctx.main_ini).unwrap();
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Prev), &[0x0A; 16]);
            txn.commit().unwrap();
        }
        let out = run_with(&h.ctx, &params(&[9], RotateMode::CatchUp))
            .await
            .unwrap();
        assert_eq!(out.status, RotateStatus::CaughtUp);
        assert_eq!(
            h.log.lock().unwrap().as_slice(),
            &["other:establish:9", "cur:establish:9"],
            "prev（= other）で書き、cur で実証"
        );
        // KVS は不変。
        assert_eq!(slot(&h, IpkEpochSlot::Current), Some(CUR));
        assert_eq!(slot(&h, IpkEpochSlot::Prev), Some([0x0A; 16]));
        assert_eq!(slot(&h, IpkEpochSlot::Next), None);
        assert_eq!(out.body(2)["status"], "caught_up");
    }

    #[tokio::test]
    async fn catch_up_without_prev_or_while_pending_is_other() {
        let h = harness(HashMap::new(), HashMap::new());
        let e = run_with(&h.ctx, &params(&[9], RotateMode::CatchUp))
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Other);
        assert!(e.detail.contains("no previous ipk epoch"), "{}", e.detail);
        {
            let mut txn = KvsTxn::open(&h.ctx.main_ini).unwrap();
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Prev), &[0x0A; 16]);
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Next), &[0x0E; 16]);
            txn.commit().unwrap();
        }
        let e = run_with(&h.ctx, &params(&[9], RotateMode::CatchUp))
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Other);
        assert!(e.detail.contains("pending"), "{}", e.detail);
    }

    #[tokio::test]
    async fn catch_up_failure_is_incomplete_with_partial_error() {
        let h = harness(
            HashMap::from([(9u64, ErrorKind::SessionFailed)]),
            HashMap::new(),
        );
        {
            let mut txn = KvsTxn::open(&h.ctx.main_ini).unwrap();
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Prev), &[0x0A; 16]);
            txn.commit().unwrap();
        }
        let out = run_with(&h.ctx, &params(&[9], RotateMode::CatchUp))
            .await
            .unwrap();
        assert_eq!(out.status, RotateStatus::CatchUpIncomplete);
        let pe = out.partial_error().unwrap();
        assert_eq!(pe.kind, ErrorKind::SessionFailed);
        assert!(pe.detail.contains("catch-up incomplete"), "{}", pe.detail);
        assert!(out.body(2)["note"]
            .as_str()
            .unwrap()
            .contains("re-commission"));
    }

    #[tokio::test]
    async fn abort_clears_pending_and_is_idle_otherwise() {
        let h = harness(HashMap::new(), HashMap::new());
        let out = run_with(&h.ctx, &params(&[], RotateMode::Abort))
            .await
            .unwrap();
        assert_eq!(out.status, RotateStatus::Idle);
        assert_eq!(out.body(2)["status"], "idle");
        {
            let mut txn = KvsTxn::open(&h.ctx.main_ini).unwrap();
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Next), &[0x0E; 16]);
            txn.commit().unwrap();
        }
        let out = run_with(&h.ctx, &params(&[], RotateMode::Abort))
            .await
            .unwrap();
        assert_eq!(out.status, RotateStatus::Aborted);
        assert_eq!(slot(&h, IpkEpochSlot::Next), None);
        assert_eq!(h.made.load(Ordering::SeqCst), 0, "abort は確立器を作らない");
    }

    /// `run()`（`NativeConfig` から組み立てる公開エントリ）の `--abort` が
    /// 資格情報の読出しより前で分岐することを確認する。ストアには
    /// `ipk-epoch-next` だけを置き、資格情報ファイルは一切作らない —
    /// `load_fabric_credentials` が呼ばれていれば store_missing 等で失敗する
    /// はずのところ、abort は成功する（= 資格情報に触っていない）。
    #[tokio::test]
    async fn run_abort_mode_does_not_require_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let main_ini = dir.path().join(MAIN_INI_FILE);
        std::fs::write(&main_ini, "[Default]\n").unwrap();
        {
            let mut txn = KvsTxn::open(&main_ini).unwrap();
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Next), &[0x0E; 16]);
            txn.commit().unwrap();
        }
        let cfg = NativeConfig {
            store: dir.path().to_path_buf(),
            iface: "lo".to_string(),
            thread_iface: None,
            fabric_index: 2,
            issuer_index: 0,
        };
        let out = run(&cfg, &params(&[], RotateMode::Abort)).await.unwrap();
        assert_eq!(out.status, RotateStatus::Aborted);
        assert_eq!(
            read_mat_ipk_epoch_slot(&main_ini, 2, IpkEpochSlot::Next).unwrap(),
            None
        );
    }
}
