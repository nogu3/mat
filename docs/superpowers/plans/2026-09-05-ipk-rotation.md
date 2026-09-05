# IPK ローテーション（`mat fabric rotate-ipk`）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** fabric の IPK（keyset 0）を新 epoch へ安全にローテーションする `mat fabric rotate-ipk`（配布 → 実証 → commit、途中失敗は pending で再開可能、取り残しは `--catch-up`）を実装し、同じ領域の `write_keyset` 既存 id 上書きを無損失化する。

**Architecture:** KVS の mat 名前空間 3 キー（`ipk-epoch` / `ipk-epoch-next` / `ipk-epoch-prev`）で状態を持ち、各ノードへ `KeySetWrite(0, {現行@1, 新@2})` を逐次配布したあと新 IPK で CASE を張り直して受理を実証、全ノード ok のときだけ 1 KvsTxn で controller 側（`f/<idx>/k/0` slot 0 + 3 キー）を切り替える。デバイス側の旧 epoch は次回ローテーションの上書きで消える（rolling 2 epoch）。`mat-native::rotate_ipk` が状態機械、`crates/mat` は CLI と body 出力だけ。

**Tech Stack:** Rust ワークスペース（`mat-controller` / `mat-native` / `mat`）、tokio current_thread、`KvsTxn`（flock + tmp+rename）、TLV `Reader`/`Writer`/`copy_value`、clap。

**Spec:** `docs/superpowers/specs/2026-09-05-ipk-rotation-design.md`

## Global Constraints

- 触らない: `crates/mat-controller/src/dnssd/`、`crates/mat-controller/src/test_support.rs`、`crates/mat-core/src/ids.rs`、`crates/mat-device/`（並行セッションの担当）。
- stdout は pure JSON、`timestamp` は `output::emit` が付ける。鍵素材（epoch / operational）は stdout・ログに一切出さない。
- KVS 書込は必ず `KvsTxn`（flock + tmp+rename）。`Locked` はハードエラー。
- chip-tool INI 互換の既存テスト（`group_settings` / `kvs`）は全部残す・通す。
- `rotate-ipk` は matd プロトコルに載せない（`commission` / `unpair` と同じ直経路専用、明示 `--matd` は exit 2）。
- matv（mat-device）は `KeySetWrite(0)` を INVALID_COMMAND で拒む — 統合テストは失敗経路を検証する（spec スコープ外）。
- 各タスク終了時に `cargo fmt` と `cargo clippy -p <crate> --all-targets -- -D warnings` を通し、コミットする。コミットメッセージ末尾: `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` と `Claude-Session: https://claude.ai/code/session_014qMsZw8YEuutZXohCUk4cY`。
- コメント・doc は既存ファイルの言語（日本語 / 英語）に合わせる。

---

### Task 1: `keyset_with_slot0` と `write_keyset` の無損失化（mat-controller）

**Files:**
- Modify: `crates/mat-controller/src/group_settings.rs`（`keyset_with_next` の直後に新ヘルパ、`write_keyset` の既存 id 腕、tests）

**Interfaces:**
- Consumes: `crate::tlv::{Reader, Writer, Tag, Value, copy_value}`、既存 `serialize_keyset` / `keyset_next` / `multi_epoch_keyset`（test fixture）。
- Produces: `pub(crate) fn keyset_with_slot0(blob: &[u8], start_time: u64, hash: u16, key: &[u8; 16]) -> Option<Vec<u8>>`（Task 2 の commit が使う）。

- [ ] **Step 1: 失敗するテストを書く**

`group_settings.rs` の `mod tests` 末尾に追加:

```rust
    #[test]
    fn keyset_with_slot0_matches_serialize_keyset_for_mat_form() {
        // mat 1 スロット形の slot0 差し替えは serialize_keyset の作り直しとバイト一致。
        let mine = serialize_keyset(0, EPOCH_START_TIME, 7, &[0x42; 16], 5);
        assert_eq!(
            keyset_with_slot0(&mine, EPOCH_START_TIME, 9, &[0x43; 16]).unwrap(),
            serialize_keyset(0, EPOCH_START_TIME, 9, &[0x43; 16], 5)
        );
    }

    #[test]
    fn keyset_with_slot0_preserves_policy_other_slots_and_next() {
        let slots = [
            (1u64, 0xAAAAu16, [0xA0u8; 16]),
            (2, 0xBBBB, [0xB0; 16]),
            (3, 0xCCCC, [0xC0; 16]),
        ];
        let blob = multi_epoch_keyset(1, &slots, 0x0BAD);
        let expected = multi_epoch_keyset(
            1,
            &[(9, 0x1234, [0x11; 16]), slots[1], slots[2]],
            0x0BAD,
        );
        assert_eq!(
            keyset_with_slot0(&blob, 9, 0x1234, &[0x11; 16]).unwrap(),
            expected
        );
        assert_eq!(keyset_next(&expected), Some(0x0BAD));
        assert_eq!(
            crate::kvs::parse_keyset_first_entry(&expected, 2).unwrap(),
            ([0x11; 16], Some(0x1234))
        );
    }

    #[test]
    fn keyset_with_slot0_touches_only_slot0_ctx456() {
        // keyset_with_next_preserves_unknown_tags_order_and_nested_slots と同じ
        // 「未知タグ / スロット内 ctx7 / ネスト」fixture。slot 0 の ctx4/5/6 以外は
        // 1 バイトも変わらない。
        fn build(start: u64, hash: u16, key: &[u8; 16]) -> Vec<u8> {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(7), 0x0BAD); // chain next（先頭）
            w.put_uint(Tag::Context(9), 42); // 未知の追加タグ
            w.put_uint(Tag::Context(1), 1); // policy
            w.put_uint(Tag::Context(2), 2); // keys_count
            w.start_array(Tag::Context(3));
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(4), start);
            w.put_uint(Tag::Context(5), u64::from(hash));
            w.put_bytes(Tag::Context(6), key);
            w.put_uint(Tag::Context(7), 99); // スロット内 ctx7
            w.start_struct(Tag::Context(8));
            w.put_uint(Tag::Context(0), 7);
            w.end_container();
            w.end_container();
            w.start_struct(Tag::Anonymous); // slot 1（無傷で残る）
            w.put_uint(Tag::Context(4), 2);
            w.put_uint(Tag::Context(5), 0x2222);
            w.put_bytes(Tag::Context(6), &[0x32; 16]);
            w.end_container();
            w.end_container(); // ctx3 array
            w.end_container(); // outer struct
            w.finish()
        }
        let blob = build(1, 0x1111, &[0x31; 16]);
        assert_eq!(
            keyset_with_slot0(&blob, 5, 0x5555, &[0x55; 16]).unwrap(),
            build(5, 0x5555, &[0x55; 16])
        );
    }

    #[test]
    fn keyset_with_slot0_rejects_broken_blobs() {
        assert!(keyset_with_slot0(&[0x15, 0x18], 1, 1, &[0; 16]).is_none());
        let blob = serialize_keyset(0, 1, 7, &[0x42; 16], 5);
        assert!(keyset_with_slot0(&blob[..blob.len() - 3], 1, 1, &[0; 16]).is_none());
        // ctx3 配列が無い struct
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0);
        w.put_uint(Tag::Context(7), 0xFFFF);
        w.end_container();
        assert!(keyset_with_slot0(&w.finish(), 1, 1, &[0; 16]).is_none());
    }

    #[test]
    fn write_keyset_reprovision_keeps_chiptool_policy_and_epochs() {
        // 同じ keyset id で 2 回 provision（re-provision）。間に chip-tool 形
        // （policy 1、3 epoch）へ差し替えておくと、2 回目は slot 0 だけ差し替わる。
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 1, 10, false);
        let slots = [
            (1u64, 0x1111u16, [0x31u8; 16]),
            (2_000_000, 0x2222, [0x32; 16]),
            (3_000_000, 0x3333, [0x33; 16]),
        ];
        {
            let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
            txn.set("f/2/k/a", &multi_epoch_keyset(1, &slots, INVALID_KEYSET_ID));
            txn.commit().unwrap();
        }
        write_group_provision(
            &p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: 1,
                keyset_id: 10,
                name: "e2e",
                epoch_key: [0x77; 16],
                rebind: true,
            },
        )
        .unwrap();
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        let op = derive_ipk_operational(&[0x77; 16], &CFID);
        let hash = derive_group_session_id(&op);
        assert_eq!(
            txn.get("f/2/k/a").unwrap().unwrap(),
            multi_epoch_keyset(
                1,
                &[(EPOCH_START_TIME, hash, op), slots[1], slots[2]],
                INVALID_KEYSET_ID
            )
        );
    }
```

`provision(&p, 1, 10, false)` は既存 test helper（`fn provision(p, group, keyset, rebind)`）。無ければ既存の同名ヘルパの引数順に合わせる。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller group_settings::tests::keyset_with_slot0 2>&1 | tail -5`
Expected: コンパイルエラー `cannot find function keyset_with_slot0`。

- [ ] **Step 3: ヘルパを実装し、`write_keyset` を差し替える**

`keyset_with_next` の直後に:

```rust
/// KeySetData の ctx3 配列・先頭 struct（slot 0）の ctx4 start_time / ctx5 hash /
/// ctx6 key だけを差し替えた blob を返す。policy / keys_count / 残スロット /
/// 未知タグ / ctx7 next は TLV 要素単位でそのまま写す（[`keyset_with_next`] と
/// 同型）— chip-tool `add-keysets` 由来の複数 epoch keyset を mat の 1 スロット形
/// に潰さずに鍵だけ置き換えるため（re-provision と IPK ローテーションの commit）。
/// 元の slot 0 に ctx4/5/6 のどれかが無ければ末尾に補う（mat / chip-tool どちらの
/// 書き手も 3 つ揃えるので実運用では起きない）。外側が struct でない / ctx3 が無い /
/// 先頭要素が struct でない / 途中で切れている blob は `None`。
pub(crate) fn keyset_with_slot0(
    blob: &[u8],
    start_time: u64,
    hash: u16,
    key: &[u8; 16],
) -> Option<Vec<u8>> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    let mut seen_slot0 = false;
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(3), Value::ArrayStart) if !seen_slot0 => {
                w.start_array(Tag::Context(3));
                if r.next().ok()??.value != Value::StructStart {
                    return None;
                }
                w.start_struct(Tag::Anonymous);
                let (mut put4, mut put5, mut put6) = (false, false, false);
                loop {
                    let e = r.next().ok()??;
                    match (e.tag, e.value) {
                        (_, Value::ContainerEnd) => break,
                        (Tag::Context(4), Value::Uint(_)) => {
                            w.put_uint(Tag::Context(4), start_time);
                            put4 = true;
                        }
                        (Tag::Context(5), Value::Uint(_)) => {
                            w.put_uint(Tag::Context(5), u64::from(hash));
                            put5 = true;
                        }
                        (Tag::Context(6), Value::Bytes(_)) => {
                            w.put_bytes(Tag::Context(6), key);
                            put6 = true;
                        }
                        (tag, value) => crate::tlv::copy_value(&mut w, &mut r, tag, value).ok()?,
                    }
                }
                if !put4 {
                    w.put_uint(Tag::Context(4), start_time);
                }
                if !put5 {
                    w.put_uint(Tag::Context(5), u64::from(hash));
                }
                if !put6 {
                    w.put_bytes(Tag::Context(6), key);
                }
                w.end_container();
                // 残りスロット（slot 1..）はそのまま写す。
                loop {
                    let e = r.next().ok()??;
                    match (e.tag, e.value) {
                        (_, Value::ContainerEnd) => break,
                        (tag, value) => crate::tlv::copy_value(&mut w, &mut r, tag, value).ok()?,
                    }
                }
                w.end_container();
                seen_slot0 = true;
            }
            (tag, value) => crate::tlv::copy_value(&mut w, &mut r, tag, value).ok()?,
        }
    }
    if !seen_slot0 {
        return None;
    }
    w.end_container();
    Some(w.finish())
}
```

`write_keyset` の既存 id 腕（`if cur == keyset_id { ... }`）を:

```rust
        if cur == keyset_id {
            let relinked = keyset_with_slot0(&blob, EPOCH_START_TIME, hash, operational)
                .ok_or_else(|| corrupt(&key, "unparseable KeySetData"))?;
            txn.set(&key, &relinked);
            return Ok(());
        }
```

`write_keyset` の doc コメントの「既存 keyset id への上書き（re-provision）は mat の 1 スロット形 … ヘルパを足すこと。」段落を次に差し替える:

```rust
/// 既存 keyset id への上書き（re-provision）は [`keyset_with_slot0`] で slot 0 の
/// 鍵・hash だけ差し替える — chip-tool `add-keysets` 由来の policy / 残スロットは
/// 落とさない（`unlink_keyset` のリンク差し替えと同じ無損失規律）。
```

- [ ] **Step 4: テストを通す**

Run: `cargo test -p mat-controller group_settings 2>&1 | tail -5`
Expected: 全 PASS（既存テストも含む）。

- [ ] **Step 5: fmt / clippy / コミット**

```bash
cargo fmt && cargo clippy -p mat-controller --all-targets -- -D warnings
git add crates/mat-controller/src/group_settings.rs
git commit -m "fix(group_settings): write_keyset の既存 id 上書きを keyset_with_slot0 で無損失化"
```

---

### Task 2: epoch スロットの読み出しとローテーション KVS トランザクション（mat-controller）

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`（`mat_ipk_epoch_key` 周辺）
- Modify: `crates/mat-controller/src/group_settings.rs`（`remove_group` の後に 3 関数 + tests）

**Interfaces:**
- Consumes: Task 1 の `keyset_with_slot0`、既存 `KvsTxn`、`derive_ipk_operational` / `derive_group_session_id`、`EPOCH_START_TIME`。
- Produces:
  - `kvs::IpkEpochSlot { Current, Next, Prev }`（`Debug, Clone, Copy, PartialEq, Eq`）
  - `kvs::mat_ipk_epoch_slot_key(fabric_index: u8, slot: IpkEpochSlot) -> String`
  - `kvs::read_mat_ipk_epoch_slot(main_ini: &Path, fabric_index: u8, slot: IpkEpochSlot) -> Result<Option<[u8; 16]>, KvsError>`
  - `group_settings::begin_ipk_rotation(main_ini: &Path, fabric_index: u8, next: &[u8; 16]) -> Result<(), GroupSettingsError>`
  - `group_settings::commit_ipk_rotation(main_ini: &Path, fabric_index: u8, cfid: &[u8; 8], cur: &[u8; 16], next: &[u8; 16]) -> Result<(), GroupSettingsError>`
  - `group_settings::abort_ipk_rotation(main_ini: &Path, fabric_index: u8) -> Result<bool, GroupSettingsError>`

- [ ] **Step 1: 失敗するテストを書く**

`kvs.rs` の `mod tests` 末尾:

```rust
    #[test]
    fn ipk_epoch_slot_keys_and_reads() {
        assert_eq!(mat_ipk_epoch_slot_key(2, IpkEpochSlot::Current), "mat/f/2/ipk-epoch");
        assert_eq!(mat_ipk_epoch_slot_key(2, IpkEpochSlot::Next), "mat/f/2/ipk-epoch-next");
        assert_eq!(mat_ipk_epoch_slot_key(2, IpkEpochSlot::Prev), "mat/f/2/ipk-epoch-prev");
        // 既存の別名はそのまま。
        assert_eq!(mat_ipk_epoch_key(2), "mat/f/2/ipk-epoch");
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        assert_eq!(read_mat_ipk_epoch_slot(&ini, 2, IpkEpochSlot::Next).unwrap(), None);
        let mut txn = KvsTxn::open(&ini).unwrap();
        txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Next), &[0x33; 16]);
        txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Prev), &[1, 2, 3]);
        txn.commit().unwrap();
        assert_eq!(
            read_mat_ipk_epoch_slot(&ini, 2, IpkEpochSlot::Next).unwrap(),
            Some([0x33; 16])
        );
        assert!(matches!(
            read_mat_ipk_epoch_slot(&ini, 2, IpkEpochSlot::Prev),
            Err(KvsError::BadKeyset { .. })
        ));
        assert_eq!(read_mat_ipk_epoch(&ini, 2).unwrap(), None);
    }
```

`group_settings.rs` の `mod tests` 末尾:

```rust
    /// rotation テスト用: chip-tool 3 epoch 形の k/0 と mat epoch キーを持つ INI。
    fn rotation_fixture(cur: &[u8; 16]) -> (tempfile::TempDir, std::path::PathBuf, [(u64, u16, [u8; 16]); 3]) {
        let (d, p) = tmp_ini("[Default]\n");
        let op = derive_ipk_operational(cur, &CFID);
        let slots = [
            (EPOCH_START_TIME, derive_group_session_id(&op), op),
            (2_000_000, 0x2222, [0x32; 16]),
            (3_000_000, 0x3333, [0x33; 16]),
        ];
        let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
        txn.set("f/2/k/0", &multi_epoch_keyset(0, &slots, INVALID_KEYSET_ID));
        txn.set(&crate::kvs::mat_ipk_epoch_key(2), cur);
        txn.commit().unwrap();
        (d, p, slots)
    }

    #[test]
    fn ipk_rotation_begin_commit_round_trip() {
        use crate::kvs::{read_mat_ipk_epoch_slot, IpkEpochSlot};
        let cur = [0x0C; 16];
        let next = [0x0E; 16];
        let (_d, p, slots) = rotation_fixture(&cur);

        begin_ipk_rotation(&p, 2, &next).unwrap();
        assert_eq!(read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Next).unwrap(), Some(next));
        // 同じ値での begin は冪等、違う値は Corrupt（並行実行）。
        begin_ipk_rotation(&p, 2, &next).unwrap();
        assert!(matches!(
            begin_ipk_rotation(&p, 2, &[0x0F; 16]),
            Err(GroupSettingsError::Corrupt { .. })
        ));

        commit_ipk_rotation(&p, 2, &CFID, &cur, &next).unwrap();
        assert_eq!(read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Current).unwrap(), Some(next));
        assert_eq!(read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Prev).unwrap(), Some(cur));
        assert_eq!(read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Next).unwrap(), None);
        let op_next = derive_ipk_operational(&next, &CFID);
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        assert_eq!(
            txn.get("f/2/k/0").unwrap().unwrap(),
            multi_epoch_keyset(
                0,
                &[(EPOCH_START_TIME, derive_group_session_id(&op_next), op_next), slots[1], slots[2]],
                INVALID_KEYSET_ID
            ),
            "k/0: slot 0 だけ新 operational、残りスロットと next リンクは無傷"
        );
        // 読み側（CASE 用）も新 operational を見る。
        assert_eq!(
            crate::kvs::parse_keyset_first_entry(&txn.get("f/2/k/0").unwrap().unwrap(), 2).unwrap().0,
            op_next
        );
    }

    #[test]
    fn ipk_rotation_commit_refuses_mismatch_and_missing_state_without_writing() {
        let cur = [0x0C; 16];
        let next = [0x0E; 16];
        let (_d, p, _) = rotation_fixture(&cur);
        let before = std::fs::read(&p).unwrap();
        // pending 無し
        assert!(matches!(
            commit_ipk_rotation(&p, 2, &CFID, &cur, &next),
            Err(GroupSettingsError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&p).unwrap(), before);
        // pending と違う next
        begin_ipk_rotation(&p, 2, &next).unwrap();
        let before = std::fs::read(&p).unwrap();
        assert!(matches!(
            commit_ipk_rotation(&p, 2, &CFID, &cur, &[0x0F; 16]),
            Err(GroupSettingsError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&p).unwrap(), before);
        // k/0 欠落
        {
            let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
            txn.remove("f/2/k/0");
            txn.commit().unwrap();
        }
        let before = std::fs::read(&p).unwrap();
        assert!(matches!(
            commit_ipk_rotation(&p, 2, &CFID, &cur, &next),
            Err(GroupSettingsError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&p).unwrap(), before);
    }

    #[test]
    fn ipk_rotation_abort_removes_pending_only() {
        use crate::kvs::{read_mat_ipk_epoch_slot, IpkEpochSlot};
        let cur = [0x0C; 16];
        let (_d, p, _) = rotation_fixture(&cur);
        assert!(!abort_ipk_rotation(&p, 2).unwrap(), "pending 無しは false");
        begin_ipk_rotation(&p, 2, &[0x0E; 16]).unwrap();
        assert!(abort_ipk_rotation(&p, 2).unwrap());
        assert_eq!(read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Next).unwrap(), None);
        assert_eq!(read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Current).unwrap(), Some(cur));
    }

    #[test]
    fn ipk_rotation_locked_kvs_is_hard_error() {
        let (_d, p, _) = rotation_fixture(&[0x0C; 16]);
        let _held = crate::kvs::KvsTxn::open(&p).unwrap();
        assert!(matches!(
            begin_ipk_rotation(&p, 2, &[0x0E; 16]),
            Err(GroupSettingsError::Kvs(KvsError::Locked))
        ));
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller ipk_epoch_slot ipk_rotation 2>&1 | tail -5`
Expected: コンパイルエラー（`IpkEpochSlot` / `begin_ipk_rotation` 未定義）。

- [ ] **Step 3: 実装**

`kvs.rs` — `mat_ipk_epoch_key` を次のブロックで置き換える（`read_mat_ipk_epoch` / `write_mat_ipk_epoch` は残す）:

```rust
/// mat 専用の epoch IPK 永続キーのスロット（M8c-3 の `ipk-epoch` に、IPK
/// ローテーション用の `-next`（配布中の新 epoch = pending）/ `-prev`（直前の
/// epoch、取り残しノードの catch-up 用）を加えたもの）。chip-tool の名前空間
/// （`f/<idx>/...` / `g/...`）と衝突しない `mat/` プレフィクス。値は 16 バイトの
/// epoch 鍵の base64。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpkEpochSlot {
    Current,
    Next,
    Prev,
}

impl IpkEpochSlot {
    fn suffix(self) -> &'static str {
        match self {
            IpkEpochSlot::Current => "ipk-epoch",
            IpkEpochSlot::Next => "ipk-epoch-next",
            IpkEpochSlot::Prev => "ipk-epoch-prev",
        }
    }
}

pub fn mat_ipk_epoch_slot_key(fabric_index: u8, slot: IpkEpochSlot) -> String {
    format!("mat/f/{fabric_index}/{}", slot.suffix())
}

/// `mat_ipk_epoch_slot_key(fabric_index, Current)` の別名（既存呼び手用）。
pub fn mat_ipk_epoch_key(fabric_index: u8) -> String {
    mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Current)
}

/// 指定スロットの epoch を読む。キー無し = `Ok(None)`。16 バイト以外は
/// `KvsError::BadKeyset`。
pub fn read_mat_ipk_epoch_slot(
    main_ini: &Path,
    fabric_index: u8,
    slot: IpkEpochSlot,
) -> Result<Option<[u8; 16]>, KvsError> {
    let text = std::fs::read_to_string(main_ini).map_err(KvsError::Io)?;
    let sec = default_section(&text).ok_or(KvsError::SectionMissing)?;
    match decode_b64(sec, &mat_ipk_epoch_slot_key(fabric_index, slot))? {
        None => Ok(None),
        Some(v) => {
            let arr: [u8; 16] = v.try_into().map_err(|_| KvsError::BadKeyset {
                fabric_index,
                reason: "mat ipk epoch must be 16 bytes",
            })?;
            Ok(Some(arr))
        }
    }
}
```

`read_mat_ipk_epoch` の本体は `read_mat_ipk_epoch_slot(main_ini, fabric_index, IpkEpochSlot::Current)` に委譲する。

`group_settings.rs` — `remove_group` の後に:

```rust
/// IPK ローテーションの pending 開始: `mat/f/<idx>/ipk-epoch-next` を書く。既に
/// 同じ値なら no-op、別の値なら `Corrupt`（呼び手は事前に read して resume 判定
/// する前提なので、違う値に出会うのは並行実行の証拠）。
pub fn begin_ipk_rotation(
    main_ini: &Path,
    fabric_index: u8,
    next: &[u8; 16],
) -> Result<(), GroupSettingsError> {
    let mut txn = KvsTxn::open(main_ini)?;
    let key = mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Next);
    match txn.get(&key)? {
        Some(existing) if existing.as_slice() == next => return Ok(()),
        Some(_) => {
            return Err(corrupt(
                &key,
                "a different ipk rotation is already pending (concurrent rotate-ipk?)",
            ))
        }
        None => {}
    }
    txn.set(&key, next);
    txn.commit()?;
    Ok(())
}

/// IPK ローテーションの commit（1 KvsTxn）: `f/<idx>/k/0` の slot 0 を
/// `derive(next)` に差し替え（[`keyset_with_slot0`] — policy / 残スロット / next
/// リンクは無傷）、`ipk-epoch := next`、`ipk-epoch-prev := cur`、`ipk-epoch-next`
/// を削除。pending が無い / 値が `next` と違う / k/0 が無い・解釈不能は `Corrupt`
/// で何も書かない。
pub fn commit_ipk_rotation(
    main_ini: &Path,
    fabric_index: u8,
    cfid: &[u8; 8],
    cur: &[u8; 16],
    next: &[u8; 16],
) -> Result<(), GroupSettingsError> {
    let mut txn = KvsTxn::open(main_ini)?;
    let next_key = mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Next);
    match txn.get(&next_key)? {
        Some(v) if v.as_slice() == next => {}
        Some(_) => {
            return Err(corrupt(
                &next_key,
                "pending ipk epoch differs from the one being committed (concurrent rotate-ipk?)",
            ))
        }
        None => return Err(corrupt(&next_key, "no ipk rotation pending")),
    }
    let k0 = format!("f/{fabric_index}/k/0");
    let blob = txn
        .get(&k0)?
        .ok_or_else(|| corrupt(&k0, "missing IPK keyset record"))?;
    let operational = derive_ipk_operational(next, cfid);
    let hash = derive_group_session_id(&operational);
    let rewritten = keyset_with_slot0(&blob, EPOCH_START_TIME, hash, &operational)
        .ok_or_else(|| corrupt(&k0, "unparseable KeySetData"))?;
    txn.set(&k0, &rewritten);
    txn.set(&mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Current), next);
    txn.set(&mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Prev), cur);
    txn.remove(&next_key);
    txn.commit()?;
    Ok(())
}

/// pending の IPK ローテーションを取り消す（`ipk-epoch-next` を消すだけ）。
/// pending が無ければ `Ok(false)`（ファイルは触らない）。
pub fn abort_ipk_rotation(main_ini: &Path, fabric_index: u8) -> Result<bool, GroupSettingsError> {
    let mut txn = KvsTxn::open(main_ini)?;
    let key = mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Next);
    if txn.get(&key)?.is_none() {
        return Ok(false);
    }
    txn.remove(&key);
    txn.commit()?;
    Ok(true)
}
```

先頭の `use crate::kvs::{KvsError, KvsTxn};` を `use crate::kvs::{mat_ipk_epoch_slot_key, IpkEpochSlot, KvsError, KvsTxn};` に。ファイル先頭のモジュール doc に 1 文追加: 「IPK ローテーション（`begin_ipk_rotation` / `commit_ipk_rotation` / `abort_ipk_rotation`）も同じ 1 KvsTxn 規律で書く。」

- [ ] **Step 4: テストを通す**

Run: `cargo test -p mat-controller 2>&1 | tail -5`
Expected: 全 PASS。

- [ ] **Step 5: fmt / clippy / コミット**

```bash
cargo fmt && cargo clippy -p mat-controller --all-targets -- -D warnings
git add crates/mat-controller/src/kvs.rs crates/mat-controller/src/group_settings.rs
git commit -m "feat(kvs): IPK epoch の next/prev スロットと rotation の begin/commit/abort トランザクション"
```

---

### Task 3: 複数 epoch の KeySetWrite 符号化と `ops::write_ipk_keyset`

**Files:**
- Modify: `crates/mat-controller/src/im/cmdfields.rs`（`encode_key_set_write_fields` + tests）
- Modify: `crates/mat-native/src/ops.rs`（`provision_node` の前に `write_ipk_keyset`、tests）

**Interfaces:**
- Consumes: `mat_controller::im::{CLUSTER_GROUP_KEY_MANAGEMENT, CMD_KEY_SET_WRITE}`、`crate::NodeConn::invoke`、`crate::test_support::FakeConn`。
- Produces:
  - `mat_controller::im::encode_key_set_write_fields_multi(keyset_id: u16, epochs: &[([u8; 16], u64)]) -> Vec<u8>`
  - `mat_native::ops::IPK_KEYSET_ID: u16 = 0`
  - `mat_native::ops::write_ipk_keyset(conn: &mut dyn NodeConn, epochs: &[([u8; 16], u64)]) -> Result<(), MatError>`（Task 5 が使う）

- [ ] **Step 1: 失敗するテストを書く**

`cmdfields.rs` の tests（無ければ `#[cfg(test)] mod tests { use super::*; use crate::tlv::{Reader, Tag, Value}; ... }` を作る）:

```rust
    #[test]
    fn key_set_write_single_epoch_is_the_multi_form_with_one_key() {
        assert_eq!(
            encode_key_set_write_fields(42, &[0xAB; 16]),
            encode_key_set_write_fields_multi(42, &[([0xAB; 16], 1)])
        );
    }

    #[test]
    fn key_set_write_multi_fills_epoch_slots_in_order_and_nulls_the_rest() {
        let tlv = encode_key_set_write_fields_multi(0, &[([0x0C; 16], 1), ([0x0E; 16], 2)]);
        // {0: GroupKeySet{0: id, 1: policy, 2: key0, 3: start0, 4: key1, 5: start1, 6: null, 7: null}}
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let el = r.next().unwrap().unwrap();
        assert_eq!((el.tag, el.value), (Tag::Context(0), Value::StructStart));
        let mut seen = Vec::new();
        loop {
            let el = r.next().unwrap().unwrap();
            if el.value == Value::ContainerEnd {
                break;
            }
            seen.push((el.tag, el.value));
        }
        assert_eq!(
            seen,
            vec![
                (Tag::Context(0), Value::Uint(0)),
                (Tag::Context(1), Value::Uint(0)),
                (Tag::Context(2), Value::Bytes(&[0x0C; 16])),
                (Tag::Context(3), Value::Uint(1)),
                (Tag::Context(4), Value::Bytes(&[0x0E; 16])),
                (Tag::Context(5), Value::Uint(2)),
                (Tag::Context(6), Value::Null),
                (Tag::Context(7), Value::Null),
            ]
        );
    }
```

`Value::Bytes` / `Value::Null` の正確な variant 名は `crates/mat-controller/src/tlv.rs` の `pub enum Value` を見て合わせる。

`ops.rs` の `mod tests`:

```rust
    #[tokio::test]
    async fn write_ipk_keyset_invokes_key_set_write_on_ep0_with_keyset_0() {
        let mut conn = FakeConn::scripted();
        write_ipk_keyset(&mut conn, &[([0x0C; 16], 1), ([0x0E; 16], 2)])
            .await
            .unwrap();
        assert_eq!(conn.calls(), &["invoke(0,0x003F,0x0000)".to_string()]);
    }

    #[tokio::test]
    async fn write_ipk_keyset_prefixes_error_with_step_name() {
        let mut conn = FakeConn {
            fail_first_send: true,
            fail_kind: ErrorKind::DeviceRejected,
            ..FakeConn::scripted()
        };
        let err = write_ipk_keyset(&mut conn, &[([0x0C; 16], 1)]).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
        assert!(err.detail.starts_with("key-set-write (ipk): "), "{}", err.detail);
    }
```

`FakeConn::calls()` の文字列書式は `test_support.rs` の `invoke` 実装（`format!("invoke({endpoint},{cluster:#06X},{command:#06X})")`）に合わせる。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller cmdfields 2>&1 | tail -3; cargo test -p mat-native write_ipk_keyset 2>&1 | tail -3`
Expected: 未定義エラー。

- [ ] **Step 3: 実装**

`cmdfields.rs` の `encode_key_set_write_fields` を置き換え:

```rust
/// `KeySetWrite` CommandFields（spec §11.2.8.1）: `{0: GroupKeySet{0: id, 1: policy
/// TrustFirst, 2/3: epochKey0/start0, 4/5: key1/start1, 6/7: key2/start2}}`。
/// `epochs` は (epoch_key, start_time) を 1〜3 本 — start_time は呼び手が単調増加
/// かつ非 0 を保証する（`EpochStartTime0 == 0` はデバイス側で INVALID_COMMAND）。
/// 無い本数は null。4 本以上は呼び手のバグ（debug_assert、release は先頭 3 本）。
pub fn encode_key_set_write_fields_multi(keyset_id: u16, epochs: &[([u8; 16], u64)]) -> Vec<u8> {
    debug_assert!(
        (1..=3).contains(&epochs.len()),
        "KeySetWrite carries 1..=3 epoch keys"
    );
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(0)); // GroupKeySet
    w.put_uint(Tag::Context(0), u64::from(keyset_id));
    w.put_uint(Tag::Context(1), 0); // GroupKeySecurityPolicy: TrustFirst
    for i in 0..3u8 {
        let key_tag = Tag::Context(2 + 2 * i);
        let start_tag = Tag::Context(3 + 2 * i);
        match epochs.get(usize::from(i)) {
            Some((key, start)) => {
                w.put_bytes(key_tag, key);
                w.put_uint(start_tag, *start);
            }
            None => {
                w.put_null(key_tag);
                w.put_null(start_tag);
            }
        }
    }
    w.end_container();
    w.end_container();
    w.finish()
}

/// epoch key 1 本（EpochStartTime0 = 1）の `KeySetWrite`。`group provision` が使う
/// 形で、[`encode_key_set_write_fields_multi`] の 1 本版。
pub fn encode_key_set_write_fields(keyset_id: u16, epoch_key: &[u8; 16]) -> Vec<u8> {
    encode_key_set_write_fields_multi(keyset_id, &[(*epoch_key, 1)])
}
```

`ops.rs` — import に `encode_key_set_write_fields_multi` を足し、`provision_node` の直前に:

```rust
/// IPK の KeySet id（spec §11.2.6.2）。
pub const IPK_KEYSET_ID: u16 = 0;

/// IPK keyset（keyset 0）へ `KeySetWrite` を 1 回打つ — `fabric rotate-ipk` の
/// 配布 / catch-up の 1 ステップ。`epochs` は (epoch_key, start_time) 1〜3 本、
/// start_time は単調増加かつ非 0（spec §11.2.8.1）。ep0、timed 無し。失敗は
/// detail に `key-set-write (ipk): ` を前置。
pub async fn write_ipk_keyset(
    conn: &mut dyn NodeConn,
    epochs: &[([u8; 16], u64)],
) -> Result<(), MatError> {
    let fields = encode_key_set_write_fields_multi(IPK_KEYSET_ID, epochs);
    conn.invoke(
        0,
        CLUSTER_GROUP_KEY_MANAGEMENT,
        CMD_KEY_SET_WRITE,
        Some(fields),
        false,
    )
    .await
    .map_err(|e| MatError::new(e.kind, format!("key-set-write (ipk): {}", e.detail)))
}
```

- [ ] **Step 4: テストを通す**

Run: `cargo test -p mat-controller 2>&1 | tail -3; cargo test -p mat-native 2>&1 | tail -3`
Expected: 全 PASS（provision の既存 KeySetWrite バイト列テストも通る）。

- [ ] **Step 5: fmt / clippy / コミット**

```bash
cargo fmt && cargo clippy -p mat-controller -p mat-native --all-targets -- -D warnings
git add crates/mat-controller/src/im/cmdfields.rs crates/mat-native/src/ops.rs
git commit -m "feat(im): KeySetWrite の複数 epoch 符号化と ops::write_ipk_keyset"
```

---

### Task 4: 資格情報の読出しと確立器生成の切り出し（mat-native lib.rs / commission.rs）

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（`NativeConfig` に `Clone`、`Engine::build_with_resolver` の前半・後半を関数化）
- Modify: `crates/mat-native/src/commission.rs`（`resolve_ipk_epoch` を `pub(crate)`）

**Interfaces:**
- Produces:
  - `pub fn load_fabric_credentials(cfg: &NativeConfig) -> Result<FabricCredentials, MatError>`
  - `pub fn case_establisher(cfg: &NativeConfig, creds: FabricCredentials, resolver: Arc<dyn Resolver>) -> Result<Box<dyn Establisher>, MatError>`
  - `pub(crate) fn commission::resolve_ipk_epoch(main_ini: &Path, fabric_index: u8, creds: &FabricCredentials) -> Result<[u8; 16], MatError>`
  - `NativeConfig: Clone`

- [ ] **Step 1: 失敗するテストを書く**

`lib.rs` の既存 `mod tests`（`Engine::build` のテストがある場所）に:

```rust
    #[test]
    fn load_fabric_credentials_maps_missing_store_to_store_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = NativeConfig {
            store: dir.path().to_path_buf(),
            iface: "lo".into(),
            thread_iface: None,
            fabric_index: 1,
            issuer_index: 0,
        };
        let err = load_fabric_credentials(&cfg).unwrap_err();
        assert_eq!(err.kind, ErrorKind::StoreMissing);
        // Clone できる（rotate-ipk が確立器生成クロージャへ move する）。
        let _ = cfg.clone();
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-native load_fabric_credentials 2>&1 | tail -3`
Expected: 未定義エラー。

- [ ] **Step 3: 実装**

`NativeConfig` に `#[derive(Clone)]`。`Engine::build_with_resolver` の先頭（`alpha_ini` 〜 `let creds = FabricCredentials::from_self_issued(...)` まで）を関数に切り出す:

```rust
/// KVS から fabric 資格情報を組み立てる（`Engine::build` の前半）。`fabric
/// rotate-ipk` も同じ経路で読む（epoch を差し替えた別 IPK の確立器を作るため）。
/// KVS 読み取り失敗は一律 `store_missing`、NOC 自己発行の失敗は `store_parse`。
pub fn load_fabric_credentials(cfg: &NativeConfig) -> Result<FabricCredentials, MatError> {
    let alpha_ini = cfg.store.join("chip_tool_config.alpha.ini");
    let main_ini = cfg.store.join(mat_controller::kvs::MAIN_INI_FILE);
    let materials = mat_controller::kvs::read_self_issue_materials(
        &alpha_ini,
        &main_ini,
        cfg.fabric_index,
        cfg.issuer_index,
    )
    .map_err(|e| {
        MatError::new(
            ErrorKind::StoreMissing,
            format!("native: read KVS credentials: {e}"),
        )
    })?;
    FabricCredentials::from_self_issued(materials).map_err(|e| {
        MatError::new(
            ErrorKind::StoreParse,
            format!("native: self-issue NOC: {e}"),
        )
    })
}

/// 資格情報から実確立器（mDNS 解決 → CASE）を作る（`Engine::build` の後半）。
/// `creds.ipk_operational` を差し替えて渡せば別 epoch の IPK で CASE を張る
/// 確立器になる（rotate-ipk の受理実証）。
pub fn case_establisher(
    cfg: &NativeConfig,
    creds: FabricCredentials,
    resolver: Arc<dyn Resolver>,
) -> Result<Box<dyn Establisher>, MatError> {
    let scope_id = mat_controller::dnssd::iface_index(&cfg.iface).map_err(|e| {
        MatError::new(
            ErrorKind::Other,
            format!("native: resolve iface {:?} index: {e}", cfg.iface),
        )
    })?;
    Ok(Box::new(CaseEstablisher {
        creds: Arc::new(creds),
        scope_id,
        resolver,
    }))
}
```

`build_with_resolver` は `let creds = load_fabric_credentials(cfg)?;` で始め、既存の `scope_id` 計算（egress 用）はそのまま残し、末尾の `let establisher = CaseEstablisher { ... }` を `let establisher = case_establisher(cfg, creds, resolver)?;` に置き換えて `Ok(Self { establisher, group: Some(group), group_settings: Some(group_settings) })`。`main_ini` の変数は `group_settings` / `group` が使うので残す。

`commission.rs`: `fn resolve_ipk_epoch(` → `pub(crate) fn resolve_ipk_epoch(`。

- [ ] **Step 4: テストを通す**

Run: `cargo test -p mat-native 2>&1 | tail -3 && cargo test -p matd 2>&1 | tail -3`
Expected: 全 PASS（`Engine::build` 既存テストの store_missing / store_parse 分類は不変）。

- [ ] **Step 5: fmt / clippy / コミット**

```bash
cargo fmt && cargo clippy -p mat-native -p matd --all-targets -- -D warnings
git add crates/mat-native/src/lib.rs crates/mat-native/src/commission.rs
git commit -m "refactor(native): 資格情報読出しと確立器生成を Engine::build から切り出し"
```

---

### Task 5: `mat-native::rotate_ipk` — 状態機械（配布・実証・commit・catch-up・abort）

**Files:**
- Create: `crates/mat-native/src/rotate_ipk.rs`
- Modify: `crates/mat-native/src/lib.rs`（`pub mod rotate_ipk;`）

**Interfaces:**
- Consumes: Task 2 の `kvs::{IpkEpochSlot, read_mat_ipk_epoch_slot, MAIN_INI_FILE}` / `group_settings::{begin_ipk_rotation, commit_ipk_rotation, abort_ipk_rotation}`、Task 3 の `ops::write_ipk_keyset`、Task 4 の `load_fabric_credentials` / `case_establisher` / `commission::resolve_ipk_epoch`、`mat_core::group::generate_epoch_key` + `ops::epoch_key_from_hex`、`crate::Establisher`。
- Produces:
  - `pub enum RotateMode { Rotate, CatchUp, Abort }`
  - `pub struct RotateIpkParams { pub node_ids: Vec<u64>, pub mode: RotateMode, pub per_node_timeout_ms: u64 }`
  - `pub enum RotateStatus { Rotated, Pending, CaughtUp, CatchUpIncomplete, Aborted, Idle }` + `as_str()`
  - `pub struct NodeOutcome { pub node_id: u64, pub error: Option<MatError> }`
  - `pub struct RotateOutcome { pub status: RotateStatus, pub nodes: Vec<NodeOutcome> }` + `body(&self, fabric_index: u8) -> serde_json::Value` + `partial_error(&self) -> Option<MatError>`
  - `pub struct RotateCtx { ... }`（テスト注入用）+ `pub async fn run_with(ctx: &RotateCtx, p: &RotateIpkParams) -> Result<RotateOutcome, MatError>`
  - `pub async fn run(cfg: &NativeConfig, p: &RotateIpkParams) -> Result<RotateOutcome, MatError>`（Task 6 の CLI が呼ぶ）

- [ ] **Step 1: 失敗するテストを書く**

`rotate_ipk.rs` を、まず tests だけを持つ形で作る（本体は Step 3）。tests 部分:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use mat_controller::fabric::{derive_group_session_id, derive_ipk_operational};
    use mat_controller::kvs::{mat_ipk_epoch_key, mat_ipk_epoch_slot_key, KvsTxn, MAIN_INI_FILE};
    use mat_controller::tlv::{Tag, Writer};
    use mat_core::error::ErrorKind;

    /// controller KVS の `f/<idx>/k/0`（mat 1 スロット形、`group_settings::
    /// serialize_keyset(0, 1, hash, key, 0xFFFF)` と同じバイト列）。
    fn ipk_keyset_blob(hash: u16, key: &[u8; 16]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0); // policy
        w.put_uint(Tag::Context(2), 1); // keys_count
        w.start_array(Tag::Context(3));
        for i in 0..3 {
            w.start_struct(Tag::Anonymous);
            if i == 0 {
                w.put_uint(Tag::Context(4), 1);
                w.put_uint(Tag::Context(5), u64::from(hash));
                w.put_bytes(Tag::Context(6), key);
            } else {
                w.put_uint(Tag::Context(4), 0);
                w.put_uint(Tag::Context(5), 0);
                w.put_bytes(Tag::Context(6), &[0u8; 16]);
            }
            w.end_container();
        }
        w.end_container();
        w.put_uint(Tag::Context(7), 0xFFFF);
        w.end_container();
        w.finish()
    }

    fn k0(h: &Harness) -> Vec<u8> {
        KvsTxn::open(&h.ctx.main_ini).unwrap().get("f/2/k/0").unwrap().unwrap()
    }

    use crate::test_support::FakeConn;

    const CFID: [u8; 8] = [7; 8];
    const CUR: [u8; 16] = [0x0C; 16];

    /// ノード id ごとに establish の成否を決め、払い出した conn の invoke 失敗も
    /// ノードごとに指定できる fake。`by_epoch` で「どの epoch 用の確立器か」を
    /// 記録し、テストが「E_cur で書いて E_next で実証した」順序を主張できる。
    struct NodeFake {
        label: &'static str,
        establish_fail: HashMap<u64, ErrorKind>,
        invoke_fail: HashMap<u64, ErrorKind>,
        log: std::sync::Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Establisher for NodeFake {
        async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            self.log.lock().unwrap().push(format!("{}:establish:{node_id}", self.label));
            if let Some(kind) = self.establish_fail.get(&node_id) {
                return Err(MatError::new(*kind, format!("fake establish failed for {node_id}")));
            }
            let fail = self.invoke_fail.get(&node_id).copied();
            Ok(Box::new(FakeConn {
                fail_first_send: fail.is_some(),
                fail_kind: fail.unwrap_or(ErrorKind::Timeout),
                ..FakeConn::scripted()
            }))
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        ctx: RotateCtx,
        log: std::sync::Arc<Mutex<Vec<String>>>,
        made: std::sync::Arc<AtomicUsize>,
    }

    /// k/0（mat 1 スロット形）+ ipk-epoch=CUR の INI を持つ RotateCtx。
    /// `establish_fail` / `invoke_fail` は全確立器に共通で適用する。
    fn harness(
        establish_fail: HashMap<u64, ErrorKind>,
        invoke_fail: HashMap<u64, ErrorKind>,
    ) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let main_ini = dir.path().join(MAIN_INI_FILE);
        std::fs::write(&main_ini, "[Default]\n").unwrap();
        let op = derive_ipk_operational(&CUR, &CFID);
        let mut txn = KvsTxn::open(&main_ini).unwrap();
        txn.set("f/2/k/0", &ipk_keyset_blob(derive_group_session_id(&op), &op));
        txn.set(&mat_ipk_epoch_key(2), &CUR);
        txn.commit().unwrap();

        let log = std::sync::Arc::new(Mutex::new(Vec::new()));
        let made = std::sync::Arc::new(AtomicUsize::new(0));
        let (log2, made2) = (std::sync::Arc::clone(&log), std::sync::Arc::clone(&made));
        let ctx = RotateCtx {
            main_ini,
            fabric_index: 2,
            cfid: CFID,
            cur_epoch: CUR,
            make_establisher: Box::new(move |epoch: &[u8; 16]| {
                made2.fetch_add(1, Ordering::SeqCst);
                // どの epoch 用かはログのラベルで見分ける。CUR 以外は "other"。
                let label = if *epoch == CUR { "cur" } else { "other" };
                Ok(Box::new(NodeFake {
                    label,
                    establish_fail: establish_fail.clone(),
                    invoke_fail: invoke_fail.clone(),
                    log: std::sync::Arc::clone(&log2),
                }) as Box<dyn Establisher>)
            }),
        };
        Harness { _dir: dir, ctx, log, made }
    }

    fn slot(h: &Harness, s: IpkEpochSlot) -> Option<[u8; 16]> {
        read_mat_ipk_epoch_slot(&h.ctx.main_ini, 2, s).unwrap()
    }

    fn params(nodes: &[u64], mode: RotateMode) -> RotateIpkParams {
        RotateIpkParams { node_ids: nodes.to_vec(), mode, per_node_timeout_ms: 0 }
    }

    #[tokio::test]
    async fn rotate_all_ok_commits_and_records_prev() {
        let h = harness(HashMap::new(), HashMap::new());
        let out = run_with(&h.ctx, &params(&[5, 6], RotateMode::Rotate)).await.unwrap();
        assert_eq!(out.status, RotateStatus::Rotated);
        assert!(out.nodes.iter().all(|n| n.error.is_none()));
        assert_eq!(
            h.log.lock().unwrap().as_slice(),
            &[
                "cur:establish:5", "other:establish:5",
                "cur:establish:6", "other:establish:6",
            ]
        );
        let next = slot(&h, IpkEpochSlot::Current).unwrap();
        assert_ne!(next, CUR);
        assert_eq!(slot(&h, IpkEpochSlot::Prev), Some(CUR));
        assert_eq!(slot(&h, IpkEpochSlot::Next), None);
        let op_next = derive_ipk_operational(&next, &CFID);
        assert_eq!(k0(&h), ipk_keyset_blob(derive_group_session_id(&op_next), &op_next));
        let body = out.body(2);
        assert_eq!(body["status"], "rotated");
        assert_eq!(body["nodes"][0], serde_json::json!({"node_id": 5, "status": "ok"}));
        assert!(body["note"].as_str().unwrap().contains("restart"));
        assert!(out.partial_error().is_none());
        // 鍵素材は body に出ない。
        let next_hex: String = next.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!body.to_string().to_lowercase().contains(&next_hex));
    }

    #[tokio::test]
    async fn rotate_with_one_failure_stays_pending_and_is_resumable() {
        let h = harness(HashMap::from([(6u64, ErrorKind::Unreachable)]), HashMap::new());
        let out = run_with(&h.ctx, &params(&[5, 6, 7], RotateMode::Rotate)).await.unwrap();
        assert_eq!(out.status, RotateStatus::Pending);
        assert_eq!(out.nodes.len(), 3, "失敗しても続行して全ノードを回る");
        assert!(out.nodes[0].error.is_none() && out.nodes[2].error.is_none());
        let e6 = out.nodes[1].error.as_ref().unwrap();
        assert_eq!(e6.kind, ErrorKind::Unreachable);
        assert!(e6.detail.starts_with("node 6: establish: "), "{}", e6.detail);
        // KVS: next だけ書かれ、current / k/0 は不変。
        let next = slot(&h, IpkEpochSlot::Next).unwrap();
        assert_eq!(slot(&h, IpkEpochSlot::Current), Some(CUR));
        assert_eq!(slot(&h, IpkEpochSlot::Prev), None);
        let op_cur = derive_ipk_operational(&CUR, &CFID);
        assert_eq!(k0(&h), ipk_keyset_blob(derive_group_session_id(&op_cur), &op_cur));
        // stderr 用エラー: 最初に失敗したノードの kind、失敗ノード列挙。
        let pe = out.partial_error().unwrap();
        assert_eq!(pe.kind, ErrorKind::Unreachable);
        assert!(pe.detail.contains("1 of 3 nodes failed") && pe.detail.contains("node 6"), "{}", pe.detail);
        assert_eq!(out.body(2)["nodes"][1]["error"]["kind"], "unreachable");

        // resume: 同じ next を使い（新鍵を作らない）、成功で commit。
        let Harness { _dir, ctx, log, made } = h;
        let h2 = Harness { _dir, ctx: harness_ctx_reusing(&ctx), log, made };
        let out = run_with(&h2.ctx, &params(&[6], RotateMode::Rotate)).await.unwrap();
        assert_eq!(out.status, RotateStatus::Rotated);
        assert_eq!(slot(&h2, IpkEpochSlot::Current), Some(next));
        assert_eq!(slot(&h2, IpkEpochSlot::Prev), Some(CUR));
    }

    /// 同じ INI を指す、失敗設定なしの RotateCtx（resume テスト用）。
    fn harness_ctx_reusing(prev: &RotateCtx) -> RotateCtx {
        let log = std::sync::Arc::new(Mutex::new(Vec::new()));
        RotateCtx {
            main_ini: prev.main_ini.clone(),
            fabric_index: prev.fabric_index,
            cfid: prev.cfid,
            cur_epoch: prev.cur_epoch,
            make_establisher: Box::new(move |epoch: &[u8; 16]| {
                let label = if *epoch == CUR { "cur" } else { "other" };
                Ok(Box::new(NodeFake {
                    label,
                    establish_fail: HashMap::new(),
                    invoke_fail: HashMap::new(),
                    log: std::sync::Arc::clone(&log),
                }) as Box<dyn Establisher>)
            }),
        }
    }

    #[tokio::test]
    async fn rotate_key_set_write_rejection_is_failed_node() {
        let h = harness(HashMap::new(), HashMap::from([(5u64, ErrorKind::DeviceRejected)]));
        let out = run_with(&h.ctx, &params(&[5], RotateMode::Rotate)).await.unwrap();
        assert_eq!(out.status, RotateStatus::Pending);
        let e = out.nodes[0].error.as_ref().unwrap();
        assert_eq!(e.kind, ErrorKind::DeviceRejected);
        assert!(e.detail.starts_with("node 5: key-set-write (ipk): "), "{}", e.detail);
        // 書込に失敗したら実証 CASE は張らない。
        assert_eq!(h.log.lock().unwrap().as_slice(), &["cur:establish:5"]);
    }

    #[tokio::test]
    async fn rotate_verify_case_failure_is_failed_node_with_step_name() {
        // "other"（= E_next）側だけ establish を落とす: 書込は成功、実証 CASE が失敗。
        let h = harness(HashMap::new(), HashMap::new());
        let log = std::sync::Arc::clone(&h.log);
        let ctx = RotateCtx {
            main_ini: h.ctx.main_ini.clone(),
            fabric_index: 2,
            cfid: CFID,
            cur_epoch: CUR,
            make_establisher: Box::new(move |epoch: &[u8; 16]| {
                let is_cur = *epoch == CUR;
                Ok(Box::new(NodeFake {
                    label: if is_cur { "cur" } else { "other" },
                    establish_fail: if is_cur { HashMap::new() } else { HashMap::from([(5u64, ErrorKind::SessionFailed)]) },
                    invoke_fail: HashMap::new(),
                    log: std::sync::Arc::clone(&log),
                }) as Box<dyn Establisher>)
            }),
        };
        let out = run_with(&ctx, &params(&[5], RotateMode::Rotate)).await.unwrap();
        assert_eq!(out.status, RotateStatus::Pending);
        let e = out.nodes[0].error.as_ref().unwrap();
        assert_eq!(e.kind, ErrorKind::SessionFailed);
        assert!(e.detail.starts_with("node 5: verify-case: "), "{}", e.detail);
    }

    #[tokio::test]
    async fn rotate_with_no_nodes_commits_immediately() {
        let h = harness(HashMap::new(), HashMap::new());
        let out = run_with(&h.ctx, &params(&[], RotateMode::Rotate)).await.unwrap();
        assert_eq!(out.status, RotateStatus::Rotated);
        assert!(out.nodes.is_empty());
        assert_eq!(slot(&h, IpkEpochSlot::Prev), Some(CUR));
    }

    #[tokio::test]
    async fn rotate_per_node_timeout_marks_node_timeout_and_continues() {
        let h = harness(HashMap::new(), HashMap::new());
        let log = std::sync::Arc::clone(&h.log);
        let ctx = RotateCtx {
            main_ini: h.ctx.main_ini.clone(),
            fabric_index: 2,
            cfid: CFID,
            cur_epoch: CUR,
            make_establisher: Box::new(move |_epoch: &[u8; 16]| {
                Ok(Box::new(SlowFake { log: std::sync::Arc::clone(&log) }) as Box<dyn Establisher>)
            }),
        };
        let p = RotateIpkParams { node_ids: vec![5, 6], mode: RotateMode::Rotate, per_node_timeout_ms: 50 };
        let out = run_with(&ctx, &p).await.unwrap();
        assert_eq!(out.status, RotateStatus::Pending);
        assert_eq!(out.nodes.len(), 2);
        assert!(out.nodes.iter().all(|n| n.error.as_ref().unwrap().kind == ErrorKind::Timeout));
    }

    /// establish に 1 秒かかる fake（per-node timeout の検証用）。
    struct SlowFake {
        log: std::sync::Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Establisher for SlowFake {
        async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            self.log.lock().unwrap().push(format!("slow:establish:{node_id}"));
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Ok(Box::new(FakeConn::scripted()))
        }
    }

    #[tokio::test]
    async fn catch_up_uses_prev_to_write_and_cur_to_verify() {
        let h = harness(HashMap::new(), HashMap::new());
        // prev を仕込む（commit 済みの状態）。
        {
            let mut txn = KvsTxn::open(&h.ctx.main_ini).unwrap();
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Prev), &[0x0A; 16]);
            txn.commit().unwrap();
        }
        let out = run_with(&h.ctx, &params(&[9], RotateMode::CatchUp)).await.unwrap();
        assert_eq!(out.status, RotateStatus::CaughtUp);
        assert_eq!(
            h.log.lock().unwrap().as_slice(),
            &["other:establish:9", "cur:establish:9"],
            "prev（= other）で書き、cur で実証"
        );
        // KVS は不変。
        assert_eq!(slot(&h, IpkEpochSlot::Current), Some(CUR));
        assert_eq!(slot(&h, IpkEpochSlot::Prev), Some([0x0A; 16]));
        assert_eq!(slot(&h, IpkEpochSlot::Next), None);
        assert_eq!(out.body(2)["status"], "caught_up");
    }

    #[tokio::test]
    async fn catch_up_without_prev_or_while_pending_is_other() {
        let h = harness(HashMap::new(), HashMap::new());
        let e = run_with(&h.ctx, &params(&[9], RotateMode::CatchUp)).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Other);
        assert!(e.detail.contains("no previous ipk epoch"), "{}", e.detail);
        {
            let mut txn = KvsTxn::open(&h.ctx.main_ini).unwrap();
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Prev), &[0x0A; 16]);
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Next), &[0x0E; 16]);
            txn.commit().unwrap();
        }
        let e = run_with(&h.ctx, &params(&[9], RotateMode::CatchUp)).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Other);
        assert!(e.detail.contains("pending"), "{}", e.detail);
    }

    #[tokio::test]
    async fn catch_up_failure_is_incomplete_with_partial_error() {
        let h = harness(HashMap::from([(9u64, ErrorKind::SessionFailed)]), HashMap::new());
        {
            let mut txn = KvsTxn::open(&h.ctx.main_ini).unwrap();
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Prev), &[0x0A; 16]);
            txn.commit().unwrap();
        }
        let out = run_with(&h.ctx, &params(&[9], RotateMode::CatchUp)).await.unwrap();
        assert_eq!(out.status, RotateStatus::CatchUpIncomplete);
        let pe = out.partial_error().unwrap();
        assert_eq!(pe.kind, ErrorKind::SessionFailed);
        assert!(pe.detail.contains("catch-up incomplete"), "{}", pe.detail);
        assert!(out.body(2)["note"].as_str().unwrap().contains("re-commission"));
    }

    #[tokio::test]
    async fn abort_clears_pending_and_is_idle_otherwise() {
        let h = harness(HashMap::new(), HashMap::new());
        let out = run_with(&h.ctx, &params(&[], RotateMode::Abort)).await.unwrap();
        assert_eq!(out.status, RotateStatus::Idle);
        assert_eq!(out.body(2)["status"], "idle");
        {
            let mut txn = KvsTxn::open(&h.ctx.main_ini).unwrap();
            txn.set(&mat_ipk_epoch_slot_key(2, IpkEpochSlot::Next), &[0x0E; 16]);
            txn.commit().unwrap();
        }
        let out = run_with(&h.ctx, &params(&[], RotateMode::Abort)).await.unwrap();
        assert_eq!(out.status, RotateStatus::Aborted);
        assert_eq!(slot(&h, IpkEpochSlot::Next), None);
        assert_eq!(h.made.load(Ordering::SeqCst), 0, "abort は確立器を作らない");
    }
}
```

注意点（実装者向け）: `ErrorKind::as_str` が無ければ `serde_json::to_value(kind)` 経由で文字列化する（本体側の注記参照）。`MatError` に `Debug` が無ければ `NodeOutcome` / `RotateOutcome` の `#[derive(Debug)]` を外す。`Harness.made` は abort テストだけが使う。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-native rotate_ipk 2>&1 | tail -5`
Expected: 未定義エラー（`RotateCtx` 等）。

- [ ] **Step 3: 本体を実装**

`rotate_ipk.rs` の tests より上:

```rust
//! `mat fabric rotate-ipk` — IPK（keyset 0）の epoch ローテーション。直経路専用
//! （matd プロトコルには載せない、`commission` / `unpair` と同じ）。
//!
//! 状態は KVS の mat 名前空間 3 キー（`ipk-epoch` = 現行 / `ipk-epoch-next` =
//! 配布中（pending）/ `ipk-epoch-prev` = 直前）だけ。流れ:
//! 1. 現行 epoch を解決（`commission::resolve_ipk_epoch`）。
//! 2. pending ならその new epoch で再開、無ければ生成して**デバイスに触る前に**
//!    永続（途中クラッシュしても同じ鍵で再開、3 本目は生まれない）。
//! 3. 各ノードへ逐次: 現行 IPK で CASE → `KeySetWrite(0, {現行@1, 新@2})` →
//!    新 IPK で CASE を張り直して受理を実証（KeySetRead は鍵を返さない）。
//!    失敗しても続行し、per-node 結果を積む。
//! 4. 全ノード ok のときだけ commit（`group_settings::commit_ipk_rotation`、
//!    1 KvsTxn）。1 つでも失敗なら pending のまま（controller は現行 epoch で
//!    整合、デバイスは両 epoch を受理する）。
//!
//! デバイス側の旧 epoch は次回ローテーションの `{現行, 新}` 上書きで消える
//! （rolling 2 epoch）。取り残しノードは `CatchUp`（prev で CASE →
//! `{prev@1, 現行@2}` → 現行で実証）。設計: docs/superpowers/specs/
//! 2026-09-05-ipk-rotation-design.md。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use mat_controller::fabric;
use mat_controller::group_settings::{self, GroupSettingsError};
use mat_controller::kvs::{self, read_mat_ipk_epoch_slot, IpkEpochSlot, KvsError};
use mat_core::error::{ErrorKind, MatError};

use crate::{Establisher, NativeConfig, OneShotResolver, Resolver};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateMode {
    Rotate,
    CatchUp,
    Abort,
}

pub struct RotateIpkParams {
    pub node_ids: Vec<u64>,
    pub mode: RotateMode,
    /// 1 ノード分（書込 + 実証 CASE）の予算。0 = 無制限。
    pub per_node_timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateStatus {
    Rotated,
    Pending,
    CaughtUp,
    CatchUpIncomplete,
    Aborted,
    Idle,
}

impl RotateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RotateStatus::Rotated => "rotated",
            RotateStatus::Pending => "pending",
            RotateStatus::CaughtUp => "caught_up",
            RotateStatus::CatchUpIncomplete => "catch_up_incomplete",
            RotateStatus::Aborted => "aborted",
            RotateStatus::Idle => "idle",
        }
    }
}

#[derive(Debug)]
pub struct NodeOutcome {
    pub node_id: u64,
    pub error: Option<MatError>,
}

#[derive(Debug)]
pub struct RotateOutcome {
    pub status: RotateStatus,
    pub nodes: Vec<NodeOutcome>,
}

impl RotateOutcome {
    /// stdout 用 body（`timestamp` は `output::emit` が付ける）。鍵素材は載せない。
    pub fn body(&self, fabric_index: u8) -> Value {
        let nodes: Vec<Value> = self
            .nodes
            .iter()
            .map(|n| match &n.error {
                None => json!({ "node_id": n.node_id, "status": "ok" }),
                Some(e) => json!({
                    "node_id": n.node_id,
                    "status": "failed",
                    "error": e.to_json()["error"].clone(),
                }),
            })
            .collect();
        let mut body = json!({
            "fabric_index": fabric_index,
            "status": self.status.as_str(),
            "nodes": nodes,
        });
        if let Some(note) = self.note() {
            body["note"] = json!(note);
        }
        body
    }

    fn note(&self) -> Option<&'static str> {
        match self.status {
            RotateStatus::Rotated => Some(
                "if matd is running, restart it before the next rotation to load the new IPK; \
                 nodes left out of --nodes need `mat fabric rotate-ipk --catch-up --nodes <N>`",
            ),
            RotateStatus::Pending => Some(
                "no controller-side change yet; re-run `mat fabric rotate-ipk` with the same nodes \
                 to retry, or with --nodes <subset> to commit without the failed ones \
                 (catch them up later with --catch-up)",
            ),
            RotateStatus::CatchUpIncomplete => Some(
                "failed nodes may be two epochs behind; if --catch-up keeps failing with \
                 session_failed, re-commission them",
            ),
            _ => None,
        }
    }

    /// 部分失敗（pending / catch_up_incomplete）のとき stderr へ出す error。
    /// kind は最初に失敗したノードのもの、detail に失敗ノードを列挙する。
    pub fn partial_error(&self) -> Option<MatError> {
        let what = match self.status {
            RotateStatus::Pending => "ipk rotation pending",
            RotateStatus::CatchUpIncomplete => "ipk catch-up incomplete",
            _ => return None,
        };
        let failed: Vec<(&NodeOutcome, &MatError)> = self
            .nodes
            .iter()
            .filter_map(|n| n.error.as_ref().map(|e| (n, e)))
            .collect();
        let (_, first) = failed.first()?;
        let list = failed
            .iter()
            .map(|(n, e)| format!("node {}: {}", n.node_id, e.kind.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        Some(MatError::new(
            first.kind,
            format!(
                "{what}: {} of {} nodes failed ({list}); see stdout for per-node detail",
                failed.len(),
                self.nodes.len()
            ),
        ))
    }
}

/// 実行に必要な材料。`run` が `NativeConfig` から組み立て、テストは fake 確立器を
/// 注入する。
pub struct RotateCtx {
    pub main_ini: PathBuf,
    pub fabric_index: u8,
    pub cfid: [u8; 8],
    pub cur_epoch: [u8; 16],
    /// epoch 鍵 → その IPK で CASE を張る確立器。
    pub make_establisher:
        Box<dyn Fn(&[u8; 16]) -> Result<Box<dyn Establisher>, MatError> + Send + Sync>,
}

pub async fn run(cfg: &NativeConfig, p: &RotateIpkParams) -> Result<RotateOutcome, MatError> {
    let main_ini = cfg.store.join(kvs::MAIN_INI_FILE);
    let fabric_index = cfg.fabric_index;
    let creds = crate::load_fabric_credentials(cfg)?;
    let cfid = fabric::compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
    let cur_epoch = crate::commission::resolve_ipk_epoch(&main_ini, fabric_index, &creds)?;
    let resolver: Arc<dyn Resolver> = Arc::new(OneShotResolver);
    let cfg = cfg.clone();
    let make = move |epoch: &[u8; 16]| {
        let mut c = creds.clone();
        c.ipk_operational = fabric::derive_ipk_operational(epoch, &cfid);
        crate::case_establisher(&cfg, c, Arc::clone(&resolver))
    };
    let ctx = RotateCtx {
        main_ini,
        fabric_index,
        cfid,
        cur_epoch,
        make_establisher: Box::new(make),
    };
    run_with(&ctx, p).await
}
```

続き:

```rust
pub async fn run_with(ctx: &RotateCtx, p: &RotateIpkParams) -> Result<RotateOutcome, MatError> {
    match p.mode {
        RotateMode::Abort => abort(ctx),
        RotateMode::Rotate => rotate(ctx, p).await,
        RotateMode::CatchUp => catch_up(ctx, p).await,
    }
}

fn read_slot(ctx: &RotateCtx, slot: IpkEpochSlot) -> Result<Option<[u8; 16]>, MatError> {
    read_mat_ipk_epoch_slot(&ctx.main_ini, ctx.fabric_index, slot)
        .map_err(|e| MatError::new(ErrorKind::StoreParse, format!("kvs ipk epoch ({slot:?}): {e}")))
}

fn map_gs_err(e: GroupSettingsError) -> MatError {
    match e {
        GroupSettingsError::Kvs(KvsError::Locked) => MatError::new(
            ErrorKind::Other,
            "controller kvs is locked by another process (concurrent rotate-ipk / provision?)",
        ),
        GroupSettingsError::Corrupt { key, reason } => MatError::new(
            ErrorKind::StoreParse,
            format!("inconsistent ipk rotation state at {key}: {reason}"),
        ),
        other => MatError::new(ErrorKind::Other, format!("controller kvs ipk rotation write failed: {other}")),
    }
}

/// CSPRNG の新 epoch（現行と一致したら引き直す）。
fn fresh_epoch(cur: &[u8; 16]) -> Result<[u8; 16], MatError> {
    loop {
        let e = crate::ops::epoch_key_from_hex(&mat_core::group::generate_epoch_key())?;
        if e != *cur {
            return Ok(e);
        }
    }
}

async fn rotate(ctx: &RotateCtx, p: &RotateIpkParams) -> Result<RotateOutcome, MatError> {
    let next = match read_slot(ctx, IpkEpochSlot::Next)? {
        Some(n) => {
            tracing::info!(fabric_index = ctx.fabric_index, "resuming pending ipk rotation");
            n
        }
        None => {
            let n = fresh_epoch(&ctx.cur_epoch)?;
            group_settings::begin_ipk_rotation(&ctx.main_ini, ctx.fabric_index, &n)
                .map_err(map_gs_err)?;
            tracing::info!(fabric_index = ctx.fabric_index, "ipk rotation started (next epoch persisted)");
            n
        }
    };
    let est_cur = (ctx.make_establisher)(&ctx.cur_epoch)?;
    let est_next = (ctx.make_establisher)(&next)?;
    let epochs = [(ctx.cur_epoch, 1u64), (next, 2u64)];
    let nodes = distribute(&*est_cur, &*est_next, &epochs, &p.node_ids, p.per_node_timeout_ms).await;
    if nodes.iter().any(|n| n.error.is_some()) {
        return Ok(RotateOutcome { status: RotateStatus::Pending, nodes });
    }
    if p.node_ids.is_empty() {
        tracing::warn!("no commissioned nodes; committing ipk rotation without distributing");
    }
    group_settings::commit_ipk_rotation(&ctx.main_ini, ctx.fabric_index, &ctx.cfid, &ctx.cur_epoch, &next)
        .map_err(map_gs_err)?;
    tracing::info!(fabric_index = ctx.fabric_index, nodes = nodes.len(), "ipk rotation committed");
    Ok(RotateOutcome { status: RotateStatus::Rotated, nodes })
}

async fn catch_up(ctx: &RotateCtx, p: &RotateIpkParams) -> Result<RotateOutcome, MatError> {
    let prev = read_slot(ctx, IpkEpochSlot::Prev)?.ok_or_else(|| {
        MatError::new(
            ErrorKind::Other,
            "no previous ipk epoch recorded for this fabric (rotate-ipk has never committed here; nothing to catch up from)",
        )
    })?;
    if read_slot(ctx, IpkEpochSlot::Next)?.is_some() {
        return Err(MatError::new(
            ErrorKind::Other,
            "an ipk rotation is pending; finish it (re-run rotate-ipk) or --abort it before --catch-up",
        ));
    }
    let est_prev = (ctx.make_establisher)(&prev)?;
    let est_cur = (ctx.make_establisher)(&ctx.cur_epoch)?;
    let epochs = [(prev, 1u64), (ctx.cur_epoch, 2u64)];
    let nodes = distribute(&*est_prev, &*est_cur, &epochs, &p.node_ids, p.per_node_timeout_ms).await;
    let status = if nodes.iter().any(|n| n.error.is_some()) {
        RotateStatus::CatchUpIncomplete
    } else {
        RotateStatus::CaughtUp
    };
    tracing::info!(fabric_index = ctx.fabric_index, status = status.as_str(), "ipk catch-up executed");
    Ok(RotateOutcome { status, nodes })
}

fn abort(ctx: &RotateCtx) -> Result<RotateOutcome, MatError> {
    let removed = group_settings::abort_ipk_rotation(&ctx.main_ini, ctx.fabric_index).map_err(map_gs_err)?;
    let status = if removed { RotateStatus::Aborted } else { RotateStatus::Idle };
    tracing::info!(fabric_index = ctx.fabric_index, status = status.as_str(), "ipk rotation abort executed");
    Ok(RotateOutcome { status, nodes: Vec::new() })
}

/// 各ノードへ逐次: `write_with` で CASE → KeySetWrite(0, epochs) → close →
/// `verify_with` で CASE（受理の実証）→ close。失敗しても続行。
async fn distribute(
    write_with: &dyn Establisher,
    verify_with: &dyn Establisher,
    epochs: &[([u8; 16], u64)],
    node_ids: &[u64],
    timeout_ms: u64,
) -> Vec<NodeOutcome> {
    let mut out = Vec::with_capacity(node_ids.len());
    for &node_id in node_ids {
        let step = one_node(write_with, verify_with, epochs, node_id);
        let result = if timeout_ms > 0 {
            match tokio::time::timeout(Duration::from_millis(timeout_ms), step).await {
                Ok(r) => r,
                Err(_) => Err(MatError::new(
                    ErrorKind::Timeout,
                    format!("node {node_id}: ipk key-set-write exceeded {timeout_ms} ms"),
                )),
            }
        } else {
            step.await
        };
        match &result {
            Ok(()) => tracing::info!(node_id, "ipk keyset written and verified"),
            Err(e) => tracing::warn!(node_id, kind = ?e.kind, detail = %e.detail, "ipk keyset step failed"),
        }
        out.push(NodeOutcome { node_id, error: result.err() });
    }
    out
}

async fn one_node(
    write_with: &dyn Establisher,
    verify_with: &dyn Establisher,
    epochs: &[([u8; 16], u64)],
    node_id: u64,
) -> Result<(), MatError> {
    let mut conn = write_with
        .establish(node_id)
        .await
        .map_err(|e| step_err(node_id, "establish", e))?;
    let written = crate::ops::write_ipk_keyset(conn.as_mut(), epochs).await;
    conn.close().await;
    written.map_err(|e| step_err(node_id, "", e))?;
    let mut conn = verify_with
        .establish(node_id)
        .await
        .map_err(|e| step_err(node_id, "verify-case", e))?;
    conn.close().await;
    Ok(())
}

fn step_err(node_id: u64, step: &str, e: MatError) -> MatError {
    let detail = if step.is_empty() {
        format!("node {node_id}: {}", e.detail)
    } else {
        format!("node {node_id}: {step}: {}", e.detail)
    };
    MatError::new(e.kind, detail)
}
```

`ErrorKind::as_str` が無ければ `serde_json::to_value(e.kind).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()` で代替する（`body.rs` の `diag_thread_success` と同じ手法）。`MatError` に `Debug` が無ければ `NodeOutcome` / `RotateOutcome` の `#[derive(Debug)]` を外す。

`lib.rs` に `pub mod rotate_ipk;` を追加（`pub mod commission;` の隣）。

- [ ] **Step 4: テストを通す**

Run: `cargo test -p mat-native rotate_ipk 2>&1 | tail -15`
Expected: 全 PASS。

- [ ] **Step 5: fmt / clippy / コミット**

```bash
cargo fmt && cargo clippy -p mat-native --all-targets -- -D warnings
git add crates/mat-native/src/rotate_ipk.rs crates/mat-native/src/lib.rs
git commit -m "feat(native): rotate_ipk — IPK ローテーションの状態機械（配布・実証・commit・catch-up・abort）"
```

---

### Task 6: CLI `mat fabric rotate-ipk` と `fabric list` の pending 表示

**Files:**
- Modify: `crates/mat/src/cli.rs`（`FabricAction`）
- Modify: `crates/mat/src/resolve.rs`（`Command::Fabric` 腕）
- Modify: `crates/mat/src/main.rs`（早期 dispatch を `Init` / `List` に限定、`RotateIpk` を直経路の `result` match へ）
- Modify: `crates/mat/src/commands/fabric.rs`（`run_rotate_ipk`、`run_list` に `ipk_rotation_pending`）
- Modify: `crates/mat/src/native_direct.rs`（`map_engine_build_error` を `pub(crate)`）
- Test: `crates/mat/tests/integration.rs`（clap / 台帳 / 出力形の CLI テスト）

**Interfaces:**
- Consumes: Task 5 の `mat_native::rotate_ipk::{run, RotateIpkParams, RotateMode}`、Task 2 の `kvs::{read_mat_ipk_epoch_slot, IpkEpochSlot}`。
- Produces: `commands::fabric::run_rotate_ipk(store_path: &Path, nodes: &[u64], catch_up: bool, abort: bool, native: Option<&native_direct::Config<'_>>, op_timeout_ms: u64) -> Result<(), MatError>`。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/tests/integration.rs` の既存スタイル（`assert_cmd` / `Command::cargo_bin("mat")` 等、ファイル先頭のヘルパを確認して合わせる）で:

```rust
#[test]
fn fabric_rotate_ipk_rejects_catch_up_with_abort() {
    let out = mat()
        .args(["fabric", "rotate-ipk", "--catch-up", "--abort"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn fabric_rotate_ipk_unknown_node_is_exit_11() {
    // fabric init 済みだが台帳に無い node id → node_not_commissioned（exit 11）。
    let dir = tempfile::tempdir().unwrap();
    let init = mat()
        .env("MAT_STORE", dir.path())
        .args(["fabric", "init"])
        .output()
        .unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    let out = mat()
        .env("MAT_STORE", dir.path())
        .env("MAT_MATD", "0")
        .args(["--iface", "lo", "fabric", "rotate-ipk", "--nodes", "99"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(11), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn fabric_rotate_ipk_with_no_nodes_rotates_locally_and_list_shows_state() {
    let dir = tempfile::tempdir().unwrap();
    let init = mat()
        .env("MAT_STORE", dir.path())
        .args(["fabric", "init"])
        .output()
        .unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    let run = |args: &[&str]| {
        mat()
            .env("MAT_STORE", dir.path())
            .env("MAT_MATD", "0")
            .args(["--iface", "lo"])
            .args(args)
            .output()
            .unwrap()
    };
    // 台帳が空 → 配布相手無しで即 commit。
    let out = run(&["fabric", "rotate-ipk"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["status"], "rotated");
    assert_eq!(body["nodes"], serde_json::json!([]));
    assert!(body["timestamp"].is_string());
    // --catch-up は prev があるので通る（ノード 0 件）。
    let out = run(&["fabric", "rotate-ipk", "--catch-up"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["status"], "caught_up");
    // --abort は pending 無し → idle。
    let out = run(&["fabric", "rotate-ipk", "--abort"]);
    assert!(out.status.success());
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["status"], "idle");
    // fabric list に pending フラグ。
    let out = mat()
        .env("MAT_STORE", dir.path())
        .args(["fabric", "list"])
        .output()
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["fabrics"][0]["ipk_rotation_pending"], false);
    assert_eq!(body["fabrics"][0]["ipk_epoch"], "mat");
}

#[test]
fn fabric_rotate_ipk_forced_matd_is_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let out = mat()
        .env("MAT_STORE", dir.path())
        .args(["--matd", "fabric", "rotate-ipk"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
}
```

`mat()` は integration.rs の既存ヘルパ名に合わせる（無ければ `assert_cmd::Command::cargo_bin("mat").unwrap()` を返す fn を足す）。`--iface lo` は `iface_index("lo")` が解決できればよく、ノード 0 件ではネットワークに出ない。`--matd` 強制のテストは `Dispatch::Dedicated("fabric")` → `unsupported_exit` の既存経路（`matd_client::resolve_route` が `Forced` を返すのに socket は要らないかを既存の `unpair --matd` テストで確認し、同じ書き方にする）。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matterctl --test integration fabric_rotate_ipk 2>&1 | tail -8`
Expected: clap が `rotate-ipk` を知らず exit 2（3 番目・2 番目のテストが FAIL）。

- [ ] **Step 3: 実装**

`cli.rs` `FabricAction` に追加:

```rust
    /// IPK（keyset 0）の epoch をローテーションする: 新 epoch を生成し、各ノードへ
    /// KeySetWrite {現行, 新} を配布・新 IPK で CASE を張って受理を実証、全ノード
    /// ok なら controller KVS を新 epoch へ切替。途中失敗は pending（再実行で同じ
    /// 新鍵から再開）。直経路専用（ネットワークに出る）。
    RotateIpk {
        /// 対象ノード（省略 = 台帳の全ノード）。pending の再実行で絞ると、その部分
        /// 集合が全部成功した時点で commit する（除外ノードは後で --catch-up）。
        #[arg(long = "nodes", num_args = 1.., value_name = "N|ALIAS")]
        nodes: Vec<NodeRef>,
        /// commit 済みローテーションに取り残されたノードへ旧 epoch で CASE して
        /// {旧, 現行} を配る
        #[arg(long, conflicts_with = "abort")]
        catch_up: bool,
        /// pending のローテーションを取り消す（デバイスは触らない）
        #[arg(long, conflicts_with = "catch_up")]
        abort: bool,
    },
```

`Fabric` の doc を「fabric 管理（初回 bootstrap / 一覧 / IPK ローテーション）。」に。

`resolve.rs` の `Command::Fabric { action } => Command::Fabric { action },` を:

```rust
        Command::Fabric {
            action: FabricAction::RotateIpk { nodes, catch_up, abort },
        } => Command::Fabric {
            action: FabricAction::RotateIpk {
                nodes: nodes
                    .into_iter()
                    .map(|n| book.resolve_node(&n).map(NodeRef::Id))
                    .collect::<Result<_, MatError>>()?,
                catch_up,
                abort,
            },
        },
        // fabric_id / admin_node_id は数値のみ — パススルー。
        Command::Fabric { action } => Command::Fabric { action },
```

（`use crate::cli::FabricAction;` を追加。）

`main.rs` の早期 dispatch:

```rust
    if let Command::Fabric { action } = &command {
        let result = match action {
            FabricAction::Init { fabric_id, admin_node_id } => Some(commands::fabric::run_init(
                &store_path, *fabric_id, *admin_node_id, args.fabric_index, args.issuer_index,
            )),
            FabricAction::List => Some(commands::fabric::run_list(&store_path, args.fabric_index)),
            // rotate-ipk はネットワークに出るので直経路 dispatch（下）へ落とす。
            FabricAction::RotateIpk { .. } => None,
        };
        if let Some(result) = result {
            return match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    e.emit();
                    ExitCode::from(e.kind.exit_code())
                }
            };
        }
    }
```

直経路の `let result = match &command {` に腕を追加（`Command::Diag { action: DiagCommand::Mesh { .. } }` の後）:

```rust
        Command::Fabric {
            action: FabricAction::RotateIpk { nodes, catch_up, abort },
        } => nodes
            .iter()
            .map(mat_core::alias::NodeRef::id)
            .collect::<Result<Vec<u64>, MatError>>()
            .and_then(|ids| {
                commands::fabric::run_rotate_ipk(
                    &store_path,
                    &ids,
                    *catch_up,
                    *abort,
                    native_cfg.as_ref(),
                    args.op_timeout_ms,
                )
            }),
```

コメント「Command::Fabric は route dispatch より前の早期 return で処理済み」を「Command::Fabric の Init / List は早期 return、RotateIpk は上の腕で処理済み」に直す。

`native_direct.rs`: `fn map_engine_build_error` → `pub(crate) fn map_engine_build_error`。

`commands/fabric.rs`:

```rust
use mat_native::rotate_ipk::{self, RotateIpkParams, RotateMode};
use mat_native::NativeConfig;

/// `mat fabric rotate-ipk`: 状態機械は `mat_native::rotate_ipk`、ここは台帳での
/// ノード解決（省略 = 全ノード）と body の emit だけ。部分失敗（pending /
/// catch_up_incomplete）は stdout に body を出したうえで stderr error を返す
/// （終了コードは最初に失敗したノードの kind）。
pub fn run_rotate_ipk(
    store_path: &Path,
    nodes: &[u64],
    catch_up: bool,
    abort: bool,
    native: Option<&crate::native_direct::Config<'_>>,
    op_timeout_ms: u64,
) -> Result<(), MatError> {
    let cfg = native.ok_or_else(|| {
        MatError::new(
            ErrorKind::Other,
            "rotate-ipk: native backend not configured (internal)",
        )
    })?;
    let store = mat_core::store::Store::open(store_path)?;
    let node_ids: Vec<u64> = if nodes.is_empty() {
        store.nodes().map(|n| n.node_id).collect()
    } else {
        for &id in nodes {
            store.require_node(id)?;
        }
        nodes.to_vec()
    };
    let mode = if abort {
        RotateMode::Abort
    } else if catch_up {
        RotateMode::CatchUp
    } else {
        RotateMode::Rotate
    };
    let params = RotateIpkParams {
        node_ids,
        mode,
        per_node_timeout_ms: op_timeout_ms,
    };
    let native_cfg = NativeConfig {
        store: store.root().to_path_buf(),
        iface: cfg.iface.to_string(),
        thread_iface: cfg.thread_iface.clone(),
        fabric_index: cfg.fabric_index,
        issuer_index: cfg.issuer_index,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| MatError::new(ErrorKind::Other, format!("tokio runtime: {e}")))?;
    let outcome = rt
        .block_on(rotate_ipk::run(&native_cfg, &params))
        .map_err(crate::native_direct::map_engine_build_error)?;
    tracing::info!(
        status = outcome.status.as_str(),
        nodes = outcome.nodes.len(),
        "fabric rotate-ipk executed"
    );
    output::emit(outcome.body(cfg.fabric_index));
    match outcome.partial_error() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
```

`run_list` のループに `let pending = kvs::read_mat_ipk_epoch_slot(&main_ini, idx, kvs::IpkEpochSlot::Next).map_err(map_err)?.is_some();` を足し、json に `"ipk_rotation_pending": pending,` を追加。ファイル先頭の doc を「`mat fabric init` / `list` / `rotate-ipk`」に更新。

- [ ] **Step 4: テストを通す**

Run: `cargo test -p matterctl 2>&1 | tail -8`
Expected: 全 PASS。`--iface lo` で `iface_index` が失敗する環境なら、テスト側を `std::env::var("MAT_E2E_IFACE").unwrap_or("lo")` で差し替え可能にして再実行する。

- [ ] **Step 5: fmt / clippy / コミット**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
git add crates/mat/src/cli.rs crates/mat/src/resolve.rs crates/mat/src/main.rs crates/mat/src/commands/fabric.rs crates/mat/src/native_direct.rs crates/mat/tests/integration.rs
git commit -m "feat(cli): mat fabric rotate-ipk（--nodes / --catch-up / --abort）と fabric list の ipk_rotation_pending"
```

---

### Task 7: docs と matv 統合 E2E スクリプト

**Files:**
- Modify: `docs/commands.md`（fabric 節の `fabric list` の後に `rotate-ipk` 節、「Supported over matd」箇条書きに追加）
- Modify: `docs/errors.md`（部分失敗の規約）
- Modify: `ARCHITECTURE.md`（backend 節の epoch 記述、将来候補 (3) の訂正）
- Modify: `CLAUDE.md`（backend 箇条書きの IPK 行）
- Create: `scripts/e2e-device-m4.sh`
- Modify: `Taskfile.yml`（`e2e:device:m4`）

**Interfaces:**
- Consumes: Task 6 の CLI（`mat fabric rotate-ipk`、`fabric list` の `ipk_rotation_pending`）。

- [ ] **Step 1: docs/commands.md に rotate-ipk 節を追加**

`fabric list` 節（「There is no `fabric show` — …」の段落）の直後に:

````markdown
#### IPK rotation (`fabric rotate-ipk`)

The fabric's IPK (Identity Protection Key, key set 0 — the key CASE uses to
hide destination identifiers, and the one `AddNOC` installs on every new node)
can be rotated to a fresh epoch. Direct path only (it talks to every node):

```bash
# fabric rotate-ipk [--nodes <N|ALIAS>...] [--catch-up | --abort]
mat fabric rotate-ipk                 # every node in the ledger
mat fabric rotate-ipk --nodes 5 6     # a subset (retry / commit without the rest)
mat fabric rotate-ipk --catch-up --nodes 7   # a node that missed a committed rotation
mat fabric rotate-ipk --abort         # drop a pending rotation
```

```json
{
  "timestamp": "2026-06-06T12:34:56+09:00",
  "fabric_index": 1,
  "status": "rotated",
  "nodes": [ { "node_id": 5, "status": "ok" }, { "node_id": 6, "status": "ok" } ],
  "note": "if matd is running, restart it before the next rotation to load the new IPK; nodes left out of --nodes need `mat fabric rotate-ipk --catch-up --nodes <N>`"
}
```

How it works, and why it is safe to interrupt at any point:

1. A new epoch key is generated and persisted as *pending*
   (`mat/f/<idx>/ipk-epoch-next`) **before any node is touched**, so a re-run
   after a crash resumes with the same key.
2. Every node (in `--nodes` order, one at a time) gets `KeySetWrite` on key
   set 0 carrying **both** epochs — the current one (epoch 0) and the new one
   (epoch 1). `mat` then re-establishes CASE with the *new* IPK to prove the
   node accepts it (`KeySetRead` never returns key material, so this is the
   only proof). A failure on one node does not stop the others.
3. Only when **every listed node** succeeded does the controller switch: key
   set 0 in the store, `mat/f/<idx>/ipk-epoch` and `-prev` are rewritten in
   one locked transaction (`status: "rotated"`). Otherwise nothing changes on
   the controller (`status: "pending"`, see below) — the controller still uses
   the old IPK and every node still accepts it.
4. The old epoch is **not** removed from the nodes: they keep `{old, new}`
   until the next rotation overwrites key set 0 with `{new, newer}`. Devices
   check every epoch of the IPK key set when answering CASE, so a controller on
   either epoch keeps working, and the IPK is not a session key — one stale
   epoch is an accepted trade-off for a rotation that can never strand a node.

Partial failure: the stdout body lists each node (`"status": "failed"` with
the usual `error` object), **and** `mat` exits with the kind / code of the
first failed node (a stderr `error` names all failed nodes). Re-run with the
same nodes to retry, or with `--nodes <subset>` to commit without the ones that
stay unreachable — those are then one epoch behind, and once they are back:
`mat fabric rotate-ipk --catch-up --nodes <N>` (CASE with the previous epoch,
write `{previous, current}`, prove with the current one). A node that missed
**two** rotations cannot be caught up (the previous epoch is gone) — it needs
`unpair` + `commission`. `--abort` drops the pending key (nodes keep the extra
epoch harmlessly).

- `--nodes` defaults to every node in the ledger; explicit ids / aliases must
  be commissioned (`node_not_commissioned`, exit `11`). `--op-timeout-ms`
  budgets each node's step (write + proof CASE) — exceeding it is `timeout`
  for that node, and the run continues.
- **matd** loads the IPK at start-up and has no reload: it keeps working after
  a rotation (the nodes accept both epochs) but **must be restarted before
  the next rotation**, or its CASE attempts will fail once the nodes drop the
  epoch it holds. `commission` picks the new epoch up immediately.
- `fabric list` shows `"ipk_rotation_pending": true` while a rotation is
  pending; `fabric rotate-ipk --abort` clears it.
- The virtual device `matv` does not accept `KeySetWrite` on key set 0 yet, so
  against `matv` a rotation always ends `pending` with `device_rejected`
  (`scripts/e2e-device-m4.sh` pins exactly that behaviour). Real devices
  follow the spec (§11.2.8.1).
- Never part of the `matd` socket protocol (direct-only like `commission`);
  explicit `--matd` exits `2`.
````

「Supported over matd」箇条書きの `` `discover` / `commission` / `unpair` / `fabric init` / `open-window` / `diag` `` に `` / `fabric rotate-ipk` `` を追加。

- [ ] **Step 2: docs/errors.md / ARCHITECTURE.md / CLAUDE.md**

`docs/errors.md` の `kind` 一覧の前（「When `commission` tries more than one route…」の段落の後）に:

```markdown
`mat fabric rotate-ipk` is the one command that can **both** print a body on
stdout and exit non-zero: a partial failure (`status: "pending"` /
`"catch_up_incomplete"`) emits the per-node result as JSON on stdout, then
reports the first failed node's `kind` on stderr (and exits with its code) so
scripts that only check the exit status still notice. stdout stays pure JSON.
```

`ARCHITECTURE.md` の M8c-3 将来候補 (3)（「(3) IPK ローテーション（全ノード KeySetWrite での epoch 完全移行 — 現状は…）」）を `~~…~~【訂正 2026-09-05: 実装済み — `mat fabric rotate-ipk`、docs/superpowers/specs/2026-09-05-ipk-rotation-design.md】` の形に。backend 節（`mat/f/<idx>/ipk-epoch` に触れている 177-180 行付近）に 1 文: 「IPK rotation (`mat fabric rotate-ipk`) keeps its state in the same namespace (`ipk-epoch-next` while pending, `ipk-epoch-prev` after commit) and distributes `{current, new}` to every node before switching the controller in one transaction.」

`CLAUDE.md` の backend 箇条書き「First-fabric bootstrap is `mat fabric init` …（persisted to `mat/f/<idx>/ipk-epoch`）」の直後に 1 文: 「IPK rotation is `mat fabric rotate-ipk` (direct-only; pending / prev epochs live at `mat/f/<idx>/ipk-epoch-next` / `-prev`; the controller switches only after every listed node holds both epochs).」

- [ ] **Step 3: `scripts/e2e-device-m4.sh` を書く**

`scripts/e2e-device-m3.sh` の 1〜200 行（ヘッダ・`json_get`・cleanup・build・matv 起動・`fabric init`・`commission`）をコピーして流用し、ヘッダのコメントを次に差し替え、`commission` 以降を下のシナリオにする（matd / listen 部分は不要なので `MATD_*` / `LISTEN_*` の変数・cleanup 行は削る）:

```bash
# M4 IPK rotation E2E against the virtual device — pins the *failure* path,
# because `matv` (mat-device) does not accept KeySetWrite on key set 0 yet
# (INVALID_COMMAND) and holds a single epoch key. Flow: build -> matv ->
# `mat fabric init` + `mat commission` -> `mat group provision` ->
# `mat fabric rotate-ipk` (expect exit 4 = device_rejected, stdout body
# status:"pending", node 1 failed with kind device_rejected) -> `mat fabric
# list` shows ipk_rotation_pending:true -> `mat on` and `mat group invoke`
# still work (the controller did NOT switch epochs) -> `mat fabric rotate-ipk
# --abort` (status:"aborted") -> `fabric list` pending:false -> `mat on` again.
#
# When matv learns KeySetWrite(0) (multi-epoch IPK), flip the expectations:
# rotate-ipk exits 0 with status:"rotated", and `mat on` afterwards proves
# CASE with the new IPK.
```

シナリオ本体（`echo "==> commissioned (node=$NODE_ID)"` の後）:

```bash
echo "==> mat group provision (group=$GROUP_ID, node=$NODE_ID, endpoint=$DEVICE_EP)" >&2
GROUP_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
        ./target/release/mat --iface "$IFACE" group provision \
            --group "$GROUP_ID" --nodes "$NODE_ID" --endpoint "$DEVICE_EP" --name e2e-group
)"
[[ "$(json_get status "$GROUP_JSON")" == "provisioned" ]]

echo "==> mat fabric rotate-ipk (expect pending: matv rejects KeySetWrite(0))" >&2
set +e
ROTATE_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" fabric rotate-ipk 2>"$WORKDIR/rotate.stderr")"
ROTATE_RC=$?
set -e
echo "$ROTATE_JSON"
cat "$WORKDIR/rotate.stderr" >&2
[[ "$ROTATE_RC" == "4" ]] || { echo "expected exit 4 (device_rejected), got $ROTATE_RC" >&2; exit 1; }
printf '%s' "$ROTATE_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["status"] == "pending", d
n = d["nodes"][0]
assert n["node_id"] == '"$NODE_ID"' and n["status"] == "failed", d
assert n["error"]["kind"] == "device_rejected", d
assert "epoch" not in json.dumps(d).lower() or "ipk" in d.get("note", ""), d
'
grep -q '"kind":"device_rejected"' "$WORKDIR/rotate.stderr"
echo "==> PASS: rotate-ipk ended pending with device_rejected on node $NODE_ID" >&2

echo "==> fabric list shows ipk_rotation_pending:true" >&2
LIST_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat fabric list)"
echo "$LIST_JSON" >&2
printf '%s' "$LIST_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
f = [x for x in d["fabrics"] if x["current"]][0]
assert f["ipk_rotation_pending"] is True, d
'

echo "==> controller still on the old IPK: mat on + group invoke keep working" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" on --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" group invoke -g "$GROUP_ID" -c onoff --command off -e "$DEVICE_EP" >&2
sleep 1
READ_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" read --node "$NODE_ID" --endpoint "$DEVICE_EP" --cluster onoff --attribute on-off)"
[[ "$(json_get value "$READ_JSON")" == "false" ]] || { echo "groupcast off did not land after pending rotation: $READ_JSON" >&2; exit 1; }
echo "==> PASS: unicast + groupcast unaffected by a pending rotation" >&2

echo "==> mat fabric rotate-ipk --abort" >&2
ABORT_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" fabric rotate-ipk --abort)"
echo "$ABORT_JSON"
[[ "$(json_get status "$ABORT_JSON")" == "aborted" ]]
LIST_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat fabric list)"
printf '%s' "$LIST_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
f = [x for x in d["fabrics"] if x["current"]][0]
assert f["ipk_rotation_pending"] is False, d
'
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" on --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2
echo "==> PASS: abort cleared pending; node still reachable" >&2
echo "==> ALL PASS (m4: ipk rotation failure path against matv)" >&2
```

`python3 -c` 内の `assert "epoch" not in ...` 行は「鍵素材が body に無い」意図が曖昧なので入れない — 代わりに `assert all(len(v) < 40 for v in json.dumps(d).split('"'))` のような形も避け、単に status / node / kind の 3 assert に留める。

`chmod +x scripts/e2e-device-m4.sh`。`Taskfile.yml` の `e2e:device:m3` の後に:

```yaml
  e2e:device:m4:
    desc: "M4 IPK ローテーション E2E（matv 相手・失敗経路）: rotate-ipk が device_rejected で pending → 旧 IPK で on/group invoke が通る → --abort（要: 実 NIC、既定 eth1 / MAT_E2E_IFACE で変更）"
    cmds:
      - bash scripts/e2e-device-m4.sh
```

- [ ] **Step 4: E2E を実行**

Run: `MAT_E2E_IFACE=<多播可能な NIC> task e2e:device:m4 2>&1 | tail -20`（NIC は `ip -o link | grep MULTICAST` から。m3 スクリプトの既定は `eth1`）。
Expected: `==> ALL PASS (m4: …)`。失敗したら、期待値ではなく原因（exit code / status / stderr の kind）を見て Task 5/6 側の欠陥かスクリプトの環境問題かを切り分ける。

- [ ] **Step 5: mdBook / fmt / コミット**

Run: `task check 2>&1 | tail -5`（fmt:check + clippy + test 全通過）。

```bash
git add docs/commands.md docs/errors.md ARCHITECTURE.md CLAUDE.md scripts/e2e-device-m4.sh Taskfile.yml
git commit -m "docs+e2e: fabric rotate-ipk の docs と matv 相手の失敗経路 E2E（e2e:device:m4）"
```

---

## 実行後（計画外・メインセッションが行う）

1. `task check` 全通過を再確認。
2. 実機スモーク（hogar-matd コンテナ内、隔離 store `/tmp/mat-rot-smoke`、本番 `/data/mat` には触らない、他セッションと同時に走らせない）: x86_64 バイナリを `docker cp` → `fabric init` → `rotate-ipk`（ノード 0 件 → rotated）→ `fabric list` → `rotate-ipk --nodes 99`（exit 11）→ `--catch-up`（caught_up）→ `--abort`（idle）。
3. main へ rebase → no-ff マージ → push。メモリの監査バックログに完了を追記。
