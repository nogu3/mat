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

use serde_json::{json, Value};

use crate::cli::{Command, GroupCommand};
use mat_core::alias::NodeRef;
use mat_core::error::{ErrorKind, MatError};
use mat_core::socket::default_socket_candidates;

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

/// `--matd` 指定時のディスパッチ。非対応サブコマンドは CLI 利用の誤り（exit 2）。
pub fn dispatch(sockets: &[PathBuf], command: &Command) -> ExitCode {
    let op = match to_op(command) {
        Ok(op) => op,
        Err(detail) => {
            emit_error(ErrorKind::Other, &detail);
            return ExitCode::from(2);
        }
    };

    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            emit_error(ErrorKind::MatdUnavailable, &detail);
            return ExitCode::from(ErrorKind::MatdUnavailable.exit_code());
        }
    };
    tracing::info!(socket = %socket.display(), "using matd (forced)");

    match exchange_on_stream(stream, &op) {
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
pub fn dispatch_auto(sockets: &[PathBuf], command: &Command) -> Option<ExitCode> {
    // matd 非対応 op（discover / commission / open-window / diag）は probe せず直経路。
    let op = to_op(command).ok()?;

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

    Some(match exchange_on_stream(stream, &op) {
        Ok(resp) => emit_response(resp),
        Err(e) => {
            e.emit();
            ExitCode::from(e.kind.exit_code())
        }
    })
}

/// サブコマンドを matd の op JSON に変換する。matd 非対応のものは `Err`。
fn to_op(command: &Command) -> Result<Value, String> {
    let op = match command {
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => json!({
            "op": "read", "node_id": node_id.id(), "endpoint": endpoint.id(),
            "cluster": cluster, "attribute": attribute,
        }),
        Command::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => json!({
            "op": "write", "node_id": node_id.id(), "endpoint": endpoint.id(),
            "cluster": cluster, "attribute": attribute, "value": value,
        }),
        Command::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            args,
        } => json!({
            "op": "invoke", "node_id": node_id.id(), "endpoint": endpoint.id(),
            "cluster": cluster, "command": command, "args": args,
        }),
        Command::Describe { node_id } => json!({ "op": "describe", "node_id": node_id.id() }),
        Command::On { node_id, endpoint } => {
            json!({ "op": "on", "node_id": node_id.id(), "endpoint": endpoint.id() })
        }
        Command::Off { node_id, endpoint } => {
            json!({ "op": "off", "node_id": node_id.id(), "endpoint": endpoint.id() })
        }
        Command::ColorTemp {
            node_id,
            endpoint,
            kelvin,
            mireds,
            transition,
        } => {
            // 換算は mat 側で 1 箇所（直経路と同じ規則）。matd へは換算済み mireds を
            // 渡し、kelvin は応答エコー用（matd 側で逆算すると丸めで入力とずれる）。
            let (mireds, kelvin) = crate::commands::invoke::resolve_color_temp(*kelvin, *mireds);
            json!({
                "op": "color_temp", "node_id": node_id.id(), "endpoint": endpoint.id(),
                "mireds": mireds, "kelvin": kelvin, "transition": transition,
            })
        }
        Command::Level {
            node_id,
            endpoint,
            percent,
            transition,
        } => {
            // 換算は mat 側で 1 箇所（直経路と同じ規則）。matd へは換算済み level を
            // 渡し、percent は応答エコー用。
            let level = crate::commands::invoke::resolve_level(*percent);
            json!({
                "op": "level", "node_id": node_id.id(), "endpoint": endpoint.id(),
                "level": level, "percent": percent, "transition": transition,
            })
        }
        Command::Color {
            node_id,
            endpoint,
            spec,
            transition,
        } => {
            // 換算は mat 側で 1 箇所（直経路と同じ規則）。matd へは換算済み 0–254 値を
            // 渡し、度 / % / name / rgb は応答エコー用。
            let c = mat_core::color::resolve_spec(
                spec.name.as_deref(),
                spec.rgb.as_deref(),
                spec.hue,
                spec.sat,
            )
            .map_err(|e| e.detail)?;
            let mut op = json!({
                "op": "color", "node_id": node_id.id(), "endpoint": endpoint.id(),
                "hue_raw": c.hue_raw, "saturation_raw": c.sat_raw,
                "hue": c.hue, "saturation": c.sat, "transition": transition,
            });
            if let Some(name) = &c.name {
                op["name"] = json!(name);
            }
            if let Some(rgb) = &c.rgb {
                op["rgb"] = json!(rgb);
            }
            op
        }
        Command::Group { action } => match action {
            GroupCommand::Provision {
                group_id,
                node_ids,
                keyset_id,
                name,
                endpoint,
                epoch_key,
                rebind,
            } => {
                // name 未指定なら group_id から決定的に補完（main の直接経路と同じ規則）。
                let gid = group_id.id();
                let name = name.clone().unwrap_or_else(|| format!("grp{gid}"));
                let ids: Vec<u64> = node_ids.iter().map(NodeRef::id).collect();
                json!({
                    "op": "group_provision", "group_id": gid, "node_ids": ids,
                    "keyset_id": keyset_id, "name": name, "endpoint": endpoint,
                    "epoch_key": epoch_key, "rebind": rebind,
                })
            }
            GroupCommand::Invoke {
                group_id,
                cluster,
                command,
                args,
                endpoint,
            } => json!({
                "op": "group_invoke", "group_id": group_id.id(), "cluster": cluster,
                "command": command, "args": args, "endpoint": endpoint,
            }),
            // grant は稀な修復操作で warm session の恩恵が小さく、mat/matd の
            // バージョンスキューにも安全なため直経路のみ（matd に op を足さない）。
            GroupCommand::Grant { .. } => return Err(unsupported("group grant")),
            GroupCommand::ColorTemp {
                group_id,
                kelvin,
                mireds,
                transition,
                endpoint,
            } => {
                // 換算は mat 側で 1 箇所（直経路と同じ規則）。kelvin はエコー用。
                let (mireds, kelvin) =
                    crate::commands::invoke::resolve_color_temp(*kelvin, *mireds);
                json!({
                    "op": "group_color_temp", "group_id": group_id.id(),
                    "mireds": mireds, "kelvin": kelvin,
                    "transition": transition, "endpoint": endpoint,
                })
            }
            GroupCommand::Level {
                group_id,
                percent,
                transition,
                endpoint,
            } => {
                // 換算は mat 側で 1 箇所（直経路と同じ規則）。percent はエコー用。
                let level = crate::commands::invoke::resolve_level(*percent);
                json!({
                    "op": "group_level", "group_id": group_id.id(),
                    "level": level, "percent": percent,
                    "transition": transition, "endpoint": endpoint,
                })
            }
            GroupCommand::Color {
                group_id,
                spec,
                transition,
                endpoint,
            } => {
                // 換算は mat 側で 1 箇所。度 / % / name / rgb は応答エコー用。
                let c = mat_core::color::resolve_spec(
                    spec.name.as_deref(),
                    spec.rgb.as_deref(),
                    spec.hue,
                    spec.sat,
                )
                .map_err(|e| e.detail)?;
                let mut op = json!({
                    "op": "group_color", "group_id": group_id.id(),
                    "hue_raw": c.hue_raw, "saturation_raw": c.sat_raw,
                    "hue": c.hue, "saturation": c.sat,
                    "transition": transition, "endpoint": endpoint,
                });
                if let Some(name) = &c.name {
                    op["name"] = json!(name);
                }
                if let Some(rgb) = &c.rgb {
                    op["rgb"] = json!(rgb);
                }
                op
            }
        },
        // matd は warm CASE セッション層。これらは native 直経路でしか実行できない。
        Command::Discover { .. } => return Err(unsupported("discover")),
        Command::Commission { .. } => return Err(unsupported("commission")),
        Command::OpenWindow { .. } => return Err(unsupported("open-window")),
        Command::Diag { .. } => return Err(unsupported("diag")),
        // fabric bootstrap は main.rs が経路解決より前に処理するため、
        // ここへは到達しない（網羅 match を保つためだけの腕）。
        Command::Fabric { .. } => return Err(unsupported("fabric")),
        // listen はストリーミング op で main.rs が経路解決より前に先取りする
        // （`dispatch_listen` 専用経路）ため、ここへは実際には到達しない。
        Command::Listen { .. } => {
            return Err(unsupported(
                "listen (streaming op; handled before route dispatch)",
            ))
        }
    };
    Ok(op)
}

fn unsupported(name: &str) -> String {
    format!("`mat --matd` does not support the `{name}` subcommand; run it without --matd (direct native path)")
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
fn exchange_on_stream(mut stream: UnixStream, op: &Value) -> Result<Value, MatError> {
    let mut line = serde_json::to_vec(op)
        .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to encode request: {e}")))?;
    line.push(b'\n');
    stream.write_all(&line).map_err(|e| {
        MatError::new(
            ErrorKind::MatdUnavailable,
            format!("failed to send request to matd: {e}"),
        )
    })?;

    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    let n = reader.read_line(&mut resp).map_err(|e| {
        MatError::new(
            ErrorKind::MatdUnavailable,
            format!("failed to read response from matd: {e}; the request may have been executed"),
        )
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

/// mat 自身のエラーを stderr に構造化 JSON で出す（matd へ届く前の失敗用）。
fn emit_error(kind: ErrorKind, detail: &str) {
    let body = json!({ "error": { "kind": kind, "detail": detail } });
    eprintln!("{body}");
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
/// ストリーム途中の matd 落ちは `matd_unavailable`（exit 13）。
pub fn dispatch_listen(sockets: &[PathBuf], command: &Command) -> ExitCode {
    let Command::Listen {
        node_id,
        endpoint,
        cluster,
        attribute,
        count,
        timeout_ms,
    } = command
    else {
        unreachable!("dispatch_listen called with non-Listen command");
    };
    let op = listen_request_json(
        node_id.as_ref().map(NodeRef::id),
        endpoint.as_ref().map(mat_core::alias::EndpointRef::id),
        cluster,
        attribute,
    );

    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            emit_error(
                ErrorKind::MatdUnavailable,
                &format!("{detail}; `mat listen` requires a running matd"),
            );
            return ExitCode::from(ErrorKind::MatdUnavailable.exit_code());
        }
    };
    tracing::info!(socket = %socket.display(), "listening via matd");

    match run_listen_stream(stream, &op, *count, *timeout_ms) {
        Ok(code) => code,
        Err(detail) => {
            emit_error(ErrorKind::MatdUnavailable, &detail);
            ExitCode::from(ErrorKind::MatdUnavailable.exit_code())
        }
    }
}

/// ack → イベント行ループ。戻り値 Ok(exit code) / Err(detail) = matd 落ち扱い。
fn run_listen_stream(
    mut stream: UnixStream,
    op: &Value,
    count: u32,
    timeout_ms: u64,
) -> Result<ExitCode, String> {
    use std::time::{Duration, Instant};

    let mut line = serde_json::to_vec(op).map_err(|e| format!("failed to encode request: {e}"))?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|e| format!("failed to send listen request to matd: {e}"))?;

    let deadline = (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms));
    let mut reader = BufReader::new(stream);
    let mut received: u32 = 0;
    let mut first = true; // 1 行目は ack（または即エラー）

    loop {
        // 残り時間を socket の read timeout に反映（0 = 無期限）。
        if let Some(dl) = deadline {
            let remaining = dl.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(finish_on_timeout(received));
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
                return Ok(finish_on_timeout(received));
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
        received += 1;
        if received >= count {
            return Ok(ExitCode::SUCCESS);
        }
    }
}

/// timeout 打ち切り: 0 件なら timeout(exit 3)、1 件以上なら成功（enl 準拠）。
fn finish_on_timeout(received: u32) -> ExitCode {
    if received == 0 {
        emit_error(ErrorKind::Timeout, "no events received within --timeout-ms");
        ExitCode::from(ErrorKind::Timeout.exit_code())
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_core::alias::{EndpointRef, GroupRef, NodeRef};

    #[test]
    fn read_maps_to_read_op() {
        let cmd = Command::Read {
            node_id: NodeRef::Id(1),
            endpoint: EndpointRef::Id(2),
            cluster: "onoff".into(),
            attribute: "on-off".into(),
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({"op":"read","node_id":1,"endpoint":2,"cluster":"onoff","attribute":"on-off"})
        );
    }

    #[test]
    fn on_maps_to_on_op_with_endpoint() {
        let cmd = Command::On {
            node_id: NodeRef::Id(3),
            endpoint: EndpointRef::Id(1),
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({"op":"on","node_id":3,"endpoint":1})
        );
    }

    #[test]
    fn color_temp_kelvin_maps_to_color_temp_op_with_converted_mireds() {
        let cmd = Command::ColorTemp {
            node_id: NodeRef::Id(6),
            endpoint: EndpointRef::Id(1),
            kelvin: Some(2700),
            mireds: None,
            transition: 30,
        };
        // 換算（2700K → 370 mireds）は mat 側で行い、kelvin はエコー用に併送する。
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({"op":"color_temp","node_id":6,"endpoint":1,"mireds":370,"kelvin":2700,"transition":30})
        );
    }

    #[test]
    fn color_temp_mireds_maps_with_computed_kelvin_echo() {
        let cmd = Command::ColorTemp {
            node_id: NodeRef::Id(6),
            endpoint: EndpointRef::Id(1),
            kelvin: None,
            mireds: Some(370),
            transition: 0,
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({"op":"color_temp","node_id":6,"endpoint":1,"mireds":370,"kelvin":2703,"transition":0})
        );
    }

    #[test]
    fn color_maps_to_color_op_with_converted_values() {
        let cmd = Command::Color {
            node_id: NodeRef::Id(6),
            endpoint: EndpointRef::Id(1),
            spec: crate::cli::ColorSpecArgs {
                name: None,
                rgb: None,
                hue: Some(330),
                sat: Some(80),
            },
            transition: 30,
        };
        // 換算（330° → 233、80% → 203）は mat 側で行い、度 / % はエコー用に併送する。
        assert_eq!(
            to_op(&cmd).unwrap(),
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
        let cmd = Command::Color {
            node_id: NodeRef::Id(6),
            endpoint: EndpointRef::Id(1),
            spec: crate::cli::ColorSpecArgs {
                name: Some("red".into()),
                rgb: Some("#ff0000".into()),
                hue: None,
                sat: None,
            },
            transition: 0,
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
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
        let cmd = Command::Group {
            action: GroupCommand::Provision {
                group_id: GroupRef::Id(7),
                node_ids: vec![NodeRef::Id(1), NodeRef::Id(2)],
                keyset_id: 42,
                name: None,
                endpoint: 1,
                epoch_key: None,
                rebind: false,
            },
        };
        // name 未指定は grp<group_id> に補完。epoch_key は null のまま（matd 側で生成）。
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({
                "op":"group_provision","group_id":7,"node_ids":[1,2],
                "keyset_id":42,"name":"grp7","endpoint":1,"epoch_key":null,
                "rebind":false
            })
        );
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
        let cmd = Command::Group {
            action: GroupCommand::Grant {
                group_id: GroupRef::Id(1),
                node_ids: vec![NodeRef::Id(5)],
            },
        };
        assert!(to_op(&cmd).is_err());
    }

    #[test]
    fn group_color_temp_maps_to_group_color_temp_op() {
        let cmd = Command::Group {
            action: GroupCommand::ColorTemp {
                group_id: GroupRef::Id(1),
                kelvin: Some(2700),
                mireds: None,
                transition: 0,
                endpoint: 1,
            },
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({
                "op":"group_color_temp","group_id":1,
                "mireds":370,"kelvin":2700,"transition":0,"endpoint":1
            })
        );
    }

    #[test]
    fn group_level_maps_to_group_level_op() {
        let cmd = Command::Group {
            action: GroupCommand::Level {
                group_id: GroupRef::Id(1),
                percent: 50,
                transition: 0,
                endpoint: 1,
            },
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({
                "op":"group_level","group_id":1,
                "level":127,"percent":50,"transition":0,"endpoint":1
            })
        );
    }

    #[test]
    fn group_color_maps_to_group_color_op_with_echo() {
        let cmd = Command::Group {
            action: GroupCommand::Color {
                group_id: GroupRef::Id(1),
                spec: crate::cli::ColorSpecArgs {
                    name: Some("blue".into()),
                    rgb: Some("#0000ff".into()),
                    hue: None,
                    sat: None,
                },
                transition: 0,
                endpoint: 1,
            },
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
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

    #[test]
    fn discover_and_commission_are_unsupported() {
        assert!(to_op(&Command::Discover { probe: false }).is_err());
        assert!(to_op(&Command::Commission {
            target: "192.0.2.1".into(),
            setup_code: "MT:DUMMY".into(),
            node_id: None,
            alias: None,
            thread_dataset: None,
        })
        .is_err());
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
        let err = exchange_on_stream(stream, &json!({ "op": "on" })).unwrap_err();
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
        let err = exchange_on_stream(stream, &json!({ "op": "on" })).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        server.join().unwrap();
    }
}
