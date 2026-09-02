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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Notify};

use mat_controller::im;
use mat_core::error::{ErrorKind, MatError};
use mat_core::output::now_iso8601;
use mat_core::store::Store;

use crate::native::NativeBackend;
use crate::protocol::{Op, Request};
use crate::subscription::{Event, SubHealth};

/// native backend の構築結果。起動時に一度だけ試み、失敗しても matd 自体は
/// 常駐を続ける（M8c-3: KVS 不在でも起動し、後から `mat fabric init` できる
/// ようにする）。各リクエストはこの結果を参照する — `Unavailable` は保持した
/// 構築エラーをそのまま返す（store_missing/store_parse; mat 直経路の
/// `native_direct::map_engine_build_error` と同じ一律化）。
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

/// 起動時に確定するデーモン基本情報（status op が返す）。
pub struct DaemonInfo {
    pub version: &'static str,
    pub started: std::time::Instant,
    pub iface: String,
    pub fabric_index: u8,
}

/// ソケットを bind し、接続を受け付け続ける。`Ctrl-C` で抜ける。
pub async fn serve(
    socket_path: &Path,
    store_path: PathBuf,
    native: Arc<NativeState>,
    events: broadcast::Sender<Event>,
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
    events: broadcast::Sender<Event>,
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
            } = &req.op
            {
                let filter = match ListenFilter::from_op(node_id, endpoint, cluster, attribute) {
                    Ok(f) => f,
                    Err(e) => {
                        // listen 経路は attach/detach/lag を記録しているので、
                        // 受け付けられなかった場合も残す（この op は dispatch に
                        // 到達しないため op ログには出ない）。
                        tracing::info!(kind = ?e.kind, detail = %e.detail, "listen client rejected");
                        let mut buf = serde_json::to_vec(&error_response(req.id, &e))
                            .unwrap_or_else(|_| b"{}".to_vec());
                        buf.push(b'\n');
                        write_half.write_all(&buf).await?;
                        write_half.flush().await?;
                        return Ok(());
                    }
                };
                // ack より先に subscribe（ack 直後のイベントを取りこぼさない）。
                let rx = events.subscribe();
                let mut ack = json!({ "timestamp": now_iso8601(), "listening": true });
                if let (Value::Object(map), Some(id)) = (&mut ack, req.id) {
                    map.insert("id".into(), id);
                }
                let mut buf = serde_json::to_vec(&ack).unwrap_or_else(|_| b"{}".to_vec());
                buf.push(b'\n');
                write_half.write_all(&buf).await?;
                write_half.flush().await?;
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
        let mut buf = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
        buf.push(b'\n');
        write_half.write_all(&buf).await?;
        // 応答をワイヤに出し切ってから停止を発火する（クライアントが確実に受け取る）。
        write_half.flush().await?;
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

/// listen ストリーム: フィルタ一致イベントを NDJSON で流し続ける。lag した
/// listener は黙って欠落させず、エラー行を送って切断する（spec ②）。
/// クライアント切断（EOF）でも抜ける。
async fn stream_events(
    mut rx: broadcast::Receiver<Event>,
    filter: ListenFilter,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
) -> std::io::Result<()> {
    // 配信件数。切断時に「そもそも 1 件も流れていない」のか
    // 「流れていたのにクライアントが消えた」のかを区別するため。
    let mut delivered: u64 = 0;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if !filter.matches(&ev) {
                        continue;
                    }
                    let mut buf = serde_json::to_vec(&ev.to_json())
                        .unwrap_or_else(|_| b"{}".to_vec());
                    buf.push(b'\n');
                    write_half.write_all(&buf).await?;
                    write_half.flush().await?;
                    delivered += 1;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        skipped = n,
                        delivered,
                        node_id = filter.node_id,
                        "listen client lagged; disconnecting"
                    );
                    let body = json!({
                        "error": { "kind": "other", "detail": "event stream lagged" },
                        "timestamp": now_iso8601(),
                    });
                    let mut buf = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
                    buf.push(b'\n');
                    write_half.write_all(&buf).await?;
                    write_half.flush().await?;
                    return Ok(());
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!(
                        delivered,
                        node_id = filter.node_id,
                        reason = "channel_closed",
                        "listen client detached"
                    );
                    return Ok(());
                }
            },
            line = lines.next_line() => {
                // クライアント切断（None/Err）でストリーム終了。listen 中の追加
                // リクエスト行は無視する（この op は接続占有の例外）。
                match line {
                    Ok(Some(_)) => continue,
                    _ => {
                        tracing::info!(
                            delivered,
                            node_id = filter.node_id,
                            reason = "client_disconnected",
                            "listen client detached"
                        );
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// listen のイベントフィルタ。リクエストの cluster/attribute 名はここで数値へ
/// 解決して照合する（イベント側は数値を持つ）。属性名は cluster 無しでは解決
/// できない（数値なら可）。
#[derive(Debug)]
pub(crate) struct ListenFilter {
    node_id: Option<u64>,
    endpoint: Option<u16>,
    cluster: Option<u32>,
    attribute: Option<u32>,
}

impl ListenFilter {
    pub(crate) fn from_op(
        node_id: &Option<u64>,
        endpoint: &Option<u16>,
        cluster: &Option<String>,
        attribute: &Option<String>,
    ) -> Result<Self, MatError> {
        let cluster_id = match cluster {
            None => None,
            Some(c) => Some(mat_core::ids::resolve_cluster(c).ok_or_else(|| {
                MatError::parse_error(format!(
                    "unknown cluster name {c:?}; numeric IDs are accepted"
                ))
            })?),
        };
        let attribute_id =
            match attribute {
                None => None,
                Some(a) => match cluster_id {
                    Some(cid) => Some(
                        mat_core::ids::resolve_attribute(cid, a)
                            .ok_or_else(|| {
                                MatError::parse_error(format!(
                                    "unknown attribute name {a:?}; numeric IDs are accepted"
                                ))
                            })?
                            .id,
                    ),
                    None => match mat_core::ids::parse_num(a) {
                        Some(n) => Some(
                            u32::try_from(n)
                                .map_err(|_| MatError::parse_error("attribute id out of range"))?,
                        ),
                        None => return Err(MatError::parse_error(
                            "attribute name filter requires a cluster filter (or use a numeric id)",
                        )),
                    },
                },
            };
        Ok(Self {
            node_id: *node_id,
            endpoint: *endpoint,
            cluster: cluster_id,
            attribute: attribute_id,
        })
    }

    pub(crate) fn matches(&self, ev: &Event) -> bool {
        self.node_id.is_none_or(|n| n == ev.node_id)
            && self.endpoint.is_none_or(|e| e == ev.endpoint)
            && self.cluster.is_none_or(|c| c == ev.cluster)
            && self.attribute.is_none_or(|a| a == ev.attribute)
    }
}

/// 成功 op のうち、この時間以上かかったものは info で残す（劣化の前兆）。
/// warm セッションの実測は 71-149ms なので、300ms を超えた成功はもう
/// 「普段と違う」= 弱リンク化 / メッシュ劣化の兆候である。
const SLOW_OP_MS: u64 = 300;

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

/// op ログの level 方針。ここに集約してテストで釘を打ち、tracing のマクロ
/// 呼び出し側は薄く保つ。
#[derive(Debug, PartialEq, Eq)]
enum OpLogClass {
    /// 経路そのものの問題（warn）。`journalctl -p warning` で劣化だけ抽出できる。
    Failed,
    /// 要求側・意味の問題（info）。warn を汚さない。
    Rejected,
    /// 成功だが遅い（info）。
    Slow,
    /// 通常の成功（debug）。既定 level（info）では出ない。
    Ok,
}

/// `ErrorKind` を網羅 match する — 将来 variant が増えたら level の判断を
/// コンパイラが強制する。
fn classify_op_log(result: &Result<Value, MatError>, elapsed_ms: u64) -> OpLogClass {
    match result {
        Ok(_) if elapsed_ms >= SLOW_OP_MS => OpLogClass::Slow,
        Ok(_) => OpLogClass::Ok,
        Err(e) => match e.kind {
            ErrorKind::Timeout
            | ErrorKind::Unreachable
            | ErrorKind::SessionFailed
            | ErrorKind::Other
            // commission / child 系は matd 経路では発生しないが、網羅 match の
            // ために分類は決めておく（発生したら経路の問題として扱う）。
            | ErrorKind::CommissionFailed
            | ErrorKind::MatdUnavailable
            | ErrorKind::ChildNotFound
            | ErrorKind::ChildFailed => OpLogClass::Failed,
            ErrorKind::StoreMissing
            | ErrorKind::StoreParse
            | ErrorKind::NodeNotCommissioned
            | ErrorKind::DeviceRejected
            | ErrorKind::ParseError => OpLogClass::Rejected,
        },
    }
}

/// op 1 件を 1 行で記録する。
///
/// `Option` のフィールドはそのまま渡す — tracing は `None` のフィールドを
/// 省略するので `node_id=Some(42)` にはならず、`grep node_id=42` が効く。
fn log_op(op: &Op, result: &Result<Value, MatError>, elapsed_ms: u64) {
    let op_name = op.name();
    let node_id = op.node_id();
    let group_id = op.group_id();
    let endpoint = op.endpoint();
    let path = op.log_path();
    let path = path.as_deref();
    match result {
        Err(e) => match classify_op_log(result, elapsed_ms) {
            OpLogClass::Rejected => tracing::info!(
                op = op_name, node_id, group_id, endpoint, path, elapsed_ms,
                kind = ?e.kind, detail = %e.detail, "matd op rejected"
            ),
            _ => tracing::warn!(
                op = op_name, node_id, group_id, endpoint, path, elapsed_ms,
                kind = ?e.kind, detail = %e.detail, "matd op failed"
            ),
        },
        Ok(_) => match classify_op_log(result, elapsed_ms) {
            OpLogClass::Slow => tracing::info!(
                op = op_name,
                node_id,
                group_id,
                endpoint,
                path,
                elapsed_ms,
                "matd op slow"
            ),
            _ => tracing::debug!(
                op = op_name,
                node_id,
                group_id,
                endpoint,
                path,
                elapsed_ms,
                "matd op ok"
            ),
        },
    }
}

/// 1 リクエスト行を処理して応答 JSON を組み立てる。戻り値の bool は shutdown 要求か。
async fn dispatch(
    line: &str,
    native: &NativeState,
    store_path: &Path,
    health: &SubHealth,
    daemon: &DaemonInfo,
    events: &broadcast::Sender<Event>,
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

/// wire `Op` → 解決済み op。名前解決・引数符号化の規則は `mat_native::op` の
/// コンストラクタ（mat 直経路と同一）。Ping / Shutdown / Listen / Status /
/// NodeTouched は `run_op` 冒頭 / `dispatch` / `handle_conn` が先取りするため
/// ここへは来ない（不変条件が破れても panic せず typed error）。
#[derive(Debug)]
pub(crate) enum MatdOp {
    Node(mat_native::op::NodeOp),
    Group(mat_native::op::GroupOp),
    Provision(mat_native::op::ProvisionParams),
    Bump,
}

pub(crate) fn to_device_op(op: &Op) -> Result<MatdOp, MatError> {
    use mat_native::op::{GroupOp, GroupOpKind, NodeOp, NodeOpKind, ProvisionParams};
    let node = |node_id: u64, kind: NodeOpKind| MatdOp::Node(NodeOp { node_id, kind });
    Ok(match op {
        Op::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => node(*node_id, NodeOpKind::read(*endpoint, cluster, attribute)?),
        Op::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => node(
            *node_id,
            NodeOpKind::write(*endpoint, cluster, attribute, value)?,
        ),
        Op::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            args,
        } => node(
            *node_id,
            NodeOpKind::invoke(*endpoint, cluster, command, args)?,
        ),
        Op::On { node_id, endpoint } => node(
            *node_id,
            NodeOpKind::On {
                endpoint: *endpoint,
            },
        ),
        Op::Off { node_id, endpoint } => node(
            *node_id,
            NodeOpKind::Off {
                endpoint: *endpoint,
            },
        ),
        // 換算済み値が wire で届く（protocol.rs の約束）— struct リテラルで組む。
        Op::ColorTemp {
            node_id,
            endpoint,
            mireds,
            kelvin,
            transition,
        } => node(
            *node_id,
            NodeOpKind::ColorTemp {
                endpoint: *endpoint,
                kelvin: *kelvin,
                mireds: *mireds,
                transition: *transition,
            },
        ),
        Op::Level {
            node_id,
            endpoint,
            level,
            percent,
            transition,
        } => node(
            *node_id,
            NodeOpKind::Level {
                endpoint: *endpoint,
                percent: *percent,
                level: *level,
                transition: *transition,
            },
        ),
        Op::Color {
            node_id,
            endpoint,
            hue_raw,
            saturation_raw,
            hue,
            saturation,
            name,
            rgb,
            transition,
        } => node(
            *node_id,
            NodeOpKind::Color {
                endpoint: *endpoint,
                color: mat_core::color::ResolvedColor {
                    hue_raw: *hue_raw,
                    sat_raw: *saturation_raw,
                    hue: *hue,
                    sat: *saturation,
                    name: name.clone(),
                    rgb: rgb.clone(),
                },
                transition: *transition,
            },
        ),
        Op::Describe { node_id } => node(*node_id, NodeOpKind::Describe),
        Op::GroupProvision {
            group_id,
            node_ids,
            keyset_id,
            name,
            endpoint,
            epoch_key,
            rebind,
        } => MatdOp::Provision(ProvisionParams {
            group_id: *group_id,
            node_ids: node_ids.clone(),
            keyset_id: *keyset_id,
            name: name.clone(),
            endpoint: *endpoint,
            epoch_key: epoch_key.clone(),
            rebind: *rebind,
        }),
        Op::GroupInvoke {
            group_id,
            cluster,
            command,
            args,
            endpoint,
        } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::invoke(cluster, command, args)?,
        }),
        Op::GroupColorTemp {
            group_id,
            mireds,
            kelvin,
            transition,
            endpoint,
        } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::ColorTemp {
                kelvin: *kelvin,
                mireds: *mireds,
                transition: *transition,
            },
        }),
        Op::GroupLevel {
            group_id,
            level,
            percent,
            transition,
            endpoint,
        } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::Level {
                percent: *percent,
                level: *level,
                transition: *transition,
            },
        }),
        Op::GroupColor {
            group_id,
            hue_raw,
            saturation_raw,
            hue,
            saturation,
            name,
            rgb,
            transition,
            endpoint,
        } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::Color {
                color: mat_core::color::ResolvedColor {
                    hue_raw: *hue_raw,
                    sat_raw: *saturation_raw,
                    hue: *hue,
                    sat: *saturation,
                    name: name.clone(),
                    rgb: rgb.clone(),
                },
                transition: *transition,
            },
        }),
        Op::GroupBump => MatdOp::Bump,
        Op::Listen { .. } | Op::Ping | Op::Status | Op::Shutdown | Op::NodeTouched { .. } => {
            return Err(MatError::parse_error(
                "internal: non-device op reached to_device_op (dispatch invariant violated)",
            ))
        }
    })
}

/// 状態変更 op → (node_id, 変化が現れる cluster)。op 相関の born-dead 検知
/// （`SubHealth::note_op`）の根拠。
///
/// **「op が成功した」は「レポートが出るはず」を含意しない**: すでに目標状態に
/// あるデバイスへの On/Off/Level は data model が変化せず、Matter 仕様上
/// 購読レポートは出ない（レポートは属性変化時のみ）。よって購読キャッシュの
/// 現在値と目標値が**不一致の時だけ**期待を返す（spec 2026-07-24）。
/// キャッシュ欠落（matd 起動直後・購読未確立）は「証明できない」ので None。
/// Color / ColorTemp / Write / Invoke は変化を証明できないため対象外
/// （受け皿は無音 deadline）。Read / Describe / Group 系も元から None。
fn op_report_expectation(
    op: &Op,
    cached_on_off: Option<&Value>,
    cached_level: Option<&Value>,
) -> Option<(u64, u32)> {
    match op {
        // 現在 off の時だけ on は変化を生む。
        Op::On { node_id, .. } => {
            (!cached_on_off?.as_bool()?).then_some((*node_id, im::CLUSTER_ON_OFF))
        }
        // 現在 on の時だけ off は変化を生む。
        Op::Off { node_id, .. } => cached_on_off?
            .as_bool()?
            .then_some((*node_id, im::CLUSTER_ON_OFF)),
        // level は mat 側で換算済みの raw 0–254 が届く（protocol.rs の約束）。
        // MoveToLevel は OptionsMask/OptionsOverride = 0 で送られる
        // （encode_move_to_level_fields）ため、ExecuteIfOff 規則により OnOff=false
        // のデバイスでは実行されずレポートも出ない。確実に消灯中と分かる時
        // （cached_on_off = Some(false)）だけ打たない — 不明（None）を消灯と
        // 決めつけず、その場合は従来通り level の比較へ進む。
        Op::Level { node_id, level, .. } => {
            if cached_on_off.and_then(Value::as_bool) == Some(false) {
                return None;
            }
            (cached_level?.as_u64()? != u64::from(*level))
                .then_some((*node_id, im::CLUSTER_LEVEL_CONTROL))
        }
        _ => None,
    }
}

/// 期待判定に使うキャッシュの参照先 (node_id, endpoint)。On/Off/Level のみ。
///
/// 網羅 match（`_ => None` を使わない）: `Op` に新しい状態変更 op が増えたとき、
/// ここを更新し忘れるとコンパイルエラーで気付ける（`op_report_expectation` 側
/// だけ更新して静かに no-op 化するのを防ぐ — `Op::node_id()` と同じ書き方）。
fn op_state_target(op: &Op) -> Option<(u64, u16)> {
    match op {
        Op::On { node_id, endpoint } | Op::Off { node_id, endpoint } => Some((*node_id, *endpoint)),
        Op::Level {
            node_id, endpoint, ..
        } => Some((*node_id, *endpoint)),
        Op::Read { .. }
        | Op::Write { .. }
        | Op::Invoke { .. }
        | Op::ColorTemp { .. }
        | Op::Color { .. }
        | Op::Describe { .. }
        | Op::GroupProvision { .. }
        | Op::GroupInvoke { .. }
        | Op::GroupColorTemp { .. }
        | Op::GroupLevel { .. }
        | Op::GroupColor { .. }
        | Op::GroupBump
        | Op::Listen { .. }
        | Op::Ping
        | Op::Status
        | Op::Shutdown
        | Op::NodeTouched { .. } => None,
    }
}

/// 成功した op に対し、レポート期待（pending）を打つべきなら打つ。
/// 購読の最終既知値を根拠にするので、no-op（すでに目標状態）では打たない。
pub(crate) fn note_op_expectation(op: &Op, health: &SubHealth) {
    let Some((node_id, endpoint)) = op_state_target(op) else {
        return;
    };
    let on_off = health.cached_value(node_id, endpoint, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF);
    let level = health.cached_value(
        node_id,
        endpoint,
        im::CLUSTER_LEVEL_CONTROL,
        im::ATTR_CURRENT_LEVEL,
    );
    if let Some((node_id, cluster)) = op_report_expectation(op, on_off.as_ref(), level.as_ref()) {
        health.note_op(node_id, cluster);
    }
}

/// store を開いて node_id が commission 済みか確認する（常駐中の台帳更新を拾うよう
/// 毎回開き直す）。
fn require_node(store_path: &Path, node_id: u64) -> Result<(), MatError> {
    Store::open(store_path)?.require_node(node_id)?;
    Ok(())
}

/// `status` op の応答ボディ（timestamp / id は dispatch が付ける）。
fn status_body(
    native: &NativeState,
    store_path: &Path,
    daemon: &DaemonInfo,
    health: &SubHealth,
    events: &broadcast::Sender<Event>,
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
        "listen_clients": events.receiver_count(),
        "nodes": health.status_nodes(),
    })
}

/// エラー応答 `{"error":{"kind","detail"}, "id"?, "timestamp"}`。
fn error_response(id: Option<Value>, e: &MatError) -> Value {
    let mut body = e.to_json();
    if let Value::Object(map) = &mut body {
        map.insert("timestamp".into(), json!(now_iso8601()));
        if let Some(id) = id {
            map.insert("id".into(), id);
        }
    }
    body
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
        let daemon = DaemonInfo {
            version: "test",
            started: std::time::Instant::now(),
            iface: "lo".into(),
            fabric_index: 2,
        };
        let (events, rx) = tokio::sync::broadcast::channel::<crate::subscription::Event>(8);
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

    #[test]
    fn listen_filter_matches_by_resolved_ids() {
        use crate::subscription::Event;
        let ev = Event {
            timestamp: "2026-07-20T00:00:00+09:00".to_string(),
            node_id: 21,
            endpoint: 1,
            cluster: 0x0406,
            attribute: 0x0000,
            value: serde_json::json!(1),
            priming: false,
            recovered: false,
        };
        let f = ListenFilter::from_op(
            &Some(21),
            &Some(1),
            &Some("occupancysensing".into()),
            &Some("occupancy".into()),
        )
        .unwrap();
        assert!(f.matches(&ev));
        // node 不一致
        let f = ListenFilter::from_op(&Some(22), &None, &None, &None).unwrap();
        assert!(!f.matches(&ev));
        // 全省略 = 全イベント
        let f = ListenFilter::from_op(&None, &None, &None, &None).unwrap();
        assert!(f.matches(&ev));
        // 数値 cluster/attribute も可
        let f =
            ListenFilter::from_op(&None, &None, &Some("0x0406".into()), &Some("0".into())).unwrap();
        assert!(f.matches(&ev));
        // 未知 cluster 名は parse_error
        let err = ListenFilter::from_op(&None, &None, &Some("nosuch".into()), &None).unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
        // 属性名フィルタは cluster 無しでは解決できない（数値なら可）
        let err =
            ListenFilter::from_op(&None, &None, &None, &Some("occupancy".into())).unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
        let f = ListenFilter::from_op(&None, &None, &None, &Some("0".into())).unwrap();
        assert!(f.matches(&ev));
    }

    use mat_native::op::{GroupOpKind, NodeOpKind};

    #[test]
    fn to_device_op_maps_node_ops_with_resolved_ids() {
        let m = to_device_op(&Op::On {
            node_id: 1,
            endpoint: 1,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Node(ref n) if n.node_id == 1 && n.kind == NodeOpKind::On { endpoint: 1 })
        );
        let m = to_device_op(&Op::ColorTemp {
            node_id: 1,
            endpoint: 1,
            mireds: 370,
            kelvin: 2700,
            transition: 0,
        })
        .unwrap();
        assert!(matches!(
            m,
            MatdOp::Node(ref n) if n.kind == NodeOpKind::ColorTemp { endpoint: 1, kelvin: 2700, mireds: 370, transition: 0 }
        ));
        let m = to_device_op(&Op::Level {
            node_id: 1,
            endpoint: 1,
            level: 127,
            percent: 50,
            transition: 0,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Node(ref n) if n.kind == NodeOpKind::Level { endpoint: 1, percent: 50, level: 127, transition: 0 })
        );
        let m = to_device_op(&Op::Color {
            node_id: 1,
            endpoint: 1,
            hue_raw: 0,
            saturation_raw: 254,
            hue: 0,
            saturation: 100,
            name: Some("red".into()),
            rgb: Some("#ff0000".into()),
            transition: 0,
        })
        .unwrap();
        match m {
            MatdOp::Node(n) => match n.kind {
                NodeOpKind::Color { color, .. } => {
                    assert_eq!(
                        (color.hue_raw, color.sat_raw, color.hue, color.sat),
                        (0, 254, 0, 100)
                    );
                    assert_eq!(color.name.as_deref(), Some("red"));
                    assert_eq!(color.rgb.as_deref(), Some("#ff0000"));
                }
                other => panic!("expected Color, got {other:?}"),
            },
            other => panic!("expected Node, got {other:?}"),
        }
        let m = to_device_op(&Op::Read {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "current-level".into(),
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Node(ref n) if matches!(n.kind, NodeOpKind::Read { cluster: 0x0008, attribute: 0, .. }))
        );
        let m = to_device_op(&Op::Write {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "on-level".into(),
            value: "128".into(),
        })
        .unwrap();
        assert!(matches!(m, MatdOp::Node(ref n) if matches!(n.kind, NodeOpKind::Write { .. })));
        let m = to_device_op(&Op::Invoke {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            command: "move-to-level".into(),
            args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Node(ref n) if matches!(n.kind, NodeOpKind::Invoke { fields_tlv: Some(_), .. }))
        );
        assert!(
            matches!(to_device_op(&Op::Describe { node_id: 5 }).unwrap(), MatdOp::Node(ref n) if n.kind == NodeOpKind::Describe)
        );
    }

    #[test]
    fn to_device_op_rejects_unresolved_names_and_unencodable_values() {
        // 未知名 → unresolved_op（parse_error、数値 ID 案内付き）。
        let err = to_device_op(&Op::Read {
            node_id: 1,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            attribute: "x".into(),
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(
            err.detail.contains("numeric IDs are accepted"),
            "{}",
            err.detail
        );
        let err = to_device_op(&Op::Invoke {
            node_id: 1,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            command: "x".into(),
            args: vec![],
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        // 名前は解決できるが list 型 → parse_error（classify の msg）。
        let err = to_device_op(&Op::Write {
            node_id: 1,
            endpoint: 1,
            cluster: "accesscontrol".into(),
            attribute: "acl".into(),
            value: "[]".into(),
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.contains("list"), "{}", err.detail);
    }

    #[test]
    fn to_device_op_maps_group_ops_and_shortcuts() {
        let m = to_device_op(&group_on_op()).unwrap();
        match m {
            MatdOp::Group(g) => {
                assert_eq!((g.group_id, g.endpoint), (10, 1));
                assert_eq!(g.kind.wire(), (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None));
            }
            other => panic!("expected Group, got {other:?}"),
        }
        // 引数過多（onoff on は 0 引数）は即 parse_error。
        let err = to_device_op(&Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec!["1".into()],
            endpoint: 1,
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        // 未知コマンド名は unresolved_op。
        let err = to_device_op(&Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "foo".into(),
            args: vec![],
            endpoint: 1,
        })
        .unwrap_err();
        assert!(
            err.detail.contains("numeric IDs are accepted"),
            "{}",
            err.detail
        );

        let m = to_device_op(&Op::GroupColorTemp {
            group_id: 10,
            mireds: 370,
            kelvin: 2702,
            transition: 0,
            endpoint: 1,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Group(ref g) if g.kind == GroupOpKind::ColorTemp { kelvin: 2702, mireds: 370, transition: 0 })
        );
        let m = to_device_op(&Op::GroupLevel {
            group_id: 10,
            level: 254,
            percent: 100,
            transition: 0,
            endpoint: 1,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Group(ref g) if g.kind == GroupOpKind::Level { percent: 100, level: 254, transition: 0 })
        );
        let m = to_device_op(&Op::GroupColor {
            group_id: 10,
            hue_raw: 180,
            saturation_raw: 200,
            hue: 254,
            saturation: 78,
            name: None,
            rgb: None,
            transition: 0,
            endpoint: 1,
        })
        .unwrap();
        assert!(matches!(m, MatdOp::Group(ref g) if matches!(g.kind, GroupOpKind::Color { .. })));
        assert!(matches!(
            to_device_op(&Op::GroupBump).unwrap(),
            MatdOp::Bump
        ));
        let m = to_device_op(&Op::GroupProvision {
            group_id: 7,
            node_ids: vec![1, 2],
            keyset_id: 42,
            name: "grp7".into(),
            endpoint: 1,
            epoch_key: None,
            rebind: true,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Provision(ref p) if p.group_id == 7 && p.node_ids == vec![1, 2] && p.rebind)
        );
    }

    /// dispatch 不変条件が破れても panic しない（v1 Task6 規律）。
    #[test]
    fn to_device_op_rejects_non_device_ops_without_panic() {
        for op in [
            Op::Ping,
            Op::Status,
            Op::Shutdown,
            Op::NodeTouched { node_id: 1 },
            Op::Listen {
                node_id: None,
                endpoint: None,
                cluster: None,
                attribute: None,
            },
        ] {
            let err = to_device_op(&op).unwrap_err();
            assert_eq!(err.kind, ErrorKind::ParseError);
            assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);
        }
    }

    use crate::native::test_support::{write_group_fixture_ini, FakeEstablisher};
    use std::path::PathBuf;

    fn group_on_op() -> Op {
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
            attribute: "current-level".into(),
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
    async fn native_write_rejects_list_type_with_parse_error() {
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let state = NativeState::Ready(Box::new(native));
        let health = SubHealth::new(None);
        let op = Op::Write {
            node_id: 5,
            endpoint: 0,
            cluster: "accesscontrol".into(),
            attribute: "acl".into(),
            value: "[]".into(),
        };
        let err = run_op(&op, &state, store_with_node_5().path(), &health, None)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
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
                crate::native::test_support::FakeConn::scripted()
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

    /// M8c-3: group_provision はコントローラ側 group state・デバイス側ともに
    /// 常に native（group_settings_ctx を注入すれば KVS への実書込みまで検証できる）。
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

    /// group_settings_ctx が未構成（テスト注入時のみ起こり得る）だと internal エラー。
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
            attribute: "on-off".into(),
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
                attribute: "on-off".into(),
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

    /// op → レポート期待の分類（spec 2026-07-24 の表）。
    /// 「op 成功」は「レポートが出る」を含意しない: 目標状態と現在値が一致する
    /// no-op はレポートを生まないので pending を打ってはならない。
    #[test]
    fn op_report_expectation_only_when_value_actually_changes() {
        let on = Op::On {
            node_id: 5,
            endpoint: 1,
        };
        let off = Op::Off {
            node_id: 5,
            endpoint: 1,
        };
        let level = Op::Level {
            node_id: 5,
            endpoint: 1,
            level: 128,
            percent: 50,
            transition: 0,
        };
        let t = json!(true);
        let f = json!(false);
        let l128 = json!(128);
        let l200 = json!(200);

        // On: 現在 off → 変化する → pending。
        assert_eq!(
            op_report_expectation(&on, Some(&f), None),
            Some((5, im::CLUSTER_ON_OFF))
        );
        // On: 既に on → no-op → 打たない。
        assert_eq!(op_report_expectation(&on, Some(&t), None), None);
        // Off: 現在 on → 変化する → pending。
        assert_eq!(
            op_report_expectation(&off, Some(&t), None),
            Some((5, im::CLUSTER_ON_OFF))
        );
        // Off: 既に off → no-op → 打たない（casa 人感ルールの誤キルの正体）。
        assert_eq!(op_report_expectation(&off, Some(&f), None), None);
        // Level: 現在値と異なる → pending / 同値 → 打たない。
        assert_eq!(
            op_report_expectation(&level, None, Some(&l200)),
            Some((5, im::CLUSTER_LEVEL_CONTROL))
        );
        assert_eq!(op_report_expectation(&level, None, Some(&l128)), None);

        // Level while off: MoveToLevel は Options=0 なので OnOff=false のデバイス
        // では実行されずレポートも出ない → 確実に消灯中なら値差分があっても打たない。
        assert_eq!(op_report_expectation(&level, Some(&f), Some(&l200)), None);
        // Level while on: 点灯中は通常通り値差分で判定する。
        assert_eq!(
            op_report_expectation(&level, Some(&t), Some(&l200)),
            Some((5, im::CLUSTER_LEVEL_CONTROL))
        );
        // Level, on-off キャッシュ欠落: 「不明」を「消灯」と決めつけず従来通り
        // level の比較へ進む（挙動不変の確認。上の `None, Some(&l200)` ケースと同じ）。

        // キャッシュ欠落: 証明できないので打たない（matd 起動直後・購読未確立）。
        assert_eq!(op_report_expectation(&on, None, None), None);
        assert_eq!(op_report_expectation(&off, None, None), None);
        assert_eq!(op_report_expectation(&level, None, None), None);
        // 型が想定外（level が null 等）でも打たない。
        assert_eq!(
            op_report_expectation(&level, None, Some(&json!(null))),
            None
        );

        // Color / ColorTemp / Write / Invoke は pending 対象から降格
        // （状態変化を証明できない。受け皿は無音 deadline）。
        let color_temp = Op::ColorTemp {
            node_id: 5,
            endpoint: 1,
            mireds: 370,
            kelvin: 2700,
            transition: 0,
        };
        assert_eq!(
            op_report_expectation(&color_temp, Some(&t), Some(&l128)),
            None
        );
        let invoke = Op::Invoke {
            node_id: 5,
            endpoint: 1,
            cluster: "onoff".into(),
            command: "toggle".into(),
            args: vec![],
        };
        assert_eq!(op_report_expectation(&invoke, Some(&t), Some(&l128)), None);
        let write = Op::Write {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "on-level".into(),
            value: "128".into(),
        };
        assert_eq!(op_report_expectation(&write, Some(&t), Some(&l128)), None);
        // Read は元から対象外。
        let read = Op::Read {
            node_id: 5,
            endpoint: 1,
            cluster: "onoff".into(),
            attribute: "on-off".into(),
        };
        assert_eq!(op_report_expectation(&read, Some(&f), None), None);
    }

    fn err(kind: ErrorKind) -> Result<Value, MatError> {
        Err(MatError::new(kind, "test"))
    }

    #[test]
    fn path_failures_are_warn_worthy() {
        for kind in [
            ErrorKind::Timeout,
            ErrorKind::Unreachable,
            ErrorKind::SessionFailed,
            ErrorKind::Other,
            ErrorKind::CommissionFailed,
            ErrorKind::MatdUnavailable,
            ErrorKind::ChildNotFound,
            ErrorKind::ChildFailed,
        ] {
            assert_eq!(
                classify_op_log(&err(kind), 10),
                OpLogClass::Failed,
                "kind: {kind:?}"
            );
        }
    }

    #[test]
    fn request_side_errors_do_not_pollute_warn() {
        for kind in [
            ErrorKind::StoreMissing,
            ErrorKind::StoreParse,
            ErrorKind::NodeNotCommissioned,
            ErrorKind::DeviceRejected,
            ErrorKind::ParseError,
        ] {
            assert_eq!(
                classify_op_log(&err(kind), 10),
                OpLogClass::Rejected,
                "kind: {kind:?}"
            );
        }
    }

    #[test]
    fn slow_threshold_is_inclusive_at_300ms() {
        let ok = Ok(json!({"value": true}));
        assert_eq!(classify_op_log(&ok, 0), OpLogClass::Ok);
        assert_eq!(classify_op_log(&ok, 299), OpLogClass::Ok);
        assert_eq!(classify_op_log(&ok, 300), OpLogClass::Slow);
        assert_eq!(classify_op_log(&ok, 8134), OpLogClass::Slow);
    }

    #[test]
    fn elapsed_time_does_not_change_error_classification() {
        // 失敗は所要時間に関わらず失敗として分類される（速い失敗を
        // 「速いから ok」に見せない）。
        assert_eq!(
            classify_op_log(&err(ErrorKind::Timeout), 0),
            OpLogClass::Failed
        );
        assert_eq!(
            classify_op_log(&err(ErrorKind::NodeNotCommissioned), 9999),
            OpLogClass::Rejected
        );
    }

    /// `log_op` の出力を直接検証するための writer。`Arc<Mutex<Vec<u8>>>` を
    /// 包むだけの薄いラッパで、新規依存は増やさない
    /// （`tracing-subscriber` は matd の通常依存なので lib のテストからも使える）。
    #[derive(Clone)]
    struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// `log_op` を自前の subscriber の下で 1 回呼び、書き出された生テキストを
    /// 返す。ANSI の有無はここでは検証しない — 検証するのは自前で組み立てた
    /// この subscriber の出力であって、matd 本体が `main.rs` で設定する
    /// `with_ansi(false)` そのものではない。
    fn capture_log_op(op: &Op, result: &Result<Value, MatError>, elapsed_ms: u64) -> String {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = CapturingWriter(buf.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_op(op, result, elapsed_ms);
        });
        let captured = buf.lock().unwrap().clone();
        String::from_utf8(captured).unwrap()
    }

    /// node_id=6（実在ノードを書かない規律 — README のプレースホルダと同じ番号）
    /// の read op。
    fn read_op() -> Op {
        serde_json::from_str::<Request>(
            r#"{"op":"read","node_id":6,"endpoint":1,"cluster":"onoff","attribute":"on-off"}"#,
        )
        .unwrap()
        .op
    }

    #[test]
    fn log_op_omits_absent_fields_entirely() {
        // Op::Ping は node_id / endpoint / group_id / path を持たない op。
        // `node_id` が `?op.node_id()` のような Debug 整形に書き換えられて
        // いたら "Some(" やフィールド名自体が出てしまう — grep node_id=42 を
        // 壊す退行をここで釘で留める。
        let out = capture_log_op(&Op::Ping, &Ok(json!({"value": true})), 0);
        assert!(!out.contains("Some("), "output: {out}");
        assert!(!out.contains("node_id"), "output: {out}");
        assert!(!out.contains("path"), "output: {out}");
    }

    #[test]
    fn log_op_emits_grep_friendly_fields_on_ok() {
        let out = capture_log_op(&read_op(), &Ok(json!({"value": true})), 0);
        // 引用符も Some() も付かない生の "node_id=6" — これが
        // `grep node_id=42` の効く形。
        assert!(out.contains("node_id=6"), "output: {out}");
        assert!(out.contains(r#"path="onoff/on-off""#), "output: {out}");
        assert!(out.contains("matd op ok"), "output: {out}");
    }

    #[test]
    fn log_op_level_and_message_follow_classification() {
        // class（Slow/Ok/Rejected/Failed）→ level/message の対応が
        // 実際に効いていることを確かめる（classify_op_log 単体のテストは
        // 既にあるが、log_op がそれを使っているかは別の話）。
        let slow = capture_log_op(&read_op(), &Ok(json!({"value": true})), 300);
        assert!(slow.contains("matd op slow"), "output: {slow}");

        let failed = capture_log_op(
            &read_op(),
            &Err(MatError::new(ErrorKind::Timeout, "no ack")),
            0,
        );
        assert!(failed.contains("matd op failed"), "output: {failed}");
        assert!(failed.contains("kind=Timeout"), "output: {failed}");
        assert!(failed.contains("detail="), "output: {failed}");
    }
}
