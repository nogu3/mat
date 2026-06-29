# 設計: `mat discover --probe`（commissioned ノードのライブ到達性）

- 対象 issue: #2 「`mat discover` の commissioned はキャッシュ（台帳）直読みで到達性を反映しない」
- 日付: 2026-06-29
- 採用方針: issue 設計案 **C + A + B**（mDNS 再利用）。D（state 語彙の分離）は不採用。

## 背景と問題

`mat discover` の出力 `devices` 配列には 2 種類のエントリが混在する。

- `commissionable` … `chip-tool discover commissionables` の**ライブ mDNS** 結果。
- `commissioned`  … `store.nodes()`（`nodes.json` 台帳）を**そのまま** push。到達性ゼロ検証で、`address` も commission 時の据え置き値。

このため、デバイスがオフライン（mDNS 広告ゼロ / 電源断 / fabric 脱落）でも、台帳に履歴がある限り `"state":"commissioned"` が出続け、ユーザーが「今使える」と誤解する。

## ゴール

- commissioned ノードについて**ライブの到達性を反映**できるようにする。
- 既定の高速性（台帳のみ）は維持し、到達性チェックは **opt-in**。
- stdout は純 JSON 維持（CLAUDE.md ルール2）、診断は stderr の構造化ログ（ルール3）。
- 到達性判定の単体テスト（解決成功→true / 不在→false）を追加。

## 非ゴール（YAGNI）

- `state` 語彙の刷新（`online`/`reachable` 等の新 state 値導入）。後方互換を優先し `reachable` フィールドで表現する。
- commissioned ノードへの chip-tool / CASE による operational read。重く間欠的に失敗するため避ける（避けている経路）。
- compressed-fabric-id による fabric 厳密判別（将来拡張、後述）。
- matd 経由の discover 対応（discover は matd では `unsupported`。本変更はローカル経路のみ）。

## 採用する到達性メカニズム

`mat diag node --deep` が既に実装している **mDNS ブラウズ**を再利用する。

- `crates/mat/src/commands/diag.rs::probe_mdns()` … `avahi-browse -rt _matter._tcp`（バイナリは `MAT_AVAHI_BROWSE_BIN` で上書き可）を実行し、`mat_core::diag::parse_avahi_matter` で `Vec<MatterInstance>` を得る。
- `MatterInstance { compressed_fabric: String, node_id: u64, addresses: Vec<String> }`。

**1 回のブラウズで全 commissioned ノードを照合できる**ため、ノード数に依らずプロセス起動は 1 回。認証情報も chip-tool も使わない（台帳の読み取りのみ）。

## 挙動

### `--probe` なし（既定）

現状と完全に同一。commissioned エントリは台帳をそのまま出し、`reachable`/`stale` フィールドは付与しない。後方互換。

### `--probe` あり

1. commissionable（従来通り）と commissioned（台帳）を集める。
2. mDNS ブラウズを **1 回**実行して `Vec<MatterInstance>` を得る。
3. 各 commissioned `NodeRecord` を **`node_id` で**ライブインスタンスに照合する。
   - **一致あり** → `reachable: true`、`address` = ライブ解決アドレス（台帳と異なる場合あり = 設計B）。`stale` は付与しない。
   - **一致なし** → `reachable: false`、`address` = 台帳値、`stale: true`（最後に判明した値だが未検証）。台帳に address が無ければ `address: null`。
4. **プローブ自体が実行不能/エラー**（avahi-browse 不在、spawn 失敗、パース不能など）→ 全 commissioned ノードを `reachable: null`（"不明"。`false` とは区別）とし、構造化警告を **stderr**（`tracing::warn`）に出す。stdout は純 JSON のまま。

commissionable エントリは `--probe` の有無に関わらず不変（元々ライブ）。

### ライブアドレスの選択

`MatterInstance.addresses` は複数あり得る。commissioned エントリの `address` は従来通り**単数文字列**を維持し、一致インスタンスの先頭アドレスを採る。台帳 address が一致インスタンスの addresses に含まれる場合はそれを優先（安定性のため）。`addresses` 配列の新設はスコープ外。

## 出力スキーマ

`--probe` 時の commissioned エントリ例:

```json
{"state":"commissioned","node_id":5,"address":"fd11:2233::5","commissioned_at":"2026-06-01T10:00:00+09:00","reachable":true}
{"state":"commissioned","node_id":7,"address":"192.0.2.7","commissioned_at":"2026-06-02T11:00:00+09:00","reachable":false,"stale":true}
{"state":"commissioned","node_id":9,"commissioned_at":"2026-06-03T12:00:00+09:00","reachable":null}
```

- `reachable`: `true` | `false` | `null`（プローブ不能時のみ null）。`--probe` 無しでは**フィールド自体が無い**。
- `stale`: `true` のみ付与（`reachable:false` で台帳アドレスにフォールバックした場合）。それ以外は付与しない。
- 既存の `state` / `node_id` / `address` / `commissioned_at` は不変。

## コード構成

### 純関数を `mat-core` に切り出す（ユニットテスト可能）

新規モジュール `crates/mat-core/src/reachability.rs`:

```rust
use crate::diag::MatterInstance;

pub struct NodeReachability {
    /// mDNS にライブ広告が見つかったか。
    pub reachable: bool,
    /// 解決できたライブアドレス（不一致なら None）。
    pub live_address: Option<String>,
}

/// node_id でライブインスタンスに照合する。台帳アドレスが一致インスタンスの
/// addresses に含まれればそれを、無ければ先頭アドレスを live_address とする。
pub fn resolve(
    node_id: u64,
    ledger_address: Option<&str>,
    instances: &[MatterInstance],
) -> NodeReachability;
```

- プロセス起動を一切含まない純関数。テスト容易。
- `diag::MatterInstance` を import（`mat_core::diag` は既存）。

### `discover.rs` のオーケストレーション

- CLI から `probe: bool` を受け取る（`fn run(store_path: &Path, probe: bool)`）。
- `probe == false`: 現状のロジックそのまま。
- `probe == true`: ブラウズ 1 回 → 各ノードに `reachability::resolve` → JSON 組み立て。
  - ブラウズ失敗時は `instances` を「不明」として扱い、各ノード `reachable: null` + stderr 警告。
- mDNS ブラウズ関数（avahi-browse 実行 + パース）は現状 `diag.rs` の私的関数 `probe_mdns`。これを discover からも使えるよう、共有ヘルパへ移す。
  - 案: `crates/mat/src/runner.rs` か新規 `crates/mat/src/probe.rs`（mat バイナリ内）に `pub(crate) fn probe_mdns() -> Result<Vec<MatterInstance>, MatError>` として移設し、diag からも参照。`probe_error_kind` も併せて検討（discover では stderr ログのみなので必須ではない）。

### CLI

`crates/mat/src/cli.rs` の `Command::Discover` を unit variant から struct variant へ:

```rust
/// commissionable / commissioned ノードを mDNS で探索する。
Discover {
    /// commissioned ノードのライブ到達性を mDNS で確認し reachable を付与する。
    #[arg(long)]
    probe: bool,
},
```

`crates/mat/src/main.rs` のディスパッチを `commands::discover::run(&store_path, *probe)` に更新。

`crates/mat/src/matd_client.rs` の `Command::Discover` パターン（`unsupported("discover")` を返す箇所、テスト含む）を struct variant 形に追従。挙動は不変（引き続き unsupported）。

## テスト

### ユニット（`mat-core/src/reachability.rs`）

- 解決成功: `node_id` 一致インスタンスあり → `reachable == true`、`live_address == Some(...)`。
- 不在: 一致インスタンスなし → `reachable == false`、`live_address == None`。
- アドレス優先: 一致インスタンスの addresses に台帳 address が含まれる → `live_address` がその値。
- 複数アドレス: 台帳 address が含まれない → 先頭アドレスを採る。

### 統合（fake バイナリ）

既存の fake-chip-tool / fake-avahi フィクスチャ（`FAKE_AVAHI_ADDR` 対応済み、diag テストで実績あり）を用いる。

- `discover --probe` で、fake-avahi が当該 node を広告 → 出力に `reachable:true` とライブアドレス。
- 広告に当該 node が無い → `reachable:false` + `stale:true`、address は台帳値。
- `--probe` なし → `reachable`/`stale` フィールドが出力に無い（後方互換）。
- avahi-browse バイナリ不在（`MAT_AVAHI_BROWSE_BIN` を存在しないパスに）→ `reachable:null`、stdout は純 JSON、終了コードは成功（discover 全体は失敗にしない）。

## 既知の限界（ドキュメント化）

- **node_id 照合はベストエフォート**: 別 fabric で同一 node_id が広告されていると偽陽性の可能性。当面許容。fabric 厳密判別には compressed-fabric-id（self_cfid）が必要だが、その取得は chip-tool の operational read（CASE = 重く間欠失敗する経路）を要するため本変更では避ける。将来拡張として `diag.rs` の self_cfid 導出（`parse_compressed_fabric_id`）の再利用を検討。
- 関連 issue #1（discovery timeout の誤分類）とは別軸。本件は「到達性を*見ていない*」、#1 は「到達失敗を*誤分類する*」。

## ドキュメント更新

- `README.md` の discover 節に `--probe` と `reachable`/`stale` の説明を追記。
- stdout は純 JSON 維持・診断は stderr の原則を逸脱しないことを確認。
