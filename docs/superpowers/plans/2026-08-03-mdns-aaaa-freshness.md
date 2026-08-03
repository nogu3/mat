# 常駐 mDNS AAAA 鮮度（監査⑤）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matd 常駐 mDNS の AAAA プールに RFC 6762 cache-flush 準拠の stale 削除と鮮度順ソートを入れ、再アドレス化ノードの再確立を新アドレス先頭試行にし、上限 8 本での学習拒否（恒久 unreachable）を根絶する。

**Architecture:** 変更は `crates/mat-controller/src/dnssd.rs` に閉じる。(1) パーサが class フィールドの cache-flush ビットを `Record` に保持、(2) fold の per-host AAAA プールを `(Ipv6Addr, last_seen: Instant)` にして cache-flush 受信時に「1 秒より古い」記録を物理削除（RFC 6762 §10.2 の burst 猶予）、(3) `ResolvedNode` 構築時に「非 LL 優先 → 同群内 last_seen 新しい順」でソート、満杯時は最古 evict。spec: `docs/superpowers/specs/2026-08-03-mdns-aaaa-freshness-design.md`。

**Tech Stack:** Rust / tokio（`tokio::time::Instant` — dnssd.rs は既にこれを import しており、`#[tokio::test(start_paused = true)]` + `advance` で時刻固定テストが既存パターン）。

## Global Constraints

- ブランチ: `fix/tier2-aaaa-freshness`（main から作成）。バージョン: 1.19.0（workspace `Cargo.toml` の `version`、最終タスクで bump）。
- 変更ファイルは `crates/mat-controller/src/dnssd.rs` と `Cargo.toml`/`Cargo.lock` のみ。`mat` 一発経路（`resolve_operational` / `OneShotResolver`）・消費側（establish ループ）・`OperationalCache` の insert/get 規律は**無変更**。
- 各タスク完了時に `cargo test -p mat-controller dnssd` が全通過していること。最終タスクで `task check`（fmt:check + clippy + test）。
- コミットは各タスク末尾で行う（このセッションで編集したファイルのみ `git add`。作業ツリーに既存の `CLAUDE.md` 変更があるが**含めない**）。
- fold 内の時刻は必ず引数 `now` を使う（`Instant::now()` を fold 内で呼ばない — テスト注入可能性の維持）。
- `Instant` は `tokio::time::Instant`（ファイル先頭で import 済み。std のものを新たに import しない）。

---

### Task 0: ブランチ作成

**Files:** なし（git 操作のみ）

- [ ] **Step 1: main から fix ブランチを切る**

```bash
git -C /home/noguk/ghq/github.com/nogu3/mat switch -c fix/tier2-aaaa-freshness main
```

Expected: `Switched to a new branch 'fix/tier2-aaaa-freshness'`（作業ツリーの `CLAUDE.md` 変更はそのまま持ち越しで良い — コミットに含めなければ無害）。

---

### Task 1: パーサが cache-flush ビットを保持する

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`（`Record` 構造体 ~L400、`parse_message` ~L467-541、テストモジュール ~L2516 の `synth_aaaa_only` 付近）

**Interfaces:**
- Produces: `Record.cache_flush: bool`（Task 2 の fold が読む）。テストヘルパ `fn synth_aaaa_class(name: &str, ttl: u32, addr: Ipv6Addr, class: u16) -> Vec<u8>`（Task 3 のテストが `CLASS_IN`＝cache-flush 無しで使う）。

- [ ] **Step 1: テストヘルパをリファクタし、失敗するテストを書く**

`crates/mat-controller/src/dnssd.rs` のテストモジュール内、既存 `synth_aaaa_only`（~L2516）を次の 2 関数に置き換える:

```rust
    /// class を指定できる AAAA 単独メッセージ（cache-flush ビット検証用）。
    fn synth_aaaa_class(name: &str, ttl: u32, addr: Ipv6Addr, class: u16) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&[0, 0, 0x84, 0x00]); // id 0, QR|AA
        m.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 0]); // qd 0, an 1
        push_name(&mut m, name);
        m.extend_from_slice(&TYPE_AAAA.to_be_bytes());
        m.extend_from_slice(&class.to_be_bytes());
        m.extend_from_slice(&ttl.to_be_bytes());
        m.extend_from_slice(&16u16.to_be_bytes());
        m.extend_from_slice(&addr.octets());
        m
    }

    fn synth_aaaa_only(name: &str, ttl: u32, addr: Ipv6Addr) -> Vec<u8> {
        synth_aaaa_class(name, ttl, addr, 0x8000 | CLASS_IN) // cache-flush|IN
    }
```

（既存 `synth_aaaa_only` の中身はビット列が同一なので既存テストは無影響。）

続けて新テストを追加:

```rust
    /// RFC 6762 §10.2 の cache-flush ビット（class 最上位）を Record に保持する。
    /// class 自体は従来通り検証しない。
    #[test]
    fn parse_message_reads_cache_flush_bit() {
        let addr: Ipv6Addr = "fd00::1".parse().unwrap();
        let with = parse_message(&synth_aaaa_class("h.local", 120, addr, 0x8000 | CLASS_IN)).unwrap();
        assert!(with[0].cache_flush);
        let without = parse_message(&synth_aaaa_class("h.local", 120, addr, CLASS_IN)).unwrap();
        assert!(!without[0].cache_flush);
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller parse_message_reads_cache_flush_bit`
Expected: コンパイルエラー `no field 'cache_flush' on type ...Record`

- [ ] **Step 3: 実装**

`Record`（~L400）にフィールド追加:

```rust
struct Record {
    name: String,
    rdata: RData,
    ttl: u32,
    /// RFC 6762 §10.2 の cache-flush ビット（class フィールド最上位）。class
    /// 自体の検証は従来通りしない（mDNS は IN-only）。
    cache_flush: bool,
}
```

`parse_message` の doc コメント（~L467-469）を差し替え:

```rust
/// Parses the answer + authority + additional records of one DNS message.
/// Record classes are not validated (mDNS is IN-only); only the RFC 6762
/// cache-flush bit (top bit of the class field) is surfaced on each record.
```

レコード読み取りループ（~L487-489）で class を拾う:

```rust
        let (name, p) = read_name(buf, pos)?;
        let rtype = be16(buf, p)?;
        let cache_flush = be16(buf, p + 2)? & 0x8000 != 0;
        let ttl = be32(buf, p + 4)?;
```

push（~L541）を変更:

```rust
        records.push(Record { name, rdata, ttl, cache_flush });
```

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller dnssd`
Expected: 全 PASS（新テスト含む。既存テストはヘルパ等価リファクタなので無影響）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/dnssd.rs
git commit -m "feat(mat-controller): parse_message が RFC 6762 cache-flush ビットを Record に保持（監査⑤ 前段）"
```

---

### Task 2: fold の鮮度化 — per-address last_seen + cache-flush での stale 物理削除

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`（`OperationalFold` ~L1242-1251、`fold_operational_into_cache` ~L1263-1335、`run_operational_cache` の呼び出し ~L1354、定数群 ~L1226、テストモジュールの fold 呼び出し 8 箇所 + 新テスト 2 本）

**Interfaces:**
- Consumes: `Record.cache_flush`（Task 1）。
- Produces: `fold_operational_into_cache(records: &[Record], fold: &mut OperationalFold, cache: &OperationalCache, now: Instant)`（第 4 引数追加）。`OperationalFold.addrs: HashMap<String, Vec<(Ipv6Addr, Instant)>>`。定数 `CACHE_FLUSH_GRACE: Duration`（Task 3 は変更しない）。

- [ ] **Step 1: 失敗するテストを 2 本書く**

テストモジュール（`fold_cross_datagram_srv_then_aaaa_completes` の近く）に追加:

```rust
    /// アドレスローテーション（監査⑤の欠陥 1）: cache-flush 付きの新 AAAA が
    /// 届いたら、1 秒より古い旧アドレスは物理削除され、キャッシュは新アドレス
    /// のみになる。
    #[tokio::test(start_paused = true)]
    async fn fold_cache_flush_evicts_stale_addresses() {
        let (cache, _rx) = OperationalCache::new();
        let mut fold = OperationalFold::default();
        let service = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target = "hosta.local";
        let old: Ipv6Addr = "fd00::1".parse().unwrap();
        let new: Ipv6Addr = "fd00::2".parse().unwrap();

        let msg = synth_response(service, target, 5540, &["SII=5000"], old);
        fold_operational_into_cache(&parse_message(&msg).unwrap(), &mut fold, &cache, Instant::now());
        assert_eq!(cache.get(service).unwrap().addresses, vec![old]);

        tokio::time::advance(Duration::from_secs(2)).await;

        // 再アドレス化: cache-flush 付き AAAA（synth_aaaa_only は cache-flush|IN）。
        let msg2 = synth_aaaa_only(target, 120, new);
        fold_operational_into_cache(&parse_message(&msg2).unwrap(), &mut fold, &cache, Instant::now());
        assert_eq!(
            cache.get(service).unwrap().addresses,
            vec![new],
            "stale address must be physically removed on cache-flush"
        );
    }

    /// RFC 6762 §10.2 の 1 秒猶予: 同一アナウンス burst の分割データグラム
    /// （≤1s 間隔）は cache-flush でも互いを消さない。
    #[tokio::test(start_paused = true)]
    async fn fold_cache_flush_grace_keeps_same_burst() {
        let (cache, _rx) = OperationalCache::new();
        let mut fold = OperationalFold::default();
        let service = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target = "hosta.local";
        let a1: Ipv6Addr = "fd00::1".parse().unwrap();
        let a2: Ipv6Addr = "fd00::2".parse().unwrap();

        let m1 = synth_aaaa_only(target, 120, a1);
        fold_operational_into_cache(&parse_message(&m1).unwrap(), &mut fold, &cache, Instant::now());
        tokio::time::advance(Duration::from_millis(500)).await;
        let m2 = synth_aaaa_only(target, 120, a2);
        fold_operational_into_cache(&parse_message(&m2).unwrap(), &mut fold, &cache, Instant::now());

        let msg = synth_srv_txt_only(service, target, 5540, &["SII=5000"]);
        fold_operational_into_cache(&parse_message(&msg).unwrap(), &mut fold, &cache, Instant::now());
        let node = cache.get(service).unwrap();
        assert_eq!(node.addresses.len(), 2, "both burst datagrams must survive the 1s grace");
        assert!(node.addresses.contains(&a1) && node.addresses.contains(&a2));
    }
```

- [ ] **Step 2: コンパイルが落ちることを確認**

Run: `cargo test -p mat-controller fold_cache_flush`
Expected: コンパイルエラー（`fold_operational_into_cache` の引数個数不一致）

- [ ] **Step 3: 実装**

(a) 定数追加（`MAX_ADDRS_PER_HOST` ~L1226 の直後）:

```rust
/// RFC 6762 §10.2: cache-flush 受信時に破棄してよいのは「1 秒より古い」記録
/// だけ。この猶予が、同一アナウンス burst が複数データグラムに分割された
/// 場合の相互破壊を防ぐ。
const CACHE_FLUSH_GRACE: Duration = Duration::from_secs(1);
```

(b) `OperationalFold.addrs` のフィールドと doc を変更（~L1248-1250）:

```rust
    /// host_lower(SRV target) → (アドレス, last_seen)。dedup、
    /// [`MAX_ADDRS_PER_HOST`] で頭打ち。last_seen は鮮度順ソートと
    /// cache-flush / 満杯 evict の判定材料（監査⑤）。
    addrs: HashMap<String, Vec<(Ipv6Addr, Instant)>>,
```

(c) `fold_operational_into_cache` に `now: Instant` を追加し、AAAA 分岐（~L1291-1308）を差し替える。**fold 内で `Instant::now()` を呼ばないこと**（時刻注入の維持）:

```rust
fn fold_operational_into_cache(
    records: &[Record],
    fold: &mut OperationalFold,
    cache: &OperationalCache,
    now: Instant,
) {
```

```rust
            RData::Aaaa(addr) => {
                let host = r.name.to_ascii_lowercase();
                let is_new_host = !fold.addrs.contains_key(&host);
                if !(is_new_host && fold.addrs.len() >= MAX_FOLD_ENTRIES) {
                    let list = fold.addrs.entry(host.clone()).or_default();
                    if r.cache_flush {
                        // cache-flush: 1 秒より古い既存アドレスは「現在の広告に
                        // 含まれない」ものとして物理削除する（stale を先に試す
                        // 79s 浪費と恒久 unreachable の元 — 監査⑤）。
                        list.retain(|(a, seen)| {
                            let keep = now.duration_since(*seen) <= CACHE_FLUSH_GRACE;
                            if !keep {
                                tracing::debug!(host = %host, addr = %a, "aaaa evicted (cache-flush)");
                            }
                            keep
                        });
                    }
                    if let Some(entry) = list.iter_mut().find(|(a, _)| a == addr) {
                        entry.1 = now;
                    } else if list.len() < MAX_ADDRS_PER_HOST {
                        list.push((*addr, now));
                    }
                }
                // この host を SRV target に持つ既知 instance を touched に。
                // これがクロス datagram 完成（SRV が先、AAAA が後）を支える。
                for (inst, (_, target, _)) in &fold.srv {
                    if target.eq_ignore_ascii_case(&host) {
                        touched.insert(inst.clone());
                    }
                }
            }
```

（満杯時の evict は Task 3。このタスクでは現行の「満杯なら追加しない」を維持する。）

(d) `ResolvedNode` 構築（~L1313-1330 の touched ループ内）をタプル対応に。**このタスクでは順序規律は現行のまま**（非 LL 先頭のみ。鮮度順は Task 3）:

```rust
        let mut addresses: Vec<Ipv6Addr> = addrs.iter().map(|(a, _)| *a).collect();
        addresses.sort_by_key(is_link_local);
```

（続く `let node = ResolvedNode { ... addresses, ... }` は無変更。）

(e) listener 呼び出し（~L1354）:

```rust
                fold_operational_into_cache(&records, &mut fold, &cache, Instant::now());
```

(f) 既存テストの fold 呼び出し全 8 箇所（L1503, L1527, L2539, L2547, L2572, L2578, L2601, L2617 付近 — `grep -n 'fold_operational_into_cache(' ` で全数確認すること）に第 4 引数 `Instant::now()` を追加。

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller dnssd`
Expected: 全 PASS。特に新 2 本と、既存の `fold_departed_instance_expires_and_is_not_refreshed` / `fold_cross_datagram_srv_then_aaaa_completes` / `fold_no_global_aaaa_starvation` が無退行であること。

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/dnssd.rs
git commit -m "fix(mat-controller): AAAA プールを鮮度付きにし cache-flush で stale を物理削除（監査⑤）"
```

---

### Task 3: 鮮度順ソート + 満杯時は最古 evict

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`（Task 2 で触った AAAA 分岐の else 側と `ResolvedNode` 構築部、新テスト 2 本）

**Interfaces:**
- Consumes: `OperationalFold.addrs: HashMap<String, Vec<(Ipv6Addr, Instant)>>`、`synth_aaaa_class`（Task 1）、`CLASS_IN`（既存定数）。
- Produces: `ResolvedNode.addresses` の並びが「非 LL 優先 → 同群内 last_seen 新しい順」になる（外部契約「非 LL 先頭」は不変）。

- [ ] **Step 1: 失敗するテストを 2 本書く**

```rust
    /// バックストップ（cache-flush を立てない実装向け・監査⑤の欠陥 1）:
    /// 旧アドレスは残るが、後から見たアドレスが先頭に並び先に試される。
    #[tokio::test(start_paused = true)]
    async fn fold_freshness_orders_latest_first() {
        let (cache, _rx) = OperationalCache::new();
        let mut fold = OperationalFold::default();
        let service = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target = "hosta.local";
        let a1: Ipv6Addr = "fd00::1".parse().unwrap();
        let a2: Ipv6Addr = "fd00::2".parse().unwrap();

        // cache-flush 無し（class=IN のみ）なので物理削除は起きない。
        let m1 = synth_aaaa_class(target, 120, a1, CLASS_IN);
        fold_operational_into_cache(&parse_message(&m1).unwrap(), &mut fold, &cache, Instant::now());
        tokio::time::advance(Duration::from_secs(2)).await;
        let m2 = synth_aaaa_class(target, 120, a2, CLASS_IN);
        fold_operational_into_cache(&parse_message(&m2).unwrap(), &mut fold, &cache, Instant::now());

        let msg = synth_srv_txt_only(service, target, 5540, &["SII=5000"]);
        fold_operational_into_cache(&parse_message(&msg).unwrap(), &mut fold, &cache, Instant::now());
        assert_eq!(
            cache.get(service).unwrap().addresses,
            vec![a2, a1],
            "freshest address must be tried first"
        );
    }

    /// 満杯 8 本での学習拒否の反転（監査⑤の欠陥 2 = 成功基準 2）: 9 本目は
    /// 最古を追い出して学習される。恒久 unreachable の根絶。
    #[tokio::test(start_paused = true)]
    async fn fold_full_pool_evicts_oldest_not_newest() {
        let (cache, _rx) = OperationalCache::new();
        let mut fold = OperationalFold::default();
        let service = "AB7DE08802E0CD54-0000000000000005._matter._tcp.local";
        let target = "hosta.local";
        let mut addrs = Vec::new();
        for i in 0..9u16 {
            let a = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, i + 1);
            addrs.push(a);
            let m = synth_aaaa_class(target, 120, a, CLASS_IN);
            fold_operational_into_cache(&parse_message(&m).unwrap(), &mut fold, &cache, Instant::now());
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        let msg = synth_srv_txt_only(service, target, 5540, &["SII=5000"]);
        fold_operational_into_cache(&parse_message(&msg).unwrap(), &mut fold, &cache, Instant::now());
        let node = cache.get(service).unwrap();
        assert_eq!(node.addresses.len(), MAX_ADDRS_PER_HOST);
        assert!(!node.addresses.contains(&addrs[0]), "oldest must be evicted");
        assert_eq!(node.addresses[0], addrs[8], "newest must be first");
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller fold_freshness_orders_latest_first fold_full_pool_evicts_oldest_not_newest` は 2 パターン指定できないため 2 回に分けて実行:
`cargo test -p mat-controller fold_freshness_orders` → FAIL（`vec![a1, a2]` の順で返る = 挿入順のまま）
`cargo test -p mat-controller fold_full_pool_evicts` → FAIL（9 本目が学習されず `addrs[0]` が残る）

- [ ] **Step 3: 実装**

(a) AAAA 分岐の `else if list.len() < MAX_ADDRS_PER_HOST { ... }`（Task 2 (c) で書いた箇所）を差し替え:

```rust
                    if let Some(entry) = list.iter_mut().find(|(a, _)| a == addr) {
                        entry.1 = now;
                    } else {
                        if list.len() >= MAX_ADDRS_PER_HOST {
                            // 満杯: 最古 last_seen を追い出して新アドレスを学習する。
                            // 旧来の「新規拒否」は全 stale 時に恒久 unreachable を
                            // 招いた（監査⑤の欠陥 2）。
                            if let Some(i) = list
                                .iter()
                                .enumerate()
                                .min_by_key(|(_, (_, seen))| *seen)
                                .map(|(i, _)| i)
                            {
                                let (evicted, _) = list.remove(i);
                                tracing::debug!(host = %host, addr = %evicted, "aaaa evicted (pool full)");
                            }
                        }
                        list.push((*addr, now));
                    }
```

(b) `ResolvedNode` 構築のソート（Task 2 (d) で書いた箇所）を鮮度順に差し替え:

```rust
        // 非 LL 優先はそのまま、同群内は last_seen の新しい順（監査⑤）。現役
        // アドレスは約 30s 周期の再広告で常に最新なので、確立ループ（最初の
        // 成功で早期リターン）は常に現アドレスから試す。
        let mut entries = addrs.clone();
        entries.sort_by_key(|(a, seen)| (is_link_local(a), std::cmp::Reverse(*seen)));
        let addresses: Vec<Ipv6Addr> = entries.into_iter().map(|(a, _)| a).collect();
```

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller dnssd`
Expected: 全 PASS（新 2 本 + Task 1/2 の 3 本 + 既存全数）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/dnssd.rs
git commit -m "fix(mat-controller): AAAA を鮮度順で試行し満杯時は最古 evict（監査⑤ 完結）"
```

---

### Task 4: バージョン 1.19.0 + フルチェック

**Files:**
- Modify: `Cargo.toml`（workspace `version = "1.18.0"` → `"1.19.0"`、L6）
- Modify: `Cargo.lock`（`cargo check` で自動更新）

- [ ] **Step 1: バージョン bump**

`Cargo.toml` L6 を `version = "1.19.0"` に変更し、`cargo check` を実行して `Cargo.lock` を更新。

- [ ] **Step 2: CI 相当のフルチェック**

Run: `task check`
Expected: fmt:check / clippy / test 全通過（clippy は `-D warnings` 相当。`list.retain` 内の `%a` 参照や `min_by_key` の型で警告が出たら修正して再実行）

- [ ] **Step 3: コミット**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: 1.19.0（常駐 mDNS AAAA 鮮度 — 安定性監査 Tier 2 ⑤）"
```

---

### Task 5: 実機回帰スモーク（マージ前ゲート — メインセッションで実施）

**Files:** なし（デプロイ・観測のみ。subagent に委譲せずメインセッションで despliegue / jarvis スキルを使って行う）

ユーザー決定: アドレスローテーションの実機演出はしない（BR prefix 変更はメッシュ全体に影響）。回帰スモークのみ。

- [ ] **Step 1: aarch64 クロスビルド**

Run: `task dist:arm64`
Expected: `matd` の aarch64-musl バイナリ生成（stale 成果物でないことを `file` で確認 — [[aarch64-musl-rust-lld-crossbuild]] の教訓）

- [ ] **Step 2: 隔離 matd スモーク（`*.new` 方式 — 本番未置換のまま検証）**

despliegue スキルの隔離 matd 手順に従い、`matd.new` を jarvis に配置して隔離起動。確認項目:
- 全 13 ノードの購読確立が attempts=1
- `matd status` 健全（SubHealth 全 present）
- journal に WARN 0
- warm read が exit 0

- [ ] **Step 3: 合格なら finishing-a-development-branch スキルで main へマージ**

マージコミットメッセージ例: `Merge fix/tier2-aaaa-freshness: 1.19.0 — 常駐 mDNS AAAA 鮮度（安定性監査 Tier 2 ⑤）`
その後、本番デプロイとメモリ（`mat-stability-audit-backlog` / `jarvis-matd-deploy`）更新はユーザーと確認の上で実施。
