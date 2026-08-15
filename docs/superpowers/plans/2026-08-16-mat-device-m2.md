# mat-device M2: Echo 相互運用ゲート Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matv を単一 OnOff 仮想デバイスとして chip-tool → Echo 実機の二段ゲートで相互運用させる（commission・On/Off 操作・音声・Subscribe 反映・再起動後の再接続まで）。

**Architecture:** M1 の逐次ランタイム（`mat-device` core/net 分離 + `matv`）の上に、chip 系コントローラが要求するプロトコル面（CASE resumptionID・wildcard read・AttributeStatusIB・チャンク分割 ReportData・Subscribe サーバ・mDNS announce/goodbye・fail-safe ロールバック・コミッショニング窓）を足す。役割中立なワイヤコーデックは `mat-controller`（im.rs / case_responder.rs）に、デバイス側状態機械は `mat-device` に置く（M1 と同じ配置原則）。

**Tech Stack:** Rust workspace（既存依存のみ・新規外部依存なし）、tokio は `mat-device/net` のみ、chip-tool は Docker（host network）で調達。

**Spec:** `docs/superpowers/specs/2026-08-16-mat-device-m2-design.md`（入力: `2026-08-15-mat-device-design.md` の「M1 完了時の申し送り」節）

## Global Constraints

- 新規外部依存を追加しない（既存 workspace 依存 + RustCrypto 系のみ）
- `mat-device/src/core/**` は tokio・ソケット・ファイル I/O 禁止。`cargo check -p mat-device --no-default-features` が通り続けること
- 各タスク完了時に `task check`（fmt + clippy -D warnings + test）が通ること
- 既存回帰を壊さない: `cargo test -p mat-controller pase_self_handshake case_self_handshake` と `task e2e:device:m1`
- コミットメッセージは既存流儀（日本語 + conventional commits: `feat(mat-device): ...` / `fix(mat-controller): ...`）
- Echo 実機・Alexa アプリ操作を要するステップは実装者だけでは完了できない。該当ステップは「人間チェックポイント」と明記してあり、そこで止まってユーザーに依頼する

---

## Phase A — chip-tool commission を通す

### Task 1: chip-tool 調達 spike + ラッパースクリプト

**Files:**
- Create: `scripts/chip-tool.sh`
- Modify: `Taskfile.yml`（`chip-tool:` タスク追記）

**Interfaces:**
- Produces: `scripts/chip-tool.sh <chip-tool args...>` — chip-tool を Docker（host network）で実行するラッパー。後続の Task 9/13 の e2e スクリプトが呼ぶ。環境変数 `CHIP_TOOL_IMAGE`（既定は Step 1 で確定したイメージ）と `CHIP_TOOL_STORE`（`~/.chip-tool-store`、`/root/.chip` 相当にマウント）を解釈する

これは spike タスク（TDD 対象外）。目的は「WSL2 上で chip-tool が実 mDNS を引けて matv に UDP が通る」実行形態を 1 つ確定させること。

- [ ] **Step 1: 候補イメージを順に試す**

候補順: (1) `ghcr.io/project-chip/chip-cert-bins`（connectedhomeip 公式のツール同梱イメージ。タグは `latest` から）→ (2) Docker Hub の連携イメージ検索（`docker search chip-tool` と GitHub connectedhomeip リポジトリの docs/docker を確認）→ (3) どちらも駄目なら connectedhomeip をローカルビルド（最終手段。`scripts/build/build_examples.py --target linux-x64-chip-tool build` — 1 時間級なので (1)(2) を十分試してから）。

検証コマンド（イメージ候補ごと）:
```bash
docker run --rm --network host <image> chip-tool --version
docker run --rm --network host <image> chip-tool discover commissionables
```
`discover commissionables` は他の Matter デバイスが LAN にいれば何か出る。何も出なくてもエラーなく走れば socket/mDNS 権限は OK とみなし、実判定は Step 3 の実機当てで行う。

- [ ] **Step 2: ラッパースクリプトを書く**

```bash
#!/usr/bin/env bash
# chip-tool を Docker（host network）で実行するラッパー。
# ペアリング状態は $CHIP_TOOL_STORE に永続化される（コンテナ間で共有）。
set -euo pipefail
IMAGE="${CHIP_TOOL_IMAGE:-<Step1 で確定したイメージ>}"
STORE="${CHIP_TOOL_STORE:-$HOME/.chip-tool-store}"
mkdir -p "$STORE"
exec docker run --rm --network host \
  -v "$STORE":/root/.chip-tool-store \
  -v "$PWD":/workdir -w /workdir \
  "$IMAGE" chip-tool "$@" --storage-directory /root/.chip-tool-store
```
注意: `--storage-directory` の位置引数仕様はイメージの chip-tool バージョンで異なりうる。`chip-tool pairing --help` で確認し合わせる。

- [ ] **Step 3: matv に対する初回 commission 試行（失敗前提の観測）**

`task e2e:device:m1` と同じ要領で matv をローカル起動し（`scripts/e2e-device-m1.sh` の matv 起動部を参考に手動で: workdir に `matv.toml` を作り `target/release/matv --config matv.toml`、iface は `MAT_E2E_IFACE` 相当）、matv stdout の JSON から `passcode`/`discriminator` を取り、

```bash
scripts/chip-tool.sh pairing onnetwork-long 1 <passcode> <discriminator> \
  --paa-trust-store-path <matv store>/paa
```
を実行する。**この時点では失敗してよい**。chip-tool 側ログ（どのステップで、どのエラーで落ちたか）を `docs/superpowers/plans/m2-chip-tool-probe.md` にメモとして残す（Task 9 のフィックスラウンドの入力）。`--paa-trust-store-path` は PAA の DER が入った**ディレクトリ**を取る（matv は `<store>/paa/paa.der` に書く）。

- [ ] **Step 4: Taskfile に補助タスクを追記**

```yaml
  chip-tool:
    desc: "chip-tool (Docker, host network)。例: task chip-tool -- pairing --help"
    cmds:
      - bash scripts/chip-tool.sh {{.CLI_ARGS}}
```

- [ ] **Step 5: Commit**

```bash
git add scripts/chip-tool.sh Taskfile.yml docs/superpowers/plans/m2-chip-tool-probe.md
git commit -m "chore(scripts): chip-tool Docker ラッパーを追加（M2 ゲート1の審査官）"
```

---

### Task 2: OnOff エンドポイント（endpoint 1 + OnOff クラスタ + PartsList）

**Files:**
- Modify: `crates/mat-controller/src/im.rs`（`DEVICE_TYPE_ON_OFF_LIGHT` 定数追記のみ）
- Create: `crates/mat-device/src/core/onoff.rs`
- Modify: `crates/mat-device/src/core/mod.rs`（`pub mod onoff;`）
- Modify: `crates/mat-device/src/core/datamodel.rs`（endpoint 1 対応: PartsList 導出、endpoint 1 の Descriptor）
- Modify: `crates/mat-device/src/device.rs`（endpoint 1 に Descriptor + OnOff を登録）
- Test: `crates/mat-device/src/core/onoff.rs` 内 `#[cfg(test)]` + datamodel の既存テストスタイル

**Interfaces:**
- Consumes: `ClusterHandler` trait（`datamodel.rs:68`）、`im::{CLUSTER_ON_OFF, ATTR_ON_OFF, CMD_ON_OFF_ON, CMD_ON_OFF_OFF, CMD_ON_OFF_TOGGLE}`（定義済み: `im.rs:21-25`）
- Produces: `pub struct OnOffHandler`（`OnOffHandler::new() -> (Self, Arc<AtomicBool>)` — `Arc<AtomicBool>` は現在状態の共有ハンドル。Task 12 で購読 dirty 通知に使い、matv がログ出力に使う）。`DescriptorHandler` は endpoint id を持つ形になり、endpoint 0 の PartsList が「Node に登録された 0 以外の endpoint」から導出される

**設計メモ:** 状態は `Arc<AtomicBool>`（core は tokio 禁止だが `std::sync::atomic` は可）。ハンドラは On/Off/Toggle を受けて bool を更新し `InvokeReply::Status(SUCCESS)` を返す。属性 `OnOff(0x0000)` の read は bool TLV。

- [ ] **Step 1: OnOffHandler の失敗するテストを書く**

`crates/mat-device/src/core/onoff.rs` を新規作成し、末尾に:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply};
    use mat_controller::im;

    #[test]
    fn on_off_toggle_flip_state_and_read_reflects_it() {
        let (mut h, state) = OnOffHandler::new();
        assert!(!state.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            h.invoke(im::CMD_ON_OFF_ON, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert!(state.load(std::sync::atomic::Ordering::SeqCst));
        h.invoke(im::CMD_ON_OFF_TOGGLE, &[], &mut InvokeCtx::default());
        assert!(!state.load(std::sync::atomic::Ordering::SeqCst));
        // read は TLV bool
        let tlv = h.read(im::ATTR_ON_OFF).unwrap();
        let mut r = mat_controller::tlv::Reader::new(&tlv);
        assert_eq!(
            r.next().unwrap().unwrap().value,
            mat_controller::tlv::Value::Bool(false)
        );
    }

    #[test]
    fn unknown_command_is_rejected() {
        let (mut h, _state) = OnOffHandler::new();
        assert_eq!(
            h.invoke(0x7F, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
    }
}
```

- [ ] **Step 2: 失敗を確認** — `cargo test -p mat-device onoff` → FAIL（`OnOffHandler` 未定義）

- [ ] **Step 3: OnOffHandler を実装**

```rust
//! OnOff クラスタサーバ (spec §1.5, cluster 0x0006)。M2 スコープ: On/Off/
//! Toggle と OnOff 属性のみ（effect 付きコマンド・GlobalSceneControl 等は
//! スコープ外）。状態は Arc<AtomicBool> — net 側ランタイム/matv がクローン
//! を持ち、購読レポートとログに使う。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mat_controller::im;
use mat_controller::tlv::{Tag, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply};

pub struct OnOffHandler {
    state: Arc<AtomicBool>,
}

impl OnOffHandler {
    pub fn new() -> (Self, Arc<AtomicBool>) {
        let state = Arc::new(AtomicBool::new(false));
        (Self { state: Arc::clone(&state) }, state)
    }
}

impl ClusterHandler for OnOffHandler {
    fn cluster_id(&self) -> u32 { im::CLUSTER_ON_OFF }
    fn read(&self, attribute: u32) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_ON_OFF => {
                let mut w = Writer::new();
                w.put_bool(Tag::Anonymous, self.state.load(Ordering::SeqCst));
                Some(w.finish())
            }
            _ => None,
        }
    }
    fn invoke(&mut self, command: u32, _fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        let new = match command {
            im::CMD_ON_OFF_ON => true,
            im::CMD_ON_OFF_OFF => false,
            im::CMD_ON_OFF_TOGGLE => !self.state.load(Ordering::SeqCst),
            _ => return InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        };
        self.state.store(new, Ordering::SeqCst);
        let _ = ctx; // Task 12 でここに変更通知が入る
        InvokeReply::Status(im::STATUS_SUCCESS)
    }
}
```
`core/mod.rs` に `pub mod onoff;` を追加。

- [ ] **Step 4: テストが通ることを確認** — `cargo test -p mat-device onoff` → PASS

- [ ] **Step 5: endpoint 1 の Descriptor と PartsList 導出の失敗するテストを書く**

`datamodel.rs` の tests に追加。要件: (a) endpoint 0 の Descriptor `PartsList` は Node に登録された 0 以外の endpoint id の配列になる（現状は常に空: `datamodel.rs:378-383`）。(b) endpoint 1 の Descriptor `DeviceTypeList` は `DEVICE_TYPE_ON_OFF_LIGHT (0x0100)`。

```rust
#[test]
fn root_parts_list_reflects_registered_endpoints() {
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    let (onoff, _state) = crate::core::onoff::OnOffHandler::new();
    node.add_endpoint(1, vec![
        Box::new(DescriptorHandler::for_device(im::DEVICE_TYPE_ON_OFF_LIGHT)),
        Box::new(onoff),
    ]);
    let req = im::encode_read_request(0, im::CLUSTER_DESCRIPTOR, im::ATTR_PARTS_LIST);
    let (_, payload) = node
        .handle_im(im::OPCODE_READ_REQUEST, &req, &mut InvokeCtx::default())
        .unwrap();
    let msg = decode_report_data_message(&payload).unwrap();
    assert_eq!(msg.reports[0].data, Some(serde_json::json!([1])));
}

#[test]
fn endpoint1_device_type_is_on_off_light() {
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    let (onoff, _state) = crate::core::onoff::OnOffHandler::new();
    node.add_endpoint(1, vec![
        Box::new(DescriptorHandler::for_device(im::DEVICE_TYPE_ON_OFF_LIGHT)),
        Box::new(onoff),
    ]);
    let req = im::encode_read_request(1, im::CLUSTER_DESCRIPTOR, im::ATTR_DEVICE_TYPE_LIST);
    let (_, payload) = node
        .handle_im(im::OPCODE_READ_REQUEST, &req, &mut InvokeCtx::default())
        .unwrap();
    let msg = decode_report_data_message(&payload).unwrap();
    assert_eq!(
        msg.reports[0].data,
        Some(serde_json::json!([{"0": im::DEVICE_TYPE_ON_OFF_LIGHT, "1": 1}]))
    );
}
```

- [ ] **Step 6: 実装**

- `im.rs` に `pub const DEVICE_TYPE_ON_OFF_LIGHT: u32 = 0x0100;` を追加（`DEVICE_TYPE_ROOT_NODE` の隣。On/Off Light, spec §Device Library 4.1。ROOT_NODE と同じく mat_core ids 表には device type が無いため drift_guard 対象外の旨をコメント）。
- `DescriptorHandler` を `struct DescriptorHandler { device_type: u32 }` に変え、`DescriptorHandler::for_device(device_type: u32) -> Self` を追加。`with_root_endpoint` は `for_device(im::DEVICE_TYPE_ROOT_NODE)` を使う。`read(ATTR_DEVICE_TYPE_LIST)` は `self.device_type` を書く。
- `PartsList` は `ServerList` と同じ手口で `Node::resolve_read` 側でインターセプト: `cluster == CLUSTER_DESCRIPTOR && attribute == ATTR_PARTS_LIST && endpoint == 0` のとき、登録済み endpoint id（0 以外）の配列を導出する（`DescriptorHandler` は siblings を知らないため。`encode_server_list` の隣に `encode_parts_list(&self.endpoints)` を置く）。endpoint != 0 の PartsList は従来どおり空配列。
- `device.rs::Device::new` で endpoint 1 を登録:

```rust
let (onoff, onoff_state) = crate::core::onoff::OnOffHandler::new();
node.add_endpoint(1, vec![
    Box::new(crate::core::datamodel::DescriptorHandler::for_device(
        mat_controller::im::DEVICE_TYPE_ON_OFF_LIGHT,
    )),
    Box::new(onoff),
]);
```
`onoff_state` は `Device` のフィールドに保持（Task 12/16 で runtime と matv に配る。当面は `#[allow(dead_code)]` でよい）。`DescriptorHandler` を `pub` にする必要がある（device.rs から使うため）。

- [ ] **Step 7: 全テスト確認** — `cargo test -p mat-device` → PASS、`task check` → PASS

- [ ] **Step 8: 自己閉ループ確認（mat から OnOff invoke）**

`crates/mat-device/tests/` の既存 direct-drive テスト（`case_establish.rs` 等）を参考に、commission 済みセッションで `endpoint 1 / CLUSTER_ON_OFF / CMD_ON_OFF_TOGGLE` を invoke して SUCCESS が返るテストを既存 integration テストファイルの隣に追加（既存テストのセットアップ関数を再利用。新規ファイルにする場合はそのセットアップを複製せず共有ヘルパへ）。

- [ ] **Step 9: Commit**

```bash
git commit -am "feat(mat-device): OnOff クラスタと endpoint 1 を追加（M2: 単一 OnOff 仮想デバイス）"
```

---

### Task 3: CASE resumption — Sigma2 TBE resumptionID + resumption Sigma1 の fallback

**Files:**
- Modify: `crates/mat-controller/src/case_responder.rs`
- Test: 同ファイル `#[cfg(test)]`

**Interfaces:**
- Consumes: `CaseResponderCore::handle_sigma1`（`case_responder.rs:239`）、`encode_tbe`（`case_responder.rs:555`）、`parse_sigma1`（`case_responder.rs:393`）
- Produces: TBE2 に `resumptionID`（context tag 4, 16 bytes 乱数）が入る。resumption フィールド（tag 6/7）付き Sigma1 も full Sigma2 で応答される（fallback）。API 変更なし

**設計メモ（spec §4.14.2）:** TBEData2 = `{1: noc, [2: icac], 3: signature, 4: resumptionID}` — resumptionID は必須フィールドで chip のパーサが期待する（申し送り筆頭）。resumption **受理**（Sigma2Resume）はスコープ外: resumption 要求付き Sigma1 は full handshake として処理するのが仕様準拠の縮退経路で、初期化直後の実デバイスは必ずこの経路を通る。M2 では fallback を正とする（spec 決定事項）。

- [ ] **Step 1: 失敗するテストを書く（TBE2 に resumptionID）**

`case_responder.rs` の tests に追加。Sigma1→Sigma2 を回し、initiator 側の鍵材料で TBE2 を復号して tag 4 を検証する。既存テスト `rejects_peer_noc_chaining_to_our_root_with_a_different_fabric_id` が initiator 側の S2K 相当（S3K）導出を手で組んでいるのと同じ手口:

```rust
#[test]
fn sigma2_tbe_carries_a_16_byte_resumption_id() {
    let f = fabric();
    let mut core = CaseResponderCore::new(vec![f.clone()], 0xB0B1);

    let initiator_secret = random_p256_secret();
    let initiator_eph = eph_pub_bytes(&initiator_secret);
    let initiator_random = [0x42u8; 32];
    let dest_id = case_destination_id(
        &f.ipk_operational, &initiator_random, &f.root_public_key, f.fabric_id, f.node_id,
    );
    let sigma1 = encode_sigma1(&initiator_random, 0x1234, &dest_id, &initiator_eph);
    let CaseOutput::Reply(sigma2, _) = core.on_message(OPCODE_SIGMA1, &sigma1).unwrap()
    else { panic!("expected Reply") };

    // Sigma2 から responder_random(1)/eph(3)/encrypted2(4) を取り出し S2K を導出
    let mut r = Reader::new(&sigma2);
    assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
    let (mut rrand, mut reph, mut enc2) = (None, None, None);
    loop {
        let el = r.next().unwrap().expect("truncated sigma2");
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Bytes(b)) => rrand = Some(<[u8; 32]>::try_from(b).unwrap()),
            (Tag::Context(3), Value::Bytes(b)) => reph = Some(<[u8; 65]>::try_from(b).unwrap()),
            (Tag::Context(4), Value::Bytes(b)) => enc2 = Some(b.to_vec()),
            _ => {}
        }
    }
    let (rrand, reph, enc2) = (rrand.unwrap(), reph.unwrap(), enc2.unwrap());
    let shared = ecdh(&initiator_secret, &reph).unwrap();
    let sigma1_hash = sha256(&sigma1);
    let mut s2k_salt = Vec::new();
    s2k_salt.extend_from_slice(&f.ipk_operational);
    s2k_salt.extend_from_slice(&rrand);
    s2k_salt.extend_from_slice(&reph);
    s2k_salt.extend_from_slice(&sigma1_hash);
    let s2k = derive_sigma_key(&shared, &s2k_salt, INFO_S2K);
    let tbe2 = decrypt_payload(&s2k, TBE2_NONCE, b"", &enc2).unwrap();

    // TBE2 の tag 4 = 16 byte resumption id
    let mut r = Reader::new(&tbe2);
    assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
    let mut resumption = None;
    loop {
        let el = r.next().unwrap().expect("truncated tbe2");
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(4), Value::Bytes(b)) => resumption = Some(b.to_vec()),
            _ => {}
        }
    }
    assert_eq!(resumption.expect("tbe2 resumption id").len(), 16);
}
```
（`derive_sigma_key`/`decrypt_payload`/`INFO_S2K`/`TBE2_NONCE` は同モジュールの use 済み・モジュール内定数なのでテストから見える。）

- [ ] **Step 2: 失敗を確認** — `cargo test -p mat-controller sigma2_tbe_carries` → FAIL（tag 4 が無い）

- [ ] **Step 3: 実装**

`encode_tbe` に `resumption_id: Option<&[u8; 16]>` パラメータを追加し、`Some` なら `w.put_bytes(Tag::Context(4), id)` を signature の後に書く。`handle_sigma1` で:

```rust
let mut resumption_id = [0u8; 16];
getrandom::getrandom(&mut resumption_id).expect("os rng");
let tbe2 = encode_tbe(&fabric.noc_tlv, fabric.icac_tlv.as_deref(), &sig2, Some(&resumption_id));
```
`handle_sigma3` 側の `encode_tbe` 呼び出しは無い（TBE3 は initiator が作る）が、TBS 用途と混同しないこと。`parse_tbe` は変更不要（trailing フィールドは既に無視）。生成した resumption_id は M2 では保持しない（Sigma2Resume 非対応 — doc comment にその旨と spec 判断を明記）。

- [ ] **Step 4: 失敗するテストを書く（resumption Sigma1 の fallback）**

```rust
#[test]
fn resumption_sigma1_falls_back_to_full_sigma2() {
    let f = fabric();
    let mut core = CaseResponderCore::new(vec![f.clone()], 0xB0B1);
    let initiator_secret = random_p256_secret();
    let initiator_eph = eph_pub_bytes(&initiator_secret);
    let initiator_random = [0x42u8; 32];
    let dest_id = case_destination_id(
        &f.ipk_operational, &initiator_random, &f.root_public_key, f.fabric_id, f.node_id,
    );
    // encode_sigma1 相当 + resumptionID(6) + initiatorResumeMIC(7) を後置
    let sigma1 = {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &initiator_random);
        w.put_uint(Tag::Context(2), 0x1234);
        w.put_bytes(Tag::Context(3), &dest_id);
        w.put_bytes(Tag::Context(4), &initiator_eph);
        w.put_bytes(Tag::Context(6), &[0xAB; 16]); // 知らない resumptionID
        w.put_bytes(Tag::Context(7), &[0xCD; 16]); // resume MIC
        w.end_container();
        w.finish()
    };
    let CaseOutput::Reply(_, opcode) = core.on_message(OPCODE_SIGMA1, &sigma1).unwrap()
    else { panic!("expected full Sigma2 fallback") };
    assert_eq!(opcode, OPCODE_SIGMA2);
}
```

- [ ] **Step 5: 実行して結果を見る**

`parse_sigma1` は未知タグを既に無視するのでこのテストは**そのまま通る可能性が高い**。通ったら「fallback がパーサ許容に依存している」ことを `parse_sigma1` の doc comment に明文化する（`resumption fields (tag 6/7) are deliberately tolerated and ignored — full-handshake fallback per spec §4.14.2; Sigma2Resume is out of M2 scope`）。通らなければパーサを直す。

- [ ] **Step 6: 回帰確認** — `cargo test -p mat-controller` 全体 + `cargo test -p mat-device` → PASS（`case_self_handshake` は test_support 経由で同じ responder を使うため、TBE2 の形状変更が initiator の `parse_tbe` 許容内であることの回帰になる）

- [ ] **Step 7: Commit**

```bash
git commit -am "feat(mat-controller): Sigma2 TBE に resumptionID を載せ、resumption Sigma1 は full fallback を明文化（M2）"
```

---

### Task 4: GC/OC の標準属性 + FabricEntry の admin_vendor_id

**Files:**
- Modify: `crates/mat-device/src/core/fabric_store.rs`（`admin_vendor_id` フィールド）
- Modify: `crates/mat-device/src/core/commissioning.rs`（GC/OC の `read` 実装、AddNOC で vendor id 保存）
- Modify: `crates/mat-controller/src/im.rs`（属性 id 定数の追記があれば）
- Test: `commissioning.rs` / `fabric_store.rs` の `#[cfg(test)]`

**Interfaces:**
- Consumes: `Inner`（`commissioning.rs:123`）、`FabricEntry`（`fabric_store.rs:45`）、`decode_add_noc`（既存: `_admin_vendor_id` を捨てている `commissioning.rs:436`）
- Produces: GC(0x0030) の read: `Breadcrumb(0)=0u64` / `BasicCommissioningInfo(1)=struct{0:60,1:900}` / `RegulatoryConfig(2)=0` / `LocationCapability(3)=2` / `SupportsConcurrentConnection(4)=true`。OC(0x003E) の read: `NOCs(0)` / `Fabrics(1)` / `SupportedFabrics(2)=5` / `CommissionedFabrics(3)=len` / `TrustedRootCertificates(4)`。`FabricEntry.admin_vendor_id: u16`（`#[serde(default)]` で旧 fabrics.json 互換）

**設計メモ:** chip 系コントローラ（chip-tool / Echo）はコミッショニング中と直後にこれらを読む。M1 は両ハンドラとも `read → None` だった（`commissioning.rs:226-229, 248-250`）。`CurrentFabricIndex(5)` はセッションの fabric index が要るため read 経路にコンテキストが無い現状では正しく返せない — Task 5 の `ReadCtx` 導入とセットでそちらで実装する（このタスクでは実装しない）。

**Fabrics(1) の TLV**（FabricDescriptorStruct, spec §11.17.5.3。fabric-scoped struct なので各要素に `254: fabric_index`）:
```
array[ struct{ 1: root_public_key(65B), 2: admin_vendor_id, 3: fabric_id,
               4: node_id, 5: "" (label), 254: fabric_index } ]
```
**NOCs(0)**: `array[ struct{ 1: noc_tlv, 2: icac_tlv(省略可), 254: fabric_index } ]`
**TrustedRootCertificates(4)**: `array[ bytes(root_tlv) ]`
**BasicCommissioningInfo(1)**: `struct{ 0: FailSafeExpiryLengthSeconds=60, 1: MaxCumulativeFailsafeSeconds=900 }`

- [ ] **Step 1: fabric_store の失敗するテスト（admin_vendor_id の永続化互換）**

`fabric_store.rs` tests: 既存 `entry()` フィクスチャに `admin_vendor_id: 0xFFF1` を足し、`serde_json` で旧形式（フィールド無し JSON）から `Deserialize` して `admin_vendor_id == 0` になることを検証:

```rust
#[test]
fn old_fabrics_json_without_admin_vendor_id_still_loads() {
    let mut v = serde_json::to_value(entry(1)).unwrap();
    v.as_object_mut().unwrap().remove("admin_vendor_id");
    let e: FabricEntry = serde_json::from_value(v).unwrap();
    assert_eq!(e.admin_vendor_id, 0);
}
```

- [ ] **Step 2: FAIL 確認 → `FabricEntry` に `#[serde(default)] pub admin_vendor_id: u16` を追加 → PASS。**呼び出し側のコンパイルエラー（`core/case.rs` のテスト、`commissioning.rs` の `handle_add_noc`、direct-drive テスト群のフィクスチャ）を全部潰す。`handle_add_noc` は `decode_add_noc` の `_admin_vendor_id` を実値で受けて entry に入れる。

- [ ] **Step 3: GC/OC read の失敗するテストを書く**

`commissioning.rs` tests に追加（`add_noc_installs_fabric` のセットアップを流用して 1 fabric 入り状態を作るヘルパ `commissioned_server() -> CommissioningServer` を先に切り出す）:

```rust
#[test]
fn gc_serves_basic_commissioning_info() {
    let server = test_server();
    let (gc, _) = server.into_cluster_handlers();
    let tlv = gc.read(1).expect("BasicCommissioningInfo");
    // struct{0: 60, 1: 900}
    let mut r = mat_controller::tlv::Reader::new(&tlv);
    assert_eq!(r.next().unwrap().unwrap().value, mat_controller::tlv::Value::StructStart);
    // (以下 tag 0 == 60, tag 1 == 900 を読む)
}

#[test]
fn oc_fabrics_and_nocs_reflect_installed_fabric() {
    let server = commissioned_server(); // fabric_id=0x1122, node=0x5001, admin_vendor_id=0xFFF1
    let (_, oc) = server.into_cluster_handlers();
    assert!(oc.read(0).is_some()); // NOCs
    assert!(oc.read(1).is_some()); // Fabrics
    // SupportedFabrics / CommissionedFabrics はスカラなので値まで検証
    // (uint TLV を Reader で読み、5 と 1 を assert)
}
```
（TLV 構造の中身検証は `Reader` の手読みで、`case_responder.rs` テストの `decode_sigma2_session_id_and_eph` と同じ流儀。）

- [ ] **Step 4: FAIL 確認 → 実装**

`Inner` に読み取りヘルパを生やし、両アダプタの `read` から委譲:

```rust
impl Inner {
    fn read_general_commissioning(&self, attribute: u32) -> Option<Vec<u8>> { ... }
    fn read_operational_credentials(&self, attribute: u32) -> Option<Vec<u8>> { ... }
}
```
属性 id はローカル const（`ATTR_GC_BREADCRUMB: u32 = 0` 等）としてこのモジュールに定義し、各値は設計メモの TLV どおり `Writer` で組む。fail-safe 期限 60s / 累積 900s は `FailSafeState` の隣に const で置く（Task 7 のロールバックでも同じ値を参照する）。

- [ ] **Step 5: PASS 確認** — `cargo test -p mat-device commissioning fabric_store` → PASS、`task check` → PASS

- [ ] **Step 6: Commit**

```bash
git commit -am "feat(mat-device): GC/OC の標準属性を実装、AddNOC の admin vendor id を永続化（chip 系コントローラの読み取り対応）"
```

---

### Task 5: wildcard read の path 展開 + AttributeStatusIB + ReadCtx

**Files:**
- Modify: `crates/mat-controller/src/im.rs`（`ReportEntryOut` / `encode_report_data_entries` / `STATUS_INVALID_ACTION` / グローバル属性 id 定数）
- Modify: `crates/mat-device/src/core/datamodel.rs`（`ClusterHandler::attributes()` / `ReadCtx` / 展開ロジック）
- Modify: `crates/mat-device/src/core/commissioning.rs`・`core/onoff.rs`（`attributes()` 実装 + `read` シグネチャ変更追随）
- Modify: `crates/mat-device/src/net/runtime.rs`（`handle_im` 呼び出しに ReadCtx を渡す）
- Test: `datamodel.rs` / `im.rs` の `#[cfg(test)]`

**Interfaces:**
- Consumes: `AttrPathIn`（`im.rs:822`、`None`=wildcard）、`decode_read_request`、既存 `encode_report_data`
- Produces:
  - `im.rs`: `pub enum ReportEntryOut { Data(AttrReportOut), Status { endpoint: u16, cluster: u32, attribute: u32, status: u8 } }`、`pub fn encode_report_data_entries(entries: &[ReportEntryOut], suppress_response: bool, subscription_id: Option<u32>, more_chunks: bool) -> Vec<u8>`（既存 `encode_report_data` は `entries` 変換して委譲する形に置き換え）、`pub const STATUS_INVALID_ACTION: u8 = 0x80;`、グローバル属性 `pub const ATTR_GENERATED_COMMAND_LIST: u32 = 0xFFF8; ATTR_ACCEPTED_COMMAND_LIST: 0xFFF9; ATTR_ATTRIBUTE_LIST: 0xFFFB; ATTR_FEATURE_MAP: 0xFFFC; ATTR_CLUSTER_REVISION: 0xFFFD;`
  - `datamodel.rs`: `pub struct ReadCtx { pub fabric_index: u8 }`。`ClusterHandler` に `fn attributes(&self) -> Vec<u32>`（クラスタ固有属性の列挙。グローバル属性は Node が合成）と `fn read(&self, attribute: u32, ctx: &ReadCtx) -> Option<Vec<u8>>`（シグネチャ変更）。`Node::handle_im(&mut self, opcode, payload, ctx: &mut InvokeCtx, read_ctx: &ReadCtx)`。`Node::read_entries(&self, paths: &[AttrPathIn], read_ctx) -> Vec<ReportEntryOut>`（Task 6/12 も使う中核）
- Produces（挙動）: wildcard（endpoint/cluster/attribute の任意の省略）が登録済み実体へ展開される。concrete path の不在は read 全体の StatusResponse ではなく per-path `AttributeStatusIB` になる。**wildcard 展開で `read` が `None` を返した属性は黙って落とす**（列挙に無いものは展開されないので通常起きないが、防御的に）。concrete path の `None` は AttributeStatusIB(UNSUPPORTED_ATTRIBUTE)。未対応 opcode（Write 等）は無視ではなく `StatusResponse(STATUS_INVALID_ACTION)` を返す

**AttributeStatusIB の TLV**（spec §8.9.6; AttributeReportIB の tag 0 側）:
```
struct{ 0: struct{ 0: struct{2:endpoint,3:cluster,4:attribute} (Path... 実際は
       AttributeStatusIB = struct{0: AttributePathIB(list), 1: StatusIB struct{0: status}}) } }
```
正確な形: `AttributeReportIB = struct{ 0: AttributeStatusIB, ... }`、`AttributeStatusIB = struct{ 0: Path(list{2,3,4}), 1: StatusIB struct{0: status(uint)} }`。`decode_report_data_message` が data 側で使う Path の list 形式（`encode_report_data` の `w.start_list(Tag::Context(1))` 部分）と同じ list コーデックを使うこと。

**グローバル属性の合成**（Node::read_entries 内、どのクラスタでも）: `ATTR_CLUSTER_REVISION` → `uint 1`、`ATTR_FEATURE_MAP` → `uint 0`、`ATTR_ATTRIBUTE_LIST` → `attributes() + グローバル5種` の uint 配列、`ATTR_ACCEPTED_COMMAND_LIST`/`ATTR_GENERATED_COMMAND_LIST` → 空配列（OnOff の accepted は `[0,1,2]` を `OnOffHandler::attributes` とは別に返したいが、コマンド列挙 API は M2 では作らない — 空配列で chip は許容する。Echo で問題が出たら Task 9/13 のフィックスラウンドで拾う）。wildcard 展開時はグローバル属性を**列挙に含めない**（chip の wildcard read が巨大化するため。concrete 指定されたときだけ答える）。…ただし chip は wildcard で AttributeList を見て整合検査することがある。まず含めない実装にし、ゲートで問題が出たら含める（判断ログを残す）。

- [ ] **Step 1: 失敗するテストを書く（展開と status IB）**

`datamodel.rs` tests:

```rust
#[test]
fn wildcard_endpoint_read_expands_to_all_endpoints() {
    let mut node = node_with_onoff(); // root + endpoint1(Descriptor+OnOff) ヘルパ
    // endpoint=None, cluster=Descriptor, attribute=DeviceTypeList
    let payload = encode_read_request_path(None, Some(im::CLUSTER_DESCRIPTOR), Some(im::ATTR_DEVICE_TYPE_LIST));
    let (op, resp) = node.handle_im(im::OPCODE_READ_REQUEST, &payload, &mut InvokeCtx::default(), &ReadCtx::default()).unwrap();
    assert_eq!(op, im::OPCODE_REPORT_DATA);
    let msg = decode_report_data_message(&resp).unwrap();
    assert_eq!(msg.reports.len(), 2); // endpoint 0 と 1
}

#[test]
fn full_wildcard_read_reports_every_attribute_without_error() {
    let mut node = node_with_onoff();
    let payload = encode_read_request_path(None, None, None);
    let (op, resp) = node.handle_im(...).unwrap();
    assert_eq!(op, im::OPCODE_REPORT_DATA);
    let msg = decode_report_data_message(&resp).unwrap();
    assert!(msg.reports.len() >= 10); // descriptor×2 + basicinfo×5 + onoff×1 + ...
}

#[test]
fn unknown_concrete_attribute_reports_status_ib_not_global_error() {
    let mut node = node_with_onoff();
    // 実在 path と不在 path の 2 本読み
    let payload = encode_read_request_paths(&[
        (Some(0), Some(im::CLUSTER_BASIC_INFORMATION), Some(im::ATTR_VENDOR_ID)),
        (Some(0), Some(im::CLUSTER_BASIC_INFORMATION), Some(0x7777)),
    ]);
    let (op, resp) = node.handle_im(...).unwrap();
    assert_eq!(op, im::OPCODE_REPORT_DATA); // StatusResponse ではない
    // decode 側: data report 1 本。status IB は decode_report_data_message が
    // 未対応なら reports に現れない — encode 側の単体テスト（im.rs 側）で
    // バイト列を Reader 手読みして status=0x86 を検証する。
}
```
`encode_read_request_path(s)` はテスト用ローカルヘルパ（`encode_read_request_cluster` の形を一般化して Writer で組む）。

- [ ] **Step 2: FAIL 確認** — 新ヘルパ・新シグネチャ未定義でコンパイルエラー → 順に型を定義して「テストが赤で走る」ところまで持っていく

- [ ] **Step 3: im.rs 側の実装**（`ReportEntryOut`・`encode_report_data_entries`・status IB エンコード・`STATUS_INVALID_ACTION`・グローバル属性定数。`encode_report_data_entries` の単体テスト: entries に Status を 1 本入れてエンコードし Reader 手読みで `0:{0:path,1:{0:status}}` を検証）

- [ ] **Step 4: datamodel.rs 側の実装**

- `ReadCtx`（`#[derive(Default)]`、`fabric_index: u8`）
- trait 変更: `attributes()` / `read(attribute, ctx)`。既存実装（Descriptor/BasicInformation/GC/OC/OnOff）に `attributes()` を実装（Descriptor: `[DEVICE_TYPE_LIST, SERVER_LIST, PARTS_LIST]`、BasicInfo: 5 種、GC: 5 種、OC: 5 種（CurrentFabricIndex 含む — read 実装もここで: `ctx.fabric_index` を uint で返す）、OnOff: `[ATTR_ON_OFF]`）
- `Node::read_entries`: paths を順に展開。endpoint wildcard → 全 endpoint、cluster wildcard → その endpoint の全 handler、attribute wildcard → `handler.attributes()`。concrete 不在 → `ReportEntryOut::Status`（endpoint 不在: UNSUPPORTED_ENDPOINT / cluster: UNSUPPORTED_CLUSTER / attribute: UNSUPPORTED_ATTRIBUTE）。ServerList/PartsList のインターセプトは従来どおりこの層。
- `handle_read` は `read_entries` + `encode_report_data_entries(entries, true, None, false)`。
- `handle_im` の未対応 opcode: `Ok((OPCODE_STATUS_RESPONSE, encode_status_response(STATUS_INVALID_ACTION)))` に変更（`ImServerError::UnsupportedOpcode` は decode 不能時のみに縮小）。
- `runtime.rs`: `serve_secured_message` で `node.handle_im(...)` に `&ReadCtx { fabric_index: /* CASE 確立時に保存した値, PASE 中は 0 */ }` を渡す。`current_session` タプルに fabric_index を足す（`net::case::run_case_once`/`drive_established` は fabric_index を返している — `runtime.rs:405` で捨てているのを保存する）。

- [ ] **Step 5: 全テスト PASS 確認** — `cargo test -p mat-device -p mat-controller` + `cargo check -p mat-device --no-default-features` + `task check`

- [ ] **Step 6: Commit**

```bash
git commit -am "feat(mat-device,mat-controller): wildcard read 展開と AttributeStatusIB、ReadCtx/グローバル属性を実装（M2）"
```

---

### Task 6: チャンク分割 ReportData（read の複数メッセージ応答）

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（チャンク分割: `Node::handle_read_chunked`）
- Modify: `crates/mat-device/src/net/runtime.rs`（チャンク送信フロー: 各チャンク送信 → StatusResponse 待ち）
- Test: `datamodel.rs` 単体 + `crates/mat-device/tests/` の閉ループ

**Interfaces:**
- Consumes: `encode_report_data_entries(entries, suppress, subscription_id, more_chunks)`（Task 5）、`SecureSession::{reply_reliable, recv}`（`session.rs:1207/568`）
- Produces: `Node` に `pub fn read_chunks(&self, paths: &[AttrPathIn], read_ctx: &ReadCtx, budget: usize) -> Vec<Vec<u8>>` — encoded ReportData ペイロードの列。非最終チャンクは `more_chunks=true, suppress_response=false`、最終は `more_chunks=false, suppress_response=true`。`budget` は 1 チャンクのエンコード済みペイロード上限（呼び出し側 const `REPORT_CHUNK_BUDGET: usize = 900` — MAX_DATAGRAM 1280 からヘッダ/MIC 余裕をみた値）

**設計メモ:** Echo/chip の full-wildcard read は OC の NOCs/TrustedRootCertificates（証明書 ~500B 級）を含むため 1 datagram に収まらない。プロトコルは priming report と同じ: 非最終チャンクに受信側が StatusResponse(0) を返し、同 exchange で次チャンクが続く（`session.rs:1329-1386` の initiator 側実装がこの形を期待している）。

- [ ] **Step 1: 分割の単体テスト（失敗する）**

```rust
#[test]
fn read_chunks_splits_when_over_budget_and_marks_more_chunks() {
    let node = node_with_onoff_and_fat_attribute(); // テスト用に 600B 値を返す DummyHandler を 3 つ登録
    let paths = [AttrPathIn { endpoint: None, cluster: None, attribute: None }];
    let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900);
    assert!(chunks.len() >= 2);
    for (i, c) in chunks.iter().enumerate() {
        let msg = im::decode_report_data_message(c).unwrap();
        let last = i == chunks.len() - 1;
        assert_eq!(msg.more_chunks, !last);
        assert_eq!(msg.suppress_response, last);
        assert!(c.len() <= 900 + 64); // 単一レポートが budget 超過の場合のみ超えうる
    }
}
```

- [ ] **Step 2: FAIL 確認 → 実装**

`read_chunks`: `read_entries` の結果を順にエンコードし、`encode_report_data_entries` で 1 本ずつ足したときの長さが budget を超えるならそこで切る（貪欲法。1 レポート単体が budget 超なら単独チャンクにする — 分割はしない）。効率より単純さ（呼び出し頻度は低い）。`handle_read` は `read_chunks` の 1 チャンク目のみ返す従来 API を維持しつつ、runtime 用に `read_chunks` を公開。

- [ ] **Step 3: runtime のチャンク送信フロー**

`serve_secured_message` 内、`OPCODE_READ_REQUEST` のときだけ `node.handle_im` を経由せず直接: `decode_read_request` → `node.read_chunks(...)` → 最終以外の各チャンクを `session.reply_reliable(msg の exchange, ...)` で送り、`session.recv(exchange_id, IM 応答タイムアウト)` で StatusResponse(0) を待ち、非 0 / タイムアウトで打ち切り。最終チャンクは従来どおり reply して終わり。1 チャンクで収まる場合は従来と同一挙動（回帰なし）。`recv` の第 2 引数は `Duration::from_secs(5)`。invoke/その他 opcode は従来どおり `handle_im`。

- [ ] **Step 4: 閉ループテスト**

`crates/mat-device/tests/` の direct-drive テストに「fat 属性入り Node に対する full-wildcard read が複数チャンクで完走する」を追加… ただし `mat` 側 `read_attribute` はチャンク非対応。**`subscribe_wildcard` の priming がチャンク対応済み**（`session.rs:1329`）なのでこの検証は Task 12 の priming テストに委ねる。ここでは代わりに手動ドライブ（テスト内で controller 役ソケットから ReadRequest を送り、ReportData → StatusResponse(0) → ReportData を生で往復する。`serve_secured_drains_and_serves_a_cross_exchange_piggybacked_request`（`runtime.rs:697`）のセットアップを流用）で 2 チャンク往復を検証する。

- [ ] **Step 5: `task check` PASS → Commit**

```bash
git commit -am "feat(mat-device): read 応答のチャンク分割（more_chunks + StatusResponse 往復）を実装"
```

---

### Task 7: fail-safe 期限切れの fabric ロールバック + next_fabric_index 修正（core）

**Files:**
- Modify: `crates/mat-device/src/core/fabric_store.rs`（`remove` / `next_fabric_index` 修正）
- Modify: `crates/mat-device/src/core/commissioning.rs`（uncommitted 追跡・ロールバック API）
- Test: 両ファイルの `#[cfg(test)]`

**Interfaces:**
- Consumes: `FailSafeState`（`commissioning.rs:90`）、`FabricStore`（`fabric_store.rs:80`）
- Produces:
  - `FabricStore::remove(&mut self, fabric_index: u8) -> Result<bool, String>`（削除して persist。無ければ `Ok(false)`。persist 失敗はメモリ上もロールバック = insert と対称）
  - `FabricStore::next_fabric_index` を `max(entries.fabric_index) + 1`（空なら 1）に修正
  - `CommissioningServer::fail_safe_deadline(&self) -> Option<std::time::Instant>`（armed 中のみ Some — runtime の select 枝が使う）
  - `CommissioningServer::expire_fail_safe(&self) -> Option<FabricEntry>`（期限超過なら: uncommitted fabric を remove して disarm し、除去した `FabricEntry` を丸ごと返す。runtime が mDNS goodbye（compressed_fabric_id/node_id が要る）に使う）
  - `Inner` の挙動変更: `handle_add_noc` 成功時に `uncommitted_fabric_index = Some(index)` を記録。`handle_commissioning_complete` でクリア（コミット確定）。`handle_arm_fail_safe`（再アーム・早期ディスアーム両方）でも uncommitted があれば **remove してから**従来処理（前回試行のゾンビを持ち越さない）

**設計メモ:** spec §11.10.7.2 — fail-safe 期限切れ時、fail-safe 中の fabric 変更は巻き戻す。現状は `armed_until` を過ぎても AddNOC 済み fabric が残り、リトライごとに増える（申し送り 3 項）。期限**検知**は 2 経路: (a) 次のコマンド処理時（Inner 内 lazy）、(b) runtime の期限タイマー（Task 8 で配線 — mDNS の operational 広告も下げるため）。core 側はどちらからも呼べる `expire_fail_safe` に寄せる。

- [ ] **Step 1: 失敗するテストを書く**

```rust
// fabric_store.rs
#[test]
fn next_fabric_index_skips_removed_indices() {
    let mut store = FabricStore::new();
    store.insert(entry(1)).unwrap();
    store.insert(entry(2)).unwrap();
    assert!(store.remove(1).unwrap());
    assert_eq!(store.next_fabric_index(), 3); // len+1 だと 2 で衝突していた
}

// commissioning.rs
#[test]
fn fail_safe_expiry_rolls_back_uncommitted_fabric() {
    let mut server = test_server();
    // ArmFailSafe(1秒) → CSR → AddTrustedRoot → AddNOC（add_noc_installs_fabric と同じ流れ、expiry=1）
    ...
    assert_eq!(server.fabrics().len(), 1);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(server.expire_fail_safe().map(|e| e.fabric_index), Some(1));
    assert!(server.fabrics().is_empty());
    assert!(server.expire_fail_safe().is_none()); // 冪等
}

#[test]
fn commissioning_complete_commits_the_fabric() {
    // 同上の流れ + CommissioningComplete → sleep → expire_fail_safe は None、fabric は残る
}

#[test]
fn rearm_rolls_back_previous_attempts_uncommitted_fabric() {
    // AddNOC まで済ませて CommissioningComplete せずに ArmFailSafe(120) を再送 →
    // fabrics() が空に戻り、next_fabric_index が新しい試行で再利用可能
}
```
（sleep 依存が嫌なら `FailSafeState` に `#[cfg(test)] fn force_expire(&mut self)` を足して差し替える — 1 秒 sleep 1 本なら許容。実装者の判断でよいが、テスト総時間を伸ばしすぎない。）

- [ ] **Step 2: FAIL 確認 → 実装**（上記 Interfaces のとおり。`expire_fail_safe` は「armed_until が Some かつ過ぎている」ときだけ発火し、`uncommitted_fabric_index.take()` → `store.remove` → `fail_safe.disarm()`。ロールバックの remove は insert と**非対称**にする: persist の save が失敗してもメモリからは必ず消す — ゾンビ fabric が CASE に使われ続ける方が「再起動で復活しうる」より害が大きい。doc comment にこの判断を明記）

- [ ] **Step 3: PASS 確認 + 既存テスト回帰**（`disarm_clears_pending_commissioning_state` 等が新ロールバックで壊れないこと）

- [ ] **Step 4: Commit**

```bash
git commit -am "fix(mat-device): fail-safe 期限切れ/再アームで未コミット fabric をロールバック、fabric index 再利用を修正"
```

---

### Task 8: mDNS unsolicited announce / goodbye + runtime の fail-safe 期限タイマー

**Files:**
- Modify: `crates/mat-device/src/core/mdns_records.rs`（goodbye エンコード）
- Modify: `crates/mat-device/src/net/mdns.rs`（announce 送信・`remove_operational`）
- Modify: `crates/mat-device/src/net/runtime.rs`（fail-safe 期限の select 枝）
- Test: `mdns_records.rs` 単体 + `net/mdns.rs` の live テストがあれば追随

**Interfaces:**
- Consumes: `encode_unsolicited_announcement`（実装・テスト済み: `mdns_records.rs:348`。net ループが未接続なだけ）、`CommissioningServer::{fail_safe_deadline, expire_fail_safe}`（Task 7）
- Produces:
  - `mdns_records.rs`: `pub fn encode_goodbye(commissionable: Option<&CommissionableAdvert>, operational: &[OperationalAdvert]) -> Vec<u8>`（announcement と同じレコード集合を TTL=0 で。実装は `encode_unsolicited_announcement` に `ttl_override: Option<u32>` を足した内部関数へ両者を委譲）
  - `net/mdns.rs`: `MdnsAdvertiser::announce(&self)`（現在の advert 集合の announcement を送信し、RFC 6762 §8.3 どおり ~1s 後にもう一度送る — 2 回目は `tokio::spawn`）。`set_commissionable`/`add_operational` を `async fn` 化し、変更後に `announce()`。`set_commissionable(None)` は先に旧 commissionable の goodbye を送る。`pub async fn remove_operational(&self, compressed_fabric_id: u64, node_id: u64)`（該当 advert の goodbye を送って除去）。`spawn` 直後（bring_up_mdns 完了時）にも announce
  - `runtime.rs`: select に第 3 の枝を追加 — `fail_safe_expiry_deadline(&comm_server)`（`fail_safe_deadline()` を `tokio::time::sleep_until` に写像、None なら `std::future::pending`。`mdns_retry_deadline` と同型）。発火時 `expire_fail_safe()` が `Some(index)` を返したら、そのとき除去された fabric の operational advert を `remove_operational` で下げる（`fabrics()` に無くなった entry の特定: expire 前に `fabrics()` のスナップショットは取れないので、`expire_fail_safe` の戻り値を `Option<FabricEntry>`（index でなく entry ごと）に変えて cfid/node_id を得る — Task 7 の Interfaces をこの形で実装しておくこと）


- [ ] **Step 1: goodbye エンコードの失敗するテスト**（`mdns_records.rs` tests: 既存の announcement テストを複製し、全レコードの TTL が 0 であることを Reader/バイト検査で確認 — 既存テストが TTL をどう検証しているかに合わせる）

- [ ] **Step 2: FAIL → `encode_goodbye` 実装 → PASS**

- [ ] **Step 3: advertiser の announce 配線**（機械的変更。`bring_up_mdns`・`serve_secured_message` の AddNOC 検知部・CommissioningComplete 検知部の呼び出し側を async 化に追随させる。コンパイルが通り既存テストが回ることが検証 — マルチキャスト実送信の自動検証は live テスト環境依存なので、`encode_goodbye`/`encode_unsolicited_announcement` の単体テストと Task 9 の実機ゲートで確認する）

- [ ] **Step 4: runtime の fail-safe 期限枝**（`mdns_retry_deadline` と同じパターン。単体テストは `fail_safe_deadline` の Some/None 写像のみ、消化そのものは Task 7 の core テスト + Task 9 実機で確認）

- [ ] **Step 5: `task check` + `task e2e:device:m1` PASS 確認**（M1 ゲートがこの変更後も生きていること — announce 化で mat 側 discovery が壊れていない証明）

- [ ] **Step 6: Commit**

```bash
git commit -am "feat(mat-device): mDNS unsolicited announce/goodbye と fail-safe 期限の runtime 配線"
```

---

### Task 9: ゲート 1 前半 — chip-tool commission を通す（フィックスラウンド）

**Files:**
- Create: `scripts/e2e-device-m2-chip.sh`
- Modify: `Taskfile.yml`（`e2e:device:m2-chip`）
- Modify: フィックスラウンドで判明した任意のファイル

**Interfaces:**
- Consumes: Task 1 のラッパー、Task 2-8 の全成果
- Produces: `task e2e:device:m2-chip` — matv 起動 → `chip-tool pairing onnetwork-long 1 <passcode> <discriminator> --paa-trust-store-path <store>/paa` が成功 → `chip-tool onoff toggle 1 1` が成功 → `chip-tool onoff read on-off 1 1` が toggle 後の値を返す、まで自動検証するスクリプト（`scripts/e2e-device-m1.sh` の構成を踏襲: 一時 workdir・matv 起動・JSON 抽出・timeout・クリーンアップ trap）

- [ ] **Step 1: スクリプトを書く**（e2e-device-m1.sh を出発点に。chip-tool 側の成否判定は exit code + 出力の `CHIP:TOO` 成功行 grep。`pairing` 後に `onoff toggle` → `onoff read` の値検証まで）

- [ ] **Step 2: 実行 → 落ちた箇所を系統的に直す（本タスクの本体）**

期待される既知の落ち方と対処の当たり:
- attestation で落ちる → `--paa-trust-store-path` の DER 形式/ファイル名規約（chip は `dcld` 形式や特定命名を要求することがある。PAA DER のファイル名を subject key id ベースにリネームして通す等）
- CASE で落ちる → Sigma2 の中身を chip 側ログ（`--trace_decode 1` 等のオプション）で確認。Task 3 の resumptionID が効いているか
- コミッショニング途中の read で落ちる → chip-tool ログにどの cluster/attribute か出る。Task 4/5 で入れた属性の不足分をその場で追加（同スタイル・TDD で）
- NetworkCommissioning cluster 要求で落ちる → on-network デバイスでも chip が endpoint 0 に NetworkCommissioning(0x0031) の FeatureMap/読みを要求する場合がある。その場合は最小実装（FeatureMap=4 (Ethernet)、Networks=[]、InterfaceEnabled=true 相当の read-only 属性）を `core/commissioning.rs` の隣に `core/netcomm.rs` として追加

各修正は 1 コミットずつ（`fix(mat-device): chip-tool 対応 — <何が要求されたか>`）。`docs/superpowers/plans/m2-chip-tool-probe.md` に観測と対処を追記していく。

- [ ] **Step 3: 再起動再接続の検証をスクリプトに足す** — commission 成功後、matv プロセスを SIGTERM → 同じ store で再起動 → `chip-tool onoff toggle 1 1` が（resumption fallback 経由の再 CASE で）成功すること

- [ ] **Step 4: Taskfile 登録 + 3 回連続グリーン確認**（フレーク検出。`for i in 1 2 3; do task e2e:device:m2-chip; done`）

- [ ] **Step 5: Commit**

```bash
git commit -am "test(e2e): chip-tool commission/操作/再起動ゲート task e2e:device:m2-chip を追加"
```

---

### Task 10: Echo attestation 早期チェックポイント【人間チェックポイント】

**Files:**
- Modify: `Taskfile.yml`（`xbuild:arm64` に matv を追加）
- Create: `docs/superpowers/plans/m2-echo-checklist.md`（Phase D で使い回すチェックリストの初版）

**Interfaces:**
- Consumes: Task 9 まで（chip-tool commission 成功状態）
- Produces: Echo が dev attestation チェーンを受け入れるかの白黒。ここが M2 続行の判断点

- [ ] **Step 1: xbuild に matv を追加**（`Taskfile.yml` の `xbuild:arm64` タスクの cross build 対象と cp 行に `matv` を足す）

- [ ] **Step 2: jarvis へ配備**（despliegue の既存フロー: `task xbuild:arm64` → jarvis へ scp → jarvis 上で `matv --config` を手動起動。matv.toml は jarvis の実 NIC（eth0）と専用 store ディレクトリ、固定 passcode/discriminator で用意。**systemd 化は Phase D — ここでは手動起動でよい**）

- [ ] **Step 3: 【人間チェックポイント】Alexa アプリで commission を試す**

ユーザーに依頼: Alexa アプリ →「デバイスの追加」→「Matter デバイス」→ matv stdout の QR ペイロード（`MT:...`）から生成した QR を読む（QR 画像化は `qrencode -t ansiutf8 '<MT:...>'` か、matv の manual code 11 桁の手入力でも可）。観測ポイント: (a) 「認定されていないデバイス」警告が出て続行できるか、(b) attestation ステップで硬く失敗するか。matv 側 stderr ログと合わせて記録する。

- [ ] **Step 4: 結果を記録して分岐**

- **成功（commission 完走 or 警告付きで完走）**: `m2-echo-checklist.md` に結果を記録し、Phase B へ進む
- **attestation で拒否**: **実装を止める**。spec の判断（「落ちた場合は実装を止めて情報収集に切り替える」）どおり、ユーザーへ報告して対応方針（Echo のファーム世代・別経路の調査）を協議する

- [ ] **Step 5: Commit**（Taskfile + checklist 初版）

```bash
git commit -am "chore(build): matv を arm64 クロスビルド対象に追加、Echo チェックリスト初版"
```

---

## Phase B — Subscribe を通す

### Task 11: Subscribe のワイヤコーデック（im.rs サーバ側半分）

**Files:**
- Modify: `crates/mat-controller/src/im.rs`
- Test: 同ファイル `#[cfg(test)]`

**Interfaces:**
- Consumes: `encode_subscribe_request`（`im.rs:884` — これの decode 対）、`decode_subscribe_response`（`im.rs:921` — これの encode 対）、`AttrPathIn`
- Produces:
  - `pub struct SubscribeRequestIn { pub keep_subscriptions: bool, pub min_interval_floor_s: u16, pub max_interval_ceiling_s: u16, pub paths: Vec<AttrPathIn> }`
  - `pub fn decode_subscribe_request(payload: &[u8]) -> Result<SubscribeRequestIn, ImError>`（`{0:keep,1:min,2:max,3:array[AttributePathIB list],7:fabric_filtered(無視),255:rev(無視)}` — `decode_read_request` の path 読み部を関数として共有する）
  - `pub fn encode_subscribe_response(subscription_id: u32, max_interval_s: u16) -> Vec<u8>`（`{0:id, 2:max_interval, 255:IM_REVISION}`）

- [ ] **Step 1: ラウンドトリップの失敗するテスト**

```rust
#[test]
fn subscribe_request_roundtrips_through_server_decode() {
    let payload = encode_subscribe_request(2, 60, false, &[CLUSTER_ON_OFF]);
    let req = decode_subscribe_request(&payload).unwrap();
    assert!(!req.keep_subscriptions);
    assert_eq!(req.min_interval_floor_s, 2);
    assert_eq!(req.max_interval_ceiling_s, 60);
    assert_eq!(req.paths, vec![AttrPathIn { endpoint: None, cluster: Some(CLUSTER_ON_OFF), attribute: None }]);
}

#[test]
fn subscribe_response_roundtrips_through_client_decode() {
    let payload = encode_subscribe_response(0xDEADBEEF, 60);
    let sr = decode_subscribe_response(&payload).unwrap();
    assert_eq!(sr.subscription_id, 0xDEADBEEF);
    assert_eq!(sr.max_interval_s, 60);
}

#[test]
fn full_wildcard_subscribe_request_decodes_to_one_empty_path() {
    let payload = encode_subscribe_request(0, 30, true, &[]);
    let req = decode_subscribe_request(&payload).unwrap();
    assert_eq!(req.paths, vec![AttrPathIn { endpoint: None, cluster: None, attribute: None }]);
}
```

- [ ] **Step 2: FAIL 確認 → 実装 → PASS**（`decode_read_request` の `AttributeRequests` 配列読みをプライベート関数 `decode_attribute_requests(&mut Reader) -> Result<Vec<AttrPathIn>, ImError>` に括り出して両者から使う）

- [ ] **Step 3: Commit**

```bash
git commit -am "feat(mat-controller): SubscribeRequest decode / SubscribeResponse encode（IM サーバ側半分）"
```

---

### Task 12: Subscribe サーバ本体（priming・dirty レポート・keep-alive）

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（`InvokeCtx.changed` + データバージョン管理）
- Create: `crates/mat-device/src/net/subscription.rs`
- Modify: `crates/mat-device/src/net/mod.rs`・`net/runtime.rs`
- Test: `crates/mat-device/tests/subscribe_loop.rs`（新規・閉ループ）

**Interfaces:**
- Consumes: Task 11 のコーデック、`Node::read_chunks`（Task 6）、`SecureSession::{send_reliable, reply_reliable, recv, new_exchange_id}`、initiator 側の検証装置 `SecureSession::{subscribe_wildcard, next_subscription_report}`（`session.rs:1293/1412` — テストの審査官）
- Produces:
  - `InvokeCtx` に `pub changed: Vec<u32>`（invoke されたクラスタ内で値が変わった attribute id。`OnOffHandler::invoke` が push する — Task 2 の `let _ = ctx;` を差し替え）。`Node::handle_invoke` が `(endpoint, cluster)` を知っているので、runtime へは `Vec<(u16, u32, u32)>`（full path）として返す: `Node::take_changed(&mut self) -> Vec<(u16, u32, u32)>` は作らず、`handle_im` の戻りを `pub struct ImOutcome { pub opcode: u8, pub payload: Vec<u8>, pub changed: Vec<(u16, u32, u32)> }` に変える
  - `Node` にデータバージョン: `versions: HashMap<(u16, u32), u32>`（endpoint, cluster → version、初期 1）。invoke で changed が出たら該当 (endpoint,cluster) を +1。`read_entries` の `AttrReportOut.data_version` にこの値を使う（`UNVERSIONED` 定数は撤去）
  - `net/subscription.rs`: `pub struct ActiveSubscription { pub id: u32, pub paths: Vec<AttrPathIn>, pub min_interval: Duration, pub max_interval: Duration, pub last_report_at: tokio::time::Instant, pub dirty: Vec<(u16, u32, u32)> }` と純粋ヘルパ `pub fn next_report_deadline(&self) -> tokio::time::Instant`（dirty ありなら `last_report_at + min_interval`、無ければ `last_report_at + max_interval - MARGIN`（MARGIN=2s、ただし max_interval が小さければ 1/2））
  - runtime: SubscribeRequest 処理 + レポート送出（下記フロー）。同時に保持する購読は 1 本（新しい SubscribeRequest・新しいセッション確立で置き換え）。`max_interval` は `clamp(ceiling, 3, 60)` 秒で自分が決める

**runtime のフロー:**
1. `OPCODE_SUBSCRIBE_REQUEST` を `serve_secured_message` で分岐: `decode_subscribe_request` → priming: `node.read_chunks(&req.paths, .., REPORT_CHUNK_BUDGET)` を **subscription_id 付き・全チャンク suppress=false** でエンコードし直し（`read_chunks` に `subscription_id: Option<u32>` パラメータを足す — priming は最終チャンクも StatusResponse を待つ点が read と違う。spec §8.10: priming の後に SubscribeResponse が同 exchange で続く）、各チャンク送信→StatusResponse(0) 待ち → `encode_subscribe_response(id, max_interval)` を reply → `ActiveSubscription` を登録
2. select ループに第 4 の枝: `subscription_deadline(&active)`（`mdns_retry_deadline` と同型）。発火時: dirty があれば dirty 分の reports、無ければ空 reports を `encode_report_data_entries(&entries, false, Some(id), false)` で **新規 exchange**（`SecureSession::new_exchange_id()` + `send_reliable`）として送り、StatusResponse(0) を待つ（`send_reliable` の戻り or `recv`）。`last_report_at` 更新・dirty クリア。送信失敗/非 0 StatusResponse は購読破棄（ログ）
3. invoke 処理後、`ImOutcome.changed` を購読の paths と突き合わせ（wildcard マッチ含む）、該当分を `dirty` に積む

- [ ] **Step 1: `ImOutcome`/changed/データバージョンの失敗するテスト**（datamodel 単体: OnOff invoke 後に `outcome.changed == [(1, CLUSTER_ON_OFF, ATTR_ON_OFF)]`、同 (endpoint,cluster) の data_version が 1→2 に上がる）

- [ ] **Step 2: FAIL → 実装 → PASS**（`handle_im` シグネチャ変更の追随: runtime・既存テスト全部。機械的だが数が多い — コンパイラに従う）

- [ ] **Step 3: `next_report_deadline` の単体テスト → 実装**（dirty あり: min_interval 側 / なし: max_interval-MARGIN 側 / max_interval=3s のとき MARGIN が過大にならない）

- [ ] **Step 4: 閉ループ統合テストを書く（失敗する）**

`crates/mat-device/tests/subscribe_loop.rs`: 既存 direct-drive テストのセットアップ（commission 済み CASE セッション確立まで）を流用し、

```rust
// 1. mat 側から subscribe（クラスタ絞り込み: OnOff）
let (sr, priming) = session.subscribe_wildcard(0, 5, false, &[im::CLUSTER_ON_OFF], &cfg).await.unwrap();
assert!(priming.iter().flat_map(|m| &m.reports).any(|r| r.attribute == Some(im::ATTR_ON_OFF)));
// 2. デバイス側の状態をテスト側から変える（onoff_state.store(true)）→ device runtime に
//    dirty を伝える経路が invoke 起点のみなので、ここは別セッション…は無い（逐次）。
//    よって「controller から invoke → その変更が subscription report で届く」を検証する:
//    同一セッションで invoke(On) → next_subscription_report が OnOff=true を運んでくる
session.invoke(1, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None, &cfg).await.unwrap();
let rd = session.next_subscription_report(Duration::from_secs(10), &cfg).await.unwrap();
assert!(rd.reports.iter().any(|r| r.attribute == Some(im::ATTR_ON_OFF) && r.data == Some(serde_json::json!(true))));
// 3. keep-alive: 変更なしでも max_interval 内に空レポートが来る
let rd = session.next_subscription_report(Duration::from_secs(10), &cfg).await.unwrap();
assert!(rd.reports.is_empty());
assert_eq!(rd.subscription_id, Some(sr.subscription_id));
```
（`session.invoke` の実シグネチャは `session.rs:667` に合わせる。subscribe/invoke/report が同一セッション上で交錯する — runtime は逐次だが、レポート送出は select 枝なので invoke 応答後に流れる。テストのタイムアウトは余裕を持つ。）

- [ ] **Step 5: FAIL 確認 → `net/subscription.rs` + runtime 配線を実装 → PASS**

- [ ] **Step 6: チャンク priming の検証**（Task 6 の宿題: fat 属性ハンドラをテスト用に登録した Node で subscribe し、priming が複数チャンクで完走することを `subscribe_wildcard` が返す `Vec<ReportDataMessage>` の長さで確認）

- [ ] **Step 7: `task check` + `task e2e:device:m1` + `task e2e:device:m2-chip` PASS → Commit**

```bash
git commit -am "feat(mat-device): Subscribe サーバ（priming/dirty レポート/keep-alive）を実装（M2）"
```

---

### Task 13: ゲート 1 後半 — chip-tool subscribe + 総合

**Files:**
- Modify: `scripts/e2e-device-m2-chip.sh`

**Interfaces:**
- Consumes: Task 9 のスクリプト、Task 12 の Subscribe
- Produces: `task e2e:device:m2-chip` の最終形: commission → toggle → read → **subscribe-event/interactive で購読が確立しレポートが届く** → matv 再起動 → 再 CASE → toggle、まで

- [ ] **Step 1: subscribe 検証をスクリプトに追加**

chip-tool の対話モード（`chip-tool interactive start` に `onoff subscribe on-off <min> <max> 1 1` を流し込み、priming レポート受信行を timeout 付き grep）。対話モードのフィード方法はイメージの chip-tool バージョンに依存 — `echo '...' | chip-tool interactive start` が最有力、駄目なら `subscribe` 単発コマンドの有無を `chip-tool onoff subscribe --help` で確認。**購読中に別プロセスの chip-tool から toggle して変更レポートが届くこと**までを検証できれば理想だが、逐次デバイス（同時 1 セッション）のため二重 CASE になる — その場合 chip-tool 側の 2 本目の CASE が現行セッション（購読）を置き換えて購読が死ぬのは既知の設計制約。**スクリプトでは priming + keep-alive 受信までを自動検証**とし、変更レポートは Task 12 の閉ループテストが担保している旨をスクリプトコメントに明記する。

- [ ] **Step 2: 3 回連続グリーン → Commit**

```bash
git commit -am "test(e2e): m2-chip ゲートに subscribe 検証を追加（ゲート1完成）"
```

---

## Phase C — 堅牢化

### Task 14: コミッショニング窓ライフサイクル

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs`
- Modify: `crates/mat-device/src/device.rs`（窓ポリシーの決定）
- Test: runtime 単体 + direct-drive

**Interfaces:**
- Consumes: `classify_unsecured`（`runtime.rs:102`）、`MdnsAdvertiser::set_commissionable`（goodbye 対応済み: Task 8）
- Produces: 窓状態 `enum CommissioningWindow { Open { until: Instant }, Closed }` を runtime が保持。**ポリシー: 起動時に fabric が 0 なら窓 open（15 分, spec §5.4.2.3 の PASE 窓上限）、fabric ありなら closed。**CommissioningComplete で close（既存の set_commissionable(None) と同時に PASE 拒否も始まる）。15 分満了で close（commissionable goodbye + PASE 拒否）。**close 中の PASE opcode は無応答 drop**（申し送り 7 項: 現状は広告だけ止まり PASE は常時応答だった）。再 open は M2 ではプロセス再起動のみ（Administrator Commissioning cluster はスコープ外 — doc comment に明記）

- [ ] **Step 1: 失敗するテスト**（runtime 単体: 窓 Closed のとき `UnsecuredFlow::Pase` を drop する分岐のテスト。分岐を純粋関数 `fn admit_unsecured(flow: UnsecuredFlow, window_open: bool) -> Option<UnsecuredFlow>`（Pase かつ !window_open → None、CASE は常時 Some）に切って単体テスト）

- [ ] **Step 2: FAIL → 実装**（select ループに窓満了 deadline 枝（既存 3 枝と同型）を追加。満了時: `set_commissionable(None)`（goodbye 込み）+ 窓 Closed。起動時の open/closed は `comm_server.fabrics().is_empty()` で決定 — fabric 0 で closed だと二度と commission できないため）

- [ ] **Step 3: direct-drive 検証**（commission 完了後の PASE 試行が無応答になること — 既存 direct-drive テストの流れで commission 後に PBKDFParamRequest を送り、timeout で応答無しを確認）

- [ ] **Step 4: e2e への影響確認**（`task e2e:device:m1` は起動直後 commission なので窓 open — 影響なしのはず。m2-chip の再起動再接続ステップは「fabric ありで起動 → 窓 closed → CASE のみ」— これも通るはず。両方回す）

- [ ] **Step 5: Commit**

```bash
git commit -am "feat(mat-device): コミッショニング窓ライフサイクル（15分上限・close 中は PASE 無応答）"
```

---

### Task 15: PASE salt の乱数化

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs`（`SALT` 定数 → 起動時乱数）
- Test: runtime 単体

**Interfaces:**
- Consumes: `PaseVerifierConfig`（salt は既に設定注入: `runtime.rs:370-375`）
- Produces: 固定 `SALT`/`ITERATIONS` 定数を撤去。salt は起動時生成の 16 byte 乱数（`run` 冒頭で生成しループ変数として保持 — PBKDFParamResponse で相手に渡るので handshake 内整合だけあればよい）、iterations は `10_000` に引き上げ（spec §3.9 の範囲 1000..=100000 内。Pi 上の PBKDF2-SHA256 10k 回はミリ秒級で commissioning 1 回きりのコスト — doc comment に判断を書く）

- [ ] **Step 1: 実装**（`run` の冒頭: `let mut pase_salt = [0u8; 16]; getrandom::getrandom(&mut pase_salt)...`、`PaseVerifierConfig { salt: pase_salt.to_vec(), iterations: 10_000, .. }`。「固定 salt はレインボーテーブル前計算に弱い」旨を旧定数コメントの置き換えとして記載）

- [ ] **Step 2: 検証** — PASE は e2e が実証（`task e2e:device:m1` = 実 commission が乱数 salt で通る）。単体では `PaseVerifierConfig` に渡る salt が `b"SPAKE2P Key Salt"` でないことを直接は観測しづらいので e2e を正とする

- [ ] **Step 3: `task check` + 両 e2e → Commit**

```bash
git commit -am "fix(mat-device): PASE salt を起動時乱数化（固定 salt の前計算耐性）"
```

---

## Phase D — Echo ゲート

### Task 16: jarvis 配備 + Echo 実機ゲート【人間チェックポイント】+ ドキュメント

**Files:**
- Modify: `docs/superpowers/plans/m2-echo-checklist.md`（結果記入）
- Modify: `docs/superpowers/specs/2026-08-16-mat-device-m2-design.md`（完了時の申し送り節を追記）
- Modify: 必要なら README/ARCHITECTURE の matv 記述

**Interfaces:**
- Consumes: 全タスクの成果、Task 10 で確立した配備手順

- [ ] **Step 1: 最終ビルドを jarvis へ配備**（`task xbuild:arm64` → scp → jarvis で起動。Task 10 と同じ手動起動でよい。matv の store は Task 10 のものを**使わない**（Echo 側に古い登録が残っていればユーザーにアプリからの削除を依頼してから新 store で開始 — 窓ライフサイクル導入後の初回なのでクリーンに）

- [ ] **Step 2: 【人間チェックポイント】Echo ゲートをチェックリストで消化**

`m2-echo-checklist.md` の項目（Task 10 で初版作成、ここで最終形に）:
1. Alexa アプリから QR で commission 成功（警告の有無も記録）
2. アプリのデバイス画面に OnOff デバイスとして表示される
3. アプリから On/Off 操作 → matv ログに invoke が出て状態が変わる
4. 「アレクサ、〈デバイス名〉をつけて」で音声操作が通る
5. chip-tool（別 fabric）ではなく **Echo の購読**が張られている（matv ログで SubscribeRequest を確認）
6. matv 側の状態変化がアプリに反映される — 検証手段: chip-tool を**使わず**、Echo の定型アクションかアプリ操作で off → アプリ表示が追随（自発変化の完全な検証は M4 の mando 接続まで持ち越し。ログで dirty レポート送出を確認できれば可）
7. matv 再起動 → アプリから再操作できる（再 CASE + operational announce）
8. 各ステップの matv ログ断片をチェックリストに貼る

- [ ] **Step 3: 落ちた項目のフィックスラウンド**（Task 9 と同じ運用: 観測 → 最小修正 → 再配備 → 再試行。Wireshark が要る場合は jarvis 上で `tcpdump -i eth0 udp port 5540 or port 5353 -w` を取り WSL2 で解析）

- [ ] **Step 4: 完了時の申し送りを spec に追記**（M3=Aggregator に向けて: Echo が実際に要求したもの/しなかったもの、head-of-line blocking の顕在化有無、Sigma2Resume の要否、購読 1 本制約の影響）

- [ ] **Step 5: `task check` + 全 e2e 最終確認 → Commit → ユーザーへ完了報告**

```bash
git commit -am "docs(superpowers): M2 完了 — Echo 相互運用ゲート通過と M3 への申し送り"
```

---

## Self-Review メモ（計画時点の既知の弱点）

- Task 9/13/16 はフィックスラウンド型で、chip-tool/Echo の実挙動に依存する未知数を意図的に残している（spec の二段ゲート戦略そのもの）。各ラウンドの観測は `m2-chip-tool-probe.md` / `m2-echo-checklist.md` に記録し、次タスクの入力にする
- `handle_im` の戻り値変更（Task 12 の `ImOutcome`）は波及が広い。Task 12 が Phase B 先頭でなく Task 11（コーデック）の後にあるのはこのため — 一括で片付ける
- 購読は同時 1 本・セッション置き換えで死ぬ設計制約を明文化した（Task 13）。Echo 単独運用では問題にならず、M4 で多コントローラ要件が出たら再訪
