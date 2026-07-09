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
        return Err(format!(
            "invalid RGB '{s}' (expected #rrggbb, rrggbb, or R,G,B)"
        ));
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
            assert!(
                builtin_color(name).is_some(),
                "missing builtin color {name}"
            );
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
