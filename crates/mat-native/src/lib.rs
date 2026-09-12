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
mod errmap;
pub mod group;
pub mod group_settings;
pub mod iface_select;
pub mod op;
pub mod ops;
pub mod resolver;
pub mod rotate_ipk;
pub mod runner;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use op::{arg_value_to_tlv, encode_command_fields, put_value};
pub use resolver::{CachingResolver, OneShotResolver, Resolver, CACHE_MISS_TIMEOUT};

use errmap::{
    map_commission_err, map_establish_err, map_resolve_err, map_session_err, EstablishRole,
};

/// Thread iface の決定結果。明示（解決失敗=ハードエラー）と自動検出
/// （解決失敗=warn+劣化続行）で失敗時の規律が違う（spec 設計 3）。
#[derive(Debug, Clone)]
pub enum ThreadIfaceChoice {
    Explicit(String),
    Auto(String),
}

/// native バックエンドの起動設定。
#[derive(Clone)]
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
    /// データ応答（CommandDataIB）を持つコマンドの invoke（NOCResponse /
    /// RemoveGroupResponse 等）。応答の CommandFields TLV を返す（status-only
    /// 応答は空 Vec）。IM status ≠ 0 は `invoke` と同じく Err。
    async fn invoke_for_data(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
        timed: bool,
    ) -> Result<Vec<u8>, MatError>;
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

/// `SubscribeConn::subscribe` の戻り: 成立情報 + priming の属性チャンク列 +
/// priming イベント（型が長いだけの組 — clippy::type_complexity 対策の別名）。
pub type SubscribeStart = (
    SubscriptionInfo,
    Vec<mat_controller::im::ReportDataMessage>,
    Vec<mat_controller::im::EventReport>,
);

/// 購読専用コネクション（専用 UdpTransport + 専用 CASE をポンプが独占する。
/// 既存 op 経路 = warm session は不変 — spec 構造判断）。
#[async_trait]
pub trait SubscribeConn: Send {
    /// Subscribe を張り、成立情報と priming（属性チャンク列 + イベント）を
    /// 返す。`clusters` 空 = full wildcard、非空 = 「endpoint wildcard +
    /// cluster 指定」のパス列挙（priming 軽量化 — subscriptions.toml 由来）。
    /// `event_paths` 空かつ `event_min` None なら EventRequests /
    /// EventFilters を出さず、ワイヤは従来の属性のみ購読と byte-equal
    /// （フェーズ A で釘打ち済み）。`event_min` は再購読時の盲目窓回収
    /// （EventFilters の EventMin = 前回見た番号 + 1）。
    async fn subscribe(
        &mut self,
        clusters: &[u32],
        event_paths: &[mat_controller::im::EventPathIn],
        event_min: Option<u64>,
    ) -> Result<SubscribeStart, MatError>;
    /// 次のデバイス発 report を属性 + イベントの両方で待つ（keep-alive は
    /// 両方空の Some で返る）。`timeout` 内無音は `Ok(None)` — エラーでは
    /// ない（pump がスライスで刻んで死活判定するための契約）。`Err` は
    /// セッション異常のみ。
    async fn next_report_full(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<mat_controller::session::SubscriptionReport>, MatError>;
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
    /// 資格情報を KVS から読み直して差し替える（IPK ローテーション後の
    /// `matd reload`）。戻り値は `ipk_operational` が変わったか。既定は非対応 —
    /// 実確立器（CaseEstablisher）だけが上書きする。同期メソッド（I/O は
    /// KVS の 1 回読みだけ、ネットワークには触れない）。
    fn reload_credentials(&self) -> Result<bool, MatError> {
        Err(MatError::new(
            ErrorKind::Other,
            "credential reload not supported by this establisher",
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

/// chip-tool 互換 KVS の alpha INI 名。ctrl-store レーンが
/// `mat_controller::kvs::ALPHA_INI_FILE` を足したらそちらへ差し替える。
pub const ALPHA_INI_FILE: &str = "chip_tool_config.alpha.ini";

/// KVS から自己発行資材（root CA 鍵・fabric id・node id）を読む。`Engine::build`
/// / `commission` / `mat` の probe が同じ 1 本を通る。読めない = fabric 未
/// bootstrap → `store_missing`（`mat fabric init` 誘導付き）。
pub fn load_self_issue_materials(
    cfg: &NativeConfig,
) -> Result<mat_controller::kvs::SelfIssueMaterials, MatError> {
    let alpha_ini = cfg.store.join(ALPHA_INI_FILE);
    let main_ini = cfg.store.join(mat_controller::kvs::MAIN_INI_FILE);
    mat_controller::kvs::read_self_issue_materials(
        &alpha_ini,
        &main_ini,
        cfg.fabric_index,
        cfg.issuer_index,
    )
    .map_err(|e| {
        MatError::new(
            ErrorKind::StoreMissing,
            format!("native: read KVS credentials: {e} — run `mat fabric init`"),
        )
    })
}

/// 資材から NOC を自己発行して `FabricCredentials` を組む。資材はあるが
/// NOC を組めない = 壊れた / 不整合な store → `store_parse`。
pub fn self_issue_credentials(
    materials: mat_controller::kvs::SelfIssueMaterials,
) -> Result<FabricCredentials, MatError> {
    FabricCredentials::from_self_issued(materials).map_err(|e| {
        MatError::new(
            ErrorKind::StoreParse,
            format!("native: self-issue NOC: {e} — run `mat fabric init`"),
        )
    })
}

/// KVS から fabric 資格情報を組み立てる（`Engine::build` の前半）。`fabric
/// rotate-ipk` も同じ経路で読む（epoch を差し替えた別 IPK の確立器を作るため）。
pub fn load_fabric_credentials(cfg: &NativeConfig) -> Result<FabricCredentials, MatError> {
    self_issue_credentials(load_self_issue_materials(cfg)?)
}

/// 運用 iface（`cfg.iface`）の scope_id（ifindex）。解決失敗は `other`。
pub fn op_scope_id(cfg: &NativeConfig) -> Result<u32, MatError> {
    mat_controller::dnssd::iface_index(&cfg.iface).map_err(|e| {
        MatError::new(
            ErrorKind::Other,
            format!("native: resolve iface {:?} index: {e}", cfg.iface),
        )
    })
}

/// 資格情報から実確立器（mDNS 解決 → CASE）を作る（`Engine::build` の後半）。
/// `creds.ipk_operational` を差し替えて渡せば別 epoch の IPK で CASE を張る
/// 確立器になる（rotate-ipk の受理実証）。`scope_id` は `op_scope_id(cfg)`。
pub fn case_establisher(
    cfg: &NativeConfig,
    creds: FabricCredentials,
    resolver: Arc<dyn Resolver>,
    scope_id: u32,
) -> Box<dyn Establisher> {
    Box::new(CaseEstablisher {
        creds: std::sync::RwLock::new(Arc::new(creds)),
        scope_id,
        resolver,
        cfg: cfg.clone(),
    })
}

impl Engine {
    /// KVS から資格情報を1回読み、NOC を自己発行し、UDP transport を bind、
    /// iface の scope_id を解決して実確立器を構築する。op (unicast) 側の
    /// scope_id はプロセス寿命で不変。group egress の scope_id は送信時に
    /// self-heal する (issue #23 — otbr 再起動で wpan0 の ifindex が変わる)。
    pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError> {
        Self::build_with_resolver(cfg, Arc::new(OneShotResolver)).await
    }

    /// [`Self::build`] と同じだが、establish の mDNS 解決に使う [`Resolver`] を注入する
    /// （matd が `CachingResolver` を渡す。`mat` 一発は `build` の OneShotResolver）。
    pub async fn build_with_resolver(
        cfg: &NativeConfig,
        resolver: Arc<dyn Resolver>,
    ) -> Result<Self, MatError> {
        let main_ini = cfg.store.join(mat_controller::kvs::MAIN_INI_FILE);
        let creds = load_fabric_credentials(cfg)?;
        let scope_id = op_scope_id(cfg)?;
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
                match group::open_egress(&name, tsid).await {
                    Ok(e) => {
                        tracing::info!(iface = %name, "groupcast thread egress enabled");
                        egress.push(e);
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
        let establisher = case_establisher(cfg, creds, resolver, scope_id);
        Ok(Self {
            establisher,
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

    /// 資格情報（IPK を含む）を KVS から読み直して確立器へ差し替える
    /// （`matd reload`）。既存 session には触れない — 次の確立から効く。
    /// 戻り値は `ipk_operational` が変わったか。
    pub fn reload_credentials(&self) -> Result<bool, MatError> {
        self.establisher.reload_credentials()
    }
}

/// 実確立器: 保持した資格情報で mDNS 解決 → CASE。op セッションのソケットは
/// ノードごとに専用（監査#3）— 共有ソケットは group multicast 送信のみ。
/// `creds` は `reload_credentials` で丸ごと差し替わる（`RwLock<Arc<_>>`: 読み手
/// は Arc をクローンして走るので、進行中の確立は旧資格情報で完走し、次の確立
/// から新しい方を使う）。`cfg` は差し替え時に KVS を読み直すための起動設定。
struct CaseEstablisher {
    creds: std::sync::RwLock<Arc<FabricCredentials>>,
    scope_id: u32,
    resolver: Arc<dyn Resolver>,
    cfg: NativeConfig,
}

impl CaseEstablisher {
    /// 現在の資格情報（Arc クローン）。poison は中身をそのまま使う
    /// （SubHealth と同じ規律 — panic 中のスレッドが壊せる不変条件は無い）。
    fn creds(&self) -> Arc<FabricCredentials> {
        Arc::clone(
            &self
                .creds
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// 資格情報を差し替え、`ipk_operational` が変わったかを返す。
    /// IPK が同じでも `FabricCredentials` は丸ごと入れ替わる（新しく自己発行
    /// した運用鍵 + NOC になる）。mat はセッションを resume しない（毎回
    /// CASE を張り直す）ので、これは無害。
    fn swap_credentials(&self, fresh: FabricCredentials) -> bool {
        let mut slot = self
            .creds
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = slot.ipk_operational != fresh.ipk_operational;
        *slot = Arc::new(fresh);
        changed
    }

    /// mDNS 解決 → 専用 UdpTransport + CASE（`case::establish_any` の
    /// staggered race）。op / 購読の違いはエラー detail とログの前置きだけ。
    async fn establish_raw(
        &self,
        node_id: u64,
        role: EstablishRole,
    ) -> Result<SessionConn, MatError> {
        // 専用ソケット: 共有ソケットでは並行 op が他ノード宛の応答を recv して
        // screen で捨てる（監査#3）。購読も op 用 transport と recv を奪い合わ
        // ないようノードごとに専用（spec 構造判断）。試行ごとの bind と候補
        // アドレスの staggered race（Happy Eyeballs）は `case::establish_any`
        // が一括して行う。
        let creds = self.creds();
        let cfid = compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
        let resolved = self
            .resolver
            .resolve(self.scope_id, cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| map_resolve_err(node_id, e))?;
        let mrp = resolved.mrp_config();
        let peers: Vec<SocketAddr> = resolved.socket_addrs(self.scope_id);
        let est = case::establish_any(&peers, &creds, node_id, &mrp, case::RACE_STAGGER)
            .await
            .map_err(|e| map_establish_err(node_id, role, e))?;
        // local port は実機切り分け（ss -uanp / tcpdump 突合）の鍵なので
        // 確立ごとに可視化する（op / 購読で同形）。
        tracing::info!(
            node_id,
            local = %est.local.map(|a| a.to_string()).unwrap_or_default(),
            peer = %est.peer,
            "{} transport bound (dedicated socket + CASE)",
            role.log_label()
        );
        Ok(SessionConn {
            session: est.session,
            mrp,
        })
    }
}

/// reload の identity 照合: fabric_id / node_id / root 公開鍵が起動時と違う
/// store は「別の fabric」なので reload では受けず restart を案内する（warm
/// session・購読・CFID がすべて別物になるため）。
pub(crate) fn check_identity(
    current: &FabricCredentials,
    fresh: &FabricCredentials,
) -> Result<(), MatError> {
    if current.fabric_id != fresh.fabric_id
        || current.node_id != fresh.node_id
        || current.root_public_key != fresh.root_public_key
    {
        return Err(MatError::new(
            ErrorKind::Other,
            "fabric identity changed (fabric_id/node_id/root key) since start-up; restart matd",
        ));
    }
    Ok(())
}

#[async_trait]
impl Establisher for CaseEstablisher {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
        Ok(Box::new(
            self.establish_raw(node_id, EstablishRole::Op).await?,
        ))
    }

    async fn establish_subscription(
        &self,
        node_id: u64,
    ) -> Result<Box<dyn SubscribeConn>, MatError> {
        Ok(Box::new(
            self.establish_raw(node_id, EstablishRole::Subscription)
                .await?,
        ))
    }

    fn reload_credentials(&self) -> Result<bool, MatError> {
        let fresh = load_fabric_credentials(&self.cfg)?;
        check_identity(&self.creds(), &fresh)?;
        Ok(self.swap_credentials(fresh))
    }
}

/// 実セッション: SecureSession + そのノードの MRP 設定。op（`NodeConn`）と
/// 購読（`SubscribeConn`）は同じ型で、確立時の役割（専用ソケット + 専用
/// CASE）が違うだけ。
struct SessionConn {
    session: mat_controller::session::SecureSession,
    mrp: MrpConfig,
}

#[async_trait]
impl SubscribeConn for SessionConn {
    async fn subscribe(
        &mut self,
        clusters: &[u32],
        event_paths: &[mat_controller::im::EventPathIn],
        event_min: Option<u64>,
    ) -> Result<SubscribeStart, MatError> {
        // 間隔 / KeepSubscriptions はこのプロセスの固定方針（上の定数）。
        // spec が運ぶのは呼び手が決める部分（属性クラスタ絞り込みと
        // イベント範囲）だけ。
        let spec = mat_controller::im::SubscribeSpec {
            min_interval_floor_s: SUBSCRIBE_MIN_INTERVAL_FLOOR_S,
            max_interval_ceiling_s: SUBSCRIBE_MAX_INTERVAL_CEILING_S,
            keep_subscriptions: SUBSCRIBE_KEEP_SUBSCRIPTIONS,
            clusters: clusters.to_vec(),
            event_paths: event_paths.to_vec(),
            event_min,
        };
        let outcome = self
            .session
            .subscribe(&spec, &self.mrp)
            .await
            .map_err(map_session_err)?;
        Ok((
            SubscriptionInfo {
                subscription_id: outcome.response.subscription_id,
                max_interval_s: outcome.response.max_interval_s,
            },
            outcome.priming,
            outcome.priming_events,
        ))
    }

    async fn next_report_full(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<mat_controller::session::SubscriptionReport>, MatError> {
        match self
            .session
            .next_subscription_report_full(timeout, &self.mrp)
            .await
        {
            Ok(report) => Ok(Some(report)),
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

    async fn invoke_for_data(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
        timed: bool,
    ) -> Result<Vec<u8>, MatError> {
        let data = self
            .session
            .invoke_for_data(
                endpoint,
                cluster,
                command,
                fields.as_deref(),
                timed.then_some(TIMED_REQUEST_MS),
                &self.mrp,
            )
            .await
            .map_err(map_session_err)?;
        Ok(data.fields_tlv.unwrap_or_default())
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

#[cfg(test)]
mod tests;
