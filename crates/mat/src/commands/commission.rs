//! `mat commission` — fabric への参加（初回 commission / multi-admin join 両対応）。
//!
//! `chip-tool pairing code <node-id> <setup-code>` をラップする。`setup_code` は
//! 印刷された QR/manual code（初回）でも、既存 admin が開いた window の発行コード
//! （join）でも一様に扱える。Root CA / 自分の NOC は chip-tool が初回 pairing 時に
//! ストア配下へ生成・永続する。
//!
//! `target`（IP/DNS）は台帳のメタとして記録する。`pairing code` はコード内の
//! discriminator から mDNS でノードを自前探索するため、chip-tool には渡さない。

use std::path::{Path, PathBuf};

use serde_json::json;

use mat_core::error::{ErrorKind, MatError};
use mat_core::output;
use mat_core::parse::commission_succeeded;
use crate::runner::ChipTool;
use mat_core::normalize::classify_failure;
use mat_core::store::{NodeRecord, Store};

pub fn run(
    store_path: &Path,
    target: &str,
    setup_code: &str,
    node_id: Option<u64>,
) -> Result<(), MatError> {
    // commission はストアを bootstrap してよい経路（初回 fabric 作成を含む）。
    let mut store = Store::open_or_init(store_path)?;
    let chip = ChipTool::new(store.root());

    let node_id = node_id.unwrap_or_else(|| next_node_id(&store));

    // 本番 Matter デバイスは DAC が本番 PAA で署名されており、chip-tool 既定の
    // 開発用 PAA だけでは attestation 検証に失敗する（Failed Device Attestation）。
    // PAA ルート証明書ディレクトリが解決できれば chip-tool に渡す。
    let mut args: Vec<String> = vec![
        "pairing".to_string(),
        "code".to_string(),
        node_id.to_string(),
        setup_code.to_string(),
    ];
    if let Some(paa) = paa_trust_store_path(store.root()) {
        args.push("--paa-trust-store-path".to_string());
        args.push(paa.to_string_lossy().into_owned());
    }

    let out = chip.run(args)?;

    if out.success() && commission_succeeded(&out.stdout) {
        store.upsert_node(NodeRecord {
            node_id,
            address: Some(target.to_string()),
            commissioned_at: output::now_iso8601(),
        })?;
        output::emit(json!({ "node_id": node_id, "status": "success" }));
        return Ok(());
    }

    // 失敗。chip-tool の粗い exit code に頼らず出力から種別を分類し、
    // 分類できなければ commission_failed にフォールバック。
    let kind = classify_failure(&out.stdout, &out.stderr).unwrap_or(ErrorKind::CommissionFailed);
    Err(MatError::new(
        kind,
        format!("commissioning node {node_id} ({target}) failed"),
    ))
}

/// 台帳の最大 node_id + 1。空なら 1。
fn next_node_id(store: &Store) -> u64 {
    store.nodes().map(|n| n.node_id).max().map_or(1, |m| m + 1)
}

/// PAA（Product Attestation Authority）ルート証明書ディレクトリを解決する。
///
/// 優先順位:
/// 1. `MAT_PAA_TRUST_STORE`（明示指定。存在は問わず chip-tool に委ねる）
/// 2. `<store>/paa-trust-store/`（存在すれば）
///
/// どちらも無ければ `None`。その場合 chip-tool は既定の開発用 PAA だけで検証する
/// ため、本番デバイスは `device_rejected`（Failed Device Attestation）になる。
/// 証明書は connectedhomeip の `credentials/production/paa-root-certs/` から取得する。
fn paa_trust_store_path(store_root: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MAT_PAA_TRUST_STORE") {
        return Some(PathBuf::from(p));
    }
    let default = store_root.join("paa-trust-store");
    default.is_dir().then_some(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paa_none_when_unset_and_no_default_dir() {
        // 干渉を避けるため env を見ない既定パス側だけを検証する。
        if std::env::var_os("MAT_PAA_TRUST_STORE").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(paa_trust_store_path(dir.path()), None);
    }

    #[test]
    fn paa_uses_default_dir_when_present() {
        if std::env::var_os("MAT_PAA_TRUST_STORE").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let paa = dir.path().join("paa-trust-store");
        std::fs::create_dir(&paa).unwrap();
        assert_eq!(paa_trust_store_path(dir.path()), Some(paa));
    }
}
