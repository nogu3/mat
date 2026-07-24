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
use mat_core::group::resolve_epoch_key;
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

/// ソケットを bind し、接続を受け付け続ける。`Ctrl-C` で抜ける。
pub async fn serve(
    socket_path: &Path,
    store_path: PathBuf,
    native: Arc<NativeState>,
    events: broadcast::Sender<Event>,
    health: Arc<SubHealth>,
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
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_conn(stream, native, store_path, shutdown, events, health).await
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
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
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
                return stream_events(rx, filter, &mut lines, &mut write_half).await;
            }
        }
        let (response, is_shutdown) = dispatch(&line, &native, &store_path, &health).await;
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

/// listen ストリーム: フィルタ一致イベントを NDJSON で流し続ける。lag した
/// listener は黙って欠落させず、エラー行を送って切断する（spec ②）。
/// クライアント切断（EOF）でも抜ける。
async fn stream_events(
    mut rx: broadcast::Receiver<Event>,
    filter: ListenFilter,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
) -> std::io::Result<()> {
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
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "listen client lagged; disconnecting");
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
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            line = lines.next_line() => {
                // クライアント切断（None/Err）でストリーム終了。listen 中の追加
                // リクエスト行は無視する（この op は接続占有の例外）。
                match line {
                    Ok(Some(_)) => continue,
                    _ => return Ok(()),
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

/// 1 リクエスト行を処理して応答 JSON を組み立てる。戻り値の bool は shutdown 要求か。
async fn dispatch(
    line: &str,
    native: &NativeState,
    store_path: &Path,
    health: &SubHealth,
) -> (Value, bool) {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return (
                error_response(
                    None,
                    &MatError::parse_error(format!("invalid request JSON: {e}")),
                ),
                false,
            )
        }
    };
    let id = req.id.clone();
    let is_shutdown = matches!(req.op, Op::Shutdown);

    let body = match run_op(&req.op, native, store_path, health).await {
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
/// 旧経路の名残）は [`unresolved_op_error`] で即 parse_error にする — フォールバック
/// 先が無いため（数値 ID は resolve 済みなので影響しない）。
async fn run_op(
    op: &Op,
    native: &NativeState,
    store_path: &Path,
    health: &SubHealth,
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
        _ => {}
    }

    let native = match native {
        NativeState::Ready(n) => n,
        NativeState::Unavailable(e) => return Err(e.clone()),
    };

    if is_native_hotpath(op) {
        let result = native_op(op, native, store_path).await;
        if result.is_ok() {
            // 前提: デバイスは invoke 応答を先に、購読 report を後に送る。
            // report が note_op より先に pump へ届く逆順だと pending が残り
            // 健全購読を 1 回余分に再購読するが、それが最悪ケース（イベント
            // 自体は配信済みで priming が状態を再配達する）。
            note_op_expectation(op, health);
        }
        return result;
    }

    if let Some(result) = native_group_params(op) {
        return match result {
            Ok((group_id, cluster, command, fields, sent_body)) => {
                // chip-tool 撤去前と同じ前提チェック（store が開けること）。
                let _store = Store::open(store_path)?;
                match native
                    .group_invoke(group_id, cluster, command, fields)
                    .await?
                {
                    crate::native::GroupOutcome::Sent => Ok(sent_body),
                    crate::native::GroupOutcome::Unavailable(reason) => {
                        Err(group_unavailable_error(&reason))
                    }
                }
            }
            // 名前は解決できたが引数が符号化不能 → 即座に拒否（mat 側
            // classify_strict と同じ規則）。
            Err(e) => Err(e),
        };
    }

    match op {
        Op::GroupProvision { .. } => group_provision(op, native, store_path).await,
        // ここに来るのは Read/Write/Invoke/GroupInvoke で cluster/attribute/command
        // 名が解決できなかった場合のみ（On/Off/Color/ColorTemp/Level/Describe は常に
        // is_native_hotpath、GroupColorTemp/GroupColor/GroupLevel は native_group_params が
        // 常に Some を返すため到達しない）。
        _ => Err(unresolved_op_error()),
    }
}

/// この op を native warm session で処理するか（ホットパス）。それ以外は
/// [`unresolved_op_error`] で拒否する。
///
/// Read/Write/Invoke/Describe の判定は mat-core::ids（`classify_write` /
/// `classify_invoke` / `resolve_cluster` + `resolve_attribute`）に委ねる —
/// mat 直経路の `native_direct::classify_strict` と同じ判定を共有する
/// （M8a Task10）。cluster/attribute/command 名が解決できない場合のみ false、
/// 名前は解決できたが値が符号化不能（list 型等）な場合は true のまま
/// native_op へ進み、そこで即 parse_error を返す（M8c-3: フォールバック先が
/// 無いため拒否する — spec 決定と同じ）。
pub(crate) fn is_native_hotpath(op: &Op) -> bool {
    match op {
        Op::On { .. }
        | Op::Off { .. }
        | Op::Color { .. }
        | Op::ColorTemp { .. }
        | Op::Level { .. }
        | Op::Describe { .. } => true,
        Op::Read {
            cluster, attribute, ..
        } => mat_core::ids::resolve_cluster(cluster)
            .and_then(|cid| mat_core::ids::resolve_attribute(cid, attribute))
            .is_some(),
        Op::Write {
            cluster,
            attribute,
            value,
            ..
        } => !matches!(
            mat_core::ids::classify_write(cluster, attribute, value),
            mat_core::ids::WriteClass::NotNative
        ),
        Op::Invoke {
            cluster,
            command,
            args,
            ..
        } => !matches!(
            mat_core::ids::classify_invoke(cluster, command, args),
            mat_core::ids::InvokeClass::NotNative
        ),
        _ => false,
    }
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
        Op::Level { node_id, level, .. } => (cached_level?.as_u64()? != u64::from(*level))
            .then_some((*node_id, im::CLUSTER_LEVEL_CONTROL)),
        _ => None,
    }
}

/// 期待判定に使うキャッシュの参照先 (node_id, endpoint)。On/Off/Level のみ。
fn op_state_target(op: &Op) -> Option<(u64, u16)> {
    match op {
        Op::On { node_id, endpoint } | Op::Off { node_id, endpoint } => Some((*node_id, *endpoint)),
        Op::Level {
            node_id, endpoint, ..
        } => Some((*node_id, *endpoint)),
        _ => None,
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

/// `native_group_params` の Ok 内訳: (group_id, cluster_id, command_id, fields_tlv,
/// 成功時 sent body)。body は op 変種が確定しているここで組む(旧 `group_sent_body`
/// の `let … else unreachable!` を型で排除)。
type GroupSendParams = (u16, u32, u32, Option<Vec<u8>>, Value);

/// group 送信 op の native 適用判定。`GroupInvoke` の cluster/command/引数は
/// mat-core::ids の `classify_invoke` に通す（onoff 限定を撤廃 — M8a
/// Task10、mat 直経路の `native_direct::classify_strict` の group invoke 腕と
/// 同じ判定）。戻り値:
/// - `None` — 非対象（cluster/command 名が解決できない）→ [`unresolved_op_error`]。
/// - `Some(Ok(params))` — native 送信対象。
/// - `Some(Err(e))` — 名前は解決できたが引数が符号化不能 → 即座にそのエラーを
///   返す（mat 側と同じ拒否規則）。
fn native_group_params(op: &Op) -> Option<Result<GroupSendParams, MatError>> {
    match op {
        Op::GroupInvoke {
            group_id,
            cluster,
            command,
            args,
            endpoint,
            ..
        } => match mat_core::ids::classify_invoke(cluster, command, args) {
            mat_core::ids::InvokeClass::NotNative => None,
            mat_core::ids::InvokeClass::Reject(msg) => Some(Err(MatError::parse_error(msg))),
            mat_core::ids::InvokeClass::Native {
                cluster: cluster_id,
                command: cmd_id,
                fields,
                ..
            } => {
                let fields_tlv = if fields.is_empty() {
                    None
                } else {
                    Some(mat_native::encode_command_fields(&fields))
                };
                let sent_body =
                    mat_core::body::group_invoke_sent(*group_id, cluster, command, *endpoint);
                Some(Ok((*group_id, cluster_id, cmd_id, fields_tlv, sent_body)))
            }
        },
        Op::GroupColorTemp {
            group_id,
            mireds,
            kelvin,
            transition,
            endpoint,
        } => Some(Ok((
            *group_id,
            im::CLUSTER_COLOR_CONTROL,
            im::CMD_MOVE_TO_COLOR_TEMPERATURE,
            Some(im::encode_move_to_color_temperature_fields(
                *mireds,
                *transition,
            )),
            mat_core::body::group_color_temp_sent(
                *group_id,
                *kelvin,
                *mireds,
                *transition,
                *endpoint,
            ),
        ))),
        Op::GroupLevel {
            group_id,
            level,
            percent,
            transition,
            endpoint,
        } => Some(Ok((
            *group_id,
            im::CLUSTER_LEVEL_CONTROL,
            im::CMD_MOVE_TO_LEVEL,
            Some(im::encode_move_to_level_fields(*level, *transition)),
            mat_core::body::group_level_sent(
                *group_id,
                mat_core::body::LevelEcho {
                    percent: *percent,
                    level: *level,
                },
                *transition,
                *endpoint,
            ),
        ))),
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
        } => {
            let color = mat_core::color::ResolvedColor {
                hue_raw: *hue_raw,
                sat_raw: *saturation_raw,
                hue: *hue,
                sat: *saturation,
                name: name.clone(),
                rgb: rgb.clone(),
            };
            Some(Ok((
                *group_id,
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_HUE_AND_SATURATION,
                Some(im::encode_move_to_hue_and_saturation_fields(
                    *hue_raw,
                    *saturation_raw,
                    *transition,
                )),
                mat_core::body::group_color_sent(*group_id, &color, *transition, *endpoint),
            )))
        }
        _ => None,
    }
}

/// native ホットパス op を warm session で実行し、成功 body を組む。
async fn native_op(op: &Op, native: &NativeBackend, store_path: &Path) -> Result<Value, MatError> {
    // commission 済みか毎回 KVS で確認する。
    if let Some(node_id) = op.node_id() {
        require_node(store_path, node_id)?;
    }
    match op {
        Op::On { node_id, endpoint } => {
            native.on(*node_id, *endpoint).await?;
            Ok(mat_core::body::invoke_success(
                *node_id, *endpoint, "onoff", "on",
            ))
        }
        Op::Off { node_id, endpoint } => {
            native.off(*node_id, *endpoint).await?;
            Ok(mat_core::body::invoke_success(
                *node_id, *endpoint, "onoff", "off",
            ))
        }
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
        } => {
            native
                .color(*node_id, *endpoint, *hue_raw, *saturation_raw, *transition)
                .await?;
            let color = mat_core::color::ResolvedColor {
                hue_raw: *hue_raw,
                sat_raw: *saturation_raw,
                hue: *hue,
                sat: *saturation,
                name: name.clone(),
                rgb: rgb.clone(),
            };
            Ok(mat_core::body::color_success(
                *node_id,
                *endpoint,
                &color,
                *transition,
            ))
        }
        Op::ColorTemp {
            node_id,
            endpoint,
            mireds,
            kelvin,
            transition,
        } => {
            native
                .color_temp(*node_id, *endpoint, *mireds, *transition)
                .await?;
            Ok(mat_core::body::color_temp_success(
                *node_id,
                *endpoint,
                *kelvin,
                *mireds,
                *transition,
            ))
        }
        Op::Level {
            node_id,
            endpoint,
            level,
            percent,
            transition,
        } => {
            native
                .level(*node_id, *endpoint, *level, *transition)
                .await?;
            Ok(mat_core::body::level_success(
                *node_id,
                *endpoint,
                mat_core::body::LevelEcho {
                    percent: *percent,
                    level: *level,
                },
                *transition,
            ))
        }
        Op::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => {
            if cluster == "onoff" && attribute == "on-off" {
                let v = native.read_onoff(*node_id, *endpoint).await?;
                Ok(mat_core::body::read_success(
                    *node_id,
                    *endpoint,
                    cluster,
                    attribute,
                    Value::Bool(v),
                ))
            } else {
                // is_native_hotpath が解決済みのはずだが、不変条件が破れても
                // panic せず typed error（v1 品質修正 6 — alias.rs id() と同じ規律）。
                let cluster_id = mat_core::ids::resolve_cluster(cluster).ok_or_else(|| {
                    MatError::parse_error(format!(
                        "internal: unknown cluster name '{cluster}' (is_native_hotpath invariant violated)"
                    ))
                })?;
                let attr =
                    mat_core::ids::resolve_attribute(cluster_id, attribute).ok_or_else(|| {
                        MatError::parse_error(format!(
                            "internal: unknown attribute name '{attribute}' for cluster '{cluster}' (is_native_hotpath invariant violated)"
                        ))
                    })?;
                let v = native
                    .read_json(*node_id, *endpoint, cluster_id, attr.id)
                    .await?;
                Ok(mat_core::body::read_success(
                    *node_id, *endpoint, cluster, attribute, v,
                ))
            }
        }
        Op::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => match mat_core::ids::classify_write(cluster, attribute, value) {
            mat_core::ids::WriteClass::NotNative => Err(MatError::parse_error(
                "internal: NotNative write reached native_op (is_native_hotpath invariant violated)",
            )),
            mat_core::ids::WriteClass::Reject(msg) => Err(MatError::parse_error(msg)),
            mat_core::ids::WriteClass::Native {
                cluster: cluster_id,
                attribute: attr_id,
                value: scalar,
                timed,
            } => {
                native
                    .write_tlv(
                        *node_id,
                        *endpoint,
                        cluster_id,
                        attr_id,
                        mat_native::scalar_to_tlv(&scalar),
                        timed,
                    )
                    .await?;
                Ok(mat_core::body::write_success(
                    *node_id, *endpoint, cluster, attribute, value,
                ))
            }
        },
        Op::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            args,
        } => match mat_core::ids::classify_invoke(cluster, command, args) {
            mat_core::ids::InvokeClass::NotNative => Err(MatError::parse_error(
                "internal: NotNative invoke reached native_op (is_native_hotpath invariant violated)",
            )),
            mat_core::ids::InvokeClass::Reject(msg) => Err(MatError::parse_error(msg)),
            mat_core::ids::InvokeClass::Native {
                cluster: cluster_id,
                command: cmd_id,
                fields,
                timed,
            } => {
                let fields_tlv = if fields.is_empty() {
                    None
                } else {
                    Some(mat_native::encode_command_fields(&fields))
                };
                native
                    .invoke_generic(*node_id, *endpoint, cluster_id, cmd_id, fields_tlv, timed)
                    .await?;
                Ok(mat_core::body::invoke_success(
                    *node_id, *endpoint, cluster, command,
                ))
            }
        },
        Op::Describe { node_id } => {
            let endpoints = native.describe(*node_id).await?;
            Ok(mat_core::body::describe_success(*node_id, &endpoints))
        }
        _ => Err(MatError::parse_error(
            "internal: native_op called with non-hotpath op (dispatch invariant violated)",
        )),
    }
}

/// `group_provision` — group の鍵束・マッピングを各ノードへ焼き、コントローラ側 group
/// state も設定する（`mat group provision` 相当）。最初の失敗で停止する。
///
/// M8c-3: コントローラ側 group state（groupsettings 系）・デバイス側 4 ステップ
/// （KeySetWrite / group-key-map / AddGroup / ACL）ともに常に native
/// （`mat_native::group_settings::write_group_provision` /
/// `mat_native::ops::provision_node`）— chip-tool へのフォールバックは撤去した。
/// `group_settings_ctx()` が `None`（本番 `Engine::build` では常に `Some` —
/// テスト注入時のみ起こり得る）は internal エラーとして拒否する。
async fn group_provision(
    op: &Op,
    native: &NativeBackend,
    store_path: &Path,
) -> Result<Value, MatError> {
    let Op::GroupProvision {
        group_id,
        node_ids,
        keyset_id,
        name,
        endpoint,
        epoch_key,
        rebind,
    } = op
    else {
        return Err(MatError::parse_error(
            "internal: group_provision called with non-GroupProvision op (dispatch invariant violated)",
        ));
    };

    let store = Store::open(store_path)?;
    // 全ノードが commission 済みか先に確認（1つでも未登録なら停止）。
    for &node_id in node_ids {
        store.require_node(node_id)?;
    }

    let epoch_key = resolve_epoch_key(epoch_key.as_deref())?;
    let epoch_key_bytes = mat_native::ops::epoch_key_from_hex(&epoch_key)?;

    // 1) コントローラ側 group state。
    let gs = native
        .group_settings_ctx()
        .ok_or_else(group_ctx_unconfigured_error)?;
    mat_native::group_settings::write_group_provision(
        gs,
        *group_id,
        *keyset_id,
        name,
        &epoch_key_bytes,
        *rebind,
    )?;

    // 2) 各デバイスへ provision（unicast, acknowledged）。
    for &node_id in node_ids {
        let p = mat_native::ops::ProvisionNodeParams {
            group_id: *group_id,
            keyset_id: *keyset_id,
            name: name.clone(),
            endpoint: *endpoint,
            epoch_key: epoch_key_bytes,
        };
        native
            .provision_node(node_id, &p)
            .await
            .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?;
    }

    Ok(mat_core::body::group_provision_success(
        *group_id, *keyset_id, name, *endpoint, node_ids, None,
    ))
}

/// store を開いて node_id が commission 済みか確認する（常駐中の台帳更新を拾うよう
/// 毎回開き直す）。
fn require_node(store_path: &Path, node_id: u64) -> Result<(), MatError> {
    Store::open(store_path)?.require_node(node_id)?;
    Ok(())
}

/// 名前解決できない（未知の cluster/attribute/command 名）op のハードエラー。
/// mat 直経路の `native_direct::unresolved_op_error` と同じ文言 —— M8c-3 で
/// chip-tool 撤去によりフォールバック先が無くなったため、数値 ID 以外は拒否する。
fn unresolved_op_error() -> MatError {
    MatError::parse_error(
        "unknown cluster/attribute/command name (or unsupported non-scalar type); \
         numeric IDs are accepted",
    )
}

/// group 送信不能（未 provision・KVS 不備等）。`mat_native::group::send` からの
/// `Unavailable` 理由をそのまま detail に載せる（`mat group provision` 誘導を
/// 含む）。mat 直経路の `native_direct::group_unavailable_error` と同じ kind。
fn group_unavailable_error(reason: &str) -> MatError {
    MatError::store_parse(format!("native group send unavailable: {reason}"))
}

/// `group_settings_ctx` / group send コンテキスト未構成（本番 `Engine::build` では
/// 常に `Some` なので実質到達しない — テスト注入時のみ）。mat 直経路の
/// `native_direct::group_ctx_unconfigured_error` と同じ。
fn group_ctx_unconfigured_error() -> MatError {
    MatError::new(
        ErrorKind::Other,
        "native group context not configured (internal)",
    )
}

/// エラー応答 `{"error":{"kind","detail"}, "id"?, "timestamp"}`。
fn error_response(id: Option<Value>, e: &MatError) -> Value {
    let mut body = json!({
        "error": { "kind": e.kind, "detail": e.detail },
        "timestamp": now_iso8601(),
    });
    if let (Value::Object(map), Some(id)) = (&mut body, id) {
        map.insert("id".into(), id);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Op;

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

    #[test]
    fn hotpath_routing_selects_native_ops() {
        // native で処理するホットパス（onoff ショートカット群）。
        assert!(is_native_hotpath(&Op::On {
            node_id: 1,
            endpoint: 1
        }));
        assert!(is_native_hotpath(&Op::Off {
            node_id: 1,
            endpoint: 1
        }));
        assert!(is_native_hotpath(&Op::ColorTemp {
            node_id: 1,
            endpoint: 1,
            mireds: 370,
            kelvin: 2700,
            transition: 0
        }));
        assert!(is_native_hotpath(&Op::Color {
            node_id: 1,
            endpoint: 1,
            hue_raw: 0,
            saturation_raw: 254,
            hue: 0,
            saturation: 100,
            name: None,
            rgb: None,
            transition: 0
        }));
        assert!(is_native_hotpath(&Op::Level {
            node_id: 1,
            endpoint: 1,
            level: 127,
            percent: 50,
            transition: 0
        }));
        // onoff on-off の read。
        assert!(is_native_hotpath(&Op::Read {
            node_id: 1,
            endpoint: 1,
            cluster: "onoff".into(),
            attribute: "on-off".into()
        }));
        // 汎用 read/write/invoke/describe も ids で名前解決できれば native
        // （M8a Task10 — mat 直経路の classify_strict と同じ判定を共有）。
        assert!(is_native_hotpath(&Op::Read {
            node_id: 1,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "current-level".into()
        }));
        assert!(is_native_hotpath(&Op::Write {
            node_id: 1,
            endpoint: 1,
            cluster: "onoff".into(),
            attribute: "on-off".into(),
            value: "1".into()
        }));
        assert!(is_native_hotpath(&Op::Invoke {
            node_id: 1,
            endpoint: 1,
            cluster: "identify".into(),
            command: "identify".into(),
            args: vec![]
        }));
        assert!(is_native_hotpath(&Op::Describe { node_id: 1 }));
        // 名前は解決できるが値が符号化不能（list 型）な write も、拒否せず
        // native_op で即 parse_error にするため hotpath=true のまま。
        assert!(is_native_hotpath(&Op::Write {
            node_id: 1,
            endpoint: 1,
            cluster: "accesscontrol".into(),
            attribute: "acl".into(),
            value: "[]".into()
        }));
    }

    #[test]
    fn hotpath_routing_rejects_unresolved_names() {
        // 未知 cluster/attribute 名は native 対象外 → run_op が unresolved_op_error。
        assert!(!is_native_hotpath(&Op::Read {
            node_id: 1,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            attribute: "x".into()
        }));
        assert!(!is_native_hotpath(&Op::Write {
            node_id: 1,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            attribute: "x".into(),
            value: "1".into()
        }));
        assert!(!is_native_hotpath(&Op::Invoke {
            node_id: 1,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            command: "x".into(),
            args: vec![]
        }));
        // GroupInvoke は native_group_params が別途扱う（is_native_hotpath の
        // 対象外 — group 送信は特定ノード宛ではないため）。
        assert!(!is_native_hotpath(&Op::GroupInvoke {
            group_id: 1,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec![],
            endpoint: 1
        }));
        assert!(!is_native_hotpath(&Op::Ping));
    }

    #[test]
    fn native_group_params_maps_onoff_and_shortcuts() {
        let on = Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec![],
            endpoint: 1,
        };
        let (gid, cluster, command, fields, _) = native_group_params(&on).unwrap().unwrap();
        assert_eq!(
            (gid, cluster, command),
            (10, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON)
        );
        assert!(fields.is_none());

        // 引数過多（onoff on は 0 引数）は即 parse_error
        // （M8a Task10: ids ベースの classify_invoke と同じ拒否規則）。
        let with_args = Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec!["1".into()],
            endpoint: 1,
        };
        let err = native_group_params(&with_args).unwrap().unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);

        // onoff 以外の cluster も、名前解決できれば native 対象（onoff 限定は
        // M8a Task10 で撤廃）。
        let other_cluster = Op::GroupInvoke {
            group_id: 10,
            cluster: "levelcontrol".into(),
            command: "move-to-level".into(),
            args: vec![],
            endpoint: 1,
        };
        let (gid, cluster, command, fields, _) =
            native_group_params(&other_cluster).unwrap().unwrap();
        assert_eq!(gid, 10);
        assert_eq!(
            cluster,
            mat_core::ids::resolve_cluster("levelcontrol").unwrap()
        );
        assert_eq!(
            command,
            mat_core::ids::resolve_command(cluster, "move-to-level")
                .unwrap()
                .id
        );
        assert!(fields.is_none()); // 引数なし → fields_tlv は None。

        // 未知コマンド名は非対象（run_op が unresolved_op_error にする）。
        let unknown_command = Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "foo".into(),
            args: vec![],
            endpoint: 1,
        };
        assert!(native_group_params(&unknown_command).is_none());

        let ct = Op::GroupColorTemp {
            group_id: 10,
            mireds: 370,
            kelvin: 2702,
            transition: 0,
            endpoint: 1,
        };
        let (_, cluster, command, fields, _) = native_group_params(&ct).unwrap().unwrap();
        assert_eq!(cluster, im::CLUSTER_COLOR_CONTROL);
        assert_eq!(command, im::CMD_MOVE_TO_COLOR_TEMPERATURE);
        assert_eq!(
            fields.unwrap(),
            im::encode_move_to_color_temperature_fields(370, 0)
        );

        let lv = Op::GroupLevel {
            group_id: 10,
            level: 254,
            percent: 100,
            transition: 0,
            endpoint: 1,
        };
        let (_, cluster, command, fields, _) = native_group_params(&lv).unwrap().unwrap();
        assert_eq!(cluster, im::CLUSTER_LEVEL_CONTROL);
        assert_eq!(command, im::CMD_MOVE_TO_LEVEL);
        assert_eq!(fields.unwrap(), im::encode_move_to_level_fields(254, 0));

        let color = Op::GroupColor {
            group_id: 10,
            hue_raw: 180,
            saturation_raw: 200,
            hue: 254,
            saturation: 78,
            name: None,
            rgb: None,
            transition: 0,
            endpoint: 1,
        };
        let (_, cluster, command, fields, _) = native_group_params(&color).unwrap().unwrap();
        assert_eq!(cluster, im::CLUSTER_COLOR_CONTROL);
        assert_eq!(command, im::CMD_MOVE_TO_HUE_AND_SATURATION);
        assert_eq!(
            fields.unwrap(),
            im::encode_move_to_hue_and_saturation_fields(180, 200, 0)
        );

        // GroupProvision は native_group_params の対象外（専用ハンドラ group_provision）。
        assert!(native_group_params(&Op::Ping).is_none());
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
                address: Some("192.0.2.10".into()),
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
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-06-08T00:00:00+09:00".into(),
            })
            .unwrap();
        dir
    }

    #[test]
    fn generic_ops_join_the_native_hotpath() {
        let read = Op::Read {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "current-level".into(),
        };
        assert!(is_native_hotpath(&read));
        let unknown = Op::Read {
            node_id: 5,
            endpoint: 1,
            cluster: "nosuch".into(),
            attribute: "x".into(),
        };
        assert!(!is_native_hotpath(&unknown)); // 未知名は unresolved_op_error（run_op）。
        let write = Op::Write {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "on-level".into(),
            value: "128".into(),
        };
        assert!(is_native_hotpath(&write));
        let inv = Op::Invoke {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            command: "move-to-level".into(),
            args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
        };
        assert!(is_native_hotpath(&inv));
        assert!(is_native_hotpath(&Op::Describe { node_id: 5 }));
    }

    #[tokio::test]
    async fn native_generic_read_body_matches_expected_schema() {
        // FakeConn の read_json は json!(1) を返す（Task 6 の fake 仕様）。
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let op = Op::Read {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "current-level".into(),
        };
        let body = native_op(&op, &native, store_with_node_5().path())
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
        let op = Op::Write {
            node_id: 5,
            endpoint: 0,
            cluster: "accesscontrol".into(),
            attribute: "acl".into(),
            value: "[]".into(),
        };
        let err = native_op(&op, &native, store_with_node_5().path())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[tokio::test]
    async fn native_generic_invoke_and_describe_bodies_match_expected_schema() {
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let dir = store_with_node_5();

        let invoke = Op::Invoke {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            command: "move-to-level".into(),
            args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
        };
        let body = native_op(&invoke, &native, dir.path()).await.unwrap();
        // 既存 simple_op(Invoke) と同形（node_id/endpoint/cluster/command/status）。
        assert_eq!(body["node_id"], 5);
        assert_eq!(body["endpoint"], 1);
        assert_eq!(body["cluster"], "levelcontrol");
        assert_eq!(body["command"], "move-to-level");
        assert_eq!(body["status"], "success");

        let describe = Op::Describe { node_id: 5 };
        let body = native_op(&describe, &native, dir.path()).await.unwrap();
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
        let body = group_provision(&op, &native, &store_path).await.unwrap();
        assert_eq!(body["status"], "provisioned");
        assert_eq!(body["nodes"], json!([1]));
        assert!(mat_controller::kvs::read_group_credentials(&ini, 2, 99).is_ok());
    }

    /// group_settings_ctx が未構成（テスト注入時のみ起こり得る）だと internal エラー。
    #[tokio::test]
    async fn group_provision_without_group_settings_ctx_is_internal_error() {
        let native = NativeBackend::with_establisher(Box::new(ScriptedEstablisher));
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
        let err = group_provision(&op, &native, &store_path)
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
                scope_id: cand.index,
                dest_port: port,
                transport,
                sender: tokio::sync::Mutex::new(None),
            };
            let native = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), Some(ctx));

            let body = run_op(
                &group_on_op(),
                &NativeState::Ready(Box::new(native)),
                &store_path,
                &SubHealth::new(None),
            )
            .await
            .unwrap();
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
    async fn group_op_returns_store_parse_when_group_ctx_unavailable() {
        let (_dir, store_path) = make_store();
        // group ctx なしの native → Unavailable → M8c-3: フォールバック無しで
        // store_parse（`mat group provision` へ誘導する detail）を即返す。
        let native = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), None);
        let err = run_op(
            &group_on_op(),
            &NativeState::Ready(Box::new(native)),
            &store_path,
            &SubHealth::new(None),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreParse);
        assert!(err.detail.contains("native group send unavailable"));
    }

    #[tokio::test]
    async fn run_op_returns_build_error_uniformly_when_native_unavailable() {
        // 起動時 native 構築失敗（KVS 不在等）は、Ping/Shutdown 以外の全 op へ
        // その構築エラーをそのまま返す（M8c-3: 一律化、Task 9 と同じ精度）。
        let (_dir, store_path) = make_store();
        let build_err = MatError::store_missing("no KVS materials for native backend");
        let state = NativeState::Unavailable(build_err.clone());
        let health = SubHealth::new(None);

        let err = run_op(&group_on_op(), &state, &store_path, &health)
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
        let err = run_op(&read, &state, &store_path, &health)
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreMissing);

        // Ping/Shutdown だけは native に触れず常に成功する。
        assert_eq!(
            run_op(&Op::Ping, &state, &store_path, &health)
                .await
                .unwrap(),
            json!({ "pong": true })
        );
        assert_eq!(
            run_op(&Op::Shutdown, &state, &store_path, &health)
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
                address: Some("192.0.2.10".into()),
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
        )
        .await
        .unwrap();
        assert!(health.pending_elapsed(5).is_none());
    }

    /// post-1.0 defer: dispatch 不変条件が破れても panic しない（v1 Task6 規律）。
    #[tokio::test]
    async fn native_op_invariant_violations_are_typed_errors_not_panics() {
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let store = store_with_node_5();

        // NotNative write（未知 cluster 名 → classify_write が NotNative）
        let op = Op::Write {
            node_id: 5,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            attribute: "x".into(),
            value: "1".into(),
        };
        let err = native_op(&op, &native, store.path()).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);

        // NotNative invoke
        let op = Op::Invoke {
            node_id: 5,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            command: "x".into(),
            args: vec![],
        };
        let err = native_op(&op, &native, store.path()).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);

        // non-hotpath op（Ping は node_id() が None なので require_node を素通りして
        // catch-all に到達する）
        let err = native_op(&Op::Ping, &native, store.path())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);
    }

    #[tokio::test]
    async fn group_provision_rejects_non_group_provision_op_without_panic() {
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let dir = tempfile::tempdir().unwrap();
        let err = group_provision(&Op::Ping, &native, dir.path())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);
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
}
