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
pub mod iface_select;
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

/// 購読パラメータ: 人感の即応性優先で floor 0、再購読時に古い購読を掃除するため
/// KeepSubscriptions=false。ceiling は当初 3600s（電池優先）だったが、実機 E2E で
/// 「flaky リンクのデバイスがレポート配送失敗時に購読を黙って破棄 → こちらは
/// MaxInterval×1.5 = 90 分間死活を検知できない」盲目窓が核心機能を殺すと判明し
/// 300s に短縮（keepalive 5 分毎、死活検知 ≤7.5 分で自動再購読）。
pub const SUBSCRIBE_MIN_INTERVAL_FLOOR_S: u16 = 0;
pub const SUBSCRIBE_MAX_INTERVAL_CEILING_S: u16 = 300;
pub const SUBSCRIBE_KEEP_SUBSCRIPTIONS: bool = false;

/// 購読成立の結果（SubscriptionId とデバイス選択の MaxInterval）。
#[derive(Debug, Clone, Copy)]
pub struct SubscriptionInfo {
    pub subscription_id: u32,
    pub max_interval_s: u16,
}

/// 購読専用コネクション（専用 UdpTransport + 専用 CASE をポンプが独占する。
/// 既存 op 経路 = warm session は不変 — spec 構造判断）。
#[async_trait]
pub trait SubscribeConn: Send {
    /// Subscribe を張り、成立情報と priming report 群を返す。`clusters` 空 =
    /// full wildcard、非空 = 「endpoint wildcard + cluster 指定」のパス列挙
    /// （priming 軽量化 — subscriptions.toml 由来）。
    async fn subscribe_wildcard(
        &mut self,
        clusters: &[u32],
    ) -> Result<(SubscriptionInfo, Vec<mat_controller::im::ReportDataMessage>), MatError>;
    /// 次のデバイス発 report を待つ（keep-alive は reports 空で返る）。
    /// 無音 `timeout` 経過は kind=Timeout。
    async fn next_report(
        &mut self,
        timeout: Duration,
    ) -> Result<mat_controller::im::ReportDataMessage, MatError>;
}

/// ノード宛の warm セッションを新規確立する手段（実 = mDNS+CASE、テスト = fake）。
#[async_trait]
pub trait Establisher: Send + Sync {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError>;
    /// 購読専用の transport + CASE を別に確立する（matd SubscriptionManager 用）。
    /// 既定は非対応 — 実確立器（CaseEstablisher）だけが上書きする。
    async fn establish_subscription(
        &self,
        _node_id: u64,
    ) -> Result<Box<dyn SubscribeConn>, MatError> {
        Err(MatError::new(
            ErrorKind::Other,
            "subscription not supported by this establisher",
        ))
    }
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

/// establish の mDNS 解決を差し替え可能にする抽象。`mat`（一発）は
/// [`OneShotResolver`]（キャッシュ無し＝設計ルール4）、`matd` は
/// `CachingResolver`（常駐キャッシュ、Task 5）を注入する。
#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(
        &self,
        scope_id: u32,
        cfid: [u8; 8],
        node_id: u64,
        timeout: Duration,
    ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError>;
}

/// 既定のリゾルバ: 一発 legacy multicast resolve を毎回実行する（キャッシュ
/// を持たない）。`mat` 一発直経路が使う。
pub struct OneShotResolver;

#[async_trait]
impl Resolver for OneShotResolver {
    async fn resolve(
        &self,
        scope_id: u32,
        cfid: [u8; 8],
        node_id: u64,
        timeout: Duration,
    ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError> {
        dnssd::resolve_operational(scope_id, &cfid, node_id, timeout).await
    }
}

/// matd 用リゾルバ: 常駐 mDNS キャッシュ（[`dnssd::OperationalCache`]）を参照し、
/// ヒットは即返し、ミス時は provoke してリスナの次アナウンスを
/// `CACHE_MISS_TIMEOUT` まで待つ。establish から渡される `timeout`（8s）ではなく
/// この内部定数を使う理由は spec 参照（`mat` 一発を無変更に保つため窓を分離）。
pub struct CachingResolver {
    cache: dnssd::OperationalCache,
}

/// cache miss 時にリスナの次アナウンス（周期~30s）を確実に跨ぐ待ち窓。
const CACHE_MISS_TIMEOUT: Duration = Duration::from_secs(35);
/// キャッシュ充填の poll 間隔（Notify を使わず単純 poll で取りこぼしを防ぐ）。
const CACHE_POLL: Duration = Duration::from_millis(500);

impl CachingResolver {
    pub fn new(cache: dnssd::OperationalCache) -> Self {
        Self { cache }
    }
}

#[async_trait]
impl Resolver for CachingResolver {
    async fn resolve(
        &self,
        _scope_id: u32,
        cfid: [u8; 8],
        node_id: u64,
        _timeout: Duration,
    ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError> {
        let instance = format!(
            "{}._matter._tcp.local",
            dnssd::operational_instance(&cfid, node_id)
        );
        if let Some(n) = self.cache.get(&instance) {
            return Ok(n);
        }
        // ミス: listener に provoke クエリを依頼し、次アナウンス/応答を待つ。
        self.cache.request(instance.clone());
        let deadline = tokio::time::Instant::now() + CACHE_MISS_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(CACHE_POLL).await;
            if let Some(n) = self.cache.get(&instance) {
                return Ok(n);
            }
        }
        Err(dnssd::DnssdError::Timeout { instance })
    }
}

impl Engine {
    /// KVS から資格情報を1回読み、NOC を自己発行し、UDP transport を bind、
    /// iface の scope_id を解決して実確立器を構築する。プロセス寿命で不変。
    pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError> {
        Self::build_with_resolver(cfg, Arc::new(OneShotResolver)).await
    }

    /// [`build`] と同じだが、establish の mDNS 解決に使う [`Resolver`] を注入する
    /// （matd が `CachingResolver` を渡す。`mat` 一発は `build` の OneShotResolver）。
    pub async fn build_with_resolver(
        cfg: &NativeConfig,
        resolver: Arc<dyn Resolver>,
    ) -> Result<Self, MatError> {
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
                ErrorKind::StoreParse,
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
            resolver,
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
    resolver: Arc<dyn Resolver>,
}

#[async_trait]
impl Establisher for CaseEstablisher {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
        let cfid = compressed_fabric_id(&self.creds.root_public_key, self.creds.fabric_id);
        let resolved = self
            .resolver
            .resolve(self.scope_id, cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| map_resolve_err(node_id, e))?;
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

    async fn establish_subscription(
        &self,
        node_id: u64,
    ) -> Result<Box<dyn SubscribeConn>, MatError> {
        // 購読専用ソケット: op 用の共有 transport と recv を奪い合わないよう、
        // ノードごとに専用 UdpTransport + 専用 CASE を確立する（spec 構造判断）。
        let transport = UdpTransport::bind().await.map_err(|e| {
            MatError::new(
                ErrorKind::Other,
                format!("native: bind subscription udp: {e}"),
            )
        })?;
        // 購読 socket の実ポートは実機切り分け（tcpdump / ss との突合）の鍵なので
        // 確立ごとに可視化する。
        let local = transport.local_addr().ok();
        let transport = Arc::new(Transport::Udp(Arc::new(transport)));
        let cfid = compressed_fabric_id(&self.creds.root_public_key, self.creds.fabric_id);
        let resolved = self
            .resolver
            .resolve(self.scope_id, cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| map_resolve_err(node_id, e))?;
        let mrp = resolved.mrp_config();
        let peers: Vec<SocketAddr> = resolved.socket_addrs(self.scope_id);
        let mut last: Option<MatError> = None;
        for peer in peers {
            match case::establish(Arc::clone(&transport), peer, &self.creds, node_id, &mrp).await {
                Ok(session) => {
                    tracing::info!(
                        node_id,
                        local = %local.map(|a| a.to_string()).unwrap_or_default(),
                        %peer,
                        "subscription transport bound (dedicated socket + CASE)"
                    );
                    return Ok(Box::new(SubscriptionSession { session, mrp }));
                }
                Err(e) => {
                    last = Some(MatError::new(
                        ErrorKind::SessionFailed,
                        format!("native: subscription CASE via {peer}: {e}"),
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

/// 購読専用の実セッション。
struct SubscriptionSession {
    session: mat_controller::session::SecureSession,
    mrp: MrpConfig,
}

#[async_trait]
impl SubscribeConn for SubscriptionSession {
    async fn subscribe_wildcard(
        &mut self,
        clusters: &[u32],
    ) -> Result<(SubscriptionInfo, Vec<mat_controller::im::ReportDataMessage>), MatError> {
        let (resp, priming) = self
            .session
            .subscribe_wildcard(
                SUBSCRIBE_MIN_INTERVAL_FLOOR_S,
                SUBSCRIBE_MAX_INTERVAL_CEILING_S,
                SUBSCRIBE_KEEP_SUBSCRIPTIONS,
                clusters,
                &self.mrp,
            )
            .await
            .map_err(map_session_err)?;
        Ok((
            SubscriptionInfo {
                subscription_id: resp.subscription_id,
                max_interval_s: resp.max_interval_s,
            },
            priming,
        ))
    }

    async fn next_report(
        &mut self,
        timeout: Duration,
    ) -> Result<mat_controller::im::ReportDataMessage, MatError> {
        self.session
            .next_subscription_report(timeout, &self.mrp)
            .await
            .map_err(map_session_err)
    }
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

/// operational mDNS resolve のエラーを mat の ErrorKind へ写像する。
/// Timeout は「窓内に広告が取れなかっただけ」（OTBR proxy の ~30s 周期広告は
/// リトライで跨げば通ることが多い）→ `timeout`(exit 3)。それ以外
/// （socket I/O 等の構造的失敗）→ `unreachable`(exit 5)。mat 直経路と matd
/// （常駐キャッシュのミス）は同じ establish を通るので分類は経路で割れない。
fn map_resolve_err(node_id: u64, e: dnssd::DnssdError) -> MatError {
    let kind = match e {
        dnssd::DnssdError::Timeout { .. } => ErrorKind::Timeout,
        // 非 timeout は構造的失敗 → unreachable。variant 追加時にここで分類を
        // 決めさせるため wildcard にしない。
        dnssd::DnssdError::Io(_) | dnssd::DnssdError::Malformed(_) => ErrorKind::Unreachable,
    };
    MatError::new(kind, format!("native: mDNS resolve node {node_id}: {e}"))
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
    fn resolve_timeout_maps_to_timeout_kind() {
        // resolve timeout は「時間内に広告が取れなかっただけ」（OTBR proxy の
        // ~30s 周期広告はリトライで跨げば通ることが多い）→ timeout(exit 3)。
        // socket I/O 等の構造的失敗は unreachable(exit 5) のまま。
        use mat_controller::dnssd::DnssdError;
        let e = map_resolve_err(
            5,
            DnssdError::Timeout {
                instance: "x".into(),
            },
        );
        assert_eq!(e.kind, ErrorKind::Timeout);
        assert!(e.detail.contains("node 5"), "detail: {}", e.detail);
        let e = map_resolve_err(5, DnssdError::Io(std::io::Error::other("boom")));
        assert_eq!(e.kind, ErrorKind::Unreachable);
        let e = map_resolve_err(5, DnssdError::Malformed("bad"));
        assert_eq!(e.kind, ErrorKind::Unreachable);
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
            matches!(
                err.kind,
                ErrorKind::StoreMissing | ErrorKind::StoreParse | ErrorKind::Other
            ),
            "unexpected kind: {:?}",
            err.kind
        );
    }

    /// resolve が実際に multicast 送受信できる iface の index を1つ探す。
    /// `crate::iface_select`（M8c-3 iface 自動検出）と同じ適格条件 — up・
    /// MULTICAST・非 loopback・非 POINTOPOINT・IPv6 link-local 保有 — を使う
    /// が、こちらは複数候補でも先頭を採用する（本番の autodetect は曖昧なら
    /// ハードエラーだが、このテストは delegation の検証に使える iface が
    /// 1つあれば十分）。単純に `flags`/`lo` だけで判定すると、この sandbox
    /// のような環境で `docker0` / `loopback0`（`lo` とは別名の仮想 NIC）/
    /// `tailscale0` を拾って `bind_mdns_socket` の send が `ENETUNREACH` で
    /// 即死し、意図した Timeout 経路を検証できなくなる。
    fn multicast_capable_iface_index() -> Option<u32> {
        const IFF_UP: u32 = 0x1;
        const IFF_LOOPBACK: u32 = 0x8;
        const IFF_POINTOPOINT: u32 = 0x10;
        const IFF_MULTICAST: u32 = 0x1000;
        let mut ll_names = std::collections::HashSet::new();
        for line in std::fs::read_to_string("/proc/net/if_inet6").ok()?.lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 6 && cols[3] == "20" {
                ll_names.insert(cols[5].to_string());
            }
        }
        let mut entries: Vec<_> = std::fs::read_dir("/sys/class/net")
            .ok()?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !ll_names.contains(&name) {
                continue;
            }
            let base = entry.path();
            let flags = std::fs::read_to_string(base.join("flags"))
                .ok()
                .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
            let operstate_up = std::fs::read_to_string(base.join("operstate"))
                .map(|s| s.trim() == "up")
                .unwrap_or(false);
            let eligible = operstate_up
                && flags & IFF_UP != 0
                && flags & IFF_MULTICAST != 0
                && flags & IFF_LOOPBACK == 0
                && flags & IFF_POINTOPOINT == 0;
            if !eligible {
                continue;
            }
            if let Ok(idx) = std::fs::read_to_string(base.join("ifindex"))
                .unwrap_or_default()
                .trim()
                .parse::<u32>()
            {
                return Some(idx);
            }
        }
        None
    }

    #[tokio::test]
    async fn oneshot_resolver_times_out_without_responder() {
        // 応答者のいない iface で resolve すると Timeout（委譲先
        // resolve_operational の契約）。無応答→Timeout は不変。
        let Some(scope) = multicast_capable_iface_index() else {
            eprintln!(
                "skipping oneshot_resolver test: no eligible multicast-capable IPv6 interface"
            );
            return;
        };
        let r = OneShotResolver;
        let out = r
            .resolve(scope, [0u8; 8], 5, std::time::Duration::from_millis(300))
            .await;
        assert!(matches!(
            out,
            Err(mat_controller::dnssd::DnssdError::Timeout { .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn caching_resolver_returns_cached_hit_immediately() {
        use mat_controller::dnssd;
        let (cache, _rx) = dnssd::OperationalCache::new();
        let inst = dnssd::operational_instance(&[0xAB; 8], 5) + "._matter._tcp.local";
        cache.insert(
            inst,
            dnssd::ResolvedNode {
                port: 5540,
                addresses: vec!["fd00::1".parse().unwrap()],
                session_idle_interval_ms: None,
                session_active_interval_ms: None,
            },
            std::time::Duration::from_secs(60),
        );
        let r = CachingResolver::new(cache);
        let n = r
            .resolve(1, [0xAB; 8], 5, std::time::Duration::from_secs(8))
            .await
            .expect("hit");
        assert_eq!(n.port, 5540);
    }

    #[tokio::test(start_paused = true)]
    async fn caching_resolver_awaits_listener_fill_then_returns() {
        use mat_controller::dnssd;
        let (cache, mut rx) = dnssd::OperationalCache::new();
        let inst = dnssd::operational_instance(&[0xAB; 8], 7) + "._matter._tcp.local";
        let filler = cache.clone();
        let inst2 = inst.clone();
        // 別タスクが少し後に埋める（リスナ相当）。
        tokio::spawn(async move {
            // provoke request が届くはず。
            let got = rx.recv().await.unwrap();
            assert_eq!(got, inst2);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            filler.insert(
                inst2,
                dnssd::ResolvedNode {
                    port: 5541,
                    addresses: vec!["fd00::2".parse().unwrap()],
                    session_idle_interval_ms: None,
                    session_active_interval_ms: None,
                },
                std::time::Duration::from_secs(60),
            );
        });
        let r = CachingResolver::new(cache);
        let n = r
            .resolve(1, [0xAB; 8], 7, std::time::Duration::from_secs(8))
            .await
            .expect("fill");
        assert_eq!(n.port, 5541);
    }

    #[tokio::test(start_paused = true)]
    async fn caching_resolver_times_out_when_never_filled() {
        use mat_controller::dnssd;
        let (cache, _rx) = dnssd::OperationalCache::new();
        let r = CachingResolver::new(cache);
        let out = r
            .resolve(1, [0xAB; 8], 9, std::time::Duration::from_secs(8))
            .await;
        assert!(matches!(out, Err(dnssd::DnssdError::Timeout { .. })));
    }

    #[tokio::test]
    async fn default_establisher_rejects_subscription() {
        // Establisher trait の default 実装は購読非対応（CaseEstablisher だけが上書き）。
        struct NoSub;
        #[async_trait]
        impl Establisher for NoSub {
            async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
                Err(MatError::new(ErrorKind::Other, "unused"))
            }
        }
        // `.unwrap_err()` would require `Box<dyn SubscribeConn>: Debug`, which
        // `SubscribeConn` deliberately doesn't require (mirrors `Engine`'s
        // manual, secret-hiding `Debug` — see its impl above): match instead.
        let err = match NoSub.establish_subscription(1).await {
            Err(e) => e,
            Ok(_) => panic!("default establish_subscription must reject"),
        };
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.contains("subscription"));
    }

    #[tokio::test]
    async fn fake_establisher_serves_scripted_subscription() {
        use crate::test_support::{FakeEstablisher, FakeSubConn};
        let est = FakeEstablisher::default();
        let mut conn = est.establish_subscription(5).await.unwrap();
        let (info, priming) = conn.subscribe_wildcard(&[]).await.unwrap();
        assert_eq!(info.max_interval_s, 60);
        assert_eq!(priming.len(), 1); // default fake は onoff=true の priming 1 チャンク
                                      // scripted report が尽きたら next_report は timeout で Err(Timeout)。
        let err = conn
            .next_report(std::time::Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Timeout);
        let _ = FakeSubConn::default(); // 型が公開されていること
    }
}
