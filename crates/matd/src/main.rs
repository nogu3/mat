//! `matd` — Matter の常駐レイヤ（Phase 4）。
//!
//! chip-tool を `interactive server`（websocket）で常駐起動し、温かい CASE
//! セッションを保持したまま unix socket で read/invoke 等を中継する。各呼び出しが
//! mDNS 解決 + CASE ハンドシェイクを払う one-shot の `mat` に対し、ハンドシェイクを
//! 省いて高速化する（ssh `ControlMaster`/`ControlPersist` モデル）。クロスプロトコルの
//! `casad` とは別物で、Matter 専用。設計は ARCHITECTURE.md を参照。
//!
//! `mat` 本体の設計ルール 4（常駐・セッションキャッシュ禁止）は `mat` に効き続ける。
//! `matd` は別バイナリ・別レイヤなので常駐してよい。

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use mat_core::error::{ErrorKind, MatError};
use mat_core::store::Store;

use matd::backend::ChipToolBackend;
use matd::server;

/// matd — warm CASE sessions for Matter over a local unix socket.
#[derive(Parser, Debug)]
#[command(name = "matd", version)]
struct Cli {
    /// 認証情報ストア（KVS）。未指定なら MAT_STORE / XDG_CONFIG_HOME / ~/.config/mat。
    #[arg(long)]
    store: Option<PathBuf>,

    /// 上流 unix socket のパス。未指定なら $XDG_RUNTIME_DIR/matd.sock（無ければ /tmp）。
    #[arg(long)]
    socket: Option<PathBuf>,

    /// chip-tool interactive server の ws ポート。
    #[arg(long, default_value_t = 9100)]
    port: u16,

    /// 子プロセスを起動せず、既に動いている chip-tool ws（--port）へ接続する。
    #[arg(long)]
    connect: bool,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            MatError::new(
                ErrorKind::Other,
                format!("failed to start tokio runtime: {e}"),
            )
            .emit();
            std::process::exit(ErrorKind::Other.exit_code() as i32);
        }
    };

    if let Err(e) = runtime.block_on(run(Cli::parse())) {
        e.emit();
        std::process::exit(e.kind.exit_code() as i32);
    }
}

async fn run(cli: Cli) -> Result<(), MatError> {
    let store_path = Store::locate(cli.store);
    // 認証情報必須レイヤ。ストアが無ければ早めに exit 10（read/invoke は通せない）。
    Store::open(&store_path)?;

    let backend = if cli.connect {
        ChipToolBackend::connect(cli.port).await?
    } else {
        ChipToolBackend::spawn(&store_path, cli.port).await?
    };

    let socket = cli.socket.unwrap_or_else(default_socket);
    server::serve(&socket, store_path, Arc::new(backend))
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("socket server failed: {e}")))
}

/// 既定のソケットパス: `$XDG_RUNTIME_DIR/matd.sock`、無ければ `/tmp/matd.sock`。
fn default_socket() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("matd.sock")
    } else {
        PathBuf::from("/tmp/matd.sock")
    }
}
