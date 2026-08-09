# verify_noc_chain に CA 制約検証を追加（commission/証明書監査 Tier 1 ①）

日付: 2026-08-09 / 対象バージョン: 1.23.0

## 背景 / 問題

commission/証明書監査（2026-08-06）Tier 1 ①（high / security）:
`crates/mat-controller/src/cert.rs:478` の `verify_noc_chain` は署名連鎖 +
issuer/subject DN 一致 + fabric-id 一致しか検証せず、basicConstraints の
cA / KeyUsage の keyCertSign を一切見ない。`parse_extensions` は
is_ca / path_len / KeyUsage をパース済みなのに検証側が読んでいない。
同リポジトリの `attestation.rs:152-158` は DAC/PAI/PAA で is_ca を検査
しており、運用証明書チェーンだけが欠落している。

**実害**: fabric 内の悪意ノード A が自分の運用鍵で偽 NOC_X
（subject=node X、issuer=NOC_A の subject）を自作し、Sigma2 の TBE に
icac=NOC_A / noc=NOC_X を積むと全検証を通過する — fabric 内任意ノードへの
成りすまし。NOC_A は cA=false / keyUsage=digitalSignature なので、
RFC 5280 §6.1.4(k)/(n)（Matter spec 1.4 §6.6.4 が MUST 参照）の検査が
あれば ICAC 位置で即拒否される。

## 決定（ユーザー承認済み）

**Matter 運用証明書プロファイル準拠**（connectedhomeip SDK の
ValidateCert 相当）。最小案（RFC 5280 §6.1.4(k)/(n) のみ）ではなく
プロファイル全体を強制する。

| 証明書 | 強制する制約 |
|---|---|
| RCAC | cA=true 必須 / keyUsage に keyCertSign 必須 |
| ICAC | cA=true 必須 / keyUsage に keyCertSign 必須 / pathLen 存在時は 0 のみ |
| NOC | cA=false 必須 / keyUsage に digitalSignature 必須かつ keyCertSign・cRLSign 禁止 / EKU ⊇ {clientAuth, serverAuth} |
| 連鎖 | RCAC pathLen=0 かつ ICAC あり → 拒否（RFC 5280 §6.1.4(m)） |
| 欠落 | basicConstraints / keyUsage 拡張の欠落 → 拒否（fail-closed。Matter spec 上、運用証明書に両拡張は必須） |

**interop リスク**: ほぼゼロ。`verify_noc_chain` が検証する証明書は全て
mat 自身（`issue_noc` / `generate_rcac`）か chip-tool（採用 fabric）の
発行物で、どちらもプロファイル準拠で発行している。KVS ロード経路
（採用済み chip-tool fabric）が起動不能になるリスクも同理由で無し。

**スコープ外**:

- criticality 検査 — Matter TLV cert 形式に criticality フラグは存在しない
  （DER 変換時に spec が固定）。
- 有効期間 — `cert_time_valid` に分離済みの意図的設計（監査でも棄却済み論点）。
- RCAC/ICAC への EKU「存在禁止」検査、cRLSign の CA 側要否 — 攻撃面に
  寄与しない cosmetic な制約は追わない（YAGNI）。
- name constraints / policy / revocation — Matter TLV cert に存在しない。

## 設計

### 1. アクセサ追加（`MatterCert`）

```rust
/// basicConstraints 拡張 (is_ca, path_len)。拡張が無ければ None。
pub fn basic_constraints(&self) -> Option<(bool, Option<u8>)>;
/// KeyUsage 拡張のビット値。拡張が無ければ None。
pub fn key_usage(&self) -> Option<u16>;
```

EKU は `verify_noc_chain` 内で extensions を直接 find する（NOC 検査
1 箇所しか使わないためアクセサは足さない）。

### 2. 役割別チェック関数（cert.rs 内部 fn）

- `check_ca_cert(cert, role: &'static str)` — cA=true + keyCertSign を検査。
  RCAC / ICAC で共用し、エラーメッセージに role を埋める。
- `check_noc_leaf(noc)` — cA=false、digitalSignature 必須、
  keyCertSign/cRLSign 禁止、EKU に clientAuth(1.3.6.1.5.5.7.3.2=2) と
  serverAuth(1.3.6.1.5.5.7.3.1=1) の両方（Matter TLV の EKU 値は spec の
  key-purpose-id 列挙値）を検査。
- pathLen 検査（ICAC=0 のみ許容 / RCAC pathLen=0 + ICAC あり拒否）は
  `verify_noc_chain` 本体でチェーン形状を見ながら行う。

チェックは `verify_noc_chain` 内部に置き、3 呼び出し経路
（`case.rs:535` Sigma2 / `fabric.rs:189` KVS ロード / `fabric.rs:243`
self-issue 自己検証）全てに一様に効かせる。

### 3. エラー表現

既存 `CertError::Malformed(&'static str)` を流用し、メッセージで判別可能に
する（例: `"icac is not a CA"` / `"noc key-usage lacks digitalSignature"`）。
新 variant は追加しない — 呼び出し側は CASE 失敗として扱うだけで、
variant で分岐する消費者がいない。

### 4. 変えないもの

- 署名連鎖 / DN 一致 / fabric-id 一致の既存検査（順序含め不変）。
- `parse_extensions` / `write_extension` / DER 変換。
- `issue_noc` / `generate_rcac` の発行内容（既にプロファイル準拠）。
- `attestation.rs` の DAC/PAI/PAA 検査（別モジュール、監査 Tier 4 の管轄）。

## テスト

`cert.rs` unit テスト。違反証明書はテスト内で `MatterCert` を直接組んで
`crypto::sign_ecdsa_p256` で署名して作る（既存テストと同じ流儀）。

1. **攻撃シナリオそのもの**: 正規発行された NOC_A を ICAC 位置に置き、
   NOC_A の鍵で署名した偽 NOC_X を積んだチェーンが拒否されること
   （修正前はこのテストが通過してしまうことが脆弱性の再現）。
2. **個別違反の拒否**: cA=false の ICAC / keyCertSign 無し RCAC /
   keyCertSign 持ち NOC / digitalSignature 無し NOC / EKU 欠落 NOC /
   ICAC pathLen=1 / RCAC pathLen=0 + ICAC あり / basicConstraints 欠落 /
   keyUsage 欠落。
3. **正常系回帰**: `issue_noc` + `generate_rcac` 産の 2-cert チェーンと
   `test_support` の ICAC 付きフィクスチャチェーンが引き続き通ること。
   既存の `case_self_handshake.rs` / fabric テスト / `task check` が
   回帰ガード。
4. **実機 E2E（マージ前必須）**: jarvis で新バイナリ（`*.new`、本番未置換）
   により実デバイスへの read が通ること（Sigma2 のピア NOC チェーン検証が
   実発行物で通過する確認）。

## 影響範囲

`crates/mat-controller/src/cert.rs` のみ（アクセサ + 内部 fn +
`verify_noc_chain` 本体 + unit テスト）。実装差分は数十行 + テスト。
