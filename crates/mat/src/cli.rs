//! clap(derive) による CLI 定義。
//!
//! Phase 0: `discover` / `commission`。Phase 1: `read` / `write` / `invoke` /
//! `describe` / `on` / `off`。Phase 2: `open-window`。Phase 3: `group provision` /
//! `group invoke`（groupcast）。

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

    /// `mat` 所有デバイスを他 admin へ共有するため commissioning window を開く。
    /// `{ node_id, manual_code, qr_payload, expires_at }` を返す（QR 画像化は上層）。
    OpenWindow {
        /// commission 済みノードの node_id。
        node_id: u64,
        /// window を開いておく秒数（既定 180）。
        #[arg(long, value_name = "S", default_value_t = 180)]
        timeout: u32,
        /// PAKE の iteration count（既定 1000）。
        #[arg(long, value_name = "N", default_value_t = 1000)]
        iteration: u32,
        /// 12-bit discriminator（既定: node_id から決定的に算出）。
        #[arg(long, value_name = "D")]
        discriminator: Option<u16>,
    },

    /// Matter wire group（groupcast）の操作。複数機器を multicast 1発で同期制御する。
    /// 論理グループ名の解決は上層の責務で、ここは GroupId ベースの on-wire 操作のみ。
    Group {
        #[command(subcommand)]
        action: GroupCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum GroupCommand {
    /// 各ノードへ group 鍵束とマッピングを焼く（KeySetWrite / GroupKeyMap / AddGroup）。
    /// コントローラ側 group state（groupsettings）も併せて設定する。
    Provision {
        /// Matter GroupId（wire group 識別子）。
        group_id: u16,
        /// provision 対象の commission 済み node_id（1つ以上）。
        #[arg(required = true, num_args = 1..)]
        node_ids: Vec<u64>,
        /// 鍵束 ID（GroupKeySetID）。既定 42。
        #[arg(long, value_name = "N", default_value_t = 42)]
        keyset_id: u16,
        /// group 名（chip-tool groupsettings / AddGroup 用）。既定 `grp<group_id>`。
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// AddGroup を行うエンドポイント（既定 1）。
        #[arg(long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
        /// epoch key（16バイト = 32桁 hex）。省略時は mat がランダム生成する。
        /// 複数コントローラで同一 wire group を共有する時のみ明示指定する。
        #[arg(long, value_name = "HEX")]
        epoch_key: Option<String>,
    },

    /// group へ multicast でコマンドを送る（unacknowledged。"sent" のみ報告）。
    Invoke {
        /// Matter GroupId。
        group_id: u16,
        /// クラスタ名（chip-tool 表記、例: `onoff`）。
        cluster: String,
        /// コマンド名（chip-tool 表記、例: `on` / `off`）。
        command: String,
        /// コマンド引数（chip-tool にそのまま渡す）。
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
        /// 宛先エンドポイント（既定 1）。
        #[arg(long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },
}
