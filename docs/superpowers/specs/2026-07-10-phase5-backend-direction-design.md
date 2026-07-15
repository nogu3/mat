# Phase 5 バックエンド方向性: フルスクラッチ Rust Matter コントローラ 設計

2026-07-10 決定。chip-tool バックエンドの後継を選定した議論の結論と根拠の記録。
実装は本 spec の承認後、別セッションで実装計画（writing-plans）から始める。

## 決定

**Phase 5 のバックエンドは、フルスクラッチの Rust Matter コントローラライブラリを
この repo 内に自作する。**

- **第一期（operational 専用）**: CASE initiator・Interaction Model
  （read / invoke）・group セッション（groupcast 送信）を自作する。
  commissioning は従来どおり chip-tool one-shot に残し、chip-tool の KVS から
  fabric 資格情報（root cert・NOC・操作鍵・IPK・group 鍵）を読んで
  **同一 fabric に相乗り**する（再コミッション不要）。
- **第二期（commissioning 自作）**: PASE（SPAKE2+）・BTP/BLE・attestation 検証を
  自作し、chip-tool 依存をゼロにする。
- 既存の rust-matc（tom-code/rust-matc, BSD-2）は**参照実装として読むのみ**。
  コードはコピーしない（クリーンなフルスクラッチ）。
- 開発は connectedhomeip の example device（`chip-all-clusters-app`）を
  x86 ローカルで相手にして高速に回し、実機（jarvis の Nanoleaf 群）は
  各マイルストーンの受け入れ検証に使う。

## 要件（ユーザー確定 2026-07-10）

- 機能は **on/off・色変更・groupcast** ができれば足りる（フルクラスタ対応は不要。
  クラスタ codec は onoff / levelcontrol / colorcontrol / groups /
  groupkeymanagement / accesscontrol の手書きで足りる）。
- メモリは実機（aarch64, RAM ~900MB）で動けば可。
- **常にラグがない**こと（数 ms 感）。warm セッション時のレイテンシは Thread 網の
  往復（10〜50ms）が支配項なので、鍵はバックエンドの言語ではなく
  **warm セッションの維持**。in-process ライブラリ化した matd がこれを担う。

## 動機

1. **chip-tool interactive server は常駐運用が想定外**。CPU 1 コアを常時
   ほぼ 100% 消費する busy-loop（上流 `WebSocketServer.cpp` の
   `lws_service(mContext, -1)`、connectedhomeip#29971、2023-10 から未修正）と、
   180 秒アイドルでの ws 切断（mat #7）。keepalive + idle-timeout 短縮
   （spec 2026-07-10-matd-ws-keepalive-design.md、実装済み）は緩和であって
   解決ではない。
2. **テキスト出力パースの脆さ**（`Data = ...` 経路）。上流バージョン更新で
   静かに壊れる構造そのものを捨てたい。
3. **所有性**: 障害時に全行を自分で説明できる状態が欲しい。個人プロトタイプ
   （matc）への依存も避けたい。
4. 作ること自体の価値（趣味プロジェクトとしての醇度）。工期よりも純度を優先する
   という明示的な選好。

## 候補評価と棄却理由（2026-07-10 時点）

| 候補 | 評価 | 棄却理由 |
|---|---|---|
| **matc fork / vendor**（tom-code/rust-matc） | プロトコルコア手書き ~18k 行は実在（sigma.rs 928 行の CASE、tlv.rs、MRP、mDNS 等）。group 送信のみ欠落 | 品質・継続性を信用しきれない個人プロトタイプ。所有性の要求はフルスクラッチでしか満たせない（参照実装としては活用する） |
| **matter.js 常駐サービス** | groups は 0.15.0 でコントローラ側も basic 実装あり。正しさ・実戦量は最強（HA 本番採用） | Node デーモン + IPC で今の chip-tool ws と同型の複雑さが残る。0.x API 破壊。fabric 資格情報インポートの難度未知。フォールバックとしても不採用（自作が詰んだら再評価） |
| **CHIP SDK 自前デーモン**（C++） | プロトコルリスク最小（group 実装済み・KVS 同一コード） | C++ が恒久的に repo 入り。gn/ninja + 巨大 checkout + aarch64 ビルドの税。Rust matd との FFI も自作になる |
| **chip-tool fork + busy-loop パッチ** | CPU 問題だけは即消える | 延命であって移行ではない。パース脆さ・ws の癖・上流追従が残る |
| **CHIP Python バインディング** | — | HA が見限った道。据え置き不採用 |
| **rs-matter**（公式 Rust） | — | コントローラ機能なし（commissioner PR #456 は未マージ close）。将来ウォッチのみ |

フルスクラッチの正味規模の見積り: operational 専用なら **~6-10k 行**
（TLV codec・メッセージ層 + AES-CCM セッション暗号・MRP 再送/ACK・
CASE initiator（Sigma1/2/3）・IM read/invoke・mDNS/DNS-SD 解決・
group セッション・必要クラスタ codec のみ）。暗号プリミティブ
（P-256 / AES-CCM / SHA-256 / HKDF）は RustCrypto の既製 crate を使い、
書くのはプロトコル状態機械。工期は月単位を許容（つなぎは keepalive 実装済みで
手当て済み。CPU 焼きは「使用中 + idle-timeout」に有限化済み）。

## アーキテクチャ方針

- 新しいコントローラ実装は **workspace 内の専用 crate**（名前は実装計画で決定）に
  閉じる。mat CLI / matd のコマンド層にはプロトコルを置かない —
  CLAUDE.md design rule 1 は「chip-tool に委譲せよ」から
  「プロトコルは専用バックエンド crate のみ」に読み替える（下記 doc 変更）。
- matd は新 crate を **in-process ライブラリ**として使う。子プロセス・ws・
  孤児ポート（9100）という障害クラスが構造ごと消える。warm CASE セッションは
  matd 内で保持する。
- mat の JSON スキーマ・サブコマンド・エラー kind・exit code は**不変**
  （バックエンドは adapter 1 枚の差し替え、が従来からの契約）。
- 移行期は chip-tool 経路と新経路が共存する。**group 送信の送信者は常に一本**
  （groupcast counter 衝突の実機知見）— 切替はコマンド単位のフラグ運用ではなく
  マイルストーン単位で行う。

## マイルストーン

- **M1**: TLV codec + メッセージ層 + セッション暗号（unsecured/secured unicast）。
  相手はローカル `chip-all-clusters-app`。
- **M2**: CASE initiator（Sigma1/2/3）+ IM read / invoke。ローカル example device
  相手に unicast on/off が通る。
- **M3**: chip-tool KVS リーダ（fabric テーブル・操作鍵・IPK の取り出し）。
  jarvis 実機の Nanoleaf に同一 fabric 相乗りで on/off・色変更が通る。

> **2026-07-11 追記（M2 spec 決定の反映）:** 最小 KVS リーダ（chip-tool Linux KVS
> v1.4.2.0 固定、fabric index 1 の RCAC/ICAC/NOC/操作鍵/IPK）は M2 に前倒しした。
> これに伴い M3 は「KVS リーダの堅牢化（バージョン互換方針含む）+ jarvis 実機
> 相乗りで on/off・色変更」に再定義する。
> 詳細: docs/superpowers/specs/2026-07-10-phase5-m2-case-im-design.md
- **M4**: matd の adapter を新 crate に差し替え（in-process、warm 維持）。
  実機で cold/warm レイテンシ計測、既存の matd 統合テスト全通過。
- **M5**: group セッション（epoch 鍵からの operational group key 導出・
  AES-CCM groupcast 暗号化・ff35:: 送信・counter 永続化）。実機 living_lights
  （group 10）で 7/7 配達 E2E。
- **M6（第二期）**: commissioning（PASE・BTP/BLE・attestation）→ chip-tool
  完全廃止。

各マイルストーンが独立の受け入れ基準を持つ。M3 まで失敗しても既存経路は無傷
（撤退可能）。

## ドキュメント変更（実装開始時に行う）

1. **ARCHITECTURE.md**: Phase 5 節を「optional・候補は matter.js / Rust
   コントローラ」から「決定済み: フルスクラッチ Rust ライブラリ（第一期
   operational、第二期 commissioning）、本 spec 参照」に書き換え。
   「Things we never do」の TLV/CASE 禁止項に「専用バックエンド crate を除く」の
   但し書き。
2. **CLAUDE.md**: design rule 1 を「プロトコル実装は専用バックエンド crate のみに
   置く（mat CLI / matd のコマンド層には置かない）。Phase 5 着地までは chip-tool
   委譲が現行経路」に修正。Roadmap discipline の「Phase 5 optional・未着手」を
   更新。

## 未決事項（実装計画で詰める）

- 新 crate の名前と公開 API の形。
- groupcast counter 永続化の排他方式（mat 直経路と matd の共存期に送信者一本化を
  どう機械的に保証するか。counter ストアのファイルロック vs group 送信の matd
  一本化ルール）。
- mat 直経路（one-shot）を新 crate に載せ替える時期（M4 と同時か、M5 後か）。
- chip-tool KVS のフォーマット互換をどのバージョン範囲で保証するか。
- subscribe の要否（要件外だが matd の将来機能として温存するか）。

## 非ゴール

- Matter デバイス側 / ブリッジになること（別プロジェクト、従来どおり）。
- フルクラスタ対応・certification・マルチ fabric 管理 UI。
- matter.js / CHIP SDK へのフォールバック実装（詰んだら本 spec を改訂して
  再評価する。先回りの保険実装はしない）。
