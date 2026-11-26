use crate::*;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

pub fn start(config: &HotspotConfig) -> Result<RuntimeState> {
    config.validate()?;
    if !is_root() {
        bail!("start needs root; use sudo or pkexec");
    }
    recover_leftovers();

    let capability = detect()?;
    if !capability.missing_tools.is_empty() {
        bail!(
            "missing required tools: {}",
            capability.missing_tools.join(", ")
        );
    }
    if Path::new(STATE_FILE).exists() {
        stop().context("could not clear the previous session")?;
    }

    let net_source = config.uplink.clone().unwrap_or_else(|| {
        if uplink_gateway(&capability.uplink).is_ok() {
            capability.uplink.clone()
        } else {
            sources()
                .into_iter()
                .next()
                .unwrap_or_else(|| capability.uplink.clone())
        }
    });
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
    let source_gateway = uplink_gateway(&net_source).with_context(|| {
        format!("{net_source} has no default route; pick another internet source")
    })?;

    let (mut capability, pin, btm_before) = if concurrent {
        // ignore band steering before the station changes channel
        let before = read_btm(&capability.uplink);
        set_btm(&capability.uplink, "1");
        match ensure_ap_band(&capability) {
            Ok((updated, pin)) => (updated, pin, before),
            Err(error) => {
                set_btm(&capability.uplink, before.as_deref().unwrap_or("0"));
                return Err(error);
            }
        }
    } else {
        (capability.clone(), None, None)
    };
    let (pinned_connection, band_before) = match pin {
        Some((name, old)) => (Some(name), old),
        None => (None, None),
    };
    let notice = if concurrent {
        pinned_connection
            .as_ref()
            .map(|_| "uplink limited to 2.4 GHz for the AP".to_string())
    } else {
        Some("dedicated AP mode".to_string())
    };

    let channel = if concurrent {
        capability.channel.unwrap_or(1)
    } else {
        config.channel.unwrap_or(6)
    };
    let band = if concurrent {
        capability.band.unwrap_or(Band::B2_4)
    } else {
        Band::B2_4
    };
    capability.channel = Some(channel);
    capability.band = Some(band);

    fs::create_dir_all(RUN_DIR)?;
    write_private(HOSTAPD_CONF, &render_hostapd_conf(channel, band, config))?;
    write_private(DNSMASQ_CONF, &render_dnsmasq_conf(config))?;

    let forward_before = fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
        .unwrap_or_else(|_| "0\n".to_string())
        .trim()
        .to_string();

    let ap_mac = ap_mac_for(&ap_base).unwrap_or_else(|| AP_MAC.to_string());

    // NM must be out of the way before the interface exists
    hide_from_networkmanager(&config.ap_interface, &ap_mac)?;

    // a failed previous run can leave the interface behind
    let _ = run("iw", &["dev", &config.ap_interface, "del"]);
    run(
        "iw",
        &[
            "dev",
            &ap_base,
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
        let _ = run(
            "nmcli",
            &["dev", "set", &config.ap_interface, "managed", "no"],
        );

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

        let firewall = firewall_up(&config.ap_interface)?;
        route_clients(&source_gateway, &net_source)?;
        fs::write("/proc/sys/net/ipv4/ip_forward", "1\n")?;

        Ok(RuntimeState {
            uplink: capability.uplink.clone(),
            net_source: net_source.clone(),
            ap_interface: config.ap_interface.clone(),
            ap_base: ap_base.clone(),
            mode: if concurrent {
                "concurrent".into()
            } else {
                "dedicated".into()
            },
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
            firewall: firewall.clone(),
            notice: notice.clone(),
        })
    })();

    match result {
        Ok(mut state) => {
            fs::write(STATE_FILE, serde_json::to_string_pretty(&state)?)?;
            state.watch_pid = if concurrent { spawn_watcher() } else { None };
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
            let _ = run("nft", &["delete", "table", "ip", "airfork_nat"]);
            let _ = run("nft", &["delete", "table", "inet", "airfork_fw"]);
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
            set_btm(&capability.uplink, btm_before.as_deref().unwrap_or("0"));
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
    firewall_down(&state.firewall, &state.ap_interface);
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
    set_btm(&state.uplink, state.btm_before.as_deref().unwrap_or("0"));
    let _ = fs::remove_file(STATE_FILE);
    Ok(())
}
