//! GUI state and message handling, split out to respect the 500-line rule.

use adw::prelude::*;
use anyhow::{Context, Result};
use gtk4::glib::ControlFlow;
use gtk4::prelude::*;
use gtk4::{Button, TextView};
use libadwaita as adw;

use airfork::{StatusInfo, clients, sources, status};
use std::sync::{Arc, Mutex};

pub enum Msg {
    Refresh(StatusInfo),
    Clients(Vec<airfork::ClientInfo>),
    Started(Result<String, String>),
    Stopped(Result<(), String>),
}

pub type Queue = Arc<Mutex<Vec<Msg>>>;

#[derive(Clone)]
pub struct Ui {
    pub uplink: adw::ActionRow,
    pub channel: adw::ActionRow,
    pub mode: adw::ActionRow,
    pub session: adw::ActionRow,
    pub combos: adw::ExpanderRow,
    pub combo_rows: Arc<Mutex<Vec<adw::ActionRow>>>,
    pub source: adw::ComboRow,
    pub clients: adw::PreferencesGroup,
    pub client_rows: Arc<Mutex<Vec<adw::ActionRow>>>,
    pub start_button: Button,
    pub stop_button: Button,
    pub log: TextView,
}

pub fn escaped(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn selected_string(combo: &adw::ComboRow) -> Option<String> {
    combo
        .selected_item()
        .and_then(|item| item.downcast::<gtk4::StringObject>().ok())
        .map(|item| item.string().to_string())
}

pub fn log(view: &TextView, line: &str) {
    let buffer = view.buffer();
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, &format!("{line}\n"));
}

pub fn update_clients(ui: &Ui, list: Vec<airfork::ClientInfo>) {
    let mut rows = ui.client_rows.lock().unwrap();
    for row in rows.iter() {
        ui.clients.remove(row);
    }
    rows.clear();
    if list.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No clients connected")
            .subtitle("devices will appear here when they join")
            .build();
        ui.clients.add(&row);
        rows.push(row);
    }
    for client in list {
        let title = client
            .hostname
            .as_deref()
            .unwrap_or(&client.mac)
            .to_string();
        let subtitle = format!(
            "{} · {}",
            client.ip.as_deref().unwrap_or("no address"),
            if client.connected {
                "connected"
            } else {
                "leased"
            }
        );
        let row = adw::ActionRow::builder()
            .title(escaped(&title))
            .subtitle(escaped(&subtitle))
            .build();
        ui.clients.add(&row);
        rows.push(row);
    }
}

pub fn handle_msg(msg: Msg, ui: &Ui) {
    match msg {
        Msg::Refresh(info) => {
            if let Some(cap) = &info.capability {
                ui.uplink.set_subtitle(&escaped(&format!(
                    "{} / {} / {}",
                    cap.uplink,
                    cap.uplink_ssid.as_deref().unwrap_or("<hidden>"),
                    cap.phy
                )));
                match (cap.channel, cap.frequency_mhz, cap.band) {
                    (Some(channel), Some(freq), Some(band)) => ui.channel.set_subtitle(&escaped(
                        &format!("{} — {} MHz, {}", channel, freq, band.label()),
                    )),
                    _ => ui.channel.set_subtitle("not connected"),
                }
                ui.mode.set_subtitle(&escaped(&format!(
                    "{} · same channel {}",
                    if cap.concurrent {
                        "supported"
                    } else {
                        "unsupported"
                    },
                    if cap.same_channel_required {
                        "required"
                    } else {
                        "optional"
                    }
                )));

                {
                    let mut added = ui.combo_rows.lock().unwrap();
                    for row in added.iter() {
                        ui.combos.remove(row);
                    }
                    added.clear();
                    for combination in &cap.combinations {
                        let row = adw::ActionRow::builder()
                            .title(escaped(combination))
                            .build();
                        ui.combos.add_row(&row);
                        added.push(row);
                    }
                }
                ui.combos.set_subtitle(&escaped(&format!(
                    "{} reported by the driver",
                    cap.combinations.len()
                )));
            } else {
                ui.uplink.set_subtitle("adapter unavailable");
                ui.channel.set_subtitle("—");
                ui.mode.set_subtitle("—");
            }

            match &info.state {
                Some(state) => ui.session.set_subtitle(&escaped(&format!(
                    "{} — {} on {}",
                    if info.running { "running" } else { "stale" },
                    state.ssid,
                    state.ap_interface
                ))),
                None => ui.session.set_subtitle("not running"),
            }

            if info.running {
                ui.start_button.set_label("Running");
                ui.start_button.set_sensitive(false);
                ui.start_button.remove_css_class("suggested-action");
                ui.stop_button.set_sensitive(true);
            } else {
                ui.start_button.set_label("Start hotspot");
                ui.start_button.set_sensitive(true);
                ui.start_button.add_css_class("suggested-action");
                ui.stop_button.set_sensitive(false);
            }

            let previous_source = selected_string(&ui.source);
            let model = gtk4::StringList::new(&[] as &[&str]);
            let mut selected = 0u32;
            for (index, source) in sources().iter().enumerate() {
                model.append(source);
                if previous_source.as_deref() == Some(source.as_str()) {
                    selected = index as u32;
                }
            }
            ui.source.set_model(Some(&model));
            ui.source.set_selected(selected);

            update_clients(ui, clients());
        }
        Msg::Clients(list) => update_clients(ui, list),
        Msg::Started(result) => match result {
            Ok(text) => {
                log(&ui.log, "started");
                for line in text.lines() {
                    log(&ui.log, line);
                }
            }
            Err(error) => log(&ui.log, &format!("start failed: {error}")),
        },
        Msg::Stopped(result) => match result {
            Ok(()) => log(&ui.log, "stopped"),
            Err(error) => log(&ui.log, &format!("stop failed: {error}")),
        },
    }
}
