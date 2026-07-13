# Phase 5 M6b: BTP/BLE コミッショニング + Thread dataset 書き込み設計

2026-07-13 起草。親 spec: `2026-07-13-phase5-m6a-commissioning-design.md`
（M6 の a/b 分割はそこで決定）。前提: M1〜M6a 実装・実機 E2E 合格済み。
ブランチは長期ブランチ `matter-controller`（main マージ禁止）。

M6b = 工場出荷状態デバイスの native commission。BLE 発見 → BTP 上で PASE →
attestation/NOC（M6a 流用）→ Thread operational dataset 書き込み
（NetworkCommissioning）→ Thread 網参加 → CASE → CommissioningComplete。
これで chip-tool が持つプロトコル機能の native 置換が完成する。**本番切替
自体は native 版 mat トラック**（既決定 2026-07-13: 本番 matd の native 化は
native 版 mat 着手時に一括）であり、M6b では本番経路に一切触れない。

## ゴール

1. BLE central として commissionable デバイスを発見する（service UUID
   0xFFF6 の advertisement service data から discriminator フィルタ）。
2. BTP（Bluetooth Transport Protocol）の自前実装で GATT 上に Matter
   メッセージの土管を作り、その上で既存 PASE を成立させる。
3. NetworkCommissioning クラスタで Thread operational dataset を書き込み、
   ConnectNetwork でデバイスを Thread 網に参加させる。
4. 参加後は M6a と同じ operational mDNS 待ち → CASE →
   CommissioningComplete。
5. 本番 `mat commission` / matd は**無変更**（ライブラリ + E2E のみ）。

## 決定

### 決定 1: BLE アクセスは bluer crate、BTP プロトコル層は自前

フルスクラッチ方針は Matter プロトコルの所有性の話であり、OS 資源アクセスは
従来も crate（UDP=tokio/socket2、曲線=p256、乱数=getrandom）。同じ線引きで
GATT 接続は bluer（BlueZ 公式 Rust binding、tokio ネイティブ）、BTP の
handshake / セグメンテーション / ACK は自前実装とする（ユーザー決定
2026-07-13）。

却下した代替: btleplug（クロスプラットフォーム抽象が現ターゲット =
jarvis/Linux には不要）、zbus で BlueZ D-Bus 直叩き（依存最小だが BlueZ
D-Bus API の実装量が M6b 最大の工数塊をさらに膨らませる）。

### 決定 2: 新規モジュール構成

| モジュール | 責務 | 依存 |
|---|---|---|
| `ble.rs` | bluer による BLE central: スキャン（0xFFF6 service data → discriminator 一致）、接続、GATT C1 write / C2 indication 購読。`GattLink` 実装を提供 | bluer |
| `btp.rs` | BTP プロトコル: handshake（バージョン/MTU/ウィンドウ交渉）、セグメント分割/再構成、ウィンドウ/ACK、keepalive。GATT 操作は trait `GattLink`（write + indication stream）越し | `ble.rs`（実体）、モック（テスト）|

既存モジュールへの拡張:

- **exchange 層**: UDP / BTP を差し替えられる channel 抽象を導入。BTP 経路は
  **MRP 無効**（BTP 自身が再送・ACK を持つ、Matter spec 準拠）。
  `session.rs` / `im.rs` / `pase.rs` は**変更なしが目標** — M6a 決定 1
  （PASE は既存基盤のもう一つの鍵導出）と同型の「BTP は既存基盤のもう一つの
  土管」。
- **commissioning.rs**: `commission_ble_thread(...)` を追加。ステップマシンは
  M6a の on-network 版と共有し、差分は (a) 発見と transport（mDNS/UDP →
  BLE/BTP）、(b) AddNOC 後に NetworkCommissioning ステップ挿入、のみ。

### 決定 3: commission_ble_thread のデータフロー

```
入力: passcode+discriminator（または setup code 文字列）,
      Thread operational dataset (hex bytes),
      fabric 素材（RCAC 鍵・fabric_id・IPK）, PAA ストアパス, 新 node_id,
      operational 発見用 iface
 1. ble: スキャン → discriminator 一致デバイスへ GATT 接続
 2. btp: handshake → GATT 上の Matter メッセージ土管
 3. pase: 既存 PASE を BTP 経路で実行 → SecureSession
 4. im over PASE/BTP: ArmFailSafe → SetRegulatoryConfig（任意扱い、M6a
    決定 7 踏襲）→ attestation → CSR → AddTrustedRootCertificate → AddNOC
    （すべて M6a のステップをそのまま流用）
 5. im: AddOrUpdateThreadNetwork(dataset) → ConnectNetwork
    → ConnectNetworkResponse の status 確認
 6. btp/ble: 切断（以後は IP）
 7. dnssd: operational advertise 待ち（リトライ付き）→ case.rs で CASE 確立
 8. im over CASE: CommissioningComplete → 完了
出力: CommissionedNode（M6a と同型）
```

Thread dataset は **hex bytes 引数**。E2E ハーネスが OTBR から
`ot-ctl dataset active -x` で取得して渡す。ライブラリは OTBR を知らない
（M6a 決定 4「素材は呼び出し側」原則の踏襲）。

### 決定 4: エラーは CommissionError の拡張、写像表も拡張

追加: `Ble(Scan | Connect | Btp)` / `NetworkConfig(status, debug_text)`
（status = ConnectNetworkResponse / NetworkConfigResponse の
NetworkCommissioningStatus 生値。debug_text も detail に保持）。

| CommissionError | ErrorKind | exit |
|---|---|---|
| `Ble(Scan)`（発見 timeout） | `timeout` | 3 |
| `Ble(Connect \| Btp)` | `unreachable` | 5 |
| `NetworkConfig(*)` | `commission_failed` | 1 |
| 既存（`Attestation` / `Pase` / …） | M6a 決定 5 の表のまま | 〃 |

### 決定 5: failsafe 延長 — ConnectNetwork 前に ArmFailSafe 再送

Thread 参加 + operational 発見は 120s を超えうる。ConnectNetwork の直前に
ArmFailSafe を再送して期限を仕切り直す（M6a リスク 2 で用意した延長機構の
流用）。失敗時は明示 revert せず failsafe 満了に任せる（M6a 決定 6 踏襲）。

## テスト

- **ユニット**: BTP handshake のバイト列、セグメント分割/再構成、ウィンドウ
  ACK（モック `GattLink` + connectedhomeip の BTP テストと突合した自作
  ベクタ）。NetworkCommissioning コマンド（AddOrUpdateThreadNetwork /
  ConnectNetwork）の TLV 組立/応答解釈。setup code の discovery capability
  bit（BLE）解釈。
- **統合（ローカル、実 BLE なし）**: モック `GattLink` 上で BTP handshake →
  既存 PASE フルハンドシェイクが通ることを確認（テスト側に BTP responder の
  最小実装を持つ）。WSL 開発機に BLE が無いため実 BLE のローカル統合は
  なし。既存 `task e2e:m6`（M6a ローカル 3 項目）は回帰として維持。
- **実機（受け入れ、jarvis = Raspberry Pi 3+ / BlueZ）**:
  1. **preflight**: hci0 / BlueZ バージョン確認、bluer によるスキャン小
     ツールで Matter デバイスの BLE advertise が観測できること（既存
     デバイスの open-window 中 advertise の有無もここで観察）。
  2. **本命**: 玄関ライト（entrance_light、ユーザー指定 2026-07-13）を工場
     リセット → native BLE+Thread commission で使い捨て fabric へ →
     使い捨て fabric の CASE + onoff 確認 → native open-window → 本番
     `mat commission`（chip-tool 経路）で本番 fabric へ join →
     使い捨て fabric を RemoveFabric で撤収 → alias・本番疎通の復元確認。
     工場リセットは 1 回で済み、本番復帰は M6a 実機で実証済みの
     multi-admin 機構の逆向き。

## 受け入れ基準

1. ユニットテスト全合格（BTP / NetworkCommissioning TLV）。
2. ローカル統合（モック GattLink 上の BTP+PASE）合格、`task e2e:m6` 回帰
   合格。
3. 実機手順（玄関ライト）合格 — 本番 fabric・本番 matd・他ノードに影響
   ゼロ、玄関ライトが本番 fabric に復帰し既存経路で疎通。
4. `task check` 合格（fmt / clippy -D warnings / 全テスト）。

## 非ゴール

- Wi-Fi credentials 書き込み（AddOrUpdateWiFiNetwork）。対象デバイスは
  Thread のみ。
- BLE peripheral 側（open-window 時に自分が BLE advertise する側になる
  機能。controller には不要）。
- 複数デバイス同時 commission、本番経路の native 切替、fabric 素材の永続化
  （native 版 mat トラック）。

## リスク

1. **Pi 3+ の BLE 品質 / BlueZ バージョン差** — preflight で先に潰す。
   接続不安定ならリトライ（スキャン→接続→BTP handshake 単位でやり直し）。
2. **BTP 自前実装の正しさ** — connectedhomeip の BTP テスト・パケット
   フォーマットと突合した自作ベクタで担保。rust-matc / SDK は参照のみ。
3. **玄関ライトの復帰失敗** — 復帰手順（open-window → 本番 join）は M6a
   実機で実証済みの機構。最悪でも Nanoleaf アプリから再セットアップ可能。
4. **Thread 参加所要時間のばらつき** — failsafe 再送延長（決定 5）+
   operational 発見リトライで吸収。
