# `mat group provision --rebind` 設計（issue #5）

日付: 2026-07-09
対象 issue: [#5 group provision: 既存グループへのノード追加を一発でできるようにする（--rebind）](https://github.com/nogu3/mat/issues/5)

## 背景

controller 側 group 状態（chip-tool `groupsettings`)は現行バージョンで storage に
**永続化**されている（2026-07-09 実機確定）。そのため既存グループへの
`mat group provision` 再実行は `bind-keyset` が `Duplicate key id`（CHIP Error
0x1A）で必ず失敗する。さらに provision は bind-keyset より先に `add-keysets`
（upsert 的に成功）を実行するため、**失敗した provision が controller 側の epoch
key だけ新ランダム鍵へ上書きし、デバイス側の旧鍵とズレた状態を残す**（groupcast
が黙って壊れる）。

既存グループにノードを追加する手動回避手順（chip-tool 直叩きの
`groupsettings unbind-keyset` を含む）は実証済みだが、mat の抽象化が破れている。

## 決定事項

issue の案 A（`--rebind` フラグ）を採用する。案 B（`mat group add-node` による
メンバー自動発見）は、設計ルール 4（credential KVS 以外の state を持たない =
メンバー台帳を持てない）の制約下で到達不能メンバーの鍵ズレ black-out を安全に
防げないため見送り。

## 設計

### CLI

`mat group provision` に `--rebind` フラグを追加（既定 off）。

- `--rebind` 無しの挙動は**完全不変**（既存グループへの再実行は Duplicate で
  失敗する。誤って鍵を回す事故の防護として意図的に残す）。
- `--help` に「既存グループへのノード追加は `--rebind` + **既存メンバー全員 +
  新規**を `--nodes` に渡す + **同じ keyset-id**」を明記する。

### 直経路（`crates/mat/src/commands/group.rs::provision`）

`--rebind` 時、controller step の bind-keyset **直前**に
`groupsettings unbind-keyset <group_id> <keyset_id>` を **best-effort** で実行する。

- unbind の失敗（「未 bind なのに unbind」を含む）は**一切無視**し、debug ログ
  のみ残す（exit code も classify_failure も見ない）。
- 根拠: unbind が本当に必要なのに失敗したケースは直後の bind-keyset が従来どおり
  Duplicate で落ちるため、失敗検知はそちらに委ねられる。この構造により「未 bind
  の新規グループでも `--rebind` 付きで成功する（冪等）」が自然に成立し、unbind の
  エラー形状（実機未確定）に依存しない。

ステップ順: add-group → add-keysets → **[unbind-keyset（rebind 時のみ）]** →
bind-keyset →（各ノード: KeySetWrite / GroupKeyMap / AddGroup / ACL）。

### matd 経路（`crates/matd/src/protocol.rs` + `server.rs::group_provision`）

- `Op::GroupProvision` に `#[serde(default)] rebind: bool` を追加。
- `server.rs::group_provision` に直経路と同じ位置・同じ best-effort 意味論で
  unbind step を挿入（v0.13.0 の ACL step 4 と同様、両経路に同じ step を持つ）。
- `mat` 側（`matd_client.rs`）は op JSON に `rebind` を載せる。

**バージョンスキュー**: 旧 matd は未知フィールド `rebind` を黙って無視する
（serde 既定）。その場合は従来どおり bind-keyset の Duplicate で失敗し、鍵事故の
リスクは現状の「--rebind 無しで再実行した」場合と同等（add-keysets が先に鍵を
回す）。README に「--rebind は mat / matd 両方 0.15.0 以上」の注記を追加する
（89956cb の既存スキュー注記パターンを踏襲）。

### 出力 JSON

- 直経路で `--rebind` 時のみ、成功出力に
  `"note": "if matd is running, restart it to reload group state"` を追加する。
  根拠: matd の warm chip-tool はメモリに旧 group 状態を持つため、直経路で
  rebind した後は matd 再起動が必要（storage は永続化されるので再起動で新状態を
  ロードする）。
- matd 経路では note を出さない（warm chip-tool 自身が unbind/bind を実行し
  状態が更新されるため）。

### fake-chip-tool（`crates/mat/tests/fixtures/fake-chip-tool.sh`）

`groupsettings` ハンドラを拡張する:

- `FAKE_GROUP_BOUND=1` のとき `bind-keyset` は `Duplicate key id`（CHIP Error
  0x1A 風のログ行）を出して exit 1。ただし同一 `FAKE_CHIP_ARGS_FILE` 内に
  `unbind-keyset` の実行記録が既にあれば成功する（「bind 済み → unbind →
  bind し直し」の状態遷移を模す）。
- `unbind-keyset` は `FAKE_GROUP_BOUND=1` なら成功、無ければ「未 bind」風の
  エラー行を出して exit 1（エラー形状は仮置きで良い — mat は無視するため）。
- `FAKE_GROUP_BOUND` 無しの既存挙動（全 groupsettings 成功）は不変。

matd 統合テスト側の fake backend にも同等のケースを追加する。

### テスト（mat / matd 両方）

1. **bind 済み（`FAKE_GROUP_BOUND=1`）+ `--rebind` → 成功**。args 記録で
   unbind-keyset → bind-keyset の順序も検証する。
2. **未 bind + `--rebind` → 成功**（unbind の失敗を無視する冪等性）。
3. **bind 済み + `--rebind` 無し → 失敗**（既存挙動の防護。exit code は
   従来の分類のまま）。

### README

「Groupcast」節に「既存グループへのノード追加」手順を追加する:

- `--rebind` + **既存メンバー全員 + 新規** を `--nodes` に渡す（新規だけだと
  epoch key が既存メンバーと食い違い black-out する）。
- **同じ keyset-id** を使う（デバイス側 keyset テーブルは max 3 で IPK が 1 枠
  食うため、別 ID だと枠が枯渇する）。
- 既存メンバーの確認方法: 各ノードの
  `mat read -e 0 -c groupkeymanagement -a group-key-map`。
- 直経路で rebind した場合は matd 再起動が必要。
- mat / matd のバージョンスキュー注記。

## 受け入れ条件（issue より）

- [ ] 既存グループ（controller 側に bind 残存）に対し `mat group provision
      --rebind` が一発で成功する
- [x] 未 provision の新規グループでも `--rebind` 付きで成功する（冪等）
- [x] `--rebind` 無しの既存挙動は不変
- [x] fake-chip-tool 統合テスト追加、`task check` 通過
- [x] README / `--help` に既存グループへのノード追加手順を記載
