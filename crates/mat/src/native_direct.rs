//! one-shot 直経路の native 実行（M7）。
//!
//! matd 稼働中は matd が優先される（main.rs の経路順）。ここに来るのは直経路のみで、
//! native 対象 op を mat-controller で in-process 実行する。
//! warm セッションは持たない: 確立 → 1 op → 破棄（設計ルール 4）。matd と違い
//! Timeout 再確立はしない（確立直後の session が stale なことはない）。
//!
//! M8c-3（chip-tool 撤去）: native がこれら op の唯一の経路。従来 chip-tool 直へ
//! フォールバックしていた分岐（エンジン構築失敗・group native 不可・名前未解決）は
//! すべてハードエラー化した（`run` は非対象 op のみ `None` を返す — discover /
//! commission / diag node など専用コマンド層を持つ op）。

use std::path::Path;

use mat_controller::im;
use mat_core::color::ResolvedColor;
use mat_core::error::MatError;
use mat_core::ids::ScalarValue;
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
    Level {
        node_id: u64,
        endpoint: u16,
        percent: u8,
        level: u8,
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
    GroupLevel {
        group_id: u16,
        percent: u8,
        level: u8,
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

pub(crate) fn classify(command: &Command) -> Option<Result<NativeOp, MatError>> {
    classify_inner(command).transpose()
}

/// `Ok(None)` = native 高速路の対象外（classify_strict へ）。`Err` = 未解決
/// alias が届いた内部バグ（alias.rs `id()` 参照）。
fn classify_inner(command: &Command) -> Result<Option<NativeOp>, MatError> {
    Ok(Some(match command {
        Command::On { node_id, endpoint } => NativeOp::On {
            node_id: node_id.id()?,
            endpoint: endpoint.id()?,
        },
        Command::Off { node_id, endpoint } => NativeOp::Off {
            node_id: node_id.id()?,
            endpoint: endpoint.id()?,
        },
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } if cluster == "onoff" && attribute == "on-off" => NativeOp::ReadOnOff {
            node_id: node_id.id()?,
            endpoint: endpoint.id()?,
        },
        Command::ColorTemp {
            node_id,
            endpoint,
            kelvin,
            mireds,
            transition,
        } => {
            let (mireds, kelvin) = crate::commands::invoke::resolve_color_temp(*kelvin, *mireds);
            NativeOp::ColorTemp {
                node_id: node_id.id()?,
                endpoint: endpoint.id()?,
                kelvin,
                mireds,
                transition: *transition,
            }
        }
        Command::Level {
            node_id,
            endpoint,
            percent,
            transition,
        } => {
            let level = crate::commands::invoke::resolve_level(*percent);
            NativeOp::Level {
                node_id: node_id.id()?,
                endpoint: endpoint.id()?,
                percent: *percent,
                level,
                transition: *transition,
            }
        }
        Command::Color {
            node_id,
            endpoint,
            spec,
            transition,
        } => {
            // 不正 color spec はここで None → `run` の `unresolved_op_error` が
            // `resolve_spec` 本来のエラーを surface する（挙動は決定的で一致）。
            let Ok(c) = mat_core::color::resolve_spec(
                spec.name.as_deref(),
                spec.rgb.as_deref(),
                spec.hue,
                spec.sat,
            ) else {
                return Ok(None);
            };
            NativeOp::Color {
                node_id: node_id.id()?,
                endpoint: endpoint.id()?,
                color: c,
                transition: *transition,
            }
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
                _ => return Ok(None),
            };
            NativeOp::GroupOnOff {
                group_id: group_id.id()?,
                command_id,
                command,
                endpoint: *endpoint,
            }
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
            NativeOp::GroupColorTemp {
                group_id: group_id.id()?,
                kelvin,
                mireds,
                transition: *transition,
                endpoint: *endpoint,
            }
        }
        Command::Group {
            action:
                GroupCommand::Level {
                    group_id,
                    percent,
                    transition,
                    endpoint,
                },
        } => {
            let level = crate::commands::invoke::resolve_level(*percent);
            NativeOp::GroupLevel {
                group_id: group_id.id()?,
                percent: *percent,
                level,
                transition: *transition,
                endpoint: *endpoint,
            }
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
            let Ok(c) = mat_core::color::resolve_spec(
                spec.name.as_deref(),
                spec.rgb.as_deref(),
                spec.hue,
                spec.sat,
            ) else {
                return Ok(None);
            };
            NativeOp::GroupColor {
                group_id: group_id.id()?,
                color: c,
                transition: *transition,
                endpoint: *endpoint,
            }
        }
        // provision / grant（M8a Task9）: デバイス側 4 ステップ（KeySetWrite /
        // group-key-map / AddGroup / ACL）を native 化。コントローラ側
        // groupsettings（KVS 書込）も native（M8c-2; `run_op` の
        // `NativeOp::GroupProvision` 実装 = `write_group_provision` を参照）。
        // name 未指定時の既定補完（`grp<id>`）は決定的な共有式。
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
            let gid = group_id.id()?;
            let resolved_name = name.clone().unwrap_or_else(|| format!("grp{gid}"));
            let resolved_nodes: Result<Vec<u64>, MatError> =
                node_ids.iter().map(NodeRef::id).collect();
            NativeOp::GroupProvision {
                group_id: gid,
                node_ids: resolved_nodes?,
                keyset_id: *keyset_id,
                name: resolved_name,
                endpoint: *endpoint,
                epoch_key: epoch_key.clone(),
                rebind: *rebind,
            }
        }
        Command::Group {
            action: GroupCommand::Grant { group_id, node_ids },
        } => {
            let resolved_nodes: Result<Vec<u64>, MatError> =
                node_ids.iter().map(NodeRef::id).collect();
            NativeOp::GroupGrant {
                group_id: group_id.id()?,
                node_ids: resolved_nodes?,
            }
        }
        // describe / diag thread / open-window（M8a Task8）: 値の符号化を
        // 伴わない読み取り専用 op なので、classify_strict と違い常に
        // Some/None（Err にはならない）。
        Command::Describe { node_id } => NativeOp::Describe {
            node_id: node_id.id()?,
        },
        Command::Diag {
            action: DiagCommand::Thread { node_id, endpoint },
        } => NativeOp::DiagThread {
            node_id: node_id.id()?,
            endpoint: endpoint.id()?,
        },
        // `Diag Node` は probe（ping6/native mDNS resolve）混在のため `run` の担当外
        // （専用コマンド層 `commands::diag::node` が native IM probe + 補助
        // プローブを実施する）。
        Command::OpenWindow {
            node_id,
            timeout,
            iteration,
            discriminator,
        } => {
            let nid = node_id.id()?;
            NativeOp::OpenWindow {
                node_id: nid,
                timeout: *timeout,
                iteration: *iteration,
                discriminator: resolve_discriminator(nid, *discriminator),
            }
        }
        // 汎用 read/write/invoke（M8a）: classify_strict の判定を再利用し、値の
        // 符号化不能（Err）はここでは黙って None（chip-tool 直路）に丸める —
        // その Err を明示的に拒否（即 parse_error）したい呼び出し側は
        // `classify_strict` を直接使う（`try_run` がそうしている）。
        _ => match classify_strict(command) {
            Some(Ok(op)) => op,
            _ => return Ok(None),
        },
    }))
}

/// 汎用形の分類: None = 名前未解決（`run` が parse_error 化）、Some(Ok) = native
/// 実行、Some(Err) = 値が符号化不能 → parse_error（spec 決定3: 明示拒否。撤去前の
/// chip-tool なら通る形をあえて拒む意図した縮小）。
pub(crate) fn classify_strict(command: &Command) -> Option<Result<NativeOp, MatError>> {
    classify_strict_inner(command).transpose()
}

/// `classify_strict` の inner。`Ok(None)` = 名前未解決（NotNative / catch-all）、
/// `Err` = 値が符号化不能（Reject）**または**未解決 alias が届いた内部バグ。
fn classify_strict_inner(command: &Command) -> Result<Option<NativeOp>, MatError> {
    Ok(Some(match command {
        Command::Read {
            node_id,
            endpoint,
            cluster,
            attribute,
        } => {
            let Some(cluster_id) = mat_core::ids::resolve_cluster(cluster) else {
                return Ok(None);
            };
            let Some(attr) = mat_core::ids::resolve_attribute(cluster_id, attribute) else {
                return Ok(None);
            };
            NativeOp::ReadAttr {
                node_id: node_id.id()?,
                endpoint: endpoint.id()?,
                cluster_in: cluster.clone(),
                attribute_in: attribute.clone(),
                cluster: cluster_id,
                attribute: attr.id,
            }
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
            mat_core::ids::WriteClass::NotNative => return Ok(None),
            mat_core::ids::WriteClass::Reject(msg) => return Err(MatError::parse_error(msg)),
            mat_core::ids::WriteClass::Native {
                cluster: cluster_id,
                attribute: attr_id,
                value: scalar,
                timed,
            } => NativeOp::WriteAttr {
                node_id: node_id.id()?,
                endpoint: endpoint.id()?,
                cluster_in: cluster.clone(),
                attribute_in: attribute.clone(),
                cluster: cluster_id,
                attribute: attr_id,
                value_in: value.clone(),
                value: scalar,
                timed,
            },
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
            mat_core::ids::InvokeClass::NotNative => return Ok(None),
            mat_core::ids::InvokeClass::Reject(msg) => return Err(MatError::parse_error(msg)),
            mat_core::ids::InvokeClass::Native {
                cluster: cluster_id,
                command: cmd_id,
                fields,
                timed,
            } => {
                let fields_tlv = if fields.is_empty() {
                    None
                } else {
                    Some(mat_native::encode_command_fields(&fields))
                };
                NativeOp::InvokeGeneric {
                    node_id: node_id.id()?,
                    endpoint: endpoint.id()?,
                    cluster_in: cluster.clone(),
                    command_in: command_name.clone(),
                    cluster: cluster_id,
                    command: cmd_id,
                    fields_tlv,
                    timed,
                }
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
            mat_core::ids::InvokeClass::NotNative => return Ok(None),
            mat_core::ids::InvokeClass::Reject(msg) => return Err(MatError::parse_error(msg)),
            mat_core::ids::InvokeClass::Native {
                cluster: cluster_id,
                command: cmd_id,
                fields,
                ..
            } => {
                let fields_tlv = if fields.is_empty() {
                    None
                } else {
                    Some(mat_native::encode_command_fields(&fields))
                };
                NativeOp::GroupInvokeGeneric {
                    group_id: group_id.id()?,
                    cluster_in: cluster.clone(),
                    command_in: command_name.clone(),
                    cluster: cluster_id,
                    command: cmd_id,
                    fields_tlv,
                    endpoint: *endpoint,
                }
            }
        },
        _ => return Ok(None),
    }))
}

/// 直経路 native の入口（M8c-3）。`None` は「この op は native_direct の担当外」
/// = discover / commission / diag node など専用コマンド層を持つ op のみ。
/// それ以外の op は必ず `Some(Result)` を返す（chip-tool フォールバックは撤去）:
/// - 名前解決できた op → native 実行結果。
/// - 名前未解決（`classify` / `classify_strict` とも該当なし）→ `parse_error`
///   （detail: unknown cluster/attribute/command name; 数値 ID は従来どおり受理）。
/// - 値が符号化不能（`classify_strict` の `Err`）→ `parse_error`。
pub(crate) fn run(
    command: &Command,
    store_path: &Path,
    cfg: &Config,
) -> Option<Result<(), MatError>> {
    // 専用コマンド層を持つ op は native_direct の担当外（呼び出し側が処理）。
    match command {
        Command::Discover { .. }
        | Command::Commission { .. }
        | Command::Fabric { .. }
        | Command::Diag {
            action: DiagCommand::Node { .. },
        }
        | Command::Diag {
            action: DiagCommand::Mesh { .. },
        } => return None,
        _ => {}
    }

    let op = match classify(command) {
        Some(Ok(op)) => op,
        // 未解決 alias が届いた内部バグ — typed error で JSON 契約を守る。
        Some(Err(e)) => return Some(Err(e)),
        None => match classify_strict(command) {
            Some(Ok(op)) => op,
            // 値が符号化不能（非スカラー型等）: 即 parse_error
            // （spec 決定3。chip-tool 側では通る形をあえて拒む opt-in の縮小）。
            Some(Err(e)) => return Some(Err(e)),
            // 名前未解決（名前→ID 表外）。chip-tool 撤去でフォールバック先が無い
            // ため、黙って落とさず parse_error にする（数値 ID は受理される）。
            None => return Some(Err(unresolved_op_error(command))),
        },
    };
    Some(execute(&op, store_path, cfg))
}

/// `classify` / `classify_strict` がともに非対象と判定した native 担当 op の
/// ハードエラー。Color / GroupColor だけは「不正 color spec」が失敗理由なので
/// `resolve_spec` 本来のエラーを surface する（撤去前の chip-tool 経路と同一の
/// 挙動を保つ）。それ以外は「名前未解決」= parse_error。
fn unresolved_op_error(command: &Command) -> MatError {
    let spec = match command {
        Command::Color { spec, .. } => Some(spec),
        Command::Group {
            action: GroupCommand::Color { spec, .. },
        } => Some(spec),
        _ => None,
    };
    if let Some(spec) = spec {
        if let Err(e) = mat_core::color::resolve_spec(
            spec.name.as_deref(),
            spec.rgb.as_deref(),
            spec.hue,
            spec.sat,
        ) {
            return e;
        }
    }
    MatError::parse_error(
        "unknown cluster/attribute/command name (or unsupported non-scalar type); \
         numeric IDs are accepted",
    )
}

/// エンジン構築失敗（M8c-3: chip-tool フォールバック撤去後のハードエラー化）。
/// `Engine::build` は KVS 資材の読取失敗を `store_missing` に写す（`mat-native`
/// 参照 — Io/NotFound と parse の細分化は将来）。ここでは store_missing に
/// 「`mat fabric init` で資材を作れ」の誘導を足して返す。他 kind はそのまま伝播。
fn map_engine_build_error(mut e: MatError) -> MatError {
    if e.kind == mat_core::error::ErrorKind::StoreMissing && !e.detail.contains("mat fabric init") {
        e.detail = format!(
            "{} — run `mat fabric init` to bootstrap the credential store",
            e.detail
        );
    }
    e
}

/// group ctx / group_settings ctx 未構成（本番 `Engine::build` では常に `Some`
/// なので実質到達しない — `with_parts` テスト注入時のみ `None`）。
fn group_ctx_unconfigured_error() -> MatError {
    MatError::new(
        mat_core::error::ErrorKind::Other,
        "native group context not configured (internal)",
    )
}

/// group 送信不能（未 provision・KVS 不備等）。撤去前は chip-tool フォールバック
/// だった。理由文字列に `mat group provision` 誘導を含む（`mat_native::group`）。
fn group_unavailable_error(reason: &str) -> MatError {
    MatError::store_parse(format!("native group send unavailable: {reason}"))
}

fn execute(op: &NativeOp, store_path: &Path, cfg: &Config) -> Result<(), MatError> {
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
        | NativeOp::Level { node_id, .. }
        | NativeOp::ReadAttr { node_id, .. }
        | NativeOp::WriteAttr { node_id, .. }
        | NativeOp::InvokeGeneric { node_id, .. }
        | NativeOp::Describe { node_id, .. }
        | NativeOp::DiagThread { node_id, .. }
        | NativeOp::OpenWindow { node_id, .. } => Some(*node_id),
        NativeOp::GroupOnOff { .. }
        | NativeOp::GroupColor { .. }
        | NativeOp::GroupColorTemp { .. }
        | NativeOp::GroupLevel { .. }
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
    // GroupProvision: CLI 指定 epoch key はバックエンド接触前に検証する
    // （不正入力に fail-fast。撤去前は provision_controller_state 冒頭で
    // resolve_epoch_key が最初に走っていた順序を保つ）。None = ランダム生成
    // （常に妥当）なのでここでは検証しない。
    if let NativeOp::GroupProvision {
        epoch_key: Some(k), ..
    } = op
    {
        mat_core::group::resolve_epoch_key(Some(k))?;
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
        let engine = Engine::build(&native_cfg)
            .await
            .map_err(map_engine_build_error)?;
        run_op(&engine, op).await
    })
}

async fn op_on(engine: &Engine, node_id: u64, endpoint: u16) -> Result<(), MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    conn.invoke(endpoint, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None, false)
        .await?;
    tracing::info!(
        node_id,
        cluster = "onoff",
        command = "on",
        "invoke executed (native direct)"
    );
    crate::commands::invoke::emit_invoke_success(node_id, endpoint, "onoff", "on");
    Ok(())
}

async fn op_off(engine: &Engine, node_id: u64, endpoint: u16) -> Result<(), MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    conn.invoke(
        endpoint,
        im::CLUSTER_ON_OFF,
        im::CMD_ON_OFF_OFF,
        None,
        false,
    )
    .await?;
    tracing::info!(
        node_id,
        cluster = "onoff",
        command = "off",
        "invoke executed (native direct)"
    );
    crate::commands::invoke::emit_invoke_success(node_id, endpoint, "onoff", "off");
    Ok(())
}

async fn op_read_onoff(engine: &Engine, node_id: u64, endpoint: u16) -> Result<(), MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    let v = conn.read_onoff(endpoint).await?;
    tracing::info!(
        node_id,
        cluster = "onoff",
        attribute = "on-off",
        "read executed (native direct)"
    );
    crate::commands::read::emit_read_success(
        node_id,
        endpoint,
        "onoff",
        "on-off",
        serde_json::json!(v),
    );
    Ok(())
}

async fn op_color(
    engine: &Engine,
    node_id: u64,
    endpoint: u16,
    color: &ResolvedColor,
    transition: u16,
) -> Result<(), MatError> {
    let fields =
        im::encode_move_to_hue_and_saturation_fields(color.hue_raw, color.sat_raw, transition);
    let mut conn = engine.establisher.establish(node_id).await?;
    conn.invoke(
        endpoint,
        im::CLUSTER_COLOR_CONTROL,
        im::CMD_MOVE_TO_HUE_AND_SATURATION,
        Some(fields),
        false,
    )
    .await?;
    tracing::info!(
        node_id,
        cluster = "colorcontrol",
        command = "move-to-hue-and-saturation",
        "invoke executed (native direct)"
    );
    crate::commands::invoke::emit_color_success(node_id, endpoint, color, transition);
    Ok(())
}

async fn op_color_temp(
    engine: &Engine,
    node_id: u64,
    endpoint: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
) -> Result<(), MatError> {
    let fields = im::encode_move_to_color_temperature_fields(mireds, transition);
    let mut conn = engine.establisher.establish(node_id).await?;
    conn.invoke(
        endpoint,
        im::CLUSTER_COLOR_CONTROL,
        im::CMD_MOVE_TO_COLOR_TEMPERATURE,
        Some(fields),
        false,
    )
    .await?;
    tracing::info!(
        node_id,
        cluster = "colorcontrol",
        command = "move-to-color-temperature",
        "invoke executed (native direct)"
    );
    crate::commands::invoke::emit_color_temp_success(node_id, endpoint, kelvin, mireds, transition);
    Ok(())
}

async fn op_level(
    engine: &Engine,
    node_id: u64,
    endpoint: u16,
    percent: u8,
    level: u8,
    transition: u16,
) -> Result<(), MatError> {
    let fields = im::encode_move_to_level_fields(level, transition);
    let mut conn = engine.establisher.establish(node_id).await?;
    conn.invoke(
        endpoint,
        im::CLUSTER_LEVEL_CONTROL,
        im::CMD_MOVE_TO_LEVEL,
        Some(fields),
        false,
    )
    .await?;
    tracing::info!(
        node_id,
        cluster = "levelcontrol",
        command = "move-to-level",
        "invoke executed (native direct)"
    );
    crate::commands::invoke::emit_level_success(node_id, endpoint, percent, level, transition);
    Ok(())
}

async fn op_group_onoff(
    engine: &Engine,
    group_id: u16,
    command_id: u32,
    command: &str,
    endpoint: u16,
) -> Result<(), MatError> {
    let Some(ctx) = &engine.group else {
        return Err(group_ctx_unconfigured_error());
    };
    match mat_native::group::send(ctx, group_id, im::CLUSTER_ON_OFF, command_id, None).await? {
        GroupOutcome::Sent => {
            crate::commands::group::emit_invoke_sent(group_id, "onoff", command, endpoint);
        }
        GroupOutcome::Unavailable(reason) => {
            return Err(group_unavailable_error(&reason));
        }
    }
    Ok(())
}

async fn op_group_color(
    engine: &Engine,
    group_id: u16,
    color: &ResolvedColor,
    transition: u16,
    endpoint: u16,
) -> Result<(), MatError> {
    let Some(ctx) = &engine.group else {
        return Err(group_ctx_unconfigured_error());
    };
    let fields =
        im::encode_move_to_hue_and_saturation_fields(color.hue_raw, color.sat_raw, transition);
    match mat_native::group::send(
        ctx,
        group_id,
        im::CLUSTER_COLOR_CONTROL,
        im::CMD_MOVE_TO_HUE_AND_SATURATION,
        Some(fields),
    )
    .await?
    {
        GroupOutcome::Sent => {
            crate::commands::group::emit_color_sent(group_id, color, transition, endpoint);
        }
        GroupOutcome::Unavailable(reason) => {
            return Err(group_unavailable_error(&reason));
        }
    }
    Ok(())
}

async fn op_group_color_temp(
    engine: &Engine,
    group_id: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
    endpoint: u16,
) -> Result<(), MatError> {
    let Some(ctx) = &engine.group else {
        return Err(group_ctx_unconfigured_error());
    };
    let fields = im::encode_move_to_color_temperature_fields(mireds, transition);
    match mat_native::group::send(
        ctx,
        group_id,
        im::CLUSTER_COLOR_CONTROL,
        im::CMD_MOVE_TO_COLOR_TEMPERATURE,
        Some(fields),
    )
    .await?
    {
        GroupOutcome::Sent => {
            crate::commands::group::emit_color_temp_sent(
                group_id, kelvin, mireds, transition, endpoint,
            );
        }
        GroupOutcome::Unavailable(reason) => {
            return Err(group_unavailable_error(&reason));
        }
    }
    Ok(())
}

async fn op_group_level(
    engine: &Engine,
    group_id: u16,
    percent: u8,
    level: u8,
    transition: u16,
    endpoint: u16,
) -> Result<(), MatError> {
    let Some(ctx) = &engine.group else {
        return Err(group_ctx_unconfigured_error());
    };
    let fields = im::encode_move_to_level_fields(level, transition);
    match mat_native::group::send(
        ctx,
        group_id,
        im::CLUSTER_LEVEL_CONTROL,
        im::CMD_MOVE_TO_LEVEL,
        Some(fields),
    )
    .await?
    {
        GroupOutcome::Sent => {
            crate::commands::group::emit_level_sent(group_id, percent, level, transition, endpoint);
        }
        GroupOutcome::Unavailable(reason) => {
            return Err(group_unavailable_error(&reason));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn op_group_invoke_generic(
    engine: &Engine,
    group_id: u16,
    cluster_in: &str,
    command_in: &str,
    cluster: u32,
    command: u32,
    fields_tlv: &Option<Vec<u8>>,
    endpoint: u16,
) -> Result<(), MatError> {
    let Some(ctx) = &engine.group else {
        return Err(group_ctx_unconfigured_error());
    };
    match mat_native::group::send(ctx, group_id, cluster, command, fields_tlv.clone()).await? {
        GroupOutcome::Sent => {
            crate::commands::group::emit_invoke_sent(group_id, cluster_in, command_in, endpoint);
        }
        GroupOutcome::Unavailable(reason) => {
            return Err(group_unavailable_error(&reason));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn op_group_provision(
    engine: &Engine,
    group_id: u16,
    node_ids: &[u64],
    keyset_id: u16,
    name: &str,
    endpoint: u16,
    epoch_key: Option<&str>,
    rebind: bool,
) -> Result<(), MatError> {
    // 1) コントローラ側 group state（M8c-2: native KVS 書込）。ctx 未構成
    //    は本番 Engine::build では起きない（Other 内部エラー）。書込エラーは
    //    hard error（ラッパー側 doc 参照 — flock WouldBlock 含む）。
    let Some(gs) = &engine.group_settings else {
        return Err(group_ctx_unconfigured_error());
    };
    let epoch_key_hex = mat_core::group::resolve_epoch_key(epoch_key)?;
    let epoch_key_bytes = mat_native::ops::epoch_key_from_hex(&epoch_key_hex)?;
    mat_native::group_settings::write_group_provision(
        gs,
        group_id,
        keyset_id,
        name,
        &epoch_key_bytes,
        rebind,
    )?;

    // 2) 各デバイスへ provision（native, unicast）— M8a のまま。
    for &node_id in node_ids {
        let mut conn = engine.establisher.establish(node_id).await?;
        let p = mat_native::ops::ProvisionNodeParams {
            group_id,
            keyset_id,
            name: name.to_string(),
            endpoint,
            epoch_key: epoch_key_bytes,
        };
        mat_native::ops::provision_node(&mut *conn, &p)
            .await
            .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?;
    }

    tracing::info!(
        group_id,
        keyset_id,
        "group provision executed (native direct)"
    );
    crate::commands::group::emit_provision_success(
        group_id, keyset_id, name, endpoint, node_ids, rebind, true,
    );
    Ok(())
}

async fn op_group_grant(engine: &Engine, group_id: u16, node_ids: &[u64]) -> Result<(), MatError> {
    let mut updated: Vec<u64> = Vec::new();
    let mut unchanged: Vec<u64> = Vec::new();
    for &node_id in node_ids {
        let mut conn = engine.establisher.establish(node_id).await?;
        if mat_native::ops::ensure_group_acl(&mut *conn, group_id)
            .await
            .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?
        {
            updated.push(node_id);
        } else {
            unchanged.push(node_id);
        }
    }
    tracing::info!(group_id, "group grant executed (native direct)");
    crate::commands::group::emit_grant_success(group_id, node_ids, &updated, &unchanged);
    Ok(())
}

async fn op_read_attr(
    engine: &Engine,
    node_id: u64,
    endpoint: u16,
    cluster_in: &str,
    attribute_in: &str,
    cluster: u32,
    attribute: u32,
) -> Result<(), MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    let v = conn.read_json(endpoint, cluster, attribute).await?;
    tracing::info!(node_id, cluster, attribute, "read executed (native direct)");
    crate::commands::read::emit_read_success(node_id, endpoint, cluster_in, attribute_in, v);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn op_write_attr(
    engine: &Engine,
    node_id: u64,
    endpoint: u16,
    cluster_in: &str,
    attribute_in: &str,
    cluster: u32,
    attribute: u32,
    value_in: &str,
    value: &ScalarValue,
    timed: bool,
) -> Result<(), MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    conn.write_tlv(
        endpoint,
        cluster,
        attribute,
        mat_native::scalar_to_tlv(value),
        timed,
    )
    .await?;
    tracing::info!(
        node_id,
        cluster,
        attribute,
        "write executed (native direct)"
    );
    crate::commands::write::emit_write_success(
        node_id,
        endpoint,
        cluster_in,
        attribute_in,
        value_in,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn op_invoke_generic(
    engine: &Engine,
    node_id: u64,
    endpoint: u16,
    cluster_in: &str,
    command_in: &str,
    cluster: u32,
    command: u32,
    fields_tlv: &Option<Vec<u8>>,
    timed: bool,
) -> Result<(), MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    conn.invoke(endpoint, cluster, command, fields_tlv.clone(), timed)
        .await?;
    tracing::info!(node_id, cluster, command, "invoke executed (native direct)");
    crate::commands::invoke::emit_invoke_success(node_id, endpoint, cluster_in, command_in);
    Ok(())
}

async fn op_describe(engine: &Engine, node_id: u64) -> Result<(), MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    let endpoints = mat_native::ops::describe(&mut *conn).await?;
    tracing::info!(node_id, "describe executed (native direct)");
    crate::commands::describe::emit_describe_success(node_id, &endpoints);
    Ok(())
}

async fn op_diag_thread(engine: &Engine, node_id: u64, endpoint: u16) -> Result<(), MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    let snap = mat_native::ops::diag_thread(&mut *conn, endpoint).await?;
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
    tracing::info!(node_id, endpoint, "diag thread executed (native direct)");
    crate::commands::diag::emit_diag_thread_success(node_id, endpoint, snap.fields, unavailable);
    Ok(())
}

async fn op_open_window(
    engine: &Engine,
    node_id: u64,
    timeout: u32,
    iteration: u32,
    discriminator: u16,
) -> Result<(), MatError> {
    let mut conn = engine.establisher.establish(node_id).await?;
    // timeout は chip-tool 経路と同じ u32 CLI 値、window API は u16
    // （spec 上 window timeout は 16-bit）。飽和させて渡す。
    let timeout_u16 = u16::try_from(timeout).unwrap_or(u16::MAX);
    let (manual_code, qr_payload) = conn
        .open_window(timeout_u16, discriminator, iteration)
        .await?;
    tracing::info!(
        node_id,
        discriminator,
        "open-window executed (native direct)"
    );
    crate::commands::open_window::emit_open_window_success(
        node_id,
        &manual_code,
        &qr_payload,
        timeout,
    );
    Ok(())
}

/// 確立 → 1 op → 破棄。値を返す op（read）は emit まで行う。ディスパッチのみ —
/// 各 op の実体は op_*（1 op = 1 関数、matd の native.rs と同じ粒度）。
///
/// M8c-3（chip-tool 撤去）: 従来 `Fallback` を返していた分岐はハードエラー化。
/// `engine.group` / `engine.group_settings` 未設定は本番 `Engine::build` では
/// 常に `Some`（`with_parts` テスト注入時のみ `None`）なので Other、
/// `GroupOutcome::Unavailable`（未 provision・KVS 不備）は store_parse で返す。
async fn run_op(engine: &Engine, op: &NativeOp) -> Result<(), MatError> {
    match op {
        NativeOp::On { node_id, endpoint } => op_on(engine, *node_id, *endpoint).await,
        NativeOp::Off { node_id, endpoint } => op_off(engine, *node_id, *endpoint).await,
        NativeOp::ReadOnOff { node_id, endpoint } => {
            op_read_onoff(engine, *node_id, *endpoint).await
        }
        NativeOp::Color {
            node_id,
            endpoint,
            color,
            transition,
        } => op_color(engine, *node_id, *endpoint, color, *transition).await,
        NativeOp::ColorTemp {
            node_id,
            endpoint,
            kelvin,
            mireds,
            transition,
        } => op_color_temp(engine, *node_id, *endpoint, *kelvin, *mireds, *transition).await,
        NativeOp::Level {
            node_id,
            endpoint,
            percent,
            level,
            transition,
        } => op_level(engine, *node_id, *endpoint, *percent, *level, *transition).await,
        NativeOp::GroupOnOff {
            group_id,
            command_id,
            command,
            endpoint,
        } => op_group_onoff(engine, *group_id, *command_id, command, *endpoint).await,
        NativeOp::GroupColor {
            group_id,
            color,
            transition,
            endpoint,
        } => op_group_color(engine, *group_id, color, *transition, *endpoint).await,
        NativeOp::GroupColorTemp {
            group_id,
            kelvin,
            mireds,
            transition,
            endpoint,
        } => op_group_color_temp(engine, *group_id, *kelvin, *mireds, *transition, *endpoint).await,
        NativeOp::GroupLevel {
            group_id,
            percent,
            level,
            transition,
            endpoint,
        } => op_group_level(engine, *group_id, *percent, *level, *transition, *endpoint).await,
        NativeOp::GroupInvokeGeneric {
            group_id,
            cluster_in,
            command_in,
            cluster,
            command,
            fields_tlv,
            endpoint,
        } => {
            op_group_invoke_generic(
                engine, *group_id, cluster_in, command_in, *cluster, *command, fields_tlv,
                *endpoint,
            )
            .await
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
            op_group_provision(
                engine,
                *group_id,
                node_ids,
                *keyset_id,
                name,
                *endpoint,
                epoch_key.as_deref(),
                *rebind,
            )
            .await
        }
        NativeOp::GroupGrant { group_id, node_ids } => {
            op_group_grant(engine, *group_id, node_ids).await
        }
        NativeOp::ReadAttr {
            node_id,
            endpoint,
            cluster_in,
            attribute_in,
            cluster,
            attribute,
        } => {
            op_read_attr(
                engine,
                *node_id,
                *endpoint,
                cluster_in,
                attribute_in,
                *cluster,
                *attribute,
            )
            .await
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
            op_write_attr(
                engine,
                *node_id,
                *endpoint,
                cluster_in,
                attribute_in,
                *cluster,
                *attribute,
                value_in,
                value,
                *timed,
            )
            .await
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
            op_invoke_generic(
                engine, *node_id, *endpoint, cluster_in, command_in, *cluster, *command,
                fields_tlv, *timed,
            )
            .await
        }
        NativeOp::Describe { node_id } => op_describe(engine, *node_id).await,
        NativeOp::DiagThread { node_id, endpoint } => {
            op_diag_thread(engine, *node_id, *endpoint).await
        }
        NativeOp::OpenWindow {
            node_id,
            timeout,
            iteration,
            discriminator,
        } => op_open_window(engine, *node_id, *timeout, *iteration, *discriminator).await,
    }
}

/// `mat diag node` の IM 部分（operational チェック + thread シグナル）を
/// native で実行した結果（M8c-2）。CFID はログパースではなく fabric 資材
/// から直接計算するため、native 経路では cfid_unavailable の系が消える。
pub(crate) struct DiagImProbe {
    pub resolved: bool,
    pub op_kind: Option<mat_core::error::ErrorKind>,
    pub self_cfid: String,
    pub thread: Result<mat_core::diag::ThreadCheck, mat_core::error::ErrorKind>,
}

/// `diag_im_probe` の入口。M8c-3（chip-tool 撤去）: エンジン構築失敗は
/// フォールバックせずハードエラー化（`execute` の build 失敗と同じ写像 —
/// store_missing に `mat fabric init` 誘導を付す）。
pub(crate) fn diag_im_probe(
    cfg: &Config<'_>,
    store_root: &Path,
    node_id: u64,
    endpoint: u16,
) -> Result<DiagImProbe, MatError> {
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
            store: store_root.to_path_buf(),
            iface: cfg.iface.to_string(),
            fabric_index: cfg.fabric_index,
            issuer_index: cfg.issuer_index,
        };
        let engine = Engine::build(&native_cfg)
            .await
            .map_err(map_engine_build_error)?;
        Ok(diag_im_with_engine(&engine, node_id, endpoint).await)
    })
}

async fn diag_im_with_engine(engine: &Engine, node_id: u64, endpoint: u16) -> DiagImProbe {
    use mat_core::ids::{resolve_attribute, resolve_cluster};
    // cfid は build 済みエンジンでは常に Some（with_parts 注入時のみ呼び出し側が保証）。
    let cfid = engine
        .group_settings
        .as_ref()
        .map(|g| g.cfid)
        .expect("built engine always carries group_settings");
    let self_cfid = format!("{:016X}", u64::from_be_bytes(cfid));

    // descriptor / parts-list は mat-core::ids 表から解決する（プロトコル知識を
    // 重複ハードコードしない）。表に無ければここで Other へフォールバックする —
    // 現行の名前表には常に載っているため通常到達しない。
    let descriptor_parts =
        resolve_cluster("descriptor").zip(resolve_attribute(0x001D, "parts-list").map(|a| a.id));

    let (resolved, op_kind, thread) = match engine.establisher.establish(node_id).await {
        Err(e) => (false, Some(e.kind), Err(e.kind)),
        Ok(mut conn) => {
            let (resolved, op_kind) = match descriptor_parts {
                None => (false, Some(mat_core::error::ErrorKind::Other)),
                Some((cluster, attr)) => match conn.read_json(0, cluster, attr).await {
                    Ok(_) => (true, None),
                    Err(e) => (false, Some(e.kind)),
                },
            };
            // thread シグナルの field-id 知識（NEIGHBOR_TABLE_FIELDS 等）は
            // mat-native::ops に閉じている（CLAUDE.md 設計ルール1）。
            let thread = match mat_native::ops::diag_thread(&mut *conn, endpoint).await {
                Err(e) => Err(e.kind),
                Ok(snap) => mat_native::ops::thread_check_from_snapshot(&snap).map_err(|e| e.kind),
            };
            (resolved, op_kind, thread)
        }
    };
    tracing::info!(node_id, "diag node executed (native)");
    DiagImProbe {
        resolved,
        op_kind,
        self_cfid,
        thread,
    }
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
            Some(Ok(NativeOp::On {
                node_id: 5,
                endpoint: 1
            }))
        ));
        let off = Command::Off {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
        };
        assert!(matches!(
            classify(&off),
            Some(Ok(NativeOp::Off {
                node_id: 5,
                endpoint: 1
            }))
        ));
        let read = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "onoff".into(),
            attribute: "on-off".into(),
        };
        assert!(matches!(
            classify(&read),
            Some(Ok(NativeOp::ReadOnOff { .. }))
        ));
        // 汎用 read（onoff on-off 以外）で名前解決できるものは M8a Task7 で native
        // 対象に拡張された（`generic_read_is_native_when_names_resolve` 参照）。
        // 名前解決できないものは classify 非対象（`run` が parse_error 化）。
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
            Some(Ok(NativeOp::ColorTemp {
                node_id: 5,
                endpoint: 1,
                kelvin: 2700,
                mireds: 370,
                transition: 0
            }))
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
            Some(Ok(NativeOp::Color {
                node_id: 5,
                endpoint: 1,
                ..
            }))
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
            Some(Ok(NativeOp::GroupOnOff { group_id: 10, .. }))
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
            Some(Ok(NativeOp::GroupInvokeGeneric {
                group_id: 10,
                fields_tlv: Some(_),
                ..
            }))
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
            Some(Ok(NativeOp::GroupGrant { group_id: 10, node_ids })) if node_ids == vec![5, 6]
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
            Some(Ok(NativeOp::GroupProvision {
                group_id: 10,
                keyset_id: 60,
                ref name,
                endpoint: 1,
                epoch_key: None,
                rebind: false,
                ..
            })) if name == "grp10"
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
            Some(Ok(NativeOp::GroupColorTemp {
                group_id: 10,
                kelvin: 2700,
                mireds: 370,
                transition: 0,
                endpoint: 1,
            }))
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
            Some(Ok(NativeOp::GroupColor { group_id: 10, .. }))
        ));
    }

    #[tokio::test]
    async fn group_onoff_hard_errors_when_engine_group_ctx_unconfigured() {
        // engine.group == None（with_parts テスト注入）: M8c-3 で chip-tool
        // フォールバックが撤去されたためハードエラー（Other, internal）。
        use mat_native::test_support::FakeEstablisher;
        let engine = mat_native::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        let err = run_op(
            &engine,
            &NativeOp::GroupOnOff {
                group_id: 10,
                command_id: mat_controller::im::CMD_ON_OFF_TOGGLE,
                command: "toggle",
                endpoint: 1,
            },
        )
        .await
        .expect_err("group ctx unconfigured must hard-error");
        assert_eq!(err.kind, mat_core::error::ErrorKind::Other);
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
            ..Default::default()
        };
        let calls = std::sync::Arc::clone(&est.calls);
        let engine = mat_native::Engine::with_parts(Box::new(est), None);
        let err = run_op(
            &engine,
            &NativeOp::ReadOnOff {
                node_id: 5,
                endpoint: 1,
            },
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
            Some(Ok(NativeOp::ReadAttr {
                cluster: 0x0008,
                attribute: 0x0000,
                ..
            }))
        ));
        // 未知クラスタ名は classify 非対象（`run` が parse_error 化）。
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
        assert!(matches!(
            classify(&byid),
            Some(Ok(NativeOp::ReadAttr { .. }))
        ));
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
        assert!(matches!(classify(&w), Some(Ok(NativeOp::WriteAttr { .. }))));
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
            Some(Ok(NativeOp::InvokeGeneric { .. }))
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
            Some(Ok(NativeOp::Describe { node_id: 5 }))
        ));

        let diag_thread = Command::Diag {
            action: DiagCommand::Thread {
                node_id: NodeRef::Id(5),
                endpoint: EndpointRef::Id(0),
            },
        };
        assert!(matches!(
            classify(&diag_thread),
            Some(Ok(NativeOp::DiagThread {
                node_id: 5,
                endpoint: 0
            }))
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
            Some(Ok(NativeOp::OpenWindow {
                node_id: 5,
                timeout: 180,
                iteration: 1000,
                discriminator: 3840,
            }))
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
            Some(Ok(NativeOp::OpenWindow {
                discriminator: 5,
                ..
            }))
        ));
    }

    #[test]
    fn diag_mesh_is_excluded_from_native_direct_run() {
        use crate::cli::DiagCommand;
        let cmd = Command::Diag {
            action: DiagCommand::Mesh { nodes: vec![] },
        };
        let cfg = Config {
            iface: "lo",
            fabric_index: 1,
            issuer_index: 0,
        };
        // 専用コマンド層を持つ op は run() が None（store にも触れない —
        // 実在しないパスで良い）。
        assert!(run(&cmd, std::path::Path::new("/nonexistent"), &cfg).is_none());
    }

    /// `mat_native::ops::provision_node` が読む group-key-map / acl に妥当な
    /// JSON（空リスト／管理者エントリのみ）を返す scripted establisher
    /// （matd の `ScriptedEstablisher`, `crates/matd/src/server.rs` 1639 行付近
    /// と同型のフィクスチャ）。
    struct ScriptedEstablisher;
    #[async_trait::async_trait]
    impl mat_native::Establisher for ScriptedEstablisher {
        async fn establish(
            &self,
            _node_id: u64,
        ) -> Result<Box<dyn mat_native::NodeConn>, MatError> {
            use mat_native::test_support::FakeConn;
            Ok(Box::new(
                FakeConn::scripted()
                    .with_read(0, 0x003F, 0x0000, serde_json::json!([]))
                    .with_read(
                        0,
                        0x001F,
                        0x0000,
                        serde_json::json!([{"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]),
                    ),
            ))
        }
    }

    fn scripted_establisher() -> ScriptedEstablisher {
        ScriptedEstablisher
    }

    #[tokio::test]
    async fn run_op_group_provision_writes_controller_state_to_kvs_natively() {
        // engine: fake establisher + group_settings ctx（一時 ini）。
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let mut engine = Engine::with_parts(Box::new(scripted_establisher()), None);
        engine.group_settings = Some(mat_native::group_settings::GroupSettingsCtx {
            main_ini: ini.clone(),
            fabric_index: 2,
            cfid: [7u8; 8],
        });
        let op = NativeOp::GroupProvision {
            group_id: 99,
            node_ids: vec![5],
            keyset_id: 99,
            name: "e2e".into(),
            endpoint: 1,
            epoch_key: Some("42".repeat(16)),
            rebind: false,
        };
        run_op(&engine, &op).await.unwrap();
        // コントローラ側 state が chip-tool spawn なしで KVS に入っている。
        assert!(mat_controller::kvs::read_group_credentials(&ini, 2, 99).is_ok());
    }

    #[tokio::test]
    async fn run_op_group_provision_hard_errors_when_ctx_missing() {
        let engine = Engine::with_parts(Box::new(scripted_establisher()), None); // ctx なし
        let op = NativeOp::GroupProvision {
            group_id: 99,
            node_ids: vec![5],
            keyset_id: 99,
            name: "e2e".into(),
            endpoint: 1,
            epoch_key: None,
            rebind: false,
        };
        // M8c-3: group_settings ctx 未構成はフォールバックせずハードエラー（Other）。
        let err = run_op(&engine, &op)
            .await
            .expect_err("missing group_settings ctx must hard-error");
        assert_eq!(err.kind, mat_core::error::ErrorKind::Other);
    }

    /// `diag_im_with_engine` の scripted establisher: parts-list（descriptor,
    /// ep0）は既定応答（`FakeConn` の未登録フォールバック `json!(1)`）で
    /// resolved=true になる。thread シグナルは `mat_native::ops::diag_thread`
    /// 経由（`read_cluster` の wildcard read 1発）に変わったため、
    /// neighbor-table（0x0035/0x0007, ep1）は `with_cluster` で構造体配列を
    /// 明示応答する（field id "5" = Lqi、ops.rs の `NEIGHBOR_TABLE_FIELDS` で
    /// 改名される）。
    struct ScriptedImEstablisher;
    #[async_trait::async_trait]
    impl mat_native::Establisher for ScriptedImEstablisher {
        async fn establish(
            &self,
            _node_id: u64,
        ) -> Result<Box<dyn mat_native::NodeConn>, MatError> {
            use mat_native::test_support::FakeConn;
            Ok(Box::new(FakeConn::scripted().with_cluster(
                1,
                0x0035,
                vec![(0x0007, serde_json::json!([{"5": 200}, {"5": 100}]))],
            )))
        }
    }

    struct FailingImEstablisher;
    #[async_trait::async_trait]
    impl mat_native::Establisher for FailingImEstablisher {
        async fn establish(
            &self,
            _node_id: u64,
        ) -> Result<Box<dyn mat_native::NodeConn>, MatError> {
            Err(MatError::new(
                mat_core::error::ErrorKind::Unreachable,
                "fake unreachable",
            ))
        }
    }

    fn failing_establisher() -> FailingImEstablisher {
        FailingImEstablisher
    }

    #[tokio::test]
    async fn diag_im_with_engine_reads_operational_and_thread_natively() {
        let mut engine = Engine::with_parts(Box::new(ScriptedImEstablisher), None);
        engine.group_settings = Some(mat_native::group_settings::GroupSettingsCtx {
            main_ini: std::path::PathBuf::from("/nonexistent"),
            fabric_index: 2,
            cfid: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
        });
        let p = diag_im_with_engine(&engine, 5, 1).await;
        assert!(p.resolved);
        assert_eq!(p.op_kind, None);
        assert_eq!(p.self_cfid, "1122334455667788");
        let t = p.thread.expect("thread check");
        assert!(t.neighbor_count >= 1);
        assert_eq!(t.best_lqi, Some(200));
    }

    #[tokio::test]
    async fn diag_im_with_engine_reports_establish_failure_as_unresolved() {
        let mut engine = Engine::with_parts(Box::new(failing_establisher()), None);
        engine.group_settings = Some(mat_native::group_settings::GroupSettingsCtx {
            main_ini: std::path::PathBuf::from("/nonexistent"),
            fabric_index: 2,
            cfid: [1u8; 8],
        });
        let p = diag_im_with_engine(&engine, 5, 1).await;
        assert!(!p.resolved);
        assert_eq!(p.op_kind, Some(mat_core::error::ErrorKind::Unreachable));
        assert!(p.thread.is_err());
    }

    #[tokio::test]
    async fn open_window_runs_via_fake_and_emits_codes() {
        // fake 経由で run_op(OpenWindow) が establish → open_window → emit まで
        // 完走することを確認する。`emit_open_window_success` は `println!`
        // ベース（`mat_core::output::emit`）で戻り値を返さないため、この
        // ユニットテストでは「最後まで panic/Err せず走り切ること」までを
        // 保証する（stdout の実際の JSON 内容は、これまで chip-tool 統合
        // テストの `open_window_returns_codes` が担っていたが、M8c-3 Task3
        // で chip-tool のダミー実行体ごと撤去した。プロセス stdout を安全に
        // キャプチャする手段が無い（`#[test]` は並行実行され、生 fd
        // リダイレクトは他テストの出力を巻き込む）ため、ここでは完走のみを
        // 保証する — 詳細は Task3 の報告を参照）。
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
        )
        .await
        .unwrap();
    }

    // ── M8c-3 Task3: read/write/invoke/describe の run_op 完走確認 ──────────
    //
    // これまで chip-tool 統合テスト（`read_parses_value` / `write_reports_success`
    // / `invoke_reports_success` / `describe_lists_endpoints_and_clusters`）が
    // 担っていた「成功系が最後まで走ること」の代替。stdout の JSON 内容自体は
    // `emit_read_success` 等が `println!` 直書き（戻り値なし）で、プロセス
    // stdout を安全にキャプチャする手段がこのテストバイナリに無いため検証
    // できない（`open_window_runs_via_fake_and_emits_codes` のコメント参照）。
    // ここでは `classify()` が実際に出す `NativeOp`（＝本番と同じ cluster/
    // attribute の数値解決）をそのまま `run_op` に通し、`FakeConn` 応答で
    // 最後まで `Ok(())` になることを保証する。

    #[tokio::test]
    async fn run_op_read_attr_completes_via_native() {
        use mat_core::alias::{EndpointRef, NodeRef};
        use mat_native::test_support::FakeEstablisher;
        let read = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            attribute: "current-level".into(),
        };
        let op = classify(&read)
            .expect("levelcontrol/current-level resolves natively")
            .unwrap();
        assert!(matches!(op, NativeOp::ReadAttr { .. }));
        let engine = mat_native::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        run_op(&engine, &op).await.unwrap();
    }

    #[tokio::test]
    async fn run_op_write_attr_completes_via_native() {
        use mat_core::alias::{EndpointRef, NodeRef};
        use mat_native::test_support::FakeEstablisher;
        let write = Command::Write {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            attribute: "on-level".into(),
            value: "128".into(),
        };
        let op = classify(&write)
            .expect("levelcontrol/on-level resolves natively")
            .unwrap();
        assert!(matches!(op, NativeOp::WriteAttr { .. }));
        let engine = mat_native::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        run_op(&engine, &op).await.unwrap();
    }

    #[tokio::test]
    async fn run_op_invoke_generic_completes_via_native() {
        use mat_core::alias::{EndpointRef, NodeRef};
        use mat_native::test_support::FakeEstablisher;
        let inv = Command::Invoke {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            command: "move-to-level".into(),
            args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
        };
        let op = classify(&inv)
            .expect("levelcontrol/move-to-level resolves natively")
            .unwrap();
        assert!(matches!(op, NativeOp::InvokeGeneric { .. }));
        let engine = mat_native::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        run_op(&engine, &op).await.unwrap();
    }

    #[tokio::test]
    async fn run_op_describe_completes_via_native() {
        use mat_core::alias::NodeRef;
        use mat_native::test_support::FakeEstablisher;
        let describe = Command::Describe {
            node_id: NodeRef::Id(5),
        };
        let op = classify(&describe).expect("describe is native").unwrap();
        assert!(matches!(op, NativeOp::Describe { .. }));
        let engine = mat_native::Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        run_op(&engine, &op).await.unwrap();
    }
}
