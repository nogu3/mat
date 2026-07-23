# post-1.0 defer 品質修正 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** v1 最終レビューで defer した品質 3 項目（防御的 `unreachable!` ×6 の typed error 化、`From<MatError> for String` 削除、`to_op` エラーの kind/exit 不一致解消）を実装する。

**Architecture:** spec は `docs/superpowers/specs/2026-07-23-post-v1-defer-quality-fixes-design.md`。v1 Task6 と同じ規律（dispatch 不変条件が破れても panic せず `MatError::parse_error`、detail は `internal: ` prefix）を 6 箇所へ展開し、`mat/src/matd_client.rs` に `ToOpError` enum（`Unsupported` / `Mat(MatError)`）を新設して `to_op` のエラー 2 種を分離する。`From<MatError> for String` は唯一の利用者が消えるため mat-core から削除。

**Tech Stack:** Rust workspace（crates: mat / matd / mat-core）。テストは既存の `#[tokio::test]` + `FakeEstablisher` パターン（`crates/matd/src/server.rs` の tests モジュール）と `matd_client.rs` の tests モジュール。

## Global Constraints

- 各タスク完了時に `task check`（fmt:check + clippy `-D warnings` + test）green であること。
- stdout は純粋な構造化 JSON のみ、エラーは stderr に `{"error":{"kind","detail"}}`（CLAUDE.md）。
- 観測可能な挙動変更は spec C 節の 1 系統のみ（forced `--matd` + to_op 内実エラーが固有 kind/exit 化）。**非対応 op の exit 2 は維持**（意図的例外としてピン留め）。
- コミットメッセージは Conventional Commits・日本語・命令形・件名 ≤50 文字。各コミット末尾に以下の trailer を付ける:
  ```
  Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01D1nMZo3ak5uZh4ibnmTrpR
  ```

---

### Task 1: matd server.rs の防御的 unreachable! ×4 を typed error 化

**Files:**
- Modify: `crates/matd/src/server.rs:714`（`native_op` Write 腕の `WriteClass::NotNative`）
- Modify: `crates/matd/src/server.rs:746`（`native_op` Invoke 腕の `InvokeClass::NotNative`）
- Modify: `crates/matd/src/server.rs:772`（`native_op` catch-all `_`）
- Modify: `crates/matd/src/server.rs:800`（`group_provision` の let-else）
- Test: `crates/matd/src/server.rs` 末尾の `mod tests` に追記

**Interfaces:**
- Consumes: 既存の `native_op(op, native, store_path) -> Result<Value, MatError>` / `group_provision(op, native, store_path) -> Result<Value, MatError>`、tests の `FakeEstablisher` / `store_with_node_5()` フィクスチャ（`server.rs:1190` 付近に既存）。
- Produces: シグネチャ変更なし。panic していた 4 経路が `MatError::parse_error`（detail は `internal: ` prefix）を返すようになる。

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/server.rs` の `mod tests` 内、既存の `native_generic_read_body_matches_expected_schema` の近くに追加:

```rust
    /// post-1.0 defer: dispatch 不変条件が破れても panic しない（v1 Task6 規律）。
    #[tokio::test]
    async fn native_op_invariant_violations_are_typed_errors_not_panics() {
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let store = store_with_node_5();

        // NotNative write（未知 cluster 名 → classify_write が NotNative）
        let op = Op::Write {
            node_id: 5,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            attribute: "x".into(),
            value: "1".into(),
        };
        let err = native_op(&op, &native, store.path()).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);

        // NotNative invoke
        let op = Op::Invoke {
            node_id: 5,
            endpoint: 1,
            cluster: "nosuchcluster".into(),
            command: "x".into(),
            args: vec![],
        };
        let err = native_op(&op, &native, store.path()).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);

        // non-hotpath op（Ping は node_id() が None なので require_node を素通りして
        // catch-all に到達する）
        let err = native_op(&Op::Ping, &native, store.path())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);
    }

    #[tokio::test]
    async fn group_provision_rejects_non_group_provision_op_without_panic() {
        let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let dir = tempfile::tempdir().unwrap();
        let err = group_provision(&Op::Ping, &native, dir.path())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        assert!(err.detail.starts_with("internal:"), "detail={}", err.detail);
    }
```

注: `ErrorKind` / `FakeEstablisher` / `store_with_node_5` は tests モジュール内で既に使える（`use super::*` + 既存 use）。`ErrorKind` が未 import なら `use mat_core::error::ErrorKind;` を tests の use 群に足す。

- [ ] **Step 2: テストが失敗（panic）することを確認**

Run: `cargo test -p matd --lib invariant_violations`
Expected: FAIL — `native_op_invariant_violations_are_typed_errors_not_panics` が `unreachable!` panic で落ちる（`group_provision_rejects...` も同様）。

- [ ] **Step 3: 4 箇所を typed error に置換**

`server.rs:714`（Write 腕）:

```rust
            mat_core::ids::WriteClass::NotNative => Err(MatError::parse_error(
                "internal: NotNative write reached native_op (is_native_hotpath invariant violated)",
            )),
```

`server.rs:746`（Invoke 腕）:

```rust
            mat_core::ids::InvokeClass::NotNative => Err(MatError::parse_error(
                "internal: NotNative invoke reached native_op (is_native_hotpath invariant violated)",
            )),
```

`server.rs:772`（catch-all）:

```rust
        _ => Err(MatError::parse_error(
            "internal: native_op called with non-hotpath op (dispatch invariant violated)",
        )),
```

`server.rs:800`（let-else）:

```rust
    else {
        return Err(MatError::parse_error(
            "internal: group_provision called with non-GroupProvision op (dispatch invariant violated)",
        ));
    };
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd --lib`
Expected: PASS（新規 2 テスト含む全テスト green）。

- [ ] **Step 5: `task check` を回してコミット**

Run: `task check`
Expected: fmt:check / clippy / test すべて green。

```bash
git add crates/matd/src/server.rs
git commit -m "refactor(matd): native_op/group_provision の unreachable! を typed error 化

dispatch 不変条件が破れても panic せず parse_error を返す
（v1 Task6 の alias.rs id() / Op::Read expect と同じ規律）。
内部バグ経路のみで通常到達不能、既存挙動の変更なし。"
```

（trailer は Global Constraints のとおり付ける。）

---

### Task 2: mat 側の同型 dispatch invariant 2 箇所を typed error 化

**Files:**
- Modify: `crates/mat/src/main.rs:21`（import に `MatError` 追加）と `crates/mat/src/main.rs:180-182`（route dispatch catch-all）
- Modify: `crates/mat/src/matd_client.rs:487-489`（`dispatch_listen` の let-else）

**Interfaces:**
- Consumes: `MatError::parse_error` / `MatError::emit` / `ErrorKind::exit_code`（mat-core、既存）。
- Produces: シグネチャ変更なし。main.rs の catch-all は `Err(MatError)` を返して既存の emit + exit 経路（`main.rs:185-192`）へ流れる。`dispatch_listen` は emit して `ExitCode::from(1)` 相当を返す。

いずれも到達には内部バグ（route dispatch の網羅漏れ）が必要で、`main()` / `ExitCode` 戻り値（`PartialEq` 非実装）の制約から単体テスト不能。コンパイル + clippy + 既存テストで担保する（spec テスト節も対象外としている）。

- [ ] **Step 1: main.rs の import と catch-all を書き換え**

`main.rs:21`:

```rust
use mat_core::error::{ErrorKind, MatError};
```

`main.rs:178-183`（既存コメントは保持し、`unreachable!` のみ置換）:

```rust
        // 他の全 op は native_direct::run が `Some` を返して上で処理済み。
        // Command::Fabric は route dispatch より前の早期 return で処理済み。
        // 不変条件が破れても panic せず typed error（v1 Task6 と同じ規律）。
        _ => Err(MatError::parse_error(
            "internal: op not handled by native_direct::run (route dispatch invariant violated)",
        )),
```

- [ ] **Step 2: dispatch_listen の let-else を書き換え**

`matd_client.rs:487-489`:

```rust
    else {
        // 内部バグ経路: 非 Listen command が来ても panic しない（v1 Task6 規律）。
        let e = MatError::parse_error(
            "internal: dispatch_listen called with non-Listen command (dispatch invariant violated)",
        );
        e.emit();
        return ExitCode::from(e.kind.exit_code());
    };
```

- [ ] **Step 3: `task check` を回してコミット**

Run: `task check`
Expected: すべて green（挙動変更なし、既存テストのみ）。

```bash
git add crates/mat/src/main.rs crates/mat/src/matd_client.rs
git commit -m "refactor(mat): route dispatch invariant の unreachable! を typed error 化

main.rs catch-all と dispatch_listen let-else が対象。matd 側
（前コミット）と合わせ、防御的 unreachable! はゼロになり
局所証明可能な 2 件（clap 排他 / 3-bit mask）のみ残る。"
```

（trailer は Global Constraints のとおり付ける。）

---

### Task 3: ToOpError 分離 + From<MatError> for String 削除

**Files:**
- Modify: `crates/mat/src/matd_client.rs`（`ToOpError` 新設、`to_op` シグネチャ変更、`unsupported()`、`dispatch` / `dispatch_auto`、group color の `.map_err(|e| e.detail)?` → `?`、tests 追記）
- Modify: `crates/mat-core/src/error.rs:132-139`（`impl From<MatError> for String` 削除）

**Interfaces:**
- Consumes: `to_op` の呼び出し元は `dispatch`（`matd_client.rs:99`）と `dispatch_auto`（`:133`）と tests のみ（grep 確認済み）。alias `id()` は `Result<u64, MatError>`（v1 Task5）。
- Produces: `fn to_op(command: &Command) -> Result<Value, ToOpError>`（private）。`ToOpError` は `#[derive(Debug)] enum { Unsupported(String), Mat(MatError) }` + `impl From<MatError> for ToOpError`。既存テストの `to_op(&cmd).unwrap()` / `.is_err()` は Debug derive によりそのままコンパイルが通る。

- [ ] **Step 1: 失敗するテストを書く**

`matd_client.rs` の `mod tests` に追加（`discover_and_commission_are_unsupported` の近く）:

```rust
    /// post-1.0 defer: to_op のエラー 2 種の分離をピン留め。
    /// - 非対応 op = Unsupported → forced dispatch は kind=other + exit 2 の
    ///   意図的例外（「2 = CLI 引数エラー」の documented シグナル維持）。
    /// - alias 解決失敗 = Mat → 固有 kind / exit（ここでは Other = exit 1）。
    #[test]
    fn to_op_separates_unsupported_from_real_errors() {
        match to_op(&Command::Discover { probe: false }) {
            Err(ToOpError::Unsupported(msg)) => assert!(msg.contains("discover")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
        let cmd = Command::On {
            node_id: NodeRef::Alias("kitchen".into()),
            endpoint: EndpointRef::Id(1),
        };
        match to_op(&cmd) {
            Err(ToOpError::Mat(e)) => {
                assert_eq!(e.kind, ErrorKind::Other);
                assert!(e.detail.contains("kitchen"), "detail={}", e.detail);
            }
            other => panic!("expected Mat, got {other:?}"),
        }
    }
```

注: `NodeRef::Alias` は `mat_core::alias::NodeRef` の variant（`alias.rs` マクロ生成、未解決 alias の `id()` は `ErrorKind::Other`）。tests の use に `EndpointRef` が無ければ追加（`use mat_core::alias::{EndpointRef, GroupRef, NodeRef};` は既存）。

- [ ] **Step 2: コンパイルが失敗することを確認**

Run: `cargo test -p mat --lib to_op_separates`
Expected: FAIL — `ToOpError` 未定義のコンパイルエラー。

- [ ] **Step 3: ToOpError を実装し to_op / unsupported を変更**

`matd_client.rs` の `to_op` 直前に追加:

```rust
/// `to_op` のエラー。matd 非対応 op（CLI 利用誤り）と、alias / color spec
/// 解決失敗など固有 kind を持つ実エラーを区別する（post-1.0 defer:
/// kind=other / exit 2 不一致の解消）。
#[derive(Debug)]
enum ToOpError {
    /// matd 非対応サブコマンド。forced では kind=other + exit 2 の意図的例外。
    Unsupported(String),
    /// 固有 kind を持つ実エラー。forced では kind どおりの exit を返す。
    Mat(MatError),
}

/// `id()?` / `resolve_spec(...)?` を to_op 内でそのまま流すための変換。
impl From<MatError> for ToOpError {
    fn from(e: MatError) -> Self {
        ToOpError::Mat(e)
    }
}
```

`to_op` のシグネチャと doc comment を変更:

```rust
/// サブコマンドを matd の op JSON に変換する。matd 非対応のものは
/// `Unsupported`、alias / color spec 解決失敗は固有 kind を保った `Mat`。
fn to_op(command: &Command) -> Result<Value, ToOpError> {
```

`unsupported()` を変更:

```rust
fn unsupported(name: &str) -> ToOpError {
    ToOpError::Unsupported(format!(
        "`mat --matd` does not support the `{name}` subcommand; run it without --matd (direct native path)"
    ))
}
```

group color の kind 落とし（`matd_client.rs:325-331`）を修正 — `.map_err(|e| e.detail)?` を `?` に:

```rust
                let c = mat_core::color::resolve_spec(
                    spec.name.as_deref(),
                    spec.rgb.as_deref(),
                    spec.hue,
                    spec.sat,
                )?;
```

- [ ] **Step 4: dispatch / dispatch_auto の分岐を変更**

`dispatch`（`matd_client.rs:99-105`）:

```rust
    let op = match to_op(command) {
        Ok(op) => op,
        // 非対応 op は CLI 利用誤り。kind=other(exit_code()=1) だが exit 2 を
        // 返すのは「2 = CLI 引数エラー」の documented シグナルを保つ意図的な
        // 例外（spec B 節、テストでピン留め）。
        Err(ToOpError::Unsupported(detail)) => {
            emit_error(ErrorKind::Other, &detail);
            return ExitCode::from(2);
        }
        // alias / color spec 解決失敗など実エラーは固有 kind / exit を返す。
        Err(ToOpError::Mat(e)) => {
            e.emit();
            return ExitCode::from(e.kind.exit_code());
        }
    };
```

`dispatch_auto`（`matd_client.rs:131-133`）:

```rust
    // matd 非対応 op（discover / commission / open-window / diag）は probe せず直経路。
    let op = match to_op(command) {
        Ok(op) => op,
        Err(ToOpError::Unsupported(_)) => return None,
        // 実エラーは直経路でも同じ解決関数が同じエラーで失敗する（決定的）。
        // ここで emit して解決 2 回を 1 回に短縮（stderr / exit は直経路と同一）。
        Err(ToOpError::Mat(e)) => {
            e.emit();
            return Some(ExitCode::from(e.kind.exit_code()));
        }
    };
```

- [ ] **Step 5: mat-core から From<MatError> for String を削除**

`crates/mat-core/src/error.rs:132-139` の以下を丸ごと削除:

```rust
/// `Result<_, String>` の関数内で `MatError` を `?` で流すための縮退変換
/// （matd_client::to_op が使う）。kind は落ち detail のみ残る — 内部バグ経路
/// 専用で、通常経路では発生しない。
impl From<MatError> for String {
    fn from(e: MatError) -> Self {
        e.detail
    }
}
```

- [ ] **Step 6: テストが通ることを確認**

Run: `cargo test -p mat --lib && cargo test -p mat-core`
Expected: PASS。workspace 全体のコンパイルが通ること自体が「From impl の暗黙利用者は to_op だけだった」ことの証明（他に利用者がいればここでコンパイルエラー）。

- [ ] **Step 7: `task check` を回してコミット**

Run: `task check`
Expected: すべて green。

```bash
git add crates/mat/src/matd_client.rs crates/mat-core/src/error.rs
git commit -m "fix(mat): to_op エラーを ToOpError で分離、kind/exit 不一致解消

forced --matd で alias / color spec 解決失敗が kind=other + exit 2 に
丸められていたのを固有 kind / exit に修正（非対応 op の exit 2 は
documented シグナルとして維持・ピン留め）。唯一の利用者が消えた
From<MatError> for String（kind を落とす footgun）を mat-core から削除。"
```

（trailer は Global Constraints のとおり付ける。）

---

## 完了後

- `task check` green を最終確認。
- superpowers:finishing-a-development-branch でブランチ統合（レビュー → main へ）。
- メモリ `mat-v1-roadmap` の post-1.0 候補 3 件を完了に更新。
