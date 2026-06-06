//! clap(derive) による CLI 定義。
//!
//! Phase 0: `discover` / `commission`。Phase 1: `read` / `write` / `invoke` /
//! `describe` / `on` / `off`。open-window / group は後続フェーズで追加する。

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

    /// 属性を読む。`{ node_id, endpoint, cluster, attribute, value, timestamp }`。
    Read {
        /// commission 済みノードの node_id。
        node_id: u64,
        /// エンドポイント番号。
        endpoint: u16,
        /// クラスタ名（chip-tool 表記、例: `onoff` / `levelcontrol`）。
        cluster: String,
        /// 属性名（chip-tool 表記、例: `on-off` / `current-level`）。
        attribute: String,
    },

    /// 書き込み可能属性を設定する。
    Write {
        /// commission 済みノードの node_id。
        node_id: u64,
        /// エンドポイント番号。
        endpoint: u16,
        /// クラスタ名（chip-tool 表記）。
        cluster: String,
        /// 属性名（chip-tool 表記）。
        attribute: String,
        /// 書き込む値（chip-tool にそのまま渡す）。
        value: String,
    },

    /// コマンドを実行する。照明 ON/OFF 等の「制御」はここ（属性 write ではない）。
    Invoke {
        /// commission 済みノードの node_id。
        node_id: u64,
        /// エンドポイント番号。
        endpoint: u16,
        /// クラスタ名（chip-tool 表記）。
        cluster: String,
        /// コマンド名（chip-tool 表記、例: `on` / `off` / `move-to-level`）。
        command: String,
        /// コマンド引数（chip-tool にそのまま渡す）。
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },

    /// ノードのエンドポイント / クラスタを introspect する。
    Describe {
        /// commission 済みノードの node_id。
        node_id: u64,
    },

    /// OnOff クラスタの On コマンドを invoke する高頻度ショートカット。
    On {
        /// commission 済みノードの node_id。
        node_id: u64,
        /// エンドポイント番号（既定 1）。
        #[arg(long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },

    /// OnOff クラスタの Off コマンドを invoke する高頻度ショートカット。
    Off {
        /// commission 済みノードの node_id。
        node_id: u64,
        /// エンドポイント番号（既定 1）。
        #[arg(long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },
}
