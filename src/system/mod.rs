/// OS-level system status and control.
///
/// All functions are designed to degrade gracefully — if `nmcli` or
/// `bluetoothctl` is not installed, or hardware isn't present, they return
/// sensible defaults rather than propagating errors.
use std::collections::HashSet;
use tokio::process::Command;

// ── Public status types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct SystemStatus {
    pub battery:   Option<BatteryStatus>,
    pub wifi:      WifiStatus,
    pub bluetooth: BtStatus,
}

#[derive(Debug, Clone)]
pub struct BatteryStatus {
    /// 0 – 100
    pub percent:  u8,
    pub charging: bool,
}

#[derive(Debug, Clone, Default)]
pub struct WifiStatus {
    pub enabled:     bool,
    pub ssid:        Option<String>,
    /// 0 – 3 signal bars
    pub signal_bars: u8,
}

#[derive(Debug, Clone, Default)]
pub struct BtStatus {
    pub enabled:          bool,
    pub connected_device: Option<String>,
}

// ── Public action/list types ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct WifiNetwork {
    pub ssid:        String,
    /// 0 – 3
    pub signal_bars: u8,
    pub secured:     bool,
    pub connected:   bool,
}

#[derive(Debug, Clone)]
pub struct BtDevice {
    pub name:      String,
    pub address:   String,
    pub connected: bool,
}

// ── Polling ───────────────────────────────────────────────────────────────────

/// Collect a full snapshot of battery, WiFi and Bluetooth state.
pub async fn poll_status() -> SystemStatus {
    let (wifi, bluetooth) = tokio::join!(read_wifi_status(), read_bt_status());
    SystemStatus {
        battery: read_battery(),
        wifi,
        bluetooth,
    }
}

// ── Battery (sysfs — no subprocess) ──────────────────────────────────────────

fn read_battery() -> Option<BatteryStatus> {
    // Standard Linux power-supply names — try common variants.
    for name in &["BAT0", "BAT1", "BAT2", "battery", "BATC", "BATT"] {
        let base = std::path::Path::new("/sys/class/power_supply").join(name);
        if !base.join("capacity").exists() {
            continue;
        }
        let percent: u8 = std::fs::read_to_string(base.join("capacity"))
            .ok()
            .and_then(|s| s.trim().parse().ok())?;
        let status = std::fs::read_to_string(base.join("status")).unwrap_or_default();
        let charging = !status.trim().eq_ignore_ascii_case("Discharging");
        return Some(BatteryStatus { percent, charging });
    }
    None
}

// ── WiFi (nmcli) ──────────────────────────────────────────────────────────────

async fn read_wifi_status() -> WifiStatus {
    // nmcli -t -f DEVICE,STATE,CONNECTION device
    // Line format: wlan0:connected:MySSID
    let Ok(out) = Command::new("nmcli")
        .args(["-t", "-f", "DEVICE,STATE,CONNECTION", "device"])
        .output()
        .await
    else {
        return WifiStatus::default();
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let mut parts = line.splitn(3, ':');
        let device = parts.next().unwrap_or("");
        let state  = parts.next().unwrap_or("");
        let conn   = parts.next().unwrap_or("");

        if !device.starts_with("wl") && !device.starts_with("wifi") {
            continue;
        }
        let enabled = state != "unavailable" && state != "unmanaged";
        if state == "connected" {
            let signal_bars = read_wifi_signal().await;
            return WifiStatus {
                enabled: true,
                ssid: Some(conn.to_string()),
                signal_bars,
            };
        }
        return WifiStatus { enabled, ssid: None, signal_bars: 0 };
    }
    WifiStatus::default()
}

async fn read_wifi_signal() -> u8 {
    // /proc/net/wireless: no subprocess, available when driver is loaded.
    if let Ok(raw) = tokio::fs::read_to_string("/proc/net/wireless").await {
        for line in raw.lines().skip(2) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 3 {
                if let Ok(q) = cols[2].trim_end_matches('.').parse::<u32>() {
                    // Link quality is typically 0-70 on most drivers.
                    return ((q * 3 + 34) / 70).min(3) as u8;
                }
            }
        }
    }
    1
}

/// Trigger a rescan and return a de-duplicated, sorted list of visible networks.
pub async fn scan_wifi() -> Vec<WifiNetwork> {
    // Ask NM to rescan (non-blocking request — NM caches results for ~30s).
    let _ = Command::new("nmcli")
        .args(["dev", "wifi", "rescan"])
        .output()
        .await;

    // Fields: SSID:SIGNAL:SECURITY:ACTIVE  (--escape no avoids \: in SSIDs)
    let Ok(out) = Command::new("nmcli")
        .args(["-t", "--escape", "no", "-f", "SSID,SIGNAL,SECURITY,ACTIVE", "dev", "wifi"])
        .output()
        .await
    else {
        return vec![];
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut seen: HashSet<String> = HashSet::new();
    let mut networks: Vec<WifiNetwork> = stdout
        .lines()
        .filter_map(parse_wifi_line)
        .filter(|n| seen.insert(n.ssid.clone()))
        .collect();

    // Connected network first, then by descending signal strength.
    networks.sort_by(|a, b| b.connected.cmp(&a.connected).then(b.signal_bars.cmp(&a.signal_bars)));
    networks
}

fn parse_wifi_line(line: &str) -> Option<WifiNetwork> {
    // rsplitn from the right so that SSIDs containing ':' are preserved.
    // rsplitn(4, ':') => [ACTIVE, SECURITY, SIGNAL, rest_as_SSID] (reversed order)
    let parts: Vec<&str> = line.rsplitn(4, ':').collect();
    if parts.len() < 4 {
        return None;
    }
    let active   = parts[0] == "yes";
    let security = parts[1];
    let signal: u32 = parts[2].parse().ok()?;
    let ssid = parts[3].trim();
    if ssid.is_empty() {
        return None; // hidden network
    }
    Some(WifiNetwork {
        ssid:        ssid.to_string(),
        signal_bars: ((signal * 3 + 49) / 100).min(3) as u8,
        secured:     !security.is_empty() && security != "--",
        connected:   active,
    })
}

// ── Bluetooth (bluetoothctl) ──────────────────────────────────────────────────

async fn read_bt_status() -> BtStatus {
    let Ok(out) = Command::new("bluetoothctl").args(["show"]).output().await else {
        return BtStatus::default();
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let powered = stdout
        .lines()
        .any(|l| l.trim().starts_with("Powered:") && l.contains("yes"));

    let connected_device = if powered { read_bt_connected().await } else { None };
    BtStatus { enabled: powered, connected_device }
}

async fn read_bt_connected() -> Option<String> {
    let out = Command::new("bluetoothctl")
        .args(["devices", "Connected"])
        .output()
        .await
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .filter(|l| l.starts_with("Device "))
        .find_map(|l| l.splitn(3, ' ').nth(2).map(|s| s.to_string()))
}

/// Return all paired Bluetooth devices with their current connection state.
pub async fn list_bt_devices() -> Vec<BtDevice> {
    let Ok(paired_out) = Command::new("bluetoothctl")
        .args(["devices", "Paired"])
        .output()
        .await
    else {
        return vec![];
    };

    // Build a set of connected addresses.
    let connected_addrs: HashSet<String> = Command::new("bluetoothctl")
        .args(["devices", "Connected"])
        .output()
        .await
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(parse_bt_device_line)
                .map(|d| d.address)
                .collect()
        })
        .unwrap_or_default();

    String::from_utf8_lossy(&paired_out.stdout)
        .lines()
        .filter_map(parse_bt_device_line)
        .map(|mut d| {
            d.connected = connected_addrs.contains(&d.address);
            d
        })
        .collect()
}

fn parse_bt_device_line(line: &str) -> Option<BtDevice> {
    // Format: "Device XX:XX:XX:XX:XX:XX Device Name"
    let mut parts = line.splitn(3, ' ');
    let keyword = parts.next()?;
    let address = parts.next()?;
    let name    = parts.next()?;
    if keyword != "Device" {
        return None;
    }
    Some(BtDevice {
        name:      name.to_string(),
        address:   address.to_string(),
        connected: false, // filled in by caller
    })
}

/// Scan for new Bluetooth devices for ~5 seconds, then return the refreshed
/// paired-device list. Newly discovered devices won't appear here until paired.
pub async fn bt_scan() -> Vec<BtDevice> {
    // `--timeout 5` available in BlueZ 5.48+ (standard on Pi OS).
    let _ = Command::new("bluetoothctl")
        .args(["--timeout", "5", "scan", "on"])
        .output()
        .await;
    list_bt_devices().await
}

// ── WiFi actions ──────────────────────────────────────────────────────────────

pub async fn wifi_toggle(enable: bool) -> anyhow::Result<()> {
    let arg = if enable { "on" } else { "off" };
    Command::new("nmcli").args(["radio", "wifi", arg]).status().await?;
    Ok(())
}

/// Connect to a known (or open) network by SSID.
/// First tries to bring up a saved connection; falls back to connecting by SSID
/// (works for open networks, fails for new secured networks without credentials).
pub async fn wifi_connect(ssid: &str) -> anyhow::Result<()> {
    let status = Command::new("nmcli")
        .args(["connection", "up", ssid])
        .status()
        .await?;
    if !status.success() {
        Command::new("nmcli")
            .args(["device", "wifi", "connect", ssid])
            .status()
            .await?;
    }
    Ok(())
}

pub async fn wifi_disconnect() -> anyhow::Result<()> {
    Command::new("nmcli")
        .args(["device", "disconnect", "wlan0"])
        .status()
        .await?;
    Ok(())
}

// ── Bluetooth actions ─────────────────────────────────────────────────────────

pub async fn bt_toggle(enable: bool) -> anyhow::Result<()> {
    let arg = if enable { "power on" } else { "power off" };
    Command::new("sh")
        .args(["-c", &format!("echo '{arg}' | bluetoothctl")])
        .status()
        .await?;
    Ok(())
}

pub async fn bt_connect(address: &str) -> anyhow::Result<()> {
    Command::new("sh")
        .args(["-c", &format!("echo 'connect {address}' | bluetoothctl")])
        .status()
        .await?;
    Ok(())
}

pub async fn bt_disconnect(address: &str) -> anyhow::Result<()> {
    Command::new("sh")
        .args(["-c", &format!("echo 'disconnect {address}' | bluetoothctl")])
        .status()
        .await?;
    Ok(())
}
