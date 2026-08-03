# 常駐 mDNS AAAA プールの鮮度（安定性監査 Tier 2 ⑤）

日付: 2026-08-03 / 対象バージョン: 1.19.0

## 背景 / 問題

matd 常駐 mDNS listener の畳み込み（`mat-controller/src/dnssd.rs` の
`OperationalFold.addrs`）は、host ごとの AAAA プールを**追記専用**で保持する。
アドレスに時刻情報が無く、dedup と `MAX_ADDRS_PER_HOST`(8) の頭打ちだけがある。
この構造には 2 つの欠陥がある（監査⑤、2026-07-25）。

1. **古いアドレスを先に試す** — ノードがアドレスを変えると（Thread の prefix
   変更・SLAAC ローテーション等）、旧アドレスがリスト先頭に残る。
   `ResolvedNode` 構築時のソートは `sort_by_key(is_link_local)` のみで、安定
   ソートのため挿入順（=古い順）が保存される。消費側
   （`mat-native/src/lib.rs` の `establish` / `establish_subscription`）は先頭
   から 1 アドレスずつフル CASE を試すので、1 stale あたり Sigma1 の MRP 予算
   （Thread SII=5000ms で約 10s）を浪費する。最悪 8 本 stale で **約 79s**。
2. **上限 8 本で新アドレスを学習拒否 = 恒久 unreachable** — プールが 8 本に
   達すると `list.len() < MAX_ADDRS_PER_HOST` の判定で新規アドレスは**挿入すら
   されない**。全部 stale なら、そのノードは matd 再起動まで恒久 unreachable。

なお同ファイルのパーサ（`parse_message`）は RFC 6762 §10.2 の **cache-flush
ビット**（class フィールド最上位ビット）を読み捨てている。mDNS 準拠デバイス
（OTBR の SRP advertising proxy を含む）は AAAA 広告にこのビットを立てて
「これが現在の全アドレスであり、旧記録は破棄せよ」と宣言しており、鮮度の
正攻法を実装する材料はワイヤ上に毎回届いている。

op ログ観測（7/27〜8/2）では down_s>60 の遅い再確立が 2 日で 6 件
（node5/6/9/11、attempts 2-3）。弱リンク RF と交絡するが機構は確定済み。

## 成功基準（ユーザー承認済み）

1. アドレスが変わったノードの再確立で**新アドレスが最初に試される**
   （stale 8 本 × 10s の浪費をなくす）。
2. 上限 8 本での新アドレス学習拒否（恒久 unreachable）を**根絶**する。

## 決定

**C 案: RFC 6762 cache-flush 準拠（主機構）+ 鮮度タイムスタンプ（バック
ストップ）の併用。** 品質最優先のユーザー判断。

- cache-flush 準拠により、デバイスが再広告するたびに stale アドレスが
  **物理削除**され、プールが常に現実と一致する。過去にローテーションした
  ノードが本当に落ちている間も、再接続試行が stale 分だけ長引く残余問題が
  消える（鮮度順序だけの A 案ではここが残る）。
- 鮮度タイムスタンプは cache-flush を立てない実装への保険であり、同時に
  上限時の「最古 evict」の判定材料になる。

変更は `crates/mat-controller/src/dnssd.rs`（+単体テスト）に閉じる。
`mat` 一発の `resolve_operational` / `OneShotResolver` は無変更
（毎回その場の応答burst から組むので鮮度問題が構造的に無い）。

## 設計

### 1. パーサ: cache-flush ビットの保持

- `Record` に `cache_flush: bool` を追加。`parse_message` が現在読み飛ばして
  いる class フィールド（レコード名直後 `p+2` の 2 バイト）の最上位ビット
  0x8000 を拾う。
- class の**検証はしない**（「mDNS は IN-only、class は無視」の現行規律を
  維持。ビットを 1 つ拾うだけ）。既存のレコード構築（テスト含む）には
  フィールド追加のみ波及。

### 2. fold: 鮮度付き AAAA プール

`OperationalFold.addrs` の要素を `Ipv6Addr` → `(Ipv6Addr, last_seen: Instant)`
に変更する。`fold_operational_into_cache` に `now: Instant` を引数追加し、
listener 呼び出し側は `Instant::now()` を渡す（fold は時刻注入で純粋関数の
ままテスト可能に保つ）。

AAAA 受信時の処理（host ごと）:

1. **cache-flush 付きの場合**: 先に同 host の「`last_seen < now - 1s`」の
   エントリを全削除する（RFC 6762 §10.2 の 1 秒猶予 — 同一アナウンス burst
   が複数データグラムに分割されても、burst 内で先着したパケットの記録は
   消さない）。削除したアドレスは debug ログに出す
   （`aaaa evicted (cache-flush)` — journal 診断の文化に合わせる）。
2. **挿入/更新**: 既知アドレスなら `last_seen = now` に更新。新規なら追加。
   プールが `MAX_ADDRS_PER_HOST` 満杯なら**最古 `last_seen` のエントリを
   追い出して**挿入する（現行の「新規拒否」を廃止 — starvation 根絶）。
   evict も debug ログ（`aaaa evicted (pool full)`）。

`MAX_FOLD_ENTRIES`（host 数上限、flood 防御）・キー小文字正規化・
touched 機構（SRV 先行 / AAAA 後着の cross-datagram 完成）は無変更。

### 3. ResolvedNode 構築: 鮮度順ソート

cache insert 時のアドレス順を「非リンクローカル優先 → 同群内 `last_seen`
新しい順」に変更する（`sort_by_key(|(a, seen)| (is_link_local(a),
Reverse(seen)))` 相当）。デバイスは約 30 秒周期で現アドレスを再広告するため
現役アドレスは常に最新 = 常に先頭になり、確立ループ（最初の成功で早期
リターン）は常に現アドレスから試す。

`ResolvedNode.addresses` の型・公開契約（非 LL 先頭）は不変。変わるのは
同群内の並びだけ。

### エラー処理

純データ構造の変更で新たな失敗経路は無い。挙動変化は「順序」と「eviction」
のみ。ワイヤ形式・cache TTL 規律（SRV TTL 尊重、goodbye 短縮）・
`OperationalCache` の insert/get は無変更。

## テスト

単体（fold は `now` 注入で時刻固定）:

1. **パース**: cache-flush ビット有り/無しの AAAA を正しく区別する。
2. **ローテーション**: 旧アドレス学習 → 2 秒後に cache-flush 付き新アドレス
   → プールは新アドレスのみ。cache insert の先頭も新アドレス。
3. **burst 猶予**: cache-flush 付き AAAA 2 本が 1 秒以内の別データグラムで
   届く → 両方生き残る。
4. **鮮度順**: cache-flush 無しでも、後から見たアドレスが先頭に並ぶ
   （バックストップの順序保証）。
5. **starvation 根絶**: 8 本満杯の host に 9 本目 → 最古が evict され新規が
   学習される（現行の「新規拒否」挙動の反転を明示的に固定）。
6. **無退行**: 既存 fold テスト（per-host 化 #1a、goodbye、touched の
   cross-datagram 完成、非 LL 先頭ソート）が全て通る。

実機 E2E（ユーザー決定: 回帰スモークで可）:

- 隔離 matd スモーク（`*.new` 方式）: 全ノード購読確立 attempts=1、
  `matd status` 健全、WARN 0。アドレスローテーションの実機演出はしない
  （BR prefix 変更はメッシュ全体に影響するため）。

## リリース

- バージョン 1.19.0、fix ブランチ（`fix/tier2-aaaa-freshness`）。
- これまでの監査項目と同じ流儀: spec/plan を docs/superpowers/ に置き、
  実機スモーク合格後に main へマージ。

## スコープ外

- AAAA goodbye（TTL=0）や TTL 経過による能動的な失効（成功基準外。
  cache-flush 再広告で実質的に代替される）。
- `mat` 一発経路・`OperationalCache`（instance→ResolvedNode 層)の変更。
- 消費側（establish ループ）の変更。
