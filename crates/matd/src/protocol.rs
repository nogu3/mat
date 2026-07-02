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

/// 操作種別。chip-tool のサブコマンド体系に 1:1 で対応する。
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
    /// ノードのエンドポイント / クラスタを introspect する（Descriptor クラスタ）。
    /// 1 リクエストで複数の chip-tool 読み出しに展開されるため [`to_cmdline`] は持たない。
    ///
    /// [`to_cmdline`]: Op::to_cmdline
    Describe { node_id: u64 },
    /// group（groupcast）を各ノードへ provision する。`mat group provision` 相当。
    /// 複数ステップに展開されるため [`to_cmdline`] は持たない。
    ///
    /// [`to_cmdline`]: Op::to_cmdline
    GroupProvision {
        group_id: u16,
        node_ids: Vec<u64>,
        keyset_id: u16,
        name: String,
        endpoint: u16,
        #[serde(default)]
        epoch_key: Option<String>,
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
    /// デーモン死活確認（chip-tool には触れない）。
    Ping,
    /// デーモンを停止する admin op（chip-tool には触れない）。`matd stop` が送る。
    /// Ping と同じく node も cmdline も持たない。
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
            | Op::Describe { node_id } => Some(*node_id),
            Op::GroupProvision { .. } | Op::GroupInvoke { .. } | Op::Ping | Op::Shutdown => None,
        }
    }

    /// chip-tool interactive server に送るコマンド行（バイナリ名を除いた argv）。
    ///
    /// one-shot の `chip-tool <cluster> <cmd> ... <node> <ep>` と同じ語順。chip-tool
    /// は宛先 node_id / endpoint を**末尾**に取るので、コマンド引数はその前に置く。
    pub fn to_cmdline(&self) -> Option<String> {
        let line = match self {
            Op::Read {
                node_id,
                endpoint,
                cluster,
                attribute,
            } => format!("{cluster} read {attribute} {node_id} {endpoint}"),
            Op::Write {
                node_id,
                endpoint,
                cluster,
                attribute,
                value,
            } => format!("{cluster} write {attribute} {value} {node_id} {endpoint}"),
            Op::Invoke {
                node_id,
                endpoint,
                cluster,
                command,
                args,
            } => {
                let mut parts = vec![cluster.clone(), command.clone()];
                parts.extend(args.iter().cloned());
                parts.push(node_id.to_string());
                parts.push(endpoint.to_string());
                parts.join(" ")
            }
            Op::On { node_id, endpoint } => format!("onoff on {node_id} {endpoint}"),
            Op::Off { node_id, endpoint } => format!("onoff off {node_id} {endpoint}"),
            // 引数は <mireds> <transition> <optionsMask> <optionsOverride>、宛先は末尾。
            Op::ColorTemp {
                node_id,
                endpoint,
                mireds,
                transition,
                ..
            } => format!(
                "colorcontrol move-to-color-temperature {mireds} {transition} 0 0 {node_id} {endpoint}"
            ),
            // 複合 op（複数コマンドに展開）と Ping は単一の cmdline を持たない。
            Op::Describe { .. }
            | Op::GroupProvision { .. }
            | Op::GroupInvoke { .. }
            | Op::Ping
            | Op::Shutdown => return None,
        };
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Request {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn read_request_parses_and_builds_cmdline() {
        let r = parse(
            r#"{"id":7,"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"}"#,
        );
        assert_eq!(r.id, Some(serde_json::json!(7)));
        assert_eq!(r.op.node_id(), Some(1));
        assert_eq!(r.op.to_cmdline().unwrap(), "onoff read on-off 1 1");
    }

    #[test]
    fn invoke_places_args_before_destination() {
        let r = parse(
            r#"{"op":"invoke","node_id":2,"endpoint":1,"cluster":"levelcontrol","command":"move-to-level","args":["128","0","0","0"]}"#,
        );
        // 引数は node_id/endpoint の前。誤って宛先扱いされると timeout する。
        assert_eq!(
            r.op.to_cmdline().unwrap(),
            "levelcontrol move-to-level 128 0 0 0 2 1"
        );
    }

    #[test]
    fn write_request_builds_cmdline() {
        let r = parse(
            r#"{"op":"write","node_id":4,"endpoint":1,"cluster":"levelcontrol","attribute":"on-level","value":"128"}"#,
        );
        assert_eq!(r.op.node_id(), Some(4));
        assert_eq!(
            r.op.to_cmdline().unwrap(),
            "levelcontrol write on-level 128 4 1"
        );
    }

    #[test]
    fn on_off_shortcuts() {
        let on = parse(r#"{"op":"on","node_id":3,"endpoint":1}"#);
        assert_eq!(on.op.to_cmdline().unwrap(), "onoff on 3 1");
        let off = parse(r#"{"op":"off","node_id":3,"endpoint":1}"#);
        assert_eq!(off.op.to_cmdline().unwrap(), "onoff off 3 1");
    }

    #[test]
    fn color_temp_shortcut_builds_move_to_color_temperature_cmdline() {
        // mireds は mat 側で換算済み。kelvin は応答エコー用で cmdline には乗らない。
        let r = parse(
            r#"{"op":"color_temp","node_id":6,"endpoint":1,"mireds":370,"kelvin":2700,"transition":30}"#,
        );
        assert_eq!(r.op.node_id(), Some(6));
        assert_eq!(
            r.op.to_cmdline().unwrap(),
            "colorcontrol move-to-color-temperature 370 30 0 0 6 1"
        );
    }

    #[test]
    fn ping_has_no_node_or_cmdline() {
        let r = parse(r#"{"op":"ping"}"#);
        assert_eq!(r.op.node_id(), None);
        assert!(r.op.to_cmdline().is_none());
    }

    #[test]
    fn invoke_args_default_empty() {
        let r = parse(
            r#"{"op":"invoke","node_id":1,"endpoint":1,"cluster":"identify","command":"identify"}"#,
        );
        assert_eq!(r.op.to_cmdline().unwrap(), "identify identify 1 1");
    }

    #[test]
    fn describe_targets_node_but_has_no_cmdline() {
        // describe は複数コマンドに展開されるため単一 cmdline を持たない。
        let r = parse(r#"{"op":"describe","node_id":5}"#);
        assert_eq!(r.op.node_id(), Some(5));
        assert!(r.op.to_cmdline().is_none());
    }

    #[test]
    fn group_provision_parses_and_has_no_single_node_or_cmdline() {
        let r = parse(
            r#"{"op":"group_provision","group_id":1,"node_ids":[1,2],"keyset_id":42,"name":"living","endpoint":1}"#,
        );
        // 複数ノードを個別に扱うため単一 node_id は持たない。
        assert_eq!(r.op.node_id(), None);
        assert!(r.op.to_cmdline().is_none());
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
    fn group_invoke_parses_with_default_args() {
        let r = parse(
            r#"{"op":"group_invoke","group_id":3,"cluster":"onoff","command":"on","endpoint":1}"#,
        );
        assert_eq!(r.op.node_id(), None);
        assert!(r.op.to_cmdline().is_none());
        assert!(matches!(r.op, Op::GroupInvoke { ref args, .. } if args.is_empty()));
    }

    #[test]
    fn shutdown_has_no_node_or_cmdline() {
        // admin op。chip-tool には触れないので node_id も cmdline も持たない。
        let r = parse(r#"{"op":"shutdown"}"#);
        assert!(matches!(r.op, Op::Shutdown));
        assert_eq!(r.op.node_id(), None);
        assert!(r.op.to_cmdline().is_none());
    }
}
