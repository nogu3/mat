# matv KeySetRemove/Read・privacy フラグ・membership 残骸・rollback assert・bitmap リプレイ窓 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matv（仮想 Matter デバイス、crate `mat-device` + バイナリ `matv`）の GroupKeyManagement に KeySetRead / KeySetRemove / KeySetReadAllIndices を足して `mat group remove` を通し、P ビット付き groupcast を復号し、削除済み endpoint の membership 残骸を起動時に掃除し、fail-safe rollback テストを補強し、リプレイ検査を 32 幅 bitmap 窓にする。

**Architecture:** すべて `crates/mat-device` 内で完結する（mat-controller / mat-core / mat-native / mat / matd は並行セッションが触っているので変更禁止）。GroupKeyManagement は `core/group_key_management.rs` の `GroupKeyStore`（Arc<Mutex> 共有 state + 任意の persist）と `GroupKeyManagementHandler`（EP0 のクラスタハンドラ）を拡張する。privacy は新モジュール `core/group_privacy.rs`（純関数、I/O 無し）で HKDF 導出と CCM keystream（= chip SDK の AES_CTR_crypt と同じ「CCM 暗号化してタグを捨てる」）を提供し、`net/group_rx.rs::classify_group_datagram` が候補 keyset ごとに header を復号してから既存の `open_message` に渡す。

**Tech Stack:** Rust stable、tokio、`mat_controller::{crypto, tlv, im, message, fabric}`、`hkdf`/`sha2`（mat-device 既存依存）、serde/serde_json。テストは `cargo test -p mat-device`、`task check`、`task e2e:device:m1` / `m3`（bash、実機不使用、`MAT_E2E_IFACE` 既定 eth1）。

**Spec:** `docs/superpowers/specs/2026-09-05-matv-keyset-remove-privacy-design.md`

## Global Constraints

- 変更してよいのは `crates/mat-device/**`、`crates/matv/**`、`scripts/e2e-device-m3.sh`、`README.md` の matv 段落、`docs/superpowers/**` のみ。`crates/mat-controller` / `crates/mat-core` / `crates/mat-native` / `crates/mat` / `crates/matd` は**触らない**（並行セッションが編集中）。必要な定数は mat-device に局所定義する。
- **依存追加なし**（`aes`/`ctr` は使わない — privacy の CTR は `mat_controller::crypto::encrypt_payload` の CCM 出力からタグを落として得る）。
- `core/` は I/O 無し（tokio・socket・fs 禁止）。`cargo check -p mat-device --no-default-features` が通ること。
- 各タスクの最後に `cargo fmt --all` と `cargo clippy -p mat-device --all-targets -- -D warnings` を通してからコミット。
- コミットメッセージは既存の流儀（`feat(mat-device): …` / `test(mat-device): …` / `docs: …`、本文は日本語可）。末尾に
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_016fq8QWcnWdxjkVb8N4EYh6
  ```
- テスト名・doc コメントに実 IP / 実 node id / 実証明書を書かない（repo は public）。
- 版（workspace version）は上げない。

---

### Task 1: `GroupKeyStore` 拡張 — `epoch_start_time0`・`remove_keyset`・`keyset_ids_for`・`find_keyset`

**Files:**
- Modify: `crates/mat-device/src/core/group_key_management.rs`（`GroupKeySet` 33-51 行、`GroupKeyStore` 86-233 行、`invoke` 341-364 行、`decode_key_set_write_fields` 446-493 行、tests）
- Modify（呼び出し側の引数追加）: `crates/mat-device/src/net/group_rx.rs`（tests の `gk.upsert_keyset(1, 42, EPOCH)`）、`crates/mat-device/src/core/commissioning.rs`（test `fail_safe_expiry_rolls_back_uncommitted_fabric` の `gk_store.upsert_keyset(1, 7, [9u8; 16])`）、その他 `grep -rn "upsert_keyset(" crates/mat-device` で出る全箇所

**Interfaces:**
- Produces:
  - `pub struct GroupKeySet { pub fabric_index: u8, pub keyset_id: u16, pub epoch_key0: [u8; 16], #[serde(default)] pub epoch_start_time0: u64 }`
  - `GroupKeyStore::upsert_keyset(&self, fabric_index: u8, keyset_id: u16, epoch_key0: [u8; 16], epoch_start_time0: u64) -> Result<(), u8>`
  - `GroupKeyStore::remove_keyset(&self, fabric_index: u8, keyset_id: u16) -> Result<bool, u8>` — `Ok(map_changed)`、無ければ `Err(im::STATUS_NOT_FOUND)`。同 fabric の map 行で `keyset_id` を参照するものも削除。
  - `GroupKeyStore::keyset_ids_for(&self, fabric_index: u8) -> Vec<u16>`（挿入順）
  - `GroupKeyStore::find_keyset(&self, fabric_index: u8, keyset_id: u16) -> Option<GroupKeySet>`
- `decode_key_set_write_fields` は `(u16, [u8; 16], u64)` を返す（`Context(3)` = EpochStartTime0、無ければ 0）。

- [ ] **Step 1: 失敗するテストを書く（tests モジュール末尾に追加）**

```rust
    #[test]
    fn remove_keyset_drops_keyset_and_referencing_map_rows_of_that_fabric_only() {
        let store = GroupKeyStore::new();
        store.upsert_keyset(1, 42, [7u8; 16], 0).unwrap();
        store.replace_fabric_map(1, vec![(10, 42), (11, 42)]);
        store.upsert_keyset(2, 42, [8u8; 16], 0).unwrap();
        store.replace_fabric_map(2, vec![(20, 42)]);

        assert_eq!(store.remove_keyset(1, 42), Ok(true), "map rows referenced it");
        assert!(!store.keyset_exists(1, 42));
        assert!(store.map_entries_for(1).is_empty());
        // 他 fabric は無傷
        assert!(store.keyset_exists(2, 42));
        assert_eq!(store.map_entries_for(2), vec![(20, 42)]);
        // 2 回目は NOT_FOUND
        assert_eq!(store.remove_keyset(1, 42), Err(im::STATUS_NOT_FOUND));
    }

    #[test]
    fn remove_keyset_reports_no_map_change_when_unreferenced() {
        let store = GroupKeyStore::new();
        store.upsert_keyset(1, 42, [7u8; 16], 0).unwrap();
        assert_eq!(store.remove_keyset(1, 42), Ok(false));
        assert!(store.keysets().is_empty());
    }

    #[test]
    fn keyset_ids_and_find_are_fabric_scoped_and_keep_start_time() {
        let store = GroupKeyStore::new();
        store.upsert_keyset(1, 42, [7u8; 16], 1_700_000_000_000).unwrap();
        store.upsert_keyset(2, 43, [8u8; 16], 5).unwrap();
        assert_eq!(store.keyset_ids_for(1), vec![42]);
        assert_eq!(store.keyset_ids_for(2), vec![43]);
        assert!(store.keyset_ids_for(3).is_empty());
        let ks = store.find_keyset(1, 42).unwrap();
        assert_eq!(ks.epoch_start_time0, 1_700_000_000_000);
        assert!(store.find_keyset(2, 42).is_none());
        // upsert は start time も置換する
        store.upsert_keyset(1, 42, [9u8; 16], 77).unwrap();
        assert_eq!(store.find_keyset(1, 42).unwrap().epoch_start_time0, 77);
    }

    /// `epoch_start_time0` を持たない旧 `group_keys.json`（v1.31.0 以前）は
    /// 0 として読める（`#[serde(default)]`）。
    #[test]
    fn keyset_json_without_start_time_loads_as_zero() {
        let json = r#"{"fabric_index":1,"keyset_id":42,"epoch_key0":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}"#;
        let ks: GroupKeySet = serde_json::from_str(json).unwrap();
        assert_eq!(ks.epoch_start_time0, 0);
        assert_eq!(ks.keyset_id, 42);
    }

    #[test]
    fn key_set_write_stores_epoch_start_time0() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        // encode_key_set_write_fields は Context(3)=EpochStartTime0 を書く
        // （値はクライアント実装が決める）— ここでは「読み捨てず保存する」
        // ことだけを、手組みの TLV で pin する。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(0));
        w.put_uint(Tag::Context(0), 0x01AA);
        w.put_uint(Tag::Context(1), 0);
        w.put_bytes(Tag::Context(2), &[0x11; 16]);
        w.put_uint(Tag::Context(3), 123_456);
        w.put_null(Tag::Context(4));
        w.put_null(Tag::Context(5));
        w.put_null(Tag::Context(6));
        w.put_null(Tag::Context(7));
        w.end_container();
        w.end_container();
        assert_eq!(
            h.invoke(im::CMD_KEY_SET_WRITE, &w.finish(), &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert_eq!(store.find_keyset(1, 0x01AA).unwrap().epoch_start_time0, 123_456);
    }
```

`Writer` に `put_null` / `put_bytes` があることは `crates/mat-controller/src/tlv.rs` で確認する（`tests/group_receive.rs` が `put_null` を使っている）。

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-device --lib group_key_management 2>&1 | tail -20`
Expected: コンパイルエラー（`upsert_keyset` の引数数、`remove_keyset` / `keyset_ids_for` / `find_keyset` / `epoch_start_time0` 未定義）。

- [ ] **Step 3: 実装**

`GroupKeySet`:

```rust
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupKeySet {
    pub fabric_index: u8,
    pub keyset_id: u16,
    pub epoch_key0: [u8; 16],
    /// KeySetWrite の `EpochStartTime0`（spec §11.2.6.2、epoch-us）。
    /// v1.31.0 以前の `group_keys.json` には無いので `default` = 0 で読む。
    /// KeySetRead が返す以外の用途は無い（epoch 1/2 非対応なので鍵選択に
    /// 使わない）。
    #[serde(default)]
    pub epoch_start_time0: u64,
}
```

`Debug` impl に `.field("epoch_start_time0", &self.epoch_start_time0)` を足す（鍵は引き続き REDACTED）。

`GroupKeyStore`:

```rust
    pub fn upsert_keyset(
        &self,
        fabric_index: u8,
        keyset_id: u16,
        epoch_key0: [u8; 16],
        epoch_start_time0: u64,
    ) -> Result<(), u8> {
        let mut guard = self.lock();
        if let Some(existing) = guard
            .keysets
            .iter_mut()
            .find(|k| k.fabric_index == fabric_index && k.keyset_id == keyset_id)
        {
            existing.epoch_key0 = epoch_key0;
            existing.epoch_start_time0 = epoch_start_time0;
            Self::save(&guard);
            return Ok(());
        }
        let count = guard
            .keysets
            .iter()
            .filter(|k| k.fabric_index == fabric_index)
            .count();
        if count >= MAX_GROUP_KEYS_PER_FABRIC {
            return Err(im::STATUS_RESOURCE_EXHAUSTED);
        }
        guard.keysets.push(GroupKeySet {
            fabric_index,
            keyset_id,
            epoch_key0,
            epoch_start_time0,
        });
        Self::save(&guard);
        Ok(())
    }

    /// KeySetRemove (spec §11.2.8.4) の実処理: `(fabric_index, keyset_id)` の
    /// KeySet を落とし、同 fabric の GroupKeyMap でその keyset を参照する行も
    /// 落とす（手順 4 のカスケード）。返り値は map が変化したか。無ければ
    /// `STATUS_NOT_FOUND`。lock 1 回・save 1 回。
    pub fn remove_keyset(&self, fabric_index: u8, keyset_id: u16) -> Result<bool, u8> {
        let mut guard = self.lock();
        let before = guard.keysets.len();
        guard
            .keysets
            .retain(|k| !(k.fabric_index == fabric_index && k.keyset_id == keyset_id));
        if guard.keysets.len() == before {
            return Err(im::STATUS_NOT_FOUND);
        }
        let map_before = guard.map.len();
        guard
            .map
            .retain(|m| !(m.fabric_index == fabric_index && m.keyset_id == keyset_id));
        let map_changed = guard.map.len() != map_before;
        Self::save(&guard);
        Ok(map_changed)
    }

    /// accessing fabric の KeySet id 一覧（挿入順）— KeySetReadAllIndices 用。
    pub fn keyset_ids_for(&self, fabric_index: u8) -> Vec<u16> {
        self.lock()
            .keysets
            .iter()
            .filter(|k| k.fabric_index == fabric_index)
            .map(|k| k.keyset_id)
            .collect()
    }

    /// `(fabric_index, keyset_id)` の KeySet のコピー — KeySetRead 用
    /// （応答は鍵を返さないので、呼び出し側は `epoch_start_time0` だけ使う）。
    pub fn find_keyset(&self, fabric_index: u8, keyset_id: u16) -> Option<GroupKeySet> {
        self.lock()
            .keysets
            .iter()
            .find(|k| k.fabric_index == fabric_index && k.keyset_id == keyset_id)
            .cloned()
    }
```

`decode_key_set_write_fields`: 戻り値を `Result<(u16, [u8; 16], u64), KeySetWriteError>` にし、内側ループに `(Tag::Context(3), Value::Uint(v)) => epoch_start_time0 = Some(v),` を追加、末尾は `let epoch_start_time0 = epoch_start_time0.unwrap_or(0);` で `Ok((keyset_id, epoch_key0, epoch_start_time0))`。doc の「`EpochStartTime0` はこの実装では読み捨てる」を「保存して KeySetRead が返す。無ければ 0（spec 上は必須だが互換のため緩く）」に直す。

`invoke` の KeySetWrite 分岐: `let (keyset_id, epoch_key0, epoch_start_time0) = match … ;` → `self.store.upsert_keyset(ctx.fabric_index, keyset_id, epoch_key0, epoch_start_time0)`。

既存テスト `mutations_persist_and_reload_in_a_new_instance` の期待値に `epoch_start_time0: 0` を足す。`keyset_debug_redacts_the_epoch_key` の struct リテラルにも `epoch_start_time0: 0`。

呼び出し側の 3 引数 → 4 引数（末尾に `0`）: `grep -rn "upsert_keyset(" crates/mat-device` で全部直す（`net/group_rx.rs` tests、`core/commissioning.rs` tests、`net/runtime.rs` tests があれば、`tests/*.rs`）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device 2>&1 | tail -15`
Expected: 全 PASS（既存テスト含む）。`cargo check -p mat-device --no-default-features` も OK。

- [ ] **Step 5: コミット**

```bash
cargo fmt --all && cargo clippy -p mat-device --all-targets -- -D warnings
git add crates/mat-device
git commit -m "feat(mat-device): GroupKeyStore に remove_keyset / keyset_ids_for / find_keyset と EpochStartTime0 保持を追加"
```

---

### Task 2: KeySetRead / KeySetRemove / KeySetReadAllIndices ハンドラ

**Files:**
- Modify: `crates/mat-device/src/core/group_key_management.rs`（モジュール doc 1-13 行、定数、`invoke` 334-368 行、`accepted_commands`、`generated_commands`、tests）

**Interfaces:**
- Consumes: Task 1 の `remove_keyset` / `keyset_ids_for` / `find_keyset`。
- Produces（pub const、mat-device 内で局所定義。`mat_controller::im` は触らない）:
  - `CMD_KEY_SET_READ: u32 = 0x01`、`RESP_KEY_SET_READ: u32 = 0x02`、`CMD_KEY_SET_REMOVE: u32 = 0x03`、`CMD_KEY_SET_READ_ALL_INDICES: u32 = 0x04`、`RESP_KEY_SET_READ_ALL_INDICES: u32 = 0x05`、`IPK_KEY_SET_ID: u16 = 0`
  - `pub fn decode_key_set_read_response(fields_tlv: &[u8]) -> Option<(u16, u8, Option<u64>)>`（テスト用に `pub(crate)` でよい: `(keyset_id, policy, epoch_start_time0)`; EpochKey が null でなければ `None`）

- [ ] **Step 1: 失敗するテストを書く**

tests モジュールに追加。既存の `declares_attributes_and_key_set_write_command` の assert を差し替え、`unknown_attribute_and_unknown_command_are_rejected` の `0x01` を `0x77` に変える（コメントも「0x77 は割り当ての無い id」に）。

```rust
    #[test]
    fn declares_all_key_set_commands() {
        let h = GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        assert_eq!(
            h.accepted_commands(),
            vec![
                im::CMD_KEY_SET_WRITE,
                CMD_KEY_SET_READ,
                CMD_KEY_SET_REMOVE,
                CMD_KEY_SET_READ_ALL_INDICES
            ]
        );
        assert_eq!(
            h.generated_commands(),
            vec![RESP_KEY_SET_READ, RESP_KEY_SET_READ_ALL_INDICES]
        );
    }

    fn write_keyset(h: &mut GroupKeyManagementHandler, fabric: u8, id: u16) -> InvokeCtx {
        let mut ctx = InvokeCtx {
            fabric_index: fabric,
            ..Default::default()
        };
        let ks = mat_controller::im::encode_key_set_write_fields(id, &[9u8; 16]);
        assert_eq!(
            h.invoke(im::CMD_KEY_SET_WRITE, &ks, &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        ctx
    }

    fn key_set_id_fields(id: u16) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), u64::from(id));
        w.end_container();
        w.finish()
    }

    /// `KeySetReadResponse {0: GroupKeySetStruct}` を `(id, policy,
    /// start_time0)` に戻す。EpochKey0/1/2 (2/4/6) が null 以外なら `None`
    /// — 鍵素材が漏れたら失敗させる。
    fn decode_key_set_read_response(fields: &[u8]) -> Option<(u16, u8, Option<u64>)> {
        let mut r = Reader::new(fields);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let el = r.next().unwrap().unwrap();
        assert_eq!((el.tag, el.value), (Tag::Context(0), Value::StructStart));
        let (mut id, mut policy, mut start) = (None, None, None);
        loop {
            let el = r.next().unwrap().unwrap();
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(0), Value::Uint(v)) => id = Some(v as u16),
                (Tag::Context(1), Value::Uint(v)) => policy = Some(v as u8),
                (Tag::Context(3), Value::Uint(v)) => start = Some(v),
                (Tag::Context(2 | 4 | 6), Value::Null) => {}
                (Tag::Context(5 | 7), Value::Null) => {}
                (Tag::Context(2 | 4 | 6), _) => return None,
                other => panic!("unexpected field {other:?}"),
            }
        }
        Some((id?, policy?, start))
    }

    #[test]
    fn key_set_read_returns_metadata_but_never_the_key() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = write_keyset(&mut h, 1, 42);
        let reply = h.invoke(CMD_KEY_SET_READ, &key_set_id_fields(42), &mut ctx);
        let InvokeReply::Data {
            response_command,
            fields_tlv,
        } = reply
        else {
            panic!("expected data reply, got {reply:?}");
        };
        assert_eq!(response_command, RESP_KEY_SET_READ);
        let start = store.find_keyset(1, 42).unwrap().epoch_start_time0;
        assert_eq!(
            decode_key_set_read_response(&fields_tlv),
            Some((42, 0, Some(start)))
        );
        // 鍵バイト列が応答に一切含まれない
        assert!(!fields_tlv.windows(16).any(|w| w == [9u8; 16]));
    }

    #[test]
    fn key_set_read_of_ipk_is_virtual_and_unknown_is_not_found() {
        let mut h =
            GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        let InvokeReply::Data { fields_tlv, .. } =
            h.invoke(CMD_KEY_SET_READ, &key_set_id_fields(0), &mut ctx)
        else {
            panic!("IPK keyset 0 must always be readable");
        };
        assert_eq!(decode_key_set_read_response(&fields_tlv), Some((0, 0, Some(0))));
        assert_eq!(
            h.invoke(CMD_KEY_SET_READ, &key_set_id_fields(42), &mut ctx),
            InvokeReply::Status(im::STATUS_NOT_FOUND)
        );
        // 他 fabric の keyset は見えない
        write_keyset(&mut h, 2, 42);
        assert_eq!(
            h.invoke(CMD_KEY_SET_READ, &key_set_id_fields(42), &mut ctx),
            InvokeReply::Status(im::STATUS_NOT_FOUND)
        );
    }

    #[test]
    fn key_set_commands_reject_pase_and_malformed_fields() {
        let mut h =
            GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        let mut pase = InvokeCtx::default();
        for cmd in [CMD_KEY_SET_READ, CMD_KEY_SET_REMOVE, CMD_KEY_SET_READ_ALL_INDICES] {
            assert_eq!(
                h.invoke(cmd, &key_set_id_fields(1), &mut pase),
                InvokeReply::Status(im::STATUS_UNSUPPORTED_ACCESS),
                "cmd {cmd:#x}"
            );
        }
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        for cmd in [CMD_KEY_SET_READ, CMD_KEY_SET_REMOVE] {
            assert_eq!(
                h.invoke(cmd, &[0xFF, 0x00], &mut ctx),
                InvokeReply::Status(im::STATUS_INVALID_COMMAND),
                "cmd {cmd:#x}"
            );
            // id 欠落（空 struct）
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.end_container();
            assert_eq!(
                h.invoke(cmd, &w.finish(), &mut ctx),
                InvokeReply::Status(im::STATUS_INVALID_COMMAND),
                "cmd {cmd:#x}"
            );
        }
    }

    #[test]
    fn key_set_remove_cascades_to_map_and_marks_the_attribute_changed() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = write_keyset(&mut h, 1, 42);
        let map = mat_controller::im::encode_group_key_map_tlv(&[(0x000A, 42)]);
        h.write(im::ATTR_GROUP_KEY_MAP, &map, false, &mut ctx).unwrap();
        ctx.changed.clear();

        assert_eq!(
            h.invoke(CMD_KEY_SET_REMOVE, &key_set_id_fields(42), &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert!(!store.keyset_exists(1, 42));
        assert!(store.map_entries_for(1).is_empty());
        assert_eq!(ctx.changed, vec![im::ATTR_GROUP_KEY_MAP]);

        // 参照の無い keyset の削除は changed を積まない
        let mut ctx = write_keyset(&mut h, 1, 43);
        ctx.changed.clear();
        assert_eq!(
            h.invoke(CMD_KEY_SET_REMOVE, &key_set_id_fields(43), &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert!(ctx.changed.is_empty());
    }

    #[test]
    fn key_set_remove_rejects_ipk_unknown_and_other_fabric() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = write_keyset(&mut h, 1, 42);
        assert_eq!(
            h.invoke(CMD_KEY_SET_REMOVE, &key_set_id_fields(0), &mut ctx),
            InvokeReply::Status(im::STATUS_INVALID_COMMAND)
        );
        assert_eq!(
            h.invoke(CMD_KEY_SET_REMOVE, &key_set_id_fields(99), &mut ctx),
            InvokeReply::Status(im::STATUS_NOT_FOUND)
        );
        let mut ctx2 = InvokeCtx {
            fabric_index: 2,
            ..Default::default()
        };
        assert_eq!(
            h.invoke(CMD_KEY_SET_REMOVE, &key_set_id_fields(42), &mut ctx2),
            InvokeReply::Status(im::STATUS_NOT_FOUND)
        );
        assert!(store.keyset_exists(1, 42));
    }

    fn decode_u16_list_response(fields: &[u8]) -> Vec<u16> {
        let mut r = Reader::new(fields);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let el = r.next().unwrap().unwrap();
        assert_eq!((el.tag, el.value), (Tag::Context(0), Value::ArrayStart));
        let mut out = Vec::new();
        loop {
            match r.next().unwrap().unwrap().value {
                Value::ContainerEnd => break,
                Value::Uint(v) => out.push(v as u16),
                other => panic!("unexpected {other:?}"),
            }
        }
        out
    }

    #[test]
    fn key_set_read_all_indices_lists_ipk_plus_this_fabrics_keysets() {
        let mut h =
            GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        let InvokeReply::Data {
            response_command,
            fields_tlv,
        } = h.invoke(CMD_KEY_SET_READ_ALL_INDICES, &[], &mut ctx)
        else {
            panic!("expected data reply");
        };
        assert_eq!(response_command, RESP_KEY_SET_READ_ALL_INDICES);
        assert_eq!(decode_u16_list_response(&fields_tlv), vec![0]);

        write_keyset(&mut h, 1, 42);
        write_keyset(&mut h, 2, 43);
        let InvokeReply::Data { fields_tlv, .. } =
            h.invoke(CMD_KEY_SET_READ_ALL_INDICES, &[], &mut ctx)
        else {
            panic!("expected data reply");
        };
        assert_eq!(decode_u16_list_response(&fields_tlv), vec![0, 42]);
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-device --lib group_key_management 2>&1 | tail -20`
Expected: コンパイルエラー（定数未定義）。

- [ ] **Step 3: 実装**

定数（`MAX_GROUP_KEYS_PER_FABRIC` の下）:

```rust
/// GroupKeyManagement のコマンド id（spec §11.2.8）。`mat_controller::im` は
/// `CMD_KEY_SET_WRITE` しか持たず、im.rs は他レーンの編集領域なのでここで
/// 局所定義する（`mat-native/src/ops.rs` の `CMD_KEY_SET_REMOVE` と同じ裁定）。
pub const CMD_KEY_SET_READ: u32 = 0x01;
pub const RESP_KEY_SET_READ: u32 = 0x02;
pub const CMD_KEY_SET_REMOVE: u32 = 0x03;
pub const CMD_KEY_SET_READ_ALL_INDICES: u32 = 0x04;
pub const RESP_KEY_SET_READ_ALL_INDICES: u32 = 0x05;
/// IPK の KeySet id（spec §11.2.6.2）。`GroupKeyStore` には持たず
/// （`FabricEntry.ipk_operational` 側）、KeySetRead/ReadAllIndices は仮想的に
/// 常在として応答し、KeySetRemove は INVALID_COMMAND で拒む。
pub const IPK_KEY_SET_ID: u16 = 0;
```

`invoke`（既存の KeySetWrite 分岐を残して分岐化）:

```rust
    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        if !matches!(
            command,
            im::CMD_KEY_SET_WRITE
                | CMD_KEY_SET_READ
                | CMD_KEY_SET_REMOVE
                | CMD_KEY_SET_READ_ALL_INDICES
        ) {
            return InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND);
        }
        if ctx.fabric_index == 0 {
            return InvokeReply::Status(im::STATUS_UNSUPPORTED_ACCESS);
        }
        match command {
            im::CMD_KEY_SET_WRITE => { /* 既存処理をそのまま移す */ }
            CMD_KEY_SET_READ => {
                let Some(keyset_id) = decode_key_set_id(fields_tlv) else {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                };
                let epoch_start_time0 = if keyset_id == IPK_KEY_SET_ID {
                    0
                } else {
                    match self.store.find_keyset(ctx.fabric_index, keyset_id) {
                        Some(ks) => ks.epoch_start_time0,
                        None => return InvokeReply::Status(im::STATUS_NOT_FOUND),
                    }
                };
                InvokeReply::Data {
                    response_command: RESP_KEY_SET_READ,
                    fields_tlv: encode_key_set_read_response(keyset_id, epoch_start_time0),
                }
            }
            CMD_KEY_SET_REMOVE => {
                let Some(keyset_id) = decode_key_set_id(fields_tlv) else {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                };
                if keyset_id == IPK_KEY_SET_ID {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                }
                match self.store.remove_keyset(ctx.fabric_index, keyset_id) {
                    Ok(true) => {
                        ctx.changed.push(im::ATTR_GROUP_KEY_MAP);
                        InvokeReply::Status(im::STATUS_SUCCESS)
                    }
                    Ok(false) => InvokeReply::Status(im::STATUS_SUCCESS),
                    Err(status) => InvokeReply::Status(status),
                }
            }
            _ => {
                // KeySetReadAllIndices: 引数は空 struct（読まない）。
                let mut ids = vec![IPK_KEY_SET_ID];
                ids.extend(self.store.keyset_ids_for(ctx.fabric_index));
                let mut w = Writer::new();
                w.start_struct(Tag::Anonymous);
                w.start_array(Tag::Context(0));
                for id in ids {
                    w.put_uint(Tag::Anonymous, u64::from(id));
                }
                w.end_container();
                w.end_container();
                InvokeReply::Data {
                    response_command: RESP_KEY_SET_READ_ALL_INDICES,
                    fields_tlv: w.finish(),
                }
            }
        }
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![
            im::CMD_KEY_SET_WRITE,
            CMD_KEY_SET_READ,
            CMD_KEY_SET_REMOVE,
            CMD_KEY_SET_READ_ALL_INDICES,
        ]
    }

    fn generated_commands(&self) -> Vec<u32> {
        vec![RESP_KEY_SET_READ, RESP_KEY_SET_READ_ALL_INDICES]
    }
```

ヘルパ（モジュール末尾、`decode_group_key_map_entry_body` の後）:

```rust
/// KeySetRead / KeySetRemove の `{0: GroupKeySetID}`。`groups.rs::decode_group_id`
/// と同じ形（先頭 struct の Context(0) uint、ネストは読み飛ばす）。形不正・
/// id 欠落は `None` → 呼び出し側は `STATUS_INVALID_COMMAND`。
fn decode_key_set_id(fields_tlv: &[u8]) -> Option<u16> {
    let mut r = Reader::new(fields_tlv);
    match r.next() {
        Ok(Some(el)) if el.value == Value::StructStart => {}
        _ => return None,
    }
    let mut keyset_id = None;
    loop {
        match r.next() {
            Ok(Some(el)) => match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(0), Value::Uint(v)) => keyset_id = u16::try_from(v).ok(),
                (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                    mat_controller::tlv::skip_container(&mut r).ok()?;
                }
                _ => {}
            },
            _ => return None,
        }
    }
    keyset_id
}

/// `KeySetReadResponse {0: GroupKeySetStruct}`（spec §11.2.8.3）。EpochKey0/1/2
/// は**必ず null**（鍵素材は返さない）、policy は TrustFirst(0) 固定（他は
/// KeySetWrite で拒む）、epoch 1/2 の StartTime も null（未対応）。
fn encode_key_set_read_response(keyset_id: u16, epoch_start_time0: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(0));
    w.put_uint(Tag::Context(0), u64::from(keyset_id));
    w.put_uint(Tag::Context(1), 0);
    w.put_null(Tag::Context(2));
    w.put_uint(Tag::Context(3), epoch_start_time0);
    w.put_null(Tag::Context(4));
    w.put_null(Tag::Context(5));
    w.put_null(Tag::Context(6));
    w.put_null(Tag::Context(7));
    w.end_container();
    w.end_container();
    w.finish()
}
```

`skip_container` の正確なシグネチャ（`&mut Reader` を取り `Result`）は既存の `decode_key_set_write_fields` の使い方に合わせる。

モジュール doc（1-13 行）の「`KeySetRead`/`KeySetRemove`/`KeySetReadAllIndices` コマンドは未実装（既知ギャップ、groupcast タスク送り）」を次に置換:

```
//! `KeySetRead`（§11.2.8.2、EpochKey は null で返す）/ `KeySetRemove`（§11.2.8.4、
//! GroupKeyMap の参照行もカスケード削除）/ `KeySetReadAllIndices`（§11.2.8.5）も
//! 実装済み。IPK = keyset 0 は `GroupKeyStore` には持たず（`FabricEntry` 側）、
//! Read/ReadAllIndices では常在の仮想 keyset として応答し、Remove は
//! INVALID_COMMAND で拒む。
```

`invoke` の doc コメント（334-340 行）も「4 コマンドを受理」「KeySetRead/Remove の形不正・id 欠落は INVALID_COMMAND」に更新。`invoke_privilege` の doc の「この実装が受理するのは `CMD_KEY_SET_WRITE` だけ」を「KeySet 系 4 コマンド全部」に。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device 2>&1 | tail -15` と `cargo check -p mat-device --no-default-features`
Expected: 全 PASS。

- [ ] **Step 5: コミット**

```bash
cargo fmt --all && cargo clippy -p mat-device --all-targets -- -D warnings
git add crates/mat-device
git commit -m "feat(mat-device): GroupKeyManagement に KeySetRead / KeySetRemove / KeySetReadAllIndices を実装（IM 0x81 解消）"
```

---

### Task 3: e2e m3 の `mat group remove` ステップ再有効化

**Files:**
- Modify: `scripts/e2e-device-m3.sh`（ヘッダ flow コメント 1-20 行、`mat group list` assert 直後の保留コメント ブロック「`mat group remove` はここでは撃てない…」を丸ごと置換）

**Interfaces:**
- Consumes: Task 2（matv が KeySetRemove を受理する）。`mat group remove` の出力形は `docs/commands.md` 839 行: `{ "group_id", "endpoint", "nodes": [ { "node_id", "acl_removed", "group_removed", "keymap_removed", "keyset_removed" } ], "controller": { "group_removed", "keyset_removed" }, "status": "removed" }`。`mat group list` は `{ "fabric_index", "groups": [...], "keysets": [ { "keyset_id", "bound_groups" } ] }`。keyset 0（IPK）はチェーンに残る（`mat-controller::group_settings` tests「keyset 0 (IPK) はチェーンに残る」）。

- [ ] **Step 1: 保留コメントブロックを次に置換**

```bash
# 撤収 4 ステップ（ACL Group エントリ除去 → RemoveGroup → group-key-map 除去 →
# 未参照 keyset の KeySetRemove）が全部 matv に着地することを assert する。
# matv の KeySetRemove は 2026-09-05 に実装（それ以前は IM 0x81 で保留していた）。
echo "==> mat group remove (group=$GROUP_ID, node=$NODE_ID, endpoint=$DEVICE_EP)" >&2
REMOVE_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
        ./target/release/mat --iface "$IFACE" group remove \
            --group "$GROUP_ID" --nodes "$NODE_ID" --endpoint "$DEVICE_EP"
)"
echo "$REMOVE_JSON"
[[ "$(json_get status "$REMOVE_JSON")" == "removed" ]]
printf '%s' "$REMOVE_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
n = d["nodes"][0]
assert n["node_id"] == '"$NODE_ID"', d
for k in ("acl_removed", "group_removed", "keymap_removed", "keyset_removed"):
    assert n[k] is True, (k, d)
assert d["controller"]["group_removed"] is True, d
'
echo "==> PASS: mat group remove reached status=removed (ACL / RemoveGroup / group-key-map / KeySetRemove all landed on matv)" >&2

echo "==> mat group list after remove (controller kvs: no groups, only the IPK keyset 0)" >&2
LIST_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat group list)"
echo "$LIST_JSON" >&2
printf '%s' "$LIST_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["groups"] == [], d
assert all(k["keyset_id"] == 0 for k in d["keysets"]), d
'

# 以降の groupcast / matd / listen 脚のために provision し直す。
echo "==> mat group provision again (group=$GROUP_ID)" >&2
GROUP_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
        ./target/release/mat --iface "$IFACE" group provision \
            --group "$GROUP_ID" --nodes "$NODE_ID" --endpoint "$DEVICE_EP" --name e2e-group
)"
echo "$GROUP_JSON"
[[ "$(json_get status "$GROUP_JSON")" == "provisioned" ]]
echo "==> PASS: re-provision after remove reached status=provisioned" >&2
```

ヘッダコメントの flow 記述（「`mat group list`, asserting the provisioned group shows up in the controller kvs (`mat group remove` is held back — see the comment at that step: matv has no `KeySetRemove`)」）を「`mat group list` … -> `mat group remove` (asserting all four removal steps landed on matv and the controller kvs is back to the IPK keyset only) -> `mat group provision` again」に書き換える。

- [ ] **Step 2: m3 を実行して PASS を確認**

Run: `task e2e:device:m3 2>&1 | tail -40`（`eth1` が無い環境なら `MAT_E2E_IFACE=<multicast 可能な iface> task e2e:device:m3`）
Expected: 末尾に `PASS: mat group remove reached status=removed`、`PASS: re-provision after remove`、`PASS: groupcast on reached matv`、`PASS: matd's resident Subscribe delivered …`。落ちたら matv の stderr（スクリプトが出す）で IM status を見る。

- [ ] **Step 3: コミット**

```bash
git add scripts/e2e-device-m3.sh
git commit -m "test(e2e): m3 に mat group remove → group list 空 → 再 provision のステップを復活（matv KeySetRemove 実装に伴い）"
```

---

### Task 4: `core/group_privacy.rs` — privacy key 導出・nonce・CTR・header 復号

**Files:**
- Create: `crates/mat-device/src/core/group_privacy.rs`
- Modify: `crates/mat-device/src/core/mod.rs`（`pub mod group_privacy;` を `group_membership` の次に）

**Interfaces:**
- Consumes: `mat_controller::crypto::{encrypt_payload, MIC_LEN}`、`mat_controller::message::MessageHeader`、`hkdf::Hkdf<sha2::Sha256>`。
- Produces:
  - `pub const PRIVACY_FLAG: u8 = 0x80;`
  - `pub const PRIVACY_HEADER_OFFSET: usize = 4;`
  - `pub fn derive_privacy_key(operational_key: &[u8; 16]) -> [u8; 16]`
  - `pub fn privacy_nonce(session_id: u16, mic: &[u8; 16]) -> [u8; 13]`
  - `pub fn privacy_crypt(key: &[u8; 16], nonce: &[u8; 13], data: &mut [u8])`
  - `pub fn deobfuscate_header(datagram: &[u8], operational_key: &[u8; 16]) -> Option<Vec<u8>>`
  - `pub fn obfuscate_header(datagram: &mut [u8], operational_key: &[u8; 16]) -> bool`（テスト用の送信側。同じ CTR なので実装は deobfuscate と同じ区間に `privacy_crypt` を当てるだけ。本番では使わないが `#[cfg(test)]` にしない — `tests/group_receive.rs`（統合テスト）から使う）

- [ ] **Step 1: 失敗するテストを書く（新ファイルにモジュール + tests）**

```rust
//! groupcast の privacy 処理（spec §4.8.3 Message Privacy、§4.16.2 Privacy Key）。
//!
//! chip SDK（`Crypto::AES_CTR_crypt` / `Crypto::DeriveGroupPrivacyKey` /
//! `CryptoContext::BuildPrivacyNonce`、2026-09-05 に master で確認）と
//! バイト一致させるための事実:
//! - 難読化は AES-CTR だが、SDK は **AES-CCM 暗号化（AAD 無し）の出力から
//!   タグを捨てる**ことで CTR を得ている → counter block は CCM の
//!   `0x01 ‖ nonce(13) ‖ 0x0001` 起点。mat-controller の `encrypt_payload` で
//!   同じものが作れるので依存追加なし。
//! - Privacy Key = HKDF-SHA256(ikm = operational group key, salt = 空,
//!   info = "PrivacyKey", 16 バイト)。
//! - Privacy Nonce = session id（big-endian 2 バイト）‖ MIC[5..16]（11 バイト）。
//! - 難読化区間 = message header の offset 4（message counter 先頭）から
//!   header 末尾（destination まで）。message flags / session id / security
//!   flags（P ビット含む）は平文で、AAD と CCM nonce にはそのまま使う。
//! - Message Extensions（X フラグ）は非対応（`MessageHeader::decode` も読まない）。
use mat_controller::crypto::{encrypt_payload, MIC_LEN};
use mat_controller::message::MessageHeader;

/// security flags の P ビット（spec §4.4.1.4）。
pub const PRIVACY_FLAG: u8 = 0x80;
/// 難読化区間の先頭 = message flags(1) + session id(2) + security flags(1)。
pub const PRIVACY_HEADER_OFFSET: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::message::{Destination, ProtocolHeader};

    const OP: [u8; 16] = [0x5A; 16];

    #[test]
    fn privacy_key_is_deterministic_and_differs_from_the_operational_key() {
        let a = derive_privacy_key(&OP);
        let b = derive_privacy_key(&OP);
        assert_eq!(a, b);
        assert_ne!(a, OP);
        assert_ne!(derive_privacy_key(&[0x5B; 16]), a);
    }

    #[test]
    fn privacy_nonce_is_big_endian_session_id_then_mic_tail() {
        let mic: [u8; 16] = core::array::from_fn(|i| i as u8);
        let n = privacy_nonce(0x1234, &mic);
        assert_eq!(n[0..2], [0x12, 0x34]);
        assert_eq!(n[2..], mic[5..16]);
    }

    #[test]
    fn privacy_crypt_is_an_involution_and_key_dependent() {
        let key = derive_privacy_key(&OP);
        let nonce = privacy_nonce(0xBEEF, &[0xCC; 16]);
        let plain: Vec<u8> = (0..14u8).collect(); // counter(4)+source(8)+group(2)
        let mut buf = plain.clone();
        privacy_crypt(&key, &nonce, &mut buf);
        assert_ne!(buf, plain);
        privacy_crypt(&key, &nonce, &mut buf);
        assert_eq!(buf, plain);
        let mut other = plain.clone();
        privacy_crypt(&derive_privacy_key(&[1u8; 16]), &nonce, &mut other);
        privacy_crypt(&key, &nonce, &mut other);
        assert_ne!(other, plain);
    }

    /// CCM keystream ピン: `encrypt_payload(key, nonce, aad=[], data)` の先頭
    /// `len` バイトと一致する（= SDK `AES_CTR_crypt` の定義そのもの）。
    #[test]
    fn privacy_crypt_equals_ccm_ciphertext_without_the_tag() {
        let key = [3u8; 16];
        let nonce = [4u8; 13];
        let data = [9u8; 20];
        let mut buf = data;
        privacy_crypt(&key, &nonce, &mut buf);
        let ccm = encrypt_payload(&key, &nonce, &[], &data).unwrap();
        assert_eq!(buf[..], ccm[..20]);
        assert_eq!(ccm.len(), 20 + MIC_LEN);
    }

    fn group_datagram_bytes(security_flags: u8) -> Vec<u8> {
        let header = MessageHeader {
            session_id: 0x0102,
            security_flags,
            message_counter: 0x11223344,
            source_node_id: Some(0x0A0B0C0D0E0F1011),
            destination: Destination::Group(0x000A),
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: false,
            acked_counter: None,
            opcode: 0x08,
            exchange_id: 1,
            protocol_id: 1,
            vendor_id: None,
        };
        mat_controller::crypto::seal_message(&OP, &header, &proto, &[1, 2, 3], 0).unwrap()
    }

    #[test]
    fn obfuscate_then_deobfuscate_restores_the_header_and_touches_nothing_else() {
        let plain = group_datagram_bytes(0x01 | PRIVACY_FLAG);
        let mut wire = plain.clone();
        assert!(obfuscate_header(&mut wire, &OP));
        // 区間 [4, 18) だけ変わり、flags/session/secflags と payload/MIC は不変
        assert_eq!(wire[..4], plain[..4]);
        assert_ne!(wire[4..18], plain[4..18]);
        assert_eq!(wire[18..], plain[18..]);
        let back = deobfuscate_header(&wire, &OP).unwrap();
        assert_eq!(back, plain);
        // 復号後は open_message がそのまま通る（AAD/nonce に P ビットが残る）
        let (h, _, body) = mat_controller::crypto::open_message(&OP, &back, 0).unwrap();
        assert_eq!(h.source_node_id, Some(0x0A0B0C0D0E0F1011));
        assert_eq!(body, vec![1, 2, 3]);
    }

    #[test]
    fn deobfuscate_rejects_datagrams_too_short_for_header_plus_mic() {
        let plain = group_datagram_bytes(0x81);
        assert!(deobfuscate_header(&plain[..18 + MIC_LEN - 1], &OP).is_none());
        assert!(deobfuscate_header(&[0u8; 3], &OP).is_none());
        assert!(!obfuscate_header(&mut [0u8; 3], &OP));
    }
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-device --lib group_privacy 2>&1 | tail -20`
Expected: コンパイルエラー（関数未定義）。

- [ ] **Step 3: 実装（tests の上）**

```rust
/// Privacy Key = HKDF-SHA256(operational group key, salt = 空, "PrivacyKey")
/// — SDK `Crypto::DeriveGroupPrivacyKey`。
pub fn derive_privacy_key(operational_key: &[u8; 16]) -> [u8; 16] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, operational_key);
    let mut out = [0u8; 16];
    hk.expand(b"PrivacyKey", &mut out)
        .expect("16 bytes is a valid hkdf-sha256 output length");
    out
}

/// Privacy Nonce = session id（BE 2 バイト）‖ MIC[5..16] — SDK
/// `CryptoContext::BuildPrivacyNonce`（offset 5・長さ 11）。
pub fn privacy_nonce(session_id: u16, mic: &[u8; 16]) -> [u8; 13] {
    let mut n = [0u8; 13];
    n[..2].copy_from_slice(&session_id.to_be_bytes());
    n[2..].copy_from_slice(&mic[5..16]);
    n
}

/// AES-CTR（対称: 暗号化も復号も同じ）。SDK `AES_CTR_crypt` と同じく CCM
/// 暗号化（AAD 無し）の先頭 `data.len()` バイト = keystream XOR。
/// `encrypt_payload` の唯一の失敗はサイズ超過で、区間は最長 20 バイト
/// （counter 4 + source 8 + destination node 8）なので到達しない — 万一
/// `Err` なら `data` を変えずに戻る（復号に失敗した datagram は後段の
/// `open_message` が MIC 不一致で落とす）。
pub fn privacy_crypt(key: &[u8; 16], nonce: &[u8; 13], data: &mut [u8]) {
    if let Ok(ct) = encrypt_payload(key, nonce, &[], data) {
        data.copy_from_slice(&ct[..data.len()]);
    } else {
        debug_assert!(false, "privacy region exceeds the CCM payload limit");
    }
}

/// `datagram` の header 難読化区間 `[PRIVACY_HEADER_OFFSET, payload_off)` に
/// `operational_key` 由来の privacy keystream を当てる（対称なので難読化・
/// 復号の両方）。header 長は message flags だけで決まるので、値が難読化
/// されていても `MessageHeader::decode` の返す offset は正しい。短すぎて
/// header + MIC が入らなければ `false`（何もしない）。
fn crypt_header_in_place(datagram: &mut [u8], operational_key: &[u8; 16]) -> bool {
    let Ok((_, payload_off)) = MessageHeader::decode(datagram) else {
        return false;
    };
    if datagram.len() < payload_off + MIC_LEN || payload_off <= PRIVACY_HEADER_OFFSET {
        return false;
    }
    let session_id = u16::from_le_bytes([datagram[1], datagram[2]]);
    let mut mic = [0u8; MIC_LEN];
    mic.copy_from_slice(&datagram[datagram.len() - MIC_LEN..]);
    let key = derive_privacy_key(operational_key);
    let nonce = privacy_nonce(session_id, &mic);
    privacy_crypt(&key, &nonce, &mut datagram[PRIVACY_HEADER_OFFSET..payload_off]);
    true
}

/// 受信側: 難読化された header を復号したコピーを返す（`None` = 短すぎ）。
/// security flags の P ビットは**落とさない** — 後段の `open_message` は
/// wire の security flags を AAD / nonce に使う（SDK も同じ）。
pub fn deobfuscate_header(datagram: &[u8], operational_key: &[u8; 16]) -> Option<Vec<u8>> {
    let mut copy = datagram.to_vec();
    crypt_header_in_place(&mut copy, operational_key).then_some(copy)
}

/// 送信側（テスト・将来の matv 送出用）: `seal_message` 済みの datagram の
/// header を in-place で難読化する。呼び出し側が security flags に
/// `PRIVACY_FLAG` を立てて封じておくこと（AAD に含まれるため後から
/// 立てられない）。
pub fn obfuscate_header(datagram: &mut [u8], operational_key: &[u8; 16]) -> bool {
    crypt_header_in_place(datagram, operational_key)
}
```

session id は wire では little-endian（`MessageHeader::encode` が `to_le_bytes`）、nonce では big-endian に詰め直す — 上のコードのとおり。`mod.rs` に `pub mod group_privacy;` を追加。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device --lib group_privacy 2>&1 | tail -15` と `cargo check -p mat-device --no-default-features`
Expected: 6 PASS。

- [ ] **Step 5: コミット**

```bash
cargo fmt --all && cargo clippy -p mat-device --all-targets -- -D warnings
git add crates/mat-device
git commit -m "feat(mat-device): groupcast privacy 処理の純関数（PrivacyKey 導出・nonce・CCM keystream・header 復号）"
```

---

### Task 5: `classify_group_datagram` の P ビット対応 + 閉ループ統合テスト

**Files:**
- Modify: `crates/mat-device/src/net/group_rx.rs`（モジュール doc 1-6 行、`PRIVACY_FLAG` 29-30 行、`GroupDrop` 79-92 行、`classify_group_datagram` 100-172 行、tests `drops_are_classified` の privacy ケース 444-451 行、新テスト）
- Modify: `crates/mat-device/tests/group_receive.rs`（`send_group_toggle` の隣に P ビット版、既存テストの末尾にステップ追加）

**Interfaces:**
- Consumes: Task 4 の `group_privacy::{PRIVACY_FLAG, deobfuscate_header, obfuscate_header}`。
- Produces: `GroupDrop` から `Privacy` を削除。`group_rx::PRIVACY_FLAG` は `pub use crate::core::group_privacy::PRIVACY_FLAG;` で再公開（既存の外部参照が無いことを `grep -rn PRIVACY_FLAG crates/` で確認 — 現状 group_rx.rs 内のみ）。

- [ ] **Step 1: 失敗するテストを書く**

`group_rx.rs` tests: `drops_are_classified` の privacy ブロックを削除し、次を追加。

```rust
    /// P ビット付き datagram を作る（chip SDK と同じ送信形）: security flags
    /// に PRIVACY_FLAG を立てて封じ、header を難読化する。
    fn privacy_datagram(f: &FabricEntry, epoch: &[u8; 16], counter: u32, group: u16) -> Vec<u8> {
        use mat_controller::message::{MessageHeader, ProtocolHeader};
        let c = creds(f, epoch);
        let header = MessageHeader {
            session_id: c.session_id,
            security_flags: SESSION_TYPE_GROUP | PRIVACY_FLAG,
            message_counter: counter,
            source_node_id: Some(SOURCE),
            destination: Destination::Group(group),
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: false,
            acked_counter: None,
            opcode: im::OPCODE_INVOKE_REQUEST,
            exchange_id: 0x42,
            protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
            vendor_id: None,
        };
        let payload = im::encode_group_invoke_request(im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE, None);
        let mut dg =
            mat_controller::crypto::seal_message(&c.encryption_key, &header, &proto, &payload, SOURCE)
                .unwrap();
        assert!(crate::core::group_privacy::obfuscate_header(&mut dg, &c.encryption_key));
        dg
    }

    #[test]
    fn privacy_flagged_datagram_is_deobfuscated_and_classified_like_plain() {
        let (fabrics, gk, m) = provisioned();
        let deps = GroupRxDeps {
            fabrics: &fabrics,
            gk_store: &gk,
            membership: &m,
        };
        let mut replay = GroupReplayGuard::new();
        let dg = privacy_datagram(&fabrics[0], &EPOCH, 100, 10);
        let batch = classify_group_datagram(&dg, &deps, &mut replay).unwrap();
        assert_eq!(
            (batch.fabric_index, batch.group_id, batch.source_node_id),
            (1, 10, SOURCE)
        );
        assert_eq!(batch.endpoints, vec![2, 3]);
        assert_eq!(
            (batch.invokes[0].cluster, batch.invokes[0].command),
            (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE)
        );
        // 同じ counter の再送は復号後のリプレイ検査で落ちる
        assert_eq!(
            classify_group_datagram(&dg, &deps, &mut replay).unwrap_err(),
            GroupDrop::Replay
        );
    }

    #[test]
    fn privacy_flagged_datagram_with_wrong_or_missing_key_is_no_keyset() {
        let (fabrics, gk, m) = provisioned();
        let deps = GroupRxDeps {
            fabrics: &fabrics,
            gk_store: &gk,
            membership: &m,
        };
        let mut replay = GroupReplayGuard::new();
        // GKH 不一致（別 epoch）→ 候補ゼロ
        let other = privacy_datagram(&fabrics[0], &[3u8; 16], 1, 10);
        assert_eq!(
            classify_group_datagram(&other, &deps, &mut replay).unwrap_err(),
            GroupDrop::NoKeyset { candidates: 0 }
        );
        // P を立てたが難読化していない（= 受信側が復号すると header が壊れる）:
        // 難読化をもう一度当てて平文 header に戻す（CTR は対称）
        let c = creds(&fabrics[0], &EPOCH);
        let mut raw = privacy_datagram(&fabrics[0], &EPOCH, 2, 10);
        assert!(crate::core::group_privacy::obfuscate_header(&mut raw, &c.encryption_key));
        assert_eq!(
            classify_group_datagram(&raw, &deps, &mut replay).unwrap_err(),
            GroupDrop::NoKeyset { candidates: 1 }
        );
        // 難読化区間を 1 バイト改竄 → 復号後の header/nonce が変わり MIC 不一致
        raw = privacy_datagram(&fabrics[0], &EPOCH, 3, 10);
        raw[5] ^= 0x01;
        assert_eq!(
            classify_group_datagram(&raw, &deps, &mut replay).unwrap_err(),
            GroupDrop::NoKeyset { candidates: 1 }
        );
    }
```

`tests/group_receive.rs`: `send_group_toggle` の直後に

```rust
/// chip SDK の送信形（security flags P ビット + header 難読化）で toggle を送る。
async fn send_private_group_toggle(creds: &GroupCredentials, to: SocketAddr, counter: u32) {
    use mat_controller::message::{Destination, MessageHeader, ProtocolHeader, PROTOCOL_ID_INTERACTION_MODEL};
    let header = MessageHeader {
        session_id: creds.session_id,
        security_flags: 0x01 | mat_device::core::group_privacy::PRIVACY_FLAG,
        message_counter: counter,
        source_node_id: Some(ADMIN_NODE_ID),
        destination: Destination::Group(GROUP_ID),
    };
    let proto = ProtocolHeader {
        initiator: true,
        needs_ack: false,
        acked_counter: None,
        opcode: im::OPCODE_INVOKE_REQUEST,
        exchange_id: 0x1235,
        protocol_id: PROTOCOL_ID_INTERACTION_MODEL,
        vendor_id: None,
    };
    let payload = im::encode_group_invoke_request(im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE, None);
    let mut dg = mat_controller::crypto::seal_message(
        &creds.encryption_key,
        &header,
        &proto,
        &payload,
        ADMIN_NODE_ID,
    )
    .unwrap();
    assert!(mat_device::core::group_privacy::obfuscate_header(&mut dg, &creds.encryption_key));
    let sock = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
    sock.send_to(&dg, to).await.unwrap();
}
```

既存テスト `groupcast_toggle_is_applied_replay_rejected_acl_enforced_and_state_persists` のステップ 3（counter 101 で on→off）の直後に:

```rust
    // 3b. P ビット付き（chip SDK の送信形）も適用される: off -> on -> off。
    send_private_group_toggle(&creds, group_addr, 150).await;
    expect_onoff(&mut session, true, "privacy-flagged group toggle").await;
    send_private_group_toggle(&creds, group_addr, 151).await;
    expect_onoff(&mut session, false, "second privacy-flagged group toggle").await;
```

（後続ステップの counter 102 以降は 150/151 より小さいので、Task 8 の bitmap 窓では「窓外の過去」= 拒否になる。**このテストの後続ステップの counter を 152, 153, … に振り直す**（既存の 102 以降の `send_group_toggle(…, 102)` 等を全部 +50 する）。Task 5 の時点では単調 guard なので同じく振り直しが必要。）

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-device --lib group_rx 2>&1 | tail -20`
Expected: 新テスト 2 本が FAIL（`GroupDrop::Privacy` が返る／コンパイルエラー）。

- [ ] **Step 3: 実装**

`group_rx.rs` 冒頭の `pub const PRIVACY_FLAG` を削除し `pub use crate::core::group_privacy::PRIVACY_FLAG;` に。`use crate::core::group_privacy::deobfuscate_header;` と `use std::borrow::Cow;` を追加。`GroupDrop::Privacy` を削除（doc に「P ビット付きは復号して通す。鍵が無ければ NoKeyset」）。

`classify_group_datagram`:

```rust
pub fn classify_group_datagram(
    buf: &[u8],
    deps: &GroupRxDeps<'_>,
    replay: &mut GroupReplayGuard,
) -> Result<GroupInvokeBatch, GroupDrop> {
    let (wire_header, _) = MessageHeader::decode(buf).map_err(|_| GroupDrop::HeaderDecode)?;
    if wire_header.security_flags & SESSION_TYPE_MASK != SESSION_TYPE_GROUP {
        return Err(GroupDrop::NotGroupSession);
    }
    let privacy = wire_header.security_flags & PRIVACY_FLAG != 0;
    if !privacy {
        // 平文 header なら復号前に形を検査できる（drop 理由の順序は従来どおり）。
        wire_header.source_node_id.ok_or(GroupDrop::NoSource)?;
        if !matches!(wire_header.destination, Destination::Group(_)) {
            return Err(GroupDrop::NotGroupDestination);
        }
    }

    // spec §4.15.3: GKH が一致する keyset を全 fabric から集めて順に試す。
    // P ビット付き（chip SDK の送信形）は候補ごとに privacy key で header を
    // 復号してから open_message に渡す（source / destination は復号後の値）。
    let mut candidates = 0usize;
    let mut opened = None;
    for ks in deps.gk_store.keysets() {
        let Some(f) = deps
            .fabrics
            .iter()
            .find(|f| f.fabric_index == ks.fabric_index)
        else {
            continue;
        };
        let operational = derive_ipk_operational(
            &ks.epoch_key0,
            &compressed_fabric_id(&f.root_public_key, f.fabric_id),
        );
        if derive_group_session_id(&operational) != wire_header.session_id {
            continue;
        }
        candidates += 1;
        let dg: Cow<'_, [u8]> = if privacy {
            match deobfuscate_header(buf, &operational) {
                Some(plain) => Cow::Owned(plain),
                None => continue,
            }
        } else {
            Cow::Borrowed(buf)
        };
        let Ok((header, _)) = MessageHeader::decode(&dg) else {
            continue;
        };
        let (Some(source), Destination::Group(group_id)) = (header.source_node_id, header.destination)
        else {
            continue;
        };
        if let Ok((_, proto, payload)) = open_message(&operational, &dg, source) {
            opened = Some((ks.fabric_index, ks.keyset_id, source, group_id, header.message_counter, proto, payload));
            break;
        }
    }
    let Some((fabric_index, keyset_id, source, group_id, counter, proto, payload)) = opened else {
        return Err(GroupDrop::NoKeyset { candidates });
    };
    if !deps
        .gk_store
        .map_entries_for(fabric_index)
        .contains(&(group_id, keyset_id))
    {
        return Err(GroupDrop::NotMapped);
    }
    let endpoints = deps.membership.endpoints_for(fabric_index, group_id);
    if endpoints.is_empty() {
        return Err(GroupDrop::NoMembers);
    }
    // 復号後（= 認証後）にリプレイ検査: 偽 datagram で窓を進められないように。
    if !replay.accept(fabric_index, source, counter) {
        return Err(GroupDrop::Replay);
    }
    if proto.protocol_id != PROTOCOL_ID_INTERACTION_MODEL
        || proto.opcode != im::OPCODE_INVOKE_REQUEST
    {
        return Err(GroupDrop::NotInvoke);
    }
    let invokes = decode_group_invoke_request(&payload).map_err(|_| GroupDrop::Malformed)?;
    Ok(GroupInvokeBatch {
        fabric_index,
        group_id,
        source_node_id: source,
        endpoints,
        invokes,
    })
}
```

上のコードで `opened` はループ内で
`opened = Some((ks.fabric_index, ks.keyset_id, source, group_id, header.message_counter, proto, payload));`
と束ね、ループ後は
`let Some((fabric_index, keyset_id, source, group_id, counter, proto, payload)) = opened else { return Err(GroupDrop::NoKeyset { candidates }); };`
で受ける（`opened` の型は 7 要素タプルの `Option`）。

モジュール doc（1-6 行）に「P ビット付きは `core::group_privacy` で header を復号してから同じ経路」を 1 文追加。`runtime.rs` の `GroupDrop` は `?reason` でしか使っていないので変更不要（`grep -n "GroupDrop::" crates/mat-device/src/net/runtime.rs` で確認）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device 2>&1 | tail -15`（統合テスト `group_receive` 含む）
Expected: 全 PASS。

- [ ] **Step 5: コミット**

```bash
cargo fmt --all && cargo clippy -p mat-device --all-targets -- -D warnings
git add crates/mat-device
git commit -m "feat(mat-device): groupcast の privacy フラグ（P ビット）付き datagram を復号して受理"
```

---

### Task 6: 削除済み endpoint の membership 残骸を起動時に掃除

**Files:**
- Modify: `crates/mat-device/src/core/group_membership.rs`（`purge_fabric` の隣に `retain_endpoints`、tests）
- Modify: `crates/mat-device/src/device.rs`（`Device::new` の `ledger.save()` 直後 394 行付近、tests）
- Modify: `crates/mat-device/src/net/endpoint_ledger.rs`（モジュール doc 1-10 行に 1 文追記）

**Interfaces:**
- Produces: `GroupMembershipStore::retain_endpoints(&self, live: &[u16]) -> usize`（落とした行数。変化があれば save）。

- [ ] **Step 1: 失敗するテストを書く**

`group_membership.rs` tests:

```rust
    #[test]
    fn retain_endpoints_drops_rows_of_endpoints_not_listed_across_fabrics() {
        let s = GroupMembershipStore::new();
        s.add(1, 10, 2).unwrap();
        s.add(1, 10, 3).unwrap();
        s.add(2, 20, 3).unwrap();
        s.add(1, 11, 4).unwrap();
        assert_eq!(s.retain_endpoints(&[2, 4]), 2);
        assert_eq!(s.endpoints_for(1, 10), vec![2]);
        assert!(s.endpoints_for(2, 20).is_empty());
        assert_eq!(s.endpoints_for(1, 11), vec![4]);
        assert_eq!(s.retain_endpoints(&[2, 4]), 0, "idempotent");
    }

    struct CountingPersist(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl GroupMembershipPersist for CountingPersist {
        fn save(&self, _: &[GroupMember]) -> Result<(), String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn load(&self) -> Result<Vec<GroupMember>, String> {
            Ok(vec![
                GroupMember { fabric_index: 1, group_id: 10, endpoint: 2 },
                GroupMember { fabric_index: 1, group_id: 10, endpoint: 3 },
            ])
        }
    }

    #[test]
    fn retain_endpoints_saves_only_when_something_was_dropped() {
        let saves = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let s = GroupMembershipStore::with_persist(Box::new(CountingPersist(saves.clone())));
        assert_eq!(s.retain_endpoints(&[2, 3]), 0);
        assert_eq!(saves.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(s.retain_endpoints(&[2]), 1);
        assert_eq!(saves.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
```

`device.rs` tests（`bridge_topology_and_ledger_stability` の隣。同じ `cfg`/`dev` クロージャの形を再掲する）:

```rust
    /// 設定から外した `[[device]]` の membership 行は起動時に掃除される
    /// （`groups.json` に存在しない endpoint が残ると GroupTable / multicast
    /// join / dispatch に化けて出る）。台帳の tombstone とは独立: 再追加で
    /// endpoint は戻るが membership は戻らない（再 provision が要る）。
    #[tokio::test]
    async fn stale_group_membership_of_removed_devices_is_pruned_on_start() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = |devices: Vec<VirtualDeviceConfig>| DeviceConfig {
            passcode: 20202021,
            discriminator: 0xF00,
            vendor_id: 0xFFF1,
            product_id: 0x8000,
            port: 0,
            store_dir: dir.path().to_path_buf(),
            iface: "lo".into(),
            attestation: AttestationMode::default(),
            group_port: 0,
            devices,
        };
        let dev = |id: &str| VirtualDeviceConfig {
            id: id.into(),
            kind: DeviceKind::OnOffLight,
            name: id.into(),
        };
        // a -> EP2, b -> EP3 を台帳に採番させる
        drop(Device::new(cfg(vec![dev("a"), dev("b")])).unwrap());
        // 両 endpoint に membership を書く（前回稼働中に AddGroup された想定）
        let groups_json = dir.path().join("groups.json");
        std::fs::write(
            &groups_json,
            serde_json::to_vec(&[
                crate::core::group_membership::GroupMember { fabric_index: 1, group_id: 10, endpoint: 2 },
                crate::core::group_membership::GroupMember { fabric_index: 1, group_id: 10, endpoint: 3 },
            ])
            .unwrap(),
        )
        .unwrap();
        // b を外して起動 → EP3 の行だけ消える
        drop(Device::new(cfg(vec![dev("a")])).unwrap());
        let left: Vec<crate::core::group_membership::GroupMember> =
            serde_json::from_slice(&std::fs::read(&groups_json).unwrap()).unwrap();
        assert_eq!(
            left,
            vec![crate::core::group_membership::GroupMember { fabric_index: 1, group_id: 10, endpoint: 2 }]
        );
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-device --lib retain_endpoints 2>&1 | tail -10` / `cargo test -p mat-device --lib stale_group_membership 2>&1 | tail -10`
Expected: コンパイルエラー（`retain_endpoints` 未定義）→ 実装後 device テストは「EP3 が残る」で FAIL するはず。

- [ ] **Step 3: 実装**

`group_membership.rs`（`purge_fabric` の後）:

```rust
    /// 起動時の掃除: `live` に無い endpoint の行を全 fabric 横断で落とす
    /// （設定から外した `[[device]]` の残骸 — `Device::new` が台帳の採番を
    /// 確定した直後に呼ぶ）。返り値は落とした行数。変化があれば save。
    pub fn retain_endpoints(&self, live: &[u16]) -> usize {
        let mut guard = self.lock();
        let before = guard.members.len();
        guard.members.retain(|m| live.contains(&m.endpoint));
        let removed = before - guard.members.len();
        if removed > 0 {
            Self::save(&guard);
        }
        removed
    }
```

`device.rs` の `ledger.save().map_err(DeviceError::Io)?;` の直後:

```rust
        // 設定から外した `[[device]]` の membership 残骸を掃除する。台帳は
        // tombstone で endpoint を保持する（再追加で同じ endpoint が戻る）が、
        // membership は戻さない — 存在しない endpoint を GroupTable / multicast
        // join / group dispatch に晒さない方を優先する（再追加後は
        // `mat group provision` で登録し直す）。
        let pruned = membership.retain_endpoints(&bridged_eps);
        if pruned > 0 {
            tracing::info!(
                removed = pruned,
                "group membership: pruned rows for endpoints no longer in config"
            );
        }
```

`endpoint_ledger.rs` モジュール doc 末尾に「Group membership (`groups.json`) is *not* kept for a tombstoned id: `Device::new` prunes rows whose endpoint is no longer configured (`GroupMembershipStore::retain_endpoints`), so a re-added device needs `mat group provision` again.」を追記。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device 2>&1 | tail -15`
Expected: 全 PASS。

- [ ] **Step 5: コミット**

```bash
cargo fmt --all && cargo clippy -p mat-device --all-targets -- -D warnings
git add crates/mat-device
git commit -m "fix(mat-device): 設定から外した [[device]] の group membership 残骸を起動時に掃除"
```

---

### Task 7: fail-safe rollback テストに ACL / membership purge の assert を追加

**Files:**
- Modify: `crates/mat-device/src/core/commissioning.rs`（tests: `fail_safe_expiry_rolls_back_uncommitted_fabric` 2860 行付近、`rearm_keeps_uncommitted_fabric_and_complete_commits_it`、新テスト 1 本）

**Interfaces:**
- Consumes: `AclStore::{new, check}`、`Subject::node`、`PRIVILEGE_ADMINISTER`（`crate::core::access_control`）、`GroupMembershipStore::{new, add, groups_by_fabric}`、`CommissioningServer::{set_acl_store, set_group_membership_store}`、`encode_arm_fail_safe(expiry_s, breadcrumb)`、`drive_invoke`。テストコードのみ、本体変更なし。

- [ ] **Step 1: 3 store を配線する共通ヘルパを tests に追加し、既存テストを書き換える**

```rust
    /// rollback / RemoveFabric の purge 対象 3 store を配線した server。
    fn server_with_stores() -> (
        CommissioningServer,
        crate::core::access_control::AclStore,
        GroupKeyStore,
        GroupMembershipStore,
    ) {
        let mut server = test_server();
        let acl = crate::core::access_control::AclStore::new();
        let gk = GroupKeyStore::new();
        let membership = GroupMembershipStore::new();
        server.set_acl_store(acl.clone());
        server.set_group_key_store(gk.clone());
        server.set_group_membership_store(membership.clone());
        (server, acl, gk, membership)
    }

    /// fabric 1 の admin (subject 0xAA) が ACL 上 Administer を持つか —
    /// AddNOC の自動 admin エントリの有無を表す。
    fn admin_allowed(acl: &crate::core::access_control::AclStore) -> bool {
        acl.check(
            1,
            crate::core::access_control::Subject::node(0xAA),
            crate::core::access_control::PRIVILEGE_ADMINISTER,
            0,
            CLUSTER_ACCESS_CONTROL,
        )
    }

    /// install 直後に 3 store へ fabric 1 の状態を仕込む（admin ACL は AddNOC が
    /// 自動で入れる）。
    fn seed_fabric_state(gk: &GroupKeyStore, membership: &GroupMembershipStore) {
        gk.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();
        gk.replace_fabric_map(1, vec![(0x000A, 7)]);
        membership.add(1, 0x000A, 2).unwrap();
    }

    fn assert_fabric_state_purged(
        acl: &crate::core::access_control::AclStore,
        gk: &GroupKeyStore,
        membership: &GroupMembershipStore,
    ) {
        assert!(!admin_allowed(acl), "ACL admin entry must be purged");
        assert!(!gk.keyset_exists(1, 7));
        assert!(gk.map_entries_for(1).is_empty());
        assert!(membership.groups_by_fabric().is_empty(), "membership must be purged");
    }

    #[test]
    fn fail_safe_expiry_rolls_back_uncommitted_fabric() {
        let (mut server, acl, gk, membership) = server_with_stores();
        install_fabric(&mut server, 0x1122, 0x5001);
        assert_eq!(server.fabrics().len(), 1);
        assert!(admin_allowed(&acl), "AddNOC installs the admin ACL entry");
        seed_fabric_state(&gk, &membership);

        server.force_expire_fail_safe();
        let removed = server.expire_fail_safe();
        assert_eq!(removed.map(|e| e.fabric_index), Some(1));
        assert!(server.fabrics().is_empty());
        assert!(server.fail_safe_deadline().is_none());
        assert_fabric_state_purged(&acl, &gk, &membership);

        // Idempotent: the marker and the timer are both already cleared.
        assert!(server.expire_fail_safe().is_none());
    }

    /// 早期 disarm（`ArmFailSafe(0)`）も満了と同じ rollback 経路: 未確定
    /// fabric と 3 store の状態が消える。
    #[test]
    fn arm_fail_safe_zero_rolls_back_uncommitted_fabric_and_purges_stores() {
        let (mut server, acl, gk, membership) = server_with_stores();
        install_fabric(&mut server, 0x1122, 0x5001);
        seed_fabric_state(&gk, &membership);

        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 3),
        );
        assert!(server.fabrics().is_empty());
        assert!(server.fail_safe_deadline().is_none());
        assert_fabric_state_purged(&acl, &gk, &membership);
    }
```

`rearm_keeps_uncommitted_fabric_and_complete_commits_it` を `server_with_stores()` + `seed_fabric_state` で始め、再アーム直後に `assert!(admin_allowed(&acl)); assert!(gk.keyset_exists(1, 7)); assert_eq!(membership.endpoints_for(1, 0x000A), vec![2]);` を足す（残りは現状どおり）。既存の `use` に `GroupMembershipStore` が無ければ tests 冒頭に `use crate::core::group_membership::GroupMembershipStore;` を追加。

- [ ] **Step 2: テストが通ることを確認（本体は既に purge している — 緑のはず）**

Run: `cargo test -p mat-device --lib commissioning::tests 2>&1 | tail -15`
Expected: 全 PASS。もし `arm_fail_safe_zero_…` が落ちるなら `handle_arm_fail_safe` の `expiry_s == 0` 分岐を読み直す（spec 上は rollback するはず。本体の修正が必要なら最小限で直し、コミットメッセージに書く）。

- [ ] **Step 3: 変異で assert が効くことを一度確認する**

`rollback_uncommitted_fabric` の `if let Some(store) = &self.group_membership_store { store.purge_fabric(fabric_index); }` を一時的にコメントアウトして `cargo test -p mat-device --lib fail_safe_expiry_rolls_back` が FAIL することを確認し、戻す（コミットしない）。

- [ ] **Step 4: コミット**

```bash
cargo fmt --all && cargo clippy -p mat-device --all-targets -- -D warnings
git add crates/mat-device
git commit -m "test(mat-device): fail-safe rollback（満了 / 早期 disarm）で ACL・GroupKey・membership の 3 store が purge されることを pin"
```

---

### Task 8: `GroupReplayGuard` を 32 幅 bitmap 窓に

**Files:**
- Modify: `crates/mat-device/src/net/group_rx.rs`（`GroupReplayGuard` 32-68 行、モジュール doc、test `replay_guard_is_strictly_increasing_per_source_and_bounded`）
- Modify: `crates/mat-device/tests/group_receive.rs`（既存テストの「2. Replay (same counter)」はそのまま通る。Task 5 で振り直した counter 列に矛盾が無いか再確認）

**Interfaces:**
- Produces: `pub const REPLAY_WINDOW: u32 = 32;` `GroupReplayGuard::accept(&mut self, fabric_index: u8, source_node_id: u64, counter: u32) -> bool`（シグネチャ不変）。

- [ ] **Step 1: 失敗するテストを書く（既存テストを差し替え + 追加）**

```rust
    #[test]
    fn replay_guard_accepts_out_of_order_within_the_window_once() {
        let mut g = GroupReplayGuard::new();
        assert!(g.accept(1, 7, 10)); // trust-first
        assert!(g.accept(1, 7, 12));
        assert!(g.accept(1, 7, 11), "順序逆転は窓内なら通る");
        assert!(!g.accept(1, 7, 11), "同じ counter の 2 回目は拒否");
        assert!(!g.accept(1, 7, 12));
        assert!(!g.accept(1, 7, 10), "trust-first で受理した値もマーク済み");
        assert!(g.accept(1, 7, 13));
        assert!(g.accept(2, 7, 1), "another fabric is another window");
    }

    #[test]
    fn replay_guard_rejects_counters_older_than_the_window() {
        let mut g = GroupReplayGuard::new();
        assert!(g.accept(1, 7, 100));
        assert!(!g.accept(1, 7, 100 - REPLAY_WINDOW - 1), "窓外（33 前）は拒否");
        assert!(g.accept(1, 7, 100 - REPLAY_WINDOW), "窓の端（32 前）は通る");
        assert!(!g.accept(1, 7, 100 - REPLAY_WINDOW), "…が 2 回目は拒否");
    }

    #[test]
    fn replay_guard_forward_jump_clears_the_window() {
        let mut g = GroupReplayGuard::new();
        assert!(g.accept(1, 7, 10));
        assert!(g.accept(1, 7, 10 + 40));
        for c in 10..=49 {
            assert!(!g.accept(1, 7, c), "counter {c} は新しい窓の外");
        }
        assert!(g.accept(1, 7, 51));
        assert!(!g.accept(1, 7, 50));
    }

    #[test]
    fn replay_guard_handles_rollover_as_forward_motion() {
        let mut g = GroupReplayGuard::new();
        assert!(g.accept(1, 7, 0xFFFF_FFF0));
        assert!(g.accept(1, 7, 5), "2^31 未満の前進は rollover でも新規");
        assert!(!g.accept(1, 7, 0xFFFF_FFF0), "旧 max は窓内でマーク済み");
        assert!(g.accept(1, 7, 0xFFFF_FFF1), "窓内の未受理値は通る");
        assert!(!g.accept(1, 7, 0x8000_0006), "2^31 以上離れた値は過去扱い → 窓外");
    }

    #[test]
    fn replay_guard_table_is_bounded_and_evicts_the_oldest_source() {
        let mut g = GroupReplayGuard::new();
        assert!(g.accept(1, 7, 10));
        for src in 100..(100 + REPLAY_TABLE_CAPACITY as u64) {
            assert!(g.accept(1, src, 1));
        }
        // 最古（fabric 1, source 7）が退去 → 同じ counter がまた通る
        assert!(g.accept(1, 7, 10));
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-device --lib replay_guard 2>&1 | tail -20`
Expected: `accepts_out_of_order` / `rejects_older` / `rollover` が FAIL（`REPLAY_WINDOW` 未定義でコンパイルエラーでもよい）。

- [ ] **Step 3: 実装**

```rust
/// spec §4.5.4.2 の group data message counter 窓幅（SDK
/// `PeerMessageCounter` の `kChallengeSize`/window と同じ 32）。
pub const REPLAY_WINDOW: u32 = 32;

/// spec §4.5.4.2 / SDK `PeerMessageCounter::VerifyOrTrustFirstGroup` と同じ
/// 「最大値 + 直近 32 件の bitmap」: `(fabric, source node)` ごとに `max` と
/// `window`（bit i = `max - (i + 1)` を受理済み）を持つ。初見の source は
/// trust-first で受理。前方（`c - max mod 2^32 < 2^31`、rollover 込み）は
/// 新規として窓をずらし、窓内（1..=32 後方）は bitmap で 1 回だけ受理、
/// それより過去は拒否。
pub struct GroupReplayGuard {
    seen: VecDeque<((u8, u64), ReplayWindow)>,
}

#[derive(Clone, Copy, Debug)]
struct ReplayWindow {
    max: u32,
    window: u32,
}

impl ReplayWindow {
    fn accept(&mut self, counter: u32) -> bool {
        if counter == self.max {
            return false;
        }
        let ahead = counter.wrapping_sub(self.max);
        if ahead < 1 << 31 {
            self.window = if ahead >= REPLAY_WINDOW {
                0
            } else {
                (self.window << ahead) | (1 << (ahead - 1))
            };
            self.max = counter;
            return true;
        }
        let behind = self.max.wrapping_sub(counter);
        if behind > REPLAY_WINDOW {
            return false;
        }
        let bit = 1u32 << (behind - 1);
        if self.window & bit != 0 {
            return false;
        }
        self.window |= bit;
        true
    }
}

impl GroupReplayGuard {
    pub fn new() -> Self {
        Self {
            seen: VecDeque::new(),
        }
    }

    pub fn accept(&mut self, fabric_index: u8, source_node_id: u64, counter: u32) -> bool {
        let key = (fabric_index, source_node_id);
        if let Some(pos) = self.seen.iter().position(|(k, _)| *k == key) {
            return self.seen[pos].1.accept(counter);
        }
        if self.seen.len() >= REPLAY_TABLE_CAPACITY {
            self.seen.pop_front();
        }
        self.seen.push_back((key, ReplayWindow { max: counter, window: 0 }));
        true
    }
}
```

`behind == 32` のとき `1u32 << 31` は定義内、`ahead == 32` は `window = 0`（`<< 32` を踏まない）— 上の分岐順で担保。モジュール doc の「bitmap window は持たず順序逆転は捨てる側に倒す」を新しい説明に置換。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device 2>&1 | tail -15`
Expected: 全 PASS（`tests/group_receive.rs` 含む）。

- [ ] **Step 5: コミット**

```bash
cargo fmt --all && cargo clippy -p mat-device --all-targets -- -D warnings
git add crates/mat-device
git commit -m "feat(mat-device): groupcast リプレイ検査を spec §4.5.4.2 の 32 幅 bitmap 窓に（順序逆転を許容、rollover 対応）"
```

---

### Task 9: ドキュメント更新と最終検証

**Files:**
- Modify: `README.md`（matv 段落 121-129 行）
- Modify: `crates/mat-device/src/net/group_rx.rs` / `core/group_key_management.rs` / `core/group_privacy.rs` のモジュール doc（Task 2/4/5/8 で更新済みなら読み直して整合だけ確認）
- Modify: `scripts/e2e-device-m3.sh` ヘッダ（Task 3 で更新済みなら確認のみ）

- [ ] **Step 1: README の matv 段落末尾を書き換える**

現行:
```
Keys and membership persist under
`<store>/group_keys.json` / `groups.json`; replay protection is a
monotonic (fabric, source) counter. Group-addressed Read/Write and
privacy-flagged groupcast are not implemented yet.
```
→
```
Keys and membership persist under
`<store>/group_keys.json` / `groups.json`; replay protection is the spec's
32-wide (fabric, source) counter window. Privacy-flagged groupcast (the form
the chip SDK, chip-tool and Apple Home send) is decrypted the same way, and
`KeySetRead` / `KeySetRemove` / `KeySetReadAllIndices` are served, so
`mat group remove` fully tears a group down on `matv`. Membership rows of a
`[[device]]` removed from `matv.toml` are pruned at startup (re-adding the
device restores its endpoint, not its groups). Group-addressed Read/Write
is not implemented.
```

- [ ] **Step 2: 全検証**

Run（順に）:
```bash
task check 2>&1 | tail -20
cargo check -p mat-device --no-default-features
task e2e:device:m1 2>&1 | tail -15
task e2e:device:m3 2>&1 | tail -25
```
Expected: `task check` 緑（fmt:check + clippy + test）、m1 / m3 ともに末尾が PASS 行。m3 は `PASS: mat group remove …` と `PASS: re-provision …` と groupcast / listen の PASS がすべて出ること。

- [ ] **Step 3: コミット**

```bash
git add README.md crates/mat-device scripts/e2e-device-m3.sh
git commit -m "docs: matv の KeySet 系コマンド・privacy フラグ・membership 掃除・bitmap リプレイ窓を README に反映"
```

---

## Self-review（plan 作成時に実施済み）

- **Spec coverage**: §2.1-2.6 → Task 1/2、§2.7 → Task 3、§3.1 → Task 4、§3.2-3.3 → Task 5、§4 → Task 6、§5 → Task 7、§6 → Task 8、§7 → Task 2/5/8 のモジュール doc + Task 9、§8 → Task 9。
- **Type consistency**: `upsert_keyset` 4 引数（Task 1 で全呼び出し側を直す。Task 7 のヘルパも 4 引数）。`remove_keyset -> Result<bool, u8>`、`find_keyset -> Option<GroupKeySet>`、`keyset_ids_for -> Vec<u16>`、`retain_endpoints -> usize`、`deobfuscate_header -> Option<Vec<u8>>`、`obfuscate_header -> bool`、`REPLAY_WINDOW: u32`。
- **注意**: Task 5 と Task 8 は同じ `group_receive.rs` の counter 列を触る。Task 5 で 150/151 を挿入し以降を +50 した状態が Task 8 の窓（32）でも矛盾しない（152 以降は単調増加）。
