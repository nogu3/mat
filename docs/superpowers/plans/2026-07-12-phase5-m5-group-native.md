# Phase 5 M5: group セッション native 化 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matd の group 送信 3 op（GroupInvoke onoff / GroupColor / GroupColorTemp）を mat-controller の native groupcast（AES-CCM + ff35:: multicast + 永続 counter）で処理する。

**Architecture:** 新モジュール `mat-controller::group` に送信専用の `GroupSender`（`SecureSession` から独立）。鍵材料は chip-tool KVS の導出済み operational credentials（GKH = session id + 16B 鍵）を読むだけで、KDF 導出はしない。counter は自前ファイル + 起動時 `max(自前, chip-tool g/gdc) + 4096` の jump-ahead。`GroupProvision` と native 不可時の group op は chip-tool フォールバック。

**Tech Stack:** Rust / tokio / ccm(AES-CCM 既存 `crypto.rs`) / socket2（multicast hop limit 設定に新規追加、lock に 0.6.4 が既存）

**Spec:** `docs/superpowers/specs/2026-07-12-phase5-m5-group-native-design.md`（決定 1〜5・受け入れ基準はここが正）

## Global Constraints

- **作業場所**: worktree `/home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/phase5-m1-controller-core`、ブランチ `matter-controller`。**各タスク冒頭で `pwd` と `git branch --show-current` を必ず確認**（サブエージェントの shell は main の repo で始まる — 過去に誤コミット事故あり）。main へのマージ・コミットは禁止。
- コミット前に `task check`（fmt:check + clippy -D warnings + test）全通過。
- repo は public。実クレデンシャル・実 node id・実 IP をコード/フィクスチャに書かない（group id 10・keyset 60・fabric index 2 は既存 spec に載っており可。鍵はダミー値のみ）。
- TDD: 失敗するテストを先に書く。
- 実機で確認済みの KVS 事実（この計画の前提、2026-07-12 採取）:
  - GroupKeyMap: `f/<idx>/gk/<n:x>`（n は 1 始まりの連番、hex 小文字）。blob は
    TLV `struct{ ctx1: group_id(uint), ctx2: keyset_id(uint), ctx3: next(uint) }`。
    chip-tool 組み込みサンプル（group 0x101..0x103 → keyset 0x1a1..0x1a3）が先頭に居座るので走査が必要。
  - keyset: `f/<idx>/k/<keyset_id:x>`。blob は `struct{ ctx1: policy, ctx2: keys_count,
    ctx3: array[3] of struct{ ctx4: start_time, ctx5: hash(u16, = Group Session ID),
    ctx6: bytes16(operational key) }, ctx7: next }`（`kvs.rs::parse_keyset` が既にほぼ同構造をパース、hash を捨てているだけ）。
  - `g/gdc`: base64 の 4 バイト u32 LE（chip-tool の Global Group Data Counter 永続値）。

---

### Task 1: kvs — group credentials / g/gdc リーダ

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`

**Interfaces:**
- Produces:
  - `pub struct GroupCredentials { pub session_id: u16, pub encryption_key: [u8; 16] }`（手動 `Debug` で鍵は `[REDACTED]` — 既存 `RawFabricCredentials` と同じ流儀）
  - `pub fn read_group_credentials(path: &Path, fabric_index: u8, group_id: u16) -> Result<GroupCredentials, KvsError>`
  - `pub fn read_group_data_counter(path: &Path) -> Result<Option<u32>, KvsError>`
  - `KvsError` 新 variant: `GroupNotFound { fabric_index: u8, group_id: u16 }`、`BadCounter(&'static str)`（`Display` 実装も追加）

- [ ] **Step 1: 失敗するテストを書く**

`kvs.rs` の `mod tests` に追加（既存 `write_ini` / `Writer` ヘルパを利用）。keyset blob ヘルパは既存 `keyset_blob_with_count` が hash=0x1234 固定で焼いているので、hash を引数化した新ヘルパを足す:

```rust
fn keymap_blob(group_id: u16, keyset_id: u16, next: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(group_id));
    w.put_uint(Tag::Context(2), u64::from(keyset_id));
    w.put_uint(Tag::Context(3), u64::from(next));
    w.end_container();
    w.finish()
}

fn keyset_blob_with_hash(key: &[u8; 16], hash: u16) -> Vec<u8> {
    // keyset_blob_with_count と同構造だが最初のエントリの ctx5 に hash を焼く
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), 0);
    w.put_uint(Tag::Context(2), 1);
    w.start_array(Tag::Context(3));
    for i in 0..3u8 {
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(4), u64::from(i == 0));
        w.put_uint(Tag::Context(5), if i == 0 { u64::from(hash) } else { 0 });
        w.put_bytes(Tag::Context(6), if i == 0 { key } else { &[0u8; 16] });
        w.end_container();
    }
    w.end_container();
    w.put_uint(Tag::Context(7), 0);
    w.end_container();
    w.finish()
}

const GROUP_KEY: [u8; 16] = [0xDD; 16];

#[test]
fn reads_group_credentials_scanning_past_builtin_entries() {
    // 実機と同形: chip-tool 組み込みサンプル (0x101→0x1a1) が先に居て、
    // 本命 (group 10 → keyset 0x3c) が gk/4 に居る。
    let path = write_ini(&[
        ("f/2/gk/1", &keymap_blob(0x101, 0x1a1, 2)[..]),
        ("f/2/gk/4", &keymap_blob(10, 0x3c, 0)[..]),
        ("f/2/k/1a1", &keyset_blob_with_hash(&[0xEE; 16], 0x1111)[..]),
        ("f/2/k/3c", &keyset_blob_with_hash(&GROUP_KEY, 0x855f)[..]),
    ]);
    let c = read_group_credentials(&path, 2, 10).unwrap();
    assert_eq!(c.session_id, 0x855f);
    assert_eq!(c.encryption_key, GROUP_KEY);
}

#[test]
fn group_not_in_keymap_is_group_not_found() {
    let path = write_ini(&[("f/2/gk/1", &keymap_blob(0x101, 0x1a1, 0)[..])]);
    assert!(matches!(
        read_group_credentials(&path, 2, 10),
        Err(KvsError::GroupNotFound { fabric_index: 2, group_id: 10 })
    ));
}

#[test]
fn keymap_hit_without_keyset_blob_is_key_missing() {
    let path = write_ini(&[("f/2/gk/1", &keymap_blob(10, 0x3c, 0)[..])]);
    assert!(matches!(
        read_group_credentials(&path, 2, 10),
        Err(KvsError::KeyMissing(k)) if k == "f/2/k/3c"
    ));
}

#[test]
fn malformed_keymap_entry_is_skipped() {
    // gk/1 が壊れていても gk/2 の本命は見つかる（容認的走査）。
    let path = write_ini(&[
        ("f/2/gk/1", &[0xFF, 0x00][..]),
        ("f/2/gk/2", &keymap_blob(10, 0x3c, 0)[..]),
        ("f/2/k/3c", &keyset_blob_with_hash(&GROUP_KEY, 0x855f)[..]),
    ]);
    assert_eq!(read_group_credentials(&path, 2, 10).unwrap().session_id, 0x855f);
}

#[test]
fn reads_group_data_counter_u32_le() {
    let path = write_ini(&[("g/gdc", &175851168u32.to_le_bytes()[..])]);
    assert_eq!(read_group_data_counter(&path).unwrap(), Some(175851168));
}

#[test]
fn missing_gdc_is_none_and_bad_length_is_error() {
    let none = write_ini(&[("f/2/n", &[0u8][..])]);
    assert_eq!(read_group_data_counter(&none).unwrap(), None);
    let bad = write_ini(&[("g/gdc", &[1u8, 2, 3][..])]);
    assert!(matches!(read_group_data_counter(&bad), Err(KvsError::BadCounter(_))));
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-controller kvs`
Expected: FAIL（`read_group_credentials` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装**

`kvs.rs` へ。既存 `parse_key_struct`（keyset の key entry パーサ）を hash も返すよう拡張し、既存 `parse_keyset`（IPK 用）は hash を捨てて従来通り:

```rust
/// Group send credentials from the GroupKeyMap + keyset blob: the group
/// session id (the keyset's GKH) and the operational encryption key.
#[derive(Clone)]
pub struct GroupCredentials {
    pub session_id: u16,
    pub encryption_key: [u8; 16],
}

impl std::fmt::Debug for GroupCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupCredentials")
            .field("session_id", &self.session_id)
            .field("encryption_key", &"[REDACTED]")
            .finish()
    }
}
```

`parse_key_struct` の返り値を `([u8; 16], u16)`（key, hash）に変更。ループ内に
`(Tag::Context(5), Value::Uint(v))` の腕を足して hash を取り、`u16::try_from`
失敗は `BadKeyset { reason: "hash out of range" }`。hash 欠落も `BadKeyset`
（`reason: "missing key hash"`）。呼び出し元 `parse_keyset` は `.0` を使う。

```rust
/// KeyMapData blob (`f/<idx>/gk/<n>`): struct{ ctx1: group_id, ctx2:
/// keyset_id, ctx3: next }. Verified against a live v1.4.2.0 store.
/// Malformed entries yield `None` so the scan can skip them.
fn parse_keymap_entry(blob: &[u8]) -> Option<(u16, u16)> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    let (mut group, mut keyset) = (None, None);
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Uint(v)) => group = u16::try_from(v).ok(),
            (Tag::Context(2), Value::Uint(v)) => keyset = u16::try_from(v).ok(),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_rest_of_container(&mut r, 0).ok()?;
            }
            _ => {}
        }
    }
    Some((group?, keyset?))
}

/// Reads the send credentials for `group_id`: scans the GroupKeyMap
/// (`f/<idx>/gk/1..=ff`, sparse after removals, so no early stop) for the
/// keyset id, then takes the first key entry's hash + operational key from
/// the keyset blob.
pub fn read_group_credentials(
    path: &Path,
    fabric_index: u8,
    group_id: u16,
) -> Result<GroupCredentials, KvsError> {
    let text = std::fs::read_to_string(path).map_err(KvsError::Io)?;
    let section = default_section(&text).ok_or(KvsError::SectionMissing)?;
    let mut keyset_id = None;
    for n in 1u32..=0xff {
        let Some(blob) = decode_b64(section, &format!("f/{fabric_index}/gk/{n:x}"))? else {
            continue;
        };
        if let Some((gid, ksid)) = parse_keymap_entry(&blob) {
            if gid == group_id {
                keyset_id = Some(ksid);
                break;
            }
        }
    }
    let keyset_id = keyset_id.ok_or(KvsError::GroupNotFound {
        fabric_index,
        group_id,
    })?;
    let key = format!("f/{fabric_index}/k/{keyset_id:x}");
    let blob = decode_b64(section, &key)?.ok_or(KvsError::KeyMissing(key))?;
    // parse_keyset と同じ枠組みで最初の key entry の (key, hash) を取る。
    let (encryption_key, session_id) = parse_keyset_first_entry(&blob, fabric_index)?;
    Ok(GroupCredentials {
        session_id,
        encryption_key,
    })
}

/// Reads chip-tool's persisted Global Group Data Counter (`g/gdc`, u32 LE).
pub fn read_group_data_counter(path: &Path) -> Result<Option<u32>, KvsError> {
    let text = std::fs::read_to_string(path).map_err(KvsError::Io)?;
    let section = default_section(&text).ok_or(KvsError::SectionMissing)?;
    match decode_b64(section, "g/gdc")? {
        None => Ok(None),
        Some(b) => {
            let arr: [u8; 4] = b
                .as_slice()
                .try_into()
                .map_err(|_| KvsError::BadCounter("g/gdc must be 4 bytes"))?;
            Ok(Some(u32::from_le_bytes(arr)))
        }
    }
}
```

実装メモ: `parse_keyset_first_entry` は既存 `parse_keyset` の中身を「(key, hash) を返す」形に一般化して共有する（`parse_keyset` はその薄いラッパにして IPK 動作・エラー reason を不変に保つ）。`skip_rest_of_container` の `fabric_index` 引数は `parse_keymap_entry` では 0 でよい（エラーは `.ok()?` で捨てるため）。

- [ ] **Step 4: テスト全通過を確認**

Run: `cargo test -p mat-controller kvs`
Expected: PASS（既存 IPK テスト含め全部）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/kvs.rs
git commit -m "feat(controller): read group send credentials + g/gdc from chip-tool KVS (M5)"
```

---

### Task 2: group モジュール — multicast アドレスと PersistedGroupCounter

**Files:**
- Create: `crates/mat-controller/src/group.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod group;` 追加）

**Interfaces:**
- Produces:
  - `pub fn group_multicast_addr(fabric_id: u64, group_id: u16) -> std::net::Ipv6Addr`
  - `pub const COUNTER_EPOCH: u32 = 4096;`
  - `pub struct PersistedGroupCounter`、`pub fn load(path: &Path, chip_tool_gdc: u32) -> io::Result<Self>`、`pub fn next(&mut self) -> io::Result<u32>`

- [ ] **Step 1: 失敗するテストを書く**（`group.rs` 内 `mod tests`）

```rust
#[test]
fn multicast_addr_packs_fabric_and_group() {
    // FF35:0040:FD || fabric_id(8B BE) || 00 || group_id(2B BE)
    assert_eq!(
        group_multicast_addr(0x1122334455667788, 0xaabb),
        std::net::Ipv6Addr::new(0xff35, 0x0040, 0xfd11, 0x2233, 0x4455, 0x6677, 0x8800, 0xaabb)
    );
    assert_eq!(
        group_multicast_addr(1, 10),
        std::net::Ipv6Addr::new(0xff35, 0x0040, 0xfd00, 0, 0, 0, 0x0100, 0x000a)
    );
}

fn tmp_counter_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("mat-group-counter-{}-{tag}", std::process::id()))
}

#[test]
fn counter_starts_above_both_sources_plus_epoch() {
    let p = tmp_counter_path("fresh");
    let _ = std::fs::remove_file(&p);
    let mut c = PersistedGroupCounter::load(&p, 1000).unwrap();
    assert_eq!(c.next().unwrap(), 1000 + COUNTER_EPOCH);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn counter_reload_never_reuses_values() {
    let p = tmp_counter_path("reload");
    let _ = std::fs::remove_file(&p);
    let mut c = PersistedGroupCounter::load(&p, 0).unwrap();
    let mut last = 0;
    for _ in 0..10 {
        last = c.next().unwrap();
    }
    drop(c);
    // 再起動相当: chip-tool 側が 0 でも、自前永続値から必ず上へ跳ぶ。
    let mut c2 = PersistedGroupCounter::load(&p, 0).unwrap();
    assert!(c2.next().unwrap() > last);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn counter_gdc_wins_when_larger_than_own_file() {
    let p = tmp_counter_path("gdcwins");
    let _ = std::fs::remove_file(&p);
    drop(PersistedGroupCounter::load(&p, 0).unwrap()); // 小さい自前値を永続化
    let mut c = PersistedGroupCounter::load(&p, 900_000).unwrap();
    assert!(c.next().unwrap() >= 900_000 + COUNTER_EPOCH);
    let _ = std::fs::remove_file(&p);
}

#[test]
fn counter_persists_ahead_across_epoch_boundary() {
    let p = tmp_counter_path("epoch");
    let _ = std::fs::remove_file(&p);
    let mut c = PersistedGroupCounter::load(&p, 0).unwrap();
    let mut prev = None;
    for _ in 0..(COUNTER_EPOCH + 5) {
        let v = c.next().unwrap();
        if let Some(p) = prev {
            assert_eq!(v, p + 1, "strictly sequential across the persist boundary");
        }
        prev = Some(v);
    }
    let _ = std::fs::remove_file(&p);
}

#[test]
fn counter_corrupt_file_is_an_error() {
    let p = tmp_counter_path("corrupt");
    std::fs::write(&p, "not a number").unwrap();
    assert!(PersistedGroupCounter::load(&p, 0).is_err());
    let _ = std::fs::remove_file(&p);
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-controller group`
Expected: FAIL（モジュール未定義）

- [ ] **Step 3: 実装**

```rust
//! Groupcast send support (M5): multicast destination address and the
//! persisted global group data counter.
//!
//! The counter shares one space with chip-tool (same source node id), so it
//! never restarts low: it persists ahead of use (SDK PersistedCounter
//! semantics) and boot-jumps past both its own file and chip-tool's `g/gdc`.

use std::io;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};

/// Persist-ahead window: the file always stores a value no counter below
/// which has been handed out, so a crash can never reuse a sent counter.
pub const COUNTER_EPOCH: u32 = 4096;

/// Matter site-local transient multicast group address (spec §2.5.9.2):
/// `FF35:0040:FD || fabric_id(8B BE) || 00 || group_id(2B BE)`.
pub fn group_multicast_addr(fabric_id: u64, group_id: u16) -> Ipv6Addr {
    let f = fabric_id.to_be_bytes();
    let g = group_id.to_be_bytes();
    Ipv6Addr::from([
        0xff, 0x35, 0x00, 0x40, 0xfd, f[0], f[1], f[2], f[3], f[4], f[5], f[6], f[7], 0x00,
        g[0], g[1],
    ])
}

/// Global Group Data Counter with persist-ahead storage (decimal text file).
pub struct PersistedGroupCounter {
    next: u32,
    ceiling: u32,
    path: PathBuf,
}

impl PersistedGroupCounter {
    /// Starts from `max(own persisted ceiling, chip-tool g/gdc) + EPOCH` and
    /// persists the new ceiling before returning. A corrupt counter file is
    /// an error (starting low would get every send dropped by receivers).
    pub fn load(path: &Path, chip_tool_gdc: u32) -> io::Result<Self> {
        let persisted = match std::fs::read_to_string(path) {
            Ok(s) => s.trim().parse::<u32>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "corrupt group counter file")
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let start = persisted.max(chip_tool_gdc).wrapping_add(COUNTER_EPOCH);
        let mut c = Self {
            next: start,
            ceiling: start,
            path: path.to_path_buf(),
        };
        c.persist(start.wrapping_add(COUNTER_EPOCH))?;
        Ok(c)
    }

    /// Returns the counter to send with and advances. Crossing the persisted
    /// ceiling persists the next window first.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> io::Result<u32> {
        if self.next == self.ceiling {
            self.persist(self.ceiling.wrapping_add(COUNTER_EPOCH))?;
        }
        let v = self.next;
        self.next = self.next.wrapping_add(1);
        Ok(v)
    }

    /// Atomic write (tmp + fsync + rename) so a crash never leaves a
    /// truncated value behind.
    fn persist(&mut self, ceiling: u32) -> io::Result<()> {
        use std::io::Write;
        let tmp = self.path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(format!("{ceiling}\n").as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        self.ceiling = ceiling;
        Ok(())
    }
}
```

`lib.rs` に `pub mod group;` を追加（既存モジュール宣言の並びに合わせる）。

- [ ] **Step 4: テスト全通過を確認**

Run: `cargo test -p mat-controller group`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/group.rs crates/mat-controller/src/lib.rs
git commit -m "feat(controller): group multicast addr + persisted group counter (M5)"
```

---

### Task 3: im — group 版 InvokeRequest エンコーダ

**Files:**
- Modify: `crates/mat-controller/src/im.rs`

**Interfaces:**
- Consumes: 既存 `Writer` / `copy_retagged` / `IM_REVISION`（`encode_invoke_request` と同じ部品）
- Produces: `pub fn encode_group_invoke_request(cluster: u32, command: u32, fields_tlv: Option<&[u8]>) -> Vec<u8>`

- [ ] **Step 1: 失敗するテストを書く**（`im.rs` の `mod tests`。既存 invoke エンコーダテストの流儀＝`Writer` で期待バイト列を組んで比較、に合わせる）

```rust
#[test]
fn group_invoke_request_suppresses_response_and_omits_endpoint() {
    let got = encode_group_invoke_request(CLUSTER_ON_OFF, CMD_ON_OFF_ON, None);
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), true); // SuppressResponse: group は応答なし
    w.put_bool(Tag::Context(1), false); // TimedRequest
    w.start_array(Tag::Context(2));
    w.start_struct(Tag::Anonymous);
    w.start_list(Tag::Context(0)); // CommandPath: group-scoped、endpoint なし
    w.put_uint(Tag::Context(1), u64::from(CLUSTER_ON_OFF));
    w.put_uint(Tag::Context(2), u64::from(CMD_ON_OFF_ON));
    w.end_container();
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    assert_eq!(got, w.finish());
}

#[test]
fn group_invoke_request_carries_fields() {
    let fields = encode_move_to_color_temperature_fields(370, 0);
    let got = encode_group_invoke_request(
        CLUSTER_COLOR_CONTROL,
        CMD_MOVE_TO_COLOR_TEMPERATURE,
        Some(&fields),
    );
    // fields が ctx1 で CommandDataIB に入ること（unicast 版と同じ再タグ規約）。
    // 厳密比較: unicast 版のテストに倣い Writer で期待列を組む。
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), true);
    w.put_bool(Tag::Context(1), false);
    w.start_array(Tag::Context(2));
    w.start_struct(Tag::Anonymous);
    w.start_list(Tag::Context(0));
    w.put_uint(Tag::Context(1), u64::from(CLUSTER_COLOR_CONTROL));
    w.put_uint(Tag::Context(2), u64::from(CMD_MOVE_TO_COLOR_TEMPERATURE));
    w.end_container();
    w.start_struct(Tag::Context(1));
    w.put_uint(Tag::Context(0), 370);
    w.put_uint(Tag::Context(1), 0);
    w.put_uint(Tag::Context(2), 0);
    w.put_uint(Tag::Context(3), 0);
    w.end_container();
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    assert_eq!(got, w.finish());
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-controller im`
Expected: FAIL（関数未定義）

- [ ] **Step 3: 実装**（`encode_invoke_request` の直後に）

```rust
/// InvokeRequestMessage for a groupcast command (spec §8.9.4): group
/// invokes carry no response, so SuppressResponse is true, and the
/// CommandPath is group-scoped (no endpoint — the device's group table
/// routes to its bound endpoints). Fields contract matches
/// `encode_invoke_request`.
pub fn encode_group_invoke_request(
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), true); // SuppressResponse
    w.put_bool(Tag::Context(1), false); // TimedRequest
    w.start_array(Tag::Context(2)); // InvokeRequests
    w.start_struct(Tag::Anonymous); // CommandDataIB
    w.start_list(Tag::Context(0)); // CommandPath (group-scoped)
    w.put_uint(Tag::Context(1), u64::from(cluster));
    w.put_uint(Tag::Context(2), u64::from(command));
    w.end_container();
    if let Some(fields) = fields_tlv {
        let mut fr = Reader::new(fields);
        copy_retagged(&mut w, &mut fr, Tag::Context(1))
            .expect("fields_tlv must be one well-formed TLV element");
    }
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}
```

- [ ] **Step 4: テスト全通過を確認**

Run: `cargo test -p mat-controller im`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/im.rs
git commit -m "feat(controller): group-scoped InvokeRequest encoder (M5)"
```

---

### Task 4: GroupSender — 暗号化データグラム組み立てと multicast 送信

**Files:**
- Modify: `crates/mat-controller/src/group.rs`
- Modify: `crates/mat-controller/src/transport.rs`（multicast hop limit 設定口）
- Modify: `crates/mat-controller/Cargo.toml`（`socket2 = "0.6"` 追加 — lock に 0.6.4 が既存）

**Interfaces:**
- Consumes: Task 1 `GroupCredentials`、Task 2 `group_multicast_addr` / `PersistedGroupCounter`、Task 3 `encode_group_invoke_request`、既存 `crypto::seal_message` / `open_message`、`MessageHeader` / `ProtocolHeader` / `Destination`
- Produces:
  - `UdpTransport::set_multicast_hops_v6(&self, hops: u32) -> io::Result<()>`
  - `pub const GROUP_SECURITY_FLAGS: u8 = 0x01;`（session type = group）
  - `pub const MULTICAST_HOP_LIMIT: u32 = 64;`
  - `pub enum GroupSendError { Crypto(CryptoError), Io(io::Error) }`（`Display`/`Error` 実装付き）
  - `pub fn build_group_datagram(creds: &GroupCredentials, source_node_id: u64, counter: u32, exchange_id: u16, group_id: u16, cluster: u32, command: u32, fields_tlv: Option<&[u8]>) -> Result<Vec<u8>, CryptoError>`
  - `pub struct GroupSender` / `pub fn new(transport: Arc<UdpTransport>, scope_id: u32, dest_port: u16, fabric_id: u64, source_node_id: u64, counter: PersistedGroupCounter) -> io::Result<Self>` / `pub async fn send_invoke(&mut self, creds: &GroupCredentials, group_id: u16, cluster: u32, command: u32, fields_tlv: Option<&[u8]>) -> Result<u32, GroupSendError>`（送った counter 値を返す — matd がログに出す）

- [ ] **Step 1: 失敗するテストを書く**（`group.rs` の `mod tests` に追加）

```rust
use crate::im::{CLUSTER_ON_OFF, CMD_ON_OFF_ON, OPCODE_INVOKE_REQUEST, PROTOCOL_ID_IM};
use crate::kvs::GroupCredentials;
use crate::message::{Destination, MessageHeader};

fn test_creds() -> GroupCredentials {
    GroupCredentials {
        session_id: 0x855f,
        encryption_key: [0xDD; 16],
    }
}

#[test]
fn group_datagram_roundtrips_with_group_header() {
    let dg =
        build_group_datagram(&test_creds(), 0x0001_0001, 5000, 0x42, 10, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None)
            .unwrap();
    // 平文ヘッダ: DSIZ=group(2) + S flag、session type = group。
    let (header, _) = MessageHeader::decode(&dg).unwrap();
    assert_eq!(header.session_id, 0x855f);
    assert_eq!(header.security_flags, GROUP_SECURITY_FLAGS);
    assert_eq!(header.message_counter, 5000);
    assert_eq!(header.source_node_id, Some(0x0001_0001));
    assert_eq!(header.destination, Destination::Group(10));
    // 復号して protocol header / payload を確認（nonce・AAD が正しい証拠）。
    let (h2, proto, payload) =
        crate::crypto::open_message(&test_creds().encryption_key, &dg, 0x0001_0001).unwrap();
    assert_eq!(h2, header);
    assert!(proto.initiator);
    assert!(!proto.needs_ack);
    assert_eq!(proto.opcode, OPCODE_INVOKE_REQUEST);
    assert_eq!(proto.protocol_id, PROTOCOL_ID_IM);
    assert_eq!(
        payload,
        crate::im::encode_group_invoke_request(CLUSTER_ON_OFF, CMD_ON_OFF_ON, None)
    );
}

#[tokio::test]
async fn group_sender_multicasts_on_loopback() {
    use crate::transport::UdpTransport;
    // 受信側: エフェメラルポートに bind して lo (ifindex 1) で group join。
    let recv = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
    let port = recv.local_addr().unwrap().port();
    let addr = group_multicast_addr(1, 10);
    recv.join_multicast_v6(&addr, 1).unwrap();

    let p = tmp_counter_path("sender");
    let _ = std::fs::remove_file(&p);
    let counter = PersistedGroupCounter::load(&p, 0).unwrap();
    let transport = std::sync::Arc::new(UdpTransport::bind().await.unwrap());
    let mut s = GroupSender::new(transport, 1, port, 1, 0x0001_0001, counter).unwrap();
    let sent_counter = s
        .send_invoke(&test_creds(), 10, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None)
        .await
        .unwrap();

    let mut buf = [0u8; 1280];
    let (n, _) = tokio::time::timeout(std::time::Duration::from_secs(2), recv.recv_from(&mut buf))
        .await
        .expect("multicast datagram should arrive on loopback")
        .unwrap();
    let (header, _) = MessageHeader::decode(&buf[..n]).unwrap();
    assert_eq!(header.destination, Destination::Group(10));
    assert_eq!(header.message_counter, sent_counter);
    let _ = std::fs::remove_file(&p);
}
```

`open_message` の返り値シグネチャは `crypto.rs` の実物を確認して合わせる（`(MessageHeader, ProtocolHeader, Vec<u8>)` 想定。異なる場合はテスト側を実シグネチャへ）。

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-controller group`
Expected: FAIL（`build_group_datagram` / `GroupSender` 未定義）

- [ ] **Step 3: 実装**

`Cargo.toml` の `[dependencies]` に `socket2 = "0.6"`。

`transport.rs` に追加:

```rust
/// Sets the hop limit for multicast sends. The OS default of 1 never
/// crosses the border router, so groupcast callers must raise it
/// (Matter SDK uses 64). Unicast sends are unaffected.
pub fn set_multicast_hops_v6(&self, hops: u32) -> io::Result<()> {
    socket2::SockRef::from(&self.socket).set_multicast_hops_v6(hops)
}
```

`group.rs` に追加:

```rust
use std::net::{SocketAddr, SocketAddrV6};
use std::sync::Arc;

use crate::crypto::{self, CryptoError};
use crate::im;
use crate::kvs::GroupCredentials;
use crate::message::{Destination, MessageHeader, ProtocolHeader};
use crate::transport::UdpTransport;

/// Security flags for a group session data message (spec §4.4.1.4:
/// session type = 1, no privacy).
pub const GROUP_SECURITY_FLAGS: u8 = 0x01;

/// Multicast hop limit for groupcast sends (Matter SDK default).
pub const MULTICAST_HOP_LIMIT: u32 = 64;

/// Groupcast send failure: encryption (caller bug / oversized payload) or
/// socket I/O.
#[derive(Debug)]
pub enum GroupSendError {
    Crypto(CryptoError),
    Io(std::io::Error),
}

impl std::fmt::Display for GroupSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Crypto(e) => write!(f, "group message encryption: {e}"),
            Self::Io(e) => write!(f, "group multicast send: {e}"),
        }
    }
}

impl std::error::Error for GroupSendError {}

/// Builds the encrypted groupcast datagram (pure; unit-testable without
/// sockets): group-session plain header + CCM-sealed group InvokeRequest.
#[allow(clippy::too_many_arguments)]
pub fn build_group_datagram(
    creds: &GroupCredentials,
    source_node_id: u64,
    counter: u32,
    exchange_id: u16,
    group_id: u16,
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
) -> Result<Vec<u8>, CryptoError> {
    let header = MessageHeader {
        session_id: creds.session_id,
        security_flags: GROUP_SECURITY_FLAGS,
        message_counter: counter,
        source_node_id: Some(source_node_id),
        destination: Destination::Group(group_id),
    };
    let proto = ProtocolHeader {
        initiator: true,
        needs_ack: false, // groupcast is unreliable by spec — no MRP
        acked_counter: None,
        opcode: im::OPCODE_INVOKE_REQUEST,
        exchange_id,
        protocol_id: im::PROTOCOL_ID_IM,
        vendor_id: None,
    };
    let payload = im::encode_group_invoke_request(cluster, command, fields_tlv);
    crypto::seal_message(&creds.encryption_key, &header, &proto, &payload, source_node_id)
}

/// Send-only groupcast path. Holds no per-group key state: credentials are
/// passed per call (the caller re-reads the KVS so re-provisioned keys are
/// picked up immediately).
pub struct GroupSender {
    transport: Arc<UdpTransport>,
    scope_id: u32,
    dest_port: u16,
    fabric_id: u64,
    source_node_id: u64,
    counter: PersistedGroupCounter,
}

impl GroupSender {
    /// Configures the shared socket's multicast hop limit and assembles the
    /// sender. `dest_port` is `message::MATTER_PORT` in production (tests
    /// point it at an ephemeral receiver).
    pub fn new(
        transport: Arc<UdpTransport>,
        scope_id: u32,
        dest_port: u16,
        fabric_id: u64,
        source_node_id: u64,
        counter: PersistedGroupCounter,
    ) -> std::io::Result<Self> {
        transport.set_multicast_hops_v6(MULTICAST_HOP_LIMIT)?;
        Ok(Self {
            transport,
            scope_id,
            dest_port,
            fabric_id,
            source_node_id,
            counter,
        })
    }

    /// Fire-and-forget groupcast InvokeRequest (single send, no response,
    /// no retransmit). Returns the message counter used, for logging.
    pub async fn send_invoke(
        &mut self,
        creds: &GroupCredentials,
        group_id: u16,
        cluster: u32,
        command: u32,
        fields_tlv: Option<&[u8]>,
    ) -> Result<u32, GroupSendError> {
        let counter = self.counter.next().map_err(GroupSendError::Io)?;
        let mut ex = [0u8; 2];
        getrandom::getrandom(&mut ex).expect("os rng");
        let datagram = build_group_datagram(
            creds,
            self.source_node_id,
            counter,
            u16::from_le_bytes(ex),
            group_id,
            cluster,
            command,
            fields_tlv,
        )
        .map_err(GroupSendError::Crypto)?;
        let dest = SocketAddr::V6(SocketAddrV6::new(
            group_multicast_addr(self.fabric_id, group_id),
            self.dest_port,
            0,
            // multicast 宛先では sin6_scope_id が送出 iface を選ぶ
            self.scope_id,
        ));
        self.transport
            .send_to(&datagram, dest)
            .await
            .map_err(GroupSendError::Io)?;
        Ok(counter)
    }
}
```

- [ ] **Step 4: テスト全通過を確認**

Run: `cargo test -p mat-controller group`
Expected: PASS（loopback multicast テスト含む。CI コンテナ等で lo の multicast join が EPERM になる場合のみ、そのテストに `#[ignore]` を付けず原因を調査 — Linux 実行前提なので通るはず）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/group.rs crates/mat-controller/src/transport.rs crates/mat-controller/Cargo.toml Cargo.lock
git commit -m "feat(controller): GroupSender — encrypted groupcast over ff35:: multicast (M5)"
```

---

### Task 5: matd native — group 送信コンテキストと group_invoke API

**Files:**
- Modify: `crates/matd/src/native.rs`

**Interfaces:**
- Consumes: Task 1 `kvs::{read_group_credentials, read_group_data_counter}`、Task 4 `group::{GroupSender, PersistedGroupCounter}`、`message::MATTER_PORT`
- Produces:
  - `pub(crate) struct GroupCtx { pub main_ini: PathBuf, pub counter_path: PathBuf, pub fabric_index: u8, pub fabric_id: u64, pub node_id: u64, pub scope_id: u32, pub dest_port: u16, pub transport: Arc<UdpTransport>, pub sender: Mutex<Option<GroupSender>> }`
  - `pub enum GroupOutcome { Sent, Unavailable(String) }`
  - `NativeBackend::group_invoke(&self, group_id: u16, cluster: u32, command: u32, fields: Option<Vec<u8>>) -> Result<GroupOutcome, MatError>`
  - `NativeBackend::with_parts(establisher: Box<dyn Establisher>, group: Option<GroupCtx>) -> Self`（テスト用。既存 `with_establisher` は `with_parts(e, None)` に委譲）

- [ ] **Step 1: 失敗するテストを書く**（`native.rs` の `mod tests`）

```rust
#[tokio::test]
async fn group_invoke_without_ctx_is_unavailable() {
    let b = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
    let r = b
        .group_invoke(10, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None)
        .await
        .unwrap();
    assert!(matches!(r, GroupOutcome::Unavailable(_)));
}

#[tokio::test]
async fn group_invoke_sends_multicast_and_reports_sent() {
    // フィクスチャ ini（gk + keyset + g/gdc）と loopback 受信で end-to-end。
    let dir = std::env::temp_dir().join(format!("mat-native-group-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ini = dir.join("chip_tool_config.ini");
    write_group_fixture_ini(&ini); // 下の実装メモ参照

    let recv = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
    let port = recv.local_addr().unwrap().port();
    recv.join_multicast_v6(&mat_controller::group::group_multicast_addr(1, 10), 1)
        .unwrap();

    let transport = Arc::new(UdpTransport::bind().await.unwrap());
    let ctx = GroupCtx {
        main_ini: ini,
        counter_path: dir.join("native_group_counter"),
        fabric_index: 2,
        fabric_id: 1,
        node_id: 0x0001_0001,
        scope_id: 1,
        dest_port: port,
        transport,
        sender: tokio::sync::Mutex::new(None),
    };
    let b = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), Some(ctx));
    let r = b
        .group_invoke(10, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None)
        .await
        .unwrap();
    assert!(matches!(r, GroupOutcome::Sent));
    let mut buf = [0u8; 1280];
    tokio::time::timeout(std::time::Duration::from_secs(2), recv.recv_from(&mut buf))
        .await
        .expect("datagram should arrive")
        .unwrap();

    // 未 provision group は Unavailable（フォールバックさせる）。
    let r = b
        .group_invoke(99, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None)
        .await
        .unwrap();
    assert!(matches!(r, GroupOutcome::Unavailable(_)));
    let _ = std::fs::remove_dir_all(&dir);
}
```

実装メモ（テストヘルパ）: `write_group_fixture_ini` は `mat_controller::tlv::Writer`（pub）で Task 1 と同じ構造の blob（`f/2/gk/1` = group 10 → keyset 0x3c、`f/2/k/3c` = hash 0x855f + ダミー鍵 `[0xDD;16]`、`g/gdc` = 1000 の u32 LE）を base64 で `[Default]` セクションに書く（`base64ct` が matd の依存に無ければ `[dev-dependencies]` に追加）。

**ヘルパの置き場所**: `FakeEstablisher`（既存、`native.rs` の `mod tests` 内）と
`write_group_fixture_ini` は Task 6 の server ルーティングテストからも使うため、
`native.rs` に `#[cfg(test)] pub(crate) mod test_support { ... }` を作ってそこへ
移し、`mod tests` は `use super::test_support::*;` で参照する（既存テストの挙動
不変のリファクタ）。

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p matd native`
Expected: FAIL（`GroupOutcome` / `with_parts` 未定義）

- [ ] **Step 3: 実装**

```rust
use mat_controller::group::{GroupSender, PersistedGroupCounter};
use mat_controller::kvs;
use mat_controller::message::MATTER_PORT;

/// group 送信に必要な材料一式。`sender`（counter を内包）は初 group op で
/// lazy 構築する。鍵は send のたびに KVS から読む（`provision --rebind`
/// 直後でも stale にならない）。
pub(crate) struct GroupCtx {
    pub main_ini: PathBuf,
    pub counter_path: PathBuf,
    pub fabric_index: u8,
    pub fabric_id: u64,
    pub node_id: u64,
    pub scope_id: u32,
    pub dest_port: u16,
    pub transport: Arc<UdpTransport>,
    pub sender: Mutex<Option<GroupSender>>,
}

/// group 送信の結果。`Unavailable` は「native では送れない（未 provision・
/// KVS 不備等）」で、server 層が chip-tool へフォールバックする合図。
pub enum GroupOutcome {
    Sent,
    Unavailable(String),
}
```

`NativeBackend` に `group: Option<GroupCtx>` フィールドを追加。`build` は
`read_self_issue_materials` → `FabricCredentials` 構築後、establisher に move
する**前に** `creds.fabric_id` / `creds.node_id` を控え、`transport` は
`Arc::new` 後に `Arc::clone` で共有して `GroupCtx` を組む:

```rust
// build() 内、CaseEstablisher 構築部を変更:
let transport = Arc::new(transport);
let group = GroupCtx {
    main_ini,
    counter_path: cfg.store.join("native_group_counter"),
    fabric_index: cfg.fabric_index,
    fabric_id: creds.fabric_id,
    node_id: creds.node_id,
    scope_id,
    dest_port: MATTER_PORT,
    transport: Arc::clone(&transport),
    sender: Mutex::new(None),
};
let establisher = CaseEstablisher {
    creds: Arc::new(creds),
    transport,
    scope_id,
};
Ok(Self::with_parts(Box::new(establisher), Some(group)))
```

（`main_ini` は build 冒頭で `cfg.store.join("chip_tool_config.ini")` として既に
あるので move 前に clone する。）

```rust
impl NativeBackend {
    /// group へ groupcast を 1 発送る。native で送れない事情（未 provision・
    /// KVS 不備・counter 初期化不能）は `Unavailable` で返し、送出自体の失敗
    /// （socket）だけを Err にする。
    pub async fn group_invoke(
        &self,
        group_id: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
    ) -> Result<GroupOutcome, MatError> {
        let Some(ctx) = &self.group else {
            return Ok(GroupOutcome::Unavailable(
                "native group context not configured".into(),
            ));
        };
        let creds = match kvs::read_group_credentials(&ctx.main_ini, ctx.fabric_index, group_id) {
            Ok(c) => c,
            Err(e) => {
                return Ok(GroupOutcome::Unavailable(format!(
                    "group {group_id} credentials: {e} (not provisioned? run `mat group provision`)"
                )))
            }
        };
        let mut slot = ctx.sender.lock().await;
        if slot.is_none() {
            let gdc = match kvs::read_group_data_counter(&ctx.main_ini) {
                Ok(Some(v)) => v,
                Ok(None) => {
                    return Ok(GroupOutcome::Unavailable(
                        "chip-tool g/gdc missing; refusing to start the group counter low".into(),
                    ))
                }
                Err(e) => return Ok(GroupOutcome::Unavailable(format!("read g/gdc: {e}"))),
            };
            let counter = match PersistedGroupCounter::load(&ctx.counter_path, gdc) {
                Ok(c) => c,
                Err(e) => {
                    return Ok(GroupOutcome::Unavailable(format!("group counter store: {e}")))
                }
            };
            match GroupSender::new(
                Arc::clone(&ctx.transport),
                ctx.scope_id,
                ctx.dest_port,
                ctx.fabric_id,
                ctx.node_id,
                counter,
            ) {
                Ok(s) => *slot = Some(s),
                Err(e) => {
                    return Ok(GroupOutcome::Unavailable(format!(
                        "multicast socket setup: {e}"
                    )))
                }
            }
        }
        match slot
            .as_mut()
            .expect("built above")
            .send_invoke(&creds, group_id, cluster, command, fields.as_deref())
            .await
        {
            Ok(counter) => {
                tracing::info!(group_id, counter, "groupcast sent (native)");
                Ok(GroupOutcome::Sent)
            }
            Err(e) => Err(MatError::new(
                ErrorKind::Unreachable,
                format!("groupcast send to group {group_id}: {e}"),
            )),
        }
    }
}
```

`with_establisher` は `Self::with_parts(establisher, None)` へ委譲。

- [ ] **Step 4: テスト全通過を確認**

Run: `cargo test -p matd native`
Expected: PASS（既存 M4 テスト含む）

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/native.rs crates/matd/Cargo.toml Cargo.lock
git commit -m "feat(matd): native groupcast path with lazy GroupSender (M5)"
```

---

### Task 6: matd server — group 送信 3 op の native 振り分けとフォールバック

**Files:**
- Modify: `crates/matd/src/server.rs`

**Interfaces:**
- Consumes: Task 5 `NativeBackend::group_invoke` / `GroupOutcome`、`im` の cluster/command 定数とフィールドエンコーダ
- Produces:
  - `fn native_group_params(op: &Op) -> Option<(u16, u32, u32, Option<Vec<u8>>)>`
  - `fn group_sent_body(op: &Op) -> Value`（chip-tool 経路の `group_invoke` / `group_color_op` と共用）

- [ ] **Step 1: 失敗するテストを書く**（`server.rs` の `mod tests`）

```rust
#[test]
fn native_group_params_maps_onoff_and_shortcuts() {
    let on = Op::GroupInvoke {
        group_id: 10,
        cluster: "onoff".into(),
        command: "on".into(),
        args: vec![],
        endpoint: 1,
    };
    let (gid, cluster, command, fields) = native_group_params(&on).unwrap();
    assert_eq!((gid, cluster, command), (10, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON));
    assert!(fields.is_none());

    // 引数付き・onoff 以外・未知コマンドは native 対象外（chip-tool へ）。
    let with_args = Op::GroupInvoke {
        group_id: 10,
        cluster: "onoff".into(),
        command: "on".into(),
        args: vec!["1".into()],
        endpoint: 1,
    };
    assert!(native_group_params(&with_args).is_none());
    let other_cluster = Op::GroupInvoke {
        group_id: 10,
        cluster: "levelcontrol".into(),
        command: "move-to-level".into(),
        args: vec![],
        endpoint: 1,
    };
    assert!(native_group_params(&other_cluster).is_none());

    let ct = Op::GroupColorTemp {
        group_id: 10,
        mireds: 370,
        kelvin: 2702,
        transition: 0,
        endpoint: 1,
    };
    let (_, cluster, command, fields) = native_group_params(&ct).unwrap();
    assert_eq!(cluster, im::CLUSTER_COLOR_CONTROL);
    assert_eq!(command, im::CMD_MOVE_TO_COLOR_TEMPERATURE);
    assert_eq!(
        fields.unwrap(),
        im::encode_move_to_color_temperature_fields(370, 0)
    );

    let color = Op::GroupColor {
        group_id: 10,
        hue_raw: 180,
        saturation_raw: 200,
        hue: 254,
        saturation: 78,
        name: None,
        rgb: None,
        transition: 0,
        endpoint: 1,
    };
    let (_, cluster, command, fields) = native_group_params(&color).unwrap();
    assert_eq!(cluster, im::CLUSTER_COLOR_CONTROL);
    assert_eq!(command, im::CMD_MOVE_TO_HUE_AND_SATURATION);
    assert_eq!(
        fields.unwrap(),
        im::encode_move_to_hue_and_saturation_fields(180, 200, 0)
    );

    // GroupProvision は常に chip-tool。
    assert!(native_group_params(&Op::Ping).is_none());
}
```

さらに `run_op` レベルの routing テスト。`ChipToolBackend::connect(port, idle)` は
遅延接続（M4 決定 4）なので、native が処理すれば chip-tool には触れない —
接続不能ポートの backend を渡すことで経路を判別できる。store フィクスチャは
`crates/matd/tests/integration.rs` の `make_store` と同形（`tempfile` は
`[dev-dependencies]` に無ければ追加）。Task 5 の `test_support`
（`FakeEstablisher` / `write_group_fixture_ini`）を使う:

```rust
use crate::native::test_support::{write_group_fixture_ini, FakeEstablisher};

fn group_on_op() -> Op {
    Op::GroupInvoke {
        group_id: 10,
        cluster: "onoff".into(),
        command: "on".into(),
        args: vec![],
        endpoint: 1,
    }
}

/// 接続先の無い lazy backend（触られたら必ず接続エラー）。
async fn dead_backend() -> ChipToolBackend {
    ChipToolBackend::connect(1, std::time::Duration::from_secs(30))
        .await
        .unwrap()
}

fn make_store() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
    store
        .upsert_node(mat_core::store::NodeRecord {
            node_id: 1,
            address: Some("192.0.2.10".into()),
            commissioned_at: "2026-06-08T00:00:00+09:00".into(),
        })
        .unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

#[tokio::test]
async fn group_op_routes_native_when_available() {
    let (_dir, store_path) = make_store();
    let ini = store_path.join("chip_tool_config.ini");
    write_group_fixture_ini(&ini);
    let recv = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
    let port = recv.local_addr().unwrap().port();
    recv.join_multicast_v6(&mat_controller::group::group_multicast_addr(1, 10), 1)
        .unwrap();
    let transport = std::sync::Arc::new(
        mat_controller::transport::UdpTransport::bind().await.unwrap(),
    );
    let ctx = crate::native::GroupCtx {
        main_ini: ini,
        counter_path: store_path.join("native_group_counter"),
        fabric_index: 2,
        fabric_id: 1,
        node_id: 0x0001_0001,
        scope_id: 1,
        dest_port: port,
        transport,
        sender: tokio::sync::Mutex::new(None),
    };
    let native = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), Some(ctx));
    let backend = dead_backend().await;

    let body = run_op(&group_on_op(), &backend, Some(&native), &store_path)
        .await
        .unwrap();
    assert_eq!(body["status"], "sent"); // native 経路で chip-tool 不要のまま成功
    let mut buf = [0u8; 1280];
    tokio::time::timeout(std::time::Duration::from_secs(2), recv.recv_from(&mut buf))
        .await
        .expect("groupcast datagram should arrive")
        .unwrap();
}

#[tokio::test]
async fn group_op_falls_back_to_chip_tool_when_unavailable() {
    let (_dir, store_path) = make_store();
    // group ctx なしの native → Unavailable → chip-tool 経路へ。dead backend が
    // エラーを返すこと自体が「フォールバックが試みられた」証拠。
    let native = NativeBackend::with_parts(Box::new(FakeEstablisher::default()), None);
    let backend = dead_backend().await;
    let err = run_op(&group_on_op(), &backend, Some(&native), &store_path)
        .await
        .unwrap_err();
    assert_ne!(err.kind, ErrorKind::Unreachable, "native 送出エラーではなく chip-tool 接続系のエラーになる");
}
```

（`FakeEstablisher::default()` が無ければ `test_support` 移設時に `Default` を
derive する。`assert_ne!` の期待 kind は dead backend の実エラー分類に合わせて
確定してよい — 主張の本体は「native 送出には到達していない」こと。）

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p matd server`
Expected: FAIL（`native_group_params` 未定義）

- [ ] **Step 3: 実装**

```rust
/// group 送信 op の native 適用判定。native で送れるなら
/// (group_id, cluster_id, command_id, fields) を返す。`GroupInvoke` は
/// onoff の引数なし on/off/toggle のみ（汎用の cluster/command 名→ID
/// テーブルは未実装 — M4 の Read 制限と同型）。None は chip-tool へ。
fn native_group_params(op: &Op) -> Option<(u16, u32, u32, Option<Vec<u8>>)> {
    match op {
        Op::GroupInvoke {
            group_id,
            cluster,
            command,
            args,
            ..
        } if cluster == "onoff" && args.is_empty() => {
            let cmd = match command.as_str() {
                "on" => im::CMD_ON_OFF_ON,
                "off" => im::CMD_ON_OFF_OFF,
                "toggle" => im::CMD_ON_OFF_TOGGLE,
                _ => return None,
            };
            Some((*group_id, im::CLUSTER_ON_OFF, cmd, None))
        }
        Op::GroupColorTemp {
            group_id,
            mireds,
            transition,
            ..
        } => Some((
            *group_id,
            im::CLUSTER_COLOR_CONTROL,
            im::CMD_MOVE_TO_COLOR_TEMPERATURE,
            Some(im::encode_move_to_color_temperature_fields(*mireds, *transition)),
        )),
        Op::GroupColor {
            group_id,
            hue_raw,
            saturation_raw,
            transition,
            ..
        } => Some((
            *group_id,
            im::CLUSTER_COLOR_CONTROL,
            im::CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(im::encode_move_to_hue_and_saturation_fields(
                *hue_raw,
                *saturation_raw,
                *transition,
            )),
        )),
        _ => None,
    }
}
```

`run_op` の native 分岐（`is_native_hotpath` チェックの直後）に追加:

```rust
if let Some(native) = native {
    if is_native_hotpath(op) {
        return native_op(op, native, store_path).await;
    }
    if let Some((group_id, cluster, command, fields)) = native_group_params(op) {
        // chip-tool 経路と同じ前提チェック（store が開けること）。
        let _store = Store::open(store_path)?;
        match native.group_invoke(group_id, cluster, command, fields).await? {
            crate::native::GroupOutcome::Sent => return Ok(group_sent_body(op)),
            crate::native::GroupOutcome::Unavailable(reason) => {
                tracing::warn!(%reason, "native group send unavailable; falling back to chip-tool");
            }
        }
    }
}
```

`group_sent_body(op)` は既存 `group_invoke` / `group_color_op` の成功 `json!`
ボディ（`status: "sent"`、`note: "unacknowledged groupcast; ..."` を含む）を
そのまま関数に抽出し、chip-tool 経路の両関数からも呼ぶ（応答スキーマは経路に
よらず同一 — DRY）。`GroupInvoke`/`GroupColorTemp`/`GroupColor` の 3 腕を持ち、
それ以外は `unreachable!`。

`im` の use を server.rs に追加: `use mat_controller::im;`（matd は既に
mat_controller に依存）。

- [ ] **Step 4: テスト全通過を確認**

Run: `cargo test -p matd`
Expected: PASS（integration.rs の既存 group テスト = native 無効時の chip-tool 経路、も無傷）

- [ ] **Step 5: `task check` → コミット**

Run: `task check`
Expected: fmt / clippy / 全テスト PASS

```bash
git add crates/matd/src/server.rs
git commit -m "feat(matd): route group send ops to native groupcast with chip-tool fallback (M5)"
```

---

### Task 7: 実機 E2E ハーネス（live test + e2e-m5.sh + Taskfile）

**Files:**
- Create: `crates/mat-controller/tests/live_matd_group.rs`
- Create: `scripts/e2e-m5.sh`（`e2e-m4.sh` を雛形に）
- Modify: `Taskfile.yml`（`e2e:m5` タスク追加、`e2e:m4` の直後）

**Interfaces:**
- Consumes: matd unix socket プロトコル（`live_matd_native.rs` の `request` ヘルパと同形）
- Produces: `task e2e:m5`（env: `MAT_E2E_HOST` / `MAT_E2E_IFACE` / `MAT_E2E_GROUP_NODES`（カンマ区切り node id、必須）/ 任意 `MAT_E2E_GROUP_ID`（既定 10）/ `MAT_E2E_ENDPOINT`（既定 1）ほか m4 と同じ）

- [ ] **Step 1: live テストを書く**（`live_matd_group.rs`。`live_matd_native.rs` の `env_u64` / `request` / `assert_ok` をコピーして流用）

2 本の `#[ignore]` テスト:

```rust
//! Live E2E (M5): native groupcast against the real living_lights group.
//! matd_group_roundtrip: group off→on→color-temp、各ノードを unicast read で
//! 検証（= N/N 配達判定）。matd_group_after_restart: スクリプトが matd を
//! 再起動した後に呼び、jump-ahead 後も配達されることを検証（消灯で終わる）。
//! Run via `task e2e:m5`. Not in CI.

fn group_nodes() -> Vec<u64> {
    std::env::var("MAT_E2E_GROUP_NODES")
        .expect("MAT_E2E_GROUP_NODES (csv node ids) required")
        .split(',')
        .map(|s| {
            let s = s.trim();
            match s.strip_prefix("0x") {
                Some(h) => u64::from_str_radix(h, 16).expect("hex id"),
                None => s.parse().expect("decimal id"),
            }
        })
        .collect()
}

async fn assert_all_onoff(socket: &str, nodes: &[u64], ep: u16, want: bool, ctx: &str) {
    for node in nodes {
        let read = format!(
            r#"{{"op":"read","node_id":{node},"endpoint":{ep},"cluster":"onoff","attribute":"on-off"}}"#
        );
        let r = request(socket, &read).await;
        assert_ok(&r, &format!("{ctx}: read node {node}"));
        assert_eq!(r["value"], serde_json::json!(want), "{ctx}: node {node}");
    }
}

#[tokio::test]
#[ignore = "requires a running native-enabled matd + a provisioned group (task e2e:m5)"]
async fn matd_group_roundtrip() {
    let socket = std::env::var("MAT_E2E_SOCKET").expect("MAT_E2E_SOCKET required");
    let gid: u16 = std::env::var("MAT_E2E_GROUP_ID").ok().and_then(|s| s.parse().ok()).unwrap_or(10);
    let ep: u16 = std::env::var("MAT_E2E_ENDPOINT").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let nodes = group_nodes();

    let ginv = |cmd: &str| {
        format!(
            r#"{{"op":"group_invoke","group_id":{gid},"cluster":"onoff","command":"{cmd}","endpoint":{ep}}}"#
        )
    };
    // off → 全ノード消灯（groupcast の伝播を 2s 待つ）。
    let r = request(&socket, &ginv("off")).await;
    assert_ok(&r, "group off");
    assert_eq!(r["status"], "sent");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_all_onoff(&socket, &nodes, ep, false, "after group off").await;

    // on → 全ノード点灯。
    assert_ok(&request(&socket, &ginv("on")).await, "group on");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_all_onoff(&socket, &nodes, ep, true, "after group on").await;

    // color-temp 370 mireds → 各ノードの color-temperature が目標±8。
    let ct = format!(
        r#"{{"op":"group_color_temp","group_id":{gid},"mireds":370,"kelvin":2702,"transition":0,"endpoint":{ep}}}"#
    );
    assert_ok(&request(&socket, &ct).await, "group color-temp");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    for node in &nodes {
        let read = format!(
            r#"{{"op":"read","node_id":{node},"endpoint":{ep},"cluster":"colorcontrol","attribute":"color-temperature"}}"#
        );
        let r = request(&socket, &read).await;
        assert_ok(&r, &format!("read color-temperature node {node}"));
        let v = r["value"].as_i64().expect("numeric mireds");
        assert!((v - 370).abs() <= 8, "node {node}: mireds {v} not near 370");
    }
}

#[tokio::test]
#[ignore = "second phase of task e2e:m5 (after the script restarts matd)"]
async fn matd_group_after_restart() {
    let socket = std::env::var("MAT_E2E_SOCKET").expect("MAT_E2E_SOCKET required");
    let gid: u16 = std::env::var("MAT_E2E_GROUP_ID").ok().and_then(|s| s.parse().ok()).unwrap_or(10);
    let ep: u16 = std::env::var("MAT_E2E_ENDPOINT").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let nodes = group_nodes();
    // 再起動後の fresh counter（jump-ahead）でも配達される = M5 受け入れ 8。
    let off = format!(
        r#"{{"op":"group_invoke","group_id":{gid},"cluster":"onoff","command":"off","endpoint":{ep}}}"#
    );
    assert_ok(&request(&socket, &off).await, "group off after restart");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_all_onoff(&socket, &nodes, ep, false, "after restart group off").await;
}
```

- [ ] **Step 2: e2e-m5.sh を書く**（`e2e-m4.sh` をコピーして差分編集）

差分（それ以外は m4 と同一の流儀 — クロスビルド検証・ssh cat 転送・socket 待ち・trap cleanup）:

- 冒頭コメント: M5 受け入れ（groupcast N/N 配達 + matd 再起動後の配達）。
  **警告**: 実行中に本番 matd / 直 chip-tool から group 送信をしないこと
  （counter 混在の実機知見）。unicast は併用可。
- 必須 env に `MAT_E2E_GROUP_NODES` 追加（`MAT_E2E_NODE_ID` は不要に）。
  任意 env `MAT_E2E_GROUP_ID`（既定 10）。
- バイナリ/ソケット名: `/tmp/matd-m5` / `/tmp/matd-m5.sock` / `/tmp/live_matd_group` /
  ws port 9111 / log `/tmp/matd-m5.log`。
- テストバイナリ: `cargo test -p mat-controller --test live_matd_group --release --target "$TARGET" --no-run`
- 実行部:

```bash
echo "== 4/5 ライブテスト (roundtrip)"
ssh "$MAT_E2E_HOST" \
  MAT_E2E_SOCKET="$SOCKET" \
  MAT_E2E_GROUP_ID="$GROUP_ID" \
  MAT_E2E_GROUP_NODES="$MAT_E2E_GROUP_NODES" \
  MAT_E2E_ENDPOINT="$ENDPOINT" \
  'exec /tmp/live_matd_group --ignored --nocapture matd_group_roundtrip'

echo "== 5/5 matd 再起動 → jump-ahead 配達検証"
ssh "$MAT_E2E_HOST" 'kill "$(cat /tmp/matd-m5.pid)" && sleep 1'
# （m4 と同じ起動ブロックを再実行して socket を待つ）
ssh "$MAT_E2E_HOST" \
  MAT_E2E_SOCKET="$SOCKET" \
  MAT_E2E_GROUP_ID="$GROUP_ID" \
  MAT_E2E_GROUP_NODES="$MAT_E2E_GROUP_NODES" \
  MAT_E2E_ENDPOINT="$ENDPOINT" \
  'exec /tmp/live_matd_group --ignored --nocapture matd_group_after_restart'

echo "== counter 履歴（jump-ahead の目視確認用）"
ssh "$MAT_E2E_HOST" 'grep "groupcast sent" /tmp/matd-m5.log || true'
echo "== e2e:m5 PASS"
```

- [ ] **Step 3: Taskfile に追加**

```yaml
  e2e:m5:
    desc: "M5 実機 E2E（native groupcast。要 MAT_E2E_HOST/IFACE/GROUP_NODES）"
    cmds:
      - bash scripts/e2e-m5.sh
```

- [ ] **Step 4: コンパイル確認**

Run: `cargo test -p mat-controller --test live_matd_group --no-run && bash -n scripts/e2e-m5.sh && task check`
Expected: 全部成功（live テストは `--no-run` でビルドのみ）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/tests/live_matd_group.rs scripts/e2e-m5.sh Taskfile.yml
git commit -m "test(matd): M5 live E2E harness — native groupcast over unix socket"
```

---

### Task 8: ドキュメント反映

**Files:**
- Modify: `ARCHITECTURE.md`（Phase 5 節の M4 記述の直後に M5 を追記）
- Modify: `README.md`（group 節に native 経路の説明を追記）

**Interfaces:** なし（文書のみ）

- [ ] **Step 1: ARCHITECTURE.md 更新**

Phase 5 節の M4 実機 E2E 記述（`grep -n "M5" ARCHITECTURE.md` で場所確認、424 行付近「group は M5 で native 化予定」）を実装済みの記述に更新:

- M5 実装内容の要約: matd の group 送信 3 op（GroupInvoke onoff / GroupColor /
  GroupColorTemp）を native groupcast 化（KVS の導出済み group 鍵 + GKH、
  ff35:: site-local multicast、自前 counter ファイル + `max(自前, g/gdc)+4096`
  jump-ahead）。`GroupProvision` と汎用 group invoke は chip-tool フォールバック。
- 実機 E2E の結果欄は「実機 E2E 未実施」とし、合格後に別コミットで更新する
  （M4 と同じ運用）。
- spec への参照: `docs/superpowers/specs/2026-07-12-phase5-m5-group-native-design.md`

- [ ] **Step 2: README.md 更新**

group 関連節（`grep -n "group" README.md` で該当節を確認）に追記:

- matd が native 有効（`MAT_MATD_IFACE`）のとき group 送信は matd 内蔵の
  groupcast 経路で送られる（chip-tool 不要）。counter は `<store>/native_group_counter`。
- 送信者一本化の note 更新: group 送信は matd（native）一本を推奨。mat 直経路
  （chip-tool）の group 送信と混在させると counter 衝突で不達になる既知の罠は
  従来どおり（native は起動時に chip-tool の g/gdc を跨ぐが、逆方向 — native の
  後に chip-tool で group 送信 — は落ちる）。

- [ ] **Step 3: `task check` → コミット**

```bash
task check
git add ARCHITECTURE.md README.md
git commit -m "docs: reflect M5 (native groupcast) in ARCHITECTURE/README"
```

---

## 実機 E2E（計画外・メインセッションで実施）

Task 1〜8 完了後、メインセッションが実施（サブエージェント任せにしない）:

1. `MAT_E2E_HOST=jarvis MAT_E2E_IFACE=eth0 MAT_E2E_FABRIC_INDEX=2 MAT_E2E_GROUP_NODES=<実 node id 群> task e2e:m5`
2. 不達なら tcpdump（eth0 で ff35:: 観測）→ spec リスク節の切り分けへ。
3. spec 受け入れ 9（`GroupProvision` の chip-tool 非回帰）: コード無変更の経路
   だが、`mat group provision --rebind` を実グループへ 1 回流して従来どおり
   通ることを確認する（v0.15.0 実証済みの手順の再走。native 側は rebind 後の
   鍵を KVS 再読込で拾う — 直後の groupcast 配達がその確認を兼ねる）。
4. 合格したら ARCHITECTURE.md の M5 欄に実機結果を反映して docs コミット
   （M4 の `0020654` と同じ形式）。
5. **注意**: E2E 中は本番 matd からの group 送信を止める（unicast は可）。
