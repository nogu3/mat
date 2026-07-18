//! clap(derive) による CLI 定義。
//!
//! Phase 0: `discover` / `commission`。Phase 1: `read` / `write` / `invoke` /
//! `describe` / `on` / `off`（後追いの高頻度ショートカットとして `color-temp` / `color` も）。
//! Phase 2: `open-window`。Phase 3: `group provision` / `group invoke`（groupcast）。

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use mat_core::alias::{EndpointRef, GroupRef, NodeRef};

#[derive(Parser, Debug)]
#[command(
    name = "mat",
    version,
    about = "Matter device control CLI (native Matter backend)"
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
    /// matd 対応は read/write/invoke/on/off/color-temp/color/describe/group のみ
    /// （discover/commission/open-window/diag は常に直経路; 本フラグ明示時は exit 2）。
    #[arg(long, global = true, value_name = "SOCK", num_args = 0..=1)]
    pub matd: Option<Option<PathBuf>>,

    /// one-shot 直経路を native（mat-controller 内蔵）で実行する場合の
    /// Thread mesh iface 名（例: eth0）。未設定なら自動検出（up・multicast・
    /// 非 P2P・非 loopback・IPv6 link-local の一意候補。曖昧ならエラー）。明示指定で
    /// 上書き。対象 op は README の native hotpath 一覧を参照（M8a で汎用
    /// read/write/invoke/describe 等、M8b で discover と mDNS probe に拡大）。
    /// matd 稼働中は matd 自動発見が優先される。
    #[arg(long, global = true, env = "MAT_IFACE", value_name = "IFACE")]
    pub iface: Option<String>,

    /// native 直経路が読む KVS fabric テーブルの index。
    #[arg(
        long,
        global = true,
        env = "MAT_FABRIC_INDEX",
        default_value_t = 1,
        value_name = "N"
    )]
    pub fabric_index: u8,

    /// native 直経路の CA issuer index。
    #[arg(
        long,
        global = true,
        env = "MAT_ISSUER_INDEX",
        default_value_t = 0,
        value_name = "N"
    )]
    pub issuer_index: u8,

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
        /// commission 成功時に aliases.toml へ登録する node alias（任意）。
        /// 純数字・使用済みの名前は commission 開始前に exit 2。
        #[arg(long, value_name = "NAME")]
        alias: Option<String>,
        /// BLE+Thread commissioning 用の Thread active operational dataset
        /// （hex）。native 経路（MAT_IFACE）で mDNS に見つからないデバイスを
        /// BLE で commission するときに必須。chip-tool 経路では未使用。
        #[arg(
            long = "thread-dataset",
            env = "MAT_THREAD_DATASET",
            value_name = "HEX"
        )]
        thread_dataset: Option<String>,
    },

    /// 属性を読む。`{ node_id, endpoint, cluster, attribute, value, timestamp }`。
    Read {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 1）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "1")]
        endpoint: EndpointRef,
        /// クラスタ名（chip-tool 表記、例: `onoff` / `levelcontrol`）。
        #[arg(short = 'c', long, value_name = "NAME")]
        cluster: String,
        /// 属性名（chip-tool 表記、例: `on-off` / `current-level`）。
        #[arg(short = 'a', long, value_name = "NAME")]
        attribute: String,
    },

    /// 書き込み可能属性を設定する。
    Write {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 1）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "1")]
        endpoint: EndpointRef,
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
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 1）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "1")]
        endpoint: EndpointRef,
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
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
    },

    /// OnOff クラスタの On コマンドを invoke する高頻度ショートカット。
    On {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 1）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "1")]
        endpoint: EndpointRef,
    },

    /// OnOff クラスタの Off コマンドを invoke する高頻度ショートカット。
    Off {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 1）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "1")]
        endpoint: EndpointRef,
    },

    /// ColorControl の MoveToColorTemperature を invoke する高頻度ショートカット。
    /// 色温度は `--kelvin`（`mireds = round(1_000_000 / kelvin)` に換算）か
    /// `--mireds`（直指定）のどちらか一方で与える。デバイス対応範囲外の値は
    /// デバイス側が clamp する（mat は事前 read / 検証をしない）。
    ColorTemp {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 1）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "1")]
        endpoint: EndpointRef,
        /// 色温度（ケルビン）。値域は mireds が u16 に収まる 16..=1000000。
        #[arg(
            long,
            value_name = "K",
            conflicts_with = "mireds",
            required_unless_present = "mireds",
            value_parser = clap::value_parser!(u32).range(16..=1_000_000)
        )]
        kelvin: Option<u32>,
        /// 色温度（mireds）。`--kelvin` と排他。
        #[arg(long, value_name = "M", value_parser = clap::value_parser!(u16).range(1..))]
        mireds: Option<u16>,
        /// 遷移時間（0.1 秒単位、既定 0 = 即時）。例: 30 = 3 秒。
        #[arg(long, value_name = "DS", default_value_t = 0)]
        transition: u16,
    },

    /// ColorControl の MoveToHueAndSaturation を invoke する高頻度ショートカット。
    /// 色は `--name`（色名）/ `--rgb`（HEX or R,G,B）/ `--hue`+`--sat`（生指定、両方
    /// 必須）の 3 系統から 1 つで指定する。名前・RGB は RGB→HSV で hue/sat へ換算
    /// し、mat が Matter の 0–254 値（`round(v / full * 254)`、255 は予約値）へ
    /// 更に落とす。名前・RGB は色だけ設定し明度（明るさ）は変えない。デバイス対応
    /// 範囲外の値はデバイス側が clamp する（mat は事前 read / 検証をしない）。
    Color {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 1）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "1")]
        endpoint: EndpointRef,
        #[command(flatten)]
        spec: ColorSpecArgs,
        /// 遷移時間（0.1 秒単位、既定 0 = 即時）。例: 30 = 3 秒。
        #[arg(long, value_name = "DS", default_value_t = 0)]
        transition: u16,
    },

    /// `mat` 所有デバイスを他 admin へ共有するため commissioning window を開く。
    /// `{ node_id, manual_code, qr_payload, expires_at }` を返す（QR 画像化は上層）。
    OpenWindow {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
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

    /// fabric 管理（初回 bootstrap）。
    Fabric {
        #[command(subcommand)]
        action: FabricAction,
    },
}

/// `mat fabric` のサブコマンド（M8c-3）。
#[derive(Subcommand, Debug)]
pub enum FabricAction {
    /// 初回 fabric bootstrap: root CA + ランダム epoch IPK を生成し KVS を新規作成
    Init {
        /// fabric id（既定 1）
        #[arg(long, default_value_t = 1)]
        fabric_id: u64,
        /// controller 自身の admin node id（既定 112233 = chip-tool 慣例値）
        #[arg(long, default_value_t = 112_233)]
        admin_node_id: u64,
    },
}

/// 色の指定（3 系統から 1 つ、排他）: `--name`（色名）/ `--rgb`（HEX or R,G,B）/
/// `--hue`+`--sat`（生指定、両方必須）。名前・RGB は RGB→HSV で hue/sat へ換算し、
/// V（明度）は捨てる — **色だけ設定し、明るさは変えない**（明るさは LevelControl
/// の領分）。点灯中でないと反映されない（ExecuteIfOff は立てない）。
#[derive(clap::Args, Debug, Clone, PartialEq)]
#[group(id = "color_spec", required = true, multiple = true)]
pub struct ColorSpecArgs {
    /// 色名。組み込み: red / pink / orange / purple / cyan / green / blue /
    /// yellow / magenta / white。aliases.toml の `[colors]` で追加・上書き可
    /// （RGB 値で定義）。色だけ設定し、明るさ（明度）は変えない。
    #[arg(long, value_name = "NAME", conflicts_with_all = ["rgb", "hue", "sat"])]
    pub name: Option<String>,
    /// RGB 値（`#ff0000` / `ff0000` / `255,0,0`）。RGB→HSV で hue/sat へ換算し、
    /// 明度（V）は捨てる（明るさは変えない）。
    #[arg(long, value_name = "HEX|R,G,B", conflicts_with_all = ["hue", "sat"])]
    pub rgb: Option<String>,
    /// 色相（度、0–360）。例: 330 = ピンク。`--sat` と併用必須。
    #[arg(long, value_name = "DEG", requires = "sat", value_parser = clap::value_parser!(u16).range(0..=360))]
    pub hue: Option<u16>,
    /// 彩度（%、0–100）。`--hue` と併用必須。
    #[arg(long, value_name = "PCT", requires = "hue", value_parser = clap::value_parser!(u8).range(0..=100))]
    pub sat: Option<u8>,
}

#[derive(Subcommand, Debug)]
pub enum DiagCommand {
    /// Thread Network Diagnostics (cluster 53) を 1 スナップショットで返す。
    /// `routing-role` / `partition-id` / `channel` / `network-name` / `rloc16` と
    /// `neighbor-table`（LQI/RSSI）/ `route-table`（cost）を集約する。
    Thread {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 0 — 診断クラスタは通常 ep0）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "0")]
        endpoint: EndpointRef,
    },

    /// commissioned ノードが「なぜ制御できないか」を層別チェックして verdict で返す。
    /// 既定は chip-tool 完結。`--deep` で ping6 / mDNS ブラウズも実施し、
    /// link_starved（弱リンク）と fabric_missing（fabric 脱落）まで切り分ける。
    Node {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 0 — 診断は通常 ep0）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "0")]
        endpoint: EndpointRef,
        /// 補助プローブ（ping6 / native mDNS targeted resolve）も実施して深掘りする。
        #[arg(long)]
        deep: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum GroupCommand {
    /// 各ノードへ group 鍵束とマッピングを焼く（KeySetWrite / GroupKeyMap / AddGroup）。
    /// コントローラ側 group state（groupsettings）も併せて設定する。
    Provision {
        /// Matter GroupId、または aliases.toml の group alias。
        #[arg(short = 'g', long = "group", value_name = "ID|ALIAS")]
        group_id: GroupRef,
        /// provision 対象の commission 済み node_id または node alias（1つ以上）。
        #[arg(long = "nodes", required = true, num_args = 1..)]
        node_ids: Vec<NodeRef>,
        /// 鍵束 ID（GroupKeySetID）。既定 42。
        #[arg(long, value_name = "N", default_value_t = 42)]
        keyset_id: u16,
        /// group 名（chip-tool groupsettings / AddGroup 用）。既定 `grp<group_id>`。
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// AddGroup を行うエンドポイント（既定 1、数値のみ — ノード文脈が無いため alias 不可）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
        /// epoch key（16バイト = 32桁 hex）。省略時は mat がランダム生成する。
        /// 複数コントローラで同一 wire group を共有する時のみ明示指定する。
        #[arg(long, value_name = "HEX")]
        epoch_key: Option<String>,
        /// 既存グループの keyset binding を unbind してから bind し直す（既存グループ
        /// へのノード追加用）。--nodes には既存メンバー全員 + 新規を渡し、--keyset-id
        /// は既存と同じ値にすること（新規だけ渡すと epoch key が既存メンバーと食い違い
        /// 届かなくなる）。未 bind の新規グループに付けても安全（冪等）。
        #[arg(long)]
        rebind: bool,
    },

    /// group へ multicast でコマンドを送る（unacknowledged。"sent" のみ報告）。
    Invoke {
        /// Matter GroupId、または aliases.toml の group alias。
        #[arg(short = 'g', long = "group", value_name = "ID|ALIAS")]
        group_id: GroupRef,
        /// クラスタ名（chip-tool 表記、例: `onoff`）。
        #[arg(short = 'c', long, value_name = "NAME")]
        cluster: String,
        /// コマンド名（chip-tool 表記、例: `on` / `off`）。
        #[arg(long, value_name = "NAME")]
        command: String,
        /// コマンド引数（chip-tool にそのまま渡す）。
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
        /// 宛先エンドポイント（既定 1、数値のみ — ノード文脈が無いため alias 不可）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },

    /// ColorControl MoveToColorTemperature を group へ multicast する高頻度
    /// ショートカット（`mat color-temp` の group 版）。`--kelvin`（mireds へ換算）
    /// か `--mireds` のどちらか一方。unacknowledged groupcast なので "sent" のみ
    /// 報告する。点灯中でないと反映されない（ExecuteIfOff は立てない）。
    ColorTemp {
        /// Matter GroupId、または aliases.toml の group alias。
        #[arg(short = 'g', long = "group", value_name = "ID|ALIAS")]
        group_id: GroupRef,
        /// 色温度（ケルビン）。値域は mireds が u16 に収まる 16..=1000000。
        #[arg(
            long,
            value_name = "K",
            conflicts_with = "mireds",
            required_unless_present = "mireds",
            value_parser = clap::value_parser!(u32).range(16..=1_000_000)
        )]
        kelvin: Option<u32>,
        /// 色温度（mireds）。`--kelvin` と排他。
        #[arg(long, value_name = "M", value_parser = clap::value_parser!(u16).range(1..))]
        mireds: Option<u16>,
        /// 遷移時間（0.1 秒単位、既定 0 = 即時）。例: 30 = 3 秒。
        #[arg(long, value_name = "DS", default_value_t = 0)]
        transition: u16,
        /// 宛先エンドポイント（既定 1、数値のみ — ノード文脈が無いため alias 不可）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },

    /// ColorControl MoveToHueAndSaturation を group へ multicast する高頻度
    /// ショートカット（`mat color` の group 版）。色は `--name` / `--rgb` /
    /// `--hue`+`--sat` の 1 系統で指定（名前・RGB は明度を変えない）。
    /// unacknowledged groupcast なので "sent" のみ報告する。点灯中でないと
    /// 反映されない（ExecuteIfOff は立てない）。
    Color {
        /// Matter GroupId、または aliases.toml の group alias。
        #[arg(short = 'g', long = "group", value_name = "ID|ALIAS")]
        group_id: GroupRef,
        #[command(flatten)]
        spec: ColorSpecArgs,
        /// 遷移時間（0.1 秒単位、既定 0 = 即時）。例: 30 = 3 秒。
        #[arg(long, value_name = "DS", default_value_t = 0)]
        transition: u16,
        /// 宛先エンドポイント（既定 1、数値のみ — ノード文脈が無いため alias 不可）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },

    /// provision 済みグループの ACL 修復: 各ノードの ACL に Group エントリ
    /// （privilege=Operate, authMode=Group, subjects=[GroupId]）を read-merge-write
    /// で追記する。既にあれば何もしない（冪等）。provision の 4 ステップ目と同じ
    /// 処理を単独実行する（controller 側 groupsettings が非冪等で provision を
    /// 再実行できない既存グループの救済用）。常に直経路（--matd 明示時は exit 2）。
    Grant {
        /// Matter GroupId、または aliases.toml の group alias。
        #[arg(short = 'g', long = "group", value_name = "ID|ALIAS")]
        group_id: GroupRef,
        /// 対象の commission 済み node_id または node alias（1つ以上）。
        #[arg(long = "nodes", required = true, num_args = 1..)]
        node_ids: Vec<NodeRef>,
    },
}
