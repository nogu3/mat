//! op 相関ヘルス表 + 購読ランタイム状態（[`SubHealth`]）と、priming 差分回復の
//! 値キャッシュ判定（[`classify_against_cache`]）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mat_core::error::MatError;

use super::Event;

/// 未確立がこの時間続いたら warn を 1 回出す（弱リンクノードの長期ブラインドを
/// 本番 info/warn レベルで可視化する — 実測で盲目窓が数時間に達した反省）。
const STUCK_WARN_AFTER: Duration = Duration::from_secs(600);

/// 購読ライフサイクル状態（status op が読む）。「ログに出す状態遷移は
/// レジストリにも書く」が規律 — 遷移点は node_subscription_loop /
/// run_subscription_once の既存ログ出力箇所と 1:1。
#[derive(Debug, Clone)]
pub(crate) enum NodeSubStatus {
    /// spawn 直後〜初回確立前のみ。喪失後の再試行中は Down のまま
    /// （attempts が増える — down_since / classify_failure と同じ見方）。
    Establishing { since: tokio::time::Instant },
    /// 購読成立中。last_device_msg はデバイス発メッセージ
    /// （keep-alive 含む）受信のたび更新。
    Established {
        since: tokio::time::Instant,
        subscription_id: u32,
        max_interval_s: u16,
        last_device_msg: tokio::time::Instant,
    },
    /// 確立失敗 or 購読喪失で backoff 中（再確立まで持続）。
    Down {
        since: tokio::time::Instant,
        attempts: u32,
        backoff: Duration,
        last_error: MatError,
    },
}

/// op 相関ヘルス表かつ購読ランタイム状態の共有点: server op 経路（書き手）と
/// 購読 pump（読み手）の共有状態。「状態変更 op が success したのにデバイス発
/// メッセージが来ない」= レポート経路死の証拠、を pending として持つ。また
/// 購読ループの状態遷移（Establishing / Established / Down）を記録し、status op
/// がこれを読んで応答スキーマを組み立てる。ephemeral なランタイム状態のみ
/// （設計ルール4の永続状態には該当しない）。
pub struct SubHealth {
    /// 購読対象クラスタ集合（subscriptions.toml 由来。空 = full wildcard = 全対象）。
    clusters: Vec<u32>,
    /// node_id → 未消化の状態変更 op の時刻。
    pending: Mutex<HashMap<u64, tokio::time::Instant>>,
    /// 属性最終既知値。購読 pump（書き手: priming / live 全イベント）と
    /// server op 経路（読み手: 「この op は本当に値を変えるか」の証明）で共有する。
    /// ephemeral なプロセス内状態のみ（設計ルール4の永続状態には該当しない）。
    values: Mutex<HashMap<ValueKey, serde_json::Value>>,
    /// node_id → そのノードで最後に見た EventNumber。再購読時の EventMin
    /// （= last + 1）で盲目窓中のイベントを回収するためだけに持つ。**プロセス
    /// メモリのみ**で、matd 再起動で消えて priming 全量からやり直す
    /// （設計ルール 4 — KVS 以外の永続状態を持たない、spec §0）。
    event_numbers: Mutex<HashMap<u64, u64>>,
    /// node_id → 購読ライフサイクル状態（status op が読む）。
    status: Mutex<HashMap<u64, NodeSubStatus>>,
    /// node_id → touched フラグ + 起床用 Notify（Issue #20）。pump は
    /// cancel-unsafe なのでフラグ+スライスポーリングで拾い、backoff 睡眠だけ
    /// Notify で起こす。同じ `Mutex<HashMap>` に同居させない理由: flag と
    /// Notify のライフサイクルが pending/status とは別軸（フラグの消費が
    /// Notify の使い回し防止と一体で、専用の消費 API が要る）。
    touched: Mutex<HashMap<u64, TouchedState>>,
}

/// [`SubHealth::touched`] の per-node 状態。
struct TouchedState {
    flag: bool,
    notify: Arc<tokio::sync::Notify>,
}

// SubHealth の毒化 Mutex はデータを回収して続行する: 各テーブルは
// ephemeral な単発 insert/remove のみで guard 跨ぎの複合不変条件が無く、
// 毒化を伝播させて全 hot-path（op 経路 / pump / status op）を panic
// させるより回収が正しい（安定性監査 Tier 3 の保険枠 — この局所ヘルパを
// `mat_controller::sync` へ共通化した）。
use mat_controller::sync::locked;

impl SubHealth {
    pub fn new(clusters: Option<Vec<u32>>) -> Self {
        Self {
            clusters: clusters.unwrap_or_default(),
            pending: Mutex::new(HashMap::new()),
            values: Mutex::new(HashMap::new()),
            event_numbers: Mutex::new(HashMap::new()),
            status: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashMap::new()),
        }
    }

    /// そのノードで最後に見た EventNumber（未知なら None = 再購読でも
    /// EventFilters を出さない = デバイスのログ全量が priming で来る）。
    pub fn last_event_number(&self, node_id: u64) -> Option<u64> {
        locked(&self.event_numbers).get(&node_id).copied()
    }

    /// 観測した EventNumber を記録する。既定は最大値を保つ（1 ReportData 内の
    /// 並び順や、priming と live の交錯で後退させないため）。
    ///
    /// 例外は `live`（購読成立後のデバイス発 report）で番号が既知値より
    /// **小さい**とき: デバイスのイベントログが再起動やカウンタリセットで
    /// 巻き戻ったということなので、記録も巻き戻す。片方向ラッチのままだと
    /// 以後の再購読が永久に満たされない EventMin を送り続け、盲目窓の回収
    /// （spec §6.2）が matd 再起動まで死ぬ。priming は最大値のまま —
    /// EventFilters を無視して全ログを返すデバイスがあるため（`PrimingRule`）。
    pub fn note_event_number(&self, node_id: u64, n: u64, live: bool) {
        use std::collections::hash_map::Entry;
        match locked(&self.event_numbers).entry(node_id) {
            Entry::Occupied(mut e) => {
                let cur = *e.get();
                if n > cur {
                    e.insert(n);
                } else if live && n < cur {
                    tracing::warn!(
                        node_id,
                        stored = cur,
                        observed = n,
                        "live event number went backwards; assuming the device event log reset and lowering EventMin"
                    );
                    e.insert(n);
                }
            }
            Entry::Vacant(e) => {
                e.insert(n);
            }
        }
    }

    /// 状態変更 op が success した。cluster が購読対象なら pending を打つ。
    pub fn note_op(&self, node_id: u64, cluster: u32) {
        if !self.clusters.is_empty() && !self.clusters.contains(&cluster) {
            return;
        }
        locked(&self.pending).insert(node_id, tokio::time::Instant::now());
    }

    /// デバイス発メッセージ（keep-alive 含む）や priming を受けた — pending 解除。
    pub fn clear_pending(&self, node_id: u64) {
        locked(&self.pending).remove(&node_id);
    }

    /// 未消化 op からの経過時間（無ければ None）。
    pub fn pending_elapsed(&self, node_id: u64) -> Option<Duration> {
        locked(&self.pending).get(&node_id).map(|t| t.elapsed())
    }

    /// 直経路 op / cold establish がこのノードのセッションを新設した合図。
    /// FP300 系はレポートを最新セッションへ付け替えるため、購読を即時
    /// 張り直して「最新」を購読セッションに塗り替える（Issue #20、spec
    /// 2026-07-31-node-touched-hint）。pump は cancel-unsafe なので
    /// フラグ+スライスポーリング、バックオフ睡眠だけ Notify で起こす。
    /// 購読が無いノード（pump 不在）でも安全な no-op — フラグは誰も読まない。
    /// 呼び手は dispatch の `node_touched` op（server.rs、外部トリガ）と
    /// `NativeBackend::on_new_session` 経由の内部トリガ（main.rs、Issue #20
    /// 経路2 — cold establish / resend-establish のたび）の2系統。`pub`
    /// なのは後者が bin crate（main.rs）から呼ぶため — `pub(crate)` は
    /// lib crate 内に閉じ、bin/lib で crate 境界が別になる cargo の構成上
    /// main.rs からは見えない。
    pub fn note_touched(&self, node_id: u64) {
        let mut map = locked(&self.touched);
        let entry = map.entry(node_id).or_insert_with(|| TouchedState {
            flag: false,
            notify: Arc::new(tokio::sync::Notify::new()),
        });
        entry.flag = true;
        entry.notify.notify_one();
    }

    /// touched フラグが立っているか（消費はしない — pump_verdict の判定用）。
    /// pub: integration test（socket 越しの node_touched op）が外部から観測する。
    pub fn touched(&self, node_id: u64) -> bool {
        locked(&self.touched).get(&node_id).is_some_and(|s| s.flag)
    }

    /// touched シグナルを消費する。フラグを倒すだけでなく Notify も
    /// 新品へ差し替える — 差し替えないと「pump 実行中に来た note_touched」の
    /// notify_one() permit が Notify に残留し、それより後の無関係な backoff
    /// 待ち（select! の notified()）を横取りして即時起床させてしまう
    /// （バックオフの意味が壊れる）。呼び手は 2 箇所: pump が Touched で
    /// 終わる直前と、backoff 睡眠が touch_notify で短絡起床したとき。
    pub(crate) fn clear_touched(&self, node_id: u64) {
        locked(&self.touched).insert(
            node_id,
            TouchedState {
                flag: false,
                notify: Arc::new(tokio::sync::Notify::new()),
            },
        );
    }

    /// ノード毎 Notify の lazy 生成（backoff 睡眠の起床に使う）。
    pub(crate) fn touch_notify(&self, node_id: u64) -> Arc<tokio::sync::Notify> {
        Arc::clone(
            &locked(&self.touched)
                .entry(node_id)
                .or_insert_with(|| TouchedState {
                    flag: false,
                    notify: Arc::new(tokio::sync::Notify::new()),
                })
                .notify,
        )
    }

    /// pump が受けた 1 イベントをキャッシュへ反映し、差分 priming なら昇格して返す。
    /// listen クライアントの有無と無関係に呼ぶ（状態追跡は購読が生きている限り継続）。
    pub(crate) fn observe(&self, ev: Event) -> Event {
        let mut cache = locked(&self.values);
        classify_against_cache(&mut cache, ev)
    }

    /// 属性の最終既知値（未知なら None）。
    pub(crate) fn cached_value(
        &self,
        node_id: u64,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
    ) -> Option<serde_json::Value> {
        locked(&self.values)
            .get(&(node_id, endpoint, cluster, attribute))
            .cloned()
    }

    /// 購読ループ spawn（初回確立前）。
    pub(crate) fn mark_establishing(&self, node_id: u64) {
        locked(&self.status).insert(
            node_id,
            NodeSubStatus::Establishing {
                since: tokio::time::Instant::now(),
            },
        );
    }

    /// 購読成立（「subscription established」ログと同時に呼ぶ）。
    pub(crate) fn mark_established(&self, node_id: u64, subscription_id: u32, max_interval_s: u16) {
        let now = tokio::time::Instant::now();
        locked(&self.status).insert(
            node_id,
            NodeSubStatus::Established {
                since: now,
                subscription_id,
                max_interval_s,
                last_device_msg: now,
            },
        );
    }

    /// デバイス発メッセージ受信（keep-alive 含む）。Established のときだけ更新。
    pub(crate) fn note_device_msg(&self, node_id: u64) {
        if let Some(NodeSubStatus::Established {
            last_device_msg, ..
        }) = locked(&self.status).get_mut(&node_id)
        {
            *last_device_msg = tokio::time::Instant::now();
        }
    }

    /// 確立失敗 or 購読喪失（「subscription lost」/ 失敗ログと同時に呼ぶ）。
    /// since はダウン起点（down_since）、attempts はダウン以降の失敗数。
    pub(crate) fn mark_down(
        &self,
        node_id: u64,
        since: tokio::time::Instant,
        attempts: u32,
        backoff: Duration,
        last_error: MatError,
    ) {
        locked(&self.status).insert(
            node_id,
            NodeSubStatus::Down {
                since,
                attempts,
                backoff,
                last_error,
            },
        );
    }

    /// 台帳から消えたノードの痕跡を全テーブルから除く（購読ループ abort と
    /// 同時に呼ぶ）。以後 `status_nodes` に現れない。
    pub(crate) fn forget(&self, node_id: u64) {
        locked(&self.pending).remove(&node_id);
        locked(&self.status).remove(&node_id);
        locked(&self.touched).remove(&node_id);
        locked(&self.event_numbers).remove(&node_id);
        locked(&self.values).retain(|k, _| k.0 != node_id);
    }

    /// 購読対象クラスタ（status 応答用）。空 = full wildcard = None
    /// （subscribe_config は空リストを起動拒否するので混同はない）。
    pub(crate) fn clusters(&self) -> Option<&[u32]> {
        if self.clusters.is_empty() {
            None
        } else {
            Some(&self.clusters)
        }
    }

    /// status 応答の nodes 配列（node_id 昇順の安定出力）。期間は全て
    /// 「今からの経過秒」— 内部時計は tokio::time::Instant で ISO 変換
    /// 不能なため、経過秒が正直な表現（spec）。
    pub(crate) fn status_nodes(&self) -> Vec<serde_json::Value> {
        let status = locked(&self.status);
        let mut ids: Vec<u64> = status.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .map(|id| match &status[&id] {
                NodeSubStatus::Establishing { since } => serde_json::json!({
                    "node_id": id,
                    "state": "establishing",
                    "for_s": since.elapsed().as_secs(),
                }),
                NodeSubStatus::Established {
                    since,
                    subscription_id,
                    max_interval_s,
                    last_device_msg,
                } => serde_json::json!({
                    "node_id": id,
                    "state": "established",
                    "for_s": since.elapsed().as_secs(),
                    "subscription_id": subscription_id,
                    "max_interval_s": max_interval_s,
                    "last_device_msg_ago_s": last_device_msg.elapsed().as_secs(),
                    // 未消化の状態変更 op（op 相関）。通常 null、値が入って
                    // いれば「op 成功後デバイス発ゼロ」を観測中の瞬間。
                    "pending_op_ago_s": self.pending_elapsed(id).map(|d| d.as_secs()),
                }),
                NodeSubStatus::Down {
                    since,
                    attempts,
                    backoff,
                    last_error,
                } => serde_json::json!({
                    "node_id": id,
                    "state": "down",
                    "for_s": since.elapsed().as_secs(),
                    "attempts": attempts,
                    "backoff_s": backoff.as_secs(),
                    "last_error": {
                        "kind": last_error.kind,
                        "detail": last_error.detail,
                    },
                }),
            })
            .collect()
    }
}

/// 確立失敗ログの出し分け（純関数 — 時計はループ側が持つ）。
/// 毎試行 info は常駐ノイズ（弱リンクはバックオフ上限 60s 毎に永久に失敗し
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
pub(crate) fn classify_against_cache(
    cache: &mut HashMap<ValueKey, serde_json::Value>,
    ev: Event,
) -> Event {
    let key = (ev.node_id, ev.endpoint, ev.cluster, ev.attribute);
    if cache.get(&key).is_some_and(|prev| *prev == ev.value) {
        return ev;
    }
    let prev = cache.insert(key, ev.value.clone());
    if let Some(prev) = prev.filter(|_| ev.priming) {
        // 昇格は journal だけで追えるように INFO で残す（issue #19 — 盲目期間
        // 中の遷移が消費者へ届いたかの診断で、昇格の有無を間接推定させない）。
        tracing::info!(
            node_id = ev.node_id,
            endpoint = ev.endpoint,
            cluster = ev.cluster,
            attribute = ev.attribute,
            old = %prev,
            new = %ev.value,
            "priming diff promoted to recovered event"
        );
        return Event {
            priming: false,
            recovered: true,
            ..ev
        };
    }
    ev
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// recovered 昇格は INFO ログで直接確認できる（issue #19: 診断時に
    /// journal だけで昇格の有無・旧値→新値を追えるようにする）。
    /// 非昇格（初見・同値）ではログを出さない。
    #[test]
    fn classify_promotion_emits_info_log_with_old_and_new_values() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }

        fn ev(value: serde_json::Value) -> Event {
            Event {
                timestamp: "2026-07-27T00:00:00+09:00".to_string(),
                node_id: 42,
                endpoint: 1,
                cluster: 0x0406,
                attribute: 0x0000,
                value,
                priming: true,
                recovered: false,
            }
        }

        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let mut cache: HashMap<ValueKey, serde_json::Value> = HashMap::new();
        tracing::subscriber::with_default(subscriber, || {
            let _ = classify_against_cache(&mut cache, ev(json!(0))); // 初見: ログ無し
            let _ = classify_against_cache(&mut cache, ev(json!(0))); // 同値: ログ無し
            let _ = classify_against_cache(&mut cache, ev(json!(1))); // 昇格: INFO
        });

        let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            log.matches("recovered").count(),
            1,
            "昇格 1 回につき 1 行だけ: {log}"
        );
        for needle in [
            "node_id=42",
            "cluster=1030",
            "attribute=0",
            "old=0",
            "new=1",
        ] {
            assert!(log.contains(needle), "{needle} が無い: {log}");
        }
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

    /// 監査 Tier 3: 保持スレッドの panic で Mutex が毒化しても、SubHealth の
    /// 全経路（op 相関 / touched / 値キャッシュ / status レジストリ）は panic
    /// せず動き続ける。中身は ephemeral な健全性テーブルのみで複合不変条件が
    /// 無く、毒化の巻き添えで matd の hot-path 全部を落とす方が実害が大きい。
    #[test]
    fn subhealth_survives_poisoned_locks() {
        use serde_json::json;
        use std::panic::{catch_unwind, AssertUnwindSafe};

        fn poison<T>(m: &Mutex<T>) {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let _guard = m.lock().unwrap();
                panic!("poison lock for test");
            }));
        }

        let h = SubHealth::new(None);
        poison(&h.pending);
        poison(&h.values);
        poison(&h.status);
        poison(&h.touched);

        // pending（op 相関）。
        h.note_op(5, 0x0006);
        assert!(h.pending_elapsed(5).is_some());
        h.clear_pending(5);
        assert!(h.pending_elapsed(5).is_none());

        // touched（Issue #20 ヒント）。
        h.note_touched(5);
        assert!(h.touched(5));
        let _notify = h.touch_notify(5);
        h.clear_touched(5);
        assert!(!h.touched(5));

        // values（最終既知値キャッシュ）。
        let ev = Event {
            timestamp: "2026-08-05T00:00:00+09:00".to_string(),
            node_id: 5,
            endpoint: 1,
            cluster: 0x0006,
            attribute: 0x0000,
            value: json!(true),
            priming: false,
            recovered: false,
        };
        let _ = h.observe(ev);
        assert_eq!(h.cached_value(5, 1, 0x0006, 0x0000), Some(json!(true)));

        // status レジストリ（status op）。
        h.mark_establishing(5);
        h.mark_established(5, 7, 300);
        h.note_device_msg(5);
        h.mark_down(
            5,
            tokio::time::Instant::now(),
            1,
            Duration::from_secs(5),
            mat_core::error::MatError::new(mat_core::error::ErrorKind::Unreachable, "no route"),
        );
        let n = h.status_nodes();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0]["state"], "down");
    }

    /// clusters(): 空 = full wildcard = None、非空はそのまま。
    #[test]
    fn clusters_exposes_narrowing_none_for_wildcard() {
        assert!(SubHealth::new(None).clusters().is_none());
        assert_eq!(
            SubHealth::new(Some(vec![0x0006, 0x0406])).clusters(),
            Some(&[0x0006u32, 0x0406][..])
        );
    }

    /// 最後に見た EventNumber は最大値を保つ（順不同で届いても後退しない）。
    /// forget で痕跡ごと消える = 再購読は priming 全量からやり直す。
    #[test]
    fn note_event_number_keeps_the_max() {
        let h = SubHealth::new(None);
        assert_eq!(h.last_event_number(5), None);
        h.note_event_number(5, 10, false);
        h.note_event_number(5, 7, false);
        assert_eq!(h.last_event_number(5), Some(10));
        h.note_event_number(5, 11, false);
        assert_eq!(h.last_event_number(5), Some(11));
        h.forget(5);
        assert_eq!(h.last_event_number(5), None);
    }

    /// live report の番号が既知値より小さい = デバイスのイベントログが
    /// 巻き戻った（再起動 / カウンタリセット）。片方向ラッチのままだと以後の
    /// 再購読が満たされない EventMin を送り続けるので、記録も巻き戻す。
    /// priming は最大値のまま（EventFilters 無視デバイス対策）。
    #[test]
    fn note_event_number_rewinds_only_for_live_reports() {
        let h = SubHealth::new(None);
        h.note_event_number(5, 100, false);
        h.note_event_number(5, 5, false);
        assert_eq!(h.last_event_number(5), Some(100), "priming は後退させない");
        h.note_event_number(5, 5, true);
        assert_eq!(
            h.last_event_number(5),
            Some(5),
            "live の巻き戻りは記録も巻き戻す"
        );
        // 巻き戻した後も通常の最大値ラッチに戻る。
        h.note_event_number(5, 6, true);
        assert_eq!(h.last_event_number(5), Some(6));
    }
}
