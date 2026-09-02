//! clap `Command` → 解決済み device op（監査④）。
//!
//! `resolve::resolve_command` の後段。alias は数値確定済みで届く（未解決は
//! 内部バグとして typed error）。名前解決・単位換算・color spec 解決はここで
//! 1 回だけ行い、以降の経路（matd wire / 直経路）には `DeviceOp` だけが流れる。
//! 専用コマンド層を持つ op（discover / commission / fabric / listen / diag node /
//! diag mesh）は `Dispatch::Dedicated(name)`（name は `--matd` 強制時の
//! unsupported 文言用）。

use crate::cli::{Command, DiagCommand, GroupCommand};
use mat_core::alias::NodeRef;
use mat_core::error::MatError;
use mat_native::op::{GroupOp, GroupOpKind, NodeOp, NodeOpKind, ProvisionParams};

/// 解決済み device op（直経路 / matd wire の共通入力）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DeviceOp {
    Node(NodeOp),
    Group(GroupOp),
    GroupProvision(ProvisionParams),
    /// 直経路専用（matd プロトコルに op を足さない — 稀な修復操作で warm
    /// session の恩恵が小さく、mat/matd のバージョンスキューにも安全）。
    GroupGrant {
        group_id: u16,
        node_ids: Vec<u64>,
    },
    GroupBump,
}

impl DeviceOp {
    /// ログ用の op 名。
    pub(crate) fn name(&self) -> &'static str {
        match self {
            DeviceOp::Node(n) => n.kind.name(),
            DeviceOp::Group(g) => g.kind.name(),
            DeviceOp::GroupProvision(_) => "group_provision",
            DeviceOp::GroupGrant { .. } => "group_grant",
            DeviceOp::GroupBump => "group_bump",
        }
    }
}

/// `classify` の結果。`Dedicated(name)` = 専用コマンド層を持つ op。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Dispatch {
    Device(DeviceOp),
    Dedicated(&'static str),
}

/// `mat open-window` の discriminator 未指定時の決定的補完（12-bit に収める）。
pub(crate) fn resolve_discriminator(node_id: u64, discriminator: Option<u16>) -> u16 {
    discriminator.unwrap_or((node_id % 4096) as u16)
}

/// `Command` の網羅 match（`_` 無し）: 新しいサブコマンドを足すとここが
/// コンパイルエラーになり、経路割当の考慮漏れを防ぐ。`Err` = 未解決 alias
/// （内部バグ）/ 名前未解決 / 値符号化不能 / 不正 color spec。
pub(crate) fn classify(command: &Command) -> Result<Dispatch, MatError> {
    fn node(node_id: &NodeRef, kind: NodeOpKind) -> Result<Dispatch, MatError> {
        Ok(Dispatch::Device(DeviceOp::Node(NodeOp {
            node_id: node_id.id()?,
            kind,
        })))
    }
    fn color(spec: &crate::cli::ColorSpecArgs) -> Result<mat_core::color::ResolvedColor, MatError> {
        mat_core::color::resolve_spec(
            spec.name.as_deref(),
            spec.rgb.as_deref(),
            spec.hue,
            spec.sat,
        )
    }
    fn nodes(ids: &[NodeRef]) -> Result<Vec<u64>, MatError> {
        ids.iter().map(NodeRef::id).collect()
    }

    match command {
        Command::Discover { .. } => Ok(Dispatch::Dedicated("discover")),
        Command::Commission { .. } => Ok(Dispatch::Dedicated("commission")),
        Command::Fabric { .. } => Ok(Dispatch::Dedicated("fabric")),
        Command::Listen { .. } => Ok(Dispatch::Dedicated("listen")),
        Command::Diag {
            action: DiagCommand::Node { .. } | DiagCommand::Mesh { .. },
        } => Ok(Dispatch::Dedicated("diag")),
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => node(
            node_id,
            NodeOpKind::read(endpoint.id()?, cluster, attribute)?,
        ),
        Command::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => node(
            node_id,
            NodeOpKind::write(endpoint.id()?, cluster, attribute, value)?,
        ),
        Command::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            args,
        } => node(
            node_id,
            NodeOpKind::invoke(endpoint.id()?, cluster, command, args)?,
        ),
        Command::Describe { node_id } => node(node_id, NodeOpKind::Describe),
        Command::On { node_id, endpoint } => node(
            node_id,
            NodeOpKind::On {
                endpoint: endpoint.id()?,
            },
        ),
        Command::Off { node_id, endpoint } => node(
            node_id,
            NodeOpKind::Off {
                endpoint: endpoint.id()?,
            },
        ),
        Command::ColorTemp {
            node_id,
            endpoint,
            kelvin,
            mireds,
            transition,
        } => node(
            node_id,
            NodeOpKind::color_temp(endpoint.id()?, *kelvin, *mireds, *transition),
        ),
        Command::Level {
            node_id,
            endpoint,
            percent,
            transition,
        } => node(
            node_id,
            NodeOpKind::level(endpoint.id()?, *percent, *transition),
        ),
        Command::Color {
            node_id,
            endpoint,
            spec,
            transition,
        } => node(
            node_id,
            NodeOpKind::Color {
                endpoint: endpoint.id()?,
                color: color(spec)?,
                transition: *transition,
            },
        ),
        // QR 画像のレンダリングは `mat` の責務ではない。複数機器の一括共有は
        // Matter 仕様上不可。
        Command::OpenWindow {
            node_id,
            timeout,
            iteration,
            discriminator,
        } => {
            let nid = node_id.id()?;
            node(
                node_id,
                NodeOpKind::OpenWindow {
                    timeout: *timeout,
                    iteration: *iteration,
                    discriminator: resolve_discriminator(nid, *discriminator),
                },
            )
        }
        Command::Diag {
            action: DiagCommand::Thread { node_id, endpoint },
        } => node(
            node_id,
            NodeOpKind::DiagThread {
                endpoint: endpoint.id()?,
            },
        ),
        Command::Group { action } => Ok(Dispatch::Device(match action {
            GroupCommand::Provision {
                group_id,
                node_ids,
                keyset_id,
                name,
                endpoint,
                epoch_key,
                rebind,
            } => {
                let gid = group_id.id()?;
                DeviceOp::GroupProvision(ProvisionParams {
                    group_id: gid,
                    node_ids: nodes(node_ids)?,
                    keyset_id: *keyset_id,
                    // name 未指定は group_id から決定的に補完（両経路共通の規則）。
                    name: name.clone().unwrap_or_else(|| format!("grp{gid}")),
                    endpoint: *endpoint,
                    epoch_key: epoch_key.clone(),
                    rebind: *rebind,
                })
            }
            GroupCommand::Invoke {
                group_id,
                cluster,
                command,
                args,
                endpoint,
            } => DeviceOp::Group(GroupOp {
                group_id: group_id.id()?,
                endpoint: *endpoint,
                kind: GroupOpKind::invoke(cluster, command, args)?,
            }),
            GroupCommand::ColorTemp {
                group_id,
                kelvin,
                mireds,
                transition,
                endpoint,
            } => DeviceOp::Group(GroupOp {
                group_id: group_id.id()?,
                endpoint: *endpoint,
                kind: GroupOpKind::color_temp(*kelvin, *mireds, *transition),
            }),
            GroupCommand::Level {
                group_id,
                percent,
                transition,
                endpoint,
            } => DeviceOp::Group(GroupOp {
                group_id: group_id.id()?,
                endpoint: *endpoint,
                kind: GroupOpKind::level(*percent, *transition),
            }),
            GroupCommand::Color {
                group_id,
                spec,
                transition,
                endpoint,
            } => DeviceOp::Group(GroupOp {
                group_id: group_id.id()?,
                endpoint: *endpoint,
                kind: GroupOpKind::Color {
                    color: color(spec)?,
                    transition: *transition,
                },
            }),
            GroupCommand::Grant { group_id, node_ids } => DeviceOp::GroupGrant {
                group_id: group_id.id()?,
                node_ids: nodes(node_ids)?,
            },
            GroupCommand::Bump => DeviceOp::GroupBump,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{ColorSpecArgs, DiagCommand, GroupCommand};
    use mat_core::alias::{EndpointRef, GroupRef, NodeRef};
    use mat_core::error::ErrorKind;
    use mat_native::op::{GroupOpKind, NodeOpKind};

    fn node_op(c: &Command) -> NodeOp {
        match classify(c).unwrap() {
            Dispatch::Device(DeviceOp::Node(n)) => n,
            other => panic!("expected Node, got {other:?}"),
        }
    }

    fn group_op(c: &Command) -> GroupOp {
        match classify(c).unwrap() {
            Dispatch::Device(DeviceOp::Group(g)) => g,
            other => panic!("expected Group, got {other:?}"),
        }
    }

    #[test]
    fn on_off_read_shapes() {
        let on = Command::On {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
        };
        assert_eq!(
            node_op(&on),
            NodeOp {
                node_id: 5,
                kind: NodeOpKind::On { endpoint: 1 }
            }
        );
        let off = Command::Off {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
        };
        assert_eq!(node_op(&off).kind, NodeOpKind::Off { endpoint: 1 });
        let read = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            attribute: "current-level".into(),
        };
        assert!(matches!(
            node_op(&read).kind,
            NodeOpKind::Read {
                cluster: 0x0008,
                attribute: 0,
                ..
            }
        ));
        let byid = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "0x0008".into(),
            attribute: "0".into(),
        };
        assert!(matches!(node_op(&byid).kind, NodeOpKind::Read { .. }));
        // 未知名は parse_error（旧 classify None → unresolved_op_error と同じ kind）。
        let unknown = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "nosuchcluster".into(),
            attribute: "x".into(),
        };
        let err = classify(&unknown).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(
            err.detail.contains("numeric IDs are accepted"),
            "{}",
            err.detail
        );
    }

    #[test]
    fn unresolved_alias_is_internal_error() {
        let on = Command::On {
            node_id: NodeRef::Alias("kitchen".into()),
            endpoint: EndpointRef::Id(1),
        };
        let err = classify(&on).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.contains("kitchen"), "{}", err.detail);
    }

    #[test]
    fn color_temp_level_color_convert_units() {
        let ct = Command::ColorTemp {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            kelvin: Some(2700),
            mireds: None,
            transition: 0,
        };
        assert_eq!(
            node_op(&ct).kind,
            NodeOpKind::ColorTemp {
                endpoint: 1,
                kelvin: 2700,
                mireds: 370,
                transition: 0
            }
        );
        let lv = Command::Level {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            percent: 50,
            transition: 3,
        };
        assert_eq!(
            node_op(&lv).kind,
            NodeOpKind::Level {
                endpoint: 1,
                percent: 50,
                level: 127,
                transition: 3
            }
        );
        // classify は resolve_command 後に呼ばれるため name は rgb 解決済みで届く。
        let c = Command::Color {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            spec: ColorSpecArgs {
                name: Some("red".into()),
                rgb: Some("#ff0000".into()),
                hue: None,
                sat: None,
            },
            transition: 0,
        };
        match node_op(&c).kind {
            NodeOpKind::Color { color, .. } => {
                assert_eq!(
                    (color.hue_raw, color.sat_raw, color.hue, color.sat),
                    (0, 254, 0, 100)
                );
                assert_eq!(color.name.as_deref(), Some("red"));
            }
            other => panic!("expected Color, got {other:?}"),
        }
        // 不正 rgb は resolve_spec のエラーがそのまま出る。
        let bad = Command::Color {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            spec: ColorSpecArgs {
                name: None,
                rgb: Some("zzz".into()),
                hue: None,
                sat: None,
            },
            transition: 0,
        };
        assert!(classify(&bad).is_err());
    }

    #[test]
    fn write_and_invoke_reject_unencodable_values() {
        let w = Command::Write {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            attribute: "on-level".into(),
            value: "128".into(),
        };
        assert!(matches!(node_op(&w).kind, NodeOpKind::Write { .. }));
        let acl = Command::Write {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "accesscontrol".into(),
            attribute: "acl".into(),
            value: "[]".into(),
        };
        let err = classify(&acl).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.contains("list"), "{}", err.detail);
        let inv = Command::Invoke {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            command: "move-to-level".into(),
            args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
        };
        assert!(matches!(
            node_op(&inv).kind,
            NodeOpKind::Invoke {
                fields_tlv: Some(_),
                ..
            }
        ));
        let ks = Command::Invoke {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "groupkeymanagement".into(),
            command: "key-set-write".into(),
            args: vec!["{}".into()],
        };
        assert_eq!(classify(&ks).unwrap_err().kind, ErrorKind::ParseError);
    }

    #[test]
    fn describe_diag_thread_open_window_and_dedicated() {
        let d = Command::Describe {
            node_id: NodeRef::Id(5),
        };
        assert_eq!(
            node_op(&d),
            NodeOp {
                node_id: 5,
                kind: NodeOpKind::Describe
            }
        );
        let t = Command::Diag {
            action: DiagCommand::Thread {
                node_id: NodeRef::Id(5),
                endpoint: EndpointRef::Id(0),
            },
        };
        assert_eq!(node_op(&t).kind, NodeOpKind::DiagThread { endpoint: 0 });
        let ow = Command::OpenWindow {
            node_id: NodeRef::Id(5),
            timeout: 180,
            iteration: 1000,
            discriminator: Some(3840),
        };
        assert_eq!(
            node_op(&ow).kind,
            NodeOpKind::OpenWindow {
                timeout: 180,
                iteration: 1000,
                discriminator: 3840
            }
        );
        // discriminator 未指定は node_id % 4096 で決定的に補完。
        let ow = Command::OpenWindow {
            node_id: NodeRef::Id(4101),
            timeout: 180,
            iteration: 1000,
            discriminator: None,
        };
        assert!(matches!(
            node_op(&ow).kind,
            NodeOpKind::OpenWindow {
                discriminator: 5,
                ..
            }
        ));

        assert_eq!(
            classify(&Command::Discover { probe: false }).unwrap(),
            Dispatch::Dedicated("discover")
        );
        let dn = Command::Diag {
            action: DiagCommand::Node {
                node_id: NodeRef::Id(5),
                endpoint: EndpointRef::Id(0),
                deep: false,
            },
        };
        assert_eq!(classify(&dn).unwrap(), Dispatch::Dedicated("diag"));
        let dm = Command::Diag {
            action: DiagCommand::Mesh { nodes: vec![] },
        };
        assert_eq!(classify(&dm).unwrap(), Dispatch::Dedicated("diag"));
    }

    #[test]
    fn group_ops_provision_grant_bump() {
        let toggle = Command::Group {
            action: GroupCommand::Invoke {
                group_id: GroupRef::Id(10),
                cluster: "onoff".into(),
                command: "toggle".into(),
                args: vec![],
                endpoint: 1,
            },
        };
        let g = group_op(&toggle);
        assert_eq!((g.group_id, g.endpoint), (10, 1));
        assert_eq!(
            g.kind.wire(),
            (
                mat_controller::im::CLUSTER_ON_OFF,
                mat_controller::im::CMD_ON_OFF_TOGGLE,
                None
            )
        );
        let generic = Command::Group {
            action: GroupCommand::Invoke {
                group_id: GroupRef::Id(10),
                cluster: "levelcontrol".into(),
                command: "move-to-level".into(),
                args: vec!["128".into()],
                endpoint: 1,
            },
        };
        assert!(matches!(
            group_op(&generic).kind,
            GroupOpKind::Invoke {
                fields_tlv: Some(_),
                ..
            }
        ));
        let ct = Command::Group {
            action: GroupCommand::ColorTemp {
                group_id: GroupRef::Id(10),
                kelvin: Some(2700),
                mireds: None,
                transition: 0,
                endpoint: 1,
            },
        };
        assert_eq!(
            group_op(&ct).kind,
            GroupOpKind::ColorTemp {
                kelvin: 2700,
                mireds: 370,
                transition: 0
            }
        );
        let lv = Command::Group {
            action: GroupCommand::Level {
                group_id: GroupRef::Id(10),
                percent: 50,
                transition: 0,
                endpoint: 1,
            },
        };
        assert_eq!(
            group_op(&lv).kind,
            GroupOpKind::Level {
                percent: 50,
                level: 127,
                transition: 0
            }
        );
        let color = Command::Group {
            action: GroupCommand::Color {
                group_id: GroupRef::Id(10),
                spec: ColorSpecArgs {
                    name: None,
                    rgb: Some("#ff0000".into()),
                    hue: None,
                    sat: None,
                },
                transition: 0,
                endpoint: 1,
            },
        };
        assert!(matches!(group_op(&color).kind, GroupOpKind::Color { .. }));

        let grant = Command::Group {
            action: GroupCommand::Grant {
                group_id: GroupRef::Id(10),
                node_ids: vec![NodeRef::Id(5), NodeRef::Id(6)],
            },
        };
        assert_eq!(
            classify(&grant).unwrap(),
            Dispatch::Device(DeviceOp::GroupGrant {
                group_id: 10,
                node_ids: vec![5, 6]
            })
        );
        let provision = Command::Group {
            action: GroupCommand::Provision {
                group_id: GroupRef::Id(10),
                node_ids: vec![NodeRef::Id(5)],
                keyset_id: 60,
                name: None,
                endpoint: 1,
                epoch_key: None,
                rebind: false,
            },
        };
        assert_eq!(
            classify(&provision).unwrap(),
            Dispatch::Device(DeviceOp::GroupProvision(ProvisionParams {
                group_id: 10,
                node_ids: vec![5],
                keyset_id: 60,
                name: "grp10".into(),
                endpoint: 1,
                epoch_key: None,
                rebind: false,
            }))
        );
        assert_eq!(
            classify(&Command::Group {
                action: GroupCommand::Bump
            })
            .unwrap(),
            Dispatch::Device(DeviceOp::GroupBump)
        );
    }
}
