//! experimental wifi sharing thing. no promises.
//!
//! it just drives iw/ip/hostapd/dnsmasq/iptables instead of reimplementing
//! them. easier to debug that way, and easier to throw away.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
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

#[derive(Debug, Clone)]
struct IwDevEntry {
    phy: String,
    name: String,
    kind: String,
    ssid: Option<String>,
    freq: Option<u32>,
}

// iw dev is indent-based. walk the lines, keep whatever block is open.
fn parse_iw_dev(text: &str) -> Vec<IwDevEntry> {
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
fn parse_combinations(text: &str) -> Vec<String> {
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
fn max_channels(block: &str) -> Option<u32> {
    let marker = "#channels <=";
    let index = block.find(marker)?;
    block[index + marker.len()..]
        .trim()
        .split_whitespace()
        .next()?
        .parse()
        .ok()
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
    pub p2p_device: bool,
    pub combinations: Vec<String>,
    pub missing_tools: Vec<String>,
}

fn tool_available(name: &str) -> bool {
    if let Some(paths) = env::var_os("PATH") {
        for directory in env::split_paths(&paths) {
            if directory.join(name).is_file() {
                return true;
            }
        }
    }
    false
}

pub fn is_root() -> bool {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(uid) = line.strip_prefix("Uid:") {
            return uid.split_whitespace().next() == Some("0");
        }
    }
    false
}

fn command_output(program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("could not execute {program}"))
}

fn stdout(program: &str, args: &[&str]) -> Result<String> {
    let output = command_output(program, args)?;
    if !output.status.success() {
        bail!(
            "{} {} failed: {}",
            program,
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = command_output(program, args)?;
    if !output.status.success() {
        bail!(
            "{} {} failed: {}",
            program,
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// read-only. safe to run as a normal user.
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

    let missing_tools = ["iw", "ip", "hostapd", "dnsmasq", "iptables"]
        .iter()
        .filter(|tool| !tool_available(tool))
        .map(|tool| (*tool).to_string())
        .collect();

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
        p2p_device: devices.iter().any(|entry| entry.kind == "P2P-device"),
        combinations,
        missing_tools,
    })
}

#[derive(Debug, Clone)]
pub struct HotspotConfig {
    pub ssid: String,
    pub password: String,
    pub ap_interface: String,
    pub country: String,
    pub uplink: Option<String>,
}

impl Default for HotspotConfig {
    fn default() -> Self {
        Self {
            ssid: "AirforkLab".into(),
            password: "airfork-2026".into(),
            ap_interface: "wlan0_ap".into(),
            country: "KE".into(),
            uplink: None,
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
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub running: bool,
    pub state: Option<RuntimeState>,
    pub capability: Option<Capability>,
}

fn render_hostapd_conf(capability: &Capability, config: &HotspotConfig) -> Result<String> {
    let channel = capability.channel.unwrap_or(1);
    let band = capability.band.unwrap_or(Band::B2_4);
    Ok(format!(
        "interface={}\ndriver=nl80211\nssid={}\nhw_mode={}\nchannel={}\nieee80211n=1\nwmm_enabled=1\ncountry_code={}\nwpa=2\nwpa_passphrase={}\nwpa_key_mgmt=WPA-PSK\nrsn_pairwise=CCMP\n",
        config.ap_interface,
        config.ssid,
        band.hostapd_mode(),
        channel,
        config.country,
        config.password
    ))
}

fn render_dnsmasq_conf(config: &HotspotConfig) -> String {
    format!(
        "interface={}\nbind-interfaces\ndhcp-range={}\npid-file={}\ndhcp-leasefile={}\n",
        config.ap_interface, DHCP_RANGE, DNSMASQ_PID, DHCP_LEASES
    )
}

// clients have to leave through wlan0 even when Ethernet owns the default
// route, otherwise the MASQUERADE rule on wlan0 never matches their packets
fn uplink_gateway(interface: &str) -> Result<String> {
    let routes = stdout("ip", &["-4", "route", "show", "default", "dev", interface])?;
    routes
        .split_whitespace()
        .skip_while(|word| *word != "via")
        .nth(1)
        .map(str::to_string)
        .context("could not find the gateway for the uplink")
}

fn route_clients(gateway: &str, interface: &str) -> Result<()> {
    let existing = stdout("ip", &["rule", "show"])
        .unwrap_or_default()
        .contains(CLIENT_SUBNET);
    if !existing {
        run(
            "ip",
            &[
                "rule",
                "add",
                "from",
                CLIENT_SUBNET,
                "table",
                ROUTE_TABLE,
                "priority",
                "100",
            ],
        )?;
    }
    let args = [
        "route",
        "replace",
        "default",
        "via",
        gateway,
        "dev",
        interface,
        "table",
        ROUTE_TABLE,
    ];
    run("ip", &args)?;
    // a reconnect can wipe the table while the session is being set up
    for _ in 0..4 {
        let table = stdout("ip", &["route", "show", "table", ROUTE_TABLE])
            .unwrap_or_default();
        if table.contains("default") {
            return Ok(());
        }
        run("ip", &args)?;
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}

fn unroute_clients(gateway: &str, interface: &str) {
    let _ = run(
        "ip",
        &[
            "rule",
            "del",
            "from",
            CLIENT_SUBNET,
            "table",
            ROUTE_TABLE,
            "priority",
            "100",
        ],
    );
    let _ = run(
        "ip",
        &[
            "route",
            "del",
            "default",
            "via",
            gateway,
            "dev",
            interface,
            "table",
            ROUTE_TABLE,
        ],
    );
}

/// A device joined to the virtual access point, with its DHCP address if known.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub mac: String,
    pub ip: Option<String>,
    pub hostname: Option<String>,
    pub connected: bool,
}

/// Interfaces that currently have an IPv4 default route, offered as internet sources.
pub fn sources() -> Vec<String> {
    let routes = stdout("ip", &["-4", "route", "show", "default"])
        .unwrap_or_default();
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

fn read_pid(path: &str) -> Result<u32> {
    let contents = fs::read_to_string(path).with_context(|| format!("could not read {path}"))?;
    contents
        .trim()
        .parse::<u32>()
        .with_context(|| format!("invalid PID in {path}"))
}

fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

// a saved PID can be recycled by an unrelated process, so check the command
fn process_is(pid: u32, name: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/cmdline"))
        .map(|command| command.contains(name))
        .unwrap_or(false)
}

// band limit, not BSSID pin. a pin stops the card from recovering when the
// router kicks it off with a BSS transition request.
// nmcli blocks on full activation by default, which can take a minute. cap
// that wait and then poll for the link and address ourselves.
fn wait_for_uplink(interface: &str) {
    for _ in 0..30 {
        let linked = stdout("iw", &["dev", interface, "link"])
            .map(|text| text.contains("Connected to"))
            .unwrap_or(false);
        let addressed = stdout("ip", &["-4", "addr", "show", interface])
            .map(|text| text.contains("inet "))
            .unwrap_or(false);
        if linked && addressed {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn ensure_ap_band(capability: &Capability) -> Result<(Capability, Option<(String, Option<String>)>)> {
    if capability.band != Some(Band::B5) && capability.band != Some(Band::B6) {
        return Ok((capability.clone(), None));
    }
    let name = stdout("nmcli", &["-g", "GENERAL.CONNECTION", "dev", "show", &capability.uplink])?
        .trim()
        .to_string();
    if name.is_empty() {
        bail!("the uplink is not managed by NetworkManager, so it cannot be limited automatically");
    }
    let previous = stdout("nmcli", &["-g", "802-11-wireless.band", "con", "show", &name])?
        .trim()
        .to_string();
    let previous = if previous.is_empty() { None } else { Some(previous) };

    run("nmcli", &["con", "mod", &name, "802-11-wireless.band", "bg"])?;
    save_band_restore(&name, &previous)?;
    let _ = run("nmcli", &["con", "up", &name]);
    wait_for_uplink(&capability.uplink);
    let updated = detect()?;
    if updated.band != Some(Band::B2_4) {
        let _ = restore_band(&name, &previous);
        clear_band_restore();
        let _ = run("nmcli", &["con", "up", &name]);
        bail!("the uplink did not move to 2.4 GHz; hotspot not started");
    }
    Ok((updated, Some((name, previous))))
}

fn restore_band(name: &str, previous: &Option<String>) -> Result<()> {
    run(
        "nmcli",
        &[
            "con",
            "mod",
            name,
            "802-11-wireless.band",
            previous.as_deref().unwrap_or(""),
        ],
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BandRestore {
    connection: String,
    band_before: Option<String>,
}

fn save_band_restore(connection: &str, band_before: &Option<String>) -> Result<()> {
    fs::create_dir_all(RESTORE_DIR)?;
    let record = BandRestore {
        connection: connection.to_string(),
        band_before: band_before.clone(),
    };
    fs::write(RESTORE_FILE, serde_json::to_string_pretty(&record)?)?;
    Ok(())
}

fn clear_band_restore() {
    let _ = fs::remove_file(RESTORE_FILE);
}

// a session that never reached stop() (crash, reboot, power cut) leaves the
// band limit in the saved profile and the NM drop-in in /etc. /run is gone by
// then, so the restore record in /var/lib is the only way back.
fn recover_leftovers() {
    if Path::new(STATE_FILE).exists() {
        return;
    }
    if let Ok(text) = fs::read_to_string(RESTORE_FILE) {
        if let Ok(record) = serde_json::from_str::<BandRestore>(&text) {
            let _ = restore_band(&record.connection, &record.band_before);
        }
        clear_band_restore();
    }
    if Path::new(NM_CONF).exists() {
        unhide_from_networkmanager();
    }
}

// MASQUERADE alone is not enough when something (Docker, ufw, firewalld) has
// set the FORWARD policy to DROP: clients join but get no internet.
fn forward_rules() -> [Vec<&'static str>; 2] {
    [
        vec![
            "FORWARD", "-s", CLIENT_SUBNET, "-j", "ACCEPT", "-m", "comment", "--comment", NAT_COMMENT,
        ],
        vec![
            "FORWARD", "-d", CLIENT_SUBNET, "-m", "conntrack", "--ctstate", "RELATED,ESTABLISHED",
            "-j", "ACCEPT", "-m", "comment", "--comment", NAT_COMMENT,
        ],
    ]
}

fn allow_forwarding() -> Result<()> {
    for rule in forward_rules() {
        let mut check = vec!["-C"];
        check.extend(rule.iter().copied());
        let present = Command::new("iptables")
            .args(&check)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !present {
            // insert at the top so it sits ahead of a DROP policy's chains
            let mut insert = vec!["-I", rule[0], "1"];
            insert.extend(rule[1..].iter().copied());
            run("iptables", &insert)?;
        }
    }
    Ok(())
}

fn remove_forwarding() {
    for rule in forward_rules() {
        let mut delete = vec!["-D"];
        delete.extend(rule.iter().copied());
        let _ = run("iptables", &delete);
    }
}

// the router periodically sends 802.11v "disassociation imminent" requests.
// ignoring them keeps the uplink steady while the AP shares the same radio.
fn set_btm(interface: &str, value: &str) {
    let _ = run("wpa_cli", &["-i", interface, "set", "disable_btm", value]);
}

fn read_btm(interface: &str) -> Option<String> {
    stdout("wpa_cli", &["-i", interface, "get", "disable_btm"])
        .ok()
        .map(|value| value.trim().to_string())
}

// the station MAC is already locally administered on many machines, so the
// XOR is what guarantees the AP address stays distinct from it
fn derive_mac(station: &str) -> Option<String> {
    let mut bytes: Vec<u8> = station
        .trim()
        .split(':')
        .map(|part| u8::from_str_radix(part, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    if bytes.len() != 6 {
        return None;
    }
    bytes[0] = (bytes[0] | 0x02) & 0xfe;
    bytes[4] ^= 0x5a;
    Some(
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

fn ap_mac_for(uplink: &str) -> Option<String> {
    let text = fs::read_to_string(format!("/sys/class/net/{uplink}/address")).ok()?;
    derive_mac(&text)
}

// NM has to be told to leave the virtual interface alone before it appears.
// Setting it unmanaged after creation is too late: NM and wpa_supplicant
// already grabbed it and hostapd then cannot bring it up.
fn hide_from_networkmanager(ap: &str, mac: &str) -> Result<()> {
    fs::write(
        NM_CONF,
        format!(
            "[keyfile]\nunmanaged-devices=interface-name:{ap};mac:{mac}\n"
        ),
    )?;
    run("nmcli", &["general", "reload"])
}

fn unhide_from_networkmanager() {
    let _ = fs::remove_file(NM_CONF);
    let _ = run("nmcli", &["general", "reload"]);
}

// hostapd daemonises before it validates the config, so -B returning is not
// proof that the AP is up. wait for the pidfile and the AP interface instead.
fn wait_for_hostapd(ap: &str) -> Result<u32> {
    for _ in 0..20 {
        if let Ok(pid) = read_pid(HOSTAPD_PID) {
            if process_alive(pid) {
                let is_ap = stdout("iw", &["dev", ap, "info"])
                    .map(|info| info.contains("type AP"))
                    .unwrap_or(false);
                if is_ap {
                    return Ok(pid);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let detail = fs::read_to_string(HOSTAPD_LOG)
        .unwrap_or_default()
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .join("\n");
    bail!("hostapd did not bring up {ap}{}", if detail.is_empty() { String::new() } else { format!(":\n{detail}") })
}

pub fn plan(config: &HotspotConfig) -> Result<String> {
    config.validate()?;
    let capability = detect()?;
    if !capability.concurrent {
        bail!(
            "this adapter cannot run station and AP interfaces together; the combinations are:\n{}",
            capability.combinations.join("\n")
        );
    }
    let channel = capability.channel.unwrap_or(1);
    let band = capability.band.unwrap_or(Band::B2_4);
    let switch_note = if band != Band::B2_4 {
        "Switch    uplink will be limited to 2.4 GHz for the AP\n".to_string()
    } else {
        String::new()
    };
    let ap_band = if band != Band::B2_4 { Band::B2_4 } else { band };
    let ap_channel = if ap_band != band { "picked after the move".to_string() } else { channel.to_string() };
    Ok(format!(
        "{switch_note}Uplink       {} on {} channel {} ({} MHz, {})\n\
         Virtual AP  {} with MAC {}\n\
         Hotspot     \"{}\" on {} channel {} (WPA2)\n\
         Network     192.168.42.0/24, DHCP 192.168.42.10 through 100\n\
         Source      internet via {}\n\
         Sharing     NAT from {} to {} via iptables and policy routing\n\
         Cleanup     airfork stop removes interface, daemons, NAT rule and state\n\
         Note        one radio, so the two roles share throughput\n",
        capability.uplink,
        capability.uplink_ssid.as_deref().unwrap_or("<hidden>"),
        channel,
        capability.frequency_mhz.unwrap_or(0),
        band.label(),
        config.ap_interface,
        ap_mac_for(&capability.uplink).unwrap_or_else(|| AP_MAC.to_string()),
        config.ssid,
        ap_band.label(),
        ap_channel,
        config.uplink.as_deref().unwrap_or(&capability.uplink),
        config.ap_interface,
        config.uplink.as_deref().unwrap_or(&capability.uplink)
    ))
}

// the actual work. everything here is reversible by stop().
pub fn start(config: &HotspotConfig) -> Result<RuntimeState> {
    config.validate()?;
    if !is_root() {
        bail!("start needs root; use sudo or pkexec");
    }
    recover_leftovers();

    let capability = detect()?;
    if !capability.concurrent {
        bail!(
            "this adapter cannot run station and AP interfaces together; the combinations are:\n{}",
            capability.combinations.join("\n")
        );
    }
    if !capability.missing_tools.is_empty() {
        bail!(
            "missing required tools: {}",
            capability.missing_tools.join(", ")
        );
    }
    if Path::new(STATE_FILE).exists() {
        stop().context("could not clear the previous session")?;
    }

    // ignore the router's band-steering before the station changes channel
    let btm_before = read_btm(&capability.uplink);
    set_btm(&capability.uplink, "1");

    let (capability, pin) = match ensure_ap_band(&capability) {
        Ok(value) => value,
        Err(error) => {
            set_btm(&capability.uplink, btm_before.as_deref().unwrap_or("0"));
            return Err(error);
        }
    };
    let (pinned_connection, band_before) = match pin {
        Some((name, old)) => (Some(name), old),
        None => (None, None),
    };
    let net_source = config.uplink.clone().unwrap_or_else(|| {
        if uplink_gateway(&capability.uplink).is_ok() {
            capability.uplink.clone()
        } else {
            sources().into_iter().next().unwrap_or_else(|| capability.uplink.clone())
        }
    });
    let source_gateway = uplink_gateway(&net_source)
        .with_context(|| format!("{net_source} has no default route; pick another internet source"))?;
    let notice = pinned_connection
        .as_ref()
        .map(|_| "uplink limited to 2.4 GHz for the AP".to_string());

    let channel = capability.channel.unwrap_or(1);
    let band = capability.band.unwrap_or(Band::B2_4);

    fs::create_dir_all(RUN_DIR)?;
    fs::write(HOSTAPD_CONF, render_hostapd_conf(&capability, config)?)?;
    fs::write(DNSMASQ_CONF, render_dnsmasq_conf(config))?;

    let forward_before = fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
        .unwrap_or_else(|_| "0\n".to_string())
        .trim()
        .to_string();

    let ap_mac = ap_mac_for(&capability.uplink).unwrap_or_else(|| AP_MAC.to_string());

    // NM must be out of the way before the interface exists
    hide_from_networkmanager(&config.ap_interface, &ap_mac)?;

    // a failed previous run can leave the interface behind
    let _ = run("iw", &["dev", &config.ap_interface, "del"]);
    run(
        "iw",
        &[
            "dev",
            &capability.uplink,
            "interface",
            "add",
            &config.ap_interface,
            "type",
            "__ap",
        ],
    )
    .context("could not create the virtual AP interface")?;

    // anything after the interface creation can fail, so undo on the way out
    let result = (|| -> Result<RuntimeState> {
        // some chips refuse the address; keep the one the kernel assigned
        let ap_mac = if run(
            "ip",
            &["link", "set", &config.ap_interface, "address", &ap_mac],
        )
        .is_ok()
        {
            ap_mac
        } else {
            fs::read_to_string(format!("/sys/class/net/{}/address", config.ap_interface))
                .unwrap_or_else(|_| ap_mac.clone())
                .trim()
                .to_string()
        };
        let _ = run("nmcli", &["dev", "set", &config.ap_interface, "managed", "no"]);

        let _ = fs::remove_file(HOSTAPD_PID);
        let _ = fs::remove_file(HOSTAPD_LOG);
        run(
            "hostapd",
            &["-B", "-P", HOSTAPD_PID, "-f", HOSTAPD_LOG, HOSTAPD_CONF],
        )?;
        let hostapd_pid = wait_for_hostapd(&config.ap_interface)?;

        run(
            "ip",
            &[
                "addr",
                "add",
                &format!("{GATEWAY}/24"),
                "dev",
                &config.ap_interface,
            ],
        )?;
        run("dnsmasq", &["-C", DNSMASQ_CONF])?;
        let dnsmasq_pid = read_pid(DNSMASQ_PID)?;

        // source-based NAT, so it works even when another interface owns the
        // default route or the policy route gets rewritten later
        let nat_args = [
            "-t",
            "nat",
            "-C",
            "POSTROUTING",
            "-s",
            CLIENT_SUBNET,
            "-j",
            "MASQUERADE",
            "-m",
            "comment",
            "--comment",
            NAT_COMMENT,
        ];
        let already_natted = Command::new("iptables")
            .args(nat_args)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if !already_natted {
            let mut add_args = nat_args.to_vec();
            add_args[2] = "-A";
            run("iptables", &add_args)?;
        }

        allow_forwarding()?;
        route_clients(&source_gateway, &net_source)?;
        fs::write("/proc/sys/net/ipv4/ip_forward", "1\n")?;

        Ok(RuntimeState {
            uplink: capability.uplink.clone(),
            net_source: net_source.clone(),
            ap_interface: config.ap_interface.clone(),
            ap_mac: ap_mac.clone(),
            channel,
            band: band.label().to_string(),
            ssid: config.ssid.clone(),
            gateway: GATEWAY.to_string(),
            hostapd_pid,
            dnsmasq_pid,
            watch_pid: None,
            ip_forward_before: forward_before.clone(),
            pinned_connection: pinned_connection.clone(),
            band_before: band_before.clone(),
            btm_before: btm_before.clone(),
            uplink_gateway: source_gateway.clone(),
            notice: notice.clone(),
        })
    })();

    match result {
        Ok(mut state) => {
            fs::write(STATE_FILE, serde_json::to_string_pretty(&state)?)?;
            state.watch_pid = spawn_watcher();
            fs::write(STATE_FILE, serde_json::to_string_pretty(&state)?)?;
            Ok(state)
        }
        Err(error) => {
            for pidfile in [HOSTAPD_PID, DNSMASQ_PID] {
                if let Ok(pid) = read_pid(pidfile) {
                    let _ = run("kill", &[&pid.to_string()]);
                }
                let _ = fs::remove_file(pidfile);
            }
            let _ = run(
                "iptables",
                &[
                    "-t",
                    "nat",
                    "-D",
                    "POSTROUTING",
                    "-s",
                    CLIENT_SUBNET,
                    "-j",
                    "MASQUERADE",
                    "-m",
                    "comment",
                    "--comment",
                    NAT_COMMENT,
                ],
            );
            remove_forwarding();
            unroute_clients(&source_gateway, &net_source);
            let _ = fs::write(
                "/proc/sys/net/ipv4/ip_forward",
                format!("{}\n", forward_before),
            );
            let _ = run("iw", &["dev", &config.ap_interface, "del"]);
            unhide_from_networkmanager();
            if let Some(name) = &pinned_connection {
                let _ = restore_band(name, &band_before);
            }
            clear_band_restore();
            set_btm(
                &capability.uplink,
                btm_before.as_deref().unwrap_or("0"),
            );
            Err(error)
        }
    }
}

pub fn stop() -> Result<()> {
    if !is_root() {
        bail!("stop needs root; use sudo or pkexec");
    }
    let contents = match fs::read_to_string(STATE_FILE) {
        Ok(value) => value,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            bail!("no active airfork session")
        }
        Err(error) => return Err(error.into()),
    };
    let state: RuntimeState = serde_json::from_str(&contents).context("invalid session state")?;

    if let Some(pid) = state.watch_pid {
        if process_is(pid, "airfork") {
            let _ = run("kill", &[&pid.to_string()]);
        }
    }
    if process_is(state.hostapd_pid, "hostapd") {
        let _ = run("kill", &[&state.hostapd_pid.to_string()]);
    }
    if process_is(state.dnsmasq_pid, "dnsmasq") {
        let _ = run("kill", &[&state.dnsmasq_pid.to_string()]);
    }
    let _ = run(
        "iptables",
        &[
            "-t",
            "nat",
            "-D",
            "POSTROUTING",
            "-s",
            CLIENT_SUBNET,
            "-j",
            "MASQUERADE",
            "-m",
            "comment",
            "--comment",
            NAT_COMMENT,
        ],
    );
    // older sessions used an interface-scoped rule with the same comment
    let _ = run(
        "iptables",
        &[
            "-t",
            "nat",
            "-D",
            "POSTROUTING",
            "-o",
            &state.net_source,
            "-j",
            "MASQUERADE",
            "-m",
            "comment",
            "--comment",
            NAT_COMMENT,
        ],
    );
    remove_forwarding();
    unroute_clients(&state.uplink_gateway, &state.net_source);
    let _ = fs::write(
        "/proc/sys/net/ipv4/ip_forward",
        format!("{}\n", state.ip_forward_before),
    );
    let _ = run("iw", &["dev", &state.ap_interface, "del"]);
    unhide_from_networkmanager();
    if let Some(name) = &state.pinned_connection {
        let _ = restore_band(name, &state.band_before);
    }
    clear_band_restore();
    set_btm(
        &state.uplink,
        state.btm_before.as_deref().unwrap_or("0"),
    );
    let _ = fs::remove_file(STATE_FILE);
    Ok(())
}

fn parse_channel_info(info: &str) -> Option<(u32, u8)> {
    for line in info.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("channel ") {
            let channel = rest.split_whitespace().next()?.parse::<u8>().ok()?;
            let freq = rest
                .split('(')
                .nth(1)?
                .split_whitespace()
                .next()?
                .parse::<u32>()
                .ok()?;
            return Some((freq, channel));
        }
    }
    None
}

fn current_channel(interface: &str) -> Option<(u32, u8)> {
    parse_channel_info(&stdout("iw", &["dev", interface, "info"]).ok()?)
}

fn watch_log(line: &str) {
    use std::io::Write;
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(WATCH_LOG) {
        let _ = writeln!(file, "{line}");
    }
}

fn move_ap_channel(state: &mut RuntimeState, freq: u32, channel: u8) -> Result<()> {
    // try a channel switch announcement first, so clients follow without a drop
    let switched = Command::new("hostapd_cli")
        .args([
            "-i",
            &state.ap_interface,
            "chan_switch",
            "5",
            &freq.to_string(),
        ])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    let mut applied = false;
    if switched {
        std::thread::sleep(std::time::Duration::from_secs(2));
        applied = current_channel(&state.ap_interface)
            .map(|(current_freq, _)| current_freq == freq)
            .unwrap_or(false);
    }
    if !applied {
        let config = fs::read_to_string(HOSTAPD_CONF).unwrap_or_default();
        let updated = config
            .lines()
            .map(|line| {
                if line.starts_with("channel=") {
                    format!("channel={channel}")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(HOSTAPD_CONF, updated)?;
        if process_alive(state.hostapd_pid) {
            let _ = run("kill", &[&state.hostapd_pid.to_string()]);
        }
        let _ = fs::remove_file(HOSTAPD_PID);
        run(
            "hostapd",
            &["-B", "-P", HOSTAPD_PID, "-f", HOSTAPD_LOG, HOSTAPD_CONF],
        )?;
        state.hostapd_pid = wait_for_hostapd(&state.ap_interface)?;
    }
    state.channel = channel;
    fs::write(STATE_FILE, serde_json::to_string_pretty(&state)?)?;
    Ok(())
}

/// Long-running watcher for the single-radio channel constraint.
pub fn watch() -> Result<()> {
    let mut last: Option<(u32, u8)> = None;
    let mut pending: Option<(u32, u8)> = None;
    watch_log("watcher started");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let text = match fs::read_to_string(STATE_FILE) {
            Ok(value) => value,
            Err(_) => break,
        };
        let mut state: RuntimeState = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => break,
        };
        let Some((freq, channel)) = current_channel(&state.uplink) else {
            continue;
        };
        if band_for_frequency(freq) != Some(Band::B2_4) {
            watch_log("station moved off 2.4 GHz; watcher stopping");
            break;
        }
        if current_channel(&state.ap_interface).map(|(f, _)| f == freq) == Some(true) {
            last = Some((freq, channel));
            pending = None;
            continue;
        }
        if last == Some((freq, channel)) {
            continue;
        }
        if pending == Some((freq, channel)) {
            match move_ap_channel(&mut state, freq, channel) {
                Ok(()) => {
                    watch_log(&format!("AP moved to channel {channel} ({freq} MHz)"));
                    last = Some((freq, channel));
                }
                Err(error) => watch_log(&format!("channel move failed: {error}")),
            }
            pending = None;
        } else {
            pending = Some((freq, channel));
        }
    }
    watch_log("watcher stopped");
    Ok(())
}

fn spawn_watcher() -> Option<u32> {
    use std::process::Stdio;
    let exe = std::env::current_exe().ok()?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(WATCH_LOG)
        .ok()?;
    let child = Command::new(exe)
        .arg("watch")
        .stdout(Stdio::from(log.try_clone().ok()?))
        .stderr(Stdio::from(log))
        .spawn()
        .ok()?;
    Some(child.id())
}

pub fn status() -> StatusInfo {
    let state = fs::read_to_string(STATE_FILE)
        .ok()
        .and_then(|contents| serde_json::from_str::<RuntimeState>(&contents).ok());
    let running = state
        .as_ref()
        .map(|state| process_alive(state.hostapd_pid) && process_alive(state.dnsmasq_pid))
        .unwrap_or(false);
    StatusInfo {
        running,
        state,
        capability: detect().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_from_frequencies() {
        assert_eq!(frequency_to_channel(2412), Some(1));
        assert_eq!(frequency_to_channel(2472), Some(13));
        assert_eq!(frequency_to_channel(2484), Some(14));
        assert_eq!(frequency_to_channel(5220), Some(44));
        assert_eq!(frequency_to_channel(5955), Some(1));
        assert_eq!(frequency_to_channel(100), None);
    }

    #[test]
    fn bands_from_frequencies() {
        assert_eq!(band_for_frequency(2437), Some(Band::B2_4));
        assert_eq!(band_for_frequency(5220), Some(Band::B5));
        assert_eq!(band_for_frequency(5955), Some(Band::B6));
        assert_eq!(band_for_frequency(100), None);
    }

    #[test]
    fn iw_dev_blocks() {
        let sample = "phy#0\n\tInterface wlan0\n\t\tifindex 3\n\t\ttype managed\n\t\tssid Lab\n\t\tchannel 44 (5220 MHz), width: 80 MHz\n";
        let entries = parse_iw_dev(sample);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].phy, "phy0");
        assert_eq!(entries[0].name, "wlan0");
        assert_eq!(entries[0].kind, "managed");
        assert_eq!(entries[0].freq, Some(5220));
        assert_eq!(entries[0].ssid.as_deref(), Some("Lab"));
    }

    #[test]
    fn iwlwifi_combinations() {
        let sample = "\
\tvalid interface combinations:
\t\t * #{ managed } <= 1, #{ P2P-client, P2P-GO } <= 1, #{ P2P-device } <= 1,
\t\t   total <= 3, #channels <= 2
\t\t * #{ managed } <= 1, #{ AP, P2P-client, P2P-GO } <= 1, #{ P2P-device } <= 1,
\t\t   total <= 3, #channels <= 1
\tHT Capability overrides:
";
        let blocks = parse_combinations(sample);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("managed") && !blocks[0].contains("AP"));
        assert!(blocks[1].contains("managed") && blocks[1].contains("AP"));
        assert_eq!(max_channels(&blocks[1]), Some(1));
    }

    #[test]
    fn hostapd_conf_enables_80211n_and_wmm() {
        let capability = Capability {
            phy: "phy0".into(),
            uplink: "wlan0".into(),
            uplink_ssid: None,
            frequency_mhz: Some(2437),
            channel: Some(6),
            band: Some(Band::B2_4),
            concurrent: true,
            same_channel_required: true,
            p2p_device: false,
            combinations: vec![],
            missing_tools: vec![],
        };
        let conf = render_hostapd_conf(&capability, &HotspotConfig::default()).unwrap();
        assert!(conf.contains("channel=6\n"));
        assert!(conf.contains("ieee80211n=1\n"));
        assert!(conf.contains("wmm_enabled=1\n"));
    }

    #[test]
    fn forward_rules_cover_both_directions() {
        let rules = forward_rules();
        assert!(rules[0].contains(&"-s") && rules[0].contains(&CLIENT_SUBNET));
        assert!(rules[1].contains(&"-d") && rules[1].contains(&"RELATED,ESTABLISHED"));
        assert!(rules.iter().all(|rule| rule[0] == "FORWARD" && rule.contains(&NAT_COMMENT)));
    }

    #[test]
    fn channel_info_parser() {
        let info = "Interface wlan0\n\ttype managed\n\tchannel 6 (2437 MHz), width: 20 MHz\n";
        assert_eq!(parse_channel_info(info), Some((2437, 6)));
        assert_eq!(parse_channel_info("no channel here"), None);
    }

    #[test]
    fn derived_ap_mac() {
        let station = "c0:b8:83:68:05:6a";
        let ap = derive_mac(station).unwrap();
        let first = u8::from_str_radix(&ap[..2], 16).unwrap();
        assert_eq!(first & 0x01, 0);
        assert_eq!(first & 0x02, 0x02);
        assert_ne!(ap, station);
        assert_eq!(derive_mac("02:00:00:00:01:01").unwrap(), "02:00:00:00:5b:01");
        assert!(derive_mac("bad").is_none());
    }

    #[test]
    fn config_checks() {
        let mut config = HotspotConfig::default();
        assert!(config.validate().is_ok());
        config.ssid = "x".repeat(33);
        assert!(config.validate().is_err());
        config.ssid = "lab".into();
        config.password = "short".into();
        assert!(config.validate().is_err());
        config.password = "x".repeat(64);
        assert!(config.validate().is_err());
        config.password = "longenough".into();
        config.ap_interface = "bad-name".into();
        assert!(config.validate().is_err());
    }
}
