use crate::*;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

pub const RUN_DIR: &str = "/run/airfork";
pub const STATE_FILE: &str = "/run/airfork/state.json";
pub const HOSTAPD_CONF: &str = "/run/airfork/hostapd.conf";
pub const DNSMASQ_CONF: &str = "/run/airfork/dnsmasq.conf";
pub const HOSTAPD_PID: &str = "/run/airfork/hostapd.pid";
pub const DNSMASQ_PID: &str = "/run/airfork/dnsmasq.pid";
pub const HOSTAPD_LOG: &str = "/run/airfork/hostapd.log";
pub const NM_CONF: &str = "/etc/NetworkManager/conf.d/99-airfork.conf";
pub const DHCP_LEASES: &str = "/run/airfork/dnsmasq.leases";
pub const CLIENT_SUBNET: &str = "192.168.42.0/24";
pub const ROUTE_TABLE: &str = "42";
pub const WATCH_LOG: &str = "/run/airfork/watch.log";

// locally administered, so it never collides with the station MAC
pub const AP_MAC: &str = "02:00:00:00:01:01";
pub const GATEWAY: &str = "192.168.42.1";
pub const DHCP_RANGE: &str = "192.168.42.10,192.168.42.100,255.255.255.0,24h";
pub const NAT_COMMENT: &str = "airfork";

// survives a reboot, unlike /run. holds what start() changed in the saved
// NetworkManager profile, so a crash or power cut cannot leave the user's
// Wi-Fi locked to 2.4 GHz with nothing left to undo it.
pub const RESTORE_DIR: &str = "/var/lib/airfork";
pub const RESTORE_FILE: &str = "/var/lib/airfork/restore.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Band {
    B2_4,
    B5,
    B6,
}

impl Band {
    pub fn hostapd_mode(self) -> &'static str {
        match self {
            Band::B2_4 => "g",
            Band::B5 | Band::B6 => "a",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Band::B2_4 => "2.4 GHz",
            Band::B5 => "5 GHz",
            Band::B6 => "6 GHz",
        }
    }
}

// 2.4 is offset from 2407, the others start at 5000 and 5950.
// close enough for what hostapd needs.
pub fn frequency_to_channel(mhz: u32) -> Option<u8> {
    match mhz {
        2412..=2472 => Some(((mhz - 2407) / 5) as u8),
        2484 => Some(14),
        5000..=5895 => {
            let channel = (mhz - 5000) / 5;
            if (36..=196).contains(&channel) {
                Some(channel as u8)
            } else {
                None
            }
        }
        5950..=7125 => {
            let channel = (mhz - 5950) / 5;
            if (1..=253).contains(&channel) {
                Some(channel as u8)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn band_for_frequency(mhz: u32) -> Option<Band> {
    match mhz {
        2412..=2484 => Some(Band::B2_4),
        5000..=5895 => Some(Band::B5),
        5950..=7125 => Some(Band::B6),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub phy: String,
    pub uplink: String,
    pub uplink_ssid: Option<String>,
    pub frequency_mhz: Option<u32>,
    pub channel: Option<u8>,
    pub band: Option<Band>,
    pub concurrent: bool,
    pub same_channel_required: bool,
    pub ap_mode: bool,
    pub no_ir: bool,
    pub radar: bool,
    pub second_adapter: Option<String>,
    pub country: Option<String>,
    pub p2p_device: bool,
    pub combinations: Vec<String>,
    pub missing_tools: Vec<String>,
}

pub fn random_password(length: usize) -> Result<String> {
    use std::io::Read;
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let length = length.clamp(8, 63);
    let mut bytes = vec![0u8; length];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect())
}

#[derive(Debug, Clone)]
pub struct HotspotConfig {
    pub ssid: String,
    pub password: String,
    pub ap_interface: String,
    pub country: String,
    pub uplink: Option<String>,
    pub ap_base: Option<String>,
    pub channel: Option<u8>,
    pub hidden: bool,
    pub dry_run: bool,
}

impl Default for HotspotConfig {
    fn default() -> Self {
        Self {
            ssid: "AirforkLab".into(),
            password: "airfork-2026".into(),
            ap_interface: "wlan0_ap".into(),
            country: "KE".into(),
            uplink: None,
            ap_base: None,
            channel: None,
            hidden: false,
            dry_run: false,
        }
    }
}

impl HotspotConfig {
    // mostly to keep newlines out of generated config files
    pub fn validate(&self) -> Result<()> {
        if self.ssid.is_empty() || self.ssid.len() > 32 {
            bail!("SSID must be between 1 and 32 bytes");
        }
        if self.password.len() < 8 || self.password.len() > 63 {
            bail!("password must be 8 to 63 characters for WPA2");
        }
        if self.ssid.contains('\n') || self.password.contains('\n') {
            bail!("SSID and password must not contain newlines");
        }
        if self.ap_interface.is_empty()
            || !self
                .ap_interface
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            bail!("AP interface name may contain only letters, digits and underscores");
        }
        if self.country.len() != 2 || !self.country.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("country code must be two ASCII alphanumeric characters");
        }
        if let Some(source) = &self.uplink {
            if source.trim().is_empty() || source.contains(char::is_whitespace) {
                bail!("internet source must be a single interface name");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub uplink: String,
    pub net_source: String,
    pub ap_interface: String,
    pub ap_base: String,
    pub mode: String,
    pub ap_mac: String,
    pub channel: u8,
    pub band: String,
    pub ssid: String,
    pub gateway: String,
    pub hostapd_pid: u32,
    pub dnsmasq_pid: u32,
    pub watch_pid: Option<u32>,
    pub ip_forward_before: String,
    pub pinned_connection: Option<String>,
    pub band_before: Option<String>,
    pub btm_before: Option<String>,
    pub uplink_gateway: String,
    #[serde(default)]
    pub firewall: String,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub running: bool,
    pub state: Option<RuntimeState>,
    pub capability: Option<Capability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub mac: String,
    pub ip: Option<String>,
    pub hostname: Option<String>,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub level: String,
    pub text: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub mode: String,
    pub ready: bool,
    pub verdict: String,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn add(&mut self, level: &str, text: impl Into<String>, detail: impl Into<String>) {
        self.checks.push(DoctorCheck {
            level: level.into(),
            text: text.into(),
            detail: detail.into(),
        });
    }
}
