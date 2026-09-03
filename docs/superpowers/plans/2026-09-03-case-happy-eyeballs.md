# CASE Happy Eyeballs + commissioning/x509 テスト補強 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 候補アドレスへの CASE 確立を逐次試行から 500ms stagger の並行レース（最初に成功したものを採用）に変え、あわせて commissioning.rs / x509.rs の純粋関数群に実機不要のユニットテストを追加する。

**Architecture:** 汎用の staggered race ヘルパを `mat-controller::race` に新設し、`case::establish_any` が候補ごとに専用 UDP ソケットを bind して `case::establish` をレースさせる。`mat-native::CaseEstablisher` の 2 つの逐次ループ（op / subscription）は `establish_any` 1 呼び出しに置き換えるだけで、`Establisher` トレイト・エラー種別・成功ログ文言は不変。テスト補強はプロダクションコード無変更で `#[cfg(test)]` のみ追加する。

**Tech Stack:** Rust 2021、tokio（`time`, `net`, `macros`, dev で `test-util`）、既存 `tlv::Writer` / `asn1` ビルダ / `x509::test_support` / `test_support::responder_task`。新規依存なし。

**Spec:** `docs/superpowers/specs/2026-09-03-case-happy-eyeballs-design.md`

## Global Constraints

- 触ってよいのは `crates/mat-controller/src/{race.rs(新規), case.rs, lib.rs, commissioning.rs, x509.rs}`、`crates/mat-controller/tests/{case_self_handshake.rs, live_remote.rs, live_commission_real.rs}`、`crates/mat-native/src/lib.rs`（`CaseEstablisher` の 2 メソッドと import のみ）、`ARCHITECTURE.md`、`docs/commands.md`、`CLAUDE.md`。
- **触らない**: `crates/mat-controller/src/im.rs`（並行レーン C）、`crates/mat-core`、`crates/mat`、`crates/matd`、`crates/mat-device`、`crates/mat-native/src/{op.rs, runner.rs}`。
- 新規 crate 依存は追加しない（`futures-util` は `ble` feature 限定なので使わない）。
- stagger 定数は `case::RACE_STAGGER = Duration::from_millis(500)`。
- エラー種別マッピングは不変: 候補ゼロ = `ErrorKind::Unreachable`（detail `native: no addresses resolved for node {node_id}`）、全滅 = `ErrorKind::SessionFailed`、bind 失敗 = `ErrorKind::Other`（detail `native: bind op udp: ...` / `native: bind subscription udp: ...`）。
- 成功ログ文言 `op transport bound (dedicated socket + CASE)` / `subscription transport bound (dedicated socket + CASE)` は不変。
- `session::budget_components_are_pinned`、`MrpConfig` 既定、`ResolvedNode::mrp_config()`、`RETRY_MIN_BUDGET` は変更しない。
- テスト補強タスク（Task 4 / 5）はプロダクションコードの挙動を変えない。可視性を `pub(crate)` に上げる以外の変更が必要に見えたら止めて報告する。
- コミットは各タスク末尾で 1 回。コミットメッセージ末尾に以下を付ける:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01K5MtCanVMoggohBywDwz7P
  ```
- 検証コマンド: 単体は `cargo test -p mat-controller <filter>`、全体は `task check`（fmt:check + clippy `-D warnings` + `cargo test`）。ワークスペースは `/home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/case-happy-eyeballs`。
- 既知の flaky: `mat_controller::group::tests::send_invoke_emits_identical_datagram_on_each_egress` が全 workspace 並列 test で稀に落ちる（単独 pass、diff 外）。これだけが落ちたら単独再実行で確認する。

---

### Task 1: `race::race_staggered` — 汎用 staggered race ヘルパ

**Files:**
- Create: `crates/mat-controller/src/race.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod race;` を `pub mod pase;` の次、アルファベット順に追加）

**Interfaces:**
- Produces:
  ```rust
  pub async fn race_staggered<I, T, E, Fut>(
      items: Vec<I>,
      stagger: Duration,
      attempt: impl FnMut(I) -> Fut,
  ) -> Result<(usize, T), Vec<E>>
  where
      Fut: Future<Output = Result<T, E>>;
  ```
  勝者は `(index, value)`。全滅は index 昇順の `Vec<E>`。空入力は `Err(vec![])`。

- [ ] **Step 1: 失敗するテストを書く（モジュールごと新規作成）**

`crates/mat-controller/src/race.rs` を以下の内容で作る（実装は `todo!()` のまま）:

```rust
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
    let _ = (items, stagger, attempt);
    todo!()
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
        assert_eq!(t0.elapsed(), Duration::ZERO, "全滅中なら stagger を待たない");
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
        let r = race_staggered(Vec::<u8>::new(), STAGGER, |i| async move {
            Ok::<u8, &str>(i)
        })
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
```

`crates/mat-controller/src/lib.rs` に `pub mod race;` を追加（`pub mod pase;` の直後）。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller race::`
Expected: コンパイルは通り、全 7 テストが `todo!()` の panic で FAIL。

- [ ] **Step 3: 実装**

`todo!()` の関数本体を以下に置き換える:

```rust
{
    let mut pending = items.into_iter().enumerate();
    let mut attempt = attempt;
    // 動いている試行（起動 index, future）。Box::pin で Unpin にしておく。
    let mut running: Vec<(usize, Pin<Box<Fut>>)> = Vec::new();
    let mut errors: Vec<(usize, E)> = Vec::new();
    // 次の起動までのタイマ。None = 起動待ちなし（まだ 1 本も起動していない）。
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
        // timer は上で poll 済み（running 非空かつ pending 残りのとき）なので
        // waker は登録されている。
        return Poll::Pending;
    })
    .await
}
```

注意: `poll_fn` のクロージャは `move` で全状態を所有するので、勝者を返してこの
future が完了・drop されると `running` に残った敗者も一緒に drop される。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller race::`
Expected: 7 passed。

- [ ] **Step 5: clippy / fmt**

Run: `cargo fmt -p mat-controller && cargo clippy -p mat-controller --all-targets -- -D warnings`
Expected: 警告ゼロ。

- [ ] **Step 6: Commit**

```bash
git add crates/mat-controller/src/race.rs crates/mat-controller/src/lib.rs
git commit -m "feat(mat-controller): race::race_staggered — Happy Eyeballs 用の汎用 staggered race（監査レーン D）"
```

---

### Task 2: `case::establish_any` — 候補ごとの専用ソケット × レース

**Files:**
- Modify: `crates/mat-controller/src/case.rs`（`establish` の直後、`#[cfg(test)]` の前に追加）
- Modify: `crates/mat-controller/tests/case_self_handshake.rs`（共通 fixture 化 + 4 テスト追加）

**Interfaces:**
- Consumes: `race::race_staggered`（Task 1）、`case::establish`、`transport::{Transport, UdpTransport}`。
- Produces:
  ```rust
  pub const RACE_STAGGER: Duration;                       // 500ms
  pub struct Established { pub session: SecureSession, pub peer: SocketAddr, pub local: Option<SocketAddr> }
  pub enum EstablishAnyError { NoAddresses, AllFailed(Vec<(SocketAddr, CaseError)>), Bind(std::io::Error) }
  pub async fn establish_any(peers: &[SocketAddr], creds: &FabricCredentials, peer_node_id: u64, cfg: &MrpConfig, stagger: Duration) -> Result<Established, EstablishAnyError>;
  ```

- [ ] **Step 1: 統合テストを書く（`tests/case_self_handshake.rs`）**

既存テストの setup を fixture 関数に括り出し、4 テストを追加する。ファイル全体を以下に置き換える（既存テスト `case_establishes_and_reads_over_loopback` の検証内容は維持）:

```rust
//! Offline CASE self-handshake (mandatory quality gate, plan Task 6).
//!
//! Drives the real CASE initiator (`case::establish`) against a test-only
//! CASE *responder* (extracted to `mat_controller::test_support`, gated
//! behind the `test-responder` feature) over loopback UDP, then performs one
//! secured IM read. This is the first *executable* coverage of the CASE
//! crypto-ordering core — transcript boundaries (S2K salted with
//! SHA256(sigma1) alone, sigma2 folded in only afterwards; S3K over
//! SHA256(sigma1||sigma2); SessionKeys over SHA256(sigma1||sigma2||sigma3)),
//! the S2K/S3K/SessionKeys HKDF derivations, TBS2/TBS3 orientation
//! (sender-eph before receiver-eph), the i2r/r2i key split, and the
//! Sigma1/2/3 + StatusReport wire framing — none of which the (device-
//! blocked) live E2E can currently exercise. See `test_support` for the
//! responder implementation and its residual-risk caveat.
//!
//! The `establish_any_*` tests below pin the Happy Eyeballs candidate race
//! (`case::establish_any`): a dead first address no longer blocks a live
//! second one, all-dead reports every peer, and a live first address wins
//! without the second responder ever seeing a Sigma1.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mat_controller::case::{self, EstablishAnyError};
use mat_controller::cert::{verify_noc_chain, MatterCert};
use mat_controller::fabric::FabricCredentials;
use mat_controller::im::{self, ImValue};
use mat_controller::kvs::SelfIssueMaterials;
use mat_controller::test_support::{
    fast_cfg, responder_task, ICA01, INITIATOR_NODE_ID, IPK, NODE01_NOC, NODE01_PRIV, ROOT01_CHIP,
    ROOT01_PRIV,
};
use mat_controller::transport::{Transport, UdpTransport};
use tokio::task::JoinHandle;

/// Responder identity (node01_01 under ica01/root01) + an initiator
/// credential set on the same fabric.
struct Fixture {
    creds: FabricCredentials,
    responder_node_id: u64,
}

fn fixture() -> Fixture {
    let noc_cert = MatterCert::parse(NODE01_NOC).expect("parse node01_01 NOC");
    let responder_node_id = noc_cert.node_id().expect("node id");
    let responder_fabric_id = noc_cert.fabric_id().expect("fabric id");
    let ica_cert = MatterCert::parse(ICA01).unwrap();
    let root_cert = MatterCert::parse(ROOT01_CHIP).unwrap();
    verify_noc_chain(&noc_cert, Some(&ica_cert), &root_cert).expect("fixture chain");

    let materials = SelfIssueMaterials {
        rcac: ROOT01_CHIP.to_vec(),
        root_private_key: ROOT01_PRIV.try_into().unwrap(),
        ipk_operational: IPK,
        node_id: INITIATOR_NODE_ID,
        fabric_id: responder_fabric_id,
    };
    let creds = FabricCredentials::from_self_issued(materials).expect("self-issued creds");
    Fixture {
        creds,
        responder_node_id,
    }
}

/// Spawns a loopback CASE responder; returns its address and the task
/// (which resolves to the initiator address it observed).
async fn spawn_responder(fx: &Fixture) -> (SocketAddr, JoinHandle<SocketAddr>) {
    let t = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap();
    let addr = t.local_addr().unwrap();
    let op_priv: [u8; 32] = NODE01_PRIV.try_into().unwrap();
    let handle = tokio::spawn(responder_task(
        t,
        INITIATOR_NODE_ID,
        fx.responder_node_id,
        NODE01_NOC.to_vec(),
        ICA01.to_vec(),
        op_priv,
        ROOT01_CHIP.to_vec(),
    ));
    (addr, handle)
}

/// A loopback port with nobody listening: bind, read the address, drop.
/// Datagrams sent there vanish (unconnected UDP gets no ICMP error), so
/// the CASE attempt times out after the MRP budget of `fast_cfg()`.
async fn dead_port() -> SocketAddr {
    let t = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap();
    t.local_addr().unwrap()
}

#[tokio::test]
async fn case_establishes_and_reads_over_loopback() {
    let fx = fixture();
    let (responder_addr, responder) = spawn_responder(&fx).await;

    let initiator_udp = Arc::new(
        UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap(),
    );
    let initiator_local = initiator_udp.local_addr().unwrap();
    let initiator_transport = Arc::new(Transport::Udp(Arc::clone(&initiator_udp)));

    let cfg = fast_cfg();
    let mut session = case::establish(
        Arc::clone(&initiator_transport),
        responder_addr,
        &fx.creds,
        fx.responder_node_id,
        &cfg,
    )
    .await
    .expect("CASE establish should succeed over loopback");

    let value = session
        .read_attribute(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &cfg)
        .await
        .expect("secured read should succeed");
    assert_eq!(value, ImValue::Bool(false));

    let observed = responder.await.expect("responder task panicked");
    assert_eq!(
        observed, initiator_local,
        "responder saw the initiator's socket"
    );
}

const TEST_STAGGER: Duration = Duration::from_millis(50);

#[tokio::test]
async fn establish_any_dead_first_address_falls_through_to_live_second() {
    let fx = fixture();
    let dead = dead_port().await;
    let (live, responder) = spawn_responder(&fx).await;
    let cfg = fast_cfg();

    let est = case::establish_any(
        &[dead, live],
        &fx.creds,
        fx.responder_node_id,
        &cfg,
        TEST_STAGGER,
    )
    .await
    .expect("live second address must win");
    assert_eq!(est.peer, live, "winner is the live candidate");
    let local = est.local.expect("winner reports its local socket");

    let mut session = est.session;
    let value = session
        .read_attribute(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &cfg)
        .await
        .expect("secured read on the winning session");
    assert_eq!(value, ImValue::Bool(false));

    let observed = responder.await.expect("responder task panicked");
    assert_eq!(observed, local, "responder saw the winning attempt's socket");
}

#[tokio::test]
async fn establish_any_all_dead_reports_every_peer() {
    let fx = fixture();
    let dead1 = dead_port().await;
    let dead2 = dead_port().await;
    let cfg = fast_cfg();

    let err = case::establish_any(
        &[dead1, dead2],
        &fx.creds,
        fx.responder_node_id,
        &cfg,
        TEST_STAGGER,
    )
    .await
    .err()
    .expect("all-dead must fail");
    match err {
        EstablishAnyError::AllFailed(list) => {
            let peers: Vec<SocketAddr> = list.iter().map(|(p, _)| *p).collect();
            assert_eq!(peers, vec![dead1, dead2], "every peer, in candidate order");
            let text = format!("{}", EstablishAnyError::AllFailed(list));
            assert!(text.starts_with("CASE failed on all 2 address(es): "), "{text}");
            assert!(text.contains(&dead1.to_string()) && text.contains(&dead2.to_string()));
        }
        other => panic!("expected AllFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn establish_any_no_candidates_is_no_addresses() {
    let fx = fixture();
    let cfg = fast_cfg();
    let err = case::establish_any(&[], &fx.creds, fx.responder_node_id, &cfg, TEST_STAGGER)
        .await
        .err()
        .expect("empty candidates must fail");
    assert!(matches!(err, EstablishAnyError::NoAddresses), "{err:?}");
    assert_eq!(format!("{err}"), "no addresses");
}

#[tokio::test]
async fn establish_any_live_first_wins_without_touching_second() {
    let fx = fixture();
    let (live1, responder1) = spawn_responder(&fx).await;
    let (live2, responder2) = spawn_responder(&fx).await;
    let cfg = fast_cfg();

    let est = case::establish_any(
        &[live1, live2],
        &fx.creds,
        fx.responder_node_id,
        &cfg,
        // Longer than a loopback handshake so the second attempt never starts.
        Duration::from_secs(5),
    )
    .await
    .expect("first live address wins");
    assert_eq!(est.peer, live1);

    let mut session = est.session;
    session
        .read_attribute(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &cfg)
        .await
        .expect("read on winner");
    responder1.await.expect("responder 1 completes");

    // Responder 2 only resolves once it has served a handshake; it must
    // still be waiting.
    let untouched = tokio::time::timeout(Duration::from_millis(200), responder2).await;
    assert!(untouched.is_err(), "second responder must not have seen a Sigma1");
}
```

- [ ] **Step 2: テストが失敗（コンパイルエラー）することを確認**

Run: `cargo test -p mat-controller --test case_self_handshake`
Expected: `establish_any` / `EstablishAnyError` / `RACE_STAGGER` が無いのでコンパイル FAIL。

- [ ] **Step 3: `case.rs` に実装を追加**

`use` 節に追加:

```rust
use crate::race::race_staggered;
use crate::transport::UdpTransport;
```

（`use crate::transport::Transport;` は既存。`Transport` と同じ行にまとめてもよい。）

`establish` 関数の直後（`#[cfg(test)] mod tests` の前）に追加:

```rust
/// 候補アドレスを順に起動する間隔（Happy Eyeballs の stagger）。RFC 8305 の
/// 250ms より長めに取り、健全な先頭アドレスの Sigma2 が返る前に 2 本目の
/// Sigma1 を撃って chip SDK デバイスの BUSY 応答を誘発しにくくしている。
/// 死んだ先頭アドレス 1 本の損失はこの値（従来は MRP 予算いっぱい、
/// SII=5000ms なら ~80 秒）。
pub const RACE_STAGGER: Duration = Duration::from_millis(500);

/// [`establish_any`] の成功結果。
pub struct Established {
    pub session: SecureSession,
    /// 勝った候補アドレス。
    pub peer: SocketAddr,
    /// 勝った試行の専用ソケットの local addr（`ss -uanp` / tcpdump 突合ログ用）。
    pub local: Option<SocketAddr>,
}

/// [`establish_any`] のエラー。
#[derive(Debug)]
pub enum EstablishAnyError {
    /// 候補が空（resolve は成功したが AAAA が 1 本も無い）。
    NoAddresses,
    /// 全候補が失敗。候補順。
    AllFailed(Vec<(SocketAddr, CaseError)>),
    /// 試行用ソケットの bind 失敗（1 本でも失敗したらその場で全体エラー）。
    Bind(std::io::Error),
}

impl std::fmt::Display for EstablishAnyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EstablishAnyError::NoAddresses => write!(f, "no addresses"),
            EstablishAnyError::AllFailed(list) => {
                write!(f, "CASE failed on all {} address(es): ", list.len())?;
                for (i, (peer, err)) in list.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{peer}: {err}")?;
                }
                Ok(())
            }
            EstablishAnyError::Bind(e) => write!(f, "bind udp: {e}"),
        }
    }
}

impl std::error::Error for EstablishAnyError {}

/// 複数の候補アドレスへ CASE を Happy Eyeballs 方式で確立する: 候補ごとに
/// **専用の** UDP ソケットを bind し、`stagger` 間隔で [`establish`] を起動、
/// 最初に成功した試行を採用して残りを drop する（[`crate::race`]）。
///
/// 専用ソケットが必須なのは、`UnsecuredExchange::screen` が自分の exchange
/// 以外のデータグラムを捨てるため — 1 ソケットを共有すると並行試行が互いの
/// 応答を吸って落とす。同一ノードへの並行 Sigma1 自体は安全（local session
/// id / exchange id / source node id は試行ごとにランダム）。
///
/// 所要時間の上限は設けない（MRP 予算は仕様どおり使い切らせる）。全体は
/// 呼び出し側の op deadline が縛る。
pub async fn establish_any(
    peers: &[SocketAddr],
    creds: &FabricCredentials,
    peer_node_id: u64,
    cfg: &MrpConfig,
    stagger: Duration,
) -> Result<Established, EstablishAnyError> {
    if peers.is_empty() {
        return Err(EstablishAnyError::NoAddresses);
    }
    let mut attempts: Vec<(SocketAddr, Arc<Transport>)> = Vec::with_capacity(peers.len());
    for peer in peers {
        let udp = UdpTransport::bind()
            .await
            .map_err(EstablishAnyError::Bind)?;
        attempts.push((*peer, Arc::new(Transport::Udp(Arc::new(udp)))));
    }

    let outcome = race_staggered(attempts, stagger, |(peer, transport)| async move {
        let local = transport.local_addr().ok();
        match establish(transport, peer, creds, peer_node_id, cfg).await {
            Ok(session) => Ok((session, local)),
            Err(e) => {
                tracing::debug!(%peer, error = %e, "CASE attempt failed");
                Err((peer, e))
            }
        }
    })
    .await;

    match outcome {
        Ok((idx, (session, local))) => Ok(Established {
            session,
            peer: peers[idx],
            local,
        }),
        Err(list) => Err(EstablishAnyError::AllFailed(list)),
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller --test case_self_handshake`
Expected: 5 passed（既存 1 + 新規 4）。所要は数秒以内（`fast_cfg` の 200ms 失敗待ち × 2 程度）。

- [ ] **Step 5: clippy / fmt / 全 crate テスト**

Run: `cargo fmt -p mat-controller && cargo clippy -p mat-controller --all-targets -- -D warnings && cargo test -p mat-controller`
Expected: 警告ゼロ、全 pass。

- [ ] **Step 6: Commit**

```bash
git add crates/mat-controller/src/case.rs crates/mat-controller/tests/case_self_handshake.rs
git commit -m "feat(mat-controller): case::establish_any — 候補アドレスを専用ソケット×staggered race で確立（監査レーン D）"
```

---

### Task 3: mat-native の 2 ループを `establish_any` に置換 + live テスト追従

**Files:**
- Modify: `crates/mat-native/src/lib.rs:17`（import）、`:499-544`（`establish`）、`:546-596`（`establish_subscription`）、新ヘルパ `map_establish_err`
- Modify: `crates/mat-controller/tests/live_remote.rs:117-140`
- Modify: `crates/mat-controller/tests/live_commission_real.rs:97-117`

**Interfaces:**
- Consumes: `case::establish_any`, `case::RACE_STAGGER`, `case::Established`, `case::EstablishAnyError`（Task 2）。
- Produces: なし（`Establisher` トレイト不変）。

- [ ] **Step 1: 現状のテストが通ることを確認（回帰の基準）**

Run: `cargo test -p mat-native concurrent_establishes_use_dedicated_sockets`
Expected: 1 passed。

- [ ] **Step 2: `CaseEstablisher` の 2 メソッドを置換**

`crates/mat-native/src/lib.rs` の `impl Establisher for CaseEstablisher { ... }` ブロック全体（`establish` と `establish_subscription`）を以下に置き換え、直後にヘルパを追加する:

```rust
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
```

import 行 `use mat_controller::transport::{Transport, UdpTransport};` は、`Transport` がこのファイルの他所で使われなくなるので `use mat_controller::transport::UdpTransport;` に縮める（`UdpTransport` は groupcast 用 bind で引き続き使う。clippy の unused import で確認）。

- [ ] **Step 3: live テストの手書きループを置換**

`crates/mat-controller/tests/live_remote.rs` の「受け入れ 4」ブロック（`let transport = std::sync::Arc::new(Transport::Udp(...` から `let mut session = session.expect(...)` まで）を以下に置換:

```rust
    // 受け入れ 4: CASE 確立（候補アドレスは Happy Eyeballs レース）
    let est = case::establish_any(
        &peers,
        &creds,
        device_node_id,
        &mrp,
        case::RACE_STAGGER,
    )
    .await
    .expect("CASE establishment failed on all resolved addresses");
    eprintln!("CASE established via {}", est.peer);
    let mut session = est.session;
```

このファイルの `use mat_controller::transport::{Transport, UdpTransport};` は他に使用箇所が無ければ削除する（`cargo test -p mat-controller --no-run` の unused import 警告で確認）。

`crates/mat-controller/tests/live_commission_real.rs` の 2/7 ブロック（`let mut prod_session = None;` から `prod_session.expect(...)` まで）を以下に置換:

```rust
    let est = case::establish_any(
        &peers,
        &prod_creds,
        device_node_id,
        &mrp,
        case::RACE_STAGGER,
    )
    .await
    .expect("CASE establishment failed on all resolved addresses");
    eprintln!("CASE established via {}", est.peer);
    let mut prod_session = est.session;
```

`session_transport`（70 行目）が他で未使用になれば削除する。`#[ignore]` テストなのでコンパイルが通ることが検証（`cargo test -p mat-controller --no-run`）。

- [ ] **Step 4: 回帰テスト + 全体**

Run: `cargo test -p mat-native concurrent_establishes_use_dedicated_sockets && cargo test -p mat-controller --no-run && cargo test -p mat-native && cargo test -p matd`
Expected: 全 pass（matd は `Establisher` の利用側なのでコンパイル確認を含む）。

- [ ] **Step 5: clippy / fmt**

Run: `cargo fmt --all && cargo clippy --all-targets -- -D warnings`
Expected: 警告ゼロ（unused import が残っていればここで落ちる → 削除）。

- [ ] **Step 6: Commit**

```bash
git add crates/mat-native/src/lib.rs crates/mat-controller/tests/live_remote.rs crates/mat-controller/tests/live_commission_real.rs
git commit -m "refactor(mat-native): CaseEstablisher の逐次候補ループを case::establish_any に置換（Happy Eyeballs、監査レーン D）"
```

---

### Task 4: commissioning.rs デコーダのユニットテスト補強

**Files:**
- Modify: `crates/mat-controller/src/commissioning.rs`（`#[cfg(test)] mod tests` 末尾に追加）

**Interfaces:**
- Consumes: 既存の private/pub 関数（同モジュール内テストなので private も呼べる）: `scan_struct_fields`, `take_u8/u16/u32`, `skip_container`（経由）, `decode_*`, `encode_*`, `fields_of`, `check_commissioning_response`, `random_discriminator`, `CommissionError`。
- Produces: なし（テストのみ）。プロダクションコード無変更。

- [ ] **Step 1: 既存テスト数を記録**

Run: `/usr/bin/grep -c '#\[test\]' crates/mat-controller/src/commissioning.rs`
Expected: `36`。

- [ ] **Step 2: テストを追加**

`mod tests` の末尾（最後の `}` の前）に追加。`mod tests` 先頭の `use` に `use crate::tlv::{Tag, Writer};` と `use crate::im;` が無ければ追加する（既にあれば重複させない）:

```rust
    // ---- 監査レーン D: デコーダの負系・境界（実機不要） ----

    /// `{0: u16, 1: bytes(97), 2: u16, 3: u32, 4: bytes}` を組む小ヘルパ。
    /// `skip` に入れたタグは省略する（欠損分岐のテスト用）。
    fn open_window_fields(skip: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        if !skip.contains(&0) {
            w.put_uint(Tag::Context(0), 180);
        }
        if !skip.contains(&1) {
            w.put_bytes(Tag::Context(1), &[7u8; 97]);
        }
        if !skip.contains(&2) {
            w.put_uint(Tag::Context(2), 0xABC);
        }
        if !skip.contains(&3) {
            w.put_uint(Tag::Context(3), 1000);
        }
        if !skip.contains(&4) {
            w.put_bytes(Tag::Context(4), &[9u8; 16]);
        }
        w.end_container();
        w.finish()
    }

    #[test]
    fn decode_open_commissioning_window_roundtrips_with_encoder() {
        let fields = encode_open_commissioning_window(180, &[7u8; 97], 0xABC, 1000, &[9u8; 16]);
        let (timeout_s, verifier, disc, iters, salt) =
            decode_open_commissioning_window(&fields).expect("decode");
        assert_eq!(timeout_s, 180);
        assert_eq!(verifier, vec![7u8; 97]);
        assert_eq!(disc, 0xABC);
        assert_eq!(iters, 1000);
        assert_eq!(salt, vec![9u8; 16]);
    }

    #[test]
    fn decode_open_commissioning_window_reports_each_missing_field() {
        let expected = [
            (0u8, "missing timeout"),
            (1, "missing verifier"),
            (2, "missing discriminator"),
            (3, "missing iterations"),
            (4, "missing salt"),
        ];
        for (tag, detail) in expected {
            match decode_open_commissioning_window(&open_window_fields(&[tag])) {
                Err(CommissionError::Malformed { detail: d, .. }) => {
                    assert_eq!(d, detail, "tag {tag}")
                }
                other => panic!("tag {tag}: expected Malformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn decode_open_commissioning_window_rejects_iterations_over_u32() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 180);
        w.put_bytes(Tag::Context(1), &[7u8; 97]);
        w.put_uint(Tag::Context(2), 0xABC);
        w.put_uint(Tag::Context(3), 1u64 << 32);
        w.put_bytes(Tag::Context(4), &[9u8; 16]);
        w.end_container();
        match decode_open_commissioning_window(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => {
                assert_eq!(detail, "iterations out of range")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    /// 全デコーダ共通の「構造が壊れた入力」— 空 / 先頭が struct でない /
    /// ContainerEnd が無い / 制御バイトが不正。どれも `Malformed` になること
    /// （panic しない・Ok にならない）を代表デコーダ 5 本で確認する。
    #[test]
    fn decoders_reject_structurally_broken_input() {
        type Decoder = Box<dyn Fn(&[u8]) -> Result<(), CommissionError>>;
        let decoders: Vec<(&str, Decoder)> = vec![
            (
                "noc_response",
                Box::new(|b: &[u8]| decode_noc_response(b).map(|_| ())),
            ),
            (
                "attestation_response",
                Box::new(|b: &[u8]| decode_attestation_response(b).map(|_| ())),
            ),
            (
                "add_noc",
                Box::new(|b: &[u8]| decode_add_noc(b).map(|_| ())),
            ),
            (
                "arm_fail_safe",
                Box::new(|b: &[u8]| decode_arm_fail_safe(b).map(|_| ())),
            ),
            (
                "commissioning_status_response",
                Box::new(|b: &[u8]| decode_commissioning_status_response(b).map(|_| ())),
            ),
        ];

        let not_struct = {
            let mut w = Writer::new();
            w.put_uint(Tag::Anonymous, 1);
            w.finish()
        };
        let unterminated = {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(0), 0);
            w.end_container();
            let mut b = w.finish();
            b.pop(); // ContainerEnd (0x18) を落とす
            b
        };
        let inputs: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("not-struct", not_struct),
            ("unterminated", unterminated),
            ("garbage", vec![0xFF, 0xFF, 0xFF]),
        ];

        for (dname, dec) in &decoders {
            for (iname, input) in &inputs {
                match dec(input) {
                    Err(CommissionError::Malformed { .. }) => {}
                    other => panic!("{dname} on {iname}: expected Malformed, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn decoders_treat_wrong_field_type_as_missing() {
        // Bytes 期待のタグに Utf8 → 「無い」扱いで missing。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_str(Tag::Context(0), "not bytes");
        w.put_bytes(Tag::Context(1), &[0u8; 64]);
        w.end_container();
        match decode_attestation_response(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => assert_eq!(detail, "missing elements"),
            other => panic!("expected Malformed, got {other:?}"),
        }
        // Uint 期待のタグに Bytes → missing statusCode。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(0), &[0]);
        w.end_container();
        match decode_noc_response(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => {
                assert_eq!(detail, "missing statusCode")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn take_helpers_reject_out_of_range_uints() {
        // take_u8 経由: statusCode = 256
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 256);
        w.end_container();
        match decode_noc_response(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => {
                assert_eq!(detail, "statusCode out of range")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
        // take_u16 経由: expiry = 65536
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 65_536);
        w.end_container();
        match decode_arm_fail_safe(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => {
                assert_eq!(detail, "expiry out of range")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
        // 上限ぴったりは通る。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 65_535);
        w.end_container();
        assert_eq!(decode_arm_fail_safe(&w.finish()).unwrap(), (65_535, 0));
    }

    #[test]
    fn scan_skips_nested_containers_under_unknown_tags() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 7);
        w.start_struct(Tag::Context(9)); // 未知タグのネスト
        w.start_array(Tag::Context(1));
        w.start_list(Tag::Anonymous);
        w.put_uint(Tag::Anonymous, 1);
        w.end_container();
        w.end_container();
        w.put_str(Tag::Context(2), "inner");
        w.end_container();
        w.put_str(Tag::Context(1), "x");
        w.end_container();
        let fields = w.finish();
        assert_eq!(
            decode_commissioning_status_response(&fields).unwrap(),
            (7, "x".to_string())
        );

        // 入れ子の閉じが無い → truncated。内側 struct の直後で切れるよう、
        // leaf 1 個だけの入れ子を作り末尾の ContainerEnd 2 個（内・外）を落とす。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 7);
        w.start_struct(Tag::Context(9));
        w.put_uint(Tag::Context(1), 1);
        w.end_container();
        w.end_container();
        let mut cut = w.finish();
        cut.truncate(cut.len() - 2);
        match decode_commissioning_status_response(&cut) {
            Err(CommissionError::Malformed { detail, .. }) => assert_eq!(detail, "truncated"),
            other => panic!("expected Malformed(truncated), got {other:?}"),
        }
    }

    #[test]
    fn scan_struct_fields_duplicate_tag_last_wins() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 1);
        w.put_uint(Tag::Context(0), 5);
        w.end_container();
        assert_eq!(
            decode_commissioning_status_response(&w.finish()).unwrap().0,
            5
        );
    }

    #[test]
    fn decode_add_noc_accepts_icac_and_rejects_bad_ipk_length() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(0), b"noc");
        w.put_bytes(Tag::Context(1), b"icac");
        w.put_bytes(Tag::Context(2), &[0xAA; 16]);
        w.put_uint(Tag::Context(3), 0x1122);
        w.put_uint(Tag::Context(4), 0xFFF1);
        w.end_container();
        let (noc, icac, ipk, subject, vid) = decode_add_noc(&w.finish()).unwrap();
        assert_eq!(noc, b"noc");
        assert_eq!(icac.as_deref(), Some(&b"icac"[..]));
        assert_eq!(ipk, [0xAA; 16]);
        assert_eq!((subject, vid), (0x1122, 0xFFF1));

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(0), b"noc");
        w.put_bytes(Tag::Context(2), &[0xAA; 15]);
        w.put_uint(Tag::Context(3), 1);
        w.put_uint(Tag::Context(4), 1);
        w.end_container();
        match decode_add_noc(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => assert_eq!(detail, "ipk length"),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn signature_carrying_responses_reject_wrong_signature_length() {
        for (name, dec) in [
            (
                "attestation",
                decode_attestation_response as fn(&[u8]) -> Result<(Vec<u8>, [u8; 64]), CommissionError>,
            ),
            ("csr", decode_csr_response),
        ] {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_bytes(Tag::Context(0), b"elements");
            w.put_bytes(Tag::Context(1), &[0u8; 63]);
            w.end_container();
            match dec(&w.finish()) {
                Err(CommissionError::Malformed { detail, .. }) => {
                    assert_eq!(detail, "signature length", "{name}")
                }
                other => panic!("{name}: expected Malformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn fields_of_maps_status_and_missing_fields() {
        let bad_status = im::InvokeResponseData {
            status: 0x85,
            cluster_status: None,
            fields_tlv: None,
        };
        match fields_of("step-a", &bad_status) {
            Err(CommissionError::CommandStatus { step, code }) => {
                assert_eq!((step, code), ("step-a", 0x85))
            }
            other => panic!("expected CommandStatus, got {other:?}"),
        }
        let no_fields = im::InvokeResponseData {
            status: 0,
            cluster_status: None,
            fields_tlv: None,
        };
        match fields_of("step-b", &no_fields) {
            Err(CommissionError::Malformed { step, detail }) => {
                assert_eq!((step, detail), ("step-b", "no command fields"))
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
        let ok = im::InvokeResponseData {
            status: 0,
            cluster_status: None,
            fields_tlv: Some(vec![0x15, 0x18]),
        };
        assert_eq!(fields_of("step-c", &ok).unwrap(), &[0x15, 0x18]);
    }

    #[test]
    fn check_commissioning_response_rejects_nonzero_error_code() {
        let resp = im::InvokeResponseData {
            status: 0,
            cluster_status: None,
            fields_tlv: Some(encode_commissioning_status_response(3, "busy")),
        };
        match check_commissioning_response("arm", &resp) {
            Err(CommissionError::CommandStatus { step, code }) => {
                assert_eq!((step, code), ("arm", 3))
            }
            other => panic!("expected CommandStatus, got {other:?}"),
        }
        let ok = im::InvokeResponseData {
            status: 0,
            cluster_status: None,
            fields_tlv: Some(encode_commissioning_status_response(0, "")),
        };
        assert!(check_commissioning_response("arm", &ok).is_ok());
    }

    #[test]
    fn random_discriminator_fits_12_bits() {
        for _ in 0..64 {
            assert!(random_discriminator() <= 0x0FFF);
        }
    }

    #[test]
    fn commission_error_display_names_each_variant() {
        let cases: Vec<(CommissionError, &str)> = vec![
            (CommissionError::Csr("bad csr"), "csr error: bad csr"),
            (CommissionError::Noc(0x0B), "NOCResponse status 0x0B"),
            (
                CommissionError::CommandStatus { step: "arm", code: 2 },
                "arm errorCode 0x02",
            ),
            (
                CommissionError::Malformed { step: "s", detail: "d" },
                "s: malformed (d)",
            ),
            (CommissionError::Timeout("resolve"), "timeout (resolve)"),
            (
                CommissionError::Ble { step: "scan", detail: "no adapter".into() },
                "ble scan: no adapter",
            ),
            (
                CommissionError::NetworkConfig { step: "connect", status: 4, debug_text: None },
                "connect NetworkingStatus 0x04",
            ),
            (
                CommissionError::NetworkConfig {
                    step: "connect",
                    status: 4,
                    debug_text: Some("no route".into()),
                },
                "connect NetworkingStatus 0x04 (no route)",
            ),
            (
                CommissionError::InvalidArgument { what: "discriminator" },
                "invalid argument: discriminator",
            ),
            (
                CommissionError::Case(crate::case::CaseError::Sigma2NotAcked),
                "case error:",
            ),
        ];
        for (err, needle) in cases {
            let text = err.to_string();
            assert!(text.starts_with("commissioning: "), "{text}");
            assert!(text.contains(needle), "{text} should contain {needle}");
        }
    }
```

- [ ] **Step 3: テストを実行し、落ちたものを精査**

Run: `cargo test -p mat-controller commissioning::tests`
Expected: 全 pass。**落ちた場合の判断**: assert の期待値がプロダクションの実挙動と違うなら、実挙動が spec（docstring）と整合している限りテスト側を実挙動に合わせる（プロダクション変更は不可）。`garbage` 入力（`0xFF`）で `Malformed` 以外が返るなら、その入力だけ `[0xFF, 0xFF, 0xFF]` から `[0x15, 0xFF]`（struct 開始 + 不正制御バイト）に替えて再試行し、それでも Malformed にならなければ入力を外して報告する。

- [ ] **Step 4: 件数確認 + clippy / fmt**

Run: `/usr/bin/grep -c '#\[test\]' crates/mat-controller/src/commissioning.rs && cargo fmt -p mat-controller && cargo clippy -p mat-controller --all-targets -- -D warnings`
Expected: `50` 以上、警告ゼロ。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-controller/src/commissioning.rs
git commit -m "test(mat-controller): commissioning デコーダの負系・境界・Display テストを追加（監査レーン D）"
```

---

### Task 5: x509.rs パーサのユニットテスト補強

**Files:**
- Modify: `crates/mat-controller/src/x509.rs`（`#[cfg(test)] mod tests` 末尾に追加）

**Interfaces:**
- Consumes: private 関数 `extract_hex_tag`, `parse_vid_pid`, `int_to_32`, `parse_ecdsa_signature`, `parse_spki`, `check_ecdsa_sha256_alg`, `parse_key_usage`, `parse_basic_constraints`, `parse_validity`, `DerReader`、OID 定数、`crate::asn1`、`crate::cert::key_usage_bits`、`test_support::make_test_cert`、`crate::case::random_p256_secret`。
- Produces: なし（テストのみ）。

- [ ] **Step 1: 既存テスト数を記録**

Run: `/usr/bin/grep -c '#\[test\]' crates/mat-controller/src/x509.rs`
Expected: `10`。

- [ ] **Step 2: テストを追加**

`mod tests` の末尾に追加（先頭の `use super::test_support::{make_test_cert, make_test_csr}; use super::*;` は既存。`use crate::asn1;` は `super::*` 経由で見えるので追加不要）:

```rust
    // ---- 監査レーン D: 各パーサの負系・境界（実機不要） ----

    #[test]
    fn extract_hex_tag_edge_cases() {
        assert_eq!(extract_hex_tag("Mvid:FFF1 Mpid:8000", "Mvid:"), Some(0xFFF1));
        assert_eq!(extract_hex_tag("Mvid:FFF1 Mpid:8000", "Mpid:"), Some(0x8000));
        assert_eq!(extract_hex_tag("Mpid:8000", "Mvid:"), None, "prefix 無し");
        assert_eq!(extract_hex_tag("Mvid:FF", "Mvid:"), None, "4 桁未満");
        assert_eq!(extract_hex_tag("Mvid:ZZZZ", "Mvid:"), None, "非 hex");
        assert_eq!(extract_hex_tag("abc Mvid:", "Mvid:"), None, "末尾 prefix");
        assert_eq!(
            extract_hex_tag("Mvid:あい", "Mvid:"),
            None,
            "マルチバイト境界で str::get が None"
        );
        assert_eq!(extract_hex_tag("Mvid:0000", "Mvid:"), Some(0));
    }

    /// `SET OF { SEQ { oid, value } }` 1 個を組む。
    fn rdn(oid_bytes: &[u8], value: Vec<u8>) -> Vec<u8> {
        asn1::set_of(&[&asn1::seq(&[&asn1::oid(oid_bytes), &value])])
    }

    #[test]
    fn parse_vid_pid_prefers_oid_rdns_and_accepts_printable_string() {
        let name = [
            rdn(OID_CN, asn1::utf8_string("Mvid:1111 Mpid:2222")),
            rdn(OID_MATTER_VID, asn1::utf8_string("FFF1")),
            rdn(OID_MATTER_PID, asn1::printable_string("8001")),
        ]
        .concat();
        assert_eq!(parse_vid_pid(&name).unwrap(), (Some(0xFFF1), Some(0x8001)));
    }

    #[test]
    fn parse_vid_pid_falls_back_to_cn_tags() {
        let name = rdn(OID_CN, asn1::utf8_string("ACME Mvid:FFF2 Mpid:8002"));
        assert_eq!(parse_vid_pid(&name).unwrap(), (Some(0xFFF2), Some(0x8002)));
        // CN に片方だけ → 片方だけ。
        let name = rdn(OID_CN, asn1::utf8_string("Mvid:FFF3"));
        assert_eq!(parse_vid_pid(&name).unwrap(), (Some(0xFFF3), None));
    }

    #[test]
    fn parse_vid_pid_ignores_unusable_values() {
        // 非 UTF-8 の UTF8String → skip
        let name = rdn(OID_MATTER_VID, asn1::tlv(0x0C, &[0xFF, 0xFE]));
        assert_eq!(parse_vid_pid(&name).unwrap(), (None, None));
        // 文字列以外の値タグ（OCTET STRING）→ skip
        let name = rdn(OID_MATTER_VID, asn1::octet_string(b"FFF1"));
        assert_eq!(parse_vid_pid(&name).unwrap(), (None, None));
        // OID RDN の hex 不正 → None（CN フォールバックも無い）
        let name = rdn(OID_MATTER_VID, asn1::utf8_string("XYZ1"));
        assert_eq!(parse_vid_pid(&name).unwrap(), (None, None));
        // 空 Name → (None, None)
        assert_eq!(parse_vid_pid(&[]).unwrap(), (None, None));
        // 壊れた構造（SET でなく SEQ が来る）→ Err
        let bad = asn1::seq(&[&asn1::oid(OID_CN)]);
        assert!(parse_vid_pid(&bad).is_err());
    }

    #[test]
    fn int_to_32_normalizes_der_integers() {
        assert_eq!(int_to_32(&[0x01; 32]).unwrap(), [0x01; 32]);
        let mut with_pad = vec![0x00];
        with_pad.extend_from_slice(&[0x80; 32]);
        assert_eq!(int_to_32(&with_pad).unwrap(), [0x80; 32], "先頭 0x00 を剥がす");
        let mut short = [0u8; 32];
        short[31] = 0x7F;
        assert_eq!(int_to_32(&[0x7F]).unwrap(), short, "左ゼロ詰め");
        assert_eq!(int_to_32(&[0x00]).unwrap(), [0u8; 32], "単独 0x00 は 0");
        assert_eq!(int_to_32(&[0x01; 33]), Err(X509Error::Der("integer out of range")));
        assert_eq!(int_to_32(&[]), Err(X509Error::Der("integer out of range")));
    }

    /// BIT STRING の中身（unused byte + DER）を組む。
    fn sig_bits(unused: u8, der: &[u8]) -> Vec<u8> {
        let mut v = vec![unused];
        v.extend_from_slice(der);
        v
    }

    #[test]
    fn parse_ecdsa_signature_roundtrips_and_rejects_malformed() {
        let mut raw = [0u8; 64];
        raw[..32].copy_from_slice(&[0x11; 32]);
        raw[32..].copy_from_slice(&[0x22; 32]);
        let ok = sig_bits(0, &asn1::ecdsa_signature(&raw));
        assert_eq!(parse_ecdsa_signature(&ok).unwrap(), raw);

        assert_eq!(
            parse_ecdsa_signature(&[]),
            Err(X509Error::Der("empty signature bit string"))
        );
        assert_eq!(
            parse_ecdsa_signature(&sig_bits(1, &asn1::ecdsa_signature(&raw))),
            Err(X509Error::Der("unexpected unused bits in signature"))
        );
        let wrong_inner = asn1::seq(&[&asn1::octet_string(&[0x11; 32]), &asn1::integer(&[0x22; 32])]);
        assert_eq!(
            parse_ecdsa_signature(&sig_bits(0, &wrong_inner)),
            Err(X509Error::Der("unexpected der tag"))
        );
        let too_long = asn1::seq(&[&asn1::integer(&[0x01; 33]), &asn1::integer(&[0x22; 32])]);
        assert_eq!(
            parse_ecdsa_signature(&sig_bits(0, &too_long)),
            Err(X509Error::Der("integer out of range"))
        );
    }

    /// SPKI SEQ の中身を組む。
    fn spki_content(alg: &[u8], curve: &[u8], unused: u8, key: &[u8]) -> Vec<u8> {
        [
            asn1::seq(&[&asn1::oid(alg), &asn1::oid(curve)]),
            asn1::bit_string(unused, key),
        ]
        .concat()
    }

    #[test]
    fn parse_spki_accepts_p256_and_rejects_other_shapes() {
        let key = [0x04u8; 65];
        assert_eq!(
            parse_spki(&spki_content(OID_EC_PUBLIC_KEY, OID_PRIME256V1, 0, &key)).unwrap(),
            key
        );
        const OID_SECP384R1: &[u8] = &[0x2B, 0x81, 0x04, 0x00, 0x22];
        const OID_RSA_ENCRYPTION: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
        assert_eq!(
            parse_spki(&spki_content(OID_EC_PUBLIC_KEY, OID_SECP384R1, 0, &key)),
            Err(X509Error::UnsupportedAlg)
        );
        assert_eq!(
            parse_spki(&spki_content(OID_RSA_ENCRYPTION, OID_PRIME256V1, 0, &key)),
            Err(X509Error::UnsupportedAlg)
        );
        assert_eq!(
            parse_spki(&spki_content(OID_EC_PUBLIC_KEY, OID_PRIME256V1, 0, &[0x02; 33])),
            Err(X509Error::BadPublicKey),
            "圧縮点は拒否"
        );
        assert_eq!(
            parse_spki(&spki_content(OID_EC_PUBLIC_KEY, OID_PRIME256V1, 1, &key)),
            Err(X509Error::BadPublicKey),
            "unused bits ≠ 0"
        );
    }

    #[test]
    fn check_ecdsa_sha256_alg_rejects_other_algorithms() {
        assert!(check_ecdsa_sha256_alg(&asn1::oid(OID_ECDSA_SHA256)).is_ok());
        const OID_SHA256_WITH_RSA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0B];
        assert_eq!(
            check_ecdsa_sha256_alg(&asn1::oid(OID_SHA256_WITH_RSA)),
            Err(X509Error::UnsupportedAlg)
        );
        assert_eq!(
            check_ecdsa_sha256_alg(&asn1::integer(&[1])),
            Err(X509Error::Der("unexpected der tag"))
        );
    }

    #[test]
    fn parse_key_usage_roundtrips_cert_bits_and_rejects_empty() {
        for bits in [0x0001u16, 0x0060, 0x0021] {
            let (unused, bytes) = crate::cert::key_usage_bits(bits);
            let value = asn1::bit_string(unused, &bytes);
            assert_eq!(parse_key_usage(&value).unwrap(), bits, "bits {bits:#06x}");
        }
        assert_eq!(
            parse_key_usage(&asn1::tlv(0x03, &[])),
            Err(X509Error::Der("empty keyUsage bit string"))
        );
        assert_eq!(
            parse_key_usage(&asn1::octet_string(&[0])),
            Err(X509Error::Der("unexpected der tag"))
        );
        // 余分なバイトは無視される（named-bit は 9 本まで見る）。
        assert_eq!(parse_key_usage(&asn1::bit_string(0, &[0x80, 0x00, 0x00])).unwrap(), 0x0001);
    }

    #[test]
    fn parse_basic_constraints_defaults_to_false() {
        assert!(!parse_basic_constraints(&asn1::seq(&[])).unwrap(), "空 SEQUENCE");
        assert!(
            !parse_basic_constraints(&asn1::seq(&[&asn1::integer(&[0])])).unwrap(),
            "先頭が BOOLEAN でない"
        );
        assert!(parse_basic_constraints(&asn1::seq(&[&asn1::boolean(true)])).unwrap());
        assert!(!parse_basic_constraints(&asn1::seq(&[&asn1::boolean(false)])).unwrap());
        assert!(
            !parse_basic_constraints(&asn1::seq(&[&asn1::tlv(0x01, &[])])).unwrap(),
            "BOOLEAN 空内容は DEFAULT FALSE"
        );
        assert_eq!(
            parse_basic_constraints(&asn1::octet_string(&[])),
            Err(X509Error::Der("unexpected der tag"))
        );
    }

    #[test]
    fn parse_validity_handles_time_choice_and_unknown_tags() {
        let both = [
            asn1::utc_time("260101000000Z"),
            asn1::generalized_time("20300101000000Z"),
        ]
        .concat();
        assert_eq!(
            parse_validity(&both).unwrap(),
            (
                Some("260101000000Z".to_string()),
                Some("20300101000000Z".to_string())
            )
        );
        let odd = [asn1::utc_time("260101000000Z"), asn1::integer(&[1])].concat();
        assert_eq!(
            parse_validity(&odd).unwrap(),
            (Some("260101000000Z".to_string()), None),
            "Time 以外のタグは None"
        );
        let non_ascii = [asn1::tlv(0x17, &[0xFF]), asn1::utc_time("260101000000Z")].concat();
        assert_eq!(parse_validity(&non_ascii).unwrap().0, None, "非 UTF-8 は None");
        assert!(parse_validity(&[0x17]).is_err(), "構造が壊れていれば Err");
    }

    #[test]
    fn der_reader_length_forms() {
        let mut r = DerReader::new(&[0x04, 0x01, 0xAB]);
        assert_eq!(r.read().unwrap(), (0x04, &[0xAB][..], &[0x04, 0x01, 0xAB][..]));
        assert!(r.is_empty());

        let mut r = DerReader::new(&[0x04, 0x81, 0x01, 0xAB]);
        assert_eq!(r.read().unwrap().1, &[0xAB]);

        let mut r = DerReader::new(&[0x04, 0x82, 0x00, 0x01, 0xAB]);
        assert_eq!(r.read().unwrap().1, &[0xAB]);

        let mut r = DerReader::new(&[0x04, 0x83, 0x00, 0x00, 0x01, 0xAB]);
        assert_eq!(r.read(), Err(X509Error::Der("unsupported der length form")));

        let mut r = DerReader::new(&[0x04, 0x80, 0xAB, 0x00, 0x00]);
        assert_eq!(r.read(), Err(X509Error::Der("unsupported der length form")), "不定長");

        let mut r = DerReader::new(&[0x04, 0x05, 0xAB]);
        assert_eq!(r.read(), Err(X509Error::Der("truncated content")));

        let mut r = DerReader::new(&[0x04]);
        assert_eq!(r.read(), Err(X509Error::Der("truncated length")));

        let mut r = DerReader::new(&[]);
        assert_eq!(r.read(), Err(X509Error::Der("truncated tag")));

        let mut r = DerReader::new(&[0x04, 0x01, 0xAB]);
        assert_eq!(r.expect(0x30), Err(X509Error::Der("unexpected der tag")));
    }

    #[test]
    fn verify_signed_by_rejects_tampered_signature_and_tbs() {
        let key = crate::case::random_p256_secret();
        let der = make_test_cert(b"self", b"self", &key, &key, true, None);
        let good = parse_x509(&der).unwrap();
        good.verify_signed_by(&good).unwrap();

        let mut bad_sig = good.clone();
        bad_sig.signature[0] ^= 0x01;
        assert_eq!(bad_sig.verify_signed_by(&good), Err(X509Error::BadSignature));

        let mut bad_tbs = good.clone();
        let last = bad_tbs.tbs.len() - 1;
        bad_tbs.tbs[last] ^= 0x01;
        assert_eq!(bad_tbs.verify_signed_by(&good), Err(X509Error::BadSignature));
    }
```

- [ ] **Step 3: テストを実行し、落ちたものを精査**

Run: `cargo test -p mat-controller x509::tests`
Expected: 全 pass。落ちた場合は Task 4 Step 3 と同じ規律（実挙動が docstring と整合ならテスト側を直す、プロダクション変更は不可）。`DerReader::read` の戻り値タプルの比較で型が合わなければ `let (t, c, raw) = r.read().unwrap(); assert_eq!(...)` に分解する。

- [ ] **Step 4: 件数確認 + clippy / fmt**

Run: `/usr/bin/grep -c '#\[test\]' crates/mat-controller/src/x509.rs && cargo fmt -p mat-controller && cargo clippy -p mat-controller --all-targets -- -D warnings`
Expected: `23` 以上（10 + 13）、警告ゼロ。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-controller/src/x509.rs
git commit -m "test(mat-controller): x509 パーサの負系・境界テストを追加（監査レーン D）"
```

---

### Task 6: ドキュメント更新 + `task check` + `task semver`

**Files:**
- Modify: `ARCHITECTURE.md`（M8c-3 訂正ブロック、(b) の記述）
- Modify: `docs/commands.md`（「Op timeout budget」節）
- Modify: `CLAUDE.md`（Backend (native) 節）
- Modify: `docs/superpowers/specs/2026-09-03-case-happy-eyeballs-design.md`（§4.2 #8 の期待値を実挙動に合わせる）

- [ ] **Step 1: ARCHITECTURE.md**

「(b) 死んだアドレス 1 本あたりの CASE 失敗待ちは MRP 予算依存（SII=5000ms なら最大 ~80 秒）で、上限キャップ・並行試行は無い】。」の文を以下に置換:

```
(b) ~~死んだアドレス 1 本あたりの CASE 失敗待ちは MRP 予算依存（SII=5000ms なら
      最大 ~80 秒）で、上限キャップ・並行試行は無い~~【解消 2026-09-03: 候補
      アドレスは `mat-controller::case::establish_any` が Happy Eyeballs 方式で
      確立する — 候補ごとに専用 UDP ソケットを bind し、500ms stagger
      （`case::RACE_STAGGER`）で `case::establish` を起動、最初の成功を採用して
      残りを drop（`mat-controller::race::race_staggered`）。死んだ先頭アドレスの
      損失は ~80 秒 → 0.5 秒。試行ごとの MRP 上限キャップは設けず、全体は従来
      どおり op deadline（`--op-timeout-ms`）が縛る。候補 1 本の完全到達不能
      ノードの所要は不変。Sigma3 まで進んだ敗者はデバイス側の idle 期限で消える。
      mat / matd / 常駐 Subscribe は `mat-native::CaseEstablisher` 経由で同じ
      関数を呼ぶので全経路に効く。(a) は未解消のまま】。
```

- [ ] **Step 2: docs/commands.md**

「Op timeout budget」節の `- **Direct path**: ...` 項目の直後に以下を追加:

```
- **Multiple resolved addresses** (either path): CASE is raced across the
  candidates Happy-Eyeballs style — each candidate gets its own UDP socket,
  attempts start 500 ms apart (non-link-local addresses first), the first
  successful handshake wins and the rest are dropped. A dead first address
  therefore costs ~0.5 s instead of the full MRP budget (~80 s at
  `SII=5000`). The race as a whole still runs under the op budget; a node
  whose only address is unreachable still ends in `timeout` when the budget
  is spent. On total failure the `session_failed` detail lists every
  address tried.
```

- [ ] **Step 3: CLAUDE.md**

「Backend (native)」節の「Route selection is per-op: ...」項目の直後に 1 項目追加:

```
- Multi-address CASE goes through `mat-controller::case::establish_any`
  (staggered race, 500 ms, one UDP socket per candidate). Never reintroduce
  a sequential `for peer in peers { case::establish }` loop in `mat-native`
  or the live tests.
```

- [ ] **Step 4: spec の実挙動追従**

`docs/superpowers/specs/2026-09-03-case-happy-eyeballs-design.md` §4.2 の項目 8
「`parse_basic_constraints`: 空 SEQUENCE → `false` / 先頭が BOOLEAN でない → `false` / BOOLEAN 空内容 → Err。」を
「`parse_basic_constraints`: 空 SEQUENCE → `false` / 先頭が BOOLEAN でない → `false` / BOOLEAN 空内容 → `false`（DEFAULT FALSE、実装の `unwrap_or(0)`）/ 値が SEQUENCE でない → Err。」に修正する。

- [ ] **Step 5: task check**

Run: `task check`
Expected: fmt:check / clippy / test 全て緑。`group::tests::send_invoke_emits_identical_datagram_on_each_egress` だけが落ちたら `cargo test -p mat-controller send_invoke_emits_identical_datagram_on_each_egress` を単独で回して pass を確認する。

- [ ] **Step 6: task semver（棚卸しのみ）**

Run: `task semver 2>&1 | tail -40`
Expected: mat-controller は新規 pub 追加のみ（`race` モジュール、`case::{RACE_STAGGER, Established, EstablishAnyError, establish_any}`）なので minor 判定。他クレートは変更なし。判定結果をそのまま最終報告に載せる（版は上げない — publish 方針は CLAUDE.md）。`cargo-semver-checks` が未インストールで失敗した場合はその旨を報告して先へ進む。

- [ ] **Step 7: Commit**

```bash
git add ARCHITECTURE.md docs/commands.md CLAUDE.md docs/superpowers/specs/2026-09-03-case-happy-eyeballs-design.md
git commit -m "docs: CASE Happy Eyeballs 化の記録（ARCHITECTURE 残余(b) 解消・commands・CLAUDE、監査レーン D）"
```

---

## Task 7（オーケストレータ自身が実施、subagent には出さない）: 実機スモーク → main マージ

spec §5.4 の手順。他セッションの実機スモークと同時に走らせない。

- [ ] musl 静的 x86_64 の `mat` / `matd` をビルド（`Taskfile.yml` の該当タスク、無ければ `cargo build --release --target x86_64-unknown-linux-musl -p mat -p matd`）。
- [ ] hogar-matd コンテナに `*.new` として docker cp、隔離 matd（`--store <本番 store のコピー> --socket /tmp/he.sock`）を起動。
- [ ] 直経路 `read`（node 23 / 24）exit 0、`op transport bound` ログに peer / local。
- [ ] 隔離 matd 経由 `read` exit 0、2 回目は再確立なし。常駐 Subscribe established。
- [ ] `avahi-browse -rtp _matter._tcp` で AAAA 2 本以上のノードを探し、あれば新旧で同じ `read` の所要を比較。無ければ候補 1 本の到達不能ノード（あれば）で新旧とも 60s `timeout` を確認し、「短縮は死+生構成のみ、loopback テストで釘打ち」と記録。
- [ ] WARN / ERROR が既知バースト以外に無い。隔離 matd を `matd.new stop --socket /tmp/he.sock` で止め、コピーと `*.new` を削除。
- [ ] `git fetch && git rebase origin/main` → `task check` → main で `git merge --no-ff worktree-case-happy-eyeballs` → push。
- [ ] メモリ `mat-code-audit-2026-08-31.md` のレーン D に完了追記。worktree / ブランチ削除。
