//! ワイヤ層: NDJSON 1 行の書き出し・エラー応答の形、そして `protocol::Op` → `mat_native::op`（[`to_device_op`]）と op → 購読レポート期待（[`note_op_expectation`]）の写像。

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use mat_controller::im;
use mat_core::error::MatError;
use mat_core::output::now_iso8601;

use crate::protocol::Op;
use crate::subscription::SubHealth;

/// wire `Op` → 解決済み op。名前解決・引数符号化の規則は `mat_native::op` の
/// コンストラクタ（mat 直経路と同一）。Ping / Shutdown / Listen / Status /
/// NodeTouched は `run_op` 冒頭 / `dispatch` / `handle_conn` が先取りするため
/// ここへは来ない（不変条件が破れても panic せず typed error）。
#[derive(Debug)]
pub(crate) enum MatdOp {
    Node(mat_native::op::NodeOp),
    Group(mat_native::op::GroupOp),
    Provision(mat_native::op::ProvisionParams),
    Bump,
}

pub(crate) fn to_device_op(op: &Op) -> Result<MatdOp, MatError> {
    use mat_native::op::{GroupOp, GroupOpKind, NodeOp, NodeOpKind, ProvisionParams};
    let node = |node_id: u64, kind: NodeOpKind| MatdOp::Node(NodeOp { node_id, kind });
    Ok(match op {
        Op::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => node(
            *node_id,
            match attribute {
                Some(a) => NodeOpKind::read(*endpoint, cluster, a)?,
                None => NodeOpKind::read_cluster(*endpoint, cluster)?,
            },
        ),
        Op::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
            timed,
        } => node(
            *node_id,
            NodeOpKind::write(*endpoint, cluster, attribute, value, *timed)?,
        ),
        Op::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            args,
            timed,
        } => node(
            *node_id,
            NodeOpKind::invoke(*endpoint, cluster, command, args, *timed)?,
        ),
        Op::On { node_id, endpoint } => node(
            *node_id,
            NodeOpKind::On {
                endpoint: *endpoint,
            },
        ),
        Op::Off { node_id, endpoint } => node(
            *node_id,
            NodeOpKind::Off {
                endpoint: *endpoint,
            },
        ),
        // 換算済み値が wire で届く（protocol.rs の約束）— struct リテラルで組む。
        Op::ColorTemp {
            node_id,
            endpoint,
            mireds,
            kelvin,
            transition,
        } => node(
            *node_id,
            NodeOpKind::ColorTemp {
                endpoint: *endpoint,
                kelvin: *kelvin,
                mireds: *mireds,
                transition: *transition,
            },
        ),
        Op::Level {
            node_id,
            endpoint,
            level,
            percent,
            transition,
        } => node(
            *node_id,
            NodeOpKind::Level {
                endpoint: *endpoint,
                percent: *percent,
                level: *level,
                transition: *transition,
            },
        ),
        Op::Color {
            node_id,
            endpoint,
            hue_raw,
            saturation_raw,
            hue,
            saturation,
            name,
            rgb,
            transition,
        } => node(
            *node_id,
            NodeOpKind::Color {
                endpoint: *endpoint,
                color: mat_core::color::ResolvedColor {
                    hue_raw: *hue_raw,
                    sat_raw: *saturation_raw,
                    hue: *hue,
                    sat: *saturation,
                    name: name.clone(),
                    rgb: rgb.clone(),
                },
                transition: *transition,
            },
        ),
        Op::Describe { node_id } => node(*node_id, NodeOpKind::Describe),
        Op::GroupProvision {
            group_id,
            node_ids,
            keyset_id,
            name,
            endpoint,
            epoch_key,
            rebind,
        } => MatdOp::Provision(ProvisionParams {
            group_id: *group_id,
            node_ids: node_ids.clone(),
            keyset_id: *keyset_id,
            name: name.clone(),
            endpoint: *endpoint,
            epoch_key: epoch_key.clone(),
            rebind: *rebind,
        }),
        Op::GroupInvoke {
            group_id,
            cluster,
            command,
            args,
            endpoint,
        } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::invoke(cluster, command, args)?,
        }),
        Op::GroupColorTemp {
            group_id,
            mireds,
            kelvin,
            transition,
            endpoint,
        } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::ColorTemp {
                kelvin: *kelvin,
                mireds: *mireds,
                transition: *transition,
            },
        }),
        Op::GroupLevel {
            group_id,
            level,
            percent,
            transition,
            endpoint,
        } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::Level {
                percent: *percent,
                level: *level,
                transition: *transition,
            },
        }),
        Op::GroupColor {
            group_id,
            hue_raw,
            saturation_raw,
            hue,
            saturation,
            name,
            rgb,
            transition,
            endpoint,
        } => MatdOp::Group(GroupOp {
            group_id: *group_id,
            endpoint: *endpoint,
            kind: GroupOpKind::Color {
                color: mat_core::color::ResolvedColor {
                    hue_raw: *hue_raw,
                    sat_raw: *saturation_raw,
                    hue: *hue,
                    sat: *saturation,
                    name: name.clone(),
                    rgb: rgb.clone(),
                },
                transition: *transition,
            },
        }),
        Op::GroupBump => MatdOp::Bump,
        Op::Listen { .. }
        | Op::Ping
        | Op::Status
        | Op::Shutdown
        | Op::NodeTouched { .. }
        | Op::Reload => {
            return Err(MatError::parse_error(
                "internal: non-device op reached to_device_op (dispatch invariant violated)",
            ))
        }
    })
}

/// 状態変更 op → (node_id, 変化が現れる cluster)。op 相関の born-dead 検知
/// （`SubHealth::note_op`）の根拠。
///
/// **「op が成功した」は「レポートが出るはず」を含意しない**: すでに目標状態に
/// あるデバイスへの On/Off/Level は data model が変化せず、Matter 仕様上
/// 購読レポートは出ない（レポートは属性変化時のみ）。よって購読キャッシュの
/// 現在値と目標値が**不一致の時だけ**期待を返す（spec 2026-07-24）。
/// キャッシュ欠落（matd 起動直後・購読未確立）は「証明できない」ので None。
/// Color / ColorTemp / Write / Invoke は変化を証明できないため対象外
/// （受け皿は無音 deadline）。Read / Describe / Group 系も元から None。
fn op_report_expectation(
    op: &Op,
    cached_on_off: Option<&Value>,
    cached_level: Option<&Value>,
) -> Option<(u64, u32)> {
    match op {
        // 現在 off の時だけ on は変化を生む。
        Op::On { node_id, .. } => {
            (!cached_on_off?.as_bool()?).then_some((*node_id, im::CLUSTER_ON_OFF))
        }
        // 現在 on の時だけ off は変化を生む。
        Op::Off { node_id, .. } => cached_on_off?
            .as_bool()?
            .then_some((*node_id, im::CLUSTER_ON_OFF)),
        // level は mat 側で換算済みの raw 0–254 が届く（protocol.rs の約束）。
        // MoveToLevel は OptionsMask/OptionsOverride = 0 で送られる
        // （encode_move_to_level_fields）ため、ExecuteIfOff 規則により OnOff=false
        // のデバイスでは実行されずレポートも出ない。確実に消灯中と分かる時
        // （cached_on_off = Some(false)）だけ打たない — 不明（None）を消灯と
        // 決めつけず、その場合は従来通り level の比較へ進む。
        Op::Level { node_id, level, .. } => {
            if cached_on_off.and_then(Value::as_bool) == Some(false) {
                return None;
            }
            (cached_level?.as_u64()? != u64::from(*level))
                .then_some((*node_id, im::CLUSTER_LEVEL_CONTROL))
        }
        _ => None,
    }
}

/// 期待判定に使うキャッシュの参照先 (node_id, endpoint)。On/Off/Level のみ。
///
/// 網羅 match（`_ => None` を使わない）: `Op` に新しい状態変更 op が増えたとき、
/// ここを更新し忘れるとコンパイルエラーで気付ける（`op_report_expectation` 側
/// だけ更新して静かに no-op 化するのを防ぐ — `Op::node_id()` と同じ書き方）。
fn op_state_target(op: &Op) -> Option<(u64, u16)> {
    match op {
        Op::On { node_id, endpoint } | Op::Off { node_id, endpoint } => Some((*node_id, *endpoint)),
        Op::Level {
            node_id, endpoint, ..
        } => Some((*node_id, *endpoint)),
        Op::Read { .. }
        | Op::Write { .. }
        | Op::Invoke { .. }
        | Op::ColorTemp { .. }
        | Op::Color { .. }
        | Op::Describe { .. }
        | Op::GroupProvision { .. }
        | Op::GroupInvoke { .. }
        | Op::GroupColorTemp { .. }
        | Op::GroupLevel { .. }
        | Op::GroupColor { .. }
        | Op::GroupBump
        | Op::Listen { .. }
        | Op::Ping
        | Op::Status
        | Op::Shutdown
        | Op::NodeTouched { .. }
        | Op::Reload => None,
    }
}

/// 成功した op に対し、レポート期待（pending）を打つべきなら打つ。
/// 購読の最終既知値を根拠にするので、no-op（すでに目標状態）では打たない。
pub(crate) fn note_op_expectation(op: &Op, health: &SubHealth) {
    let Some((node_id, endpoint)) = op_state_target(op) else {
        return;
    };
    let on_off = health.cached_value(node_id, endpoint, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF);
    let level = health.cached_value(
        node_id,
        endpoint,
        im::CLUSTER_LEVEL_CONTROL,
        im::ATTR_CURRENT_LEVEL,
    );
    if let Some((node_id, cluster)) = op_report_expectation(op, on_off.as_ref(), level.as_ref()) {
        health.note_op(node_id, cluster);
    }
}

/// エラー応答 `{"error":{"kind","detail"}, "id"?, "timestamp"}`。
pub(super) fn error_response(id: Option<Value>, e: &MatError) -> Value {
    let mut body = e.to_json();
    if let Value::Object(map) = &mut body {
        map.insert("timestamp".into(), json!(now_iso8601()));
        if let Some(id) = id {
            map.insert("id".into(), id);
        }
    }
    body
}

/// 応答 / イベント 1 件を NDJSON 1 行で書き、flush する。JSON 化不能（実質
/// 到達しない）は `{}` を書く — 行を欠かして相手の枚数勘定を狂わせない。
pub(super) async fn write_line(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    v: &Value,
) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    buf.push(b'\n');
    write_half.write_all(&buf).await?;
    write_half.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_core::error::ErrorKind;

    use crate::server::tests::group_on_op;

    use mat_native::op::{GroupOpKind, NodeOpKind};

    #[test]
    fn to_device_op_maps_node_ops_with_resolved_ids() {
        let m = to_device_op(&Op::On {
            node_id: 1,
            endpoint: 1,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Node(ref n) if n.node_id == 1 && n.kind == NodeOpKind::On { endpoint: 1 })
        );
        let m = to_device_op(&Op::ColorTemp {
            node_id: 1,
            endpoint: 1,
            mireds: 370,
            kelvin: 2700,
            transition: 0,
        })
        .unwrap();
        assert!(matches!(
            m,
            MatdOp::Node(ref n) if n.kind == NodeOpKind::ColorTemp { endpoint: 1, kelvin: 2700, mireds: 370, transition: 0 }
        ));
        let m = to_device_op(&Op::Level {
            node_id: 1,
            endpoint: 1,
            level: 127,
            percent: 50,
            transition: 0,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Node(ref n) if n.kind == NodeOpKind::Level { endpoint: 1, percent: 50, level: 127, transition: 0 })
        );
        let m = to_device_op(&Op::Color {
            node_id: 1,
            endpoint: 1,
            hue_raw: 0,
            saturation_raw: 254,
            hue: 0,
            saturation: 100,
            name: Some("red".into()),
            rgb: Some("#ff0000".into()),
            transition: 0,
        })
        .unwrap();
        match m {
            MatdOp::Node(n) => match n.kind {
                NodeOpKind::Color { color, .. } => {
                    assert_eq!(
                        (color.hue_raw, color.sat_raw, color.hue, color.sat),
                        (0, 254, 0, 100)
                    );
                    assert_eq!(color.name.as_deref(), Some("red"));
                    assert_eq!(color.rgb.as_deref(), Some("#ff0000"));
                }
                other => panic!("expected Color, got {other:?}"),
            },
            other => panic!("expected Node, got {other:?}"),
        }
        let m = to_device_op(&Op::Read {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: Some("current-level".into()),
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Node(ref n) if matches!(n.kind, NodeOpKind::Read { cluster: 0x0008, attribute: 0, .. }))
        );
        let m = to_device_op(&Op::Write {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "on-level".into(),
            value: "128".into(),
            timed: false,
        })
        .unwrap();
        assert!(matches!(m, MatdOp::Node(ref n) if matches!(n.kind, NodeOpKind::Write { .. })));
        let m = to_device_op(&Op::Invoke {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            command: "move-to-level".into(),
            args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
            timed: false,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Node(ref n) if matches!(n.kind, NodeOpKind::Invoke { fields_tlv: Some(_), .. }))
        );
        assert!(
            matches!(to_device_op(&Op::Describe { node_id: 5 }).unwrap(), MatdOp::Node(ref n) if n.kind == NodeOpKind::Describe)
        );
    }

    #[test]
    fn to_device_op_applies_timed_override() {
        let m = to_device_op(&Op::Invoke {
            node_id: 1,
            endpoint: 1,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec![],
            timed: true,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Node(ref n) if matches!(n.kind, NodeOpKind::Invoke { timed: true, .. }))
        );
    }

    #[test]
    fn to_device_op_rejects_unresolved_names_and_unencodable_values() {
        // 未知名 → unresolved_op（parse_error、数値 ID 案内付き）。
        let err = to_device_op(&Op::Read {
            node_id: 1,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            attribute: Some("x".into()),
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(
            err.detail.contains("numeric IDs are accepted"),
            "{}",
            err.detail
        );
        let err = to_device_op(&Op::Invoke {
            node_id: 1,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            command: "x".into(),
            args: vec![],
            timed: false,
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        // 名前は解決できるが JSON 形が不正（list 属性に struct）→ parse_error（classify の msg）。
        let err = to_device_op(&Op::Write {
            node_id: 1,
            endpoint: 1,
            cluster: "accesscontrol".into(),
            attribute: "acl".into(),
            value: "{}".into(),
            timed: false,
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(
            err.detail.contains("expected a JSON array"),
            "{}",
            err.detail
        );
    }

    #[test]
    fn to_device_op_maps_group_ops_and_shortcuts() {
        let m = to_device_op(&group_on_op()).unwrap();
        match m {
            MatdOp::Group(g) => {
                assert_eq!((g.group_id, g.endpoint), (10, 1));
                assert_eq!(g.kind.wire(), (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None));
            }
            other => panic!("expected Group, got {other:?}"),
        }
        // 引数過多（onoff on は 0 引数）は即 parse_error。
        let err = to_device_op(&Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "on".into(),
            args: vec!["1".into()],
            endpoint: 1,
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        // 未知コマンド名は unresolved_op。
        let err = to_device_op(&Op::GroupInvoke {
            group_id: 10,
            cluster: "onoff".into(),
            command: "foo".into(),
            args: vec![],
            endpoint: 1,
        })
        .unwrap_err();
        assert!(
            err.detail.contains("numeric IDs are accepted"),
            "{}",
            err.detail
        );

        let m = to_device_op(&Op::GroupColorTemp {
            group_id: 10,
            mireds: 370,
            kelvin: 2702,
            transition: 0,
            endpoint: 1,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Group(ref g) if g.kind == GroupOpKind::ColorTemp { kelvin: 2702, mireds: 370, transition: 0 })
        );
        let m = to_device_op(&Op::GroupLevel {
            group_id: 10,
            level: 254,
            percent: 100,
            transition: 0,
            endpoint: 1,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Group(ref g) if g.kind == GroupOpKind::Level { percent: 100, level: 254, transition: 0 })
        );
        let m = to_device_op(&Op::GroupColor {
            group_id: 10,
            hue_raw: 180,
            saturation_raw: 200,
            hue: 254,
            saturation: 78,
            name: None,
            rgb: None,
            transition: 0,
            endpoint: 1,
        })
        .unwrap();
        assert!(matches!(m, MatdOp::Group(ref g) if matches!(g.kind, GroupOpKind::Color { .. })));
        assert!(matches!(
            to_device_op(&Op::GroupBump).unwrap(),
            MatdOp::Bump
        ));
        let m = to_device_op(&Op::GroupProvision {
            group_id: 7,
            node_ids: vec![1, 2],
            keyset_id: 42,
            name: "grp7".into(),
            endpoint: 1,
            epoch_key: None,
            rebind: true,
        })
        .unwrap();
        assert!(
            matches!(m, MatdOp::Provision(ref p) if p.group_id == 7 && p.node_ids == vec![1, 2] && p.rebind)
        );
    }

    /// dispatch 不変条件が破れても panic しない（v1 Task6 規律）。
    #[test]
    fn to_device_op_rejects_non_device_ops_without_panic() {
        for op in [
            Op::Ping,
            Op::Status,
            Op::Shutdown,
            Op::NodeTouched { node_id: 1 },
            Op::Reload,
            Op::Listen {
                node_id: None,
                endpoint: None,
                cluster: None,
                attribute: None,
                event: None,
            },
        ] {
            let err = to_device_op(&op).unwrap_err();
            assert_eq!(err.kind, ErrorKind::ParseError);
            assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);
        }
    }

    /// op → レポート期待の分類（spec 2026-07-24 の表）。
    /// 「op 成功」は「レポートが出る」を含意しない: 目標状態と現在値が一致する
    /// no-op はレポートを生まないので pending を打ってはならない。
    #[test]
    fn op_report_expectation_only_when_value_actually_changes() {
        let on = Op::On {
            node_id: 5,
            endpoint: 1,
        };
        let off = Op::Off {
            node_id: 5,
            endpoint: 1,
        };
        let level = Op::Level {
            node_id: 5,
            endpoint: 1,
            level: 128,
            percent: 50,
            transition: 0,
        };
        let t = json!(true);
        let f = json!(false);
        let l128 = json!(128);
        let l200 = json!(200);

        // On: 現在 off → 変化する → pending。
        assert_eq!(
            op_report_expectation(&on, Some(&f), None),
            Some((5, im::CLUSTER_ON_OFF))
        );
        // On: 既に on → no-op → 打たない。
        assert_eq!(op_report_expectation(&on, Some(&t), None), None);
        // Off: 現在 on → 変化する → pending。
        assert_eq!(
            op_report_expectation(&off, Some(&t), None),
            Some((5, im::CLUSTER_ON_OFF))
        );
        // Off: 既に off → no-op → 打たない（casa 人感ルールの誤キルの正体）。
        assert_eq!(op_report_expectation(&off, Some(&f), None), None);
        // Level: 現在値と異なる → pending / 同値 → 打たない。
        assert_eq!(
            op_report_expectation(&level, None, Some(&l200)),
            Some((5, im::CLUSTER_LEVEL_CONTROL))
        );
        assert_eq!(op_report_expectation(&level, None, Some(&l128)), None);

        // Level while off: MoveToLevel は Options=0 なので OnOff=false のデバイス
        // では実行されずレポートも出ない → 確実に消灯中なら値差分があっても打たない。
        assert_eq!(op_report_expectation(&level, Some(&f), Some(&l200)), None);
        // Level while on: 点灯中は通常通り値差分で判定する。
        assert_eq!(
            op_report_expectation(&level, Some(&t), Some(&l200)),
            Some((5, im::CLUSTER_LEVEL_CONTROL))
        );
        // Level, on-off キャッシュ欠落: 「不明」を「消灯」と決めつけず従来通り
        // level の比較へ進む（挙動不変の確認。上の `None, Some(&l200)` ケースと同じ）。

        // キャッシュ欠落: 証明できないので打たない（matd 起動直後・購読未確立）。
        assert_eq!(op_report_expectation(&on, None, None), None);
        assert_eq!(op_report_expectation(&off, None, None), None);
        assert_eq!(op_report_expectation(&level, None, None), None);
        // 型が想定外（level が null 等）でも打たない。
        assert_eq!(
            op_report_expectation(&level, None, Some(&json!(null))),
            None
        );

        // Color / ColorTemp / Write / Invoke は pending 対象から降格
        // （状態変化を証明できない。受け皿は無音 deadline）。
        let color_temp = Op::ColorTemp {
            node_id: 5,
            endpoint: 1,
            mireds: 370,
            kelvin: 2700,
            transition: 0,
        };
        assert_eq!(
            op_report_expectation(&color_temp, Some(&t), Some(&l128)),
            None
        );
        let invoke = Op::Invoke {
            node_id: 5,
            endpoint: 1,
            cluster: "onoff".into(),
            command: "toggle".into(),
            args: vec![],
            timed: false,
        };
        assert_eq!(op_report_expectation(&invoke, Some(&t), Some(&l128)), None);
        let write = Op::Write {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "on-level".into(),
            value: "128".into(),
            timed: false,
        };
        assert_eq!(op_report_expectation(&write, Some(&t), Some(&l128)), None);
        // Read は元から対象外。
        let read = Op::Read {
            node_id: 5,
            endpoint: 1,
            cluster: "onoff".into(),
            attribute: Some("on-off".into()),
        };
        assert_eq!(op_report_expectation(&read, Some(&f), None), None);
    }
}
