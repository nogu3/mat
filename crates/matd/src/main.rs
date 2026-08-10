//! `matd` — Matter の常駐レイヤ（Phase 4、M8c-3 で native 一本化）。
//!
//! 埋め込みの native Matter コントローラ（`mat-controller`/`mat-native`）で
//! 温かい CASE セッションを保持したまま unix socket で read/invoke 等を中継する。
//! 各呼び出しが mDNS 解決 + CASE ハンドシェイクを払う one-shot の `mat` に対し、
//! ハンドシェイクを省いて高速化する（ssh `ControlMaster`/`ControlPersist`
//! モデル）。Matter 専用。設計は ARCHITECTURE.md を参照。
//!
//! M8c-3: chip-tool 経路を完全撤去した。native backend の構築に失敗しても
//! （KVS 資材が読めない等）matd は起動を続け、以後のリクエストへ構築エラーを
//! per-op で返す（[`matd::server::NativeState::Unavailable`]）——
//! `mat fabric init` で資材を用意すれば再起動で解消できる。
//!
//! `mat` 本体の設計ルール 4（常駐・セッションキャッシュ禁止）は `mat` に効き続ける。
//! `matd` は別バイナリ・別レイヤなので常駐してよい。

use std::path::Path;
use std::path::PathBuf;

use clap::Parser;
use serde_json::Value;

use mat_core::error::{ErrorKind, MatError};
use mat_core::store::Store;

use matd::server;

/// matd — warm CASE sessions for Matter over a local unix socket.
#[derive(Parser, Debug)]
#[command(name = "matd", version)]
struct Cli {
    /// 認証情報ストア（KVS）。未指定なら MAT_STORE / XDG_CONFIG_HOME / ~/.config/mat。
    #[arg(long)]
    store: Option<PathBuf>,

    /// 上流 unix socket のパス。未指定なら $XDG_RUNTIME_DIR/matd/matd.sock
    /// （dir は 0700 で自動作成。XDG 不在なら /tmp/matd.sock）。serve / stop 両方が使う。
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// native warm session に使う Thread mesh の iface 名。未指定なら自動検出
    /// （M8c-3 native 既定化。曖昧なら起動拒否）。
    #[arg(long, env = "MAT_MATD_IFACE")]
    iface: Option<String>,

    /// groupcast を追加送出する Thread TUN の iface 名（例: wpan0）。未設定なら
    /// `wpan*` がちょうど 1 本あるとき自動採用。明示指定の解決失敗は以後の
    /// 全 op がエラーになる（matd status で確認可。起動そのものは拒否しない）。
    #[arg(long, env = "MAT_MATD_THREAD_IFACE")]
    thread_iface: Option<String>,

    /// KVS fabric テーブルの index（jarvis 本番は 2、alpha は 1）。
    #[arg(long, env = "MAT_MATD_FABRIC_INDEX", default_value_t = 1)]
    fabric_index: u8,

    /// CA issuer index。
    #[arg(long, env = "MAT_MATD_ISSUER_INDEX", default_value_t = 0)]
    issuer_index: u8,

    #[command(subcommand)]
    command: Option<Command>,
}

/// matd のサブコマンド。無指定は serve（従来どおり）。
#[derive(clap::Subcommand, Debug)]
enum Command {
    /// 稼働中の matd を停止する（socket 経由で graceful shutdown）。
    Stop,
    /// 稼働中 matd の購読とデーモンの現況を JSON で返す（socket 経由）。
    Status,
}

fn main() {
    // レベルは mat 本体と同じく `MAT_LOG`（無ければ `RUST_LOG`）で制御。
    // 既定は info（常駐デーモンなので状態遷移は既定で残す）。空文字は
    // 未設定扱い、パースできない指定は次の候補へ送る（`mat_core::log` 参照）。
    let filter = mat_core::log::log_filter_candidates_from_env()
        .into_iter()
        .find_map(|s| tracing_subscriber::EnvFilter::try_new(&s).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // デーモンなので ANSI は常に無効。tracing-subscriber は tty 判定を
        // せず（`is_ansi = cfg!(feature = "ansi") && NO_COLOR 未設定`）、
        // 既定では journald に `^[[3mnode_id^[[0m^[[2m=^[[0m42` を書いて
        // しまい `grep node_id=42` が空振りする。
        .with_ansi(false)
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
    match cli.command {
        Some(Command::Stop) => admin_op(cli.socket, "shutdown").await,
        Some(Command::Status) => admin_op(cli.socket, "status").await,
        None => serve_daemon(cli).await,
    }
}

/// serve: 単一インスタンスロックを取ってから native backend を構築し、socket を bind する。
async fn serve_daemon(cli: Cli) -> Result<(), MatError> {
    // 既定パス（$XDG_RUNTIME_DIR/matd/matd.sock）のときだけ親 dir を 0700 で
    // 用意する。明示 --socket の親不在は従来どおり bind エラーに任せる。
    // lock ファイル（<socket>.lock）も同 dir なので acquire より前に作る。
    let socket = match cli.socket.clone() {
        Some(p) => p,
        None => {
            let p = mat_core::socket::default_socket_path();
            mat_core::socket::ensure_socket_dir(&p).map_err(|e| {
                MatError::new(
                    ErrorKind::Other,
                    format!("failed to create socket dir for {}: {e}", p.display()),
                )
            })?;
            p
        }
    };

    // 二重起動ガード。socket bind より前に取る。_lock はプロセス生存中保持する
    // （Drop でロック解放）。
    let _lock = matd::lock::acquire(&socket)?;

    let store_path = Store::locate(cli.store);
    // M8c-3: KVS 不在でも matd は起動する（後から `mat fabric init` できるよう、
    // per-op store_missing で応答する — 下の native 構築失敗の扱いを参照）。
    // ここで早期 exit 10 にはしない。

    // native warm session バックエンド。iface は env / --iface、未設定なら自動
    // 検出（M8c-3 native 既定化）。自動検出の候補 0 / 複数は起動拒否 —
    // 全 op が死ぬ設定不備なので per-op エラーではなく fail-fast にする
    // （jarvis の systemd unit は env 設定済みで影響なし）。
    let iface: String = match &cli.iface {
        Some(i) => i.clone(),
        None => match mat_native::iface_select::autodetect() {
            Ok(i) => {
                tracing::info!(iface = %i, "iface auto-selected (matd native default)");
                i
            }
            Err(e) => {
                e.emit();
                std::process::exit(e.kind.exit_code() as i32);
            }
        },
    };

    // groupcast の Thread TUN 追加送出先: 明示指定を優先、未設定なら wpan* を
    // 自動検出（失敗は None のまま — LAN 単独送出。mat 本体 main.rs と同じ流儀）。
    let thread_iface = match &cli.thread_iface {
        Some(n) => Some(mat_native::ThreadIfaceChoice::Explicit(n.clone())),
        None => mat_native::iface_select::detect_thread_iface_auto().map(|n| {
            tracing::info!(iface = %n, "thread iface auto-detected (matd groupcast egress)");
            mat_native::ThreadIfaceChoice::Auto(n)
        }),
    };

    // native 構築失敗（KVS 資材が読めない等）は致命にしない。matd は起動を続け、
    // 以後の全リクエストへこの構築エラーをそのまま返す（M8c-3: chip-tool
    // フォールバックが撤去されたため、per-op ハードエラーで運転する）。thread_iface
    // 明示指定の解決失敗（Engine::build がハードエラーを返す、Task 3）もこの
    // 既存の分岐に合流する — 追加コードは不要（Step 2 確認事項、詳細は下記）。
    let cfg = matd::native::NativeConfig {
        store: store_path.clone(),
        iface: iface.clone(),
        thread_iface: thread_iface.clone(),
        fabric_index: cli.fabric_index,
        issuer_index: cli.issuer_index,
    };
    // 常駐 mDNS キャッシュ（周期アナウンス依存の弱リンク Thread ノードの
    // establish を確実化）。bind 失敗時は OneShotResolver に degrade。
    let resolver: std::sync::Arc<dyn mat_native::Resolver> =
        match mat_controller::dnssd::iface_index(&cfg.iface) {
            Ok(scope_id) => match mat_controller::dnssd::spawn_operational_cache(scope_id) {
                Ok(cache) => {
                    tracing::info!("matd: resident mDNS operational cache enabled");
                    std::sync::Arc::new(mat_native::CachingResolver::new(cache))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "matd: mDNS cache bind failed; using one-shot resolver");
                    std::sync::Arc::new(mat_native::OneShotResolver)
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "matd: iface index unresolved; using one-shot resolver");
                std::sync::Arc::new(mat_native::OneShotResolver)
            }
        };

    let native = match matd::native::NativeBackend::build_with_resolver(&cfg, resolver).await {
        Ok(b) => {
            tracing::info!(%iface, thread_iface = ?thread_iface, fabric_index = cli.fabric_index, "native backend enabled");
            server::NativeState::Ready(Box::new(b))
        }
        Err(mut e) => {
            // mat 側の map_engine_build_error（crates/mat/src/native_direct.rs）と
            // 同様に `mat fabric init` への誘導を detail に足す（二重付与は避ける）。
            // store_missing の場合のみ誘導を追加（他の kind は事象特有）。
            if e.kind == mat_core::error::ErrorKind::StoreMissing
                && !e.detail.contains("mat fabric init")
            {
                e.detail = format!(
                    "{} — run `mat fabric init` to bootstrap the credential store",
                    e.detail
                );
            }
            tracing::warn!(
                error = %e.detail,
                "native backend build failed; matd will start but every op will \
                 error until this is resolved (e.g. `mat fabric init`)"
            );
            server::NativeState::Unavailable(e)
        }
    };

    let native = std::sync::Arc::new(native);
    // listen へのイベント配信路。購読 → broadcast → listen 接続（spec ②）。
    // 初期 receiver は即 drop する（status の listen_clients を 1 過大にしない
    // — 受信者ゼロの send エラーは購読側で正常扱い済み）。
    let (events_tx, initial_rx) = tokio::sync::broadcast::channel(1024);
    drop(initial_rx);
    // 常駐購読のクラスタ絞り込み（subscriptions.toml、無し = full wildcard）。
    // 設定不備は fail-fast: 黙って wildcard に落ちると弱リンク対策が無効化
    // されたことに気づけない（ambiguous iface autodetect と同じ規律）。
    let sub_clusters = match matd::subscribe_config::load(&store_path) {
        Ok(c) => c,
        Err(e) => {
            e.emit();
            std::process::exit(e.kind.exit_code() as i32);
        }
    };
    if let Some(c) = &sub_clusters {
        tracing::info!(
            clusters = c.len(),
            "subscriptions.toml loaded; narrowing resident subscribe paths"
        );
    }
    // op 相関ヘルス表: server（note_op）と購読 pump（判定）の共有。
    let sub_health = std::sync::Arc::new(matd::subscription::SubHealth::new(sub_clusters.clone()));
    // 常駐購読は native が使えるときだけ張る（Unavailable なら listen は
    // ack だけ返り、イベントは流れない — `mat fabric init` 後の再起動で解消）。
    // supervisor は 60s 周期で台帳を再読し、稼働中に commission された
    // ノードにも購読を張る（監査#4）。
    let _sub_handle = matd::subscription::spawn_subscription_manager(
        std::sync::Arc::clone(&native),
        store_path.clone(),
        events_tx.clone(),
        sub_clusters,
        std::sync::Arc::clone(&sub_health),
    );

    // 内部トリガ（Issue #20 経路2）: native が新規 CASE セッションを確立する
    // たび（cold establish / resend-establish）に SubHealth へ即時張り直しを
    // 合図する。native.rs は subscription.rs を知らない汎用コールバックなので、
    // 注入は組み立てが揃うここ（sub_health 構築後・server::serve より前）で行う。
    if let server::NativeState::Ready(b) = native.as_ref() {
        let sub_health_for_hint = std::sync::Arc::clone(&sub_health);
        b.set_on_new_session(Box::new(move |n| sub_health_for_hint.note_touched(n)));
    }

    let daemon = std::sync::Arc::new(server::DaemonInfo {
        version: env!("CARGO_PKG_VERSION"),
        started: std::time::Instant::now(),
        iface: iface.clone(),
        fabric_index: cli.fabric_index,
    });
    server::serve(&socket, store_path, native, events_tx, sub_health, daemon)
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("socket server failed: {e}")))
}

/// stop / status: 稼働中 matd の socket へ admin op を 1 行送り、応答 JSON を
/// stdout へ出す。居なければ「not running」で exit 1。
async fn admin_op(socket: Option<PathBuf>, op: &str) -> Result<(), MatError> {
    let socket = socket.unwrap_or_else(mat_core::socket::default_socket_path);
    let resp = send_admin_op(&socket, op).await?;
    // 成功応答は stdout（純粋 JSON）。
    println!("{resp}");
    Ok(())
}

/// socket に `{"op":"<op>"}` を送り応答 1 行を読む。接続不能は「not running」
/// （応答なし拒否 = stale socket はどの op でも掃除してよい）。
async fn send_admin_op(socket: &Path, op: &str) -> Result<Value, MatError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(socket).await.map_err(|e| {
        // 応答なしで拒否 = stale socket が残っているだけのことがある。掃除する。
        if e.kind() == std::io::ErrorKind::ConnectionRefused {
            let _ = std::fs::remove_file(socket);
        }
        MatError::new(
            ErrorKind::Other,
            format!("matd not running at {} ({e})", socket.display()),
        )
    })?;

    let (read_half, mut write_half) = stream.into_split();
    let mut line = serde_json::to_vec(&serde_json::json!({ "op": op })).unwrap();
    line.push(b'\n');
    write_half
        .write_all(&line)
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to send {op}: {e}")))?;

    let mut lines = BufReader::new(read_half).lines();
    let line = lines
        .next_line()
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to read response: {e}")))?
        .ok_or_else(|| {
            MatError::new(
                ErrorKind::Other,
                "matd closed the connection without responding".to_string(),
            )
        })?;
    serde_json::from_str(&line)
        .map_err(|e| MatError::parse_error(format!("matd response was not JSON: {e}; body={line}")))
}
