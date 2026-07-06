//! `mat invoke` — コマンドを実行する。
//!
//! `chip-tool <cluster> <command> [args...] <node_id> <endpoint>` をラップ。
//! chip-tool は宛先 node_id / endpoint を**末尾**に取る。コマンド引数はその前。
//! `mat on` / `mat off` は OnOff クラスタの On/Off コマンドを **invoke** に
//! マップしたショートカット（属性 write ではない）で、ここを再利用する。

use std::path::Path;

use serde_json::json;

use crate::runner::ChipTool;
use mat_core::error::{ErrorKind, MatError};
use mat_core::normalize::classify_failure;
use mat_core::output;
use mat_core::parse::operation_succeeded;
use mat_core::store::Store;

pub fn run(
    store_path: &Path,
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    command: &str,
    args: &[String],
) -> Result<(), MatError> {
    execute(store_path, node_id, endpoint, cluster, command, args)?;
    output::emit(json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": cluster,
        "command": command,
        "status": "success",
    }));
    Ok(())
}

/// invoke の実行部（出力なし）。成功判定までを行い、emit は呼び出し側の責務。
fn execute(
    store_path: &Path,
    node_id: u64,
    endpoint: u16,
    cluster: &str,
    command: &str,
    args: &[String],
) -> Result<(), MatError> {
    let store = Store::open(store_path)?;
    store.require_node(node_id)?;
    let chip = ChipTool::new(store.root());

    // chip-tool は `<cluster> <command> [command-args...] <node_id> <endpoint>` の順で
    // 宛先を末尾に取る。コマンド引数を node_id/endpoint の前に置かないと、引数が宛先
    // として誤読され（node_id=0 等）応答が来ず timeout する。
    let mut argv = vec![cluster.to_string(), command.to_string()];
    argv.extend(args.iter().cloned());
    argv.push(node_id.to_string());
    argv.push(endpoint.to_string());

    let out = chip.run(argv)?;

    if let Some(kind) = classify_failure(&out.stdout, &out.stderr) {
        return Err(MatError::new(
            kind,
            format!("invoke {cluster}/{command} on node {node_id} endpoint {endpoint} failed"),
        ));
    }
    if !out.success() || !operation_succeeded(&out.stdout) {
        return Err(MatError::new(
            ErrorKind::ChildFailed,
            format!("invoke {cluster}/{command} on node {node_id} did not report success"),
        ));
    }
    Ok(())
}

/// `mat on` / `mat off` の実体。OnOff クラスタの On/Off コマンドを invoke する。
pub fn run_onoff(store_path: &Path, node_id: u64, endpoint: u16, on: bool) -> Result<(), MatError> {
    let command = if on { "on" } else { "off" };
    run(store_path, node_id, endpoint, "onoff", command, &[])
}

/// `mat color-temp` の実体。ColorControl の MoveToColorTemperature を invoke する。
/// 出力には入力の kelvin と換算後の mireds を両方載せ、`color-temperature-mireds`
/// の読み返しと突合しやすくする。
pub fn run_color_temp(
    store_path: &Path,
    node_id: u64,
    endpoint: u16,
    kelvin: u32,
    mireds: u16,
    transition: u16,
) -> Result<(), MatError> {
    // MoveToColorTemperature の引数は <mireds> <transition> <optionsMask> <optionsOverride>。
    let args = [
        mireds.to_string(),
        transition.to_string(),
        "0".to_string(),
        "0".to_string(),
    ];
    execute(
        store_path,
        node_id,
        endpoint,
        "colorcontrol",
        "move-to-color-temperature",
        &args,
    )?;
    output::emit(json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "colorcontrol",
        "command": "move-to-color-temperature",
        "kelvin": kelvin,
        "mireds": mireds,
        "transition": transition,
        "status": "success",
    }));
    Ok(())
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
}
