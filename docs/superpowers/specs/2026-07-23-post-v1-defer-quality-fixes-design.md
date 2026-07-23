# post-1.0 defer 品質修正（防御的 unreachable! 一掃 + to_op エラー型分離）設計

- 日付: 2026-07-23
- 対象: v1（1.0.0）最終レビューで defer した品質 3 項目
  1. matd server.rs の残 `unreachable!` ×4 の typed error 化
  2. `From<MatError> for String` の workspace footgun 解消
  3. dispatch `to_op` エラーの kind=other / exit 2 不一致の解消
- 方針決定: 項目 2・3 は同根（`From<MatError> for String` の唯一の利用者が
  `to_op` の `id()?`）のため、`to_op` のエラー型分離で同時に解決する。
  項目 1 は defer 明記の server.rs ×4 に加え、同型の dispatch invariant
  2 箇所（mat 側）もスコープに含める（ユーザー承認済み）。

## A. 防御的 `unreachable!` の typed error 化（6 箇所）

v1 Task6（`alias.rs id()` の Result 化 / matd `Op::Read` の expect×2 →
typed parse_error）と同じ規律に統一する: **dispatch 不変条件が破れても
panic せず `MatError::parse_error`**。detail は `internal: ` prefix +
破れた不変条件の説明（例: `internal: NotNative write reached native_op
(is_native_hotpath invariant violated)`）。

| 箇所 | 現状 | 修正 |
|---|---|---|
| `crates/matd/src/server.rs:714` | `native_op` Write 腕の `WriteClass::NotNative` → `unreachable!` | `Err(MatError::parse_error(...))` |
| `crates/matd/src/server.rs:746` | `native_op` Invoke 腕の `InvokeClass::NotNative` → `unreachable!` | 同上 |
| `crates/matd/src/server.rs:772` | `native_op` の catch-all `_` → `unreachable!` | 同上（non-hotpath op） |
| `crates/matd/src/server.rs:800` | `group_provision` let-else → `unreachable!` | 同上（non-GroupProvision op） |
| `crates/mat/src/main.rs:181` | route dispatch catch-all `_` → `unreachable!` | `Err(MatError::parse_error(...))`（`result` はそのまま既存の emit + exit 経路へ流れる） |
| `crates/mat/src/matd_client.rs:488` | `dispatch_listen` let-else → `unreachable!` | parse_error を emit して `exit_code()` で return（同関数内の alias エラー処理と同型） |

**残置する `unreachable!`（スコープ外・意図的）:**

- `crates/mat/src/commands/invoke.rs:65` — clap が `--kelvin` / `--mireds`
  の排他を保証（局所証明可能）。
- `crates/mat-controller/src/tlv.rs:358` — 3-bit マスクの全ケース網羅
  （局所証明可能）。

matd 側のエラーは従来どおり socket 越しに error JSON で返り、クライアント
（mat）が kind→exit へマップする。`parse_error` = exit 1。いずれも内部バグ
経路のみで通常到達不能。

## B. `to_op` エラー型分離（項目 2 + 3 の同時解決）

### 現状の問題

`to_op` は `Result<Value, String>` で、エラーに 2 種類が混ざっている:

1. **matd 非対応 op**（`--matd discover` 等）— 正真正銘の CLI 利用誤り。
2. **alias 解決失敗**（`id()?` の `MatError`）— 本来 `store_parse`
   （exit 10）等の固有 kind を持つ実エラー。

forced `--matd` の `dispatch` では両方が `kind=other` emit + exit 2 に
丸められる。「kind=other（exit_code()=1）なのに exit 2」の不一致に加え、
alias エラーの本来の kind が失われるのが実害。`id()?` を通すためだけに
`From<MatError> for String`（kind を落とす縮退変換）が mat-core に存在し、
workspace 全体の footgun になっている。

### 修正

- `crates/mat/src/matd_client.rs` に専用エラー型を新設:

  ```rust
  enum ToOpError {
      /// matd 非対応 op（CLI 利用誤り、exit 2）
      Unsupported(String),
      /// alias 解決失敗など、固有 kind を持つ実エラー
      Mat(MatError),
  }
  impl From<MatError> for ToOpError { ... }
  ```

  `to_op` は `Result<Value, ToOpError>` に変更。`From` impl により
  14 箇所の `id()?` は無改変で動く。`unsupported()` ヘルパは
  `ToOpError::Unsupported` を返すよう変更。

- **forced `dispatch`**:
  - `Unsupported` → 従来どおり `kind=other` emit + **exit 2**。
    「CLI 利用誤り = exit 2」の documented シグナルを維持する**意図的な
    kind/exit 例外**として、コメントとテストでピン留めする（ユーザー決定）。
  - `Mat(e)` → `e.emit()` + `e.kind.exit_code()`。alias 失敗が本来の
    kind/exit を取り戻す（実害の修正）。

- **`dispatch_auto`**:
  - `Unsupported` → 従来どおり `None`（直経路 fallback）。
  - `Mat(e)` → その場で `e.emit()` + `Some(exit)`。現状は直経路に落ちて
    同じ `id()` が同じ `MatError` で失敗するため、**観測される stderr /
    exit は同一**。alias 解決が 2 回 → 1 回になるだけ。

- **`impl From<MatError> for String` を `crates/mat-core/src/error.rs`
  から削除**（`into_detail()` 代替も置かない — 唯一の利用者が消えるため）。
  workspace 全体のコンパイルが通ることが「他に暗黙利用者がいない」証明。

## C. 挙動変更の整理

観測可能な挙動変更は **1 つだけ**:

- forced `--matd` + alias 解決失敗: exit 2 / kind=other →
  **固有 kind / 固有 exit**（例: aliases.toml 破損なら store_parse / 10）。

不変なもの: 非対応 op の exit 2、auto 経路の出力、正常系すべて、
README の exit code 表（実装時に要再確認）。unreachable! 6 箇所は
panic → error JSON + exit 1 になるが内部バグ経路のみ。

## テスト

1. 既存 `to_op` テスト（`unwrap()` / `is_err()`）のシグネチャ追随。
2. forced dispatch: 非対応 op = `kind=other` + exit 2 のピン留め
   （意図的例外の回帰防止）。
3. forced dispatch: alias 解決失敗が固有 kind / exit を保つこと。
4. `native_op` に NotNative write / invoke / non-hotpath op を直接渡すと
   `parse_error`（panic しない）。`group_provision` の非 GroupProvision も
   同様。

## 完了条件

- `task check`（fmt:check + clippy + test）green。
- 挙動変更（C 節の 1 点）が意図どおりであることをテストで確認。
