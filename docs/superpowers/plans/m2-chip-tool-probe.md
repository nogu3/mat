# M2 ゲート1: chip-tool 調達 spike — 初回 commission 試行の観測

Task 1（`.superpowers/sdd/2026-08-16-mat-device-m2/task-1-brief.md`）の Step 1〜3 の記録。
Task 9（フィックスラウンド）の入力。

## Step 1: 候補イメージ

| 候補 | 結果 |
| --- | --- |
| (1) `ghcr.io/project-chip/chip-cert-bins:latest` | **NG**。`docker pull` は `denied`（匿名トークンでも `DENIED: invalid token` — パッケージが public リポジトリに紐付いていない/非公開設定）。 |
| (2a) Docker Hub `connectedhomeip/chip-cert-bins` | **NG（アーキ不一致）**。Docker Hub API でタグ一覧（96件）を確認したが全て `architecture: arm64` のみ。本ホストは x86_64（WSL2）のため不可。 |
| (2b) Docker Hub `atios/chip-tool` | **OK**。`docker pull atios/chip-tool` → `linux/amd64`, 277MB（展開後）。`docker inspect` で `Architecture: amd64` を確認。README:「Run the connectedhomeip chip-tool command directly from this Docker container」。`ENTRYPOINT ["chip-tool"]` のため呼び出し時に `chip-tool` を重ねて渡すとエラーになる（後述）。2年前の commit ベースの古いビルドだが、`pairing onnetwork-long` の引数仕様・`--storage-directory` フラグは brief のラッパー雛形と一致した。 |
| (3) connectedhomeip ローカルビルド | 未実施（(2b) で確定したため不要）。 |

確定イメージ: **`atios/chip-tool:latest`**（Docker Hub, x86_64/amd64）。

**digest ピン止め（fix round 1）**: レビュー指摘によりラッパーの既定イメージはタグ (`:latest`) ではなく本 spike で観測した digest に固定した。

```
$ docker inspect --format='{{index .RepoDigests 0}}' atios/chip-tool:latest
atios/chip-tool@sha256:b0f75334f7264af16c19ea0f4880a20ed86b821cd12c6a553c8e012aa0344277
```

`scripts/chip-tool.sh` の `IMAGE` 既定値はこの digest（`atios/chip-tool@sha256:b0f75334f7264af16c19ea0f4880a20ed86b821cd12c6a553c8e012aa0344277`）。`CHIP_TOOL_IMAGE` 環境変数での差し替えは引き続き可能。

### 検証コマンドの結果

- `docker run --rm --network host atios/chip-tool chip-tool --version` — **不可**。このイメージの `ENTRYPOINT` は既に `chip-tool` なので、引数に `chip-tool` を重ねると `Unknown cluster or command set: chip-tool` になる。正しい起動形は `docker run --rm --network host atios/chip-tool <args>`（`chip-tool` を省く）。
- また、このビルドの chip-tool バイナリは `--version` フラグ自体をサポートしていない（グローバルオプションとしての `--version` は存在せず、cluster/command-set 名として解釈されて usage が出るだけ）。**wrapper の疎通確認は `chip-tool.sh --version` ではなく「usage が表示されエラー落ち（docker/exec 起動失敗）しないこと」で行った**。
- `docker run --rm --network host atios/chip-tool discover commissionables` — **OK**。mDNS ソケットの bind/listen はエラーなく通り、LAN 上に commissionable device が無いため約30秒後に内部タイムアウトで `CHIP Error 0x00000032: Timeout` を出して exit 1。クラッシュや permission denied は無し → socket/mDNS 権限は問題なしと判断（brief の判定基準どおり）。

## Step 2: ラッパースクリプト

`scripts/chip-tool.sh` を作成。brief の雛形どおり `IMAGE`/`STORE` を解釈し `docker run --rm --network host` で実行するが、以下 2 点は実機検証結果に合わせて雛形から調整した:

1. `CHIP_TOOL_IMAGE` の既定値は `atios/chip-tool:latest`（Step 1 で確定）。
2. コマンド列に `chip-tool` を重ねない（`"$IMAGE" "$@" --storage-directory ...` であって `"$IMAGE" chip-tool "$@" ...` ではない）。理由は上記の ENTRYPOINT 二重呼び出しエラーのため。

`pairing onnetwork-long --help` 相当（`pairing onnetwork-long` を引数なしで実行し usage を出させる）で確認した結果、`--storage-directory` は末尾の位置引数オプションとして受理される（brief の注意書きどおり、イメージのバージョンで仕様が変わりうる点は当該イメージでは雛形のまま通った）。

`task chip-tool -- pairing --help` で Taskfile 経由の呼び出しも確認済み。

## Step 3: matv に対する初回 commission 試行

### 準備

- ビルド: `cargo build --release -p matv -p mat`（release、約25秒で完了）。
- iface: `eth1`（`ip link` で確認。UP かつ IPv6 リンクローカルあり。`lo` は不適、`e2e-device-m1.sh` と同じ選定）。
- matv 設定（`.e2e-cache/chip-tool-probe/matv.toml`。**ラッパーが `$PWD` を `/workdir` にマウントする仕様上、matv の store はリポジトリ配下に置く必要がある** — `/tmp` 配下だとコンテナから見えず `--paa-trust-store-path` が解決できない。詳細は「つまずき」節）:
  ```toml
  passcode = 20202021
  discriminator = 3840
  vendor_id = 0xFFF1
  product_id = 0x8000
  port = 0
  store = ".e2e-cache/chip-tool-probe/device-store"
  iface = "eth1"
  ```
- 起動: `RUST_LOG=info ./target/release/matv --config matv.toml`。stdout 1行 JSON:
  ```json
  {"manual_code":"34970112332","port":47977,"qr_payload":"MT:Y.K90AFN00KA0648G00","store":".e2e-cache/chip-tool-probe/device-store"}
  ```
  `store/paa/paa.der` が生成されることを確認。

### 実行コマンド

```bash
CHIP_TOOL_STORE=/tmp/chip-tool-store-test2 scripts/chip-tool.sh \
  pairing onnetwork-long 1 20202021 3840 \
  --paa-trust-store-path .e2e-cache/chip-tool-probe/device-store/paa
```
（passcode=20202021, discriminator=3840 は matv.toml で自分が設定した値をそのまま使用。matv の stdout JSON には passcode/discriminator の生値は出ないため — 代わりに qr_payload/manual_code が入る）

### 結果: mDNS 発見 → PBKDF 交換で失敗（Timeout）。exit 1

**ここまで到達（成功箇所）**:
- chip-tool がフル起動し RCAC/ICAC/NOC を生成、fabric に参加。
- **matv を実 mDNS で発見**: `CHIP:TOO: Discovered Device: fe80::2351:9165:67de:87b4:47977`
- matv の UDP エンドポイントへ実際に PBKDFParamRequest を送信:
  ```
  CHIP:EM: <<< [E:47271i S:0 M:217627266] (U) Msg TX to 0:0000000000000000 [0000] --- Type 0000:20 (SecureChannel:PBKDFParamRequest)
  CHIP:IN: (U) Sending msg 217627266 to IP address 'UDP:[fe80::2351:9165:67de:87b4%eth1]:47977'
  ```
  → **ここまでで brief の目的「WSL2 上で chip-tool が実 mDNS を引けて matv に UDP が通る」は達成**。

**この先で失敗**:
```
CHIP:IN: Received malformed unsecure packet with source 0x0000000000000000 destination 0x0000000000000000
```
が PBKDFParamRequest 送信直後から繰り返し発生（4回のリトライすべてで直後に出る）。matv から何らかの UDP 応答は返ってきているが、chip-tool 側でパケットのパース段階（送信元/宛先の node id が 0 = unsecure packet ヘッダの解釈失敗）で弾かれている。

```
CHIP:EM: Failed to Send CHIP MessageCounter:217627266 on exchange 47271i sendCount: 4 max retries: 4
CHIP:SC: PASESession timed out while waiting for a response from the peer. Expected message type was 33
CHIP:TOO: Secure Pairing Failed
CHIP:TOO: Pairing Failure: src/protocols/secure_channel/PASESession.cpp:255: CHIP Error 0x00000032: Timeout
CHIP:TOO: Run command failure: src/protocols/secure_channel/PASESession.cpp:255: CHIP Error 0x00000032: Timeout
```
exit code 1。

**matv 側ログ**: `RUST_LOG=info` で起動したが stderr は空のまま（PBKDFParamRequest 受信・応答送信を示すログが一切出ない）。matv がリクエストをどう処理したか（受理して応答を返したのか、そもそも黙って捨てたのか）はこのログからは判別できない。tcpdump 等でパケットキャプチャしないと切り分けできない — Task 9 側で要調査。

### Task 9 への申し送り

1. **本命**: matv → chip-tool 方向の PBKDFParamResponse（またはそれ以前の応答パケット）のワイヤーフォーマットが chip-tool（本家 SDK 実装）の unsecure packet ヘッダ解釈と食い違っている可能性が高い（"malformed unsecure packet" が PBKDFParamRequest 送信直後に毎回出ている = matv からの応答自体が届いているが解釈できていないと推測される。ただし matv 側の info ログが空なので、matv が実際に何か送り返しているかはパケットキャプチャで要確認）。
2. matv の info レベルログが PASE/PBKDF のやり取りで何も出力しない。デバッグ性のため、Task 9 でリクエスト受信・応答送信のログ追加を検討する価値あり（本 spike のスコープ外につき未対応）。
3. 再現手順は上記「準備」「実行コマンド」節のとおり。再現用の一時ディレクトリ（`.e2e-cache/chip-tool-probe/`）は `.gitignore` 済みのためリポジトリには残していない。

### つまずき: `scripts/chip-tool.sh` の `$PWD` マウント境界

初回は matv の store を `/tmp/claude-.../scratchpad/...` に置いて実行したところ、`--paa-trust-store-path <その絶対パス>/paa` を渡しても chip-tool 側で
```
CHIP:TOO: No PAAs found in path: /tmp/.../device-store/paa
CHIP:SPT: VerifyOrDie failure at src/lib/support/Pool.h:337: Allocated() == 0
```
となり **chip-tool プロセスがクラッシュ（exit 139 = SIGSEGV）** した。原因は wrapper が `-v "$PWD":/workdir -w /workdir` だけをマウントしており、`$PWD` 配下以外のホストパスはコンテナから見えないため。`--paa-trust-store-path` にホスト絶対パスをそのまま渡すのは不可 — **リポジトリ配下の相対パス（`.e2e-cache/` 等）を使う必要がある**。この制約はラッパーの仕様（Step 2, brief どおり）から来るものなので wrapper 自体は変更せず、本ノートと今後の e2e スクリプト（Task 9/13）向けの申し送りとして記録する。

---

# Task 9: フィックスラウンド — chip-tool commission を通す

Task 9（`.superpowers/sdd/2026-08-16-mat-device-m2/task-9-brief.md`）の記録。
Task 1 の spike から Tasks 2〜8 が入った状態で再観測し、落ちるたびに直した。

環境は Task 1 と同じ（`atios/chip-tool@sha256:b0f7...`、iface `eth1`、
matv は `.e2e-cache/` 配下の store、`--paa-trust-store-path` はリポジトリ相対）。

## デバッグ性の改善（前提整備）

Task 1 の申し送り 2（「matv の info ログが PASE/PBKDF で何も出ない」）に対応し、
`net::runtime` に debug レベルのログを入れた（コミット `dc0ccfe`）。unsecured
データグラムの分類結果・PASE/CASE の確立と失敗・secured データグラムのドロップ
理由・IM の invoke/read の中身が `RUST_LOG=debug` で読める。以降の観測はすべて
このログと chip-tool のログを突き合わせて行った。

## 観測 → 仮説 → 修正 → 結果

### 1. PBKDFParamResponse が chip に届いても捨てられる（Task 1 の本命の答え）

**観測（再現）**: Task 1 とまったく同じ。`Received malformed unsecure packet with
source 0x0000000000000000 destination 0x0000000000000000` が PBKDFParamRequest
送信直後に毎回 3 回（standalone ack + 応答 + 再送）出て、4 回リトライして
`PASESession timed out`。

**仮説**: chip の `SessionManager::UnauthenticatedMessageDispatch` は

```cpp
if ((source.HasValue() && destination.HasValue()) || (!source.HasValue() && !destination.HasValue()))
{
    ChipLogError(Inet, "Received malformed unsecure packet with source 0x" ... " destination 0x" ...);
    return;
}
```

で、unsecured セッションのメッセージは source node id と destination node id の
**ちょうど片方だけ**を持たなければならない（spec §4.4.1.2 / §4.6.1.5: initiator の
ephemeral node id を、initiator は source に、responder は destination に載せる）。
ログの `source 0x0 destination 0x0` は「両方無い」を意味する。
`ResponderExchange::build` は実際に両方 `None` を出していた。

**修正**（`962cc80`）: `ResponderExchange::adopt` で initiator の source node id を
控え、応答（standalone ack を含む）の destination に載せる。TDD: `exchange.rs` に
「応答の destination が initiator の ephemeral node id、source は無し」を検査する
テスト 2 本（`reply_reliable` 経路と `screen` の standalone ack 経路）。

**結果**: PASE が通った。`Pairing Success` → `ReadCommissioningInfo` 成功
（Task 4/5 の wildcard read とグローバル属性がそのまま効いた）→ 次の壁へ。

### 2. ArmFailSafe の応答が `End of TLV` で拒否される（2 段構え）

**観測**: `Error on commissioning step 'ArmFailSafe': CHIP Error 0x00000021: End of TLV`。

**仮説 2a**: `ArmFailSafeResponse` の `DebugText`（タグ 1）は spec §11.10.6.3/.5/.7 で
mandatory。`encode_commissioning_status_response` は空文字列のときタグごと省いていた。

**修正 2a**（`6101773`）: 空でも必ず書く。TDD: 自前 decoder は欠落を許すので往復
テストでは差が出ない → ワイヤ形状（タグ 1 の存在）を直接検査するテスト。

**結果 2a**: **変わらず `End of TLV`**。ただし chip の pretty print には
`0x0 = 0, 0x1 = "" (0 chars)` が出るようになり、CommandFields までは正しく
読めていることが確定した（＝原因はもう 1 段外側）。

**仮説 2b**: chip の `CommandSender::ProcessInvokeResponse` は
`invokeResponseMessage.GetSuppressResponse(&suppressResponse)` を必ず呼ぶ。
`SuppressResponse`（タグ 0, bool）は InvokeResponseMessage の mandatory
フィールド（spec §8.9.4）で、無ければ `FindElementWithTag` が
`CHIP_END_OF_TLV`（= 0x21）を返す。`encode_invoke_response_status` /
`encode_invoke_response_data` はどちらも省いていた。

**修正 2b**（`c5477c6`）: 両 encoder が先頭に `SuppressResponse=false` を書く。
TDD: 同じくワイヤ形状（先頭要素が `Context(0)=Bool(false)`）の直接検査。

**結果**: ArmFailSafe / ConfigRegulatory / PAI・DAC の CertificateChainRequest /
AttestationRequest まで一気に通った。次は attestation の検証段。

**教訓**: 自前の decoder が寛容（未知タグを読み飛ばす・欠落を既定値で埋める）だと、
encoder 側の必須フィールド欠落は往復テストで永遠に検出できない。相互運用で効く
形は「ワイヤ形状を直接検査するテスト」でしか固定できない。以降の修正はすべて
この形でテストしている。

### 3. attestation 証明書の basicConstraints が spec 形でない

**観測**: `Failed in verifying 'Attestation Information' command received from the
device: err 203`。`AttestationVerificationResult` の 203 = `kPaiFormatInvalid`
（v1.2-branch の `DeviceAttestationVerifier.h` で確認）。

**仮説**: 203 の発生箇所は 3 つ（`VerifyAttestationCertificateFormat` /
`ExtractVIDPIDFromX509Cert` / `ExtractAKIDFromX509Cert`）。生成した PAI を openssl
で見ると VID/PID の RDN も AKID も揃っている一方、basicConstraints が
`CA:TRUE`（pathLenConstraint なし）だった。chip の
`VerifyAttestationCertificateFormat`（`src/crypto/CHIPCryptoPALOpenSSL.cpp`）は
役割ごとに厳密で:

| 役割 | cA | pathLenConstraint | 拡張の要否 |
| --- | --- | --- | --- |
| PAA | TRUE | 省略 または 1 | 必須 |
| PAI | TRUE | **0** | 必須 |
| DAC | FALSE | 省略 | **必須**（我々は拡張ごと無かった） |

**修正**（`ef11e46`）: `make_test_cert_ext` の `bool` 引数を `BasicConstraints`
enum（`Absent` / `EndEntity` / `Ca { path_len }`）に置き換え、
`generate_dev_attestation` は役割別の形を明示指定する。既存の attestation
フィクスチャの意味（`is_ca=false` は拡張なし）は据え置き。TDD: 拡張値の DER
バイト列を役割ごとに直接検査。

**結果**: 203 が消え、**err 600** に進んだ。

### 4. Certification Declaration が無い（本タスク最大の判断）

**観測**: `err 600` = `kCertificationDeclarationNoKeyId`。あわせて chip のログに
`AutoCommissioner setting attestationElements buffer size 53/53` — 53 バイトは
nonce+timestamp だけで、CD が入っていない。M1 は `b"mat-dev-cd"` という
プレースホルダを載せていた（自前コミッショナの `verify_cd_warn` が warn のみで
通すので M1 では露見しなかった）。

**選択肢**:

| 案 | 内容 | 判断 |
| --- | --- | --- |
| A | chip-tool の `--bypass-attestation-verifier true` で検証を飛ばす | **不採用** |
| B | 自前の CD 署名証明書を作り `--cd-trust-store-path` で信頼させる | 不採用 |
| C | 公開されている Matter テスト CD 署名鍵で本物の CD を作る | **採用** |

- A はゲートを弱める。かつ Task 10（Echo での attestation 早期チェックポイント）は
  bypass できないので、そこで同じ壁に当たるだけ。人間チェックポイントを無駄にする。
- B は chip-tool は通るが Echo では確実に落ちる（Echo に信頼ストアを渡せない）。
- C の鍵は connectedhomeip に平文で公開されている「Matter Test CD Signing
  Authority」（`credentials/test/certification-declaration/Chip-Test-CD-Signing-Key.pem`）。
  chip SDK の**全ビルドがこの公開鍵を既定で CD 信頼ストアに内蔵**しており
  （`gTestCdPubkeyKid` = `62FA8233...`）、`--only-allow-trusted-cd-keys` を明示的に
  立てた場合のみ拒否される。esp-matter などの開発用デバイスや chip の example app が
  使っているのと同じ「未認証デバイスの標準形」で、spec が Task 10 の前提として
  挙げている「HA Matter Hub が test 証明書で Alexa ペアリングできている実績」と
  同じ土俵に乗る。**CSA 認証の代わりではない**点は PAA/PAI/DAC と同じ。

**修正**（`a014bb0`）: `mat-controller` に `cd` モジュールを新設。spec §6.3.1 の
`cd-struct` TLV（タグ昇順 — chip の `DecodeCertificationElements` は順番に読む）と、
chip の `Credentials::CMS_Sign` と同形の CMS SignedData を組む。署名対象は
CD 本体の生バイト列（signedAttrs なし）、SignerInfo の sid は
`[0] IMPLICIT OCTET STRING` の key id。`DevAttestation` に
`certification_declaration` が増え、`generate_dev_attestation` は CD に載せる
device type を第 3 引数で受け取る。TDD: CMS 封筒を chip と同じ順にたどって
(a) key id が取り出せる (b) eContent が元の CD (c) 署名がテスト鍵の**公開鍵**で
検証できる、を検査。

**結果**: `Successfully validated 'Attestation Information'` →
CSR → NOC → CASE → `Device commissioning completed with success`。**commission 成功**。

## 到達点

`scripts/e2e-device-m2-chip.sh`（`task e2e:device:m2-chip`）で自動検証:

1. matv 起動 → `chip-tool pairing onnetwork-long` が
   `Device commissioning completed with success` を出す
2. `onoff read on-off` → `onoff toggle` → `onoff read` が実際に反転する
3. matv を SIGTERM → **同じ store で再起動**（`port = 0` なので UDP ポートは変わる）
   → 再ペアリング無しで read/toggle/read がまた通る（chip-tool が operational
   mDNS で新しいポートを引き直し、CASE を張り直している）

3 回連続グリーン。`task e2e:device:m1` も引き続きグリーン。

## 副次的な観測（Echo ゲート＝Task 13 以降への申し送り）

- **CASE resumption は毎回 full fallback している**。chip-tool は 1 コマンド 1
  プロセスなので毎回 Sigma1 から張り直すのが自然だが、matv 側のログでも
  resumption 経路に入った形跡はない（Task 3 の resumptionID は Sigma2 TBE に
  載っているだけで、chip 側が使ってこない）。Echo のような常駐コントローラでは
  resumption を試してくる可能性があり、そこが初めての実地検証になる。
- **`DataVersion` が常に 1 のまま**。`onoff toggle` で属性値が変わっても
  chip-tool の read は `DataVersion: 1` を返す。read だけなら問題ないが、
  chip の subscription は DataVersion で dirty 判定をするので、Echo の
  Subscribe（Task 11 以降）で「値が変わったのに通知が飛ばない／
  data-version-filter で握りつぶされる」形で顕在化しうる。
- **OnOff の状態は再起動で消える**（永続化しているのは fabric だけ）。e2e は
  再起動後にもう一度 baseline read を取ってから toggle するので影響を受けないが、
  実運用のデバイスとしては要検討。
- **並列 exchange で刺さる兆候は無かった**。commissioning 中も chip-tool は
  exchange を 1 本ずつ進めており、逐次デバイス（同時 1 セッション）の設計で
  commission は完走した。head-of-line blocking の解消はスコープ外のまま。
