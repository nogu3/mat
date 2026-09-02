# 汎用 write / invoke の型拡張（float・list・struct）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 汎用 `write` / `invoke` が float・list・struct の値を生成テーブルの型記述に沿って JSON→TLV 符号化できるようにし、listen が list/struct を捨てる非対称を仕様として明文化する。

**Architecture:** `scripts/gen-ids.py` を拡張して struct のフィールド定義（`StructDef` / `StructField`）を `ids_gen.rs` に生成し、`mat-core::ids` の型記述を `Ty`（Scalar / Struct / List / ListOfStruct）に変える。値ツリー `ScalarValue` に F32/F64/List/Struct を足し、`parse_value_typed` が JSON を型記述で検査して値ツリーにし、`mat-native::put_value` が再帰的に TLV へ書く。専用エンコーダ（provision/grant）は据え置き、等価性テストで縛る。

**Tech Stack:** Rust（workspace、`serde_json`）、Python 3（生成スクリプト）、connectedhomeip v1.4.2.0 data-model XML（sparse clone）。

**Spec:** `docs/superpowers/specs/2026-09-03-write-types-design.md`

## Global Constraints

- 作業ブランチ `worktree-write-types`、worktree `.claude/worktrees/write-types`（main = `b68e3ab` 起点）。
- **触ってよいファイル**: `crates/mat-core/src/{ids.rs,ids_gen.rs,parse.rs}`、`scripts/gen-ids.py`、`crates/mat-controller/src/im.rs`（`ImValue` / `value_to_im` / `encode_im_value` / JSON ヘルパ追加のみ）、`crates/mat-native/src/lib.rs`、`crates/mat-native/src/ops.rs`（`encode_acl_entries_tlv` の可視性のみ）、`crates/matd/src/subscription.rs`（コメントのみ）、docs（`docs/commands.md` / `docs/errors.md` / `ARCHITECTURE.md` / `CLAUDE.md`）。
- **触らない**: `crates/mat/src/*`（レーン B）、`crates/mat-native/src/op.rs`（レーン B — 例外は Task 4 のテスト 1 件の期待値差し替えのみ）、`crates/mat-controller/src/{case.rs,session.rs,x509.rs}`（レーン D）。
- 各 Task の最後に `task check`（fmt:check + clippy + test）を通してからコミットする。コミットメッセージ末尾に `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` と `Claude-Session: https://claude.ai/code/session_01191m4kryUFk6tD6HjbrBb8` を付ける。
- connectedhomeip XML は scratchpad の sparse clone `$CHIP`（`/tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/3303d6fe-cd2f-4bd4-8dc0-01c078aa92bc/scratchpad/chip`、タグ v1.4.2.0）を使う。無ければ Task 1 Step 1 の手順で取得する。再生成コマンドは常に `python3 scripts/gen-ids.py "$CHIP" > crates/mat-core/src/ids_gen.rs`。
- リポジトリは公開。実 IP・実 node id・実証明書をコミットしない。
- 名前は chip-tool 記法（クラスタ = 小文字英数字、属性/コマンド/フィールド = kebab-case）。

---

## ファイル構成

| ファイル | 責務 |
|---|---|
| `scripts/gen-ids.py` | XML → `ids_gen.rs`。struct 定義（cluster スコープ解決、到達閉包）と `Ty` 記述の生成を追加 |
| `crates/mat-core/src/ids_gen.rs` | 生成物（手編集禁止）。`S_*` static と `Ty::...` を含む |
| `crates/mat-core/src/ids.rs` | 型記述（`TypeTag` / `Ty` / `StructDef` / `StructField`）、値ツリー `ScalarValue`、`parse_value_typed`（リテラル + JSON）、`classify_write` / `classify_invoke` |
| `crates/mat-core/src/parse.rs` | `normalize_value` の JSON 入力対応 |
| `crates/mat-controller/src/im.rs` | `ImValue::F32/F64`、`tlv_to_json` 公開ヘルパ |
| `crates/mat-native/src/lib.rs` | `put_value`（値ツリー → TLV）、`scalar_to_tlv` / `encode_command_fields` / `scalar_to_im`、等価性テスト |
| `crates/mat-native/src/ops.rs` | `encode_acl_entries_tlv` を `pub(crate)` に |
| `crates/matd/src/subscription.rs` | `events_from_report` のコメント更新 |
| docs | 契約の更新 |

---

### Task 1: float write（`F32` / `F64`）

**Files:**
- Modify: `scripts/gen-ids.py`（`BASE_TYPES`、ヘッダ docstring）
- Modify: `crates/mat-core/src/ids.rs`（`TypeTag`、`ScalarValue`、`parse_scalar_typed`、`parse_scalar_inferred`、tests）
- Regenerate: `crates/mat-core/src/ids_gen.rs`
- Modify: `crates/mat-controller/src/im.rs:235-242`（`ImValue`）、`:364-376`（`value_to_im`）、`:2039-2049`（`encode_im_value`）
- Modify: `crates/mat-native/src/lib.rs:106-135`（`scalar_to_im` / `scalar_to_tlv`）、`:141-158`（`encode_command_fields`）
- Modify: `docs/commands.md:1014-1024`、`docs/errors.md:47-51`、`CLAUDE.md`（Backend 節の "Generic write..." 段落）

**Interfaces:**
- Produces: `TypeTag::{F32, F64}`（`Float` は削除）、`ScalarValue::{F32(f32), F64(f64)}`、`ImValue::{F32(f32), F64(f64)}`。`parse_scalar_typed("1.5", TypeTag::F64) == Ok(ScalarValue::F64(1.5))`。`scalar_to_tlv(&ScalarValue::F32(x))` は TLV 要素型 0x0A、`F64` は 0x0B。

- [ ] **Step 1: XML sparse clone があることを確認（無ければ取得）**

```bash
CHIP=/tmp/claude-1000/-home-noguk-ghq-github-com-nogu3-mat/3303d6fe-cd2f-4bd4-8dc0-01c078aa92bc/scratchpad/chip
test -d "$CHIP/src/app/zap-templates/zcl/data-model/chip" || {
  mkdir -p "$(dirname "$CHIP")" && cd "$(dirname "$CHIP")" &&
  git clone -q --depth 1 --branch v1.4.2.0 --filter=blob:none --sparse https://github.com/project-chip/connectedhomeip.git chip &&
  git -C chip sparse-checkout set src/app/zap-templates/zcl/data-model/chip; }
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/write-types
python3 scripts/gen-ids.py "$CHIP" | diff -q - crates/mat-core/src/ids_gen.rs && echo REPRODUCIBLE
```
Expected: `REPRODUCIBLE`（現行スクリプトの出力が checkin 済みと一致）。

- [ ] **Step 2: 失敗するテストを書く（ids.rs）**

`crates/mat-core/src/ids.rs` の tests に追加。既存テスト `parse_scalar_typed_rejects_unsupported_and_bad_literals` の `assert!(parse_scalar_typed("1.5", TypeTag::Float).is_err());` 行は削除する。

```rust
    #[test]
    fn float_attributes_resolve_to_f32_or_f64() {
        // unittesting (0xFFF1FC05): FloatSingle = single → F32, FloatDouble = double → F64。
        let a = resolve_attribute(0xFFF1FC05, "float-single").unwrap();
        assert_eq!(a.def.unwrap().ty, TypeTag::F32);
        let a = resolve_attribute(0xFFF1FC05, "float-double").unwrap();
        assert_eq!(a.def.unwrap().ty, TypeTag::F64);
    }

    #[test]
    fn parse_scalar_typed_floats() {
        use ScalarValue as V;
        assert_eq!(parse_scalar_typed("1.5", TypeTag::F64), Ok(V::F64(1.5)));
        assert_eq!(parse_scalar_typed("-3", TypeTag::F64), Ok(V::F64(-3.0)));
        assert_eq!(parse_scalar_typed("2e-3", TypeTag::F32), Ok(V::F32(2e-3)));
        assert_eq!(parse_scalar_typed("null", TypeTag::F32), Ok(V::Null));
        // nan / inf / 非数値は拒否（TLV には載るがデバイスは CONSTRAINT_ERROR）。
        assert!(parse_scalar_typed("nan", TypeTag::F64).is_err());
        assert!(parse_scalar_typed("inf", TypeTag::F32).is_err());
        assert!(parse_scalar_typed("abc", TypeTag::F64).is_err());
    }

    #[test]
    fn parse_scalar_inferred_float_literal() {
        // 数値 ID 直指定: 小数点 / 指数を含む数値リテラルは F64 に推定する。
        assert_eq!(parse_scalar_inferred("1.5"), ScalarValue::F64(1.5));
        assert_eq!(parse_scalar_inferred("2e3"), ScalarValue::F64(2000.0));
        // 整数はこれまでどおり UInt / Int。
        assert_eq!(parse_scalar_inferred("42"), ScalarValue::UInt(42));
        assert_eq!(parse_scalar_inferred("-1"), ScalarValue::Int(-1));
    }
```

- [ ] **Step 3: 失敗を確認**

Run: `cargo test -p mat-core ids:: 2>&1 | tail -20`
Expected: コンパイルエラー（`TypeTag::F32` / `ScalarValue::F64` が無い）。

- [ ] **Step 4: gen-ids.py の single/double を分離し、ids.rs の型を更新**

`scripts/gen-ids.py`:

```python
BASE_TYPES = {
    "boolean": "Bool",
    "single": "F32", "double": "F64",
    "char_string": "Str", "long_char_string": "Str",
    "octet_string": "Bytes", "long_octet_string": "Bytes",
}
```

docstring の「使い方」の下に取得手順を追記:

```
connectedhomeip の取得（フル clone 不要、data-model XML だけ sparse checkout）:
    git clone --depth 1 --branch v1.4.2.0 --filter=blob:none --sparse \
        https://github.com/project-chip/connectedhomeip.git chip
    git -C chip sparse-checkout set src/app/zap-templates/zcl/data-model/chip
    python3 scripts/gen-ids.py chip > crates/mat-core/src/ids_gen.rs
```

`crates/mat-core/src/ids.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTag {
    Bool,
    UInt,
    Int,
    /// Matter `single`（TLV 0x0A）。
    F32,
    /// Matter `double`（TLV 0x0B）。
    F64,
    Str,
    Bytes,
    List,
    Struct,
    Unknown,
}
```

`ScalarValue` に `F32(f32)` と `F64(f64)` を `Int` の後に追加（doc コメントに「Task 3 で list/struct も持つ値ツリーになる」と書かなくてよい — Task 3 で書く）。

`parse_scalar_typed` の `TypeTag::Float => Err(...)` 腕を置き換え:

```rust
        TypeTag::F32 => parse_finite_f64(s).map(|f| ScalarValue::F32(f as f32)),
        TypeTag::F64 => parse_finite_f64(s).map(ScalarValue::F64),
```

ヘルパ（`parse_hex_bytes` の隣）:

```rust
/// float リテラル（`1.5` / `-3` / `2e-3`）。nan / inf は拒否 — TLV には載るが
/// デバイス側は CONSTRAINT_ERROR にしかならないので早期に parse_error にする。
fn parse_finite_f64(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(f),
        _ => Err(format!("not a finite float literal: {s:?}")),
    }
}
```

`parse_scalar_inferred` の `if let Ok(i) = s.parse::<i64>()` の後、`ScalarValue::Str` に落ちる前に:

```rust
    if (s.contains('.') || s.contains(['e', 'E'])) && !s.starts_with("0x") {
        if let Ok(f) = parse_finite_f64(s) {
            return ScalarValue::F64(f);
        }
    }
```

再生成: `python3 scripts/gen-ids.py "$CHIP" > crates/mat-core/src/ids_gen.rs`。`git diff --stat crates/mat-core/src/ids_gen.rs` で変更が `TypeTag::Float` → `F32`/`F64` の行（65 行）だけであることを確認する。

- [ ] **Step 5: im.rs と mat-native lib.rs の float 腕**

`crates/mat-controller/src/im.rs` `ImValue` に `F32(f32)` / `F64(f64)` を `Int` の後に追加し、doc コメントの「Containers are not supported」はそのまま。`value_to_im`:

```rust
        Value::F32(f) => Ok(ImValue::F32(f)),
        Value::F64(f) => Ok(ImValue::F64(f)),
```
（`ImValue has no float variant` のコメント行は削除。）`encode_im_value`:

```rust
        ImValue::F32(f) => w.put_f32(Tag::Anonymous, *f),
        ImValue::F64(f) => w.put_f64(Tag::Anonymous, *f),
```

`crates/mat-native/src/lib.rs` の `scalar_to_im` / `scalar_to_tlv` / `encode_command_fields` にそれぞれ:

```rust
        S::F32(f) => ImValue::F32(*f),
        S::F64(f) => ImValue::F64(*f),
```
```rust
        S::F32(f) => w.put_f32(tag_or_anonymous, *f),
        S::F64(f) => w.put_f64(tag_or_anonymous, *f),
```

im.rs のテスト（`mod tests` 末尾）:

```rust
    #[test]
    fn im_value_floats_roundtrip_through_encode_and_decode() {
        for v in [ImValue::F32(1.5), ImValue::F64(-2.25)] {
            let tlv = encode_im_value(&v);
            // 要素型: single = 0x0A, double = 0x0B（anonymous tag → control byte だけ）。
            let expect = if matches!(v, ImValue::F32(_)) { 0x0A } else { 0x0B };
            assert_eq!(tlv[0] & 0x1F, expect, "{v:?}");
            let mut r = Reader::new(&tlv);
            let el = r.next().unwrap().unwrap();
            assert_eq!(value_to_im(el.value).unwrap(), v);
        }
    }
```
（`Reader` が tests スコープに無ければ `use crate::tlv::Reader;` を足す。`value_to_im` のシグネチャが `Value` を値で受けるか参照かはファイルを確認して合わせる。）

mat-native lib.rs の既存テスト `scalar_conversions` に追加:

```rust
        let b = scalar_to_tlv(&S::F64(0.5));
        let mut r = mat_controller::tlv::Reader::new(&b);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            mat_controller::tlv::Value::F64(f) if f == 0.5
        ));
```

- [ ] **Step 6: テストと check**

Run: `cargo test -p mat-core -p mat-controller -p mat-native 2>&1 | grep -E "^test result|FAILED|panicked" | head`
Expected: 全 `ok`。
Run: `task check 2>&1 | tail -5`
Expected: 成功。

- [ ] **Step 7: docs の float 記述**

- `docs/commands.md` の「Scalar-only generic write / invoke」段落: `bool / int / uint / enum / bitmap / string / octstr` に `/ float (single/double, decimal literal like 1.5)` を足し、「known to be `list` / `struct` / `float`」を「known to be `list` / `struct`」にする。
- `docs/errors.md:48` の `` `list` / `struct` / `float` `` → `` `list` / `struct` ``。
- `CLAUDE.md` Backend 節: `(bool/int/uint/enum/bitmap/string/octstr, bytes as \`hex:\`)` → `(bool/int/uint/enum/bitmap/float/string/octstr, bytes as \`hex:\`)`、`` `list`/`struct`/`float` fields `` → `` `list`/`struct` fields ``。

- [ ] **Step 8: コミット**

```bash
git add scripts/gen-ids.py crates/mat-core/src/ids.rs crates/mat-core/src/ids_gen.rs crates/mat-controller/src/im.rs crates/mat-native/src/lib.rs docs/commands.md docs/errors.md CLAUDE.md
git commit -m "feat(write): float 属性の write 対応 — single/double を F32/F64 に分離、TLV 0x0A/0x0B で符号化（レーン C 1/3）"
```

---

### Task 2: struct スキーマ生成（`gen-ids.py` + `Ty`）

**Files:**
- Modify: `scripts/gen-ids.py`（struct 収集・解決・到達閉包・emit）
- Modify: `crates/mat-core/src/ids.rs`（`Ty` / `StructDef` / `StructField`、`AttrDef.ty` / `FieldDef.ty` を `Ty` に、`parse_scalar_typed` → `parse_value_typed` の骨組み、tests）
- Regenerate: `crates/mat-core/src/ids_gen.rs`

**Interfaces:**
- Consumes: Task 1 の `TypeTag::{F32,F64}`。
- Produces:
  ```rust
  pub enum TypeTag { Bool, UInt, Int, F32, F64, Str, Bytes, Unknown }   // List / Struct 削除
  #[derive(Clone, Copy)] pub enum Ty { Scalar(TypeTag), Struct(&'static StructDef), List(TypeTag), ListOfStruct(&'static StructDef) }
  pub struct StructDef { pub name: &'static str, pub fields: &'static [StructField] }
  pub struct StructField { pub name: &'static str, pub id: u8, pub ty: Ty, pub optional: bool }
  pub struct AttrDef { name, id, ty: Ty, writable, timed_write }
  pub struct FieldDef { name, ty: Ty, optional }
  pub fn parse_value_typed(input: &str, ty: &Ty) -> Result<ScalarValue, String>  // この Task では Scalar のみ実装、他は Err
  ```
  fabric-scoped struct（XML `isFabricScoped="true"`）には生成側で `StructField { name: "fabric-index", id: 254, ty: Ty::Scalar(TypeTag::UInt), optional: true }` を末尾に付ける。

- [ ] **Step 1: 失敗するテストを書く（ids.rs）**

```rust
    #[test]
    fn struct_schema_is_generated_for_acl_and_group_key_map() {
        // acl = list of AccessControlEntryStruct（cluster 0x001F スコープ）。
        let a = resolve_attribute(0x001F, "acl").unwrap();
        let Ty::ListOfStruct(entry) = a.def.unwrap().ty else {
            panic!("acl should be ListOfStruct, got {:?}", a.def.unwrap().ty);
        };
        assert_eq!(entry.name, "AccessControlEntryStruct");
        let names: Vec<&str> = entry.fields.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            ["privilege", "auth-mode", "subjects", "targets", "fabric-index"]
        );
        let subjects = &entry.fields[2];
        assert_eq!(subjects.id, 3);
        assert!(matches!(subjects.ty, Ty::List(TypeTag::UInt)));
        let targets = &entry.fields[3];
        assert_eq!(targets.id, 4);
        let Ty::ListOfStruct(t) = targets.ty else { panic!("targets") };
        assert_eq!(t.name, "AccessControlTargetStruct");
        assert_eq!(t.fields.len(), 3); // Cluster / Endpoint / DeviceType（fabric-scoped ではない）
        // fabric-index は生成側で付けた暗黙 optional フィールド（read 出力の "254" を書き戻せる）。
        let fi = &entry.fields[4];
        assert_eq!((fi.id, fi.optional), (254, true));
        assert!(matches!(fi.ty, Ty::Scalar(TypeTag::UInt)));

        let a = resolve_attribute(0x003F, "group-key-map").unwrap();
        let Ty::ListOfStruct(m) = a.def.unwrap().ty else { panic!("group-key-map") };
        assert_eq!(m.name, "GroupKeyMapStruct");
        assert_eq!(m.fields[0].name, "group-id");
        assert_eq!(m.fields[1].name, "group-key-set-id");
        assert_eq!(m.fields[1].id, 2);
    }

    #[test]
    fn command_struct_args_and_scalar_lists_are_typed() {
        let c = resolve_command(0x003F, "key-set-write").unwrap();
        let Ty::Struct(ks) = c.def.unwrap().fields[0].ty else { panic!("key-set-write arg0") };
        assert_eq!(ks.name, "GroupKeySetStruct");
        assert_eq!(ks.fields.len(), 8);
        assert!(matches!(ks.fields[2].ty, Ty::Scalar(TypeTag::Bytes))); // EpochKey0
        // スカラー list 属性: descriptor server-list = list<cluster_id>。
        let a = resolve_attribute(0x001D, "server-list").unwrap();
        assert!(matches!(a.def.unwrap().ty, Ty::List(TypeTag::UInt)));
        // 同名 struct のクラスタスコープ解決: modeselect の supported-modes は
        // modeselect 自身の ModeOptionStruct（Label/Mode/SemanticTags）。
        let a = resolve_attribute(0x0050, "supported-modes").unwrap();
        let Ty::ListOfStruct(mo) = a.def.unwrap().ty else { panic!("supported-modes") };
        assert_eq!(mo.fields[0].name, "label");
        assert_eq!(mo.fields[1].name, "mode");
    }

    #[test]
    fn global_attributes_keep_scalar_list_shape() {
        let a = resolve_attribute(0x0006, "attribute-list").unwrap();
        assert!(matches!(a.def.unwrap().ty, Ty::List(TypeTag::UInt)));
        let a = resolve_attribute(0x0006, "feature-map").unwrap();
        assert!(matches!(a.def.unwrap().ty, Ty::Scalar(TypeTag::UInt)));
    }
```

既存テストの `TypeTag::List` / `TypeTag::Struct` 比較（`resolves_known_attributes_with_types` の `neighbor-table` / `acl` / `group-key-map` / `attribute-list`、`resolves_known_commands_with_fields` の `key-set-write`、`global_attributes_resolve_on_every_cluster`）は `matches!(ty, Ty::ListOfStruct(_))` / `Ty::List(_)` / `Ty::Struct(_)` に書き換える。`TypeTag::Bool` 等の比較は `Ty::Scalar(TypeTag::Bool)` に。`parse_scalar_typed` を呼ぶ既存テストは `parse_value_typed(x, &Ty::Scalar(tag))` に書き換え、`TypeTag::List` / `Struct` を渡していた assert（`"[]"`, `"{}"`）は `&Ty::List(TypeTag::UInt)` / `&Ty::Struct(&DUMMY)` にする。`DUMMY` は tests 内に:

```rust
    static DUMMY: StructDef = StructDef { name: "Dummy", fields: &[] };
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-core ids:: 2>&1 | tail -5`
Expected: コンパイルエラー（`Ty` が無い）。

- [ ] **Step 3: ids.rs の型記述を実装**

```rust
/// スカラー型。list / struct の形は `Ty` が持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTag {
    Bool,
    UInt,
    Int,
    /// Matter `single`（TLV 0x0A）。
    F32,
    /// Matter `double`（TLV 0x0B）。
    F64,
    Str,
    Bytes,
    /// 生成テーブルが型を解決できなかった（符号化は拒否）。
    Unknown,
}

/// 属性 / コマンド引数 / struct フィールドに共通の型記述。
#[derive(Clone, Copy)]
pub enum Ty {
    Scalar(TypeTag),
    Struct(&'static StructDef),
    /// スカラー要素の list（Matter の `array`、TLV Array）。
    List(TypeTag),
    ListOfStruct(&'static StructDef),
}

impl std::fmt::Debug for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Scalar(t) => write!(f, "Scalar({t:?})"),
            Ty::Struct(d) => write!(f, "Struct({})", d.name),
            Ty::List(t) => write!(f, "List({t:?})"),
            Ty::ListOfStruct(d) => write!(f, "ListOfStruct({})", d.name),
        }
    }
}

impl Ty {
    /// 人間可読の型名（parse_error detail 用）。
    pub fn describe(&self) -> String {
        format!("{self:?}").to_lowercase()
    }
}

/// struct 型の定義（ids_gen.rs の `S_*` static）。`fields` は fieldId 昇順。
pub struct StructDef {
    pub name: &'static str,
    pub fields: &'static [StructField],
}
/// struct のフィールド。`id` は TLV context tag。fabric-scoped struct には生成側が
/// `fabric-index`(254, optional) を末尾に付ける — read 出力の `"254"` をそのまま
/// 書き戻せるようにするため（サーバは書込時にこの値を無視・置換する）。
pub struct StructField {
    pub name: &'static str,
    pub id: u8,
    pub ty: Ty,
    pub optional: bool,
}
```

`AttrDef.ty: Ty`、`FieldDef.ty: Ty` に変更。`parse_scalar_typed` を改名して骨組みに:

```rust
/// 型記述に従って CLI 入力文字列を値へ。Err は人間可読の理由（そのまま
/// parse_error detail に使える）。スカラーはリテラル構文、list / struct は
/// JSON（Task 3 で実装 — 本 Task では Err）。
pub fn parse_value_typed(input: &str, ty: &Ty) -> Result<ScalarValue, String> {
    let s = input.trim();
    match ty {
        Ty::Scalar(tag) => parse_scalar_literal(s, *tag),
        other => Err(format!(
            "this attribute is a {} type; generic native write supports scalars only (M8a)",
            other.describe()
        )),
    }
}

fn parse_scalar_literal(s: &str, ty: TypeTag) -> Result<ScalarValue, String> {
    if s == "null" {
        return Ok(ScalarValue::Null);
    }
    match ty {
        // 以下は旧 `parse_scalar_typed` の腕をそのまま移す（Bool: true/false/1/0、
        // UInt: parse_num、Int: i64、Str: 生文字列、Bytes: parse_hex_bytes、
        // F32/F64: parse_finite_f64、Unknown: Err）。List / Struct 腕は削除。
        TypeTag::Bool => match s {
            "true" | "1" => Ok(ScalarValue::Bool(true)),
            "false" | "0" => Ok(ScalarValue::Bool(false)),
            _ => Err(format!("not a bool literal: {s:?}")),
        },
        TypeTag::UInt => parse_num(s)
            .map(ScalarValue::UInt)
            .ok_or(format!("not an unsigned integer: {s:?}")),
        TypeTag::Int => s
            .parse::<i64>()
            .map(ScalarValue::Int)
            .map_err(|_| format!("not an integer: {s:?}")),
        TypeTag::F32 => parse_finite_f64(s).map(|f| ScalarValue::F32(f as f32)),
        TypeTag::F64 => parse_finite_f64(s).map(ScalarValue::F64),
        TypeTag::Str => Ok(ScalarValue::Str(s.to_string())),
        TypeTag::Bytes => parse_hex_bytes(s).map(ScalarValue::Bytes),
        TypeTag::Unknown => Err("attribute type unknown; cannot encode value".into()),
    }
}
```

`classify_write` は `parse_value_typed(value, &def.ty)`、`classify_invoke` は `parse_value_typed(arg, &def.fields[i].ty)` に。

- [ ] **Step 4: gen-ids.py に struct 生成を実装**

`parse_files` を拡張して struct 要素も集める:

```python
def parse_files(root_dir: str):
    ...
    structs = {}   # (cluster_id or None, name) -> {"name", "fabric_scoped", "items": [(fieldId, name, type, is_array, optional)]}
    for f in files:
        tree = ET.parse(f)
        for e in tree.getroot().iter("enum"): enums.add(e.get("name", ""))
        for e in tree.getroot().iter("bitmap"): bitmaps.add(e.get("name", ""))
        for s in tree.getroot().iter("struct"):
            name = s.get("name", "")
            items = []
            for idx, it in enumerate(s.findall("item")):
                # fieldId 無しの struct（unittesting の NullablesAndOptionalsStruct 等 170
                # 項目、混在は無い）は zap の慣例どおり出現順 = fieldId。
                fid = int(it.get("fieldId"), 0) if it.get("fieldId") is not None else idx
                items.append((fid, it.get("name", ""), it.get("type", ""),
                              it.get("array", "false") == "true", it.get("optional", "false") == "true"))
            info = {"name": name, "fabric_scoped": s.get("isFabricScoped", "false") == "true",
                    "items": sorted(items)}
            codes = [int(c.get("code"), 0) for c in s.findall("cluster")]
            for key in ([(c, name) for c in codes] or [(None, name)]):
                structs.setdefault(key, info)   # 先勝ち
        ...
    return cluster_elems, global_elems, enums, bitmaps, structs
```

型解決（`type_tag` を置き換え）:

```python
SCALAR_OF = {"boolean": "Bool", "single": "F32", "double": "F64",
             "char_string": "Str", "long_char_string": "Str",
             "octet_string": "Bytes", "long_octet_string": "Bytes"}

def scalar_tag(ty: str, enums, bitmaps) -> str:
    tl = ty.strip().lower()
    if tl in SCALAR_OF: return SCALAR_OF[tl]
    if re.fullmatch(r"int\d+u", tl) or re.fullmatch(r"enum\d+", tl) or re.fullmatch(r"bitmap\d+", tl): return "UInt"
    if re.fullmatch(r"int\d+s?", tl): return "Int" if tl.endswith("s") else "UInt"
    if ty in enums or ty in bitmaps: return "UInt"
    return "UInt"   # zap 派生型（epoch_s, node_id, percent ...）は符号なし整数ベース

def resolve_struct(structs, cluster_id, name):
    return (cluster_id, name) if (cluster_id, name) in structs else ((None, name) if (None, name) in structs else None)

def ty_of(cluster_id, ty: str, is_array: bool, entry: str | None, enums, bitmaps, structs, used: set):
    """戻り値は Rust の `Ty::...` 式。到達した struct キーを used に積む。"""
    elem = (entry or ty).strip()
    skey = resolve_struct(structs, cluster_id, elem)
    if skey is None and "struct" in elem.lower():
        tag = "Unknown"          # 名前は struct 風だが定義が無い
    elif skey is None:
        tag = scalar_tag(elem, enums, bitmaps)
    else:
        used.add(skey)
        ref = "&" + static_name(skey)
        return f"Ty::ListOfStruct({ref})" if (is_array or entry) else f"Ty::Struct({ref})"
    return f"Ty::List(TypeTag::{tag})" if (is_array or entry) else f"Ty::Scalar(TypeTag::{tag})"
```

`static_name(key)`: `S_GLOBAL_<NAME>` または `S_<CLUSTERKEY>_<NAME>`（NAME は struct 名を `re.sub(r"[^A-Za-z0-9]", "", name).upper()`、CLUSTERKEY は cluster id → `cluster_key(name)` の upper。cluster id → key の辞書は main で `cluster_elems` から作り、`ty_of` からも見えるようにモジュール変数 `CLUSTER_KEY_OF_ID` に置く）。cluster id がどのクラスタ要素にも無い struct キーは `S_C<hex>_<NAME>` にする。

属性・コマンド引数の tuple は `(name, id, ty_expr, writable, timed)` / `(name, ty_expr, optional)` に変わる（`tag` 文字列の代わりに `Ty::...` 式）。global 属性の `List` は `Ty::List(TypeTag::UInt)`（AttributeList 等は id の list）。

到達閉包: `main` で全 attrs / cmd fields の `ty_of` 呼び出しで溜まった `used` を起点に、struct の items を `ty_of` で辿って固定点まで回す（items の `ty_of` にも同じ `used` を渡す）。struct 側の cluster_id は **struct キーの cluster_id**（None のときは None）。

emit（`CLUSTERS` の前に）:

```python
    for key in sorted(used, key=static_name):
        info = structs[key]
        print(f"static {static_name(key)}: StructDef = StructDef {{ name: \"{info['name']}\", fields: &[")
        for (fid, fname, fty, is_array, optional) in info["items"]:
            expr = ty_of(key[0], fty, is_array, None, enums, bitmaps, structs, used)
            print(f'    StructField {{ name: "{kebab(fname)}", id: {fid}, ty: {expr}, optional: {str(optional).lower()} }},')
        if info["fabric_scoped"]:
            print('    StructField { name: "fabric-index", id: 254, ty: Ty::Scalar(TypeTag::UInt), optional: true },')
        print("] };")
```

`use super::ids::{AttrDef, ClusterDef, CmdDef, FieldDef, StructDef, StructField, Ty, TypeTag};` に更新。

再生成して `cargo build -p mat-core` が通ることを確認。`static` 同士の前方参照は Rust が許すので順序は気にしない。

- [ ] **Step 5: テストと check**

Run: `cargo test -p mat-core 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: 全 `ok`。
Run: `task check 2>&1 | tail -3`
Expected: 成功（mat-native の `op.rs` テストは `"[]"` が引き続き Reject なので変化なし）。
Run: `wc -l crates/mat-core/src/ids_gen.rs`（参考値として記録。struct 群で 1,000〜2,000 行増える想定）。

- [ ] **Step 6: コミット**

```bash
git add scripts/gen-ids.py crates/mat-core/src/ids.rs crates/mat-core/src/ids_gen.rs
git commit -m "feat(ids): struct スキーマを生成 — Ty(Scalar/Struct/List/ListOfStruct) + StructDef、fabric-index 暗黙フィールド（レーン C 2/3）"
```

---

### Task 3: list / struct の JSON → 値ツリー（パーサ）

**Files:**
- Modify: `crates/mat-core/src/ids.rs`（`ScalarValue::{List,Struct}`、`parse_value_typed` の JSON 経路、`classify_write` / `classify_invoke` の数値 ID 腕、tests）

**Interfaces:**
- Consumes: Task 2 の `Ty` / `StructDef` / `StructField`。
- Produces: `ScalarValue::List(Vec<ScalarValue>)`、`ScalarValue::Struct(Vec<(u8, ScalarValue)>)`（id 昇順）。`parse_value_typed(json, &Ty::ListOfStruct(def))` が JSON を検査して値ツリーに。`classify_write("accesscontrol","acl","[...]")` が `Native`。

- [ ] **Step 1: 失敗するテストを書く**

```rust
    fn acl_ty() -> Ty {
        resolve_attribute(0x001F, "acl").unwrap().def.unwrap().ty
    }

    #[test]
    fn parse_value_typed_struct_list_by_name_and_by_id() {
        use ScalarValue as V;
        // 名前キー。
        let v = parse_value_typed(
            r#"[{"privilege":5,"auth-mode":2,"subjects":[112233,"0x1122"],"targets":null}]"#,
            &acl_ty(),
        )
        .unwrap();
        let expect = V::List(vec![V::Struct(vec![
            (1, V::UInt(5)),
            (2, V::UInt(2)),
            (3, V::List(vec![V::UInt(112233), V::UInt(0x1122)])),
            (4, V::Null),
        ])]);
        assert_eq!(v, expect);
        // 番号キー（read 出力の形、fabric-index 254 込み）でも同じ — 順序は id 昇順に整列。
        let v = parse_value_typed(
            r#"[{"4":null,"254":1,"3":[112233,4386],"2":2,"1":5}]"#,
            &acl_ty(),
        )
        .unwrap();
        let V::List(items) = &v else { panic!() };
        let V::Struct(fields) = &items[0] else { panic!() };
        let ids: Vec<u8> = fields.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [1, 2, 3, 4, 254]);
        // targets の struct 要素（nullable スカラー）。
        let v = parse_value_typed(
            r#"[{"privilege":3,"auth-mode":2,"subjects":[7],"targets":[{"cluster":6,"endpoint":null,"device-type":null}]}]"#,
            &acl_ty(),
        )
        .unwrap();
        let V::List(items) = &v else { panic!() };
        let V::Struct(fields) = &items[0] else { panic!() };
        assert_eq!(
            fields[3].1,
            V::List(vec![V::Struct(vec![(0, V::UInt(6)), (1, V::Null), (2, V::Null)])])
        );
    }

    #[test]
    fn parse_value_typed_bytes_accept_both_hex_forms_inside_json() {
        use ScalarValue as V;
        let ks = resolve_command(0x003F, "key-set-write").unwrap().def.unwrap().fields[0].ty;
        let j = r#"{"group-key-set-id":1,"group-key-security-policy":0,"epoch-key0":"hex:00112233445566778899aabbccddeeff","epoch-start-time0":1,"epoch-key1":null,"epoch-start-time1":null,"epoch-key2":"00112233445566778899aabbccddeeff","epoch-start-time2":null}"#;
        let V::Struct(f) = parse_value_typed(j, &ks).unwrap() else { panic!() };
        assert_eq!(f[2].1, V::Bytes(vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
        assert_eq!(f[6].1, f[2].1);
    }

    #[test]
    fn parse_value_typed_scalar_list_and_top_level_null() {
        use ScalarValue as V;
        let t = Ty::List(TypeTag::UInt);
        assert_eq!(parse_value_typed("[1, 2, 3]", &t), Ok(V::List(vec![V::UInt(1), V::UInt(2), V::UInt(3)])));
        assert_eq!(parse_value_typed("[]", &t), Ok(V::List(vec![])));
        assert_eq!(parse_value_typed("null", &t), Ok(V::Null));
    }

    #[test]
    fn parse_value_typed_rejects_bad_json_shapes() {
        let t = acl_ty();
        let err = |s: &str| parse_value_typed(s, &t).unwrap_err();
        assert!(err("{}").contains("array"), "struct given for list");
        assert!(err("not json").contains("JSON"));
        assert!(err(r#"[{"privilege":5,"auth-mode":2,"subjects":[],"targets":null,"bogus":1}]"#).contains("bogus"));
        assert!(err(r#"[{"privilege":5,"auth-mode":2,"subjects":[]}]"#).contains("targets"), "missing required");
        assert!(err(r#"[{"privilege":5,"1":5,"auth-mode":2,"subjects":[],"targets":null}]"#).contains("twice"));
        assert!(err(r#"[{"privilege":-1,"auth-mode":2,"subjects":[],"targets":null}]"#).contains("unsigned"));
        assert!(err(r#"[{"privilege":"five","auth-mode":2,"subjects":[],"targets":null}]"#).contains("unsigned"));
    }

    #[test]
    fn parse_value_typed_optional_fields_may_be_omitted_but_required_may_not() {
        // unittesting list-nullables-and-optionals-struct: NullableInt(0) は必須
        // (nullable だが optional ではない)、OptionalInt(1) / NullableOptionalInt(2)
        // 等は optional。fieldId 無しの struct なので id は出現順。
        let ty = resolve_attribute(0xFFF1FC05, "list-nullables-and-optionals-struct")
            .unwrap()
            .def
            .unwrap()
            .ty;
        let Ty::ListOfStruct(def) = ty else { panic!("{ty:?}") };
        assert_eq!(def.fields[0].name, "nullable-int");
        assert_eq!((def.fields[0].id, def.fields[0].optional), (0, false));
        assert_eq!((def.fields[1].id, def.fields[1].optional), (1, true));
        let ok = r#"[{"nullable-int":null,"nullable-string":null,"nullable-struct":null,"nullable-list":null}]"#;
        assert!(parse_value_typed(ok, &ty).is_ok(), "{:?}", parse_value_typed(ok, &ty));
        let err = parse_value_typed("[{}]", &ty).unwrap_err();
        assert!(err.contains("nullable-int"), "{err}");
    }

    #[test]
    fn classify_write_accepts_list_and_rejects_numeric_id_containers() {
        match classify_write("accesscontrol", "acl", "[]") {
            WriteClass::Native { cluster: 0x001F, attribute: 0, value: ScalarValue::List(v), timed: false } => assert!(v.is_empty()),
            other => panic!("{other:?}"),
        }
        match classify_write("groupkeymanagement", "group-key-map", r#"[{"group-id":1,"group-key-set-id":2}]"#) {
            WriteClass::Native { value: ScalarValue::List(v), .. } => assert_eq!(v.len(), 1),
            other => panic!("{other:?}"),
        }
        // 数値 ID 直指定には型情報が無いので list/struct は Reject（従来は Str で送っていた）。
        match classify_write("0x1F", "0", "[]") {
            WriteClass::Reject(msg) => assert!(msg.contains("numeric"), "{msg}"),
            other => panic!("{other:?}"),
        }
        match classify_invoke("0x3F", "0", &["{}".into()]) {
            InvokeClass::Reject(msg) => assert!(msg.contains("numeric"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn classify_invoke_accepts_struct_arg() {
        let j = r#"{"group-key-set-id":1,"group-key-security-policy":0,"epoch-key0":"hex:00112233445566778899aabbccddeeff","epoch-start-time0":1,"epoch-key1":null,"epoch-start-time1":null,"epoch-key2":null,"epoch-start-time2":null}"#;
        match classify_invoke("groupkeymanagement", "key-set-write", &[j.into()]) {
            InvokeClass::Native { fields, .. } => assert!(matches!(fields[0], ScalarValue::Struct(_))),
            other => panic!("{other:?}"),
        }
        // 必須欠落は依然 Reject（op.rs の既存テストが "{}" の Reject を期待している）。
        assert!(matches!(
            classify_invoke("groupkeymanagement", "key-set-write", &["{}".into()]),
            InvokeClass::Reject(_)
        ));
    }
```

既存テスト `classify_write_rejects_list_type_with_parse_error_message`（`acl` に `[]` → Reject）は削除する。`parse_scalar_typed_rejects_unsupported_and_bad_literals` の `"[]"` / `"{}"` を拒否する assert は「不正な JSON 形」（`Ty::List(TypeTag::UInt)` に `"{}"`）に置き換える。`classify_invoke_rejects_too_many_or_non_scalar_args` の key-set-write `{}` は Reject のまま（必須欠落）なので残す。

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-core ids:: 2>&1 | tail -5`
Expected: コンパイルエラー（`ScalarValue::List` が無い）。

- [ ] **Step 3: 実装**

`ScalarValue`:

```rust
/// write / invoke 引数の値ツリー。名前は歴史的に `ScalarValue` だが list /
/// struct も持つ（改名は mat-native::op と同時に行う — 直列後回しの可読性分割）。
/// mat-controller の ImValue と同形のスカラー + container。mat-core は
/// mat-controller に依存できないため別型で持ち、mat-native 側で TLV に写す。
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    Bool(bool),
    UInt(u64),
    Int(i64),
    F32(f32),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    Null,
    /// TLV Array。要素は全て同じ型記述。
    List(Vec<ScalarValue>),
    /// TLV Struct。(context tag = fieldId, 値)、id 昇順に整列済み。
    Struct(Vec<(u8, ScalarValue)>),
}
```

`parse_value_typed` の非スカラー腕:

```rust
        other => {
            let json: serde_json::Value = serde_json::from_str(s).map_err(|e| {
                format!("{} value must be JSON: {e}", other.describe())
            })?;
            json_to_value(&json, other, "value")
        }
```

JSON → 値ツリー:

```rust
/// JSON を型記述で検査しつつ値ツリーへ。`at` はエラー位置（`value[0].targets` 等）。
fn json_to_value(j: &serde_json::Value, ty: &Ty, at: &str) -> Result<ScalarValue, String> {
    if j.is_null() {
        return Ok(ScalarValue::Null); // nullable かどうかはデバイスが検証する。
    }
    match *ty {
        Ty::Scalar(tag) => json_scalar(j, tag, at),
        Ty::List(elem) => json_list(j, &Ty::Scalar(elem), at),
        Ty::ListOfStruct(def) => json_list(j, &Ty::Struct(def), at),
        Ty::Struct(def) => {
            let obj = j
                .as_object()
                .ok_or_else(|| format!("{at}: expected a JSON object ({})", def.name))?;
            json_struct(obj, def, at)
        }
    }
}

fn json_list(j: &serde_json::Value, elem: &Ty, at: &str) -> Result<ScalarValue, String> {
    let arr = j
        .as_array()
        .ok_or_else(|| format!("{at}: expected a JSON array (list of {})", elem.describe()))?;
    arr.iter()
        .enumerate()
        .map(|(i, e)| json_to_value(e, elem, &format!("{at}[{i}]")))
        .collect::<Result<Vec<_>, _>>()
        .map(ScalarValue::List)
}

fn json_scalar(j: &serde_json::Value, tag: TypeTag, at: &str) -> Result<ScalarValue, String> {
    use serde_json::Value as J;
    let bad = |want: &str| format!("{at}: expected {want}, got {j}");
    match tag {
        TypeTag::Bool => j.as_bool().map(ScalarValue::Bool).ok_or_else(|| bad("a bool")),
        TypeTag::UInt => match j {
            J::Number(n) => n.as_u64().map(ScalarValue::UInt),
            J::String(s) => parse_num(s).map(ScalarValue::UInt),
            _ => None,
        }
        .ok_or_else(|| bad("an unsigned integer (number or \"0x..\" string)")),
        TypeTag::Int => match j {
            J::Number(n) => n.as_i64().map(ScalarValue::Int),
            J::String(s) => s.trim().parse::<i64>().ok().map(ScalarValue::Int),
            _ => None,
        }
        .ok_or_else(|| bad("an integer")),
        TypeTag::F32 => j.as_f64().map(|f| ScalarValue::F32(f as f32)).ok_or_else(|| bad("a number")),
        TypeTag::F64 => j.as_f64().map(ScalarValue::F64).ok_or_else(|| bad("a number")),
        TypeTag::Str => j.as_str().map(|s| ScalarValue::Str(s.to_string())).ok_or_else(|| bad("a string")),
        TypeTag::Bytes => {
            let s = j.as_str().ok_or_else(|| bad("a hex string"))?;
            let h = s.strip_prefix("hex:").unwrap_or(s);
            parse_hex_bytes(&format!("hex:{h}")).map(ScalarValue::Bytes).map_err(|e| format!("{at}: {e}"))
        }
        TypeTag::Unknown => Err(format!("{at}: type unknown; cannot encode value")),
    }
}

fn json_struct(
    obj: &serde_json::Map<String, serde_json::Value>,
    def: &StructDef,
    at: &str,
) -> Result<ScalarValue, String> {
    let mut out: Vec<(u8, ScalarValue)> = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        let field = def
            .fields
            .iter()
            .find(|f| f.name == k)
            .or_else(|| k.parse::<u8>().ok().and_then(|id| def.fields.iter().find(|f| f.id == id)))
            .ok_or_else(|| {
                let valid: Vec<&str> = def.fields.iter().map(|f| f.name).collect();
                format!("{at}: unknown field {k:?} of {}; valid fields: {}", def.name, valid.join(", "))
            })?;
        if out.iter().any(|(id, _)| *id == field.id) {
            return Err(format!("{at}: field {:?} given twice (by name and by id)", field.name));
        }
        out.push((field.id, json_to_value(v, &field.ty, &format!("{at}.{}", field.name))?));
    }
    for f in def.fields {
        if !f.optional && !out.iter().any(|(id, _)| *id == f.id) {
            return Err(format!("{at}: missing required field {:?} of {}", f.name, def.name));
        }
    }
    out.sort_by_key(|(id, _)| *id);
    Ok(ScalarValue::Struct(out))
}
```

`classify_write` の `None =>` 腕と `classify_invoke` の `None =>` 腕: 値が `[` / `{` で始まるなら Reject:

```rust
/// 数値 ID 直指定には型情報が無い — list/struct 風の値は符号化できないので拒否。
fn reject_container_literal(what: &str, v: &str) -> Option<String> {
    let t = v.trim();
    (t.starts_with('[') || t.starts_with('{')).then(|| {
        format!("{what}: list/struct values need a cluster/attribute name the table knows (numeric ids carry no schema)")
    })
}
```

`classify_write`: `None => match reject_container_literal(&format!("write {cluster}/{attribute}"), value) { Some(m) => return WriteClass::Reject(m), None => Ok(parse_scalar_inferred(value)) }`。`classify_invoke` の None 腕: 引数を走査して最初の container 風で `InvokeClass::Reject`。

- [ ] **Step 4: テストと check**

Run: `cargo test -p mat-core 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: 全 `ok`。
Run: `task check 2>&1 | tail -3`
Expected: **mat-native の `op.rs` テスト `write_scalar_ok_list_rejected_unknown_unresolved` が失敗する**（`acl` に `[]` が Native になったため）。これは Task 4 で期待値を差し替える。それ以外は緑であること。mat-native がコンパイルできない（`scalar_to_tlv` の match 網羅）場合は、Task 4 に進む前にこの Task で `lib.rs` の match に `S::List(_) | S::Struct(_) => unreachable!()` を **置かず**、Task 4 と同じ `put_value` を先に入れてよい（その場合 Task 4 のコミットにまとめる）。

- [ ] **Step 5: コミット（mat-core のみ）**

```bash
git add crates/mat-core/src/ids.rs
git commit -m "feat(ids): list/struct 値の JSON → 値ツリー（名前/番号キー、必須検査、数値 ID は拒否）（レーン C 2/3）"
```

---

### Task 4: 値ツリー → TLV と等価性テスト

**Files:**
- Modify: `crates/mat-native/src/lib.rs:106-158`（`put_value` / `scalar_to_tlv` / `encode_command_fields` / `scalar_to_im`、tests）
- Modify: `crates/mat-native/src/ops.rs:369`（`fn encode_acl_entries_tlv` → `pub(crate) fn`）
- Modify: `crates/mat-controller/src/im.rs`（`pub fn tlv_to_json`）
- Modify: `crates/mat-native/src/op.rs:699-701`（テスト期待値のみ）
- Modify: `crates/mat-core/src/parse.rs:28`（`normalize_value`）

**Interfaces:**
- Consumes: `ScalarValue::{List,Struct,F32,F64}`、`parse_value_typed`、`resolve_attribute` / `resolve_command`、`ops::encode_acl_entries_tlv(&[AclEntry]) -> Vec<u8>`、`im::encode_group_key_map_tlv(&[(u16,u16)])`、`im::encode_key_set_write_fields(u16, &[u8;16])`。
- Produces: `mat_native::put_value(w: &mut Writer, tag: Tag, v: &ScalarValue)`、`mat_controller::im::tlv_to_json(tlv: &[u8]) -> Result<serde_json::Value, ImError>`、`scalar_to_im(&ScalarValue) -> Option<ImValue>`。

- [ ] **Step 1: 失敗するテストを書く（mat-native lib.rs tests）**

```rust
    #[test]
    fn put_value_encodes_list_of_struct_as_tlv_array_and_roundtrips_to_read_json() {
        use mat_core::ids::{parse_value_typed, resolve_attribute};
        let ty = resolve_attribute(0x001F, "acl").unwrap().def.unwrap().ty;
        let v = parse_value_typed(
            r#"[{"privilege":5,"auth-mode":2,"subjects":[112233],"targets":null,"fabric-index":1}]"#,
            &ty,
        )
        .unwrap();
        let tlv = scalar_to_tlv(&v);
        // 先頭要素は TLV Array（0x16、anonymous）。
        assert_eq!(tlv[0], 0x16);
        // read 側の JSON 化（番号キー）に戻ると同じ内容。
        let j = mat_controller::im::tlv_to_json(&tlv).unwrap();
        assert_eq!(
            j,
            serde_json::json!([{"1":5,"2":2,"3":[112233],"4":null,"254":1}])
        );
    }

    #[test]
    fn generic_acl_encoding_matches_dedicated_encoder() {
        use mat_core::acl::{AclEntry, AclTarget};
        use mat_core::ids::{parse_value_typed, resolve_attribute};
        let entries = vec![
            AclEntry { privilege: 5, auth_mode: 2, subjects: vec![112233, 0x1122], targets: None, fabric_index: 1 },
            AclEntry {
                privilege: 3,
                auth_mode: 3,
                subjects: vec![0xFFFF_FFFF_FFFF_0001],
                targets: Some(vec![AclTarget { cluster: Some(6), endpoint: None, device_type: None }]),
                fabric_index: 1,
            },
        ];
        let dedicated = crate::ops::encode_acl_entries_tlv(&entries);
        let ty = resolve_attribute(0x001F, "acl").unwrap().def.unwrap().ty;
        let generic = scalar_to_tlv(
            &parse_value_typed(
                r#"[
                  {"privilege":5,"auth-mode":2,"subjects":[112233,4386],"targets":null,"fabric-index":1},
                  {"privilege":3,"auth-mode":3,"subjects":["0xFFFFFFFFFFFF0001"],
                   "targets":[{"cluster":6,"endpoint":null,"device-type":null}],"fabric-index":1}
                ]"#,
                &ty,
            )
            .unwrap(),
        );
        assert_eq!(generic, dedicated);
    }

    #[test]
    fn generic_group_key_map_encoding_matches_dedicated_encoder() {
        use mat_core::ids::{parse_value_typed, resolve_attribute};
        let dedicated = mat_controller::im::encode_group_key_map_tlv(&[(1, 2), (0x0101, 7)]);
        let ty = resolve_attribute(0x003F, "group-key-map").unwrap().def.unwrap().ty;
        let generic = scalar_to_tlv(
            &parse_value_typed(r#"[{"group-id":1,"group-key-set-id":2},{"1":257,"2":7}]"#, &ty).unwrap(),
        );
        assert_eq!(generic, dedicated);
    }

    #[test]
    fn generic_key_set_write_encoding_matches_dedicated_encoder() {
        use mat_core::ids::{classify_invoke, InvokeClass};
        let key: [u8; 16] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let dedicated = mat_controller::im::encode_key_set_write_fields(1, &key);
        let j = r#"{"group-key-set-id":1,"group-key-security-policy":0,"epoch-key0":"hex:00112233445566778899aabbccddeeff","epoch-start-time0":1,"epoch-key1":null,"epoch-start-time1":null,"epoch-key2":null,"epoch-start-time2":null}"#;
        let InvokeClass::Native { fields, .. } = classify_invoke("groupkeymanagement", "key-set-write", &[j.into()]) else {
            panic!("expected Native");
        };
        assert_eq!(encode_command_fields(&fields), dedicated);
    }
```

`scalar_conversions` の `scalar_to_im` 呼び出しは `Some(...)` 比較に変え、`assert_eq!(scalar_to_im(&S::List(vec![])), None);` を足す。

`crates/mat-native/src/op.rs:699-701` の既存テスト（`write_scalar_ok_list_rejected_unknown_unresolved`）で `"acl", "[]"` を Reject 期待している行を `"acl", "{}"`（list に object → 形不一致で Reject）に変える。

`crates/mat-core/src/parse.rs` tests に:

```rust
    #[test]
    fn normalize_value_parses_json_containers() {
        assert_eq!(
            normalize_value(r#"[{"1":5,"2":2}]"#),
            serde_json::json!([{"1":5,"2":2}])
        );
        assert_eq!(normalize_value("{\"a\": [1, 2]}"), serde_json::json!({"a":[1,2]}));
        // JSON でなければ従来どおり文字列。
        assert_eq!(normalize_value("[oops"), serde_json::Value::String("[oops".into()));
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-native 2>&1 | tail -5`
Expected: コンパイルエラー（`tlv_to_json` / `put_value` が無い、match 非網羅）。

- [ ] **Step 3: 実装**

`crates/mat-controller/src/im.rs`（`tlv_element_to_json` の直前）:

```rust
/// 1 要素の well-formed TLV（container 可）を JSON へ。`tlv_element_to_json` の
/// 公開入口 — 汎用 write の値ツリー符号化が read 側の JSON 化と同形になることを
/// mat-native のテストで固定するために使う。
pub fn tlv_to_json(tlv: &[u8]) -> Result<serde_json::Value, ImError> {
    let mut r = Reader::new(tlv);
    let first = r.next()?.ok_or(ImError::Malformed("empty tlv"))?;
    tlv_element_to_json(&mut r, first)
}
```

`crates/mat-native/src/lib.rs`:

```rust
/// 値ツリー（`mat_core::ids::ScalarValue`）を 1 要素の TLV として `w` に書く。
/// List → TLV Array（属性 list の型。TLV List 0x17 は path 専用）、Struct →
/// TLV Struct（context tag = fieldId、呼び出し側で id 昇順整列済み）。
pub fn put_value(w: &mut mat_controller::tlv::Writer, tag: mat_controller::tlv::Tag, v: &mat_core::ids::ScalarValue) {
    use mat_controller::tlv::Tag;
    use mat_core::ids::ScalarValue as S;
    match v {
        S::Bool(b) => w.put_bool(tag, *b),
        S::UInt(n) => w.put_uint(tag, *n),
        S::Int(n) => w.put_int(tag, *n),
        S::F32(f) => w.put_f32(tag, *f),
        S::F64(f) => w.put_f64(tag, *f),
        S::Str(s) => w.put_str(tag, s),
        S::Bytes(b) => w.put_bytes(tag, b),
        S::Null => w.put_null(tag),
        S::List(items) => {
            w.start_array(tag);
            for item in items {
                put_value(w, Tag::Anonymous, item);
            }
            w.end_container();
        }
        S::Struct(fields) => {
            w.start_struct(tag);
            for (id, val) in fields {
                put_value(w, Tag::Context(*id), val);
            }
            w.end_container();
        }
    }
}

pub fn scalar_to_tlv(v: &mat_core::ids::ScalarValue) -> Vec<u8> {
    let mut w = mat_controller::tlv::Writer::new();
    put_value(&mut w, mat_controller::tlv::Tag::Anonymous, v);
    w.finish()
}

pub fn encode_command_fields(args: &[mat_core::ids::ScalarValue]) -> Vec<u8> {
    use mat_controller::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    for (i, v) in args.iter().enumerate() {
        put_value(&mut w, Tag::Context(i as u8), v);
    }
    w.end_container();
    w.finish()
}

/// `ScalarValue` → `ImValue`（スカラーのみ。container は `None`）。
pub fn scalar_to_im(v: &mat_core::ids::ScalarValue) -> Option<ImValue> {
    use mat_core::ids::ScalarValue as S;
    Some(match v {
        S::Bool(b) => ImValue::Bool(*b),
        S::UInt(n) => ImValue::Uint(*n),
        S::Int(n) => ImValue::Int(*n),
        S::F32(f) => ImValue::F32(*f),
        S::F64(f) => ImValue::F64(*f),
        S::Str(s) => ImValue::Utf8(s.clone()),
        S::Bytes(b) => ImValue::Bytes(b.clone()),
        S::Null => ImValue::Null,
        S::List(_) | S::Struct(_) => return None,
    })
}
```

`crates/mat-native/src/ops.rs`: `fn encode_acl_entries_tlv` → `pub(crate) fn encode_acl_entries_tlv`。

`crates/mat-core/src/parse.rs` `normalize_value` 先頭（文字列リテラル判定の前）:

```rust
    // 汎用 write の list/struct 値（JSON）はそのまま JSON として返す。
    let t = raw.trim_start();
    if t.starts_with('[') || t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
            return v;
        }
    }
```

- [ ] **Step 4: テストと check**

Run: `cargo test -p mat-native -p mat-core -p mat-controller 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: 全 `ok`（等価性テスト 3 本を含む）。
Run: `task check 2>&1 | tail -3`
Expected: 成功。

- [ ] **Step 5: コミット**

```bash
git add crates/mat-native/src/lib.rs crates/mat-native/src/ops.rs crates/mat-native/src/op.rs crates/mat-controller/src/im.rs crates/mat-core/src/parse.rs
git commit -m "feat(write): 値ツリー → TLV（Array/Struct 再帰）+ 専用エンコーダとの等価性テスト、write エコーの JSON 化（レーン C 2/3）"
```

---

### Task 5: write / invoke の型契約ドキュメント

**Files:**
- Modify: `docs/commands.md:1014-1024`（節の書き換え）、`:171-182`（例の追加）
- Modify: `docs/errors.md:47-51`
- Modify: `ARCHITECTURE.md:1013-1014`、`:1050-1056`
- Modify: `CLAUDE.md`（Backend 節の "Generic write..." 段落）

- [ ] **Step 1: docs/commands.md の節を書き換える**

見出し `#### Scalar-only generic write / invoke` を `#### Generic write / invoke value encoding` に変え、本文を:

```markdown
#### Generic write / invoke value encoding

Generic `write` / `invoke` (and `group invoke`) encode the value from the type
the generated name table (`mat-core::ids`, connectedhomeip v1.4.2.0) knows for
the attribute / command field:

- **Scalars** use plain literals: `true` / `false` / `1` / `0` (bool), decimal
  or `0x..` (uint / enum / bitmap), decimal (int), `1.5` / `2e-3` (float —
  `single` / `double` pick TLV f32 / f64), any text (string), `hex:aabb`
  (octstr). `null` writes a null (nullability is checked by the device).
- **Lists and structs** take **JSON**. A struct is an object whose keys are the
  field names in kebab-case (`privilege`, `auth-mode`, `group-key-set-id`) **or**
  the numeric field ids as strings (`"1"`, `"254"`) — the shape `read` prints —
  so a `read` result can be written back verbatim. Inside JSON, uint fields also
  accept `"0x.."` strings (64-bit subjects), octstr fields accept `"aabb"` or
  `"hex:aabb"`, and fabric-scoped structs accept the implicit `fabric-index`
  (254; the server ignores / replaces it). Unknown keys, a field given both by
  name and by id, a missing non-optional field, or a JSON shape that does not
  match the type are `parse_error` with the offending path in `detail`.
- **Numeric ids** (a cluster / attribute / command the table does not know)
  carry no schema: scalars are inferred from the literal (`true`, `42`, `-1`,
  `1.5`, `hex:..`, else string); a list / struct JSON is a `parse_error` —
  use a name the table knows.

The `group provision` / `grant` list/struct writes (KeySetWrite, GroupKeyMap,
ACL read-modify-write) keep their dedicated encoders; unit tests pin that the
generic encoder produces byte-identical TLV for the same entries.

```bash
# Write back a list-of-struct attribute exactly as read prints it (numeric keys)
mat write --node 5 --cluster groupkeymanagement --attribute group-key-map \
  --value '[{"1":257,"2":1,"254":1}]'
# ... or with field names
mat write --node 5 --cluster groupkeymanagement --attribute group-key-map \
  --value '[{"group-id":257,"group-key-set-id":1}]'
```
```

State operations の例（`:175` の write 例の下）に 1 行:

```bash
# List / struct values are JSON (see "Generic write / invoke value encoding")
mat write --node 5 --cluster accesscontrol --attribute acl --value '[{"privilege":5,"auth-mode":2,"subjects":[112233],"targets":null}]'
```

- [ ] **Step 2: docs/errors.md / ARCHITECTURE.md / CLAUDE.md**

`docs/errors.md` の `parse_error` 項:

```markdown
- `parse_error` — this kind is returned when a generic `write` / `invoke` value
  does not fit the type the generated table knows for the attribute / command
  field (bad literal, JSON of the wrong shape, unknown / missing struct field —
  the detail names the path), when a list / struct JSON is given with a numeric
  id (no schema), or when a cluster / attribute / command name is not in the
  table (pass the numeric id instead).
```

`ARCHITECTURE.md:1013-1014` の「(2) 汎用 list/struct TLV エンコード（現状 scalar のみが仕様、汎用 write/invoke の後退を受容）」を `~~(2) 汎用 list/struct TLV エンコード（現状 scalar のみが仕様、汎用 write/invoke の後退を受容）~~【訂正 2026-09-03: 実装済み — float/list/struct を生成テーブルの struct スキーマで符号化、docs/superpowers/specs/2026-09-03-write-types-design.md】` に。`:1050-1053` の「(4) **JSON→TLV の型サポートはスカラーのみ** … 明示拒否。」も同様に取り消し線 + 同じ訂正注記（専用エンコーダの記述はそのまま有効なので残す）。

`CLAUDE.md` Backend 節の段落を:

```markdown
- Generic `write`/`invoke`/`group invoke` encode JSON→TLV from the type the
  `mat-core::ids` table knows: scalars as literals (bool/int/uint/enum/bitmap/
  float/string/octstr, bytes as `hex:`), lists and structs as JSON (keys =
  kebab field names or numeric field ids, the shape `read` prints). Names the
  table does not know are `parse_error` (numeric IDs are the escape hatch for
  scalars only — no schema, so no list/struct). `group provision`/`grant`
  list/struct writes keep dedicated encoders, pinned byte-equal to the generic
  path by tests.
```

- [ ] **Step 3: check とコミット**

Run: `task check 2>&1 | tail -3`
Expected: 成功。

```bash
git add docs/commands.md docs/errors.md ARCHITECTURE.md CLAUDE.md
git commit -m "docs: 汎用 write/invoke の値符号化契約（float/list/struct、JSON キー規約、数値 ID の制限）（レーン C 2/3）"
```

---

### Task 6: listen の list/struct 非対称を明文化（doc のみ）

**Files:**
- Modify: `docs/commands.md:545-548`
- Modify: `ARCHITECTURE.md:1116-1117`
- Modify: `crates/matd/src/subscription.rs:497-500`（doc コメント）

- [ ] **Step 1: docs/commands.md の listen 節**

```markdown
  restart) for a fresh trigger. Only **scalar** values become events —
  `list` / `struct` attributes (ACL, server-list, etc., which show up in a
  wildcard priming burst) are dropped with a debug log. This is a
  **listen-only** limitation (generic `read` and `write` handle lists and
  structs as JSON): the resident Subscribe processes each ReportData message on
  its own and does not reassemble chunked lists (`MoreChunkedMessages` plus
  per-element `ListIndex: null` appends), so a list event could only ever carry
  a partial value, and the priming-diff recovery would misread per-element
  appends as transitions. Consumers that need a list value read it on demand.
```

- [ ] **Step 2: ARCHITECTURE.md と subscription.rs**

`ARCHITECTURE.md:1116` の「scalar 値のみイベント化（list/struct は generic read と同じ既知の制限で debug ログのみに捨てる）」→「scalar 値のみイベント化（list/struct は listen だけの制限 — Subscribe 経路は ReportData を 1 通ずつ処理しチャンク再組み立てを持たないため途中 list しか作れず、list-diff recovery も誤爆する。generic read/write は list/struct を JSON で扱う。docs/commands.md「Listen」参照）」。

`crates/matd/src/subscription.rs` の `events_from_report` doc コメント:

```rust
/// ReportDataMessage をイベント列へ。scalar 値のみイベント化し、list/struct
/// （ACL・server-list 等 wildcard priming に混ざるもの）は debug ログで捨てる。
/// これは **listen だけの制限**（generic read/write は list/struct を JSON で
/// 扱う）: この経路は ReportData を 1 通ずつ処理しチャンク再組み立て
/// （MoreChunkedMessages + ListIndex:null 追記列）を持たないので途中 list しか
/// 作れず、list-diff priming recovery も要素追記を遷移と誤認する。path が欠けた
/// report・status-only も捨てる。
```

- [ ] **Step 3: check とコミット**

Run: `task check 2>&1 | tail -3`
Expected: 成功。

```bash
git add docs/commands.md ARCHITECTURE.md crates/matd/src/subscription.rs
git commit -m "docs(listen): list/struct を捨てるのは listen だけの制限と明記（チャンク再組み立て無し・recovery 誤爆）（レーン C 3/3）"
```

---

### Task 7: 実機スモーク + 仕上げ（メインセッションで実施、subagent に出さない）

**Files:** なし（記録は memory とマージコミット）

- [ ] **Step 1: 全体 check と x86_64 バイナリのビルド**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/write-types && task check 2>&1 | tail -3
cargo build --release -p matterctl 2>&1 | tail -1 && ls -la target/release/mat
```

- [ ] **Step 2: 他セッションのスモークが走っていないことを確認してからコンテナへ投入**

```bash
ssh nas 'docker exec hogar-matd sh -c "ls /tmp; ps -eo pid,etime,cmd | grep -v grep | grep -E \"mat(\\.new)? \" || true"'
scp target/release/mat nas:/tmp/mat.new && ssh nas 'docker cp /tmp/mat.new hogar-matd:/tmp/mat.new && docker exec hogar-matd chmod 755 /tmp/mat.new && docker exec hogar-matd /tmp/mat.new --version'
```

- [ ] **Step 3: list-of-struct write の無害パターン（直経路）**

group メンバーノード（`matd status` の購読一覧、または memory の node 17 等）で:

```bash
ssh nas 'docker exec -e MAT_MATD=0 -e MAT_FABRIC_INDEX=2 -e MAT_STORE=/data/mat hogar-matd /tmp/mat.new read --node <N> --endpoint 0 --cluster groupkeymanagement --attribute group-key-map'
# 出力の value（番号キー JSON）をそのまま --value に渡す
ssh nas 'docker exec -e MAT_MATD=0 -e MAT_FABRIC_INDEX=2 -e MAT_STORE=/data/mat hogar-matd /tmp/mat.new write --node <N> --endpoint 0 --cluster groupkeymanagement --attribute group-key-map --value '"'"'<value>'"'"''
ssh nas 'docker exec -e MAT_MATD=0 -e MAT_FABRIC_INDEX=2 -e MAT_STORE=/data/mat hogar-matd /tmp/mat.new read --node <N> --endpoint 0 --cluster groupkeymanagement --attribute group-key-map'
```
Expected: write exit 0、再 read の `value` が最初の read と一致。**ACL には書かない**。

- [ ] **Step 4: スカラー write の回帰 + matd 経路 read**

```bash
ssh nas 'docker exec -e MAT_MATD=0 -e MAT_FABRIC_INDEX=2 -e MAT_STORE=/data/mat hogar-matd /tmp/mat.new read --node <N> --cluster levelcontrol --attribute on-level'
ssh nas 'docker exec -e MAT_MATD=0 -e MAT_FABRIC_INDEX=2 -e MAT_STORE=/data/mat hogar-matd /tmp/mat.new write --node <N> --cluster levelcontrol --attribute on-level --value <same>'
ssh nas 'docker exec hogar-matd /tmp/mat.new read --node <N> --cluster onoff --attribute on-off --matd /run/matd/matd.sock'
```
Expected: 全て exit 0。後始末: `ssh nas 'docker exec hogar-matd rm /tmp/mat.new; rm /tmp/mat.new'`。

- [ ] **Step 5: main へ rebase → no-ff マージ → push、memory 追記**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat && git fetch -q && cd .claude/worktrees/write-types && git rebase origin/main && task check 2>&1 | tail -2
cd /home/noguk/ghq/github.com/nogu3/mat && git merge --no-ff worktree-write-types -m "Merge: 汎用 write/invoke の float/list/struct 対応 + listen 非対称の明文化（監査レーン C）" && git push origin main
```
memory `mat-code-audit-2026-08-31.md` の「並列レーン計画」節にレーン C 完了（日付・マージコミット・スモーク結果・実機で検証できなかった float / invoke struct の注記）を追記。worktree とブランチを削除。
