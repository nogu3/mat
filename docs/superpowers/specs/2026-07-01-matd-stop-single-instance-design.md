# 設計: `matd stop` コマンド + 二重起動ガード

- 日付: 2026-07-01
- 対象: `crates/matd/src/main.rs`, `crates/matd/src/server.rs`,
  `crates/matd/src/protocol.rs`, `crates/matd/src/lock.rs`（新規）,
  `crates/matd/Cargo.toml`, `README.md`
- 関連メモリ: `matd-port9100-orphan`（matd を `kill` すると子 chip-tool が孤児化し
  port 9100 を掴みっぱなしになる）

## 背景

現状の `matd` はフラグのみの CLI で、起動すると serve ループに入り **Ctrl-C でしか**
止まらない（`server.rs:52-69`）。Ctrl-C 経路の graceful shutdown は reaper を止め、
`backend.shutdown()` で chip-tool 子プロセスを畳み、socket ファイルを消す
（`server.rs:71-75`）— つまり後始末は既に正しい。問題は 2 点:

1. **停止手段が Ctrl-C しか無い。** バックグラウンド（`matd &`）や別セッションで
   起動した matd を止めるには `kill` するしかなく、`kill` は Ctrl-C 経路の後始末を
   通らない。結果、子 chip-tool が孤児化し port 9100 を掴んだままになる
   （メモリ `matd-port9100-orphan` の罠）。
2. **二重起動を防いでいない。** `serve()` は既存 socket を削除してから bind する
   （`server.rs:31-35`）。2 個目の matd が 1 個目の socket を黙って奪い、さらに 2 個目の
   chip-tool が port 9100 を奪い合う。

本設計は「別プロセスから graceful shutdown を発火する `matd stop`」と「flock による
単一インスタンス保証」を追加し、両方を解消する。

## 設計判断（確定済み）

- **停止機構: socket 経由の shutdown op。** 既存の unix socket に新 op を送り、serve
  ループを break させて**既存の** graceful shutdown 経路を再利用する。PID ファイル +
  SIGTERM 案は stale PID 管理が増えるため不採用。
- **二重起動防止: flock ロックファイル。** 起動時に排他 advisory ロックを取得。
  kill/crash 含むあらゆる終了で OS が自動解放するため stale 状態が残らない。socket
  ping 案はレース／生存 matd と孤児 socket の区別が弱いため不採用。

## コンポーネント

### 1. CLI の形（`main.rs`）

オプショナルなサブコマンドを追加する。素の `matd` は今まで通り serve を起動し、
README の `matd &` を壊さない。

```
matd [--store --socket --port --connect --idle-timeout]   # serve（既定・従来通り）
matd stop [--socket]                                        # 稼働中デーモンを停止
```

- `--socket` は serve / stop 両方が使う（stop は「どの socket の matd を止めるか」に
  使う）。他フラグ（`--store` / `--port` / `--connect` / `--idle-timeout`）は serve
  専用で、stop では無視される（トップレベルに置いたまま stop 経路が参照しないだけ）。
- `command: Option<Command>` を追加。`None` → serve、`Some(Command::Stop)` → stop。
- serve 側の socket 解決（`main.rs:89-91`、`--socket` 省略時 `default_socket_path`）は
  stop 側でも同じ関数で行い、既定パスを一致させる。

### 2. 単一インスタンスガード（`lock.rs` 新規）

`rustix`（`fs` feature）で `flock` を安全に呼ぶ（`unsafe` 無し。`rustix` は既に依存
ツリーにある）。

- ロックファイルパス: 解決済み socket パス + `.lock`
  （例 `$XDG_RUNTIME_DIR/matd.sock.lock`）。
- serve パスで、**chip-tool 起動・socket bind より前**に
  `flock(fd, FlockOperation::NonBlockingLockExclusive)` を取得する。取得順を早めるのが
  肝: 二重起動時に port 9100 を奪い合う 2 個目の chip-tool を起こさず即失敗させる。
- 取得した `File` はプロセス生存中ずっと保持する（`run` まで持ち上げてスコープに残し、
  Drop されないようにする）。
- ロック競合（`WouldBlock` 相当）→ `MatError::new(ErrorKind::Other, "matd already
  running (lock held at <path>)")` を返し exit 1。ロックファイルを開く／作る I/O の
  失敗も `ErrorKind::Other` にマップ。
- ロックは kill/crash 含むあらゆる終了で OS が自動解放する。ロックファイル自体は
  graceful shutdown 時に削除を試みる（残っていても次回 flock で再利用できるので必須
  ではない — best-effort）。

インターフェース案:

```rust
/// 単一インスタンスロックを取得する。既に別 matd が保持していれば Err。
/// 返す File はプロセス生存中保持する（Drop でロック解放）。
pub fn acquire(socket_path: &Path) -> Result<File, MatError>;

/// ロックファイルパス（socket_path + ".lock"）。
pub fn lock_path(socket_path: &Path) -> PathBuf;
```

### 3. shutdown op（`protocol.rs` + `server.rs`）

- `protocol.rs`: `Op::Shutdown` を追加。`node_id()` は `None`、`to_cmdline()` は
  `None`（Ping と同じ扱い）。doc コメントで「chip-tool には触れない admin op」と明記。
- `server.rs::serve`: `Arc<tokio::sync::Notify>` を作り、各接続ハンドラ
  （`handle_conn` の spawn）にクローンを渡す。`tokio::select!` に既存の accept /
  `ctrl_c` と並べて `_ = shutdown.notified() => break` を追加。break 後は既存の
  graceful shutdown（reaper.abort → backend.shutdown → socket 削除）へそのまま落ちる。
  加えてロックファイルを best-effort で削除。
- `run_op`: `Op::Shutdown` → `Ok(json!({ "stopping": true }))`。timestamp は既存の
  dispatch が付与する。
- **応答をワイヤに出してから停止する**（順序保証）: `handle_conn` が shutdown op を
  検出したら、応答を `write_all` + **`flush().await`** した**後**に notify を発火して
  ループを抜ける。これにより serve ループの break（→ プロセス終了）より前に応答
  バイトがクライアントへ渡る。検出は `dispatch` が `(Value, is_shutdown: bool)` を
  返す形にして `handle_conn` へ伝える（op を二重パースしない）。

### 4. `matd stop` クライアント（`main.rs`）

serve と同じ tokio ランタイム内で動く小ヘルパー。

- tokio `UnixStream::connect(socket)` → `{"op":"shutdown"}\n` を送信 → 応答 1 行を読む
  → stdout へ出力（成功時 `{"stopping":true,...}`）。
- 接続不能（`NotFound` / `ConnectionRefused`）→ 「matd は動いていない」とみなし、
  stderr に `{"error":{"kind":"other","detail":"matd not running at <socket>"}}` を出し
  exit 1。stale な socket ファイルが残っていれば best-effort で削除。
- shutdown は `matd stop` 専用の admin op。`mat --matd` の `to_op`（`matd_client.rs`）
  には**追加しない**（公開しない）。

## データフロー

```
$ matd &                       # serve: flock 取得 → chip-tool 起動 → socket bind → accept ループ
$ matd stop                    # connect → {"op":"shutdown"} 送信
   matd: handle_conn 受信 → {"stopping":true} を write+flush → notify 発火
   matd: serve ループ break → reaper.abort → backend.shutdown（子 chip-tool kill）
         → socket 削除 → lock 削除 → run() return → プロセス終了（flock 解放）
   matd stop: {"stopping":true} を stdout に出力し exit 0
```

二重起動:

```
$ matd &                       # 1個目: flock 取得成功 → serve
$ matd                         # 2個目: flock 取得失敗
   → stderr {"error":{"kind":"other","detail":"matd already running (lock held at ...)"}}
   → exit 1（chip-tool は起動しない）
```

## エラーハンドリング

| 事象 | kind | exit | 備考 |
|---|---|---|---|
| 二重起動（ロック競合） | `other` | 1 | chip-tool 未起動で即失敗 |
| ロックファイル I/O 失敗 | `other` | 1 | open/create 失敗 |
| `matd stop` で接続不能 | `other` | 1 | 「動いていない」。stale socket は掃除 |
| shutdown 応答受信 | — | 0 | 正常停止 |

`matd stop` を何も動いていない状態で叩いたときは exit 1 + 明確な "not running"
エラー（mat の構造化エラー規約に一致。冪等 exit 0 は採らない）。

## テスト

- `protocol` ユニット: `Op::Shutdown` がパースでき、`node_id()`/`to_cmdline()` が
  ともに `None`。
- `server` / integration: fake ws を立てて serve を起動し、socket に
  `{"op":"shutdown"}` を送ると `{"stopping":true}` が返り、その後サーバが accept を
  止める（後続 connect が失敗 or サーバタスクが終了）ことを検証。既存の
  `crates/matd/tests/integration.rs` の fake ws ハーネスを再利用。
- `lock` ユニット/integration: 一時ファイルに `acquire` した後、同じパスへの 2 度目の
  `acquire` が `Err`（`ErrorKind::Other`）になること。1 個目の `File` を drop すると
  2 度目が成功すること。

## ドキュメント

- README: `matd stop` の使い方と単一インスタンス動作（`matd already running`）を追記。
  「停止は `matd stop`（`kill` ではなく）」を推奨として明記し、port 9100 孤児の
  ワークアラウンドを更新。
- メモリ `matd-port9100-orphan` に「`matd stop` で graceful に止めれば孤児化しない」を
  追記（実装完了後）。

## スコープ外

- `matd status` / `matd restart` 等の追加サブコマンド（YAGNI）。
- SIGTERM ハンドラ（socket 経由 shutdown で足りる。将来 systemd 統合が要るなら別途）。
- `mat --matd` からの shutdown 公開（admin 操作は `matd` バイナリに閉じる）。
