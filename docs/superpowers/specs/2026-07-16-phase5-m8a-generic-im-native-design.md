# Phase 5 M8a: 汎用 IM native 化（name→ID テーブル + read/write/invoke/describe/diag/open-window/group provision）設計

2026-07-16 起草。親 spec: `2026-07-10-phase5-backend-direction-design.md`。
前提: M1〜M7 実装・実機 E2E 合格・main マージ済み（本番 jarvis は 0.17.0 で
native 有効運用中）。本 spec は **M8（chip-tool 完全廃止）の第一弾**であり、
冒頭に M8 全体の 3 分割と横断的決定を記録した上で、M8a の設計を詳述する。

## M8 全体分割（ユーザー決定 2026-07-16）

M8 = chip-tool 完全廃止は規模が大きい（5 コマンド族の native 化 + name→ID +
KVS 書込所有 + バイナリ撤去）ため、M6a/M6b と同様にサブマイルストーンへ
3 分割する。各段で実機 E2E 合格を受け入れ条件とし、M8a/M8b までは chip-tool
フォールバックが生きているため撤退可能。

- **M8a（本 spec、0.18.0）— 汎用 IM native 化**: name→ID 全クラスタ生成
  テーブル + IM Write / wildcard read 実装 + `read`（汎用形）/ `write` /
  `invoke`（汎用形）/ `describe` / `diag` / `open-window` / `group`
  provision・grant 系の native 化（one-shot 直経路と matd の両方）。
- **M8b（0.19.0）— discover native 化**: mDNS browse（`_matter._tcp`
  operational + `_matterc._udp` commissionable）+ probe reachability。
  既存 `dnssd.rs`（operational 解決）の browse 拡張。
- **M8c（0.20.0）— commission native 化 + chip-tool 完全撤去**: 本番
  `mat commission` の native 化、KVS 書込所有、native 既定化、
  runner.rs / chip-tool 分岐 / fake-chip-tool テスト基盤 / Docker の
  chip-tool / `MAT_CHIP_TOOL_BIN` の全削除。

### M8 横断の決定（ユーザー決定 2026-07-16、主に M8c で実施）

1. **KVS 書込所有 = chip-tool INI 形式を継続**。既存 `kvs.rs` リーダと実機の
   fabric データをそのまま活かし、同じ形式で mat が書く（flock 排他）。
   マイグレーション不要・既存ノード無害。chip-tool 撤去後は mat が唯一の
   ライターになるため、親 spec の未決事項「chip-tool KVS のフォーマット互換を
   どのバージョン範囲で保証するか」は**自然消滅**（自分が書いた形式だけ読めば
   よい。読み側の基準は従来どおり chip-tool v1.4.2.0 形式に固定）。
2. **name→ID = 全クラスタ生成テーブル**（M8a で実施、下記決定 1）。
3. **BLE = 本番ビルドで feature `ble` を既定有効化**（M8c）。bluer は
   D-Bus（unix socket）経由なので musl でもリンク可の見込み — **musl×bluer
   ビルド検証を M8c の最初のタスクに置く**。BlueZ 無し環境でも BLE を使わない
   限り影響なし。ARCHITECTURE.md の M6b 記述（「本番バイナリは BLE 依存を
   持たない」）は M8c で改訂する。
4. **完全撤去**（M8c）: native を既定化（`MAT_IFACE` 未設定でも動作。group
   送信の iface は自動選択 + 設定で上書き — multicast egress の罠があるため
   自動選択の設計は M8c spec で詰める）。chip-tool 経路はコード・Docker
   イメージとも全削除。戻しは git revert + 旧バージョンバイナリ。

## M8a のゴール

1. **任意クラスタの read / write / invoke が chip-tool 記法の名前のまま
   native で通る**（数値 ID 直指定も常に許可）。
2. **describe / diag / open-window / group provision・grant 系が native で
   通る**（出力 JSON スキーマは完全維持）。
3. one-shot 直経路（`MAT_IFACE`）と matd（`MAT_MATD_IFACE`）の**両方**に
   配線し、経路優先順位・失敗分岐は M7 と同型のまま（chip-tool は M8c まで
   フォールバックとして残る）。

## 決定 1: name→ID 全クラスタ生成テーブル

- **生成元**: connectedhomeip の data model XML（chip-tool と同じ zap 定義、
  既存 KVS リーダと同じ **v1.4.2.0 タグに固定**）。
- **方式**: 生成スクリプト（`scripts/` に同梱、XML → Rust）を一度実行し、
  **生成済み Rust テーブルをチェックイン**（ビルド時に XML・ネットワーク
  不要）。再生成手順はスクリプト冒頭に記載。
- **置き場所**: `mat-core` 内の新モジュール（mat / matd / mat-native から
  共用）。
- **内容**:
  - cluster 名（chip-tool kebab-case 記法）→ cluster ID
  - (cluster, attribute 名) → attribute ID + **型タグ**（write の TLV 符号化
    に必須）
  - (cluster, command 名) → command ID + **フィールド順序と型**（invoke の
    positional args を TLV フィールドに写すのに必須）
  - 逆引き（ID → 名前、describe 等の出力用）
- 数値 ID 直指定（cluster / attribute / command とも）は全経路で常に許可。
  cluster / attribute 名の意味論は従来どおり chip-tool 記法（CLAUDE.md の
  「Cluster / attribute names stay chip-tool notation」を維持 — テーブルは
  その記法を native で解決するためのもの）。

検討した代替: (2) 利用クラスタのみ手書きの小テーブル — 工数最小だが未知
クラスタが数値 ID 必須になり `mat read` の互換が狭まる。(3) 段階導入 —
M8c 前に結局全テーブル化するので二度手間。いずれも不採用（ユーザー決定:
全クラスタ生成）。

## 決定 2: IM 拡張（mat-controller）

- **WriteRequest / WriteResponse の encode/decode を追加**（IM Write は
  現状未実装）。timed write 対応（`encode_timed_request` は M6a 実装済みを
  流用）。
- **attribute wildcard read を追加**（cluster 内全属性の一括 read）。
  describe / diag が複数属性を一往復で取るために使う。

## 決定 3: JSON→TLV の型サポート範囲

- 汎用 `write` / `invoke` は**スカラー型のみ**: bool / int / uint / enum /
  bitmap / string / octstr。CLI 入力文字列を生成テーブルの型タグに従って
  TLV に符号化する。
- **list / struct の汎用符号化はやらない**。未対応型は `parse_error` で
  明示拒否（detail に「この属性は list/struct 型のため M8a の汎用 write では
  未対応」と型名を含める）。
- group provision・grant 系が必要とする list/struct 書込（KeySetWrite・
  GroupKeyMap write・binding write・ACL read-modify-write）は形が固定なので
  **専用エンコーダ**として実装（既存 `group.rs` / `acl.rs` の延長。生成
  テーブルの struct スキーマには依存しない）。

## 決定 4: 配線

- one-shot 直経路の `NativeOp` と matd の native 判定に、Read汎用 / Write /
  Invoke汎用 / Describe / Diag / OpenWindow / Provision 系を追加。
- **経路優先順位は M7 と同型**（op 単位: ① matd 自動発見 → ② native 直
  （iface 設定時）→ ③ chip-tool 直）。失敗分岐も同型: エンジン構築失敗 →
  warn + chip-tool フォールバック / unicast op 失敗 → ErrorKind 写像で
  即エラー（フォールバックしない、二重実行回避）/ group native 不可 →
  chip-tool フォールバック。
- `MAT_IFACE` / `MAT_MATD_IFACE` 未設定なら従来どおり全 op が chip-tool
  （**挙動変化ゼロ**、opt-in は M8c の既定化まで維持）。
- **出力 JSON スキーマは完全維持**（既存統合テストがゴールデン）。エラーは
  mat-native の既存 `ErrorKind` 写像を流用。
- open-window は M6b で native 実装済み（commissioning.rs）— 配線のみ。

## テスト

- **単体**: 生成テーブルのスポットチェック（onoff / level / color /
  descriptor / thread-diag / group 系の既知 ID と突合、往復変換）、IM Write /
  wildcard read の encode/decode、JSON→TLV 型変換（正常系 + 未対応型の明示
  拒否）、provision 専用エンコーダ。
- **統合**: 既存 fake-chip-tool テストは**全維持**（chip-tool 経路は M8c
  まで生きるため回帰対象）。native 経路は mat-controller / mat-native 既存の
  モック方式（fake Establisher / mock link）。
- **実機 E2E**: `task e2e:m8a:real`（jarvis）— read 汎用 / write / describe /
  diag / open-window / group provision を native 直経路で実証（chip-tool
  未 spawn の確認込み）+ matd native 経由でも同 op を確認。

## 受け入れ基準（5 項目）

1. one-shot 直・matd の両経路で対象 op が native 動作（chip-tool 未 spawn を
   実機で実証）。
2. 出力 JSON スキーマ回帰なし（`task check` 全 green、既存統合テスト無改変で
   通過）。
3. 実機 E2E 合格（既存ノードへの実 read / write / describe / diag、
   living_lights への provision 再実行で N/N 配達維持）。
4. `MAT_IFACE` / `MAT_MATD_IFACE` 未設定時は従来どおり全 op が chip-tool
   （挙動無変化）。
5. 汎用 write / invoke の未対応型（list / struct）が `parse_error` で明確に
   拒否される。

## 運用

- **ブランチ**: `matter-controller`（温存済み worktree
  `.claude/worktrees/phase5-m1-controller-core`）を main（b15a739 マージ後）に
  追従させて M8a を実施。実機 E2E 合格後に main へ `--no-ff` マージ
  （M7 決定 4 と同じ運用）。
- **バージョン**: 0.18.0。
- **ドキュメント**: ARCHITECTURE.md の Phase 5 節に M8 の 3 分割と横断決定
  4 点を追記（M8a 完了時に実績も追記して最終化）。

## 非ゴール（M8a では行わない）

- discover の native 化（M8b）。
- commission の native 化・KVS 書込・native 既定化・chip-tool 撤去（M8c）。
- list / struct の汎用 JSON→TLV 符号化（必要になったら M8c 以降で再評価）。
- subscribe / イベント read（親 spec の非ゴールを維持）。
