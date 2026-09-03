#!/usr/bin/env python3
"""mat-core/src/ids_gen.rs を connectedhomeip の data model XML から生成する。

使い方:
    python3 scripts/gen-ids.py /path/to/connectedhomeip > crates/mat-core/src/ids_gen.rs

connectedhomeip の取得（フル clone 不要、data-model XML だけ sparse checkout）:
    git clone --depth 1 --branch v1.4.2.0 --filter=blob:none --sparse \
        https://github.com/project-chip/connectedhomeip.git chip
    git -C chip sparse-checkout set src/app/zap-templates/zcl/data-model/chip
    python3 scripts/gen-ids.py chip > crates/mat-core/src/ids_gen.rs

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


SCALAR_OF = {
    "boolean": "Bool",
    "single": "F32", "double": "F64",
    "char_string": "Str", "long_char_string": "Str",
    "octet_string": "Bytes", "long_octet_string": "Bytes",
}

# cluster id -> cluster_key（static 名の組み立てに使う）。main で埋める。
CLUSTER_KEY_OF_ID = {}


def scalar_tag(ty: str, enums: set, bitmaps: set) -> str:
    t = ty.strip()
    tl = t.lower()
    if tl in SCALAR_OF:
        return SCALAR_OF[tl]
    if re.fullmatch(r"int\d+u", tl) or re.fullmatch(r"enum\d+", tl) \
       or re.fullmatch(r"bitmap\d+", tl):
        return "UInt"
    if re.fullmatch(r"int\d+s?", tl):
        # "int8s".."int64s" は Int、"int8".."int64"（無印）は歴史的に符号なし扱い。
        return "Int" if tl.endswith("s") else "UInt"
    if t in enums or t in bitmaps:
        return "UInt"
    # zap の派生型（epoch_s, fabric_idx, node_id, percent, temperature 等）は
    # ほぼ全て符号なし整数ベース。
    return "UInt"


def static_name(key) -> str:
    cid, name = key
    n = re.sub(r"[^A-Za-z0-9]", "", name).upper()
    if cid is None:
        return f"S_GLOBAL_{n}"
    ckey = CLUSTER_KEY_OF_ID.get(cid)
    if ckey is None:
        # どのクラスタ要素にも無い cluster code（コメントアウト済みクラスタ等）。
        return f"S_C{cid:04X}_{n}"
    return f"S_{ckey.upper()}_{n}"


def resolve_struct(structs: dict, cluster_id, name: str):
    """struct 型名の解決: (cluster, name) 優先、無ければ global (None, name)。"""
    if (cluster_id, name) in structs:
        return (cluster_id, name)
    if (None, name) in structs:
        return (None, name)
    return None


def ty_of(cluster_id, ty: str, is_array: bool, entry, enums, bitmaps, structs,
          used: set) -> str:
    """戻り値は Rust の `Ty::...` 式。到達した struct キーを used に積む。"""
    elem = (entry or ty).strip()
    skey = resolve_struct(structs, cluster_id, elem)
    if skey is not None:
        used.add(skey)
        ref = "&" + static_name(skey)
        return f"Ty::ListOfStruct({ref})" if (is_array or entry) \
            else f"Ty::Struct({ref})"
    if "struct" in elem.lower():
        tag = "Unknown"          # 名前は struct 風だが定義が無い
    else:
        tag = scalar_tag(elem, enums, bitmaps)
    return f"Ty::List(TypeTag::{tag})" if (is_array or entry) \
        else f"Ty::Scalar(TypeTag::{tag})"


def parse_files(root_dir: str):
    xml_dir = os.path.join(
        root_dir, "src", "app", "zap-templates", "zcl", "data-model", "chip")
    files = sorted(glob.glob(os.path.join(xml_dir, "*.xml")))
    if not files:
        sys.exit(f"no xml under {xml_dir}")
    enums, bitmaps = set(), set()
    # (cluster_id or None, name) -> {"name", "fabric_scoped", "items"}
    structs = {}
    cluster_elems = []
    global_elems = []
    for f in files:
        tree = ET.parse(f)
        for e in tree.getroot().iter("enum"):
            enums.add(e.get("name", ""))
        for e in tree.getroot().iter("bitmap"):
            bitmaps.add(e.get("name", ""))
        for st in tree.getroot().iter("struct"):
            sname = st.get("name", "")
            items = []
            # <item> は enum / bitmap の中にも出るので struct 直下だけを見る。
            for idx, it in enumerate(st.findall("item")):
                # fieldId 無しの struct（unittesting の NullablesAndOptionalsStruct 等)
                # は zap の慣例どおり出現順 = fieldId（struct 内で混在はしない）。
                fid = int(it.get("fieldId"), 0) \
                    if it.get("fieldId") is not None else idx
                items.append((fid, it.get("name", ""), it.get("type", ""),
                              it.get("array", "false") == "true",
                              it.get("optional", "false") == "true"))
            info = {"name": sname,
                    "fabric_scoped": st.get("isFabricScoped", "false") == "true",
                    "items": sorted(items)}
            codes = [int(c.get("code"), 0) for c in st.findall("cluster")]
            for key in ([(c, sname) for c in codes] or [(None, sname)]):
                structs.setdefault(key, info)   # 先勝ち
        for c in tree.getroot().iter("cluster"):
            cluster_elems.append(c)
        for g in tree.getroot().iter("global"):
            global_elems.append(g)
    return cluster_elems, global_elems, enums, bitmaps, structs


def parse_global_attrs(global_elems, enums, bitmaps, structs, used):
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
            # global 属性（AttributeList 等）の list 要素は id の list。
            # struct 名の解決スコープは "global"（None）。
            expr = ty_of(None, ty, ty.lower() == "array", entry,
                         enums, bitmaps, structs, used)
            attrs.append((kebab(an), int(acode, 0), expr,
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
    for c in cluster_elems:
        cname = c.findtext("name", "").strip()
        ccode = c.findtext("code", "").strip()
        if cname and ccode:
            CLUSTER_KEY_OF_ID.setdefault(int(ccode, 0), cluster_key(cname))
    used = set()
    global_attrs = parse_global_attrs(global_elems, enums, bitmaps, structs, used)
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
            expr = ty_of(cid, ty, ty.lower() == "array", entry,
                         enums, bitmaps, structs, used)
            attrs.append((kebab(an), int(acode, 0), expr,
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
                fexpr = ty_of(cid, fty, arg.get("array", "false") == "true",
                              None, enums, bitmaps, structs, used)
                fields.append((kebab(fn), fexpr,
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
    # 到達閉包: 属性 / コマンド引数から届いた struct の items をたどり、
    # 新しい struct が現れなくなるまで回す。
    while True:
        n = len(used)
        for key in list(used):
            for (_fid, _fn, fty, is_array, _opt) in structs[key]["items"]:
                ty_of(key[0], fty, is_array, None, enums, bitmaps, structs, used)
        if len(used) == n:
            break
    emit(clusters, structs, enums, bitmaps, used)


def emit(clusters, structs, enums, bitmaps, used):
    print("// @generated by scripts/gen-ids.py — DO NOT EDIT BY HAND.")
    print("// Source: connectedhomeip v1.4.2.0 data-model XML. 再生成手順は")
    print("// scripts/gen-ids.py のヘッダ参照。")
    print("#![cfg_attr(rustfmt, rustfmt::skip)]")
    print("#![allow(clippy::unreadable_literal)]")
    print("use super::ids::{AttrDef, ClusterDef, CmdDef, FieldDef, StructDef, "
          "StructField, Ty, TypeTag};")
    print()
    # struct 定義。static 同士の前方参照は Rust が許すので順序は名前順でよい。
    for key in sorted(used, key=static_name):
        info = structs[key]
        print(f'static {static_name(key)}: StructDef = StructDef {{ '
              f'name: "{info["name"]}", fields: &[')
        for (fid, fname, fty, is_array, optional) in info["items"]:
            expr = ty_of(key[0], fty, is_array, None, enums, bitmaps, structs,
                         used)
            print(f'    StructField {{ name: "{kebab(fname)}", id: {fid}, '
                  f"ty: {expr}, optional: {str(optional).lower()} }},")
        if info["fabric_scoped"]:
            # fabric-index は spec 上の暗黙フィールド（XML には現れない）。
            print('    StructField { name: "fabric-index", id: 254, '
                  "ty: Ty::Scalar(TypeTag::UInt), optional: true },")
        print("] };")
    print()
    names = sorted(clusters.keys())
    for key in names:
        cid, attrs, cmds = clusters[key]
        up = key.upper()
        print(f"static ATTRS_{up}: &[AttrDef] = &[")
        for (n, i, t, w, tw) in attrs:
            print(f'    AttrDef {{ name: "{n}", id: {i:#06x}, '
                  f"ty: {t}, writable: {str(w).lower()}, "
                  f"timed_write: {str(tw).lower()} }},")
        print("];")
        print(f"static CMDS_{up}: &[CmdDef] = &[")
        for (n, i, timed, fields) in cmds:
            fl = ", ".join(
                f'FieldDef {{ name: "{fn}", ty: {ft}, '
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
