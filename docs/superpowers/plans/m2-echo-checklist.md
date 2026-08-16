# M2 Echo attestation チェックリスト

Task 10（早期チェックポイント）と Task 16（最終受け入れ）で使い回す。Alexa アプリでの
commission 試行前後にこのファイルへ結果を追記していく。

## 前提

- matv は jarvis 上で手動起動（systemd 化は Phase D、ここでは対象外）。
- Echo 側で「認定されていないデバイス」警告が出ても続行できれば成功扱い。attestation
  ステップで硬く失敗した場合は spec の判断どおり **実装を止めて** ユーザーに報告し、
  Echo のファーム世代・別経路の調査へ切り替える。

## セットアップ記録（Task 10 実施分）

- **デプロイした binary の commit SHA**: `5ab8014497ff258f349f6bac4d5e8a894dff8627`
  （`dist:arm64` タスクへの matv 追加コミット自体は Taskfile/checklist のみでバイナリ内容に影響しないため、
  ビルド元ソースはこの SHA 時点のもの）
- **jarvis 上のパス**:
  - binary: `~/.local/bin/matv`（新規配置、上書き対象なし）
  - config/作業ディレクトリ: `~/matv-m2/`
  - store: `~/matv-m2/store/`
  - stdout log: `~/matv-m2/matv.stdout.log`
  - stderr log: `~/matv-m2/matv.stderr.log`
- **NIC**: `eth0`（`ip -6 addr show scope link` で確認した唯一の物理有線 NIC。他に
  `wpan0`・`tailscale0` があるが対象外）
- **matv.toml**:

  ```toml
  passcode = 74593218
  discriminator = 3210
  vendor_id = 0xFFF1
  product_id = 0x8000
  port = 5540
  store = "/home/jarvis/matv-m2/store"
  iface = "eth0"
  ```

  （passcode は spec §5.1.3.1 の well-known/trivial 値— 20202021 含む `INVALID_PASSCODES`
  ——を避けて選定）

- **QR payload**: `MT:Y.K90Q1212XCO93YL10`
- **manual code**: `31325045526`
- **QR 画像化コマンド**（ローカルに `qrencode` があれば）:

  ```bash
  qrencode -t ansiutf8 'MT:Y.K90Q1212XCO93YL10'
  ```

  作業マシン（このセッションの WSL 環境）には `qrencode` が **入っていない**
  （`command -v qrencode` が失敗）。**manual code の手入力（`31325045526`）が確実な代替手段。**
  Alexa アプリの「デバイスの追加」→「Matter デバイス」フローでは QR 読み取りの代わりに
  コード手入力を選べるはず — QR 画像化がすぐに用意できない場合はそちらを使う。

- **起動確認**:

  ```
  $ ssh jarvis 'pgrep -fa matv'
  771514 /home/jarvis/.local/bin/matv --config matv.toml
  ```

  stdout ログ 1 行目（JSON）:

  ```json
  {"manual_code":"31325045526","port":5540,"qr_payload":"MT:Y.K90Q1212XCO93YL10","store":"/home/jarvis/matv-m2/store"}
  ```

  stderr ログは起動時点で空（エラーなし）。

## Step 3: Alexa アプリでの commission 試行（人間チェックポイント・未実施）

- [ ] Alexa アプリ →「デバイスの追加」→「Matter デバイス」
- [ ] QR（`qrencode` 画像 or 上記コマンド出力）または manual code `31325045526` を入力
- [ ] 観測 (a): 「認定されていないデバイス」警告が出て続行できるか
- [ ] 観測 (b): attestation ステップで硬く失敗するか
- [ ] matv 側 stderr ログ（`ssh jarvis 'cat ~/matv-m2/matv.stderr.log'`）を試行前後で確認し、
      ここに貼り付ける

## Step 4: 結果と分岐（未実施）

- 結果: TBD
- 分岐: TBD（成功 → Phase B へ / attestation 拒否 → 実装を止めて情報収集に切り替え）

## Task 16（最終受け入れ）用メモ

- Task 16 では **この Task 10 の store をそのまま使わない**。Echo 側に古い登録が
  残っていればユーザーにアプリからの削除を依頼してから新 store で開始する。

## Task 10 結果（2026-08-16）: Echo attestation 早期チェック — 不通過【Amazon 側要因で確定】

- attempt 1-2（自己生成チェーン + 自作 CD）: 警告→同意→ AttestationResponse 受領後にクラウド側で停滞、fail-safe 満了で Echo が中断（GS014）
- attempt 3（観測強化後）: 同一パターンをログで確定。AttestationResponse は MRP ack 済み、以降 80 秒間 Echo からの着信ゼロ（pcap で裏取り）→ ArmFailSafe(0) の後始末のみ
- attempt 4（正典 chip テスト証明書チェーン PAA-FFF1→PAI→DAC）: 変化なし（GS014）
- attempt 5（CD の certificate_id="CSA00000SWC00000-00" / device_type_id=22 を matter.js に一致）: 変化なし（GS014）
- attempt 6【対照実験】: matter.js 公式サンプル（@matter/examples 0.15.6、Alexa ペアリング実績のある参照実装）を同一 jarvis/LAN/Echo で commission → **同一パターンで失敗**（attestationRequest 応答後 80 秒沈黙 → armFailSafe 後始末 →「Alexaをデバイスに接続できません」）

**結論**: 自作スタックの問題ではない。「認定されていないデバイス」への同意はローカル続行のみを解錠し、Amazon クラウド側の attestation 検証が未認証（テスト証明書）デバイスを通していない（このアカウント/リージョン/ファーム世代で）。参照実装も同様に落ちるため、デバイス側で回避できる差分は存在しない。

**申し送り**: M2 の Echo ゲート（Task 16）は Amazon 側の道（開発者コンソール登録・サポート問い合わせ・別ファーム世代の Echo での再試行等）が見つかるまで保留。残タスク（Subscribe/窓/salt）は chip-tool ゲートで完成させる方針をユーザーが承認済み。相互運用検証の代替として Apple Home / Google Home（自作証明書に寛容）が選択肢。
