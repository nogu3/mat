# Phase 5 M2b: 自己発行 operational identity で CASE を通す 設計

2026-07-11 起草（未承認）。親 spec: `2026-07-10-phase5-m2-case-im-design.md`（M2 本体）
および `2026-07-10-phase5-backend-direction-design.md`（M1〜M6 全体像）。
**実装は本 spec の承認後、writing-plans から始める。** ブランチは長期ブランチ
`matter-controller`（main マージ禁止、ユーザー決定 2026-07-10）。

## 背景 — なぜ M2 が受け入れで詰まったか

M2（CASE initiator + IM read/invoke、モジュール 1〜8）は実装・レビュー完了し
`matter-controller` に積んである（head 6ee034e、最終 whole-branch レビュー通過）。
しかし M2 の受け入れ（`task e2e:m2`）が **spec 前提の誤り**で走らない:

M2 spec は「chip-tool の Linux KVS から fabric index 1 の RCAC / ICAC / NOC /
**操作鍵** / IPK を読んで CASE する」と決めていた。だが connectedhomeip
v1.4.2.0 の chip-tool は、**自分自身の commissioner identity の操作鍵を KVS に
永続化しない**。SDK 実測（`examples/chip-tool/commands/common/CHIPCommand.cpp:481`）:

```cpp
chip::Crypto::P256Keypair ephemeralKey;                       // スタックローカル
...
// TODO - OpCreds should only be generated for pairing command
//        store the credentials in persistent storage, and
//        generate when not available in the storage.          ← SDK 自身の TODO
ephemeralKey.Initialize(chip::Crypto::ECPKeyTarget::ECDSA);    // 毎起動 新規生成
mCredIssuerCmds->GenerateControllerNOCChain(..., ephemeralKey, rcac, icac, noc);
commissionerParams.operationalKeypair = &ephemeralKey;         // 永続化されない
```

leaf NOC も毎起動 `ephemeralKey` の公開鍵に対して再発行される。したがって
`f/1/o`（PersistentStorageOperationalKeystore のキー）は chip-tool 自 identity
には存在せず、M2 の kvs リーダは `KeyMissing("f/1/o")` で止まる（CASE/IM 層は
到達すらしない）。これは実装バグではなく、M2 spec の前提が SDK 実挙動と食い違う。

**この問題は M3（jarvis 相乗り）でも同じく発生する** — 本番 fabric でも
chip-tool の identity をそのまま借りることはできない。したがって「自前で
operational identity を用意する」capability は、ローカル受け入れだけでなく
本番相互運用の前提でもある。

## chip-tool KVS に**実際に永続化されている**もの（SDK 実測）

chip-tool の `ExampleOperationalCredentialsIssuer`（`src/controller/`）と
`ExamplePersistentStorage` が ini KVS（alpha identity は
`chip_tool_config.alpha.ini`、セクション `[Default]`、値 base64）に永続化する:

| KVS キー | 内容 | 形式 |
|---|---|---|
| `ExampleOpCredsCAKey<idx>` | **root CA 鍵**（発行者） | 生 `P256SerializedKeypair` = 公開鍵65 ‖ 秘密鍵32 = 97B（**TLV ラップ無し**） |
| `ExampleOpCredsICAKey<idx>` | 中間 CA 鍵 | 同上 97B 生 |
| `ExampleCARootCert<idx>` | RCAC | Matter TLV 証明書 |
| `ExampleCAIntermediateCert<idx>` | ICAC | Matter TLV 証明書 |
| `LocalNodeId` | controller の node id | u64 LE（未設定時は `kTestControllerNodeId`） |
| `f/1/k/0` | group keyset（IPK operational key を含む） | M2 kvs リーダで既読 |

- `<idx>` サフィックスは発行者インデックス（`PERSISTENT_KEY_OP` マクロが
  `snprintf(key, "%s%" PRIx64, keyPrefix, node)` で付ける小文字16進）。**正確な
  値は実装計画で実 ini ダンプに突き合わせて確定**（未決事項）。
- **fabric id = 1**（alpha identity = `kIdentityAlphaFabricId`）。
- CA 鍵は `f/1/o` の TLV ラップ形式（struct{version, bytes[97]}）とは違い、
  **生 97 バイト**。新リーダはこれを直接扱う。
- 重要: **root CA 鍵は永続化されている。** これが本 spec の成立根拠 — 我々が
  自前の NOC を発行するのに必要な発行鍵が手に入る。

## 方針（承認要 — 3 つの決定）

### 決定 1: root 直署名の 2 証明書チェーンで自前 NOC を発行する（ICA 迂回）

chip-tool 自身は root→ICA→NOC の 3 段だが、デバイスは commission 時に **root**
（RCAC）を信頼アンカーとして格納しているので、**root が直接署名した NOC
（root→NOC の 2 段、ICAC 無し）も CASE で受理される**。これを採る:

- 読む: `ExampleCARootCert`(RCAC TLV) + `ExampleOpCredsCAKey`(root 鍵 97B)。ICAC /
  ICA 鍵は**読まない**。
- 我々が P-256 operational 鍵ペアを新規生成。
- その公開鍵に対し、subject = {node id, fabric id}、issuer = RCAC の subject DN で
  NOC（Matter TLV 証明書）を組み、**root 鍵で DER TBS に ECDSA 署名**して完成。
- CASE の Sigma3 では NOC のみ送る（ICAC 無し）。Sigma3 TBS は我々の新 operational
  鍵で署名。

**根拠**: 段数が減り、ICA 鍵・ICAC の読み出しと連結検証を省ける最小経路。機能的
デメリット無し（デバイスは root にチェーンすれば受理）。
**代替（棄却）**: chip-tool と同じ root→ICA→NOC を再現。ICA 鍵読み出し + ICAC
同梱 + チェーン 3 段検証が要るが、相互運用上の利点は無い。

### 決定 2: NOC の node id は chip-tool の `LocalNodeId` と同一にする

デバイスの ACL は commission した controller の node id に admin を付与している。
自前 NOC を**同じ node id**で出せば、CASE 後の `onoff toggle` / `read` が admin
権限で通る。別 node id だと CASE は成立しても IM 操作が `ACCESS_DENIED (0x7E)`
になりうる。

- node id は KVS の `LocalNodeId`（未設定なら `kTestControllerNodeId`）から取る。
- 同一 (fabric, node) に対し鍵の違う NOC が複数あっても、Matter の ACL は
  node id / CAT ベースで鍵をピン留めしないため、デバイスは提示された NOC が
  信頼 root にチェーンし node id が ACL に載っていれば受理する（2 台目の admin
  controller が join するのと同じ理屈）。**この前提の実機確認が本 spec 最大の
  リスク項目**（受け入れで検証）。

### 決定 3: セキュリティ正・機能最小を M2 から継承

証明書生成でも検証と同じ厳密さ（正しい DER TBS を組んで正しく署名）。自己発行の
妥当性は、生成した NOC を**既存 `cert::verify_noc_chain` で自己検証**して担保
（生成器と検証器の相互チェック）。M2 で棄却した範囲（write/subscribe/resumption、
証明書検証の省略）は引き続き対象外。

## スコープ

`mat-controller` crate への追加のみ。mat / matd は無変更（adapter 差し替えは M4）。

| モジュール | 変更 |
|---|---|
| `cert` | **TLV 証明書エンコーダを追加**（`MatterCert` → TLV バイト列）。Task 4 のパースの逆。DN / extensions / validity を TLV で組む。既存の `tbs_der()`（DER TBS 生成）を署名対象に流用 |
| `crypto` | `sign_raw_ecdsa` / `verify_raw_ecdsa` を `case` から昇格し `pub`、正式なエラー型付き（NOC 署名と Sigma3 署名で共用） |
| `kvs` | CA 資格情報リーダを追加: `ExampleCARootCert` / `ExampleOpCredsCAKey`(生97B) / `LocalNodeId` を読む。既存の `read_fabric_credentials`（`f/1/o` 前提）は温存し、別関数として追加 |
| `fabric` | `FabricCredentials` を「自前発行鍵 + 自己発行 NOC」から組む経路を追加（`from_raw` は温存、`from_self_issued` 等を追加）。NOC 発行ロジック（新鍵生成 → NOC 組立 → root 署名 → 自己検証）をここに置く |
| `case` | 無変更（`FabricCredentials` を受け取るだけ。ICAC が `None` の経路は M2 で実装済み） |

**新依存なし**（p256 の鍵生成・署名は M2 で導入済み）。

## CASE オフライン自己検証ハーネス（最終レビュー必須要件）

M2 最終 whole-branch レビュー（Important #2）で必須と判定: CASE 暗号コア
（transcript 境界・S2K/S3K/SessionKeys 導出・TBS 配置・鍵分割）は現状**オフライン
テストがゼロ**で、担保する予定だったライブ E2E がこのブロックで止まっている。
本 spec で **テスト専用の最小 CASE レスポンダ**を実装し、フィクスチャ
（node01_01 証明書 + 秘密鍵 + ica/root チェーン）で自己ハンドシェイクを回す:

- Sigma1 受信 → responder 役で S2K 導出 → Sigma2/TBE2 生成 → Sigma3 検証 →
  導出鍵で secured IM read を 1 往復。
- これで transcript 境界・ECDH・鍵分割/配置・ワイヤ framing が実行カバレッジに入る。
- 残留リスク（両側で同一の定数を同じく間違えるとライブまで検出不能、例: info
  文字列）はテスト doc に明記。orientation / ordering / framing バグ（歴史的に
  出やすい層）は全て捕捉できる。

この対応で、自己発行経路が実機前にオフラインで CASE 全体を通せることを確認してから
ライブ受け入れに進める。

## 受け入れ基準（M2b、ローカル）

`task e2e:m2`（M2 で書いたハーネスを流用・修正）で以下が一括で通ること:

1. ローカル chip-tool で chip-all-clusters-app をコミッション（使い捨て alpha fabric）
2. 新 kvs リーダが KVS から RCAC / root 鍵 / LocalNodeId / IPK を取得
3. `fabric` が自前 operational 鍵を生成し、root 署名の NOC を自己発行、
   `verify_noc_chain` で自己検証 pass
4. CASE 確立（Sigma1/2/3 + StatusReport success）— **実機が我々の自己発行 NOC を
   受理することの初実証**
5. `onoff toggle` invoke が admin 権限で成功（決定 2 の ACL 継承の実証）
6. `on-off` read が変化後の値を返す

加えて **CASE 自己ハンドシェイクハーネスがオフラインで pass**（CI 内、デバイス不要）。
ライブテストは `#[ignore]`。既存の M1/M2 テストは全通過のまま。

## 非ゴール

- ICA 経由の 3 段チェーン発行（決定 1 で root 直署名を採用）。
- 独自 fabric の新規コミッション（PASE、mat 側 commissioner） — 後続マイルストーン。
- write / subscribe / timed invoke / チャンク読み / マルチ fabric。
- jarvis 本番 fabric への CASE（M3。ただし本 capability が前提として要る）。
- mat / matd の adapter 差し替え（M4）。

## 未決事項（実装計画で詰める）

- `Example*<idx>` キーの正確な `<idx>` サフィックス（実 ini ダンプで確定）。
- `LocalNodeId` 未設定時の `kTestControllerNodeId` 実値と、コミッション時に
  chip-tool が実際に使う node id との一致確認（決定 2 の ACL 継承の前提）。
- NOC の validity（not-before / not-after）と subject DN 以外の必須 extension
  （key-usage = digitalSignature、EKU = server+client auth、basic-constraints
  CA=false）の正確な値 — Matter spec §6.5 と SDK の NOC テンプレートに突き合わせる。
- 決定 2 のリスク（同一 node id・別鍵の NOC を実機が受理するか）が外れた場合の
  フォールバック（デバイス ACL に我々の node id を追加する経路、または chip-tool の
  node id を使わず ACL 追加を chip-tool 側で行う手順）。

## 最終 whole-branch レビューからの持ち越し（本 spec/実装計画で回収）

- **必須**: CASE 自己ハンドシェイクハーネス（上記）。
- cert: `verify_noc_chain` に validity チェックを追加（NOC 発行で validity を
  持つので自然な回収先）。rcac self-issued チェック。
- fabric: `NocMissingIds` の dead code 整理、`FabricError::source()`。
- cert `sign_raw_ecdsa`/`verify_raw_ecdsa` の crypto 昇格（決定のスコープに含む）。
- 受け入れ環境の既知の罠（`mat-discovery-timeout-misclassified`、`matd port9100`
  孤児）を実行時に留意。
