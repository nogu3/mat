//! mat → matd クライアント経路（`--matd` / `MAT_MATD_SOCKET` / `MAT_MATD` 指定時）。
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
use mat_core::error::ErrorKind;
use mat_core::socket::default_socket_path;

/// matd 経由で実行するか、するならどの socket かを決める（純粋関数; env は注入）。
///
/// 優先順: `--matd <path>` > `--matd`（既定パス）> `MAT_MATD_SOCKET=<path>` >
/// `MAT_MATD=<truthy>`（既定パス）> どれも無し（= 直 chip-tool 経路、`None`）。
pub fn resolve_socket(
    flag: &Option<Option<PathBuf>>,
    env_socket: Option<OsString>,
    env_enable: Option<OsString>,
) -> Option<PathBuf> {
    match flag {
        // --matd <path>
        Some(Some(path)) => Some(path.clone()),
        // --matd（値省略）→ 既定パス
        Some(None) => Some(default_socket_path()),
        // フラグ無し → env で opt-in
        None => {
            if let Some(s) = env_socket.filter(|s| !s.is_empty()) {
                return Some(PathBuf::from(s));
            }
            if env_enable.as_deref().is_some_and(is_truthy) {
                return Some(default_socket_path());
            }
            None
        }
    }
}

/// `MAT_MATD` の真偽判定。`1` / `true` / `yes` / `on`（大小無視）を有効とみなす。
fn is_truthy(v: &OsStr) -> bool {
    matches!(
        v.to_str().map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
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

/// サブコマンドを matd の op JSON に変換する。matd 非対応のものは `Err`。
fn to_op(command: &Command) -> Result<Value, String> {
    let op = match command {
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => json!({
            "op": "read", "node_id": node_id, "endpoint": endpoint,
            "cluster": cluster, "attribute": attribute,
        }),
        Command::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => json!({
            "op": "write", "node_id": node_id, "endpoint": endpoint,
            "cluster": cluster, "attribute": attribute, "value": value,
        }),
        Command::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            args,
        } => json!({
            "op": "invoke", "node_id": node_id, "endpoint": endpoint,
            "cluster": cluster, "command": command, "args": args,
        }),
        Command::Describe { node_id } => json!({ "op": "describe", "node_id": node_id }),
        Command::On { node_id, endpoint } => {
            json!({ "op": "on", "node_id": node_id, "endpoint": endpoint })
        }
        Command::Off { node_id, endpoint } => {
            json!({ "op": "off", "node_id": node_id, "endpoint": endpoint })
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
                let name = name.clone().unwrap_or_else(|| format!("grp{group_id}"));
                json!({
                    "op": "group_provision", "group_id": group_id, "node_ids": node_ids,
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
                "op": "group_invoke", "group_id": group_id, "cluster": cluster,
                "command": command, "args": args, "endpoint": endpoint,
            }),
        },
        // matd は warm CASE セッション層。これらは chip-tool 直経路でしか実行できない。
        Command::Discover => return Err(unsupported("discover")),
        Command::Commission { .. } => return Err(unsupported("commission")),
        Command::OpenWindow { .. } => return Err(unsupported("open-window")),
        Command::Diag { .. } => return Err(unsupported("diag")),
    };
    Ok(op)
}

fn unsupported(name: &str) -> String {
    format!("`mat --matd` does not support the `{name}` subcommand; run it without --matd (direct chip-tool path)")
}

/// matd へ 1 行送り 1 行受け取る。接続/送受信の失敗は detail 文字列で返す。
fn exchange(socket: &Path, op: &Value) -> Result<Value, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("could not connect to matd at {}: {e}", socket.display()))?;

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

    #[test]
    fn read_maps_to_read_op() {
        let cmd = Command::Read {
            node_id: 1,
            endpoint: 2,
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
            node_id: 3,
            endpoint: 1,
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({"op":"on","node_id":3,"endpoint":1})
        );
    }

    #[test]
    fn group_provision_fills_default_name_and_keeps_null_epoch() {
        let cmd = Command::Group {
            action: GroupCommand::Provision {
                group_id: 7,
                node_ids: vec![1, 2],
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
    fn resolve_socket_precedence() {
        let some_path = PathBuf::from("/x/y.sock");
        let dflt = default_socket_path();

        // --matd <path> が最優先。
        assert_eq!(
            resolve_socket(
                &Some(Some(some_path.clone())),
                Some("/env.sock".into()),
                None
            ),
            Some(some_path)
        );
        // --matd（値省略）→ 既定パス。
        assert_eq!(resolve_socket(&Some(None), None, None), Some(dflt.clone()));
        // フラグ無し + MAT_MATD_SOCKET → そのパス。
        assert_eq!(
            resolve_socket(&None, Some("/env.sock".into()), None),
            Some(PathBuf::from("/env.sock"))
        );
        // フラグ無し + MAT_MATD=1 → 既定パス。
        assert_eq!(
            resolve_socket(&None, None, Some("1".into())),
            Some(dflt.clone())
        );
        // MAT_MATD_SOCKET は MAT_MATD より優先。
        assert_eq!(
            resolve_socket(&None, Some("/env.sock".into()), Some("1".into())),
            Some(PathBuf::from("/env.sock"))
        );
        // 何も無し → 直経路（None）。空文字 socket / falsy enable も無効。
        assert_eq!(resolve_socket(&None, None, None), None);
        assert_eq!(
            resolve_socket(&None, Some("".into()), Some("0".into())),
            None
        );
    }

    #[test]
    fn discover_and_commission_are_unsupported() {
        assert!(to_op(&Command::Discover).is_err());
        assert!(to_op(&Command::Commission {
            target: "192.0.2.1".into(),
            setup_code: "MT:DUMMY".into(),
            node_id: None,
        })
        .is_err());
    }
}
