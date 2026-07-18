//! `mat commission` — fabric への参加（初回 commission / multi-admin join 両対応）。
//!
//! バックエンドは native（`mat-native::commission`, M8c-1; M8c-3 で chip-tool
//! 経路は撤去）。`setup_code` は印刷された QR/manual code（初回）でも、既存 admin が
//! 開いた window の発行コード（join）でも一様に扱える。Root CA / 自分の NOC は
//! `mat fabric init` が事前に生成・永続する。
//!
//! `target`（IP/DNS）は台帳のメタとして記録する。コード内の discriminator から
//! mDNS でノードを自前探索する。

use std::path::{Path, PathBuf};

use serde_json::json;

use mat_core::alias::AliasBook;
use mat_core::error::{ErrorKind, MatError};
use mat_core::output;
use mat_core::store::{NodeRecord, Store};

pub fn run(
    store_path: &Path,
    target: &str,
    setup_code: &str,
    node_id: Option<u64>,
    alias: Option<&str>,
    native: Option<&crate::native_direct::Config<'_>>,
    thread_dataset: Option<&str>,
) -> Result<(), MatError> {
    // commission はストアを bootstrap してよい経路（node_id 採番のため）。
    let mut store = Store::open_or_init(store_path)?;
    let node_id = node_id.unwrap_or_else(|| next_node_id(&store));

    // native commission（M8c-1; M8c-3 で唯一の経路）。発見空振り = unreachable、
    // KVS/資材/epoch 系 = store_missing/store_parse、PASE 開始後の失敗も含め
    // すべてハードエラー（chip-tool フォールバックは撤去）。
    let cfg = native.ok_or_else(|| {
        MatError::new(
            ErrorKind::Other,
            "commission: native backend not configured (internal)",
        )
    })?;
    native_commission(cfg, &store, setup_code, node_id, thread_dataset)?;
    record_success(&mut store, node_id, target, alias)
}

fn native_commission(
    cfg: &crate::native_direct::Config<'_>,
    store: &Store,
    setup_code: &str,
    node_id: u64,
    thread_dataset: Option<&str>,
) -> Result<(), MatError> {
    let dataset = thread_dataset
        .map(|h| {
            decode_hex(h).ok_or_else(|| {
                MatError::new(
                    ErrorKind::Other,
                    "invalid --thread-dataset: expected hex bytes".to_string(),
                )
            })
        })
        .transpose()?;
    let req = mat_native::commission::CommissionRequest {
        setup_code: setup_code.to_string(),
        device_node_id: node_id,
        thread_dataset: dataset,
        paa_dir: paa_trust_store_path(store.root()),
        cd_signer_dir: cd_signer_store_path(store.root()),
    };
    let ncfg = mat_native::NativeConfig {
        store: store.root().to_path_buf(),
        iface: cfg.iface.to_string(),
        fabric_index: cfg.fabric_index,
        issuer_index: cfg.issuer_index,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| MatError::new(ErrorKind::Other, format!("tokio runtime: {e}")))?;
    rt.block_on(mat_native::commission::commission(&ncfg, &req))
}

/// 台帳 upsert + alias + JSON 出力（chip-tool 経路の成功側と共通）。
fn record_success(
    store: &mut Store,
    node_id: u64,
    target: &str,
    alias: Option<&str>,
) -> Result<(), MatError> {
    store.upsert_node(NodeRecord {
        node_id,
        address: Some(target.to_string()),
        commissioned_at: output::now_iso8601(),
    })?;
    if let Some(name) = alias {
        // 名前の妥当性・重複は resolve 層で事前検証済み。ここで失敗するのは
        // 書き込みエラー等のみ（commission 自体は成功しているので detail に明記）。
        let mut book = AliasBook::load(store.root())?;
        book.insert_node_alias(name, node_id, store.root())
            .map_err(|e| {
                MatError::new(
                    e.kind,
                    format!(
                        "node {node_id} was commissioned, but writing alias '{name}' failed: {}",
                        e.detail
                    ),
                )
            })?;
    }
    output::emit(json!({ "node_id": node_id, "status": "success" }));
    Ok(())
}

/// 偶数桁の hex 文字列 → bytes。
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() || !s.is_ascii() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// CD signer 証明書ディレクトリ（PAA と同型の解決順）。
fn cd_signer_store_path(store_root: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MAT_CD_SIGNER_STORE") {
        return Some(PathBuf::from(p));
    }
    let default = store_root.join("cd-signer-store");
    default.is_dir().then_some(default)
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

    #[test]
    fn decode_hex_rejects_odd_length() {
        assert_eq!(decode_hex("abc"), None);
    }

    #[test]
    fn decode_hex_rejects_empty() {
        assert_eq!(decode_hex(""), None);
    }

    #[test]
    fn decode_hex_parses_valid_bytes() {
        assert_eq!(decode_hex("0e08"), Some(vec![0x0e, 0x08]));
    }

    #[test]
    fn decode_hex_rejects_non_ascii_without_panicking() {
        // 偶数バイト長の非ASCII（"aéa" = 4バイト）でも panic せず None。
        assert_eq!(decode_hex("aéa"), None);
    }

    #[test]
    fn cd_signer_none_when_unset_and_no_default_dir() {
        if std::env::var_os("MAT_CD_SIGNER_STORE").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(cd_signer_store_path(dir.path()), None);
    }

    #[test]
    fn cd_signer_uses_default_dir_when_present() {
        if std::env::var_os("MAT_CD_SIGNER_STORE").is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cd = dir.path().join("cd-signer-store");
        std::fs::create_dir(&cd).unwrap();
        assert_eq!(cd_signer_store_path(dir.path()), Some(cd));
    }
}
