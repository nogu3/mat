# Phase 5 M3: KVS 堅牢化 + jarvis 実機相乗り 設計

2026-07-12 起草。親 spec: `2026-07-10-phase5-backend-direction-design.md`（M1〜M6 全体像）、
`2026-07-11-phase5-m2b-self-issued-noc-design.md`（自己発行 NOC、M3 の前提 capability）。
ブランチは長期ブランチ `matter-controller`（main マージ禁止、ユーザー決定 2026-07-10）。

## ゴール（親 spec の M3 受け入れ + M2b 最終レビュー繰り越し）

**jarvis 実機の Nanoleaf に、chip-tool の fabric へ相乗りして on/off・色変更が通る。**
chip-tool KVS リーダ自体は M2/M2b で先行実装済みなので、M3 の残りは:

1. **堅牢化（M2b 最終レビュー繰り越し、実機前に必須）**
   - [Important #1] `fabric_id` が `u64::from(fabric_index)`（KVS テーブル index 流用）。
     alpha（index 1 == id 1）でのみ偶然正しい。index ≠ id の fabric では NOC subject
     （tag 21）と `case_destination_id` が誤り、CASE がデバイスに拒否される。
   - [Minor #3] `LocalNodeId` が present-but-≠8B のとき黙って既定 112233 に
     フォールバック。corrupt KVS で ACCESS_DENIED の原因になる。
2. **mDNS/DNS-SD 解決**（M1 計画の申し送り「MRP の再送パラメータは M3 の mDNS 実装時に
   接続する」の回収）: operational service
   `<CompressedFabricId>-<NodeId>._matter._tcp.local` から IPv6/port を引き、
   TXT の SII/SAI を MRP 再送パラメータに接続する。
3. **色変更**: colorcontrol `MoveToHueAndSaturation` を IM invoke で送れること
   （初のフィールド付きコマンド。`encode_invoke_request` の `fields_tlv` は M2 実装済み）。
4. **実機受け入れハーネス**: aarch64-musl クロスビルド → jarvis 転送 → 実行の
   一発スクリプト（`task e2e:m3`）。

## 決定

### 決定 1: node_id / fabric_id は fabric テーブルの永続 NOC（`f/<idx>/n`）から読む

M2b は `LocalNodeId`（無ければ 112233）+ `fabric_id = fabric_index` だった。M3 では
**main KVS の `f/<idx>/n`（chip-tool 自身の operational NOC、Matter-TLV 形）を
`MatterCert::parse` でパースし、subject の node id（tag 17）/ fabric id（tag 21）を
定義的ソースとして使う**。

- 根拠: デバイスの ACL が admin を許すのは「commission した controller の NOC の
  node id」。その NOC そのものが `f/<idx>/n` に永続化されている（M2b の probe-kvs 実測で
  存在確認済み）。ここから読めば index ≠ id の fabric でも、`LocalNodeId` 既定値の
  変更でも壊れない。fabric id も同じ NOC の subject にあり、単一ソースになる。
- `LocalNodeId` の読み出しと `DEFAULT_CONTROLLER_NODE_ID` は**削除**する
  （NOC が常に優越する定義的ソースであり、fallback を残すと [Minor #3] の
  「黙ったフォールバック」構造が残るため）。これで繰り越し 2 件が両方閉じる。
- `f/<idx>/n` 欠落は `KeyMissing`、パース不能/subject 欠落は新エラー
  `KvsError::BadNoc { fabric_index, reason }`（実キー名を Display に出す既存流儀）。

### 決定 2: mDNS は自前の one-shot リゾルバ（legacy unicast クエリ）

親 spec のフルスクラッチ範囲に「mDNS/DNS-SD 解決」が含まれる。新モジュール
`mat-controller::dnssd` に最小実装:

- **legacy unicast 方式**（RFC 6762 §6.7: source port ≠ 5353 のクエリには
  応答者が我々へ直接 unicast で返す）。ff02::fb:5353 へ SRV+TXT の 2 question を
  送り、応答の SRV/TXT/追加レコードの AAAA を集める。target の AAAA が
  additional に無ければ AAAA を追撃クエリ。応答が来るまで 1 秒間隔で再送、
  呼び出し元指定の deadline で timeout。
- DNS ワイヤ codec（query encode + 圧縮ポインタ対応の response parse）は手書き
  （依存追加なし。ポインタループは hop 上限でガード）。
- インスタンス名は `format!("{:016X}-{:016X}", compressed_fabric_id, node_id)`
  （大文字 16 hex、jarvis の avahi-browse 実測と同形）。
- TXT の `SII`（session idle interval, ms）を `MrpConfig::initial_interval` に
  接続（CASE 開始時点のセッションは idle なので SII を使う。無ければ Matter 既定
  500ms、spec 上限 3600000ms でクランプ）。`SAI` も読んで保持（将来用）。
- リンクローカル以外のアドレス（Thread 機の fd../ULA）を優先して返す。
  マルチキャスト送信先の scope_id は呼び出し元が渡す（`/sys/class/net/<if>/ifindex`
  を読むヘルパを提供。ライブ試験は env で iface 指定 or リモートの default route から自動検出）。

**代替（棄却）**: mdns 系 crate の導入 — 親 spec がフルスクラッチ（依存は暗号
プリミティブのみ既製）を選好。必要なのは one-shot 解決だけで手書き ~400 行で足りる。
**代替（棄却）**: E2E に `MAT_E2E_PEER` 手渡しだけで済ませる — M4（matd in-process
化）が結局解決を必要とし、M1 申し送り（SII/SAI 接続）が宙に浮く。なお `MAT_E2E_PEER`
上書きは mDNS 障害切り分け用に残す。

### 決定 3: 色変更は colorcontrol `MoveToHueAndSaturation`（0x0300 / cmd 0x06）

既存の `mat color` と同じコマンド。`im` に cluster/attr/cmd 定数と
CommandFields エンコーダ（hue, saturation, transition_time, options_mask=0,
options_override=0 の struct）を追加。受け入れは実機で move 後に
`current-hue` / `current-saturation` を read して照合（デバイス量子化を考慮し
許容誤差 ±8）。

## スコープ

`mat-controller` crate + E2E ハーネスのみ。mat / matd は無変更（adapter 差し替えは M4）。

| モジュール | 変更 |
|---|---|
| `kvs` | 決定 1（`f/<idx>/n` パース、`LocalNodeId` 削除、`BadNoc` 追加） |
| `dnssd`（新規） | 決定 2（DNS codec + one-shot リゾルバ + MrpConfig 接続） |
| `im` | 決定 3（colorcontrol 定数 + fields エンコーダ） |
| `tests/live_jarvis.rs`（新規） | 実機受け入れ（自己発行 → 解決 → CASE → onoff/色） |
| `scripts/e2e-m3.sh` + Taskfile | クロスビルド → 転送 → 実行の一発化 |

## 受け入れ基準（M3、jarvis 実機）

`task e2e:m3`（要 `MAT_E2E_HOST` / `MAT_E2E_NODE_ID`）で以下が一括で通ること:

1. jarvis の chip-tool KVS（`~/.config/mat`）から CA 材料 + NOC subject の
   node/fabric id を取得（**fabric id は index 流用でなく NOC 由来**）
2. 自己発行 NOC で本番 fabric に相乗り
3. 我々の mDNS リゾルバが実機 Nanoleaf の IPv6/port/SII を解決
4. CASE 確立（Thread 実機、SII 由来の MRP 再送間隔で）
5. onoff: toggle → read 反転 → toggle → read 復元
6. colorcontrol: `MoveToHueAndSaturation` invoke 成功 + `current-hue` read が
   目標値 ±8。元の hue/sat に復元して終了
7. CI（`task check`）は実機なしで全通過のまま（ライブは `#[ignore]`）。
   既存 `task e2e:m2` も引き続き通る（node/fabric id ソース変更の互換確認）

## 非ゴール

- subscribe / browse（PTR 列挙）/ mDNS 応答側・広告側。リゾルバは one-shot query のみ。
- group セッション（M5）。matd adapter 差し替え（M4）。
- commissionable discovery（`_matterc._udp`、第二期）。
- chip-tool KVS フォーマットの複数バージョン互換（v1.4.2.0 に固定。上流更新で
  壊れたらユニットテストが検出する、が従来方針）。

## リスク

- **同一 node id の並行 CASE**: jarvis では matd（chip-tool interactive）が同じ
  node id で warm セッションを張っている可能性がある。Matter は同一 (fabric, node)
  からの複数 CASE セッションを許す（M2b 決定 2 と同根）ので衝突しない想定。
  万一デバイス側 session 資源逼迫で蹴られたら matd を止めて再試行（手順に記載）。
- **mDNS が jarvis 環境で引けない**: SRP 未登録ノード（node5 の既知事象）は
  広告ゼロで timeout する。切り分けは `avahi-browse -rtp _matter._tcp` と
  `MAT_E2E_PEER` 直指定フォールバック。
- **groupcast counter 混在禁止**（実機知見）は unicast のみの M3 には非該当。
