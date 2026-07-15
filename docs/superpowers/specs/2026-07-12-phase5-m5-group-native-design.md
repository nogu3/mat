# Phase 5 M5: group セッション native 化（groupcast 送信の in-process 化）設計

2026-07-12 起草。親 spec: `2026-07-10-phase5-backend-direction-design.md`（M5 =
「group セッション（epoch 鍵からの operational group key 導出・AES-CCM groupcast
暗号化・ff35:: 送信・counter 永続化）。実機 living_lights（group 10）で 7/7 配達
E2E」）。前提: M1〜M4 実装・jarvis 実機 E2E 合格済み（M4 = matd の unicast
ホットパス native 化）。ブランチは長期ブランチ `matter-controller`（main マージ
禁止）。

## ゴール

**matd の group 送信 op（`GroupInvoke` / `GroupColor` / `GroupColorTemp`）を
`mat-controller` の native groupcast 経路で処理する。** chip-tool の group 送信
腕は matd から外れ、group 送信者は matd native に一本化される（groupcast counter
混在禁止の実機知見への構造的な答え）。`GroupProvision`（groupsettings 状態書き
込み + デバイス側 KeySetWrite / GroupKeyMap / AddGroup / ACL）は chip-tool の
まま（M5 スコープ外）。

## 前提事実（実機 KVS 確認済み、2026-07-12）

chip-tool Linux ini KVS（v1.4.2.0、jarvis main ini）の group 関連キー:

- `f/<idx>/gk/<n>`: GroupKeyMap エントリ（group_id → keyset_id の対応）。
  実機例: `f/2/gk/1` 〜 `f/2/gk/4`。
- `f/<idx>/k/<keyset_id hex>`: keyset blob。実機例: `f/2/k/3c`（keyset 60 =
  living_lights）。TLV 構造は IPK（`k/0`）と同一で、**epoch 鍵ではなく導出済み
  operational credentials を持つ**: struct{ ctx1: policy, ctx2: keys_count,
  ctx3: array of struct{ ctx4: start_time, ctx5: **hash（= Group Session ID,
  u16）**, ctx6: **operational key（16B）** }, ctx7: next }。
- `g/gdc`: chip-tool の Global Group Data Counter 永続値（u32）。

**帰結**: 親 spec の「epoch 鍵からの operational group key 導出」は不要。KVS の
導出済み credentials を読めば送信に必要な鍵材料（暗号鍵 + session id）が揃い、
chip-tool と鍵が一致することが構造的に保証される。KDF 実装は持たない（YAGNI）。

## 決定

### 決定 1: `GroupSender` を mat-controller に新設（`SecureSession` とは独立の型）

groupcast は unicast CASE session と性質が違う — 応答なし・MRP なし・RxWindow
なし・counter はランダム初期化でなく**永続**・session id は CASE 交渉でなく鍵の
GKH。`SecureSession` に条件分岐で相乗りさせず、送信専用の独立型にする
（unicast ホットパスへの回帰リスクをゼロにする）。

新モジュール `mat-controller::group`。`GroupSender` の保持物:

- `transport: Arc<UdpTransport>`（M4 の NativeBackend と同じ socket を共有）
- `fabric_id: u64` / `source_node_id: u64`（自己発行 NOC と同一 = chip-tool の
  `LocalNodeId` と同一）
- `creds: GroupCredentials { session_id: u16, encryption_key: [u8; 16] }`
- `counter: PersistedGroupCounter`（決定 3）

送信メッセージの組み立て:

- Message Flags: S Flag = 1（source node id 付き）、DSIZ = 2（16-bit group id
  宛先）。Security Flags: Session Type = 1（group session）。Session ID = GKH。
- nonce = security flags ‖ counter ‖ source node id（unicast と同式。既存
  `crypto.rs` の AES-CCM をそのまま再利用）。
- ペイロードは IM InvokeRequest（SuppressResponse = true, timed = false）。
  Exchange Flags は Initiator = 1、R Flag = 0（MRP なし）。送信一発・応答待ち
  なし・再送なし（groupcast は unreliable が仕様。chip-tool も同じで 7/7 実績）。
- privacy 拡張は使わない（P Flag = 0。chip-tool 既定と同じ）。

### 決定 2: KVS リーダ拡張 — group credentials と g/gdc の読み出し

`kvs.rs` に追加:

- `read_group_credentials(path, fabric_index, group_id) -> GroupCredentials`:
  `f/<idx>/gk/<n>` を n=0 から走査して group_id に対応する keyset_id を解決 →
  `f/<idx>/k/<keyset_id hex>` の keyset blob から最初の epoch エントリの
  hash（session id）と operational key を取る。既存 `parse_keyset` と同系の
  容認的パーサ（未知タグ/コンテナは skip）。group_id が GroupKeyMap に無ければ
  専用エラー（matd 層で `node_not_commissioned` 相当ではなく「group 未 provision」
  が分かる detail にする）。
- `read_group_data_counter(path) -> Option<u32>`: `g/gdc` を読む（無ければ None。
  決定 3 の jump-ahead 初期化用）。

GroupKeyMap blob（`gk/<n>`）の正確な TLV 構造は実装時に実物 + SDK v1.4.2.0 で
確定し、フィクスチャ単体テストに焼く（M3 の keyset パーサと同じ進め方）。

### 決定 3: counter は自前ファイル + 起動時 jump-ahead（ユーザー決定 2026-07-12）

groupcast の受信側は送信元 node id ごとに counter 窓を持つ。native は chip-tool
と同一 node id で送るため counter 空間を共有する — 低い counter で送ると全滅する
（実機知見 `groupcast-e2e-findings` の counter 衝突と同根）。

- 永続先: `<store>/native_group_counter`（mat の store 配下。chip-tool の ini は
  **書かない** — chip-tool 相乗り破壊リスクを持たない）。
- 方式: SDK `PersistedCounter` と同じ persist-ahead。ファイルには「ここまで使って
  よい上限」を書き、メモリ内 counter が上限に達したら +4096 して書き直す。
  クラッシュ/再起動しても未使用領域から再開でき、重複送信 counter が出ない。
- 初期化: `max(自前ファイルの永続値, chip-tool g/gdc の値) + 4096` から開始。
  chip-tool がこれまでに送った履歴より必ず上から始まる。
- 排他: group 送信は **matd native 一本**（matd の chip-tool fallback から group
  送信腕を削除するので、matd 経由では構造的に混在しない）。mat 直経路の group
  送信（matd 停止時のみ動く従来形）は chip-tool 経路のままで、混在禁止の運用
  note を README/知見に継続する。counter ファイルに flock は掛けない（書くのは
  matd プロセス 1 本だけ。matd 自体に既存の二重起動ガードがある）。

**棄却案**: chip-tool の `g/gdc` を flock で共有 — chip-tool 自身が flock を
知らず、interactive セッション中はメモリ内 counter を使うため排他が成立しない。
ini の書き換え衝突（KVS 破壊）リスクも増える。

**2026-07-13 最終レビュー追記（訂正）**: 上の「matd 経由では構造的に混在
しない」は native-eligible な 3 形（onoff 引数なし on/off/toggle の
`group invoke`、`group color`、`group color-temp`）に限った話で、それ以外の
group 送信（他 cluster の `group invoke`、引数付き onoff の `group invoke`
等）は matd 経由でも従来どおり chip-tool を通る（`server.rs::native_group_params`
が `None` を返す形）。この chip-tool 経路は matd native の counter とは別に
chip-tool 自身の counter を使うため、native がすでに送信を重ねた後だと
chip-tool 側 counter は同じ送信元 node id の native より下回り得る（counter
窓は送信元 node id ごと・全 group 共通）。同じ group を native 経由でも受けて
いるデバイスは、この chip-tool 経由の送信を古い/重複として黙って捨てる
可能性がある（intra-matd counter mixing）。対応: ルーティングは変更せず、
`server.rs::run_op` に `tracing::warn!` を追加して観測可能にした
（README「matd's native groupcast」節にも運用注記を追記）。native-eligible
形を拡張して全 group 送信を native 化する／該当送信を拒否する、といった
根本対応は製品判断として見送り（このレビューのスコープ外）。

### 決定 4: 宛先アドレスと multicast 送信

- 宛先: Matter 仕様の site-local transient multicast、port 5540。16 バイトの
  バイト配置は `FF 35 00 40 FD ‖ fabric_id(8B, BE) ‖ 00 ‖ group_id(2B, BE)`
  （正確な配置は SDK `MakeIPv6TransientMulticast` に合わせ、単体テストで
  既知アドレス（実 fabric id + group 10 の期待値）と一致させる）。
- 送出 socket は M4 の `Arc<UdpTransport>` を共有。送出時に
  **`IPV6_MULTICAST_HOPS` を明示設定**（OS 既定 1 のままだと border router を
  越えない）と `IPV6_MULTICAST_IF`（`MAT_MATD_IFACE` の ifindex）を設定する。
- 単一 iface（`MAT_MATD_IFACE`、jarvis は eth0）への一発送出。Thread 側へは
  otbr が backbone link 上の site-scope multicast を MLR で mesh へ転送する
  現行トポロジ前提（chip-tool の 7/7 実績と同経路）。

### 決定 5: matd 配線 — group 送信 3 op を native へ

`server.rs::run_op` の振り分けに group 送信 op を追加:

- native 有効時（M4 と同条件: `MAT_MATD_IFACE` 設定 + NativeBackend 構築成功）:
  `GroupInvoke`（onoff on/off/toggle）・`GroupColor`（move-to-hue-and-saturation）・
  `GroupColorTemp`（move-to-color-temperature）→ `NativeBackend`。IM エンコーダは
  M4 の unicast 用（`im.rs`）をそのまま使う（コマンド field は unicast/group で
  同一。group メッセージに endpoint は無く、デバイス側の group table
  （AddGroup 済み endpoint）が配送先を解決する）。
- `GroupProvision` は従来どおり chip-tool（groupsettings + unicast 書き込み群）。
- native 無効時は全 group op が chip-tool へ（M4 と同じ安全フォールバック）。
  さらに `GroupSender` の構築失敗（KVS 読めない・group が GroupKeyMap に無い・
  `g/gdc` 欠落で counter 初期化不能）も当該 op を chip-tool フォールバックへ
  回す（native の異常で機能停止しない。原因は warn ログに出す）。
- 応答 JSON スキーマは現行 chip-tool 経路と同一（group 送信は fire-and-forget
  なので現行も送信受理の応答を返している — 形を変えない）。native 送信路の
  エラー分類は送出 socket エラー → `unreachable` のみ。

## スコープ

| モジュール | 変更 |
|---|---|
| `mat-controller::kvs` | 決定 2（GroupKeyMap / keyset(hash+key) / g/gdc リーダ） |
| `mat-controller::group`（新規） | 決定 1・3・4（`GroupSender`、message 組み立て、AES-CCM、`PersistedGroupCounter`、multicast 宛先） |
| `mat-controller::transport` | multicast 送出オプション（hops / iface）の設定口 |
| `matd::native` | `GroupSender` の保持（lazy 構築: 初 group op 時に KVS 読み + counter 初期化） |
| `matd::server` | 決定 5（group 送信 3 op の native 振り分け、エラー写像） |
| `tests/` | パーサ/アドレス/counter 単体、matd 統合（native 無効時の非回帰含む） |
| `scripts/e2e-m5.sh` + Taskfile | クロスビルド → 転送 → 実機 groupcast E2E 一発化 |

## 受け入れ基準（M5）

### CI（実機なし、`task check` 全通過を維持）

1. GroupKeyMap / keyset パーサ: フィクスチャ blob から group_id → (session_id,
   key) が取れる。group 未登録・壊れ blob はエラー分類つきで返る。
2. multicast アドレス構成: fabric id + group id → 期待 ff35:: アドレスの一致。
3. group message 組み立て: 既知入力（鍵・counter・source node id・コマンド）から
   flags / session id / nonce 構成が期待バイト列と一致（暗号化前ヘッダ + CCM の
   決定論テスト）。
4. `PersistedGroupCounter`: persist-ahead（上限到達で +4096 再永続化）、再起動
   相当の再読込で未使用領域から再開、`g/gdc` と自前値の max + 4096 初期化。
5. matd 統合: native 有効時に group 送信 3 op が native へ、`GroupProvision` は
   chip-tool へ。native 無効時は全 group op が chip-tool へ（非回帰）。

### jarvis 実機（`task e2e:m5`）

6. living_lights（group 10）へ native groupcast `off` → 7/7 消灯 → `on` → 7/7
   点灯（各ノード unicast read で状態検証 = 7/7 配達の判定）。
7. `group color-temp` が native groupcast で通り、全ノードの color-temperature
   read が目標付近。元へ復元。
8. matd 再起動 → 再度 groupcast → 7/7 配達（jump-ahead / persist-ahead の実機
   実証。counter が単調増加していることをログで確認）。
9. `GroupProvision`（`mat group provision --rebind` 相当）が chip-tool 経路で
   従来どおり動く（非回帰）。

## 非ゴール

- `GroupProvision` の native 化（KeySetWrite / GroupKeyMap write / AddGroup /
  ACL は unicast IM write が必要 — 残 unicast op の native 化と同じ後続範囲）。
- mat 直経路（one-shot CLI）の group native 化（直経路は chip-tool のまま。
  混在禁止 note 継続）。
- groupcast の受信（mat は device ではない）。privacy 拡張（P Flag）。
- MCSP（group control counter `g/gcc` 系）— data counter のみ扱う。
- 複数 iface への同報・iface 自動検出（`MAT_MATD_IFACE` 単一。リスク節参照）。

## リスク

- **multicast 経路**: eth0 への一発送出で otbr が Thread mesh へ転送する前提が
  外れると届かない。chip-tool の 7/7 実績が同一ホスト・同一トポロジなので低
  リスクだが、E2E で不達なら送出 iface の複数化（wpan0 直送を含む）を追補する
  設計余地を決定 4 に残している。切り分けは tcpdump（eth0/wpan0 の ff35::
  観測）。
- **counter 初期化漏れ**: 自前ファイルも g/gdc も読めない初回起動で低い値から
  始めると、chip-tool の送信歴がある受信側に全部落とされる。初期化式が
  `max(...) + 4096` で g/gdc を必ず参照すること、g/gdc が読めない KVS は
  GroupSender 構築失敗（= 当該 op を chip-tool フォールバック）にすることで防ぐ。
- **E2E 中の送信者混在**: 実機検証中に本番 matd（chip-tool 経路の group 送信）と
  native matd を並走させると既知の counter 衝突を踏む。E2E 手順で本番 matd の
  group 送信を止める（unicast は混在可）。
- **GroupKeyMap blob の構造想定違い**: `gk/<n>` のパーサは実物優先で書く
  （決定 2）。想定違いはフィクスチャ焼き直しで吸収し、パース不能は chip-tool
  フォールバックに落ちるので機能停止はしない。
- **デバイス側の per-source counter 窓の永続性**: 受信側（デバイス）が再起動で
  窓を忘れる/覚える挙動に依存しない設計（常に単調増加で送るので、どちらでも
  受理される）。
