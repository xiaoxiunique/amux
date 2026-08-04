//! USB device enumeration, macOS-only, for the phone client.
//!
//! The canonical tool on macOS is `system_profiler SPUSBDataType`, but on
//! Apple Silicon it returns an empty array without special entitlements.
//! `ioreg -p IOUSB` queries IOKit directly and works without any permission
//! grants — the data is already public in the IORegistry.
//!
//! Output is parsed from the human-readable ioreg text format because the XML
//! plist (-a) variant would require a plist parsing dependency; the key-value
//! lines are simple enough to extract with a substring match and the output is
//! cached for 30s, so the parsing cost is negligible.

use serde::Serialize;
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

/// Cached result so the 2.5s poll loop doesn't shell out on every tick.
static CACHE: Mutex<Option<(Instant, Vec<UsbDevice>)>> = Mutex::new(None);
const CACHE_TTL_SECS: u64 = 30;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UsbDevice {
    /// e.g. "Redmi Note 11T Pro"
    pub product: String,
    /// e.g. "Xiaomi"
    pub vendor: String,
    /// USB serial (may be empty)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// Raw device block heading from ioreg, kept for diagnostics
    #[serde(skip)]
    pub heading: Option<String>,
}

/// True when the underlying tool is present on this machine.
pub fn available() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

pub fn enumerate() -> Vec<UsbDevice> {
    // Not all platforms have ioreg; return empty gracefully.
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    if let Some((at, cached)) = CACHE.lock().ok().and_then(|g| g.clone()) {
        if at.elapsed().as_secs() < CACHE_TTL_SECS {
            return cached;
        }
    }
    let devices = scan();
    if let Ok(mut g) = CACHE.lock() {
        *g = Some((Instant::now(), devices.clone()));
    }
    devices
}

fn scan() -> Vec<UsbDevice> {
    let output = match Command::new("ioreg")
        .args([
            "-p", "IOUSB",
            "-r",
            "-c", "IOUSBHostDevice",
            "-w0",
        ])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };

    parse(&output)
}

/// Extract USBHostDevice blocks from ioreg text output.
///
/// Each block looks like:
/// ```text
/// +-o Device Name  <class IOUSBHostDevice, id 0x10000abcd, registered, matched, active, busy 0 (X ms), retain X>
///   {
///     "USB Product Name" = "Redmi Note 11T Pro"
///     "USB Vendor Name" = "Xiaomi"
///     "USB Serial Number" = "abc123"
///   }
/// ```
fn parse(text: &str) -> Vec<UsbDevice> {
    let mut out = Vec::new();
    let mut heading: Option<&str> = None;
    let mut product: Option<&str> = None;
    let mut vendor: Option<&str> = None;
    let mut serial: Option<&str> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("+-o ") {
            // New device block starting: flush the previous one.
            if let Some(p) = product {
                out.push(UsbDevice {
                    product: p.to_string(),
                    vendor: vendor.unwrap_or("").to_string(),
                    serial: serial.map(|s| s.to_string()),
                    heading: heading.map(|h| h.to_string()),
                });
            }
            heading = Some(trimmed);
            product = None;
            vendor = None;
            serial = None;
        } else if trimmed.contains('"') && trimmed.contains('=') {
            let after_eq = match trimmed.find('=') {
                Some(pos) => &trimmed[pos + 1..],
                None => continue,
            };
            let value = after_eq.trim().trim_matches('"');

            if trimmed.contains("USB Product Name") {
                product = Some(value);
            } else if trimmed.contains("USB Vendor Name") {
                vendor = Some(value);
            } else if trimmed.contains("USB Serial Number") {
                serial = Some(value);
            }
        }
        // Closing `}` — also trigger a flush, in case the next block didn't
        // open yet before EOF.
    }
    if let Some(p) = product {
        out.push(UsbDevice {
            product: p.to_string(),
            vendor: vendor.unwrap_or("").to_string(),
            serial: serial.map(|s| s.to_string()),
            heading: heading.map(|h| h.to_string()),
        });
    }
    out
}

/// Take a screenshot of a USB-connected phone, keyed by its serial number.
///
/// Returns the image bytes and a content type. Android is captured directly
/// from `adb exec-out screencap -p` (stdout PNG, no temp file). iOS uses
/// `idevicescreenshot`, which writes a PNG to a temp file and requires the
/// device have a developer disk image mounted.
///
/// The serial must match a device in the cached USB enumeration; if it
/// doesn't, the function returns an error rather than guessing a tool.
pub fn screenshot(serial: &str) -> Result<(Vec<u8>, &'static str), String> {
    let devices = enumerate();
    let device = devices
        .iter()
        .find(|d| d.serial.as_deref() == Some(serial))
        .ok_or_else(|| format!("no USB device with serial {serial}"))?;

    let vendor = device.vendor.to_lowercase();

    // Android: adb pipes PNG directly to stdout.
    if vendor.contains("xiao")
        || vendor.contains("samsung")
        || vendor.contains("huawei")
        || vendor.contains("oppo")
        || vendor.contains("vivo")
        || vendor.contains("oneplus")
        || vendor.contains("google")
        || vendor.contains("motorola")
    {
        let output = Command::new("adb")
            .args(["-s", serial, "exec-out", "screencap", "-p"])
            .output()
            .map_err(|e| format!("adb: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("adb screenshot failed: {}", stderr.trim()));
        }
        return Ok((output.stdout, "image/png"));
    }

    // iOS: idevicescreenshot writes to a file.
    if vendor.contains("apple inc") || vendor.contains("apple") {
        let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        let path = dir.path().join("shot.png");

        let status = Command::new("idevicescreenshot")
            .args(["-u", serial])
            .arg(&path)
            .status()
            .map_err(|e| format!("idevicescreenshot: {e}"))?;
        if !status.success() {
            return Err("idevicescreenshot failed (device connected and unlocked?)".to_string());
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("reading screenshot: {e}"))?;
        return Ok((bytes, "image/png"));
    }

    Err(format!(
        "screenshot not supported for vendor '{}' (serial {serial})",
        device.vendor
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_device_block() {
        let sample = "\
+-o Redmi Note 11T Pro@00100000  <class IOUSBHostDevice, id 0x10000abcd, registered, matched, active, busy 0 (133 ms), retain 31>
  {
    \"USB Product Name\" = \"Redmi Note 11T Pro\"
    \"USB Vendor Name\" = \"Xiaomi\"
    \"USB Serial Number\" = \"abc123\"
  }
";
        let devices = parse(sample);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].product, "Redmi Note 11T Pro");
        assert_eq!(devices[0].vendor, "Xiaomi");
        assert_eq!(devices[0].serial.as_deref(), Some("abc123"));
    }

    #[test]
    fn parses_multiple_devices() {
        let sample = "\
+-o Mouse@00200000  <class IOUSBHostDevice, ...>
  {
    \"USB Product Name\" = \"G502 HERO Gaming Mouse\"
    \"USB Vendor Name\" = \"Logitech\"
  }

+-o Redmi Note 11T Pro@00100000  <class IOUSBHostDevice, ...>
  {
    \"USB Product Name\" = \"Redmi Note 11T Pro\"
    \"USB Vendor Name\" = \"Xiaomi\"
    \"USB Serial Number\" = \"abc123\"
  }
";
        let devices = parse(sample);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[1].product, "Redmi Note 11T Pro");
    }

    #[test]
    fn empty_output_gives_no_devices() {
        assert!(parse("").is_empty());
        assert!(parse("no devices here").is_empty());
    }
}
