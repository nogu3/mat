# Phase 5 M2: CASE initiator + IM read/invoke 設計

2026-07-10 設計確定。親 spec は
`2026-07-10-phase5-backend-direction-design.md`（M1〜M6 の全体像）。
M1（TLV / メッセージ層 / AES-CCM / MRP、crate `mat-controller`）は実装済み・
ローカル chip-all-clusters-app と Thread 実機 Nanoleaf 2 ノードで E2E 合格済み。
**実装は本 spec の承認後、別セッションで実装計画（writing-plans）から始める。**
開発は長期ブランチ `matter-controller` 上で行う（main は本番につきマージしない、
ユーザー決定 2026-07-10）。

## 決定（ユーザー確定 2026-07-10）

1. **最小 KVS リーダを M2 に前倒しする。** CASE には fabric 資格情報
   （RCAC / ICAC / NOC / 操作鍵 / IPK）が必須のため、chip-tool の Linux KVS から
   fabric index 1 の 5 項目だけを読む最小リーダを M2 で書く。対象フォーマットは
   connectedhomeip **v1.4.2.0 固定**（Docker の chip-tool / example device と同一）。
   これに伴い **M3 を「KVS リーダの堅牢化（バージョン互換方針含む）+ jarvis
   実機相乗りで on/off・色変更」に再定義**する（M2 実装開始時に親 spec へ追記）。
2. **受け入れはローカルのみ。** jarvis 実機（本番 fabric への CASE）は M3。
   失敗時の切り分けを優先する。
3. アプローチは「セキュリティ正・機能最小」: CASE は spec どおり完全に
   （NOC チェーン検証・署名検証込み）、IM は read 単一属性 + invoke のみ。
   証明書検証の省略（検討済み）と matc 相当の機能幅（write/subscribe/resumption）
   は棄却。

## スコープ

`mat-controller` crate への追加のみ。mat / matd は無変更（adapter 差し替えは M4）。

| 新モジュール | 責務 |
|---|---|
| `kvs` | chip-tool Linux KVS（ini 形式・base64 値）から fabric index 1 の RCAC / ICAC / NOC / 操作鍵 / IPK を読む最小リーダ。v1.4.2.0 固定。読めない・鍵欠落は明示エラー（panic なし） |
| `cert` | Matter TLV 証明書のパース（subject node id / fabric id / 公開鍵 / TBS バイト列 / 署名）と発行者公開鍵による ECDSA 署名検証。X.509 変換はしない |
| `fabric` | `FabricCredentials`（root cert・NOC・ICAC(任意)・操作鍵・IPK・fabric id・node id）、compressed fabric id / destination id / IPK operational key の HKDF 導出 |
| `case` | CASE initiator 状態機械。M1 の `UnsecuredExchange` 上で Sigma1 → Sigma2 検証 → Sigma3 → StatusReport。出力 = セッション鍵 + session id ペア + peer 情報 |
| `session` | `SecureSession` + secured exchange。M1 の `seal_message`/`open_message`（Result シグネチャ）と MRP 意味論（再送・ACK・RxWindow 重複排除）を secured セッションへ接続 |
| `im` | 最小 Interaction Model: ReadRequest / ReportData（単一属性）、InvokeRequest / InvokeResponse + StatusResponse。onoff cluster (0x0006) の command/attribute 定数 |

**新依存**（暗号プリミティブは RustCrypto 既製のみ、親 spec どおり自作しない）:
`p256`（ECDH / ECDSA）、`sha2`、`hkdf`、`hmac`。

## CASE 確立フロー（`case`）

1. **Sigma1 送信** — ephemeral P-256 鍵生成。destination id を IPK operational
   key・root 公開鍵・fabric id・peer node id から導出。initiator random・
   提案 session id を含め TLV で組み、`send_reliable` で送信。
2. **Sigma2 受信** — 応答の `acked_counter` を明示確認する（M1 申し送り:
   `send_reliable` の戻りが自送信を ack した保証はない）。相手 ephemeral 鍵と
   ECDH → S2K 導出 → TBEData2 復号 → 相手 NOC を root 証明書でチェーン検証し
   node id / fabric id の一致を確認 → TBS 署名を NOC 公開鍵で検証。
3. **Sigma3 送信** — 自分の NOC（+ICAC）と操作鍵による TBS 署名を TBEData3 に
   暗号化して送信 → StatusReport(SessionEstablishmentSuccess) を受信。
4. transcript hash（SHA-256、Sigma1 から逐次）から HKDF で SEKeys
   （I2R / R2I / attestation challenge）を導出 → `SecureSession` 完成。

## SecureSession / IM フロー（`session` / `im`）

- `SecureSession::read_attribute(endpoint, cluster, attribute)` /
  `invoke(endpoint, cluster, command, payload)` が secured exchange で IM
  メッセージを往復し、応答 TLV を M1 の `tlv::Reader` で解析して返す。
- M1 申し送りの反映事項:
  - nonce の node id は **sender 方向**（送信 = 自 node id、受信 = peer node id）。
  - secured 受信経路では message header の **DSIZ 予約値 0b11 をエラー化**する
    （M1 では未使用経路のため素通し）。
  - `encrypt_payload` / `seal_message` は `Result` シグネチャ
    （`PayloadTooLarge`、M1 実行時訂正）。
- セッション再開（Sigma2Resume）は持たない。matd の warm 維持（M4）が
  レイテンシ要件を担うため、切れたら再確立でよい。

## エラー方針

- crate 内は各モジュールの小さなエラー enum（M1 と同形式、`Display + Error`）。
- `CaseError` は「どの Sigma 段階で・何が拒否されたか」を保持する。M4 で mat の
  エラー kind（`session_failed` / `device_rejected` 等）へ写像する材料になる。
  mat の JSON スキーマ・エラー kind・exit code は不変（親 spec の契約）。

## テスト戦略

- **既知ベクタ単体テスト**: compressed fabric id・destination id は Matter spec
  掲載のテストベクタで固定。TLV 証明書パースは合成ダミー証明書で CI 検証。
- **実 fabric フィクスチャはコミットしない**（repo は public）。ローカルで
  使い捨て fabric を作る手順（chip-tool でコミッション → KVS から抽出）を
  実装計画に記載し、それを使うテストは `#[ignore]`。
- **CASE 全体の正しさはライブ E2E が担う**（相手 = 実 CHIP スタック）。
  M1 の暗号 round-trip は自己整合のみだったが、M2 の CASE 成立が初の暗号
  相互運用証明になる（M1 spec の既知の限界を解消）。
- CI（`task test`）はデバイス・実資格情報なしで通る（M1 と同じ規律）。

## 受け入れ基準（M2、ローカルのみ）

`task e2e:m2`（新設）で以下が一括で通ること:

1. ローカル chip-tool で chip-all-clusters-app をコミッション（テスト fabric）
2. `kvs` リーダが chip-tool KVS から資格情報 5 項目を取得
3. CASE 確立（Sigma1/2/3 + StatusReport success）
4. `onoff toggle` invoke が成功し、デバイス状態が変わる
5. `on-off` read が変化後の値を返す

ライブテストは `#[ignore]`。既存の M1 テスト・既存 crate のテストは全通過のまま。

## 非ゴール

- Sigma2Resume / セッション再開、subscribe、write、timed invoke、
  チャンク読み（大きな属性）、attestation 検証、マルチ fabric。
- jarvis 実機・本番 fabric への CASE（M3）。group セッション（M5）。
- mat / matd の adapter 差し替え（M4）。公開 API の facade 化も M4 で判断。

## 未決事項（実装計画で詰める）

- chip-tool KVS の正確なキー名（`f/1/r` 系）と操作鍵のシリアライズ形式
  （SDK v1.4.2.0 の `PersistentStorageOpCertStore` / `PersistentStorageOperationalKeystore` /
  `GroupDataProvider` ソースで確定する）。
- CASE の session id 割当・SessionParams（MRP パラメータ交換）の扱いの詳細。
- IM メッセージの InteractionModelRevision 等の定数値。
