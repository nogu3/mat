//! `mat diag thread` — Thread Network Diagnostics (cluster 53) のスナップショット。
//!
//! メッシュの健全性分析用に、1 ノードの Thread 診断属性をワンショットで集約する
//! （複数回 `chip-tool` を呼ぶ。`describe` と同じ作法）。返す JSON で「近くに何台
//! いて電波がどれだけ強いか（neighbor-table の LQI/RSSI）」「中継しているか
//! （routing-role）」「メッシュが分断していないか（partition-id）」を読み取れる。
//!
//! 複数ノードを束ねた「メッシュ地図」化は上位層の責務。ここは 1 ノードの生診断を
//! `mat` スキーマへ正規化するだけ（クラスタ名/enum の名前解決はしない＝数値のまま）。
//!
//! **部分結果を返す**: Thread 機器は間欠的に不通になり、属性ごとに成否が割れる。
//! 1 属性の失敗で全スナップショットを捨てると診断にならないので、読めた分だけ返し、
//! 失敗属性は `unavailable`（attribute + kind）に記録する。読めなかったフィールドは
//! `null`、テーブルが空（隣接ゼロ＝孤立）なら `[]` とし、**「未取得」と「真にゼロ」を
//! 区別**する（メッシュ分析でこの差は致命的）。全属性が失敗（＝完全不達）なら最初の
//! 失敗 kind をエラーで返し、`unreachable` / `timeout` を伝播する。

use std::ffi::OsString;
use std::path::Path;
use std::process::Command as StdCommand;

use serde_json::{json, Map, Value};

use crate::runner::ChipTool;
use mat_core::diag::{
    derive_verdict, parse_compressed_fabric_id, parse_operational_instance_cfid, parse_ping6,
    Checks, IpCheck, MdnsCheck, OperationalCheck, ThreadCheck,
};
use mat_core::error::{ErrorKind, MatError};
use mat_core::normalize::classify_failure;
use mat_core::output;
use mat_core::parse::{parse_read_value, parse_struct_list};
use mat_core::store::Store;

/// Thread Network Diagnostics の chip-tool クラスタ名。
const CLUSTER: &str = "threadnetworkdiagnostics";

/// スカラ属性: (出力キー, chip-tool 属性名)。`extended-pan-id` / `pan-id` は
/// 「どの Thread 網に属すか」の識別子（同値＝同じネットワーク／同じ BR 配下）。
/// `routing-role` は中継しているか、`partition-id` はメッシュ分断の検知に使う。
const SCALARS: &[(&str, &str)] = &[
    ("routing_role", "routing-role"),
    ("network_name", "network-name"),
    ("extended_pan_id", "extended-pan-id"),
    ("pan_id", "pan-id"),
    ("partition_id", "partition-id"),
    ("channel", "channel"),
];

/// list-of-struct 属性: (出力キー, chip-tool 属性名)。隣接（LQI/RSSI）と経路（cost）。
/// メッシュ分析の本命。`mat read` のスカラ正規化では潰れる形。
const TABLES: &[(&str, &str)] = &[
    ("neighbor_table", "neighbor-table"),
    ("route_table", "route-table"),
];

pub fn thread(store_path: &Path, node_id: u64, endpoint: u16) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    store.require_node(node_id)?;
    let chip = ChipTool::new(store.root());

    let mut thread = Map::new();
    let mut unavailable = Vec::new();
    let mut any_ok = false;
    let mut first_err: Option<MatError> = None;

    for (key, attr) in SCALARS {
        match read_attr(&chip, node_id, endpoint, attr) {
            Ok(stdout) => {
                any_ok = true;
                thread.insert(
                    key.to_string(),
                    parse_read_value(&stdout).unwrap_or(Value::Null),
                );
            }
            Err(e) => {
                thread.insert(key.to_string(), Value::Null);
                record_failure(attr, e, &mut unavailable, &mut first_err);
            }
        }
    }

    for (key, attr) in TABLES {
        match read_attr(&chip, node_id, endpoint, attr) {
            Ok(stdout) => {
                any_ok = true;
                let rows: Vec<Value> = parse_struct_list(&stdout)
                    .into_iter()
                    .map(Value::Object)
                    .collect();
                thread.insert(key.to_string(), Value::Array(rows));
            }
            Err(e) => {
                // null = 未取得（テーブルが空 `[]` = 真に隣接ゼロ、とは区別する）。
                thread.insert(key.to_string(), Value::Null);
                record_failure(attr, e, &mut unavailable, &mut first_err);
            }
        }
    }

    // 全属性が失敗 = ノード完全不達。診断は出さず原因 kind を伝播（unreachable/timeout）。
    if !any_ok {
        return Err(first_err.unwrap_or_else(|| {
            MatError::new(
                ErrorKind::Other,
                format!("diag thread: no attribute read succeeded for node {node_id}"),
            )
        }));
    }

    emit_diag_thread_success(node_id, endpoint, thread, unavailable);
    Ok(())
}

/// `diag thread` の成功 JSON を stdout へ emit する。chip-tool 経路と native
/// 直経路（`native_direct`）の両方から呼ばれる単一ソース（スキーマ不変）。
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

/// 失敗属性を `unavailable` に記録し、最初の失敗を保持する（全滅時の伝播用）。
fn record_failure(
    attr: &str,
    err: MatError,
    unavailable: &mut Vec<Value>,
    first_err: &mut Option<MatError>,
) {
    unavailable.push(json!({
        "attribute": attr,
        "kind": serde_json::to_value(err.kind).unwrap_or(Value::Null),
    }));
    if first_err.is_none() {
        *first_err = Some(err);
    }
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
    let chip = ChipTool::new(store.root());

    let mut checks = Checks::default();
    let mut unavailable: Vec<Value> = Vec::new();

    // IM 部分（operational + thread）: native 資材があれば native（M8c-2）、
    // 構築失敗・未設定は従来の chip-tool 経路。
    let native_im = native
        .and_then(|cfg| crate::native_direct::diag_im_probe(cfg, store.root(), node_id, endpoint));
    let self_cfid: Option<String> = match native_im {
        Some(p) => {
            checks.operational = Some(OperationalCheck {
                resolved: p.resolved,
                kind: p.op_kind,
            });
            match p.thread {
                Ok(tc) => checks.thread = Some(tc),
                Err(kind) => unavailable.push(json!({ "check": "thread", "kind": kind })),
            }
            Some(p.self_cfid)
        }
        None => {
            // operational: 軽量な descriptor read（ep0、全ノード共通）で解決を試す。
            let op_out = chip.run([
                "descriptor".to_string(),
                "read".to_string(),
                "parts-list".to_string(),
                node_id.to_string(),
                "0".to_string(),
            ])?; // ChildNotFound はここで伝播（診断不能）。
                 // chip-tool は CFID シグナル（[FP] Compressed FabricId 行 / [DIS] の <CFID>-<NodeId>
                 // インスタンス名）を stdout に出す（実機実測; stderr は空のことがある）。fake は
                 // stderr に出すため、両ストリームを結合して走査しバックエンド差に頑健化する。
            let op_logs = format!("{}\n{}", op_out.stdout, op_out.stderr);
            let self_cfid = parse_operational_instance_cfid(&op_logs, node_id)
                .or_else(|| parse_compressed_fabric_id(&op_logs));
            let op_kind = classify_failure(&op_out.stdout, &op_out.stderr);
            let resolved = op_kind.is_none() && op_out.success();
            checks.operational = Some(OperationalCheck {
                resolved,
                kind: op_kind,
            });

            // thread: neighbor-table の LQI と routing-role（部分結果可）。
            match read_thread_signal(&chip, node_id, endpoint) {
                Ok(tc) => checks.thread = Some(tc),
                Err(e) => unavailable.push(json!({ "check": "thread", "kind": e.kind })),
            }

            self_cfid
        }
    };

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

/// neighbor-table（LQI/隣接数）＋ routing-role を読む。neighbor-table が読めなければ Err。
fn read_thread_signal(
    chip: &ChipTool,
    node_id: u64,
    endpoint: u16,
) -> Result<ThreadCheck, MatError> {
    let nt = read_attr(chip, node_id, endpoint, "neighbor-table")?;
    let rows = parse_struct_list(&nt);
    let neighbor_count = rows.len();
    let best_lqi = rows
        .iter()
        .filter_map(|r| r.get("Lqi").and_then(Value::as_u64))
        .map(|v| v as u8)
        .max();
    // routing-role は best-effort（失敗しても thread シグナルは返す）。
    let routing_role = read_attr(chip, node_id, endpoint, "routing-role")
        .ok()
        .and_then(|s| parse_read_value(&s))
        .and_then(|v| v.as_i64());
    Ok(ThreadCheck {
        neighbor_count,
        best_lqi,
        routing_role,
    })
}

/// `--deep` の補助プローブ。ping6（IP生存）と mDNS 広告確認（iface 指定時は
/// native の targeted resolve、未指定・失敗時は avahi-browse）を実施。
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
                unavailable.push(json!({
                    "check": "mdns_self_fabric",
                    "kind": "cfid_unavailable",
                    "detail": "could not obtain self compressed-fabric-id from chip-tool operational logs"
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

/// `chip-tool threadnetworkdiagnostics read <attr> <node> <ep>` を実行し stdout を返す。
/// 失敗分類が立てばそれを、立たず非 0 終了なら child_failed を返す（`read` と同形）。
fn read_attr(chip: &ChipTool, node_id: u64, endpoint: u16, attr: &str) -> Result<String, MatError> {
    let out = chip.run([
        CLUSTER.to_string(),
        "read".to_string(),
        attr.to_string(),
        node_id.to_string(),
        endpoint.to_string(),
    ])?;

    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("diag thread: reading {attr} on node {node_id} endpoint {endpoint} failed"),
        ));
    }
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!(
                "diag thread: chip-tool read {attr} exited with {:?}",
                out.code
            ),
        ));
    }
    Ok(out.stdout)
}
