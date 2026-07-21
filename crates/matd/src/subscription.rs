//! matd 常駐 Subscribe（spec: 2026-07-20-matd-subscribe-listen-design.md ②）。
//!
//! 起動時に KVS から commissioned ノード一覧を読み、ノードごとに購読タスクを
//! 1 本張る: resolve（常駐 mDNS キャッシュ）→ 専用 CASE → wildcard Subscribe →
//! ポンプ。失敗・死亡時は指数 backoff（5s 開始、上限 5min）で再購読。
//! イベントは `tokio::sync::broadcast` で listen 接続へ配る。
//! 状態は持たない（リングバッファ/リプレイ無し — 聞いている間だけ届く契約）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use mat_controller::im::ReportDataMessage;
use mat_core::output::now_iso8601;
use mat_core::store::Store;

use crate::server::NativeState;

/// 購読死亡判定: デバイス選択 MaxInterval の 1.5 倍無音でループを抜け再購読。
const DEATH_FACTOR: f64 = 1.5;
/// 再購読 backoff の初期値 / 上限。
const BACKOFF_INITIAL: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// 未確立がこの時間続いたら warn を 1 回出す（弱リンクノードの長期ブラインドを
/// 本番 info/warn レベルで可視化する — 実測で盲目窓が数時間に達した反省）。
const STUCK_WARN_AFTER: Duration = Duration::from_secs(600);

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
            tokio::spawn(
                async move { node_subscription_loop(node_id, native, events, clusters).await },
            )
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
        match run_subscription_once(node_id, backend, &events, &clusters, down_since, failures)
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
    for msg in &priming {
        for ev in events_from_report(node_id, msg, true) {
            let _ = events.send(ev); // 受信者ゼロは正常（listen 接続なし）
        }
    }
    let deadline = Duration::from_secs_f64(f64::from(info.max_interval_s) * DEATH_FACTOR)
        .max(Duration::from_secs(5)); // MaxInterval が極端に小さくても常識的な下限
    tracing::debug!(
        node_id,
        deadline_s = deadline.as_secs(),
        "report pump running"
    );
    loop {
        match conn.next_report(deadline).await {
            Ok(Some(msg)) => {
                for ev in events_from_report(node_id, &msg, false) {
                    let _ = events.send(ev);
                }
                // keep-alive（reports 空）も無音 deadline をリセットするだけで良い。
            }
            Ok(None) => {
                // 無音 deadline 切れ → 再購読（Task 4 で born-dead/op 相関の
                // 判定に置き換わる暫定形）。
                tracing::info!(node_id, "report pump ended (silence)");
                return Ok(());
            }
            Err(e) => {
                // セッションエラー → 再購読。何で死んだかは切り分けに必須なので
                // 詳細を残す（直後に caller が「subscription lost」を出す）。
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
        };
        let j = ev.to_json();
        assert_eq!(j["node_id"], 21);
        assert_eq!(j["endpoint"], 1);
        assert_eq!(j["cluster"], "occupancysensing");
        assert_eq!(j["attribute"], "occupancy");
        assert_eq!(j["value"], 1);
        assert_eq!(j["priming"], false);
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
        let _handles = spawn_subscription_manager(state, dir.path().to_path_buf(), tx, None);

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
        let _handles = spawn_subscription_manager(
            state,
            dir.path().to_path_buf(),
            tx,
            Some(vec![0x0006, 0x0406]),
        );

        // priming イベントが届いた時点で subscribe_wildcard は呼ばれている。
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no event within 2s")
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![0x0006, 0x0406]);
    }
}
