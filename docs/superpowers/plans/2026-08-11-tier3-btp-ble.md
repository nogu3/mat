# Tier 3: BTP / BLE 修正 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** BTP セッション会計のオフバイワン修正・受信 ack/seq の spec 準拠検証・BLE スキャンの検出漏れ修正（commission/証明書監査 Tier 3 の 5 件、1.27.0）。

**Architecture:** 全変更が `crates/mat-controller/src/btp.rs`（feature 無関係、ローカルでテスト可）と `crates/mat-controller/src/ble.rs`（feature "ble"、ローカルビルド不可 → `cross check` で検証）に閉じる。spec は `docs/superpowers/specs/2026-08-11-tier3-btp-ble-design.md`。

**Tech Stack:** Rust / tokio。テストは btp.rs 内の既存ループバックパターン（`FakePeripheral` + `fake_link()`）を使う。

## Global Constraints

- バージョンは **1.27.0**（workspace `Cargo.toml:6`、現在 1.26.0）。bump は Task 4 の最終コミット。
- btp.rs のテストは `cargo test -p mat-controller btp` で走る（ble feature 不要）。
- **ble feature はローカルでビルドできない**（libdbus ヘッダなし）。ble.rs を触ったら `cross check --target aarch64-unknown-linux-gnu --features ble -p mat-controller` で検証する（dist:arm64 と同じコンテナ、libdbus あり）。
- コミット前に `task check`（fmt:check + clippy + test）。clippy は `-D warnings` 相当なので警告も落ちる。
- リポジトリは公開。実 IP・実 node_id・実証明書をテストに書かない。
- コミットメッセージは既存流儀（`fix(btp): ...` / `test(btp): ...` / `chore: 1.27.0（...）`、日本語可）。

---

### Task 1: 送受シーケンス初期値の番兵化（peer_acked / last_rx_seq = 255）

**Files:**
- Modify: `crates/mat-controller/src/btp.rs`（`SessionState` 定義 ~272-297 行、tests モジュール）

**Interfaces:**
- Consumes: 既存の `SessionState` / `process_incoming` / `FakePeripheral`（すべて btp.rs 内）
- Produces: `SessionState::new()` が `peer_acked = 255` / `last_rx_seq = 255` を返す（Task 2 の seq 検証は `last_rx_seq.wrapping_add(1)` = 0 を初回期待値として使う）

- [ ] **Step 1: 失敗するテストを 2 本書く**

btp.rs の `mod tests` 内、既存 `ack_accounting_handles_seq_wrap_past_255` の直後に追加:

```rust
#[test]
fn ack_of_seq0_is_counted() {
    // 自分の初回フレーム（seq=0）への ack が計上されること。旧実装は
    // peer_acked 初期値 0 のせいで newly = 0.wrapping_sub(0) = 0 になり、
    // 幽霊 unacked=1 が恒久残留していた（実効ウィンドウ -1 + watchdog
    // 述語 unacked>0 の汚染）。
    let mut st = SessionState::new();
    st.tx_seq = 1; // seq=0 を 1 枚送った直後
    st.unacked = 1;
    let pkt = Packet {
        beginning: false,
        ending: false,
        ack: Some(0),
        seq: Some(0),
        msg_len: None,
        payload: vec![],
    };
    assert!(process_incoming(&pkt, &mut st).unwrap().is_none());
    assert_eq!(st.unacked, 0);
    assert_eq!(st.peer_acked, 0);
}

#[tokio::test]
async fn btp_window_one_device_accepts_second_message() {
    // window_size=1 のデバイス相手に 2 通目が送れること（監査の具体症状:
    // 幽霊 unacked=1 で実効ウィンドウが 0 になり、ack 済みなのに 2 通目が
    // ウィンドウ待ちで詰まる）。send_to は actor のキューに積むだけで
    // 完了してしまうので、詰まりの観測は peripheral 側の受信で行う。
    let (link, mut p) = fake_link();
    let peripheral = tokio::spawn(async move {
        p.do_handshake(244, 1).await;
        let (m1, s1) = p.recv_message().await;
        assert_eq!(m1, b"first");
        p.send_ack(s1).await;
        let (m2, _) = p.recv_message().await;
        assert_eq!(m2, b"second");
    });
    let (_, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
    t.send_to(b"first", crate::transport::RELIABLE_PEER)
        .await
        .unwrap();
    t.send_to(b"second", crate::transport::RELIABLE_PEER)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), peripheral)
        .await
        .expect("second message must not stall on a ghost unacked frame")
        .unwrap();
}
```

- [ ] **Step 2: 失敗を確認する**

Run: `cargo test -p mat-controller btp::tests::ack_of_seq0_is_counted btp::tests::btp_window_one_device_accepts_second_message`
（2 本走らせるなら `cargo test -p mat-controller btp` でフィルタして個別名を確認でも可）

Expected: `ack_of_seq0_is_counted` は `assert_eq!(st.unacked, 0)` で FAIL（実際は 1）。`btp_window_one_device_accepts_second_message` は 3 秒 timeout の expect で FAIL。

- [ ] **Step 3: SessionState を番兵初期化に変える**

`SessionState` のフィールドコメントと `new()` を変更:

```rust
struct SessionState {
    tx_seq: u8,        // 次に使う自分の sequence
    peer_acked: u8,    // peer が ack 済みの自分の最新 seq（255 = 番兵、未 ack）
    unacked: u8,       // 未 ack の自分のフレーム数
    last_rx_seq: u8,   // 受信した最新の peer seq（255 = 番兵、未受信）
    pending_ack: bool, // peer へ ack を返す義務があるか
    reasm: Reassembler,
    // レビュー指摘対応（fix wave 1）:
    last_ack_progress: tokio::time::Instant, // 直近で unacked が減った時刻
    segs_since_ack: u8,                      // 直近の送信ack以降に受信した「実データ」segment数
}

impl SessionState {
    fn new() -> Self {
        Self {
            tx_seq: 0,
            // 「まだ何も ack / 受信していない」の番兵 255。u8 wrap 会計で初回
            // seq=0 が (255+1)=0 として自然に繋がる。0 初期化だと自分の初回
            // フレーム（seq=0）への ack が newly=0 となり永久未計上だった。
            peer_acked: 0u8.wrapping_sub(1),
            unacked: 0,
            last_rx_seq: 0u8.wrapping_sub(1),
            pending_ack: false,
            reasm: Reassembler::new(),
            last_ack_progress: tokio::time::Instant::now(),
            segs_since_ack: 0,
        }
    }
}
```

`last_rx_seq` の番兵 255 が ack 値として線に出る経路はない: `send_standalone` も piggyback（`with_ack`）も `pending_ack == true` が前提で、`pending_ack` は sequenced フレームを受信して `last_rx_seq` が実値に更新されたときにしか立たない。

- [ ] **Step 4: テストが通ることを確認する**

Run: `cargo test -p mat-controller btp`
Expected: 新 2 本を含め全 PASS（既存テストは `FakePeripheral` が正しい連番を刻んでいるため無傷。手組みの `ack_accounting_handles_seq_wrap_past_255` は `peer_acked` を明示上書きしているので影響なし）。

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/btp.rs
git commit -m "fix(btp): peer_acked/last_rx_seq を番兵 255 初期化に修正（監査 Tier3: seq-0 ack 未計上のオフバイワン）"
```

---

### Task 2: 受信 ack / seq の妥当性検証（spec 準拠 close）

**Files:**
- Modify: `crates/mat-controller/src/btp.rs`（`process_incoming` ~302-325 行、`run_session` の `Err(e)` ログ ~386 行、tests モジュール）

**Interfaces:**
- Consumes: Task 1 の番兵初期化（`last_rx_seq = 255` → 初回期待 seq = `wrapping_add(1)` = 0）
- Produces: `process_incoming` が不正 ack / 連番飛び seq で `Err(BtpError::Protocol(...))` を返す（呼び出し元 2 箇所は既存のエラー分岐で session close する — 変更不要）

- [ ] **Step 1: 失敗するテストを 2 本書く**

btp.rs の `mod tests` 内に追加:

```rust
#[tokio::test]
async fn btp_closes_on_ack_for_unsent_frame() {
    // 送っていないフレームへの ack（newly > unacked）は本物の破損
    // （GATT indication は ATT 層で信頼配送）— spec 準拠でセッション close。
    let (link, mut p) = fake_link();
    let peripheral = tokio::spawn(async move {
        p.do_handshake(244, 4).await;
        // client は sequenced フレームを 1 枚も送っていないのに ack=5
        p.to_client
            .send(encode_standalone_ack(0, 5).to_vec())
            .await
            .unwrap();
        p // drop するとチャネル閉鎖で修正なしでも close してしまう — 生かして返す
    });
    let (_, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
    let p = peripheral.await.unwrap();
    let mut buf = [0u8; 64];
    let err = tokio::time::timeout(std::time::Duration::from_secs(2), t.recv_from(&mut buf))
        .await
        .expect("session must close on invalid ack")
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    drop(p);
}

#[tokio::test]
async fn btp_closes_on_seq_gap() {
    // 連番でない seq も同様に close（indication は順序保証されるため、
    // 飛びは本物のバグか破損）。
    let (link, mut p) = fake_link();
    let peripheral = tokio::spawn(async move {
        p.do_handshake(244, 4).await;
        p.send_message(b"ok", 244, None).await; // seq=0 正常
        // seq=1 を飛ばして seq=2
        let frame = encode_data_packet(
            2,
            None,
            SegmentPos::First { ending: true },
            Some(2),
            b"ng",
        );
        p.to_client.send(frame).await.unwrap();
    });
    let (_, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
    let mut buf = [0u8; 64];
    let (n, _) = t.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ok");
    let err = t.recv_from(&mut buf).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    peripheral.await.unwrap();
}
```

- [ ] **Step 2: 失敗を確認する**

Run: `cargo test -p mat-controller btp::tests::btp_closes_on_ack_for_unsent_frame btp::tests::btp_closes_on_seq_gap`

Expected: 両方 FAIL — 現行は不正 ack を `saturating_sub` で黙って飲むため session が閉じず、`btp_closes_on_ack_for_unsent_frame` は 2 秒 timeout の expect で FAIL。`btp_closes_on_seq_gap` は seq 飛びが無検証で "ng" が普通に配送され、`unwrap_err()` の panic で FAIL。

- [ ] **Step 3: process_incoming に検証を入れる**

`process_incoming` を次の形に変更（ack 側: 検証 + `saturating_sub` → 検証済み減算。seq 側: 連番チェック追加）:

```rust
fn process_incoming(pkt: &Packet, st: &mut SessionState) -> Result<Option<Vec<u8>>, BtpError> {
    if let Some(a) = pkt.ack {
        // a..=直前 tx_seq-1 のうち ack された分を勘定（u8 wrap 対応）。
        let newly = a.wrapping_sub(st.peer_acked);
        // 送っていないフレームへの ack は本物の破損（GATT indication は
        // ATT 層で信頼配送）— spec 準拠で close。newly == 0 の重複 ack は
        // piggyback で毎フレーム来る合法パターン。
        if newly > st.unacked {
            return Err(BtpError::Protocol("ack for unsent frame"));
        }
        if newly > 0 {
            st.last_ack_progress = tokio::time::Instant::now();
        }
        st.unacked -= newly;
        st.peer_acked = a;
    }
    if let Some(s) = pkt.seq {
        // indication は順序保証されるため連番以外は破損 — spec 準拠で close。
        if s != st.last_rx_seq.wrapping_add(1) {
            return Err(BtpError::Protocol("out-of-order seq"));
        }
        st.last_rx_seq = s;
        st.pending_ack = true;
        // payload が空 = standalone ack/keepalive（Reassembler::push と同じ
        // 判定基準）。実データを運ぶ segment だけを積算対象にする — でない
        // と「ack への ack」が無限に連鎖してしまう（純粋な ack 交換は
        // 完了/keepalive 任せのままでよい。brief の対象は複数segmentに
        // またがる実メッセージの詰まり）。
        if !pkt.payload.is_empty() {
            st.segs_since_ack = st.segs_since_ack.saturating_add(1);
        }
    }
    st.reasm.push(pkt)
}
```

`run_session` の再構成エラーの warn は ack/seq 違反も通るようになるため文言を汎用化:

```rust
Err(e) => { tracing::warn!(error=%e, "btp protocol violation"); break; }
```

- [ ] **Step 4: 全テスト通過を確認する**

Run: `cargo test -p mat-controller btp`
Expected: 全 PASS。特に既存 `ack_accounting_handles_seq_wrap_past_255`（手組み state: `peer_acked=254, unacked=4`, ack=1 → newly=3 ≤ 4 で合法、seq=0 は番兵 255 の次で合法）が無傷であること。

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/btp.rs
git commit -m "fix(btp): 受信 ack/seq を検証し違反は spec 準拠でセッション close（監査 Tier3）"
```

---

### Task 3: ウィンドウ待ち中の send_standalone ガード + 64KiB 超メッセージ拒否

**Files:**
- Modify: `crates/mat-controller/src/btp.rs`（`send_message` ~441-512 行、tests モジュール）

**Interfaces:**
- Consumes: Task 1-2 適用済みの `process_incoming` / `SessionState`
- Produces: `send_message` が `msg.len() > u16::MAX` で `Err(BtpError::Protocol("message too long for btp"))`；ウィンドウ満杯中は standalone ack を送らない

- [ ] **Step 1: 失敗するテストを 2 本書く**

btp.rs の `mod tests` 内に追加:

```rust
#[tokio::test]
async fn btp_no_standalone_ack_while_window_full() {
    // window=1: 2 セグメント送信の 1 枚目で満杯。待機中に peer のメッセージが
    // 完成しても、残量ゼロのまま standalone ack を送ってはならない（こちらの
    // 送信ウィンドウ違反になる）。ack はウィンドウ解放後に送られる。
    let (link, mut p) = fake_link();
    let msg: Vec<u8> = (0u8..40).collect(); // segment 30 → 2 フレーム
    let peripheral = tokio::spawn(async move {
        p.do_handshake(30, 1).await;
        let f1 = p.from_client.recv().await.unwrap();
        let s1 = Packet::decode(&f1).unwrap().seq.unwrap();
        // ack を積まない完全なメッセージ → client のウィンドウは満杯のまま
        p.send_message(b"hi", 30, None).await;
        // 満杯の間は standalone ack を含む一切のフレームが来てはならない
        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(300), p.from_client.recv())
                .await;
        assert!(blocked.is_err(), "client must not send while window is full");
        p.send_ack(s1).await; // 解放
        // 解放後は溜まっていた ack（proactive、ウィンドウを再消費する）を
        // 都度 ack で返しながら、2 枚目のデータセグメントを待つ。
        loop {
            let f = p.from_client.recv().await.unwrap();
            let pkt = Packet::decode(&f).unwrap();
            if !pkt.payload.is_empty() {
                break;
            }
            p.send_ack(pkt.seq.unwrap()).await;
        }
    });
    let (_, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
    let send_fut = t.send_to(&msg, crate::transport::RELIABLE_PEER);
    let mut buf = [0u8; 64];
    let recv_fut = t.recv_from(&mut buf);
    let (send_res, recv_res) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(send_fut, recv_fut)
    })
    .await
    .expect("send/recv must not hang");
    send_res.unwrap();
    let (n, _) = recv_res.unwrap();
    assert_eq!(&buf[..n], b"hi");
    tokio::time::timeout(std::time::Duration::from_secs(2), peripheral)
        .await
        .expect("peripheral must not hang")
        .unwrap();
}

#[tokio::test]
async fn btp_rejects_message_longer_than_u16_max() {
    // 宣言長は u16。65535 を超えるメッセージは `as u16` の黙った切り捨てでは
    // なくエラー（セッション close）にする。
    let (link, mut p) = fake_link();
    let peripheral = tokio::spawn(async move {
        p.do_handshake(244, 8).await;
        // 修正後はデータフレームが 1 枚も来ないまま writer が閉じる。
        // 切り捨てられた宣言長（70000 % 65536 = 4464）が流れてきたら失敗。
        while let Some(frame) = p.from_client.recv().await {
            let pkt = Packet::decode(&frame).unwrap();
            assert_ne!(pkt.msg_len, Some(4464), "truncated msg_len leaked");
        }
    });
    let (_, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
    let msg = vec![0u8; 70_000];
    let _ = t.send_to(&msg, crate::transport::RELIABLE_PEER).await;
    let mut buf = [0u8; 64];
    let err = tokio::time::timeout(std::time::Duration::from_secs(2), t.recv_from(&mut buf))
        .await
        .expect("session must close promptly on oversized message")
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    peripheral.await.unwrap();
}
```

- [ ] **Step 2: 失敗を確認する**

Run: `cargo test -p mat-controller btp::tests::btp_no_standalone_ack_while_window_full btp::tests::btp_rejects_message_longer_than_u16_max`

Expected: `btp_no_standalone_ack_while_window_full` は「満杯中に standalone ack が届いてしまう」ため `blocked.is_err()` の assert で FAIL。`btp_rejects_message_longer_than_u16_max` は切り捨てフレームが流れて peripheral の `assert_ne!` panic か、2 秒 timeout の expect で FAIL。

- [ ] **Step 3: send_message を修正する**

冒頭に長さチェック、ウィンドウ待ちループ内の `send_standalone` にガード:

```rust
async fn send_message(
    link: &mut GattLink,
    st: &mut SessionState,
    params: &BtpParams,
    msg: &[u8],
    in_tx: &mpsc::Sender<Vec<u8>>,
) -> Result<(), BtpError> {
    // 宣言長（Beginning segment の Message Length）は u16。超過は切り捨てず
    // 拒否する（Matter メッセージは実際には遥かに小さい — 防御）。
    if msg.len() > usize::from(u16::MAX) {
        return Err(BtpError::Protocol("message too long for btp"));
    }
    let mut off = 0usize;
    ...
            match process_incoming(&pkt, st)? {
                Some(m) => {
                    if in_tx.send(m).await.is_err() {
                        return Err(BtpError::Closed);
                    }
                    // ウィンドウに残量があるときだけ即 ack。満杯なら
                    // pending_ack を立てたまま piggyback / keepalive /
                    // proactive ack に任せる（keepalive 経路と同じガード —
                    // 無条件送出はこちらの送信ウィンドウ違反）。
                    if st.unacked < params.window_size {
                        send_standalone(link, st).await?;
                    }
                }
    ...
```

（`...` 部分は既存コードのまま。変更は「冒頭の長さチェック追加」と「`Some(m)` 分岐の `send_standalone` を `if st.unacked < params.window_size` で包む」の 2 点のみ。）

- [ ] **Step 4: 全テスト通過を確認する**

Run: `cargo test -p mat-controller btp`
Expected: 全 PASS。既存 `btp_delivers_message_received_while_window_full` は peer が ack 相乗りメッセージを送る（= 処理時点で unacked が減っていて残量あり → standalone ack は従来どおり出る）ため無傷のはず。

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/btp.rs
git commit -m "fix(btp): ウィンドウ満杯中の standalone ack 抑止 + 64KiB 超メッセージ拒否（監査 Tier3）"
```

---

### Task 4: BLE スキャンの PropertiesChanged 対応 + 1.27.0 bump

**Files:**
- Modify: `crates/mat-controller/src/ble.rs:57`（`find_commissionable`）
- Modify: `Cargo.toml:6`（version）、`Cargo.lock`（追従）

**Interfaces:**
- Consumes: なし（独立変更）
- Produces: なし（挙動は「検出できるケースが増える」のみ、シグネチャ不変）

- [ ] **Step 1: discover_devices_with_changes へ切り替える**

`crates/mat-controller/src/ble.rs` の `find_commissionable` 内 1 行 + コメント:

```rust
    adapter.set_powered(true).await.map_err(gatt("power"))?;
    // discover_devices() は DeviceAdded しか流さず、BlueZ にキャッシュ済みの
    // デバイスがスキャン開始後に広告を始めるケース（目の前でペアリング
    // ボタンを押す）を検出できない。_with_changes はプロパティ変更時にも
    // DeviceAdded を再発行するため、以降のマッチループは無変更でよい。
    let mut events = adapter
        .discover_devices_with_changes()
        .await
        .map_err(gatt("discover"))?;
```

ble.rs はユニットテスト不能（bluer 実物依存）。ロジック変更はこの 1 呼び出しのみで、検証は実機（E2E: スキャン開始後にペアリングモード入り）で行う。

- [ ] **Step 2: ble feature がコンパイルできることを確認する**

Run: `cross check --target aarch64-unknown-linux-gnu --features ble -p mat-controller`
Expected: エラーなし（ローカル直ビルドは libdbus 欠如で不可 — cross コンテナで行うこと）。

- [ ] **Step 3: コミット**

```bash
git add crates/mat-controller/src/ble.rs
git commit -m "fix(ble): スキャンを discover_devices_with_changes に切替（監査 Tier3: スキャン開始後の広告開始を検出）"
```

- [ ] **Step 4: バージョンを 1.27.0 に上げる**

`Cargo.toml:6` の `version = "1.26.0"` → `"1.27.0"`。その後 `cargo check` を一度走らせて `Cargo.lock` を追従させる。

- [ ] **Step 5: task check 全緑を確認する**

Run: `task check`
Expected: fmt:check / clippy / test すべて成功。

- [ ] **Step 6: コミット**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: 1.27.0（BTP 会計オフバイワン・ack/seq 検証・BLE スキャン検出 — 監査 Tier3）"
```

---

## マージ前検証（計画外・メインセッションで実施）

1. `task dist:arm64` → jarvis へ `*.new` 転送（本番未置換のまま、既存規律 [[e2e-before-merge]]）
2. jarvis 通常 E2E（BLE 非経由の退行なし確認）
3. **BLE commission 実走**: 余剰デバイスを decommission → jarvis から BLE+Thread で再 commission。確認点: (a) ConnectNetwork 後の Thread 参加待ちで切断されないこと (b) スキャン開始**後**にペアリングモード入りしたデバイスを検出できること（変更 3 の実機担保）
4. 合格後 main マージ・push、監査バックログのメモリ更新

## 監査バックログとの対応

| 監査項目 | タスク |
|---|---|
| btp.rs:288 peer_acked 初期値 0 のオフバイワン | Task 1 |
| btp.rs:305/312 受信 ack/seq の妥当性未検証 | Task 2 |
| btp.rs:464 ウィンドウ待ち中の send_standalone が残量未確認 | Task 3 |
| btp.rs:499 `len() as u16` 切り捨て | Task 3 |
| ble.rs:57 スキャンが DeviceAdded のみ監視 | Task 4 |
