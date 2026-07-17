# Phase 5 M8c-3: native 既定化 + chip-tool 完全撤去 + fabric bootstrap 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `MAT_IFACE` / `MAT_MATD_IFACE` 未設定でも全 op が native で動き（iface 自動検出）、chip-tool / avahi-browse 経路をコード・テスト基盤・Docker から完全撤去し、`mat fabric init` で初回 fabric bootstrap（ランダム epoch IPK）ができる（0.22.0）。

**Architecture:** 二段構え。**Stage 1**（Task 1–7）= iface 自動検出（`mat-native::iface_select`）+ テスト基盤のトレイト fake 化 + native 既定化配線 + epoch 採用永続 — chip-tool フォールバックはコード上温存し、**実機 E2E ゲート 1**（jarvis、フォールバック発火ゼロ）で native-only 運用を実証。**Stage 2**（Task 8–13、ゲート 1 全 GREEN が着手条件）= `mat fabric init`（`CommissioningFabric` の KVS 永続）+ chip-tool / avahi 撤去 + Docker/Taskfile の gnu 一本化 + **実機 E2E ゲート 2**。spec: `docs/superpowers/specs/2026-07-17-phase5-m8c3-native-default-design.md`（**必読** — 特に「ユーザー決定」と「設計 2: epoch の永続と解決順」）。

**Tech Stack:** Rust (workspace)。新規依存なし（getrandom / rustix / base64ct / p256 は mat-controller に既存）。bash（実機 E2E）。

## Global Constraints

- **作業ブランチ**: `m8c3-native-default`（Task 1 で main から作成、worktree `.claude/worktrees/m8c3-native-default`）。**全タスクの冒頭で `pwd` と `git branch --show-current` を確認**（サブエージェントの shell はメイン repo (main) で始まる罠が既知）。
- **バージョン**: workspace `Cargo.toml` の `version = "0.22.0"`（Task 1）。
- **Stage 2（Task 8 以降）はゲート 1（Task 7、実機）全 GREEN が着手条件**。ゲート 1 が失敗したら中止してユーザーへ（Stage 1 まではフォールバック温存で撤退可能）。
- **iface 自動検出の候補条件**（spec 設計 3、ユーザー決定）: operstate up・MULTICAST・非 loopback・非 POINTOPOINT・IPv6 link-local 保有。候補ちょうど 1 つ → 採用、0 または複数 → **ハードエラー**（kind `other`、detail に候補列挙 + `set MAT_IFACE`）。Stage 1 からハードエラー（chip-tool へ黙って落とさない）。
- **epoch 解決順**（spec 設計 2）: KVS の mat-epoch キー（`mat/f/<idx>/ipk-epoch`）→ 無ければ定数を `verify_default_ipk_epoch` で検証し**その場で KVS へ採用永続**→使用。不一致 = `store_parse` ハードエラー。**Stage 1 でも epoch 系はフォールバックさせない**（採用永続の書込失敗含む。M8c-1 の「不一致→フォールバック」からの挙動変更、spec 承認済み）。
- **エラー kind は新設しない**: iface 曖昧/ゼロ = `other`(1)、KVS 不在 = `store_missing`(10)、KVS 不整合/epoch 不一致 = `store_parse`(10)、名前未解決 = `parse_error`(1)、commission 発見空振り = `unreachable`(5)。exit 12（`child_not_found`）は 0.22.0 で**廃止**（ErrorKind バリアントは wire 互換のため残すが、どこからも emit しない）。
- **flock 規律は M8c-2 と同じ**: KVS 書込は `KvsTxn`（sidecar flock + tmp+fsync+rename）。WouldBlock・I/O エラーは hard error、フォールバックしない。
- **マーカーログ**（E2E が verbatim で grep）: `iface auto-selected (native default)`（info、mat）/ `iface auto-selected (matd native default)`（info、matd 起動時）/ `ipk epoch adopted (kvs)`（info）/ `fabric bootstrap written (native kvs)`（info）。フォールバック warn（`falling back to chip-tool` を含む文言）は Stage 1 では既存のまま温存（ゲート 1 が不在を grep）、Stage 2 で発生源ごと消滅。
- **出力 JSON スキーマ・台帳・alias の挙動は現行と同一**（native 経路が既に出している形が正）。`timestamp` 必須（ISO 8601）。
- リポジトリは公開: 実ノード ID・実鍵・実証明書・実 IP をコミットしない（テストはダミー値、RFC 5737 / RFC 3849）。
- コミットは各タスク末尾、メッセージ末尾に `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。コミット前 `cargo fmt`。Task 6 / 12 / 13 で `task check`。
- rtk プロキシ環境: `git status` 等は自動で rtk 経由になるが挙動は同じ。

---

### Task 1: 前提確認 + worktree + バージョン 0.22.0

**Files:**
- Modify: `Cargo.toml`（workspace version のみ）

**Interfaces:**
- Produces: main から切った `m8c3-native-default` の worktree。以後の全タスクはここで作業。

- [ ] **Step 1: main の状態確認（着手ゲート）**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat
git log --oneline main | head -5
```

M8c-2 マージ（`64d5795` または `Merge branch 'm8c2-groupsettings-native'`）と M8c-3 spec コミット（`docs: M8c-3`）が見えること。見えなければ中止してユーザーへ。

- [ ] **Step 2: worktree + ブランチ作成**

```bash
git worktree add .claude/worktrees/m8c3-native-default -b m8c3-native-default main
cd .claude/worktrees/m8c3-native-default && pwd && git branch --show-current
```

- [ ] **Step 3: バージョン 0.22.0**

workspace `Cargo.toml` の `[workspace.package]` の `version = "0.21.0"` を `"0.22.0"` に変更。

- [ ] **Step 4: ビルド確認 + Commit**

```bash
cargo build -p mat 2>&1 | tail -3   # Cargo.lock の version 反映込みで通ること
git add Cargo.toml Cargo.lock
git commit -m "chore: version 0.22.0 (M8c-3 開始)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: mat-native — iface 自動検出（`iface_select`）

**Files:**
- Create: `crates/mat-native/src/iface_select.rs`
- Modify: `crates/mat-native/src/lib.rs`（`pub mod iface_select;` 追加）

**Interfaces:**
- Produces:
  - `mat_native::iface_select::IfaceInfo { pub name: String, pub flags: u32, pub operstate_up: bool, pub has_ipv6_ll: bool }`
  - `mat_native::iface_select::select(infos: &[IfaceInfo]) -> Result<String, SelectError>`（純関数、表駆動テスト対象）
  - `mat_native::iface_select::SelectError { NoCandidate, Ambiguous(Vec<String>) }`
  - `mat_native::iface_select::autodetect() -> Result<String, mat_core::error::MatError>`（`/sys`/`/proc` 走査 + `MatError`（kind `Other`）への写像。Task 4/5 が使用）

- [ ] **Step 1: 失敗するユニットテストを書く**

`crates/mat-native/src/iface_select.rs` を新規作成し、末尾に表駆動テストを書く（実装は最小スタブ `todo!()` でよい — まずテストがコンパイル・失敗することを確認する）:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn ifi(name: &str, flags: u32, up: bool, ll: bool) -> IfaceInfo {
        IfaceInfo { name: name.into(), flags, operstate_up: up, has_ipv6_ll: ll }
    }

    // flags: IFF_UP=0x1, IFF_LOOPBACK=0x8, IFF_POINTOPOINT=0x10, IFF_MULTICAST=0x1000
    const ETH: u32 = 0x1 | 0x1000; // up|multicast
    const LO: u32 = 0x1 | 0x8 | 0x1000;
    const TS: u32 = 0x1 | 0x10 | 0x1000; // tailscale0: up|pointopoint|multicast

    #[test]
    fn selects_single_ethernet() {
        let infos = [ifi("eth0", ETH, true, true), ifi("lo", LO, true, true)];
        assert_eq!(select(&infos).unwrap(), "eth0");
    }

    #[test]
    fn excludes_pointopoint_tailscale() {
        let infos = [ifi("eth0", ETH, true, true), ifi("tailscale0", TS, true, true)];
        assert_eq!(select(&infos).unwrap(), "eth0");
    }

    #[test]
    fn excludes_down_carrier_and_missing_ll() {
        // docker0 は up フラグはあるが operstate down / veth は link-local 無しを模す
        let infos = [
            ifi("eth0", ETH, true, true),
            ifi("docker0", ETH, false, true),
            ifi("veth1", ETH, true, false),
        ];
        assert_eq!(select(&infos).unwrap(), "eth0");
    }

    #[test]
    fn zero_candidates_is_error() {
        let infos = [ifi("lo", LO, true, true)];
        assert!(matches!(select(&infos), Err(SelectError::NoCandidate)));
    }

    #[test]
    fn multiple_candidates_is_ambiguous_and_lists_names() {
        let infos = [ifi("eth0", ETH, true, true), ifi("wlan0", ETH, true, true)];
        match select(&infos) {
            Err(SelectError::Ambiguous(names)) => assert_eq!(names, vec!["eth0", "wlan0"]),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

```bash
cargo test -p mat-native iface_select 2>&1 | tail -5
```

Expected: コンパイルエラー（`select` 未定義）または `todo!()` panic による FAIL。

- [ ] **Step 3: 実装**

```rust
//! iface 自動検出（M8c-3 native 既定化）。
//!
//! `MAT_IFACE` / `MAT_MATD_IFACE` 未設定時に Matter 用の iface を選ぶ。
//! 候補条件: operstate up（carrier 有 — 未使用 docker0 等を除外）・
//! MULTICAST・非 loopback・非 POINTOPOINT（tailscale0 / tun 系を除外 —
//! multicast egress で経路解決に勝ってしまう罠の回避）・IPv6 link-local
//! アドレス保有。候補ちょうど 1 つなら採用、0 または複数はハードエラー
//! （曖昧なまま選ぶと group 送信がサイレント不達 + カウンタ汚染になる
//! 前科があるため、決定的に選ばず利用者に `MAT_IFACE` 指定を求める）。
//! 毎回実行時に検出し状態は持たない（設計ルール 4）。

use mat_core::error::{ErrorKind, MatError};

const IFF_UP: u32 = 0x1;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_POINTOPOINT: u32 = 0x10;
const IFF_MULTICAST: u32 = 0x1000;

#[derive(Debug, Clone)]
pub struct IfaceInfo {
    pub name: String,
    pub flags: u32,
    pub operstate_up: bool,
    pub has_ipv6_ll: bool,
}

#[derive(Debug)]
pub enum SelectError {
    NoCandidate,
    Ambiguous(Vec<String>),
}

fn eligible(i: &IfaceInfo) -> bool {
    i.operstate_up
        && i.has_ipv6_ll
        && i.flags & IFF_UP != 0
        && i.flags & IFF_MULTICAST != 0
        && i.flags & IFF_LOOPBACK == 0
        && i.flags & IFF_POINTOPOINT == 0
}

/// 候補選別の純関数（表駆動テスト対象）。`infos` の列挙順を保つ。
pub fn select(infos: &[IfaceInfo]) -> Result<String, SelectError> {
    let mut names: Vec<String> = infos.iter().filter(|i| eligible(i)).map(|i| i.name.clone()).collect();
    match names.len() {
        0 => Err(SelectError::NoCandidate),
        1 => Ok(names.remove(0)),
        _ => Err(SelectError::Ambiguous(names)),
    }
}

/// `/sys/class/net` + `/proc/net/if_inet6` を走査して候補を集め、`select` する。
/// 失敗はすべて kind `other`（新 kind は設けない — spec 設計 4）。
pub fn autodetect() -> Result<String, MatError> {
    let infos = scan().map_err(|e| {
        MatError::new(ErrorKind::Other, format!("iface autodetect: scan failed: {e}"))
    })?;
    select(&infos).map_err(|e| match e {
        SelectError::NoCandidate => MatError::new(
            ErrorKind::Other,
            "iface autodetect: no usable interface (need up/multicast/non-p2p with IPv6 link-local); set MAT_IFACE".to_string(),
        ),
        SelectError::Ambiguous(names) => MatError::new(
            ErrorKind::Other,
            format!("iface autodetect: ambiguous candidates [{}]; set MAT_IFACE", names.join(", ")),
        ),
    })
}

fn scan() -> std::io::Result<Vec<IfaceInfo>> {
    // IPv6 link-local を持つ iface 名の集合: /proc/net/if_inet6 の各行は
    // "<addr32hex> <ifindex> <prefixlen> <scope> <flags> <name>"。scope 0x20 = link-local。
    let mut ll_names = std::collections::HashSet::new();
    for line in std::fs::read_to_string("/proc/net/if_inet6")?.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 6 && cols[3] == "20" {
            ll_names.insert(cols[5].to_string());
        }
    }
    let mut infos = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir("/sys/class/net")?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name); // 決定的な列挙順
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let base = entry.path();
        let flags = std::fs::read_to_string(base.join("flags"))
            .ok()
            .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        let operstate_up = std::fs::read_to_string(base.join("operstate"))
            .map(|s| s.trim() == "up")
            .unwrap_or(false);
        infos.push(IfaceInfo {
            has_ipv6_ll: ll_names.contains(&name),
            name,
            flags,
            operstate_up,
        });
    }
    Ok(infos)
}
```

`crates/mat-native/src/lib.rs` のモジュール宣言部（`pub mod ops;` 等の並び）に `pub mod iface_select;` を追加。

- [ ] **Step 4: テストが通ることを確認**

```bash
cargo test -p mat-native iface_select 2>&1 | tail -5
```

Expected: 5 テスト PASS。

- [ ] **Step 5: 実環境スモーク（WSL）**

```bash
cargo test -p mat-native --lib -- --nocapture 2>&1 | tail -3   # 既存テストが壊れていないこと
```

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/mat-native/src/iface_select.rs crates/mat-native/src/lib.rs
git commit -m "feat(native): iface 自動検出 iface_select（M8c-3 Task2）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: テスト基盤置換 — fake-chip-tool 依存の縮小（Stage 1 で先行）

**Files:**
- Modify: `crates/mat/tests/integration.rs`（1918 行 → chip-tool 非依存のテストだけに縮小）
- Modify: `crates/mat/tests/matd_auto.rs`
- Modify: `crates/matd/tests/integration.rs`
- Delete: `crates/mat/tests/fixtures/fake-chip-tool.sh`
- Modify（必要なら）: `crates/mat/src/native_direct.rs`（`#[cfg(test)]` の出力スキーマテスト拡充）

**Interfaces:**
- Consumes: `mat_native::test_support::FakeConn`（既存、feature `test-support`）
- Produces: chip-tool を一切 spawn しないテストスイート。Task 9–11 の撤去時にテスト改修が不要になる。

**背景（この順序の理由):** Stage 1 の native 既定化（Task 4）で `MAT_IFACE` 未設定でも native 経路に入るため、fake-chip-tool 前提の統合テストは環境依存で挙動が変わってしまう（例: discover は native browse の 0 件が正常結果になり fake の出力を見ない）。先にテストを目標形（トレイト fake + 最小バイナリテスト）へ置換してから既定化する。chip-tool 経路のコードは Stage 1 では残る（安全網、ゲート 1 が「使われていない」ことを実証する）。

- [ ] **Step 1: 現状のテスト一覧を把握**

```bash
grep -n "^fn \|fn .*()" crates/mat/tests/integration.rs | grep -v "^fn fake_chip_tool\|^fn mat(" | head -120
```

各テストを「chip-tool 依存（fake_chip_tool() を PATH/MAT_CHIP_TOOL_BIN に注入して成功出力をパースさせる）」と「バイナリだけで完結（arg エラー / store エラー / alias 解決 / matd 排他）」に分類してメモする。

- [ ] **Step 2: 残すテストを確定し、消すテストを削除**

**残す（chip-tool 非依存に改修の上）:**
- arg エラー系（unknown subcommand / 不正値 → exit 2）
- store 系（store 不在 → exit 10 `store_missing`、壊れた toml → exit 10 `store_parse`、未 commission node → exit 11）
- alias 解決系（`aliases.toml` の解決成功・未定義 alias エラー。**alias 解決はバックエンド到達前に完結する経路のみ**）
- `--matd` 明示 + 非対応 op → exit 2

**消す:** `FAKE_CHIP_MODE` / `FAKE_CHIP_ARGS_FILE` / `fake_chip_tool()` を使う全テスト（discover / read / write / invoke / describe / open-window / diag / group / commission の成功系・chip-tool エラー分類系）。

**残すテストの環境固定:** `mat()` ヘルパで `MAT_IFACE` を `lo` に固定する（自動検出の環境依存を排除。`lo` は明示指定なので autodetect は走らず、native エンジン構築が KVS 不在で fail → Stage 1 は warn + フォールスルーし、store 系エラーは従来どおりコマンド層で出る。Stage 2 で挙動が `store_missing` ハードエラーに変わっても exit code は同じ 10 — **テストは exit code と stderr の kind だけを assert し、detail 文字列に依存させない**）:

既存ヘルパ（`Command::cargo_bin` + `--store`、integration.rs:22）を次の形に改修（`MAT_CHIP_TOOL_BIN` / `fake_chip_tool()` は Step 3 で fixture ごと消える）:

```rust
fn mat(store: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_IFACE", "lo") // autodetect の環境依存を排除（テスト決定性）
        .env("MAT_MATD", "0")
        .arg("--store")
        .arg(store);
    c
}
```

- [ ] **Step 3: fixture 削除**

```bash
git rm crates/mat/tests/fixtures/fake-chip-tool.sh
```

`crates/mat/tests/matd_auto.rs` と `crates/matd/tests/integration.rs` も同様に fake-chip-tool 依存テストを削除し、matd プロトコル層のテスト（socket 発見 / dispatch / エラー伝搬）は fake ソケット・`FakeConn` ベースのものだけ残す。matd 側で fake-chip-tool を参照している場合は同じ分類基準で処理。

- [ ] **Step 4: 出力スキーマの補償テストを FakeConn 側に確認・拡充**

chip-tool 統合テストが担っていた「成功時 JSON スキーマ」の検証は native 側のユニットテストが正になる。現状の `crates/mat/src/native_direct.rs` の `#[cfg(test)]`（1572 行以降）を確認し、**read / write / invoke / describe / diag node の成功時 JSON（timestamp 除く全フィールド）を assert するテストが無い op があれば `FakeConn::scripted()` で追加**する。既にある op は重複追加しない（DRY）。

```bash
grep -n "fn .*json\|assert.*\"cluster\"\|assert.*\"attribute\"" crates/mat/src/native_direct.rs | head -20
```

- [ ] **Step 5: テスト全通過を確認**

```bash
cargo test -p mat -p matd 2>&1 | tail -10
```

Expected: 全 PASS（テスト数は縮小前より減る。chip-tool バイナリ・fake スクリプトへの参照が 0 件になったことも確認）:

```bash
grep -rn "fake-chip-tool\|FAKE_CHIP" crates/ --include="*.rs" | wc -l   # → 0
```

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A crates/mat/tests crates/matd/tests crates/mat/src/native_direct.rs
git commit -m "test: fake-chip-tool 基盤をトレイトfake+最小バイナリテストへ置換（M8c-3 Task3）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: mat 直経路の native 既定化

**Files:**
- Modify: `crates/mat/src/main.rs`（経路選択 — `MAT_IFACE` 未設定でも native_cfg を組む）
- Modify: `crates/mat/src/cli.rs`（`--iface` の doc コメント更新）

**Interfaces:**
- Consumes: `mat_native::iface_select::autodetect()`（Task 2）
- Produces: `mat` の全 op が env 未設定でも native 経路に入る。マーカー `iface auto-selected (native default)`。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/tests/integration.rs` に追加（Task 3 の `mat()` は `MAT_IFACE=lo` 固定なので、このテストだけ env を外して autodetect を発火させる。**候補数は環境依存のため、「iface エラー（other）または後段エラー」のどちらかで JSON エラー形式が出ること** = 少なくとも panic せず構造化エラーで落ちることを assert する）:

```rust
#[test]
fn no_iface_env_reaches_autodetect_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("mat").unwrap();
    cmd.env_remove("MAT_IFACE")
        .env("MAT_MATD", "0")
        .arg("--store")
        .arg(dir.path())
        .args(["read", "1", "1", "onoff", "on-off"]);
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    // 自動検出が走った証拠: 候補一意なら native 経路に入り store 系エラー、
    // 候補 0/複数なら iface autodetect エラー。いずれも構造化 JSON エラー。
    assert!(stderr.contains("\"error\""), "structured error expected: {stderr}");
}
```

- [ ] **Step 2: テストが失敗することを確認**

```bash
cargo test -p mat --test integration no_iface_env 2>&1 | tail -5
```

Expected: FAIL（現状 `MAT_IFACE` 無しは chip-tool 直行 → chip-tool が無い環境では exit 12 の `child_not_found` エラー…これも `"error"` を含むため PASS してしまう場合がある。その場合はこの Step を「現状挙動の記録」とし、Step 3 実装後に stderr へ `iface auto-selected` marker または autodetect エラーが出ることの assert を追加して赤→緑を確認する）。

- [ ] **Step 3: main.rs の経路選択を書き換え**

`crates/mat/src/main.rs` の native 直経路ブロック（65 行目付近）を置換:

```rust
// native 直経路: MAT_IFACE 設定時はその iface、未設定なら自動検出
// （M8c-3 native 既定化）。自動検出の候補 0 / 複数はハードエラー
// （chip-tool へ黙って落とさない — spec 設計 3）。
let iface_owned: String = match &args.iface {
    Some(i) => i.clone(),
    None => match mat_native::iface_select::autodetect() {
        Ok(i) => {
            tracing::info!(iface = %i, "iface auto-selected (native default)");
            i
        }
        Err(e) => {
            e.emit();
            return ExitCode::from(e.kind.exit_code());
        }
    },
};
let native_cfg = Some(native_direct::Config {
    iface: &iface_owned,
    fabric_index: args.fabric_index,
    issuer_index: args.issuer_index,
});
```

以降の `if let Some(cfg) = &native_cfg { ... }` と各コマンドへの `native_cfg.as_ref()` 渡しは無変更で生きる（常に `Some` になるだけ）。`mat` crate の `Cargo.toml` に `mat-native` 依存が既にあることを確認（M7 から有り）。

`crates/mat/src/cli.rs` の `--iface` doc コメントを更新: 「未設定なら自動検出（up・multicast・非P2P・IPv6 link-local の一意候補。曖昧ならエラー）。明示指定で上書き。」

- [ ] **Step 4: テスト全通過を確認**

```bash
cargo test -p mat 2>&1 | tail -5
```

Expected: 全 PASS（Task 3 で `MAT_IFACE=lo` 固定済みのため既存テストは autodetect を踏まない）。

- [ ] **Step 5: 手元スモーク（native 既定化の実感確認）**

```bash
unset MAT_IFACE; MAT_STORE=$(mktemp -d) cargo run -p mat -- read 1 1 onoff on-off 2>&1 | head -5
```

Expected: `iface auto-selected (native default)`（WSL で eth0 一意の場合）または autodetect エラーの構造化 JSON。chip-tool 不在による exit 12 で**ない**こと。

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add crates/mat/src/main.rs crates/mat/src/cli.rs crates/mat/tests/integration.rs
git commit -m "feat(mat): MAT_IFACE 未設定でも native 既定（iface 自動検出）（M8c-3 Task4）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: matd の native 既定化（起動時自動検出）

**Files:**
- Modify: `crates/matd/src/main.rs`（131–153 行の native 構築分岐）

**Interfaces:**
- Consumes: `mat_native::iface_select::autodetect()`（Task 2）
- Produces: `MAT_MATD_IFACE` 未設定でも matd が native backend を有効化。曖昧なら**起動拒否**（exit 1）。マーカー `iface auto-selected (matd native default)`。

- [ ] **Step 1: 現状の分岐を確認**

`crates/matd/src/main.rs:131-153`: `cli.iface` が `Some` なら native backend 構築、`None` なら `"MAT_MATD_IFACE unset; native backend disabled (chip-tool only)"` を info で出して無効化している。

- [ ] **Step 2: 分岐を置換**

```rust
// native warm session バックエンド。iface は env / --iface、未設定なら自動
// 検出（M8c-3 native 既定化）。自動検出の候補 0 / 複数は起動拒否 —
// 全 op が死ぬ設定不備なので per-op エラーではなく fail-fast にする
// （jarvis の systemd unit は env 設定済みで影響なし）。
let iface: String = match &cli.iface {
    Some(i) => i.clone(),
    None => match mat_native::iface_select::autodetect() {
        Ok(i) => {
            tracing::info!(iface = %i, "iface auto-selected (matd native default)");
            i
        }
        Err(e) => {
            tracing::error!(kind = ?e.kind, detail = %e.detail, "iface autodetect failed; refusing to start (set MAT_MATD_IFACE)");
            std::process::exit(1);
        }
    },
};
```

続く native backend 構築（旧 `Some(iface)` アーム内のコード）は `iface` 変数をそのまま使う形に付け替える。構築失敗時の既存挙動（warn + chip-tool フォールバック運転）は Stage 1 では温存。`matd` crate に `mat-native` 依存が既にあることを確認。

CLI doc コメント（`crates/matd/src/main.rs:51-54`）も「未指定なら自動検出（曖昧なら起動拒否）」に更新。

- [ ] **Step 3: ユニット/統合テスト確認**

```bash
cargo test -p matd 2>&1 | tail -5
```

Expected: 全 PASS（matd のテストは iface を明示注入しているものだけが残っている — Task 3 の置換結果。していないテストがあれば `MAT_MATD_IFACE=lo` を注入）。

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add crates/matd/src/main.rs
git commit -m "feat(matd): MAT_MATD_IFACE 未設定でも native 既定（起動時自動検出、曖昧は起動拒否）（M8c-3 Task5）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: epoch の KVS 永続 + 採用（adopt）

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`（mat-epoch キーの read/write）
- Modify: `crates/mat-native/src/commission.rs`（ガード → 解決順 + 採用永続へ書き換え）

**Interfaces:**
- Produces:
  - `mat_controller::kvs::mat_ipk_epoch_key(fabric_index: u8) -> String`（= `"mat/f/<idx>/ipk-epoch"`）
  - `mat_controller::kvs::read_mat_ipk_epoch(main_ini: &Path, fabric_index: u8) -> Result<Option<[u8; 16]>, KvsError>`
  - `mat_controller::kvs::write_mat_ipk_epoch(main_ini: &Path, fabric_index: u8, epoch: &[u8; 16]) -> Result<(), KvsError>`（`KvsTxn` 経由、flock 排他）
  - Task 8 の fabric init も `write_mat_ipk_epoch` を使う。

- [ ] **Step 1: kvs.rs — 失敗するテストを書く**

`crates/mat-controller/src/kvs.rs` の `#[cfg(test)]` に追加（既存テストの INI 組み立てヘルパの流儀に合わせる）:

```rust
#[test]
fn mat_ipk_epoch_roundtrip_and_absent() {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("chip_tool_config.ini");
    std::fs::write(&ini, "[Default]\n").unwrap();
    assert_eq!(read_mat_ipk_epoch(&ini, 1).unwrap(), None);
    let epoch = [0xA5u8; 16];
    write_mat_ipk_epoch(&ini, 1, &epoch).unwrap();
    assert_eq!(read_mat_ipk_epoch(&ini, 1).unwrap(), Some(epoch));
    // 別 fabric index は独立
    assert_eq!(read_mat_ipk_epoch(&ini, 2).unwrap(), None);
    // 16 バイト以外は KvsError（手で壊す）
    let text = std::fs::read_to_string(&ini).unwrap();
    std::fs::write(&ini, text.replace(&base64ct::Base64::encode_string(&epoch), "AAAA")).unwrap();
    assert!(read_mat_ipk_epoch(&ini, 1).is_err());
}
```

- [ ] **Step 2: テストが失敗することを確認**

```bash
cargo test -p mat-controller mat_ipk_epoch 2>&1 | tail -5
```

Expected: コンパイルエラー（関数未定義）。

- [ ] **Step 3: kvs.rs 実装**

```rust
/// mat 専用の epoch IPK 永続キー（M8c-3）。chip-tool の名前空間
/// （`f/<idx>/...` / `g/...` / `ExampleOpCredsCAKey<n>` 等）と衝突しない
/// `mat/` プレフィクスを使う。chip-tool は未知キーを無視するため、
/// Stage 1（chip-tool 共存期）でも安全。値は 16 バイトの epoch 鍵の base64。
pub fn mat_ipk_epoch_key(fabric_index: u8) -> String {
    format!("mat/f/{fabric_index}/ipk-epoch")
}

pub fn read_mat_ipk_epoch(main_ini: &Path, fabric_index: u8) -> Result<Option<[u8; 16]>, KvsError> {
    let text = std::fs::read_to_string(main_ini).map_err(KvsError::Io)?;
    let sec = default_section(&text).ok_or(KvsError::SectionMissing)?;
    match decode_b64(sec, &mat_ipk_epoch_key(fabric_index))? {
        None => Ok(None),
        Some(v) => {
            let arr: [u8; 16] = v.try_into().map_err(|_| KvsError::BadKeySet {
                fabric_index,
                reason: "mat ipk epoch must be 16 bytes",
            })?;
            Ok(Some(arr))
        }
    }
}

pub fn write_mat_ipk_epoch(main_ini: &Path, fabric_index: u8, epoch: &[u8; 16]) -> Result<(), KvsError> {
    let mut txn = KvsTxn::open(main_ini)?;
    txn.set(&mat_ipk_epoch_key(fabric_index), epoch);
    txn.commit()
}
```

※ `KvsError::BadKeySet` が `{fabric_index, reason}` 形でない場合は既存バリアントの形に合わせる（`grep -n "BadKeySet\|enum KvsError" crates/mat-controller/src/kvs.rs` で確認し、無理に新バリアントを足さず既存の「不正データ」系を使う）。

- [ ] **Step 4: kvs テスト通過確認**

```bash
cargo test -p mat-controller mat_ipk_epoch 2>&1 | tail -5
```

Expected: PASS。

- [ ] **Step 5: commission.rs — ガードを解決順 + 採用永続へ書き換え**

`crates/mat-native/src/commission.rs` の epoch ガードブロック（`verify_default_ipk_epoch` 呼び出し〜`from_materials`、148–158 行付近）を置換:

```rust
// epoch IPK 解決（M8c-3 spec 設計 2）: ① KVS の mat-epoch キー →
// ② 無ければ chip-tool 既定定数を KDF ガードで検証し、その場で KVS へ
// 採用永続（adopt）→ 使用。③ 不一致は store_parse ハードエラー。
// この解決はフォールバックさせない（採用永続の書込失敗も hard error —
// flock 規律は M8c-2 と同じ）。M8c-1 の「不一致 → Unavailable（フォール
// バック）」からの挙動変更（spec 承認済み）。
let ipk_epoch: [u8; 16] = match kvs::read_mat_ipk_epoch(&main_ini, cfg.fabric_index) {
    Err(e) => {
        return Err(MatError::new(
            ErrorKind::StoreParse,
            format!("kvs ipk epoch: {e}"),
        ))
    }
    Ok(Some(epoch)) => {
        // 永続済み epoch と KVS operational の整合を毎回検証（片方だけ
        // 書き換わった不整合ストアで commission しない）。
        let cfid = fabric::compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
        if fabric::derive_ipk_operational(&epoch, &cfid) != creds.ipk_operational {
            return Err(MatError::new(
                ErrorKind::StoreParse,
                "kvs ipk epoch does not derive the stored operational key (inconsistent store)".to_string(),
            ));
        }
        epoch
    }
    Ok(None) => {
        if !fabric::verify_default_ipk_epoch(
            &creds.root_public_key,
            creds.fabric_id,
            &creds.ipk_operational,
        ) {
            return Err(MatError::new(
                ErrorKind::StoreParse,
                "fabric IPK epoch unknown: not persisted and not the chip-tool default (rotated or foreign fabric)".to_string(),
            ));
        }
        kvs::write_mat_ipk_epoch(&main_ini, cfg.fabric_index, &fabric::CHIP_TOOL_DEFAULT_IPK_EPOCH)
            .map_err(|e| MatError::new(ErrorKind::StoreParse, format!("kvs ipk epoch adopt write: {e}")))?;
        tracing::info!(fabric_index = cfg.fabric_index, "ipk epoch adopted (kvs)");
        fabric::CHIP_TOOL_DEFAULT_IPK_EPOCH
    }
};
let commissioning_fabric = CommissioningFabric::from_materials(materials, ipk_epoch);
```

`commission()` の戻り値は `Result<CommissionAttempt, MatError>` のまま（`Err` は呼び出し側 `commands/commission.rs` で即エラーになり、フォールバックしない — 既存の contract どおり）。`use mat_core::error::ErrorKind;` が無ければ追加。`compressed_fabric_id` が `creds` から直接取れる場合（`FabricCredentials` にフィールドがあるか `grep -n "compressed" crates/mat-controller/src/fabric.rs` で確認）はそちらを使う。

- [ ] **Step 6: commission.rs のユニットテスト追加**

`crates/mat-native/src/commission.rs` の `#[cfg(test)]` に、temp INI + ダミー資材で 3 分岐（採用永続が起きる / 永続済みを読む / 不一致で store_parse）を検証するテストを書く。資材の組み立ては `crates/mat-controller/src/kvs.rs` の既存テスト（`read_self_issue_materials` 系）の INI フィクスチャ生成コードを参照し、`CommissioningFabric::generate()` + `write_kvs_bootstrap` は Task 8 でしか入らないため、ここでは kvs テストと同様に手組み INI を使う。検証ポイント:

```rust
// 1) epoch キー無し + operational が定数由来 → Ok になり、INI に mat/f/1/ipk-epoch が書かれる
// 2) epoch キー有り（定数と別のランダム値）+ operational がその epoch 由来 → Ok、追記なし
// 3) epoch キー無し + operational が定数と無関係 → Err(kind == StoreParse)
```

※ commission() 全体を走らせると mDNS に出てしまうため、epoch 解決部を `fn resolve_ipk_epoch(main_ini: &Path, fabric_index: u8, creds: &fabric::FabricCredentials) -> Result<[u8; 16], MatError>` として関数に切り出してからテストする（Step 5 のコードをその関数へ移し、`commission()` は呼ぶだけにする）。

- [ ] **Step 7: テスト通過確認 + task check**

```bash
cargo test -p mat-controller -p mat-native 2>&1 | tail -5
task check 2>&1 | tail -5
```

Expected: 全 PASS。

- [ ] **Step 8: Commit**

```bash
cargo fmt
git add crates/mat-controller/src/kvs.rs crates/mat-native/src/commission.rs
git commit -m "feat(native): epoch IPK の KVS 永続 + 定数採用（adopt）（M8c-3 Task6）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: 実機 E2E ゲート 1 ハーネス + 実行（★CHECKPOINT）

**Files:**
- Create: `scripts/e2e-m8c3-real.sh`（STAGE=1 / STAGE=2 の二相対応。この Task では STAGE=1 部分）
- Modify: `Taskfile.yml`（`e2e:m8c3:real` タスク追加）

**Interfaces:**
- Consumes: Task 2–6 の全マーカーログ。
- Produces: ゲート 1 の合否。**全 GREEN が Task 8 以降の着手条件。**

- [ ] **Step 1: ハーネス作成**

`scripts/e2e-m8c2-real.sh` の骨格（trap 後始末・stderr への PASS/FAIL・positive marker 二重チェック・musl rust-lld クロスビルド・`ssh -n` の作法・KVS バックアップ実在ガード）を流用し、`scripts/e2e-m8c3-real.sh` を書く。`STAGE` 環境変数（既定 `1`）で検証セットを切り替える。**STAGE=1 の検証項目**（spec「実機 E2E ゲート 1」）:

1. 準備: musl クロスビルド（BLE 不要 — gate 1 の commission は on-network）→ scp → matd 停止（trap で復帰）→ KVS バックアップ。
2. **env 未設定 native スイープ（直経路）**: `MAT_IFACE` / `MAT_MATD_IFACE` を unset した状態で
   discover / read / write / invoke / describe / diag thread / diag node --deep /
   open-window / group provision（使い捨て group 99、--rebind 含む）/ group invoke
   が全て成功。各実行の stderr に `iface auto-selected (native default)` があること。
3. **matd 経路**: matd を env 無しで起動（`iface auto-selected (matd native default)` を確認）→ read / write / group 系が matd 経由で成功。
4. **フォールバック発火ゼロ**: 全実行ログを結合して `falling back` の出現 0 件を assert（`assert_no_fallback`）。加えて `MAT_CHIP_TOOL_BIN=/nonexistent/chip-tool` を全コマンドに付与し、spawn があれば exit 12 で即 FAIL する二重チェック（M8a 以来の作法）。
5. **epoch 採用永続**: KVS に `mat/f/1/ipk-epoch` がまだ無いことを確認 → on-network commission（開いている使い捨てデバイス or open-window 済みノード。M8c-1 E2E と同じ対象選定、都合が悪ければ WARN + 人力確認へ切替可）→ stderr に `ipk epoch adopted (kvs)` → INI に `mat/f/1/ipk-epoch` が現れる → 2 回目の commission 系操作で adopted マーカーが**出ない**（読み出し経路）ことを確認 → 台帳・デバイスを後始末。
6. **iface 一意選択の実測**: jarvis 上で `mat discover` の marker が `eth0`（tailscale0 で無いこと）。
7. 後始末: KVS リストア（バックアップ実在ガード付き）、matd 再起動。

- [ ] **Step 2: Taskfile にタスク追加**

```yaml
  e2e:m8c3:real:
    desc: "M8c-3 実機 E2E（jarvis）。STAGE=1 既定 / STAGE=2 は撤去後の最終受け入れ"
    cmds:
      - bash scripts/e2e-m8c3-real.sh
```

- [ ] **Step 3: シェル構文チェック + dry 検証**

```bash
bash -n scripts/e2e-m8c3-real.sh
shellcheck scripts/e2e-m8c3-real.sh 2>&1 | head -20   # 無ければ skip
```

- [ ] **Step 4: Commit**

```bash
git add scripts/e2e-m8c3-real.sh Taskfile.yml
git commit -m "test(e2e): M8c-3 実機ハーネス（STAGE=1 ゲート1）（M8c-3 Task7）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

- [ ] **Step 5: ★実機ゲート 1 実行（ユーザーと実施）**

```bash
task e2e:m8c3:real
```

**全 GREEN を確認してから Task 8 へ。**FAIL したら中止し、原因を systematic-debugging で切り分けてユーザーへ報告（Stage 1 はフォールバック温存のため本番影響なしで撤退可能）。

---

### Task 8: `mat fabric init`（fabric bootstrap、Stage 2 開始）

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`（`KvsTxn::create` — 新規 INI 作成）
- Modify: `crates/mat-controller/src/group_settings.rs`（`serialize_keyset` / `EPOCH_START_TIME` を `pub(crate)` へ昇格）
- Modify: `crates/mat-controller/src/commissioning.rs`（`CommissioningFabric::write_kvs_bootstrap`）
- Modify: `crates/mat/src/cli.rs`（`fabric init` サブコマンド）
- Modify: `crates/mat/src/main.rs`（dispatch — iface 解決**前**に処理）
- Create: `crates/mat/src/commands/fabric.rs`
- Modify: `crates/mat/src/commands/mod.rs`
- Test: `crates/mat/tests/integration.rs`（ローカル完結フル E2E）

**Interfaces:**
- Consumes: `CommissioningFabric::generate(fabric_id, admin_node_id)`（既存 — ランダム epoch は generate 内で生成済み）、`kvs::write_mat_ipk_epoch`（Task 6）、`cert::MatterCert::parse`、`case::random_p256_secret`、`CommissioningFabric::issue_device_noc`、`fabric::derive_group_session_id` / `derive_ipk_operational` / `compressed_fabric_id`。
- Produces:
  - `KvsTxn::create(path: &Path) -> Result<KvsTxn, KvsError>`（存在時 `KvsError::AlreadyExists`、`[Default]` セクションで初期化、flock 保持）
  - `CommissioningFabric::write_kvs_bootstrap(&self, store: &Path, fabric_index: u8, issuer_index: u8) -> Result<(), KvsError>`
  - CLI `mat fabric init [--fabric-id N] [--admin-node-id N]`（既定: fabric-id 1、admin-node-id 112233 = chip-tool alpha の慣例値。`--fabric-index` / `--issuer-index` は既存グローバル引数を使用）

- [ ] **Step 1: 失敗するテストを書く（mat-controller round-trip）**

`crates/mat-controller/src/commissioning.rs` の `#[cfg(test)]` に追加:

```rust
#[test]
fn bootstrap_roundtrip_via_kvs_readers() {
    let dir = tempfile::tempdir().unwrap();
    let fab = CommissioningFabric::generate(1, 112233).unwrap();
    fab.write_kvs_bootstrap(dir.path(), 1, 0).unwrap();
    // 既存リーダで読み戻せる = chip-tool INI 互換形式の証明
    let m = crate::kvs::read_self_issue_materials(
        &dir.path().join("chip_tool_config.alpha.ini"),
        &dir.path().join("chip_tool_config.ini"),
        1,
        0,
    )
    .unwrap();
    assert_eq!(m.fabric_id, 1);
    assert_eq!(m.node_id, 112233);
    // epoch → operational の導出チェーンが KVS の中身と一致
    let creds = crate::fabric::FabricCredentials::from_self_issued(m.clone()).unwrap();
    let epoch = crate::kvs::read_mat_ipk_epoch(&dir.path().join("chip_tool_config.ini"), 1)
        .unwrap()
        .expect("epoch persisted");
    let cfid = crate::fabric::compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
    assert_eq!(crate::fabric::derive_ipk_operational(&epoch, &cfid), m.ipk_operational);
    // 二重 init は拒否
    assert!(matches!(
        fab.write_kvs_bootstrap(dir.path(), 1, 0),
        Err(crate::kvs::KvsError::AlreadyExists)
    ));
}
```

- [ ] **Step 2: テストが失敗することを確認**

```bash
cargo test -p mat-controller bootstrap_roundtrip 2>&1 | tail -5
```

Expected: コンパイルエラー（`write_kvs_bootstrap` / `AlreadyExists` 未定義）。

- [ ] **Step 3: `KvsTxn::create` + `AlreadyExists` を実装**

`crates/mat-controller/src/kvs.rs`:
- `enum KvsError` に `AlreadyExists` バリアントを追加（Display は `"kvs already exists"` 系）。
- `KvsTxn::create(path)`: `path` が存在すれば `Err(KvsError::AlreadyExists)`。無ければ sidecar `.lock` を作って flock（`open` と同じ手順）→ `lines = vec!["[Default]".to_string()]`、`default_start = 1`、`default_end = 1`、`trailing_newline = true` で `KvsTxn` を組んで返す（`commit` は既存実装がそのまま書き出す）。`open` の flock 部を小さな private fn に括り出して共有する。

- [ ] **Step 4: `serialize_keyset` / `EPOCH_START_TIME` の可視性昇格**

`crates/mat-controller/src/group_settings.rs` の `fn serialize_keyset(...)` と `EPOCH_START_TIME` 定数を `pub(crate)` にする（シグネチャ・実装は無変更。keyset blob は「常に 3 スロット・ゼロ埋め・終端 0xFFFF」の既存規律をそのまま流用するのが狙い）。

- [ ] **Step 5: `write_kvs_bootstrap` を実装**

`crates/mat-controller/src/commissioning.rs` の `impl CommissioningFabric` に追加:

```rust
/// 初回 fabric bootstrap（M8c-3）: この fabric を chip-tool INI 互換 KVS へ
/// 新規永続する。書くもの:
///   alpha ini … ExampleOpCredsCAKey<issuer> = pub65||priv32（97B）
///   main ini  … f/<idx>/r = RCAC(TLV) / f/<idx>/n = admin NOC(TLV)
///               f/<idx>/k/0 = IPK keyset blob（3 スロット、終端 0xFFFF）
///               mat/f/<idx>/ipk-epoch = ランダム epoch（mat 専用キー）
/// 既に KVS があれば `KvsError::AlreadyExists`（上書きしない — 誤 store
/// パスでのサイレント別 fabric 生成を防ぐ、spec ユーザー決定）。
pub fn write_kvs_bootstrap(
    &self,
    store: &std::path::Path,
    fabric_index: u8,
    issuer_index: u8,
) -> Result<(), crate::kvs::KvsError> {
    use crate::kvs::KvsTxn;
    let alpha_path = store.join("chip_tool_config.alpha.ini");
    let main_path = store.join("chip_tool_config.ini");
    // どちらか一方でも実在したら拒否（中途半端な store を悪化させない）。
    if alpha_path.exists() || main_path.exists() {
        return Err(crate::kvs::KvsError::AlreadyExists);
    }

    let rcac = MatterCert::parse(&self.rcac_tlv).map_err(|_| crate::kvs::KvsError::BadNoc {
        fabric_index,
        reason: "generated rcac unparseable (bug)",
    })?;
    let cfid = fabric::compressed_fabric_id(&rcac.pub_key, self.fabric_id);
    let operational = fabric::derive_ipk_operational(&self.ipk_epoch, &cfid);

    // admin NOC: 使い捨て op 鍵で自己発行（f/<idx>/n はリーダが node_id /
    // fabric_id を読むためだけに使う — 実行時の CASE 用 NOC は毎回
    // FabricCredentials::from_self_issued が自己発行するので秘密鍵は捨てる）。
    let op_secret = crate::case::random_p256_secret();
    let op_public = crate::case::p256_public_key_uncompressed(&op_secret);
    let admin_noc = self
        .issue_device_noc(&op_public, self.admin_node_id)
        .map_err(|_| crate::kvs::KvsError::BadNoc {
            fabric_index,
            reason: "admin noc issuance failed (bug)",
        })?;

    // alpha ini
    let mut ca_key = Vec::with_capacity(97);
    ca_key.extend_from_slice(&rcac.pub_key);
    ca_key.extend_from_slice(&self.root_private_key);
    let mut alpha = KvsTxn::create(&alpha_path)?;
    alpha.set(&format!("ExampleOpCredsCAKey{issuer_index}"), &ca_key);
    alpha.commit()?;

    // main ini（1 flock 区間 + 1 commit）
    let gkh = fabric::derive_group_session_id(&operational);
    let mut main = KvsTxn::create(&main_path)?;
    main.set(&format!("f/{fabric_index}/r"), &self.rcac_tlv);
    main.set(&format!("f/{fabric_index}/n"), &admin_noc);
    main.set(
        &format!("f/{fabric_index}/k/0"),
        &crate::group_settings::serialize_keyset(
            0,
            crate::group_settings::EPOCH_START_TIME,
            gkh,
            &operational,
            0xFFFF,
        ),
    );
    main.set(&crate::kvs::mat_ipk_epoch_key(fabric_index), &self.ipk_epoch);
    main.commit()?;
    Ok(())
}
```

※ `case::p256_public_key_uncompressed` が存在しない場合は `crates/mat-controller/src/case.rs` / `cert.rs` を grep し、既存の「secret → 65 バイト uncompressed 公開鍵」変換（`generate_rcac` や自己発行 NOC が内部でやっている処理）を関数に括り出して使う。新しい暗号コードを書かないこと。`BadNoc` の形が違う場合も Task 6 と同様、既存バリアントに合わせる。

- [ ] **Step 6: round-trip テスト通過確認**

```bash
cargo test -p mat-controller bootstrap_roundtrip 2>&1 | tail -5
```

Expected: PASS。

- [ ] **Step 7: CLI + commands/fabric.rs**

`crates/mat/src/cli.rs` の `Command` enum に追加（既存サブコマンドの流儀に合わせる）:

```rust
/// fabric 管理（初回 bootstrap）。
Fabric {
    #[command(subcommand)]
    action: FabricAction,
},
```

```rust
#[derive(Subcommand)]
pub enum FabricAction {
    /// 初回 fabric bootstrap: root CA + ランダム epoch IPK を生成し KVS を新規作成
    Init {
        /// fabric id（既定 1）
        #[arg(long, default_value_t = 1)]
        fabric_id: u64,
        /// controller 自身の admin node id（既定 112233 = chip-tool 慣例値）
        #[arg(long, default_value_t = 112_233)]
        admin_node_id: u64,
    },
}
```

`crates/mat/src/commands/fabric.rs`:

```rust
//! `mat fabric init` — 初回 fabric bootstrap（M8c-3）。直経路のみ・
//! ネットワーク未接触（KVS ローカル生成だけ）。iface 解決より前に
//! dispatch される（main.rs 参照）。

use std::path::Path;
use std::process::ExitCode;

use mat_controller::commissioning::CommissioningFabric;
use mat_core::error::{ErrorKind, MatError};

pub fn run_init(store_path: &Path, fabric_id: u64, admin_node_id: u64, fabric_index: u8, issuer_index: u8) -> ExitCode {
    let fab = match CommissioningFabric::generate(fabric_id, admin_node_id) {
        Ok(f) => f,
        Err(e) => {
            let err = MatError::new(ErrorKind::Other, format!("fabric generate: {e}"));
            err.emit();
            return ExitCode::from(err.kind.exit_code());
        }
    };
    if let Err(e) = fab.write_kvs_bootstrap(store_path, fabric_index, issuer_index) {
        let kind = match e {
            mat_controller::kvs::KvsError::AlreadyExists => ErrorKind::Other,
            _ => ErrorKind::StoreParse,
        };
        let err = MatError::new(
            kind,
            format!("fabric init: {e} (store: {}; 既存 KVS の上書きはしない — 再初期化は両 ini を手動削除)", store_path.display()),
        );
        err.emit();
        return ExitCode::from(err.kind.exit_code());
    }
    tracing::info!("fabric bootstrap written (native kvs)");
    // 出力 JSON（スキーマ: timestamp 必須、実値サンプルはドキュメントに書かない）
    let rcac = mat_controller::cert::MatterCert::parse(&fab.rcac_tlv).expect("generated rcac parses");
    let cfid = mat_controller::fabric::compressed_fabric_id(&rcac.pub_key, fab.fabric_id);
    let out = serde_json::json!({
        "timestamp": mat_core::now_iso8601(),
        "store": store_path.display().to_string(),
        "fabric_id": fab.fabric_id,
        "fabric_index": fabric_index,
        "compressed_fabric_id": format!("{:016X}", u64::from_be_bytes(cfid)),
        "admin_node_id": fab.admin_node_id,
    });
    println!("{out}");
    ExitCode::SUCCESS
}
```

※ `mat_core::now_iso8601()` は既存の timestamp 生成ヘルパ名に合わせる（`grep -rn "timestamp" crates/mat/src/commands/read.rs` で現行コマンドの生成方法を確認して同じものを使う）。`commands/mod.rs` に `pub mod fabric;` 追加。

`crates/mat/src/main.rs`: **iface 解決（Task 4 のブロック）より前**、matd dispatch よりも前に処理する（bootstrap はネットワーク・backend 不要、autodetect エラーに巻き込まない）:

```rust
if let Command::Fabric { action } = &command {
    let cli::FabricAction::Init { fabric_id, admin_node_id } = action;
    return commands::fabric::run_init(&store_path, *fabric_id, *admin_node_id, args.fabric_index, args.issuer_index);
}
```

※ store_path の解決は既存の共通処理を通った後に置く（`store_missing` チェックより前 — init は store ディレクトリが無ければ作る側。`std::fs::create_dir_all(&store_path)` を `run_init` 冒頭に追加し、失敗は `store_missing` で emit）。

- [ ] **Step 8: バイナリレベルのローカル完結フル E2E テスト**

`crates/mat/tests/integration.rs` に追加:

```rust
#[test]
fn fabric_init_full_local_cycle() {
    let dir = tempfile::tempdir().unwrap();
    // 1) init 成功: JSON スキーマ検証
    let out = mat(dir.path()).args(["fabric", "init"]).output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["timestamp"].is_string());
    assert_eq!(v["fabric_id"], 1);
    assert_eq!(v["admin_node_id"], 112233);
    assert!(v["compressed_fabric_id"].as_str().unwrap().len() == 16);
    // 2) KVS 2 ファイル + epoch キーが生成されている
    let main_ini = std::fs::read_to_string(dir.path().join("chip_tool_config.ini")).unwrap();
    assert!(main_ini.contains("mat/f/1/ipk-epoch"));
    assert!(std::fs::metadata(dir.path().join("chip_tool_config.alpha.ini")).is_ok());
    // 3) 再 init は拒否（exit 1、error JSON に kind: other）
    let out2 = mat(dir.path()).args(["fabric", "init"]).output().unwrap();
    assert_eq!(out2.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out2.stderr).contains("\"other\""));
}
```

- [ ] **Step 9: テスト通過確認**

```bash
cargo test -p mat -p mat-controller 2>&1 | tail -5
```

Expected: 全 PASS。

- [ ] **Step 10: Commit**

```bash
cargo fmt
git add -A crates/mat crates/mat-controller
git commit -m "feat: mat fabric init（初回 bootstrap、ランダム epoch IPK 永続）（M8c-3 Task8）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: mat の chip-tool 撤去（runner / 分岐 / MAT_CHIP_TOOL_BIN）

**Files:**
- Delete: `crates/mat/src/runner.rs`
- Modify: `crates/mat/src/main.rs`（chip-tool フォールスルーの削除）
- Modify: `crates/mat/src/native_direct.rs`（`Fallback` / `None` 契約 → ハードエラー化）
- Modify: `crates/mat/src/commands/*.rs`（read / write / invoke / describe / discover / commission / open_window / diag / group — chip-tool 分岐の削除）
- Modify: `crates/mat-core/src/error.rs`（`ChildNotFound` / `ChildFailed` に「0.22.0 以降 emit されない（wire 互換のため残置）」の doc コメント）
- Modify（縮小）: `crates/mat-core/src/normalize.rs` 等の chip-tool 出力パーサ（native からも使う関数は残す — 削除前に `grep -rn "normalize::" crates/` で参照を確認）

**Interfaces:**
- Consumes: Task 3 のテスト基盤（chip-tool 非依存）— このタスクでテスト改修は原則不要。
- Produces: `mat` は chip-tool を一切 spawn しない。フォールバックだった分岐は次の写像でハードエラー化:
  - エンジン構築失敗: `KvsError::Io(NotFound)` → `store_missing`（detail に「run `mat fabric init`」誘導）/ その他 KvsError → `store_parse`
  - 汎用 op の名前未解決（name→ID テーブル外）→ `parse_error`（detail: unknown cluster/attribute/command name; 数値 ID は従来どおり受理）
  - commission の `CommissionAttempt::Unavailable` 撤廃 → 発見空振り（mDNS/BLE miss、manual code 0 件）= `unreachable`、KVS/資材系 = `store_missing`/`store_parse`（Task 6 で epoch 系は済）

- [ ] **Step 1: 撤去対象の洗い出し**

```bash
grep -rn "runner::\|ChipTool\|MAT_CHIP_TOOL_BIN" crates/mat/src/ | grep -v native_direct | head -40
grep -n "Fallback\|falling back" crates/mat/src/native_direct.rs | head -20
```

- [ ] **Step 2: native_direct の契約変更**

`crates/mat/src/native_direct.rs`:
- `try_run` の「`None` = chip-tool 直で実行すべき」「`RunResult::Fallback`」契約を廃止し、**全対象 op を native で実行して `Result` を返す** `run(&command, &store_path, cfg) -> Option<Result<(), MatError>>` に変える（`None` は「この op は native_direct の担当外」= discover/commission/fabric など専用コマンド層を持つ op のみ）。
- エンジン構築失敗の warn + `None` を上記写像のハードエラーに変更。`falling back to chip-tool` の warn 文言はこの crate から全削除。
- 名前未解決（`classify` が `None` を返していた汎用形）→ `parse_error` エラーを返す。

- [ ] **Step 3: main.rs / commands の chip-tool 分岐削除**

- `main.rs`: `mod runner;` 削除、native 実行後の chip-tool フォールスルー `match &command {...}`（commands::xxx::run の呼び出し群）は、**バックエンド不要の責務（台帳更新・alias・discover/commission の native 呼び出し・JSON 合成）だけを残して** chip-tool 実行部を消す。native_direct が担う op は native_direct の結果をそのまま返す。
- `commands/discover.rs`: native browse 一本化（avahi は Task 11 で処理、chip-tool 分岐のみ削除）。
- `commands/commission.rs`: `Unavailable` 分岐（chip-tool フォールバック）を Step 2 の写像でエラー化。`mat-native/src/commission.rs` 側も `CommissionAttempt::Unavailable` バリアントを削除し、型で表現（`Err(MatError)` へ）。
- `commands/open_window.rs` / `diag.rs` / `group.rs` / read / write / invoke / describe: chip-tool 実行・出力パース部を削除。
- `crates/mat/src/runner.rs` を `git rm`。

- [ ] **Step 4: ビルド + テスト**

```bash
cargo build -p mat 2>&1 | tail -5
cargo test -p mat 2>&1 | tail -5
grep -rn "chip-tool\|chip_tool" crates/mat/src/ | grep -v "chip_tool_config\|// \|//!" | wc -l
```

Expected: ビルド・テスト PASS。grep はほぼ 0（`chip_tool_config.*.ini` のファイル名と歴史的コメントのみ許容）。

- [ ] **Step 5: スモーク（撤去の実感確認）**

```bash
MAT_STORE=$(mktemp -d) MAT_IFACE=lo cargo run -p mat -- read 1 1 onoff on-off 2>&1 | tail -3
```

Expected: `store_missing`（exit 10、detail に `mat fabric init` 誘導）。exit 12 が二度と出ないこと。

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A crates/mat crates/mat-core crates/mat-native
git commit -m "feat(mat)!: chip-tool 経路を完全撤去（native 一本化）（M8c-3 Task9）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 10: matd の chip-tool 撤去

**Files:**
- Modify: `crates/matd/src/backend.rs`（chip-tool ランナー削除）
- Modify: `crates/matd/src/server.rs`（フォールバック分岐 → per-op エラー）
- Modify: `crates/matd/src/main.rs`（native backend 構築失敗 = 起動時 warn + per-op エラー運転 or 起動拒否 — **KVS 不在でも matd は起動する**（後から `mat fabric init` できるよう per-op `store_missing` を返す）。iface 曖昧の起動拒否（Task 5）は維持）

**Interfaces:**
- Produces: matd は chip-tool を一切 spawn しない。native backend 未構築時の全 op は `store_missing`/`store_parse` エラー応答（プロトコルの wire 形式は無変更 — エラー kind はプロトコルに既に流れる形）。

- [ ] **Step 1: 撤去対象の洗い出し**

```bash
grep -n "chip.tool\|ChipTool\|fall" crates/matd/src/backend.rs crates/matd/src/server.rs | head -30
```

- [ ] **Step 2: backend.rs / server.rs の書き換え**

- `backend.rs` の chip-tool 実行部（spawn・出力パース・`MAT_CHIP_TOOL_BIN`）を削除。native backend の呼び出しだけ残す。
- `server.rs` の「native 不可 → chip-tool で処理」分岐を「native 不可 → 該当 op にエラー応答」へ。エンジン未構築（起動時の構築失敗）はリクエスト毎に `store_missing`/`store_parse` を返す（応答フォーマットは既存のエラー応答と同じ）。
- `falling back to chip-tool` 文言を matd から全削除。

- [ ] **Step 3: ビルド + テスト**

```bash
cargo build -p matd && cargo test -p matd 2>&1 | tail -5
grep -rn "chip-tool\|chip_tool" crates/matd/src/ | grep -v "chip_tool_config\|// \|//!" | wc -l   # → 0 目標
```

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add -A crates/matd
git commit -m "feat(matd)!: chip-tool 経路を完全撤去（M8c-3 Task10）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 11: avahi-browse 撤去

**Files:**
- Modify: `crates/mat/src/probe.rs`（avahi フォールバック削除 — mDNS I/O エラーはそのままエラー）
- Modify: `crates/mat/src/commands/discover.rs` / `crates/mat/src/commands/diag.rs`（avahi 分岐削除）
- Modify: `crates/mat/src/cli.rs`（avahi 言及の doc 更新）
- Modify（縮小）: `crates/mat-core/src/diag.rs`（avahi 出力パーサ — native からの参照が無ければ削除。`grep -rn "diag::parse" crates/` で確認）
- Modify: `crates/mat-controller/src/dnssd.rs`（avahi はコメント言及のみのはず — 確認して整合）

**Interfaces:**
- Produces: mDNS はすべて `mat-controller::dnssd`。avahi-browse のプロセス起動が 0 箇所。M8b の規則「0 件はフォールバックしない」は「I/O エラーもフォールバック先が無い」に単純化。

- [ ] **Step 1: 洗い出し + 削除**

```bash
grep -rln "avahi" crates/ --include="*.rs" | grep -v target
```

各ファイルの avahi 分岐（spawn・パース・フォールバック）を削除。I/O エラーは従来のエラー分類（`other` / `unreachable`）で emit。

- [ ] **Step 2: ビルド + テスト + Commit**

```bash
cargo build 2>&1 | tail -3 && cargo test -p mat -p mat-core 2>&1 | tail -5
grep -rn "avahi" crates/ --include="*.rs" | grep -v target | wc -l   # → 0（歴史的コメント除く）
cargo fmt
git add -A crates
git commit -m "feat: avahi-browse フォールバック撤去（mDNS は dnssd 一本）（M8c-3 Task11）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 12: Docker / Taskfile / Cross の gnu 一本化 + ドキュメント

**Files:**
- Modify: `Dockerfile`（chip-tool ビルド/焼き込みステージ削除、`MAT_CHIP_TOOL_BIN` env 削除、runtime 依存から avahi/libssl 等 chip-tool 由来を削除）
- Modify: `Taskfile.yml`（chiptool 系タスク削除、`dist:arm64` を cross gnu + `--features ble` に変更、`run` の desc 更新）
- Modify: `Cross.toml`（コメント更新 — gnu + ble が deploy 標準であることを明記）
- Modify: `README.md` / `ARCHITECTURE.md` / `CLAUDE.md`
- Modify: `scripts/e2e-m2.sh` 等の旧ハーネス（冒頭に歴史的アーカイブ注記）

**Interfaces:**
- Produces: deploy 標準 = `cross build --release --target aarch64-unknown-linux-gnu --features ble`。musl deploy 経路の記述は削除（ローカル `task check` は host build のまま）。

- [ ] **Step 1: Dockerfile**

- `chip-builder` / `chip-builder-arm64` ステージと `COPY --from=chip-builder` / `ENV MAT_CHIP_TOOL_BIN` を削除。
- runtime ステージの依存を mat/matd の実際の動的依存だけに縮小（native は pure Rust — `ldd target/release/mat` で確認し、不要な `libavahi-client3` 等を削る）。
- arm64 クロスビルドステージは Rust クロスだけ残す。

- [ ] **Step 2: Taskfile**

- `chiptool:build` / `chiptool:arm64` 系タスク（58–87 行）を削除。
- `dist:arm64`（相当タスク）を書き換え:

```yaml
  dist:arm64:
    desc: "aarch64 gnu + BLE の deploy 成果物を ./dist/arm64/ に作る（jarvis 用標準ビルド）"
    cmds:
      - cross build --release --target aarch64-unknown-linux-gnu --features ble
      - mkdir -p dist/arm64
      - cp target/aarch64-unknown-linux-gnu/release/mat target/aarch64-unknown-linux-gnu/release/matd dist/arm64/
      - 'echo "実機ランタイム依存: libdbus-1-3（BLE 用）。scp dist/arm64/{mat,matd} <host>:~/"'
```

- `run` タスクの desc から「chip-tool が PATH 上に必要」を削除、`test` の desc から「ダミー chip-tool」を削除。

- [ ] **Step 3: 旧 E2E スクリプトへ歴史的注記**

`scripts/e2e-m2.sh` 〜 `scripts/e2e-m8c2-real.sh`（chip-tool 前提のもの全部）の冒頭 2 行目に追記:

```bash
# [M8c-3] chip-tool 撤去済みのため 0.22.0 以降では動かない（歴史的アーカイブ。
# 動かすなら git tag の 0.21.0 時点を checkout）。現行ハーネスは e2e-m8c3-real.sh。
```

Taskfile に対応タスクがあれば削除（`e2e:m8c3:real` だけ残す）。

- [ ] **Step 4: ドキュメント**

- **README.md**: Backend 節を全面書き換え（native 一本、iface 自動検出と `MAT_IFACE` 上書き、`mat fabric init`、epoch 採用永続）。環境変数表から `MAT_CHIP_TOOL_BIN` 削除。エラー表: exit 12 を「0.22.0 で廃止（歴史的欠番）」に。汎用 write/invoke の「scalar のみ（list/struct/float は parse_error、数値 ID は可）」を仕様として明記。`child_not_found`/`child_failed` kind の説明も「0.22.0 以降 emit されない」に更新。
- **ARCHITECTURE.md**: M8c-3 完了記録（実装済み内容 + 実機 E2E 結果は Task 13 後に追記）。「将来候補」として fake Matter デバイス（UDP loopback responder テスト基盤）/ 汎用 list/struct TLV エンコード / IPK ローテーションを記録。
- **CLAUDE.md**: Backend 節を書き換え（chip-tool 前提の記述を native 前提に。設計ルール 1–4 の本文は維持、「chip-tool」への言及を整理。スコープリマインダは不変）。

- [ ] **Step 5: 検証 + Commit**

```bash
task check 2>&1 | tail -5
docker build -t mat-test . 2>&1 | tail -3   # Docker が使える環境なら
grep -rn "MAT_CHIP_TOOL_BIN" . --include="*.rs" --include="*.yml" --include="Dockerfile" | grep -v scripts/ | grep -v docs/ | wc -l   # → 0
git add -A
git commit -m "chore!: Docker/Taskfile gnu一本化 + chip-tool 痕跡の撤去 + docs（M8c-3 Task12）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 13: 実機 E2E ゲート 2（最終受け入れ、★CHECKPOINT）

**Files:**
- Modify: `scripts/e2e-m8c3-real.sh`（STAGE=2 部分の実装）

**Interfaces:**
- Consumes: Task 8–12 の成果すべて。
- Produces: M8c-3 の最終合否。

- [ ] **Step 1: STAGE=2 検証部の実装**

**STAGE=2 の検証項目**（spec「実機 E2E ゲート 2」）:

1. 準備: **gnu + ble クロスビルド**（`task dist:arm64` = cross。M6b/M8c-1 の Cross.toml 経路）→ scp → matd 停止（trap で復帰）→ KVS バックアップ。
2. **chip-tool 不在での全 op スイープ**: リモート側で `PATH` から chip-tool を外した環境（`env PATH=/usr/bin:/bin` 等、chip-tool の実在しない PATH）で STAGE=1 と同じ op スイープ + matd 経路を再実行、全合格。`falling back` grep は「文言がバイナリから消えている」ことも確認: `! grep -q "falling back to chip-tool" dist/arm64/mat`。
3. **fabric init 実機検証**（実運用 fabric は無傷で維持）: 別 store（`mktemp -d`）で `mat fabric init` → JSON 確認 → 既存 fabric 側から対象ノードに `mat open-window` → 新 store で `mat commission`（on-network、open-window の manual code）→ 新 fabric で `mat read` 成功 → 新 fabric の RemoveFabric（`mat invoke ... operationalcredentials remove-fabric` 相当）で掃除 → 元 fabric から read が引き続き成功。
4. **BLE 経路**: gnu+ble バイナリで BLE commission（M8c-1 E2E と同じ対象。デバイス都合が悪ければ WARN + 人力確認へ切替可）。
5. `task check` 全通過（ローカル）+ Docker イメージビルド成功。

- [ ] **Step 2: 構文チェック + Commit**

```bash
bash -n scripts/e2e-m8c3-real.sh
git add scripts/e2e-m8c3-real.sh
git commit -m "test(e2e): M8c-3 STAGE=2（最終受け入れ）ハーネス（M8c-3 Task13）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

- [ ] **Step 3: ★実機ゲート 2 実行（ユーザーと実施）**

```bash
STAGE=2 task e2e:m8c3:real
task check 2>&1 | tail -5
```

全 GREEN → ARCHITECTURE.md に実機 E2E 結果を追記して commit（`docs: M8c-3 実機 E2E 合格を記録`）。

## 完了後（plan の範囲外、ユーザーと実施）

1. superpowers:finishing-a-development-branch で main へのマージ方式を確認。
2. 本番デプロイ（0.22.0、現行本番 0.19.0）はユーザー判断 — deploy するなら `task dist:arm64` の gnu+ble 成果物を jarvis へ（メモリの scp / systemd 手順）。**注意: 0.20/0.21 を跨ぐため、matd unit の env（`MAT_MATD_IFACE` 等）はそのままで動くが、初回 native commission で epoch 採用永続が KVS に書かれる。**
3. auto-memory の Phase 5 記録更新（M8c-3 完了、chip-tool 退役、Phase 5 完了の節目）。
