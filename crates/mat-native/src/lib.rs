//! mat one-shot / matd 常駐の両方が使う native エンジン。warm セッションの
//! 保持方針は呼び出し側の責務。
//!
//! mat-controller の CASE セッション確立・group 送信をここに集約し、
//! チャネルの寿命管理（毎回確立→破棄 or per-node warm 保持）は上位に委ねる。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::{compressed_fabric_id, FabricCredentials};
use mat_controller::im::{ImValue, ATTR_ON_OFF, CLUSTER_ON_OFF};
use mat_controller::message::MATTER_PORT;
use mat_controller::transport::{Transport, UdpTransport};
use mat_controller::{case, dnssd};
use mat_core::error::{ErrorKind, MatError};

pub mod commission;
pub mod group;
pub mod group_settings;
pub mod ops;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

/// native バックエンドの起動設定。
pub struct NativeConfig {
    /// chip-tool KVS のあるディレクトリ（chip-tool の --storage-directory と同一）。
    pub store: std::path::PathBuf,
    /// mDNS scope に使う Thread mesh の iface 名。
    pub iface: String,
    /// KVS fabric テーブルの index（jarvis 本番は 2、alpha は 1）。
    pub fabric_index: u8,
    /// CA issuer index（既定 0）。
    pub issuer_index: u8,
}

/// warm な per-node セッションが提供する操作（実 CASE session or テスト fake）。
#[async_trait]
pub trait NodeConn: Send {
    async fn read_onoff(&mut self, endpoint: u16) -> Result<bool, MatError>;
    async fn invoke(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
        timed: bool,
    ) -> Result<(), MatError>;
    /// 単一属性を任意形状（scalar/struct/array/list）で JSON 読み取る。
    async fn read_json(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
    ) -> Result<serde_json::Value, MatError>;
    /// クラスタ内の全属性をワイルドカード読み取る
    /// （`(attribute_id, value)` を先勝ち順で返す）。
    async fn read_cluster(
        &mut self,
        endpoint: u16,
        cluster: u32,
    ) -> Result<Vec<(u32, serde_json::Value)>, MatError>;
    /// 単一属性へ 1 個の TLV 要素（任意トップレベルタグ）を書き込む。
    async fn write_tlv(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        data_tlv: Vec<u8>,
        timed: bool,
    ) -> Result<(), MatError>;
    /// Enhanced Commissioning Method で一時 commissioning window を開く。
    /// `(manual_code, qr_payload)` を返す（`SecureSession` は `NodeConn` に
    /// 隠蔽されているため、window を開く操作もここに生やす）。
    async fn open_window(
        &mut self,
        timeout_s: u16,
        discriminator: u16,
        iterations: u32,
    ) -> Result<(String, String), MatError>;
}

/// timed リクエストに使う既定タイムアウト（open-window 等の既存値と同じ 10 秒）。
const TIMED_REQUEST_MS: u16 = 10_000;

/// `mat_core::ids::ScalarValue` → `mat_controller::im::ImValue`。mat-core は
/// mat-controller に依存しない設計のため、両者を知る mat-native がここで橋渡しする。
pub fn scalar_to_im(v: &mat_core::ids::ScalarValue) -> ImValue {
    use mat_core::ids::ScalarValue as S;
    match v {
        S::Bool(b) => ImValue::Bool(*b),
        S::UInt(n) => ImValue::Uint(*n),
        S::Int(n) => ImValue::Int(*n),
        S::Str(s) => ImValue::Utf8(s.clone()),
        S::Bytes(b) => ImValue::Bytes(b.clone()),
        S::Null => ImValue::Null,
    }
}

/// `ScalarValue` を Anonymous タグの単一 TLV 要素へ（`write_tlv`/
/// `write_attribute_tlv` に渡す形。呼び出し側がトップレベルタグを再付与する）。
pub fn scalar_to_tlv(v: &mat_core::ids::ScalarValue) -> Vec<u8> {
    use mat_controller::tlv::{Tag, Writer};
    use mat_core::ids::ScalarValue as S;
    let mut w = Writer::new();
    match v {
        S::Bool(b) => w.put_bool(Tag::Anonymous, *b),
        S::UInt(n) => w.put_uint(Tag::Anonymous, *n),
        S::Int(n) => w.put_int(Tag::Anonymous, *n),
        S::Str(s) => w.put_str(Tag::Anonymous, s),
        S::Bytes(b) => w.put_bytes(Tag::Anonymous, b),
        S::Null => w.put_null(Tag::Anonymous),
    }
    w.finish()
}

/// invoke のコマンド引数（スカラー値の列）を CommandFields TLV へ。context tag
/// は引数添字（0-based、`CmdDef::fields` の添字と一致 — `mat_core::ids` の
/// コメント参照）。mat 直経路 (`native_direct`) / matd (`server::native_op`)
/// の両方が使う共有ヘルパ（M8a Task10 で mat 側から移設・一本化）。
pub fn encode_command_fields(args: &[mat_core::ids::ScalarValue]) -> Vec<u8> {
    use mat_controller::tlv::{Tag, Writer};
    use mat_core::ids::ScalarValue as S;
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    for (i, v) in args.iter().enumerate() {
        let tag = Tag::Context(i as u8);
        match v {
            S::Bool(b) => w.put_bool(tag, *b),
            S::UInt(n) => w.put_uint(tag, *n),
            S::Int(n) => w.put_int(tag, *n),
            S::Str(s) => w.put_str(tag, s),
            S::Bytes(b) => w.put_bytes(tag, b),
            S::Null => w.put_null(tag),
        }
    }
    w.end_container();
    w.finish()
}

/// ノード宛の warm セッションを新規確立する手段（実 = mDNS+CASE、テスト = fake）。
#[async_trait]
pub trait Establisher: Send + Sync {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError>;
}

/// native エンジン: 確立器 + （任意の）group 送信コンテキスト。
/// warm セッションを保持するか（matd）、確立→1 op→破棄するか（mat one-shot）は
/// 呼び出し側が決める —— Engine 自体はセッションを持たない。
pub struct Engine {
    pub establisher: Box<dyn Establisher>,
    pub group: Option<group::GroupCtx>,
    pub group_settings: Option<group_settings::GroupSettingsCtx>,
}

/// 手動 `Debug`: `Box<dyn Establisher>` / group ctx は `Debug` を持たず、
/// また表示すべき秘密（鍵）を内包し得るため中身は出さない。`Result::expect_err`
/// が `Engine: Debug` を要求する（build のテスト）ためだけに提供する。
impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

/// mDNS 解決 timeout。SII が来ない場合でも過度に待たない上限。
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);

impl Engine {
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
        // establisher に creds/transport を move する前に、group 送信に要る値を控える。
        let fabric_id = creds.fabric_id;
        let node_id = creds.node_id;
        let cfid = compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
        let group_settings = group_settings::GroupSettingsCtx {
            main_ini: main_ini.clone(),
            fabric_index: cfg.fabric_index,
            cfid,
        };
        let transport = Arc::new(transport);
        let group = group::GroupCtx {
            main_ini,
            counter_path: cfg.store.join("native_group_counter"),
            fabric_index: cfg.fabric_index,
            fabric_id,
            node_id,
            scope_id,
            dest_port: MATTER_PORT,
            transport: Arc::clone(&transport),
            sender: tokio::sync::Mutex::new(None),
        };
        // CaseEstablisher (CASE 確立) は Arc<Transport> を取る一方、GroupCtx
        // の multicast 送信は UdpTransport を直接使い続ける（M6b: BTP 対応の
        // 土台。group 送信は unicast CASE と無関係なので Transport 化しない）。
        let establisher = CaseEstablisher {
            creds: Arc::new(creds),
            transport: Arc::new(Transport::Udp(Arc::clone(&transport))),
            scope_id,
        };
        Ok(Self {
            establisher: Box::new(establisher),
            group: Some(group),
            group_settings: Some(group_settings),
        })
    }

    /// テスト用: 任意の Establisher / group ctx を注入する。group_settings は
    /// None（テストは pub フィールドへ直接代入して注入する）。
    pub fn with_parts(establisher: Box<dyn Establisher>, group: Option<group::GroupCtx>) -> Self {
        Self {
            establisher,
            group,
            group_settings: None,
        }
    }
}

/// 実確立器: 保持した資格情報で mDNS 解決 → CASE。
struct CaseEstablisher {
    creds: Arc<FabricCredentials>,
    transport: Arc<Transport>,
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
        timed: bool,
    ) -> Result<(), MatError> {
        if timed {
            self.session
                .invoke_for_data(
                    endpoint,
                    cluster,
                    command,
                    fields.as_deref(),
                    Some(TIMED_REQUEST_MS),
                    &self.mrp,
                )
                .await
                .map_err(map_session_err)?;
        } else {
            self.session
                .invoke(endpoint, cluster, command, fields.as_deref(), &self.mrp)
                .await
                .map_err(map_session_err)?;
        }
        Ok(())
    }

    async fn read_json(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
    ) -> Result<serde_json::Value, MatError> {
        self.session
            .read_attribute_json(endpoint, cluster, attribute, &self.mrp)
            .await
            .map_err(map_session_err)
    }

    async fn read_cluster(
        &mut self,
        endpoint: u16,
        cluster: u32,
    ) -> Result<Vec<(u32, serde_json::Value)>, MatError> {
        self.session
            .read_cluster_json(endpoint, cluster, &self.mrp)
            .await
            .map_err(map_session_err)
    }

    async fn write_tlv(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        data_tlv: Vec<u8>,
        timed: bool,
    ) -> Result<(), MatError> {
        let timed_ms = timed.then_some(TIMED_REQUEST_MS);
        self.session
            .write_attribute_tlv(endpoint, cluster, attribute, &data_tlv, timed_ms, &self.mrp)
            .await
            .map_err(map_session_err)
    }

    async fn open_window(
        &mut self,
        timeout_s: u16,
        discriminator: u16,
        iterations: u32,
    ) -> Result<(String, String), MatError> {
        let window = mat_controller::commissioning::open_commissioning_window(
            &mut self.session,
            timeout_s,
            discriminator,
            iterations,
            &self.mrp,
        )
        .await
        .map_err(map_commission_err)?;
        Ok((window.manual_code, window.qr_payload))
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

/// `open_commissioning_window`（既存 CASE セッション上の invoke）のエラーを
/// mat の ErrorKind へ写像する。実質的な失敗経路は `Session`（invoke の
/// SessionError と同分類）と `CommandStatus`（デバイスが拒否）に限られる
/// （PASE/attestation 等は既存 operational セッション上では発生しない）が、
/// 網羅性のため他 variant も `Other` へ落とす。
fn map_commission_err(e: mat_controller::commissioning::CommissionError) -> MatError {
    use mat_controller::commissioning::CommissionError;
    match e {
        CommissionError::Session(se) => map_session_err(se),
        CommissionError::CommandStatus { .. } => {
            MatError::new(ErrorKind::DeviceRejected, format!("native: {e}"))
        }
        CommissionError::Timeout(_) => MatError::new(ErrorKind::Timeout, format!("native: {e}")),
        _ => MatError::new(ErrorKind::Other, format!("native: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generic_read_write_via_fake() {
        use crate::test_support::FakeEstablisher;
        let engine = Engine::with_parts(Box::new(FakeEstablisher::default()), None);
        let mut conn = engine.establisher.establish(5).await.unwrap();
        // fake は read_json に固定値を返す（test_support 拡張で定義）。
        let v = conn.read_json(1, 0x0008, 0x0000).await.unwrap();
        assert!(v.is_number());
        conn.write_tlv(
            1,
            0x0008,
            0x0011,
            scalar_to_tlv(&mat_core::ids::ScalarValue::UInt(128)),
            false,
        )
        .await
        .unwrap();
        let all = conn.read_cluster(1, 0x0006).await.unwrap();
        assert!(!all.is_empty());
    }

    #[test]
    fn scalar_conversions() {
        use mat_controller::im::ImValue;
        use mat_core::ids::ScalarValue as S;
        assert_eq!(scalar_to_im(&S::Bool(true)), ImValue::Bool(true));
        assert_eq!(scalar_to_im(&S::UInt(7)), ImValue::Uint(7));
        // scalar_to_tlv は Reader で読み戻して値一致を確認。
        let b = scalar_to_tlv(&S::Str("x".into()));
        let mut r = mat_controller::tlv::Reader::new(&b);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            mat_controller::tlv::Value::Utf8("x")
        ));
    }

    #[test]
    fn encode_command_fields_uses_positional_context_tags() {
        use mat_core::ids::ScalarValue as S;
        let tlv = encode_command_fields(&[S::UInt(128), S::UInt(0)]);
        let mut r = mat_controller::tlv::Reader::new(&tlv);
        let el = r.next().unwrap().unwrap();
        assert!(matches!(el.value, mat_controller::tlv::Value::StructStart));
        // 空引数は空 struct（要素 0 個）にエンコードされる。
        let empty = encode_command_fields(&[]);
        let mut r2 = mat_controller::tlv::Reader::new(&empty);
        assert!(r2.next().unwrap().is_some());
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
        let err = Engine::build(&cfg).await.expect_err("no KVS present");
        assert!(
            matches!(err.kind, ErrorKind::StoreMissing | ErrorKind::Other),
            "unexpected kind: {:?}",
            err.kind
        );
    }
}
