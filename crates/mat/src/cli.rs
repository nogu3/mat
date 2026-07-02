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

    /// matd の unix socket 経由での実行を強制する（接続失敗はエラー、フォールバック無し）。
    /// 値を省略すると socket は `MAT_MATD_SOCKET` があればそれ、無ければ既定パス
    /// （`$XDG_RUNTIME_DIR/matd.sock`、無ければ `/tmp/matd.sock`）。
    /// 本フラグが無くても mat は既定で matd を**自動発見**する: 上記の socket へ接続を
    /// 試み、matd がいればそちら、いなければ直 chip-tool にフォールバック。
    /// `MAT_MATD=1` は本フラグ相当（強制）、`MAT_MATD=0` は自動発見の無効化（常に直経路）。
    /// `MAT_MATD_SOCKET` は socket パスの指定のみで経路は変えない。
    /// matd 対応は read/write/invoke/on/off/describe/group のみ
    /// （discover/commission/open-window/diag は常に直経路; 本フラグ明示時は exit 2）。
    #[arg(long, global = true, value_name = "SOCK", num_args = 0..=1)]
    pub matd: Option<Option<PathBuf>>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// commissionable / commissioned ノードを mDNS で探索する。
    Discover {
        /// commissioned ノードのライブ到達性を mDNS で確認し reachable を付与する。
        #[arg(long)]
        probe: bool,
    },

    /// fabric への参加（初回 commission / multi-admin join 両対応）。
    Commission {
        /// 対象の IP アドレスまたは DNS-SD ホスト名。
        #[arg(long, value_name = "HOST")]
        target: String,
        /// setup code（QR ペイロード `MT:...` または 11/21桁の manual code）。
        #[arg(long = "setup-code", value_name = "CODE")]
        setup_code: String,
        /// 割り当てる node_id（省略時は台帳の最大値+1 を自動採番）。
        #[arg(short = 'n', long = "node", value_name = "N")]
        node_id: Option<u64>,
    },

    /// 属性を読む。`{ node_id, endpoint, cluster, attribute, value, timestamp }`。
    Read {
        /// commission 済みノードの node_id。
        #[arg(short = 'n', long = "node", value_name = "N")]
        node_id: u64,
        /// エンドポイント番号（既定 1）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
        /// クラスタ名（chip-tool 表記、例: `onoff` / `levelcontrol`）。
        #[arg(short = 'c', long, value_name = "NAME")]
        cluster: String,
        /// 属性名（chip-tool 表記、例: `on-off` / `current-level`）。
        #[arg(short = 'a', long, value_name = "NAME")]
        attribute: String,
    },

    /// 書き込み可能属性を設定する。
    Write {
        /// commission 済みノードの node_id。
        #[arg(short = 'n', long = "node", value_name = "N")]
        node_id: u64,
        /// エンドポイント番号（既定 1）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
        /// クラスタ名（chip-tool 表記）。
        #[arg(short = 'c', long, value_name = "NAME")]
        cluster: String,
        /// 属性名（chip-tool 表記）。
        #[arg(short = 'a', long, value_name = "NAME")]
        attribute: String,
        /// 書き込む値（chip-tool にそのまま渡す）。
        #[arg(long, value_name = "VALUE")]
        value: String,
    },

    /// コマンドを実行する。照明 ON/OFF 等の「制御」はここ（属性 write ではない）。
    Invoke {
        /// commission 済みノードの node_id。
        #[arg(short = 'n', long = "node", value_name = "N")]
        node_id: u64,
        /// エンドポイント番号（既定 1）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
        /// クラスタ名（chip-tool 表記）。
        #[arg(short = 'c', long, value_name = "NAME")]
        cluster: String,
        /// コマンド名（chip-tool 表記、例: `on` / `off` / `move-to-level`）。
        #[arg(long, value_name = "NAME")]
        command: String,
        /// コマンド引数（chip-tool にそのまま渡す）。
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },

    /// ノードのエンドポイント / クラスタを introspect する。
    Describe {
        /// commission 済みノードの node_id。
        #[arg(short = 'n', long = "node", value_name = "N")]
        node_id: u64,
    },

    /// OnOff クラスタの On コマンドを invoke する高頻度ショートカット。
    On {
        /// commission 済みノードの node_id。
        #[arg(short = 'n', long = "node", value_name = "N")]
        node_id: u64,
        /// エンドポイント番号（既定 1）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },

    /// OnOff クラスタの Off コマンドを invoke する高頻度ショートカット。
    Off {
        /// commission 済みノードの node_id。
        #[arg(short = 'n', long = "node", value_name = "N")]
        node_id: u64,
        /// エンドポイント番号（既定 1）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },

    /// `mat` 所有デバイスを他 admin へ共有するため commissioning window を開く。
    /// `{ node_id, manual_code, qr_payload, expires_at }` を返す（QR 画像化は上層）。
    OpenWindow {
        /// commission 済みノードの node_id。
        #[arg(short = 'n', long = "node", value_name = "N")]
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

    /// ネットワーク診断スナップショット（メッシュ健全性の分析用）。
    Diag {
        #[command(subcommand)]
        action: DiagCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum DiagCommand {
    /// Thread Network Diagnostics (cluster 53) を 1 スナップショットで返す。
    /// `routing-role` / `partition-id` / `channel` / `network-name` / `rloc16` と
    /// `neighbor-table`（LQI/RSSI）/ `route-table`（cost）を集約する。
    Thread {
        /// commission 済みノードの node_id。
        #[arg(short = 'n', long = "node", value_name = "N")]
        node_id: u64,
        /// エンドポイント番号（既定 0 — 診断クラスタは通常 ep0）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 0)]
        endpoint: u16,
    },

    /// commissioned ノードが「なぜ制御できないか」を層別チェックして verdict で返す。
    /// 既定は chip-tool 完結。`--deep` で ping6 / mDNS ブラウズも実施し、
    /// link_starved（弱リンク）と fabric_missing（fabric 脱落）まで切り分ける。
    Node {
        /// commission 済みノードの node_id。
        #[arg(short = 'n', long = "node", value_name = "N")]
        node_id: u64,
        /// エンドポイント番号（既定 0 — 診断は通常 ep0）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 0)]
        endpoint: u16,
        /// 補助プローブ（ping6 / avahi-browse）も実施して深掘りする。
        #[arg(long)]
        deep: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum GroupCommand {
    /// 各ノードへ group 鍵束とマッピングを焼く（KeySetWrite / GroupKeyMap / AddGroup）。
    /// コントローラ側 group state（groupsettings）も併せて設定する。
    Provision {
        /// Matter GroupId（wire group 識別子）。
        #[arg(short = 'g', long = "group", value_name = "ID")]
        group_id: u16,
        /// provision 対象の commission 済み node_id（1つ以上）。
        #[arg(long = "nodes", required = true, num_args = 1..)]
        node_ids: Vec<u64>,
        /// 鍵束 ID（GroupKeySetID）。既定 42。
        #[arg(long, value_name = "N", default_value_t = 42)]
        keyset_id: u16,
        /// group 名（chip-tool groupsettings / AddGroup 用）。既定 `grp<group_id>`。
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// AddGroup を行うエンドポイント（既定 1）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
        /// epoch key（16バイト = 32桁 hex）。省略時は mat がランダム生成する。
        /// 複数コントローラで同一 wire group を共有する時のみ明示指定する。
        #[arg(long, value_name = "HEX")]
        epoch_key: Option<String>,
    },

    /// group へ multicast でコマンドを送る（unacknowledged。"sent" のみ報告）。
    Invoke {
        /// Matter GroupId。
        #[arg(short = 'g', long = "group", value_name = "ID")]
        group_id: u16,
        /// クラスタ名（chip-tool 表記、例: `onoff`）。
        #[arg(short = 'c', long, value_name = "NAME")]
        cluster: String,
        /// コマンド名（chip-tool 表記、例: `on` / `off`）。
        #[arg(long, value_name = "NAME")]
        command: String,
        /// コマンド引数（chip-tool にそのまま渡す）。
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
        /// 宛先エンドポイント（既定 1）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },
}
