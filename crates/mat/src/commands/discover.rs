//! `mat discover` — commissionable / commissioned ノードを探索する。
//!
//! commissionable は `chip-tool discover commissionables` の mDNS 探索結果、
//! commissioned は `mat` の台帳（KVS）から読む。両者を1つの `devices` 配列にまとめる。
//!
//! `--probe` 指定時は commissioned ノードについても mDNS を 1 回ブラウズして
//! ライブ到達性を判定し、`reachable`（true/false/null）と、不達時の `stale` を付与する。
//! 既定（`--probe` 無し）は台帳をそのまま出す高速経路で、出力は従来と完全に同一。
//!
//! commissionable 探索は認証情報不要のため、store 無しでも動く（無ければ空ストアを
//! bootstrap し、commissioned は空配列になる）。
//!
//! iface（MAT_IFACE）設定時は commissionable / probe とも native browse（M8b）。

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::runner::ChipTool;
use mat_core::diag::MatterInstance;
use mat_core::error::MatError;
use mat_core::output;
use mat_core::parse::parse_commissionables;
use mat_core::parse::DiscoveredDevice;
use mat_core::reachability::resolve;
use mat_core::store::Store;

pub fn run(store_path: &Path, probe: bool, iface: Option<&str>) -> Result<(), MatError> {
    // discover の commissionable 探索は認証情報不要。store 無しでも動くべきなので
    // open ではなく open_or_init（無ければ空ストアを bootstrap）。commissioned は
    // 台帳から読むが、空ストアなら空配列になるだけ。
    let store = Store::open_or_init(store_path)?;

    // commissionable 探索: iface 指定時は native browse（M8b）、IO 失敗は
    // warn + chip-tool フォールバック（read-only なので二重実行の害なし）。
    // 結果 0 件は正常であり chip-tool に fall back しない（平常時に毎回
    // 二重スキャンしないため）。
    let native = match iface {
        Some(i) => match native_commissionables(i) {
            Ok(list) => Some(list),
            Err(e) => {
                tracing::warn!(
                    iface = i,
                    error = %e,
                    "native commissionable browse failed; falling back to chip-tool"
                );
                None
            }
        },
        None => None,
    };
    let commissionable = match native {
        Some(list) => list,
        None => {
            let chip = ChipTool::new(store.root());
            // chip-tool は探索を時間で打ち切るため非 0 終了もあり得る。exit code で
            // 失敗扱いにせず、得られた行をパースする（child_not_found = exit 12
            // だけは run() がエラーで返す）。
            let out = chip.run(["discover", "commissionables"])?;
            parse_commissionables(&out.stdout)
        }
    };

    let mut devices = Vec::new();
    for d in &commissionable {
        let mut v = serde_json::to_value(d).map_err(|e| {
            MatError::parse_error(format!("cannot serialize discovered device: {e}"))
        })?;
        if let Some(obj) = v.as_object_mut() {
            obj.insert("state".into(), json!("commissionable"));
        }
        devices.push(v);
    }

    // --probe: commissioned ノードのライブ到達性を判定するため mDNS を 1 回だけ
    // ブラウズする。None = 未実施 or 実施不能（後者は reachable:null）。
    let instances: Option<Vec<MatterInstance>> = if probe {
        match crate::probe::mdns(iface) {
            Ok(list) => Some(list),
            Err(e) => {
                tracing::warn!(
                    detail = %e.detail,
                    kind = ?e.kind,
                    "discover --probe: mDNS browse failed; reachability unknown"
                );
                None
            }
        }
    } else {
        None
    };

    for n in store.nodes() {
        let mut obj = Map::new();
        obj.insert("state".into(), json!("commissioned"));
        obj.insert("node_id".into(), json!(n.node_id));
        obj.insert("commissioned_at".into(), json!(n.commissioned_at));
        match (probe, instances.as_deref()) {
            // 既定: 台帳そのまま（従来出力と同一）。
            (false, _) => {
                obj.insert("address".into(), json!(n.address));
            }
            // --probe だがプローブ実施不能 → 到達性不明。
            (true, None) => {
                obj.insert("address".into(), json!(n.address));
                obj.insert("reachable".into(), Value::Null);
            }
            // --probe 成功 → node_id 照合で到達性判定。
            (true, Some(list)) => {
                let r = resolve(n.node_id, n.address.as_deref(), list);
                obj.insert("reachable".into(), json!(r.reachable));
                if r.reachable {
                    // ライブ解決アドレスを優先、無ければ台帳値（announce のみ等）。
                    let addr = r.live_address.or_else(|| n.address.clone());
                    obj.insert("address".into(), json!(addr));
                } else {
                    // 据え置きの台帳値に stale 印を付ける。
                    obj.insert("address".into(), json!(n.address));
                    obj.insert("stale".into(), json!(true));
                }
            }
        }
        devices.push(Value::Object(obj));
    }

    output::emit(json!({ "devices": devices }));
    Ok(())
}

/// native commissionable browse（M8b）→ 既存 `DiscoveredDevice` へ写す
/// （既存 Serialize で出力スキーマ完全一致）。
fn native_commissionables(
    iface: &str,
) -> Result<Vec<DiscoveredDevice>, Box<dyn std::error::Error>> {
    let scope_id = mat_controller::dnssd::iface_index(iface)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let list = rt.block_on(mat_controller::dnssd::browse_commissionable(
        scope_id,
        mat_controller::dnssd::BROWSE_WINDOW,
    ))?;
    tracing::info!(devices = list.len(), "discover executed (native browse)");
    Ok(list.into_iter().map(to_discovered).collect())
}

fn to_discovered(c: mat_controller::dnssd::CommissionableInstance) -> DiscoveredDevice {
    DiscoveredDevice {
        hostname: c.hostname,
        addresses: c.addresses.iter().map(|a| a.to_string()).collect(),
        port: c.port,
        discriminator: c.discriminator,
        vendor_id: c.vendor_id,
        product_id: c.product_id,
    }
}
