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
use mat_native::{Engine, NativeConfig};

use crate::cli::Command;

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
    // Group 3 形は Task 5 で追加。
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
        _ => None,
    }
}

enum Executed {
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
    let op = classify(command)?;
    match execute(&op, store_path, cfg) {
        Ok(Executed::Done) => Some(Ok(())),
        Ok(Executed::Fallback) => None,
        Err(e) => Some(Err(e)),
    }
}

fn execute(op: &NativeOp, store_path: &Path, cfg: &Config) -> Result<Executed, MatError> {
    // store / commission チェックは chip-tool 経路と同一の順序・エラー(exit 10/11)。
    let store = Store::open(store_path)?;
    let node_id = match op {
        NativeOp::On { node_id, .. }
        | NativeOp::Off { node_id, .. }
        | NativeOp::ReadOnOff { node_id, .. }
        | NativeOp::Color { node_id, .. }
        | NativeOp::ColorTemp { node_id, .. } => Some(*node_id),
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
        run_op(&engine, op).await?;
        Ok(Executed::Done)
    })
}

/// 確立 → 1 op → 破棄。値を返す op（read）は emit まで行う。
async fn run_op(engine: &Engine, op: &NativeOp) -> Result<(), MatError> {
    use mat_controller::im;
    match op {
        NativeOp::On { node_id, endpoint } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(*endpoint, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None)
                .await?;
            crate::commands::invoke::emit_invoke_success(*node_id, *endpoint, "onoff", "on");
        }
        NativeOp::Off { node_id, endpoint } => {
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(*endpoint, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_OFF, None)
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
    }
    Ok(())
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
        // 汎用 read（onoff on-off 以外）は非対象 —— matd の is_native_hotpath とパリティ。
        let other = Command::Read {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            cluster: "levelcontrol".into(),
            attribute: "current-level".into(),
        };
        assert!(classify(&other).is_none());
        // discover / describe / write / diag 等は非対象。
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
}
