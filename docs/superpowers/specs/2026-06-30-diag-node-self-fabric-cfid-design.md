# 設計: `mat diag node --deep` の `advertised_self_fabric` を実機で機能させる

- 日付: 2026-06-30
- 対象 Issue: #3（`mat diag node --deep`: `advertised_self_fabric` が実機で `None` になる）
- 関連: `crates/mat/src/commands/diag.rs`, `crates/mat-core/src/diag.rs`,
  `crates/mat/tests/fixtures/fake-chip-tool.sh`

## 背景

`mat diag node --deep` の `mdns` チェックは2つを区別する:

| フィールド | 意味 | 必要なもの |
|---|---|---|
| `advertised_any_fabric` | ノードが何らかの fabric 向けに mDNS 広告しているか | 広告アドレスと台帳アドレスの照合のみ（動作している） |
| `advertised_self_fabric` | ノードが**自 fabric 向けに**広告しているか | **自 fabric の compressed fabric id (CFID)** が必要 |

`advertised_self_fabric` の判定には、広告サービス名に出ている CFID と「自 fabric の
CFID」を突き合わせる必要がある。後者を実機で取得できないのが #3。

現状の取得経路（`diag.rs:157`）:

```rust
if let Some(cfid) = parse_compressed_fabric_id(&op_out.stderr) {
    self_cfid = Some(cfid);
}
```

operational read（`descriptor read parts-list <node> 0`）の **stderr** から
`Compressed FabricId 0x<hex>` 行（`[FP]` = FabricProvisioning モジュール）をパースして
いる。実 chip-tool は既定の verbosity ではこの行を operational read の stderr に**必ずしも
出さない**ため `self_cfid` が `None` になり、`advertised_self_fabric` も安全側に `None`
へ劣化する。fake-chip-tool は `[FP] Compressed FabricId 0x...` を常に出すので
（`fake-chip-tool.sh:18`）、ユニット/統合テストは緑のまま実機差分が見えなかった。

## 調査で確定した制約

実機リポジトリ同梱の `chip-tool` バイナリを実測して確認:

- chip-tool に **fabric / CFID を offline でダンプするサブコマンドは無い**
  （`storage` は `clear-all` のみ、`sessionmanagement` は CASE/PASE セッション操作のみで
  fabric 一覧なし）。
- ログ詳細度を上げる **per-command の CLI フラグも無い**（`--trace_file` /
  `--trace_log` / `--trace_decode` / `--trace-to` はトレースファイル出力用であり、stderr の
  ログレベルを上げるものではない）。

→ CFID は **chip-tool が operational 接続する過程で stderr に吐くログ行**からしか得られ
ない。現行方針（ログ行パース）は正しく、改善すべきは「どの行を、どれだけ頑健に拾うか」と
「取れない時の振る舞い」である。CLAUDE.md ルール1（TLV/CASE/暗号を `mat` 内で話さない）
により、CFID を root 公開鍵 + fabric id から自前で HKDF 導出する案は採らない。

## 目的（受け入れ条件）

- 実機の `mat diag node --deep` で `advertised_self_fabric` が `true` / `false` で返る。
- CFID 取得不能時のフォールバック挙動がテストで担保される（黙って `None` にせず可観測）。

## 設計

### 1. CFID 取得を複数シグナルのフォールバック連鎖にする（主軸）

現在は1本（`[FP] Compressed FabricId 0x...`）に依存。これを**優先順位付きで複数の取得元**
から拾う。対象は既に走らせている operational read の stderr 全体。

1. **operational discovery が解決したインスタンス名** `<CFID>-<NodeId>._matter._tcp` を
   `[DIS]` ログから抽出し、**`NodeId` が対象ノードと一致する行の CFID プレフィックス**を採用。
   - 第1候補とする理由: 我々が実際に走らせる operational read そのものが必ず通る解決経路の
     ログであり、fabric init の `[FP]` 行より出やすい。
   - サービス名は 16 桁 hex 2 つをハイフン連結（`<16hex>-<16hex>`）。`NodeId` は対象
     `node_id` を 16 桁ゼロ詰め大文字 hex にして突き合わせる。CFID は大文字正規化
     （既存 `parse_compressed_fabric_id` と同じ正規化方針）。
2. `Compressed FabricId 0x<hex>`（現行 `parse_compressed_fabric_id`）を**第2候補**として維持。

新パーサは `crates/mat-core/src/diag.rs` に純関数として追加し、単体テスト可能にする
（例: `parse_operational_instance_cfid(stderr: &str, node_id: u64) -> Option<String>`）。
`diag.rs:157` 付近を「第1候補 → 失敗時に第2候補」の連鎖に置き換える。

### 2. 取得不能時の可観測化（受け入れ条件）

両シグナルとも拾えなかった場合、`advertised_self_fabric` を黙って `None` にせず、`unavailable`
に理由を積む:

```json
{"check": "mdns_self_fabric", "kind": "cfid_unavailable",
 "detail": "could not obtain self compressed-fabric-id from chip-tool operational logs"}
```

`advertised_any_fabric` は従来どおり出るため verdict は引き続き成立する。`mdns` チェック
オブジェクト自体は維持し、`advertised_self_fabric` は省略（`Option` の `None`）のままとする
が、「なぜ出ないか」が `unavailable` で可観測になる。

### 3. 実機での経験的確定（実装の最初のステップ）

**実機の chip-tool が既定 verbosity の stderr に上記どちらの行を吐くかは、実機実測でしか
確定できない**（このリポジトリの x86 バイナリ＋fabric 無し環境では再現不可）。よって実装の
最初のステップを実測ドリブンにする:

1. jarvis の検証対象ノード（node 5）に対し `descriptor read parts-list 5 0` を直叩きで実行し
   （`MAT_CHIP_TOOL_BIN` 経由、warm セッションを避けて one-shot で）、**生 stderr を採取**する。
2. CFID を載せている行を特定する:
   - `[DIS]` の operational discovery 解決行にインスタンス名 `<CFID>-<NodeId>` が出るか。
   - `[FP] Compressed FabricId 0x...` 行が出るか。
3. 採取結果に応じて:
   - いずれかが既定で出る → そのシグナルを第1候補にしてパーサを実装。
   - **どちらも既定では出ない**場合のみ、verbosity を上げる手段（環境変数 / トレース）を
     実機で確定し、operational read 実行時に適用する。本設計はまず「既定で拾えるか」を確認
     してから verbosity 操作の要否を判断する（不要なら追加しない、YAGNI）。

この実測結果は実装計画のステップ1で記録し、採用したシグナルと（必要なら）verbosity 手段を
コメント/コミットメッセージに残す。

### テスト

- **fake-chip-tool に `[DIS]` インスタンス名行を追加**（現状 `[FP]` 行のみ）。実機で採用した
  形式に合わせる。両シグナルからの抽出を単体・統合でカバー。
- `parse_operational_instance_cfid` の単体テスト: 一致 `NodeId` 行から CFID 抽出 /
  不一致 `NodeId` のみ → `None` / 複数行混在から正しい1件。
- フォールバック連鎖の単体テスト: 第1候補ヒット / 第1欠落→第2候補ヒット / 両欠落→`None`。
- **両シグナル欠落 → `cfid_unavailable` が `unavailable` に出る**統合テスト（受け入れ条件の担保）。
- 実機 E2E: `mat diag node --deep` で `advertised_self_fabric` が `true`/`false` で返ること。

## スコープ外

- CFID の自前導出（HKDF 計算）。CLAUDE.md ルール1 に抵触。
- `mdns` チェックの語彙変更や verdict ロジックの変更（本件は self_fabric の取得改善のみ）。
- matd 経由（warm セッション）での挙動変更。本件は one-shot `mat diag node` のパス。

## CLAUDE.md 整合

- stdout は純 JSON のまま（ルール2）。`unavailable` への理由追記も既存スキーマ内。
- 診断ログは stderr / `tracing`（ルール3）。chip-tool stderr は引き続き分類・パースに使うのみ。
- 状態は持たない（ルール4）。CFID はその場の operational read 出力から都度抽出。
