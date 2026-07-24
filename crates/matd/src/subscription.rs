//! matd 常駐 Subscribe（spec: 2026-07-20-matd-subscribe-listen-design.md ②）。
//!
//! 起動時に KVS から commissioned ノード一覧を読み、ノードごとに購読タスクを
//! 1 本張る: resolve（常駐 mDNS キャッシュ）→ 専用 CASE → wildcard Subscribe →
//! ポンプ。失敗・死亡時は指数 backoff（5s 開始、上限 5min）で再購読。
//! イベントは `tokio::sync::broadcast` で listen 接続へ配る。
//! 状態は持たない（リングバッファ/リプレイ無し — 聞いている間だけ届く契約）。
//! op 相関 + 無音 deadline = max_interval+30s の死活判定（spec 2026-07-21-matd-borndead-detection）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::broadcast;

use mat_controller::im::ReportDataMessage;
use mat_core::output::now_iso8601;
use mat_core::store::Store;

use crate::server::NativeState;

/// 再購読 backoff の初期値 / 上限。
const BACKOFF_INITIAL: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// 未確立がこの時間続いたら warn を 1 回出す（弱リンクノードの長期ブラインドを
/// 本番 info/warn レベルで可視化する — 実測で盲目窓が数時間に達した反省）。
const STUCK_WARN_AFTER: Duration = Duration::from_secs(600);

/// pump の受信待ち 1 スライス。op 相関検知（SubHealth）をこの周期で確認する。
/// `next_report` は recv → screen → StatusResponse の多段 await で cancel-safe
/// でないため、`select!` ではなくスライスで刻む（spec §1）。
const PUMP_SLICE: Duration = Duration::from_secs(5);
/// 状態変更 op 成功からデバイス発メッセージ皆無をこの時間まで許す（spec §1）。
const OP_GRACE: Duration = Duration::from_secs(10);
/// 無音 deadline: デバイス選択 max_interval + この slack。デバイスは
/// max_interval までに必ず report か keep-alive を送る義務があり、slack は
/// MRP 再送とジッタの余裕（旧 DEATH_FACTOR 1.5 = 450s を置換、spec §2）。
const SILENCE_SLACK: Duration = Duration::from_secs(30);

/// 無音 deadline の計算（純関数）。
pub(crate) fn silence_deadline(max_interval_s: u16) -> Duration {
    (Duration::from_secs(u64::from(max_interval_s)) + SILENCE_SLACK).max(Duration::from_secs(5))
}

/// pump 終了理由（純関数 `pump_verdict` の出力 — ログ文言の出し分けに使う）。
#[derive(Debug, PartialEq)]
pub(crate) enum PumpEnd {
    /// 状態変更 op から OP_GRACE 経過してもデバイス発ゼロ（op 相関の born-dead 検知）。
    OpGrace { since_op: Duration },
    /// 確立以降デバイス発ゼロのまま無音 deadline 超過（born-dead）。
    BornDeadSilence,
    /// 生存実績のあと無音 deadline 超過（通常の購読死）。
    Silence,
}

/// pump を殺すべきか判定する（純関数 — 時計は pump が持つ）。
/// op 相関を無音 deadline より先に評価する（そちらが常に早く満ちるため）。
pub(crate) fn pump_verdict(
    proven: bool,
    since_last_msg: Duration,
    deadline: Duration,
    pending_op: Option<Duration>,
) -> Option<PumpEnd> {
    if let Some(since_op) = pending_op {
        if since_op >= OP_GRACE {
            return Some(PumpEnd::OpGrace { since_op });
        }
    }
    if since_last_msg >= deadline {
        return Some(if proven {
            PumpEnd::Silence
        } else {
            PumpEnd::BornDeadSilence
        });
    }
    None
}

/// op 相関ヘルス表: server op 経路（書き手）と購読 pump（読み手）の共有状態。
/// 「状態変更 op が success したのにデバイス発メッセージが来ない」= レポート
/// 経路死の証拠、を pending として持つ。ephemeral なランタイム状態のみ
/// （設計ルール4の永続状態には該当しない）。
pub struct SubHealth {
    /// 購読対象クラスタ集合（subscriptions.toml 由来。空 = full wildcard = 全対象）。
    clusters: Vec<u32>,
    /// node_id → 未消化の状態変更 op の時刻。
    pending: Mutex<HashMap<u64, tokio::time::Instant>>,
    /// 属性最終既知値。購読 pump（書き手: priming / live 全イベント）と
    /// server op 経路（読み手: 「この op は本当に値を変えるか」の証明）で共有する。
    /// ephemeral なプロセス内状態のみ（設計ルール4の永続状態には該当しない）。
    #[allow(dead_code)] // テスト検証済み、Task 3 で購読 pump へ配線予定
    values: Mutex<HashMap<ValueKey, serde_json::Value>>,
}

impl SubHealth {
    pub fn new(clusters: Option<Vec<u32>>) -> Self {
        Self {
            clusters: clusters.unwrap_or_default(),
            pending: Mutex::new(HashMap::new()),
            values: Mutex::new(HashMap::new()),
        }
    }

    /// 状態変更 op が success した。cluster が購読対象なら pending を打つ。
    pub fn note_op(&self, node_id: u64, cluster: u32) {
        if !self.clusters.is_empty() && !self.clusters.contains(&cluster) {
            return;
        }
        self.pending
            .lock()
            .unwrap()
            .insert(node_id, tokio::time::Instant::now());
    }

    /// デバイス発メッセージ（keep-alive 含む）や priming を受けた — pending 解除。
    pub fn clear_pending(&self, node_id: u64) {
        self.pending.lock().unwrap().remove(&node_id);
    }

    /// 未消化 op からの経過時間（無ければ None）。
    pub fn pending_elapsed(&self, node_id: u64) -> Option<Duration> {
        self.pending
            .lock()
            .unwrap()
            .get(&node_id)
            .map(|t| t.elapsed())
    }

    /// pump が受けた 1 イベントをキャッシュへ反映し、差分 priming なら昇格して返す。
    /// listen クライアントの有無と無関係に呼ぶ（状態追跡は購読が生きている限り継続）。
    #[allow(dead_code)] // テスト検証済み、Task 3 で購読 pump へ配線予定
    pub(crate) fn observe(&self, ev: Event) -> Event {
        let mut cache = self.values.lock().unwrap();
        classify_against_cache(&mut cache, ev)
    }

    /// 属性の最終既知値（未知なら None）。
    #[allow(dead_code)] // テスト検証済み、Task 3 で server op 経路へ配線予定
    pub(crate) fn cached_value(
        &self,
        node_id: u64,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
    ) -> Option<serde_json::Value> {
        self.values
            .lock()
            .unwrap()
            .get(&(node_id, endpoint, cluster, attribute))
            .cloned()
    }
}

/// 確立失敗ログの出し分け（純関数 — 時計はループ側が持つ）。
/// 毎試行 info は常駐ノイズ（弱リンクはバックオフ上限 5 分毎に永久に失敗し
/// 続ける）なので、状態遷移 + 間引きで出す — spec ①。
#[derive(Debug)]
pub(crate) enum FailureLog {
    /// 成功（or 起動）後の最初の失敗: info。
    First,
    /// 未確立 STUCK_WARN_AFTER 超・未警告: warn を 1 回。
    StuckWarn,
    /// それ以外: debug。
    Quiet,
}

pub(crate) fn classify_failure(
    consecutive_failures: u32,
    down_for: Duration,
    warned: bool,
) -> FailureLog {
    if consecutive_failures == 1 {
        FailureLog::First
    } else if !warned && down_for >= STUCK_WARN_AFTER {
        FailureLog::StuckWarn
    } else {
        FailureLog::Quiet
    }
}

/// listen へ配る 1 イベント。cluster/attribute は数値で持ち、JSON 化時に
/// chip-tool 記法へ名前化する（フィルタ照合は数値で行うため）。timestamp は
/// report 受信時に一度だけ採取した値を保持する（listener ごと・emit 時刻での
/// 再採取はしない — 同一 report 由来のイベントは全リスナーで同じ時刻を返す）。
#[derive(Debug, Clone)]
pub struct Event {
    pub timestamp: String,
    pub node_id: u64,
    pub endpoint: u16,
    pub cluster: u32,
    pub attribute: u32,
    pub value: serde_json::Value,
    pub priming: bool,
    /// priming 差分回復で昇格したイベント（購読の盲目期間中に起きた実遷移を
    /// 再購読時の priming から検出したもの）。`priming` と直交し、昇格時は
    /// `priming: false` + `recovered: true` になる。timestamp は受信時刻で
    /// あり、実際の遷移時刻ではない。
    pub recovered: bool,
}

/// 属性値キャッシュのキー: (node_id, endpoint, cluster, attribute)。
pub(crate) type ValueKey = (u64, u16, u32, u32);

/// priming イベントをキャッシュと突き合わせ、盲目期間中に起きた実遷移なら
/// 通常イベントへ昇格する（spec 2026-07-23 priming 差分回復）。
///
/// - 同値: 何も変えず素通し（消費者は priming を無視する）。
/// - 既知の値と異なる priming: `priming=false` + `recovered=true` へ昇格。
/// - 初見（キャッシュに無い）: 昇格**しない**（matd 起動直後の全量 priming で
///   誤発火させないため）。キャッシュには格納する。
/// - 非 priming: 素通し + キャッシュ更新。
#[allow(dead_code)] // テスト検証済み、Task 3 で購読 pump へ配線予定
pub(crate) fn classify_against_cache(
    cache: &mut HashMap<ValueKey, serde_json::Value>,
    ev: Event,
) -> Event {
    let key = (ev.node_id, ev.endpoint, ev.cluster, ev.attribute);
    if cache.get(&key).is_some_and(|prev| *prev == ev.value) {
        return ev;
    }
    let known = cache.contains_key(&key);
    cache.insert(key, ev.value.clone());
    if known && ev.priming {
        return Event {
            priming: false,
            recovered: true,
            ..ev
        };
    }
    ev
}

impl Event {
    /// mat スキーマの NDJSON 1 行分。cluster/attribute は `mat-core::ids` に
    /// あれば chip-tool 記法名、無ければ数値のまま（read と同じ規律）。
    /// timestamp は report 受信時に採取済みの値をそのまま使う（emit 時刻の
    /// 再採取はしない）。
    pub fn to_json(&self) -> serde_json::Value {
        let cluster = match mat_core::ids::find_cluster(self.cluster) {
            Some(def) => serde_json::json!(def.name),
            None => serde_json::json!(self.cluster),
        };
        let attribute = match mat_core::ids::find_cluster(self.cluster)
            .and_then(|c| c.attrs.iter().find(|a| a.id == self.attribute))
        {
            Some(def) => serde_json::json!(def.name),
            None => serde_json::json!(self.attribute),
        };
        serde_json::json!({
            "timestamp": self.timestamp.clone(),
            "node_id": self.node_id,
            "endpoint": self.endpoint,
            "cluster": cluster,
            "attribute": attribute,
            "value": self.value,
            "priming": self.priming,
            "recovered": self.recovered,
        })
    }
}

/// ReportDataMessage をイベント列へ。scalar 値のみイベント化し、list/struct
/// （ACL・server-list 等 wildcard priming に混ざるもの）は debug ログで捨てる
/// （generic read と同じ既知の制限）。path が欠けた report・status-only も捨てる。
pub fn events_from_report(node_id: u64, msg: &ReportDataMessage, priming: bool) -> Vec<Event> {
    let mut out = Vec::new();
    // 1 report から生まれる全イベントで同じ受信時刻を共有する（listener ごと・
    // emit 時刻での再採取はしない — 同時到着イベントは同じ timestamp が正しい）。
    let ts = now_iso8601();
    for rep in &msg.reports {
        let (Some(endpoint), Some(cluster), Some(attribute)) =
            (rep.endpoint, rep.cluster, rep.attribute)
        else {
            continue;
        };
        let Some(data) = &rep.data else { continue };
        if data.is_array() || data.is_object() {
            tracing::debug!(
                node_id,
                endpoint,
                cluster,
                attribute,
                "dropping non-scalar report"
            );
            continue;
        }
        out.push(Event {
            timestamp: ts.clone(),
            node_id,
            endpoint,
            cluster,
            attribute,
            value: data.clone(),
            priming,
            recovered: false,
        });
    }
    out
}

/// 指数 backoff: 5s 開始、倍々、上限 5min。
pub(crate) fn next_backoff(cur: Duration) -> Duration {
    if cur.is_zero() {
        BACKOFF_INITIAL
    } else {
        (cur * 2).min(BACKOFF_MAX)
    }
}

/// commissioned 全ノードへ購読タスクを張る。cluster 絞り込みは subscriptions.toml で
/// 実装済み（`clusters` パラメータに配線）。今後はノード単位の絞り込み（per-node 粒度）
/// の検討。native が Unavailable なら何もしない。
pub fn spawn_subscription_manager(
    native: Arc<NativeState>,
    store_path: PathBuf,
    events: broadcast::Sender<Event>,
    clusters: Option<Vec<u32>>,
    health: Arc<SubHealth>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let node_ids: Vec<u64> = match Store::open(&store_path) {
        Ok(store) => store.nodes().map(|n| n.node_id).collect(),
        Err(e) => {
            tracing::warn!(error = %e.detail, "subscription manager: store unreadable; no subscriptions");
            return Vec::new();
        }
    };
    // None = subscriptions.toml 無し = full wildcard（空 slice がワイヤ上の wildcard 形）。
    let clusters: Arc<[u32]> = clusters.unwrap_or_default().into();
    tracing::info!(nodes = node_ids.len(), "subscription manager starting");
    node_ids
        .into_iter()
        .map(|node_id| {
            let native = Arc::clone(&native);
            let events = events.clone();
            let clusters = Arc::clone(&clusters);
            let health = Arc::clone(&health);
            tokio::spawn(async move {
                node_subscription_loop(node_id, native, events, clusters, health).await
            })
        })
        .collect()
}

/// 1 ノードの購読ループ。確立 → priming 配信 → ポンプ。失敗・死亡は backoff 再購読。
/// ストリーク初回失敗は info、未確立 10 分で warn 1 回、以降リトライは debug、確立/喪失は info
/// （弱リンクノードを常駐ノイズにしない規律は不変）。
async fn node_subscription_loop(
    node_id: u64,
    native: Arc<NativeState>,
    events: broadcast::Sender<Event>,
    clusters: Arc<[u32]>,
    health: Arc<SubHealth>,
) {
    let NativeState::Ready(backend) = &*native else {
        return;
    };
    let mut backoff = Duration::ZERO;
    // ダウン起点（起動 or 購読喪失）とその後の失敗ストリーク。established で
    // リセットされる（run_subscription_once が確立ログにダウン時間を載せる）。
    let mut down_since = tokio::time::Instant::now();
    let mut failures: u32 = 0;
    let mut warned = false;
    loop {
        match run_subscription_once(
            node_id, backend, &events, &clusters, &health, down_since, failures,
        )
        .await
        {
            Ok(()) => {
                // 購読が成立して喪失した: 状態遷移なので info、状態リセット。
                tracing::info!(node_id, "subscription lost; resubscribing");
                backoff = Duration::ZERO;
                down_since = tokio::time::Instant::now();
                failures = 0;
                warned = false;
            }
            Err(e) => {
                failures += 1;
                match classify_failure(failures, down_since.elapsed(), warned) {
                    FailureLog::First => {
                        tracing::info!(
                            node_id,
                            kind = ?e.kind,
                            detail = %e.detail,
                            "subscription attempt failed; retrying with backoff"
                        );
                    }
                    FailureLog::StuckWarn => {
                        warned = true;
                        tracing::warn!(
                            node_id,
                            attempts = failures,
                            down_s = down_since.elapsed().as_secs(),
                            kind = ?e.kind,
                            detail = %e.detail,
                            "subscription still not established"
                        );
                    }
                    FailureLog::Quiet => {
                        tracing::debug!(node_id, kind = ?e.kind, detail = %e.detail, "subscription attempt failed");
                    }
                }
            }
        }
        backoff = next_backoff(backoff);
        tokio::time::sleep(backoff).await;
    }
}

/// 1 回の購読試行。確立+Subscribe 成立まで到達したら Ok を返して抜ける
/// （ポンプ死亡=正常喪失）。確立前の失敗は Err。
async fn run_subscription_once(
    node_id: u64,
    backend: &crate::native::NativeBackend,
    events: &broadcast::Sender<Event>,
    clusters: &[u32],
    health: &SubHealth,
    down_since: tokio::time::Instant,
    prior_failures: u32,
) -> Result<(), mat_core::error::MatError> {
    let mut conn = backend.establish_subscription(node_id).await?;
    let (info, priming) = conn.subscribe_wildcard(clusters).await?;
    tracing::info!(
        node_id,
        subscription_id = info.subscription_id,
        max_interval_s = info.max_interval_s,
        down_s = down_since.elapsed().as_secs(),
        attempts = prior_failures + 1,
        "subscription established"
    );
    // priming は現在状態の全量 — down 中の op はここで配信されるので pending 解除。
    health.clear_pending(node_id);
    for msg in &priming {
        for ev in events_from_report(node_id, msg, true) {
            let _ = events.send(ev); // 受信者ゼロは正常（listen 接続なし）
        }
    }
    let deadline = silence_deadline(info.max_interval_s);
    tracing::debug!(
        node_id,
        deadline_s = deadline.as_secs(),
        "report pump running"
    );
    // 確立以降デバイス発を 1 度でも受けたか（born-dead 判定）。
    let mut proven = false;
    let mut last_msg = tokio::time::Instant::now();
    loop {
        if let Some(end) = pump_verdict(
            proven,
            last_msg.elapsed(),
            deadline,
            health.pending_elapsed(node_id),
        ) {
            // 再購読直後に同じ pending で即再発火しないよう先に消す。
            health.clear_pending(node_id);
            match end {
                PumpEnd::OpGrace { since_op } => tracing::info!(
                    node_id,
                    since_op_s = since_op.as_secs(),
                    "report pump ended (op-correlated: no device message after op)"
                ),
                PumpEnd::BornDeadSilence => tracing::info!(
                    node_id,
                    silent_s = last_msg.elapsed().as_secs(),
                    "report pump ended (born-dead: no device message since establishment)"
                ),
                PumpEnd::Silence => tracing::info!(
                    node_id,
                    silent_s = last_msg.elapsed().as_secs(),
                    "report pump ended (silence past deadline)"
                ),
            }
            return Ok(());
        }
        let remaining = deadline.saturating_sub(last_msg.elapsed());
        let slice = PUMP_SLICE.min(remaining);
        match conn.next_report(slice).await {
            Ok(Some(msg)) => {
                proven = true;
                last_msg = tokio::time::Instant::now();
                health.clear_pending(node_id);
                for ev in events_from_report(node_id, &msg, false) {
                    let _ = events.send(ev);
                }
                // keep-alive（reports 空）も受信 = 経路生存の証明として扱う。
            }
            Ok(None) => {
                // スライス無音 — 次周回の pump_verdict で判定する。
            }
            Err(e) => {
                // セッションエラー → 再購読。何で死んだかは切り分けに必須なので
                // 詳細を残す（直後に caller が「subscription lost」を出す）。
                health.clear_pending(node_id);
                tracing::info!(node_id, kind = ?e.kind, detail = %e.detail, "report pump ended");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_native::test_support::{onoff_report, FakeEstablisher};
    use serde_json::json;

    #[test]
    fn event_json_uses_chip_tool_names_and_numeric_fallback() {
        let ev = Event {
            timestamp: "2026-07-20T00:00:00+09:00".to_string(),
            node_id: 21,
            endpoint: 1,
            cluster: 0x0406,   // occupancysensing
            attribute: 0x0000, // occupancy
            value: json!(1),
            priming: false,
            recovered: false,
        };
        let j = ev.to_json();
        assert_eq!(j["node_id"], 21);
        assert_eq!(j["endpoint"], 1);
        assert_eq!(j["cluster"], "occupancysensing");
        assert_eq!(j["attribute"], "occupancy");
        assert_eq!(j["value"], 1);
        assert_eq!(j["priming"], false);
        // 差分回復で昇格したイベントかどうかは常に載る（既定 false）。
        assert_eq!(j["recovered"], false);
        assert_eq!(j["timestamp"], "2026-07-20T00:00:00+09:00");

        // ids テーブルに無いものは数値のまま。
        let ev = Event {
            cluster: 0xFFF1_0001,
            attribute: 0x9999,
            ..ev
        };
        let j = ev.to_json();
        assert_eq!(j["cluster"], 0xFFF1_0001u32);
        assert_eq!(j["attribute"], 0x9999);

        // 昇格イベントは priming=false と recovered=true が同居する。
        let ev = Event {
            priming: false,
            recovered: true,
            ..ev
        };
        assert_eq!(ev.to_json()["recovered"], true);
    }

    #[test]
    fn events_from_report_keeps_scalars_and_drops_containers() {
        let mut msg = onoff_report(1, true);
        // list/struct（wildcard priming に混ざる ACL / server-list 等）は捨てる。
        msg.reports.push(mat_controller::im::AttributeReport {
            endpoint: Some(0),
            cluster: Some(0x001F),
            attribute: Some(0x0000),
            list_append: false,
            data: Some(json!([{ "1": 5 }])),
            status: None,
        });
        // status-only / path 欠落も捨てる。
        msg.reports.push(mat_controller::im::AttributeReport {
            endpoint: None,
            cluster: None,
            attribute: None,
            list_append: false,
            data: None,
            status: Some(0x7E),
        });
        let evs = events_from_report(7, &msg, true);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].node_id, 7);
        assert_eq!(evs[0].cluster, 0x0006);
        assert_eq!(evs[0].value, json!(true));
        assert!(evs[0].priming);
    }

    #[test]
    fn backoff_doubles_from_5s_capped_at_5min() {
        use std::time::Duration;
        assert_eq!(next_backoff(Duration::ZERO), Duration::from_secs(5));
        assert_eq!(
            next_backoff(Duration::from_secs(5)),
            Duration::from_secs(10)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(160)),
            Duration::from_secs(300)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(300)),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn failure_log_first_then_quiet_then_single_warn() {
        use std::time::Duration;
        // 1 回目の失敗は info（First）。
        assert!(matches!(
            classify_failure(1, Duration::from_secs(3), false),
            FailureLog::First
        ));
        // 2 回目以降は debug（Quiet）。
        assert!(matches!(
            classify_failure(2, Duration::from_secs(20), false),
            FailureLog::Quiet
        ));
        // 未確立 10 分超で warn（StuckWarn）— 一度だけ。
        assert!(matches!(
            classify_failure(5, Duration::from_secs(601), false),
            FailureLog::StuckWarn
        ));
        assert!(matches!(
            classify_failure(6, Duration::from_secs(900), true),
            FailureLog::Quiet
        ));
        // 初回失敗が既に 10 分超（あり得ないが）でも First 優先で情報は出る。
        assert!(matches!(
            classify_failure(1, Duration::from_secs(700), false),
            FailureLog::First
        ));
    }

    /// manager 経路: fake establisher の priming report が priming=true イベントで
    /// broadcast へ流れる。
    #[tokio::test]
    async fn manager_emits_priming_events_from_fake_subscription() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-20T00:00:00+09:00".into(),
            })
            .unwrap();

        let native =
            crate::native::NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let state = std::sync::Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let health = std::sync::Arc::new(SubHealth::new(None));
        let _handles =
            spawn_subscription_manager(state, dir.path().to_path_buf(), tx, None, health);

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no event within 2s")
            .unwrap();
        assert_eq!(ev.node_id, 5);
        assert_eq!(ev.cluster, 0x0006);
        assert!(ev.priming);
    }

    /// manager 経路: subscriptions.toml 由来のクラスタ集合が SubscribeConn::
    /// subscribe_wildcard まで届く（絞り込みの配線の釘打ち）。
    #[tokio::test]
    async fn manager_passes_clusters_to_subscribe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-21T00:00:00+09:00".into(),
            })
            .unwrap();

        let est = FakeEstablisher::default();
        let seen = std::sync::Arc::clone(&est.sub_clusters);
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let state = std::sync::Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let health = std::sync::Arc::new(SubHealth::new(None));
        let _handles = spawn_subscription_manager(
            state,
            dir.path().to_path_buf(),
            tx,
            Some(vec![0x0006, 0x0406]),
            health,
        );

        // priming イベントが届いた時点で subscribe_wildcard は呼ばれている。
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no event within 2s")
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![0x0006, 0x0406]);
    }

    #[test]
    fn silence_deadline_is_max_interval_plus_slack() {
        assert_eq!(silence_deadline(300), Duration::from_secs(330));
        assert_eq!(silence_deadline(60), Duration::from_secs(90));
        // 極端に小さくても常識的な下限（5s）を割らない。
        assert!(silence_deadline(0) >= Duration::from_secs(5));
    }

    #[test]
    fn pump_verdict_prioritizes_op_grace_then_silence() {
        let dl = Duration::from_secs(330);
        // 平常: 何も返さない。
        assert!(pump_verdict(true, Duration::from_secs(10), dl, None).is_none());
        // op から OP_GRACE 未満はまだ待つ。
        assert!(pump_verdict(
            true,
            Duration::from_secs(10),
            dl,
            Some(Duration::from_secs(9))
        )
        .is_none());
        // op から OP_GRACE 経過でデバイス発ゼロ → op 相関死。
        assert!(matches!(
            pump_verdict(
                true,
                Duration::from_secs(15),
                dl,
                Some(Duration::from_secs(10))
            ),
            Some(PumpEnd::OpGrace { .. })
        ));
        // 無音 deadline 超過: 生存実績なし → born-dead、あり → 通常無音死。
        assert!(matches!(
            pump_verdict(false, Duration::from_secs(330), dl, None),
            Some(PumpEnd::BornDeadSilence)
        ));
        assert!(matches!(
            pump_verdict(true, Duration::from_secs(330), dl, None),
            Some(PumpEnd::Silence)
        ));
    }

    #[tokio::test]
    async fn sub_health_notes_and_clears_pending_respecting_clusters() {
        // 絞り込み無し = 全 cluster が対象。
        let h = SubHealth::new(None);
        assert!(h.pending_elapsed(5).is_none());
        h.note_op(5, 0x0006);
        assert!(h.pending_elapsed(5).is_some());
        h.clear_pending(5);
        assert!(h.pending_elapsed(5).is_none());
        // 絞り込みあり: 対象外 cluster の op は無視。
        let h = SubHealth::new(Some(vec![0x0402]));
        h.note_op(5, 0x0006);
        assert!(h.pending_elapsed(5).is_none());
        h.note_op(5, 0x0402);
        assert!(h.pending_elapsed(5).is_some());
    }

    /// op 相関検知: 確立後に note_op して沈黙させると、無音 deadline (90s) を
    /// 待たず grace+backoff 内（<40s）に再購読 = 2 回目の priming が届く。
    #[tokio::test(start_paused = true)]
    async fn op_grace_triggers_fast_resubscribe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-21T00:00:00+09:00".into(),
            })
            .unwrap();
        let native =
            crate::native::NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let state = std::sync::Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, mut rx) = tokio::sync::broadcast::channel(64);
        let health = std::sync::Arc::new(SubHealth::new(None));
        let _handles = spawn_subscription_manager(
            state,
            dir.path().to_path_buf(),
            tx,
            None,
            std::sync::Arc::clone(&health),
        );
        // 1 回目の priming（確立）。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        // 状態変更 op（デバイス発は来ない = born-dead 相当）。
        let t0 = tokio::time::Instant::now();
        health.note_op(5, 0x0006);
        // grace(10s) + backoff(5s) + スライス誤差内に再購読の priming が届く。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(40), rx.recv())
            .await
            .expect("re-priming after op-grace")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(10),
            "grace より早く殺さない: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(40),
            "無音 deadline (90s) を待っていないこと: {elapsed:?}"
        );
    }

    /// live report（keep-alive 相当含む）が届けば pending は解除され、
    /// 無音 deadline 前に再購読は起きない。
    #[tokio::test(start_paused = true)]
    async fn live_report_clears_pending_without_resubscribe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-21T00:00:00+09:00".into(),
            })
            .unwrap();
        let est = FakeEstablisher::default();
        let live = std::sync::Arc::clone(&est.sub_live);
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let state = std::sync::Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, mut rx) = tokio::sync::broadcast::channel(64);
        let health = std::sync::Arc::new(SubHealth::new(None));
        let _handles = spawn_subscription_manager(
            state,
            dir.path().to_path_buf(),
            tx,
            None,
            std::sync::Arc::clone(&health),
        );
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        // op → 直後に live report が届く（健全経路）。
        health.note_op(5, 0x0006);
        live.lock().unwrap().push_back(onoff_report(1, false));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("live event")
            .unwrap();
        assert!(!ev.priming);
        assert!(health.pending_elapsed(5).is_none(), "受信で pending 解除");
        // 無音 deadline (90s) 未満の 80s の間、再購読（= 追加イベント）は起きない。
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(80), rx.recv())
                .await
                .is_err(),
            "健全な購読を殺していないこと"
        );
    }

    /// 純関数の契約（priming 差分回復 spec の挙動表）:
    /// 初見 priming → 非昇格・格納 / 同値 priming → 非昇格・素通し /
    /// 差分 priming → 昇格 / 非 priming → 素通し・更新。
    #[test]
    fn classify_against_cache_promotes_only_changed_priming() {
        fn ev(value: serde_json::Value, priming: bool) -> Event {
            Event {
                timestamp: "2026-07-24T00:00:00+09:00".to_string(),
                node_id: 5,
                endpoint: 1,
                cluster: 0x0006,
                attribute: 0x0000,
                value,
                priming,
                recovered: false,
            }
        }
        let mut cache: HashMap<ValueKey, serde_json::Value> = HashMap::new();

        // 初見 priming: 昇格しない（matd 起動直後の全量で誤発火しないため）。
        let out = classify_against_cache(&mut cache, ev(json!(true), true));
        assert!(out.priming);
        assert!(!out.recovered);
        assert_eq!(cache[&(5, 1, 0x0006, 0x0000)], json!(true));

        // 同値 priming: 素通し（消費者は priming として無視する）。
        let out = classify_against_cache(&mut cache, ev(json!(true), true));
        assert!(out.priming);
        assert!(!out.recovered);

        // 差分 priming: 盲目期間中の実遷移 → 昇格 + キャッシュ更新。
        let out = classify_against_cache(&mut cache, ev(json!(false), true));
        assert!(!out.priming);
        assert!(out.recovered);
        assert_eq!(out.value, json!(false));
        assert_eq!(cache[&(5, 1, 0x0006, 0x0000)], json!(false));

        // 非 priming（live）: 素通し + キャッシュ更新。昇格フラグは立てない。
        let out = classify_against_cache(&mut cache, ev(json!(true), false));
        assert!(!out.priming);
        assert!(!out.recovered);
        assert_eq!(cache[&(5, 1, 0x0006, 0x0000)], json!(true));

        // キーは (node, endpoint, cluster, attribute) 単位で独立している。
        let other = Event {
            node_id: 6,
            ..ev(json!(false), true)
        };
        let out = classify_against_cache(&mut cache, other);
        assert!(out.priming, "別ノードの初見は昇格しない");
        assert_eq!(cache.len(), 2);
    }

    /// SubHealth 越しに同じキャッシュを読み書きできる（op 経路と pump の共有点）。
    #[test]
    fn sub_health_observe_updates_shared_value_cache() {
        let h = SubHealth::new(None);
        assert!(h.cached_value(5, 1, 0x0006, 0x0000).is_none());
        let ev = Event {
            timestamp: "2026-07-24T00:00:00+09:00".to_string(),
            node_id: 5,
            endpoint: 1,
            cluster: 0x0006,
            attribute: 0x0000,
            value: json!(true),
            priming: true,
            recovered: false,
        };
        let out = h.observe(ev);
        assert!(out.priming && !out.recovered, "初見は素通し");
        assert_eq!(h.cached_value(5, 1, 0x0006, 0x0000), Some(json!(true)));
    }
}
