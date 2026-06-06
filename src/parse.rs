//! `chip-tool` のログ志向テキスト出力を `mat` のスキーマへ正規化するパーサ。
//!
//! `chip-tool` の出力はバージョン差でぶれるため、ここに薄く閉じ込めてユニット
//! テストで固める。バージョン更新でテストが落ちれば気づける。

use serde::Serialize;

/// `mat discover` が返す1デバイス分。
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct DiscoveredDevice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discriminator: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_id: Option<u32>,
}

impl DiscoveredDevice {
    fn is_empty(&self) -> bool {
        self.hostname.is_none()
            && self.addresses.is_empty()
            && self.port.is_none()
            && self.discriminator.is_none()
            && self.vendor_id.is_none()
            && self.product_id.is_none()
    }
}

/// chip-tool のログ1行から `ラベル: 値` を取り出す。行頭のタイムスタンプ／タグ
/// （`[...][CHIP:DIS]`）やインデントは無視する。
fn field<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let pos = line.find(label)?;
    let after = &line[pos + label.len()..];
    // ラベルと値の間に "#1" 等が挟まる行（"IP Address #1: ..."）があるため、
    // ラベル以降の最初の ':' を区切りとする。
    let colon = after.find(':')?;
    let val = after[colon + 1..].trim();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// `chip-tool discover commissionables` の stdout をパースする。
///
/// 「Discovered ... node」行を区切りに各デバイスを切り出す。1件もマーカーが無い
/// 場合は空 Vec（探索ヒット 0 件として正常）。
pub fn parse_commissionables(stdout: &str) -> Vec<DiscoveredDevice> {
    let mut devices = Vec::new();
    let mut cur: Option<DiscoveredDevice> = None;

    for line in stdout.lines() {
        if line.contains("Discovered") && line.contains("node") {
            if let Some(d) = cur.take() {
                if !d.is_empty() {
                    devices.push(d);
                }
            }
            cur = Some(DiscoveredDevice::default());
            continue;
        }
        let Some(dev) = cur.as_mut() else { continue };

        if let Some(v) = field(line, "Hostname") {
            dev.hostname = Some(v.to_string());
        } else if let Some(v) = field(line, "IP Address") {
            // "IP Address #1" のように番号付き。値だけ拾う。
            dev.addresses.push(v.to_string());
        } else if let Some(v) = field(line, "Port") {
            dev.port = v.parse().ok();
        } else if let Some(v) = field(line, "Long Discriminator") {
            dev.discriminator = v.parse().ok();
        } else if let Some(v) = field(line, "Vendor ID") {
            dev.vendor_id = v.parse().ok();
        } else if let Some(v) = field(line, "Product ID") {
            dev.product_id = v.parse().ok();
        }
    }
    if let Some(d) = cur.take() {
        if !d.is_empty() {
            devices.push(d);
        }
    }
    devices
}

/// `chip-tool pairing ...` の stdout から commissioning 成功を判定する。
pub fn commission_succeeded(stdout: &str) -> bool {
    let hay = stdout.to_ascii_lowercase();
    hay.contains("commissioning completed with success")
        || hay.contains("successfully finished commissioning")
        || hay.contains("device commissioning completed with success")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISCOVER_SAMPLE: &str = "\
[1717][CHIP:DIS] Discovered commissionable/commissioner node:
[1717][CHIP:DIS] \tHostname: B827EBA8C9F0
[1717][CHIP:DIS] \tIP Address #1: 192.0.2.10
[1717][CHIP:DIS] \tIP Address #2: fe80::1
[1717][CHIP:DIS] \tPort: 5540
[1717][CHIP:DIS] \tLong Discriminator: 3840
[1717][CHIP:DIS] \tVendor ID: 65521
[1717][CHIP:DIS] \tProduct ID: 32769
[1717][CHIP:DIS] Discovered commissionable/commissioner node:
[1717][CHIP:DIS] \tHostname: AABBCCDDEEFF
[1717][CHIP:DIS] \tIP Address #1: 192.0.2.20
[1717][CHIP:DIS] \tPort: 5541
[1717][CHIP:DIS] \tLong Discriminator: 100
";

    #[test]
    fn parses_two_devices() {
        let devs = parse_commissionables(DISCOVER_SAMPLE);
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].hostname.as_deref(), Some("B827EBA8C9F0"));
        assert_eq!(devs[0].addresses, vec!["192.0.2.10", "fe80::1"]);
        assert_eq!(devs[0].port, Some(5540));
        assert_eq!(devs[0].discriminator, Some(3840));
        assert_eq!(devs[0].vendor_id, Some(65521));
        assert_eq!(devs[0].product_id, Some(32769));
        assert_eq!(devs[1].hostname.as_deref(), Some("AABBCCDDEEFF"));
        assert_eq!(devs[1].port, Some(5541));
    }

    #[test]
    fn empty_output_is_zero_devices() {
        assert!(parse_commissionables("").is_empty());
        assert!(parse_commissionables("no markers here\njust noise").is_empty());
    }

    #[test]
    fn detects_commission_success() {
        assert!(commission_succeeded(
            "[CTL] Successfully finished commissioning, deviceId=5"
        ));
        assert!(commission_succeeded(
            "[TOO] Device commissioning completed with success"
        ));
        assert!(!commission_succeeded("[TOO] Run command failure"));
    }
}
