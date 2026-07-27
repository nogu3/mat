# 購読の無音 probe + レポート破棄修正（issue #15 / 安定性監査 #1）設計

- 日付: 2026-07-27
- 対象 issue: [#15](https://github.com/nogu3/mat/issues/15)（無音 deadline teardown + backoff でセンサーが最長 47 分 blind）
- 対象監査項目: 安定性監査（2026-07-25）Tier 1 #1「受信済みレポートを 2 経路で捨てている」
- バージョン: 1.6.0（挙動変更あり）

## 背景

jarvis 本番（matd 1.2.1、24 時間実測）で、購読 teardown 129 回中 95 回（73%）が
無音 deadline（max_interval 300s + slack 30s = 330s）由来だった。全ノード合計で
1 日約 3.7 時間の購読 blind、node16（照明を駆動する人感センサー）は 1 日 108.8 分・
最長 47.6 分 blind。「入室しても照明が点かない」として実害が観測されている。

構造は二段:

1. **無音 deadline が keep-alive 1 発のロスも許さない**。デバイスの keep-alive
   送出間隔が 300s なので、1 回取りこぼすと次の機会は 300s 後 = 必ず deadline 超過。
   ただし「無音 = 購読は生きている」の証明はなく、デバイス側も購読を畳んでいた
   可能性は否定できない（本設計の probe はこの切り分けを兼ねる）。
2. **再購読 backoff 上限 5 分が blind を増幅する**。リンク回復後、最大 5 分
   何もせず待つ。node16 の最長 47.6 分（attempts=13）の大半は 300s の塊の累積。

別軸で、安定性監査 #1: `mat-controller/src/session.rs` の購読 pump が
**デコード済みレポートを 2 経路で捨てる**。

- 経路A（`next_subscription_report`）: レポートをデコードした後の
  `respond_status(...).await?` が `?` で失敗を伝播し、手中の `rd` を道連れにする。
  read 経路は同じ問題を `let _ =` の best-effort で解決済み（`read_attribute`）なのに
  購読 pump に規律が持ち越されていない。
- 経路B（`respond_status` の ack 待ちループ）: 同 exchange に届いた続きチャンク
  （ReportData）が、ack 照合の副産物として payload ごと破棄される。

症状はどちらも「センサーイベントの取りこぼし + 余分な再購読の盲目窓」で、
issue #15 と同じ痛点に合流する。よって 1 ブランチで扱う。

## スコープ

やること:

1. 無音 deadline 到達時の **probe + キャップ付き延長**（計測と救済を兼ねる）
2. **BACKOFF_MAX 300s → 60s**
3. 監査 #1 の **レポート破棄 2 経路の修正**

やらないこと（明示的に別ブランチ）:

- `SILENCE_SLACK`（30s）の値変更 — probe の偽陽性率を本番 journal で 1〜2 週
  実測してから決める（issue #15 の提案順序どおり）
- 監査 #3（UDP ソケット共有）/ #4（台帳再読）— 別セッションで実施
- backoff の jitter 追加（監査 Tier2 ⑧）— スコープ外

## §1 matd 側: probe の配線とポンプ変更

### SubscribeConn trait に `probe()` を追加（`mat-native`）

```rust
async fn probe(&mut self) -> Result<(), MatError>;
```

実装 `SubscriptionSession::probe` は、購読と同じ CASE セッション・専用ソケット上で
`read_attribute(endpoint 0, cluster 0x0028 basicinformation,
attribute 0x0000 data-model-revision)` を 1 発撃ち、値は捨てて成否だけ返す。
probe の応答待ち中にデバイス発 ReportData が届いても、`screen_with` の
フィルタ落ち待避（`peer_initiated` バッファ）が既にあるため失われない。

`FakeSubConn`（`mat-native::test_support`）にも `probe` を実装し、
`fail_probe`（残り回数だけ失敗させる AtomicUsize、既存 `fail_next_report` と
同じ流儀）を追加する。

### ポンプ（`matd/src/subscription.rs` `run_subscription_once`）の変更

- `probes_used: u32` カウンタを追加。**デバイス発メッセージ受信で 0 にリセット**。
- `pump_verdict` が `Silence` を返したとき、`should_probe` が真なら probe を撃つ:
  - **成功** → `last_msg` リセット（deadline 再武装）+ `probes_used += 1`、
    INFO `silence probe passed; deadline re-armed`（node_id, probes=N 付き）を
    出して pump 継続。
  - **失敗** → teardown。ログは
    `report pump ended (silence past deadline; probe failed)`。
  - **キャップ到達**（`probes_used >= 2`）→ probe せず teardown。ログは
    `report pump ended (silence past deadline; probe extensions exhausted)`。
- `BornDeadSilence` / `OpGrace` は従来どおり即 teardown（probe しない）。
  born-dead は「op 経路は生きてレポート経路だけ死んでいる」状態なので、
  op 経路と同型の probe が成功しても何も証明しないため。
- 判定は純関数に切り出す:

```rust
const SILENCE_PROBE_MAX: u32 = 2;
fn should_probe(end: &PumpEnd, probes_used: u32) -> bool {
    matches!(end, PumpEnd::Silence) && probes_used < SILENCE_PROBE_MAX
}
```

- 現状は verdict 確定と同時に `health.clear_pending` しているが、probe 継続時に
  op 相関シグナル（pending 経過 < OP_GRACE の窓）を消さないよう、
  **実際に teardown する時だけ消す**に直す。

### キャップの意味（ゾンビ購読対策）

probe はセッションの生存しか証明できない。デバイス側が購読を畳んでいても
probe は成功するため、無制限延長は「セッションは生きているが購読は死んでいる」
ゾンビ購読の恒久盲目を生む。キャップ 2 で盲目上限を約 3×deadline（≈16.5 分）に
抑えつつ、次の 2 パターンをログで区別可能にする:

- `probe passed` の後に実レポート/keep-alive が再開 → **真の偽陽性**
  （teardown する必要がなかった。従来コードは無駄に購読を殺していた）
- `probe passed` を重ねて `extensions exhausted` で死ぬ → **デバイス側でも購読死**
  （teardown は正当だった）

この比率が将来の `SILENCE_SLACK` 決定（別ブランチ）の実測データになる。

### backoff

`BACKOFF_MAX` を 300s → 60s（ラダーは 5→10→20→40→60 頭打ち）。
リンク回復から最大 1 分で再確立試行に戻る。永続ダウンノードへの CASE 試行は
毎分 1 回 × 該当ノード数となり、jarvis / メッシュに許容範囲。
既存の純関数テスト・ラダー統合テストの期待値を更新する。

## §2 session.rs のレポート破棄修正（監査 #1）

### 経路A: `next_subscription_report`

`SecureSession` に `deferred_err: Option<SessionError>` フィールドを追加。

- 呼び出し先頭で `deferred_err` が `Some` なら take して即 `Err` を返す。
- レポートをデコードした後の `respond_status` が失敗したら、エラーを
  `deferred_err` に保存し、**デコード済みレポートは `Ok(rd)` で返す**。

効果: レポートは失われず（matd はイベントを配信できる）、セッション死は次の
`next_report` 呼び出し（PUMP_SLICE = 5s 以内）で顕在化して従来どおり再購読に
入る。330s の無音 deadline を待つ盲目化はしない。

`let _ =`（read 経路と同じ完全 best-effort）にしない理由: 購読 pump では
respond_status の失敗が「セッションが死んでいる」重要シグナルであり、
握りつぶすと発見が無音 deadline まで遅れる。deferred 持ち越しなら
「レポートを失わない」と「即時死検知」を両立できる。

### 経路B: `respond_status` の ack 待ちループ

`screen_with(PeerExchange)` が届けたメッセージのうち、IM の ReportData は
`acked_counter` の照合結果に関わらず `peer_initiated` キューへ待避してから
処理を続ける（`screen_with` 自身がフィルタ落ち時にやっている待避と同じ規律・
同じ上限 `MAX_PEER_INITIATED_BUFFER`）。待避したチャンクは次の
`next_subscription_report` が `pop_front` で自然に消費し、それぞれに
StatusResponse(0) も返る（既存のチャンク応答契約どおり）。
ReportData 以外は従来どおり破棄。

## §3 テスト

Tier 2 ⑪ で整備したテスト足場（`FakeEstablisher` + `start_paused` 時計制御 +
`spawn_manager`）をそのまま使う。

純関数:

- `should_probe`: `Silence` × カウンタ境界（0/1/2）、`BornDeadSilence` /
  `OpGrace` は常に false。
- `next_backoff`: 60s 頭打ち（既存テストの期待値更新）。

matd 統合（`FakeEstablisher`）:

1. 無音 → probe 成功で deadline を越えても再購読しない（延長の実証。
   従来なら deadline + backoff で 2 回目の priming が届くはずの窓で届かない）。
2. probe 失敗 → 従来どおり deadline で teardown → 再購読。
3. 延長 2 回 → 3 回目の deadline で probe せず teardown。
4. 実レポート受信で `probes_used` リセット（受信後、再び 2 回延長できる）。

session 統合（既存の UDP ペア足場）:

5. respond_status の ack を落とす → デコード済みレポートは `Ok` で返り、
   次呼び出しが `Err`（deferred）。
6. ack 待ち中に届いた続きチャンクが破棄されず、次の
   `next_subscription_report` で配信される。

## §4 出荷

1. `task check`（fmt + clippy + テスト）
2. **マージ前に jarvis 隔離 matd で実機 E2E**（別 socket + store コピー +
   台帳 1 ノード、本番未置換）。probe の実機誘発は「隔離 matd 確立 → 本番との
   追い出し合戦で無音死」の既知手順で `silence probe` ログを直接観測。
   経路A/B は実機での直接誘発が難しいため単体/統合テストで担保し、
   E2E では購読確立・イベント配信・teardown ログ形状の無回帰を確認する。
3. E2E 合格 → main マージ → push → jarvis デプロイ →
   スモーク（購読確立本数、`probe passed` / teardown 内訳の初期観測）。
4. 1〜2 週後に journal を集計し、偽陽性率から `SILENCE_SLACK` の値を決める
   （別ブランチ）。

## 観測コマンド（デプロイ後）

```bash
# probe の成否と teardown 内訳
journalctl --user -u matd --since "<date>" --no-pager \
  | grep -oE "silence probe passed|report pump ended \([^)]*\)" | sort | uniq -c
```
