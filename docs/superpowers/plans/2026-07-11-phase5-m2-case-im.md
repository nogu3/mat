# Phase 5 M2: CASE initiator + IM read/invoke 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat-controller` crate に chip-tool KVS リーダ・Matter 証明書検証・CASE initiator・SecureSession・最小 IM (read/invoke) を追加し、ローカル chip-all-clusters-app に対して CASE 確立 → onoff toggle → on-off read が通る（`task e2e:m2`）。

**Architecture:** M1 の TLV / message / crypto / exchange 層の上に、`kvs`（資格情報読取）→ `cert`+`asn1`（証明書検証）→ `fabric`（導出）→ `case`（ハンドシェイク）→ `session`（secured exchange + MRP）→ `im`（IM ペイロード）の順で積む。mat / matd は無変更。

**Tech Stack:** Rust, tokio (M1 継承), RustCrypto: `p256`(ECDH/ECDSA), `sha2`, `hkdf`, `hmac`, `base64ct`。暗号プリミティブの自作はしない。

**Spec:** `docs/superpowers/specs/2026-07-10-phase5-m2-case-im-design.md`（承認済み）

## Global Constraints

- 対象 SDK フォーマットは connectedhomeip **v1.4.2.0 固定**（Docker の chip-tool / all-clusters-app と同一）。
- 暗号プリミティブは RustCrypto 既製のみ。自作しない。
- `mat` / `matd` クレートは**無変更**（adapter 差し替えは M4）。プロトコルコードは `mat-controller` のみ。
- repo は public: 実 fabric の資格情報・実 IP・実 node id をコミットしない。テストフィクスチャは connectedhomeip の**公開テスト証明書**（Apache-2.0 のダミー、実デバイスと無関係）のみ可。
- CI (`task test`) はデバイス・実資格情報なしで全通過。ライブテストは `#[ignore]`。
- ブランチは `matter-controller`（main にマージしない）。
- 各タスクのコミット前に `task check`（fmt:check + clippy -D warnings + test）を通す。
- crate 内エラーは M1 と同形式の小さな enum（`Display + Error` 実装、panic なし。ただし TLV Writer のプログラマエラー assert は M1 踏襲）。

## Spec からの明確化（実装前に承知しておくこと）

1. **DER TBS 再構築は必要。** spec の cert 行に「X.509 変換はしない」とあるが、Matter TLV 証明書の署名は **DER 形式の TBSCertificate に対する ECDSA 署名**（Matter spec §6.5.10。TLV は X.509 の圧縮表現）。署名検証には TBS 部分の DER 再構築が数学的に不可避。「X.509 変換をしない」は「フル X.509 証明書の生成や X.509 ライブラリへの依存をしない」の意で運用する（rs-matter も同方式）。正当性は SDK 公開ベクタ（TLV/DER ペア）との **TBS バイト完全一致テスト**で担保する。
2. **IPK の epoch→operational 導出は不要。** chip-tool KVS の keyset (`f/1/k/0`) には `DeriveGroupOperationalCredentials` 済みの **operational key** が格納されている（SDK `GroupDataProviderImpl::SetKeySet` が導出してから保存する）。`fabric` モジュールの導出は compressed fabric id と destination id のみ。
3. **Sigma1 の SessionParams (tag 5) は送らない**（optional。MRP パラメータは M1 デフォルトを使用）。Sigma2 側の tag 5 は読み飛ばす。
4. **セッション id 割当**: initiator local session id は乱数の非ゼロ u16。
5. **InteractionModelRevision = 12**（SDK v1.4.2.0 `SpecificationDefinedRevisions.h`）、トップレベル IM メッセージの context tag 255 に必須。

## プロトコル事実（SDK v1.4.2.0 ソースで検証済み。再調査不要）

### chip-tool KVS（ini）
- ファイル: `<storage-directory>/chip_tool_config.ini`（`--storage-directory` 未指定時は `$TMPDIR` または `/tmp`）。`src/controller/ExamplePersistentStorage.cpp`。
- 形式: ini。セクション `[Default]`。値は **base64**。キーは `\x20` 以下・`=`・`\`・`0x7F` 以上のみ `\xNN` エスケープされる → `f/1/r` 系はエスケープ無しの生キー。
- fabric index 1 のキー（`DefaultStorageKeyAllocator`）:
  - `f/1/r` = RCAC、`f/1/i` = ICAC（無い場合あり）、`f/1/n` = NOC — いずれも Matter TLV 証明書バイト列
  - `f/1/o` = 操作鍵。TLV: `struct { ctx0: uint16 version(=1), ctx1: bytes[97] }`、bytes = 非圧縮公開鍵(65) || 秘密鍵(32)
  - `f/1/k/0` = IPK keyset。TLV: `struct { ctx1: policy(u16), ctx2: keys_count(u16), ctx3: array [ struct { ctx4: start_time(u64), ctx5: hash(u16), ctx6: bytes[16](operational key) } ×3 ], ... }`（後続タグは無視可）。先頭エントリの ctx6 が operational IPK（keys_count>=1 のとき有効）。

### 導出（Matter spec §4.3.2 / §4.14.2）
- compressed fabric id = HKDF-SHA256(ikm = root 公開鍵の先頭 0x04 を除いた 64B, salt = fabric id **BE** 8B, info = `"CompressedFabric"`, L=8)
  - specベクタ: root pub `044a9f42b1ca4840d37292bbc7f6a7e11e22200c976fc900dbc98a7a383a641cb8254a2e56d4e295a847943b4e3897c4a773e930277b4d9fbede8a052686bfacfa`, fabric id `0x2906C908D115D362` → `87e1b004e235a130`
- destination id = HMAC-SHA256(key = operational IPK(16B), msg = initiatorRandom(32) || rootPubKey(65) || fabricId **LE**(8) || nodeId **LE**(8))
  - specベクタ: ipk `9bc61cd9c62a2df6d64dfcaa9dc472d4`, random `7e171231568dfa17206b3accf8faec2f4d21b580113196f47c7c4deb810a73dc`, 上記 root pub / fabric id, node id `0xCD5544AA7B13EF14` → `dc35dd5fc9134cc5544538c9c3fc4297c1ec3370c839136a80e10796451d4c53`

### CASE（Secure Channel protocol id 0x0000）
- opcode: Sigma1=0x30, Sigma2=0x31, Sigma3=0x32, Sigma2Resume=0x33, StatusReport=0x40
- Sigma1 TLV (anonymous struct, context tags): 1=initiatorRandom(32B bytes), 2=initiatorSessionId(u16 uint), 3=destinationId(32B bytes), 4=initiatorEphPubKey(65B bytes), [5=sessionParams 省略, 6/7=resume 省略]
- Sigma2: 1=responderRandom(32B), 2=responderSessionId(u16), 3=responderEphPubKey(65B), 4=encrypted2(bytes), [5=sessionParams 読み飛ばし]
- TBEData2 (encrypted2 を復号した TLV struct): 1=responderNOC, 2=responderICAC(任意), 3=signature(64B raw r||s), 4=resumptionID
- Sigma2 TBS (署名対象、TLV anonymous struct): 1=responderNOC, 2=responderICAC(任意), 3=responderEphPubKey, 4=initiatorEphPubKey
- Sigma3: 1=encrypted3。TBEData3: 1=initiatorNOC, 2=initiatorICAC(任意), 3=signature。Sigma3 TBS: 1=initiatorNOC, 2=initiatorICAC(任意), 3=initiatorEphPubKey, 4=responderEphPubKey
- 鍵導出（すべて HKDF-SHA256, ikm = ECDH 共有秘密の x 座標 32B）:
  - S2K: salt = IPK || responderRandom || responderEphPubKey || SHA256(Sigma1), info=`"Sigma2"`, L=16
  - S3K: salt = IPK || SHA256(Sigma1||Sigma2), info=`"Sigma3"`, L=16
  - SessionKeys: salt = IPK || SHA256(Sigma1||Sigma2||Sigma3), info=`"SessionKeys"`, L=48 → I2R(16) || R2I(16) || AttestationChallenge(16)
  - transcript は**プロトコルヘッダを除く TLV ペイロードバイト列**を逐次 SHA-256（S2K の salt は Sigma2 を hash に足す**前**の digest）
- TBE 暗号: AES-128-CCM, MIC 16B, AAD なし, nonce 13B ASCII = `"NCASE_Sigma2N"` / `"NCASE_Sigma3N"`（M1 `crypto::encrypt_payload`/`decrypt_payload` を aad=`b""` で再利用可）
- StatusReport ペイロード: generalCode(u16 LE) || protocolId(u32 LE) || protocolCode(u16 LE)。成功 = (0, 0x0000, 0x0000)。主な失敗 protocolCode: 0x0001=NO_SHARED_TRUST_ROOTS, 0x0002=INVALID_PARAMETER, 0x0003=CLOSE_SESSION, 0x0004=BUSY
- ECDSA 署名は raw r||s 64B（TLV 内）。ECDSA/P-256 は SHA-256 でメッセージをハッシュ（p256 の `Signer`/`Verifier` デフォルト）

### Secure session メッセージ（spec §4.7）
- 送信 header: session_id = **peer の** session id, security_flags=0, source/destination node id なし。nonce の node id = **送信者**（送信時 = 自 node id、受信時 = peer node id）→ M1 `seal_message`/`open_message` の `session_source_node_id` 引数がそのまま使える。
- メッセージカウンタはセッション単位（送信 = 乱数初期化 TxCounter、受信 = セッション毎 RxWindow）。exchange 毎ではない。
- DSIZ 予約値 0b11 はデコードエラー化（M2 spec の M1 申し送り）。

### IM（protocol id 0x0001）
- opcode: StatusResponse=0x01, ReadRequest=0x02, ReportData=0x05, InvokeRequest=0x08, InvokeResponse=0x09
- ReadRequestMessage: struct { 0: AttributeRequests array [ AttributePathIB **list** { 2: Endpoint, 3: Cluster, 4: Attribute } ], 3: FabricFiltered bool(必須, false), 255: IMRevision uint(12) }
- ReportDataMessage: struct { [0: SubscriptionId], 1: AttributeReportIBs array [ struct { 0: AttributeStatus struct { 0: Path, 1: StatusIB struct{0:Status,1:ClusterStatus} } | 1: AttributeData struct { 0: DataVersion, 1: Path(list), 2: Data(任意型) } } ], [3: MoreChunked], [4: SuppressResponse bool], 255 }
- InvokeRequestMessage: struct { 0: SuppressResponse bool(false), 1: TimedRequest bool(false), 2: InvokeRequests array [ CommandDataIB struct { 0: CommandPathIB **list** { 0: Endpoint, 1: Cluster, 2: Command }, [1: CommandFields struct] } ], 255: 12 }（フィールド無しコマンドは 1 を省略）
- InvokeResponseMessage: struct { 0: SuppressResponse, 1: InvokeResponses array [ InvokeResponseIB struct { 0: Command(CommandDataIB) | 1: Status(CommandStatusIB struct { 0: Path, 1: StatusIB }) } ], [2: MoreChunked], 255 }
- StatusResponseMessage: struct { 0: Status uint, 255 }。IM Status 0 = SUCCESS
- onoff: cluster 0x0006, attribute on-off 0x0000, command Off=0x00 / On=0x01 / Toggle=0x02（toggle はフィールド無し・timed 不要）
- 単一属性 read の ReportData は通常 SuppressResponse=true。false/欠落なら initiator が StatusResponse(SUCCESS) を返す。

### Matter TLV 証明書（`CHIPCert.h`）
- anonymous struct, context tags: 1=serial(bytes, BER INTEGER 中身), 2=sig-algo(uint, 1=ecdsa-with-sha256 のみ対応), 3=issuer(**list**), 4=not-before(u32, 2000-01-01 epoch 秒), 5=not-after(u32, 0=無期限), 6=subject(list), 7=pubkey-algo(uint, 1=EC), 8=curve(uint, 1=prime256v1), 9=ec-pub-key(65B bytes), 10=extensions(**list**), 11=signature(64B raw r||s)
- DN list 要素: context tag = 属性種別、値は uint(Matter id 系) または Utf8。base tag: 1=CommonName(2.5.4.3), 17=matter-node-id, 19=matter-icac-id, 20=matter-rcac-id, 21=matter-fabric-id, 22=matter-noc-cat。tag に **0x80 が立っていたら PrintableString** 表現（DER 変換時 0x13、無印は UTF8String 0x0C）。Matter id の DER 値は **大文字16進 UTF8String**（64bit → 16文字、noc-cat → 8文字）。
- extensions list 要素 (context tags): 1=basic-constraints struct{1: is-ca bool, 2: path-len uint}, 2=key-usage(uint, RFC5280 bit番号), 3=extended-key-usage array[uint], 4=subject-key-id(bytes 20), 5=authority-key-id(bytes 20), 6=future-extension
- DER TBS 構造・OID・エンコード規則は Task 4 に全記載。
- 署名検証: SHA-256(DER TBS) への ECDSA。発行者公開鍵で検証。

## File Structure

| ファイル | 責務 |
|---|---|
| `crates/mat-controller/src/fabric.rs` (新規) | compressed fabric id / destination id 導出、`FabricCredentials`（Task 1, 5） |
| `crates/mat-controller/src/kvs.rs` (新規) | chip-tool ini KVS リーダ → `RawFabricCredentials`（Task 2） |
| `crates/mat-controller/src/asn1.rs` (新規) | 最小 DER ライタ（TBS 再構築用プリミティブ）（Task 3） |
| `crates/mat-controller/src/cert.rs` (新規) | Matter TLV 証明書パース・DER TBS 再構築・署名/チェーン検証（Task 4） |
| `crates/mat-controller/src/message.rs` (変更) | DSIZ=0b11 拒否（Task 6） |
| `crates/mat-controller/src/exchange.rs` (変更) | `last_sent_counter()` 追加（Task 6） |
| `crates/mat-controller/src/session.rs` (新規) | `SecureSession` + secured MRP exchange、IM メソッド（Task 6, 8） |
| `crates/mat-controller/src/case.rs` (新規) | CASE initiator 状態機械（Task 7） |
| `crates/mat-controller/src/im.rs` (新規) | IM ペイロード encode/decode + onoff 定数（Task 8） |
| `crates/mat-controller/tests/fixtures/` (新規) | SDK 公開テスト証明書バイナリ + README（Task 4） |
| `crates/mat-controller/tests/live_case_im.rs` (新規) | ライブ E2E（`#[ignore]`）（Task 9） |
| `scripts/e2e-m2.sh` (新規) + `Taskfile.yml` (変更) | 使い捨て fabric 一括 E2E ハーネス（Task 9） |

---

### Task 1: 依存追加 + `fabric` 導出関数（spec ベクタ）

**Files:**
- Modify: `crates/mat-controller/Cargo.toml`
- Create: `crates/mat-controller/src/fabric.rs`
- Modify: `crates/mat-controller/src/lib.rs`
- Modify: `docs/superpowers/specs/2026-07-10-phase5-backend-direction-design.md`（M3 再定義の追記）

**Interfaces:**
- Produces: `fabric::compressed_fabric_id(root_public_key: &[u8; 65], fabric_id: u64) -> [u8; 8]`、`fabric::case_destination_id(ipk_operational: &[u8; 16], initiator_random: &[u8; 32], root_public_key: &[u8; 65], fabric_id: u64, node_id: u64) -> [u8; 32]`

- [ ] **Step 1: 親 spec に M3 再定義を追記**

`docs/superpowers/specs/2026-07-10-phase5-backend-direction-design.md` の M3 を記述している箇所（`M3` で検索）の直後に、以下の趣旨の追記を行う（既存文面は消さず、追記であることを明示）:

```markdown
> **2026-07-11 追記（M2 spec 決定の反映）:** 最小 KVS リーダ（chip-tool Linux KVS
> v1.4.2.0 固定、fabric index 1 の RCAC/ICAC/NOC/操作鍵/IPK）は M2 に前倒しした。
> これに伴い M3 は「KVS リーダの堅牢化（バージョン互換方針含む）+ jarvis 実機
> 相乗りで on/off・色変更」に再定義する。
> 詳細: docs/superpowers/specs/2026-07-10-phase5-m2-case-im-design.md
```

- [ ] **Step 2: Cargo.toml に暗号依存を追加**

`crates/mat-controller/Cargo.toml` の `[dependencies]` に追加:

```toml
sha2 = "0.10"
hkdf = "0.12"
hmac = "0.12"
```

- [ ] **Step 3: 失敗するテストを書く**

`crates/mat-controller/src/fabric.rs` を新規作成し、まずテストのみ書く（本体は `todo!()` でなく未定義のままだとコンパイルが通らないので、テストと同時に空実装を書き、アサートで落とす方式ではなく **TDD の red はコンパイルエラーで確認**する）:

```rust
//! Fabric credentials and CASE-related derivations (spec §4.3.2, §4.14.2).

#[cfg(test)]
mod tests {
    use super::*;

    // Matter spec §4.3.2.2 / §4.14.2 掲載のテストベクタ（SDK TestChipCryptoPAL.cpp と同一）
    const ROOT_PUB: [u8; 65] = [
        0x04, 0x4a, 0x9f, 0x42, 0xb1, 0xca, 0x48, 0x40, 0xd3, 0x72, 0x92, 0xbb, 0xc7, 0xf6, 0xa7,
        0xe1, 0x1e, 0x22, 0x20, 0x0c, 0x97, 0x6f, 0xc9, 0x00, 0xdb, 0xc9, 0x8a, 0x7a, 0x38, 0x3a,
        0x64, 0x1c, 0xb8, 0x25, 0x4a, 0x2e, 0x56, 0xd4, 0xe2, 0x95, 0xa8, 0x47, 0x94, 0x3b, 0x4e,
        0x38, 0x97, 0xc4, 0xa7, 0x73, 0xe9, 0x30, 0x27, 0x7b, 0x4d, 0x9f, 0xbe, 0xde, 0x8a, 0x05,
        0x26, 0x86, 0xbf, 0xac, 0xfa,
    ];
    const FABRIC_ID: u64 = 0x2906_C908_D115_D362;

    #[test]
    fn derives_spec_compressed_fabric_id() {
        assert_eq!(
            compressed_fabric_id(&ROOT_PUB, FABRIC_ID),
            [0x87, 0xe1, 0xb0, 0x04, 0xe2, 0x35, 0xa1, 0x30]
        );
    }

    #[test]
    fn derives_spec_destination_id() {
        let ipk = [
            0x9b, 0xc6, 0x1c, 0xd9, 0xc6, 0x2a, 0x2d, 0xf6, 0xd6, 0x4d, 0xfc, 0xaa, 0x9d, 0xc4,
            0x72, 0xd4,
        ];
        let random = [
            0x7e, 0x17, 0x12, 0x31, 0x56, 0x8d, 0xfa, 0x17, 0x20, 0x6b, 0x3a, 0xcc, 0xf8, 0xfa,
            0xec, 0x2f, 0x4d, 0x21, 0xb5, 0x80, 0x11, 0x31, 0x96, 0xf4, 0x7c, 0x7c, 0x4d, 0xeb,
            0x81, 0x0a, 0x73, 0xdc,
        ];
        let expected = [
            0xdc, 0x35, 0xdd, 0x5f, 0xc9, 0x13, 0x4c, 0xc5, 0x54, 0x45, 0x38, 0xc9, 0xc3, 0xfc,
            0x42, 0x97, 0xc1, 0xec, 0x33, 0x70, 0xc8, 0x39, 0x13, 0x6a, 0x80, 0xe1, 0x07, 0x96,
            0x45, 0x1d, 0x4c, 0x53,
        ];
        assert_eq!(
            case_destination_id(&ipk, &random, &ROOT_PUB, FABRIC_ID, 0xCD55_44AA_7B13_EF14),
            expected
        );
    }
}
```

`crates/mat-controller/src/lib.rs` に `pub mod fabric;` を追加（アルファベット順: `counter` の後、`message` の前に `fabric`, `kvs` 等を随時追加していく）。

- [ ] **Step 4: テストが落ちる（コンパイルエラー）ことを確認**

Run: `cargo test -p mat-controller fabric`
Expected: FAIL（`compressed_fabric_id` 未定義のコンパイルエラー）

- [ ] **Step 5: 実装**

`fabric.rs` のテストの上に:

```rust
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Compressed fabric id (spec §4.3.2.2): HKDF over the root public key
/// (uncompressed-point prefix dropped) with the fabric id as big-endian salt.
pub fn compressed_fabric_id(root_public_key: &[u8; 65], fabric_id: u64) -> [u8; 8] {
    let hk = Hkdf::<Sha256>::new(Some(&fabric_id.to_be_bytes()), &root_public_key[1..]);
    let mut out = [0u8; 8];
    hk.expand(b"CompressedFabric", &mut out)
        .expect("8 bytes is a valid hkdf-sha256 output length");
    out
}

/// CASE destination identifier (spec §4.14.2.1.2). Fabric id / node id are
/// little-endian here (unlike the big-endian salt above).
pub fn case_destination_id(
    ipk_operational: &[u8; 16],
    initiator_random: &[u8; 32],
    root_public_key: &[u8; 65],
    fabric_id: u64,
    node_id: u64,
) -> [u8; 32] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(ipk_operational).expect("hmac accepts any key length");
    mac.update(initiator_random);
    mac.update(root_public_key);
    mac.update(&fabric_id.to_le_bytes());
    mac.update(&node_id.to_le_bytes());
    mac.finalize().into_bytes().into()
}
```

- [ ] **Step 6: テストが通ることを確認**

Run: `cargo test -p mat-controller fabric`
Expected: PASS (2 tests)

- [ ] **Step 7: `task check` → コミット**

```bash
task check
git add crates/mat-controller/Cargo.toml Cargo.lock crates/mat-controller/src/fabric.rs crates/mat-controller/src/lib.rs docs/superpowers/specs/2026-07-10-phase5-backend-direction-design.md
git commit -m "feat(mat-controller): fabric derivations (compressed fabric id / destination id) + M3 respec note"
```

---

### Task 2: `kvs` — chip-tool ini KVS リーダ

**Files:**
- Create: `crates/mat-controller/src/kvs.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod kvs;`）
- Modify: `crates/mat-controller/Cargo.toml`（`base64ct`）

**Interfaces:**
- Consumes: `tlv::{Reader, Value, Tag}`（M1）
- Produces:
  ```rust
  pub struct RawFabricCredentials {
      pub rcac: Vec<u8>,
      pub icac: Option<Vec<u8>>,
      pub noc: Vec<u8>,
      pub op_public_key: [u8; 65],
      pub op_private_key: [u8; 32],
      pub ipk_operational: [u8; 16],
  }
  pub enum KvsError { Io(std::io::Error), SectionMissing, KeyMissing(String), BadBase64(String), BadOpKey(&'static str), BadKeyset(&'static str) }
  pub fn read_fabric_credentials(path: &std::path::Path, fabric_index: u8) -> Result<RawFabricCredentials, KvsError>
  ```

- [ ] **Step 1: 依存追加**

`crates/mat-controller/Cargo.toml`:

```toml
base64ct = { version = "1", features = ["alloc"] }
```

- [ ] **Step 2: 失敗するテストを書く**

`kvs.rs` のテストは ini を一時ファイルに書いて読む。フィクスチャは合成（実資格情報は使わない）。opkey / keyset blob は M1 `tlv::Writer` で組み立てる:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Tag, Writer};
    use base64ct::{Base64, Encoding};

    fn opkey_blob(pubkey: &[u8; 65], privkey: &[u8; 32]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 1); // version
        let mut kp = Vec::with_capacity(97);
        kp.extend_from_slice(pubkey);
        kp.extend_from_slice(privkey);
        w.put_bytes(Tag::Context(1), &kp);
        w.end_container();
        w.finish()
    }

    fn keyset_blob(key: &[u8; 16]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0); // policy
        w.put_uint(Tag::Context(2), 1); // keys_count
        w.start_array(Tag::Context(3));
        for i in 0..3u8 {
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(4), 0); // start_time
            w.put_uint(Tag::Context(5), 0x1234); // hash
            w.put_bytes(Tag::Context(6), if i == 0 { key } else { &[0u8; 16] });
            w.end_container();
        }
        w.end_container();
        w.put_uint(Tag::Context(7), 0xFFFF); // next keyset id (リンクリスト、無視される)
        w.end_container();
        w.finish()
    }

    fn write_ini(entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        let mut body = String::from("[Default]\n");
        for (k, v) in entries {
            body.push_str(&format!("{} = {}\n", k, Base64::encode_string(v)));
        }
        let path = std::env::temp_dir().join(format!(
            "mat-kvs-test-{}-{}.ini",
            std::process::id(),
            entries.len()
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    const PUB: [u8; 65] = [0xAA; 65];
    const PRIV: [u8; 32] = [0xBB; 32];
    const IPK: [u8; 16] = [0xCC; 16];

    #[test]
    fn reads_all_five_items() {
        let op = opkey_blob(&PUB, &PRIV);
        let ks = keyset_blob(&IPK);
        let path = write_ini(&[
            ("f/1/r", b"rcac-bytes"),
            ("f/1/i", b"icac-bytes"),
            ("f/1/n", b"noc-bytes"),
            ("f/1/o", &op),
            ("f/1/k/0", &ks),
        ]);
        let c = read_fabric_credentials(&path, 1).unwrap();
        assert_eq!(c.rcac, b"rcac-bytes");
        assert_eq!(c.icac.as_deref(), Some(b"icac-bytes".as_slice()));
        assert_eq!(c.noc, b"noc-bytes");
        assert_eq!(c.op_public_key, PUB);
        assert_eq!(c.op_private_key, PRIV);
        assert_eq!(c.ipk_operational, IPK);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn missing_icac_is_none_but_missing_noc_is_error() {
        let op = opkey_blob(&PUB, &PRIV);
        let ks = keyset_blob(&IPK);
        let path = write_ini(&[
            ("f/1/r", b"rcac-bytes"),
            ("f/1/n", b"noc-bytes"),
            ("f/1/o", &op),
            ("f/1/k/0", &ks),
        ]);
        let c = read_fabric_credentials(&path, 1).unwrap();
        assert_eq!(c.icac, None);
        std::fs::remove_file(&path).ok();

        let path = write_ini(&[("f/1/r", b"rcac-bytes")]);
        let err = read_fabric_credentials(&path, 1).unwrap_err();
        assert!(matches!(err, KvsError::KeyMissing(k) if k == "f/1/n"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn rejects_bad_opkey_version_and_bad_base64() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 2); // 未知バージョン
        w.put_bytes(Tag::Context(1), &[0u8; 97]);
        w.end_container();
        let bad_op = w.finish();
        let ks = keyset_blob(&IPK);
        let path = write_ini(&[
            ("f/1/r", b"r"),
            ("f/1/n", b"n"),
            ("f/1/o", &bad_op),
            ("f/1/k/0", &ks),
        ]);
        assert!(matches!(
            read_fabric_credentials(&path, 1).unwrap_err(),
            KvsError::BadOpKey(_)
        ));
        std::fs::remove_file(&path).ok();

        let path = std::env::temp_dir().join(format!("mat-kvs-badb64-{}.ini", std::process::id()));
        std::fs::write(&path, "[Default]\nf/1/r = !!notbase64!!\n").unwrap();
        assert!(matches!(
            read_fabric_credentials(&path, 1).unwrap_err(),
            KvsError::BadBase64(_)
        ));
        std::fs::remove_file(path).ok();
    }
}
```

- [ ] **Step 3: 落ちることを確認** — `cargo test -p mat-controller kvs` → コンパイルエラー

- [ ] **Step 4: 実装**

```rust
//! Minimal reader for chip-tool's Linux ini KVS (connectedhomeip v1.4.2.0).
//!
//! Reads the five fabric credentials CASE needs. Format facts (verified
//! against SDK v1.4.2.0): `[Default]` section, base64 values, keys
//! `f/<index>/{r,i,n,o}` and `f/<index>/k/0`; the keyset stores the already
//! derived *operational* group key, not the epoch key.

use std::path::Path;

use base64ct::{Base64, Encoding};

use crate::tlv::{Reader, Tag, Value};

// ... (RawFabricCredentials / KvsError の定義: Interfaces のとおり。
//      Display は「どのキー・何が悪いか」を含める: 例 `kvs key "f/1/o": bad op key: <理由>`)

pub fn read_fabric_credentials(
    path: &Path,
    fabric_index: u8,
) -> Result<RawFabricCredentials, KvsError> {
    let text = std::fs::read_to_string(path).map_err(KvsError::Io)?;
    let section = default_section(&text).ok_or(KvsError::SectionMissing)?;
    let get = |key: String| -> Result<Option<Vec<u8>>, KvsError> {
        match lookup(section, &key) {
            None => Ok(None),
            Some(v) if v.is_empty() => Ok(None),
            Some(v) => Base64::decode_vec(v)
                .map(Some)
                .map_err(|_| KvsError::BadBase64(key)),
        }
    };
    let must = |key: String| -> Result<Vec<u8>, KvsError> {
        get(key.clone())?.ok_or(KvsError::KeyMissing(key))
    };

    let rcac = must(format!("f/{fabric_index}/r"))?;
    let icac = get(format!("f/{fabric_index}/i"))?;
    let noc = must(format!("f/{fabric_index}/n"))?;
    let (op_public_key, op_private_key) = parse_opkey(&must(format!("f/{fabric_index}/o"))?)?;
    let ipk_operational = parse_keyset(&must(format!("f/{fabric_index}/k/0"))?)?;

    Ok(RawFabricCredentials { rcac, icac, noc, op_public_key, op_private_key, ipk_operational })
}
```

補助関数:
- `default_section(&str) -> Option<&str>`: 行走査。`[Default]` 行の次から、次の `[` 開始行まで（大文字小文字区別あり）。
- `lookup(section: &str, key: &str) -> Option<&str>`: 各行を最初の `=` で分割し、両側 trim して比較。
- `parse_opkey(blob) -> Result<([u8;65],[u8;32]), KvsError>`: `tlv::Reader` で走査。StructStart → `Context(0)` の `Uint` が 1 でなければ `BadOpKey("unsupported version")` → `Context(1)` の `Bytes` が 97B でなければ `BadOpKey("keypair must be 97 bytes")` → 分割。TLV エラー・型違いは `BadOpKey("malformed tlv")`。
- `parse_keyset(blob) -> Result<[u8;16], KvsError>`: `Context(2)`(keys_count) が 1 以上であること、`Context(3)` の array 先頭 struct 内 `Context(6)` の 16B bytes を取得。深さを追跡し、未知タグは読み飛ばす。失敗は `BadKeyset(理由)`。

- [ ] **Step 5: 通ることを確認** — `cargo test -p mat-controller kvs` → PASS (3 tests)

- [ ] **Step 6: `task check` → コミット**

```bash
git add crates/mat-controller/Cargo.toml Cargo.lock crates/mat-controller/src/kvs.rs crates/mat-controller/src/lib.rs
git commit -m "feat(mat-controller): chip-tool ini KVS reader for fabric credentials"
```

---

### Task 3: `asn1` — 最小 DER ライタ

**Files:**
- Create: `crates/mat-controller/src/asn1.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod asn1;`）

**Interfaces:**
- Produces（全部 `Vec<u8>` を返す純関数。失敗しない）:
  ```rust
  pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8>;            // 定長 DER（長さ128以上は long form）
  pub fn seq(children: &[&[u8]]) -> Vec<u8>;                 // 0x30
  pub fn set_of(children: &[&[u8]]) -> Vec<u8>;              // 0x31
  pub fn integer(content: &[u8]) -> Vec<u8>;                 // 0x02、content はそのまま（呼び手が最小表現を保証）
  pub fn boolean(v: bool) -> Vec<u8>;                        // 0x01 01 FF/00
  pub fn bit_string(unused_bits: u8, bytes: &[u8]) -> Vec<u8>; // 0x03
  pub fn octet_string(bytes: &[u8]) -> Vec<u8>;              // 0x04
  pub fn utf8_string(s: &str) -> Vec<u8>;                    // 0x0C
  pub fn printable_string(s: &str) -> Vec<u8>;               // 0x13
  pub fn utc_time(s: &str) -> Vec<u8>;                       // 0x17
  pub fn generalized_time(s: &str) -> Vec<u8>;               // 0x18
  pub fn context_constructed(n: u8, content: &[u8]) -> Vec<u8>;   // 0xA0|n
  pub fn context_primitive(n: u8, content: &[u8]) -> Vec<u8>;     // 0x80|n
  ```

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_short_and_long_lengths() {
        assert_eq!(tlv(0x04, &[0xAB]), vec![0x04, 0x01, 0xAB]);
        assert_eq!(tlv(0x04, &[]), vec![0x04, 0x00]);
        let long = vec![0x00; 200]; // 128..256 → 0x81 プレフィクス
        let enc = tlv(0x04, &long);
        assert_eq!(&enc[..3], &[0x04, 0x81, 200]);
        assert_eq!(enc.len(), 3 + 200);
        let longer = vec![0x00; 300]; // 256.. → 0x82 + u16 BE
        let enc = tlv(0x04, &longer);
        assert_eq!(&enc[..4], &[0x04, 0x82, 0x01, 0x2C]);
    }

    #[test]
    fn encodes_primitives() {
        assert_eq!(integer(&[0x02]), vec![0x02, 0x01, 0x02]);
        assert_eq!(boolean(true), vec![0x01, 0x01, 0xFF]);
        assert_eq!(boolean(false), vec![0x01, 0x01, 0x00]);
        assert_eq!(bit_string(7, &[0x80]), vec![0x03, 0x02, 0x07, 0x80]);
        assert_eq!(octet_string(&[1, 2]), vec![0x04, 0x02, 0x01, 0x02]);
        assert_eq!(utf8_string("AB"), vec![0x0C, 0x02, 0x41, 0x42]);
        assert_eq!(printable_string("A"), vec![0x13, 0x01, 0x41]);
        assert_eq!(
            utc_time("260101000000Z"),
            [vec![0x17, 0x0D], b"260101000000Z".to_vec()].concat()
        );
    }

    #[test]
    fn encodes_containers() {
        assert_eq!(
            seq(&[&integer(&[0x01]), &boolean(true)]),
            vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x01, 0x01, 0xFF]
        );
        assert_eq!(set_of(&[&integer(&[0x01])]), vec![0x31, 0x03, 0x02, 0x01, 0x01]);
        assert_eq!(
            context_constructed(0, &integer(&[0x02])),
            vec![0xA0, 0x03, 0x02, 0x01, 0x02]
        );
        assert_eq!(context_primitive(0, &[0xAA]), vec![0x80, 0x01, 0xAA]);
    }
}
```

- [ ] **Step 2: 落ちることを確認** — `cargo test -p mat-controller asn1`

- [ ] **Step 3: 実装**

```rust
//! Minimal DER writer — just enough to rebuild the TBSCertificate of a
//! Matter operational certificate for signature verification (cert.rs).
//! Not a general ASN.1 library; no parsing.

pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 4);
    out.push(tag);
    let len = content.len();
    if len < 128 {
        out.push(len as u8);
    } else if len < 256 {
        out.push(0x81);
        out.push(len as u8);
    } else {
        // 証明書 TBS は 64KiB を超えない
        assert!(len <= usize::from(u16::MAX), "der content too large");
        out.push(0x82);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    }
    out.extend_from_slice(content);
    out
}

pub fn seq(children: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &children.concat())
}
// set_of / integer / boolean / bit_string / octet_string / utf8_string /
// printable_string / utc_time / generalized_time / context_* も同様に tlv() 呼び出しの薄いラッパ。
// bit_string は content = [unused_bits] ++ bytes。
// context_constructed は tag = 0xA0 | n、context_primitive は 0x80 | n。
```

- [ ] **Step 4: 通ることを確認** → **Step 5: `task check` → コミット**

```bash
git add crates/mat-controller/src/asn1.rs crates/mat-controller/src/lib.rs
git commit -m "feat(mat-controller): minimal DER writer for cert TBS reconstruction"
```

---

### Task 4: `cert` — Matter TLV 証明書パース・DER TBS・署名/チェーン検証

**Files:**
- Create: `crates/mat-controller/tests/fixtures/`（`*.bin` + `README.md`）
- Create: `crates/mat-controller/src/cert.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod cert;`）
- Modify: `crates/mat-controller/Cargo.toml`（`p256`）

**Interfaces:**
- Consumes: `asn1::*`、`tlv::{Reader, Tag, Value}`
- Produces:
  ```rust
  pub struct MatterCert {
      pub serial: Vec<u8>,
      pub issuer: Vec<DnAttr>,
      pub not_before: u32,
      pub not_after: u32,
      pub subject: Vec<DnAttr>,
      pub pub_key: [u8; 65],
      pub extensions: Vec<CertExtension>, // TLV 出現順を保持（DER 再構築に必要）
      pub signature: [u8; 64],
  }
  #[derive(PartialEq)] pub struct DnAttr { pub tlv_tag: u8, pub value: DnValue }
  #[derive(PartialEq)] pub enum DnValue { MatterId(u64), Text(String) }
  pub enum CertExtension {
      BasicConstraints { is_ca: bool, path_len: Option<u8> },
      KeyUsage(u16),
      ExtendedKeyUsage(Vec<u64>),
      SubjectKeyId(Vec<u8>),
      AuthorityKeyId(Vec<u8>),
  }
  pub enum CertError { Tlv(crate::tlv::TlvError), Malformed(&'static str), UnsupportedAlgorithm, UnsupportedDnAttr(u8), BadSignature, BadPublicKey }
  impl MatterCert {
      pub fn parse(tlv_bytes: &[u8]) -> Result<MatterCert, CertError>;
      pub fn tbs_der(&self) -> Result<Vec<u8>, CertError>;
      pub fn verify_signed_by(&self, issuer_public_key: &[u8; 65]) -> Result<(), CertError>;
      pub fn subject_matter_id(&self, tlv_tag: u8) -> Option<u64>; // 17=node,19=icac,20=rcac,21=fabric
      pub fn node_id(&self) -> Option<u64>;   // subject tag 17
      pub fn fabric_id(&self) -> Option<u64>; // subject tag 21
  }
  pub fn verify_noc_chain(noc: &MatterCert, icac: Option<&MatterCert>, rcac: &MatterCert) -> Result<(), CertError>;
  ```

- [ ] **Step 1: `p256` 依存を追加**

```toml
p256 = { version = "0.13", features = ["ecdh", "ecdsa"] }
```

- [ ] **Step 2: SDK 公開テスト証明書フィクスチャを抽出**

connectedhomeip v1.4.2.0 の公開テスト証明書（Apache-2.0 のダミー。実デバイス・実 fabric と無関係で、public repo にコミット可）を Docker イメージ `mat-chip-builder`（M1 でビルド済み。無ければ `docker build --target chip-builder -t mat-chip-builder .`）から抽出する。scratchpad に以下を `extract_cert_fixtures.py` として保存:

```python
import re, sys, pathlib

src = pathlib.Path(sys.argv[1]).read_text()
outdir = pathlib.Path(sys.argv[2]); outdir.mkdir(parents=True, exist_ok=True)
wanted = {
    "sTestCert_Root01_Chip": "root01_chip.bin",
    "sTestCert_Root01_DER": "root01_der.bin",
    "sTestCert_Root01_PublicKey": "root01_pubkey.bin",
    "sTestCert_ICA01_Chip": "ica01_chip.bin",
    "sTestCert_ICA01_DER": "ica01_der.bin",
    "sTestCert_ICA01_PublicKey": "ica01_pubkey.bin",
    "sTestCert_Node01_01_Chip": "node01_01_chip.bin",
    "sTestCert_Node01_01_DER": "node01_01_der.bin",
    "sTestCert_Node01_01_PublicKey": "node01_01_pubkey.bin",
    "sTestCert_Node01_01_PrivateKey": "node01_01_privkey.bin",
}
for sym, fname in wanted.items():
    m = re.search(re.escape(sym) + r"\(\(const uint8_t\[\]\)\{(.*?)\}\);", src, re.S)
    if not m:
        sys.exit(f"symbol {sym} not found")
    data = bytes(int(t, 16) for t in re.findall(r"0x[0-9a-fA-F]{2}", m.group(1)))
    (outdir / fname).write_bytes(data)
    print(f"{fname}: {len(data)} bytes")
```

実行:

```bash
docker run --rm mat-chip-builder cat /work/connectedhomeip/src/credentials/tests/CHIPCert_test_vectors.cpp > <scratchpad>/vectors.cpp
python3 <scratchpad>/extract_cert_fixtures.py <scratchpad>/vectors.cpp crates/mat-controller/tests/fixtures
```

Expected: 10 ファイル。`*_chip.bin` は 0x15 で始まる（TLV struct）、`*_der.bin` は 0x30 で始まる（DER SEQUENCE）、`*_pubkey.bin` は 65B、`*_privkey.bin` は 32B。

`crates/mat-controller/tests/fixtures/README.md` を作成:

```markdown
# Test certificate fixtures

connectedhomeip v1.4.2.0 `src/credentials/tests/CHIPCert_test_vectors.cpp`
(Apache-2.0) から抽出した公開テスト証明書。実デバイス・実 fabric とは無関係の
ダミー証明書（チェーン: Root01 → ICA01 → Node01_01）。

- `*_chip.bin`: Matter TLV 形式 / `*_der.bin`: 同一証明書の X.509 DER 形式
- `*_pubkey.bin`: P-256 公開鍵 (65B uncompressed) / `*_privkey.bin`: 秘密鍵 (32B)

再抽出手順は `docs/superpowers/plans/2026-07-11-phase5-m2-case-im.md` Task 4 参照。
```

- [ ] **Step 3: 失敗するテストを書く**

`cert.rs` 末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_CHIP: &[u8] = include_bytes!("../tests/fixtures/root01_chip.bin");
    const ROOT_DER: &[u8] = include_bytes!("../tests/fixtures/root01_der.bin");
    const ROOT_PUB: &[u8] = include_bytes!("../tests/fixtures/root01_pubkey.bin");
    const ICA_CHIP: &[u8] = include_bytes!("../tests/fixtures/ica01_chip.bin");
    const ICA_DER: &[u8] = include_bytes!("../tests/fixtures/ica01_der.bin");
    const ICA_PUB: &[u8] = include_bytes!("../tests/fixtures/ica01_pubkey.bin");
    const NODE_CHIP: &[u8] = include_bytes!("../tests/fixtures/node01_01_chip.bin");
    const NODE_DER: &[u8] = include_bytes!("../tests/fixtures/node01_01_der.bin");

    /// DER 証明書 = SEQUENCE { TBSCertificate, sigAlg, sigValue }。
    /// 先頭要素 (TBSCertificate) のバイト列を切り出す。
    fn tbs_of(der: &[u8]) -> &[u8] {
        assert_eq!(der[0], 0x30);
        let (hdr, _) = der_len(&der[1..]);
        let inner = &der[1 + hdr..];
        assert_eq!(inner[0], 0x30);
        let (h2, l2) = der_len(&inner[1..]);
        &inner[..1 + h2 + l2]
    }

    fn der_len(buf: &[u8]) -> (usize, usize) {
        if buf[0] < 0x80 {
            (1, buf[0] as usize)
        } else {
            let n = (buf[0] & 0x7F) as usize;
            let mut len = 0usize;
            for b in &buf[1..1 + n] {
                len = len << 8 | *b as usize;
            }
            (1 + n, len)
        }
    }

    #[test]
    fn parses_node_cert_ids() {
        let node = MatterCert::parse(NODE_CHIP).unwrap();
        assert!(node.node_id().is_some());
        assert!(node.fabric_id().is_some());
        let root = MatterCert::parse(ROOT_CHIP).unwrap();
        assert_eq!(root.pub_key.as_slice(), ROOT_PUB);
        assert!(root.subject_matter_id(20).is_some()); // rcac id
    }

    #[test]
    fn rebuilt_tbs_matches_der_vectors_byte_for_byte() {
        for (chip, der) in [(ROOT_CHIP, ROOT_DER), (ICA_CHIP, ICA_DER), (NODE_CHIP, NODE_DER)] {
            let cert = MatterCert::parse(chip).unwrap();
            assert_eq!(cert.tbs_der().unwrap().as_slice(), tbs_of(der));
        }
    }

    #[test]
    fn verifies_signatures_and_chain() {
        let root = MatterCert::parse(ROOT_CHIP).unwrap();
        let ica = MatterCert::parse(ICA_CHIP).unwrap();
        let node = MatterCert::parse(NODE_CHIP).unwrap();
        root.verify_signed_by(&root.pub_key.clone()).unwrap(); // self-signed
        ica.verify_signed_by(&root.pub_key.clone()).unwrap();
        node.verify_signed_by(&ICA_PUB.try_into().unwrap()).unwrap();
        verify_noc_chain(&node, Some(&ica), &root).unwrap();
        // ICAC 抜きだと NOC の署名が root で検証できず失敗する
        assert!(verify_noc_chain(&node, None, &root).is_err());
    }

    #[test]
    fn rejects_tampered_cert() {
        let mut sig_flipped = MatterCert::parse(NODE_CHIP).unwrap();
        sig_flipped.signature[0] ^= 0x01;
        assert!(matches!(
            sig_flipped.verify_signed_by(&ICA_PUB.try_into().unwrap()),
            Err(CertError::BadSignature)
        ));
        let mut subj_changed = MatterCert::parse(NODE_CHIP).unwrap();
        if let Some(DnAttr { value: DnValue::MatterId(id), .. }) =
            subj_changed.subject.iter_mut().find(|a| a.tlv_tag == 17)
        {
            *id ^= 1;
        }
        assert!(matches!(
            subj_changed.verify_signed_by(&ICA_PUB.try_into().unwrap()),
            Err(CertError::BadSignature)
        ));
    }

    #[test]
    fn rejects_malformed_tlv() {
        assert!(MatterCert::parse(&[0x15, 0x18]).is_err()); // 空 struct
        assert!(MatterCert::parse(b"junk").is_err());
    }
}
```

- [ ] **Step 4: 落ちることを確認** — `cargo test -p mat-controller cert`

- [ ] **Step 5: パース実装**

`MatterCert::parse`: `tlv::Reader` で anonymous StructStart → 各 context tag を match:
- 1: serial `Bytes` → Vec
- 2: sig-algo `Uint` — 1 以外は `UnsupportedAlgorithm`
- 3 / 6: DN。`ListStart` から `ContainerEnd` まで: `Tag::Context(t)` × `Value::Uint(v)` → `DnAttr { tlv_tag: t, value: MatterId(v) }`、`Value::Utf8(s)` → `Text(s.to_string())`。その他の値型は `Malformed("dn value")`。
- 4 / 5: not-before / not-after `Uint`（u32 に収まらなければ `Malformed`）
- 7: pubkey-algo `Uint`=1、8: curve `Uint`=1（以外は `UnsupportedAlgorithm`）
- 9: `Bytes` 65B → pub_key
- 10: extensions。`ListStart` 内、`Context(1)`=StructStart{1: bool is-ca, 2: uint path-len}、`Context(2)`=Uint→KeyUsage(u16)、`Context(3)`=ArrayStart[Uint]→ExtendedKeyUsage、`Context(4)`/`Context(5)`=Bytes→SubjectKeyId/AuthorityKeyId。`Context(6)`(future-extension) は `Malformed("future-extension unsupported")`（運用証明書には出ない）。**出現順のまま Vec に push**。
- 11: `Bytes` 64B → signature

必須フィールド（serial, sig-algo, issuer, validity, subject, pubkey, signature）の欠落は `Malformed(名前)`。

- [ ] **Step 6: DER TBS 実装**

事前計算済み OID 定数（DER バイト列、`06` タグ含む完全形）:

```rust
const OID_ECDSA_WITH_SHA256: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02]; // 1.2.840.10045.4.3.2
const OID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];           // 1.2.840.10045.2.1
const OID_PRIME256V1: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];         // 1.2.840.10045.3.1.7
const OID_COMMON_NAME: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];                                      // 2.5.4.3
// Matter arc 1.3.6.1.4.1.37244.1.x → 2B 06 01 04 01 82 A2 7C 01 xx
const OID_MATTER_NODE_ID: &[u8] = &[0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x01];
const OID_MATTER_FIRMWARE_SIGNING_ID: &[u8] = &[0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x02];
const OID_MATTER_ICAC_ID: &[u8] = &[0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x03];
const OID_MATTER_RCAC_ID: &[u8] = &[0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x04];
const OID_MATTER_FABRIC_ID: &[u8] = &[0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x05];
const OID_MATTER_NOC_CAT: &[u8] = &[0x06, 0x0A, 0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x06];
const OID_EXT_BASIC_CONSTRAINTS: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x13]; // 2.5.29.19
const OID_EXT_KEY_USAGE: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x0F];        // 2.5.29.15
const OID_EXT_EXTENDED_KEY_USAGE: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x25]; // 2.5.29.37
const OID_EXT_SUBJECT_KEY_ID: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x0E];   // 2.5.29.14
const OID_EXT_AUTHORITY_KEY_ID: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x23]; // 2.5.29.35
// extended key usage の値 (TLV uint → OID): 1=serverAuth..6=OCSPSigning
// 1.3.6.1.5.5.7.3.x → 2B 06 01 05 05 07 03 xx (x = 1,2,3,4)、5→...03 08, 6→...03 09
```

TBS 組み立て（`asn1::` 使用）:

```text
seq(
  context_constructed(0, integer(&[2])),          # version v3
  integer(serial),                                 # TLV の bytes をそのまま（BER INTEGER 中身）
  seq(OID_ECDSA_WITH_SHA256),
  name(issuer),
  seq(time(not_before), time(not_after)),
  name(subject),
  seq( seq(OID_EC_PUBLIC_KEY, OID_PRIME256V1), bit_string(0, pub_key) ),
  context_constructed(3, seq(extensions…)),
)
```

- `name(attrs)`: `seq(各 attr → set_of(seq(oid, value)))`。base tag = `tlv_tag & 0x7F`。
  - base 17/19/20/21 → 対応 Matter OID + `utf8_string(format!("{:016X}", id))`
  - base 22 → `OID_MATTER_NOC_CAT` + `utf8_string(format!("{:08X}", id))`
  - base 1 (`Text`) → `OID_COMMON_NAME` + `if tlv_tag & 0x80 != 0 { printable_string } else { utf8_string }`
  - base 18 → `OID_MATTER_FIRMWARE_SIGNING_ID`（同 16 hex）
  - それ以外 → `CertError::UnsupportedDnAttr(tag)`（Matter 運用証明書には出ない）
- `time(epoch)`: epoch == 0 → `generalized_time("99991231235959Z")`（X.509 の no-well-defined-expiration。chip `ChipEpochToASN1Time` と同じ）。それ以外は 2000-01-01T00:00:00Z + epoch 秒 → civil 変換し、year が 1950..=2049 → `utc_time("YYMMDDHHMMSSZ")`、それ以外 → `generalized_time("YYYYMMDDHHMMSSZ")`。civil 変換は days→(y,m,d) の標準アルゴリズム（Howard Hinnant の civil_from_days）:

```rust
/// 1970-01-01 からの日数を (year, month, day) へ（proleptic Gregorian）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
// Matter epoch → unix: secs + 946_684_800。days = unix.div_euclid(86400)、
// 残り秒から h/m/s。
```

- extensions（TLV 出現順のまま）: `Extension ::= SEQUENCE { OID, [critical BOOLEAN], extnValue OCTET STRING }`
  - BasicConstraints（critical）: `seq(OID, boolean(true), octet_string(seq( is_ca なら boolean(true) [DEFAULT FALSE のため false は省略], path_len があれば integer(&[len]) )))`
  - KeyUsage（critical）: `seq(OID, boolean(true), octet_string(bit_string(unused, bytes)))`。RFC5280 named bits: TLV uint の bit i（LSB=digitalSignature=bit0）→ DER bit列では `bytes[i/8] |= 0x80 >> (i%8)`。末尾のゼロビットは落とし、`unused = 8 - (最高使用bit % 8 + 1)`（9bit 目 decipherOnly まで対応、bytes は 1–2 オクテット）。
  - ExtendedKeyUsage（critical）: `seq(OID, boolean(true), octet_string(seq(各値の OID)))`
  - SubjectKeyId（非 critical）: `seq(OID, octet_string(octet_string(keyid)))`
  - AuthorityKeyId（非 critical）: `seq(OID, octet_string(seq(context_primitive(0, keyid))))`

- [ ] **Step 7: 署名・チェーン検証実装**

```rust
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};

impl MatterCert {
    pub fn verify_signed_by(&self, issuer_public_key: &[u8; 65]) -> Result<(), CertError> {
        let key = VerifyingKey::from_sec1_bytes(issuer_public_key)
            .map_err(|_| CertError::BadPublicKey)?;
        let sig = Signature::from_slice(&self.signature).map_err(|_| CertError::BadSignature)?;
        key.verify(&self.tbs_der()?, &sig).map_err(|_| CertError::BadSignature)
    }
}

/// NOC チェーン検証: 署名の連鎖 + DN リンク + fabric 整合。
pub fn verify_noc_chain(
    noc: &MatterCert,
    icac: Option<&MatterCert>,
    rcac: &MatterCert,
) -> Result<(), CertError> {
    rcac.verify_signed_by(&rcac.pub_key)?; // self-signed root
    let signer = match icac {
        Some(ica) => {
            ica.verify_signed_by(&rcac.pub_key)?;
            if ica.issuer != rcac.subject {
                return Err(CertError::Malformed("icac issuer != rcac subject"));
            }
            ica
        }
        None => rcac,
    };
    noc.verify_signed_by(&signer.pub_key)?;
    if noc.issuer != signer.subject {
        return Err(CertError::Malformed("noc issuer != signer subject"));
    }
    if noc.node_id().is_none() || noc.fabric_id().is_none() {
        return Err(CertError::Malformed("noc missing node/fabric id"));
    }
    if let Some(ica) = icac {
        if let (Some(a), Some(b)) = (ica.fabric_id(), noc.fabric_id()) {
            if a != b {
                return Err(CertError::Malformed("fabric id mismatch in chain"));
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 8: 通ることを確認** — `cargo test -p mat-controller cert` → PASS (5 tests)

TBS バイト一致テストが落ちた場合のデバッグ: 期待値（`tbs_of(der)`）と実際の hex を先頭から突き合わせ、最初に食い違うオフセットの DER 要素を特定する（フィールド順・時刻表現・critical フラグ・named-bit の unused 数が定番の間違い）。

- [ ] **Step 9: `task check` → コミット**

```bash
git add crates/mat-controller/Cargo.toml Cargo.lock crates/mat-controller/src/cert.rs crates/mat-controller/src/lib.rs crates/mat-controller/tests/fixtures/
git commit -m "feat(mat-controller): Matter TLV certificate parse, DER TBS rebuild, ECDSA chain verify"
```

---

### Task 5: `fabric::FabricCredentials`（kvs + cert の結合）

**Files:**
- Modify: `crates/mat-controller/src/fabric.rs`

**Interfaces:**
- Consumes: `kvs::RawFabricCredentials`、`cert::{MatterCert, verify_noc_chain, CertError}`
- Produces:
  ```rust
  pub struct FabricCredentials {
      pub rcac_tlv: Vec<u8>,
      pub icac_tlv: Option<Vec<u8>>,
      pub noc_tlv: Vec<u8>,
      pub op_public_key: [u8; 65],
      pub op_private_key: [u8; 32],
      pub ipk_operational: [u8; 16],
      pub node_id: u64,
      pub fabric_id: u64,
      pub root_public_key: [u8; 65],
  }
  pub enum FabricError { Cert(CertError), NocMissingIds, OpKeyMismatch }
  impl FabricCredentials {
      pub fn from_raw(raw: crate::kvs::RawFabricCredentials) -> Result<Self, FabricError>;
  }
  ```

- [ ] **Step 1: 失敗するテストを書く**（`fabric.rs` の tests mod に追加）

```rust
    #[test]
    fn builds_credentials_from_fixture_chain() {
        let noc = include_bytes!("../tests/fixtures/node01_01_chip.bin").to_vec();
        let icac = include_bytes!("../tests/fixtures/ica01_chip.bin").to_vec();
        let rcac = include_bytes!("../tests/fixtures/root01_chip.bin").to_vec();
        let node_pub: [u8; 65] = include_bytes!("../tests/fixtures/node01_01_pubkey.bin")
            .as_slice()
            .try_into()
            .unwrap();
        let node_priv: [u8; 32] = include_bytes!("../tests/fixtures/node01_01_privkey.bin")
            .as_slice()
            .try_into()
            .unwrap();
        let raw = crate::kvs::RawFabricCredentials {
            rcac,
            icac: Some(icac),
            noc,
            op_public_key: node_pub,
            op_private_key: node_priv,
            ipk_operational: [0xCC; 16],
        };
        let creds = FabricCredentials::from_raw(raw).unwrap();
        assert_ne!(creds.node_id, 0);
        assert_ne!(creds.fabric_id, 0);
        assert_eq!(
            creds.root_public_key.as_slice(),
            include_bytes!("../tests/fixtures/root01_pubkey.bin")
        );
    }

    #[test]
    fn rejects_opkey_not_matching_noc() {
        let raw = crate::kvs::RawFabricCredentials {
            rcac: include_bytes!("../tests/fixtures/root01_chip.bin").to_vec(),
            icac: Some(include_bytes!("../tests/fixtures/ica01_chip.bin").to_vec()),
            noc: include_bytes!("../tests/fixtures/node01_01_chip.bin").to_vec(),
            op_public_key: [0xAA; 65], // NOC の公開鍵と不一致
            op_private_key: [0xBB; 32],
            ipk_operational: [0xCC; 16],
        };
        assert!(matches!(
            FabricCredentials::from_raw(raw),
            Err(FabricError::OpKeyMismatch)
        ));
    }
```

- [ ] **Step 2: 落ちることを確認** → **Step 3: 実装**

`from_raw`: NOC / ICAC / RCAC を `MatterCert::parse` → `verify_noc_chain`（自 fabric の資格情報が壊れていたら CASE 前に検出）→ `node_id` / `fabric_id` を NOC subject から（無ければ `NocMissingIds`）→ `root_public_key` = RCAC の `pub_key` → **`op_public_key` が NOC の `pub_key` と一致すること**（不一致は `OpKeyMismatch` — KVS の鍵と証明書の食い違い検出）。TLV バイト列（rcac/icac/noc）は Sigma3 用にそのまま保持。

- [ ] **Step 4: 通ることを確認** — `cargo test -p mat-controller fabric` → PASS (4 tests)

- [ ] **Step 5: `task check` → コミット**

```bash
git add crates/mat-controller/src/fabric.rs
git commit -m "feat(mat-controller): FabricCredentials assembly with own-chain verification"
```

---

### Task 6: DSIZ 拒否 + `last_sent_counter` + `session::SecureSession`

**Files:**
- Modify: `crates/mat-controller/src/message.rs`
- Modify: `crates/mat-controller/src/exchange.rs`
- Create: `crates/mat-controller/src/session.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod session;`）

**Interfaces:**
- Consumes: `crypto::{seal_message, open_message, OpenError, CryptoError}`、`counter::{TxCounter, RxWindow}`、`message::*`、`transport::UdpTransport`、`exchange::{MrpConfig, IncomingMessage}`
- Produces:
  ```rust
  // message.rs 追加
  MessageError::ReservedDestination            // DSIZ == 0b11
  // exchange.rs 追加
  impl UnsecuredExchange<'_> { pub fn last_sent_counter(&self) -> Option<u32>; }
  // session.rs
  pub struct SessionKeys { pub i2r: [u8; 16], pub r2i: [u8; 16], pub attestation_challenge: [u8; 16] }
  pub enum SessionError { Timeout, Io(std::io::Error), Message(MessageError), Crypto(CryptoError) }
  pub struct SecureSession<'t> { /* private */ }
  impl<'t> SecureSession<'t> {
      pub fn new(transport: &'t UdpTransport, peer: SocketAddr, local_session_id: u16,
                 peer_session_id: u16, keys: SessionKeys, local_node_id: u64, peer_node_id: u64) -> Self;
      pub fn peer_node_id(&self) -> u64;
      pub fn new_exchange_id() -> u16; // 乱数
      pub async fn send_reliable(&mut self, exchange_id: u16, protocol_id: u16, opcode: u8,
                                 payload: &[u8], cfg: &MrpConfig)
          -> Result<Option<IncomingMessage>, SessionError>;
      pub async fn recv(&mut self, exchange_id: u16, timeout: Duration)
          -> Result<IncomingMessage, SessionError>;
  }
  ```

- [ ] **Step 1: message.rs — 失敗するテスト**

`rejects_bad_message_header` テストに追記:

```rust
        // DSIZ 予約値 0b11（spec 4.4.1.2 reserved）は拒否
        assert_eq!(
            MessageHeader::decode(&[0x03, 0, 0, 0, 0, 0, 0, 0]),
            Err(MessageError::ReservedDestination)
        );
```

- [ ] **Step 2: 落ちることを確認** — `cargo test -p mat-controller message`

- [ ] **Step 3: 実装**

`MessageError` に `ReservedDestination` を追加（Display: `"reserved destination size in message header"`）。`decode` の destination match を:

```rust
        let destination = match flags & 0x03 {
            1 => Destination::Node(c.u64()?),
            2 => Destination::Group(c.u16()?),
            3 => return Err(MessageError::ReservedDestination),
            _ => Destination::None,
        };
```

- [ ] **Step 4: exchange.rs — `last_sent_counter`**

`UnsecuredExchange` にフィールド `last_sent_counter: Option<u32>` を追加（`new` で `None`）。`send_reliable` の先頭 `build` 後に `self.last_sent_counter = Some(our_counter);`。getter を追加。既存テスト `send_reliable_completes_on_standalone_ack` に `assert_eq!(ex.last_sent_counter().is_some(), true);` 相当の確認を足す（ack された counter と一致することは responder 側 assert で担保済み）。

- [ ] **Step 5: session.rs — 失敗するテストを書く**

テスト補助: 「デバイス側」をエミュレートするヘルパ。R2I 鍵・デバイス node id で `seal_message` した応答を作る:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::seal_message;
    use crate::message::{
        Destination, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
        OPCODE_STATUS_REPORT, PROTOCOL_ID_SECURE_CHANNEL,
    };
    use crate::transport::{UdpTransport, MAX_DATAGRAM};
    use std::time::Duration;

    const I2R: [u8; 16] = [0x11; 16];
    const R2I: [u8; 16] = [0x22; 16];
    const OUR_NODE: u64 = 0xAAAA;
    const DEV_NODE: u64 = 0xBBBB;
    const LOCAL_SID: u16 = 0x1234;
    const PEER_SID: u16 = 0x5678;

    fn keys() -> SessionKeys {
        SessionKeys { i2r: I2R, r2i: R2I, attestation_challenge: [0; 16] }
    }

    fn fast_cfg() -> MrpConfig {
        MrpConfig { initial_interval: Duration::from_millis(50), max_retries: 2, backoff: 1.0 }
    }

    async fn bind_local() -> UdpTransport {
        UdpTransport::bind_addr("[::1]:0".parse().unwrap()).await.unwrap()
    }

    /// デバイス→controller のセキュアデータグラムを作る。
    fn device_datagram(
        exchange_id: u16,
        opcode: u8,
        acked: Option<u32>,
        needs_ack: bool,
        counter: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        let header = MessageHeader {
            session_id: LOCAL_SID, // デバイスは「こちらの」session id 宛に送る
            security_flags: 0,
            message_counter: counter,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: false,
            needs_ack,
            acked_counter: acked,
            opcode,
            exchange_id,
            protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        };
        seal_message(&R2I, &header, &proto, payload, DEV_NODE).unwrap()
    }

    /// デバイス側で受信 → 復号して (header, proto) を返す。
    fn open_from_controller(buf: &[u8]) -> (MessageHeader, ProtocolHeader, Vec<u8>) {
        crate::crypto::open_message(&I2R, buf, OUR_NODE).unwrap()
    }

    #[tokio::test]
    async fn send_reliable_encrypts_and_completes_on_sealed_ack() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = bind_local().await;
        let mut s = SecureSession::new(&transport, peer, LOCAL_SID, PEER_SID, keys(), OUR_NODE, DEV_NODE);
        let ex = SecureSession::new_exchange_id();

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            // 平文では読めない（先頭ヘッダ以外は暗号化されている）
            let (h, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(h.session_id, PEER_SID); // デバイス側 session id 宛
            assert!(p.needs_ack);
            assert_eq!(body, b"ping");
            let ack = device_datagram(
                p.exchange_id, OPCODE_MRP_STANDALONE_ACK, Some(h.message_counter), false, 9000, &[],
            );
            device.send_to(&ack, from).await.unwrap();
        });

        let res = s
            .send_reliable(ex, PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"ping", &fast_cfg())
            .await
            .unwrap();
        assert!(res.is_none());
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn recv_decrypts_dedups_and_acks() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = bind_local().await;
        let local = transport.local_addr().unwrap();
        let mut s = SecureSession::new(&transport, peer, LOCAL_SID, PEER_SID, keys(), OUR_NODE, DEV_NODE);
        let ex = SecureSession::new_exchange_id();

        let dev = tokio::spawn(async move {
            let msg = device_datagram(ex, OPCODE_STATUS_REPORT, None, true, 500, b"report");
            device.send_to(&msg, local).await.unwrap();
            device.send_to(&msg, local).await.unwrap(); // 重複
            // ACK は暗号化されて 2 回返る
            for _ in 0..2 {
                let mut buf = [0u8; MAX_DATAGRAM];
                let (n, _) = device.recv_from(&mut buf).await.unwrap();
                let (_, p, _) = open_from_controller(&buf[..n]);
                assert_eq!(p.opcode, OPCODE_MRP_STANDALONE_ACK);
                assert_eq!(p.acked_counter, Some(500));
            }
        });

        let got = s.recv(ex, Duration::from_millis(500)).await.unwrap();
        assert_eq!(got.payload, b"report");
        // 重複は渡ってこない
        assert!(matches!(
            s.recv(ex, Duration::from_millis(200)).await,
            Err(SessionError::Timeout)
        ));
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn ignores_wrong_key_wrong_session_and_wrong_exchange() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = bind_local().await;
        let local = transport.local_addr().unwrap();
        let mut s = SecureSession::new(&transport, peer, LOCAL_SID, PEER_SID, keys(), OUR_NODE, DEV_NODE);
        let ex = SecureSession::new_exchange_id();

        let dev = tokio::spawn(async move {
            // 鍵違い（I2R で封緘 = 復号失敗）
            let header = MessageHeader {
                session_id: LOCAL_SID, security_flags: 0, message_counter: 1,
                source_node_id: None, destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: false, needs_ack: true, acked_counter: None,
                opcode: OPCODE_STATUS_REPORT, exchange_id: ex,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL, vendor_id: None,
            };
            let bad_key = seal_message(&I2R, &header, &proto, b"x", DEV_NODE).unwrap();
            device.send_to(&bad_key, local).await.unwrap();
            // session id 違い
            let mut h2 = header;
            h2.session_id = 0x9999;
            let bad_sid = seal_message(&R2I, &h2, &proto, b"x", DEV_NODE).unwrap();
            device.send_to(&bad_sid, local).await.unwrap();
            // exchange 違い（正しく封緘されるが screening で落ちる）
            let other_ex = device_datagram(ex.wrapping_add(1), OPCODE_STATUS_REPORT, None, true, 7, b"x");
            device.send_to(&other_ex, local).await.unwrap();
        });

        assert!(matches!(
            s.recv(ex, Duration::from_millis(300)).await,
            Err(SessionError::Timeout)
        ));
        dev.await.unwrap();
    }
}
```

- [ ] **Step 6: 落ちることを確認** — `cargo test -p mat-controller session`

- [ ] **Step 7: 実装**

`session.rs` 本体（M1 `exchange.rs` の構造をミラーし、送受信を seal/open に置換）:

```rust
//! Secure unicast session and MRP-reliable exchanges over it (spec §4.7, §4.12).
//!
//! Mirrors the M1 unsecured exchange semantics — retransmit, standalone ack,
//! RxWindow dedup — but seals every datagram with the session keys. Message
//! counters and the replay window are session-scoped (not per exchange), so
//! this type owns them and exchanges are just an `exchange_id` argument.

pub struct SecureSession<'t> {
    transport: &'t UdpTransport,
    peer: SocketAddr,
    local_session_id: u16,
    peer_session_id: u16,
    keys: SessionKeys,
    local_node_id: u64,
    peer_node_id: u64,
    counter: TxCounter,
    rx_window: RxWindow,
}
```

要点:
- `new()`: `counter: TxCounter::new_random()`, `rx_window: RxWindow::new()`。
- `new_exchange_id()`: `getrandom` で乱数 u16。
- `seal(&mut self, exchange_id, protocol_id, opcode, needs_ack, acked, payload) -> Result<(Vec<u8>, u32), SessionError>`: header は `session_id: self.peer_session_id`, `security_flags: 0`, `source_node_id: None`, `destination: Destination::None`。proto は `initiator: true`。`seal_message(&self.keys.i2r, .., self.local_node_id)`。
- `screen(&mut self, buf, from, exchange_id)`: ① `from == self.peer` ② 平文ヘッダを `MessageHeader::decode`（失敗 = 無視）③ `header.session_id == self.local_session_id` ④ `open_message(&self.keys.r2i, buf, self.peer_node_id)`（`OpenError` = 無視: 攻撃・破損データグラムで落ちない）⑤ `rx_window.check_and_commit`（重複は needs_ack なら再 ACK して skip）⑥ `proto.exchange_id == exchange_id && !proto.initiator`（違えば skip、ACK しない）⑦ needs_ack なら暗号化 standalone ack 送信。
- `send_reliable` / `recv`: M1 と同じ再送・選別ループ。standalone ack の判定も同一（SC 0x10）。
- `SessionError` の `From` 実装（io / MessageError / CryptoError）。

Note: DSIZ=0b11 は `MessageHeader::decode` が `ReservedDestination` を返すため、secured 受信経路では復号前に弾かれる（M2 spec の申し送り対応。screen では他の不正データグラムと同様 skip 扱い）。

- [ ] **Step 8: 通ることを確認** — `cargo test -p mat-controller` → 全 PASS（message/exchange の既存テスト含む）

- [ ] **Step 9: `task check` → コミット**

```bash
git add crates/mat-controller/src/message.rs crates/mat-controller/src/exchange.rs crates/mat-controller/src/session.rs crates/mat-controller/src/lib.rs
git commit -m "feat(mat-controller): secure session with MRP semantics; reject reserved DSIZ"
```

---

### Task 7: `case` — CASE initiator 状態機械

**Files:**
- Create: `crates/mat-controller/src/case.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod case;`）

**Interfaces:**
- Consumes: `fabric::{FabricCredentials, case_destination_id}`、`cert::{MatterCert, verify_noc_chain}`、`crypto::{encrypt_payload, decrypt_payload}`、`exchange::{UnsecuredExchange, MrpConfig, ExchangeError}`、`session::{SecureSession, SessionKeys}`、`tlv`、p256 / sha2 / hkdf
- Produces:
  ```rust
  pub enum CaseError {
      Exchange(ExchangeError),
      UnexpectedMessage { stage: &'static str, opcode: u8 },
      PeerStatus { stage: &'static str, general_code: u16, protocol_code: u16 },
      Sigma2NotAcked,
      Sigma2Malformed(&'static str),
      Tbe2DecryptFailed,
      PeerCertInvalid(crate::cert::CertError),
      PeerIdentityMismatch { expected_node_id: u64, cert_node_id: u64, expected_fabric_id: u64, cert_fabric_id: u64 },
      Sigma2SignatureInvalid,
      EstablishmentFailed { general_code: u16, protocol_code: u16 }, // StatusReport が success でない
      Crypto(&'static str),
  }
  pub async fn establish<'t>(
      transport: &'t UdpTransport,
      peer: std::net::SocketAddr,
      creds: &FabricCredentials,
      peer_node_id: u64,
      cfg: &MrpConfig,
  ) -> Result<SecureSession<'t>, CaseError>;
  ```
- 内部ヘルパ（テスト対象、`pub(crate)`）: `encode_sigma1`, `parse_sigma2`, `decrypt_tbe2`, `parse_status_report`, `derive_sigma_key`, `derive_session_keys`

- [ ] **Step 1: 定数を定義**

```rust
const OPCODE_CASE_SIGMA1: u8 = 0x30;
const OPCODE_CASE_SIGMA2: u8 = 0x31;
const OPCODE_CASE_SIGMA3: u8 = 0x32;
// StatusReport は message::OPCODE_STATUS_REPORT (0x40)
const TBE2_NONCE: &[u8; 13] = b"NCASE_Sigma2N";
const TBE3_NONCE: &[u8; 13] = b"NCASE_Sigma3N";
const INFO_S2K: &[u8] = b"Sigma2";
const INFO_S3K: &[u8] = b"Sigma3";
const INFO_SESSION_KEYS: &[u8] = b"SessionKeys";
const STATUS_SUCCESS: (u16, u32, u16) = (0, 0, 0); // (general, protocol id, code)
```

- [ ] **Step 2: 失敗するテストを書く**（ワイヤに出ない純関数部分）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn sigma1_has_spec_structure() {
        let random = [0xAB; 32];
        let dest = [0xCD; 32];
        let eph = [0x04; 65];
        let buf = encode_sigma1(&random, 0x0BB8, &dest, &eph);
        let mut r = Reader::new(&buf);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let e = r.next().unwrap().unwrap();
        assert_eq!((e.tag, e.value), (Tag::Context(1), Value::Bytes(&random)));
        let e = r.next().unwrap().unwrap();
        assert_eq!((e.tag, e.value), (Tag::Context(2), Value::Uint(0x0BB8)));
        let e = r.next().unwrap().unwrap();
        assert_eq!((e.tag, e.value), (Tag::Context(3), Value::Bytes(&dest)));
        let e = r.next().unwrap().unwrap();
        assert_eq!((e.tag, e.value), (Tag::Context(4), Value::Bytes(&eph)));
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);
        assert_eq!(r.next().unwrap(), None); // optional は送らない
    }

    #[test]
    fn parses_sigma2_and_skips_session_params() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[0x11; 32]);
        w.put_uint(Tag::Context(2), 0x1234);
        w.put_bytes(Tag::Context(3), &[0x22; 65]);
        w.put_bytes(Tag::Context(4), b"encrypted-blob");
        w.start_struct(Tag::Context(5)); // session params は読み飛ばす
        w.put_uint(Tag::Context(1), 5000);
        w.end_container();
        w.end_container();
        let s2 = parse_sigma2(&w.finish()).unwrap();
        assert_eq!(s2.responder_random, [0x11; 32]);
        assert_eq!(s2.responder_session_id, 0x1234);
        assert_eq!(s2.responder_eph_pub, [0x22; 65]);
        assert_eq!(s2.encrypted2, b"encrypted-blob");
        assert!(parse_sigma2(&[0x15, 0x18]).is_err()); // 必須欠落
    }

    #[test]
    fn decrypts_and_parses_tbe2_roundtrip() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), b"noc-tlv");
        w.put_bytes(Tag::Context(3), &[0x77; 64]);
        w.put_bytes(Tag::Context(4), &[0x88; 16]);
        w.end_container();
        let key = [0x42; 16];
        let ct = crate::crypto::encrypt_payload(&key, TBE2_NONCE, b"", &w.finish()).unwrap();
        let tbe = decrypt_tbe2(&key, &ct).unwrap();
        assert_eq!(tbe.noc, b"noc-tlv");
        assert_eq!(tbe.icac, None);
        assert_eq!(tbe.signature, [0x77; 64]);
        assert!(matches!(decrypt_tbe2(&[0x00; 16], &ct), Err(CaseError::Tbe2DecryptFailed)));
    }

    #[test]
    fn parses_status_report() {
        let ok = [0u8, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_status_report(&ok).unwrap(), (0, 0, 0));
        let busy = [1u8, 0, 0, 0, 0, 0, 4, 0]; // FAILURE / SC / BUSY
        assert_eq!(parse_status_report(&busy).unwrap(), (1, 0, 4));
        assert!(parse_status_report(&[0u8; 4]).is_err());
    }

    #[test]
    fn session_key_derivation_is_deterministic() {
        let keys = derive_session_keys(&[0x01; 32], &[0x02; 16], &[0x03; 32]);
        let again = derive_session_keys(&[0x01; 32], &[0x02; 16], &[0x03; 32]);
        assert_eq!(keys.i2r, again.i2r);
        assert_eq!(keys.r2i, again.r2i);
        assert_ne!(keys.i2r, keys.r2i);
    }
}
```

- [ ] **Step 3: 落ちることを確認** — `cargo test -p mat-controller case`

- [ ] **Step 4: ヘルパ実装**

```rust
pub(crate) struct Sigma2 {
    pub responder_random: [u8; 32],
    pub responder_session_id: u16,
    pub responder_eph_pub: [u8; 65],
    pub encrypted2: Vec<u8>,
}
pub(crate) struct Tbe2 {
    pub noc: Vec<u8>,
    pub icac: Option<Vec<u8>>,
    pub signature: [u8; 64],
}

pub(crate) fn encode_sigma1(random: &[u8; 32], session_id: u16, dest_id: &[u8; 32], eph_pub: &[u8; 65]) -> Vec<u8> { /* Writer で struct{1,2,3,4} */ }
pub(crate) fn parse_sigma2(payload: &[u8]) -> Result<Sigma2, CaseError> { /* Reader 走査、tag5 等未知タグはコンテナ深さを数えて skip */ }
pub(crate) fn decrypt_tbe2(s2k: &[u8; 16], encrypted2: &[u8]) -> Result<Tbe2, CaseError> { /* decrypt_payload(aad=b"") → Reader */ }
pub(crate) fn parse_status_report(payload: &[u8]) -> Result<(u16, u32, u16), CaseError> { /* 8B LE */ }

pub(crate) fn derive_sigma_key(shared: &[u8], salt: &[u8], info: &[u8]) -> [u8; 16] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(salt), shared);
    let mut out = [0u8; 16];
    hk.expand(info, &mut out).expect("valid length");
    out
}

pub(crate) fn derive_session_keys(shared: &[u8], ipk: &[u8; 16], transcript: &[u8; 32]) -> SessionKeys {
    let mut salt = Vec::with_capacity(48);
    salt.extend_from_slice(ipk);
    salt.extend_from_slice(transcript);
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(&salt), shared);
    let mut okm = [0u8; 48];
    hk.expand(INFO_SESSION_KEYS, &mut okm).expect("valid length");
    SessionKeys {
        i2r: okm[..16].try_into().expect("16"),
        r2i: okm[16..32].try_into().expect("16"),
        attestation_challenge: okm[32..].try_into().expect("16"),
    }
}
```

- [ ] **Step 5: `establish` 実装**

```rust
pub async fn establish<'t>(
    transport: &'t UdpTransport,
    peer: SocketAddr,
    creds: &FabricCredentials,
    peer_node_id: u64,
    cfg: &MrpConfig,
) -> Result<SecureSession<'t>, CaseError> {
    use sha2::Digest;

    // 1. 素材: initiator random / ephemeral 鍵 / local session id（非ゼロ乱数）
    let mut initiator_random = [0u8; 32];
    getrandom::getrandom(&mut initiator_random).expect("os rng");
    let eph_secret = random_p256_secret();
    let eph_pub: [u8; 65] = eph_secret
        .public_key()
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .expect("uncompressed p256 point is 65 bytes");
    let local_session_id = random_nonzero_u16();

    // 2. Sigma1
    let dest_id = crate::fabric::case_destination_id(
        &creds.ipk_operational, &initiator_random, &creds.root_public_key,
        creds.fabric_id, peer_node_id,
    );
    let sigma1 = encode_sigma1(&initiator_random, local_session_id, &dest_id, &eph_pub);
    let mut transcript = sha2::Sha256::new();
    transcript.update(&sigma1);

    let mut ex = UnsecuredExchange::new(transport, peer);
    let resp = ex
        .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_CASE_SIGMA1, &sigma1, cfg)
        .await
        .map_err(CaseError::Exchange)?;
    let msg = match resp {
        Some(m) => m,
        None => ex.recv(RECV_TIMEOUT).await.map_err(CaseError::Exchange)?, // standalone ack 先行
    };
    // M1 申し送り: 実応答が Sigma1 を ack していることを明示確認
    if msg.proto.acked_counter != ex.last_sent_counter() {
        return Err(CaseError::Sigma2NotAcked);
    }
    match msg.proto.opcode {
        OPCODE_CASE_SIGMA2 => {}
        crate::message::OPCODE_STATUS_REPORT => {
            let (g, _p, c) = parse_status_report(&msg.payload)?;
            return Err(CaseError::PeerStatus { stage: "sigma1", general_code: g, protocol_code: c });
        }
        op => return Err(CaseError::UnexpectedMessage { stage: "sigma1", opcode: op }),
    }

    // 3. Sigma2 検証
    let sigma2 = parse_sigma2(&msg.payload)?;
    let shared = ecdh(&eph_secret, &sigma2.responder_eph_pub)?; // 32B
    let sigma1_hash: [u8; 32] = transcript.clone().finalize().into();
    let mut s2k_salt = Vec::with_capacity(16 + 32 + 65 + 32);
    s2k_salt.extend_from_slice(&creds.ipk_operational);
    s2k_salt.extend_from_slice(&sigma2.responder_random);
    s2k_salt.extend_from_slice(&sigma2.responder_eph_pub);
    s2k_salt.extend_from_slice(&sigma1_hash);
    let s2k = derive_sigma_key(&shared, &s2k_salt, INFO_S2K);
    transcript.update(&msg.payload); // salt 計算後に Sigma2 を足す（chip と同順）

    let tbe2 = decrypt_tbe2(&s2k, &sigma2.encrypted2)?;
    let peer_noc = MatterCert::parse(&tbe2.noc).map_err(CaseError::PeerCertInvalid)?;
    let peer_icac = tbe2.icac.as_deref().map(MatterCert::parse).transpose()
        .map_err(CaseError::PeerCertInvalid)?;
    let our_rcac = MatterCert::parse(&creds.rcac_tlv).map_err(CaseError::PeerCertInvalid)?;
    verify_noc_chain(&peer_noc, peer_icac.as_ref(), &our_rcac).map_err(CaseError::PeerCertInvalid)?;
    let (cert_node, cert_fabric) = (
        peer_noc.node_id().expect("verify_noc_chain guarantees ids"),
        peer_noc.fabric_id().expect("verify_noc_chain guarantees ids"),
    );
    if cert_node != peer_node_id || cert_fabric != creds.fabric_id {
        return Err(CaseError::PeerIdentityMismatch { /* 4 値 */ });
    }
    // Sigma2 TBS 署名検証（NOC の公開鍵で。TBS = struct{1:noc, 2:icac?, 3:responder eph, 4:our eph}）
    let tbs2 = encode_tbs(&tbe2.noc, tbe2.icac.as_deref(), &sigma2.responder_eph_pub, &eph_pub);
    verify_raw_ecdsa(&peer_noc.pub_key, &tbs2, &tbe2.signature).map_err(|_| CaseError::Sigma2SignatureInvalid)?;

    // 4. Sigma3
    let tbs3 = encode_tbs(&creds.noc_tlv, creds.icac_tlv.as_deref(), &eph_pub, &sigma2.responder_eph_pub);
    let signature = sign_raw_ecdsa(&creds.op_private_key, &tbs3)?; // 64B r||s
    let tbe3 = encode_tbe3(&creds.noc_tlv, creds.icac_tlv.as_deref(), &signature);
    let sigma2_hash: [u8; 32] = transcript.clone().finalize().into();
    let mut s3k_salt = Vec::with_capacity(48);
    s3k_salt.extend_from_slice(&creds.ipk_operational);
    s3k_salt.extend_from_slice(&sigma2_hash);
    let s3k = derive_sigma_key(&shared, &s3k_salt, INFO_S3K);
    let encrypted3 = crate::crypto::encrypt_payload(&s3k, TBE3_NONCE, b"", &tbe3)
        .map_err(|_| CaseError::Crypto("sigma3 payload too large"))?;
    let sigma3 = encode_sigma3(&encrypted3); // struct{1: bytes}
    transcript.update(&sigma3);

    let resp = ex
        .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_CASE_SIGMA3, &sigma3, cfg)
        .await
        .map_err(CaseError::Exchange)?;
    let msg = match resp {
        Some(m) => m,
        None => ex.recv(RECV_TIMEOUT).await.map_err(CaseError::Exchange)?,
    };
    if msg.proto.opcode != crate::message::OPCODE_STATUS_REPORT {
        return Err(CaseError::UnexpectedMessage { stage: "sigma3", opcode: msg.proto.opcode });
    }
    let (general, proto_id, code) = parse_status_report(&msg.payload)?;
    if (general, proto_id, code) != STATUS_SUCCESS {
        return Err(CaseError::EstablishmentFailed { general_code: general, protocol_code: code });
    }

    // 5. セッション鍵
    let final_hash: [u8; 32] = transcript.finalize().into();
    let keys = derive_session_keys(&shared, &creds.ipk_operational, &final_hash);
    Ok(SecureSession::new(
        transport, peer, local_session_id, sigma2.responder_session_id, keys,
        creds.node_id, peer_node_id,
    ))
}
```

補助（同ファイル内）:
- `RECV_TIMEOUT`: `Duration::from_secs(10)`（TBE 計算・デバイス側検証の余裕）
- `random_p256_secret()`: `getrandom` 32B → `p256::SecretKey::from_slice`、失敗（0 or ≥n）はループ。
- `random_nonzero_u16()`: 0 ならループ。
- `ecdh(secret, peer_pub65) -> Result<[u8;32], CaseError>`: `p256::PublicKey::from_sec1_bytes`（失敗 = `Sigma2Malformed("responder ephemeral key")`）→ `p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), pk.as_affine())` → `raw_secret_bytes()`。
- `encode_tbs(noc, icac, sender_eph, receiver_eph)`: Writer struct{1: noc bytes, [2: icac bytes], 3: sender, 4: receiver}。
- `encode_tbe3(noc, icac, sig)`: struct{1: noc, [2: icac], 3: sig}。`encode_sigma3(enc)`: struct{1: enc}。
- `sign_raw_ecdsa(priv32, msg) -> Result<[u8;64], CaseError>`: `p256::ecdsa::SigningKey::from_slice` → `use p256::ecdsa::signature::Signer; let sig: p256::ecdsa::Signature = key.sign(msg);` → `sig.to_bytes().into()`。
- `verify_raw_ecdsa(pub65, msg, sig64)`: `VerifyingKey::from_sec1_bytes` + `Signature::from_slice` + `verify`。

`CaseError` の `Display` は「どの段階で何が拒否されたか」を必ず含める（M4 での kind 写像材料）。例: `"case sigma2: peer certificate chain invalid: <cert error>"`。

- [ ] **Step 6: 通ることを確認** — `cargo test -p mat-controller case` → PASS (5 tests)

- [ ] **Step 7: `task check` → コミット**

```bash
git add crates/mat-controller/src/case.rs crates/mat-controller/src/lib.rs
git commit -m "feat(mat-controller): CASE initiator (Sigma1-3, NOC chain + signature verification)"
```

---

### Task 8: `im` — IM ペイロード + `SecureSession::read_attribute` / `invoke`

**Files:**
- Create: `crates/mat-controller/src/im.rs`
- Modify: `crates/mat-controller/src/session.rs`（IM メソッド追加、`SessionError::Im` 追加）
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod im;`）

**Interfaces:**
- Consumes: `tlv`、`session::SecureSession`
- Produces:
  ```rust
  // im.rs
  pub const PROTOCOL_ID_IM: u16 = crate::message::PROTOCOL_ID_INTERACTION_MODEL; // 0x0001
  pub const OPCODE_STATUS_RESPONSE: u8 = 0x01;
  pub const OPCODE_READ_REQUEST: u8 = 0x02;
  pub const OPCODE_REPORT_DATA: u8 = 0x05;
  pub const OPCODE_INVOKE_REQUEST: u8 = 0x08;
  pub const OPCODE_INVOKE_RESPONSE: u8 = 0x09;
  pub const IM_REVISION: u8 = 12;
  pub const CLUSTER_ON_OFF: u32 = 0x0006;
  pub const ATTR_ON_OFF: u32 = 0x0000;
  pub const CMD_ON_OFF_OFF: u32 = 0x00;
  pub const CMD_ON_OFF_ON: u32 = 0x01;
  pub const CMD_ON_OFF_TOGGLE: u32 = 0x02;

  #[derive(Debug, Clone, PartialEq)]
  pub enum ImValue { Bool(bool), Uint(u64), Int(i64), Utf8(String), Bytes(Vec<u8>), Null }
  pub struct ReportData { pub suppress_response: bool, pub value: Option<ImValue>, pub status: Option<u8> }
  pub struct InvokeOutcome { pub status: u8, pub cluster_status: Option<u8> }
  pub enum ImError { Tlv(crate::tlv::TlvError), Malformed(&'static str), UnsupportedValue, AttributeStatus(u8), StatusResponse(u8), CommandStatus { status: u8, cluster_status: Option<u8> } }

  pub fn encode_read_request(endpoint: u16, cluster: u32, attribute: u32) -> Vec<u8>;
  pub fn decode_report_data(payload: &[u8]) -> Result<ReportData, ImError>;
  pub fn encode_invoke_request(endpoint: u16, cluster: u32, command: u32, fields_tlv: Option<&[u8]>) -> Vec<u8>;
  pub fn decode_invoke_response(payload: &[u8]) -> Result<InvokeOutcome, ImError>;
  pub fn encode_status_response(status: u8) -> Vec<u8>;
  pub fn decode_status_response(payload: &[u8]) -> Result<u8, ImError>;

  // session.rs 追加
  impl SecureSession<'_> {
      pub async fn read_attribute(&mut self, endpoint: u16, cluster: u32, attribute: u32, cfg: &MrpConfig)
          -> Result<ImValue, SessionError>;
      pub async fn invoke(&mut self, endpoint: u16, cluster: u32, command: u32,
                          fields_tlv: Option<&[u8]>, cfg: &MrpConfig)
          -> Result<InvokeOutcome, SessionError>;
  }
  // SessionError に Im(ImError) と UnexpectedOpcode(u8) を追加
  ```

- [ ] **Step 1: im.rs の失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn read_request_has_spec_structure() {
        let buf = encode_read_request(1, CLUSTER_ON_OFF, ATTR_ON_OFF);
        let mut r = Reader::new(&buf);
        let mut els = Vec::new();
        while let Some(e) = r.next().unwrap() {
            els.push(e);
        }
        // struct{ 0: array[ list{2,3,4} ], 3: false, 255: 12 }
        assert_eq!(els[0].value, Value::StructStart);
        assert_eq!((els[1].tag, els[1].value), (Tag::Context(0), Value::ArrayStart));
        assert_eq!(els[2].value, Value::ListStart);
        assert_eq!((els[3].tag, els[3].value), (Tag::Context(2), Value::Uint(1)));
        assert_eq!((els[4].tag, els[4].value), (Tag::Context(3), Value::Uint(0x0006)));
        assert_eq!((els[5].tag, els[5].value), (Tag::Context(4), Value::Uint(0)));
        assert_eq!(els[6].value, Value::ContainerEnd); // list
        assert_eq!(els[7].value, Value::ContainerEnd); // array
        assert_eq!((els[8].tag, els[8].value), (Tag::Context(3), Value::Bool(false)));
        assert_eq!((els[9].tag, els[9].value), (Tag::Context(255), Value::Uint(12)));
        assert_eq!(els[10].value, Value::ContainerEnd);
    }

    fn report_data(value: bool, suppress: bool) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1)); // AttributeReportIBs
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // AttributeData
        w.put_uint(Tag::Context(0), 1); // DataVersion
        w.start_list(Tag::Context(1)); // Path
        w.put_uint(Tag::Context(2), 1);
        w.put_uint(Tag::Context(3), 6);
        w.put_uint(Tag::Context(4), 0);
        w.end_container();
        w.put_bool(Tag::Context(2), value); // Data
        w.end_container();
        w.end_container();
        w.end_container();
        if suppress {
            w.put_bool(Tag::Context(4), true);
        }
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        w.finish()
    }

    #[test]
    fn decodes_report_data_bool() {
        let rd = decode_report_data(&report_data(true, true)).unwrap();
        assert!(rd.suppress_response);
        assert_eq!(rd.value, Some(ImValue::Bool(true)));
        assert_eq!(rd.status, None);
        let rd = decode_report_data(&report_data(false, false)).unwrap();
        assert!(!rd.suppress_response);
        assert_eq!(rd.value, Some(ImValue::Bool(false)));
    }

    #[test]
    fn decodes_report_data_attribute_status() {
        // AttributeStatus (tag 0) = 読めない属性: struct{0: Path, 1: StatusIB{0: status}}
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(0)); // AttributeStatus
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0x86); // UNSUPPORTED_ATTRIBUTE
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_bool(Tag::Context(4), true);
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let rd = decode_report_data(&w.finish()).unwrap();
        assert_eq!(rd.status, Some(0x86));
        assert_eq!(rd.value, None);
    }

    #[test]
    fn invoke_request_and_response_roundtrip_shapes() {
        let buf = encode_invoke_request(1, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None);
        let mut r = Reader::new(&buf);
        let mut els = Vec::new();
        while let Some(e) = r.next().unwrap() {
            els.push(e);
        }
        assert_eq!((els[1].tag, els[1].value), (Tag::Context(0), Value::Bool(false)));
        assert_eq!((els[2].tag, els[2].value), (Tag::Context(1), Value::Bool(false)));
        assert_eq!((els[3].tag, els[3].value), (Tag::Context(2), Value::ArrayStart));
        // CommandDataIB struct → path list {0:1, 1:6, 2:2}
        assert_eq!(els[4].value, Value::StructStart);
        assert_eq!((els[5].tag, els[5].value), (Tag::Context(0), Value::ListStart));
        assert_eq!(els[6].value, Value::Uint(1));
        assert_eq!(els[7].value, Value::Uint(6));
        assert_eq!(els[8].value, Value::Uint(2));

        // InvokeResponse: Status(成功)
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // Status = CommandStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let out = decode_invoke_response(&w.finish()).unwrap();
        assert_eq!(out.status, 0);
        assert_eq!(out.cluster_status, None);
    }

    #[test]
    fn status_response_roundtrip() {
        assert_eq!(decode_status_response(&encode_status_response(0)).unwrap(), 0);
        assert_eq!(decode_status_response(&encode_status_response(0x7E)).unwrap(), 0x7E);
    }
}
```

- [ ] **Step 2: 落ちることを確認** — `cargo test -p mat-controller im`

- [ ] **Step 3: im.rs 実装**

エンコーダは `tlv::Writer` の素直な組み立て（構造は「プロトコル事実」節どおり）。デコーダは `tlv::Reader` の走査で実装。要点:
- **深さ追跡 skip**: 未知タグ・興味のないコンテナは、`*Start` で depth+1 / `ContainerEnd` で depth-1 しながら読み飛ばすヘルパ `skip_container(reader)` を書く。
- `decode_report_data`: struct 直下を走査。`Context(1)` (array) の**先頭** AttributeReportIB のみ解釈（単一属性 read 前提。2 個目以降は無視）。`Context(4)` → suppress。IB 内 `Context(0)` → AttributeStatus → StatusIB の `Context(0)` uint を `status` に。`Context(1)` → AttributeData → `Context(2)` の Data 値を `ImValue` へ（Bool/Uint/Int/Utf8/Bytes/Null。コンテナ開始なら `UnsupportedValue`）。value も status も無ければ `Malformed("empty report")`。
- `decode_invoke_response`: `Context(1)` array の先頭 InvokeResponseIB。`Context(1)`(Status) → CommandStatusIB `Context(1)`(StatusIB) の `Context(0)` / `Context(1)`。`Context(0)`(Command) が来たら成功扱い `InvokeOutcome { status: 0, cluster_status: None }`（M2 の onoff では発生しないがフィールド付き応答を壊さない）。
- `ImError` の Display は具体的に（例: `"device rejected read: IM status 0x86"`）。

- [ ] **Step 4: session.rs に IM メソッドの失敗するテストを書く**

session.rs の tests に追加（Task 6 のヘルパを流用。デバイス役は受信 → `open_from_controller` → im 形の応答を `device_datagram` で返す）:

```rust
    #[tokio::test]
    async fn read_attribute_roundtrip() {
        // デバイス役: ReadRequest を受けたら ReportData(bool true, suppress) を ack 付きで返す
        // (payload は Task 8 Step 1 の report_data() 相当を Writer で構築)
        // 検証: read_attribute が Ok(ImValue::Bool(true))
    }

    #[tokio::test]
    async fn invoke_roundtrip_and_status_response_error() {
        // デバイス役1: InvokeRequest → InvokeResponse(status 0) → invoke が Ok(status=0)
        // デバイス役2: ReadRequest → StatusResponse(0x7E ACCESS_DENIED) →
        //             read_attribute が Err(SessionError::Im(ImError::StatusResponse(0x7E)))
    }
```

（コード全文は Task 6 のテストと同型のため省略しない実装を書くこと。`device_datagram` の `protocol_id` は IM のとき `PROTOCOL_ID_INTERACTION_MODEL` を使うよう引数を追加調整してよい。）

- [ ] **Step 5: 落ちることを確認** → **Step 6: session.rs 実装**

```rust
impl SecureSession<'_> {
    pub async fn read_attribute(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        cfg: &MrpConfig,
    ) -> Result<crate::im::ImValue, SessionError> {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_read_request(endpoint, cluster, attribute);
        let resp = self
            .send_reliable(exchange_id, im::PROTOCOL_ID_IM, im::OPCODE_READ_REQUEST, &req, cfg)
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        match msg.proto.opcode {
            im::OPCODE_REPORT_DATA => {
                let rd = im::decode_report_data(&msg.payload).map_err(SessionError::Im)?;
                if !rd.suppress_response {
                    // 相手が StatusResponse を待っている
                    let ok = im::encode_status_response(0);
                    let _ = self
                        .send_reliable(exchange_id, im::PROTOCOL_ID_IM, im::OPCODE_STATUS_RESPONSE, &ok, cfg)
                        .await?;
                }
                if let Some(status) = rd.status {
                    return Err(SessionError::Im(ImError::AttributeStatus(status)));
                }
                rd.value.ok_or(SessionError::Im(ImError::Malformed("no value")))
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                Err(SessionError::Im(ImError::StatusResponse(s)))
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
        }
    }

    pub async fn invoke(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields_tlv: Option<&[u8]>,
        cfg: &MrpConfig,
    ) -> Result<crate::im::InvokeOutcome, SessionError> {
        // 同型: OPCODE_INVOKE_REQUEST → OPCODE_INVOKE_RESPONSE を decode_invoke_response、
        // OPCODE_STATUS_RESPONSE は ImError::StatusResponse。
        // outcome.status != 0 は Err(SessionError::Im(ImError::CommandStatus{..})) にする。
    }
}
```

`IM_RECV_TIMEOUT`: `Duration::from_secs(10)`。

- [ ] **Step 7: 通ることを確認** — `cargo test -p mat-controller` 全 PASS

- [ ] **Step 8: `task check` → コミット**

```bash
git add crates/mat-controller/src/im.rs crates/mat-controller/src/session.rs crates/mat-controller/src/lib.rs
git commit -m "feat(mat-controller): minimal IM read/invoke over secure session"
```

---

### Task 9: ライブ E2E + `task e2e:m2` ハーネス + ドキュメント

**Files:**
- Create: `crates/mat-controller/tests/live_case_im.rs`
- Create: `scripts/e2e-m2.sh`
- Modify: `Taskfile.yml`（`e2e:m2` タスク追加）
- Modify: `ARCHITECTURE.md`（Phase 5 の進捗記述に M2 を反映。既存の M1 記述の書き方に合わせ 1–2 行）

**Interfaces:**
- Consumes: これまでの全モジュール
- 環境変数: `MAT_E2E_KVS`（chip-tool の ini パス、必須）、`MAT_E2E_NODE_ID`（デバイス node id、`0x` 接頭辞可、必須）、`MAT_E2E_PEER`（省略時 `[::1]:5540`）

- [ ] **Step 1: ライブテストを書く**

```rust
//! Live E2E: CASE + IM against a commissioned chip-all-clusters-app.
//! Run via `task e2e:m2` (sets up a throwaway fabric) — see scripts/e2e-m2.sh.
//! Not run in CI; requires MAT_E2E_KVS / MAT_E2E_NODE_ID.

use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::FabricCredentials;
use mat_controller::im::{ImValue, ATTR_ON_OFF, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE};
use mat_controller::message::MATTER_PORT;
use mat_controller::transport::UdpTransport;
use mat_controller::{case, kvs};

fn env_node_id() -> u64 {
    let s = std::env::var("MAT_E2E_NODE_ID").expect("MAT_E2E_NODE_ID required");
    match s.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).expect("hex node id"),
        None => s.parse().expect("decimal node id"),
    }
}

#[tokio::test]
#[ignore = "requires a commissioned device and chip-tool KVS (task e2e:m2)"]
async fn case_read_toggle_read() {
    let kvs_path = std::path::PathBuf::from(
        std::env::var("MAT_E2E_KVS").expect("MAT_E2E_KVS required"),
    );
    let node_id = env_node_id();
    let peer = std::env::var("MAT_E2E_PEER")
        .unwrap_or_else(|_| format!("[::1]:{MATTER_PORT}"))
        .parse()
        .expect("MAT_E2E_PEER must be a socket address");

    // 受け入れ 2: KVS から資格情報 5 項目
    let raw = kvs::read_fabric_credentials(&kvs_path, 1).expect("kvs read");
    let creds = FabricCredentials::from_raw(raw).expect("credentials assemble+verify");
    eprintln!(
        "fabric id 0x{:016X}, controller node id 0x{:016X}, icac: {}",
        creds.fabric_id,
        creds.node_id,
        creds.icac_tlv.is_some()
    );

    // 受け入れ 3: CASE 確立
    let transport = UdpTransport::bind().await.unwrap();
    let cfg = MrpConfig::default();
    let mut session = case::establish(&transport, peer, &creds, node_id, &cfg)
        .await
        .expect("CASE establishment");
    eprintln!("CASE established with node 0x{:016X}", session.peer_node_id());

    // 受け入れ 5(前半): read on-off
    let before = session
        .read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg)
        .await
        .expect("read on-off");
    let ImValue::Bool(before) = before else {
        panic!("on-off should be bool, got {before:?}")
    };
    eprintln!("on-off before: {before}");

    // 受け入れ 4: toggle invoke
    let outcome = session
        .invoke(1, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, &cfg)
        .await
        .expect("invoke toggle");
    assert_eq!(outcome.status, 0);

    // 受け入れ 5: 変化後の値
    let after = session
        .read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg)
        .await
        .expect("read on-off after toggle");
    assert_eq!(after, ImValue::Bool(!before), "toggle must flip on-off");
    eprintln!("on-off after: {:?}", after);
}
```

- [ ] **Step 2: ハーネススクリプト**

`scripts/e2e-m2.sh`（`chmod +x`）:

```bash
#!/usr/bin/env bash
# Phase 5 M2 受け入れ E2E: 使い捨て fabric を作って CASE + IM を検証する。
# 前提: ./chip-all-clusters-app と ./chip-tool（task chip:extract:app / chip:extract）
set -euo pipefail
cd "$(dirname "$0")/.."

APP=${MAT_E2E_APP:-./chip-all-clusters-app}
CHIP_TOOL=${MAT_CHIP_TOOL_BIN:-./chip-tool}
NODE_ID=0x12344321
PASSCODE=20202021

[[ -x "$APP" ]] || { echo "error: $APP がない（task chip:extract:app）" >&2; exit 1; }
[[ -x "$CHIP_TOOL" ]] || { echo "error: $CHIP_TOOL がない（task chip:extract）" >&2; exit 1; }

WORK=$(mktemp -d)
APP_PID=""
cleanup() {
  [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "== 1/3 chip-all-clusters-app 起動 (KVS: $WORK/device_kvs)"
"$APP" --KVS "$WORK/device_kvs" >"$WORK/app.log" 2>&1 &
APP_PID=$!
sleep 2
kill -0 "$APP_PID" || { echo "error: app が起動しない"; cat "$WORK/app.log"; exit 1; }

echo "== 2/3 chip-tool で使い捨て fabric にコミッション (node $NODE_ID)"
"$CHIP_TOOL" pairing already-discovered "$NODE_ID" "$PASSCODE" ::1 5540 \
  --storage-directory "$WORK" >"$WORK/pairing.log" 2>&1 \
  || { echo "error: pairing 失敗"; tail -40 "$WORK/pairing.log"; exit 1; }
grep -q "Device commissioning completed with success" "$WORK/pairing.log" \
  || { echo "error: コミッション成功ログが見つからない"; tail -40 "$WORK/pairing.log"; exit 1; }

echo "== 3/3 CASE + IM ライブテスト"
MAT_E2E_KVS="$WORK/chip_tool_config.ini" \
MAT_E2E_NODE_ID="$NODE_ID" \
MAT_E2E_PEER="[::1]:5540" \
  cargo test -p mat-controller --test live_case_im -- --ignored --nocapture

echo "== e2e:m2 PASS"
```

- [ ] **Step 3: Taskfile に追加**（`e2e:m1` の直後）

```yaml
  e2e:m2:
    desc: M2 ライブ E2E（使い捨て fabric で CASE + onoff toggle/read。要 ./chip-tool と ./chip-all-clusters-app）
    cmds:
      - bash scripts/e2e-m2.sh
```

- [ ] **Step 4: CI 相当が通ることを確認**

Run: `task check`
Expected: PASS（ライブテストは `#[ignore]` なので走らない）

- [ ] **Step 5: 受け入れ実行**

Run: `task e2e:m2`
Expected: `== e2e:m2 PASS`（受け入れ基準 1–5 を一括検証）

失敗時の切り分け:
- pairing 失敗 → `$WORK/pairing.log`（chip-tool は診断ログを **stdout** に出す）。ポート 5540 の残プロセス（`ss -ulpn | grep 5540`）と孤児 all-clusters-app（`pkill -f chip-all-clusters-app`）を確認。
- CASE Sigma2 が来ずタイムアウト → destination id 不一致が定番（IPK / root pub key / fabric id / node id のどれか）。`CaseError` の stage 表示を確認。
- `Tbe2DecryptFailed` → S2K 導出（salt 連結順・transcript タイミング）を疑う。
- 証明書チェーン失敗 → Task 4 の TBS 一致テストが通っている限り、KVS から読んだ TLV の切り出しを疑う。

- [ ] **Step 6: ARCHITECTURE.md 更新**

Phase 5 の記述（`M1` で検索）に、M1 の記述スタイルに合わせて M2 完了を 1–2 行追記（例: 「M2: KVS リーダ + CASE + IM read/invoke、ローカル all-clusters-app で E2E 合格（`task e2e:m2`）」）。

- [ ] **Step 7: `task check` → コミット**

```bash
task check
git add crates/mat-controller/tests/live_case_im.rs scripts/e2e-m2.sh Taskfile.yml ARCHITECTURE.md
git commit -m "feat(mat-controller): M2 live E2E (CASE + onoff toggle/read) with throwaway-fabric harness"
```

---

## Self-Review チェック済み事項

- spec スコープ 6 モジュール（kvs/cert/fabric/case/session/im）→ Task 2/4/1+5/7/6/8。受け入れ基準 1–5 → Task 9 のスクリプト+テストに対応付けあり。M3 再定義追記 → Task 1 Step 1。M1 申し送り 3 点（acked_counter 明示確認 / DSIZ 0b11 / nonce 方向・Result シグネチャ）→ Task 7 Step 5 / Task 6 Step 3 / Task 6 Step 7。未決事項 3 点（KVS キー名・opkey 形式 / session id 割当・SessionParams / IM 定数）→ 冒頭「プロトコル事実」で確定済み。
- 型整合: `RawFabricCredentials`(kvs) → `FabricCredentials::from_raw`(fabric) → `case::establish(creds)` → `SecureSession`(session) → `read_attribute/invoke`(im 型を返す) の受け渡しはすべて Interfaces 節に明記。
- 「X.509 変換はしない」と DER TBS 再構築の関係は「Spec からの明確化」1 に記載（逸脱ではなく精密化）。
