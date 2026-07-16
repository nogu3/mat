# Phase 5 M8c-1: commission native 化 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `MAT_IFACE` 設定時、`mat commission` が chip-tool を spawn せず native（M6a on-network / M6b BLE+Thread）で完走する（0.20.0）。

**Architecture:** `kvs.rs` に epoch IPK 読み出しを追加 → `CommissioningFabric::from_materials`（既存 fabric への commission を可能にする）→ `mat-native::commission` ラッパー（発見・経路選択・ErrorKind 写像）→ mat 側 `commands/commission.rs` の native 分岐、の4層。BLE は feature `ble` を mat → mat-native → mat-controller に貫通。spec: `docs/superpowers/specs/2026-07-17-phase5-m8c1-commission-native-design.md`。

**Tech Stack:** Rust (workspace)、bluer (feature ble、cross gnu ビルド)、bash（実機 E2E）。

## Global Constraints

- **着手前提: M8b の実機 E2E 合格 + main マージ済みであること**（Task 1 で `git log main` に M8b マージが存在することを確認。無ければ全作業を中止しユーザーへ）。
- **作業ブランチ**: `m8c1-commission-native`（Task 1 で main から作成、worktree `.claude/worktrees/m8c1-commission-native`）。**全タスクの冒頭で `pwd` と `git branch --show-current` を確認**（サブエージェントの shell はメイン repo (main) で始まる罠が既知）。
- **バージョン**: workspace `Cargo.toml` の `version = "0.20.0"`（Task 1）。
- **出力 JSON・台帳・alias の挙動は現行と完全同一**。`MAT_IFACE` 未設定は完全無変更。既存統合テスト（fake-chip-tool）は無改変で全通過。
- **フォールバック規則（spec 案A）**: chip-tool へフォールバックするのは**ワイヤ未接触の失敗のみ** = ①資材/エンジン構築失敗（KVS 不備等）②デバイス未発見（mDNS 空振り + BLE 不成立）。**PASE 開始後（PASE/attestation/NOC/Thread 参加/CASE の失敗）は即エラー、chip-tool 再試行しない**（二重 commission 回避）。
- **経路選択は自動 mDNS→BLE**。BLE 経路は feature `ble` ビルド + `--thread-dataset` 指定時のみ。**manual code（short discriminator 4bit のみ）では BLE 経路を使わない**（BLE scan は 12bit 完全一致 — manual code で BLE が必要なら QR を使う旨の明確なエラー）。manual code の on-network は M8b の `browse_commissionable` 全列挙 + `(D >> 8) == short` フィルタで解決（0件=未発見、2件以上=曖昧エラー）。
- **マーカーログ**（E2E が verbatim で grep）: `commission executed (native on-network)` / `commission executed (native ble-thread)`（info）、`falling back to chip-tool`（warn）。
- **KVS への書込は一切しない**（M8c-1 スコープ外。読みだけ）。
- リポジトリは公開: 実ノード ID・実証明書・実 dataset をコミットしない。
- コミットは各タスク末尾、メッセージ末尾に `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。コミット前 `cargo fmt`、最終タスクで `task check`。

---

### Task 1: 前提確認 + ブランチ + バージョン 0.20.0

**Files:**
- Modify: `Cargo.toml`（workspace version のみ）

**Interfaces:**
- Produces: main から切った `m8c1-commission-native` の worktree。以後の全タスクはここで作業。

- [ ] **Step 1: M8b マージ済みの確認（着手ゲート）**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat
git fetch origin 2>/dev/null; git log --oneline main | head -20
```

`M8b` を含むマージコミット（または `discover native` 系コミット群が main に
入っていること）を確認。**無ければここで中止し、コントローラへ「M8b が main
に未マージ」と報告する。**

- [ ] **Step 2: worktree とブランチを作る**

```bash
git worktree add .claude/worktrees/m8c1-commission-native -b m8c1-commission-native main
cd .claude/worktrees/m8c1-commission-native
pwd && git branch --show-current   # => m8c1-commission-native
```

- [ ] **Step 3: バージョン**

`Cargo.toml` の `version = "0.19.0"` → `version = "0.20.0"`。

- [ ] **Step 4: ビルド確認 + Commit**

```bash
cargo build --workspace 2>&1 | tail -3
git add Cargo.toml Cargo.lock
git commit -m "chore: version 0.20.0 (M8c-1開始) (M8c-1 Task1)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: kvs.rs — epoch IPK の所在調査と読み出し

**最重要・最リスクのタスク。** 既存 fabric への commission は AddNOC で
**epoch IPK** をデバイスに渡す必要がある（デバイスが自分で operational を
KDF 導出する）。現行 `kvs.rs` は `f/<idx>/k/0` keyset blob の第1エントリ鍵を
**operational** として読んでいる（M5 実機実証: その鍵 + ctx5 の GKH で
groupcast が通る = 永続鍵は operational）。しかし chip-tool 自身が同じ KVS で
commission できている以上、**epoch はどこかに永続されているはず**。

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`
- （調査用参照のみ）connectedhomeip v1.4.2.0 の
  `src/credentials/GroupDataProviderImpl.cpp`（`KeySetData` の
  Serialize/Deserialize と storage key の組み立て）、
  `src/lib/support/DefaultStorageKeyAllocator.h`（`FabricKeyset` 等の
  storage key 書式）

**Interfaces:**
- Consumes: 既存 `parse_keyset_first_entry` / `read_self_issue_materials` /
  `fabric::derive_ipk_operational(epoch, cfid)`。
- Produces（Task 3 が使う）: `SelfIssueMaterials` に **`pub ipk_epoch:
  [u8; 16]`** を追加し `read_self_issue_materials` が埋める。

- [ ] **Step 1: 上流の永続形式を確定する**

connectedhomeip **v1.4.2.0 タグ**のソースで以下を読む（ローカル checkout が
無ければ GitHub の raw URL を WebFetch/curl で取得。ネットワーク不可なら
`scripts/gen-ids.py` が使った checkout の残骸を探す）:

1. `DefaultStorageKeyAllocator.h` — `FabricKeyset(fabric, keyset)` が
   `f/%x/k/%x` であること、他に keyset を持つ storage key が無いか。
2. `GroupDataProviderImpl.cpp` — `KeySetData::Save/Load` が TLV で何を
   書くか。**確認ポイント: 永続されるのは epoch キーそのものか、導出済み
   operational か、両方か**。（既存 `parse_keyset_first_entry` は
   ctx tag 5 = hash / ctx tag 6 = key を読んでいる — このタグ割当を上流の
   `TagKey()`/`TagKeyHash()` 等と突き合わせ、**start_time など読んでいない
   フィールドに epoch が入っていないか**を見る。）

結論を `kvs.rs` の doc コメントに引用付きで固定する（後続が再調査しなくて
よいように、上流ファイル名・タグ番号を明記）。

- [ ] **Step 2: 失敗するテストを書く**

判明した形式に合わせ、合成 blob でのユニットテストを書く。**自己整合
チェックを必ず含める**: 合成 blob に epoch と operational の両方が入る形式
なら、`derive_ipk_operational(epoch, cfid) == operational` が成り立つ
フィクスチャを組み、リーダが取り出した `ipk_epoch` からの導出が
`ipk_operational` と一致することを assert（タグの取り違えを構造的に検出
する）。

```rust
    #[test]
    fn self_issue_materials_expose_ipk_epoch_consistent_with_operational() {
        // 合成 keyset blob（Step 1 で確定した上流形式に従う）から epoch を
        // 読み出し、operational との KDF 整合を確認する。cfid は
        // フィクスチャの root 公開鍵 + fabric id から計算する。
        // （具体的な blob バイト列は Step 1 の確定形式で組む — 既存
        // parse_keyset 系テストのフィクスチャ構築ヘルパを流用）
        let m = read_self_issue_materials(&alpha_path, &main_path, 1, 0).unwrap();
        let cfid = fabric::compressed_fabric_id(&root_public_key, m.fabric_id);
        assert_eq!(
            fabric::derive_ipk_operational(&m.ipk_epoch, &cfid),
            m.ipk_operational
        );
    }
```

Run: `cargo test -p mat-controller kvs 2>&1 | tail -5` → FAIL（`ipk_epoch`
フィールド未定義）。

- [ ] **Step 3: 実装**

`SelfIssueMaterials` に `pub ipk_epoch: [u8; 16]` を追加し、
`read_self_issue_materials` で Step 1 の形式に従って抽出する（`Debug` impl
は `[REDACTED]` — 既存の秘匿方針に従う）。既存の `ipk_operational` の読みは
**変更しない**（CASE 経路の回帰ゼロ）。

**エスカレーション条件**: Step 1 の調査で「KVS に epoch が存在しない」と
確定した場合は実装せず **BLOCKED で報告**（既存 fabric への native
commission の設計前提が崩れるため、コントローラ/ユーザーの設計判断が要る）。

- [ ] **Step 4: テスト + Commit**

```bash
cargo fmt && cargo test -p mat-controller kvs 2>&1 | tail -5
cargo clippy -p mat-controller --all-targets -- -D warnings 2>&1 | tail -3
git add crates/mat-controller/src/kvs.rs
git commit -m "feat(mat-controller): kvs から epoch IPK を読み出す（既存fabricへのAddNOCに必須） (M8c-1 Task2)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: `CommissioningFabric::from_materials`

**Files:**
- Modify: `crates/mat-controller/src/commissioning.rs`

**Interfaces:**
- Consumes: `SelfIssueMaterials`（Task 2 で `ipk_epoch` 追加済み）。
- Produces（Task 4 が使う）:
  `impl CommissioningFabric { pub fn from_materials(m: kvs::SelfIssueMaterials) -> Self }`

- [ ] **Step 1: 失敗するテストを書く**

`commissioning.rs` の `mod tests` に追記:

```rust
    #[test]
    fn commissioning_fabric_from_materials_maps_fields() {
        let m = crate::kvs::SelfIssueMaterials {
            rcac: vec![0x15, 0x01, 0x02],
            root_private_key: [7u8; 32],
            ipk_operational: [8u8; 16],
            ipk_epoch: [9u8; 16],
            node_id: 0xAA55,
            fabric_id: 0xFAB2,
        };
        let f = CommissioningFabric::from_materials(m);
        assert_eq!(f.rcac_tlv, vec![0x15, 0x01, 0x02]);
        assert_eq!(f.fabric_id, 0xFAB2);
        assert_eq!(f.ipk_epoch, [9u8; 16]);
        assert_eq!(f.admin_node_id, 0xAA55);
    }
```

（`SelfIssueMaterials` のフィールドが非 pub で直接構築できない場合は
`kvs.rs` に `#[cfg(test)]` コンストラクタを足すのではなく、フィールドを
確認して pub で構築する — 現行定義は全フィールド pub。）

Run: `cargo test -p mat-controller commissioning_fabric_from 2>&1 | tail -3`
→ FAIL（メソッド未定義）。

- [ ] **Step 2: 実装**

`impl CommissioningFabric` に追加（`generate` の隣）:

```rust
    /// chip-tool KVS の自己発行資材から、**既存 fabric** 上で commissioning
    /// するための CommissioningFabric を組む。`generate`（新規 fabric）と
    /// 対になる読み込み側。AddNOC でデバイスへ渡す IPK は epoch 側
    /// （`m.ipk_epoch`）— operational を渡すとデバイス側の KDF 導出が
    /// 二重になり CASE が壊れる。
    pub fn from_materials(m: crate::kvs::SelfIssueMaterials) -> Self {
        Self {
            rcac_tlv: m.rcac,
            root_private_key: m.root_private_key,
            fabric_id: m.fabric_id,
            ipk_epoch: m.ipk_epoch,
            admin_node_id: m.node_id,
        }
    }
```

- [ ] **Step 3: テスト + Commit**

```bash
cargo fmt && cargo test -p mat-controller 2>&1 | tail -3
cargo clippy -p mat-controller --all-targets -- -D warnings 2>&1 | tail -3
git add crates/mat-controller/src/commissioning.rs
git commit -m "feat(mat-controller): CommissioningFabric::from_materials（既存fabricでのcommission） (M8c-1 Task3)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: `mat-native::commission` ラッパー + feature `ble` 貫通

**Files:**
- Create: `crates/mat-native/src/commission.rs`
- Modify: `crates/mat-native/src/lib.rs`（`pub mod commission;` 追加）
- Modify: `crates/mat-native/Cargo.toml`（`[features] ble = ["mat-controller/ble"]`）
- Modify: `crates/mat-controller/src/commissioning.rs`（doc 表の ErrorKind 写像追従 — コード変更なし、コメントのみ）

**Interfaces:**
- Consumes: Task 2/3 の `ipk_epoch` / `from_materials`、既存
  `commission_on_network` / `commission_ble_thread`（feature ble）/
  `setup_code::{parse_qr, parse_manual_code}` /
  `dnssd::{iface_index, resolve_commissionable, browse_commissionable, BROWSE_WINDOW}` /
  `transport::UdpTransport` / `NativeConfig`。
- Produces（Task 5 が使う）:

```rust
pub struct CommissionRequest {
    pub setup_code: String,
    pub device_node_id: u64,
    /// hex デコード済みの Thread active operational dataset（BLE 経路用）。
    pub thread_dataset: Option<Vec<u8>>,
    pub paa_dir: Option<std::path::PathBuf>,
    pub cd_signer_dir: Option<std::path::PathBuf>,
}

pub enum CommissionAttempt {
    /// native で完了（成功）。
    Done,
    /// native では引き受けられない（資材構築失敗 / デバイス未発見 =
    /// ワイヤ未接触）。理由付き — 呼び出し側は warn を出して chip-tool へ
    /// フォールバックする。
    Unavailable(String),
}

/// PASE 開始後の失敗は Err（ErrorKind 写像済み — フォールバック禁止）。
pub async fn commission(
    cfg: &NativeConfig,
    req: &CommissionRequest,
) -> Result<CommissionAttempt, MatError>
```

- [ ] **Step 1: 失敗するテストを書く**

`commission.rs` 内 `mod tests` に純ロジックのテストを書く（socket 不要部分）:

```rust
    use super::*;

    #[test]
    fn error_kind_mapping_follows_spec() {
        use mat_controller::commissioning::CommissionError as E;
        use mat_core::error::ErrorKind;
        assert_eq!(kind_of(&E::Timeout("pase")), ErrorKind::Timeout);
        assert_eq!(
            kind_of(&E::CommandStatus { step: "add-noc", code: 1 }),
            ErrorKind::DeviceRejected
        );
        assert_eq!(
            kind_of(&E::Malformed { step: "csr", detail: "bad tlv" }),
            ErrorKind::ParseError
        );
        assert_eq!(
            kind_of(&E::NetworkConfig { step: "connect-network", status: 1, debug_text: None }),
            ErrorKind::Unreachable
        );
        // attestation 失敗 = デバイス拒否相当（純正でないデバイス等）。
        // 具体 variant は Attestation(_) — テストでは構築できる最小の値を使う。
    }

    #[test]
    fn manual_code_short_filter_selects_unique_match() {
        let list = vec![
            fake_commissionable(0x0800), // short 8
            fake_commissionable(0x0F00), // short 15
        ];
        assert_eq!(pick_by_short(&list, 8).unwrap().discriminator, Some(0x0800));
        assert!(pick_by_short(&list, 1).is_none()); // 0 件
    }

    #[test]
    fn manual_code_short_filter_rejects_ambiguous() {
        let list = vec![fake_commissionable(0x0801), fake_commissionable(0x08FF)];
        // 同一 short (8) が 2 台 → 曖昧。Err 側で報告する設計。
        assert!(matches!(pick_by_short_strict(&list, 8), Err(_)));
    }

    fn fake_commissionable(d: u32) -> mat_controller::dnssd::CommissionableInstance {
        mat_controller::dnssd::CommissionableInstance {
            hostname: None,
            port: Some(5540),
            addresses: vec!["fd00::1".parse().unwrap()],
            discriminator: Some(d),
            vendor_id: None,
            product_id: None,
        }
    }
```

（`pick_by_short` = 0/1 件用、`pick_by_short_strict` = 1 件を要求し 2 件以上
は `Err(MatError)`（`commission_failed`, "ambiguous short discriminator"）。
実装で名前を1つに統合してよいが、テストの意味 — 0件は None / 1件は Some /
2件以上はエラー — は保つこと。）

Run: `cargo test -p mat-native commission 2>&1 | tail -3` → FAIL。

- [ ] **Step 2: 実装（commission.rs 本体）**

```rust
//! native commissioning（M8c-1）。setup code のパース・発見・経路選択
//! （mDNS → BLE）・ErrorKind 写像を担う薄いラッパー。プロトコル本体は
//! mat-controller（M6a `commission_on_network` / M6b `commission_ble_thread`）。
//!
//! フォールバック境界（spec 案A）: ワイヤ未接触（資材構築失敗・デバイス
//! 未発見）だけが `Unavailable` = chip-tool へフォールバック可。PASE 開始後
//! の失敗は Err — chip-tool での自動再実行は二重 commission を招くため
//! 呼び出し側でもフォールバックしないこと。

use std::sync::Arc;

use mat_controller::commissioning::{
    self, CommissionError, CommissionParams, CommissionTarget, CommissioningFabric,
};
use mat_controller::dnssd;
use mat_controller::kvs;
use mat_controller::setup_code;
use mat_controller::transport::UdpTransport;
use mat_core::error::{ErrorKind, MatError};

use crate::NativeConfig;

pub struct CommissionRequest {
    pub setup_code: String,
    pub device_node_id: u64,
    pub thread_dataset: Option<Vec<u8>>,
    pub paa_dir: Option<std::path::PathBuf>,
    pub cd_signer_dir: Option<std::path::PathBuf>,
}

pub enum CommissionAttempt {
    Done,
    Unavailable(String),
}

/// setup code をパースした発見キー。QR は 12bit long、manual は 4bit short。
enum Code {
    Qr { passcode: u32, long: u16 },
    Manual { passcode: u32, short: u8 },
}

fn parse_code(s: &str) -> Result<Code, MatError> {
    if s.starts_with("MT:") {
        let p = setup_code::parse_qr(s).map_err(|e| {
            MatError::new(ErrorKind::Other, format!("invalid QR payload: {e}"))
        })?;
        Ok(Code::Qr { passcode: p.passcode, long: p.discriminator })
    } else {
        let m = setup_code::parse_manual_code(s).map_err(|e| {
            MatError::new(ErrorKind::Other, format!("invalid manual code: {e}"))
        })?;
        Ok(Code::Manual { passcode: m.passcode, short: m.short_discriminator })
    }
}

/// `CommissionError` → mat の `ErrorKind`（spec の写像。発見の空振り =
/// `Discovery` はここに来ない — 呼び出し側で Unavailable に落とす）。
fn kind_of(e: &CommissionError) -> ErrorKind {
    match e {
        CommissionError::Timeout(_) => ErrorKind::Timeout,
        CommissionError::Attestation(_) => ErrorKind::DeviceRejected,
        CommissionError::Noc(_) | CommissionError::CommandStatus { .. } => {
            ErrorKind::DeviceRejected
        }
        CommissionError::NetworkConfig { .. } => ErrorKind::Unreachable,
        CommissionError::Malformed { .. } | CommissionError::Csr(_) => ErrorKind::ParseError,
        _ => ErrorKind::CommissionFailed,
    }
}

fn commission_error(e: CommissionError) -> MatError {
    MatError::new(kind_of(&e), format!("native commissioning failed: {e}"))
}

/// manual code の short discriminator で commissionable 一覧から一意に選ぶ。
/// 0 件 = Ok(None)（未発見 → フォールバック）、2 件以上 = Err（曖昧 —
/// chip-tool でも同じ曖昧さなのでフォールバックしない）。
fn pick_by_short_strict(
    list: &[dnssd::CommissionableInstance],
    short: u8,
) -> Result<Option<&dnssd::CommissionableInstance>, MatError> {
    let mut it = list
        .iter()
        .filter(|c| c.discriminator.is_some_and(|d| (d >> 8) as u8 == short));
    let first = it.next();
    if it.next().is_some() {
        return Err(MatError::new(
            ErrorKind::CommissionFailed,
            format!("ambiguous short discriminator {short}: multiple commissionable devices"),
        ));
    }
    Ok(first)
}

pub async fn commission(
    cfg: &NativeConfig,
    req: &CommissionRequest,
) -> Result<CommissionAttempt, MatError> {
    let code = parse_code(&req.setup_code)?;

    // 資材構築（未接触 — 失敗は Unavailable = フォールバック可）。
    let scope_id = match dnssd::iface_index(&cfg.iface) {
        Ok(s) => s,
        Err(e) => return Ok(CommissionAttempt::Unavailable(format!("iface: {e}"))),
    };
    let alpha = cfg.store.join("chip_tool_config.alpha.ini");
    let main_ini = cfg.store.join("chip_tool_config.ini");
    let materials = match kvs::read_self_issue_materials(
        &alpha,
        &main_ini,
        cfg.fabric_index,
        cfg.issuer_index,
    ) {
        Ok(m) => m,
        Err(e) => return Ok(CommissionAttempt::Unavailable(format!("kvs: {e}"))),
    };
    let fabric = CommissioningFabric::from_materials(materials);

    // 発見と経路選択（mDNS → BLE）。
    let (passcode, target) = match code {
        Code::Qr { passcode, long } => {
            match dnssd::resolve_commissionable(
                scope_id,
                long,
                std::time::Duration::from_secs(5),
            )
            .await
            {
                Ok(_) => (passcode, CommissionTarget::Discriminator(long)),
                Err(dnssd::DnssdError::Timeout { .. }) => {
                    // mDNS に居ない → BLE を試す（ble ビルド + dataset 必須）。
                    return ble_path(&fabric, req, passcode, long, scope_id).await;
                }
                Err(e) => return Ok(CommissionAttempt::Unavailable(format!("mdns: {e}"))),
            }
        }
        Code::Manual { passcode, short } => {
            let list = match dnssd::browse_commissionable(scope_id, dnssd::BROWSE_WINDOW).await
            {
                Ok(l) => l,
                Err(e) => return Ok(CommissionAttempt::Unavailable(format!("mdns: {e}"))),
            };
            match pick_by_short_strict(&list, short)? {
                Some(c) => {
                    let Some(addr) = c.addresses.first() else {
                        return Ok(CommissionAttempt::Unavailable(
                            "commissionable found but no address resolved".into(),
                        ));
                    };
                    let port = c.port.unwrap_or(5540);
                    let scope = if (addr.segments()[0] & 0xffc0) == 0xfe80 {
                        scope_id
                    } else {
                        0
                    };
                    (
                        passcode,
                        CommissionTarget::Addr(std::net::SocketAddr::V6(
                            std::net::SocketAddrV6::new(*addr, port, 0, scope),
                        )),
                    )
                }
                // manual code は BLE 経路なし（scan は 12bit 完全一致 —
                // BLE で commission したい場合は QR を使う）。
                None => {
                    return Ok(CommissionAttempt::Unavailable(
                        "not found via mDNS (manual code cannot use BLE; use the QR payload)"
                            .into(),
                    ))
                }
            }
        }
    };

    // on-network 実行（ここから先はワイヤ接触 — 失敗は Err）。
    let transport = Arc::new(
        UdpTransport::bind()
            .await
            .map_err(|e| MatError::new(ErrorKind::Other, format!("udp bind: {e}")))?,
    );
    let dev = commissioning::commission_on_network(
        transport,
        &fabric,
        CommissionParams {
            passcode,
            target,
            device_node_id: req.device_node_id,
            paa_dir: req.paa_dir.as_deref(),
            cd_signer_dir: req.cd_signer_dir.as_deref(),
            scope_id,
        },
    )
    .await
    .map_err(|e| match e {
        // 発見段階（resolve）内での空振りは未接触 — ただし上で事前 resolve
        // 済みのため実際にはほぼ来ない。来たら安全側（フォールバック）。
        CommissionError::Discovery(_) => MatError::new(
            ErrorKind::Unreachable,
            "commissionable disappeared between discovery and PASE".to_string(),
        ),
        other => commission_error(other),
    })?;
    tracing::info!(
        node_id = dev.node_id,
        fabric_index = ?dev.fabric_index,
        "commission executed (native on-network)"
    );
    Ok(CommissionAttempt::Done)
}

#[cfg(feature = "ble")]
async fn ble_path(
    fabric: &CommissioningFabric,
    req: &CommissionRequest,
    passcode: u32,
    long: u16,
    scope_id: u32,
) -> Result<CommissionAttempt, MatError> {
    let Some(dataset) = req.thread_dataset.as_deref() else {
        return Ok(CommissionAttempt::Unavailable(
            "not found via mDNS and no --thread-dataset for the BLE path".into(),
        ));
    };
    let dev = commissioning::commission_ble_thread(
        fabric,
        mat_controller::commissioning::BleThreadParams {
            passcode,
            discriminator: long,
            thread_dataset: dataset,
            device_node_id: req.device_node_id,
            paa_dir: req.paa_dir.as_deref(),
            cd_signer_dir: req.cd_signer_dir.as_deref(),
            scope_id,
        },
    )
    .await
    .map_err(|e| match e {
        // BLE scan の空振り（デバイスが見えない）はワイヤ未接触。
        CommissionError::Ble { step: "scan", detail } => {
            return_unavailable_marker(detail)
        }
        other => commission_error(other),
    });
    match dev {
        Ok(d) => {
            tracing::info!(
                node_id = d.node_id,
                fabric_index = ?d.fabric_index,
                "commission executed (native ble-thread)"
            );
            Ok(CommissionAttempt::Done)
        }
        Err(e) if e.detail.starts_with(UNAVAILABLE_MARKER) => Ok(CommissionAttempt::Unavailable(
            e.detail.trim_start_matches(UNAVAILABLE_MARKER).to_string(),
        )),
        Err(e) => Err(e),
    }
}

#[cfg(not(feature = "ble"))]
async fn ble_path(
    _fabric: &CommissioningFabric,
    _req: &CommissionRequest,
    _passcode: u32,
    _long: u16,
    _scope_id: u32,
) -> Result<CommissionAttempt, MatError> {
    Ok(CommissionAttempt::Unavailable(
        "not found via mDNS; this build has no BLE support (feature \"ble\")".into(),
    ))
}

/// BLE scan 空振りを Err 経路に一旦載せて上で Unavailable に戻すための
/// 内部マーカー。`MatError` に variant を増やさず境界を 1 箇所に保つ。
const UNAVAILABLE_MARKER: &str = "\u{1}unavailable:";

#[cfg(feature = "ble")]
fn return_unavailable_marker(detail: String) -> MatError {
    MatError::new(
        ErrorKind::Other,
        format!("{UNAVAILABLE_MARKER}ble scan: {detail}"),
    )
}
```

実装上の注意:
- `commission_ble_thread` の scan 空振りが `Ble { step: "scan" }` で表現
  されるかは実装を確認し、**scan タイムアウトを一意に識別できる形**
  （step 文字列 or 専用 variant）に合わせる。識別できない場合は
  `find_commissionable` を wrapper 側で直接呼んで scan を分離する
  （scan 空振り = Unavailable、以降 = `commission_btp_thread` 系で Err）—
  この場合 `commission_ble_thread` ではなく scan → `open_link` →
  `commission_btp_thread` の組み合わせに置き換える（M6b の
  `live_commission_ble.rs` を参照）。マーカー文字列の小細工より分離の方が
  素直なら分離を選んでよい（テストの意味を保つこと）。
- `CommissionableInstance.discriminator` は `Option<u32>`（M8b）。
- `dnssd::resolve_commissionable` の timeout 5 秒は chip-tool の探索より
  短いが、BLE への切替を早くするための意図的な値（spec の自動経路選択）。

- [ ] **Step 3: feature 貫通**

`crates/mat-native/Cargo.toml`:

```toml
[features]
ble = ["mat-controller/ble"]
```

`crates/mat-native/src/lib.rs` に `pub mod commission;` を追加。

- [ ] **Step 4: commissioning.rs の doc 表を追従**

`commissioning.rs` 冒頭（または `CommissionError` の doc）の ErrorKind 写像
表を、Step 2 の `kind_of` と一致する内容に更新（M6b fix-later の解消）:
Timeout→`timeout` / Attestation・Noc・CommandStatus→`device_rejected` /
NetworkConfig→`unreachable` / Malformed・Csr→`parse_error` / その他→
`commission_failed` / Discovery→（エラーではなく chip-tool フォールバック）。

- [ ] **Step 5: テスト + Commit**

```bash
cargo fmt
cargo test -p mat-native 2>&1 | tail -3
cargo test -p mat-native --features ble 2>&1 | tail -3   # libdbus 無しで cfg 分岐がコンパイルできない場合は cfg(feature) の位置を直す。ローカルに libdbus が無くビルド不能なら、その旨を報告に明記（cross で E2E 時に検証）
cargo clippy -p mat-native --all-targets -- -D warnings 2>&1 | tail -3
git add crates/mat-native crates/mat-controller/src/commissioning.rs
git commit -m "feat(mat-native): commission ラッパー（mDNS→BLE自動経路+ErrorKind写像+feature ble貫通） (M8c-1 Task4)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: mat 側配線（CLI + commands/commission.rs の native 分岐）

**Files:**
- Modify: `crates/mat/src/cli.rs`（Commission に `--thread-dataset` 追加）
- Modify: `crates/mat/src/commands/commission.rs`
- Modify: `crates/mat/src/main.rs`（Commission 呼び出しに iface/fabric/issuer を渡す）
- Modify: `crates/mat/Cargo.toml`（`[features] ble = ["mat-native/ble"]`）
- Test: `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: `mat_native::commission::{commission, CommissionRequest, CommissionAttempt}`（Task 4）。
- Produces: `commands::commission::run(store_path, target, setup_code, node_id, alias, native: Option<&native_direct::Config>, thread_dataset: Option<&str>)`
  相当の新シグネチャ（`native_direct::Config { iface, fabric_index, issuer_index }` は既存 struct を流用）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/tests/integration.rs` の commission 節に追記:

```rust
#[test]
fn commission_native_bogus_iface_falls_back_to_chip_tool() {
    // 存在しない iface → 資材構築前に iface_index が失敗 → warn +
    // chip-tool フォールバックで従来どおり成功する。
    let store = TempDir::new().unwrap();
    mat(store.path())
        .env("MAT_IFACE", "mat-test-no-such-iface")
        .env("MAT_LOG", "warn")
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"success\""))
        .stderr(predicate::str::contains("falling back to chip-tool"));
}

#[test]
fn commission_thread_dataset_arg_is_accepted() {
    // --thread-dataset は BLE 経路（native）専用だが、chip-tool フォール
    // バック時も引数としては受理される（exit 2 にならない）。
    let store = TempDir::new().unwrap();
    mat(store.path())
        .env("MAT_IFACE", "mat-test-no-such-iface")
        .env("MAT_LOG", "warn")
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:FAKE",
            "--thread-dataset",
            "0e080000000000010000",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"success\""));
}
```

Run: `cargo test -p mat --test integration commission_native 2>&1 | tail -5`
→ FAIL（`--thread-dataset` 未定義 / "falling back" 不在）。

- [ ] **Step 2: cli.rs**

Commission variant に追加:

```rust
        /// BLE+Thread commissioning 用の Thread active operational dataset
        /// （hex）。native 経路（MAT_IFACE）で mDNS に見つからないデバイスを
        /// BLE で commission するときに必須。chip-tool 経路では未使用。
        #[arg(long = "thread-dataset", env = "MAT_THREAD_DATASET", value_name = "HEX")]
        thread_dataset: Option<String>,
```

- [ ] **Step 3: commands/commission.rs の native 分岐**

`run` のシグネチャを変更し、chip-tool 実行の前に native 分岐を挿入:

```rust
pub fn run(
    store_path: &Path,
    target: &str,
    setup_code: &str,
    node_id: Option<u64>,
    alias: Option<&str>,
    native: Option<&crate::native_direct::Config<'_>>,
    thread_dataset: Option<&str>,
) -> Result<(), MatError> {
    let mut store = Store::open_or_init(store_path)?;
    let node_id = node_id.unwrap_or_else(|| next_node_id(&store));

    // native 直経路（M8c-1）: MAT_IFACE 設定時は mat-controller で
    // in-process commission。Unavailable（未接触失敗）のみ chip-tool へ
    // フォールバック。Err（PASE 開始後の失敗）は即エラー — chip-tool での
    // 自動再実行は二重 commission を招くためフォールバックしない。
    if let Some(cfg) = native {
        match native_commission(cfg, &store, setup_code, node_id, thread_dataset) {
            Ok(NativeOutcome::Done) => {
                return record_success(&mut store, node_id, target, alias);
            }
            Ok(NativeOutcome::Unavailable(reason)) => {
                tracing::warn!(%reason, "native commissioning unavailable; falling back to chip-tool");
            }
            Err(e) => return Err(e),
        }
    }

    let chip = ChipTool::new(store.root());
    // …（既存の chip-tool 実行 + 成否処理はそのまま。成功側は
    //    record_success を呼ぶ形に共通化する）…
}

enum NativeOutcome {
    Done,
    Unavailable(String),
}

fn native_commission(
    cfg: &crate::native_direct::Config<'_>,
    store: &Store,
    setup_code: &str,
    node_id: u64,
    thread_dataset: Option<&str>,
) -> Result<NativeOutcome, MatError> {
    let dataset = thread_dataset
        .map(|h| {
            decode_hex(h).ok_or_else(|| {
                MatError::new(
                    ErrorKind::Other,
                    "invalid --thread-dataset: expected hex bytes".to_string(),
                )
            })
        })
        .transpose()?;
    let req = mat_native::commission::CommissionRequest {
        setup_code: setup_code.to_string(),
        device_node_id: node_id,
        thread_dataset: dataset,
        paa_dir: paa_trust_store_path(store.root()),
        cd_signer_dir: cd_signer_store_path(store.root()),
    };
    let ncfg = mat_native::NativeConfig {
        store: store.root().to_path_buf(),
        iface: cfg.iface.to_string(),
        fabric_index: cfg.fabric_index,
        issuer_index: cfg.issuer_index,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| MatError::new(ErrorKind::Other, format!("tokio runtime: {e}")))?;
    match rt.block_on(mat_native::commission::commission(&ncfg, &req))? {
        mat_native::commission::CommissionAttempt::Done => Ok(NativeOutcome::Done),
        mat_native::commission::CommissionAttempt::Unavailable(r) => {
            Ok(NativeOutcome::Unavailable(r))
        }
    }
}

/// 台帳 upsert + alias + JSON 出力（chip-tool 経路の成功側と共通）。
fn record_success(
    store: &mut Store,
    node_id: u64,
    target: &str,
    alias: Option<&str>,
) -> Result<(), MatError> {
    // …既存の成功側コード（upsert_node / alias / emit）をここへ移動…
}

/// 偶数桁の hex 文字列 → bytes。
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || s.is_empty() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// CD signer 証明書ディレクトリ（PAA と同型の解決順）。
fn cd_signer_store_path(store_root: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MAT_CD_SIGNER_STORE") {
        return Some(PathBuf::from(p));
    }
    let default = store_root.join("cd-signer-store");
    default.is_dir().then_some(default)
}
```

（既存 `paa_trust_store_path` は戻り型そのまま流用。`decode_hex` /
`cd_signer_store_path` / `decode_hex` の境界ケースは `mod tests` に
ユニットテストを足す: 奇数桁 None / 空 None / 正常系。）

- [ ] **Step 4: main.rs / Cargo.toml**

main.rs の Commission arm を新シグネチャに合わせる（native_direct と同じく
`args.iface` があるときだけ `native_direct::Config` を組んで渡す）:

```rust
        Command::Commission {
            target,
            setup_code,
            node_id,
            alias,
            thread_dataset,
        } => commands::commission::run(
            &store_path,
            target,
            setup_code,
            *node_id,
            alias.as_deref(),
            args.iface
                .as_deref()
                .map(|iface| native_direct::Config {
                    iface,
                    fabric_index: args.fabric_index,
                    issuer_index: args.issuer_index,
                })
                .as_ref(),
            thread_dataset.as_deref(),
        ),
```

（`.map().as_ref()` は一時値のライフタイムで怒られる — その場合は
`let native_cfg = args.iface.as_deref().map(|iface| native_direct::Config { .. });`
を match の前に置き `native_cfg.as_ref()` を渡す。）

`crates/mat/Cargo.toml` に:

```toml
[features]
ble = ["mat-native/ble"]
```

- [ ] **Step 5: テスト + Commit**

```bash
cargo fmt && cargo test -p mat 2>&1 | tail -5
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
git add crates/mat crates/mat-native
git commit -m "feat(mat): commission の native 分岐（MAT_IFACE opt-in、未接触失敗のみchip-toolへ、--thread-dataset追加） (M8c-1 Task5)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: 実機 E2E ハーネス

**Files:**
- Create: `scripts/e2e-m8c1-real.sh`
- Modify: `Taskfile.yml`（`e2e:m8c1:real` 追加）

**Interfaces:**
- Consumes: 0.20.0 の mat（**cross gnu + feature ble ビルド**）、jarvis、
  玄関ライト（fabric 無し・BLE commissionable）、jarvis の OTBR
  （Thread dataset 取得元）。

- [ ] **Step 1: 既存ハーネスの流儀を読む**

`scripts/e2e-m8b-real.sh`（構成・env ガード・外部バイナリ /nonexistent 方式・
マーカー grep）と `scripts/e2e-m6b-real.sh` 相当（BLE ビルド・転送の作法 —
`Cross.toml` のコメント参照）を読む。

- [ ] **Step 2: ハーネスを書く**

`scripts/e2e-m8c1-real.sh` — 必須 env: `MAT_E2E_HOST`（jarvis）、
`MAT_E2E_IFACE`（既定 eth0）、`MAT_E2E_FABRIC_INDEX`（既定 2）、
`MAT_E2E_BLE_DISCRIMINATOR` 等はデバイス QR から自動なので不要。
setup code はハーネス引数でなく **実行時プロンプトで人力入力**（QR は
デバイス毎・repo にコミットしない）。検証項目:

1. **ビルド**: `cross build --release --target aarch64-unknown-linux-gnu -p mat --features ble`
   → scp。`file` で aarch64 動的リンクを確認（musl 静的でないこと）。
2. **native BLE+Thread commission**: jarvis 上で Thread dataset を取得
   （`sudo ot-ctl dataset active -x`）→ `MAT_IFACE=$IFACE MAT_THREAD_DATASET=<dataset>
   MAT_CHIP_TOOL_BIN=/nonexistent/chip-tool mat commission --target <addr> --setup-code <QR>`
   が成功し、(a) stderr に `commission executed (native ble-thread)`、
   (b) chip-tool 不在でも成功 = 純 native の実証、(c) 台帳に node 記録。
3. **制御確認**: 新 node に `mat on/off`（native 直経路）が通る。
4. **on-network 経路**（状態が許せば）: RemoveFabric（`mat invoke ...
   remove-fabric` 相当 or ハーネス内で native API）→ Thread 残留の
   commissionable を `commission executed (native on-network)` マーカーで
   再 commission。難しければ WARN + 人力確認に切替（M8b と同流儀）。
5. **フォールバック健全性**: `MAT_IFACE` 未設定 + 実 chip-tool で
   commission 相当操作が従来どおり動く（対象は状態次第 — 最低限
   `mat discover` 等の無害 op で chip-tool 経路の生存確認）。

- [ ] **Step 3: Taskfile 配線 + ローカル dry-run**

```yaml
  e2e:m8c1:real:
    desc: "M8c-1 実機 E2E: native commission (要 jarvis + MAT_E2E_HOST)"
    cmds:
      - bash scripts/e2e-m8c1-real.sh
```

`bash -n` + 必須 env ガードの動作確認まで（実機なしで）。

- [ ] **Step 4: Commit**

```bash
git add scripts/e2e-m8c1-real.sh Taskfile.yml
git commit -m "test(e2e): M8c-1実機ハーネス（native BLE+Thread/on-network commission + フォールバック） (M8c-1 Task6)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: ドキュメント + 最終確認

**Files:**
- Modify: `ARCHITECTURE.md`（M8c 3分割の反映 + M8c-1 実装済み内容。実機 E2E は「別途実施後に追記」）
- Modify: `README.md`（commission の native 対応 + `--thread-dataset` / `MAT_THREAD_DATASET` / `MAT_CD_SIGNER_STORE`、フォールバック規則「未接触失敗のみ」）
- Modify: `CLAUDE.md`（Backend 節の native hotpath 一覧に commission を追記）

- [ ] **Step 1: 各ドキュメント更新**

ARCHITECTURE.md: Phase 5 節の M8c bullet を 3 分割（M8c-1/2/3、spec 参照）に
書き換え、M8c-1 実装済み内容（epoch IPK 読み出し・from_materials・
mDNS→BLE 自動経路・フォールバック境界・manual code の short フィルタと
BLE 非対応・feature ble 貫通と cross gnu ビルド）を M8a/M8b と同じ密度で追記。

README.md: commission コマンドの節に native 経路（`MAT_IFACE`）と新引数を
追記。「PASE 開始後は chip-tool へフォールバックしない」を明記。

CLAUDE.md: Backend 節の op 一覧に commission（M8c-1）を 1 行追記。

- [ ] **Step 2: 最終チェック + Commit**

```bash
task check 2>&1 | tail -5   # all green
git add ARCHITECTURE.md README.md CLAUDE.md
git commit -m "docs: M8c-1（commission native化）の実装内容をARCHITECTURE/README/CLAUDE.mdに反映 (M8c-1 Task7)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## 完了後（plan の範囲外、ユーザーと実施）

1. jarvis での `task e2e:m8c1:real`（BLE は物理デバイス必須。玄関ライトを
   mat fabric へ commission する — HA 再アダプトの前提整備を兼ねる）。
2. 合格後: main へ `--no-ff` マージ + ARCHITECTURE に E2E 結果追記 +
   本番 0.20.0 デプロイ（BLE 有効ビルドを本番に載せるかは M8c-3 の
   「BLE 既定化」判断まで保留 — E2E 用ビルドと本番 musl ビルドの二本立て可）。
3. M8c-2（KVS group 書込所有 + diag node）の brainstorming へ。
