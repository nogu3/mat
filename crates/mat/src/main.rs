//! `mat` — Matter デバイス操作 CLI。`chip-tool` をサブプロセスで呼び、その出力を
//! `mat` のスキーマに正規化して返す。
//!
//! stdout は純粋な構造化 JSON のみ。診断は stderr に構造化ログ（`tracing`）。
//! 認証情報 KVS 以外の永続状態は持たない。

mod cli;
mod commands;
mod matd_client;
mod runner;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::{fmt, EnvFilter};

use cli::{Cli, Command, DiagCommand, GroupCommand};
use mat_core::store::Store;

fn main() -> ExitCode {
    init_tracing();

    // 引数エラー（exit 2）は clap が直接処理する。
    let args = Cli::parse();

    // --matd フラグ（または MAT_MATD=truthy）で有効化された時は chip-tool を直に起動せず、
    // 常駐 matd（warm CASE セッション）経由で実行する。MAT_MATD_SOCKET は socket パスの
    // 指定のみで単独では有効化しない。store の locate は不要（node 解決は matd 側が KVS で行う）。
    if let Some(socket) = matd_client::resolve_socket(
        &args.matd,
        std::env::var_os("MAT_MATD_SOCKET"),
        std::env::var_os("MAT_MATD"),
    ) {
        return matd_client::dispatch(&socket, &args.command);
    }

    let store_path = Store::locate(args.store);

    let result = match &args.command {
        Command::Discover => commands::discover::run(&store_path),
        Command::Commission {
            target,
            setup_code,
            node_id,
        } => commands::commission::run(&store_path, target, setup_code, *node_id),
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => commands::read::run(&store_path, *node_id, *endpoint, cluster, attribute),
        Command::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => commands::write::run(&store_path, *node_id, *endpoint, cluster, attribute, value),
        Command::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            args,
        } => commands::invoke::run(&store_path, *node_id, *endpoint, cluster, command, args),
        Command::Describe { node_id } => commands::describe::run(&store_path, *node_id),
        Command::On { node_id, endpoint } => {
            commands::invoke::run_onoff(&store_path, *node_id, *endpoint, true)
        }
        Command::Off { node_id, endpoint } => {
            commands::invoke::run_onoff(&store_path, *node_id, *endpoint, false)
        }
        Command::OpenWindow {
            node_id,
            timeout,
            iteration,
            discriminator,
        } => {
            // discriminator 未指定なら node_id から決定的に算出（12-bit に収める）。
            let disc = discriminator.unwrap_or_else(|| (*node_id % 4096) as u16);
            commands::open_window::run(&store_path, *node_id, *timeout, *iteration, disc)
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
                // name 未指定なら group_id から決定的に補完（open-window の disc と同様）。
                let name = name.clone().unwrap_or_else(|| format!("grp{group_id}"));
                commands::group::provision(
                    &store_path,
                    *group_id,
                    node_ids,
                    *keyset_id,
                    &name,
                    *endpoint,
                    epoch_key.as_deref(),
                )
            }
            GroupCommand::Invoke {
                group_id,
                cluster,
                command,
                args,
                endpoint,
            } => commands::group::invoke(&store_path, *group_id, cluster, command, args, *endpoint),
        },
        Command::Diag { action } => match action {
            DiagCommand::Thread { node_id, endpoint } => {
                commands::diag::thread(&store_path, *node_id, *endpoint)
            }
            DiagCommand::Node {
                node_id,
                endpoint,
                deep,
            } => commands::diag::node(&store_path, *node_id, *endpoint, *deep),
        },
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
