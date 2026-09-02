# 汎用 write / invoke の型拡張（float・list・struct）と listen 非対称の明文化 — 設計

日付: 2026-09-03 / 監査バックログ（2026-08-31）レーン C / ブランチ `worktree-write-types`

## 背景と目的

汎用 `write` / `invoke`（`group invoke` 含む）の JSON→TLV 符号化はスカラー
（bool/uint/int/enum/bitmap/string/octstr）のみで、生成テーブルが `list` /
`struct` / `float` と知っている属性・引数は `parse_error` で門前払いしている
（docs/commands.md「Scalar-only generic write / invoke」）。根本原因は
`ids_gen.rs` に属性 struct の内部フィールド定義が無いこと。一方 generic `read`
は list/struct/float を JSON 化できる（`im::tlv_element_to_json`）ので、
read できる属性を write できない非対称になっている。

本設計で:

1. **float write** を通す（TLV writer の `put_f32/put_f64` は既にある）。
2. **list / struct write（および invoke の struct/list 引数）** を通す —
   `gen-ids.py` を拡張して struct のフィールド定義を生成し、型記述に沿って
   JSON→値ツリー→TLV を符号化する。
3. **listen が list/struct を捨てる非対称**を仕様として明文化する（コード変更
   なし、理由は後述）。

触る範囲（並行レーンとの衝突回避）: `mat-core`（`ids.rs` / `ids_gen.rs` /
`parse.rs` の `normalize_value`）、`scripts/gen-ids.py`、`mat-controller/src/im.rs`
（`ImValue` の float 腕のみ）、`mat-native/src/lib.rs`（TLV 符号化ヘルパ）と
`mat-native/src/ops.rs`（専用エンコーダの可視性をテスト用に `pub(crate)` へ）、
`matd/src/subscription.rs`（コメントのみ）、docs。**`mat-native/src/op.rs` は
レーン B の担当**なので、既存テスト 1 件（`acl` に `[]` を書くと Reject を期待
する `write_scalar_ok_list_rejected_unknown_unresolved`）の期待値差し替え以外は
触らない。`mat-controller` の `case.rs` / `session.rs` / `x509.rs`（レーン D）は
触らない。

## スコープ外

- 数値 ID 直指定（テーブルに無い名前）での list/struct write。型情報が無いので
  符号化できない。`[` / `{` で始まる値は「名前が要る」旨の `parse_error` にする
  （従来は文字列として送っていた — 意図した値になることはないので拒否に倒す）。
- `write` 出力 `value` の型付きエコー。`body::write_success` は入力文字列を
  `normalize_value` で正規化して返す契約のまま。JSON 入力は JSON としてパース
  して返す（`normalize_value` の小さな拡張、下記）。
- read 出力の struct キー命名（現状は context tag 番号の10進文字列）。本設計で
  write が名前キーを受けるようになるが、read 側は変えない（別件）。
- listen の list/struct イベント化（理由は §5）。
- 専用エンコーダ（`group provision` / `grant`）の汎用エンコーダへの置換。

## 1. 型記述（`mat-core::ids`）

### 1.1 `TypeTag` をスカラー専用にし、`Ty` で形を表す

```rust
/// スカラー型。List / Struct / Float は廃止（形は `Ty` が持つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTag { Bool, UInt, Int, F32, F64, Str, Bytes, Unknown }

/// 属性 / コマンド引数 / struct フィールドに共通の型記述。
#[derive(Debug, Clone, Copy)]
pub enum Ty {
    Scalar(TypeTag),
    Struct(&'static StructDef),
    /// スカラー要素の list（Matter の `array`）。
    List(TypeTag),
    ListOfStruct(&'static StructDef),
}

pub struct AttrDef  { name, id: u32, ty: Ty, writable, timed_write }
pub struct FieldDef { name, ty: Ty, optional }            // コマンド引数
pub struct StructDef { name: &'static str, fields: &'static [StructField] }
pub struct StructField { name: &'static str, id: u8, ty: Ty, optional: bool }
```

- XML `single` → `F32`、`double` → `F64`（現状は両方 `Float` で区別が無く、
  TLV 要素型 0x0A / 0x0B を選べなかった）。
- XML で `type="array"` + `entryType` / `array="true"`（コマンド引数・struct
  項目）は `List(elem)` か `ListOfStruct(def)`。要素型が解決できない場合は
  `List(TypeTag::Unknown)`（符号化時に「型不明」で拒否、現状の `Unknown` と同じ扱い）。
- struct 名がテーブルに無い（型名に "Struct" を含むが定義が見つからない）場合は
  `Scalar(TypeTag::Unknown)`。
- `ty` を参照するのは `ids.rs` 内のみ（他クレートは `TypeTag::` を参照していない
  ことを確認済み）なので、この形の変更は外に漏れない。

### 1.2 struct の解決スコープ（`gen-ids.py`）

同名 struct が複数クラスタに別定義で存在する（`ModeOptionStruct` /
`TargetStruct` / `SemanticTagStruct` 等 10 件）ため、struct は
**(クラスタ, 名前)** で解決する: 参照元クラスタに `<cluster code>` を持つ同名
struct → 無ければ `<cluster>` 子を持たない global struct（`global-structs.xml`
等）。どちらも無ければ未解決。

生成するのは **属性またはクライアントコマンド引数から到達可能な struct の閉包**
のみ（到達しない struct は出力しない — テーブル肥大の抑制）。static 名は
`S_<CLUSTERKEY>_<STRUCTNAME_UPPER>`、global は `S_GLOBAL_<NAME>`。struct が
struct を含む再帰参照は `static` 同士の `&S_...` 参照で表す（Rust の static
初期化子は他 static への参照を許す）。

`gen-ids.py` のヘッダに connectedhomeip の取得手順を追記する:
`git clone --depth 1 --branch v1.4.2.0 --filter=blob:none --sparse` +
`git sparse-checkout set src/app/zap-templates/zcl/data-model/chip`
（フル clone 不要。現行スクリプトの出力が checkin 済み `ids_gen.rs` と
一致することは確認済みなので、拡張前後で差分は追加分だけになる）。

フィールド名は既存 `kebab()` と同じ変換（`GroupKeySetID` → `group-key-set-id`）。

## 2. 値モデルとパーサ（`mat-core::ids`）

### 2.1 `ScalarValue` の拡張

```rust
pub enum ScalarValue {
    Bool(bool), UInt(u64), Int(i64), F32(f32), F64(f64),
    Str(String), Bytes(Vec<u8>), Null,
    List(Vec<ScalarValue>),
    /// (field id, 値)。符号化時のタグ順（id 昇順）に整列済み。
    Struct(Vec<(u8, ScalarValue)>),
}
```

名前は `ScalarValue` のまま据え置く（`mat-native::op` が参照しており、改名は
レーン B と衝突する。「スカラーでない値も持つ」旨を doc コメントに書き、改名は
直列後回しの可読性分割で扱う）。`PartialEq` は derive（`Eq` は f32 のため外す —
現状も `PartialEq` のみ）。

### 2.2 パース規則

`parse_scalar_typed(input, TypeTag)` を `parse_value_typed(input, &Ty)` に
置き換える（呼び出し元は `classify_write` / `classify_invoke` のみ）。

**トップレベルがスカラー型**（`Ty::Scalar`）: 従来のリテラル構文のまま。
`null` → `Null`、Bool は `true/false/1/0`、UInt は 10進/`0x`、Int は 10進、
Str は生文字列、Bytes は `hex:` 必須。**追加**: `F32` / `F64` は Rust の
float リテラル（`1.5` / `-3` / `2e-3`）。`nan` / `inf` は拒否（TLV に載せても
デバイスは CONSTRAINT_ERROR）。

**トップレベルが list / struct**: 入力全体を JSON としてパース（`serde_json`、
mat-core は既に依存）し、型記述に沿って再帰変換する:

| 期待型 | 受け付ける JSON | 備考 |
|---|---|---|
| Bool | `true` / `false` | |
| UInt | 非負整数、または文字列 `"123"` / `"0x1234"` | 文字列は ACL subject 等で 64bit node id を 16 進で書くため |
| Int | 整数、または文字列 10 進 | |
| F32 / F64 | 数値 | |
| Str | 文字列 | |
| Bytes | 文字列: `"hex:aabb"` または `"aabb"` | read 出力（プレフィックス無し小文字 hex）をそのまま書き戻せるように両方 |
| 任意 | `null` | `Null`（nullable かどうかはデバイスが検証する — 現状のトップレベル `null` と同じ方針） |
| Struct | オブジェクト | キーは **フィールド名（kebab）または fieldId の10進文字列**（read 出力の形） |
| List / ListOfStruct | 配列 | |

struct の検査: 未知キー → エラー（有効なキー一覧を detail に含める）、同じ
フィールドを名前と番号の両方で指定 → エラー、`optional: false` のフィールド
欠落 → エラー（デバイスに投げる前に落とす — 「何が足りないか」を detail に出す
方が IM ステータスより回復しやすい）。出力は fieldId 昇順に整列。

型不一致（struct に配列、UInt に負数、等）は「期待型 / 実際の JSON 型」を含む
`Err(String)` → 既存どおり `WriteClass::Reject` / `InvokeClass::Reject` →
`parse_error`。ネストの深さは JSON パーサ任せ（serde_json の既定 128 段）。

**数値 ID 直指定（def 無し）**: `parse_scalar_inferred` は据え置きだが、
(a) `1.5` のような float リテラル（`.` / `e` を含み f64 として読める）は
`F64` に推定する（従来は `Str` に落ちていた — 有用だったことはない）、
(b) `[` / `{` で始まる値は `classify_write` / `classify_invoke` の None 腕で
Reject（「list/struct はテーブルが知る名前が必要」）。

### 2.3 `normalize_value`（`mat-core::parse`）

`write` 出力の `value` エコー用。先頭が `[` / `{` で JSON としてパースできれば
その JSON を返す（従来は先頭トークン規則の末尾で生文字列に落ちていた）。
他は不変。

## 3. TLV 符号化（`mat-native::lib`）と `ImValue`（`mat-controller::im`）

- `mat-native::lib` に再帰ヘルパ `put_value(w: &mut Writer, tag: Tag, v: &ScalarValue)`
  を置き、`scalar_to_tlv`（write の Data 要素）と `encode_command_fields`
  （invoke の CommandFields、context tag = 引数添字）の両方がこれを使う。
  `List` → `start_array(tag)`（属性 list は TLV Array 0x16。TLV List 0x17 は
  path 専用）、`Struct` → `start_struct(tag)` + 各フィールド `Tag::Context(id)`、
  `F32` → `put_f32`、`F64` → `put_f64`。
- `scalar_to_im`（`ScalarValue` → `ImValue`）はテストでしか使われていない。
  container を写せないので `Option<ImValue>` を返すように変える（container は
  `None`）。
- `ImValue` に `F32(f32)` / `F64(f64)` を追加し、`value_to_im`（report の
  scalar デコード、im.rs:374 の `UnsupportedValue`）と `encode_im_value` に腕を
  足す。`Value::StructStart | ArrayStart | ListStart` の `UnsupportedValue` は
  据え置き（scalar read 専用デコーダの契約。汎用 read は `read_json` 経路）。
  `ImValue` を網羅 match しているのは `im.rs` 内と `mat-native::lib`
  だけ（`session.rs` は `Bool` 腕 + ワイルドカード）。

## 4. 専用エンコーダとの関係

`group provision` / `grant` の list/struct 書込（`ops.rs::encode_acl_entries_tlv`、
`im::encode_group_key_map_tlv`、`im::encode_key_set_write_fields`）は **据え置く**。
理由: これらは read-merge-write の途中で Rust の型付き構造体（`AclEntry` 等）
から直接符号化しており、ユーザー入力 JSON を経由しない。汎用経路に寄せると
「構造体 → JSON → 値ツリー → TLV」の遠回りになるだけで利点が無い。

代わりに **等価性テスト**で両者を縛る: 同じ ACL エントリ / group-key-map /
KeySetWrite 引数を (a) 専用エンコーダ、(b) 名前キー JSON → `parse_value_typed`
→ `put_value` で符号化し、**バイト列が一致**することを `mat-native` の単体
テストで固定する。生成テーブル（fieldId・型）と専用エンコーダのどちらかが
ずれたら即検知できる。テストのため `encode_acl_entries_tlv` を `pub(crate)` に
する（`ops.rs` の可視性変更のみ）。

## 5. listen の list/struct 非対称（doc のみ）

`matd/src/subscription.rs::events_from_report` は list/struct 値と list 要素追記
report を debug ログで捨てる。**揃えない**と判断した理由:

1. Subscribe 経路は ReportDataMessage を 1 通ずつ処理しており、チャンク
   （`MoreChunkedMessages` + `ListIndex: null` 追記列）の再組み立てが無い。
   list をイベント化すると「先頭チャンクだけの途中 list」を値として流すことになる。
   再組み立ては `subscription.rs` の中核（priming / recovery / 無音 deadline）
   に状態を足す中規模改修で、本番常駐デーモンのリスクに見合う消費者がいない
   （casa / mando はスカラー属性しか購読しない）。
2. list-diff priming recovery が list の要素単位変化を実遷移と誤認して
   `recovered` を量産する（コード内コメントの既知の害）。
3. read は one-shot で `merge_reports` によりチャンクを併合できる — 非対称は
   経路の性質の差であって、単なる実装漏れではない。

やること: docs/commands.md の listen 節から「generic `read` と同じ既知の制限」
という**陳腐化した記述を訂正**し（read は list/struct を JSON 化できる、
write もできるようになる）、「listen だけの制限」として上記 1〜2 の理由を書く。
ARCHITECTURE.md の listen 記述（1116 行付近）と `subscription.rs` のコメントも
同じ文言に揃える。

## 6. ドキュメント

- docs/commands.md「Scalar-only generic write / invoke」→「Generic write /
  invoke value encoding」に書き換え: スカラーのリテラル構文、list/struct は
  JSON、キー = フィールド名または fieldId、bytes の hex 両形式、`null`、数値 ID
  直指定では list/struct 不可、float 対応、専用エンコーダとの関係。
  State operations の例に list write（`group-key-map` を read 出力そのままで
  書き戻す例）を 1 つ足す。
- docs/errors.md の `parse_error` 説明から「list/struct/float は未対応」を外し、
  「JSON が型記述に合わない / 数値 ID で list/struct」を理由として書く。
- ARCHITECTURE.md の M8c-3 記録「(4) JSON→TLV の型サポートはスカラーのみ」と
  「将来候補 (2) 汎用 list/struct TLV エンコード」に取り消し線 + 【訂正
  2026-09-03】で本設計への参照を付ける（記録は書き換えず訂正を重ねる、
  リポジトリの流儀）。
- CLAUDE.md「Generic write/invoke/group invoke encode scalar JSON→TLV only」の
  段落を新しい契約に更新。

## 7. テスト

単体（`task check` = fmt + clippy + test）:

- `ids.rs`: 生成テーブルのスポットチェック（`acl` = `ListOfStruct(AccessControlEntryStruct)`
  で `subjects` が `List(UInt)`、`targets` が `ListOfStruct`、
  `group-key-map`、`key-set-write` 引数 0 = `Struct(GroupKeySetStruct)`、
  unittesting の `float-single` = `F32` / `float-double` = `F64`）、
  `parse_value_typed` の正常系（名前キー / 番号キー / 混在、文字列 16 進 UInt、
  bytes 両形式、null、optional 欠落 OK）と異常系（未知キー、必須欠落、名前と
  番号の重複、型不一致、`nan`、数値 ID + `[`）、`classify_write` / `classify_invoke`
  の list/struct 受理と Reject 文言。
- `mat-native::lib`: `put_value` の TLV 往復（Reader で読み戻し → `tlv_element_to_json`
  で read と同形になる）、float の要素型バイト（0x0A / 0x0B）、§4 の等価性
  テスト 3 本。
- `mat-native::op` 既存テスト 1 件の期待値差し替え（`acl` に `[]` は受理、
  `acl` に `{}` は Reject）。
- `im.rs`: `ImValue::F32/F64` のデコード（`value_to_im`）と `encode_im_value`。
- `parse.rs`: `normalize_value` の JSON 入力。

実機スモーク（hogar-matd コンテナ内、直経路 `MAT_MATD=0 MAT_FABRIC_INDEX=2`、
`MAT_STORE=/data/mat`。本番 matd 1.30.0 は list write を知らないので matd 経路は
使わない。手元で `cargo build --release -p matterctl` した x86_64 glibc 2.35
バイナリをコンテナ（Debian bookworm, glibc 2.36）へ `docker cp`。他セッションの
スモークと同時に走らせない）:

1. list-of-struct write の無害パターン: group メンバーノード 1 台で
   `read groupkeymanagement group-key-map`（番号キー JSON）→ **同じ JSON を
   `--value` に渡して write** → 再 read が一致。ACL は自分を締め出す事故の
   影響が大きいので対象にしない。
2. スカラー write の回帰: `levelcontrol on-level` を現在値と同じ値で write →
   exit 0。
3. float / invoke struct 引数は手元デバイスに書込可能な対象が無い（float は
   unittesting クラスタのみ、KeySetWrite は本番 group 鍵に触る）ので単体テスト
   のみ。スモークの記録は plan の最終タスクで残す。

## 8. コミット分割

1. `feat(write): float 対応` — `TypeTag::F32/F64`、`ScalarValue::F32/F64`、
   `ImValue::F32/F64`、`put_value` の float 腕、gen-ids.py の single/double
   分離と `ids_gen.rs` 再生成、docs の float 記述。ここまでで単独マージ可能
   （レーン B の `--timed` が `ids.rs` を待っている）。
2. `feat(ids): struct スキーマ生成`（gen-ids.py + `Ty` + `ids_gen.rs` 再生成）、
   `feat(write): list/struct の JSON→TLV`（パーサ + 符号化 + 等価性テスト +
   `normalize_value`）、`docs: write/invoke の型契約更新` — 3 コミット。
3. `docs(listen): list/struct を捨てる理由を明記` — 1 コミット。

完了時: main へ rebase → no-ff マージ → push、メモリ「並列レーン計画」に
レーン C 完了を追記。
