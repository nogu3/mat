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
    #[serde(flatten)]
    pub op: Op,
}

/// 操作種別。Matter クラスタコマンド体系に 1:1 で対応する。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// 属性を読む。
    Read {
        node_id: u64,
        endpoint: u16,
        cluster: String,
        attribute: String,
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
    /// デーモン死活確認（native backend には触れない）。
    Ping,
    /// デーモンを停止する admin op（native backend には触れない）。`matd stop` が送る。
    /// Ping と同じく単一 node は持たない。
    Shutdown,
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
            | Op::Color { node_id, .. }
            | Op::Describe { node_id } => Some(*node_id),
            Op::GroupProvision { .. }
            | Op::GroupInvoke { .. }
            | Op::GroupColorTemp { .. }
            | Op::GroupColor { .. }
            | Op::Ping
            | Op::Shutdown => None,
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
}
