# CLAUDE.md

`mat` — Matter デバイス操作 CLI。`enl`（ECHONET Lite）の兄弟 CLI。Matter コントローラ（当面 `chip-tool`）をサブプロセスで呼び、stdout を `mat` のスキーマに正規化して返す。

> 名前: **`mat`** 確定（`mtr` は既存 network diagnostic と衝突するため使わない）。
> リポジトリ: **パブリック / 独立リポジトリ**。
> バックエンド: **`chip-tool`（公式 C++ 参照実装）をサブプロセス呼び出し**。
> 認証情報: ローカル KVS に永続化（**リポジトリには含めない**）。

---

## プロジェクトの目的と立ち位置

`casa`（横断クライアント）の下で、`enl` と同じ層に並ぶプロトコル固有 CLI。`casa` から `protocol = "matter"` のディスパッチ先として呼ばれることを前提とする。

`enl` と同様に **AI-native かつ UNIX-friendly**: stdout は純粋な構造化 JSON（1コマンド = 1 JSON オブジェクト）、診断と機械可読エラーは stderr、exit code で呼び出し側が分岐できる。

### `mat` の責務
- Matter コントローラ（`chip-tool`）の一貫したラッパ UX。
- `chip-tool` の冗長なテキスト出力を `mat` のスキーマに正規化して再出力。
- fabric 認証情報（Root CA / 自分の NOC / commission 済みノード）のローカル KVS 管理。
- commissioning（fabric への参加・他 admin への共有）。

### `mat` の非責務
- **人間に優しい名前 →（node_id, endpoint, cluster）の解決**。これは `casa` の責務。`mat` は数値の `node_id` で受ける。
- スケジューリング・常駐・状態保持（**認証情報 KVS を除く**。後述）。
- セッションキャッシュ・購読・フレッシュネス管理。すべて上層（`casad`）の責務。
- 論理グループ（「リビングの照明7台」等）の定義。これも `casa`／`casad` の責務（後述の二層グループ参照）。
- **「Matter デバイスになる」側（ブリッジ）**。`mat` はコントローラ＝Matter 機器を“操作する”側に徹する。非 Matter 機器（ECHONET / SwitchBot 等）を Matter 機器として再公開し、Alexa/Apple/Google に出す「ブリッジ」は、自分が Matter デバイスになる別の生き物。**`rs-matter` のデバイスモードで実装する別プロジェクト（仮 `casa-bridge`）**に切り出す。`mat` には絶対に持ち込まない（コントローラとデバイスを兼ねると Home Assistant 化する）。
- **シーン・自動化、および Alexa 等からの入口**。「複数機器をまとめてこの状態に」というシーンロジックと、その音声/UI トリガの受け口は `casad` の責務。`mat` はワンショットで1機器を叩くだけ。

---

## `enl` との決定的な違い — なぜステートフルか

`enl` がワンショットで成立するのは、ECHONET Lite が「コネクションレス UDP・認証なし・各コマンドが独立」だから。Matter はこのどれも当てはまらない。

read/write/invoke するには、(1) 自分が fabric メンバであること（Root CA + 自分の NOC）、(2) 相手と CASE セッションを確立していること（Sigma ハンドシェイク）、(3) 相手を一度 commission して認証情報を保持していること、が前提になる。**fabric 認証情報という永続状態がどこかに必ず必要**で、純粋ステートレスは原理的に不可能。

### 解決方針: ワンショット・インターフェース + 永続クレデンシャル
- **プロセスとしてはワンショット**。`mat read` / `mat write` は1コマンドで完結し、終了する。
- **認証材料だけがディスクに残る**。`git` が `.git` に、`ssh` が `~/.ssh` に依存しつつ各コマンドはワンショット、というのと同じ UNIX モデル。
- **遅さは `casad` が吸収する**。ワンショット起動のたびに mDNS 解決 + CASE ハンドシェイクを払うため、一発ごとは遅い（数百ms〜秒）。速度が要るユースケースは、暖かいセッションを保持する常駐層 `casad` が担う。`mat` 自体は遅くてよい。この線引きを崩さない。

---

## Matter と Thread の関係（`mat` のスコープ）

「Matter/Thread を操作する」は2層の混同。Thread は IPv6 メッシュ（802.15.4）の**ネットワーク層**で、その上に Matter という**アプリ層**が乗る。デバイスに対して喋るのは常に Matter。

- Thread デバイスも Wi-Fi/Ethernet の Matter デバイスも、Thread Border Router 経由で IPv6 に乗った時点で、コントローラから見れば**区別なく同じ Matter ノード**になる。
- したがって作るのは `mat`（Matter コントローラ CLI）一つ。**Thread は透過**で、`mat` のコマンド体系に Thread 固有概念は出さない。
- Thread ネットワーク自体の管理（Border Router の dataset 設定等）は別の関心事。**`mat` のスコープ外**とし、OS / Border Router に委ねる。

---

## fabric 所有モデル（multi-admin）

`mat` は**自分の fabric を持つ**。Aqara Home / HomeAssistant / Apple Home と並ぶ、もう一人の admin として振る舞う。Matter デバイスは複数 fabric に同時所属できる（multi-admin）。**HA と並行運用してよい**。

### commission の2系統（同じ `chip-tool pairing code` で扱える）
Matter の setup code（QR / 11桁）は、出どころが違うだけで `chip-tool pairing code <node-id> <code>` で一様に扱える。

1. **初回 commission**（工場出荷／リセット直後のデバイス）: 印刷された setup code をそのまま使う。
2. **multi-admin join**（既に HA 等に commission 済みのデバイスを `mat` にも足す）: 印刷コードは使えない（commissioning モードを抜けているため）。**既存 admin（HA 等）側で commissioning window を開く**と一回限りのコードが発行され、`mat` はそれで join する。

> 現実の主経路は (2)。あなたのデバイスの多くは既に HA fabric にいるため、「HA 側で共有 → 発行コードで `mat` が join」が日常フロー。

### 自分が共有する側（`mat open-window`）
`mat` が所有するデバイスを他コントローラ（Alexa / Apple / Google 等）へ共有する場面のために、`mat` 側から commissioning window を開くコマンドを持つ（`chip-tool pairing open-commissioning-window` のラップ）。

- 出力 JSON には **`manual_code`（11桁）と `qr_payload`（`MT:...` 文字列）の両方**を含める。`{ "node_id", "manual_code", "qr_payload", "expires_at" }`。
- **QR 画像のレンダリングは `mat` の責務ではない**。stdout は純粋 JSON のまま `qr_payload` 文字列を出すだけにし、QR の描画は上層（`casad` / ビューア）が行う。人間装飾を stdout に混ぜない原則を守る。
- **「複数機器を QR 1枚でまとめて共有」は Matter 仕様上できない**（マルチアドミンは1機器1コミッション）。QR 1枚で多数の機器が見える状態は、必ずそれらを束ねる**ブリッジ**（= 1台の Matter ノードとして見せる）を意味し、それは `mat` ではなく別プロジェクト（`casa-bridge`）の責務。`mat open-window` はあくまでネイティブ Matter 機器を1台ずつ共有する。

### 注意点
- **fabric 数の上限。** デバイスは対応 fabric 数を Operational Credentials クラスタで広告する。安価なノードだと5程度のことがあり、Aqara + HA + Apple + Google + `mat` で枠を食い潰す機種がある。
- **ブリッジ vs ネイティブ。** Aqara のセンサが Zigbee で Aqara ハブが Matter ブリッジとして公開している構成なら、`mat` が multi-admin する相手は**ハブ一台**で、配下センサはブリッジドエンドポイントとして見える。ネイティブ Matter-over-Thread なら各機器を個別に commission する。

---

## バックエンド: `chip-tool`

`casa` の原則「公式 CLI が存在するプロトコルは公式を使う、自作はしない」に従う。`chip-tool` は CSA の公式参照実装。

### なぜ `chip-tool` か
1. **groupcast が今できるのは事実上 `chip-tool` だけ**（最重要機能、後述）。Group Key Management とグループコマンドの完全な経路を持つ。
2. **仕様完全性が最高**。新クラスタ・新機能がまず本家に入る。マイナーなデバイスでも動く確率が高い。
3. **デバッグの地の利**。Matter のフォーラム・issue・公式ドキュメントは全部 `chip-tool` コマンドで書かれている。バックエンドが同一だと「自分が悪いのか機器が悪いのか」の切り分けで迷子にならない。
4. **`casa` の `Command::new` モデルに最も素直**。ネイティブバイナリをサブプロセス起動して終了、という `enl`/`casa` の形そのまま。

### ビルドと配布（x86 / UGREEN 前提）
- 実行先は **x86_64 の UGREEN NAS（DXP4800 Plus / Pentium Gold 8505）**。クロスコンパイル不要、glibc ミスマッチなし。
- **GPU はビルドに無関係**（C++ コンパイルは CPU・RAM・ディスクが律速）。
- connectedhomeip はサブモジュール込みで数GB、pigweed ベースのビルド環境を引き込む。**Docker のマルチステージビルドで一度焼き、ランタイムイメージにバイナリだけ載せる**。ビルドの重さを初回イメージ作成に閉じ込める。
- NAS は HA / n8n も同居するため、ビルドはリソース制限付きコンテナ or 低負荷時に回す。動かす分は軽い。

### 唯一の残コスト: 出力パースの脆さ
`chip-tool` はログ志向のテキスト出力で、`mat` 側で JSON 化する必要がある。バージョン差でパーサが壊れうる。

- read/write/invoke の `Data = ...` 形式は比較的規則的。**Phase 0 でパーサにテストを当てて固める**。
- **`chip-tool` の exit code は粗い**（失敗時はおおむね `1`、詳細はログ）。`enl` のような綺麗なコード分岐は来ないので、`mat` は stdout/stderr をパースして失敗種別（timeout / unreachable / rejected）を分類し、`mat` 自身の exit code / error kind にマップする。これもパース負担の一部。
- バージョン更新時にパーサのテストが落ちて気づける運用にしておく。

### バックエンドは差し替え可能（アダプタ境界）
`mat` はバックエンドと **`mat` 自身の JSON スキーマだけで結合**する（`casa` が `enl` と stdout JSON だけで結合するのと同じ）。一方通行のドアではない。

- **将来候補**: 出力パースが辛くなったら **matter.js ベースの薄い JS シム**（最初から構造化オブジェクトが取れる、C++ ビルド不要、軽量）。または Rust 純度を取るなら **`matc`（tom-code/rust-matc、コントローラ側プロトタイプ）**。
- **`rs-matter` はコントローラではない**。あれは「コミッションされる側（デバイス）」の実装で、テスト自体が `chip-tool` をコントローラとして使う。コントローラ候補から外す。

---

## 絶対に守る設計原則

1. **プロトコルを直接喋らない**
   TLV を組まない、CASE を自前で張らない、マルチキャストルーティングを自前で持たない。すべて `chip-tool` に委譲する。持ち込みたくなったら、それはバックエンド差し替えの議論であって `mat` 本体の責務ではない。
2. **stdout は純粋な構造化 JSON のみ**
   `chip-tool` の出力をパースし、`mat` のスキーマに正規化して再出力する。人間装飾（カラー・プログレス・対話プロンプト）は一切混ぜない。
3. **診断は stderr に構造化ログ**（`tracing`）
   `chip-tool` の stderr も呑まず、少なくとも debug レベルで残す。
4. **認証情報 KVS 以外の状態を持たない**
   セッションキャッシュ DB なし、デーモンなし、内部スケジューラなし。

---

## 認証情報ストア（KVS）

### 場所と所有
- 既定パス: `$XDG_CONFIG_HOME/mat/`（既定 `~/.config/mat/`）。Root CA・controller の鍵/証明書・commission 済みノードの NOC・`chip-tool` の永続ストレージを格納。
- パスは `--store <path>` および環境変数 `MAT_STORE` で上書き可能。
- **認証情報はリポジトリで管理しない**（パブリックのため）。`.gitignore` で確実に除外する。

### リポジトリ内のサンプル・テスト
- サンプルは**必ずダミー値のみ**（RFC 5737 `192.0.2.0/24` 等）。実 IP・実 node_id・実証明書をコミットしない。

---

## コマンド体系

### 探索・introspection
- `mat discover` — commissionable / commissioned ノードを mDNS で探索。`{ "devices": [...] }`。
- `mat describe <node_id>` — ノードのエンドポイント / クラスタ / 属性を introspect。LLM が「何を叩けるか」を知るための AI-native の肝。

### デバイス管理（commissioning）
- `mat commission <ip_or_dns> <setup_code> [--node-id N]` — fabric への参加（初回 / join 両対応）。`{ "node_id": N, "status": "success" }`。
- `mat open-window <node_id> [--timeout S]` — `mat` 所有デバイスを他 admin（Alexa 等）へ共有するため commissioning window を開く。`{ "node_id", "manual_code", "qr_payload", "expires_at" }` を返す（`qr_payload` は `MT:...` 文字列。QR 画像化は上層の責務、stdout には文字列のみ）。

### 状態操作（read / write / invoke）
**重要な非対称: read は属性、制御は invoke。** 照明の ON/OFF は OnOff 属性を `write` するのではなく On/Off コマンドを `invoke` する。

- `mat read <node_id> <endpoint> <cluster> <attribute>` — `{ "node_id", "endpoint", "cluster", "attribute", "value", "timestamp" }`。
- `mat write <node_id> <endpoint> <cluster> <attribute> <value>` — 書き込み可能属性の設定。
- `mat invoke <node_id> <endpoint> <cluster> <command> [args...]` — コマンド実行。

### ショートカット
- `mat on <node_id>` / `mat off <node_id>` — 高頻度操作。**OnOff クラスタの On/Off コマンドを `invoke` にマップ**（`write` ではない）。マッピングは `mat` 内ハードコード（プロトコルロジックではなく UX なので OK）。

### グループ制御（groupcast）— 後半フェーズ、要実機検証
`chip-tool` の Group Key Management + グループコマンドのラップ。**`mat` が扱うのは Matter ワイヤレベルのグループ**（GroupId + Key Set を各機器に焼く、マルチキャスト送信）であって、論理グループ（人間の「照明7台」）ではない（後述）。

- `mat group provision <group_id> <node_id>...` — 各ノードに Group Key Set を焼き、Groups クラスタに追加。
- `mat group invoke <group_id> <cluster> <command>` — マルチキャストでの一斉送信。

> **groupcast の制約（設計に織り込むこと）**
> - **無確認（unacknowledged）**: マルチキャストへの撃ちっぱなしで、ノードごとの成否は返らない。`mat group invoke` が返せるのは「送信した」までで「7台全部ついた」は保証できない。AI-native の「自己記述的エラー」「read-after-write 検証」と衝突する点を呼び出し側に明示する。
> - **Thread 上で特に不安定**: マルチキャスト再送が電波時間を食う / IPv6 マルチキャストのパケットドロップで到達率が落ちる、という既知問題がある。「完全同期」は transport 依存で、Thread 照明では保証が薄い。Wi-Fi/Ethernet の Matter 照明のほうがまだマシ。
> - **事前プロビジョニングが重い**: KeySetWrite / GroupKeyMap / AddGroup を全ノードに対して行う。Matter で最も壊れやすい機能。

---

## 規約

### stdout
- 成功時は結果データを JSON で stdout に出す。`chip-tool` 出力をそのまま流さず、**`mat` のスキーマで再構成**する。
- **`timestamp` フィールドを必須**とする（**ISO 8601**、`mat` が応答を組み立てた時刻）。`casa`/`enl` の規約に揃える（Unix epoch ではなく ISO 8601）。
- 例:
  ```json
  {
    "timestamp": "2026-06-03T12:34:56+09:00",
    "node_id": 1,
    "endpoint": 1,
    "cluster": "onoff",
    "attribute": "on-off",
    "value": true
  }
  ```

### stderr
- `chip-tool` のエラーは構造化ログで stderr に流す。
- `mat` 自体のエラーも同じ形式: `{"error": {"kind": "...", "detail": "..."}}`。`detail` は AI がリカバリ判断できる粒度で（例: `"Node 12 is unreachable"`）。
- `kind` 例: `store_missing` / `store_parse` / `node_not_commissioned` / `child_not_found`（chip-tool 無し）/ `child_failed` / `commission_failed` / `timeout` / `unreachable` / `device_rejected` / `parse_error`（chip-tool 出力がパースできない）。

### exit code
| code | 意味 |
|---|---|
| 0 | 成功 |
| 2 | CLI 引数エラー（clap 既定） |
| 10 | 認証情報ストアが無い / パース失敗 |
| 11 | node_id が未 commission（ストアに無い） |
| 12 | `chip-tool` バイナリが見つからない / 実行不可 |
| 3 | timeout（`mat` が chip-tool 出力から分類） |
| 4 | device rejected（同上） |
| 5 | unreachable / network（同上） |
| 1 | その他 |

> `enl` と違い `chip-tool` は失敗時 exit code が粗い（おおむね `1`）。`mat` が stdout/stderr をパースして `3`/`4`/`5` に分類する。分類できなければ `parse_error` + exit `1`。

---

## casa / casad との三層分離

```
Web ページ / LLM / その他クライアント
       │
       ▼
   casad（常駐・状態を持つ。別リポジトリ）
       │  暖かい CASE セッション / キャッシュ / 購読 / フレッシュネス
       │  プロセス起動（mat / enl を CLI として呼ぶ）
       ▼
   casa（名前 → node_id 等の解決。ステートレス）
       │  Command::new("mat") / "enl" / ...
       ▼
   mat（ワンショット維持。認証情報 KVS のみ永続）
       │  Command::new("chip-tool")
       ▼
   chip-tool ── Matter 実機（Thread / Wi-Fi / Ethernet）
```

### 二層グループ（責務分離）
「グループ」が2つある。混同して二重定義しないこと。

- **論理グループ**（「リビングの照明7台」）= 名前付けの関心事。**`casa`／`casad` が持つ**（`casa` は「名前 → アドレス解決」を自分の責務と宣言している）。
- **Matter ワイヤグループ**（GroupId + Key Set を各機器に焼く、マルチキャストアドレス）= オンワイヤのプロトコル操作。**`mat` が持つ**（`mat group provision` / `mat group invoke`）。

上層が論理グループを解決し、その実体として `mat` のワイヤグループ操作を呼ぶ。`mat` は人間向けのグルーピング名を一切持たない。

---

## ロードマップ

フェーズは**順番に**進める。前フェーズが完全に終わる（全テストが通る・受け入れ基準を満たす）まで次に進まない。

### Phase 0 — 雛形 + chip-tool ラッパ基盤 + commission + KVS

**ゴール**: fabric を作り、デバイスを commission して、その認証情報を KVS に永続できる。探索もできる。

**スコープ**:
- `clap`(derive)・`serde`・`serde_json`・`tracing`・`tracing-subscriber` を入れた Cargo プロジェクト。
- 「子ランナー」モジュール: `chip-tool` をサブプロセス起動し、stdout/stderr を捕捉、JSON にパースするかエラーを返す。`chip-tool` は PATH 解決、`MAT_CHIP_TOOL_BIN` でフルパス上書き可。
- 認証情報ストア（`--store` / `MAT_STORE` / 既定 `~/.config/mat/`）の設計と初期化。Root CA / controller 証明書の bootstrap。
- `mat discover`: commissionable / commissioned ノードを JSON で出す。
- `mat commission`: 初回 / join 両対応（`chip-tool pairing code`）。
- x86 UGREEN 向け Docker マルチステージビルド（chip-tool を焼く → ランタイムにバイナリ載せ）。

**スコープ外**: read/write/invoke / describe / on/off / group / open-window。

**完了条件**:
- `cargo build`・`cargo test`・`cargo clippy -- -D warnings` が通る。
- **ダミー `chip-tool`**（固定テキストを吐くスクリプト）を使った discover / commission の統合テスト。CI で実 chip-tool 不要。
- 実機相手の手動 E2E（HA で window を開いて join）が README に記載（CI には載せない）。
- ストア無しで起動すると exit `10`、chip-tool 無しで `12`、未 commission node で `11`。
- chip-tool 出力パーサにユニットテスト（正常系・パース不能 = `parse_error`）。

### Phase 1 — read / write / invoke + describe + on/off

**ゴール**: 名前ではなく node_id で、ノードを日常操作できる。

**スコープ**:
- `mat read` / `mat write` / `mat invoke`。chip-tool 出力を `mat` スキーマに正規化、`timestamp`(ISO 8601) 付与。
- `mat describe`（introspection）。
- `mat on` / `mat off`（OnOff コマンドを **invoke** にマップ）。
- chip-tool 失敗を `timeout`/`unreachable`/`device_rejected` に分類して exit code `3`/`5`/`4` にマップ。

**スコープ外**: open-window / group。

**完了条件**:
- ダミー chip-tool での統合テスト（read の値パース、invoke、on/off マッピング、各失敗分類）。
- README に read=属性 / 制御=invoke の非対称と、on/off のマッピング先を記載。
- error `kind` 値が安定・文書化されている。

### Phase 2 — multi-admin 共有（open-window）

**ゴール**: `mat` 所有デバイスを他コントローラへ共有できる。

**スコープ**:
- `mat open-window`（`chip-tool pairing open-commissioning-window` ラップ）。発行コードを JSON で返す。
- multi-admin 運用（fabric 数上限・ブリッジ vs ネイティブ）の挙動を README に整理。

**完了条件**: ダミー chip-tool での open-window テスト。実機での「mat → 他 admin 共有」手動 E2E を README 記載。

### Phase 3 — groupcast（最難・要実機検証）

**ゴール**: 複数照明の同期 ON/OFF を Matter ワイヤグループで実行する。**当初の動機（ポップコーン現象）だが、最も脆いので最後に回す。**

**スコープ**:
- `mat group provision`（KeySetWrite / GroupKeyMap / AddGroup を全ノードに）。
- `mat group invoke`（マルチキャスト一斉送信）。
- 戻り値は「送信した」まで（無確認のため成否保証なし）と明記。

**完了条件**:
- ダミー chip-tool での group コマンド組み立てテスト。
- **実機検証**: 自宅の照明が Thread か Wi-Fi かを測り、groupcast の到達・同期を実測。Thread で不安定なら「unicast 並行発射 + casad 側で束ねる」フォールバックを README に記載。
- 無確認・Thread マルチキャストの制約を README に明記。

### Phase 4 — ネイティブ化 / バックエンド差し替え（option）

chip-tool の出力パースやビルド配布がボトルネックになった場合のみ。

- **第一候補: matter.js シム**（構造化出力、軽量、C++ ビルド不要）。
- **第二候補: `matc`（rust-matc）**（Rust 純度。ただし個人プロトタイプ、groupcast 等は要自前）。
- **`rs-matter` はコントローラではないので対象外。**
- 差し替えは `mat` の JSON スキーマを契約に、子ランナーのアダプタ1個分で閉じること。サブコマンドや出力スキーマは変えない。

---

## やらないこと

- TLV / CASE / マルチキャストルーティングを `mat` 内で実装しない（必ず `chip-tool` に委譲）。
- 人間向けの名前・論理グループを `mat` が持たない（`casa`/`casad` の責務）。
- セッションキャッシュ・購読・デーモン・内部スケジューラを足さない（`casad` の責務）。
- **Matter ブリッジ（自分が Matter 機器になる側）を `mat` に持ち込まない。** 非 Matter 機器を Alexa 等へ出すブリッジは `rs-matter` ベースの別プロジェクト（`casa-bridge`）。
- **シーン・自動化・Alexa 等の入口を `mat` に持たない**（`casad` の責務）。`mat` はワンショットで1機器を叩くだけ。
- **QR コードの画像化・表示を stdout でしない**（`qr_payload` 文字列を出すまで。描画は上層）。
- 認証情報・実トポロジ・実証明書をリポジトリにコミットしない。
- `rs-matter` をコントローラとして採用しない（デバイス側実装のため）。

---

## 開発コマンド

タスクは [Task](https://taskfile.dev) で定義（`task` で一覧）。`enl` と同じ構成。

```bash
task build            # リリースビルド → target/release/mat
task install          # ~/.cargo/bin にインストール
task run -- discover  # 実行（chip-tool が PATH 上に必要）
task test             # テスト（ダミー chip-tool 統合テスト含む）
task clippy           # Lint（-D warnings）
task fmt              # 整形
task check            # CI 相当（fmt:check + clippy + test）

# Docker（x86 UGREEN 向け、chip-tool 同梱）
task docker:build
task docker:run -- discover
task docker:test      # ローカルツールチェーン不要
```

> ローカル実行は chip-tool を PATH 上に置く（or `MAT_CHIP_TOOL_BIN`）。chip-tool 自体のビルドは重いので Docker イメージに同梱する。
> Matter は mDNS / IPv6 マルチキャストを使うため、Docker 実行は **host networking 必須**（bridge では応答を受けられない）。
