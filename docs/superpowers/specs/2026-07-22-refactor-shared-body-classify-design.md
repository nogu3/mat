# リファクタ: 成功 JSON 共有 builder 化 + classify 解決済み ID 一貫化 + 小粒整理 — 設計

日付: 2026-07-22
状態: 設計承認済み
種別: リファクタ（**出力・挙動は完全不変**。JSON スキーマ、exit code、エラー kind に一切変更なし）

## 背景

3 観点のコード監査（unwrap/panic 残存・エラー分類・構造品質）で、バグ級の問題は
なかったが、以下の構造的リスクが挙がった。v1 (1.0.0) bump の前段として、挙動を
変えないリファクタのみを先にまとめて行う（エラー分類の改善など挙動が変わる品質
修正は本リファクタ後・v1 前に別途行う）。

1. **成功 JSON スキーマの二重実装**: 同一 op の成功出力が、直経路
   （`crates/mat/src/commands/*.rs` の `emit_*`）と matd 経路
   （`crates/matd/src/server.rs` の `*_body` 群）で別コードとして組み立てられて
   おり、同形性を保証するのは「直経路と同形」というコメントのみ。片側だけ変更
   しても CI をすり抜ける。0.23.1 で実際に踏んだ「sibling 関数への修正適用漏れ」
   と同じバグクラス。
2. **classifier と dispatch の再解決 drift**: `mat_core::ids::classify_write` /
   `classify_invoke` が cluster 名を内部で解決するのに解決済み ID を返さないため、
   呼び手（`native_direct.rs:439/471/515`、matd `server.rs:665` 付近）が
   `resolve_cluster(...).expect("classify_* already resolved ...")` で再解決して
   いる。両者が乖離すると panic。
3. **matd body builder の `let … else unreachable!` 群**: `*_body` が `&Op` を
   受けて再 destructure しているための構造的 panic 点（`server.rs:695/718` 等）。
4. **巨大関数**: `crates/mat/src/native_direct.rs` の `run_op` が約 530 行。
   matd 側（`native.rs`）は 1 op = 1 メソッドに分割済みで非対称。
5. **小粒**: `op_report_expectation`（matd `server.rs:448`、born-dead 検知の根拠）
   だけ cluster が生 hex リテラル。`mat-core/src/store.rs:139` に
   `#[allow(dead_code)]` の未使用 `contains`。

## スコープ

### セクション1: 共有 body builder（`mat_core::body` 新設）

timestamp 抜きの成功 body（`serde_json::Value`）を返す純関数群を mat-core に
新設する。timestamp 付与は従来どおり `mat_core::output::emit`（直経路）/ matd
envelope（matd 経路）の責務のまま変えない。

**対象は両経路に存在する op のみ**:

- `read_success` / `write_success` / `invoke_success`（on/off 含む）
- `color_success` / `color_temp_success` / `level_success`
- `describe_success`
- group 送信 4 形: `group_invoke_sent` / `group_color_temp_sent` /
  `group_level_sent` / `group_color_sent`
- `group_provision_success`

引数はプレーン値（`node_id: u64`, `endpoint: u16`, `&ResolvedColor` など。
`ResolvedColor` は既に mat-core 在住）。`Op` や `Command` 型には依存しない。

- **mat 側**: `commands/*.rs` の対応する `emit_*` は
  `output::emit(body::…(...))` の薄ラッパに変える。呼び出し面（関数名・引数）は
  不変。
- **matd 側**: `server.rs` の `write_success_body` / `invoke_success_body` /
  `describe_success_body` / `hotpath_success_body` / `group_sent_body` /
  `group_provision` 内の `json!` を `body::…` 呼び出しに置換。`&Op` の
  destructure は dispatch の match アーム側へ移し、`let … else unreachable!`
  を構造的に排除する。
- **移動しないもの**: 直経路専用 op（`open-window` / `diag` / `grant` /
  `discover` / `commission` / `fabric init` 等）の emit は重複が存在しないため
  そのまま（YAGNI）。

**検証**: mat-core に各 builder の形状固定ユニットテストを追加。既存の matd 側
スキーマ期待値テスト（`server.rs:1362` 以下の `*_matches_expected_schema` 群）は
**変更せずに残し**、移行後も通ることをもって「出力不変」の証明とする。

### セクション2: classify の解決済み ID 一貫化 + `run_op` 分割

- `mat_core::ids::classify_write` / `classify_invoke` の `Native` variant に
  解決済み `cluster` ID を含めて返すよう変更する。呼び手の
  `resolve_cluster(...).expect(...)` 再解決（`native_direct.rs:439/471/515`、
  matd `server.rs:665` 付近）を全て削除し、classifier と dispatch の drift を
  型で不可能にする。
- `run_op`（約 530 行）を 1 op = 1 async fn に分割する（matd の `native.rs` と
  同じ粒度）。match の網羅性で消せる `unreachable!` は消す。
- `mat-core/src/alias.rs:52` の `unreachable!`（ユーザー入力経路の panic 点）は
  panic→エラー化で失敗モードが変わるため**対象外**（v1 前の品質修正側で扱う）。

### セクション3: 小粒

- `op_report_expectation`（matd `server.rs:448`）の生 hex
  `0x0006 / 0x0008 / 0x0300` を `im::CLUSTER_*` 定数参照へ（現状値は一致して
  おり挙動不変。定数変更・stateful op 追加時に型で追従させるため）。
- `mat-core/src/store.rs:139` の `#[allow(dead_code)]` 付き未使用 `contains` を
  削除。

## スコープ外（「あとで」= v1 前の品質修正で扱う）

- commissioning 失敗分類（`mat-native/src/commission.rs` `kind_of`）の
  timeout / device_rejected 振り分け（exit code の意味変更 = 挙動変更）。
- group 送信 `Crypto` エラーの `unreachable` → `other` 分離（同上）。
- matd 経路の途中失敗 kind / detail 改善（`matd_client.rs`）。
- `alias.rs` の `unreachable!` エラー化。
- `resolve_operational` の in-process ソケットテスト追加（挙動不変だがリファクタ
  ではなくテスト追加であり、優先度判断を分けるため本スコープから外す。v1 前に
  品質修正と合わせて検討）。

## 検証と完了条件

- `task check`（fmt:check + clippy -D warnings + test）が通ること。
- 既存の matd スキーマ期待値テスト・バイナリ統合テストが**無変更で**通ること。
- 挙動不変のため実機 E2E は必須としない（次回デプロイ時のスモークで確認）。

## バージョン

挙動不変のリファクタのため **0.28.1（patch）**。v1 (1.0.0) はスコープ外の品質
修正を入れた後に bump する。
