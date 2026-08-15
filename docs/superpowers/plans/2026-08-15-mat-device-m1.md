# mat-device M1（mat で自己コミッショニング）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat commission` が成功する最小の Matter デバイス（`mat-device` crate + `matv` バイナリ）を作る。

**Architecture:** mat-controller に「initiator の対になる欠けた半分」（Spake2pVerifier・PASE コーデック・ResponderExchange・SecureSession の応答メソッド）を足し、デバイス側の状態機械（PASE/CASE responder・IM サーバ・コミッショニングサーバ・mDNS 広告）は新 crate `mat-device` に置く。`core/` は I/O レス、`net/` に tokio を隔離。実証済みコードは `test_support.rs`（PASE verifier :573–817、CASE responder :317–541）にあり、これを本実装へ昇格→test_support を共有部品利用にリファクタし既存テストを回帰ガードにする。

**Tech Stack:** Rust 2021 / tokio / RustCrypto（p256, hkdf, hmac, sha2, ccm, aes — 既存依存のみ）/ Task / cargo

**Spec:** `docs/superpowers/specs/2026-08-15-mat-device-design.md`（経緯の正本は jarvis-brain vault の `2026-08-15-matv-alexa-design.md`）

## Global Constraints

- 新規外部依存を追加しない（workspace 既存依存のみ。`[workspace.dependencies]` にあるものは利用可）
- `mat-device/src/core/**` に tokio・`std::net`・ファイル I/O を import しない（feature `net` を default とし、`--no-default-features` で core のみがコンパイルできる構成を保つ）
- 各コミット前に `task check`（`cargo fmt --check` → `cargo clippy --all-targets -- -D warnings` → `cargo test`）を通す
- コミットメッセージは日本語 conventional commits（例: `feat(mat-device): ...`）
- `mat` / `matd` の既存公開 API・既存テストを壊さない。test_support のリファクタは既存テスト（`pase_self_handshake` / `case_self_handshake`）green を維持したまま行う
- プロトコル定数・ワイヤ形状は既存実装のものを正とする（PASE: `pase.rs`、CASE: `case.rs` + `test_support.rs`、IM: `im.rs`、コミッショニング: `commissioning.rs`）。仕様書から独自に再解釈しない

---

### Task 1: mat-device crate スケルトン + non-goal 改訂 + CI

**Files:**
- Create: `crates/mat-device/Cargo.toml`, `crates/mat-device/src/lib.rs`, `crates/mat-device/src/core/mod.rs`, `crates/mat-device/src/net/mod.rs`
- Modify: `Cargo.toml`（workspace members に `"crates/mat-device"` 追加）, `ARCHITECTURE.md:50-54` と `:1148` 付近, `CLAUDE.md:44`, `.github/workflows/ci.yml`

**Interfaces:**
- Produces: 空の `mat_device` crate。feature `net`（default）。`mat_device::core` は tokio 非依存

- [ ] **Step 1: crate 作成**

`crates/mat-device/Cargo.toml`:

```toml
[package]
name = "mat-device"
version.workspace = true
edition.workspace = true
license.workspace = true

[features]
default = ["net"]
net = ["dep:tokio", "dep:mat-core"]

[dependencies]
mat-controller = { workspace = true }
mat-core = { workspace = true, optional = true }
tokio = { version = "1", features = ["net", "time", "rt", "macros", "sync"], optional = true }
tracing = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
getrandom = { workspace = true }

[dev-dependencies]
mat-controller = { workspace = true, features = ["test-responder"] }
tempfile = { workspace = true }
```

注意: `mat-controller` は tokio が非 optional なので core も間接的に tokio をリンクはする。「core 規律」は **mat-device 自身の `core/**` ソースが tokio/I-O を import しない**ことを指す（lib.rs に `#[cfg(feature = "net")] pub mod net;` を置き、`--no-default-features` ビルドで net/ が消えることを機械検査とする）。

`src/lib.rs`:

```rust
pub mod core;
#[cfg(feature = "net")]
pub mod net;
```

- [ ] **Step 2: ビルド確認**

Run: `cargo check -p mat-device && cargo check -p mat-device --no-default-features`
Expected: 両方成功

- [ ] **Step 3: non-goal 改訂**

`ARCHITECTURE.md` の non-goals（:50-54）と "Things we never do"（:1148）の該当行を改訂: 「ブリッジは separate project」→「`mat`/`matd` バイナリには混ぜない。デバイス役は sibling crate `mat-device` + 別バイナリ `matv` として同居する（2026-08-15 の spec 参照）」。`CLAUDE.md:44` の同趣旨の行も同様に改訂。

- [ ] **Step 4: CI に core 検査を追加**

`.github/workflows/ci.yml` の test ステップ群に追加: `cargo check -p mat-device --no-default-features`

- [ ] **Step 5: `task check` → コミット**

```bash
git add crates/mat-device Cargo.toml ARCHITECTURE.md CLAUDE.md .github/workflows/ci.yml
git commit -m "feat(mat-device): デバイス側 crate の骨組みと non-goal 改訂（M1 Task1）"
```

---

### Task 2: Spake2pVerifier（mat-controller/spake2p.rs）

**Files:**
- Modify: `crates/mat-controller/src/spake2p.rs`（型追加 + インラインテスト）, `crates/mat-controller/src/test_support.rs:685-699`（open-coded 検算を新型利用に置換）

**Interfaces:**
- Consumes: 既存 `derive_w0_w1` :67, `compute_verifier` :78, `pub(crate)` の `decode_point`/`build_transcript`/`split_hash`/`confirmation_keys`/`hmac32`/`random_scalar`
- Produces:

```rust
pub struct PakeVerifierShared { pub c_b: [u8; 32], pub expected_c_a: [u8; 32], pub k_e: [u8; 16] }
pub struct Spake2pVerifier { /* w0: Scalar, l: ProjectivePoint, y: Scalar */ }
impl Spake2pVerifier {
    pub fn from_passcode(passcode: u32, salt: &[u8], iterations: u32) -> Self;      // w0,w1→L=w1·P
    pub fn from_verifier_material(material: &[u8; 97]) -> Result<Self, SpakeError>; // w0(32)||L(65)
    pub fn p_b(&self) -> [u8; 65];                                                  // Y = y·P + w0·N
    pub fn finish(&self, p_a: &[u8], context: &[u8], id_p: &[u8], id_v: &[u8])
        -> Result<PakeVerifierShared, SpakeError>;  // Z=y·(X−w0·M), V=y·L → transcript → cB/期待cA/Ke
}
```

- [ ] **Step 1: failing test を書く**（`spake2p.rs` のインラインテストに追加）

```rust
#[test]
fn prover_and_verifier_agree() {
    let salt = b"SPAKE2P Key Salt";
    let (w0, w1) = derive_w0_w1(20202021, salt, 1000);
    let prover = Spake2pProver::new(w0, w1);
    let verifier = Spake2pVerifier::from_passcode(20202021, salt, 1000);
    let p_a = prover.p_a();
    let p_b = verifier.p_b();
    let ctx = [0x5A; 32];
    let vs = verifier.finish(&p_a, &ctx, b"", b"").unwrap();
    let ps = prover.finish(&p_b, &ctx, b"", b"").unwrap();
    assert_eq!(ps.c_a, vs.expected_c_a);
    assert_eq!(ps.expected_c_b, vs.c_b);
    assert_eq!(ps.k_e, vs.k_e);
}

#[test]
fn verifier_material_roundtrip() {
    let salt = b"SPAKE2P Key Salt";
    let material = compute_verifier(20202021, salt, 1000);
    let v1 = Spake2pVerifier::from_passcode(20202021, salt, 1000);
    let v2 = Spake2pVerifier::from_verifier_material(&material).unwrap();
    // 同じ乱数 y を注入できないため、p_b 同士ではなく w0/L の一致で検証する
    // （実装では from_* の内部片を比較できる #[cfg(test)] アクセサを設ける）
}
```

- [ ] **Step 2: 失敗確認** Run: `cargo test -p mat-controller spake2p` → コンパイルエラー（型未定義）
- [ ] **Step 3: 実装** — 数式は `test_support.rs:685-699` を移植（`Y = y·P + w0·N` / `Z = y·(X − w0·M)` / `V = y·L` → `build_transcript` → `split_hash` → `confirmation_keys` → `c_b = hmac32(kc_b, p_a)`、`expected_c_a = hmac32(kc_a, p_b)`）。乱数 y は `random_scalar()`。
- [ ] **Step 4: green 確認** Run: `cargo test -p mat-controller spake2p`
- [ ] **Step 5: test_support を新型に置換** — `pase_responder_task` の :685-699 を `Spake2pVerifier` 利用に書き換え。Run: `cargo test -p mat-controller --features test-responder --test pase_self_handshake` → PASS（回帰ガード）
- [ ] **Step 6: `task check` → コミット** `feat(spake2p): SPAKE2+ verifier 役を追加し test responder を移行`

---

### Task 3: PASE コーデックの欠けた 5 半分 + context 関数（mat-controller/pase.rs）

**Files:**
- Modify: `crates/mat-controller/src/pase.rs`, `crates/mat-controller/src/test_support.rs:573-680`（手書きコーデックを新関数利用に置換）

**Interfaces:**
- Produces（すべて `pub`）:

```rust
pub const OPCODE_PASE_PAKE1: u8 = 0x22;  // pub(crate) → pub（PAKE2/3 も同様）
pub struct PbkdfParamRequest { pub initiator_random: [u8; 32], pub initiator_session_id: u16, pub has_pbkdf_parameters: bool }
pub fn decode_pbkdf_param_request(payload: &[u8]) -> Result<PbkdfParamRequest, PaseError>;
pub fn encode_pbkdf_param_response(initiator_random: &[u8; 32], responder_random: &[u8; 32],
    responder_session_id: u16, iterations: u32, salt: &[u8]) -> Vec<u8>;
pub fn decode_pake1(payload: &[u8]) -> Result<[u8; 65], PaseError>;
pub fn encode_pake2(p_b: &[u8; 65], c_b: &[u8; 32]) -> Vec<u8>;
pub fn decode_pake3(payload: &[u8]) -> Result<[u8; 32], PaseError>;
pub fn pake_context(request_bytes: &[u8], response_bytes: &[u8]) -> [u8; 32]; // SHA256("CHIP PAKE V1 Commissioning"||req||resp)
```

ワイヤ形状は既存コメント（pase.rs）を正とする: PBKDFParamRequest `{1:initiatorRandom[32], 2:initiatorSessionId, 3:passcodeId=0, 4:hasPBKDFParameters}` / Response `{1:initiatorRandom, 2:responderRandom, 3:responderSessionId, 4:{1:iterations, 2:salt}}` / Pake1 `{1:pA}` / Pake2 `{1:pB, 2:cB}` / Pake3 `{1:cA}`。

- [ ] **Step 1: failing test** — 既存の encode/decode と突き合わせるラウンドトリップ:

```rust
#[test]
fn pbkdf_request_roundtrip() {
    let bytes = encode_pbkdf_param_request(&[7u8; 32], 0x1234);
    let req = decode_pbkdf_param_request(&bytes).unwrap();
    assert_eq!(req.initiator_random, [7u8; 32]);
    assert_eq!(req.initiator_session_id, 0x1234);
    assert!(!req.has_pbkdf_parameters);
}

#[test]
fn pbkdf_response_roundtrip() {
    let bytes = encode_pbkdf_param_response(&[1u8; 32], &[2u8; 32], 0xB0B1, 1000, b"SPAKE2P Key Salt");
    let resp = decode_pbkdf_param_response(&bytes).unwrap(); // 既存 decoder
    assert_eq!(resp.responder_session_id, 0xB0B1);
    assert_eq!(resp.iterations, 1000);
}

#[test]
fn pake_message_roundtrips() {
    assert_eq!(decode_pake1(&encode_pake1(&[3u8; 65])).unwrap(), [3u8; 65]);
    let p2 = encode_pake2(&[4u8; 65], &[5u8; 32]);
    assert_eq!(decode_pake2(&p2).unwrap(), ([4u8; 65], [5u8; 32]));
    assert_eq!(decode_pake3(&encode_pake3(&[6u8; 32])).unwrap(), [6u8; 32]);
}
```

- [ ] **Step 2: 失敗確認** Run: `cargo test -p mat-controller pase::` → コンパイルエラー
- [ ] **Step 3: 実装** — 既存 encoder/decoder と同じ TLV 構築様式（`Writer`/`Reader`、匿名 struct + context tag）。`validate_pbkdf_params` 相当の境界チェック（iterations 1000..=100_000, salt 16..=32B）を decode 側に入れる。`PbkdfParamResponse` 型と `decode_pbkdf_param_response` は `pub` に昇格。`establish` :363-367 のインライン context 計算を `pake_context` 呼び出しに置換。
- [ ] **Step 4: green 確認** Run: `cargo test -p mat-controller pase`
- [ ] **Step 5: test_support 置換** — `pase_responder_task` の手書き encode/decode を新関数に置換。Run: `cargo test -p mat-controller --features test-responder --test pase_self_handshake` → PASS
- [ ] **Step 6: `task check` → コミット** `feat(pase): responder 向けコーデック半分と pake_context を公開`

---

### Task 4: ResponderExchange（mat-controller/exchange.rs）

**Files:**
- Modify: `crates/mat-controller/src/exchange.rs`

**Interfaces:**
- Consumes: `Transport`, `MessageHeader`/`ProtocolHeader`, `TxCounter`/`RxWindow`, `MrpConfig`, `IncomingMessage`
- Produces:

```rust
/// peer が開始した unsecured exchange の応答側。PASE/CASE の全フローを 1 exchange で捌く。
pub struct ResponderExchange<'t> { /* transport, peer, exchange_id, tx_counter, rx_window, last_peer_counter */ }
impl<'t> ResponderExchange<'t> {
    /// 受信済みの最初の peer-initiated メッセージから採番を引き継いで作る
    pub fn adopt(transport: &'t Transport, peer: SocketAddr, first: &IncomingMessage) -> Self;
    pub fn first_needs_ack(&self) -> Option<u32>;
    /// initiator:false で応答し、同一 exchange の次の peer メッセージを待つ（MRP 再送・ack piggyback 込み）
    pub async fn reply_reliable(&mut self, protocol_id: u16, opcode: u8, payload: &[u8], cfg: &MrpConfig)
        -> Result<Option<IncomingMessage>, ExchangeError>;
    /// 応答して待たない（StatusReport 終端用）。needs_ack を立て、ack 受信までは再送する
    pub async fn reply_final(&mut self, protocol_id: u16, opcode: u8, payload: &[u8], cfg: &MrpConfig)
        -> Result<(), ExchangeError>;
}
```

screen 規則は `UnsecuredExchange::screen`（:214）の**鏡像**: `proto.initiator == false` を捨て、exchange_id 不一致を捨て、重複 counter（RxWindow）で standalone-ack を返す。

- [ ] **Step 1: failing test** — 同一プロセス内で `UnsecuredExchange`（initiator）と `ResponderExchange` を `ReliableChannel::pair()` の両端に置き、request→reply→next→final の往復を検証:

```rust
#[tokio::test]
async fn responder_exchange_round_trips() {
    let (a, b) = ReliableChannel::pair();
    let cfg = MrpConfig::default();
    let init = tokio::spawn(async move {
        let mut ex = UnsecuredExchange::new(&a, RELIABLE_PEER);
        let reply = ex.send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x20, b"req1", &cfg).await.unwrap().unwrap();
        assert_eq!(reply.proto.opcode, 0x21);
        let fin = ex.send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x22, b"req2", &cfg).await.unwrap().unwrap();
        assert_eq!(fin.proto.opcode, OPCODE_STATUS_REPORT);
    });
    // responder 側: 最初のメッセージを recv → adopt → reply_reliable → reply_final
    let first = recv_first_unsecured(&b).await; // テスト内ヘルパ: MessageHeader::decode + ProtocolHeader::decode
    let mut re = ResponderExchange::adopt(&b, RELIABLE_PEER, &first);
    let next = re.reply_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x21, b"resp1", &cfg).await.unwrap().unwrap();
    assert_eq!(next.payload, b"req2");
    re.reply_final(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_STATUS_REPORT, &[0u8; 8], &cfg).await.unwrap();
    init.await.unwrap();
}
```

- [ ] **Step 2: 失敗確認** Run: `cargo test -p mat-controller exchange::` → コンパイルエラー
- [ ] **Step 3: 実装** — フレーム構築は `test_support::build_unsecured`（:143、initiator:false の実証済みフレーマ）を一般化して取り込む。ack piggyback（直前 peer counter を `acked_counter` に載せる）と standalone-ack 送出（OPCODE_MRP_STANDALONE_ACK 0x10）を実装。
- [ ] **Step 4: green 確認** Run: `cargo test -p mat-controller exchange`
- [ ] **Step 5: `task check` → コミット** `feat(exchange): peer-initiated exchange の応答側 ResponderExchange を追加`

---

### Task 5: SecureSession のデバイス役サポート（mat-controller/session.rs）

**Files:**
- Modify: `crates/mat-controller/src/session.rs`

**Interfaces:**
- Produces:

```rust
impl SecureSession {
    /// デバイス役の構築: keys の i2r/r2i を入れ替え、node id も入れ替えて new() する薄い糖衣
    pub fn new_device_role(transport: Arc<Transport>, peer: SocketAddr,
        local_session_id: u16, peer_session_id: u16, keys: SessionKeys,
        local_node_id: u64, peer_node_id: u64) -> Self;
    /// peer-initiated のリクエストを 1 件受ける（ack 送出・重複排除込み）
    pub async fn recv_request(&mut self, timeout: Duration) -> Result<IncomingMessage, SessionError>;
    /// 受けた exchange に initiator:false で応答する（respond_status :968 の一般形）
    pub async fn reply_reliable(&mut self, request: &IncomingMessage,
        protocol_id: u16, opcode: u8, payload: &[u8], cfg: &MrpConfig)
        -> Result<Option<IncomingMessage>, SessionError>;
}
```

実装メモ: `seal()` は `self.keys.i2r` で封じ `peer_session_id` をヘッダに書く（:205-249）。デバイス役では「i2r スロットに実際の r2i 鍵、peer_session_id スロットに initiator のセッション id」を入れて構築すれば送信は正しくなり、受信 screen（r2i スロット = 実際の i2r 鍵で open）も正しくなる。`new_device_role` はこの入れ替えを一箇所に閉じ込めるためにある。`reply_reliable` は `respond_status` の initiator:false + acked_counter 埋めをテンプレートに一般化する。

- [ ] **Step 1: failing test** — `ReliableChannel::pair()` の両端にコントローラ役（既存 `SecureSession::new`）とデバイス役（`new_device_role`）を同一 `SessionKeys` から構築し、コントローラの `invoke` をデバイス側 `recv_request` で受けて `reply_reliable` で InvokeResponse を返し、`invoke` が Ok になることを検証:

```rust
#[tokio::test]
async fn device_role_session_serves_invoke() {
    let (ta, tb) = ReliableChannel::pair();
    let keys = SessionKeys { i2r: [1; 16], r2i: [2; 16], attestation_challenge: [0; 16] };
    let cfg = MrpConfig::default();
    let mut ctrl = SecureSession::new(Arc::new(ta), RELIABLE_PEER, 10, 20, keys.clone(), 111, 222);
    let mut dev = SecureSession::new_device_role(Arc::new(tb), RELIABLE_PEER, 20, 10, keys, 222, 111);
    let server = tokio::spawn(async move {
        let req = dev.recv_request(Duration::from_secs(5)).await.unwrap();
        assert_eq!(req.proto.opcode, im::OPCODE_INVOKE_REQUEST);
        let resp = im::encode_invoke_response_status(1, 0x0006, 1, 0, None); // Task 7 で作る encoder。ここでは仮に手組み TLV でも可
        dev.reply_reliable(&req, im::PROTOCOL_ID_IM, im::OPCODE_INVOKE_RESPONSE, &resp, &cfg).await.unwrap();
    });
    let out = ctrl.invoke(1, 0x0006, 1, &[], &cfg).await.unwrap();
    assert_eq!(out.status, 0);
    server.await.unwrap();
}
```

（このテストの InvokeResponse ペイロードは Task 7 完了までは `test_support` の ReportData 手組みと同様に手組み TLV で書く。Task 7 完了後に encoder 利用へ差し替える）

- [ ] **Step 2: 失敗確認** Run: `cargo test -p mat-controller session::` → コンパイルエラー
- [ ] **Step 3: 実装**（上記メモどおり）
- [ ] **Step 4: green 確認** Run: `cargo test -p mat-controller session`
- [ ] **Step 5: `task check` → コミット** `feat(session): SecureSession にデバイス役の構築と応答メソッドを追加`

---

### Task 6: PASE responder 状態機械（mat-device/core/pase.rs + net 統合テスト）

**Files:**
- Create: `crates/mat-device/src/core/pase.rs`, `crates/mat-device/tests/pase_establish.rs`

**Interfaces:**
- Consumes: Task 2 `Spake2pVerifier`, Task 3 コーデック群, `mat_controller::session::SessionKeys`
- Produces（core、I/O レス。1 メッセージ in → 1 応答 out の純状態機械）:

```rust
pub struct PaseVerifierConfig { pub passcode: u32, pub salt: Vec<u8>, pub iterations: u32, pub responder_session_id: u16 }
pub enum PaseOutput { Reply(Vec<u8>, u8 /*opcode*/), Established { reply: Vec<u8>, opcode: u8, keys: SessionKeys, peer_session_id: u16 } }
pub struct PaseResponderCore { /* config, state enum, context, verifier, request/response bytes */ }
impl PaseResponderCore {
    pub fn new(config: PaseVerifierConfig) -> Self;
    /// opcode + payload を食わせて応答を得る。プロトコル違反は Err（呼び出し側が StatusReport failure を送る）
    pub fn on_message(&mut self, opcode: u8, payload: &[u8]) -> Result<PaseOutput, PaseCoreError>;
}
```

状態遷移: `Idle --PBKDFParamRequest--> AwaitPake1 --Pake1--> AwaitPake3 --Pake3--> Established`。Pake3 成功時の応答は StatusReport success（8 zero bytes、`OPCODE_STATUS_REPORT`）。鍵導出は `HKDF48(k_e, salt=[], b"SessionKeys")`（test_support :573-817 の実証フローを移植）。

- [ ] **Step 1: failing unit test**（core 単体、I/O なし）— initiator 側も既存 pub コーデックで手動駆動:

```rust
#[test]
fn pase_core_full_handshake() {
    let mut core = PaseResponderCore::new(PaseVerifierConfig {
        passcode: 20202021, salt: b"SPAKE2P Key Salt".to_vec(), iterations: 1000, responder_session_id: 0xB0B1 });
    let req = pase::encode_pbkdf_param_request(&[9u8; 32], 0x0011);
    let PaseOutput::Reply(resp_bytes, op) = core.on_message(pase::OPCODE_PBKDF_PARAM_REQUEST, &req).unwrap() else { panic!() };
    assert_eq!(op, pase::OPCODE_PBKDF_PARAM_RESPONSE);
    // prover 側を Spake2pProver + pake_context で進め、Pake1→Pake2→Pake3 を検証し、
    // Established の keys が prover 側導出と一致することを assert する
}
```

- [ ] **Step 2: 失敗確認** Run: `cargo test -p mat-device pase` → コンパイルエラー
- [ ] **Step 3: 実装**（core）
- [ ] **Step 4: green 確認**
- [ ] **Step 5: 統合テスト（net）** — `crates/mat-device/tests/pase_establish.rs`: 実 UDP loopback で `ResponderExchange` + `PaseResponderCore` を回すタスクを立て、**production の `mat_controller::pase::establish`** で接続して Ok を assert（`pase_self_handshake.rs` の production 版ミラー）:

```rust
#[tokio::test]
async fn mat_establish_against_mat_device_core() {
    let transport = UdpTransport::bind_addr("[::1]:0".parse().unwrap()).await.unwrap();
    let addr = transport.local_addr().unwrap();
    let dev = tokio::spawn(run_pase_once(transport, 20202021)); // net 側ヘルパ: recv→adopt→core 駆動
    let t2 = Arc::new(Transport::Udp(Arc::new(UdpTransport::bind().await.unwrap())));
    let session = pase::establish(t2, addr, 20202021, &fast_cfg()).await.unwrap();
    assert_eq!(session.peer_node_id(), 0); // PASE は未認証: 両側 node id 0
    dev.await.unwrap();
}
```

- [ ] **Step 6: green 確認** Run: `cargo test -p mat-device --test pase_establish`
- [ ] **Step 7: `task check` → コミット** `feat(mat-device): PASE responder 状態機械（mat の establish で検証）`

---

### Task 7: IM サーバ（mat-device/core/im.rs + データモデル骨格）

**Files:**
- Create: `crates/mat-device/src/core/im.rs`, `crates/mat-device/src/core/datamodel.rs`
- Modify: `crates/mat-controller/src/im.rs`（サーバ向け encoder/decoder を追加 — ワイヤ知識は im.rs に集約する方針）

**Interfaces:**
- Produces（mat-controller/im.rs に追加、すべて `pub`）:

```rust
pub struct InvokeRequestIn { pub endpoint: u16, pub cluster: u32, pub command: u32, pub fields_tlv: Vec<u8>, pub suppress_response: bool, pub timed: bool }
pub fn decode_invoke_request(payload: &[u8]) -> Result<InvokeRequestIn, ImError>;
pub struct AttrPathIn { pub endpoint: Option<u16>, pub cluster: Option<u32>, pub attribute: Option<u32> }
pub fn decode_read_request(payload: &[u8]) -> Result<Vec<AttrPathIn>, ImError>;
pub fn encode_invoke_response_status(endpoint: u16, cluster: u32, command: u32, status: u8, cluster_status: Option<u8>) -> Vec<u8>;
pub fn encode_invoke_response_data(endpoint: u16, cluster: u32, response_command: u32, fields_tlv: &[u8]) -> Vec<u8>;
pub struct AttrReportOut { pub endpoint: u16, pub cluster: u32, pub attribute: u32, pub data_version: u32, pub value_tlv: Vec<u8> }
pub fn encode_report_data(reports: &[AttrReportOut], suppress_response: bool) -> Vec<u8>;
```

（mat-device/core/datamodel.rs）:

```rust
pub trait ClusterHandler {
    fn cluster_id(&self) -> u32;
    fn read(&self, attribute: u32) -> Option<Vec<u8>>;               // TLV 値（Anonymous 要素 1 個）
    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply;
}
pub enum InvokeReply { Status(u8), Data { response_command: u32, fields_tlv: Vec<u8> } }
pub struct Node { /* endpoints: Vec<(u16, Vec<Box<dyn ClusterHandler>>)> */ }
impl Node {
    pub fn handle_im(&mut self, opcode: u8, payload: &[u8], ctx: &mut InvokeCtx) -> Result<(u8, Vec<u8>), ImServerError>; // (応答opcode, payload)
}
```

エンドポイント 0 の初期クラスタ: Descriptor（DeviceTypeList=RootNode(0x0016), ServerList, PartsList）と BasicInformation（DataModelRevision, VendorID, ProductID, VendorName="mat", ProductName="matv"）。属性 ID は `mat_core::ids` の表を使う。ReportData のネスト形状は `test_support::report_data_false_suppressed`（:285-305）を正とする: `struct{1: array[struct{1: struct{0:DataVersion, 1:list{2:endpoint,3:cluster,4:attribute}, 2:Data}}], 4:SuppressResponse, 255:IM_REVISION}`。

- [ ] **Step 1: failing test（コーデック）** — 既存クライアント側と突き合わせ:

```rust
#[test]
fn invoke_request_roundtrip() {
    let payload = encode_invoke_request(1, 0x0006, 1, &[]);
    let req = decode_invoke_request(&payload).unwrap();
    assert_eq!((req.endpoint, req.cluster, req.command), (1, 0x0006, 1));
}
#[test]
fn invoke_response_status_decodes_with_client_decoder() {
    let payload = encode_invoke_response_status(1, 0x0006, 1, 0, None);
    let out = decode_invoke_response(&payload).unwrap();
    assert_eq!(out.status, 0);
}
#[test]
fn report_data_decodes_with_client_decoder() {
    let mut w = Writer::new(); w.put_bool(Tag::Anonymous, true);
    let payload = encode_report_data(&[AttrReportOut { endpoint: 0, cluster: 0x0028, attribute: 0, data_version: 1, value_tlv: w.finish() }], true);
    let msg = decode_report_data_message(&payload).unwrap();
    assert_eq!(msg.reports.len(), 1);
}
```

- [ ] **Step 2: 失敗確認** → **Step 3: 実装** → **Step 4: green** Run: `cargo test -p mat-controller im`
- [ ] **Step 5: failing test（datamodel）** — `Node` に endpoint0 を組み、`decode_read_request(encode_read_request(0, 0x0028, 0))` 経由で BasicInformation DataModelRevision の ReportData が返ること、未知クラスタ invoke が Status(0xC3 UnsupportedCluster 相当。既存 im.rs の status 定数に合わせる) になることを assert
- [ ] **Step 6: 実装 → green** Run: `cargo test -p mat-device datamodel`
- [ ] **Step 7: Task 5 のテストの手組み TLV を `encode_invoke_response_status` に差し替え**
- [ ] **Step 8: `task check` → コミット** `feat(im): サーバ側コーデックと mat-device のデータモデル骨格`

---

### Task 8: dev attestation チェーンと CSR 生成（mat-controller/x509.rs）

**Files:**
- Modify: `crates/mat-controller/src/x509.rs`（`test_support` の cert/CSR 生成を production の `devcert` として公開）

**Interfaces:**
- Produces:

```rust
pub fn generate_csr(secret: &p256::SecretKey) -> Result<Vec<u8>, X509Error>; // make_test_csr :607 の昇格
pub struct DevAttestation { pub paa_der: Vec<u8>, pub pai_der: Vec<u8>, pub dac_der: Vec<u8>, pub dac_private_key: [u8; 32] }
/// 開発用 PAA→PAI→DAC チェーンを生成する（VID/PID 拡張入り）。本物の認証は M2 以降もスコープ外
pub fn generate_dev_attestation(vid: u16, pid: u16) -> Result<DevAttestation, X509Error>;
```

- [ ] **Step 1: failing test**:

```rust
#[test]
fn dev_attestation_chain_verifies() {
    let da = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
    let dac = parse_x509(&da.dac_der).unwrap();
    let pai = parse_x509(&da.pai_der).unwrap();
    let paa = parse_x509(&da.paa_der).unwrap();
    dac.verify_signed_by(&pai).unwrap();
    pai.verify_signed_by(&paa).unwrap();
    assert_eq!(dac.vid, Some(0xFFF1));
}
#[test]
fn generated_csr_parses() {
    let secret = p256::SecretKey::random(&mut rand_core_shim()); // 既存テストの乱数取得流儀に合わせる
    let csr = generate_csr(&secret).unwrap();
    let pubkey = parse_csr(&csr).unwrap();
    assert_eq!(pubkey.len(), 65);
}
```

- [ ] **Step 2: 失敗確認** → **Step 3: 実装** — `make_test_cert_ext`（:535）を土台に CA フラグ・keyUsage・VID/PID 拡張を正しく積む。`attestation.rs::verify_device_attestation` が要求するチェーン制約（:103 以降の検査項目）を読み、それを満たす形にする。
- [ ] **Step 4: green 確認** Run: `cargo test -p mat-controller x509`
- [ ] **Step 5: 相互検証テスト** — `verify_device_attestation` に自己生成チェーン + 自己署名 elements を食わせて Ok になる統合テストを `attestation.rs` 側に追加（elements 構築は Task 9 と共有するため `pub fn encode_attestation_elements(cd: &[u8], nonce: &[u8; 32], timestamp: u32) -> Vec<u8>` と `pub fn attestation_tbs(elements: &[u8], challenge: &[u8; 16]) -> Vec<u8>` を attestation.rs に追加し、verify 側もそれを使うようリファクタ）
- [ ] **Step 6: `task check` → コミット** `feat(x509): 開発用 DAC チェーンと CSR 生成を公開`

---

### Task 9: コミッショニングサーバ（mat-device/core/commissioning.rs + fabric 永続化）

**Files:**
- Create: `crates/mat-device/src/core/commissioning.rs`, `crates/mat-device/src/core/fabric_store.rs`
- Modify: `crates/mat-controller/src/commissioning.rs`（リクエスト側 decoder / レスポンス側 encoder を追加: `decode_arm_fail_safe` / `encode_commissioning_status_response` / `decode_attestation_request` / `decode_csr_request` / `decode_add_trusted_root` / `decode_add_noc` / `encode_attestation_response` / `encode_csr_response` / `encode_noc_response` — 既存の逆方向と同じ TLV 形状・すべて既存 pub 定数を使用）

**Interfaces:**
- Produces（mat-device/core/commissioning.rs）:

```rust
pub struct CommissioningServer { /* dev_attestation: DevAttestation, fail_safe: FailSafeState, pending: PendingCommissioning, store: FabricStore */ }
impl CommissioningServer {
    pub fn new(dev: DevAttestation, store: FabricStore) -> Self;
    /// GeneralCommissioning / OperationalCredentials への invoke を処理して InvokeReply を返す（ClusterHandler として Node に登録）
}
pub struct FabricEntry { pub fabric_index: u8, pub root_tlv: Vec<u8>, pub noc_tlv: Vec<u8>, pub icac_tlv: Option<Vec<u8>>,
    pub op_private_key: [u8; 32], pub ipk_operational: [u8; 16], pub node_id: u64, pub fabric_id: u64,
    pub root_public_key: [u8; 65], pub admin_subject: u64 }
pub struct FabricStore { /* entries + 保存先パス（core は trait 経由。ファイル I/O 実装は net 側 */ }
```

コマンド処理（invoke ハンドラ、attestation_challenge はセッションから引き渡す）:

- `ArmFailSafe` → タイマー記録、`CommissioningStatusResponse{errorCode:0}`
- `SetRegulatoryConfig` → 記録のみ、成功応答
- `AttestationRequest{nonce}` → `encode_attestation_elements(dummy_cd, nonce, timestamp)` + `sign_ecdsa_p256(dac_priv, attestation_tbs(elements, challenge))` → `AttestationResponse`
- `CertificateChainRequest{type}` → DAC/PAI の DER を返す
- `CSRRequest{nonce}` → 新規 op 鍵ペア生成 → `generate_csr` → nocsr_elements`{1:csr, 2:nonce}` を DAC 鍵で署名 → `CSRResponse`
- `AddTrustedRootCertificate` → pending に保持
- `AddNOC{noc, icac?, ipk, caseAdminSubject, adminVendorId}` → `MatterCert::parse` + `verify_noc_chain` → `derive_ipk_operational(ipk_epoch, compressed_fabric_id)` → FabricEntry 構築・保存 → `NOCResponse{status:0, fabricIndex:1}`
- `CommissioningComplete` → fail-safe 解除、成功応答

- [ ] **Step 1: failing test** — **mat の既存 pub encoder/decoder を初手として使う閉ループ**（例）:

```rust
#[test]
fn add_noc_installs_fabric() {
    let mut server = test_server(); // dev attestation + tempdir store
    // mat 側の CommissioningFabric で本物の NOC を発行して食わせる
    let fabric = CommissioningFabric::generate(0x1122, 0xAA).unwrap();
    let csr_resp = drive_invoke(&mut server, CMD_CSR_REQUEST, &encode_csr_request(&[3u8; 32]));
    let (elements, _sig) = decode_csr_response(&csr_resp).unwrap();
    let (csr_der, _) = parse_nocsr_elements(&elements).unwrap();
    let device_pub = parse_csr(&csr_der).unwrap();
    let noc = fabric.issue_device_noc(&device_pub, 0x5001).unwrap();
    drive_invoke(&mut server, CMD_ADD_TRUSTED_ROOT, &encode_add_trusted_root(&fabric_rcac_tlv(&fabric)));
    let resp = drive_invoke(&mut server, CMD_ADD_NOC, &encode_add_noc(&noc, &ipk, 0xAA, 0xFFF1));
    let (status, fabric_index) = decode_noc_response(&resp).unwrap();
    assert_eq!(status, 0);
    assert_eq!(fabric_index, Some(1));
    assert_eq!(server.fabrics().len(), 1);
}
```

同様に: ArmFailSafe 応答 / AttestationRequest の応答を `verify_device_attestation` に通す test（Task 8 Step5 の関数を使用）/ fail-safe 未 arm での AddNOC 拒否 / FabricStore の save→load ラウンドトリップ（tempfile）。

- [ ] **Step 2: 失敗確認** → **Step 3: 実装** → **Step 4: green** Run: `cargo test -p mat-device commissioning`
- [ ] **Step 5: `task check` → コミット** `feat(mat-device): コミッショニングサーバと fabric 永続化`

---

### Task 10: CASE responder（mat-device/core/case.rs）

**Files:**
- Create: `crates/mat-device/src/core/case.rs`, `crates/mat-device/tests/case_establish.rs`
- Modify: `crates/mat-controller/src/case.rs`（`derive_sigma_key` / `derive_session_keys` / `encode_status_report` / `parse_status_report` を pub 昇格）, `crates/mat-controller/src/test_support.rs:317-541`（responder_task を新 core 利用に置換）

**Interfaces:**
- Consumes: Task 9 の `FabricEntry`（NOC/op key/IPK/root）, `fabric::case_destination_id`
- Produces:

```rust
pub struct CaseResponderCore { /* fabrics: Vec<FabricEntry>, state, transcript hashes, eph keys */ }
pub enum CaseOutput { Reply(Vec<u8>, u8), Established { reply: Vec<u8>, opcode: u8, keys: SessionKeys, peer_session_id: u16, peer_node_id: u64, fabric_index: u8 } }
impl CaseResponderCore {
    pub fn new(fabrics: Vec<FabricEntry>, responder_session_id: u16) -> Self;
    pub fn on_message(&mut self, opcode: u8, payload: &[u8]) -> Result<CaseOutput, CaseCoreError>;
}
```

実装は `test_support::responder_task`（:317-541）の忠実な移植 + 2 点の production 化: (1) Sigma1 の destinationId を `case_destination_id` で全 fabric に対して照合して fabric 選択（不一致は StatusReport NoSharedTrustRoots）、(2) peer NOC のチェーン検証は既存どおり `verify_noc_chain`。

- [ ] **Step 1: failing unit test** — Sigma1 を `case` の initiator 側関数（pub 昇格分＋必要なら test 専用ヘルパ）で組んで食わせ、Sigma2 が返ることを確認
- [ ] **Step 2: 失敗確認** → **Step 3: 実装** → **Step 4: green** Run: `cargo test -p mat-device case`
- [ ] **Step 5: 統合テスト** — `tests/case_establish.rs`: FabricEntry を CommissioningFabric から作り、production の CASE initiator（`mat_native` 経由でなく `case::establish` 直。可視性が pub(crate) なら `commissioning::operational_case_and_complete` の CASE 部分を pub 化する最小変更を入れる）で loopback UDP 接続 → 秘匿 IM read が通ることを assert（`case_self_handshake.rs` の production 版ミラー）
- [ ] **Step 6: test_support::responder_task を CaseResponderCore 利用に置換** Run: `cargo test -p mat-controller --features test-responder --test case_self_handshake` → PASS（回帰ガード）
- [ ] **Step 7: `task check` → コミット** `feat(mat-device): CASE responder（mat の establish で検証、test responder を移行）`

---

### Task 11: mDNS 広告（mat-device/core/mdns_records.rs + net/mdns.rs）

**Files:**
- Create: `crates/mat-device/src/core/mdns_records.rs`（RR シリアライズ、純関数）, `crates/mat-device/src/net/mdns.rs`（ソケットループ）
- Test: 各ファイルのインラインテスト + `crates/mat-device/tests/discover_live.rs`（`#[ignore]`）

**Interfaces:**
- Produces:

```rust
// core/mdns_records.rs — DNS メッセージの answer/RR エンコード（新規: 既存 dnssd は question しか組めない）
pub struct CommissionableAdvert { pub instance: String /* 64bit hex */, pub hostname: String,
    pub discriminator: u16, pub vendor_id: u16, pub product_id: u16, pub port: u16, pub addr_v6: Ipv6Addr }
pub fn encode_commissionable_response(q_name: &str, ad: &CommissionableAdvert, unicast: bool) -> Option<Vec<u8>>;
pub struct OperationalAdvert { pub compressed_fabric_id: [u8; 8], pub node_id: u64, pub hostname: String, pub port: u16, pub addr_v6: Ipv6Addr }
pub fn encode_operational_response(q_name: &str, ad: &OperationalAdvert, unicast: bool) -> Option<Vec<u8>>;
pub fn encode_unsolicited_announcement(commissionable: Option<&CommissionableAdvert>, operational: &[OperationalAdvert]) -> Vec<u8>;
// net/mdns.rs
pub struct MdnsAdvertiser { /* socket(ff02::fb join, port 5353), ads の RwLock */ }
impl MdnsAdvertiser {
    pub async fn spawn(iface_scope: u32) -> Result<Arc<Self>, io::Error>;
    pub fn set_commissionable(&self, ad: Option<CommissionableAdvert>);
    pub fn add_operational(&self, ad: OperationalAdvert);
}
```

応答対象の質問名: `_matterc._udp.local` PTR / `_L<d>._sub._matterc._udp.local` PTR / `_S<sd>._sub...` PTR / インスタンス名の SRV・TXT / hostname の AAAA。operational は `_matter._tcp.local` と `<CFID>-<NID>._matter._tcp.local`（`dnssd::operational_instance` :107 を再利用）。TXT: commissionable は `D=<disc>`, `VP=<vid>+<pid>`, `CM=1`, `SII=300`, `SAI=300`。QU ビット（class 0x8000）が立った質問には unicast 応答。

- [ ] **Step 1: failing test（core）** — golden bytes ではなく**自己パース**で検証: 小さなテスト用 RR パーサをテストモジュール内に書き（name 圧縮なしで出すので単純）、PTR→SRV→TXT→AAAA が期待値どおり入っていることを assert。さらに `dnssd` の既存 pub パーサが使える範囲（`resolve_commissionable` はソケット前提のため不可）は TXT 値の一致のみ目視形式で assert
- [ ] **Step 2: 失敗確認** → **Step 3: 実装** → **Step 4: green** Run: `cargo test -p mat-device mdns`
- [ ] **Step 5: live テスト（`#[ignore]`）** — `discover_live.rs`: `MdnsAdvertiser` を立てて実機 `mat discover`（または `dnssd::browse_commissionable`）で自分が見えることを確認。`#[ignore = "要: 実 NIC。task e2e:device:m1 で実行"]`
- [ ] **Step 6: `task check` → コミット** `feat(mat-device): mDNS 広告（commissionable/operational）`

---

### Task 12: Device ランタイム組み立て + 自己コミッショニング E2E

**Files:**
- Create: `crates/mat-device/src/net/runtime.rs`, `crates/mat-device/src/device.rs`, `crates/mat-device/tests/self_commission_live.rs`, `scripts/e2e-device-m1.sh`
- Modify: `Taskfile.yml`（`e2e:device:m1` 追加）

**Interfaces:**
- Consumes: Task 4-11 の全部品
- Produces:

```rust
pub struct DeviceConfig { pub passcode: u32, pub discriminator: u16, pub vendor_id: u16, pub product_id: u16,
    pub port: u16, pub store_dir: PathBuf }
pub struct Device { /* config, node: core::datamodel::Node, fabric store, mdns */ }
impl Device {
    pub fn new(config: DeviceConfig) -> Result<Self, DeviceError>;   // store 読込 → fabric 復元
    pub fn qr_payload(&self) -> String;                              // setup_code::encode_qr
    pub fn manual_code(&self) -> String;
    pub async fn run(self) -> Result<(), DeviceError>;               // 停止は Ctrl-C（呼び出し側）
}
```

`run()` の単一ループ: `[::]:{port}` に `UdpTransport::bind_addr` → 受信 datagram を `MessageHeader::decode` で分類 — (a) unsecured session-id 0 + PASE opcode → `ResponderExchange` + `PaseResponderCore` を駆動 → 確立後 PASE セッションを current session に登録、(b) unsecured + Sigma1 → `CaseResponderCore`、(c) secured（既知 local session id）→ `SecureSession::recv_request` 相当のデコード → `Node::handle_im` → 応答。逐次処理・同時 1 セッション（spec の設計原則）。fail-safe 期限切れで pending をロールバック。AddNOC 成功時に `MdnsAdvertiser::add_operational`、CommissioningComplete で `set_commissionable(None)`。

- [ ] **Step 1: failing test（ランタイム, loopback）** — mDNS を使わない直結テスト: Device をエフェメラルポートで起動 → `pase::establish` → `run_credential_steps` 相当を **mat の pub encoder + SecureSession::invoke** で順に叩き（ArmFailSafe → Attestation（`verify_device_attestation` で検証）→ CSR → AddTrustedRoot → AddNOC）→ その後 `case` で CASE 確立 → CommissioningComplete invoke → Ok を assert。`commission_on_network` は mDNS 前提なのでここでは使わない
- [ ] **Step 2: 失敗確認** → **Step 3: 実装** → **Step 4: green** Run: `cargo test -p mat-device --test self_commission_live -- --ignored なしの直結テスト部`
- [ ] **Step 5: live E2E（`#[ignore]`）+ スクリプト** — `scripts/e2e-device-m1.sh`: `cargo build --release` → matv（Task 13。本タスク時点では `examples/device_m1.rs` で代用）をバックグラウンド起動 → 表示された QR で `mat fabric init`（tmp store）→ `mat commission --setup-code "<QR>" --node 1 --paa-dir <dev paa dir>` → exit 0 確認 → 再度 `mat on --node 1` は M2 スコープなので**やらない**。`Taskfile.yml` に `e2e:device:m1: bash scripts/e2e-device-m1.sh` を追加。dev PAA の DER は起動時に store_dir へ書き出し、スクリプトがそれを `--paa-dir` に渡す
- [ ] **Step 6: 実行** Run: `task e2e:device:m1`
Expected: `mat commission` の stdout JSON が `"status": "success"`
- [ ] **Step 7: `task check` → コミット** `feat(mat-device): デバイスランタイム — mat による自己コミッショニング成立（M1）`

---

### Task 13: matv バイナリ

**Files:**
- Create: `crates/matv/Cargo.toml`, `crates/matv/src/main.rs`, `crates/matv/tests/cli.rs`
- Modify: `Cargo.toml`（workspace members）, `Taskfile.yml`（install に matv 追加）

**Interfaces:**
- Consumes: `mat_device::{Device, DeviceConfig}`
- Produces: バイナリ `matv`。`matv --config matv.toml`。設定 TOML:

```toml
# matv.toml（M1 の全項目）
passcode = 20202021
discriminator = 3840
vendor_id = 0xFFF1
product_id = 0x8000
port = 5540
store = "/home/user/.config/matv"
```

起動時に stdout へ JSON 1 行（mat の流儀: stdout=JSON, ログ=stderr）:

```json
{"qr_payload":"MT:...","manual_code":"...","port":5540,"store":"/home/user/.config/matv"}
```

- [ ] **Step 1: failing CLI test**（assert_cmd）:

```rust
#[test]
fn prints_setup_payload_and_stays_up() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("matv.toml");
    std::fs::write(&cfg, format!("passcode = 20202021\ndiscriminator = 3840\nvendor_id = 65521\nproduct_id = 32768\nport = 0\nstore = \"{}\"\n", dir.path().display())).unwrap();
    let mut child = Command::cargo_bin("matv").unwrap().arg("--config").arg(&cfg).spawn_with_stdout_line().unwrap();
    // 1 行目が JSON で qr_payload を含む。その後プロセスが生存していることを確認して kill
}
```

- [ ] **Step 2: 失敗確認** → **Step 3: 実装**（clap、TOML は workspace の `toml` crate、Ctrl-C ハンドラで graceful 終了）→ **Step 4: green** Run: `cargo test -p matv`
- [ ] **Step 5: e2e スクリプトを examples から matv に切替**（Task 12 Step 5 の TODO 解消）Run: `task e2e:device:m1`
- [ ] **Step 6: `task check` → コミット** `feat(matv): 仮想デバイスホストの CLI（M1: 単一ノード）`

---

## Self-Review 済み事項

- **Spec 対応**: M1 受け入れ条件（mat commission 成功 = Task 12/13、dev attestation = Task 8/9、CI core 検査 = Task 1、fabric 永続化 = Task 9/12）を全タスクにマップ済み。M2 以降（Echo/Aggregator/mando/Subscribe）は本計画のスコープ外で、M1 完了後に別計画を書く
- **型整合**: `PaseOutput`/`CaseOutput` の対称形、`FabricEntry` は Task 9 定義を Task 10/12 が消費、IM encoder 名は Task 5(仮組み)→Task 7(本実装) で差し替え手順を明記
- **既知の不確実点（実装時に確認して差し替え可）**: ① `case::establish` の可視性（pub でなければ Task 10 Step 5 で最小 pub 化）② `decode_pbkdf_param_response` の現行シグネチャ（Task 3 で pub 昇格時に合わせる）③ p256 乱数生成の既存流儀（`random_p256_secret` の pub 昇格で統一するのが第一候補）。いずれも該当タスクの実装 Step 内で解決する
