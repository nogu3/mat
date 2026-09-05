# IPK ローテーション（`mat fabric rotate-ipk`）と write_keyset 無損失化 — 設計

日付: 2026-09-05 / 監査バックログ（2026-08-31）残件 / ブランチ `worktree-ipk-rotation`

## 背景と目的

fabric の IPK（Identity Protection Key、keyset 0）は `mat fabric init` でランダム
epoch を生成して以来、あるいは chip-tool 産 fabric の固定 epoch
`temporary ipk 01` を採用永続して以来、一度も変えられない。IPK は CASE の
destination id の秘匿と keyset 0 に紐づく groupcast の鍵素材であり、Matter spec
（§4.15.2 / §11.2.6）は Administrator が `KeySetWrite`（GroupKeySetID 0）で
ローテーションすることを想定している。M8c-3 で「将来候補 (3)」として残した
「全ノード KeySetWrite での epoch 完全移行」を実装する。

同じ `group_settings` 領域の小フォローも先に片付ける: `write_keyset` の既存 id
上書きが mat の 1 スロット形で作り直すため、chip-tool `add-keysets` 由来の複数
epoch keyset の policy / 残スロットが落ちる。`keyset_with_next` と同型の slot-0
差し替えヘルパで無損失化する（ローテーションの controller 側 commit も同じ
ヘルパで k/0 を書き換えるので、先に作ると両方が使える）。

触る範囲: `mat-controller` の `group_settings.rs` / `kvs.rs` / `im/cmdfields.rs`、
`mat-native` の `lib.rs`（確立器の生成口）/ `ops.rs`（デバイス側 1 ステップ）/
新規 `rotate_ipk.rs` / `commission.rs`（`resolve_ipk_epoch` の可視性）、
`crates/mat` の `cli.rs` / `main.rs` / `commands/fabric.rs`、`mat-core::body`、
docs。**触らない**: `mat-controller` の `dnssd/` / `test_support.rs`、
`mat-core` の `ids.rs`、`mat-device`（並行セッションの担当）。

## スコープ外

- **matv（`mat-device`）側の IPK ローテーション対応。** 現状の matv は
  `KeySetWrite(0)` を INVALID_COMMAND で拒み、keyset は epoch key 0 の 1 本しか
  持てない（`core/group_key_management.rs` のモジュール doc）。mat-device は
  並行レーンの担当なので本設計では触らず、matv 相手の統合テストは**失敗経路**
  （device_rejected → pending → 既存 IPK で引き続き動作 → abort）を検証する。
  成功経路は fake `NodeConn` のユニットテストで担保し、matv の対応
  （KeySetWrite(0) 受理・複数 epoch の destination id 候補）は後続案件とする。
- matd のホットリロード（socket op の追加）。matd は group 鍵と fabric 資格情報を
  起動時に読み込んでメモリに持つが、リロード機構は無く、`group provision` も
  「restart 案内の note」で運用している。本設計も同じ規律（§6）。2 epoch 併存に
  より restart 前でも 1 世代は動作が続く。
- 旧 epoch をデバイスから即時退役させる第 3 パス。rolling 2 epoch（§3.4）で
  次回ローテーション時に自然に消える。
- 本番 fabric への実行（ユーザー判断）。
- ノードの並列処理（provision / grant / remove と同じ逐次）。

## 1. 用語と不変条件

- **E_cur**: 現行 epoch。`mat/f/<idx>/ipk-epoch` に永続（無ければ commission と
  同じ規則で chip-tool 既定定数を検証して採用永続 — `resolve_ipk_epoch`）。
  controller KVS の `f/<idx>/k/0` slot 0 は常に `derive(E_cur)` と一致する
  （既存の整合検証をそのまま保つ）。
- **E_next**: 配布中の新 epoch。`mat/f/<idx>/ipk-epoch-next`。存在 = pending。
- **E_prev**: 直前の epoch。`mat/f/<idx>/ipk-epoch-prev`。commit 時に E_cur から
  写す。取り残しノードの追いつき（§3.5）にだけ使う。
- 3 キーとも値は 16 バイト epoch 鍵の base64（既存 `ipk-epoch` と同形）。
  chip-tool は未知キーを無視するので INI 互換に影響しない。
- **状態**: `idle`（next 無し）/ `pending`（next 有り）。遷移は §3。

## 2. KVS 層（`mat-controller`）

### 2.1 `keyset_with_slot0`（write_keyset 無損失化）

```rust
/// KeySetData の ctx3 配列・先頭 struct の (ctx4 start_time, ctx5 hash, ctx6 key)
/// だけを差し替えた blob を返す。policy / keys_count / 残スロット / 未知タグ /
/// ctx7 next は TLV 要素単位でそのまま写す（`keyset_with_next` と同型）。
/// 外側が struct でない / ctx3 が無い / 先頭 struct が無い / 途中で切れている
/// blob は `None`。
fn keyset_with_slot0(blob: &[u8], start_time: u64, hash: u16, key: &[u8; 16]) -> Option<Vec<u8>>
```

- 先頭 struct 内では ctx4/5/6 を新値で置き、それ以外の要素（未知タグ・ネスト）は
  写す。ctx4/5/6 のいずれかが元 blob に無い場合は補って書く（順序は 4,5,6 を
  先頭 struct の末尾に置く。mat / chip-tool どちらの書き手も 3 つ揃えるので
  実運用では発生しない）。
- `write_keyset` の既存 id 上書き腕は `serialize_keyset(...)` の作り直しをやめ、
  `keyset_with_slot0(&blob, EPOCH_START_TIME, hash, operational)` に置き換える
  （None は既存どおり `Corrupt`）。既存の「1 スロット上書き」注記 doc を更新。
- テスト: (a) mat 1 スロット形に対して `serialize_keyset` と**バイト一致**
  （既存 provision テストが暗黙に守っている形を pin）。(b) chip-tool 3 epoch 形
  （既存 fixture `multi_epoch_keyset`）で policy / keys_count / slot 1,2 / next が
  無傷、slot 0 だけ変わる。(c) `keyset_with_next_preserves_unknown_tags...` と
  同じ「スロット内 ctx7 / 未知 ctx9 / ネスト」fixture で先頭 struct の
  ctx4/5/6 以外は 1 バイトも変わらない。(d) 壊れた blob は None。
  (e) `write_group_provision` を同 keyset id で 2 回（re-provision）→ chip-tool
  形の前置きレコードが policy と残スロットを保つ（回帰テスト、既存
  `remove_group_unlinks_after_chiptool_multi_epoch...` の対）。

### 2.2 epoch キーの読み書き（`kvs.rs`）

既存 `mat_ipk_epoch_key` / `read_mat_ipk_epoch` / `write_mat_ipk_epoch` に倣い、
汎用化する:

```rust
pub enum IpkEpochSlot { Current, Next, Prev }   // "ipk-epoch" / "ipk-epoch-next" / "ipk-epoch-prev"
pub fn mat_ipk_epoch_slot_key(fabric_index: u8, slot: IpkEpochSlot) -> String;
pub fn read_mat_ipk_epoch_slot(main_ini, fabric_index, slot) -> Result<Option<[u8;16]>, KvsError>;
```

既存 2 関数はシグネチャを変えず `Current` の薄い別名として残す（呼び手 3 箇所は
触らない）。書込はすべて §2.3 の 1 トランザクション関数経由で行い、単独 write
は追加しない（`write_mat_ipk_epoch` は既存のまま）。

### 2.3 ローテーションの KVS トランザクション（`group_settings.rs`）

```rust
/// pending 開始: `ipk-epoch-next` を書く（既に同じ値なら no-op、別の値なら Corrupt
/// — 呼び手は事前に read して resume 判定する想定なので、ここで違う値に出会うのは
/// 並行実行の証拠）。
pub fn begin_ipk_rotation(main_ini, fabric_index, next: &[u8;16]) -> Result<(), GroupSettingsError>;

/// commit: 1 KvsTxn で (1) f/<idx>/k/0 を keyset_with_slot0(EPOCH_START_TIME,
/// derive_group_session_id(op_next), op_next) で差し替え（k/0 欠落 / 解釈不能は
/// Corrupt）、(2) ipk-epoch := next、(3) ipk-epoch-prev := cur、(4) ipk-epoch-next
/// を削除。`next` が KVS の ipk-epoch-next と一致しなければ Corrupt（並行実行）。
pub fn commit_ipk_rotation(main_ini, fabric_index, cfid: &[u8;8], cur: &[u8;16], next: &[u8;16]) -> Result<(), GroupSettingsError>;

/// abort: ipk-epoch-next を削除するだけ。無ければ Ok(false)。
pub fn abort_ipk_rotation(main_ini, fabric_index) -> Result<bool, GroupSettingsError>;
```

flock + tmp+rename は `KvsTxn` に既に備わる（provision と同じ規律）。
`Locked` はハードエラー。テストは tempdir の INI で: begin→commit 後に
`read_fabric_credentials` の `ipk_operational == derive(next)`、`ipk-epoch ==
next`、`prev == cur`、`next` 無し、k/0 の policy / 残スロット / next リンク
（chip-tool 3 epoch 形の k/0 を fixture に）無傷、commit の不一致 / k/0 欠落は
Corrupt で何も書かれない（INI バイト不変）。

### 2.4 KeySetWrite の複数 epoch 符号化（`im/cmdfields.rs`）

```rust
/// GroupKeySet{0: id, 1: policy TrustFirst, 2/3: epochKey0/start0, 4/5: key1/start1,
/// 6/7: key2/start2}。`epochs` は (key, start_time) を 1〜3 本、start_time は
/// 呼び手が単調増加かつ非 0 を保証（spec §11.2.7.1 の INVALID_COMMAND 条件）。
/// 無い本数は null。
pub fn encode_key_set_write_fields_multi(keyset_id: u16, epochs: &[([u8;16], u64)]) -> Vec<u8>
```

既存 `encode_key_set_write_fields(id, key)` は `multi(id, &[(key, 1)])` の別名に
して、provision の既存テスト（バイト列 pin）で等価を保証する。

## 3. ローテーションの流れ（`mat-native::rotate_ipk`）

### 3.1 入力

```rust
pub struct RotateIpkParams {
    pub node_ids: Vec<u64>,      // CLI が台帳 / --nodes から解決済み
    pub mode: RotateMode,        // Rotate | CatchUp | Abort
    pub per_node_timeout_ms: u64 // 0 = 無制限（CLI の --op-timeout-ms）
}
```

### 3.2 `Rotate`（既定）

1. 資格情報を KVS から読む（`Engine::build` と同じ `read_self_issue_materials` →
   `FabricCredentials`。`lib.rs` に `load_fabric_credentials(cfg)` として切り出す）。
   E_cur は `commission::resolve_ipk_epoch`（`pub(crate)` に上げる）で解決 —
   永続済みなら k/0 との整合を検証、無ければ chip-tool 既定を検証採用。
2. `ipk-epoch-next` を読む。有れば **resume**（E_next = その値、新鍵は作らない）。
   無ければ CSPRNG 16 バイトを E_next とし（E_cur と一致したら引き直す）、
   `begin_ipk_rotation` で永続してから先へ進む — デバイスに触る前に永続する
   ので、途中クラッシュしても再実行は同じ E_next で再開し、3 本目の鍵が
   生まれない。
3. 確立器を 2 つ作る: `est_cur`（creds そのまま）と `est_next`
   （`ipk_operational := derive(E_next, cfid)` に差し替えた clone）。
   `lib.rs` に `Engine::case_establisher(cfg, creds, resolver) -> Box<dyn Establisher>`
   を公開し、`build_with_resolver` もそれを使う（確立器の作り方は 1 箇所）。
   group egress の UDP bind は不要なので Engine は作らない。
4. 各ノードを **逐次**、失敗しても続行:
   1. `est_cur.establish(node)` → `ops::write_ipk_keyset(conn, &[(E_cur,1),(E_next,2)])`
      （`KeySetWrite` keyset 0、ep0、timed 無し）→ close。
   2. **受理の実証**: `est_next.establish(node)` → close。新 IPK で Sigma1 の
      destination id が受理された = デバイスが E_next を持った証拠。KeySetRead は
      鍵を返さないので、これが唯一の実証手段。
   3. 1〜2 全体を `per_node_timeout_ms` で囲む（超過は `timeout`）。結果を
      `NodeOutcome { node_id, status: ok|failed, error: Option<MatError> }` に積む。
      失敗の detail には `node N: ` と、どのステップ（`key-set-write` /
      `verify-case`）かを載せる。
5. 全ノード ok → `commit_ipk_rotation(cur, next)` → `status: "rotated"`。
   1 つでも失敗 → commit しない（pending のまま）→ `status: "pending"`。
   ノード 0 件（台帳が空で `--nodes` 無し）は配布相手が無いので即 commit
   （warn ログ）。

### 3.3 途中失敗と再実行

- pending 中の再実行は同じ E_next で 4〜5 をやり直す。既に {E_cur, E_next} を持つ
  ノードへの再書込は冪等（同じ内容の KeySetWrite）。
- `--nodes` を絞って再実行すると、その部分集合が全部 ok で commit する
  （前回 ok だったノードは再検証しない）。**除外したノードは E_cur のままになる**
  ので、後で §3.5 の追いつきが要る — 出力 `note` で明示する。
- 恒久的に届かないノード（電池切れ・撤去済み）はこの「絞って commit →
  戻ったら catch-up」で扱う。`--force` 相当は設けない。
- **停止に失敗はない**: 中断・クラッシュはどの時点でも安全。commit 前ならデバイスは
  {E_cur, E_next} / controller は E_cur で整合。commit は 1 トランザクション。

### 3.4 rolling 2 epoch（デバイス側の旧鍵退役）

配布は常に「現行 + 新」の 2 本を書くので、デバイスの keyset 0 は
{E_{n-1}, E_n} → 次回 {E_n, E_{n+1}} と入れ替わり、2 世代前は自然に消える。
CASE responder は全 epoch を試す（spec §4.13.2.4 — SDK `FindLocalNodeFromDestinationId`
も全 `num_keys_used` を走査）ので、controller / matd が E_cur でも E_next でも
接続できる。IPK は session 鍵ではなく destination id の秘匿用なので、1 世代の
残存は許容する（spec は最大 3 epoch の併存を前提にしている）。

### 3.5 `CatchUp`

前提: idle かつ `ipk-epoch-prev` 有り（無ければ `other`「no previous epoch」）。
各ノード: `est_prev.establish` → `write_ipk_keyset(&[(E_prev,1),(E_cur,2)])`
→ close → `est_cur.establish`（実証）→ close。KVS は変えない。
`status: "caught_up"` / 失敗有りは §5 の pending と同じ扱い（status は
`"catch_up_incomplete"`）。2 世代取り残されたノードは prev でも繋がらないので
`unreachable` / `session_failed` になる — detail に「re-commission」を案内。

### 3.6 `Abort`

`abort_ipk_rotation`。デバイス側は触らない（{E_cur, E_next} が残るが E_cur で
動作し続け、次回ローテーションで上書きされる）。`status: "aborted"`、
pending でなければ `"idle"`（エラーではない）。

## 4. CLI（`crates/mat`）

```
mat fabric rotate-ipk [--nodes <N>...] [--catch-up | --abort]
```

- `FabricAction::RotateIpk { nodes: Vec<String>, catch_up: bool, abort: bool }`
  （`catch_up` と `abort` は `conflicts_with`）。`nodes` は alias 解決対象
  （既存の `--nodes` と同じ `resolve` 経路）。省略時は台帳 `Store::nodes()` の
  全ノード（node_id 昇順）。指定時は各 id を `require_node`（exit 11）。
- `fabric init` / `list` と違いネットワークに触るので、main.rs の早期 dispatch
  には入れず、iface 解決後の**直経路専用**（`commission` / `unpair` と同じ位置、
  `--matd` は無視して warn — matd プロトコルには載せない）。
- `--op-timeout-ms` は per-node 予算として渡す。
- `commands/fabric.rs::run_rotate_ipk(store_path, native_cfg, params)` が
  `mat_native::rotate_ipk::run` を tokio current_thread で回し、body を emit。
- `fabric list` の各行に `"ipk_rotation_pending": bool` を追加（`ipk-epoch-next`
  の有無）。

## 5. 出力 JSON と終了コード

成功（rotated / caught_up / aborted / idle）は stdout に:

```json
{
  "timestamp": "2026-09-05T12:34:56+09:00",
  "fabric_index": 2,
  "status": "rotated",
  "nodes": [
    { "node_id": 5, "status": "ok" },
    { "node_id": 6, "status": "ok" }
  ],
  "note": "if matd is running, restart it to load the new IPK"
}
```

部分失敗（pending / catch_up_incomplete）は **stdout に同じ形の body**
（失敗ノードは `{"node_id": 7, "status": "failed", "error": {"kind": "unreachable",
"detail": "..."}}`）を出したうえで、**stderr に error、終了コードは最初に失敗した
ノードの kind** に従う（`unreachable` → 5 など）。error の detail は
`ipk rotation pending: 1 of 3 nodes failed (node 7: unreachable); re-run
`mat fabric rotate-ipk` (same nodes, or --nodes <subset> to commit without
them)`。stdout の body は pure JSON のまま（設計ルール 2）で、機械可読な per-node
結果はそちらに載せる。鍵素材は一切出さない。

ローカル側の失敗（store_missing / store_parse / KVS locked = other / 並行実行
= store_parse「inconsistent rotation state」）は従来どおり stderr error のみ。

## 6. matd との関係

- `rotate-ipk` は matd プロトコルに載せない（`commission` / `unpair` と同じ
  直経路専用）。matd 稼働中でも直経路が別 CASE session を張るだけ。
- matd はメモリの `FabricCredentials`（E_cur 由来の `ipk_operational`）で新規
  CASE を張り続けるが、デバイスは {E_cur, E_next} を受理するので commit 後も
  1 世代は動作する。**次回ローテーションの前に必ず restart** が必要 —
  `note` と docs に明記。既存の warm session / 購読は影響を受けない（session 鍵は
  確立時に導出済み）。
- commit 後の新規 `commission` は `resolve_ipk_epoch` が E_next（= 新 E_cur）を
  返すので AddNOC の IPKValue も新鍵になる。

## 7. テスト

- **ユニット（mat-controller）**: §2.1 (a)〜(e)、§2.3、§2.4（1 本形が既存
  エンコーダとバイト一致、2 本形の TLV 形。4 本以上は呼び手のバグなので
  `debug_assert!` + doc で禁じ、release では先頭 3 本だけ書く）。chip-tool INI 互換の既存テストは全部残す。
- **ユニット（mat-native）**: fake `Establisher` / `NodeConn`（`mat-native/src/
  test_support.rs` の既存 fake を利用 — 触ってはいけないのは `mat-controller`
  側の `test_support.rs`。足りない fake は `rotate_ipk.rs` の test mod に閉じる）で: 全 ok → commit・KVS 3 キー・k/0
  slot0 が derive(next)、1 ノード失敗 → pending・KVS は next だけ・k/0 不変、
  resume が同じ next を使う、verify-CASE 失敗は failed 扱い、catch-up が prev で
  確立し {prev, cur} を書く、abort、ノード 0 件で即 commit、per-node timeout。
  KeySetWrite の fields バイト列（keyset 0・2 epoch・start 1/2）を pin。
- **統合（matv、`scripts/e2e-device-m4.sh` + `task e2e:device:m4`）**: fresh
  store → `fabric init` → `commission`（matv）→ `group provision` → `rotate-ipk`
  → 期待: exit 4（device_rejected、matv は KeySetWrite(0) を拒む）、stdout body
  `status:"pending"`、`fabric list` が `ipk_rotation_pending:true` → `mat on` /
  `group invoke` が**旧 IPK で引き続き通る**（controller 未 commit の証明）→
  `rotate-ipk --abort` → `fabric list` pending:false → `mat on` 再度 ok。
  matv が KeySetWrite(0) に対応した後は同スクリプトの期待を成功経路
  （rotated → `mat on` が新 IPK で通る）に差し替える旨をコメントに残す。
- **実機（hogar-matd コンテナ、隔離 store `/tmp/mat-rot-smoke`、本番 `/data/mat`
  には触らない）**: x86_64 バイナリを `docker cp` して `fabric init` →
  `rotate-ipk`（ノード 0 件 → rotated）→ `fabric list` → `rotate-ipk --nodes 99`
  → exit 11 → `--catch-up`（prev 有り、nodes 0 件 → caught_up）。ネットワークには
  出ない。本番 fabric の rotate はユーザー判断で、本設計の作業では回さない。
- `task check` 全通過。

## 8. docs

- `docs/commands.md`: fabric 節に `rotate-ipk`（流れ・状態・再実行・catch-up・
  abort・matd restart・rolling 2 epoch・matv 未対応の注記）、`fabric list` の
  新フィールド、「Routing through matd」の直経路専用リストに追加。
- `docs/errors.md`: 部分失敗の「stdout body + stderr error」規約を追記。
- `ARCHITECTURE.md`: M8c-3 将来候補 (3) を実装済みに訂正、KVS の 3 キーを
  backend 節へ。`CLAUDE.md` の backend 箇条書き（IPK / `ipk-epoch`）に 1 行。
- `README.md` の fabric 記述があれば 1 行。
