//! mat → matd クライアント経路。
//!
//! 経路は 3 状態: `--matd` / `MAT_MATD=truthy` で**強制 matd**（接続失敗はエラー、
//! フォールバック無し）、`MAT_MATD=falsy` で**強制 native 直経路**、どちらも無ければ
//! **自動検出**（既定ソケットへ connect を試み、matd がいればそちら、いなければ
//! native 直経路にフォールバック）。`MAT_MATD_SOCKET` は「どのソケットか」の指定のみで
//! 経路は変えない。
//!
//! matd は unix socket 上で newline-delimited JSON を喋る（1 行 = 1 リクエスト = 1
//! レスポンス）。ここはサブコマンドを matd の op JSON に変換して 1 行送り、返ってきた
//! 1 行（mat スキーマ）を stdout（成功）/ stderr（エラー）へ出すだけの薄い口。
//!
//! mat 本体は同期コードなので接続も std の [`UnixStream`] を使う（tokio は matd 内部
//! の native エンジン用で、上流 ⇔ matd は unix socket）。M8c-3 で chip-tool は撤去済み
//! — この経路も native 直経路も、プロトコルは全て mat-controller / mat-native
//! （in-process）が担う。

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::{json, Value};

use crate::cli::Command;
use crate::device_op::DeviceOp;
use mat_core::alias::NodeRef;
use mat_core::error::{ErrorKind, MatError};
use mat_core::socket::default_socket_candidates;
use mat_native::op::{GroupOpKind, NodeOpKind};

/// matd の構造化エラーを待つ read timeout の余裕。matd は予算ちょうどで構造化
/// timeout を返すので、こちらは予算 + slack まで待って必ず先に受け取る。
/// slack を使い切る（= matd が予算内に応答しない）のは旧 matd か matd 停止。
const CLIENT_SLACK: Duration = Duration::from_secs(2);

/// 予算対象 op（`DeviceOp::budget_applies`）へ deadline_ms を付与し、
/// 適用時の read timeout を返す。非対象は無変更・read timeout なし。
/// 0 = 明示無制限（matd 既定 60s の適用を止める）— read timeout も掛けない。
fn attach_deadline(op: &mut Value, applies: bool, op_timeout_ms: u64) -> Option<Duration> {
    if !applies {
        return None;
    }
    if let Value::Object(map) = op {
        map.insert("deadline_ms".into(), json!(op_timeout_ms));
    }
    (op_timeout_ms > 0).then(|| Duration::from_millis(op_timeout_ms) + CLIENT_SLACK)
}

/// mat の実行経路。`resolve_route` が決める。socket は探索候補リスト
/// （明示指定は 1 本、既定は subdir 新既定 → flat 旧既定の順で connect 試行）。
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// 明示有効化（`--matd` / `MAT_MATD=truthy`）: matd 固定。全候補接続失敗は
    /// エラー、非対応 op は exit 2。フォールバックしない。
    Forced(Vec<PathBuf>),
    /// 既定（どちらも未設定）: 候補へ順に connect を試み、成功なら matd、
    /// 全滅なら mat 自身の native 直経路にフォールバック。
    Auto(Vec<PathBuf>),
    /// 明示無効化（`MAT_MATD=falsy`）: 常に native 直経路。probe もしない。
    Direct,
}

/// 経路と socket 候補を決める（純粋関数; env は注入）。
///
/// - `--matd [<path>]` or `MAT_MATD=truthy` → `Forced`
/// - `MAT_MATD=falsy`（`0`/`false`/`no`/`off`） → `Direct`
/// - どちらも無し（truthy/falsy どちらでもない値も同じ） → `Auto`
///
/// socket 候補の優先順: `--matd <path>`（明示、1 本）> `MAT_MATD_SOCKET=<path>`（非空、
/// 1 本）> 既定候補（subdir → flat）。`MAT_MATD_SOCKET` はパス指定のみで経路は変えない。
pub fn resolve_route(
    flag: &Option<Option<PathBuf>>,
    env_socket: Option<OsString>,
    env_enable: Option<OsString>,
) -> Route {
    match flag {
        // --matd <path> → 明示パスで強制 matd（候補 1 本）。
        Some(Some(path)) => Route::Forced(vec![path.clone()]),
        // --matd（値省略）→ 強制 matd。パスは MAT_MATD_SOCKET > 既定候補。
        Some(None) => Route::Forced(sockets_from_env_or_default(env_socket)),
        None => match env_enable.as_deref() {
            Some(v) if is_truthy(v) => Route::Forced(sockets_from_env_or_default(env_socket)),
            Some(v) if is_falsy(v) => Route::Direct,
            // 未設定（or 解釈不能な値）→ 自動検出。
            _ => Route::Auto(sockets_from_env_or_default(env_socket)),
        },
    }
}

/// 有効化済みのときに使う socket 候補: `MAT_MATD_SOCKET`（非空、1 本）> 既定候補。
fn sockets_from_env_or_default(env_socket: Option<OsString>) -> Vec<PathBuf> {
    env_socket
        .filter(|s| !s.is_empty())
        .map(|s| vec![PathBuf::from(s)])
        .unwrap_or_else(default_socket_candidates)
}

/// `MAT_MATD` の真偽判定。`1` / `true` / `yes` / `on`（大小無視）を有効とみなす。
fn is_truthy(v: &OsStr) -> bool {
    matches!(
        v.to_str().map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// `MAT_MATD` の否定判定。`0` / `false` / `no` / `off`（大小無視）を無効化とみなす。
/// truthy とも falsy とも解釈できない値は「未設定」と同じ（自動検出）。
fn is_falsy(v: &OsStr) -> bool {
    matches!(
        v.to_str().map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

/// `--matd` 強制時の非対応 op。kind=other だが exit 2 を返すのは「2 = CLI
/// 引数エラー」の documented シグナルを保つ意図的な例外（spec B 節）。
pub fn unsupported_exit(name: &str) -> ExitCode {
    MatError::new(ErrorKind::Other, unsupported_detail(name)).emit();
    ExitCode::from(2)
}

fn unsupported_detail(name: &str) -> String {
    format!(
        "`mat --matd` does not support the `{name}` subcommand; run it without --matd (direct native path)"
    )
}

/// `--matd` 指定時のディスパッチ。非対応 op は CLI 利用の誤り（exit 2）。
/// alias / color spec 解決は `device_op::classify` が既に済ませているため、
/// ここでの唯一の失敗理由は matd 非対応 op（`to_op` の `Err`）。
pub fn dispatch(sockets: &[PathBuf], op: &DeviceOp, op_timeout_ms: u64) -> ExitCode {
    let mut op_json = match to_op(op) {
        Ok(v) => v,
        // 非対応 op は CLI 利用誤り。kind=other(exit_code()=1) だが exit 2 を
        // 返すのは「2 = CLI 引数エラー」の documented シグナルを保つ意図的な
        // 例外（spec B 節、テストでピン留め）。
        Err(detail) => {
            MatError::new(ErrorKind::Other, &detail).emit();
            return ExitCode::from(2);
        }
    };

    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            MatError::new(ErrorKind::MatdUnavailable, &detail).emit();
            return ExitCode::from(ErrorKind::MatdUnavailable.exit_code());
        }
    };
    tracing::info!(socket = %socket.display(), "using matd (forced)");

    let read_timeout = attach_deadline(&mut op_json, op.budget_applies(), op_timeout_ms);
    match exchange_on_stream(stream, &op_json, read_timeout) {
        Ok(resp) => emit_response(resp),
        Err(e) => {
            e.emit();
            ExitCode::from(e.kind.exit_code())
        }
    }
}

/// 自動検出モードのディスパッチ。matd 経路で完結した場合のみ `Some(exit code)`。
/// `None` = 呼び出し側が native 直経路で実行すべき（matd 非対応 op / connect 失敗）。
///
/// connect した stream をそのまま本リクエストに使う（probe 後の再接続はしない）ので、
/// フォールバックが起きるのは 1 バイトも送る前だけ。接続後のエラーは matd 経路の
/// エラーとしてそのまま返し、直経路で再実行しない（write / invoke の二重実行防止）。
pub fn dispatch_auto(sockets: &[PathBuf], op: &DeviceOp, op_timeout_ms: u64) -> Option<ExitCode> {
    // matd 非対応 op（open-window / diag thread / grant）は probe せず直経路。
    let mut op_json = match to_op(op) {
        Ok(v) => v,
        Err(_) => return None,
    };

    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            tracing::info!(
                error = %detail,
                "matd not reachable, falling back to direct native backend"
            );
            return None;
        }
    };
    tracing::info!(socket = %socket.display(), "using matd (auto-detected)");

    let read_timeout = attach_deadline(&mut op_json, op.budget_applies(), op_timeout_ms);
    Some(match exchange_on_stream(stream, &op_json, read_timeout) {
        Ok(resp) => emit_response(resp),
        Err(e) => {
            e.emit();
            ExitCode::from(e.kind.exit_code())
        }
    })
}

/// `DeviceOp` を matd の op JSON に変換する。wire は名前のまま（`*_in`）—
/// 契約不変。直経路専用 op は `Err(detail)`。
fn to_op(op: &DeviceOp) -> Result<Value, String> {
    Ok(match op {
        DeviceOp::Node(n) => {
            let node_id = n.node_id;
            match &n.kind {
                NodeOpKind::Read {
                    endpoint,
                    cluster_in,
                    attribute_in,
                    ..
                } => json!({
                    "op": "read", "node_id": node_id, "endpoint": endpoint,
                    "cluster": cluster_in, "attribute": attribute_in,
                }),
                NodeOpKind::ReadCluster {
                    endpoint,
                    cluster_in,
                    ..
                } => json!({
                    "op": "read", "node_id": node_id, "endpoint": endpoint,
                    "cluster": cluster_in,
                }),
                NodeOpKind::Write {
                    endpoint,
                    cluster_in,
                    attribute_in,
                    value_in,
                    timed,
                    ..
                } => {
                    let mut op = json!({
                        "op": "write", "node_id": node_id, "endpoint": endpoint,
                        "cluster": cluster_in, "attribute": attribute_in, "value": value_in,
                    });
                    if *timed {
                        op["timed"] = json!(true);
                    }
                    op
                }
                NodeOpKind::Invoke {
                    endpoint,
                    cluster_in,
                    command_in,
                    args_in,
                    timed,
                    ..
                } => {
                    let mut op = json!({
                        "op": "invoke", "node_id": node_id, "endpoint": endpoint,
                        "cluster": cluster_in, "command": command_in, "args": args_in,
                    });
                    if *timed {
                        op["timed"] = json!(true);
                    }
                    op
                }
                NodeOpKind::Describe => json!({ "op": "describe", "node_id": node_id }),
                NodeOpKind::On { endpoint } => {
                    json!({ "op": "on", "node_id": node_id, "endpoint": endpoint })
                }
                NodeOpKind::Off { endpoint } => {
                    json!({ "op": "off", "node_id": node_id, "endpoint": endpoint })
                }
                // 換算済み値を渡し、kelvin / percent / 度 / % / name / rgb は
                // 応答エコー用（matd 側で逆算すると丸めで入力とずれる）。
                NodeOpKind::ColorTemp {
                    endpoint,
                    kelvin,
                    mireds,
                    transition,
                } => json!({
                    "op": "color_temp", "node_id": node_id, "endpoint": endpoint,
                    "mireds": mireds, "kelvin": kelvin, "transition": transition,
                }),
                NodeOpKind::Level {
                    endpoint,
                    percent,
                    level,
                    transition,
                } => json!({
                    "op": "level", "node_id": node_id, "endpoint": endpoint,
                    "level": level, "percent": percent, "transition": transition,
                }),
                NodeOpKind::Color {
                    endpoint,
                    color,
                    transition,
                } => {
                    let mut op = json!({
                        "op": "color", "node_id": node_id, "endpoint": endpoint,
                        "hue_raw": color.hue_raw, "saturation_raw": color.sat_raw,
                        "hue": color.hue, "saturation": color.sat, "transition": transition,
                    });
                    if let Some(name) = &color.name {
                        op["name"] = json!(name);
                    }
                    if let Some(rgb) = &color.rgb {
                        op["rgb"] = json!(rgb);
                    }
                    op
                }
                // matd は warm CASE セッション層。これらは直経路でしか実行できない。
                NodeOpKind::DiagThread { .. } => return Err(unsupported_detail("diag")),
                NodeOpKind::OpenWindow { .. } => return Err(unsupported_detail("open-window")),
                NodeOpKind::RemoveFabric => return Err(unsupported_detail("unpair")),
            }
        }
        DeviceOp::Group(g) => {
            let (group_id, endpoint) = (g.group_id, g.endpoint);
            match &g.kind {
                GroupOpKind::Invoke {
                    cluster_in,
                    command_in,
                    args_in,
                    ..
                } => json!({
                    "op": "group_invoke", "group_id": group_id, "cluster": cluster_in,
                    "command": command_in, "args": args_in, "endpoint": endpoint,
                }),
                GroupOpKind::ColorTemp {
                    kelvin,
                    mireds,
                    transition,
                } => json!({
                    "op": "group_color_temp", "group_id": group_id,
                    "mireds": mireds, "kelvin": kelvin,
                    "transition": transition, "endpoint": endpoint,
                }),
                GroupOpKind::Level {
                    percent,
                    level,
                    transition,
                } => json!({
                    "op": "group_level", "group_id": group_id,
                    "level": level, "percent": percent,
                    "transition": transition, "endpoint": endpoint,
                }),
                GroupOpKind::Color { color, transition } => {
                    let mut op = json!({
                        "op": "group_color", "group_id": group_id,
                        "hue_raw": color.hue_raw, "saturation_raw": color.sat_raw,
                        "hue": color.hue, "saturation": color.sat,
                        "transition": transition, "endpoint": endpoint,
                    });
                    if let Some(name) = &color.name {
                        op["name"] = json!(name);
                    }
                    if let Some(rgb) = &color.rgb {
                        op["rgb"] = json!(rgb);
                    }
                    op
                }
            }
        }
        DeviceOp::GroupProvision(p) => json!({
            "op": "group_provision", "group_id": p.group_id, "node_ids": p.node_ids,
            "keyset_id": p.keyset_id, "name": p.name, "endpoint": p.endpoint,
            "epoch_key": p.epoch_key, "rebind": p.rebind,
        }),
        DeviceOp::GroupBump => json!({ "op": "group_bump" }),
        // grant は稀な修復操作で warm session の恩恵が小さく、mat/matd の
        // バージョンスキューにも安全なため直経路のみ。
        DeviceOp::GroupGrant { .. } => return Err(unsupported_detail("group grant")),
    })
}

/// 直経路 op（native_direct）完了後、matd がいれば `node_touched` ヒントを送る
/// fire-and-forget 通知（Issue #20）。常駐購読が古いセッションを掴んだままに
/// なるのを防ぐための best-effort で、matd 不在・旧 matd（`parse_error` 応答）・
/// タイムアウトなど全ての失敗は呼び出し側（native_direct の op 結果 / exit code）
/// に一切影響させない（`tracing::debug!` のみ）。`attach_deadline` /
/// `emit_response` は使わない専用送信路。
pub(crate) fn hint_node_touched(node_id: u64) {
    let sockets = sockets_from_env_or_default(std::env::var_os("MAT_MATD_SOCKET"));
    hint_node_touched_at(&sockets, node_id);
}

/// [`hint_node_touched`] の socket 候補注入版（テスト用に env 非依存の核）。
fn hint_node_touched_at(sockets: &[PathBuf], node_id: u64) {
    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            tracing::debug!(node_id, error = %detail, "node_touched hint: matd unreachable");
            return;
        }
    };
    if let Err(e) = send_hint_line(stream, node_id) {
        tracing::debug!(
            node_id,
            socket = %socket.display(),
            error = %e,
            "node_touched hint: send/recv failed"
        );
    }
}

/// `{"op":"node_touched","node_id":N}` を 1 行送り、応答を 1 行読み捨てる。
///
/// ブロッキング I/O（std `UnixStream` の read timeout）を使うのは、呼び出し元が
/// one-shot CLI の終了間際で他に走っている非同期タスクが無いため（async 化す
/// る価値が無い）。応答本体には関心が無い（ack `{"resubscribing":true}` でも
/// 旧 matd の `parse_error` でも同じ扱い）ので、300ms 上限で読み捨てるだけで
/// 十分（matd 応答が来ない＝matd 停止/ハング相当、これ以上待つ理由が無い）。
fn send_hint_line(mut stream: UnixStream, node_id: u64) -> std::io::Result<()> {
    let op = json!({ "op": "node_touched", "node_id": node_id });
    let mut line = serde_json::to_vec(&op)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    let _ = reader.read_line(&mut resp); // 応答は読み捨てるだけ（内容不問）
    Ok(())
}

/// 候補 socket へ順に connect し、最初に成功した stream と使用パスを返す。
/// 全滅は Err（試行した全パスと各エラーを列挙 — Forced 経路のエラー detail 用）。
fn connect_candidates(sockets: &[PathBuf]) -> Result<(UnixStream, &Path), String> {
    let mut attempts = Vec::new();
    for socket in sockets {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok((stream, socket)),
            Err(e) => attempts.push(format!("{} ({e})", socket.display())),
        }
    }
    Err(format!(
        "could not connect to matd at {}",
        attempts.join(", ")
    ))
}

/// 接続済み stream で 1 行送り 1 行受け取る（自動検出は probe した接続を使い回す）。
///
/// v1 品質修正 3: 途中失敗を typed error 化。送受信の I/O 断・応答なし切断は
/// 「matd がいなくなった」= `matd_unavailable`（送信後はリクエストが実行済みの
/// 可能性があるので detail で明示）。応答が JSON でないのは `parse_error`。
fn exchange_on_stream(
    mut stream: UnixStream,
    op: &Value,
    read_timeout: Option<Duration>,
) -> Result<Value, MatError> {
    let mut line = serde_json::to_vec(op)
        .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to encode request: {e}")))?;
    line.push(b'\n');
    stream.write_all(&line).map_err(|e| {
        MatError::new(
            ErrorKind::MatdUnavailable,
            format!("failed to send request to matd: {e}"),
        )
    })?;

    if let Some(t) = read_timeout {
        stream.set_read_timeout(Some(t)).map_err(|e| {
            MatError::new(ErrorKind::Other, format!("failed to set read timeout: {e}"))
        })?;
    }

    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    let n = reader.read_line(&mut resp).map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            MatError::new(
                ErrorKind::Timeout,
                format!("no response from matd within the op budget: {e}; the request may have been executed"),
            )
        } else {
            MatError::new(
                ErrorKind::MatdUnavailable,
                format!("failed to read response from matd: {e}; the request may have been executed"),
            )
        }
    })?;
    if n == 0 {
        return Err(MatError::new(
            ErrorKind::MatdUnavailable,
            "matd closed the connection without responding; the request may have been executed",
        ));
    }
    serde_json::from_str(&resp)
        .map_err(|e| MatError::parse_error(format!("matd response was not JSON: {e}; body={resp}")))
}

/// matd 応答を mat の規約どおり出力する: 成功は stdout、エラーは stderr。exit code は
/// error.kind から逆引きする（matd と mat で ErrorKind 表が共通）。
fn emit_response(resp: Value) -> ExitCode {
    if let Some(err) = resp.get("error") {
        eprintln!("{resp}");
        let kind = match err
            .get("kind")
            .and_then(|k| serde_json::from_value::<ErrorKind>(k.clone()).ok())
        {
            Some(k) => k,
            None => {
                let raw_kind = err.get("kind").cloned().unwrap_or(Value::Null);
                tracing::warn!(
                    kind = %raw_kind,
                    "unknown error kind from matd; mapping to `other` for the exit code"
                );
                ErrorKind::Other
            }
        };
        ExitCode::from(kind.exit_code())
    } else {
        println!("{resp}");
        ExitCode::SUCCESS
    }
}

/// listen リクエスト行を組む（None フィルタは省略）。
fn listen_request_json(
    node: Option<u64>,
    endpoint: Option<u16>,
    cluster: &Option<String>,
    attribute: &Option<String>,
) -> Value {
    let mut op = json!({ "op": "listen" });
    if let Some(n) = node {
        op["node_id"] = json!(n);
    }
    if let Some(e) = endpoint {
        op["endpoint"] = json!(e);
    }
    if let Some(c) = cluster {
        op["cluster"] = json!(c);
    }
    if let Some(a) = attribute {
        op["attribute"] = json!(a);
    }
    op
}

/// `mat listen`: matd へ接続し、ack 後のイベント行をそのまま stdout へ流す。
/// count/timeout は mat 側制御（enl listen と同じ UX）。matd 不在・応答なし・
/// ストリーム途中の matd 落ちは `matd_unavailable`（exit 13）。`--reconnect`
/// 指定時はその喪失を backoff 再接続で跨ぐ（count 累積・deadline 1 本）。
pub fn dispatch_listen(sockets: &[PathBuf], command: &Command) -> ExitCode {
    let Command::Listen {
        node_id,
        endpoint,
        cluster,
        attribute,
        count,
        timeout_ms,
        reconnect,
    } = command
    else {
        // 内部バグ経路: 非 Listen command が来ても panic しない（v1 Task6 規律）。
        let e = MatError::parse_error(
            "internal: dispatch_listen called with non-Listen command (dispatch invariant violated)",
        );
        e.emit();
        return ExitCode::from(e.kind.exit_code());
    };
    // 未解決 alias が届いた場合（内部バグ）は typed error を emit して抜ける
    // （他の経路と同じ MatError::emit + exit_code パターン、panic しない）。
    let node_num = match node_id.as_ref().map(NodeRef::id).transpose() {
        Ok(n) => n,
        Err(e) => {
            e.emit();
            return ExitCode::from(e.kind.exit_code());
        }
    };
    let endpoint_num = match endpoint
        .as_ref()
        .map(mat_core::alias::EndpointRef::id)
        .transpose()
    {
        Ok(e) => e,
        Err(e) => {
            e.emit();
            return ExitCode::from(e.kind.exit_code());
        }
    };
    let op = listen_request_json(node_num, endpoint_num, cluster, attribute);

    if *reconnect {
        return run_listen_reconnecting(sockets, &op, *count, *timeout_ms);
    }

    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            MatError::new(
                ErrorKind::MatdUnavailable,
                format!("{detail}; `mat listen` requires a running matd"),
            )
            .emit();
            return ExitCode::from(ErrorKind::MatdUnavailable.exit_code());
        }
    };
    tracing::info!(socket = %socket.display(), "listening via matd");

    match run_listen_stream(stream, &op, *count, *timeout_ms) {
        Ok(code) => code,
        Err(detail) => {
            MatError::new(ErrorKind::MatdUnavailable, &detail).emit();
            ExitCode::from(ErrorKind::MatdUnavailable.exit_code())
        }
    }
}

/// 従来の単一接続 listen（`--reconnect` 無し）。ack → イベント行ループ。
/// 戻り値 Ok(exit code) / Err(detail) = matd 落ち扱い。
fn run_listen_stream(
    stream: UnixStream,
    op: &Value,
    count: u32,
    timeout_ms: u64,
) -> Result<ExitCode, String> {
    let deadline = listen_deadline(timeout_ms);
    let mut received = 0u32;
    stream_events(BufReader::new(stream), op, count, deadline, &mut received)
}

/// `--timeout-ms` を締切に変換（0 = 無期限）。
fn listen_deadline(timeout_ms: u64) -> Option<std::time::Instant> {
    (timeout_ms > 0)
        .then(|| std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms))
}

/// 1 接続分: listen 要求送信 → ack → イベント行ループ。`Ok(code)` = 終端
/// （count 到達 / timeout / エラー行）、`Err(detail)` = 接続喪失（EOF・read
/// エラー・非 JSON 行・ack 不正）。`received` は呼び手が再接続を跨いで持つ。
fn stream_events(
    mut reader: BufReader<UnixStream>,
    op: &Value,
    count: u32,
    deadline: Option<std::time::Instant>,
    received: &mut u32,
) -> Result<ExitCode, String> {
    use std::time::Instant;

    let mut line = serde_json::to_vec(op).map_err(|e| format!("failed to encode request: {e}"))?;
    line.push(b'\n');
    // UnixStream は &mut で write 可（BufReader は読み側だけを包む）。
    reader
        .get_mut()
        .write_all(&line)
        .map_err(|e| format!("failed to send listen request to matd: {e}"))?;

    let mut first = true; // 1 行目は ack（または即エラー）

    loop {
        // 残り時間を socket の read timeout に反映（0 = 無期限）。
        if let Some(dl) = deadline {
            let remaining = dl.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(finish_on_timeout(*received));
            }
            reader
                .get_ref()
                .set_read_timeout(Some(remaining))
                .map_err(|e| format!("failed to set read timeout: {e}"))?;
        }
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => {
                // EOF = matd がストリーム途中で落ちた（出力済みイベントはそのまま）。
                return Err("matd closed the event stream".to_string());
            }
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(finish_on_timeout(*received));
            }
            Err(e) => return Err(format!("failed to read from matd: {e}")),
        }
        let v: Value = serde_json::from_str(&buf)
            .map_err(|e| format!("matd sent non-JSON line: {e}; body={buf}"))?;
        if let Some(err) = v.get("error") {
            // ack 前のエラー（フィルタ不正等）/ ストリーム中の lag 切断。
            eprintln!("{v}");
            let kind = err
                .get("kind")
                .and_then(|k| serde_json::from_value::<ErrorKind>(k.clone()).ok())
                .unwrap_or(ErrorKind::Other);
            return Ok(ExitCode::from(kind.exit_code()));
        }
        if first {
            // ack 行 `{"listening":true}` は出力せず読み捨てる。
            first = false;
            if v.get("listening").is_none() {
                return Err(format!("matd listen ack malformed: {v}"));
            }
            continue;
        }
        println!("{v}");
        *received += 1;
        if count > 0 && *received >= count {
            return Ok(ExitCode::SUCCESS);
        }
    }
}

/// `--reconnect`: 接続失敗 / 切断を backoff（1s→2s→…→30s 上限、成功でリセット）
/// で再接続し続ける。deadline は 1 本（再接続待ちも含む）、count は累積。
fn run_listen_reconnecting(
    sockets: &[PathBuf],
    op: &Value,
    count: u32,
    timeout_ms: u64,
) -> ExitCode {
    use std::time::{Duration, Instant};

    let deadline = listen_deadline(timeout_ms);
    let mut received = 0u32;
    let mut backoff = Duration::from_secs(1);
    let mut attempt: u32 = 0;
    loop {
        match connect_candidates(sockets) {
            Ok((stream, socket)) => {
                if attempt == 0 {
                    tracing::info!(socket = %socket.display(), "listening via matd");
                } else {
                    tracing::info!(socket = %socket.display(), attempt, "matd reconnected");
                }
                backoff = Duration::from_secs(1);
                match stream_events(BufReader::new(stream), op, count, deadline, &mut received) {
                    Ok(code) => return code,
                    Err(detail) => tracing::warn!(error = %detail, "matd lost; reconnecting"),
                }
            }
            Err(detail) => tracing::warn!(error = %detail, "matd unreachable; reconnecting"),
        }
        attempt += 1;
        let wait = match deadline {
            Some(dl) => {
                let remaining = dl.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return finish_on_timeout(received);
                }
                backoff.min(remaining)
            }
            None => backoff,
        };
        tracing::warn!(
            attempt,
            backoff_ms = wait.as_millis() as u64,
            "reconnecting to matd"
        );
        std::thread::sleep(wait);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// timeout 打ち切り: 0 件なら timeout(exit 3)、1 件以上なら成功（enl 準拠）。
fn finish_on_timeout(received: u32) -> ExitCode {
    if received == 0 {
        MatError::new(ErrorKind::Timeout, "no events received within --timeout-ms").emit();
        ExitCode::from(ErrorKind::Timeout.exit_code())
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_op::DeviceOp;
    use mat_native::op::{GroupOp, GroupOpKind, NodeOp, NodeOpKind, ProvisionParams};

    fn node(node_id: u64, kind: NodeOpKind) -> DeviceOp {
        DeviceOp::Node(NodeOp { node_id, kind })
    }
    fn group(group_id: u16, endpoint: u16, kind: GroupOpKind) -> DeviceOp {
        DeviceOp::Group(GroupOp {
            group_id,
            endpoint,
            kind,
        })
    }

    #[test]
    fn read_maps_to_read_op() {
        let op = node(1, NodeOpKind::read(2, "onoff", "on-off").unwrap());
        assert_eq!(
            to_op(&op).unwrap(),
            json!({"op":"read","node_id":1,"endpoint":2,"cluster":"onoff","attribute":"on-off"})
        );
    }

    #[test]
    fn on_maps_to_on_op_with_endpoint() {
        let op = node(3, NodeOpKind::On { endpoint: 1 });
        assert_eq!(
            to_op(&op).unwrap(),
            json!({"op":"on","node_id":3,"endpoint":1})
        );
    }

    #[test]
    fn color_temp_kelvin_maps_to_color_temp_op_with_converted_mireds() {
        let op = node(6, NodeOpKind::color_temp(1, Some(2700), None, 30));
        // 換算（2700K → 370 mireds）は mat 側で行い、kelvin はエコー用に併送する。
        assert_eq!(
            to_op(&op).unwrap(),
            json!({"op":"color_temp","node_id":6,"endpoint":1,"mireds":370,"kelvin":2700,"transition":30})
        );
    }

    #[test]
    fn color_temp_mireds_maps_with_computed_kelvin_echo() {
        let op = node(6, NodeOpKind::color_temp(1, None, Some(370), 0));
        assert_eq!(
            to_op(&op).unwrap(),
            json!({"op":"color_temp","node_id":6,"endpoint":1,"mireds":370,"kelvin":2703,"transition":0})
        );
    }

    #[test]
    fn color_maps_to_color_op_with_converted_values() {
        let op = node(
            6,
            NodeOpKind::Color {
                endpoint: 1,
                color: mat_core::color::resolve_spec(None, None, Some(330), Some(80)).unwrap(),
                transition: 30,
            },
        );
        // 換算（330° → 233、80% → 203）は mat 側で行い、度 / % はエコー用に併送する。
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"color","node_id":6,"endpoint":1,
                "hue_raw":233,"saturation_raw":203,
                "hue":330,"saturation":80,"transition":30
            })
        );
    }

    #[test]
    fn color_name_op_includes_name_and_rgb_echo() {
        // resolve 層通過後の形（name あり + 正規化済み rgb）。
        let op = node(
            6,
            NodeOpKind::Color {
                endpoint: 1,
                color: mat_core::color::resolve_spec(Some("red"), Some("#ff0000"), None, None)
                    .unwrap(),
                transition: 0,
            },
        );
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"color","node_id":6,"endpoint":1,
                "hue_raw":0,"saturation_raw":254,
                "hue":0,"saturation":100,"transition":0,
                "name":"red","rgb":"#ff0000"
            })
        );
    }

    #[test]
    fn group_provision_fills_default_name_and_keeps_null_epoch() {
        // name 補完（grp<group_id>）は Task 8 の classify のテストで担保。
        let op = DeviceOp::GroupProvision(ProvisionParams {
            group_id: 7,
            node_ids: vec![1, 2],
            keyset_id: 42,
            name: "grp7".into(),
            endpoint: 1,
            epoch_key: None,
            rebind: false,
        });
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"group_provision","group_id":7,"node_ids":[1,2],
                "keyset_id":42,"name":"grp7","endpoint":1,"epoch_key":null,
                "rebind":false
            })
        );
    }

    #[test]
    fn group_bump_maps_to_group_bump_op() {
        assert_eq!(
            to_op(&DeviceOp::GroupBump).unwrap(),
            json!({ "op": "group_bump" })
        );
    }

    #[test]
    fn write_invoke_and_group_invoke_keep_names_and_args_on_the_wire() {
        let w = node(
            1,
            NodeOpKind::write(1, "levelcontrol", "on-level", "128", false).unwrap(),
        );
        assert_eq!(
            to_op(&w).unwrap(),
            json!({"op":"write","node_id":1,"endpoint":1,"cluster":"levelcontrol","attribute":"on-level","value":"128"})
        );
        let args: Vec<String> = vec!["128".into(), "0".into(), "0".into(), "0".into()];
        let i = node(
            1,
            NodeOpKind::invoke(1, "levelcontrol", "move-to-level", &args, false).unwrap(),
        );
        assert_eq!(
            to_op(&i).unwrap(),
            json!({"op":"invoke","node_id":1,"endpoint":1,"cluster":"levelcontrol","command":"move-to-level","args":["128","0","0","0"]})
        );
        let g = group(10, 1, GroupOpKind::invoke("onoff", "on", &[]).unwrap());
        assert_eq!(
            to_op(&g).unwrap(),
            json!({"op":"group_invoke","group_id":10,"cluster":"onoff","command":"on","args":[],"endpoint":1})
        );
        assert_eq!(
            to_op(&node(1, NodeOpKind::Describe)).unwrap(),
            json!({"op":"describe","node_id":1})
        );
    }

    #[test]
    fn timed_true_is_sent_on_wire_and_false_is_omitted() {
        let op = node(1, NodeOpKind::invoke(1, "onoff", "on", &[], true).unwrap());
        assert_eq!(to_op(&op).unwrap()["timed"], json!(true));
        let op = node(1, NodeOpKind::invoke(1, "onoff", "on", &[], false).unwrap());
        assert!(to_op(&op).unwrap().get("timed").is_none());
        let op = node(
            1,
            NodeOpKind::write(1, "levelcontrol", "on-level", "128", true).unwrap(),
        );
        assert_eq!(to_op(&op).unwrap()["timed"], json!(true));
    }

    #[test]
    fn resolve_route_three_states() {
        let some_path = PathBuf::from("/x/y.sock");
        let dflt = mat_core::socket::default_socket_candidates();

        // --matd <path> → 強制 matd（明示パスが MAT_MATD_SOCKET より優先、候補 1 本）。
        assert_eq!(
            resolve_route(
                &Some(Some(some_path.clone())),
                Some("/env.sock".into()),
                None
            ),
            Route::Forced(vec![some_path])
        );
        // --matd（値省略）→ 強制 matd。パスは MAT_MATD_SOCKET（1 本）> 既定候補。
        assert_eq!(
            resolve_route(&Some(None), None, None),
            Route::Forced(dflt.clone())
        );
        assert_eq!(
            resolve_route(&Some(None), Some("/env.sock".into()), None),
            Route::Forced(vec![PathBuf::from("/env.sock")])
        );
        // MAT_MATD=truthy → 強制 matd。
        assert_eq!(
            resolve_route(&None, None, Some("1".into())),
            Route::Forced(dflt.clone())
        );
        // MAT_MATD=falsy → 強制直。socket env が設定されていても probe しない。
        assert_eq!(resolve_route(&None, None, Some("0".into())), Route::Direct);
        assert_eq!(
            resolve_route(&None, Some("/env.sock".into()), Some("off".into())),
            Route::Direct
        );
        // 未設定 → 自動。probe 先は MAT_MATD_SOCKET（非空、1 本）> 既定候補。
        assert_eq!(resolve_route(&None, None, None), Route::Auto(dflt.clone()));
        assert_eq!(
            resolve_route(&None, Some("/env.sock".into()), None),
            Route::Auto(vec![PathBuf::from("/env.sock")])
        );
        // truthy でも falsy でもない値 → 未設定と同じ（自動）。
        assert_eq!(
            resolve_route(&None, None, Some("abc".into())),
            Route::Auto(dflt)
        );
    }

    #[test]
    fn connect_candidates_falls_through_to_second_socket() {
        // 候補 1 = 存在しないパス、候補 2 = 生きた listener → 候補 2 で繋がる。
        let dir = tempfile::tempdir().unwrap();
        let dead = dir.path().join("matd").join("matd.sock"); // 不在（dir ごと無い）
        let alive = dir.path().join("matd.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&alive).unwrap();

        // 戻り値の &Path は候補スライスを借用するため、候補は変数に束縛してから渡す。
        let candidates = [dead, alive.clone()];
        let (_stream, used) = connect_candidates(&candidates).expect("second candidate connects");
        assert_eq!(used, alive.as_path());
    }

    #[test]
    fn connect_candidates_skips_stale_socket_file() {
        // 候補 1 = stale socket ファイル（listener 死亡済み）→ connect 失敗で候補 2 へ。
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap()); // ファイルは残る
        assert!(stale.exists());
        let alive = dir.path().join("alive.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&alive).unwrap();

        let candidates = [stale, alive.clone()];
        let (_stream, used) = connect_candidates(&candidates).expect("stale is skipped");
        assert_eq!(used, alive.as_path());
    }

    #[test]
    fn connect_candidates_error_lists_all_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.sock");
        let b = dir.path().join("b.sock");
        let err = connect_candidates(&[a.clone(), b.clone()]).unwrap_err();
        assert!(err.contains(&a.display().to_string()), "got: {err}");
        assert!(err.contains(&b.display().to_string()), "got: {err}");
    }

    #[test]
    fn group_grant_is_unsupported_via_matd() {
        // grant は稀な修復操作で warm session の恩恵が小さく、mat/matd バージョン
        // スキューにも安全なため直経路のみ（matd プロトコルに op を足さない）。
        let op = DeviceOp::GroupGrant {
            group_id: 1,
            node_ids: vec![5],
        };
        assert!(to_op(&op).unwrap_err().contains("group grant"));
    }

    #[test]
    fn group_color_temp_maps_to_group_color_temp_op() {
        let op = group(1, 1, GroupOpKind::color_temp(Some(2700), None, 0));
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"group_color_temp","group_id":1,
                "mireds":370,"kelvin":2700,"transition":0,"endpoint":1
            })
        );
    }

    #[test]
    fn group_level_maps_to_group_level_op() {
        let op = group(1, 1, GroupOpKind::level(50, 0));
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"group_level","group_id":1,
                "level":127,"percent":50,"transition":0,"endpoint":1
            })
        );
    }

    #[test]
    fn group_color_maps_to_group_color_op_with_echo() {
        let op = group(
            1,
            1,
            GroupOpKind::Color {
                color: mat_core::color::resolve_spec(Some("blue"), Some("#0000ff"), None, None)
                    .unwrap(),
                transition: 0,
            },
        );
        assert_eq!(
            to_op(&op).unwrap(),
            json!({
                "op":"group_color","group_id":1,
                "hue_raw":169,"saturation_raw":254,
                "hue":240,"saturation":100,"transition":0,"endpoint":1,
                "name":"blue","rgb":"#0000ff"
            })
        );
    }

    #[test]
    fn listen_request_json_omits_absent_filters() {
        assert_eq!(
            listen_request_json(None, None, &None, &None),
            json!({"op":"listen"})
        );
        assert_eq!(
            listen_request_json(
                Some(21),
                Some(1),
                &Some("occupancysensing".into()),
                &Some("occupancy".into()),
            ),
            json!({
                "op":"listen","node_id":21,"endpoint":1,
                "cluster":"occupancysensing","attribute":"occupancy"
            })
        );
    }

    /// 直経路専用 op は matd へ送らない（文言はサブコマンド名入り）。
    #[test]
    fn direct_only_ops_are_unsupported_via_matd() {
        let dt = node(1, NodeOpKind::DiagThread { endpoint: 0 });
        assert!(to_op(&dt).unwrap_err().contains("diag"));
        let ow = node(
            1,
            NodeOpKind::OpenWindow {
                timeout: 180,
                iteration: 1000,
                discriminator: 1,
            },
        );
        assert!(to_op(&ow).unwrap_err().contains("open-window"));
    }

    /// v1 品質修正 3: matd 経路の途中失敗が一律 `other` だったのを分離。
    /// 応答なし切断（EOF）= matd 側が死んだ → `matd_unavailable`(exit 13)。
    #[test]
    fn exchange_on_stream_maps_eof_to_matd_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            // リクエスト行を消費してから切断する（先にドロップすると client の
            // write_all がリクエスト到達前に broken pipe で失敗し得るため、EOF-on-read
            // を確実に踏ませるにはここで 1 行読んでおく必要がある）。
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut req = String::new();
            reader.read_line(&mut req).unwrap();
            drop(conn); // 1 行も返さず切断 → クライアント側は EOF
        });
        let stream = UnixStream::connect(&path).unwrap();
        let err = exchange_on_stream(stream, &json!({ "op": "on" }), None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::MatdUnavailable);
        assert!(
            err.detail.contains("may have been executed"),
            "detail should warn about possible partial execution: {}",
            err.detail
        );
        server.join().unwrap();
    }

    /// 応答は来たが JSON でない → `parse_error`（native 経路の出力不能時と同じ分類）。
    #[test]
    fn exchange_on_stream_maps_non_json_response_to_parse_error() {
        use std::io::{BufRead as _, BufReader, Write as _};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut req = String::new();
            reader.read_line(&mut req).unwrap(); // リクエスト 1 行を消費
            let mut conn = conn;
            conn.write_all(b"garbage\n").unwrap();
        });
        let stream = UnixStream::connect(&path).unwrap();
        let err = exchange_on_stream(stream, &json!({ "op": "on" }), None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        server.join().unwrap();
    }

    /// Issue #20: 直経路 op 後の fire-and-forget ヒント。matd がいれば
    /// `{"op":"node_touched","node_id":N}` を 1 行送る（応答は読み捨て）。
    #[test]
    fn hint_node_touched_sends_op_line_to_matd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut req = String::new();
            reader.read_line(&mut req).unwrap();
            let mut conn = conn;
            conn.write_all(b"{\"resubscribing\":true}\n").unwrap();
            req
        });

        hint_node_touched_at(&[path], 42);

        let req = server.join().unwrap();
        let v: Value = serde_json::from_str(&req).unwrap();
        assert_eq!(v, json!({"op":"node_touched","node_id":42}));
    }

    /// 旧 matd（node_touched 未対応）は `parse_error` を返してくるが、
    /// ヒント送信側はその応答も内容を見ずに読み捨てるだけで完走する。
    #[test]
    fn hint_node_touched_ignores_old_matd_parse_error_response() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut req = String::new();
            reader.read_line(&mut req).unwrap();
            let mut conn = conn;
            conn.write_all(b"{\"error\":{\"kind\":\"parse_error\",\"detail\":\"unknown op\"}}\n")
                .unwrap();
        });

        hint_node_touched_at(&[path], 7); // panic せず完走すること

        server.join().unwrap();
    }

    /// matd 不在（socket に誰もいない）でも panic せず、戻り値なしで完走する。
    #[test]
    fn hint_node_touched_is_silent_without_matd() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such.sock");
        hint_node_touched_at(&[missing], 1);
    }

    #[test]
    fn attach_deadline_only_when_budget_applies() {
        let mut op =
            json!({"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"});
        let rt = attach_deadline(&mut op, true, 15_000);
        assert_eq!(op["deadline_ms"], json!(15_000));
        assert_eq!(
            rt,
            Some(std::time::Duration::from_millis(15_000) + CLIENT_SLACK)
        );
        // 0 = 明示無制限: フィールドは付く（matd の既定 60s を止める）が read timeout なし。
        let mut op = json!({"op":"on","node_id":3,"endpoint":1});
        assert_eq!(attach_deadline(&mut op, true, 0), None);
        assert_eq!(op["deadline_ms"], json!(0));
        // 対象外（group 系・bump）: 無変更・read timeout なし。
        let mut op = json!({"op":"group_bump"});
        assert_eq!(attach_deadline(&mut op, false, 15_000), None);
        assert!(op.get("deadline_ms").is_none());
    }

    #[test]
    fn exchange_read_timeout_maps_to_timeout_kind() {
        // 応答しないサーバ相手に read timeout → ErrorKind::Timeout（exit 3）。
        let (client, _server) = UnixStream::pair().unwrap();
        let err = exchange_on_stream(
            client,
            &json!({"op":"ping"}),
            Some(std::time::Duration::from_millis(100)),
        )
        .expect_err("must time out");
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(
            err.detail.contains("may have been executed"),
            "detail: {}",
            err.detail
        );
    }

    #[test]
    fn read_cluster_maps_to_read_op_without_attribute_key() {
        let op = node(1, NodeOpKind::read_cluster(2, "onoff").unwrap());
        assert_eq!(
            to_op(&op).unwrap(),
            json!({"op":"read","node_id":1,"endpoint":2,"cluster":"onoff"})
        );
    }
}
