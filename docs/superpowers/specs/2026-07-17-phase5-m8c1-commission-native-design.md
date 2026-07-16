# Phase 5 M8c-1: commission native 化設計（+ M8c 3分割の記録）

2026-07-17 起草。親 spec: `2026-07-10-phase5-backend-direction-design.md`、
M8 全体分割: `2026-07-16-phase5-m8a-generic-im-native-design.md` 冒頭。
前提: M8a 完了・main マージ済み。**M8b はコード完了・最終レビュー合格
（branch `m8b-discover-native`）だが実機 E2E 待ち — M8c-1 の実装着手は
M8b の E2E 合格 + main マージ後**（本 spec の起草・レビューは先行してよい）。

## M8c 全体の 3 分割（ユーザー決定 2026-07-17）

M8c（commission native 化 + chip-tool 完全撤去）は M8a より大きく 1 spec に
収まらないため 3 分割する。各段で実機 E2E 合格を受け入れ条件とし、M8c-2 まで
chip-tool フォールバックが生きているため撤退可能。

- **M8c-1（本 spec、0.20.0）— commission native 化**: 既存 fabric 上の
  `mat commission` を M6a（on-network）/ M6b（BLE+Thread）実装へ配線。
  **KVS 書込なし**（commissioning は既存 fabric 上では KVS 書込を必要と
  しない — node 台帳は mat の store、デバイス側 NOC 発行は既存 read 系
  API で足りる）。attestation PAA 対応、`CommissioningError`→`ErrorKind`
  写像の追従（M6b fix-later の解消）、chip-tool フォールバック維持。
- **M8c-2（0.21.0）— KVS group 書込所有 + diag node 再訪**: controller 側
  `groupsettings`（chip-tool interactive）の native 化 = keyset / group
  table を mat が chip-tool INI 形式 KVS へ flock 排他で書く。matd
  provision のハイブリッド（M8a）解消。`diag node` の IM 部分 native 化。
- **M8c-3（0.22.0）— native 既定化 + chip-tool 完全撤去**: 初回 fabric
  bootstrap（root CA 生成 + KVS 新規作成 — **mat が唯一の読者になってから**
  やる。chip-tool が同じ KVS を読む間は「実 chip-tool も受理できる新規
  fabric」という最難の互換問題を抱えるため、撤去後に回すのが安全）、
  `MAT_IFACE` 未設定でも native（group 送信 iface の自動選択 — multicast
  egress の罠、tailscale0 が経路解決で勝つ問題の設計をここで詰める）、
  runner.rs / chip-tool 分岐 / fake-chip-tool テスト基盤の置換 / Docker・
  repo 直下バイナリ・`MAT_CHIP_TOOL_BIN` の全削除、avahi-browse 撤去、
  BLE ビルド既定化の判断。

### ビルド検証スパイクの結果（M8 横断決定③の宿題、2026-07-17 実施）

- **musl×bluer は現状ビルド不可**: bluer → dbus → `libdbus-sys`（C の
  libdbus）が pkg-config で見つからず失敗。vendored ビルド
  （`build_vendored.rs`）の道はあるが aarch64-musl の C クロスツール
  チェーンがローカルに無く、どのみち追加整備が要る。
- **確立済みの代替（M6b、Cross.toml に記録済み）**: `cross build --target
  aarch64-unknown-linux-gnu --features ble`（docker + arm64 libdbus）で
  動的リンクバイナリを作る。jarvis（Debian 13 / glibc / libdbus-1.so.3）で
  動作実績あり。
- **M8c-1 はこの cross gnu 経路を使う**（BLE 有効ビルドが必要な受け入れ
  E2E のため）。musl 経路は無変更で残す（BLE なし）。「本番ビルドで
  feature ble を既定有効化」（横断決定③）の最終形 — gnu 一本化 or
  musl+gnu 二本立て or vendored 整備 — は **M8c-3 で判断**する。

## M8c-1 のゴール

1. `MAT_IFACE` 設定時、`mat commission` が chip-tool を spawn せず native で
   完走する（on-network / BLE+Thread の両経路）。
2. 出力 JSON・台帳・alias の挙動は現行と完全同一。
3. `MAT_IFACE` 未設定時は完全無変更（chip-tool 経路）。

## ユーザー決定（2026-07-17）

- **BLE+Thread を M8c-1 に含める**（玄関ライトの HA 再アダプト等の実需要。
  feature `ble` ビルド限定、Thread dataset は新引数で注入）。
- **経路選択は自動 mDNS→BLE**（現行 `pairing code` の「コードだけ渡せば
  よい」UX 互換。明示フラグ方式は不採用）。
- **アプローチ = M7/M8a と同型の opt-in native op（案A）**。検討した代替:
  (B) M8c-1 で chip-tool フォールバックも同時撤去 — 段階的撤退の原則に
  反し不採用。(C) `--native` 明示フラグ — 既存 op 群と選択方式が不整合で
  不採用。

## 設計

### 1. native フロー（`MAT_IFACE` 設定時の `mat commission`）

1. setup code をパース（既存 `mat-controller::setup_code`）→ passcode +
   long discriminator。
2. node_id 決定（`--node-id` or 台帳 max+1 — 現行 `next_node_id` 維持）。
3. エンジン資材構築（`MAT_FABRIC_INDEX` 既定 1 / `MAT_ISSUER_INDEX` 既定 0、
   M7 と同じ env・既定値）。**構築失敗（KVS 不備等）→ warn +
   chip-tool フォールバック**（M7 と同型）。
4. **発見**: まず `resolve_commissionable(scope_id, long_discriminator,
   timeout)`（既存）で mDNS 探索 → 見つかれば **on-network commission**
   （M6a `commission_on_network`）。見つからず、`ble` ビルドかつ Thread
   dataset 指定あり → **BLE scan → `commission_ble_thread`**（M6b）。
   **どちらでも未発見 → warn + chip-tool フォールバック**（ワイヤ未接触
   なので二重実行の危険なし。chip-tool の探索が native と別の経路で
   見つける可能性も残す）。
5. **PASE 開始後の失敗 → 即エラー**（chip-tool 再試行しない — 部分
   commission 状態からの自動再実行は二重 commission を招く。unicast native
   失敗の「フォールバックしない」と同じ理屈）。
6. 成功 → `CommissionedDevice` を受けて台帳 upsert（address は現行どおり
   `target` 引数のメタ記録）・alias 登録・`{"node_id": N, "status":
   "success"}` 出力 — 現行と完全同一。

### 2. CLI / 資格情報の注入

- 新引数 **`--thread-dataset <hex>`**（env `MAT_THREAD_DATASET`）を
  commission サブコマンドに追加。BLE 経路でのみ使用。BLE デバイスを発見
  したが dataset 未指定 → 明確なエラー（`detail` に「--thread-dataset が
  必要」）。
- **PAA**: 現行の解決順（`MAT_PAA_TRUST_STORE` → `<store>/paa-trust-store`）
  をそのまま `CommissionParams.paa_dir` に接続。`None` = attestation
  チェーン検証は必ず失敗（M6b の「PAA 必須、警告なしで弱めない」を維持）。
- **CD signer**: 同型の解決順 `MAT_CD_SIGNER_STORE` →
  `<store>/cd-signer-store`。`None` でも続行（CD 検証は warn のみ、M6b
  決定どおり）。

### 3. 実装構造

- **`mat-native::commission` を新設**: `NativeConfig`（iface / fabric_index /
  issuer_index / store root）から資材（self-issue materials 等）を読み、
  `CommissionParams` を組んで `commission_on_network` /
  `commission_ble_thread` を呼ぶ薄いラッパー。プロトコル知識は backend
  crate に閉じる（設計ルール1）。matd には配線しない（commission は
  従来どおり matd 対象外）。
- **mat 側 `commands/commission.rs`** は discover.rs と同じ形の native
  分岐のみ: `Some(iface)` → native、戻りの失敗種別で
  fallback（未接触）/ hard error（接触後）を分岐。
- **BLE 分岐は `#[cfg(feature = "ble")]` を mat → mat-native →
  mat-controller まで貫通**（mat / mat-native に feature `ble` を追加し
  下位へ伝播）。musl ビルドでは BLE 分岐がコンパイル時に消え、BLE が
  必要な状況では「このビルドは BLE 非対応」の明確なエラー。
- **ErrorKind 写像（M6b fix-later の解消）**: `CommissioningError` →
  `ErrorKind` の写像を mat-native ラッパーに実装し、commissioning.rs の
  doc 表を親 spec 決定 4 に追従させる。分類: attestation 失敗 =
  `device_rejected` / PASE・CASE 確立の timeout（接触後）= `timeout` /
  Thread 参加失敗 = `unreachable` / TLV 等の不正応答 = `parse_error` /
  その他 = `commission_failed`（発見の空振りは未接触なのでエラーではなく
  フォールバック）。exit code 表は不変。
- **マーカーログ**（E2E 用、M8b と同流儀）: 成功経路で
  `commission executed (native on-network)` / `commission executed
  (native ble-thread)`（info）、フォールバックで
  `falling back to chip-tool`（warn）。

### 4. テスト

- 純ロジックのユニットテスト: setup code → 経路決定（mDNS ヒット /
  BLE+dataset / 未発見）、ErrorKind 写像、PAA/CD ディレクトリ解決。
- 統合テスト（fake-chip-tool）: `MAT_IFACE` 未設定 = 完全無変更の回帰、
  bogus iface → warn + chip-tool フォールバックで現行出力。
- native commission 本体は実機 E2E（M6a/M6b と同じ流儀 — PASE/BTP は
  実デバイスでしか検証できない）。

## 受け入れ基準（実機 E2E、jarvis）

1. 玄関ライト（現在 fabric 無し）を `MAT_IFACE` + `--thread-dataset` で
   **native BLE+Thread commission**（chip-tool spawn ゼロをログで実証）
   → on/off 制御 → 台帳記録確認。
2. 可能なら RemoveFabric → Thread 残留状態から **on-network 経路**でも
   commission（同一デバイスで両経路検証。玄関ライトの状態次第で WARN +
   人力確認に切替可）。
3. フォールバック健全性: `MAT_IFACE` 未設定で commission が従来どおり
   chip-tool 経由で成功（または現行同等の失敗分類）。
4. `task check` 全通過。

## やらないこと（M8c-1）

- KVS への書込・初回 fabric bootstrap（M8c-3）。
- groupsettings / diag node の native 化（M8c-2）。
- chip-tool / avahi 経路の削除、native 既定化（M8c-3）。
- matd への commission op 追加（恒久的に対象外）。
- BLE ビルドの既定有効化（M8c-3 で判断。M8c-1 は cross gnu の opt-in
  ビルドで E2E する）。
