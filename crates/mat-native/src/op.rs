//! op → TLV → 成功 body の単一ソース（監査④）。
//!
//! `mat`（one-shot 直経路）と `matd`（warm セッション）の両方がここを通る。
//! 値はすべて解決済み（cluster/attribute/command は数値 ID、色・色温度・
//! level は raw 値、`*_in` は応答エコー用の入力文字列）。名前解決と換算の
//! 規則はこのモジュールのコンストラクタだけが持つ。

use crate::NodeConn;
use mat_controller::commissioning;
use mat_controller::im;
use mat_core::body;
use mat_core::color::ResolvedColor;
use mat_core::error::{ErrorKind, MatError};
use mat_core::ids::{self, InvokeClass, ScalarValue, WriteClass};
use serde_json::Value;

use crate::group::{self, BumpOutcome, GroupOutcome};
use crate::Engine;

/// OperationalCredentials / CurrentFabricIndex（属性 0x0005）。commissioning.rs は
/// コマンド定数しか持たないのでここで局所定義する（im.rs はレーン C が触るため
/// 足さない）。
const ATTR_CURRENT_FABRIC_INDEX: u32 = 0x0005;

/// 経路非依存の入力換算（CLI 入力 → Matter 生値）。旧 `mat/src/units.rs`。
pub(crate) mod units {
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
    /// `mat unpair` のデバイス側: CurrentFabricIndex を読んでその index を
    /// RemoveFabric する（直経路専用 — 台帳の書き手は mat だけ）。
    RemoveFabric,
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
            NodeOpKind::RemoveFabric => "remove_fabric",
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

/// 単一ノード op を 1 回実行し、成功 body（timestamp 抜き）を返す。
/// op → NodeConn 呼び出し（TLV 符号化）→ body 組立はここだけ。セッションの
/// 取得・後始末は呼び出し側（`runner`）の責務。
pub async fn run_node_op(conn: &mut dyn NodeConn, op: &NodeOp) -> Result<Value, MatError> {
    let node_id = op.node_id;
    let body = match &op.kind {
        NodeOpKind::On { endpoint } => {
            conn.invoke(
                *endpoint,
                im::CLUSTER_ON_OFF,
                im::CMD_ON_OFF_ON,
                None,
                false,
            )
            .await?;
            body::invoke_success(node_id, *endpoint, "onoff", "on")
        }
        NodeOpKind::Off { endpoint } => {
            conn.invoke(
                *endpoint,
                im::CLUSTER_ON_OFF,
                im::CMD_ON_OFF_OFF,
                None,
                false,
            )
            .await?;
            body::invoke_success(node_id, *endpoint, "onoff", "off")
        }
        NodeOpKind::Color {
            endpoint,
            color,
            transition,
        } => {
            let fields = im::encode_move_to_hue_and_saturation_fields(
                color.hue_raw,
                color.sat_raw,
                *transition,
            );
            conn.invoke(
                *endpoint,
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_HUE_AND_SATURATION,
                Some(fields),
                false,
            )
            .await?;
            body::color_success(node_id, *endpoint, color, *transition)
        }
        NodeOpKind::ColorTemp {
            endpoint,
            kelvin,
            mireds,
            transition,
        } => {
            let fields = im::encode_move_to_color_temperature_fields(*mireds, *transition);
            conn.invoke(
                *endpoint,
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_COLOR_TEMPERATURE,
                Some(fields),
                false,
            )
            .await?;
            body::color_temp_success(node_id, *endpoint, *kelvin, *mireds, *transition)
        }
        NodeOpKind::Level {
            endpoint,
            percent,
            level,
            transition,
        } => {
            let fields = im::encode_move_to_level_fields(*level, *transition);
            conn.invoke(
                *endpoint,
                im::CLUSTER_LEVEL_CONTROL,
                im::CMD_MOVE_TO_LEVEL,
                Some(fields),
                false,
            )
            .await?;
            body::level_success(
                node_id,
                *endpoint,
                body::LevelEcho {
                    percent: *percent,
                    level: *level,
                },
                *transition,
            )
        }
        NodeOpKind::Read {
            endpoint,
            cluster_in,
            attribute_in,
            cluster,
            attribute,
        } => {
            // onoff/on-off は bool 専用 read（両経路の従来挙動）。数値 ID 指定
            // （6/0）も同じ腕に落ちるが JSON は Bool で同形。
            let v = if *cluster == im::CLUSTER_ON_OFF && *attribute == im::ATTR_ON_OFF {
                Value::Bool(conn.read_onoff(*endpoint).await?)
            } else {
                conn.read_json(*endpoint, *cluster, *attribute).await?
            };
            body::read_success(node_id, *endpoint, cluster_in, attribute_in, v)
        }
        NodeOpKind::Write {
            endpoint,
            cluster_in,
            attribute_in,
            cluster,
            attribute,
            value_in,
            value,
            timed,
        } => {
            conn.write_tlv(
                *endpoint,
                *cluster,
                *attribute,
                crate::scalar_to_tlv(value),
                *timed,
            )
            .await?;
            body::write_success(node_id, *endpoint, cluster_in, attribute_in, value_in)
        }
        NodeOpKind::Invoke {
            endpoint,
            cluster_in,
            command_in,
            cluster,
            command,
            fields_tlv,
            timed,
            ..
        } => {
            conn.invoke(*endpoint, *cluster, *command, fields_tlv.clone(), *timed)
                .await?;
            body::invoke_success(node_id, *endpoint, cluster_in, command_in)
        }
        NodeOpKind::Describe => {
            let endpoints = crate::ops::describe(conn).await?;
            body::describe_success(node_id, &endpoints)
        }
        NodeOpKind::DiagThread { endpoint } => {
            let snap = crate::ops::diag_thread(conn, *endpoint).await?;
            body::diag_thread_success(node_id, *endpoint, snap.fields, &snap.unavailable)
        }
        NodeOpKind::OpenWindow {
            timeout,
            iteration,
            discriminator,
        } => {
            // CLI の timeout は u32、window API は u16（spec 上 16-bit）。飽和。
            let timeout_u16 = u16::try_from(*timeout).unwrap_or(u16::MAX);
            let (manual_code, qr_payload) = conn
                .open_window(timeout_u16, *discriminator, *iteration)
                .await?;
            body::open_window_success(node_id, &manual_code, &qr_payload, *timeout)
        }
        NodeOpKind::RemoveFabric => {
            let v = conn
                .read_json(
                    0,
                    commissioning::CLUSTER_OPERATIONAL_CREDENTIALS,
                    ATTR_CURRENT_FABRIC_INDEX,
                )
                .await?;
            let idx = v
                .as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or_else(|| {
                    MatError::parse_error(format!("current-fabric-index is not a u8: {v}"))
                })?;
            let resp = conn
                .invoke_for_data(
                    0,
                    commissioning::CLUSTER_OPERATIONAL_CREDENTIALS,
                    commissioning::CMD_REMOVE_FABRIC,
                    Some(commissioning::encode_remove_fabric(idx)),
                    false,
                )
                .await?;
            let (status, _) = commissioning::decode_noc_response(&resp)
                .map_err(|e| MatError::parse_error(format!("RemoveFabric response: {e}")))?;
            if status != 0 {
                return Err(MatError::new(
                    ErrorKind::DeviceRejected,
                    format!(
                        "RemoveFabric rejected by node {node_id}: NOCResponse status {status:#04x} (fabric_index {idx})"
                    ),
                ));
            }
            body::unpair_device(idx)
        }
    };
    tracing::debug!(node_id, op = op.kind.name(), "node op executed");
    Ok(body)
}

impl GroupOpKind {
    /// 送出する (cluster, command, CommandFields TLV)。
    pub fn wire(&self) -> (u32, u32, Option<Vec<u8>>) {
        match self {
            GroupOpKind::Invoke {
                cluster,
                command,
                fields_tlv,
                ..
            } => (*cluster, *command, fields_tlv.clone()),
            GroupOpKind::Color { color, transition } => (
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_HUE_AND_SATURATION,
                Some(im::encode_move_to_hue_and_saturation_fields(
                    color.hue_raw,
                    color.sat_raw,
                    *transition,
                )),
            ),
            GroupOpKind::ColorTemp {
                mireds, transition, ..
            } => (
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_COLOR_TEMPERATURE,
                Some(im::encode_move_to_color_temperature_fields(
                    *mireds,
                    *transition,
                )),
            ),
            GroupOpKind::Level {
                level, transition, ..
            } => (
                im::CLUSTER_LEVEL_CONTROL,
                im::CMD_MOVE_TO_LEVEL,
                Some(im::encode_move_to_level_fields(*level, *transition)),
            ),
        }
    }
}

impl GroupOp {
    /// 送出後の "sent" body。`egress` は実送出した iface 名。
    pub fn sent_body(&self, egress: &[String]) -> Value {
        match &self.kind {
            GroupOpKind::Invoke {
                cluster_in,
                command_in,
                ..
            } => body::group_invoke_sent(
                self.group_id,
                cluster_in,
                command_in,
                self.endpoint,
                egress,
            ),
            GroupOpKind::Color { color, transition } => {
                body::group_color_sent(self.group_id, color, *transition, self.endpoint, egress)
            }
            GroupOpKind::ColorTemp {
                kelvin,
                mireds,
                transition,
            } => body::group_color_temp_sent(
                self.group_id,
                *kelvin,
                *mireds,
                *transition,
                self.endpoint,
                egress,
            ),
            GroupOpKind::Level {
                percent,
                level,
                transition,
            } => body::group_level_sent(
                self.group_id,
                body::LevelEcho {
                    percent: *percent,
                    level: *level,
                },
                *transition,
                self.endpoint,
                egress,
            ),
        }
    }
}

/// groupcast を 1 発送り "sent" body を返す。`engine.group` 未構成（テスト
/// 注入時のみ）は Other、未 provision / KVS 不備は `store_parse`。
pub async fn run_group_op(engine: &Engine, op: &GroupOp) -> Result<Value, MatError> {
    let Some(ctx) = &engine.group else {
        return Err(MatError::group_ctx_unconfigured());
    };
    let (cluster, command, fields) = op.kind.wire();
    match group::send(ctx, op.group_id, cluster, command, fields).await? {
        GroupOutcome::Sent { egress } => {
            tracing::debug!(group_id = op.group_id, op = op.kind.name(), "group op sent");
            Ok(op.sent_body(&egress))
        }
        GroupOutcome::Unavailable(reason) => Err(MatError::group_unavailable(&reason)),
    }
}

/// group 送信 counter の窓ジャンプ（Issue #14）。
pub async fn run_group_bump(engine: &Engine) -> Result<Value, MatError> {
    let Some(ctx) = &engine.group else {
        return Err(MatError::group_ctx_unconfigured());
    };
    match group::bump(ctx).await {
        BumpOutcome::Bumped { from, to } => Ok(body::group_bump(from, to)),
        BumpOutcome::Unavailable(reason) => Err(MatError::group_unavailable(&reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakeConn;
    use mat_controller::im::{self, CLUSTER_ON_OFF, CMD_ON_OFF_ON, CMD_ON_OFF_TOGGLE};
    use mat_core::error::ErrorKind;
    use serde_json::json;

    fn node(kind: NodeOpKind) -> NodeOp {
        NodeOp { node_id: 5, kind }
    }

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
    fn write_scalar_ok_bad_json_shape_rejected_unknown_unresolved() {
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
        let err = NodeOpKind::write(1, "accesscontrol", "acl", "{}").unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(
            err.detail.contains("expected a JSON array"),
            "{}",
            err.detail
        );
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

    #[tokio::test]
    async fn on_off_invoke_onoff_and_build_invoke_body() {
        let mut conn = FakeConn::default();
        let body = run_node_op(&mut conn, &node(NodeOpKind::On { endpoint: 1 }))
            .await
            .unwrap();
        assert_eq!(body, mat_core::body::invoke_success(5, 1, "onoff", "on"));
        let body = run_node_op(&mut conn, &node(NodeOpKind::Off { endpoint: 1 }))
            .await
            .unwrap();
        assert_eq!(body, mat_core::body::invoke_success(5, 1, "onoff", "off"));
        assert_eq!(
            conn.calls(),
            &[
                format!(
                    "invoke(1,{:#06X},{:#06X})",
                    im::CLUSTER_ON_OFF,
                    im::CMD_ON_OFF_ON
                ),
                format!(
                    "invoke(1,{:#06X},{:#06X})",
                    im::CLUSTER_ON_OFF,
                    im::CMD_ON_OFF_OFF
                ),
            ]
        );
    }

    #[tokio::test]
    async fn color_color_temp_level_send_expected_commands() {
        let mut conn = FakeConn::default();
        let color = ResolvedColor {
            hue_raw: 233,
            sat_raw: 203,
            hue: 330,
            sat: 80,
            name: None,
            rgb: None,
        };
        let body = run_node_op(
            &mut conn,
            &node(NodeOpKind::Color {
                endpoint: 1,
                color: color.clone(),
                transition: 30,
            }),
        )
        .await
        .unwrap();
        assert_eq!(body, mat_core::body::color_success(5, 1, &color, 30));
        let body = run_node_op(
            &mut conn,
            &node(NodeOpKind::color_temp(1, Some(2700), None, 0)),
        )
        .await
        .unwrap();
        assert_eq!(body, mat_core::body::color_temp_success(5, 1, 2700, 370, 0));
        let body = run_node_op(&mut conn, &node(NodeOpKind::level(1, 50, 0)))
            .await
            .unwrap();
        assert_eq!(
            body,
            mat_core::body::level_success(
                5,
                1,
                mat_core::body::LevelEcho {
                    percent: 50,
                    level: 127
                },
                0
            )
        );
        assert_eq!(
            conn.calls(),
            &[
                format!(
                    "invoke(1,{:#06X},{:#06X})",
                    im::CLUSTER_COLOR_CONTROL,
                    im::CMD_MOVE_TO_HUE_AND_SATURATION
                ),
                format!(
                    "invoke(1,{:#06X},{:#06X})",
                    im::CLUSTER_COLOR_CONTROL,
                    im::CMD_MOVE_TO_COLOR_TEMPERATURE
                ),
                format!(
                    "invoke(1,{:#06X},{:#06X})",
                    im::CLUSTER_LEVEL_CONTROL,
                    im::CMD_MOVE_TO_LEVEL
                ),
            ]
        );
    }

    #[tokio::test]
    async fn read_onoff_uses_bool_fast_path_and_generic_read_uses_json() {
        // FakeConn::read_onoff は常に true、read_json は登録値（未登録は 1）。
        let mut conn = FakeConn::scripted().with_read(1, 0x0008, 0x0000, json!(200));
        let body = run_node_op(
            &mut conn,
            &node(NodeOpKind::read(1, "onoff", "on-off").unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(
            body,
            mat_core::body::read_success(5, 1, "onoff", "on-off", json!(true))
        );
        let body = run_node_op(
            &mut conn,
            &node(NodeOpKind::read(1, "levelcontrol", "current-level").unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(body["value"], json!(200));
        assert_eq!(body["cluster"], "levelcontrol");
        assert_eq!(body["attribute"], "current-level");
    }

    #[tokio::test]
    async fn write_encodes_scalar_tlv_and_echoes_normalized_value() {
        let mut conn = FakeConn::default();
        let op = node(NodeOpKind::write(1, "levelcontrol", "on-level", "128").unwrap());
        let body = run_node_op(&mut conn, &op).await.unwrap();
        assert_eq!(
            body,
            mat_core::body::write_success(5, 1, "levelcontrol", "on-level", "128")
        );
        let (ep, cluster, attr, tlv) = &conn.written_tlv()[0];
        assert_eq!((*ep, *cluster, *attr), (1, 0x0008, 0x0011));
        assert_eq!(tlv, &crate::scalar_to_tlv(&ScalarValue::UInt(128)));
    }

    #[tokio::test]
    async fn invoke_generic_forwards_ids_and_builds_body() {
        let mut conn = FakeConn::default();
        let args: Vec<String> = vec!["128".into(), "0".into(), "0".into(), "0".into()];
        let op = node(NodeOpKind::invoke(1, "levelcontrol", "move-to-level", &args).unwrap());
        let body = run_node_op(&mut conn, &op).await.unwrap();
        assert_eq!(
            body,
            mat_core::body::invoke_success(5, 1, "levelcontrol", "move-to-level")
        );
        assert_eq!(
            conn.calls(),
            &[format!(
                "invoke(1,{:#06X},{:#06X})",
                im::CLUSTER_LEVEL_CONTROL,
                im::CMD_MOVE_TO_LEVEL
            )]
        );
    }

    #[tokio::test]
    async fn describe_diag_thread_and_open_window_build_bodies() {
        let mut conn = FakeConn::scripted().with_cluster(
            0,
            0x0035,
            vec![(0x0007, json!([{"5": 200}, {"5": 100}]))],
        );
        let body = run_node_op(&mut conn, &node(NodeOpKind::Describe))
            .await
            .unwrap();
        assert_eq!(body["node_id"], 5);
        assert!(body["endpoints"].is_array());

        let body = run_node_op(&mut conn, &node(NodeOpKind::DiagThread { endpoint: 0 }))
            .await
            .unwrap();
        assert_eq!(body["endpoint"], 0);
        assert!(body["thread"].is_object());

        let body = run_node_op(
            &mut conn,
            &node(NodeOpKind::OpenWindow {
                timeout: 180,
                iteration: 1000,
                discriminator: 3840,
            }),
        )
        .await
        .unwrap();
        assert_eq!(body["manual_code"], "34970112332");
        assert!(body["qr_payload"].as_str().unwrap().starts_with("MT:"));
        assert!(body["expires_at"].is_string());
    }

    fn noc_response_tlv(status: u8, fabric_index: Option<u8>) -> Vec<u8> {
        use mat_controller::tlv::{Tag, Writer};
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), u64::from(status));
        if let Some(idx) = fabric_index {
            w.put_uint(Tag::Context(1), u64::from(idx));
        }
        w.end_container();
        w.finish()
    }

    #[tokio::test]
    async fn remove_fabric_reads_current_index_then_invokes_and_reports_it() {
        let mut conn = FakeConn::scripted()
            .with_read(0, 0x003E, 0x0005, serde_json::json!(2))
            .with_invoke_response(0, 0x003E, 0x0A, noc_response_tlv(0, Some(2)));
        let body = run_node_op(&mut conn, &node(NodeOpKind::RemoveFabric))
            .await
            .unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "removed": true, "fabric_index": 2 })
        );
        assert_eq!(
            conn.calls(),
            &["invoke_for_data(0,0x003E,0x000A)".to_string()]
        );
    }

    #[tokio::test]
    async fn remove_fabric_non_zero_status_is_device_rejected() {
        let mut conn = FakeConn::scripted()
            .with_read(0, 0x003E, 0x0005, serde_json::json!(2))
            .with_invoke_response(0, 0x003E, 0x0A, noc_response_tlv(0x0B, None));
        let err = run_node_op(&mut conn, &node(NodeOpKind::RemoveFabric))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
        assert!(err.detail.contains("0x0b"), "{}", err.detail);
    }

    #[test]
    fn remove_fabric_name_and_budget() {
        assert_eq!(NodeOpKind::RemoveFabric.name(), "remove_fabric");
        assert!(NodeOpKind::RemoveFabric.budget_applies());
    }

    #[tokio::test]
    async fn conn_error_propagates_unchanged() {
        let mut conn = FakeConn::default();
        conn.fail_first_send = true;
        conn.fail_kind = ErrorKind::Timeout;
        let err = run_node_op(
            &mut conn,
            &node(NodeOpKind::read(1, "onoff", "on-off").unwrap()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Timeout);
    }

    #[test]
    fn group_wire_and_sent_body_for_shortcuts() {
        let ct = GroupOp {
            group_id: 10,
            endpoint: 1,
            kind: GroupOpKind::color_temp(Some(2702), None, 0),
        };
        let (cluster, command, fields) = ct.kind.wire();
        assert_eq!(
            (cluster, command),
            (im::CLUSTER_COLOR_CONTROL, im::CMD_MOVE_TO_COLOR_TEMPERATURE)
        );
        assert_eq!(
            fields.unwrap(),
            im::encode_move_to_color_temperature_fields(370, 0)
        );
        assert_eq!(
            ct.sent_body(&["eth0".into()]),
            mat_core::body::group_color_temp_sent(10, 2702, 370, 0, 1, &["eth0".to_string()])
        );

        let lv = GroupOp {
            group_id: 10,
            endpoint: 1,
            kind: GroupOpKind::level(100, 0),
        };
        let (cluster, command, fields) = lv.kind.wire();
        assert_eq!(
            (cluster, command),
            (im::CLUSTER_LEVEL_CONTROL, im::CMD_MOVE_TO_LEVEL)
        );
        assert_eq!(fields.unwrap(), im::encode_move_to_level_fields(254, 0));

        let color = ResolvedColor {
            hue_raw: 180,
            sat_raw: 200,
            hue: 254,
            sat: 78,
            name: None,
            rgb: None,
        };
        let c = GroupOp {
            group_id: 10,
            endpoint: 1,
            kind: GroupOpKind::Color {
                color: color.clone(),
                transition: 0,
            },
        };
        let (cluster, command, fields) = c.kind.wire();
        assert_eq!(
            (cluster, command),
            (
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_HUE_AND_SATURATION
            )
        );
        assert_eq!(
            fields.unwrap(),
            im::encode_move_to_hue_and_saturation_fields(180, 200, 0)
        );
        assert_eq!(
            c.sent_body(&[]),
            mat_core::body::group_color_sent(10, &color, 0, 1, &[])
        );

        let inv = GroupOp {
            group_id: 10,
            endpoint: 1,
            kind: GroupOpKind::invoke("onoff", "on", &[]).unwrap(),
        };
        assert_eq!(
            inv.kind.wire(),
            (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None)
        );
        assert_eq!(
            inv.sent_body(&[]),
            mat_core::body::group_invoke_sent(10, "onoff", "on", 1, &[])
        );
    }

    #[tokio::test]
    async fn group_op_hard_errors_when_engine_group_ctx_unconfigured() {
        use crate::test_support::FakeEstablisher;
        let engine = crate::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        let op = GroupOp {
            group_id: 10,
            endpoint: 1,
            kind: GroupOpKind::invoke("onoff", "toggle", &[]).unwrap(),
        };
        let err = run_group_op(&engine, &op)
            .await
            .expect_err("group ctx unconfigured must hard-error");
        assert_eq!(err.kind, ErrorKind::Other);
        let err = run_group_bump(&engine)
            .await
            .expect_err("bump without ctx must hard-error");
        assert_eq!(err.kind, ErrorKind::Other);
    }

    #[tokio::test]
    async fn group_bump_advances_counter_via_engine() {
        // 旧 native_direct::tests::group_bump_advances_counter_via_engine の移植。
        use crate::group::GroupCtx;
        use crate::test_support::{write_group_fixture_ini, FakeEstablisher};
        use mat_controller::transport::UdpTransport;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        write_group_fixture_ini(&ini);
        let counter_path = dir.path().join("native_group_counter");
        let transport = Arc::new(UdpTransport::bind().await.unwrap());
        let group_ctx = GroupCtx {
            main_ini: ini,
            counter_path: counter_path.clone(),
            fabric_index: 2,
            fabric_id: 1,
            node_id: 0x0001_0001,
            egress: vec![mat_controller::group::GroupEgress {
                iface: "lo".into(),
                transport,
                scope_id: 1,
            }],
            dest_port: 5540,
            op_iface: "lo".into(),
            thread_retry: false,
            sender: Mutex::new(None),
        };
        let engine =
            crate::Engine::with_parts(Box::new(FakeEstablisher::default()), Some(group_ctx));
        assert!(!counter_path.exists());
        let body = run_group_bump(&engine)
            .await
            .expect("bump must succeed when ctx is configured");
        assert!(body["group_counter"]["from"].is_number());
        assert!(body["group_counter"]["to"].is_number());
        assert!(
            counter_path.exists(),
            "counter file must be created/advanced by bump"
        );
    }
}
