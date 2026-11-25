//! cli frontend. nothing clever here.

use anyhow::Result;
use clap::{Parser, Subcommand};
use airfork::{detect, plan, start, status, stop, Capability, HotspotConfig, StatusInfo};

// piping to head should not make the tool panic
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

#[derive(Parser, Debug)]
#[command(
    name = "airfork",
    version,
    about = "experimental: share wifi over wifi without dropping the uplink"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Report the adapter's concurrent interface capability
    Detect {
        #[arg(long)]
        json: bool,
    },
    /// Show the exact steps that `start` would take
    Plan {
        #[arg(long)]
        ssid: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long, value_name = "IFACE")]
        ap_interface: Option<String>,
        #[arg(long)]
        country: Option<String>,
        #[arg(long, value_name = "IFACE")]
        uplink: Option<String>,
    },
    /// Start the virtual access point; requires root
    Start {
        #[arg(long)]
        ssid: Option<String>,
        #[arg(long)]
        password: Option<String>,
        #[arg(long, value_name = "IFACE")]
        ap_interface: Option<String>,
        #[arg(long)]
        country: Option<String>,
        #[arg(long, value_name = "IFACE")]
        uplink: Option<String>,
    },
    /// List interfaces that can provide the internet uplink
    Sources,
    /// Stop the virtual access point and remove its resources; requires root
    Stop,
    #[command(hide = true)]
    Watch,
    /// Report the active session and the adapter state
    Status {
        #[arg(long)]
        json: bool,
    },
}

fn config_from(
    ssid: Option<String>,
    password: Option<String>,
    ap_interface: Option<String>,
    country: Option<String>,
    uplink: Option<String>,
) -> HotspotConfig {
    let mut config = HotspotConfig::default();
    if let Some(value) = ssid {
        config.ssid = value;
    }
    if let Some(value) = password {
        config.password = value;
    }
    if let Some(value) = ap_interface {
        config.ap_interface = value;
    }
    if let Some(value) = country {
        config.country = value;
    }
    if let Some(value) = uplink {
        config.uplink = Some(value);
    }
    config
}

fn print_capability(capability: &Capability) {
    out!("Adapter      {}", capability.phy);
    out!("Uplink       {}", capability.uplink);
    if let Some(ssid) = &capability.uplink_ssid {
        out!("Network      {ssid}");
    }
    match (capability.frequency_mhz, capability.channel, capability.band) {
        (Some(freq), Some(channel), Some(band)) => {
            out!(
                "Channel      {} ({} MHz, {})",
                channel,
                freq,
                band.label()
            );
        }
        _ => out!("Channel      none; the station is not connected"),
    }
    out!(
        "Concurrent   {}",
        if capability.concurrent { "yes" } else { "no" }
    );
    out!(
        "Same channel {}",
        if capability.same_channel_required {
            "required"
        } else {
            "not required"
        }
    );
    out!(
        "P2P device   {}",
        if capability.p2p_device { "yes" } else { "no" }
    );
    out!(
        "Tools        {}",
        if capability.missing_tools.is_empty() {
            "all present".to_string()
        } else {
            format!("missing {}", capability.missing_tools.join(", "))
        }
    );
    if !capability.combinations.is_empty() {
        out!("Combinations");
        for combination in &capability.combinations {
            out!("  {combination}");
        }
    }
}

fn print_status(info: &StatusInfo) {
    match &info.state {
        Some(state) => {
            out!(
                "Session      {}",
                if info.running { "running" } else { "stale" }
            );
            out!("Hotspot      \"{}\"", state.ssid);
            out!("Virtual AP   {}", state.ap_interface);
            out!(
                "Uplink       {} on {} channel {}",
                state.uplink, state.band, state.channel
            );
            out!("Gateway      {}", state.gateway);
            out!("Daemons      hostapd {}, dnsmasq {}", state.hostapd_pid, state.dnsmasq_pid);
        }
        None => out!("Session      not running"),
    }
    if let Some(capability) = &info.capability {
        out!();
        print_capability(capability);
    } else {
        out!();
        out!("Adapter      unavailable; connect to a Wi-Fi network first");
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Detect { json } => {
            let capability = detect()?;
            if json {
                out!("{}", serde_json::to_string_pretty(&capability)?);
            } else {
                print_capability(&capability);
            }
        }
        Command::Plan {
            ssid,
            password,
            ap_interface,
            country,
            uplink,
        } => {
            let config = config_from(ssid, password, ap_interface, country, uplink);
            out!("{}", plan(&config)?);
        }
        Command::Start {
            ssid,
            password,
            ap_interface,
            country,
            uplink,
        } => {
            let config = config_from(ssid, password, ap_interface, country, uplink);
            let state = start(&config)?;
            out!("Hotspot \"{}\" is running on {}", state.ssid, state.ap_interface);
            if let Some(notice) = &state.notice {
                out!("Note         {notice}");
            }
            out!("Clients receive an address on {} and reach the uplink through NAT.", state.gateway);
            out!("Stop it with: airfork stop");
        }
        Command::Watch => {
            airfork::watch()?;
        }
        Command::Sources => {
            for source in airfork::sources() {
                out!("{source}");
            }
        }
        Command::Stop => {
            stop()?;
            out!("Hotspot stopped and all created resources removed.");
        }
        Command::Status { json } => {
            let info = status();
            if json {
                out!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                print_status(&info);
            }
        }
    }
    Ok(())
}
