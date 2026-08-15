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
