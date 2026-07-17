# Phase 5 M8c-2: KVS group 書込所有 + diag node native 化 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `MAT_IFACE` 設定時、`mat group provision` のコントローラ側 group state（chip-tool `groupsettings` 相当）を mat が chip-tool INI KVS へ直接書き（直経路 + matd の M8a ハイブリッド解消）、`mat diag node` の IM 部分も native で走る（0.21.0）。

**Architecture:** ①`mat-controller::kvs` に flock + tmp+rename の INI 書込トランザクション（`KvsTxn`）②`mat-controller::group_settings` に chip-tool GroupDataProvider 互換の 5 レコード書込（`write_group_provision`）③`mat-native::group_settings` の薄いラッパー（`GroupSettingsCtx`、Engine が保持）④mat 直経路 / matd の配線 + `diag node` の native IM プローブ、の4層。プロトコル・永続形式の知識は backend crate に閉じる（設計ルール1）。spec: `docs/superpowers/specs/2026-07-17-phase5-m8c2-groupsettings-native-design.md`（**必読** — 特に「事前調査の確定事項」と「番兵値・リンク終端」）。

**Tech Stack:** Rust (workspace)。新規依存なし（rustix / base64ct / hkdf は mat-controller に既存）。bash（実機 E2E）。

## Global Constraints

- **作業ブランチ**: `m8c2-groupsettings-native`（Task 1 で main から作成、worktree `.claude/worktrees/m8c2-groupsettings-native`）。**全タスクの冒頭で `pwd` と `git branch --show-current` を確認**（サブエージェントの shell はメイン repo (main) で始まる罠が既知）。
- **バージョン**: workspace `Cargo.toml` の `version = "0.21.0"`（Task 1）。
- **`MAT_IFACE` 未設定は完全無変更**。既存統合テスト（fake-chip-tool）は無改変で全通過（唯一の例外: `emit_provision_success` の引数追加に伴う機械的な呼び出し側修正は可、出力は不変）。
- **chip-tool 互換が至上命題**: mat が書いた KVS を実 chip-tool が読めること。TLV タグ・番兵値・リンク規律は spec の確定事項どおり（group=**末尾挿入**・終端 0 / keyset=**head 挿入**・終端 **0xFFFF**（id 0 = IPK は有効値）/ keymap=末尾連結・id は max+1 で sparse / KeySet 配列は**常に 3 スロット**ゼロ埋め / 走査は count 正）。
- **flock 規律**: KVS 書込は sidecar `<ini>.lock` への advisory flock（NonBlocking）下で read-modify-write し tmp+fsync+rename で置換。**WouldBlock は hard error**（chip-tool へフォールバックしない — flock 非参加の chip-tool がその書込と競合するため）。書込 I/O エラーも hard error。フォールバックしてよいのは KVS 資材の解決失敗（= Engine 構築失敗、既存分岐）のみ。
- **1 回の provision の 5 レコード（FabricList / FabricData / GroupData / KeyMapData / KeySetData）は 1 つの flock 区間 + 1 commit で書く**。
- **リンク切れ・解釈不能な既存レコードを見つけたら書かずに hard error**（上流は黙って進むが、mat は不整合ストアを悪化させない）。
- **マーカーログ**（E2E が verbatim で grep）: `group provision controller state written (native kvs)`（info、mat-native ラッパー）/ `diag node executed (native)`（info）/ フォールバックは既存 warn 形式（`falling back to chip-tool` を含む）。
- リポジトリは公開: 実ノード ID・実鍵・実証明書をコミットしない（テストはダミー値、RFC 5737 等）。
- コミットは各タスク末尾、メッセージ末尾に `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。コミット前 `cargo fmt`、最終タスクで `task check`。

---

### Task 1: 前提確認 + ブランチ + バージョン 0.21.0

**Files:**
- Modify: `Cargo.toml`（workspace version のみ）

**Interfaces:**
- Produces: main から切った `m8c2-groupsettings-native` の worktree。以後の全タスクはここで作業。

- [ ] **Step 1: main の状態確認（着手ゲート）**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat
git log --oneline main | head -5
```

M8c-2 spec コミット（`docs: M8c-2`）と M8c-1 マージ（ca93946）が見えること。見えなければ中止してユーザーへ。

- [ ] **Step 2: worktree + ブランチ作成**

```bash
git worktree add .claude/worktrees/m8c2-groupsettings-native -b m8c2-groupsettings-native main
cd .claude/worktrees/m8c2-groupsettings-native && pwd && git branch --show-current
```

- [ ] **Step 3: バージョン 0.21.0**

workspace `Cargo.toml` の `[workspace.package]` の `version = "0.20.0"` を `"0.21.0"` に変更。

- [ ] **Step 4: ビルド確認 + Commit**

```bash
cargo build -p mat 2>&1 | tail -3   # Cargo.lock の version 反映込みで通ること
git add Cargo.toml Cargo.lock
git commit -m "chore: version 0.21.0 (M8c-2 開始)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: mat-controller — GKH 導出 + `KvsTxn`（INI 書込トランザクション）

**Files:**
- Modify: `crates/mat-controller/src/fabric.rs`（`derive_group_session_id` 追加）
- Modify: `crates/mat-controller/src/kvs.rs`（`KvsTxn` + `KvsError::Locked` 追加）

**Interfaces:**
- Consumes: 既存 `KvsError`、`default_section`/`lookup`/`decode_b64`（kvs.rs 内 private、流儀の参照元）、`rustix::fs::flock`（`mat-controller/src/group.rs` の `PersistedGroupCounter::load` と同流儀）。
- Produces:
  - `pub fn fabric::derive_group_session_id(operational_key: &[u8; 16]) -> u16`
  - `pub struct kvs::KvsTxn` — `pub fn open(path: &Path) -> Result<KvsTxn, KvsError>` / `pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvsError>` / `pub fn set(&mut self, key: &str, value: &[u8])` / `pub fn remove(&mut self, key: &str)` / `pub fn commit(self) -> Result<(), KvsError>`
  - `KvsError::Locked` variant（Display: `"kvs: locked by another process"`）

- [ ] **Step 1: 失敗テストを書く（kvs.rs の `#[cfg(test)] mod tests` に追加）**

```rust
    // ---- M8c-2: KvsTxn ----

    fn tmp_ini(lines: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chip_tool_config.ini");
        std::fs::write(&p, lines).unwrap();
        (dir, p)
    }

    #[test]
    fn kvs_txn_set_get_roundtrip_and_preserves_unrelated_lines() {
        let (_d, p) = tmp_ini("[Default]\ng/gdc=AQAAAA==\n");
        let mut txn = KvsTxn::open(&p).unwrap();
        assert_eq!(txn.get("nope").unwrap(), None);
        txn.set("f/2/g", &[0x15, 0x18]);
        assert_eq!(txn.get("f/2/g").unwrap().unwrap(), vec![0x15, 0x18]);
        txn.commit().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        // 無関係キーは保全、新キーは [Default] 内に chip-tool inipp 形式（key=value）で追記。
        assert!(text.contains("g/gdc=AQAAAA=="), "{text}");
        assert!(text.contains("f/2/g=FRg="), "{text}");
        // 再読込でも読める（自作 reader との整合）。
        let txn2 = KvsTxn::open(&p).unwrap();
        assert_eq!(txn2.get("f/2/g").unwrap().unwrap(), vec![0x15, 0x18]);
    }

    #[test]
    fn kvs_txn_set_replaces_existing_line_in_place() {
        let (_d, p) = tmp_ini("[Default]\nf/2/g=AAAA\nother=x\n");
        let mut txn = KvsTxn::open(&p).unwrap();
        txn.set("f/2/g", &[1]);
        txn.commit().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches("f/2/g=").count(), 1, "{text}");
        assert!(text.contains("other=x"));
    }

    #[test]
    fn kvs_txn_remove_deletes_line() {
        let (_d, p) = tmp_ini("[Default]\nf/2/gk/1=AAAA\nkeep=y\n");
        let mut txn = KvsTxn::open(&p).unwrap();
        txn.remove("f/2/gk/1");
        txn.commit().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("f/2/gk/1"), "{text}");
        assert!(text.contains("keep=y"));
    }

    #[test]
    fn kvs_txn_open_fails_without_default_section() {
        let (_d, p) = tmp_ini("[Other]\nk=v\n");
        assert!(matches!(KvsTxn::open(&p), Err(KvsError::SectionMissing)));
    }

    #[test]
    fn kvs_txn_second_open_would_block() {
        let (_d, p) = tmp_ini("[Default]\n");
        let _held = KvsTxn::open(&p).unwrap();
        assert!(matches!(KvsTxn::open(&p), Err(KvsError::Locked)));
    }
```

fabric.rs の tests に追加:

```rust
    #[test]
    fn group_session_id_is_deterministic_and_key_dependent() {
        let a = derive_group_session_id(&[1u8; 16]);
        let b = derive_group_session_id(&[1u8; 16]);
        let c = derive_group_session_id(&[2u8; 16]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-controller kvs_txn 2>&1 | tail -5` → コンパイルエラー（`KvsTxn` 未定義）。

- [ ] **Step 3: 実装**

fabric.rs（`derive_ipk_operational` の直後に）:

```rust
/// operational group key から GKH（Group Key Hash = group session id、spec
/// §4.15.2: Crypto_KDF(operational, salt=なし, info="GroupKeyHash", 16bit)
/// → big-endian u16）を導出する。ワイヤの group session id と、chip-tool KVS
/// KeySetData（`f/<idx>/k/<ksid>` ctx5 hash）に永続される値がこれ。
/// 上流 v1.4.2.0 `CHIPCryptoPAL.cpp` `DeriveGroupSessionId` と同一。
pub fn derive_group_session_id(operational_key: &[u8; 16]) -> u16 {
    let hk = Hkdf::<Sha256>::new(None, operational_key);
    let mut out = [0u8; 2];
    hk.expand(b"GroupKeyHash", &mut out)
        .expect("2 bytes is a valid hkdf-sha256 output length");
    u16::from_be_bytes(out)
}
```

（注: 上流の salt は空 span。RFC 5869 で「salt 省略 = HashLen のゼロ」、HMAC は鍵をゼロ埋めするため空 salt と等価 — `None` でよい。`derive_ipk_operational` の doc comment に「IPK は keyset 0 の特殊例で、この KDF は任意 group keyset の epoch→operational に共通（M8c-2 の group_settings も使用）」の一文を追記。）

kvs.rs — `KvsError` に `Locked` を追加（Display: `"kvs: locked by another process"`）し、`KvsTxn` を実装:

```rust
/// chip-tool INI KVS への書込トランザクション（M8c-2）。
///
/// open で sidecar `<ini>.lock` に advisory flock（NonBlocking exclusive、
/// `group.rs` の counter と同流儀 — 本体は tmp+rename で置換されるので本体
/// fd への flock は無効化される）を取り、ファイル全行をメモリへ読む。
/// set/remove は [Default] セクション内の行だけを操作し（既存行は in-place
/// 置換、新規は末尾追記、書式は chip-tool inipp と同じ `key=value`）、他の
/// 行は byte 単位で保全する。commit が tmp+fsync+rename の原子置換。
/// ロックは Drop まで保持（commit を呼ばなければ何も書かれない）。
pub struct KvsTxn {
    path: std::path::PathBuf,
    lines: Vec<String>,
    /// [Default] セクション内の行範囲（`lines` の添字、[start, end)）。
    default_start: usize,
    default_end: usize,
    _lock: std::fs::File,
}

impl KvsTxn {
    pub fn open(path: &Path) -> Result<Self, KvsError> {
        use rustix::fs::{flock, FlockOperation};
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(std::path::PathBuf::from(lock_path))
            .map_err(KvsError::Io)?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                KvsError::Locked
            } else {
                KvsError::Io(std::io::Error::other(e))
            }
        })?;
        let text = std::fs::read_to_string(path).map_err(KvsError::Io)?;
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        // [Default] セクション境界を行単位で確定。
        let mut default_start = None;
        let mut default_end = lines.len();
        for (i, line) in lines.iter().enumerate() {
            match default_start {
                None => {
                    if line.trim() == "[Default]" {
                        default_start = Some(i + 1);
                    }
                }
                Some(_) => {
                    if line.trim_start().starts_with('[') {
                        default_end = i;
                        break;
                    }
                }
            }
        }
        let default_start = default_start.ok_or(KvsError::SectionMissing)?;
        Ok(Self {
            path: path.to_path_buf(),
            lines,
            default_start,
            default_end,
            _lock: lock,
        })
    }

    /// [Default] 内で key の行を探す（先頭 `=` で分割し両側 trim — 読み側
    /// `lookup` と同じ寛容さ）。
    fn find(&self, key: &str) -> Option<usize> {
        (self.default_start..self.default_end).find(|&i| {
            self.lines[i]
                .split_once('=')
                .is_some_and(|(k, _)| k.trim() == key)
        })
    }

    /// key の値を base64 デコードして返す。無い・空は None（読み側
    /// `decode_b64` と同じ扱い）。
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, KvsError> {
        use base64ct::{Base64, Encoding};
        match self.find(key) {
            None => Ok(None),
            Some(i) => {
                let v = self.lines[i].split_once('=').expect("find matched").1.trim();
                if v.is_empty() {
                    return Ok(None);
                }
                Base64::decode_vec(v)
                    .map(Some)
                    .map_err(|_| KvsError::BadBase64(key.to_string()))
            }
        }
    }

    pub fn set(&mut self, key: &str, value: &[u8]) {
        use base64ct::{Base64, Encoding};
        let line = format!("{key}={}", Base64::encode_string(value));
        match self.find(key) {
            Some(i) => self.lines[i] = line,
            None => {
                self.lines.insert(self.default_end, line);
                self.default_end += 1;
            }
        }
    }

    pub fn remove(&mut self, key: &str) {
        if let Some(i) = self.find(key) {
            self.lines.remove(i);
            self.default_end -= 1;
        }
    }

    /// tmp + fsync + rename の原子置換（`group.rs` counter の persist と同流儀）。
    pub fn commit(self) -> Result<(), KvsError> {
        use std::io::Write;
        let tmp = self.path.with_extension("ini.tmp");
        let mut f = std::fs::File::create(&tmp).map_err(KvsError::Io)?;
        let mut body = self.lines.join("\n");
        body.push('\n');
        f.write_all(body.as_bytes()).map_err(KvsError::Io)?;
        f.sync_all().map_err(KvsError::Io)?;
        std::fs::rename(&tmp, &self.path).map_err(KvsError::Io)?;
        Ok(())
    }
}
```

（実装時の注意: `use base64ct` は関数内 use でなくファイル先頭の既存 import を使ってよい。`with_extension` はファイル名の最後の `.ini` を置換するので `chip_tool_config.ini.tmp` にしたければ `path.with_extension("ini.tmp")` — 実挙動を単体テストの一時ファイル名で確認して調整。）

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller kvs_txn group_session_id 2>&1 | tail -5`
Expected: PASS（6本）。

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add crates/mat-controller/src/kvs.rs crates/mat-controller/src/fabric.rs
git commit -m "feat(controller): KvsTxn（flock+原子置換のINI書込）+ GKH導出 (M8c-2 Task2)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: mat-controller — `group_settings`（chip-tool 互換 5 レコード書込）

**Files:**
- Create: `crates/mat-controller/src/group_settings.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod group_settings;` 追加）

**Interfaces:**
- Consumes: Task 2 の `KvsTxn` / `KvsError::Locked`、`fabric::derive_ipk_operational` / `fabric::derive_group_session_id`、`tlv::{Reader, Tag, Value, Writer}`。
- Produces:

```rust
pub enum GroupSettingsError {
    DuplicateBind { group_id: u16, keyset_id: u16 },
    Corrupt { key: String, reason: &'static str },
    Kvs(kvs::KvsError),
}
pub struct GroupProvisionWrite<'a> {
    pub group_id: u16,
    pub keyset_id: u16,
    pub name: &'a str,
    pub epoch_key: [u8; 16],
    pub rebind: bool,
}
pub fn write_group_provision(
    main_ini: &Path,
    fabric_index: u8,
    compressed_fabric_id: &[u8; 8],
    w: &GroupProvisionWrite<'_>,
) -> Result<(), GroupSettingsError>
```

- [ ] **Step 1: 失敗テストを書く（`group_settings.rs` 内 `#[cfg(test)]`、ファイルごと新規作成してよい — 型・関数はテストが要求する形で先に書く）**

テストは「書いた結果を**既存の読み側**（`kvs::read_group_credentials`）と**自前の走査**で検証する」構成。fixture は Task 2 の `tmp_ini` と同型のヘルパで作る。

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fabric::{derive_group_session_id, derive_ipk_operational};

    const CFID: [u8; 8] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11];

    fn tmp_ini(lines: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chip_tool_config.ini");
        std::fs::write(&p, lines).unwrap();
        (dir, p)
    }

    fn provision(p: &std::path::Path, group: u16, keyset: u16, rebind: bool) {
        write_group_provision(
            p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: group,
                keyset_id: keyset,
                name: "e2e",
                epoch_key: [0x42; 16],
                rebind,
            },
        )
        .unwrap();
    }

    #[test]
    fn fresh_fabric_full_provision_readable_by_existing_parser() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 99, 99, false);
        // 読み側パーサ（実機検証済み）で読み戻せる = タグ互換の証明。
        let creds = crate::kvs::read_group_credentials(&p, 2, 99).unwrap();
        let op = derive_ipk_operational(&[0x42; 16], &CFID);
        assert_eq!(creds.encryption_key, op);
        assert_eq!(creds.session_id, derive_group_session_id(&op));
        // 5 レコードが揃っている。
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        for key in ["g/gfl", "f/2/g", "f/2/g/63", "f/2/gk/1", "f/2/k/63"] {
            assert!(txn.get(key).unwrap().is_some(), "missing {key}");
        }
    }

    #[test]
    fn keyset_blob_has_three_slots_and_terminator_ffff() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 99, 99, false);
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        let blob = txn.get("f/2/k/63").unwrap().unwrap();
        // struct{ctx1 policy=0, ctx2 keys_count=1, ctx3 array[3 structs], ctx7 next=0xFFFF}
        // を Reader で走査して検証（スロット2,3 は start_time=0/hash=0/key=16B ゼロ）。
        let mut r = crate::tlv::Reader::new(&blob);
        assert!(matches!(r.next().unwrap().unwrap().value, crate::tlv::Value::StructStart));
        let mut slots = 0;
        let mut next: Option<u64> = None;
        loop {
            let el = r.next().unwrap().unwrap();
            match (el.tag, el.value) {
                (_, crate::tlv::Value::ContainerEnd) => break,
                (crate::tlv::Tag::Context(3), crate::tlv::Value::ArrayStart) => loop {
                    let e = r.next().unwrap().unwrap();
                    match e.value {
                        crate::tlv::Value::StructStart => {
                            slots += 1;
                            // struct を読み飛ばす。
                            loop {
                                if matches!(r.next().unwrap().unwrap().value, crate::tlv::Value::ContainerEnd) {
                                    break;
                                }
                            }
                        }
                        crate::tlv::Value::ContainerEnd => break,
                        _ => {}
                    }
                },
                (crate::tlv::Tag::Context(7), crate::tlv::Value::Uint(v)) => next = Some(v),
                _ => {}
            }
        }
        assert_eq!(slots, 3);
        assert_eq!(next, Some(0xFFFF));
    }

    #[test]
    fn existing_chiptool_like_store_gets_tail_group_and_head_keyset() {
        // chip-tool が作った既存状態を再現: IPK keyset 0 + group 10/keyset 60
        //（keyset チェーン 60→0→end、group チェーン 10→end、map gk/1）。
        // fixture は自前の write_group_provision で group10/keyset60 を書き、
        // 手で first_keyset チェーンに keyset 0 を差し込む …のは複雑なので、
        // 「まず 10/60 を provision → 次に 99/99 を provision」して
        // 2 group 目のリンク規律を検証する（keyset 0 の混在は
        // ipk_keyset_zero_in_chain_is_preserved で別途）。
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 10, 60, false);
        provision(&p, 99, 99, false);
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        // FabricData: group 末尾挿入 → first_group=10 のまま、group 10 の next=99。
        // keyset head 挿入 → first_keyset=99、99 の next=60。map は gk/1→gk/2。
        let fabric = parse_fabric_data(&txn.get("f/2/g").unwrap().unwrap()).unwrap();
        assert_eq!(fabric.first_group, 10);
        assert_eq!(fabric.group_count, 2);
        assert_eq!(fabric.first_keyset, 99);
        assert_eq!(fabric.keyset_count, 2);
        assert_eq!(fabric.first_map, 1);
        assert_eq!(fabric.map_count, 2);
        let g10 = parse_group_data(&txn.get("f/2/g/a").unwrap().unwrap()).unwrap();
        assert_eq!(g10.next, 99);
        let g99 = parse_group_data(&txn.get("f/2/g/63").unwrap().unwrap()).unwrap();
        assert_eq!(g99.next, 0);
        assert_eq!(g99.first_endpoint, 0xFFFF);
        let m1 = parse_keymap(&txn.get("f/2/gk/1").unwrap().unwrap()).unwrap();
        assert_eq!((m1.group_id, m1.keyset_id, m1.next), (10, 60, 2));
        let m2 = parse_keymap(&txn.get("f/2/gk/2").unwrap().unwrap()).unwrap();
        assert_eq!((m2.group_id, m2.keyset_id, m2.next), (99, 99, 0));
        // 両 group とも読み側で解決できる。
        assert!(crate::kvs::read_group_credentials(&p, 2, 10).is_ok());
        assert!(crate::kvs::read_group_credentials(&p, 2, 99).is_ok());
    }

    #[test]
    fn duplicate_bind_without_rebind_is_error() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 99, 99, false);
        let err = write_group_provision(
            &p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: 99,
                keyset_id: 99,
                name: "e2e",
                epoch_key: [0x42; 16],
                rebind: false,
            },
        )
        .expect_err("duplicate bind must fail");
        assert!(matches!(err, GroupSettingsError::DuplicateBind { group_id: 99, keyset_id: 99 }));
    }

    #[test]
    fn rebind_unbinds_then_binds_and_map_ids_stay_sparse() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 10, 60, false); // gk/1
        provision(&p, 99, 99, false); // gk/2
        provision(&p, 99, 99, true); // unbind gk/2 → 新 id は max+1=3（詰め直さない）
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        assert!(txn.get("f/2/gk/2").unwrap().is_none(), "unbound entry must be deleted");
        let m3 = parse_keymap(&txn.get("f/2/gk/3").unwrap().unwrap()).unwrap();
        assert_eq!((m3.group_id, m3.keyset_id, m3.next), (99, 99, 0));
        let m1 = parse_keymap(&txn.get("f/2/gk/1").unwrap().unwrap()).unwrap();
        assert_eq!(m1.next, 3, "prev link must be re-pointed");
        let fabric = parse_fabric_data(&txn.get("f/2/g").unwrap().unwrap()).unwrap();
        assert_eq!(fabric.map_count, 2);
        // group / keyset は更新のみ（重複レコードにならない）。
        assert_eq!(fabric.group_count, 2);
        assert_eq!(fabric.keyset_count, 2);
    }

    #[test]
    fn ipk_keyset_zero_in_chain_is_preserved() {
        // keyset id 0（IPK）が既にチェーンにいる状態（jarvis 実機と同型）を
        // 手組みで再現: まず 99/99 を書き、first_keyset チェーンを
        // 99 → 0 に差し替え + k/0 を置く（0 は有効 id、終端は 0xFFFF）。
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 99, 99, false);
        {
            let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
            // k/0 = keyset 99 の blob を流用し next を 0xFFFF のままにする
            //（中身は問わない — 走査対象になることだけが重要）。
            let blob = txn.get("f/2/k/63").unwrap().unwrap();
            txn.set("f/2/k/0", &blob);
            // f/2/k/63 の next を 0 に書き換え（serialize_keyset で再生成）。
            let op = derive_ipk_operational(&[0x42; 16], &CFID);
            let hash = derive_group_session_id(&op);
            txn.set("f/2/k/63", &serialize_keyset(0, 1, hash, &op, 0));
            let mut fabric = parse_fabric_data(&txn.get("f/2/g").unwrap().unwrap()).unwrap();
            fabric.keyset_count = 2;
            txn.set("f/2/g", &fabric.serialize());
            txn.commit().unwrap();
        }
        // ここへ新 keyset 100 を head 挿入しても、0 を終端と誤認せず
        // チェーンが 100 → 99 → 0 になる。
        write_group_provision(
            &p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: 100,
                keyset_id: 100,
                name: "x",
                epoch_key: [0x43; 16],
                rebind: false,
            },
        )
        .unwrap();
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        let fabric = parse_fabric_data(&txn.get("f/2/g").unwrap().unwrap()).unwrap();
        assert_eq!(fabric.first_keyset, 100);
        assert_eq!(fabric.keyset_count, 3);
        assert_eq!(keyset_next(&txn.get("f/2/k/64").unwrap().unwrap()).unwrap(), 99);
    }

    #[test]
    fn corrupt_chain_is_hard_error_and_writes_nothing() {
        // first_group が指す group レコードが無い → Corrupt、ファイル無変更。
        let (_d, p) = tmp_ini("[Default]\n");
        {
            let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
            let fabric = FabricData { first_group: 7, group_count: 1, ..FabricData::empty() };
            txn.set("f/2/g", &fabric.serialize());
            txn.commit().unwrap();
        }
        let before = std::fs::read_to_string(&p).unwrap();
        let err = write_group_provision(
            &p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: 99,
                keyset_id: 99,
                name: "x",
                epoch_key: [0x42; 16],
                rebind: false,
            },
        )
        .expect_err("corrupt chain");
        assert!(matches!(err, GroupSettingsError::Corrupt { .. }), "{err:?}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before, "must not write");
    }

    #[test]
    fn group_name_is_truncated_to_16_bytes_on_char_boundary() {
        let (_d, p) = tmp_ini("[Default]\n");
        write_group_provision(
            &p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: 5,
                keyset_id: 5,
                name: "0123456789abcdefOVERFLOW",
                epoch_key: [0x42; 16],
                rebind: false,
            },
        )
        .unwrap();
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        let g = parse_group_data(&txn.get("f/2/g/5").unwrap().unwrap()).unwrap();
        assert_eq!(g.name, "0123456789abcdef");
    }
}
```

（テストが参照する private ヘルパ `parse_fabric_data` / `parse_group_data` / `parse_keymap` / `keyset_next` / `serialize_keyset` / `FabricData::{empty, serialize}` は Step 3 の実装の一部。`f/2/g/a` は group 10 の hex、`f/2/k/63`・`f/2/g/63` は 99 の hex、`f/2/k/64` は 100 の hex。）

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-controller group_settings 2>&1 | tail -5` → コンパイルエラー。

- [ ] **Step 3: 実装（`group_settings.rs` 本体）**

構成（全て同ファイル内、レコード型は private、上流 v1.4.2.0 `GroupDataProviderImpl.cpp` のセマンティクスを spec の確定事項どおり写す）:

```rust
//! chip-tool 互換 KVS への controller 側 group state 書込（M8c-2）。
//!
//! chip-tool `groupsettings add-group / add-keysets / (unbind-keyset) /
//! bind-keyset` が KVS に残す 5 レコード（g/gfl, f/<i>/g, f/<i>/g/<gid>,
//! f/<i>/gk/<id>, f/<i>/k/<ksid>）を、上流 v1.4.2.0 GroupDataProviderImpl
//! と同じリンク規律（group=末尾挿入・終端0 / keyset=head 挿入・終端0xFFFF、
//! id 0 = IPK は有効値 / keymap=末尾連結・id は max+1 で sparse / 走査は
//! count 正）で書く。1 回の provision は 1 つの KvsTxn（flock 区間）で
//! 読み・変更・commit まで完結する。
//!
//! 上流との意図的な差分は 2 つ: ①リンク切れ・解釈不能レコードは黙って
//! 進まず [`GroupSettingsError::Corrupt`]（不整合ストアを悪化させない）。
//! ②新規 GroupData の first_endpoint は常に kInvalidEndpointId（0xFFFF）
//! —— 上流は直前に走査した他レコードの値が漏れ込むが、endpoint_count=0 の
//! とき読者はこの欄を見ないため互換に影響しない。

use std::path::Path;

use crate::fabric::{derive_group_session_id, derive_ipk_operational};
use crate::kvs::{KvsError, KvsTxn};
use crate::tlv::{Reader, Tag, Value, Writer};

/// keyset リンクの終端（上流 kInvalidKeysetId — id 0 は IPK で有効値）。
const INVALID_KEYSET_ID: u16 = 0xFFFF;
/// endpoint 無しの GroupData first_endpoint（上流 kInvalidEndpointId）。
const INVALID_ENDPOINT_ID: u16 = 0xFFFF;
/// KeySetData の operational key 配列は常に 3 スロット（KeySet::kEpochKeysMax）。
const KEYSET_SLOTS: usize = 3;
/// デバイス側 epochStartTime0 と一致させる（mat-core::group::EPOCH_START_TIME = "1"）。
const EPOCH_START_TIME: u64 = 1;
/// GroupName の最大バイト数（上流 CHIP_CONFIG_MAX_GROUP_NAME_LENGTH）。
const GROUP_NAME_MAX: usize = 16;
```

エラー型（Display 付き、`detail` に鍵名を残す）:

```rust
#[derive(Debug)]
pub enum GroupSettingsError {
    /// 既に同じ (group, keyset) の bind がある（chip-tool の
    /// CHIP_ERROR_DUPLICATE_KEY_ID 相当 — `--rebind` で解消する）。
    DuplicateBind { group_id: u16, keyset_id: u16 },
    /// 既存レコードのリンク切れ・解釈不能（書かずに中断）。
    Corrupt { key: String, reason: &'static str },
    Kvs(KvsError),
}
```

（`impl Display` / `impl std::error::Error` / `impl From<KvsError>` を通例どおり書く。）

レコード型 + parse/serialize（タグは spec 確定値。parse は `Reader` で `kvs.rs` の `parse_keymap_entry` と同じ寛容さ — 未知タグ skip、必須欠落は None）:

```rust
struct FabricData { first_group: u16, group_count: u16, first_map: u16, map_count: u16, first_keyset: u16, keyset_count: u16, next: u16 }
impl FabricData {
    fn empty() -> Self { Self { first_group: 0, group_count: 0, first_map: 0, map_count: 0, first_keyset: INVALID_KEYSET_ID, keyset_count: 0, next: 0 } }
    fn parse(blob: &[u8]) -> Option<Self>   // struct{ctx1..ctx7 全て Uint}
    fn serialize(&self) -> Vec<u8>          // Writer: start_struct(Anonymous) → put_uint(Context(1..=7)) → end
}
struct GroupData { name: String, first_endpoint: u16, endpoint_count: u16, next: u16 }
//   parse: ctx1 Utf8 / ctx2..4 Uint。serialize: put_str(ctx1) + put_uint(ctx2..4)。
struct KeyMap { group_id: u16, keyset_id: u16, next: u16 }
//   parse/serialize: ctx1..3 Uint。
fn serialize_keyset(policy: u16, start_time: u64, hash: u16, key: &[u8; 16], next: u16) -> Vec<u8>
//   struct{ ctx1 policy, ctx2 keys_count=1, ctx3 array[KEYSET_SLOTS 個の
//   struct{ctx4 start_time, ctx5 hash, ctx6 bytes16}]（スロット1のみ実値、
//   残りは 0/0/[0u8;16]）, ctx7 next }
fn keyset_next(blob: &[u8]) -> Option<u16>  // ctx7 だけ読む（既存 keyset の next 維持用）
struct FabricList { first_entry: u16, entry_count: u16 }  // g/gfl、ctx1/ctx2
```

本体:

```rust
pub struct GroupProvisionWrite<'a> {
    pub group_id: u16,
    pub keyset_id: u16,
    pub name: &'a str,
    pub epoch_key: [u8; 16],
    pub rebind: bool,
}

pub fn write_group_provision(
    main_ini: &Path,
    fabric_index: u8,
    compressed_fabric_id: &[u8; 8],
    w: &GroupProvisionWrite<'_>,
) -> Result<(), GroupSettingsError> {
    let mut txn = KvsTxn::open(main_ini)?;
    let fkey = format!("f/{fabric_index}/g");
    let mut fabric = match txn.get(&fkey)? {
        None => FabricData::empty(),
        Some(b) => FabricData::parse(&b).ok_or_else(|| corrupt(&fkey, "unparseable FabricData"))?,
    };

    // 1) add-group（SetGroupInfo: 既存なら名前更新のみ、新規は末尾挿入）
    // 2) add-keysets（SetKeySet: 既存なら next 維持で上書き、新規は head 挿入）
    // 3) rebind なら unbind-keyset（best-effort: 見つからなくても続行）
    // 4) bind-keyset（SetGroupKeyAt: 重複は DuplicateBind、id は max+1）
    // 5) FabricList 登録 + FabricData 保存 → commit
    ...
    txn.commit()?;
    Ok(())
}
```

各ステップの走査は count 回のループで、レコード読出し失敗（キー欠落 / parse 不能）は `Corrupt`。実装の要点（上流の対応関数を doc comment で参照すること）:

- **group 走査**（`f/{fi}/g/{gid:x}` を `first_group` から `group_count` 回）: 対象 group_id が見つかれば name を差し替えて該当キーへ再 serialize（`first_endpoint`/`endpoint_count`/`next` は既存値維持）。無ければ `GroupData { name: truncated, first_endpoint: INVALID_ENDPOINT_ID, endpoint_count: 0, next: 0 }` を `f/{fi}/g/{group_id:x}` へ書き、`group_count == 0` なら `fabric.first_group = group_id`、そうでなければ走査で覚えた**末尾** group のレコードの `next = group_id` に書き換えて再保存。`fabric.group_count += 1`。
- **name 切詰め**: `GROUP_NAME_MAX` バイト以内へ **char 境界で**切る（`name.char_indices().take_while(|(i, c)| i + c.len_utf8() <= GROUP_NAME_MAX)` 方式。上流はバイト切りだが UTF-8 を割らない方へ倒す — doc comment に明記）。
- **keyset 走査**（`f/{fi}/k/{ksid:x}` を `first_keyset` から `keyset_count` 回、`INVALID_KEYSET_ID` は保険の終端）: 対象 keyset_id が既存なら `keyset_next` で既存 next を読み、同じ next で上書き。新規なら `next = fabric.first_keyset` で書いて `fabric.first_keyset = keyset_id; fabric.keyset_count += 1`。operational = `derive_ipk_operational(&w.epoch_key, compressed_fabric_id)`、hash = `derive_group_session_id(&operational)`、policy = 0、start_time = `EPOCH_START_TIME`。
- **map 走査**（`f/{fi}/gk/{id:x}` を `first_map` から `map_count` 回。走査中に `max_id` と各エントリの (id, KeyMap) を集める）: rebind の unbind は (group, keyset) 一致エントリを削除し、前エントリの next を繋ぎ替え（先頭なら `fabric.first_map = removed.next`）、`map_count -= 1`。見つからなければ何もしない（best-effort、chip-tool 経路と同じ）。bind は一致エントリが**残っていれば** `DuplicateBind`。新 id = `max_id + 1`（unbind で消した id も max に数えたままなので再利用しない — 上流と同じ）、`KeyMap { group_id, keyset_id, next: 0 }` を書き、`map_count == 0` なら `fabric.first_map = id`、そうでなければ末尾エントリの next = id に再保存。`fabric.map_count += 1`。
- **FabricList**（`g/gfl`）: 無ければ `{ first_entry: fabric_index, entry_count: 1 }` を書く。有れば `first_entry` から `FabricData.next` チェーンを `entry_count` 回辿って fabric_index の有無を確認（各 `f/{n}/g` の parse 失敗は Corrupt）、無ければ `fabric.next = list.first_entry; list.first_entry = fabric_index; list.entry_count += 1` で head 挿入して `g/gfl` を再保存。
- 最後に `txn.set(&fkey, &fabric.serialize())` → `txn.commit()`。

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller group_settings 2>&1 | tail -8`
Expected: PASS（8本）。

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add crates/mat-controller/src/group_settings.rs crates/mat-controller/src/lib.rs
git commit -m "feat(controller): group_settings — chip-tool互換のKVS group書込（5レコード/リンク規律再現） (M8c-2 Task3)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: mat-native — `GroupSettingsCtx` + ラッパー + matd アクセサ

**Files:**
- Create: `crates/mat-native/src/group_settings.rs`
- Modify: `crates/mat-native/src/lib.rs`（`pub mod group_settings;`、`Engine` にフィールド追加、`build`/`with_parts` 更新）
- Modify: `crates/matd/src/native.rs`（`group_settings_ctx()` アクセサ + テスト用ビルダ）

**Interfaces:**
- Consumes: Task 3 の `mat_controller::group_settings::{write_group_provision, GroupProvisionWrite, GroupSettingsError}`、`mat_controller::fabric::compressed_fabric_id`。
- Produces:

```rust
// mat-native
pub struct group_settings::GroupSettingsCtx { pub main_ini: PathBuf, pub fabric_index: u8, pub cfid: [u8; 8] }
pub fn group_settings::write_group_provision(
    ctx: &GroupSettingsCtx, group_id: u16, keyset_id: u16, name: &str,
    epoch_key: &[u8; 16], rebind: bool,
) -> Result<(), MatError>
pub struct Engine { ..., pub group_settings: Option<group_settings::GroupSettingsCtx> }  // build で Some、with_parts で None（pub フィールドなのでテストは直接代入）
// matd
impl NativeBackend { pub fn group_settings_ctx(&self) -> Option<&mat_native::group_settings::GroupSettingsCtx> }
```

- [ ] **Step 1: 失敗テストを書く（`group_settings.rs` 内）**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &tempfile::TempDir) -> GroupSettingsCtx {
        let p = dir.path().join("chip_tool_config.ini");
        std::fs::write(&p, "[Default]\n").unwrap();
        GroupSettingsCtx { main_ini: p, fabric_index: 2, cfid: [7u8; 8] }
    }

    #[test]
    fn write_group_provision_writes_kvs_and_maps_duplicate_to_other() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        write_group_provision(&c, 99, 99, "e2e", &[0x42; 16], false).unwrap();
        // 読み側で解決できる（controller 層の round-trip は Task 3 で証明済み、
        // ここは配線の確認だけ）。
        assert!(mat_controller::kvs::read_group_credentials(&c.main_ini, 2, 99).is_ok());
        // 二重 bind は Other + "--rebind" 誘導の detail。
        let err = write_group_provision(&c, 99, 99, "e2e", &[0x42; 16], false).unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::Other);
        assert!(err.detail.contains("--rebind"), "{}", err.detail);
        // rebind なら通る。
        write_group_provision(&c, 99, 99, "e2e", &[0x42; 16], true).unwrap();
    }

    #[test]
    fn locked_kvs_is_hard_error_not_fallback_shaped() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(&dir);
        let _held = mat_controller::kvs::KvsTxn::open(&c.main_ini).unwrap();
        let err = write_group_provision(&c, 99, 99, "e2e", &[0x42; 16], false).unwrap_err();
        assert_eq!(err.kind, mat_core::error::ErrorKind::Other);
        assert!(err.detail.contains("locked"), "{}", err.detail);
    }
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-native group_settings 2>&1 | tail -5` → コンパイルエラー。

- [ ] **Step 3: 実装**

`crates/mat-native/src/group_settings.rs`:

```rust
//! controller 側 group state の native KVS 書込（M8c-2）。
//!
//! chip-tool `groupsettings` 4 コマンド相当を `mat-controller::group_settings`
//! に委譲する薄いラッパー。mat 直経路（native_direct）と matd（server::
//! group_provision）の両方が使う。エラーは mat の ErrorKind へ写像する
//! （フォールバック判断は呼び出し側 — ctx 未構成のみフォールバック対象。
//! ここから返るエラーは全て hard error）。

use std::path::PathBuf;

use mat_controller::group_settings::{GroupProvisionWrite, GroupSettingsError};
use mat_core::error::{ErrorKind, MatError};

/// KVS 書込に必要な資材。`Engine::build` が KVS 読出し時に組み立てる。
pub struct GroupSettingsCtx {
    pub main_ini: PathBuf,
    pub fabric_index: u8,
    pub cfid: [u8; 8],
}

pub fn write_group_provision(
    ctx: &GroupSettingsCtx,
    group_id: u16,
    keyset_id: u16,
    name: &str,
    epoch_key: &[u8; 16],
    rebind: bool,
) -> Result<(), MatError> {
    mat_controller::group_settings::write_group_provision(
        &ctx.main_ini,
        ctx.fabric_index,
        &ctx.cfid,
        &GroupProvisionWrite {
            group_id,
            keyset_id,
            name,
            epoch_key: *epoch_key,
            rebind,
        },
    )
    .map_err(map_gs_err)?;
    tracing::info!(
        group_id,
        keyset_id,
        "group provision controller state written (native kvs)"
    );
    Ok(())
}

/// GroupSettingsError → ErrorKind。全て hard error（ワイヤ未接触だが KVS は
/// 触った可能性があるため chip-tool を重ねない）。kind は Other に寄せ、
/// detail で復旧手段を示す（chip-tool 経路の分類とは厳密一致しない —
/// native op の写像表と同じ扱い）。
fn map_gs_err(e: GroupSettingsError) -> MatError {
    let detail = match &e {
        GroupSettingsError::DuplicateBind { group_id, keyset_id } => format!(
            "keyset {keyset_id} is already bound to group {group_id} in the controller kvs; use --rebind"
        ),
        GroupSettingsError::Kvs(mat_controller::kvs::KvsError::Locked) => {
            "controller kvs is locked by another process (concurrent provision?)".to_string()
        }
        other => format!("controller kvs group write failed: {other}"),
    };
    MatError::new(ErrorKind::Other, detail)
}
```

`crates/mat-native/src/lib.rs`:
- `pub mod group_settings;` 追加。
- `Engine` に `pub group_settings: Option<group_settings::GroupSettingsCtx>` を追加。
- `Engine::build`: `let fabric_id = creds.fabric_id;` の近く（creds を establisher へ move する**前**）で

```rust
        let cfid = compressed_fabric_id(&creds.root_public_key, creds.fabric_id);
        let group_settings = group_settings::GroupSettingsCtx {
            main_ini: main_ini.clone(),
            fabric_index: cfg.fabric_index,
            cfid,
        };
```

を組み、`Ok(Self::with_parts(...))` を `Ok(Self { establisher: Box::new(establisher), group: Some(group), group_settings: Some(group_settings) })` 形に変更（`main_ini` は `GroupCtx` にも move されるので clone 順に注意）。
- `with_parts` は `group_settings: None` で従来シグネチャ維持（テストは pub フィールドへ直接代入）。

`crates/matd/src/native.rs`:

```rust
    /// controller 側 group state の KVS 書込資材（M8c-2）。None = native 構築が
    /// 未完（テスト注入等）— 呼び出し側は chip-tool ws へフォールバックする。
    pub fn group_settings_ctx(&self) -> Option<&mat_native::group_settings::GroupSettingsCtx> {
        self.engine.group_settings.as_ref()
    }

    /// テスト用: with_parts + group_settings 注入。
    #[cfg(test)]
    pub(crate) fn with_parts_gs(
        establisher: Box<dyn Establisher>,
        group: Option<GroupCtx>,
        gs: Option<mat_native::group_settings::GroupSettingsCtx>,
    ) -> Self {
        let mut engine = mat_native::Engine::with_parts(establisher, group);
        engine.group_settings = gs;
        Self::from_engine(engine)
    }
```

- [ ] **Step 4: テスト通過 + 全体回帰**

Run: `cargo test -p mat-native 2>&1 | tail -3` / `cargo test -p matd 2>&1 | tail -3`
Expected: PASS（既存含む）。

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add crates/mat-native crates/matd/src/native.rs
git commit -m "feat(native): GroupSettingsCtx+write_group_provisionラッパー（EngineがKVS書込資材を保持） (M8c-2 Task4)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: mat 直経路配線（native_direct の GroupProvision + note）

**Files:**
- Modify: `crates/mat/src/native_direct.rs`（`run_op` の `GroupProvision` 腕、`store_root` 引数の要否整理）
- Modify: `crates/mat/src/commands/group.rs`（`emit_provision_success` に `native_kvs: bool` 追加）
- Modify: `crates/mat/tests/integration.rs`（回帰確認のみ、原則無改変）

**Interfaces:**
- Consumes: Task 4 の `engine.group_settings` / `mat_native::group_settings::write_group_provision`、既存 `mat_core::group::resolve_epoch_key` / `mat_native::ops::epoch_key_from_hex`。
- Produces: `emit_provision_success(group_id, keyset_id, name, endpoint, node_ids, rebind, native_kvs)`（chip-tool 経路の呼び出しは `native_kvs=false`）。

- [ ] **Step 1: 失敗テストを書く（native_direct.rs の `#[cfg(test)]`）**

既存テスト群（`group_onoff_generic_provision_and_grant_are_all_native` 付近）の流儀で、`run_op` を直接叩く tokio テストを追加。`FakeEstablisher`（`mat_native::test_support`、dev-deps 設定済み）+ 一時 KVS ini を使う。fake の scripted 読み（group-key-map / acl）は matd の `group_provision_routes_device_side_steps_through_native`（`crates/matd/src/server.rs` 1639 行付近の ScriptedEstablisher）を参考に、mat 側 `test_support` の同等機能を使う（無ければ同型の Establisher をテスト内に定義）。

```rust
    #[tokio::test]
    async fn run_op_group_provision_writes_controller_state_to_kvs_natively() {
        // engine: fake establisher + group_settings ctx（一時 ini）。
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let mut engine = Engine::with_parts(Box::new(scripted_establisher()), None);
        engine.group_settings = Some(mat_native::group_settings::GroupSettingsCtx {
            main_ini: ini.clone(),
            fabric_index: 2,
            cfid: [7u8; 8],
        });
        let op = NativeOp::GroupProvision {
            group_id: 99,
            node_ids: vec![5],
            keyset_id: 99,
            name: "e2e".into(),
            endpoint: 1,
            epoch_key: Some("42".repeat(16)),
            rebind: false,
        };
        let out = run_op(&engine, &op).await.unwrap();
        assert!(matches!(out, RunOutcome::Done));
        // コントローラ側 state が chip-tool spawn なしで KVS に入っている。
        assert!(mat_controller::kvs::read_group_credentials(&ini, 2, 99).is_ok());
    }

    #[tokio::test]
    async fn run_op_group_provision_falls_back_when_ctx_missing() {
        let engine = Engine::with_parts(Box::new(scripted_establisher()), None); // ctx なし
        let op = NativeOp::GroupProvision {
            group_id: 99,
            node_ids: vec![5],
            keyset_id: 99,
            name: "e2e".into(),
            endpoint: 1,
            epoch_key: None,
            rebind: false,
        };
        assert!(matches!(run_op(&engine, &op).await.unwrap(), RunOutcome::Fallback));
    }
```

（`scripted_establisher()` は group-key-map read に `[]`、acl read に管理者 1 エントリを返す fake — matd の同名テストと同じ形。`run_op` の `store_root` 引数が不要になる（下記 Step 3）ためテストは 2 引数で呼ぶ。stdout への emit はテストでは検証しない — 出力形は既存 fake-chip-tool 統合テストの守備範囲。）

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat --lib native_direct 2>&1 | tail -5` → コンパイルエラー。

- [ ] **Step 3: 実装**

`commands/group.rs` — `emit_provision_success` に `native_kvs: bool` を追加:

```rust
pub(crate) fn emit_provision_success(
    group_id: u16,
    keyset_id: u16,
    name: &str,
    endpoint: u16,
    node_ids: &[u64],
    rebind: bool,
    native_kvs: bool,
) {
    let mut body = json!({ ...現行どおり... });
    if native_kvs {
        // native は rebind の有無によらず KVS を直接書くので常にこの note。
        body["note"] = json!(
            "controller group state written natively to kvs; if matd is running, restart it to reload group state"
        );
    } else if rebind {
        body["note"] = json!(
            "rebound keyset binding; if matd is running, restart it to reload group state"
        );
    }
    output::emit(body);
}
```

chip-tool 経路 `provision()` の呼び出しは `..., rebind, false)` に更新（出力不変）。

`native_direct.rs` の `GroupProvision` 腕を差し替え:

```rust
        NativeOp::GroupProvision {
            group_id,
            node_ids,
            keyset_id,
            name,
            endpoint,
            epoch_key,
            rebind,
        } => {
            // 1) コントローラ側 group state（M8c-2: native KVS 書込）。ctx 未構成
            //    はワイヤ・KVS とも未接触なので chip-tool へフォールバック
            //    （group 送信の ctx 判定と対）。書込エラーは hard error
            //    （ラッパー側 doc 参照 — flock WouldBlock 含む）。
            let Some(gs) = &engine.group_settings else {
                tracing::warn!(
                    "native group settings context not configured; falling back to chip-tool"
                );
                return Ok(RunOutcome::Fallback);
            };
            let epoch_key_hex = mat_core::group::resolve_epoch_key(epoch_key.as_deref())?;
            let epoch_key_bytes = mat_native::ops::epoch_key_from_hex(&epoch_key_hex)?;
            mat_native::group_settings::write_group_provision(
                gs, *group_id, *keyset_id, name, &epoch_key_bytes, *rebind,
            )?;

            // 2) 各デバイスへ provision（native, unicast）— M8a のまま。
            for &node_id in node_ids {
                let mut conn = engine.establisher.establish(node_id).await?;
                let p = mat_native::ops::ProvisionNodeParams {
                    group_id: *group_id,
                    keyset_id: *keyset_id,
                    name: name.clone(),
                    endpoint: *endpoint,
                    epoch_key: epoch_key_bytes,
                };
                mat_native::ops::provision_node(&mut *conn, &p)
                    .await
                    .map_err(|e| MatError::new(e.kind, format!("node {node_id}: {}", e.detail)))?;
            }

            tracing::info!(group_id, keyset_id, "group provision executed (native direct)");
            crate::commands::group::emit_provision_success(
                *group_id, *keyset_id, name, *endpoint, node_ids, *rebind, true,
            );
        }
```

`run_op` の `store_root: &Path` 引数は GroupProvision 専用だった（doc comment 参照）ので**削除**し、呼び出し元（`try_run` 内 `run_op(&engine, op, store.root())`）と doc comment を更新。コンパイラ警告・エラーで漏れを検出。

- [ ] **Step 4: テスト通過 + 統合回帰**

Run: `cargo test -p mat 2>&1 | tail -5`
Expected: 全 PASS（fake-chip-tool 統合テスト含む。`MAT_IFACE` 未設定経路の出力不変を既存テストが保証）。

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add crates/mat/src/native_direct.rs crates/mat/src/commands/group.rs
git commit -m "feat(mat): 直経路provisionのcontroller側をnative KVS書込へ（chip-tool groupsettings spawn廃止+note付与） (M8c-2 Task5)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: matd 配線（group_provision のハイブリッド解消）

**Files:**
- Modify: `crates/matd/src/server.rs`（`group_provision` のコントローラ側分岐 + note）
- Test: `crates/matd/src/server.rs` の `#[cfg(test)]`（または `crates/matd/tests/integration.rs` — 既存 `group_provision_routes_device_side_steps_through_native` の隣）

**Interfaces:**
- Consumes: Task 4 の `NativeBackend::group_settings_ctx()` / `with_parts_gs`、`mat_native::group_settings::write_group_provision`。
- Produces: matd の provision 出力 JSON — native KVS 書込時のみ `note` 追加（他は不変）。

- [ ] **Step 1: 失敗テストを書く**

既存 `group_provision_routes_device_side_steps_through_native`（server.rs 1664 行付近）を**無改変で残し**（ctx None → 従来どおり ws groupsettings に流れることの回帰になる）、隣に新テスト:

```rust
    #[tokio::test]
    async fn group_provision_writes_controller_state_natively_when_ctx_present() {
        // ctx あり → groupsettings の ws コマンドが 1 本も流れず、KVS に書かれ、
        // note が付く。デバイス側は従来どおり native（ScriptedEstablisher）。
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("chip_tool_config.ini");
        std::fs::write(&ini, "[Default]\n").unwrap();
        let gs = mat_native::group_settings::GroupSettingsCtx {
            main_ini: ini.clone(),
            fabric_index: 2,
            cfid: [7u8; 8],
        };
        let native = NativeBackend::with_parts_gs(Box::new(ScriptedEstablisher), None, Some(gs));
        // backend は「groupsettings が来たら即 panic する fake ws」を使う
        //（既存テストの fake ws ヘルパを流用し、is_controller_step 分岐を
        //  panic に差し替えた版をテスト内に定義）。
        let (backend, _guard) = spawn_fake_ws_failing_on_groupsettings().await;
        let op = Op::GroupProvision { group_id: 99, node_ids: vec![5], keyset_id: 99,
            name: "e2e".into(), endpoint: 1, epoch_key: None, rebind: false };
        let (store_path, _store_guard) = store_with_nodes(&[5]); // 既存ヘルパ流儀
        let body = group_provision(&op, &backend, Some(&native), &store_path).await.unwrap();
        assert_eq!(body["status"], "provisioned");
        assert!(body["note"].as_str().unwrap().contains("restart"), "{body}");
        assert!(mat_controller::kvs::read_group_credentials(&ini, 2, 99).is_ok());
    }
```

（既存テストのヘルパ名は server.rs 内を読んで正確に合わせること — `spawn_fake_ws_*` / store 準備の流儀は 1599 行付近の既存 fake を参照。「groupsettings で panic」は fake ws の分岐に `assert!(!line.starts_with("groupsettings "))` を入れる形でよい。）

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p matd group_provision_writes_controller_state 2>&1 | tail -5` → コンパイルエラー（`with_parts_gs` 呼び出しはあるが server 側が ws へ流すため fake が panic、等）。

- [ ] **Step 3: 実装（server.rs `group_provision`）**

コントローラ側ステップ（`group_step` 4 連発 + rebind 分岐）を丸ごと以下に置換:

```rust
    // 1) コントローラ側 group state。native の KVS 書込資材（M8c-2）があれば
    //    chip-tool を介さず直接書く。無ければ従来どおり chip-tool ws（M8a まで
    //    のハイブリッド形 — native 無効時・テスト注入時）。
    let native_gs = native.and_then(|n| n.group_settings_ctx());
    if let Some(gs) = native_gs {
        let epoch_key_bytes = mat_native::ops::epoch_key_from_hex(&epoch_key)?;
        mat_native::group_settings::write_group_provision(
            gs, *group_id, *keyset_id, name, &epoch_key_bytes, *rebind,
        )?;
    } else {
        ...（現行の add-group / add-keysets / (unbind) / bind-keyset の group_step 4 連発をそのまま移動）...
    }
```

出力 body（関数末尾）に note を追加:

```rust
    let mut body = json!({ ...現行どおり... });
    if native_gs.is_some() {
        // matd 自身の warm chip-tool は古い group 状態をメモリに持ったまま —
        // fallback op が chip-tool に流れる構成なら再起動で再読込させる。
        body["note"] = json!(
            "controller group state written natively to kvs; if matd is running, restart it to reload group state"
        );
    }
    Ok(body)
```

- [ ] **Step 4: テスト通過 + matd 全体回帰**

Run: `cargo test -p matd 2>&1 | tail -3`
Expected: 全 PASS（既存 `group_provision_routes_device_side_steps_through_native` は ctx None のまま従来経路で通る）。

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add crates/matd/src/server.rs crates/matd/tests 2>/dev/null; git add -u
git commit -m "feat(matd): provisionのcontroller側group stateをnative KVS書込へ（M8aハイブリッド解消） (M8c-2 Task6)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: diag node の IM 部分 native 化

**Files:**
- Modify: `crates/mat/src/native_direct.rs`（`diag_im_probe` 追加）
- Modify: `crates/mat/src/commands/diag.rs`（`node()` の native 分岐）
- Modify: `crates/mat/tests/integration.rs`（bogus iface フォールバック回帰 1 本）

**Interfaces:**
- Consumes: `Engine::build` / `engine.establisher.establish` / `NodeConn::read_json`、`engine.group_settings`（cfid 取得）、`mat_core::ids::resolve_attribute`、`mat_core::diag::ThreadCheck`。
- Produces:

```rust
// native_direct.rs
pub(crate) struct DiagImProbe {
    pub resolved: bool,
    pub op_kind: Option<mat_core::error::ErrorKind>,
    pub self_cfid: String,                    // 16桁大文字hex（probe.rs と同形式）
    pub thread: Result<mat_core::diag::ThreadCheck, mat_core::error::ErrorKind>,
}
pub(crate) fn diag_im_probe(cfg: &Config<'_>, store_root: &Path, node_id: u64, endpoint: u16) -> Option<DiagImProbe>
// None = エンジン構築失敗（呼び出し側が chip-tool 経路へフォールバック）
```

- [ ] **Step 1: 失敗テストを書く（native_direct.rs の `#[cfg(test)]`）**

テスト容易性のため実体は `async fn diag_im_with_engine(engine: &Engine, node_id: u64, endpoint: u16) -> DiagImProbe` に置き、`diag_im_probe` は「runtime 構築 + `Engine::build`（失敗で warn + None）+ 呼出し」だけの薄い皮にする。

```rust
    #[tokio::test]
    async fn diag_im_with_engine_reads_operational_and_thread_natively() {
        use mat_native::test_support::FakeEstablisher;
        // parts-list(ep0/0x001D/0x0003) と neighbor-table(0x0035/0x0007)、
        // routing-role(0x0035/0x0001) を scripted fake で返す。
        let establisher = FakeEstablisher::default(); // read_json は数値/配列を返す既定 fake
        let mut engine = Engine::with_parts(Box::new(establisher), None);
        engine.group_settings = Some(mat_native::group_settings::GroupSettingsCtx {
            main_ini: std::path::PathBuf::from("/nonexistent"),
            fabric_index: 2,
            cfid: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
        });
        let p = diag_im_with_engine(&engine, 5, 1).await;
        assert!(p.resolved);
        assert_eq!(p.op_kind, None);
        assert_eq!(p.self_cfid, "1122334455667788");
        let t = p.thread.expect("thread check");
        // FakeEstablisher の既定応答形に合わせて neighbor_count / best_lqi を検証
        //（fake が構造体配列を返さない場合は scripted fake with_read で
        //  [{"5": 200}, {"5": 100}] を返す形にし、best_lqi=200 を確認）。
        assert!(t.neighbor_count >= 1);
    }

    #[tokio::test]
    async fn diag_im_with_engine_reports_establish_failure_as_unresolved() {
        // establish が Unreachable を返す fake（既存 FailingConn/Establisher 流儀で
        // テスト内に定義）→ resolved=false / op_kind=Some(Unreachable) /
        // thread=Err(Unreachable)。cfid は資材から出るので establish 失敗でも返る。
        let mut engine = Engine::with_parts(Box::new(failing_establisher()), None);
        engine.group_settings = Some(mat_native::group_settings::GroupSettingsCtx {
            main_ini: std::path::PathBuf::from("/nonexistent"),
            fabric_index: 2,
            cfid: [1u8; 8],
        });
        let p = diag_im_with_engine(&engine, 5, 1).await;
        assert!(!p.resolved);
        assert_eq!(p.op_kind, Some(mat_core::error::ErrorKind::Unreachable));
        assert!(p.thread.is_err());
    }
```

（`FakeEstablisher` の scripted 応答 API は `mat-native/src/test_support.rs` を読んで正確に合わせること — ops.rs のテスト `FakeConn::scripted().with_read(...)` の形が使えるなら `FakeConn` を返す Establisher をテスト内に定義してよい。）

integration.rs に bogus iface 回帰（既存の bogus iface テストの隣、同流儀で）:

```rust
// MAT_IFACE=bogus で diag node → warn + chip-tool フォールバックで従来出力。
// 既存 discover/commission の bogus iface テストを grep してコピーし、
// コマンドだけ `diag node -n <id>` に替える。アサートは exit 成功 +
// stdout の verdict キー存在（fake-chip-tool の既存フィクスチャで通る形）。
```

（fake-chip-tool が diag node の各 read に応答するフィクスチャは既存 — `mat diag node` の既存統合テストを探して同じ node/fixture を使う。）

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat --lib diag_im 2>&1 | tail -5` → コンパイルエラー。

- [ ] **Step 3: 実装**

native_direct.rs:

```rust
/// `mat diag node` の IM 部分（operational チェック + thread シグナル）を
/// native で実行した結果（M8c-2）。CFID はログパースではなく fabric 資材
/// から直接計算するため、native 経路では cfid_unavailable の系が消える。
pub(crate) struct DiagImProbe { ...上記 Interfaces のとおり... }

pub(crate) fn diag_im_probe(
    cfg: &Config<'_>,
    store_root: &Path,
    node_id: u64,
    endpoint: u16,
) -> Option<DiagImProbe> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let native_cfg = NativeConfig {
            store: store_root.to_path_buf(),
            iface: cfg.iface.to_string(),
            fabric_index: cfg.fabric_index,
            issuer_index: cfg.issuer_index,
        };
        let engine = match Engine::build(&native_cfg).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e.detail, "native diag build failed; falling back to chip-tool");
                return None;
            }
        };
        Some(diag_im_with_engine(&engine, node_id, endpoint).await)
    })
}

async fn diag_im_with_engine(engine: &Engine, node_id: u64, endpoint: u16) -> DiagImProbe {
    use mat_core::ids::resolve_attribute;
    // cfid は build 済みエンジンでは常に Some（with_parts 注入時のみ呼び出し側が保証）。
    let cfid = engine
        .group_settings
        .as_ref()
        .map(|g| g.cfid)
        .expect("built engine always carries group_settings");
    let self_cfid = format!("{:016X}", u64::from_be_bytes(cfid));

    const CLUSTER_DESCRIPTOR: u32 = 0x001D;
    const ATTR_PARTS_LIST: u32 = 0x0003;
    const CLUSTER_THREAD_DIAG: u32 = 0x0035;

    let (resolved, op_kind, thread) = match engine.establisher.establish(node_id).await {
        Err(e) => (false, Some(e.kind), Err(e.kind)),
        Ok(mut conn) => {
            let op = conn.read_json(0, CLUSTER_DESCRIPTOR, ATTR_PARTS_LIST).await;
            let (resolved, op_kind) = match op {
                Ok(_) => (true, None),
                Err(e) => (false, Some(e.kind)),
            };
            let nt_attr = resolve_attribute(CLUSTER_THREAD_DIAG, "neighbor-table").map(|a| a.id);
            let rr_attr = resolve_attribute(CLUSTER_THREAD_DIAG, "routing-role").map(|a| a.id);
            let thread = match nt_attr {
                None => Err(mat_core::error::ErrorKind::Other),
                Some(id) => match conn.read_json(endpoint, CLUSTER_THREAD_DIAG, id).await {
                    Err(e) => Err(e.kind),
                    Ok(v) => {
                        let rows = v.as_array().cloned().unwrap_or_default();
                        // struct のキーは field id の10進文字列（"5" = Lqi）。
                        let best_lqi = rows
                            .iter()
                            .filter_map(|r| r.get("5").and_then(serde_json::Value::as_u64))
                            .map(|v| v as u8)
                            .max();
                        let routing_role = match rr_attr {
                            None => None,
                            Some(id) => conn
                                .read_json(endpoint, CLUSTER_THREAD_DIAG, id)
                                .await
                                .ok()
                                .and_then(|v| v.as_i64()),
                        };
                        Ok(mat_core::diag::ThreadCheck {
                            neighbor_count: rows.len(),
                            best_lqi,
                            routing_role,
                        })
                    }
                },
            };
            (resolved, op_kind, thread)
        }
    };
    tracing::info!(node_id, "diag node executed (native)");
    DiagImProbe { resolved, op_kind, self_cfid, thread }
}
```

diag.rs `node()` — chip-tool の operational read + `read_thread_signal` の区間を分岐に:

```rust
    let mut checks = Checks::default();
    let mut unavailable: Vec<Value> = Vec::new();

    // IM 部分（operational + thread）: native 資材があれば native（M8c-2）、
    // 構築失敗・未設定は従来の chip-tool 経路。
    let native_im = native.and_then(|cfg| {
        crate::native_direct::diag_im_probe(cfg, store.root(), node_id, endpoint)
    });
    let self_cfid: Option<String> = match native_im {
        Some(p) => {
            checks.operational = Some(OperationalCheck { resolved: p.resolved, kind: p.op_kind });
            match p.thread {
                Ok(tc) => checks.thread = Some(tc),
                Err(kind) => unavailable.push(json!({ "check": "thread", "kind": kind })),
            }
            Some(p.self_cfid)
        }
        None => {
            ...（現行の chip.run descriptor read 〜 parse_operational_instance_cfid 〜
                read_thread_signal のブロックをそのまま移動。cfid は現行どおり
                ログパース結果）...
        }
    };
```

以降（deep_probes / verdict / 出力）は無変更。`cfid_unavailable` の push は chip-tool 分岐の中に残る（native では発生しない）。

- [ ] **Step 4: テスト通過 + 統合回帰**

Run: `cargo test -p mat 2>&1 | tail -5`
Expected: 全 PASS（`MAT_IFACE` 未設定の diag node 既存統合テストが無改変で通る = スキーマ不変の証明）。

- [ ] **Step 5: Commit**

```bash
cargo fmt && git add crates/mat/src/native_direct.rs crates/mat/src/commands/diag.rs crates/mat/tests/integration.rs
git commit -m "feat(mat): diag nodeのIM部分native化（CFIDは資材から直接計算、1セッション共有） (M8c-2 Task7)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: 実機 E2E ハーネス

**Files:**
- Create: `scripts/e2e-m8c2-real.sh`
- Modify: `Taskfile.yml`（`e2e:m8c2:real` 追加）

**Interfaces:**
- Consumes: 0.21.0 の mat（aarch64-musl、BLE 不要）、jarvis（実 chip-tool・matd systemd・fabric index 2）、既存 commission 済みノード 2 台。

- [ ] **Step 1: 既存ハーネスの流儀を読む**

`scripts/e2e-m8a-real.sh`（直経路テスト時の matd 停止/復帰、`MAT_CHIP_TOOL_BIN=/nonexistent` 方式、マーカー grep、musl ビルド+scp）と `scripts/e2e-m8b-real.sh` を読み、env ガード・trap・色付き PASS/FAIL の形を合わせる。

- [ ] **Step 2: ハーネスを書く**

`scripts/e2e-m8c2-real.sh` — env: `MAT_E2E_HOST`（必須）、`MAT_E2E_IFACE`（既定 eth0）、`MAT_E2E_FABRIC_INDEX`（既定 2）、`MAT_E2E_TEST_NODES`（既定 `"8 9"` — living_lights の 2 台を使い捨てグループにも入れる）、`MAT_E2E_GROUP_ID`（既定 99）、`MAT_E2E_KEYSET_ID`（既定 99）、`MAT_E2E_ENDPOINT`（既定 1）。検証項目:

1. **準備**: musl ビルド（メモリの rust-lld 流儀 / 既存 task があればそれ）→ scp で `~/mat-e2e-0.21.0` へ（本番バイナリは触らない）。`sudo systemctl stop matd`（trap で必ず restart）。**KVS をバックアップ**: `cp <store>/chip_tool_config.ini{,.bak-m8c2}`（復旧はバックアップからの手動 — ハーネスは自動 restore しない、メッセージで案内のみ）。
2. **native provision（chip-tool spawn ゼロ）**: `MAT_IFACE=$IFACE MAT_CHIP_TOOL_BIN=/nonexistent/chip-tool mat group provision -g $GROUP --nodes $NODES --keyset-id $KEYSET --name e2e-m8c2` が成功し、stderr に `group provision controller state written (native kvs)` と `group provision executed (native direct)`、stdout JSON に `"status":"provisioned"` と note（`restart`）が出る。
3. **native groupcast**: `MAT_IFACE=$IFACE MAT_CHIP_TOOL_BIN=/nonexistent/chip-tool mat group invoke -g $GROUP -c onoff --command toggle` → 各ノード `mat read`（native）で on-off 反転を確認 → もう一度 toggle で戻す。
4. **rebind**: 手順 2 を `--rebind` 付きで再実行 → 成功（Duplicate にならない）。rebind 無しの再実行が `--rebind` 誘導の detail で失敗することも確認（exit≠0 + stderr grep）。
5. **chip-tool 互換**: (a) 実 chip-tool で `groupsettings show-groups` / `show-keysets`（interactive echo か単発 — 実機の chip-tool の作法で）に group 99 / keyset 99 が現れる = **mat の書いた KVS を実 chip-tool が読めた証明**（互換の主検証）。(b) groupcast 互換（best-effort）: chip-tool の group counter `g/gdc` は native counter より遅れており、そのまま送るとデバイスの replay window で落ちる（既知の counter 共有空間の性質）。ハーネスは `<store>/native_group_counter` の値を読み、python3 で `g/gdc` を「native counter + 4096」の LE u32/base64 に書き換えてから、`MAT_IFACE` 未設定の `mat group invoke` を実行 → 点滅すれば PASS、しなければ WARN（(a) が通っていれば互換自体は証明済み）。
6. **diag node native**: `MAT_IFACE=$IFACE MAT_CHIP_TOOL_BIN=/nonexistent/chip-tool mat diag node -n <NODES[0]> --deep` が成功し、stderr に `diag node executed (native)`、stdout に `"verdict"` と `checks.mdns`。`MAT_IFACE` 未設定 + 実 chip-tool でも従来どおり成功。
7. **後片付け + living_lights 無傷確認**: デバイス側の使い捨て group を best-effort で除去（各ノードへ `mat invoke -c groups --command remove-group $GROUP` 相当 — 正確な CLI は `mat invoke --help` で確認、失敗は WARN）。controller 側は実 chip-tool `groupsettings remove-group` / `remove-keyset`（失敗は WARN）。`sudo systemctl restart matd` → `mat group off/on -g 10`（matd 経由）で living_lights が応答。

- [ ] **Step 3: Taskfile 配線 + 構文チェック**

```yaml
  e2e:m8c2:real:
    desc: "M8c-2 実機 E2E: native groupsettings KVS書込 + diag node (要 jarvis + MAT_E2E_HOST)"
    cmds:
      - bash scripts/e2e-m8c2-real.sh
```

`bash -n scripts/e2e-m8c2-real.sh` + env 未設定時の明確な usage 表示を確認（実機なしで）。

- [ ] **Step 4: Commit**

```bash
git add scripts/e2e-m8c2-real.sh Taskfile.yml
git commit -m "test(e2e): M8c-2実機ハーネス（native KVS書込/chip-tool互換/diag node native） (M8c-2 Task8)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: ドキュメント + 最終確認

**Files:**
- Modify: `README.md`（native op 一覧に「group provision のコントローラ側 KVS 書込」「diag node の IM 部分」を追記、provision の note 説明）
- Modify: `ARCHITECTURE.md`（Phase 5 M8c-2 完了状況）
- Modify: `CLAUDE.md`（Backend 節の native hotpath 記述に M8c-2 分を追記 — M8b/M8c-1 の文の直後に同じ粒度で）

**Interfaces:**
- Consumes: Task 2–8 の成果。

- [ ] **Step 1: ドキュメント更新**

CLAUDE.md Backend 節へ追記する内容（一文で、既存の M8c-1 文の直後）: 「As of M8c-2, `group provision` のコントローラ側 group state（groupsettings 相当）は native 時 KVS 直書込（flock 排他、matd 経由も同様 — ハイブリッド解消）、`diag node` の IM 部分も native。KVS 書込失敗は chip-tool フォールバックしない（資材解決失敗のみフォールバック）。」README は「Native backend」相当節の op 一覧と Errors 節の note 説明を同粒度で。ARCHITECTURE はロードマップの M8c-2 を「実装済み（E2E 待ち or 合格）」へ。

- [ ] **Step 2: 全体確認**

```bash
task check 2>&1 | tail -5
```

Expected: fmt / clippy(-D warnings) / test 全通過。

- [ ] **Step 3: Commit**

```bash
git add README.md ARCHITECTURE.md CLAUDE.md
git commit -m "docs: M8c-2（KVS group書込所有+diag node native化）を反映 (M8c-2 Task9)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## 完了後（plan の範囲外、ユーザーと実施）

1. `task e2e:m8c2:real` を実機（jarvis）で実行 — 受け入れ基準は spec 参照。**KVS バックアップからの復旧手順を確認してから**回す。
2. E2E 合格 → 実測をハーネス/メモリへ反映 → main へ `--no-ff` マージ → push。
3. 本番デプロイ（0.21.0）はユーザー判断（M8c-1 と同様、M8c-3 のビルド一本化判断に絡む）。
