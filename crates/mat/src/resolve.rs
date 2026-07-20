//! clap parse 直後の alias 一括解決。
//!
//! ここを通った後の `Command` は NodeRef / GroupRef / EndpointRef が全て `Id` に
//! 確定している（matd 経路・直経路の両方がこの後段）。exit code 規約: 壊れた
//! aliases.toml は `store_parse`（10）、未知 alias / 不正 alias 名は CLI 引数
//! エラー（2）— main が `kind` で振り分ける。

use std::path::Path;

use crate::cli::{ColorSpecArgs, Command, DiagCommand, GroupCommand};
use mat_core::alias::{AliasBook, EndpointRef, GroupRef, NodeRef};
use mat_core::color;
use mat_core::error::{ErrorKind, MatError};

/// command 内の alias を全て数値（`Id`）へ確定した `Command` を返す。
/// aliases.toml が無ければ数値はパススルー（従来動作）。
///
/// match は網羅（`_` 無し）: 新しいサブコマンドを足すとここがコンパイルエラーに
/// なり、alias 解決の考慮漏れを防ぐ。
pub fn resolve_command(command: Command, store_root: &Path) -> Result<Command, MatError> {
    let book = AliasBook::load(store_root)?;
    Ok(match command {
        Command::Discover { probe } => Command::Discover { probe },
        Command::Commission {
            target,
            setup_code,
            node_id,
            alias,
            thread_dataset,
        } => {
            // 名前の妥当性・重複は commission 開始前に検証する（開始後に alias
            // 書き込みだけ失敗する中途半端な状態を作らない）。
            if let Some(name) = &alias {
                book.validate_new_node_alias(name)?;
            }
            Command::Commission {
                target,
                setup_code,
                node_id,
                alias,
                thread_dataset,
            }
        }
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Read {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                cluster,
                attribute,
            }
        }
        Command::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Write {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                cluster,
                attribute,
                value,
            }
        }
        Command::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            args,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Invoke {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                cluster,
                command,
                args,
            }
        }
        Command::Describe { node_id } => Command::Describe {
            node_id: NodeRef::Id(book.resolve_node(&node_id)?),
        },
        Command::On { node_id, endpoint } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::On {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
            }
        }
        Command::Off { node_id, endpoint } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Off {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
            }
        }
        Command::ColorTemp {
            node_id,
            endpoint,
            kelvin,
            mireds,
            transition,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::ColorTemp {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                kelvin,
                mireds,
                transition,
            }
        }
        Command::Level {
            node_id,
            endpoint,
            percent,
            transition,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Level {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                percent,
                transition,
            }
        }
        Command::Color {
            node_id,
            endpoint,
            spec,
            transition,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Color {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                spec: resolve_color_spec(&book, spec)?,
                transition,
            }
        }
        Command::OpenWindow {
            node_id,
            timeout,
            iteration,
            discriminator,
        } => Command::OpenWindow {
            node_id: NodeRef::Id(book.resolve_node(&node_id)?),
            timeout,
            iteration,
            discriminator,
        },
        Command::Group { action } => Command::Group {
            action: match action {
                GroupCommand::Provision {
                    group_id,
                    node_ids,
                    keyset_id,
                    name,
                    endpoint,
                    epoch_key,
                    rebind,
                } => GroupCommand::Provision {
                    group_id: GroupRef::Id(book.resolve_group(&group_id)?),
                    node_ids: node_ids
                        .iter()
                        .map(|n| book.resolve_node(n).map(NodeRef::Id))
                        .collect::<Result<Vec<_>, _>>()?,
                    keyset_id,
                    name,
                    endpoint,
                    epoch_key,
                    rebind,
                },
                GroupCommand::Invoke {
                    group_id,
                    cluster,
                    command,
                    args,
                    endpoint,
                } => GroupCommand::Invoke {
                    group_id: GroupRef::Id(book.resolve_group(&group_id)?),
                    cluster,
                    command,
                    args,
                    endpoint,
                },
                GroupCommand::Grant { group_id, node_ids } => GroupCommand::Grant {
                    group_id: GroupRef::Id(book.resolve_group(&group_id)?),
                    node_ids: node_ids
                        .iter()
                        .map(|n| book.resolve_node(n).map(NodeRef::Id))
                        .collect::<Result<Vec<_>, _>>()?,
                },
                GroupCommand::ColorTemp {
                    group_id,
                    kelvin,
                    mireds,
                    transition,
                    endpoint,
                } => GroupCommand::ColorTemp {
                    group_id: GroupRef::Id(book.resolve_group(&group_id)?),
                    kelvin,
                    mireds,
                    transition,
                    endpoint,
                },
                GroupCommand::Color {
                    group_id,
                    spec,
                    transition,
                    endpoint,
                } => GroupCommand::Color {
                    group_id: GroupRef::Id(book.resolve_group(&group_id)?),
                    spec: resolve_color_spec(&book, spec)?,
                    transition,
                    endpoint,
                },
            },
        },
        Command::Diag { action } => Command::Diag {
            action: match action {
                DiagCommand::Thread { node_id, endpoint } => {
                    let node = book.resolve_node(&node_id)?;
                    let ep = book.resolve_endpoint(node, &endpoint)?;
                    DiagCommand::Thread {
                        node_id: NodeRef::Id(node),
                        endpoint: EndpointRef::Id(ep),
                    }
                }
                DiagCommand::Node {
                    node_id,
                    endpoint,
                    deep,
                } => {
                    let node = book.resolve_node(&node_id)?;
                    let ep = book.resolve_endpoint(node, &endpoint)?;
                    DiagCommand::Node {
                        node_id: NodeRef::Id(node),
                        endpoint: EndpointRef::Id(ep),
                        deep,
                    }
                }
            },
        },
        // fabric_id / admin_node_id は数値のみ（alias 対象フィールドが無い）— パススルー。
        Command::Fabric { action } => Command::Fabric { action },
    })
}

/// 色指定の name / rgb を正規化済み RGB（`#rrggbb`）へ確定する。hue/sat 生指定は
/// パススルー。色名解決は node/group/endpoint alias と同じレイヤ（ここ）で行い、
/// 以降の経路（直 / matd）には数値換算可能な形だけが流れる。未知の色名・不正な
/// `--rgb` は kind=Other（main が exit 2 に写す）。
fn resolve_color_spec(book: &AliasBook, spec: ColorSpecArgs) -> Result<ColorSpecArgs, MatError> {
    if let Some(name) = &spec.name {
        let rgb = book.resolve_color_name(name)?;
        return Ok(ColorSpecArgs {
            name: spec.name.clone(),
            rgb: Some(color::hex_string(rgb)),
            hue: None,
            sat: None,
        });
    }
    if let Some(rgb) = &spec.rgb {
        let parsed = color::parse_rgb(rgb).map_err(|e| MatError::new(ErrorKind::Other, e))?;
        return Ok(ColorSpecArgs {
            name: None,
            rgb: Some(color::hex_string(parsed)),
            hue: None,
            sat: None,
        });
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(toml: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("aliases.toml"), toml).unwrap();
        dir
    }

    const SAMPLE: &str = r#"
        [nodes]
        living-light = 5

        [groups]
        all-lights = 258

        [endpoints.living-light]
        night = 2
    "#;

    #[test]
    fn read_alias_resolves_node_then_endpoint() {
        let dir = store_with(SAMPLE);
        let cmd = Command::Read {
            node_id: NodeRef::Alias("living-light".into()),
            endpoint: EndpointRef::Alias("night".into()),
            cluster: "onoff".into(),
            attribute: "on-off".into(),
        };
        let resolved = resolve_command(cmd, dir.path()).unwrap();
        match resolved {
            Command::Read {
                node_id, endpoint, ..
            } => {
                assert_eq!(node_id, NodeRef::Id(5));
                assert_eq!(endpoint, EndpointRef::Id(2));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn numeric_node_still_resolves_endpoint_alias() {
        // -n 5 -e night: 外側キーが alias 表記でも解決後 node で照合される。
        let dir = store_with(SAMPLE);
        let cmd = Command::On {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Alias("night".into()),
        };
        match resolve_command(cmd, dir.path()).unwrap() {
            Command::On { endpoint, .. } => assert_eq!(endpoint, EndpointRef::Id(2)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn group_provision_resolves_group_and_each_node() {
        let dir = store_with(SAMPLE);
        let cmd = Command::Group {
            action: GroupCommand::Provision {
                group_id: GroupRef::Alias("all-lights".into()),
                node_ids: vec![NodeRef::Alias("living-light".into()), NodeRef::Id(7)],
                keyset_id: 42,
                name: None,
                endpoint: 1,
                epoch_key: None,
                rebind: false,
            },
        };
        match resolve_command(cmd, dir.path()).unwrap() {
            Command::Group {
                action:
                    GroupCommand::Provision {
                        group_id, node_ids, ..
                    },
            } => {
                assert_eq!(group_id, GroupRef::Id(258));
                assert_eq!(node_ids, vec![NodeRef::Id(5), NodeRef::Id(7)]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_alias_is_kind_other() {
        let dir = store_with(SAMPLE);
        let cmd = Command::Describe {
            node_id: NodeRef::Alias("bogus".into()),
        };
        let err = resolve_command(cmd, dir.path()).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
    }

    #[test]
    fn no_aliases_file_passes_numerics_through() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = Command::Describe {
            node_id: NodeRef::Id(5),
        };
        match resolve_command(cmd, dir.path()).unwrap() {
            Command::Describe { node_id } => assert_eq!(node_id, NodeRef::Id(5)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn color_name_resolves_to_normalized_rgb() {
        let dir = store_with("[colors]\nwarm = \"255,140,0\"\n");
        let cmd = Command::Color {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            spec: crate::cli::ColorSpecArgs {
                name: Some("warm".into()),
                rgb: None,
                hue: None,
                sat: None,
            },
            transition: 0,
        };
        match resolve_command(cmd, dir.path()).unwrap() {
            Command::Color { spec, .. } => {
                assert_eq!(spec.name.as_deref(), Some("warm"));
                assert_eq!(spec.rgb.as_deref(), Some("#ff8c00"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
