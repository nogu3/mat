# `mat color` コマンド実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ColorControl の MoveToHueAndSaturation を invoke する高頻度ショートカット `mat color --node <N|ALIAS> --hue <DEG> --sat <PCT> [--transition <DS>]` を、直経路・matd 経路の両方に追加する。

**Architecture:** 既存の `mat color-temp` の完全な相似形。CLI（clap）→ alias 解決（resolve.rs）→ 経路分岐（matd_client / 直 chip-tool）の各層に `Color` を追加し、単位換算（度→0–254、%→0–254)は mat 側の 1 関数 `resolve_color` に集約する。matd へは換算済み生値＋エコー用の度/% を併送し、matd 側では逆算しない（丸めズレ防止、color_temp と同じ設計判断）。

**Tech Stack:** Rust / clap(derive) / serde_json / assert_cmd + predicates + tempfile（テスト）/ fake-chip-tool.sh（統合テスト）

## Global Constraints

- stdout は純粋な構造化 JSON のみ。`timestamp` 必須（`output::emit` が自動付与）。
- chip-tool 引数は `colorcontrol move-to-hue-and-saturation <hue> <sat> <transition> 0 0 <node_id> <endpoint>`（末尾 2 つの 0 は optionsMask / optionsOverride 固定、宛先は**末尾**）。
- 換算式: `hue_raw = round(hue / 360 * 254)`、`sat_raw = round(sat / 100 * 254)`。255 は Matter の予約値なので上限は 254（360° / 100% がちょうど 254.5 → 254 に丸まる）。
- `--hue`（0–360 度）と `--sat`（0–100 %）は**両方必須**。値域は clap で検証（exit 2）、デバイス対応範囲の事前 read はしない（範囲外はデバイス側 clamp）。
- `--transition` は 0.1 秒単位、既定 0。
- matd 対応 op に `color` を追加（`--matd` 強制時も通る）。
- 各タスク末尾のコミット前に `task check`（fmt:check + clippy -D warnings + test）を通すこと。
- コミットメッセージ末尾: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`

## 出力スキーマ（直経路・matd 経路で同形）

```json
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "colorcontrol", "command": "move-to-hue-and-saturation", "hue": 330, "saturation": 80, "hue_raw": 233, "saturation_raw": 203, "transition": 0, "status": "success" }
```

`hue` / `saturation` は入力の度・%、`hue_raw` / `saturation_raw` は chip-tool へ渡した 0–254 生値。color-temp が kelvin（入力）と mireds（wire 値）を両方エコーするのと同じ「読み返し突合」思想で、`current-hue` / `current-saturation` の read と直接比較できる。

## matd op スキーマ

```json
{ "op": "color", "node_id": 6, "endpoint": 1, "hue_raw": 233, "saturation_raw": 203, "hue": 330, "saturation": 80, "transition": 30 }
```

換算は mat 側で完了済み。`hue` / `saturation`（度・%）は応答エコー用。

---

### Task 1: matd — `color` op（protocol + server）

**Files:**
- Modify: `crates/matd/src/protocol.rs`（`Op` enum に `Color` 追加、`node_id()` / `to_cmdline()` の arm、tests mod にテスト追加）
- Modify: `crates/matd/src/server.rs:240` 付近（`Op::ColorTemp` の直後に `Op::Color` の応答 body を追加）
- Test: `crates/matd/tests/integration.rs`（`color_temp_echoes_kelvin_and_mireds` の直後にテスト追加）

**Interfaces:**
- Consumes: なし（mat crate に依存しない独立タスク）
- Produces: matd socket op `"color"`（上記 matd op スキーマ）。応答は出力スキーマ節の JSON（`timestamp` / `id` エコーはサーバ共通処理が付与）。

- [ ] **Step 1: protocol.rs にユニットテストを書く（失敗確認用）**

`crates/matd/src/protocol.rs` の tests mod、`color_temp_shortcut_builds_move_to_color_temperature_cmdline` の直後に追加:

```rust
#[test]
fn color_shortcut_builds_move_to_hue_and_saturation_cmdline() {
    // hue_raw / saturation_raw は mat 側で換算済みの 0–254 値。hue / saturation
    // （度・%）は応答エコー用で cmdline には乗らない。
    let r = parse(
        r#"{"op":"color","node_id":6,"endpoint":1,"hue_raw":233,"saturation_raw":203,"hue":330,"saturation":80,"transition":30}"#,
    );
    assert_eq!(r.op.node_id(), Some(6));
    assert_eq!(
        r.op.to_cmdline().unwrap(),
        "colorcontrol move-to-hue-and-saturation 233 203 30 0 0 6 1"
    );
}
```

- [ ] **Step 2: テストが失敗（コンパイルエラー）することを確認**

Run: `cargo test -p matd color_shortcut`
Expected: FAIL — `unknown variant \`color\``（デシリアライズエラー）またはコンパイルエラー

- [ ] **Step 3: `Op::Color` variant と各 arm を実装**

`crates/matd/src/protocol.rs` の `Op` enum、`ColorTemp` variant の直後に追加:

```rust
    /// ColorControl MoveToHueAndSaturation のショートカット（`mat color` 相当）。
    /// `hue_raw` / `saturation_raw` は mat 側で換算済みの 0–254 値を受け取る。
    /// `hue`（度）/ `saturation`（%）は応答へのエコー用
    /// （matd 側で逆算すると丸めで入力とずれるため、換算は mat の 1 箇所に置く）。
    Color {
        node_id: u64,
        endpoint: u16,
        hue_raw: u8,
        saturation_raw: u8,
        hue: u16,
        saturation: u8,
        #[serde(default)]
        transition: u16,
    },
```

`node_id()` の or パターンに `| Op::Color { node_id, .. }` を追加（`Op::ColorTemp { node_id, .. }` の直後）。

`to_cmdline()` の match、`Op::ColorTemp` arm の直後に追加:

```rust
            // 引数は <hue> <saturation> <transition> <optionsMask> <optionsOverride>、宛先は末尾。
            Op::Color {
                node_id,
                endpoint,
                hue_raw,
                saturation_raw,
                transition,
                ..
            } => format!(
                "colorcontrol move-to-hue-and-saturation {hue_raw} {saturation_raw} {transition} 0 0 {node_id} {endpoint}"
            ),
```

`crates/matd/src/server.rs` の応答 body match、`Op::ColorTemp` arm（240 行付近）の直後に追加:

```rust
        Op::Color {
            node_id,
            endpoint,
            hue_raw,
            saturation_raw,
            hue,
            saturation,
            transition,
        } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": "colorcontrol", "command": "move-to-hue-and-saturation",
            // 入力の度 / % と換算後 0–254 生値を両方エコー（読み返し突合用; 直経路と同形）。
            "hue": hue, "saturation": saturation,
            "hue_raw": hue_raw, "saturation_raw": saturation_raw,
            "transition": transition,
            "status": "success",
        }),
```

注意: server.rs のこの match は網羅なので、variant 追加時点で `Op::Color` arm が無いとコンパイルエラーになる（それが正しい誘導）。

- [ ] **Step 4: ユニットテストが通ることを確認**

Run: `cargo test -p matd color_shortcut`
Expected: PASS

- [ ] **Step 5: matd 統合テストを書く**

`crates/matd/tests/integration.rs` の `color_temp_echoes_kelvin_and_mireds`（272 行付近）の直後に追加:

```rust
/// color: ColorControl MoveToHueAndSaturation にマップされ、hue / saturation
/// （度・% と換算済み 0–254 生値）を応答へエコーする（直経路 `mat color` と同形）。
#[tokio::test]
async fn color_echoes_hue_and_saturation() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[
            json!({"id":1,"op":"color","node_id":1,"endpoint":1,"hue_raw":233,"saturation_raw":203,"hue":330,"saturation":80,"transition":30}),
            json!({"op":"color","node_id":99,"endpoint":1,"hue_raw":233,"saturation_raw":203,"hue":330,"saturation":80}),
        ],
    )
    .await;

    let r = &resps[0];
    assert_eq!(r["id"], json!(1));
    assert_eq!(r["cluster"], "colorcontrol");
    assert_eq!(r["command"], "move-to-hue-and-saturation");
    assert_eq!(r["hue"], json!(330));
    assert_eq!(r["saturation"], json!(80));
    assert_eq!(r["hue_raw"], json!(233));
    assert_eq!(r["saturation_raw"], json!(203));
    assert_eq!(r["transition"], json!(30));
    assert_eq!(r["status"], "success");
    assert!(r.get("result").is_none(), "raw ws result must not leak");

    // 未 commission node は他 op 同様 node_not_commissioned。
    assert_eq!(resps[1]["error"]["kind"], "node_not_commissioned");

    handle.abort();
}
```

- [ ] **Step 6: 統合テストが通ることを確認**

Run: `cargo test -p matd color_echoes`
Expected: PASS

- [ ] **Step 7: 全体チェックしてコミット**

Run: `task check`
Expected: fmt:check / clippy / test すべて成功

```bash
git add crates/matd/src/protocol.rs crates/matd/src/server.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): color op（MoveToHueAndSaturation ショートカット）を追加

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: mat — `color` サブコマンド（換算 + CLI + 両経路配線）

**Files:**
- Modify: `crates/mat/src/commands/invoke.rs`（`resolve_color` 換算関数 + `run_color` + ユニットテスト）
- Modify: `crates/mat/src/cli.rs`（`Command::Color` variant 追加、モジュール doc とmatd 対応リストの doc comment 更新）
- Modify: `crates/mat/src/resolve.rs`（alias 解決 arm 追加）
- Modify: `crates/mat/src/main.rs`（ディスパッチ arm 追加）
- Modify: `crates/mat/src/matd_client.rs`（`to_op` arm + ユニットテスト追加）
- Test: `crates/mat/tests/integration.rs`（直経路の統合テスト、`color_temp_unknown_node_exits_11` の直後）
- Test: `crates/mat/tests/matd_auto.rs`（自動検出経路のテスト、`auto_routes_color_temp_with_converted_mireds` の直後）

**Interfaces:**
- Consumes: Task 1 の matd op `"color"` スキーマ（テストは fake matd なので Task 1 未完でもコンパイル・テストは通るが、スキーマは一致させること）
- Produces:
  - `commands::invoke::resolve_color(hue_deg: u16, sat_pct: u8) -> (u8, u8)` — `(hue_raw, sat_raw)` を返す
  - `commands::invoke::run_color(store_path: &Path, node_id: u64, endpoint: u16, hue_deg: u16, sat_pct: u8, transition: u16) -> Result<(), MatError>`
  - CLI: `mat color -n <N|ALIAS> [-e <EP|ALIAS>] --hue <DEG> --sat <PCT> [--transition <DS>]`

- [ ] **Step 1: `resolve_color` のユニットテストを書く（失敗確認用）**

`crates/mat/src/commands/invoke.rs` の tests mod 末尾に追加:

```rust
    #[test]
    fn hue_330_sat_80_convert_to_233_203() {
        // round(330 / 360 * 254) = 233、round(80 / 100 * 254) = 203。
        assert_eq!(resolve_color(330, 80), (233, 203));
    }

    #[test]
    fn hue_sat_full_scale_caps_at_254() {
        // 255 は Matter の予約値。360° / 100% は 254.5 → 254 に丸まり超えない。
        assert_eq!(resolve_color(0, 0), (0, 0));
        assert_eq!(resolve_color(360, 100), (254, 254));
    }

    #[test]
    fn sat_50_rounds_to_127() {
        // round(50 / 100 * 254) = 127（パステル系の中間彩度）。
        assert_eq!(resolve_color(330, 50), (233, 127));
    }
```

- [ ] **Step 2: テストが失敗（コンパイルエラー）することを確認**

Run: `cargo test -p mat resolve_color 2>&1 | head -20`（`hue_330` などテスト名でも可）
Expected: FAIL — `cannot find function \`resolve_color\``

- [ ] **Step 3: `resolve_color` と `run_color` を実装**

`crates/mat/src/commands/invoke.rs` の `resolve_color_temp` の直後に追加:

```rust
/// `mat color` の実体。ColorControl の MoveToHueAndSaturation を invoke する。
/// 入力の度 / % と換算後の 0–254 生値を両方エコーし、`current-hue` /
/// `current-saturation` の読み返しと突合しやすくする。
pub fn run_color(
    store_path: &Path,
    node_id: u64,
    endpoint: u16,
    hue_deg: u16,
    sat_pct: u8,
    transition: u16,
) -> Result<(), MatError> {
    let (hue_raw, sat_raw) = resolve_color(hue_deg, sat_pct);
    // MoveToHueAndSaturation の引数は <hue> <saturation> <transition>
    // <optionsMask> <optionsOverride>。
    let args = [
        hue_raw.to_string(),
        sat_raw.to_string(),
        transition.to_string(),
        "0".to_string(),
        "0".to_string(),
    ];
    execute(
        store_path,
        node_id,
        endpoint,
        "colorcontrol",
        "move-to-hue-and-saturation",
        &args,
    )?;
    output::emit(json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "colorcontrol",
        "command": "move-to-hue-and-saturation",
        "hue": hue_deg,
        "saturation": sat_pct,
        "hue_raw": hue_raw,
        "saturation_raw": sat_raw,
        "transition": transition,
        "status": "success",
    }));
    Ok(())
}

/// `mat color` の `--hue`（0–360 度）/ `--sat`（0–100 %）を Matter の 0–254 値へ
/// 換算する（255 は予約値、フルスケールは 254.5 → 254 に丸まる）。値域は clap が
/// 保証する。決定的な数値換算のみで、デバイス対応範囲の検証はしない
/// （範囲外はデバイス側が clamp する）。
pub fn resolve_color(hue_deg: u16, sat_pct: u8) -> (u8, u8) {
    // round(v / full * 254) を整数演算で（+full/2 で四捨五入）。
    fn scale(v: u32, full: u32) -> u8 {
        ((v * 254 + full / 2) / full) as u8
    }
    (
        scale(u32::from(hue_deg), 360),
        scale(u32::from(sat_pct), 100),
    )
}
```

注意: この時点では `run_color` / `resolve_color` が未使用で dead_code 警告が出るが、Step 5 以降で配線されるので `task check` はタスク末尾（Step 12）まで走らせない。

- [ ] **Step 4: ユニットテストが通ることを確認**

Run: `cargo test -p mat hue_330 && cargo test -p mat hue_sat_full && cargo test -p mat sat_50`
Expected: PASS（3 テストとも）

- [ ] **Step 5: CLI に `Color` variant を追加**

`crates/mat/src/cli.rs` の `ColorTemp` variant（172 行 `},` ）の直後に追加:

```rust
    /// ColorControl の MoveToHueAndSaturation を invoke する高頻度ショートカット。
    /// `--hue`（0–360 度）と `--sat`（0–100 %）は両方必須で、mat が Matter の
    /// 0–254 値（`round(v / full * 254)`、255 は予約値）へ換算する。デバイス対応
    /// 範囲外の値はデバイス側が clamp する（mat は事前 read / 検証をしない）。
    Color {
        /// commission 済みノードの node_id、または aliases.toml の node alias。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: NodeRef,
        /// エンドポイント番号、または aliases.toml の endpoint alias（既定 1）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS", default_value = "1")]
        endpoint: EndpointRef,
        /// 色相（度、0–360）。例: 330 = ピンク。
        #[arg(long, value_name = "DEG", value_parser = clap::value_parser!(u16).range(0..=360))]
        hue: u16,
        /// 彩度（%、0–100）。
        #[arg(long, value_name = "PCT", value_parser = clap::value_parser!(u8).range(0..=100))]
        sat: u8,
        /// 遷移時間（0.1 秒単位、既定 0 = 即時）。例: 30 = 3 秒。
        #[arg(long, value_name = "DS", default_value_t = 0)]
        transition: u16,
    },
```

あわせて doc comment 2 箇所を更新:
- `crates/mat/src/cli.rs:4` — `（後追いの高頻度ショートカットとして \`color-temp\` も）` → `（後追いの高頻度ショートカットとして \`color-temp\` / \`color\` も）`
- `crates/mat/src/cli.rs:30` — `matd 対応は read/write/invoke/on/off/color-temp/describe/group のみ` → `matd 対応は read/write/invoke/on/off/color-temp/color/describe/group のみ`

- [ ] **Step 6: コンパイルエラーで網羅 match の未対応箇所を確認**

Run: `cargo build -p mat 2>&1 | grep -A2 "non-exhaustive\|not covered"`
Expected: `resolve.rs` / `main.rs` / `matd_client.rs` の match で `Color` not covered エラー（網羅 match が考慮漏れを弾く設計どおり）

- [ ] **Step 7: resolve.rs / main.rs / matd_client.rs に arm を追加**

`crates/mat/src/resolve.rs` の `Command::ColorTemp` arm（125 行 `}` ）の直後に追加:

```rust
        Command::Color {
            node_id,
            endpoint,
            hue,
            sat,
            transition,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Color {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                hue,
                sat,
                transition,
            }
        }
```

`crates/mat/src/main.rs` の `Command::ColorTemp` arm（130 行 `}` ）の直後に追加:

```rust
        Command::Color {
            node_id,
            endpoint,
            hue,
            sat,
            transition,
        } => commands::invoke::run_color(
            &store_path,
            node_id.id(),
            endpoint.id(),
            *hue,
            *sat,
            *transition,
        ),
```

`crates/mat/src/matd_client.rs` の `to_op` 内 `Command::ColorTemp` arm（198 行 `}` ）の直後に追加:

```rust
        Command::Color {
            node_id,
            endpoint,
            hue,
            sat,
            transition,
        } => {
            // 換算は mat 側で 1 箇所（直経路と同じ規則）。matd へは換算済み 0–254 値を
            // 渡し、度 / % は応答エコー用（matd 側で逆算すると丸めで入力とずれる）。
            let (hue_raw, sat_raw) = crate::commands::invoke::resolve_color(*hue, *sat);
            json!({
                "op": "color", "node_id": node_id.id(), "endpoint": endpoint.id(),
                "hue_raw": hue_raw, "saturation_raw": sat_raw,
                "hue": hue, "saturation": sat, "transition": transition,
            })
        }
```

- [ ] **Step 8: ビルドが通ることを確認**

Run: `cargo build -p mat`
Expected: 成功（警告なし）

- [ ] **Step 9: matd_client のユニットテストを追加して確認**

`crates/mat/src/matd_client.rs` の tests mod、`color_temp_mireds_maps_with_computed_kelvin_echo` の直後に追加:

```rust
    #[test]
    fn color_maps_to_color_op_with_converted_values() {
        let cmd = Command::Color {
            node_id: NodeRef::Id(6),
            endpoint: EndpointRef::Id(1),
            hue: 330,
            sat: 80,
            transition: 30,
        };
        // 換算（330° → 233、80% → 203）は mat 側で行い、度 / % はエコー用に併送する。
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({
                "op":"color","node_id":6,"endpoint":1,
                "hue_raw":233,"saturation_raw":203,
                "hue":330,"saturation":80,"transition":30
            })
        );
    }
```

Run: `cargo test -p mat color_maps_to_color_op`
Expected: PASS

- [ ] **Step 10: 直経路の統合テストを追加して確認**

`crates/mat/tests/integration.rs` の `color_temp_unknown_node_exits_11`（448 行 `}` ）の直後に追加:

```rust
#[test]
fn color_expands_to_move_to_hue_and_saturation() {
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args(["color", "--node", "5", "--hue", "330", "--sat", "80"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"cluster\":\"colorcontrol\""))
        .stdout(predicate::str::contains(
            "\"command\":\"move-to-hue-and-saturation\"",
        ))
        // 入力の度 / % と換算後の 0–254 生値を両方エコーする（読み返し突合用）。
        .stdout(predicate::str::contains("\"hue\":330"))
        .stdout(predicate::str::contains("\"saturation\":80"))
        .stdout(predicate::str::contains("\"hue_raw\":233"))
        .stdout(predicate::str::contains("\"saturation_raw\":203"))
        .stdout(predicate::str::contains("\"status\":\"success\""))
        .stdout(predicate::str::contains("\"timestamp\""));
    // chip-tool へは hue/sat 生値 + transition + optionsMask/Override、宛先は末尾。
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("colorcontrol move-to-hue-and-saturation 233 203 0 0 0 5 1"),
        "expected converted hue/sat invoke argv: {recorded}"
    );
}

#[test]
fn color_transition_is_passed_to_chip_tool() {
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args([
            "color",
            "--node",
            "5",
            "--hue",
            "330",
            "--sat",
            "80",
            "--transition",
            "30",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"transition\":30"));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("colorcontrol move-to-hue-and-saturation 233 203 30 0 0 5 1"),
        "expected transition time in argv: {recorded}"
    );
}

#[test]
fn color_requires_both_hue_and_sat() {
    let store = store_with_node5();
    // --sat 欠け → CLI 引数エラー（exit 2）。
    mat(store.path())
        .args(["color", "--node", "5", "--hue", "330"])
        .assert()
        .code(2);
    // --hue 欠け → 同じく exit 2。
    mat(store.path())
        .args(["color", "--node", "5", "--sat", "80"])
        .assert()
        .code(2);
}

#[test]
fn color_rejects_out_of_range_values() {
    let store = store_with_node5();
    // hue は 0–360 度、sat は 0–100 %。超過は clap の値域検証で exit 2。
    mat(store.path())
        .args(["color", "--node", "5", "--hue", "361", "--sat", "80"])
        .assert()
        .code(2);
    mat(store.path())
        .args(["color", "--node", "5", "--hue", "330", "--sat", "101"])
        .assert()
        .code(2);
}

#[test]
fn color_unknown_node_exits_11() {
    let store = store_with_node5();
    mat(store.path())
        .args(["color", "--node", "99", "--hue", "330", "--sat", "80"])
        .assert()
        .code(11)
        .stderr(predicate::str::contains("node_not_commissioned"));
}
```

Run: `cargo test -p mat --test integration color_`
Expected: PASS（color-temp 既存分含め全部）

- [ ] **Step 11: matd 自動検出経路のテストを追加して確認**

`crates/mat/tests/matd_auto.rs` の `auto_routes_color_temp_with_converted_mireds`（121 行 `}` ）の直後に追加:

```rust
#[test]
fn auto_routes_color_with_converted_values() {
    let store = store_with_node5();
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd(socket.clone());

    mat_auto(store.path(), &socket)
        .args(["color", "--node", "5", "--hue", "330", "--sat", "80"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"via\":\"fake-matd\""));

    // 換算（330° → 233、80% → 203）は mat 側で済んだ状態で matd に届く。
    let req = matd.join().unwrap();
    assert!(req.contains("\"op\":\"color\""), "request line: {req}");
    assert!(req.contains("\"hue_raw\":233"), "request line: {req}");
    assert!(req.contains("\"saturation_raw\":203"), "request line: {req}");
}
```

Run: `cargo test -p mat --test matd_auto auto_routes_color_with`
Expected: PASS

- [ ] **Step 12: 全体チェックしてコミット**

Run: `task check`
Expected: fmt:check / clippy / test すべて成功

```bash
git add crates/mat/src/commands/invoke.rs crates/mat/src/cli.rs crates/mat/src/resolve.rs crates/mat/src/main.rs crates/mat/src/matd_client.rs crates/mat/tests/integration.rs crates/mat/tests/matd_auto.rs
git commit -m "feat(mat): color サブコマンド（ColorControl MoveToHueAndSaturation）を追加

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: ドキュメント反映（README / ARCHITECTURE）

**Files:**
- Modify: `README.md`（ショートカット例・出力 JSON 例・matd 対応リスト・alias 受け付けコマンド一覧）
- Modify: `ARCHITECTURE.md`（matd op 一覧・auto-detect 対応リスト）

**Interfaces:**
- Consumes: Task 2 の CLI（`mat color --node 5 --hue 330 --sat 80`）と出力スキーマ
- Produces: なし（ドキュメントのみ）

- [ ] **Step 1: README.md を更新（4 箇所）**

(1) `README.md:167`（`mat color-temp --node 5 --mireds 370` の行）の直後、コードブロック終端 ``` の前に追加:

```bash

# Hue / saturation (ColorControl MoveToHueAndSaturation): --hue in degrees
# (0-360) and --sat in percent (0-100), both required. mat converts each to
# Matter's 0-254 scale (round(v / full * 254); 255 is reserved so full scale
# tops out at 254). --transition is in tenths of a second (default 0). Values
# outside the device's supported range are clamped by the device itself.
mat color --node 5 --hue 330 --sat 80
mat color --node 5 --hue 330 --sat 80 --transition 30
```

(2) `README.md:190`（color-temp の出力 JSON 例の行）の直後に追加:

```json

// color — echoes the input degrees/percent plus the converted 0-254 raw
// values so the result can be cross-checked against `current-hue` /
// `current-saturation` reads
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "colorcontrol", "command": "move-to-hue-and-saturation", "hue": 330, "saturation": 80, "hue_raw": 233, "saturation_raw": 203, "transition": 0, "status": "success" }
```

(3) `README.md:444-445` の matd 対応リスト:

変更前: `` `color-temp` / `describe` / `group`. ``
変更後: `` `color-temp` / `color` / `describe` / `group`. ``

(4) `README.md:486-487` の alias 受け付けコマンド一覧:

変更前: `describe / on / off / color-temp / open-window / diag thread / diag node)`
変更後: `describe / on / off / color-temp / color / open-window / diag thread / diag node)`

- [ ] **Step 2: ARCHITECTURE.md を更新（2 箇所）**

(1) `ARCHITECTURE.md:347` の op 一覧:

変更前: `` `op` ∈ `read | write | invoke | on | off | color_temp | describe | group | ping` ``
変更後: `` `op` ∈ `read | write | invoke | on | off | color_temp | color | describe | group | ping` ``

(2) `ARCHITECTURE.md:369` の auto-detect 対応リスト:

変更前: `read/write/invoke/on/off/color-temp/describe/group,`
変更後: `read/write/invoke/on/off/color-temp/color/describe/group,`

- [ ] **Step 3: チェックしてコミット**

Run: `task check`
Expected: 成功（ドキュメントのみだが習慣として実行）

```bash
git add README.md ARCHITECTURE.md
git commit -m "docs: mat color（hue/sat 指定）を README/ARCHITECTURE に反映

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## 補足（実装者向けメモ）

- `output::emit`（mat-core）が `timestamp` を自動付与するので、`json!` に timestamp を書かないこと（color-temp と同じ）。
- fake-chip-tool.sh は未知クラスタ/コマンドを invoke 汎用ケース（SUCCESS 応答 + `FAKE_CHIP_ARGS_FILE` 記録）で処理するため、fixture の変更は不要。
- `resolve.rs` / `matd_client.rs::to_op` / `server.rs` の match は意図的に網羅（`_` 無し）。arm 追加を忘れるとコンパイルエラーになるのは設計どおり。
- 片方だけの MoveToHue / MoveToSaturation 対応は YAGNI（必要になってから）。
- バージョン bump（直近の `chore: version 0.12.0` の流儀）はこの計画には含めない。リリース時に別途。
