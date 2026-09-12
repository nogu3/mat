//! 1 ノードの購読ループ（[`node_subscription_loop`]）: resolve → 専用 CASE →
//! wildcard Subscribe → ポンプ。失敗・死亡は指数 backoff で再購読、op 相関 +
//! 無音 deadline の死活判定は純関数 [`pump_verdict`]。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use mat_core::error::MatError;
use mat_core::output::now_iso8601;

use crate::server::NativeState;

use super::events::{events_from_event_reports, events_from_report, events_from_report_at};
use super::{classify_failure, Emitted, FailureLog, SubHealth, SubscribeScope};

/// 再購読 backoff の初期値 / 上限。上限は当初 300s だったが、リンク回復後に
/// 最大 5 分無試行 = センサーの照明 1 回分不発になるため 60s へ短縮
/// （issue #15、blind 実測 1 日 3.7 時間の主因の一つ）。
const BACKOFF_INITIAL: Duration = Duration::from_secs(5);
pub(super) const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// pump の受信待ち 1 スライス。op 相関検知（SubHealth）をこの周期で確認する。
/// `next_report` は recv → screen → StatusResponse の多段 await で cancel-safe
/// でないため、`select!` ではなくスライスで刻む（spec §1）。
pub(super) const PUMP_SLICE: Duration = Duration::from_secs(5);
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

/// 1 ノードの購読ループ。確立 → priming 配信 → ポンプ。失敗・死亡は backoff 再購読。
/// ストリーク初回失敗は info、未確立 10 分で warn 1 回、以降リトライは debug、確立/喪失は info
/// （弱リンクノードを常駐ノイズにしない規律は不変）。
pub(super) async fn node_subscription_loop(
    node_id: u64,
    initial_delay: Duration,
    native: Arc<NativeState>,
    events: broadcast::Sender<Emitted>,
    scope: SubscribeScope,
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
            node_id, backend, &events, &scope, &health, down_since, failures,
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

/// イベント行の `priming` フラグの決め方（`emit_event_lines` の入力）。
enum PrimingRule {
    /// 購読成立後の live report — 全て実イベント。
    Live,
    /// EventMin 無しの priming（起動直後）= デバイスのイベントログ全量。
    PrimingAll,
    /// EventMin 付き再購読の priming: 番号が `min` 以上のものだけが盲目窓中の
    /// 実イベント。**未満は `priming: true` に落とす** — EventFilters を
    /// 無視して全ログを返すデバイスがあると、再購読（pump 死 + backoff は
    /// 日常）のたびに古いボタン押下が `priming: false` で流れ、消費者が
    /// 再発火してしまう。捨てずに落とすのは、消費者が priming 行を無視する
    /// 既存契約に乗せたまま、ワイヤに来た事実は隠さないため。
    PrimingSince(u64),
}

impl PrimingRule {
    fn for_priming(event_min: Option<u64>) -> Self {
        match event_min {
            None => Self::PrimingAll,
            Some(min) => Self::PrimingSince(min),
        }
    }

    /// この番号のイベントを `priming` として流すか。
    fn priming_for(&self, node_id: u64, event_number: u64) -> bool {
        match self {
            Self::Live => false,
            Self::PrimingAll => true,
            Self::PrimingSince(min) => {
                let below = event_number < *min;
                if below {
                    tracing::debug!(
                        node_id,
                        event_number,
                        event_min = min,
                        "priming event below EventMin; device ignored EventFilters, keeping priming"
                    );
                }
                below
            }
        }
    }
}

/// EventReport 群をイベント行にして listen へ流し、番号を health へ記録する
/// （priming と live の違いは `rule` が決める `priming` フラグだけ）。番号の
/// 記録は送信前に行う: 受信者ゼロ（listen 接続なし）でも次の再購読の EventMin
/// は前へ進める必要がある。
fn emit_event_lines(
    node_id: u64,
    reports: &[mat_controller::im::EventReport],
    rule: &PrimingRule,
    ts: &str,
    events: &broadcast::Sender<Emitted>,
    health: &SubHealth,
) {
    // `priming` はいったん true で組み、番号が分かってから rule で決め直す
    // （`events_from_event_reports` は 1 通ぶんに一律のフラグしか持てない）。
    let live = matches!(rule, PrimingRule::Live);
    for mut item in events_from_event_reports(node_id, reports, true, ts) {
        item.priming = rule.priming_for(node_id, item.event_number);
        health.note_event_number(node_id, item.event_number, live);
        let _ = events.send(Emitted::Event(item)); // 受信者ゼロは正常
    }
}

/// 1 回の購読試行。確立+Subscribe 成立まで到達したら Ok(reason) を返して抜ける
/// （ポンプ死亡=正常喪失。reason は pump 終了理由の人間可読文字列で、呼び手が
/// `Down.last_error` の detail に使う）。確立前の失敗は Err。
pub(super) async fn run_subscription_once(
    node_id: u64,
    backend: &crate::native::NativeBackend,
    events: &broadcast::Sender<Emitted>,
    scope: &SubscribeScope,
    health: &SubHealth,
    down_since: tokio::time::Instant,
    prior_failures: u32,
) -> Result<String, mat_core::error::MatError> {
    let mut conn = backend.establish_subscription(node_id).await?;
    // 前回の購読で見た最後の EventNumber を知っていれば EventMin = last + 1 を
    // 載せる → 盲目窓中に起きたイベントだけが priming に乗って戻る（spec §6.2）。
    let event_min = health.last_event_number(node_id).map(|n| n + 1);
    let event_paths = scope.events.to_paths();
    let (info, priming, priming_events) = match conn
        .subscribe(&scope.clusters, &event_paths, event_min)
        .await
    {
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
        event_paths = event_paths.len(),
        event_min = ?event_min,
        "subscription established"
    );
    health.mark_established(node_id, info.subscription_id, info.max_interval_s);
    // priming は現在状態の全量 — down 中の op はここで配信されるので pending 解除。
    health.clear_pending(node_id);
    for msg in &priming {
        for ev in events_from_report(node_id, msg, true) {
            // 盲目期間中に起きた実遷移はここで通常イベントへ昇格する。
            let _ = events.send(Emitted::Attribute(health.observe(ev))); // 受信者ゼロは正常（listen 接続なし）
        }
    }
    // priming イベントは EventMin を載せられたときだけ「実イベント」:
    // 番号が EventMin 以上 = 盲目窓中に本当に起きた（属性の recovered 推定に
    // 相当するものを推定なしで得る）。起動直後（EventMin 無し）はデバイスの
    // ログ全量なので priming: true（消費者は無視する既存契約、spec §6.2）。
    // 番号の検査は matd 側で行う — デバイスの EventFilters 尊重を信用しない
    // （`PrimingRule::PrimingSince` のコメント）。
    emit_event_lines(
        node_id,
        &priming_events,
        &PrimingRule::for_priming(event_min),
        &now_iso8601(),
        events,
        health,
    );
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
        match conn.next_report_full(slice).await {
            Ok(Some(report)) => {
                proven = true;
                last_msg = tokio::time::Instant::now();
                health.clear_pending(node_id);
                health.note_device_msg(node_id);
                // 同じ ReportData 由来の属性行とイベント行は同じ受信時刻を持つ。
                let ts = now_iso8601();
                for ev in events_from_report_at(node_id, &report.data, false, &ts) {
                    let _ = events.send(Emitted::Attribute(health.observe(ev)));
                }
                emit_event_lines(
                    node_id,
                    &report.events,
                    &PrimingRule::Live,
                    &ts,
                    events,
                    health,
                );
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
}
