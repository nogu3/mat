//! one-shot 直経路の native 実行（M7）。
//!
//! matd 稼働中は matd が優先される（main.rs の経路順）。ここに来るのは直経路のみで、
//! `MAT_IFACE` 設定時に native 対象 op を mat-controller で in-process 実行する。
//! warm セッションは持たない: 確立 → 1 op → 破棄（設計ルール 4）。matd と違い
//! Timeout 再確立はしない（確立直後の session が stale なことはない）。
//! エンジン構築失敗（KVS 不備等）と group native 不可は warn を出して
//! chip-tool 直へフォールバック（matd の起動時フォールバックと同型）。

use std::path::Path;

use mat_core::error::MatError;
use mat_core::store::Store;
use mat_native::group::GroupOutcome;
use mat_native::{Engine, NativeConfig};

use mat_core::alias::NodeRef;

use crate::cli::{Command, DiagCommand, GroupCommand};

pub(crate) struct Config<'a> {
    pub iface: &'a str,
    pub fabric_index: u8,
    pub issuer_index: u8,
}

/// native 対象 op の分類（matd の is_native_hotpath / native_group_params と対）。
#[derive(Debug)]
pub(crate) enum NativeOp {
    On {
        node_id: u64,
        endpoint: u16,
    },
    Off {
        node_id: u64,
        endpoint: u16,
    },
    ReadOnOff {
        node_id: u64,
        endpoint: u16,
    },
    Color {
        node_id: u64,
        endpoint: u16,
        color: mat_core::color::ResolvedColor,
        transition: u16,
    },
    ColorTemp {
        node_id: u64,
        endpoint: u16,
        kelvin: u32,
        mireds: u16,
        transition: u16,
    },
    GroupOnOff {
        group_id: u16,
        command_id: u32,
        command: &'static str,
        endpoint: u16,
    },
    GroupColor {
        group_id: u16,
        color: mat_core::color::ResolvedColor,
        transition: u16,
        endpoint: u16,
    },
    GroupColorTemp {
        group_id: u16,
        kelvin: u32,
        mireds: u16,
        transition: u16,
        endpoint: u16,
    },
    /// group への汎用 invoke（onoff on/off/toggle 引数なし以外 — M8a Task9）。
    /// `GroupOnOff` と違い cluster/command を名前解決した任意コマンドを送る。
    GroupInvokeGeneric {
        group_id: u16,
        cluster_in: String,
        command_in: String,
        cluster: u32,
        command: u32,
        fields_tlv: Option<Vec<u8>>,
        endpoint: u16,
    },
    /// `mat group provision`（M8a Task9）: コントローラ側 group state は
    /// chip-tool のまま（KVS 書込所有は M8c）、デバイス側 4 ステップのみ native。
    GroupProvision {
        group_id: u16,
        node_ids: Vec<u64>,
        keyset_id: u16,
        name: String,
        endpoint: u16,
        epoch_key: Option<String>,
        rebind: bool,
    },
    /// `mat group grant`（M8a Task9）: 各ノードへ ACL read-merge-write のみ。
    GroupGrant {
        group_id: u16,
        node_ids: Vec<u64>,
    },
    ReadAttr {
        node_id: u64,
        endpoint: u16,
        cluster_in: String,
        attribute_in: String,
        cluster: u32,
        attribute: u32,
    },
    WriteAttr {
        node_id: u64,
        endpoint: u16,
        cluster_in: String,
        attribute_in: String,
        cluster: u32,
        attribute: u32,
        value_in: String,
        value: mat_core::ids::ScalarValue,
        timed: bool,
    },
    InvokeGeneric {
        node_id: u64,
        endpoint: u16,
        cluster_in: String,
        command_in: String,
        cluster: u32,
        command: u32,
        fields_tlv: Option<Vec<u8>>,
        timed: bool,
    },
    Describe {
        node_id: u64,
    },
    DiagThread {
        node_id: u64,
        endpoint: u16,
    },
    OpenWindow {
        node_id: u64,
        timeout: u32,
        iteration: u32,
        discriminator: u16,
    },
}

/// `mat open-window` の discriminator 未指定時の決定的補完（12-bit に収める）。
/// main.rs（chip-tool 経路の既定値算出）と `classify`（native 直経路の対象値
/// 算出）の両方が使う共有式 —— 経路によって既定値がずれないようにする。
pub(crate) fn resolve_discriminator(node_id: u64, discriminator: Option<u16>) -> u16 {
    discriminator.unwrap_or((node_id % 4096) as u16)
}

pub(crate) fn classify(command: &Command) -> Option<NativeOp> {
    match command {
        Command::On { node_id, endpoint } => Some(NativeOp::On {
            node_id: node_id.id(),
            endpoint: endpoint.id(),
        }),
        Command::Off { node_id, endpoint } => Some(NativeOp::Off {
            node_id: node_id.id(),
            endpoint: endpoint.id(),
        }),
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } if cluster == "onoff" && attribute == "on-off" => Some(NativeOp::ReadOnOff {
            node_id: node_id.id(),
            endpoint: endpoint.id(),
        }),
        Command::ColorTemp {
            node_id,
            endpoint,
            kelvin,
            mireds,
            transition,
        } => {
            let (mireds, kelvin) = crate::commands::invoke::resolve_color_temp(*kelvin, *mireds);
            Some(NativeOp::ColorTemp {
                node_id: node_id.id(),
                endpoint: endpoint.id(),
                kelvin,
                mireds,
                transition: *transition,
            })
        }
        Command::Color {
            node_id,
            endpoint,
            spec,
            transition,
        } => {
            // 不正 color spec はここで None → chip-tool 経路が同一エラーを出す
            // （resolve は決定的なので挙動は一致する）。
            let c = mat_core::color::resolve_spec(
                spec.name.as_deref(),
                spec.rgb.as_deref(),
                spec.hue,
                spec.sat,
            )
            .ok()?;
            Some(NativeOp::Color {
                node_id: node_id.id(),
                endpoint: endpoint.id(),
                color: c,
                transition: *transition,
            })
        }
        // group 送信 3 形 + provision/grant（M8a Task9 でデバイス側 native 化）。
        // GroupInvoke は onoff の引数なし on/off/toggle のみここで直接
        // NativeOp::GroupOnOff にする（cluster/command 名前解決を経ない専用
        // 高速路）。それ以外（cluster != onoff / args 非空）の group invoke は
        // 下の classify_strict（GroupInvokeGeneric）に委ねる。
        // GroupColor / GroupColorTemp は常に native 対象。
        Command::Group {
            action:
                GroupCommand::Invoke {
                    group_id,
                    cluster,
                    command,
                    args,
                    endpoint,
                },
        } if cluster == "onoff" && args.is_empty() => {
            let (command_id, command) = match command.as_str() {
                "on" => (mat_controller::im::CMD_ON_OFF_ON, "on"),
                "off" => (mat_controller::im::CMD_ON_OFF_OFF, "off"),
                "toggle" => (mat_controller::im::CMD_ON_OFF_TOGGLE, "toggle"),
                _ => return None,
            };
            Some(NativeOp::GroupOnOff {
                group_id: group_id.id(),
                command_id,
                command,
                endpoint: *endpoint,
            })
        }
        Command::Group {
            action:
                GroupCommand::ColorTemp {
                    group_id,
                    kelvin,
                    mireds,
                    transition,
                    endpoint,
                },
        } => {
            let (mireds, kelvin) = crate::commands::invoke::resolve_color_temp(*kelvin, *mireds);
            Some(NativeOp::GroupColorTemp {
                group_id: group_id.id(),
                kelvin,
                mireds,
                transition: *transition,
                endpoint: *endpoint,
            })
        }
        Command::Group {
            action:
                GroupCommand::Color {
                    group_id,
                    spec,
                    transition,
                    endpoint,
                },
        } => {
            let c = mat_core::color::resolve_spec(
                spec.name.as_deref(),
                spec.rgb.as_deref(),
                spec.hue,
                spec.sat,
            )
            .ok()?;
            Some(NativeOp::GroupColor {
                group_id: group_id.id(),
                color: c,
                transition: *transition,
                endpoint: *endpoint,
            })
        }
        // provision / grant（M8a Task9）: デバイス側 4 ステップ（KeySetWrite /
        // group-key-map / AddGroup / ACL）を native 化。コントローラ側
        // groupsettings（chip-tool の KVS 書込）は変わらず chip-tool 側で行う
        // （`run_op` の `NativeOp::GroupProvision` 実装を参照。KVS 書込所有の
        // 分割は M8c）。name 未指定時の既定補完（`grp<id>`）は main.rs の
        // chip-tool 経路と同じ式を共有する。
        Command::Group {
            action:
                GroupCommand::Provision {
                    group_id,
                    node_ids,
                    keyset_id,
                    name,
                    endpoint,
                    epoch_key,
                    rebind,
                },
        } => {
            let gid = group_id.id();
            let resolved_name = name.clone().unwrap_or_else(|| format!("grp{gid}"));
            Some(NativeOp::GroupProvision {
                group_id: gid,
                node_ids: node_ids.iter().map(NodeRef::id).collect(),
                keyset_id: *keyset_id,
                name: resolved_name,
                endpoint: *endpoint,
                epoch_key: epoch_key.clone(),
                rebind: *rebind,
            })
        }
        Command::Group {
            action: GroupCommand::Grant { group_id, node_ids },
        } => Some(NativeOp::GroupGrant {
            group_id: group_id.id(),
            node_ids: node_ids.iter().map(NodeRef::id).collect(),
        }),
        // describe / diag thread / open-window（M8a Task8）: 値の符号化を
        // 伴わない読み取り専用 op なので、classify_strict と違い常に
        // Some/None（Err にはならない）。
        Command::Describe { node_id } => Some(NativeOp::Describe {
            node_id: node_id.id(),
        }),
        Command::Diag {
            action: DiagCommand::Thread { node_id, endpoint },
        } => Some(NativeOp::DiagThread {
            node_id: node_id.id(),
            endpoint: endpoint.id(),
        }),
        // `Diag Node` は probe（ping6/avahi-browse）混在のため対象外
        // （chip-tool 経路のまま。M8b/M8c で再訪）。
        Command::OpenWindow {
            node_id,
            timeout,
            iteration,
            discriminator,
        } => Some(NativeOp::OpenWindow {
            node_id: node_id.id(),
            timeout: *timeout,
            iteration: *iteration,
            discriminator: resolve_discriminator(node_id.id(), *discriminator),
        }),
        // 汎用 read/write/invoke（M8a）: classify_strict の判定を再利用し、値の
        // 符号化不能（Err）はここでは黙って None（chip-tool 直路）に丸める —
        // その Err を明示的に拒否（即 parse_error）したい呼び出し側は
        // `classify_strict` を直接使う（`try_run` がそうしている）。
        _ => classify_strict(command)?.ok(),
    }
}

/// 汎用形の分類: None = 非対象（chip-tool へ）、Some(Ok) = native 実行、
/// Some(Err) = native 対象だが値が符号化不能 → 即 parse_error（spec 決定3:
/// フォールバックせず明示拒否。chip-tool なら通る形をあえて拒むのは
/// opt-in（MAT_IFACE）下の意図した縮小）。
pub(crate) fn classify_strict(command: &Command) -> Option<Result<NativeOp, MatError>> {
    match command {
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => {
            let cluster_id = mat_core::ids::resolve_cluster(cluster)?;
            let attr = mat_core::ids::resolve_attribute(cluster_id, attribute)?;
            Some(Ok(NativeOp::ReadAttr {
                node_id: node_id.id(),
                endpoint: endpoint.id(),
                cluster_in: cluster.clone(),
                attribute_in: attribute.clone(),
                cluster: cluster_id,
                attribute: attr.id,
            }))
        }
        // 汎用 write（M8a Task7、M8a Task10 で classify_write へ一本化）:
        // cluster/attribute 名の解決 + 値の型スカラー化は mat-core::ids に
        // 集約されている（matd の native_op と判定を共有）。
        Command::Write {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
        } => match mat_core::ids::classify_write(cluster, attribute, value) {
            mat_core::ids::WriteClass::NotNative => None,
            mat_core::ids::WriteClass::Reject(msg) => Some(Err(MatError::parse_error(msg))),
            mat_core::ids::WriteClass::Native {
                attribute: attr_id,
                value: scalar,
                timed,
            } => {
                let cluster_id = mat_core::ids::resolve_cluster(cluster)
                    .expect("classify_write already resolved this cluster name");
                Some(Ok(NativeOp::WriteAttr {
                    node_id: node_id.id(),
                    endpoint: endpoint.id(),
                    cluster_in: cluster.clone(),
                    attribute_in: attribute.clone(),
                    cluster: cluster_id,
                    attribute: attr_id,
                    value_in: value.clone(),
                    value: scalar,
                    timed,
                }))
            }
        },
        // 汎用 invoke（M8a Task7、M8a Task10 で classify_invoke へ一本化）:
        // cluster/command 名の解決 + 引数の型スカラー化は mat-core::ids に
        // 集約されている（matd の native_op と判定を共有）。
        Command::Invoke {
            node_id,
            endpoint,
            cluster,
            command: command_name,
            args,
        } => match mat_core::ids::classify_invoke(cluster, command_name, args) {
            mat_core::ids::InvokeClass::NotNative => None,
            mat_core::ids::InvokeClass::Reject(msg) => Some(Err(MatError::parse_error(msg))),
            mat_core::ids::InvokeClass::Native {
                command: cmd_id,
                fields,
                timed,
            } => {
                let cluster_id = mat_core::ids::resolve_cluster(cluster)
                    .expect("classify_invoke already resolved this cluster name");
                let fields_tlv = if fields.is_empty() {
                    None
                } else {
                    Some(mat_native::encode_command_fields(&fields))
                };
                Some(Ok(NativeOp::InvokeGeneric {
                    node_id: node_id.id(),
                    endpoint: endpoint.id(),
                    cluster_in: cluster.clone(),
                    command_in: command_name.clone(),
                    cluster: cluster_id,
                    command: cmd_id,
                    fields_tlv,
                    timed,
                }))
            }
        },
        // group invoke の汎用形（M8a Task9、M8a Task10 で classify_invoke へ
        // 一本化 — 単体 invoke と ~50 行重複していた判定ロジックの解消）:
        // `classify` の GroupOnOff 専用ショートカット（onoff 引数なし
        // on/off/toggle）に当たらなかった group invoke がここに落ちる。
        // 宛先エンドポイントが無い（group-scoped）以外は `Command::Invoke` と
        // 同型。エラーメッセージの "invoke ..." プレフィックスは一本化前の
        // "group invoke ..." から差異が生じるが、その文言を検査する既存
        // テストは無い（kind のみ検査）。
        Command::Group {
            action:
                GroupCommand::Invoke {
                    group_id,
                    cluster,
                    command: command_name,
                    args,
                    endpoint,
                },
        } => match mat_core::ids::classify_invoke(cluster, command_name, args) {
            mat_core::ids::InvokeClass::NotNative => None,
            mat_core::ids::InvokeClass::Reject(msg) => Some(Err(MatError::parse_error(msg))),
            mat_core::ids::InvokeClass::Native {
                command: cmd_id,
                fields,
                ..
            } => {
                let cluster_id = mat_core::ids::resolve_cluster(cluster)
                    .expect("classify_invoke already resolved this cluster name");
                let fields_tlv = if fields.is_empty() {
                    None
                } else {
                    Some(mat_native::encode_command_fields(&fields))
                };
                Some(Ok(NativeOp::GroupInvokeGeneric {
                    group_id: group_id.id(),
                    cluster_in: cluster.clone(),
                    command_in: command_name.clone(),
                    cluster: cluster_id,
                    command: cmd_id,
                    fields_tlv,
                    endpoint: *endpoint,
                }))
            }
        },
        _ => None,
    }
}

enum Executed {
    Done,
    Fallback,
}

/// `run_op` の結果。`Fallback` は「op 自体は native 対象だが、この呼び出しでは
/// native で完遂できない事情（group native 未整備等）」で、chip-tool 直へ
/// フォールバックする合図（`Executed::Fallback` へそのまま写す）。
#[derive(Debug)]
enum RunOutcome {
    Done,
    Fallback,
}

/// 直経路 native の入口。None = chip-tool 直で実行すべき
/// （非対象 op / エンジン構築不可 / group native 不可）。
pub(crate) fn try_run(
    command: &Command,
    store_path: &Path,
    cfg: &Config,
) -> Option<Result<(), MatError>> {
    let op = match classify(command) {
        Some(op) => op,
        None => match classify_strict(command)? {
            Ok(op) => op,
            // 値が符号化不能（非スカラー型等）: フォールバックせず即 parse_error
            // （spec 決定3。chip-tool 側では通る形をあえて拒む opt-in の縮小）。
            Err(e) => return Some(Err(e)),
        },
    };
    match execute(&op, store_path, cfg) {
        Ok(Executed::Done) => Some(Ok(())),
        Ok(Executed::Fallback) => None,
        Err(e) => Some(Err(e)),
    }
}

fn execute(op: &NativeOp, store_path: &Path, cfg: &Config) -> Result<Executed, MatError> {
    // store / commission チェックは chip-tool 経路と同一の順序・エラー(exit 10/11)。
    let store = Store::open(store_path)?;
    // group 送信 3 形は require_node をしない（chip-tool 経路の
    // `commands::group::send` と同じ — 特定ノード宛ではないため）。
    let node_id = match op {
        NativeOp::On { node_id, .. }
        | NativeOp::Off { node_id, .. }
        | NativeOp::ReadOnOff { node_id, .. }
        | NativeOp::Color { node_id, .. }
        | NativeOp::ColorTemp { node_id, .. }
        | NativeOp::ReadAttr { node_id, .. }
        | NativeOp::WriteAttr { node_id, .. }
        | NativeOp::InvokeGeneric { node_id, .. }
        | NativeOp::Describe { node_id, .. }
        | NativeOp::DiagThread { node_id, .. }
        | NativeOp::OpenWindow { node_id, .. } => Some(*node_id),
        NativeOp::GroupOnOff { .. }
        | NativeOp::GroupColor { .. }
        | NativeOp::GroupColorTemp { .. }
        | NativeOp::GroupInvokeGeneric { .. } => None,
        // provision / grant は複数ノード宛（`node_id: Option<u64>` に収まらない）
        // ので、ここで別途 require_node する（chip-tool 経路の `provision`/
        // `grant` と同じ「1つでも未 commission なら exit 11」）。
        NativeOp::GroupProvision { node_ids, .. } | NativeOp::GroupGrant { node_ids, .. } => {
            for &id in node_ids {
                store.require_node(id)?;
            }
            None
        }
    };
    if let Some(id) = node_id {
        store.require_node(id)?;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            MatError::new(
                mat_core::error::ErrorKind::Other,
                format!("tokio runtime: {e}"),
            )
        })?;
    rt.block_on(async {
        let native_cfg = NativeConfig {
            store: store.root().to_path_buf(),
            iface: cfg.iface.to_string(),
            fabric_index: cfg.fabric_index,
            issuer_index: cfg.issuer_index,
        };
        let engine = match Engine::build(&native_cfg).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e.detail, "native direct build failed; falling back to chip-tool");
                return Ok(Executed::Fallback);
            }
        };
        match run_op(&engine, op, store.root()).await? {
            RunOutcome::Done => Ok(Executed::Done),
            RunOutcome::Fallback => Ok(Executed::Fallback),
        }
    })
}

/// 確立 → 1 op → 破棄。値を返す op（read）は emit まで行う。unicast 4 形は
/// 常に `Done`。group 3 形は `engine.group` 未設定 / `GroupOutcome::Unavailable`
/// のとき `Fallback` を返し、chip-tool 直へ譲る（matd の native_group_params
/// と対の判定を CLI 直経路で再現）。`store_root` は `GroupProvision` がコントローラ側
/// group state（chip-tool 経由、KVS 書込所有は M8c まで chip-tool）を実行するのに使う
/// —— 他の op には無関係（未使用でも警告にならないよう Group* 系のみが読む）。
async fn run_op(engine: &Engine, op: &NativeOp, store_root: &Path) -> Result<RunOutcome, MatError> {
    use mat_controller::im;
    match op {
        NativeOp::On { node_id, endpoint } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(
                *endpoint,
                im::CLUSTER_ON_OFF,
                im::CMD_ON_OFF_ON,
                None,
                false,
            )
            .await?;
            crate::commands::invoke::emit_invoke_success(*node_id, *endpoint, "onoff", "on");
        }
        NativeOp::Off { node_id, endpoint } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(
                *endpoint,
                im::CLUSTER_ON_OFF,
                im::CMD_ON_OFF_OFF,
                None,
                false,
            )
            .await?;
            crate::commands::invoke::emit_invoke_success(*node_id, *endpoint, "onoff", "off");
        }
        NativeOp::ReadOnOff { node_id, endpoint } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            let v = conn.read_onoff(*endpoint).await?;
            crate::commands::read::emit_read_success(
                *node_id,
                *endpoint,
                "onoff",
                "on-off",
                serde_json::json!(v),
            );
        }
        NativeOp::Color {
            node_id,
            endpoint,
            color,
            transition,
        } => {
            let fields = im::encode_move_to_hue_and_saturation_fields(
                color.hue_raw,
                color.sat_raw,
                *transition,
            );
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(
                *endpoint,
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_HUE_AND_SATURATION,
                Some(fields),
                false,
            )
            .await?;
            crate::commands::invoke::emit_color_success(*node_id, *endpoint, color, *transition);
        }
        NativeOp::ColorTemp {
            node_id,
            endpoint,
            kelvin,
            mireds,
            transition,
        } => {
            let fields = im::encode_move_to_color_temperature_fields(*mireds, *transition);
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(
                *endpoint,
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_COLOR_TEMPERATURE,
                Some(fields),
                false,
            )
            .await?;
            crate::commands::invoke::emit_color_temp_success(
                *node_id,
                *endpoint,
                *kelvin,
                *mireds,
                *transition,
            );
        }
        NativeOp::GroupOnOff {
            group_id,
            command_id,
            command,
            endpoint,
        } => {
            let Some(ctx) = &engine.group else {
                tracing::warn!("native group context not configured; falling back to chip-tool");
                return Ok(RunOutcome::Fallback);
            };
            match mat_native::group::send(ctx, *group_id, im::CLUSTER_ON_OFF, *command_id, None)
                .await?
            {
                GroupOutcome::Sent => {
                    crate::commands::group::emit_invoke_sent(
                        *group_id, "onoff", command, *endpoint,
                    );
                }
                GroupOutcome::Unavailable(reason) => {
                    tracing::warn!(
                        group_id,
                        reason,
                        "native group send unavailable; falling back to chip-tool"
                    );
                    return Ok(RunOutcome::Fallback);
                }
            }
        }
        NativeOp::GroupColor {
            group_id,
            color,
            transition,
            endpoint,
        } => {
            let Some(ctx) = &engine.group else {
                tracing::warn!("native group context not configured; falling back to chip-tool");
                return Ok(RunOutcome::Fallback);
            };
            let fields = im::encode_move_to_hue_and_saturation_fields(
                color.hue_raw,
                color.sat_raw,
                *transition,
            );
            match mat_native::group::send(
                ctx,
                *group_id,
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_HUE_AND_SATURATION,
                Some(fields),
            )
            .await?
            {
                GroupOutcome::Sent => {
                    crate::commands::group::emit_color_sent(
                        *group_id,
                        color,
                        *transition,
                        *endpoint,
                    );
                }
                GroupOutcome::Unavailable(reason) => {
                    tracing::warn!(
                        group_id,
                        reason,
                        "native group send unavailable; falling back to chip-tool"
                    );
                    return Ok(RunOutcome::Fallback);
                }
            }
        }
        NativeOp::GroupColorTemp {
            group_id,
            kelvin,
            mireds,
            transition,
            endpoint,
        } => {
            let Some(ctx) = &engine.group else {
                tracing::warn!("native group context not configured; falling back to chip-tool");
                return Ok(RunOutcome::Fallback);
            };
            let fields = im::encode_move_to_color_temperature_fields(*mireds, *transition);
            match mat_native::group::send(
                ctx,
                *group_id,
                im::CLUSTER_COLOR_CONTROL,
                im::CMD_MOVE_TO_COLOR_TEMPERATURE,
                Some(fields),
            )
            .await?
            {
                GroupOutcome::Sent => {
                    crate::commands::group::emit_color_temp_sent(
                        *group_id,
                        *kelvin,
                        *mireds,
                        *transition,
                        *endpoint,
                    );
                }
                GroupOutcome::Unavailable(reason) => {
                    tracing::warn!(
                        group_id,
                        reason,
                        "native group send unavailable; falling back to chip-tool"
                    );
                    return Ok(RunOutcome::Fallback);
                }
            }
        }
        NativeOp::GroupInvokeGeneric {
            group_id,
            cluster_in,
            command_in,
            cluster,
            command,
            fields_tlv,
            endpoint,
        } => {
            let Some(ctx) = &engine.group else {
                tracing::warn!("native group context not configured; falling back to chip-tool");
                return Ok(RunOutcome::Fallback);
            };
            match mat_native::group::send(ctx, *group_id, *cluster, *command, fields_tlv.clone())
                .await?
            {
                GroupOutcome::Sent => {
                    crate::commands::group::emit_invoke_sent(
                        *group_id, cluster_in, command_in, *endpoint,
                    );
                }
                GroupOutcome::Unavailable(reason) => {
                    tracing::warn!(
                        group_id,
                        reason,
                        "native group send unavailable; falling back to chip-tool"
                    );
                    return Ok(RunOutcome::Fallback);
                }
            }
        }
        NativeOp::GroupProvision {
            group_id,
            node_ids,
            keyset_id,
            name,
            endpoint,
            epoch_key,
            rebind,
        } => {
            // 1) コントローラ側 group state（chip-tool、KVS 書込所有は M8c まで
            //    chip-tool 側 —— native 化しない）。デバイス側と同じ epoch key を
            //    使う必要があるため、解決結果（hex 文字列）を受け取る。
            let chip = crate::runner::ChipTool::new(store_root);
            let epoch_key_hex = crate::commands::group::provision_controller_state(
                &chip,
                *group_id,
                *keyset_id,
                name,
                epoch_key.as_deref(),
                *rebind,
            )?;
            let epoch_key_bytes = mat_native::ops::epoch_key_from_hex(&epoch_key_hex)?;

            // 2) 各デバイスへ provision（native, unicast）。最初の失敗で停止
            //    （chip-tool 経路と同じ fail-fast、部分結果を stdout に出さない）。
            for &node_id in node_ids {
                let mut conn = engine.establisher.establish(node_id).await?;
                let p = mat_native::ops::ProvisionNodeParams {
                    group_id: *group_id,
                    keyset_id: *keyset_id,
                    name: name.clone(),
                    endpoint: *endpoint,
                    epoch_key: epoch_key_bytes,
                };
                mat_native::ops::provision_node(&mut *conn, &p)
                    .await
                    .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?;
            }

            crate::commands::group::emit_provision_success(
                *group_id, *keyset_id, name, *endpoint, node_ids, *rebind,
            );
        }
        NativeOp::GroupGrant { group_id, node_ids } => {
            let mut updated: Vec<u64> = Vec::new();
            let mut unchanged: Vec<u64> = Vec::new();
            for &node_id in node_ids {
                let mut conn = engine.establisher.establish(node_id).await?;
                if mat_native::ops::ensure_group_acl(&mut *conn, *group_id)
                    .await
                    .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?
                {
                    updated.push(node_id);
                } else {
                    unchanged.push(node_id);
                }
            }
            crate::commands::group::emit_grant_success(*group_id, node_ids, &updated, &unchanged);
        }
        NativeOp::ReadAttr {
            node_id,
            endpoint,
            cluster_in,
            attribute_in,
            cluster,
            attribute,
        } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            let v = conn.read_json(*endpoint, *cluster, *attribute).await?;
            crate::commands::read::emit_read_success(
                *node_id,
                *endpoint,
                cluster_in,
                attribute_in,
                v,
            );
        }
        NativeOp::WriteAttr {
            node_id,
            endpoint,
            cluster_in,
            attribute_in,
            cluster,
            attribute,
            value_in,
            value,
            timed,
        } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.write_tlv(
                *endpoint,
                *cluster,
                *attribute,
                mat_native::scalar_to_tlv(value),
                *timed,
            )
            .await?;
            crate::commands::write::emit_write_success(
                *node_id,
                *endpoint,
                cluster_in,
                attribute_in,
                value_in,
            );
        }
        NativeOp::InvokeGeneric {
            node_id,
            endpoint,
            cluster_in,
            command_in,
            cluster,
            command,
            fields_tlv,
            timed,
        } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(*endpoint, *cluster, *command, fields_tlv.clone(), *timed)
                .await?;
            crate::commands::invoke::emit_invoke_success(
                *node_id, *endpoint, cluster_in, command_in,
            );
        }
        NativeOp::Describe { node_id } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            let endpoints = mat_native::ops::describe(&mut *conn).await?;
            crate::commands::describe::emit_describe_success(*node_id, &endpoints);
        }
        NativeOp::DiagThread { node_id, endpoint } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            let snap = mat_native::ops::diag_thread(&mut *conn, *endpoint).await?;
            // wildcard read は per-attribute の失敗を出さないため native 経路の
            // unavailable は通常空だが、スキーマ整合のため chip-tool 経路と同じ
            // 形（{"attribute", "kind"}）へ変換して渡す。
            let unavailable: Vec<serde_json::Value> = snap
                .unavailable
                .iter()
                .map(|(attr, kind)| {
                    serde_json::json!({
                        "attribute": attr,
                        "kind": serde_json::to_value(kind).unwrap_or(serde_json::Value::Null),
                    })
                })
                .collect();
            crate::commands::diag::emit_diag_thread_success(
                *node_id,
                *endpoint,
                snap.fields,
                unavailable,
            );
        }
        NativeOp::OpenWindow {
            node_id,
            timeout,
            iteration,
            discriminator,
        } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            // timeout は chip-tool 経路と同じ u32 CLI 値、window API は u16
            // （spec 上 window timeout は 16-bit）。飽和させて渡す。
            let timeout_u16 = u16::try_from(*timeout).unwrap_or(u16::MAX);
            let (manual_code, qr_payload) = conn
                .open_window(timeout_u16, *discriminator, *iteration)
                .await?;
            crate::commands::open_window::emit_open_window_success(
                *node_id,
                &manual_code,
                &qr_payload,
                *timeout,
            );
        }
    }
    Ok(RunOutcome::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_off_read_onoff_and_color_shapes_are_native() {
        use mat_core::alias::{EndpointRef, NodeRef};
        let on = Command::On {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
        };
        assert!(matches!(
            classify(&on),
            Some(NativeOp::On {
                node_id: 5,
                endpoint: 1
            })
        ));
        let off = Command::Off {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
        };
        assert!(matches!(
            classify(&off),
            Some(NativeOp::Off {
                node_id: 5,
                endpoint: 1
            })
        ));
        let read = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "onoff".into(),
            attribute: "on-off".into(),
        };
        assert!(matches!(classify(&read), Some(NativeOp::ReadOnOff { .. })));
        // 汎用 read（onoff on-off 以外）で名前解決できるものは M8a Task7 で native
        // 対象に拡張された（`generic_read_is_native_when_names_resolve` 参照）。
        // 名前解決できないものは引き続き非対象 —— matd の is_native_hotpath とパリティ。
        let unresolvable = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "nosuchcluster".into(),
            attribute: "x".into(),
        };
        assert!(classify(&unresolvable).is_none());
        // discover は非対象（describe は M8a Task8 で native 対象化
        // — `describe_diag_thread_open_window_shapes_are_native` 参照）。
        assert!(classify(&Command::Discover { probe: false }).is_none());
    }

    #[test]
    fn color_temp_shape_is_native() {
        use mat_core::alias::{EndpointRef, NodeRef};
        let ct = Command::ColorTemp {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            kelvin: Some(2700),
            mireds: None,
            transition: 0,
        };
        assert!(matches!(
            classify(&ct),
            Some(NativeOp::ColorTemp {
                node_id: 5,
                endpoint: 1,
                kelvin: 2700,
                mireds: 370,
                transition: 0
            })
        ));
    }

    #[test]
    fn color_shape_is_native() {
        use crate::cli::ColorSpecArgs;
        use mat_core::alias::{EndpointRef, NodeRef};
        // classify は resolve::resolve_command 後（main.rs の呼び出し順）に呼ばれる
        // ため、name は既に rgb へ解決済みの形で届く（resolve_color_spec 参照）。
        let c = Command::Color {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            spec: ColorSpecArgs {
                name: None,
                rgb: Some("#ff0000".to_string()),
                hue: None,
                sat: None,
            },
            transition: 0,
        };
        assert!(matches!(
            classify(&c),
            Some(NativeOp::Color {
                node_id: 5,
                endpoint: 1,
                ..
            })
        ));
    }

    #[test]
    fn group_onoff_generic_provision_and_grant_are_all_native() {
        // M8a Task9: provision/grant のデバイス側 + 汎用 group invoke も native
        // 対象になった（旧テスト名 `..._is_not` は M7 当時の逆の期待だった —
        // 実態に合わせて改名）。
        use crate::cli::GroupCommand;
        use mat_core::alias::{GroupRef, NodeRef};
        let native = Command::Group {
            action: GroupCommand::Invoke {
                group_id: GroupRef::Id(10),
                cluster: "onoff".into(),
                command: "toggle".into(),
                args: vec![],
                endpoint: 1,
            },
        };
        assert!(matches!(
            classify(&native),
            Some(NativeOp::GroupOnOff { group_id: 10, .. })
        ));
        // 引数付き / onoff 以外の group invoke も、cluster/command 名前解決 +
        // 引数スカラー化ができれば native 対象（M8a Task9、classify_strict の
        // GroupInvokeGeneric 経由）。
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
            classify(&generic),
            Some(NativeOp::GroupInvokeGeneric {
                group_id: 10,
                fields_tlv: Some(_),
                ..
            })
        ));
        // provision / grant のデバイス側ステップも native 対象（コントローラ側
        // groupsettings は M8c まで chip-tool のまま — `run_op` 参照）。
        let grant = Command::Group {
            action: GroupCommand::Grant {
                group_id: GroupRef::Id(10),
                node_ids: vec![NodeRef::Id(5), NodeRef::Id(6)],
            },
        };
        assert!(matches!(
            classify(&grant),
            Some(NativeOp::GroupGrant { group_id: 10, node_ids }) if node_ids == vec![5, 6]
        ));
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
        assert!(matches!(
            classify(&provision),
            Some(NativeOp::GroupProvision {
                group_id: 10,
                keyset_id: 60,
                ref name,
                endpoint: 1,
                epoch_key: None,
                rebind: false,
                ..
            }) if name == "grp10"
        ));
    }

    #[test]
    fn group_color_and_color_temp_shapes_are_always_native() {
        use crate::cli::{ColorSpecArgs, GroupCommand};
        use mat_core::alias::GroupRef;
        let ct = Command::Group {
            action: GroupCommand::ColorTemp {
                group_id: GroupRef::Id(10),
                kelvin: Some(2700),
                mireds: None,
                transition: 0,
                endpoint: 1,
            },
        };
        assert!(matches!(
            classify(&ct),
            Some(NativeOp::GroupColorTemp {
                group_id: 10,
                kelvin: 2700,
                mireds: 370,
                transition: 0,
                endpoint: 1,
            })
        ));
        let color = Command::Group {
            action: GroupCommand::Color {
                group_id: GroupRef::Id(10),
                spec: ColorSpecArgs {
                    name: None,
                    rgb: Some("#ff0000".to_string()),
                    hue: None,
                    sat: None,
                },
                transition: 0,
                endpoint: 1,
            },
        };
        assert!(matches!(
            classify(&color),
            Some(NativeOp::GroupColor { group_id: 10, .. })
        ));
    }

    #[tokio::test]
    async fn group_onoff_falls_back_when_engine_group_ctx_unconfigured() {
        // engine.group == None（Config::build 中の group ctx 準備前に相当するテスト
        // 経路）: run_op は Fallback を返し、chip-tool 直に譲る。
        use mat_native::test_support::FakeEstablisher;
        let engine = mat_native::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        let outcome = run_op(
            &engine,
            &NativeOp::GroupOnOff {
                group_id: 10,
                command_id: mat_controller::im::CMD_ON_OFF_TOGGLE,
                command: "toggle",
                endpoint: 1,
            },
            Path::new("/nonexistent-store"),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, RunOutcome::Fallback));
    }

    #[tokio::test]
    async fn one_shot_does_not_retry_on_timeout() {
        use mat_core::error::ErrorKind;
        use mat_native::test_support::FakeEstablisher;
        use std::sync::atomic::Ordering;
        // 確立直後の送信 Timeout: one-shot は再確立せずそのまま返す（matd と違い
        // stale session はあり得ないため。chip-tool one-shot の失敗と同じ扱い）。
        let est = FakeEstablisher {
            calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            fail_first_send: true,
            fail_kind: ErrorKind::Timeout,
        };
        let calls = std::sync::Arc::clone(&est.calls);
        let engine = mat_native::Engine::with_parts(Box::new(est), None);
        let err = run_op(
            &engine,
            &NativeOp::ReadOnOff {
                node_id: 5,
                endpoint: 1,
            },
            Path::new("/nonexistent-store"),
        )
        .await
        .expect_err("timeout must surface");
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn one_shot_invoke_succeeds_via_engine() {
        use mat_native::test_support::FakeEstablisher;
        let engine = mat_native::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        run_op(
            &engine,
            &NativeOp::On {
                node_id: 5,
                endpoint: 1,
            },
            Path::new("/nonexistent-store"),
        )
        .await
        .unwrap();
    }

    #[test]
    fn generic_read_is_native_when_names_resolve() {
        use mat_core::alias::{EndpointRef, NodeRef};
        let read = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            attribute: "current-level".into(),
        };
        assert!(matches!(
            classify(&read),
            Some(NativeOp::ReadAttr {
                cluster: 0x0008,
                attribute: 0x0000,
                ..
            })
        ));
        // 未知クラスタ名は chip-tool へ（互換）。
        let unknown = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "nosuch".into(),
            attribute: "x".into(),
        };
        assert!(classify(&unknown).is_none());
        // 数値直指定も native。
        let byid = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "0x0008".into(),
            attribute: "0".into(),
        };
        assert!(matches!(classify(&byid), Some(NativeOp::ReadAttr { .. })));
    }

    #[test]
    fn write_scalar_native_and_list_rejected() {
        use mat_core::alias::{EndpointRef, NodeRef};
        let w = Command::Write {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            attribute: "on-level".into(),
            value: "128".into(),
        };
        assert!(matches!(classify(&w), Some(NativeOp::WriteAttr { .. })));
        // list 型（acl）への汎用 write は parse_error（classify_strict 経由で確認）。
        let acl = Command::Write {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "accesscontrol".into(),
            attribute: "acl".into(),
            value: "[]".into(),
        };
        let err = classify_strict(&acl).unwrap().unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
        assert!(err.detail.contains("list"), "{}", err.detail);
    }

    #[test]
    fn generic_invoke_scalar_args_native_and_bad_args_rejected() {
        use mat_core::alias::{EndpointRef, NodeRef};
        let inv = Command::Invoke {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            command: "move-to-level".into(),
            args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
        };
        assert!(matches!(
            classify(&inv),
            Some(NativeOp::InvokeGeneric { .. })
        ));
        // struct field を要求するコマンド（key-set-write）への引数 → parse_error。
        let ks = Command::Invoke {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "groupkeymanagement".into(),
            command: "key-set-write".into(),
            args: vec!["{}".into()],
        };
        let err = classify_strict(&ks).unwrap().unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
    }

    #[test]
    fn describe_diag_thread_open_window_shapes_are_native() {
        use crate::cli::DiagCommand;
        use mat_core::alias::{EndpointRef, NodeRef};
        let describe = Command::Describe {
            node_id: NodeRef::Id(5),
        };
        assert!(matches!(
            classify(&describe),
            Some(NativeOp::Describe { node_id: 5 })
        ));

        let diag_thread = Command::Diag {
            action: DiagCommand::Thread {
                node_id: NodeRef::Id(5),
                endpoint: EndpointRef::Id(0),
            },
        };
        assert!(matches!(
            classify(&diag_thread),
            Some(NativeOp::DiagThread {
                node_id: 5,
                endpoint: 0
            })
        ));

        // `diag node` は probe 混在のため引き続き非対象（chip-tool 直）。
        let diag_node = Command::Diag {
            action: DiagCommand::Node {
                node_id: NodeRef::Id(5),
                endpoint: EndpointRef::Id(0),
                deep: false,
            },
        };
        assert!(classify(&diag_node).is_none());

        // discriminator 明示指定はそのまま使う。
        let ow = Command::OpenWindow {
            node_id: NodeRef::Id(5),
            timeout: 180,
            iteration: 1000,
            discriminator: Some(3840),
        };
        assert!(matches!(
            classify(&ow),
            Some(NativeOp::OpenWindow {
                node_id: 5,
                timeout: 180,
                iteration: 1000,
                discriminator: 3840,
            })
        ));
        // discriminator 未指定は node_id % 4096 で決定的に補完（main.rs と同じ式）。
        let ow_default = Command::OpenWindow {
            node_id: NodeRef::Id(5),
            timeout: 180,
            iteration: 1000,
            discriminator: None,
        };
        assert!(matches!(
            classify(&ow_default),
            Some(NativeOp::OpenWindow {
                discriminator: 5,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn open_window_runs_via_fake_and_emits_codes() {
        // fake 経由で run_op(OpenWindow) が establish → open_window → emit まで
        // 完走することを確認する（emit 先の stdout 内容そのものは
        // `emit_open_window_success` の既存ユニットテストで担保済み）。
        use mat_native::test_support::FakeEstablisher;
        let engine = mat_native::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        run_op(
            &engine,
            &NativeOp::OpenWindow {
                node_id: 5,
                timeout: 180,
                iteration: 1000,
                discriminator: 3840,
            },
            Path::new("/nonexistent-store"),
        )
        .await
        .unwrap();
    }
}
