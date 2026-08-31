//! 経路非依存の入力換算（CLI 入力 → Matter 生値）。native 直経路
//! （`native_direct`）と matd 経路（`matd_client::to_op`）の両方が使う。

/// `mat color-temp` の `--kelvin` / `--mireds`（排他・どちらか必須）を
/// `(mireds, kelvin)` に解決する。与えられなかった側は `round(1_000_000 / x)` で
/// 補完し、出力 JSON へのエコー（読み返し突合用）に使う。決定的な数値換算のみで、
/// デバイス対応範囲（color-temp-physical-min/max-mireds）の検証はしない
/// （範囲外はデバイス側が clamp する）。
pub(crate) fn resolve_color_temp(kelvin: Option<u32>, mireds: Option<u16>) -> (u16, u32) {
    // round(1_000_000 / v)。K→mireds も mireds→K も同じ逆数換算。
    fn recip(v: u32) -> u32 {
        (1_000_000 + v / 2) / v
    }
    match (kelvin, mireds) {
        // CLI 側の値域制約（16..=1_000_000 K）により mireds は 1..=62500 で u16 に収まる。
        (Some(k), None) => (recip(k) as u16, k),
        (None, Some(m)) => (m, recip(u32::from(m))),
        // clap がどちらか一方のみを強制する。
        _ => unreachable!("clap enforces exactly one of --kelvin / --mireds"),
    }
}

/// `mat level` の `--percent`（0–100）を Matter LevelControl の 0–254 生値へ
/// 換算する（`color` の hue/sat と同じ整数換算: round(v / full * 254)、255 は
/// 予約値）。デバイス対応範囲（min/max level）の検証はしない（範囲外は
/// デバイス側が clamp する）。
pub(crate) fn resolve_level(percent: u8) -> u8 {
    ((u32::from(percent) * 254 + 50) / 100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kelvin_2700_converts_to_370_mireds() {
        assert_eq!(resolve_color_temp(Some(2700), None), (370, 2700));
    }

    #[test]
    fn kelvin_6500_rounds_to_154_mireds() {
        // 1_000_000 / 6500 = 153.85 → round = 154。
        assert_eq!(resolve_color_temp(Some(6500), None), (154, 6500));
    }

    #[test]
    fn mireds_direct_computes_kelvin_echo() {
        // 1_000_000 / 370 = 2702.7 → round = 2703（エコー用の逆換算）。
        assert_eq!(resolve_color_temp(None, Some(370)), (370, 2703));
    }

    #[test]
    fn resolve_level_rounds_percent_to_254_scale() {
        // round(percent / 100 * 254)。255 は予約値なので 100% は 254。
        assert_eq!(resolve_level(0), 0);
        assert_eq!(resolve_level(1), 3);
        assert_eq!(resolve_level(50), 127);
        assert_eq!(resolve_level(100), 254);
    }
}
