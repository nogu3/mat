//! 上流クライアント（`mat --matd` など）⇔ `matd` のソケットプロトコル。
//!
//! newline-delimited JSON。1 行 = 1 リクエスト = 1 レスポンス。`mat` の one-shot
//! CLI と同じ「1 操作 = 1 JSON」精神を保ち、stdout 相当（ソケット応答）は純粋な
//! 構造化 JSON のみ。`op` で内部タグ付けし、`id` があれば応答にエコーする。

use serde::Deserialize;
use serde_json::Value;

/// 上流からの 1 リクエスト。`id` は任意（応答にそのまま返す）、残りは `op` で分岐。
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// 呼び出し側の相関 ID（任意）。応答にエコーする。
    #[serde(default)]
    pub id: Option<Value>,
    /// クライアントの op 予算（相対 ms）。単一ノード op のみに適用される。
    /// `Some(0)` = 明示無制限、無指定（旧クライアント）= matd 既定 60s
    /// （server::DEFAULT_OP_BUDGET）。Issue #16。
    #[serde(default)]
    pub deadline_ms: Option<u64>,
    #[serde(flatten)]
    pub op: Op,
}

/// 操作種別。Matter クラスタコマンド体系に 1:1 で対応する。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// 属性を読む。`attribute` 省略 = cluster wildcard read。
    Read {
        node_id: u64,
        endpoint: u16,
        cluster: String,
        #[serde(default)]
        attribute: Option<String>,
    },
    /// 書き込み可能属性を設定する。
    Write {
        node_id: u64,
        endpoint: u16,
        cluster: String,
        attribute: String,
        value: String,
    },
    /// クラスタコマンドを実行する。
    Invoke {
        node_id: u64,
        endpoint: u16,
        cluster: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// OnOff On のショートカット。
    On { node_id: u64, endpoint: u16 },
    /// OnOff Off のショートカット。
    Off { node_id: u64, endpoint: u16 },
    /// ColorControl MoveToColorTemperature のショートカット（`mat color-temp` 相当）。
    /// `mireds` は mat 側で換算済みの値を受け取る。`kelvin` は応答へのエコー用
    /// （matd 側で逆算すると丸めで入力とずれるため、換算は mat の 1 箇所に置く）。
    ColorTemp {
        node_id: u64,
        endpoint: u16,
        mireds: u16,
        kelvin: u32,
        #[serde(default)]
        transition: u16,
    },
    /// LevelControl MoveToLevel のショートカット（`mat level` 相当）。
    /// `level` は mat 側で換算済みの 0–254 生値。`percent` は応答へのエコー用
    /// （換算は mat の 1 箇所に置く — color_temp の mireds/kelvin と同じ約束）。
    Level {
        node_id: u64,
        endpoint: u16,
        level: u8,
        percent: u8,
        #[serde(default)]
        transition: u16,
    },
    /// ColorControl MoveToHueAndSaturation のショートカット（`mat color` 相当）。
    /// `hue_raw` / `saturation_raw` は mat 側で換算済みの 0–254 値を受け取る。
    /// `hue`（度）/ `saturation`（%）は応答へのエコー用
    /// （matd 側で逆算すると丸めで入力とずれるため、換算は mat の 1 箇所に置く）。
    /// `name` / `rgb` は name / RGB 指定時の応答エコー用（任意）。
    Color {
        node_id: u64,
        endpoint: u16,
        hue_raw: u8,
        saturation_raw: u8,
        hue: u16,
        saturation: u8,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        rgb: Option<String>,
        #[serde(default)]
        transition: u16,
    },
    /// ノードのエンドポイント / クラスタを introspect する（Descriptor クラスタ）。
    Describe { node_id: u64 },
    /// group（groupcast）を各ノードへ provision する。`mat group provision` 相当。
    GroupProvision {
        group_id: u16,
        node_ids: Vec<u64>,
        keyset_id: u16,
        name: String,
        endpoint: u16,
        #[serde(default)]
        epoch_key: Option<String>,
        /// 既存グループの keyset binding を unbind してから bind し直す（issue #5）。
        /// 旧 mat からの op には無いフィールドなので default = false。
        #[serde(default)]
        rebind: bool,
    },
    /// group へ multicast でコマンドを送る。`mat group invoke` 相当。
    GroupInvoke {
        group_id: u16,
        cluster: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        endpoint: u16,
    },
    /// ColorControl MoveToColorTemperature の group ショートカット
    /// （`mat group color-temp` 相当、groupcast）。`mireds` は mat 側で換算済み、
    /// `kelvin` は応答エコー用。unacknowledged なので "sent" のみ報告する。
    GroupColorTemp {
        group_id: u16,
        mireds: u16,
        kelvin: u32,
        #[serde(default)]
        transition: u16,
        endpoint: u16,
    },
    /// LevelControl MoveToLevel の group ショートカット（`mat group level`
    /// 相当、groupcast）。`level` は mat 側で換算済み、`percent` はエコー用。
    /// unacknowledged なので "sent" のみ報告する。
    GroupLevel {
        group_id: u16,
        level: u8,
        percent: u8,
        #[serde(default)]
        transition: u16,
        endpoint: u16,
    },
    /// ColorControl MoveToHueAndSaturation の group ショートカット
    /// （`mat group color` 相当、groupcast）。raw は mat 側で換算済み、
    /// 度・%・name・rgb は応答エコー用。unacknowledged なので "sent" のみ報告する。
    GroupColor {
        group_id: u16,
        hue_raw: u8,
        saturation_raw: u8,
        hue: u16,
        saturation: u8,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        rgb: Option<String>,
        #[serde(default)]
        transition: u16,
        endpoint: u16,
    },
    /// group 送信 counter の窓ジャンプ（`mat group bump` 相当、Issue #14 応急
    /// コマンド）。counter は fabric 全体で 1 本 — 対象 group は取らない。
    GroupBump,
    /// イベントストリーム購読（matd 専用 op）。ack 1 行の後、フィルタ一致
    /// イベントを同接続へ流し続ける（「1行=1往復」の唯一の例外）。全省略 = 全イベント。
    Listen {
        #[serde(default)]
        node_id: Option<u64>,
        #[serde(default)]
        endpoint: Option<u16>,
        #[serde(default)]
        cluster: Option<String>,
        #[serde(default)]
        attribute: Option<String>,
    },
    /// デーモン死活確認（native backend には触れない）。
    Ping,
    /// 購読とデーモンの現況を返す admin op（`matd status` が送る）。
    /// Ping と同じく単一 node は持たず、デバイス・ワイヤには触れない
    /// （dispatch がレジストリ snapshot を JSON 化するだけ）。
    Status,
    /// デーモンを停止する admin op（native backend には触れない）。`matd stop` が送る。
    /// Ping と同じく単一 node は持たない。
    Shutdown,
    /// 外部（mat 直経路等）が対象ノードへ触れた合図。SubHealth に touched を立てて
    /// 購読の即時張り直しを促す（Issue #20）。native には触れないため Status と同じく
    /// dispatch で短絡する。`node_id()` は意図的に None — abort_op の drop_session /
    /// deadline 対象にしない（fire-and-forget、再購読完了を待たない契約）。
    NodeTouched { node_id: u64 },
}

impl Op {
    /// この操作が対象とする単一 node_id（あれば）。`require_node` 用。
    ///
    /// `GroupProvision` は複数ノードを各々チェックするため、`GroupInvoke` は特定ノード
    /// 宛でない（multicast）ため、ここでは `None`（個別に扱う）。
    pub fn node_id(&self) -> Option<u64> {
        match self {
            Op::Read { node_id, .. }
            | Op::Write { node_id, .. }
            | Op::Invoke { node_id, .. }
            | Op::On { node_id, .. }
            | Op::Off { node_id, .. }
            | Op::ColorTemp { node_id, .. }
            | Op::Level { node_id, .. }
            | Op::Color { node_id, .. }
            | Op::Describe { node_id } => Some(*node_id),
            Op::GroupProvision { .. }
            | Op::GroupInvoke { .. }
            | Op::GroupColorTemp { .. }
            | Op::GroupLevel { .. }
            | Op::GroupColor { .. }
            | Op::GroupBump
            | Op::Listen { .. }
            | Op::Ping
            | Op::Status
            | Op::Shutdown
            | Op::NodeTouched { .. } => None,
        }
    }

    /// 構造化ログ用の op 名。ソケットプロトコルの `op` タグと同じ snake_case
    /// （`Op` は `Deserialize` のみなのでタグ文字列を再利用できず、手書きの
    /// 網羅 match にする — op 追加時にコンパイラが漏れを強制する）。
    pub fn name(&self) -> &'static str {
        match self {
            Op::Read { .. } => "read",
            Op::Write { .. } => "write",
            Op::Invoke { .. } => "invoke",
            Op::On { .. } => "on",
            Op::Off { .. } => "off",
            Op::ColorTemp { .. } => "color_temp",
            Op::Level { .. } => "level",
            Op::Color { .. } => "color",
            Op::Describe { .. } => "describe",
            Op::GroupProvision { .. } => "group_provision",
            Op::GroupInvoke { .. } => "group_invoke",
            Op::GroupColorTemp { .. } => "group_color_temp",
            Op::GroupLevel { .. } => "group_level",
            Op::GroupColor { .. } => "group_color",
            Op::GroupBump => "group_bump",
            Op::Listen { .. } => "listen",
            Op::Ping => "ping",
            Op::Status => "status",
            Op::Shutdown => "shutdown",
            Op::NodeTouched { .. } => "node_touched",
        }
    }

    /// group 系 op の対象 group_id（ログ用）。node_id を持たない op の識別に使う。
    pub fn group_id(&self) -> Option<u16> {
        match self {
            Op::GroupProvision { group_id, .. }
            | Op::GroupInvoke { group_id, .. }
            | Op::GroupColorTemp { group_id, .. }
            | Op::GroupLevel { group_id, .. }
            | Op::GroupColor { group_id, .. } => Some(*group_id),
            Op::Read { .. }
            | Op::Write { .. }
            | Op::Invoke { .. }
            | Op::On { .. }
            | Op::Off { .. }
            | Op::ColorTemp { .. }
            | Op::Level { .. }
            | Op::Color { .. }
            | Op::Describe { .. }
            | Op::GroupBump
            | Op::Listen { .. }
            | Op::Ping
            | Op::Status
            | Op::Shutdown
            | Op::NodeTouched { .. } => None,
        }
    }

    /// ログ用の endpoint。`Listen` の `endpoint` は `Option` だが、この op は
    /// dispatch に到達しない（`handle_conn` が先取りする）ので None を返す。
    pub fn endpoint(&self) -> Option<u16> {
        match self {
            Op::Read { endpoint, .. }
            | Op::Write { endpoint, .. }
            | Op::Invoke { endpoint, .. }
            | Op::On { endpoint, .. }
            | Op::Off { endpoint, .. }
            | Op::ColorTemp { endpoint, .. }
            | Op::Level { endpoint, .. }
            | Op::Color { endpoint, .. }
            | Op::GroupProvision { endpoint, .. }
            | Op::GroupInvoke { endpoint, .. }
            | Op::GroupColorTemp { endpoint, .. }
            | Op::GroupLevel { endpoint, .. }
            | Op::GroupColor { endpoint, .. } => Some(*endpoint),
            Op::Describe { .. }
            | Op::GroupBump
            | Op::Listen { .. }
            | Op::Ping
            | Op::Status
            | Op::Shutdown
            | Op::NodeTouched { .. } => None,
        }
    }

    /// ログ用の対象パス。属性系は `cluster/attribute`、コマンド系は
    /// `cluster/command`。op 名だけで足りるショートカットは None。
    /// （フィールド名に `target` を使わないのは tracing のマクロが `target:` を
    /// 特別扱いするため。）
    pub fn log_path(&self) -> Option<String> {
        match self {
            Op::Read {
                cluster, attribute, ..
            } => Some(format!("{cluster}/{}", attribute.as_deref().unwrap_or("*"))),
            Op::Write {
                cluster, attribute, ..
            } => Some(format!("{cluster}/{attribute}")),
            Op::Invoke {
                cluster, command, ..
            }
            | Op::GroupInvoke {
                cluster, command, ..
            } => Some(format!("{cluster}/{command}")),
            Op::On { .. }
            | Op::Off { .. }
            | Op::ColorTemp { .. }
            | Op::Level { .. }
            | Op::Color { .. }
            | Op::Describe { .. }
            | Op::GroupProvision { .. }
            | Op::GroupColorTemp { .. }
            | Op::GroupLevel { .. }
            | Op::GroupColor { .. }
            | Op::GroupBump
            | Op::Listen { .. }
            | Op::Ping
            | Op::Status
            | Op::Shutdown
            | Op::NodeTouched { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Request {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn read_request_parses() {
        let r = parse(
            r#"{"id":7,"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"}"#,
        );
        assert_eq!(r.id, Some(serde_json::json!(7)));
        assert_eq!(r.op.node_id(), Some(1));
        assert!(matches!(
            r.op,
            Op::Read {
                node_id: 1,
                endpoint: 1,
                ..
            }
        ));
    }

    #[test]
    fn invoke_parses_args_in_order() {
        let r = parse(
            r#"{"op":"invoke","node_id":2,"endpoint":1,"cluster":"levelcontrol","command":"move-to-level","args":["128","0","0","0"]}"#,
        );
        assert_eq!(r.op.node_id(), Some(2));
        assert!(matches!(
            r.op,
            Op::Invoke { ref args, .. } if args == &["128", "0", "0", "0"]
        ));
    }

    #[test]
    fn write_request_parses() {
        let r = parse(
            r#"{"op":"write","node_id":4,"endpoint":1,"cluster":"levelcontrol","attribute":"on-level","value":"128"}"#,
        );
        assert_eq!(r.op.node_id(), Some(4));
        assert!(matches!(
            r.op,
            Op::Write { ref value, .. } if value == "128"
        ));
    }

    #[test]
    fn on_off_shortcuts() {
        let on = parse(r#"{"op":"on","node_id":3,"endpoint":1}"#);
        assert_eq!(on.op.node_id(), Some(3));
        assert!(matches!(on.op, Op::On { .. }));
        let off = parse(r#"{"op":"off","node_id":3,"endpoint":1}"#);
        assert!(matches!(off.op, Op::Off { .. }));
    }

    #[test]
    fn color_temp_shortcut_parses() {
        // mireds は mat 側で換算済み。kelvin は応答エコー用。
        let r = parse(
            r#"{"op":"color_temp","node_id":6,"endpoint":1,"mireds":370,"kelvin":2700,"transition":30}"#,
        );
        assert_eq!(r.op.node_id(), Some(6));
        assert!(matches!(r.op, Op::ColorTemp { mireds: 370, .. }));
    }

    #[test]
    fn level_shortcut_parses() {
        // level は mat 側で換算済み。percent は応答エコー用。
        let r = parse(
            r#"{"op":"level","node_id":6,"endpoint":1,"level":127,"percent":50,"transition":30}"#,
        );
        assert_eq!(r.op.node_id(), Some(6));
        assert!(matches!(r.op, Op::Level { level: 127, .. }));
        let g =
            parse(r#"{"op":"group_level","group_id":10,"level":254,"percent":100,"endpoint":1}"#);
        assert_eq!(g.op.node_id(), None);
        assert!(matches!(
            g.op,
            Op::GroupLevel {
                level: 254,
                transition: 0,
                ..
            }
        ));
    }

    #[test]
    fn color_shortcut_parses() {
        // hue_raw / saturation_raw は mat 側で換算済みの 0–254 値。hue / saturation
        // （度・%）は応答エコー用。
        let r = parse(
            r#"{"op":"color","node_id":6,"endpoint":1,"hue_raw":233,"saturation_raw":203,"hue":330,"saturation":80,"transition":30}"#,
        );
        assert_eq!(r.op.node_id(), Some(6));
        assert!(matches!(
            r.op,
            Op::Color {
                hue_raw: 233,
                saturation_raw: 203,
                ..
            }
        ));
    }

    #[test]
    fn ping_has_no_node() {
        let r = parse(r#"{"op":"ping"}"#);
        assert_eq!(r.op.node_id(), None);
        assert!(matches!(r.op, Op::Ping));
    }

    #[test]
    fn invoke_args_default_empty() {
        let r = parse(
            r#"{"op":"invoke","node_id":1,"endpoint":1,"cluster":"identify","command":"identify"}"#,
        );
        assert!(matches!(r.op, Op::Invoke { ref args, .. } if args.is_empty()));
    }

    #[test]
    fn describe_targets_node() {
        let r = parse(r#"{"op":"describe","node_id":5}"#);
        assert_eq!(r.op.node_id(), Some(5));
    }

    #[test]
    fn group_provision_parses_and_has_no_single_node() {
        let r = parse(
            r#"{"op":"group_provision","group_id":1,"node_ids":[1,2],"keyset_id":42,"name":"living","endpoint":1}"#,
        );
        // 複数ノードを個別に扱うため単一 node_id は持たない。
        assert_eq!(r.op.node_id(), None);
        // epoch_key は省略可。
        assert!(matches!(
            r.op,
            Op::GroupProvision {
                epoch_key: None,
                ..
            }
        ));
    }

    #[test]
    fn group_provision_rebind_defaults_false_and_parses_true() {
        // 旧 mat からの op（rebind フィールド無し）は false に落ちる（後方互換）。
        let r = parse(
            r#"{"op":"group_provision","group_id":1,"node_ids":[1],"keyset_id":42,"name":"g","endpoint":1}"#,
        );
        assert!(matches!(r.op, Op::GroupProvision { rebind: false, .. }));

        let r = parse(
            r#"{"op":"group_provision","group_id":1,"node_ids":[1],"keyset_id":42,"name":"g","endpoint":1,"rebind":true}"#,
        );
        assert!(matches!(r.op, Op::GroupProvision { rebind: true, .. }));
    }

    #[test]
    fn group_invoke_parses_with_default_args() {
        let r = parse(
            r#"{"op":"group_invoke","group_id":3,"cluster":"onoff","command":"on","endpoint":1}"#,
        );
        assert_eq!(r.op.node_id(), None);
        assert!(matches!(r.op, Op::GroupInvoke { ref args, .. } if args.is_empty()));
    }

    #[test]
    fn listen_parses_with_all_filters_optional() {
        let r = parse(r#"{"op":"listen"}"#);
        assert_eq!(r.op.node_id(), None);
        assert!(matches!(
            r.op,
            Op::Listen {
                node_id: None,
                endpoint: None,
                cluster: None,
                attribute: None
            }
        ));
        let r = parse(
            r#"{"op":"listen","node_id":21,"endpoint":1,"cluster":"occupancysensing","attribute":"occupancy"}"#,
        );
        assert!(matches!(
            r.op,
            Op::Listen {
                node_id: Some(21),
                endpoint: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn shutdown_has_no_node() {
        // admin op。native backend には触れないので単一 node を持たない。
        let r = parse(r#"{"op":"shutdown"}"#);
        assert!(matches!(r.op, Op::Shutdown));
        assert_eq!(r.op.node_id(), None);
    }

    #[test]
    fn group_color_temp_parses_with_no_node() {
        let r = parse(
            r#"{"op":"group_color_temp","group_id":1,"mireds":370,"kelvin":2700,"transition":0,"endpoint":1}"#,
        );
        // multicast 宛で単一 node を持たず、group_invoke と同じく専用ハンドラで捌く。
        assert_eq!(r.op.node_id(), None);
    }

    #[test]
    fn group_color_parses_with_optional_name_and_rgb() {
        let r = parse(
            r##"{"op":"group_color","group_id":1,"hue_raw":169,"saturation_raw":254,"hue":240,"saturation":100,"name":"blue","rgb":"#0000ff","endpoint":1}"##,
        );
        assert_eq!(r.op.node_id(), None);
        assert!(matches!(r.op, Op::GroupColor { name: Some(_), .. }));
        // name / rgb は省略可（--hue/--sat 生指定のとき）。
        let r = parse(
            r#"{"op":"group_color","group_id":1,"hue_raw":233,"saturation_raw":203,"hue":330,"saturation":80,"endpoint":1}"#,
        );
        assert!(matches!(
            r.op,
            Op::GroupColor {
                name: None,
                rgb: None,
                ..
            }
        ));
    }

    #[test]
    fn color_accepts_optional_name_and_rgb_echo() {
        // 単体 color も name / rgb エコーを受ける。
        let r = parse(
            r##"{"op":"color","node_id":6,"endpoint":1,"hue_raw":0,"saturation_raw":254,"hue":0,"saturation":100,"name":"red","rgb":"#ff0000"}"##,
        );
        assert!(matches!(
            r.op,
            Op::Color {
                name: Some(ref n),
                rgb: Some(ref rgb),
                ..
            } if n == "red" && rgb == "#ff0000"
        ));
    }

    #[test]
    fn name_matches_the_wire_op_tag() {
        for (line, expected) in [
            (
                r#"{"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"}"#,
                "read",
            ),
            (
                r#"{"op":"color_temp","node_id":1,"endpoint":1,"mireds":300,"kelvin":3333}"#,
                "color_temp",
            ),
            (
                r#"{"op":"group_invoke","group_id":10,"cluster":"onoff","command":"on","endpoint":1}"#,
                "group_invoke",
            ),
            (r#"{"op":"ping"}"#, "ping"),
            (r#"{"op":"shutdown"}"#, "shutdown"),
        ] {
            assert_eq!(parse(line).op.name(), expected, "line: {line}");
        }
    }

    #[test]
    fn group_bump_parses_with_no_fields() {
        let r = parse(r#"{"op":"group_bump"}"#);
        assert!(matches!(r.op, Op::GroupBump));
        // fabric 全体で counter は 1 本 — node / group / endpoint を持たない。
        assert_eq!(r.op.node_id(), None);
        assert_eq!(r.op.group_id(), None);
        assert_eq!(r.op.endpoint(), None);
        assert_eq!(r.op.log_path(), None);
        assert_eq!(r.op.name(), "group_bump");
    }

    #[test]
    fn log_accessors_pick_the_right_fields() {
        let read = parse(
            r#"{"op":"read","node_id":6,"endpoint":1,"cluster":"onoff","attribute":"on-off"}"#,
        )
        .op;
        assert_eq!(read.node_id(), Some(6));
        assert_eq!(read.group_id(), None);
        assert_eq!(read.endpoint(), Some(1));
        assert_eq!(read.log_path().as_deref(), Some("onoff/on-off"));

        let group_invoke = parse(
            r#"{"op":"group_invoke","group_id":10,"cluster":"onoff","command":"on","endpoint":1}"#,
        )
        .op;
        assert_eq!(group_invoke.node_id(), None);
        assert_eq!(group_invoke.group_id(), Some(10));
        assert_eq!(group_invoke.endpoint(), Some(1));
        assert_eq!(group_invoke.log_path().as_deref(), Some("onoff/on"));

        // ショートカット op は op 名だけで用が足りるので path を持たない。
        let on = parse(r#"{"op":"on","node_id":6,"endpoint":1}"#).op;
        assert_eq!(on.log_path(), None);
        assert_eq!(on.endpoint(), Some(1));

        // endpoint も group_id も持たない op。
        let ping = parse(r#"{"op":"ping"}"#).op;
        assert_eq!(ping.endpoint(), None);
        assert_eq!(ping.group_id(), None);
        assert_eq!(ping.log_path(), None);
    }

    #[test]
    fn deadline_ms_parses_and_defaults_to_none() {
        // 新 mat からの deadline 付きリクエスト。
        let r = parse(
            r#"{"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off","deadline_ms":15000}"#,
        );
        assert_eq!(r.deadline_ms, Some(15000));
        // 旧 mat（フィールド無し）は None。
        let r = parse(r#"{"op":"ping"}"#);
        assert_eq!(r.deadline_ms, None);
        // 明示 0（無制限）も値として通る。
        let r = parse(r#"{"op":"on","node_id":3,"endpoint":1,"deadline_ms":0}"#);
        assert_eq!(r.deadline_ms, Some(0));
    }

    #[test]
    fn unknown_top_level_fields_are_tolerated() {
        // 前方互換の釘打ち: 未知フィールドは無視される（新 mat → 旧 matd で
        // deadline_ms が未知でも parse_error にならないことの一般形）。
        let r = parse(
            r#"{"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off","future_field":42}"#,
        );
        assert!(matches!(r.op, Op::Read { .. }));
    }

    #[test]
    fn status_has_no_node_and_matches_wire_tag() {
        // admin op（`matd status` が送る）。native にもデバイスにも触れない。
        let r = parse(r#"{"op":"status"}"#);
        assert!(matches!(r.op, Op::Status));
        assert_eq!(r.op.node_id(), None);
        assert_eq!(r.op.group_id(), None);
        assert_eq!(r.op.endpoint(), None);
        assert_eq!(r.op.log_path(), None);
        assert_eq!(r.op.name(), "status");
    }

    #[test]
    fn read_without_attribute_parses_as_wildcard() {
        let req: Request =
            serde_json::from_str(r#"{"op":"read","node_id":1,"endpoint":1,"cluster":"onoff"}"#)
                .unwrap();
        assert!(matches!(
            req.op,
            Op::Read {
                attribute: None,
                ..
            }
        ));
        assert_eq!(req.op.log_path().as_deref(), Some("onoff/*"));
    }
}
