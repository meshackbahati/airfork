use crate::*;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

pub fn parse_channel_info(info: &str) -> Option<(u32, u8)> {
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

pub fn current_channel(interface: &str) -> Option<(u32, u8)> {
    parse_channel_info(&stdout("iw", &["dev", interface, "info"]).ok()?)
}

pub fn watch_log(line: &str) {
    use std::io::Write;
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(WATCH_LOG)
    {
        let _ = writeln!(file, "{line}");
    }
}

pub fn move_ap_channel(state: &mut RuntimeState, freq: u32, channel: u8) -> Result<()> {
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

pub fn spawn_watcher() -> Option<u32> {
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
