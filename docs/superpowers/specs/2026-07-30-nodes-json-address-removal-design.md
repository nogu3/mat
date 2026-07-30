# nodes.json の address 削除 + 台帳/alias の atomic write（Issue #18 + 監査 Tier 3）

- 日付: 2026-07-30
- 対象: `crates/mat-core/src/store.rs` / `alias.rs` / `reachability.rs`、
  `crates/mat/src/cli.rs` / `commands/commission.rs` / `commands/discover.rs` /
  `commands/diag.rs`、README
- 出自: Issue #18「nodes.json の address が stale で診断を誤らせる」+
  2026-07-25 安定性監査 Tier 3「nodes.json / aliases.toml が `fs::write`
  （O_TRUNC 上書き、電源断で全レコード破壊）」
- 想定バージョン: 1.13.0

## 背景と問題

### Issue #18: address が stale で診断を誤らせる

`NodeRecord.address` の書き込み元は `mat commission` の 1 箇所のみで、CLI の
`--target` フラグの値をそのまま保存する（`commission.rs` `record_success`）。
commission 自体は setup code の discriminator から mDNS / BLE で自前探索する
ため、**`--target` はワイヤ上で一切使われないメタ情報**。実測（jarvis、
2026-07-27）で確認された非 IP 値（`entrance-recover` / `ble-thread` /
`thread`）や別ノードのアドレスは、commission 実行時に人間が `--target` に
入れたラベル・コピペがそのまま残ったもの。Thread の prefix はネットワーク
再構成で変わるため、保存した IP は原理的に stale になる。

読み出し先は 2 箇所のみ:

1. `mat discover` — 既定は台帳値を表示。`--probe` では node_id 照合が主で、
   address はライブアドレス選択の tiebreak と不達時の据え置き表示のみ。
2. `mat diag node --deep` — **実害箇所**。台帳 address へ ping6 し、mDNS 広告
   の照合もアドレスベースで行うため、stale 値だと「ping 不通」「広告なし」と
   誤診する。

matd は address を一切読まない（テストフィクスチャのみ）。実行時に未使用で
あることが確定したので、Issue の案 1「保存しない」を採る。

### 監査 Tier 3: 台帳と alias が torn write に弱い

`store.rs::save_ledger`（nodes.json）と `alias.rs::insert_node_alias`
（aliases.toml）は `std::fs::write` = O_TRUNC 上書きで、電源断・クラッシュの
タイミング次第で全レコードを失う。`mat-controller::group.rs` の persist
（tmp + fsync + rename）という正しい前例がコード内にあるのに適用されていない
だけ。台帳を書き直すこの機会に同梱する。

## 設計

### 1. データモデル（mat-core/store.rs）

`NodeRecord` から `address: Option<String>` を削除。残るフィールドは
`node_id` と `commissioned_at` のみ。

互換性: serde は未知フィールドを黙って無視するため、旧形式ファイル
（address 付き）はそのまま読める。旧バイナリも `#[serde(default)]` 済みなので
新ファイル（address 無し）を読める（両方向互換）。次の台帳書き込みで旧
フィールドは自然に消える。

### 2. atomic write ヘルパ（mat-core 新設）

`mat-core` に `write_atomic(path, bytes)` を新設する。パターンは
`mat-controller::group.rs` の persist と同一: 同一ディレクトリに `.tmp` 拡張子
の一時ファイルを作成 → 書き込み → `sync_all` → `rename`。適用先:

- `store.rs::save_ledger`（nodes.json）
- `alias.rs::insert_node_alias`（aliases.toml）

エラー写像は各呼び出し元の既存 kind を維持する。`mat-controller` 側
（`group.rs` / `group_settings.rs`）は既に正しく、かつ mat-core に依存して
いないため共通化しない。flock は導入しない — nodes.json は従来から無ロックで
書き込み元は手動 commission のみ。torn write の解消だけがスコープ。

### 3. CLI（commission）

`Commission` サブコマンドから `--target` フラグを撤去する。
`commands/commission.rs` の `run` / `record_success` から target 引数を外す。
既存の呼び出し（スクリプト・手順メモ）は clap の exit 2 で即座に判明する。
受けて無視する deprecation 経路は作らない（形骸化した引数を残さない）。

### 4. mat discover

- 既定（台帳モード）: 出力から `address` キーが消える。commissioned エントリは
  `state` / `node_id` / `commissioned_at` のみ。
- `--probe`: node_id 照合は現行どおり。`reachable: true` のときだけライブ解決
  アドレスを `address` に出す。`reachable: false` は address を出さない。
  **`stale` フィールドは廃止**（「据え置きの台帳値」が存在しなくなり意味を
  失う。`reachable: false` だけで十分）。`reachable: null`（プローブ実施不能）
  も address 無し。
- `mat-core::reachability::resolve` の `ledger_address` 引数を削除する。
  `live_address` はインスタンスの先頭アドレスを返す。

### 5. mat diag node --deep

補助プローブの実行順を反転する: **mDNS targeted resolve → 解決アドレスへ
ping6**。

- mDNS 照合は node_id ベースに一本化（アドレス照合を撤去）。
  `advertised_self_fabric` は compressed-fabric-id + node_id 一致で判定。
- ping6 の宛先は node_id 一致インスタンスの先頭アドレス。mDNS で解決できなけ
  れば ip チェックは `unavailable`（kind は `no_address_in_store` に代えて
  `mdns_unresolved`）。
- 効果: stale 値への ping6 誤診が構造的に消え、「いま広告されている実アドレス
  の生存」を見るようになる。

### 6. matd

挙動不変。本番コードは台帳の address を読んでいない（LEDGER_RESCAN も
node_id のみ使用）。テストフィクスチャの address フィールド撤去のみ。

## エラーハンドリング

- `write_atomic` の失敗（tmp 作成 / 書き込み / fsync / rename のいずれか）は
  ハードエラーで、呼び出し元の既存 kind（store 系は `Other`、alias は既存の
  写像）のまま detail に段階を明記する。黙って劣化しない。
- `diag node --deep` で mDNS 解決に失敗した場合、ping6 は実施不能として
  `unavailable` に畳む（診断コマンドの部分失敗は JSON 内に畳む既存方針の
  踏襲）。

## テスト

- store: 旧形式（address 付き）nodes.json が読めることを固定するテスト /
  atomic write（書き込み後に `.tmp` が残らない・内容一致）/ 既存テストの
  address 撤去。
- alias: atomic 化後も既存 roundtrip テストが通ること。
- reachability: `resolve` の新シグネチャでの照合テスト更新。
- discover / diag: 出力変更（address 出現条件、stale 廃止、`mdns_unresolved`）
  への追随。既存のユニットテスト構造に従う。
- matd: フィクスチャ更新のみ（挙動アサーションは不変）。

## ドキュメントと移行

- README: discover の出力例（address / stale）、`--probe` の説明、commission
  の使用例（--target 削除）を更新。エラー表・exit code は変更なし。
- 既存ファイルの掃除: コードでは行わない（次の台帳書き込みで自然消滅）。
  **jarvis デプロイ時に `jq 'del(.nodes[].address)'` で一掃**し、即座に
  ファイルをクリーン化する（デプロイ手順に含める）。
- 実機 E2E（マージ前必須）: jarvis で `discover --probe` と
  `mat diag node --deep`（到達ノード / 不達ノードの両方）を確認。matd は
  挙動不変のため隔離 matd は不要（store 読み取りのみ）。
- Issue #18 はマージ・デプロイ後にクローズ。

## 検討して却下した代替案

- **解決のたびに address を更新 + resolved_at 併記**（Issue 案 2）: 書き込み
  箇所が増え、実行間の stale は残る。実行時に使わない値を鮮度管理するコスト
  に見合わない。
- **出力スキーマ温存（address: null を残す）**: 消費側が null 分岐を強いられ
  フィールドの意味も曖昧なまま。どうせ破壊的変更なので温存の利益が薄い。
- **discover --probe の stale 印をライブキャッシュで代替**: discover は
  direct-only で matd キャッシュに依存できない。YAGNI。
- **読み込み時に旧フィールドを能動的に strip して書き戻す**: read-only 操作が
  store を書き換える意外性がある。デプロイ時の一回掃除で足りる。
