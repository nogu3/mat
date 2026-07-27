# commission の経路選択 — フォールバックと `--transport`（1.4.0）

日付: 2026-07-26 / 対象: `mat-native`（`commission.rs`）、`mat`（`cli.rs` /
`commands/commission.rs`）、README

## 問題

`mat commission` は **mDNS で commissionable が見つかった時点で on-network 経路に
確定し、その後どれだけ失敗しても BLE を試さない**。`crates/mat-native/src/commission.rs:233-250`:

```rust
match dnssd::resolve_commissionable(scope_id, long, Duration::from_secs(5)).await {
    Ok(_)  => (passcode, CommissionTarget::Discriminator(long)),  // → on-network 確定
    Err(dnssd::DnssdError::Timeout { .. }) => {
        // mDNS に居ない → BLE を試す（ble ビルド + dataset 必須）。
        return ble_path(&commissioning_fabric, req, passcode, long, scope_id).await;
    }
    Err(e) => return Err(MatError::new(ErrorKind::Unreachable, ...)),
}
```

`resolve_commissionable` の成功が意味するのは「**その discriminator の DNS-SD
レコードが存在する**」であって「**そのデバイスに届く**」ではない。Thread デバイスの
レコードは SRP のリースが切れるまで残るため、実体が応答しなくなってもレコードだけが
生き続ける。この状態で再試行すると、mat は毎回その死んだ宛先を選び、PASE が MRP
再送を使い切って `timeout` で終わる。**BLE 経路が正常に使える場合でも、一度も試されない。**

### 実測（2026-07-26、廊下クローゼットのテープライト = Nanoleaf vid 4442 / pid 68）

1. 初回、BLE 経路で BTP → PASE → attestation まで成功し、デバイスは Thread に参加。
   最後の CASE が MRP no-ack で失敗し commission は未完了。**だが SRP 登録は残った。**
2. 以降の再試行 7 回はすべて mDNS hit → on-network → `pase error: no acknowledgement
   within MRP retry budget`。`ping6` はその IPv6 に対して 100% loss。
3. `MAT_IFACE=wpan0` を指定して mDNS resolve を意図的に空振りさせ BLE 経路に落としたところ、
   BLE 発見 20 秒で **一発成功**（node N として登録）。

つまり経路の可用性ではなく、経路選択の設計が原因だった。

なお**工場出荷状態の機体では起きない**。SRP レコードが存在しないため mDNS が miss し、
既定のまま BLE 経路に落ちる（同日、別の機体 = Aqara Door and Window Sensor P2 を
node M として、既定設定の attempt 1 で完走）。この不具合が刺さるのは「一度 BLE で Thread 参加まで
行ったが完結しなかった機体の再試行」である。commission が途中で落ちること自体は
弱リンク環境では珍しくないので、再現性は低くない。

### 派生する 2 つの欠陥

- **経路を手で固定する手段がない。** `mat commission` の引数は `--target` /
  `--setup-code` / `--node` / `--alias` / `--thread-dataset` のみ（`crates/mat/src/cli.rs:80-101`）。
  上記の `MAT_IFACE=wpan0` は、iface 自動選択が弾く point-to-point インタフェースを
  明示指定して mDNS が届かない副作用を利用しているだけで、scope_id も巻き添えで変わる。
  BR ホスト上でたまたま CASE も通ったに過ぎず、常用できる手段ではない。
- **袋小路であることがエラーから読めない。** `pase error: ...` だけでは、どの経路を
  選んだのか、BLE を試したのかが分からない。

## 設計

### 1. 経路計画と経路実行の分離

`commission()`（現状 `commission.rs:187-340`、150 行超で資材構築・発見・実行が同居）を
3 段に割る。

- **資材構築** — code パース / scope_id / KVS 資材 / `FabricCredentials` / IPK epoch。
  現状のロジックをそのまま前段に据え置く。
- **経路計画** — 試す順番のリストを決める純関数。**mDNS の I/O は呼び出し側が済ませて
  結果を渡す**（テスト容易性のため）。
- **経路実行** — 候補を順に試し、「次に進んでよい失敗」なら次へ、そうでなければ即打ち切り。

```rust
// mat-native 側（clap 非依存 — mat-native は clap を持たない）
enum Route {
    OnNetwork(CommissionTarget),
    Ble,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport { #[default] Auto, OnNetwork, Ble }
```

`CommissionRequest` に `transport: Transport` を足す。CLI 側は
`crates/mat/src/cli.rs` に `clap::ValueEnum` を導出した独自の enum を置き、
`mat_native::commission::Transport` へ写す（`--transport` / `MAT_TRANSPORT` の
env 連携は既存の `--thread-dataset` と同じく clap の `env` 属性で行う）。

`plan_routes` は mDNS の結果（hit したターゲット / miss）と `Transport` と code 種別を
受け取り `Vec<Route>` を返す。I/O を含まないので単体テストで表を全部固定できる。

| `--transport` | QR コード（`MT:`） | manual code（11/21 桁） |
|---|---|---|
| `auto`（既定） | hit → `[OnNetwork, Ble]` / miss → `[Ble]` | hit → `[OnNetwork]` / miss → unreachable |
| `on-network` | hit → `[OnNetwork]` / miss → unreachable | 同左 |
| `ble` | mDNS を引かず `[Ble]` | CLI 層で exit 2 |

- **hit 側の BLE 後詰めは「BLE が実際に走れるとき」だけ**（最終レビュー指摘 1 で
  追加）。`plan_routes` に `ble_usable: bool`（`cfg!(feature = "ble")` かつ
  `--thread-dataset` あり）を渡し、呼び出し側で計算する（`plan_routes` は純関数の
  まま）。走れない BLE を後詰めすると、`ble_path` の「mDNS で見つからなかった」と
  読める `unreachable`(exit 5) が on-network の `timeout`(exit 3) を上書きし、
  **本 branch が直そうとしている場面でちょうど exit code が化ける**。載せなかった
  ときは stderr の `INFO` に理由（feature / dataset のどちらが欠けたか）を出す。
  **miss 側は無条件で `[Ble]` のまま** — 走れない理由を述べる文言が唯一の診断なので
  1 バイトも変えない。
- manual code は long discriminator を持たず BLE scan（12bit 完全一致）に使えないため、
  BLE を候補に入れない。miss 時の文言は現状を維持する
  （`not found via mDNS (manual code cannot use BLE; use the QR payload)`）。
- `--transport ble` は mDNS を**一切引かない**。今回の `MAT_IFACE=wpan0` ハックを
  正式な手段で置き換える。
- `--transport on-network` は miss でも BLE に落ちない。
- `--transport ble` × manual code は成立しない組み合わせなので、**CLI 層**で
  `setup-code` が `MT:` で始まらないことを検出して exit 2（引数エラー）とする。
  native 層まで持ち込まない。

### 2. 「次に進んでよい失敗」の判定

判定は `MatError` に写す前の `CommissionError` に対して行い、1 箇所の述語に集約する。

```rust
fn is_dead_end(e: &CommissionError) -> bool {
    matches!(e,
        CommissionError::Timeout("no usable address")
        | CommissionError::Pase(PaseError::Exchange(ExchangeError::Timeout))
        | CommissionError::Discovery(_))
}
```

- **安全性の根拠は「PASE 完了前に failsafe は一切 arm されていない」の一点。**
  「PASE は最初の 1 交換だからデバイス側に状態が無い」ではない —— PASE は 3 往復あり、
  `Pake1`（`pase.rs:330`）も `Pake3`（`pase.rs:384`）も `Exchange(Timeout)` に写るし、
  `ex.recv(RECV_TIMEOUT)`（`pase.rs:343` / `:389`）はデバイスが我々に ack を返した
  **後**にしか走らない。正しい不変条件はこう: ここで拾う 3 つはすべて
  `pase::establish` が `Ok` を返す前に出るものであり、`ArmFailSafe` は
  `run_credential_steps` の最初のステップ（`commissioning.rs:629`）＝ `pase::establish`
  の**後**にしか走らない。よって failsafe も fabric 状態もまだ存在せず、別経路で
  やり直しても中途状態と衝突しない。
- **既知の副作用（バグではない）。** PASE 後半（Pake1/Pake3 送信後）で中断すると、
  responder 側が PASE 確立スロットを保持したままになることがある
  （`pase.rs:363-372` の実機 E2E 知見。`ConfirmMismatch` では明示 StatusReport で
  解放させているが、`Exchange(Timeout)` は相手が無言な経路なので通知できない）。
  つまり Pake1/Pake3 の timeout でフォールバックすると、BLE 経路が「PASE を受け付け
  ないデバイス」を掴む可能性がある。代償は BLE scan 〜30 秒とその後の PASE 失敗で、
  デバイス側に不可逆な状態は残らない —— 上の安全性の根拠を崩すものではない。
- 3 つの分岐はそれぞれ実在の生成箇所に対応する:
  `Timeout("no usable address")` = `commission_on_network` のターゲット解決
  （`commissioning.rs:855`、`pase::establish` 呼び出し前）、
  `Pase(Exchange(Timeout))` = PASE 自身の MRP 予算切れ、
  `Discovery(_)` = 事前 resolve 成功後・PASE 直前に候補が消えた。
- **`Timeout(_)` へ広げてはならない。** `CommissionError::Timeout` には BLE 経路の
  post-PASE な 2 つ（`commissioning.rs:1072` の
  `"operational discovery after thread join"` と `1078` の
  `"no usable operational address"`）があり、これらは PASE 成功後・Thread 参加後に
  出る。拾ってしまうと failsafe 中の機体を再駆動することになる。
- **これ以外はすべて即打ち切り。** attestation / NOC / CASE / 明示拒否（passcode 不一致・
  StatusReport 拒否）/ malformed では、デバイス側に failsafe 中の部分状態がある可能性が
  あるため、自動で二度目を打たない。
- 特に **CASE の timeout は dead end ではない**。`kind_of()` では
  `Case(Exchange(Timeout))` も `ErrorKind::Timeout` に写るため、`ErrorKind` を見て
  判定すると誤って CASE 失敗までフォールバック対象に含めてしまう。判定を
  `CommissionError` の段で行うのはこのためである。

### 3. 全経路が尽きたときのエラー

`kind` は**最後に試した経路**のものを採用し、`detail` に経路ごとの結果を並べる。

```
native commissioning: all routes failed — on-network(2001:db8::1): pase: no
acknowledgement within MRP retry budget; ble: not found via mDNS and no
--thread-dataset for the BLE path
```

候補が 1 本しかない場合の `kind` / `detail` は現状と変わらない（表現の互換性を保つ）。

### 4. 診断出力

stdout のスキーマは変更しない（`{node_id, status, timestamp}` のまま）。stderr の
`tracing` に以下を足す。

- 経路計画時 `INFO`: 選んだ候補列、`transport` の値、mDNS が hit したか miss したか。
- 経路切替時 `WARN`: `from` / `to` / `reason`（dead end と判定した `CommissionError`）。
- 成功時: 既存の `commission executed (native on-network|ble-thread)` をそのまま使う。

「どの経路で入ったか」は stderr で足りる。stdout の JSON には足さない（契約を広げない）。

## テスト

- `plan_routes` の 6 マス（上表）をユニットテストで全部固定する。純関数なので mDNS 不要。
- `is_dead_end` の分類テストを、既存の `error_kind_mapping_follows_spec` と同じ
  `mod tests` に追加する。**`Case(Exchange(Timeout))` が `false` になることを
  明示的に固定**する（取り違えの防止が目的）。
- 全経路失敗時の `detail` 合成を 1 ケース固定する。
- CLI: `--transport` のパース、`ble` × manual code = exit 2 を
  `crates/mat/tests/integration.rs` に追加する。
- 実機（`crates/mat-controller/tests/live_commission_ble.rs` 相当の手動 E2E）:
  1. `--transport ble` で BLE 直行が通ること。
  2. 既定 `auto` で工場出荷状態の機体が従来どおり一発で通ること（回帰の要点）。

## README

- 「Discover and commissioning」に経路選択の表と `--transport` の説明を追加する。
- 環境変数表に `MAT_TRANSPORT` を追加する。
- 「Errors and exit codes」に「複数経路を試した場合は最後に試した経路の結果を報告する」
  を明記する。
- `MAT_IFACE=wpan0` の回避策は記載しない。正式な手段ができるため、今日限りの
  応急処置として畳む。

## 互換性・設計ルール

- 設計ルール 1〜4 に抵触しない（プロトコルはバックエンド内、stdout は純 JSON、
  診断は stderr、状態は持たない）。
- stdout スキーマ不変。`--transport` は既定値ありの追加引数で後方互換。
- 既定 `auto` の挙動変化は「従来 `timeout` / `unreachable` で終わっていたケースが
  成功しうる」方向のみ。成功していたケースの経路は変わらない。
- バージョンは 1.4.0（機能追加）。

## スコープ外

- manual code での BLE 対応（short discriminator 4bit からの絞り込み）。今回の事象とは
  独立した別の制約であり、必要になってから扱う。
- `--thread-dataset` の自動取得（BR ホストでの `ot-ctl` 実行）。外部プロセス依存を
  持ち込むため採らない。
- SRP レコードの鮮度検証。mat から制御できない。
