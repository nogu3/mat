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
//! `--deep` の補助プローブ（ping6 / native mDNS targeted resolve）をこのモジュール
//! で実施する（Task 11 で avahi-browse フォールバックを撤去 — mDNS は dnssd 一本）。

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
            cfg,
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
/// resolve、mDNS は dnssd 一本 — Task 11 で avahi-browse フォールバックを撤去）
/// を実施。
fn deep_probes(
    checks: &mut Checks,
    unavailable: &mut Vec<Value>,
    node_id: u64,
    address: Option<String>,
    self_cfid: Option<String>,
    cfg: &crate::native_direct::Config<'_>,
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

    // mdns: native targeted resolve（dnssd 一本）
    match crate::probe::mdns(crate::probe::NativeProbe {
        iface: cfg.iface,
        fabric_index: cfg.fabric_index,
        issuer_index: cfg.issuer_index,
        store_root,
        node_ids: std::slice::from_ref(&node_id),
    }) {
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

/// `mat diag mesh` — メッシュ全体のトポロジーを 1 JSON で返す。
/// `node_ids` 空 = store の全 commission 済みノード。probe の部分失敗は
/// JSON 内（`probed:false` + `probe_error`）に畳み、全滅時のみ最頻 kind を
/// トップレベルエラーへ写像する。
pub fn mesh(
    store_path: &Path,
    node_ids: &[u64],
    native: Option<&crate::native_direct::Config<'_>>,
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    let targets: Vec<u64> = if node_ids.is_empty() {
        store.nodes().map(|r| r.node_id).collect()
    } else {
        for &id in node_ids {
            store.require_node(id)?;
        }
        // id はグラフの安定キーなので重複指定（alias 経由の二重指定も含む）は
        // 1 回の probe / グラフノードに畳む。
        dedup_preserving_order(node_ids)
    };
    let book = mat_core::alias::AliasBook::load(store.root())?;

    // 対象 0 = 空グラフで正常終了（バックエンド未接触）。
    let items = if targets.is_empty() {
        Vec::new()
    } else {
        let cfg = native.ok_or_else(|| {
            MatError::new(
                ErrorKind::Other,
                "diag mesh: native backend not configured (internal)",
            )
        })?;
        crate::native_direct::diag_mesh_probe(cfg, store.root(), &targets)?
    };

    if !items.is_empty() && items.iter().all(|i| i.result.is_err()) {
        return Err(dominant_error(&items));
    }

    let inputs: Vec<mat_core::mesh::NodeInput> = items
        .into_iter()
        .map(|i| mat_core::mesh::NodeInput {
            node_id: i.node_id,
            alias: book.node_alias_of(i.node_id).map(str::to_string),
            probe: i.result.map_err(|e| mat_core::mesh::ProbeFailure {
                kind: e.kind,
                detail: e.detail,
            }),
        })
        .collect();
    let graph = mat_core::mesh::build_graph(&inputs, &book.thread_labels());
    let body = serde_json::to_value(&graph)
        .map_err(|e| MatError::parse_error(format!("serialize mesh graph: {e}")))?;
    output::emit(body);
    Ok(())
}

/// 順序を保ったまま重複を除去する（`--nodes` の重複指定を 1 回の probe に
/// 畳むため）。件数が小さい前提で `contains` の線形探索を使う。
fn dedup_preserving_order(ids: &[u64]) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::with_capacity(ids.len());
    for &id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// 全ノード probe 失敗時のトップレベルエラー: 最頻の失敗 kind（同数タイは
/// 先勝ち）+ per-node detail の列挙。
fn dominant_error(items: &[crate::native_direct::MeshProbeItem]) -> MatError {
    let mut counts: Vec<(ErrorKind, usize)> = Vec::new();
    for it in items {
        if let Err(e) = &it.result {
            match counts.iter_mut().find(|(k, _)| *k == e.kind) {
                Some((_, c)) => *c += 1,
                None => counts.push((e.kind, 1)),
            }
        }
    }
    // 先勝ちタイ: 厳密により大きい時だけ更新。
    let mut best = ErrorKind::Other;
    let mut best_n = 0usize;
    for (k, n) in counts {
        if n > best_n {
            best = k;
            best_n = n;
        }
    }
    let detail: Vec<String> = items
        .iter()
        .filter_map(|it| {
            it.result
                .as_ref()
                .err()
                .map(|e| format!("node {}: {}", it.node_id, e.detail))
        })
        .collect();
    MatError::new(
        best,
        format!(
            "all {} mesh probes failed: {}",
            items.len(),
            detail.join("; ")
        ),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use mat_core::error::{ErrorKind, MatError};

    fn item(node_id: u64, kind: ErrorKind) -> crate::native_direct::MeshProbeItem {
        crate::native_direct::MeshProbeItem {
            node_id,
            result: Err(MatError::new(kind, format!("node {node_id} failed"))),
        }
    }

    #[test]
    fn dominant_error_picks_most_frequent_kind() {
        let items = vec![
            item(1, ErrorKind::Timeout),
            item(2, ErrorKind::Unreachable),
            item(3, ErrorKind::Unreachable),
        ];
        let e = dominant_error(&items);
        assert_eq!(e.kind, ErrorKind::Unreachable);
        assert!(e.detail.contains("node 1"));
        assert!(e.detail.contains("node 3"));
    }

    #[test]
    fn dominant_error_tie_is_first_seen() {
        let items = vec![item(1, ErrorKind::Timeout), item(2, ErrorKind::Unreachable)];
        assert_eq!(dominant_error(&items).kind, ErrorKind::Timeout);
    }

    #[test]
    fn dedup_preserving_order_removes_duplicates_keeping_first_seen_order() {
        assert_eq!(dedup_preserving_order(&[7, 7, 3, 7, 5, 3]), vec![7, 3, 5]);
        assert_eq!(dedup_preserving_order(&[]), Vec::<u64>::new());
        assert_eq!(dedup_preserving_order(&[1, 2, 3]), vec![1, 2, 3]);
    }
}
