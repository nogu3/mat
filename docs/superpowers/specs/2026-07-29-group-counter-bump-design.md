# `mat group bump` — group counter 窓ジャンプ応急コマンド（Issue #14 帰結）

日付: 2026-07-29
対象 Issue: #14（group: 送信カウンタの経路間衝突で groupcast が一部ノードだけ黙って不達になる）

## 背景 — Issue #14 の再検証で前提が変わった

Issue #14 は「直経路/matd の counter 系列混在」を真因と想定し、対応案1
（counter 払い出しの flock 一元化）を本命としていた。2026-07-29 の実機
フォレンジックで以下が確定し、この前提は覆った。

**否定された仮説（すべて実測ベース）:**

1. **直経路/matd の同一ストア混在**: 0.21.0（2026-07-15、e55d2ec）の
   `native_group_counter.lock` flock で構造的に防止済み。matd は初 group op
   で lock を取得し常駐中保持、one-shot の `load()` は WouldBlock →
   `store_parse` ハードエラーで送信不能。さらに 7/25〜7/26 の counter
   ファイル値遷移（176557216 → 176565408 → 176573600…）と journal の
   送信系列開始値（176561312 / 176569504 / 176577696 / 176585888）が
   「本番 matd の load のみが書いた」場合の算術（load 毎に +8192、
   開始値 = 前ファイル値 + 4096）と完全一致。外部書込はゼロ。
2. **隔離 E2E matd（ストアコピー）の高 counter 送信**: 7/24〜26 の
   Claude セッション記録 90 ファイル全数調査で、非本番経路の group 送信は
   ゼロ。疑惑帯 176561405〜176569503 の counter 値はどこにも出現しない。
   7/25 の隔離 matd（store コピー）は node 6 への unicast read のみ。
3. **受信側リブートによる窓の先回し復元**: node 17 は UpTime=12,872,248s
   （約149日）無リブート（RebootCount=18）。

**確定した事実:** node 17 は 176561402〜404 を 3 連続で黙殺し、matd 再起動後
の 176569504（最終送信値から +8100）を即受理。unicast は終始正常。node 6
（group 所属・keyset とも node 17 と完全同一: grp11/keyset42 のみ）は正常受理。
送信側に高 counter を撃った者は存在しない。

**残存仮説:** 受信側（node 17 FW）の内部挙動 — ピア group counter 窓を
persist-ahead で永続化し UpTime に現れない内部リセットで復元した等。
送信側・経路側では説明がつかない。

なお 2026-07-09 の初回事象は flock 導入**前**であり、当時の「直経路/matd
混在で counter 衝突」診断はそのまま正。その真因は 0.21.0 で解消済み。

**診断シグネチャの訂正:** 「`native_group_counter` ファイル値が matd ログ系列
より最大 8192 先行」は persist-ahead の正常動作（load 直後: ファイル値 =
初回払い出し値 + 8192）。乖離の存在は異常の証拠に**ならない**。判定は
「matd 再起動（または本コマンド）で即回復するか」で行う。

## 目的

受信側のリプレイ窓がこちらの送信系列より先行した状態（原因を問わず）を、
**matd 再起動なし・常駐購読を落とさずに**回復する応急コマンドを提供する。
やることは matd 再起動が偶然やっていたことの意図的実行:
送信 counter を再起動 1 回相当だけ前方へジャンプする。

## CLI / 出力スキーマ

```
mat group bump
```

- オプション・引数なし。counter は fabric 全体で 1 本（Global Group Data
  Counter）なので group id は取らない。
- stdout（成功時）:

```json
{
  "timestamp": "2026-07-29T14:00:00+09:00",
  "group_counter": { "from": 176561405, "to": 176569504 }
}
```

- `from` = ジャンプ前の次回送信値、`to` = ジャンプ後の次回送信値。

## ルーティング

他の group op と同じ per-op 選択: **matd 自動発見 → matd op、不在 → 直経路**。
matd 常駐中は counter の実体（in-memory next/ceiling + flock）が matd 内に
あるため、ファイルだけ書き換えても matd の送信系列は変わらない — matd 内で
ジャンプさせることが必須。よって matd socket プロトコルに `group bump` op を
追加する（`group provision` と同類。direct-only 除外リストの対象ではない）。

- **matd 側**: `GroupCtx.sender` が未構築なら send 時と同じ lazy 初期化
  （`load()`）を行ってからジャンプ。構築済みならその
  `PersistedGroupCounter::jump()` を呼ぶ。
- **直経路**: `load()`（flock 取得。matd が実は生きていて lock 保持中なら
  WouldBlock → 既存どおり `store_parse` ハードエラー）→ `jump()`。

## コア: `PersistedGroupCounter::jump()`

`mat-controller::group` に追加。

```
jump(): from = self.next;
        self.next = self.ceiling.wrapping_add(COUNTER_EPOCH);
        persist(self.next.wrapping_add(COUNTER_EPOCH));  // 新 ceiling
        to = self.next;
        (from, to) を返す
```

- ジャンプ幅は「現 ceiling + EPOCH」= `load()` と同じ算術 = matd 再起動
  1 回相当（最終送信値から +4096〜+8192）。
- persist-ahead 不変条件（ファイル値 ≥ 未払い出しの最小値）は維持。
- 既存 `load()` / `next()` は無変更。

## エラー

新種なし。KVS 不備・counter 破損は既存の `Unavailable → store_parse` 写像、
persist 失敗は I/O エラー（既存の counter 書込失敗と同じ扱い）。

## テスト

- unit（mat-controller）: jump の算術 — 単調性、persist-ahead 不変条件、
  jump 後に drop → reload しても払い出し値が重ならないこと。
- matd op: socket プロトコル経由で bump → 応答 schema 検証（ローカル op、
  ワイヤ不要）。
- CLI（バイナリ統合）: 直経路 bump の JSON schema・exit code。
- 実機 E2E（マージ前必須）: jarvis で bump → 直後の groupcast counter が
  ジャンプ済みであること（journal）+ grp11 配達確認。

## Issue #14 の扱い

1. 本 spec の「背景」相当のフォレンジック全文と診断シグネチャ訂正を
   issue にコメントで追記。
2. 次回再発時の採証手順を明記: tcpdump で BR まで groupcast が届いているか、
   対象ノードの UpTime / RebootCount（0x33/2, 0x33/1）の即時取得。
3. `mat group bump` のマージ・デプロイ後にクローズ（7/9 初回の真因 =
   経路混在は 0.21.0 で解消済み、7/26 再発は受信側残存仮説 + 応急手段
   整備、と明記）。

## スコープ外

- Issue #14 対応案1（毎送信 flock 払い出し）: 守ろうとした対象は 0.21.0 で
  既に守られており、バグ修正としての根拠が消滅。直経路/matd 混在時の挙動を
  ハードエラー → 合法化に変えるセマンティクス変更であり、今回は見送り。
- 受信側 FW の窓復元挙動の解明（採証手順を issue に残し、再発時に実施）。
- ジャンプ幅のオプション化（YAGNI。再起動相当で十分と実証済み）。
