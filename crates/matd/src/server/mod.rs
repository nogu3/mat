//! 上流ソケットサーバ。unix socket で newline-delimited JSON リクエストを受け、
//! native バックエンド（[`NativeBackend`]）へ中継して応答を返す。
//!
//! 応答は `mat` の one-shot CLI と同じく純粋な構造化 JSON（mat スキーマ + `timestamp`）。
//! 人間装飾は混ぜない。node_id の解決可否は毎リクエスト KVS で確認する（常駐中に
//! `mat commission` が台帳を更新しても拾えるよう、開きっぱなしにしない）。
//!
//! M8c-3: native がリクエスト処理の唯一の経路になった（chip-tool 経路を完全撤去）。
//! 起動時の native 構築失敗（KVS 資材が読めない等）は matd を落とさず、以後の全
//! リクエストへその構築エラーをそのまま返す（[`NativeState::Unavailable`]）——
//! `mat fabric init` で資材を用意すれば `matd` を再起動して解消できる。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Notify};

use mat_core::error::{ErrorKind, MatError};
use mat_core::output::now_iso8601;
use mat_core::store::Store;

use crate::native::NativeBackend;
use crate::protocol::{Op, Request};
use crate::subscription::{Emitted, SubHealth};

mod listen;
mod oplog;
mod wire;

use listen::stream_events;
pub(crate) use listen::ListenFilter;
use oplog::log_op;
use wire::{error_response, write_line};
pub(crate) use wire::{note_op_expectation, to_device_op, MatdOp};

/// native backend の構築結果。起動時に一度だけ試み、失敗しても matd 自体は
/// 常駐を続ける（M8c-3: KVS 不在でも起動し、後から `mat fabric init` できる
/// ようにする）。各リクエストはこの結果を参照する — `Unavailable` は保持した
/// 構築エラーをそのまま返す（store_missing/store_parse; mat 直経路の
/// `MatError::with_fabric_init_hint` と同じ一律化）。
pub enum NativeState {
    // Box: NativeBackend は MatError よりかなり大きく、素の enum は
    // clippy::large_enum_variant に触れる。プロセス起動時に 1 回だけ作る値
    // なので間接参照のコストは無視できる。
    Ready(Box<NativeBackend>),
    Unavailable(MatError),
}

impl NativeState {
    fn is_ready(&self) -> bool {
        matches!(self, NativeState::Ready(_))
    }
}

/// `reload` の回数と最終時刻（`status` の `reloads`）。`&self` で更新できるよう
/// Atomic + Mutex（`DaemonInfo` は `Arc` 共有）。
#[derive(Default)]
pub struct ReloadStats {
    count: std::sync::atomic::AtomicU64,
    last_at: std::sync::Mutex<Option<String>>,
}

impl ReloadStats {
    /// 成功 1 回を記録し、この回を含む累計を返す。
    pub fn record(&self) -> u64 {
        use std::sync::atomic::Ordering;
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        *self
            .last_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(now_iso8601());
        n
    }

    /// `status` 用 snapshot: `{"count": N, "last_at": "<ISO 8601>" | null}`。
    pub fn snapshot(&self) -> Value {
        use std::sync::atomic::Ordering;
        let last_at = self
            .last_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        json!({ "count": self.count.load(Ordering::SeqCst), "last_at": last_at })
    }
}

/// `status` が返すデーモン基本情報（起動時に確定する値 + reload 統計）。
pub struct DaemonInfo {
    pub version: &'static str,
    pub started: std::time::Instant,
    pub iface: String,
    pub fabric_index: u8,
    pub reloads: ReloadStats,
}

/// ソケットを bind し、接続を受け付け続ける。`Ctrl-C` で抜ける。
pub async fn serve(
    socket_path: &Path,
    store_path: PathBuf,
    native: Arc<NativeState>,
    events: broadcast::Sender<Emitted>,
    health: Arc<SubHealth>,
    daemon: Arc<DaemonInfo>,
) -> std::io::Result<()> {
    tracing::info!(native_ready = native.is_ready(), "matd backend");
    // 前回の残骸を掃除してから bind。
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }
    let listener = UnixListener::bind(socket_path)?;
    tracing::info!(socket = %socket_path.display(), "matd listening");

    // shutdown op（`matd stop`）で serve ループを抜けるための通知。
    let shutdown = Arc::new(Notify::new());

    let store_path = Arc::new(store_path);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = accepted?;
                let native = Arc::clone(&native);
                let store_path = Arc::clone(&store_path);
                let shutdown = Arc::clone(&shutdown);
                let events = events.clone();
                let health = Arc::clone(&health);
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_conn(stream, native, store_path, shutdown, events, health, daemon)
                            .await
                    {
                        tracing::warn!(error = %e, "connection handler ended with error");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl-C, shutting down");
                break;
            }
            _ = shutdown.notified() => {
                tracing::info!("received shutdown op, shutting down");
                break;
            }
        }
    }

    // graceful shutdown: socket を消して抜ける（native セッションは warm 保持のみ
    // で子プロセスを持たないため、明示的な teardown は不要）。
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// 1 接続。複数行のリクエストを順に処理し、各行に 1 行 JSON で応答する。
///
/// `listen` op だけは例外: ack 1 行を送った後、この接続を占有してフィルタ一致
/// イベントを流し続ける（`stream_events` に委譲して抜ける）。
async fn handle_conn(
    stream: UnixStream,
    native: Arc<NativeState>,
    store_path: Arc<PathBuf>,
    shutdown: Arc<Notify>,
    events: broadcast::Sender<Emitted>,
    health: Arc<SubHealth>,
    daemon: Arc<DaemonInfo>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let mut pending_line: Option<String> = None;
    loop {
        let line = match pending_line.take() {
            Some(l) => l,
            None => match lines.next_line().await? {
                Some(l) => l,
                None => break,
            },
        };
        if line.trim().is_empty() {
            continue;
        }
        // listen だけは「ack 1 行 + 以後ストリーム」の例外。この接続を占有する。
        if let Ok(req) = serde_json::from_str::<Request>(&line) {
            if let Op::Listen {
                node_id,
                endpoint,
                cluster,
                attribute,
                event,
            } = &req.op
            {
                let filter = match ListenFilter::from_op(
                    node_id, endpoint, cluster, attribute, event,
                ) {
                    Ok(f) => f,
                    Err(e) => {
                        // listen 経路は attach/detach/lag を記録しているので、
                        // 受け付けられなかった場合も残す（この op は dispatch に
                        // 到達しないため op ログには出ない）。
                        tracing::info!(kind = ?e.kind, detail = %e.detail, "listen client rejected");
                        write_line(&mut write_half, &error_response(req.id, &e)).await?;
                        return Ok(());
                    }
                };
                // ack より先に subscribe（ack 直後のイベントを取りこぼさない）。
                let rx = events.subscribe();
                let mut ack = json!({ "timestamp": now_iso8601(), "listening": true });
                if let (Value::Object(map), Some(id)) = (&mut ack, req.id) {
                    map.insert("id".into(), id);
                }
                write_line(&mut write_half, &ack).await?;
                // 「センサーが反応しなかった」の切り分けに、購読者が居たか
                // どうかを残す。フィルタは全て Option なので未指定は省略される。
                // `scripts/e2e-device-m3.sh` はこのログの `"listen client
                // attached"` という文字列を verbatim に grep して attach 検知
                // している — この文字列を変えるならスクリプト側も直すこと。
                tracing::info!(
                    node_id = filter.node_id,
                    endpoint = filter.endpoint,
                    cluster = filter.cluster,
                    attribute = filter.attribute,
                    "listen client attached"
                );
                return stream_events(rx, filter, &mut lines, &mut write_half).await;
            }
        }
        let started = std::time::Instant::now();
        // ブロックスコープで dispatch future の寿命を区切る: ClientGone で break
        // した時点ではまだ future が per-node Mutex を握っている可能性があり、
        // その状態で abort_op（slot 破棄の lock().await）を呼ぶとデッドロック
        // する。ブロックを抜けて future を drop してから後始末する。
        let turn = {
            let dispatch_fut = dispatch(&line, &native, &store_path, &health, &daemon, &events);
            tokio::pin!(dispatch_fut);
            loop {
                tokio::select! {
                    res = &mut dispatch_fut => break OpTurn::Done(res),
                    // op 実行中の追加行は 1 行だけバッファ（逐次セマンティクス維持）。
                    // バッファ済みなら次の行は読まない（取りこぼし防止）。
                    next = lines.next_line(), if pending_line.is_none() => match next {
                        Ok(Some(l)) => pending_line = Some(l),
                        // クライアント切断: op を破棄する。future drop で per-node
                        // Mutex が解放され、後続 op の head-of-line blocking が
                        // 消える（Issue #16）。応答は書かない（相手がいない）。
                        _ => break OpTurn::ClientGone,
                    },
                }
            }
        }; // ← ここで dispatch future が drop され、Mutex が解放される
        let (response, is_shutdown) = match turn {
            OpTurn::Done(res) => res,
            OpTurn::ClientGone => {
                abort_op(&line, &native, started).await;
                return Ok(());
            }
        };
        write_line(&mut write_half, &response).await?;
        // 応答をワイヤに出し切ってから停止を発火する（クライアントが確実に受け取る）。
        if is_shutdown {
            shutdown.notify_one();
            break;
        }
    }
    Ok(())
}

/// 1 op の帰結: 応答あり（通常）か、クライアント切断で放棄したか。
enum OpTurn {
    Done((Value, bool)),
    ClientGone,
}

/// クライアント切断で放棄された op の後始末: 観測ログ + 単一ノード op なら
/// slot 破棄（drop された op future が session を中途 exchange のまま残しうる）。
/// `line` の再パースは切断時のみのコストで、通常経路には乗らない。
async fn abort_op(line: &str, native: &NativeState, started: std::time::Instant) {
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (op_name, node_id) = match serde_json::from_str::<Request>(line) {
        Ok(req) => (req.op.name(), req.op.node_id()),
        Err(_) => ("unknown", None),
    };
    tracing::warn!(
        op = op_name,
        node_id,
        elapsed_ms,
        "op aborted (client disconnected)"
    );
    if let (Some(node_id), NativeState::Ready(b)) = (node_id, native) {
        b.drop_session(node_id).await;
    }
}

/// deadline_ms 未指定（旧 mat クライアント）の単一ノード op に適用する既定予算。
/// per-node Mutex の無期限保持を防ぐ受け皿（Issue #16）。
const DEFAULT_OP_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// リクエストの deadline_ms を絶対時刻へ変換する（単一ノード op 用）。
/// `Some(0)` = 明示無制限、`Some(n)` = n ms、`None`（旧クライアント）= 既定 60s。
fn op_deadline(deadline_ms: Option<u64>) -> Option<std::time::Instant> {
    match deadline_ms {
        Some(0) => None,
        Some(n) => Some(std::time::Instant::now() + std::time::Duration::from_millis(n)),
        None => Some(std::time::Instant::now() + DEFAULT_OP_BUDGET),
    }
}

/// 1 リクエスト行を処理して応答 JSON を組み立てる。戻り値の bool は shutdown 要求か。
async fn dispatch(
    line: &str,
    native: &NativeState,
    store_path: &Path,
    health: &SubHealth,
    daemon: &DaemonInfo,
    events: &broadcast::Sender<Emitted>,
) -> (Value, bool) {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            // op ログの唯一の穴を塞ぐ。未知の op（新しい mat ↔ 古い matd の
            // バージョン差異）もここに来るので、無音だと切り分けができない。
            // `line` 自体は出さない — 要求のペイロードを journald に残さない。
            tracing::info!(kind = ?ErrorKind::ParseError, detail = %e, "matd request rejected");
            return (
                error_response(
                    None,
                    &MatError::parse_error(format!("invalid request JSON: {e}")),
                ),
                false,
            );
        }
    };
    let id = req.id.clone();
    let is_shutdown = matches!(req.op, Op::Shutdown);

    // op の所要時間は run_op のみを測る（JSON パース・応答書き込みは含めない）。
    let started = std::time::Instant::now();
    let deadline = op_deadline(req.deadline_ms);
    // status はレジストリ snapshot の JSON 化のみ（デバイス・ワイヤに触れず
    // per-node Mutex も取らない）— run_op を通さず dispatch で完結する。
    let result = match &req.op {
        Op::Status => Ok(status_body(native, store_path, daemon, health, events)),
        // native 不要・per-node Mutex 不要 — SubHealth に合図して即 ack。
        // 再購読完了は待たない（ヒントは fire-and-forget が契約、Issue #20）。
        Op::NodeTouched { node_id } => {
            health.note_touched(*node_id);
            tracing::info!(
                node_id = *node_id,
                source = "external",
                "node touched; resubscribing"
            );
            Ok(json!({ "resubscribing": true }))
        }
        // 確立器の資格情報を差し替えるだけ（warm session / 購読 / per-node
        // Mutex には触れない）。失敗時はメモリも回数も変えない。
        Op::Reload => reload_body(native, daemon),
        _ => run_op(&req.op, native, store_path, health, deadline).await,
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    log_op(&req.op, &result, elapsed_ms);

    let body = match result {
        Ok(mut body) => {
            // id をエコーし、timestamp を必ず付ける（mat スキーマ規約）。
            if let Value::Object(map) = &mut body {
                if let Some(id) = id {
                    map.insert("id".into(), id);
                }
                map.entry("timestamp".to_string())
                    .or_insert_with(|| Value::String(now_iso8601()));
            }
            body
        }
        Err(e) => error_response(id, &e),
    };
    (body, is_shutdown)
}

/// 操作を実行し、mat スキーマの成功ボディ（timestamp 抜き）を返す。応答は `mat` の
/// one-shot CLI と同じ純粋スキーマ。
///
/// M8c-3: native が唯一の経路。`NativeState::Unavailable`（起動時の構築失敗）は
/// 全 op（Ping/Shutdown を除く）へそのエラーをそのまま返す。native 構築済みでも
/// 名前解決できない cluster/attribute/command（chip-tool 互換の任意名を受けられた
/// 旧経路の名残）は [`MatError::unresolved_op`] で即 parse_error にする — フォールバック
/// 先が無いため（数値 ID は resolve 済みなので影響しない）。
///
/// 名前解決・値符号化は `to_device_op` → `mat_native::op` に集約（監査④）。
/// 未解決名は `require_node` より先に `parse_error` になる（mat 直経路と同順）。
async fn run_op(
    op: &Op,
    native: &NativeState,
    store_path: &Path,
    health: &SubHealth,
    deadline: Option<std::time::Instant>,
) -> Result<Value, MatError> {
    // Ping / Shutdown は native に触れず即応。
    match op {
        Op::Ping => return Ok(json!({ "pong": true })),
        Op::Shutdown => return Ok(json!({ "stopping": true })),
        // listen は handle_conn が行パース段階で先取りしてストリームへ分岐する
        // ため、ここには到達しない（防御的に拒否する）。
        Op::Listen { .. } => {
            return Err(MatError::parse_error("listen must be the streaming path"))
        }
        // status は dispatch が先取りする（防御的に拒否）。
        Op::Status => return Err(MatError::parse_error("status is handled in dispatch")),
        _ => {}
    }

    let native = match native {
        NativeState::Ready(n) => n,
        NativeState::Unavailable(e) => return Err(e.clone()),
    };

    match to_device_op(op)? {
        MatdOp::Node(node_op) => {
            // commission 済みか毎回 KVS で確認する。
            require_node(store_path, node_op.node_id)?;
            let body = mat_native::runner::run_node(native.as_ref(), &node_op, deadline).await?;
            // 前提: デバイスは invoke 応答を先に、購読 report を後に送る。
            // report が note_op より先に pump へ届く逆順だと pending が残り
            // 健全購読を 1 回余分に再購読するが、それが最悪ケース。
            note_op_expectation(op, health);
            Ok(body)
        }
        MatdOp::Group(group_op) => {
            // chip-tool 撤去前と同じ前提チェック（store が開けること）。
            let _store = Store::open(store_path)?;
            mat_native::op::run_group_op(native.engine(), &group_op).await
        }
        MatdOp::Provision(p) => {
            let store = Store::open(store_path)?;
            // 全ノードが commission 済みか先に確認（1つでも未登録なら停止）。
            for &node_id in &p.node_ids {
                store.require_node(node_id)?;
            }
            // matd 経路の provision は note 無し（KVS は matd 自身が書くため
            // 再起動案内は不要）。
            mat_native::runner::provision(native.as_ref(), native.engine(), &p, None).await
        }
        MatdOp::Bump => {
            let _store = Store::open(store_path)?;
            mat_native::op::run_group_bump(native.engine()).await
        }
    }
}

/// store を開いて node_id が commission 済みか確認する（常駐中の台帳更新を拾うよう
/// 毎回開き直す）。
fn require_node(store_path: &Path, node_id: u64) -> Result<(), MatError> {
    Store::open(store_path)?.require_node(node_id)?;
    Ok(())
}

/// `reload`: 確立器へ KVS の資格情報（IPK を含む）を読み直させ、成功なら回数を
/// 進める。`Unavailable`（起動時の構築失敗）は他 op と同じくそのエラーを返す —
/// reload での復帰はスコープ外（restart が唯一の復帰手段）。
fn reload_body(native: &NativeState, daemon: &DaemonInfo) -> Result<Value, MatError> {
    let backend = match native {
        NativeState::Ready(b) => b,
        NativeState::Unavailable(e) => return Err(e.clone()),
    };
    let changed = backend.engine().reload_credentials()?;
    let count = daemon.reloads.record();
    tracing::info!(
        ipk_changed = changed,
        reload_count = count,
        "credentials reloaded from kvs"
    );
    Ok(json!({
        "reloaded": true,
        "ipk": if changed { "changed" } else { "unchanged" },
        "reload_count": count,
    }))
}

/// `status` op の応答ボディ（timestamp / id は dispatch が付ける）。
fn status_body(
    native: &NativeState,
    store_path: &Path,
    daemon: &DaemonInfo,
    health: &SubHealth,
    events: &broadcast::Sender<Emitted>,
) -> Value {
    let native_json = match native {
        NativeState::Ready(_) => json!("ready"),
        NativeState::Unavailable(e) => json!({ "kind": e.kind, "detail": e.detail }),
    };
    // subscriptions.toml 由来の絞り込み。ids に無いクラスタは数値のまま
    // （listen イベントの Event::to_json と同じ規律）。無し = wildcard = null。
    let clusters = health.clusters().map(|ids| {
        ids.iter()
            .map(|&id| match mat_core::ids::find_cluster(id) {
                Some(def) => json!(def.name),
                None => json!(id),
            })
            .collect::<Vec<_>>()
    });
    json!({
        "version": daemon.version,
        "uptime_s": daemon.started.elapsed().as_secs(),
        "native": native_json,
        "iface": daemon.iface,
        "fabric_index": daemon.fabric_index,
        "store": store_path.display().to_string(),
        "subscribed_clusters": clusters,
        "reloads": daemon.reloads.snapshot(),
        "listen_clients": events.receiver_count(),
        "nodes": health.status_nodes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Op;

    /// status は Unavailable でも応答し、構築エラーと空 nodes が見える。
    /// subscribed_clusters は ids 名で返る。
    #[tokio::test]
    async fn dispatch_status_reports_native_unavailable() {
        let (_dir, store_path) = make_store();
        let state = NativeState::Unavailable(MatError::store_missing("no KVS materials"));
        let health = SubHealth::new(Some(vec![0x0006]));
        let daemon = test_daemon();
        let (events, rx) = tokio::sync::broadcast::channel::<crate::subscription::Emitted>(8);
        drop(rx);

        let (body, is_shutdown) = dispatch(
            r#"{"op":"status","id":3}"#,
            &state,
            &store_path,
            &health,
            &daemon,
            &events,
        )
        .await;

        assert!(!is_shutdown);
        assert_eq!(body["id"], 3);
        assert_eq!(body["native"]["kind"], "store_missing");
        assert_eq!(body["native"]["detail"], "no KVS materials");
        assert_eq!(body["version"], "test");
        assert_eq!(body["iface"], "lo");
        assert_eq!(body["fabric_index"], 2);
        assert_eq!(body["store"], store_path.display().to_string());
        assert_eq!(body["subscribed_clusters"], json!(["onoff"]));
        assert_eq!(body["listen_clients"], 0);
        assert!(body["nodes"].as_array().unwrap().is_empty());
        assert!(body["uptime_s"].is_u64());
        assert!(body["timestamp"].is_string());
    }

    fn test_daemon() -> DaemonInfo {
        DaemonInfo {
            version: "test",
            started: std::time::Instant::now(),
            iface: "lo".into(),
            fabric_index: 2,
            reloads: ReloadStats::default(),
        }
    }

    /// reload に成功する確立器（Task 1 の既定実装を上書き）。establish は使わない。
    struct ReloadOkEstablisher {
        changed: bool,
    }
    #[async_trait::async_trait]
    impl mat_native::Establisher for ReloadOkEstablisher {
        async fn establish(
            &self,
            _node_id: u64,
        ) -> Result<Box<dyn mat_native::NodeConn>, MatError> {
            Err(MatError::new(ErrorKind::Other, "not used"))
        }
        fn reload_credentials(&self) -> Result<bool, MatError> {
            Ok(self.changed)
        }
    }

    /// reload 成功: 応答形・回数・status への反映。
    #[tokio::test]
    async fn dispatch_reload_swaps_credentials_and_counts() {
        let (_dir, store_path) = make_store();
        let native =
            NativeBackend::with_establisher(Box::new(ReloadOkEstablisher { changed: true }));
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);
        let daemon = test_daemon();
        let (events, rx) = tokio::sync::broadcast::channel::<crate::subscription::Emitted>(8);
        drop(rx);

        let (body, is_shutdown) = dispatch(
            r#"{"op":"reload","id":9}"#,
            &state,
            &store_path,
            &health,
            &daemon,
            &events,
        )
        .await;
        assert!(!is_shutdown);
        assert_eq!(body["id"], 9);
        assert_eq!(body["reloaded"], true);
        assert_eq!(body["ipk"], "changed");
        assert_eq!(body["reload_count"], 1);
        assert!(body["timestamp"].is_string());

        let (status, _) = dispatch(
            r#"{"op":"status"}"#,
            &state,
            &store_path,
            &health,
            &daemon,
            &events,
        )
        .await;
        assert_eq!(status["reloads"]["count"], 1);
        assert!(status["reloads"]["last_at"].is_string());

        // 2 回目は IPK が動かなかった確立器で: `ipk` は unchanged に写り、
        // それでも reload 自体は成功なので回数は進む。
        let unchanged = NativeState::Ready(Box::new(NativeBackend::with_establisher(Box::new(
            ReloadOkEstablisher { changed: false },
        ))));
        let (body, _) = dispatch(
            r#"{"op":"reload"}"#,
            &unchanged,
            &store_path,
            &health,
            &daemon,
            &events,
        )
        .await;
        assert_eq!(body["reloaded"], true);
        assert_eq!(body["ipk"], "unchanged");
        assert_eq!(body["reload_count"], 2);
    }

    /// 確立器が reload 非対応（既定実装）→ other、回数は進まず status も未 reload。
    #[tokio::test]
    async fn dispatch_reload_unsupported_establisher_is_other_and_not_counted() {
        use crate::native::test_support::FakeEstablisher;
        let (_dir, store_path) = make_store();
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);
        let daemon = test_daemon();
        let (events, rx) = tokio::sync::broadcast::channel::<crate::subscription::Emitted>(8);
        drop(rx);

        let (body, _) = dispatch(
            r#"{"op":"reload"}"#,
            &state,
            &store_path,
            &health,
            &daemon,
            &events,
        )
        .await;
        assert_eq!(body["error"]["kind"], "other");
        assert!(body["error"]["detail"]
            .as_str()
            .unwrap()
            .contains("not supported"));

        let (status, _) = dispatch(
            r#"{"op":"status"}"#,
            &state,
            &store_path,
            &health,
            &daemon,
            &events,
        )
        .await;
        assert_eq!(status["reloads"]["count"], 0);
        assert!(status["reloads"]["last_at"].is_null());
    }

    /// native が起動時 Unavailable → そのエラーをそのまま返す（他 op と同じ規律）。
    #[tokio::test]
    async fn dispatch_reload_reports_native_unavailable() {
        let (_dir, store_path) = make_store();
        let state = NativeState::Unavailable(MatError::store_missing("no KVS materials"));
        let health = SubHealth::new(None);
        let daemon = test_daemon();
        let (events, rx) = tokio::sync::broadcast::channel::<crate::subscription::Emitted>(8);
        drop(rx);

        let (body, _) = dispatch(
            r#"{"op":"reload"}"#,
            &state,
            &store_path,
            &health,
            &daemon,
            &events,
        )
        .await;
        assert_eq!(body["error"]["kind"], "store_missing");
        assert_eq!(body["error"]["detail"], "no KVS materials");
    }

    #[test]
    fn op_deadline_semantics() {
        // Some(0) = 明示無制限。
        assert!(op_deadline(Some(0)).is_none());
        // Some(n) = 今 + n ms（下限だけ確認 — 実行遅延で厳密比較はしない）。
        let d = op_deadline(Some(5_000)).expect("finite budget");
        assert!(d <= std::time::Instant::now() + std::time::Duration::from_millis(5_000));
        // None（旧クライアント）= 既定 60s。
        let d = op_deadline(None).expect("default budget");
        assert!(d > std::time::Instant::now() + std::time::Duration::from_secs(59));
    }

    use crate::native::test_support::{write_group_fixture_ini, FakeEstablisher};
    use std::path::PathBuf;

    pub(super) fn group_on_op() -> Op {
        Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec![],
            endpoint: 1,
        }
    }

    fn make_store() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 1,
                commissioned_at: "2026-06-08T00:00:00+09:00".into(),
            })
            .unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    /// commission 済みノード node_id=5 を持つ一時 store（native_op の汎用
    /// read/write テスト用フィクスチャ、M8a Task10）。
    fn store_with_node_5() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                commissioned_at: "2026-06-08T00:00:00+09:00".into(),
            })
            .unwrap();
        dir
    }

    #[tokio::test]
    async fn native_generic_read_body_matches_expected_schema() {
        // FakeConn の read_json は json!(1) を返す（Task 6 の fake 仕様）。
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);
        let op = Op::Read {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: Some("current-level".into()),
        };
        let body = run_op(&op, &state, store_with_node_5().path(), &health, None)
            .await
            .unwrap();
        // 既存 hotpath_success_body(Read) と同形（node_id/endpoint/cluster/attribute/value）。
        assert_eq!(body["node_id"], 5);
        assert_eq!(body["endpoint"], 1);
        assert_eq!(body["cluster"], "levelcontrol");
        assert_eq!(body["attribute"], "current-level");
        assert!(body["value"].is_number());
    }

    #[tokio::test]
    async fn native_write_rejects_bad_json_shape_with_parse_error() {
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);
        let op = Op::Write {
            node_id: 5,
            endpoint: 0,
            cluster: "accesscontrol".into(),
            attribute: "acl".into(),
            value: "{}".into(),
            timed: false,
        };
        let err = run_op(&op, &state, store_with_node_5().path(), &health, None)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(
            err.detail.contains("expected a JSON array"),
            "{}",
            err.detail
        );
    }

    #[tokio::test]
    async fn native_generic_invoke_and_describe_bodies_match_expected_schema() {
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);
        let dir = store_with_node_5();

        let invoke = Op::Invoke {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            command: "move-to-level".into(),
            args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
            timed: false,
        };
        let body = run_op(&invoke, &state, dir.path(), &health, None)
            .await
            .unwrap();
        // 既存 simple_op(Invoke) と同形（node_id/endpoint/cluster/command/status）。
        assert_eq!(body["node_id"], 5);
        assert_eq!(body["endpoint"], 1);
        assert_eq!(body["cluster"], "levelcontrol");
        assert_eq!(body["command"], "move-to-level");
        assert_eq!(body["status"], "success");

        let describe = Op::Describe { node_id: 5 };
        let body = run_op(&describe, &state, dir.path(), &health, None)
            .await
            .unwrap();
        // node_id/endpoints[].{endpoint,clusters} の形。
        assert_eq!(body["node_id"], 5);
        let endpoints = body["endpoints"].as_array().unwrap();
        assert!(!endpoints.is_empty());
        assert!(endpoints[0].get("endpoint").is_some());
        assert!(endpoints[0]["clusters"].is_array());
    }

    /// `mat_native::ops::provision_node` が読む group-key-map / acl に妥当な
    /// JSON（空リスト／管理者エントリのみ）を返す scripted `FakeConn` を確立する
    /// establisher（`ops.rs` の `provision_node_runs_steps_in_order` と同じ
    /// フィクスチャ形）。
    struct ScriptedEstablisher;
    #[async_trait::async_trait]
    impl mat_native::Establisher for ScriptedEstablisher {
        async fn establish(
            &self,
            _node_id: u64,
        ) -> Result<Box<dyn mat_native::NodeConn>, MatError> {
            Ok(Box::new(
                crate::native::test_support::FakeConn::with_group_provision_fixture(),
            ))
        }
    }

    /// M8c-3: group_provision はコントローラ側 group state・デバイス側ともに
    /// 常に native（group_settings を注入すれば KVS への実書込みまで検証できる）。
    #[tokio::test]
    async fn group_provision_writes_controller_and_device_state_natively() {
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let gs = mat_native::group_settings::GroupSettingsCtx {
            main_ini: ini.clone(),
            fabric_index: 2,
            cfid: [7u8; 8],
        };
        let native = NativeBackend::with_parts_gs(Box::new(ScriptedEstablisher), None, Some(gs));
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);

        let (_dir2, store_path) = make_store();
        let op = Op::GroupProvision {
            group_id: 99,
            node_ids: vec![1],
            keyset_id: 99,
            name: "e2e".into(),
            endpoint: 1,
            epoch_key: None,
            rebind: false,
        };
        let body = run_op(&op, &state, &store_path, &health, None)
            .await
            .unwrap();
        assert_eq!(body["status"], "provisioned");
        assert_eq!(body["nodes"], json!([1]));
        assert!(mat_controller::kvs::read_group_credentials(&ini, 2, 99).is_ok());
    }

    /// group_settings が未構成（テスト注入時のみ起こり得る）だと internal エラー。
    #[tokio::test]
    async fn group_provision_without_group_settings_ctx_is_internal_error() {
        let native = NativeBackend::with_establisher(Box::new(ScriptedEstablisher));
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);
        let (_dir, store_path) = make_store();
        let op = Op::GroupProvision {
            group_id: 1,
            node_ids: vec![1],
            keyset_id: 1,
            name: "g".into(),
            endpoint: 1,
            epoch_key: None,
            rebind: false,
        };
        let err = run_op(&op, &state, &store_path, &health, None)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
    }

    #[tokio::test]
    async fn group_op_routes_native_when_available() {
        let (_dir, store_path) = make_store();
        let ini = store_path.join("chip_tool_config.ini");
        write_group_fixture_ini(&ini);

        // `lo` lacks IFF_MULTICAST; reuse the runtime interface-discovery
        // helper shared with native.rs's own multicast test.
        let mut sent = false;
        for cand in crate::native::test_support::multicast_capable_interfaces() {
            let recv = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
            let port = recv.local_addr().unwrap().port();
            if recv
                .join_multicast_v6(
                    &mat_controller::group::group_multicast_addr(1, 10),
                    cand.index,
                )
                .is_err()
            {
                continue;
            }
            let transport = std::sync::Arc::new(
                mat_controller::transport::UdpTransport::bind()
                    .await
                    .unwrap(),
            );
            let ctx = crate::native::GroupCtx {
                main_ini: ini.clone(),
                counter_path: store_path.join(format!("native_group_counter-{}", cand.index)),
                fabric_index: 2,
                fabric_id: 1,
                node_id: 0x0001_0001,
                egress: vec![mat_controller::group::GroupEgress {
                    iface: cand.name.clone(),
                    transport,
                    scope_id: cand.index,
                }],
                dest_port: port,
                op_iface: cand.name.clone(),
                thread_retry: false,
                sender: tokio::sync::Mutex::new(None),
            };
            let native = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), Some(ctx));

            // A send failure just moves on to the next candidate (same
            // treatment as a join failure): docker0 / veth* / WSL2's
            // loopback0 advertise IFF_UP|IFF_MULTICAST but carry no IPv6
            // source address, so the ff35::/16 send fails with EADDRNOTAVAIL
            // — a real NIC candidate can still deliver.
            let body = match run_op(
                &group_on_op(),
                &NativeState::Ready(Box::new(native)),
                &store_path,
                &SubHealth::new(None),
                None,
            )
            .await
            {
                Ok(b) => b,
                Err(_) => continue,
            };
            assert_eq!(body["status"], "sent"); // native 経路のみで成功
            let mut buf = [0u8; 1280];
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                recv.recv_from(&mut buf),
            )
            .await;
            if result.is_ok() {
                sent = true;
                break;
            }
        }
        assert!(
            sent,
            "no multicast-capable interface delivered the groupcast datagram \
             (lo excluded — it lacks IFF_MULTICAST on Linux)"
        );
    }

    #[tokio::test]
    async fn group_op_hard_errors_when_group_ctx_unavailable() {
        let (_dir, store_path) = make_store();
        // group ctx なしの native → `mat_native::op::run_group_op` が
        // `MatError::group_ctx_unconfigured()`（Other）で即返す。監査④で
        // matd の group 送信も `mat` 直経路と同じ `mat_native::op` を経由する
        // ようになったため、旧 store_parse（`GroupOutcome::Unavailable` 経由の
        // matd 固有マッピング）ではなく Other に統一された（本番 `Engine::build`
        // では group ctx は常に `Some` — テスト注入時のみ到達）。
        let native = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), None);
        let err = run_op(
            &group_on_op(),
            &NativeState::Ready(Box::new(native)),
            &store_path,
            &SubHealth::new(None),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.contains("native group context not configured"));
    }

    /// Issue #14 応急コマンド: group ctx が構成済みなら `Op::GroupBump` は
    /// counter を fresh counter file の lazy init 直後の窓（2*COUNTER_EPOCH =
    /// 8192）だけジャンプし、from/to を body へ載せる。bump は送信を伴わない
    /// ため multicast join 可否に依存しない（scope_id は lo で足りる）。
    #[tokio::test]
    async fn group_bump_dispatch_reports_from_and_to() {
        let (_dir, store_path) = make_store();
        let ini = store_path.join("chip_tool_config.ini");
        write_group_fixture_ini(&ini);
        let counter_path = store_path.join("native_group_counter-bump-test");
        let _ = std::fs::remove_file(&counter_path);
        let transport = std::sync::Arc::new(
            mat_controller::transport::UdpTransport::bind()
                .await
                .unwrap(),
        );
        let ctx = crate::native::GroupCtx {
            main_ini: ini,
            counter_path,
            fabric_index: 2,
            fabric_id: 1,
            node_id: 0x0001_0001,
            egress: vec![mat_controller::group::GroupEgress {
                iface: "lo".into(), // 送信しないので join 可否は無関係
                transport,
                scope_id: 1,
            }],
            dest_port: 5540,
            op_iface: "lo".into(),
            thread_retry: false,
            sender: tokio::sync::Mutex::new(None),
        };
        let native = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), Some(ctx));

        let body = run_op(
            &Op::GroupBump,
            &NativeState::Ready(Box::new(native)),
            &store_path,
            &SubHealth::new(None),
            None,
        )
        .await
        .unwrap();

        let from = body["group_counter"]["from"]
            .as_u64()
            .expect("group_counter.from present");
        let to = body["group_counter"]["to"]
            .as_u64()
            .expect("group_counter.to present");
        assert_eq!(to - from, 8192);
    }

    #[tokio::test]
    async fn run_op_returns_build_error_uniformly_when_native_unavailable() {
        // 起動時 native 構築失敗（KVS 不在等）は、Ping/Shutdown 以外の全 op へ
        // その構築エラーをそのまま返す（M8c-3: 一律化、Task 9 と同じ精度）。
        let (_dir, store_path) = make_store();
        let build_err = MatError::store_missing("no KVS materials for native backend");
        let state = NativeState::Unavailable(build_err.clone());
        let health = SubHealth::new(None);

        let err = run_op(&group_on_op(), &state, &store_path, &health, None)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreMissing);
        assert_eq!(err.detail, build_err.detail);

        let read = Op::Read {
            node_id: 1,
            endpoint: 1,
            cluster: "onoff".into(),
            attribute: Some("on-off".into()),
        };
        let err = run_op(&read, &state, &store_path, &health, None)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreMissing);

        // Ping/Shutdown だけは native に触れず常に成功する。
        assert_eq!(
            run_op(&Op::Ping, &state, &store_path, &health, None)
                .await
                .unwrap(),
            json!({ "pong": true })
        );
        assert_eq!(
            run_op(&Op::Shutdown, &state, &store_path, &health, None)
                .await
                .unwrap(),
            json!({ "stopping": true })
        );
    }

    /// 状態変更 op の success が SubHealth に pending を打つ（read は打たない）。
    #[tokio::test]
    async fn run_op_success_marks_pending_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                commissioned_at: "2026-07-21T00:00:00+09:00".into(),
            })
            .unwrap();
        let native =
            crate::native::NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let state = NativeState::Ready(Box::new(native));
        let health = std::sync::Arc::new(SubHealth::new(None));

        // キャッシュが空（購読未確立）なら、成功した off でも pending は打たない
        // — 「値が変わる」ことを証明できないため（spec 2026-07-24）。
        let body = run_op(
            &Op::Off {
                node_id: 5,
                endpoint: 1,
            },
            &state,
            dir.path(),
            &health,
            None,
        )
        .await
        .unwrap();
        assert_eq!(body["status"], "success");
        assert!(health.pending_elapsed(5).is_none());

        // 購読キャッシュが on-off=true を知っている状態で off → 変化するので pending。
        health.observe(crate::subscription::Event {
            timestamp: "2026-07-24T00:00:00+09:00".to_string(),
            node_id: 5,
            endpoint: 1,
            cluster: 0x0006,
            attribute: 0x0000,
            value: json!(true),
            priming: true,
            recovered: false,
        });
        let body = run_op(
            &Op::Off {
                node_id: 5,
                endpoint: 1,
            },
            &state,
            dir.path(),
            &health,
            None,
        )
        .await
        .unwrap();
        assert_eq!(body["status"], "success");
        assert!(health.pending_elapsed(5).is_some());

        // 既に on のノードへ on を撃つ: 値が変わらないので pending は立たない。
        health.clear_pending(5);
        let _ = run_op(
            &Op::On {
                node_id: 5,
                endpoint: 1,
            },
            &state,
            dir.path(),
            &health,
            None,
        )
        .await
        .unwrap();
        assert!(
            health.pending_elapsed(5).is_none(),
            "既に on のノードへの on は no-op — レポートは出ないので pending を打たない"
        );

        // read は状態を変えないので pending を打たない。
        health.clear_pending(5);
        let _ = run_op(
            &Op::Read {
                node_id: 5,
                endpoint: 1,
                cluster: "onoff".into(),
                attribute: Some("on-off".into()),
            },
            &state,
            dir.path(),
            &health,
            None,
        )
        .await
        .unwrap();
        assert!(health.pending_elapsed(5).is_none());
    }
}
