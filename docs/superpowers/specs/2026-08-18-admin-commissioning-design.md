# mat-device / matv — AdministratorCommissioning（ECM）+ fabric 後始末 設計

> 背景の実測: `docs/superpowers/plans/m2-echo-checklist.md` の「2026-08-18 追記」節。
> Android HA アプリ（Google Play Services が commissioner）の追加フローは、スマホが一時
> fabric で commission → **AdministratorCommissioning の OpenCommissioningWindow（ECM）で
> 窓を開けて HA サーバに引き渡す** multi-admin 方式。matv は cluster 0x3C 未実装のため、
> スマホが WindowStatus を 3 回ポーリングして諦め、ArmFailSafe(1) で後始末して中断する
> ところまで実測済み（それ以前 — attestation・AddNOC・CASE — は全て通過。Google は
> テスト証明書を受け入れる）。

## ゴール

スマホの HA アプリからの「Matter デバイス追加」が**端から端まで**通ること。

受け入れ条件:

1. Android HA アプリからの追加で、matv が HA の Matter server に登録され、HA 上で
   OnOff 操作できる（人間チェックポイント）
2. リグレッション: chip-tool ゲート（`task e2e:device:m2-chip`）green、
   matter-server WS ゲート（ローカル matter-server 1.1.7 + WS commission →
   OnOff トグル）green
3. スマホの一時 fabric が RemoveFabric で除去され、fabric 残留なし

## スコープ

- AdministratorCommissioning cluster（0x003C）: WindowStatus / AdminFabricIndex /
  AdminVendorId 属性、OpenCommissioningWindow / RevokeCommissioning コマンド
- PASE responder の ECM モード（verifier 素材ベース）
- runtime の窓状態機拡張（動的再 open + CM=2 広告）
- OperationalCredentials の UpdateFabricLabel（9）/ RemoveFabric（10）

スコープ外（明示）:

- OpenBasicCommissioningWindow（BC feature。Google/HA は ECM を使う）
- timed 必須コマンドの enforcement（NEEDS_TIMED_INTERACTION / TIMEOUT）— M3 送りの
  既定方針どおり。受理は timed/非 timed 両対応
- 窓状態の再起動またぎ永続化（spec 上不要）
- Administrator Commissioning の MaxCumulativeFailsafeSeconds 系の厳密化

## 1. 全体像

cluster 0x3C のハンドラは `CommissioningServer`（`core/commissioning.rs`）に追加する
（GeneralCommissioning 48 / OperationalCredentials 62 と同じディスパッチ・同じ
`Inner` 共有状態）。core は同期・純粋のまま、**窓オープンの副作用は runtime が
dispatch 後に検知して適用する**（AddNOC の fabric 差分検知、CommissioningComplete の
応答デコード検知と同じ既存パターン）:

- core: OpenCommissioningWindow 成功時に `pending_window_request:
  Option<WindowRequest>`（verifier 97B・discriminator・salt・iterations・timeout 秒・
  開けた admin の fabric_index / vendor_id）を stage
- runtime: dispatch 後に `take_pending_window_request()` で回収し、
  (a) PASE 受付を ECM 設定に切替、(b) mDNS commissionable 広告を
  D=<新 discriminator> + **CM=2** で再開、(c) 窓期限タイマーを timeout 秒に設定
- RevokeCommissioning / 期限満了 / CommissioningComplete 成功はいずれも窓を閉じ、
  mDNS goodbye（既存の `set_commissionable(None)` 配線を流用）

## 2. クラスタ仕様（spec §11.19）

属性（EP0）:

| id | 名前 | 型 | 内容 |
|---|---|---|---|
| 0 | WindowStatus | enum8 | 0=WindowNotOpen / 1=EnhancedWindowOpen（2=Basic は使わない） |
| 1 | AdminFabricIndex | fabric-idx, nullable | 窓を開けた admin の fabric index。閉窓時 null |
| 2 | AdminVendorId | vendor-id, nullable | 同 vendor id。閉窓時 null |

コマンド:

- **OpenCommissioningWindow(0)**: `{0: CommissioningTimeout(u16 秒), 1:
  PAKEPasscodeVerifier(97B octstr), 2: Discriminator(12bit), 3: Iterations(u32),
  4: Salt(octstr 16..32B)}`
  - 検証: 窓が既に開いていれば cluster status **Busy(2)**。verifier 長 ≠ 97、
    iterations が 1000..100000 の範囲外、salt 長が 16..32 の範囲外は
    **PAKEParameterError(3)**。timeout の下限/上限（180..900 秒）逸脱は
    INVALID_COMMAND（グローバル）
  - 成功: WindowStatus=1、AdminFabricIndex/AdminVendorId を呼び出しセッションの
    fabric から記録、`pending_window_request` を stage
- **RevokeCommissioning(2)**: 窓が閉じていれば cluster status **WindowNotOpen(4)**。
  成功で窓 close（runtime が goodbye）

InvokeResponse の**クラスタ固有ステータス**（status IB の cluster-status フィールド）を
エンコーダが未対応なら追加する（現状はグローバル status のみの可能性が高い）。

## 3. PASE の ECM モード

`PaseVerifierConfig` を 2 モード化する:

```rust
enum PaseSecret {
    Passcode(u32),                  // 起動時窓（QR の passcode から導出）
    VerifierMaterial([u8; 97]),     // ECM 窓（OCW の w0‖L をそのまま使う）
}
```

- ECM 中の SPAKE2+ は `Spake2pVerifier::from_verifier_material`（**実装済み**）を使う
- PBKDFParamResponse の salt / iterations は OCW で渡された値を返す
- QR の元 passcode での PASE 試行は verifier 不一致で自然に失敗する（明示拒否は不要）

## 4. runtime の窓状態機

```rust
enum CommissioningWindow {
    Closed,
    Open { until: Instant },                       // 起動時窓（既存, CM=1, QR passcode）
    EnhancedOpen { until: Instant, request: WindowRequest },  // ECM 窓（CM=2）
}
```

- 期限監視は既存の `commissioning_window_deadline` を流用（`until` を持つ variant 全て）
- 閉窓後に起動時窓へは戻らない（既存挙動を維持）
- mDNS: `CommissionableAdvert` に discriminator と CM 値を渡せるようにする
  （現状 CM=1 固定 → パラメータ化）

## 5. UpdateFabricLabel（62/9）/ RemoveFabric（62/10）

- **UpdateFabricLabel**: `FabricEntry` に `label: String` を追加（永続化 —
  fabrics.json の schema 追加。既存ファイルは label 欠落を空文字で読む後方互換）。
  NOCResponse(Ok, fabric_index) を返し、Fabrics 属性の読みに Label を反映
- **RemoveFabric**: `{0: FabricIndex}`。存在しなければ NOCResponse(InvalidFabricIndex=0x0A)。
  成功時: NOCResponse(Ok) を**先に返してから** store 削除 + 当該 fabric の operational
  mDNS 広告撤去。削除対象が現行 CASE セッションの fabric の場合（スマホの自己削除が
  このケース）、応答送信後にセッションを終了する

## 6. テストと受け入れ

- TDD: 各ハンドラのユニット（OCW の検証分岐・Busy・Revoke・属性読み）、PASE ECM の
  ユニット（verifier 素材でハンドシェイク成立 / 元 passcode は失敗）、
  UpdateFabricLabel / RemoveFabric のユニット（後方互換読み込み含む）
- runtime 統合テスト: 「OCW → ECM 窓が開き、verifier ベース PASE で新コントローラが
  commission できる」「Revoke / 期限満了で goodbye」
- リグレッション: `cargo test --workspace` + chip-tool ゲート + matter-server WS ゲート
- 最終受け入れ: スマホ HA アプリからの追加が端から端まで通る（人間チェックポイント）

## 完了時の申し送り（2026-08-19）

**実装は全タスク完了・final review Approve（コード指摘なし）**。自動ゲート: workspace 1088 テスト + clippy / chip-tool フルゲート PASS / **ECM 2 コントローラ E2E PASS**（matter-server 1.1.7 ×2、jarvis 実機: commission → OCW → 第 2 コントローラが verifier PASE で commission → 操作。UpdateFabricLabel も実機確認）。

**スマホ実機 E2E は 99% 到達で保留**: 実測（2026-08-18 21:59）でスマホの commission → CASE → **OpenCommissioningWindow invoke → 本物の HA サーバが ECM 窓から PASE 接続**まで全て成功。HA サーバが attestation でテスト証明書を即拒否して中断 — 残る前提は **HA の Matter Server アドオンで Test Net DCL を有効化**することのみ（ユーザー操作待ち）。

**M3 送り（final review の deferred minors）**:
- runtime の単一セッション制約: 旧セッション経由の RemoveFabric が不達（スマホの一時 fabric が zombie 化しうる。手動 RemoveFabric で回収可能）→ multi-session 化
- OCW の Busy 判定順序が spec §11.19.8.1 と逆（Busy より先にパラメータ検証）
- WindowRequest が Debug/Clone 派生（verifier 素材の保護一貫性）
- 最終 fabric 削除後に再 commissioning 不能（要 restart。spec は commissioning mode 再進入を期待）
- DropSession 伝播・mDNS retry の seam 自動テスト無し（pure-fn テスト + 実機ゲートでカバー中）

**検証リグ（再利用可）**: jarvis `~/ms-test`（matter-server 1.1.7 npm 導入済み、`--enable-test-net-dcl` で起動）、`~/matv-ha/`（matv 手動起動、discriminator 2314 / passcode 63852174）。WS 自動化スクリプトの手順は plan の Task 7 参照。
