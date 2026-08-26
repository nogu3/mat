# mat-device / matv — M3: Aggregator + bridged endpoints 設計

> 正本（経緯・全体像）: jarvis-brain vault `docs/superpowers/specs/2026-08-15-matv-alexa-design.md`。
> 本書は M3 の mat repo 側技術 spec。入力は `2026-08-16-mat-device-m2-design.md` の
> 「M3（Aggregator）冒頭で拾うべき事項」と `2026-08-22-apple-home-interview-design.md` の
> 「M3 送り」。

## ゴール

matv を設定ファイル駆動の **Aggregator + bridged endpoints**（マトリョーシカ構造）にする。
mando 接続は M4 — M3 の仮想デバイスは in-memory 状態のダミーで、bridge のプロトコル機構
（トポロジ・採番・BDBI・複数 endpoint の IM/Subscribe）を完成させるのが目的。

要件握り（2026-08-26 決定）:

- **デバイス種別は OnOff のみ実装**。設定 schema（`kind` enum）と ClusterHandler 組み立ては
  種別拡張可能に設計し、Thermostat / LevelControl は M4 以降で必要になった時に追加（YAGNI）
- **endpoint 採番は自動台帳を store に永続化**（設定ファイルに明示 ID は書かない）
- **受け入れゲートは chip-tool + `mat describe` のみ**。Apple Home 実機検証は M4 にまとめる

## Endpoint トポロジ

- **EP0** = root。現行クラスタ群のまま
- **EP1** = **Aggregator**（device type 0x000E）。Descriptor のみ持ち、その PartsList が
  bridged endpoints を列挙する。Actions クラスタは実装しない
- **EP2〜** = bridged endpoints。各 EP:
  - Descriptor: DeviceTypeList = [On/Off Light (0x0100), **Bridged Node (0x0013)**]
  - **Bridged Device Basic Information (0x0039)** — 新規クラスタ。NodeLabel（config の
    `name`）/ Reachable = true 固定（mando 不達判定は M4）/ UniqueID（台帳キーから導出）
    + 必須 globals
  - OnOff + Identify + Groups（EP1 用に実装済みのハンドラを流用）
- EP0 の PartsList は全 endpoint（既存の registry 導出のまま）、EP1 の PartsList は
  bridged EP 群のみ

既存の「EP1 = OnOff Light ハードコード」は廃止し、**matv は純 bridge 化**する（単体デバイス
モードは残さない）。既存 e2e ゲート（`e2e:device:m1` / `e2e:device:m2-chip`）は仮想デバイス
1 台の bridged 構成に書き換えて維持する。

## 設定ファイル

```toml
# 既存フィールド（passcode/discriminator/vendor_id/product_id/port/store/iface/attestation）
# はそのまま

[[device]]
id   = "living-light"   # 台帳キー。安定識別子・改名しない
kind = "onoff-light"    # enum。M3 はこれのみ。将来 "thermostat" 等を追加
name = "リビング照明"    # BDBI NodeLabel（コントローラに表示される名前）
```

- `kind` → ClusterHandler 一式を組むファクトリを 1 枚かませ、種別追加が「enum 1 値 +
  ファクトリ 1 分岐」で済む形にする
- `name` 変更は同一 endpoint のまま NodeLabel だけ変わる（アクセサリ対応は壊れない）
- `id` 重複・空の `[[device]]` 列は起動時バリデーションで拒否

## 採番台帳（store/endpoints.json）

```json
{ "next": 5, "map": { "living-light": 2, "bedroom-light": 3 } }
```

- bridged EP は 2 起点で**単調増加・再利用禁止**（Matter bridge 仕様の endpoint 安定要件）
- **削除されたデバイスのエントリも map に残す**（tombstone）。同じ `id` を再追加したら旧
  endpoint を復元し、コントローラ側のアクセサリ対応が生き返る
- 設定反映は**再起動のみ**。ホットリロード（SIGHUP / ファイル監視）は scope 外（YAGNI）

## 申し送り（deferred）の取り込み

**M3 で払う**:

1. **OC 属性の fabric scoping（IsFabricFiltered）+ SupportedFabrics=5 の容量 enforcement**
   — NOCs/Fabrics/TrustedRootCertificates が全 fabric 分を返す cross-fabric 漏えいの解消。
   Apple が fabric 2 本張る実測（apple-home-interview spec の観測メモ）どおりマルチ fabric は
   既に常態
2. **ClusterRevision の実値化** — `ClusterHandler::revision()` を追加し全クラスタ 1 固定を
   解消（実値: Descriptor 2, BasicInfo 3, OnOff 6 ほか各クラスタの現行仕様値）
3. **DataVersion のブート時乱数初期化**（spec §7.10.3）
4. **ACL / root NodeLabel の永続化** — 現状 in-memory で再起動消失。Apple 実機ゲート（M4）の
   前提条件を先に払い、M4 を mando glue に集中させる
5. 小粒の Apple deferred — NodeLabel write の値変化 dedup / PASE（fabric 0）からの ACL write
   拒否 / `load_or_create_unique_id` の getrandom expect → エラー伝播 / Location の write 対応。
   触るファイルで自然に拾う

**送る**: dirty レポートのチャンク化（発火条件が出たら）/ 逐次ハンドシェイクの head-of-line
blocking（顕在化時）/ GroupKeyManagement のコマンド群 / Timed write・chunked write /
FabricFiltered=false の read / ChipTest モードの VID/PID 検証ガード / dirty 期限
floor=ceiling の keep-alive 同時刻問題 / ACL enforcement（認可判定）

## 受け入れ条件

- `matv --config`（仮想デバイス 3 台程度の config）に対し chip-tool commission が成功する
- chip-tool から: wildcard read で Aggregator / BDBI / PartsList のトポロジが正しく読める /
  各 bridged EP の OnOff が個別にトグルできる / Subscribe で各 EP の状態変化が正しい
  endpoint 付きで届く
- **`mat describe` で parts-list（マトリョーシカ構造）が正しく見える**（vault spec の M3 条件）
- 台帳シナリオ: 再起動で endpoint 不変 / config に 1 台追加 → 新 EP 採番・既存不変 /
  削除 → 再追加 → 旧 EP 復元
- `task check` が通り、既存回帰（`pase_self_handshake` / `case_self_handshake` /
  `e2e:device:m1` / `e2e:device:m2-chip` の bridged 版）が生きている

## テスト戦略

- 各クラスタ・台帳・config バリデーションは TDD（unit）
- トポロジ検証は mat-controller の wildcard read / subscribe 発行を使った自己閉ループの
  統合テスト
- chip-tool ゲートは従来どおり統合テストの外側の実機ゲート
- M2 のプロセス教訓を継続: ラッパーコマンドの成否は exit code で判定 / 相互運用フィックスは
  ワイヤバイトを直接 assert（lenient decoder 同士の roundtrip に頼らない）

## スコープ外（M3）

- mando 転送・Reachable の実判定・Thermostat / LevelControl 等の追加種別（M4）
- Apple Home / Echo 実機検証（M4。Echo は Amazon 側凍結が解けた場合のみ）
- ホットリロード / 動的 endpoint 追加（PartsList の live 更新）
- 「送る」に列挙した deferred 群
