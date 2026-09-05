# matv 積み残し（KeySetRemove/Read・privacy フラグ・membership 残骸・rollback assert・bitmap リプレイ窓）設計

日付: 2026-09-05 / 対象ブランチ: worktree-matv-keyset-remove（base: main b1443de = v1.31.0）
触る範囲: `crates/mat-device` + `crates/matv` + `scripts/e2e-device-m3.sh` + README の matv 段落。
mat-core / mat-controller / mat-native / crates/mat / matd には触らない（並行セッションが
mat-controller / mat-core を触っている。im.rs 分割を含む）。必要な定数・暗号ヘルパは
すべて mat-device 内に置く。

## 0. 背景と現状（コード確認済み）

監査バックログ（レーン A フェーズ 2 の残、2026-09-03）の matv 側 5 件:

1. **KeySetRemove / KeySetRead / KeySetReadAllIndices 未実装** —
   `core/group_key_management.rs` の `invoke` は `CMD_KEY_SET_WRITE`(0x00) 以外を
   `STATUS_UNSUPPORTED_COMMAND`(0x81) で弾く。レーン B の `mat group remove`
   （`mat-native/src/ops.rs::remove_group_node`: ACL 除去 → RemoveGroup → group-key-map
   read-merge-write → 未参照 keyset に `KeySetRemove {0: id}`）が最終ステップで
   `device_rejected` になるため、`scripts/e2e-device-m3.sh` は remove ステップを保留中
   （スクリプト内コメントに再有効化手順あり）。`GroupKeySet` は `epoch_key0` だけ保持し
   `EpochStartTime0` は読み捨てている。IPK（keyset 0）は `GroupKeyStore` には無く
   `FabricEntry.ipk_operational` にある。
2. **privacy フラグ（security flags P ビット 0x80）** — `net/group_rx.rs::classify_group_datagram`
   は `GroupDrop::Privacy` で即 drop。chip SDK（chip-tool / Apple Home / matter.js 経由の
   HA）の groupcast は常に P ビット付きで送るので、実証済みなのは mat/matd（`GROUP_SECURITY_FLAGS
   = 0x01`、P 無し）からの受信だけ。mat-controller の crypto には AES-CTR も privacy key
   導出も無い（`ccm`/`aes`/`hkdf` はある。`ctr 0.9.2` は ccm 経由で Cargo.lock に既にある）。
3. **削除した `[[device]]` の membership が `groups.json` に残る** — `EndpointLedger` は
   tombstone で endpoint を保持する設計（再追加で同じ endpoint を復元）だが、
   `GroupMembershipStore` は起動時に何も掃除しない。残骸は `GroupTable` 属性に存在しない
   endpoint として出る・multicast join 対象になる・`handle_group_invoke` が存在しない
   endpoint へディスパッチする。
4. **fail-safe rollback テストの assert 不足** — `commissioning.rs::rollback_uncommitted_fabric`
   は ACL / GroupKey / membership の 3 store を purge するが、
   `fail_safe_expiry_rolls_back_uncommitted_fabric` は GroupKeyStore しか assert していない。
   disarm（`ArmFailSafe(0)`）経由の rollback にはテストが無い。
5. **リプレイ窓が単調 counter のみ** — `GroupReplayGuard` は `(fabric, source)` ごとの最終
   counter 以下を全部落とす。spec §4.5.4.2 の group data message counter は「最大値 + 直近
   32 件の bitmap」で順序逆転を許容し、2^31 未満の前進を rollover 込みで新規扱いする。

## 1. スコープ

**入る**: 上記 5 件すべて + e2e m3 の `mat group remove` ステップ再有効化 + README/モジュール
doc の更新。

**入らない**: GroupKeySecurityPolicy=CacheAndSync、epoch key 1/2（`MAX_GROUP_KEYS_PER_FABRIC=1`
のまま）、IPK ローテーション、matv からの groupcast 送出、group 宛 Read/Write、
Message Extensions（X フラグ）、chip SDK 実機との privacy 相互接続確認（実機不使用。
導出式は SDK `Crypto::DeriveGroupPrivacyKey` / `CryptoContext::BuildPrivacyNonce` に合わせ、
ラウンドトリップと定数ピンでテストする。実 Apple Home / chip-tool での確認は後続）、
mat-controller 側の変更（P ビット付き送出も含めて一切触らない）、版上げ。

## 2. GroupKeyManagement: KeySetRead / KeySetRemove / KeySetReadAllIndices

### 2.1 定数（mat-device 内に局所定義）

`mat_controller::im` には `CMD_KEY_SET_WRITE` しか無く im.rs は触れないので、
`core/group_key_management.rs` に置く:

| 定数 | 値 | spec |
|---|---|---|
| `CMD_KEY_SET_READ` | 0x01 | §11.2.8.2 |
| `RESP_KEY_SET_READ` | 0x02 | §11.2.8.3 |
| `CMD_KEY_SET_REMOVE` | 0x03 | §11.2.8.4 |
| `CMD_KEY_SET_READ_ALL_INDICES` | 0x04 | §11.2.8.5 |
| `RESP_KEY_SET_READ_ALL_INDICES` | 0x05 | §11.2.8.6 |
| `IPK_KEY_SET_ID` | 0 | §11.2.6.2 |

`accepted_commands` = `[0x00, 0x01, 0x03, 0x04]`、`generated_commands` = `[0x02, 0x05]`。
`invoke_privilege` は全コマンド Administer のまま（spec §11.2.5: KeySet 系はすべて Administer）。
PASE（`ctx.fabric_index == 0`）は全コマンド `STATUS_UNSUPPORTED_ACCESS`（現行 KeySetWrite と同じ）。

### 2.2 `GroupKeySet` に `epoch_start_time0: u64` を追加

`#[serde(default)]` で `group_keys.json` の旧形式（フィールド無し）を 0 として読む。
KeySetWrite の `Context(3)` を保存する（無ければ 0 — 現行の「読み捨て」から後方互換に
緩く倒す。spec 上は必須だが、他コミッショナの互換を壊さない側）。`Debug` は引き続き
鍵だけ伏せる。`upsert_keyset(fabric, id, epoch_key0, epoch_start_time0)` にシグネチャ拡張。

### 2.3 KeySetRead `{0: GroupKeySetID}` → `KeySetReadResponse {0: GroupKeySetStruct}`

- フィールドの struct 形が壊れている / id 欠落 → `STATUS_INVALID_COMMAND`
  （`groups.rs::decode_group_id` と同じ裁定。KeySetWrite の Malformed/Constraint 2 段は
  フィールドが多いための区別で、単一 id には不要）。
- `id == 0`（IPK）→ 仮想 keyset として応答: `{0: 0, 1: 0(TrustFirst), 2: null, 3: 0, 4: null,
  5: null, 6: null, 7: null}`。IPK は CASE を張れた fabric に必ず存在するので store を見ない。
- store に `(fabric, id)` あり → `{0: id, 1: 0, 2: null, 3: epoch_start_time0, 4: null, 5: null,
  6: null, 7: null}`。**EpochKey0/1/2 は必ず null**（spec §11.2.8.3: 鍵素材は返さない）。
- 無し → `STATUS_NOT_FOUND`(0x8B)。

### 2.4 KeySetRemove `{0: GroupKeySetID}`

- 形不正 / id 欠落 → `STATUS_INVALID_COMMAND`。
- `id == 0` → `STATUS_INVALID_COMMAND`（spec §11.2.8.4: IPK は外せない。`mat group remove` 側にも
  同じガードがある）。
- store に無し → `STATUS_NOT_FOUND`。
- あり → keyset を削除し、**同 fabric の GroupKeyMap でその keyset を参照する行も削除**
  （spec §11.2.8.4 手順 4 のカスケード）。map が変化したら `ctx.changed.push(ATTR_GROUP_KEY_MAP)`。
  応答は `STATUS_SUCCESS`（response command 無し）。`GroupKeyStore::remove_keyset(fabric, id)
  -> Result<bool /* map_changed */, u8>` を新設し、1 回の lock・1 回の save で行う。
- groupcast 受信側は `gk_store.keysets()` を毎 datagram 引くので、削除後は GKH 一致候補が
  無くなり `NoKeyset` で落ちる（追加配線なし）。multicast join は membership 由来なので不変。

### 2.5 KeySetReadAllIndices `{}` → `{0: [GroupKeySetID...]}`

引数は空 struct（Matter 1.1 以降）。壊れた TLV でも引数は使わないので読まずに応答する
（chip も同様に無視する）。応答リストは `[0]` + accessing fabric の store 上の id を
挿入順に。`GroupKeyStore::keyset_ids_for(fabric) -> Vec<u16>` を新設。

### 2.6 テスト（`group_key_management.rs` の tests）

- `declares_attributes_and_key_set_write_command` を accepted/generated 更新。
- 既存 `unknown_attribute_and_unknown_command_are_rejected` は 0x01 を「未対応の例」に
  使っているので、0x77（存在しない id）に差し替える。
- KeySetRead: 書いた keyset を読むと id/policy/start time が返り、EpochKey フィールドは
  すべて null、他 fabric からは NOT_FOUND、id 0 は仮想応答、PASE は UNSUPPORTED_ACCESS。
- KeySetRemove: 成功 → `keyset_exists` false + map の参照行も消える + `ctx.changed` に
  GroupKeyMap、参照無しなら changed 無し、未知 id NOT_FOUND、id 0 INVALID_COMMAND、
  他 fabric の keyset は消えない、persist（`MemPersist`）に反映。
- KeySetReadAllIndices: `[0]` のみ / `[0, 42]`、他 fabric は混ざらない。
- `epoch_start_time0` の永続化ラウンドトリップ + 旧 JSON（フィールド無し）の load。

### 2.7 e2e m3（`scripts/e2e-device-m3.sh`）

`mat group list` の assert の直後、保留コメントを置き換えて:

1. `mat group remove --group $GROUP_ID --nodes $NODE_ID --endpoint $DEVICE_EP` →
   `status == "removed"`、`nodes[0]` の `acl_removed/group_removed/keymap_removed/keyset_removed`
   すべて true（python3 で assert）。
2. `mat group list` → `groups == []` かつ `keysets` に `keyset_id != 0` の要素が無い
   （keyset 0 = IPK は残る。docs/commands.md の出力例と `mat group remove` の
   「keyset 0 は絶対に外さない」ガードに従う。実際の空リスト形は実装時に確認して assert を
   合わせる）。
3. `mat group provision`（同じ引数）を撃ち直して `status == "provisioned"`、以降の
   groupcast / matd / listen 脚は現状のまま。

## 3. privacy フラグ（P ビット）対応

### 3.1 暗号ヘルパ `core/group_privacy.rs`（新規、I/O 無し、**依存追加なし**）

chip SDK 実装を GitHub で確認した事実（2026-09-05、master）:

- `Crypto::AES_CTR_crypt` は **`AES_CCM_encrypt`（AAD 無し）を呼んでタグを捨てる**だけ
  （"Discard tag portion of CCM to apply only CTR mode"）。つまり privacy の keystream は
  CCM の counter block `0x01 ‖ nonce(13) ‖ 0x0001` から始まる CTR そのもの。
- `Crypto::DeriveGroupPrivacyKey` = `HKDF-SHA256(ikm = operational group key, salt = 空,
  info = "PrivacyKey", L = 16)`。
- `CryptoContext::BuildPrivacyNonce` = `session_id` **big-endian** 2 バイト ‖ `mic[5..16]`
  （`kPrivacyNonceMicFragmentOffset = 5`, `kPrivacyNonceMicFragmentLength = 11`）。
- 難読化区間 = `PacketHeader::kPrivacyHeaderOffset = 4` から `PrivacyHeaderLength()` =
  4（message counter）+ 8（source があれば）+ 8 or 2（destination node / group）。
  Message Extensions は含まない。security flags（P ビット含む）は平文のまま AAD / nonce に使う。
- 送信側 `SessionManager::PrepareMessage` は group session で常に `kPrivacyFlag` を立てる。

したがって mat-controller の既存 `crypto::encrypt_payload(key, nonce, aad = &[], data)`
（AES-128-CCM、`MIC_LEN` = 16）の出力からタグ 16 バイトを落とせば SDK とバイト一致する
CTR になり、`aes`/`ctr` クレートの追加は不要。復号も同じ関数（CTR は対称）。

- `pub const PRIVACY_FLAG: u8 = 0x80`（group_rx.rs から移す。group_rx は re-export）。
- `pub fn derive_privacy_key(operational_key: &[u8; 16]) -> [u8; 16]` — 上記 HKDF
  （mat-device 既存依存の `hkdf`/`sha2` で実装）。
- `pub fn privacy_nonce(session_id: u16, mic: &[u8; 16]) -> [u8; 13]` — 上記レイアウト。
- `pub fn privacy_crypt(key: &[u8; 16], nonce: &[u8; 13], data: &mut [u8])` —
  `encrypt_payload(key, nonce, &[], data)` の先頭 `data.len()` バイトで上書き（タグは捨てる）。
  `encrypt_payload` の唯一の失敗はサイズ超過で、区間は最長 20 バイトなので到達不能
  （`expect` ではなく `unreachable` 扱いにせず、失敗時は data を変えない `debug_assert` 付き）。
- `pub fn deobfuscate_header(datagram: &[u8], operational_key: &[u8; 16]) -> Option<Vec<u8>>` —
  `MessageHeader::decode` で header 長 `payload_off` を得る（長さは flags バイトだけで決まり、
  値は難読化されていても影響しない）。`datagram.len() < payload_off + MIC_LEN` なら `None`。
  コピーを作り、区間 `[4, payload_off)` を `privacy_crypt(derive_privacy_key(op),
  privacy_nonce(session_id, 末尾 16 バイト))` で復号して返す。session id（bytes 1..3）・
  security flags（byte 3、P ビット含む）はそのまま。**P ビットは落とさない**。

### 3.2 `classify_group_datagram` の変更（`net/group_rx.rs`）

```
decode header (offset + session_id + security_flags)
session type != group → NotGroupSession
privacy = security_flags & PRIVACY_FLAG != 0
if !privacy: source / Destination::Group を wire header から先に検査（現行どおり NoSource / NotGroupDestination）
for ks in keysets (GKH 一致のみ, candidates += 1):
    operational = derive_ipk_operational(...)
    dg: Cow<[u8]> = if privacy { deobfuscate_header(buf, &operational)? (None → continue) } else { buf }
    header = MessageHeader::decode(&dg)
    source = header.source_node_id (無ければ continue)   ← privacy 時のみ到達し得る
    Destination::Group(gid) でなければ continue
    open_message(&operational, &dg, source) Ok → opened = (fabric, keyset, header, proto, payload); break
none → NoKeyset { candidates }
以降は現行どおり（NotMapped / NoMembers / Replay / NotInvoke / Malformed）。group_id・source は opened の header から取る。
```

- `GroupDrop::Privacy` は削除（到達不能になる）。P 付きで鍵が合わなければ `NoKeyset`。
- privacy key は候補ごとに HKDF 1 回（候補は通常 1 件。キャッシュは YAGNI）。
- 非 P の drop 理由の順序は不変（既存テスト `drops_are_classified` の期待を変えない。
  privacy ケースの期待だけ差し替える）。

### 3.3 テスト

- `core/group_privacy.rs` 単体: (a) `derive_privacy_key` は決定的で operational と異なる、
  (b) `privacy_nonce` のレイアウト（`0x1234` → `[0x12, 0x34, mic[5], …, mic[15]]`）、
  (c) `privacy_crypt` 2 回で元に戻る・鍵が違えば戻らない、(d) `deobfuscate_header` は短すぎる
  datagram で `None`、session id / security flags バイトを変えない。
- `group_rx.rs` tests: テスト側 sealer `privacy_datagram(fabric, epoch, counter, group)` =
  `MessageHeader{security_flags: 0x81, …}` + `ProtocolHeader`（InvokeRequest）+
  `im::encode_group_invoke_request` を `crypto::seal_message` で封じ → MIC = 末尾 16 バイト →
  `privacy_crypt` で `[4, payload_off)` を難読化。assert: (a) classify が `Ok` で source /
  group / invokes が平文版と一致、(b) 難読化前（P 立てたが CTR 未適用）は `NoKeyset`
  （復号後 header が壊れる）、(c) 別 epoch key の keyset だけの store は `NoKeyset{candidates:0}`
  （GKH 不一致）、(d) counter が同じ P datagram 2 通目は `Replay`。
- `tests/group_receive.rs`: 既存の閉ループ（provision → datagram → CASE read）に P ビット
  版 1 本追加 — 同じ関数で難読化した datagram を group socket へ送り toggle が適用される。

## 4. 削除した `[[device]]` の membership 残骸

- `GroupMembershipStore::retain_endpoints(&self, live: &[u16]) -> usize` — `live` に無い
  endpoint の行を全 fabric 横断で落とし、変化があれば save。落とした件数を返す。
- `Device::new`（`device.rs`）で ledger の `bridged_eps` 確定直後（`ledger.save()` の後、
  endpoint 登録の前）に `membership.retain_endpoints(&bridged_eps)` を呼び、`> 0` なら
  `tracing::info!(removed, "group membership: pruned rows for endpoints no longer in config")`。
- **決定（tombstone との整合）**: 同じ device id を再追加すると `EndpointLedger` は旧 endpoint
  を復元するが membership は復元しない → コントローラは `mat group provision --rebind`
  等で再登録が必要。これは「存在しない endpoint を GroupTable / join / dispatch に晒さない」
  を優先した裁定で、`device.rs` の該当箇所と `endpoint_ledger.rs` のモジュール doc に明記する。
- テスト: (a) store 単体（`retain_endpoints` が指定外だけ落とす・save される・無変化なら
  save しない）、(b) `device.rs` tests に「`groups.json` に EP2/EP3 の行を書き、`[[device]]`
  1 件（EP2）で `Device::new` → ファイルに EP2 行だけ残る」。

## 5. fail-safe rollback テストの assert 追加（`commissioning.rs` tests）

- `fail_safe_expiry_rolls_back_uncommitted_fabric`: `AclStore::new()` + `GroupMembershipStore::new()`
  も `set_acl_store` / `set_group_membership_store` で配線し、`install_fabric` 後に
  `acl.check(1, Subject::node(0xAA), PRIVILEGE_ADMINISTER, 0, CLUSTER_ACCESS_CONTROL)` が true
  （AddNOC の自動 admin エントリ）、`membership.add(1, 10, 2)` を入れておき、満了後に
  `check` false・`membership.groups_by_fabric()` 空を assert。
- 新規 `arm_fail_safe_zero_rolls_back_uncommitted_fabric_and_purges_stores`: 同じ仕込みで
  `ArmFailSafe(expiry=0)` を drive → fabric 0 件・3 store すべて purge 済み。
- 既存の「再アームは巻き戻さない」テストは、ついでに 3 store が**残る**ことも assert する
  （purge の誤発火防止）。

## 6. bitmap リプレイ窓（`GroupReplayGuard`）

spec §4.5.4.2 / SDK `PeerMessageCounter::VerifyOrTrustFirstGroup` と同じ:

- 状態: `(fabric, source) → { max: u32, window: u32 }`。`window` の bit `i`（0 始まり）=
  `max - (i + 1)` を受理済み。容量 64 エントリ・最古退去は現行のまま。
- `accept(fabric, source, c)`:
  - 未知の source → trust-first: `max = c, window = 0`、受理。
  - `c == max` → 拒否。
  - `d = c.wrapping_sub(max)`; `d < 2^31`（前方、rollover 込み）→ 受理。`window = if d >= 32
    { 0 } else { (window << d) | (1 << (d - 1)) }`（旧 max を bit d-1 に立てる）、`max = c`。
  - それ以外（後方）: `b = max.wrapping_sub(c)`; `b > 32` → 拒否（窓外）。`b <= 32` → bit
    `b - 1` が立っていれば拒否、立っていなければ立てて受理。
- `WINDOW_SIZE: u32 = 32` を pub const に。
- テスト: 順序逆転（10, 12, 11 → 全部受理・11 の再送は拒否）、窓外（max=100 で 67 は拒否、
  68 は受理）、前方ジャンプで窓クリア（10 → 10+40 で 10..=49 は全拒否）、rollover
  （max=0xFFFF_FFF0 → 5 は前方として受理、その後 0xFFFF_FFF0 は窓内で受理済み扱い→拒否）、
  容量退去は現行テストのまま。

## 7. ドキュメント

- `core/group_key_management.rs` モジュール doc: 「未実装（既知ギャップ）」の文を
  「KeySetRead/Remove/ReadAllIndices 実装済み、EpochKey は null で返す、IPK=keyset 0 は仮想」
  に更新。
- `net/group_rx.rs` モジュール doc: P ビット処理と bitmap 窓の記述。
- README.md の matv 段落末尾「privacy-flagged groupcast are not implemented yet」→ privacy
  対応済み・replay は 32 幅 bitmap・group 宛 Read/Write のみ未対応、に更新。
- `scripts/e2e-device-m3.sh` ヘッダコメントの flow 記述に remove → re-provision を追加。

## 8. 検証

- `task check`（fmt:check + clippy + test）。
- `task e2e:device:m1` / `task e2e:device:m3`（`MAT_E2E_IFACE` 既定 eth1、実機不使用）。
- 完了後: main へ rebase → no-ff マージ → push → メモリ `mat-code-audit-2026-08-31` に追記。

## 9. 実装順（plan の粒度の目安）

1. GroupKeyStore 拡張（epoch_start_time0 / remove_keyset / keyset_ids_for）
2. KeySetRead / KeySetRemove / KeySetReadAllIndices ハンドラ
3. e2e m3 の remove ステップ再有効化（1–2 の後、実行して PASS を確認）
4. `core/group_privacy.rs`
5. `classify_group_datagram` の P ビット対応 + tests + group_receive 閉ループ
6. membership `retain_endpoints` + Device 起動時 prune
7. rollback テスト assert 追加
8. bitmap リプレイ窓
9. docs（README / モジュール doc / e2e ヘッダ）+ 最終 task check / e2e
