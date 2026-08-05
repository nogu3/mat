# probe 並行 resolve の unicast 応答喪失修正 — 単一共有ソケット + demux（監査⑩ 完結）

日付: 2026-08-05 / 対象バージョン: 1.21.0（窓統一と同一ブランチ
`fix/tier2-probe-resolve-timeout`、同一リリース）

## 背景 / 問題

窓統一（3s→8s、同日 spec 参照）の実機 E2E で、established 14 ノード中 10
ノード（node5–14）が旧・新バイナリとも同一に `reachable:false` になる既存バグが
露呈した。jarvis 上の pcap（`udp port 5353`、probe 実行中）で真因を確定:

1. **avahi（SRP advertising proxy）は QU ビットに従い unicast で応答する。**
   node5–14 への応答は毎秒 SRV+TXT+AAAA 完備で送出されているが、宛先は
   `ff02::fb` ではなく問い合わせ元アドレス:5353 への unicast（`lo In`）。
   `bind_mdns_socket` の設計前提「responder はどうせ multicast で返す」
   （2026-07-19 の OTBR proxy 捕獲に基づく）は avahi の unicast 応答経路では
   成り立たない。
2. **unicast は同一ポート多重 bind の 1 ソケットにしか配達されない。**
   probe はノード毎に `[::]:5353` を `SO_REUSEADDR` で並行 bind するため
   （E2E 時 15 本）、各 unicast 応答は最新 bind の 1 ソケットだけに届き、
   受け取った resolver は自分の instance 名フィルタで他ノード宛の答えを黙殺する。
3. **無応答ノードのソケットがブラックホール化する。** 解決済みソケットは閉じて
   「最新 bind」が繰り下がるため、bind 順の後ろから 1 秒 1 ノードずつ
   カスケード解決（観測: 19→17→16。18 はデバイス自身の multicast 応答で即時）
   した後、永遠に応答が来ない node15 のソケットが残り窓の unicast を全て吸い、
   node5–14 が全滅する。観測の完全な再現性（常に `{16,17,18,19}` のみ true、
   単発 `diag node --deep` は成功）をすべて説明する。

診断ツールとして probe が「CASE なら届くノード」を系統的に `reachable:false` と
誤報する — 監査⑩の実体はこれであり、窓統一だけでは直らない。

## 成功基準（ユーザー承認済み）

1. jarvis 実機で `discover --probe` が established ノードをすべて
   `reachable:true` と報告する（node15 等の真の mDNS 未解決のみ `false`）。
2. unicast-only 応答・複数 instance の demux がユニットテストで釘打ちされる。
3. 単発 resolve（establish / diag / commissioning）の挙動は不変。

## 決定

**単一共有ソケット + instance 名 demux の `resolve_operational_many` を
`mat-controller::dnssd` に新設し、probe の JoinSet 並行実行を置き換える。**
ソケットがプロセス内で 1 本なら「unicast がどのソケットに届くか」問題自体が
消滅する（avahi / matd 常駐ソケットより後に bind するため unicast も届く —
単発 diag が成功していたのと同じ位置取り）。

- 却下: resolve の直列化（14 ノード × 8s = 最悪 ~2 分。診断ツールとして論外）。
- 却下: bind 順の制御や SO_REUSEPORT 等のソケット小細工（配達規則への依存を
  深めるだけで、根本の「1 応答 1 ソケット」は変わらない）。

## 設計

### 1. `resolve_operational_many`（`crates/mat-controller/src/dnssd.rs`）

```rust
pub async fn resolve_operational_many(
    scope_id: u32,
    compressed_fabric_id: &[u8; 8],
    node_ids: &[u64],
    timeout: Duration,
) -> Result<Vec<(u64, Result<ResolvedNode, DnssdError>)>, DnssdError>
```

- 外側 `Err` = ソケット bind / send の I/O 失敗（全ノード共倒れの環境問題。
  例: `MAT_IFACE=lo` で multicast send 不能）。呼び出し側はこれを従来の
  「全ノード Io エラー → `Unreachable`」判定の代替として使う。
- 内側 per-node = `Ok(ResolvedNode)` / `Err(Timeout)`。
- 実装: `bind_mdns_socket` を 1 回だけ呼び、ノード毎の fold 状態
  （srv / txt / aaaa / aaaa_queried — 現 `resolve_operational` のローカル変数を
  per-node 構造体に持ち上げたもの）を map で管理する。1 秒毎に**未解決ノード
  だけ** SRV+TXT クエリを再送（パケットはノード毎に分離 — 質問の合積は MTU
  超過の別問題を持ち込むため不採用）。受信 datagram は全レコードを instance 名
  （SRV/TXT）と SRV target 名（AAAA）で該当ノードへ振り分ける。SRV 判明済み・
  アドレス未着のノードには従来同様 AAAA follow-up を 1 回だけ送る。
  SRV + アドレス ≥1 が揃ったノードから完成（非 LL 優先ソート等は既存ヘルパを
  そのまま使用）。全ノード完成で早期 return、deadline で残りは `Timeout`。

### 2. `resolve_operational` は thin wrapper 化

既存シグネチャを維持したまま、内部を
`resolve_operational_many(scope_id, cfid, &[node_id], timeout)` の委譲に
置き換える（外側 `Err` は `DnssdError` をそのまま伝播、内側の単一要素を返す）。
単発と並行が同一エンジンになり、挙動乖離が構造的に不可能になる。既存の
multicast-only 応答実機テスト（`resolve_operational_receives_multicast_only_response`）
が wrapper 経由でエンジンを釘打ちする。

### 3. `crates/mat/src/probe.rs`

JoinSet + per-node `resolve_operational` 呼び出しブロックを
`resolve_operational_many` の単一 await に置き換える。

- 外側 `Err(DnssdError::Io)` → 従来の `all_io_err` 分岐と同じ
  `ErrorKind::Unreachable`（メッセージも同趣旨を維持）。それ以外の外側 Err は
  従来どおり個別扱いにならないため同様に写像（bind 失敗も Io）。
- per-node `Timeout` / 成功の扱い・出力スキーマ・`StoreMissing` 分類は不変。
- モジュール doc の M8b 記述に「単一共有ソケット化（unicast 応答の配達先問題、
  監査⑩ 完結）」を追記。

### 4. `bind_mdns_socket` の doc 修正

「responder は multicast で返すのでそれに依る」という誤った前提の段落を実測に
合わせて更新する: multicast join は デバイス直接応答（multicast）受信のために
引き続き必須、加えて avahi は QU に unicast で応答するため、**並行 resolve は
ソケットを共有しなければならない**（unicast は最新 bind の 1 ソケットにしか
届かない）という規律を明記。コード変更はなし。

### 5. 変えないもの

- matd の `CachingResolver` / 常駐 `OperationalCache`（別経路、影響なし）。
- `commissioning.rs` の resolve リトライループ（単発 = wrapper 経由で挙動不変）。
- `browse` / `resolve_commissionable` / `resolve_all`（commissionable 系。単一
  ソケットで既に動作しており対象外）。
- `OPERATIONAL_RESOLVE_TIMEOUT`（8s、窓統一 spec のまま）。
- 出力スキーマ・エラー分類・exit code。

## テスト

- **新規**: unicast-only フェイク responder（テスト内 UDP ソケットが 5353 宛
  unicast で応答）× 複数 instance に対し `resolve_operational_many` が全
  instance を解決する（demux + unicast 受信の釘打ち。既存 multicast-only
  テストの対）。応答を返さない instance を 1 つ混ぜ、その 1 つだけ
  `Timeout` になること（ブラックホール根絶の直接検証）。
- 既存テスト全通過（`task check`）。特に
  `resolve_operational_receives_multicast_only_response` が wrapper 経由で通る
  こと。
- **実機 E2E（マージ前必須）**: jarvis で `mat.new discover --probe` を再実行し、
  窓統一 E2E の判定基準に加えて **established 14 ノード全て `reachable:true`
  （node15 のみ false）** を確認。所要は全ノード即応なら数秒以内に短縮される
  はず（8s 頭打ちは未解決ノードが残る場合のみ）。

## 影響範囲

`crates/mat-controller/src/dnssd.rs`（`resolve_operational_many` 新設 +
`resolve_operational` wrapper 化 + doc 修正 + テスト）、
`crates/mat/src/probe.rs`（呼び出し置き換え + doc）。バージョンは 1.21.0 の
まま（同一リリースに同梱）。
