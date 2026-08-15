# Tier 4/5: attestation/x509 検証強化 + テスト層ギャップ — 設計

2026-08-15。commission/証明書監査（2026-08-06）の残り = Tier 4（attestation/x509、
全 low、5 件）+ Tier 5（テスト層、4 件）を 1 ブランチで実装する。バージョンは
1.28.0。これで監査バックログは全消化。

## 背景

Tier 4 は attestation 経路（DAC/PAI/PAA の X.509 DER 検証）の検証漏れのまとめ。
NOC チェーン（Matter TLV 証明書、cert.rs）は Tier 1 で cA/keyUsage 検証済みだが、
X.509 側（x509.rs / attestation.rs）は KeyUsage を読み捨てている（x509.rs:249）。
CD は warn-only 設計（2026-07-13 ユーザー決定）— CD 系の修正はすべて warn 経路の
中の品質改善で、strict 化はしない。

Tier 5 はテスト層の構造的ギャップ: PASE 成功経路のオフラインテスト欠如、
device-free commissioning ハーネス（e2e-m6.sh）の誤った死蔵ヘッダ、
e2e-m8c3 の状態変更 op 無条件リトライ、docs.yml の checksum 未検証。

## Tier 4 — attestation/x509（crates/mat-controller）

### 変更 1: x509.rs — KeyUsage 拡張のパース

- `parse_extensions` ループに OID 2.5.29.15（KeyUsage、BIT STRING）を追加し、
  `X509Cert.key_usage: Option<u16>` に入れる（RFC 5280 named-bit、bit 0 =
  digitalSignature が MSB 側にある DER BIT STRING を u16 へ正規化。既存
  cert.rs の `key_usage_bits` と同じビット割当 = LSB が digitalSignature に
  そろえる）。
- test_support の `make_test_cert` / `make_test_cert_ext` に keyUsage 拡張の
  発行を追加（CA には keyCertSign|cRLSign、leaf には digitalSignature を既定で
  付与し、テストで上書きできる形にする）。
- 既存 SDK DER フィクスチャのテスト（`sdk_fixtures_expose_is_ca_and_validity`
  系）に key_usage の値アサートを足してパースを実物で固定する。

### 変更 2: attestation.rs — KeyUsage 検査（strict）

`verify_device_attestation` の cA 検査（basicConstraints）ブロックの直後に追加:

- **PAI / PAA**: `key_usage` が Some かつ keyCertSign ビットが立っていること。
  それ以外（拡張なし含む）は `Chain("pai/paa keyusage missing keycertsign")`。
  RFC 5280 §4.2.1.3 は CA 証明書に keyUsage を MUST とし、CSA 認証済み
  デバイスの PAI/PAA は必ず持つ（upstream CHIP の DA verifier も strict）。
- **DAC**: `key_usage` が Some の場合、digitalSignature が立っていて、かつ
  keyCertSign / cRLSign が立っていないこと。違反は
  `Chain("dac keyusage ...")`。拡張なしは許容（is_ca の DAC 側の寛容と同じ
  精神 — leaf の欠落でホームデバイスの commissioning を割らない）。

### 変更 3: attestation.rs — VID スコープ PAA / DAC PID（strict）

- PAA 選定後、`paa.vid` が Some（= VID スコープ PAA）なら `pai.vid` と一致
  必須。不一致は `Chain("vid-scoped paa/pai vid mismatch")`（spec §6.2.2.2:
  VID スコープ PAA の下の PAI は同一 VID でなければならない）。
- `dac.pid` が None なら `Chain("dac missing pid")`（spec §6.2.2.1: DAC の
  subject は VID と PID を必ず持つ。VID は既存の vid 一致検査が None を
  すでに弾いている）。

### 変更 4: attestation.rs — parse_elements のネスト ContainerEnd

`parse_elements` のループに深さカウンタを導入: StructStart/ArrayStart/ListStart
で +1、ContainerEnd は深さ 0 のときだけ終端、それ以外は -1。cd / nonce の
捕捉は深さ 0 のフィールドに限定する。これで vendor-reserved フィールドに
コンテナが来ても外側ループが打ち切られない（現状は最初のネスト
ContainerEnd で break → 正当なデバイスを Elements エラーで落とす）。

### 変更 5: attestation.rs — CD CMS の messageDigest 結合（warn 経路内）

`parse_signer_info` で signedAttrs があるとき、SET 内の Attribute から
messageDigest（OID 1.2.840.113549.1.9.4、値は OCTET STRING）を取り出し、
`SHA-256(eContent)` と比較する。不在・不一致は「署名検証失敗」と同じ扱い =
warn して署名検証をスキップ（CMS §5.4: signedAttrs 使用時は messageDigest が
eContent を結合する。これが無いと signedAttrs への正しい署名が eContent と
無関係でも「検証成功」扱いになる）。CD 全体は従来どおり warn-only —
strict 化はしない。

### テスト（Tier 4）

既存 attestation.rs テストのフィクスチャ流儀（make_test_cert チェーン）で:
PAI keyUsage 欠落/ keyCertSign なし → 拒否、DAC keyUsage=keyCertSign → 拒否、
DAC keyUsage なし → 受理、VID スコープ PAA 不一致 → 拒否、DAC PID なし →
拒否、elements にネストコンテナ → cd/nonce が正しく取れる、CMS
messageDigest 不一致 → warn 経路（検証失敗扱い）で全体は Ok。

## Tier 5 — テスト層

### 変更 6: PASE 成功経路のオフラインテスト

- test_support（feature `test-responder`）に PASE 応答器
  `pase_responder_task` を追加: PBKDFParamRequest → PBKDFParamResponse
  （正当な iterations/salt）、Pake1 → Pake2（本物の SPAKE2+ verifier 計算 =
  pB, cB）、Pake3 → cA 検証 → StatusReport(success)、続けて secured IM read に
  1 回答える（CASE の `responder_task` と同型）。
- SPAKE2+ verifier（device 役: `Y = y·P + w0·N`、`Z = y·(X − w0·M)`、
  `V = y·L`、`L = w1·P`）は spake2p.rs の既存プリミティブ
  （derive_w0_w1 / PakeShared / 定数 M,N）を再利用して test_support 側に実装
  する。必要なら spake2p.rs の内部を pub(crate) に広げる（本体ロジックは
  変更しない）。
- 新テスト `tests/pase_self_handshake.rs`: ループバック UDP で
  `pase::establish` を本物の応答器に当て、確立した secured session で IM read
  1 回まで通す（Ke 導出・i2r/r2i 分割・cA/cB 確認の初の実行可能カバレッジ）。

### 変更 7: e2e-m6.sh の死蔵ヘッダ是正

ヘッダの「chip-tool 撤去済みのため 0.22.0 以降では動かない（歴史的アーカイブ）」
は誤り（スクリプトは chip-tool 非依存で、現行 native commissioning の唯一の
device-free ハーネス）。ヘッダと line 10 のエラーメッセージを是正:

- 現行コードで動く device-free ハーネスであることを明記。
- 前提バイナリの入手方法を正確に書く: `task chip:extract:app` は撤去済み
  （c415bda）なので、`git show c415bda~1:Dockerfile` の all-clusters-builder
  ステージを使うか、upstream connectedhomeip の
  `examples/all-clusters-app/linux` をビルドして `MAT_E2E_APP` で渡す。

SDK の Docker ステージ復活はしない（chip-tool 退役コミットで意図的に消した
巨大ビルドを戻さない — 入手手順の文書化で「死蔵」を解く）。

### 変更 8: e2e-m8c3-real.sh の状態変更 op リトライ抑止

`_run_ssh_capture` に `NO_RETRY`（既定 0）ガードを追加: `NO_RETRY=1` のとき
rc=5/141/255 でもリトライしない。状態変更 op の呼び出し点（toggle / write /
RemoveFabric / commission / open-window / group provision 系）を
`NO_RETRY=1 run_xxx ...` に切り替える。読み取り系（read / discover / diag /
status）は従来どおりリトライ。二重実行（例: toggle 2 回 = no-op に見える、
commission 2 回 = fabric 重複）の恐れを塞ぐ。

### 変更 9: docs.yml の mdBook tarball checksum 検証

curl | tar の直結をやめ、ファイルへ落として `sha256sum -c` してから展開する。
期待値は実装時に該当リリース（v0.5.4 x86_64-unknown-linux-gnu）から取得して
ピン留め（MDBOOK_VERSION と並べて env に置く）。

## やらないこと

- CD 検証・有効期間の strict 化（2026-07-13 ユーザー決定の warn-only を維持）。
- CMS contentType attr 検証・parse_cd_vid_pid のネスト対応（監査指摘外。
  CD は warn-only なので実害なし）。
- SDK Docker ステージ（all-clusters-builder）の復活。
- 棄却済み 3 件（cB 非定数時間・NOC 有効期間・issue_noc 鍵対応）の再実装。

## 検証

- `task check`（fmt / clippy / test）全緑 + `bash -n` で両 e2e スクリプトの
  構文確認。
- jarvis 実機 E2E（マージ前必須の運用ルール）: `task dist:arm64` → `*.new` 転送 →
  e2e-m8c3-real.sh。STAGE=2 の cross-fabric commission が実 Nanoleaf の
  DAC/PAI/PAA チェーンに対する新 keyUsage / PID / VID 検査を実機実証する
  （CSA 認証チェーンなら通るはず — 落ちたら該当検査を warn へ降格して再判断）。
