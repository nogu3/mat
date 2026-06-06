//! `mat` — Matter デバイス操作 CLI。`chip-tool` をサブプロセスで呼び、その出力を
//! `mat` のスキーマに正規化して返す。
//!
//! stdout は純粋な構造化 JSON のみ。診断は stderr に構造化ログ（`tracing`）。
//! 認証情報 KVS 以外の永続状態は持たない。

mod cli;
mod commands;
mod error;
mod output;
mod parse;
mod runner;
mod store;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::{fmt, EnvFilter};

use cli::{Cli, Command};
use store::Store;

fn main() -> ExitCode {
    init_tracing();

    // 引数エラー（exit 2）は clap が直接処理する。
    let args = Cli::parse();
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
