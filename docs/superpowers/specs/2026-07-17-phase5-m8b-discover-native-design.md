# Phase 5 M8b: discover native 化（mDNS browse + probe reachability）設計

2026-07-17 起草。親 spec: `2026-07-10-phase5-backend-direction-design.md`、
M8 全体分割の記録: `2026-07-16-phase5-m8a-generic-im-native-design.md` 冒頭。
前提: M1〜M8a 実装・実機 E2E 合格・main マージ済み（本番 jarvis は M7 以降
native 有効運用中）。本 spec は **M8（chip-tool 完全廃止）の
第二弾**で、`mat discover` の commissionable 探索と probe 到達性
（`discover --probe` / `diag node --deep` 共有の `probe::mdns`）を native 化
する。バージョンは **0.19.0**。

## ゴール

1. `MAT_IFACE` 設定時、`mat discover`（`--probe` 含む）と
   `mat diag node --deep` の mDNS プローブが **chip-tool / avahi-browse を
   一切 spawn せず** native で完走する。
2. 出力 JSON スキーマは**完全維持**（chip-tool / avahi 経路と構造一致）。
3. `MAT_IFACE` 未設定時の挙動は**完全無変更**（従来どおり chip-tool +
   avahi-browse。M8c まで撤退可能）。

## ユーザー決定（2026-07-17）

- **probe は discover / diag の両方を native 化**: `probe::mdns()` は
  `discover --probe` と `diag node --deep` で共有されているため、probe 自体に
  native 分岐を入れて両方を同時に avahi 非依存にする（M8c での avahi 撤去を
  一段で済ませる）。
- **dead API 掃除を M8b の先頭タスクに置く**: M8a で不要化した matd
  `ensure_group_acl` 等の未使用コードを削除する（挙動変更なし）。M8a 完了時の
  推奨事項の実行。
- **実装方式は案A（`dnssd.rs` の browse 拡張）**: 既存 one-shot legacy
  unicast 方式を PTR 列挙に広げる。依存追加ゼロ。検討した代替: (B) 既製
  mDNS crate（mdns-sd 等）導入 — 依存追加 + iface 制御（multicast egress の
  罠）が crate 任せになるため不採用。(C) commissionable のみ native 化し
  probe は avahi 温存 — avahi 撤去が二段になるため不採用。

## 現状（M8a 時点）

- commissionable 探索 = `chip-tool discover commissionables` の stdout を
  `mat_core::parse::parse_commissionables` でパース →
  `DiscoveredDevice { hostname, addresses, port, discriminator, vendor_id,
  product_id }`。
- commissioned 一覧 = 台帳（store）読みのみ（外部依存なし、M8b 対象外）。
- probe = `mat::probe::mdns()` が `avahi-browse -rt _matter._tcp` を spawn し
  `mat_core::diag::parse_avahi_matter` → `MatterInstance { compressed_fabric,
  node_id, addresses }`。`discover --probe` は `mat_core::reachability::
  resolve`（node_id 照合）、`diag node --deep` は self-fabric CFID 照合に使う。
- `mat-controller::dnssd` は**特定 instance の解決**のみ
  （`resolve_operational` / `resolve_commissionable`）。browse は無い。

## 設計

### 1. 経路選択・フォールバック（M7/M8a と同型の opt-in）

- `MAT_IFACE` 設定時のみ native browse。未設定は従来経路（完全無変更）。
- native browse の **IO エラー**（socket bind 失敗等）→ `tracing::warn` +
  従来経路フォールバック（read-only op なので二重実行の害なし）。
- **結果 0 件はエラーではない**（周囲に commissionable が無いのは正常）。
  フォールバックしない — さもないと平常時に毎回二重スキャンになる。
- discover は従来どおり **matd プロトコル対象外**（one-shot 直経路のみ）。
- CASE も credential も不要な op のため、エンジン（fabric/NOC/KVS）構築は
  一切走らない。必要なのは UDP socket と ifindex（`dnssd::iface_index`）
  だけ。store 無しでも動く現行セマンティクス（`open_or_init`）は維持。

### 2. `dnssd.rs` browse 拡張（`mat-controller`）

方式は既存 `resolve_*` と同じ **one-shot legacy unicast**（source port ≠
5353 → 応答は unicast で直接返る。RFC 6762 §6.7）。listener 常駐・キャッシュ
なし（設計ルール4）。

共通の畳み込みループ `browse(scope_id, service, window)`:

1. `<service>.local` への PTR クエリを送信（`QUERY_RESEND_INTERVAL` = 1 秒で
   再送）。
2. 応答の PTR から instance 名を収集（ASCII 大文字小文字無視で dedup）。
   additional に同梱された SRV/TXT/AAAA はその場で畳み込む（既存
   `commissionable_from_response` と同じ考え方）。
3. 同梱が無かった instance には SRV/TXT/AAAA のフォローアップクエリを送る。
4. **早期 return なし** — 全員から集めるため `window` 満了まで収集して
   打ち切る。window は定数 **3 秒**（新 CLI フラグなし）。
5. instance 数上限（32）と AAAA 畳み込み上限は既存の `record_capacity` /
   `push_aaaa` 系ガードを流用（偽装 flood 対策は既存と同水準）。
6. 受信バッファは 1500 → **9000 バイト**（browse 応答は大きくなり得る。
   mDNS の実質上限）。
7. 他人の壊れたデータグラムで browse を中断しない（既存 `parse_message` の
   Err skip と同じ）。

薄いラッパー 2 つ:

- `browse_commissionable(scope_id, window) -> Vec<CommissionableInstance>`
  — `_matterc._udp` を列挙。TXT から `D`（long discriminator）と `VP`
  （`vid+pid`）、SRV から port と hostname（target の末尾 `.local.` を
  落とした形）、AAAA からアドレス群。`resolve_commissionable` と違い特定
  デバイスを探すのではなく全列挙が目的のため、**TXT `D` の一致検証は
  しない**。
- `browse_operational(scope_id, window) -> Vec<OperationalInstance>`
  — `_matter._tcp` を列挙。instance 名 `<CFID 16hex>-<NodeId 16hex>` を
  パースして compressed_fabric + node_id を得る。形式に合わない instance は
  skip（他プロトコルの流れ弾）。**SRV/AAAA が揃わなくても PTR が見えた
  instance は返す**（announce のみ = addresses 空。avahi の `+` 行のみと
  同じ扱いで、既存 `reachability::resolve` の「announce のみ →
  reachable=true / live_address=None」セマンティクスを保存）。

畳み込みロジックは純関数に切り出し、合成 DNS メッセージでユニットテスト
（socket 不要 — 既存テストと同じ流儀）。

### 3. mat 側の写し

- `probe.rs`: `MAT_IFACE` 設定時は `browse_operational` →
  `mat_core::diag::MatterInstance` へ変換。**既存構造体をそのまま使う**ため
  `reachability::resolve`（discover --probe）も self-fabric CFID 照合
  （diag node --deep）も無改変で動く。未設定時は avahi（無変更）。
- `discover.rs`: `MAT_IFACE` 設定時は `browse_commissionable` →
  `mat_core::parse::DiscoveredDevice` へ変換（既存 Serialize でスキーマ
  完全一致）。未設定時は chip-tool（無変更）。
- tokio runtime は既存 native 直経路（`native_direct::try_run`）と同じく
  current-thread を都度構築。
- `VP` の `vid+pid` 分解・hostname 整形など TXT/DNS 由来のパースは
  `dnssd.rs`（mat-controller）に置いてユニットテスト（mat-core は
  mat-controller に依存できないため、プロトコル由来のパースは backend crate
  側に置く — 設計ルール1とも整合）。mat 側は構造体の写しのみ。

### 4. エラー処理

- native browse IO エラー → warn + 従来経路フォールバック（上記）。
- 従来経路のエラー分類は無変更（avahi 不在 = `child_not_found` →
  `reachable: null`、chip-tool 不在 = exit 12 等）。

### 5. テスト

- `dnssd.rs` ユニット: 合成メッセージ fold（複数 instance の PTR 列挙、
  additional 同梱/非同梱、dedup、announce のみ、instance 名パース不能 skip、
  flood ガード）。
- 変換ユニット: browse 結果 → `DiscoveredDevice` / `MatterInstance` 写像
  （`VP` 分解・hostname 整形は `dnssd.rs` 側、構造体の写しは mat 側で
  それぞれユニットテスト）。
- 統合テスト（fake-chip-tool）: `MAT_IFACE` 未設定の従来経路が完全無変更で
  あることの回帰。
- native browse のループ本体は socket 実物が要るため実機 E2E で実証
  （既存 `resolve_*` と同じ流儀）。

## 受け入れ基準（実機 E2E、jarvis）

1. `MAT_IFACE=eth0 mat discover --probe` が native のみで完走
   （chip-tool / avahi-browse の spawn ゼロをログで実証）し、commissioned
   全ノードが `reachable: true` + live アドレス。
2. commissionable 検出: fabric 無しの玄関ライト（`_matterc._udp` 広告中の
   はず）が discriminator / vendor_id / product_id 付きで出る。chip-tool
   経路の出力と構造一致。
3. `MAT_IFACE=eth0 mat diag node --deep` の mDNS チェック
   （`advertised_self_fabric` / `advertised_any_fabric`）が avahi 経路と
   同じ判定になる。
4. フォールバック健全性: `MAT_IFACE` 未設定で 1〜3 相当が従来経路で全通過
   （回帰ゼロ）。
5. `task check` 全通過。

## 手順・ブランチ

- main から新規作業ブランチ（`matter-controller` は M7 で main に合流済み）。
- 先頭タスク = dead API 掃除（matd `ensure_group_acl` 等）。
- 最後に ARCHITECTURE.md / README の M8b 記述更新、バージョン 0.19.0。
- 実行方式は Subagent-Driven（ユーザー標準方針）。

## やらないこと

- 出力 JSON スキーマの変更・新 CLI フラグ。
- subscription / cache / 常駐 listener（設計ルール4）。
- `diag node` の IM 部分の native 化（M8c で再訪 — 今回は probe のみ）。
- commissioned 台帳読みの変更（外部依存なし、対象外）。
- avahi-browse / chip-tool 経路の削除（M8c の完全撤去で実施）。
