//! clap(derive) による CLI 定義。
//!
//! Phase 0 のスコープは `discover` と `commission` のみ。read/write/invoke/
//! describe/on/off/open-window/group は後続フェーズで追加する。

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "mat",
    version,
    about = "Matter device control CLI (chip-tool wrapper)"
)]
pub struct Cli {
    /// 認証情報ストアのパス（既定: $MAT_STORE / $XDG_CONFIG_HOME/mat / ~/.config/mat）。
    #[arg(long, global = true, value_name = "PATH")]
    pub store: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// commissionable / commissioned ノードを mDNS で探索する。
    Discover,

    /// fabric への参加（初回 commission / multi-admin join 両対応）。
    Commission {
        /// 対象の IP アドレスまたは DNS-SD ホスト名。
        target: String,
        /// setup code（QR ペイロード `MT:...` または 11/21桁の manual code）。
        setup_code: String,
        /// 割り当てる node_id（省略時は台帳の最大値+1 を自動採番）。
        #[arg(long, value_name = "N")]
        node_id: Option<u64>,
    },
}
