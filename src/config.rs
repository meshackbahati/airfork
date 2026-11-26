use crate::*;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

pub fn render_hostapd_conf(channel: u8, band: Band, config: &HotspotConfig) -> String {
    let hidden = if config.hidden {
        "ignore_broadcast_ssid=1\n"
    } else {
        ""
    };
    format!(
        "interface={}\ndriver=nl80211\nssid={}\nhw_mode={}\nchannel={}\nieee80211n=1\nwmm_enabled=1\ncountry_code={}\n{hidden}wpa=2\nwpa_passphrase={}\nwpa_key_mgmt=WPA-PSK\nrsn_pairwise=CCMP\n",
        config.ap_interface,
        config.ssid,
        band.hostapd_mode(),
        channel,
        config.country,
        config.password
    )
}

pub fn render_dnsmasq_conf(config: &HotspotConfig) -> String {
    format!(
        "interface={}\nbind-interfaces\ndhcp-range={}\npid-file={}\ndhcp-leasefile={}\n",
        config.ap_interface, DHCP_RANGE, DNSMASQ_PID, DHCP_LEASES
    )
}

// clients have to leave through wlan0 even when Ethernet owns the default
// route, otherwise the MASQUERADE rule on wlan0 never matches their packets
pub fn plan(config: &HotspotConfig) -> Result<String> {
    config.validate()?;
    let capability = detect()?;
    let ap_base = config
        .ap_base
        .clone()
        .unwrap_or_else(|| capability.uplink.clone());
    let concurrent = capability.concurrent && ap_base == capability.uplink;
    if !concurrent && !capability.ap_mode && capability.second_adapter.is_none() {
        bail!(
            "no adapter here can host an AP; combinations:\n{}",
            capability.combinations.join("\n")
        );
    }
    let (channel, band, ap_channel, switch_note) = if concurrent {
        let channel = capability.channel.unwrap_or(1);
        let band = capability.band.unwrap_or(Band::B2_4);
        let note = if band != Band::B2_4 {
            "Switch    uplink will be limited to 2.4 GHz for the AP\n".to_string()
        } else {
            String::new()
        };
        let ap_channel = if band != Band::B2_4 {
            "picked after the move".to_string()
        } else {
            channel.to_string()
        };
        (channel, Band::B2_4, ap_channel, note)
    } else {
        let channel = config.channel.unwrap_or(6);
        (
            channel,
            Band::B2_4,
            channel.to_string(),
            "Mode        dedicated AP\n".to_string(),
        )
    };
    let source = config.uplink.as_deref().unwrap_or(&capability.uplink);
    Ok(format!(
        "{switch_note}Uplink       {} on {} channel {} ({} MHz, {})\n\
         Virtual AP  {} on {} with MAC {}\n\
         Hotspot     \"{}\" on {} channel {} (WPA2{})\n\
         Network     192.168.42.0/24, DHCP 192.168.42.10 through 100\n\
         Source      internet via {}\n\
         Sharing     NAT from {} to {} via policy routing\n\
         Cleanup     airfork stop removes interface, daemons, NAT rule and state\n",
        capability.uplink,
        capability.uplink_ssid.as_deref().unwrap_or("<hidden>"),
        channel,
        capability.frequency_mhz.unwrap_or(0),
        band.label(),
        config.ap_interface,
        ap_base,
        ap_mac_for(&ap_base).unwrap_or_else(|| AP_MAC.to_string()),
        config.ssid,
        band.label(),
        ap_channel,
        if config.hidden { ", hidden" } else { "" },
        source,
        config.ap_interface,
        source
    ))
}

// the actual work. everything here is reversible by stop().
