# matd socket 既定パスの subdir 化 + mat の候補探索（0.27.0）

日付: 2026-07-21 / 対象: `mat-core::socket` / `mat::matd_client` / `matd`（bind）

## 問題

mat/matd の既定 socket パスは `$XDG_RUNTIME_DIR/matd.sock`（flat、XDG 不在時
`/tmp/matd.sock`）の一本。一方、本番 jarvis の systemd user unit は
`RuntimeDirectory=matd` 慣習により `--socket %t/matd/matd.sock`（subdir）へ
明示バインドしており、既定探索から外れる。結果、ssh 非対話シェルから素で
`mat listen` を叩くと exit 13（`matd_unavailable`）、read/on は黙って直経路に
落ちる。運用では毎回 `MAT_MATD_SOCKET=/run/user/1000/matd/matd.sock` の前置きが
必要だった。

## 決定（ユーザー承認 2026-07-21）

既定を systemd の `RuntimeDirectory` 慣習（subdir 形式）へ寄せる。

### matd（`--socket` 省略時）

- `$XDG_RUNTIME_DIR/matd/matd.sock` にバインド。親ディレクトリ
  `$XDG_RUNTIME_DIR/matd/` が無ければ **0700 で作成**する。
- `XDG_RUNTIME_DIR` 不在時は従来どおり `/tmp/matd.sock`（変更しない —
  `/tmp` 直下に固定名ディレクトリを作ると他ユーザーの dir squatting 面が
  増えるだけで、実質デッドパス）。

### mat（既定探索時）

候補リストを**順に connect 試行**し、最初に成功したものを採用する:

1. `$XDG_RUNTIME_DIR/matd/matd.sock`（新既定）
2. `$XDG_RUNTIME_DIR/matd.sock`（旧既定 — 移行期互換。手動起動の旧 matd を拾う）

XDG 不在時は `/tmp/matd.sock` のみ（候補 1 本）。stale socket ファイルは
connect が失敗するので自然にスキップされる。

### 明示指定は不変

`--matd <path>` > `MAT_MATD_SOCKET`（非空）> 上記候補リスト。明示時は候補探索
しない（1 本のみ）。`MAT_MATD_SOCKET` がパス指定のみで経路を変えない性質も不変。

### 全候補失敗時の挙動（現行踏襲）

- Auto → native 直経路フォールバック
- `listen` → `matd_unavailable`（exit 13）
- Forced（`--matd` 値省略 / `MAT_MATD=truthy`）→ エラー。detail に試行した
  全候補パスを列挙する。

## 実装点

- `mat-core::socket`: `default_socket_candidates() -> Vec<PathBuf>` を追加。
  env 読み取りは注入引数（`Option<OsString>`）を取る純関数に切り出して
  テスト可能にする。`default_socket_path()`（matd の bind 用）は新 subdir
  形式を返すよう変更。
- `matd`: bind 前に親ディレクトリを 0700 で作成（既定パス時のみで良い —
  明示 `--socket` の親不在は従来どおりエラー）。lock ファイルパスは socket
  パス派生（`lock.rs`）なので自動で追従する。
- `mat::matd_client`: `Route::Auto` / `Route::Forced`（パス省略時）が候補
  `Vec<PathBuf>` を保持し、connect 失敗で次候補へ。`--matd <path>` 明示は
  従来どおり単一パスの `Forced`。

## 代替案（不採用）

- **旧 flat 候補を残さない一発切替**: 実装は僅かに単純だが、新 mat × 旧 matd
  （手動起動）の組み合わせが黙って直経路に落ちる。互換候補 1 本の維持コストは
  ほぼゼロなのでリスクだけ増える。
- **mat 側の探索だけ足して matd 既定は flat 維持**: 後方互換は最大だが、
  mat/matd の既定が食い違ったままになり「同じ既定を指すよう一箇所で定義する」
  という `mat-core::socket` の存在理由が崩れる。

## テスト

- 候補生成の単体テスト（XDG 有無 × env 注入、順序のピン）。
- `resolve_route` の既存テスト更新（Auto/Forced が候補リストを持つ形へ）。
- 候補フォールバックの単体テスト: 候補 1 が不在/stale で候補 2 の matd に
  届くこと（tempdir 上の実 unix socket で可能、実デバイス不要）。
- matd の既定パス時 dir 自動作成の単体テスト。
- バイナリ統合テスト（tempdir socket）は既存パターン踏襲で回帰確認。

## ドキュメント・波及

- README: matd の socket 既定と `MAT_MATD_SOCKET` の説明を候補リスト形式へ
  更新。`mat --matd` / `matd --socket` のヘルプ文（`cli.rs` / `matd/main.rs`）
  も同様。
- jarvis-iac の unit は `--socket %t/matd/matd.sock` 明示なので**無変更で
  互換**。既定と一致するため将来フラグを落とせる（別作業、iac 側）。
- メモリ `jarvis-matd-deploy` の「ssh から listen は MAT_MATD_SOCKET 必須」は
  0.27.0 デプロイ後に陳腐化するので、デプロイ時に更新する。
- バージョン: 0.27.0（minor — 既定パスの変更は挙動変更だが、明示指定・
  systemd 運用は無影響）。
