# groupcast Thread iface 直送（二重送出）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** groupcast を運用 iface（LAN）に加えて Thread TUN（wpan*）へも直接送出し、他社 Primary BBR の中継に依存しない配達経路を作る。

**Architecture:** spec は `docs/superpowers/specs/2026-08-10-groupcast-thread-iface-design.md`。Thread iface は起動時に 1 回決定（明示 env/flag > `wpan*` 一意自動検出 > なし=従来動作）。`GroupSender` を egress リスト方式に変え、counter 1 個で組んだ同一 datagram を全 egress へ sendto する。明示指定の解決失敗はハードエラー、自動検出の失敗は warn + LAN 単独続行。

**Tech Stack:** Rust (tokio)。crates: `mat-controller`（送出コア）→ `mat-native`（エンジン配線）→ `mat-core`（JSON body）→ `mat` / `matd`（CLI/常駐）。

## Global Constraints

- コメント・コミットメッセージは既存流儀どおり日本語。
- コミット前に `task check`（fmt:check + clippy + test）を通す。
- stdout は純 JSON、診断は stderr `tracing`（設計ルール 2/3）。
- 新しい error kind は設けない（iface 解決失敗は既存の `other`）。
- 既存挙動の絶対互換: Thread iface が決まらない環境では従来と完全同一動作。
- wpan0 の operstate は **`unknown`**（TUN）。`up` 判定に operstate を使うと
  wpan0 を取りこぼす — flags の `IFF_UP` で判定する。

---

### Task 1: `iface_select::detect_thread_iface`

**Files:**
- Modify: `crates/mat-native/src/iface_select.rs`

**Interfaces:**
- Produces: `pub fn detect_thread_iface(infos: &[IfaceInfo]) -> Option<String>`（純関数）
- Produces: `pub fn detect_thread_iface_auto() -> Option<String>`（`scan()` を使う実走査版。scan 失敗も None）

- [ ] **Step 1: 失敗するテストを書く**

`iface_select.rs` の `mod tests` に追加:

```rust
// wpan0 (OTBR TUN): up|pointopoint|multicast, operstate は "unknown"(=up 扱いしない)
const WPAN: u32 = 0x1 | 0x10 | 0x1000;

#[test]
fn thread_iface_detects_single_wpan() {
    let infos = [
        ifi("eth0", ETH, true, true),
        ifi("wpan0", WPAN, false, true), // operstate unknown → up=false でも採用
    ];
    assert_eq!(detect_thread_iface(&infos), Some("wpan0".into()));
}

#[test]
fn thread_iface_requires_wpan_prefix() {
    // tailscale0 は wpan と同じ flags 形状だが名前で不採用
    let infos = [ifi("eth0", ETH, true, true), ifi("tailscale0", TS, true, true)];
    assert_eq!(detect_thread_iface(&infos), None);
}

#[test]
fn thread_iface_ambiguous_wpan_is_none() {
    let infos = [ifi("wpan0", WPAN, true, true), ifi("wpan1", WPAN, true, true)];
    assert_eq!(detect_thread_iface(&infos), None);
}

#[test]
fn thread_iface_down_wpan_is_none() {
    // IFF_UP なし
    let infos = [ifi("wpan0", 0x10 | 0x1000, false, true)];
    assert_eq!(detect_thread_iface(&infos), None);
}
```

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat-native iface_select`
Expected: FAIL（`detect_thread_iface` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装**

`select()` の下に追加:

```rust
/// Thread TUN 自動検出（groupcast egress 用 — spec 設計 1）。
/// 名前が `wpan` で始まり IFF_UP|IFF_MULTICAST な iface がちょうど 1 本
/// あればそれ。0 または複数は None（従来動作 = LAN 単独送出）。
/// operstate は見ない — TUN は carrier 概念が無く "unknown" になる。
pub fn detect_thread_iface(infos: &[IfaceInfo]) -> Option<String> {
    let mut names: Vec<&IfaceInfo> = infos
        .iter()
        .filter(|i| {
            i.name.starts_with("wpan")
                && i.flags & IFF_UP != 0
                && i.flags & IFF_MULTICAST != 0
        })
        .collect();
    match names.len() {
        1 => Some(names.remove(0).name.clone()),
        _ => None,
    }
}

/// `detect_thread_iface` の実走査版。scan 失敗も None（auto は best-effort）。
pub fn detect_thread_iface_auto() -> Option<String> {
    scan().ok().and_then(|infos| detect_thread_iface(&infos))
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native iface_select`
Expected: PASS（既存テスト含め全緑）

- [ ] **Step 5: Commit**

```bash
git add crates/mat-native/src/iface_select.rs
git commit -m "feat(mat-native): Thread TUN (wpan*) の自動検出 detect_thread_iface を追加"
```

---

### Task 2: `GroupSender` の egress リスト化（mat-controller）

**Files:**
- Modify: `crates/mat-controller/src/group.rs`（`GroupSender` 本体と tests）

**Interfaces:**
- Produces:

```rust
#[derive(Clone)]
pub struct GroupEgress {
    pub iface: String,          // ログ/JSON 用の iface 名
    pub transport: Arc<UdpTransport>,
    pub scope_id: u32,
}

impl GroupSender {
    pub fn new(
        egress: Vec<GroupEgress>,   // 先頭 = 運用 iface（従来挙動）。1 本以上
        dest_port: u16,
        fabric_id: u64,
        source_node_id: u64,
        counter: PersistedGroupCounter,
    ) -> std::io::Result<Self>;

    /// 戻り値: (使用 counter, 送出に成功した egress の iface 名)。
    /// 全 egress 失敗のときだけ Err（最初の失敗を返す）。
    pub async fn send_invoke(
        &mut self,
        creds: &GroupCredentials,
        group_id: u16,
        cluster: u32,
        command: u32,
        fields_tlv: Option<&[u8]>,
    ) -> Result<(u32, Vec<String>), GroupSendError>;
}
```

- `bump_counter` は無変更。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/group.rs` の `mod tests` に追加。lo は multicast join
不可のため、2 egress とも「同じ iface」を使い**独立ソケット 2 本から同一バイト列が
2 回届く**ことを固定する（iface 探索は mat-native 側の既存テストと同じ要領で
`if_nametoindex` 相当を `dnssd::iface_index` は使わず、テスト内では受信側 join が
成功した iface を採用する。join できる iface がなければ skip ではなく既存テスト同様
候補走査でエラー内容を出す）:

```rust
#[tokio::test]
async fn send_invoke_emits_identical_datagram_on_each_egress() {
    // 受信側: 同一 multicast group に join できた iface を 1 つ探す
    //（mat-native の group_invoke_sends_multicast_and_reports_sent と同じ制約。
    //  ここは GroupSender 単体なので 1 iface に 2 ソケットで受ける）。
    let addr = group_multicast_addr(1, 10);
    let mut chosen = None;
    for idx in 1u32..=64 {
        let r1 = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
        if r1.join_multicast_v6(&addr, idx).is_ok() {
            let r2 = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
            r2.join_multicast_v6(&addr, idx).unwrap();
            chosen = Some((idx, r1, r2));
            break;
        }
    }
    let Some((idx, recv1, recv2)) = chosen else {
        eprintln!("no multicast-capable iface; skipping");
        return;
    };
    let port = recv1.local_addr().unwrap().port();
    // 注: 2 ソケットが別ポートになるため、送信は recv1 のポート宛のみ届く。
    // 独立配達の検証は「egress 2 本 → 同一ポートへ 2 датagram」で行う:
    // SO_REUSEPORT は使わず、egress ごとに宛先ポートを分けられないので
    // ここでは 1 ソケット受信で「2 回届く」ことを確認する。
    drop(recv2);

    let e1 = GroupEgress {
        iface: "egress-a".into(),
        transport: Arc::new(UdpTransport::bind().await.unwrap()),
        scope_id: idx,
    };
    let e2 = GroupEgress {
        iface: "egress-b".into(),
        transport: Arc::new(UdpTransport::bind().await.unwrap()),
        scope_id: idx,
    };
    let p = tmp_counter_path("dual-egress");
    let _ = std::fs::remove_file(&p);
    let counter = PersistedGroupCounter::load(&p, 0).unwrap();
    let creds = test_credentials(); // 既存テストヘルパが無ければ fixture から構築
    let mut s = GroupSender::new(vec![e1, e2], port, 1, 0x0001_0001, counter).unwrap();
    let (counter_used, sent) = s
        .send_invoke(&creds, 10, 6, 1, None)
        .await
        .unwrap();
    assert_eq!(sent, vec!["egress-a".to_string(), "egress-b".to_string()]);
    assert!(counter_used > 0);

    // 同一バイト列が 2 回届く（egress ごとに 1 回）
    let mut buf1 = [0u8; 1280];
    let (n1, _) = tokio::time::timeout(
        std::time::Duration::from_millis(500), recv1.recv_from(&mut buf1))
        .await.expect("first datagram").unwrap();
    let mut buf2 = [0u8; 1280];
    let (n2, _) = tokio::time::timeout(
        std::time::Duration::from_millis(500), recv1.recv_from(&mut buf2))
        .await.expect("second datagram").unwrap();
    assert_eq!(&buf1[..n1], &buf2[..n2], "同一 counter の同一 datagram");
    let _ = std::fs::remove_file(&p);
}
```

`test_credentials()` が存在しない場合は、既存の `build_group_datagram` テストや
`mat-native::test_support::write_group_fixture_ini` の keyset 値を流用して
`GroupCredentials` を直接構築するヘルパを同 `mod tests` 内に書く
（`kvs::read_group_credentials` をフィクスチャ ini に対して呼ぶのが最short）。

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat-controller group::`
Expected: FAIL（`GroupEgress` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装**

`GroupSender` を書き換え:

```rust
/// groupcast の送出先 1 本分。transport は egress 専用（unicast と共有しない）。
#[derive(Clone)]
pub struct GroupEgress {
    pub iface: String,
    pub transport: Arc<UdpTransport>,
    pub scope_id: u32,
}

pub struct GroupSender {
    egress: Vec<GroupEgress>,
    dest_port: u16,
    fabric_id: u64,
    source_node_id: u64,
    counter: PersistedGroupCounter,
}

impl GroupSender {
    pub fn new(
        egress: Vec<GroupEgress>,
        dest_port: u16,
        fabric_id: u64,
        source_node_id: u64,
        counter: PersistedGroupCounter,
    ) -> std::io::Result<Self> {
        // 各 egress socket に hop limit と IPV6_MULTICAST_IF を焼く
        //（従来コメントどおり: 宛先 sin6_scope_id だけでは egress を選べない
        //  環境があるため明示固定。multicast 送信専用オプション）。
        for e in &egress {
            e.transport.set_multicast_hops_v6(MULTICAST_HOP_LIMIT)?;
            e.transport.set_multicast_if_v6(e.scope_id)?;
        }
        Ok(Self { egress, dest_port, fabric_id, source_node_id, counter })
    }

    pub async fn send_invoke(
        &mut self,
        creds: &GroupCredentials,
        group_id: u16,
        cluster: u32,
        command: u32,
        fields_tlv: Option<&[u8]>,
    ) -> Result<(u32, Vec<String>), GroupSendError> {
        let counter = self.counter.next().map_err(GroupSendError::Io)?;
        let mut ex = [0u8; 2];
        getrandom::getrandom(&mut ex).expect("os rng");
        let datagram = build_group_datagram(
            creds, self.source_node_id, counter, u16::from_le_bytes(ex),
            group_id, cluster, command, fields_tlv,
        )
        .map_err(GroupSendError::Crypto)?;
        let mut sent = Vec::new();
        let mut first_err: Option<std::io::Error> = None;
        for e in &self.egress {
            let dest = SocketAddr::V6(SocketAddrV6::new(
                group_multicast_addr(self.fabric_id, group_id),
                self.dest_port,
                0,
                e.scope_id,
            ));
            match e.transport.send_to(&datagram, dest).await {
                Ok(_) => sent.push(e.iface.clone()),
                Err(err) => {
                    tracing::warn!(iface = %e.iface, error = %err,
                        "groupcast egress send failed");
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }
        }
        if sent.is_empty() {
            return Err(GroupSendError::Io(
                first_err.unwrap_or_else(|| std::io::Error::other("no egress")),
            ));
        }
        Ok((counter, sent))
    }
    // bump_counter は無変更
}
```

同ファイル内・同 crate 内の `GroupSender::new` / `send_invoke` 既存呼び出し
（テスト含む）を新シグネチャに追従させる（単一 egress は
`vec![GroupEgress { iface: "test".into(), transport, scope_id }]`）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller group::`
Expected: PASS

注: この時点で `mat-native` はコンパイルが壊れる（次 Task で追従）。
コミット単位を workspace 緑に保つため、**Task 3 と同一コミットにしてよい**。
その場合このコミットは Task 3 Step 5 で行う。`cargo test -p mat-controller`
が緑ならここでは commit せず Task 3 へ進む。

---

### Task 3: mat-native 配線（NativeConfig / Engine::build / GroupCtx / GroupOutcome）

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（`NativeConfig`, `Engine::build_with_resolver`）
- Modify: `crates/mat-native/src/group.rs`（`GroupCtx`, `GroupOutcome`, `init_sender`, `send`）
- Modify: 追従コンパイル修正: `crates/mat/src/native_direct.rs`（`NativeConfig` 構築 3 箇所: 758/1609/1694 行付近 — `thread_iface: None` を足すだけ）、`crates/matd/src/native.rs`（922 行付近のテスト同様）、`crates/matd/src/main.rs`（160 行付近 — `thread_iface: None`）、`GroupOutcome::Sent` マッチ箇所（`crates/mat/src/native_direct.rs` / `crates/matd/src/server.rs` — `Sent { .. }` に直すだけ。egress の JSON 反映は Task 4）

**Interfaces:**
- Produces（mat-native）:

```rust
/// Thread iface の決定結果。明示（解決失敗=ハードエラー）と自動検出
/// （解決失敗=warn+劣化続行）で失敗時の規律が違う（spec 設計 3）。
#[derive(Debug, Clone)]
pub enum ThreadIfaceChoice {
    Explicit(String),
    Auto(String),
}

pub struct NativeConfig {
    pub store: std::path::PathBuf,
    pub iface: String,
    pub thread_iface: Option<ThreadIfaceChoice>,   // 追加
    pub fabric_index: u8,
    pub issuer_index: u8,
}
```

- Produces（mat-native::group）:

```rust
pub struct GroupCtx {
    pub main_ini: PathBuf,
    pub counter_path: PathBuf,
    pub fabric_index: u8,
    pub fabric_id: u64,
    pub node_id: u64,
    /// 送出先リスト。先頭 = 運用 iface（従来挙動）。
    pub egress: Vec<mat_controller::group::GroupEgress>,
    pub dest_port: u16,
    pub sender: Mutex<Option<GroupSender>>,
}
pub enum GroupOutcome {
    Sent { egress: Vec<String> },
    Unavailable(String),
}
```

（旧 `scope_id: u32` / `transport: Arc<UdpTransport>` フィールドは `egress` に統合）

- [ ] **Step 1: 失敗するテストを書く（Engine::build の egress 構成分岐）**

`crates/mat-native/src/lib.rs` のテスト（`Engine::build` を実 KVS 抜きで試すのは
重いので、分岐ロジックを純関数に切り出してテストする）。`lib.rs` に:

```rust
/// thread iface 選択を egress 追加判断に写像する純関数（テスト対象）。
/// 戻り: Ok(Some(name)) = 第 2 egress を張る, Ok(None) = LAN 単独,
/// Err(detail) = ハードエラー（明示指定の解決失敗のみ）。
fn thread_egress_decision(
    choice: &Option<ThreadIfaceChoice>,
    resolve: impl Fn(&str) -> Result<u32, String>,
) -> Result<Option<(String, u32)>, String> { ... }
```

テスト:

```rust
#[test]
fn thread_egress_explicit_failure_is_hard_error() {
    let r = thread_egress_decision(
        &Some(ThreadIfaceChoice::Explicit("wpan9".into())),
        |_| Err("no such iface".into()),
    );
    assert!(r.is_err());
}

#[test]
fn thread_egress_auto_failure_degrades_to_lan_only() {
    let r = thread_egress_decision(
        &Some(ThreadIfaceChoice::Auto("wpan0".into())),
        |_| Err("no such iface".into()),
    );
    assert_eq!(r.unwrap(), None);
}

#[test]
fn thread_egress_resolved_returns_scope() {
    let r = thread_egress_decision(
        &Some(ThreadIfaceChoice::Auto("wpan0".into())),
        |_| Ok(7),
    );
    assert_eq!(r.unwrap(), Some(("wpan0".into(), 7)));
}

#[test]
fn thread_egress_none_is_lan_only() {
    let r = thread_egress_decision(&None, |_| unreachable!());
    assert_eq!(r.unwrap(), None);
}
```

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat-native thread_egress`
Expected: FAIL（未定義）

- [ ] **Step 3: 実装**

`thread_egress_decision`:

```rust
fn thread_egress_decision(
    choice: &Option<ThreadIfaceChoice>,
    resolve: impl Fn(&str) -> Result<u32, String>,
) -> Result<Option<(String, u32)>, String> {
    match choice {
        None => Ok(None),
        Some(ThreadIfaceChoice::Explicit(name)) => match resolve(name) {
            Ok(idx) => Ok(Some((name.clone(), idx))),
            Err(e) => Err(format!(
                "native: resolve thread iface {name:?} index: {e} (explicit MAT_THREAD_IFACE must resolve)"
            )),
        },
        Some(ThreadIfaceChoice::Auto(name)) => match resolve(name) {
            Ok(idx) => Ok(Some((name.clone(), idx))),
            Err(e) => {
                tracing::warn!(iface = %name, error = %e,
                    "thread iface auto-detected but unresolvable; groupcast stays LAN-only");
                Ok(None)
            }
        },
    }
}
```

`Engine::build_with_resolver` の GroupCtx 構築部を差し替え:

```rust
let mut egress = vec![mat_controller::group::GroupEgress {
    iface: cfg.iface.clone(),
    transport: Arc::clone(&transport),
    scope_id,
}];
match thread_egress_decision(&cfg.thread_iface, |n| {
    mat_controller::dnssd::iface_index(n).map_err(|e| e.to_string())
}) {
    Ok(Some((name, tsid))) => {
        // Thread egress は専用 socket（LAN 側の IPV6_MULTICAST_IF と独立）
        match UdpTransport::bind().await {
            Ok(t) => {
                tracing::info!(iface = %name, "groupcast thread egress enabled");
                egress.push(mat_controller::group::GroupEgress {
                    iface: name,
                    transport: Arc::new(t),
                    scope_id: tsid,
                });
            }
            Err(e) => match &cfg.thread_iface {
                Some(ThreadIfaceChoice::Explicit(_)) => {
                    return Err(MatError::new(ErrorKind::Other,
                        format!("native: bind thread egress socket: {e}")));
                }
                _ => tracing::warn!(error = %e,
                    "thread egress socket bind failed; groupcast stays LAN-only"),
            },
        }
    }
    Ok(None) => {}
    Err(detail) => return Err(MatError::new(ErrorKind::Other, detail)),
}
let group = group::GroupCtx {
    main_ini,
    counter_path: cfg.store.join("native_group_counter"),
    fabric_index: cfg.fabric_index,
    fabric_id,
    node_id,
    egress,
    dest_port: MATTER_PORT,
    sender: tokio::sync::Mutex::new(None),
};
```

`mat-native/src/group.rs`:
- `GroupCtx` フィールドを `egress: Vec<GroupEgress>` に変更
- `init_sender`: `GroupSender::new(ctx.egress.clone(), ctx.dest_port, ctx.fabric_id, ctx.node_id, counter)`
- `send()` の成功パス:

```rust
Ok((counter, sent)) => {
    tracing::info!(group_id, counter, egress = %sent.join("+"), "groupcast sent (native)");
    Ok(GroupOutcome::Sent { egress: sent })
}
```

- 既存テスト追従: `group_invoke_sends_multicast_and_reports_sent` は
  `egress: vec![GroupEgress { iface: cand.name.clone(), transport, scope_id: cand.index }]`
  で単一 egress を構築し、`matches!(r, GroupOutcome::Sent { .. })` に変更。
  bump テスト等の `GroupCtx` 構築も同様に追従。
- workspace 追従: `crates/mat/src/native_direct.rs` と `crates/matd/src/main.rs` /
  `crates/matd/src/native.rs` の `NativeConfig` 構築に `thread_iface: None` を追加、
  `GroupOutcome::Sent` のマッチを `Sent { .. }` へ（JSON 反映は Task 4）。
  mat 側 `native_direct::Config` は Task 5 で拡張するのでここでは `None` 固定。

- [ ] **Step 4: workspace 全体が緑なことを確認**

Run: `task check`
Expected: PASS（fmt / clippy / 全 crate テスト）

- [ ] **Step 5: Commit（Task 2 の分も含む）**

```bash
git add crates/mat-controller/src/group.rs crates/mat-native/src/group.rs \
        crates/mat-native/src/lib.rs crates/mat/src/native_direct.rs \
        crates/matd/src/main.rs crates/matd/src/native.rs
git commit -m "feat(mat-controller,mat-native): groupcast を egress リスト方式にし Thread iface 二重送出に対応"
```

---

### Task 4: JSON body に `egress` フィールド（mat-core + 両呼び出し側）

**Files:**
- Modify: `crates/mat-core/src/body.rs`（`group_invoke_sent` / `group_color_temp_sent` / `group_level_sent` / `group_color_sent` + 形状テスト）
- Modify: `crates/mat/src/commands/group.rs`（body 呼び出しに egress を渡す）
- Modify: `crates/matd/src/server.rs`（同上、830/848 行付近ほか sent body 組み立て箇所）

**Interfaces:**
- Produces: 4 builder すべてに末尾引数 `egress: &[String]` を追加し、JSON に
  `"egress": ["eth0", "wpan0"]` を出す。egress が LAN 単独でも常に出す
  （後方互換な追加フィールド）。

- [ ] **Step 1: 失敗するテストを書く**

`body.rs` の既存形状テストを更新（例: `group_invoke_sent_shape`）:

```rust
#[test]
fn group_invoke_sent_shape() {
    assert_eq!(
        group_invoke_sent(10, "onoff", "on", 1, &["eth0".into(), "wpan0".into()]),
        json!({
            "group_id": 10, "cluster": "onoff", "command": "on",
            "endpoint": 1, "status": "sent",
            "egress": ["eth0", "wpan0"],
            "note": "unacknowledged groupcast; per-device delivery not confirmed",
        })
    );
}
```

`group_color_temp_sent` / `group_level_sent` / `group_color_sent` の形状テストも
同様に `egress` を追加。

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat-core body::`
Expected: FAIL（引数数不一致のコンパイルエラー）

- [ ] **Step 3: 実装**

各 builder に `egress: &[String]` を追加し、`json!` へ `"egress": egress` を挿入。
呼び出し側:
- `crates/mat/src/commands/group.rs`: `GroupOutcome::Sent { egress }` を受けて
  `body::group_invoke_sent(..., &egress)` 等に渡す。
- `crates/matd/src/server.rs`: 同様（`Sent { egress }` を分解して渡す）。

- [ ] **Step 4: 緑を確認**

Run: `task check`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/mat-core/src/body.rs crates/mat/src/commands/group.rs crates/matd/src/server.rs
git commit -m "feat(mat-core): group 送信系 JSON に egress フィールドを追加"
```

---

### Task 5: mat CLI 配線（--thread-iface / MAT_THREAD_IFACE + 自動検出）

**Files:**
- Modify: `crates/mat/src/cli.rs`（グローバル引数追加 — `--iface` の隣）
- Modify: `crates/mat/src/main.rs`（iface 決定ブロックの直後で thread iface を決定）
- Modify: `crates/mat/src/native_direct.rs`（`Config` に `thread_iface` を追加し `NativeConfig` へ伝播）

**Interfaces:**
- Consumes: `mat_native::iface_select::detect_thread_iface_auto()`（Task 1）、
  `mat_native::ThreadIfaceChoice`（Task 3）
- Produces: `native_direct::Config` に
  `pub thread_iface: Option<mat_native::ThreadIfaceChoice>` を追加。
  `NativeConfig` 構築 3 箇所（758/1609/1694 行付近）で `cfg.thread_iface.clone()` を渡す。

- [ ] **Step 1: cli.rs に引数を追加**

`--iface` の定義に倣い（doc コメントも同じ調子で）:

```rust
/// groupcast を追加送出する Thread TUN の iface 名（例: wpan0）。未設定なら
/// `wpan*` がちょうど 1 本あるとき自動採用。明示指定の解決失敗はハード
/// エラー、自動検出の失敗は warn + LAN 単独送出（spec 設計 3）。
#[arg(long, global = true, env = "MAT_THREAD_IFACE")]
pub thread_iface: Option<String>,
```

（`--iface` が `global = true` でなければそれに合わせる — 既存定義を見て同じ形に。）

- [ ] **Step 2: main.rs で決定して native_direct::Config へ**

iface 決定ブロック（130 行付近）の直後:

```rust
let thread_iface: Option<mat_native::ThreadIfaceChoice> = match &args.thread_iface {
    Some(n) => Some(mat_native::ThreadIfaceChoice::Explicit(n.clone())),
    None => mat_native::iface_select::detect_thread_iface_auto().map(|n| {
        tracing::info!(iface = %n, "thread iface auto-detected (groupcast egress)");
        mat_native::ThreadIfaceChoice::Auto(n)
    }),
};
let native_cfg = Some(native_direct::Config {
    iface: &iface_owned,
    thread_iface,
    fabric_index: args.fabric_index,
    issuer_index: args.issuer_index,
});
```

`native_direct.rs` の `Config` に `pub thread_iface: Option<mat_native::ThreadIfaceChoice>`
を追加し、`NativeConfig` 構築 3 箇所の `thread_iface: None`（Task 3 で入れた仮値）を
`cfg.thread_iface.clone()` に置換。テスト内 Config 構築（2314 行付近 `iface: "lo"`）には
`thread_iface: None` を追加。

- [ ] **Step 3: 緑を確認**

Run: `task check`
Expected: PASS

- [ ] **Step 4: 手動スモーク（挙動確認）**

Run: `cargo run -p mat -- group invoke --help 2>&1 | grep -A2 thread-iface`
Expected: ヘルプに `--thread-iface` と `MAT_THREAD_IFACE` が出る

- [ ] **Step 5: Commit**

```bash
git add crates/mat/src/cli.rs crates/mat/src/main.rs crates/mat/src/native_direct.rs
git commit -m "feat(mat): --thread-iface / MAT_THREAD_IFACE と wpan* 自動検出を配線"
```

---

### Task 6: matd 配線（--thread-iface / MAT_MATD_THREAD_IFACE + 起動ログ）

**Files:**
- Modify: `crates/matd/src/main.rs`（CLI 引数 41-44 行付近、iface 決定 139-160 行付近、起動ログ 188 行付近）

**Interfaces:**
- Consumes: Task 1 / Task 3 と同じ。
- Produces: matd 起動ログ `native backend enabled` に `thread_iface` フィールド追加。

- [ ] **Step 1: CLI 引数追加**

`iface` フィールドの下に:

```rust
/// groupcast を追加送出する Thread TUN の iface 名（例: wpan0)。未設定なら
/// `wpan*` がちょうど 1 本あるとき自動採用。明示指定の解決失敗は起動拒否。
#[arg(long, env = "MAT_MATD_THREAD_IFACE")]
thread_iface: Option<String>,
```

- [ ] **Step 2: 決定ロジックと NativeConfig / 起動ログ**

iface 決定（143-158 行付近）の直後:

```rust
let thread_iface = match &cli.thread_iface {
    Some(n) => Some(mat_native::ThreadIfaceChoice::Explicit(n.clone())),
    None => mat_native::iface_select::detect_thread_iface_auto().map(|n| {
        tracing::info!(iface = %n, "thread iface auto-detected (matd groupcast egress)");
        mat_native::ThreadIfaceChoice::Auto(n)
    }),
};
```

`NativeConfig` 構築（160 行付近、Task 3 の `thread_iface: None` 仮値）を
`thread_iface: thread_iface.clone()` に置換。
起動ログ（188 行付近）を:

```rust
tracing::info!(%iface, thread_iface = ?thread_iface, fabric_index = cli.fabric_index, "native backend enabled");
```

明示指定の解決失敗は `Engine::build` がハードエラーを返す（Task 3）ので、matd の
既存の build 失敗ハンドリング（起動拒否）がそのまま効く — 追加コード不要なことを
確認する（build エラーで matd が exit するパスを目視確認）。

- [ ] **Step 3: 緑を確認**

Run: `task check`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/matd/src/main.rs
git commit -m "feat(matd): --thread-iface / MAT_MATD_THREAD_IFACE と wpan* 自動検出を配線"
```

---

### Task 7: docs + version 1.26.0

**Files:**
- Modify: `docs/commands.md`（"Groupcast counter" 節の近くに "Groupcast egress" 節を追加）
- Modify: `README.md`（Backend 節に iface の記述があれば 1-2 文追記。無ければ触らない）
- Modify: `Cargo.toml` / `Cargo.lock`（workspace version 1.26.0 — 直近の `chore: 1.25.0` コミット（`git show b7d4ee2 --stat`）と同じファイル群を同じ流儀で上げる）

- [ ] **Step 1: docs/commands.md に節を追加**

"Groupcast counter (shared between `mat` and `matd`)" 節の直後に:

```markdown
#### Groupcast egress (LAN + Thread TUN)

Groupcast is sent on the operational interface (the same one mDNS uses), and
— when a Thread TUN is available — **also directly on that interface** (MPL
injection, no dependency on another border router's multicast relay). The
Thread interface is picked once at startup: `--thread-iface` /
`MAT_THREAD_IFACE` (`MAT_MATD_THREAD_IFACE` for `matd`) wins; otherwise, if
exactly one `wpan*` interface is up it is auto-selected; otherwise groupcast
stays LAN-only (previous behavior). An explicitly configured interface that
fails to resolve is a hard error (`matd` refuses to start); an auto-detected
one that fails degrades to LAN-only with a warning. The same datagram (same
counter) goes out on every egress — receivers drop the duplicate via replay
protection. The result JSON reports the interfaces actually used:
`"egress": ["eth0", "wpan0"]`.
```

- [ ] **Step 2: version bump**

`git show b7d4ee2 --stat` で対象ファイルを確認し、同じ流儀で 1.26.0 に上げる。

- [ ] **Step 3: 緑を確認して Commit**

Run: `task check`
Expected: PASS

```bash
git add docs/commands.md README.md Cargo.toml Cargo.lock
git commit -m "chore: 1.26.0（groupcast Thread iface 直送 — egress 二重送出）"
```

---

### Task 8: jarvis 実機 E2E（マージ前必須）

**Files:** なし（運用検証タスク）。手順は memory の despliegue skill / [[jarvis-matd-deploy]] /
[[e2e-before-merge]] の隔離 matd 方式に従う。

- [ ] **Step 1: クロスビルドして jarvis へ転送**

`task dist:arm64`（aarch64-musl。stale 成果物は `file` で確認）→ `scp` で
jarvis の `~/.local/bin/mat.new` / `matd.new` へ（既存 `*.new` 運用と同じ。
本番バイナリは置換しない）。

- [ ] **Step 2: 隔離 matd スモーク（自動検出の確認）**

jarvis 上で隔離 matd（別 socket / 同一 store は counter flock 競合するため、
**本番 matd を止めて**短時間で検証する。手順は [[jarvis-matd-deploy]] の隔離方式）:

```bash
ssh jarvis 'systemctl --user stop matd && \
  MAT_MATD_FABRIC_INDEX=2 ~/.local/bin/matd.new --store ~/.config/mat \
    --socket /tmp/matd-e2e.sock & sleep 5; \
  journalctl --user 2>/dev/null; true'
```

確認点: stderr ログに `thread iface auto-detected (matd groupcast egress) iface=wpan0`
と `native backend enabled ... thread_iface=Some(Auto("wpan0"))` が出ること。

- [ ] **Step 3: 配達 E2E（off/on × 2、read で反転確認）**

```bash
ssh jarvis 'MAT_MATD_SOCKET=/tmp/matd-e2e.sock MAT_MATD=1 \
  ~/.local/bin/mat.new group invoke --group desk_room_lights --cluster onoff --command off'
# 6s 待って read（node 6 / 17 とも false になること）
# 続けて on → read（true）。off/on を2周
```

出力 JSON に `"egress": ["eth0", "wpan0"]` が入ること・**node 17 が毎回反転する**
こと（旧経路では 0/10 だった）を確認。終了後: 隔離 matd を kill →
`systemctl --user start matd` → `mat read` で本番経路の生存確認。照明は on に戻す。

- [ ] **Step 4: 結果を記録**

E2E 結果（成功回数・egress 表示・ログ）をセッションに記録し、
memory `groupcast-e2e-findings` / `jarvis-matd-deploy` を更新する
（`*.new` = 1.26.0 転送済み、恒久デプロイは次回デプロイ同乗）。

---

## Self-Review 済み事項

- spec の全要求に対応タスクあり: 決定順序=Task 1/5/6、egress 送出=Task 2/3、
  失敗規律=Task 3（decision 純関数）、可観測性=Task 3(ログ)/4(JSON)/6(起動ログ)、
  docs=Task 7、E2E=Task 8。
- wpan0 の operstate=unknown 罠を Global Constraints に明記（実機 `ip link` で確認済み）。
- 型整合: `ThreadIfaceChoice`（mat-native）を mat / matd 両方が使う。
  `GroupEgress`（mat-controller）を mat-native が re-use。
  `GroupOutcome::Sent { egress: Vec<String> }` → body builder `&[String]`。
