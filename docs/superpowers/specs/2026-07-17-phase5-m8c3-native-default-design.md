# Phase 5 M8c-3: native 既定化 + chip-tool 完全撤去 + fabric bootstrap 設計

2026-07-17 起草。親 spec: `2026-07-10-phase5-backend-direction-design.md`、
M8c 全体 3 分割の記録: `2026-07-17-phase5-m8c1-commission-native-design.md`
冒頭。前提: M8c-2 完了・実機 E2E 合格・main マージ済み（64d5795）。
バージョンは **0.22.0**、branch `m8c3-native-default`。Phase 5 の最終
マイルストーン（chip-tool 退役）。

## ゴール

1. `MAT_IFACE` / `MAT_MATD_IFACE` 未設定でも全 op が native で動く
   （iface は自動検出、env は明示上書きとして存続）。
2. chip-tool / avahi-browse 経路をコード・テスト基盤・Docker イメージ・
   ドキュメントから完全撤去。mat が KVS の唯一の読み書き者になる。
3. `mat fabric init` で初回 fabric bootstrap（root CA 生成 + KVS 新規作成 +
   **ランダム epoch IPK** 生成・永続）ができ、chip-tool 既定定数
   `temporary ipk 01` への依存から脱却する（M8c-1 spec の必須項目）。

## ユーザー決定（2026-07-17、ブレスト時）

- **1 spec + 1 実装計画で一気に**（M8c-3a/3b への再分割はしない）。
  ただし内部を二段構えにし、撤去（不可逆）の前に実機 E2E ゲートを置く。
- **BLE ビルドは gnu 一本化**: deploy 成果物を
  `cross build --target aarch64-unknown-linux-gnu --features ble` に統一、
  musl deploy 経路は廃止（検討した代替: musl+gnu 二本立て = 機能差の恒久化
  と CI 二重化で不採用 / vendored libdbus + musl C ツールチェーン整備 =
  コスト未知数で不採用）。ローカル開発 (`task check`) は host build・
  ble なしのまま無変更。
- **既存 fabric（定数 epoch）は「検証して採用永続」**: 初回 native
  commission 時に既存 KDF ガード（`verify_default_ipk_epoch`）で定数を
  検証し、一致したら mat の KVS キーへ永続（adopt）。以降は新規 fabric と
  同じ「KVS から epoch 読み出し」に一本化。IPK ローテーション（全ノード
  KeySetWrite）は将来の別件（検討した代替: 完全移行 = オフライン/間欠
  ノードで fabric 分裂リスク / 定数を毎回検証 = 定数依存が恒久化、で
  どちらも不採用）。
- **テスト基盤はトレイト fake + 最小バイナリテスト**: バックエンド挙動は
  既存 `mat-native::test_support::FakeConn` に寄せ、バイナリ spawn テストは
  バックエンド不要な範囲に縮小。fake Matter デバイス（UDP loopback で
  PASE/CASE/IM 応答）は将来マイルストーン候補として ARCHITECTURE に記録
  のみ（IM responder 側の新規実装が必要で M8c-3 が大幅に膨らむため不採用）。
- **iface 自動検出は毎回実行・候補複数はハードエラー**: 状態を持たず
  （設計規則 4）、曖昧なら候補名を列挙して `MAT_IFACE` 指定を促す。
  決定的に 1 つ選ぶ+warn 方式は、誤 iface での group 送信サイレント不達
  （カウンタ汚染の前科あり）のリスクで不採用。store への永続方式は
  stale 化リスクで不採用。
- **list/struct/float の汎用 write/invoke は後退を受容し文書化**:
  native は scalar のみが仕様（従来は chip-tool 経路が担っていた）。
  実用上の主要ユースケース（onoff/level/color、`group grant` の ACL
  エントリ追加）は native 対応済み。汎用 list/struct TLV エンコードは
  将来候補として ARCHITECTURE に記録。数値 ID 指定は既存機能として残る
  逃げ道（name→ID テーブル未解決の名前も同様）。
- **fabric bootstrap は明示サブコマンド `mat fabric init`**: KVS 不在時の
  自動作成は store パス typo でサイレントに別 fabric が生える事故が怖く
  不採用（現行の `store_missing` エラー挙動を維持）。

## 全体構成: 二段構え

### Stage 1 — native 既定化（chip-tool フォールバック温存）

- iface 自動検出（下記）。`MAT_IFACE` / `MAT_MATD_IFACE` 未設定でも
  native 経路に入る。
- 既存 fabric の epoch 採用永続（下記）。
- chip-tool フォールバック・avahi フォールバックはコード上まだ生きて
  いる（発火しないことを E2E で実証するのがゲート 1）。
- → **実機 E2E ゲート 1**: jarvis 実運用 fabric、env 未設定で全 op
  native 完走、「falling back to chip-tool」発火ゼロ。

### Stage 2 — 完全撤去 + fabric bootstrap（ゲート 1 全 GREEN が着手条件）

- chip-tool / avahi-browse 経路の全削除、テスト基盤置換、Docker
  スリム化、gnu 一本化。
- **`mat fabric init` はここで実装**。理由: ランダム epoch で bootstrap
  した KVS に chip-tool が触ると、chip-tool は commissioner 初期化のたび
  定数 epoch 由来の operational 鍵を `f/<idx>/k/0` へ上書きするため、
  フォールバックが生きている Stage 1 に出すと自壊リスクがある。
  ARCHITECTURE の注記「mat が唯一の読者になってからやる」と整合。
- → **実機 E2E ゲート 2**（最終受け入れ、下記）。

## 設計 1: fabric bootstrap（`mat fabric init`、Stage 2）

- 直経路のみ（matd ソケットプロトコル対象外 — diag thread / open-window /
  group grant / commission と同じ扱い）。
- フロー: 既存 M2b 機構（`CommissioningFabric::generate()` /
  `SelfIssueMaterials`）で root CA・fabric 資材を生成 → OS 乱数
  （`getrandom` 系）で 16 バイト epoch IPK を生成 →
  `derive_ipk_operational` で operational へ KDF 導出 → chip-tool INI
  形式 KVS へ新規書き出し（M8c-2 の `mat-controller::group_settings`
  ライター基盤 = flock 排他 + tmp+rename atomic replace を流用）+
  mat-epoch キー（下記）も同時に書く。
- **KVS が既に存在すれば拒否**（`--force` なし。上書きは手動削除前提）。
- 他コマンドは KVS 不在なら従来どおり `store_missing`（exit 10）。detail
  に「run `mat fabric init`」の誘導を追記。
- 出力: fabric 情報の JSON（`timestamp` / store パス / `fabric_id` /
  `compressed_fabric_id` 等。ドキュメントのサンプルはダミー値のみ）。

## 設計 2: epoch の永続と解決順（Stage 1 から一本化）

- KVS に mat 専用キーで epoch を base64 永続。キー名は chip-tool の
  名前空間（`f/<idx>/...` 等）と衝突しない `mat/...` 系とし、正確な
  キー名・INI エスケープ規則の確認は実装計画で確定する。chip-tool は
  未知キーを無視するため Stage 1 の共存も安全。
- 解決順（native で epoch が必要な op = commission の AddNOC IPK）:
  1. KVS の mat-epoch キー → あればそれを使用。
  2. 無ければ定数 `CHIP_TOOL_DEFAULT_IPK_EPOCH` を既存 KDF ガード
     `verify_default_ipk_epoch` で検証。一致なら**その場で KVS へ採用
     永続**（flock 書込）してから使用。書込失敗（`WouldBlock` 含む）は
     M8c-2 と同じくハードエラー（フォールバックしない）。
  3. 不一致（非 chip-tool fabric / IPK ローテーション済み）→
     `store_parse` ハードエラー（exit 10）。Stage 1 でもこの op は
     chip-tool へフォールバックさせない（M8c-1 の「不一致 →
     フォールバック」から挙動変更 — 恒久挙動へ前倒し）。
- 採用永続は初回 native commission 時に自然発生。E2E ゲート 1 で実測
  確認する。

## 設計 3: native 既定化（iface 自動検出）と matd

- **経路選択の最終形（per-op）**: matd 自動発見（無変更）→ native
  （常時。iface = `MAT_IFACE` or 自動検出）。chip-tool 直経路は Stage 2
  で消滅。
- **iface 自動検出**（mat / matd 共通実装、`mat-native` に置く）:
  - 候補条件: operstate up（carrier 有 — 未使用 docker0 等を除外）・
    MULTICAST・非 loopback・**非 POINTOPOINT**（tailscale0 / tun 系を
    除外）・IPv6 link-local アドレス保有。
  - 候補ちょうど 1 つ → 採用（debug ログに iface 名）。0 または 2 つ
    以上 → 構造化エラー（kind `other`、detail に候補名列挙 +
    「set MAT_IFACE」）。**Stage 1 からハードエラー**（曖昧なまま
    chip-tool へ黙って落とすとゲート 1 の発火ゼロ検証が汚れるため）。
  - 毎回実行時に検出、状態は持たない。jarvis（eth0+tailscale0）/ WSL
    （eth0）は実質 1 候補で自動決定。
  - 候補選別ロジックは純関数に切り出し、フラグ組合せの表駆動ユニット
    テストを書く。
- **matd**: `MAT_MATD_IFACE` 未設定なら起動時に同じ自動検出。曖昧なら
  **起動拒否**（全 op が死ぬ設定不備なので per-op エラーでなく
  fail-fast。jarvis の systemd unit は env 設定済みで影響なし）。
  Stage 2 で matd の chip-tool 全面フォールバック（資材不足時）は
  per-op の構造化エラーへ変わる。
- **フォールバック消滅後の残存エラー化（Stage 2）**:
  - name→ID テーブル未解決の名前 → `parse_error` + detail
    「unknown cluster/attribute/command name」（数値 ID が逃げ道）。
  - list/struct/float → 従来どおり `parse_error`（後退受容、README に
    仕様として明記）。
  - mDNS I/O エラー → avahi-browse フォールバック消滅、そのままエラー。
  - KVS 資材不足 → `store_missing` / 不整合 → `store_parse`。

## 設計 4: 撤去範囲（Stage 2）とビルド一本化

- **コード削除**:
  - `crates/mat`: `runner.rs`（chip-tool spawn / パーサ）、各コマンドの
    chip-tool 分岐、`MAT_CHIP_TOOL_BIN`、probe / discover の
    avahi-browse フォールバック。
  - `crates/matd`: `backend.rs` の chip-tool ランナー、`server.rs` の
    フォールバック分岐。
  - chip-tool 由来の stdout/stderr パーサ（`Data = ...` パス等）で他 op
    から参照されないものは削除。exit 3/4/5/6 へのエラー分類は native 側
    `ErrorKind` 写像が既に担っており存続。
- **exit code 12（chip-tool not found）は廃止**: README の表で
  「0.22.0 で廃止（歴史的欠番として予約）」と明記。他の kind / exit
  code は無変更。**新しい error kind は追加しない**（iface 曖昧 =
  `other`、KVS 系 = `store_missing` / `store_parse` で吸収）。
- **ビルド・配布**: deploy 成果物 = aarch64-gnu + `--features ble`
  （Taskfile に deploy ビルドタスクを正式化）。musl deploy 経路
  （Cross.toml の musl 設定・関連手順）は削除。ローカル開発は無変更
  （`ble` は opt-in cargo feature のまま。WSL に libdbus 不要）。
- **Docker**: chip-tool の焼き込みを削除しスリム化（mat / matd バイナリ
  のみ）。`docker:test`（ツールチェーンイメージ）は維持。host
  networking 要件は変わらず。
- **テスト・スクリプト**: `fake-chip-tool.sh` 削除。旧マイルストーン
  E2E スクリプト（e2e-m2〜m8c2）は実装計画で個別判断（現行機能の検証
  として再利用価値があるものは native 前提に書き換え、純粋に歴史的な
  ものは削除）。新規 `scripts/e2e-m8c3-real.sh`（ゲート 1 / 2 両対応）
  を追加。
- **ドキュメント**: README（Backend 節全面書き換え・環境変数表・エラー
  表・scalar 限定の仕様明記）、ARCHITECTURE（M8c-3 完了記録 + 将来候補
  として fake Matter デバイス / 汎用 list/struct TLV エンコード / IPK
  ローテーションを記録）、CLAUDE.md（Backend 節）。

## 設計 5: テスト戦略

- **自動テスト（`task test`、実デバイス不要）**:
  - バックエンド挙動は `FakeConn`（トレイトレベル）へ寄せる。既存の
    FakeConn 系ユニットテストは温存・拡充。
  - バイナリ spawn テスト（integration.rs 置換）はバックエンド不要な
    範囲に縮小: arg エラー（exit 2）、alias 解決、store 系エラー
    （exit 10/11）、iface 自動検出のエラー形状、**`fabric init` の
    ローカル完結フル E2E**（新規 KVS 生成 → INI 内容検証 → 既存 KVS で
    拒否 → 再 init 拒否）。
  - matd 統合テストも同様に fake-chip-tool 依存を除去。
- **実機 E2E ゲート 1（Stage 1 完了時、jarvis・既存 fabric）**:
  1. env 未設定で discover / read / write / invoke / describe /
     diag thread / diag node / open-window / group provision /
     group invoke / commission が全て native 完走（直経路 + matd 経路。
     matd 対象外 op は直経路のみ）。
  2. 全ログで「falling back to chip-tool」発火ゼロ。
  3. 初回 commission で epoch 採用永続が発生し KVS に mat-epoch キーが
     書かれ、2 回目以降は KVS 読み出しで通る。
  4. iface 自動検出が jarvis（eth0+tailscale0）で eth0 を一意選択。
- **実機 E2E ゲート 2（Stage 2 完了時・最終受け入れ）**:
  1. chip-tool を PATH から外した環境で上記 op スイープ再実行・全合格。
  2. `mat fabric init` 実機検証: 別 store で init → 既存ノードに
     open-window → 新 fabric へ commission → read → RemoveFabric で掃除
     （実運用 fabric は無傷）。
  3. deploy 成果物（aarch64-gnu + ble）が jarvis で動作（BLE commission
     経路含む。デバイス都合が悪ければ M8c-1 と同様 WARN + 人力確認へ
     切替可）。
  4. `task check` 全通過、Docker イメージビルド成功。

## リスクと撤退

- Stage 1 まではフォールバック温存で撤退可能。**Stage 2 着手 = 退路を
  断つのはゲート 1 全 GREEN が条件**。
- 本番デプロイ（0.22.0、現行本番は 0.19.0）は E2E 後のユーザー判断
  （これまでのマイルストーンと同じ運用）。

## やらないこと（M8c-3）

- IPK ローテーション（全ノード KeySetWrite での epoch 完全移行）。
- 汎用 list/struct/float の TLV エンコード（scalar 限定を仕様化）。
- fake Matter デバイス（UDP loopback responder）テスト基盤。
- matd への commission / fabric init op 追加（恒久的に対象外）。
- vendored libdbus / musl BLE ビルド整備。
- スコープ外リマインダ全般（CLAUDE.md）は従来どおり。
