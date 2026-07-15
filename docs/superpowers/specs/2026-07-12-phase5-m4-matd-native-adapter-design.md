# Phase 5 M4: matd の native adapter 差し替え（warm CASE in-process）設計

2026-07-12 起草。親 spec: `2026-07-10-phase5-backend-direction-design.md`（M1〜M6 全体像、
M4 = 「matd の adapter を新 crate に差し替え、in-process・warm 維持」）。
前提 capability: M1〜M3（TLV/message/crypto/MRP・CASE initiator・IM read/invoke・
自己発行 NOC・chip-tool KVS 相乗り・自前 mDNS 解決）は `matter-controller` ブランチに
実装・jarvis 実機 E2E 合格済み。ブランチは長期ブランチ `matter-controller`
（main マージ禁止、ユーザー決定 2026-07-10）。

## ゴール

**matd の常駐プロセス内で `mat-controller` の warm CASE セッションを保持し、ホットパス
（on/off・色・色温度・onoff read）を native 経路で処理する。** 現行の chip-tool
interactive server 経路は、native が未対応の op（write・任意 cluster の read/invoke・
describe・group 系）へのフォールバックとして残す。

**将来像（設計の前提）**: 最終的に chip-tool を完全に外す。したがって native を
「一時的な追加経路」ではなく **将来の唯一の経路** として設計し、chip-tool
フォールバックは同一の内部インターフェース背後に隔離する。後続マイルストーンで
残る op を native へ移すたびにフォールバック腕を削るだけで済み、構造の作り直しを
伴わない形にする。

## 決定

### 決定 1: warm セッション保持のため `SecureSession` から借用ライフタイムを除去（`Arc<UdpTransport>` 化）

現状 `SecureSession<'t>` は `transport: &'t UdpTransport` を借用し、`case::establish<'t>`
も `&'t UdpTransport` を取る。matd が `HashMap<node_id, SecureSession>` を長期保持すると:

- **自己参照問題**: transport とそれを借用する session を同一構造体が所有できない。
- **`'static` 要件**: session は `tokio::spawn` された接続ハンドラ間で共有されるが、
  借用は spawn 境界を越えられない。

**変更**: `SecureSession` / `case::establish` / `UnsecuredExchange` を
`Arc<UdpTransport>` 保持に変える（ライフタイムパラメータ削除）。単一の `UdpTransport`
（1 本の bound UDP socket）が全 peer への送受信を担えるので、backend が
`Arc<UdpTransport>` を1つ持ち、全 session がその Arc を複製保持する。

- 単一 socket で複数 peer を捌く根拠: `send_to(buf, peer)` / `recv_from` は宛先を
  引数で取り、session が peer アドレスと session id で受信を選別する（`screen` が
  `from != self.peer` と session id 不一致を既に弾いている）。
- 影響範囲: `session.rs`（`transport` フィールドと `new` シグネチャを `Arc<UdpTransport>`
  へ）、`case.rs`（`establish` が `Arc<UdpTransport>` を取り、返す `SecureSession` に
  `Arc::clone` を渡す）。`UnsecuredExchange<'t>` は `establish` 内で一時利用され返り値に
  含まれないため **借用のまま据え置き**（`establish` 内で `&*transport` を渡す）。
  `exchange.rs` は無変更。既存テスト/ライブ E2E の `&transport` 呼び出しを `Arc` へ。
  **ワイヤ挙動は不変**（借用形態のみの変更）なので、既存の `case_self_handshake`
  ループバックテストが回帰チェックになる。

**代替（棄却）**: `self_cell`/`ouroboros` でノードごとに transport+session を1ユニット
自己参照保持 — 依存追加かつ不透明。`Arc` 化のほうが単純で mat-controller の API も素直。

### 決定 2: 新 `NativeBackend`（crate matd）が per-node warm セッションを管理

新モジュール `matd::native`。保持物:

- `creds: FabricCredentials`（起動時に KVS→自己発行 NOC で1回構築、プロセス寿命で不変）
- `transport: Arc<UdpTransport>`（起動時 bind、1本）
- `scope_id: u32`（`MAT_MATD_IFACE` の ifindex、起動時解決）
- `sessions: Mutex<HashMap<u64, Arc<Mutex<Option<SecureSession>>>>>`

**ロック方針（ノード間並行・同一ノード直列）**: 外側 `Mutex<HashMap>` は per-node の
`Arc<Mutex<Option<SecureSession>>>` を get-or-insert する短時間だけ保持し、すぐ手放す。
往復（mDNS/CASE/IM の await）は per-node の内側 Mutex を保持して行う。これにより
異なるノードへのコマンドは並行し、同一ノードは直列化される（single CASE session は
`TxCounter` / `RxWindow` を共有するため往復の重畳は不可）。

**セッション確立と寿命（無期限保持・失敗時のみ再確立）**:

1. コマンド処理時、per-node slot をロック。`Some(session)` なら再利用。
2. `None`（初回 or 破棄後）なら `dnssd::resolve_operational`（決定 1 で堅牢化した
   リゾルバ）→ 解決アドレスを順に `case::establish` → 成功した session を slot へ。
3. IM 送信（`read_attribute` / `invoke`）を実行。
4. **送信失敗（MRP 再送尽き = `SessionError`/`ImError` の送達失敗系）**なら slot を
   `None` に落とし、**同一コマンド内で1回だけ** 再解決→再CASE→再送をリトライ。
   これで「デバイスが session を evict した」「Thread リンクが一時切れた」場合に
   次コマンドを待たず回復する。リトライも失敗なら分類してエラー返却。
5. idle reap は **しない**（native session は chip-tool と違い busy-loop が無く、
   保持コストはメモリ数 KB。常にラグなしの目標のため warm を落とさない）。

**mat スキーマ応答は現行と同一**: native 経路の成功 body は `server.rs::simple_op` が
組み立てる JSON（`node_id`/`endpoint`/`cluster`/`command`/`status` 等）をそのまま使う。
`read` は `ImValue` → `value` へ正規化（chip-tool ws 経路の `normalize_value` と同じ
型に揃える）。

### 決定 3: `server.rs::run_op` を native/chip-tool の分岐点にする

ホットパス op（`On`/`Off`/`Color`/`ColorTemp`/`Read`(onoff on-off のみ)）は
`NativeBackend` へ。それ以外（`Write`・任意 cluster の `Read`/`Invoke`・`Describe`・
`GroupProvision`/`GroupInvoke`/`GroupColorTemp`/`GroupColor`）は現行どおり
`ChipToolBackend`。

- **native の適用条件**: `MAT_MATD_IFACE` が設定され `NativeBackend` の構築に成功して
  いること。未設定 or 構築失敗（KVS 読めない等）なら native を無効化し、**全 op を
  chip-tool へ回す**（安全フォールバック。脱 chip-tool 前は現行挙動を完全維持できる）。
- **`Read` の native 対象は onoff `on-off` に限定**（M4 スコープ）。他の cluster/attr の
  read は汎用 attribute 名→ID テーブルが要るため chip-tool フォールバック。判定は
  `cluster == "onoff" && attribute == "on-off"`。
- 応答スキーマ・エラー分類（exit code へ効く `ErrorKind`）は経路によらず一致させる。
  native の失敗は `session_failed`（CASE 確立失敗）/ `timeout`（MRP 尽き）/ `unreachable`
  （mDNS 解決 timeout）へ、chip-tool 経路の分類（README の表）と同じ `kind` に写像する。

### 決定 4: `ChipToolBackend` を lazy spawn 化

現状 `ChipToolBackend::new` は起動時に eager connect（chip-tool を spawn して ws 接続）。
native ホットパスのみの負荷では chip-tool を起こしたくない。

**変更**: 起動時の eager `ensure_connected` を外し、初回フォールバック/group op が来た
ときに遅延 spawn する。native 無効時（`MAT_MATD_IFACE` 未設定）は初回コマンドで結局
spawn されるので現行と実質同挙動。idle reap / keepalive / respawn ロジックは不変。

- 副作用: 「chip-tool 不在」エラーが起動時でなく初回フォールバック op 時に出る。
  native-only ワークロードでは chip-tool 不在でも matd が立ち上がる（脱 chip-tool への
  布石として妥当）。起動ログに native/chip-tool どちらが有効かを出す。

### 決定 5: `im` に move-to-color-temperature を追加

`ColorTemp` ホットパスに必要。`im.rs` へ:

- `CMD_MOVE_TO_COLOR_TEMPERATURE: u32 = 0x0A`
- `encode_move_to_color_temperature_fields(mireds: u16, transition: u16)`:
  `struct{0: ColorTemperatureMireds(u16), 1: TransitionTime(u16), 2: OptionsMask(u8)=0,
  3: OptionsOverride(u8)=0}`（`MoveToHueAndSaturation` エンコーダと同じ構造・TDD）。

換算（kelvin→mireds、% など）は現行どおり mat CLI 側で済んでおり、matd/native は
数値をそのまま送る。

## スコープ

| モジュール | 変更 |
|---|---|
| `mat-controller::session` | 決定 1（`Arc<UdpTransport>` 化、ライフタイム除去） |
| `mat-controller::case` | 決定 1（`establish` が `Arc<UdpTransport>` を取り clone を渡す） |
| `mat-controller::im` | 決定 5（color-temperature 定数 + エンコーダ） |
| `matd::native`（新規） | 決定 2（`NativeBackend`、per-node warm session 管理） |
| `matd::server` | 決定 3（`run_op` 分岐、native 応答 body の共用、エラー写像） |
| `matd::backend` | 決定 4（lazy spawn 化） |
| `matd::main` | 起動時に `NativeBackend` を構築（`MAT_MATD_IFACE` 有効時）、両 backend を serve へ |
| `tests/`（matd 統合 + live_jarvis 系） | native 経路のユニット/実機受け入れ |
| `scripts/e2e-m4.sh` + Taskfile | クロスビルド → 転送 → matd 起動 → socket 経由コマンドの一発化 |

## 受け入れ基準（M4）

### CI（実機なし、`task check` 全通過を維持）

1. 決定 1 のライフタイム除去後も `case_self_handshake`（ループバック CASE + read）が通る
   （ワイヤ挙動不変の回帰チェック）。
2. `im` の move-to-color-temperature エンコーダのユニットテスト（既知バイト列と一致）。
3. `NativeBackend` のロジックをユニットテスト（実ソケット loopback で）:
   - 同一ノード連続コマンドが session を再利用する（2回目は再 CASE しない）。
   - 送信失敗で slot を破棄し1回だけ再確立を試みる。
   - 未対応 op が chip-tool 経路へ回る分岐（`run_op` の振り分け）。
4. native 無効（`MAT_MATD_IFACE` 未設定）時、全 op が現行どおり chip-tool へ回る。

### jarvis 実機（`task e2e:m4`、要 `MAT_E2E_HOST` / `MAT_E2E_NODE_ID` / `MAT_E2E_IFACE`）

matd を native ビルドで起動（chip-tool は既存 systemd 版と別ポート/別 socket で）、
unix socket 経由で:

5. `on` / `off` / `onoff read` が native warm session で往復し、read が状態を反映。
6. 2回目以降の同一ノードコマンドが mDNS+CASE を払わず高速（warm 実証。ログで
   「session reused」を確認、初回との所要時間差）。
7. `color`（move-to-hue-and-saturation）と `color-temp`（move-to-color-temperature）が
   native で通り、`current-hue`/`color-temperature` read が目標付近（±8 相当）。元へ復元。
8. `describe` と `write` がフォールバックで従来どおり動く（chip-tool 経路の非回帰）。
9. デバイスが session を落とした状況（別経路で evict 等）を模し、次コマンドが自動
   再確立で回復する（決定 2 step 4 の実機確認。難しければ「送信失敗→再確立」を
   ログで確認できれば可）。

## 非ゴール

- 残る unicast op（write・任意 cluster read/invoke・describe）の native 化 — 後続
  マイルストーン（脱 chip-tool の本丸）。M4 はホットパスのみ。
- group セッション（groupcast 送信）の native 化 — M5。M4 では group は完全に
  chip-tool のまま。
- commissioning の native 化（第二期、chip-tool one-shot 継続）。
- mDNS のキャッシュ/再解決の高度化・複数 iface 自動検出。`MAT_MATD_IFACE` 単一指定。
- one-shot `mat` CLI 直経路の native 化（親 spec で「M4 と同時か M5 後か」未決。M4 は
  matd のみ触る）。

## リスク

- **同一デバイスへの native CASE と chip-tool CASE の並存**: 例 `on` は native、
  `describe` は chip-tool で、同一ノードに2つの CASE session が立ちうる。Matter は
  同一 (fabric, node) からの複数 CASE を許すので機能上は問題ない（M2b/M3 と同根）。
  ただしデバイスの session テーブル枠は有限。単一デバイスに数 session 程度なら許容。
  逼迫の兆候（CASE が `Busy`/`NoSpace` で蹴られる）はログで観測し、実機 E2E で
  frequency を確認。
- **groupcast counter 混在禁止**（実機知見 `groupcast-e2e-findings`）は unicast のみの
  native には非該当。group は chip-tool 一本のままなので送信者一本化が保たれる。
- **`Arc` 化のリグレッション**: ワイヤ形は変えない。`SecureSession` の `TxCounter` /
  `RxWindow` 等の状態が借用→Arc 保持で崩れないこと（`establish` は依然 `UnsecuredExchange`
  で Sigma1〜3 を張り、成立後の `SecureSession` に同一 socket の Arc を渡す）。
  ループバックテスト（受け入れ 1）と `task e2e:m2`/`e2e:m3` の再走で担保。
- **native と chip-tool の別 socket/別ポート運用**: 実機検証では既存本番 matd
  （chip-tool 版、systemd）を止めずに native matd を別 socket で立てる。port9100
  孤児の罠（`matd-port9100-orphan`）を避けるため、native matd はデフォルトで
  chip-tool を spawn しない（決定 4）ので衝突しにくいが、フォールバック検証時は
  別ポート指定。
- **`MAT_MATD_IFACE` 誤設定**: 解決 timeout → `unreachable`。切り分けは
  `avahi-browse -rtp _matter._tcp` と、必要なら iface 名の再確認。native 構築失敗時は
  chip-tool フォールバックに落ちるので matd 自体は無停止。
