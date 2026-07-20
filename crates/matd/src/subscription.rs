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

/// listen へ配る 1 イベント。cluster/attribute は数値で持ち、JSON 化時に
/// chip-tool 記法へ名前化する（フィルタ照合は数値で行うため）。
#[derive(Debug, Clone)]
pub struct Event {
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
            "timestamp": now_iso8601(),
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

/// commissioned 全ノードへ購読タスクを張る（v1: 起動時の台帳スナップショット。
/// 将来 subscriptions.toml で絞り込み）。native が Unavailable なら何もしない。
pub fn spawn_subscription_manager(
    native: Arc<NativeState>,
    store_path: PathBuf,
    events: broadcast::Sender<Event>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let node_ids: Vec<u64> = match Store::open(&store_path) {
        Ok(store) => store.nodes().map(|n| n.node_id).collect(),
        Err(e) => {
            tracing::warn!(error = %e.detail, "subscription manager: store unreadable; no subscriptions");
            return Vec::new();
        }
    };
    tracing::info!(nodes = node_ids.len(), "subscription manager starting");
    node_ids
        .into_iter()
        .map(|node_id| {
            let native = Arc::clone(&native);
            let events = events.clone();
            tokio::spawn(async move { node_subscription_loop(node_id, native, events).await })
        })
        .collect()
}

/// 1 ノードの購読ループ。確立 → priming 配信 → ポンプ。失敗・死亡は backoff 再購読。
/// リトライは debug、確立/喪失の状態遷移のみ info（弱リンクノードを常駐ノイズに
/// しない — spec ②）。
async fn node_subscription_loop(
    node_id: u64,
    native: Arc<NativeState>,
    events: broadcast::Sender<Event>,
) {
    let NativeState::Ready(backend) = &*native else {
        return;
    };
    let mut backoff = Duration::ZERO;
    loop {
        match run_subscription_once(node_id, backend, &events).await {
            Ok(()) => {
                // 購読が成立して喪失した: 状態遷移なので info、backoff はリセット。
                tracing::info!(node_id, "subscription lost; resubscribing");
                backoff = Duration::ZERO;
            }
            Err(e) => {
                tracing::debug!(node_id, kind = ?e.kind, detail = %e.detail, "subscription attempt failed");
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
) -> Result<(), mat_core::error::MatError> {
    let mut conn = backend.establish_subscription(node_id).await?;
    let (info, priming) = conn.subscribe_wildcard().await?;
    tracing::info!(
        node_id,
        subscription_id = info.subscription_id,
        max_interval_s = info.max_interval_s,
        "subscription established"
    );
    for msg in &priming {
        for ev in events_from_report(node_id, msg, true) {
            let _ = events.send(ev); // 受信者ゼロは正常（listen 接続なし）
        }
    }
    let deadline = Duration::from_secs_f64(f64::from(info.max_interval_s) * DEATH_FACTOR)
        .max(Duration::from_secs(5)); // MaxInterval が極端に小さくても常識的な下限
    loop {
        match conn.next_report(deadline).await {
            Ok(msg) => {
                for ev in events_from_report(node_id, &msg, false) {
                    let _ = events.send(ev);
                }
                // keep-alive（reports 空）も無音 deadline をリセットするだけで良い。
            }
            Err(_) => return Ok(()), // 無音死亡 or セッションエラー → 再購読
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
        assert!(j["timestamp"].is_string());

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
        let _handles = spawn_subscription_manager(state, dir.path().to_path_buf(), tx);

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no event within 2s")
            .unwrap();
        assert_eq!(ev.node_id, 5);
        assert_eq!(ev.cluster, 0x0006);
        assert!(ev.priming);
    }
}
