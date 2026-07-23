# matd priming 差分回復（state-diff recovery）— 設計

日付: 2026-07-23
状態: 承認済み（実装は別セッション）

## 目的

購読が死んでいる間に失われた属性遷移を、再購読時の priming report から**差分検知で回復**する。casad 等の listen 消費者は変更なしで、失われた遷移を通常イベントとして受け取れるようにする。

## 背景（実測エビデンス）

- 書斎人感センサー（node 16、電池式、occupancysensing）は購読の keep-alive を**一切送らない**。ceiling 300s では `silent_s=330` ちょうどで、実験的に ceiling 3600s を与え自選させても `silent_s=3630` ちょうどで購読喪失（2026-07-23 実測）。**どの MaxInterval でもアイドル keep-alive を送らないファームウェア**と確定。
- このため matd は無音 deadline ごとに再購読するが、**再購読の空白（実測 10〜270 秒）中の遷移レポートは失われる**。casad は priming を無条件で捨てる設計（誤発火防止）のため、失われた遷移は次の実遷移まで回復しない。実害: 「退室したのに書斎ライトが消えない」（2026-07-23 発生）。
- 電池式デバイスが keep-alive を守らないのは広く知られた現実（matter.js server の IKEA 電池センサー未解決 issue 等）。Home Assistant が同条件で「動く」のは、コントローラが**ステートフル**で、priming をエンティティ状態キャッシュに適用し、**キャッシュ上の遷移**でオートメーションを発火するため。本設計はこの方式を matd に持ち込む。

## 設計

### 原理

matd の per-node 購読ループに**属性ごとの最終既知値キャッシュ**を持たせ、priming イベントをキャッシュと突き合わせる:

| priming イベントの状況 | 挙動 |
|---|---|
| キャッシュに**エントリがあり、値が異なる** | **昇格**: `priming: false` + `recovered: true` で送出し、キャッシュ更新。= 盲目期間中に起きた実遷移の回復 |
| キャッシュにエントリがあり、値が同じ | 従来どおり `priming: true` で送出（消費者は無視）。キャッシュ更新不要 |
| キャッシュに**エントリが無い**（初見） | 従来どおり `priming: true` で送出し、キャッシュに格納。**昇格しない** |
| （参考）非 priming イベント | 従来どおり送出し、キャッシュ更新 |

初見を昇格させないのが要諦: matd 起動直後の最初の priming 全量は全属性が初見なのでキャッシュを黙って埋めるだけになり、「matd 再起動で現在値が発火する」という当初の priming 破棄設計が守っていた性質はそのまま保たれる。

### 消費者への効果（casad は無変更）

- 昇格イベントは `priming: false` なので、casad の既存 Matter イベントトリガがそのまま発火する。
- 値が変わっていない priming は今までどおり `priming: true` で無視される。**on が冪等でないデバイス（ファン＝turnOn の度に速度が変わる等）でも、値が変わらない限りイベントは昇格しないので誤動作しない**（「priming を無条件発火」案を却下した理由がこの設計では消える）。
- `recovered: true` は観測用の付加フィールド。casad は serde で未知フィールドを無視するため互換。

### 実装の置き場所（matd / crates/matd/src/subscription.rs）

1. `node_subscription_loop`（ノード毎・再購読を跨いで生存するループ）にキャッシュを保持:
   `last_known: HashMap<(u16, u32, u32), serde_json::Value>`（key = endpoint, cluster, attribute の数値。値は `Event::value` と同じ JSON scalar）。
2. `run_subscription_once` に `&mut last_known` を渡し、priming 配信ループとポンプ内の `events_from_report` 送出箇所で上表のロジックを適用する。適用は `Event` 生成後・`events.send` 前の純関数に切り出す（`classify_against_cache(&mut last_known, ev) -> Event` の形。テスト対象）。
3. `Event` に `recovered: bool` を追加（`#[serde(default)]` 相当・`to_json` では常に出力）。既存の `priming` と直交。
4. broadcast 受信者ゼロ（listen クライアント不在）でもキャッシュ更新は行う（send の成否と無関係に、状態追跡は購読が生きている限り継続する）。

### キャッシュの寿命と限界（意図した割り切り）

- **matd プロセス内メモリのみ**。永続化しない（matd の「状態はプロセス寿命」原則のまま）。matd 再起動を跨ぐ遷移は初見扱いになり回復されない — 再起動直後の誤発火防止を優先する意図的な選択。
- 昇格イベントの `timestamp` は受信時刻。実際の遷移時刻は原理的に不明（盲目期間内のどこか）である旨をスキーマ文書に明記する。
- 回復レイテンシは再購読タイミングに依存: ceiling 300 のままなら、無音開始から最悪 約330 秒 + 確立時間で回復する。
- サイズ: ノードあたり属性数十件の JSON scalar。無視できる。

### ceiling は 300 を維持（実験の 3600 は廃棄）

- 3600 実験は仮説（デバイスが自選値なら守る）の検証が目的で、棄却された。回復レイテンシの観点でも、本設計では**むしろ短い ceiling の方が有利**（再購読が早い = priming 差分回復が早い）。300 短縮の歴史的経緯（flaky ライトの盲目窓対策）とも整合。
- 懸念として、node 16 はアイドル 5.5 分毎に CASE 再確立が走る（電池消費・Thread トラフィック）。これは本設計のスコープ外とし、電池消耗が観測されたら per-node ceiling 設定を別途検討する。
- **後始末**: jarvis の matd は現在 ceiling 3600 の実験ビルド（未コミット編集のビルド。ローカルツリーは revert 済み）。本設計の実装をデプロイする時点で正規ビルドに置き換わり自然に解消する。それより先に何かをデプロイする場合も正規ビルドで上書きすること。

## テスト

- 単体（純関数 `classify_against_cache`）: 初見 priming → 非昇格・キャッシュ格納 / 同値 priming → 非昇格・素通し / 差分 priming → 昇格（`priming=false, recovered=true`）・キャッシュ更新 / 非 priming → 素通し・キャッシュ更新。
- manager 経路（既存の FakeEstablisher / `onoff_report` パターン流用）: 再購読を跨いでキャッシュが生存し、2 回目の priming で値が変わっていれば昇格イベントが broadcast に流れる / 変わっていなければ `priming=true` のまま。
- `Event::to_json` に `recovered` が常に載る（既存 `event_json_uses_chip_tool_names_and_numeric_fallback` の拡張）。
- 実機 E2E（デプロイ後）: 書斎で「在室 → 購読喪失を待つ（アイドル 5.5 分+）→ 退室 → 再購読」の順で、`recovered: true` の occupancy=0 イベントと casad の消灯発火を journal で確認。

## スコープ外

- per-node の max_interval ceiling 設定（電池消耗が問題化したら別 spec）。
- 無音 deadline 前の生存プローブ・定期再購読（本設計で目的が満たされるため不要）。
- キャッシュの永続化・casad 側の変更・mat listen の CLI 変更（`recovered` はイベント行に載るのみ）。
- casad 側の `on_priming` ルールフラグ（案 A）— 本設計で不要になり廃案。

## ドキュメント更新（実装時）

- mat README / `mat schema listen` 相当の記述に `recovered` フィールドを追記（「priming 差分回復で昇格したイベント。timestamp は受信時刻であり実遷移時刻ではない」）。
- casa リポジトリ側: CLAUDE.md / README の「priming は発火しない」記述に「matd 側の差分回復で失われた遷移は `recovered` イベントとして届く」旨を一行補足（casa 側は別リポジトリ作業）。
