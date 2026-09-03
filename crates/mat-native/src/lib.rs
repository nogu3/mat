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
use mat_controller::transport::UdpTransport;
use mat_controller::{case, dnssd};
use mat_core::error::{ErrorKind, MatError};

pub mod commission;
pub mod group;
pub mod group_settings;
pub mod iface_select;
pub mod op;
pub mod ops;
pub mod runner;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

/// Thread iface の決定結果。明示（解決失敗=ハードエラー）と自動検出
/// （解決失敗=warn+劣化続行）で失敗時の規律が違う（spec 設計 3）。
#[derive(Debug, Clone)]
pub enum ThreadIfaceChoice {
    Explicit(String),
    Auto(String),
}

/// native バックエンドの起動設定。
pub struct NativeConfig {
    /// chip-tool KVS のあるディレクトリ（chip-tool の --storage-directory と同一）。
    pub store: std::path::PathBuf,
    /// mDNS scope に使う Thread mesh の iface 名。
    pub iface: String,
    /// groupcast の第 2 egress に使う Thread TUN iface 名（`None` = LAN 単独）。
    pub thread_iface: Option<ThreadIfaceChoice>,
    /// KVS fabric テーブルの index（本番機で 2、検証機で 1 のような使い分け）。
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
    /// セッションを手放す直前の後始末。CloseSession を best-effort 送信する
    /// （Issue #20: 放置セッションが FP300 系の常駐購読を黙殺する）。fake は
    /// 既定 no-op で足りるよう default 実装を持つ。
    async fn close(&mut self) {}
}

/// timed リクエストに使う既定タイムアウト（open-window 等の既存値と同じ 10 秒）。
const TIMED_REQUEST_MS: u16 = 10_000;

/// 値ツリー（`mat_core::ids::ScalarValue`）を 1 要素の TLV として `w` に書く。
/// List → TLV Array（属性 list の型。TLV List 0x17 は path 専用）、Struct →
/// TLV Struct（context tag = fieldId、呼び出し側で id 昇順整列済み）。
pub fn put_value(
    w: &mut mat_controller::tlv::Writer,
    tag: mat_controller::tlv::Tag,
    v: &mat_core::ids::ScalarValue,
) {
    use mat_controller::tlv::Tag;
    use mat_core::ids::ScalarValue as S;
    match v {
        S::Bool(b) => w.put_bool(tag, *b),
        S::UInt(n) => w.put_uint(tag, *n),
        S::Int(n) => w.put_int(tag, *n),
        S::F32(f) => w.put_f32(tag, *f),
        S::F64(f) => w.put_f64(tag, *f),
        S::Str(s) => w.put_str(tag, s),
        S::Bytes(b) => w.put_bytes(tag, b),
        S::Null => w.put_null(tag),
        S::List(items) => {
            w.start_array(tag);
            for item in items {
                put_value(w, Tag::Anonymous, item);
            }
            w.end_container();
        }
        S::Struct(fields) => {
            w.start_struct(tag);
            for (id, val) in fields {
                put_value(w, Tag::Context(*id), val);
            }
            w.end_container();
        }
    }
}

/// `ScalarValue` を Anonymous タグの単一 TLV 要素へ（`write_tlv`/
/// `write_attribute_tlv` に渡す形。呼び出し側がトップレベルタグを再付与する）。
pub fn scalar_to_tlv(v: &mat_core::ids::ScalarValue) -> Vec<u8> {
    let mut w = mat_controller::tlv::Writer::new();
    put_value(&mut w, mat_controller::tlv::Tag::Anonymous, v);
    w.finish()
}

/// invoke のコマンド引数（値ツリーの列）を CommandFields TLV へ。context tag は
/// 引数添字（0-based、`CmdDef::fields` の添字と一致 — `mat_core::ids` のコメント
/// 参照）。mat 直経路 (`native_direct`) / matd (`server::native_op`) の両方が使う
/// 共有ヘルパ（M8a Task10 で mat 側から移設・一本化）。
pub fn encode_command_fields(args: &[mat_core::ids::ScalarValue]) -> Vec<u8> {
    use mat_controller::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    for (i, v) in args.iter().enumerate() {
        put_value(&mut w, Tag::Context(i as u8), v);
    }
    w.end_container();
    w.finish()
}

/// `ScalarValue` → `ImValue`（スカラーのみ。container は `None`）。mat-core は
/// mat-controller に依存しない設計のため、両者を知る mat-native がここで橋渡しする。
pub fn scalar_to_im(v: &mat_core::ids::ScalarValue) -> Option<ImValue> {
    use mat_core::ids::ScalarValue as S;
    Some(match v {
        S::Bool(b) => ImValue::Bool(*b),
        S::UInt(n) => ImValue::Uint(*n),
        S::Int(n) => ImValue::Int(*n),
        S::F32(f) => ImValue::F32(*f),
        S::F64(f) => ImValue::F64(*f),
        S::Str(s) => ImValue::Utf8(s.clone()),
        S::Bytes(b) => ImValue::Bytes(b.clone()),
        S::Null => ImValue::Null,
        S::List(_) | S::Struct(_) => return None,
    })
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
    /// 次のデバイス発 report を待つ（keep-alive は reports 空の Some で返る）。
    /// `timeout` 内無音は `Ok(None)` — エラーではない（pump がスライスで刻んで
    /// 死活判定するための契約）。`Err` はセッション異常のみ。
    async fn next_report(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<mat_controller::im::ReportDataMessage>, MatError>;
    /// セッションを手放す直前の後始末。CloseSession を best-effort 送信する
    /// （Issue #20: 放置セッションが FP300 系の常駐購読を黙殺する）。fake は
    /// 既定 no-op で足りるよう default 実装を持つ。
    async fn close(&mut self) {}
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

/// mDNS 解決 timeout（`dnssd::OPERATIONAL_RESOLVE_TIMEOUT` の別名 — probe と
/// 共有、監査⑩）。SII が来ない場合でも過度に待たない上限。
const RESOLVE_TIMEOUT: Duration = dnssd::OPERATIONAL_RESOLVE_TIMEOUT;

/// thread iface 選択を egress 追加判断に写像する純関数（テスト対象）。
/// 戻り: `Ok(Some(name, scope_id))` = 第 2 egress を張る、`Ok(None)` = LAN
/// 単独、`Err(detail)` = ハードエラー（明示指定の解決失敗のみ — 自動検出の
/// 解決失敗は warn+劣化続行で `Ok(None)` に写像する、spec 設計 3）。
///
/// `op_iface`（`cfg.iface`、運用 iface）と thread iface 名が一致する場合は
/// 解決すら試みず `Ok(None)`（info ログのみ）— `MAT_IFACE=wpan0` かつ
/// wpan0 自動検出のように同一 iface へ二重に egress を張ってしまう構成の
/// 回避（監査 Minor-1）。explicit / auto どちらの由来でも同じ規律（同一
/// iface への二重送出は単に無駄で、ハードエラーにする理由がない）。
fn thread_egress_decision(
    op_iface: &str,
    choice: &Option<ThreadIfaceChoice>,
    resolve: impl Fn(&str) -> Result<u32, String>,
) -> Result<Option<(String, u32)>, String> {
    let (name, explicit) = match choice {
        None => return Ok(None),
        Some(ThreadIfaceChoice::Explicit(name)) => (name, true),
        Some(ThreadIfaceChoice::Auto(name)) => (name, false),
    };
    if name == op_iface {
        tracing::info!(iface = %name,
            "thread iface matches operating iface; skipping duplicate groupcast egress");
        return Ok(None);
    }
    match resolve(name) {
        Ok(idx) => Ok(Some((name.clone(), idx))),
        Err(e) if explicit => Err(format!(
            "native: resolve thread iface {name:?} index: {e} (explicit MAT_THREAD_IFACE must resolve)"
        )),
        Err(e) => {
            tracing::warn!(iface = %name, error = %e,
                "thread iface auto-detected but unresolvable; groupcast stays LAN-only");
            Ok(None)
        }
    }
}

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

/// cache miss 時にリスナの次アナウンス（周期~30s）を確実に跨ぐ待ち窓。op 予算設計の成分（Issue #16）。
pub const CACHE_MISS_TIMEOUT: Duration = Duration::from_secs(35);
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
    /// iface の scope_id を解決して実確立器を構築する。op (unicast) 側の
    /// scope_id はプロセス寿命で不変。group egress の scope_id は送信時に
    /// self-heal する (issue #23 — otbr 再起動で wpan0 の ifindex が変わる)。
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
        // establisher に creds を move する前に、group 送信に要る値を控える。
        let fabric_id = creds.fabric_id;
        let node_id = creds.node_id;
        let cfid = compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
        let group_settings = group_settings::GroupSettingsCtx {
            main_ini: main_ini.clone(),
            fabric_index: cfg.fabric_index,
            cfid,
        };
        let transport = Arc::new(transport);
        let mut egress = vec![mat_controller::group::GroupEgress {
            iface: cfg.iface.clone(),
            transport: Arc::clone(&transport),
            scope_id,
        }];
        match thread_egress_decision(&cfg.iface, &cfg.thread_iface, |n| {
            mat_controller::dnssd::iface_index(n).map_err(|e| e.to_string())
        }) {
            Ok(Some((name, tsid))) => {
                // Thread egress は専用 socket（LAN 側の IPV6_MULTICAST_IF と独立）。
                match UdpTransport::bind().await {
                    Ok(t) => {
                        tracing::info!(iface = %name, "groupcast thread egress enabled");
                        egress.push(mat_controller::group::GroupEgress {
                            iface: name,
                            transport: Arc::new(t),
                            scope_id: tsid,
                        });
                    }
                    Err(e) => match &cfg.thread_iface {
                        Some(ThreadIfaceChoice::Explicit(_)) => {
                            return Err(MatError::new(
                                ErrorKind::Other,
                                format!("native: bind thread egress socket: {e}"),
                            ));
                        }
                        _ => tracing::warn!(error = %e,
                            "thread egress socket bind failed; groupcast stays LAN-only"),
                    },
                }
            }
            Ok(None) => {}
            Err(detail) => return Err(MatError::new(ErrorKind::Other, detail)),
        }
        // thread egress を build 時に確立できなかった Auto/None 由来の構成は
        // 送信時再検出の対象にする (issue #23 起動順の罠)。Explicit は build
        // 時に確定 (解決失敗はハードエラー) なので対象外。
        let thread_retry =
            !matches!(&cfg.thread_iface, Some(ThreadIfaceChoice::Explicit(_))) && egress.len() == 1;
        let group = group::GroupCtx {
            main_ini,
            counter_path: cfg.store.join("native_group_counter"),
            fabric_index: cfg.fabric_index,
            fabric_id,
            node_id,
            egress,
            dest_port: MATTER_PORT,
            op_iface: cfg.iface.clone(),
            thread_retry,
            sender: tokio::sync::Mutex::new(None),
        };
        // build が bind する共有 UdpTransport は group multicast 送信専用。
        // op / 購読の unicast セッションはノードごとに専用ソケットを bind する
        // （監査#3 / 購読 spec）。
        let establisher = CaseEstablisher {
            creds: Arc::new(creds),
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

/// 実確立器: 保持した資格情報で mDNS 解決 → CASE。op セッションのソケットは
/// ノードごとに専用（監査#3）— 共有ソケットは group multicast 送信のみ。
struct CaseEstablisher {
    creds: Arc<FabricCredentials>,
    scope_id: u32,
    resolver: Arc<dyn Resolver>,
}

#[async_trait]
impl Establisher for CaseEstablisher {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
        // op 専用ソケット: 共有ソケットでは並行 op が他ノード宛の応答を
        // recv して screen で捨てる（監査#3）。試行ごとの専用 UdpTransport の
        // bind と候補アドレスの staggered race（Happy Eyeballs）は
        // `case::establish_any` が一括して行う。
        let cfid = compressed_fabric_id(&self.creds.root_public_key, self.creds.fabric_id);
        let resolved = self
            .resolver
            .resolve(self.scope_id, cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| map_resolve_err(node_id, e))?;
        let mrp = resolved.mrp_config();
        let peers: Vec<SocketAddr> = resolved.socket_addrs(self.scope_id);
        let est = case::establish_any(&peers, &self.creds, node_id, &mrp, case::RACE_STAGGER)
            .await
            .map_err(|e| map_establish_err(node_id, EstablishRole::Op, e))?;
        // local port は実機切り分け（ss -uanp / tcpdump 突合）の鍵なので
        // 確立ごとに可視化する（購読側の同名ログと対）。
        tracing::info!(
            node_id,
            local = %est.local.map(|a| a.to_string()).unwrap_or_default(),
            peer = %est.peer,
            "op transport bound (dedicated socket + CASE)"
        );
        Ok(Box::new(SessionConn {
            session: est.session,
            mrp,
        }))
    }

    async fn establish_subscription(
        &self,
        node_id: u64,
    ) -> Result<Box<dyn SubscribeConn>, MatError> {
        // 購読専用ソケット: op 用の transport と recv を奪い合わないよう、
        // ノードごとに専用 UdpTransport + 専用 CASE（spec 構造判断）。bind と
        // 候補レースは op 側と同じく `case::establish_any`。
        let cfid = compressed_fabric_id(&self.creds.root_public_key, self.creds.fabric_id);
        let resolved = self
            .resolver
            .resolve(self.scope_id, cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| map_resolve_err(node_id, e))?;
        let mrp = resolved.mrp_config();
        let peers: Vec<SocketAddr> = resolved.socket_addrs(self.scope_id);
        let est = case::establish_any(&peers, &self.creds, node_id, &mrp, case::RACE_STAGGER)
            .await
            .map_err(|e| map_establish_err(node_id, EstablishRole::Subscription, e))?;
        tracing::info!(
            node_id,
            local = %est.local.map(|a| a.to_string()).unwrap_or_default(),
            peer = %est.peer,
            "subscription transport bound (dedicated socket + CASE)"
        );
        Ok(Box::new(SubscriptionSession {
            session: est.session,
            mrp,
        }))
    }
}

/// `map_establish_err` の detail 前置き分岐（op / 購読でログ・detail の
/// 文言を従来どおり出し分ける）。
#[derive(Clone, Copy)]
enum EstablishRole {
    Op,
    Subscription,
}

/// `case::establish_any` の失敗を mat のエラー種別へ写す。種別の対応は
/// 逐次ループ時代と同じ: 候補ゼロ = unreachable、CASE 全滅 = session_failed、
/// bind 失敗 = other。detail は全候補のエラーを列挙する（旧実装は最後の
/// 1 本だけだった）。
fn map_establish_err(node_id: u64, role: EstablishRole, e: case::EstablishAnyError) -> MatError {
    use case::EstablishAnyError as E;
    let (bind_role, fail_prefix) = match role {
        EstablishRole::Op => ("op", ""),
        EstablishRole::Subscription => ("subscription", "subscription "),
    };
    match &e {
        E::NoAddresses => MatError::new(
            ErrorKind::Unreachable,
            format!("native: no addresses resolved for node {node_id}"),
        ),
        E::Bind(err) => MatError::new(
            ErrorKind::Other,
            format!("native: bind {bind_role} udp: {err}"),
        ),
        E::AllFailed(_) => MatError::new(
            ErrorKind::SessionFailed,
            format!("native: {fail_prefix}{e}"),
        ),
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
    ) -> Result<Option<mat_controller::im::ReportDataMessage>, MatError> {
        match self
            .session
            .next_subscription_report(timeout, &self.mrp)
            .await
        {
            Ok(msg) => Ok(Some(msg)),
            Err(mat_controller::session::SessionError::Silence) => Ok(None),
            Err(e) => Err(map_session_err(e)),
        }
    }

    async fn close(&mut self) {
        self.session.send_close_session().await;
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

    async fn close(&mut self) {
        self.session.send_close_session().await;
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
    use mat_controller::im::ImError;
    use mat_controller::session::SessionError;
    match e {
        // MRP 再送尽き。session が死んでいる兆候 → 上位が1回だけ再確立を試みる。
        SessionError::Timeout => MatError::new(ErrorKind::Timeout, format!("native: {e}")),
        // 購読の無音 deadline 切れ。通常は SubscriptionSession::next_report が
        // Ok(None) に写像するのでここへは来ないが、防御的に Timeout kind へ。
        SessionError::Silence => MatError::new(ErrorKind::Timeout, format!("native: {e}")),
        // デバイスがコマンド/読みを IM ステータスで拒否 → コマンドは届いた。
        // デコード失敗（Tlv/Malformed/UnsupportedValue）は「応答は来たが解釈
        // 不能」= parse_error（Message(_) と同じ規律）。内側 match は wildcard
        // なしの全 variant 列挙 — ImError の variant 追加時にここがコンパイル
        // エラーになり分類を決めさせる（外側の `_` に黙って落とさない）。
        SessionError::Im(ref im) => {
            let kind = match im {
                ImError::StatusResponse(_)
                | ImError::AttributeStatus(_)
                | ImError::CommandStatus { .. } => ErrorKind::DeviceRejected,
                ImError::Tlv(_) | ImError::Malformed(_) | ImError::UnsupportedValue => {
                    ErrorKind::ParseError
                }
            };
            MatError::new(kind, format!("native: {e}"))
        }
        SessionError::Io(_) => MatError::new(ErrorKind::Unreachable, format!("native: {e}")),
        // ピアの応答がメッセージ層で壊れている → 応答は来た（不達ではない）が
        // 解釈不能 = parse_error（v1 品質修正 4）。
        SessionError::Message(_) => MatError::new(ErrorKind::ParseError, format!("native: {e}")),
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
        CommissionError::InvalidArgument { .. } => {
            MatError::new(ErrorKind::ParseError, format!("native: {e}"))
        }
        _ => MatError::new(ErrorKind::Other, format!("native: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_egress_explicit_failure_is_hard_error() {
        let r = thread_egress_decision(
            "eth0",
            &Some(ThreadIfaceChoice::Explicit("wpan9".into())),
            |_| Err("no such iface".into()),
        );
        assert!(r.is_err());
    }

    #[test]
    fn thread_egress_auto_failure_degrades_to_lan_only() {
        let r = thread_egress_decision(
            "eth0",
            &Some(ThreadIfaceChoice::Auto("wpan0".into())),
            |_| Err("no such iface".into()),
        );
        assert_eq!(r.unwrap(), None);
    }

    #[test]
    fn thread_egress_resolved_returns_scope() {
        let r = thread_egress_decision(
            "eth0",
            &Some(ThreadIfaceChoice::Auto("wpan0".into())),
            |_| Ok(7),
        );
        assert_eq!(r.unwrap(), Some(("wpan0".into(), 7)));
    }

    #[test]
    fn thread_egress_none_is_lan_only() {
        let r = thread_egress_decision("eth0", &None, |_| unreachable!());
        assert_eq!(r.unwrap(), None);
    }

    /// 監査 Minor-1: 運用 iface（`cfg.iface`）と thread iface が同名なら
    /// `resolve` すら呼ばず第 2 egress を張らない（`MAT_IFACE=wpan0` +
    /// wpan0 自動検出の同一 iface 二重送出を回避）。`resolve` を
    /// `unreachable!()` にして「呼ばれないこと」自体を固定する。
    #[test]
    fn thread_egress_same_as_op_iface_is_skipped_without_resolving_auto() {
        let r = thread_egress_decision(
            "wpan0",
            &Some(ThreadIfaceChoice::Auto("wpan0".into())),
            |_| unreachable!("resolve must not be called when thread iface == op iface"),
        );
        assert_eq!(r.unwrap(), None);
    }

    /// 同上、explicit 指定でも同じ規律（同一 iface はハードエラーにせず
    /// 単に第 2 egress を張らない）。
    #[test]
    fn thread_egress_same_as_op_iface_is_skipped_without_resolving_explicit() {
        let r = thread_egress_decision(
            "wpan0",
            &Some(ThreadIfaceChoice::Explicit("wpan0".into())),
            |_| unreachable!("resolve must not be called when thread iface == op iface"),
        );
        assert_eq!(r.unwrap(), None);
    }

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
    fn cache_miss_timeout_is_pinned() {
        // Issue #16: op 予算設計（最悪 45〜60s の導出成分）の釘打ち。
        assert_eq!(CACHE_MISS_TIMEOUT.as_secs(), 35);
    }

    #[test]
    fn map_session_err_maps_malformed_message_to_parse_error() {
        // v1 品質修正 4: ピアの壊れた応答（Message 層のパース失敗）は「応答は来た
        // が解釈不能」= `parse_error`。旧実装は catch-all で `other` に落ちていた。
        let e = map_session_err(mat_controller::session::SessionError::Message(
            mat_controller::message::MessageError::Truncated,
        ));
        assert_eq!(e.kind, ErrorKind::ParseError);
    }

    #[test]
    fn map_session_err_splits_im_decode_failure_from_device_rejection() {
        // 監査⑨: デコード失敗（Tlv/Malformed/UnsupportedValue）は「応答は来たが
        // 解釈不能」= parse_error（Message(_) と同じ規律）。device_rejected は
        // 本当のデバイス拒否（StatusResponse/AttributeStatus/CommandStatus）だけ。
        use mat_controller::im::ImError;
        use mat_controller::session::SessionError;
        let e = map_session_err(SessionError::Im(ImError::Malformed(
            "truncated report data",
        )));
        assert_eq!(e.kind, ErrorKind::ParseError);
        let e = map_session_err(SessionError::Im(ImError::UnsupportedValue));
        assert_eq!(e.kind, ErrorKind::ParseError);
        let e = map_session_err(SessionError::Im(ImError::Tlv(
            mat_controller::tlv::TlvError::InvalidType(0xFF),
        )));
        assert_eq!(e.kind, ErrorKind::ParseError);
        let e = map_session_err(SessionError::Im(ImError::StatusResponse(0x80)));
        assert_eq!(e.kind, ErrorKind::DeviceRejected);
        let e = map_session_err(SessionError::Im(ImError::AttributeStatus(0x86)));
        assert_eq!(e.kind, ErrorKind::DeviceRejected);
        let e = map_session_err(SessionError::Im(ImError::CommandStatus {
            status: 0x01,
            cluster_status: None,
        }));
        assert_eq!(e.kind, ErrorKind::DeviceRejected);
    }

    #[test]
    fn invalid_argument_maps_to_parse_error() {
        let e = map_commission_err(
            mat_controller::commissioning::CommissionError::InvalidArgument {
                what: "iterations must be in 1000..=100000",
            },
        );
        assert_eq!(e.kind, ErrorKind::ParseError);
    }

    #[test]
    fn scalar_conversions() {
        use mat_controller::im::ImValue;
        use mat_core::ids::ScalarValue as S;
        assert_eq!(scalar_to_im(&S::Bool(true)), Some(ImValue::Bool(true)));
        assert_eq!(scalar_to_im(&S::UInt(7)), Some(ImValue::Uint(7)));
        // scalar_to_tlv は Reader で読み戻して値一致を確認。
        let b = scalar_to_tlv(&S::Str("x".into()));
        let mut r = mat_controller::tlv::Reader::new(&b);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            mat_controller::tlv::Value::Utf8("x")
        ));

        let b = scalar_to_tlv(&S::F64(0.5));
        let mut r = mat_controller::tlv::Reader::new(&b);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            mat_controller::tlv::Value::F64(f) if f == 0.5
        ));

        assert_eq!(scalar_to_im(&S::List(vec![])), None);

        // write 経路の float 要素型: single = 0x0A, double = 0x0B（anonymous tag → control byte のみ）。
        assert_eq!(scalar_to_tlv(&S::F32(1.5))[0] & 0x1F, 0x0A);
        assert_eq!(scalar_to_tlv(&S::F64(1.5))[0] & 0x1F, 0x0B);
    }

    #[test]
    fn put_value_encodes_list_of_struct_as_tlv_array_and_roundtrips_to_read_json() {
        use mat_core::ids::{parse_value_typed, resolve_attribute};
        let ty = resolve_attribute(0x001F, "acl").unwrap().def.unwrap().ty;
        let v = parse_value_typed(
            r#"[{"privilege":5,"auth-mode":2,"subjects":[112233],"targets":null,"fabric-index":1}]"#,
            &ty,
        )
        .unwrap();
        let tlv = scalar_to_tlv(&v);
        // 先頭要素は TLV Array（0x16、anonymous）。
        assert_eq!(tlv[0], 0x16);
        // read 側の JSON 化（番号キー）に戻ると同じ内容。
        let j = mat_controller::im::tlv_to_json(&tlv).unwrap();
        assert_eq!(
            j,
            serde_json::json!([{"1":5,"2":2,"3":[112233],"4":null,"254":1}])
        );
    }

    #[test]
    fn generic_acl_encoding_matches_dedicated_encoder() {
        use mat_core::acl::{AclEntry, AclTarget};
        use mat_core::ids::{parse_value_typed, resolve_attribute};
        let entries = vec![
            AclEntry {
                privilege: 5,
                auth_mode: 2,
                subjects: vec![112233, 0x1122],
                targets: None,
                fabric_index: 1,
            },
            AclEntry {
                privilege: 3,
                auth_mode: 3,
                subjects: vec![0xFFFF_FFFF_FFFF_0001],
                targets: Some(vec![AclTarget {
                    cluster: Some(6),
                    endpoint: None,
                    device_type: None,
                }]),
                fabric_index: 1,
            },
        ];
        let dedicated = crate::ops::encode_acl_entries_tlv(&entries);
        let ty = resolve_attribute(0x001F, "acl").unwrap().def.unwrap().ty;
        let generic = scalar_to_tlv(
            &parse_value_typed(
                r#"[
                  {"privilege":5,"auth-mode":2,"subjects":[112233,4386],"targets":null,"fabric-index":1},
                  {"privilege":3,"auth-mode":3,"subjects":["0xFFFFFFFFFFFF0001"],
                   "targets":[{"cluster":6,"endpoint":null,"device-type":null}],"fabric-index":1}
                ]"#,
                &ty,
            )
            .unwrap(),
        );
        assert_eq!(generic, dedicated);
    }

    #[test]
    fn generic_group_key_map_encoding_matches_dedicated_encoder() {
        use mat_core::ids::{parse_value_typed, resolve_attribute};
        let dedicated = mat_controller::im::encode_group_key_map_tlv(&[(1, 2), (0x0101, 7)]);
        let ty = resolve_attribute(0x003F, "group-key-map")
            .unwrap()
            .def
            .unwrap()
            .ty;
        let generic = scalar_to_tlv(
            &parse_value_typed(
                r#"[{"group-id":1,"group-key-set-id":2},{"1":257,"2":7}]"#,
                &ty,
            )
            .unwrap(),
        );
        assert_eq!(generic, dedicated);
    }

    #[test]
    fn generic_key_set_write_encoding_matches_dedicated_encoder() {
        use mat_core::ids::{classify_invoke, InvokeClass};
        let key: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let dedicated = mat_controller::im::encode_key_set_write_fields(1, &key);
        let j = r#"{"group-key-set-id":1,"group-key-security-policy":0,"epoch-key0":"hex:00112233445566778899aabbccddeeff","epoch-start-time0":1,"epoch-key1":null,"epoch-start-time1":null,"epoch-key2":null,"epoch-start-time2":null}"#;
        let InvokeClass::Native { fields, .. } =
            classify_invoke("groupkeymanagement", "key-set-write", &[j.into()])
        else {
            panic!("expected Native");
        };
        assert_eq!(encode_command_fields(&fields), dedicated);
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
            thread_iface: None,
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
                                      // scripted report が尽きたら next_report は timeout まで待って Ok(None)（無音）。
        let silent = conn
            .next_report(std::time::Duration::from_millis(50))
            .await
            .unwrap();
        assert!(silent.is_none());
        // 共有 live キューに積めば次の next_report が払い出す。
        est.sub_live
            .lock()
            .unwrap()
            .push_back(crate::test_support::onoff_report(1, false));
        let msg = conn
            .next_report(std::time::Duration::from_millis(50))
            .await
            .unwrap()
            .expect("live report");
        assert_eq!(msg.reports.len(), 1);
        let _ = FakeSubConn::default(); // 型が公開されていること
    }

    /// fake の失敗カウンタ: 残り回数だけ失敗し、尽きたら成功する
    /// （matd の再確立ラダーを回すための足場）。既定 0 = 常に成功なので
    /// 既存テストの挙動は変わらない。
    #[tokio::test]
    async fn fake_establisher_fails_subscription_n_times_then_succeeds() {
        use crate::test_support::FakeEstablisher;
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        est.fail_subscription.store(2, Ordering::SeqCst);
        for attempt in 1..=2 {
            let err = match est.establish_subscription(5).await {
                Err(e) => e,
                Ok(_) => panic!("attempt {attempt} は失敗するはず"),
            };
            assert_eq!(err.kind, ErrorKind::Timeout, "既定 fail_kind を使う");
        }
        assert!(
            est.establish_subscription(5).await.is_ok(),
            "カウンタが尽きたら成功する"
        );
        // 失敗も試行として数える（matd 側テストが calls で試行回数を主張できる）。
        assert_eq!(est.calls.load(Ordering::SeqCst), 3);
    }

    /// 確立の**あと**に注入した fail_next_report が pump 側（FakeSubConn）へ効く。
    /// Arc 共有でないとこの順序が表現できない。
    #[tokio::test]
    async fn fake_sub_conn_next_report_fails_when_injected_after_establish() {
        use crate::test_support::FakeEstablisher;
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let mut conn = est.establish_subscription(5).await.unwrap();
        conn.subscribe_wildcard(&[]).await.unwrap();

        est.fail_next_report.store(1, Ordering::SeqCst);
        let err = match conn.next_report(std::time::Duration::from_millis(50)).await {
            Err(e) => e,
            Ok(_) => panic!("注入した 1 回は Err になるはず"),
        };
        assert_eq!(err.kind, ErrorKind::SessionFailed);
        // 尽きたら従来どおり無音 Ok(None)。
        assert!(conn
            .next_report(std::time::Duration::from_millis(50))
            .await
            .unwrap()
            .is_none());
    }
}

#[cfg(test)]
mod dedicated_op_socket_tests {
    use super::*;
    use mat_controller::cert::MatterCert;
    use mat_controller::kvs::SelfIssueMaterials;
    use mat_controller::test_support as case_ts;
    use mat_controller::transport::UdpTransport;
    use std::net::Ipv6Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 呼び出し順に固定ポートを払い出す fake resolver。2 応答器が同一
    /// fixture 識別（同一 node_id）なので、どちらの establish がどちらの
    /// 応答器に着いても対称で問題ない。
    struct FixedPortResolver {
        ports: Vec<u16>,
        next: AtomicUsize,
    }

    #[async_trait]
    impl Resolver for FixedPortResolver {
        async fn resolve(
            &self,
            _scope_id: u32,
            _cfid: [u8; 8],
            _node_id: u64,
            _timeout: Duration,
        ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError> {
            let i = self.next.fetch_add(1, Ordering::SeqCst);
            Ok(dnssd::ResolvedNode {
                port: self.ports[i],
                addresses: vec![Ipv6Addr::LOCALHOST],
                session_idle_interval_ms: Some(50),
                session_active_interval_ms: Some(50),
            })
        }
    }

    /// 監査#3 の釘打ち: 異なるノードへの並行 op が互いの応答を吸わない。
    /// ループバックに CASE 応答器を 2 つ立て、並行 establish + read が両方
    /// 成功し、応答器の観測した initiator ソースポートが異なる（= ノード
    /// ごとの専用ソケット）ことを assert する。共有ソケットに退行すると
    /// ポートが一致して確実に落ちる。
    #[tokio::test]
    async fn concurrent_establishes_use_dedicated_sockets() {
        let noc = MatterCert::parse(case_ts::NODE01_NOC).expect("parse fixture NOC");
        let responder_node_id = noc.node_id().expect("node id");
        let fabric_id = noc.fabric_id().expect("fabric id");
        let op_priv: [u8; 32] = case_ts::NODE01_PRIV.try_into().unwrap();

        // 応答器 2 つ（同一識別・別ポート）。
        let mut handles = Vec::new();
        let mut ports = Vec::new();
        for _ in 0..2 {
            let t = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap();
            ports.push(t.local_addr().unwrap().port());
            handles.push(tokio::spawn(case_ts::responder_task(
                t,
                case_ts::INITIATOR_NODE_ID,
                responder_node_id,
                case_ts::NODE01_NOC.to_vec(),
                case_ts::ICA01.to_vec(),
                op_priv,
                case_ts::ROOT01_CHIP.to_vec(),
            )));
        }

        let materials = SelfIssueMaterials {
            rcac: case_ts::ROOT01_CHIP.to_vec(),
            root_private_key: case_ts::ROOT01_PRIV.try_into().unwrap(),
            ipk_operational: case_ts::IPK,
            node_id: case_ts::INITIATOR_NODE_ID,
            fabric_id,
        };
        let creds = FabricCredentials::from_self_issued(materials).expect("creds");
        let est = CaseEstablisher {
            creds: Arc::new(creds),
            scope_id: 0,
            resolver: Arc::new(FixedPortResolver {
                ports,
                next: AtomicUsize::new(0),
            }),
        };

        let (a, b) = tokio::join!(
            est.establish(responder_node_id),
            est.establish(responder_node_id)
        );
        let mut a = a.expect("establish 1");
        let mut b = b.expect("establish 2");
        let (ra, rb) = tokio::join!(a.read_onoff(1), b.read_onoff(1));
        // 応答器は on-off=false を返す（clippy: bool_assert_comparison を避け assert! で）。
        assert!(!ra.expect("read 1"));
        assert!(!rb.expect("read 2"));

        let sa = handles.pop().unwrap().await.expect("responder 2");
        let sb = handles.pop().unwrap().await.expect("responder 1");
        assert_ne!(
            sa.port(),
            sb.port(),
            "op sockets must be dedicated per establish (audit #3)"
        );
    }
}
