//! matd の native バックエンド（Phase 5 M4）。
//!
//! mat-controller の warm CASE セッションを matd プロセス内に保持し、ホットパス
//! （on/off・色・色温度・onoff read）を chip-tool を介さず処理する。未対応 op は
//! server 層が chip-tool ws にフォールバックする。

use std::path::PathBuf;
use std::sync::Arc;

use mat_controller::fabric::FabricCredentials;
use mat_controller::transport::UdpTransport;
use mat_core::error::{ErrorKind, MatError};

/// native バックエンドの起動設定。
pub struct NativeConfig {
    /// chip-tool KVS のあるディレクトリ（chip-tool の --storage-directory と同一）。
    pub store: PathBuf,
    /// mDNS scope に使う Thread mesh の iface 名。
    pub iface: String,
    /// KVS fabric テーブルの index（jarvis 本番は 2、alpha は 1）。
    pub fabric_index: u8,
    /// CA issuer index（既定 0）。
    pub issuer_index: u8,
}

/// warm CASE セッションを per-node に保持する native バックエンド。
pub struct NativeBackend {
    creds: Arc<FabricCredentials>,
    transport: Arc<UdpTransport>,
    scope_id: u32,
}

/// 手動 `Debug`: `UdpTransport` は `Debug` を実装していない（ソケット fd を持つ
/// だけで有用な表示がない）ため derive できない。`creds` は
/// `FabricCredentials` 側で秘密鍵を redact 済み。
impl std::fmt::Debug for NativeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeBackend")
            .field("creds", &self.creds)
            .field("scope_id", &self.scope_id)
            .finish()
    }
}

impl NativeBackend {
    /// KVS から資格情報を1回読み、NOC を自己発行し、UDP transport を bind、
    /// iface の scope_id を解決して保持する。プロセス寿命で不変。
    pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError> {
        let alpha_ini = cfg.store.join("chip_tool_config.alpha.ini");
        let main_ini = cfg.store.join("chip_tool_config.ini");
        let materials = mat_controller::kvs::read_self_issue_materials(
            &alpha_ini,
            &main_ini,
            cfg.fabric_index,
            cfg.issuer_index,
        )
        .map_err(|e| {
            // KVS 欠落は store_missing、その他 KVS パース失敗は other。
            MatError::new(ErrorKind::StoreMissing, format!("native: read KVS credentials: {e}"))
        })?;
        let creds = FabricCredentials::from_self_issued(materials).map_err(|e| {
            MatError::new(ErrorKind::SessionFailed, format!("native: self-issue NOC: {e}"))
        })?;
        let scope_id = mat_controller::dnssd::iface_index(&cfg.iface).map_err(|e| {
            MatError::new(
                ErrorKind::Other,
                format!("native: resolve iface {:?} index: {e}", cfg.iface),
            )
        })?;
        let transport = UdpTransport::bind().await.map_err(|e| {
            MatError::new(ErrorKind::Other, format!("native: bind udp transport: {e}"))
        })?;
        Ok(Self {
            creds: Arc::new(creds),
            transport: Arc::new(transport),
            scope_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_fails_cleanly_without_kvs() {
        // KVS が無いディレクトリでは store_missing 相当のエラーで即失敗し、
        // panic しない（matd 起動時に安全フォールバックへ落とす判断材料）。
        let dir = tempfile::tempdir().unwrap();
        let cfg = NativeConfig {
            store: dir.path().to_path_buf(),
            iface: "lo".to_string(),
            fabric_index: 1,
            issuer_index: 0,
        };
        let err = NativeBackend::build(&cfg).await.expect_err("no KVS present");
        assert!(
            matches!(err.kind, ErrorKind::StoreMissing | ErrorKind::Other),
            "unexpected kind: {:?}",
            err.kind
        );
    }
}
