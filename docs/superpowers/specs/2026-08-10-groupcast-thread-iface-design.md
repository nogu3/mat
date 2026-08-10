# groupcast の Thread iface 直送（二重送出）設計

2026-08-10。実機調査で確定した問題への恒久対応。

## 背景 / 問題

mat / matd の groupcast は唯一の運用 iface（例: eth0）へ multicast を送出する。
Thread mesh への配達は **LAN 上の Primary BBR（他社デバイス）による中継**に依存し、
jarvis 実機では node 17 (Onvis S4) へ恒常不達（0/10）・node 6 (NL68) へも約 50%
ロスだった。jarvis 自身が OTBR（wpan0）なのに、その MPL 直注入経路を使っていない。

実証: matd 停止 + `MAT_MATD=0 MAT_IFACE=wpan0 mat group invoke ...` で両ノード
2/2 配達（read で状態反転確認）。デバイス側設定（group-table / key-map / ACL /
keyset）は全数正常で、送出経路だけの問題。

unicast は OMR アドレス宛でカーネル経路表が既に wpan0 へ流しており変更不要。
mDNS resolve は infra（LAN）側の世界で Thread 化は原理的に不可能・対象外。
**本設計のスコープは groupcast の egress iface のみ。**

## 設計

### 1. Thread iface の決定（プロセス起動時に 1 回）

優先順:

1. 明示指定: `--thread-iface` / env `MAT_THREAD_IFACE`（matd は
   `--thread-iface` / `MAT_MATD_THREAD_IFACE` ミラー）
2. 自動検出: **名前が `wpan` で始まる up な iface がちょうど 1 本**あればそれ
   （OTBR TUN の既定名。tailscale0 等の P2P iface とは名前で衝突しない）
3. ゼロ or 複数候補: 「Thread iface なし」= 従来動作（LAN 単独送出）

実装は `mat-native::iface_select` に `detect_thread_iface()` を追加。

### 2. 送出: egress リスト方式

`mat-controller::group::GroupSender` を「egress のリスト」を持つ形に変更する。

- **LAN egress**: 従来どおり共有 transport（unicast と同居、
  `IPV6_MULTICAST_IF` = 運用 iface）
- **Thread egress**（決定できた場合のみ): groupcast 専用に bind した第 2
  ソケット + Thread iface の scope_id

datagram は counter 1 個で 1 回だけ組み立て、**同一バイト列を全 egress に
sendto** する。重複コピーは受信側のリプレイ保護が同一カウンタとして捨てるので
無害。Wi-Fi Matter デバイスがグループに混在する構成は LAN 側コピーが担保する。
counter 永続化・`bump` は無変更。

### 3. 失敗時の規律（既存の設定思想と同じ）

- **明示指定**の thread iface が解決・bind できない → ハードエラー
  （`mat` 直経路は起動拒否。`matd` は M8c-3 の既存 native 構築失敗ハンドリング
  に合流し、起動は継続するが以後の全 op がこのエラーを返す — silent degrade
  はしない・`matd status` で可視）。明示設定は黙って劣化しない。
- **自動検出**の thread iface が bind できない → warn ログ + LAN 単独で続行
  （auto は best-effort、absent = 挙動不変の既存規律）。
- 送出時に片方の sendto が失敗 → 他方が出せていれば `"sent"`（warn ログ）。
  全 egress 失敗で初めてエラー。

### 4. 可観測性

- tracing: `groupcast sent (native) group_id=.. counter=.. egress=eth0+wpan0`
- 出力 JSON（`group invoke` / `color` / `level` / `color-temp`）に
  `"egress": ["eth0", "wpan0"]` フィールドを追加（後方互換な追加）
- matd 起動ログに thread_iface を出す
- docs/commands.md とスキーマ文書を更新

### 5. テスト

- unit: `detect_thread_iface()` — wpan 一意で採用 / 複数で不採用 /
  tailscale0 非マッチ / down は不採用
- unit: egress リスト構築の分岐（明示 / auto / なし、明示失敗 = ハード、
  auto 失敗 = 劣化続行）
- loopback: 既存の group ループバックテスト基盤で、2 egress へ同一バイト列が
  配達されることを固定
- **実機 E2E（マージ前必須）**: jarvis へ隔離 matd でデプロイし、
  (a) 無設定で wpan0 が自動採用されるログ確認、
  (b) desk group off/on×2 が node 6 / node 17 とも read で状態反転すること

## 非スコープ

- mDNS / resolve の Thread 化（原理的に不可能）
- unicast の経路変更（既に直 Thread）
- SRP サーバ直接参照などの OTBR 同居ホスト専用の裏口
- mando 側の設定変更（本対応で不要になる）
