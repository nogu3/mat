//! Happy Eyeballs 風の staggered race（RFC 8305 の考え方を CASE の候補
//! アドレスに適用する）。
//!
//! N 個の試行を `stagger` 間隔で順に起動し、最初に `Ok` を返した試行を
//! 採用する。残りは future の drop でキャンセルされる。動いている試行が
//! 全部 `Err` になった時点でまだ起動していない試行があれば stagger を待たず
//! 即起動する（死んだ候補が速く失敗するときに無駄待ちしない）。
//!
//! 時間以外に副作用は無いので `tokio::time::pause` で決定的にテストできる。
//! 呼び出し側（`case::establish_any`）が試行ごとの資源（UDP ソケット）を
//! 用意する — このモジュールは future を回すだけで、ネットワークを知らない。

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

/// `items` の各要素に `attempt` を適用した future を `stagger` 間隔で起動し、
/// 最初に `Ok` になった試行の `(index, value)` を返す。全試行が `Err` なら
/// index 昇順の `Vec<E>`。空入力は `Err(vec![])`。
///
/// 起動規則:
/// - 最初の試行は即起動。
/// - 以降は前の起動から `stagger` 経過ごとに 1 本起動。
/// - 動いている試行がゼロになり、未起動が残っていれば stagger を待たず即起動。
///
/// 勝者確定時点で残りの試行は drop される（この関数の future と一緒に）。
pub async fn race_staggered<I, T, E, Fut>(
    items: Vec<I>,
    stagger: Duration,
    attempt: impl FnMut(I) -> Fut,
) -> Result<(usize, T), Vec<E>>
where
    Fut: Future<Output = Result<T, E>>,
{
    let mut pending = items.into_iter().enumerate();
    let mut attempt = attempt;
    // 動いている試行（起動 index, future）。Box::pin で Unpin にしておく。
    let mut running: Vec<(usize, Pin<Box<Fut>>)> = Vec::new();
    let mut errors: Vec<(usize, E)> = Vec::new();
    // 次の起動までのタイマ。None = 起動待ちなし(まだ 1 本も起動していない)。
    let mut timer: Option<Pin<Box<tokio::time::Sleep>>> = None;

    std::future::poll_fn(move |cx| loop {
        // 1. 起動判定。
        let mut started_now = false;
        if pending.len() > 0 {
            let due = running.is_empty()
                || match timer.as_mut() {
                    Some(t) => t.as_mut().poll(cx).is_ready(),
                    None => true,
                };
            if due {
                if let Some((idx, item)) = pending.next() {
                    running.push((idx, Box::pin(attempt(item))));
                    timer = Some(Box::pin(tokio::time::sleep(stagger)));
                    started_now = true;
                }
            }
        }

        // 2. 動いている試行を回す。
        let mut i = 0;
        while i < running.len() {
            match running[i].1.as_mut().poll(cx) {
                Poll::Ready(Ok(v)) => {
                    let idx = running[i].0;
                    return Poll::Ready(Ok((idx, v)));
                }
                Poll::Ready(Err(e)) => {
                    let (idx, _) = running.swap_remove(i);
                    errors.push((idx, e));
                }
                Poll::Pending => i += 1,
            }
        }

        // 3. 終了判定。
        if running.is_empty() && pending.len() == 0 {
            errors.sort_by_key(|(idx, _)| *idx);
            let errs = std::mem::take(&mut errors);
            return Poll::Ready(Err(errs.into_iter().map(|(_, e)| e).collect()));
        }
        // 起動したばかり、または全滅で未起動が残るなら即ループして次を起動。
        if started_now || running.is_empty() {
            continue;
        }
        // timer は上で poll 済み(running 非空かつ pending 残りのとき)なので
        // waker は登録されている。
        return Poll::Pending;
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use tokio::time::{sleep, Instant};

    const STAGGER: Duration = Duration::from_millis(500);

    /// 各試行の起動時刻を記録する。
    fn recorder() -> Rc<RefCell<Vec<Instant>>> {
        Rc::new(RefCell::new(Vec::new()))
    }

    #[tokio::test(start_paused = true)]
    async fn first_immediate_ok_wins_and_second_never_starts() {
        let starts = recorder();
        let r = race_staggered(vec![0u8, 1], STAGGER, |i| {
            starts.borrow_mut().push(Instant::now());
            async move {
                if i == 0 {
                    Ok::<u8, &str>(i)
                } else {
                    sleep(Duration::from_secs(10)).await;
                    Ok(i)
                }
            }
        })
        .await;
        assert_eq!(r, Ok((0, 0)));
        assert_eq!(starts.borrow().len(), 1, "2 本目は起動されない");
    }

    #[tokio::test(start_paused = true)]
    async fn slow_first_loses_to_staggered_second() {
        let t0 = Instant::now();
        let r = race_staggered(vec![0u8, 1], STAGGER, |i| async move {
            if i == 0 {
                sleep(Duration::from_secs(10)).await;
            }
            Ok::<u8, &str>(i)
        })
        .await;
        assert_eq!(r, Ok((1, 1)));
        assert_eq!(t0.elapsed(), STAGGER, "2 本目は stagger 後に起動して即 Ok");
    }

    #[tokio::test(start_paused = true)]
    async fn immediate_err_starts_next_without_waiting_stagger() {
        let t0 = Instant::now();
        let r = race_staggered(vec![0u8, 1], STAGGER, |i| async move {
            if i == 0 {
                Err("dead")
            } else {
                Ok::<u8, &str>(i)
            }
        })
        .await;
        assert_eq!(r, Ok((1, 1)));
        assert_eq!(
            t0.elapsed(),
            Duration::ZERO,
            "全滅中なら stagger を待たない"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn all_err_returns_errors_in_index_order() {
        // 0 は遅く失敗、1 は速く失敗 — 完了順は 1,0 だが返り値は index 順。
        let r = race_staggered(vec![0u8, 1], STAGGER, |i| async move {
            if i == 0 {
                sleep(Duration::from_secs(5)).await;
            }
            Err::<u8, String>(format!("err{i}"))
        })
        .await;
        assert_eq!(r, Err(vec!["err0".to_string(), "err1".to_string()]));
    }

    #[tokio::test(start_paused = true)]
    async fn empty_input_is_err_empty() {
        let r = race_staggered(
            Vec::<u8>::new(),
            STAGGER,
            |i| async move { Ok::<u8, &str>(i) },
        )
        .await;
        assert_eq!(r, Err(vec![]));
    }

    #[tokio::test(start_paused = true)]
    async fn attempts_start_at_stagger_multiples() {
        let starts = recorder();
        let t0 = Instant::now();
        let r = race_staggered(vec![0u8, 1, 2], STAGGER, |_| {
            starts.borrow_mut().push(Instant::now());
            async move {
                sleep(Duration::from_secs(5)).await;
                Err::<u8, &str>("slow fail")
            }
        })
        .await;
        assert!(r.is_err());
        let offsets: Vec<Duration> = starts.borrow().iter().map(|t| *t - t0).collect();
        assert_eq!(offsets, vec![Duration::ZERO, STAGGER, STAGGER * 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn losers_are_dropped_when_winner_returns() {
        struct Guard(Rc<Cell<bool>>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let dropped = Rc::new(Cell::new(false));
        let flag = Rc::clone(&dropped);
        let r = race_staggered(vec![0u8, 1], STAGGER, move |i| {
            let flag = Rc::clone(&flag);
            async move {
                if i == 0 {
                    let _g = Guard(flag);
                    sleep(Duration::from_secs(60)).await;
                    Ok::<u8, &str>(0)
                } else {
                    Ok(1)
                }
            }
        })
        .await;
        assert_eq!(r, Ok((1, 1)));
        assert!(dropped.get(), "敗者の future は勝者確定で drop される");
    }
}
