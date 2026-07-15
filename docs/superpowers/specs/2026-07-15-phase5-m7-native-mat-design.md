# Phase 5 M7: native 版 mat（one-shot 直経路の native 化）+ 本番 matd の native 化 設計

2026-07-15 起草。親 spec: `2026-07-10-phase5-backend-direction-design.md`。
前提: M1〜M6b 実装・実機 E2E 合格済み（M6b = BLE+Thread commissioning を
chip-tool 無しでフル完走、head `b12746a`）。本 spec は親 spec の未決事項
「mat 直経路（one-shot）を新 crate に載せ替える時期」への答え =「今、M7 で、
本番 matd の native 有効化と同時に」（ユーザー決定 2026-07-13 / 2026-07-15）。

## ゴール

1. **mat one-shot 直経路に matd と同じ native ホットパスを実装する**
   （unicast: on/off/color/color-temp/onoff read、group: onoff 引数なし
   on/off/toggle・color・color-temp）。対象外 op は従来どおり chip-tool 直。
2. **本番 jarvis の matd を native 有効（`MAT_MATD_IFACE=eth0` +
   `MAT_MATD_FABRIC_INDEX=2`）で運用開始する。**
3. 実装は matd の `native.rs` から**共有クレート `mat-native` を抽出**して
   一本化する（one-shot と常駐でエンジンを共有、挙動パリティを構造的に保証）。

chip-tool 完全廃止（write/describe/diag/discover/commission の native 化、
汎用 name→ID テーブル、KVS 書込所有、バイナリ撤去）は **M8 スコープ**。

## 決定 1: 共有エンジン抽出（案 1、ユーザー承認 2026-07-15）

新 crate **`crates/mat-native`**（依存: `mat-core` + `mat-controller`）に、
matd `native.rs`（849 行）からプロセス形態非依存のコアを移す:

- `NativeConfig`（store / iface / fabric_index / issuer_index）とエンジン構築
  （KVS 読取 → NOC 自己発行 → UDP transport bind → scope_id 解決）。
- `Establisher` / `NodeConn` trait と実実装（mDNS 解決 + CASE 確立、
  onoff/color/color-temp invoke、onoff read）。テスト用 fake も共有側に置く。
- `GroupCtx` / group 送信 / `GroupOutcome` / MatError（ErrorKind）写像。

matd に残るのは matd 固有部分のみ: warm セッション slot 管理（per-node
HashMap）、`Op` → native 形判定（`is_native_hotpath` / `native_group_params`）、
server / protocol 層。**matd の外部挙動（socket protocol・JSON・フォール
バック分岐）は不変**で、既存の matd 統合テストが無改変で全通過することを
抽出リファクタの回帰ガードとする。

検討した代替: (2) mat 側に軽量 native クライアントを別実装 — matd 無改修で
リスク最小だが確立手順・エラー写像・counter 扱いが二重化し、M8 での統合が
どうせ必要（負債の先送り）。(3) matd 自動起動に寄せる — mat one-shot の哲学
（デーモン不要）と衝突。いずれも不採用。

## 決定 2: mat 直経路の配線

- **有効化**: グローバル `--iface` / 環境変数 **`MAT_IFACE`**（+
  `MAT_FABRIC_INDEX`、既定 1 / `MAT_ISSUER_INDEX`、既定 0）。未設定なら全 op
  が従来どおり chip-tool 直で**挙動変化ゼロ**（opt-in、matd M4 と同じ思想）。
  matd の `MAT_MATD_IFACE` とは別名（別プロセスの設定を混ぜない）。
- **経路優先順位**（op 単位）: ① matd 自動発見（従来どおり最優先）→
  ② native 直（iface 設定時、native 対象 op のみ）→ ③ chip-tool 直。
- **one-shot モード**: 確立 → 1 op → 破棄。warm セッション・キャッシュは
  持たない（設計ルール 4「credential KVS 以外の状態を持たない」維持。
  group counter ファイルは M5 で導入済みの `<store>/native_group_counter` で
  KVS の一部扱い）。mat は同期バイナリのまま、native op 実行時のみ
  current-thread tokio runtime を `block_on` する。
- **失敗分岐は matd と同型**: エンジン構築失敗（KVS 不備等）→ warn +
  chip-tool フォールバック / unicast op 失敗 → ErrorKind 写像で即エラー
  （フォールバックしない — 二重実行回避、matd M4 と同じ理由）/ group native
  不可（`GroupOutcome::Unavailable`）→ chip-tool フォールバック + counter
  混在 warn。
- **stdout の JSON スキーマは完全不変**（既存スキーマで re-emit のみ）。

## 決定 3: group counter のプロセス間共有

one-shot native の group 送信も matd と同じ `<store>/native_group_counter` を
使う。

- one-shot は counter ファイルを **flock 排他**で開き、jump-ahead
  （max(自前, `g/gdc`)+4096）→ persist → 送信 → クローズ。matd 側の counter
  永続化書込にも flock を足し、ファイル破損を防ぐ。
- matd 稼働中は経路優先順位①により group 送信は matd に回るため通常は
  衝突しない。**`MAT_MATD=0` 強制直 + matd 稼働中の group 送信は従来どおり
  禁止**（matd のメモリ上 counter と意味的に混在するため。既存の
  「直経路/matd 混在禁止」実機知見と同じ扱いで README 注記を維持）。
- one-shot 1 回毎の +4096 消費は chip-tool one-shot の `g/gdc` +4096 と同等
  （現状維持、許容）。

## 決定 4: ブランチ運用 — main マージ解禁（ユーザー決定 2026-07-15）

M7 の実装・実機 E2E 合格後に **`matter-controller` を `main` にマージ**し、
以降は従来どおり main から本番デプロイする。2026-07-10 のマージ禁止は
Phase 5 の実験性が理由であり、M1〜M6 全実機 E2E 合格で解消した。本番=main の
原則を回復し、長期別ブランチ運用の事故リスクを畳む。バージョンは **0.17.0**。

## 本番切替の段取りとロールバック

1. 実装 + `task check` 全通過（matter-controller ブランチ、worktree 運用）。
2. 実機 E2E `task e2e:m7`（jarvis、下記受け入れ基準 1〜3）。
3. matter-controller → main マージ（方式は finishing-a-development-branch で
   確認）。
4. 本番デプロイ: main から aarch64-musl クロスビルド → scp → systemd unit に
   `MAT_MATD_IFACE=eth0` / `MAT_MATD_FABRIC_INDEX=2` を追加 → restart →
   起動ログ "native backend enabled" を確認。
5. 本番受け入れ（受け入れ基準 4〜5）。

**ロールバック二段構え**: unit から env を消して restart すれば native 無効
（全 op chip-tool、M4 の安全フォールバック）。バイナリも旧 0.16.0 に戻せる。

## 受け入れ基準

1. **one-shot 直 native（実機）**: matd 停止状態 + `MAT_IFACE=eth0
   MAT_FABRIC_INDEX=2` で unicast on/off/color/color-temp/onoff read が成功し、
   group 3 形が living_lights（group 10）で 7/7 配達。
2. **counter 共有（実機）**: 1 の後に matd（native）を再起動し、本番経路の
   group 送信が 7/7（jump-ahead がプロセス間で正しく機能する実証）。
3. **フォールバック（実機）**: native 対象外 op（describe / diag / write）が
   chip-tool 経由で従来どおり成功。
4. **本番 matd native**: 本番 systemd matd が native 有効で起動し、warm
   unicast（on/off/color/color-temp、warm ~100ms 台）+ group 7/7 +
   フォールバック op が動く。
5. **回帰**: `task check` 全通過。特に matd の既存統合テストは**無改変**で
   通ること（共有エンジン抽出の回帰ガード）。mat の既存 fake-chip-tool
   統合テストも無改変で通ること（`MAT_IFACE` 未設定 = 挙動不変の証明）。

## テスト方針

- `mat-native` へ移す unit / fake テストは移設して維持。
- mat 側の新規統合テスト（fake `Establisher`）: native 対象 op の判定 /
  `MAT_IFACE` 未設定時の挙動不変 / エンジン構築失敗フォールバック /
  stdout JSON スキーマ不変。
- flock まわり: 同時アクセスの unit テスト（別プロセスは不要、fd 二重 open で
  排他を検証）。

## fix-later 回収（M6 からの持ち越し）

- `commissioning.rs` の ErrorKind 写像 doc 表を M6a spec 決定 4 に追従
  （doc のみ。native 版 mat 配線の前提整備）。
- `thread_ext_pan_id` の境界テスト追加。

## ドキュメント変更

- README: `MAT_IFACE` / `MAT_FABRIC_INDEX`、直経路 native の説明、group
  counter 注意の更新。
- ARCHITECTURE.md: Phase 5 に M7 節を追記。親 spec 未決事項「mat 直経路の
  載せ替え時期」を解決済みに。「KVS フォーマット互換の保証範囲」は M8
  （chip-tool 完全廃止）送りと明記。
- CLAUDE.md: 経路優先順位（matd → native 直 → chip-tool 直）の一文を追記
  （必要最小限）。

## スコープ外（M8 へ）

- write / describe / diag / discover / commission（one-shot CLI）の native 化。
- 汎用 cluster / attribute / command name→ID テーブル。
- KVS への書込所有（group provision native 化を含む）と chip-tool バイナリ撤去。
- subscribe（親 spec 未決のまま温存）。
