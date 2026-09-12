//! matd 常駐 Subscribe（spec: 2026-07-20-matd-subscribe-listen-design.md ②）。
//!
//! supervisor が `LEDGER_RESCAN_INTERVAL`（60s）ごとに台帳を読み直し、
//! 新規ノードへ購読ループを spawn（監査#4）。ノードごと: resolve（常駐 mDNS
//! キャッシュ）→ 専用 CASE → wildcard Subscribe → ポンプ。失敗・死亡は指数
//! backoff（5s 開始、上限 60s）で再購読。
//! 配信物（属性変化行 + デバイス発イベント行 = `Emitted`）は
//! `tokio::sync::broadcast` で listen 接続へ配る。
//! 状態は持たない（リングバッファ/リプレイ無し — 聞いている間だけ届く契約）。
//! op 相関 + 無音 deadline = max_interval+30s の死活判定（spec 2026-07-21-matd-borndead-detection。teardown 前の probe 延長は実測で純損失と判明し撤去 — spec 2026-07-30）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use mat_core::store::Store;

use crate::server::NativeState;
use crate::subscribe_config::EventScope;

/// イベント行（EventReport 由来）と、属性行との合流点 `Emitted`。
mod events;
mod health;
mod pump;
pub use events::{
    events_from_event_reports, events_from_report, events_from_report_at, Emitted, Event, EventItem,
};
pub use health::SubHealth;
pub(crate) use health::{classify_failure, FailureLog};

/// 台帳の再読間隔。稼働中に `mat commission` されたノードを最大この遅延で
/// 拾って購読を張る（監査#4: 従来は起動時スナップショットのみで、稼働中
/// commission ノードは matd 再起動まで購読されず `mat listen` が無音だった）。
pub(crate) const LEDGER_RESCAN_INTERVAL: Duration = Duration::from_secs(60);

/// 起動 herd の stagger 刻み。同一ティックで複数ノードを spawn するとき、
/// バッチ内 index × この値だけ初回確立を遅らせる（本番 13 台 → 0〜12s に
/// 均等分散）。デプロイ再起動のたびに全ノード同時 CASE で BR 無線が CCA
/// 飽和 → no-ack 1〜2 分、が監査⑧の実 symptom。乱数でなく index 均等なのは
/// herd が単一プロセス内の現象で、均等間隔が厳密に非衝突なため。
pub(crate) const STAGGER_STEP: Duration = Duration::from_secs(1);

/// バッチ内 index → 初期遅延（純関数）。バッチ 1 = 遅延ゼロ（rescan の
/// 単発追加を現行どおり即購読に保つ）。
pub(crate) fn stagger_delay(batch_index: usize, batch_len: usize) -> Duration {
    if batch_len <= 1 {
        Duration::ZERO
    } else {
        STAGGER_STEP * u32::try_from(batch_index).unwrap_or(u32::MAX)
    }
}

/// 常駐購読の範囲（subscriptions.toml 由来）: 属性のクラスタ絞り込みと
/// イベント範囲。両者は独立（spec §6.2）だが、ノードごとの購読ループへは
/// 常に一緒に運ぶので 1 つにまとめる。全ループで共有するので `Arc`。
#[derive(Clone)]
pub(crate) struct SubscribeScope {
    /// 属性の絞り込み。空 = full wildcard（空 slice がワイヤ上の wildcard 形）。
    pub(super) clusters: Arc<[u32]>,
    pub(super) events: Arc<EventScope>,
}

/// commissioned 全ノードへ購読タスクを張る supervisor を起動する。
/// `LEDGER_RESCAN_INTERVAL` ごとに台帳を読み直し、新規ノードに購読ループを
/// 追加 spawn する（op 経路の `require_node` が毎回 store を開き直すのと同じ
/// 「常駐中の台帳更新を拾う」規律）。`mat unpair` で台帳から消えたノードは
/// 逆に購読ループを abort して health からも外す。属性の cluster 絞り込み
/// （`clusters`）とイベント範囲（`event_scope`）はどちらも subscriptions.toml
/// 由来で、独立に効く（spec §6.2）。native が Unavailable なら何もしない
/// （`mat fabric init` 後の再起動で解消 — 再読で直る状態ではないので空回り
/// させない）。
pub fn spawn_subscription_manager(
    native: Arc<NativeState>,
    store_path: PathBuf,
    events: broadcast::Sender<Emitted>,
    clusters: Option<Vec<u32>>,
    event_scope: EventScope,
    health: Arc<SubHealth>,
) -> tokio::task::JoinHandle<()> {
    // None = subscriptions.toml 無し = full wildcard（空 slice がワイヤ上の wildcard 形）。
    let scope = SubscribeScope {
        clusters: clusters.unwrap_or_default().into(),
        events: Arc::new(event_scope),
    };
    tokio::spawn(async move {
        if !matches!(&*native, NativeState::Ready(_)) {
            return;
        }
        // 購読ループを張った node_id → そのループの JoinHandle。台帳から
        // 消えたノードを abort するために handle を持つ。
        let mut subscribed: HashMap<u64, tokio::task::JoinHandle<()>> = HashMap::new();
        let mut announced = false;
        let mut read_fail_streak: u32 = 0;
        loop {
            match Store::open(&store_path) {
                Ok(store) => {
                    read_fail_streak = 0;
                    let node_ids: Vec<u64> = store.nodes().map(|n| n.node_id).collect();
                    // レーン B: unpair で台帳から消えたノードは購読ループを abort
                    // して status からも外す。abort は購読ループの cancel-safe 性に
                    // 依存しない — ループは establish / Subscribe / pump の await と
                    // backoff sleep しか持たず、途中で落として壊れる複合不変条件は
                    // 無い（SubConn は drop で閉じる。RemoveFabric 済みのデバイス側
                    // セッションはどうせ無効）。
                    let current: HashSet<u64> = node_ids.iter().copied().collect();
                    let removed: Vec<u64> = subscribed
                        .keys()
                        .filter(|id| !current.contains(id))
                        .copied()
                        .collect();
                    for node_id in removed {
                        if let Some(handle) = subscribed.remove(&node_id) {
                            // abort() は「次の await 点で落とす」予約でしかない。
                            // join せずに forget すると、走行中のループが await 間で
                            // 書く health（mark_establishing / record_*）が forget の
                            // 後に着地し、matd 再起動まで status / values に幽霊行が
                            // 残る。ループは spawn_blocking / block_in_place を持たず
                            // await 点しか無いので、この join は即座に返る。
                            handle.abort();
                            let _ = handle.await;
                        }
                        health.forget(node_id);
                        tracing::info!(node_id, "ledger rescan: node removed; unsubscribed");
                    }
                    // 初回の成功読みだけ台数つきの starting ログ（現行踏襲）。
                    // 以降の新規検出はノード単位の info（commission は稀な操作
                    // なのでノイズにならず、「ログに一切現れない」誤診の罠を潰す）。
                    let initial = !announced;
                    if initial {
                        tracing::info!(nodes = node_ids.len(), "subscription manager starting");
                        announced = true;
                    }
                    // このティックで新規に張るノードだけを先にバッチとして
                    // 確定してから index つきで spawn する（stagger_delay の
                    // 分母はこのバッチのサイズ。台帳全体のサイズではない）。
                    let new_nodes: Vec<u64> = node_ids
                        .into_iter()
                        .filter(|id| !subscribed.contains_key(id))
                        .collect();
                    for (i, node_id) in new_nodes.iter().copied().enumerate() {
                        if !initial {
                            tracing::info!(node_id, "ledger rescan: new node; subscribing");
                        }
                        let delay = stagger_delay(i, new_nodes.len());
                        let native = Arc::clone(&native);
                        let events = events.clone();
                        let scope = scope.clone();
                        let health_for_task = Arc::clone(&health);
                        let handle = tokio::spawn(async move {
                            pump::node_subscription_loop(
                                node_id,
                                delay,
                                native,
                                events,
                                scope,
                                health_for_task,
                            )
                            .await
                        });
                        subscribed.insert(node_id, handle);
                    }
                }
                Err(e) => {
                    // ストリーク初回 warn、以降 debug（60 秒ごとの warn 連打を
                    // 避ける — classify_failure と同じ思想）。transient な失敗
                    //（flock 競合等）は次のティックで自己回復する。
                    read_fail_streak += 1;
                    if read_fail_streak == 1 {
                        tracing::warn!(error = %e.detail, "subscription manager: store unreadable; will retry");
                    } else {
                        tracing::debug!(error = %e.detail, "subscription manager: store unreadable");
                    }
                }
            }
            tokio::time::sleep(LEDGER_RESCAN_INTERVAL).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_native::test_support::{onoff_report, FakeEstablisher};
    use pump::{run_subscription_once, BACKOFF_MAX, PUMP_SLICE};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// node 5 だけの台帳と fake establisher で購読マネージャを起動する共通足場。
    ///
    /// 戻り値の `TempDir` は**テスト側が束縛して生かし続ける**こと（`_dir` は可、
    /// `_` は不可 — `_` は即 drop され store ごと消える）。`JoinHandle` も同様に
    /// 束縛しておく（既存テストの寿命の握り方をそのまま踏襲）。
    pub(super) fn spawn_manager(
        est: FakeEstablisher,
        clusters: Option<Vec<u32>>,
    ) -> (
        AttrRx,
        Arc<SubHealth>,
        tempfile::TempDir,
        tokio::task::JoinHandle<()>,
    ) {
        let (rx, health, dir, handle) = spawn_manager_with(est, clusters, EventScope::Wildcard);
        (AttrRx(rx), health, dir, handle)
    }

    /// 属性行だけを取り出す受信ラッパ。既存の購読テストは属性行しか作らない
    /// （fake の priming/live イベントキューは既定で空）ので、`rx.recv()` の
    /// 呼び出し形をそのまま保てる。イベント行が混ざったら足場の想定違いなので
    /// panic させる。
    pub(super) struct AttrRx(broadcast::Receiver<Emitted>);

    impl AttrRx {
        pub(super) async fn recv(&mut self) -> Result<Event, broadcast::error::RecvError> {
            match self.0.recv().await? {
                Emitted::Attribute(e) => Ok(e),
                Emitted::Event(e) => panic!("属性行を期待したがイベント行が来た: {e:?}"),
            }
        }
    }

    /// `spawn_manager` の event スコープ指定版（イベント購読のテスト用）。
    pub(super) fn spawn_manager_with(
        est: FakeEstablisher,
        clusters: Option<Vec<u32>>,
        event_scope: EventScope,
    ) -> (
        broadcast::Receiver<Emitted>,
        Arc<SubHealth>,
        tempfile::TempDir,
        tokio::task::JoinHandle<()>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                commissioned_at: "2026-07-20T00:00:00+09:00".into(),
            })
            .unwrap();
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let state = Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, rx) = broadcast::channel(64);
        let health = Arc::new(SubHealth::new(None));
        let handle = spawn_subscription_manager(
            state,
            dir.path().to_path_buf(),
            tx,
            clusters,
            event_scope,
            Arc::clone(&health),
        );
        (rx, health, dir, handle)
    }

    /// 起動 stagger: 同一ティックのバッチ(>1)だけ index × 1s に分散。
    /// rescan の単発追加（バッチ 1）は現行どおり遅延ゼロ。
    #[test]
    fn stagger_delay_spreads_batches_only() {
        assert_eq!(stagger_delay(0, 1), Duration::ZERO);
        assert_eq!(stagger_delay(0, 13), Duration::ZERO);
        assert_eq!(stagger_delay(1, 13), Duration::from_secs(1));
        assert_eq!(stagger_delay(12, 13), Duration::from_secs(12));
    }

    /// manager 経路: fake establisher の priming report が priming=true イベントで
    /// broadcast へ流れる。
    #[tokio::test]
    async fn manager_emits_priming_events_from_fake_subscription() {
        let (mut rx, _health, _dir, _handles) = spawn_manager(FakeEstablisher::default(), None);

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no event within 2s")
            .unwrap();
        assert_eq!(ev.node_id, 5);
        assert_eq!(ev.cluster, 0x0006);
        assert!(ev.priming);
    }

    /// manager 経路: subscriptions.toml 由来のクラスタ集合が SubscribeConn::
    /// subscribe まで届く（絞り込みの配線の釘打ち）。
    #[tokio::test]
    async fn manager_passes_clusters_to_subscribe() {
        let est = FakeEstablisher::default();
        let seen = Arc::clone(&est.sub_clusters);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, Some(vec![0x0006, 0x0406]));

        // priming イベントが届いた時点で subscribe は呼ばれている。
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no event within 2s")
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![0x0006, 0x0406]);
    }

    /// op 相関検知: 確立後に note_op して沈黙させると、無音 deadline (90s) を
    /// 待たず grace+backoff 内（<40s）に再購読 = 2 回目の priming が届く。
    #[tokio::test(start_paused = true)]
    async fn op_grace_triggers_fast_resubscribe() {
        let (mut rx, health, _dir, _handles) = spawn_manager(FakeEstablisher::default(), None);
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
        let est = FakeEstablisher::default();
        let live = Arc::clone(&est.sub_live);
        let (mut rx, health, _dir, _handles) = spawn_manager(est, None);
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

    /// 差分回復の統合: priming(true) → live(false) → 購読死 → 再 priming(true)。
    /// 2 回目の priming はキャッシュ(false)と異なるので昇格イベントとして届く。
    /// キャッシュが live イベントでも更新されること（spec テスト (c)）も同時に釘打ち。
    #[tokio::test(start_paused = true)]
    async fn priming_diff_after_resubscribe_is_promoted_to_recovered_event() {
        let est = FakeEstablisher::default();
        let live = Arc::clone(&est.sub_live);
        let (mut rx, health, _dir, _handles) = spawn_manager(est, None);

        // 1 回目の priming（on-off=true）: 初見なので昇格しない。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming && !ev.recovered);
        assert_eq!(health.cached_value(5, 1, 0x0006, 0x0000), Some(json!(true)));

        // live で false へ遷移 → キャッシュ更新（priming/live 両経路で更新される証明）。
        live.lock().unwrap().push_back(onoff_report(1, false));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("live event")
            .unwrap();
        assert!(!ev.priming && !ev.recovered);
        assert_eq!(
            health.cached_value(5, 1, 0x0006, 0x0000),
            Some(json!(false))
        );

        // 購読を殺して再購読させる（fake の priming は常に on-off=true）。
        health.note_op(5, 0x0006);
        let ev = tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv())
            .await
            .expect("re-priming after resubscribe")
            .unwrap();
        // 盲目期間中に false→true の実遷移があったとみなし、通常イベントへ昇格。
        assert!(!ev.priming, "昇格イベントは priming=false");
        assert!(
            ev.recovered,
            "recovered=true で消費者の既存トリガが発火する"
        );
        assert_eq!(ev.value, json!(true));
        assert_eq!(health.cached_value(5, 1, 0x0006, 0x0000), Some(json!(true)));
    }

    /// Level 配線の釘打ち: `note_op_expectation` の levelcontrol/current-level
    /// キャッシュ引き当て（cluster 0x0008 / attribute 0x0000）が実際に機能して
    /// いること。On/Off と同じく、同値の Level op は pending を立てず、差分の
    /// Level op は pending を立てる。
    #[tokio::test]
    async fn note_op_expectation_wires_level_cluster_cache() {
        let health = SubHealth::new(None);
        health.observe(Event {
            timestamp: "2026-07-24T00:00:00+09:00".to_string(),
            node_id: 5,
            endpoint: 1,
            cluster: 0x0008,
            attribute: 0x0000,
            value: json!(128),
            priming: true,
            recovered: false,
        });

        // 同値（キャッシュ 128 / op level 128）: no-op なので pending を立てない。
        crate::server::note_op_expectation(
            &crate::protocol::Op::Level {
                node_id: 5,
                endpoint: 1,
                level: 128,
                percent: 50,
                transition: 0,
            },
            &health,
        );
        assert!(
            health.pending_elapsed(5).is_none(),
            "同値の level op は no-op — pending を立てない"
        );

        // 差分（キャッシュ 128 / op level 200）: 値が変わるので pending を立てる。
        crate::server::note_op_expectation(
            &crate::protocol::Op::Level {
                node_id: 5,
                endpoint: 1,
                level: 200,
                percent: 78,
                transition: 0,
            },
            &health,
        );
        assert!(
            health.pending_elapsed(5).is_some(),
            "差分の level op は pending を立てる"
        );
    }

    /// 誤爆の釘打ち（spec テスト (a)）: priming でキャッシュが埋まった後、
    /// 同値の op（既に on のノードへの on）は pending を立てず、健全な購読を
    /// 無音 deadline 前に殺さない。
    #[tokio::test(start_paused = true)]
    async fn noop_op_does_not_kill_healthy_subscription() {
        let (mut rx, health, _dir, _handles) = spawn_manager(FakeEstablisher::default(), None);
        // priming（on-off=true）でキャッシュが埋まる。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);

        // 既に on のノードへ on = no-op。デバイスはレポートを出さないので
        // 期待を打ってはいけない。
        crate::server::note_op_expectation(
            &crate::protocol::Op::On {
                node_id: 5,
                endpoint: 1,
            },
            &health,
        );
        assert!(
            health.pending_elapsed(5).is_none(),
            "no-op で pending を打たない"
        );

        // 無音 deadline (90s) 未満の 80s の間、再購読（= 追加イベント）は起きない。
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(80), rx.recv())
                .await
                .is_err(),
            "健全な購読を殺していないこと"
        );
    }

    /// 真の born-dead 検知の維持（spec テスト (b)）: 値が実際に変わる op
    /// （on のノードへの off）でデバイスが沈黙したままなら、従来どおり
    /// grace + backoff 内（<40s）に再購読する。
    #[tokio::test(start_paused = true)]
    async fn changing_op_with_silent_device_triggers_fast_resubscribe() {
        let (mut rx, health, _dir, _handles) = spawn_manager(FakeEstablisher::default(), None);
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);

        // on のノードへ off = 値が変わる → レポートが出るはず → 期待を打つ。
        let t0 = tokio::time::Instant::now();
        crate::server::note_op_expectation(
            &crate::protocol::Op::Off {
                node_id: 5,
                endpoint: 1,
            },
            &health,
        );
        assert!(health.pending_elapsed(5).is_some());
        // デバイスは沈黙 → grace(10s) + backoff(5s) 内に再購読の priming が届く。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(40), rx.recv())
            .await
            .expect("re-priming after op-grace")
            .unwrap();
        assert_eq!(ev.value, json!(true));
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

    /// 確立が 3 回失敗したら backoff ラダー（5s → 10s → 20s）を実際に登り、
    /// 4 回目で回復する。`next_backoff` の純関数テストはあったが、ループが
    /// その間隔で再試行することは一度も通されていなかった。
    #[tokio::test(start_paused = true)]
    async fn establish_failures_climb_backoff_then_recover() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let calls = Arc::clone(&est.calls);
        est.fail_subscription.store(3, Ordering::SeqCst);
        let t0 = tokio::time::Instant::now();
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("4 回目の確立で priming が届く")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(26),
            "5+10+20 のラダーを実際に登ること（jitter で最小 ×0.75）: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(45),
            "経過は 5+10+20 のラダーちょうど（35s）範囲であり、次段は登っていないこと（jitter で最大 ×1.25）: {elapsed:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "失敗 3 + 成功 1 = 4 試行（失敗も試行として数える）"
        );
    }

    /// pump がセッションエラーで死んだら、無音 deadline (90s) を待たずに
    /// backoff 5s で再購読する（`run_subscription_once` の `Err` 分岐が
    /// `Ok(())` を返してループが「購読喪失」として扱う経路）。
    #[tokio::test(start_paused = true)]
    async fn pump_session_error_resubscribes_without_waiting_deadline() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let calls = Arc::clone(&est.calls);
        let fail_next_report = Arc::clone(&est.fail_next_report);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);

        // 確立の**あと**に注入する = 走っている pump を狙って殺す。
        let t0 = tokio::time::Instant::now();
        fail_next_report.store(1, Ordering::SeqCst);

        let ev = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .expect("再購読の priming")
            .unwrap();
        assert!(ev.priming);
        // 2 回目の priming が「本物の再確立」から来たことを検証する
        // （`ev.priming` フラグだけでは推測に留まる）。
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "2 回目の priming は本物の 2 回目の確立から来ていること"
        );
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(20),
            "無音 deadline (90s) を待たず backoff 5s で戻ること: {elapsed:?}"
        );
    }

    /// 完全無音のまま無音 deadline（max_interval 60s + slack 30s = 90s）を
    /// 超えたら購読を殺して再購読する。実機で最も頻繁に踏まれる死に方。
    /// なお `BornDeadSilence` と `Silence` は `tracing::info!` のメッセージ
    /// 文字列が違うだけで制御フローは同一なため、どちらの無音バリアントが
    /// 選ばれるかはここでは検証できない（できているのは純関数テストの
    /// `pump_verdict_prioritizes_op_grace_then_silence`）。このテストが
    /// 固定しているのは、完全無音の購読が 90s deadline で殺されて
    /// 再購読されるという一点のみ。
    #[tokio::test(start_paused = true)]
    async fn silent_subscription_dies_at_deadline_and_resubscribes() {
        let (mut rx, _health, _dir, _handles) = spawn_manager(FakeEstablisher::default(), None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        let t0 = tokio::time::Instant::now();

        // live キューへ何も入れない = デバイス発ゼロのまま（born-dead）。
        let ev = tokio::time::timeout(Duration::from_secs(180), rx.recv())
            .await
            .expect("deadline 超過で再購読の priming が届く")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(90),
            "deadline より早く購読を殺さないこと: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(120),
            "deadline + backoff 5s の範囲で再購読すること: {elapsed:?}"
        );
    }

    /// pump が無音 deadline で終わるとき close が呼ばれる（Issue #20）。
    /// close を落とすとデバイスが死んだセッションを「最新」のまま保持し、
    /// 以後の report をそこへ黙って再アンカーしてしまう。
    #[tokio::test(start_paused = true)]
    async fn pump_silence_end_closes_subscription_session() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let close_calls = Arc::clone(&est.sub_close_calls);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);

        // live キューへ何も入れない = born-dead のまま deadline 到達 → close。
        let ev = tokio::time::timeout(Duration::from_secs(180), rx.recv())
            .await
            .expect("deadline 超過で再購読の priming が届く")
            .unwrap();
        assert!(ev.priming);
        assert_eq!(
            close_calls.load(Ordering::SeqCst),
            1,
            "無音 deadline で終わった最初の購読セッションが close されていること"
        );
    }

    /// subscribe が失敗したとき（CASE は成立済み）も close される
    /// （Issue #20）。establish_subscription 自体の失敗は CASE 未成立なので
    /// close 不要 — この経路とは区別する。
    #[tokio::test]
    async fn subscribe_failure_closes_session() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher {
            fail_subscribe: true,
            ..Default::default()
        };
        let close_calls = Arc::clone(&est.sub_close_calls);
        let health = Arc::new(SubHealth::new(None));
        let (events, _rx) = broadcast::channel(4);
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let scope = SubscribeScope {
            clusters: Vec::new().into(),
            events: Arc::new(EventScope::Wildcard),
        };
        let err = run_subscription_once(
            5,
            &native,
            &events,
            &scope,
            &health,
            tokio::time::Instant::now(),
            0,
        )
        .await
        .expect_err("subscribe 失敗は Err で伝播すること");
        assert_eq!(err.kind, mat_core::error::ErrorKind::SessionFailed);
        assert_eq!(
            close_calls.load(Ordering::SeqCst),
            1,
            "CASE 成立後の subscribe 失敗でも close されていること"
        );
    }

    /// 生存実績ありの無音は deadline (90s) で即 teardown → backoff 5s で
    /// 再購読する（probe 延長は Issue #15 の実測で「救済 0/18・再購読
    /// 中央値 9s」= 純損失と判明し撤去 — spec 2026-07-30）。
    #[tokio::test(start_paused = true)]
    async fn proven_silence_tears_down_at_deadline() {
        let est = FakeEstablisher::default();
        let live = Arc::clone(&est.sub_live);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        // 生存実績を作る（proven=true — born-dead ではなく Silence 経路に
        // 乗せる）。値は priming デフォルト（on-off=true）と揃える: 変える
        // と再確立時の priming が差分回復（`classify_against_cache`）に
        // 昇格して `recovered: true` になり、「本物の再確立」検証を汚す。
        live.lock().unwrap().push_back(onoff_report(1, true));
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("live event")
            .unwrap();
        assert!(!ev.priming);
        let t0 = tokio::time::Instant::now();

        // 以後完全無音 → deadline (90s) + backoff 5s の範囲で再購読の
        // priming が届く（probe 延長で 270s まで引き延ばさないこと）。
        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("silence teardown 後の再購読 priming")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(90),
            "deadline より早く購読を殺さないこと: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(120),
            "deadline + backoff 5s の範囲で再購読すること: {elapsed:?}"
        );
    }

    /// 確立に成功したら backoff ラダーがリセットされる。ラダーを 20s まで
    /// 育ててから確立させ、その購読を殺す。リセットされていれば次の再試行は
    /// 5s 後、されていなければ 40s 後 — 15s の閾値で明確に区別できる。
    #[tokio::test(start_paused = true)]
    async fn backoff_resets_after_successful_establishment() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let calls = Arc::clone(&est.calls);
        let fail_next_report = Arc::clone(&est.fail_next_report);
        est.fail_subscription.store(3, Ordering::SeqCst);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        // 3 回失敗（backoff は 20s まで育つ）→ 4 回目で確立。
        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("ラダーを登った先の priming")
            .unwrap();
        assert!(ev.priming);
        // このテストの前提: 3 回失敗して実際にラダーを登ったこと。
        // ここを確認しないと、fail_subscription が何らかの理由で効かなくなり
        // 1 回目の試行がいきなり成功しても（backoff == 0）本テストは
        // 「リセットされた/されていない」のどちらとも見分けがつかず、
        // 何も検証しないまま green で居座ってしまう。
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "失敗 3 + 成功 1 = 4 試行になっていること（このテストの成立前提）"
        );

        // 確立できた購読を殺す。
        let t0 = tokio::time::Instant::now();
        fail_next_report.store(1, Ordering::SeqCst);

        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("再購読の priming")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(15),
            "確立成功で backoff が 5s へリセットされること（未リセットなら 40s）: {elapsed:?}"
        );
    }

    /// 監査#4: matd 稼働中に台帳へ追加されたノードの購読が、次の再読ティック
    /// （60s）で自動的に張られる。従来は起動時スナップショットのみで、稼働中
    /// commission ノードは matd 再起動まで永久に購読されなかった。
    #[tokio::test(start_paused = true)]
    async fn manager_picks_up_node_added_after_start() {
        let (mut rx, _health, dir, _handle) = spawn_manager(FakeEstablisher::default(), None);
        // 起動時から台帳に居る node 5 の priming が届く（初回読みは従来どおり）。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("node5 priming should arrive")
            .unwrap();
        assert_eq!(ev.node_id, 5);
        // 稼働中に node 6 を commission（= 台帳へ追記）。
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 6,
                commissioned_at: "2026-07-27T00:00:00+09:00".into(),
            })
            .unwrap();
        // 次の再読ティック（60s）以内に node 6 の購読が張られ priming が届く。
        // node 5 側のイベントが混ざり得るので node 6 が来るまで読み飛ばす。
        let ev = loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
                .await
                .expect("node6 priming should arrive within one rescan tick")
                .unwrap();
            if ev.node_id == 6 {
                break ev;
            }
        };
        assert!(ev.priming);
    }

    /// レーン B（unpair）: 台帳から消えたノードの購読ループは次の再読ティック
    /// で abort され、status からも消える（従来は「台帳は増える一方」前提で
    /// 永久に再試行し続けた）。
    #[tokio::test(start_paused = true)]
    async fn manager_drops_node_removed_from_ledger() {
        let (mut rx, health, dir, _handle) = spawn_manager(FakeEstablisher::default(), None);
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("node5 priming should arrive")
            .unwrap();
        assert_eq!(ev.node_id, 5);
        assert!(health.status_nodes().iter().any(|n| n["node_id"] == 5));
        // 稼働中に node 5 を unpair（= 台帳から削除）。
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        assert!(store.remove_node(5).unwrap());
        // 次の再読ティックを越える。
        tokio::time::sleep(LEDGER_RESCAN_INTERVAL + std::time::Duration::from_secs(1)).await;
        assert!(
            !health.status_nodes().iter().any(|n| n["node_id"] == 5),
            "removed node must vanish from status: {:?}",
            health.status_nodes()
        );
    }

    /// レーン B 最終レビュー F2: 台帳から消えたノードの購読ループは `abort()`
    /// を「投げっぱなし」にせず join してから health から外す。abort が実際に
    /// 効いたこと（= ループが以後 1 回も establish を試みないこと）を
    /// establisher の呼び出し回数が横ばいであることで主張する。
    #[tokio::test(start_paused = true)]
    async fn manager_stops_the_loop_of_a_removed_node() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                commissioned_at: "2026-09-03T00:00:00+09:00".into(),
            })
            .unwrap();
        // 常に購読確立を失敗させ、ループを backoff リトライで回し続ける
        // （= 生きている限り calls が増え続ける観測可能な状態にする）。
        let est = FakeEstablisher {
            fail_subscription: Arc::new(AtomicUsize::new(usize::MAX)),
            ..Default::default()
        };
        let calls = Arc::clone(&est.calls);
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let state = Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, _rx) = broadcast::channel(64);
        let health = Arc::new(SubHealth::new(None));
        let _handle = spawn_subscription_manager(
            state,
            dir.path().to_path_buf(),
            tx,
            None,
            EventScope::Wildcard,
            Arc::clone(&health),
        );
        // リトライが実際に回っていることを先に確かめる。
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        let before = calls.load(Ordering::SeqCst);
        assert!(before >= 2, "loop should be retrying, calls={before}");
        // 稼働中に unpair（= 台帳から削除）。
        store.remove_node(5).unwrap();
        tokio::time::sleep(LEDGER_RESCAN_INTERVAL + std::time::Duration::from_secs(1)).await;
        assert!(
            !health.status_nodes().iter().any(|n| n["node_id"] == 5),
            "removed node must vanish from status: {:?}",
            health.status_nodes()
        );
        let at_removal = calls.load(Ordering::SeqCst);
        // さらに BACKOFF_MAX を数回跨いでも呼び出しは増えない = ループは死んだ。
        tokio::time::sleep(BACKOFF_MAX * 4).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            at_removal,
            "aborted loop must not attempt any further establish"
        );
        assert!(
            !health.status_nodes().iter().any(|n| n["node_id"] == 5),
            "and must not resurrect the health row: {:?}",
            health.status_nodes()
        );
    }

    /// 監査#4 の副次修正: 起動時に store が読めなくても supervisor は次の
    /// 再読ティックで自己回復する（従来は warn を出して購読ゼロで確定だった）。
    #[tokio::test(start_paused = true)]
    async fn manager_recovers_from_unreadable_store_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        // まだ存在しないパス → 初回 Store::open は store_missing で失敗する。
        let store_path = dir.path().join("store");
        let est = FakeEstablisher::default();
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let state = Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, rx) = broadcast::channel(64);
        let mut rx = AttrRx(rx);
        let health = Arc::new(SubHealth::new(None));
        let _handle = spawn_subscription_manager(
            state,
            store_path.clone(),
            tx,
            None,
            EventScope::Wildcard,
            Arc::clone(&health),
        );
        // supervisor に初回ティック（読み失敗）を踏ませてから store を作る。
        // start_paused の単一スレッド実行では、この sleep の await 中に
        // supervisor タスクが走る。
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let mut store = mat_core::store::Store::open_or_init(&store_path).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 7,
                commissioned_at: "2026-07-27T00:00:00+09:00".into(),
            })
            .unwrap();
        // 次のティック（60s）で購読が張られ priming が届く。
        let ev = loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
                .await
                .expect("node7 priming should arrive after store becomes readable")
                .unwrap();
            if ev.node_id == 7 {
                break ev;
            }
        };
        assert!(ev.priming);
    }

    /// 起動バッチ(>1)はノード毎に STAGGER_STEP ずつずれて確立する（監査⑧）。
    /// priming 到着の仮想時刻差で stagger を観測する。
    #[tokio::test(start_paused = true)]
    async fn initial_batch_staggers_subscriptions() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("store");
        let mut store = mat_core::store::Store::open_or_init(&store_path).unwrap();
        for node_id in [1u64, 2u64] {
            store
                .upsert_node(mat_core::store::NodeRecord {
                    node_id,
                    commissioned_at: "2026-08-03T00:00:00+09:00".into(),
                })
                .unwrap();
        }
        let est = FakeEstablisher::default();
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let state = Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, rx) = broadcast::channel(64);
        let mut rx = AttrRx(rx);
        let health = Arc::new(SubHealth::new(None));
        let _handle = spawn_subscription_manager(
            state,
            store_path.clone(),
            tx,
            None,
            EventScope::Wildcard,
            Arc::clone(&health),
        );
        // 2 ノードぶんの priming 初着時刻（仮想時計）を記録する。
        let mut first_seen: std::collections::HashMap<u64, tokio::time::Instant> =
            std::collections::HashMap::new();
        while first_seen.len() < 2 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
                .await
                .expect("both nodes should prime")
                .unwrap();
            first_seen
                .entry(ev.node_id)
                .or_insert_with(tokio::time::Instant::now);
        }
        // 台帳列挙順は保証されないため、両向きの絶対差で stagger を検証する。
        let (t1, t2) = (first_seen[&1], first_seen[&2]);
        let gap = if t1 >= t2 {
            t1.duration_since(t2)
        } else {
            t2.duration_since(t1)
        };
        assert!(
            gap >= STAGGER_STEP,
            "batch spawn should stagger by STAGGER_STEP, gap={gap:?}"
        );
    }

    /// レジストリの遷移と JSON 形（spec の応答スキーマ nodes 配列）。
    /// tokio::time::Instant なので start_paused + advance で経過秒を決定化できる。
    #[tokio::test(start_paused = true)]
    async fn status_nodes_reflects_lifecycle_transitions() {
        use serde_json::json;
        let h = SubHealth::new(None);
        assert!(h.status_nodes().is_empty());

        // establishing: spawn 直後。
        h.mark_establishing(5);
        tokio::time::advance(Duration::from_secs(2)).await;
        let n = h.status_nodes();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0]["node_id"], 5);
        assert_eq!(n[0]["state"], "establishing");
        assert_eq!(n[0]["for_s"], 2);

        // established: 確立時刻から for_s、受信で last_device_msg_ago_s が縮む。
        h.mark_established(5, 7, 300);
        tokio::time::advance(Duration::from_secs(40)).await;
        h.note_device_msg(5);
        tokio::time::advance(Duration::from_secs(2)).await;
        let n = h.status_nodes();
        assert_eq!(n[0]["state"], "established");
        assert_eq!(n[0]["for_s"], 42);
        assert_eq!(n[0]["subscription_id"], 7);
        assert_eq!(n[0]["max_interval_s"], 300);
        assert_eq!(n[0]["last_device_msg_ago_s"], 2);
        assert_eq!(n[0]["pending_op_ago_s"], serde_json::Value::Null);

        // op 相関 pending が経過秒で載る。
        h.note_op(5, 0x0006);
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(h.status_nodes()[0]["pending_op_ago_s"], 3);

        // down: attempts / backoff_s / last_error（kind は snake_case 名）。
        h.clear_pending(5);
        h.mark_down(
            5,
            tokio::time::Instant::now(),
            3,
            Duration::from_secs(20),
            mat_core::error::MatError::new(mat_core::error::ErrorKind::Unreachable, "no route"),
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        let n = h.status_nodes();
        assert_eq!(n[0]["state"], "down");
        assert_eq!(n[0]["for_s"], 1);
        assert_eq!(n[0]["attempts"], 3);
        assert_eq!(n[0]["backoff_s"], 20);
        assert_eq!(
            n[0]["last_error"],
            json!({"kind": "unreachable", "detail": "no route"})
        );

        // node_id 昇順の安定出力。
        h.mark_establishing(2);
        let n = h.status_nodes();
        assert_eq!(n[0]["node_id"], 2);
        assert_eq!(n[1]["node_id"], 5);
    }

    /// manager 経路の統合: established（priming 到達後）→ down（op 相関死 +
    /// 確立失敗の継続）→ 再 established をレジストリで追える。
    #[tokio::test(start_paused = true)]
    async fn status_nodes_tracks_established_down_reestablished() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let fail_subscription = Arc::clone(&est.fail_subscription);
        let (mut rx, health, _dir, _handles) = spawn_manager(est, None);

        // priming 到達 = established（subscription_id / max_interval は fake の値）。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        let nodes = health.status_nodes();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["node_id"], 5);
        assert_eq!(nodes[0]["state"], "established");
        assert_eq!(nodes[0]["subscription_id"], 1);
        assert_eq!(nodes[0]["max_interval_s"], 60);

        // 以後の確立を失敗させ続けてから op 相関で pump を殺す → down が観測できる。
        fail_subscription.store(1000, Ordering::SeqCst);
        health.note_op(5, 0x0006);
        let mut down = None;
        for _ in 0..300 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let nodes = health.status_nodes();
            if nodes[0]["state"] == "down" {
                down = Some(nodes[0].clone());
                break;
            }
        }
        let down = down.expect("status reaches down");
        // 最初の down は pump 終了理由（op 相関）が last_error に入る。以後の
        // 確立失敗で attempts が増え、last_error は establish 失敗へ置き換わる —
        // どちらを観測するかはタイミング次第なので形だけ釘打ちする。
        assert!(down["for_s"].is_u64());
        assert!(down["attempts"].is_u64());
        assert!(down["backoff_s"].as_u64().unwrap() >= 5);
        assert!(down["last_error"]["kind"].is_string());
        assert!(down["last_error"]["detail"].is_string());

        // 失敗注入を解除 → 再確立で established に戻る。
        fail_subscription.store(0, Ordering::SeqCst);
        let ev = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
            .await
            .expect("re-priming after recovery")
            .unwrap();
        assert!(ev.priming);
        assert_eq!(health.status_nodes()[0]["state"], "established");
    }

    /// note_touched で pump がスライス内に終了し、バックオフ無しで再確立する
    /// （Issue #20）。FakeEstablisher の establish 回数と経過時間で検証する。
    #[tokio::test(start_paused = true)]
    async fn touched_ends_pump_and_resubscribes_without_backoff() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let calls = Arc::clone(&est.calls);
        let (mut rx, health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let t0 = tokio::time::Instant::now();
        health.note_touched(5);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("touched による再確立の priming")
            .unwrap();
        assert!(ev.priming);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "touched が本物の再確立を引き起こしていること"
        );
        let elapsed = t0.elapsed();
        // 検知は PUMP_SLICE(5s) の周期ポーリングに縛られる（cancel-unsafe な
        // next_report を割り込めないため）が、backoff(5s) は挟まない — 挟むなら
        // 10s 以上になるはずなのでその手前で区別する。
        assert!(
            elapsed < Duration::from_secs(10),
            "backoff を挟まずに再確立すること（挟むと PUMP_SLICE+backoff=10s 以上）: {elapsed:?}"
        );
    }

    /// バックオフ睡眠中の note_touched が sleep を打ち切って即再試行する。
    #[tokio::test(start_paused = true)]
    async fn touched_wakes_backoff_sleep() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let calls = Arc::clone(&est.calls);
        // 2 回確立を失敗させ backoff を 5s → 10s へ育てる。
        est.fail_subscription.store(2, Ordering::SeqCst);
        let t0 = tokio::time::Instant::now();
        let (mut rx, health, _dir, _handles) = spawn_manager(est, None);

        // 1 回目の失敗backoff(5s)を消化させ、2 回目の失敗backoff(10s)の
        // 途中（t=8s）で捕まえる。
        tokio::time::sleep(Duration::from_secs(8)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "2 回目の確立試行（失敗）まで進んでいること（本テストの前提）"
        );
        health.note_touched(5);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("touched による再確立の priming")
            .unwrap();
        assert!(ev.priming);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "touched が 3 回目の確立を即座に引き起こしたこと"
        );
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(15),
            "2 回目の backoff(10s) の残り待ち時間を消化しないこと（消化すると t=15s 以降になる）: {elapsed:?}"
        );

        // 使い残しの touched フラグ/Notify permit が次サイクルへ漏れて
        // 即座に再々終了しないこと（PUMP_SLICE×2 生存確認）。
        assert!(
            tokio::time::timeout(PUMP_SLICE * 2, rx.recv())
                .await
                .is_err(),
            "backoff 短絡で消費した touched が使い回されないこと"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "余計な再確立が起きていないこと"
        );
    }

    /// touched フラグは消費後クリアされ、次周回で再発火しない。
    #[tokio::test(start_paused = true)]
    async fn touched_flag_is_consumed_once() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let calls = Arc::clone(&est.calls);
        let (mut rx, health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);

        health.note_touched(5);
        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("touched による再確立の priming")
            .unwrap();
        assert!(ev.priming);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "touched で 1 回だけ再確立されること"
        );

        // touched フラグは消費済み — 以後 PUMP_SLICE×2 生存しても即終了しない。
        assert!(
            tokio::time::timeout(PUMP_SLICE * 2, rx.recv())
                .await
                .is_err(),
            "touched フラグが使い回されて即再終了しないこと"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "余計な再確立が起きていないこと"
        );
    }

    /// イベント行だけを n 件集める（属性行は読み飛ばす）。
    pub(super) async fn collect_event_lines(
        rx: &mut broadcast::Receiver<Emitted>,
        n: usize,
    ) -> Vec<EventItem> {
        let mut out = Vec::new();
        while out.len() < n {
            let got = tokio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .expect("イベント行が届かない")
                .unwrap();
            if let Emitted::Event(item) = got {
                out.push(item);
            }
        }
        out
    }

    /// spec §6.2 の核: 起動直後（最後の EventNumber を知らない）の priming
    /// イベントは `priming: true`。pump が死んで再購読するときは EventMin =
    /// 最後に見た番号 + 1 を載せ、そこで届く priming は盲目窓中の実イベント
    /// なので `priming: false` で流す。
    #[tokio::test(start_paused = true)]
    async fn priming_events_become_real_events_once_event_min_is_known() {
        use mat_controller::im::EventPathIn;
        use mat_native::test_support::switch_press_event;

        let est = FakeEstablisher::default();
        *est.sub_priming_events.lock().unwrap() =
            vec![switch_press_event(12, 1), switch_press_event(10, 1)];
        let priming_events = Arc::clone(&est.sub_priming_events);
        let seen_min = Arc::clone(&est.sub_event_min);
        let seen_paths = Arc::clone(&est.sub_event_paths);
        let fail_next_report = Arc::clone(&est.fail_next_report);
        let (mut rx, health, _dir, _handles) = spawn_manager_with(est, None, EventScope::Wildcard);

        let first = collect_event_lines(&mut rx, 2).await;
        assert!(
            first.iter().all(|e| e.priming),
            "初回 priming は priming: true"
        );
        assert_eq!(
            first.iter().map(|e| e.event_number).collect::<Vec<_>>(),
            vec![10, 12],
            "EventNumber 昇順で流す"
        );
        assert_eq!(*seen_min.lock().unwrap(), None, "初回は EventFilters 無し");
        assert_eq!(
            *seen_paths.lock().unwrap(),
            vec![EventPathIn::WILDCARD_URGENT]
        );
        assert_eq!(health.last_event_number(5), Some(12));

        // 盲目窓中に起きた 2 件（EventMin = 13 以上）を次の priming に用意する。
        *priming_events.lock().unwrap() =
            vec![switch_press_event(13, 1), switch_press_event(15, 1)];
        // 確立の**あと**に注入して走っている pump を殺す = 再購読させる。
        fail_next_report.store(1, std::sync::atomic::Ordering::SeqCst);
        let second = collect_event_lines(&mut rx, 2).await;
        assert!(
            second.iter().all(|e| !e.priming),
            "EventMin を知っている再購読の priming は実イベント: {second:?}"
        );
        assert_eq!(
            *seen_min.lock().unwrap(),
            Some(13),
            "EventMin = 最後に見た番号 + 1"
        );
    }

    /// pump が受けた live イベントは `Emitted::Event` で流れ、同じ ReportData
    /// 由来の属性行と `timestamp` を共有する。番号は last_event_number へ。
    #[tokio::test(start_paused = true)]
    async fn live_events_share_the_report_timestamp_and_advance_event_min() {
        use mat_native::test_support::switch_press_event;

        let est = FakeEstablisher::default();
        let live = Arc::clone(&est.sub_live);
        let live_events = Arc::clone(&est.sub_live_events);
        let (mut rx, health, _dir, _handles) = spawn_manager_with(est, None, EventScope::Wildcard);

        // 確立（priming 属性行）を待ってから live を注入する。
        let priming = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("priming")
            .unwrap();
        assert!(matches!(priming, Emitted::Attribute(e) if e.priming));

        live.lock().unwrap().push_back(onoff_report(1, false));
        live_events
            .lock()
            .unwrap()
            .push_back(vec![switch_press_event(31, 1)]);

        let attr = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("live 属性行")
            .unwrap();
        let Emitted::Attribute(attr) = attr else {
            panic!("属性行が先に来るはず: {attr:?}");
        };
        assert!(!attr.priming);
        let event = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("live イベント行")
            .unwrap();
        let Emitted::Event(event) = event else {
            panic!("イベント行が来るはず: {event:?}");
        };
        assert!(!event.priming);
        assert_eq!(event.event_number, 31);
        assert_eq!(
            event.timestamp, attr.timestamp,
            "同一 ReportData の属性行とイベント行は同じ受信時刻"
        );
        assert_eq!(health.last_event_number(5), Some(31));
    }

    /// デバイスのイベントログが巻き戻った（再起動 / カウンタリセット）ときは、
    /// live report の番号まで記録も戻す。戻さないと以後の再購読は永久に
    /// 満たされない EventMin を送り続け、盲目窓の回収が matd 再起動まで死ぬ。
    #[tokio::test(start_paused = true)]
    async fn live_event_below_the_stored_number_rewinds_event_min() {
        use mat_native::test_support::switch_press_event;

        let est = FakeEstablisher::default();
        // priming で 100 まで見た状態を作る。
        *est.sub_priming_events.lock().unwrap() = vec![switch_press_event(100, 1)];
        let live = Arc::clone(&est.sub_live);
        let live_events = Arc::clone(&est.sub_live_events);
        let (mut rx, health, _dir, _handles) = spawn_manager_with(est, None, EventScope::Wildcard);

        let primed = collect_event_lines(&mut rx, 1).await;
        assert_eq!(primed[0].event_number, 100);
        assert_eq!(health.last_event_number(5), Some(100));

        // デバイスがログを巻き戻したあとの live イベント（番号 5）。
        live.lock().unwrap().push_back(onoff_report(1, false));
        live_events
            .lock()
            .unwrap()
            .push_back(vec![switch_press_event(5, 1)]);

        let live_line = collect_event_lines(&mut rx, 1).await;
        assert_eq!(live_line[0].event_number, 5);
        assert!(!live_line[0].priming);
        assert_eq!(
            health.last_event_number(5),
            Some(5),
            "live の巻き戻りは記録も巻き戻す（次の EventMin = 6）"
        );
    }

    /// デバイスが EventFilters を無視してイベントログ全量を priming で返しても、
    /// 盲目窓の契約は matd 側で守る: EventMin **未満**の priming イベントは
    /// `priming: true` に落とす（捨てない — 消費者は priming 行を無視する契約
    /// なので、落とすとワイヤの事実を隠すことになる）。EventMin **以上**だけが
    /// 盲目窓中の実イベント = `priming: false`。
    #[tokio::test(start_paused = true)]
    async fn priming_events_below_event_min_are_downgraded_to_priming() {
        use mat_native::test_support::switch_press_event;

        let est = FakeEstablisher::default();
        // 1 回目の priming は 12 だけ → last = 12 → 再購読の EventMin = 13。
        *est.sub_priming_events.lock().unwrap() = vec![switch_press_event(12, 1)];
        let priming_events = Arc::clone(&est.sub_priming_events);
        let seen_min = Arc::clone(&est.sub_event_min);
        let fail_next_report = Arc::clone(&est.fail_next_report);
        let (mut rx, health, _dir, _handles) = spawn_manager_with(est, None, EventScope::Wildcard);

        let first = collect_event_lines(&mut rx, 1).await;
        assert_eq!(first[0].event_number, 12);
        assert!(first[0].priming);
        assert_eq!(health.last_event_number(5), Some(12));

        // 2 回目の priming は 12（フィルタ無視の再送）と 13（盲目窓中の実イベント）。
        *priming_events.lock().unwrap() =
            vec![switch_press_event(12, 1), switch_press_event(13, 1)];
        fail_next_report.store(1, std::sync::atomic::Ordering::SeqCst);

        let second = collect_event_lines(&mut rx, 2).await;
        assert_eq!(*seen_min.lock().unwrap(), Some(13));
        assert_eq!(
            second
                .iter()
                .map(|e| (e.event_number, e.priming))
                .collect::<Vec<_>>(),
            vec![(12, true), (13, false)],
            "EventMin 未満は priming: true のまま、以上だけ実イベント: {second:?}"
        );
        assert_eq!(health.last_event_number(5), Some(13));
    }

    /// `events = []`（`EventScope::Off`）は EventRequests / EventFilters を
    /// 出さない = フェーズ A 以前のワイヤに戻る。
    #[tokio::test]
    async fn event_scope_off_sends_no_event_paths_and_no_event_min() {
        let est = FakeEstablisher::default();
        let seen_paths = Arc::clone(&est.sub_event_paths);
        let seen_min = Arc::clone(&est.sub_event_min);
        let (mut rx, _health, _dir, _handles) = spawn_manager_with(est, None, EventScope::Off);

        // priming 属性行が届いた時点で subscribe は呼ばれている。
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("no event within 2s")
            .unwrap();
        assert!(seen_paths.lock().unwrap().is_empty());
        assert_eq!(*seen_min.lock().unwrap(), None);
    }
}
