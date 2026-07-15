# Phase 5 M6a: on-network commissioning native 化（PASE・attestation・NOC 発行）設計

2026-07-13 起草。親 spec: `2026-07-10-phase5-backend-direction-design.md`（M6 =
「commissioning（PASE・BTP/BLE・attestation）→ chip-tool 完全廃止」）。前提:
M1〜M5 実装・実機 E2E 合格済み。ブランチは長期ブランチ `matter-controller`
（main マージ禁止）。

**M6 は M6a / M6b に分割する（本 spec の決定）。** M6a = on-network
commissioning（IP 上の PASE。multi-admin join と既にネットワーク上にいる
デバイスの commission）。M6b = BTP/BLE + Thread データセット書き込み
（NetworkCommissioning）で、工場出荷状態デバイスの commission を扱う。
BLE は WSL 開発機で使えず検証が jarvis 上に限られ、BlueZ 統合が工数の最大の
塊のため、撤退可能性を保って分割する。chip-tool の完全廃止は M6b +
「native 版 mat」（one-shot 直経路 native 化、既決定で別トラック）が揃った
後の話であり、M6a では本番経路に一切触れない。

## ゴール

**mat-controller に on-network commissioning の完全な native 実装を持たせ、
E2E ハーネスで実証する。** 具体的には:

1. SPAKE2+ (P-256) prover と PASE ハンドシェイクで secured session を確立する。
2. commissioning ステップマシン（ArmFailSafe → attestation → CSR →
   AddTrustedRootCertificate → AddNOC → CASE → CommissioningComplete）。
3. attestation 検証: DAC→PAI→PAA チェーン・nonce・署名は厳格（失敗 = 中止）、
   CD 署名と VID/PID 整合は warn 継続（ユーザー決定 2026-07-13）。
4. native open-window（PBKDF verifier 生成 + `AdministratorCommissioning.
   OpenCommissioningWindow` invoke + setup code 文字列生成）。
5. 本番 `mat commission` / matd は**無変更**（ライブラリ + E2E のみ。本番切替は
   native 版 mat 着手時に一括 — 既決定の段階投入方針と整合）。

## 決定

### 決定 1: PASE は既存 session/exchange 基盤の「もう一つの鍵導出」として統合

PASE と CASE の違いは鍵の作り方（SPAKE2+ vs Sigma）だけで、成立後の secured
session の土管は同一。PASE 成立後は既存 `SessionKeys` を作って
`SecureSession::new` に注入し、`im.rs` の encode/decode をそのまま使う。
`session.rs` / `im.rs` / `exchange.rs` は**変更なしが目標**。

却下した代替: 外部 crate（rs-matter 等）の commissioning 部分の依存追加
（親 spec が CHIP SDK 直リンクを退けたのと同じ所有性・二重層の理由）、
PASE 専用の独立チャネル実装（暗号化パスの二重実装になり M6b の BTP で
同じ統合をやり直す）。

### 決定 2: 新規モジュール構成

| モジュール | 責務 | 依存 |
|---|---|---|
| `spake2p.rs` | SPAKE2+ P-256 数学（w0/w1/L 導出、X/Y 交換、cA/cB 確認、Ke 出力）。prover（controller 側）と verifier 計算（open-window の w0/L 登録値） | p256 群演算、`crypto.rs` の HKDF/HMAC |
| `pase.rs` | PASE プロトコル層: PBKDFParamRequest/Response・Pake1/2/3 の TLV 組立/解釈、StatusReport 処理、`SessionKeys` 導出 | `spake2p.rs`、`exchange.rs`（MRP）、`tlv.rs` |
| `attestation.rs` | DAC/PAI の DER 解析、PAA ストアに対するチェーン検証（厳格）、attestation-elements の nonce/署名検証（厳格）、CD の CMS 解析 + CSA 鍵署名検証 + VID/PID 整合（warn） | `crypto.rs` の ECDSA verify、`asn1.rs` |
| `setup_code.rs` | manual pairing code（11/21 桁, Verhoeff 検査桁）と QR payload（`MT:` base38）の parse / 生成（純関数） | なし |
| `commissioning.rs` | ステップマシン本体。公開 API: `commission_on_network(...)` / `open_commissioning_window(...)` | 上記全部 + `im.rs`、`case.rs`、`cert.rs`、`dnssd.rs` |

既存モジュールへの拡張:

- `cert.rs`: RCAC 自己生成（新規 P-256 鍵ペア + self-signed 証明書、Matter TLV
  / DER 両形）。NOC 発行は既存 `issue_noc` を流用。
- `dnssd.rs`: `_matterc._udp` の one-shot browse（TXT の `D=` long
  discriminator でフィルタし IP+port を返す）。

### 決定 3: commission_on_network のデータフロー

```
入力: iface, passcode+discriminator（または setup code 文字列）,
      fabric 素材（RCAC 鍵・fabric_id・IPK）, PAA ストアパス, 新 node_id
 1. dnssd: _matterc._udp browse → discriminator 一致の IP:port
 2. pase: PBKDFParamReq→Resp(salt/iter) → SPAKE2+ Pake1/2/3
    → SessionKeys → SecureSession
 3. im over PASE: ArmFailSafe(120s) → SetRegulatoryConfig
 4. im: AttestationRequest(nonce) + CertificateChainRequest(DAC, PAI)
    → attestation.rs で検証（チェーン/nonce/署名 = 厳格、CD/VID-PID = warn）
 5. im: CSRRequest(nonce) → CSR 署名検証
    → cert.rs issue_noc(RCAC 鍵, CSR 公開鍵, fabric_id, node_id)
 6. im: AddTrustedRootCertificate(RCAC) → AddNOC(NOC, IPK,
    caseAdminSubject=自 node_id)
 7. dnssd: 新 fabric の operational advertise を待つ（リトライ付き）
    → case.rs で CASE 確立
 8. im over CASE: CommissioningComplete → 完了（failsafe 解除）
出力: CommissionedNode { node_id, fabric_id, ip, ... }
```

`open_commissioning_window` は逆方向の部品: ランダム passcode + salt 生成 →
`spake2p.rs` で verifier(w0/L) 計算 → 既存 CASE セッションで
`OpenCommissioningWindow(verifier, discriminator, iterations, salt, timeout)`
invoke → `setup_code.rs` で manual code / qr_payload 文字列を生成して返す。

### 決定 4: fabric 素材は呼び出し側の引数（永続化は M6a スコープ外）

使い捨て第二 fabric の RCAC 鍵・IPK は E2E ハーネスが生成して渡す引数。
mat-controller 側に永続化の置き場所は作らない。置き場所の決定は native 版
mat 着手時（chip-tool KVS からの独立を設計するとき）。chip-tool KVS への
**書き込みはしない**（読み取り専用の現方針を維持）。

### 決定 5: エラーは段階付き enum、本番 ErrorKind への対応表を先に固定

`CommissionError`: `Discovery(timeout)` / `Pase(InvalidPasscode | Timeout |
StatusReport)` / `Attestation(ChainVerify | Nonce | Signature)` / `Csr(..)` /
`AddNoc(NOCResponse status)` / `OperationalCase(..)` / `Timeout`。

将来の本番配線時の対応（README の既存 kind/exit code 体系に写像）:

| CommissionError | ErrorKind | exit |
|---|---|---|
| `Attestation(*)` | `device_rejected` | 4 |
| `Pase(InvalidPasscode)` | `device_rejected` | 4 |
| `Discovery` / `Timeout` | `timeout` | 3 |
| `OperationalCase` | `session_failed` | 6 |
| その他（`Csr`/`AddNoc` 等） | `commission_failed` | 1 |

PASE の StatusReport 生値と AddNOC の `NOCResponse.statusCode` は detail に
保持する（AI が復旧判断できる粒度、既存規約どおり）。

### 決定 6: 失敗時は failsafe の期限切れに任せる（明示 revert なし）

途中失敗時に明示的な failsafe 解除は送らない（chip-tool と同じ挙動）。
AddNOC 成功後・CommissioningComplete 前の失敗では、第二 fabric が failsafe
満了で自動 revert されることを実機 E2E の確認事項に含める。

### 決定 7: デバイス実装癖への耐性 — ステップごとに必須/任意を定義

ArmFailSafe と証明書系ステップ（attestation / CSR / AddRoot / AddNOC）は
必須（失敗 = 中止）。SetRegulatoryConfig は任意扱い — UNSUPPORTED_CLUSTER /
UNSUPPORTED_COMMAND は warn で先に進む（Nanoleaf 等の実装癖対策）。

## テスト

- **ユニット**: SPAKE2+ は RFC 9383（P256-SHA256-HKDF-HMAC）+ Matter spec
  付録のテストベクタ。manual code / QR payload は spec 記載の既知ペア
  （例: passcode 20202021 / discriminator 3840 → `MT:-24J0AFN00KA0648G00`）
  + Verhoeff / base38 の往復性。attestation は SDK テスト証明書（DAC/PAI/PAA）
  で正常系 + チェーン不一致 / nonce 不一致 / 署名破損の失敗系。
- **統合**（`task e2e:m6`、実プロトコル）: all-clusters-app を起動し、
  ① native commission → 新 fabric で CASE + onoff toggle、
  ② native open-window → 第二 admin としてもう一度 native commission
  （multi-admin のローカル完全リハーサル）、
  ③ 異常系 1 本（誤 passcode → `Pase` エラー）。
  all-clusters-app は test PAA 署名なので、テスト用 PAA 証明書は e2e
  セットアップ時に connectedhomeip リポジトリから取得する（コミットしない —
  リポジトリは public、証明書類は入れない方針を機械的に守る）。
- **実機**（受け入れ）: jarvis の Nanoleaf 1 台に対し、本番 fabric 側から
  native open-window → 使い捨て第二 fabric へ native commission（本物 DAC の
  厳格 attestation 通過）→ 新 fabric で CASE + onoff → RemoveFabric で撤収 →
  本番 fabric 無傷を確認（既存経路で onoff）。本番 PAA ストアは jarvis の
  `<store>/paa-trust-store`。

## 受け入れ基準

1. ユニットテスト全合格（SPAKE2+ / setup code / attestation のベクタ）。
2. `task e2e:m6` ローカル 3 項目合格。
3. 実機手順（上記）合格 — 本番 fabric・本番 matd に影響ゼロ。
4. `task check` 合格（fmt / clippy -D warnings / 全テスト）。

## 非ゴール

- BTP/BLE、Thread データセット書き込み、工場出荷状態デバイス（M6b）。
- 本番 `mat commission` / matd の native 切替、fabric 素材の永続化設計
  （native 版 mat トラック）。
- subscribe、CommissioningWindow の自動延長、複数デバイス同時 commission。
- constant-time な SPAKE2+（controller 側 prover のみで攻撃面が限定的なため、
  正しさ優先。テストベクタで担保）。

## リスク

1. **SPAKE2+ 自前実装の正しさ** — RFC 9383 / Matter spec のテストベクタで
   担保。rust-matc は参照のみ（コピーしない、BSD-2 でも方針として）。
2. **AddNOC 後の operational advertise 待ち** — デバイスにより数秒かかる。
   mDNS リトライ + 全体を failsafe 期限（120s、CommissioningComplete まで）
   内に収める。長引く場合は ArmFailSafe 再送で延長。
3. **DER/CMS 解析の攻撃面** — 入力はデバイス由来。既存 `asn1.rs` の方針
   （長さ検査・panic なし）を踏襲。
4. **all-clusters-app と実デバイスの挙動差** — ローカル②の multi-admin
   リハーサルで手順を固めてから実機に行く（M3〜M5 と同じ段取り）。
