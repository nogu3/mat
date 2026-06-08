//! バックエンド: chip-tool を `interactive server`（websocket）で常駐起動し、
//! 温かい CASE セッションを保持したまま 1 本の ws 接続でコマンドを直列実行する。
//!
//! one-shot の `mat` が毎回 mDNS 解決 + CASE ハンドシェイクを払うのに対し、ここは
//! chip-tool プロセスと CASE セッションを生かしたまま使い回す（ssh の
//! `ControlMaster`/`ControlPersist` モデル）。プロトコルは直接喋らない — 駆動と
//! コマンド中継だけを担い、Matter の実体は chip-tool に委譲する（設計ルール 1）。
//!
//! `ControlPersist` と同じく、一定時間アイドルだとセッションを畳む（[`reap_if_idle`]）。
//! 畳んだ後は次のコマンドで遅延再確立する。`Spawn` モードは子プロセスごと落として
//! 起こし直し、`Connect` モード（外部 chip-tool）は ws を張り直すだけ。
//!
//! [`reap_if_idle`]: ChipToolBackend::reap_if_idle

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use mat_core::error::{ErrorKind, MatError};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// 1 コマンドの応答を待つ上限。実機では CASE 確立に数秒かかりうるので長め。
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// 子プロセスの ws ポートが開くまで待つ上限（chip-tool 初期化 + mDNS 起動分）。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// 接続の張り方。`ControlPersist` 的なアイドル畳み込みの後、再確立の手段が異なる。
enum Mode {
    /// 子プロセスを起こして繋ぐ。アイドル畳み込み後は起こし直す。
    Spawn { store: PathBuf, port: u16 },
    /// 既存の外部 chip-tool に繋ぐだけ。畳み込み後は ws を張り直す。
    Connect { port: u16 },
}

/// 現在の接続状態。`ws` が `None` なら未確立（遅延確立される）。
struct Conn {
    ws: Option<Ws>,
    child: Option<Child>,
    last_used: Instant,
}

/// 常駐 chip-tool への接続。ws 1 本を Mutex で直列化する（chip-tool は単一接続で
/// コマンドを順次処理するため）。
pub struct ChipToolBackend {
    mode: Mode,
    idle: Duration,
    conn: Mutex<Conn>,
}

impl ChipToolBackend {
    /// chip-tool を `interactive server` で起動して繋ぐ。`idle` 無アクセスでセッションを
    /// 畳む。起動時に一度確立してエラーを早期検出する。
    pub async fn spawn(store: &Path, port: u16, idle: Duration) -> Result<Self, MatError> {
        Self::new(
            Mode::Spawn {
                store: store.to_path_buf(),
                port,
            },
            idle,
        )
        .await
    }

    /// 既に動いている ws サーバへ接続する（テストや外部起動の chip-tool 用）。
    pub async fn connect(port: u16, idle: Duration) -> Result<Self, MatError> {
        Self::new(Mode::Connect { port }, idle).await
    }

    async fn new(mode: Mode, idle: Duration) -> Result<Self, MatError> {
        let backend = ChipToolBackend {
            mode,
            idle,
            conn: Mutex::new(Conn {
                ws: None,
                child: None,
                last_used: Instant::now(),
            }),
        };
        // 早期接続。失敗すれば matd 起動を失敗させる。
        let mut conn = backend.conn.lock().await;
        backend.ensure_connected(&mut conn).await?;
        drop(conn);
        Ok(backend)
    }

    /// アイドル畳み込みの基準時間。reaper の周期決めに使う。
    pub fn idle(&self) -> Duration {
        self.idle
    }

    /// コマンド行を送り、最初に返る Text メッセージ（= 実行結果 JSON）を返す。
    ///
    /// chip-tool ws はコマンド完了時に結果メッセージを 1 つ返す。通信に失敗したら
    /// 壊れた接続を畳み、次回コマンドで再確立する（遅延応答の混線を断つ）。
    pub async fn run_cmdline(&self, line: &str) -> Result<Value, MatError> {
        let mut conn = self.conn.lock().await;
        self.ensure_connected(&mut conn).await?;

        let result = exchange(conn.ws.as_mut().expect("ensured above"), line).await;
        match result {
            Ok(value) => {
                conn.last_used = Instant::now();
                Ok(value)
            }
            Err(e) => {
                // 接続が壊れた可能性。畳んで次回フル再確立に委ねる。
                teardown(&mut conn).await;
                Err(e)
            }
        }
    }

    /// アイドルが `idle` を超えていればセッションを畳む。reaper から定期的に呼ぶ。
    pub async fn reap_if_idle(&self) {
        let mut conn = self.conn.lock().await;
        if conn.ws.is_some() && conn.last_used.elapsed() >= self.idle {
            tracing::info!(idle = ?self.idle, "tearing down idle chip-tool session");
            teardown(&mut conn).await;
        }
    }

    /// セッションを畳む（ctrl_c 後のクリーンアップ用）。
    pub async fn shutdown(&self) {
        let mut conn = self.conn.lock().await;
        teardown(&mut conn).await;
    }

    /// ws 未確立なら確立する。Spawn は子が無ければ起こしてから繋ぐ。
    async fn ensure_connected(&self, conn: &mut Conn) -> Result<(), MatError> {
        if conn.ws.is_some() {
            return Ok(());
        }
        let port = match &self.mode {
            Mode::Spawn { store, port } => {
                if conn.child.is_none() {
                    conn.child = Some(spawn_child(store, *port)?);
                }
                *port
            }
            Mode::Connect { port } => *port,
        };
        let ws = connect_with_retry(port, STARTUP_TIMEOUT).await?;
        conn.ws = Some(ws);
        Ok(())
    }
}

/// 確立済みの ws で 1 往復する。
async fn exchange(ws: &mut Ws, line: &str) -> Result<Value, MatError> {
    ws.send(Message::Text(line.to_string()))
        .await
        .map_err(|e| MatError::new(ErrorKind::ChildFailed, format!("ws send failed: {e}")))?;

    let text = match tokio::time::timeout(COMMAND_TIMEOUT, next_text(ws)).await {
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

/// セッションを畳む。ws を閉じ、子プロセスがあれば落として待つ。
async fn teardown(conn: &mut Conn) {
    conn.ws = None; // Drop で close フレーム送出。
    if let Some(mut child) = conn.child.take() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// chip-tool を `interactive server` で起動する。
fn spawn_child(store: &Path, port: u16) -> Result<Child, MatError> {
    let bin = std::env::var_os("MAT_CHIP_TOOL_BIN").unwrap_or_else(|| OsString::from("chip-tool"));
    tracing::info!(?bin, port, "spawning chip-tool interactive server");
    Command::new(&bin)
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
                MatError::new(
                    ErrorKind::ChildFailed,
                    format!("failed to spawn chip-tool: {e}"),
                )
            }
        })
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
    let deadline = Instant::now() + within;
    loop {
        match connect_async(&url).await {
            Ok((ws, _resp)) => {
                tracing::info!(port, "connected to chip-tool ws");
                return Ok(ws);
            }
            Err(e) => {
                // まだ立ち上がっていないだけかもしれない。期限超過のときだけ諦める。
                if Instant::now() >= deadline {
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
