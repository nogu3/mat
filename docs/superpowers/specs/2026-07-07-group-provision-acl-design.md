# group provision への ACL 書き込みステップ追加 — 設計

日付: 2026-07-07
ステータス: 承認済み（実装前）

## 背景 / 動機

Matter の group コマンドは authMode=Group として届くため、デバイスの ACL に
`{privilege: Operate, authMode: Group, subjects: [<GroupId>]}` のエントリが**別途必要**。
コミッショニングが作るのは CASE 管理者エントリ（controller node id）だけで、
`mat group provision` は鍵束・GroupKeyMap・AddGroup を焼くのに **ACL を書かない**。
結果、provision が全ステップ成功しても groupcast は全デバイスで「権限なし」として
黙殺される（groupcast は unacknowledged なので外から見えない）。2026-07-06 の実機
デバッグ（照明 6 台）でこのギャップが確定した。

## スコープ

1. **provision への組み込み**: 各ノード処理の 4 ステップ目として ACL read-merge-write
   を追加。**mat 直経路と matd 経由の両方**（matd 経由の provision だけ ACL が入らない
   と事故が再発するため両方必須）。
2. **修復コマンド `mat group grant` の新設**: provision 済みグループ（controller 側
   groupsettings が非冪等なので provision を再実行できない）に対して ACL ステップ
   だけを実行する。**直経路のみ**（matd プロトコルへの op 追加はしない。修復は稀な
   操作で warm session の恩恵が小さく、mat/matd バージョンスキューにも安全）。

### スコープ外

- ACL エントリの削除（グループ解体時の掃除）
- privilege の指定オプション（Operate 固定。Administer は authMode=Group と組み合わせ
  不可という Matter 仕様もある）
- targets の限定（null = 全クラスタ/全エンドポイント許可で固定）

## 設計

### ACL ステップの動作（read-merge-write、案 A）

```
accesscontrol read acl <node> 0
  → エントリ列を解釈
  → {authMode: Group, subjects ∋ group_id} なエントリが
      ある  → 何もしない（冪等。write スキップ）
      ない  → 既存リスト末尾に {privilege: 3 (Operate), authMode: 3 (Group),
              subjects: [group_id], targets: null} を追記して
              accesscontrol write acl '<全リスト JSON>' <node> 0
```

- ACL の attribute write は**全置換**なので、write は必ず「read できたリスト + 追記」
  のみ。**read が失敗（解釈不能含む）したら絶対に write しない**（管理者エントリを
  失うとデバイスが管理不能になり工場リセット行きのため）。
- 固定 2 エントリの blind write（案 B）は、同一デバイスへの複数グループ provision で
  先行グループのエントリを破壊するため不採用。chip-tool にリスト append 構文はない
  （案 C 不成立）。
- write する JSON の fabricIndex は read で得た値をそのまま渡す（サーバ側で無視・
  置換されるが、read 値を使えばハードコード不要）。

### コンポーネント配置

**mat-core: 新モジュール `crates/mat-core/src/acl.rs`**（値の解釈・変換のみ。状態は
持たない — 設計ルール 4 準拠）

- `AclEntry { privilege: u8, auth_mode: u8, subjects: Vec<u64>, targets: Option<Vec<AclTarget>>, fabric_index: u8 }`
- `AclTarget { cluster: Option<u32>, endpoint: Option<u16>, device_type: Option<u32> }`
- `group_acl_entry(group_id, fabric_index) -> AclEntry`
- `merge_group_entry(entries: &[AclEntry], group_id: u16) -> Option<Vec<AclEntry>>`
  （`None` = 既に存在 = write 不要）
- パーサ 2 種:
  - `parse_acl_from_chip_log(stdout: &str) -> Result<Vec<AclEntry>, MatError>` —
    直経路用。chip-tool の `[TOO]` ログ形式（`ACL: n entries` / `Privilege: 5` /
    `Subjects: 1 entries` / `Targets: null` / `FabricIndex: 4` …）を解釈。
    解釈不能は `ErrorKind::ParseError`。
  - `acl_entries_from_ws_value(value: &serde_json::Value) -> Result<Vec<AclEntry>, MatError>` —
    matd 用。ws 応答の数値フィールド ID キー（`"1"`=privilege, `"2"`=authMode,
    `"3"`=subjects, `"4"`=targets, `"254"`=fabricIndex。targets 内は `"0"`=cluster,
    `"1"`=endpoint, `"2"`=deviceType）を解釈。
- `to_chip_write_json(entries: &[AclEntry]) -> String` — write 引数用の名前付きキー
  compact JSON（matd の ws コマンド行「空白なし 1 引数」制約に適合）。

**mat 直経路（`crates/mat/src/commands/group.rs` + `cli.rs`）**

- `provision()`: ノードループに step 4 を追加。既存 `run_node_step` パターンを踏襲し、
  step 名は `acl read` / `acl write`（失敗時に node と step が detail に残る）。
- 新サブコマンド `mat group grant --group <ID|ALIAS> --nodes <N|ALIAS>...`:
  ACL ステップだけを各ノードに実行。alias 解決は既存 resolve 層。`--matd` 明示時は
  exit 2（commission / discover と同じ「常に直経路」扱い）。
- grant の出力（stdout 純 JSON、timestamp 必須）:
  `{"timestamp": ..., "group_id": 10, "nodes": [5,7,8], "updated": [5,7], "unchanged": [8], "status": "granted"}`
- provision の出力 JSON は現状維持（フィールド追加なし）。

**matd（`crates/matd/src/server.rs` の `group_provision`）**

- 同じ step 4 を ws コマンドで実行:
  `accesscontrol read acl {node} 0` → `results[0].value` を
  `acl_entries_from_ws_value` で解釈 → merge → 必要なら
  `accesscontrol write acl {compact_json} {node} 0`。
- `protocol.rs` への op 追加なし（grant は直経路のみ）。
- matd のバージョンを 0.10.0 に上げる（挙動変更のため）。

### エラー処理

- ACL read 解釈不能 → `parse_error` / exit 1（CLAUDE.md の fragile-parse ルール通り、
  chip-tool のバージョン変化を検知する砦として単体テストで形式を固定する）。
- ACL write 失敗 → 既存の `classify_failure` による分類 + fail-fast
  （provision の現行方針と同じ。部分結果は stdout に出さない）。
- grant はノードごとに fail-fast（provision と同じ挙動で統一）。

### 互換性

- **旧 matd（≤0.9）+ 新 mat**: matd 経由の provision に ACL ステップが入らない挙動差が
  残る（プロトコル上検知できない）。README に注記し、`mat group grant`（直経路）で
  事後修復できることを書く。
- **既存グループの救済**: 今回の実機（group 10）のように provision 済みで ACL 欠落の
  グループは `mat group grant` で修復する（これが grant の主目的）。

### テスト

- **mat-core 単体**（`acl.rs` 内 + parse 形式固定）:
  - TOO ログパーサ: admin 1 エントリ / admin+group 2 エントリ / targets 非 null /
    0 エントリ / 壊れた形式 → `parse_error`
  - ws value 変換: 数値キー形式（実機で確定済みの形）、targets null / 非 null
  - `merge_group_entry`: 追記される / 既存在で `None` / 他グループのエントリは保全
  - `to_chip_write_json`: 空白なし compact であること、round-trip
- **fake-chip-tool 統合**（mat）:
  - provision のステップ列に `accesscontrol read acl` / `write acl` が加わる
    （既存 fixture 更新）
  - ACL に Group エントリ既存在 → write が飛ばない
  - `grant` の正常系 / 未 commission ノード → exit 11
- **fake-ws 統合**（matd）: group_provision のシーケンスに acl read/write を追加。
  read 応答 fixture は実機の数値キー形式を使う。

### ドキュメント

- README: provision のステップ説明に ACL を追記、`grant` コマンドの節を追加、
  「groupcast が届かない時は ACL を疑う」トラブルシュートを Groupcast E2E 節に追記、
  旧 matd との挙動差の注記。
- ARCHITECTURE.md: group provision のステップ一覧に ACL を反映（該当節がある場合のみ）。

## 受け入れ基準

1. `task check` が通る（fmt / clippy -D warnings / 全テスト）。
2. fake-chip-tool 統合テストで provision が 4 ステップ目（acl read → 条件付き write）
   を実行することが固定されている。
3. `mat group grant` が既存グループの ACL 欠落を修復できる（実機: jarvis の group 10
   相当のシナリオを fake で再現）。
4. ACL read が解釈不能なとき write せずに `parse_error` で停止する。
