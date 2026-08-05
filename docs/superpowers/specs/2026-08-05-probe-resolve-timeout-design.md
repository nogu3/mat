# probe の resolve 窓を CASE 経路と統一（安定性監査 Tier 2 ⑩）

日付: 2026-08-05 / 対象バージョン: 1.21.0

## 背景 / 問題

監査⑩（2026-07-25）: `mat discover --probe` / `mat diag node --deep` が共有する
mDNS probe（`crates/mat/src/probe.rs`）は、台帳ノードごとの targeted resolve
（`mat-controller::dnssd::resolve_operational`、1 秒間隔で query 再送）を
**独自の 3 秒窓**（`PROBE_RESOLVE_TIMEOUT`）で打ち切る。一方、CASE が実際に
使う establish 経路は `mat-native/src/lib.rs` の `RESOLVE_TIMEOUT = 8s` で同じ
関数を呼ぶ。Thread メッシュ + advertising proxy 経由の resolve は 3〜8 秒
かかることがあり、その帯のノードは「CASE（read/write/invoke）なら届くのに
probe は `reachable:false`」と誤報する。

診断ツールが本番経路より厳しい基準で「不達」と言う逆転であり、監査の共通
パターン「正しい規律の前例（8s）がコード内にあるのに別の関数に未適用」の
一例。op ログでは観測不能（probe は mat CLI 側・使用時のみ）だが、診断の
信頼性問題としてユーザー方針「安定化につながることは基本的に全部やる」に
基づき修正する。

## 成功基準（ユーザー承認済み）

1. probe の resolve 窓が CASE establish 経路と**同一定数**になり、
   「CASE が届く範囲 = probe が `reachable:true` と言う範囲」が定義上一致する。
2. 将来どちらかの値だけ変える改変が構造的に不可能（共有定数の参照）。

## 決定

**案A: 8s に統一し、定数を `mat-controller::dnssd` に一元化。**

- 却下した案B（3s のまま失敗分だけ第 2 波リトライ）: 誤報を消すには結局
  合計 8 秒相当まで待つ必要があり、A と同コストに複雑さだけ上乗せ。
- 却下した案C（`--probe-timeout-ms` フラグで可変化）: 既定値を正さない限り
  誤報は残る。ノブ追加は YAGNI。

**受容したトレードオフ（ユーザー承認済み）**: 全ノード並行実行のため、落ちて
いるノードが 1 台でもあると probe の総所要が最大 3s→8s に伸びる。診断は
「速いが嘘をつく」より「最大 5 秒遅いが CASE と同じ判定」が正しい。
全ノード健在なら resolve 到着次第終わるので所要は伸びない。

## 設計

### 1. 共有定数の新設（`crates/mat-controller/src/dnssd.rs`）

```rust
/// CASE 前 targeted resolve（`resolve_operational`）の共通窓。
/// establish（mat-native）と probe（mat）が共有し、「CASE が届く範囲 =
/// probe が reachable と言う範囲」を定義上一致させる。
pub const OPERATIONAL_RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);
```

### 2. 参照への置き換え（挙動: establish 不変 / probe 3s→8s）

- `crates/mat-native/src/lib.rs:225` — ローカル
  `const RESOLVE_TIMEOUT: Duration = Duration::from_secs(8)` を
  `dnssd::OPERATIONAL_RESOLVE_TIMEOUT` の参照に置き換え（値 8s のまま、
  establish の挙動は不変）。
- `crates/mat/src/probe.rs:35` — `PROBE_RESOLVE_TIMEOUT`（3s）を削除し
  共有定数を使用。モジュール doc に「独自 3s 窓が CASE の 8s と乖離し
  健全ノードを誤報していた」経緯を追記。

### 3. 変えないもの

- `resolve_operational` 本体（再送間隔 `QUERY_RESEND_INTERVAL = 1s` 含む）。
- matd の `CachingResolver` / `CACHE_MISS_TIMEOUT`（窓分離は既存設計の意図）。
- `commissioning.rs:1054` の resolve（5s×12 回リトライ + 3s sleep）— コミッション
  直後の operational 出現待ちという別規律で、リトライ総量（~96s）が窓を担う。
  誤報リスクの構造ではないためスコープ外。
- エラー分類（`StoreMissing` / `Unreachable` / `Other` の作り分け）、
  出力スキーマ（`reachable` true/false/null）、exit code。
- 実機テスト 2 本（`live_jarvis.rs` / `live_commission_real.rs`）の 8s 直値は
  テストローカルな明示値なので任意（置き換えは必須でない）。

## テスト

- **構造的ピン**: 乖離防止はコンパイル時の同一定数参照そのもの。値を assert
  する tautology なユニットテストは追加しない。
- 既存テストは 3s に依存していないため無変更で全通過すること
  （`task check`）。
- **実機 E2E（マージ前必須）**: jarvis で新 `mat` バイナリ（`*.new`、本番未
  置換）により `MAT_FABRIC_INDEX=2 mat discover --probe` を実行し、
  (a) 台帳の健全ノードが `reachable:true`、(b) 所要時間が健全時は従来同等、
  (c) WARN なし、を確認する。

## 影響範囲

`crates/mat-controller/src/dnssd.rs`（定数追加）、
`crates/mat-native/src/lib.rs`（参照置換）、`crates/mat/src/probe.rs`
（参照置換 + doc）。3 ファイル、実装差分は十数行。バージョン 1.21.0。
