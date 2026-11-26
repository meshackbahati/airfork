use crate::*;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output};

pub fn ensure_ap_band(
    capability: &Capability,
) -> Result<(Capability, Option<(String, Option<String>)>)> {
    if capability.band != Some(Band::B5) && capability.band != Some(Band::B6) {
        return Ok((capability.clone(), None));
    }
    let name = stdout(
        "nmcli",
        &[
            "-g",
            "GENERAL.CONNECTION",
            "dev",
            "show",
            &capability.uplink,
        ],
    )?
    .trim()
    .to_string();
    if name.is_empty() {
        bail!("the uplink is not managed by NetworkManager, so it cannot be limited automatically");
    }
    let previous = stdout(
        "nmcli",
        &["-g", "802-11-wireless.band", "con", "show", &name],
    )?
    .trim()
    .to_string();
    let previous = if previous.is_empty() {
        None
    } else {
        Some(previous)
    };

    run(
        "nmcli",
        &["con", "mod", &name, "802-11-wireless.band", "bg"],
    )?;
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

pub fn restore_band(name: &str, previous: &Option<String>) -> Result<()> {
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
pub struct BandRestore {
    connection: String,
    band_before: Option<String>,
}

pub fn save_band_restore(connection: &str, band_before: &Option<String>) -> Result<()> {
    fs::create_dir_all(RESTORE_DIR)?;
    let record = BandRestore {
        connection: connection.to_string(),
        band_before: band_before.clone(),
    };
    fs::write(RESTORE_FILE, serde_json::to_string_pretty(&record)?)?;
    Ok(())
}

pub fn clear_band_restore() {
    let _ = fs::remove_file(RESTORE_FILE);
}

// a session that never reached stop() (crash, reboot, power cut) leaves the
// band limit in the saved profile and the NM drop-in in /etc. /run is gone by
// then, so the restore record in /var/lib is the only way back.
/// Restore a Wi-Fi profile left changed by a session that never reached stop.
pub fn recover_leftovers() {
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
