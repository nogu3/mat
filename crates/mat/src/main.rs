//! `mat` — Matter デバイス操作 CLI。native バックエンド（`mat-controller` /
//! `mat-native`）で直接 Matter を喋り、結果を `mat` のスキーマで返す（M8c-3 で
//! chip-tool 経路は撤去）。
//!
//! stdout は純粋な構造化 JSON のみ。診断は stderr に構造化ログ（`tracing`）。
//! 認証情報 KVS 以外の永続状態は持たない。

mod cli;
mod commands;
mod matd_client;
mod native_direct;
mod probe;
mod resolve;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::{fmt, EnvFilter};

use cli::{Cli, Command, DiagCommand, FabricAction};
use mat_core::error::ErrorKind;
use mat_core::store::Store;

fn main() -> ExitCode {
    init_tracing();

    // 引数エラー（exit 2）は clap が直接処理する。
    let args = Cli::parse();

    let store_path = Store::locate(args.store);

    // alias 一括解決（aliases.toml が無ければ数値パススルー）。matd 経路も数値しか
    // 受けないため、経路解決より前に行う。未知 alias / 不正 alias 名は CLI 引数
    // エラー（exit 2）、壊れた aliases.toml は store_parse（exit 10）。
    let command = match resolve::resolve_command(args.command, &store_path) {
        Ok(c) => c,
        Err(e) => {
            e.emit();
            return match e.kind {
                ErrorKind::StoreParse => ExitCode::from(e.kind.exit_code()),
                _ => ExitCode::from(2),
            };
        }
    };

    // fabric bootstrap（M8c-3）: ネットワーク・backend 未接触のローカル完結処理
    // なので、iface 自動検出（下のブロック）にも matd 経路にも巻き込まない —
    // ここで最優先 dispatch する。
    if let Command::Fabric { action } = &command {
        let FabricAction::Init {
            fabric_id,
            admin_node_id,
        } = action;
        return match commands::fabric::run_init(
            &store_path,
            *fabric_id,
            *admin_node_id,
            args.fabric_index,
            args.issuer_index,
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                e.emit();
                ExitCode::from(e.kind.exit_code())
            }
        };
    }

    // listen は初の matd 専用 op（direct fallback なし — 常駐なしに購読は成立
    // しない）。経路解決の socket だけ流用し、Direct（MAT_MATD=falsy）は
    // matd_unavailable で即エラー。
    if let Command::Listen { .. } = &command {
        return match matd_client::resolve_route(
            &args.matd,
            std::env::var_os("MAT_MATD_SOCKET"),
            std::env::var_os("MAT_MATD"),
        ) {
            matd_client::Route::Forced(sockets) | matd_client::Route::Auto(sockets) => {
                matd_client::dispatch_listen(&sockets, &command)
            }
            matd_client::Route::Direct => {
                mat_core::error::MatError::new(
                    ErrorKind::MatdUnavailable,
                    "`mat listen` requires matd (MAT_MATD=0 disables it)",
                )
                .emit();
                ExitCode::from(ErrorKind::MatdUnavailable.exit_code())
            }
        };
    }

    // 経路解決（matd_client::resolve_route）: --matd / MAT_MATD=truthy は強制 matd、
    // MAT_MATD=falsy は強制直、どちらも無ければ自動検出（connect 成功時のみ matd 経由、
    // 失敗時と非対応 op は下の native 直経路へフォールスルー）。store の locate は
    // 不要（node 解決は matd 側が KVS で行う）。
    match matd_client::resolve_route(
        &args.matd,
        std::env::var_os("MAT_MATD_SOCKET"),
        std::env::var_os("MAT_MATD"),
    ) {
        matd_client::Route::Forced(sockets) => return matd_client::dispatch(&sockets, &command),
        matd_client::Route::Auto(sockets) => {
            if let Some(code) = matd_client::dispatch_auto(&sockets, &command) {
                return code;
            }
        }
        matd_client::Route::Direct => {}
    }

    // native 直経路: MAT_IFACE 設定時はその iface、未設定なら自動検出
    // （M8c-3 native 既定化）。自動検出の候補 0 / 複数はハードエラー
    // （黙って落とさない — spec 設計 3）。
    let iface_owned: String = match &args.iface {
        Some(i) => i.clone(),
        None => match mat_native::iface_select::autodetect() {
            Ok(i) => {
                tracing::info!(iface = %i, "iface auto-selected (native default)");
                i
            }
            Err(e) => {
                e.emit();
                return ExitCode::from(e.kind.exit_code());
            }
        },
    };
    let native_cfg = Some(native_direct::Config {
        iface: &iface_owned,
        fabric_index: args.fabric_index,
        issuer_index: args.issuer_index,
    });
    if let Some(cfg) = &native_cfg {
        if let Some(result) = native_direct::run(&command, &store_path, cfg) {
            return match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    tracing::debug!(kind = ?e.kind, detail = %e.detail, "native direct failed");
                    e.emit();
                    ExitCode::from(e.kind.exit_code())
                }
            };
        }
    }

    // native_direct::run が `None` を返す op = 専用コマンド層を持つもの
    // （discover / commission / diag node）。それ以外の op はすべて native_direct
    // が処理済み（M8c-3 で chip-tool 経路は撤去）。
    let result = match &command {
        Command::Discover { probe } => {
            commands::discover::run(&store_path, *probe, native_cfg.as_ref())
        }
        Command::Commission {
            target,
            setup_code,
            node_id,
            alias,
            thread_dataset,
        } => commands::commission::run(
            &store_path,
            target,
            setup_code,
            *node_id,
            alias.as_deref(),
            native_cfg.as_ref(),
            thread_dataset.as_deref(),
        ),
        Command::Diag {
            action:
                DiagCommand::Node {
                    node_id,
                    endpoint,
                    deep,
                },
        } => node_id.id().and_then(|node| {
            endpoint.id().and_then(|ep| {
                commands::diag::node(&store_path, node, ep, *deep, native_cfg.as_ref())
            })
        }),
        // 他の全 op は native_direct::run が `Some` を返して上で処理済み。
        // Command::Fabric は route dispatch より前の早期 return で処理済み。
        _ => {
            unreachable!("native_direct::run handles all ops except discover/commission/diag-node")
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::debug!(kind = ?e.kind, detail = %e.detail, "command failed");
            e.emit();
            ExitCode::from(e.kind.exit_code())
        }
    }
}

/// 診断ログを stderr に出す。レベルは `MAT_LOG`（無ければ `RUST_LOG`）で制御、
/// 既定は `warn`。stdout は JSON 専用なので絶対に汚さない。
fn init_tracing() {
    let filter = EnvFilter::try_from_env("MAT_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
