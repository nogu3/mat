//! `mat discover` — commissionable / commissioned ノードを探索する。
//!
//! commissionable は native mDNS browse（`mat-controller::dnssd`）の結果、
//! commissioned は `mat` の台帳（KVS）から読む。両者を1つの `devices` 配列にまとめる。
//!
//! `--probe` 指定時は commissioned ノードそれぞれへ targeted resolve を並行実行して
//! ライブ到達性を判定し、`reachable`（true/false/null）と、不達時の `stale` を付与する
//! （M8b: 列挙(browse)ベースから切り替え — 実機の advertising proxy が一部ノードの
//! PTR 列挙に応答しないため、CFID+NodeId 既知の対象を直接 resolve する）。
//! 既定（`--probe` 無し）は台帳をそのまま出す高速経路で、出力は従来と完全に同一。
//!
//! commissionable 探索は認証情報不要のため、store 無しでも動く（無ければ空ストアを
//! bootstrap し、commissioned は空配列になる）。
//!
//! commissionable 探索は native browse、probe は native targeted resolve（M8b）。

use std::path::Path;

use serde_json::{json, Map, Value};

use mat_core::diag::MatterInstance;
use mat_core::error::{ErrorKind, MatError};
use mat_core::output;
use mat_core::parse::DiscoveredDevice;
use mat_core::reachability::resolve;
use mat_core::store::Store;

pub fn run(
    store_path: &Path,
    probe: bool,
    native: Option<&crate::native_direct::Config<'_>>,
) -> Result<(), MatError> {
    // discover の commissionable 探索は認証情報不要。store 無しでも動くべきなので
    // open ではなく open_or_init（無ければ空ストアを bootstrap）。commissioned は
    // 台帳から読むが、空ストアなら空配列になるだけ。
    let store = Store::open_or_init(store_path)?;

    // commissionable 探索は native browse 一本化（M8c-3 で chip-tool 経路撤去、
    // Task 11 で avahi-browse フォールバックも撤去 — mDNS は dnssd 一本）。
    // 結果 0 件は正常。IO 失敗はハードエラー（黙って落とさない — spec 設計3）。
    let cfg = native.ok_or_else(|| {
        MatError::new(
            ErrorKind::Other,
            "discover: native backend not configured (internal)",
        )
    })?;
    let iface = cfg.iface;
    let commissionable = native_commissionables(iface).map_err(|e| {
        MatError::new(
            ErrorKind::Unreachable,
            format!("native commissionable browse failed on {iface}: {e}"),
        )
    })?;

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

    // --probe: 台帳ノードごとに targeted resolve を並行実行しライブ到達性を
    // 判定する（M8b）。None = 未実施 or 実施不能（後者は reachable:null）。
    let instances: Option<Vec<MatterInstance>> = if probe {
        let ids: Vec<u64> = store.nodes().map(|n| n.node_id).collect();
        match crate::probe::mdns(crate::probe::NativeProbe {
            iface: cfg.iface,
            fabric_index: cfg.fabric_index,
            issuer_index: cfg.issuer_index,
            store_root: store.root(),
            node_ids: &ids,
        }) {
            Ok(list) => Some(list),
            Err(e) => {
                tracing::warn!(
                    detail = %e.detail,
                    kind = ?e.kind,
                    "discover --probe: mDNS probe failed; reachability unknown"
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
