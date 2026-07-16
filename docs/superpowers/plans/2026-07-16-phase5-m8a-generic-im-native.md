# Phase 5 M8a: 汎用 IM native 化 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** name→ID 全クラスタ生成テーブルを導入し、`read`(汎用)/`write`/`invoke`(汎用)/`describe`/`diag`/`open-window`/`group provision・grant` を one-shot 直経路と matd の両方で native 化する（chip-tool は M8c までフォールバックとして残る）。

**Architecture:** 生成テーブル（mat-core）→ IM 汎用化（mat-controller: Write・wildcard read・チャンク対応 ReportData・TLV→JSON）→ NodeConn 拡張（mat-native）→ CLI/matd 配線、の4層を下から積む。spec: `docs/superpowers/specs/2026-07-16-phase5-m8a-generic-im-native-design.md`。

**Tech Stack:** Rust (workspace: mat-core / mat-controller / mat-native / mat / matd), Python 3 (生成スクリプト), connectedhomeip data model XML (v1.4.2.0)。

## Global Constraints

- **作業ブランチ**: `matter-controller`（worktree `.claude/worktrees/phase5-m1-controller-core`）。**全タスクの冒頭で `pwd` と `git branch --show-current` を確認する**こと（サブエージェントの shell はメイン repo (main) で始まる罠が既知）。main へのマージは実機 E2E 合格後に別途（このplanの範囲外）。
- **バージョン**: workspace `Cargo.toml` の `version = "0.18.0"`（Task 1 で上げる）。
- **出力 JSON スキーマは完全維持**。既存統合テスト（fake-chip-tool 含む）は**無改変で全通過**が各タスクの回帰条件。
- **経路優先順位・失敗分岐は M7 と同型**: matd 自動発見 → native 直（iface 設定時）→ chip-tool 直。unicast native 失敗は即エラー（フォールバックしない）。エンジン構築失敗は warn + chip-tool フォールバック。
- **汎用 write/invoke はスカラー型のみ**（bool/int/uint/enum/bitmap/string/octstr）。名前解決できた上で list/struct/float/unknown 型 → `parse_error` で明示拒否（フォールバックしない — spec 決定3）。**名前が解決できない**（テーブルに無く数値でもない）→ native 対象外 = chip-tool へ（挙動互換）。
- **数値 ID 直指定**（cluster/attribute/command とも、`10` / `0x0A` 形式）は常に許可。
- コミットは各タスク末尾で行い、メッセージ末尾に `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` を付ける。コミット前に `cargo fmt` を通すこと（最終タスクで `task check`）。
- 生成テーブルの生成元: connectedhomeip **タグ v1.4.2.0** の `src/app/zap-templates/zcl/data-model/chip/*.xml`。生成済み Rust をチェックインし、ビルド時に XML・ネットワークは不要。

---

### Task 1: ブランチ追従 + バージョン 0.18.0

**Files:**
- Modify: `Cargo.toml`（workspace version のみ）

**Interfaces:**
- Produces: main (b15a739 マージ以後) に追従した `matter-controller` ブランチ。以後の全タスクはこの worktree で行う。

- [ ] **Step 1: worktree とブランチの状態確認**

```bash
cd /home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/phase5-m1-controller-core
pwd && git branch --show-current   # => matter-controller
git fetch origin && git log --oneline -3 main
```

- [ ] **Step 2: main を取り込む**

matter-controller は M7 マージ（b15a739）前の 2c26c0a にいる。main を merge する（rebase 不可 — 公開済みブランチ）:

```bash
git merge main -m "merge: main (M7完了+M8a spec) を matter-controller に取り込み (M8a Task1)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

コンフリクトは出ない見込み（main は matter-controller のマージ + docs のみ）。出た場合は main 側を優先。

- [ ] **Step 3: バージョンを 0.18.0 に**

`Cargo.toml`（workspace ルート）の `version = "0.17.0"` → `version = "0.18.0"`。

- [ ] **Step 4: ビルド確認**

```bash
cargo build --workspace 2>&1 | tail -3   # 成功すること
cargo test -p mat-native 2>&1 | tail -3  # 既存テスト green
```

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: version 0.18.0 (M8a開始) (M8a Task1)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: name→ID 全クラスタ生成テーブル（生成スクリプト + mat-core::ids）

**Files:**
- Create: `scripts/gen-ids.py`（生成スクリプト、チェックイン）
- Create: `crates/mat-core/src/ids_gen.rs`（生成物、チェックイン。手編集禁止ヘッダ付き）
- Create: `crates/mat-core/src/ids.rs`（lookup API、手書き）
- Modify: `crates/mat-core/src/lib.rs`（`pub mod ids; mod ids_gen;` を追加）
- Test: `crates/mat-core/src/ids.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Produces（mat-core::ids、後続タスク全部が使う）:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTag { Bool, UInt, Int, Float, Str, Bytes, List, Struct, Unknown }

pub struct ClusterDef { pub name: &'static str, pub id: u32,
    pub attrs: &'static [AttrDef], pub cmds: &'static [CmdDef] }
pub struct AttrDef { pub name: &'static str, pub id: u32, pub ty: TypeTag,
    pub writable: bool, pub timed_write: bool }
pub struct CmdDef { pub name: &'static str, pub id: u32, pub timed: bool,
    pub fields: &'static [FieldDef] }
pub struct FieldDef { pub name: &'static str, pub ty: TypeTag, pub optional: bool }
// FieldDef の TLV context tag は fields 配列内の添字（0-based）。

/// "onoff" / "6" / "0x0006" → cluster id。名前はテーブル、数値はそのまま。
pub fn resolve_cluster(input: &str) -> Option<u32>;
pub fn find_cluster(id: u32) -> Option<&'static ClusterDef>;

/// 解決結果: id は常にある。def はテーブルに定義がある場合のみ（数値直指定は None）。
pub struct AttrRef { pub id: u32, pub def: Option<&'static AttrDef> }
pub struct CmdRef { pub id: u32, pub def: Option<&'static CmdDef> }
pub fn resolve_attribute(cluster: u32, input: &str) -> Option<AttrRef>;
pub fn resolve_command(cluster: u32, input: &str) -> Option<CmdRef>;

/// "10" / "0x0A" を u64 に（名前解決の数値パス。負数・空は None）。
pub fn parse_num(input: &str) -> Option<u64>;
```

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/ids.rs` を lookup API の宣言だけ（`todo!()` なし、ids_gen 参照でコンパイルエラーになる状態は避けるため、まずテストから）:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_known_cluster_names_and_ids() {
        // 生成テーブルのスポットチェック: 既知の chip-tool 名 → 既知 ID。
        assert_eq!(resolve_cluster("onoff"), Some(0x0006));
        assert_eq!(resolve_cluster("colorcontrol"), Some(0x0300));
        assert_eq!(resolve_cluster("threadnetworkdiagnostics"), Some(0x0035));
        assert_eq!(resolve_cluster("accesscontrol"), Some(0x001F));
        assert_eq!(resolve_cluster("descriptor"), Some(0x001D));
        assert_eq!(resolve_cluster("groupkeymanagement"), Some(0x003F));
        assert_eq!(resolve_cluster("groups"), Some(0x0004));
        assert_eq!(resolve_cluster("levelcontrol"), Some(0x0008));
        // 数値直指定（10進 / 16進）。
        assert_eq!(resolve_cluster("6"), Some(6));
        assert_eq!(resolve_cluster("0x0300"), Some(0x0300));
        // 未知名は None。
        assert_eq!(resolve_cluster("nosuchcluster"), None);
    }

    #[test]
    fn resolves_known_attributes_with_types() {
        let a = resolve_attribute(0x0006, "on-off").unwrap();
        assert_eq!(a.id, 0x0000);
        assert_eq!(a.def.unwrap().ty, TypeTag::Bool);
        let a = resolve_attribute(0x0300, "color-temperature-mireds").unwrap();
        assert_eq!(a.id, 0x0007);
        assert_eq!(a.def.unwrap().ty, TypeTag::UInt);
        let a = resolve_attribute(0x0035, "neighbor-table").unwrap();
        assert_eq!(a.id, 0x0007);
        assert_eq!(a.def.unwrap().ty, TypeTag::List);
        let a = resolve_attribute(0x001F, "acl").unwrap();
        assert_eq!(a.id, 0x0000);
        assert_eq!(a.def.unwrap().ty, TypeTag::List);
        let a = resolve_attribute(0x003F, "group-key-map").unwrap();
        assert_eq!(a.id, 0x0000);
        assert_eq!(a.def.unwrap().ty, TypeTag::List);
        let a = resolve_attribute(0x001D, "parts-list").unwrap();
        assert_eq!(a.id, 0x0003);
        // descriptor server-list。
        let a = resolve_attribute(0x001D, "server-list").unwrap();
        assert_eq!(a.id, 0x0001);
        // 数値直指定は def なしで通る。
        let a = resolve_attribute(0x0006, "0x4001").unwrap();
        assert_eq!(a.id, 0x4001);
        assert!(a.def.is_none());
    }

    #[test]
    fn resolves_known_commands_with_fields() {
        let c = resolve_command(0x0006, "on").unwrap();
        assert_eq!(c.id, 0x01);
        assert!(c.def.unwrap().fields.is_empty());
        let c = resolve_command(0x0300, "move-to-color-temperature").unwrap();
        assert_eq!(c.id, 0x0A);
        // fields: ColorTemperatureMireds, TransitionTime, OptionsMask, OptionsOverride
        assert_eq!(c.def.unwrap().fields.len(), 4);
        assert_eq!(c.def.unwrap().fields[0].ty, TypeTag::UInt);
        let c = resolve_command(0x003F, "key-set-write").unwrap();
        assert_eq!(c.id, 0x00);
        // KeySetWrite の field 0 は GroupKeySetStruct。
        assert_eq!(c.def.unwrap().fields[0].ty, TypeTag::Struct);
        let c = resolve_command(0x0004, "add-group").unwrap();
        assert_eq!(c.id, 0x00);
        // open-commissioning-window は timed invoke 必須。
        let c = resolve_command(0x003C, "open-commissioning-window").unwrap();
        assert!(c.def.unwrap().timed);
    }

    #[test]
    fn parse_num_accepts_dec_and_hex() {
        assert_eq!(parse_num("10"), Some(10));
        assert_eq!(parse_num("0x0A"), Some(10));
        assert_eq!(parse_num("0X0a"), Some(10));
        assert_eq!(parse_num(""), None);
        assert_eq!(parse_num("-1"), None);
        assert_eq!(parse_num("on-off"), None);
    }
}
```

- [ ] **Step 2: 生成スクリプトを書く**

`scripts/gen-ids.py`。使い方と名前変換規則をヘッダに明記する:

```python
#!/usr/bin/env python3
"""mat-core/src/ids_gen.rs を connectedhomeip の data model XML から生成する。

使い方:
    python3 scripts/gen-ids.py /path/to/connectedhomeip > crates/mat-core/src/ids_gen.rs

前提: connectedhomeip は **タグ v1.4.2.0** を checkout していること
（chip-tool KVS リーダと同じバージョン固定。ids のスポットチェック単体テストが
名前・ID の回帰を検知する）。

名前変換（chip-tool 互換）:
- cluster 名:  lowercase + 非英数字除去    ("On/Off" -> "onoff")
- attr/cmd 名: kebab-case                  ("ColorTemperatureMireds" ->
               "color-temperature-mireds", "ACL" -> "acl",
               "KeySetWrite" -> "key-set-write")
"""
import glob
import os
import re
import sys
import xml.etree.ElementTree as ET


def cluster_key(name: str) -> str:
    return re.sub(r"[^a-z0-9]", "", name.lower())


def kebab(name: str) -> str:
    # 空白/スラッシュ/アンダースコアは区切り。camelCase 境界と
    # 大文字連続の末尾 ("ACLEntry" -> "acl-entry") にも区切りを入れる。
    s = re.sub(r"[ /_\-]+", "-", name.strip())
    s = re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "-", s)
    s = re.sub(r"(?<=[A-Z])(?=[A-Z][a-z])", "-", s)
    s = re.sub(r"-+", "-", s)
    return s.lower()


BASE_TYPES = {
    "boolean": "Bool",
    "single": "Float", "double": "Float",
    "char_string": "Str", "long_char_string": "Str",
    "octet_string": "Bytes", "long_octet_string": "Bytes",
}


def type_tag(ty: str, enums: set, bitmaps: set, structs: set) -> str:
    t = ty.strip()
    tl = t.lower()
    if tl in BASE_TYPES:
        return BASE_TYPES[tl]
    if tl == "array":
        return "List"
    if re.fullmatch(r"int\d+u", tl) or re.fullmatch(r"enum\d+", tl) \
       or re.fullmatch(r"bitmap\d+", tl):
        return "UInt"
    if re.fullmatch(r"int\d+s?", tl):
        # "int8s".."int64s" は Int、"int8".."int64"（無印）は歴史的に符号なし扱い。
        return "Int" if tl.endswith("s") else "UInt"
    # zap の派生型（epoch_s, fabric_idx, node_id, percent, temperature 等）は
    # ほぼ全て符号なし整数ベース。enum/bitmap/struct の名前付き型を先に判定。
    if t in structs:
        return "Struct"
    if t in enums or t in bitmaps:
        return "UInt"
    # 名前付き型でなければ符号なし整数系の派生型とみなす。ただし保守的に、
    # 明らかに構造的な型名（"Struct" を含む）は Struct に。
    if "struct" in tl:
        return "Struct"
    return "UInt"


def parse_files(root_dir: str):
    xml_dir = os.path.join(
        root_dir, "src", "app", "zap-templates", "zcl", "data-model", "chip")
    files = sorted(glob.glob(os.path.join(xml_dir, "*.xml")))
    if not files:
        sys.exit(f"no xml under {xml_dir}")
    enums, bitmaps, structs = set(), set(), set()
    cluster_elems = []
    for f in files:
        tree = ET.parse(f)
        for e in tree.getroot().iter("enum"):
            enums.add(e.get("name", ""))
        for e in tree.getroot().iter("bitmap"):
            bitmaps.add(e.get("name", ""))
        for e in tree.getroot().iter("struct"):
            structs.add(e.get("name", ""))
        for c in tree.getroot().iter("cluster"):
            cluster_elems.append(c)
    return cluster_elems, enums, bitmaps, structs


def attr_name(a) -> str:
    # 属性名は要素テキスト、新形式では name 属性のこともある。
    if a.get("name"):
        return a.get("name")
    if a.text and a.text.strip():
        return a.text.strip()
    d = a.find("description")
    return d.text.strip() if d is not None and d.text else ""


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    cluster_elems, enums, bitmaps, structs = parse_files(sys.argv[1])
    clusters = {}
    for c in cluster_elems:
        name = c.findtext("name", "").strip()
        code = c.findtext("code", "").strip()
        if not name or not code:
            continue
        cid = int(code, 0)
        attrs, cmds = [], []
        for a in c.iter("attribute"):
            an = attr_name(a)
            acode = a.get("code")
            if not an or acode is None:
                continue
            ty = a.get("type", "")
            entry = a.get("entryType")
            tag = "List" if (entry or ty.lower() == "array") \
                else type_tag(ty, enums, bitmaps, structs)
            attrs.append((kebab(an), int(acode, 0), tag,
                          a.get("writable", "false") == "true",
                          a.get("mustUseTimedWrite", "false") == "true"))
        for cmd in c.iter("command"):
            if cmd.get("source") != "client":
                continue
            cn, ccode = cmd.get("name", ""), cmd.get("code")
            if not cn or ccode is None:
                continue
            fields = []
            for arg in cmd.iter("arg"):
                fn, fty = arg.get("name", ""), arg.get("type", "")
                ftag = "List" if arg.get("array", "false") == "true" \
                    else type_tag(fty, enums, bitmaps, structs)
                fields.append((kebab(fn), ftag,
                               arg.get("optional", "false") == "true"))
            cmds.append((kebab(cn), int(ccode, 0),
                         cmd.get("mustUseTimedInvoke", "false") == "true",
                         fields))
        key = cluster_key(name)
        # 同一クラスタが複数ファイルに現れる場合は先勝ち（chip 配下は一意のはず）。
        if key not in clusters:
            clusters[key] = (cid, sorted(set(attrs)), sorted({
                (n, i, t, tuple(f)) for (n, i, t, f) in cmds}))
    emit(clusters)


def emit(clusters):
    print("// @generated by scripts/gen-ids.py — DO NOT EDIT BY HAND.")
    print("// Source: connectedhomeip v1.4.2.0 data-model XML. 再生成手順は")
    print("// scripts/gen-ids.py のヘッダ参照。")
    print("use super::ids::{AttrDef, ClusterDef, CmdDef, FieldDef, TypeTag};")
    print()
    names = sorted(clusters.keys())
    for key in names:
        cid, attrs, cmds = clusters[key]
        up = key.upper()
        print(f"static ATTRS_{up}: &[AttrDef] = &[")
        for (n, i, t, w, tw) in attrs:
            print(f'    AttrDef {{ name: "{n}", id: {i:#06x}, '
                  f"ty: TypeTag::{t}, writable: {str(w).lower()}, "
                  f"timed_write: {str(tw).lower()} }},")
        print("];")
        print(f"static CMDS_{up}: &[CmdDef] = &[")
        for (n, i, timed, fields) in cmds:
            fl = ", ".join(
                f'FieldDef {{ name: "{fn}", ty: TypeTag::{ft}, '
                f"optional: {str(fo).lower()} }}"
                for (fn, ft, fo) in fields)
            print(f'    CmdDef {{ name: "{n}", id: {i:#04x}, '
                  f"timed: {str(timed).lower()}, fields: &[{fl}] }},")
        print("];")
    print()
    print("/// 名前昇順（binary search 用）。")
    print("pub(super) static CLUSTERS: &[ClusterDef] = &[")
    for key in names:
        cid, _, _ = clusters[key]
        up = key.upper()
        print(f'    ClusterDef {{ name: "{key}", id: {cid:#06x}, '
              f"attrs: ATTRS_{up}, cmds: CMDS_{up} }},")
    print("];")


if __name__ == "__main__":
    main()
```

- [ ] **Step 3: connectedhomeip を取得してテーブルを生成**

```bash
# 既存のローカル checkout を先に探す（jarvis/WSL に chip-tool ビルド用がある可能性）。
ghq list -p 2>/dev/null | grep -i connectedhomeip || ls ~/connectedhomeip 2>/dev/null
# 無ければ浅い clone（XML だけ要るので blobless でもよい）。
git clone --depth 1 --branch v1.4.2.0 \
  https://github.com/project-chip/connectedhomeip /tmp/chip-v1420
python3 scripts/gen-ids.py /tmp/chip-v1420 > crates/mat-core/src/ids_gen.rs
wc -l crates/mat-core/src/ids_gen.rs   # 数千行になるはず
```

ネットワークが使えない場合は既存のローカル checkout（`ghq list`）を探し、`git -C <path> describe --tags` でバージョンを記録すること（v1.4.2.0 でない場合はテスト Step 5 の期待値で検知される。乖離があればテスト側でなく **XML 側を v1.4.2.0 に合わせる**）。

- [ ] **Step 4: lookup API を実装**

`crates/mat-core/src/ids.rs`:

```rust
//! chip-tool 記法の cluster/attribute/command 名 → Matter 数値 ID の解決。
//!
//! テーブルは `ids_gen.rs`（scripts/gen-ids.py で connectedhomeip v1.4.2.0 から
//! 生成、チェックイン）。名前の意味論は chip-tool 記法のまま（CLAUDE.md）。
//! 数値直指定（"10" / "0x0A"）は常に許可 — その場合 `def` は `None` で、
//! write の型推定は値リテラルから行う（Task 3）。

use super::ids_gen::CLUSTERS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTag { Bool, UInt, Int, Float, Str, Bytes, List, Struct, Unknown }

pub struct ClusterDef {
    pub name: &'static str,
    pub id: u32,
    pub attrs: &'static [AttrDef],
    pub cmds: &'static [CmdDef],
}
pub struct AttrDef {
    pub name: &'static str,
    pub id: u32,
    pub ty: TypeTag,
    pub writable: bool,
    pub timed_write: bool,
}
pub struct CmdDef {
    pub name: &'static str,
    pub id: u32,
    pub timed: bool,
    pub fields: &'static [FieldDef],
}
/// TLV context tag は `CmdDef::fields` 内の添字（0-based）。
pub struct FieldDef {
    pub name: &'static str,
    pub ty: TypeTag,
    pub optional: bool,
}

pub struct AttrRef { pub id: u32, pub def: Option<&'static AttrDef> }
pub struct CmdRef { pub id: u32, pub def: Option<&'static CmdDef> }

pub fn parse_num(input: &str) -> Option<u64> {
    let s = input.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok();
    }
    s.parse::<u64>().ok()
}

pub fn resolve_cluster(input: &str) -> Option<u32> {
    if let Some(n) = parse_num(input) {
        return u32::try_from(n).ok();
    }
    CLUSTERS
        .binary_search_by(|c| c.name.cmp(input))
        .ok()
        .map(|i| CLUSTERS[i].id)
}

pub fn find_cluster(id: u32) -> Option<&'static ClusterDef> {
    CLUSTERS.iter().find(|c| c.id == id)
}

pub fn resolve_attribute(cluster: u32, input: &str) -> Option<AttrRef> {
    if let Some(n) = parse_num(input) {
        return u32::try_from(n).ok().map(|id| AttrRef { id, def: None });
    }
    let def = find_cluster(cluster)?.attrs.iter().find(|a| a.name == input)?;
    Some(AttrRef { id: def.id, def: Some(def) })
}

pub fn resolve_command(cluster: u32, input: &str) -> Option<CmdRef> {
    if let Some(n) = parse_num(input) {
        return u32::try_from(n).ok().map(|id| CmdRef { id, def: None });
    }
    let def = find_cluster(cluster)?.cmds.iter().find(|c| c.name == input)?;
    Some(CmdRef { id: def.id, def: Some(def) })
}
```

`crates/mat-core/src/lib.rs` に `pub mod ids;` と `mod ids_gen;` を追加（ids_gen は `pub(crate)` アイテムのみ）。ids_gen.rs 冒頭に `#![allow(clippy::all)]` 相当が必要なら `#[rustfmt::skip]` をモジュール参照側に付けるのではなく、生成ファイル先頭に `// @generated` があるので `rustfmt.toml` は触らず、`cargo fmt` が生成物を壊す場合のみ生成ファイル先頭行に `#![cfg_attr(rustfmt, rustfmt::skip)]` を出すよう gen-ids.py を直す。

- [ ] **Step 5: テスト実行**

```bash
cargo test -p mat-core ids 2>&1 | tail -5
```

Expected: PASS。失敗したら（名前変換 or 型判定の乖離）、期待値でなく **gen-ids.py の変換規則を直して再生成**（期待値は chip-tool 実記法として固定）。

- [ ] **Step 6: clippy + fmt**

```bash
cargo clippy -p mat-core -- -D warnings && cargo fmt
```

生成物が巨大で clippy に引っかかる場合（`unreadable_literal` 等）は ids_gen.rs 先頭に `#![allow(clippy::unreadable_literal)]` を生成するよう script を直す。

- [ ] **Step 7: Commit**

```bash
git add scripts/gen-ids.py crates/mat-core/src/ids.rs crates/mat-core/src/ids_gen.rs crates/mat-core/src/lib.rs
git commit -m "feat(mat-core): name→ID 全クラスタ生成テーブル (M8a Task2)

connectedhomeip v1.4.2.0 data-model XML から生成（scripts/gen-ids.py）。
cluster/attribute/command の chip-tool 記法名→ID+型タグ+コマンドフィールド。
数値ID直指定は常に許可。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: CLI 文字列 → スカラー値変換（typed / inferred）

**Files:**
- Modify: `crates/mat-core/src/ids.rs`（追記）
- Test: 同ファイル `#[cfg(test)]`

**Interfaces:**
- Produces（mat-core::ids）:

```rust
/// write / invoke 引数のスカラー値（mat-controller の ImValue と同形。
/// mat-core は mat-controller に依存できないため別型で持ち、mat-native 側で写す）。
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue { Bool(bool), UInt(u64), Int(i64), Str(String), Bytes(Vec<u8>), Null }

/// 型タグに従って CLI 入力文字列をスカラーへ。Err は人間可読の理由
/// （そのまま parse_error detail に使える）。
pub fn parse_scalar_typed(input: &str, ty: TypeTag) -> Result<ScalarValue, String>;

/// 数値 ID 直指定（def 無し）用: JSON リテラル風に型推定する。
/// true/false→Bool, null→Null, 整数→UInt(負なら Int), "hex:AABB"→Bytes, その他→Str。
pub fn parse_scalar_inferred(input: &str) -> ScalarValue;
```

- [ ] **Step 1: 失敗するテストを書く**（`ids.rs` の tests mod へ追記）

```rust
#[test]
fn parse_scalar_typed_scalars() {
    use ScalarValue as V;
    assert_eq!(parse_scalar_typed("true", TypeTag::Bool), Ok(V::Bool(true)));
    assert_eq!(parse_scalar_typed("0", TypeTag::Bool), Ok(V::Bool(false)));
    assert_eq!(parse_scalar_typed("1", TypeTag::Bool), Ok(V::Bool(true)));
    assert_eq!(parse_scalar_typed("128", TypeTag::UInt), Ok(V::UInt(128)));
    assert_eq!(parse_scalar_typed("0x80", TypeTag::UInt), Ok(V::UInt(128)));
    assert_eq!(parse_scalar_typed("-5", TypeTag::Int), Ok(V::Int(-5)));
    assert_eq!(parse_scalar_typed("hello", TypeTag::Str), Ok(V::Str("hello".into())));
    assert_eq!(
        parse_scalar_typed("hex:d0d1", TypeTag::Bytes),
        Ok(V::Bytes(vec![0xd0, 0xd1]))
    );
    assert_eq!(parse_scalar_typed("null", TypeTag::UInt), Ok(V::Null));
}

#[test]
fn parse_scalar_typed_rejects_unsupported_and_bad_literals() {
    assert!(parse_scalar_typed("[]", TypeTag::List).is_err());
    assert!(parse_scalar_typed("{}", TypeTag::Struct).is_err());
    assert!(parse_scalar_typed("1.5", TypeTag::Float).is_err()); // float write は M8a 未対応
    assert!(parse_scalar_typed("abc", TypeTag::UInt).is_err());
    assert!(parse_scalar_typed("xyz", TypeTag::Bool).is_err());
    assert!(parse_scalar_typed("hex:zz", TypeTag::Bytes).is_err());
    assert!(parse_scalar_typed("1", TypeTag::Unknown).is_err());
    // エラーメッセージは型名を含む（spec 受け入れ5: AI が判断できる detail）。
    let e = parse_scalar_typed("[]", TypeTag::List).unwrap_err();
    assert!(e.contains("list"), "{e}");
}

#[test]
fn parse_scalar_inferred_literals() {
    use ScalarValue as V;
    assert_eq!(parse_scalar_inferred("true"), V::Bool(true));
    assert_eq!(parse_scalar_inferred("null"), V::Null);
    assert_eq!(parse_scalar_inferred("42"), V::UInt(42));
    assert_eq!(parse_scalar_inferred("-1"), V::Int(-1));
    assert_eq!(parse_scalar_inferred("hex:00ff"), V::Bytes(vec![0, 0xff]));
    assert_eq!(parse_scalar_inferred("foo"), V::Str("foo".into()));
}
```

- [ ] **Step 2: 失敗を確認**

```bash
cargo test -p mat-core parse_scalar 2>&1 | tail -5   # コンパイルエラー（未定義）
```

- [ ] **Step 3: 実装**

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue { Bool(bool), UInt(u64), Int(i64), Str(String), Bytes(Vec<u8>), Null }

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let h = s.strip_prefix("hex:").ok_or("bytes value must use hex: prefix")?;
    if h.len() % 2 != 0 {
        return Err(format!("odd-length hex literal: {s:?}"));
    }
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16)
            .map_err(|_| format!("invalid hex literal: {s:?}")))
        .collect()
}

pub fn parse_scalar_typed(input: &str, ty: TypeTag) -> Result<ScalarValue, String> {
    let s = input.trim();
    if s == "null" {
        return Ok(ScalarValue::Null); // nullable 属性の消去 write。
    }
    match ty {
        TypeTag::Bool => match s {
            "true" | "1" => Ok(ScalarValue::Bool(true)),
            "false" | "0" => Ok(ScalarValue::Bool(false)),
            _ => Err(format!("not a bool literal: {s:?}")),
        },
        TypeTag::UInt => parse_num(s)
            .map(ScalarValue::UInt)
            .ok_or(format!("not an unsigned integer: {s:?}")),
        TypeTag::Int => s.parse::<i64>()
            .map(ScalarValue::Int)
            .map_err(|_| format!("not an integer: {s:?}")),
        TypeTag::Str => Ok(ScalarValue::Str(s.to_string())),
        TypeTag::Bytes => parse_hex_bytes(s).map(ScalarValue::Bytes),
        TypeTag::List => Err(
            "this attribute is a list type; generic native write supports scalars only (M8a)"
                .into()),
        TypeTag::Struct => Err(
            "this attribute is a struct type; generic native write supports scalars only (M8a)"
                .into()),
        TypeTag::Float => Err(
            "float attributes are not supported by generic native write (M8a)".into()),
        TypeTag::Unknown => Err("attribute type unknown; cannot encode value".into()),
    }
}

pub fn parse_scalar_inferred(input: &str) -> ScalarValue {
    let s = input.trim();
    match s {
        "true" => return ScalarValue::Bool(true),
        "false" => return ScalarValue::Bool(false),
        "null" => return ScalarValue::Null,
        _ => {}
    }
    if let Ok(b) = parse_hex_bytes(s) {
        return ScalarValue::Bytes(b);
    }
    if let Some(u) = parse_num(s) {
        return ScalarValue::UInt(u);
    }
    if let Ok(i) = s.parse::<i64>() {
        return ScalarValue::Int(i);
    }
    ScalarValue::Str(s.to_string())
}
```

- [ ] **Step 4: テスト通過を確認 + Commit**

```bash
cargo test -p mat-core 2>&1 | tail -3 && cargo clippy -p mat-core -- -D warnings && cargo fmt
git add crates/mat-core/src/ids.rs
git commit -m "feat(mat-core): CLI文字列→スカラー値変換（typed/inferred、非スカラーは理由付き拒否） (M8a Task3)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: IM 汎用デコード（TLV→JSON + 複数IB/チャンク/リスト追記対応 ReportData）

**Files:**
- Modify: `crates/mat-controller/src/im.rs`
- Modify: `crates/mat-controller/Cargo.toml`（`serde_json` を dependencies へ — dev-deps に既にあれば移動）
- Test: `im.rs` の `#[cfg(test)]`

**Interfaces:**
- Consumes: `tlv::{Reader, Writer, Tag, Value}`（既存）
- Produces:

```rust
/// 1 AttributeReportIB のデコード結果（汎用形）。
pub struct AttributeReport {
    pub endpoint: Option<u16>,
    pub attribute: Option<u32>,
    /// path に ListIndex(null) があれば true（チャンク化 list の item 追記）。
    pub list_append: bool,
    /// AttributeDataIB の Data 要素を JSON 化したもの（status レポートなら None）。
    pub data: Option<serde_json::Value>,
    pub status: Option<u8>,
}
pub struct ReportDataMessage {
    pub reports: Vec<AttributeReport>,
    pub more_chunks: bool,
    pub suppress_response: bool,
}
pub fn decode_report_data_message(payload: &[u8]) -> Result<ReportDataMessage, ImError>;

/// wildcard read（cluster 内全属性）: AttributePathIB から attribute を省略。
pub fn encode_read_request_cluster(endpoint: u16, cluster: u32) -> Vec<u8>;

/// 複数メッセージ・リスト追記を統合し attribute id → JSON 値へ。
pub fn merge_reports(msgs: &[ReportDataMessage]) -> Vec<(u32, serde_json::Value)>;
```

JSON 化規約（ここで固定。read の value / diag のテーブルがこの形になる）:
Bool→bool, Uint/Int→number, F32/F64→number, Utf8→string, Bytes→**小文字hex文字列**, Null→null, Array/List→JSON array, Struct→JSON object（キーは **context tag 番号の10進文字列**。名前付けは上位層の責務 — diag は Task 8 で固定マップを適用）。

- [ ] **Step 1: 失敗するテストを書く**（`im.rs` tests へ追記）

```rust
#[test]
fn decode_report_data_message_multiple_ibs_and_types() {
    // ReportData { 1: [ AttrReport{1: Data{1: path(ep,cl,attr), 2: data}},
    //                   AttrReport{...} ], 4: suppress }
    // を Writer で組み、bool と list-of-struct の 2 属性が JSON になること。
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(1)); // AttributeReports
    // 属性1: on-off = true
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(1)); // AttributeDataIB
    w.put_uint(Tag::Context(0), 1); // DataVersion
    w.start_list(Tag::Context(1)); // AttributePathIB
    w.put_uint(Tag::Context(2), 1); // endpoint
    w.put_uint(Tag::Context(3), 0x0006);
    w.put_uint(Tag::Context(4), 0x0000);
    w.end_container();
    w.put_bool(Tag::Context(2), true); // Data
    w.end_container();
    w.end_container();
    // 属性2: 構造体1件のリスト
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(1));
    w.start_list(Tag::Context(1));
    w.put_uint(Tag::Context(2), 0);
    w.put_uint(Tag::Context(3), 0x0035);
    w.put_uint(Tag::Context(4), 0x0007); // neighbor-table
    w.end_container();
    w.start_array(Tag::Context(2)); // Data: array of struct
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), 42);
    w.put_int(Tag::Context(1), -60);
    w.end_container();
    w.end_container();
    w.end_container();
    w.end_container();
    w.end_container(); // AttributeReports
    w.put_bool(Tag::Context(4), true); // SuppressResponse
    w.end_container();
    let msg = decode_report_data_message(&w.finish()).unwrap();
    assert!(msg.suppress_response);
    assert!(!msg.more_chunks);
    assert_eq!(msg.reports.len(), 2);
    assert_eq!(msg.reports[0].attribute, Some(0x0000));
    assert_eq!(msg.reports[0].data, Some(serde_json::json!(true)));
    assert_eq!(msg.reports[1].attribute, Some(0x0007));
    assert_eq!(
        msg.reports[1].data,
        Some(serde_json::json!([{"0": 42, "1": -60}]))
    );
}

#[test]
fn merge_reports_joins_chunked_list_appends() {
    // msg1: neighbor-table = []（Replace）+ more_chunks
    // msg2: ListIndex null の追記 IB × 2
    // → 統合結果は 2 要素の array。
    fn path(w: &mut Writer, attr: u32, append: bool) {
        w.start_list(Tag::Context(1));
        w.put_uint(Tag::Context(2), 0);
        w.put_uint(Tag::Context(3), 0x0035);
        w.put_uint(Tag::Context(4), attr);
        if append {
            w.put_null(Tag::Context(5)); // ListIndex = null → 追記
        }
        w.end_container();
    }
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(1));
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(1));
    path(&mut w, 0x0007, false);
    w.start_array(Tag::Context(2));
    w.end_container(); // 空 array（replace）
    w.end_container();
    w.end_container();
    w.end_container();
    w.put_bool(Tag::Context(3), true); // MoreChunkedMessages
    w.end_container();
    let m1 = decode_report_data_message(&w.finish()).unwrap();
    assert!(m1.more_chunks);

    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(1));
    for v in [7u64, 8u64] {
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1));
        path(&mut w, 0x0007, true);
        w.put_uint(Tag::Context(2), v); // Data = list item
        w.end_container();
        w.end_container();
    }
    w.end_container();
    w.end_container();
    let m2 = decode_report_data_message(&w.finish()).unwrap();
    assert_eq!(m2.reports.len(), 2);
    assert!(m2.reports[0].list_append);

    let merged = merge_reports(&[m1, m2]);
    assert_eq!(merged, vec![(0x0007, serde_json::json!([7, 8]))]);
}

#[test]
fn encode_read_request_cluster_omits_attribute() {
    let b = encode_read_request_cluster(1, 0x0035);
    let mut r = Reader::new(&b);
    let mut saw_attr_tag = false;
    while let Some(el) = r.next().unwrap() {
        if el.tag == Tag::Context(4) {
            saw_attr_tag = true;
        }
    }
    assert!(!saw_attr_tag, "wildcard read must omit the attribute path field");
}
```

- [ ] **Step 2: 失敗を確認**

```bash
cargo test -p mat-controller decode_report_data_message 2>&1 | tail -5
```

- [ ] **Step 3: 実装**

`im.rs` へ追加。要点:

```rust
/// TLV 単一要素（コンテナ含む）を JSON へ。`first` は既に読んだ先頭要素。
fn tlv_element_to_json(r: &mut Reader, first: Element) -> Result<serde_json::Value, ImError> {
    use serde_json::Value as J;
    Ok(match first.value {
        Value::Bool(b) => J::Bool(b),
        Value::Uint(u) => J::from(u),
        Value::Int(i) => J::from(i),
        Value::F32(f) => serde_json::json!(f),
        Value::F64(f) => serde_json::json!(f),
        Value::Utf8(s) => J::String(s.to_string()),
        Value::Bytes(b) => J::String(hex_lower(b)),
        Value::Null => J::Null,
        Value::ArrayStart | Value::ListStart => {
            let mut items = Vec::new();
            loop {
                let el = r.next()?.ok_or(ImError::Malformed("truncated array"))?;
                if el.value == Value::ContainerEnd {
                    break;
                }
                items.push(tlv_element_to_json(r, el)?);
            }
            J::Array(items)
        }
        Value::StructStart => {
            let mut map = serde_json::Map::new();
            loop {
                let el = r.next()?.ok_or(ImError::Malformed("truncated struct"))?;
                if el.value == Value::ContainerEnd {
                    break;
                }
                let key = match el.tag {
                    Tag::Context(n) => n.to_string(),
                    _ => continue, // 想定外タグはスキップ（前方互換）
                };
                map.insert(key, tlv_element_to_json(r, el)?);
            }
            J::Object(map)
        }
        Value::ContainerEnd => return Err(ImError::Malformed("dangling container end")),
    })
}

fn hex_lower(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
```

`decode_report_data_message` は既存 `decode_report_data` と同じ走査骨格で:
- 外側 struct → `Context(1)`: AttributeReports array を反復。各 anonymous struct 内で `Context(0)`=AttributeStatusIB（既存 `decode_attribute_status_ib` を path も拾う形に拡張 or status IB 用に path デコードを追加）/ `Context(1)`=AttributeDataIB。
- AttributeDataIB 内: `Context(1)`（path list）から endpoint(2)/attribute(4)/ListIndex(5、`Value::Null` なら `list_append=true`) を読む。`Context(2)` が来たら `tlv_element_to_json` で JSON 化。
- 外側の `Context(3)` = MoreChunkedMessages(bool)、`Context(4)` = SuppressResponse(bool)。
- 既存 `decode_report_data`（M2 API）は**そのまま残す**（既存呼び出し・テスト無改変）。

`merge_reports`:

```rust
pub fn merge_reports(msgs: &[ReportDataMessage]) -> Vec<(u32, serde_json::Value)> {
    let mut order: Vec<u32> = Vec::new();
    let mut map: std::collections::HashMap<u32, serde_json::Value> = Default::default();
    for m in msgs {
        for r in &m.reports {
            let Some(attr) = r.attribute else { continue };
            let Some(data) = r.data.clone() else { continue }; // status-only は値なし
            if r.list_append {
                match map.entry(attr).or_insert_with(|| serde_json::json!([])) {
                    serde_json::Value::Array(items) => items.push(data),
                    slot => *slot = serde_json::json!([data]), // 追記が先に来た異常系
                }
            } else {
                if !map.contains_key(&attr) {
                    order.push(attr);
                }
                map.insert(attr, data);
            }
            if !order.contains(&attr) {
                order.push(attr);
            }
        }
    }
    order.into_iter().filter_map(|a| map.remove(&a).map(|v| (a, v))).collect()
}
```

`encode_read_request_cluster` は `encode_read_request` のコピーで `Context(4)` の put を省くだけ。

- [ ] **Step 4: テスト全通過 + Commit**

```bash
cargo test -p mat-controller 2>&1 | tail -3 && cargo clippy -p mat-controller -- -D warnings && cargo fmt
git add crates/mat-controller/src/im.rs crates/mat-controller/Cargo.toml
git commit -m "feat(mat-controller): IM汎用デコード — TLV→JSON・複数IB・チャンク・リスト追記統合 (M8a Task4)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: IM Write + セッション層の汎用 read/write

**Files:**
- Modify: `crates/mat-controller/src/im.rs`（WriteRequest/WriteResponse）
- Modify: `crates/mat-controller/src/session.rs`（`write_attribute` / `read_attribute_json` / `read_cluster_json`）
- Test: 両ファイルの `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 4 の `decode_report_data_message` / `merge_reports` / `encode_read_request_cluster`、既存 `ImValue`・`encode_timed_request`・`send_reliable`/`recv`。
- Produces:

```rust
// im.rs
/// WriteRequestMessage (spec §8.9.2.4)。data_tlv は Data 要素 1 個の完全な TLV
/// （トップレベルタグは何でもよい — Context(2) に書き直して埋め込む）。
pub fn encode_write_request_tlv(endpoint: u16, cluster: u32, attribute: u32,
                                data_tlv: &[u8]) -> Vec<u8>;
/// スカラー用の糖衣: ImValue を TLV 化して encode_write_request_tlv に渡す。
pub fn encode_write_request(endpoint: u16, cluster: u32, attribute: u32,
                            value: &ImValue) -> Vec<u8>;
/// WriteResponseMessage から最初の AttributeStatusIB の status を返す。
pub fn decode_write_response(payload: &[u8]) -> Result<u8, ImError>;
pub const OPCODE_WRITE_REQUEST: u8 = 0x06;
pub const OPCODE_WRITE_RESPONSE: u8 = 0x07;

// session.rs
impl SecureSession {
    /// 属性 write。status != 0 は ImError::AttributeStatus で返す。
    /// timed_ms Some なら TimedRequest → StatusResponse(0) → WriteRequest。
    pub async fn write_attribute_tlv(&mut self, endpoint: u16, cluster: u32,
        attribute: u32, data_tlv: &[u8], timed_ms: Option<u16>, cfg: &MrpConfig)
        -> Result<(), SessionError>;
    /// 単一属性 read の汎用版（チャンク対応、値は JSON）。
    pub async fn read_attribute_json(&mut self, endpoint: u16, cluster: u32,
        attribute: u32, cfg: &MrpConfig) -> Result<serde_json::Value, SessionError>;
    /// cluster 内全属性の wildcard read（チャンク対応）。(attribute_id, JSON) の列。
    pub async fn read_cluster_json(&mut self, endpoint: u16, cluster: u32,
        cfg: &MrpConfig) -> Result<Vec<(u32, serde_json::Value)>, SessionError>;
}
```

- [ ] **Step 1: im の失敗するテストを書く**

```rust
#[test]
fn write_request_roundtrip_scalar() {
    let b = encode_write_request(1, 0x0008, 0x0011, &ImValue::Uint(128));
    // 形の検証: WriteRequests(2) 配列の中に AttributeDataIB があり、
    // path(ep=1, cluster=8, attr=0x11) と Data(Context2)=128 を含む。
    let mut r = Reader::new(&b);
    let (mut saw_ep, mut saw_data) = (false, false);
    while let Some(el) = r.next().unwrap() {
        if el.tag == Tag::Context(2) && el.value == Value::Uint(128) {
            saw_data = true;
        }
        if el.tag == Tag::Context(2) && el.value == Value::Uint(1) {
            saw_ep = true;
        }
    }
    assert!(saw_ep && saw_data);
}

#[test]
fn decode_write_response_returns_first_status() {
    // WriteResponse { 0: [ AttrStatusIB{0: path, 1: StatusIB{0: 0}} ] }
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(0));
    w.start_struct(Tag::Anonymous);
    w.start_list(Tag::Context(0)); // path
    w.end_container();
    w.start_struct(Tag::Context(1)); // StatusIB
    w.put_uint(Tag::Context(0), 0);
    w.end_container();
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    assert_eq!(decode_write_response(&w.finish()).unwrap(), 0);
}
```

- [ ] **Step 2: im 実装**

`encode_write_request_tlv`（spec §8.9.2.4 WriteRequestMessage）:

```rust
pub const OPCODE_WRITE_REQUEST: u8 = 0x06;
pub const OPCODE_WRITE_RESPONSE: u8 = 0x07;

pub fn encode_write_request_tlv(
    endpoint: u16, cluster: u32, attribute: u32, data_tlv: &[u8],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false); // SuppressResponse
    w.put_bool(Tag::Context(1), false); // TimedRequest フラグは caller が timed 時 true に
    w.start_array(Tag::Context(2)); // WriteRequests
    w.start_struct(Tag::Anonymous); // AttributeDataIB
    w.start_list(Tag::Context(1)); // AttributePathIB
    w.put_uint(Tag::Context(2), u64::from(endpoint));
    w.put_uint(Tag::Context(3), u64::from(cluster));
    w.put_uint(Tag::Context(4), u64::from(attribute));
    w.end_container();
    w.put_raw_element(Tag::Context(2), data_tlv); // Data（下記ヘルパ）
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}
```

注意: `Writer` に「既存 TLV 要素のタグを書き換えて埋め込む」ヘルパが必要。`tlv.rs` に追加する:

```rust
/// 完全な TLV 要素 1 個（element）のタグを `tag` に書き換えて埋め込む。
/// element の中身はコピーで、control byte のタグ形式だけ差し替える。
pub fn put_raw_element(&mut self, tag: Tag, element: &[u8]) { ... }
```

実装は `Reader` で element の先頭 control byte（上位3bit がタグ形式）を読み、素の値部分を新しい control+tag で書き直す。**コンテナの場合は先頭要素のタグのみ差し替え、残りバイト列は verbatim コピー**（`Reader` を使って先頭要素のヘッダ長を求める）。既存 `InvokeResponseData::fields_tlv` の「トップレベルタグを Anonymous に書き直す」処理（`im.rs` 内にある — 検索: `Tag::Anonymous` 書き換え）と同じ要領なので、その実装を関数化して共有すること。

timed フラグ: `write_attribute_tlv` の timed 経路では `Context(1)` を true にした WriteRequest が要る。`encode_write_request_tlv` に `timed: bool` 引数を足すか、2 関数に分けるかは実装者判断（既存 `encode_invoke_request` / `encode_invoke_request_timed` の対にならい **2 関数**を推奨）。

`decode_write_response`: 外側 struct → `Context(0)` array → 最初の anonymous struct 内 `Context(1)`(StatusIB) の `Context(0)`。既存 `decode_attribute_status_ib` を流用。

- [ ] **Step 3: session 実装**

`read_attribute_json` / `read_cluster_json`（チャンクループ共通ヘルパ `collect_reports`）:

```rust
async fn collect_reports(&mut self, exchange_id: u16, first: crate::message::Received,
    cfg: &MrpConfig) -> Result<Vec<crate::im::ReportDataMessage>, SessionError> {
    use crate::im;
    let mut msgs = Vec::new();
    let mut msg = first;
    loop {
        match msg.proto.opcode {
            im::OPCODE_REPORT_DATA => {
                let rd = im::decode_report_data_message(&msg.payload)
                    .map_err(SessionError::Im)?;
                let more = rd.more_chunks;
                let suppress = rd.suppress_response;
                msgs.push(rd);
                if more {
                    // チャンク継続: StatusResponse(0) で次チャンクを促す。
                    let ok = im::encode_status_response(0);
                    let resp = self.send_reliable(exchange_id, im::PROTOCOL_ID_IM,
                        im::OPCODE_STATUS_RESPONSE, &ok, cfg).await?;
                    msg = match resp {
                        Some(m) => m,
                        None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
                    };
                    continue;
                }
                if !suppress {
                    // 最終チャンクを閉じる StatusResponse（read_attribute と同じ
                    // best-effort — データは手元にあるので失敗は無視）。
                    let ok = im::encode_status_response(0);
                    let _ = self.send_reliable(exchange_id, im::PROTOCOL_ID_IM,
                        im::OPCODE_STATUS_RESPONSE, &ok, cfg).await;
                }
                return Ok(msgs);
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload)
                    .map_err(SessionError::Im)?;
                return Err(SessionError::Im(im::ImError::StatusResponse(s)));
            }
            op => return Err(SessionError::UnexpectedOpcode(op)),
        }
    }
}
```

`read_attribute_json`: 既存 `read_attribute` と同じ送信部（`encode_read_request`）→ `collect_reports` → `merge_reports` → 単一属性なので先頭タプルの JSON を返す（reports が status のみなら `ImError::AttributeStatus`）。
`read_cluster_json`: `encode_read_request_cluster` → 同様 → `merge_reports` の全結果を返す。
`write_attribute_tlv`: timed_ms Some なら `encode_timed_request(timeout)` を送り StatusResponse(0) を確認（既存 `invoke_for_data` の timed 前段と同じ — その処理を関数化して共有すること）→ WriteRequest（timed フラグ true）→ WriteResponse を `decode_write_response` し `status != 0` なら `Err(SessionError::Im(ImError::AttributeStatus(status)))`。

session のテストは既存の session テスト方式（同ファイル `#[cfg(test)]` にモックリンクのペアで実デバイス役を書く形）に倣う。まず `session.rs` の tests mod を読み、既存の「セッション確立済みペアを作るヘルパ」を特定して流用する。**次の 3 本を書く**（デバイス役はヘルパのペア側 session で受信 → 固定応答を送るタスクとして書く）:

```rust
#[tokio::test]
async fn write_attribute_reports_status_zero_as_ok() {
    // デバイス役: WriteRequest 受信 → decode_write_response のテストで組んだのと
    // 同じ WriteResponse(status=0) を返す。
    // 検証: write_attribute_tlv(..., None, ...) が Ok(())。
}

#[tokio::test]
async fn write_attribute_maps_nonzero_status_to_attribute_status_error() {
    // デバイス役: WriteResponse(status=0x87 CONSTRAINT_ERROR) を返す。
    // 検証: Err(SessionError::Im(ImError::AttributeStatus(0x87)))。
}

#[tokio::test]
async fn read_cluster_json_merges_two_chunks() {
    // デバイス役: ReportData{attr A, MoreChunkedMessages=true} → StatusResponse(0)
    // を受けてから ReportData{attr B（list_append 2件）, more=false} を返す。
    // 検証: read_cluster_json が [(A, ..), (B, [..2件..])] を返す。
    // TLV の組み立ては Task 4 のテスト（merge_reports_joins_chunked_list_appends）
    // の Writer コードをヘルパ化して共有する。
}
```

- [ ] **Step 4: テスト・回帰確認 + Commit**

```bash
cargo test -p mat-controller 2>&1 | tail -3
cargo clippy -p mat-controller -- -D warnings && cargo fmt
git add crates/mat-controller/src/im.rs crates/mat-controller/src/session.rs crates/mat-controller/src/tlv.rs
git commit -m "feat(mat-controller): IM Write + wildcard/チャンク対応セッションread/write (M8a Task5)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: mat-native NodeConn 拡張（汎用 read/write/invoke + fake）

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（trait + SessionConn 実装）
- Modify: `crates/mat-native/src/test_support.rs`（FakeEstablisher/FakeConn 拡張）
- Modify: `crates/matd/src/native.rs`（`invoke` 呼び出しに timed 引数追加の追従のみ）
- Modify: `crates/mat/src/native_direct.rs`（同上の追従のみ）
- Test: `crates/mat-native/src/lib.rs` の `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 5 のセッション API、`mat_core::ids::ScalarValue`。
- Produces（trait 拡張。**既存メソッドの意味は不変**、`invoke` に `timed: bool` を追加）:

```rust
#[async_trait]
pub trait NodeConn: Send {
    async fn read_onoff(&mut self, endpoint: u16) -> Result<bool, MatError>;
    async fn invoke(&mut self, endpoint: u16, cluster: u32, command: u32,
        fields: Option<Vec<u8>>, timed: bool) -> Result<(), MatError>;
    async fn read_json(&mut self, endpoint: u16, cluster: u32, attribute: u32)
        -> Result<serde_json::Value, MatError>;
    async fn read_cluster(&mut self, endpoint: u16, cluster: u32)
        -> Result<Vec<(u32, serde_json::Value)>, MatError>;
    async fn write_tlv(&mut self, endpoint: u16, cluster: u32, attribute: u32,
        data_tlv: Vec<u8>, timed: bool) -> Result<(), MatError>;
}

/// ScalarValue → ImValue 写像（mat-core は mat-controller に依存しないため
/// ここで変換する）。
pub fn scalar_to_im(v: &mat_core::ids::ScalarValue) -> mat_controller::im::ImValue;
/// ScalarValue を Data TLV 要素へ（Anonymous タグ、write_tlv に渡す形）。
pub fn scalar_to_tlv(v: &mat_core::ids::ScalarValue) -> Vec<u8>;
```

- [ ] **Step 1: 失敗するテストを書く**（fake 経由で新メソッドの呼び出し形を固定）

```rust
#[tokio::test]
async fn generic_read_write_via_fake() {
    use mat_native::test_support::FakeEstablisher;
    let engine = Engine::with_parts(Box::new(FakeEstablisher::default()), None);
    let mut conn = engine.establisher.establish(5).await.unwrap();
    // fake は read_json に固定値を返す（test_support 拡張で定義）。
    let v = conn.read_json(1, 0x0008, 0x0000).await.unwrap();
    assert!(v.is_number());
    conn.write_tlv(1, 0x0008, 0x0011,
        scalar_to_tlv(&mat_core::ids::ScalarValue::UInt(128)), false)
        .await
        .unwrap();
    let all = conn.read_cluster(1, 0x0006).await.unwrap();
    assert!(!all.is_empty());
}

#[test]
fn scalar_conversions() {
    use mat_core::ids::ScalarValue as S;
    use mat_controller::im::ImValue;
    assert_eq!(scalar_to_im(&S::Bool(true)), ImValue::Bool(true));
    assert_eq!(scalar_to_im(&S::UInt(7)), ImValue::Uint(7));
    // scalar_to_tlv は Reader で読み戻して値一致を確認。
    let b = scalar_to_tlv(&S::Str("x".into()));
    let mut r = mat_controller::tlv::Reader::new(&b);
    assert!(matches!(r.next().unwrap().unwrap().value,
        mat_controller::tlv::Value::Utf8("x")));
}
```

- [ ] **Step 2: 実装**

- `SessionConn` に新 3 メソッド実装（`read_attribute_json` / `read_cluster_json` / `write_attribute_tlv` を呼び、`map_session_err` で写像）。timed は `Some(10_000)`（open-window の既存値と同じ 10 秒）に固定。
- `invoke` の `timed: bool` 追加: SessionConn は timed=true のとき `invoke_for_data(..., Some(10_000), ...)` 相当（既存 `SecureSession::invoke` に timed 版が無い場合は `invoke_for_data` を使い status!=0 を `ImError::CommandStatus` エラーに写像 — 既存 `invoke` の挙動と揃える）。
- 既存呼び出し元の追従: `matd/src/native.rs`（4箇所）と `mat/src/native_direct.rs`（run_op 内 5箇所）の `conn.invoke(...)` / `c.invoke(...)` に `, false` を追加。
- `test_support.rs` の Fake: 既存 FakeConn に新メソッドを足す（read_json → `json!(1)`、read_cluster → `vec![(0u32, json!(true))]`、write_tlv → 既存 `fail_first_send` 尊重で Ok(())）。
- `scalar_to_im` / `scalar_to_tlv` は素直な match（`Writer::new()` + put_* + finish）。

- [ ] **Step 3: 回帰確認 + Commit**

```bash
cargo test -p mat-native -p matd -p mat 2>&1 | tail -3   # 既存テスト無改変で全通過
cargo clippy --workspace -- -D warnings && cargo fmt
git add crates/mat-native crates/matd/src/native.rs crates/mat/src/native_direct.rs
git commit -m "feat(mat-native): NodeConn拡張 — 汎用read/write/invoke(timed)とfake (M8a Task6)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: mat 直経路 — 汎用 read / write / invoke の native 化

**Files:**
- Modify: `crates/mat/src/native_direct.rs`（classify + run_op）
- Modify: `crates/mat/src/commands/write.rs`（emit を pub(crate) ヘルパへ抽出）
- Test: `native_direct.rs` の `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 2/3 の `ids::*`、Task 6 の NodeConn 新メソッド、既存 `emit_read_success` / `emit_invoke_success`。
- Produces: `NativeOp` 追加バリアント:

```rust
ReadAttr { node_id: u64, endpoint: u16, cluster_in: String, attribute_in: String,
           cluster: u32, attribute: u32 },
WriteAttr { node_id: u64, endpoint: u16, cluster_in: String, attribute_in: String,
            cluster: u32, attribute: u32, value_in: String,
            value: mat_core::ids::ScalarValue, timed: bool },
InvokeGeneric { node_id: u64, endpoint: u16, cluster_in: String, command_in: String,
                cluster: u32, command: u32, fields_tlv: Option<Vec<u8>>, timed: bool },
```

分類規則（**この順で判定**。M7 の on/off/color 系の分岐は現状のまま先に評価される）:
1. `Command::Read`: `resolve_cluster` + `resolve_attribute` が両方通れば `ReadAttr`（onoff/on-off は従来どおり `ReadOnOff` が先にマッチ）。どちらか解決不能 → `None`（chip-tool 互換路）。
2. `Command::Write`: 解決可 + `parse_scalar_typed`（def あり）/`parse_scalar_inferred`（数値直指定）が Ok → `WriteAttr`（timed = `def.timed_write`）。**解決可だが値が非スカラー型で Err → classify は Some を返し、実行前に `parse_error` 即返し**（下記 Step 2 の `classify_strict` 参照）。解決不能 → `None`。
3. `Command::Invoke`: cluster/command 解決可、かつ (a) def あり: `args.len() <= fields.len()` で各 arg を `parse_scalar_typed(arg, fields[i].ty)`、全部 Ok → fields_tlv 構築（下記）; 非スカラー field への引数 → `parse_error`; args 過多 → `parse_error`。(b) 数値直指定（def なし）: args が空なら fields_tlv=None で native、args ありは型不明なので `None`（chip-tool へ）。

fields_tlv 構築（CommandFields struct、context tag = 引数添字）:

```rust
fn encode_command_fields(args: &[mat_core::ids::ScalarValue]) -> Vec<u8> {
    use mat_controller::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    for (i, v) in args.iter().enumerate() {
        let tag = Tag::Context(i as u8);
        match v {
            mat_core::ids::ScalarValue::Bool(b) => w.put_bool(tag, *b),
            mat_core::ids::ScalarValue::UInt(u) => w.put_uint(tag, *u),
            mat_core::ids::ScalarValue::Int(x) => w.put_int(tag, *x),
            mat_core::ids::ScalarValue::Str(s) => w.put_str(tag, s),
            mat_core::ids::ScalarValue::Bytes(b) => w.put_bytes(tag, b),
            mat_core::ids::ScalarValue::Null => w.put_null(tag),
        }
    }
    w.end_container();
    w.finish()
}
```

- [ ] **Step 1: 失敗するテストを書く**（classify のテストを既存 tests mod に追記）

```rust
#[test]
fn generic_read_is_native_when_names_resolve() {
    use mat_core::alias::{EndpointRef, NodeRef};
    let read = Command::Read {
        node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
        cluster: "levelcontrol".into(), attribute: "current-level".into(),
    };
    assert!(matches!(classify(&read), Some(NativeOp::ReadAttr { cluster: 0x0008, attribute: 0x0000, .. })));
    // 未知クラスタ名は chip-tool へ（互換）。
    let unknown = Command::Read {
        node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
        cluster: "nosuch".into(), attribute: "x".into(),
    };
    assert!(classify(&unknown).is_none());
    // 数値直指定も native。
    let byid = Command::Read {
        node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
        cluster: "0x0008".into(), attribute: "0".into(),
    };
    assert!(matches!(classify(&byid), Some(NativeOp::ReadAttr { .. })));
}

#[test]
fn write_scalar_native_and_list_rejected() {
    use mat_core::alias::{EndpointRef, NodeRef};
    let w = Command::Write {
        node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
        cluster: "levelcontrol".into(), attribute: "on-level".into(),
        value: "128".into(),
    };
    assert!(matches!(classify(&w), Some(NativeOp::WriteAttr { .. })));
    // list 型（acl）への汎用 write は parse_error（classify_strict 経由で確認）。
    let acl = Command::Write {
        node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
        cluster: "accesscontrol".into(), attribute: "acl".into(),
        value: "[]".into(),
    };
    let err = classify_strict(&acl).unwrap().unwrap_err();
    assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
    assert!(err.detail.contains("list"), "{}", err.detail);
}

#[test]
fn generic_invoke_scalar_args_native_and_bad_args_rejected() {
    use mat_core::alias::{EndpointRef, NodeRef};
    let inv = Command::Invoke {
        node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
        cluster: "levelcontrol".into(), command: "move-to-level".into(),
        args: vec!["128".into(), "0".into(), "0".into(), "0".into()],
    };
    assert!(matches!(classify(&inv), Some(NativeOp::InvokeGeneric { .. })));
    // struct field を要求するコマンド（key-set-write）への引数 → parse_error。
    let ks = Command::Invoke {
        node_id: NodeRef::Id(5), endpoint: EndpointRef::Id(1),
        cluster: "groupkeymanagement".into(), command: "key-set-write".into(),
        args: vec!["{}".into()],
    };
    let err = classify_strict(&ks).unwrap().unwrap_err();
    assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
}
```

- [ ] **Step 2: 実装**

classify の返し方を拡張する。既存 `classify(&Command) -> Option<NativeOp>` は温存し、新たに:

```rust
/// 汎用形の分類: None = 非対象（chip-tool へ）、Some(Ok) = native 実行、
/// Some(Err) = native 対象だが値が符号化不能 → 即 parse_error（spec 決定3:
/// フォールバックせず明示拒否。chip-tool なら通る形をあえて拒むのは
/// opt-in（MAT_IFACE）下の意図した縮小）。
pub(crate) fn classify_strict(command: &Command) -> Option<Result<NativeOp, MatError>>;
```

`try_run` は `classify(command)`（M7 形）→ 無ければ `classify_strict(command)` の順で見て、`Some(Err(e))` は `Some(Err(e))` をそのまま返す（main.rs はそれを emit + exit code 1 にする — ParseError の exit_code は既存どおり）。

`run_op` に 3 バリアントの実行を追加:

```rust
NativeOp::ReadAttr { node_id, endpoint, cluster_in, attribute_in, cluster, attribute } => {
    let mut conn = engine.establisher.establish(*node_id).await?;
    let v = conn.read_json(*endpoint, *cluster, *attribute).await?;
    crate::commands::read::emit_read_success(*node_id, *endpoint, cluster_in, attribute_in, v);
}
NativeOp::WriteAttr { node_id, endpoint, cluster_in, attribute_in, cluster, attribute, value_in, value, timed } => {
    let mut conn = engine.establisher.establish(*node_id).await?;
    conn.write_tlv(*endpoint, *cluster, *attribute,
        mat_native::scalar_to_tlv(value), *timed).await?;
    crate::commands::write::emit_write_success(*node_id, *endpoint, cluster_in, attribute_in, value_in);
}
NativeOp::InvokeGeneric { node_id, endpoint, cluster_in, command_in, cluster, command, fields_tlv, timed } => {
    let mut conn = engine.establisher.establish(*node_id).await?;
    conn.invoke(*endpoint, *cluster, *command, fields_tlv.clone(), *timed).await?;
    crate::commands::invoke::emit_invoke_success(*node_id, *endpoint, cluster_in, command_in);
}
```

`write.rs` から emit を抽出（chip-tool 経路と共有、スキーマ不変）:

```rust
pub(crate) fn emit_write_success(node_id: u64, endpoint: u16, cluster: &str,
                                 attribute: &str, value: &str) {
    output::emit(json!({
        "node_id": node_id, "endpoint": endpoint,
        "cluster": cluster, "attribute": attribute,
        "value": normalize_value(value),
        "status": "success",
    }));
}
```

既存 `write::run` の emit 部をこの関数呼び出しに置換（挙動不変）。

- [ ] **Step 3: テスト・回帰 + Commit**

```bash
cargo test -p mat 2>&1 | tail -3    # 新テスト + 既存統合テスト（fake-chip-tool）全green
cargo clippy -p mat -- -D warnings && cargo fmt
git add crates/mat/src/native_direct.rs crates/mat/src/commands/write.rs
git commit -m "feat(mat): 直経路native — 汎用read/write/invoke（非スカラーはparse_error明示拒否） (M8a Task7)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: mat 直経路 — describe / diag thread / open-window の native 化

**Files:**
- Create: `crates/mat-native/src/ops.rs`（describe / diag の共有ロジック）
- Modify: `crates/mat-native/src/lib.rs`（`pub mod ops;`）
- Modify: `crates/mat-controller/src/commissioning.rs`（open-window の引数拡張）
- Modify: `crates/mat/src/native_direct.rs` / `crates/mat/src/commands/describe.rs` / `diag.rs` / `open_window.rs`（emit 抽出と配線）
- Test: `ops.rs` + `native_direct.rs`

**Interfaces:**
- Produces（mat-native::ops — matd Task 10 も使う）:

```rust
/// descriptor 歩き: ep0 の parts-list → 各 ep の server-list。
/// 返り値: (endpoint, cluster id 群) の列（ep0 先頭、重複なし）。
pub async fn describe(conn: &mut dyn NodeConn) -> Result<Vec<(u16, Vec<u64>)>, MatError>;

/// Thread 診断スナップショット（cluster 0x0035 の wildcard read 1発 + 整形）。
/// 部分結果ポリシーは chip-tool 経路と同じ: 読めた属性のみ、失敗は unavailable。
pub struct ThreadSnapshot {
    pub fields: serde_json::Map<String, serde_json::Value>, // 出力キー→値(null含む)
    pub unavailable: Vec<(String, mat_core::error::ErrorKind)>, // (chip-tool属性名, kind)
}
pub async fn diag_thread(conn: &mut dyn NodeConn, endpoint: u16)
    -> Result<ThreadSnapshot, MatError>;
```

- Modify（mat-controller::commissioning、既存呼び出し元も追従）:

```rust
pub async fn open_commissioning_window(
    session: &mut SecureSession,
    timeout_s: u16,
    discriminator: u16,   // 追加: CLI 指定を尊重（従来は内部乱数）
    iterations: u32,      // 追加: CLI 指定を尊重（従来は固定 1000）
    cfg: &MrpConfig,
) -> Result<OpenedWindow, CommissionError>;
```

- [ ] **Step 1: describe / diag の失敗するテストを書く**（fake NodeConn に descriptor/診断応答を教える）

`test_support.rs` の FakeConn を拡張し、`read_json` / `read_cluster` が「呼ばれた (endpoint, cluster, attribute) に応じたプリセット」を返せる形にする（`HashMap<(u16,u32,u32), serde_json::Value>` と `HashMap<(u16,u32), Vec<(u32, serde_json::Value)>>` を Fake に持たせ、テストから流し込む）。テスト:

```rust
#[tokio::test]
async fn describe_walks_parts_and_server_lists() {
    // ep0 parts-list = [1], ep0 server-list = [29, 31], ep1 server-list = [6, 8]
    let mut conn = FakeConn::scripted()
        .with_read(0, 0x001D, 0x0003, serde_json::json!([1]))
        .with_read(0, 0x001D, 0x0001, serde_json::json!([29, 31]))
        .with_read(1, 0x001D, 0x0001, serde_json::json!([6, 8]));
    let eps = ops::describe(&mut conn).await.unwrap();
    assert_eq!(eps, vec![(0, vec![29, 31]), (1, vec![6, 8])]);
}

#[tokio::test]
async fn diag_thread_maps_names_and_partial_results() {
    // wildcard read が routing-role(1=数値), neighbor-table(structの配列) を返し、
    // network-name 等は欠けている → fields は読めた分 + null、unavailable は無し
    // （wildcard は「無い属性」を返さないだけで per-attr エラーが出ない点が
    //  chip-tool 経路と違う。全滅時のみ Err — テスト2本目で確認）。
    let mut conn = FakeConn::scripted().with_cluster(1, 0x0035, vec![
        (0x0001, serde_json::json!(3)),                       // routing-role
        (0x0007, serde_json::json!([{"0": 42, "7": -60}])),   // neighbor-table
    ]);
    let snap = ops::diag_thread(&mut conn, 1).await.unwrap();
    assert_eq!(snap.fields["routing_role"], serde_json::json!(3));
    // struct キーがフィールド名へ改名されていること（chip-tool ログ互換名）。
    let nt = snap.fields["neighbor_table"].as_array().unwrap();
    assert!(nt[0].get("ExtAddress").is_some() || nt[0].get("Age").is_some(),
        "field-id keys must be renamed: {nt:?}");
    // 返らなかった属性は null。
    assert_eq!(snap.fields["network_name"], serde_json::Value::Null);
}
```

- [ ] **Step 2: ops.rs 実装**

- `describe`: `read_json(0, 0x001D, 0x0003)`（parts-list）→ 配列を u16 化 → ep0 を先頭に重複排除 → 各 ep で `read_json(ep, 0x001D, 0x0001)`（server-list）→ u64 配列。JSON が配列でない・数値でない要素はスキップ（chip-tool 経路の `parse_id_list` と同じ寛容さ）。
- `diag_thread`: `read_cluster(endpoint, 0x0035)` 1発 → attribute id → 出力キーへ写像。スカラー 6 種（routing_role=0x0001? — **注意: 属性 ID は ids テーブルで引く**こと。`ids::resolve_attribute(0x0035, "routing-role")` 等で ID を取得し、ハードコードしない）+ テーブル 2 種。テーブルの struct フィールド名改名マップは cluster 53 の 2 struct のみ**この場で定数定義**する。**フィールド名・field id の正は既存 fake-chip-tool フィクスチャ / `parse_struct_list` のテストデータ**（`crates/mat-core/src/parse.rs` の tests と `tests/` の fake chip-tool 出力を読んで、chip-tool が出す表記 — 例 "ExtAddress", "Rloc16", "Lqi" — と一致させる。ここが describe/diag のスキーマ回帰の勘所）。wildcard で返らなかった出力キーは `Value::Null`。**read_cluster 自体が失敗**（不達等）なら Err をそのまま返す（chip-tool 経路の「全滅時は最初の失敗 kind を伝播」と同義になる）。
- 注: wildcard read は per-attribute の失敗が出ない（デバイスは持っている属性だけ返す）ため、`unavailable` は native 経路では通常空。スキーマ上 `unavailable` キーは「あれば出す」なので互換（diag.rs の emit は `!unavailable.is_empty()` ガード済み）。

- [ ] **Step 3: open-window 拡張 + CLI 配線**

- `commissioning.rs`: 上記シグネチャに変更し、乱数 discriminator / 固定 iterations=1000 を引数使用に置換。既存呼び出し元（`grep -rn open_commissioning_window crates/` — M6b ハーネス等）は `disc` 乱数生成・`1000` を渡す形に追従（挙動不変）。
- `open_window.rs`: emit を抽出:

```rust
pub(crate) fn emit_open_window_success(node_id: u64, manual_code: &str,
                                       qr_payload: &str, timeout: u32) {
    output::emit(json!({
        "node_id": node_id,
        "manual_code": manual_code,
        "qr_payload": qr_payload,
        "expires_at": output::expires_in(i64::from(timeout)),
    }));
}
```

- `native_direct.rs`: バリアント追加

```rust
Describe { node_id: u64 },
DiagThread { node_id: u64, endpoint: u16 },
OpenWindow { node_id: u64, timeout: u32, iteration: u32, discriminator: u16 },
```

classify: `Command::Describe` / `Command::Diag { action: DiagCommand::Thread {..} }` / `Command::OpenWindow`（discriminator 未指定の補完 `node_id % 4096` は main.rs と同じ式をここでも適用 — main.rs の該当行を関数化して共有すること）。`Diag Node` は対象外（probe 混在のため chip-tool 経路のまま。M8b/M8c で再訪）。
run_op: describe → `ops::describe` → `commands::describe` から抽出した emit（`{"node_id", "endpoints": [{"endpoint", "clusters"}]}` — 既存 run の emit 部を `pub(crate) fn emit_describe_success(node_id, endpoints: &[(u16, Vec<u64>)])` に抽出して両経路共有）。diag → `ops::diag_thread` → `diag.rs` の emit 部も同様に抽出（`thread` map + `unavailable` + 全滅時エラーの組み立てを関数化）。open-window → establish → `open_commissioning_window(&mut session, timeout as u16, discriminator, iteration, ...)` → emit。

**注意**: `NodeConn` は `SecureSession` を隠蔽しているため open-window は `NodeConn` 経由で呼べない。`NodeConn` に

```rust
async fn open_window(&mut self, timeout_s: u16, discriminator: u16, iterations: u32)
    -> Result<(String, String), MatError>; // (manual_code, qr_payload)
```

を追加し、SessionConn 実装で `open_commissioning_window` を呼ぶ（fake は固定文字列）。timeout の u32→u16 は `min(u16::MAX)` で飽和。

- [ ] **Step 4: テスト・回帰 + Commit**

```bash
cargo test --workspace 2>&1 | tail -3
cargo clippy --workspace -- -D warnings && cargo fmt
git add crates/mat-native crates/mat-controller/src/commissioning.rs crates/mat/src
git commit -m "feat(mat): 直経路native — describe/diag thread/open-window (M8a Task8)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 9: group provision / grant のデバイス側 native 化

**Files:**
- Modify: `crates/mat-native/src/ops.rs`（provision デバイス側ステップ + ACL read-merge-write）
- Modify: `crates/mat-core/src/acl.rs`（IM JSON ⇄ AclEntry 変換）
- Modify: `crates/mat-controller/src/im.rs`（KeySetWrite / GroupKeyMap / AddGroup の専用エンコーダ）
- Modify: `crates/mat/src/native_direct.rs` + `crates/mat/src/commands/group.rs`（配線・共有化）
- Test: 各ファイル

**Interfaces:**
- Produces（mat-controller::im — 形が固定の専用エンコーダ、spec 決定3）:

```rust
pub const CLUSTER_GROUP_KEY_MANAGEMENT: u32 = 0x003F;
pub const CMD_KEY_SET_WRITE: u32 = 0x00;
pub const CLUSTER_GROUPS: u32 = 0x0004;
pub const CMD_ADD_GROUP: u32 = 0x00;
pub const CLUSTER_ACCESS_CONTROL: u32 = 0x001F;
pub const ATTR_ACL: u32 = 0x0000;
pub const ATTR_GROUP_KEY_MAP: u32 = 0x0000;

/// KeySetWrite の CommandFields。GroupKeySetStruct（field 0）:
/// {0: groupKeySetID, 1: securityPolicy(0), 2: epochKey0(16B), 3: epochStartTime0(1),
///  4..7: epochKey1/StartTime1/epochKey2/StartTime2 = null}
pub fn encode_key_set_write_fields(keyset_id: u16, epoch_key: &[u8; 16]) -> Vec<u8>;
/// group-key-map 属性（list of GroupKeyMapStruct {1: groupId, 2: groupKeySetID}）
/// の Data TLV（write_tlv に渡す形）。書込は全置換なので既存 map とのマージが要る
/// — 引数は最終形のリスト。
pub fn encode_group_key_map_tlv(entries: &[(u16, u16)]) -> Vec<u8>;
/// AddGroup の CommandFields {0: groupId, 1: groupName}
pub fn encode_add_group_fields(group_id: u16, name: &str) -> Vec<u8>;
```

- Produces（mat-core::acl）:

```rust
/// tlv_to_json の数値キー struct（AccessControlEntryStruct:
/// {1: privilege, 2: authMode, 3: subjects, 4: targets, 254: fabricIndex}）から
/// AclEntry 列へ。解釈不能なら Err（read できなければ絶対に write しない既存方針）。
pub fn entries_from_im_json(v: &serde_json::Value) -> Result<Vec<AclEntry>, MatError>;
```

- Produces（mat-native::ops）:

```rust
pub struct ProvisionNodeParams {
    pub group_id: u16,
    pub keyset_id: u16,
    pub name: String,
    pub endpoint: u16,
    pub epoch_key: [u8; 16],
}
/// 1 ノード分のデバイス側 provision（KeySetWrite → group-key-map read-merge-write →
/// AddGroup → ACL read-merge-write）。失敗はステップ名を detail に含めて即 Err
/// （chip-tool 経路の run_node_step と同粒度）。
pub async fn provision_node(conn: &mut dyn NodeConn, p: &ProvisionNodeParams)
    -> Result<(), MatError>;
/// ACL read-merge-write 単体（grant の本体）。true = write した / false = 冪等 skip。
pub async fn ensure_group_acl(conn: &mut dyn NodeConn, group_id: u16)
    -> Result<bool, MatError>;
```

**設計メモ（実装者向け）**:
- 「コントローラ側 group state」（`groupsettings add-group/add-keysets/bind-keyset` = ローカル KVS 書込）は **M8a では従来どおり chip-tool**（KVS 書込所有は M8c — spec の M8 分割どおり）。provision は「コントローラ側 = chip-tool、デバイス側 = native」のハイブリッドになる。
- group-key-map は**全置換 write**なので、`read_json` で現状を取り `entries` にマージ（同 groupId は置換、他は温存）してから `encode_group_key_map_tlv`。chip-tool 経路（`json!([{...}])` 1 要素で write）は実は**全置換で他グループのマッピングを消していた**可能性がある — native では read-merge-write にする（挙動改善。ただし出力スキーマは不変）。既存挙動と差が出る点として commit メッセージと ARCHITECTURE 追記に明記する。
- KeySetWrite は timed 不要（`resolve_command(0x003F, "key-set-write")` の timed フラグに従う）。epoch_key は `mat_core::group::resolve_epoch_key` が返す hex 文字列を `[u8;16]` へ（既存 parse を確認して合わせる）。
- ACL: `read_json(0, 0x001F, 0x0000)` → `entries_from_im_json` → 既存 `merge_group_entry` → 変化ありなら AclEntry 列を TLV へ（`ops.rs` 内で `tlv::Writer` により AccessControlEntryStruct を組む: subjects は array、targets は None なら null）→ `write_tlv(0, 0x001F, 0x0000, ..., timed=false)`。

- [ ] **Step 1: 失敗するテストを書く**

im エンコーダ（roundtrip: Writer で組んで `tlv_element_to_json` で読み戻し形を検証）、`entries_from_im_json`、ops の 3 群。FakeConn には Task 8 の scripted 拡張に加えて呼び出し記録（`calls: Vec<String>` — `"write_tlv(0,0x003F,0x0000)"` 形式）を持たせる:

```rust
// mat-controller im.rs tests:
#[test]
fn key_set_write_fields_shape() {
    let f = encode_key_set_write_fields(60, &[0xAB; 16]);
    let mut r = Reader::new(&f);
    let first = r.next().unwrap().unwrap();
    let j = tlv_element_to_json(&mut r, first).unwrap();
    // field 0 = GroupKeySetStruct: {0: 60, 1: 0, 2: "abab..", 3: 1, 4..7: null}
    assert_eq!(j["0"]["0"], serde_json::json!(60));
    assert_eq!(j["0"]["1"], serde_json::json!(0));
    assert_eq!(j["0"]["3"], serde_json::json!(1));
    assert!(j["0"]["4"].is_null() && j["0"]["7"].is_null());
}

#[test]
fn group_key_map_tlv_is_list_of_structs() {
    let t = encode_group_key_map_tlv(&[(10, 60), (11, 61)]);
    let mut r = Reader::new(&t);
    let first = r.next().unwrap().unwrap();
    let j = tlv_element_to_json(&mut r, first).unwrap();
    assert_eq!(j, serde_json::json!([{"1": 10, "2": 60}, {"1": 11, "2": 61}]));
}

// mat-core acl.rs tests:
#[test]
fn entries_from_im_json_maps_numeric_keys() {
    let v = serde_json::json!([
        {"1": 5, "2": 2, "3": [112233445566u64], "4": null, "254": 2}
    ]);
    let e = entries_from_im_json(&v).unwrap();
    assert_eq!(e[0].privilege, 5);
    assert_eq!(e[0].auth_mode, 2);
    assert_eq!(e[0].subjects, vec![112233445566]);
    assert!(e[0].targets.is_none());
    assert_eq!(e[0].fabric_index, 2);
    // 解釈不能（privilege 欠落）は Err — read できなければ write しない方針の要。
    assert!(entries_from_im_json(&serde_json::json!([{"2": 2}])).is_err());
}

// mat-native ops.rs tests:
#[tokio::test]
async fn provision_node_runs_steps_in_order() {
    let mut conn = FakeConn::scripted()
        .with_read(0, 0x003F, 0x0000, serde_json::json!([]))   // group-key-map read
        .with_read(0, 0x001F, 0x0000, serde_json::json!([      // acl read（管理者のみ）
            {"1": 5, "2": 2, "3": [1], "4": null, "254": 2}]));
    let p = ProvisionNodeParams { group_id: 10, keyset_id: 60,
        name: "grp10".into(), endpoint: 1, epoch_key: [0xAB; 16] };
    ops::provision_node(&mut conn, &p).await.unwrap();
    let calls = conn.calls();
    // KeySetWrite invoke → group-key-map read → write → AddGroup invoke →
    // acl read → acl write の順。
    assert!(calls[0].starts_with("invoke(1?0,0x003F"), "{calls:?}"); // ep0 宛
    assert!(calls.iter().any(|c| c.starts_with("write_tlv(0,0x003F")));
    assert!(calls.iter().any(|c| c.starts_with("invoke(1,0x0004")));
    assert!(calls.last().unwrap().starts_with("write_tlv(0,0x001F"), "{calls:?}");
}

#[tokio::test]
async fn ensure_group_acl_is_idempotent_when_entry_exists() {
    let mut conn = FakeConn::scripted().with_read(0, 0x001F, 0x0000,
        serde_json::json!([
            {"1": 5, "2": 2, "3": [1], "4": null, "254": 2},
            {"1": 3, "2": 3, "3": [10], "4": null, "254": 2}  // 既に Group エントリ
        ]));
    let wrote = ops::ensure_group_acl(&mut conn, 10).await.unwrap();
    assert!(!wrote);
    assert!(!conn.calls().iter().any(|c| c.starts_with("write_tlv")),
        "must not write when the Group entry already exists");
}
```

（`invoke(1?0,...)` の宛先 ep は実装時に確定させる — KeySetWrite/group-key-map/ACL は ep0、AddGroup のみ指定 endpoint。chip-tool 経路の argv がその形（`group.rs:125-175`）なので合わせる。）

- [ ] **Step 2: 実装**

上記 Interfaces どおり。`native_direct.rs` にバリアント追加:

```rust
GroupProvision { group_id: u16, node_ids: Vec<u64>, keyset_id: u16, name: String,
                 endpoint: u16, epoch_key: Option<String>, rebind: bool },
GroupGrant { group_id: u16, node_ids: Vec<u64> },
```

run_op:
- GroupProvision: (1) `commands::group` から抽出した `provision_controller_state(chip, ...)`（groupsettings 3〜4 ステップ + rebind unbind を関数化、chip-tool 経路と共有）を呼ぶ（ChipTool は `store.root()` から構築）→ (2) 各 node へ `establish` → `ops::provision_node` → (3) 既存 emit（`commands::group::provision` の emit 部を `pub(crate) fn emit_provision_success(...)` に抽出して共有）。
- GroupGrant: 各 node へ establish → `ops::ensure_group_acl` → updated/unchanged を集計 → 抽出した `emit_grant_success`。
- classify: `GroupCommand::Provision` / `GroupCommand::Grant` を上記バリアントへ（M7 の「provision/grant は常に chip-tool」テスト `group_onoff_no_args_is_native_but_generic_group_invoke_is_not` の該当 assert は**期待反転で更新**する — 既存テスト無改変の原則の唯一の例外で、M8a の意図した挙動変更）。
- group invoke の汎用形（onoff 以外 / 引数付き）も Task 7 の invoke と同じ規則で native 化する: cluster/command 解決可 + args スカラー → `NativeOp::GroupInvokeGeneric { group_id, cluster, command, fields_tlv, endpoint, cluster_in, command_in }` を追加し、`mat_native::group::send` に流す（応答なし送信なので fields_tlv だけ組めればよい。Unavailable → chip-tool フォールバックは既存 group 3 形と同型）。

- [ ] **Step 3: テスト・回帰 + Commit**

```bash
cargo test --workspace 2>&1 | tail -3
cargo clippy --workspace -- -D warnings && cargo fmt
git add crates/mat-native crates/mat-core/src/acl.rs crates/mat-controller/src/im.rs crates/mat/src
git commit -m "feat(mat): group provision/grant デバイス側native + group invoke汎用形 (M8a Task9)

コントローラ側groupsettingsはM8cまでchip-tool（KVS書込所有の分割どおり）。
group-key-mapはread-merge-write化（chip-tool経路の全置換より安全）。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 10: matd — 汎用 read/write/invoke/describe/provision の native 化

**Files:**
- Modify: `crates/matd/src/server.rs`（`is_native_hotpath` / `native_op` / `hotpath_success_body` / group 系）
- Modify: `crates/matd/src/native.rs`（NativeBackend に汎用メソッド追加）
- Test: `crates/matd/src/server.rs` + 既存 matd 統合テスト（無改変）

**Interfaces:**
- Consumes: Task 6 の NodeConn 新メソッド、Task 8/9 の `mat_native::ops`、Task 2/3 の ids。
- Produces（NativeBackend 追加メソッド — with_session ラップ、既存の再確立ポリシーそのまま）:

```rust
pub async fn read_json(&self, node_id: u64, endpoint: u16, cluster: u32, attribute: u32)
    -> Result<serde_json::Value, MatError>;
pub async fn write_tlv(&self, node_id: u64, endpoint: u16, cluster: u32, attribute: u32,
    data_tlv: Vec<u8>, timed: bool) -> Result<(), MatError>;
pub async fn invoke_generic(&self, node_id: u64, endpoint: u16, cluster: u32,
    command: u32, fields: Option<Vec<u8>>, timed: bool) -> Result<(), MatError>;
pub async fn describe(&self, node_id: u64) -> Result<Vec<(u16, Vec<u64>)>, MatError>;
pub async fn provision_node(&self, node_id: u64, p: &mat_native::ops::ProvisionNodeParams)
    -> Result<(), MatError>;
pub async fn ensure_group_acl(&self, node_id: u64, group_id: u16) -> Result<bool, MatError>;
```

- [ ] **Step 1: 失敗するテストを書く**

server.rs の tests に分類テストと body 同形テストを追加:

```rust
#[test]
fn generic_ops_join_the_native_hotpath() {
    let read = Op::Read { node_id: 5, endpoint: 1,
        cluster: "levelcontrol".into(), attribute: "current-level".into() };
    assert!(is_native_hotpath(&read));
    let unknown = Op::Read { node_id: 5, endpoint: 1,
        cluster: "nosuch".into(), attribute: "x".into() };
    assert!(!is_native_hotpath(&unknown)); // 未知名は chip-tool へ（互換）
    let write = Op::Write { node_id: 5, endpoint: 1,
        cluster: "levelcontrol".into(), attribute: "on-level".into(),
        value: "128".into() };
    assert!(is_native_hotpath(&write));
    let inv = Op::Invoke { node_id: 5, endpoint: 1,
        cluster: "levelcontrol".into(), command: "move-to-level".into(),
        args: vec!["128".into(), "0".into(), "0".into(), "0".into()] };
    assert!(is_native_hotpath(&inv));
    assert!(is_native_hotpath(&Op::Describe { node_id: 5 }));
}

#[tokio::test]
async fn native_generic_read_body_matches_chip_tool_schema() {
    // FakeConn の read_json は json!(1) を返す（Task 6 の fake 仕様）。
    let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
    let op = Op::Read { node_id: 5, endpoint: 1,
        cluster: "levelcontrol".into(), attribute: "current-level".into() };
    let body = native_op(&op, &native, store_with_node_5().path()).await.unwrap();
    // 既存 hotpath_success_body(Read) と同形（node_id/endpoint/cluster/attribute/value）。
    assert_eq!(body["cluster"], "levelcontrol");
    assert_eq!(body["attribute"], "current-level");
    assert!(body["value"].is_number());
}

#[tokio::test]
async fn native_write_rejects_list_type_with_parse_error() {
    let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
    let op = Op::Write { node_id: 5, endpoint: 0,
        cluster: "accesscontrol".into(), attribute: "acl".into(), value: "[]".into() };
    let err = native_op(&op, &native, store_with_node_5().path()).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::ParseError);
}
```

（`store_with_node_5()` は既存 server テストの store フィクスチャヘルパを流用 — tests mod を grep して現物の名前に合わせる。）

- [ ] **Step 2: 実装**

- `is_native_hotpath` / 新分類: matd 側も **classify_strict 相当**が要る。mat 側 native_direct の分類ロジック（名前解決 + スカラー判定）とロジック重複を避けるため、**判定の中核を mat-core::ids に置く**:

```rust
// mat-core::ids に追加（Task 10 で。mat 側 native_direct も これを使う形に
// リファクタして一本化する）:
pub enum WriteClass { Native { attribute: u32, value: ScalarValue, timed: bool },
                      Reject(String), NotNative }
pub fn classify_write(cluster: &str, attribute: &str, value: &str) -> WriteClass;
pub enum InvokeClass { Native { command: u32, fields: Vec<ScalarValue>, timed: bool },
                       Reject(String), NotNative }
pub fn classify_invoke(cluster: &str, command: &str, args: &[String]) -> InvokeClass;
```

- server.rs: `run_op` の native 分岐に Read汎用/Write/Invoke/Describe を追加。Reject は `MatError::parse_error(reason)` を即返し。body は既存 chip-tool 経路と同じ組み立てを共有（Read は既存 `hotpath_success_body` の Read 形をそのまま使える。Write/Invoke/Describe は chip-tool 経路の body 組み立て箇所を server.rs 内で探して関数化・共有 — **grep して既存 body と JSON キー完全一致を確認**すること）。
- GroupProvision: 既存 `group_provision`（chip-tool 実装）の**デバイス側ステップだけ** native に置換したバージョンを native 有効時に使う。コントローラ側 groupsettings は既存どおり chip-tool ws（backend）。GroupInvoke 汎用形は `native_group_params` を ids ベースに拡張（onoff 限定を撤廃、スカラー引数の fields_tlv 化。Reject 形は chip-tool へ落とさず parse_error — mat 側と同じ規則）。
- **既存 matd 統合テストが無改変で通ることが回帰条件**（native 無効時の挙動ゼロ変化）。

- [ ] **Step 3: テスト・回帰 + Commit**

```bash
cargo test -p matd 2>&1 | tail -3
cargo test --workspace 2>&1 | tail -3
cargo clippy --workspace -- -D warnings && cargo fmt
git add crates/matd crates/mat-core/src/ids.rs crates/mat/src/native_direct.rs
git commit -m "feat(matd): 汎用read/write/invoke/describe/provisionのnative化（mat直経路と判定共有） (M8a Task10)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 11: 実機 E2E ハーネス

**Files:**
- Create: `scripts/e2e-m8a-real.sh`
- Modify: `Taskfile.yml`（`e2e:m8a:real` タスク追加）

**Interfaces:**
- Consumes: 0.18.0 の mat/matd バイナリ、jarvis 実機環境（`MAT_IFACE=eth0` / `MAT_FABRIC_INDEX=2`、living_lights=group 10、既知ノード）。

- [ ] **Step 1: 既存ハーネスの流儀を読む**

`scripts/e2e-m7.sh` を読み、共通の骨格（chip-tool 未 spawn の検出方法 = PATH shim / カウンタ検証 / trap 後始末 / 色付き PASS/FAIL 出力が **stderr** に出ること）を把握する。

- [ ] **Step 2: ハーネスを書く**

`scripts/e2e-m8a-real.sh` — e2e-m7.sh の骨格を流用し、以下を検証（ノード ID・iface は環境変数で注入、既定は jarvis 実機の値。**real ノード ID をリポジトリにハードコードしない** — `MAT_E2E_NODE` 等の必須環境変数にする）:

1. **read 汎用**: `mat read levelcontrol current-level $NODE 1` が native（chip-tool shim 未呼び出し）で数値を返す。
2. **write**: `mat write levelcontrol on-level 128 $NODE 1` native 成功 → `mat read levelcontrol on-level` で読み返し 128。後始末で `null` を write して戻す。
3. **未対応型拒否**: `mat write accesscontrol acl '[]' $NODE 0` が exit 1 + stderr の error.kind == `parse_error`（受け入れ基準 5）。
4. **invoke 汎用**: `mat invoke levelcontrol move-to-level 200 0 0 0 $NODE 1` native 成功 → current-level 読み返し。
5. **describe**: `mat describe $NODE` native 成功、endpoints に ep0 + 実エンドポイント。chip-tool 経路（`MAT_IFACE` 抜き）の出力と **JSON 構造一致**（jq でキー比較）。
6. **diag**: `mat diag thread $NODE` native 成功、`thread.neighbor_table` が配列で `Lqi` 系キーを含む。chip-tool 経路と主要キー一致。
7. **open-window**: `mat open-window $NODE --timeout 180` native 成功、manual_code が 11 桁 / qr_payload が `MT:` 始まり。（開けた window は timeout 失効に任せる）
8. **group grant 冪等**: `mat group grant 10 --nodes $NODES` native 成功、2 回目が全ノード unchanged。
9. **group provision 再実行**: `mat group provision 10 --nodes $NODES --keyset-id 60 --rebind ...` native（デバイス側）成功 → `mat group off 10` / `mat group on 10` で N/N 配達を人力確認するプロンプト（M5/M7 ハーネスと同じ流儀）。
10. **matd 経由**: matd を `MAT_MATD_IFACE` 付きで起動し、1/2/4/5 と同等の op が native（matd ログに chip-tool spawn なし）で通る。
11. **フォールバック健全**: `MAT_IFACE` 未設定で 1〜9 の全コマンドが従来どおり成功（受け入れ基準 4）。

- [ ] **Step 3: Taskfile 配線 + ローカル dry-run**

`Taskfile.yml` に `e2e:m8a:real:` を追加（`bash scripts/e2e-m8a-real.sh`）。ローカルでは実機が無いので `bash -n scripts/e2e-m8a-real.sh`（構文チェック）と、`MAT_E2E_NODE` 未設定時に「必須環境変数が無い」と即 FAIL するガードの動作確認まで。

- [ ] **Step 4: Commit**

```bash
git add scripts/e2e-m8a-real.sh Taskfile.yml
git commit -m "test(e2e): M8a実機ハーネス（汎用IM native直経路+matd+フォールバック+型拒否） (M8a Task11)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 12: ドキュメント + 最終確認

**Files:**
- Modify: `ARCHITECTURE.md`（Phase 5 節に M8 分割と M8a 実装済みを追記）
- Modify: `README.md`（native 対象 op 一覧の更新 — 「Backend」相当節）
- Modify: `CLAUDE.md`（Backend 節の native hotpath 記述を「M8a で汎用 IM に拡大」へ 1 行更新）

- [ ] **Step 1: ARCHITECTURE.md 追記**

M7 の記述（`- M7 実装済み(2026-07-15): ...`）の直後に M8 の節を追加: M8 の 3 分割（spec 参照）、横断決定 4 点（KVS=INI 継続 / name→ID 全生成 / BLE 既定有効(M8c) / 完全撤去(M8c)）、M8a 実装済み内容（生成テーブル・IM Write/wildcard/チャンク・native 化 op 一覧・provision ハイブリッド（コントローラ側 groupsettings は M8c まで chip-tool）・group-key-map の read-merge-write 化・実機 E2E は未実施なら「別途実施後に追記」と明記）。

- [ ] **Step 2: README.md 更新**

native 直経路の対象 op 一覧（M7 の unicast5形+group3形）を M8a の拡大後（汎用 read/write/invoke・describe・diag thread・open-window・group provision/grant/invoke 汎用形）に更新。「未対応型（list/struct）の汎用 write/invoke は `parse_error`」の 1 行を Errors 節近くに追加。

- [ ] **Step 3: 最終チェック**

```bash
task check 2>&1 | tail -5   # fmt:check + clippy + 全テスト
```

Expected: all green。落ちたら直してから次へ。

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md README.md CLAUDE.md
git commit -m "docs: M8a（汎用IM native化）の実装内容とM8分割をARCHITECTURE/READMEに反映 (M8a Task12)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## 完了後（planの範囲外、ユーザーと実施）

1. jarvis での `task e2e:m8a:real` 実行（受け入れ基準 1〜5 の実機確認）。
2. 合格後: `matter-controller` → `main` へ `--no-ff` マージ、ARCHITECTURE.md に E2E 結果追記。
3. 本番 jarvis の mat/matd を 0.18.0 へ更新（M7 と同じデプロイ手順、memory 参照）。
