# matd 常駐 mDNS キャッシュ Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matd に operational レコードの常駐 mDNS キャッシュを持たせ、周期
アナウンス依存の弱リンク Thread ノードでも warm session の establish/再確立を
確実に成功させる（native resolve 回帰の恒久修正・層2）。

**Architecture:** `mat-native` に `Resolver` トレイトを導入し `CaseEstablisher`
に注入する。`mat`（一発）は `OneShotResolver`（キャッシュ無し=設計ルール4維持）、
`matd` は `CachingResolver` + `mat-controller::dnssd` の常駐 `OperationalCache`
リスナ（`ff02::fb` を join した単一 5353 socket で周期アナウンスを畳み込み保持）。
establish はキャッシュ参照→ヒット即返し／ミス時はリスナの次アナウンスを最大 35s
待つ。

**Tech Stack:** Rust, tokio, socket2, async-trait（既存 `Establisher` で使用中）。

## Global Constraints

- 設計ルール1: protocol（TLV/CASE/mDNS/multicast）は backend crate に閉じる。
  mDNS 受信・畳み込み・ソケットは `mat-controller`。matd/server は配線のみ。
- 設計ルール2/3: stdout は純 JSON、診断は stderr（`tracing`）。本作業は無影響。
- 設計ルール4: `mat`（一発）は KVS 以外の state・キャッシュ・常駐リスナを持たない。
  キャッシュは **matd 専用**。`mat` 一発直経路のロジックは無変更。
- 出力スキーマ・matd socket プロトコル・エラー `kind` は不変。
- `task check`（`cargo fmt --check` + `cargo clippy -D warnings` + `cargo test`）が
  全タスクで green。
- コミット対象はそのタスクで自分が編集したファイルのみ。
- コミットメッセージ末尾に:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VA7F1Xguc3LeDhxefPHh3W
  ```
- 前提: 層1修正（`dnssd::bind_mdns_socket` / `QU_CLASS_IN` / 5353 bind + `ff02::fb`
  join + QU + hop255）は本ブランチ `fix/native-mdns-multicast-resolve` に
  コミット済み（commit 86ebc61）。本計画はその上に積む。

---

### Task 1: `Resolver` トレイト + `OneShotResolver` + `CaseEstablisher` 配線

establish の mDNS 解決を差し替え可能にする抽象を導入する。既存挙動は不変
（`Engine::build` は `OneShotResolver` を使い、従来どおり `resolve_operational` を
呼ぶ）。

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（`Resolver` 追加、`CaseEstablisher` に
  `resolver` フィールド追加、`build` の establisher 構築、`establish` の解決呼び）

**Interfaces:**
- Consumes: `mat_controller::dnssd::{resolve_operational, ResolvedNode, DnssdError}`（既存）。
- Produces:
  - `pub trait Resolver: Send + Sync`（`#[async_trait]`）:
    `async fn resolve(&self, scope_id: u32, cfid: [u8; 8], node_id: u64, timeout: Duration) -> Result<dnssd::ResolvedNode, dnssd::DnssdError>`
  - `pub struct OneShotResolver;`（`Resolver` 実装、`resolve_operational` へ委譲）
  - `CaseEstablisher` に `resolver: Arc<dyn Resolver>` フィールド。
  - `Engine::build(cfg)` シグネチャ不変（内部で `Arc::new(OneShotResolver)` を使用）。

- [ ] **Step 1: `resolve_operational` を直呼びしている establish のテスト前提を確認**

Run: `cargo test -p mat-native --lib 2>&1 | tail -5`
Expected: PASS（既存テストが通ることを基準線として確認）。

- [ ] **Step 2: `Resolver` トレイトと `OneShotResolver` を追加**

`crates/mat-native/src/lib.rs` の `RESOLVE_TIMEOUT` 定義付近（`impl Engine` の直前）に追加:

```rust
/// establish の mDNS 解決を差し替え可能にする抽象。`mat`（一発）は
/// [`OneShotResolver`]（キャッシュ無し＝設計ルール4）、`matd` は
/// `CachingResolver`（常駐キャッシュ、Task 5）を注入する。
#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(
        &self,
        scope_id: u32,
        cfid: [u8; 8],
        node_id: u64,
        timeout: Duration,
    ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError>;
}

/// 既定のリゾルバ: 一発 legacy multicast resolve を毎回実行する（キャッシュ
/// を持たない）。`mat` 一発直経路が使う。
pub struct OneShotResolver;

#[async_trait]
impl Resolver for OneShotResolver {
    async fn resolve(
        &self,
        scope_id: u32,
        cfid: [u8; 8],
        node_id: u64,
        timeout: Duration,
    ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError> {
        dnssd::resolve_operational(scope_id, &cfid, node_id, timeout).await
    }
}
```

（`dnssd` は既に `use mat_controller::{... dnssd ...}` で参照可能。未 import なら
`use mat_controller::dnssd;` を追加。`Duration` は `std::time::Duration`、既存 import を流用。）

- [ ] **Step 3: `CaseEstablisher` に `resolver` を持たせ、`establish` で使う**

`struct CaseEstablisher` にフィールド追加:

```rust
struct CaseEstablisher {
    creds: Arc<FabricCredentials>,
    transport: Arc<Transport>,
    scope_id: u32,
    resolver: Arc<dyn Resolver>,
}
```

`establish` 内の解決呼び出しを差し替え（従来 `dnssd::resolve_operational(self.scope_id, &cfid, node_id, RESOLVE_TIMEOUT)`）:

```rust
        let cfid = compressed_fabric_id(&self.creds.root_public_key, self.creds.fabric_id);
        let resolved = self
            .resolver
            .resolve(self.scope_id, cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| {
                MatError::new(
                    ErrorKind::Unreachable,
                    format!("native: mDNS resolve node {node_id}: {e}"),
                )
            })?;
```

- [ ] **Step 4: `Engine::build` の establisher 構築に `OneShotResolver` を渡す**

`build` 内の `CaseEstablisher { ... }` 構築を private ヘルパー経由にする。`build`
本体の establisher 構築部を以下へ置換:

```rust
        let establisher = CaseEstablisher {
            creds: Arc::new(creds),
            transport: Arc::new(Transport::Udp(Arc::clone(&transport))),
            scope_id,
            resolver: Arc::new(OneShotResolver),
        };
```

- [ ] **Step 5: ビルド & 既存テスト**

Run: `cargo test -p mat-native --lib 2>&1 | tail -5`
Expected: PASS（挙動不変。`resolver` は OneShot でこれまでと同じ経路）。

- [ ] **Step 6: `OneShotResolver` の委譲を固定するテストを追加**

`crates/mat-native/src/lib.rs` の `#[cfg(test)] mod tests`（無ければ末尾に新設）に:

```rust
#[tokio::test]
async fn oneshot_resolver_times_out_on_loopback_without_responder() {
    // 応答者のいない lo で resolve すると Timeout（委譲先 resolve_operational の
    // 契約）。scope_id=1(lo 相当) は環境依存だが、無応答→Timeout は不変。
    let scope = mat_controller::dnssd::iface_index("lo").unwrap_or(1);
    let r = OneShotResolver;
    let out = r
        .resolve(scope, [0u8; 8], 5, std::time::Duration::from_millis(300))
        .await;
    assert!(matches!(
        out,
        Err(mat_controller::dnssd::DnssdError::Timeout { .. })
    ));
}
```

Run: `cargo test -p mat-native --lib oneshot_resolver 2>&1 | tail -8`
Expected: PASS。

- [ ] **Step 7: `task check` & commit**

Run: `task check 2>&1 | grep -E "test result:|error|warning:" | tail -5`
Expected: すべて ok、error/warning 無し。

```bash
git add crates/mat-native/src/lib.rs
git commit -m "$(cat <<'EOF'
refactor(native): establish の mDNS 解決を Resolver 抽象へ（挙動不変）

CaseEstablisher に Resolver を注入。既定は OneShotResolver（従来どおり
resolve_operational を毎回呼ぶ・キャッシュ無し）。matd 用 CachingResolver
（常駐キャッシュ）を後続タスクで注入するための土台。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VA7F1Xguc3LeDhxefPHh3W
EOF
)"
```

---

### Task 2: `OperationalCache`（get/insert/request/expiry/上限）

matd 常駐キャッシュのハンドル。listener（Task 4）と `CachingResolver`（Task 5）が
`Arc` で共有する。

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`（`OperationalCache` 追加 + ユニットテスト）

**Interfaces:**
- Consumes: `dnssd::ResolvedNode`（既存）、`tokio::time::Instant`、`std::time::Duration`。
- Produces:
  - `pub struct OperationalCache`（`#[derive(Clone)]`、内部 `Arc`）
  - `pub fn new() -> (OperationalCache, tokio::sync::mpsc::UnboundedReceiver<String>)`
  - `pub fn get(&self, instance: &str) -> Option<ResolvedNode>`（期限切れ/不在は None）
  - `pub fn request(&self, instance: String)`（listener に provoke クエリ依頼）
  - `pub fn insert(&self, instance: String, node: ResolvedNode, ttl: Duration)`
  - 定数 `MAX_CACHE: usize = 256`

- [ ] **Step 1: 失敗するテストを書く（insert/get/expiry/cap）**

`crates/mat-controller/src/dnssd.rs` の `#[cfg(test)] mod tests` に追加:

```rust
    fn sample_node(port: u16) -> ResolvedNode {
        ResolvedNode {
            port,
            addresses: vec!["fd00::1".parse().unwrap()],
            session_idle_interval_ms: Some(5000),
            session_active_interval_ms: Some(300),
        }
    }

    #[test]
    fn opcache_insert_get_and_expiry() {
        let (cache, _rx) = OperationalCache::new();
        let inst = "AABB-0005._matter._tcp.local".to_string();
        cache.insert(inst.clone(), sample_node(5540), Duration::from_secs(60));
        assert_eq!(cache.get(&inst).map(|n| n.port), Some(5540));
        assert!(cache.get("nope._matter._tcp.local").is_none());
        // 期限切れは None。
        cache.insert(inst.clone(), sample_node(5540), Duration::from_millis(0));
        assert!(cache.get(&inst).is_none());
    }

    #[test]
    fn opcache_caps_new_instances_but_updates_existing() {
        let (cache, _rx) = OperationalCache::new();
        for i in 0..MAX_CACHE {
            cache.insert(format!("i{i}._matter._tcp.local"), sample_node(1), Duration::from_secs(60));
        }
        // 上限到達後の新規は無視。
        cache.insert("overflow._matter._tcp.local".into(), sample_node(1), Duration::from_secs(60));
        assert!(cache.get("overflow._matter._tcp.local").is_none());
        // 既存キーの更新は上限後も許可。
        cache.insert("i0._matter._tcp.local".into(), sample_node(9999), Duration::from_secs(60));
        assert_eq!(cache.get("i0._matter._tcp.local").map(|n| n.port), Some(9999));
    }

    #[test]
    fn opcache_request_does_not_panic_and_is_received() {
        let (cache, mut rx) = OperationalCache::new();
        cache.request("x._matter._tcp.local".into());
        assert_eq!(rx.try_recv().unwrap(), "x._matter._tcp.local");
    }
```

- [ ] **Step 2: テストが失敗（未定義）することを確認**

Run: `cargo test -p mat-controller --lib dnssd::tests::opcache 2>&1 | tail -8`
Expected: FAIL / コンパイルエラー（`OperationalCache` 未定義）。

- [ ] **Step 3: `OperationalCache` を実装**

`crates/mat-controller/src/dnssd.rs` の末尾（`#[cfg(test)]` の直前）に追加。
ファイル冒頭に不足 import を足す（`use std::collections::HashMap;`、
`use std::sync::Mutex as StdMutex;` は衝突回避のため別名、`use tokio::sync::mpsc;`）:

```rust
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use tokio::sync::mpsc;

/// キャッシュ上限（偽装 flood でメモリを伸ばさない — MAX_INSTANCES と同思想）。
const MAX_CACHE: usize = 256;

struct CacheEntry {
    node: ResolvedNode,
    expiry: Instant,
}

struct CacheInner {
    map: StdMutex<HashMap<String, CacheEntry>>,
    query_tx: mpsc::UnboundedSender<String>,
}

/// matd 常駐 mDNS キャッシュのハンドル。listener タスク（[`run_operational_cache`]）
/// と `CachingResolver` が `Arc` で共有する。設計ルール4: `mat` 一発は使わない
/// （matd 専用）。`Clone` は内部 `Arc` の複製。
#[derive(Clone)]
pub struct OperationalCache {
    inner: std::sync::Arc<CacheInner>,
}

impl OperationalCache {
    /// ハンドルと、listener が読む provoke-request 受信端を返す。
    pub fn new() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                inner: std::sync::Arc::new(CacheInner {
                    map: StdMutex::new(HashMap::new()),
                    query_tx: tx,
                }),
            },
            rx,
        )
    }

    /// 鮮度のあるエントリのみ返す（期限切れ/不在は None）。
    pub fn get(&self, instance: &str) -> Option<ResolvedNode> {
        let map = self.inner.map.lock().expect("opcache mutex");
        map.get(instance)
            .filter(|e| Instant::now() < e.expiry)
            .map(|e| e.node.clone())
    }

    /// listener に instance の provoke クエリ送信を依頼する（listener 不在でも無害）。
    pub fn request(&self, instance: String) {
        let _ = self.inner.query_tx.send(instance);
    }

    /// listener が呼ぶ: エントリを入れて期限を更新する。上限超過時、新規キーは
    /// 挿入しない（既存キーの更新は常に許可＝鮮度維持を止めない）。
    pub fn insert(&self, instance: String, node: ResolvedNode, ttl: Duration) {
        let mut map = self.inner.map.lock().expect("opcache mutex");
        if !map.contains_key(&instance) && map.len() >= MAX_CACHE {
            return;
        }
        map.insert(
            instance,
            CacheEntry {
                node,
                expiry: Instant::now() + ttl,
            },
        );
    }
}
```

（`Instant` は既に `use tokio::time::Instant;` 済み、`Duration` は
`use std::time::Duration;` 済み。`tokio::time::Instant::now()` は tokio ランタイム外の
同期テストでも動作する。）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller --lib dnssd::tests::opcache 2>&1 | tail -8`
Expected: PASS（3 テスト）。

- [ ] **Step 5: `task check` & commit**

Run: `task check 2>&1 | grep -E "test result:|error|warning:" | tail -5`
Expected: ok。

```bash
git add crates/mat-controller/src/dnssd.rs
git commit -m "$(cat <<'EOF'
feat(dnssd): OperationalCache（matd 常駐 mDNS キャッシュのハンドル）

instance→(ResolvedNode, expiry) の共有マップ。get は期限切れ/不在で None、
insert は TTL 尊重・上限 MAX_CACHE で新規を制限（既存更新は許可）、request は
listener への provoke クエリ依頼。listener/CachingResolver が Arc 共有する。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VA7F1Xguc3LeDhxefPHh3W
EOF
)"
```

---

### Task 3: operational レコードの畳み込み（`fold_operational_into_cache`）

受信した DNS レコード群から operational instance の SRV/TXT/AAAA を畳み込み、
完成した instance を `OperationalCache` に insert する純ロジック。socket I/O は
含めず、`parse_message` の出力を食う形でユニットテストする。

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`（`OperationalFold` + `fold_operational_into_cache` + テスト）

**Interfaces:**
- Consumes: `Record`, `RData`, `ResolvedNode`, `is_link_local`, `OperationalCache`（既存/Task 2）。
- Produces:
  - `struct OperationalFold`（`Default`、private）: instance ごとの srv/txt/ttl と AAAA プール。
  - `fn fold_operational_into_cache(records: &[Record], fold: &mut OperationalFold, cache: &OperationalCache)`（`pub(crate)` 相当、同モジュール内 private でよい）

- [ ] **Step 1: 失敗するテストを書く**

`#[cfg(test)] mod tests` に追加（既存 `synth_response` を再利用）:

```rust
    #[test]
    fn fold_operational_populates_cache_from_one_message() {
        let (cache, _rx) = OperationalCache::new();
        let addr: Ipv6Addr = "fd54:4b81:8cce:1::b92a".parse().unwrap();
        // operational instance の完成応答（SRV+TXT+AAAA を 1 メッセージに）。
        let msg = synth_response(
            "AB7DE08802E0CD54-0000000000000005._matter._tcp.local",
            "12B41A22758B788A.local",
            5540,
            &["SII=5000", "SAI=300", "T=0"],
            addr,
        );
        let records = parse_message(&msg).unwrap();
        let mut fold = OperationalFold::default();
        fold_operational_into_cache(&records, &mut fold, &cache);

        let node = cache
            .get("AB7DE08802E0CD54-0000000000000005._matter._tcp.local")
            .expect("operational instance should be cached");
        assert_eq!(node.port, 5540);
        assert_eq!(node.addresses, vec![addr]);
        assert_eq!(node.session_idle_interval_ms, Some(5000));
    }

    #[test]
    fn fold_operational_ignores_non_matter_and_incomplete() {
        let (cache, _rx) = OperationalCache::new();
        // commissionable(_matterc._udp) は operational ではないので無視。
        let msg = synth_commissionable_response(
            "_L3840._sub._matterc._udp.local",
            "ABCD1234._matterc._udp.local",
            "dev.local",
            5540,
            &["D=3840"],
            "fd00::1".parse().unwrap(),
        );
        let records = parse_message(&msg).unwrap();
        let mut fold = OperationalFold::default();
        fold_operational_into_cache(&records, &mut fold, &cache);
        assert!(cache.get("ABCD1234._matterc._udp.local").is_none());
    }
```

- [ ] **Step 2: テスト失敗を確認**

Run: `cargo test -p mat-controller --lib dnssd::tests::fold_operational 2>&1 | tail -8`
Expected: FAIL（`OperationalFold` / `fold_operational_into_cache` 未定義）。

- [ ] **Step 3: `OperationalFold` と畳み込み関数を実装**

`OperationalCache` 定義の直後に追加:

```rust
/// operational instance を _matter._tcp のサービス名で判定する接尾辞。
const OPERATIONAL_SUFFIX: &str = "._matter._tcp.local";

struct InstAcc {
    srv: Option<(u16, String)>,
    txt: Option<Vec<Vec<u8>>>,
    /// SRV レコードの TTL（秒）。エントリ expiry の基準。
    srv_ttl: u32,
}

/// 複数データグラムにまたがる operational レコードの畳み込み状態。listener が
/// データグラムごとに [`fold_operational_into_cache`] へ食わせる。
#[derive(Default)]
struct OperationalFold {
    /// instance 完全名 → 蓄積。
    instances: HashMap<String, InstAcc>,
    /// hostname(SRV target) → AAAA プール。instance 横断で共有し完成時に引く。
    aaaa: Vec<(String, Ipv6Addr)>,
}

/// `records` を畳み込み、SRV + 一致 AAAA が揃った operational instance を
/// `cache` に insert する（TTL は SRV レコード値を尊重）。commissionable
/// (`_matterc._udp`) など operational でない名前は無視する。
fn fold_operational_into_cache(
    records: &[Record],
    fold: &mut OperationalFold,
    cache: &OperationalCache,
) {
    for r in records {
        match &r.rdata {
            RData::Srv { port, target } if r.name.ends_with(OPERATIONAL_SUFFIX) => {
                let acc = fold.instances.entry(r.name.clone()).or_insert(InstAcc {
                    srv: None,
                    txt: None,
                    srv_ttl: 0,
                });
                acc.srv = Some((*port, target.clone()));
                acc.srv_ttl = r.ttl;
            }
            RData::Txt(strings) if r.name.ends_with(OPERATIONAL_SUFFIX) => {
                let acc = fold.instances.entry(r.name.clone()).or_insert(InstAcc {
                    srv: None,
                    txt: None,
                    srv_ttl: 0,
                });
                acc.txt = Some(strings.clone());
            }
            RData::Aaaa(addr) => {
                if !fold
                    .aaaa
                    .iter()
                    .any(|(n, a)| a == addr && n.eq_ignore_ascii_case(&r.name))
                {
                    if fold.aaaa.len() < MAX_BROWSE_AAAA {
                        fold.aaaa.push((r.name.clone(), *addr));
                    }
                }
            }
            _ => {}
        }
    }
    // 完成した instance を cache へ。
    for (instance, acc) in &fold.instances {
        let Some((port, target)) = &acc.srv else {
            continue;
        };
        let mut addresses: Vec<Ipv6Addr> = Vec::new();
        for (n, a) in &fold.aaaa {
            if n.eq_ignore_ascii_case(target) && !addresses.contains(a) {
                addresses.push(*a);
            }
        }
        if addresses.is_empty() {
            continue;
        }
        addresses.sort_by_key(is_link_local);
        let strings: &[Vec<u8>] = acc.txt.as_deref().unwrap_or(&[]);
        let node = ResolvedNode {
            port: *port,
            addresses,
            session_idle_interval_ms: txt_u32(strings, "SII"),
            session_active_interval_ms: txt_u32(strings, "SAI"),
        };
        // TTL 0（goodbye）は即時失効相当なので短く。通常は広告 TTL を尊重。
        let ttl = Duration::from_secs(u64::from(acc.srv_ttl));
        cache.insert(instance.clone(), node, ttl);
    }
}
```

（`MAX_BROWSE_AAAA` / `txt_u32` / `is_link_local` は既存 private を流用。)

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller --lib dnssd::tests::fold_operational 2>&1 | tail -8`
Expected: PASS（2 テスト）。

- [ ] **Step 5: `task check` & commit**

Run: `task check 2>&1 | grep -E "test result:|error|warning:" | tail -5`
Expected: ok。

```bash
git add crates/mat-controller/src/dnssd.rs
git commit -m "$(cat <<'EOF'
feat(dnssd): operational レコードの畳み込み→OperationalCache 充填

fold_operational_into_cache: 受信レコードから _matter._tcp instance の
SRV/TXT/AAAA を畳み込み、SRV+一致AAAA が揃ったら TTL 尊重で cache に insert。
commissionable 等 operational でない名前は無視。socket I/O 非依存で純ロジック。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VA7F1Xguc3LeDhxefPHh3W
EOF
)"
```

---

### Task 4: 常駐リスナ `run_operational_cache` + `spawn_operational_cache`

単一 mDNS socket を常駐させ、受信を畳み込みキャッシュを温める。provoke
リクエストが来たらそのクエリを送出する。matd が起動時に spawn し、bind 失敗は
`Err`（matd が OneShotResolver に degrade するため）。

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`（`run_operational_cache` + `spawn_operational_cache`）

**Interfaces:**
- Consumes: `bind_mdns_socket`, `parse_message`, `OperationalCache`, `OperationalFold`,
  `fold_operational_into_cache`, `encode_query`, `MDNS_GROUP`, `MDNS_PORT`, `TYPE_SRV`, `TYPE_TXT`（既存/Task 2-3）。
- Produces:
  - `async fn run_operational_cache(sock: UdpSocket, cache: OperationalCache, requests: mpsc::UnboundedReceiver<String>, scope_id: u32)`
  - `pub fn spawn_operational_cache(scope_id: u32) -> std::io::Result<OperationalCache>`

- [ ] **Step 1: `run_operational_cache` と `spawn_operational_cache` を実装**

Task 3 の関数群の直後に追加:

```rust
/// operational レコードを常駐で受信・畳み込みキャッシュを温める。matd が起動時に
/// spawn する。provoke リクエスト受信時はその instance の SRV+TXT クエリを送出。
/// I/O エラー・パース失敗では落とさず継続する（listener はプロセス寿命）。
async fn run_operational_cache(
    sock: UdpSocket,
    cache: OperationalCache,
    mut requests: mpsc::UnboundedReceiver<String>,
    scope_id: u32,
) {
    let dest = SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, scope_id));
    let mut fold = OperationalFold::default();
    // browse と同様、複数 instance の additional 同梱に備え広めに取る。
    let mut buf = vec![0u8; 9000];
    loop {
        tokio::select! {
            recv = sock.recv_from(&mut buf) => {
                let Ok((n, _)) = recv else { continue; };
                let Ok(records) = parse_message(&buf[..n]) else { continue; };
                fold_operational_into_cache(&records, &mut fold, &cache);
            }
            req = requests.recv() => {
                match req {
                    // instance は "<CFID>-<NodeId>._matter._tcp.local"。
                    Some(instance) => {
                        let q = encode_query(0, &[(instance.as_str(), TYPE_SRV), (instance.as_str(), TYPE_TXT)]);
                        let _ = sock.send_to(&q, dest).await;
                    }
                    // 全 sender が drop（= 実質プロセス終了時のみ）。
                    None => return,
                }
            }
        }
    }
}

/// matd 用: mDNS socket を bind し常駐 cache タスクを spawn する。bind 失敗は
/// `Err`（matd は OneShotResolver に degrade する）。tokio ランタイム内で呼ぶこと。
pub fn spawn_operational_cache(scope_id: u32) -> std::io::Result<OperationalCache> {
    let sock = bind_mdns_socket(scope_id)?;
    let (cache, requests) = OperationalCache::new();
    tokio::spawn(run_operational_cache(
        sock,
        cache.clone(),
        requests,
        scope_id,
    ));
    Ok(cache)
}
```

（`OperationalFold` はこのタスクで初めて実際に使われる。Task 3 で
`#[derive(Default)]` のみでは dead_code 警告が出得るため、Task 3・4 は連続実装を
推奨。単独で clippy を通す必要がある場合、Task 3 の型に一時的な
`#[allow(dead_code)]` は付けない — Task 4 で消費されるので順に実装すること。）

- [ ] **Step 2: bind 成功パスのスモークテストを追加**

`#[cfg(test)] mod tests` に追加（loopback で bind できることのみ確認。実応答は
E2E 側で検証）:

```rust
    #[tokio::test]
    async fn spawn_operational_cache_binds_on_loopback() {
        // lo の ifindex。取得できない CI もあるため取得可否で分岐。
        let Ok(scope) = iface_index("lo") else {
            return; // lo が無い環境ではスキップ（bind の型検証は他テストで担保）。
        };
        // 二重 bind（REUSEADDR）で失敗しないこと＝常駐 socket が確立できる。
        let a = spawn_operational_cache(scope);
        assert!(a.is_ok(), "operational cache should bind on lo: {a:?}");
    }
```

- [ ] **Step 3: テスト実行**

Run: `cargo test -p mat-controller --lib dnssd::tests::spawn_operational_cache 2>&1 | tail -8`
Expected: PASS（または lo 無し環境で早期 return）。

- [ ] **Step 4: `task check` & commit**

Run: `task check 2>&1 | grep -E "test result:|error|warning:" | tail -5`
Expected: ok（dead_code 警告が無いこと＝`OperationalFold` が消費されている）。

```bash
git add crates/mat-controller/src/dnssd.rs
git commit -m "$(cat <<'EOF'
feat(dnssd): 常駐 operational mDNS リスナ run/spawn_operational_cache

単一 mDNS socket(5353+ff02::fb join) を常駐させ受信を畳み込みキャッシュを温める。
provoke リクエストで対象 instance の SRV+TXT クエリを送出。bind 失敗は Err で
返し、呼び出し側(matd)が degrade できるようにする。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VA7F1Xguc3LeDhxefPHh3W
EOF
)"
```

---

### Task 5: `CachingResolver`（キャッシュ参照 + ミス時の await）

matd 用リゾルバ。ヒットは即返し、ミスは provoke してリスナの次アナウンスを最大
`CACHE_MISS_TIMEOUT`（35s）まで poll で待つ。`RESOLVE_TIMEOUT`（8s）は変えず、
miss 窓は CachingResolver 内部定数で持つ（`mat` 一発を無変更に保つ）。

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（`CachingResolver` 追加 + テスト）

**Interfaces:**
- Consumes: `Resolver`（Task 1）、`dnssd::{OperationalCache, ResolvedNode, DnssdError, operational_instance}`（既存/Task 2）。
- Produces:
  - `pub struct CachingResolver { cache: dnssd::OperationalCache }`
  - `pub fn new(cache: dnssd::OperationalCache) -> Self`
  - `Resolver` 実装（ヒット即返し / ミス await / timeout=`DnssdError::Timeout`）
  - 定数 `CACHE_MISS_TIMEOUT: Duration = Duration::from_secs(35)`、`CACHE_POLL: Duration = Duration::from_millis(500)`

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-native/src/lib.rs` の tests に追加:

```rust
    #[tokio::test(start_paused = true)]
    async fn caching_resolver_returns_cached_hit_immediately() {
        use mat_controller::dnssd;
        let (cache, _rx) = dnssd::OperationalCache::new();
        let inst = dnssd::operational_instance(&[0xAB; 8], 5) + "._matter._tcp.local";
        cache.insert(
            inst,
            dnssd::ResolvedNode {
                port: 5540,
                addresses: vec!["fd00::1".parse().unwrap()],
                session_idle_interval_ms: None,
                session_active_interval_ms: None,
            },
            std::time::Duration::from_secs(60),
        );
        let r = CachingResolver::new(cache);
        let n = r
            .resolve(1, [0xAB; 8], 5, std::time::Duration::from_secs(8))
            .await
            .expect("hit");
        assert_eq!(n.port, 5540);
    }

    #[tokio::test(start_paused = true)]
    async fn caching_resolver_awaits_listener_fill_then_returns() {
        use mat_controller::dnssd;
        let (cache, mut rx) = dnssd::OperationalCache::new();
        let inst = dnssd::operational_instance(&[0xAB; 8], 7) + "._matter._tcp.local";
        let filler = cache.clone();
        let inst2 = inst.clone();
        // 別タスクが少し後に埋める（リスナ相当）。
        tokio::spawn(async move {
            // provoke request が届くはず。
            let got = rx.recv().await.unwrap();
            assert_eq!(got, inst2);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            filler.insert(
                inst2,
                dnssd::ResolvedNode {
                    port: 5541,
                    addresses: vec!["fd00::2".parse().unwrap()],
                    session_idle_interval_ms: None,
                    session_active_interval_ms: None,
                },
                std::time::Duration::from_secs(60),
            );
        });
        let r = CachingResolver::new(cache);
        let n = r.resolve(1, [0xAB; 8], 7, std::time::Duration::from_secs(8)).await.expect("fill");
        assert_eq!(n.port, 5541);
    }

    #[tokio::test(start_paused = true)]
    async fn caching_resolver_times_out_when_never_filled() {
        use mat_controller::dnssd;
        let (cache, _rx) = dnssd::OperationalCache::new();
        let r = CachingResolver::new(cache);
        let out = r.resolve(1, [0xAB; 8], 9, std::time::Duration::from_secs(8)).await;
        assert!(matches!(out, Err(dnssd::DnssdError::Timeout { .. })));
    }
```

- [ ] **Step 2: テスト失敗を確認**

Run: `cargo test -p mat-native --lib caching_resolver 2>&1 | tail -8`
Expected: FAIL（`CachingResolver` 未定義）。

- [ ] **Step 3: `CachingResolver` を実装**

`OneShotResolver`（Task 1）の直後に追加:

```rust
/// matd 用リゾルバ: 常駐 mDNS キャッシュ（[`dnssd::OperationalCache`]）を参照し、
/// ヒットは即返し、ミス時は provoke してリスナの次アナウンスを
/// `CACHE_MISS_TIMEOUT` まで待つ。establish から渡される `timeout`（8s）ではなく
/// この内部定数を使う理由は spec 参照（`mat` 一発を無変更に保つため窓を分離）。
pub struct CachingResolver {
    cache: dnssd::OperationalCache,
}

/// cache miss 時にリスナの次アナウンス（周期~30s）を確実に跨ぐ待ち窓。
const CACHE_MISS_TIMEOUT: Duration = Duration::from_secs(35);
/// キャッシュ充填の poll 間隔（Notify を使わず単純 poll で取りこぼしを防ぐ）。
const CACHE_POLL: Duration = Duration::from_millis(500);

impl CachingResolver {
    pub fn new(cache: dnssd::OperationalCache) -> Self {
        Self { cache }
    }
}

#[async_trait]
impl Resolver for CachingResolver {
    async fn resolve(
        &self,
        _scope_id: u32,
        cfid: [u8; 8],
        node_id: u64,
        _timeout: Duration,
    ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError> {
        let instance =
            format!("{}._matter._tcp.local", dnssd::operational_instance(&cfid, node_id));
        if let Some(n) = self.cache.get(&instance) {
            return Ok(n);
        }
        // ミス: listener に provoke クエリを依頼し、次アナウンス/応答を待つ。
        self.cache.request(instance.clone());
        let deadline = tokio::time::Instant::now() + CACHE_MISS_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(CACHE_POLL).await;
            if let Some(n) = self.cache.get(&instance) {
                return Ok(n);
            }
        }
        Err(dnssd::DnssdError::Timeout { instance })
    }
}
```

（`dnssd::operational_instance` は既に `pub`。`DnssdError::Timeout` は `pub`。
`async_trait` / `Duration` は Task 1 で import 済み。）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native --lib caching_resolver 2>&1 | tail -10`
Expected: PASS（3 テスト。`start_paused` で 35s 待ちも即時消化）。

- [ ] **Step 5: `task check` & commit**

Run: `task check 2>&1 | grep -E "test result:|error|warning:" | tail -5`
Expected: ok。

```bash
git add crates/mat-native/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(native): CachingResolver（matd 常駐キャッシュ参照リゾルバ）

ヒットは即返し、ミス時は provoke してリスナの次アナウンスを内部窓
CACHE_MISS_TIMEOUT(35s) まで poll で待つ。RESOLVE_TIMEOUT(8s) は据え置き、
miss 窓を分離して mat 一発の挙動を変えない。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VA7F1Xguc3LeDhxefPHh3W
EOF
)"
```

---

### Task 6: `Engine::build_with_resolver` + matd 配線（degrade 付き）

matd 起動時に常駐キャッシュを spawn し `CachingResolver` を注入する。bind 失敗は
warn して `OneShotResolver`（従来動作）に degrade。

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（`Engine::build_with_resolver` + `build` を委譲化）
- Modify: `crates/matd/src/native.rs`（`NativeBackend::build_with_resolver`）
- Modify: `crates/matd/src/main.rs`（起動時のキャッシュ spawn + resolver 選択）

**Interfaces:**
- Consumes: `Resolver`, `OneShotResolver`, `CachingResolver`（Task 1/5）、
  `dnssd::spawn_operational_cache`（Task 4）、`dnssd::iface_index`（既存）。
- Produces:
  - `Engine::build_with_resolver(cfg: &NativeConfig, resolver: Arc<dyn Resolver>) -> Result<Self, MatError>`
  - `NativeBackend::build_with_resolver(cfg: &NativeConfig, resolver: Arc<dyn Resolver>) -> Result<Self, MatError>`

- [ ] **Step 1: `Engine::build` を `build_with_resolver` へ委譲化**

`crates/mat-native/src/lib.rs`: `build` 本体を `build_with_resolver` に移し、
establisher 構築の `resolver:` を引数から受ける。`build` は既定を渡す薄いラッパに:

```rust
    pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError> {
        Self::build_with_resolver(cfg, Arc::new(OneShotResolver)).await
    }

    /// [`build`] と同じだが、establish の mDNS 解決に使う [`Resolver`] を注入する
    /// （matd が `CachingResolver` を渡す。`mat` 一発は `build` の OneShotResolver）。
    pub async fn build_with_resolver(
        cfg: &NativeConfig,
        resolver: Arc<dyn Resolver>,
    ) -> Result<Self, MatError> {
        // ... 既存 build の本体（KVS 読み〜transport bind〜group 構築）をそのまま ...
        let establisher = CaseEstablisher {
            creds: Arc::new(creds),
            transport: Arc::new(Transport::Udp(Arc::clone(&transport))),
            scope_id,
            resolver,
        };
        Ok(Self { establisher: Box::new(establisher), group: Some(group), group_settings: Some(group_settings) })
    }
```

（Task 1 で `build` 内に直接書いた `resolver: Arc::new(OneShotResolver)` は、この
移設で `build_with_resolver` の引数 `resolver` に置き換わる。）

- [ ] **Step 2: `NativeBackend::build_with_resolver` を追加**

`crates/matd/src/native.rs` の `build` の隣に:

```rust
    /// [`build`] と同じだが Resolver を注入する（matd が CachingResolver を渡す）。
    pub async fn build_with_resolver(
        cfg: &NativeConfig,
        resolver: std::sync::Arc<dyn mat_native::Resolver>,
    ) -> Result<Self, MatError> {
        Ok(Self::from_engine(
            mat_native::Engine::build_with_resolver(cfg, resolver).await?,
        ))
    }
```

（`mat_native::Resolver` を再エクスポート: `native.rs` 冒頭の
`pub use mat_native::{Establisher, NativeConfig, NodeConn};` に `Resolver` を追加。）

- [ ] **Step 3: matd 起動で常駐キャッシュを spawn し resolver を選ぶ**

`crates/matd/src/main.rs` の `NativeBackend::build(&cfg)` 呼び出し箇所を特定し
（`cfg.iface` / `NativeConfig` を組む付近）、以下へ置換:

```rust
    // 常駐 mDNS キャッシュ（周期アナウンス依存の弱リンク Thread ノードの
    // establish を確実化）。bind 失敗時は OneShotResolver に degrade。
    let resolver: std::sync::Arc<dyn mat_native::Resolver> =
        match mat_controller::dnssd::iface_index(&cfg.iface) {
            Ok(scope_id) => match mat_controller::dnssd::spawn_operational_cache(scope_id) {
                Ok(cache) => {
                    tracing::info!("matd: resident mDNS operational cache enabled");
                    std::sync::Arc::new(mat_native::CachingResolver::new(cache))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "matd: mDNS cache bind failed; using one-shot resolver");
                    std::sync::Arc::new(mat_native::OneShotResolver)
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "matd: iface index unresolved; using one-shot resolver");
                std::sync::Arc::new(mat_native::OneShotResolver)
            }
        };
    let backend = NativeBackend::build_with_resolver(&cfg, resolver).await;
```

（元の `let backend = NativeBackend::build(&cfg).await;` を置換。`cfg` は
`NativeConfig`。`mat_controller` / `mat_native` が matd の依存に無ければ Cargo.toml
に追加 — 既に `mat_native` は依存、`mat_controller` も native.rs 経由で推移依存だが
直接 path 参照が要る場合は `crates/matd/Cargo.toml` に `mat-controller` を追加。)

- [ ] **Step 4: ビルド & 既存テスト**

Run: `cargo build -p matd 2>&1 | tail -5 && cargo test -p matd 2>&1 | grep "test result" | tail -5`
Expected: ビルド成功、既存 matd テスト（FakeEstablisher 経由 = resolver 非依存）PASS。

- [ ] **Step 5: matd が CachingResolver で構築できる結線テスト**

`crates/matd/src/native.rs` の tests に追加（実 socket は張らず、キャッシュ
ハンドルから CachingResolver を作り build_with_resolver に渡せることを型・構築で確認）:

```rust
    #[tokio::test]
    async fn build_with_resolver_accepts_caching_resolver() {
        // 実 KVS が無いので build 自体は Err になるが、CachingResolver を
        // Arc<dyn Resolver> として渡せる（型・API の結線）ことを確認する。
        let (cache, _rx) = mat_controller::dnssd::OperationalCache::new();
        let resolver: std::sync::Arc<dyn mat_native::Resolver> =
            std::sync::Arc::new(mat_native::CachingResolver::new(cache));
        let cfg = NativeConfig {
            store: std::path::PathBuf::from("/nonexistent"),
            iface: "lo".into(),
            fabric_index: 1,
            issuer_index: 0,
        };
        let r = NativeBackend::build_with_resolver(&cfg, resolver).await;
        assert!(r.is_err()); // KVS 不在で store_missing。型結線が通ることが要点。
    }
```

（`mat_controller` が matd の dev/通常依存に必要。無ければ Cargo.toml に追加。)

Run: `cargo test -p matd build_with_resolver 2>&1 | tail -8`
Expected: PASS。

- [ ] **Step 6: `task check` & commit**

Run: `task check 2>&1 | grep -E "test result:|error|warning:" | tail -5`
Expected: ok。

```bash
git add crates/mat-native/src/lib.rs crates/matd/src/native.rs crates/matd/src/main.rs crates/matd/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(matd): 起動時に常駐 mDNS キャッシュを spawn し CachingResolver を注入

Engine/NativeBackend に build_with_resolver を追加。matd は iface scope を
解決して operational cache を spawn、成功なら CachingResolver、bind/iface 失敗は
warn して OneShotResolver に degrade（従来動作で継続）。mat 一発は build のまま
OneShotResolver。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VA7F1Xguc3LeDhxefPHh3W
EOF
)"
```

---

### Task 7: 実機 E2E（jarvis）+ ドキュメント + バージョン

**Files:**
- Modify: `ARCHITECTURE.md`（M8c-3 回帰の恒久修正＝層1/層2 の記録追記）
- Modify: `Cargo.toml`（version 0.22.0 → 0.23.0）
- Modify: `crates/mat-controller/src/dnssd.rs` モジュール doc（常駐キャッシュの言及、任意）

**Interfaces:** なし（ドキュメント/リリース）。

- [ ] **Step 1: aarch64 ビルド**

Run: `task dist:arm64 2>&1 | tail -3`
Expected: `dist/arm64/{mat,matd}` 生成。

- [ ] **Step 2: jarvis へ matd を差し替え（despliegue skill 準拠）**

despliegue skill の手順で `dist/arm64/matd` を jarvis の `~/.local/bin/matd` へ
配置し、`MAT_MATD_IFACE=eth0` / `MAT_MATD_FABRIC_INDEX=2` のまま再起動する。
（本番は 0.19.0 のままなので、まず検証機／検証用パスで確認 → 問題なければ
本採用。ロールバックの経緯は [[jarvis-matd-deploy]] を尊重し、ユーザー確認の上で。)

- [ ] **Step 3: 受け入れ E2E — 再起動直後の確実性**

```bash
ssh jarvis 'sudo systemctl restart matd; sleep 1; \
  for R in 1 2 3 4 5; do printf "round $R: "; \
    for N in 5 8 14; do \
      if mat read -n $N -e 1 -c onoff -a on-off >/dev/null 2>&1; then printf "n$N=OK "; else printf "n$N=FAIL "; fi; \
    done; echo; done'
```
Expected: 初回ラウンドは cold で一部が最大~35s かかり得るが、**全ラウンドで
最終的に OK**（間欠 FAIL が消えること）。node6 等 healthy に回帰なし。

- [ ] **Step 4: 回帰確認（healthy ノード即応・warm 再利用）**

同じノードへ連続コマンドが warm session で即応することを確認（establish は初回のみ）。

- [ ] **Step 5: バージョン & ドキュメント更新**

`Cargo.toml` の `version = "0.22.0"` を `"0.23.0"` に。`ARCHITECTURE.md` の M8c-3
記録に、native resolve 回帰の恒久修正（層1: 5353 bind+join+QU / 層2: matd 常駐
mDNS キャッシュ）と実機 E2E 結果を追記する。

- [ ] **Step 6: `task check` & commit**

Run: `task check 2>&1 | grep -E "test result:|error|warning:" | tail -5`
Expected: ok。

```bash
git add Cargo.toml ARCHITECTURE.md crates/mat-controller/src/dnssd.rs
git commit -m "$(cat <<'EOF'
docs+release: native resolve 回帰の恒久修正を記録、0.23.0

層1(5353 bind+join+QU) + 層2(matd 常駐 mDNS キャッシュ)で弱リンク Thread ノード
の establish を確実化。jarvis 実機 E2E で matd 再起動後の node5/8/14 制御が
全ラウンド成功することを確認。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01VA7F1Xguc3LeDhxefPHh3W
EOF
)"
```

---

## Self-Review

**Spec coverage:**
- 常駐キャッシュ（get/insert/TTL/上限）→ Task 2. ✓
- 畳み込み（operational 判定・SRV+AAAA 完成でキャッシュ）→ Task 3. ✓
- 常駐リスナ（単一 socket・provoke・耐性）→ Task 4. ✓
- Resolver 抽象（mat=OneShot / matd=Caching）→ Task 1, 5. ✓
- ミス時 await ~35s / ヒット即返し / 窓分離（RESOLVE_TIMEOUT 8s 据置）→ Task 5. ✓
- matd 配線 + bind 失敗 degrade → Task 6. ✓
- 設計ルール4（mat 無変更）→ Task 1(build 既定 OneShot)・全体で mat 側ロジック不変. ✓
- テスト（cache/fold/resolver ユニット + 実機 E2E）→ Task 2-6 各ユニット + Task 7 E2E. ✓
- バージョン/ドキュメント → Task 7. ✓

**Placeholder scan:** TBD/TODO/「適宜」なし。各コード step に実コードあり。E2E は
外部実機のため手順を明示（コード step ではなく運用 step、許容）。

**Type consistency:**
- `OperationalCache::new() -> (Self, mpsc::UnboundedReceiver<String>)` — Task 2 定義、
  Task 4(`spawn`)・Task 5/6 テストで同形で使用. ✓
- `get(&str)->Option<ResolvedNode>` / `request(String)` / `insert(String,ResolvedNode,Duration)`
  — Task 2 定義、Task 3(insert)・4(request 経路)・5(get/request) で一致. ✓
- `Resolver::resolve(&self,u32,[u8;8],u64,Duration)->Result<ResolvedNode,DnssdError>`
  — Task 1 定義、OneShot(Task1)/Caching(Task5) 実装で一致. ✓
- `spawn_operational_cache(u32)->io::Result<OperationalCache>` — Task 4 定義、Task 6 で使用. ✓
- `Engine::build_with_resolver(&NativeConfig, Arc<dyn Resolver>)` / 同 NativeBackend —
  Task 6 定義・使用で一致. ✓
- `fold_operational_into_cache(&[Record], &mut OperationalFold, &OperationalCache)` —
  Task 3 定義、Task 4 で使用. ✓
