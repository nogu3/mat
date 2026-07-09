# issue #6: group/単体 color ショートカットと named color / RGB 指定 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `mat color` / `mat group color` に `--name` / `--rgb` / `--hue`+`--sat` の 3 系統色指定を追加し、`mat group color-temp` / `mat group color` の換算付き groupcast ショートカット（直経路 + matd 経路）を新設する。

**Architecture:** 色変換（RGB→HSV→0–254 生値）と組み込み色名テーブルは `mat-core` の新モジュール `color.rs` に置き、単体・group・matd クライアントの 3 経路から共有する。色名解決（`[colors]` ユーザー定義が組み込みを上書き）は既存 alias 解決と同じレイヤ（`resolve.rs`）で行い、以降は数値のみが流れる。matd には groupcast 用の新 op `group_color_temp` / `group_color` を足す（換算は mat 側 1 箇所、matd はエコーのみ）。

**Tech Stack:** Rust (clap derive / serde / toml), fake-chip-tool 統合テスト, tokio + fake ws (matd)

## Global Constraints

- stdout は純粋な構造化 JSON のみ（`timestamp` は `output::emit` が自動付与）。
- ワイヤ / chip-tool / matd に渡る値は常に数値。色名→RGB→hue/sat は決定的なローカル換算。
- `aliases.toml` 無し = 挙動不変。壊れた `[colors]`（RGB パース不能・不正名）は `store_parse` / exit 10。未知の色名・不正な `--rgb` は CLI 引数エラー（kind=Other → exit 2）。
- V（明度）は捨てる: `--name` / `--rgb` は色だけ設定し明るさは変えない（--help に明記）。
- groupcast は unacknowledged: group ショートカットは `"status": "sent"` のみ報告。
- MoveToHueAndSaturation を継続採用（MoveToColor(xy) は使わない）。optionsMask=0 据え置き（点灯中のみ反映、--help に注記）。
- 各タスク完了時に `cargo test -p <crate>` が通ること。最終タスクで `task check`（fmt:check + clippy -D warnings + 全テスト）通過。
- コミットメッセージ末尾: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`

## 変換仕様（全タスク共通の正）

- RGB→HSV: `r,g,b ∈ [0,1]`、`delta = max - min`。`delta == 0 || max == 0` なら `(h, s) = (0, 0)`（無彩色。white はここで sat=0 に落ちる）。それ以外:
  - `max == r`: `h = 60 * ((g - b) / delta).rem_euclid(6.0)`
  - `max == g`: `h = 60 * ((b - r) / delta + 2.0)`
  - `max == b`: `h = 60 * ((r - g) / delta + 4.0)`
  - `s = delta / max`
- 生値: `hue_raw = round(h / 360 * 254)`、`sat_raw = round(s * 254)`（f64 → `.round() as u8`）。
- エコー用: `hue = round(h)` 度、`sat = round(s * 100)` %。
- `--hue`/`--sat` 生指定は既存の整数換算のまま: `raw = (v * 254 + full/2) / full`。
- 代表値（テストで固定する）:

| 入力 | h | s | hue_raw | sat_raw | echo hue | echo sat |
|---|---|---|---|---|---|---|
| red `#ff0000` | 0 | 1 | 0 | 254 | 0 | 100 |
| white `#ffffff` | 0 | 0 | 0 | 0 | 0 | 0 |
| pink `#ffc0cb` | 349.52 | 0.2471 | 247 | 63 | 350 | 25 |
| blue `#0000ff` | 240 | 1 | 169 | 254 | 240 | 100 |
| `#ff00aa` (255,0,170) | 320 | 1 | 226 | 254 | 320 | 100 |
| `#ff8c00` (255,140,0) | 32.94 | 1 | 23 | 254 | 33 | 100 |

- 組み込み色名テーブル（CSS 準拠、RGB 値で定義）: blue `#0000ff` / cyan `#00ffff` / green `#008000` / magenta `#ff00ff` / orange `#ffa500` / pink `#ffc0cb` / purple `#800080` / red `#ff0000` / white `#ffffff` / yellow `#ffff00`。名前は case-sensitive。

---

### Task 1: mat-core `color.rs` — 変換・組み込みテーブル・resolve_spec

**Files:**
- Create: `crates/mat-core/src/color.rs`
- Modify: `crates/mat-core/src/lib.rs`（`pub mod color;` 追加、アルファベット順で `alias` の次）

**Interfaces:**
- Produces（後続タスクが依存する公開 API）:
  - `pub const BUILTIN_COLORS: &[(&str, [u8; 3])]`
  - `pub fn builtin_color(name: &str) -> Option<[u8; 3]>`
  - `pub fn parse_rgb(s: &str) -> Result<[u8; 3], String>`
  - `pub fn hex_string(rgb: [u8; 3]) -> String`（`"#rrggbb"` 小文字）
  - `pub struct ResolvedColor { pub hue_raw: u8, pub sat_raw: u8, pub hue: u16, pub sat: u8, pub name: Option<String>, pub rgb: Option<String> }`
  - `pub fn from_rgb(rgb: [u8; 3], name: Option<String>) -> ResolvedColor`
  - `pub fn from_hue_sat(hue_deg: u16, sat_pct: u8) -> ResolvedColor`
  - `pub fn resolve_spec(name: Option<&str>, rgb: Option<&str>, hue: Option<u16>, sat: Option<u8>) -> Result<ResolvedColor, MatError>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/color.rs` を作成し、まず実装スケルトン（`todo!()` ではなく後述の完全実装をこのタスク内で書くが、TDD としてテストを先に書いてから実装を埋める）とテストを置く:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rgb_accepts_hex_with_and_without_hash() {
        assert_eq!(parse_rgb("#ff0000").unwrap(), [255, 0, 0]);
        assert_eq!(parse_rgb("ff00aa").unwrap(), [255, 0, 170]);
        assert_eq!(parse_rgb("#FFC0CB").unwrap(), [255, 192, 203]); // hex は大小無視
    }

    #[test]
    fn parse_rgb_accepts_decimal_triplet() {
        assert_eq!(parse_rgb("255,0,170").unwrap(), [255, 0, 170]);
        assert_eq!(parse_rgb(" 255 , 0 , 170 ").unwrap(), [255, 0, 170]);
    }

    #[test]
    fn parse_rgb_rejects_malformed_input() {
        assert!(parse_rgb("zzz").is_err());
        assert!(parse_rgb("#ff00").is_err()); // 桁不足
        assert!(parse_rgb("255,0").is_err()); // 要素不足
        assert!(parse_rgb("256,0,0").is_err()); // u8 範囲外
        assert!(parse_rgb("").is_err());
    }

    #[test]
    fn hex_string_normalizes_lowercase() {
        assert_eq!(hex_string([255, 192, 203]), "#ffc0cb");
    }

    #[test]
    fn from_rgb_red_is_full_saturation_hue_zero() {
        let c = from_rgb([255, 0, 0], Some("red".into()));
        assert_eq!((c.hue_raw, c.sat_raw), (0, 254));
        assert_eq!((c.hue, c.sat), (0, 100));
        assert_eq!(c.rgb.as_deref(), Some("#ff0000"));
        assert_eq!(c.name.as_deref(), Some("red"));
    }

    #[test]
    fn from_rgb_white_collapses_to_sat_zero() {
        // white は RGB→HSV で自然に sat=0（無彩色）に落ちる。特別扱い無し。
        let c = from_rgb([255, 255, 255], Some("white".into()));
        assert_eq!((c.hue_raw, c.sat_raw), (0, 0));
        assert_eq!((c.hue, c.sat), (0, 0));
    }

    #[test]
    fn from_rgb_pink_matches_reference_values() {
        // #ffc0cb → h=349.52°, s=0.2471 → raw (247, 63)、エコー (350°, 25%)。
        let c = from_rgb([255, 192, 203], None);
        assert_eq!((c.hue_raw, c.sat_raw), (247, 63));
        assert_eq!((c.hue, c.sat), (350, 25));
        assert_eq!(c.name, None);
    }

    #[test]
    fn from_rgb_blue_and_magenta_branches() {
        // max==b の分岐: blue #0000ff → h=240 → raw 169。
        let c = from_rgb([0, 0, 255], None);
        assert_eq!((c.hue_raw, c.sat_raw), (169, 254));
        // max==r で負の差分（rem_euclid の分岐）: #ff00aa → h=320 → raw 226。
        let c = from_rgb([255, 0, 170], None);
        assert_eq!((c.hue_raw, c.sat_raw), (226, 254));
        assert_eq!((c.hue, c.sat), (320, 100));
    }

    #[test]
    fn from_hue_sat_matches_legacy_integer_scaling() {
        // 既存 `mat color --hue/--sat` と同じ換算（round(v / full * 254)）。
        let c = from_hue_sat(330, 80);
        assert_eq!((c.hue_raw, c.sat_raw), (233, 203));
        assert_eq!((c.hue, c.sat), (330, 80));
        assert_eq!((c.name, c.rgb), (None, None));
        // フルスケールは 254 で頭打ち（255 は Matter の予約値）。
        let c = from_hue_sat(360, 100);
        assert_eq!((c.hue_raw, c.sat_raw), (254, 254));
    }

    #[test]
    fn builtin_table_covers_issue_names() {
        for name in [
            "red", "pink", "orange", "purple", "cyan", "green", "blue", "yellow", "magenta",
            "white",
        ] {
            assert!(builtin_color(name).is_some(), "missing builtin color {name}");
        }
        assert_eq!(builtin_color("red"), Some([255, 0, 0]));
        assert_eq!(builtin_color("sakura"), None);
        assert_eq!(builtin_color("RED"), None); // case-sensitive
    }

    #[test]
    fn resolve_spec_dispatches_rgb_then_hue_sat() {
        let c = resolve_spec(Some("red"), Some("#ff0000"), None, None).unwrap();
        assert_eq!((c.hue_raw, c.sat_raw), (0, 254));
        assert_eq!(c.name.as_deref(), Some("red"));
        let c = resolve_spec(None, None, Some(330), Some(80)).unwrap();
        assert_eq!((c.hue_raw, c.sat_raw), (233, 203));
        // 3 系統どれも無ければエラー（clap が防ぐが防御的に）。
        assert!(resolve_spec(None, None, None, None).is_err());
        // 不正な rgb 文字列は kind=Other。
        let err = resolve_spec(None, Some("zzz"), None, None).unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Other);
    }
}
```

- [ ] **Step 2: テストが失敗（コンパイルエラー）することを確認**

Run: `cargo test -p mat-core color 2>&1 | head -30`
Expected: FAIL（`parse_rgb` 等が未定義のコンパイルエラー）

- [ ] **Step 3: 実装を書く**

`crates/mat-core/src/color.rs` の実装部（テストの上に置く）:

```rust
//! 色指定（name / RGB / hue+sat）→ MoveToHueAndSaturation の 0–254 生値。
//!
//! 名前・RGB は RGB→HSV で hue/sat に落とす 1 本の変換パス。V（明度）は捨てる
//! （明度は LevelControl の領分で、色指定は明るさを変えない）。組み込み色名
//! テーブルは RGB 値で定義し、aliases.toml の `[colors]`（同じく RGB 値）が
//! 同名を上書きする。ワイヤに出るのは常に数値（決定的なローカル換算のみ）。

use crate::error::{ErrorKind, MatError};

/// 組み込み色名テーブル（RGB 値で定義、CSS 色に準拠）。名前順。case-sensitive。
pub const BUILTIN_COLORS: &[(&str, [u8; 3])] = &[
    ("blue", [0x00, 0x00, 0xff]),
    ("cyan", [0x00, 0xff, 0xff]),
    ("green", [0x00, 0x80, 0x00]),
    ("magenta", [0xff, 0x00, 0xff]),
    ("orange", [0xff, 0xa5, 0x00]),
    ("pink", [0xff, 0xc0, 0xcb]),
    ("purple", [0x80, 0x00, 0x80]),
    ("red", [0xff, 0x00, 0x00]),
    ("white", [0xff, 0xff, 0xff]),
    ("yellow", [0xff, 0xff, 0x00]),
];

/// 組み込みテーブルから色名を引く。
pub fn builtin_color(name: &str) -> Option<[u8; 3]> {
    BUILTIN_COLORS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, rgb)| *rgb)
}

/// RGB 文字列をパースする: `#rrggbb` / `rrggbb`（hex は大小無視）/ `R,G,B`（10進）。
pub fn parse_rgb(s: &str) -> Result<[u8; 3], String> {
    let t = s.trim();
    if t.contains(',') {
        let parts: Vec<&str> = t.split(',').map(str::trim).collect();
        if parts.len() != 3 {
            return Err(format!(
                "invalid RGB '{s}' (expected three comma-separated 0-255 values)"
            ));
        }
        let mut rgb = [0u8; 3];
        for (i, p) in parts.iter().enumerate() {
            rgb[i] = p
                .parse::<u8>()
                .map_err(|_| format!("invalid RGB component '{p}' in '{s}' (must be 0-255)"))?;
        }
        return Ok(rgb);
    }
    let hex = t.strip_prefix('#').unwrap_or(t);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("invalid RGB '{s}' (expected #rrggbb, rrggbb, or R,G,B)"));
    }
    Ok([
        u8::from_str_radix(&hex[0..2], 16).expect("validated hex"),
        u8::from_str_radix(&hex[2..4], 16).expect("validated hex"),
        u8::from_str_radix(&hex[4..6], 16).expect("validated hex"),
    ])
}

/// RGB を正規形 `"#rrggbb"`（小文字）にする（出力 JSON のエコー用）。
pub fn hex_string(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// 換算済みの色。`hue_raw` / `sat_raw` がワイヤに乗る 0–254 生値、`hue`（度）/
/// `sat`（%）と `name` / `rgb` は出力 JSON へのエコー（読み返し突合用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedColor {
    pub hue_raw: u8,
    pub sat_raw: u8,
    pub hue: u16,
    pub sat: u8,
    pub name: Option<String>,
    pub rgb: Option<String>,
}

/// RGB→HSV の H（度 0–360 未満）と S（0–1）。V（明度）は捨てる。
/// 無彩色（delta=0、白・黒・グレー）は (0, 0)。
fn rgb_to_hue_sat([r, g, b]: [u8; 3]) -> (f64, f64) {
    let (r, g, b) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    if delta == 0.0 || max == 0.0 {
        return (0.0, 0.0);
    }
    let h = if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (h, delta / max)
}

/// RGB から換算する（name / rgb 指定の合流点）。生値は HSV の float から直接
/// 丸め、エコー用の度・% は表示向けに別途丸める（生値の再換算はしない）。
pub fn from_rgb(rgb: [u8; 3], name: Option<String>) -> ResolvedColor {
    let (h, s) = rgb_to_hue_sat(rgb);
    ResolvedColor {
        hue_raw: (h / 360.0 * 254.0).round() as u8,
        sat_raw: (s * 254.0).round() as u8,
        hue: h.round() as u16,
        sat: (s * 100.0).round() as u8,
        name,
        rgb: Some(hex_string(rgb)),
    }
}

/// `--hue`（0–360 度）/ `--sat`（0–100 %）の生指定を換算する（従来の
/// `mat color` と同じ整数換算: round(v / full * 254)、255 は予約値）。
pub fn from_hue_sat(hue_deg: u16, sat_pct: u8) -> ResolvedColor {
    fn scale(v: u32, full: u32) -> u8 {
        ((v * 254 + full / 2) / full) as u8
    }
    ResolvedColor {
        hue_raw: scale(u32::from(hue_deg), 360),
        sat_raw: scale(u32::from(sat_pct), 100),
        hue: hue_deg,
        sat: sat_pct,
        name: None,
        rgb: None,
    }
}

/// 色指定（3 系統のうち 1 つ）を換算する。`rgb` は resolve 層で正規化・検証済みの
/// 前提だが、防御的に再パースする（name は rgb と併走: 名前解決後のエコー用）。
pub fn resolve_spec(
    name: Option<&str>,
    rgb: Option<&str>,
    hue: Option<u16>,
    sat: Option<u8>,
) -> Result<ResolvedColor, MatError> {
    if let Some(hex) = rgb {
        let c = parse_rgb(hex).map_err(|e| MatError::new(ErrorKind::Other, e))?;
        return Ok(from_rgb(c, name.map(str::to_string)));
    }
    match (hue, sat) {
        (Some(h), Some(s)) => Ok(from_hue_sat(h, s)),
        _ => Err(MatError::new(
            ErrorKind::Other,
            "color spec requires --name, --rgb, or both --hue and --sat".to_string(),
        )),
    }
}
```

`crates/mat-core/src/lib.rs` に追加:

```rust
pub mod color;
```

（`pub mod alias;` の直後、アルファベット順）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-core color`
Expected: PASS（新テスト全件）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-core/src/color.rs crates/mat-core/src/lib.rs
git commit -m "feat(mat-core): 色指定モジュール（RGB→HSV 換算・組み込み色名テーブル）"
```

---

### Task 2: aliases.toml `[colors]` — カスタム色名（組み込みを上書き）

**Files:**
- Modify: `crates/mat-core/src/alias.rs`

**Interfaces:**
- Consumes: Task 1 の `color::parse_rgb` / `color::builtin_color` / `color::BUILTIN_COLORS`
- Produces: `AliasBook::resolve_color_name(&self, name: &str) -> Result<[u8; 3], MatError>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/alias.rs` の `tests` モジュールに追加:

```rust
#[test]
fn colors_section_resolves_and_overrides_builtin() {
    let dir = tempfile::tempdir().unwrap();
    write_aliases(
        dir.path(),
        r#"
        [colors]
        warm = "#ff8c00"
        mypink = "255,182,193"
        red = "0,0,255"
    "#,
    );
    let book = AliasBook::load(dir.path()).unwrap();
    // ユーザー定義。
    assert_eq!(book.resolve_color_name("warm").unwrap(), [255, 140, 0]);
    assert_eq!(book.resolve_color_name("mypink").unwrap(), [255, 182, 193]);
    // 同名のユーザー定義が組み込み（red = #ff0000）を上書きする。
    assert_eq!(book.resolve_color_name("red").unwrap(), [0, 0, 255]);
    // 組み込みへのフォールバック。
    assert_eq!(book.resolve_color_name("blue").unwrap(), [0, 0, 255]);
}

#[test]
fn builtin_colors_work_without_aliases_file() {
    // ファイル無し = 組み込みのみで挙動不変。
    let dir = tempfile::tempdir().unwrap();
    let book = AliasBook::load(dir.path()).unwrap();
    assert_eq!(book.resolve_color_name("red").unwrap(), [255, 0, 0]);
}

#[test]
fn unknown_color_name_lists_known_names() {
    let dir = tempfile::tempdir().unwrap();
    write_aliases(dir.path(), "[colors]\nwarm = \"#ff8c00\"\n");
    let book = AliasBook::load(dir.path()).unwrap();
    let err = book.resolve_color_name("sakura").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Other);
    assert!(err.detail.contains("warm"), "{}", err.detail);
    assert!(err.detail.contains("red"), "{}", err.detail); // 組み込みも列挙
}

#[test]
fn corrupt_color_value_or_name_is_store_parse() {
    let dir = tempfile::tempdir().unwrap();
    // RGB としてパース不能な値は load 時に store_parse（exit 10）。
    write_aliases(dir.path(), "[colors]\nbad = \"zzz\"\n");
    assert_eq!(
        AliasBook::load(dir.path()).unwrap_err().kind,
        ErrorKind::StoreParse
    );
    // 色名も既存 alias と同じ検証（純数字・空は NG）。
    write_aliases(dir.path(), "[colors]\n42 = \"#ff0000\"\n");
    assert_eq!(
        AliasBook::load(dir.path()).unwrap_err().kind,
        ErrorKind::StoreParse
    );
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-core alias 2>&1 | head -30`
Expected: FAIL（`resolve_color_name` 未定義 / `colors` フィールド未定義のコンパイルエラー）

- [ ] **Step 3: 実装**

`AliasFile` にフィールド追加（`endpoints` の後）:

```rust
    /// カスタム色名 → RGB 値（`#rrggbb` / `rrggbb` / `R,G,B`）。組み込みテーブル
    /// （mat-core::color::BUILTIN_COLORS）の同名を上書きする。
    #[serde(default)]
    colors: BTreeMap<String, String>,
```

`Default for AliasFile` の手動 impl にも `colors: BTreeMap::new(),` を追加。

`validate` の alias_names チェーンに `.chain(file.colors.keys())` を追加し、末尾（`Ok(())` の直前）に値検証を追加:

```rust
        for (name, value) in &file.colors {
            if let Err(e) = crate::color::parse_rgb(value) {
                return Err(MatError::store_parse(format!(
                    "invalid RGB value for color '{name}' in {}: {e}",
                    path.display()
                )));
            }
        }
```

`impl AliasBook` にメソッド追加（`resolve_endpoint` の後）:

```rust
    /// 色名を RGB へ確定する。`[colors]`（ユーザー定義）が組み込みテーブルを
    /// 上書きする。未知の名前は kind=Other（main が exit 2 に写す）で、既知の
    /// 名前（組み込み + ユーザー定義）を列挙して自己修復を助ける。
    pub fn resolve_color_name(&self, name: &str) -> Result<[u8; 3], MatError> {
        if let Some(value) = self.file.colors.get(name) {
            // 値は load 時に検証済み。ここでの失敗はロジックエラーのみ。
            return crate::color::parse_rgb(value).map_err(MatError::store_parse);
        }
        crate::color::builtin_color(name).ok_or_else(|| {
            let mut known: Vec<&str> = crate::color::BUILTIN_COLORS
                .iter()
                .map(|(n, _)| *n)
                .collect();
            known.extend(self.file.colors.keys().map(String::as_str));
            known.sort_unstable();
            known.dedup();
            MatError::new(
                ErrorKind::Other,
                format!("unknown color name '{name}' (known: {})", known.join(", ")),
            )
        })
    }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-core`
Expected: PASS（alias の既存テスト含め全件）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-core/src/alias.rs
git commit -m "feat(mat-core): aliases.toml [colors] でカスタム色名（組み込み上書き）"
```

---

### Task 3: 単体 `mat color` の 3 系統化（--name / --rgb / --hue+--sat）

**Files:**
- Modify: `crates/mat/src/cli.rs`（`ColorSpecArgs` 新設、`Command::Color` を flatten に変更）
- Modify: `crates/mat/src/resolve.rs`（色名 / rgb の正規化）
- Modify: `crates/mat/src/commands/invoke.rs`（`run_color` の引数を `ResolvedColor` に、旧 `resolve_color` 削除）
- Modify: `crates/mat/src/main.rs`（Color arm）
- Modify: `crates/mat/src/matd_client.rs`（Color の to_op + 既存テスト修正）
- Test: `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: Task 1 `mat_core::color::{ResolvedColor, resolve_spec, hex_string, parse_rgb}`、Task 2 `AliasBook::resolve_color_name`
- Produces:
  - `pub struct ColorSpecArgs { pub name: Option<String>, pub rgb: Option<String>, pub hue: Option<u16>, pub sat: Option<u8> }`（cli.rs、Task 4 の group color も flatten で使う）
  - `resolve.rs` 内 `fn resolve_color_spec(book: &AliasBook, spec: ColorSpecArgs) -> Result<ColorSpecArgs, MatError>`（Task 4 も呼ぶ）
  - `commands::invoke::run_color(store_path, node_id, endpoint, color: &ResolvedColor, transition)`
  - 出力 JSON: 既存フィールドに加え、name/rgb 指定時は `"name"` / `"rgb"`（正規形 `#rrggbb`）をエコー

- [ ] **Step 1: 失敗する統合テストを書く**

`crates/mat/tests/integration.rs` の color テスト群の後に追加:

```rust
#[test]
fn color_name_red_converts_via_rgb_hsv() {
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args(["color", "--node", "5", "--name", "red"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"name\":\"red\""))
        .stdout(predicate::str::contains("\"rgb\":\"#ff0000\""))
        .stdout(predicate::str::contains("\"hue_raw\":0"))
        .stdout(predicate::str::contains("\"saturation_raw\":254"))
        .stdout(predicate::str::contains("\"status\":\"success\""));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("colorcontrol move-to-hue-and-saturation 0 254 0 0 0 5 1"),
        "expected red converted argv: {recorded}"
    );
}

#[test]
fn color_name_white_collapses_to_sat_zero() {
    // white = #ffffff は RGB→HSV で自然に sat=0（無彩色）。特別扱い無し。
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args(["color", "--node", "5", "--name", "white"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"saturation_raw\":0"));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("colorcontrol move-to-hue-and-saturation 0 0 0 0 0 5 1"),
        "expected white sat=0 argv: {recorded}"
    );
}

#[test]
fn color_rgb_hex_and_decimal_forms_are_equivalent() {
    let store = store_with_node5();
    for rgb in ["#ff00aa", "ff00aa", "255,0,170"] {
        let args_file = store.path().join("recorded-args.txt");
        mat(store.path())
            .env("FAKE_CHIP_ARGS_FILE", &args_file)
            .args(["color", "--node", "5", "--rgb", rgb])
            .assert()
            .success()
            // 入力表記によらず正規形 #rrggbb でエコーする。
            .stdout(predicate::str::contains("\"rgb\":\"#ff00aa\""))
            .stdout(predicate::str::contains("\"hue\":320"))
            .stdout(predicate::str::contains("\"hue_raw\":226"))
            .stdout(predicate::str::contains("\"saturation_raw\":254"));
        let recorded = std::fs::read_to_string(&args_file).unwrap();
        assert!(
            recorded.contains("colorcontrol move-to-hue-and-saturation 226 254 0 0 0 5 1"),
            "expected #ff00aa argv for input {rgb}: {recorded}"
        );
    }
}

#[test]
fn color_spec_systems_are_mutually_exclusive() {
    let store = store_with_node5();
    // 複数系統の同時指定は exit 2。
    mat(store.path())
        .args(["color", "--node", "5", "--name", "red", "--rgb", "#ff0000"])
        .assert()
        .code(2);
    mat(store.path())
        .args(["color", "--node", "5", "--name", "red", "--hue", "0", "--sat", "100"])
        .assert()
        .code(2);
    mat(store.path())
        .args(["color", "--node", "5", "--rgb", "#ff0000", "--hue", "0", "--sat", "100"])
        .assert()
        .code(2);
    // どの系統も無し / hue・sat の片割れも exit 2（既存挙動の維持）。
    mat(store.path()).args(["color", "--node", "5"]).assert().code(2);
    mat(store.path())
        .args(["color", "--node", "5", "--hue", "330"])
        .assert()
        .code(2);
}

#[test]
fn color_custom_name_from_aliases_colors_overrides_builtin() {
    let store = store_with_node5();
    std::fs::write(
        store.path().join("aliases.toml"),
        "[colors]\nwarm = \"#ff8c00\"\nred = \"0,0,255\"\n",
    )
    .unwrap();
    let args_file = store.path().join("recorded-args.txt");
    // ユーザー定義色。#ff8c00 → hue_raw 23, sat_raw 254。
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args(["color", "--node", "5", "--name", "warm"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"name\":\"warm\""))
        .stdout(predicate::str::contains("\"rgb\":\"#ff8c00\""));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("colorcontrol move-to-hue-and-saturation 23 254 0 0 0 5 1"),
        "expected warm argv: {recorded}"
    );
    // 組み込み red をユーザー定義（青）が上書き → hue_raw 169。
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args(["color", "--node", "5", "--name", "red"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"rgb\":\"#0000ff\""));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("colorcontrol move-to-hue-and-saturation 169 254 0 0 0 5 1"),
        "expected overridden red (=blue) argv: {recorded}"
    );
}

#[test]
fn color_unknown_name_exits_2_and_broken_colors_exits_10() {
    let store = store_with_node5();
    // 未知の色名は CLI 引数エラー（exit 2）。既知名を列挙する。
    mat(store.path())
        .args(["color", "--node", "5", "--name", "sakura"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown color name"));
    // 壊れた [colors]（RGB パース不能）は store_parse（exit 10）。
    std::fs::write(store.path().join("aliases.toml"), "[colors]\nbad = \"zzz\"\n").unwrap();
    mat(store.path())
        .args(["color", "--node", "5", "--name", "red"])
        .assert()
        .code(10)
        .stderr(predicate::str::contains("store_parse"));
}

#[test]
fn color_invalid_rgb_exits_2() {
    let store = store_with_node5();
    mat(store.path())
        .args(["color", "--node", "5", "--rgb", "zzz"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid RGB"));
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat --test integration color 2>&1 | tail -20`
Expected: FAIL（`--name` / `--rgb` は unknown argument → exit 2 でテストの success 期待が落ちる。排他系テストは通ってしまってよい）

- [ ] **Step 3: cli.rs — `ColorSpecArgs` を新設し `Command::Color` に flatten**

`crates/mat/src/cli.rs` に追加（`Command` enum の後、`DiagCommand` の前あたり）:

```rust
/// 色の指定（3 系統から 1 つ、排他）: `--name`（色名）/ `--rgb`（HEX or R,G,B）/
/// `--hue`+`--sat`（生指定、両方必須）。名前・RGB は RGB→HSV で hue/sat へ換算し、
/// V（明度）は捨てる — **色だけ設定し、明るさは変えない**（明るさは LevelControl
/// の領分）。点灯中でないと反映されない（ExecuteIfOff は立てない）。
#[derive(clap::Args, Debug, Clone, PartialEq)]
#[group(id = "color_spec", required = true, multiple = true)]
pub struct ColorSpecArgs {
    /// 色名。組み込み: red / pink / orange / purple / cyan / green / blue /
    /// yellow / magenta / white。aliases.toml の `[colors]` で追加・上書き可
    /// （RGB 値で定義）。色だけ設定し、明るさ（明度）は変えない。
    #[arg(long, value_name = "NAME", conflicts_with_all = ["rgb", "hue", "sat"])]
    pub name: Option<String>,
    /// RGB 値（`#ff0000` / `ff0000` / `255,0,0`）。RGB→HSV で hue/sat へ換算し、
    /// 明度（V）は捨てる（明るさは変えない）。
    #[arg(long, value_name = "HEX|R,G,B", conflicts_with_all = ["hue", "sat"])]
    pub rgb: Option<String>,
    /// 色相（度、0–360）。例: 330 = ピンク。`--sat` と併用必須。
    #[arg(long, value_name = "DEG", requires = "sat", value_parser = clap::value_parser!(u16).range(0..=360))]
    pub hue: Option<u16>,
    /// 彩度（%、0–100）。`--hue` と併用必須。
    #[arg(long, value_name = "PCT", requires = "hue", value_parser = clap::value_parser!(u8).range(0..=100))]
    pub sat: Option<u8>,
}
```

`Command::Color` の `hue: u16` / `sat: u8` フィールド 2 つを削除し、代わりに:

```rust
        #[command(flatten)]
        spec: ColorSpecArgs,
```

variant の doc コメントも 3 系統対応に更新する（「`--hue`（0–360 度）と `--sat`（0–100 %）は両方必須で」の段落を、上記 3 系統・明度は変えない旨の説明に差し替え）。

- [ ] **Step 4: resolve.rs — 色名 / rgb の正規化**

import に追加: `use mat_core::color;` と `use mat_core::error::ErrorKind;`（既存 import と統合）。

`Command::Color` の arm を差し替え:

```rust
        Command::Color {
            node_id,
            endpoint,
            spec,
            transition,
        } => {
            let node = book.resolve_node(&node_id)?;
            let ep = book.resolve_endpoint(node, &endpoint)?;
            Command::Color {
                node_id: NodeRef::Id(node),
                endpoint: EndpointRef::Id(ep),
                spec: resolve_color_spec(&book, spec)?,
                transition,
            }
        }
```

ファイル末尾（tests の前）に free fn を追加:

```rust
/// 色指定の name / rgb を正規化済み RGB（`#rrggbb`）へ確定する。hue/sat 生指定は
/// パススルー。色名解決は node/group/endpoint alias と同じレイヤ（ここ）で行い、
/// 以降の経路（直 / matd）には数値換算可能な形だけが流れる。未知の色名・不正な
/// `--rgb` は kind=Other（main が exit 2 に写す）。
fn resolve_color_spec(book: &AliasBook, spec: ColorSpecArgs) -> Result<ColorSpecArgs, MatError> {
    if let Some(name) = &spec.name {
        let rgb = book.resolve_color_name(name)?;
        return Ok(ColorSpecArgs {
            name: spec.name.clone(),
            rgb: Some(color::hex_string(rgb)),
            hue: None,
            sat: None,
        });
    }
    if let Some(rgb) = &spec.rgb {
        let parsed =
            color::parse_rgb(rgb).map_err(|e| MatError::new(ErrorKind::Other, e))?;
        return Ok(ColorSpecArgs {
            name: None,
            rgb: Some(color::hex_string(parsed)),
            hue: None,
            sat: None,
        });
    }
    Ok(spec)
}
```

（`use crate::cli::ColorSpecArgs;` を import に追加）

`resolve.rs` の tests に追加:

```rust
    #[test]
    fn color_name_resolves_to_normalized_rgb() {
        let dir = store_with("[colors]\nwarm = \"255,140,0\"\n");
        let cmd = Command::Color {
            node_id: NodeRef::Id(5),
            endpoint: EndpointRef::Id(1),
            spec: crate::cli::ColorSpecArgs {
                name: Some("warm".into()),
                rgb: None,
                hue: None,
                sat: None,
            },
            transition: 0,
        };
        match resolve_command(cmd, dir.path()).unwrap() {
            Command::Color { spec, .. } => {
                assert_eq!(spec.name.as_deref(), Some("warm"));
                assert_eq!(spec.rgb.as_deref(), Some("#ff8c00"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
```

- [ ] **Step 5: invoke.rs — `run_color` を `ResolvedColor` 受けに変更**

import に `use mat_core::color::ResolvedColor;` を追加。`run_color` と旧 `resolve_color`（+ その単体テスト `hue_330_sat_80_convert_to_233_203` / `hue_sat_full_scale_caps_at_254` / `sat_50_rounds_to_127` — Task 1 で mat-core に移設済み）を以下で置き換え:

```rust
/// `mat color` の実体。ColorControl の MoveToHueAndSaturation を invoke する。
/// 入力（name / rgb / 度・%）と換算後の 0–254 生値を両方エコーし、`current-hue` /
/// `current-saturation` の読み返しと突合しやすくする。
pub fn run_color(
    store_path: &Path,
    node_id: u64,
    endpoint: u16,
    color: &ResolvedColor,
    transition: u16,
) -> Result<(), MatError> {
    // MoveToHueAndSaturation の引数は <hue> <saturation> <transition>
    // <optionsMask> <optionsOverride>。
    let args = [
        color.hue_raw.to_string(),
        color.sat_raw.to_string(),
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
    let mut body = json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "colorcontrol",
        "command": "move-to-hue-and-saturation",
        "hue": color.hue,
        "saturation": color.sat,
        "hue_raw": color.hue_raw,
        "saturation_raw": color.sat_raw,
        "transition": transition,
        "status": "success",
    });
    if let Some(name) = &color.name {
        body["name"] = json!(name);
    }
    if let Some(rgb) = &color.rgb {
        body["rgb"] = json!(rgb);
    }
    output::emit(body);
    Ok(())
}
```

- [ ] **Step 6: main.rs — Color arm を更新**

```rust
        Command::Color {
            node_id,
            endpoint,
            spec,
            transition,
        } => mat_core::color::resolve_spec(
            spec.name.as_deref(),
            spec.rgb.as_deref(),
            spec.hue,
            spec.sat,
        )
        .and_then(|c| {
            commands::invoke::run_color(&store_path, node_id.id(), endpoint.id(), &c, *transition)
        }),
```

- [ ] **Step 7: matd_client.rs — Color の to_op を更新（+ 既存テスト修正）**

`Command::Color` の arm を差し替え:

```rust
        Command::Color {
            node_id,
            endpoint,
            spec,
            transition,
        } => {
            // 換算は mat 側で 1 箇所（直経路と同じ規則）。matd へは換算済み 0–254 値を
            // 渡し、度 / % / name / rgb は応答エコー用。
            let c = mat_core::color::resolve_spec(
                spec.name.as_deref(),
                spec.rgb.as_deref(),
                spec.hue,
                spec.sat,
            )
            .map_err(|e| e.detail)?;
            let mut op = json!({
                "op": "color", "node_id": node_id.id(), "endpoint": endpoint.id(),
                "hue_raw": c.hue_raw, "saturation_raw": c.sat_raw,
                "hue": c.hue, "saturation": c.sat, "transition": transition,
            });
            if let Some(name) = &c.name {
                op["name"] = json!(name);
            }
            if let Some(rgb) = &c.rgb {
                op["rgb"] = json!(rgb);
            }
            op
        }
```

既存テスト `color_maps_to_color_op_with_converted_values` の構築部を新フィールドに合わせて修正:

```rust
        let cmd = Command::Color {
            node_id: NodeRef::Id(6),
            endpoint: EndpointRef::Id(1),
            spec: crate::cli::ColorSpecArgs {
                name: None,
                rgb: None,
                hue: Some(330),
                sat: Some(80),
            },
            transition: 30,
        };
```

（期待 JSON は変更なし。name/rgb は None なのでキー自体が乗らない）

さらにテスト追加:

```rust
    #[test]
    fn color_name_op_includes_name_and_rgb_echo() {
        // resolve 層通過後の形（name あり + 正規化済み rgb）。
        let cmd = Command::Color {
            node_id: NodeRef::Id(6),
            endpoint: EndpointRef::Id(1),
            spec: crate::cli::ColorSpecArgs {
                name: Some("red".into()),
                rgb: Some("#ff0000".into()),
                hue: None,
                sat: None,
            },
            transition: 0,
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({
                "op":"color","node_id":6,"endpoint":1,
                "hue_raw":0,"saturation_raw":254,
                "hue":0,"saturation":100,"transition":0,
                "name":"red","rgb":"#ff0000"
            })
        );
    }
```

- [ ] **Step 8: 全テストが通ることを確認**

Run: `cargo test -p mat`
Expected: PASS（既存の `--hue`/`--sat` テスト含め全件。`color_maps_to_color_op_with_converted_values` は修正後の形で PASS）

- [ ] **Step 9: コミット**

```bash
git add crates/mat/src/cli.rs crates/mat/src/resolve.rs crates/mat/src/commands/invoke.rs crates/mat/src/main.rs crates/mat/src/matd_client.rs crates/mat/tests/integration.rs
git commit -m "feat(mat): mat color に --name / --rgb 指定を追加（RGB→HSV で hue/sat へ一本化）"
```

---

### Task 4: `mat group color-temp` / `mat group color`（直経路 + matd op 変換）

**Files:**
- Modify: `crates/mat/src/cli.rs`（`GroupCommand::ColorTemp` / `GroupCommand::Color` 追加）
- Modify: `crates/mat/src/commands/group.rs`（送信部の抽出 + `color_temp` / `color`）
- Modify: `crates/mat/src/resolve.rs`（新 variant の alias / 色名解決）
- Modify: `crates/mat/src/main.rs`（新 variant のディスパッチ）
- Modify: `crates/mat/src/matd_client.rs`（`group_color_temp` / `group_color` op への変換 + テスト）
- Test: `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: Task 3 の `ColorSpecArgs` / `resolve_color_spec`、既存 `commands::invoke::resolve_color_temp`、`mat_core::color::resolve_spec`
- Produces:
  - `commands::group::color_temp(store_path, group_id: u16, kelvin: u32, mireds: u16, transition: u16, endpoint: u16)`
  - `commands::group::color(store_path, group_id: u16, color: &ResolvedColor, transition: u16, endpoint: u16)`
  - matd op JSON: `{"op":"group_color_temp","group_id":G,"mireds":M,"kelvin":K,"transition":T,"endpoint":E}` / `{"op":"group_color","group_id":G,"hue_raw":H,"saturation_raw":S,"hue":deg,"saturation":pct,"transition":T,"endpoint":E,("name":...,)("rgb":...)}`（Task 5 の matd 側が受ける）

- [ ] **Step 1: 失敗する統合テストを書く**

`crates/mat/tests/integration.rs` の group テスト群の後に追加:

```rust
#[test]
fn group_color_temp_converts_kelvin_and_reports_sent() {
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args(["group", "color-temp", "--group", "1", "--kelvin", "2700"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"group_id\":1"))
        .stdout(predicate::str::contains(
            "\"command\":\"move-to-color-temperature\"",
        ))
        .stdout(predicate::str::contains("\"kelvin\":2700"))
        .stdout(predicate::str::contains("\"mireds\":370"))
        .stdout(predicate::str::contains("\"status\":\"sent\""))
        .stdout(predicate::str::contains("unacknowledged"));
    // 換算済み mireds + group multicast 宛先（0xffffffffffff0001）。
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("colorcontrol move-to-color-temperature 370 0 0 0 0xffffffffffff0001 1"),
        "expected groupcast color-temp argv: {recorded}"
    );
}

#[test]
fn group_color_temp_requires_exactly_one_of_kelvin_or_mireds() {
    let store = store_with_node5();
    mat(store.path())
        .args(["group", "color-temp", "--group", "1"])
        .assert()
        .code(2);
    mat(store.path())
        .args([
            "group", "color-temp", "--group", "1", "--kelvin", "2700", "--mireds", "370",
        ])
        .assert()
        .code(2);
}

#[test]
fn group_color_name_converts_and_reports_sent() {
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args(["group", "color", "--group", "1", "--name", "blue"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"group_id\":1"))
        .stdout(predicate::str::contains(
            "\"command\":\"move-to-hue-and-saturation\"",
        ))
        .stdout(predicate::str::contains("\"name\":\"blue\""))
        .stdout(predicate::str::contains("\"rgb\":\"#0000ff\""))
        .stdout(predicate::str::contains("\"hue_raw\":169"))
        .stdout(predicate::str::contains("\"saturation_raw\":254"))
        .stdout(predicate::str::contains("\"status\":\"sent\""));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("colorcontrol move-to-hue-and-saturation 169 254 0 0 0 0xffffffffffff0001 1"),
        "expected groupcast color argv: {recorded}"
    );
}

#[test]
fn group_color_hue_sat_and_rgb_forms_work() {
    let store = store_with_node5();
    // 生指定（既存単体 color と同じ換算）。
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args([
            "group", "color", "--group", "1", "--hue", "330", "--sat", "80", "--transition", "30",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"transition\":30"));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("colorcontrol move-to-hue-and-saturation 233 203 30 0 0 0xffffffffffff0001 1"),
        "expected raw hue/sat groupcast argv: {recorded}"
    );
    // RGB 指定。
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .args(["group", "color", "--group", "1", "--rgb", "255,0,170"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"rgb\":\"#ff00aa\""));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        recorded.contains("colorcontrol move-to-hue-and-saturation 226 254 0 0 0 0xffffffffffff0001 1"),
        "expected rgb groupcast argv: {recorded}"
    );
}

#[test]
fn group_color_spec_systems_are_mutually_exclusive() {
    let store = store_with_node5();
    mat(store.path())
        .args(["group", "color", "--group", "1", "--name", "red", "--hue", "0", "--sat", "100"])
        .assert()
        .code(2);
    mat(store.path())
        .args(["group", "color", "--group", "1"])
        .assert()
        .code(2);
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat --test integration group_color 2>&1 | tail -20`
Expected: FAIL（`color-temp` / `color` は group の unknown subcommand → exit 2 で success 期待が落ちる）

- [ ] **Step 3: cli.rs — GroupCommand に variant 追加**

`GroupCommand` enum の `Invoke` の後に追加:

```rust
    /// ColorControl MoveToColorTemperature を group へ multicast する高頻度
    /// ショートカット（`mat color-temp` の group 版）。`--kelvin`（mireds へ換算）
    /// か `--mireds` のどちらか一方。unacknowledged groupcast なので "sent" のみ
    /// 報告する。点灯中でないと反映されない（ExecuteIfOff は立てない）。
    ColorTemp {
        /// Matter GroupId、または aliases.toml の group alias。
        #[arg(short = 'g', long = "group", value_name = "ID|ALIAS")]
        group_id: GroupRef,
        /// 色温度（ケルビン）。値域は mireds が u16 に収まる 16..=1000000。
        #[arg(
            long,
            value_name = "K",
            conflicts_with = "mireds",
            required_unless_present = "mireds",
            value_parser = clap::value_parser!(u32).range(16..=1_000_000)
        )]
        kelvin: Option<u32>,
        /// 色温度（mireds）。`--kelvin` と排他。
        #[arg(long, value_name = "M", value_parser = clap::value_parser!(u16).range(1..))]
        mireds: Option<u16>,
        /// 遷移時間（0.1 秒単位、既定 0 = 即時）。例: 30 = 3 秒。
        #[arg(long, value_name = "DS", default_value_t = 0)]
        transition: u16,
        /// 宛先エンドポイント（既定 1、数値のみ — ノード文脈が無いため alias 不可）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },

    /// ColorControl MoveToHueAndSaturation を group へ multicast する高頻度
    /// ショートカット（`mat color` の group 版）。色は `--name` / `--rgb` /
    /// `--hue`+`--sat` の 1 系統で指定（名前・RGB は明度を変えない）。
    /// unacknowledged groupcast なので "sent" のみ報告する。点灯中でないと
    /// 反映されない（ExecuteIfOff は立てない）。
    Color {
        /// Matter GroupId、または aliases.toml の group alias。
        #[arg(short = 'g', long = "group", value_name = "ID|ALIAS")]
        group_id: GroupRef,
        #[command(flatten)]
        spec: ColorSpecArgs,
        /// 遷移時間（0.1 秒単位、既定 0 = 即時）。例: 30 = 3 秒。
        #[arg(long, value_name = "DS", default_value_t = 0)]
        transition: u16,
        /// 宛先エンドポイント（既定 1、数値のみ — ノード文脈が無いため alias 不可）。
        #[arg(short = 'e', long, value_name = "EP", default_value_t = 1)]
        endpoint: u16,
    },
```

- [ ] **Step 4: group.rs — 送信部を抽出し color_temp / color を実装**

`invoke` の本体（`Store::open` から `if !out.success()` の判定まで）を新 fn `send` に移し、`invoke` は `send` + emit だけにする:

```rust
/// groupcast の送信部（出力なし）。invoke / color-temp / color ショートカットで共有。
/// groupcast は unacknowledged で応答（SUCCESS 行）は返らないため operation_succeeded
/// は見ない。送信プロセスが正常終了したかだけで「送った」と判断する。
fn send(
    store_path: &Path,
    group_id: u16,
    cluster: &str,
    command: &str,
    args: &[String],
    endpoint: u16,
) -> Result<(), MatError> {
    // 特定 node 宛ではないので require_node はしないが、chip-tool の永続ストレージ
    // （焼いた group 鍵を含む）参照のため store は必要。
    let store = Store::open(store_path)?;
    let chip = ChipTool::new(store.root());

    let group_node_id = group_node_id(group_id);

    // invoke と同じ並び: `<cluster> <command> [args...] <宛先> <endpoint>`。
    // 宛先に group node-id を置くと chip-tool が multicast 送信する。
    let mut argv = vec![cluster.to_string(), command.to_string()];
    argv.extend(args.iter().cloned());
    argv.push(group_node_id.clone());
    argv.push(endpoint.to_string());

    let out = chip.run(argv)?;

    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("group invoke {cluster}/{command} to group {group_id} failed"),
        ));
    }
    if !out.success() {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!(
                "chip-tool group invoke exited with {:?} (group {group_id})",
                out.code
            ),
        ));
    }
    Ok(())
}

/// `mat group invoke` — group へ multicast でコマンドを送る。
pub fn invoke(
    store_path: &Path,
    group_id: u16,
    cluster: &str,
    command: &str,
    args: &[String],
    endpoint: u16,
) -> Result<(), MatError> {
    send(store_path, group_id, cluster, command, args, endpoint)?;
    output::emit(json!({
        "group_id": group_id,
        "cluster": cluster,
        "command": command,
        "endpoint": endpoint,
        "status": "sent",
        "note": "unacknowledged groupcast; per-device delivery not confirmed",
    }));
    Ok(())
}

/// `mat group color-temp` — MoveToColorTemperature を groupcast する
/// （`mat color-temp` の group 版）。入力 kelvin と換算後 mireds を両方エコーする。
pub fn color_temp(
    store_path: &Path,
    group_id: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
    endpoint: u16,
) -> Result<(), MatError> {
    // MoveToColorTemperature の引数は <mireds> <transition> <optionsMask> <optionsOverride>。
    let args = [
        mireds.to_string(),
        transition.to_string(),
        "0".to_string(),
        "0".to_string(),
    ];
    send(
        store_path,
        group_id,
        "colorcontrol",
        "move-to-color-temperature",
        &args,
        endpoint,
    )?;
    output::emit(json!({
        "group_id": group_id,
        "cluster": "colorcontrol",
        "command": "move-to-color-temperature",
        "kelvin": kelvin,
        "mireds": mireds,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "note": "unacknowledged groupcast; per-device delivery not confirmed",
    }));
    Ok(())
}

/// `mat group color` — MoveToHueAndSaturation を groupcast する（`mat color` の
/// group 版）。入力（name / rgb / 度・%）と換算後の 0–254 生値を両方エコーする。
pub fn color(
    store_path: &Path,
    group_id: u16,
    color: &ResolvedColor,
    transition: u16,
    endpoint: u16,
) -> Result<(), MatError> {
    // MoveToHueAndSaturation の引数は <hue> <saturation> <transition>
    // <optionsMask> <optionsOverride>。
    let args = [
        color.hue_raw.to_string(),
        color.sat_raw.to_string(),
        transition.to_string(),
        "0".to_string(),
        "0".to_string(),
    ];
    send(
        store_path,
        group_id,
        "colorcontrol",
        "move-to-hue-and-saturation",
        &args,
        endpoint,
    )?;
    let mut body = json!({
        "group_id": group_id,
        "cluster": "colorcontrol",
        "command": "move-to-hue-and-saturation",
        "hue": color.hue,
        "saturation": color.sat,
        "hue_raw": color.hue_raw,
        "saturation_raw": color.sat_raw,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "note": "unacknowledged groupcast; per-device delivery not confirmed",
    });
    if let Some(name) = &color.name {
        body["name"] = json!(name);
    }
    if let Some(rgb) = &color.rgb {
        body["rgb"] = json!(rgb);
    }
    output::emit(body);
    Ok(())
}
```

（import に `use mat_core::color::ResolvedColor;` を追加）

- [ ] **Step 5: resolve.rs — 新 variant の解決**

`GroupCommand` の match に arm 追加（`Invoke` の後）:

```rust
                GroupCommand::ColorTemp {
                    group_id,
                    kelvin,
                    mireds,
                    transition,
                    endpoint,
                } => GroupCommand::ColorTemp {
                    group_id: GroupRef::Id(book.resolve_group(&group_id)?),
                    kelvin,
                    mireds,
                    transition,
                    endpoint,
                },
                GroupCommand::Color {
                    group_id,
                    spec,
                    transition,
                    endpoint,
                } => GroupCommand::Color {
                    group_id: GroupRef::Id(book.resolve_group(&group_id)?),
                    spec: resolve_color_spec(&book, spec)?,
                    transition,
                    endpoint,
                },
```

- [ ] **Step 6: main.rs — ディスパッチ追加**

`Command::Group` の match に arm 追加（`Invoke` の後）:

```rust
            GroupCommand::ColorTemp {
                group_id,
                kelvin,
                mireds,
                transition,
                endpoint,
            } => {
                // --kelvin / --mireds を (mireds, kelvin) に解決（単体 color-temp と同じ規則）。
                let (mireds, kelvin) = commands::invoke::resolve_color_temp(*kelvin, *mireds);
                commands::group::color_temp(
                    &store_path,
                    group_id.id(),
                    kelvin,
                    mireds,
                    *transition,
                    *endpoint,
                )
            }
            GroupCommand::Color {
                group_id,
                spec,
                transition,
                endpoint,
            } => mat_core::color::resolve_spec(
                spec.name.as_deref(),
                spec.rgb.as_deref(),
                spec.hue,
                spec.sat,
            )
            .and_then(|c| {
                commands::group::color(&store_path, group_id.id(), &c, *transition, *endpoint)
            }),
```

- [ ] **Step 7: matd_client.rs — 新 op への変換**

`GroupCommand` の match（`Invoke` の後、`Grant` の前）に追加:

```rust
            GroupCommand::ColorTemp {
                group_id,
                kelvin,
                mireds,
                transition,
                endpoint,
            } => {
                // 換算は mat 側で 1 箇所（直経路と同じ規則）。kelvin はエコー用。
                let (mireds, kelvin) = crate::commands::invoke::resolve_color_temp(*kelvin, *mireds);
                json!({
                    "op": "group_color_temp", "group_id": group_id.id(),
                    "mireds": mireds, "kelvin": kelvin,
                    "transition": transition, "endpoint": endpoint,
                })
            }
            GroupCommand::Color {
                group_id,
                spec,
                transition,
                endpoint,
            } => {
                // 換算は mat 側で 1 箇所。度 / % / name / rgb は応答エコー用。
                let c = mat_core::color::resolve_spec(
                    spec.name.as_deref(),
                    spec.rgb.as_deref(),
                    spec.hue,
                    spec.sat,
                )
                .map_err(|e| e.detail)?;
                let mut op = json!({
                    "op": "group_color", "group_id": group_id.id(),
                    "hue_raw": c.hue_raw, "saturation_raw": c.sat_raw,
                    "hue": c.hue, "saturation": c.sat,
                    "transition": transition, "endpoint": endpoint,
                });
                if let Some(name) = &c.name {
                    op["name"] = json!(name);
                }
                if let Some(rgb) = &c.rgb {
                    op["rgb"] = json!(rgb);
                }
                op
            }
```

tests に追加:

```rust
    #[test]
    fn group_color_temp_maps_to_group_color_temp_op() {
        let cmd = Command::Group {
            action: GroupCommand::ColorTemp {
                group_id: GroupRef::Id(1),
                kelvin: Some(2700),
                mireds: None,
                transition: 0,
                endpoint: 1,
            },
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({
                "op":"group_color_temp","group_id":1,
                "mireds":370,"kelvin":2700,"transition":0,"endpoint":1
            })
        );
    }

    #[test]
    fn group_color_maps_to_group_color_op_with_echo() {
        let cmd = Command::Group {
            action: GroupCommand::Color {
                group_id: GroupRef::Id(1),
                spec: crate::cli::ColorSpecArgs {
                    name: Some("blue".into()),
                    rgb: Some("#0000ff".into()),
                    hue: None,
                    sat: None,
                },
                transition: 0,
                endpoint: 1,
            },
        };
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({
                "op":"group_color","group_id":1,
                "hue_raw":169,"saturation_raw":254,
                "hue":240,"saturation":100,"transition":0,"endpoint":1,
                "name":"blue","rgb":"#0000ff"
            })
        );
    }
```

- [ ] **Step 8: 全テストが通ることを確認**

Run: `cargo test -p mat`
Expected: PASS（Step 1 の統合テスト含め全件）

- [ ] **Step 9: コミット**

```bash
git add crates/mat/src/cli.rs crates/mat/src/commands/group.rs crates/mat/src/resolve.rs crates/mat/src/main.rs crates/mat/src/matd_client.rs crates/mat/tests/integration.rs
git commit -m "feat(mat): mat group color-temp / group color ショートカット（groupcast、換算付き）"
```

---

### Task 5: matd — `group_color_temp` / `group_color` op と Color の name/rgb エコー

**Files:**
- Modify: `crates/matd/src/protocol.rs`（新 op 2 つ + `Op::Color` に optional name/rgb）
- Modify: `crates/matd/src/server.rs`（新 op のハンドラ + Color エコー）
- Test: `crates/matd/tests/integration.rs`

**Interfaces:**
- Consumes: Task 4 が送る op JSON（`group_color_temp` / `group_color`、`color` の name/rgb キー）
- Produces: 応答 JSON — 直経路の `mat group color-temp` / `mat group color` と同形（`status: "sent"` + エコー）

- [ ] **Step 1: 失敗する protocol 単体テストを書く**

`crates/matd/src/protocol.rs` の tests に追加:

```rust
    #[test]
    fn group_color_temp_parses_with_no_node_or_cmdline() {
        let r = parse(
            r#"{"op":"group_color_temp","group_id":1,"mireds":370,"kelvin":2700,"transition":0,"endpoint":1}"#,
        );
        // multicast 宛で単一 node を持たず、group_invoke と同じく専用ハンドラで捌く。
        assert_eq!(r.op.node_id(), None);
        assert!(r.op.to_cmdline().is_none());
    }

    #[test]
    fn group_color_parses_with_optional_name_and_rgb() {
        let r = parse(
            r#"{"op":"group_color","group_id":1,"hue_raw":169,"saturation_raw":254,"hue":240,"saturation":100,"name":"blue","rgb":"#0000ff","endpoint":1}"#,
        );
        assert_eq!(r.op.node_id(), None);
        assert!(r.op.to_cmdline().is_none());
        assert!(matches!(r.op, Op::GroupColor { name: Some(_), .. }));
        // name / rgb は省略可（--hue/--sat 生指定のとき）。
        let r = parse(
            r#"{"op":"group_color","group_id":1,"hue_raw":233,"saturation_raw":203,"hue":330,"saturation":80,"endpoint":1}"#,
        );
        assert!(matches!(r.op, Op::GroupColor { name: None, rgb: None, .. }));
    }

    #[test]
    fn color_accepts_optional_name_and_rgb_echo() {
        // 単体 color も name / rgb エコーを受ける（cmdline には乗らない）。
        let r = parse(
            r#"{"op":"color","node_id":6,"endpoint":1,"hue_raw":0,"saturation_raw":254,"hue":0,"saturation":100,"name":"red","rgb":"#ff0000"}"#,
        );
        assert_eq!(
            r.op.to_cmdline().unwrap(),
            "colorcontrol move-to-hue-and-saturation 0 254 0 0 0 6 1"
        );
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd protocol 2>&1 | head -30`
Expected: FAIL（`group_color_temp` は unknown variant のパニック / `Op::GroupColor` 未定義のコンパイルエラー）

- [ ] **Step 3: protocol.rs 実装**

`Op::Color` に optional フィールド追加（`transition` の前）:

```rust
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        rgb: Option<String>,
```

（doc コメントに「`name` / `rgb` は name / RGB 指定時の応答エコー用（任意）」を追記）

`Op` enum に variant 追加（`GroupInvoke` の後）:

```rust
    /// ColorControl MoveToColorTemperature の group ショートカット
    /// （`mat group color-temp` 相当、groupcast）。`mireds` は mat 側で換算済み、
    /// `kelvin` は応答エコー用。unacknowledged なので "sent" のみ報告する。
    GroupColorTemp {
        group_id: u16,
        mireds: u16,
        kelvin: u32,
        #[serde(default)]
        transition: u16,
        endpoint: u16,
    },
    /// ColorControl MoveToHueAndSaturation の group ショートカット
    /// （`mat group color` 相当、groupcast）。raw は mat 側で換算済み、
    /// 度・%・name・rgb は応答エコー用。unacknowledged なので "sent" のみ報告する。
    GroupColor {
        group_id: u16,
        hue_raw: u8,
        saturation_raw: u8,
        hue: u16,
        saturation: u8,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        rgb: Option<String>,
        #[serde(default)]
        transition: u16,
        endpoint: u16,
    },
```

`node_id()` の None 側 arm に `Op::GroupColorTemp { .. } | Op::GroupColor { .. }` を追加。
`to_cmdline()` の `return None` arm にも同様に追加。
`Op::Color` の分解パターン（`to_cmdline` 内）はフィールド追加後も `..` で吸収されることを確認（既存パターンが `..` を含むのでそのまま）。

- [ ] **Step 4: server.rs 実装**

`run_op` の match に追加（`GroupInvoke` の後）:

```rust
        Op::GroupColorTemp { .. } | Op::GroupColor { .. } => {
            group_color_op(op, backend, store_path).await
        }
```

`simple_op` の `Op::Color` arm を name/rgb エコー付きに変更:

```rust
        Op::Color {
            node_id,
            endpoint,
            hue_raw,
            saturation_raw,
            hue,
            saturation,
            name,
            rgb,
            transition,
        } => {
            let mut body = json!({
                "node_id": node_id, "endpoint": endpoint,
                "cluster": "colorcontrol", "command": "move-to-hue-and-saturation",
                // 入力の度 / % と換算後 0–254 生値を両方エコー（読み返し突合用; 直経路と同形）。
                "hue": hue, "saturation": saturation,
                "hue_raw": hue_raw, "saturation_raw": saturation_raw,
                "transition": transition,
                "status": "success",
            });
            if let Some(n) = name {
                body["name"] = json!(n);
            }
            if let Some(r) = rgb {
                body["rgb"] = json!(r);
            }
            body
        }
```

`simple_op` 末尾の `unreachable!` arm に `Op::GroupColorTemp { .. } | Op::GroupColor { .. }` を追加。

新ハンドラを `group_invoke` の後に追加:

```rust
/// group 版 color-temp / color ショートカット（`mat group color-temp` / `mat group
/// color` 相当）。groupcast なので group_invoke と同じく unacknowledged（ws 応答が
/// 返れば "sent"）。換算は mat 側で済んでおり、ここは送信とエコーのみ。
async fn group_color_op(
    op: &Op,
    backend: &ChipToolBackend,
    store_path: &Path,
) -> Result<Value, MatError> {
    // 特定 node 宛ではないが、chip-tool の永続ストレージ（焼いた group 鍵）参照の
    // ため store は必要。
    let _store = Store::open(store_path)?;
    match op {
        Op::GroupColorTemp {
            group_id,
            mireds,
            kelvin,
            transition,
            endpoint,
        } => {
            let line = format!(
                "colorcontrol move-to-color-temperature {mireds} {transition} 0 0 {} {endpoint}",
                group_node_id(*group_id)
            );
            let _ = backend.run_cmdline(&line).await?;
            Ok(json!({
                "group_id": group_id, "cluster": "colorcontrol",
                "command": "move-to-color-temperature",
                "kelvin": kelvin, "mireds": mireds, "transition": transition,
                "endpoint": endpoint, "status": "sent",
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            }))
        }
        Op::GroupColor {
            group_id,
            hue_raw,
            saturation_raw,
            hue,
            saturation,
            name,
            rgb,
            transition,
            endpoint,
        } => {
            let line = format!(
                "colorcontrol move-to-hue-and-saturation {hue_raw} {saturation_raw} {transition} 0 0 {} {endpoint}",
                group_node_id(*group_id)
            );
            let _ = backend.run_cmdline(&line).await?;
            let mut body = json!({
                "group_id": group_id, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": hue, "saturation": saturation,
                "hue_raw": hue_raw, "saturation_raw": saturation_raw,
                "transition": transition, "endpoint": endpoint,
                "status": "sent",
                "note": "unacknowledged groupcast; per-device delivery not confirmed",
            });
            if let Some(n) = name {
                body["name"] = json!(n);
            }
            if let Some(r) = rgb {
                body["rgb"] = json!(r);
            }
            Ok(body)
        }
        _ => unreachable!("group_color_op called with non group color op"),
    }
}
```

- [ ] **Step 5: matd 統合テストを追加**

`crates/matd/tests/integration.rs` の `group_invoke_reports_sent` の後に追加（同じ helper 構成: `spawn_fake_ws` / `make_store` / `start_matd` / `roundtrip`。既存テストの終了処理（`handle.abort()` 等）があれば同じ形に合わせる）:

```rust
/// group_color_temp: 換算済み mireds で groupcast し、kelvin / mireds をエコー、
/// status="sent"（unacknowledged; 直経路 `mat group color-temp` と同形）。
#[tokio::test]
async fn group_color_temp_reports_sent_with_echo() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, _handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({"id":1,"op":"group_color_temp","group_id":1,"mireds":370,"kelvin":2700,"transition":0,"endpoint":1})],
    )
    .await;
    assert_eq!(resps[0]["status"], "sent");
    assert_eq!(resps[0]["kelvin"], 2700);
    assert_eq!(resps[0]["mireds"], 370);
    assert_eq!(resps[0]["command"], "move-to-color-temperature");
    assert!(resps[0]["timestamp"].is_string());
}

/// group_color: 換算済み raw で groupcast し、name / rgb / 度・% をエコー、
/// status="sent"（直経路 `mat group color` と同形）。
#[tokio::test]
async fn group_color_reports_sent_with_echo() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, _handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({"id":1,"op":"group_color","group_id":1,"hue_raw":169,"saturation_raw":254,"hue":240,"saturation":100,"name":"blue","rgb":"#0000ff","transition":0,"endpoint":1})],
    )
    .await;
    assert_eq!(resps[0]["status"], "sent");
    assert_eq!(resps[0]["name"], "blue");
    assert_eq!(resps[0]["rgb"], "#0000ff");
    assert_eq!(resps[0]["hue_raw"], 169);
    assert_eq!(resps[0]["command"], "move-to-hue-and-saturation");
}

/// 単体 color の name / rgb エコー（op に載せた任意フィールドが応答へ返る）。
#[tokio::test]
async fn color_echoes_optional_name_and_rgb() {
    let port = spawn_fake_ws().await;
    let (_dir, store_path) = make_store();
    let (socket, _handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({"id":1,"op":"color","node_id":1,"endpoint":1,"hue_raw":0,"saturation_raw":254,"hue":0,"saturation":100,"name":"red","rgb":"#ff0000","transition":0})],
    )
    .await;
    assert_eq!(resps[0]["status"], "success");
    assert_eq!(resps[0]["name"], "red");
    assert_eq!(resps[0]["rgb"], "#ff0000");
}
```

（import で `group_node_id` が必要なら `use mat_core::group::group_node_id;` — server.rs には既にある）

- [ ] **Step 6: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: PASS（protocol 単体 + 統合の新テスト含め全件）

- [ ] **Step 7: コミット**

```bash
git add crates/matd/src/protocol.rs crates/matd/src/server.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): group_color_temp / group_color op と color の name/rgb エコー"
```

---

### Task 6: ドキュメント更新・バージョン・最終チェック

**Files:**
- Modify: `README.md`（State operations の色ブロック、Groupcast 節、Aliases 節、matd 対応 op）
- Modify: `CLAUDE.md`（scope reminder の aliases 例外に `[colors]` を追記）
- Modify: `Cargo.toml`（workspace version 0.13.0 → 0.14.0）

**Interfaces:**
- Consumes: Task 1–5 の確定仕様（コマンド名・フラグ・出力スキーマ・エラー）

- [ ] **Step 1: CLAUDE.md の scope reminder を更新**

「Scope reminders」の最初の項目の例外文を以下に差し替え:

```markdown
- Resolving human names on the wire or in the backend (chip-tool / matd always
  receive numeric values). The only exception: if `<store>/aliases.toml`
  exists, the CLI layer resolves node / group / endpoint aliases — and color
  names via `[colors]` (RGB-defined, overriding the built-in color table in
  `mat-core`) — to numbers right after arg parsing — optional, local, and
  absent-file = no behavior change (built-in color names still work without
  the file). Cluster / attribute names stay chip-tool notation (no aliasing).
```

- [ ] **Step 2: README を更新**

1. **State operations の色ブロック**（`mat color --node 5 --hue 330 --sat 80` のあたり）: hue/sat の説明段落の後に 3 系統の説明と例を追加:

```markdown
# Named colors and RGB: --name looks up a built-in table (red / pink / orange /
# purple / cyan / green / blue / yellow / magenta / white; extend or override
# via [colors] in aliases.toml), --rgb takes #rrggbb / rrggbb / R,G,B. Both are
# converted RGB -> HSV -> hue/sat; the V (brightness) component is discarded,
# so these set the color only and never change brightness (use LevelControl
# for that). `--name white` naturally lands on sat=0 (desaturate); color-temp
# can also produce white but through a different pipeline — both are kept.
# The three spec systems (--name / --rgb / --hue+--sat) are mutually exclusive.
mat color --node 5 --name pink
mat color --node 5 --rgb "#ff00aa"
mat color --node 5 --rgb 255,0,170
```

2. **State operations の出力例**: 既存 `// color` の JSON 例の後に name 指定時の例を追加:

```markdown
// color with --name / --rgb — additionally echoes the input name and the
// normalized #rrggbb so the conversion can be audited
{ "timestamp": "...", "node_id": 5, "endpoint": 1, "cluster": "colorcontrol", "command": "move-to-hue-and-saturation", "hue": 350, "saturation": 25, "hue_raw": 247, "saturation_raw": 63, "transition": 0, "name": "pink", "rgb": "#ffc0cb", "status": "success" }
```

3. **Groupcast 節**（`### Routing through matd` の前）: group ショートカットの説明と例を追加:

```markdown
Color shortcuts for groups (same conversions as the single-node `mat
color-temp` / `mat color`, delivered as an unacknowledged groupcast — the
result is `"status": "sent"` only; per-device delivery is not confirmed).
Like all ColorControl commands sent with optionsMask=0, they only take effect
on devices that are currently on:

```bash
mat group color-temp --group 1 --kelvin 2700
mat group color --group 1 --name pink
mat group color --group 1 --rgb "#ff00aa" --transition 30
mat group color --group 1 --hue 330 --sat 80
```
```

4. **Aliases 節**: TOML 例に `[colors]` を追加し、箇条書きに 1 項目追加:

TOML 例の末尾に:

```toml
[colors]
warm = "#ff8c00"
mypink = "255,182,193"
```

箇条書き（`endpoints` の項目の後）に:

```markdown
- `colors`: custom color name → RGB value (`#rrggbb` / `rrggbb` / `R,G,B`),
  used by `--name` in `color` / `group color`. Entries are defined as RGB and
  go through the same RGB → HSV pipeline as `--rgb`. A user-defined name
  **overrides** the built-in color table (you can redefine `red`). Without the
  file the built-in table still works. A value that does not parse as RGB is
  `store_parse` (exit `10`); an unknown color name is a CLI argument error
  (exit `2`) listing the known names.
```

5. **matd 対応 op の列挙**（line 484 近辺 `color-temp` / `color` / `describe` / `group` の文）: `group`（provision / invoke / color-temp / color; grant は直経路のみ）である旨を確認し、必要なら追記。

- [ ] **Step 3: バージョンを 0.14.0 に**

`Cargo.toml` の `[workspace.package] version = "0.13.0"` → `"0.14.0"`。
`cargo build` を一度実行して `Cargo.lock` のバージョン行も更新されることを確認し、両方コミットに含める。

- [ ] **Step 4: 最終チェック**

Run: `task check`
Expected: fmt:check / clippy (-D warnings) / 全テスト PASS

- [ ] **Step 5: コミット**

```bash
git add README.md CLAUDE.md Cargo.toml Cargo.lock
git commit -m "docs: 色指定 3 系統・group color ショートカット・[colors] を反映、0.14.0"
```

---

## 受け入れ条件との対応

| issue #6 の条件 | 担保するタスク / テスト |
|---|---|
| `mat group color-temp` / `mat group color` が換算付きで動作 | Task 4 統合テスト（fake で groupcast argv 確認）+ Task 5（matd） |
| `--name red` / `--name white`（sat=0）/ `--rgb #ff00aa` / `--rgb 255,0,170` / `--hue/--sat` が単体・group 双方で動く | Task 3 / Task 4 統合テスト |
| RGB→HSV 経由で hue/sat に落ち、入力と生値を両方エコー | Task 1 単体テスト + Task 3/4 の stdout 検証 |
| 3 系統の排他制御 | Task 3 `color_spec_systems_are_mutually_exclusive` / Task 4 group 版 |
| `[colors]` カスタム色名・組み込み上書き・ファイル無し不変・破損 store_parse(10) | Task 2 単体テスト + Task 3 統合テスト |
| CLAUDE.md scope reminder / README aliases 節に `[colors]` | Task 6 |
| mat-core 単体テスト + fake 統合テスト、`task check` 通過 | 各タスク + Task 6 Step 4 |
| README / --help に色指定方法と「明度は変えない」注記 | Task 3 Step 3（--help）/ Task 6 Step 2（README） |
