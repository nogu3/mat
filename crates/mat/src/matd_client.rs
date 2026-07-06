//! mat → matd クライアント経路。
//!
//! 経路は 3 状態: `--matd` / `MAT_MATD=truthy` で**強制 matd**（接続失敗はエラー、
//! フォールバック無し）、`MAT_MATD=falsy` で**強制直 chip-tool**、どちらも無ければ
//! **自動検出**（既定ソケットへ connect を試み、matd がいればそちら、いなければ
//! 直経路にフォールバック）。`MAT_MATD_SOCKET` は「どのソケットか」の指定のみで
//! 経路は変えない。
//!
//! matd は unix socket 上で newline-delimited JSON を喋る（1 行 = 1 リクエスト = 1
//! レスポンス）。ここはサブコマンドを matd の op JSON に変換して 1 行送り、返ってきた
//! 1 行（mat スキーマ）を stdout（成功）/ stderr（エラー）へ出すだけの薄い口。
//!
//! mat 本体は同期コードなので接続も std の [`UnixStream`] を使う（tokio / ws は
//! matd 内部 ⇔ chip-tool 用で、上流 ⇔ matd は unix socket）。chip-tool には触れない。

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{json, Value};

use crate::cli::{Command, GroupCommand};
use mat_core::alias::NodeRef;
use mat_core::error::ErrorKind;
use mat_core::socket::default_socket_path;

/// mat の実行経路。`resolve_route` が決める。
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// 明示有効化（`--matd` / `MAT_MATD=truthy`）: matd 固定。接続失敗はエラー、
    /// 非対応 op は exit 2。フォールバックしない。
    Forced(PathBuf),
    /// 既定（どちらも未設定）: socket へ connect を試み、成功なら matd、
    /// 失敗なら直 chip-tool にフォールバック。
    Auto(PathBuf),
    /// 明示無効化（`MAT_MATD=falsy`）: 常に直 chip-tool。probe もしない。
    Direct,
}

/// 経路と socket パスを決める（純粋関数; env は注入）。
///
/// - `--matd [<path>]` or `MAT_MATD=truthy` → `Forced`
/// - `MAT_MATD=falsy`（`0`/`false`/`no`/`off`） → `Direct`
/// - どちらも無し（truthy/falsy どちらでもない値も同じ） → `Auto`
///
/// socket パスの優先順: `--matd <path>`（明示）> `MAT_MATD_SOCKET=<path>`（非空）>
/// 既定パス。`MAT_MATD_SOCKET` はパス指定のみで経路は変えない。
pub fn resolve_route(
    flag: &Option<Option<PathBuf>>,
    env_socket: Option<OsString>,
    env_enable: Option<OsString>,
) -> Route {
    match flag {
        // --matd <path> → 明示パスで強制 matd。
        Some(Some(path)) => Route::Forced(path.clone()),
        // --matd（値省略）→ 強制 matd。パスは MAT_MATD_SOCKET > 既定。
        Some(None) => Route::Forced(socket_from_env_or_default(env_socket)),
        None => match env_enable.as_deref() {
            Some(v) if is_truthy(v) => Route::Forced(socket_from_env_or_default(env_socket)),
            Some(v) if is_falsy(v) => Route::Direct,
            // 未設定（or 解釈不能な値）→ 自動検出。
            _ => Route::Auto(socket_from_env_or_default(env_socket)),
        },
    }
}

/// 有効化済みのときに使う socket パスを決める: `MAT_MATD_SOCKET`（非空）> 既定パス。
fn socket_from_env_or_default(env_socket: Option<OsString>) -> PathBuf {
    env_socket
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_socket_path)
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
pub fn dispatch(socket: &Path, command: &Command) -> ExitCode {
    let op = match to_op(command) {
        Ok(op) => op,
        Err(detail) => {
            emit_error(ErrorKind::Other, &detail);
            return ExitCode::from(2);
        }
    };

    match exchange(socket, &op) {
        Ok(resp) => emit_response(resp),
        Err(detail) => {
            emit_error(ErrorKind::Other, &detail);
            ExitCode::FAILURE
        }
    }
}

/// 自動検出モードのディスパッチ。matd 経路で完結した場合のみ `Some(exit code)`。
/// `None` = 呼び出し側が直 chip-tool 経路で実行すべき（matd 非対応 op / connect 失敗）。
///
/// connect した stream をそのまま本リクエストに使う（probe 後の再接続はしない）ので、
/// フォールバックが起きるのは 1 バイトも送る前だけ。接続後のエラーは matd 経路の
/// エラーとしてそのまま返し、直経路で再実行しない（write / invoke の二重実行防止）。
pub fn dispatch_auto(socket: &Path, command: &Command) -> Option<ExitCode> {
    // matd 非対応 op（discover / commission / open-window / diag）は probe せず直経路。
    let op = to_op(command).ok()?;

    let stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(
                socket = %socket.display(),
                error = %e,
                "matd not reachable, falling back to direct chip-tool"
            );
            return None;
        }
    };
    tracing::info!(socket = %socket.display(), "using matd (auto-detected)");

    Some(match exchange_on_stream(stream, &op) {
        Ok(resp) => emit_response(resp),
        Err(detail) => {
            emit_error(ErrorKind::Other, &detail);
            ExitCode::FAILURE
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
        Command::Color {
            node_id,
            endpoint,
            hue,
            sat,
            transition,
        } => {
            // 換算は mat 側で 1 箇所（直経路と同じ規則）。matd へは換算済み 0–254 値を
            // 渡し、度 / % は応答エコー用（matd 側で逆算すると丸めで入力とずれる）。
            let (hue_raw, sat_raw) = crate::commands::invoke::resolve_color(*hue, *sat);
            json!({
                "op": "color", "node_id": node_id.id(), "endpoint": endpoint.id(),
                "hue_raw": hue_raw, "saturation_raw": sat_raw,
                "hue": hue, "saturation": sat, "transition": transition,
            })
        }
        Command::Group { action } => match action {
            GroupCommand::Provision {
                group_id,
                node_ids,
                keyset_id,
                name,
                endpoint,
                epoch_key,
            } => {
                // name 未指定なら group_id から決定的に補完（main の直接経路と同じ規則）。
                let gid = group_id.id();
                let name = name.clone().unwrap_or_else(|| format!("grp{gid}"));
                let ids: Vec<u64> = node_ids.iter().map(NodeRef::id).collect();
                json!({
                    "op": "group_provision", "group_id": gid, "node_ids": ids,
                    "keyset_id": keyset_id, "name": name, "endpoint": endpoint,
                    "epoch_key": epoch_key,
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
        },
        // matd は warm CASE セッション層。これらは chip-tool 直経路でしか実行できない。
        Command::Discover { .. } => return Err(unsupported("discover")),
        Command::Commission { .. } => return Err(unsupported("commission")),
        Command::OpenWindow { .. } => return Err(unsupported("open-window")),
        Command::Diag { .. } => return Err(unsupported("diag")),
    };
    Ok(op)
}

fn unsupported(name: &str) -> String {
    format!("`mat --matd` does not support the `{name}` subcommand; run it without --matd (direct chip-tool path)")
}

/// matd へ接続して 1 行送り 1 行受け取る。接続/送受信の失敗は detail 文字列で返す。
fn exchange(socket: &Path, op: &Value) -> Result<Value, String> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| format!("could not connect to matd at {}: {e}", socket.display()))?;
    exchange_on_stream(stream, op)
}

/// 接続済み stream で 1 行送り 1 行受け取る（自動検出は probe した接続を使い回す）。
fn exchange_on_stream(mut stream: UnixStream, op: &Value) -> Result<Value, String> {
    let mut line = serde_json::to_vec(op).map_err(|e| format!("failed to encode request: {e}"))?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|e| format!("failed to send request to matd: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    let n = reader
        .read_line(&mut resp)
        .map_err(|e| format!("failed to read response from matd: {e}"))?;
    if n == 0 {
        return Err("matd closed the connection without responding".to_string());
    }
    serde_json::from_str(&resp).map_err(|e| format!("matd response was not JSON: {e}; body={resp}"))
}

/// matd 応答を mat の規約どおり出力する: 成功は stdout、エラーは stderr。exit code は
/// error.kind から逆引きする（matd と mat で ErrorKind 表が共通）。
fn emit_response(resp: Value) -> ExitCode {
    if let Some(err) = resp.get("error") {
        eprintln!("{resp}");
        let kind = err
            .get("kind")
            .and_then(|k| serde_json::from_value::<ErrorKind>(k.clone()).ok())
            .unwrap_or(ErrorKind::Other);
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
            hue: 330,
            sat: 80,
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
    fn group_provision_fills_default_name_and_keeps_null_epoch() {
        let cmd = Command::Group {
            action: GroupCommand::Provision {
                group_id: GroupRef::Id(7),
                node_ids: vec![NodeRef::Id(1), NodeRef::Id(2)],
                keyset_id: 42,
                name: None,
                endpoint: 1,
                epoch_key: None,
            },
        };
        // name 未指定は grp<group_id> に補完。epoch_key は null のまま（matd 側で生成）。
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({
                "op":"group_provision","group_id":7,"node_ids":[1,2],
                "keyset_id":42,"name":"grp7","endpoint":1,"epoch_key":null
            })
        );
    }

    #[test]
    fn resolve_route_three_states() {
        let some_path = PathBuf::from("/x/y.sock");
        let dflt = default_socket_path();

        // --matd <path> → 強制 matd（明示パスが MAT_MATD_SOCKET より優先）。
        assert_eq!(
            resolve_route(
                &Some(Some(some_path.clone())),
                Some("/env.sock".into()),
                None
            ),
            Route::Forced(some_path)
        );
        // --matd（値省略）→ 強制 matd。パスは MAT_MATD_SOCKET > 既定。
        assert_eq!(
            resolve_route(&Some(None), None, None),
            Route::Forced(dflt.clone())
        );
        assert_eq!(
            resolve_route(&Some(None), Some("/env.sock".into()), None),
            Route::Forced(PathBuf::from("/env.sock"))
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
        // 未設定 → 自動。probe 先は MAT_MATD_SOCKET（非空）> 既定。
        assert_eq!(resolve_route(&None, None, None), Route::Auto(dflt.clone()));
        assert_eq!(
            resolve_route(&None, Some("/env.sock".into()), None),
            Route::Auto(PathBuf::from("/env.sock"))
        );
        // truthy でも falsy でもない値 → 未設定と同じ（自動）。
        assert_eq!(
            resolve_route(&None, None, Some("abc".into())),
            Route::Auto(dflt)
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
        })
        .is_err());
    }
}
