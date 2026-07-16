# Phase 5 M8b: discover native 化 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat discover`（commissionable 探索）と probe 到達性（`discover --probe` / `diag node --deep` 共有の `probe::mdns`）を `MAT_IFACE` opt-in で native 化し、設定時は chip-tool / avahi-browse を一切 spawn しない（0.19.0）。

**Architecture:** `mat-controller::dnssd` の one-shot legacy unicast を PTR 列挙 browse に拡張（`_matterc._udp` commissionable / `_matter._tcp` operational の 2 ラッパー）→ mat 側 `probe.rs` / `commands/discover.rs` に native 分岐を追加。CASE も KVS も不要（UDP socket + ifindex のみ）。spec: `docs/superpowers/specs/2026-07-17-phase5-m8b-discover-native-design.md`。

**Tech Stack:** Rust (workspace: mat-core / mat-controller / mat-native / mat / matd), tokio (current-thread runtime), bash (実機 E2E)。

## Global Constraints

- **作業ブランチ**: `m8b-discover-native`（Task 1 で main から作成、worktree `.claude/worktrees/m8b-discover-native`）。**全タスクの冒頭で `pwd` と `git branch --show-current` を確認する**こと（サブエージェントの shell はメイン repo (main) で始まる罠が既知）。main へのマージは実機 E2E 合格後に別途（この plan の範囲外）。
- **バージョン**: workspace `Cargo.toml` の `version = "0.19.0"`（Task 1 で上げる）。
- **出力 JSON スキーマは完全維持**。既存統合テスト（fake-chip-tool / fake-avahi 含む）は**無改変で全通過**が各タスクの回帰条件（Task 5 の `mat()` ヘルパへの `env_remove("MAT_IFACE")` 追加のみ例外 — 既存テストの隔離強化であり期待値は変えない）。
- **経路選択**: `MAT_IFACE`（`--iface`）設定時のみ native browse。未設定は従来経路（完全無変更）。native browse の **IO エラー**（ifindex 解決失敗・bind/send 失敗等）→ `tracing::warn` + 従来経路フォールバック（read-only op なので二重実行の害なし）。**結果 0 件はエラーではない**（フォールバックしない — 平常時に毎回二重スキャンしないため）。
- **discover は matd プロトコル対象外のまま**（one-shot 直経路のみ。matd 側のコードは触らない — Task 2 の dead API 削除を除く）。
- **browse は window 満了まで収集**（早期 return なし）。window は `dnssd::BROWSE_WINDOW` = 3 秒の定数。新 CLI フラグは追加しない。
- **既知の罠**: `lo` は IFF_MULTICAST 無し = ifindex 1 への multicast 送信はどの環境でも失敗する（統合テストではこれを決定的なフォールバック誘発に使う）。
- コミットは各タスク末尾で行い、メッセージ末尾に `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` を付ける。コミット前に `cargo fmt` を通すこと（最終タスクで `task check`）。

---

### Task 1: ブランチ + worktree 作成、バージョン 0.19.0

**Files:**
- Modify: `Cargo.toml`（workspace version のみ）

**Interfaces:**
- Produces: main から切った `m8b-discover-native` ブランチの worktree。以後の全タスクはこの worktree で行う。

- [ ] **Step 1: worktree とブランチを作る**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat
git status --short   # クリーンであること（M8b spec はコミット済み）
git worktree add .claude/worktrees/m8b-discover-native -b m8b-discover-native main
cd .claude/worktrees/m8b-discover-native
pwd && git branch --show-current   # => m8b-discover-native
```

- [ ] **Step 2: バージョンを 0.19.0 に**

worktree の `Cargo.toml`（workspace ルート）の `version = "0.18.0"` → `version = "0.19.0"`。

- [ ] **Step 3: ビルド確認（Cargo.lock 追従込み）**

```bash
cargo build --workspace 2>&1 | tail -3   # 成功すること
```

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: version 0.19.0 (M8b開始) (M8b Task1)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: dead API 掃除（matd `NativeBackend::ensure_group_acl`）

M8a で matd の provision native 化が `mat_native::ops::provision_node`（内部で
ACL を処理）に一本化された結果、matd 側の個別ラッパーが呼び出しゼロで残って
いる。M8a 完了時の推奨事項（「次のマイルストーンの頭で dead API 掃除」）の実行。

**Files:**
- Modify: `crates/matd/src/native.rs`（`ensure_group_acl` メソッド削除、268 行目付近）

**Interfaces:**
- Consumes: なし
- Produces: なし（挙動変更ゼロ。`mat_native::ops::ensure_group_acl` 本体は
  `ops::provision_node` と `mat::native_direct` が使うので**残す**）

- [ ] **Step 1: 呼び出しゼロを確認**

```bash
grep -rn "ensure_group_acl" crates/matd/src crates/mat/src crates/mat-native/src
```

Expected: `crates/matd/src/native.rs` の定義（2 行）以外に matd 内の呼び出しが
無いこと。`crates/mat/src/native_direct.rs` と `crates/mat-native/src/ops.rs`
のヒットは `mat_native::ops::ensure_group_acl`（別物、残す）。

- [ ] **Step 2: メソッドを削除**

`crates/matd/src/native.rs` から以下のメソッド（doc コメント含む）を削除:

```rust
    pub async fn ensure_group_acl(&self, node_id: u64, group_id: u16) -> Result<bool, MatError> {
        self.with_conn(node_id, |c| {
            Box::pin(mat_native::ops::ensure_group_acl(c.as_mut(), group_id))
        })
        .await
    }
```

（実ファイルの doc コメント行も一緒に消す。前後のメソッドは触らない。）

- [ ] **Step 3: 他に呼び出しゼロの pub API が無いか確認**

```bash
for m in read_onoff "\.on(" "\.off(" "\.color(" color_temp read_json write_tlv invoke_generic "\.describe(" provision_node group_invoke; do
  echo -n "$m: "; grep -rn "$m" crates/matd/src/server.rs crates/matd/src/main.rs crates/matd/src/backend.rs | wc -l
done
```

Expected: 全メソッドに 1 件以上の呼び出し。0 件のものが見つかったら同様に削除
（2026-07-17 時点の調査では `ensure_group_acl` のみが 0 件）。

- [ ] **Step 4: テスト**

```bash
cargo fmt && cargo test -p matd 2>&1 | tail -3
```

Expected: green。

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/native.rs
git commit -m "refactor(matd): M8aで呼び出しゼロになった NativeBackend::ensure_group_acl を削除 (M8b Task2)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: dnssd.rs — browse 畳み込み（純ロジック + 型 + 変換）

browse の心臓部。socket に触れない純ロジックだけを先に TDD で固める。
既存の `parse_message` / `Record` / `RData` / `txt_u32` / `is_link_local` と
テスト用 synth ヘルパを流用する。

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`

**Interfaces:**
- Consumes: 既存の `Record` / `RData`（private）、`txt_u32`、`is_link_local`、
  定数 `TYPE_PTR` / `TYPE_SRV` / `TYPE_TXT` / `TYPE_AAAA`。
- Produces（Task 4 が使う）:
  - `pub struct CommissionableInstance { pub hostname: Option<String>, pub port: Option<u16>, pub addresses: Vec<Ipv6Addr>, pub discriminator: Option<u32>, pub vendor_id: Option<u32>, pub product_id: Option<u32> }`
  - `pub struct OperationalInstance { pub compressed_fabric: String, pub node_id: u64, pub addresses: Vec<Ipv6Addr> }`
  - private: `BrowseFold`（`new(service)` / `fold(&[Record])` / `pending_questions() -> Vec<(String, u16)>` / `finish() -> Vec<FoldedInstance>`）、`FoldedInstance`、`commissionable_from_fold` / `operational_from_fold`。

- [ ] **Step 1: 失敗するテストを書く**

`dnssd.rs` の `mod tests` に追記。まず browse 用 synth ヘルパ（既存
`synth_commissionable_response` と同型だが PTR の name がサービスタイプ
そのもの）:

```rust
    /// browse 用の合成応答: PTR(service→instance) + SRV/TXT/AAAA を 1 メッセージに
    /// 詰める（additional 同梱の行儀良い responder 相当）。`records` で個別に
    /// 抜き差しできるよう、載せるレコード種を引数で選ぶ。
    #[allow(clippy::too_many_arguments)]
    fn synth_browse_response(
        service: &str,
        instance: &str,
        with_srv: Option<(u16, &str)>,
        with_txt: Option<&[&str]>,
        with_aaaa: Option<(&str, Ipv6Addr)>,
    ) -> Vec<u8> {
        let mut msg = Vec::new();
        msg.extend_from_slice(&0u16.to_be_bytes()); // id
        msg.extend_from_slice(&0x8400u16.to_be_bytes()); // QR|AA
        msg.extend_from_slice(&0u16.to_be_bytes()); // qd
        let mut count: u16 = 1; // PTR
        if with_srv.is_some() {
            count += 1;
        }
        if with_txt.is_some() {
            count += 1;
        }
        if with_aaaa.is_some() {
            count += 1;
        }
        msg.extend_from_slice(&count.to_be_bytes()); // an
        msg.extend_from_slice(&[0, 0, 0, 0]); // ns/ar
        // PTR: service -> instance
        push_name(&mut msg, service);
        msg.extend_from_slice(&TYPE_PTR.to_be_bytes());
        msg.extend_from_slice(&CLASS_IN.to_be_bytes());
        msg.extend_from_slice(&[0, 0, 0, 120]);
        let mut ptr_rdata = Vec::new();
        push_name(&mut ptr_rdata, instance);
        msg.extend_from_slice(&(ptr_rdata.len() as u16).to_be_bytes());
        msg.extend_from_slice(&ptr_rdata);
        if let Some((port, target)) = with_srv {
            push_name(&mut msg, instance);
            msg.extend_from_slice(&TYPE_SRV.to_be_bytes());
            msg.extend_from_slice(&CLASS_IN.to_be_bytes());
            msg.extend_from_slice(&[0, 0, 0, 120]);
            let mut rdata = vec![0, 0, 0, 0]; // priority/weight
            rdata.extend_from_slice(&port.to_be_bytes());
            push_name(&mut rdata, target);
            msg.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            msg.extend_from_slice(&rdata);
        }
        if let Some(strings) = with_txt {
            push_name(&mut msg, instance);
            msg.extend_from_slice(&TYPE_TXT.to_be_bytes());
            msg.extend_from_slice(&CLASS_IN.to_be_bytes());
            msg.extend_from_slice(&[0, 0, 0, 120]);
            let mut rdata = Vec::new();
            for s in strings {
                rdata.push(s.len() as u8);
                rdata.extend_from_slice(s.as_bytes());
            }
            msg.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            msg.extend_from_slice(&rdata);
        }
        if let Some((host, addr)) = with_aaaa {
            push_name(&mut msg, host);
            msg.extend_from_slice(&TYPE_AAAA.to_be_bytes());
            msg.extend_from_slice(&CLASS_IN.to_be_bytes());
            msg.extend_from_slice(&[0, 0, 0, 120]);
            msg.extend_from_slice(&16u16.to_be_bytes());
            msg.extend_from_slice(&addr.octets());
        }
        msg
    }
```

テスト本体:

```rust
    const MC: &str = "_matterc._udp.local";
    const MO: &str = "_matter._tcp.local";

    #[test]
    fn browse_fold_collects_two_instances_from_bundled_responses() {
        let a1: Ipv6Addr = "fd00::1".parse().unwrap();
        let a2: Ipv6Addr = "fd00::2".parse().unwrap();
        let d1 = synth_browse_response(
            MC,
            &format!("INST1.{MC}"),
            Some((5540, "h1.local")),
            Some(&["D=3840", "VP=65521+32768"]),
            Some(("h1.local", a1)),
        );
        let d2 = synth_browse_response(
            MC,
            &format!("INST2.{MC}"),
            Some((5541, "h2.local")),
            Some(&["D=100"]),
            Some(("h2.local", a2)),
        );
        let mut fold = BrowseFold::new(MC);
        fold.fold(&parse_message(&d1).unwrap());
        fold.fold(&parse_message(&d2).unwrap());
        let out = fold.finish();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].port, Some(5540));
        assert_eq!(out[0].addresses, vec![a1]);
        assert_eq!(out[1].port, Some(5541));
        assert_eq!(out[1].addresses, vec![a2]);
    }

    #[test]
    fn browse_fold_is_order_independent_within_a_datagram() {
        // SRV/TXT/AAAA が PTR より前に並んでいても畳み込める（fold は 2 パス）。
        // synth は PTR を先頭に置くので、parse 結果を並べ替えて食わせる。
        let a1: Ipv6Addr = "fd00::1".parse().unwrap();
        let d = synth_browse_response(
            MC,
            &format!("INST1.{MC}"),
            Some((5540, "h1.local")),
            Some(&["D=1"]),
            Some(("h1.local", a1)),
        );
        let mut records = parse_message(&d).unwrap();
        records.reverse(); // PTR が最後
        let mut fold = BrowseFold::new(MC);
        fold.fold(&records);
        let out = fold.finish();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].port, Some(5540));
        assert_eq!(out[0].addresses, vec![a1]);
    }

    #[test]
    fn browse_fold_dedupes_instances_and_caps_growth() {
        let mut fold = BrowseFold::new(MC);
        let d = synth_browse_response(MC, &format!("INST1.{MC}"), None, None, None);
        fold.fold(&parse_message(&d).unwrap());
        fold.fold(&parse_message(&d).unwrap()); // 同じ PTR を 2 回
        assert_eq!(fold.instances.len(), 1);
        for i in 0..(MAX_INSTANCES + 5) {
            let d = synth_browse_response(MC, &format!("X{i}.{MC}"), None, None, None);
            fold.fold(&parse_message(&d).unwrap());
        }
        assert_eq!(fold.instances.len(), MAX_INSTANCES);
    }

    #[test]
    fn browse_fold_ignores_records_for_other_services() {
        // 同じ網に有線 LAN プリンタ等がいても混ざらない。
        let mut fold = BrowseFold::new(MC);
        let d = synth_browse_response(
            "_ipp._tcp.local",
            "printer._ipp._tcp.local",
            Some((631, "printer.local")),
            None,
            None,
        );
        fold.fold(&parse_message(&d).unwrap());
        assert!(fold.instances.is_empty());
    }

    #[test]
    fn browse_pending_questions_lists_missing_srv_txt_aaaa() {
        let mut fold = BrowseFold::new(MC);
        // PTR のみ → SRV と TXT を要求。
        let d = synth_browse_response(MC, &format!("INST1.{MC}"), None, None, None);
        fold.fold(&parse_message(&d).unwrap());
        let q = fold.pending_questions();
        assert!(q.contains(&(format!("INST1.{MC}"), TYPE_SRV)));
        assert!(q.contains(&(format!("INST1.{MC}"), TYPE_TXT)));
        // SRV が来たら target の AAAA を要求（プールにまだ無い）。
        let d = synth_browse_response(
            MC,
            &format!("INST1.{MC}"),
            Some((5540, "h1.local")),
            Some(&["D=1"]),
            None,
        );
        fold.fold(&parse_message(&d).unwrap());
        let q = fold.pending_questions();
        assert!(q.contains(&("h1.local".to_string(), TYPE_AAAA)));
        assert!(!q.iter().any(|(_, t)| *t == TYPE_SRV));
    }

    #[test]
    fn commissionable_from_fold_parses_txt_hostname_and_sorts_addresses() {
        let global: Ipv6Addr = "fd00::10".parse().unwrap();
        let ll: Ipv6Addr = "fe80::10".parse().unwrap();
        let f = FoldedInstance {
            name: format!("INST1.{MC}"),
            port: Some(5540),
            target: Some("HOST01.local".to_string()),
            txt: vec![b"D=3840".to_vec(), b"VP=65521+32768".to_vec()],
            addresses: vec![global, ll],
        };
        let c = commissionable_from_fold(&f).unwrap();
        assert_eq!(c.hostname.as_deref(), Some("HOST01"));
        assert_eq!(c.port, Some(5540));
        assert_eq!(c.discriminator, Some(3840));
        assert_eq!(c.vendor_id, Some(65521));
        assert_eq!(c.product_id, Some(32768));
        assert_eq!(c.addresses, vec![global, ll]);
    }

    #[test]
    fn commissionable_from_fold_accepts_vendor_only_vp_and_skips_empty() {
        let f = FoldedInstance {
            name: format!("INST1.{MC}"),
            port: None,
            target: None,
            txt: vec![b"VP=65521".to_vec()],
            addresses: vec![],
        };
        let c = commissionable_from_fold(&f).unwrap();
        assert_eq!(c.vendor_id, Some(65521));
        assert_eq!(c.product_id, None);
        // 素材ゼロ（PTR しか見えなかった instance）は出さない。
        let empty = FoldedInstance {
            name: format!("INST2.{MC}"),
            port: None,
            target: None,
            txt: vec![],
            addresses: vec![],
        };
        assert!(commissionable_from_fold(&empty).is_none());
    }

    #[test]
    fn operational_from_fold_parses_label_and_keeps_announce_only() {
        let f = FoldedInstance {
            name: format!("00AABB1122CC3344-000000000000000B.{MO}"),
            port: Some(5540),
            target: None,
            txt: vec![],
            addresses: vec![],
        };
        let o = operational_from_fold(&f).unwrap();
        assert_eq!(o.compressed_fabric, "00AABB1122CC3344");
        assert_eq!(o.node_id, 0x0B);
        assert!(o.addresses.is_empty()); // announce のみ → 空で返す（skip しない）
    }

    #[test]
    fn operational_from_fold_rejects_malformed_labels() {
        for bad in [
            format!("shortname.{MO}"),
            format!("GGGGBB1122CC3344-000000000000000B.{MO}"), // 非 hex
            format!("00AABB1122CC3344.{MO}"),                  // '-' 無し
            format!("00AABB1122CC3344-0B.{MO}"),               // 桁不足
        ] {
            let f = FoldedInstance {
                name: bad,
                port: None,
                target: None,
                txt: vec![],
                addresses: vec![],
            };
            assert!(operational_from_fold(&f).is_none());
        }
    }
```

- [ ] **Step 2: テストが失敗（コンパイルエラー）することを確認**

```bash
cargo test -p mat-controller dnssd 2>&1 | tail -5
```

Expected: `BrowseFold` 等が未定義でコンパイルエラー。

- [ ] **Step 3: 実装を書く**

`dnssd.rs` の `resolve_commissionable` の後（`mod tests` の前）に追記:

```rust
// ── browse（M8b: discover native 化）───────────────────────────────────

/// browse の収集ウィンドウ。resolve と違い「全員から集める」ため早期 return
/// せず、この時間で打ち切る。
pub const BROWSE_WINDOW: Duration = Duration::from_secs(3);
/// browse が追跡する instance 数の上限（偽装 flood でメモリを伸ばさない —
/// MAX_AAAA と同思想）。
const MAX_INSTANCES: usize = 32;
/// browse 中の AAAA 候補プール上限（instance 横断で共有）。
const MAX_BROWSE_AAAA: usize = 64;
/// フォローアップクエリ 1 メッセージあたりの質問数上限（MTU 超え回避）。
const MAX_QUESTIONS_PER_MSG: usize = 8;

/// `_matterc._udp` で見つかった commissionable 1 台分（TXT パース済み）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommissionableInstance {
    /// SRV target から末尾 `.local` を除いた形。
    pub hostname: Option<String>,
    pub port: Option<u16>,
    /// 非 link-local 優先でソート、dedup 済み。
    pub addresses: Vec<Ipv6Addr>,
    /// TXT `D`（long discriminator）。
    pub discriminator: Option<u32>,
    /// TXT `VP`（`<vendor>+<product>`、product は省略され得る）。
    pub vendor_id: Option<u32>,
    pub product_id: Option<u32>,
}

/// `_matter._tcp` で見つかった operational 1 台分。SRV/AAAA が期限内に揃わなく
/// ても PTR が見えた instance は返す（announce のみ = addresses 空 — 到達性
/// 判定側の「広告あり・アドレス未解決」セマンティクスを保存するため）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationalInstance {
    /// 16 桁大文字 hex。
    pub compressed_fabric: String,
    pub node_id: u64,
    pub addresses: Vec<Ipv6Addr>,
}

/// finish() が返す、サービス種別に依存しない 1 instance 分の素材。
struct FoldedInstance {
    /// instance の完全名（先頭ラベルが instance 名）。
    name: String,
    port: Option<u16>,
    target: Option<String>,
    txt: Vec<Vec<u8>>,
    /// SRV target に一致した AAAA（非 link-local 優先ソート、dedup 済み）。
    addresses: Vec<Ipv6Addr>,
}

#[derive(Default)]
struct InstanceFold {
    srv: Option<(u16, String)>,
    txt: Option<Vec<Vec<u8>>>,
}

/// browse の畳み込み状態。データグラム単位で [`fold`](Self::fold) に食わせ、
/// window 満了後に [`finish`](Self::finish) で取り出す。
struct BrowseFold {
    /// 例 "_matterc._udp.local"（大文字小文字無視で照合）。
    service: String,
    /// key = instance 完全名。到着順・dedup・MAX_INSTANCES で打ち止め。
    instances: Vec<(String, InstanceFold)>,
    /// hostname → アドレスのプール（instance 横断で共有し、finish 時に
    /// SRV target 名で引く）。
    aaaa: Vec<(String, Ipv6Addr)>,
}

impl BrowseFold {
    fn new(service: &str) -> Self {
        BrowseFold {
            service: service.to_string(),
            instances: Vec::new(),
            aaaa: Vec::new(),
        }
    }

    /// 1 データグラム分を畳み込む。PTR を先に全部拾ってから SRV/TXT/AAAA を
    /// 処理する 2 パス（同一データグラム内のレコード順に依存しない）。
    fn fold(&mut self, records: &[Record]) {
        for r in records {
            if let RData::Ptr(inst) = &r.rdata {
                if r.name.eq_ignore_ascii_case(&self.service)
                    && self.instances.len() < MAX_INSTANCES
                    && !self
                        .instances
                        .iter()
                        .any(|(n, _)| n.eq_ignore_ascii_case(inst))
                {
                    self.instances.push((inst.clone(), InstanceFold::default()));
                }
            }
        }
        for r in records {
            match &r.rdata {
                RData::Srv { port, target } => {
                    if let Some((_, f)) = self
                        .instances
                        .iter_mut()
                        .find(|(n, _)| n.eq_ignore_ascii_case(&r.name))
                    {
                        f.srv = Some((*port, target.clone()));
                    }
                }
                RData::Txt(strings) => {
                    if let Some((_, f)) = self
                        .instances
                        .iter_mut()
                        .find(|(n, _)| n.eq_ignore_ascii_case(&r.name))
                    {
                        f.txt = Some(strings.clone());
                    }
                }
                RData::Aaaa(addr) => {
                    if self.aaaa.len() < MAX_BROWSE_AAAA
                        && !self
                            .aaaa
                            .iter()
                            .any(|(n, a)| a == addr && n.eq_ignore_ascii_case(&r.name))
                    {
                        self.aaaa.push((r.name.clone(), *addr));
                    }
                }
                _ => {}
            }
        }
    }

    /// まだ足りない素材へのフォローアップ質問 (name, qtype)。
    fn pending_questions(&self) -> Vec<(String, u16)> {
        let mut out = Vec::new();
        for (name, f) in &self.instances {
            if f.srv.is_none() {
                out.push((name.clone(), TYPE_SRV));
            }
            if f.txt.is_none() {
                out.push((name.clone(), TYPE_TXT));
            }
            if let Some((_, target)) = &f.srv {
                if !self.aaaa.iter().any(|(n, _)| n.eq_ignore_ascii_case(target)) {
                    out.push((target.clone(), TYPE_AAAA));
                }
            }
        }
        out
    }

    fn finish(self) -> Vec<FoldedInstance> {
        let pool = self.aaaa;
        self.instances
            .into_iter()
            .map(|(name, f)| {
                let (port, target) = match f.srv {
                    Some((p, t)) => (Some(p), Some(t)),
                    None => (None, None),
                };
                let mut addresses: Vec<Ipv6Addr> = Vec::new();
                if let Some(t) = &target {
                    for (n, a) in &pool {
                        if n.eq_ignore_ascii_case(t) && !addresses.contains(a) {
                            addresses.push(*a);
                        }
                    }
                    addresses.sort_by_key(is_link_local);
                }
                FoldedInstance {
                    name,
                    port,
                    target,
                    txt: f.txt.unwrap_or_default(),
                    addresses,
                }
            })
            .collect()
    }
}

/// TXT から文字列値（key は大文字小文字無視）を取り出す。
fn txt_str<'a>(strings: &'a [Vec<u8>], key: &str) -> Option<&'a str> {
    for s in strings {
        let Ok(s) = std::str::from_utf8(s) else {
            continue;
        };
        let Some((k, v)) = s.split_once('=') else {
            continue;
        };
        if k.eq_ignore_ascii_case(key) {
            return Some(v);
        }
    }
    None
}

/// TXT `VP`（`<vendor>+<product>`、product 省略可、10 進）を分解する。
fn split_vp(vp: &str) -> (Option<u32>, Option<u32>) {
    match vp.split_once('+') {
        Some((v, p)) => (v.parse().ok(), p.parse().ok()),
        None => (vp.parse().ok(), None),
    }
}

/// SRV target（例 "HOST01.local"）→ hostname（末尾 ".local" を除去）。
fn hostname_from_target(target: &str) -> String {
    target.strip_suffix(".local").unwrap_or(target).to_string()
}

/// instance 完全名の先頭ラベル `<CFID 16hex>-<NodeId 16hex>` をパースする。
/// 形式外は None（他プロトコル / 他サービスの流れ弾）。
fn parse_operational_label(name: &str) -> Option<(String, u64)> {
    let label = name.split('.').next()?;
    let (cfid, node) = label.split_once('-')?;
    if cfid.len() != 16 || node.len() != 16 {
        return None;
    }
    if !cfid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let node_id = u64::from_str_radix(node, 16).ok()?;
    Some((cfid.to_ascii_uppercase(), node_id))
}

/// 畳み込んだ素材 → commissionable。素材ゼロ（PTR しか見えず SRV/TXT/AAAA が
/// 期限内に揃わなかった）は None（chip-tool 経路の空エントリ skip と同じ扱い）。
fn commissionable_from_fold(f: &FoldedInstance) -> Option<CommissionableInstance> {
    let discriminator = txt_u32(&f.txt, "D");
    let (vendor_id, product_id) = txt_str(&f.txt, "VP")
        .map(split_vp)
        .unwrap_or((None, None));
    let c = CommissionableInstance {
        hostname: f.target.as_deref().map(hostname_from_target),
        port: f.port,
        addresses: f.addresses.clone(),
        discriminator,
        vendor_id,
        product_id,
    };
    if c.hostname.is_none()
        && c.port.is_none()
        && c.addresses.is_empty()
        && c.discriminator.is_none()
        && c.vendor_id.is_none()
        && c.product_id.is_none()
    {
        return None;
    }
    Some(c)
}

/// 畳み込んだ素材 → operational。announce のみ（addresses 空）でも返す。
fn operational_from_fold(f: &FoldedInstance) -> Option<OperationalInstance> {
    let (compressed_fabric, node_id) = parse_operational_label(&f.name)?;
    Some(OperationalInstance {
        compressed_fabric,
        node_id,
        addresses: f.addresses.clone(),
    })
}
```

`BROWSE_WINDOW` はこの Task では未参照で dead_code 警告になるため、定数定義は
Task 4 に回してもよい（clippy `-D warnings` を通すこと優先。この Task に含める
場合は `pub` なので lib クレートでは警告にならない — そのまま置いてよい）。

- [ ] **Step 4: テストが通ることを確認**

```bash
cargo fmt && cargo test -p mat-controller dnssd 2>&1 | tail -5
cargo clippy -p mat-controller --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: 全 green（既存 dnssd テスト含む）。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-controller/src/dnssd.rs
git commit -m "feat(mat-controller): dnssd に browse 畳み込み（PTR列挙+SRV/TXT/AAAA fold+commissionable/operational変換） (M8b Task3)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: dnssd.rs — 非同期 browse ループ + 公開 API

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`

**Interfaces:**
- Consumes: Task 3 の `BrowseFold` / `commissionable_from_fold` /
  `operational_from_fold`、既存 `encode_query` / `parse_message`。
- Produces（Task 5 / 6 が使う）:
  - `pub async fn browse_commissionable(scope_id: u32, window: Duration) -> Result<Vec<CommissionableInstance>, DnssdError>`
  - `pub async fn browse_operational(scope_id: u32, window: Duration) -> Result<Vec<OperationalInstance>, DnssdError>`
  - `pub const BROWSE_WINDOW: Duration`（Task 3 で未定義ならここで定義）

- [ ] **Step 1: browse ループを書く**

Task 3 の変換関数群の直前に追記（`resolve_operational` と同じ送受信パターン。
違いは (1) 早期 return せず window 満了まで収集 (2) フォローアップ質問が
複数 instance 分 (3) 受信バッファ 9000）:

```rust
/// One-shot legacy unicast mDNS browse: `service`（例 "_matterc._udp.local"）
/// の PTR を列挙し、instance ごとに SRV/TXT/AAAA を畳み込む。resolve_* と
/// 違い早期 return せず `window` 満了まで収集する（全員から集めるため、
/// 実行時間 = window で固定）。クエリは 1 秒間隔で再送。
async fn browse(
    scope_id: u32,
    service: &str,
    window: Duration,
) -> Result<Vec<FoldedInstance>, DnssdError> {
    let sock = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))
        .await
        .map_err(DnssdError::Io)?;
    let dest = SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, scope_id));
    let mut fold = BrowseFold::new(service);
    let deadline = Instant::now() + window;
    let mut next_send = Instant::now();
    // browse 応答は resolve より大きくなり得る（複数 instance の additional
    // 同梱）ため、受信バッファは mDNS の実質上限まで取る。
    let mut buf = vec![0u8; 9000];
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= next_send {
            let q = encode_query(0, &[(service, TYPE_PTR)]);
            sock.send_to(&q, dest).await.map_err(DnssdError::Io)?;
            let pending = fold.pending_questions();
            for chunk in pending.chunks(MAX_QUESTIONS_PER_MSG) {
                let qs: Vec<(&str, u16)> =
                    chunk.iter().map(|(n, t)| (n.as_str(), *t)).collect();
                let q = encode_query(0, &qs);
                sock.send_to(&q, dest).await.map_err(DnssdError::Io)?;
            }
            next_send = now + QUERY_RESEND_INTERVAL;
        }
        let wait = deadline.min(next_send).saturating_duration_since(now);
        let Ok(recv) = tokio::time::timeout(wait, sock.recv_from(&mut buf)).await else {
            continue;
        };
        let (n, _) = recv.map_err(DnssdError::Io)?;
        // 他人の壊れたデータグラムで browse を中断しない。
        let Ok(records) = parse_message(&buf[..n]) else {
            continue;
        };
        fold.fold(&records);
    }
    Ok(fold.finish())
}

/// `_matterc._udp` の全 commissionable を列挙する（spec §4.3.1）。
/// 0 件は正常（周囲に commissioning モードのデバイスが無い）。
pub async fn browse_commissionable(
    scope_id: u32,
    window: Duration,
) -> Result<Vec<CommissionableInstance>, DnssdError> {
    Ok(browse(scope_id, "_matterc._udp.local", window)
        .await?
        .iter()
        .filter_map(commissionable_from_fold)
        .collect())
}

/// `_matter._tcp` の全 operational instance を列挙する（spec §4.3）。
/// announce のみ（SRV/AAAA 未解決）の instance も addresses 空で含める。
pub async fn browse_operational(
    scope_id: u32,
    window: Duration,
) -> Result<Vec<OperationalInstance>, DnssdError> {
    Ok(browse(scope_id, "_matter._tcp.local", window)
        .await?
        .iter()
        .filter_map(operational_from_fold)
        .collect())
}
```

- [ ] **Step 2: モジュール doc を更新**

`dnssd.rs` 冒頭の doc コメントの `No browsing, no advertising, no cache:` の
段落を実態に合わせる。差し替え:

```rust
//! No advertising, no cache: send a legacy unicast query (source port ≠
//! 5353, so responders reply straight back to us), fold responses until
//! SRV + at least one AAAA for its target are in hand. TXT is folded when
//! it arrives in the same responses but is not waited for — MRP falls back
//! to the spec default interval without it.
//!
//! M8b adds one-shot browse (`browse_commissionable` / `browse_operational`):
//! same legacy unicast transport, but enumerating PTR answers for a whole
//! service type and folding SRV/TXT/AAAA per instance until a fixed window
//! ([`BROWSE_WINDOW`]) expires — no early return, still no cache.
```

- [ ] **Step 3: ビルド + 既存テスト + clippy**

```bash
cargo fmt && cargo test -p mat-controller dnssd 2>&1 | tail -5
cargo clippy -p mat-controller --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: 全 green（ループ本体は socket 実物が要るため実機 E2E で実証 —
既存 `resolve_*` と同じ流儀）。

- [ ] **Step 4: Commit**

```bash
git add crates/mat-controller/src/dnssd.rs
git commit -m "feat(mat-controller): one-shot legacy unicast mDNS browse（commissionable/operational 列挙） (M8b Task4)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: mat probe native 分岐 + iface の配線（diag / discover シグネチャ）

`probe::mdns` に native 分岐を入れ、`--iface`（`MAT_IFACE`）を main.rs から
discover / diag node に通す。commissionable の native 分岐は Task 6。

**Files:**
- Modify: `crates/mat/src/probe.rs`
- Modify: `crates/mat/src/main.rs`（discover / diag node の呼び出し 2 箇所）
- Modify: `crates/mat/src/commands/diag.rs`（`node` シグネチャ + probe 呼び出し）
- Modify: `crates/mat/src/commands/discover.rs`（`run` シグネチャ + probe 呼び出し）
- Modify: `crates/mat/tests/integration.rs`（`mat()` ヘルパ + 新テスト 2 本）

**Interfaces:**
- Consumes: `mat_controller::dnssd::{iface_index, browse_operational, BROWSE_WINDOW, OperationalInstance}`（Task 4）。
- Produces（Task 6 が使う）:
  - `probe::mdns(iface: Option<&str>) -> Result<Vec<MatterInstance>, MatError>`（シグネチャ変更）
  - `commands::discover::run(store_path: &Path, probe: bool, iface: Option<&str>)`（iface を受けるが、この Task では probe にだけ渡す）
  - `commands::diag::node(store_path, node_id, endpoint, deep, iface: Option<&str>)`
  - 実機 E2E が grep する positive marker: `probe executed (native browse)`（info）と `falling back to avahi-browse`（warn）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/tests/integration.rs` — まず既存 `mat()` ヘルパに
`env_remove("MAT_IFACE")` を追加（開発機で `MAT_IFACE` が export されていても
既存テストが native 分岐に入らないようにする隔離強化。期待値は不変）:

```rust
fn mat(store: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_CHIP_TOOL_BIN", fake_chip_tool())
        .env("MAT_MATD", "0")
        .env_remove("MAT_IFACE")
        .arg("--store")
        .arg(store);
    c
}
```

新テスト（`discover --probe` 節の末尾に追記）:

```rust
#[test]
fn discover_probe_native_lo_falls_back_to_avahi() {
    // lo は IFF_MULTICAST 無し = ifindex 1 への multicast 送信はどの環境でも
    // 失敗する（既知）→ native browse が IO エラー → warn + avahi フォール
    // バックで従来どおりの結果になる。
    let store = store_with_node5();
    mat(store.path())
        .env("MAT_IFACE", "lo")
        .env("MAT_LOG", "warn")
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .env("FAKE_AVAHI_ADDR", "192.0.2.99")
        .args(["discover", "--probe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"reachable\":true"))
        .stdout(predicate::str::contains("\"address\":\"192.0.2.99\""))
        .stderr(predicate::str::contains("falling back to avahi-browse"));
}

#[test]
fn diag_deep_native_bogus_iface_falls_back_to_avahi() {
    // 存在しない iface 名 → iface_index が即失敗 → warn + avahi フォール
    // バック（native 分岐のもう一方の入口も決定的に通す）。
    let store = store_with_node5();
    mat(store.path())
        .env("MAT_IFACE", "mat-test-no-such-iface")
        .env("MAT_LOG", "warn")
        .env("MAT_AVAHI_BROWSE_BIN", fake_avahi())
        .args(["diag", "node", "--node", "5", "--deep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"advertised_any_fabric\""))
        .stderr(predicate::str::contains("falling back to avahi-browse"));
}
```

（`diag node --deep` の引数形は既存の deep テストに合わせること — 既存テスト
（`integration.rs` 1400 行目付近）を見て `--node` の位置・追加引数を揃える。）

- [ ] **Step 2: テストが失敗することを確認**

```bash
cargo test -p mat --test integration discover_probe_native 2>&1 | tail -5
```

Expected: FAIL（`mdns()` がまだ iface を見ないため "falling back" が出ない。
シグネチャ変更前はコンパイルは通る — テストだけ落ちる）。

- [ ] **Step 3: probe.rs を書き換える**

```rust
//! mDNS プローブ。`--iface`（`MAT_IFACE`）設定時は native browse
//! （`mat-controller::dnssd`、M8b）、未設定・IO 失敗時は `avahi-browse` に
//! フォールバック。プロセス起動（avahi）と socket I/O を伴うため副作用なしの
//! `mat-core` ではなくバイナリ側に置く。`diag node --deep` と
//! `discover --probe` が共有する。

use std::ffi::OsString;
use std::process::Command as StdCommand;

use mat_core::diag::{parse_avahi_matter, MatterInstance};
use mat_core::error::{ErrorKind, MatError};

/// `_matter._tcp` インスタンスを列挙する。iface 指定時は native browse、
/// IO 失敗は warn + avahi-browse フォールバック（read-only なので二重実行の
/// 害なし）。結果 0 件は正常（フォールバックしない）。
pub fn mdns(iface: Option<&str>) -> Result<Vec<MatterInstance>, MatError> {
    if let Some(iface) = iface {
        match native(iface) {
            Ok(list) => return Ok(list),
            Err(e) => {
                tracing::warn!(
                    iface,
                    error = %e,
                    "native mDNS browse failed; falling back to avahi-browse"
                );
            }
        }
    }
    avahi()
}

/// native browse（M8b）。エラーは呼び出し側が avahi へフォールバックする。
fn native(iface: &str) -> Result<Vec<MatterInstance>, Box<dyn std::error::Error>> {
    let scope_id = mat_controller::dnssd::iface_index(iface)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let list = rt.block_on(mat_controller::dnssd::browse_operational(
        scope_id,
        mat_controller::dnssd::BROWSE_WINDOW,
    ))?;
    tracing::info!(instances = list.len(), "probe executed (native browse)");
    Ok(list.into_iter().map(to_matter_instance).collect())
}

/// browse 結果 → 既存の診断データモデルへの写し。到達性判定
/// （`mat_core::reachability::resolve`）と diag の self-fabric 照合は
/// この型を経由するため無改変で動く。
fn to_matter_instance(o: mat_controller::dnssd::OperationalInstance) -> MatterInstance {
    MatterInstance {
        compressed_fabric: o.compressed_fabric,
        node_id: o.node_id,
        addresses: o.addresses.iter().map(|a| a.to_string()).collect(),
    }
}

/// `avahi-browse -rt _matter._tcp` を実行して `_matter._tcp` インスタンスを得る。
/// バイナリは `MAT_AVAHI_BROWSE_BIN` で上書き可。
fn avahi() -> Result<Vec<MatterInstance>, MatError> {
    let bin =
        std::env::var_os("MAT_AVAHI_BROWSE_BIN").unwrap_or_else(|| OsString::from("avahi-browse"));
    let out = StdCommand::new(&bin)
        .args(["-rt", "_matter._tcp"])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MatError::child_not_found(format!("avahi-browse not found ({bin:?})"))
            } else {
                MatError::new(
                    ErrorKind::Other,
                    format!("avahi-browse spawn failed ({bin:?}): {e}"),
                )
            }
        })?;
    let text = String::from_utf8_lossy(&out.stdout);
    let stderr_text = String::from_utf8_lossy(&out.stderr);
    tracing::debug!(%text, "avahi-browse stdout");
    tracing::debug!(%stderr_text, "avahi-browse stderr");
    Ok(parse_avahi_matter(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_matter_instance_stringifies_addresses() {
        let o = mat_controller::dnssd::OperationalInstance {
            compressed_fabric: "00AABB1122CC3344".to_string(),
            node_id: 5,
            addresses: vec!["fd00::10".parse().unwrap()],
        };
        let m = to_matter_instance(o);
        assert_eq!(m.node_id, 5);
        assert_eq!(m.addresses, vec!["fd00::10".to_string()]);
    }
}
```

- [ ] **Step 4: 呼び出し側を配線する**

`crates/mat/src/commands/diag.rs`:
- `pub fn node(store_path: &Path, node_id: u64, endpoint: u16, deep: bool)` →
  `pub fn node(store_path: &Path, node_id: u64, endpoint: u16, deep: bool, iface: Option<&str>)`
- 中の `crate::probe::mdns()` → `crate::probe::mdns(iface)`

`crates/mat/src/commands/discover.rs`:
- `pub fn run(store_path: &Path, probe: bool)` →
  `pub fn run(store_path: &Path, probe: bool, iface: Option<&str>)`
- 中の `crate::probe::mdns()` → `crate::probe::mdns(iface)`
  （commissionable 側の native 分岐は Task 6）

`crates/mat/src/main.rs`:
- `Command::Discover { probe } => commands::discover::run(&store_path, *probe)` →
  `commands::discover::run(&store_path, *probe, args.iface.as_deref())`
- `DiagCommand::Node { .. } => commands::diag::node(&store_path, node_id.id(), endpoint.id(), *deep)` →
  `commands::diag::node(&store_path, node_id.id(), endpoint.id(), *deep, args.iface.as_deref())`

（`args` がその位置で見えているか確認 — main.rs の command match は `args`
から `command` を借りた後なので、`args.iface` は参照できる。借用エラーが出る
場合は match の前に `let iface = args.iface.clone();` を置いて `iface.as_deref()`
を渡す。）

- [ ] **Step 5: テストが通ることを確認**

```bash
cargo fmt && cargo test -p mat 2>&1 | tail -5
cargo clippy -p mat --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: 新テスト 2 本を含め全 green（既存テストも `env_remove` 追加のみで
無改変全通過）。

- [ ] **Step 6: Commit**

```bash
git add crates/mat/src crates/mat/tests/integration.rs
git commit -m "feat(mat): probe::mdns を MAT_IFACE で native browse 化（diag --deep / discover --probe 共有、IOエラーはavahiへフォールバック） (M8b Task5)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: mat discover — commissionable 探索の native 分岐

**Files:**
- Modify: `crates/mat/src/commands/discover.rs`
- Modify: `crates/mat/src/cli.rs`（`--iface` の doc コメント 1 箇所）
- Modify: `crates/mat/tests/integration.rs`（新テスト 1 本）

**Interfaces:**
- Consumes: `mat_controller::dnssd::{iface_index, browse_commissionable, BROWSE_WINDOW, CommissionableInstance}`（Task 4）、`probe::mdns(iface)`（Task 5）。
- Produces: 実機 E2E が grep する positive marker:
  `discover executed (native browse)`（info）と
  `falling back to chip-tool`（warn、commissionable 探索側）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/tests/integration.rs` の discover 節に追記:

```rust
#[test]
fn discover_native_bogus_iface_falls_back_to_chip_tool() {
    // 存在しない iface 名 → iface_index が即失敗 → warn + chip-tool フォール
    // バックで commissionable が従来どおり出る。
    let store = TempDir::new().unwrap();
    mat(store.path())
        .env("MAT_IFACE", "mat-test-no-such-iface")
        .env("MAT_LOG", "warn")
        .arg("discover")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"commissionable\""))
        .stdout(predicate::str::contains("192.0.2.10"))
        .stderr(predicate::str::contains("falling back to chip-tool"));
}
```

- [ ] **Step 2: テストが失敗することを確認**

```bash
cargo test -p mat --test integration discover_native_bogus 2>&1 | tail -5
```

Expected: FAIL（native 分岐がまだ無く "falling back to chip-tool" が出ない）。

- [ ] **Step 3: discover.rs に native 分岐を書く**

`run()` の commissionable 探索部（`let chip = ChipTool::new(...)` から
`parse_commissionables` まで）を差し替え:

```rust
    // commissionable 探索: iface 指定時は native browse（M8b）、IO 失敗は
    // warn + chip-tool フォールバック（read-only なので二重実行の害なし）。
    // 結果 0 件は正常であり chip-tool に fall back しない（平常時に毎回
    // 二重スキャンしないため）。
    let native = match iface {
        Some(i) => match native_commissionables(i) {
            Ok(list) => Some(list),
            Err(e) => {
                tracing::warn!(
                    iface = i,
                    error = %e,
                    "native commissionable browse failed; falling back to chip-tool"
                );
                None
            }
        },
        None => None,
    };
    let commissionable = match native {
        Some(list) => list,
        None => {
            let chip = ChipTool::new(store.root());
            // chip-tool は探索を時間で打ち切るため非 0 終了もあり得る。exit code で
            // 失敗扱いにせず、得られた行をパースする（child_not_found = exit 12
            // だけは run() がエラーで返す）。
            let out = chip.run(["discover", "commissionables"])?;
            parse_commissionables(&out.stdout)
        }
    };
```

ファイル末尾（`run()` の後）に追加:

```rust
/// native commissionable browse（M8b）→ 既存 `DiscoveredDevice` へ写す
/// （既存 Serialize で出力スキーマ完全一致）。
fn native_commissionables(
    iface: &str,
) -> Result<Vec<DiscoveredDevice>, Box<dyn std::error::Error>> {
    let scope_id = mat_controller::dnssd::iface_index(iface)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let list = rt.block_on(mat_controller::dnssd::browse_commissionable(
        scope_id,
        mat_controller::dnssd::BROWSE_WINDOW,
    ))?;
    tracing::info!(devices = list.len(), "discover executed (native browse)");
    Ok(list.into_iter().map(to_discovered).collect())
}

fn to_discovered(c: mat_controller::dnssd::CommissionableInstance) -> DiscoveredDevice {
    DiscoveredDevice {
        hostname: c.hostname,
        addresses: c.addresses.iter().map(|a| a.to_string()).collect(),
        port: c.port,
        discriminator: c.discriminator,
        vendor_id: c.vendor_id,
        product_id: c.product_id,
    }
}
```

use 追加: `use mat_core::parse::DiscoveredDevice;`（既に
`parse_commissionables` を use している行の隣）。モジュール冒頭の doc コメント
（`//! commissionable は ...`）に 1 行追記:
`//! iface（MAT_IFACE）設定時は commissionable / probe とも native browse（M8b）。`

- [ ] **Step 4: cli.rs の `--iface` doc コメントを更新**

対象 op の列挙文（M7 時点の記述）に discover / probe を反映。既存:

```rust
    /// one-shot 直経路を native（mat-controller 内蔵）で実行する場合の
    /// Thread mesh iface 名（例: eth0）。未設定なら従来どおり chip-tool 直。
    /// 対象 op は on/off/color/color-temp/onoff on-off read と group の
    /// onoff 引数なし on/off/toggle・color・color-temp のみ（他は chip-tool 直）。
    /// matd 稼働中は matd 自動発見が優先される。
```

差し替え:

```rust
    /// one-shot 直経路を native（mat-controller 内蔵）で実行する場合の
    /// Thread mesh iface 名（例: eth0）。未設定なら従来どおり chip-tool 直
    /// （probe は avahi-browse）。対象 op は README の native hotpath 一覧を
    /// 参照（M8a で汎用 read/write/invoke/describe 等、M8b で discover と
    /// mDNS probe に拡大）。matd 稼働中は matd 自動発見が優先される。
```

- [ ] **Step 5: テストが通ることを確認**

```bash
cargo fmt && cargo test -p mat 2>&1 | tail -5
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
```

Expected: 全 green。

- [ ] **Step 6: Commit**

```bash
git add crates/mat/src crates/mat/tests/integration.rs
git commit -m "feat(mat): discover の commissionable 探索を MAT_IFACE で native browse 化（IOエラーはchip-toolへフォールバック） (M8b Task6)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: 実機 E2E ハーネス

**Files:**
- Create: `scripts/e2e-m8b-real.sh`
- Modify: `Taskfile.yml`（`e2e:m8b:real` タスク追加）

**Interfaces:**
- Consumes: 0.19.0 の mat バイナリ、jarvis 実機環境（`MAT_IFACE=eth0` /
  `MAT_FABRIC_INDEX=2`、commissioned ノード群、fabric 無しの玄関ライトが
  `_matterc._udp` を広告中のはず）。
- Produces: 受け入れ基準 1〜4 の検証スクリプト（基準 5 = `task check` は Task 8）。

- [ ] **Step 1: 既存ハーネスの流儀を読む**

`scripts/e2e-m8a-real.sh` を読み、共通の骨格（trap 後始末 / stderr への
PASS/FAIL / positive marker grep + "falling back" 不在の二重チェック /
実ノード ID を環境変数で注入しハードコードしない）を把握する。

- [ ] **Step 2: ハーネスを書く**

`scripts/e2e-m8b-real.sh` — e2e-m8a-real.sh の骨格を流用し、以下を検証
（`MAT_E2E_NODE` 等の必須環境変数、既定 iface は `MAT_E2E_IFACE`（既定 eth0）、
fabric index は `MAT_E2E_FABRIC_INDEX`（既定 2））。**native 実行の実アサー
ションは「外部バイナリを実在させない」方式**: native 検証項目は
`MAT_CHIP_TOOL_BIN=/nonexistent/chip-tool` と
`MAT_AVAHI_BROWSE_BIN=/nonexistent/avahi-browse` を立てて走らせる —
フォールバックが起きれば即 exit 12 / reachable:null になるため、成功 =
純 native の証明になる（marker grep より強い）。加えて positive marker
（`discover executed (native browse)` / `probe executed (native browse)`、
`MAT_LOG=info`）も grep する:

1. **native discover + probe**: `MAT_IFACE=$IFACE mat discover --probe`
   （外部バイナリ無効化付き）が成功し、(a) commissioned 全ノード（
   `MAT_E2E_NODES` のカンマ区切り node_id 群）が `state:commissioned` +
   `reachable:true` + address 非 null、(b) stderr に 2 つの positive marker、
   (c) `falling back` 不在。
2. **commissionable 検出**: 1 の出力に `state:commissionable` のエントリが
   あり `discriminator` を持つ（玄関ライトが広告中のはず。玄関ライトが
   commissioning 広告を止めている場合に備え、0 件時は WARN を出して人力確認
   プロンプト — FAIL にはしない。M5/M7 ハーネスの人力確認と同じ流儀）。
3. **diag --deep native**: `MAT_IFACE=$IFACE mat diag node --node $NODE --deep`
   （外部バイナリ無効化付き）が成功し、`checks.mdns.advertised_any_fabric` が
   true、`advertised_self_fabric` が true（jarvis は自 fabric 広告あり）。
4. **chip-tool 経路との構造一致**: `MAT_IFACE` 無し（実 chip-tool + 実
   avahi-browse）で `mat discover --probe` を走らせ、native 出力と
   `jq` でキー集合を比較（commissioned エントリのキー一致、commissionable
   は件数が揃うことまでは要求しない — chip-tool の探索窓と native の 3 秒窓で
   ヒット差があり得る。両経路とも `reachable:true` の node 集合が一致する
   ことは要求する）。
5. **フォールバック健全性**: `MAT_IFACE=mat-e2e-bogus-iface`（+ 実 chip-tool /
   実 avahi）で `mat discover --probe` が成功し、stderr に
   `falling back to chip-tool` と `falling back to avahi-browse` の両方が
   出て、出力 JSON は 4 の chip-tool 経路と同じキー構造。

- [ ] **Step 3: Taskfile 配線 + ローカル dry-run**

`Taskfile.yml` の `e2e:m8a:real:` の直後に追加:

```yaml
  e2e:m8b:real:
    desc: "M8b実機E2E (discover native化; 要 jarvis + MAT_E2E_NODES)"
    cmds:
      - bash scripts/e2e-m8b-real.sh
```

（desc の文言・インデントは既存 `e2e:m8a:real` エントリに合わせる。）
ローカルでは実機が無いので `bash -n scripts/e2e-m8b-real.sh`（構文チェック）
と、必須環境変数（`MAT_E2E_NODES` 等）未設定時に即 FAIL するガードの動作
確認まで。

- [ ] **Step 4: Commit**

```bash
git add scripts/e2e-m8b-real.sh Taskfile.yml
git commit -m "test(e2e): M8b実機ハーネス（native discover/probe + diag --deep + 構造一致 + フォールバック） (M8b Task7)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: ドキュメント + 最終確認

**Files:**
- Modify: `ARCHITECTURE.md`（M8b の bullet に実装済み内容を追記）
- Modify: `README.md`（native 対象 op 一覧に discover / probe を追記）
- Modify: `CLAUDE.md`（Backend 節の native hotpath 記述に discover / probe を 1 行反映）

- [ ] **Step 1: ARCHITECTURE.md 追記**

Phase 5 節の `- **M8b（0.19.0）— discover native 化**: ...` bullet の末尾に
実装済み内容を追記: dnssd browse（one-shot legacy unicast の PTR 列挙、
window 3 秒、早期 return なし、announce のみは addresses 空で保持）、
`probe::mdns` の native 分岐（`discover --probe` と `diag node --deep` の
両方が対象）、commissionable 探索の native 分岐、フォールバック規則
（IO エラーのみフォールバック / 0 件は正常でフォールバックしない）、
matd 対象外のまま、dead API 掃除（matd `ensure_group_acl` 削除）実施。
**実機 E2E は未実施なので「実機 E2E は別途実施後に追記」と明記**する。

- [ ] **Step 2: README.md 更新**

native 直経路の対象 op 一覧（M8a で拡大した節）に `discover`（commissionable
browse）と mDNS probe（`discover --probe` / `diag node --deep`）を追記。
フォールバック規則の記述に「discover / probe は IO エラー時のみ chip-tool /
avahi-browse にフォールバック、探索 0 件は正常（フォールバックしない）」を
1 文追加。

- [ ] **Step 3: CLAUDE.md 更新**

Backend 節の native hotpath 列挙（`describe`/`diag thread`/`open-window`/
`group provision`/`group grant`/`group invoke` の並び）に
`discover`/mDNS probe を追加し、「M8b で discover native 化」と分かる形に
1〜2 行で更新（大改稿しない）。

- [ ] **Step 4: 最終チェック**

```bash
task check 2>&1 | tail -5   # fmt:check + clippy + 全テスト
```

Expected: all green。落ちたら直してから次へ。

- [ ] **Step 5: Commit**

```bash
git add ARCHITECTURE.md README.md CLAUDE.md
git commit -m "docs: M8b（discover native化）の実装内容をARCHITECTURE/README/CLAUDE.mdに反映 (M8b Task8)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## 完了後（plan の範囲外、ユーザーと実施）

1. jarvis での `task e2e:m8b:real` 実行（受け入れ基準 1〜4 の実機確認。
   aarch64-musl クロスビルド + scp デプロイは既存手順 — memory 参照）。
2. 合格後: `m8b-discover-native` → `main` へ `--no-ff` マージ、
   ARCHITECTURE.md に E2E 結果追記。
3. 本番 jarvis の mat/matd を 0.19.0 へ更新（M7/M8a と同じデプロイ手順）。
