//! `mat invoke` — コマンドを実行する。
//!
//! バックエンド実行は native 直経路（`native_direct`）が担う（M8c-3 で chip-tool
//! 経路は撤去）。成功 JSON の形は `mat_core::body`（直経路・matd 共有の単一
//! ソース）、このモジュールは stdout への emit のみを持つ。経路非依存の入力換算
//! ヘルパー（`resolve_color_temp` / `resolve_level`）も持つ。`mat on` / `mat off`
//! は OnOff クラスタの On/Off コマンドを invoke にマップしたショートカット。

use mat_core::color::ResolvedColor;
use mat_core::{body, output};

/// `invoke` / `on` / `off` の成功 JSON を stdout へ emit する（body は `mat_core::body` 共有）。
pub(crate) fn emit_invoke_success(node_id: u64, endpoint: u16, cluster: &str, command: &str) {
    output::emit(body::invoke_success(node_id, endpoint, cluster, command));
}

/// `color-temp` の成功 JSON を stdout へ emit する（body は `mat_core::body` 共有）。
/// 出力には入力の kelvin と換算後の mireds を両方載せ、`color-temperature-mireds`
/// の読み返しと突合しやすくする。
pub(crate) fn emit_color_temp_success(
    node_id: u64,
    endpoint: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
) {
    output::emit(body::color_temp_success(
        node_id, endpoint, kelvin, mireds, transition,
    ));
}

/// `level` の成功 JSON を stdout へ emit する（body は `mat_core::body` 共有）。
/// 出力には入力の percent と換算後の level を両方載せ、`current-level` の
/// 読み返しと突合しやすくする。
pub(crate) fn emit_level_success(
    node_id: u64,
    endpoint: u16,
    percent: u8,
    level: u8,
    transition: u16,
) {
    output::emit(body::level_success(
        node_id, endpoint, percent, level, transition,
    ));
}

/// `mat color-temp` の `--kelvin` / `--mireds`（排他・どちらか必須）を
/// `(mireds, kelvin)` に解決する。与えられなかった側は `round(1_000_000 / x)` で
/// 補完し、出力 JSON へのエコー（読み返し突合用）に使う。決定的な数値換算のみで、
/// デバイス対応範囲（color-temp-physical-min/max-mireds）の検証はしない
/// （範囲外はデバイス側が clamp する）。
pub fn resolve_color_temp(kelvin: Option<u32>, mireds: Option<u16>) -> (u16, u32) {
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

/// `color` の成功 JSON を stdout へ emit する（body は `mat_core::body` 共有）。
/// 入力（name / rgb / 度・%）と換算後の 0–254 生値を両方エコーし、`current-hue` /
/// `current-saturation` の読み返しと突合しやすくする。
pub(crate) fn emit_color_success(
    node_id: u64,
    endpoint: u16,
    color: &ResolvedColor,
    transition: u16,
) {
    output::emit(body::color_success(node_id, endpoint, color, transition));
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
