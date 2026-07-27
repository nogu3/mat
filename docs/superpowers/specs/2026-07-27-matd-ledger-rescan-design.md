# matd 購読台帳の定期再読（安定性監査 Tier 1 #4）

- 日付: 2026-07-27
- 対象: `crates/matd/src/subscription.rs`（主）、`crates/matd/src/main.rs`（呼び出し側）
- 出自: 2026-07-25 の安定性監査バックログ Tier 1 #4
  「台帳を起動時 1 回しか読まない」

## 背景と問題

`matd` の常駐購読は `spawn_subscription_manager`（`subscription.rs:353`）が起点で、
起動時に `Store::open` で台帳（`nodes.json`）を **1 回だけ**読み、その時点の
node_id ごとに `node_subscription_loop` を spawn する。以後、台帳は二度と読まない。

その結果、**matd 稼働中に `mat commission` したノード**は:

- op 経路は通る（`server.rs` の `require_node` が「常駐中の台帳更新を拾うよう
  毎回開き直す」規律で毎回 `Store::open` している — `server.rs:1052-1057`）。
- しかし購読は張られず、`mat listen` はそのノードについて**永久に無音**。
- さらに matd のログにそのノードが**一切現れない**ため、「購読が張られていない」
  ことに気づく手掛かりがゼロ（誤診の罠。listen 側の不調・デバイスの不調と
  区別がつかない）。

回復手段は matd の再起動のみ。op 経路には正しい規律の前例が既にあり、購読側
だけがスナップショットになっている「規律の適用漏れ」であって、設計変更は不要。

付随する既知の弱点として、起動時に store が読めない（transient な flock 競合等）
と `spawn_subscription_manager` は warn を出して空を返し、**そのまま永久に購読
ゼロ**になる（`subscription.rs:360-366`）。

## 検討した代替案

- **ファイル監視（notify crate / inotify）**: 即時検知だが、依存追加と
  プラットフォーム差異のコストに対して得られるのは最大 60 秒の遅延短縮のみ。却下。
- **commission が matd に通知**: `commission` は matd socket プロトコルに
  含めない直経路専用 op（CLAUDE.md 明記）。設計違反。却下。
- **commission 成功 JSON への note 追加（監査時の応急策案）**: supervisor 化で
  60 秒以内に自動追随するため不要。commission は直経路 op で matd の稼働有無を
  知らないので、無条件 note はノイズ、条件付き検知は複雑化。**入れない**
  （ユーザー確認済み 2026-07-27）。

## 設計

### supervisor 化（60s 周期の台帳再読）

`spawn_subscription_manager` を「supervisor タスク 1 本を spawn して返す」形に
変える。supervisor は購読済み node_id の `HashSet<u64>` を持ち、次のループを回す:

1. `Store::open(&store_path)` で台帳を読む。
2. 台帳にあって `HashSet` に無い node_id ごとに `node_subscription_loop` を
   spawn し、`HashSet` に追加。
3. `LEDGER_RESCAN_INTERVAL`（60 秒）sleep して 1 へ。

- `node_subscription_loop` は**無変更**。
- `LEDGER_RESCAN_INTERVAL: Duration = Duration::from_secs(60)` はコード内定数
  （`BACKOFF_MAX` 等と同じ扱い。設定ファイル化しない）。
- 初回ティックは sleep 前に実行する（起動直後の購読開始タイミングは現状と同じ）。

### 戻り値の変更

現行の `Vec<tokio::task::JoinHandle<()>>`（ノードごとのループの handle）から、
**supervisor 自身の `JoinHandle<()>` 1 本**に変える。ノードループの handle は
supervisor 内部の所有になる（detach で十分 — 現行も handle は `main.rs` で
`_sub_handles` に束縛されるだけで、await も abort もしていない）。呼び出し側は
`main.rs:232` と テストの `spawn_manager` 足場のみ。

### NativeState::Unavailable 時

現状どおり購読は張られない: supervisor タスクの先頭で `Ready` でなければ即
return する（現行 `node_subscription_loop` 冒頭と同じパターン。戻り値を常に
`JoinHandle` 1 本にできる）。`mat fabric init` 後の再起動で解消、という既存の
運用契約を維持する。
台帳再読で Unavailable → Ready が直ることはないため、supervisor を空回りさせる
理由がない。

### 起動時 store 読み失敗の扱い（副次修正）

現行の「warn して購読ゼロで確定」から、「warn して次のティックで再試行」に
変わる（supervisor 化の自然な帰結）。transient な読み失敗が自己回復する。

### ログ規律

弱リンクノードを常駐ノイズにしない既存規律（`classify_failure` の
First/StuckWarn/Quiet と同じ思想）に合わせる:

- supervisor 起動: `info!(nodes = n, "subscription manager starting")`（現行踏襲）。
- 再読で新規ノード検出: `info!(node_id, "ledger rescan: new node; subscribing")`
  （状態遷移なので info。commission は稀な操作でありノイズにならない）。
- ティックでの台帳読み失敗: **ストリーク初回 warn、以降 debug**。成功で
  ストリークをリセット。60 秒ごとの warn 連打を避ける。
- 変化のないティック: ログ無し（毎分のノイズを出さない）。

### 非対応と明記する範囲

- **ノード削除**: 台帳 API に削除が存在しない（`Store` は `upsert_node` のみ、
  forget/remove 系 op も無い）。手編集で台帳から消えたノードのループは matd
  再起動まで残るが、害は無害な backoff リトライのみ。supervisor は追加だけを扱う。
- **既存ノードのレコード変更**（address 更新等）: 購読ループはレコード内容を
  参照していない（node_id のみ）ため、検知不要。

## テスト

Tier 2 ⑪ で整備した `FakeEstablisher` 足場を流用。`spawn_manager` 足場の戻り値を
supervisor の単一 handle に更新した上で:

1. **稼働中追加の追随**（本命）: node 5 だけの台帳で起動 → 確立を確認 →
   `upsert_node(6)` → `tokio::time::advance` で 60 秒進める → node 6 の購読が
   確立される（establisher の呼び出し先 node_id / イベントで assert）。
2. **起動時 store 読み失敗 → 次ティック回復**: 読めない store パスで起動 →
   購読ゼロ → store を作って 60 秒進める → 購読が張られる。
3. **既存テストの無風確認**: 既存の統合テスト群が戻り値の型変更以外の修正なしで
   通ること（起動直後の挙動は不変のはずである）。

既存テストは `tokio::time::pause()` ベースなので同じ流儀で書ける。

## 実機 E2E（マージ前必須）

jarvis 上の隔離 matd（別 socket + store コピー）方式で:

1. 台帳 1 ノードで隔離 matd を起動、購読確立をログで確認。
2. 稼働中に store コピーの `nodes.json` へ 2 ノード目を追記（実 commission は
   不要 — 検証対象は「台帳追随」であり、追記ノードが実在すれば購読確立まで、
   実在しなければ確立試行ログ（`subscription attempt failed`）が出ることまでで
   台帳追随の証明になる）。
3. 60 秒以内に `ledger rescan: new node; subscribing` が出ることを確認。

## バージョン / リリース

- 1.7.0（挙動追加）。
- `task check` 合格 + 実機 E2E 合格 → main マージ → jarvis デプロイ。

## 受け入れ基準

- matd 稼働中に台帳へ追加されたノードの購読が、追加から 60 秒 + 確立時間以内に
  張られる（実機 E2E で確認）。
- 新規ノード検出時に info ログが出る（誤診の罠の解消）。
- 起動時の transient な store 読み失敗が自己回復する（統合テストで確認）。
- 既存の購読挙動（確立ラダー、backoff、無音 probe、op 相関検知）に変更がない
  （既存テスト無風で確認）。
