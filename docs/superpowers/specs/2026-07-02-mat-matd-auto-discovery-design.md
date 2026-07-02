# mat の matd 自動発見（auto-discovery）設計

日付: 2026-07-02
ステータス: 承認済み（実装前）

## 背景 / 目的

現状、`mat` が matd（warm CASE セッションを持つ常駐デーモン）経由で実行されるのは
**明示有効化されたときだけ**（`--matd` フラグ or `MAT_MATD=truthy`）。matd が動いて
いるのに env / フラグを付け忘れると cold な直 chip-tool 経路に落ち、warm セッションの
恩恵を受けられない。毎回 `MAT_MATD_SOCKET=... MAT_MATD=1` を前置きするのは冗長で、
AI エージェントの利用でも間違えやすい。

そこで `mat` を**既定で matd を自動発見**するようにする: 既定ソケットパスへ接続を
試み、matd がいればそちら、いなければ従来どおり直 chip-tool にフォールバックする。

## 経路解決（3 状態）

| 状態 | トリガー | 挙動 |
|---|---|---|
| 強制 matd | `--matd` / `MAT_MATD=truthy`（`1`/`true`/`yes`/`on`） | 現行どおり。接続失敗はエラー、matd 非対応 op は exit 2。フォールバックしない |
| 強制直 | `MAT_MATD=falsy`（`0`/`false`/`no`/`off`） | 自動検出せず常に直 chip-tool |
| 自動（新規・既定） | どちらも未設定 | matd 対応 op なら既定ソケットへ connect 試行。成功 → matd 経路、失敗 → 直 chip-tool |

- ソケットパスの選択は現行どおり: `--matd <path>`（明示） > `MAT_MATD_SOCKET`（非空）>
  既定パス（`$XDG_RUNTIME_DIR/matd.sock`、無ければ `/tmp/matd.sock`）。自動検出も
  このパス解決を使う。`MAT_MATD_SOCKET` は引き続き**パス指定のみ**（単独では強制
  有効化しない — 自動モードの probe 先が変わるだけ）。
- `MAT_MATD` の falsy は既存の truthy 判定（`is_truthy`）と対になる `is_falsy` を
  追加して判定する。truthy でも falsy でもない値（例 `MAT_MATD=abc`）は未設定と
  同じく自動モード。

### 自動検出の判定 = connect 試行（存在チェックではない）

matd が SIGKILL 等で死ぬとソケットファイルが残る（stale socket）。ファイル存在
チェックだと死骸に引っかかり全コマンドが接続エラーになるため、判定は
`UnixStream::connect` の成否で行う:

- 成功 → matd 経路
- 失敗（ENOENT / ECONNREFUSED その他）→ 直 chip-tool にフォールバック

### matd 非対応 op の扱い

discover / commission / open-window / diag は matd 非対応。自動モードでは
**probe せず黙って直経路**で実行する。exit 2（unsupported）になるのは明示
`--matd` のときだけ（現行維持）。

## 二重実行の防止

probe と本送信の間に matd が落ちる隙間をなくすため、自動モードでは **connect した
stream をそのまま本リクエストに使う**（probe 後に接続を捨てて再接続、はしない）。

接続成功後のエラー（送受信失敗・matd エラー応答）は matd 経路のエラーとしてそのまま
返し、直経路での再実行は**しない**。write / invoke が二重に走るのを防ぐためで、
フォールバックが起きるのは「1 バイトも送る前」（connect 失敗）だけ。

実装上は `exchange(socket_path, op)` を「接続済み stream を受ける」形
（`exchange_on_stream(stream, op)` 等）に分離し、強制 matd 経路は従来どおり
パスから接続、自動経路は probe で得た stream を渡す。

## 可視性

どちらの経路で実行したかは stderr の構造化ログ（`tracing`）に出す:

- info: `using matd (auto-detected) socket=...`
- info: `matd not reachable, falling back to direct chip-tool socket=...`

既定フィルタは warn なので普段は無音。`MAT_LOG=info` で見える。stdout の JSON
スキーマ・exit code 表は両経路で共通（不変）。

## 互換性の注意

これは**既定挙動の変更**: matd が動いていれば、今まで直経路だったコマンドが matd
経由になる。応答スキーマ・exit code は両経路共通なので、observable な違いは速度
（warm セッション）のみ。従来挙動が必要なら `MAT_MATD=0`。

## テスト

- **単体（`matd_client`）**: 経路解決の純粋関数を 3 状態に拡張してテスト
  （env 注入スタイルは現行 `resolve_socket` テスト踏襲）。
  - 強制 matd / 強制直 / 自動の分岐
  - `MAT_MATD=abc`（truthy でも falsy でもない）→ 自動
  - 自動モードの probe 先パスが `MAT_MATD_SOCKET`（非空）> 既定パスの順で
    解決されること（明示パス `--matd <path>` は強制 matd になるので自動モードには
    存在しない）
- **統合（`crates/mat/tests`）**: tmp dir に実 UnixListener を立てて
  - matd 対応 op が自動検出で matd 経路に乗る（fake matd が応答を返す）
  - ソケット無し → 直経路（fake chip-tool 側が呼ばれる）
  - stale socket（bind 後 listener を drop したファイル）→ ECONNREFUSED →
    直経路に落ちる
  - `MAT_MATD=0` + matd 稼働 → 直経路
  - 非対応 op（discover 等）は matd 稼働中でも直経路

## ドキュメント

- README: matd 節を「既定で自動検出。`MAT_MATD=0` で無効化、`--matd` /
  `MAT_MATD=1` で強制」に更新。env 変数表（`MAT_MATD` / `MAT_MATD_SOCKET`）も同期。
- `--matd` の CLI ヘルプ文と `matd_client.rs` 冒頭のモジュールコメントを新しい
  3 状態の説明に更新。

## スコープ外

- matd の自動起動（mat が matd を spawn する）— しない。発見のみ。
- matd 側の変更 — 不要。プロトコル・ソケットパス規約は不変。
- `--no-matd` フラグ新設 — `MAT_MATD=0` で足りる（YAGNI）。
