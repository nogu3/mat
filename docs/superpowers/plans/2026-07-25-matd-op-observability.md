# matd の op 可観測性 実装計画（1.3.0）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `matd` が op ごとに 1 行の構造化ログを出し、そのログが journald 上で
実際に grep できる状態にする（ANSI 抑止 + `MAT_LOG=""` の罠修正を含む）。

**Architecture:** ログは `server.rs::dispatch` の 1 箇所で `run_op` を挟んで出す。
level 方針は純関数 `classify_op_log` に集約してテストで釘打ちし、tracing のマクロ
呼び出しは薄く保つ。op 名・endpoint・group_id・path は `protocol.rs` の `Op` に
網羅 match のアクセサとして足すので、op 追加時にコンパイラが漏れを強制する。
ログフィルタ指定の選択規則だけを `mat-core` の純関数に置き、subscriber の組み立ては
各バイナリに残す（`tracing-subscriber` 依存を `mat-core` に持ち込まない）。

**Tech Stack:** Rust 2021 / `tracing` 0.1 + `tracing-subscriber` 0.3（`fmt` +
`env-filter`）/ tokio / Taskfile（`task check` = `fmt:check` + `clippy -D warnings` + `test`）

**Spec:** `docs/superpowers/specs/2026-07-25-matd-op-observability-design.md`

## Global Constraints

これは全タスクの要件に暗黙に含まれる。

- **挙動変更はゼロ。** 応答 JSON のスキーマ・exit code・エラー分類・タイミングを
  変えてはいけない。追加するのは stderr のログと、ログ初期化の 2 つの不具合修正だけ。
- **設計規則（CLAUDE.md）**: stdout は純粋な構造化 JSON のみ。診断は stderr の
  構造化ログ。`mat` / `matd` の command 層にプロトコルコードを置かない。
- **tracing のフィールド名に `target` / `name` / `parent` / `level` を使わない。**
  マクロが `target:` 等を特別扱いするため。cluster/attribute は `path` を使う。
- **`Option` フィールドをそのまま渡す。** `impl<T: Value> Value for Option<T>` は
  `None` のときフィールドごと省略するので `node_id=Some(42)` にはならない。
  `?op.node_id()` のような Debug 整形はしてはいけない（`grep node_id=42` が壊れる）。
- **ブランチは `feat/matd-op-observability`**（既に spec のコミット `b501073` がある）。
- **コミットするのはこの作業で編集したファイルだけ。** リポジトリ直下の
  `thread-map.html` は作業開始前から untracked。触らないし add しない。
- **public repo。** テスト・ドキュメント・コメントに実在の node_id / IP / 証明書を
  書かない（ダミー値のみ）。
- 各タスクの最後に `task check` が緑であること。コメントは既存コードと同じ日本語。
- バージョンは **Task 6 で一度だけ** `Cargo.toml` の workspace version を
  `1.2.1` → `1.3.0` にする。他のタスクで触らない。

## File Structure

| ファイル | 責務 | タスク |
|---|---|---|
| `crates/mat-core/src/log.rs`（新規） | ログフィルタ指定の選択規則（純関数のみ。subscriber は組み立てない） | 1 |
| `crates/mat-core/src/lib.rs` | `pub mod log;` の登録 | 1 |
| `crates/matd/src/main.rs` | matd の subscriber 組み立て（ANSI 固定 off、既定 info） | 1 |
| `crates/mat/src/main.rs` | mat の subscriber 組み立て（tty 判定、既定 warn） | 1 |
| `crates/matd/src/protocol.rs` | `Op` のログ用アクセサ 4 つ（`name` / `group_id` / `endpoint` / `log_path`） | 2 |
| `crates/matd/src/server.rs` | op ログの分類（`classify_op_log`）と出力（`log_op`）、listen の attach/detach | 3, 5 |
| `crates/matd/src/native.rs` | cold（初回 CASE 確立）の 1 行 | 4 |
| `README.md` | `## Logs (stderr)` 節と `MAT_LOG` の説明 | 6 |
| `Cargo.toml` | workspace version 1.3.0 | 6 |

---

### Task 1: ログ初期化を直す（ANSI 抑止 + `MAT_LOG=""` の罠）

このタスクだけで「journald のログが grep できる」という独立した価値が出る。
以降のタスクが増やすログもこの初期化の上に載る。

**Files:**
- Create: `crates/mat-core/src/log.rs`
- Modify: `crates/mat-core/src/lib.rs`（モジュール登録、7-21 行のリスト内）
- Modify: `crates/matd/src/main.rs:65-76`（`fn main` 冒頭の subscriber 組み立て）
- Modify: `crates/mat/src/main.rs:205-213`（`fn init_tracing`）
- Test: `crates/mat-core/src/log.rs` の `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: なし（最初のタスク）
- Produces: `mat_core::log::log_filter_candidates(mat_log: Option<&str>, rust_log: Option<&str>) -> Vec<String>`
  と `mat_core::log::log_filter_candidates_from_env() -> Vec<String>`。以降のタスクは
  これに依存しないが、ログが ANSI なしで出ることを前提に検証手順を書く。

**候補を 1 つに絞らず順序付きで返す理由**（挙動変更ゼロの制約に直結する）:
旧実装 `EnvFilter::try_from_env("MAT_LOG").or_else(|_| try_from_default_env())` は
`try_from_env` が**変数未設定のときも、設定されているが不正な指定のときも `Err`**
を返すため、「`MAT_LOG` が不正なら `RUST_LOG` を使う」挙動を持っていた。指定を
1 つに先決めしてから 1 回だけパースすると、この経路が失われて既定 level に
落ちてしまう（`MAT_LOG=garbage!! RUST_LOG=debug` が debug でなく warn/info に
なる）。候補列を返して呼び出し側が順に `try_new` する形にすれば、空文字の修正と
旧挙動の保持を同時に満たせる。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/log.rs` を新規作成する。この時点では実装を書かず、
モジュールの doc コメントとテストだけ置く。

```rust
//! ログ初期化の共有ヘルパ。
//!
//! subscriber の組み立ては各バイナリ（`mat` / `matd`）が行う — `mat-core` に
//! `tracing-subscriber` 依存を持ち込まないため、ここにはフィルタ指定の
//! 選択規則だけを純関数で置く。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_spec_is_treated_as_unset() {
        // 空文字は EnvFilter としては「ディレクティブ 0 個」の有効な指定に
        // なり、ERROR すら出なくなる（実測）。未設定と同じ扱いにする。
        assert!(log_filter_candidates(Some(""), None).is_empty());
        assert!(log_filter_candidates(Some("   "), None).is_empty());
    }

    #[test]
    fn mat_log_comes_before_rust_log() {
        assert_eq!(
            log_filter_candidates(Some("debug"), Some("trace")),
            vec!["debug".to_string(), "trace".to_string()]
        );
    }

    #[test]
    fn empty_mat_log_falls_back_to_rust_log() {
        assert_eq!(
            log_filter_candidates(Some(""), Some("info")),
            vec!["info".to_string()]
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            log_filter_candidates(Some(" info "), None),
            vec!["info".to_string()]
        );
    }

    #[test]
    fn absent_everywhere_is_empty() {
        assert!(log_filter_candidates(None, None).is_empty());
        assert!(log_filter_candidates(None, Some("  ")).is_empty());
    }
}
```

`crates/mat-core/src/lib.rs` のモジュール宣言リスト（`pub mod ids;` の次、
`mod ids_gen;` の前後はアルファベット順に並んでいる）に 1 行足す:

```rust
pub mod log;
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-core --lib log::`
Expected: コンパイルエラー `cannot find function 'log_filter_candidates' in this scope`

- [ ] **Step 3: 最小の実装を書く**

`crates/mat-core/src/log.rs` の doc コメントの直後、`#[cfg(test)]` の前に足す:

```rust
/// ログフィルタ指定の候補を `MAT_LOG` → `RUST_LOG` の優先順で返す。
///
/// 空文字・空白のみは **未設定として扱う**。`EnvFilter` としてはディレクティブ
/// 0 個の有効な指定になり、既定 level に落ちずログが全 OFF になるため
/// （`systemctl --user set-environment MAT_LOG=...` で一時 debug を入れて
/// 戻すときに踏みやすい）。
///
/// 1 つに絞らず順序付きで返すのは、**パースできない指定を次の候補へ送る**ため。
/// 旧実装の `try_from_env("MAT_LOG").or_else(|_| try_from_default_env())` は
/// 「`MAT_LOG` が不正なら `RUST_LOG` を使う」挙動を持っていた（`try_from_env` は
/// 未設定でも不正でも `Err`）。呼び出し側が順に `try_new` することでそれを保つ。
pub fn log_filter_candidates(mat_log: Option<&str>, rust_log: Option<&str>) -> Vec<String> {
    [mat_log, rust_log]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// 環境変数を読んで [`log_filter_candidates`] を適用する薄いラッパ。
pub fn log_filter_candidates_from_env() -> Vec<String> {
    let mat_log = std::env::var("MAT_LOG").ok();
    let rust_log = std::env::var("RUST_LOG").ok();
    log_filter_candidates(mat_log.as_deref(), rust_log.as_deref())
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-core --lib log::`
Expected: PASS（5 テスト）

- [ ] **Step 5: matd の subscriber を差し替える**

`crates/matd/src/main.rs` の `fn main()` 冒頭。現在:

```rust
fn main() {
    // レベルは mat 本体と同じく `MAT_LOG`（無ければ `RUST_LOG`）で制御。
    // 既定は info（常駐デーモンなので状態遷移は既定で残す）。
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MAT_LOG")
                .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
```

これを置き換える:

```rust
fn main() {
    // レベルは mat 本体と同じく `MAT_LOG`（無ければ `RUST_LOG`）で制御。
    // 既定は info（常駐デーモンなので状態遷移は既定で残す）。空文字は
    // 未設定扱い、パースできない指定は次の候補へ送る（`mat_core::log` 参照）。
    let filter = mat_core::log::log_filter_candidates_from_env()
        .into_iter()
        .find_map(|s| tracing_subscriber::EnvFilter::try_new(&s).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // デーモンなので ANSI は常に無効。tracing-subscriber は tty 判定を
        // せず（`is_ansi = cfg!(feature = "ansi") && NO_COLOR 未設定`）、
        // 既定では journald に `^[[3mnode_id^[[0m^[[2m=^[[0m42` を書いて
        // しまい `grep node_id=42` が空振りする。
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
```

無効なフィルタ文字列のときの挙動は旧実装と同じ: `MAT_LOG` が不正なら
`RUST_LOG` を試し、それも駄目（または未設定）なら既定 level へ落ちる。

- [ ] **Step 6: mat の subscriber を差し替える**

`crates/mat/src/main.rs` の `fn init_tracing()`。現在:

```rust
fn init_tracing() {
    let filter = EnvFilter::try_from_env("MAT_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
```

これを置き換える:

```rust
fn init_tracing() {
    // 空文字は未設定扱い、パースできない指定は次の候補へ送る
    // （`mat_core::log` 参照）。既定は warn。
    let filter = mat_core::log::log_filter_candidates_from_env()
        .into_iter()
        .find_map(|s| EnvFilter::try_new(&s).ok())
        .unwrap_or_else(|| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        // 対話 tty では色を許すが、パイプや mando 経由では ANSI を出さない
        // （構造化ログを grep できる形に保つ）。
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
}
```

同ファイルの use ブロックに `IsTerminal` を足す。`crates/mat/src/main.rs` には
`use std::io::...` が無いので、`use std::process::ExitCode;` の直前に新しい行を
入れる（std を先に並べる既存の順序に合わせる）:

```rust
use std::io::IsTerminal;
use std::process::ExitCode;
```

- [ ] **Step 7: ANSI が消えたことと `MAT_LOG=""` が既定に落ちることを実測**

```bash
cargo build -p matd
S=$(mktemp -d)
# この環境の iface autodetect は [eth1, eth4] で曖昧なので明示する。
# socket も一時ディレクトリに逃がす（/run/user/... に残骸を作らない）。
# matd 自身の bind 先は `--socket`。`MAT_MATD_SOCKET` は mat クライアント側が
# 読む変数なので matd には効かない。
MAT_STORE=$S MAT_MATD_IFACE=eth1 MAT_LOG=info \
  timeout 3 ./target/debug/matd --socket $S/t.sock 2>&1 | cat -v | head -5
```

Expected: `INFO matd: matd: resident mDNS operational cache enabled` のような行が
出て、**`^[[` が 1 つも含まれない**（修正前は `^[[2m2026-...^[[0m ^[[32m INFO^[[0m`
のように必ず含まれていた）。

```bash
MAT_STORE=$S MAT_MATD_IFACE=eth1 MAT_LOG="" \
  timeout 3 ./target/debug/matd --socket $S/t.sock 2>&1 | head -3
rm -rf $S
```

Expected: 修正前は**出力が完全に空**だった。修正後は既定 info のログ（上と同じ
起動ログ）が出る。

- [ ] **Step 8: `task check` が緑であることを確認**

Run: `task check`
Expected: exit 0（`fmt:check` + `clippy -D warnings` + 全テスト）

- [ ] **Step 9: コミット**

```bash
git add crates/mat-core/src/log.rs crates/mat-core/src/lib.rs \
        crates/matd/src/main.rs crates/mat/src/main.rs
git commit -m "$(cat <<'EOF'
fix(log): ANSI エスケープ抑止 + MAT_LOG="" の全 OFF を塞ぐ

matd のログは既定で ANSI が入り、journald 上で `grep node_id=42` が
空振りしていた（tracing-subscriber は tty 判定をしない）。matd は
with_ansi(false) 固定、mat は stderr の IsTerminal 判定にする。

MAT_LOG="" は EnvFilter としてディレクティブ 0 個の有効な指定になり
既定 level に落ちずログが全 OFF になっていた（実測）。空文字・空白のみを
未設定扱いにする選択規則を mat-core の純関数に置く（subscriber の組み立ては
各バイナリに残し、mat-core に tracing-subscriber 依存を作らない）。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_017dy99qhTKvT89sEjNaUo8m
EOF
)"
```

---

### Task 2: `Op` のログ用アクセサ 4 つ

**Files:**
- Modify: `crates/matd/src/protocol.rs`（`impl Op` ブロック、176-202 行の
  `node_id()` の直後）
- Test: `crates/matd/src/protocol.rs` の既存 `#[cfg(test)] mod tests`（204 行以降）

**Interfaces:**
- Consumes: なし
- Produces: `Op::name(&self) -> &'static str` / `Op::group_id(&self) -> Option<u16>` /
  `Op::endpoint(&self) -> Option<u16>` / `Op::log_path(&self) -> Option<String>`。
  Task 3 の `log_op` がこの 4 つと既存の `node_id()` を使う。

`Op` の 17 variant は `Read` / `Write` / `Invoke` / `On` / `Off` / `ColorTemp` /
`Level` / `Color` / `Describe` / `GroupProvision` / `GroupInvoke` /
`GroupColorTemp` / `GroupLevel` / `GroupColor` / `Listen` / `Ping` / `Shutdown`。
enum は `#[serde(tag = "op", rename_all = "snake_case")]` なので、`name()` は
ワイヤの `op` タグと同じ文字列を返す。

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/protocol.rs` の `mod tests` 内（既存の `fn parse(s: &str) -> Request`
ヘルパを使う）に追加する。

```rust
    #[test]
    fn name_matches_the_wire_op_tag() {
        for (line, expected) in [
            (
                r#"{"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"}"#,
                "read",
            ),
            (
                r#"{"op":"color_temp","node_id":1,"endpoint":1,"mireds":300,"kelvin":3333}"#,
                "color_temp",
            ),
            (
                r#"{"op":"group_invoke","group_id":10,"cluster":"onoff","command":"on","endpoint":1}"#,
                "group_invoke",
            ),
            (r#"{"op":"ping"}"#, "ping"),
            (r#"{"op":"shutdown"}"#, "shutdown"),
        ] {
            assert_eq!(parse(line).op.name(), expected, "line: {line}");
        }
    }

    #[test]
    fn log_accessors_pick_the_right_fields() {
        let read = parse(
            r#"{"op":"read","node_id":6,"endpoint":1,"cluster":"onoff","attribute":"on-off"}"#,
        )
        .op;
        assert_eq!(read.node_id(), Some(6));
        assert_eq!(read.group_id(), None);
        assert_eq!(read.endpoint(), Some(1));
        assert_eq!(read.log_path().as_deref(), Some("onoff/on-off"));

        let group_invoke = parse(
            r#"{"op":"group_invoke","group_id":10,"cluster":"onoff","command":"on","endpoint":1}"#,
        )
        .op;
        assert_eq!(group_invoke.node_id(), None);
        assert_eq!(group_invoke.group_id(), Some(10));
        assert_eq!(group_invoke.endpoint(), Some(1));
        assert_eq!(group_invoke.log_path().as_deref(), Some("onoff/on"));

        // ショートカット op は op 名だけで用が足りるので path を持たない。
        let on = parse(r#"{"op":"on","node_id":6,"endpoint":1}"#).op;
        assert_eq!(on.log_path(), None);
        assert_eq!(on.endpoint(), Some(1));

        // endpoint も group_id も持たない op。
        let ping = parse(r#"{"op":"ping"}"#).op;
        assert_eq!(ping.endpoint(), None);
        assert_eq!(ping.group_id(), None);
        assert_eq!(ping.log_path(), None);
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd --lib protocol::tests::name_matches_the_wire_op_tag protocol::tests::log_accessors_pick_the_right_fields`
Expected: コンパイルエラー `no method named 'name' found for enum 'Op'`

- [ ] **Step 3: アクセサを実装する**

`crates/matd/src/protocol.rs` の `impl Op` の中、`node_id()` の直後に足す。
4 つとも**網羅 match**にする（`_ =>` を使わない）。op を増やしたときに
コンパイラが漏れを指摘するのが目的。

```rust
    /// 構造化ログ用の op 名。ソケットプロトコルの `op` タグと同じ snake_case
    /// （`Op` は `Deserialize` のみなのでタグ文字列を再利用できず、手書きの
    /// 網羅 match にする — op 追加時にコンパイラが漏れを強制する）。
    pub fn name(&self) -> &'static str {
        match self {
            Op::Read { .. } => "read",
            Op::Write { .. } => "write",
            Op::Invoke { .. } => "invoke",
            Op::On { .. } => "on",
            Op::Off { .. } => "off",
            Op::ColorTemp { .. } => "color_temp",
            Op::Level { .. } => "level",
            Op::Color { .. } => "color",
            Op::Describe { .. } => "describe",
            Op::GroupProvision { .. } => "group_provision",
            Op::GroupInvoke { .. } => "group_invoke",
            Op::GroupColorTemp { .. } => "group_color_temp",
            Op::GroupLevel { .. } => "group_level",
            Op::GroupColor { .. } => "group_color",
            Op::Listen { .. } => "listen",
            Op::Ping => "ping",
            Op::Shutdown => "shutdown",
        }
    }

    /// group 系 op の対象 group_id（ログ用）。node_id を持たない op の識別に使う。
    pub fn group_id(&self) -> Option<u16> {
        match self {
            Op::GroupProvision { group_id, .. }
            | Op::GroupInvoke { group_id, .. }
            | Op::GroupColorTemp { group_id, .. }
            | Op::GroupLevel { group_id, .. }
            | Op::GroupColor { group_id, .. } => Some(*group_id),
            Op::Read { .. }
            | Op::Write { .. }
            | Op::Invoke { .. }
            | Op::On { .. }
            | Op::Off { .. }
            | Op::ColorTemp { .. }
            | Op::Level { .. }
            | Op::Color { .. }
            | Op::Describe { .. }
            | Op::Listen { .. }
            | Op::Ping
            | Op::Shutdown => None,
        }
    }

    /// ログ用の endpoint。`Listen` の `endpoint` は `Option` だが、この op は
    /// dispatch に到達しない（`handle_conn` が先取りする）ので None を返す。
    pub fn endpoint(&self) -> Option<u16> {
        match self {
            Op::Read { endpoint, .. }
            | Op::Write { endpoint, .. }
            | Op::Invoke { endpoint, .. }
            | Op::On { endpoint, .. }
            | Op::Off { endpoint, .. }
            | Op::ColorTemp { endpoint, .. }
            | Op::Level { endpoint, .. }
            | Op::Color { endpoint, .. }
            | Op::GroupProvision { endpoint, .. }
            | Op::GroupInvoke { endpoint, .. }
            | Op::GroupColorTemp { endpoint, .. }
            | Op::GroupLevel { endpoint, .. }
            | Op::GroupColor { endpoint, .. } => Some(*endpoint),
            Op::Describe { .. } | Op::Listen { .. } | Op::Ping | Op::Shutdown => None,
        }
    }

    /// ログ用の対象パス。属性系は `cluster/attribute`、コマンド系は
    /// `cluster/command`。op 名だけで足りるショートカットは None。
    /// （フィールド名に `target` を使わないのは tracing のマクロが `target:` を
    /// 特別扱いするため。）
    pub fn log_path(&self) -> Option<String> {
        match self {
            Op::Read {
                cluster, attribute, ..
            }
            | Op::Write {
                cluster, attribute, ..
            } => Some(format!("{cluster}/{attribute}")),
            Op::Invoke {
                cluster, command, ..
            }
            | Op::GroupInvoke {
                cluster, command, ..
            } => Some(format!("{cluster}/{command}")),
            Op::On { .. }
            | Op::Off { .. }
            | Op::ColorTemp { .. }
            | Op::Level { .. }
            | Op::Color { .. }
            | Op::Describe { .. }
            | Op::GroupProvision { .. }
            | Op::GroupColorTemp { .. }
            | Op::GroupLevel { .. }
            | Op::GroupColor { .. }
            | Op::Listen { .. }
            | Op::Ping
            | Op::Shutdown => None,
        }
    }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd --lib protocol::`
Expected: PASS（既存 18 + 追加 2）

- [ ] **Step 5: `task check` が緑であることを確認**

Run: `task check`
Expected: exit 0

- [ ] **Step 6: コミット**

```bash
git add crates/matd/src/protocol.rs
git commit -m "$(cat <<'EOF'
feat(matd): Op にログ用アクセサ（name / group_id / endpoint / log_path）

op ログを 1 箇所で出すための材料。node_id() と同形の網羅 match にして、
op を増やしたときにコンパイラが漏れを強制する形にした。name() は
ワイヤの op タグと同じ snake_case を返す（テストで一致を釘打ち）。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_017dy99qhTKvT89sEjNaUo8m
EOF
)"
```

---

### Task 3: op ログ本体（`classify_op_log` + `log_op` + dispatch）

**Files:**
- Modify: `crates/matd/src/server.rs`（`dispatch` = 284-321 行。その直前に
  `SLOW_OP_MS` / `OpLogClass` / `classify_op_log` / `log_op` を置く）
- Test: `crates/matd/src/server.rs` の既存 `#[cfg(test)] mod tests`（960 行以降）

**Interfaces:**
- Consumes: Task 2 の `Op::name()` / `group_id()` / `endpoint()` / `log_path()` と
  既存の `Op::node_id()`
- Produces: `classify_op_log(result: &Result<Value, MatError>, elapsed_ms: u64) -> OpLogClass`
  （private）と `log_op(op: &Op, result: &Result<Value, MatError>, elapsed_ms: u64)`
  （private）。Task 5 と 6 はこれに依存しない。

`server.rs` は `mat_core::error::{ErrorKind, MatError}` と `serde_json::Value` を
既に import しているので use の追加は不要。

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/server.rs` の `mod tests` に追加する。`ErrorKind` 全 13 variant を
明示的に並べる — これが level 方針の唯一の釘。

```rust
    fn err(kind: ErrorKind) -> Result<Value, MatError> {
        Err(MatError::new(kind, "test"))
    }

    #[test]
    fn path_failures_are_warn_worthy() {
        for kind in [
            ErrorKind::Timeout,
            ErrorKind::Unreachable,
            ErrorKind::SessionFailed,
            ErrorKind::Other,
            ErrorKind::CommissionFailed,
            ErrorKind::MatdUnavailable,
            ErrorKind::ChildNotFound,
            ErrorKind::ChildFailed,
        ] {
            assert_eq!(
                classify_op_log(&err(kind), 10),
                OpLogClass::Failed,
                "kind: {kind:?}"
            );
        }
    }

    #[test]
    fn request_side_errors_do_not_pollute_warn() {
        for kind in [
            ErrorKind::StoreMissing,
            ErrorKind::StoreParse,
            ErrorKind::NodeNotCommissioned,
            ErrorKind::DeviceRejected,
            ErrorKind::ParseError,
        ] {
            assert_eq!(
                classify_op_log(&err(kind), 10),
                OpLogClass::Rejected,
                "kind: {kind:?}"
            );
        }
    }

    #[test]
    fn slow_threshold_is_inclusive_at_300ms() {
        let ok = Ok(json!({"value": true}));
        assert_eq!(classify_op_log(&ok, 0), OpLogClass::Ok);
        assert_eq!(classify_op_log(&ok, 299), OpLogClass::Ok);
        assert_eq!(classify_op_log(&ok, 300), OpLogClass::Slow);
        assert_eq!(classify_op_log(&ok, 8134), OpLogClass::Slow);
    }

    #[test]
    fn elapsed_time_does_not_change_error_classification() {
        // 失敗は所要時間に関わらず失敗として分類される（速い失敗を
        // 「速いから ok」に見せない）。
        assert_eq!(classify_op_log(&err(ErrorKind::Timeout), 0), OpLogClass::Failed);
        assert_eq!(
            classify_op_log(&err(ErrorKind::NodeNotCommissioned), 9999),
            OpLogClass::Rejected
        );
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd --lib server::tests::path_failures_are_warn_worthy`
Expected: コンパイルエラー `cannot find function 'classify_op_log'`

- [ ] **Step 3: 分類と出力を実装する**

`crates/matd/src/server.rs` の `async fn dispatch` の直前に足す:

```rust
/// 成功 op のうち、この時間以上かかったものは info で残す（劣化の前兆）。
/// warm セッションの実測は 71-149ms なので、300ms を超えた成功はもう
/// 「普段と違う」= 弱リンク化 / メッシュ劣化の兆候である。
const SLOW_OP_MS: u64 = 300;

/// op ログの level 方針。ここに集約してテストで釘を打ち、tracing のマクロ
/// 呼び出し側は薄く保つ。
#[derive(Debug, PartialEq, Eq)]
enum OpLogClass {
    /// 経路そのものの問題（warn）。`journalctl -p warning` で劣化だけ抽出できる。
    Failed,
    /// 要求側・意味の問題（info）。warn を汚さない。
    Rejected,
    /// 成功だが遅い（info）。
    Slow,
    /// 通常の成功（debug）。既定 level（info）では出ない。
    Ok,
}

/// `ErrorKind` を網羅 match する — 将来 variant が増えたら level の判断を
/// コンパイラが強制する。
fn classify_op_log(result: &Result<Value, MatError>, elapsed_ms: u64) -> OpLogClass {
    match result {
        Ok(_) if elapsed_ms >= SLOW_OP_MS => OpLogClass::Slow,
        Ok(_) => OpLogClass::Ok,
        Err(e) => match e.kind {
            ErrorKind::Timeout
            | ErrorKind::Unreachable
            | ErrorKind::SessionFailed
            | ErrorKind::Other
            // commission / child 系は matd 経路では発生しないが、網羅 match の
            // ために分類は決めておく（発生したら経路の問題として扱う）。
            | ErrorKind::CommissionFailed
            | ErrorKind::MatdUnavailable
            | ErrorKind::ChildNotFound
            | ErrorKind::ChildFailed => OpLogClass::Failed,
            ErrorKind::StoreMissing
            | ErrorKind::StoreParse
            | ErrorKind::NodeNotCommissioned
            | ErrorKind::DeviceRejected
            | ErrorKind::ParseError => OpLogClass::Rejected,
        },
    }
}

/// op 1 件を 1 行で記録する。
///
/// `Option` のフィールドはそのまま渡す — tracing は `None` のフィールドを
/// 省略するので `node_id=Some(42)` にはならず、`grep node_id=42` が効く。
fn log_op(op: &Op, result: &Result<Value, MatError>, elapsed_ms: u64) {
    let op_name = op.name();
    let node_id = op.node_id();
    let group_id = op.group_id();
    let endpoint = op.endpoint();
    let path = op.log_path();
    let path = path.as_deref();
    match result {
        Err(e) => match classify_op_log(result, elapsed_ms) {
            OpLogClass::Rejected => tracing::info!(
                op = op_name, node_id, group_id, endpoint, path, elapsed_ms,
                kind = ?e.kind, detail = %e.detail, "matd op rejected"
            ),
            _ => tracing::warn!(
                op = op_name, node_id, group_id, endpoint, path, elapsed_ms,
                kind = ?e.kind, detail = %e.detail, "matd op failed"
            ),
        },
        Ok(_) => match classify_op_log(result, elapsed_ms) {
            OpLogClass::Slow => tracing::info!(
                op = op_name, node_id, group_id, endpoint, path, elapsed_ms,
                "matd op slow"
            ),
            _ => tracing::debug!(
                op = op_name, node_id, group_id, endpoint, path, elapsed_ms,
                "matd op ok"
            ),
        },
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd --lib server::`
Expected: PASS（既存 17 + 追加 4）。`log_op` はまだ呼ばれていないので
`dead_code` 警告が出る場合は Step 5 で解消される（`task check` は Step 6 で走らせる）。

- [ ] **Step 5: dispatch から呼ぶ**

`crates/matd/src/server.rs` の `dispatch`。現在:

```rust
    let id = req.id.clone();
    let is_shutdown = matches!(req.op, Op::Shutdown);

    let body = match run_op(&req.op, native, store_path, health).await {
        Ok(mut body) => {
```

これを置き換える（`run_op` を計測して結果をログしてからボディを組む）:

```rust
    let id = req.id.clone();
    let is_shutdown = matches!(req.op, Op::Shutdown);

    // op の所要時間は run_op のみを測る（JSON パース・応答書き込みは含めない）。
    let started = std::time::Instant::now();
    let result = run_op(&req.op, native, store_path, health).await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    log_op(&req.op, &result, elapsed_ms);

    let body = match result {
        Ok(mut body) => {
```

`Ok(mut body) => { ... }` / `Err(e) => error_response(id, &e)` の中身は**変えない**。

- [ ] **Step 6: `task check` が緑であることを確認**

Run: `task check`
Expected: exit 0

- [ ] **Step 7: 実際に 1 行出ることを目視**

`matd` はデバイスが無いと op が失敗するので、warn 側の 1 行を確認する。

```bash
cargo build -p matd
S=$(mktemp -d)
MAT_STORE=$S MAT_MATD_IFACE=eth1 MAT_LOG=info \
  ./target/debug/matd --socket $S/t.sock 2>$S/log &
sleep 1
printf '{"op":"read","node_id":6,"endpoint":1,"cluster":"onoff","attribute":"on-off"}\n' \
  | timeout 5 nc -U $S/t.sock
printf '{"op":"shutdown"}\n' | timeout 5 nc -U $S/t.sock
cat -v $S/log | grep 'matd op'
rm -rf $S
```

Expected: `WARN matd::server: matd op failed op=read node_id=6 endpoint=1
path=onoff/on-off elapsed_ms=... kind=... detail=...` の形の 1 行が出る
（この環境では native 構築が失敗しているので kind は `StoreMissing` 系＝
`matd op rejected` になることもある。どちらでも「1 行出て `node_id=6` と
`path=onoff/on-off` が ANSI なしで grep できる」ことが確認事項）。
`nc` が無ければ `python3 -c` の socket ワンライナーでもよい。

- [ ] **Step 8: コミット**

```bash
git add crates/matd/src/server.rs
git commit -m "$(cat <<'EOF'
feat(matd): op ごとに 1 行の構造化ログを出す

matd は op 経路のログを 1 行も出しておらず、実機の失敗を後から突き合わせ
られなかった（mat の直経路には 19 箇所ある非対称）。dispatch 1 箇所で
run_op を計測し、結果に応じて 1 行出す。

level 方針は純関数 classify_op_log に集約し、ErrorKind 全 13 variant を
網羅 match してテストで釘打ちした（将来 variant が増えたら判断を強制される）。
経路の問題は warn、要求側の問題は info、300ms 以上の成功は info、
通常の成功は debug。挙動変更はゼロ。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_017dy99qhTKvT89sEjNaUo8m
EOF
)"
```

---

### Task 4: cold（初回 CASE 確立）を 1 行残す

**Files:**
- Modify: `crates/matd/src/native.rs:132-136`（`with_session` の slot 確立部）

**Interfaces:**
- Consumes: なし
- Produces: なし（ログ行のみ）

**このタスクに単体テストは無い。** 出力するのは tracing の 1 行で、それを検証
するには subscriber を捕まえる dev-dependency（`tracing-test`）が必要になる。
今回は入れない（YAGNI）。代わりに Task 6 の実機 E2E 項目 2 で確認する。

- [ ] **Step 1: 1 行足す**

`crates/matd/src/native.rs` の `with_session`。現在:

```rust
        let slot = self.slot(node_id).await;
        let mut guard = slot.lock().await;
        if guard.is_none() {
            *guard = Some(self.engine.establisher.establish(node_id).await?);
        }
```

これを置き換える:

```rust
        let slot = self.slot(node_id).await;
        let mut guard = slot.lock().await;
        if guard.is_none() {
            // cold: warm セッションが無いので CASE から張る。同じノードで
            // これが繰り返し出るなら session churn（op ログの elapsed_ms が
            // 伸びる原因）。再確立側は下の Timeout / その他エラー腕で既に
            // info を出しているので、これで確立の両側が揃う。
            tracing::info!(node_id, "no warm session; establishing");
            *guard = Some(self.engine.establisher.establish(node_id).await?);
        }
```

確立が成功したことは op ログ側（`matd op ok` / `slow`）に出るので、成功時の
2 行目は出さない。

- [ ] **Step 2: `task check` が緑であることを確認**

Run: `task check`
Expected: exit 0

- [ ] **Step 3: コミット**

```bash
git add crates/matd/src/native.rs
git commit -m "$(cat <<'EOF'
feat(matd): cold（初回 CASE 確立）を info 1 行で残す

with_session は再確立だけログしていて初回確立が無音だったため、
「毎回 cold になっている（= session churn）」が観測できなかった。
確立の両側が揃い、op ログの elapsed_ms が伸びた理由が読める。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_017dy99qhTKvT89sEjNaUo8m
EOF
)"
```

---

### Task 5: listen の接続 / 切断を記録する

**Files:**
- Modify: `crates/matd/src/server.rs:143-153`（`handle_conn` の listen 分岐、
  ack flush の直後）
- Modify: `crates/matd/src/server.rs:173-215`（`stream_events`）

**Interfaces:**
- Consumes: `ListenFilter`（同一モジュールの private struct。フィールドは
  `node_id: Option<u64>` / `endpoint: Option<u16>` / `cluster: Option<u32>` /
  `attribute: Option<u32>`）
- Produces: なし（ログ行のみ）

**このタスクにも単体テストは無い**（Task 4 と同じ理由）。Task 6 の実機 E2E
項目 4 で確認する。

- [ ] **Step 1: attach を 1 行足す**

`handle_conn` の listen 分岐、ack を flush して `stream_events` に渡す直前。現在:

```rust
                let mut buf = serde_json::to_vec(&ack).unwrap_or_else(|_| b"{}".to_vec());
                buf.push(b'\n');
                write_half.write_all(&buf).await?;
                write_half.flush().await?;
                return stream_events(rx, filter, &mut lines, &mut write_half).await;
```

これを置き換える（`filter` は次行で move されるので、ログはその前に出す）:

```rust
                let mut buf = serde_json::to_vec(&ack).unwrap_or_else(|_| b"{}".to_vec());
                buf.push(b'\n');
                write_half.write_all(&buf).await?;
                write_half.flush().await?;
                // 「センサーが反応しなかった」の切り分けに、購読者が居たか
                // どうかを残す。フィルタは全て Option なので未指定は省略される。
                tracing::info!(
                    node_id = filter.node_id,
                    endpoint = filter.endpoint,
                    cluster = filter.cluster,
                    attribute = filter.attribute,
                    "listen client attached"
                );
                return stream_events(rx, filter, &mut lines, &mut write_half).await;
```

- [ ] **Step 2: detach を記録する**

`stream_events` を書き換える。`delivered` を数え、正常終了 2 経路で 1 行出し、
既存の lag warn には `delivered` を足す。現在の関数本体（`loop { tokio::select! { ... } }`）を
次のように変える — `loop` の前に carrier を 1 つ置き、`return` の直前でログする:

```rust
async fn stream_events(
    mut rx: broadcast::Receiver<Event>,
    filter: ListenFilter,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
) -> std::io::Result<()> {
    // 配信件数。切断時に「そもそも 1 件も流れていない」のか
    // 「流れていたのにクライアントが消えた」のかを区別するため。
    let mut delivered: u64 = 0;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if !filter.matches(&ev) {
                        continue;
                    }
                    let mut buf = serde_json::to_vec(&ev.to_json())
                        .unwrap_or_else(|_| b"{}".to_vec());
                    buf.push(b'\n');
                    write_half.write_all(&buf).await?;
                    write_half.flush().await?;
                    delivered += 1;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        skipped = n,
                        delivered,
                        node_id = filter.node_id,
                        "listen client lagged; disconnecting"
                    );
                    let body = json!({
                        "error": { "kind": "other", "detail": "event stream lagged" },
                        "timestamp": now_iso8601(),
                    });
                    let mut buf = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
                    buf.push(b'\n');
                    write_half.write_all(&buf).await?;
                    write_half.flush().await?;
                    return Ok(());
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!(
                        delivered,
                        node_id = filter.node_id,
                        reason = "channel_closed",
                        "listen client detached"
                    );
                    return Ok(());
                }
            },
            line = lines.next_line() => {
                // クライアント切断（None/Err）でストリーム終了。listen 中の追加
                // リクエスト行は無視する（この op は接続占有の例外）。
                match line {
                    Ok(Some(_)) => continue,
                    _ => {
                        tracing::info!(
                            delivered,
                            node_id = filter.node_id,
                            reason = "client_disconnected",
                            "listen client detached"
                        );
                        return Ok(());
                    }
                }
            }
        }
    }
}
```

書き込みエラー（`write_all(...)?`）での切断には新しい行を足さない — 既存の
`connection handler ended with error`（`server.rs:83`）に出る。`?` の伝播を
変えないための意図的な割り切りで、この経路だけ `delivered` は取れない。

- [ ] **Step 3: 既存の listen テストが壊れていないことを確認**

Run: `cargo test -p matd`
Expected: PASS（`crates/matd/tests/integration.rs` の listen 系 10 テスト込み）

- [ ] **Step 4: `task check` が緑であることを確認**

Run: `task check`
Expected: exit 0

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/server.rs
git commit -m "$(cat <<'EOF'
feat(matd): listen クライアントの接着 / 切断を記録する

stream_events はクライアント EOF で無言 return していたため、casad が
繋がっていたか・いつ切れたかが後から分からなかった。attach 時に
フィルタを、detach 時に delivered と理由（client_disconnected /
channel_closed）を残す。既存の lag warn にも delivered を足した。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_017dy99qhTKvT89sEjNaUo8m
EOF
)"
```

---

### Task 6: README・バージョン 1.3.0・実機 E2E

**Files:**
- Modify: `README.md`（`## Backend`（1169 行）の直前に `## Logs (stderr)` を挿入、
  および 1200 行の `MAT_LOG` 行）
- Modify: `Cargo.toml:6`（`version = "1.2.1"` → `"1.3.0"`）

**Interfaces:**
- Consumes: Task 1-5 の全ログ行（文書化の対象）
- Produces: なし

- [ ] **Step 1: README に `## Logs (stderr)` を足す**

`README.md` の `## Backend` の直前（1169 行）に挿入する:

```markdown
## Logs (stderr)

`mat` and `matd` write diagnostics to **stderr** as structured `tracing` logs
(stdout stays pure JSON). The filter comes from `MAT_LOG`, falling back to
`RUST_LOG`; the default is `warn` for `mat` and `info` for `matd`. An empty
value counts as unset, so `MAT_LOG=` falls back to the default instead of
silencing everything.

`matd` never emits ANSI escapes, and `mat` colors only when stderr is a
terminal — so `grep node_id=42` works on a journal or through a pipe.

`matd` logs one line per op:

| line | level | when |
|---|---|---|
| `matd op failed` | warn | the path itself failed — `timeout` / `unreachable` / `session_failed` / `other` (plus the retired `child_*` kinds). Carries `kind` and `detail`. |
| `matd op rejected` | info | the request or its meaning was refused — `store_missing` / `store_parse` / `node_not_commissioned` / `device_rejected` / `parse_error` |
| `matd op slow` | info | success that took **≥ 300 ms**. A warm session is normally 71–149 ms, so this is the early sign of a weak link or a degraded mesh. |
| `matd op ok` | debug | ordinary success — not shown at the default level |

Fields: `op`, `node_id` / `group_id`, `endpoint`, `path` (`cluster/attribute`
or `cluster/command`), `elapsed_ms` (the op itself, excluding JSON handling).
Absent fields are omitted rather than printed as `None`. String values are
quoted by the formatter, numbers are not:

```
WARN matd::server: matd op failed op="read" node_id=42 endpoint=1 path="occupancysensing/occupancy" elapsed_ms=8134 kind=Timeout detail=no acknowledgement within MRP retry budget
```

Related lines:

- `no warm session; establishing` (info) — a CASE session had to be built for
  this op. Repeatedly for one node means session churn.
- `listen client attached` / `listen client detached` (info) — an event-stream
  client connected or went away. `detached` carries `delivered` and `reason`
  (`client_disconnected` / `channel_closed`).
- `subscription established` / `report pump ended` / `subscription lost;
  resubscribing` (info) — the resident Subscribe lifecycle (see
  [Subscriptions](#subscriptions-subscriptionstoml-optional-matd-only)).

`journalctl -p warning` gives you just the degradation.
```

- [ ] **Step 2: `MAT_LOG` の説明行を更新する**

`README.md` の環境変数テーブル。現在:

```markdown
| `MAT_LOG` | `tracing` filter for stderr logs (e.g. `info`) |
```

これを置き換える:

```markdown
| `MAT_LOG` | `tracing` filter for stderr logs (e.g. `info`); empty counts as unset — see [Logs](#logs-stderr) |
```

- [ ] **Step 3: バージョンを上げる**

`Cargo.toml` の 6 行目:

```toml
version = "1.3.0"
```

- [ ] **Step 4: `task check` が緑であることを確認**

Run: `task check`
Expected: exit 0

- [ ] **Step 5: コミット**

```bash
git add README.md Cargo.toml
git commit -m "$(cat <<'EOF'
docs: matd の op ログを README に記載（1.3.0）

出るログ行・level 方針・300ms 閾値・MAT_LOG="" の扱い・ANSI 無効化を
「Logs (stderr)」節にまとめた。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_017dy99qhTKvT89sEjNaUo8m
EOF
)"
```

- [ ] **Step 6: jarvis で実機 E2E（マージ前・必須）**

本番 matd は差し替えない。`.new` を**別 socket に 30 秒だけ**立てて即止める
（隔離 matd と本番 matd の購読追い出し合戦は既知。短時間なので許容する。
代償は本番購読が 1 回再確立する程度）。

```bash
task dist:arm64
scp dist/arm64/matd jarvis:~/matd.new
scp dist/arm64/mat jarvis:~/mat.new
ssh jarvis 'chmod +x ~/matd.new ~/mat.new'
```

jarvis 上で（`XDG_RUNTIME_DIR=/run/user/1000` を前置きすること。直経路の
非対話 ssh は `MAT_FABRIC_INDEX=2` が必要）:

```bash
S=/tmp/matd-e2e; mkdir -p $S
MAT_LOG=info ~/matd.new --store ~/.config/mat --socket $S/t.sock \
  --fabric-index 2 2>$S/log &
sleep 5
~/mat.new --matd $S/t.sock read -n <node> -e 1 -c onoff -a on-off   # cold
~/mat.new --matd $S/t.sock read -n <node> -e 1 -c onoff -a on-off   # warm
~/mat.new --matd $S/t.sock read -n 9999 -e 1 -c onoff -a on-off     # 未台帳
timeout 3 ~/mat.new --matd $S/t.sock listen -n <node> ; true        # attach→切断
printf '{"op":"shutdown"}\n' | nc -U $S/t.sock
cat -v $S/log
```

確認項目（spec の 6 項目）:

1. `matd op ok`（debug）が既定 info では出ないこと。`MAT_LOG=debug` で再実行
   すると出ること
2. warm read の `matd op ok` / `slow` の `elapsed_ms` が 3 桁未満。cold read は
   `no warm session; establishing` の直後に `matd op slow`
3. 到達しない node への read が `matd op failed`（warn）で `kind` / `detail` 付き
   （`node 9999` は `matd op rejected` + `kind=NodeNotCommissioned` になる。
   到達不能の warn を見るには弱リンクのノードか電源を切ったノードを使う）
4. `listen client attached` と `listen client detached reason=client_disconnected
   delivered=N`
5. `cat -v` に `^[[` が 1 つも無いこと。`grep 'node_id=<node>' $S/log` が実際に
   引っかかること
6. `MAT_LOG=` （空）で起動しても既定 info のログが出ること

後片付け（**必ず実行する** — 隔離 matd を残さない）:

```bash
pkill -f 'matd.new' ; rm -rf /tmp/matd-e2e ~/matd.new ~/mat.new
systemctl --user status matd   # 本番が active のままであること
```

- [ ] **Step 7: E2E の結果を記録してマージ判断を仰ぐ**

確認項目 1-6 の実測結果を（ログ行の実物を引用して）報告する。全項目が
通っていれば `superpowers:finishing-a-development-branch` でマージ方式を
決める。1 つでも想定と違えば、原因を突き止めるまでマージしない。

---

## Self-Review

**1. Spec coverage**

| spec の決定 | 実装タスク |
|---|---|
| 1. op ログを dispatch 1 箇所で出す / `elapsed_ms` は `run_op` のみ | Task 3 Step 5 |
| 2. level 方針を純関数 `classify_op_log` で釘打ち（`ErrorKind` 網羅・閾値 300ms） | Task 3 Step 1, 3 |
| 3. 初期化（matd は `with_ansi(false)`、mat は `IsTerminal`、`MAT_LOG=""` を未設定扱い、既定 level 維持） | Task 1 Step 3, 5, 6 |
| 4. warm/cold の初回確立 1 行 | Task 4 Step 1 |
| 5. listen の attach / detach、lag warn に `delivered` | Task 5 Step 1, 2 |
| 6. `Op` のアクセサ 4 つ（`name` / `group_id` / `endpoint` / `log_path`） | Task 2 Step 3 |
| フィールド名に `target` を使わない（`path` にする） | Global Constraints + Task 2 Step 3 |
| テスト（classify 網羅 / アクセサ代表ケース / `log_filter_candidates`） | Task 3 Step 1、Task 2 Step 1、Task 1 Step 1 |
| 実機 E2E（別 socket に 30 秒、確認 6 項目） | Task 6 Step 6 |
| README `## Logs (stderr)` | Task 6 Step 1, 2 |
| バージョン 1.3.0 | Task 6 Step 3 |

漏れは無い。ANSI とログ本文を単体テストしないという spec の決定も、Task 4 / 5 で
「テストは無い・E2E で確認する」と明示している。

**2. Placeholder scan**

`<node>` は E2E 手順の実ノード番号のみ（public repo にダミー以外の実 node_id を
書かない規律による意図的なプレースホルダで、実行者が自分の環境の値を入れる）。
それ以外に TBD / TODO / 「適切に処理する」類は無い。全コードステップに実物の
コードブロックがある。

**3. Type consistency**

- `classify_op_log(&Result<Value, MatError>, u64) -> OpLogClass` — Task 3 の
  テスト・実装・`log_op` の呼び出しで一致。`SLOW_OP_MS: u64` も同じ型。
- `elapsed_ms` は全経路で `u64`（`as_millis()` は `u128` なので
  `u64::try_from(...).unwrap_or(u64::MAX)` で変換。tracing の `Value` は `u64` を
  実装しているが `u128` は当てにしない）。
- `Op::log_path()` は `Option<String>`。`log_op` は `as_deref()` して
  `Option<&str>` で渡す。テストも `as_deref()` で比較している。
- `Op::group_id()` / `endpoint()` は `Option<u16>`、`node_id()` は既存の
  `Option<u64>`。
- `mat_core::log::log_filter_candidates` は `Option<&str>` × 2 → `Vec<String>`。
  両バイナリは `log_filter_candidates_from_env()`（引数なし）を呼び、
  `.into_iter().find_map(|s| EnvFilter::try_new(&s).ok())` で順に試す。
