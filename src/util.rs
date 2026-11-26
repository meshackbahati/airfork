use crate::*;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

pub fn tool_available(name: &str) -> bool {
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

pub fn command_output(program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("could not execute {program}"))
}

pub fn stdout(program: &str, args: &[&str]) -> Result<String> {
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

pub fn run(program: &str, args: &[&str]) -> Result<()> {
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
pub fn write_private(path: &str, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

pub fn read_pid(path: &str) -> Result<u32> {
    let contents = fs::read_to_string(path).with_context(|| format!("could not read {path}"))?;
    contents
        .trim()
        .parse::<u32>()
        .with_context(|| format!("invalid PID in {path}"))
}

pub fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

// a saved PID can be recycled by an unrelated process, so check the command
pub fn process_is(pid: u32, name: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/cmdline"))
        .map(|command| command.contains(name))
        .unwrap_or(false)
}

// band limit, not BSSID pin. a pin stops the card from recovering when the
// router kicks it off with a BSS transition request.
// nmcli blocks on full activation by default, which can take a minute. cap
// that wait and then poll for the link and address ourselves.
pub fn wait_for_uplink(interface: &str) {
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
pub fn set_btm(interface: &str, value: &str) {
    let _ = run("wpa_cli", &["-i", interface, "set", "disable_btm", value]);
}

pub fn read_btm(interface: &str) -> Option<String> {
    stdout("wpa_cli", &["-i", interface, "get", "disable_btm"])
        .ok()
        .map(|value| value.trim().to_string())
}

// the station MAC is already locally administered on many machines, so the
// XOR is what guarantees the AP address stays distinct from it
pub fn derive_mac(station: &str) -> Option<String> {
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

pub fn ap_mac_for(uplink: &str) -> Option<String> {
    let text = fs::read_to_string(format!("/sys/class/net/{uplink}/address")).ok()?;
    derive_mac(&text)
}

// NM has to be told to leave the virtual interface alone before it appears.
// Setting it unmanaged after creation is too late: NM and wpa_supplicant
// already grabbed it and hostapd then cannot bring it up.
pub fn hide_from_networkmanager(ap: &str, mac: &str) -> Result<()> {
    fs::write(
        NM_CONF,
        format!("[keyfile]\nunmanaged-devices=interface-name:{ap};mac:{mac}\n"),
    )?;
    run("nmcli", &["general", "reload"])
}

pub fn unhide_from_networkmanager() {
    let _ = fs::remove_file(NM_CONF);
    let _ = run("nmcli", &["general", "reload"]);
}

// hostapd daemonises before it validates the config, so -B returning is not
// proof that the AP is up. wait for the pidfile and the AP interface instead.
pub fn wait_for_hostapd(ap: &str) -> Result<u32> {
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
    bail!(
        "hostapd did not bring up {ap}{}",
        if detail.is_empty() {
            String::new()
        } else {
            format!(":\n{detail}")
        }
    )
}
