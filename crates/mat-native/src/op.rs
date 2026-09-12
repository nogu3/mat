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
use mat_core::ids::{self, ArgValue, InvokeClass, WriteClass};
use serde_json::Value;

use crate::group::{self, BumpOutcome, GroupOutcome};
use crate::Engine;

/// OperationalCredentials / CurrentFabricIndex（属性 0x0005）。commissioning.rs は
/// コマンド定数しか持たないのでここで局所定義する（`mat_controller::im` には
/// 足さない）。
const ATTR_CURRENT_FABRIC_INDEX: u32 = 0x0005;

/// 値ツリー（`mat_core::ids::ArgValue`）を 1 要素の TLV として `w` に書く。
/// List → TLV Array（属性 list の型。TLV List 0x17 は path 専用）、Struct →
/// TLV Struct（context tag = fieldId、呼び出し側で id 昇順整列済み）。
pub fn put_value(
    w: &mut mat_controller::tlv::Writer,
    tag: mat_controller::tlv::Tag,
    v: &mat_core::ids::ArgValue,
) {
    use mat_controller::tlv::Tag;
    use mat_core::ids::ArgValue as V;
    match v {
        V::Bool(b) => w.put_bool(tag, *b),
        V::UInt(n) => w.put_uint(tag, *n),
        V::Int(n) => w.put_int(tag, *n),
        V::F32(f) => w.put_f32(tag, *f),
        V::F64(f) => w.put_f64(tag, *f),
        V::Str(s) => w.put_str(tag, s),
        V::Bytes(b) => w.put_bytes(tag, b),
        V::Null => w.put_null(tag),
        V::List(items) => {
            w.start_array(tag);
            for item in items {
                put_value(w, Tag::Anonymous, item);
            }
            w.end_container();
        }
        V::Struct(fields) => {
            w.start_struct(tag);
            for (id, val) in fields {
                put_value(w, Tag::Context(*id), val);
            }
            w.end_container();
        }
    }
}

/// `ArgValue` を Anonymous タグの単一 TLV 要素へ（`write_tlv`/
/// `write_attribute_tlv` に渡す形。呼び出し側がトップレベルタグを再付与する）。
pub fn arg_value_to_tlv(v: &mat_core::ids::ArgValue) -> Vec<u8> {
    let mut w = mat_controller::tlv::Writer::new();
    put_value(&mut w, mat_controller::tlv::Tag::Anonymous, v);
    w.finish()
}

/// invoke のコマンド引数（値ツリーの列）を CommandFields TLV へ。context tag は
/// 引数添字（0-based、`CmdDef::fields` の添字と一致 — `mat_core::ids` のコメント
/// 参照）。mat 直経路 (`native_direct`) / matd (`server::native_op`) の両方が使う
/// 共有ヘルパ（M8a Task10 で mat 側から移設・一本化）。
pub fn encode_command_fields(args: &[mat_core::ids::ArgValue]) -> Vec<u8> {
    use mat_controller::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    for (i, v) in args.iter().enumerate() {
        put_value(&mut w, Tag::Context(i as u8), v);
    }
    w.end_container();
    w.finish()
}

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
    /// cluster 内の全属性を wildcard read（`--attribute` 省略）。
    ReadCluster {
        endpoint: u16,
        cluster_in: String,
        cluster: u32,
    },
    Write {
        endpoint: u16,
        cluster_in: String,
        attribute_in: String,
        cluster: u32,
        attribute: u32,
        value_in: String,
        value: ArgValue,
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
                Some(encode_command_fields(&fields))
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

    /// cluster 名（または数値 ID）だけを解決する wildcard read。
    pub fn read_cluster(endpoint: u16, cluster_in: &str) -> Result<Self, MatError> {
        let cluster = ids::resolve_cluster(cluster_in).ok_or_else(MatError::unresolved_op)?;
        Ok(NodeOpKind::ReadCluster {
            endpoint,
            cluster_in: cluster_in.to_string(),
            cluster,
        })
    }

    /// 名前解決 + 値のスカラー化。`NotNative` = 未解決、`Reject` = 符号化不能。
    ///
    /// `timed_override` は true への上書きのみ（表が true なら常に true）。
    pub fn write(
        endpoint: u16,
        cluster_in: &str,
        attribute_in: &str,
        value_in: &str,
        timed_override: bool,
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
                timed: timed || timed_override,
            }),
        }
    }

    /// 名前解決 + 引数のスカラー化 → CommandFields TLV。
    ///
    /// `timed_override` は true への上書きのみ（表が true なら常に true）。
    pub fn invoke(
        endpoint: u16,
        cluster_in: &str,
        command_in: &str,
        args: &[String],
        timed_override: bool,
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
            timed: timed || timed_override,
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
            NodeOpKind::ReadCluster { .. } => "read_cluster",
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

/// ショートカット op（color / color-temp / level）の (cluster, command,
/// CommandFields) 三つ組。単一ノード（`run_node_op`）と groupcast
/// （`GroupOpKind::wire`）が同じワイヤを出すことをここで保証する。
pub(crate) mod shortcut {
    use mat_controller::im;
    use mat_core::color::ResolvedColor;

    pub fn color(color: &ResolvedColor, transition: u16) -> (u32, u32, Option<Vec<u8>>) {
        (
            im::CLUSTER_COLOR_CONTROL,
            im::CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(im::encode_move_to_hue_and_saturation_fields(
                color.hue_raw,
                color.sat_raw,
                transition,
            )),
        )
    }

    pub fn color_temp(mireds: u16, transition: u16) -> (u32, u32, Option<Vec<u8>>) {
        (
            im::CLUSTER_COLOR_CONTROL,
            im::CMD_MOVE_TO_COLOR_TEMPERATURE,
            Some(im::encode_move_to_color_temperature_fields(
                mireds, transition,
            )),
        )
    }

    pub fn level(level: u8, transition: u16) -> (u32, u32, Option<Vec<u8>>) {
        (
            im::CLUSTER_LEVEL_CONTROL,
            im::CMD_MOVE_TO_LEVEL,
            Some(im::encode_move_to_level_fields(level, transition)),
        )
    }
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
            let (cluster, command, fields) = shortcut::color(color, *transition);
            conn.invoke(*endpoint, cluster, command, fields, false)
                .await?;
            body::color_success(node_id, *endpoint, color, *transition)
        }
        NodeOpKind::ColorTemp {
            endpoint,
            kelvin,
            mireds,
            transition,
        } => {
            let (cluster, command, fields) = shortcut::color_temp(*mireds, *transition);
            conn.invoke(*endpoint, cluster, command, fields, false)
                .await?;
            body::color_temp_success(node_id, *endpoint, *kelvin, *mireds, *transition)
        }
        NodeOpKind::Level {
            endpoint,
            percent,
            level,
            transition,
        } => {
            let (cluster, command, fields) = shortcut::level(*level, *transition);
            conn.invoke(*endpoint, cluster, command, fields, false)
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
        NodeOpKind::ReadCluster {
            endpoint,
            cluster_in,
            cluster,
        } => {
            let rows = conn.read_cluster(*endpoint, *cluster).await?;
            body::read_cluster_success(node_id, *endpoint, cluster_in, *cluster, rows)
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
                arg_value_to_tlv(value),
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
            GroupOpKind::Color { color, transition } => shortcut::color(color, *transition),
            GroupOpKind::ColorTemp {
                mireds, transition, ..
            } => shortcut::color_temp(*mireds, *transition),
            GroupOpKind::Level {
                level, transition, ..
            } => shortcut::level(*level, *transition),
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
mod tests;
