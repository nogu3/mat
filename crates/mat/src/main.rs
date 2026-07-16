//! `mat` — Matter デバイス操作 CLI。`chip-tool` をサブプロセスで呼び、その出力を
//! `mat` のスキーマに正規化して返す。
//!
//! stdout は純粋な構造化 JSON のみ。診断は stderr に構造化ログ（`tracing`）。
//! 認証情報 KVS 以外の永続状態は持たない。

mod cli;
mod commands;
mod matd_client;
mod native_direct;
mod probe;
mod resolve;
mod runner;

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::{fmt, EnvFilter};

use cli::{Cli, Command, DiagCommand, GroupCommand};
use mat_core::alias::NodeRef;
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

    // 経路解決（matd_client::resolve_route）: --matd / MAT_MATD=truthy は強制 matd、
    // MAT_MATD=falsy は強制直、どちらも無ければ自動検出（connect 成功時のみ matd 経由、
    // 失敗時と非対応 op は下の直 chip-tool 経路へフォールスルー）。store の locate は
    // 不要（node 解決は matd 側が KVS で行う）。
    match matd_client::resolve_route(
        &args.matd,
        std::env::var_os("MAT_MATD_SOCKET"),
        std::env::var_os("MAT_MATD"),
    ) {
        matd_client::Route::Forced(socket) => return matd_client::dispatch(&socket, &command),
        matd_client::Route::Auto(socket) => {
            if let Some(code) = matd_client::dispatch_auto(&socket, &command) {
                return code;
            }
        }
        matd_client::Route::Direct => {}
    }

    // native 直経路（M7）: MAT_IFACE 設定時、対象 op なら mat-controller で
    // in-process 実行。None は chip-tool 直へフォールスルー。
    if let Some(iface) = &args.iface {
        let cfg = native_direct::Config {
            iface,
            fabric_index: args.fabric_index,
            issuer_index: args.issuer_index,
        };
        if let Some(result) = native_direct::try_run(&command, &store_path, &cfg) {
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

    let result = match &command {
        Command::Discover { probe } => commands::discover::run(&store_path, *probe),
        Command::Commission {
            target,
            setup_code,
            node_id,
            alias,
        } => commands::commission::run(&store_path, target, setup_code, *node_id, alias.as_deref()),
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => commands::read::run(&store_path, node_id.id(), endpoint.id(), cluster, attribute),
        Command::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => commands::write::run(
            &store_path,
            node_id.id(),
            endpoint.id(),
            cluster,
            attribute,
            value,
        ),
        Command::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            args,
        } => commands::invoke::run(
            &store_path,
            node_id.id(),
            endpoint.id(),
            cluster,
            command,
            args,
        ),
        Command::Describe { node_id } => commands::describe::run(&store_path, node_id.id()),
        Command::On { node_id, endpoint } => {
            commands::invoke::run_onoff(&store_path, node_id.id(), endpoint.id(), true)
        }
        Command::Off { node_id, endpoint } => {
            commands::invoke::run_onoff(&store_path, node_id.id(), endpoint.id(), false)
        }
        Command::ColorTemp {
            node_id,
            endpoint,
            kelvin,
            mireds,
            transition,
        } => {
            // --kelvin / --mireds を (mireds, kelvin) に解決（欠けた側は逆数換算で補完）。
            let (mireds, kelvin) = commands::invoke::resolve_color_temp(*kelvin, *mireds);
            commands::invoke::run_color_temp(
                &store_path,
                node_id.id(),
                endpoint.id(),
                kelvin,
                mireds,
                *transition,
            )
        }
        Command::Color {
            node_id,
            endpoint,
            spec,
            transition,
        } => mat_core::color::resolve_spec(
            spec.name.as_deref(),
            spec.rgb.as_deref(),
            spec.hue,
            spec.sat,
        )
        .and_then(|c| {
            commands::invoke::run_color(&store_path, node_id.id(), endpoint.id(), &c, *transition)
        }),
        Command::OpenWindow {
            node_id,
            timeout,
            iteration,
            discriminator,
        } => {
            // discriminator 未指定なら node_id から決定的に算出（12-bit に収める、
            // native 直経路 classify と同じ共有式）。
            let disc = native_direct::resolve_discriminator(node_id.id(), *discriminator);
            commands::open_window::run(&store_path, node_id.id(), *timeout, *iteration, disc)
        }
        Command::Group { action } => match action {
            GroupCommand::Provision {
                group_id,
                node_ids,
                keyset_id,
                name,
                endpoint,
                epoch_key,
                rebind,
            } => {
                // name 未指定なら group_id から決定的に補完（open-window の disc と同様）。
                let gid = group_id.id();
                let name = name.clone().unwrap_or_else(|| format!("grp{gid}"));
                let ids: Vec<u64> = node_ids.iter().map(NodeRef::id).collect();
                commands::group::provision(
                    &store_path,
                    gid,
                    &ids,
                    *keyset_id,
                    &name,
                    *endpoint,
                    epoch_key.as_deref(),
                    *rebind,
                )
            }
            GroupCommand::Invoke {
                group_id,
                cluster,
                command,
                args,
                endpoint,
            } => commands::group::invoke(
                &store_path,
                group_id.id(),
                cluster,
                command,
                args,
                *endpoint,
            ),
            GroupCommand::Grant { group_id, node_ids } => {
                let ids: Vec<u64> = node_ids.iter().map(NodeRef::id).collect();
                commands::group::grant(&store_path, group_id.id(), &ids)
            }
            GroupCommand::ColorTemp {
                group_id,
                kelvin,
                mireds,
                transition,
                endpoint,
            } => {
                // --kelvin / --mireds を (mireds, kelvin) に解決（単体 color-temp と同じ規則）。
                let (mireds, kelvin) = commands::invoke::resolve_color_temp(*kelvin, *mireds);
                commands::group::color_temp(
                    &store_path,
                    group_id.id(),
                    kelvin,
                    mireds,
                    *transition,
                    *endpoint,
                )
            }
            GroupCommand::Color {
                group_id,
                spec,
                transition,
                endpoint,
            } => mat_core::color::resolve_spec(
                spec.name.as_deref(),
                spec.rgb.as_deref(),
                spec.hue,
                spec.sat,
            )
            .and_then(|c| {
                commands::group::color(&store_path, group_id.id(), &c, *transition, *endpoint)
            }),
        },
        Command::Diag { action } => match action {
            DiagCommand::Thread { node_id, endpoint } => {
                commands::diag::thread(&store_path, node_id.id(), endpoint.id())
            }
            DiagCommand::Node {
                node_id,
                endpoint,
                deep,
            } => commands::diag::node(&store_path, node_id.id(), endpoint.id(), *deep),
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
