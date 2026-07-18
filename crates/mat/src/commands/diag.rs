//! `mat diag thread` — Thread Network Diagnostics (cluster 53) のスナップショット。
//!
//! メッシュの健全性分析用に、1 ノードの Thread 診断属性をワンショットで集約する。
//! 返す JSON で「近くに何台いて電波がどれだけ強いか（neighbor-table の LQI/RSSI）」
//! 「中継しているか（routing-role）」「メッシュが分断していないか（partition-id）」
//! を読み取れる。
//!
//! バックエンド実行は native 直経路（`native_direct`）が担う（M8c-3 で chip-tool
//! 経路は撤去）。`diag thread` の emit（`emit_diag_thread_success`）は native 経路の
//! 単一ソース。`diag node` は native IM probe（operational + thread）に加え、
//! `--deep` の補助プローブ（ping6 / mDNS）をこのモジュールで実施する（avahi 経路は
//! Task 11 で扱う）。

use std::ffi::OsString;
use std::path::Path;
use std::process::Command as StdCommand;

use serde_json::{json, Map, Value};

use mat_core::diag::{derive_verdict, parse_ping6, Checks, IpCheck, MdnsCheck, OperationalCheck};
use mat_core::error::{ErrorKind, MatError};
use mat_core::output;
use mat_core::store::Store;

/// `diag thread` の成功 JSON を stdout へ emit する。native 直経路
/// （`native_direct`）から呼ばれる単一ソース（スキーマ不変）。
/// `unavailable` は空なら省略する（native 経路では通常空 — `ops::diag_thread`
/// のコメント参照）。
pub(crate) fn emit_diag_thread_success(
    node_id: u64,
    endpoint: u16,
    thread: Map<String, Value>,
    unavailable: Vec<Value>,
) {
    let mut body = Map::new();
    body.insert("node_id".to_string(), json!(node_id));
    body.insert("endpoint".to_string(), json!(endpoint));
    body.insert("thread".to_string(), Value::Object(thread));
    if !unavailable.is_empty() {
        body.insert("unavailable".to_string(), Value::Array(unavailable));
    }
    output::emit(Value::Object(body));
}

/// `mat diag node` — 到達不能の根本原因を層別チェックで分類する。
pub fn node(
    store_path: &Path,
    node_id: u64,
    endpoint: u16,
    deep: bool,
    native: Option<&crate::native_direct::Config<'_>>,
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    let rec = store.require_node(node_id)?;
    let address = rec.address.clone();

    let mut checks = Checks::default();
    let mut unavailable: Vec<Value> = Vec::new();

    // IM 部分（operational + thread）は native（M8c-2; M8c-3 で唯一の経路）。
    // エンジン構築失敗はハードエラー（`diag_im_probe` が写像済み）。
    let cfg = native.ok_or_else(|| {
        MatError::new(
            ErrorKind::Other,
            "diag node: native backend not configured (internal)",
        )
    })?;
    let p = crate::native_direct::diag_im_probe(cfg, store.root(), node_id, endpoint)?;
    checks.operational = Some(OperationalCheck {
        resolved: p.resolved,
        kind: p.op_kind,
    });
    match p.thread {
        Ok(tc) => checks.thread = Some(tc),
        Err(kind) => unavailable.push(json!({ "check": "thread", "kind": kind })),
    }
    let self_cfid: Option<String> = Some(p.self_cfid);

    if deep {
        deep_probes(
            &mut checks,
            &mut unavailable,
            node_id,
            address,
            self_cfid,
            native,
            store.root(),
        );
    } else {
        unavailable.push(json!({ "check": "ip", "kind": "skipped_no_deep" }));
        unavailable.push(json!({ "check": "mdns", "kind": "skipped_no_deep" }));
    }

    let v = derive_verdict(&checks);

    let mut body = Map::new();
    body.insert("node_id".to_string(), json!(node_id));
    body.insert("endpoint".to_string(), json!(endpoint));
    body.insert(
        "verdict".to_string(),
        serde_json::to_value(v.verdict).unwrap_or(Value::Null),
    );
    body.insert("summary".to_string(), json!(v.summary));
    body.insert(
        "checks".to_string(),
        serde_json::to_value(&checks)
            .map_err(|e| MatError::parse_error(format!("serialize checks: {e}")))?,
    );
    if !unavailable.is_empty() {
        body.insert("unavailable".to_string(), Value::Array(unavailable));
    }
    body.insert("recommendation".to_string(), json!(v.recommendation));
    output::emit(Value::Object(body));
    Ok(())
}

/// `--deep` の補助プローブ。ping6（IP生存）と mDNS 広告確認（native の targeted
/// resolve、失敗時は avahi-browse — avahi 経路は Task 11 で扱う）を実施。
fn deep_probes(
    checks: &mut Checks,
    unavailable: &mut Vec<Value>,
    node_id: u64,
    address: Option<String>,
    self_cfid: Option<String>,
    native: Option<&crate::native_direct::Config<'_>>,
    store_root: &Path,
) {
    // ip: ping6
    match address.as_deref() {
        Some(addr) => match probe_ping6(addr) {
            Ok(stats) => {
                checks.ip = Some(IpCheck {
                    ok: stats.loss_pct < 100,
                    loss_pct: stats.loss_pct,
                    rtt_ms: stats.rtt_ms,
                    method: "ping6",
                })
            }
            Err(e) => {
                let kind_val = probe_error_kind(&e);
                unavailable.push(json!({ "check": "ip", "kind": kind_val, "detail": e.detail }))
            }
        },
        None => unavailable.push(json!({ "check": "ip", "kind": "no_address_in_store" })),
    }

    // mdns: native targeted resolve（iface 指定時）または avahi-browse
    match crate::probe::mdns(native.map(|c| crate::probe::NativeProbe {
        iface: c.iface,
        fabric_index: c.fabric_index,
        issuer_index: c.issuer_index,
        store_root,
        node_ids: std::slice::from_ref(&node_id),
    })) {
        Ok(instances) => {
            // アドレスベースで照合（ストアの address が Some の場合）。
            // None ならベストエフォートで node_id を使う（この場合 self_fabric は None のまま）。
            let addr = address.as_deref();
            let advertised_any_fabric = match addr {
                Some(a) => instances.iter().any(|i| i.addresses.iter().any(|x| x == a)),
                None => instances.iter().any(|i| i.node_id == node_id),
            };
            let advertised_self_fabric =
                match (self_cfid.as_ref(), addr) {
                    (Some(cfid), Some(a)) => Some(instances.iter().any(|i| {
                        &i.compressed_fabric == cfid && i.addresses.iter().any(|x| x == a)
                    })),
                    _ => None,
                };
            if self_cfid.is_none() {
                // native 経路では self_cfid は常に取れる（fabric 資材から算出）ため
                // 実質到達しない。防御的に残す。
                unavailable.push(json!({
                    "check": "mdns_self_fabric",
                    "kind": "cfid_unavailable",
                    "detail": "could not obtain self compressed-fabric-id"
                }));
            }
            checks.mdns = Some(MdnsCheck {
                advertised_self_fabric,
                advertised_any_fabric,
            });
        }
        Err(e) => {
            let kind_val = probe_error_kind(&e);
            unavailable.push(json!({ "check": "mdns", "kind": kind_val, "detail": e.detail }))
        }
    }
}

/// プローブエラーの kind を JSON 値に変換。
/// バイナリが見つからない場合は `"tool_missing"`、その他はエラーの ErrorKind の snake_case。
fn probe_error_kind(e: &MatError) -> Value {
    if e.kind == ErrorKind::ChildNotFound {
        Value::String("tool_missing".to_string())
    } else {
        serde_json::to_value(e.kind).unwrap_or(Value::Null)
    }
}

/// `ping6 -c 3 -W 2 <addr>` を実行して統計をパース。バイナリは `MAT_PING6_BIN` で上書き可。
fn probe_ping6(addr: &str) -> Result<mat_core::diag::Ping6Stats, MatError> {
    let bin = std::env::var_os("MAT_PING6_BIN").unwrap_or_else(|| OsString::from("ping6"));
    let out = StdCommand::new(&bin)
        .args(["-c", "3", "-W", "2", addr])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MatError::child_not_found(format!("ping6 not found ({bin:?})"))
            } else {
                MatError::new(
                    ErrorKind::Other,
                    format!("ping6 spawn failed ({bin:?}): {e}"),
                )
            }
        })?;
    let text = String::from_utf8_lossy(&out.stdout);
    let stderr_text = String::from_utf8_lossy(&out.stderr);
    tracing::debug!(%text, "ping6 stdout");
    tracing::debug!(%stderr_text, "ping6 stderr");
    parse_ping6(&text).ok_or_else(|| MatError::parse_error("ping6 output unparseable"))
}
