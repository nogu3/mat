//! mDNS プローブ。`--iface`（`MAT_IFACE`）設定時、native の**台帳ノードごとの
//! targeted resolve 並行実行**（`mat-controller::dnssd::resolve_operational`、
//! M8b）を実施する。M8c-3 Task 11 で `avahi-browse` フォールバックを撤去 —
//! mDNS は dnssd 一本で、I/O エラーもそのままエラーを返す（フォールバック先が
//! 無い）。
//!
//! M8b で列挙(browse)ベースから切り替えた: 実機の advertising proxy は一部の
//! 登録済み instance（例: node 6/8/9）について `_matter._tcp` の PTR 列挙に
//! 一切応答しない（KA suppression 後も、tcpdump で確認）一方、targeted な
//! resolve（CASE が使うのと同じ経路）は同ノードに成功する（native read 実証
//! 済み、2026-07-17）。probe は対象ノードの CFID/NodeId が既知なので、列挙で
//! 発見する必要がなく、resolve を並行実行すれば十分。
//!
//! socket I/O を伴うため副作用なしの `mat-core` ではなくバイナリ側に置く。
//! `diag node --deep` と `discover --probe` が共有する。

use std::time::Duration;

use mat_controller::{dnssd, fabric, kvs};
use mat_core::diag::MatterInstance;
use mat_core::error::{ErrorKind, MatError};

/// native probe の入力。probe は CFID 計算のため KVS（読み取りのみ）を使う。
pub struct NativeProbe<'a> {
    pub iface: &'a str,
    pub fabric_index: u8,
    pub issuer_index: u8,
    pub store_root: &'a std::path::Path,
    /// 到達性を判定したい台帳ノード（diag は対象 1 ノードのみ）。
    pub node_ids: &'a [u64],
}

/// 1 ノードあたりの resolve タイムアウト。全ノード並行実行のため、
/// 台帳が何ノードあっても総所要時間はおよそこの値に収まる。
const PROBE_RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

/// `_matter._tcp` の到達性を判定する。native の targeted resolve を並行実行
/// する。結果 0 件は正常（Ok(vec![])）。失敗はそのままエラーを返す
/// （フォールバック先が無い — Task 11）。失敗源ごとに ErrorKind を作り分ける
/// （Task 11 レビュー修正）: iface 解決失敗は `Other`、KVS 資材の読み取り /
/// NOC 自己発行失敗は `StoreMissing`（`mat fabric init` 未実施のヒント付き）、
/// 全ノード I/O エラーのみ `Unreachable`。
pub fn mdns(p: NativeProbe<'_>) -> Result<Vec<MatterInstance>, MatError> {
    resolve_ledger_nodes(&p)
}

/// 台帳ノードそれぞれへ `resolve_operational` を並行実行する（M8b）。
/// 失敗源ごとに ErrorKind を作り分ける（mat-native/lib.rs・commission.rs の
/// 資材エラー写像に合わせる — Task 11 レビュー修正）。
fn resolve_ledger_nodes(p: &NativeProbe<'_>) -> Result<Vec<MatterInstance>, MatError> {
    // 台帳が空なら何も解決しない（KVS も socket も不要）。
    if p.node_ids.is_empty() {
        return Ok(vec![]);
    }

    let scope_id = dnssd::iface_index(p.iface).map_err(|e| {
        MatError::new(
            ErrorKind::Other,
            format!("native mDNS probe: resolve iface {:?} index: {e}", p.iface),
        )
    })?;

    let materials = kvs::read_self_issue_materials(
        &p.store_root.join("chip_tool_config.alpha.ini"),
        &p.store_root.join("chip_tool_config.ini"),
        p.fabric_index,
        p.issuer_index,
    )
    .map_err(|e| {
        MatError::new(
            ErrorKind::StoreMissing,
            format!("native mDNS probe: read KVS credentials: {e} — run `mat fabric init`"),
        )
    })?;
    let creds = fabric::FabricCredentials::from_self_issued(materials).map_err(|e| {
        MatError::new(
            ErrorKind::StoreMissing,
            format!("native mDNS probe: self-issue NOC: {e} — run `mat fabric init`"),
        )
    })?;
    let cfid = fabric::compressed_fabric_id(&creds.root_public_key, creds.fabric_id);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            MatError::new(
                ErrorKind::Other,
                format!("native mDNS probe: build async runtime: {e}"),
            )
        })?;
    let results: Vec<(u64, Result<dnssd::ResolvedNode, dnssd::DnssdError>)> = rt.block_on(async {
        let mut set = tokio::task::JoinSet::new();
        for &node_id in p.node_ids {
            set.spawn(async move {
                let res =
                    dnssd::resolve_operational(scope_id, &cfid, node_id, PROBE_RESOLVE_TIMEOUT)
                        .await;
                (node_id, res)
            });
        }
        let mut out = Vec::with_capacity(p.node_ids.len());
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(pair) => out.push(pair),
                Err(e) => tracing::debug!(error = %e, "probe: resolve task join failed"),
            }
        }
        out
    });

    // 全ノードが Io エラーの場合はハードエラーを返す（例: MAT_IFACE=lo では
    // multicast send 自体が全ノードで失敗する。フォールバック先は無い —
    // Task 11 で撤去済み）。混在時は成功分を返す（個々の Timeout/Malformed は
    // 「不達」として扱い、全滅ではない）。
    let all_io_err = !results.is_empty()
        && results
            .iter()
            .all(|(_, r)| matches!(r, Err(dnssd::DnssdError::Io(_))));
    if all_io_err {
        return Err(MatError::new(
            ErrorKind::Unreachable,
            format!(
                "native mDNS probe failed on {}: all ledger node resolves failed with an I/O error",
                p.iface
            ),
        ));
    }

    let cfid_hex = cfid_hex(&cfid);
    let mut list = Vec::new();
    for (node_id, res) in results {
        match res {
            Ok(node) => list.push(MatterInstance {
                compressed_fabric: cfid_hex.clone(),
                node_id,
                addresses: node.addresses.iter().map(|a| a.to_string()).collect(),
            }),
            Err(dnssd::DnssdError::Timeout { .. }) => {
                tracing::debug!(node_id, "probe: node did not resolve within the deadline");
            }
            Err(e) => {
                tracing::debug!(node_id, error = %e, "probe: node resolve failed");
            }
        }
    }

    tracing::info!(
        resolved = list.len(),
        probed = p.node_ids.len(),
        "probe executed (native resolve)"
    );
    Ok(list)
}

/// compressed fabric id → 16 桁大文字 hex（`MatterInstance::compressed_fabric`
/// / diag の self-fabric 照合が期待する形）。
fn cfid_hex(cfid: &[u8; 8]) -> String {
    cfid.iter().map(|b| format!("{b:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfid_hex_formats_16_uppercase_hex() {
        let cfid: [u8; 8] = [0x00, 0xAA, 0xBB, 0x11, 0x22, 0xCC, 0x33, 0x44];
        assert_eq!(cfid_hex(&cfid), "00AABB1122CC3344");
    }

    /// KVS 資材が無い store（`mat fabric init` 未実施相当）+ 台帳に 1 ノードの
    /// 組み合わせは `StoreMissing`（`Unreachable` への一律写像ではない）に
    /// 分類されるべき — Task 11 レビュー修正。
    #[test]
    fn resolve_ledger_nodes_maps_missing_kvs_materials_to_store_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = NativeProbe {
            iface: "lo",
            fabric_index: 1,
            issuer_index: 1,
            store_root: dir.path(),
            node_ids: &[5],
        };

        let err = resolve_ledger_nodes(&p).expect_err("missing KVS materials must fail");

        assert_eq!(err.kind, ErrorKind::StoreMissing);
        assert!(
            err.detail.contains("mat fabric init"),
            "detail should hint at `mat fabric init`: {}",
            err.detail
        );
    }

    /// 台帳が空なら KVS にも socket にも触れず Ok(vec![]) を返す（既存動作の
    /// 回帰確認 — iface / KVS が壊れていても失敗しないことを保証する）。
    #[test]
    fn resolve_ledger_nodes_empty_ledger_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        let p = NativeProbe {
            iface: "no-such-iface-at-all",
            fabric_index: 1,
            issuer_index: 1,
            store_root: dir.path(),
            node_ids: &[],
        };

        let result = resolve_ledger_nodes(&p).expect("empty ledger must short-circuit to Ok");
        assert!(result.is_empty());
    }
}
