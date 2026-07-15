# Phase 5 M1: mat-controller コア（TLV / メッセージ層 / セッション暗号 / MRP）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** フルスクラッチ Rust Matter コントローラの最下層 — TLV codec・メッセージヘッダ codec・AES-CCM セッション暗号・メッセージカウンタ・UDP トランスポート・exchange + MRP（再送/ACK）— を新 crate `mat-controller` に実装し、ローカル `chip-all-clusters-app` に対して「信頼性フラグ付き unsecured メッセージを送り ACK を受信できる」ところまで通す。

**Architecture:** workspace に新 crate `crates/mat-controller` を追加（tokio ベースの async ライブラリ）。プロトコル実装はこの crate のみに置く（spec 2026-07-10-phase5-backend-direction-design.md のアーキテクチャ方針）。M1 は CASE より下の全レイヤ。M2（CASE + IM）はこの上に積む。既存の chip-tool 経路には一切触れない — M3 まで失敗しても既存経路は無傷。

**Tech Stack:** Rust (edition 2021, workspace 準拠), tokio (net/time/rt/macros), RustCrypto `ccm` 0.5 + `aes` 0.8, getrandom 0.2（workspace 既存）。参照実装 rust-matc（tom-code/rust-matc, BSD-2）は**読むだけ・コードコピー禁止**。仕様の一次ソースは Matter Core Specification 1.4 の §4.4（Message Format）・§4.6（Exchange）・§4.12（MRP相当, Reliable Messaging）・Appendix A（TLV）。

## Global Constraints

- crate 名は `mat-controller`（ユーザー確定 2026-07-10）。workspace members に追加する。
- プロトコル実装（TLV/CASE/暗号）は `crates/mat-controller` のみ。mat CLI / matd のコマンド層には置かない。
- rust-matc のコードはコピーしない（クリーンなフルスクラッチ。読解による参照は可）。
- 既存の mat / matd / mat-core のコードと挙動は不変。既存テストは全通過のまま。
- コミット前に必ず `task check`（fmt:check + clippy -D warnings + test）を通す。
- CI（`task test`）は実デバイス・実 chip-all-clusters-app なしで通ること。ライブテストは `#[ignore]` を付ける。
- repo は public。実 IP・実 node_id・実証明書を含めない（テストはダミー値、RFC 5737 / ループバックのみ）。
- workspace version は 0.16.0 のまま（ユーザー向け機能変更なし。バンプは M4 で matd に載る時）。

---

### Task 1: crate スキャフォールドとドキュメント更新

**Files:**
- Create: `crates/mat-controller/Cargo.toml`
- Create: `crates/mat-controller/src/lib.rs`
- Modify: `Cargo.toml`（workspace members とdependencies）
- Modify: `ARCHITECTURE.md:189-202`（Design rules）, `ARCHITECTURE.md:393-399`（Phase 5 節）, `ARCHITECTURE.md:403-416`（Things we never do）
- Modify: `CLAUDE.md`（design rule 1 と Roadmap discipline）

**Interfaces:**
- Produces: 空モジュール `tlv` / `message` / `crypto` / `counter` / `transport` / `exchange` を持つ crate `mat-controller`。後続タスクは各モジュールを埋める。

- [ ] **Step 1: crate を作る**

`crates/mat-controller/Cargo.toml`:

```toml
[package]
name = "mat-controller"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "From-scratch Matter controller library (Phase 5 backend)"

[dependencies]
tokio = { version = "1", features = ["net", "time", "rt", "macros"] }
ccm = "0.5"
aes = "0.8"
getrandom = { workspace = true }
```

`crates/mat-controller/src/lib.rs`:

```rust
//! From-scratch Matter controller library (Phase 5 backend).
//!
//! Protocol implementation lives here and only here — mat CLI / matd
//! command layers never speak TLV / CASE / crypto directly.
//! M1 scope: TLV codec, message layer, session crypto, MRP.

pub mod counter;
pub mod crypto;
pub mod exchange;
pub mod message;
pub mod tlv;
pub mod transport;
```

各モジュールファイルは空で作る（`crates/mat-controller/src/{tlv,message,crypto,counter,transport,exchange}.rs` に `//! placeholder` ではなく、モジュールの1行 doc コメントのみ）:

```rust
//! Matter TLV codec (Matter Core Spec 1.4, Appendix A).
```

（各ファイル先頭の doc コメントはそれぞれ: tlv=上記, message=`//! Message / protocol header codec (spec §4.4).`, crypto=`//! AES-128-CCM session crypto and nonce construction (spec §4.7).`, counter=`//! Message counters and replay-protection window (spec §4.5).`, transport=`//! UDP transport (IPv6, Matter port 5540).`, exchange=`//! Exchange layer and MRP reliability (spec §4.6, §4.12).`）

ルート `Cargo.toml` の members を変更:

```toml
members = ["crates/mat-core", "crates/mat", "crates/matd", "crates/mat-controller"]
```

- [ ] **Step 2: ビルドが通ることを確認**

Run: `cargo build -p mat-controller`
Expected: 成功（警告なし。unused dependency 警告は出ない — cargo はデフォルトで出さない）

- [ ] **Step 3: ARCHITECTURE.md を更新**

(a) Design rules の rule 1（`ARCHITECTURE.md:191-194`）を差し替え:

```markdown
1. **Protocol code lives only in the backend crate.** TLV, CASE, session
   crypto, multicast routing — all of it belongs to `mat-controller`
   (Phase 5) and nowhere else. The `mat` CLI and `matd` command layers
   never speak the protocol; until Phase 5 lands they delegate everything
   to `chip-tool`, which remains the production path.
```

(b) Phase 5 節（`ARCHITECTURE.md:393-399`）を差し替え:

```markdown
### Phase 5 — native backend (from-scratch Rust controller)  *(decided 2026-07-10, in progress)*
Decision record: `docs/superpowers/specs/2026-07-10-phase5-backend-direction-design.md`.
- A from-scratch Rust Matter controller library, crate `mat-controller` in
  this workspace. First stage is operational-only (CASE initiator, IM
  read/invoke, group sessions) riding the existing fabric by reading
  chip-tool's KVS; commissioning stays on chip-tool one-shot. Second stage
  (PASE / BTP / attestation) removes chip-tool entirely.
- rust-matc (tom-code/rust-matc, BSD-2) is a reference to read, never to
  copy.
- Milestones M1–M6 with independent acceptance criteria; the chip-tool
  path stays untouched until M4 swaps matd's adapter in-process.
- The replacement is still one adapter with `mat`'s JSON schema as the
  contract. Subcommands and output schema do not change.
```

(c) Things we never do の先頭項目（`ARCHITECTURE.md:405-406`）を差し替え:

```markdown
- Implement TLV / CASE / multicast routing inside `mat` or `matd` command
  layers (protocol code lives only in the `mat-controller` crate; the
  chip-tool delegation path remains until Phase 5 lands).
```

- [ ] **Step 4: CLAUDE.md を更新**

(a) Design rules の rule 1 を差し替え:

```markdown
1. **Protocol code lives only in the backend crate.** No TLV, no CASE, no
   multicast routing inside `mat` / `matd` command layers — that all
   belongs to the `mat-controller` crate (Phase 5, in progress). Until
   Phase 5 lands, the production path delegates everything to `chip-tool`.
```

(b) Roadmap discipline の段落末尾「**Phase 5** (native / backend replacement) is optional and not started.」を差し替え:

```markdown
**Phase 5** (native backend: from-scratch Rust controller, crate
`mat-controller`) is decided and in progress — see
`docs/superpowers/specs/2026-07-10-phase5-backend-direction-design.md`.
```

- [ ] **Step 5: 検証してコミット**

Run: `task check`
Expected: fmt / clippy / test 全通過

```bash
git add Cargo.toml Cargo.lock crates/mat-controller ARCHITECTURE.md CLAUDE.md
git commit -m "feat(mat-controller): Phase 5 M1 crate scaffold, docs rule updates"
```

---

### Task 2: TLV writer

**Files:**
- Modify: `crates/mat-controller/src/tlv.rs`

**Interfaces:**
- Produces: `tlv::Tag`（enum: `Anonymous | Context(u8) | CommonProfile16(u16) | CommonProfile32(u32) | ImplicitProfile16(u16) | ImplicitProfile32(u32) | FullyQualified48 { vendor: u16, profile: u16, tag: u16 } | FullyQualified64 { vendor: u16, profile: u16, tag: u32 }`）と `tlv::Writer`（`new() / put_uint(Tag, u64) / put_int(Tag, i64) / put_bool(Tag, bool) / put_f32(Tag, f32) / put_f64(Tag, f64) / put_str(Tag, &str) / put_bytes(Tag, &[u8]) / put_null(Tag) / start_struct(Tag) / start_array(Tag) / start_list(Tag) / end_container() / finish() -> Vec<u8>`）。Task 3 の Reader、M2 の CASE/IM がこれで payload を組む。

TLV ワイヤ形式（Matter spec Appendix A）: 各要素 = control byte（上位3bit = tag control, 下位5bit = element type）→ tag バイト列 → 値。数値・長さは全て little-endian。element type: `0x00-0x03` = i8/i16/i32/i64, `0x04-0x07` = u8/u16/u32/u64, `0x08` = false, `0x09` = true, `0x0A` = f32, `0x0B` = f64, `0x0C-0x0F` = UTF-8 文字列（長さ幅 1/2/4/8B）, `0x10-0x13` = octet string（同）, `0x14` = null, `0x15` = struct, `0x16` = array, `0x17` = list, `0x18` = end-of-container。tag control: `0x00` = anonymous（tagバイトなし）, `0x20` = context（1B）, `0x40`/`0x60` = common profile（2/4B）, `0x80`/`0xA0` = implicit profile（2/4B）, `0xC0` = fully-qualified 6B（vendor 2 + profile 2 + tag 2）, `0xE0` = fully-qualified 8B（vendor 2 + profile 2 + tag 4）。数値は最小幅で符号化する。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/tlv.rs` の末尾に追加:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn one(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::new();
        f(&mut w);
        w.finish()
    }

    #[test]
    fn writes_uints_minimal_width() {
        assert_eq!(one(|w| w.put_uint(Tag::Anonymous, 42)), vec![0x04, 0x2A]);
        assert_eq!(one(|w| w.put_uint(Tag::Anonymous, 420)), vec![0x05, 0xA4, 0x01]);
        assert_eq!(
            one(|w| w.put_uint(Tag::Anonymous, 70000)),
            [vec![0x06], 70000u32.to_le_bytes().to_vec()].concat()
        );
        assert_eq!(
            one(|w| w.put_uint(Tag::Anonymous, u64::MAX)),
            [vec![0x07], u64::MAX.to_le_bytes().to_vec()].concat()
        );
    }

    #[test]
    fn writes_ints_minimal_width() {
        assert_eq!(one(|w| w.put_int(Tag::Anonymous, -17)), vec![0x00, 0xEF]);
        assert_eq!(
            one(|w| w.put_int(Tag::Anonymous, -40000)),
            [vec![0x02], (-40000i32).to_le_bytes().to_vec()].concat()
        );
        assert_eq!(one(|w| w.put_int(Tag::Anonymous, 127)), vec![0x00, 0x7F]);
    }

    #[test]
    fn writes_bool_null_floats() {
        assert_eq!(one(|w| w.put_bool(Tag::Anonymous, false)), vec![0x08]);
        assert_eq!(one(|w| w.put_bool(Tag::Anonymous, true)), vec![0x09]);
        assert_eq!(one(|w| w.put_null(Tag::Anonymous)), vec![0x14]);
        assert_eq!(
            one(|w| w.put_f32(Tag::Anonymous, 17.9)),
            [vec![0x0A], 17.9f32.to_le_bytes().to_vec()].concat()
        );
        assert_eq!(
            one(|w| w.put_f64(Tag::Anonymous, 17.9)),
            [vec![0x0B], 17.9f64.to_le_bytes().to_vec()].concat()
        );
    }

    #[test]
    fn writes_strings_and_bytes() {
        assert_eq!(
            one(|w| w.put_str(Tag::Anonymous, "Hello!")),
            vec![0x0C, 0x06, b'H', b'e', b'l', b'l', b'o', b'!']
        );
        assert_eq!(
            one(|w| w.put_bytes(Tag::Anonymous, &[0, 1, 2, 3, 4])),
            vec![0x10, 0x05, 0x00, 0x01, 0x02, 0x03, 0x04]
        );
        // 256 バイトは 2 バイト長になる
        let long = vec![0xAB; 256];
        let enc = one(|w| w.put_bytes(Tag::Anonymous, &long));
        assert_eq!(&enc[..3], &[0x11, 0x00, 0x01]);
        assert_eq!(enc.len(), 3 + 256);
    }

    #[test]
    fn writes_tag_forms() {
        assert_eq!(one(|w| w.put_uint(Tag::Context(1), 42)), vec![0x24, 0x01, 0x2A]);
        assert_eq!(
            one(|w| w.put_uint(Tag::CommonProfile16(0x0100), 42)),
            vec![0x44, 0x00, 0x01, 0x2A]
        );
        assert_eq!(
            one(|w| w.put_uint(Tag::ImplicitProfile16(0x0200), 42)),
            vec![0x84, 0x00, 0x02, 0x2A]
        );
        assert_eq!(
            one(|w| w.put_uint(
                Tag::FullyQualified48 { vendor: 0xFFF1, profile: 0xDEED, tag: 1 },
                42
            )),
            vec![0xC4, 0xF1, 0xFF, 0xED, 0xDE, 0x01, 0x00, 0x2A]
        );
    }

    #[test]
    fn writes_containers() {
        assert_eq!(one(|w| { w.start_struct(Tag::Anonymous); w.end_container(); }), vec![0x15, 0x18]);
        assert_eq!(
            one(|w| {
                w.start_array(Tag::Anonymous);
                for v in 0..3 {
                    w.put_uint(Tag::Anonymous, v);
                }
                w.end_container();
            }),
            vec![0x16, 0x04, 0x00, 0x04, 0x01, 0x04, 0x02, 0x18]
        );
        assert_eq!(
            one(|w| {
                w.start_struct(Tag::Anonymous);
                w.put_uint(Tag::Context(0), 42);
                w.end_container();
            }),
            vec![0x15, 0x24, 0x00, 0x2A, 0x18]
        );
    }

    #[test]
    #[should_panic]
    fn finish_panics_on_unbalanced_container() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.finish();
    }
}
```

- [ ] **Step 2: テストが失敗する（コンパイルしない）ことを確認**

Run: `cargo test -p mat-controller tlv`
Expected: FAIL — `Tag` / `Writer` 未定義のコンパイルエラー

- [ ] **Step 3: 実装する**

`crates/mat-controller/src/tlv.rs` の doc コメント直後に追加:

```rust
/// TLV tag (Matter spec Appendix A.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Anonymous,
    Context(u8),
    CommonProfile16(u16),
    CommonProfile32(u32),
    ImplicitProfile16(u16),
    ImplicitProfile32(u32),
    FullyQualified48 { vendor: u16, profile: u16, tag: u16 },
    FullyQualified64 { vendor: u16, profile: u16, tag: u32 },
}

/// Streaming TLV encoder. Panics on unbalanced containers at `finish()`
/// (programmer error, not wire data).
pub struct Writer {
    buf: Vec<u8>,
    depth: usize,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new(), depth: 0 }
    }

    fn control_and_tag(&mut self, type_bits: u8, tag: Tag) {
        match tag {
            Tag::Anonymous => self.buf.push(type_bits),
            Tag::Context(t) => {
                self.buf.push(0x20 | type_bits);
                self.buf.push(t);
            }
            Tag::CommonProfile16(t) => {
                self.buf.push(0x40 | type_bits);
                self.buf.extend_from_slice(&t.to_le_bytes());
            }
            Tag::CommonProfile32(t) => {
                self.buf.push(0x60 | type_bits);
                self.buf.extend_from_slice(&t.to_le_bytes());
            }
            Tag::ImplicitProfile16(t) => {
                self.buf.push(0x80 | type_bits);
                self.buf.extend_from_slice(&t.to_le_bytes());
            }
            Tag::ImplicitProfile32(t) => {
                self.buf.push(0xA0 | type_bits);
                self.buf.extend_from_slice(&t.to_le_bytes());
            }
            Tag::FullyQualified48 { vendor, profile, tag } => {
                self.buf.push(0xC0 | type_bits);
                self.buf.extend_from_slice(&vendor.to_le_bytes());
                self.buf.extend_from_slice(&profile.to_le_bytes());
                self.buf.extend_from_slice(&tag.to_le_bytes());
            }
            Tag::FullyQualified64 { vendor, profile, tag } => {
                self.buf.push(0xE0 | type_bits);
                self.buf.extend_from_slice(&vendor.to_le_bytes());
                self.buf.extend_from_slice(&profile.to_le_bytes());
                self.buf.extend_from_slice(&tag.to_le_bytes());
            }
        }
    }

    pub fn put_uint(&mut self, tag: Tag, v: u64) {
        if v <= u64::from(u8::MAX) {
            self.control_and_tag(0x04, tag);
            self.buf.push(v as u8);
        } else if v <= u64::from(u16::MAX) {
            self.control_and_tag(0x05, tag);
            self.buf.extend_from_slice(&(v as u16).to_le_bytes());
        } else if v <= u64::from(u32::MAX) {
            self.control_and_tag(0x06, tag);
            self.buf.extend_from_slice(&(v as u32).to_le_bytes());
        } else {
            self.control_and_tag(0x07, tag);
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
    }

    pub fn put_int(&mut self, tag: Tag, v: i64) {
        if let Ok(v) = i8::try_from(v) {
            self.control_and_tag(0x00, tag);
            self.buf.extend_from_slice(&v.to_le_bytes());
        } else if let Ok(v) = i16::try_from(v) {
            self.control_and_tag(0x01, tag);
            self.buf.extend_from_slice(&v.to_le_bytes());
        } else if let Ok(v) = i32::try_from(v) {
            self.control_and_tag(0x02, tag);
            self.buf.extend_from_slice(&v.to_le_bytes());
        } else {
            self.control_and_tag(0x03, tag);
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
    }

    pub fn put_bool(&mut self, tag: Tag, v: bool) {
        self.control_and_tag(if v { 0x09 } else { 0x08 }, tag);
    }

    pub fn put_f32(&mut self, tag: Tag, v: f32) {
        self.control_and_tag(0x0A, tag);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_f64(&mut self, tag: Tag, v: f64) {
        self.control_and_tag(0x0B, tag);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn put_len_prefixed(&mut self, base_type: u8, tag: Tag, data: &[u8]) {
        let len = data.len();
        if let Ok(len) = u8::try_from(len) {
            self.control_and_tag(base_type, tag);
            self.buf.push(len);
        } else if let Ok(len) = u16::try_from(len) {
            self.control_and_tag(base_type + 1, tag);
            self.buf.extend_from_slice(&len.to_le_bytes());
        } else if let Ok(len) = u32::try_from(len) {
            self.control_and_tag(base_type + 2, tag);
            self.buf.extend_from_slice(&len.to_le_bytes());
        } else {
            self.control_and_tag(base_type + 3, tag);
            self.buf.extend_from_slice(&(len as u64).to_le_bytes());
        }
        self.buf.extend_from_slice(data);
    }

    pub fn put_str(&mut self, tag: Tag, v: &str) {
        self.put_len_prefixed(0x0C, tag, v.as_bytes());
    }

    pub fn put_bytes(&mut self, tag: Tag, v: &[u8]) {
        self.put_len_prefixed(0x10, tag, v);
    }

    pub fn put_null(&mut self, tag: Tag) {
        self.control_and_tag(0x14, tag);
    }

    pub fn start_struct(&mut self, tag: Tag) {
        self.control_and_tag(0x15, tag);
        self.depth += 1;
    }

    pub fn start_array(&mut self, tag: Tag) {
        self.control_and_tag(0x16, tag);
        self.depth += 1;
    }

    pub fn start_list(&mut self, tag: Tag) {
        self.control_and_tag(0x17, tag);
        self.depth += 1;
    }

    pub fn end_container(&mut self) {
        assert!(self.depth > 0, "end_container without open container");
        self.buf.push(0x18);
        self.depth -= 1;
    }

    pub fn finish(self) -> Vec<u8> {
        assert_eq!(self.depth, 0, "finish with unbalanced containers");
        self.buf
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller tlv`
Expected: PASS（7 tests）

- [ ] **Step 5: 検証してコミット**

Run: `task check`
Expected: 全通過

```bash
git add crates/mat-controller/src/tlv.rs
git commit -m "feat(mat-controller): TLV writer (all tag forms, minimal-width scalars)"
```

---

### Task 3: TLV reader

**Files:**
- Modify: `crates/mat-controller/src/tlv.rs`

**Interfaces:**
- Consumes: `tlv::Tag`, `tlv::Writer`（Task 2）
- Produces: `tlv::TlvError`（enum: `Truncated | InvalidType(u8) | InvalidUtf8 | LengthOverflow`）、`tlv::Value<'a>`（enum: `Int(i64) | Uint(u64) | Bool(bool) | F32(f32) | F64(f64) | Utf8(&'a str) | Bytes(&'a [u8]) | Null | StructStart | ArrayStart | ListStart | ContainerEnd`）、`tlv::Element<'a>`（`pub tag: Tag, pub value: Value<'a>`）、`tlv::Reader<'a>`（`new(&'a [u8]) / next() -> Result<Option<Element<'a>>, TlvError>`）。M2 の IM/CASE レスポンス解析がこれを使う。フラットなイベント列で返す（ネスト検証は上位層の責務）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/tlv.rs` の `mod tests` 内に追加:

```rust
    fn read_all(buf: &[u8]) -> Vec<Element<'_>> {
        let mut r = Reader::new(buf);
        let mut out = Vec::new();
        while let Some(el) = r.next().expect("valid tlv") {
            out.push(el);
        }
        out
    }

    #[test]
    fn reads_scalars() {
        assert_eq!(
            read_all(&[0x04, 0x2A]),
            vec![Element { tag: Tag::Anonymous, value: Value::Uint(42) }]
        );
        assert_eq!(
            read_all(&[0x00, 0xEF]),
            vec![Element { tag: Tag::Anonymous, value: Value::Int(-17) }]
        );
        assert_eq!(
            read_all(&[0x08]),
            vec![Element { tag: Tag::Anonymous, value: Value::Bool(false) }]
        );
        assert_eq!(
            read_all(&[0x14]),
            vec![Element { tag: Tag::Anonymous, value: Value::Null }]
        );
        assert_eq!(
            read_all(&[0x24, 0x01, 0x2A]),
            vec![Element { tag: Tag::Context(1), value: Value::Uint(42) }]
        );
    }

    #[test]
    fn reads_strings_bytes_containers() {
        assert_eq!(
            read_all(&[0x0C, 0x02, b'h', b'i']),
            vec![Element { tag: Tag::Anonymous, value: Value::Utf8("hi") }]
        );
        assert_eq!(
            read_all(&[0x15, 0x24, 0x00, 0x2A, 0x18]),
            vec![
                Element { tag: Tag::Anonymous, value: Value::StructStart },
                Element { tag: Tag::Context(0), value: Value::Uint(42) },
                Element { tag: Tag::Anonymous, value: Value::ContainerEnd },
            ]
        );
    }

    #[test]
    fn roundtrips_writer_output() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0xDEAD_BEEF);
        w.put_int(Tag::Context(1), -40000);
        w.put_str(Tag::Context(2), "Hello!");
        w.put_bytes(Tag::Context(3), &[1, 2, 3]);
        w.put_f64(Tag::Context(4), 17.9);
        w.start_array(Tag::Context(5));
        w.put_bool(Tag::Anonymous, true);
        w.end_container();
        w.put_uint(
            Tag::FullyQualified48 { vendor: 0xFFF1, profile: 0xDEED, tag: 7 },
            1,
        );
        w.end_container();
        let buf = w.finish();
        let els = read_all(&buf);
        assert_eq!(els[1].value, Value::Uint(0xDEAD_BEEF));
        assert_eq!(els[2].value, Value::Int(-40000));
        assert_eq!(els[3].value, Value::Utf8("Hello!"));
        assert_eq!(els[4].value, Value::Bytes(&[1, 2, 3]));
        assert_eq!(els[5].value, Value::F64(17.9));
        assert_eq!(els[6].value, Value::ArrayStart);
        assert_eq!(els[7].value, Value::Bool(true));
        assert_eq!(els[8].value, Value::ContainerEnd);
        assert_eq!(
            els[9].tag,
            Tag::FullyQualified48 { vendor: 0xFFF1, profile: 0xDEED, tag: 7 }
        );
        assert_eq!(els.len(), 11);
    }

    #[test]
    fn rejects_malformed_input() {
        // 値が足りない
        assert_eq!(Reader::new(&[0x04]).next(), Err(TlvError::Truncated));
        // 長さプレフィクスより実データが短い
        assert_eq!(Reader::new(&[0x0C, 0x05, b'h', b'i']).next(), Err(TlvError::Truncated));
        // 予約 element type (0x19-0x1F)
        assert_eq!(Reader::new(&[0x19]).next(), Err(TlvError::InvalidType(0x19)));
        // 不正 UTF-8
        assert_eq!(
            Reader::new(&[0x0C, 0x02, 0xFF, 0xFE]).next(),
            Err(TlvError::InvalidUtf8)
        );
        // tag バイトが足りない
        assert_eq!(Reader::new(&[0x24]).next(), Err(TlvError::Truncated));
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller tlv`
Expected: FAIL — `Reader` / `Value` / `Element` / `TlvError` 未定義のコンパイルエラー

- [ ] **Step 3: 実装する**

`crates/mat-controller/src/tlv.rs` に追加。`Element`/`Value` の `PartialEq` 導出、`TlvError` は `Debug + PartialEq`（テスト比較用）+ `std::fmt::Display` + `std::error::Error` を実装:

```rust
/// TLV decode error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlvError {
    Truncated,
    InvalidType(u8),
    InvalidUtf8,
    LengthOverflow,
}

impl std::fmt::Display for TlvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlvError::Truncated => write!(f, "tlv truncated"),
            TlvError::InvalidType(t) => write!(f, "invalid tlv element type 0x{t:02X}"),
            TlvError::InvalidUtf8 => write!(f, "invalid utf-8 in tlv string"),
            TlvError::LengthOverflow => write!(f, "tlv length exceeds buffer"),
        }
    }
}

impl std::error::Error for TlvError {}

/// Decoded TLV value. Strings/bytes borrow from the input buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    Int(i64),
    Uint(u64),
    Bool(bool),
    F32(f32),
    F64(f64),
    Utf8(&'a str),
    Bytes(&'a [u8]),
    Null,
    StructStart,
    ArrayStart,
    ListStart,
    ContainerEnd,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Element<'a> {
    pub tag: Tag,
    pub value: Value<'a>,
}

/// Streaming TLV decoder returning a flat event sequence.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], TlvError> {
        let end = self.pos.checked_add(n).ok_or(TlvError::LengthOverflow)?;
        if end > self.buf.len() {
            return Err(TlvError::Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn take_u16(&mut self) -> Result<u16, TlvError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn take_u32(&mut self) -> Result<u32, TlvError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn take_u64(&mut self) -> Result<u64, TlvError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn read_tag(&mut self, control: u8) -> Result<Tag, TlvError> {
        match control & 0xE0 {
            0x00 => Ok(Tag::Anonymous),
            0x20 => Ok(Tag::Context(self.take(1)?[0])),
            0x40 => Ok(Tag::CommonProfile16(self.take_u16()?)),
            0x60 => Ok(Tag::CommonProfile32(self.take_u32()?)),
            0x80 => Ok(Tag::ImplicitProfile16(self.take_u16()?)),
            0xA0 => Ok(Tag::ImplicitProfile32(self.take_u32()?)),
            0xC0 => {
                let vendor = self.take_u16()?;
                let profile = self.take_u16()?;
                let tag = self.take_u16()?;
                Ok(Tag::FullyQualified48 { vendor, profile, tag })
            }
            0xE0 => {
                let vendor = self.take_u16()?;
                let profile = self.take_u16()?;
                let tag = self.take_u32()?;
                Ok(Tag::FullyQualified64 { vendor, profile, tag })
            }
            _ => unreachable!("3-bit mask covers all cases"),
        }
    }

    fn read_len(&mut self, width_selector: u8) -> Result<usize, TlvError> {
        let len = match width_selector {
            0 => u64::from(self.take(1)?[0]),
            1 => u64::from(self.take_u16()?),
            2 => u64::from(self.take_u32()?),
            _ => self.take_u64()?,
        };
        usize::try_from(len).map_err(|_| TlvError::LengthOverflow)
    }

    /// Returns the next element, `Ok(None)` at end of input.
    pub fn next(&mut self) -> Result<Option<Element<'a>>, TlvError> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let control = self.take(1)?[0];
        let type_bits = control & 0x1F;
        let tag = self.read_tag(control)?;
        let value = match type_bits {
            0x00 => Value::Int(i64::from(self.take(1)?[0] as i8)),
            0x01 => Value::Int(i64::from(i16::from_le_bytes(
                self.take(2)?.try_into().unwrap(),
            ))),
            0x02 => Value::Int(i64::from(i32::from_le_bytes(
                self.take(4)?.try_into().unwrap(),
            ))),
            0x03 => Value::Int(i64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            0x04 => Value::Uint(u64::from(self.take(1)?[0])),
            0x05 => Value::Uint(u64::from(self.take_u16()?)),
            0x06 => Value::Uint(u64::from(self.take_u32()?)),
            0x07 => Value::Uint(self.take_u64()?),
            0x08 => Value::Bool(false),
            0x09 => Value::Bool(true),
            0x0A => Value::F32(f32::from_le_bytes(self.take(4)?.try_into().unwrap())),
            0x0B => Value::F64(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            0x0C..=0x0F => {
                let len = self.read_len(type_bits - 0x0C)?;
                let bytes = self.take(len)?;
                Value::Utf8(std::str::from_utf8(bytes).map_err(|_| TlvError::InvalidUtf8)?)
            }
            0x10..=0x13 => {
                let len = self.read_len(type_bits - 0x10)?;
                Value::Bytes(self.take(len)?)
            }
            0x14 => Value::Null,
            0x15 => Value::StructStart,
            0x16 => Value::ArrayStart,
            0x17 => Value::ListStart,
            0x18 => Value::ContainerEnd,
            t => return Err(TlvError::InvalidType(t)),
        };
        Ok(Some(Element { tag, value }))
    }
}
```

実装注意: `InvalidType` エラーは control byte 全体ではなく type bits（`0x19..=0x1F`）を返す。テストの `InvalidType(0x19)` は control byte `0x19`（anonymous + type 0x19）なので一致する。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller tlv`
Expected: PASS（11 tests）

- [ ] **Step 5: 検証してコミット**

Run: `task check`
Expected: 全通過

```bash
git add crates/mat-controller/src/tlv.rs
git commit -m "feat(mat-controller): TLV reader (flat event stream, malformed-input errors)"
```

---

### Task 4: メッセージヘッダ / プロトコルヘッダ codec

**Files:**
- Modify: `crates/mat-controller/src/message.rs`

**Interfaces:**
- Produces:
  - 定数 `PROTOCOL_ID_SECURE_CHANNEL: u16 = 0x0000` / `PROTOCOL_ID_INTERACTION_MODEL: u16 = 0x0001` / `OPCODE_MRP_STANDALONE_ACK: u8 = 0x10` / `OPCODE_STATUS_REPORT: u8 = 0x40` / `MATTER_PORT: u16 = 5540`
  - `message::Destination`（enum: `None | Node(u64) | Group(u16)`）
  - `message::MessageHeader`（`pub session_id: u16, pub security_flags: u8, pub message_counter: u32, pub source_node_id: Option<u64>, pub destination: Destination` / `encode(&self, out: &mut Vec<u8>)` / `encoded(&self) -> Vec<u8>` / `decode(&[u8]) -> Result<(MessageHeader, usize), MessageError>`（usize = payload 先頭オフセット））
  - `message::ProtocolHeader`（`pub initiator: bool, pub needs_ack: bool, pub acked_counter: Option<u32>, pub opcode: u8, pub exchange_id: u16, pub protocol_id: u16, pub vendor_id: Option<u16>` / `encode(&self, out: &mut Vec<u8>)` / `decode(&[u8]) -> Result<(ProtocolHeader, usize), MessageError>`）
  - `message::MessageError`（enum: `Truncated | UnsupportedVersion(u8)`、`Display + Error` 実装）
- Task 5（暗号の AAD/nonce）と Task 8（exchange/MRP）が使う。

ワイヤ形式（spec §4.4、全て little-endian）:
- **Message header**: message flags 1B（bit7-4 = version（0 のみ受理）, bit2 = S（source node id あり）, bit1-0 = DSIZ（0=なし, 1=node id 8B, 2=group id 2B））→ session id 2B → security flags 1B（bit7 = P, bit6 = C, bit5 = MX, bit1-0 = session type（0=unicast, 1=group））→ message counter 4B → [source node id 8B] → [dest node id 8B / group id 2B]
- **Protocol header**: exchange flags 1B（bit0 = I（initiator）, bit1 = A（ack あり）, bit2 = R（ack 要求）, bit3 = SX, bit4 = V（vendor id あり））→ opcode 1B → exchange id 2B → [protocol vendor id 2B（V 時）] → protocol id 2B → [acked message counter 4B（A 時）]

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/message.rs` の末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_minimal_unsecured_header() {
        let h = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter: 0x1234_5678,
            source_node_id: None,
            destination: Destination::None,
        };
        assert_eq!(
            h.encoded(),
            vec![0x00, 0x00, 0x00, 0x00, 0x78, 0x56, 0x34, 0x12]
        );
    }

    #[test]
    fn encodes_source_and_dest() {
        let h = MessageHeader {
            session_id: 0x0BB8,
            security_flags: 0,
            message_counter: 1,
            source_node_id: Some(0x0102_0304_0506_0708),
            destination: Destination::Node(0x1111_2222_3333_4444),
        };
        let buf = h.encoded();
        assert_eq!(buf[0], 0x05); // S | DSIZ=1
        assert_eq!(&buf[1..3], &[0xB8, 0x0B]);
        assert_eq!(&buf[8..16], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&buf[16..24], &0x1111_2222_3333_4444u64.to_le_bytes());
        let (dec, off) = MessageHeader::decode(&buf).unwrap();
        assert_eq!(dec, h);
        assert_eq!(off, 24);
    }

    #[test]
    fn roundtrips_group_dest() {
        let h = MessageHeader {
            session_id: 0x0102,
            security_flags: 0x01, // group session type
            message_counter: 7,
            source_node_id: Some(42),
            destination: Destination::Group(0x000A),
        };
        let buf = h.encoded();
        assert_eq!(buf[0], 0x06); // S | DSIZ=2
        let (dec, off) = MessageHeader::decode(&buf).unwrap();
        assert_eq!(dec, h);
        assert_eq!(off, buf.len());
    }

    #[test]
    fn rejects_bad_message_header() {
        assert_eq!(MessageHeader::decode(&[0x00, 0x00]), Err(MessageError::Truncated));
        assert_eq!(
            MessageHeader::decode(&[0x10, 0, 0, 0, 0, 0, 0, 0]),
            Err(MessageError::UnsupportedVersion(1))
        );
        // S フラグありなのに source が無い
        assert_eq!(
            MessageHeader::decode(&[0x04, 0, 0, 0, 0, 0, 0, 0]),
            Err(MessageError::Truncated)
        );
    }

    #[test]
    fn roundtrips_protocol_header() {
        let p = ProtocolHeader {
            initiator: true,
            needs_ack: true,
            acked_counter: None,
            opcode: 0x20,
            exchange_id: 0xABCD,
            protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        };
        let mut buf = Vec::new();
        p.encode(&mut buf);
        assert_eq!(buf, vec![0x05, 0x20, 0xCD, 0xAB, 0x00, 0x00]);
        let (dec, off) = ProtocolHeader::decode(&buf).unwrap();
        assert_eq!(dec, p);
        assert_eq!(off, 6);
    }

    #[test]
    fn roundtrips_protocol_header_with_ack_and_vendor() {
        let p = ProtocolHeader {
            initiator: false,
            needs_ack: false,
            acked_counter: Some(0xCAFE_F00D),
            opcode: OPCODE_MRP_STANDALONE_ACK,
            exchange_id: 1,
            protocol_id: 0xFC01,
            vendor_id: Some(0xFFF1),
        };
        let mut buf = Vec::new();
        p.encode(&mut buf);
        assert_eq!(buf[0], 0x12); // A | V
        assert_eq!(&buf[4..6], &[0xF1, 0xFF]); // vendor id は protocol id の前
        let (dec, off) = ProtocolHeader::decode(&buf).unwrap();
        assert_eq!(dec, p);
        assert_eq!(off, buf.len());
    }

    #[test]
    fn rejects_truncated_protocol_header() {
        assert_eq!(ProtocolHeader::decode(&[0x02, 0x10, 0x01, 0x00, 0x00]), Err(MessageError::Truncated));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller message`
Expected: FAIL — 型未定義のコンパイルエラー

- [ ] **Step 3: 実装する**

`crates/mat-controller/src/message.rs`:

```rust
pub const PROTOCOL_ID_SECURE_CHANNEL: u16 = 0x0000;
pub const PROTOCOL_ID_INTERACTION_MODEL: u16 = 0x0001;
pub const OPCODE_MRP_STANDALONE_ACK: u8 = 0x10;
pub const OPCODE_STATUS_REPORT: u8 = 0x40;
pub const MATTER_PORT: u16 = 5540;

const FLAG_SOURCE_PRESENT: u8 = 0x04;
const EXCHANGE_FLAG_INITIATOR: u8 = 0x01;
const EXCHANGE_FLAG_ACK: u8 = 0x02;
const EXCHANGE_FLAG_RELIABILITY: u8 = 0x04;
const EXCHANGE_FLAG_VENDOR: u8 = 0x10;

/// Message header codec error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageError {
    Truncated,
    UnsupportedVersion(u8),
}

impl std::fmt::Display for MessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageError::Truncated => write!(f, "message truncated"),
            MessageError::UnsupportedVersion(v) => write!(f, "unsupported message version {v}"),
        }
    }
}

impl std::error::Error for MessageError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    None,
    Node(u64),
    Group(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageHeader {
    pub session_id: u16,
    pub security_flags: u8,
    pub message_counter: u32,
    pub source_node_id: Option<u64>,
    pub destination: Destination,
}

impl MessageHeader {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut flags = 0u8; // version 0
        if self.source_node_id.is_some() {
            flags |= FLAG_SOURCE_PRESENT;
        }
        flags |= match self.destination {
            Destination::None => 0,
            Destination::Node(_) => 1,
            Destination::Group(_) => 2,
        };
        out.push(flags);
        out.extend_from_slice(&self.session_id.to_le_bytes());
        out.push(self.security_flags);
        out.extend_from_slice(&self.message_counter.to_le_bytes());
        if let Some(src) = self.source_node_id {
            out.extend_from_slice(&src.to_le_bytes());
        }
        match self.destination {
            Destination::None => {}
            Destination::Node(n) => out.extend_from_slice(&n.to_le_bytes()),
            Destination::Group(g) => out.extend_from_slice(&g.to_le_bytes()),
        }
    }

    pub fn encoded(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(26);
        self.encode(&mut out);
        out
    }

    /// Decodes the header; returns it with the offset where the payload starts.
    pub fn decode(buf: &[u8]) -> Result<(MessageHeader, usize), MessageError> {
        let mut c = Cursor { buf, pos: 0 };
        let flags = c.u8()?;
        let version = flags >> 4;
        if version != 0 {
            return Err(MessageError::UnsupportedVersion(version));
        }
        let session_id = c.u16()?;
        let security_flags = c.u8()?;
        let message_counter = c.u32()?;
        let source_node_id = if flags & FLAG_SOURCE_PRESENT != 0 {
            Some(c.u64()?)
        } else {
            None
        };
        let destination = match flags & 0x03 {
            1 => Destination::Node(c.u64()?),
            2 => Destination::Group(c.u16()?),
            _ => Destination::None,
        };
        Ok((
            MessageHeader { session_id, security_flags, message_counter, source_node_id, destination },
            c.pos,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolHeader {
    pub initiator: bool,
    pub needs_ack: bool,
    pub acked_counter: Option<u32>,
    pub opcode: u8,
    pub exchange_id: u16,
    pub protocol_id: u16,
    pub vendor_id: Option<u16>,
}

impl ProtocolHeader {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut flags = 0u8;
        if self.initiator {
            flags |= EXCHANGE_FLAG_INITIATOR;
        }
        if self.acked_counter.is_some() {
            flags |= EXCHANGE_FLAG_ACK;
        }
        if self.needs_ack {
            flags |= EXCHANGE_FLAG_RELIABILITY;
        }
        if self.vendor_id.is_some() {
            flags |= EXCHANGE_FLAG_VENDOR;
        }
        out.push(flags);
        out.push(self.opcode);
        out.extend_from_slice(&self.exchange_id.to_le_bytes());
        if let Some(v) = self.vendor_id {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.protocol_id.to_le_bytes());
        if let Some(a) = self.acked_counter {
            out.extend_from_slice(&a.to_le_bytes());
        }
    }

    pub fn decode(buf: &[u8]) -> Result<(ProtocolHeader, usize), MessageError> {
        let mut c = Cursor { buf, pos: 0 };
        let flags = c.u8()?;
        let opcode = c.u8()?;
        let exchange_id = c.u16()?;
        let vendor_id = if flags & EXCHANGE_FLAG_VENDOR != 0 {
            Some(c.u16()?)
        } else {
            None
        };
        let protocol_id = c.u16()?;
        let acked_counter = if flags & EXCHANGE_FLAG_ACK != 0 {
            Some(c.u32()?)
        } else {
            None
        };
        Ok((
            ProtocolHeader {
                initiator: flags & EXCHANGE_FLAG_INITIATOR != 0,
                needs_ack: flags & EXCHANGE_FLAG_RELIABILITY != 0,
                acked_counter,
                opcode,
                exchange_id,
                protocol_id,
                vendor_id,
            },
            c.pos,
        ))
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], MessageError> {
        let end = self.pos + n;
        if end > self.buf.len() {
            return Err(MessageError::Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, MessageError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, MessageError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, MessageError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, MessageError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller message`
Expected: PASS（7 tests）

- [ ] **Step 5: 検証してコミット**

Run: `task check`
Expected: 全通過

```bash
git add crates/mat-controller/src/message.rs
git commit -m "feat(mat-controller): message / protocol header codec (spec 4.4)"
```

---

### Task 5: AES-CCM セッション暗号（nonce / encrypt / decrypt / seal / open）

**Files:**
- Modify: `crates/mat-controller/src/crypto.rs`

**Interfaces:**
- Consumes: `message::{MessageHeader, ProtocolHeader, MessageError}`（Task 4）
- Produces:
  - `crypto::MIC_LEN: usize = 16`
  - `crypto::CryptoError`（enum: `AuthFailed`、`Display + Error`）
  - `crypto::OpenError`（enum: `Message(MessageError) | Crypto(CryptoError)`、`Display + Error` + `From` 両実装）
  - `crypto::build_nonce(security_flags: u8, message_counter: u32, source_node_id: u64) -> [u8; 13]`
  - `crypto::encrypt_payload(key: &[u8; 16], nonce: &[u8; 13], aad: &[u8], plaintext: &[u8]) -> Vec<u8>`
  - `crypto::decrypt_payload(key: &[u8; 16], nonce: &[u8; 13], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError>`
  - `crypto::seal_message(key: &[u8; 16], header: &MessageHeader, proto: &ProtocolHeader, payload: &[u8], session_source_node_id: u64) -> Vec<u8>`（完成した secured データグラムを返す）
  - `crypto::open_message(key: &[u8; 16], datagram: &[u8], session_source_node_id: u64) -> Result<(MessageHeader, ProtocolHeader, Vec<u8>), OpenError>`
- M2 の CASE 完了後セッションと M5 の groupcast がこの上に載る。

仕様（spec §4.7）: AES-128-CCM、MIC 16B、nonce 13B = security flags 1B ‖ message counter 4B LE ‖ source node id 8B LE。AAD = メッセージヘッダのエンコード済みバイト列全体。source node id がヘッダに無い unicast セッションでは、セッション文脈で既知の送信側 node id を nonce に使う（`seal_message`/`open_message` の `session_source_node_id` 引数。ヘッダに source があればそちらを優先）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/crypto.rs` の末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Destination, MessageHeader, ProtocolHeader};

    const KEY: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
        0x0E, 0x0F,
    ];

    #[test]
    fn builds_nonce_layout() {
        let n = build_nonce(0x00, 0x1122_3344, 0x8877_6655_4433_2211);
        assert_eq!(
            n,
            [0x00, 0x44, 0x33, 0x22, 0x11, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
    }

    #[test]
    fn roundtrips_payload() {
        let nonce = build_nonce(0, 1, 42);
        let aad = b"header-bytes";
        let ct = encrypt_payload(&KEY, &nonce, aad, b"hello matter");
        assert_eq!(ct.len(), b"hello matter".len() + MIC_LEN);
        let pt = decrypt_payload(&KEY, &nonce, aad, &ct).unwrap();
        assert_eq!(pt, b"hello matter");
    }

    #[test]
    fn rejects_tampered_ciphertext_and_aad() {
        let nonce = build_nonce(0, 1, 42);
        let mut ct = encrypt_payload(&KEY, &nonce, b"aad", b"payload");
        ct[0] ^= 0x01;
        assert!(decrypt_payload(&KEY, &nonce, b"aad", &ct).is_err());
        let ct = encrypt_payload(&KEY, &nonce, b"aad", b"payload");
        assert!(decrypt_payload(&KEY, &nonce, b"AAD", &ct).is_err());
    }

    #[test]
    fn seals_and_opens_message() {
        let header = MessageHeader {
            session_id: 0x0BB8,
            security_flags: 0,
            message_counter: 0x0100_0001,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: true,
            acked_counter: None,
            opcode: 0x08,
            exchange_id: 0x1234,
            protocol_id: crate::message::PROTOCOL_ID_INTERACTION_MODEL,
            vendor_id: None,
        };
        let datagram = seal_message(&KEY, &header, &proto, b"im-payload", 0xAAAA);
        // ヘッダ 8B は平文のまま先頭に載る
        assert_eq!(&datagram[..8], header.encoded().as_slice());
        let (h2, p2, body) = open_message(&KEY, &datagram, 0xAAAA).unwrap();
        assert_eq!(h2, header);
        assert_eq!(p2, proto);
        assert_eq!(body, b"im-payload");
        // nonce の node id が違えば開かない
        assert!(open_message(&KEY, &datagram, 0xBBBB).is_err());
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller crypto`
Expected: FAIL — 関数未定義のコンパイルエラー

- [ ] **Step 3: 実装する**

`crates/mat-controller/src/crypto.rs`:

```rust
use aes::Aes128;
use ccm::aead::{Aead, KeyInit, Payload};
use ccm::consts::{U13, U16};
use ccm::Ccm;

use crate::message::{MessageError, MessageHeader, ProtocolHeader};

type Aes128Ccm = Ccm<Aes128, U16, U13>;

/// MIC (auth tag) length for Matter secured messages.
pub const MIC_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    AuthFailed,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "message authentication failed")
    }
}

impl std::error::Error for CryptoError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    Message(MessageError),
    Crypto(CryptoError),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Message(e) => e.fmt(f),
            OpenError::Crypto(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<MessageError> for OpenError {
    fn from(e: MessageError) -> Self {
        OpenError::Message(e)
    }
}

impl From<CryptoError> for OpenError {
    fn from(e: CryptoError) -> Self {
        OpenError::Crypto(e)
    }
}

/// Nonce = security flags (1B) || message counter (4B LE) || source node id (8B LE).
pub fn build_nonce(security_flags: u8, message_counter: u32, source_node_id: u64) -> [u8; 13] {
    let mut n = [0u8; 13];
    n[0] = security_flags;
    n[1..5].copy_from_slice(&message_counter.to_le_bytes());
    n[5..13].copy_from_slice(&source_node_id.to_le_bytes());
    n
}

pub fn encrypt_payload(key: &[u8; 16], nonce: &[u8; 13], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    Aes128Ccm::new(key.into())
        .encrypt(nonce.into(), Payload { msg: plaintext, aad })
        .expect("ccm encrypt cannot fail for in-memory sizes")
}

pub fn decrypt_payload(
    key: &[u8; 16],
    nonce: &[u8; 13],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    Aes128Ccm::new(key.into())
        .decrypt(nonce.into(), Payload { msg: ciphertext, aad })
        .map_err(|_| CryptoError::AuthFailed)
}

/// Builds a complete secured datagram: plain header || CCM(protocol header || payload).
pub fn seal_message(
    key: &[u8; 16],
    header: &MessageHeader,
    proto: &ProtocolHeader,
    payload: &[u8],
    session_source_node_id: u64,
) -> Vec<u8> {
    let header_bytes = header.encoded();
    let nonce_node = header.source_node_id.unwrap_or(session_source_node_id);
    let nonce = build_nonce(header.security_flags, header.message_counter, nonce_node);
    let mut plaintext = Vec::with_capacity(payload.len() + 12);
    proto.encode(&mut plaintext);
    plaintext.extend_from_slice(payload);
    let ct = encrypt_payload(key, &nonce, &header_bytes, &plaintext);
    let mut out = header_bytes;
    out.extend_from_slice(&ct);
    out
}

/// Opens a secured datagram; returns headers and the decrypted app payload.
pub fn open_message(
    key: &[u8; 16],
    datagram: &[u8],
    session_source_node_id: u64,
) -> Result<(MessageHeader, ProtocolHeader, Vec<u8>), OpenError> {
    let (header, payload_off) = MessageHeader::decode(datagram)?;
    let nonce_node = header.source_node_id.unwrap_or(session_source_node_id);
    let nonce = build_nonce(header.security_flags, header.message_counter, nonce_node);
    let aad = &datagram[..payload_off];
    let plaintext = decrypt_payload(key, &nonce, aad, &datagram[payload_off..])?;
    let (proto, body_off) = ProtocolHeader::decode(&plaintext)?;
    Ok((header, proto, plaintext[body_off..].to_vec()))
}
```

実装注意: 相互運用の最終検証は M2 の実デバイス CASE で行う（round-trip テストは自己整合のみを保証する）。既知値ベクタは M2 で `chip-all-clusters-app` のログ／実キャプチャから採取して追加する。この前提は plan 末尾の「M1 の既知の限界」にも記載。

> **実行時訂正（2026-07-10 レビュー指摘）**: 上記コードの `encrypt_payload` の
> `expect("ccm encrypt cannot fail...")` は誤り — ccm 0.5 は 13B nonce（L=2）で
> payload 65535B 超に `Err` を返す。実装は `encrypt_payload` / `seal_message` を
> `Result<Vec<u8>, CryptoError>`（`PayloadTooLarge` variant 追加、
> `MAX_CCM_PAYLOAD = 65535` を文書化）へ変更済み（commit 732d11e）。
> M2 以降の計画はこのシグネチャを前提にすること。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller crypto`
Expected: PASS（4 tests）

- [ ] **Step 5: 検証してコミット**

Run: `task check`
Expected: 全通過

```bash
git add crates/mat-controller/src/crypto.rs Cargo.lock
git commit -m "feat(mat-controller): AES-128-CCM session crypto, nonce, seal/open"
```

---

### Task 6: メッセージカウンタとリプレイ保護ウィンドウ

**Files:**
- Modify: `crates/mat-controller/src/counter.rs`

**Interfaces:**
- Produces:
  - `counter::TxCounter`（`new_random() -> Self`（初期値 [1, 2^28] の乱数、spec §4.5.1）/ `next(&mut self) -> u32`（現在値を返して増分））
  - `counter::RxWindow`（`new() -> Self` / `check_and_commit(&mut self, counter: u32) -> bool`（新規なら true、重複・窓外の古い値なら false。窓幅 32））
- Task 8 の exchange/MRP が送信カウンタと受信重複排除に使う。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/counter.rs` の末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_counter_starts_in_spec_range_and_increments() {
        let mut c = TxCounter::new_random();
        let a = c.next();
        let b = c.next();
        assert!((1..=(1u32 << 28)).contains(&a));
        assert_eq!(b, a + 1);
    }

    #[test]
    fn rx_window_accepts_fresh_rejects_duplicates() {
        let mut w = RxWindow::new();
        assert!(w.check_and_commit(100));
        assert!(!w.check_and_commit(100));
        assert!(w.check_and_commit(101));
        assert!(!w.check_and_commit(101));
        assert!(!w.check_and_commit(100));
    }

    #[test]
    fn rx_window_accepts_out_of_order_within_window() {
        let mut w = RxWindow::new();
        assert!(w.check_and_commit(100));
        assert!(w.check_and_commit(105));
        assert!(w.check_and_commit(103)); // 窓内・未見
        assert!(!w.check_and_commit(103)); // 二度目は重複
        assert!(!w.check_and_commit(100)); // commit 済み
        assert!(w.check_and_commit(104));
    }

    #[test]
    fn rx_window_rejects_too_old() {
        let mut w = RxWindow::new();
        assert!(w.check_and_commit(1000));
        assert!(!w.check_and_commit(1000 - 33)); // 窓幅 32 の外
        assert!(w.check_and_commit(1000 - 32)); // ちょうど窓の端は受理
    }

    #[test]
    fn rx_window_survives_large_jump() {
        let mut w = RxWindow::new();
        assert!(w.check_and_commit(10));
        assert!(w.check_and_commit(10_000));
        assert!(!w.check_and_commit(10_000));
        assert!(!w.check_and_commit(10)); // 窓外に落ちた
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller counter`
Expected: FAIL — 型未定義のコンパイルエラー

- [ ] **Step 3: 実装する**

`crates/mat-controller/src/counter.rs`:

```rust
/// Outgoing message counter, randomly initialized per spec 4.5.1.
pub struct TxCounter(u32);

impl TxCounter {
    pub fn new_random() -> Self {
        let mut b = [0u8; 4];
        getrandom::getrandom(&mut b).expect("os rng");
        Self((u32::from_le_bytes(b) & 0x0FFF_FFFF) + 1)
    }

    /// Returns the current counter value and advances.
    pub fn next(&mut self) -> u32 {
        let v = self.0;
        self.0 = self.0.wrapping_add(1);
        v
    }
}

/// Sliding replay-protection window (width 32) over received counters.
pub struct RxWindow {
    max: u32,
    /// bit i set = counter (max - 1 - i) already seen
    bitmap: u32,
    empty: bool,
}

impl RxWindow {
    pub fn new() -> Self {
        Self { max: 0, bitmap: 0, empty: true }
    }

    /// Returns true (and commits) if the counter is fresh; false on
    /// duplicates and on counters older than the window.
    pub fn check_and_commit(&mut self, counter: u32) -> bool {
        if self.empty {
            self.empty = false;
            self.max = counter;
            self.bitmap = 0;
            return true;
        }
        if counter > self.max {
            let delta = counter - self.max;
            self.bitmap = if delta >= 32 {
                0
            } else {
                (self.bitmap << delta) | (1 << (delta - 1))
            };
            self.max = counter;
            return true;
        }
        if counter == self.max {
            return false;
        }
        let offset = self.max - counter; // >= 1
        if offset > 32 {
            return false;
        }
        let bit = 1u32 << (offset - 1);
        if self.bitmap & bit != 0 {
            return false;
        }
        self.bitmap |= bit;
        true
    }
}

impl Default for RxWindow {
    fn default() -> Self {
        Self::new()
    }
}
```

実装注意: `offset == 32` のとき `1u32 << 31` で境界ちょうどが窓に入る（テスト `rejects_too_old` が固定）。M1 ではロールオーバー（u32 折り返し）は扱わない — セッションは短命で、実 Matter でもカウンタ枯渇はセッション再確立。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller counter`
Expected: PASS（5 tests）

- [ ] **Step 5: 検証してコミット**

Run: `task check`
Expected: 全通過

```bash
git add crates/mat-controller/src/counter.rs
git commit -m "feat(mat-controller): tx counter and replay-protection window"
```

---

### Task 7: UDP トランスポート

**Files:**
- Modify: `crates/mat-controller/src/transport.rs`

**Interfaces:**
- Produces:
  - `transport::MAX_DATAGRAM: usize = 1280`（Matter UDP MTU）
  - `transport::UdpTransport`（`bind() -> io::Result<Self>`（`[::]:0`）/ `bind_addr(addr: SocketAddr) -> io::Result<Self>` / `send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()>` / `recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)>` / `local_addr(&self) -> io::Result<SocketAddr>`）— 全メソッド async（`local_addr` 除く）
- Task 8 の exchange と Task 9 のライブテストが使う。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/transport.rs` の末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrips_datagram_over_loopback() {
        let a = UdpTransport::bind_addr("[::1]:0".parse().unwrap()).await.unwrap();
        let b = UdpTransport::bind_addr("[::1]:0".parse().unwrap()).await.unwrap();
        a.send_to(b"ping", b.local_addr().unwrap()).await.unwrap();
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = b.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
        assert_eq!(from, a.local_addr().unwrap());
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller transport`
Expected: FAIL — 型未定義のコンパイルエラー

- [ ] **Step 3: 実装する**

`crates/mat-controller/src/transport.rs`:

```rust
use std::io;
use std::net::SocketAddr;

use tokio::net::UdpSocket;

/// Matter caps UDP payloads at 1280 bytes (IPv6 minimum MTU).
pub const MAX_DATAGRAM: usize = 1280;

pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// Binds an ephemeral IPv6 port for controller use.
    pub async fn bind() -> io::Result<Self> {
        Self::bind_addr("[::]:0".parse().expect("static addr")).await
    }

    pub async fn bind_addr(addr: SocketAddr) -> io::Result<Self> {
        Ok(Self { socket: UdpSocket::bind(addr).await? })
    }

    pub async fn send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()> {
        let n = self.socket.send_to(buf, dest).await?;
        if n != buf.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short datagram send"));
        }
        Ok(())
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller transport`
Expected: PASS（1 test）

- [ ] **Step 5: 検証してコミット**

Run: `task check`
Expected: 全通過

```bash
git add crates/mat-controller/src/transport.rs
git commit -m "feat(mat-controller): tokio UDP transport"
```

---

### Task 8: exchange 層と MRP（再送 / ACK / 重複排除）

**Files:**
- Modify: `crates/mat-controller/src/exchange.rs`

**Interfaces:**
- Consumes: `transport::{UdpTransport, MAX_DATAGRAM}`（Task 7）, `message::*`（Task 4）, `counter::{TxCounter, RxWindow}`（Task 6）
- Produces:
  - `exchange::MrpConfig`（`pub initial_interval: Duration, pub max_retries: u32, pub backoff: f64`、`Default` = 300ms / 4 / 1.6）
  - `exchange::ExchangeError`（enum: `Timeout | Io(std::io::Error) | Message(MessageError)`、`Display + Error + From`）
  - `exchange::IncomingMessage`（`pub header: MessageHeader, pub proto: ProtocolHeader, pub payload: Vec<u8>`）
  - `exchange::UnsecuredExchange<'t>`:
    - `new(transport: &'t UdpTransport, peer: SocketAddr) -> Self`（exchange id と ephemeral source node id を乱数生成）
    - `send_reliable(&mut self, protocol_id: u16, opcode: u8, payload: &[u8], cfg: &MrpConfig) -> Result<Option<IncomingMessage>, ExchangeError>`（R+I フラグ付き送信、ACK 到達まで再送。standalone ack のみなら `None`、実応答メッセージが来たら `Some`）
    - `recv(&mut self, timeout: Duration) -> Result<IncomingMessage, ExchangeError>`（この exchange の次の実メッセージを返す。R フラグ付き受信は自動で standalone ack、重複はウィンドウで排除して再 ACK）
- M2 の CASE（Sigma1 送信 → Sigma2 受信 → Sigma3 送信）はこの 2 メソッドの上に書ける。

MRP 挙動（spec §4.12）: 再送は**同一データグラム**（カウンタ再増分なし）。バックオフは `interval *= backoff` で `max_retries` 回まで。受信側は R フラグ付きメッセージに standalone ack（SC protocol 0x0000, opcode 0x10, acked counter = 受信メッセージのカウンタ）を返す。重複（リプレイウィンドウで既見）は処理せず再 ACK のみ。exchange 起点側が送るメッセージは I フラグ = 1。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/exchange.rs` の末尾:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        Destination, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
        OPCODE_STATUS_REPORT, PROTOCOL_ID_SECURE_CHANNEL,
    };
    use crate::transport::{UdpTransport, MAX_DATAGRAM};
    use std::net::SocketAddr;
    use std::time::Duration;

    fn fast_cfg() -> MrpConfig {
        MrpConfig { initial_interval: Duration::from_millis(50), max_retries: 2, backoff: 1.0 }
    }

    async fn bind_local() -> UdpTransport {
        UdpTransport::bind_addr("[::1]:0".parse().unwrap()).await.unwrap()
    }

    async fn read_msg(t: &UdpTransport) -> (MessageHeader, ProtocolHeader, SocketAddr) {
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = t.recv_from(&mut buf).await.unwrap();
        let (h, off) = MessageHeader::decode(&buf[..n]).unwrap();
        let (p, _) = ProtocolHeader::decode(&buf[off..n]).unwrap();
        (h, p, from)
    }

    fn reply_datagram(
        exchange_id: u16,
        opcode: u8,
        acked: Option<u32>,
        needs_ack: bool,
        msg_counter: u32,
    ) -> Vec<u8> {
        let h = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter: msg_counter,
            source_node_id: None,
            destination: Destination::None,
        };
        let p = ProtocolHeader {
            initiator: false,
            needs_ack,
            acked_counter: acked,
            opcode,
            exchange_id,
            protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        };
        let mut buf = h.encoded();
        p.encode(&mut buf);
        buf
    }

    #[tokio::test]
    async fn send_reliable_completes_on_standalone_ack() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);

        let responder_task = tokio::spawn(async move {
            let (h, p, from) = read_msg(&responder).await;
            assert!(p.needs_ack);
            assert!(p.initiator);
            let ack = reply_datagram(
                p.exchange_id,
                OPCODE_MRP_STANDALONE_ACK,
                Some(h.message_counter),
                false,
                7000,
            );
            responder.send_to(&ack, from).await.unwrap();
        });

        let res = ex
            .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"", &fast_cfg())
            .await
            .unwrap();
        assert!(res.is_none());
        responder_task.await.unwrap();
    }

    #[tokio::test]
    async fn send_reliable_retransmits_same_counter() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);

        let responder_task = tokio::spawn(async move {
            let (h1, _, _) = read_msg(&responder).await; // 1通目は握りつぶす
            let (h2, p2, from) = read_msg(&responder).await; // 再送
            assert_eq!(h1.message_counter, h2.message_counter);
            let ack = reply_datagram(
                p2.exchange_id,
                OPCODE_MRP_STANDALONE_ACK,
                Some(h2.message_counter),
                false,
                7000,
            );
            responder.send_to(&ack, from).await.unwrap();
        });

        let res = ex
            .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"", &fast_cfg())
            .await
            .unwrap();
        assert!(res.is_none());
        responder_task.await.unwrap();
    }

    #[tokio::test]
    async fn send_reliable_times_out_without_ack() {
        let responder = bind_local().await; // 何も返さない
        let peer = responder.local_addr().unwrap();
        let transport = bind_local().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);
        let err = ex
            .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"", &fast_cfg())
            .await
            .unwrap_err();
        assert!(matches!(err, ExchangeError::Timeout));
    }

    #[tokio::test]
    async fn send_reliable_returns_piggybacked_response_and_acks_it() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);

        let responder_task = tokio::spawn(async move {
            let (h, p, from) = read_msg(&responder).await;
            // 実応答（StatusReport）に A フラグを相乗りさせ、こちらも ACK を要求する
            let reply = reply_datagram(
                p.exchange_id,
                OPCODE_STATUS_REPORT,
                Some(h.message_counter),
                true,
                8000,
            );
            responder.send_to(&reply, from).await.unwrap();
            // 相手側 MRP が standalone ack を返してくるはず
            let (_, ack_p, _) = read_msg(&responder).await;
            assert_eq!(ack_p.opcode, OPCODE_MRP_STANDALONE_ACK);
            assert_eq!(ack_p.acked_counter, Some(8000));
        });

        let res = ex
            .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"", &fast_cfg())
            .await
            .unwrap()
            .expect("real response expected");
        assert_eq!(res.proto.opcode, OPCODE_STATUS_REPORT);
        responder_task.await.unwrap();
    }

    #[tokio::test]
    async fn recv_dedups_and_reacks_duplicates() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local().await;
        let local = transport.local_addr().unwrap();
        let mut ex = UnsecuredExchange::new(&transport, peer);
        let exchange_id = ex.exchange_id();

        let responder_task = tokio::spawn(async move {
            let msg = reply_datagram(exchange_id, OPCODE_STATUS_REPORT, None, true, 9000);
            // 同一メッセージを2回送る（重複）
            responder.send_to(&msg, local).await.unwrap();
            responder.send_to(&msg, local).await.unwrap();
            // ACK は2回来る（初回 + 重複への再 ACK）が、メッセージ本体は1度しか渡らない
            let (_, a1, _) = read_msg(&responder).await;
            let (_, a2, _) = read_msg(&responder).await;
            assert_eq!(a1.opcode, OPCODE_MRP_STANDALONE_ACK);
            assert_eq!(a1.acked_counter, Some(9000));
            assert_eq!(a2.opcode, OPCODE_MRP_STANDALONE_ACK);
            assert_eq!(a2.acked_counter, Some(9000));
        });

        let first = ex.recv(Duration::from_millis(500)).await.unwrap();
        assert_eq!(first.header.message_counter, 9000);
        // 2通目（重複）は渡ってこない → タイムアウト
        let err = ex.recv(Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, ExchangeError::Timeout));
        responder_task.await.unwrap();
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller exchange`
Expected: FAIL — 型未定義のコンパイルエラー

- [ ] **Step 3: 実装する**

`crates/mat-controller/src/exchange.rs`:

```rust
use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::Instant;

use crate::counter::{RxWindow, TxCounter};
use crate::message::{
    Destination, MessageError, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
    PROTOCOL_ID_SECURE_CHANNEL,
};
use crate::transport::{UdpTransport, MAX_DATAGRAM};

/// MRP retransmission parameters (spec 4.12; defaults follow chip defaults).
pub struct MrpConfig {
    pub initial_interval: Duration,
    pub max_retries: u32,
    pub backoff: f64,
}

impl Default for MrpConfig {
    fn default() -> Self {
        Self {
            initial_interval: Duration::from_millis(300),
            max_retries: 4,
            backoff: 1.6,
        }
    }
}

#[derive(Debug)]
pub enum ExchangeError {
    Timeout,
    Io(std::io::Error),
    Message(MessageError),
}

impl std::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExchangeError::Timeout => write!(f, "no acknowledgement within MRP retry budget"),
            ExchangeError::Io(e) => write!(f, "transport error: {e}"),
            ExchangeError::Message(e) => write!(f, "peer sent malformed message: {e}"),
        }
    }
}

impl std::error::Error for ExchangeError {}

impl From<std::io::Error> for ExchangeError {
    fn from(e: std::io::Error) -> Self {
        ExchangeError::Io(e)
    }
}

impl From<MessageError> for ExchangeError {
    fn from(e: MessageError) -> Self {
        ExchangeError::Message(e)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IncomingMessage {
    pub header: MessageHeader,
    pub proto: ProtocolHeader,
    pub payload: Vec<u8>,
}

/// One unsecured (session id 0) exchange, this side as initiator, with MRP.
pub struct UnsecuredExchange<'t> {
    transport: &'t UdpTransport,
    peer: SocketAddr,
    exchange_id: u16,
    source_node_id: u64,
    counter: TxCounter,
    rx_window: RxWindow,
}

impl<'t> UnsecuredExchange<'t> {
    pub fn new(transport: &'t UdpTransport, peer: SocketAddr) -> Self {
        let mut b = [0u8; 10];
        getrandom::getrandom(&mut b).expect("os rng");
        Self {
            transport,
            peer,
            exchange_id: u16::from_le_bytes([b[0], b[1]]),
            source_node_id: u64::from_le_bytes(b[2..10].try_into().expect("8 bytes")),
            counter: TxCounter::new_random(),
            rx_window: RxWindow::new(),
        }
    }

    pub fn exchange_id(&self) -> u16 {
        self.exchange_id
    }

    fn build(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        needs_ack: bool,
        acked_counter: Option<u32>,
        payload: &[u8],
    ) -> (Vec<u8>, u32) {
        let message_counter = self.counter.next();
        let header = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter,
            source_node_id: Some(self.source_node_id),
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack,
            acked_counter,
            opcode,
            exchange_id: self.exchange_id,
            protocol_id,
            vendor_id: None,
        };
        let mut buf = header.encoded();
        proto.encode(&mut buf);
        buf.extend_from_slice(payload);
        (buf, message_counter)
    }

    async fn send_standalone_ack(&mut self, acked: u32) -> Result<(), ExchangeError> {
        let (buf, _) = self.build(
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_MRP_STANDALONE_ACK,
            false,
            Some(acked),
            &[],
        );
        self.transport.send_to(&buf, self.peer).await?;
        Ok(())
    }

    /// Decodes a datagram and screens it for this exchange. Returns `None`
    /// for foreign/duplicate/ack-only traffic that the caller should skip
    /// (duplicates are re-acked here).
    async fn screen(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
        if from != self.peer {
            return Ok(None);
        }
        let (header, off) = match MessageHeader::decode(buf) {
            Ok(v) => v,
            Err(_) => return Ok(None), // 不正データグラムは無視（DoS 耐性）
        };
        if header.session_id != 0 || header.security_flags != 0 {
            return Ok(None);
        }
        let (proto, body_off) = match ProtocolHeader::decode(&buf[off..]) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if proto.exchange_id != self.exchange_id || proto.initiator {
            return Ok(None);
        }
        if !self.rx_window.check_and_commit(header.message_counter) {
            if proto.needs_ack {
                self.send_standalone_ack(header.message_counter).await?;
            }
            return Ok(None);
        }
        if proto.needs_ack {
            self.send_standalone_ack(header.message_counter).await?;
        }
        Ok(Some(IncomingMessage {
            header,
            proto,
            payload: buf[off + body_off..].to_vec(),
        }))
    }

    /// Sends a reliability-flagged message and retransmits until the peer
    /// acknowledges it. Returns the peer's real response if one carried the
    /// ack (or arrived on the exchange), `None` for a standalone ack.
    pub async fn send_reliable(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
        let (datagram, our_counter) = self.build(protocol_id, opcode, true, None, payload);
        let mut interval = cfg.initial_interval;
        let mut attempts = 0u32;
        loop {
            self.transport.send_to(&datagram, self.peer).await?;
            let deadline = Instant::now() + interval;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let mut buf = [0u8; MAX_DATAGRAM];
                let Ok(recv) =
                    tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
                else {
                    break; // interval 経過 → 再送
                };
                let (n, from) = recv?;
                let Some(msg) = self.screen(&buf[..n], from).await? else {
                    // ack-only の可能性: screen は standalone ack も Some で返す
                    continue;
                };
                let acks_us = msg.proto.acked_counter == Some(our_counter);
                let is_standalone_ack = msg.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                    && msg.proto.opcode == OPCODE_MRP_STANDALONE_ACK;
                if is_standalone_ack {
                    if acks_us {
                        return Ok(None);
                    }
                    continue;
                }
                // exchange 上の実メッセージは応答とみなす（相手が処理した証拠）
                return Ok(Some(msg));
            }
            attempts += 1;
            if attempts > cfg.max_retries {
                return Err(ExchangeError::Timeout);
            }
            interval = interval.mul_f64(cfg.backoff);
        }
    }

    /// Waits for the next real (non-ack) message on this exchange.
    pub async fn recv(&mut self, timeout: Duration) -> Result<IncomingMessage, ExchangeError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ExchangeError::Timeout);
            }
            let mut buf = [0u8; MAX_DATAGRAM];
            let Ok(recv) =
                tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
            else {
                return Err(ExchangeError::Timeout);
            };
            let (n, from) = recv?;
            let Some(msg) = self.screen(&buf[..n], from).await? else {
                continue;
            };
            if msg.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                && msg.proto.opcode == OPCODE_MRP_STANDALONE_ACK
            {
                continue;
            }
            return Ok(msg);
        }
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller exchange`
Expected: PASS（5 tests）。タイミング依存（50ms 間隔）なので flaky なら間隔を 100ms に上げてよい。

- [ ] **Step 5: 検証してコミット**

Run: `task check`
Expected: 全通過

```bash
git add crates/mat-controller/src/exchange.rs
git commit -m "feat(mat-controller): unsecured exchange with MRP retransmit/ack/dedup"
```

---

### Task 9: chip-all-clusters-app ハーネスとライブ E2E（M1 受け入れ）

**Files:**
- Modify: `Dockerfile`（`all-clusters-builder` ステージ追加）
- Modify: `Taskfile.yml`（`chip:extract:app` / `e2e:m1` タスク追加）
- Modify: `.gitignore`（取り出しバイナリ除外）
- Create: `crates/mat-controller/tests/live_all_clusters.rs`

**Interfaces:**
- Consumes: `transport::UdpTransport`, `exchange::{UnsecuredExchange, MrpConfig}`, `message::{MATTER_PORT, PROTOCOL_ID_SECURE_CHANNEL}`（Task 4/7/8）
- Produces: M1 受け入れ試験そのもの。M2 も同じハーネス（ローカル example device）を相手に CASE を開発する。

- [ ] **Step 1: Dockerfile にステージを追加**

`Dockerfile` の Stage 1（chip-builder）末尾と Stage 2 の間に挿入:

```dockerfile
# ── Stage 1b: chip-all-clusters-app（Phase 5 開発の相手役デバイス）──────────────
# chip-builder のビルド済みツリー上で example を1つ追加ビルドするだけ（キャッシュが効く）。
FROM chip-builder AS all-clusters-builder
RUN bash -c "source scripts/activate.sh && \
    scripts/examples/gn_build_example.sh examples/all-clusters-app/linux out/all-clusters"
```

出力パス: `/work/connectedhomeip/out/all-clusters/chip-all-clusters-app`

- [ ] **Step 2: Taskfile.yml にタスクを追加**

`chip:extract:arm64` の後に追加（既存タスクのスタイルに合わせる）:

```yaml
  chip:extract:app:
    desc: chip-all-clusters-app（Phase 5 開発用の相手役デバイス）を Docker でビルドし ./chip-all-clusters-app に取り出す
    cmds:
      - docker build --target all-clusters-builder -t mat-all-clusters-builder .
      - |
        id=$(docker create mat-all-clusters-builder)
        docker cp "$id:/work/connectedhomeip/out/all-clusters/chip-all-clusters-app" ./chip-all-clusters-app
        docker rm "$id"
      - 'echo "取り出し完了: $PWD/chip-all-clusters-app"'
      - 'echo "起動: ./chip-all-clusters-app  （udp/5540 で待ち受け。KVS は /tmp/chip_kvs）"'

  e2e:m1:
    desc: M1 ライブ E2E（先に ./chip-all-clusters-app を起動しておく）
    cmds:
      - cargo test -p mat-controller --test live_all_clusters -- --ignored --nocapture
```

`.gitignore` の chip-tool の項の下に追加:

```gitignore
# Docker から取り出した example device バイナリ（Phase 5 開発用）
/chip-all-clusters-app
```

- [ ] **Step 3: ライブテストを書く**

`crates/mat-controller/tests/live_all_clusters.rs`:

```rust
//! Live E2E against a local chip-all-clusters-app. Not run in CI.
//!
//! Setup:
//!   task chip:extract:app
//!   ./chip-all-clusters-app          # udp/5540 で待ち受け
//!   task e2e:m1                      # または cargo test ... -- --ignored

use std::time::Duration;

use mat_controller::exchange::{MrpConfig, UnsecuredExchange};
use mat_controller::message::{MATTER_PORT, PROTOCOL_ID_SECURE_CHANNEL};
use mat_controller::transport::UdpTransport;

/// Secure Channel: CASE Sigma1 opcode.
const OPCODE_CASE_SIGMA1: u8 = 0x30;

#[tokio::test]
#[ignore = "requires local chip-all-clusters-app on udp/5540"]
async fn reliable_message_gets_acked_by_real_device() {
    let transport = UdpTransport::bind().await.unwrap();
    let peer = format!("[::1]:{MATTER_PORT}").parse().unwrap();
    let mut ex = UnsecuredExchange::new(&transport, peer);

    // 中身が TLV として不正な Sigma1。デバイスの CASE ハンドラはパースに失敗して
    // StatusReport を返すが、MRP 層は処理結果と無関係に受信を ACK する。
    // M1 の合格条件は「実デバイスがこちらの reliable メッセージを ACK する」まで。
    let res = ex
        .send_reliable(
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_CASE_SIGMA1,
            &[0xDE, 0xAD],
            &MrpConfig::default(),
        )
        .await
        .expect("device must acknowledge our reliable message");

    match res {
        Some(msg) => {
            assert_eq!(msg.proto.protocol_id, PROTOCOL_ID_SECURE_CHANNEL);
            eprintln!(
                "device responded: SC opcode 0x{:02X}, {} byte payload",
                msg.proto.opcode,
                msg.payload.len()
            );
            // 追加応答が reliable で来た場合の後始末（ACK は screen 内で送信済み）
        }
        None => eprintln!("device sent a standalone ack"),
    }

    // 少し待って、遅れて届く応答（standalone ack 後の StatusReport）も観測して
    // ACK を返しておく。失敗しても M1 合格条件には影響しない。
    if let Ok(late) = ex.recv(Duration::from_millis(800)).await {
        eprintln!("late response: SC opcode 0x{:02X}", late.proto.opcode);
    }
}
```

- [ ] **Step 4: CI が壊れていないことを確認**

Run: `task check`
Expected: 全通過（ライブテストは `#[ignore]` なので実行されない）

- [ ] **Step 5: 相手役をビルド・起動して E2E を実行**

```bash
task chip:extract:app          # 初回のみ（chip-builder キャッシュが効いていれば十数分）
./chip-all-clusters-app &      # 別ターミナル推奨。ログが大量に出る
task e2e:m1
```

Expected: `reliable_message_gets_acked_by_real_device ... ok`。stderr に `device sent a standalone ack` または `device responded: SC opcode 0x40`（StatusReport）が出る。

確認後、`kill %1`（または起動したターミナルで Ctrl-C）で相手役を止め、`rm -f /tmp/chip_kvs*` で KVS を掃除してよい。

トラブルシュート:
- `Timeout` で落ちる → app が起動しているか（`ss -ulpn | grep 5540`）。WSL2 なら v6 loopback は常時使える。app のログに `Received message` 系が出ているのに ACK が来ない場合は message flags / exchange flags のバイト並びを疑う（Wireshark の Matter dissector か app 側ログの `Msg RX` 行と突き合わせる）。
- app が起動直後に落ちる → `/tmp/chip_kvs` の残骸を消して再起動。

- [ ] **Step 6: コミット**

```bash
git add Dockerfile Taskfile.yml .gitignore crates/mat-controller/tests/live_all_clusters.rs
git commit -m "feat(mat-controller): all-clusters-app harness and M1 live ack E2E"
```

---

## M1 の既知の限界（M2 への引き継ぎ事項）

- **暗号の相互運用は未検証**: AES-CCM seal/open は round-trip テストのみ（自己整合）。実デバイスとの初の暗号相互運用は M2 の CASE 完了時に初めて証明される。nonce の node id 選択（ヘッダ欠落時にセッション文脈の送信側 node id を使う）が最初に疑うべき箇所。
- **exchange は起点側・単一 exchange のみ**: 応答側 exchange・並行 exchange・unsolicited dispatch は M2/M4（matd 常駐化）で必要になった時に足す。
- **secured セッション上の MRP は未接続**: `UnsecuredExchange` は session id 0 専用。CASE 確立後の secured exchange は M2 で `seal_message`/`open_message`（Task 5）と MRP ループを組み合わせて作る。
- **グループ / privacy / control message / message extensions は未対応**（M5 以降。decode は security flags を素通しするので、来ても壊れず screen で弾かれる）。
- MRP の再送パラメータはデバイス広告値（mDNS TXT の SII/SAI）を読まず固定デフォルト。M3 の mDNS 実装時に接続する。

## Self-Review（記録）

- spec §マイルストーン M1「TLV codec + メッセージ層 + セッション暗号（unsecured/secured unicast）。相手はローカル chip-all-clusters-app」→ Task 2/3（TLV）、Task 4（メッセージ層）、Task 5（セッション暗号 secured unicast の seal/open）、Task 6-8（カウンタ・トランスポート・MRP、メッセージ層の一部）、Task 9（相手役 + ライブ受け入れ）で全て充足。
- spec §アーキテクチャ方針「専用 crate に閉じる」「doc 変更」→ Task 1。
- spec §未決事項のうち本計画で確定したもの: crate 名 = `mat-controller`（ユーザー確定）。公開 API の形 = 本計画の Interfaces 節（M1 範囲）。残り（counter 永続化の排他、mat 直経路の載せ替え時期、KVS 互換範囲、subscribe 要否）は M3〜M5 の計画で扱う。
- 型整合: `Tag`/`Writer`/`Reader`（Task 2→3）、`MessageHeader`/`ProtocolHeader`（Task 4→5/8/9）、`TxCounter`/`RxWindow`（Task 6→8）、`UdpTransport`（Task 7→8/9）、`UnsecuredExchange::{new, exchange_id, send_reliable, recv}`（Task 8→9）で名前・シグネチャ一致を確認済み。
