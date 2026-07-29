# warm session の UDP ソケットをノードごとに分離（安定性監査 Tier 1 #3）

- 日付: 2026-07-29
- 対象: `crates/mat-native/src/lib.rs`（主）、`crates/mat-controller/src/`（テスト
  足場の共有化のみ）
- 出自: 2026-07-25 の安定性監査バックログ Tier 1 #3
  「warm session が全ノードで UDP ソケット 1 本共有」

## 背景と問題

`Engine::build`（`lib.rs:344`）は UDP ソケットを 1 本 bind し、`CaseEstablisher`
が全ノードの CASE 確立とその後の全 op でこれを共有する。matd は per-node slot
（`matd/src/native.rs` の `with_session`）で warm session を保持し、**異なる
ノードへの op は並行**に走る（M4 spec の契約）。

その結果、並行 op では複数の `SecureSession` が同一ソケットで `recv_from` を
奪い合う。勝った側が他ノード宛のデータグラムを受信すると、`screen_with` の
foreign-address フィルタ（`session.rs:289` の `from != self.peer`）で **黙って
破棄**する。本来の宛先セッションは受信機会を永久に失い、デバイス側の MRP 再送
頼みになる。最悪ケースでは op が timeout → matd が slot を捨てて再確立（session
churn）と、無駄な再送・レイテンシ・確立チャーンが並行度に比例して発生する。

「単一 socket で複数 peer を捌ける」（M4 spec の当初想定）と「異なるノードへの
op は並行」は両立しない。一方、**購読側は既にこの問題を踏まえて解決済み**:
`establish_subscription`（`lib.rs:450-459`）はノードごとに専用 `UdpTransport` +
専用 CASE を確立し、local port を info ログで可視化している。op 側だけが規律の
適用漏れであり、設計変更は不要。

発生頻度は未確認（監査時点の注記どおり「機構は確定・頻度は未確認」）だが、
機構上、並行 op が重なった瞬間に必ず起きる。

## 検討した代替案

- **単一ソケット + 中央 demux タスク**: 受信を 1 タスクに集約し peer / session id
  で各セッションの channel に配送する。fd は増えないが、受信ループの所有権を
  全セッション・購読側から奪う大改造で、既に専用ソケット化した購読側と二重構造に
  なる。ノード数が数百になるまで不要（YAGNI）。却下。
- **op のグローバル直列化**: M4 spec の「異なるノードへの op は並行」を壊す退行。
  却下。

## 設計

### establish() のノード専用ソケット化

`CaseEstablisher::establish`（`lib.rs:405`）を `establish_subscription`
（`lib.rs:450-459`）と同じ形にする:

1. 冒頭で `UdpTransport::bind()` → `Arc<Transport::Udp(..)>` をノード専用に作る。
   bind 失敗は既存の購読側と同じく `ErrorKind::Other` に写像。
2. `case::establish` へは共有 transport ではなくこの専用 transport を渡す。
3. CASE 成功時に
   `tracing::info!(node_id, local, %peer, "op transport bound (dedicated socket + CASE)")`
   を出す（購読側の `subscription transport bound` と対をなす。`ss -uanp` /
   tcpdump 突合の鍵）。

### 共有 transport の後始末

- `CaseEstablisher.transport` フィールド（`lib.rs:398`）は用途がなくなるので削除。
- `Engine::build` の `UdpTransport::bind()` 自体は **GroupCtx（multicast group
  送信）専用として残す**。`Transport::Udp` へのラップ（`lib.rs:373`）だけ消える。

### セッションと fd のライフサイクル

`SessionConn` が `SecureSession` 経由で専用 transport を所有する。matd が slot を
捨てる（`*guard = None`）とソケットも閉じ、fd 使用量は warm session 数に厳密に
一致する（+1 fd / warm ノード。jarvis 規模で +15 程度、購読側の専用ソケットと
同水準）。リーク経路はない。

### 影響範囲

- matd の warm op 経路と `mat` 一発直経路の両方がこの `establish` を通り、両方が
  専用ソケット化される。`mat` 一発は単ノードなので挙動変化なし（プロセス寿命中
  fd が +1 になるだけ）。
- commissioning / PASE / BLE / group 送信 / 購読の各経路は不変。
- matd 側（`with_session` / slot 管理）は無変更。

## テスト

### 応答器足場の共有化（mat-controller）

`tests/case_self_handshake.rs` の CASE 応答器（`responder_task` と付随ヘルパ・
fixtures 定数）を `mat-controller` の `#[doc(hidden)] pub mod test_support` へ
抽出し、cargo feature `test-responder` で gate する（プロダクションビルドには
入らない。mat-native は `[dev-dependencies]` で
`mat-controller = { workspace = true, features = ["test-responder"] }` を張る —
dev-deps の feature はテストビルドでのみ有効化される）。既存の
`case_self_handshake.rs` は抽出後の足場を使う形に書き換え、テスト内容は不変。

### 並行 2 応答器テスト（mat-native、本命）

`lib.rs` の `#[cfg(test)]` ユニットテスト（私有の `CaseEstablisher` に触れる）で:

1. ループバック上に CASE 応答器を **2 つ**（別ポート、node_id 別）立てる。
2. `Resolver` trait の fake（固定 addr を返す）を注入した `CaseEstablisher` で、
   `tokio::join!` により **並行に** establish + IM read を実行。
3. 両ノードとも成功することを釘打つ。専用ソケット化により決定的に通る。共有
   ソケットに退行すると screen 破棄 + MRP 再送で flaky / timeout 化して検知できる。
4. 2 セッションの local port が異なることも assert する（構造の直接確認）。

### 既存テストの無風確認

FakeConn / バイナリ統合テスト群が修正なしで通ること（op の意味論は不変）。

## 実機 E2E（マージ前必須）

jarvis 上の隔離 matd（別 socket + store コピー）方式で:

1. 台帳 2 ノード以上で隔離 matd を起動。
2. 2 ノードへの op を並行発行（`MAT_MATD_SOCKET=<隔離socket> mat read ...` を
   `&` で同時起動）し、
   両方成功することを確認。
3. `op transport bound (dedicated socket + CASE)` ログと `ss -uanp` で matd の
   UDP ソケットがノードごとに分かれていることを確認。
4. 通常 op / listen のスモーク（既存挙動の無風確認)。

## バージョン / リリース

- 1.8.0（挙動変更: ソケット分離）。
- `task check` 合格 + 実機 E2E 合格 → main マージ → jarvis デプロイ。

## 受け入れ基準

- 異なるノードへの並行 op が互いの応答を破棄しない（並行 2 応答器テストで釘打ち、
  実機 E2E で確認）。
- op セッションの local port が確立時に info ログで可視化される。
- fd 使用量が warm session 数 + 購読数 + group 1 本に一致し、slot 破棄で解放される。
- commissioning / group / 購読の各経路に挙動変更がない（既存テスト無風で確認）。
