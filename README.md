# mat

Matter デバイス操作 CLI。Matter コントローラ（`chip-tool`）をサブプロセスで呼び、その冗長なテキスト出力を `mat` のスキーマに正規化した **純粋な構造化 JSON** として返す。`enl`（ECHONET Lite）の兄弟 CLI。

- stdout = 1コマンド1 JSON オブジェクト。人間装飾は混ぜない。
- 診断は stderr に構造化ログ（`tracing`）。
- 認証情報 KVS 以外の状態を持たない（プロセスはワンショット）。

設計の背景・三層分離（`casa`/`casad`）・非責務は [CLAUDE.md](./CLAUDE.md) を参照。

## ステータス

**Phase 0 実装済み**: 雛形 + chip-tool ラッパ基盤 + commission + 認証情報 KVS + discover。
read/write/invoke・describe・on/off・open-window・group は後続フェーズ（CLAUDE.md のロードマップ参照）。

## コマンド（Phase 0）

```bash
# commissionable / commissioned ノードを探索
mat discover

# fabric への参加（初回 commission / multi-admin join 両対応）
# 値はすべてダミー（RFC 5737 192.0.2.0/24）
mat commission 192.0.2.10 "MT:Y.K9042C00KA0648G00" --node-id 5
```

`discover` 出力例:

```json
{
  "timestamp": "2026-06-06T12:34:56+09:00",
  "devices": [
    { "state": "commissionable", "hostname": "B827EBA8C9F0", "addresses": ["192.0.2.10"], "port": 5540, "discriminator": 3840, "vendor_id": 65521, "product_id": 32769 },
    { "state": "commissioned", "node_id": 5, "address": "192.0.2.10", "commissioned_at": "2026-06-06T12:00:00+09:00" }
  ]
}
```

`commission` 出力例:

```json
{ "timestamp": "2026-06-06T12:34:56+09:00", "node_id": 5, "status": "success" }
```

## 認証情報ストア

配置の優先順位: `--store <path>` > `$MAT_STORE` > `$XDG_CONFIG_HOME/mat` > `~/.config/mat`。
Root CA・controller 鍵/証明書・commission 済みノードの台帳（`nodes.json`）・`chip-tool` の永続ストレージを格納する。**リポジトリには含めない**（`.gitignore` で除外）。

## エラーと exit code

エラーは stderr に `{"error":{"kind":"...","detail":"..."}}` で出る。

| code | 意味 |
|---|---|
| 0 | 成功 |
| 2 | CLI 引数エラー（clap 既定） |
| 10 | 認証情報ストアが無い / パース失敗 |
| 11 | node_id が未 commission |
| 12 | `chip-tool` が見つからない / 実行不可 |
| 3 | timeout |
| 4 | device rejected |
| 5 | unreachable / network |
| 1 | その他 |

`chip-tool` は失敗時の exit code が粗い（おおむね `1`）。`mat` が stdout/stderr をパースして `3`/`4`/`5` に分類する。分類できなければ exit `1`。

## バックエンド（chip-tool）

ローカル実行は `chip-tool` を PATH 上に置く。フルパス上書きは `MAT_CHIP_TOOL_BIN`。
`chip-tool` 自体のビルドは重いので、x86 UGREEN 向けには Docker イメージに同梱する（[Dockerfile](./Dockerfile)）。

> Matter は mDNS / IPv6 マルチキャストを使うため、Docker 実行は **host networking 必須**（`docker run --network host`）。bridge では応答を受けられない。

## 開発

[Task](https://taskfile.dev) でタスク定義（`task` で一覧）。

```bash
task build            # リリースビルド → target/release/mat
task install          # ~/.cargo/bin にインストール
task run -- discover  # 実行（chip-tool が PATH 上に必要）
task test             # テスト（ダミー chip-tool 統合テスト含む。実 chip-tool 不要）
task clippy           # Lint（-D warnings）
task fmt              # 整形
task check            # CI 相当（fmt:check + clippy + test）

task docker:build     # x86 UGREEN 向けイメージ（chip-tool 同梱）
task docker:run -- discover
task docker:test      # ローカルツールチェーン不要
```

CI は実 `chip-tool` 不要。`tests/fixtures/fake-chip-tool.sh`（固定テキストを吐くダミー）を `MAT_CHIP_TOOL_BIN` で差して統合テストを回す。

## 実機 E2E（手動・CI 非対象）

現実の主経路は **multi-admin join**（既に Home Assistant 等に commission 済みのデバイスを `mat` にも足す）。印刷コードは使えない（commissioning モードを抜けているため）ので、既存 admin 側で commissioning window を開いて一回限りのコードを発行する。

1. **HA 側で共有**: Home Assistant の対象デバイスで「Matter で共有 / Share」を実行し、発行される setup code（`MT:...` または 11桁）を控える。
2. **`mat` で join**:
   ```bash
   mat commission <device-ip-or-host> "<発行された setup code>" --node-id 5
   ```
   `{ "node_id": 5, "status": "success" }` が返り、`~/.config/mat/nodes.json` に台帳が記録される。
3. **確認**: `mat discover` の `devices` に `"state": "commissioned"` の node 5 が現れる。

> 工場出荷/リセット直後のデバイスなら、印刷された setup code をそのまま `commission` に渡せる（初回 commission）。
