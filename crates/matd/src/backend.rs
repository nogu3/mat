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
use tokio::io::{AsyncRead, AsyncWrite};
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
    /// chip-tool ws はコマンド完了時に結果メッセージを 1 つ返す。送信自体の失敗は
    /// 「ソケットが送信前から死んでいた」ことを意味する（chip-tool には届いていない）
    /// ので、ws だけ張り直して 1 回だけ透過リトライする — 子プロセスは温存し warm CASE
    /// セッションを守る（issue #7）。
    pub async fn run_cmdline(&self, line: &str) -> Result<Value, MatError> {
        let mut conn = self.conn.lock().await;
        self.ensure_connected(&mut conn).await?;

        let mut result = exchange(conn.ws.as_mut().expect("ensured above"), line).await;

        if let Err(ExchangeError::Send(e)) = &result {
            tracing::info!(error = %e.detail, "ws send failed; reconnecting and retrying once");
            conn.ws = None;
            self.ensure_connected(&mut conn).await?;
            result = exchange(conn.ws.as_mut().expect("ensured above"), line).await;
        }

        match result {
            Ok(mut value) => {
                conn.last_used = Instant::now();
                drop_logs(&mut value);
                Ok(value)
            }
            Err(e) => {
                // 接続が壊れた可能性。畳んで次回フル再確立に委ねる。
                // （Task 2 で受信失敗の温存経路に置き換える）
                teardown(&mut conn).await;
                Err(e.into_mat())
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

/// chip-tool ws 応答の `logs`（base64 でくるんだ chip-tool テキストログ。冗長）を
/// 応答から落とす。CLAUDE.md ルール 2「素通し禁止」・ルール 3「診断は debug ログ」に
/// 従い、件数だけ debug に残して構造化結果（`results`）のみ上流へ返す。
///
/// ただし `results[i].error` の汎用 `FAILURE` だけでは timeout/unreachable を判別
/// できず、その分類シグナル（例 discovery timeout の `CHIP Error 0x00000032`）は
/// この logs にしか出ない（one-shot 経路は chip-tool の stdout/stderr 全体を分類器に
/// 通すので拾える）。経路で分類が割れるのを防ぐため（#1）、落とす前に logs を
/// デコードして 1 本のテキストにまとめ、分類専用フィールド `diag` として残す。`diag`
/// は [`super::server::ensure_ok`] の分類入力にのみ使い、上流の mat スキーマ応答
/// （server が組み直す body）には載らない。
fn drop_logs(value: &mut Value) {
    if let Value::Object(map) = value {
        if let Some(logs) = map.remove("logs") {
            let count = logs.as_array().map(|a| a.len()).unwrap_or(0);
            tracing::debug!(count, "dropped chip-tool ws logs from response");
            let diag = decode_logs(&logs);
            if !diag.is_empty() {
                map.insert("diag".into(), Value::String(diag));
            }
        }
    }
}

/// ws `logs` 配列を分類用の 1 本のテキストへデコードする。
///
/// 実機 chip-tool のエントリは base64 文字列（または `{module,category,message}` で
/// `message` が base64）。base64 として解せない／非 UTF-8 のエントリは生文字列を
/// そのまま使う（古い fixture や非 base64 ログにも耐える）。
fn decode_logs(logs: &Value) -> String {
    use base64::Engine as _;
    let Some(arr) = logs.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for entry in arr {
        let raw = match entry {
            Value::String(s) => s.as_str(),
            Value::Object(_) => entry.get("message").and_then(Value::as_str).unwrap_or(""),
            _ => "",
        };
        if raw.is_empty() {
            continue;
        }
        let line = match base64::engine::general_purpose::STANDARD.decode(raw) {
            Ok(bytes) => String::from_utf8(bytes).unwrap_or_else(|_| raw.to_string()),
            Err(_) => raw.to_string(),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// exchange の失敗を送信/受信で区別する。送信失敗はコマンドが chip-tool に届いて
/// いないことが確定しているので安全に再試行できる。送信後の失敗（timeout・切断・
/// parse）はコマンドが実行された可能性を排除できない（toggle 等は再送で二重実行に
/// なる）。
enum ExchangeError {
    /// 送信自体が失敗した（chip-tool には届いていない）。
    Send(MatError),
    /// 送信後に失敗した（実行された可能性がある）。
    AfterSend(MatError),
}

impl ExchangeError {
    fn into_mat(self) -> MatError {
        match self {
            ExchangeError::Send(e) | ExchangeError::AfterSend(e) => e,
        }
    }
}

/// 確立済みの ws で 1 往復する。
async fn exchange<S>(ws: &mut WebSocketStream<S>, line: &str) -> Result<Value, ExchangeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    ws.send(Message::Text(line.to_string()))
        .await
        .map_err(|e| {
            ExchangeError::Send(MatError::new(
                ErrorKind::ChildFailed,
                format!("ws send failed: {e}"),
            ))
        })?;

    let text = match tokio::time::timeout(COMMAND_TIMEOUT, next_text(ws)).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return Err(ExchangeError::AfterSend(e)),
        Err(_) => {
            return Err(ExchangeError::AfterSend(MatError::new(
                ErrorKind::Timeout,
                format!("no response from chip-tool within {COMMAND_TIMEOUT:?} for: {line}"),
            )))
        }
    };

    // 生 ws 応答（results / 失敗 error の実形状）を debug に残す。診断のみ stderr
    // （CLAUDE.md ルール 3）。失敗時 `results[i].error` の形状はこのログで実機確定済み:
    // `{"results":[{"error":"FAILURE"}],"logs":[...]}` ― `error` は status 名の
    // **文字列**（数値ではない）。[`super::server::ensure_ok`] がこれを分類する。
    tracing::debug!(%text, "chip-tool ws raw response");

    serde_json::from_str(&text).map_err(|e| {
        ExchangeError::AfterSend(MatError::parse_error(format!(
            "chip-tool ws response was not JSON: {e}; body={text}"
        )))
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
async fn next_text<S>(ws: &mut WebSocketStream<S>) -> Result<String, MatError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::json;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[test]
    fn drop_logs_removes_logs_and_extracts_decoded_diag() {
        // 実機 chip-tool interactive server の logs は base64 でくるんだテキストログ。
        // discovery timeout シグナルはここにしか出ない（#1）。落とす前にデコードして
        // 分類用の `diag` として残し、`logs` 自体は応答から消す。
        let mut v = json!({
            "results": [{ "error": "FAILURE" }],
            "logs": [
                b64("[DIS] Timeout waiting for mDNS resolution."),
                b64("[DIS] operational discovery failed: CHIP Error 0x00000032: Timeout"),
            ],
        });
        drop_logs(&mut v);
        assert!(v.get("logs").is_none(), "verbose logs must be removed");
        let diag = v
            .get("diag")
            .and_then(Value::as_str)
            .expect("diag attached");
        assert!(
            diag.contains("0x00000032") && diag.contains("Timeout waiting for mDNS"),
            "decoded diag should carry discovery timeout signal, got: {diag}"
        );
    }

    #[test]
    fn drop_logs_tolerates_non_base64_string_entries() {
        // base64 として解せない素の文字列エントリ（古い fixture 等）でも落ちない。
        let mut v = json!({ "results": [], "logs": ["dis9hcnt"] });
        drop_logs(&mut v);
        assert!(v.get("logs").is_none());
    }

    #[tokio::test]
    async fn exchange_classifies_transport_write_failure_as_send() {
        use tokio_tungstenite::tungstenite::protocol::Role;
        // 相手側を先に落とした in-memory ストリーム → write が確実に失敗する。
        let (client_io, server_io) = tokio::io::duplex(1024);
        drop(server_io);
        let mut ws = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;

        let result = exchange(&mut ws, "onoff on 5 1").await;
        let Err(ExchangeError::Send(e)) = result else {
            panic!("expected ExchangeError::Send for transport write failure");
        };
        assert_eq!(e.kind, ErrorKind::ChildFailed);
        assert!(e.detail.contains("ws send failed"), "got: {}", e.detail);
    }

    #[test]
    fn drop_logs_handles_object_message_entries() {
        // logs エントリがオブジェクト {module,category,message(base64)} 形式でも message を拾う。
        let mut v = json!({
            "results": [{ "error": "FAILURE" }],
            "logs": [{ "module": "DIS", "category": "Error",
                       "message": b64("CHIP Error 0x00000032: Timeout") }],
        });
        drop_logs(&mut v);
        let diag = v
            .get("diag")
            .and_then(Value::as_str)
            .expect("diag attached");
        assert!(diag.contains("0x00000032"), "got: {diag}");
    }
}
