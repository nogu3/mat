//! matd の native バックエンド（Phase 5 M4）。
//!
//! mat-controller の warm CASE セッションを matd プロセス内に保持し、ホットパス
//! （on/off・色・色温度・onoff read）を chip-tool を介さず処理する。未対応 op は
//! server 層が chip-tool ws にフォールバックする。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;

use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::{compressed_fabric_id, FabricCredentials};
use mat_controller::im::{
    self, ImValue, ATTR_ON_OFF, CLUSTER_COLOR_CONTROL, CLUSTER_ON_OFF,
    CMD_MOVE_TO_COLOR_TEMPERATURE, CMD_MOVE_TO_HUE_AND_SATURATION, CMD_ON_OFF_OFF, CMD_ON_OFF_ON,
};
use mat_controller::transport::UdpTransport;
use mat_controller::{case, dnssd};
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

/// warm な per-node セッションが提供する操作（実 CASE session or テスト fake）。
#[async_trait]
pub(crate) trait NodeConn: Send {
    async fn read_onoff(&mut self, endpoint: u16) -> Result<bool, MatError>;
    async fn invoke(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
    ) -> Result<(), MatError>;
}

/// ノード宛の warm セッションを新規確立する手段（実 = mDNS+CASE、テスト = fake）。
#[async_trait]
pub(crate) trait Establisher: Send + Sync {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError>;
}

/// per-node の warm session slot。`None` = 未確立 or 破棄済み（次回確立）。
/// 外側 `Arc` を短時間の外側ロック下で clone し、往復は内側 `Mutex` で直列化する。
type NodeSlot = Arc<Mutex<Option<Box<dyn NodeConn>>>>;

/// warm CASE セッションを per-node に保持する native バックエンド。
pub struct NativeBackend {
    establisher: Box<dyn Establisher>,
    sessions: Mutex<HashMap<u64, NodeSlot>>,
}

/// 手動 `Debug`: `Box<dyn Establisher>` / warm セッションは `Debug` を持たず、
/// また表示すべき秘密（鍵）を内包し得るため中身は出さない。`Result::expect_err`
/// が `NativeBackend: Debug` を要求する（build のテスト）ためだけに提供する。
impl std::fmt::Debug for NativeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeBackend").finish_non_exhaustive()
    }
}

/// mDNS 解決 timeout。SII が来ない場合でも過度に待たない上限。
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);

impl NativeBackend {
    /// KVS から資格情報を1回読み、NOC を自己発行し、UDP transport を bind、
    /// iface の scope_id を解決して実確立器を構築する。プロセス寿命で不変。
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
            // KVS 読み取り失敗は一律 store_missing に写像（細分化は将来）。
            MatError::new(
                ErrorKind::StoreMissing,
                format!("native: read KVS credentials: {e}"),
            )
        })?;
        let creds = FabricCredentials::from_self_issued(materials).map_err(|e| {
            MatError::new(
                ErrorKind::SessionFailed,
                format!("native: self-issue NOC: {e}"),
            )
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
        let establisher = CaseEstablisher {
            creds: Arc::new(creds),
            transport: Arc::new(transport),
            scope_id,
        };
        Ok(Self::with_establisher(Box::new(establisher)))
    }

    /// テスト用: 任意の Establisher を注入する。
    pub(crate) fn with_establisher(establisher: Box<dyn Establisher>) -> Self {
        Self {
            establisher,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// この node の per-node slot（`Arc<Mutex<Option<..>>>`）を得る。外側ロックは
    /// slot 取得の間だけ保持して即解放する（ノード間の並行性を保つ）。
    async fn slot(&self, node_id: u64) -> NodeSlot {
        let mut map = self.sessions.lock().await;
        Arc::clone(
            map.entry(node_id)
                .or_insert_with(|| Arc::new(Mutex::new(None))),
        )
    }

    /// warm セッションで `op` を実行する。slot が空なら確立。送信が Timeout
    /// （MRP 尽き=session が死んでいる兆候）なら slot を捨てて1回だけ再確立し再送する。
    /// device_rejected 等（コマンドは届いた）は再送しない。
    async fn with_session<F, T>(&self, node_id: u64, op: F) -> Result<T, MatError>
    where
        F: for<'a> Fn(
            &'a mut Box<dyn NodeConn>,
        ) -> Pin<
            Box<dyn std::future::Future<Output = Result<T, MatError>> + Send + 'a>,
        >,
    {
        let slot = self.slot(node_id).await;
        let mut guard = slot.lock().await;
        if guard.is_none() {
            *guard = Some(self.establisher.establish(node_id).await?);
        }
        let result = op(guard.as_mut().expect("established above")).await;
        match result {
            Ok(v) => Ok(v),
            Err(e) if e.kind == ErrorKind::Timeout => {
                // session が死んだ疑い。捨てて1回だけ再確立→再送。
                tracing::info!(
                    node_id,
                    "native session send timed out; re-establishing once"
                );
                *guard = None;
                *guard = Some(self.establisher.establish(node_id).await?);
                op(guard.as_mut().expect("re-established")).await
            }
            Err(e) => Err(e),
        }
    }

    pub async fn read_onoff(&self, node_id: u64, endpoint: u16) -> Result<bool, MatError> {
        self.with_session(node_id, |c| c.read_onoff(endpoint)).await
    }

    pub async fn on(&self, node_id: u64, endpoint: u16) -> Result<(), MatError> {
        self.with_session(node_id, |c| {
            c.invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None)
        })
        .await
    }

    pub async fn off(&self, node_id: u64, endpoint: u16) -> Result<(), MatError> {
        self.with_session(node_id, |c| {
            c.invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_OFF, None)
        })
        .await
    }

    pub async fn color(
        &self,
        node_id: u64,
        endpoint: u16,
        hue_raw: u8,
        saturation_raw: u8,
        transition: u16,
    ) -> Result<(), MatError> {
        let fields =
            im::encode_move_to_hue_and_saturation_fields(hue_raw, saturation_raw, transition);
        self.with_session(node_id, move |c| {
            c.invoke(
                endpoint,
                CLUSTER_COLOR_CONTROL,
                CMD_MOVE_TO_HUE_AND_SATURATION,
                Some(fields.clone()),
            )
        })
        .await
    }

    pub async fn color_temp(
        &self,
        node_id: u64,
        endpoint: u16,
        mireds: u16,
        transition: u16,
    ) -> Result<(), MatError> {
        let fields = im::encode_move_to_color_temperature_fields(mireds, transition);
        self.with_session(node_id, move |c| {
            c.invoke(
                endpoint,
                CLUSTER_COLOR_CONTROL,
                CMD_MOVE_TO_COLOR_TEMPERATURE,
                Some(fields.clone()),
            )
        })
        .await
    }
}

/// 実確立器: 保持した資格情報で mDNS 解決 → CASE。
struct CaseEstablisher {
    creds: Arc<FabricCredentials>,
    transport: Arc<UdpTransport>,
    scope_id: u32,
}

#[async_trait]
impl Establisher for CaseEstablisher {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
        let cfid = compressed_fabric_id(&self.creds.root_public_key, self.creds.fabric_id);
        let resolved = dnssd::resolve_operational(self.scope_id, &cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| {
                MatError::new(
                    ErrorKind::Unreachable,
                    format!("native: mDNS resolve node {node_id}: {e}"),
                )
            })?;
        let mrp = resolved.mrp_config();
        let peers: Vec<SocketAddr> = resolved.socket_addrs(self.scope_id);
        let mut last: Option<MatError> = None;
        for peer in peers {
            match case::establish(
                Arc::clone(&self.transport),
                peer,
                &self.creds,
                node_id,
                &mrp,
            )
            .await
            {
                Ok(session) => {
                    return Ok(Box::new(SessionConn { session, mrp }));
                }
                Err(e) => {
                    last = Some(MatError::new(
                        ErrorKind::SessionFailed,
                        format!("native: CASE via {peer}: {e}"),
                    ));
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            MatError::new(
                ErrorKind::Unreachable,
                format!("native: no addresses resolved for node {node_id}"),
            )
        }))
    }
}

/// 実セッション: SecureSession + そのノードの MRP 設定。
struct SessionConn {
    session: mat_controller::session::SecureSession,
    mrp: MrpConfig,
}

#[async_trait]
impl NodeConn for SessionConn {
    async fn read_onoff(&mut self, endpoint: u16) -> Result<bool, MatError> {
        match self
            .session
            .read_attribute(endpoint, CLUSTER_ON_OFF, ATTR_ON_OFF, &self.mrp)
            .await
            .map_err(map_session_err)?
        {
            ImValue::Bool(b) => Ok(b),
            other => Err(MatError::parse_error(format!(
                "native: on-off not a bool: {other:?}"
            ))),
        }
    }

    async fn invoke(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
    ) -> Result<(), MatError> {
        self.session
            .invoke(endpoint, cluster, command, fields.as_deref(), &self.mrp)
            .await
            .map_err(map_session_err)?;
        Ok(())
    }
}

/// SecureSession のエラーを mat の ErrorKind へ写像する（経路によらず分類を揃える）。
fn map_session_err(e: mat_controller::session::SessionError) -> MatError {
    use mat_controller::session::SessionError;
    match e {
        // MRP 再送尽き。session が死んでいる兆候 → 上位が1回だけ再確立を試みる。
        SessionError::Timeout => MatError::new(ErrorKind::Timeout, format!("native: {e}")),
        // デバイスがコマンド/読みを IM ステータスで拒否 → コマンドは届いた。
        SessionError::Im(_) => MatError::new(ErrorKind::DeviceRejected, format!("native: {e}")),
        SessionError::Io(_) => MatError::new(ErrorKind::Unreachable, format!("native: {e}")),
        _ => MatError::new(ErrorKind::Other, format!("native: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    /// 送信 n 回目に Timeout を返すよう設定できる fake セッション。
    struct FakeConn {
        fail_first_send: bool,
        sent: AtomicUsize,
    }

    #[async_trait]
    impl NodeConn for FakeConn {
        async fn read_onoff(&mut self, _endpoint: u16) -> Result<bool, MatError> {
            let n = self.sent.fetch_add(1, Ordering::SeqCst);
            if self.fail_first_send && n == 0 {
                return Err(MatError::new(ErrorKind::Timeout, "fake MRP exhausted"));
            }
            Ok(true)
        }
        async fn invoke(
            &mut self,
            _endpoint: u16,
            _cluster: u32,
            _command: u32,
            _fields: Option<Vec<u8>>,
        ) -> Result<(), MatError> {
            Ok(())
        }
    }

    /// establish 呼び出し回数を外部の `Arc<AtomicUsize>` で数える fake。
    /// `fail_first_send` を確立する Conn に伝える（2 回目の確立=再確立では成功）。
    struct FakeEstablisher {
        calls: std::sync::Arc<AtomicUsize>,
        fail_first_send: bool,
    }

    #[async_trait]
    impl Establisher for FakeEstablisher {
        async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeConn {
                fail_first_send: self.fail_first_send && n == 0,
                sent: AtomicUsize::new(0),
            }))
        }
    }

    #[tokio::test]
    async fn reuses_warm_session_for_same_node() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: false,
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        backend.read_onoff(0x1234, 1).await.unwrap();
        backend.read_onoff(0x1234, 1).await.unwrap();
        // 2 回のコマンドで establish は 1 回だけ（warm 再利用）。
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn re_establishes_once_on_send_timeout() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: true,
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // 1 回目の send が Timeout → slot 破棄 → 再確立 → 再送成功。
        let v = backend.read_onoff(0x1234, 1).await.unwrap();
        assert!(v);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

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
        let err = NativeBackend::build(&cfg)
            .await
            .expect_err("no KVS present");
        assert!(
            matches!(err.kind, ErrorKind::StoreMissing | ErrorKind::Other),
            "unexpected kind: {:?}",
            err.kind
        );
    }
}
