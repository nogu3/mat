# Phase 5 M8c-2: KVS group 書込所有 + diag node native 化設計

2026-07-17 起草。親 spec: `2026-07-10-phase5-backend-direction-design.md`、
M8c 3 分割の記録: `2026-07-17-phase5-m8c1-commission-native-design.md` 冒頭。
前提: M8c-1 完了・実機 E2E 合格・main マージ済み（ca93946）。

## ゴール

1. `MAT_IFACE` 設定時、`mat group provision`（直経路・matd 経路とも）の
   コントローラ側 group state 書込（`groupsettings` 相当）が chip-tool を
   spawn せず native で完結する（M8a で残したハイブリッドの解消）。
2. `MAT_IFACE` 設定時、`mat diag node` の IM 部分（operational チェック +
   thread シグナル）が native で走る。
3. mat が書いた KVS を**実 chip-tool がそのまま読める**（互換性が受け入れ
   条件 — M8c-2 の間 chip-tool フォールバックは生存し、撤退可能を維持）。
4. `MAT_IFACE` 未設定時は完全無変更。出力 JSON スキーマ・exit code 表は不変。

## ユーザー決定（2026-07-17）

- **スコープ = 2 本立て**（groupsettings native 化 + diag node IM native 化、
  0.21.0）。M8c-1 spec の分割定義どおり。
- **実機 E2E は使い捨てグループで検証**（新規 group id + 新規 keyset、既存
  ノード 1–2 台）。本番 living_lights（group 10 / keyset 60 / ノード
  5,7,8,9,10,11,14）は無傷のまま。chip-tool 互換性の実証も使い捨て
  グループで行う。
- **matd 稼働中の KVS 整合性は現行 note 方式踏襲**: native で KVS を書いた
  provision の成功出力に「matd 稼働中なら再起動」note を常時付与（rebind の
  既存 note と同型）。warm chip-tool の古いメモリ像が INI を書き戻すと mat の
  書込が消えるため。シームレス化・自動再起動は不採用（M8c-3 の chip-tool
  完全撤去で問題自体が消滅する）。
- **アプローチ = 案 A: chip-tool 互換 KVS 書込を mat-controller に実装**
  （読み側 `kvs.rs` と対称）。検討した代替: (B) mat 独自 group 台帳 —
  chip-tool フォールバックから group state が見えなくなり段階的撤退の原則に
  反し不採用。(C) groupsettings を M8c-3 へ先送り — M8c-3 は「mat が KVS の
  唯一の読者になってから bootstrap」が前提で、書込所有を先に確立しないと
  M8c-3 が膨らむため不採用。

## 事前調査の確定事項（上流 v1.4.2.0、2026-07-17 実施）

chip-tool `groupsettings` の永続形式を上流ソース
（`DefaultStorageKeyAllocator.h` / `GroupDataProviderImpl.cpp` /
`CommonPersistentData.h` / `CHIPCryptoPAL.cpp` /
`examples/chip-tool/commands/group/Commands.h`）から確定した。
`mat-controller::kvs` の読み側パーサ（実機 v1.4.2.0 store 検証済み）と
タグ完全一致を確認済み。

### ストレージキー（全て hex `%x`、値は INI に base64）

| キー | 内容 |
|---|---|
| `g/gfl` | FabricList — group データを持つ fabric の一覧 |
| `f/<idx>/g` | FabricData — 各 linked list の head + count |
| `f/<idx>/g/<gid>` | GroupData — group 名、endpoint list head |
| `f/<idx>/gk/<id>` | KeyMapData — GroupKeyMap エントリ（`<id>` は内部 id、group id ではない） |
| `f/<idx>/k/<ksid>` | KeySetData — keyset（`<ksid>` = keyset_id） |

### TLV 形式（全て匿名タグの struct）

- **FabricList**: { ctx1: first_entry (u16 fabric_index), ctx2: entry_count (u16) }
- **FabricData**: { ctx1: first_group, ctx2: group_count, ctx3: first_map,
  ctx4: map_count, ctx5: first_keyset, ctx6: keyset_count, ctx7: next }
- **GroupData**: { ctx1: name (string), ctx2: first_endpoint,
  ctx3: endpoint_count, ctx4: next }
- **KeyMapData**: { ctx1: group_id, ctx2: keyset_id, ctx3: next }
- **KeySetData**: { ctx1: policy, ctx2: keys_count, ctx3: array[ struct{
  ctx4: start_time (u64), ctx5: hash (u16, GKH = group session id),
  ctx6: encryption_key (16B) } ], ctx7: next }。**永続されるのは operational
  （導出済み）鍵。epoch 鍵と privacy key は永続されない**（privacy は
  Deserialize 時に都度導出）。

### 書込セマンティクス

- **add-group** = `SetGroupInfo`: GroupData 新規作成（endpoint なし）、
  linked list は **head 挿入**（`group.next = fabric.first_group;
  fabric.first_group = group_id; group_count++`）、fabric を FabricList に
  登録（未登録なら）。既存 group への再実行は内容更新のみ（count 不変）。
- **add-keysets** = `SetKeySet`: 各 epoch 鍵から operational を導出して
  KeySetData を書く。FabricData は head 挿入（first_keyset 更新 +
  keyset_count++）。既存 keyset の再実行は内容更新のみ。
- **bind-keyset** = `SetGroupKeyAt(index=現 map 数)`: KeyMapData の storage
  id は **max_id + 1** 割当（連番でも group_id でもない）。**末尾に連結**
  （prev.next = new id、先頭なら fabric.first_map = id）。map_count++。
  既存 (group_id, keyset_id) と重複する bind は **Duplicate エラー**
  （rebind で unbind が先に要る現行挙動の由来）。
- **unbind-keyset** = 走査で該当 (group_id, keyset_id) を探して
  `RemoveGroupKeyAt`: linked list 繋ぎ替えのみ（prev.next = removed.next /
  先頭なら fabric.first_map = removed.next）、map_count--。**id の再割当・
  詰め直しはしない**（sparse になる — 読み側の `1..=0xff` 全走査と整合）。

### 鍵導出

- **epoch → operational**: HKDF(key=epoch, salt=compressed_fabric_id,
  info=`"GroupKey v1.0"`, 16B) — M8c-1 の `fabric::derive_ipk_operational` と
  同一 KDF。汎用名で流用する。
- **GKH（KeySetData ctx5 hash = group session id、ワイヤに乗る値）**:
  HKDF(key=operational, salt=空, info=`"GroupKeyHash"`, 2B) →
  **big-endian u16**。mat-controller に未実装 → 本マイルストーンで追加。
- privacy key（info=`"PrivacyKey"`）は永続不要のため実装しない。

## 設計

### 1. 書込レイヤ（mat-controller）

- `kvs.rs` に INI 書込プリミティブを追加: read-modify-write、排他は
  `counter.rs` と同じ流儀 — sidecar `<kvs>.lock` へ rustix advisory flock
  （NonBlocking、WouldBlock は「他プロセスが書込中」の明確なエラー）+
  tmp+rename の原子置換。INI の既存キー・セクション・コメントは保全する
  （書き換えるのは対象キーのみ。chip-tool が読み戻せることが至上命題）。
- セマンティック操作（`kvs.rs` 内 or 新モジュール `group_settings.rs`、
  実装時に量で判断）: `set_group_info` / `set_keyset` / `bind_keyset` /
  `unbind_keyset` — 上記「書込セマンティクス」を忠実に再現。**1 回の
  provision で触る 5 レコード（FabricList / FabricData / GroupData /
  KeyMapData / KeySetData）は 1 つの flock 区間内で読み・変更・書きを完結**
  させ、中途半端な状態を他の mat プロセスに見せない。
- `EPOCH_START_TIME`（mat-core::group、デバイス側 epochStartTime0 = 1 と
  一致）を KeySetData の start_time に使う。KeySetData の key entry は
  常に 1 本（現行 chip-tool 運用と同じ。epoch ローテーション非対応）。

### 2. 配線とフォールバック

- **mat-native に薄いラッパー**: `NativeConfig`（store root / fabric_index）
  から KVS パスと compressed_fabric_id（root cert 資材から計算 — 既存の
  読み側と同じ解決）を得て、mat-controller のセマンティック操作を呼ぶ。
  epoch 鍵の解決（明示指定の検証 or ランダム生成 = `resolve_epoch_key`）は
  現行どおり CLI 層。
- **直経路**: `native_direct` の `NativeOp::GroupProvision` が
  `provision_controller_state`（chip-tool spawn）の代わりにラッパーを呼ぶ。
  デバイス側 4 ステップ（M8a で native 済み）は不変。
- **matd**: `server.rs::group_provision` のコントローラ側ステップ
  （現行 4 つの `groupsettings` ws コマンド）を native 有効時のみ同
  ラッパーに置換。native 無効時は現行 ws 経路のまま。
- **フォールバック**: KVS 資材の解決失敗（ファイル無し・root cert 読めず
  等）→ warn + chip-tool の groupsettings へ（コントローラローカル操作で
  ワイヤ未接触、常に安全 — M7 同型）。**flock WouldBlock は hard error**
  （他プロセスが KVS を書込中の合図。flock に参加しない chip-tool へ
  フォールバックすると、まさにその書込と競合する — counter.rs の
  WouldBlock エラーと同じ扱い）。**書込自体の失敗（tmp+rename の I/O
  エラー等）も hard error**（中途書込の可能性がある状態に chip-tool を
  重ねない）。
- **rebind**: native でも unbind → bind の順序を踏襲。unbind は
  best-effort（対象が無くても失敗を無視、検知は bind の Duplicate に委ねる —
  現行と同じ理屈）。
- **matd 整合性 note**: native で KVS を書いた provision の成功出力に
  「matd 稼働中なら再起動」note を常時付与（既存 rebind note と同型・統合。
  chip-tool 経路で書いた場合は現行どおり rebind 時のみ）。

### 3. diag node native 化（直経路のみ — diag は matd 非対象）

`diag::node` は既に `native: Option<&Config>` を受けている（M8b の mDNS
probe 用）。これを IM 部分へ拡張する:

- **operational チェック**: chip-tool の descriptor parts-list read →
  native 汎用 read（M8a 基盤）に置換。**CFID はログパースをやめ、fabric
  資材（root 公開鍵 + fabric id）から直接計算**する — chip-tool ログ形式
  非依存になり、native 経路では `cfid_unavailable` の系が消える。
- **thread シグナル**: neighbor-table / routing-role を native read
  （struct list は `ops::diag_thread` 基盤が対応済み）。operational
  チェックと **1 本の CASE セッションを共有**する（従来は chip-tool を
  属性ごとに spawn していた）。
- 資材構築失敗 → warn + chip-tool フォールバック（M7 同型）。ping6 /
  mDNS probe は無変更（mDNS は M8b で native 済み）。
- 出力 JSON（verdict / checks / unavailable / recommendation）は現行と
  完全同一スキーマ。

### 4. エラー分類・マーカーログ

- 分類は既存 native IM op の `ErrorKind` 写像を流用。exit code 表・JSON
  スキーマ不変。
- マーカーログ（E2E 用、M8b/M8c-1 と同流儀）: 成功経路で
  `group provision controller state written (native kvs)` /
  `diag node executed (native)`（info）、フォールバックで
  `falling back to chip-tool`（warn、既存形式）。

## テスト

- **ユニット（mat-controller）**: TLV serialize の round-trip（書いた blob を
  既存パーサで読み戻す）、linked-list 操作（head 挿入 / max_id+1 割当 /
  sparse 削除の繋ぎ替え / FabricList 登録）、GKH 導出、重複 bind の
  Duplicate 検査、INI 既存キー保全（無関係キーが消えない）、flock 競合
  （WouldBlock）。
- **統合（fake-chip-tool）**: `MAT_IFACE` 未設定 = 完全無変更の回帰、
  bogus iface → warn + chip-tool フォールバックで現行出力（provision /
  diag node の両方）。
- **実機前の互換予備検証**: jarvis の実 KVS のコピーに対して mat の書込を
  実行し、実 chip-tool（`groupsettings show-groups` 等）が読めることを
  E2E 前に確認できれば行う（主検証は下記 E2E）。

## 受け入れ基準（実機 E2E、jarvis）

1. **使い捨てグループ**（新規 group id + 新規 keyset、既存ノード 1–2 台）を
   `MAT_IFACE` 設定で native provision（コントローラ側 chip-tool spawn
   ゼロをマーカーログで実証）→ native groupcast で点滅確認。
2. **chip-tool 互換実証**: 同じグループへ `MAT_IFACE` 未設定の
   `mat group invoke`（= chip-tool 経路）でも点滅する — mat が書いた KVS を
   実 chip-tool が読めた証明。**注意**: 既知の「group 送信の直経路 / matd
   混在禁止」罠（カウンタ衝突で以後不達）を踏むため、この確認の直後に
   matd を再起動する（既知の運用手順）。
3. `--rebind` の native 動作（unbind → bind、Duplicate にならない）確認。
4. `MAT_IFACE` 設定で `diag node --deep` が chip-tool spawn ゼロで完走し
   verdict / スキーマが現行同一。未設定で完全無変更。
5. **living_lights は無傷**（group 10 の送達が検証後も正常）。使い捨て
   グループのデバイス側 state は可能な範囲で後片付け。
6. `task check` 全通過。

## やらないこと（M8c-2）

- remove-group / remove-keyset の native 化（mat のコマンド面が使って
  いない。使い捨てグループの後片付けは chip-tool / デバイス側操作でよい）。
- 初回 fabric bootstrap・native 既定化・chip-tool / avahi 撤去（M8c-3）。
- matd への diag / commission op 追加（恒久対象外）。
- epoch 鍵ローテーション（key entry 複数本）対応。
- matd 稼働中 KVS 書込のシームレス整合（note 方式で運用、M8c-3 で消滅）。

## 実施メモ

ブランチ `m8c2-groupsettings-native`、バージョン 0.21.0。実機 E2E 合格後に
main へ `--no-ff` マージ（従来どおり）。
