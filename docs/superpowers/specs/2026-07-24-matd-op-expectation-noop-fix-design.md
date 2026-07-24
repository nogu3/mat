# matd op相関検知の no-op 誤爆修正（op 期待の健全化）— 設計

日付: 2026-07-24
状態: 承認済み（実装は別セッション）
関連: `2026-07-21-matd-borndead-detection-design.md`（本設計はその §1 op相関検知を修正する）、
`2026-07-23-priming-diff-recovery-design.md`（値キャッシュを共有する — 「実装の置き場所」参照）

## 問題

0.28.0 の op相関検知は「状態変更 op が成功したのにデバイス発メッセージが
OP_GRACE(10s) 以内に来ない = 購読死」と判定するが、**「op が成功した」は
「レポートが出るはず」を含意しない**。すでに目標状態にあるデバイスへの
On/Off/Level は data model が変化せず、Matter 仕様上レポートは出ない（購読
レポートは属性変化時のみ）。よって現行判定は unsound。

実測（2026-07-23 夜、jarvis）: casa の人感ルール「人感OFFで消灯」は消灯済み
でも off を撃つため、ルール発火27回のほぼ全てが op+10〜14s で健全な購読を
キル（node 6: 7回 / 17: 5回 / 12: 4回）。キル毎の CASE 再確立がメッシュ
トラフィックとなり、RF 劣化時には「キル → CASE 嵐 → 輻輳 → 他ノードの
無音誤キル」の正帰還で全域遅延を増幅した。健全なメッシュ上でも誤キルは
発火の度に再現し続ける（22:24 / 22:29 / 22:30 の連続キルで確認）。

## 方針

**pending（レポート期待）を「op が状態を実際に変えると証明できる時」だけ
打つ**。証明には matd が自分の購読イベントストリームから保持する属性最終値
キャッシュを使う。検知器の意味論を「期待が成立している時のみ作動する」
sound なものへ直す — grace 10s・無音 deadline・`pump_verdict` は無変更。

## 変更点（すべて `crates/matd/`）

### 1. 属性最終値キャッシュ（priming 差分回復と共有）

`2026-07-23-priming-diff-recovery-design.md` が導入する last_known キャッシュ
（priming イベントの差分昇格用）と**同一の状態**を使う。二重実装しない。

- 置き場所は両機能から届く共有構造とする: SubHealth（server op 経路から参照
  可能）に `values: Mutex<HashMap<(u64, u16, u32, u32), serde_json::Value>>`
  （key = node, endpoint, cluster, attribute）。priming 差分回復 spec の
  「node_subscription_loop 内ローカル」という置き場所指定は本統合により
  ここへ差し替える（純関数 `classify_against_cache` の契約・挙動表は不変。
  ロック越しに同じ map を触るだけ）。
- 全 scalar 属性を保持する（差分回復側の要件。サイズはノードあたり数十件の
  JSON scalar で無視できる — 先方 spec の見積りを踏襲）。本設計が読むのは
  onoff/on-off (0x0006/0x0000) と levelcontrol/current-level (0x0008/0x0000)
  の 2 キーのみ。
- 更新点: pump が受けた全イベント（priming / live 両方）。listen クライアント
  不在でも更新する（先方 spec の決定と同じ）。ephemeral なプロセス内状態のみ
  （設計ルール 4 の永続状態に非該当 — 既存 pending と同格）。

### 2. op→期待の分類強化（`server.rs` の `op_report_expectation`）

| op | 判定 |
|---|---|
| `On` / `Off` | キャッシュの on-off と目標値（true/false）を比較。**不一致なら pending**、一致なら打たない |
| `Level` | キャッシュの current-level と `level`（raw 0–254、mat 側換算済みで届く）を比較。同上 |
| `Color` / `ColorTemp` / `Write` / `Invoke` | **pending 対象から降格**（状態変化を証明できない。無音 deadline が受け皿） |
| キャッシュ欠落 | pending を打たない（証明できない以上 unsound。matd 起動直後・購読未確立ノードでの誤爆を防ぐ） |

- 判定は純関数に切り出す（`(op, cached_onoff, cached_level) →
  Option<(node_id, cluster)>` の形。テスト対象）。
- `note_op` 内の subscriptions.toml クラスタフィルタは維持（購読対象外
  クラスタにレポート期待を置くのは元々 unsound）。
- server.rs 既存コメントの race（report が note_op より先に届く逆順）は
  従来どおり許容 — 最悪ケースは健全購読の 1 回余分な再購読で不変。

## 意図的なトレードオフ（決定の記録）

- **Color / generic Invoke の born-dead 高速検知を失う。** 受け皿は無音
  deadline（最大 max_interval + 30s）。実運用の op はほぼ On/Off/Level
  （casa ルール・mando）なので実害は小さい。コマンド→期待属性の対応表を
  ColorControl 等へ広げる拡張は、この純関数に腕を足すだけの位置に置く。
- **キャッシュがステイルでも誤爆経路はない。**「実際は変化するのに一致と
  誤判定」→ 高速検知を 1 回逃すだけ（無害）。「実際は no-op なのに不一致と
  誤判定」→ 購読生存中はレポート追従でキャッシュ≈真値・購読死中は pump
  不在で pending は評価されず、再確立時に `clear_pending` 済み。
- **NL68 の物理消灯固着（state=on なのに消灯）は本設計に無関係。** 判定
  対象は data model のレポート有無であり、data model が on のままなら on
  への op はレポートを出さない → 「一致 = 打たない」が正しい。物理状態の
  救済はデバイス操作側の問題（casa 側 issue 参照）。

## 変更しないと決めたこと

- **SILENCE_SLACK（max_interval + 30s）は据え置き。** 短い無音 deadline は
  priming 差分回復設計では「再購読が早い = 回復が早い」という**機能**であり、
  広げる理由がない。keep-alive を送らないデバイス（node 16 実測）の周期的
  再購読は先方 spec が意図的に受け入れた挙動。
- **`pump_verdict` / `OpGrace` / grace 10s は無変更。** 修正は「pending を
  いつ打つか」だけに閉じる。

## テスト（既存 FakeEstablisher 流儀）

- 純関数（分類）: On/Off/Level それぞれの一致（打たない）/ 不一致（打つ）/
  キャッシュ欠落（打たない）、Color / Invoke の降格（打たない）。
- 統合 (a) 誤爆の釘打ち: priming でキャッシュが埋まった後、同値 Off →
  pending が立たず、無音 deadline 前に再購読が**起きない**
  （`live_report_clears_pending_without_resubscribe` の変奏）。
- 統合 (b) 真の born-dead 検知の維持: 不一致 Off + デバイス沈黙 → 従来どおり
  grace + backoff 内に再購読（既存 `op_grace_triggers_fast_resubscribe` を
  キャッシュ前提に合わせて更新）。
- 統合 (c): live イベント・priming の両方でキャッシュが更新される。

## 受け入れ（実機 E2E、マージ前必須）

jarvis 上で `*.new` バイナリの隔離 matd に対し:

1. 消灯済みノードへ matd 経由 off → journal に op-correlated キルが
   **出ない**こと。
2. 点灯中ノードへ off → レポート到達で pending 解除（キルなし）、イベントが
   listen に流れること。
3. （回帰）通常の on/off/read/describe が従来どおり成功すること。

## 実装順の指定

priming 差分回復（2026-07-23 spec）と本設計は同じキャッシュを触るため、
**同一実装セッションで、キャッシュを最初から SubHealth 共有形で実装する**
ことを推奨。別セッションに分ける場合は差分回復を先行し、キャッシュは初回
から共有形（SubHealth）に置くこと（ローカル形で作って後から移すのは二度
手間）。

## スコープ外

- casad 側の no-op スキップ（off 限定・状態キャッシュ鮮度条件付き）は casa
  リポジトリへ issue 起票のみ（無線トラフィック削減が目的。matd 本修正で
  誤キル自体は消えるため必須ではない。on 側スキップは NL68 固着があるため
  禁止、と issue に明記）。
- per-node の購読絞り込み・ceiling 設定（先方 spec と同じ扱い）。
- バージョンは patch 想定（実装セッションで確定）。README 変更なし
  （外部契約不変。挙動変更は matd 内部の死活判定のみ）。
