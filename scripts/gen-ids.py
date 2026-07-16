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
    global_elems = []
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
        for g in tree.getroot().iter("global"):
            global_elems.append(g)
    return cluster_elems, global_elems, enums, bitmaps, structs


def parse_global_attrs(global_elems, enums, bitmaps, structs):
    # global-attributes.xml: <configurator><global><attribute side="server" .../></global></configurator>.
    # ClusterRevision(0xFFFD) / FeatureMap(0xFFFC) / AttributeList(0xFFFB) /
    # AcceptedCommandList(0xFFF9) / GeneratedCommandList(0xFFF8) は全クラスタ共通で、
    # <cluster> 側の attribute イテレーションには現れない。ここで一度だけ集める。
    attrs = []
    for g in global_elems:
        for a in g.iter("attribute"):
            if a.get("side", "server") != "server":
                continue
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
    return attrs


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
    cluster_elems, global_elems, enums, bitmaps, structs = parse_files(sys.argv[1])
    global_attrs = parse_global_attrs(global_elems, enums, bitmaps, structs)
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
            # global ZCL 属性（FeatureMap 等）を全クラスタの attrs に合流。
            # 0xFFF8-0xFFFD は予約域なのでクラスタ固有属性と ID が衝突することはない。
            all_attrs = attrs + global_attrs
            clusters[key] = (cid, sorted(set(all_attrs)), sorted({
                (n, i, t, tuple(f)) for (n, i, t, f) in cmds}))
    emit(clusters)


def emit(clusters):
    print("// @generated by scripts/gen-ids.py — DO NOT EDIT BY HAND.")
    print("// Source: connectedhomeip v1.4.2.0 data-model XML. 再生成手順は")
    print("// scripts/gen-ids.py のヘッダ参照。")
    print("#![cfg_attr(rustfmt, rustfmt::skip)]")
    print("#![allow(clippy::unreadable_literal)]")
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
