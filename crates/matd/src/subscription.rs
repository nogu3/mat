//! matd 常駐 Subscribe（spec: 2026-07-20-matd-subscribe-listen-design.md ②）。
//!
//! supervisor が `LEDGER_RESCAN_INTERVAL`（60s）ごとに台帳を読み直し、
//! 新規ノードへ購読ループを spawn（監査#4）。ノードごと: resolve（常駐 mDNS
//! キャッシュ）→ 専用 CASE → wildcard Subscribe → ポンプ。失敗・死亡は指数
//! backoff（5s 開始、上限 60s）で再購読。
//! イベントは `tokio::sync::broadcast` で listen 接続へ配る。
//! 状態は持たない（リングバッファ/リプレイ無し — 聞いている間だけ届く契約）。
//! op 相関 + 無音 deadline = max_interval+30s の死活判定（spec 2026-07-21-matd-borndead-detection。teardown 前の probe 延長は実測で純損失と判明し撤去 — spec 2026-07-30）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::broadcast;

use mat_controller::im::ReportDataMessage;
use mat_core::error::MatError;
use mat_core::output::now_iso8601;
use mat_core::store::Store;

use crate::server::NativeState;

/// 再購読 backoff の初期値 / 上限。上限は当初 300s だったが、リンク回復後に
/// 最大 5 分無試行 = センサーの照明 1 回分不発になるため 60s へ短縮
/// （issue #15、blind 実測 1 日 3.7 時間の主因の一つ）。
const BACKOFF_INITIAL: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

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
    /// note_touched: 直経路 op / cold establish がこのノードのセッションを
    /// 新設した合図（Issue #20）。他の全終了理由より優先して判定する —
    /// 「セッションが塗り替えられた」ことは既に確定しているので、無音 /
    /// op 相関の判定を待つ意味がない。
    Touched,
    /// 状態変更 op から OP_GRACE 経過してもデバイス発ゼロ（op 相関の born-dead 検知）。
    OpGrace { since_op: Duration },
    /// 確立以降デバイス発ゼロのまま無音 deadline 超過（born-dead）。
    BornDeadSilence,
    /// 生存実績のあと無音 deadline 超過（通常の購読死）。
    Silence,
}

/// pump を殺すべきか判定する（純関数 — 時計は pump が持つ）。
/// touched を最優先、次いで op 相関を無音 deadline より先に評価する
/// （そちらが常に早く満ちるため）。
pub(crate) fn pump_verdict(
    touched: bool,
    proven: bool,
    since_last_msg: Duration,
    deadline: Duration,
    pending_op: Option<Duration>,
) -> Option<PumpEnd> {
    if touched {
        return Some(PumpEnd::Touched);
    }
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
    /// node_id → 購読ライフサイクル状態（status op が読む）。
    status: Mutex<HashMap<u64, NodeSubStatus>>,
    /// node_id → touched フラグ + 起床用 Notify（Issue #20）。pump は
    /// cancel-unsafe なのでフラグ+スライスポーリングで拾い、backoff 睡眠だけ
    /// Notify で起こす。同じ Mutex<HashMap> に同居させない理由: flag と
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
            status: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashMap::new()),
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
        // list 属性は要素ごとに別々の report として届くことがある
        // (AttributePathIB.ListIndex = null → デコーダは list_append: true)。
        // その 1 要素は scalar なので下の is_array/is_object 判定を素通りして
        // しまう — list_append 単体で落とす（list-diff priming recovery が
        // 同一キーへの要素ごとの値変化を実遷移と誤認して recovered を大量発生
        // させる害の元。README の「list/struct attributes are dropped」契約どおり）。
        if rep.list_append {
            tracing::debug!(
                node_id,
                endpoint,
                cluster,
                attribute,
                "dropping list-append element report"
            );
            continue;
        }
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

/// 指数 backoff: 5s 開始、倍々、上限 60s。
pub(crate) fn next_backoff(cur: Duration) -> Duration {
    if cur.is_zero() {
        BACKOFF_INITIAL
    } else {
        (cur * 2).min(BACKOFF_MAX)
    }
}

/// backoff の実 sleep に乗せるジッタ: cap 適用後の名目値 × [0.75, 1.25)。
/// cap 後に掛けるので、長期障害で全ノードが BACKOFF_MAX に飽和しても実待ちは
/// 45〜75s に散り続け、リトライ波が再同期しない（cap 前に掛けると飽和ノードが
/// 全員ちょうど 60s で再同期する — 監査⑧）。`mark_down` / status の表示は
/// 名目値のまま（表示はエンベロープの説明であって実 sleep の予告ではない）。
pub(crate) fn jittered_backoff(nominal: Duration, r: f64) -> Duration {
    nominal.mul_f64(0.75 + 0.5 * r)
}

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

/// commissioned 全ノードへ購読タスクを張る supervisor を起動する。
/// `LEDGER_RESCAN_INTERVAL` ごとに台帳を読み直し、新規ノードに購読ループを
/// 追加 spawn する（op 経路の `require_node` が毎回 store を開き直すのと同じ
/// 「常駐中の台帳更新を拾う」規律）。ノード削除は台帳 API に存在しないため
/// 扱わない。cluster 絞り込みは subscriptions.toml で実装済み（`clusters`
/// パラメータに配線）。native が Unavailable なら何もしない（`mat fabric
/// init` 後の再起動で解消 — 再読で直る状態ではないので空回りさせない）。
pub fn spawn_subscription_manager(
    native: Arc<NativeState>,
    store_path: PathBuf,
    events: broadcast::Sender<Event>,
    clusters: Option<Vec<u32>>,
    health: Arc<SubHealth>,
) -> tokio::task::JoinHandle<()> {
    // None = subscriptions.toml 無し = full wildcard（空 slice がワイヤ上の wildcard 形）。
    let clusters: Arc<[u32]> = clusters.unwrap_or_default().into();
    tokio::spawn(async move {
        if !matches!(&*native, NativeState::Ready(_)) {
            return;
        }
        // 購読ループを張った node_id。台帳は増える一方（削除 API 無し）なので
        // 集合の縮小は考えない。
        let mut subscribed = HashSet::new();
        let mut announced = false;
        let mut read_fail_streak: u32 = 0;
        loop {
            match Store::open(&store_path) {
                Ok(store) => {
                    read_fail_streak = 0;
                    let node_ids: Vec<u64> = store.nodes().map(|n| n.node_id).collect();
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
                        .filter(|id| subscribed.insert(*id))
                        .collect();
                    for (i, node_id) in new_nodes.iter().copied().enumerate() {
                        if !initial {
                            tracing::info!(node_id, "ledger rescan: new node; subscribing");
                        }
                        let delay = stagger_delay(i, new_nodes.len());
                        let native = Arc::clone(&native);
                        let events = events.clone();
                        let clusters = Arc::clone(&clusters);
                        let health = Arc::clone(&health);
                        tokio::spawn(async move {
                            node_subscription_loop(node_id, delay, native, events, clusters, health)
                                .await
                        });
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

/// 1 ノードの購読ループ。確立 → priming 配信 → ポンプ。失敗・死亡は backoff 再購読。
/// ストリーク初回失敗は info、未確立 10 分で warn 1 回、以降リトライは debug、確立/喪失は info
/// （弱リンクノードを常駐ノイズにしない規律は不変）。
async fn node_subscription_loop(
    node_id: u64,
    initial_delay: Duration,
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
    health.mark_establishing(node_id);
    if !initial_delay.is_zero() {
        // 起動バッチの stagger（監査⑧）。establishing 表示にしてから待つ —
        // status に現れない 12 秒を作らない。
        tracing::debug!(
            node_id,
            delay_s = initial_delay.as_secs(),
            "staggering initial subscribe"
        );
        tokio::time::sleep(initial_delay).await;
    }
    loop {
        let last_error = match run_subscription_once(
            node_id, backend, &events, &clusters, &health, down_since, failures,
        )
        .await
        {
            Ok(reason) => {
                // 購読が成立して喪失した: 状態遷移なので info、状態リセット。
                tracing::info!(node_id, "subscription lost; resubscribing");
                backoff = Duration::ZERO;
                down_since = tokio::time::Instant::now();
                failures = 0;
                warned = false;
                // Touched は「セッションが塗り替えられた」ことが確定している
                // 喪失 — バックオフで待つ理由がない（Issue #20）。文字列
                // prefix で運ぶのは、戻り値を enum 化するより既存コードへの
                // 摩擦が小さいため（reason は元々ログ/last_error 用の人間可読
                // 文字列で、enum 化すると呼び出し全箇所の型が変わる）。
                if reason.starts_with("touched:") {
                    continue;
                }
                MatError::new(mat_core::error::ErrorKind::Other, reason)
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
                e
            }
        };
        backoff = next_backoff(backoff);
        health.mark_down(node_id, down_since, failures, backoff, last_error);
        // sleep(backoff) は cancel-safe（pump の next_report と違い、ここは
        // 途中で打ち切っても失うステートが無い）なので、backoff 中に来た
        // note_touched はここで select! で拾って即座に再試行へ回す
        // （Issue #20）。起床側で touched を消費する — 消費しないと、この
        // 起床が使い切ったはずの touched シグナルが次の run_subscription_once
        // 先頭の health.touched() 判定に残り、確立直後の pump を無条件で
        // Touched 即終了させてしまう（無限に近い張り直しループになる）。
        let touch_notify = health.touch_notify(node_id);
        let sleep_dur = jittered_backoff(backoff, mat_controller::exchange::unit_random());
        tokio::select! {
            _ = tokio::time::sleep(sleep_dur) => {}
            _ = touch_notify.notified() => {
                backoff = Duration::ZERO;
                health.clear_touched(node_id);
            }
        }
    }
}

/// 1 回の購読試行。確立+Subscribe 成立まで到達したら Ok(reason) を返して抜ける
/// （ポンプ死亡=正常喪失。reason は pump 終了理由の人間可読文字列で、呼び手が
/// `Down.last_error` の detail に使う）。確立前の失敗は Err。
async fn run_subscription_once(
    node_id: u64,
    backend: &crate::native::NativeBackend,
    events: &broadcast::Sender<Event>,
    clusters: &[u32],
    health: &SubHealth,
    down_since: tokio::time::Instant,
    prior_failures: u32,
) -> Result<String, mat_core::error::MatError> {
    let mut conn = backend.establish_subscription(node_id).await?;
    let (info, priming) = match conn.subscribe_wildcard(clusters).await {
        Ok(v) => v,
        Err(e) => {
            // CASE は成立済み — 放置すると Issue #20 の黙殺経路になる
            // （establish 自体の失敗はセッションが無いので close 不要、`?` のまま）。
            conn.close().await;
            return Err(e);
        }
    };
    tracing::info!(
        node_id,
        subscription_id = info.subscription_id,
        max_interval_s = info.max_interval_s,
        down_s = down_since.elapsed().as_secs(),
        attempts = prior_failures + 1,
        "subscription established"
    );
    health.mark_established(node_id, info.subscription_id, info.max_interval_s);
    // priming は現在状態の全量 — down 中の op はここで配信されるので pending 解除。
    health.clear_pending(node_id);
    for msg in &priming {
        for ev in events_from_report(node_id, msg, true) {
            // 盲目期間中に起きた実遷移はここで通常イベントへ昇格する。
            let _ = events.send(health.observe(ev)); // 受信者ゼロは正常（listen 接続なし）
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
    // pump 終了理由は loop の外まで持ち出して、末尾で必ず close してから返す
    // （Issue #20: どの終了経路でも死んだセッションを放置しない）。
    let reason = loop {
        if let Some(end) = pump_verdict(
            health.touched(node_id),
            proven,
            last_msg.elapsed(),
            deadline,
            health.pending_elapsed(node_id),
        ) {
            // 再購読直後に同じ pending で即再発火しないよう先に消す。
            health.clear_pending(node_id);
            match end {
                PumpEnd::Touched => {
                    // フラグ消費は「touched: ...」腕の中だけで行う — pump が
                    // Touched 以外の理由（op 相関 / 無音）で終わったときに
                    // 誤って隣の touched シグナルを消してしまわないため。
                    health.clear_touched(node_id);
                    tracing::info!(
                        node_id,
                        "report pump ended (touched: direct-path session superseded)"
                    );
                    break "touched: direct-path session superseded".to_string();
                }
                PumpEnd::OpGrace { since_op } => {
                    tracing::info!(
                        node_id,
                        since_op_s = since_op.as_secs(),
                        "report pump ended (op-correlated: no device message after op)"
                    );
                    break format!(
                        "op-correlated: no device message {}s after op",
                        since_op.as_secs()
                    );
                }
                PumpEnd::BornDeadSilence => {
                    tracing::info!(
                        node_id,
                        silent_s = last_msg.elapsed().as_secs(),
                        "report pump ended (born-dead: no device message since establishment)"
                    );
                    break format!(
                        "born-dead: no device message since establishment ({}s silent)",
                        last_msg.elapsed().as_secs()
                    );
                }
                PumpEnd::Silence => {
                    tracing::info!(
                        node_id,
                        silent_s = last_msg.elapsed().as_secs(),
                        "report pump ended (silence past deadline)"
                    );
                    break format!("silence past deadline ({}s)", last_msg.elapsed().as_secs());
                }
            }
        }
        let remaining = deadline.saturating_sub(last_msg.elapsed());
        let slice = PUMP_SLICE.min(remaining);
        match conn.next_report(slice).await {
            Ok(Some(msg)) => {
                proven = true;
                last_msg = tokio::time::Instant::now();
                health.clear_pending(node_id);
                health.note_device_msg(node_id);
                for ev in events_from_report(node_id, &msg, false) {
                    let _ = events.send(health.observe(ev));
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
                break format!("pump ended: {}", e.detail);
            }
        }
    };
    conn.close().await;
    Ok(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_native::test_support::{onoff_report, FakeEstablisher};
    use serde_json::json;

    /// node 5 だけの台帳と fake establisher で購読マネージャを起動する共通足場。
    ///
    /// 戻り値の `TempDir` は**テスト側が束縛して生かし続ける**こと（`_dir` は可、
    /// `_` は不可 — `_` は即 drop され store ごと消える）。`JoinHandle` も同様に
    /// 束縛しておく（既存テストの寿命の握り方をそのまま踏襲）。
    fn spawn_manager(
        est: FakeEstablisher,
        clusters: Option<Vec<u32>>,
    ) -> (
        broadcast::Receiver<Event>,
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
            Arc::clone(&health),
        );
        (rx, health, dir, handle)
    }

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
        // list 要素として届く scalar（AttributePathIB.ListIndex=null → デコーダは
        // list_append: true として表現）はイベント化せず捨てる。値そのものは
        // scalar（JSON array/object ではない）なので is_array/is_object 判定は
        // 素通りしてしまう — list_append フラグでの判定が必要。
        msg.reports.push(mat_controller::im::AttributeReport {
            endpoint: Some(1),
            cluster: Some(0x0008),
            attribute: Some(0xFFFB), // attribute-list
            list_append: true,
            data: Some(json!(65531)),
            status: None,
        });
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
        // 唯一残るのは onoff_report が積んだ通常の scalar（list_append: false）。
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].node_id, 7);
        assert_eq!(evs[0].cluster, 0x0006);
        assert_eq!(evs[0].value, json!(true));
        assert!(evs[0].priming);
    }

    /// このバグが差分回復へ及ぼしていた害の釘打ち: list 属性は要素ごとに別々の
    /// AttributeReport（list_append: true, data: scalar）として届き、同一キー
    /// (node/endpoint/cluster/attribute) に対して値が要素ごとに違う。もし
    /// list_append 要素をイベント化してしまうと、classify_against_cache が
    /// 「値が変わった priming」と誤認して recovered: true を大量発生させる
    /// （実機 node 13 の再購読 priming で観測: onoff/attribute-list 等）。
    /// events_from_report が list_append 要素を落とす限り、SubHealth::observe
    /// を通しても recovered イベントは 1 つも出ない。
    #[test]
    fn list_append_elements_never_produce_recovered_events() {
        let health = SubHealth::new(None);
        let round = |values: &[i64]| {
            let msg = mat_controller::im::ReportDataMessage {
                reports: values
                    .iter()
                    .map(|&v| mat_controller::im::AttributeReport {
                        endpoint: Some(1),
                        cluster: Some(0x0006),
                        attribute: Some(0xFFFB), // attribute-list
                        list_append: true,
                        data: Some(json!(v)),
                        status: None,
                    })
                    .collect(),
                subscription_id: Some(1),
                more_chunks: false,
                suppress_response: false,
            };
            events_from_report(13, &msg, true)
                .into_iter()
                .map(|ev| health.observe(ev))
                .collect::<Vec<_>>()
        };
        // 1 回目の priming（要素 0, 1）: 同一キーに異なる値が複数届く。
        let first = round(&[0, 1]);
        // 再購読後の 2 回目の priming（要素の値も変わり得る = list append の実態）。
        let second = round(&[0, 1, 65533]);
        assert!(
            first.iter().all(|e| !e.recovered) && second.iter().all(|e| !e.recovered),
            "list_append 要素起因の偽 recovered は出ない: first={first:?} second={second:?}"
        );
        // list_append 要素はそもそもイベント化されない（events_from_report の契約）。
        assert!(first.is_empty() && second.is_empty());
    }

    #[test]
    fn backoff_doubles_from_5s_capped_at_60s() {
        use std::time::Duration;
        assert_eq!(next_backoff(Duration::ZERO), Duration::from_secs(5));
        assert_eq!(
            next_backoff(Duration::from_secs(5)),
            Duration::from_secs(10)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(40)),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    /// backoff jitter: cap 後の名目値 × [0.75, 1.25)。中央値（r=0.5）は名目値
    /// のまま = 設計軌道（down_s 中央値 7-9s）を変えない。
    #[test]
    fn jittered_backoff_range_preserves_median() {
        let n = Duration::from_secs(60);
        assert_eq!(jittered_backoff(n, 0.0), Duration::from_secs(45));
        assert_eq!(jittered_backoff(n, 0.5), n);
        assert!(jittered_backoff(n, 0.999_999) < Duration::from_secs(75));
        assert_eq!(jittered_backoff(Duration::ZERO, 0.7), Duration::ZERO);
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
    /// subscribe_wildcard まで届く（絞り込みの配線の釘打ち）。
    #[tokio::test]
    async fn manager_passes_clusters_to_subscribe() {
        let est = FakeEstablisher::default();
        let seen = Arc::clone(&est.sub_clusters);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, Some(vec![0x0006, 0x0406]));

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
        assert!(pump_verdict(false, true, Duration::from_secs(10), dl, None).is_none());
        // op から OP_GRACE 未満はまだ待つ。
        assert!(pump_verdict(
            false,
            true,
            Duration::from_secs(10),
            dl,
            Some(Duration::from_secs(9))
        )
        .is_none());
        // op から OP_GRACE 経過でデバイス発ゼロ → op 相関死。
        assert!(matches!(
            pump_verdict(
                false,
                true,
                Duration::from_secs(15),
                dl,
                Some(Duration::from_secs(10))
            ),
            Some(PumpEnd::OpGrace { .. })
        ));
        // 無音 deadline 超過: 生存実績なし → born-dead、あり → 通常無音死。
        assert!(matches!(
            pump_verdict(false, false, Duration::from_secs(330), dl, None),
            Some(PumpEnd::BornDeadSilence)
        ));
        assert!(matches!(
            pump_verdict(false, true, Duration::from_secs(330), dl, None),
            Some(PumpEnd::Silence)
        ));
        // touched は他の全条件より優先される — op 相関/無音条件を同時に
        // 満たしていても Touched が勝つ（Issue #20）。
        assert!(matches!(
            pump_verdict(
                true,
                true,
                Duration::from_secs(330),
                dl,
                Some(Duration::from_secs(10))
            ),
            Some(PumpEnd::Touched)
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

    /// subscribe_wildcard が失敗したとき（CASE は成立済み）も close される
    /// （Issue #20）。establish_subscription 自体の失敗は CASE 未成立なので
    /// close 不要 — この経路とは区別する。
    #[tokio::test]
    async fn subscribe_failure_closes_session() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher {
            fail_wildcard: true,
            ..Default::default()
        };
        let close_calls = Arc::clone(&est.sub_close_calls);
        let health = Arc::new(SubHealth::new(None));
        let (events, _rx) = broadcast::channel(4);
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let err = run_subscription_once(
            5,
            &native,
            &events,
            &[],
            &health,
            tokio::time::Instant::now(),
            0,
        )
        .await
        .expect_err("subscribe_wildcard 失敗は Err で伝播すること");
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
        let (tx, mut rx) = broadcast::channel(64);
        let health = Arc::new(SubHealth::new(None));
        let _handle =
            spawn_subscription_manager(state, store_path.clone(), tx, None, Arc::clone(&health));
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
        let (tx, mut rx) = broadcast::channel(64);
        let health = Arc::new(SubHealth::new(None));
        let _handle =
            spawn_subscription_manager(state, store_path.clone(), tx, None, Arc::clone(&health));
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

    /// clusters(): 空 = full wildcard = None、非空はそのまま。
    #[test]
    fn clusters_exposes_narrowing_none_for_wildcard() {
        assert!(SubHealth::new(None).clusters().is_none());
        assert_eq!(
            SubHealth::new(Some(vec![0x0006, 0x0406])).clusters(),
            Some(&[0x0006u32, 0x0406][..])
        );
    }
}
