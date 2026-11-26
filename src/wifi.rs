use crate::*;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

#[derive(Debug, Clone)]
pub struct IwDevEntry {
    pub phy: String,
    pub name: String,
    pub kind: String,
    pub ssid: Option<String>,
    pub freq: Option<u32>,
}

// iw dev is indent-based. walk the lines, keep whatever block is open.
pub fn parse_iw_dev(text: &str) -> Vec<IwDevEntry> {
    let mut entries = Vec::new();
    let mut phy = String::new();
    let mut current: Option<IwDevEntry> = None;

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("phy#") {
            phy = format!("phy{rest}");
            continue;
        }
        if let Some(name) = line.strip_prefix("Interface ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(IwDevEntry {
                phy: phy.clone(),
                name: name.to_string(),
                kind: String::new(),
                ssid: None,
                freq: None,
            });
            continue;
        }
        if line.starts_with("Unnamed/non-netdev interface") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(IwDevEntry {
                phy: phy.clone(),
                name: "<p2p>".into(),
                kind: String::new(),
                ssid: None,
                freq: None,
            });
            continue;
        }
        if let Some(entry) = current.as_mut() {
            if let Some(value) = line.strip_prefix("type ") {
                entry.kind = value.to_string();
            } else if let Some(value) = line.strip_prefix("ssid ") {
                entry.ssid = Some(value.to_string());
            } else if line.starts_with("channel ") {
                entry.freq = line
                    .split('(')
                    .nth(1)
                    .and_then(|part| part.split_whitespace().next())
                    .and_then(|value| value.parse::<u32>().ok());
            }
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

// glue each "* #{ ... }" block and its "total ... #channels <=" continuation
// into one line. managed and AP have to be in the same block, that's the trick.
pub fn parse_combinations(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_section = false;
    let mut current = String::new();

    for line in text.lines() {
        if line.contains("valid interface combinations:") {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        let line = line.trim();
        if line.starts_with('*') {
            if !current.is_empty() {
                blocks.push(current.clone());
            }
            current = line.to_string();
        } else if line.starts_with('#') || line.starts_with("total") {
            current.push(' ');
            current.push_str(line);
        } else if line.is_empty() {
            continue;
        } else {
            break;
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

// dig the number out of "#channels <= 1"
pub fn max_channels(block: &str) -> Option<u32> {
    let marker = "#channels <=";
    let index = block.find(marker)?;
    block[index + marker.len()..]
        .trim()
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

// "Supported interface modes" in `iw phy X info`
pub fn supported_modes(text: &str) -> Vec<String> {
    let mut modes = Vec::new();
    let mut in_section = false;
    for line in text.lines() {
        if line.contains("Supported interface modes:") {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        let trimmed = line.trim();
        if let Some(mode) = trimmed.strip_prefix('*') {
            modes.push(mode.trim().to_string());
        } else if !trimmed.is_empty() {
            break;
        }
    }
    modes
}

// per-frequency flags, e.g. "* 5220 MHz [44] (20.0 dBm) (no IR, radar detection)"
pub fn freq_flags(text: &str, freq: Option<u32>) -> (bool, bool) {
    let Some(freq) = freq else {
        return (false, false);
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with(&format!("* {freq} MHz"))
            && !trimmed.starts_with(&format!("* {freq}.0 MHz"))
        {
            continue;
        }
        return (
            trimmed.contains("no IR") || trimmed.contains("passive scan"),
            trimmed.contains("radar detection"),
        );
    }
    (false, false)
}

pub fn regulatory_country() -> Option<String> {
    let reg = stdout("iw", &["reg", "get"]).ok()?;
    for line in reg.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("country ") {
            let code = rest.split(':').next()?.split_whitespace().next()?;
            if code != "00" && code != "99" {
                return Some(code.to_string());
            }
        }
    }
    None
}

pub fn detect() -> Result<Capability> {
    let devices = parse_iw_dev(&stdout("iw", &["dev"])?);
    let station = devices
        .iter()
        .find(|entry| entry.kind == "managed" && entry.freq.is_some())
        .or_else(|| devices.iter().find(|entry| entry.kind == "managed"))
        .ok_or_else(|| anyhow!("no managed Wi-Fi interface found; connect to a network first"))?;

    let info = stdout("iw", &["phy", &station.phy, "info"])?;
    let combinations = parse_combinations(&info);
    let concurrent = combinations
        .iter()
        .any(|block| block.contains("managed") && block.contains("AP"));
    let same_channel_required = combinations
        .iter()
        .filter(|block| block.contains("managed") && block.contains("AP"))
        .all(|block| max_channels(block) == Some(1));
    let ap_mode = supported_modes(&info).iter().any(|mode| mode == "AP");
    let (no_ir, radar) = freq_flags(&info, station.freq);

    let second_adapter = devices
        .iter()
        .filter(|entry| entry.kind == "managed" && entry.phy != station.phy)
        .find(|entry| {
            stdout("iw", &["phy", &entry.phy, "info"])
                .map(|text| supported_modes(&text).iter().any(|mode| mode == "AP"))
                .unwrap_or(false)
        })
        .map(|entry| entry.name.clone());

    let mut missing_tools: Vec<String> = ["iw", "ip", "hostapd", "dnsmasq"]
        .iter()
        .filter(|tool| !tool_available(tool))
        .map(|tool| (*tool).to_string())
        .collect();
    if !tool_available("nft") && !tool_available("iptables") {
        missing_tools.push("nftables or iptables".into());
    }

    let ssid = station
        .ssid
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    Ok(Capability {
        phy: station.phy.clone(),
        uplink: station.name.clone(),
        uplink_ssid: ssid,
        frequency_mhz: station.freq,
        channel: station.freq.and_then(frequency_to_channel),
        band: station.freq.and_then(band_for_frequency),
        concurrent,
        same_channel_required,
        ap_mode,
        no_ir,
        radar,
        second_adapter,
        country: regulatory_country(),
        p2p_device: devices.iter().any(|entry| entry.kind == "P2P-device"),
        combinations,
        missing_tools,
    })
}
pub fn doctor() -> DoctorReport {
    let mut report = DoctorReport {
        mode: String::new(),
        ready: false,
        verdict: String::new(),
        checks: Vec::new(),
    };
    let mut missing = Vec::new();
    for tool in ["iw", "ip", "hostapd", "dnsmasq"] {
        if tool_available(tool) {
            report.add("ok", format!("{tool} installed"), "");
        } else {
            report.add("fail", format!("{tool} is missing"), "");
            missing.push(tool);
        }
    }
    if tool_available("nft") {
        report.add(
            "ok",
            "nftables installed",
            "used for private firewall tables",
        );
    } else if tool_available("iptables") {
        report.add(
            "warn",
            "nftables missing, using iptables",
            "tagged rules only",
        );
    } else {
        report.add("fail", "no firewall tool", "install nftables or iptables");
        missing.push("nftables or iptables");
    }

    match detect() {
        Ok(cap) => {
            report.add(
                "info",
                format!(
                    "adapter {} ({}, {})",
                    cap.uplink,
                    cap.phy,
                    if cap.concurrent {
                        "concurrent"
                    } else {
                        "single role"
                    }
                ),
                cap.uplink_ssid.clone().unwrap_or_default(),
            );
            if cap.no_ir || cap.radar {
                report.add(
                    "warn",
                    format!("channel {} is not usable for AP", cap.channel.unwrap_or(0)),
                    if cap.radar {
                        "DFS channel; use 2.4 GHz or 36-48/149-165"
                    } else {
                        "no IR; connect on 2.4 GHz or use a different country"
                    },
                );
            }
            if cap.country.is_none() {
                report.add(
                    "warn",
                    "no regulatory country set",
                    "5 GHz AP may fail; set your country with iw reg set",
                );
            }
            if cap.ap_mode && cap.concurrent {
                report.mode = "concurrent".into();
                report.add(
                    "ok",
                    "client and hotspot at once",
                    if cap.same_channel_required {
                        "AP will follow the station channel"
                    } else {
                        "different channels allowed"
                    },
                );
            } else if let Some(second) = &cap.second_adapter {
                report.mode = "dedicated".into();
                report.add(
                    "ok",
                    format!("second adapter {second} can host the hotspot"),
                    "keep the uplink on the other interface",
                );
            } else if sources().iter().any(|source| source != &cap.uplink) {
                report.mode = "dedicated".into();
                report.add(
                    "ok",
                    "a non-Wi-Fi uplink can feed a Wi-Fi-only hotspot",
                    "use --uplink with the Wi-Fi chip as AP",
                );
            } else {
                report.verdict = "NOT READY: this chip cannot client and hotspot at once, and there is no second adapter or cable".into();
                return report;
            }
            if !missing.is_empty() {
                report.verdict = format!(
                    "ALMOST: {} mode once {} is installed",
                    report.mode,
                    missing.join(", ")
                );
            } else {
                report.ready = true;
                report.verdict = format!("READY: {} mode on {}", report.mode, cap.uplink);
            }
        }
        Err(error) => report.verdict = format!("NOT READY: {error}"),
    }
    report
}

/// Interfaces that currently have an IPv4 default route, offered as internet sources.
pub fn sources() -> Vec<String> {
    let routes = stdout("ip", &["-4", "route", "show", "default"]).unwrap_or_default();
    let mut found = Vec::new();
    for line in routes.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if let Some(index) = words.iter().position(|word| *word == "dev") {
            if let Some(name) = words.get(index + 1) {
                if !found.contains(&(*name).to_string()) {
                    found.push((*name).to_string());
                }
            }
        }
    }
    found
}

/// Reads dnsmasq leases and the hostapd station table. Read only.
pub fn clients() -> Vec<ClientInfo> {
    use std::collections::BTreeMap;

    let state: Option<RuntimeState> = fs::read_to_string(STATE_FILE)
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok());
    let mut found: BTreeMap<String, ClientInfo> = BTreeMap::new();

    if let Ok(leases) = fs::read_to_string(DHCP_LEASES) {
        for line in leases.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 3 {
                continue;
            }
            let hostname = if fields[3.min(fields.len() - 1)] == "*" {
                None
            } else {
                Some(fields[3.min(fields.len() - 1)].to_string())
            };
            found.insert(
                fields[1].to_string(),
                ClientInfo {
                    mac: fields[1].to_string(),
                    ip: Some(fields[2].to_string()),
                    hostname,
                    connected: false,
                },
            );
        }
    }

    if let Some(state) = state {
        if let Ok(table) = stdout("iw", &["dev", &state.ap_interface, "station", "dump"]) {
            for line in table.lines() {
                let line = line.trim();
                if let Some(mac) = line.strip_prefix("Station ") {
                    let mac = mac.split_whitespace().next().unwrap_or("").to_string();
                    found
                        .entry(mac.clone())
                        .or_insert(ClientInfo {
                            mac: mac.clone(),
                            ip: None,
                            hostname: None,
                            connected: false,
                        })
                        .connected = true;
                }
            }
        }
    }

    found.into_values().collect()
}

// hostapd.conf holds the WPA passphrase, so it must not be world-readable
