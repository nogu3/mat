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

/// `chip-tool <cluster> read <attribute> ...` の stdout から属性値を取り出す。
///
/// chip-tool は読んだ値を `Data = <値>,` 行で出す（CLAUDE.md の「比較的規則的な
/// `Data = ...` 形式」）。最後に現れた `Data =` を採用し、`mat` の JSON 値へ正規化
/// する。1件も無ければ `None`（呼び出し側が `parse_error` にする）。
pub fn parse_read_value(stdout: &str) -> Option<serde_json::Value> {
    let mut last: Option<&str> = None;
    for line in stdout.lines() {
        if let Some(pos) = line.find("Data =") {
            let raw = line[pos + "Data =".len()..].trim();
            // 行末のカンマを落とす（`Data = false,`）。
            let raw = raw.strip_suffix(',').unwrap_or(raw).trim();
            if !raw.is_empty() {
                last = Some(raw);
            }
        }
    }
    last.map(normalize_value)
}

/// chip-tool の生テキスト値を `mat` の JSON 値へ正規化する。
fn normalize_value(raw: &str) -> serde_json::Value {
    // 文字列リテラル（両端ダブルクォート）。
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        return serde_json::Value::String(raw[1..raw.len() - 1].to_string());
    }
    match raw.to_ascii_lowercase().as_str() {
        "true" => return serde_json::Value::Bool(true),
        "false" => return serde_json::Value::Bool(false),
        "null" => return serde_json::Value::Null,
        _ => {}
    }
    if let Ok(i) = raw.parse::<i64>() {
        return serde_json::Value::from(i);
    }
    if let Ok(u) = raw.parse::<u64>() {
        return serde_json::Value::from(u);
    }
    if let Ok(f) = raw.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return serde_json::Value::Number(n);
        }
    }
    serde_json::Value::String(raw.to_string())
}

/// `chip-tool` の write / invoke の stdout から「IM レベルで成功した」ことを判定する。
///
/// write は `status = 0x00 (SUCCESS)`、invoke は `... Status=0x0 (SUCCESS)` のような
/// 行を出す。どちらかの成功シグナルがあれば真。明示シグナルが無い出力は偽。
pub fn operation_succeeded(stdout: &str) -> bool {
    for line in stdout.lines() {
        let l = line.to_ascii_lowercase();
        // write の AttributeStatusIB（`status = 0x00 (SUCCESS)`）。
        if l.contains("status = 0x00") {
            return true;
        }
        // invoke の InvokeResponse（`Status=0x0 (SUCCESS)`）。
        if let Some(pos) = l.find("status=") {
            let after = l[pos + "status=".len()..].trim_start();
            if after == "0x0"
                || after.starts_with("0x0 ")
                || after.starts_with("0x00")
                || after.starts_with("0x0(")
            {
                return true;
            }
        }
    }
    false
}

/// `mat open-window` が返す発行コード（multi-admin 共有用）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenWindowCodes {
    /// 11桁の manual pairing code。
    pub manual_code: Option<String>,
    /// QR ペイロード文字列（`MT:...`）。画像化はしない（上層の責務）。
    pub qr_payload: Option<String>,
}

/// `chip-tool pairing open-commissioning-window ...` の stdout から発行コードを拾う。
///
/// chip-tool は ECM（option 1）で `Manual pairing code: [<code>]` と
/// `SetupQRCode: [MT:...]` を出す。どちらも角括弧内の値を取り出す。1つでも欠ければ
/// 呼び出し側が `parse_error` にする。
pub fn parse_open_window(stdout: &str) -> OpenWindowCodes {
    let mut codes = OpenWindowCodes::default();
    for line in stdout.lines() {
        if let Some(v) = bracketed_after(line, "Manual pairing code:") {
            codes.manual_code = Some(v.to_string());
        } else if let Some(v) = bracketed_after(line, "SetupQRCode:") {
            codes.qr_payload = Some(v.to_string());
        }
    }
    codes
}

/// `<label> ... [<値>]` 行からラベル以降 最初の `[...]` の中身を返す。
fn bracketed_after<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let pos = line.find(label)?;
    let after = &line[pos + label.len()..];
    let open = after.find('[')?;
    let close = after[open + 1..].find(']')?;
    let val = after[open + 1..open + 1 + close].trim();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// `chip-tool descriptor read <list> ...` の stdout から ID リストを取り出す。
///
/// chip-tool はリスト属性を `[<idx>]: <値>` 行で列挙する（PartsList / ServerList 等）。
/// 各エントリ行の値を数値として拾う。順序保持。
pub fn parse_id_list(stdout: &str) -> Vec<u64> {
    let mut ids = Vec::new();
    for line in stdout.lines() {
        let Some(entry) = strip_log_prefix(line) else {
            continue;
        };
        let entry = entry.trim_start();
        if !entry.starts_with('[') {
            continue;
        }
        let Some(close) = entry.find(']') else {
            continue;
        };
        // `[n]` の n が数値であることを確認（インデックス行のみ対象）。
        if entry[1..close].trim().parse::<u64>().is_err() {
            continue;
        }
        let rest = entry[close + 1..].trim_start();
        let Some(val) = rest.strip_prefix(':') else {
            continue;
        };
        if let Ok(id) = val.trim().parse::<u64>() {
            ids.push(id);
        }
    }
    ids
}

/// 行頭の chip-tool ログ接頭辞を取り除いた残り（payload）を返す。
///
/// chip-tool のログ形式はバージョンで揺れる。少なくとも次の2系統を扱う:
/// - 旧テスト fixture: `[1717][CHIP:DIS] payload`（整数 ts + `CHIP:` タグ、隙間なし）
/// - 実機 v1.4.2.0:   `[1780817887.948] [32231:32235] [TOO] payload`
///   （小数点 ts + `pid:tid` + `CHIP:` 無しタグ、スペース区切り）
///
/// 方針: 行頭から `[...]` ブロックを見て、
/// - 英字を含むブロック（`CHIP:DIS` / `TOO` / `DMG` 等のタグ）に当たったら、それを
///   接頭辞の最後とみなして以降の payload を返す。
/// - 数字・ドット・コロンのみのブロック（ts / `pid:tid`）で、直後に別ブロックが続く
///   ものはメタ情報として読み飛ばす。
/// - それ以外（`[1]: 6` のようなインデックス行）は剥がさない。
fn strip_log_prefix(line: &str) -> Option<&str> {
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start();
        if !trimmed.starts_with('[') {
            return Some(trimmed);
        }
        let Some(close) = trimmed.find(']') else {
            return Some(trimmed);
        };
        let block = &trimmed[1..close];
        let after = trimmed[close + 1..].trim_start();

        // タグブロック（英字を含む）= 接頭辞の最後。payload を返す。
        if block.chars().any(|c| c.is_ascii_alphabetic()) {
            return Some(after);
        }
        // メタブロック（ts `1780817887.948` / pid:tid `32231:32235`）は数字・ドット・
        // コロンのみで構成され、直後に別ブロックが続く。読み飛ばして継続。素の
        // `[1]: 6`（直後が `[` でない）はインデックス行なので剥がさない。
        let is_meta = !block.is_empty()
            && block
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.' || c == ':');
        if is_meta && after.starts_with('[') {
            rest = after;
            continue;
        }
        return Some(trimmed);
    }
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

    #[test]
    fn read_value_bool() {
        let s = "[1656][CHIP:DMG]                         Data = false,";
        assert_eq!(parse_read_value(s), Some(serde_json::Value::Bool(false)));
        let s = "[1656][CHIP:DMG]   Data = TRUE";
        assert_eq!(parse_read_value(s), Some(serde_json::Value::Bool(true)));
    }

    #[test]
    fn read_value_integer_and_string() {
        let s = "[1656][CHIP:DMG] Data = 254,";
        assert_eq!(parse_read_value(s), Some(serde_json::Value::from(254)));
        let s = "[1656][CHIP:DMG] Data = \"living room\",";
        assert_eq!(
            parse_read_value(s),
            Some(serde_json::Value::String("living room".into()))
        );
    }

    #[test]
    fn read_value_takes_last_data_line() {
        // ReportData の入れ子で Data が複数出ても最後（実値）を採用。
        let s = "Data = 0,\nData = 42,";
        assert_eq!(parse_read_value(s), Some(serde_json::Value::from(42)));
    }

    #[test]
    fn read_value_none_when_absent() {
        assert_eq!(parse_read_value("no data here"), None);
    }

    #[test]
    fn operation_success_write_and_invoke() {
        assert!(operation_succeeded(
            "[1656][CHIP:DMG]   status = 0x00 (SUCCESS),"
        ));
        assert!(operation_succeeded(
            "[1656][CHIP:DMG] Received Command Response Status for Endpoint=0x1 Cluster=0x0000_0006 Command=0x0000_0001 Status=0x0 (SUCCESS)"
        ));
        assert!(!operation_succeeded(
            "[1656][CHIP:DMG] status = 0x01 (FAILURE)"
        ));
        assert!(!operation_succeeded("nothing useful"));
    }

    #[test]
    fn open_window_extracts_both_codes() {
        let s = "\
[1656][CHIP:CTL] Manual pairing code: [36217551492]
[1656][CHIP:SVR] SetupQRCode: [MT:-24J0AFN00KA0648G00]
";
        let codes = parse_open_window(s);
        assert_eq!(codes.manual_code.as_deref(), Some("36217551492"));
        assert_eq!(codes.qr_payload.as_deref(), Some("MT:-24J0AFN00KA0648G00"));
    }

    #[test]
    fn open_window_missing_codes_are_none() {
        let codes = parse_open_window("nothing useful here");
        assert!(codes.manual_code.is_none());
        assert!(codes.qr_payload.is_none());
    }

    #[test]
    fn id_list_extracts_entries() {
        let s = "\
[1717][CHIP:TOO]   ServerList: 3 entries
[1717][CHIP:TOO]     [1]: 6
[1717][CHIP:TOO]     [2]: 29
[1717][CHIP:TOO]     [3]: 31
";
        assert_eq!(parse_id_list(s), vec![6, 29, 31]);
    }

    #[test]
    fn id_list_empty_when_no_entries() {
        assert!(parse_id_list("[1717][CHIP:TOO]   ServerList: 0 entries").is_empty());
    }

    #[test]
    fn id_list_realworld_log_format() {
        // 実機 chip-tool v1.4.2.0: 小数点 ts + `pid:tid` + `CHIP:` 無しタグ +
        // スペース区切り。旧パーサはこの接頭辞を剥がせず describe が空になっていた。
        let s = "\
[1780817887.948] [32231:32235] [TOO]   ServerList: 3 entries
[1780817887.948] [32231:32235] [TOO]     [1]: 6
[1780817887.948] [32231:32235] [TOO]     [2]: 29
[1780817887.948] [32231:32235] [TOO]     [3]: 31
";
        assert_eq!(parse_id_list(s), vec![6, 29, 31]);
    }

    #[test]
    fn id_list_realworld_parts_list_single() {
        // describe で実際に取りこぼした PartsList（endpoint 1 のみ）。
        let s = "\
[1780817887.948] [32231:32235] [TOO]   PartsList: 1 entries
[1780817887.948] [32231:32235] [TOO]     [1]: 1
";
        assert_eq!(parse_id_list(s), vec![1]);
    }

    #[test]
    fn read_value_realworld_log_format() {
        // 実機形式の Data 行（ANSI はランナーで除去済みの前提）。
        let s = "[1780817887.948] [32231:32235] [DMG]   Data = true,";
        assert_eq!(parse_read_value(s), Some(serde_json::Value::Bool(true)));
    }
}
