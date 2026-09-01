# Issue #23: groupcast egress self-heal 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** otbr 再起動で wpan0 の ifindex (scope_id) が失効しても matd の groupcast が自動復旧し、matd が otbr より先に起動した場合も thread egress を後付けで確立する。

**Architecture:** (A) `mat-controller::group::GroupSender::send_invoke` に「送出が ENETUNREACH/ENODEV で失敗 → `if_nametoindex` 再解決 → `IPV6_MULTICAST_IF` 焼き直し → 同一送信内 1 回リトライ」の self-heal を入れる。(B) `GroupSender::add_egress` を追加し、`mat-native::group::send` が「thread egress 未確立なら送信前に `detect_thread_iface_auto()` を引き直して後付け」する。どちらも best-effort: 失敗は warn + LAN 単独継続で、次回送信が自然な再試行になる。

**Tech Stack:** Rust (tokio, tracing)。ワークスペース: `crates/mat-controller` (protocol), `crates/mat-native` (engine)。

**Spec:** 仕様書ファイルは無し (bounded fix)。正は GitHub issue #23 本文 + 本ドキュメント冒頭の Architecture。発症ログ・再現手順は `gh issue view 23`。

## Global Constraints

- `task check` (fmt:check + clippy + test) が各タスクのコミット前に必ず通ること。
- stdout JSON スキーマ・エラー kind・exit code は一切変えない (成功時 `egress` 配列に wpan0 が復活するのは既存スキーマの範囲内)。
- 診断は stderr の `tracing` 構造化ログのみ。
- プロトコル/送出コードは backend クレート (`mat-controller`/`mat-native`) の中だけ。`mat`/`matd` のコマンド層は触らない。
- 新規依存クレート追加禁止 (errno は数値リテラル + コメントで持つ。libc 依存を増やさない)。
- コメント・ログの流儀は既存コードに合わせる (コメントは日本語混じり可、tracing メッセージは英語)。
- コミットメッセージは既存の流儀 (`fix(scope): 日本語要約`)。

## 依存関係

- Task 1 と Task 2 は独立 (どちらが先でもよい)。
- Task 3 は Task 2 の `add_egress`/`egress_count` に依存。
- Task 4 は全タスク後。
- Task 5 (実機 E2E) はメインセッションの手動ゲート。subagent には出さない。

---

### Task 1: scope_id self-heal (`mat-controller::group`)

**Files:**
- Modify: `crates/mat-controller/src/group.rs` (struct `GroupSender` 約 205 行目、`new()` 約 229 行目、`send_invoke()` 約 276 行目、`mod tests`)

**Interfaces:**
- Consumes: `crate::dnssd::iface_index(name: &str) -> std::io::Result<u32>`、`UdpTransport::set_multicast_if_v6(&self, ifindex: u32) -> io::Result<()>`
- Produces: `send_invoke` の self-heal 挙動 (シグネチャ不変)。`GroupSender` に private フィールド `iface_resolver: Box<dyn Fn(&str) -> std::io::Result<u32> + Send + Sync>` (テストは同一モジュール内から直接代入で差し替える)。private fn `is_stale_iface_error(&std::io::Error) -> bool`。

- [ ] **Step 1: 失敗するテストを書く (分類器)**

`crates/mat-controller/src/group.rs` の `mod tests` 内に追加:

```rust
/// issue #23: otbr 再起動で ifindex が失効したときの errno だけを
/// self-heal 対象にする。EADDRNOTAVAIL は iface が生きていて v6 源
/// アドレスが無いだけ (docker0/veth) なので対象外。
#[test]
fn stale_iface_error_classifier() {
    assert!(is_stale_iface_error(
        &std::io::Error::from_raw_os_error(101) // ENETUNREACH (実発生 2026-08-31)
    ));
    assert!(is_stale_iface_error(
        &std::io::Error::from_raw_os_error(19) // ENODEV
    ));
    assert!(!is_stale_iface_error(
        &std::io::Error::from_raw_os_error(99) // EADDRNOTAVAIL
    ));
    assert!(!is_stale_iface_error(&std::io::Error::other("not os")));
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-controller stale_iface_error_classifier`
Expected: コンパイルエラー (`is_stale_iface_error` 未定義)

- [ ] **Step 3: 分類器を実装**

`send_invoke` の直前 (GroupSender impl の外、`is_stale_iface_error` として module レベル) に:

```rust
/// otbr 再起動などで iface が破棄・再作成されると、保持している ifindex
/// (scope_id) が失効して送出がこれらの errno で落ちる (issue #23)。
/// EADDRNOTAVAIL は対象外 — iface は生きていて IPv6 源アドレスが無いだけ
/// (docker0/veth 等) で、ifindex 再解決では直らない。
fn is_stale_iface_error(e: &std::io::Error) -> bool {
    // Linux errno: ENETUNREACH=101, ENODEV=19 (libc 依存を避け数値で持つ)
    matches!(e.raw_os_error(), Some(101) | Some(19))
}
```

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller stale_iface_error_classifier`
Expected: PASS

- [ ] **Step 5: 失敗するテストを書く (self-heal 本体)**

`mod tests` に追加。既存の `group_sender_multicast_loops_back_locally` (group.rs:582) と同じ候補走査流儀:

```rust
/// issue #23 本丸: 失効した scope_id での送出失敗を検知し、iface 名から
/// ifindex を再解決して同一送信内でリトライすること。iface 再作成は
/// テストでは作れないので、「存在しない ifindex を保持した状態」で
/// 失効を再現し、resolver 注入で正しい index へ回復させる。
#[tokio::test]
async fn send_invoke_heals_stale_scope_id_and_retries() {
    use crate::transport::UdpTransport;

    let addr = group_multicast_addr(1, 10);
    let mut tried = Vec::new();

    for cand in multicast_capable_interfaces() {
        let recv = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
        let port = recv.local_addr().unwrap().port();
        if recv.join_multicast_v6(&addr, cand.index).is_err() {
            tried.push(format!("{}(idx={}): join failed", cand.name, cand.index));
            continue;
        }

        let p = tmp_counter_path(&format!("heal-{}", cand.index));
        let _ = std::fs::remove_file(&p);
        let counter = PersistedGroupCounter::load(&p, 0).unwrap();
        let transport = std::sync::Arc::new(UdpTransport::bind().await.unwrap());
        let egress = vec![GroupEgress {
            iface: cand.name.clone(),
            transport,
            scope_id: cand.index,
        }];
        let mut s = GroupSender::new(egress, port, 1, 0x0001_0001, counter).unwrap();

        // otbr 再起動相当: iface 名は同じまま ifindex だけ失効した状態を作る。
        // (sockopt は new() で有効な index に焼かれている — 実障害と同じく
        // 送出時の dest sin6_scope_id / 経路解決が落ちる)
        s.egress[0].scope_id = 0x7fff_fffe; // 存在しない ifindex
        let good = cand.index;
        s.iface_resolver = Box::new(move |_| Ok(good));

        let (sent_counter, sent) = match s
            .send_invoke(&test_creds(), 10, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_file(&p);
                tried.push(format!(
                    "{}(idx={}): send failed: {e:?}",
                    cand.name, cand.index
                ));
                continue;
            }
        };
        assert_eq!(sent, vec![cand.name.clone()], "healed egress must be counted as sent");
        assert_eq!(s.egress[0].scope_id, cand.index, "scope_id must be refreshed");

        let mut buf = [0u8; 1280];
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            recv.recv_from(&mut buf),
        )
        .await;
        let _ = std::fs::remove_file(&p);

        match result {
            Ok(Ok((n, _))) => {
                let (header, _) = MessageHeader::decode(&buf[..n]).unwrap();
                assert_eq!(header.message_counter, sent_counter);
                return; // 配達できる iface が 1 本見つかれば PASS
            }
            _ => tried.push(format!("{}(idx={}): no delivery", cand.name, cand.index)),
        }
    }
    panic!("no candidate healed+delivered; tried: {tried:?}");
}
```

- [ ] **Step 6: 失敗を確認 (errno 実測を兼ねる)**

Run: `cargo test -p mat-controller send_invoke_heals_stale_scope_id -- --nocapture`
Expected: FAIL。`iface_resolver` フィールド未定義のコンパイルエラー → Step 7 実装後に再実行して観測される errno を確認する。

**errno 実測の判断規則:** 存在しない sin6_scope_id への送出 errno がカーネルにより EINVAL(22) になる場合がある。実行して heal が発動せず素通りで `Err` になった場合、`tried` のログで errno を確認し、それが失効 ifindex 由来として妥当 (EINVAL) なら `is_stale_iface_error` に `Some(22)` を追加し、Step 1 の分類器テストにも 1 行足す (コメント付き)。EADDRNOTAVAIL を足すのは禁止。

- [ ] **Step 7: self-heal を実装**

`GroupSender` struct にフィールド追加:

```rust
pub struct GroupSender {
    egress: Vec<GroupEgress>,
    dest_port: u16,
    fabric_id: u64,
    source_node_id: u64,
    counter: PersistedGroupCounter,
    /// 送出失敗時の scope_id 再解決 (issue #23 self-heal)。既定は
    /// `dnssd::iface_index`。テストは同一モジュールから直接差し替える。
    iface_resolver: Box<dyn Fn(&str) -> std::io::Result<u32> + Send + Sync>,
}
```

`new()` の `Ok(Self { ... })` に `iface_resolver: Box::new(|name| crate::dnssd::iface_index(name)),` を追加。

`send_invoke` の egress ループを差し替え (datagram 構築までは現状のまま):

```rust
        let mut sent = Vec::new();
        let mut first_err: Option<std::io::Error> = None;
        let dest_port = self.dest_port;
        let dest_addr = group_multicast_addr(self.fabric_id, group_id);
        let Self {
            egress,
            iface_resolver,
            ..
        } = self;
        for e in egress.iter_mut() {
            let dest = |scope_id: u32| {
                // multicast 宛先では sin6_scope_id が送出 iface を選ぶ
                SocketAddr::V6(SocketAddrV6::new(dest_addr, dest_port, 0, scope_id))
            };
            match e.transport.send_to(&datagram, dest(e.scope_id)).await {
                Ok(_) => sent.push(e.iface.clone()),
                Err(err) if is_stale_iface_error(&err) => {
                    // ifindex 失効の疑い (otbr 再起動で TUN が再作成されると
                    // 名前は同じまま index が変わる — issue #23)。iface 名から
                    // 再解決し、sockopt を焼き直して同一送信内で 1 回だけ
                    // リトライする。再解決失敗 (iface がまだ無い = otbr 起動中)
                    // は従来どおり warn + この egress 脱落 — 次回送信が自然な
                    // 再試行になる。
                    match (iface_resolver)(&e.iface)
                        .and_then(|idx| e.transport.set_multicast_if_v6(idx).map(|()| idx))
                    {
                        Ok(idx) => {
                            let old = e.scope_id;
                            e.scope_id = idx;
                            match e.transport.send_to(&datagram, dest(idx)).await {
                                Ok(_) => {
                                    tracing::info!(iface = %e.iface, old_scope_id = old,
                                        new_scope_id = idx,
                                        "groupcast egress scope_id refreshed; send retried");
                                    sent.push(e.iface.clone());
                                }
                                Err(err2) => {
                                    tracing::warn!(iface = %e.iface, error = %err2,
                                        "groupcast egress send failed after scope_id refresh");
                                    if first_err.is_none() {
                                        first_err = Some(err2);
                                    }
                                }
                            }
                        }
                        Err(re) => {
                            tracing::warn!(iface = %e.iface, error = %err, refresh_error = %re,
                                "groupcast egress send failed; iface re-resolve failed (iface absent?)");
                            if first_err.is_none() {
                                first_err = Some(err);
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(iface = %e.iface, error = %err, "groupcast egress send failed");
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }
        }
```

注意: `self.counter.next()` と `build_group_datagram` (self.source_node_id 等を使う) は destructure より**前**に済ませること (借用分割のため)。

- [ ] **Step 8: テスト通過を確認**

Run: `cargo test -p mat-controller group`
Expected: 新規 2 本を含め全 PASS (既存の egress 系テスト回帰なし)。Step 6 の判断規則に該当したら分類器を直してから再実行。

- [ ] **Step 9: コミット**

```bash
task check
git add crates/mat-controller/src/group.rs
git commit -m "fix(group): 送出失敗時に scope_id を再解決してリトライする self-heal (issue #23)"
```

---

### Task 2: `GroupSender::add_egress` / `egress_count` (`mat-controller::group`)

**Files:**
- Modify: `crates/mat-controller/src/group.rs` (impl GroupSender、`mod tests`)

**Interfaces:**
- Consumes: `MULTICAST_HOP_LIMIT` (同ファイル既存 const)、`UdpTransport` sockopt 群
- Produces: `pub fn add_egress(&mut self, e: GroupEgress) -> std::io::Result<()>`、`pub fn egress_count(&self) -> usize` (Task 3 が使う)

- [ ] **Step 1: 失敗するテストを書く**

`mod tests` に追加:

```rust
/// issue #23 起動順対応の土台: 稼働中の sender へ egress を後付けできる
/// こと。sockopt (hop limit + IPV6_MULTICAST_IF) が new() と同じく焼かれ、
/// 以後の send_invoke が全 egress へ送出することを固定する。
#[tokio::test]
async fn add_egress_applies_sockopts_and_joins_send() {
    use crate::transport::UdpTransport;

    let mut tried = Vec::new();
    for cand in multicast_capable_interfaces() {
        let p = tmp_counter_path(&format!("late-{}", cand.index));
        let _ = std::fs::remove_file(&p);
        let counter = PersistedGroupCounter::load(&p, 0).unwrap();
        let transport = std::sync::Arc::new(UdpTransport::bind().await.unwrap());
        let egress = vec![GroupEgress {
            iface: cand.name.clone(),
            transport,
            scope_id: cand.index,
        }];
        // 受信はしない (送出成功の iface 名リストだけ固定する) が、宛先 port
        // は実 5540 を避けエフェメラルにする (LAN へ流れても無害な宛先)。
        let sink = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
        let port = sink.local_addr().unwrap().port();
        let mut s = GroupSender::new(egress, port, 1, 0x0001_0001, counter).unwrap();
        assert_eq!(s.egress_count(), 1);

        let t2 = std::sync::Arc::new(UdpTransport::bind().await.unwrap());
        s.add_egress(GroupEgress {
            iface: "late0".into(),
            transport: std::sync::Arc::clone(&t2),
            scope_id: cand.index, // 同一 iface の独立 socket (2 本目の実 iface は環境に無い前提)
        })
        .unwrap();
        assert_eq!(s.egress_count(), 2);
        assert_eq!(t2.multicast_if_v6().unwrap(), cand.index);

        match s
            .send_invoke(&test_creds(), 10, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None)
            .await
        {
            Ok((_c, sent)) => {
                let _ = std::fs::remove_file(&p);
                assert_eq!(sent, vec![cand.name.clone(), "late0".to_string()]);
                return;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&p);
                tried.push(format!("{}(idx={}): send failed: {e:?}", cand.name, cand.index));
                continue; // docker0/veth 等の EADDRNOTAVAIL は次候補へ
            }
        }
    }
    panic!("no candidate accepted a 2-egress send; tried: {tried:?}");
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-controller add_egress_applies_sockopts`
Expected: コンパイルエラー (`add_egress`/`egress_count` 未定義)

- [ ] **Step 3: 実装**

`impl GroupSender` に追加 (`bump_counter` の近く):

```rust
    /// 稼働中の sender へ egress を後付けする (issue #23: matd が otbr より
    /// 先に起動して thread egress 無しで構築された場合の遅延確立)。`new()`
    /// と同じ sockopt を焼き、失敗した egress は追加しない (呼び出し側が
    /// warn し、次回送信で再試行する)。counter には触れない。
    pub fn add_egress(&mut self, e: GroupEgress) -> std::io::Result<()> {
        e.transport.set_multicast_hops_v6(MULTICAST_HOP_LIMIT)?;
        e.transport.set_multicast_if_v6(e.scope_id)?;
        self.egress.push(e);
        Ok(())
    }

    /// 現在の egress 本数 (先頭は常に運用 iface)。thread egress 後付け要否の
    /// 判定 (mat-native::group) が読む。
    pub fn egress_count(&self) -> usize {
        self.egress.len()
    }
```

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller add_egress_applies_sockopts`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
task check
git add crates/mat-controller/src/group.rs
git commit -m "feat(group): GroupSender::add_egress / egress_count — egress の後付け確立 (issue #23)"
```

---

### Task 3: thread egress の後付け確立 (`mat-native`)

**Files:**
- Modify: `crates/mat-native/src/group.rs` (GroupCtx、send()、新 private fn、`mod tests` 内の GroupCtx リテラル 3 箇所)
- Modify: `crates/mat-native/src/lib.rs` (Engine::build の GroupCtx 構築 442 行目付近、build doc コメント 357 行目)
- Modify: `crates/matd/src/server.rs` (テスト内 GroupCtx リテラル 1853/1940 行目付近)
- Modify: `crates/mat/src/native_direct.rs` (テスト内 GroupCtx リテラル 2026 行目付近)

**Interfaces:**
- Consumes: Task 2 の `GroupSender::add_egress` / `egress_count`、`crate::iface_select::detect_thread_iface_auto() -> Option<String>`、`mat_controller::dnssd::iface_index`、`mat_controller::transport::UdpTransport::bind()`
- Produces: `GroupCtx` に `pub op_iface: String` と `pub thread_retry: bool` の 2 フィールド追加。private `async fn acquire_late_thread_egress(op_iface: &str, detected: Option<String>, sender: &mut GroupSender)`。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-native/src/group.rs` の `mod tests` に追加:

```rust
/// issue #23 起動順の罠: matd が otbr より先に起動して thread egress 無しで
/// 構築されても、送信時の再検出 (注入) で egress が後付けされること。
/// op iface と同名・検出無し・未解決名は現状維持 (LAN 単独継続)。
#[tokio::test]
async fn late_thread_egress_acquired_and_skipped_correctly() {
    use mat_controller::transport::UdpTransport;

    let Some(cand) = crate::test_support::multicast_capable_interfaces()
        .into_iter()
        .next()
    else {
        panic!("no multicast-capable interface");
    };

    async fn one_egress_sender(tag: &str, cand_index: u32) -> GroupSender {
        let p = std::env::temp_dir().join(format!(
            "mat-native-late-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        let counter = PersistedGroupCounter::load(&p, 0).unwrap();
        let transport = std::sync::Arc::new(UdpTransport::bind().await.unwrap());
        GroupSender::new(
            vec![GroupEgress {
                iface: "op0".into(),
                transport,
                scope_id: cand_index,
            }],
            5540,
            1,
            0x0001_0001,
            counter,
        )
        .unwrap()
    }

    // 検出あり + op iface と別名 → 後付け成功
    let mut s = one_egress_sender("add", cand.index).await;
    acquire_late_thread_egress("op0", Some(cand.name.clone()), &mut s).await;
    assert_eq!(s.egress_count(), 2);

    // op iface と同名 → 張らない (二重送出回避)
    let mut s = one_egress_sender("dup", cand.index).await;
    acquire_late_thread_egress(&cand.name, Some(cand.name.clone()), &mut s).await;
    assert_eq!(s.egress_count(), 1);

    // 検出無し → no-op
    let mut s = one_egress_sender("none", cand.index).await;
    acquire_late_thread_egress("op0", None, &mut s).await;
    assert_eq!(s.egress_count(), 1);

    // 検出名が解決不能 (iface 実在せず) → warn + 現状維持
    let mut s = one_egress_sender("bad", cand.index).await;
    acquire_late_thread_egress("op0", Some("no-such-iface0".into()), &mut s).await;
    assert_eq!(s.egress_count(), 1);
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-native late_thread_egress`
Expected: コンパイルエラー (`acquire_late_thread_egress` 未定義)

- [ ] **Step 3: 実装**

`crates/mat-native/src/group.rs`:

(a) 冒頭の `#[cfg(test)] use mat_controller::transport::UdpTransport;` を無条件 use に変更 (本体コードで使うため)。`use std::sync::Arc;` も同様に本体へ (現在 `#[cfg(test)]`)。

(b) `GroupCtx` へフィールド追加 (`dest_port` と `sender` の間):

```rust
    /// 運用 iface 名 (egress[0] と同じもの)。thread egress 後付け時の
    /// 二重送出回避 (同名なら張らない) に使う。
    pub op_iface: String,
    /// build 時に thread egress を確立できなかった Auto/None 由来の構成なら
    /// true — send のたびに検出を引き直して後付けを試みる (issue #23 起動順の
    /// 罠)。Explicit 指定は build 時に確定する (解決失敗はハードエラー) ため
    /// 常に false。
    pub thread_retry: bool,
```

(c) `send()` の `init_sender` 直後に挿入:

```rust
    let sender = slot.as_mut().expect("built above");
    // thread egress が未確立 (egress = 運用 iface のみ) なら送信前に検出を
    // 引き直す (issue #23: matd が otbr より先に起動した場合の後付け確立)。
    if ctx.thread_retry && sender.egress_count() < 2 {
        acquire_late_thread_egress(
            &ctx.op_iface,
            crate::iface_select::detect_thread_iface_auto(),
            sender,
        )
        .await;
    }
    match sender
        .send_invoke(&creds, group_id, cluster, command, fields.as_deref())
        .await
```

(既存の `slot.as_mut().expect("built above")` 呼び出しはこの `sender` 束縛に置き換える)

(d) private fn を追加:

```rust
/// thread egress の後付け確立 (issue #23 起動順の罠)。best-effort: どの
/// 失敗も warn + LAN 単独継続で、次回送信でまた試す。`detected` は
/// `detect_thread_iface_auto()` の結果を呼び出し側が渡す (テスト注入点)。
async fn acquire_late_thread_egress(
    op_iface: &str,
    detected: Option<String>,
    sender: &mut GroupSender,
) {
    let Some(name) = detected else { return };
    if name == op_iface {
        // 運用 iface と同一なら二重送出になるだけ (build 時の
        // thread_egress_decision と同じ規律)。毎送信で通り得るので debug。
        tracing::debug!(iface = %name, "late thread iface matches operating iface; skipping");
        return;
    }
    let scope_id = match mat_controller::dnssd::iface_index(&name) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(iface = %name, error = %e,
                "late thread iface detected but unresolvable; groupcast stays LAN-only");
            return;
        }
    };
    let transport = match UdpTransport::bind().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(iface = %name, error = %e,
                "late thread egress socket bind failed; groupcast stays LAN-only");
            return;
        }
    };
    match sender.add_egress(GroupEgress {
        iface: name.clone(),
        transport: Arc::new(transport),
        scope_id,
    }) {
        Ok(()) => {
            tracing::info!(iface = %name, scope_id, "groupcast thread egress acquired late")
        }
        Err(e) => tracing::warn!(iface = %name, error = %e,
            "late thread egress sockopt setup failed; groupcast stays LAN-only"),
    }
}
```

(e) `crates/mat-native/src/lib.rs` Engine::build (442 行目付近) の `GroupCtx` 構築を更新:

```rust
        // thread egress を build 時に確立できなかった Auto/None 由来の構成は
        // 送信時再検出の対象にする (issue #23 起動順の罠)。Explicit は build
        // 時に確定 (解決失敗はハードエラー) なので対象外。
        let thread_retry = !matches!(&cfg.thread_iface, Some(ThreadIfaceChoice::Explicit(_)))
            && egress.len() == 1;
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
```

(f) `crates/mat-native/src/lib.rs:357` の build doc コメント「プロセス寿命で不変。」を実態に合わせ更新:

```
    /// KVS から資格情報を1回読み、NOC を自己発行し、UDP transport を bind、
    /// iface の scope_id を解決して実確立器を構築する。op (unicast) 側の
    /// scope_id はプロセス寿命で不変。group egress の scope_id は送信時に
    /// self-heal する (issue #23 — otbr 再起動で wpan0 の ifindex が変わる)。
```

(g) コンパイルエラーになる全 `GroupCtx` リテラルへ 2 フィールドを足す (すべてテスト、値は `op_iface: "op0".into(), thread_retry: false` を既定とし、egress[0].iface に既に実名を使う箇所ではその名前を op_iface に合わせる):
- `crates/mat-native/src/group.rs` tests: 3 箇所 (197/263/302 行目付近)
- `crates/matd/src/server.rs` tests: 2 箇所 (1853/1940 行目付近)
- `crates/mat/src/native_direct.rs` tests: 1 箇所 (2026 行目付近)

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-native late_thread_egress && cargo test -p mat-native group && cargo test -p matd && cargo test -p mat`
Expected: 全 PASS

- [ ] **Step 5: コミット**

```bash
task check
git add crates/mat-native/src/group.rs crates/mat-native/src/lib.rs crates/matd/src/server.rs crates/mat/src/native_direct.rs
git commit -m "fix(native): thread egress を送信時に後付け確立 — matd 先行起動の罠を解消 (issue #23)"
```

---

### Task 4: バージョンと全体検証

**Files:**
- Modify: `Cargo.toml` (workspace.package.version と path+version 併記の依存 7 行: 1.28.0 → 1.29.0)
- Modify: `Cargo.lock` (cargo が更新)

**Interfaces:**
- Consumes: 全タスクの成果
- Produces: v1.29.0 のワークスペース

- [ ] **Step 1: バージョンを上げる**

ルート `Cargo.toml` 内の `"1.28.0"` を全部 `"1.29.0"` に置換 (workspace.package.version + 依存併記)。

```bash
sed -i 's/"1\.28\.0"/"1.29.0"/g' Cargo.toml
cargo check --workspace   # Cargo.lock を追従させる
```

注記: 既に main には pub API 削除 (先行リファクタ) が乗っており、crates.io publish 時の semver 判断は別途 (メモリ済み)。今回の追加 (add_egress 等) は additive で minor bump に収まる。

- [ ] **Step 2: 全体検証**

Run: `task check`
Expected: fmt:check + clippy + 全テスト PASS

- [ ] **Step 3: コミット**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: v1.29.0 — groupcast egress self-heal (issue #23)"
```

---

### Task 5 (手動・メインセッション): 実機 E2E — マージ前必須

subagent には出さない。メインセッションが despliegue/nas-docker の流儀で行う:

- [ ] aarch64 ビルドを NAS の隔離 matd (`*.new` 方式) に配置
- [ ] hogar-otbr を再起動して wpan0 を再作成 → `mat group invoke` (grp11) → matd ログに `scope_id refreshed` が出て JSON `egress` が `["bridge0","wpan0"]` に戻ること
- [ ] matd → otbr の順で起動し直し → 初回 groupcast で `thread egress acquired late` が出ること
- [ ] node 17 (デスクライト, groupcast 専制御) の実点灯確認
- [ ] 合格後に main へマージ
