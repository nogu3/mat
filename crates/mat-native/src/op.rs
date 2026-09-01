//! op → TLV → 成功 body の単一ソース（監査④）。
//!
//! `mat`（one-shot 直経路）と `matd`（warm セッション）の両方がここを通る。
//! 値はすべて解決済み（cluster/attribute/command は数値 ID、色・色温度・
//! level は raw 値、`*_in` は応答エコー用の入力文字列）。名前解決と換算の
//! 規則はこのモジュールのコンストラクタだけが持つ。

use mat_core::color::ResolvedColor;
use mat_core::error::MatError;
use mat_core::ids::{self, InvokeClass, ScalarValue, WriteClass};

/// 経路非依存の入力換算（CLI 入力 → Matter 生値）。旧 `mat/src/units.rs`。
pub mod units {
    /// `--kelvin` / `--mireds`（排他・どちらか必須）を `(mireds, kelvin)` に
    /// 解決する。与えられなかった側は `round(1_000_000 / x)` で補完し、出力
    /// JSON へのエコーに使う。デバイス対応範囲の検証はしない。
    pub fn resolve_color_temp(kelvin: Option<u32>, mireds: Option<u16>) -> (u16, u32) {
        fn recip(v: u32) -> u32 {
            (1_000_000 + v / 2) / v
        }
        match (kelvin, mireds) {
            // CLI の値域制約（16..=1_000_000 K）により mireds は u16 に収まる。
            (Some(k), None) => (recip(k) as u16, k),
            (None, Some(m)) => (m, recip(u32::from(m))),
            _ => unreachable!("clap enforces exactly one of --kelvin / --mireds"),
        }
    }

    /// `--percent`（0–100）を LevelControl の 0–254 生値へ（255 は予約値）。
    pub fn resolve_level(percent: u8) -> u8 {
        ((u32::from(percent) * 254 + 50) / 100) as u8
    }
}

/// 単一ノード宛 op。
#[derive(Debug, Clone, PartialEq)]
pub struct NodeOp {
    pub node_id: u64,
    pub kind: NodeOpKind,
}

/// 単一ノード宛 op の種別。値は解決済み。
#[derive(Debug, Clone, PartialEq)]
pub enum NodeOpKind {
    On {
        endpoint: u16,
    },
    Off {
        endpoint: u16,
    },
    Color {
        endpoint: u16,
        color: ResolvedColor,
        transition: u16,
    },
    ColorTemp {
        endpoint: u16,
        kelvin: u32,
        mireds: u16,
        transition: u16,
    },
    Level {
        endpoint: u16,
        percent: u8,
        level: u8,
        transition: u16,
    },
    Read {
        endpoint: u16,
        cluster_in: String,
        attribute_in: String,
        cluster: u32,
        attribute: u32,
    },
    Write {
        endpoint: u16,
        cluster_in: String,
        attribute_in: String,
        cluster: u32,
        attribute: u32,
        value_in: String,
        value: ScalarValue,
        timed: bool,
    },
    Invoke {
        endpoint: u16,
        cluster_in: String,
        command_in: String,
        /// wire（matd）へ引数を名前のまま渡すためのエコー。
        args_in: Vec<String>,
        cluster: u32,
        command: u32,
        fields_tlv: Option<Vec<u8>>,
        timed: bool,
    },
    Describe,
    DiagThread {
        endpoint: u16,
    },
    OpenWindow {
        timeout: u32,
        iteration: u32,
        discriminator: u16,
    },
}

/// `classify_invoke` の結果を (cluster, command, fields_tlv, timed) に写す共通部。
#[allow(clippy::type_complexity)]
fn resolve_invoke(
    cluster_in: &str,
    command_in: &str,
    args: &[String],
) -> Result<(u32, u32, Option<Vec<u8>>, bool), MatError> {
    match ids::classify_invoke(cluster_in, command_in, args) {
        InvokeClass::NotNative => Err(MatError::unresolved_op()),
        InvokeClass::Reject(msg) => Err(MatError::parse_error(msg)),
        InvokeClass::Native {
            cluster,
            command,
            fields,
            timed,
        } => {
            let fields_tlv = if fields.is_empty() {
                None
            } else {
                Some(crate::encode_command_fields(&fields))
            };
            Ok((cluster, command, fields_tlv, timed))
        }
    }
}

impl NodeOpKind {
    /// 名前（または数値 ID）解決。未解決は `unresolved_op`（parse_error）。
    pub fn read(endpoint: u16, cluster_in: &str, attribute_in: &str) -> Result<Self, MatError> {
        let cluster = ids::resolve_cluster(cluster_in).ok_or_else(MatError::unresolved_op)?;
        let attr =
            ids::resolve_attribute(cluster, attribute_in).ok_or_else(MatError::unresolved_op)?;
        Ok(NodeOpKind::Read {
            endpoint,
            cluster_in: cluster_in.to_string(),
            attribute_in: attribute_in.to_string(),
            cluster,
            attribute: attr.id,
        })
    }

    /// 名前解決 + 値のスカラー化。`NotNative` = 未解決、`Reject` = 符号化不能。
    pub fn write(
        endpoint: u16,
        cluster_in: &str,
        attribute_in: &str,
        value_in: &str,
    ) -> Result<Self, MatError> {
        match ids::classify_write(cluster_in, attribute_in, value_in) {
            WriteClass::NotNative => Err(MatError::unresolved_op()),
            WriteClass::Reject(msg) => Err(MatError::parse_error(msg)),
            WriteClass::Native {
                cluster,
                attribute,
                value,
                timed,
            } => Ok(NodeOpKind::Write {
                endpoint,
                cluster_in: cluster_in.to_string(),
                attribute_in: attribute_in.to_string(),
                cluster,
                attribute,
                value_in: value_in.to_string(),
                value,
                timed,
            }),
        }
    }

    /// 名前解決 + 引数のスカラー化 → CommandFields TLV。
    pub fn invoke(
        endpoint: u16,
        cluster_in: &str,
        command_in: &str,
        args: &[String],
    ) -> Result<Self, MatError> {
        let (cluster, command, fields_tlv, timed) = resolve_invoke(cluster_in, command_in, args)?;
        Ok(NodeOpKind::Invoke {
            endpoint,
            cluster_in: cluster_in.to_string(),
            command_in: command_in.to_string(),
            args_in: args.to_vec(),
            cluster,
            command,
            fields_tlv,
            timed,
        })
    }

    pub fn color_temp(
        endpoint: u16,
        kelvin: Option<u32>,
        mireds: Option<u16>,
        transition: u16,
    ) -> Self {
        let (mireds, kelvin) = units::resolve_color_temp(kelvin, mireds);
        NodeOpKind::ColorTemp {
            endpoint,
            kelvin,
            mireds,
            transition,
        }
    }

    pub fn level(endpoint: u16, percent: u8, transition: u16) -> Self {
        NodeOpKind::Level {
            endpoint,
            percent,
            level: units::resolve_level(percent),
            transition,
        }
    }

    /// `--op-timeout-ms` / matd `deadline_ms` の対象か（単一ノードの
    /// read/write/invoke/on/off/color 系/level/describe のみ）。
    pub fn budget_applies(&self) -> bool {
        !matches!(
            self,
            NodeOpKind::DiagThread { .. } | NodeOpKind::OpenWindow { .. }
        )
    }

    /// ログ用の op 名（wire の snake_case タグと同じ）。
    pub fn name(&self) -> &'static str {
        match self {
            NodeOpKind::On { .. } => "on",
            NodeOpKind::Off { .. } => "off",
            NodeOpKind::Color { .. } => "color",
            NodeOpKind::ColorTemp { .. } => "color_temp",
            NodeOpKind::Level { .. } => "level",
            NodeOpKind::Read { .. } => "read",
            NodeOpKind::Write { .. } => "write",
            NodeOpKind::Invoke { .. } => "invoke",
            NodeOpKind::Describe => "describe",
            NodeOpKind::DiagThread { .. } => "diag_thread",
            NodeOpKind::OpenWindow { .. } => "open_window",
        }
    }
}

/// groupcast op（unacknowledged、"sent" のみ報告）。
#[derive(Debug, Clone, PartialEq)]
pub struct GroupOp {
    pub group_id: u16,
    pub endpoint: u16,
    pub kind: GroupOpKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GroupOpKind {
    Invoke {
        cluster_in: String,
        command_in: String,
        args_in: Vec<String>,
        cluster: u32,
        command: u32,
        fields_tlv: Option<Vec<u8>>,
    },
    Color {
        color: ResolvedColor,
        transition: u16,
    },
    ColorTemp {
        kelvin: u32,
        mireds: u16,
        transition: u16,
    },
    Level {
        percent: u8,
        level: u8,
        transition: u16,
    },
}

impl GroupOpKind {
    /// 単体 invoke と同じ解決規則（timed は groupcast に無いので捨てる）。
    pub fn invoke(cluster_in: &str, command_in: &str, args: &[String]) -> Result<Self, MatError> {
        let (cluster, command, fields_tlv, _timed) = resolve_invoke(cluster_in, command_in, args)?;
        Ok(GroupOpKind::Invoke {
            cluster_in: cluster_in.to_string(),
            command_in: command_in.to_string(),
            args_in: args.to_vec(),
            cluster,
            command,
            fields_tlv,
        })
    }

    pub fn color_temp(kelvin: Option<u32>, mireds: Option<u16>, transition: u16) -> Self {
        let (mireds, kelvin) = units::resolve_color_temp(kelvin, mireds);
        GroupOpKind::ColorTemp {
            kelvin,
            mireds,
            transition,
        }
    }

    pub fn level(percent: u8, transition: u16) -> Self {
        GroupOpKind::Level {
            percent,
            level: units::resolve_level(percent),
            transition,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            GroupOpKind::Invoke { .. } => "group_invoke",
            GroupOpKind::Color { .. } => "group_color",
            GroupOpKind::ColorTemp { .. } => "group_color_temp",
            GroupOpKind::Level { .. } => "group_level",
        }
    }
}

/// `group provision` の入力（直経路・matd 共通）。`epoch_key` は 32 桁 hex
/// または None（ランダム生成）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProvisionParams {
    pub group_id: u16,
    pub node_ids: Vec<u64>,
    pub keyset_id: u16,
    pub name: String,
    pub endpoint: u16,
    pub epoch_key: Option<String>,
    pub rebind: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::im::{CLUSTER_ON_OFF, CMD_ON_OFF_ON, CMD_ON_OFF_TOGGLE};
    use mat_core::error::ErrorKind;

    #[test]
    fn kelvin_2700_converts_to_370_mireds() {
        assert_eq!(units::resolve_color_temp(Some(2700), None), (370, 2700));
    }

    #[test]
    fn kelvin_6500_rounds_to_154_mireds() {
        assert_eq!(units::resolve_color_temp(Some(6500), None), (154, 6500));
    }

    #[test]
    fn mireds_direct_computes_kelvin_echo() {
        assert_eq!(units::resolve_color_temp(None, Some(370)), (370, 2703));
    }

    #[test]
    fn resolve_level_rounds_percent_to_254_scale() {
        assert_eq!(units::resolve_level(0), 0);
        assert_eq!(units::resolve_level(1), 3);
        assert_eq!(units::resolve_level(50), 127);
        assert_eq!(units::resolve_level(100), 254);
    }

    #[test]
    fn read_resolves_names_and_numeric_ids() {
        let k = NodeOpKind::read(1, "levelcontrol", "current-level").unwrap();
        assert!(matches!(
            k,
            NodeOpKind::Read {
                endpoint: 1,
                cluster: 0x0008,
                attribute: 0x0000,
                ..
            }
        ));
        let k = NodeOpKind::read(1, "0x0008", "0").unwrap();
        assert!(matches!(
            k,
            NodeOpKind::Read {
                cluster: 0x0008,
                attribute: 0,
                ..
            }
        ));
        let err = NodeOpKind::read(1, "nosuchcluster", "x").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(
            err.detail.contains("numeric IDs are accepted"),
            "{}",
            err.detail
        );
    }

    #[test]
    fn write_scalar_ok_list_rejected_unknown_unresolved() {
        let k = NodeOpKind::write(1, "levelcontrol", "on-level", "128").unwrap();
        assert!(matches!(
            k,
            NodeOpKind::Write {
                cluster: 0x0008,
                value: ScalarValue::UInt(128),
                timed: false,
                ..
            }
        ));
        let err = NodeOpKind::write(1, "accesscontrol", "acl", "[]").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.contains("list"), "{}", err.detail);
        let err = NodeOpKind::write(1, "nosuch", "x", "1").unwrap_err();
        assert!(
            err.detail.contains("numeric IDs are accepted"),
            "{}",
            err.detail
        );
    }

    #[test]
    fn invoke_scalar_args_ok_struct_args_rejected() {
        let args: Vec<String> = vec!["128".into(), "0".into(), "0".into(), "0".into()];
        let k = NodeOpKind::invoke(1, "levelcontrol", "move-to-level", &args).unwrap();
        assert!(matches!(
            k,
            NodeOpKind::Invoke {
                cluster: 0x0008,
                fields_tlv: Some(_),
                ..
            }
        ));
        let k = NodeOpKind::invoke(1, "onoff", "on", &[]).unwrap();
        assert!(matches!(
            k,
            NodeOpKind::Invoke {
                cluster: CLUSTER_ON_OFF,
                command: CMD_ON_OFF_ON,
                fields_tlv: None,
                ..
            }
        ));
        let err = NodeOpKind::invoke(1, "groupkeymanagement", "key-set-write", &["{}".into()])
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[test]
    fn group_invoke_resolves_like_node_invoke() {
        let k = GroupOpKind::invoke("onoff", "toggle", &[]).unwrap();
        assert!(matches!(
            k,
            GroupOpKind::Invoke {
                cluster: CLUSTER_ON_OFF,
                command: CMD_ON_OFF_TOGGLE,
                fields_tlv: None,
                ..
            }
        ));
        let err = GroupOpKind::invoke("onoff", "on", &["1".into()]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        let err = GroupOpKind::invoke("onoff", "foo", &[]).unwrap_err();
        assert!(
            err.detail.contains("numeric IDs are accepted"),
            "{}",
            err.detail
        );
    }

    #[test]
    fn color_temp_and_level_constructors_convert_units() {
        assert_eq!(
            NodeOpKind::color_temp(1, Some(2700), None, 0),
            NodeOpKind::ColorTemp {
                endpoint: 1,
                kelvin: 2700,
                mireds: 370,
                transition: 0
            }
        );
        assert_eq!(
            NodeOpKind::level(1, 50, 0),
            NodeOpKind::Level {
                endpoint: 1,
                percent: 50,
                level: 127,
                transition: 0
            }
        );
        assert_eq!(
            GroupOpKind::color_temp(None, Some(370), 5),
            GroupOpKind::ColorTemp {
                kelvin: 2703,
                mireds: 370,
                transition: 5
            }
        );
        assert_eq!(
            GroupOpKind::level(100, 0),
            GroupOpKind::Level {
                percent: 100,
                level: 254,
                transition: 0
            }
        );
    }

    #[test]
    fn budget_applies_only_to_single_node_hotpath_ops() {
        assert!(NodeOpKind::On { endpoint: 1 }.budget_applies());
        assert!(NodeOpKind::Off { endpoint: 1 }.budget_applies());
        assert!(NodeOpKind::level(1, 1, 0).budget_applies());
        assert!(NodeOpKind::color_temp(1, Some(2700), None, 0).budget_applies());
        assert!(NodeOpKind::read(1, "onoff", "on-off")
            .unwrap()
            .budget_applies());
        assert!(NodeOpKind::write(1, "onoff", "on-off", "true")
            .unwrap()
            .budget_applies());
        assert!(NodeOpKind::invoke(1, "onoff", "on", &[])
            .unwrap()
            .budget_applies());
        assert!(NodeOpKind::Describe.budget_applies());
        assert!(!NodeOpKind::DiagThread { endpoint: 0 }.budget_applies());
        assert!(!NodeOpKind::OpenWindow {
            timeout: 180,
            iteration: 1000,
            discriminator: 1
        }
        .budget_applies());
    }

    #[test]
    fn names_are_snake_case_wire_tags() {
        assert_eq!(NodeOpKind::On { endpoint: 1 }.name(), "on");
        assert_eq!(
            NodeOpKind::color_temp(1, Some(2700), None, 0).name(),
            "color_temp"
        );
        assert_eq!(
            NodeOpKind::OpenWindow {
                timeout: 1,
                iteration: 1,
                discriminator: 1
            }
            .name(),
            "open_window"
        );
        assert_eq!(GroupOpKind::level(1, 0).name(), "group_level");
    }
}
