# `mat level --percent` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** LevelControl MoveToLevel の高頻度ショートカット `mat level --percent` / `mat group level --percent` を `color-temp` と完全同型で追加する（Issue #10、spec = `docs/superpowers/specs/2026-07-20-level-percent-shortcut-design.md`）。

**Architecture:** `color-temp` が通る全レイヤに sibling を足すだけ。新規抽象なし。換算（percent→0–254）は mat CLI 層の 1 箇所（`resolve_color_temp` と同じ配置）。ワイヤ/matd プロトコルには換算済み `level` + エコー用 `percent` を渡す。

**Tech Stack:** Rust workspace（clap derive / serde / 自前 TLV Writer）。

## Global Constraints

- 各タスク完了時に `task check`（fmt:check + clippy -D warnings + test）が通ること。コミットは各タスクの最後。
- コミットメッセージ末尾に必ず付ける:
  ```
  Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01D1nMZo3ak5uZh4ibnmTrpR
  ```
- 作業ディレクトリは worktree `/home/noguk/ghq/github.com/nogu3/mat/.claude/worktrees/feat-level-percent`（branch `worktree-feat-level-percent`）。**着手前に必ず `pwd` と `git branch --show-current` で検証すること（main への誤コミット事故防止）。**
- 換算式: `level = round(percent / 100 * 254)` = `(percent * 254 + 50) / 100`（u32 経由）。`--percent` は 0..=100（0 許容）。255 は予約値なので出ない（100→254）。
- MoveToLevel = cluster 0x0008, command 0x00, fields `{0: level(u8), 1: transition-time(u16, 0.1s), 2: options-mask=0, 3: options-override=0}`（ExecuteIfOff は立てない）。
- stdout は純 JSON のみ（設計ルール2）。成功 body は color-temp の mireds/kelvin を level/percent に置換した同型。

---

### Task 1: `im.rs` — MoveToLevel の TLV エンコーダ

**Files:**
- Modify: `crates/mat-controller/src/im.rs`（定数ブロック 19-29 行付近、`encode_move_to_color_temperature_fields` の直後 ~743 行、テスト群 ~1650 行付近）

**Interfaces:**
- Produces: `pub const CLUSTER_LEVEL_CONTROL: u32 = 0x0008;` / `pub const CMD_MOVE_TO_LEVEL: u32 = 0x00;` / `pub fn encode_move_to_level_fields(level: u8, transition_time_ds: u16) -> Vec<u8>`（後続タスクが matd native / native_direct / server から使う）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/im.rs` のテストモジュール内、`move_to_color_temperature_fields_match_wire_shape` の直後に追加:

```rust
    #[test]
    fn move_to_level_fields_match_wire_shape() {
        // CommandFields (levelcontrol MoveToLevel, cluster spec §1.6.7.1):
        // {0: Level(u8), 1: TransitionTime(u16 0.1s),
        //  2: OptionsMask(u8)=0, 3: OptionsOverride(u8)=0}.
        // MoveToColorTemperature エンコーダと同じ手筋。
        let bytes = encode_move_to_level_fields(127, 30);
        assert_eq!(bytes.first(), Some(&0x15), "opens anonymous struct");
        assert_eq!(bytes.last(), Some(&0x18), "closes container");
        // level=127=0x7F が context tag 0 の u8 として載る（0x24 = ctx-tag u8）
        assert!(
            bytes.windows(3).any(|w| w == [0x24, 0x00, 0x7F]),
            "level 127 as ctx-tag-0 u8, got {bytes:02X?}"
        );
        // transition=30=0x1E が context tag 1 の u8 として載る
        assert!(
            bytes.windows(3).any(|w| w == [0x24, 0x01, 0x1E]),
            "transition 30 as ctx-tag-1 u8, got {bytes:02X?}"
        );
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-controller --lib im::tests::move_to_level_fields_match_wire_shape`
Expected: コンパイルエラー `cannot find function encode_move_to_level_fields`（= RED）

- [ ] **Step 3: 実装**

定数ブロック（`CMD_MOVE_TO_COLOR_TEMPERATURE` の下）に追加:

```rust
pub const CLUSTER_LEVEL_CONTROL: u32 = 0x0008;
pub const ATTR_CURRENT_LEVEL: u32 = 0x0000;
pub const CMD_MOVE_TO_LEVEL: u32 = 0x00;
```

`encode_move_to_color_temperature_fields` の直後に追加:

```rust
/// CommandFields for levelcontrol MoveToLevel (cluster spec §1.6.7.1):
/// `{0: Level(u8), 1: TransitionTime(u16, 0.1 s units), 2: OptionsMask(u8),
/// 3: OptionsOverride(u8)}`. Options are fixed to 0 (execute per the
/// device's Options attribute), matching what chip-tool sends by default.
pub fn encode_move_to_level_fields(level: u8, transition_time_ds: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(level));
    w.put_uint(Tag::Context(1), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(2), 0);
    w.put_uint(Tag::Context(3), 0);
    w.end_container();
    w.finish()
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller --lib im::tests::move_to_level_fields_match_wire_shape`
Expected: PASS。続けて `cargo test -p mat-controller --lib` 全緑。

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/im.rs
git commit -m "feat(im): MoveToLevel の CommandFields エンコーダ追加 (level shortcut Task1)"
```

---

### Task 2: matd 側 — Op variant / native メソッド / server 分岐

**Files:**
- Modify: `crates/matd/src/protocol.rs`（Op enum ~62 行 / `node_id()` ~147-164 行 / テスト ~233 行）
- Modify: `crates/matd/src/native.rs`（`use` 18-21 行 / `color_temp` メソッドの直後 ~226 行）
- Modify: `crates/matd/src/server.rs`（`is_native_hotpath` ~227 / `native_group_params` ~298 / `native_op` ~360 / `hotpath_success_body` ~525 / `group_sent_body` ~673 / 各テスト）

**Interfaces:**
- Consumes: Task 1 の `CLUSTER_LEVEL_CONTROL` / `CMD_MOVE_TO_LEVEL` / `encode_move_to_level_fields`
- Produces: `Op::Level { node_id: u64, endpoint: u16, level: u8, percent: u8, transition: u16 }` / `Op::GroupLevel { group_id: u16, level: u8, percent: u8, transition: u16, endpoint: u16 }`（wire 名 `"level"` / `"group_level"`、Task 4 の matd_client がこの JSON を送る）、`NativeBackend::level(node_id, endpoint, level, transition)`

- [ ] **Step 1: 失敗するテストを書く（protocol parse + server hotpath/group params + body）**

`crates/matd/src/protocol.rs` のテスト（`color_temp_shortcut_parses` の直後）:

```rust
    #[test]
    fn level_shortcut_parses() {
        // level は mat 側で換算済み。percent は応答エコー用。
        let r = parse(
            r#"{"op":"level","node_id":6,"endpoint":1,"level":127,"percent":50,"transition":30}"#,
        );
        assert_eq!(r.op.node_id(), Some(6));
        assert!(matches!(r.op, Op::Level { level: 127, .. }));
        let g = parse(
            r#"{"op":"group_level","group_id":10,"level":254,"percent":100,"endpoint":1}"#,
        );
        assert_eq!(g.op.node_id(), None);
        assert!(matches!(g.op, Op::GroupLevel { level: 254, transition: 0, .. }));
    }
```

`crates/matd/src/server.rs` の `is_native_hotpath` テスト（既存 ColorTemp assert の直後）:

```rust
        assert!(is_native_hotpath(&Op::Level {
            node_id: 1,
            endpoint: 1,
            level: 127,
            percent: 50,
            transition: 0
        }));
```

`native_group_params` テスト（既存 GroupColorTemp 検証の直後）:

```rust
        let lv = Op::GroupLevel {
            group_id: 10,
            level: 254,
            percent: 100,
            transition: 0,
            endpoint: 1,
        };
        let (_, cluster, command, fields) = native_group_params(&lv).unwrap().unwrap();
        assert_eq!(cluster, im::CLUSTER_LEVEL_CONTROL);
        assert_eq!(command, im::CMD_MOVE_TO_LEVEL);
        assert_eq!(fields.unwrap(), im::encode_move_to_level_fields(254, 0));
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p matd --lib`
Expected: コンパイルエラー（`Op::Level` variant 不在）（= RED）

- [ ] **Step 3: 実装**

`protocol.rs` — `Op::ColorTemp` の直後に:

```rust
    /// LevelControl MoveToLevel のショートカット（`mat level` 相当）。
    /// `level` は mat 側で換算済みの 0–254 生値。`percent` は応答へのエコー用
    /// （換算は mat の 1 箇所に置く — color_temp の mireds/kelvin と同じ約束）。
    Level {
        node_id: u64,
        endpoint: u16,
        level: u8,
        percent: u8,
        #[serde(default)]
        transition: u16,
    },
```

`Op::GroupColorTemp` の直後に:

```rust
    /// LevelControl MoveToLevel の group ショートカット（`mat group level`
    /// 相当、groupcast）。`level` は mat 側で換算済み、`percent` はエコー用。
    /// unacknowledged なので "sent" のみ報告する。
    GroupLevel {
        group_id: u16,
        level: u8,
        percent: u8,
        #[serde(default)]
        transition: u16,
        endpoint: u16,
    },
```

`node_id()` の match: `| Op::ColorTemp { node_id, .. }` の並びに `| Op::Level { node_id, .. }` を、`| Op::GroupColorTemp { .. }` の並びに `| Op::GroupLevel { .. }` を追加。

`native.rs` — `use mat_controller::im::{...}` に `CLUSTER_LEVEL_CONTROL, CMD_MOVE_TO_LEVEL` を追加し、`color_temp` メソッドの直後に:

```rust
    pub async fn level(
        &self,
        node_id: u64,
        endpoint: u16,
        level: u8,
        transition: u16,
    ) -> Result<(), MatError> {
        let fields = im::encode_move_to_level_fields(level, transition);
        self.with_session(node_id, move |c| {
            c.invoke(
                endpoint,
                CLUSTER_LEVEL_CONTROL,
                CMD_MOVE_TO_LEVEL,
                Some(fields.clone()),
                false,
            )
        })
        .await
    }
```

`server.rs` — 4 箇所:

(a) `is_native_hotpath` の常時 true 群に `| Op::Level { .. }` を追加。

(b) `native_group_params` の `GroupColorTemp` arm の直後に:

```rust
        Op::GroupLevel {
            group_id,
            level,
            transition,
            ..
        } => Some(Ok((
            *group_id,
            im::CLUSTER_LEVEL_CONTROL,
            im::CMD_MOVE_TO_LEVEL,
            Some(im::encode_move_to_level_fields(*level, *transition)),
        ))),
```

(c) `native_op` の `Op::ColorTemp` arm の直後に:

```rust
        Op::Level {
            node_id,
            endpoint,
            level,
            transition,
            ..
        } => {
            native.level(*node_id, *endpoint, *level, *transition).await?;
            Ok(hotpath_success_body(op, None))
        }
```

(d) `hotpath_success_body` の `Op::ColorTemp` arm の直後に:

```rust
        Op::Level {
            node_id,
            endpoint,
            level,
            percent,
            transition,
        } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": "levelcontrol", "command": "move-to-level",
            // 換算後 level と入力 percent を両方エコー（読み返し突合用; 直経路と同形）。
            "percent": percent, "level": level, "transition": transition,
            "status": "success",
        }),
```

(e) `group_sent_body` の `Op::GroupColorTemp` arm の直後に:

```rust
        Op::GroupLevel {
            group_id,
            level,
            percent,
            transition,
            endpoint,
        } => json!({
            "group_id": group_id, "cluster": "levelcontrol",
            "command": "move-to-level",
            "percent": percent, "level": level, "transition": transition,
            "endpoint": endpoint, "status": "sent",
            "note": "unacknowledged groupcast; per-device delivery not confirmed",
        }),
```

（既存 match に網羅性エラーが出る箇所があれば、上記と同じ ColorTemp/GroupColorTemp の sibling 配置で追随する。）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: 全 PASS（新テスト含む）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/matd/src/protocol.rs crates/matd/src/native.rs crates/matd/src/server.rs
git commit -m "feat(matd): level / group_level op を追加（native hotpath + groupcast） (level shortcut Task2)"
```

---

### Task 3: mat unicast — CLI・換算・直経路・matd_client

**Files:**
- Modify: `crates/mat/src/cli.rs`（`ColorTemp` の直後 ~210 行に `Level` を追加）
- Modify: `crates/mat/src/commands/invoke.rs`（`resolve_color_temp` の直後に `resolve_level`、`emit_color_temp_success` の直後に `emit_level_success`）
- Modify: `crates/mat/src/native_direct.rs`（`NativeOp` variant / `classify` / `execute` の node_id match / `run_op`）
- Modify: `crates/mat/src/matd_client.rs`（`to_op` の `Command::ColorTemp` arm の直後）
- Test: `crates/mat/tests/integration.rs`、`crates/mat/tests/matd_auto.rs`

**Interfaces:**
- Consumes: Task 1 の `im::CLUSTER_LEVEL_CONTROL` / `im::CMD_MOVE_TO_LEVEL` / `im::encode_move_to_level_fields`、Task 2 の wire 名 `"op":"level"`（フィールド: node_id, endpoint, level, percent, transition）
- Produces: `Command::Level { node_id: NodeRef, endpoint: EndpointRef, percent: u8, transition: u16 }`、`pub(crate) fn resolve_level(percent: u8) -> u8`、`pub(crate) fn emit_level_success(node_id: u64, endpoint: u16, percent: u8, level: u8, transition: u16)`（Task 4 の group 版が sibling として参照）

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/tests/integration.rs`（`color_temp_unknown_node_exits_11` の直後）:

```rust
#[test]
fn level_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
        .args(["level", "--node", "99", "--percent", "50"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}

#[test]
fn level_percent_out_of_range_exits_2() {
    let store = store_with_node5();
    mat(store.path())
        .args(["level", "--node", "5", "--percent", "101"])
        .assert()
        .code(2);
    // --percent は必須。
    mat(store.path())
        .args(["level", "--node", "5"])
        .assert()
        .code(2);
}
```

`crates/mat/tests/matd_auto.rs`（`auto_routes_color_temp_with_converted_mireds` の直後）:

```rust
#[test]
fn auto_routes_level_with_converted_raw_value() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd(socket.clone());

    mat_auto(store.path(), &socket)
        .args(["level", "--node", "5", "--percent", "50"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"via\":\"fake-matd\""));

    // 換算（50% → 127）は mat 側で済んだ状態で matd に届く。
    let req = matd.join().unwrap();
    assert!(req.contains("\"op\":\"level\""), "request line: {req}");
    assert!(req.contains("\"level\":127"), "request line: {req}");
    assert!(req.contains("\"percent\":50"), "request line: {req}");
}
```

`crates/mat/src/commands/invoke.rs` のテストモジュール（`resolve_color_temp` のテストがあれば直後、無ければ新設）に換算の単体テスト:

```rust
    #[test]
    fn resolve_level_rounds_percent_to_254_scale() {
        // round(percent / 100 * 254)。255 は予約値なので 100% は 254。
        assert_eq!(resolve_level(0), 0);
        assert_eq!(resolve_level(1), 3);
        assert_eq!(resolve_level(50), 127);
        assert_eq!(resolve_level(100), 254);
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat`
Expected: コンパイルエラー（`resolve_level` 不在 / CLI に `level` サブコマンド不在で integration が FAIL）（= RED）

- [ ] **Step 3: 実装**

`cli.rs` — `ColorTemp` variant の直後に:

```rust
    /// LevelControl の MoveToLevel を invoke する高頻度ショートカット（明るさ）。
    /// `--percent`（0–100）を Matter の 0–254 生値（`round(percent / 100 * 254)`、
    /// 255 は予約値）へ mat が換算する。0 は消灯相当（挙動はデバイス依存）。
    /// デバイス対応範囲（min/max level）外はデバイス側が clamp する
    /// （mat は事前 read / 検証をしない）。
    Level {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 1）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "1")]
        endpoint: EndpointRef,
        /// 明るさ（%）。0–100。
        #[arg(long, value_name = "PCT", value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
        /// 遷移時間（0.1 秒単位、既定 0 = 即時）。例: 30 = 3 秒。
        #[arg(long, value_name = "DS", default_value_t = 0)]
        transition: u16,
    },
```

`commands/invoke.rs` — `resolve_color_temp` の直後に:

```rust
/// `mat level` の `--percent`（0–100）を Matter LevelControl の 0–254 生値へ
/// 換算する（`color` の hue/sat と同じ整数換算: round(v / full * 254)、255 は
/// 予約値）。デバイス対応範囲（min/max level）の検証はしない（範囲外は
/// デバイス側が clamp する）。
pub(crate) fn resolve_level(percent: u8) -> u8 {
    ((u32::from(percent) * 254 + 50) / 100) as u8
}
```

`emit_color_temp_success` の直後に:

```rust
/// `level` の成功 JSON を stdout へ emit する（native 直経路の単一ソース）。
/// 出力には入力の percent と換算後の level を両方載せ、`current-level` の
/// 読み返しと突合しやすくする。
pub(crate) fn emit_level_success(
    node_id: u64,
    endpoint: u16,
    percent: u8,
    level: u8,
    transition: u16,
) {
    output::emit(json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": percent,
        "level": level,
        "transition": transition,
        "status": "success",
    }));
}
```

`native_direct.rs` — `NativeOp::ColorTemp` variant の直後に:

```rust
    Level {
        node_id: u64,
        endpoint: u16,
        percent: u8,
        level: u8,
        transition: u16,
    },
```

`classify` の `Command::ColorTemp` arm の直後に:

```rust
        Command::Level {
            node_id,
            endpoint,
            percent,
            transition,
        } => {
            let level = crate::commands::invoke::resolve_level(*percent);
            Some(NativeOp::Level {
                node_id: node_id.id(),
                endpoint: endpoint.id(),
                percent: *percent,
                level,
                transition: *transition,
            })
        }
```

`execute` の node_id match の `| NativeOp::ColorTemp { node_id, .. }` の並びに `| NativeOp::Level { node_id, .. }` を追加。

`run_op` の `NativeOp::ColorTemp` arm の直後に:

```rust
        NativeOp::Level {
            node_id,
            endpoint,
            percent,
            level,
            transition,
        } => {
            let fields = im::encode_move_to_level_fields(*level, *transition);
            let mut conn = engine.establisher.establish(*node_id).await?;
            conn.invoke(
                *endpoint,
                im::CLUSTER_LEVEL_CONTROL,
                im::CMD_MOVE_TO_LEVEL,
                Some(fields),
                false,
            )
            .await?;
            tracing::info!(
                node_id,
                cluster = "levelcontrol",
                command = "move-to-level",
                "invoke executed (native direct)"
            );
            crate::commands::invoke::emit_level_success(
                *node_id,
                *endpoint,
                *percent,
                *level,
                *transition,
            );
        }
```

`matd_client.rs` — `to_op` の `Command::ColorTemp` arm の直後に:

```rust
        Command::Level {
            node_id,
            endpoint,
            percent,
            transition,
        } => {
            // 換算は mat 側で 1 箇所（直経路と同じ規則）。matd へは換算済み level を
            // 渡し、percent は応答エコー用。
            let level = crate::commands::invoke::resolve_level(*percent);
            json!({
                "op": "level", "node_id": node_id.id(), "endpoint": endpoint.id(),
                "level": level, "percent": percent, "transition": transition,
            })
        }
```

`Command` への variant 追加で他の網羅 match（`resolve::resolve_command` の alias 解決 arm 等）にコンパイルエラーが出たら、ColorTemp の sibling 配置で追随する（node/endpoint の alias 解決は ColorTemp と同一の書き方）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat`
Expected: 全 PASS（新テスト 4 本含む）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat/src/cli.rs crates/mat/src/commands/invoke.rs crates/mat/src/native_direct.rs crates/mat/src/matd_client.rs crates/mat/tests/integration.rs crates/mat/tests/matd_auto.rs
git commit -m "feat(mat): mat level --percent（unicast、%→0-254換算はCLI層） (level shortcut Task3)"
```

（`resolve_command` 等、追随で触ったファイルがあれば `git add` に含める。）

---

### Task 4: mat group — `mat group level`

**Files:**
- Modify: `crates/mat/src/cli.rs`（`GroupCommand::ColorTemp` の直後）
- Modify: `crates/mat/src/commands/group.rs`（`emit_color_temp_sent` の直後）
- Modify: `crates/mat/src/native_direct.rs`（`NativeOp::GroupColorTemp` sibling 一式）
- Modify: `crates/mat/src/matd_client.rs`（`GroupCommand::ColorTemp` arm の直後）
- Test: `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: Task 3 の `resolve_level`、Task 1 の im 定数/エンコーダ、Task 2 の wire 名 `"op":"group_level"`（フィールド: group_id, level, percent, transition, endpoint）
- Produces: `GroupCommand::Level { group_id: GroupRef, percent: u8, transition: u16, endpoint: u16 }`、`pub(crate) fn emit_level_sent(group_id: u16, percent: u8, level: u8, transition: u16, endpoint: u16)`

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/tests/integration.rs`（`group_color_temp_requires_exactly_one_of_kelvin_or_mireds` の近く）:

```rust
#[test]
fn group_level_percent_out_of_range_exits_2() {
    let store = store_with_node5();
    mat(store.path())
        .args(["group", "level", "--group", "10", "--percent", "101"])
        .assert()
        .code(2);
    mat(store.path())
        .args(["group", "level", "--group", "10"])
        .assert()
        .code(2);
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat --test integration group_level`
Expected: FAIL（`level` は `group` のサブコマンドに無い → clap が exit 2 を返すが「unrecognized subcommand」であることを確認。テストとしては code(2) が通ってしまう可能性があるため、Step 2 では `mat group level --group 10 --percent 50` を手で実行し「unrecognized subcommand」エラーを確認してから進む）

Run: `cargo run -p mat -- group level --group 10 --percent 50 2>&1 | head -3`
Expected: `unrecognized subcommand`（= 機能不在の確認）

- [ ] **Step 3: 実装**

`cli.rs` — `GroupCommand::ColorTemp` の直後に:

```rust
    /// LevelControl MoveToLevel を group へ multicast する高頻度ショートカット
    /// （`mat level` の group 版）。`--percent`（0–100）を 0–254 生値へ換算。
    /// unacknowledged groupcast なので "sent" のみ報告する。点灯中でないと
    /// 反映されない（ExecuteIfOff は立てない）。
    Level {
        /// Matter GroupId、または aliases.toml の group alias。
        #[arg(short = 'g', long = "group", value_name = "ID|ALIAS")]
        group_id: GroupRef,
        /// 明るさ（%）。0–100。
        #[arg(long, value_name = "PCT", value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
        /// 遷移時間（0.1 秒単位、既定 0 = 即時）。例: 30 = 3 秒。
        #[arg(long, value_name = "DS", default_value_t = 0)]
        transition: u16,
        /// 宛先エンドポイント（既定 1、数値のみ — ノード文脈が無いため alias 不可）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },
```

`commands/group.rs` — `emit_color_temp_sent` の直後に:

```rust
/// `level` の出力部（native 直経路の単一ソース）。
pub(crate) fn emit_level_sent(
    group_id: u16,
    percent: u8,
    level: u8,
    transition: u16,
    endpoint: u16,
) {
    output::emit(json!({
        "group_id": group_id,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": percent,
        "level": level,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "note": "unacknowledged groupcast; per-device delivery not confirmed",
    }));
}
```

`native_direct.rs` — `NativeOp::GroupColorTemp` variant の直後に:

```rust
    GroupLevel {
        group_id: u16,
        percent: u8,
        level: u8,
        transition: u16,
        endpoint: u16,
    },
```

`classify` の group `ColorTemp` arm の直後に:

```rust
        Command::Group {
            action:
                GroupCommand::Level {
                    group_id,
                    percent,
                    transition,
                    endpoint,
                },
        } => {
            let level = crate::commands::invoke::resolve_level(*percent);
            Some(NativeOp::GroupLevel {
                group_id: group_id.id(),
                percent: *percent,
                level,
                transition: *transition,
                endpoint: *endpoint,
            })
        }
```

`execute` の node_id match の `| NativeOp::GroupColorTemp { .. }` の並びに `| NativeOp::GroupLevel { .. }` を追加。

`run_op` の `NativeOp::GroupColorTemp` arm の直後に:

```rust
        NativeOp::GroupLevel {
            group_id,
            percent,
            level,
            transition,
            endpoint,
        } => {
            let Some(ctx) = &engine.group else {
                return Err(group_ctx_unconfigured_error());
            };
            let fields = im::encode_move_to_level_fields(*level, *transition);
            match mat_native::group::send(
                ctx,
                *group_id,
                im::CLUSTER_LEVEL_CONTROL,
                im::CMD_MOVE_TO_LEVEL,
                Some(fields),
            )
            .await?
            {
                GroupOutcome::Sent => {
                    crate::commands::group::emit_level_sent(
                        *group_id,
                        *percent,
                        *level,
                        *transition,
                        *endpoint,
                    );
                }
                GroupOutcome::Unavailable(reason) => {
                    return Err(group_unavailable_error(&reason));
                }
            }
        }
```

`matd_client.rs` — `GroupCommand::ColorTemp` arm の直後に:

```rust
            GroupCommand::Level {
                group_id,
                percent,
                transition,
                endpoint,
            } => {
                // 換算は mat 側で 1 箇所（直経路と同じ規則）。percent はエコー用。
                let level = crate::commands::invoke::resolve_level(*percent);
                json!({
                    "op": "group_level", "group_id": group_id.id(),
                    "level": level, "percent": percent,
                    "transition": transition, "endpoint": endpoint,
                })
            }
```

他の `GroupCommand` 網羅 match（alias 解決等）にコンパイルエラーが出たら ColorTemp sibling で追随。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat`
Expected: 全 PASS

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat/src/cli.rs crates/mat/src/commands/group.rs crates/mat/src/native_direct.rs crates/mat/src/matd_client.rs crates/mat/tests/integration.rs
git commit -m "feat(mat): mat group level --percent（groupcast 専用形） (level shortcut Task4)"
```

---

### Task 5: ドキュメント + sibling 全数確認

**Files:**
- Modify: `README.md`（ショートカット使用例 ~202-209 / 出力例 ~249-252 / group ショートカット ~515-526 / matd 対応リスト ~594-599 / alias 受理列挙 ~745-757）
- Modify: `crates/mat/src/cli.rs`（module doc 4 行目・30 行目）

**Interfaces:**
- Consumes: Task 3/4 の CLI 形（`mat level --node 5 --percent 50` / `mat group level --group 1 --percent 100`）と出力 body

- [ ] **Step 1: README 更新**

(a) ショートカット使用例（color-temp 例の直後）:

```
# Brightness (LevelControl MoveToLevel): give a percentage (0-100) and mat
# converts to the raw 0-254 level (round(percent / 100 * 254); 255 is
# reserved). --transition is in tenths of a second (30 = 3 s, default 0).
# Values outside the device's supported range are clamped by the device
# itself (mat does not pre-read or validate).
mat level --node 5 --percent 50
mat level --node 5 --percent 100 --transition 30
```

(b) 出力例（color-temp 出力例の直後）:

```
// level — echoes both the input percent and the converted raw level so the
// result can be cross-checked against a `current-level` read
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "levelcontrol", "command": "move-to-level", "percent": 50, "level": 127, "transition": 0, "status": "success" }
```

(c) group ショートカット節: 先頭の説明文を「Color shortcuts」から「Color / brightness shortcuts」に広げ、bash 例に追加:

```bash
mat group level --group 1 --percent 100
```

（「optionsMask=0 のため点灯中のデバイスにのみ効く」の既存注記が level にも掛かる書き方に調整。）

(d) matd 対応リスト: `on` / `off` / `color-temp` / `color` の並びに `level` を、group 括弧内 (`provision` / `invoke` / `color-temp` / `color`) に `level` を追加。

(e) alias 受理列挙: `-n/--node` 受理コマンド列に `level` を、group alias 受理列挙（`color-temp` / `color` の並び）に `level` を、`-e` numeric-only 列挙（`group color-temp` / `group color` の並び）に `group level` を追加。

(f) `cli.rs` module doc: 4 行目の `color-temp` / `color` 列挙と 30 行目の matd 対応リストに `level` を追加。

- [ ] **Step 2: sibling 全数確認（0.23.1 の教訓）**

Run: `grep -rn "color-temp\|color_temp\|ColorTemp" --include="*.rs" crates/ README.md | grep -v test | wc -l` で出現箇所を列挙し、各箇所に level の sibling が存在するか（または意図的に不要か — 例: kelvin/mireds 固有の換算関数）を 1 件ずつ確認。漏れがあれば該当タスクの形で追加する。確認結果（対応済み/不要の別）をコミットメッセージに 1 行で要約。

- [ ] **Step 3: `task check` → コミット**

```bash
task check
git add README.md crates/mat/src/cli.rs
git commit -m "docs: level ショートカットを README / cli doc に反映 + sibling 全数確認 (level shortcut Task5)"
```

---

## 完了後（メインセッションが実施）

1. requesting-code-review でブランチ全体レビュー（Base = worktree 分岐点、Head = Task 5 のコミット）。
2. Critical/Important を fix 後、ユーザーへマージ提案（0.24.0 — 機能追加なので minor bump）。
3. マージ後: Issue #10 クローズ（`gh issue close 10 --comment`）。mando 側 `sh -c` ラッパー撤去は別作業として Issue #10 のクローズコメントに明記。
