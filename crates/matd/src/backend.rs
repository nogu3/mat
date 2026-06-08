//! バックエンド: chip-tool を `interactive server`（websocket）で常駐起動し、
//! 温かい CASE セッションを保持したまま 1 本の ws 接続でコマンドを直列実行する。
//!
//! one-shot の `mat` が毎回 mDNS 解決 + CASE ハンドシェイクを払うのに対し、ここは
//! chip-tool プロセスと CASE セッションを生かしたまま使い回す（ssh の
//! `ControlMaster`/`ControlPersist` モデル）。プロトコルは直接喋らない — 駆動と
//! コマンド中継だけを担い、Matter の実体は chip-tool に委譲する（設計ルール 1）。

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use mat_core::error::{ErrorKind, MatError};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 1 コマンドの応答を待つ上限。実機では CASE 確立に数秒かかりうるので長め。
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// 子プロセスの ws ポートが開くまで待つ上限（chip-tool 初期化 + mDNS 起動分）。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// 常駐 chip-tool への接続。ws 1 本を Mutex で直列化する（chip-tool は単一接続で
/// コマンドを順次処理するため）。
pub struct ChipToolBackend {
    ws: Mutex<Ws>,
    /// 子プロセス。Drop で kill する（保持しないと孤児化する）。
    _child: Option<Child>,
}

impl ChipToolBackend {
    /// chip-tool を `interactive server` で起動し、ws が開くまで待って接続する。
    ///
    /// バイナリは `MAT_CHIP_TOOL_BIN` があればフルパス上書き、無ければ PATH の
    /// `chip-tool`（runner と同じ規約）。`store` は chip-tool の永続ストレージ。
    pub async fn spawn(store: &Path, port: u16) -> Result<Self, MatError> {
        let bin =
            std::env::var_os("MAT_CHIP_TOOL_BIN").unwrap_or_else(|| OsString::from("chip-tool"));

        tracing::info!(?bin, port, "spawning chip-tool interactive server");
        let child = Command::new(&bin)
            .arg("interactive")
            .arg("server")
            .arg("--port")
            .arg(port.to_string())
            .arg("--storage-directory")
            .arg(store)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    MatError::child_not_found(format!(
                        "chip-tool binary not found ({bin:?}); set MAT_CHIP_TOOL_BIN or add it to PATH"
                    ))
                } else {
                    MatError::new(ErrorKind::ChildFailed, format!("failed to spawn chip-tool: {e}"))
                }
            })?;

        let ws = connect_with_retry(port, STARTUP_TIMEOUT).await?;
        Ok(ChipToolBackend {
            ws: Mutex::new(ws),
            _child: Some(child),
        })
    }

    /// 既に動いている ws サーバへ接続する（テストや外部起動の chip-tool 用）。
    pub async fn connect(port: u16) -> Result<Self, MatError> {
        let ws = connect_with_retry(port, STARTUP_TIMEOUT).await?;
        Ok(ChipToolBackend {
            ws: Mutex::new(ws),
            _child: None,
        })
    }

    /// コマンド行を送り、最初に返る Text メッセージ（= 実行結果 JSON）を返す。
    ///
    /// chip-tool ws はコマンド完了時に結果メッセージを 1 つ返す。Ping/Pong など
    /// 制御フレームは読み飛ばす。応答が JSON でなければ [`ErrorKind::ParseError`]。
    pub async fn run_cmdline(&self, line: &str) -> Result<Value, MatError> {
        let mut ws = self.ws.lock().await;

        ws.send(Message::Text(line.to_string()))
            .await
            .map_err(|e| MatError::new(ErrorKind::ChildFailed, format!("ws send failed: {e}")))?;

        let recv = tokio::time::timeout(COMMAND_TIMEOUT, next_text(&mut ws));
        let text = match recv.await {
            Ok(r) => r?,
            Err(_) => {
                return Err(MatError::new(
                    ErrorKind::Timeout,
                    format!("no response from chip-tool within {COMMAND_TIMEOUT:?} for: {line}"),
                ))
            }
        };

        serde_json::from_str(&text).map_err(|e| {
            MatError::parse_error(format!(
                "chip-tool ws response was not JSON: {e}; body={text}"
            ))
        })
    }
}

/// 次の Text メッセージを読む。制御フレームは読み飛ばし、Close/切断は ChildFailed。
async fn next_text(ws: &mut Ws) -> Result<String, MatError> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Ok(t),
            Some(Ok(Message::Binary(b))) => {
                return String::from_utf8(b).map_err(|e| {
                    MatError::parse_error(format!("ws binary response was not utf-8: {e}"))
                })
            }
            // 制御フレームは無視して次を待つ。
            Some(Ok(Message::Ping(_)))
            | Some(Ok(Message::Pong(_)))
            | Some(Ok(Message::Frame(_))) => continue,
            Some(Ok(Message::Close(_))) | None => {
                return Err(MatError::new(
                    ErrorKind::ChildFailed,
                    "chip-tool ws closed before responding".to_string(),
                ))
            }
            Some(Err(e)) => {
                return Err(MatError::new(
                    ErrorKind::ChildFailed,
                    format!("ws receive failed: {e}"),
                ))
            }
        }
    }
}

/// ws が開くまで一定間隔で接続を試みる。
async fn connect_with_retry(port: u16, within: Duration) -> Result<Ws, MatError> {
    let url = format!("ws://127.0.0.1:{port}/");
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match connect_async(&url).await {
            Ok((ws, _resp)) => {
                tracing::info!(port, "connected to chip-tool ws");
                return Ok(ws);
            }
            Err(e) => {
                // まだ立ち上がっていないだけかもしれない。期限超過のときだけ諦める。
                if tokio::time::Instant::now() >= deadline {
                    return Err(MatError::new(
                        ErrorKind::ChildFailed,
                        format!(
                            "chip-tool ws on port {port} did not come up within {within:?}: {e}"
                        ),
                    ));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}
